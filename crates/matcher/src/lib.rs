/*!
这个crate提供了一个正则表达式接口，重点关注面向行的搜索。这个crate的目的是提供一个低级的匹配接口，允许任何类型的子串或正则表达式实现来支持
[`grep-searcher`](https://docs.rs/grep-searcher)
crate提供的搜索例程。

这个crate提供的主要功能是
[`Matcher`](trait.Matcher.html)
trait。这个trait定义了一个用于文本搜索的抽象接口。它足够强大，支持从基本的子串搜索到任意复杂的正则表达式实现，而不会牺牲性能。

在这个crate中做出的一个关键设计决策是使用*内部迭代*，又称为搜索的“推”模型。在这个范式中，`Matcher` trait的实现将驱动搜索，并在找到匹配时执行由调用者提供的回调函数。
这与Rust生态系统中普遍使用的*外部迭代*（“拉”模型）的风格不同。选择内部迭代有两个主要原因：

* 一些搜索实现本身可能需要内部迭代。将内部迭代器转换为外部迭代器可能是非常复杂甚至实际上是不可能的。
* Rust的类型系统在不放弃其他东西（即易用性和/或性能）的情况下，不能以外部迭代的方式编写通用接口。

换句话说，选择内部迭代是因为它是最低公共分母，并且因为在当今的Rust中，这可能是表达接口的最不糟糕的方式。因此，这个trait并不是专门为日常使用而设计的，
尽管如果您想编写可以在多个不同的正则表达式实现之间通用的代码，您可能会发现它是一个值得付出的代价。
*/

#![deny(missing_docs)]

use std::fmt;
use std::io;
use std::ops;
use std::u64;

use crate::interpolate::interpolate;

mod interpolate;

/// The type of a match.
///
/// The type of a match is a possibly empty range pointing to a contiguous
/// block of addressable memory.
///
/// Every `Match` is guaranteed to satisfy the invariant that `start <= end`.
///
/// # Indexing
///
/// This type is structurally identical to `std::ops::Range<usize>`, but
/// is a bit more ergonomic for dealing with match indices. In particular,
/// this type implements `Copy` and provides methods for building new `Match`
/// values based on old `Match` values. Finally, the invariant that `start`
/// is always less than or equal to `end` is enforced.
///
/// A `Match` can be used to slice a `&[u8]`, `&mut [u8]` or `&str` using
/// range notation. e.g.,
///
/// ```
/// use grep_matcher::Match;
///
/// let m = Match::new(2, 5);
/// let bytes = b"abcdefghi";
/// assert_eq!(b"cde", &bytes[m]);
/// ```
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Match {
    start: usize,
    end: usize,
}

impl Match {
    /// 创建一个新的匹配。
    ///
    /// # Panics
    ///
    /// 如果 `start > end`，则此函数会导致 panic。
    #[inline]
    pub fn new(start: usize, end: usize) -> Match {
        assert!(start <= end);
        Match { start, end }
    }

    /// 在给定的偏移量创建一个零宽度的匹配。
    #[inline]
    pub fn zero(offset: usize) -> Match {
        Match { start: offset, end: offset }
    }

    /// 返回此匹配的起始偏移量。
    #[inline]
    pub fn start(&self) -> usize {
        self.start
    }

    /// 返回此匹配的结束偏移量。
    #[inline]
    pub fn end(&self) -> usize {
        self.end
    }

    /// 返回一个新的匹配，起始偏移量被替换为给定的值。
    ///
    /// # Panics
    ///
    /// 如果 `start > self.end`，则此方法会导致 panic。
    #[inline]
    pub fn with_start(&self, start: usize) -> Match {
        assert!(start <= self.end, "{} 不小于等于 {}", start, self.end);
        Match { start, ..*self }
    }

    /// 返回一个新的匹配，结束偏移量被替换为给定的值。
    ///
    /// # Panics
    ///
    /// 如果 `self.start > end`，则此方法会导致 panic。
    #[inline]
    pub fn with_end(&self, end: usize) -> Match {
        assert!(self.start <= end, "{} 不小于等于 {}", self.start, end);
        Match { end, ..*self }
    }

    /// 通过给定的数量偏移此匹配，并返回一个新的匹配。
    ///
    /// 这会将给定的偏移量添加到此匹配的起始和结束，并返回结果匹配。
    ///
    /// # Panics
    ///
    /// 如果将给定的数量添加到起始或结束偏移量会导致溢出，则会发生 panic。
    #[inline]
    pub fn offset(&self, amount: usize) -> Match {
        Match {
            start: self.start.checked_add(amount).unwrap(),
            end: self.end.checked_add(amount).unwrap(),
        }
    }

    /// 返回此匹配的字节数。
    #[inline]
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// 如果且仅如果此匹配为空，则返回 true。
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl ops::Index<Match> for [u8] {
    type Output = [u8];

    #[inline]
    fn index(&self, index: Match) -> &[u8] {
        &self[index.start..index.end]
    }
}

impl ops::IndexMut<Match> for [u8] {
    #[inline]
    fn index_mut(&mut self, index: Match) -> &mut [u8] {
        &mut self[index.start..index.end]
    }
}

impl ops::Index<Match> for str {
    type Output = str;

    #[inline]
    fn index(&self, index: Match) -> &str {
        &self[index.start..index.end]
    }
}

/// 一个行终止符。
///
/// 行终止符表示行的结束。通常，每行要么由流的末尾终止，要么由特定的字节（或字节序列）终止。
///
/// 一般来说，行终止符是一个单字节，具体而言，在类Unix系统中为 `\n`。在Windows上，行终止符为 `\r\n`
/// （称为 `CRLF`，即 `回车换行`）。
///
/// 在所有平台上，默认的行终止符都是 `\n`。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LineTerminator(LineTerminatorImp);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum LineTerminatorImp {
    /// 表示任何单字节的行终止符。
    ///
    /// 我们将其表示为一个数组，以便可以将其安全地转换为切片，以便方便地访问。
    /// 在某些情况下，我们可以使用 `std::slice::from_ref` 来替代。
    Byte([u8; 1]),
    /// 由 `\r\n` 表示的行终止符。
    ///
    /// 当使用此选项时，使用者通常可以将独立的 `\n` 视为行终止符，除了 `\r\n` 之外。
    CRLF,
}

impl LineTerminator {
    /// 返回一个新的单字节行终止符。任何字节都是有效的。
    #[inline]
    pub fn byte(byte: u8) -> LineTerminator {
        LineTerminator(LineTerminatorImp::Byte([byte]))
    }

    /// 返回一个由 `\r\n` 表示的新的行终止符。
    ///
    /// 当使用此选项时，使用者通常可以将独立的 `\n` 视为行终止符，除了 `\r\n` 之外。
    #[inline]
    pub fn crlf() -> LineTerminator {
        LineTerminator(LineTerminatorImp::CRLF)
    }

    /// 如果且仅如果此行终止符为 CRLF，则返回 true。
    #[inline]
    pub fn is_crlf(&self) -> bool {
        self.0 == LineTerminatorImp::CRLF
    }

    /// 将此行终止符作为单个字节返回。
    ///
    /// 如果行终止符为 CRLF，则返回 `\n`。这对于一些例程非常有用，
    /// 例如，通过将 `\n` 视为行终止符来查找行边界，即使它没有被 `\r` 之前的字符所包围。
    #[inline]
    pub fn as_byte(&self) -> u8 {
        match self.0 {
            LineTerminatorImp::Byte(array) => array[0],
            LineTerminatorImp::CRLF => b'\n',
        }
    }

    /// 将此行终止符作为字节序列返回。
    ///
    /// 对于除了 `CRLF` 之外的所有行终止符，返回一个只有一个元素的序列。
    ///
    /// 返回的切片保证至少有长度 `1`。
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        match self.0 {
            LineTerminatorImp::Byte(ref array) => array,
            LineTerminatorImp::CRLF => &[b'\r', b'\n'],
        }
    }

    /// 如果且仅如果给定的切片以此行终止符结尾，则返回 true。
    ///
    /// 如果行终止符为 `CRLF`，则只检查最后一个字节是否为 `\n`。
    #[inline]
    pub fn is_suffix(&self, slice: &[u8]) -> bool {
        slice.last().map_or(false, |&b| b == self.as_byte())
    }
}

impl Default for LineTerminator {
    #[inline]
    fn default() -> LineTerminator {
        LineTerminator::byte(b'\n')
    }
}
/// 一个字节集合。
///
/// 在这个 crate 中，字节集合用于表示在特定 `Matcher` 特性的实现中，某些字节绝对不会出现在匹配中的情况。
/// 具体来说，如果能够确定这样的集合，那么调用者可以执行额外的操作，因为某些字节可能永远不会匹配。
///
/// 例如，如果配置了一个可能产生跨越多行的结果的搜索，但调用者提供的模式永远不会跨越多行匹配，
/// 那么可能会转向更优化的面向行的例程，而不需要处理多行匹配情况。
#[derive(Clone, Debug)]
pub struct ByteSet(BitSet);

#[derive(Clone, Copy)]
struct BitSet([u64; 4]);

impl fmt::Debug for BitSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut fmtd = f.debug_set();
        for b in (0..256).map(|b| b as u8) {
            if ByteSet(*self).contains(b) {
                fmtd.entry(&b);
            }
        }
        fmtd.finish()
    }
}

impl ByteSet {
    /// 创建一个空的字节集合。
    pub fn empty() -> ByteSet {
        ByteSet(BitSet([0; 4]))
    }

    /// 创建一个包含所有可能字节的完整字节集合。
    pub fn full() -> ByteSet {
        ByteSet(BitSet([u64::MAX; 4]))
    }

    /// 添加一个字节到这个集合中。
    ///
    /// 如果给定的字节已经属于此集合，则不会进行操作。
    pub fn add(&mut self, byte: u8) {
        let bucket = byte / 64;
        let bit = byte % 64;
        (self.0).0[bucket as usize] |= 1 << bit;
    }

    /// 添加一个字节范围（包括起始和结束字节）。
    pub fn add_all(&mut self, start: u8, end: u8) {
        for b in (start as u64..end as u64 + 1).map(|b| b as u8) {
            self.add(b);
        }
    }

    /// 从这个集合中移除一个字节。
    ///
    /// 如果给定的字节不在此集合中，则不会进行操作。
    pub fn remove(&mut self, byte: u8) {
        let bucket = byte / 64;
        let bit = byte % 64;
        (self.0).0[bucket as usize] &= !(1 << bit);
    }

    /// 移除一个字节范围（包括起始和结束字节）。
    pub fn remove_all(&mut self, start: u8, end: u8) {
        for b in (start as u64..end as u64 + 1).map(|b| b as u8) {
            self.remove(b);
        }
    }

    /// 返回 true 如果且仅如果给定的字节在此集合中。
    pub fn contains(&self, byte: u8) -> bool {
        let bucket = byte / 64;
        let bit = byte % 64;
        (self.0).0[bucket as usize] & (1 << bit) > 0
    }
}

/// 描述捕获组实现的特性。
///
/// 当匹配器支持捕获组提取时，它的责任是提供此特性的实现。
///
/// 主要来说，这个特性提供了一种以统一的方式访问捕获组的方法，而不需要任何特定的表示法。
/// 换句话说，不同的匹配器实现可能需要不同的内存表示捕获组的方式。
/// 这个特性允许匹配器维护其特定的内存表示。
///
/// 注意，这个特性明确不提供构建新捕获值的方法。相反，`Matcher` 负责构建一个，
/// 这可能需要了解匹配器的内部实现细节。
pub trait Captures {
    /// 返回捕获组的总数。这包括没有匹配任何内容的捕获组。
    fn len(&self) -> usize;

    /// 返回给定索引的捕获组匹配。如果没有匹配该捕获组，则返回 `None`。
    ///
    /// 当匹配器报告具有捕获组的匹配时，第一个捕获组（索引为 `0`）必须始终对应于整体匹配的偏移量。
    fn get(&self, i: usize) -> Option<Match>;

    /// 如果且仅如果这些捕获组为空，则返回 true。这发生在 `len` 为 `0` 时。
    ///
    /// 注意，具有非零长度但否则不包含匹配组的捕获组 *不* 是空的。
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 将 `replacement` 中的所有 `$name` 实例扩展为相应的捕获组 `name`，并将其写入给定的 `dst` 缓冲区中。
    ///
    ///（注意：如果您想要一个方便的方法来执行带插值的替换，
    /// 那么您会想要使用 `Matcher` 特性的 `replace_with_captures` 方法。）
    ///
    /// `name` 可以是与捕获组索引相对应的整数（按打开括号的顺序计数，其中 `0` 是整体匹配），
    /// 也可以是与命名捕获组相对应的名称（由字母、数字或下划线组成）。
    ///
    /// 通过给定的 `name_to_index` 函数，将 `name` 转换为捕获组索引。
    /// 如果 `name` 不是有效的捕获组（无论名称是否存在或是否是有效的索引），
    /// 则用空字符串替换它。
    ///
    /// 使用最长可能的名称。例如，`$1a` 查找名为 `1a` 的捕获组，而不是索引为 `1` 的捕获组。
    /// 要对名称更精确地进行控制，可以使用大括号，例如 `${1}a`。
    /// 在所有情况下，捕获组名称限制为 ASCII 字母、数字和下划线。
    ///
    /// 要写入字面量 `$`，请使用 `$$`。
    ///
    /// 注意，捕获组匹配索引是通过对给定的 `haystack` 进行切片来解析的。
    /// 通常，这意味着 `haystack` 应该是搜索以获取当前捕获组匹配的相同切片。
    fn interpolate<F>(
        &self,
        name_to_index: F,
        haystack: &[u8],
        replacement: &[u8],
        dst: &mut Vec<u8>,
    ) where
        F: FnMut(&str) -> Option<usize>,
    {
        interpolate(
            replacement,
            |i, dst| {
                if let Some(range) = self.get(i) {
                    dst.extend(&haystack[range]);
                }
            },
            name_to_index,
            dst,
        )
    }
}
/// `NoCaptures` 提供了 `Captures` 特性的始终为空的实现。
///
/// 这个类型对于不支持捕获组的 `Matcher` 实现非常有用。
#[derive(Clone, Debug)]
pub struct NoCaptures(());

impl NoCaptures {
    /// 创建一个空的捕获组集合。
    pub fn new() -> NoCaptures {
        NoCaptures(())
    }
}

impl Captures for NoCaptures {
    fn len(&self) -> usize {
        0
    }
    fn get(&self, _: usize) -> Option<Match> {
        None
    }
}

/// `NoError` 为从不产生错误的匹配器提供了错误类型。
///
/// 这个错误类型实现了 `std::error::Error` 和 `fmt::Display` 特性，用于匹配器实现中从不产生错误的情况。
///
/// 这个类型的 `fmt::Debug` 和 `fmt::Display` 实现会导致 panic。
#[derive(Debug, Eq, PartialEq)]
pub struct NoError(());

impl ::std::error::Error for NoError {
    fn description(&self) -> &str {
        "no error"
    }
}

impl fmt::Display for NoError {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        panic!("BUG for NoError: an impossible error occurred")
    }
}

impl From<NoError> for io::Error {
    fn from(_: NoError) -> io::Error {
        panic!("BUG for NoError: an impossible error occurred")
    }
}

/// 用于行导向匹配器的匹配类型。
#[derive(Clone, Copy, Debug)]
pub enum LineMatchKind {
    /// 行内已知包含匹配的位置。
    ///
    /// 此位置可以在行中的任何地方。它不需要指向匹配的位置。
    Confirmed(usize),
    /// 行内可能包含匹配的位置，需要进行搜索以进行验证。
    ///
    /// 此位置可以在行中的任何地方。它不需要指向匹配的位置。
    Candidate(usize),
}

/// 匹配器定义了正则表达式实现的接口。
///
/// 虽然这个特性很大，但只有两个必须提供的方法：`find_at` 和 `new_captures`。
/// 如果您的实现不支持捕获组，则可以使用 [`NoCaptures`](struct.NoCaptures.html) 实现 `new_captures`。
/// 如果您的实现确实支持捕获组，则还应该根据文档的规定实现其他与捕获相关的方法。重要的是，这包括 `captures_at`。
///
/// 此特性上的其余方法都在 `find_at` 和 `new_captures` 之上提供了默认实现。
/// 在某些情况下，实现可能能够提供某些方法的更快变体；在这些情况下，只需覆盖默认实现即可。

pub trait Matcher {
    /// 用于此匹配器的捕获组的具体类型。
    ///
    /// 如果此实现不支持捕获组，则将其设置为 `NoCaptures`。
    type Captures: Captures;

    /// 此匹配器使用的错误类型。
    ///
    /// 对于不可能发生错误的匹配器，建议在此类型中使用本库中的 `NoError` 类型。
    /// 在将来，当“never”类型（用 `!` 表示）稳定下来时，可能应该使用它。
    type Error: fmt::Display;

    /// 在 `at` 之后的 `haystack` 中查找第一个匹配的起始和结束字节范围，其中字节偏移量相对于 `haystack` 的开始（而不是 `at`）。
    /// 如果没有匹配，那么将返回 `None`。
    ///
    /// `haystack` 的文本编码未严格指定。建议匹配器假设为 UTF-8，或者在最坏的情况下，一些兼容 ASCII 编码。
    ///
    /// 起始点的重要性在于它考虑了周围的上下文。例如，`\A` 锚只能在 `at == 0` 时匹配。
    fn find_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<Option<Match>, Self::Error>;

    /// 创建适用于此特性的捕获 API 的空捕获组。
    ///
    /// 不支持捕获组的实现应使用 `NoCaptures` 类型，并通过调用 `NoCaptures::new()` 来实现此方法。
    fn new_captures(&self) -> Result<Self::Captures, Self::Error>;

    /// 返回此匹配器中捕获组的总数。
    ///
    /// 如果匹配器支持捕获组，则此值必须始终至少为 1，其中第一个捕获组始终对应于整个匹配。
    ///
    /// 如果匹配器不支持捕获组，则应始终返回 0。
    ///
    /// 默认情况下，不支持捕获组，因此始终返回 0。
    fn capture_count(&self) -> usize {
        0
    }

    /// 将给定的捕获组名称映射到其相应的捕获组索引（如果存在）。如果不存在，则返回 `None`。
    ///
    /// 如果给定的捕获组名称映射到多个索引，则不指定返回哪一个。但是，保证返回其中之一。
    ///
    /// 默认情况下，不支持捕获组，因此始终返回 `None`。
    fn capture_index(&self, _name: &str) -> Option<usize> {
        None
    }

    /// 返回在 `haystack` 中第一个匹配的起始和结束字节范围。如果没有匹配，那么将返回 `None`。
    ///
    /// `haystack` 的文本编码未严格指定。建议匹配器假设为 UTF-8，或者在最坏的情况下，一些兼容 ASCII 编码。
    fn find(&self, haystack: &[u8]) -> Result<Option<Match>, Self::Error> {
        self.find_at(haystack, 0)
    }

    /// 在 `haystack` 中连续非重叠匹配上执行给定的函数。如果没有匹配，那么永远不会调用给定的函数。如果函数返回 `false`，则停止迭代。
    fn find_iter<F>(
        &self,
        haystack: &[u8],
        matched: F,
    ) -> Result<(), Self::Error>
    where
        F: FnMut(Match) -> bool,
    {
        self.find_iter_at(haystack, 0, matched)
    }

    /// 在 `haystack` 中连续非重叠匹配上执行给定的函数。如果没有匹配，那么永远不会调用给定的函数。如果函数返回 `false`，则停止迭代。
    ///
    /// 起始点的重要性在于它考虑了周围的上下文。例如，`\A` 锚只能在 `at == 0` 时匹配。
    fn find_iter_at<F>(
        &self,
        haystack: &[u8],
        at: usize,
        mut matched: F,
    ) -> Result<(), Self::Error>
    where
        F: FnMut(Match) -> bool,
    {
        self.try_find_iter_at(haystack, at, |m| Ok(matched(m)))
            .map(|r: Result<(), ()>| r.unwrap())
    }

    /// 在 `haystack` 中连续非重叠匹配上执行给定的函数。如果没有匹配，那么永远不会调用给定的函数。如果函数返回 `false`，则停止迭代。
    /// 类似地，如果函数返回错误，则停止迭代并产生错误。如果执行搜索时发生错误，则转换为 `E`。
    fn try_find_iter<F, E>(
        &self,
        haystack: &[u8],
        matched: F,
    ) -> Result<Result<(), E>, Self::Error>
    where
        F: FnMut(Match) -> Result<bool, E>,
    {
        self.try_find_iter_at(haystack, 0, matched)
    }

    /// 在 `haystack` 中连续非重叠匹配上执行给定的函数。如果没有匹配，那么永远不会调用给定的函数。如果函数返回 `false`，则停止迭代。
    /// 类似地，如果函数返回错误，则停止迭代并产生错误。如果执行搜索时发生错误，则转换为 `E`。
    ///
    /// 起始点的重要性在于它考虑了周围的上下文。例如，`\A` 锚只能在 `at == 0` 时匹配。
    fn try_find_iter_at<F, E>(
        &self,
        haystack: &[u8],
        at: usize,
        mut matched: F,
    ) -> Result<Result<(), E>, Self::Error>
    where
        F: FnMut(Match) -> Result<bool, E>,
    {
        let mut last_end = at;
        let mut last_match = None;

        loop {
            if last_end > haystack.len() {
                return Ok(Ok(()));
            }
            let m = match self.find_at(haystack, last_end)? {
                None => return Ok(Ok(())),
                Some(m) => m,
            };
            if m.start == m.end {
                // 这是一个空匹配。为确保我们取得进展，从接下来的下一个匹配的最小可能起始位置开始下一次搜索。
                last_end = m.end + 1;
                // 不要立即接受跟在匹配后面的空匹配。继续下一个匹配。
                if Some(m.end) == last_match {
                    continue;
                }
            } else {
                last_end = m.end;
            }
            last_match = Some(m.end);
            match matched(m) {
                Ok(true) => continue,
                Ok(false) => return Ok(Ok(())),
                Err(err) => return Ok(Err(err)),
            }
        }
    }

    /// 将 `haystack` 中第一个匹配的捕获组结果填充到 `caps` 中。如果没有匹配，那么返回 `false`。
    fn captures(
        &self,
        haystack: &[u8],
        caps: &mut Self::Captures,
    ) -> Result<bool, Self::Error> {
        self.captures_at(haystack, 0, caps)
    }

    /// 在 `haystack` 中连续非重叠匹配上执行给定的函数，并从每个匹配中提取捕获组。如果没有匹配，那么永远不会调用给定的函数。如果函数返回 `false`，则停止迭代。
    fn captures_iter<F>(
        &self,
        haystack: &[u8],
        caps: &mut Self::Captures,
        matched: F,
    ) -> Result<(), Self::Error>
    where
        F: FnMut(&Self::Captures) -> bool,
    {
        self.captures_iter_at(haystack, 0, caps, matched)
    }

    /// 在 `haystack` 中连续非重叠匹配上执行给定的函数，并从每个匹配中提取捕获组。如果没有匹配，那么永远不会调用给定的函数。如果函数返回 `false`，则停止迭代。
    ///
    /// 起始点的重要性在于它考虑了周围的上下文。例如，`\A` 锚只能在 `at == 0` 时匹配。
    fn captures_iter_at<F>(
        &self,
        haystack: &[u8],
        at: usize,
        caps: &mut Self::Captures,
        mut matched: F,
    ) -> Result<(), Self::Error>
    where
        F: FnMut(&Self::Captures) -> bool,
    {
        self.try_captures_iter_at(haystack, at, caps, |caps| Ok(matched(caps)))
            .map(|r: Result<(), ()>| r.unwrap())
    }
    /// 在 `haystack` 中连续非重叠匹配上执行给定的函数，并从每个匹配中提取捕获组。如果没有匹配，那么永远不会调用给定的函数。
    /// 如果函数返回 `false`，则停止迭代。类似地，如果函数返回错误，则停止迭代并产生错误。如果执行搜索时发生错误，则转换为 `E`。
    fn try_captures_iter<F, E>(
        &self,
        haystack: &[u8],
        caps: &mut Self::Captures,
        matched: F,
    ) -> Result<Result<(), E>, Self::Error>
    where
        F: FnMut(&Self::Captures) -> Result<bool, E>,
    {
        self.try_captures_iter_at(haystack, 0, caps, matched)
    }

    /// 在 `haystack` 中连续非重叠匹配上执行给定的函数，并从每个匹配中提取捕获组。如果没有匹配，那么永远不会调用给定的函数。
    /// 如果函数返回 `false`，则停止迭代。类似地，如果函数返回错误，则停止迭代并产生错误。如果执行搜索时发生错误，则转换为 `E`。
    ///
    /// 起始点的重要性在于它考虑了周围的上下文。例如，`\A` 锚只能在 `at == 0` 时匹配。
    fn try_captures_iter_at<F, E>(
        &self,
        haystack: &[u8],
        at: usize,
        caps: &mut Self::Captures,
        mut matched: F,
    ) -> Result<Result<(), E>, Self::Error>
    where
        F: FnMut(&Self::Captures) -> Result<bool, E>,
    {
        let mut last_end = at;
        let mut last_match = None;

        loop {
            if last_end > haystack.len() {
                return Ok(Ok(()));
            }
            if !self.captures_at(haystack, last_end, caps)? {
                return Ok(Ok(()));
            }
            let m = caps.get(0).unwrap();
            if m.start == m.end {
                // 这是一个空匹配。为确保我们取得进展，从接下来的下一个匹配的最小可能起始位置开始下一次搜索。
                last_end = m.end + 1;
                // 不要立即接受跟在匹配后面的空匹配。继续下一个匹配。
                if Some(m.end) == last_match {
                    continue;
                }
            } else {
                last_end = m.end;
            }
            last_match = Some(m.end);
            match matched(caps) {
                Ok(true) => continue,
                Ok(false) => return Ok(Ok(())),
                Err(err) => return Ok(Err(err)),
            }
        }
    }

    /// 将 `haystack` 中第一个匹配的捕获组结果填充到 `matches` 中，位置在 `at` 之后，其中每个捕获组中的字节偏移量相对于 `haystack` 的开始（而不是 `at`）。
    /// 如果没有匹配，那么返回 `false`，并且给定的捕获组的内容是未指定的。
    ///
    /// `haystack` 的文本编码未严格指定。建议匹配器假设为 UTF-8，或者在最坏的情况下，一些兼容 ASCII 编码。
    ///
    /// 起始点的重要性在于它考虑了周围的上下文。例如，`\A` 锚只能在 `at == 0` 时匹配。
    ///
    /// 默认情况下，不支持捕获组，并且此实现始终会像匹配是不可能的一样行事。
    ///
    /// 提供对捕获组的支持的实现必须保证当发生匹配时，第一个捕获组匹配（索引为 `0`）始终设置为整体匹配偏移量。
    ///
    /// 请注意，如果实现者希望支持捕获组，则应实现此方法。基于捕获组的其他匹配方法将自动工作。
    fn captures_at(
        &self,
        _haystack: &[u8],
        _at: usize,
        _caps: &mut Self::Captures,
    ) -> Result<bool, Self::Error> {
        Ok(false)
    }

    /// 使用调用 `append` 的结果，将给定 haystack 中的每个匹配替换为结果。`append` 函数会接收匹配的起始和结束位置，以及所提供的 `dst` 缓冲区的句柄。
    ///
    /// 如果给定的 `append` 函数返回 `false`，则替换停止。
    fn replace<F>(
        &self,
        haystack: &[u8],
        dst: &mut Vec<u8>,
        mut append: F,
    ) -> Result<(), Self::Error>
    where
        F: FnMut(Match, &mut Vec<u8>) -> bool,
    {
        let mut last_match = 0;
        self.find_iter(haystack, |m| {
            dst.extend(&haystack[last_match..m.start]);
            last_match = m.end;
            append(m, dst)
        })?;
        dst.extend(&haystack[last_match..]);
        Ok(())
    }

    /// 使用匹配的捕获组，将给定 haystack 中的每个匹配替换为调用 `append` 的结果。
    ///
    /// 如果给定的 `append` 函数返回 `false`，则替换停止。
    fn replace_with_captures<F>(
        &self,
        haystack: &[u8],
        caps: &mut Self::Captures,
        dst: &mut Vec<u8>,
        append: F,
    ) -> Result<(), Self::Error>
    where
        F: FnMut(&Self::Captures, &mut Vec<u8>) -> bool,
    {
        self.replace_with_captures_at(haystack, 0, caps, dst, append)
    }

    /// 使用匹配的捕获组，将给定 haystack 中的每个匹配替换为调用 `append` 的结果。
    ///
    /// 如果给定的 `append` 函数返回 `false`，则替换停止。
    ///
    /// 起始点的重要性在于它考虑了周围的上下文。例如，`\A` 锚只能在 `at == 0` 时匹配。
    fn replace_with_captures_at<F>(
        &self,
        haystack: &[u8],
        at: usize,
        caps: &mut Self::Captures,
        dst: &mut Vec<u8>,
        mut append: F,
    ) -> Result<(), Self::Error>
    where
        F: FnMut(&Self::Captures, &mut Vec<u8>) -> bool,
    {
        let mut last_match = at;
        self.captures_iter_at(haystack, at, caps, |caps| {
            let m = caps.get(0).unwrap();
            dst.extend(&haystack[last_match..m.start]);
            last_match = m.end;
            append(caps, dst)
        })?;
        dst.extend(&haystack[last_match..]);
        Ok(())
    }

    /// 当且仅当匹配器匹配给定 haystack 时，返回 true。
    ///
    /// 默认情况下，此方法通过调用 `shortest_match` 实现。
    fn is_match(&self, haystack: &[u8]) -> Result<bool, Self::Error> {
        self.is_match_at(haystack, 0)
    }

    /// 当且仅当匹配器从给定位置开始匹配给定 haystack 时，返回 true。
    ///
    /// 默认情况下，此方法通过调用 `shortest_match_at` 实现。
    ///
    /// 起始点的重要性在于它考虑了周围的上下文。例如，`\A` 锚只能在 `at == 0` 时匹配。
    fn is_match_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<bool, Self::Error> {
        Ok(self.shortest_match_at(haystack, at)?.is_some())
    }

    /// 返回 `haystack` 中第一个匹配的结束位置。如果没有匹配，那么返回 `None`。
    ///
    /// 请注意，此方法报告的结束位置可能小于 `find` 报告的相同结束位置。
    /// 例如，在 haystack 上运行模式为 `a+` 的 `find` 方法，`aaa` 应报告范围为 `[0, 3)`，
    /// 但 `shortest_match` 可能报告 `1` 作为结束位置，因为保证在此位置上发生匹配。
    ///
    /// 此方法不应报告假阳性或假阴性。此方法的目的是，一些实现者可能能够提供比 `find` 更快的实现。
    ///
    /// 默认情况下，此方法通过调用 `find` 实现。
    fn shortest_match(
        &self,
        haystack: &[u8],
    ) -> Result<Option<usize>, Self::Error> {
        self.shortest_match_at(haystack, 0)
    }
    /// 返回在给定位置开始的 `haystack` 中第一个匹配的结束位置。如果没有匹配，那么返回 `None`。
    ///
    /// 请注意，此方法报告的结束位置可能小于 `find` 报告的相同结束位置。
    /// 例如，在 `aaa` 上运行模式为 `a+` 的 `find` 方法，应报告范围为 `[0, 3)`，
    /// 但 `shortest_match` 可能报告 `1` 作为结束位置，因为这是确保发生匹配的位置。
    ///
    /// 此方法不应报告假阳性或假阴性。此方法的目的是，一些实现者可能能够提供比 `find` 更快的实现。
    ///
    /// 默认情况下，此方法通过调用 `find_at` 实现。
    ///
    /// 起始点的重要性在于它考虑了周围的上下文。例如，`\A` 锚只能在 `at == 0` 时匹配。
    fn shortest_match_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<Option<usize>, Self::Error> {
        Ok(self.find_at(haystack, at)?.map(|m| m.end))
    }

    /// 如果可用，返回一组永远不会出现在实现产生的匹配中的字节。
    ///
    /// 具体来说，如果可以确定这样的集合，那么调用者可以根据某些字节永远不会匹配来执行其他操作。
    ///
    /// 例如，如果搜索配置为可能生成跨越多行的结果，但调用者提供的模式永远不会跨越多行匹配，
    /// 那么可能会转向更优化的面向行的例程，这些例程不需要处理多行匹配情况。
    ///
    /// 生成此集合的实现绝不能报告假阳性，但可能会产生假阴性。
    /// 也就是说，如果一个字节在此集合中，则必须保证它永远不在匹配中。
    /// 但是，如果一个字节不在此集合中，则调用者不能假设存在与该字节匹配的情况。
    ///
    /// 默认情况下，返回 `None`。
    fn non_matching_bytes(&self) -> Option<&ByteSet> {
        None
    }

    /// 如果此匹配器是作为面向行的匹配器编译的，则仅当行终止符永远不出现在此匹配器产生的任何匹配中时，
    /// 此方法返回行终止符。如果未将其编译为面向行的匹配器，或者无法提供上述保证，则必须返回 `None`，
    /// 这是默认情况。返回行终止符，当它可以出现在匹配结果中时，会导致未指定的行为。
    ///
    /// 行终止符通常为 `b'\n'`，但可以是任何单字节或 `CRLF`。
    ///
    /// 默认情况下，返回 `None`。
    fn line_terminator(&self) -> Option<LineTerminator> {
        None
    }

    /// 返回以下之一：已确认的行匹配、候选行匹配（可能是假阳性）或根本没有匹配（**不得**为假阴性）。
    /// 在报告已确认或候选匹配时，返回的位置可以是行中的任何位置。
    ///
    /// 默认情况下，此方法永远不会返回候选匹配，而始终返回已确认匹配或根本没有匹配。
    ///
    /// 当匹配器可以跨越多行匹配范围时，此方法的行为是未指定的。
    /// 也就是说，仅在调用者寻找下一个匹配行时才有用。也就是说，只有在 `line_terminator` 不返回 `None` 时，
    /// 调用者才应使用此方法。
    ///
    /// # 设计理念
    ///
    /// 面向行的匹配器从根本上讲是普通的匹配器，只是多了一个可选的方法：查找行。
    /// 默认情况下，通过匹配器的 `shortest_match` 方法实现此例程，该方法始终返回无匹配或 `LineMatchKind::Confirmed`。
    /// 但是，实现者可以提供一个例程，可以返回需要进一步验证以确认为匹配的候选行。
    /// 在某些情况下，通过其他方式查找候选行可能更快，而不是依赖于用于解析 `\w+foo\s+` 的更通用实现。
    /// 此时，调用者负责确认是否存在匹配。
    ///
    /// 请注意，尽管此方法可能报告假阳性，但不能报告假阴性。也就是说，它绝不能跳过包含匹配的行。
    fn find_candidate_line(
        &self,
        haystack: &[u8],
    ) -> Result<Option<LineMatchKind>, Self::Error> {
        Ok(self.shortest_match(haystack)?.map(LineMatchKind::Confirmed))
    }
}

impl<'a, M: Matcher> Matcher for &'a M {
    type Captures = M::Captures;
    type Error = M::Error;

    fn find_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<Option<Match>, Self::Error> {
        (*self).find_at(haystack, at)
    }

    fn new_captures(&self) -> Result<Self::Captures, Self::Error> {
        (*self).new_captures()
    }

    fn captures_at(
        &self,
        haystack: &[u8],
        at: usize,
        caps: &mut Self::Captures,
    ) -> Result<bool, Self::Error> {
        (*self).captures_at(haystack, at, caps)
    }

    fn capture_index(&self, name: &str) -> Option<usize> {
        (*self).capture_index(name)
    }

    fn capture_count(&self) -> usize {
        (*self).capture_count()
    }

    fn find(&self, haystack: &[u8]) -> Result<Option<Match>, Self::Error> {
        (*self).find(haystack)
    }

    fn find_iter<F>(
        &self,
        haystack: &[u8],
        matched: F,
    ) -> Result<(), Self::Error>
    where
        F: FnMut(Match) -> bool,
    {
        (*self).find_iter(haystack, matched)
    }

    fn find_iter_at<F>(
        &self,
        haystack: &[u8],
        at: usize,
        matched: F,
    ) -> Result<(), Self::Error>
    where
        F: FnMut(Match) -> bool,
    {
        (*self).find_iter_at(haystack, at, matched)
    }

    fn try_find_iter<F, E>(
        &self,
        haystack: &[u8],
        matched: F,
    ) -> Result<Result<(), E>, Self::Error>
    where
        F: FnMut(Match) -> Result<bool, E>,
    {
        (*self).try_find_iter(haystack, matched)
    }

    fn try_find_iter_at<F, E>(
        &self,
        haystack: &[u8],
        at: usize,
        matched: F,
    ) -> Result<Result<(), E>, Self::Error>
    where
        F: FnMut(Match) -> Result<bool, E>,
    {
        (*self).try_find_iter_at(haystack, at, matched)
    }

    fn captures(
        &self,
        haystack: &[u8],
        caps: &mut Self::Captures,
    ) -> Result<bool, Self::Error> {
        (*self).captures(haystack, caps)
    }

    fn captures_iter<F>(
        &self,
        haystack: &[u8],
        caps: &mut Self::Captures,
        matched: F,
    ) -> Result<(), Self::Error>
    where
        F: FnMut(&Self::Captures) -> bool,
    {
        (*self).captures_iter(haystack, caps, matched)
    }

    fn captures_iter_at<F>(
        &self,
        haystack: &[u8],
        at: usize,
        caps: &mut Self::Captures,
        matched: F,
    ) -> Result<(), Self::Error>
    where
        F: FnMut(&Self::Captures) -> bool,
    {
        (*self).captures_iter_at(haystack, at, caps, matched)
    }

    fn try_captures_iter<F, E>(
        &self,
        haystack: &[u8],
        caps: &mut Self::Captures,
        matched: F,
    ) -> Result<Result<(), E>, Self::Error>
    where
        F: FnMut(&Self::Captures) -> Result<bool, E>,
    {
        (*self).try_captures_iter(haystack, caps, matched)
    }

    fn try_captures_iter_at<F, E>(
        &self,
        haystack: &[u8],
        at: usize,
        caps: &mut Self::Captures,
        matched: F,
    ) -> Result<Result<(), E>, Self::Error>
    where
        F: FnMut(&Self::Captures) -> Result<bool, E>,
    {
        (*self).try_captures_iter_at(haystack, at, caps, matched)
    }

    fn replace<F>(
        &self,
        haystack: &[u8],
        dst: &mut Vec<u8>,
        append: F,
    ) -> Result<(), Self::Error>
    where
        F: FnMut(Match, &mut Vec<u8>) -> bool,
    {
        (*self).replace(haystack, dst, append)
    }

    fn replace_with_captures<F>(
        &self,
        haystack: &[u8],
        caps: &mut Self::Captures,
        dst: &mut Vec<u8>,
        append: F,
    ) -> Result<(), Self::Error>
    where
        F: FnMut(&Self::Captures, &mut Vec<u8>) -> bool,
    {
        (*self).replace_with_captures(haystack, caps, dst, append)
    }

    fn replace_with_captures_at<F>(
        &self,
        haystack: &[u8],
        at: usize,
        caps: &mut Self::Captures,
        dst: &mut Vec<u8>,
        append: F,
    ) -> Result<(), Self::Error>
    where
        F: FnMut(&Self::Captures, &mut Vec<u8>) -> bool,
    {
        (*self).replace_with_captures_at(haystack, at, caps, dst, append)
    }

    fn is_match(&self, haystack: &[u8]) -> Result<bool, Self::Error> {
        (*self).is_match(haystack)
    }

    fn is_match_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<bool, Self::Error> {
        (*self).is_match_at(haystack, at)
    }

    fn shortest_match(
        &self,
        haystack: &[u8],
    ) -> Result<Option<usize>, Self::Error> {
        (*self).shortest_match(haystack)
    }

    fn shortest_match_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<Option<usize>, Self::Error> {
        (*self).shortest_match_at(haystack, at)
    }

    fn non_matching_bytes(&self) -> Option<&ByteSet> {
        (*self).non_matching_bytes()
    }

    fn line_terminator(&self) -> Option<LineTerminator> {
        (*self).line_terminator()
    }

    fn find_candidate_line(
        &self,
        haystack: &[u8],
    ) -> Result<Option<LineMatchKind>, Self::Error> {
        (*self).find_candidate_line(haystack)
    }
}
