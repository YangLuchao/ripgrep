use std::io;
use termcolor;

use crate::is_tty_stdout;

/// 支持在行缓冲或块缓冲下着色的写入器。
pub struct StandardStream(StandardStreamKind);

/// 根据给定的颜色选择返回可能带缓冲的标准输出写入器。
///
/// 返回的写入器可以是行缓冲或块缓冲。这个决定是根据是否在标准输出上连接了终端自动做出的。
/// 如果连接了终端，那么使用行缓冲。否则，使用块缓冲。通常来说，块缓冲更高效，但可能会增加用户看到输出的时间。
///
/// 如果你需要更精细的控制缓冲模式，可以使用 `stdout_buffered_line` 或 `stdout_buffered_block`。
///
/// 给定的颜色选择将传递给底层写入器。要完全禁用所有情况下的颜色，可以使用 `ColorChoice::Never`。
pub fn stdout(color_choice: termcolor::ColorChoice) -> StandardStream {
    if is_tty_stdout() {
        stdout_buffered_line(color_choice)
    } else {
        stdout_buffered_block(color_choice)
    }
}

/// 根据给定的颜色选择返回行缓冲的标准输出写入器。
///
/// 当将结果直接打印到终端时，这个写入器很有用，用户会立即看到输出。这种方法的缺点是它可能会更慢，
/// 尤其是在有大量输出时。
///
/// 你可能会考虑使用 [`stdout`](fn.stdout.html)，
/// 它会根据标准输出是否连接到终端自动选择缓冲策略。
pub fn stdout_buffered_line(
    color_choice: termcolor::ColorChoice,
) -> StandardStream {
    let out = termcolor::StandardStream::stdout(color_choice);
    StandardStream(StandardStreamKind::LineBuffered(out))
}

/// 根据给定的颜色选择返回块缓冲的标准输出写入器。
///
/// 当将结果写入文件时，这个写入器很有用，因为它摊销了写入数据的成本。这种方法的缺点是它可能会增加写入终端时的显示延迟。
///
/// 你可能会考虑使用 [`stdout`](fn.stdout.html)，
/// 它会根据标准输出是否连接到终端自动选择缓冲策略。
pub fn stdout_buffered_block(
    color_choice: termcolor::ColorChoice,
) -> StandardStream {
    let out = termcolor::BufferedStandardStream::stdout(color_choice);
    StandardStream(StandardStreamKind::BlockBuffered(out))
}

enum StandardStreamKind {
    LineBuffered(termcolor::StandardStream),
    BlockBuffered(termcolor::BufferedStandardStream),
}

impl io::Write for StandardStream {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        use self::StandardStreamKind::*;

        match self.0 {
            LineBuffered(ref mut w) => w.write(buf),
            BlockBuffered(ref mut w) => w.write(buf),
        }
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        use self::StandardStreamKind::*;

        match self.0 {
            LineBuffered(ref mut w) => w.flush(),
            BlockBuffered(ref mut w) => w.flush(),
        }
    }
}

impl termcolor::WriteColor for StandardStream {
    #[inline]
    fn supports_color(&self) -> bool {
        use self::StandardStreamKind::*;

        match self.0 {
            LineBuffered(ref w) => w.supports_color(),
            BlockBuffered(ref w) => w.supports_color(),
        }
    }

    #[inline]
    fn set_color(&mut self, spec: &termcolor::ColorSpec) -> io::Result<()> {
        use self::StandardStreamKind::*;

        match self.0 {
            LineBuffered(ref mut w) => w.set_color(spec),
            BlockBuffered(ref mut w) => w.set_color(spec),
        }
    }

    #[inline]
    fn reset(&mut self) -> io::Result<()> {
        use self::StandardStreamKind::*;

        match self.0 {
            LineBuffered(ref mut w) => w.reset(),
            BlockBuffered(ref mut w) => w.reset(),
        }
    }

    #[inline]
    fn is_synchronous(&self) -> bool {
        use self::StandardStreamKind::*;

        match self.0 {
            LineBuffered(ref w) => w.is_synchronous(),
            BlockBuffered(ref w) => w.is_synchronous(),
        }
    }
}
