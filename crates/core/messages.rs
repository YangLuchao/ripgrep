use std::sync::atomic::{AtomicBool, Ordering};

static MESSAGES: AtomicBool = AtomicBool::new(false);
static IGNORE_MESSAGES: AtomicBool = AtomicBool::new(false);
static ERRORED: AtomicBool = AtomicBool::new(false);

/// 类似于 eprintln，但锁定 STDOUT 以防止行交错。
#[macro_export]
macro_rules! eprintln_locked {
    ($($tt:tt)*) => {{
        {
            // 这有点违反了抽象，因为在打印到 STDERR 之前，我们显式地锁定了 STDOUT。
            // 这避免了在 ripgrep 中插入行，因为 `search_parallel` 使用 `termcolor`，
            // 当写入行时会访问相同的 STDOUT 锁。
            let stdout = std::io::stdout();
            let _handle = stdout.lock();
            eprintln!($($tt)*);
        }
    }}
}

/// 发出非致命错误消息，除非禁用了消息。
#[macro_export]
macro_rules! message {
    ($($tt:tt)*) => {
        if crate::messages::messages() {
            eprintln_locked!($($tt)*);
        }
    }
}

/// 类似于 message，但设置了 ripgrep 的 "errored" 标志，该标志控制退出状态。
#[macro_export]
macro_rules! err_message {
    ($($tt:tt)*) => {
        crate::messages::set_errored();
        message!($($tt)*);
    }
}

/// 发出与忽略相关的非致命错误消息（如解析错误），除非禁用了 ignore-messages。
#[macro_export]
macro_rules! ignore_message {
    ($($tt:tt)*) => {
        if crate::messages::messages() && crate::messages::ignore_messages() {
            eprintln_locked!($($tt)*);
        }
    }
}

/// 仅当消息需要显示时返回 true。
pub fn messages() -> bool {
    MESSAGES.load(Ordering::SeqCst)
}

/// 设置是否应显示消息。
///
/// 默认情况下，它们不会被显示。
pub fn set_messages(yes: bool) {
    MESSAGES.store(yes, Ordering::SeqCst)
}

/// 仅当需要显示与“忽略”相关的消息时返回 true。
pub fn ignore_messages() -> bool {
    IGNORE_MESSAGES.load(Ordering::SeqCst)
}

/// 设置是否应显示与“忽略”相关的消息。
///
/// 默认情况下，它们不会被显示。
///
/// 请注意，如果禁用了 `messages`，则此设置将被覆盖。换句话说，如果禁用了 `messages`，
/// 则不会显示“忽略”消息，无论此设置如何。
pub fn set_ignore_messages(yes: bool) {
    IGNORE_MESSAGES.store(yes, Ordering::SeqCst)
}

/// 仅当 ripgrep 遇到非致命错误时返回 true。
pub fn errored() -> bool {
    ERRORED.load(Ordering::SeqCst)
}

/// 表明 ripgrep 遇到了非致命错误。
pub fn set_errored() {
    ERRORED.store(true, Ordering::SeqCst);
}
