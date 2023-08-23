// 该模块提供一个名为 `Ignore` 的数据结构，将“目录遍历”与“忽略匹配器”相连接。
// 具体来说，它了解 gitignore 的语义和优先级，并基于目录层次结构进行组织。
// 每个匹配器在逻辑上对应于来自单个目录的忽略规则，并指向其相应父目录的匹配器。
// 从这个意义上说，`Ignore` 是一个*持久化*数据结构。
//
// 此设计是为了使这个数据结构能够在并行目录迭代器中使用。
//
// 我最初的意图是将该模块作为这个 crate 的公共 API 的一部分公开，
// 但我认为该数据结构的公共 API 太过复杂，存在非显而易见的故障模式。
// 可惜，这些问题还没有被很好地记录。

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{File, FileType};
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::gitignore::{self, Gitignore, GitignoreBuilder};
use crate::overrides::{self, Override};
use crate::pathutil::{is_hidden, strip_prefix};
use crate::types::{self, Types};
use crate::walk::DirEntry;
use crate::{Error, Match, PartialErrorBuilder};
/// `IgnoreMatch` 表示在使用 `Ignore` 匹配器时匹配的信息来源。
#[derive(Clone, Debug)]
pub struct IgnoreMatch<'a>(IgnoreMatchInner<'a>);

/// `IgnoreMatchInner` 精确描述匹配信息的来源。为了将来能够扩展更多的匹配器，该类型是私有的。
#[derive(Clone, Debug)]
enum IgnoreMatchInner<'a> {
    Override(overrides::Glob<'a>),
    Gitignore(&'a gitignore::Glob),
    Types(types::Glob<'a>),
    Hidden,
}

impl<'a> IgnoreMatch<'a> {
    fn overrides(x: overrides::Glob<'a>) -> IgnoreMatch<'a> {
        IgnoreMatch(IgnoreMatchInner::Override(x))
    }

    fn gitignore(x: &'a gitignore::Glob) -> IgnoreMatch<'a> {
        IgnoreMatch(IgnoreMatchInner::Gitignore(x))
    }

    fn types(x: types::Glob<'a>) -> IgnoreMatch<'a> {
        IgnoreMatch(IgnoreMatchInner::Types(x))
    }

    fn hidden() -> IgnoreMatch<'static> {
        IgnoreMatch(IgnoreMatchInner::Hidden)
    }
}

/// 忽略匹配器的选项，用于匹配器本身和构建器之间共享。
#[derive(Clone, Copy, Debug)]
struct IgnoreOptions {
    /// 是否忽略隐藏文件路径。
    hidden: bool,
    /// 是否读取 .ignore 文件。
    ignore: bool,
    /// 是否遵循父目录中的任何 ignore 文件。
    parents: bool,
    /// 是否读取 git 的全局 gitignore 文件。
    git_global: bool,
    /// 是否读取 .gitignore 文件。
    git_ignore: bool,
    /// 是否读取 .git/info/exclude 文件。
    git_exclude: bool,
    /// 是否忽略文件的大小写。
    ignore_case_insensitive: bool,
    /// 是否要求存在 git 仓库才能应用任何与 git 相关的忽略规则。
    require_git: bool,
}

/// `Ignore` 是一个用于递归地遍历一个或多个目录的匹配器。
#[derive(Clone, Debug)]
pub struct Ignore(Arc<IgnoreInner>);

#[derive(Clone, Debug)]
struct IgnoreInner {
    /// 所有已编译为匹配器的现有目录的映射。
    ///
    /// 注意，此映射在匹配时永不使用，只在添加新的父目录匹配器时使用。
    /// 这避免了在搜索许多路径时需要重新构建父目录的 glob 集。
    compiled: Arc<RwLock<HashMap<OsString, Ignore>>>,
    /// 用于构建匹配器的目录的路径。
    dir: PathBuf,
    /// 一个覆盖匹配器（默认为空）。
    overrides: Arc<Override>,
    /// 文件类型匹配器。
    types: Arc<Types>,
    /// 下一个要匹配的父目录。
    ///
    /// 如果这是根目录或者没有更多目录要匹配，那么 `parent` 是 `None`。
    parent: Option<Ignore>,
    /// 是否为绝对父匹配器，由 `add_parent` 添加。
    is_absolute_parent: bool,
    /// 此匹配器的绝对基本路径。仅在添加父目录时填充。
    absolute_base: Option<Arc<PathBuf>>,
    /// 由调用者指定的显式全局忽略匹配器。
    explicit_ignores: Arc<Vec<Gitignore>>,
    /// 附加到 `.ignore` 的自定义忽略文件名。
    custom_ignore_filenames: Arc<Vec<OsString>>,
    /// 自定义忽略文件的匹配器。
    custom_ignore_matcher: Gitignore,
    /// `.ignore` 文件的匹配器。
    ignore_matcher: Gitignore,
    /// 全局 gitignore 匹配器，通常来自于 $XDG_CONFIG_HOME/git/ignore。
    git_global_matcher: Arc<Gitignore>,
    /// `.gitignore` 文件的匹配器。
    git_ignore_matcher: Gitignore,
    /// `.git/info/exclude` 文件的特殊匹配器。
    git_exclude_matcher: Gitignore,
    /// 是否包含 `.git` 子目录。
    has_git: bool,
    /// 忽略配置。
    opts: IgnoreOptions,
}

impl Ignore {
    /// 返回此匹配器的目录路径。
    pub fn path(&self) -> &Path {
        &self.0.dir
    }

    /// 如果此匹配器没有父目录，则返回 true。
    pub fn is_root(&self) -> bool {
        self.0.parent.is_none()
    }

    /// 如果此匹配器是通过 `add_parents` 方法添加的，则返回 true。
    pub fn is_absolute_parent(&self) -> bool {
        self.0.is_absolute_parent
    }

    /// 返回此匹配器的父目录（如果存在）。
    pub fn parent(&self) -> Option<Ignore> {
        self.0.parent.clone()
    }

    /// 使用目录的父目录创建一个新的 `Ignore` 匹配器。
    ///
    /// 注意，只能在没有父目录的 `Ignore` 匹配器上调用此方法（即 `is_root` 返回 `true`）。
    /// 否则会触发 panic。
    pub fn add_parents<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> (Ignore, Option<Error>) {
        if !self.0.opts.parents
            && !self.0.opts.git_ignore
            && !self.0.opts.git_exclude
            && !self.0.opts.git_global
        {
            // 如果我们从不需要来自父目录的信息，则不执行任何操作。
            return (self.clone(), None);
        }
        if !self.is_root() {
            panic!("在非根匹配器上调用了 Ignore::add_parents");
        }
        let absolute_base = match path.as_ref().canonicalize() {
            Ok(path) => Arc::new(path),
            Err(_) => {
                // 在这里我们无能为力，所以只需返回现有的匹配器。
                // 我们忽略错误，以与处理忽略文件时忽略 I/O 错误的一般模式保持一致。
                return (self.clone(), None);
            }
        };
        // 从子到根的父目录列表。
        let mut parents = vec![];
        let mut path = &**absolute_base;
        while let Some(parent) = path.parent() {
            parents.push(parent);
            path = parent;
        }
        let mut errs = PartialErrorBuilder::default();
        let mut ig = self.clone();
        for parent in parents.into_iter().rev() {
            let mut compiled = self.0.compiled.write().unwrap();
            if let Some(prebuilt) = compiled.get(parent.as_os_str()) {
                ig = prebuilt.clone();
                continue;
            }
            let (mut igtmp, err) = ig.add_child_path(parent);
            errs.maybe_push(err);
            igtmp.is_absolute_parent = true;
            igtmp.absolute_base = Some(absolute_base.clone());
            igtmp.has_git =
                if self.0.opts.require_git && self.0.opts.git_ignore {
                    parent.join(".git").exists()
                } else {
                    false
                };
            ig = Ignore(Arc::new(igtmp));
            compiled.insert(parent.as_os_str().to_os_string(), ig.clone());
        }
        (ig, errs.into_error_option())
    }

    /// 为给定的子目录创建一个新的 `Ignore` 匹配器。
    ///
    /// 由于构建匹配器可能需要从多个文件中读取，因此此方法可能部分成功。
    /// 因此，总是返回一个匹配器（可能匹配空内容）并返回一个存在的错误。
    ///
    /// 注意，所有 I/O 错误都完全被忽略。
    pub fn add_child<P: AsRef<Path>>(
        &self,
        dir: P,
    ) -> (Ignore, Option<Error>) {
        let (ig, err) = self.add_child_path(dir.as_ref());
        (Ignore(Arc::new(ig)), err)
    }

    /// 类似于 `add_child`，但接受完整路径并返回 `IgnoreInner`。
    fn add_child_path(&self, dir: &Path) -> (IgnoreInner, Option<Error>) {
        let git_type = if self.0.opts.require_git
            && (self.0.opts.git_ignore || self.0.opts.git_exclude)
        {
            dir.join(".git").metadata().ok().map(|md| md.file_type())
        } else {
            None
        };
        let has_git = git_type.map(|_| true).unwrap_or(false);

        let mut errs = PartialErrorBuilder::default();
        let custom_ig_matcher = if self.0.custom_ignore_filenames.is_empty() {
            Gitignore::empty()
        } else {
            let (m, err) = create_gitignore(
                &dir,
                &dir,
                &self.0.custom_ignore_filenames,
                self.0.opts.ignore_case_insensitive,
            );
            errs.maybe_push(err);
            m
        };
        let ig_matcher = if !self.0.opts.ignore {
            Gitignore::empty()
        } else {
            let (m, err) = create_gitignore(
                &dir,
                &dir,
                &[".ignore"],
                self.0.opts.ignore_case_insensitive,
            );
            errs.maybe_push(err);
            m
        };
        let gi_matcher = if !self.0.opts.git_ignore {
            Gitignore::empty()
        } else {
            let (m, err) = create_gitignore(
                &dir,
                &dir,
                &[".gitignore"],
                self.0.opts.ignore_case_insensitive,
            );
            errs.maybe_push(err);
            m
        };
        let gi_exclude_matcher = if !self.0.opts.git_exclude {
            Gitignore::empty()
        } else {
            match resolve_git_commondir(dir, git_type) {
                Ok(git_dir) => {
                    let (m, err) = create_gitignore(
                        &dir,
                        &git_dir,
                        &["info/exclude"],
                        self.0.opts.ignore_case_insensitive,
                    );
                    errs.maybe_push(err);
                    m
                }
                Err(err) => {
                    errs.maybe_push(err);
                    Gitignore::empty()
                }
            }
        };
        let ig = IgnoreInner {
            compiled: self.0.compiled.clone(),
            dir: dir.to_path_buf(),
            overrides: self.0.overrides.clone(),
            types: self.0.types.clone(),
            parent: Some(self.clone()),
            is_absolute_parent: false,
            absolute_base: self.0.absolute_base.clone(),
            explicit_ignores: self.0.explicit_ignores.clone(),
            custom_ignore_filenames: self.0.custom_ignore_filenames.clone(),
            custom_ignore_matcher: custom_ig_matcher,
            ignore_matcher: ig_matcher,
            git_global_matcher: self.0.git_global_matcher.clone(),
            git_ignore_matcher: gi_matcher,
            git_exclude_matcher: gi_exclude_matcher,
            has_git,
            opts: self.0.opts,
        };
        (ig, errs.into_error_option())
    }

    /// 返回 true 如果至少存在一种类型的忽略规则需要匹配。
    fn has_any_ignore_rules(&self) -> bool {
        let opts = self.0.opts;
        let has_custom_ignore_files =
            !self.0.custom_ignore_filenames.is_empty();
        let has_explicit_ignores = !self.0.explicit_ignores.is_empty();

        opts.ignore
            || opts.git_global
            || opts.git_ignore
            || opts.git_exclude
            || has_custom_ignore_files
            || has_explicit_ignores
    }

    /// 类似于 `matched`，但适用于目录条目。
    pub fn matched_dir_entry<'a>(
        &'a self,
        dent: &DirEntry,
    ) -> Match<IgnoreMatch<'a>> {
        let m = self.matched(dent.path(), dent.is_dir());
        if m.is_none() && self.0.opts.hidden && is_hidden(dent) {
            return Match::Ignore(IgnoreMatch::hidden());
        }
        m
    }

    /// 返回一个匹配，指示给定的文件路径是否应该被忽略。
    ///
    /// 匹配包含有关其来源的信息。
    fn matched<'a, P: AsRef<Path>>(
        &'a self,
        path: P,
        is_dir: bool,
    ) -> Match<IgnoreMatch<'a>> {
        // 我们需要小心处理路径。如果它具有前导的 ./，则将其删除，因为它只会带来麻烦。
        let mut path = path.as_ref();
        if let Some(p) = strip_prefix("./", path) {
            path = p;
        }
        // 根据覆盖模式匹配。如果有任何一个覆盖匹配，不管是白名单还是忽略，立即返回该结果。
        // 覆盖具有最高的优先级。
        if !self.0.overrides.is_empty() {
            let mat = self
                .0
                .overrides
                .matched(path, is_dir)
                .map(IgnoreMatch::overrides);
            if !mat.is_none() {
                return mat;
            }
        }
        let mut whitelisted = Match::None;
        if self.has_any_ignore_rules() {
            let mat = self.matched_ignore(path, is_dir);
            if mat.is_ignore() {
                return mat;
            } else if mat.is_whitelist() {
                whitelisted = mat;
            }
        }
        if !self.0.types.is_empty() {
            let mat =
                self.0.types.matched(path, is_dir).map(IgnoreMatch::types);
            if mat.is_ignore() {
                return mat;
            } else if mat.is_whitelist() {
                whitelisted = mat;
            }
        }
        whitelisted
    }

    /// 仅对此目录及其所有父目录的忽略文件执行匹配。
    fn matched_ignore<'a>(
        &'a self,
        path: &Path,
        is_dir: bool,
    ) -> Match<IgnoreMatch<'a>> {
        let (
            mut m_custom_ignore,
            mut m_ignore,
            mut m_gi,
            mut m_gi_exclude,
            mut m_explicit,
        ) = (Match::None, Match::None, Match::None, Match::None, Match::None);
        let any_git =
            !self.0.opts.require_git || self.parents().any(|ig| ig.0.has_git);
        let mut saw_git = false;
        for ig in self.parents().take_while(|ig| !ig.0.is_absolute_parent) {
            if m_custom_ignore.is_none() {
                m_custom_ignore =
                    ig.0.custom_ignore_matcher
                        .matched(path, is_dir)
                        .map(IgnoreMatch::gitignore);
            }
            if m_ignore.is_none() {
                m_ignore =
                    ig.0.ignore_matcher
                        .matched(path, is_dir)
                        .map(IgnoreMatch::gitignore);
            }
            if any_git && !saw_git && m_gi.is_none() {
                m_gi =
                    ig.0.git_ignore_matcher
                        .matched(path, is_dir)
                        .map(IgnoreMatch::gitignore);
            }
            if any_git && !saw_git && m_gi_exclude.is_none() {
                m_gi_exclude =
                    ig.0.git_exclude_matcher
                        .matched(path, is_dir)
                        .map(IgnoreMatch::gitignore);
            }
            saw_git = saw_git || ig.0.has_git;
        }
        if self.0.opts.parents {
            if let Some(abs_parent_path) = self.absolute_base() {
                let path = abs_parent_path.join(path);
                for ig in
                    self.parents().skip_while(|ig| !ig.0.is_absolute_parent)
                {
                    if m_custom_ignore.is_none() {
                        m_custom_ignore =
                            ig.0.custom_ignore_matcher
                                .matched(&path, is_dir)
                                .map(IgnoreMatch::gitignore);
                    }
                    if m_ignore.is_none() {
                        m_ignore =
                            ig.0.ignore_matcher
                                .matched(&path, is_dir)
                                .map(IgnoreMatch::gitignore);
                    }
                    if any_git && !saw_git && m_gi.is_none() {
                        m_gi =
                            ig.0.git_ignore_matcher
                                .matched(&path, is_dir)
                                .map(IgnoreMatch::gitignore);
                    }
                    if any_git && !saw_git && m_gi_exclude.is_none() {
                        m_gi_exclude =
                            ig.0.git_exclude_matcher
                                .matched(&path, is_dir)
                                .map(IgnoreMatch::gitignore);
                    }
                    saw_git = saw_git || ig.0.has_git;
                }
            }
        }
        for gi in self.0.explicit_ignores.iter().rev() {
            if !m_explicit.is_none() {
                break;
            }
            m_explicit = gi.matched(path, is_dir).map(IgnoreMatch::gitignore);
        }
        let m_global = if any_git {
            self.0
                .git_global_matcher
                .matched(path, is_dir)
                .map(IgnoreMatch::gitignore)
        } else {
            Match::None
        };

        m_custom_ignore
            .or(m_ignore)
            .or(m_gi)
            .or(m_gi_exclude)
            .or(m_global)
            .or(m_explicit)
    }

    /// 返回一个迭代器，遍历父级忽略匹配器，包括本身。
    pub fn parents(&self) -> Parents<'_> {
        Parents(Some(self))
    }

    /// 如果存在至少一个绝对父级的绝对路径，则返回第一个绝对父级的绝对路径。
    fn absolute_base(&self) -> Option<&Path> {
        self.0.absolute_base.as_ref().map(|p| &***p)
    }
}

/// 一个遍历忽略匹配器及其所有父级的迭代器，包括自身。
///
/// 生命周期 'a 指的是初始 Ignore 匹配器的生命周期。
pub struct Parents<'a>(Option<&'a Ignore>);

impl<'a> Iterator for Parents<'a> {
    type Item = &'a Ignore;

    fn next(&mut self) -> Option<&'a Ignore> {
        match self.0.take() {
            None => None,
            Some(ig) => {
                self.0 = ig.0.parent.as_ref();
                Some(ig)
            }
        }
    }
}

/// 用于创建 Ignore 匹配器的构建器。
#[derive(Clone, Debug)]
pub struct IgnoreBuilder {
    /// 此 ignore 匹配器的根目录路径。
    dir: PathBuf,
    /// 一个覆盖匹配器（默认为空）。
    overrides: Arc<Override>,
    /// 一个类型匹配器（默认为空）。
    types: Arc<Types>,
    /// 显式的全局忽略匹配器。
    explicit_ignores: Vec<Gitignore>,
    /// 除了 .ignore 文件外的其他忽略文件。
    custom_ignore_filenames: Vec<OsString>,
    /// 忽略配置。
    opts: IgnoreOptions,
}
impl IgnoreBuilder {
    /// 创建一个新的 `Ignore` 匹配器的构建器。
    ///
    /// 所有相对文件路径都将相对于当前工作目录解析。
    pub fn new() -> IgnoreBuilder {
        IgnoreBuilder {
            dir: Path::new("").to_path_buf(),
            overrides: Arc::new(Override::empty()),
            types: Arc::new(Types::empty()),
            explicit_ignores: vec![],
            custom_ignore_filenames: vec![],
            opts: IgnoreOptions {
                hidden: true,
                ignore: true,
                parents: true,
                git_global: true,
                git_ignore: true,
                git_exclude: true,
                ignore_case_insensitive: false,
                require_git: true,
            },
        }
    }

    /// 构建一个新的 `Ignore` 匹配器。
    ///
    /// 返回的匹配器在添加来自目录的忽略规则之前将不会匹配任何内容。
    pub fn build(&self) -> Ignore {
        let git_global_matcher = if !self.opts.git_global {
            Gitignore::empty()
        } else {
            let mut builder = GitignoreBuilder::new("");
            builder
                .case_insensitive(self.opts.ignore_case_insensitive)
                .unwrap();
            let (gi, err) = builder.build_global();
            if let Some(err) = err {
                log::debug!("{}", err);
            }
            gi
        };

        Ignore(Arc::new(IgnoreInner {
            compiled: Arc::new(RwLock::new(HashMap::new())),
            dir: self.dir.clone(),
            overrides: self.overrides.clone(),
            types: self.types.clone(),
            parent: None,
            is_absolute_parent: true,
            absolute_base: None,
            explicit_ignores: Arc::new(self.explicit_ignores.clone()),
            custom_ignore_filenames: Arc::new(
                self.custom_ignore_filenames.clone(),
            ),
            custom_ignore_matcher: Gitignore::empty(),
            ignore_matcher: Gitignore::empty(),
            git_global_matcher: Arc::new(git_global_matcher),
            git_ignore_matcher: Gitignore::empty(),
            git_exclude_matcher: Gitignore::empty(),
            has_git: false,
            opts: self.opts,
        }))
    }

    /// 添加一个覆盖匹配器。
    ///
    /// 默认情况下，不使用任何覆盖匹配器。
    ///
    /// 这将覆盖任何先前的设置。
    pub fn overrides(&mut self, overrides: Override) -> &mut IgnoreBuilder {
        self.overrides = Arc::new(overrides);
        self
    }

    /// 添加一个文件类型匹配器。
    ///
    /// 默认情况下，不使用任何文件类型匹配器。
    ///
    /// 这将覆盖任何先前的设置。
    pub fn types(&mut self, types: Types) -> &mut IgnoreBuilder {
        self.types = Arc::new(types);
        self
    }

    /// 添加一个来自给定忽略文件路径的全局忽略匹配器。
    pub fn add_ignore(&mut self, ig: Gitignore) -> &mut IgnoreBuilder {
        self.explicit_ignores.push(ig);
        self
    }

    /// 添加自定义忽略文件名
    ///
    /// 这些忽略文件的优先级高于所有其他忽略文件。
    ///
    /// 在指定多个名称时，先前的名称优先级低于后来的名称。
    pub fn add_custom_ignore_filename<S: AsRef<OsStr>>(
        &mut self,
        file_name: S,
    ) -> &mut IgnoreBuilder {
        self.custom_ignore_filenames.push(file_name.as_ref().to_os_string());
        self
    }

    /// 启用或禁用忽略隐藏文件。
    ///
    /// 默认情况下启用。
    pub fn hidden(&mut self, yes: bool) -> &mut IgnoreBuilder {
        self.opts.hidden = yes;
        self
    }

    /// 启用或禁用读取 `.ignore` 文件。
    ///
    /// `.ignore` 文件具有与 `gitignore` 文件相同的语义，并且受到工具（如 ripgrep 和 The Silver Searcher）的支持。
    ///
    /// 默认情况下启用。
    pub fn ignore(&mut self, yes: bool) -> &mut IgnoreBuilder {
        self.opts.ignore = yes;
        self
    }

    /// 启用或禁用从父目录读取忽略文件。
    ///
    /// 如果启用，将尊重每个给定文件路径的父目录中的 .gitignore 文件。否则，将忽略它们。
    ///
    /// 默认情况下启用。
    pub fn parents(&mut self, yes: bool) -> &mut IgnoreBuilder {
        self.opts.parents = yes;
        self
    }

    /// 添加全局 gitignore 匹配器。
    ///
    /// 其优先级低于正常的 `.gitignore` 文件和 `.git/info/exclude` 文件。
    ///
    /// 这将覆盖任何先前的全局 gitignore 设置。
    ///
    /// 默认情况下启用。
    pub fn git_global(&mut self, yes: bool) -> &mut IgnoreBuilder {
        self.opts.git_global = yes;
        self
    }

    /// 启用或禁用读取 `.gitignore` 文件。
    ///
    /// `.gitignore` 文件的匹配语义如 `gitignore` 手册中所述。
    ///
    /// 默认情况下启用。
    pub fn git_ignore(&mut self, yes: bool) -> &mut IgnoreBuilder {
        self.opts.git_ignore = yes;
        self
    }

    /// 启用或禁用读取 `.git/info/exclude` 文件。
    ///
    /// `.git/info/exclude` 文件的匹配语义如 `gitignore` 手册中所述。
    ///
    /// 默认情况下启用。
    pub fn git_exclude(&mut self, yes: bool) -> &mut IgnoreBuilder {
        self.opts.git_exclude = yes;
        self
    }

    /// 是否需要存在 git 仓库以应用 git 相关的忽略规则（全局规则、.gitignore 和本地排除规则）。
    ///
    /// 当禁用时，即使在 git 仓库外搜索时，也将应用 git 相关的忽略规则。
    pub fn require_git(&mut self, yes: bool) -> &mut IgnoreBuilder {
        self.opts.require_git = yes;
        self
    }

    /// 处理忽略文件时是否区分大小写
    ///
    /// 默认情况下禁用。
    pub fn ignore_case_insensitive(
        &mut self,
        yes: bool,
    ) -> &mut IgnoreBuilder {
        self.opts.ignore_case_insensitive = yes;
        self
    }
}

/// 为给定目录创建一个新的 gitignore 匹配器。
///
/// 该匹配器旨在匹配位于 `dir` 以下的文件。
/// 忽略的 glob 从每个相对于 `dir_for_ignorefile` 的文件名中提取，按照给定的顺序排列（较早的名称优先级低于较晚的名称）。
///
/// 忽略 I/O 错误。
pub fn create_gitignore<T: AsRef<OsStr>>(
    dir: &Path,
    dir_for_ignorefile: &Path,
    names: &[T],
    case_insensitive: bool,
) -> (Gitignore, Option<Error>) {
    let mut builder = GitignoreBuilder::new(dir);
    let mut errs = PartialErrorBuilder::default();
    builder.case_insensitive(case_insensitive).unwrap();
    for name in names {
        let gipath = dir_for_ignorefile.join(name.as_ref());
        // 这个检查不是必要的，但是为了性能而添加的。特别是，
        // 一个简单的检查是否存在可能会比尝试打开文件稍微快一点。
        // 由于没有忽略文件的目录数量很可能远远超过具有忽略文件的数量，
        // 因此这个检查通常是有意义的。
        //
        // 然而，除非有证据证明不是这样的，否则我们暂时不在 Windows 上执行此操作，
        // 因为 Windows 以文件系统操作缓慢而著名。
        // 特别是，在 Windows 上是否适用这种分析还不清楚。
        //
        // 详细信息：https://github.com/BurntSushi/ripgrep/pull/1381
        if cfg!(windows) || gipath.exists() {
            errs.maybe_push_ignore_io(builder.add(gipath));
        }
    }
    let gi = match builder.build() {
        Ok(gi) => gi,
        Err(err) => {
            errs.push(err);
            GitignoreBuilder::new(dir).build().unwrap()
        }
    };
    (gi, errs.into_error_option())
}

/// 找到给定 git 工作树的 GIT_COMMON_DIR。
///
/// 这是可能包含私有忽略文件 "info/exclude" 的目录。
/// 与 git 不同，此函数不读取环境变量 GIT_DIR 和 GIT_COMMON_DIR，因为不清楚如何在搜索多个仓库时使用它们。
///
/// 忽略一些 I/O 错误。
fn resolve_git_commondir(
    dir: &Path,
    git_type: Option<FileType>,
) -> Result<PathBuf, Option<Error>> {
    let git_dir_path = || dir.join(".git");
    let git_dir = git_dir_path();
    if !git_type.map_or(false, |ft| ft.is_file()) {
        return Ok(git_dir);
    }
    let file = match File::open(git_dir) {
        Ok(file) => io::BufReader::new(file),
        Err(err) => {
            return Err(Some(Error::Io(err).with_path(git_dir_path())));
        }
    };
    let dot_git_line = match file.lines().next() {
        Some(Ok(line)) => line,
        Some(Err(err)) => {
            return Err(Some(Error::Io(err).with_path(git_dir_path())));
        }
        None => return Err(None),
    };
    if !dot_git_line.starts_with("gitdir: ") {
        return Err(None);
    }
    let real_git_dir = PathBuf::from(&dot_git_line["gitdir: ".len()..]);
    let git_commondir_file = || real_git_dir.join("commondir");
    let file = match File::open(git_commondir_file()) {
        Ok(file) => io::BufReader::new(file),
        Err(_) => return Err(None),
    };
    let commondir_line = match file.lines().next() {
        Some(Ok(line)) => line,
        Some(Err(err)) => {
            return Err(Some(Error::Io(err).with_path(git_commondir_file())));
        }
        None => return Err(None),
    };
    let commondir_abs = if commondir_line.starts_with(".") {
        real_git_dir.join(commondir_line) // 相对 commondir
    } else {
        PathBuf::from(commondir_line)
    };
    Ok(commondir_abs)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::Path;

    use crate::dir::IgnoreBuilder;
    use crate::gitignore::Gitignore;
    use crate::tests::TempDir;
    use crate::Error;

    fn wfile<P: AsRef<Path>>(path: P, contents: &str) {
        let mut file = File::create(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    fn mkdirp<P: AsRef<Path>>(path: P) {
        fs::create_dir_all(path).unwrap();
    }

    fn partial(err: Error) -> Vec<Error> {
        match err {
            Error::Partial(errs) => errs,
            _ => panic!("expected partial error but got {:?}", err),
        }
    }

    fn tmpdir() -> TempDir {
        TempDir::new().unwrap()
    }

    #[test]
    fn explicit_ignore() {
        let td = tmpdir();
        wfile(td.path().join("not-an-ignore"), "foo\n!bar");

        let (gi, err) = Gitignore::new(td.path().join("not-an-ignore"));
        assert!(err.is_none());
        let (ig, err) =
            IgnoreBuilder::new().add_ignore(gi).build().add_child(td.path());
        assert!(err.is_none());
        assert!(ig.matched("foo", false).is_ignore());
        assert!(ig.matched("bar", false).is_whitelist());
        assert!(ig.matched("baz", false).is_none());
    }

    #[test]
    fn git_exclude() {
        let td = tmpdir();
        mkdirp(td.path().join(".git/info"));
        wfile(td.path().join(".git/info/exclude"), "foo\n!bar");

        let (ig, err) = IgnoreBuilder::new().build().add_child(td.path());
        assert!(err.is_none());
        assert!(ig.matched("foo", false).is_ignore());
        assert!(ig.matched("bar", false).is_whitelist());
        assert!(ig.matched("baz", false).is_none());
    }

    #[test]
    fn gitignore() {
        let td = tmpdir();
        mkdirp(td.path().join(".git"));
        wfile(td.path().join(".gitignore"), "foo\n!bar");

        let (ig, err) = IgnoreBuilder::new().build().add_child(td.path());
        assert!(err.is_none());
        assert!(ig.matched("foo", false).is_ignore());
        assert!(ig.matched("bar", false).is_whitelist());
        assert!(ig.matched("baz", false).is_none());
    }

    #[test]
    fn gitignore_no_git() {
        let td = tmpdir();
        wfile(td.path().join(".gitignore"), "foo\n!bar");

        let (ig, err) = IgnoreBuilder::new().build().add_child(td.path());
        assert!(err.is_none());
        assert!(ig.matched("foo", false).is_none());
        assert!(ig.matched("bar", false).is_none());
        assert!(ig.matched("baz", false).is_none());
    }

    #[test]
    fn gitignore_allowed_no_git() {
        let td = tmpdir();
        wfile(td.path().join(".gitignore"), "foo\n!bar");

        let (ig, err) = IgnoreBuilder::new()
            .require_git(false)
            .build()
            .add_child(td.path());
        assert!(err.is_none());
        assert!(ig.matched("foo", false).is_ignore());
        assert!(ig.matched("bar", false).is_whitelist());
        assert!(ig.matched("baz", false).is_none());
    }

    #[test]
    fn ignore() {
        let td = tmpdir();
        wfile(td.path().join(".ignore"), "foo\n!bar");

        let (ig, err) = IgnoreBuilder::new().build().add_child(td.path());
        assert!(err.is_none());
        assert!(ig.matched("foo", false).is_ignore());
        assert!(ig.matched("bar", false).is_whitelist());
        assert!(ig.matched("baz", false).is_none());
    }

    #[test]
    fn custom_ignore() {
        let td = tmpdir();
        let custom_ignore = ".customignore";
        wfile(td.path().join(custom_ignore), "foo\n!bar");

        let (ig, err) = IgnoreBuilder::new()
            .add_custom_ignore_filename(custom_ignore)
            .build()
            .add_child(td.path());
        assert!(err.is_none());
        assert!(ig.matched("foo", false).is_ignore());
        assert!(ig.matched("bar", false).is_whitelist());
        assert!(ig.matched("baz", false).is_none());
    }

    // Tests that a custom ignore file will override an .ignore.
    #[test]
    fn custom_ignore_over_ignore() {
        let td = tmpdir();
        let custom_ignore = ".customignore";
        wfile(td.path().join(".ignore"), "foo");
        wfile(td.path().join(custom_ignore), "!foo");

        let (ig, err) = IgnoreBuilder::new()
            .add_custom_ignore_filename(custom_ignore)
            .build()
            .add_child(td.path());
        assert!(err.is_none());
        assert!(ig.matched("foo", false).is_whitelist());
    }

    // Tests that earlier custom ignore files have lower precedence than later.
    #[test]
    fn custom_ignore_precedence() {
        let td = tmpdir();
        let custom_ignore1 = ".customignore1";
        let custom_ignore2 = ".customignore2";
        wfile(td.path().join(custom_ignore1), "foo");
        wfile(td.path().join(custom_ignore2), "!foo");

        let (ig, err) = IgnoreBuilder::new()
            .add_custom_ignore_filename(custom_ignore1)
            .add_custom_ignore_filename(custom_ignore2)
            .build()
            .add_child(td.path());
        assert!(err.is_none());
        assert!(ig.matched("foo", false).is_whitelist());
    }

    // Tests that an .ignore will override a .gitignore.
    #[test]
    fn ignore_over_gitignore() {
        let td = tmpdir();
        wfile(td.path().join(".gitignore"), "foo");
        wfile(td.path().join(".ignore"), "!foo");

        let (ig, err) = IgnoreBuilder::new().build().add_child(td.path());
        assert!(err.is_none());
        assert!(ig.matched("foo", false).is_whitelist());
    }

    // Tests that exclude has lower precedent than both .ignore and .gitignore.
    #[test]
    fn exclude_lowest() {
        let td = tmpdir();
        wfile(td.path().join(".gitignore"), "!foo");
        wfile(td.path().join(".ignore"), "!bar");
        mkdirp(td.path().join(".git/info"));
        wfile(td.path().join(".git/info/exclude"), "foo\nbar\nbaz");

        let (ig, err) = IgnoreBuilder::new().build().add_child(td.path());
        assert!(err.is_none());
        assert!(ig.matched("baz", false).is_ignore());
        assert!(ig.matched("foo", false).is_whitelist());
        assert!(ig.matched("bar", false).is_whitelist());
    }

    #[test]
    fn errored() {
        let td = tmpdir();
        wfile(td.path().join(".gitignore"), "{foo");

        let (_, err) = IgnoreBuilder::new().build().add_child(td.path());
        assert!(err.is_some());
    }

    #[test]
    fn errored_both() {
        let td = tmpdir();
        wfile(td.path().join(".gitignore"), "{foo");
        wfile(td.path().join(".ignore"), "{bar");

        let (_, err) = IgnoreBuilder::new().build().add_child(td.path());
        assert_eq!(2, partial(err.expect("an error")).len());
    }

    #[test]
    fn errored_partial() {
        let td = tmpdir();
        mkdirp(td.path().join(".git"));
        wfile(td.path().join(".gitignore"), "{foo\nbar");

        let (ig, err) = IgnoreBuilder::new().build().add_child(td.path());
        assert!(err.is_some());
        assert!(ig.matched("bar", false).is_ignore());
    }

    #[test]
    fn errored_partial_and_ignore() {
        let td = tmpdir();
        wfile(td.path().join(".gitignore"), "{foo\nbar");
        wfile(td.path().join(".ignore"), "!bar");

        let (ig, err) = IgnoreBuilder::new().build().add_child(td.path());
        assert!(err.is_some());
        assert!(ig.matched("bar", false).is_whitelist());
    }

    #[test]
    fn not_present_empty() {
        let td = tmpdir();

        let (_, err) = IgnoreBuilder::new().build().add_child(td.path());
        assert!(err.is_none());
    }

    #[test]
    fn stops_at_git_dir() {
        // This tests that .gitignore files beyond a .git barrier aren't
        // matched, but .ignore files are.
        let td = tmpdir();
        mkdirp(td.path().join(".git"));
        mkdirp(td.path().join("foo/.git"));
        wfile(td.path().join(".gitignore"), "foo");
        wfile(td.path().join(".ignore"), "bar");

        let ig0 = IgnoreBuilder::new().build();
        let (ig1, err) = ig0.add_child(td.path());
        assert!(err.is_none());
        let (ig2, err) = ig1.add_child(ig1.path().join("foo"));
        assert!(err.is_none());

        assert!(ig1.matched("foo", false).is_ignore());
        assert!(ig2.matched("foo", false).is_none());

        assert!(ig1.matched("bar", false).is_ignore());
        assert!(ig2.matched("bar", false).is_ignore());
    }

    #[test]
    fn absolute_parent() {
        let td = tmpdir();
        mkdirp(td.path().join(".git"));
        mkdirp(td.path().join("foo"));
        wfile(td.path().join(".gitignore"), "bar");

        // First, check that the parent gitignore file isn't detected if the
        // parent isn't added. This establishes a baseline.
        let ig0 = IgnoreBuilder::new().build();
        let (ig1, err) = ig0.add_child(td.path().join("foo"));
        assert!(err.is_none());
        assert!(ig1.matched("bar", false).is_none());

        // Second, check that adding a parent directory actually works.
        let ig0 = IgnoreBuilder::new().build();
        let (ig1, err) = ig0.add_parents(td.path().join("foo"));
        assert!(err.is_none());
        let (ig2, err) = ig1.add_child(td.path().join("foo"));
        assert!(err.is_none());
        assert!(ig2.matched("bar", false).is_ignore());
    }

    #[test]
    fn absolute_parent_anchored() {
        let td = tmpdir();
        mkdirp(td.path().join(".git"));
        mkdirp(td.path().join("src/llvm"));
        wfile(td.path().join(".gitignore"), "/llvm/\nfoo");

        let ig0 = IgnoreBuilder::new().build();
        let (ig1, err) = ig0.add_parents(td.path().join("src"));
        assert!(err.is_none());
        let (ig2, err) = ig1.add_child("src");
        assert!(err.is_none());

        assert!(ig1.matched("llvm", true).is_none());
        assert!(ig2.matched("llvm", true).is_none());
        assert!(ig2.matched("src/llvm", true).is_none());
        assert!(ig2.matched("foo", false).is_ignore());
        assert!(ig2.matched("src/foo", false).is_ignore());
    }

    #[test]
    fn git_info_exclude_in_linked_worktree() {
        let td = tmpdir();
        let git_dir = td.path().join(".git");
        mkdirp(git_dir.join("info"));
        wfile(git_dir.join("info/exclude"), "ignore_me");
        mkdirp(git_dir.join("worktrees/linked-worktree"));
        let commondir_path =
            || git_dir.join("worktrees/linked-worktree/commondir");
        mkdirp(td.path().join("linked-worktree"));
        let worktree_git_dir_abs = format!(
            "gitdir: {}",
            git_dir.join("worktrees/linked-worktree").to_str().unwrap(),
        );
        wfile(td.path().join("linked-worktree/.git"), &worktree_git_dir_abs);

        // relative commondir
        wfile(commondir_path(), "../..");
        let ib = IgnoreBuilder::new().build();
        let (ignore, err) = ib.add_child(td.path().join("linked-worktree"));
        assert!(err.is_none());
        assert!(ignore.matched("ignore_me", false).is_ignore());

        // absolute commondir
        wfile(commondir_path(), git_dir.to_str().unwrap());
        let (ignore, err) = ib.add_child(td.path().join("linked-worktree"));
        assert!(err.is_none());
        assert!(ignore.matched("ignore_me", false).is_ignore());

        // missing commondir file
        assert!(fs::remove_file(commondir_path()).is_ok());
        let (_, err) = ib.add_child(td.path().join("linked-worktree"));
        // We squash the error in this case, because it occurs in repositories
        // that are not linked worktrees but have submodules.
        assert!(err.is_none());

        wfile(td.path().join("linked-worktree/.git"), "garbage");
        let (_, err) = ib.add_child(td.path().join("linked-worktree"));
        assert!(err.is_none());

        wfile(td.path().join("linked-worktree/.git"), "gitdir: garbage");
        let (_, err) = ib.add_child(td.path().join("linked-worktree"));
        assert!(err.is_none());
    }
}
