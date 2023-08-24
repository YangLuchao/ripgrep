use std::io::{self, Write};

use termcolor::{ColorSpec, WriteColor};

/// 一个写入器，用于统计写入成功的字节数。
#[derive(Clone, Debug)]
pub struct CounterWriter<W> {
    wtr: W,
    count: u64,
    total_count: u64,
}

impl<W: Write> CounterWriter<W> {
    pub fn new(wtr: W) -> CounterWriter<W> {
        CounterWriter { wtr: wtr, count: 0, total_count: 0 }
    }
}

impl<W> CounterWriter<W> {
    /// 返回自创建以来或上次调用 `reset` 以来写入的总字节数。
    pub fn count(&self) -> u64 {
        self.count
    }

    /// 返回自创建以来写入的总字节数。
    pub fn total_count(&self) -> u64 {
        self.total_count + self.count
    }

    /// 将写入的字节数重置为 `0`。
    pub fn reset_count(&mut self) {
        self.total_count += self.count;
        self.count = 0;
    }

    /// clear 方法重置该写入器的所有计数相关状态。
    ///
    /// 在此调用之后，写入到底层写入器的总字节数将被擦除并重置。
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.count = 0;
        self.total_count = 0;
    }

    #[allow(dead_code)]
    pub fn get_ref(&self) -> &W {
        &self.wtr
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.wtr
    }

    pub fn into_inner(self) -> W {
        self.wtr
    }
}

impl<W: Write> Write for CounterWriter<W> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        let n = self.wtr.write(buf)?;
        self.count += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        self.wtr.flush()
    }
}

impl<W: WriteColor> WriteColor for CounterWriter<W> {
    fn supports_color(&self) -> bool {
        self.wtr.supports_color()
    }

    fn set_color(&mut self, spec: &ColorSpec) -> io::Result<()> {
        self.wtr.set_color(spec)
    }

    fn reset(&mut self) -> io::Result<()> {
        self.wtr.reset()
    }

    fn is_synchronous(&self) -> bool {
        self.wtr.is_synchronous()
    }
}
