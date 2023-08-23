/*!
`gitignore` 模块提供了一种匹配来自 gitignore 文件的 glob 与文件路径的方法。

请注意，此模块从头开始实现了 `gitignore` 手册中描述的规范。也就是说，此模块*不会*使用 `git` 命令行工具。
*/

use std::cell::RefCell;
use std::env;
use std::fs::File;
use std::io::{self, BufRead, Read};
use std::path::{Path, PathBuf};
use std::str;
use std::sync::Arc;

use globset::{Candidate, GlobBuilder, GlobSet, GlobSetBuilder};
use regex::bytes::Regex;
use thread_local::ThreadLocal;

use crate::pathutil::{is_file_name, strip_prefix};
use crate::{Error, Match, PartialErrorBuilder};
/// Glob 表示 gitignore 文件中的一个 glob 表达式。
///
/// 用于报告在一个或多个 gitignore 文件中匹配的最高优先级 glob 的信息。
#[derive(Clone, Debug)]
pub struct Glob {
    /// 定义这个 glob 表达式的文件路径。
    from: Option<PathBuf>,
    /// 原始的 glob 字符串。
    original: String,
    /// 转换为正则表达式的实际 glob 字符串。
    actual: String,
    /// 是否为白名单的 glob。
    is_whitelist: bool,
    /// 是否只匹配目录。
    is_only_dir: bool,
}

impl Glob {
    /// 返回定义此 glob 表达式的文件路径。
    pub fn from(&self) -> Option<&Path> {
        self.from.as_ref().map(|p| &**p)
    }

    /// 原始的 glob 表达式，就如在 gitignore 文件中定义的那样。
    pub fn original(&self) -> &str {
        &self.original
    }

    /// 编译为符合 gitignore 语义的实际 glob 表达式。
    pub fn actual(&self) -> &str {
        &self.actual
    }

    /// 是否为白名单的 glob。
    pub fn is_whitelist(&self) -> bool {
        self.is_whitelist
    }

    /// 是否只匹配目录。
    pub fn is_only_dir(&self) -> bool {
        self.is_only_dir
    }

    /// 如果且仅当此 glob 具有 `**/` 前缀时返回 true。
    fn has_doublestar_prefix(&self) -> bool {
        self.actual.starts_with("**/") || self.actual == "**"
    }
}

/// Gitignore 是一个匹配器，用于匹配位于同一目录中一个或多个 gitignore 文件中的 glob 表达式。
#[derive(Clone, Debug)]
pub struct Gitignore {
    set: GlobSet,
    root: PathBuf,
    globs: Vec<Glob>,
    num_ignores: u64,
    num_whitelists: u64,
    matches: Option<Arc<ThreadLocal<RefCell<Vec<usize>>>>>,
}

impl Gitignore {
    /// 根据给定的 gitignore 文件路径创建一个新的 gitignore 匹配器。
    ///
    /// 如果希望在单个匹配器中包含多个 gitignore 文件，或者从不同来源读取 gitignore glob 表达式，
    /// 则可以使用 `GitignoreBuilder`。
    ///
    /// 即使它是空的，它也总是返回一个有效的匹配器。特别地，一个 gitignore 文件可以部分有效，
    /// 例如，一个 glob 无效但其余的有效。
    ///
    /// 请注意，会忽略 I/O 错误。要更精细地控制错误，可以使用 `GitignoreBuilder`。
    pub fn new<P: AsRef<Path>>(
        gitignore_path: P,
    ) -> (Gitignore, Option<Error>) {
        let path = gitignore_path.as_ref();
        let parent = path.parent().unwrap_or(Path::new("/"));
        let mut builder = GitignoreBuilder::new(parent);
        let mut errs = PartialErrorBuilder::default();
        errs.maybe_push_ignore_io(builder.add(path));
        match builder.build() {
            Ok(gi) => (gi, errs.into_error_option()),
            Err(err) => {
                errs.push(err);
                (Gitignore::empty(), errs.into_error_option())
            }
        }
    }

    /// 根据全局 gitignore 文件（如果存在）创建一个新的 gitignore 匹配器。
    ///
    /// 全局配置文件的路径由 git 的 `core.excludesFile` 配置选项指定。
    ///
    /// Git 的配置文件位置是 `$HOME/.gitconfig`。如果 `$HOME/.gitconfig` 不存在或未指定
    /// `core.excludesFile`，则将读取 `$XDG_CONFIG_HOME/git/ignore`。
    /// 如果 `$XDG_CONFIG_HOME` 未设置或为空，则使用 `$HOME/.config/git/ignore`。
    pub fn global() -> (Gitignore, Option<Error>) {
        GitignoreBuilder::new("").build_global()
    }

    /// 创建一个新的空的 gitignore 匹配器，它永远不会匹配任何内容。
    ///
    /// 它的路径为空。
    pub fn empty() -> Gitignore {
        Gitignore {
            set: GlobSet::empty(),
            root: PathBuf::from(""),
            globs: vec![],
            num_ignores: 0,
            num_whitelists: 0,
            matches: None,
        }
    }

    /// 返回包含此 gitignore 匹配器的目录。
    ///
    /// 所有匹配都相对于此路径进行。
    pub fn path(&self) -> &Path {
        &*self.root
    }

    /// 当且仅当此 gitignore 具有零个 glob 时（因此永远不会匹配任何文件路径）返回 true。
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// 返回总的 glob 数量，应该等于 `num_ignores + num_whitelists`。
    pub fn len(&self) -> usize {
        self.set.len()
    }

    /// 返回总的忽略 glob 数量。
    pub fn num_ignores(&self) -> u64 {
        self.num_ignores
    }

    /// 返回总的白名单 glob 数量。
    pub fn num_whitelists(&self) -> u64 {
        self.num_whitelists
    }

    /// 返回给定路径（文件或目录）是否在此 gitignore 匹配器中匹配了一个模式。
    ///
    /// 如果 `is_dir` 为 true，则路径应为目录；否则为文件。
    ///
    /// 给定路径相对于构建匹配器时给定的路径进行匹配。具体来说，在匹配 `path` 之前，会删除其前缀
    /// （由包含此 gitignore 的目录的公共后缀确定）。如果没有公共后缀/前缀重叠，则假定 `path`
    /// 相对于此匹配器。
    pub fn matched<P: AsRef<Path>>(
        &self,
        path: P,
        is_dir: bool,
    ) -> Match<&Glob> {
        if self.is_empty() {
            return Match::None;
        }
        self.matched_stripped(self.strip(path.as_ref()), is_dir)
    }

    /// 返回给定路径（文件或目录，应在根目录下）或其任何父目录（最多到根目录）是否在此 gitignore
    /// 匹配器中匹配了一个模式。
    ///
    /// 注意：与从上到下遍历目录层次结构并匹配条目相比，此方法的效率更低。但是，在存在没有层次结构的
    /// 情况下，有一个可用的路径列表时更容易使用。
    ///
    /// 如果 `is_dir` 为 true，则路径应为目录；否则为文件。
    ///
    /// 给定路径相对于构建匹配器时给定的路径进行匹配。具体来说，在匹配 `path` 之前，会删除其前缀
    /// （由包含此 gitignore 的目录的公共后缀确定）。如果没有公共后缀/前缀重叠，则假定 `path`
    /// 相对于此匹配器。
    ///
    /// # Panics
    ///
    /// 如果给定的文件路径不在此匹配器的根路径下，则此方法会 panic。
    pub fn matched_path_or_any_parents<P: AsRef<Path>>(
        &self,
        path: P,
        is_dir: bool,
    ) -> Match<&Glob> {
        if self.is_empty() {
            return Match::None;
        }
        let mut path = self.strip(path.as_ref());
        assert!(!path.has_root(), "预期路径在根目录下");

        match self.matched_stripped(path, is_dir) {
            Match::None => (), // 向上遍历
            a_match => return a_match,
        }
        while let Some(parent) = path.parent() {
            match self.matched_stripped(parent, /* is_dir */ true) {
                Match::None => path = parent, // 向上遍历
                a_match => return a_match,
            }
        }
        Match::None
    }
    /// 与 matched 相似，但接受已经被剥离的路径。
    fn matched_stripped<P: AsRef<Path>>(
        &self,
        path: P,
        is_dir: bool,
    ) -> Match<&Glob> {
        if self.is_empty() {
            return Match::None;
        }
        let path = path.as_ref();
        let _matches = self.matches.as_ref().unwrap().get_or_default();
        let mut matches = _matches.borrow_mut();
        let candidate = Candidate::new(path);
        self.set.matches_candidate_into(&candidate, &mut *matches);
        for &i in matches.iter().rev() {
            let glob = &self.globs[i];
            if !glob.is_only_dir() || is_dir {
                return if glob.is_whitelist() {
                    Match::Whitelist(glob)
                } else {
                    Match::Ignore(glob)
                };
            }
        }
        Match::None
    }

    /// 剥离给定的路径，使其适用于与此 gitignore 匹配器进行匹配。
    fn strip<'a, P: 'a + AsRef<Path> + ?Sized>(
        &'a self,
        path: &'a P,
    ) -> &'a Path {
        let mut path = path.as_ref();
        // 一个前导的 ./ 是多余的。我们还从我们的 gitignore 根路径中剥离了它，
        // 因此我们需要从候选路径中剥离它。
        if let Some(p) = strip_prefix("./", path) {
            path = p;
        }
        // 剥离候选路径与 gitignore 的根目录之间的任何共同前缀，以确保我们正确处理相对匹配。
        // 但是，文件名可能没有任何目录组件，此时我们不希望意外地剥离文件名的任何部分。
        //
        // 作为一个额外的特殊情况，如果根路径仅为 `.`，那么我们不应尝试剥离任何内容，例如，当路径以 `.` 开头时。
        if self.root != Path::new(".") && !is_file_name(path) {
            if let Some(p) = strip_prefix(&self.root, path) {
                path = p;
                // 如果剩下一个前导斜杠，则将其去除。
                if let Some(p) = strip_prefix("/", path) {
                    path = p;
                }
            }
        }
        path
    }
}

/// 构建来自 .gitignore 文件中单个 glob 集的匹配器。
#[derive(Clone, Debug)]
pub struct GitignoreBuilder {
    builder: GlobSetBuilder,
    root: PathBuf,
    globs: Vec<Glob>,
    case_insensitive: bool,
}

impl GitignoreBuilder {
    /// 为一个 gitignore 文件创建一个新的构建器。
    ///
    /// 给定的路径应该是应该匹配此 gitignore 文件的 glob 的路径。注意，路径总是相对于
    /// 此处给定的根路径进行匹配。一般来说，根路径应对应包含 `.gitignore` 文件的*目录*。
    pub fn new<P: AsRef<Path>>(root: P) -> GitignoreBuilder {
        let root = root.as_ref();
        GitignoreBuilder {
            builder: GlobSetBuilder::new(),
            root: strip_prefix("./", root).unwrap_or(root).to_path_buf(),
            globs: vec![],
            case_insensitive: false,
        }
    }

    /// 从迄今为止添加的 glob 构建一个新的匹配器。
    ///
    /// 一旦构建了匹配器，就无法再向其中添加新的 glob。
    pub fn build(&self) -> Result<Gitignore, Error> {
        let nignore = self.globs.iter().filter(|g| !g.is_whitelist()).count();
        let nwhite = self.globs.iter().filter(|g| g.is_whitelist()).count();
        let set = self
            .builder
            .build()
            .map_err(|err| Error::Glob { glob: None, err: err.to_string() })?;
        Ok(Gitignore {
            set: set,
            root: self.root.clone(),
            globs: self.globs.clone(),
            num_ignores: nignore as u64,
            num_whitelists: nwhite as u64,
            matches: Some(Arc::new(ThreadLocal::default())),
        })
    }

    /// 使用此构建器中的配置构建全局 gitignore 匹配器。
    ///
    /// 与 `build` 不同，这会消耗构建器的所有权，因为它必须对构建器进行修改以添加全局 gitignore 的 glob。
    ///
    /// 请注意，这会忽略传递给此构建器构造函数的路径，并且会自动从 git 的全局配置中派生路径。
    pub fn build_global(mut self) -> (Gitignore, Option<Error>) {
        match gitconfig_excludes_path() {
            None => (Gitignore::empty(), None),
            Some(path) => {
                if !path.is_file() {
                    (Gitignore::empty(), None)
                } else {
                    let mut errs = PartialErrorBuilder::default();
                    errs.maybe_push_ignore_io(self.add(path));
                    match self.build() {
                        Ok(gi) => (gi, errs.into_error_option()),
                        Err(err) => {
                            errs.push(err);
                            (Gitignore::empty(), errs.into_error_option())
                        }
                    }
                }
            }
        }
    }

    /// 从给定的文件路径添加每个 glob。
    ///
    /// 给定的文件应格式化为 `gitignore` 文件。
    ///
    /// 请注意，可能会返回部分错误。例如，如果添加一个 glob 时出现问题，将返回一个针对该错误的错误，但所有其他有效的 glob 仍将被添加。
    pub fn add<P: AsRef<Path>>(&mut self, path: P) -> Option<Error> {
        let path = path.as_ref();
        let file = match File::open(path) {
            Err(err) => return Some(Error::Io(err).with_path(path)),
            Ok(file) => file,
        };
        let rdr = io::BufReader::new(file);
        let mut errs = PartialErrorBuilder::default();
        for (i, line) in rdr.lines().enumerate() {
            let lineno = (i + 1) as u64;
            let line = match line {
                Ok(line) => line,
                Err(err) => {
                    errs.push(Error::Io(err).tagged(path, lineno));
                    break;
                }
            };
            if let Err(err) = self.add_line(Some(path.to_path_buf()), &line) {
                errs.push(err.tagged(path, lineno));
            }
        }
        errs.into_error_option()
    }

    /// 从给定的字符串添加每个 glob 行。
    ///
    /// 如果此字符串来自特定的 `gitignore` 文件，则应在此处提供其路径。
    ///
    /// 给定的字符串应格式化为 `gitignore` 文件。
    #[cfg(test)]
    fn add_str(
        &mut self,
        from: Option<PathBuf>,
        gitignore: &str,
    ) -> Result<&mut GitignoreBuilder, Error> {
        for line in gitignore.lines() {
            self.add_line(from.clone(), line)?;
        }
        Ok(self)
    }
    /// 向此构建器添加来自 gitignore 文件的一行。
    ///
    /// 如果此行来自特定的 `gitignore` 文件，则应在此处提供其路径。
    ///
    /// 如果无法将行解析为 glob，则会返回错误。
    pub fn add_line(
        &mut self,
        from: Option<PathBuf>,
        mut line: &str,
    ) -> Result<&mut GitignoreBuilder, Error> {
        #![allow(deprecated)]

        if line.starts_with("#") {
            return Ok(self);
        }
        if !line.ends_with("\\ ") {
            line = line.trim_right();
        }
        if line.is_empty() {
            return Ok(self);
        }
        let mut glob = Glob {
            from: from,
            original: line.to_string(),
            actual: String::new(),
            is_whitelist: false,
            is_only_dir: false,
        };
        let mut is_absolute = false;
        if line.starts_with("\\!") || line.starts_with("\\#") {
            line = &line[1..];
            is_absolute = line.chars().nth(0) == Some('/');
        } else {
            if line.starts_with("!") {
                glob.is_whitelist = true;
                line = &line[1..];
            }
            if line.starts_with("/") {
                // `man gitignore` 表示，如果一个 glob 以斜杠开头，
                // 那么这个 glob 只能匹配路径的开头（相对于 gitignore 的位置）。
                // 我们通过简单地禁止通配符与 / 匹配来实现这一点。
                line = &line[1..];
                is_absolute = true;
            }
        }
        // 如果以斜杠结尾，则此项应仅匹配目录，但在进行 glob 匹配时不应使用斜杠。
        if line.as_bytes().last() == Some(&b'/') {
            glob.is_only_dir = true;
            line = &line[..line.len() - 1];
            // 如果斜杠被转义，则移除转义。
            // 参见：https://github.com/BurntSushi/ripgrep/issues/2236
            if line.as_bytes().last() == Some(&b'\\') {
                line = &line[..line.len() - 1];
            }
        }
        glob.actual = line.to_string();
        // 如果有一个字面上的斜杠，则这是一个必须匹配整个路径名的 glob。
        // 否则，我们应该让它在任何地方匹配，所以使用 **/ 前缀。
        if !is_absolute && !line.chars().any(|c| c == '/') {
            // ... 但前提是我们还没有 **/ 前缀。
            if !glob.has_doublestar_prefix() {
                glob.actual = format!("**/{}", glob.actual);
            }
        }
        // 如果 glob 以 `/**` 结尾，则我们应该仅匹配目录内的所有内容，但不包括目录本身。
        // 标准的 glob 会匹配目录本身。所以我们添加 `/*` 来强制执行这一点。
        if glob.actual.ends_with("/**") {
            glob.actual = format!("{}/*", glob.actual);
        }
        let parsed = GlobBuilder::new(&glob.actual)
            .literal_separator(true)
            .case_insensitive(self.case_insensitive)
            .backslash_escape(true)
            .build()
            .map_err(|err| Error::Glob {
                glob: Some(glob.original.clone()),
                err: err.kind().to_string(),
            })?;
        self.builder.add(parsed);
        self.globs.push(glob);
        Ok(self)
    }

    /// 切换 glob 是否应进行大小写不敏感匹配。
    ///
    /// 更改此选项后，只有在更改后添加的 glob 才会受到影响。
    ///
    /// 默认情况下，此选项是禁用的。
    pub fn case_insensitive(
        &mut self,
        yes: bool,
    ) -> Result<&mut GitignoreBuilder, Error> {
        // TODO: 这不应返回 `Result`。在下一个版本中修复这个问题。
        self.case_insensitive = yes;
        Ok(self)
    }
}
/// 返回当前环境的全局 gitignore 文件的文件路径。
///
/// 注意，返回的文件路径可能不存在。
pub fn gitconfig_excludes_path() -> Option<PathBuf> {
    // git 支持 $HOME/.gitconfig 和 $XDG_CONFIG_HOME/git/config。
    // 需要注意的是，两者可以同时有效，其中 $HOME/.gitconfig 优先。
    match gitconfig_home_contents().and_then(|x| parse_excludes_file(&x)) {
        Some(path) => return Some(path),
        None => {}
    }
    match gitconfig_xdg_contents().and_then(|x| parse_excludes_file(&x)) {
        Some(path) => return Some(path),
        None => {}
    }
    excludes_file_default()
}

/// 返回 git 全局配置文件的文件内容，如果存在的话，在用户的主目录中。
fn gitconfig_home_contents() -> Option<Vec<u8>> {
    let home = match home_dir() {
        None => return None,
        Some(home) => home,
    };
    let mut file = match File::open(home.join(".gitconfig")) {
        Err(_) => return None,
        Ok(file) => io::BufReader::new(file),
    };
    let mut contents = vec![];
    file.read_to_end(&mut contents).ok().map(|_| contents)
}

/// 返回 git 全局配置文件的文件内容，如果存在的话，在用户的 XDG_CONFIG_HOME 目录中。
fn gitconfig_xdg_contents() -> Option<Vec<u8>> {
    let path = env::var_os("XDG_CONFIG_HOME")
        .and_then(|x| if x.is_empty() { None } else { Some(PathBuf::from(x)) })
        .or_else(|| home_dir().map(|p| p.join(".config")))
        .map(|x| x.join("git/config"));
    let mut file = match path.and_then(|p| File::open(p).ok()) {
        None => return None,
        Some(file) => io::BufReader::new(file),
    };
    let mut contents = vec![];
    file.read_to_end(&mut contents).ok().map(|_| contents)
}

/// 返回全局 .gitignore 文件的默认文件路径。
///
/// 具体来说，这会考虑 XDG_CONFIG_HOME。
fn excludes_file_default() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .and_then(|x| if x.is_empty() { None } else { Some(PathBuf::from(x)) })
        .or_else(|| home_dir().map(|p| p.join(".config")))
        .map(|x| x.join("git/ignore"))
}

/// 从给定的原始文件内容中提取 git 的 `core.excludesfile` 配置设置。
fn parse_excludes_file(data: &[u8]) -> Option<PathBuf> {
    // 注意：这是一种懒惰的方法，虽然不是严格正确的，但在更多情况下可能有效。
    // 我们理想情况下应该有一个完整的 INI 解析器。但是这个方法可能会在许多情况下有效。
    lazy_static::lazy_static! {
        static ref RE: Regex = Regex::new(
            r"(?xim-u)
            ^[[:space:]]*excludesfile[[:space:]]*
            =
            [[:space:]]*(.+)[[:space:]]*$
            "
        ).unwrap();
    };
    let caps = match RE.captures(data) {
        None => return None,
        Some(caps) => caps,
    };
    str::from_utf8(&caps[1]).ok().map(|s| PathBuf::from(expand_tilde(s)))
}

/// 将文件路径中的 ~ 扩展为 $HOME 的值。
fn expand_tilde(path: &str) -> String {
    let home = match home_dir() {
        None => return path.to_string(),
        Some(home) => home.to_string_lossy().into_owned(),
    };
    path.replace("~", &home)
}

/// 返回用户主目录的位置。
fn home_dir() -> Option<PathBuf> {
    // 目前使用 env::home_dir 是可以的。其存在的问题，在我看来，是非常小的边缘情况。
    // 我们最终可能会迁移到 `dirs` crate 来获取正确的实现。
    #![allow(deprecated)]
    env::home_dir()
}

#[cfg(test)]
mod tests {
    use super::{Gitignore, GitignoreBuilder};
    use std::path::Path;

    fn gi_from_str<P: AsRef<Path>>(root: P, s: &str) -> Gitignore {
        let mut builder = GitignoreBuilder::new(root);
        builder.add_str(None, s).unwrap();
        builder.build().unwrap()
    }

    macro_rules! ignored {
        ($name:ident, $root:expr, $gi:expr, $path:expr) => {
            ignored!($name, $root, $gi, $path, false);
        };
        ($name:ident, $root:expr, $gi:expr, $path:expr, $is_dir:expr) => {
            #[test]
            fn $name() {
                let gi = gi_from_str($root, $gi);
                assert!(gi.matched($path, $is_dir).is_ignore());
            }
        };
    }

    macro_rules! not_ignored {
        ($name:ident, $root:expr, $gi:expr, $path:expr) => {
            not_ignored!($name, $root, $gi, $path, false);
        };
        ($name:ident, $root:expr, $gi:expr, $path:expr, $is_dir:expr) => {
            #[test]
            fn $name() {
                let gi = gi_from_str($root, $gi);
                assert!(!gi.matched($path, $is_dir).is_ignore());
            }
        };
    }

    const ROOT: &'static str = "/home/foobar/rust/rg";

    ignored!(ig1, ROOT, "months", "months");
    ignored!(ig2, ROOT, "*.lock", "Cargo.lock");
    ignored!(ig3, ROOT, "*.rs", "src/main.rs");
    ignored!(ig4, ROOT, "src/*.rs", "src/main.rs");
    ignored!(ig5, ROOT, "/*.c", "cat-file.c");
    ignored!(ig6, ROOT, "/src/*.rs", "src/main.rs");
    ignored!(ig7, ROOT, "!src/main.rs\n*.rs", "src/main.rs");
    ignored!(ig8, ROOT, "foo/", "foo", true);
    ignored!(ig9, ROOT, "**/foo", "foo");
    ignored!(ig10, ROOT, "**/foo", "src/foo");
    ignored!(ig11, ROOT, "**/foo/**", "src/foo/bar");
    ignored!(ig12, ROOT, "**/foo/**", "wat/src/foo/bar/baz");
    ignored!(ig13, ROOT, "**/foo/bar", "foo/bar");
    ignored!(ig14, ROOT, "**/foo/bar", "src/foo/bar");
    ignored!(ig15, ROOT, "abc/**", "abc/x");
    ignored!(ig16, ROOT, "abc/**", "abc/x/y");
    ignored!(ig17, ROOT, "abc/**", "abc/x/y/z");
    ignored!(ig18, ROOT, "a/**/b", "a/b");
    ignored!(ig19, ROOT, "a/**/b", "a/x/b");
    ignored!(ig20, ROOT, "a/**/b", "a/x/y/b");
    ignored!(ig21, ROOT, r"\!xy", "!xy");
    ignored!(ig22, ROOT, r"\#foo", "#foo");
    ignored!(ig23, ROOT, "foo", "./foo");
    ignored!(ig24, ROOT, "target", "grep/target");
    ignored!(ig25, ROOT, "Cargo.lock", "./tabwriter-bin/Cargo.lock");
    ignored!(ig26, ROOT, "/foo/bar/baz", "./foo/bar/baz");
    ignored!(ig27, ROOT, "foo/", "xyz/foo", true);
    ignored!(ig28, "./src", "/llvm/", "./src/llvm", true);
    ignored!(ig29, ROOT, "node_modules/ ", "node_modules", true);
    ignored!(ig30, ROOT, "**/", "foo/bar", true);
    ignored!(ig31, ROOT, "path1/*", "path1/foo");
    ignored!(ig32, ROOT, ".a/b", ".a/b");
    ignored!(ig33, "./", ".a/b", ".a/b");
    ignored!(ig34, ".", ".a/b", ".a/b");
    ignored!(ig35, "./.", ".a/b", ".a/b");
    ignored!(ig36, "././", ".a/b", ".a/b");
    ignored!(ig37, "././.", ".a/b", ".a/b");
    ignored!(ig38, ROOT, "\\[", "[");
    ignored!(ig39, ROOT, "\\?", "?");
    ignored!(ig40, ROOT, "\\*", "*");
    ignored!(ig41, ROOT, "\\a", "a");
    ignored!(ig42, ROOT, "s*.rs", "sfoo.rs");
    ignored!(ig43, ROOT, "**", "foo.rs");
    ignored!(ig44, ROOT, "**/**/*", "a/foo.rs");

    not_ignored!(ignot1, ROOT, "amonths", "months");
    not_ignored!(ignot2, ROOT, "monthsa", "months");
    not_ignored!(ignot3, ROOT, "/src/*.rs", "src/grep/src/main.rs");
    not_ignored!(ignot4, ROOT, "/*.c", "mozilla-sha1/sha1.c");
    not_ignored!(ignot5, ROOT, "/src/*.rs", "src/grep/src/main.rs");
    not_ignored!(ignot6, ROOT, "*.rs\n!src/main.rs", "src/main.rs");
    not_ignored!(ignot7, ROOT, "foo/", "foo", false);
    not_ignored!(ignot8, ROOT, "**/foo/**", "wat/src/afoo/bar/baz");
    not_ignored!(ignot9, ROOT, "**/foo/**", "wat/src/fooa/bar/baz");
    not_ignored!(ignot10, ROOT, "**/foo/bar", "foo/src/bar");
    not_ignored!(ignot11, ROOT, "#foo", "#foo");
    not_ignored!(ignot12, ROOT, "\n\n\n", "foo");
    not_ignored!(ignot13, ROOT, "foo/**", "foo", true);
    not_ignored!(
        ignot14,
        "./third_party/protobuf",
        "m4/ltoptions.m4",
        "./third_party/protobuf/csharp/src/packages/repositories.config"
    );
    not_ignored!(ignot15, ROOT, "!/bar", "foo/bar");
    not_ignored!(ignot16, ROOT, "*\n!**/", "foo", true);
    not_ignored!(ignot17, ROOT, "src/*.rs", "src/grep/src/main.rs");
    not_ignored!(ignot18, ROOT, "path1/*", "path2/path1/foo");
    not_ignored!(ignot19, ROOT, "s*.rs", "src/foo.rs");

    fn bytes(s: &str) -> Vec<u8> {
        s.to_string().into_bytes()
    }

    fn path_string<P: AsRef<Path>>(path: P) -> String {
        path.as_ref().to_str().unwrap().to_string()
    }

    #[test]
    fn parse_excludes_file1() {
        let data = bytes("[core]\nexcludesFile = /foo/bar");
        let got = super::parse_excludes_file(&data).unwrap();
        assert_eq!(path_string(got), "/foo/bar");
    }

    #[test]
    fn parse_excludes_file2() {
        let data = bytes("[core]\nexcludesFile = ~/foo/bar");
        let got = super::parse_excludes_file(&data).unwrap();
        assert_eq!(path_string(got), super::expand_tilde("~/foo/bar"));
    }

    #[test]
    fn parse_excludes_file3() {
        let data = bytes("[core]\nexcludeFile = /foo/bar");
        assert!(super::parse_excludes_file(&data).is_none());
    }

    // See: https://github.com/BurntSushi/ripgrep/issues/106
    #[test]
    fn regression_106() {
        gi_from_str("/", " ");
    }

    #[test]
    fn case_insensitive() {
        let gi = GitignoreBuilder::new(ROOT)
            .case_insensitive(true)
            .unwrap()
            .add_str(None, "*.html")
            .unwrap()
            .build()
            .unwrap();
        assert!(gi.matched("foo.html", false).is_ignore());
        assert!(gi.matched("foo.HTML", false).is_ignore());
        assert!(!gi.matched("foo.htm", false).is_ignore());
        assert!(!gi.matched("foo.HTM", false).is_ignore());
    }

    ignored!(cs1, ROOT, "*.html", "foo.html");
    not_ignored!(cs2, ROOT, "*.html", "foo.HTML");
    not_ignored!(cs3, ROOT, "*.html", "foo.htm");
    not_ignored!(cs4, ROOT, "*.html", "foo.HTM");
}
