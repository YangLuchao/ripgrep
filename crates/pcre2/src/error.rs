use std::error;
use std::fmt;

/// 可能在此 crate 中出现的错误。
///
/// 通常，此错误对应于构建正则表达式时可能出现的问题，
/// 无论是在解析、编译还是配置优化方面出现的问题。
#[derive(Clone, Debug)]
pub struct Error {
    kind: ErrorKind,
}

impl Error {
    pub(crate) fn regex<E: error::Error>(err: E) -> Error {
        Error { kind: ErrorKind::Regex(err.to_string()) }
    }

    /// 返回此错误的类型。
    pub fn kind(&self) -> &ErrorKind {
        &self.kind
    }
}

/// 可能发生的错误类型。
#[derive(Clone, Debug)]
pub enum ErrorKind {
    /// 由于解析正则表达式而引发的错误。
    /// 这可能是语法错误，也可能是尝试编译过大的正则表达式而引发的错误。
    ///
    /// 此处的字符串是将底层错误转换为字符串。
    Regex(String),
    /// 提示不应该详尽地解构此类型。
    ///
    /// 此枚举可能会增加其他变体，因此这会确保客户端不依赖于详尽匹配。
    /// （否则，添加新变体可能会破坏现有代码。）
    #[doc(hidden)]
    __Nonexhaustive,
}

impl error::Error for Error {
    fn description(&self) -> &str {
        match self.kind {
            ErrorKind::Regex(_) => "正则表达式错误",
            ErrorKind::__Nonexhaustive => unreachable!(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ErrorKind::Regex(ref s) => write!(f, "{}", s),
            ErrorKind::__Nonexhaustive => unreachable!(),
        }
    }
}
