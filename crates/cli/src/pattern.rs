use std::error;
use std::ffi::OsStr;
use std::fmt;
use std::fs::File;
use std::io;
use std::path::Path;
use std::str;

use bstr::io::BufReadExt;

use crate::escape::{escape, escape_os};

/// 在将模式转换为有效的UTF-8时出错的错误类型。
///
/// 此错误的目的是为了提供更有针对性的故障模式，用于描述无效的UTF-8模式。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidPatternError {
    original: String,
    valid_up_to: usize,
}

impl InvalidPatternError {
    /// 返回在给定字符串中验证有效UTF-8的索引。
    pub fn valid_up_to(&self) -> usize {
        self.valid_up_to
    }
}

impl error::Error for InvalidPatternError {
    fn description(&self) -> &str {
        "无效的模式"
    }
}

impl fmt::Display for InvalidPatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "在模式的字节偏移量 {} 处找到无效的UTF-8：{} \
             （禁用Unicode模式并使用十六进制转义序列来匹配模式中的任意字节，例如 '(?-u)\\xFF'）",
            self.valid_up_to, self.original,
        )
    }
}

impl From<InvalidPatternError> for io::Error {
    fn from(paterr: InvalidPatternError) -> io::Error {
        io::Error::new(io::ErrorKind::Other, paterr)
    }
}

/// 将OS字符串转换为正则表达式模式。
///
/// 如果给定的模式无效的UTF-8，转换会失败，此时会返回一个带有更多关于无效UTF-8位置信息的有针对性的错误。
/// 错误还会建议使用十六进制转义序列，这在许多正则表达式引擎中都支持。
pub fn pattern_from_os(pattern: &OsStr) -> Result<&str, InvalidPatternError> {
    pattern.to_str().ok_or_else(|| {
        let valid_up_to = pattern
            .to_string_lossy()
            .find('\u{FFFD}')
            .expect("无效UTF-8的Unicode替换码点");
        InvalidPatternError { original: escape_os(pattern), valid_up_to }
    })
}

/// 将任意字节转换为正则表达式模式。
///
/// 如果给定的模式无效的UTF-8，转换会失败，此时会返回一个带有更多关于无效UTF-8位置信息的有针对性的错误。
/// 错误还会建议使用十六进制转义序列，这在许多正则表达式引擎中都支持。
pub fn pattern_from_bytes(
    pattern: &[u8],
) -> Result<&str, InvalidPatternError> {
    str::from_utf8(pattern).map_err(|err| InvalidPatternError {
        original: escape(pattern),
        valid_up_to: err.valid_up_to(),
    })
}

/// 从文件路径读取模式，每行一个。
///
/// 如果读取出现问题，或者任何模式包含无效UTF-8，都会返回一个错误。
/// 如果特定模式出现问题，错误消息将包括行号和文件路径。
pub fn patterns_from_path<P: AsRef<Path>>(path: P) -> io::Result<Vec<String>> {
    let path = path.as_ref();
    let file = File::open(path).map_err(|err| {
        io::Error::new(
            io::ErrorKind::Other,
            format!("{}: {}", path.display(), err),
        )
    })?;
    patterns_from_reader(file).map_err(|err| {
        io::Error::new(
            io::ErrorKind::Other,
            format!("{}:{}", path.display(), err),
        )
    })
}

/// 从标准输入读取模式，每行一个。
///
/// 如果读取出现问题，或者任何模式包含无效UTF-8，都会返回一个错误。
/// 如果特定模式出现问题，错误消息将包括行号和来自stdin的事实。
pub fn patterns_from_stdin() -> io::Result<Vec<String>> {
    let stdin = io::stdin();
    let locked = stdin.lock();
    patterns_from_reader(locked).map_err(|err| {
        io::Error::new(io::ErrorKind::Other, format!("<stdin>:{}", err))
    })
}

/// 从任何读取器读取模式，每行一个。
///
/// 如果读取出现问题，或者任何模式包含无效UTF-8，都会返回一个错误。
/// 如果特定模式出现问题，错误消息将包括行号。
///
/// 请注意，此函数使用自己的内部缓冲区，因此调用者不应该提供自己的缓冲读取器（如果可能的话）。
///
/// # 示例
///
/// 下面演示了如何解析每行一个的模式。
///
/// ```
/// use grep_cli::patterns_from_reader;
///
/// # fn example() -> Result<(), Box<::std::error::Error>> {
/// let patterns = "\
/// foo
/// bar\\s+foo
/// [a-z]{3}
/// ";
///
/// assert_eq!(patterns_from_reader(patterns.as_bytes())?, vec![
///     r"foo",
///     r"bar\s+foo",
///     r"[a-z]{3}",
/// ]);
/// # Ok(()) }
/// ```
pub fn patterns_from_reader<R: io::Read>(rdr: R) -> io::Result<Vec<String>> {
    let mut patterns = vec![];
    let mut line_number = 0;
    io::BufReader::new(rdr).for_byte_line(|line| {
        line_number += 1;
        match pattern_from_bytes(line) {
            Ok(pattern) => {
                patterns.push(pattern.to_string());
                Ok(true)
            }
            Err(err) => Err(io::Error::new(
                io::ErrorKind::Other,
                format!("{}: {}", line_number, err),
            )),
        }
    })?;
    Ok(patterns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes() {
        let pat = b"abc\xFFxyz";
        let err = pattern_from_bytes(pat).unwrap_err();
        assert_eq!(3, err.valid_up_to());
    }

    #[test]
    #[cfg(unix)]
    fn os() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let pat = OsStr::from_bytes(b"abc\xFFxyz");
        let err = pattern_from_os(pat).unwrap_err();
        assert_eq!(3, err.valid_up_to());
    }
}
