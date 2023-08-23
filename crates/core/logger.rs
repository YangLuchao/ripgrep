// 该模块定义了一个与 `log` 库配合使用的超级简单的记录器。
// 我们不需要任何花哨的东西；只需要基本的日志级别和将日志输出到标准错误流的功能。
// 因此，我们避免了为此功能额外引入依赖。

use log::{self, Log};

/// 最简单的日志记录器，将日志输出到标准错误流。
///
/// 此记录器不进行过滤。相反，它依赖于 `log` 库通过其全局的 max_level 设置进行过滤。
#[derive(Debug)]
pub struct Logger(());

const LOGGER: &'static Logger = &Logger(());

impl Logger {
    /// 创建一个新的日志记录器，将日志输出到标准错误流，并将其初始化为全局日志记录器。
    /// 如果设置日志记录器时出现问题，则返回错误。
    pub fn init() -> Result<(), log::SetLoggerError> {
        log::set_logger(LOGGER)
    }
}

impl Log for Logger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        // 我们通过 log::set_max_level 设置日志级别，因此不需要在这里实现过滤。
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        match (record.file(), record.line()) {
            (Some(file), Some(line)) => {
                eprintln_locked!(
                    "{}|{}|{}:{}: {}",
                    record.level(),
                    record.target(),
                    file,
                    line,
                    record.args()
                );
            }
            (Some(file), None) => {
                eprintln_locked!(
                    "{}|{}|{}: {}",
                    record.level(),
                    record.target(),
                    file,
                    record.args()
                );
            }
            _ => {
                eprintln_locked!(
                    "{}|{}: {}",
                    record.level(),
                    record.target(),
                    record.args()
                );
            }
        }
    }

    fn flush(&self) {
        // 我们使用每次调用都会刷新的 eprintln_locked!。
    }
}
