/*!
该 crate 提供了在命令行应用程序中使用的常见例程，重点关注于对于搜索导向的应用程序有用的例程。作为一个实用工具库，没有中心类型或函数。然而，这个 crate 的关键重点是改进失败模式，并在出现问题时提供用户友好的错误消息。

在最大程度上，这个 crate 中的所有内容都适用于 Windows、macOS 和 Linux。

# 标准 I/O

[`is_readable_stdin`](fn.is_readable_stdin.html)、
[`is_tty_stderr`](fn.is_tty_stderr.html)、
[`is_tty_stdin`](fn.is_tty_stdin.html) 和
[`is_tty_stdout`](fn.is_tty_stdout.html)
这些例程查询了标准 I/O 的各个方面。`is_readable_stdin` 确定是否可以有效地从 stdin 读取数据，而 `tty` 方法确定 stdin/stdout/stderr 是否连接到 tty。

当编写一个根据应用程序是如何通过 stdin 读取数据来改变行为的应用程序时，`is_readable_stdin` 非常有用。例如，`rg foo` 可能会递归搜索当前工作目录中是否有 `foo` 的出现，但 `rg foo < file` 只搜索 `file` 的内容。

`tty` 方法出于类似的原因也是有用的。也就是说，像 `ls` 这样的命令会根据它们是否打印到终端来改变输出。例如，当将 stdout 重定向到文件或管道时，`ls` 每行显示一个文件，但当 stdout 连接到 tty 时，它会将输出压缩以显示可能有很多文件的一行。

# 着色和缓冲

[`stdout`](fn.stdout.html)、
[`stdout_buffered_block`](fn.stdout_buffered_block.html) 和
[`stdout_buffered_line`](fn.stdout_buffered_line.html)
这些例程是 [`StandardStream`](struct.StandardStream.html) 的替代构造函数。`StandardStream` 实现了 `termcolor::WriteColor`，它提供了一种向终端发出颜色的方法。它的关键用途是封装缓冲样式。即，如果 stdout 连接到 tty，则 `stdout` 将返回一个行缓冲的 `StandardStream`，否则将返回一个块缓冲的 `StandardStream`。行缓冲对于在 tty 上使用很重要，因为它通常会减少终端用户看到输出的延迟。块缓冲在其他情况下使用，因为它更快，将 stdout 重定向到文件通常不会从行缓冲提供的减少的延迟中受益。

`stdout_buffered_block` 和 `stdout_buffered_line` 可以用于显式设置缓冲策略，无论 stdout 是否连接到 tty。

# 转义

[`escape`](fn.escape.html)、
[`escape_os`](fn.escape_os.html)、
[`unescape`](fn.unescape.html) 和
[`unescape_os`](fn.unescape_os.html)
这些例程提供了一种处理可以表示任意字节的 UTF-8 编码字符串的用户友好方法。例如，您可能希望将包含任意字节的字符串作为命令行参数进行接受，但大多数交互式 shell 使得输入这样的字符串变得困难。相反，我们可以要求用户使用转义序列。

例如，`a\xFFz` 本身是一个有效的 UTF-8 字符串，对应以下字节：

```ignore
[b'a', b'\\', b'x', b'F', b'F', b'z']
```

但是，我们可以使用 `unescape`/`unescape_os` 例程将 `\xFF` 解释为一个转义序列，这将产生

```ignore
[b'a', b'\xFF', b'z']
```

例如：

```rust
use grep_cli::unescape;

// 注意使用原始字符串！
assert_eq!(vec![b'a', b'\xFF', b'z'], unescape(r"a\xFFz"));
```

`escape`/`escape_os` 例程提供了相反的转换，这使得显示涉及任意字节的用户友好的错误消息变得容易。

# 构建模式

通常，正则表达式模式必须是有效的 UTF-8。然而，命令行参数不能保证是有效的 UTF-8。

不幸的是，标准库的从 `OsStr` 转换为 UTF-8 的函数没有提供良好的错误消息。然而，[`pattern_from_bytes`](fn.pattern_from_bytes.html) 和 [`pattern_from_os`](fn.pattern_from_os.html) 可以，包括报告第一个无效的 UTF-8 字节出现的确切位置。

此外，从文件中读取模式并报告包括行号的良好错误消息也是有用的。[`patterns_from_path`](fn.patterns_from_path.html)、[`patterns_from_reader`](fn.patterns_from_reader.html) 和 [`patterns_from_stdin`](fn.patterns_from_stdin.html) 这些例程正是这样做的。如果找到任何无效的 UTF-8 模式，那么错误将包括文件路径（如果有的话），以及第一个无效的 UTF-8 字节被观察到的行号和字节偏移量。

# 读取进程输出

有时，命令行应用程序需要执行其他进程并以流式方式读取其 stdout。[`CommandReader`](struct.CommandReader.html) 提供了这种功能，其显式目标是改进故障模式。特别是，如果进程退出并带有错误代码，则 stderr 会被读取并转换为普通的 Rust 错误以显示给最终用户。这使得底层的故障模式变得明确，并为最终用户提供了更多的信息来调试问题。

作为一个特例，[`DecompressionReader`](struct.DecompressionReader.html) 提供了一种方法，通过将文件扩展名与相应的解压缩程序（如 `gzip` 和 `xz`）进行匹配，来解压缩任意文件。这对于以一种不绑定特定压缩库的方式执行简单的解压缩是有用的。不过，这会带来一些开销，因此如果需要解压缩大量小文件，这可能不是一个适合使用的方便方法。

每个阅读器都有一个相应的构建器，用于进行额外的配置，例如是否异步地读取 stderr 以避免死锁（默认情况下启用）。

# 其他解析

[`parse_human_readable_size`](fn.parse_human_readable_size.html) 例程解析诸如 `2M` 的字符串，并将其转换为相应的字节数（在这种情况下为 `2 * 1<<20`）。如果发现无效的大小，则会创建一个很好的错误消息，通常会告诉用户如何解决问题。
*/

#![deny(missing_docs)]

mod decompress;
mod escape;
mod human;
mod pattern;
mod process;
mod wtr;

use std::io::IsTerminal;

pub use crate::decompress::{
    resolve_binary, DecompressionMatcher, DecompressionMatcherBuilder,
    DecompressionReader, DecompressionReaderBuilder,
};
pub use crate::escape::{escape, escape_os, unescape, unescape_os};
pub use crate::human::{parse_human_readable_size, ParseSizeError};
pub use crate::pattern::{
    pattern_from_bytes, pattern_from_os, patterns_from_path,
    patterns_from_reader, patterns_from_stdin, InvalidPatternError,
};
pub use crate::process::{CommandError, CommandReader, CommandReaderBuilder};
pub use crate::wtr::{
    stdout, stdout_buffered_block, stdout_buffered_line, StandardStream,
};

/// 返回 true 当且仅当 stdin 可被读取。
///
/// 当 stdin 可读时，命令行程序可以选择在 stdin 无法读取数据时改变行为。例如，`command foo` 可能会递归搜索当前工作目录中是否有 `foo` 的出现，但 `command foo < some-file` 或 `cat some-file | command foo` 可能只会搜索 stdin 中是否有 `foo` 的出现。
pub fn is_readable_stdin() -> bool {
    #[cfg(unix)]
    fn imp() -> bool {
        use same_file::Handle;
        use std::os::unix::fs::FileTypeExt;

        let ft = match Handle::stdin().and_then(|h| h.as_file().metadata()) {
            Err(_) => return false,
            Ok(md) => md.file_type(),
        };
        ft.is_file() || ft.is_fifo() || ft.is_socket()
    }

    #[cfg(windows)]
    fn imp() -> bool {
        use winapi_util as winutil;

        winutil::file::typ(winutil::HandleRef::stdin())
            .map(|t| t.is_disk() || t.is_pipe())
            .unwrap_or(false)
    }

    !is_tty_stdin() && imp()
}

/// 返回 true 当且仅当 stdin 被认为连接到 tty 或控制台。
pub fn is_tty_stdin() -> bool {
    std::io::stdin().is_terminal()
}

/// 返回 true 当且仅当 stdout 被认为连接到 tty 或控制台。
///
/// 这对于当您希望您的命令行程序根据它是直接打印到用户终端还是被重定向到其他地方时产生不同的输出时非常有用。例如，`ls` 的实现通常会在 stdout 被重定向时每行显示一个项，但在连接到 tty 时会压缩输出以显示可能有很多文件的一行。
pub fn is_tty_stdout() -> bool {
    std::io::stdout().is_terminal()
}

/// 返回 true 当且仅当 stderr 被认为连接到 tty 或控制台。
pub fn is_tty_stderr() -> bool {
    std::io::stderr().is_terminal()
}
