use std::error;
use std::fmt;
use std::io::{self, Read};
use std::iter;
use std::process;
use std::thread::{self, JoinHandle};
/// 在运行命令并读取其输出时可能发生的错误。
///
/// 通过 `From` 实现，此错误可以无缝转换为 `io::Error`。
#[derive(Debug)]
pub struct CommandError {
    kind: CommandErrorKind,
}

#[derive(Debug)]
enum CommandErrorKind {
    Io(io::Error),
    Stderr(Vec<u8>),
}

impl CommandError {
    /// 从 I/O 错误创建一个错误。
    pub(crate) fn io(ioerr: io::Error) -> CommandError {
        CommandError { kind: CommandErrorKind::Io(ioerr) }
    }

    /// 从 stderr 的内容创建一个错误（可能为空）。
    pub(crate) fn stderr(bytes: Vec<u8>) -> CommandError {
        CommandError { kind: CommandErrorKind::Stderr(bytes) }
    }

    /// 当且仅当此错误的 stderr 数据为空时返回 true。
    pub(crate) fn is_empty(&self) -> bool {
        match self.kind {
            CommandErrorKind::Stderr(ref bytes) => bytes.is_empty(),
            _ => false,
        }
    }
}

impl error::Error for CommandError {
    fn description(&self) -> &str {
        "命令错误"
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            CommandErrorKind::Io(ref e) => e.fmt(f),
            CommandErrorKind::Stderr(ref bytes) => {
                let msg = String::from_utf8_lossy(bytes);
                if msg.trim().is_empty() {
                    write!(f, "<stderr 为空>")
                } else {
                    let div = iter::repeat('-').take(79).collect::<String>();
                    write!(
                        f,
                        "\n{div}\n{msg}\n{div}",
                        div = div,
                        msg = msg.trim()
                    )
                }
            }
        }
    }
}

impl From<io::Error> for CommandError {
    fn from(ioerr: io::Error) -> CommandError {
        CommandError { kind: CommandErrorKind::Io(ioerr) }
    }
}

impl From<CommandError> for io::Error {
    fn from(cmderr: CommandError) -> io::Error {
        match cmderr.kind {
            CommandErrorKind::Io(ioerr) => ioerr,
            CommandErrorKind::Stderr(_) => {
                io::Error::new(io::ErrorKind::Other, cmderr)
            }
        }
    }
}

/// 配置并构建用于处理进程输出的流式读取器。
#[derive(Clone, Debug, Default)]
pub struct CommandReaderBuilder {
    async_stderr: bool,
}

impl CommandReaderBuilder {
    /// 创建一个具有默认配置的新构建器。
    pub fn new() -> CommandReaderBuilder {
        CommandReaderBuilder::default()
    }

    /// 构建给定命令输出的新流式读取器。
    ///
    /// 调用者应该在构建读取器之前设置给定命令的所有必需属性，如其参数、环境和当前工作目录。
    /// 诸如 stdout 和 stderr（但不包括 stdin）管道的设置将被覆盖，以便可以由读取器控制。
    ///
    /// 如果生成给定命令出现问题，则返回其错误。
    pub fn build(
        &self,
        command: &mut process::Command,
    ) -> Result<CommandReader, CommandError> {
        let mut child = command
            .stdout(process::Stdio::piped())
            .stderr(process::Stdio::piped())
            .spawn()?;
        let stderr = if self.async_stderr {
            StderrReader::r#async(child.stderr.take().unwrap())
        } else {
            StderrReader::sync(child.stderr.take().unwrap())
        };
        Ok(CommandReader { child, stderr, eof: false })
    }

    /// 当启用时，读取器将异步读取命令的 stderr 输出的内容。当禁用时，只有在 stdout 流被用尽（或进程以错误码退出）后，才会读取 stderr。
    ///
    /// 请注意，当启用此选项时，可能需要启动一个额外的线程来读取 stderr 的内容。
    /// 这样做是为了确保被执行的进程不会被阻塞，从而无法写入 stdout 或 stderr。
    /// 如果禁用此选项，那么进程有可能填满 stderr 缓冲区并导致死锁。
    ///
    /// 默认情况下启用此选项。
    pub fn async_stderr(&mut self, yes: bool) -> &mut CommandReaderBuilder {
        self.async_stderr = yes;
        self
    }
}
/// 用于处理命令输出的流式读取器。
///
/// 此读取器的目的是为了在读取进程的 stdout 时以流式方式执行进程，同时在进程以退出码失败时也可以访问进程的 stderr。这使得在错误情况下能够展示底层的失败模式。
///
/// 此外，默认情况下，此读取器将异步读取进程的 stderr。这可以防止对 stderr 有大量写入的嘈杂进程导致的微妙死锁错误。目前，stderr 的整个内容都被读取到堆上。
///
/// # 示例
///
/// 此示例演示如何调用 `gzip` 来解压文件的内容。如果 `gzip` 命令报告失败的退出状态，则其 stderr 会作为错误返回。
///
/// ```no_run
/// use std::io::Read;
/// use std::process::Command;
/// use grep_cli::CommandReader;
///
/// # fn example() -> Result<(), Box<::std::error::Error>> {
/// let mut cmd = Command::new("gzip");
/// cmd.arg("-d").arg("-c").arg("/usr/share/man/man1/ls.1.gz");
///
/// let mut rdr = CommandReader::new(&mut cmd)?;
/// let mut contents = vec![];
/// rdr.read_to_end(&mut contents)?;
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct CommandReader {
    child: process::Child,
    stderr: StderrReader,
    /// 一旦 'read' 返回零字节，就会将其设置为 true。当未设置时，关闭读取器时，我们预期在回收子进程时会产生管道错误，并将其抑制。
    eof: bool,
}

impl CommandReader {
    /// 使用默认配置创建给定命令的新流式读取器。
    ///
    /// 调用者应该在构建读取器之前设置给定命令的所有必需属性，如其参数、环境和当前工作目录。
    /// 诸如 stdout 和 stderr（但不包括 stdin）管道的设置将被覆盖，以便可以由读取器控制。
    ///
    /// 如果生成给定命令出现问题，则返回其错误。
    ///
    /// 如果调用者需要为返回的读取器进行其他配置，则使用 [`CommandReaderBuilder`](struct.CommandReaderBuilder.html)。
    pub fn new(
        cmd: &mut process::Command,
    ) -> Result<CommandReader, CommandError> {
        CommandReaderBuilder::new().build(cmd)
    }

    /// 关闭 CommandReader，释放其底层子进程使用的任何资源。如果子进程以非零退出码退出，则返回的 Err 值将包括其 stderr。
    ///
    /// `close` 是幂等的，意味着可以安全地多次调用。第一次调用将关闭 CommandReader，后续的调用不会做任何操作。
    ///
    /// 在部分读取文件后，应调用此方法以防止资源泄漏。但是，如果代码始终调用 `read` 到 EOF，则无需显式调用 `close`。
    ///
    /// 在 `drop` 中也会调用 `close`，作为对防止资源泄漏的最后防线。然后，任何来自子进程的错误都会被打印为 stderr 中的警告。
    /// 可以通过在 CommandReader 被丢弃之前显式调用 `close` 来避免这种情况。
    pub fn close(&mut self) -> io::Result<()> {
        // 丢弃 stdout 会关闭底层文件描述符，这应该会导致一个行为良好的子进程退出。
        // 如果 child.stdout 为 None，则我们假定已经调用了 close()，不做任何操作。
        let stdout = match self.child.stdout.take() {
            None => return Ok(()),
            Some(stdout) => stdout,
        };
        drop(stdout);
        if self.child.wait()?.success() {
            Ok(())
        } else {
            let err = self.stderr.read_to_end();
            // 在特定情况下，如果我们还没有完全消耗子进程的数据，那么上面关闭 stdout 会导致大多数情况下引发管道信号。
            // 但我认为没有可靠且可移植的方法来检测它。相反，如果我们知道尚未达到 EOF（因此我们预期会出现破损的管道错误），
            // 并且如果 stderr 否则没有任何内容，那么我们假设完全成功。
            if !self.eof && err.is_empty() {
                return Ok(());
            }
            Err(io::Error::from(err))
        }
    }
}

impl Drop for CommandReader {
    fn drop(&mut self) {
        if let Err(error) = self.close() {
            log::warn!("{}", error);
        }
    }
}

impl io::Read for CommandReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let stdout = match self.child.stdout {
            None => return Ok(0),
            Some(ref mut stdout) => stdout,
        };
        let nread = stdout.read(buf)?;
        if nread == 0 {
            self.eof = true;
            self.close().map(|_| 0)
        } else {
            Ok(nread)
        }
    }
}

/// 读取器，封装异步或同步读取 stderr。
#[derive(Debug)]
enum StderrReader {
    Async(Option<JoinHandle<CommandError>>),
    Sync(process::ChildStderr),
}

impl StderrReader {
    /// 创建一个用于异步读取 stderr 内容的读取器。
    fn r#async(mut stderr: process::ChildStderr) -> StderrReader {
        let handle =
            thread::spawn(move || stderr_to_command_error(&mut stderr));
        StderrReader::Async(Some(handle))
    }

    /// 创建一个用于同步读取 stderr 内容的读取器。
    fn sync(stderr: process::ChildStderr) -> StderrReader {
        StderrReader::Sync(stderr)
    }

    /// 将 stderr 的所有内容消耗到堆上并将其作为错误返回。
    ///
    /// 如果读取 stderr 本身出现问题，则返回 I/O 命令错误。
    fn read_to_end(&mut self) -> CommandError {
        match *self {
            StderrReader::Async(ref mut handle) => {
                let handle =
                    handle.take().expect("read_to_end 不可以调用超过一次");
                handle.join().expect("stderr 读取线程不会 panic")
            }
            StderrReader::Sync(ref mut stderr) => {
                stderr_to_command_error(stderr)
            }
        }
    }
}

fn stderr_to_command_error(stderr: &mut process::ChildStderr) -> CommandError {
    let mut bytes = vec![];
    match stderr.read_to_end(&mut bytes) {
        Ok(_) => CommandError::stderr(bytes),
        Err(err) => CommandError::io(err),
    }
}
