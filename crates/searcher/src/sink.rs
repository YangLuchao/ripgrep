use std::error;
use std::fmt;
use std::io;

use grep_matcher::LineTerminator;

use crate::lines::LineIter;
use crate::searcher::{ConfigError, Searcher};
/// 描述搜索器和 `Sink` 实现可能报告的错误的 trait。
///
/// 除非您有特殊的用例，否则您可能不需要显式实现此 trait。通常情况下，对于错误类型，
/// 使用 `io::Error`（实现了此 trait）可能已经足够，因为在搜索期间发生的大多数错误可能都是 `io::Error`。
pub trait SinkError: Sized {
    /// 将满足 `fmt::Display` trait 的任何值转换为错误的构造函数。
    fn error_message<T: fmt::Display>(message: T) -> Self;

    /// 将搜索期间发生的 I/O 错误转换为此错误类型的构造函数。
    ///
    /// 默认情况下，通过 `error_message` 构造函数来实现。
    fn error_io(err: io::Error) -> Self {
        Self::error_message(err)
    }

    /// 将在构建搜索器时发生的配置错误转换为此错误类型的构造函数。
    ///
    /// 默认情况下，通过 `error_message` 构造函数来实现。
    fn error_config(err: ConfigError) -> Self {
        Self::error_message(err)
    }
}

/// `io::Error` 可以直接用作 `Sink` 实现的错误。
impl SinkError for io::Error {
    fn error_message<T: fmt::Display>(message: T) -> io::Error {
        io::Error::new(io::ErrorKind::Other, message.to_string())
    }

    fn error_io(err: io::Error) -> io::Error {
        err
    }
}

/// `Box<std::error::Error>` 可以直接用作 `Sink` 实现的错误。
impl SinkError for Box<dyn error::Error> {
    fn error_message<T: fmt::Display>(message: T) -> Box<dyn error::Error> {
        Box::<dyn error::Error>::from(message.to_string())
    }
}

/// 定义如何处理搜索器的结果的 trait。
///
/// 在此 crate 中，搜索器遵循“推送”模型。这意味着搜索器驱动执行，并将结果推送回调用者。
/// 这与“拉取”模型相反，其中调用者驱动执行，并在需要结果时获取它们。这些也被称为“内部”和“外部”迭代策略。
///
/// 由于搜索器实现的复杂性等多种原因，此 crate 选择了“推送”或“内部”执行模型。因此，
/// 要在搜索结果上执行操作，调用者必须为搜索器提供此 trait 的实现，然后搜索器负责调用此 trait 上的方法。
///
/// 此 trait 定义了几种行为：
///
/// * 在找到匹配项时要执行的操作。调用者必须提供这些操作。
/// * 在发生错误时要执行的操作。调用者必须通过 [`SinkError`](trait.SinkError.html) trait 提供这些操作。
///   通常情况下，调用者可以使用 `io::Error` 来实现，因为它已经实现了 `SinkError`。
/// * 在找到上下文行时要执行的操作。默认情况下，这些操作会被忽略。
/// * 在上下文行之间找到间隔时要执行的操作。默认情况下，这些操作会被忽略。
/// * 在搜索开始时要执行的操作。默认情况下，这些操作不执行任何操作。
/// * 在搜索成功完成时要执行的操作。默认情况下，这些操作不执行任何操作。
///
/// 调用者至少必须指定在发生错误时的行为以及在找到匹配项时的行为。其余的是可选的。
/// 对于每个行为，调用者可以报告错误（例如，如果写入结果到另一个位置失败），
/// 或者如果他们希望搜索停止（例如，在实现对要显示的搜索结果数量的限制时），
/// 则可以简单地返回 `false`。
///
/// 当错误被报告时（无论是在搜索器中还是在 `Sink` 的实现中），
/// 搜索器会立即退出而不会调用 `finish`。
///
/// 对于 `Sink` 的更简单用法，调用者可以选择使用 [`sinks`](sinks/index.html) 模块中更方便但不太灵活的实现之一。
pub trait Sink {
    /// 应由搜索器报告的错误类型。
    ///
    /// 此类型的错误不仅在此 trait 的方法中返回，还在 `SinkError` 中定义的构造函数中使用。
    /// 例如，当从文件中读取数据时发生 I/O 错误。
    type Error: SinkError;

    /// 每当找到匹配项时，将调用此方法。
    ///
    /// 如果搜索器启用了多行模式，则此处报告的匹配项可能跨越多行，并且可能包含多个匹配项。
    /// 当禁用多行模式时，确保匹配项仅跨足够多的行（至少为一个行终止符）。
    ///
    /// 如果返回 `true`，则继续搜索。如果返回 `false`，则立即停止搜索并调用 `finish`。
    ///
    /// 如果返回错误，则立即停止搜索，不会调用 `finish`，
    /// 并且错误会向上传播到搜索器的调用者。
    fn matched(
        &mut self,
        _searcher: &Searcher,
        _mat: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error>;

    /// 每当找到上下文行时，将调用此方法，但是此方法是可选的，可以选择实现。
    /// 默认情况下，它不执行任何操作并返回 `true`。
    ///
    /// 在所有情况下，所提供的上下文都确保仅跨足够多的行（至少为一个行终止符）。
    ///
    /// 如果返回 `true`，则继续搜索。如果返回 `false`，则立即停止搜索并调用 `finish`。
    ///
    /// 如果返回错误，则立即停止搜索，不会调用 `finish`，
    /// 并且错误会向上传播到搜索器的调用者。
    #[inline]
    fn context(
        &mut self,
        _searcher: &Searcher,
        _context: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    /// 每当找到上下文行之间的间隔时，将调用此方法，但是此方法是可选的，可以选择实现。
    /// 默认情况下，它不执行任何操作并返回 `true`。
    ///
    /// 仅当启用上下文报告时（即 `before_context` 或 `after_context` 中的任一个大于 `0`）
    /// 才会出现间隔。更准确地说，间隔在非连续的行组之间出现。
    ///
    /// 如果返回 `true`，则继续搜索。如果返回 `false`，则立即停止搜索并调用 `finish`。
    ///
    /// 如果返回错误，则立即停止搜索，不会调用 `finish`，
    /// 并且错误会向上传播到搜索器的调用者。
    #[inline]
    fn context_break(
        &mut self,
        _searcher: &Searcher,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    /// 每当启用二进制检测并找到二进制数据时，将调用此方法。如果找到二进制数据，
    /// 则至少会为第一次出现调用一次，参数是二进制数据开始的绝对字节偏移量。
    ///
    /// 如果返回 `true`，则继续搜索。如果返回 `false`，则立即停止搜索并调用 `finish`。
    ///
    /// 如果返回错误，则立即停止搜索，不会调用 `finish`，
    /// 并且错误会向上传播到搜索器的调用者。
    ///
    /// 默认情况下，不执行任何操作并返回 `true`。
    #[inline]
    fn binary_data(
        &mut self,
        _searcher: &Searcher,
        _binary_byte_offset: u64,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    /// 在搜索开始时调用此方法，在执行任何搜索之前调用。默认情况下，不执行任何操作。
    ///
    /// 如果返回 `true`，则继续搜索。如果返回 `false`，则立即停止搜索并调用 `finish`。
    ///
    /// 如果返回错误，则立即停止搜索，不会调用 `finish`，
    /// 并且错误会向上传播到搜索器的调用者。
    #[inline]
    fn begin(&mut self, _searcher: &Searcher) -> Result<bool, Self::Error> {
        Ok(true)
    }

    /// 当搜索完成时调用此方法。默认情况下，不执行任何操作。
    ///
    /// 如果返回错误，则错误会向上传播到搜索器的调用者。
    #[inline]
    fn finish(
        &mut self,
        _searcher: &Searcher,
        _: &SinkFinish,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}
/// 为实现了 `Sink` 的类型定义一个代理 `Sink`，使其实现可以通过引用进行调用。
///
/// 这允许对 `Sink` 的引用进行调用，而无需复制其所有权。
impl<'a, S: Sink> Sink for &'a mut S {
    type Error = S::Error;

    /// 将 `matched` 方法委托给实际的 `Sink` 实现。
    #[inline]
    fn matched(
        &mut self,
        searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, S::Error> {
        (**self).matched(searcher, mat)
    }

    /// 将 `context` 方法委托给实际的 `Sink` 实现。
    #[inline]
    fn context(
        &mut self,
        searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, S::Error> {
        (**self).context(searcher, context)
    }

    /// 将 `context_break` 方法委托给实际的 `Sink` 实现。
    #[inline]
    fn context_break(
        &mut self,
        searcher: &Searcher,
    ) -> Result<bool, S::Error> {
        (**self).context_break(searcher)
    }

    /// 将 `binary_data` 方法委托给实际的 `Sink` 实现。
    #[inline]
    fn binary_data(
        &mut self,
        searcher: &Searcher,
        binary_byte_offset: u64,
    ) -> Result<bool, S::Error> {
        (**self).binary_data(searcher, binary_byte_offset)
    }

    /// 将 `begin` 方法委托给实际的 `Sink` 实现。
    #[inline]
    fn begin(&mut self, searcher: &Searcher) -> Result<bool, S::Error> {
        (**self).begin(searcher)
    }

    /// 将 `finish` 方法委托给实际的 `Sink` 实现。
    #[inline]
    fn finish(
        &mut self,
        searcher: &Searcher,
        sink_finish: &SinkFinish,
    ) -> Result<(), S::Error> {
        (**self).finish(searcher, sink_finish)
    }
}

/// 为实现了 `Sink` 的类型定义一个代理 `Sink`，使其实现可以通过 `Box` 进行调用。
///
/// 这允许对 `Sink` 的实现进行堆分配，并在需要时进行所有权转移。
impl<S: Sink + ?Sized> Sink for Box<S> {
    type Error = S::Error;

    /// 将 `matched` 方法委托给实际的 `Sink` 实现。
    #[inline]
    fn matched(
        &mut self,
        searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, S::Error> {
        (**self).matched(searcher, mat)
    }

    /// 将 `context` 方法委托给实际的 `Sink` 实现。
    #[inline]
    fn context(
        &mut self,
        searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, S::Error> {
        (**self).context(searcher, context)
    }

    /// 将 `context_break` 方法委托给实际的 `Sink` 实现。
    #[inline]
    fn context_break(
        &mut self,
        searcher: &Searcher,
    ) -> Result<bool, S::Error> {
        (**self).context_break(searcher)
    }

    /// 将 `binary_data` 方法委托给实际的 `Sink` 实现。
    #[inline]
    fn binary_data(
        &mut self,
        searcher: &Searcher,
        binary_byte_offset: u64,
    ) -> Result<bool, S::Error> {
        (**self).binary_data(searcher, binary_byte_offset)
    }

    /// 将 `begin` 方法委托给实际的 `Sink` 实现。
    #[inline]
    fn begin(&mut self, searcher: &Searcher) -> Result<bool, S::Error> {
        (**self).begin(searcher)
    }

    /// 将 `finish` 方法委托给实际的 `Sink` 实现。
    #[inline]
    fn finish(
        &mut self,
        searcher: &Searcher,
        sink_finish: &SinkFinish,
    ) -> Result<(), S::Error> {
        (**self).finish(searcher, sink_finish)
    }
}

/// 在搜索结束时报告的摘要数据。
///
/// 这些数据包括总搜索的字节数以及如果发现了二进制数据，则第一个字节的绝对偏移量。
///
/// 由于出现错误而提前停止的搜索器不会调用 `finish`。
/// 因为 `Sink` 实现者的指示而提前停止的搜索器仍将调用 `finish`。
#[derive(Clone, Debug)]
pub struct SinkFinish {
    pub(crate) byte_count: u64,
    pub(crate) binary_byte_offset: Option<u64>,
}

impl SinkFinish {
    /// 返回总搜索的字节数。
    #[inline]
    pub fn byte_count(&self) -> u64 {
        self.byte_count
    }

    /// 如果启用了二进制检测，并且找到了二进制数据，则返回检测到的二进制数据的第一个字节的绝对偏移量。
    ///
    /// 请注意，由于这是一个绝对字节偏移量，因此不能依赖它来索引任何可寻址的内存。
    #[inline]
    pub fn binary_byte_offset(&self) -> Option<u64> {
        self.binary_byte_offset
    }
}

/// 描述搜索器报告的匹配项的类型。
#[derive(Clone, Debug)]
pub struct SinkMatch<'b> {
    pub(crate) line_term: LineTerminator,
    pub(crate) bytes: &'b [u8],
    pub(crate) absolute_byte_offset: u64,
    pub(crate) line_number: Option<u64>,
    pub(crate) buffer: &'b [u8],
    pub(crate) bytes_range_in_buffer: std::ops::Range<usize>,
}

impl<'b> SinkMatch<'b> {
    /// 返回所有匹配行的字节，包括行终止符（如果存在）。
    #[inline]
    pub fn bytes(&self) -> &'b [u8] {
        self.bytes
    }

    /// 返回此匹配中的行的迭代器。
    ///
    /// 如果启用了多行搜索，则可能会产生多行（但始终至少为一行）。
    /// 如果禁用了多行搜索，则始终报告一行（但可能只包括行终止符）。
    ///
    /// 此迭代器生成的行包括其终止符。
    #[inline]
    pub fn lines(&self) -> LineIter<'b> {
        LineIter::new(self.line_term.as_byte(), self.bytes)
    }

    /// 返回此匹配的开始处的绝对字节偏移量。此偏移量是绝对的，
    /// 因为它相对于搜索中的开头，绝不可依赖于将其用作可寻址内存的索引。
    #[inline]
    pub fn absolute_byte_offset(&self) -> u64 {
        self.absolute_byte_offset
    }

    /// 返回此匹配中的第一行的行号（如果可用）。
    ///
    /// 仅当搜索生成器被指示计算行号时，才可用。
    #[inline]
    pub fn line_number(&self) -> Option<u64> {
        self.line_number
    }

    /// TODO
    #[inline]
    pub fn buffer(&self) -> &'b [u8] {
        self.buffer
    }

    /// TODO
    #[inline]
    pub fn bytes_range_in_buffer(&self) -> std::ops::Range<usize> {
        self.bytes_range_in_buffer.clone()
    }
}
/// 描述搜索器报告的上下文类型。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SinkContextKind {
    /// 报告的行出现在匹配项之前。
    Before,
    /// 报告的行出现在匹配项之后。
    After,
    /// 报告的其他类型的上下文，例如搜索器的“透传”模式的结果。
    Other,
}

/// 描述搜索器报告的上下文行的类型。
#[derive(Clone, Debug)]
pub struct SinkContext<'b> {
    #[cfg(test)]
    pub(crate) line_term: LineTerminator,
    pub(crate) bytes: &'b [u8],
    pub(crate) kind: SinkContextKind,
    pub(crate) absolute_byte_offset: u64,
    pub(crate) line_number: Option<u64>,
}

impl<'b> SinkContext<'b> {
    /// 返回上下文字节，包括行终止符。
    #[inline]
    pub fn bytes(&self) -> &'b [u8] {
        self.bytes
    }

    /// 返回上下文类型。
    #[inline]
    pub fn kind(&self) -> &SinkContextKind {
        &self.kind
    }

    /// 返回匹配的行的迭代器。
    ///
    /// 这始终只生成一行（该行可能只包含行终止符）。
    ///
    /// 此迭代器生成的行包括其终止符。
    #[cfg(test)]
    pub(crate) fn lines(&self) -> LineIter<'b> {
        LineIter::new(self.line_term.as_byte(), self.bytes)
    }

    /// 返回此上下文的开始处的绝对字节偏移量。此偏移量是绝对的，
    /// 因为它相对于搜索中的开头，绝不可依赖于将其用作可寻址内存的索引。
    #[inline]
    pub fn absolute_byte_offset(&self) -> u64 {
        self.absolute_byte_offset
    }

    /// 返回此上下文中第一行的行号（如果可用）。
    ///
    /// 仅当搜索生成器被指示计算行号时，才可用。
    #[inline]
    pub fn line_number(&self) -> Option<u64> {
        self.line_number
    }
}

/// `sinks` 模块提供的 `Sink` 的便捷实现集合。
///
/// 此模块中的每个实现在某种程度上都以某种牺牲的方式，以便于使用常见情况。
/// 最常见的是，每个类型都是调用者指定的闭包的包装，该闭包提供对 `Sink` 的完整信息的有限访问。
///
/// 例如，`UTF8` 汇聚了以下牺牲：
///
/// * 所有匹配必须是 UTF-8。任意的 `Sink` 并没有此限制，可以处理任意数据。
///   如果此汇聚遇到无效的 UTF-8，则返回错误并停止搜索。
///   （使用 `Lossy` 汇聚以代替可抑制此错误。）
/// * 搜索器必须配置为报告行号。如果未配置，将在第一个匹配处报告错误并停止搜索。
/// * 忽略在上下文行、上下文中断以及搜索结束时报告的摘要数据。
/// * 实现者被强制使用 `io::Error` 作为其错误类型。
///
/// 如果需要更大的灵活性，则建议直接实现 `Sink` trait。
pub mod sinks {
    use std::io;
    use std::str;

    use super::{Sink, SinkError, SinkMatch};
    use crate::searcher::Searcher;

    /// 一种汇聚，提供了行号和匹配项作为字符串，并忽略其他内容。
    ///
    /// 如果匹配项包含无效的 UTF-8 或搜索器未配置为计数行号，则此实现将返回错误。
    /// 可以通过使用 `Lossy` 汇聚来抑制无效 UTF-8 上的错误。
    ///
    /// 闭包接受两个参数：行号和包含匹配数据的 UTF-8 字符串。
    /// 闭包返回一个 `Result<bool, io::Error>`。如果 `bool` 为 `false`，
    /// 则搜索立即停止。否则，继续搜索。
    ///
    /// 如果启用了多行模式，则行号指的是匹配中第一行的行号。
    #[derive(Clone, Debug)]
    pub struct UTF8<F>(pub F)
    where
        F: FnMut(u64, &str) -> Result<bool, io::Error>;

    impl<F> Sink for UTF8<F>
    where
        F: FnMut(u64, &str) -> Result<bool, io::Error>,
    {
        type Error = io::Error;

        fn matched(
            &mut self,
            _searcher: &Searcher,
            mat: &SinkMatch<'_>,
        ) -> Result<bool, io::Error> {
            let matched = match str::from_utf8(mat.bytes()) {
                Ok(matched) => matched,
                Err(err) => return Err(io::Error::error_message(err)),
            };
            let line_number = match mat.line_number() {
                Some(line_number) => line_number,
                None => {
                    let msg = "未启用行号";
                    return Err(io::Error::error_message(msg));
                }
            };
            (self.0)(line_number, &matched)
        }
    }

    /// 一种汇聚，提供了行号和匹配项作为（通过丢失转换）的字符串，并忽略其他内容。
    ///
    /// 这类似于 `UTF8`，但是如果匹配项包含无效的 UTF-8，则会将其转换为有效的 UTF-8，
    /// 方法是用 Unicode 替换字符替换无效的 UTF-8。
    ///
    /// 如果匹配项包含无效的 UTF-8 或搜索器未配置为计数行号，则此实现将在第一个匹配处返回错误。
    ///
    /// 闭包接受两个参数：行号和包含匹配数据的 UTF-8 字符串。
    /// 闭包返回一个 `Result<bool, io::Error>`。如果 `bool` 为 `false`，
    /// 则搜索立即停止。否则，继续搜索。
    ///
    /// 如果启用了多行模式，则行号指的是匹配中第一行的行号。
    #[derive(Clone, Debug)]
    pub struct Lossy<F>(pub F)
    where
        F: FnMut(u64, &str) -> Result<bool, io::Error>;

    impl<F> Sink for Lossy<F>
    where
        F: FnMut(u64, &str) -> Result<bool, io::Error>,
    {
        type Error = io::Error;

        fn matched(
            &mut self,
            _searcher: &Searcher,
            mat: &SinkMatch<'_>,
        ) -> Result<bool, io::Error> {
            use std::borrow::Cow;

            let matched = match str::from_utf8(mat.bytes()) {
                Ok(matched) => Cow::Borrowed(matched),
                // TODO: 理论上，可以在此处摊销分配，但是 `std` 没有提供这样的 API。
                // 不过，这仅在具有无效 UTF-8 的匹配上发生，这应该非常少见。
                Err(_) => String::from_utf8_lossy(mat.bytes()),
            };
            let line_number = match mat.line_number() {
                Some(line_number) => line_number,
                None => {
                    let msg = "未启用行号";
                    return Err(io::Error::error_message(msg));
                }
            };
            (self.0)(line_number, &matched)
        }
    }

    /// 一种汇聚，提供了行号和匹配项作为原始字节，忽略其他内容。
    ///
    /// 如果搜索器未配置为计数行号，则此实现将在第一个匹配处返回错误。
    ///
    /// 闭包接受两个参数：行号和包含匹配数据的原始字节字符串。
    /// 闭包返回一个 `Result<bool, io::Error>`。如果 `bool` 为 `false`，
    /// 则搜索立即停止。否则，继续搜索。
    ///
    /// 如果启用了多行模式，则行号指的是匹配中第一行的行号。
    #[derive(Clone, Debug)]
    pub struct Bytes<F>(pub F)
    where
        F: FnMut(u64, &[u8]) -> Result<bool, io::Error>;

    impl<F> Sink for Bytes<F>
    where
        F: FnMut(u64, &[u8]) -> Result<bool, io::Error>,
    {
        type Error = io::Error;

        fn matched(
            &mut self,
            _searcher: &Searcher,
            mat: &SinkMatch<'_>,
        ) -> Result<bool, io::Error> {
            let line_number = match mat.line_number() {
                Some(line_number) => line_number,
                None => {
                    let msg = "未启用行号";
                    return Err(io::Error::error_message(msg));
                }
            };
            (self.0)(line_number, mat.bytes())
        }
    }
}
