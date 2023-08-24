/// 在这个crate中可能发生的错误。
///
/// 通常，这个错误对应于构建正则表达式时出现的问题，无论是在解析、编译还是配置优化方面出现的问题。
#[derive(Clone, Debug)]
pub struct Error {
    kind: ErrorKind,
}

impl Error {
    /// 创建一个具有给定错误种类的新错误实例。
    pub(crate) fn new(kind: ErrorKind) -> Error {
        Error { kind }
    }

    /// 将`regex_automata::meta::BuildError`转换为错误。
    pub(crate) fn regex(err: regex_automata::meta::BuildError) -> Error {
        if let Some(size_limit) = err.size_limit() {
            let kind = ErrorKind::Regex(format!(
                "编译的正则表达式超过了大小限制 {size_limit}",
            ));
            Error { kind }
        } else if let Some(ref err) = err.syntax_error() {
            Error::generic(err)
        } else {
            Error::generic(err)
        }
    }

    /// 将通用的错误转换为错误。
    pub(crate) fn generic<E: std::error::Error>(err: E) -> Error {
        Error { kind: ErrorKind::Regex(err.to_string()) }
    }

    /// 将任意类型的消息转换为错误。
    pub(crate) fn any<E: ToString>(msg: E) -> Error {
        Error { kind: ErrorKind::Regex(msg.to_string()) }
    }

    /// 返回此错误的种类。
    pub fn kind(&self) -> &ErrorKind {
        &self.kind
    }
}

/// 可能发生的错误种类。
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ErrorKind {
    /// 解析正则表达式时出现的错误。这可以是语法错误，也可以是尝试编译过大的正则表达式而导致的错误。
    ///
    /// 这里的字符串是底层错误转换为字符串的结果。
    Regex(String),
    /// 构建不允许匹配行终止符的正则表达式时出现的错误。
    /// 通常情况下，构建正则表达式会尽最大努力使匹配行终止符变得不可能（例如，通过从`\s`字符类中删除`\n`），
    /// 但是如果正则表达式包含`\n`文字，则无法做出合理的选择，因此会报告错误。
    ///
    /// 字符串是在正则表达式中找到的不允许的字面序列。
    NotAllowed(String),
    /// 当提供了非ASCII行终止符时出现的错误。
    ///
    /// 无效的字节包含在此错误中。
    InvalidLineTerminator(u8),
}

impl std::error::Error for Error {}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        use bstr::ByteSlice;

        match self.kind {
            ErrorKind::Regex(ref s) => write!(f, "{}", s),
            ErrorKind::NotAllowed(ref lit) => {
                write!(f, "在正则表达式中不允许使用字面值 {:?} ", lit)
            }
            ErrorKind::InvalidLineTerminator(byte) => {
                write!(
                    f,
                    "行终止符必须是ASCII字符，但 {} 不是",
                    [byte].as_bstr()
                )
            }
        }
    }
}
