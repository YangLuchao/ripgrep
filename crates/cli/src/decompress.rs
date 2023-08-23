use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::process::{CommandError, CommandReader, CommandReaderBuilder};
/// 用于构建决定哪些文件将被解压缩的匹配器的构建器。
#[derive(Clone, Debug)]
pub struct DecompressionMatcherBuilder {
    /// 每个匹配的 glob 对应的命令。
    commands: Vec<DecompressionCommand>,
    /// 是否包含默认的匹配规则。
    defaults: bool,
}

/// 表示一个用于在进程外部解压缩数据的单个命令的表示。
#[derive(Clone, Debug)]
struct DecompressionCommand {
    /// 匹配此命令的 glob。
    glob: String,
    /// 命令或二进制名称。
    bin: PathBuf,
    /// 与命令一起调用的参数。
    args: Vec<OsString>,
}

impl Default for DecompressionMatcherBuilder {
    fn default() -> DecompressionMatcherBuilder {
        DecompressionMatcherBuilder::new()
    }
}

impl DecompressionMatcherBuilder {
    /// 创建一个新的构建器，用于配置解压缩匹配器。
    pub fn new() -> DecompressionMatcherBuilder {
        DecompressionMatcherBuilder { commands: vec![], defaults: true }
    }

    /// 构建用于确定如何解压缩文件的匹配器。
    ///
    /// 如果编译匹配器时出现问题，则返回错误。
    pub fn build(&self) -> Result<DecompressionMatcher, CommandError> {
        let defaults = if !self.defaults {
            vec![]
        } else {
            default_decompression_commands()
        };
        let mut glob_builder = GlobSetBuilder::new();
        let mut commands = vec![];
        for decomp_cmd in defaults.iter().chain(&self.commands) {
            let glob = Glob::new(&decomp_cmd.glob).map_err(|err| {
                CommandError::io(io::Error::new(io::ErrorKind::Other, err))
            })?;
            glob_builder.add(glob);
            commands.push(decomp_cmd.clone());
        }
        let globs = glob_builder.build().map_err(|err| {
            CommandError::io(io::Error::new(io::ErrorKind::Other, err))
        })?;
        Ok(DecompressionMatcher { globs, commands })
    }

    /// 启用时，将默认匹配规则编译到此匹配器中，然后再添加任何其他关联。禁用时，只使用显式提供给此构建器的规则。
    ///
    /// 默认情况下启用此选项。
    pub fn defaults(&mut self, yes: bool) -> &mut DecompressionMatcherBuilder {
        self.defaults = yes;
        self
    }

    /// 将 glob 与解压缩匹配的命令关联起来。
    ///
    /// 如果多个 glob 匹配同一个文件，则最近添加的 glob 优先。
    ///
    /// glob 的语法在 [`globset` crate](https://docs.rs/globset/#syntax) 中有文档记录。
    ///
    /// 给定的 `program` 将根据 `PATH` 解析，并在内部被转换为绝对路径，然后由当前平台执行。
    /// 特别是在 Windows 上，这避免了将相对路径传递给 `CreateProcess`，后者将自动在当前目录中搜索匹配的程序的安全问题。
    /// 如果无法解析该程序，则会被静默忽略，并且关联会被删除。因此，调用者应优先使用 `try_associate`。
    pub fn associate<P, I, A>(
        &mut self,
        glob: &str,
        program: P,
        args: I,
    ) -> &mut DecompressionMatcherBuilder
    where
        P: AsRef<OsStr>,
        I: IntoIterator<Item = A>,
        A: AsRef<OsStr>,
    {
        let _ = self.try_associate(glob, program, args);
        self
    }

    /// 将 glob 与解压缩匹配的命令关联起来。
    ///
    /// 如果多个 glob 匹配同一个文件，则最近添加的 glob 优先。
    ///
    /// glob 的语法在 [`globset` crate](https://docs.rs/globset/#syntax) 中有文档记录。
    ///
    /// 给定的 `program` 将根据 `PATH` 解析，并在内部被转换为绝对路径，然后由当前平台执行。
    /// 特别是在 Windows 上，这避免了将相对路径传递给 `CreateProcess`，后者将自动在当前目录中搜索匹配的程序的安全问题。
    /// 如果无法解析该程序，则会返回错误。
    pub fn try_associate<P, I, A>(
        &mut self,
        glob: &str,
        program: P,
        args: I,
    ) -> Result<&mut DecompressionMatcherBuilder, CommandError>
    where
        P: AsRef<OsStr>,
        I: IntoIterator<Item = A>,
        A: AsRef<OsStr>,
    {
        let glob = glob.to_string();
        let bin = try_resolve_binary(Path::new(program.as_ref()))?;
        let args =
            args.into_iter().map(|a| a.as_ref().to_os_string()).collect();
        self.commands.push(DecompressionCommand { glob, bin, args });
        Ok(self)
    }
}

/// 用于确定如何解压缩文件的匹配器。
#[derive(Clone, Debug)]
pub struct DecompressionMatcher {
    /// 要匹配的 glob 集合。每个 glob 在 `commands` 中都有对应的条目。
    /// 当 glob 匹配时，应使用相应的命令执行进程外部解压缩。
    globs: GlobSet,
    /// 每个匹配的 glob 对应的命令。
    commands: Vec<DecompressionCommand>,
}

impl Default for DecompressionMatcher {
    fn default() -> DecompressionMatcher {
        DecompressionMatcher::new()
    }
}

impl DecompressionMatcher {
    /// 创建一个具有默认规则的新匹配器。
    ///
    /// 要添加更多匹配规则，请使用 [`DecompressionMatcherBuilder`](struct.DecompressionMatcherBuilder.html) 构建匹配器。
    pub fn new() -> DecompressionMatcher {
        DecompressionMatcherBuilder::new()
            .build()
            .expect("内置匹配规则应始终能够编译")
    }

    /// 基于给定文件路径创建一个预构建的命令，可以解压缩其内容。如果不知道这样的解压程序，则返回 `None`。
    ///
    /// 如果有多个可能的命令与给定路径匹配，则最后添加的命令优先。
    pub fn command<P: AsRef<Path>>(&self, path: P) -> Option<Command> {
        for i in self.globs.matches(path).into_iter().rev() {
            let decomp_cmd = &self.commands[i];
            let mut cmd = Command::new(&decomp_cmd.bin);
            cmd.args(&decomp_cmd.args);
            return Some(cmd);
        }
        None
    }

    /// 当且仅当给定文件路径至少有一个匹配的命令可以执行解压缩时，返回 true。
    pub fn has_command<P: AsRef<Path>>(&self, path: P) -> bool {
        self.globs.is_match(path)
    }
}

/// 配置和构建用于解压缩数据的流式阅读器。
#[derive(Clone, Debug, Default)]
pub struct DecompressionReaderBuilder {
    matcher: DecompressionMatcher,
    command_builder: CommandReaderBuilder,
}

impl DecompressionReaderBuilder {
    /// 使用默认配置创建一个新的构建器。
    pub fn new() -> DecompressionReaderBuilder {
        DecompressionReaderBuilder::default()
    }

    /// 构建用于解压缩数据的新流式阅读器。
    ///
    /// 如果解压缩是在进程外部完成的，且在生成进程时遇到问题，则会在调试级别记录错误，并返回一个不执行解压缩的 passthru 阅读器。
    /// 这通常发生在给定的文件路径与解压缩命令匹配，但在解压缩命令不可用的环境中执行。
    ///
    /// 如果给定的文件路径无法与解压缩策略匹配，则返回一个不执行解压缩的 passthru 阅读器。
    pub fn build<P: AsRef<Path>>(
        &self,
        path: P,
    ) -> Result<DecompressionReader, CommandError> {
        let path = path.as_ref();
        let mut cmd = match self.matcher.command(path) {
            None => return DecompressionReader::new_passthru(path),
            Some(cmd) => cmd,
        };
        cmd.arg(path);

        match self.command_builder.build(&mut cmd) {
            Ok(cmd_reader) => Ok(DecompressionReader { rdr: Ok(cmd_reader) }),
            Err(err) => {
                log::debug!(
                    "{}: 生成命令 '{:?}' 时出错：{} \
                     （回退到未压缩的阅读器）",
                    path.display(),
                    cmd,
                    err,
                );
                DecompressionReader::new_passthru(path)
            }
        }
    }

    /// 设置要用于查找每个文件路径的解压缩命令的匹配器。
    ///
    /// 默认情况下启用了一组明智的规则。设置此选项将完全替换当前规则。
    pub fn matcher(
        &mut self,
        matcher: DecompressionMatcher,
    ) -> &mut DecompressionReaderBuilder {
        self.matcher = matcher;
        self
    }

    /// 获取当前构建器当前使用的基础匹配器。
    pub fn get_matcher(&self) -> &DecompressionMatcher {
        &self.matcher
    }

    /// 当启用时，阅读器将异步读取命令的 stderr 输出。当禁用时，在 stdout 流耗尽后才读取 stderr，或者进程以错误代码退出。
    ///
    /// 请注意，启用此功能可能需要启动一个额外的线程来读取 stderr。这样做是为了确保正在执行的进程不会被阻塞，无法写入 stdout 或 stderr。
    /// 如果禁用此功能，则进程可能会填充 stderr 缓冲区并死锁。
    ///
    /// 默认情况下启用此选项。
    pub fn async_stderr(
        &mut self,
        yes: bool,
    ) -> &mut DecompressionReaderBuilder {
        self.command_builder.async_stderr(yes);
        self
    }
}
/// 用于解压缩文件内容的流式阅读器。
///
/// 此阅读器的目的是通过使用当前环境中的现有工具，提供一种无缝的方式来解压缩文件的内容。
/// 这是使用外部命令（如 `gzip` 和 `xz`）而不是使用解压缩库的简单性和可移植性的替代方法。
/// 这会带来生成进程的开销，因此如果无法接受此开销，则应寻找其他执行解压缩的方式。
///
/// 解压缩阅读器配备了一组默认的匹配规则，用于将文件路径与用于解压缩文件的相应命令关联起来。
/// 例如，像 `*.gz` 这样的通配符将与使用命令 `gzip -d -c` 解压缩的 gzip 压缩文件匹配。
/// 如果文件路径与任何现有规则不匹配，或者与规则匹配的命令在当前环境中不存在，则解压缩阅读器将通过底层文件内容而不执行任何解压缩。
///
/// 默认的匹配规则可能对大多数情况都足够好，如果需要修改，欢迎提交请求。
/// 在必须更改或扩展它们的情况下，可以通过使用 [`DecompressionMatcherBuilder`](struct.DecompressionMatcherBuilder.html)
/// 和 [`DecompressionReaderBuilder`](struct.DecompressionReaderBuilder.html) 进行自定义。
///
/// 默认情况下，此阅读器将异步读取进程的 stderr 输出。
/// 这可以防止对 stderr 写入大量内容的嘈杂进程的微妙死锁错误。当前，stderr 的整个内容都会被读入堆中。
///
/// # 示例
///
/// 以下示例演示了如何读取文件的解压缩内容，而无需显式选择要运行的解压缩命令。
///
/// 注意，如果您需要解压缩多个文件，则最好使用 `DecompressionReaderBuilder`，
/// 它将摊销匹配器的构建成本。
///
/// ```no_run
/// use std::io::Read;
/// use std::process::Command;
/// use grep_cli::DecompressionReader;
///
/// # fn example() -> Result<(), Box<::std::error::Error>> {
/// let mut rdr = DecompressionReader::new("/usr/share/man/man1/ls.1.gz")?;
/// let mut contents = vec![];
/// rdr.read_to_end(&mut contents)?;
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct DecompressionReader {
    rdr: Result<CommandReader, File>,
}

impl DecompressionReader {
    /// 构建一个新的用于解压缩数据的流式阅读器。
    ///
    /// 如果解压缩是在进程外部完成的，且在生成进程时遇到问题，则会返回错误。
    ///
    /// 如果给定的文件路径无法与解压缩策略匹配，则将返回不执行解压缩的 passthru 阅读器。
    ///
    /// 这将使用默认的匹配规则来确定如何解压缩给定的文件。
    /// 要更改这些匹配规则，请使用 [`DecompressionReaderBuilder`](struct.DecompressionReaderBuilder.html)
    /// 和 [`DecompressionMatcherBuilder`](struct.DecompressionMatcherBuilder.html)。
    ///
    /// 在为多个路径创建阅读器时，最好使用构建器，因为它将摊销匹配器的构建成本。
    pub fn new<P: AsRef<Path>>(
        path: P,
    ) -> Result<DecompressionReader, CommandError> {
        DecompressionReaderBuilder::new().build(path)
    }

    /// 创建一个新的“passthru”解压缩阅读器，从与给定路径对应的文件中读取，而不进行解压缩并且不执行其他进程。
    fn new_passthru(path: &Path) -> Result<DecompressionReader, CommandError> {
        let file = File::open(path)?;
        Ok(DecompressionReader { rdr: Err(file) })
    }

    /// 关闭此阅读器，释放其底层子进程使用的任何资源。
    /// 如果使用了子进程，且子进程以非零退出代码退出，则返回的 Err 值将包括其 stderr。
    ///
    /// `close` 是幂等的，意味着可以安全地多次调用。第一次调用将关闭 CommandReader，后续调用将不起作用。
    ///
    /// 应在部分读取文件后调用此方法，以防止资源泄漏。但是，如果代码始终调用 `read` 到 EOF，则不需要显式调用 `close`。
    ///
    /// `close` 也会在 `drop` 中调用，作为防止资源泄漏的最后一道防线。然后，来自子进程的任何错误将作为警告打印到 stderr。
    /// 可以通过在 CommandReader 被丢弃之前显式调用 `close` 来避免这种情况。
    pub fn close(&mut self) -> io::Result<()> {
        match self.rdr {
            Ok(ref mut rdr) => rdr.close(),
            Err(_) => Ok(()),
        }
    }
}

impl io::Read for DecompressionReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.rdr {
            Ok(ref mut rdr) => rdr.read(buf),
            Err(ref mut rdr) => rdr.read(buf),
        }
    }
}

/// 通过在 `PATH` 中查找程序的路径来解析程序的路径。
///
/// 如果无法解析程序，则返回错误。
///
/// 这样做的目的是，与直接将程序路径传递给 Command::new 不同，
/// Command::new 将相对路径传递给 Windows 上的 CreateProcess，后者将隐式搜索当前工作目录中的可执行文件。
/// 鉴于安全原因，这可能是不希望的行为。例如，在不受信任的目录树上使用 -z/--search-zip 标志运行 ripgrep
/// 可能会导致在 Windows 上执行任意程序。
///
/// 请注意，如果 PATH 包含相对路径，这仍然可能返回相对路径。我们允许这样做，因为假定用户已经显式设置了这一点，
/// 因此期望此行为。
///
/// 在非 Windows 系统上，这是一个空操作。
pub fn resolve_binary<P: AsRef<Path>>(
    prog: P,
) -> Result<PathBuf, CommandError> {
    if !cfg!(windows) {
        return Ok(prog.as_ref().to_path_buf());
    }
    try_resolve_binary(prog)
}

/// 通过在 `PATH` 中查找程序的路径来解析程序的路径。
///
/// 如果无法解析程序，则返回错误。
///
/// 这样做的目的是，与直接将程序路径传递给 Command::new 不同，
/// Command::new 将相对路径传递给 Windows 上的 CreateProcess，后者将隐式搜索当前工作目录中的可执行文件。
/// 鉴于安全原因，这可能是不希望的行为。例如，在不受信任的目录树上使用 -z/--search-zip 标志运行 ripgrep
/// 可能会导致在 Windows 上执行任意程序。
///
/// 请注意，如果 PATH 包含相对路径，这仍然可能返回相对路径。我们允许这样做，因为假定用户已经显式设置了这一点，
/// 因此期望此行为。
///
/// 如果 `check_exists` 为 false，或者路径已经是绝对路径，则会立即返回。
fn try_resolve_binary<P: AsRef<Path>>(
    prog: P,
) -> Result<PathBuf, CommandError> {
    use std::env;

    fn is_exe(path: &Path) -> bool {
        let md = match path.metadata() {
            Err(_) => return false,
            Ok(md) => md,
        };
        !md.is_dir()
    }

    let prog = prog.as_ref();
    if prog.is_absolute() {
        return Ok(prog.to_path_buf());
    }
    let syspaths = match env::var_os("PATH") {
        Some(syspaths) => syspaths,
        None => {
            let msg = "无法找到系统 PATH 环境变量";
            return Err(CommandError::io(io::Error::new(
                io::ErrorKind::Other,
                msg,
            )));
        }
    };
    for syspath in env::split_paths(&syspaths) {
        if syspath.as_os_str().is_empty() {
            continue;
        }
        let abs_prog = syspath.join(prog);
        if is_exe(&abs_prog) {
            return Ok(abs_prog.to_path_buf());
        }
        if abs_prog.extension().is_none() {
            for extension in ["com", "exe"] {
                let abs_prog = abs_prog.with_extension(extension);
                if is_exe(&abs_prog) {
                    return Ok(abs_prog.to_path_buf());
                }
            }
        }
    }
    let msg = format!("{}: 无法在 PATH 中找到可执行文件", prog.display());
    return Err(CommandError::io(io::Error::new(io::ErrorKind::Other, msg)));
}

/// 默认的解压缩命令。
fn default_decompression_commands() -> Vec<DecompressionCommand> {
    const ARGS_GZIP: &[&str] = &["gzip", "-d", "-c"];
    const ARGS_BZIP: &[&str] = &["bzip2", "-d", "-c"];
    const ARGS_XZ: &[&str] = &["xz", "-d", "-c"];
    const ARGS_LZ4: &[&str] = &["lz4", "-d", "-c"];
    const ARGS_LZMA: &[&str] = &["xz", "--format=lzma", "-d", "-c"];
    const ARGS_BROTLI: &[&str] = &["brotli", "-d", "-c"];
    const ARGS_ZSTD: &[&str] = &["zstd", "-q", "-d", "-c"];
    const ARGS_UNCOMPRESS: &[&str] = &["uncompress", "-c"];

    fn add(glob: &str, args: &[&str], cmds: &mut Vec<DecompressionCommand>) {
        let bin = match resolve_binary(Path::new(args[0])) {
            Ok(bin) => bin,
            Err(err) => {
                log::debug!("{}", err);
                return;
            }
        };
        cmds.push(DecompressionCommand {
            glob: glob.to_string(),
            bin,
            args: args
                .iter()
                .skip(1)
                .map(|s| OsStr::new(s).to_os_string())
                .collect(),
        });
    }
    let mut cmds = vec![];
    add("*.gz", ARGS_GZIP, &mut cmds);
    add("*.tgz", ARGS_GZIP, &mut cmds);
    add("*.bz2", ARGS_BZIP, &mut cmds);
    add("*.tbz2", ARGS_BZIP, &mut cmds);
    add("*.xz", ARGS_XZ, &mut cmds);
    add("*.txz", ARGS_XZ, &mut cmds);
    add("*.lz4", ARGS_LZ4, &mut cmds);
    add("*.lzma", ARGS_LZMA, &mut cmds);
    add("*.br", ARGS_BROTLI, &mut cmds);
    add("*.zst", ARGS_ZSTD, &mut cmds);
    add("*.zstd", ARGS_ZSTD, &mut cmds);
    add("*.Z", ARGS_UNCOMPRESS, &mut cmds);
    cmds
}
