/*!
用于执行对行进行操作的一系列例程。
*/

use bstr::ByteSlice;
use bytecount;
use grep_matcher::{LineTerminator, Match};

/// 一个在特定字节切片中迭代行的迭代器。
///
/// 行终止符被视为终止它们的行的一部分。迭代器产生的所有行都保证是非空的。
///
/// `'b` 表示底层字节的生命周期。
#[derive(Debug)]
pub struct LineIter<'b> {
    bytes: &'b [u8],
    stepper: LineStep,
}

impl<'b> LineIter<'b> {
    /// 创建一个新的行迭代器，该迭代器在给定的字节中产生由 `line_term` 终止的行。
    pub fn new(line_term: u8, bytes: &'b [u8]) -> LineIter<'b> {
        LineIter {
            bytes: bytes,
            stepper: LineStep::new(line_term, 0, bytes.len()),
        }
    }
}

impl<'b> Iterator for LineIter<'b> {
    type Item = &'b [u8];

    fn next(&mut self) -> Option<&'b [u8]> {
        self.stepper.next_match(self.bytes).map(|m| &self.bytes[m])
    }
}

/// 一个显式的迭代器，在特定字节切片中迭代行。
///
/// 这个迭代器避免了直接借用字节本身，而是要求在通过迭代器移动时显式地提供字节。
/// 虽然不太符合惯例，但这提供了一种在迭代行时不需要借用切片本身的简单方法，这可能很方便。
///
/// 行终止符被视为终止它们的行的一部分。迭代器产生的所有行都保证是非空的。
#[derive(Debug)]
pub struct LineStep {
    line_term: u8,
    pos: usize,
    end: usize,
}

impl LineStep {
    /// 使用给定的行终止符在给定字节范围内创建一个新的行迭代器。
    ///
    /// 调用者应该为每次调用 `next` 提供实际的字节。每次调用都必须提供相同的切片。
    ///
    /// 如果 `start` 不小于或等于 `end`，则会发生 panic。
    pub fn new(line_term: u8, start: usize, end: usize) -> LineStep {
        LineStep { line_term, pos: start, end: end }
    }

    /// 返回给定字节中下一行的起始和结束位置。
    ///
    /// 调用者必须为每次调用 `next` 提供确切的字节切片。
    ///
    /// 返回的范围包括行终止符。范围始终是非空的。
    pub fn next(&mut self, bytes: &[u8]) -> Option<(usize, usize)> {
        self.next_impl(bytes)
    }

    /// 类似于 next，但返回一个 `Match` 而不是元组。
    #[inline(always)]
    pub(crate) fn next_match(&mut self, bytes: &[u8]) -> Option<Match> {
        self.next_impl(bytes).map(|(s, e)| Match::new(s, e))
    }

    #[inline(always)]
    fn next_impl(&mut self, mut bytes: &[u8]) -> Option<(usize, usize)> {
        bytes = &bytes[..self.end];
        match bytes[self.pos..].find_byte(self.line_term) {
            None => {
                if self.pos < bytes.len() {
                    let m = (self.pos, bytes.len());
                    assert!(m.0 <= m.1);

                    self.pos = m.1;
                    Some(m)
                } else {
                    None
                }
            }
            Some(line_end) => {
                let m = (self.pos, self.pos + line_end + 1);
                assert!(m.0 <= m.1);

                self.pos = m.1;
                Some(m)
            }
        }
    }
}

/// 计算 `bytes` 中 `line_term` 出现的次数。
pub fn count(bytes: &[u8], line_term: u8) -> u64 {
    bytecount::count(bytes, line_term) as u64
}

/// 给定可能以终止符结束的行，返回不包含终止符的行。
#[inline(always)]
pub fn without_terminator(bytes: &[u8], line_term: LineTerminator) -> &[u8] {
    let line_term = line_term.as_bytes();
    let start = bytes.len().saturating_sub(line_term.len());
    if bytes.get(start..) == Some(line_term) {
        return &bytes[..bytes.len() - line_term.len()];
    }
    bytes
}

/// 返回包含给定字节范围的行的起始和结束偏移量。
///
/// 行终止符被视为终止它们的行的一部分。
#[inline(always)]
pub fn locate(bytes: &[u8], line_term: u8, range: Match) -> Match {
    let line_start =
        bytes[..range.start()].rfind_byte(line_term).map_or(0, |i| i + 1);
    let line_end =
        if range.end() > line_start && bytes[range.end() - 1] == line_term {
            range.end()
        } else {
            bytes[range.end()..]
                .find_byte(line_term)
                .map_or(bytes.len(), |i| range.end() + i + 1)
        };
    Match::new(line_start, line_end)
}

/// 返回在 `bytes` 的最后一行之前 `count` 行之前可能出现的行的最小起始偏移量。
///
/// 行由 `line_term` 终止。如果 `count` 为零，则返回 `bytes` 中最后一行的起始偏移量。
///
/// 如果 `bytes` 以行终止符结束，则终止符本身被视为最后一行的一部分。
pub fn preceding(bytes: &[u8], line_term: u8, count: usize) -> usize {
    preceding_by_pos(bytes, bytes.len(), line_term, count)
}

/// 返回在包含 `pos` 的行之前 `count` 行之前可能出现的行的最小起始偏移量。
/// 行由 `line_term` 终止。如果 `

/// 如果 `count` 为零，则返回包含 `pos` 的行的起始偏移量。
///
/// 如果 `pos` 恰好指向行终止符之后，那么它被视为终止它的行的一部分。例如，给定 `bytes =
/// b"abc\nxyz\n"` 和 `pos = 7`，`preceding(bytes, pos, b'\n', 0)` 返回 `4`
///（与 `pos = 8` 一样），而 `preceding(bytes, pos, b'\n', 1)` 返回 `0`。
fn preceding_by_pos(
    bytes: &[u8],
    mut pos: usize,
    line_term: u8,
    mut count: usize,
) -> usize {
    if pos == 0 {
        return 0;
    } else if bytes[pos - 1] == line_term {
        pos -= 1;
    }
    loop {
        match bytes[..pos].rfind_byte(line_term) {
            None => {
                return 0;
            }
            Some(i) => {
                if count == 0 {
                    return i + 1;
                } else if i == 0 {
                    return 0;
                }
                count -= 1;
                pos = i;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use grep_matcher::Match;
    use std::ops::Range;
    use std::str;

    const SHERLOCK: &'static str = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
be, to a very large extent, the result of luck. Sherlock Holmes
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.\
";

    fn m(start: usize, end: usize) -> Match {
        Match::new(start, end)
    }

    fn lines(text: &str) -> Vec<&str> {
        let mut results = vec![];
        let mut it = LineStep::new(b'\n', 0, text.len());
        while let Some(m) = it.next_match(text.as_bytes()) {
            results.push(&text[m]);
        }
        results
    }

    fn line_ranges(text: &str) -> Vec<Range<usize>> {
        let mut results = vec![];
        let mut it = LineStep::new(b'\n', 0, text.len());
        while let Some(m) = it.next_match(text.as_bytes()) {
            results.push(m.start()..m.end());
        }
        results
    }

    fn prev(text: &str, pos: usize, count: usize) -> usize {
        preceding_by_pos(text.as_bytes(), pos, b'\n', count)
    }

    fn loc(text: &str, start: usize, end: usize) -> Match {
        locate(text.as_bytes(), b'\n', Match::new(start, end))
    }

    #[test]
    fn line_count() {
        assert_eq!(0, count(b"", b'\n'));
        assert_eq!(1, count(b"\n", b'\n'));
        assert_eq!(2, count(b"\n\n", b'\n'));
        assert_eq!(2, count(b"a\nb\nc", b'\n'));
    }

    #[test]
    fn line_locate() {
        let t = SHERLOCK;
        let lines = line_ranges(t);

        assert_eq!(
            loc(t, lines[0].start, lines[0].end),
            m(lines[0].start, lines[0].end)
        );
        assert_eq!(
            loc(t, lines[0].start + 1, lines[0].end),
            m(lines[0].start, lines[0].end)
        );
        assert_eq!(
            loc(t, lines[0].end - 1, lines[0].end),
            m(lines[0].start, lines[0].end)
        );
        assert_eq!(
            loc(t, lines[0].end, lines[0].end),
            m(lines[1].start, lines[1].end)
        );

        assert_eq!(
            loc(t, lines[5].start, lines[5].end),
            m(lines[5].start, lines[5].end)
        );
        assert_eq!(
            loc(t, lines[5].start + 1, lines[5].end),
            m(lines[5].start, lines[5].end)
        );
        assert_eq!(
            loc(t, lines[5].end - 1, lines[5].end),
            m(lines[5].start, lines[5].end)
        );
        assert_eq!(
            loc(t, lines[5].end, lines[5].end),
            m(lines[5].start, lines[5].end)
        );
    }

    #[test]
    fn line_locate_weird() {
        assert_eq!(loc("", 0, 0), m(0, 0));

        assert_eq!(loc("\n", 0, 1), m(0, 1));
        assert_eq!(loc("\n", 1, 1), m(1, 1));

        assert_eq!(loc("\n\n", 0, 0), m(0, 1));
        assert_eq!(loc("\n\n", 0, 1), m(0, 1));
        assert_eq!(loc("\n\n", 1, 1), m(1, 2));
        assert_eq!(loc("\n\n", 1, 2), m(1, 2));
        assert_eq!(loc("\n\n", 2, 2), m(2, 2));

        assert_eq!(loc("a\nb\nc", 0, 1), m(0, 2));
        assert_eq!(loc("a\nb\nc", 1, 2), m(0, 2));
        assert_eq!(loc("a\nb\nc", 2, 3), m(2, 4));
        assert_eq!(loc("a\nb\nc", 3, 4), m(2, 4));
        assert_eq!(loc("a\nb\nc", 4, 5), m(4, 5));
        assert_eq!(loc("a\nb\nc", 5, 5), m(4, 5));
    }

    #[test]
    fn line_iter() {
        assert_eq!(lines("abc"), vec!["abc"]);

        assert_eq!(lines("abc\n"), vec!["abc\n"]);
        assert_eq!(lines("abc\nxyz"), vec!["abc\n", "xyz"]);
        assert_eq!(lines("abc\nxyz\n"), vec!["abc\n", "xyz\n"]);

        assert_eq!(lines("abc\n\n"), vec!["abc\n", "\n"]);
        assert_eq!(lines("abc\n\n\n"), vec!["abc\n", "\n", "\n"]);
        assert_eq!(lines("abc\n\nxyz"), vec!["abc\n", "\n", "xyz"]);
        assert_eq!(lines("abc\n\nxyz\n"), vec!["abc\n", "\n", "xyz\n"]);
        assert_eq!(lines("abc\nxyz\n\n"), vec!["abc\n", "xyz\n", "\n"]);

        assert_eq!(lines("\n"), vec!["\n"]);
        assert_eq!(lines(""), Vec::<&str>::new());
    }

    #[test]
    fn line_iter_empty() {
        let mut it = LineStep::new(b'\n', 0, 0);
        assert_eq!(it.next(b"abc"), None);
    }

    #[test]
    fn preceding_lines_doc() {
        // These are the examples mentions in the documentation of `preceding`.
        let bytes = b"abc\nxyz\n";
        assert_eq!(4, preceding_by_pos(bytes, 7, b'\n', 0));
        assert_eq!(4, preceding_by_pos(bytes, 8, b'\n', 0));
        assert_eq!(0, preceding_by_pos(bytes, 7, b'\n', 1));
        assert_eq!(0, preceding_by_pos(bytes, 8, b'\n', 1));
    }

    #[test]
    fn preceding_lines_sherlock() {
        let t = SHERLOCK;
        let lines = line_ranges(t);

        // The following tests check the count == 0 case, i.e., finding the
        // beginning of the line containing the given position.
        assert_eq!(0, prev(t, 0, 0));
        assert_eq!(0, prev(t, 1, 0));
        // The line terminator is addressed by `end-1` and terminates the line
        // it is part of.
        assert_eq!(0, prev(t, lines[0].end - 1, 0));
        assert_eq!(lines[0].start, prev(t, lines[0].end, 0));
        // The end position of line addresses the byte immediately following a
        // line terminator, which puts it on the following line.
        assert_eq!(lines[1].start, prev(t, lines[0].end + 1, 0));

        // Now tests for count > 0.
        assert_eq!(0, prev(t, 0, 1));
        assert_eq!(0, prev(t, 0, 2));
        assert_eq!(0, prev(t, 1, 1));
        assert_eq!(0, prev(t, 1, 2));
        assert_eq!(0, prev(t, lines[0].end - 1, 1));
        assert_eq!(0, prev(t, lines[0].end - 1, 2));
        assert_eq!(0, prev(t, lines[0].end, 1));
        assert_eq!(0, prev(t, lines[0].end, 2));
        assert_eq!(lines[3].start, prev(t, lines[4].end - 1, 1));
        assert_eq!(lines[3].start, prev(t, lines[4].end, 1));
        assert_eq!(lines[4].start, prev(t, lines[4].end + 1, 1));

        // The last line has no line terminator.
        assert_eq!(lines[5].start, prev(t, lines[5].end, 0));
        assert_eq!(lines[5].start, prev(t, lines[5].end - 1, 0));
        assert_eq!(lines[4].start, prev(t, lines[5].end, 1));
        assert_eq!(lines[0].start, prev(t, lines[5].end, 5));
    }

    #[test]
    fn preceding_lines_short() {
        let t = "a\nb\nc\nd\ne\nf\n";
        let lines = line_ranges(t);
        assert_eq!(12, t.len());

        assert_eq!(lines[5].start, prev(t, lines[5].end, 0));
        assert_eq!(lines[4].start, prev(t, lines[5].end, 1));
        assert_eq!(lines[3].start, prev(t, lines[5].end, 2));
        assert_eq!(lines[2].start, prev(t, lines[5].end, 3));
        assert_eq!(lines[1].start, prev(t, lines[5].end, 4));
        assert_eq!(lines[0].start, prev(t, lines[5].end, 5));
        assert_eq!(lines[0].start, prev(t, lines[5].end, 6));

        assert_eq!(lines[5].start, prev(t, lines[5].end - 1, 0));
        assert_eq!(lines[4].start, prev(t, lines[5].end - 1, 1));
        assert_eq!(lines[3].start, prev(t, lines[5].end - 1, 2));
        assert_eq!(lines[2].start, prev(t, lines[5].end - 1, 3));
        assert_eq!(lines[1].start, prev(t, lines[5].end - 1, 4));
        assert_eq!(lines[0].start, prev(t, lines[5].end - 1, 5));
        assert_eq!(lines[0].start, prev(t, lines[5].end - 1, 6));

        assert_eq!(lines[4].start, prev(t, lines[5].start, 0));
        assert_eq!(lines[3].start, prev(t, lines[5].start, 1));
        assert_eq!(lines[2].start, prev(t, lines[5].start, 2));
        assert_eq!(lines[1].start, prev(t, lines[5].start, 3));
        assert_eq!(lines[0].start, prev(t, lines[5].start, 4));
        assert_eq!(lines[0].start, prev(t, lines[5].start, 5));

        assert_eq!(lines[3].start, prev(t, lines[4].end - 1, 1));
        assert_eq!(lines[2].start, prev(t, lines[4].start, 1));

        assert_eq!(lines[2].start, prev(t, lines[3].end - 1, 1));
        assert_eq!(lines[1].start, prev(t, lines[3].start, 1));

        assert_eq!(lines[1].start, prev(t, lines[2].end - 1, 1));
        assert_eq!(lines[0].start, prev(t, lines[2].start, 1));

        assert_eq!(lines[0].start, prev(t, lines[1].end - 1, 1));
        assert_eq!(lines[0].start, prev(t, lines[1].start, 1));

        assert_eq!(lines[0].start, prev(t, lines[0].end - 1, 1));
        assert_eq!(lines[0].start, prev(t, lines[0].start, 1));
    }

    #[test]
    fn preceding_lines_empty1() {
        let t = "\n\n\nd\ne\nf\n";
        let lines = line_ranges(t);
        assert_eq!(9, t.len());

        assert_eq!(lines[0].start, prev(t, lines[0].end, 0));
        assert_eq!(lines[0].start, prev(t, lines[0].end, 1));
        assert_eq!(lines[1].start, prev(t, lines[1].end, 0));
        assert_eq!(lines[0].start, prev(t, lines[1].end, 1));

        assert_eq!(lines[5].start, prev(t, lines[5].end, 0));
        assert_eq!(lines[4].start, prev(t, lines[5].end, 1));
        assert_eq!(lines[3].start, prev(t, lines[5].end, 2));
        assert_eq!(lines[2].start, prev(t, lines[5].end, 3));
        assert_eq!(lines[1].start, prev(t, lines[5].end, 4));
        assert_eq!(lines[0].start, prev(t, lines[5].end, 5));
        assert_eq!(lines[0].start, prev(t, lines[5].end, 6));
    }

    #[test]
    fn preceding_lines_empty2() {
        let t = "a\n\n\nd\ne\nf\n";
        let lines = line_ranges(t);
        assert_eq!(10, t.len());

        assert_eq!(lines[0].start, prev(t, lines[0].end, 0));
        assert_eq!(lines[0].start, prev(t, lines[0].end, 1));
        assert_eq!(lines[1].start, prev(t, lines[1].end, 0));
        assert_eq!(lines[0].start, prev(t, lines[1].end, 1));

        assert_eq!(lines[5].start, prev(t, lines[5].end, 0));
        assert_eq!(lines[4].start, prev(t, lines[5].end, 1));
        assert_eq!(lines[3].start, prev(t, lines[5].end, 2));
        assert_eq!(lines[2].start, prev(t, lines[5].end, 3));
        assert_eq!(lines[1].start, prev(t, lines[5].end, 4));
        assert_eq!(lines[0].start, prev(t, lines[5].end, 5));
        assert_eq!(lines[0].start, prev(t, lines[5].end, 6));
    }
}
