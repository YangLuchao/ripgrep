use std::ffi::OsStr;
use std::str;

use bstr::{ByteSlice, ByteVec};

/// `unescape` 函数使用的状态机中的单个状态。
#[derive(Clone, Copy, Eq, PartialEq)]
enum State {
    /// 在看到 `\` 后的状态。
    Escape,
    /// 在看到 `\x` 后的状态。
    HexFirst,
    /// 在看到 `\x[0-9A-Fa-f]` 后的状态。
    HexSecond(char),
    /// 默认状态。
    Literal,
}

/// 将任意字节转义为可读的字符串。
///
/// 这将把 `\t`、`\r` 和 `\n` 转换为其转义形式。它还将转换非可打印的 ASCII 字符的子集以及无效的 UTF-8 字节为十六进制转义序列。
/// 其他一切保持不变。
///
/// 这个函数的对应函数是 [`unescape`](fn.unescape.html)。
///
/// # 示例
///
/// 以下示例演示了如何将包含 `\n` 和无效 UTF-8 字节的字节字符串转换为 `String`。
///
/// 特别注意使用了原始字符串。即，`r"\n"` 等同于 `"\\n"`。
///
/// ```
/// use grep_cli::escape;
///
/// assert_eq!(r"foo\nbar\xFFbaz", escape(b"foo\nbar\xFFbaz"));
/// ```
pub fn escape(bytes: &[u8]) -> String {
    let mut escaped = String::new();
    for (s, e, ch) in bytes.char_indices() {
        if ch == '\u{FFFD}' {
            for b in bytes[s..e].bytes() {
                escape_byte(b, &mut escaped);
            }
        } else {
            escape_char(ch, &mut escaped);
        }
    }
    escaped
}

/// 将 OS 字符串转义为可读的字符串。
///
/// 这类似于 [`escape`](fn.escape.html)，但接受一个 OS 字符串。
pub fn escape_os(string: &OsStr) -> String {
    escape(Vec::from_os_str_lossy(string).as_bytes())
}

/// 反转义字符串。
///
/// 它支持有限的转义序列：
///
/// * `\t`、`\r` 和 `\n` 映射到相应的 ASCII 字节。
/// * `\xZZ` 十六进制转义映射到它们的字节。
///
/// 其他一切保持不变，包括非十六进制转义，如 `\xGG`。
///
/// 这在需要一个命令行参数能够指定任意字节，或者以其他方式更容易指定不可打印字符时非常有用。
///
/// 这个函数的对应函数是 [`escape`](fn.escape.html)。
///
/// # 示例
///
/// 以下示例演示了如何将一个已转义的字符串（它是有效的 UTF-8）转换为相应的字节序列。
/// 每个转义序列映射为它的字节，这可能包括无效的 UTF-8。
///
/// 特别注意使用了原始字符串。即，`r"\n"` 等同于 `"\\n"`。
///
/// ```
/// use grep_cli::unescape;
///
/// assert_eq!(&b"foo\nbar\xFFbaz"[..], &*unescape(r"foo\nbar\xFFbaz"));
/// ```
pub fn unescape(s: &str) -> Vec<u8> {
    use self::State::*;

    let mut bytes = vec![];
    let mut state = Literal;
    for c in s.chars() {
        match state {
            Escape => match c {
                '\\' => {
                    bytes.push(b'\\');
                    state = Literal;
                }
                'n' => {
                    bytes.push(b'\n');
                    state = Literal;
                }
                'r' => {
                    bytes.push(b'\r');
                    state = Literal;
                }
                't' => {
                    bytes.push(b'\t');
                    state = Literal;
                }
                'x' => {
                    state = HexFirst;
                }
                c => {
                    bytes.extend(format!(r"\{}", c).into_bytes());
                    state = Literal;
                }
            },
            HexFirst => match c {
                '0'..='9' | 'A'..='F' | 'a'..='f' => {
                    state = HexSecond(c);
                }
                c => {
                    bytes.extend(format!(r"\x{}", c).into_bytes());
                    state = Literal;
                }
            },
            HexSecond(first) => match c {
                '0'..='9' | 'A'..='F' | 'a'..='f' => {
                    let ordinal = format!("{}{}", first, c);
                    let byte = u8::from_str_radix(&ordinal, 16).unwrap();
                    bytes.push(byte);
                    state = Literal;
                }
                c => {
                    let original = format!(r"\x{}{}", first, c);
                    bytes.extend(original.into_bytes());
                    state = Literal;
                }
            },
            Literal => match c {
                '\\' => {
                    state = Escape;
                }
                c => {
                    bytes.extend(c.to_string().as_bytes());
                }
            },
        }
    }
    match state {
        Escape => bytes.push(b'\\'),
        HexFirst => bytes.extend(b"\\x"),
        HexSecond(c) => bytes.extend(format!("\\x{}", c).into_bytes()),
        Literal => {}
    }
    bytes
}

/// 将 OS 字符串反转义为字节。
///
/// 这类似于 [`unescape`](fn.unescape.html)，但接受一个 OS 字符串。
///
/// 请注意，首先将给定的 OS 字符串以 UTF-8 格式进行了丢失解码。
/// 也就是说，一个已转义的字符串（给定的内容）应该是有效的 UTF-8。
pub fn unescape_os(string: &OsStr) -> Vec<u8> {
    unescape(&string.to_string_lossy())
}

/// 将给定的码点添加到给定的字符串中，必要时进行转义。
fn escape_char(cp: char, into: &mut String) {
    if cp.is_ascii() {
        escape_byte(cp as u8, into);
    } else {
        into.push(cp);
    }
}

/// 将给定的字节添加到给定的字符串中，必要时进行转义。
fn escape_byte(byte: u8, into: &mut String) {
    match byte {
        0x21..=0x5B | 0x5D..=0x7D => into.push(byte as char),
        b'\n' => into.push_str(r"\n"),
        b'\r' => into.push_str(r"\r"),
        b'\t' => into.push_str(r"\t"),
        b'\\' => into.push_str(r"\\"),
        _ => into.push_str(&format!(r"\x{:02X}", byte)),
    }
}

#[cfg(test)]
mod tests {
    use super::{escape, unescape};

    fn b(bytes: &'static [u8]) -> Vec<u8> {
        bytes.to_vec()
    }

    #[test]
    fn empty() {
        assert_eq!(b(b""), unescape(r""));
        assert_eq!(r"", escape(b""));
    }

    #[test]
    fn backslash() {
        assert_eq!(b(b"\\"), unescape(r"\\"));
        assert_eq!(r"\\", escape(b"\\"));
    }

    #[test]
    fn nul() {
        assert_eq!(b(b"\x00"), unescape(r"\x00"));
        assert_eq!(r"\x00", escape(b"\x00"));
    }

    #[test]
    fn nl() {
        assert_eq!(b(b"\n"), unescape(r"\n"));
        assert_eq!(r"\n", escape(b"\n"));
    }

    #[test]
    fn tab() {
        assert_eq!(b(b"\t"), unescape(r"\t"));
        assert_eq!(r"\t", escape(b"\t"));
    }

    #[test]
    fn carriage() {
        assert_eq!(b(b"\r"), unescape(r"\r"));
        assert_eq!(r"\r", escape(b"\r"));
    }

    #[test]
    fn nothing_simple() {
        assert_eq!(b(b"\\a"), unescape(r"\a"));
        assert_eq!(b(b"\\a"), unescape(r"\\a"));
        assert_eq!(r"\\a", escape(b"\\a"));
    }

    #[test]
    fn nothing_hex0() {
        assert_eq!(b(b"\\x"), unescape(r"\x"));
        assert_eq!(b(b"\\x"), unescape(r"\\x"));
        assert_eq!(r"\\x", escape(b"\\x"));
    }

    #[test]
    fn nothing_hex1() {
        assert_eq!(b(b"\\xz"), unescape(r"\xz"));
        assert_eq!(b(b"\\xz"), unescape(r"\\xz"));
        assert_eq!(r"\\xz", escape(b"\\xz"));
    }

    #[test]
    fn nothing_hex2() {
        assert_eq!(b(b"\\xzz"), unescape(r"\xzz"));
        assert_eq!(b(b"\\xzz"), unescape(r"\\xzz"));
        assert_eq!(r"\\xzz", escape(b"\\xzz"));
    }

    #[test]
    fn invalid_utf8() {
        assert_eq!(r"\xFF", escape(b"\xFF"));
        assert_eq!(r"a\xFFb", escape(b"a\xFFb"));
    }
}
