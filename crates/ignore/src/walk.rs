use std::cmp;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, FileType, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::vec;

use same_file::Handle;
use walkdir::{self, WalkDir};

use crate::dir::{Ignore, IgnoreBuilder};
use crate::gitignore::GitignoreBuilder;
use crate::overrides::Override;
use crate::types::Types;
use crate::{Error, PartialErrorBuilder};

/// 带有可能附加错误的目录条目。
///
/// 错误通常指的是在特定目录中解析忽略文件时出现的问题。
#[derive(Clone, Debug)]
pub struct DirEntry {
    dent: DirEntryInner,
    err: Option<Error>,
}

impl DirEntry {
    /// 此条目表示的完整路径。
    pub fn path(&self) -> &Path {
        self.dent.path()
    }

    /// 此条目表示的完整路径。
    /// 与 [`path`] 类似，但会移动路径的所有权。
    ///
    /// [`path`]: struct.DirEntry.html#method.path
    pub fn into_path(self) -> PathBuf {
        self.dent.into_path()
    }

    /// 此条目是否对应符号链接。
    pub fn path_is_symlink(&self) -> bool {
        self.dent.path_is_symlink()
    }

    /// 仅当此条目对应 stdin 时才返回 true。
    ///
    /// 即，条目的深度为 0，且其文件名为 `-`。
    pub fn is_stdin(&self) -> bool {
        self.dent.is_stdin()
    }

    /// 返回此条目所指向文件的元数据。
    pub fn metadata(&self) -> Result<Metadata, Error> {
        self.dent.metadata()
    }

    /// 返回此条目所指向文件的文件类型。
    ///
    /// 如果此条目对应 stdin，则没有文件类型。
    pub fn file_type(&self) -> Option<FileType> {
        self.dent.file_type()
    }

    /// 返回此条目的文件名。
    ///
    /// 如果此条目没有文件名（例如 `/`），则返回完整路径。
    pub fn file_name(&self) -> &OsStr {
        self.dent.file_name()
    }

    /// 返回此条目相对于根目录的创建深度。
    pub fn depth(&self) -> usize {
        self.dent.depth()
    }

    /// 返回底层 inode 号（如果存在）。
    ///
    /// 如果此条目没有 inode 号，则返回 `None`。
    #[cfg(unix)]
    pub fn ino(&self) -> Option<u64> {
        self.dent.ino()
    }

    /// 返回与处理此条目相关联的错误（如果存在）。
    ///
    /// 错误的一个示例是在解析忽略文件时发生的错误。与遍历目录树本身相关的错误作为产生目录条目的一部分报告，而不是作为此方法的一部分报告。
    pub fn error(&self) -> Option<&Error> {
        self.err.as_ref()
    }

    /// 仅当此条目指向目录时才返回 true。
    pub(crate) fn is_dir(&self) -> bool {
        self.dent.is_dir()
    }

    fn new_stdin() -> DirEntry {
        DirEntry { dent: DirEntryInner::Stdin, err: None }
    }

    fn new_walkdir(dent: walkdir::DirEntry, err: Option<Error>) -> DirEntry {
        DirEntry { dent: DirEntryInner::Walkdir(dent), err: err }
    }

    fn new_raw(dent: DirEntryRaw, err: Option<Error>) -> DirEntry {
        DirEntry { dent: DirEntryInner::Raw(dent), err: err }
    }
}
/// `DirEntryInner` 是 `DirEntry` 的实现。
///
/// 它特别表示目录条目的三个不同来源：
///
/// 1. 来自 `walkdir` crate。
/// 2. 表示诸如 `stdin` 之类的特殊条目。
/// 3. 来自路径。
///
/// 具体来说，（3）必须从根本上重新创建 `DirEntry` 来自 `WalkDir` 的实现。
#[derive(Clone, Debug)]
enum DirEntryInner {
    Stdin,
    Walkdir(walkdir::DirEntry),
    Raw(DirEntryRaw),
}

impl DirEntryInner {
    /// 返回条目的路径。
    fn path(&self) -> &Path {
        use self::DirEntryInner::*;
        match *self {
            Stdin => Path::new("<stdin>"),
            Walkdir(ref x) => x.path(),
            Raw(ref x) => x.path(),
        }
    }

    /// 将条目转换为路径。
    fn into_path(self) -> PathBuf {
        use self::DirEntryInner::*;
        match self {
            Stdin => PathBuf::from("<stdin>"),
            Walkdir(x) => x.into_path(),
            Raw(x) => x.into_path(),
        }
    }

    /// 检查条目的路径是否为符号链接。
    fn path_is_symlink(&self) -> bool {
        use self::DirEntryInner::*;
        match *self {
            Stdin => false,
            Walkdir(ref x) => x.path_is_symlink(),
            Raw(ref x) => x.path_is_symlink(),
        }
    }

    /// 检查条目是否为 `stdin`。
    fn is_stdin(&self) -> bool {
        match *self {
            DirEntryInner::Stdin => true,
            _ => false,
        }
    }

    /// 获取条目的元数据。
    fn metadata(&self) -> Result<Metadata, Error> {
        use self::DirEntryInner::*;
        match *self {
            Stdin => {
                let err = Error::Io(io::Error::new(
                    io::ErrorKind::Other,
                    "<stdin> 没有元数据",
                ));
                Err(err.with_path("<stdin>"))
            }
            Walkdir(ref x) => x.metadata().map_err(|err| {
                Error::Io(io::Error::from(err)).with_path(x.path())
            }),
            Raw(ref x) => x.metadata(),
        }
    }

    /// 获取条目的文件类型。
    fn file_type(&self) -> Option<FileType> {
        use self::DirEntryInner::*;
        match *self {
            Stdin => None,
            Walkdir(ref x) => Some(x.file_type()),
            Raw(ref x) => Some(x.file_type()),
        }
    }

    /// 获取条目的文件名。
    fn file_name(&self) -> &OsStr {
        use self::DirEntryInner::*;
        match *self {
            Stdin => OsStr::new("<stdin>"),
            Walkdir(ref x) => x.file_name(),
            Raw(ref x) => x.file_name(),
        }
    }

    /// 获取条目的深度。
    fn depth(&self) -> usize {
        use self::DirEntryInner::*;
        match *self {
            Stdin => 0,
            Walkdir(ref x) => x.depth(),
            Raw(ref x) => x.depth(),
        }
    }

    #[cfg(unix)]
    /// 获取 inode 编号。
    fn ino(&self) -> Option<u64> {
        use self::DirEntryInner::*;
        use walkdir::DirEntryExt;
        match *self {
            Stdin => None,
            Walkdir(ref x) => Some(x.ino()),
            Raw(ref x) => Some(x.ino()),
        }
    }

    /// 返回 true 当且仅当此条目指向一个目录。
    fn is_dir(&self) -> bool {
        self.file_type().map(|ft| ft.is_dir()).unwrap_or(false)
    }
}
/// `DirEntryRaw` 从 `walkdir` crate 复制而来，以便我们可以在并行迭代器中从头构建 `DirEntry`。
#[derive(Clone)]
struct DirEntryRaw {
    /// 路径，由 `fs::ReadDir` 迭代器报告（即使它是符号链接）。
    path: PathBuf,
    /// 文件类型。对于递归迭代是必要的，因此将其存储起来。
    ty: FileType,
    /// 当该条目是从符号链接创建的且用户希望迭代器跟随符号链接时设置。
    follow_link: bool,
    /// 生成此条目的相对于根的深度。
    depth: usize,
    /// 基础 inode 编号（仅适用于 Unix）。
    #[cfg(unix)]
    ino: u64,
    /// 基础元数据（仅适用于 Windows）。我们在 Windows 上存储这个，因为在读取目录时这是免费的。
    #[cfg(windows)]
    metadata: fs::Metadata,
}

impl fmt::Debug for DirEntryRaw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 不包括 FileType，因为在 Rust 1.9 中它没有 Debug 实现。
        // 如果我们真的想要的话，可以通过手动查询每个可能的文件类型来添加它。---AG
        f.debug_struct("DirEntryRaw")
            .field("path", &self.path)
            .field("follow_link", &self.follow_link)
            .field("depth", &self.depth)
            .finish()
    }
}

impl DirEntryRaw {
    /// 获取路径。
    fn path(&self) -> &Path {
        &self.path
    }

    /// 将条目转换为路径。
    fn into_path(self) -> PathBuf {
        self.path
    }

    /// 检查路径是否为符号链接。
    fn path_is_symlink(&self) -> bool {
        self.ty.is_symlink() || self.follow_link
    }

    /// 获取元数据。
    fn metadata(&self) -> Result<Metadata, Error> {
        self.metadata_internal()
    }

    #[cfg(windows)]
    fn metadata_internal(&self) -> Result<fs::Metadata, Error> {
        if self.follow_link {
            fs::metadata(&self.path)
        } else {
            Ok(self.metadata.clone())
        }
        .map_err(|err| Error::Io(io::Error::from(err)).with_path(&self.path))
    }

    #[cfg(not(windows))]
    fn metadata_internal(&self) -> Result<fs::Metadata, Error> {
        if self.follow_link {
            fs::metadata(&self.path)
        } else {
            fs::symlink_metadata(&self.path)
        }
        .map_err(|err| Error::Io(io::Error::from(err)).with_path(&self.path))
    }

    /// 获取文件类型。
    fn file_type(&self) -> FileType {
        self.ty
    }

    /// 获取文件名。
    fn file_name(&self) -> &OsStr {
        self.path.file_name().unwrap_or_else(|| self.path.as_os_str())
    }

    /// 获取深度。
    fn depth(&self) -> usize {
        self.depth
    }

    #[cfg(unix)]
    fn ino(&self) -> u64 {
        self.ino
    }

    fn from_entry(
        depth: usize,
        ent: &fs::DirEntry,
    ) -> Result<DirEntryRaw, Error> {
        let ty = ent.file_type().map_err(|err| {
            let err = Error::Io(io::Error::from(err)).with_path(ent.path());
            Error::WithDepth { depth: depth, err: Box::new(err) }
        })?;
        DirEntryRaw::from_entry_os(depth, ent, ty)
    }

    #[cfg(windows)]
    fn from_entry_os(
        depth: usize,
        ent: &fs::DirEntry,
        ty: fs::FileType,
    ) -> Result<DirEntryRaw, Error> {
        let md = ent.metadata().map_err(|err| {
            let err = Error::Io(io::Error::from(err)).with_path(ent.path());
            Error::WithDepth { depth: depth, err: Box::new(err) }
        })?;
        Ok(DirEntryRaw {
            path: ent.path(),
            ty: ty,
            follow_link: false,
            depth: depth,
            metadata: md,
        })
    }

    #[cfg(unix)]
    fn from_entry_os(
        depth: usize,
        ent: &fs::DirEntry,
        ty: fs::FileType,
    ) -> Result<DirEntryRaw, Error> {
        use std::os::unix::fs::DirEntryExt;

        Ok(DirEntryRaw {
            path: ent.path(),
            ty: ty,
            follow_link: false,
            depth: depth,
            ino: ent.ino(),
        })
    }

    // 占位实现，允许在非标准平台（例如 wasm32）上编译。
    #[cfg(not(any(windows, unix)))]
    fn from_entry_os(
        depth: usize,
        ent: &fs::DirEntry,
        ty: fs::FileType,
    ) -> Result<DirEntryRaw, Error> {
        Err(Error::Io(io::Error::new(io::ErrorKind::Other, "不支持的平台")))
    }

    #[cfg(windows)]
    fn from_path(
        depth: usize,
        pb: PathBuf,
        link: bool,
    ) -> Result<DirEntryRaw, Error> {
        let md =
            fs::metadata(&pb).map_err(|err| Error::Io(err).with_path(&pb))?;
        Ok(DirEntryRaw {
            path: pb,
            ty: md.file_type(),
            follow_link: link,
            depth: depth,
            metadata: md,
        })
    }

    #[cfg(unix)]
    fn from_path(
        depth: usize,
        pb: PathBuf,
        link: bool,
    ) -> Result<DirEntryRaw, Error> {
        use std::os::unix::fs::MetadataExt;

        let md =
            fs::metadata(&pb).map_err(|err| Error::Io(err).with_path(&pb))?;
        Ok(DirEntryRaw {
            path: pb,
            ty: md.file_type(),
            follow_link: link,
            depth: depth,
            ino: md.ino(),
        })
    }

    // 占位实现，允许在非标准平台（例如 wasm32）上编译。
    #[cfg(not(any(windows, unix)))]
    fn from_path(
        depth: usize,
        pb: PathBuf,
        link: bool,
    ) -> Result<DirEntryRaw, Error> {
        Err(Error::Io(io::Error::new(io::ErrorKind::Other, "不支持的平台")))
    }
}

/// `WalkBuilder` 构建递归目录迭代器。
///
/// 该构建器支持大量可配置的选项。这包括特定的 glob 覆盖、文件类型匹配、切换是否忽略隐藏文件，当然还包括支持遵循 gitignore 文件。
///
/// 默认情况下，会尊重找到的所有忽略文件。这包括 `.ignore`、`.gitignore`、`.git/info/exclude` 以及通常位于 `$XDG_CONFIG_HOME/git/ignore` 的全局 gitignore glob。
///
/// 该构建器还支持一些标准的递归目录选项，比如限制递归深度或是否遵循符号链接（默认情况下禁用）。
///
/// # 忽略规则
///
/// 有许多规则会影响迭代器是否跳过特定的文件或目录。这些规则在这里进行了文档化。注意这些规则假设默认配置。
///
/// * 首先，会检查 glob 覆盖。如果路径与 glob 覆盖匹配，匹配会停止。然后，只有在匹配路径的 glob 是忽略 glob 时，该路径才会被跳过。（覆盖 glob 是白名单 glob，除非它以 `!` 开头，这种情况下它是忽略 glob。）
///
/// * 其次，会检查忽略文件。忽略文件目前仅来自于 git 忽略文件（`.gitignore`、`.git/info/exclude` 和配置的全局 gitignore 文件）、纯粹的 `.ignore` 文件（与 gitignore 文件具有相同的格式）或显式添加的忽略文件。优先级顺序是：`.ignore`、`.gitignore`、`.git/info/exclude`、全局 gitignore，最后是显式添加的忽略文件。请注意，不同类型的忽略文件之间的优先级不受目录层次结构的影响；任何 `.ignore` 文件都会覆盖所有 `.gitignore` 文件。在每个优先级级别内，嵌套更深的忽略文件的优先级高于嵌套较浅的忽略文件。
///
/// * 第三，如果前面的步骤产生了忽略匹配，那么所有匹配都会停止，路径会被跳过。如果它产生了白名单匹配，那么匹配将继续。白名单匹配可以被后面的匹配器覆盖。
///
/// * 第四，除非路径是目录，否则会在路径上运行文件类型匹配器。与上述相同，如果它产生了忽略匹配，那么所有匹配都会停止，路径会被跳过。如果它产生了白名单匹配，那么匹配将继续。
///
/// * 第五，如果路径未被列入白名单，并且它是隐藏的，则路径会被跳过。
///
/// * 第六，除非路径是目录，否则会将文件的大小与最大文件大小限制进行比较。如果超过了限制，就会跳过它。
///
/// * 第七，如果路径到了这一步，那么它会在迭代器中生成。
#[derive(Clone)]
pub struct WalkBuilder {
    paths: Vec<PathBuf>,
    ig_builder: IgnoreBuilder,
    max_depth: Option<usize>,
    max_filesize: Option<u64>,
    follow_links: bool,
    same_file_system: bool,
    sorter: Option<Sorter>,
    threads: usize,
    skip: Option<Arc<Handle>>,
    filter: Option<Filter>,
}

#[derive(Clone)]
enum Sorter {
    ByName(
        Arc<dyn Fn(&OsStr, &OsStr) -> cmp::Ordering + Send + Sync + 'static>,
    ),
    ByPath(Arc<dyn Fn(&Path, &Path) -> cmp::Ordering + Send + Sync + 'static>),
}

#[derive(Clone)]
struct Filter(Arc<dyn Fn(&DirEntry) -> bool + Send + Sync + 'static>);

impl fmt::Debug for WalkBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WalkBuilder")
            .field("paths", &self.paths)
            .field("ig_builder", &self.ig_builder)
            .field("max_depth", &self.max_depth)
            .field("max_filesize", &self.max_filesize)
            .field("follow_links", &self.follow_links)
            .field("threads", &self.threads)
            .field("skip", &self.skip)
            .finish()
    }
}

impl WalkBuilder {
    /// 为给定的目录创建一个递归目录迭代器的新建器。
    ///
    /// 注意，如果要遍历多个不同的目录，最好在该建造者上调用 `add` 而不是创建多个 `Walk` 值。
    pub fn new<P: AsRef<Path>>(path: P) -> WalkBuilder {
        WalkBuilder {
            paths: vec![path.as_ref().to_path_buf()],
            ig_builder: IgnoreBuilder::new(),
            max_depth: None,
            max_filesize: None,
            follow_links: false,
            same_file_system: false,
            sorter: None,
            threads: 0,
            skip: None,
            filter: None,
        }
    }

    /// 构建一个新的 `Walk` 迭代器。
    pub fn build(&self) -> Walk {
        let follow_links = self.follow_links;
        let max_depth = self.max_depth;
        let sorter = self.sorter.clone();
        let its = self
            .paths
            .iter()
            .map(move |p| {
                if p == Path::new("-") {
                    (p.to_path_buf(), None)
                } else {
                    let mut wd = WalkDir::new(p);
                    wd = wd.follow_links(follow_links || p.is_file());
                    wd = wd.same_file_system(self.same_file_system);
                    if let Some(max_depth) = max_depth {
                        wd = wd.max_depth(max_depth);
                    }
                    if let Some(ref sorter) = sorter {
                        match sorter.clone() {
                            Sorter::ByName(cmp) => {
                                wd = wd.sort_by(move |a, b| {
                                    cmp(a.file_name(), b.file_name())
                                });
                            }
                            Sorter::ByPath(cmp) => {
                                wd = wd.sort_by(move |a, b| {
                                    cmp(a.path(), b.path())
                                });
                            }
                        }
                    }
                    (p.to_path_buf(), Some(WalkEventIter::from(wd)))
                }
            })
            .collect::<Vec<_>>()
            .into_iter();
        let ig_root = self.ig_builder.build();
        Walk {
            its: its,
            it: None,
            ig_root: ig_root.clone(),
            ig: ig_root.clone(),
            max_filesize: self.max_filesize,
            skip: self.skip.clone(),
            filter: self.filter.clone(),
        }
    }

    /// 构建一个新的 `WalkParallel` 迭代器。
    ///
    /// 请注意，这个函数不会返回实现 `Iterator` 的东西。
    /// 相反，返回的值必须与闭包一起运行，例如：
    /// `builder.build_parallel().run(|| |path| println!("{:?}", path))`。
    pub fn build_parallel(&self) -> WalkParallel {
        WalkParallel {
            paths: self.paths.clone().into_iter(),
            ig_root: self.ig_builder.build(),
            max_depth: self.max_depth,
            max_filesize: self.max_filesize,
            follow_links: self.follow_links,
            same_file_system: self.same_file_system,
            threads: self.threads,
            skip: self.skip.clone(),
            filter: self.filter.clone(),
        }
    }
    /// 向迭代器中添加文件路径。
    ///
    /// 添加额外的文件路径将进行递归遍历。这应该优先于构建多个 `Walk` 迭代器，因为这样可以在迭代过程中重用资源。
    pub fn add<P: AsRef<Path>>(&mut self, path: P) -> &mut WalkBuilder {
        self.paths.push(path.as_ref().to_path_buf());
        self
    }

    /// 递归的最大深度。
    ///
    /// 默认值为 `None`，表示没有深度限制。
    pub fn max_depth(&mut self, depth: Option<usize>) -> &mut WalkBuilder {
        self.max_depth = depth;
        self
    }

    /// 是否跟踪符号链接。
    pub fn follow_links(&mut self, yes: bool) -> &mut WalkBuilder {
        self.follow_links = yes;
        self
    }

    /// 是否忽略大小超过指定限制的文件。
    pub fn max_filesize(&mut self, filesize: Option<u64>) -> &mut WalkBuilder {
        self.max_filesize = filesize;
        self
    }

    /// 用于遍历的线程数。
    ///
    /// 请注意，仅在使用 `build_parallel` 时才会产生影响。
    ///
    /// 默认设置为 `0`，会自动使用启发式算法选择线程数。
    pub fn threads(&mut self, n: usize) -> &mut WalkBuilder {
        self.threads = n;
        self
    }

    /// 向匹配器添加全局忽略文件。
    ///
    /// 这比所有其他忽略规则的优先级都低。
    ///
    /// 如果添加忽略文件时出现问题，则会返回一个错误。请注意，错误可能会指示*部分*失败。例如，如果一个忽略文件包含无效的通配符，仍然会应用所有其他通配符。
    pub fn add_ignore<P: AsRef<Path>>(&mut self, path: P) -> Option<Error> {
        let mut builder = GitignoreBuilder::new("");
        let mut errs = PartialErrorBuilder::default();
        errs.maybe_push(builder.add(path));
        match builder.build() {
            Ok(gi) => {
                self.ig_builder.add_ignore(gi);
            }
            Err(err) => {
                errs.push(err);
            }
        }
        errs.into_error_option()
    }

    /// 添加自定义的忽略文件名
    ///
    /// 这些忽略文件的优先级高于所有其他忽略文件。
    ///
    /// 当指定多个名称时，较早的名称的优先级低于较后的名称。
    pub fn add_custom_ignore_filename<S: AsRef<OsStr>>(
        &mut self,
        file_name: S,
    ) -> &mut WalkBuilder {
        self.ig_builder.add_custom_ignore_filename(file_name);
        self
    }

    /// 添加一个覆盖匹配器。
    ///
    /// 默认情况下，不使用任何覆盖匹配器。
    ///
    /// 这会覆盖任何先前的设置。
    pub fn overrides(&mut self, overrides: Override) -> &mut WalkBuilder {
        self.ig_builder.overrides(overrides);
        self
    }

    /// 添加一个文件类型匹配器。
    ///
    /// 默认情况下，不使用任何文件类型匹配器。
    ///
    /// 这会覆盖任何先前的设置。
    pub fn types(&mut self, types: Types) -> &mut WalkBuilder {
        self.ig_builder.types(types);
        self
    }

    /// 启用所有标准的忽略过滤器。
    ///
    /// 这会一组一组地切换所有默认情况下启用的过滤器：
    ///
    /// - [hidden()](#method.hidden)
    /// - [parents()](#method.parents)
    /// - [ignore()](#method.ignore)
    /// - [git_ignore()](#method.git_ignore)
    /// - [git_global()](#method.git_global)
    /// - [git_exclude()](#method.git_exclude)
    ///
    /// 调用此函数后，仍然可以单独切换每个过滤器。
    ///
    /// 默认情况下已启用（根据定义）。
    pub fn standard_filters(&mut self, yes: bool) -> &mut WalkBuilder {
        self.hidden(yes)
            .parents(yes)
            .ignore(yes)
            .git_ignore(yes)
            .git_global(yes)
            .git_exclude(yes)
    }
    /// 启用对隐藏文件的忽略。
    ///
    /// 默认情况下，此选项已启用。
    pub fn hidden(&mut self, yes: bool) -> &mut WalkBuilder {
        self.ig_builder.hidden(yes);
        self
    }

    /// 启用从父目录中读取忽略文件的功能。
    ///
    /// 如果启用此选项，则会尊重每个给定文件路径的父目录中的 `.gitignore` 文件。否则，它们将被忽略。
    ///
    /// 默认情况下，此选项已启用。
    pub fn parents(&mut self, yes: bool) -> &mut WalkBuilder {
        self.ig_builder.parents(yes);
        self
    }

    /// 启用读取 `.ignore` 文件的功能。
    ///
    /// `.ignore` 文件的语义与 `gitignore` 文件相同，并且受到诸如 ripgrep 和 The Silver Searcher 等搜索工具的支持。
    ///
    /// 默认情况下，此选项已启用。
    pub fn ignore(&mut self, yes: bool) -> &mut WalkBuilder {
        self.ig_builder.ignore(yes);
        self
    }

    /// 启用读取全局 gitignore 文件的功能，其路径在 git 的 `core.excludesFile` 配置选项中指定。
    ///
    /// git 的配置文件位置为 `$HOME/.gitconfig`。如果 `$HOME/.gitconfig` 不存在或未指定 `core.excludesFile`，则会读取 `$XDG_CONFIG_HOME/git/ignore`。如果未设置 `$XDG_CONFIG_HOME` 或为空，则会使用 `$HOME/.config/git/ignore`。
    ///
    /// 默认情况下，此选项已启用。
    pub fn git_global(&mut self, yes: bool) -> &mut WalkBuilder {
        self.ig_builder.git_global(yes);
        self
    }

    /// 启用读取 `.gitignore` 文件的功能。
    ///
    /// `.gitignore` 文件的匹配语义如 `gitignore` 手册中所述。
    ///
    /// 默认情况下，此选项已启用。
    pub fn git_ignore(&mut self, yes: bool) -> &mut WalkBuilder {
        self.ig_builder.git_ignore(yes);
        self
    }

    /// 启用读取 `.git/info/exclude` 文件的功能。
    ///
    /// `.git/info/exclude` 文件的匹配语义如 `gitignore` 手册中所述。
    ///
    /// 默认情况下，此选项已启用。
    pub fn git_exclude(&mut self, yes: bool) -> &mut WalkBuilder {
        self.ig_builder.git_exclude(yes);
        self
    }

    /// 是否需要 git 仓库来应用与 git 相关的忽略规则（全局规则、.gitignore 和本地排除规则）。
    ///
    /// 当禁用时，即使在 git 仓库之外进行搜索，也会应用与 git 相关的忽略规则。
    pub fn require_git(&mut self, yes: bool) -> &mut WalkBuilder {
        self.ig_builder.require_git(yes);
        self
    }

    /// 处理忽略文件时是否不区分大小写。
    ///
    /// 默认情况下，此选项已禁用。
    pub fn ignore_case_insensitive(&mut self, yes: bool) -> &mut WalkBuilder {
        self.ig_builder.ignore_case_insensitive(yes);
        self
    }

    /// 设置一个函数，用于按照路径对目录条目进行排序。
    ///
    /// 如果设置了比较函数，生成的迭代器将按照排序顺序返回所有路径。比较函数将用于比较来自同一目录的条目。
    ///
    /// 这类似于 `sort_by_file_name`，但比较器接受 `&Path` 而不是基本文件名，允许按照更多标准进行排序。
    ///
    /// 此方法将覆盖此方法或 `sort_by_file_name` 设置的任何先前排序器。
    ///
    /// 请注意，这不会在并行迭代器中使用。
    pub fn sort_by_file_path<F>(&mut self, cmp: F) -> &mut WalkBuilder
    where
        F: Fn(&Path, &Path) -> cmp::Ordering + Send + Sync + 'static,
    {
        self.sorter = Some(Sorter::ByPath(Arc::new(cmp)));
        self
    }

    /// 设置一个按文件名对目录条目进行排序的函数。
    ///
    /// 如果设置了比较函数，生成的迭代器将按照排序顺序返回所有路径。比较函数将用于仅使用条目的名称比较来自同一目录的条目。
    ///
    /// 此方法将覆盖此方法或 `sort_by_file_path` 设置的任何先前排序器。
    ///
    /// 请注意，这不会在并行迭代器中使用。
    pub fn sort_by_file_name<F>(&mut self, cmp: F) -> &mut WalkBuilder
    where
        F: Fn(&OsStr, &OsStr) -> cmp::Ordering + Send + Sync + 'static,
    {
        self.sorter = Some(Sorter::ByName(Arc::new(cmp)));
        self
    }

    /// 不要跨越文件系统边界。
    ///
    /// 启用此选项时，目录遍历将不会进入与根路径不同文件系统的目录。
    ///
    /// 目前，此选项仅在 Unix 和 Windows 上受支持。如果在不受支持的平台上使用此选项，目录遍历将立即返回错误，不会产生任何条目。
    pub fn same_file_system(&mut self, yes: bool) -> &mut WalkBuilder {
        self.same_file_system = yes;
        self
    }

    /// 不要生成据信对应于标准输出的目录条目。
    ///
    /// 当通过 shell 重定向调用命令到同时正在读取的文件时，这很有用。例如，`grep -r foo ./ > results` 可能会尝试搜索 `results`，即使它也在将内容写入其中，这可能会导致无限反馈循环。设置此选项可通过跳过 `results` 文件来防止发生这种情况。
    ///
    /// 默认情况下，此选项已禁用。
    pub fn skip_stdout(&mut self, yes: bool) -> &mut WalkBuilder {
        if yes {
            self.skip = stdout_handle().map(Arc::new);
        } else {
            self.skip = None;
        }
        self
    }

    /// 仅生成满足给定谓词的条目，并跳过不满足给定谓词的目录。
    ///
    /// 谓词应用于所有条目。如果谓词为真，则迭代会像正常一样继续进行。如果谓词为假，则将忽略条目，如果它是一个目录，则不会进入其中。
    ///
    /// 请注意，仍将生成可能不满足谓词的条目的错误。
    pub fn filter_entry<P>(&mut self, filter: P) -> &mut WalkBuilder
    where
        P: Fn(&DirEntry) -> bool + Send + Sync + 'static,
    {
        self.filter = Some(Filter(Arc::new(filter)));
        self
    }
}

/// `Walk` 是一个在一个或多个目录中递归遍历文件路径的迭代器。
///
/// 只有与规则匹配的文件和目录路径会被返回。默认情况下，将尊重类似 `.gitignore` 的忽略文件。关于精确的匹配规则和优先级，请参阅 `WalkBuilder` 的文档。
pub struct Walk {
    its: vec::IntoIter<(PathBuf, Option<WalkEventIter>)>,
    it: Option<WalkEventIter>,
    ig_root: Ignore,
    ig: Ignore,
    max_filesize: Option<u64>,
    skip: Option<Arc<Handle>>,
    filter: Option<Filter>,
}

impl Walk {
    /// 创建一个新的递归目录迭代器，用于给定的文件路径。
    ///
    /// 请注意，这使用默认设置，其中包括尊重 `.gitignore` 文件。要配置迭代器，请改用 `WalkBuilder`。
    pub fn new<P: AsRef<Path>>(path: P) -> Walk {
        WalkBuilder::new(path).build()
    }

    fn skip_entry(&self, ent: &DirEntry) -> Result<bool, Error> {
        if ent.depth() == 0 {
            return Ok(false);
        }
        // 在执行任何其他可能的昂贵操作（如 stat、文件系统操作）之前，我们确保先进行简单的跳过。这似乎是一个显而易见的优化，但在文件系统操作甚至是像 stat 这样的简单操作可能导致显著的开销时，这变得至关重要。例如，在 Windows 中，有一个专门的文件系统层，用于远程托管文件，并且在发生特定的文件系统操作时按需下载它们。使用此系统的用户，如果确保使用正确的文件类型过滤器，则仍可能会获得不必要的文件访问，导致大量下载。
        if should_skip_entry(&self.ig, ent) {
            return Ok(true);
        }
        if let Some(ref stdout) = self.skip {
            if path_equals(ent, stdout)? {
                return Ok(true);
            }
        }
        if self.max_filesize.is_some() && !ent.is_dir() {
            return Ok(skip_filesize(
                self.max_filesize.unwrap(),
                ent.path(),
                &ent.metadata().ok(),
            ));
        }
        if let Some(Filter(filter)) = &self.filter {
            if !filter(ent) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl Iterator for Walk {
    type Item = Result<DirEntry, Error>;

    #[inline(always)]
    fn next(&mut self) -> Option<Result<DirEntry, Error>> {
        loop {
            let ev = match self.it.as_mut().and_then(|it| it.next()) {
                Some(ev) => ev,
                None => {
                    match self.its.next() {
                        None => return None,
                        Some((_, None)) => {
                            return Some(Ok(DirEntry::new_stdin()));
                        }
                        Some((path, Some(it))) => {
                            self.it = Some(it);
                            if path.is_dir() {
                                let (ig, err) = self.ig_root.add_parents(path);
                                self.ig = ig;
                                if let Some(err) = err {
                                    return Some(Err(err));
                                }
                            } else {
                                self.ig = self.ig_root.clone();
                            }
                        }
                    }
                    continue;
                }
            };
            match ev {
                Err(err) => {
                    return Some(Err(Error::from_walkdir(err)));
                }
                Ok(WalkEvent::Exit) => {
                    self.ig = self.ig.parent().unwrap();
                }
                Ok(WalkEvent::Dir(ent)) => {
                    let mut ent = DirEntry::new_walkdir(ent, None);
                    let should_skip = match self.skip_entry(&ent) {
                        Err(err) => return Some(Err(err)),
                        Ok(should_skip) => should_skip,
                    };
                    if should_skip {
                        self.it.as_mut().unwrap().it.skip_current_dir();
                        // 仍需要将此推入堆栈，因为我们将为此目录获得 WalkEvent::Exit 事件。
                        // 我们不关心它是否出错。
                        let (igtmp, _) = self.ig.add_child(ent.path());
                        self.ig = igtmp;
                        continue;
                    }
                    let (igtmp, err) = self.ig.add_child(ent.path());
                    self.ig = igtmp;
                    ent.err = err;
                    return Some(Ok(ent));
                }
                Ok(WalkEvent::File(ent)) => {
                    let ent = DirEntry::new_walkdir(ent, None);
                    let should_skip = match self.skip_entry(&ent) {
                        Err(err) => return Some(Err(err)),
                        Ok(should_skip) => should_skip,
                    };
                    if should_skip {
                        continue;
                    }
                    return Some(Ok(ent));
                }
            }
        }
    }
}
/// `WalkEventIter` 将 `WalkDir` 迭代器转换为一个更准确描述目录树的迭代器。
/// 具体来说，它发出三种类型的事件之一：目录、文件或“退出”事件。 "退出" 事件表示整个目录的内容已枚举完毕。
struct WalkEventIter {
    depth: usize,
    it: walkdir::IntoIter,
    next: Option<Result<walkdir::DirEntry, walkdir::Error>>,
}

#[derive(Debug)]
enum WalkEvent {
    Dir(walkdir::DirEntry),
    File(walkdir::DirEntry),
    Exit,
}

impl From<WalkDir> for WalkEventIter {
    fn from(it: WalkDir) -> WalkEventIter {
        WalkEventIter { depth: 0, it: it.into_iter(), next: None }
    }
}

impl Iterator for WalkEventIter {
    type Item = walkdir::Result<WalkEvent>;

    #[inline(always)]
    fn next(&mut self) -> Option<walkdir::Result<WalkEvent>> {
        let dent = self.next.take().or_else(|| self.it.next());
        let depth = match dent {
            None => 0,
            Some(Ok(ref dent)) => dent.depth(),
            Some(Err(ref err)) => err.depth(),
        };
        if depth < self.depth {
            self.depth -= 1;
            self.next = dent;
            return Some(Ok(WalkEvent::Exit));
        }
        self.depth = depth;
        match dent {
            None => None,
            Some(Err(err)) => Some(Err(err)),
            Some(Ok(dent)) => {
                if walkdir_is_dir(&dent) {
                    self.depth += 1;
                    Some(Ok(WalkEvent::Dir(dent)))
                } else {
                    Some(Ok(WalkEvent::File(dent)))
                }
            }
        }
    }
}

/// `WalkState` 用于并行递归目录迭代器，指示是否应该继续正常遍历、跳过不遍历特定目录或完全退出遍历。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WalkState {
    /// 继续正常遍历。
    Continue,
    /// 如果给定的目录条目是目录，则不进入其中。在所有其他情况下，这无效。
    Skip,
    /// 尽快退出整个迭代器。
    ///
    /// 请注意，这是一种本质上是异步操作。在指示迭代器退出后，仍然有可能产生更多的条目。
    Quit,
}

impl WalkState {
    fn is_continue(&self) -> bool {
        *self == WalkState::Continue
    }

    fn is_quit(&self) -> bool {
        *self == WalkState::Quit
    }
}

/// 用于使用 [`WalkParallel::visit`](struct.WalkParallel.html#method.visit) 时构建访问者的构建器。
/// 该构建器将为 `WalkParallel` 启动的每个线程调用一次。每个构建器返回的访问者随后在每个访问的目录条目上被调用。
pub trait ParallelVisitorBuilder<'s> {
    /// 为 `WalkParallel` 创建每个线程的 `ParallelVisitor`。
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's>;
}

impl<'a, 's, P: ParallelVisitorBuilder<'s>> ParallelVisitorBuilder<'s>
    for &'a mut P
{
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        (**self).build()
    }
}

/// 接收当前线程的文件和目录。
///
/// 遍历的设置可以作为 [`ParallelVisitorBuilder::build`](trait.ParallelVisitorBuilder.html#tymethod.build) 的一部分实现。
/// 当遍历完成时，可以通过在您的遍历类型上实现 `Drop` 特性来实现拆卸。
pub trait ParallelVisitor: Send {
    /// 接收当前线程的文件和目录。这将在遍历访问的每个目录条目上调用一次。
    fn visit(&mut self, entry: Result<DirEntry, Error>) -> WalkState;
}

struct FnBuilder<F> {
    builder: F,
}

impl<'s, F: FnMut() -> FnVisitor<'s>> ParallelVisitorBuilder<'s>
    for FnBuilder<F>
{
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        let visitor = (self.builder)();
        Box::new(FnVisitorImp { visitor })
    }
}

type FnVisitor<'s> =
    Box<dyn FnMut(Result<DirEntry, Error>) -> WalkState + Send + 's>;

struct FnVisitorImp<'s> {
    visitor: FnVisitor<'s>,
}

impl<'s> ParallelVisitor for FnVisitorImp<'s> {
    fn visit(&mut self, entry: Result<DirEntry, Error>) -> WalkState {
        (self.visitor)(entry)
    }
}

/// `WalkParallel` 是一个并行递归目录迭代器，用于一个或多个目录中的文件路径。
///
/// 只有与规则匹配的文件和目录路径会被返回。默认情况下，将尊重类似 `.gitignore` 的忽略文件。关于精确的匹配规则和优先级，请参阅 `WalkBuilder` 的文档。
///
/// 与 `Walk` 不同，这使用多个线程来遍历目录。
pub struct WalkParallel {
    paths: vec::IntoIter<PathBuf>,
    ig_root: Ignore,
    max_filesize: Option<u64>,
    max_depth: Option<usize>,
    follow_links: bool,
    same_file_system: bool,
    threads: usize,
    skip: Option<Arc<Handle>>,
    filter: Option<Filter>,
}

impl WalkParallel {
    /// 执行并行递归目录迭代器。 `mkf` 将为每个用于迭代的线程调用。
    /// `mkf` 生成的函数随后将为每个访问的文件路径调用。
    pub fn run<'s, F>(self, mkf: F)
    where
        F: FnMut() -> FnVisitor<'s>,
    {
        self.visit(&mut FnBuilder { builder: mkf })
    }

    /// 使用自定义访问者执行并行递归目录迭代器。
    ///
    /// 给定的构建器用于为此遍历使用的每个线程构建一个访问者。每个构建器返回的访问者随后对每个线程看到的每个目录条目进行调用。
    ///
    /// 通常，创建自定义访问者对于在遍历结束时执行某些清理操作非常有用。可以通过为您的构建器实现 `Drop`（或者为您的访问者实现 `Drop`，如果您希望为启动的每个线程执行清理操作）来实现此目的。
    ///
    /// 例如，每个访问者可能会构建一个结果数据结构，该数据结构对应于每个线程看到的目录条目。由于每个访问者仅在一个线程上运行，因此可以在不进行同步的情况下执行此构建。然后，一旦遍历完成，所有结果都可以合并到一个数据结构中。
    pub fn visit(mut self, builder: &mut dyn ParallelVisitorBuilder<'_>) {
        let threads = self.threads();
        let stack = Arc::new(Mutex::new(vec![]));
        {
            let mut stack = stack.lock().unwrap();
            let mut visitor = builder.build();
            let mut paths = Vec::new().into_iter();
            std::mem::swap(&mut paths, &mut self.paths);
            // 将初始的根路径集发送到工作池。请注意，我们只发送目录。对于文件，我们直接发送给回调函数。
            for path in paths {
                let (dent, root_device) = if path == Path::new("-") {
                    (DirEntry::new_stdin(), None)
                } else {
                    let root_device = if !self.same_file_system {
                        None
                    } else {
                        match device_num(&path) {
                            Ok(root_device) => Some(root_device),
                            Err(err) => {
                                let err = Error::Io(err).with_path(path);
                                if visitor.visit(Err(err)).is_quit() {
                                    return;
                                }
                                continue;
                            }
                        }
                    };
                    match DirEntryRaw::from_path(0, path, false) {
                        Ok(dent) => {
                            (DirEntry::new_raw(dent, None), root_device)
                        }
                        Err(err) => {
                            if visitor.visit(Err(err)).is_quit() {
                                return;
                            }
                            continue;
                        }
                    }
                };
                stack.push(Message::Work(Work {
                    dent: dent,
                    ignore: self.ig_root.clone(),
                    root_device: root_device,
                }));
            }
            // ... 但是如果我们不需要它们，就无需启动工作线程。
            if stack.is_empty() {
                return;
            }
        }
        // 创建工作线程，然后等待它们完成。
        let quit_now = Arc::new(AtomicBool::new(false));
        let num_pending =
            Arc::new(AtomicUsize::new(stack.lock().unwrap().len()));
        std::thread::scope(|s| {
            let mut handles = vec![];
            for _ in 0..threads {
                let worker = Worker {
                    visitor: builder.build(),
                    stack: stack.clone(),
                    quit_now: quit_now.clone(),
                    num_pending: num_pending.clone(),
                    max_depth: self.max_depth,
                    max_filesize: self.max_filesize,
                    follow_links: self.follow_links,
                    skip: self.skip.clone(),
                    filter: self.filter.clone(),
                };
                handles.push(s.spawn(|| worker.run()));
            }
            for handle in handles {
                handle.join().unwrap();
            }
        });
    }

    fn threads(&self) -> usize {
        if self.threads == 0 {
            2
        } else {
            self.threads
        }
    }
}
/// `Message` 是工作指令集，一个工作线程知道如何处理这些指令。
enum Message {
    /// 工作项对应应该进入的目录。应该跳过或忽略的条目的工作项不应产生。
    Work(Work),
    /// 此指令指示工作线程退出。
    Quit,
}

/// 每个工作线程要处理的工作单元。
///
/// 每个工作单元对应应该进入的目录。
struct Work {
    /// 目录条目。
    dent: DirEntry,
    /// 为此目录的父目录构建的任何忽略匹配器。
    ignore: Ignore,
    /// 根设备号。当存在时，只应考虑具有相同设备号的文件。
    root_device: Option<u64>,
}

impl Work {
    /// 仅当此工作项为目录时返回 `true`。
    fn is_dir(&self) -> bool {
        self.dent.is_dir()
    }

    /// 仅当此工作项为符号链接时返回 `true`。
    fn is_symlink(&self) -> bool {
        self.dent.file_type().map_or(false, |ft| ft.is_symlink())
    }

    /// 为父目录添加忽略规则。
    ///
    /// 请注意，这仅适用于深度为 0 的条目。对于所有其他条目，这是一个空操作。
    fn add_parents(&mut self) -> Option<Error> {
        if self.dent.depth() > 0 {
            return None;
        }
        // 在深度为 0 时，此条目的路径是根路径，因此我们可以直接使用它来添加父级忽略规则。
        let (ig, err) = self.ignore.add_parents(self.dent.path());
        self.ignore = ig;
        err
    }

    /// 读取此工作项的目录内容，并为此目录添加忽略规则。
    ///
    /// 如果读取目录内容时出现问题，则返回错误。如果读取此目录的忽略规则时出现问题，则将错误附加到此工作项的目录条目上。
    fn read_dir(&mut self) -> Result<fs::ReadDir, Error> {
        let readdir = match fs::read_dir(self.dent.path()) {
            Ok(readdir) => readdir,
            Err(err) => {
                let err = Error::from(err)
                    .with_path(self.dent.path())
                    .with_depth(self.dent.depth());
                return Err(err);
            }
        };
        let (ig, err) = self.ignore.add_child(self.dent.path());
        self.ignore = ig;
        self.dent.err = err;
        Ok(readdir)
    }
}

/// 工作线程负责进入目录、更新忽略匹配器、生成新工作并调用调用者的回调。
///
/// 请注意，工作线程既是生产者又是消费者。
struct Worker<'s> {
    /// 调用者的回调。
    visitor: Box<dyn ParallelVisitor + 's>,
    /// 要处理的工作堆栈。
    ///
    /// 我们使用堆栈而不是通道，因为堆栈可以使我们以深度优先顺序访问目录。这可以通过将文件路径数和 gitignore 匹配器的内存占用保持较低来显著降低内存使用峰值。
    stack: Arc<Mutex<Vec<Message>>>,
    /// 是否所有工作线程都应在下一个机会终止。请注意，我们需要这个因为我们不希望在我们退出后继续完成其他 `Work`。如果有一个优先级通道，我们就不需要这个了。
    quit_now: Arc<AtomicBool>,
    /// 未完成工作项的数量。
    num_pending: Arc<AtomicUsize>,
    /// 目录的最大深度下降。值为 `0` 表示根本不下降。
    max_depth: Option<usize>,
    /// 搜索文件的最大大小（以字节为单位）。如果文件超过此大小，将跳过它。
    max_filesize: Option<u64>,
    /// 是否要跟随符号链接。启用此选项时，将执行循环检测。
    follow_links: bool,
    /// 要跳过的文件句柄，当前为 `None` 或者是 stdout，如果请求跳过与 stdout 相同的文件。
    skip: Option<Arc<Handle>>,
    /// 适用于目录条目的谓词。如果为真，则将跳过该条目及其所有子项。
    filter: Option<Filter>,
}

impl<'s> Worker<'s> {
    /// 运行该工作线程，直到没有更多工作要做为止。
    ///
    /// 该工作线程将对所有未被忽略匹配器跳过的条目调用调用者的回调。
    fn run(mut self) {
        while let Some(work) = self.get_work() {
            if let WalkState::Quit = self.run_one(work) {
                self.quit_now();
            }
            self.work_done();
        }
    }

    fn run_one(&mut self, mut work: Work) -> WalkState {
        // 如果工作项不是目录，则可以立即执行调用者的回调并继续。
        if work.is_symlink() || !work.is_dir() {
            return self.visitor.visit(Ok(work.dent));
        }
        if let Some(err) = work.add_parents() {
            let state = self.visitor.visit(Err(err));
            if state.is_quit() {
                return state;
            }
        }

        let descend = if let Some(root_device) = work.root_device {
            match is_same_file_system(root_device, work.dent.path()) {
                Ok(true) => true,
                Ok(false) => false,
                Err(err) => {
                    let state = self.visitor.visit(Err(err));
                    if state.is_quit() {
                        return state;
                    }
                    false
                }
            }
        } else {
            true
        };

        // 尝试首先读取目录，然后再将所有权转移到提供的闭包。
        // 但不要立即解包，因为我们可能会在没有足够读取权限来列出目录的情况下收到 `Err` 值。
        // 在这种情况下，我们仍然希望在传递错误值之前向闭包提供一个有效的条目。
        let readdir = work.read_dir();
        let depth = work.dent.depth();
        let state = self.visitor.visit(Ok(work.dent));
        if !state.is_continue() {
            return state;
        }
        if !descend {
            return WalkState::Skip;
        }

        let readdir = match readdir {
            Ok(readdir) => readdir,
            Err(err) => {
                return self.visitor.visit(Err(err));
            }
        };

        if self.max_depth.map_or(false, |max| depth >= max) {
            return WalkState::Skip;
        }
        for result in readdir {
            let state = self.generate_work(
                &work.ignore,
                depth + 1,
                work.root_device,
                result,
            );
            if state.is_quit() {
                return state;
            }
        }
        WalkState::Continue
    }

    /// 决定是否将给定目录条目作为要搜索的文件提交。
    ///
    /// 如果条目是应该被忽略的路径，则这是一个空操作。
    /// 否则，将条目推送到队列中。（实际的回调执行发生在 `run_one` 中。）
    ///
    /// 如果在读取条目时发生错误，则将其发送到调用者的回调。
    ///
    /// `ig` 是父目录的 `Ignore` 匹配器。`depth` 应该是此条目的深度。`result` 应该是目录迭代器产生的项。
    fn generate_work(
        &mut self,
        ig: &Ignore,
        depth: usize,
        root_device: Option<u64>,
        result: Result<fs::DirEntry, io::Error>,
    ) -> WalkState {
        let fs_dent = match result {
            Ok(fs_dent) => fs_dent,
            Err(err) => {
                return self
                    .visitor
                    .visit(Err(Error::from(err).with_depth(depth)));
            }
        };
        let mut dent = match DirEntryRaw::from_entry(depth, &fs_dent) {
            Ok(dent) => DirEntry::new_raw(dent, None),
            Err(err) => {
                return self.visitor.visit(Err(err));
            }
        };
        let is_symlink = dent.file_type().map_or(false, |ft| ft.is_symlink());
        if self.follow_links && is_symlink {
            let path = dent.path().to_path_buf();
            dent = match DirEntryRaw::from_path(depth, path, true) {
                Ok(dent) => DirEntry::new_raw(dent, None),
                Err(err) => {
                    return self.visitor.visit(Err(err));
                }
            };
            if dent.is_dir() {
                if let Err(err) = check_symlink_loop(ig, dent.path(), depth) {
                    return self.visitor.visit(Err(err));
                }
            }
        }
        // N.B. 见单线程实现中的类似调用，了解为什么这在下面的检查之前很重要。
        if should_skip_entry(ig, &dent) {
            return WalkState::Continue;
        }
        if let Some(ref stdout) = self.skip {
            let is_stdout = match path_equals(&dent, stdout) {
                Ok(is_stdout) => is_stdout,
                Err(err) => return self.visitor.visit(Err(err)),
            };
            if is_stdout {
                return WalkState::Continue;
            }
        }
        let should_skip_filesize =
            if self.max_filesize.is_some() && !dent.is_dir() {
                skip_filesize(
                    self.max_filesize.unwrap(),
                    dent.path(),
                    &dent.metadata().ok(),
                )
            } else {
                false
            };
        let should_skip_filtered =
            if let Some(Filter(predicate)) = &self.filter {
                !predicate(&dent)
            } else {
                false
            };
        if !should_skip_filesize && !should_skip_filtered {
            self.send(Work { dent, ignore: ig.clone(), root_device });
        }
        WalkState::Continue
    }

    /// 获取下一个要进入的目录。
    ///
    /// 如果所有工作都已用尽，则返回 None。然后工作线程应随后退出。
    fn get_work(&mut self) -> Option<Work> {
        let mut value = self.recv();
        loop {
            // 模拟优先级通道：如果设置了 quit_now 标志，我们只能接收退出消息。
            if self.is_quit_now() {
                value = Some(Message::Quit)
            }
            match value {
                Some(Message::Work(work)) => {
                    return Some(work);
                }
                Some(Message::Quit) => {
                    // 重复退出消息以唤醒正在休眠的线程（如果有的话）。
                    // 骨牌效应将确保每个线程都会退出。
                    self.send_quit();
                    return None;
                }
                None => {
                    // 一旦 num_pending 达到 0，它就不可能再增加了。
                    // 也就是说，它只有在所有工作都已运行，以便没有工作生成更多工作时才会达到 0。
                    // 我们之所以有这个保证，是因为在每个作业提交之前，num_pending 总是会递增，
                    // 并且只有在每个作业完全完成后才会递减一次。
                    // 因此，如果这个值达到零，那么就不可能有其他作业在运行。
                    if self.num_pending() == 0 {
                        // 每个其他线程都会在下一个 recv() 处于阻塞状态。
                        // 发送初始退出消息并退出。
                        self.send_quit();
                        return None;
                    }
                    // 等待下一个 `Work` 或 `Quit` 消息。
                    loop {
                        if let Some(v) = self.recv() {
                            value = Some(v);
                            break;
                        }
                        // 我们的堆栈不是阻塞的。
                        // 而不是烧毁 CPU 等待，我们让线程休眠一会儿。
                        // 一般来说，这通常只会在搜索接近终止时发生。
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        }
    }

    /// 指示所有工作线程立即退出。
    fn quit_now(&self) {
        self.quit_now.store(true, Ordering::SeqCst);
    }

    /// 如果该工作线程应立即退出，则返回 true。
    fn is_quit_now(&self) -> bool {
        self.quit_now.load(Ordering::SeqCst)
    }

    /// 返回未完成的作业数量。
    fn num_pending(&self) -> usize {
        self.num_pending.load(Ordering::SeqCst)
    }

    /// 发送工作。
    fn send(&self, work: Work) {
        self.num_pending.fetch_add(1, Ordering::SeqCst);
        let mut stack = self.stack.lock().unwrap();
        stack.push(Message::Work(work));
    }

    /// 发送退出消息。
    fn send_quit(&self) {
        let mut stack = self.stack.lock().unwrap();
        stack.push(Message::Quit);
    }

    /// 接收工作。
    fn recv(&self) -> Option<Message> {
        let mut stack = self.stack.lock().unwrap();
        stack.pop()
    }

    /// 表示工作已完成。
    fn work_done(&self) {
        self.num_pending.fetch_sub(1, Ordering::SeqCst);
    }
}

fn check_symlink_loop(
    ig_parent: &Ignore,
    child_path: &Path,
    child_depth: usize,
) -> Result<(), Error> {
    let hchild = Handle::from_path(child_path).map_err(|err| {
        Error::from(err).with_path(child_path).with_depth(child_depth)
    })?;
    for ig in ig_parent.parents().take_while(|ig| !ig.is_absolute_parent()) {
        let h = Handle::from_path(ig.path()).map_err(|err| {
            Error::from(err).with_path(child_path).with_depth(child_depth)
        })?;
        if hchild == h {
            return Err(Error::Loop {
                ancestor: ig.path().to_path_buf(),
                child: child_path.to_path_buf(),
            }
            .with_depth(child_depth));
        }
    }
    Ok(())
}
// 在调用此函数之前，请确保您已确保这是必要的，因为参数意味着一个文件状态。
fn skip_filesize(
    max_filesize: u64,
    path: &Path,
    ent: &Option<Metadata>,
) -> bool {
    let filesize = match *ent {
        Some(ref md) => Some(md.len()),
        None => None,
    };

    if let Some(fs) = filesize {
        if fs > max_filesize {
            log::debug!("忽略文件：{}，大小：{} 字节", path.display(), fs);
            true
        } else {
            false
        }
    } else {
        false
    }
}

fn should_skip_entry(ig: &Ignore, dent: &DirEntry) -> bool {
    let m = ig.matched_dir_entry(dent);
    if m.is_ignore() {
        log::debug!("忽略：{}，匹配器：{:?}", dent.path().display(), m);
        true
    } else if m.is_whitelist() {
        log::debug!("加入白名单：{}，匹配器：{:?}", dent.path().display(), m);
        false
    } else {
        false
    }
}

/// 返回用于过滤搜索的 stdout 句柄。
///
/// 仅当标准输出被重定向到文件时才返回句柄。
/// 返回的句柄对应于该文件。
///
/// 这可以用于确保我们不会尝试搜索我们可能也正在写入的文件。
fn stdout_handle() -> Option<Handle> {
    let h = match Handle::stdout() {
        Err(_) => return None,
        Ok(h) => h,
    };
    let md = match h.as_file().metadata() {
        Err(_) => return None,
        Ok(md) => md,
    };
    if !md.is_file() {
        return None;
    }
    Some(h)
}

/// 当且仅当给定的目录条目被认为等同于给定句柄时，返回 true。
/// 如果在查询路径信息以确定等同性时发生问题，则返回该错误。
fn path_equals(dent: &DirEntry, handle: &Handle) -> Result<bool, Error> {
    #[cfg(unix)]
    fn never_equal(dent: &DirEntry, handle: &Handle) -> bool {
        dent.ino() != Some(handle.ino())
    }

    #[cfg(not(unix))]
    fn never_equal(_: &DirEntry, _: &Handle) -> bool {
        false
    }

    // 如果我们确定这两个事物肯定不相等，则避免昂贵的额外状态调用以确定等同性。
    if dent.is_stdin() || never_equal(dent, handle) {
        return Ok(false);
    }
    Handle::from_path(dent.path())
        .map(|h| &h == handle)
        .map_err(|err| Error::Io(err).with_path(dent.path()))
}

/// 当且仅当给定的 walkdir 条目对应于目录时，返回 true。
///
/// 这通常只是 `dent.file_type().is_dir()`，但当我们不
/// 跟随符号链接时，根目录条目可能是链接到目录的符号链接---通过用户显式指定。
/// 在这种情况下，我们需要跟随符号链接并查询它是否是目录。
/// 但我们只对根条目执行此操作，以避免在大多数情况下进行额外的状态检查。
fn walkdir_is_dir(dent: &walkdir::DirEntry) -> bool {
    if dent.file_type().is_dir() {
        return true;
    }
    if !dent.file_type().is_symlink() || dent.depth() > 0 {
        return false;
    }
    dent.path().metadata().ok().map_or(false, |md| md.file_type().is_dir())
}

/// 当且仅当给定的路径与给定的根设备相同设备时，返回 true。
fn is_same_file_system(root_device: u64, path: &Path) -> Result<bool, Error> {
    let dent_device =
        device_num(path).map_err(|err| Error::Io(err).with_path(path))?;
    Ok(root_device == dent_device)
}

#[cfg(unix)]
fn device_num<P: AsRef<Path>>(path: P) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;

    path.as_ref().metadata().map(|md| md.dev())
}

#[cfg(windows)]
fn device_num<P: AsRef<Path>>(path: P) -> io::Result<u64> {
    use winapi_util::{file, Handle};

    let h = Handle::from_path_any(path)?;
    file::information(h).map(|info| info.volume_serial_number())
}

#[cfg(not(any(unix, windows)))]
fn device_num<P: AsRef<Path>>(_: P) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Other,
        "walkdir: 在此平台上不支持 same_file_system 选项",
    ))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use super::{DirEntry, WalkBuilder, WalkState};
    use crate::tests::TempDir;

    fn wfile<P: AsRef<Path>>(path: P, contents: &str) {
        let mut file = File::create(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    fn wfile_size<P: AsRef<Path>>(path: P, size: u64) {
        let file = File::create(path).unwrap();
        file.set_len(size).unwrap();
    }

    #[cfg(unix)]
    fn symlink<P: AsRef<Path>, Q: AsRef<Path>>(src: P, dst: Q) {
        use std::os::unix::fs::symlink;
        symlink(src, dst).unwrap();
    }

    fn mkdirp<P: AsRef<Path>>(path: P) {
        fs::create_dir_all(path).unwrap();
    }

    fn normal_path(unix: &str) -> String {
        if cfg!(windows) {
            unix.replace("\\", "/")
        } else {
            unix.to_string()
        }
    }

    fn walk_collect(prefix: &Path, builder: &WalkBuilder) -> Vec<String> {
        let mut paths = vec![];
        for result in builder.build() {
            let dent = match result {
                Err(_) => continue,
                Ok(dent) => dent,
            };
            let path = dent.path().strip_prefix(prefix).unwrap();
            if path.as_os_str().is_empty() {
                continue;
            }
            paths.push(normal_path(path.to_str().unwrap()));
        }
        paths.sort();
        paths
    }

    fn walk_collect_parallel(
        prefix: &Path,
        builder: &WalkBuilder,
    ) -> Vec<String> {
        let mut paths = vec![];
        for dent in walk_collect_entries_parallel(builder) {
            let path = dent.path().strip_prefix(prefix).unwrap();
            if path.as_os_str().is_empty() {
                continue;
            }
            paths.push(normal_path(path.to_str().unwrap()));
        }
        paths.sort();
        paths
    }

    fn walk_collect_entries_parallel(builder: &WalkBuilder) -> Vec<DirEntry> {
        let dents = Arc::new(Mutex::new(vec![]));
        builder.build_parallel().run(|| {
            let dents = dents.clone();
            Box::new(move |result| {
                if let Ok(dent) = result {
                    dents.lock().unwrap().push(dent);
                }
                WalkState::Continue
            })
        });

        let dents = dents.lock().unwrap();
        dents.to_vec()
    }

    fn mkpaths(paths: &[&str]) -> Vec<String> {
        let mut paths: Vec<_> = paths.iter().map(|s| s.to_string()).collect();
        paths.sort();
        paths
    }

    fn tmpdir() -> TempDir {
        TempDir::new().unwrap()
    }

    fn assert_paths(prefix: &Path, builder: &WalkBuilder, expected: &[&str]) {
        let got = walk_collect(prefix, builder);
        assert_eq!(got, mkpaths(expected), "单线程");
        let got = walk_collect_parallel(prefix, builder);
        assert_eq!(got, mkpaths(expected), "并行");
    }

    #[test]
    fn no_ignores() {
        let td = tmpdir();
        mkdirp(td.path().join("a/b/c"));
        mkdirp(td.path().join("x/y"));
        wfile(td.path().join("a/b/foo"), "");
        wfile(td.path().join("x/y/foo"), "");

        assert_paths(
            td.path(),
            &WalkBuilder::new(td.path()),
            &["x", "x/y", "x/y/foo", "a", "a/b", "a/b/foo", "a/b/c"],
        );
    }

    #[test]
    fn custom_ignore() {
        let td = tmpdir();
        let custom_ignore = ".customignore";
        mkdirp(td.path().join("a"));
        wfile(td.path().join(custom_ignore), "foo");
        wfile(td.path().join("foo"), "");
        wfile(td.path().join("a/foo"), "");
        wfile(td.path().join("bar"), "");
        wfile(td.path().join("a/bar"), "");

        let mut builder = WalkBuilder::new(td.path());
        builder.add_custom_ignore_filename(&custom_ignore);
        assert_paths(td.path(), &builder, &["bar", "a", "a/bar"]);
    }

    #[test]
    fn custom_ignore_exclusive_use() {
        let td = tmpdir();
        let custom_ignore = ".customignore";
        mkdirp(td.path().join("a"));
        wfile(td.path().join(custom_ignore), "foo");
        wfile(td.path().join("foo"), "");
        wfile(td.path().join("a/foo"), "");
        wfile(td.path().join("bar"), "");
        wfile(td.path().join("a/bar"), "");

        let mut builder = WalkBuilder::new(td.path());
        builder.ignore(false);
        builder.git_ignore(false);
        builder.git_global(false);
        builder.git_exclude(false);
        builder.add_custom_ignore_filename(&custom_ignore);
        assert_paths(td.path(), &builder, &["bar", "a", "a/bar"]);
    }

    #[test]
    fn gitignore() {
        let td = tmpdir();
        mkdirp(td.path().join(".git"));
        mkdirp(td.path().join("a"));
        wfile(td.path().join(".gitignore"), "foo");
        wfile(td.path().join("foo"), "");
        wfile(td.path().join("a/foo"), "");
        wfile(td.path().join("bar"), "");
        wfile(td.path().join("a/bar"), "");

        assert_paths(
            td.path(),
            &WalkBuilder::new(td.path()),
            &["bar", "a", "a/bar"],
        );
    }

    #[test]
    fn explicit_ignore() {
        let td = tmpdir();
        let igpath = td.path().join(".not-an-ignore");
        mkdirp(td.path().join("a"));
        wfile(&igpath, "foo");
        wfile(td.path().join("foo"), "");
        wfile(td.path().join("a/foo"), "");
        wfile(td.path().join("bar"), "");
        wfile(td.path().join("a/bar"), "");

        let mut builder = WalkBuilder::new(td.path());
        assert!(builder.add_ignore(&igpath).is_none());
        assert_paths(td.path(), &builder, &["bar", "a", "a/bar"]);
    }

    #[test]
    fn explicit_ignore_exclusive_use() {
        let td = tmpdir();
        let igpath = td.path().join(".not-an-ignore");
        mkdirp(td.path().join("a"));
        wfile(&igpath, "foo");
        wfile(td.path().join("foo"), "");
        wfile(td.path().join("a/foo"), "");
        wfile(td.path().join("bar"), "");
        wfile(td.path().join("a/bar"), "");

        let mut builder = WalkBuilder::new(td.path());
        builder.standard_filters(false);
        assert!(builder.add_ignore(&igpath).is_none());
        assert_paths(
            td.path(),
            &builder,
            &[".not-an-ignore", "bar", "a", "a/bar"],
        );
    }

    #[test]
    fn gitignore_parent() {
        let td = tmpdir();
        mkdirp(td.path().join(".git"));
        mkdirp(td.path().join("a"));
        wfile(td.path().join(".gitignore"), "foo");
        wfile(td.path().join("a/foo"), "");
        wfile(td.path().join("a/bar"), "");

        let root = td.path().join("a");
        assert_paths(&root, &WalkBuilder::new(&root), &["bar"]);
    }

    #[test]
    fn max_depth() {
        let td = tmpdir();
        mkdirp(td.path().join("a/b/c"));
        wfile(td.path().join("foo"), "");
        wfile(td.path().join("a/foo"), "");
        wfile(td.path().join("a/b/foo"), "");
        wfile(td.path().join("a/b/c/foo"), "");

        let mut builder = WalkBuilder::new(td.path());
        assert_paths(
            td.path(),
            &builder,
            &["a", "a/b", "a/b/c", "foo", "a/foo", "a/b/foo", "a/b/c/foo"],
        );
        assert_paths(td.path(), builder.max_depth(Some(0)), &[]);
        assert_paths(td.path(), builder.max_depth(Some(1)), &["a", "foo"]);
        assert_paths(
            td.path(),
            builder.max_depth(Some(2)),
            &["a", "a/b", "foo", "a/foo"],
        );
    }

    #[test]
    fn max_filesize() {
        let td = tmpdir();
        mkdirp(td.path().join("a/b"));
        wfile_size(td.path().join("foo"), 0);
        wfile_size(td.path().join("bar"), 400);
        wfile_size(td.path().join("baz"), 600);
        wfile_size(td.path().join("a/foo"), 600);
        wfile_size(td.path().join("a/bar"), 500);
        wfile_size(td.path().join("a/baz"), 200);

        let mut builder = WalkBuilder::new(td.path());
        assert_paths(
            td.path(),
            &builder,
            &["a", "a/b", "foo", "bar", "baz", "a/foo", "a/bar", "a/baz"],
        );
        assert_paths(
            td.path(),
            builder.max_filesize(Some(0)),
            &["a", "a/b", "foo"],
        );
        assert_paths(
            td.path(),
            builder.max_filesize(Some(500)),
            &["a", "a/b", "foo", "bar", "a/bar", "a/baz"],
        );
        assert_paths(
            td.path(),
            builder.max_filesize(Some(50000)),
            &["a", "a/b", "foo", "bar", "baz", "a/foo", "a/bar", "a/baz"],
        );
    }

    #[cfg(unix)] // because symlinks on windows are weird
    #[test]
    fn symlinks() {
        let td = tmpdir();
        mkdirp(td.path().join("a/b"));
        symlink(td.path().join("a/b"), td.path().join("z"));
        wfile(td.path().join("a/b/foo"), "");

        let mut builder = WalkBuilder::new(td.path());
        assert_paths(td.path(), &builder, &["a", "a/b", "a/b/foo", "z"]);
        assert_paths(
            td.path(),
            &builder.follow_links(true),
            &["a", "a/b", "a/b/foo", "z", "z/foo"],
        );
    }
    #[cfg(unix)] // 因为 Windows 上的符号链接很奇怪
    #[test]
    fn first_path_not_symlink() {
        let td = tmpdir();
        mkdirp(td.path().join("foo"));

        let dents = WalkBuilder::new(td.path().join("foo"))
            .build()
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(1, dents.len());
        assert!(!dents[0].path_is_symlink());

        let dents = walk_collect_entries_parallel(&WalkBuilder::new(
            td.path().join("foo"),
        ));
        assert_eq!(1, dents.len());
        assert!(!dents[0].path_is_symlink());
    }

    #[cfg(unix)] // 因为 Windows 上的符号链接很奇怪
    #[test]
    fn symlink_loop() {
        let td = tmpdir();
        mkdirp(td.path().join("a/b"));
        symlink(td.path().join("a"), td.path().join("a/b/c"));

        let mut builder = WalkBuilder::new(td.path());
        assert_paths(td.path(), &builder, &["a", "a/b", "a/b/c"]);
        assert_paths(td.path(), &builder.follow_links(true), &["a", "a/b"]);
    }

    // 测试 'same_file_system' 选项有点棘手，因为我们需要一个多于一个文件系统的环境。
    // 我们采用了一个启发式方法，即 /sys 在 Linux 上通常是一个不同的卷，并采用此方法。
    #[test]
    #[cfg(target_os = "linux")]
    fn same_file_system() {
        use super::device_num;

        // 如果由于某种原因 /sys 不存在或不是目录，只需跳过此测试。
        if !Path::new("/sys").is_dir() {
            return;
        }

        // 如果我们的测试目录实际上不是与 /sys 不同的卷，那么此测试是没有意义的，我们不应该运行它。
        let td = tmpdir();
        if device_num(td.path()).unwrap() == device_num("/sys").unwrap() {
            return;
        }

        mkdirp(td.path().join("same_file"));
        symlink("/sys", td.path().join("same_file").join("alink"));

        // 创建到 sys 的符号链接并启用跟随符号链接。如果 same_file_system 选项不起作用，
        // 那么这可能会遇到权限错误。否则，它应该完全跳过符号链接。
        let mut builder = WalkBuilder::new(td.path());
        builder.follow_links(true).same_file_system(true);
        assert_paths(td.path(), &builder, &["same_file", "same_file/alink"]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn no_read_permissions() {
        let dir_path = Path::new("/root");

        // 没有 /etc/sudoers.d，跳过测试。
        if !dir_path.is_dir() {
            return;
        }
        // 我们是 root，所以测试不会检查我们想要的内容。
        if fs::read_dir(&dir_path).is_ok() {
            return;
        }

        // 检查我们是否无法下降但可以获取父目录的条目。
        let builder = WalkBuilder::new(&dir_path);
        assert_paths(dir_path.parent().unwrap(), &builder, &["root"]);
    }

    #[test]
    fn filter() {
        let td = tmpdir();
        mkdirp(td.path().join("a/b/c"));
        mkdirp(td.path().join("x/y"));
        wfile(td.path().join("a/b/foo"), "");
        wfile(td.path().join("x/y/foo"), "");

        assert_paths(
            td.path(),
            &WalkBuilder::new(td.path()),
            &["x", "x/y", "x/y/foo", "a", "a/b", "a/b/foo", "a/b/c"],
        );

        assert_paths(
            td.path(),
            &WalkBuilder::new(td.path())
                .filter_entry(|entry| entry.file_name() != OsStr::new("a")),
            &["x", "x/y", "x/y/foo"],
        );
    }
}
