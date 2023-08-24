/*!
ignore crate 提供了一个快速的递归目录迭代器，它能够尊重各种过滤器，比如通配符、文件类型和 `.gitignore` 文件。精确的匹配规则和优先级在 `WalkBuilder` 的文档中有解释。

此外，该 crate 还为需要更精细控制的用例提供了 gitignore 和文件类型匹配器。

# 示例

以下示例展示了这个 crate 的最基本用法。这段代码会递归遍历当前目录，并根据类似 `.ignore` 和 `.gitignore` 文件中的忽略通配符自动过滤文件和目录：

```rust,no_run
use ignore::Walk;

for result in Walk::new("./") {
    // 迭代器产生的每个项都是一个目录条目或者一个错误，所以要么打印路径，要么打印错误。
    match result {
        Ok(entry) => println!("{}", entry.path().display()),
        Err(err) => println!("ERROR: {}", err),
    }
}
```

# 示例：高级用法

默认情况下，递归目录迭代器会忽略隐藏文件和目录。可以通过使用 `WalkBuilder` 来禁用这个行为：

```rust,no_run
use ignore::WalkBuilder;

for result in WalkBuilder::new("./").hidden(false).build() {
    println!("{:?}", result);
}
```

有关 `WalkBuilder` 的许多其他选项，请参阅其文档。
*/
#![deny(missing_docs)]

use std::error;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

pub use crate::walk::{
    DirEntry, ParallelVisitor, ParallelVisitorBuilder, Walk, WalkBuilder,
    WalkParallel, WalkState,
};

mod default_types;
mod dir;
pub mod gitignore;
pub mod overrides;
mod pathutil;
pub mod types;
mod walk;

/// 表示解析 gitignore 文件时可能出现的错误。
#[derive(Debug)]
pub enum Error {
    /// 一组“软”错误。这些错误在部分添加忽略文件时出现。
    Partial(Vec<Error>),
    /// 与特定行号相关联的错误。
    WithLineNumber {
        /// 行号。
        line: u64,
        /// 原始错误。
        err: Box<Error>,
    },
    /// 与特定文件路径相关联的错误。
    WithPath {
        /// 文件路径。
        path: PathBuf,
        /// 原始错误。
        err: Box<Error>,
    },
    /// 与递归遍历目录时的特定目录深度相关的错误。
    WithDepth {
        /// 目录深度。
        depth: usize,
        /// 原始错误。
        err: Box<Error>,
    },
    /// 在遍历符号链接时检测到文件循环的错误。
    Loop {
        /// 循环中的祖先文件路径。
        ancestor: PathBuf,
        /// 循环中的子文件路径。
        child: PathBuf,
    },
    /// 发生 I/O 错误时的错误，比如读取忽略文件。
    Io(io::Error),
    /// 尝试解析通配符时发生的错误。
    Glob {
        /// 导致此错误的原始通配符。此通配符当可用时，始终对应于终端用户提供的通配符。
        /// 例如，它是在 `.gitignore` 文件中写的通配符。
        ///
        /// （这个通配符可能与实际编译后的通配符不同，因为要考虑到 `gitignore` 的语义。）
        glob: Option<String>,
        /// 作为字符串的底层通配符错误。
        err: String,
    },
    /// 未定义的文件类型选择。
    UnrecognizedFileType(String),
    /// 用户指定的文件类型定义无法解析。
    InvalidDefinition,
}

impl Clone for Error {
    fn clone(&self) -> Error {
        match *self {
            Error::Partial(ref errs) => Error::Partial(errs.clone()),
            Error::WithLineNumber { line, ref err } => {
                Error::WithLineNumber { line: line, err: err.clone() }
            }
            Error::WithPath { ref path, ref err } => {
                Error::WithPath { path: path.clone(), err: err.clone() }
            }
            Error::WithDepth { depth, ref err } => {
                Error::WithDepth { depth: depth, err: err.clone() }
            }
            Error::Loop { ref ancestor, ref child } => Error::Loop {
                ancestor: ancestor.clone(),
                child: child.clone(),
            },
            Error::Io(ref err) => match err.raw_os_error() {
                Some(e) => Error::Io(io::Error::from_raw_os_error(e)),
                None => Error::Io(io::Error::new(err.kind(), err.to_string())),
            },
            Error::Glob { ref glob, ref err } => {
                Error::Glob { glob: glob.clone(), err: err.clone() }
            }
            Error::UnrecognizedFileType(ref err) => {
                Error::UnrecognizedFileType(err.clone())
            }
            Error::InvalidDefinition => Error::InvalidDefinition,
        }
    }
}

impl Error {
    /// 如果这是一个部分错误，则返回 true。
    ///
    /// 部分错误发生在只有部分操作失败，而其他操作可能已成功的情况下。例如，忽略文件可能包含一个无效的通配符，但其他通配符仍然有效。
    pub fn is_partial(&self) -> bool {
        match *self {
            Error::Partial(_) => true,
            Error::WithLineNumber { ref err, .. } => err.is_partial(),
            Error::WithPath { ref err, .. } => err.is_partial(),
            Error::WithDepth { ref err, .. } => err.is_partial(),
            _ => false,
        }
    }

    /// 如果此错误仅为 I/O 错误，则返回 true。
    pub fn is_io(&self) -> bool {
        match *self {
            Error::Partial(ref errs) => errs.len() == 1 && errs[0].is_io(),
            Error::WithLineNumber { ref err, .. } => err.is_io(),
            Error::WithPath { ref err, .. } => err.is_io(),
            Error::WithDepth { ref err, .. } => err.is_io(),
            Error::Loop { .. } => false,
            Error::Io(_) => true,
            Error::Glob { .. } => false,
            Error::UnrecognizedFileType(_) => false,
            Error::InvalidDefinition => false,
        }
    }

    /// 检查是否有原始 [`io::Error`]。
    ///
    /// 如果 [`Error`] 不对应于 [`io::Error`]，则返回 [`None`]。这可能发生在例如在跟随符号链接时在目录树中发现循环的错误。
    ///
    /// 此方法返回一个与 [`Error`] 的生命周期绑定的借用值。要获取拥有的值，可以使用 [`into_io_error`]。
    ///
    /// > 这是原始 [`io::Error`]，与 [`impl From<Error> for std::io::Error`][impl] 不同，后者包含了关于错误的其他上下文。
    ///
    /// [`None`]: https://doc.rust-lang.org/stable/std/option/enum.Option.html#variant.None
    /// [`io::Error`]: https://doc.rust-lang.org/stable/std/io/struct.Error.html
    /// [`From`]: https://doc.rust-lang.org/stable/std/convert/trait.From.html
    /// [`Error`]: struct.Error.html
    /// [`into_io_error`]: struct.Error.html#method.into_io_error
    /// [impl]: struct.Error.html#impl-From%3CError%3E
    pub fn io_error(&self) -> Option<&std::io::Error> {
        match *self {
            Error::Partial(ref errs) => {
                if errs.len() == 1 {
                    errs[0].io_error()
                } else {
                    None
                }
            }
            Error::WithLineNumber { ref err, .. } => err.io_error(),
            Error::WithPath { ref err, .. } => err.io_error(),
            Error::WithDepth { ref err, .. } => err.io_error(),
            Error::Loop { .. } => None,
            Error::Io(ref err) => Some(err),
            Error::Glob { .. } => None,
            Error::UnrecognizedFileType(_) => None,
            Error::InvalidDefinition => None,
        }
    }

    /// 类似于 [`io_error`]，但将自身消耗以转换为原始的 [`io::Error`]（如果存在）。
    ///
    /// [`io_error`]: struct.Error.html#method.io_error
    /// [`io::Error`]: https://doc.rust-lang.org/stable/std/io/struct.Error.html
    pub fn into_io_error(self) -> Option<std::io::Error> {
        match self {
            Error::Partial(mut errs) => {
                if errs.len() == 1 {
                    errs.remove(0).into_io_error()
                } else {
                    None
                }
            }
            Error::WithLineNumber { err, .. } => err.into_io_error(),
            Error::WithPath { err, .. } => err.into_io_error(),
            Error::WithDepth { err, .. } => err.into_io_error(),
            Error::Loop { .. } => None,
            Error::Io(err) => Some(err),
            Error::Glob { .. } => None,
            Error::UnrecognizedFileType(_) => None,
            Error::InvalidDefinition => None,
        }
    }

    /// 返回与递归遍历目录相关联的深度（如果此错误是从递归目录迭代器生成的）。
    pub fn depth(&self) -> Option<usize> {
        match *self {
            Error::WithPath { ref err, .. } => err.depth(),
            Error::WithDepth { depth, .. } => Some(depth),
            _ => None,
        }
    }

    /// 将错误转换为带有给定文件路径的标记错误。
    fn with_path<P: AsRef<Path>>(self, path: P) -> Error {
        Error::WithPath {
            path: path.as_ref().to_path_buf(),
            err: Box::new(self),
        }
    }

    /// 将错误转换为带有给定深度的标记错误。
    fn with_depth(self, depth: usize) -> Error {
        Error::WithDepth { depth: depth, err: Box::new(self) }
    }

    /// 将错误转换为带有给定文件路径和行号的标记错误。如果路径为空，则省略错误中的路径。
    fn tagged<P: AsRef<Path>>(self, path: P, lineno: u64) -> Error {
        let errline =
            Error::WithLineNumber { line: lineno, err: Box::new(self) };
        if path.as_ref().as_os_str().is_empty() {
            return errline;
        }
        errline.with_path(path)
    }

    /// 从 walkdir 错误构建错误。
    fn from_walkdir(err: walkdir::Error) -> Error {
        let depth = err.depth();
        if let (Some(anc), Some(child)) = (err.loop_ancestor(), err.path()) {
            return Error::WithDepth {
                depth: depth,
                err: Box::new(Error::Loop {
                    ancestor: anc.to_path_buf(),
                    child: child.to_path_buf(),
                }),
            };
        }
        let path = err.path().map(|p| p.to_path_buf());
        let mut ig_err = Error::Io(io::Error::from(err));
        if let Some(path) = path {
            ig_err = Error::WithPath { path: path, err: Box::new(ig_err) };
        }
        ig_err
    }
}

impl error::Error for Error {
    #[allow(deprecated)]
    fn description(&self) -> &str {
        match *self {
            Error::Partial(_) => "partial error",
            Error::WithLineNumber { ref err, .. } => err.description(),
            Error::WithPath { ref err, .. } => err.description(),
            Error::WithDepth { ref err, .. } => err.description(),
            Error::Loop { .. } => "file system loop found",
            Error::Io(ref err) => err.description(),
            Error::Glob { ref err, .. } => err,
            Error::UnrecognizedFileType(_) => "unrecognized file type",
            Error::InvalidDefinition => "invalid definition",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Partial(ref errs) => {
                let msgs: Vec<String> =
                    errs.iter().map(|err| err.to_string()).collect();
                write!(f, "{}", msgs.join("\n"))
            }
            Error::WithLineNumber { line, ref err } => {
                write!(f, "line {}: {}", line, err)
            }
            Error::WithPath { ref path, ref err } => {
                write!(f, "{}: {}", path.display(), err)
            }
            Error::WithDepth { ref err, .. } => err.fmt(f),
            Error::Loop { ref ancestor, ref child } => write!(
                f,
                "File system loop found: \
                           {} points to an ancestor {}",
                child.display(),
                ancestor.display()
            ),
            Error::Io(ref err) => err.fmt(f),
            Error::Glob { glob: None, ref err } => write!(f, "{}", err),
            Error::Glob { glob: Some(ref glob), ref err } => {
                write!(f, "error parsing glob '{}': {}", glob, err)
            }
            Error::UnrecognizedFileType(ref ty) => {
                write!(f, "unrecognized file type: {}", ty)
            }
            Error::InvalidDefinition => write!(
                f,
                "invalid definition (format is type:glob, e.g., \
                           html:*.html)"
            ),
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::Io(err)
    }
}

#[derive(Debug, Default)]
struct PartialErrorBuilder(Vec<Error>);

impl PartialErrorBuilder {
    fn push(&mut self, err: Error) {
        self.0.push(err);
    }

    fn push_ignore_io(&mut self, err: Error) {
        if !err.is_io() {
            self.push(err);
        }
    }

    fn maybe_push(&mut self, err: Option<Error>) {
        if let Some(err) = err {
            self.push(err);
        }
    }

    fn maybe_push_ignore_io(&mut self, err: Option<Error>) {
        if let Some(err) = err {
            self.push_ignore_io(err);
        }
    }

    fn into_error_option(mut self) -> Option<Error> {
        if self.0.is_empty() {
            None
        } else if self.0.len() == 1 {
            Some(self.0.pop().unwrap())
        } else {
            Some(Error::Partial(self.0))
        }
    }
}
/// 匹配结果的枚举类型，代表通配符匹配的结果。
///
/// 类型参数 `T` 通常指的是提供有关特定匹配的更多信息的类型。
/// 例如，它可能标识导致匹配的特定 gitignore 文件和特定通配符模式。
#[derive(Clone, Debug)]
pub enum Match<T> {
    /// 路径未匹配任何通配符。
    None,
    /// 最高优先级的匹配的通配符指示路径应该被忽略。
    Ignore(T),
    /// 最高优先级的匹配的通配符指示路径应该被加入白名单。
    Whitelist(T),
}

impl<T> Match<T> {
    /// 如果匹配结果未匹配任何通配符，则返回 true。
    pub fn is_none(&self) -> bool {
        match *self {
            Match::None => true,
            Match::Ignore(_) | Match::Whitelist(_) => false,
        }
    }

    /// 如果匹配结果表明路径应该被忽略，则返回 true。
    pub fn is_ignore(&self) -> bool {
        match *self {
            Match::Ignore(_) => true,
            Match::None | Match::Whitelist(_) => false,
        }
    }

    /// 如果匹配结果表明路径应该被加入白名单，则返回 true。
    pub fn is_whitelist(&self) -> bool {
        match *self {
            Match::Whitelist(_) => true,
            Match::None | Match::Ignore(_) => false,
        }
    }

    /// 反转匹配，使 `Ignore` 变为 `Whitelist`，`Whitelist` 变为 `Ignore`。
    /// 非匹配结果保持不变。
    pub fn invert(self) -> Match<T> {
        match self {
            Match::None => Match::None,
            Match::Ignore(t) => Match::Whitelist(t),
            Match::Whitelist(t) => Match::Ignore(t),
        }
    }

    /// 如果存在，则返回此匹配中的值。
    pub fn inner(&self) -> Option<&T> {
        match *self {
            Match::None => None,
            Match::Ignore(ref t) => Some(t),
            Match::Whitelist(ref t) => Some(t),
        }
    }

    /// 对此匹配中的值应用给定的函数。
    ///
    /// 如果匹配没有值，则返回不变的匹配结果。
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> Match<U> {
        match self {
            Match::None => Match::None,
            Match::Ignore(t) => Match::Ignore(f(t)),
            Match::Whitelist(t) => Match::Whitelist(f(t)),
        }
    }

    /// 如果不是 none 匹配，则返回匹配本身。否则，返回其他匹配。
    pub fn or(self, other: Self) -> Self {
        if self.is_none() {
            other
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::error;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::result;

    /// 一个方便的结果类型别名。
    pub type Result<T> =
        result::Result<T, Box<dyn error::Error + Send + Sync>>;

    macro_rules! err {
        ($($tt:tt)*) => {
            Box::<dyn error::Error + Send + Sync>::from(format!($($tt)*))
        }
    }

    /// 一个简单的包装器，用于创建临时目录，在丢弃时会自动删除。
    ///
    /// 我们使用这个来替代 tempfile，因为 tempfile 带来了太多依赖。
    #[derive(Debug)]
    pub struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    impl TempDir {
        /// 在系统配置的临时目录下创建一个新的空临时目录。
        pub fn new() -> Result<TempDir> {
            use std::sync::atomic::{AtomicUsize, Ordering};

            static TRIES: usize = 100;
            static COUNTER: AtomicUsize = AtomicUsize::new(0);

            let tmpdir = env::temp_dir();
            for _ in 0..TRIES {
                let count = COUNTER.fetch_add(1, Ordering::SeqCst);
                let path = tmpdir.join("rust-ignore").join(count.to_string());
                if path.is_dir() {
                    continue;
                }
                fs::create_dir_all(&path)
                    .map_err(|e| err!("无法创建 {}: {}", path.display(), e))?;
                return Ok(TempDir(path));
            }
            Err(err!("在 {} 次尝试后无法创建临时目录", TRIES))
        }

        /// 返回此临时目录的基础路径。
        pub fn path(&self) -> &Path {
            &self.0
        }
    }
}
