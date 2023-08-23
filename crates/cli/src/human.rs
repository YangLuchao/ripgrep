use std::error;
use std::fmt;
use std::io;
use std::num::ParseIntError;

use regex::Regex;

/// 解析人类可读的大小描述时出现的错误。
///
/// 此错误提供了一个用户友好的消息，描述了为什么无法解析描述以及预期的格式是什么。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseSizeError {
    original: String,
    kind: ParseSizeErrorKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ParseSizeErrorKind {
    无效的格式,
    无效的整数(ParseIntError),
    溢出,
}

impl ParseSizeError {
    fn format(original: &str) -> ParseSizeError {
        ParseSizeError {
            original: original.to_string(),
            kind: ParseSizeErrorKind::无效的格式,
        }
    }

    fn int(original: &str, err: ParseIntError) -> ParseSizeError {
        ParseSizeError {
            original: original.to_string(),
            kind: ParseSizeErrorKind::无效的整数(err),
        }
    }

    fn overflow(original: &str) -> ParseSizeError {
        ParseSizeError {
            original: original.to_string(),
            kind: ParseSizeErrorKind::溢出,
        }
    }
}

impl error::Error for ParseSizeError {
    fn description(&self) -> &str {
        "无效的大小"
    }
}

impl fmt::Display for ParseSizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use self::ParseSizeErrorKind::*;

        match self.kind {
            无效的格式 => write!(
                f,
                "大小 '{}' 的格式无效，应为一个由数字组成的序列，后面跟有可选的 'K'、'M' 或 'G' 后缀",
                self.original
            ),
            无效的整数(ref err) => write!(
                f,
                "大小 '{}' 中的整数无效：{}",
                self.original, err
            ),
            溢出 => write!(f, "大小过大：'{}'", self.original),
        }
    }
}

impl From<ParseSizeError> for io::Error {
    fn from(size_err: ParseSizeError) -> io::Error {
        io::Error::new(io::ErrorKind::Other, size_err)
    }
}

/// 解析人类可读的大小，如 `2M`，为相应的字节数。
///
/// 支持的大小后缀为 `K`（千字节）、`M`（兆字节）和 `G`（千兆字节）。
/// 如果缺少大小后缀，则将大小解释为字节。
/// 如果大小太大无法适应 `u64`，则返回错误。
///
/// 随着时间的推移，可能会添加更多的后缀。
pub fn parse_human_readable_size(size: &str) -> Result<u64, ParseSizeError> {
    lazy_static::lazy_static! {
        // 通常，我会手动解析这么简单的内容，以避免使用 regex，
        // 但是我们无论如何都会使用 regex 进行 glob 匹配，所以不妨就用它。
        static ref RE: Regex = Regex::new(r"^([0-9]+)([KMG])?$").unwrap();
    }

    let caps = match RE.captures(size) {
        Some(caps) => caps,
        None => return Err(ParseSizeError::format(size)),
    };
    let value: u64 =
        caps[1].parse().map_err(|err| ParseSizeError::int(size, err))?;
    let suffix = match caps.get(2) {
        None => return Ok(value),
        Some(cap) => cap.as_str(),
    };
    let bytes = match suffix {
        "K" => value.checked_mul(1 << 10),
        "M" => value.checked_mul(1 << 20),
        "G" => value.checked_mul(1 << 30),
        // 因为如果 regex 匹配了这个组，它必须是 [KMG]。
        _ => unreachable!(),
    };
    bytes.ok_or_else(|| ParseSizeError::overflow(size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_none() {
        let x = parse_human_readable_size("123").unwrap();
        assert_eq!(123, x);
    }

    #[test]
    fn suffix_k() {
        let x = parse_human_readable_size("123K").unwrap();
        assert_eq!(123 * (1 << 10), x);
    }

    #[test]
    fn suffix_m() {
        let x = parse_human_readable_size("123M").unwrap();
        assert_eq!(123 * (1 << 20), x);
    }

    #[test]
    fn suffix_g() {
        let x = parse_human_readable_size("123G").unwrap();
        assert_eq!(123 * (1 << 30), x);
    }

    #[test]
    fn invalid_empty() {
        assert!(parse_human_readable_size("").is_err());
    }

    #[test]
    fn invalid_non_digit() {
        assert!(parse_human_readable_size("a").is_err());
    }

    #[test]
    fn invalid_overflow() {
        assert!(parse_human_readable_size("9999999999999999G").is_err());
    }

    #[test]
    fn invalid_suffix() {
        assert!(parse_human_readable_size("123T").is_err());
    }
}
