use std::borrow::Cow;
use std::fmt;
use std::io;
use std::path::Path;
use std::time;

use bstr::{ByteSlice, ByteVec};
use grep_matcher::{Captures, LineTerminator, Match, Matcher};
use grep_searcher::{
    LineIter, Searcher, SinkContext, SinkContextKind, SinkError, SinkMatch,
};
#[cfg(feature = "serde1")]
use serde::{Serialize, Serializer};

use crate::MAX_LOOK_AHEAD;
/// 用于在分摊分配的情况下处理替换的类型。
pub struct Replacer<M: Matcher> {
    space: Option<Space<M>>,
}

/// 用于存储替换时的空间。
struct Space<M: Matcher> {
    /// 存储捕获位置的地方。
    caps: M::Captures,
    /// 写入替换结果的地方。
    dst: Vec<u8>,
    /// 存储匹配在 `dst` 中的偏移量的地方。
    matches: Vec<Match>,
}

impl<M: Matcher> fmt::Debug for Replacer<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (dst, matches) = self.replacement().unwrap_or((&[], &[]));
        f.debug_struct("Replacer")
            .field("dst", &dst)
            .field("matches", &matches)
            .finish()
    }
}

impl<M: Matcher> Replacer<M> {
    /// 为特定匹配器创建一个新的替换器。
    ///
    /// 此构造函数不分配内存。只有在需要时，才会延迟分配处理替换所需的空间。
    pub fn new() -> Replacer<M> {
        Replacer { space: None }
    }

    /// 对给定主题字符串执行替换操作，将所有匹配项替换为给定的替换内容。
    /// 要访问替换结果，请使用 `replacement` 方法。
    ///
    /// 如果底层匹配器报告错误，则可能失败。
    pub fn replace_all<'a>(
        &'a mut self,
        searcher: &Searcher,
        matcher: &M,
        mut subject: &[u8],
        range: std::ops::Range<usize>,
        replacement: &[u8],
    ) -> io::Result<()> {
        // 详见下面 'find_iter_at_in_context' 中的大型注释，解释为什么要这样处理。
        let is_multi_line = searcher.multi_line_with_matcher(&matcher);
        if is_multi_line {
            if subject[range.end..].len() >= MAX_LOOK_AHEAD {
                subject = &subject[..range.end + MAX_LOOK_AHEAD];
            }
        } else {
            // 当搜索单行时，应该移除行终止符。否则，正则表达式（通过环视）可能会观察到行终止符，导致无法匹配。
            let mut m = Match::new(0, range.end);
            trim_line_terminator(searcher, subject, &mut m);
            subject = &subject[..m.end()];
        }
        {
            let &mut Space { ref mut dst, ref mut caps, ref mut matches } =
                self.allocate(matcher)?;
            dst.clear();
            matches.clear();

            replace_with_captures_in_context(
                matcher,
                subject,
                range.clone(),
                caps,
                dst,
                |caps, dst| {
                    let start = dst.len();
                    caps.interpolate(
                        |name| matcher.capture_index(name),
                        subject,
                        replacement,
                        dst,
                    );
                    let end = dst.len();
                    matches.push(Match::new(start, end));
                    true
                },
            )
            .map_err(io::Error::error_message)?;
        }
        Ok(())
    }

    /// 返回先前替换的结果以及返回的替换缓冲区内的所有替换出现的匹配偏移量。
    ///
    /// 如果没有发生替换，则返回 `None`。
    pub fn replacement<'a>(&'a self) -> Option<(&'a [u8], &'a [Match])> {
        match self.space {
            None => None,
            Some(ref space) => {
                if space.matches.is_empty() {
                    None
                } else {
                    Some((&space.dst, &space.matches))
                }
            }
        }
    }

    /// 清除用于执行替换的空间。
    ///
    /// 在调用 `clear`（但在执行另一个替换之前）后，调用 `replacement` 将始终返回 `None`。
    pub fn clear(&mut self) {
        if let Some(ref mut space) = self.space {
            space.dst.clear();
            space.matches.clear();
        }
    }

    /// 为与给定匹配器一起使用时分配替换所需的空间，并返回对该空间的可变引用。
    ///
    /// 如果为来自给定匹配器的捕获位置分配空间失败，则可能会失败。
    fn allocate(&mut self, matcher: &M) -> io::Result<&mut Space<M>> {
        if self.space.is_none() {
            let caps =
                matcher.new_captures().map_err(io::Error::error_message)?;
            self.space =
                Some(Space { caps: caps, dst: vec![], matches: vec![] });
        }
        Ok(self.space.as_mut().unwrap())
    }
}

/// 一个简单的抽象层，用于在搜索器报告的匹配项或上下文行之间进行抽象。
///
/// 特别是，这提供了一个 API，它将 `SinkMatch` 和 `SinkContext` 类型联合起来，
/// 同时还公开了所有单独匹配位置的列表。
///
/// 虽然这是一个方便的机制，可以在 `SinkMatch` 和 `SinkContext` 上进行抽象，
/// 但这还提供了一种在替换后抽象的方法。也就是说，替换后，可以使用替换结果的结果来构造一个 `Sunk` 值，
/// 而不是直接使用搜索器直接报告的字节。
#[derive(Debug)]
pub struct Sunk<'a> {
    bytes: &'a [u8],
    absolute_byte_offset: u64,
    line_number: Option<u64>,
    context_kind: Option<&'a SinkContextKind>,
    matches: &'a [Match],
    original_matches: &'a [Match],
}

impl<'a> Sunk<'a> {
    #[inline]
    pub fn empty() -> Sunk<'static> {
        Sunk {
            bytes: &[],
            absolute_byte_offset: 0,
            line_number: None,
            context_kind: None,
            matches: &[],
            original_matches: &[],
        }
    }

    #[inline]
    pub fn from_sink_match(
        sunk: &'a SinkMatch<'a>,
        original_matches: &'a [Match],
        replacement: Option<(&'a [u8], &'a [Match])>,
    ) -> Sunk<'a> {
        let (bytes, matches) =
            replacement.unwrap_or_else(|| (sunk.bytes(), original_matches));
        Sunk {
            bytes: bytes,
            absolute_byte_offset: sunk.absolute_byte_offset(),
            line_number: sunk.line_number(),
            context_kind: None,
            matches: matches,
            original_matches: original_matches,
        }
    }

    #[inline]
    pub fn from_sink_context(
        sunk: &'a SinkContext<'a>,
        original_matches: &'a [Match],
        replacement: Option<(&'a [u8], &'a [Match])>,
    ) -> Sunk<'a> {
        let (bytes, matches) =
            replacement.unwrap_or_else(|| (sunk.bytes(), original_matches));
        Sunk {
            bytes: bytes,
            absolute_byte_offset: sunk.absolute_byte_offset(),
            line_number: sunk.line_number(),
            context_kind: Some(sunk.kind()),
            matches: matches,
            original_matches: original_matches,
        }
    }

    #[inline]
    pub fn context_kind(&self) -> Option<&'a SinkContextKind> {
        self.context_kind
    }

    #[inline]
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    #[inline]
    pub fn matches(&self) -> &'a [Match] {
        self.matches
    }

    #[inline]
    pub fn original_matches(&self) -> &'a [Match] {
        self.original_matches
    }

    #[inline]
    pub fn lines(&self, line_term: u8) -> LineIter<'a> {
        LineIter::new(line_term, self.bytes())
    }

    #[inline]
    pub fn absolute_byte_offset(&self) -> u64 {
        self.absolute_byte_offset
    }

    #[inline]
    pub fn line_number(&self) -> Option<u64> {
        self.line_number
    }
}
/// 一个简单的封装，表示打印机使用的文件路径。
///
/// 这表示我们可能希望在路径上执行的任何转换，例如将其转换为有效的 UTF-8 和/或将其分隔符替换为其他内容。
/// 这使我们能够在我们为每个匹配项打印文件路径时分摊工作。
///
/// 在常见情况下，不需要进行转换，这使我们可以避免分配。通常，只有在 Windows 需要进行转换，因为我们不能直接访问路径的原始字节，
/// 首先需要将其转换为 UTF-8。Windows 通常也是路径分隔符替换的地方，例如，在 cygwin 环境中使用 `/` 代替 `\`。
///
/// 使用此类型的用户应该从标准库中找到的普通 `Path` 构造它。然后可以使用 `as_bytes` 方法将其写入任何 `io::Write` 实现。
/// 这样可以实现平台的可移植性，但代价很小：在 Windows 上，不是有效 UTF-16 的路径将无法正确回路。
#[derive(Clone, Debug)]
pub struct PrinterPath<'a>(Cow<'a, [u8]>);

impl<'a> PrinterPath<'a> {
    /// 创建适合打印的新路径。
    pub fn new(path: &'a Path) -> PrinterPath<'a> {
        PrinterPath(Vec::from_path_lossy(path))
    }

    /// 从给定路径创建一个新的打印机路径，可以在不分配的情况下有效地写入 writer。
    ///
    /// 如果存在给定分隔符，则`path`中的任何分隔符都将被替换为它。
    pub fn with_separator(path: &'a Path, sep: Option<u8>) -> PrinterPath<'a> {
        let mut ppath = PrinterPath::new(path);
        if let Some(sep) = sep {
            ppath.replace_separator(sep);
        }
        ppath
    }

    /// 用给定的分隔符替换此路径中的路径分隔符，并进行原地操作。
    /// 在 Windows 上，`/` 和 `\` 都被视为路径分隔符，都将被 `new_sep` 替换。
    /// 在其他所有环境中，只有 `/` 被视为路径分隔符。
    fn replace_separator(&mut self, new_sep: u8) {
        let transformed_path: Vec<u8> = self
            .0
            .bytes()
            .map(|b| {
                if b == b'/' || (cfg!(windows) && b == b'\\') {
                    new_sep
                } else {
                    b
                }
            })
            .collect();
        self.0 = Cow::Owned(transformed_path);
    }

    /// 返回此路径的原始字节。
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// 为 std::time::Duration 提供更好的 Display 和 Serialize 实现的类型。
/// 序列化格式实际上应与 std::time::Duration 的 Deserialize 实现兼容，
/// 因为此类型只添加了新字段。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NiceDuration(pub time::Duration);

impl fmt::Display for NiceDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:0.6}s", self.fractional_seconds())
    }
}

impl NiceDuration {
    /// 返回此持续时间的秒数的分数形式。
    /// 小数点左边的数字是秒数，右边的数字是毫秒数。
    fn fractional_seconds(&self) -> f64 {
        let fractional = (self.0.subsec_nanos() as f64) / 1_000_000_000.0;
        self.0.as_secs() as f64 + fractional
    }
}

#[cfg(feature = "serde1")]
impl Serialize for NiceDuration {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        let mut state = ser.serialize_struct("Duration", 2)?;
        state.serialize_field("secs", &self.0.as_secs())?;
        state.serialize_field("nanos", &self.0.subsec_nanos())?;
        state.serialize_field("human", &format!("{}", self))?;
        state.end()
    }
}

/// 从给定切片中修剪前缀 ASCII 空格，并返回相应的范围。
///
/// 这会在遇到非空格或行终止符时停止修剪前缀。
pub fn trim_ascii_prefix(
    line_term: LineTerminator,
    slice: &[u8],
    range: Match,
) -> Match {
    fn is_space(b: u8) -> bool {
        match b {
            b'\t' | b'\n' | b'\x0B' | b'\x0C' | b'\r' | b' ' => true,
            _ => false,
        }
    }

    let count = slice[range]
        .iter()
        .take_while(|&&b| -> bool {
            is_space(b) && !line_term.as_bytes().contains(&b)
        })
        .count();
    range.with_start(range.start() + count)
}

pub fn find_iter_at_in_context<M, F>(
    searcher: &Searcher,
    matcher: M,
    mut bytes: &[u8],
    range: std::ops::Range<usize>,
    mut matched: F,
) -> io::Result<()>
where
    M: Matcher,
    F: FnMut(Match) -> bool,
{
    // 这种奇怪的做法是为了考虑正则表达式可能的前瞻情况。这里的问题是，mat.bytes() 不包括多行模式下匹配边界之外的行，
    // 这意味着当我们在这里重新发现完整的匹配集时，如果正则表达式需要一些超出匹配行的前瞻，那么正则表达式可能不再匹配。
    //
    // PCRE2（以及 grep-matcher 接口）没有指定搜索的结束范围的方法。所以我们来进行调整，
    // 让正则表达式引擎搜索缓冲区的剩余部分... 但为了避免事情变得太疯狂，我们将缓冲区限制在一定范围内。
    //
    // 如果不是多行模式，那么这都是不需要的。或者，如果我们将 grep 接口重新设计为从搜索器中传递所有匹配项（如果有的话），
    // 那么这可能也有所帮助。但是，这会导致在不需要计数的情况下支付前端不可避免的成本。所以，那时你必须引入一种条件情况下传递匹配项的方式，只在需要时传递。
    // 唉。也许更大的问题是搜索器应该负责在必要时找到匹配项，打印机不应该在这方面涉及。叹息。抽象边界很难。
    let is_multi_line = searcher.multi_line_with_matcher(&matcher);
    if is_multi_line {
        if bytes[range.end..].len() >= MAX_LOOK_AHEAD {
            bytes = &bytes[..range.end + MAX_LOOK_AHEAD];
        }
    } else {
        // 在搜索单行时，应该删除行终止符。否则，正则表达式（通过环视）可能会观察到行终止符并且由于它而不匹配。
        let mut m = Match::new(0, range.end);
        trim_line_terminator(searcher, bytes, &mut m);
        bytes = &bytes[..m.end()];
    }
    matcher
        .find_iter_at(bytes, range.start, |m| {
            if m.start() >= range.end {
                return false;
            }
            matched(m)
        })
        .map_err(io::Error::error_message)
}

/// 给定一个缓冲区和一些边界，如果在给定边界的末尾有一个行终止符，则修剪边界以移除行终止符。
pub fn trim_line_terminator(
    searcher: &Searcher,
    buf: &[u8],
    line: &mut Match,
) {
    // 获取搜索器的行终止符
    let lineterm = searcher.line_terminator();

    // 如果行终止符是缓冲区的末尾部分，则进行修剪
    if lineterm.is_suffix(&buf[*line]) {
        let mut end = line.end() - 1;

        // 如果行终止符是 CRLF，并且在倒数第二个位置有一个 '\r'，则将末尾减一
        if lineterm.is_crlf() && end > 0 && buf.get(end - 1) == Some(&b'\r') {
            end -= 1;
        }

        // 更新 Match 的结束位置
        *line = line.with_end(end);
    }
}

/// 类似于 `Matcher::replace_with_captures_at`，但接受一个结束范围。
///
/// 也可以参见：`find_iter_at_in_context`，了解为什么我们需要这个。
fn replace_with_captures_in_context<M, F>(
    matcher: M,
    bytes: &[u8],
    range: std::ops::Range<usize>,
    caps: &mut M::Captures,
    dst: &mut Vec<u8>,
    mut append: F,
) -> Result<(), M::Error>
where
    M: Matcher,
    F: FnMut(&M::Captures, &mut Vec<u8>) -> bool,
{
    let mut last_match = range.start;
    matcher.captures_iter_at(bytes, range.start, caps, |caps| {
        let m = caps.get(0).unwrap();
        if m.start() >= range.end {
            return false;
        }
        dst.extend(&bytes[last_match..m.start()]);
        last_match = m.end();
        append(caps, dst)
    })?;
    let end = std::cmp::min(bytes.len(), range.end);
    dst.extend(&bytes[last_match..end]);
    Ok(())
}
