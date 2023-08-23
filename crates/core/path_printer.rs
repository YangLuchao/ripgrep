use std::io;
use std::path::Path;

use grep::printer::{ColorSpecs, PrinterPath};
use termcolor::WriteColor;

/// 描述如何写入路径的配置。
#[derive(Clone, Debug)]
struct Config {
    colors: ColorSpecs,
    separator: Option<u8>,
    terminator: u8,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            colors: ColorSpecs::default(),
            separator: None,
            terminator: b'\n',
        }
    }
}

/// 用于构建要搜索的内容的构建器。
#[derive(Clone, Debug)]
pub struct PathPrinterBuilder {
    config: Config,
}

impl PathPrinterBuilder {
    /// 使用默认配置返回一个新的主题构建器。
    pub fn new() -> PathPrinterBuilder {
        PathPrinterBuilder { config: Config::default() }
    }

    /// 使用当前配置创建一个新的路径打印机，将路径写入给定的写入器。
    pub fn build<W: WriteColor>(&self, wtr: W) -> PathPrinter<W> {
        PathPrinter { config: self.config.clone(), wtr }
    }

    /// 设置此打印机的颜色规范。
    ///
    /// 目前，仅使用给定规范的 `path` 组件。
    pub fn color_specs(
        &mut self,
        specs: ColorSpecs,
    ) -> &mut PathPrinterBuilder {
        self.config.colors = specs;
        self
    }

    /// 路径分隔符。
    ///
    /// 当提供时，将使用给定分隔符替换路径的默认分隔符。
    ///
    /// 默认情况下，不设置此项，将使用系统的默认路径分隔符。
    pub fn separator(&mut self, sep: Option<u8>) -> &mut PathPrinterBuilder {
        self.config.separator = sep;
        self
    }

    /// 路径终止符。
    ///
    /// 在打印路径时，将由给定的字节终止。
    ///
    /// 默认情况下，设置为 `\n`。
    pub fn terminator(&mut self, terminator: u8) -> &mut PathPrinterBuilder {
        self.config.terminator = terminator;
        self
    }
}

/// 用于向写入器发出路径的打印机，支持可选的颜色。
#[derive(Debug)]
pub struct PathPrinter<W> {
    config: Config,
    wtr: W,
}

impl<W: WriteColor> PathPrinter<W> {
    /// 将给定的路径写入底层写入器。
    pub fn write_path(&mut self, path: &Path) -> io::Result<()> {
        let ppath = PrinterPath::with_separator(path, self.config.separator);
        if !self.wtr.supports_color() {
            self.wtr.write_all(ppath.as_bytes())?;
        } else {
            self.wtr.set_color(self.config.colors.path())?;
            self.wtr.write_all(ppath.as_bytes())?;
            self.wtr.reset()?;
        }
        self.wtr.write_all(&[self.config.terminator])
    }
}
