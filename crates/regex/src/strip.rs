use {
    grep_matcher::LineTerminator,
    regex_syntax::hir::{self, Hir, HirKind},
};

use crate::error::{Error, ErrorKind};

/// 如果可能，返回一个 HIR，该 HIR 绝对不会匹配给定的行终止符。
///
/// 如果无法进行这种转换，则返回错误。
///
/// 一般来说，如果文本中的字面行终止符出现在 HIR 中的任何位置，则会返回错误。
/// 但是，如果行终止符出现在一个字符类中，并且该字符类中至少包含一个其他字符
/// （不是行终止符），则行终止符会从该字符类中被简单地去除。
///
/// 如果给定的行终止符不是 ASCII，此函数将返回错误。
///
/// 请注意，截至 regex 1.9 版本，理论上可以在不返回错误的情况下实现此过程。
/// 例如，我们可以将 `foo\nbar` 转换为 `foo[a&&b]bar`。也就是说，将行终止符替换为一个永远不会匹配的子表达式。
/// 因此，ripgrep 将接受这种正则表达式，但不会匹配任何内容。在 1.8 版本之前的正则表达式版本不支持这样的构造。
/// 我最终决定保留返回错误的现有行为。例如：
///
/// ```text
/// $ echo -n 'foo\nbar\n' | rg 'foo\nbar'
/// the literal '"\n"' is not allowed in a regex
///
/// Consider enabling multiline mode with the --multiline flag (or -U for short).
/// When multiline mode is enabled, new line characters can be matched.
/// ```
///
/// 我认为这是一个很好的错误信息，甚至提供了用户可以使用的标志。
pub(crate) fn strip_from_match(
    expr: Hir,
    line_term: LineTerminator,
) -> Result<Hir, Error> {
    if line_term.is_crlf() {
        let expr1 = strip_from_match_ascii(expr, b'\r')?;
        strip_from_match_ascii(expr1, b'\n')
    } else {
        strip_from_match_ascii(expr, line_term.as_byte())
    }
}

/// strip_from_match 的实现。给定的字节必须是 ASCII。
/// 否则，此函数将返回错误。如果无法在不留下空字符类的情况下从给定的正则表达式中删除 `\n`，也会返回错误。
fn strip_from_match_ascii(expr: Hir, byte: u8) -> Result<Hir, Error> {
    if !byte.is_ascii() {
        return Err(Error::new(ErrorKind::InvalidLineTerminator(byte)));
    }
    let ch = char::from(byte);
    let invalid = || Err(Error::new(ErrorKind::NotAllowed(ch.to_string())));
    Ok(match expr.into_kind() {
        HirKind::Empty => Hir::empty(),
        HirKind::Literal(hir::Literal(lit)) => {
            if lit.iter().find(|&&b| b == byte).is_some() {
                return invalid();
            }
            Hir::literal(lit)
        }
        HirKind::Class(hir::Class::Unicode(mut cls)) => {
            if cls.ranges().is_empty() {
                return Ok(Hir::class(hir::Class::Unicode(cls)));
            }
            let remove = hir::ClassUnicode::new(Some(
                hir::ClassUnicodeRange::new(ch, ch),
            ));
            cls.difference(&remove);
            if cls.ranges().is_empty() {
                return invalid();
            }
            Hir::class(hir::Class::Unicode(cls))
        }
        HirKind::Class(hir::Class::Bytes(mut cls)) => {
            if cls.ranges().is_empty() {
                return Ok(Hir::class(hir::Class::Bytes(cls)));
            }
            let remove = hir::ClassBytes::new(Some(
                hir::ClassBytesRange::new(byte, byte),
            ));
            cls.difference(&remove);
            if cls.ranges().is_empty() {
                return invalid();
            }
            Hir::class(hir::Class::Bytes(cls))
        }
        HirKind::Look(x) => Hir::look(x),
        HirKind::Repetition(mut x) => {
            x.sub = Box::new(strip_from_match_ascii(*x.sub, byte)?);
            Hir::repetition(x)
        }
        HirKind::Capture(mut x) => {
            x.sub = Box::new(strip_from_match_ascii(*x.sub, byte)?);
            Hir::capture(x)
        }
        HirKind::Concat(xs) => {
            let xs = xs
                .into_iter()
                .map(|e| strip_from_match_ascii(e, byte))
                .collect::<Result<Vec<Hir>, Error>>()?;
            Hir::concat(xs)
        }
        HirKind::Alternation(xs) => {
            let xs = xs
                .into_iter()
                .map(|e| strip_from_match_ascii(e, byte))
                .collect::<Result<Vec<Hir>, Error>>()?;
            Hir::alternation(xs)
        }
    })
}

#[cfg(test)]
mod tests {
    use regex_syntax::Parser;

    use super::{strip_from_match, LineTerminator};
    use crate::error::Error;

    fn roundtrip(pattern: &str, byte: u8) -> String {
        roundtrip_line_term(pattern, LineTerminator::byte(byte)).unwrap()
    }

    fn roundtrip_crlf(pattern: &str) -> String {
        roundtrip_line_term(pattern, LineTerminator::crlf()).unwrap()
    }

    fn roundtrip_err(pattern: &str, byte: u8) -> Result<String, Error> {
        roundtrip_line_term(pattern, LineTerminator::byte(byte))
    }

    fn roundtrip_line_term(
        pattern: &str,
        line_term: LineTerminator,
    ) -> Result<String, Error> {
        let expr1 = Parser::new().parse(pattern).unwrap();
        let expr2 = strip_from_match(expr1, line_term)?;
        Ok(expr2.to_string())
    }

    #[test]
    fn various() {
        assert_eq!(roundtrip(r"[a\n]", b'\n'), "a");
        assert_eq!(roundtrip(r"[a\n]", b'a'), "\n");
        assert_eq!(roundtrip_crlf(r"[a\n]"), "a");
        assert_eq!(roundtrip_crlf(r"[a\r]"), "a");
        assert_eq!(roundtrip_crlf(r"[a\r\n]"), "a");

        assert_eq!(roundtrip(r"(?-u)\s", b'a'), r"(?-u:[\x09-\x0D\x20])");
        assert_eq!(roundtrip(r"(?-u)\s", b'\n'), r"(?-u:[\x09\x0B-\x0D\x20])");

        assert!(roundtrip_err(r"\n", b'\n').is_err());
        assert!(roundtrip_err(r"abc\n", b'\n').is_err());
        assert!(roundtrip_err(r"\nabc", b'\n').is_err());
        assert!(roundtrip_err(r"abc\nxyz", b'\n').is_err());
        assert!(roundtrip_err(r"\x0A", b'\n').is_err());
        assert!(roundtrip_err(r"\u000A", b'\n').is_err());
        assert!(roundtrip_err(r"\U0000000A", b'\n').is_err());
        assert!(roundtrip_err(r"\u{A}", b'\n').is_err());
        assert!(roundtrip_err("\n", b'\n').is_err());
    }
}
