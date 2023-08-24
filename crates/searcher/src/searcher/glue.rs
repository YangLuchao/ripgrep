use std::cmp;
use std::io;

use crate::line_buffer::{LineBufferReader, DEFAULT_BUFFER_CAPACITY};
use crate::lines::{self, LineStep};
use crate::sink::{Sink, SinkError};
use grep_matcher::Matcher;

use crate::searcher::core::Core;
use crate::searcher::{Config, Range, Searcher};

#[derive(Debug)]
pub struct ReadByLine<'s, M, R, S> {
    config: &'s Config,
    core: Core<'s, M, S>,
    rdr: LineBufferReader<'s, R>,
}
/// `ReadByLine` 结构体的实现，实现了逐行读取并进行匹配的功能。
impl<'s, M, R, S> ReadByLine<'s, M, R, S>
where
    M: Matcher,
    R: io::Read,
    S: Sink,
{
    /// 创建一个新的 `ReadByLine` 实例。
    pub fn new(
        searcher: &'s Searcher,
        matcher: M,
        read_from: LineBufferReader<'s, R>,
        write_to: S,
    ) -> ReadByLine<'s, M, R, S> {
        debug_assert!(!searcher.multi_line_with_matcher(&matcher));

        ReadByLine {
            config: &searcher.config,
            core: Core::new(searcher, matcher, write_to, false),
            rdr: read_from,
        }
    }

    /// 执行匹配过程，逐行读取并进行匹配。
    pub fn run(mut self) -> Result<(), S::Error> {
        // 开始匹配过程，检查是否可以继续。
        if self.core.begin()? {
            // 逐行处理输入数据，直到没有数据可读。
            while self.fill()? && self.core.match_by_line(self.rdr.buffer())? {
            }
        }
        // 结束匹配过程，将剩余数据下沉到输出。
        self.core.finish(
            self.rdr.absolute_byte_offset(),
            self.rdr.binary_byte_offset(),
        )
    }

    /// 填充输入缓冲区，返回是否成功填充数据。
    fn fill(&mut self) -> Result<bool, S::Error> {
        assert!(self.rdr.buffer()[self.core.pos()..].is_empty());

        let already_binary = self.rdr.binary_byte_offset().is_some();
        let old_buf_len = self.rdr.buffer().len();
        let consumed = self.core.roll(self.rdr.buffer());
        self.rdr.consume(consumed);
        let didread = match self.rdr.fill() {
            Err(err) => return Err(S::Error::error_io(err)),
            Ok(didread) => didread,
        };
        if !already_binary {
            if let Some(offset) = self.rdr.binary_byte_offset() {
                if !self.core.binary_data(offset)? {
                    return Ok(false);
                }
            }
        }
        if !didread || self.should_binary_quit() {
            return Ok(false);
        }
        // 如果滚动缓冲区未导致任何数据被消耗，
        // 并且重新填充缓冲区也没有添加任何字节，则缓冲区中仅剩余上下文数据，
        // 而此时没有数据可供搜索，所以强制退出。
        if consumed == 0 && old_buf_len == self.rdr.buffer().len() {
            self.rdr.consume(old_buf_len);
            return Ok(false);
        }
        Ok(true)
    }

    /// 判断是否应该在二进制匹配模式下退出匹配过程。
    fn should_binary_quit(&self) -> bool {
        self.rdr.binary_byte_offset().is_some()
            && self.config.binary.quit_byte().is_some()
    }
}

/// `SliceByLine` 结构体的实现，实现了对给定切片进行逐行匹配的功能。
#[derive(Debug)]
pub struct SliceByLine<'s, M, S> {
    core: Core<'s, M, S>,
    slice: &'s [u8],
}

impl<'s, M: Matcher, S: Sink> SliceByLine<'s, M, S> {
    /// 创建一个新的 `SliceByLine` 实例。
    pub fn new(
        searcher: &'s Searcher,
        matcher: M,
        slice: &'s [u8],
        write_to: S,
    ) -> SliceByLine<'s, M, S> {
        debug_assert!(!searcher.multi_line_with_matcher(&matcher));

        SliceByLine {
            core: Core::new(searcher, matcher, write_to, true),
            slice: slice,
        }
    }

    /// 执行匹配过程，对给定切片进行逐行匹配。
    pub fn run(mut self) -> Result<(), S::Error> {
        // 开始匹配过程，检查是否可以继续。
        if self.core.begin()? {
            // 首先尝试检测是否为二进制数据，如果不是则逐行匹配。
            let binary_upto =
                cmp::min(self.slice.len(), DEFAULT_BUFFER_CAPACITY);
            let binary_range = Range::new(0, binary_upto);
            if !self.core.detect_binary(self.slice, &binary_range)? {
                while !self.slice[self.core.pos()..].is_empty()
                    && self.core.match_by_line(self.slice)?
                {}
            }
        }
        // 结束匹配过程，将剩余数据下沉到输出。
        let byte_count = self.byte_count();
        let binary_byte_offset = self.core.binary_byte_offset();
        self.core.finish(byte_count, binary_byte_offset)
    }

    /// 计算要下沉的字节数。
    fn byte_count(&mut self) -> u64 {
        match self.core.binary_byte_offset() {
            Some(offset) if offset < self.core.pos() as u64 => offset,
            _ => self.core.pos() as u64,
        }
    }
}

/// `MultiLine` 结构体的实现，实现了多行匹配的功能。
#[derive(Debug)]
pub struct MultiLine<'s, M, S> {
    config: &'s Config,
    core: Core<'s, M, S>,
    slice: &'s [u8],
    last_match: Option<Range>,
}

/// `MultiLine` 结构体的实现，实现了多行匹配的功能。
impl<'s, M: Matcher, S: Sink> MultiLine<'s, M, S> {
    /// 创建一个新的 `MultiLine` 实例。
    pub fn new(
        searcher: &'s Searcher,
        matcher: M,
        slice: &'s [u8],
        write_to: S,
    ) -> MultiLine<'s, M, S> {
        debug_assert!(searcher.multi_line_with_matcher(&matcher));

        MultiLine {
            config: &searcher.config,
            core: Core::new(searcher, matcher, write_to, true),
            slice: slice,
            last_match: None,
        }
    }

    /// 执行多行匹配过程。
    pub fn run(mut self) -> Result<(), S::Error> {
        // 开始匹配过程，检查是否可以继续。
        if self.core.begin()? {
            // 首先尝试检测是否为二进制数据，如果不是则进行多行匹配。
            let binary_upto =
                cmp::min(self.slice.len(), DEFAULT_BUFFER_CAPACITY);
            let binary_range = Range::new(0, binary_upto);
            if !self.core.detect_binary(self.slice, &binary_range)? {
                let mut keepgoing = true;
                while !self.slice[self.core.pos()..].is_empty() && keepgoing {
                    keepgoing = self.sink()?;
                }
                if keepgoing {
                    keepgoing = match self.last_match.take() {
                        None => true,
                        Some(last_match) => {
                            if self.sink_context(&last_match)? {
                                self.sink_matched(&last_match)?;
                            }
                            true
                        }
                    };
                }
                // 处理最后一个匹配后的剩余上下文。
                if keepgoing {
                    if self.config.passthru {
                        self.core.other_context_by_line(
                            self.slice,
                            self.slice.len(),
                        )?;
                    } else {
                        self.core.after_context_by_line(
                            self.slice,
                            self.slice.len(),
                        )?;
                    }
                }
            }
        }
        // 结束匹配过程，将剩余数据下沉到输出。
        let byte_count = self.byte_count();
        let binary_byte_offset = self.core.binary_byte_offset();
        self.core.finish(byte_count, binary_byte_offset)
    }

    /// 处理匹配的下沉过程。
    fn sink(&mut self) -> Result<bool, S::Error> {
        if self.config.invert_match {
            return self.sink_matched_inverted();
        }
        let mat = match self.find()? {
            Some(range) => range,
            None => {
                self.core.set_pos(self.slice.len());
                return Ok(true);
            }
        };
        self.advance(&mat);

        let line =
            lines::locate(self.slice, self.config.line_term.as_byte(), mat);
        // 为了确保相邻匹配能够合并到一起，我们会延迟下沉匹配结果。
        // 相邻匹配是指在同一行上开始和结束的不同匹配。
        // 这确保了每一行最多只会下沉一次。
        match self.last_match.take() {
            None => {
                self.last_match = Some(line);
                Ok(true)
            }
            Some(last_match) => {
                // 如果前一个匹配的行与当前匹配的行有重叠部分，
                // 则只需要扩展匹配范围并继续匹配。
                // 这种情况发生在下一个匹配从上一个匹配的结束行开始。
                //
                // 需要注意的是，严格的重叠在此并不是必要的。
                // 我们只需要确保两行是相邻的即可。
                // 这样做能够为打印机提供更大的文本块，
                // 并且在处理替换时会表现更好。
                //
                // 参考：https://github.com/BurntSushi/ripgrep/issues/1311
                // 以及解决 #1311 的提交记录。
                if last_match.end() >= line.start() {
                    self.last_match = Some(last_match.with_end(line.end()));
                    Ok(true)
                } else {
                    self.last_match = Some(line);
                    if !self.sink_context(&last_match)? {
                        return Ok(false);
                    }
                    self.sink_matched(&last_match)
                }
            }
        }
    }

    /// 处理反转匹配下沉过程。
    fn sink_matched_inverted(&mut self) -> Result<bool, S::Error> {
        assert!(self.config.invert_match);

        let invert_match = match self.find()? {
            None => {
                let range = Range::new(self.core.pos(), self.slice.len());
                self.core.set_pos(range.end());
                range
            }
            Some(mat) => {
                let line = lines::locate(
                    self.slice,
                    self.config.line_term.as_byte(),
                    mat,
                );
                let range = Range::new(self.core.pos(), line.start());
                self.advance(&line);
                range
            }
        };
        if invert_match.is_empty() {
            return Ok(true);
        }
        if !self.sink_context(&invert_match)? {
            return Ok(false);
        }
        let mut stepper = LineStep::new(
            self.config.line_term.as_byte(),
            invert_match.start(),
            invert_match.end(),
        );
        while let Some(line) = stepper.next_match(self.slice) {
            if !self.sink_matched(&line)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// 处理匹配下沉过程。
    fn sink_matched(&mut self, range: &Range) -> Result<bool, S::Error> {
        if range.is_empty() {
            // 如果匹配结果是空行，说明我们匹配的是刚好在搜索末尾的位置，
            // 而且这个位置也是行终止符的位置。
            // 我们不想报告这样的匹配，而且此时搜索已经结束，停止继续搜索。
            return Ok(false);
        }
        self.core.matched(self.slice, range)
    }

    /// 处理上下文下沉过程。
    fn sink_context(&mut self, range: &Range) -> Result<bool, S::Error> {
        if self.config.passthru {
            if !self.core.other_context_by_line(self.slice, range.start())? {
                return Ok(false);
            }
        } else {
            if !self.core.after_context_by_line(self.slice, range.start())? {
                return Ok(false);
            }
            if !self.core.before_context_by_line(self.slice, range.start())? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// 查找下一个匹配的位置。
    fn find(&mut self) -> Result<Option<Range>, S::Error> {
        match self.core.matcher().find(&self.slice[self.core.pos()..]) {
            Err(err) => Err(S::Error::error_message(err)),
            Ok(None) => Ok(None),
            Ok(Some(m)) => Ok(Some(m.offset(self.core.pos()))),
        }
    }

    /// 根据上一个匹配的范围调整搜索位置。
    ///
    /// 如果上一个匹配是零宽度的，将搜索位置调整到匹配结束的下一个字节位置。
    fn advance(&mut self, range: &Range) {
        self.core.set_pos(range.end());
        if range.is_empty() && self.core.pos() < self.slice.len() {
            let newpos = self.core.pos() + 1;
            self.core.set_pos(newpos);
        }
    }

    /// 计算输出的字节总数。
    fn byte_count(&mut self) -> u64 {
        match self.core.binary_byte_offset() {
            Some(offset) if offset < self.core.pos() as u64 => offset,
            _ => self.core.pos() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::searcher::{BinaryDetection, SearcherBuilder};
    use crate::testutil::{KitchenSink, RegexMatcher, SearcherTester};

    use super::*;

    const SHERLOCK: &'static str = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
be, to a very large extent, the result of luck. Sherlock Holmes
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.\
";

    const CODE: &'static str = "\
extern crate snap;

use std::io;

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();

    // Wrap the stdin reader in a Snappy reader.
    let mut rdr = snap::Reader::new(stdin.lock());
    let mut wtr = stdout.lock();
    io::copy(&mut rdr, &mut wtr).expect(\"I/O operation failed\");
}
";

    #[test]
    fn basic1() {
        let exp = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
129:be, to a very large extent, the result of luck. Sherlock Holmes

byte count:366
";
        SearcherTester::new(SHERLOCK, "Sherlock")
            .line_number(false)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn basic2() {
        let exp = "\nbyte count:366\n";
        SearcherTester::new(SHERLOCK, "NADA")
            .line_number(false)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn basic3() {
        let exp = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
65:Holmeses, success in the province of detective work must always
129:be, to a very large extent, the result of luck. Sherlock Holmes
193:can extract a clew from a wisp of straw or a flake of cigar ash;
258:but Doctor Watson has to have it taken out for him and dusted,
321:and exhibited clearly, with a label attached.
byte count:366
";
        SearcherTester::new(SHERLOCK, "a")
            .line_number(false)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn basic4() {
        let haystack = "\
a
b

c


d
";
        let byte_count = haystack.len();
        let exp = format!("0:a\n\nbyte count:{}\n", byte_count);
        SearcherTester::new(haystack, "a")
            .line_number(false)
            .expected_no_line_number(&exp)
            .test();
    }

    #[test]
    fn invert1() {
        let exp = "\
65:Holmeses, success in the province of detective work must always
193:can extract a clew from a wisp of straw or a flake of cigar ash;
258:but Doctor Watson has to have it taken out for him and dusted,
321:and exhibited clearly, with a label attached.
byte count:366
";
        SearcherTester::new(SHERLOCK, "Sherlock")
            .line_number(false)
            .invert_match(true)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn line_number1() {
        let exp = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
129:be, to a very large extent, the result of luck. Sherlock Holmes

byte count:366
";
        let exp_line = "\
1:0:For the Doctor Watsons of this world, as opposed to the Sherlock
3:129:be, to a very large extent, the result of luck. Sherlock Holmes

byte count:366
";
        SearcherTester::new(SHERLOCK, "Sherlock")
            .expected_no_line_number(exp)
            .expected_with_line_number(exp_line)
            .test();
    }

    #[test]
    fn line_number_invert1() {
        let exp = "\
65:Holmeses, success in the province of detective work must always
193:can extract a clew from a wisp of straw or a flake of cigar ash;
258:but Doctor Watson has to have it taken out for him and dusted,
321:and exhibited clearly, with a label attached.
byte count:366
";
        let exp_line = "\
2:65:Holmeses, success in the province of detective work must always
4:193:can extract a clew from a wisp of straw or a flake of cigar ash;
5:258:but Doctor Watson has to have it taken out for him and dusted,
6:321:and exhibited clearly, with a label attached.
byte count:366
";
        SearcherTester::new(SHERLOCK, "Sherlock")
            .invert_match(true)
            .expected_no_line_number(exp)
            .expected_with_line_number(exp_line)
            .test();
    }

    #[test]
    fn multi_line_overlap1() {
        let haystack = "xxx\nabc\ndefxxxabc\ndefxxx\nxxx";
        let byte_count = haystack.len();
        let exp = format!(
            "4:abc\n8:defxxxabc\n18:defxxx\n\nbyte count:{}\n",
            byte_count
        );

        SearcherTester::new(haystack, "abc\ndef")
            .by_line(false)
            .line_number(false)
            .expected_no_line_number(&exp)
            .test();
    }

    #[test]
    fn multi_line_overlap2() {
        let haystack = "xxx\nabc\ndefabc\ndefxxx\nxxx";
        let byte_count = haystack.len();
        let exp = format!(
            "4:abc\n8:defabc\n15:defxxx\n\nbyte count:{}\n",
            byte_count
        );

        SearcherTester::new(haystack, "abc\ndef")
            .by_line(false)
            .line_number(false)
            .expected_no_line_number(&exp)
            .test();
    }

    #[test]
    fn empty_line1() {
        let exp = "\nbyte count:0\n";
        SearcherTester::new("", r"^$")
            .expected_no_line_number(exp)
            .expected_with_line_number(exp)
            .test();
    }

    #[test]
    fn empty_line2() {
        let exp = "0:\n\nbyte count:1\n";
        let exp_line = "1:0:\n\nbyte count:1\n";

        SearcherTester::new("\n", r"^$")
            .expected_no_line_number(exp)
            .expected_with_line_number(exp_line)
            .test();
    }

    #[test]
    fn empty_line3() {
        let exp = "0:\n1:\n\nbyte count:2\n";
        let exp_line = "1:0:\n2:1:\n\nbyte count:2\n";

        SearcherTester::new("\n\n", r"^$")
            .expected_no_line_number(exp)
            .expected_with_line_number(exp_line)
            .test();
    }

    #[test]
    fn empty_line4() {
        // See: https://github.com/BurntSushi/ripgrep/issues/441
        let haystack = "\
a
b

c


d
";
        let byte_count = haystack.len();
        let exp = format!("4:\n7:\n8:\n\nbyte count:{}\n", byte_count);
        let exp_line =
            format!("3:4:\n5:7:\n6:8:\n\nbyte count:{}\n", byte_count);

        SearcherTester::new(haystack, r"^$")
            .expected_no_line_number(&exp)
            .expected_with_line_number(&exp_line)
            .test();
    }

    #[test]
    fn empty_line5() {
        // See: https://github.com/BurntSushi/ripgrep/issues/441
        // This is like empty_line4, but lacks the trailing line terminator.
        let haystack = "\
a
b

c


d";
        let byte_count = haystack.len();
        let exp = format!("4:\n7:\n8:\n\nbyte count:{}\n", byte_count);
        let exp_line =
            format!("3:4:\n5:7:\n6:8:\n\nbyte count:{}\n", byte_count);

        SearcherTester::new(haystack, r"^$")
            .expected_no_line_number(&exp)
            .expected_with_line_number(&exp_line)
            .test();
    }

    #[test]
    fn empty_line6() {
        // See: https://github.com/BurntSushi/ripgrep/issues/441
        // This is like empty_line4, but includes an empty line at the end.
        let haystack = "\
a
b

c


d

";
        let byte_count = haystack.len();
        let exp = format!("4:\n7:\n8:\n11:\n\nbyte count:{}\n", byte_count);
        let exp_line =
            format!("3:4:\n5:7:\n6:8:\n8:11:\n\nbyte count:{}\n", byte_count);

        SearcherTester::new(haystack, r"^$")
            .expected_no_line_number(&exp)
            .expected_with_line_number(&exp_line)
            .test();
    }

    #[test]
    fn big1() {
        let mut haystack = String::new();
        haystack.push_str("a\n");
        // Pick an arbitrary number above the capacity.
        for _ in 0..(4 * (DEFAULT_BUFFER_CAPACITY + 7)) {
            haystack.push_str("zzz\n");
        }
        haystack.push_str("a\n");

        let byte_count = haystack.len();
        let exp = format!("0:a\n1048690:a\n\nbyte count:{}\n", byte_count);

        SearcherTester::new(&haystack, "a")
            .line_number(false)
            .expected_no_line_number(&exp)
            .test();
    }

    #[test]
    fn big_error_one_line() {
        let mut haystack = String::new();
        haystack.push_str("a\n");
        // Pick an arbitrary number above the capacity.
        for _ in 0..(4 * (DEFAULT_BUFFER_CAPACITY + 7)) {
            haystack.push_str("zzz\n");
        }
        haystack.push_str("a\n");

        let matcher = RegexMatcher::new("a");
        let mut sink = KitchenSink::new();
        let mut searcher = SearcherBuilder::new()
            .heap_limit(Some(3)) // max line length is 4, one byte short
            .build();
        let result =
            searcher.search_reader(&matcher, haystack.as_bytes(), &mut sink);
        assert!(result.is_err());
    }

    #[test]
    fn big_error_multi_line() {
        let mut haystack = String::new();
        haystack.push_str("a\n");
        // Pick an arbitrary number above the capacity.
        for _ in 0..(4 * (DEFAULT_BUFFER_CAPACITY + 7)) {
            haystack.push_str("zzz\n");
        }
        haystack.push_str("a\n");

        let matcher = RegexMatcher::new("a");
        let mut sink = KitchenSink::new();
        let mut searcher = SearcherBuilder::new()
            .multi_line(true)
            .heap_limit(Some(haystack.len())) // actually need one more byte
            .build();
        let result =
            searcher.search_reader(&matcher, haystack.as_bytes(), &mut sink);
        assert!(result.is_err());
    }

    #[test]
    fn binary1() {
        let haystack = "\x00a";
        let exp = "\nbyte count:0\nbinary offset:0\n";

        SearcherTester::new(haystack, "a")
            .binary_detection(BinaryDetection::quit(0))
            .line_number(false)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn binary2() {
        let haystack = "a\x00";
        let exp = "\nbyte count:0\nbinary offset:1\n";

        SearcherTester::new(haystack, "a")
            .binary_detection(BinaryDetection::quit(0))
            .line_number(false)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn binary3() {
        let mut haystack = String::new();
        haystack.push_str("a\n");
        for _ in 0..DEFAULT_BUFFER_CAPACITY {
            haystack.push_str("zzz\n");
        }
        haystack.push_str("a\n");
        haystack.push_str("zzz\n");
        haystack.push_str("a\x00a\n");
        haystack.push_str("zzz\n");
        haystack.push_str("a\n");

        // The line buffered searcher has slightly different semantics here.
        // Namely, it will *always* detect binary data in the current buffer
        // before searching it. Thus, the total number of bytes searched is
        // smaller than below.
        let exp = "0:a\n\nbyte count:262146\nbinary offset:262153\n";
        // In contrast, the slice readers (for multi line as well) will only
        // look for binary data in the initial chunk of bytes. After that
        // point, it only looks for binary data in matches. Note though that
        // the binary offset remains the same. (See the binary4 test for a case
        // where the offset is explicitly different.)
        let exp_slice =
            "0:a\n262146:a\n\nbyte count:262153\nbinary offset:262153\n";

        SearcherTester::new(&haystack, "a")
            .binary_detection(BinaryDetection::quit(0))
            .line_number(false)
            .auto_heap_limit(false)
            .expected_no_line_number(exp)
            .expected_slice_no_line_number(exp_slice)
            .test();
    }

    #[test]
    fn binary4() {
        let mut haystack = String::new();
        haystack.push_str("a\n");
        for _ in 0..DEFAULT_BUFFER_CAPACITY {
            haystack.push_str("zzz\n");
        }
        haystack.push_str("a\n");
        // The Read searcher will detect binary data here, but since this is
        // beyond the initial buffer size and doesn't otherwise contain a
        // match, the Slice reader won't detect the binary data until the next
        // line (which is a match).
        haystack.push_str("b\x00b\n");
        haystack.push_str("a\x00a\n");
        haystack.push_str("a\n");

        let exp = "0:a\n\nbyte count:262146\nbinary offset:262149\n";
        // The binary offset for the Slice readers corresponds to the binary
        // data in `a\x00a\n` since the first line with binary data
        // (`b\x00b\n`) isn't part of a match, and is therefore undetected.
        let exp_slice =
            "0:a\n262146:a\n\nbyte count:262153\nbinary offset:262153\n";

        SearcherTester::new(&haystack, "a")
            .binary_detection(BinaryDetection::quit(0))
            .line_number(false)
            .auto_heap_limit(false)
            .expected_no_line_number(exp)
            .expected_slice_no_line_number(exp_slice)
            .test();
    }

    #[test]
    fn passthru_sherlock1() {
        let exp = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
65-Holmeses, success in the province of detective work must always
129:be, to a very large extent, the result of luck. Sherlock Holmes
193-can extract a clew from a wisp of straw or a flake of cigar ash;
258-but Doctor Watson has to have it taken out for him and dusted,
321-and exhibited clearly, with a label attached.
byte count:366
";
        SearcherTester::new(SHERLOCK, "Sherlock")
            .passthru(true)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn passthru_sherlock_invert1() {
        let exp = "\
0-For the Doctor Watsons of this world, as opposed to the Sherlock
65:Holmeses, success in the province of detective work must always
129-be, to a very large extent, the result of luck. Sherlock Holmes
193:can extract a clew from a wisp of straw or a flake of cigar ash;
258:but Doctor Watson has to have it taken out for him and dusted,
321:and exhibited clearly, with a label attached.
byte count:366
";
        SearcherTester::new(SHERLOCK, "Sherlock")
            .passthru(true)
            .line_number(false)
            .invert_match(true)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn context_sherlock1() {
        let exp = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
65-Holmeses, success in the province of detective work must always
129:be, to a very large extent, the result of luck. Sherlock Holmes
193-can extract a clew from a wisp of straw or a flake of cigar ash;

byte count:366
";
        let exp_lines = "\
1:0:For the Doctor Watsons of this world, as opposed to the Sherlock
2-65-Holmeses, success in the province of detective work must always
3:129:be, to a very large extent, the result of luck. Sherlock Holmes
4-193-can extract a clew from a wisp of straw or a flake of cigar ash;

byte count:366
";
        // before and after + line numbers
        SearcherTester::new(SHERLOCK, "Sherlock")
            .after_context(1)
            .before_context(1)
            .line_number(true)
            .expected_no_line_number(exp)
            .expected_with_line_number(exp_lines)
            .test();

        // after
        SearcherTester::new(SHERLOCK, "Sherlock")
            .after_context(1)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();

        // before
        let exp = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
65-Holmeses, success in the province of detective work must always
129:be, to a very large extent, the result of luck. Sherlock Holmes

byte count:366
";
        SearcherTester::new(SHERLOCK, "Sherlock")
            .before_context(1)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn context_sherlock_invert1() {
        let exp = "\
0-For the Doctor Watsons of this world, as opposed to the Sherlock
65:Holmeses, success in the province of detective work must always
129-be, to a very large extent, the result of luck. Sherlock Holmes
193:can extract a clew from a wisp of straw or a flake of cigar ash;
258:but Doctor Watson has to have it taken out for him and dusted,
321:and exhibited clearly, with a label attached.
byte count:366
";
        let exp_lines = "\
1-0-For the Doctor Watsons of this world, as opposed to the Sherlock
2:65:Holmeses, success in the province of detective work must always
3-129-be, to a very large extent, the result of luck. Sherlock Holmes
4:193:can extract a clew from a wisp of straw or a flake of cigar ash;
5:258:but Doctor Watson has to have it taken out for him and dusted,
6:321:and exhibited clearly, with a label attached.
byte count:366
";
        // before and after + line numbers
        SearcherTester::new(SHERLOCK, "Sherlock")
            .after_context(1)
            .before_context(1)
            .line_number(true)
            .invert_match(true)
            .expected_no_line_number(exp)
            .expected_with_line_number(exp_lines)
            .test();

        // before
        SearcherTester::new(SHERLOCK, "Sherlock")
            .before_context(1)
            .line_number(false)
            .invert_match(true)
            .expected_no_line_number(exp)
            .test();

        // after
        let exp = "\
65:Holmeses, success in the province of detective work must always
129-be, to a very large extent, the result of luck. Sherlock Holmes
193:can extract a clew from a wisp of straw or a flake of cigar ash;
258:but Doctor Watson has to have it taken out for him and dusted,
321:and exhibited clearly, with a label attached.
byte count:366
";
        SearcherTester::new(SHERLOCK, "Sherlock")
            .after_context(1)
            .line_number(false)
            .invert_match(true)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn context_sherlock2() {
        let exp = "\
65-Holmeses, success in the province of detective work must always
129:be, to a very large extent, the result of luck. Sherlock Holmes
193:can extract a clew from a wisp of straw or a flake of cigar ash;
258-but Doctor Watson has to have it taken out for him and dusted,
321:and exhibited clearly, with a label attached.
byte count:366
";
        let exp_lines = "\
2-65-Holmeses, success in the province of detective work must always
3:129:be, to a very large extent, the result of luck. Sherlock Holmes
4:193:can extract a clew from a wisp of straw or a flake of cigar ash;
5-258-but Doctor Watson has to have it taken out for him and dusted,
6:321:and exhibited clearly, with a label attached.
byte count:366
";
        // before + after + line numbers
        SearcherTester::new(SHERLOCK, " a ")
            .after_context(1)
            .before_context(1)
            .line_number(true)
            .expected_no_line_number(exp)
            .expected_with_line_number(exp_lines)
            .test();

        // before
        SearcherTester::new(SHERLOCK, " a ")
            .before_context(1)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();

        // after
        let exp = "\
129:be, to a very large extent, the result of luck. Sherlock Holmes
193:can extract a clew from a wisp of straw or a flake of cigar ash;
258-but Doctor Watson has to have it taken out for him and dusted,
321:and exhibited clearly, with a label attached.
byte count:366
";
        SearcherTester::new(SHERLOCK, " a ")
            .after_context(1)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn context_sherlock_invert2() {
        let exp = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
65:Holmeses, success in the province of detective work must always
129-be, to a very large extent, the result of luck. Sherlock Holmes
193-can extract a clew from a wisp of straw or a flake of cigar ash;
258:but Doctor Watson has to have it taken out for him and dusted,
321-and exhibited clearly, with a label attached.
byte count:366
";
        let exp_lines = "\
1:0:For the Doctor Watsons of this world, as opposed to the Sherlock
2:65:Holmeses, success in the province of detective work must always
3-129-be, to a very large extent, the result of luck. Sherlock Holmes
4-193-can extract a clew from a wisp of straw or a flake of cigar ash;
5:258:but Doctor Watson has to have it taken out for him and dusted,
6-321-and exhibited clearly, with a label attached.
byte count:366
";
        // before + after + line numbers
        SearcherTester::new(SHERLOCK, " a ")
            .after_context(1)
            .before_context(1)
            .line_number(true)
            .invert_match(true)
            .expected_no_line_number(exp)
            .expected_with_line_number(exp_lines)
            .test();

        // before
        let exp = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
65:Holmeses, success in the province of detective work must always
--
193-can extract a clew from a wisp of straw or a flake of cigar ash;
258:but Doctor Watson has to have it taken out for him and dusted,

byte count:366
";
        SearcherTester::new(SHERLOCK, " a ")
            .before_context(1)
            .line_number(false)
            .invert_match(true)
            .expected_no_line_number(exp)
            .test();

        // after
        let exp = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
65:Holmeses, success in the province of detective work must always
129-be, to a very large extent, the result of luck. Sherlock Holmes
--
258:but Doctor Watson has to have it taken out for him and dusted,
321-and exhibited clearly, with a label attached.
byte count:366
";
        SearcherTester::new(SHERLOCK, " a ")
            .after_context(1)
            .line_number(false)
            .invert_match(true)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn context_sherlock3() {
        let exp = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
65-Holmeses, success in the province of detective work must always
129:be, to a very large extent, the result of luck. Sherlock Holmes
193-can extract a clew from a wisp of straw or a flake of cigar ash;
258-but Doctor Watson has to have it taken out for him and dusted,

byte count:366
";
        let exp_lines = "\
1:0:For the Doctor Watsons of this world, as opposed to the Sherlock
2-65-Holmeses, success in the province of detective work must always
3:129:be, to a very large extent, the result of luck. Sherlock Holmes
4-193-can extract a clew from a wisp of straw or a flake of cigar ash;
5-258-but Doctor Watson has to have it taken out for him and dusted,

byte count:366
";
        // before and after + line numbers
        SearcherTester::new(SHERLOCK, "Sherlock")
            .after_context(2)
            .before_context(2)
            .line_number(true)
            .expected_no_line_number(exp)
            .expected_with_line_number(exp_lines)
            .test();

        // after
        SearcherTester::new(SHERLOCK, "Sherlock")
            .after_context(2)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();

        // before
        let exp = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
65-Holmeses, success in the province of detective work must always
129:be, to a very large extent, the result of luck. Sherlock Holmes

byte count:366
";
        SearcherTester::new(SHERLOCK, "Sherlock")
            .before_context(2)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn context_sherlock4() {
        let exp = "\
129-be, to a very large extent, the result of luck. Sherlock Holmes
193-can extract a clew from a wisp of straw or a flake of cigar ash;
258:but Doctor Watson has to have it taken out for him and dusted,
321-and exhibited clearly, with a label attached.
byte count:366
";
        let exp_lines = "\
3-129-be, to a very large extent, the result of luck. Sherlock Holmes
4-193-can extract a clew from a wisp of straw or a flake of cigar ash;
5:258:but Doctor Watson has to have it taken out for him and dusted,
6-321-and exhibited clearly, with a label attached.
byte count:366
";
        // before and after + line numbers
        SearcherTester::new(SHERLOCK, "dusted")
            .after_context(2)
            .before_context(2)
            .line_number(true)
            .expected_no_line_number(exp)
            .expected_with_line_number(exp_lines)
            .test();

        // after
        let exp = "\
258:but Doctor Watson has to have it taken out for him and dusted,
321-and exhibited clearly, with a label attached.
byte count:366
";
        SearcherTester::new(SHERLOCK, "dusted")
            .after_context(2)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();

        // before
        let exp = "\
129-be, to a very large extent, the result of luck. Sherlock Holmes
193-can extract a clew from a wisp of straw or a flake of cigar ash;
258:but Doctor Watson has to have it taken out for him and dusted,

byte count:366
";
        SearcherTester::new(SHERLOCK, "dusted")
            .before_context(2)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn context_sherlock5() {
        let exp = "\
0-For the Doctor Watsons of this world, as opposed to the Sherlock
65:Holmeses, success in the province of detective work must always
129-be, to a very large extent, the result of luck. Sherlock Holmes
193-can extract a clew from a wisp of straw or a flake of cigar ash;
258-but Doctor Watson has to have it taken out for him and dusted,
321:and exhibited clearly, with a label attached.
byte count:366
";
        let exp_lines = "\
1-0-For the Doctor Watsons of this world, as opposed to the Sherlock
2:65:Holmeses, success in the province of detective work must always
3-129-be, to a very large extent, the result of luck. Sherlock Holmes
4-193-can extract a clew from a wisp of straw or a flake of cigar ash;
5-258-but Doctor Watson has to have it taken out for him and dusted,
6:321:and exhibited clearly, with a label attached.
byte count:366
";
        // before and after + line numbers
        SearcherTester::new(SHERLOCK, "success|attached")
            .after_context(2)
            .before_context(2)
            .line_number(true)
            .expected_no_line_number(exp)
            .expected_with_line_number(exp_lines)
            .test();

        // after
        let exp = "\
65:Holmeses, success in the province of detective work must always
129-be, to a very large extent, the result of luck. Sherlock Holmes
193-can extract a clew from a wisp of straw or a flake of cigar ash;
--
321:and exhibited clearly, with a label attached.
byte count:366
";
        SearcherTester::new(SHERLOCK, "success|attached")
            .after_context(2)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();

        // before
        let exp = "\
0-For the Doctor Watsons of this world, as opposed to the Sherlock
65:Holmeses, success in the province of detective work must always
--
193-can extract a clew from a wisp of straw or a flake of cigar ash;
258-but Doctor Watson has to have it taken out for him and dusted,
321:and exhibited clearly, with a label attached.
byte count:366
";
        SearcherTester::new(SHERLOCK, "success|attached")
            .before_context(2)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn context_sherlock6() {
        let exp = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
65-Holmeses, success in the province of detective work must always
129:be, to a very large extent, the result of luck. Sherlock Holmes
193-can extract a clew from a wisp of straw or a flake of cigar ash;
258-but Doctor Watson has to have it taken out for him and dusted,
321-and exhibited clearly, with a label attached.
byte count:366
";
        let exp_lines = "\
1:0:For the Doctor Watsons of this world, as opposed to the Sherlock
2-65-Holmeses, success in the province of detective work must always
3:129:be, to a very large extent, the result of luck. Sherlock Holmes
4-193-can extract a clew from a wisp of straw or a flake of cigar ash;
5-258-but Doctor Watson has to have it taken out for him and dusted,
6-321-and exhibited clearly, with a label attached.
byte count:366
";
        // before and after + line numbers
        SearcherTester::new(SHERLOCK, "Sherlock")
            .after_context(3)
            .before_context(3)
            .line_number(true)
            .expected_no_line_number(exp)
            .expected_with_line_number(exp_lines)
            .test();

        // after
        let exp = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
65-Holmeses, success in the province of detective work must always
129:be, to a very large extent, the result of luck. Sherlock Holmes
193-can extract a clew from a wisp of straw or a flake of cigar ash;
258-but Doctor Watson has to have it taken out for him and dusted,
321-and exhibited clearly, with a label attached.
byte count:366
";
        SearcherTester::new(SHERLOCK, "Sherlock")
            .after_context(3)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();

        // before
        let exp = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
65-Holmeses, success in the province of detective work must always
129:be, to a very large extent, the result of luck. Sherlock Holmes

byte count:366
";
        SearcherTester::new(SHERLOCK, "Sherlock")
            .before_context(3)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn context_code1() {
        // before and after
        let exp = "\
33-
34-fn main() {
46:    let stdin = io::stdin();
75-    let stdout = io::stdout();
106-
107:    // Wrap the stdin reader in a Snappy reader.
156:    let mut rdr = snap::Reader::new(stdin.lock());
207-    let mut wtr = stdout.lock();
240-    io::copy(&mut rdr, &mut wtr).expect(\"I/O operation failed\");

byte count:307
";
        let exp_lines = "\
4-33-
5-34-fn main() {
6:46:    let stdin = io::stdin();
7-75-    let stdout = io::stdout();
8-106-
9:107:    // Wrap the stdin reader in a Snappy reader.
10:156:    let mut rdr = snap::Reader::new(stdin.lock());
11-207-    let mut wtr = stdout.lock();
12-240-    io::copy(&mut rdr, &mut wtr).expect(\"I/O operation failed\");

byte count:307
";
        // before and after + line numbers
        SearcherTester::new(CODE, "stdin")
            .after_context(2)
            .before_context(2)
            .line_number(true)
            .expected_no_line_number(exp)
            .expected_with_line_number(exp_lines)
            .test();

        // after
        let exp = "\
46:    let stdin = io::stdin();
75-    let stdout = io::stdout();
106-
107:    // Wrap the stdin reader in a Snappy reader.
156:    let mut rdr = snap::Reader::new(stdin.lock());
207-    let mut wtr = stdout.lock();
240-    io::copy(&mut rdr, &mut wtr).expect(\"I/O operation failed\");

byte count:307
";
        SearcherTester::new(CODE, "stdin")
            .after_context(2)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();

        // before
        let exp = "\
33-
34-fn main() {
46:    let stdin = io::stdin();
75-    let stdout = io::stdout();
106-
107:    // Wrap the stdin reader in a Snappy reader.
156:    let mut rdr = snap::Reader::new(stdin.lock());

byte count:307
";
        SearcherTester::new(CODE, "stdin")
            .before_context(2)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn context_code2() {
        let exp = "\
34-fn main() {
46-    let stdin = io::stdin();
75:    let stdout = io::stdout();
106-
107-    // Wrap the stdin reader in a Snappy reader.
156-    let mut rdr = snap::Reader::new(stdin.lock());
207:    let mut wtr = stdout.lock();
240-    io::copy(&mut rdr, &mut wtr).expect(\"I/O operation failed\");
305-}

byte count:307
";
        let exp_lines = "\
5-34-fn main() {
6-46-    let stdin = io::stdin();
7:75:    let stdout = io::stdout();
8-106-
9-107-    // Wrap the stdin reader in a Snappy reader.
10-156-    let mut rdr = snap::Reader::new(stdin.lock());
11:207:    let mut wtr = stdout.lock();
12-240-    io::copy(&mut rdr, &mut wtr).expect(\"I/O operation failed\");
13-305-}

byte count:307
";
        // before and after + line numbers
        SearcherTester::new(CODE, "stdout")
            .after_context(2)
            .before_context(2)
            .line_number(true)
            .expected_no_line_number(exp)
            .expected_with_line_number(exp_lines)
            .test();

        // after
        let exp = "\
75:    let stdout = io::stdout();
106-
107-    // Wrap the stdin reader in a Snappy reader.
--
207:    let mut wtr = stdout.lock();
240-    io::copy(&mut rdr, &mut wtr).expect(\"I/O operation failed\");
305-}

byte count:307
";
        SearcherTester::new(CODE, "stdout")
            .after_context(2)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();

        // before
        let exp = "\
34-fn main() {
46-    let stdin = io::stdin();
75:    let stdout = io::stdout();
--
107-    // Wrap the stdin reader in a Snappy reader.
156-    let mut rdr = snap::Reader::new(stdin.lock());
207:    let mut wtr = stdout.lock();

byte count:307
";
        SearcherTester::new(CODE, "stdout")
            .before_context(2)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn context_code3() {
        let exp = "\
20-use std::io;
33-
34:fn main() {
46-    let stdin = io::stdin();
75-    let stdout = io::stdout();
106-
107-    // Wrap the stdin reader in a Snappy reader.
156:    let mut rdr = snap::Reader::new(stdin.lock());
207-    let mut wtr = stdout.lock();
240-    io::copy(&mut rdr, &mut wtr).expect(\"I/O operation failed\");

byte count:307
";
        let exp_lines = "\
3-20-use std::io;
4-33-
5:34:fn main() {
6-46-    let stdin = io::stdin();
7-75-    let stdout = io::stdout();
8-106-
9-107-    // Wrap the stdin reader in a Snappy reader.
10:156:    let mut rdr = snap::Reader::new(stdin.lock());
11-207-    let mut wtr = stdout.lock();
12-240-    io::copy(&mut rdr, &mut wtr).expect(\"I/O operation failed\");

byte count:307
";
        // before and after + line numbers
        SearcherTester::new(CODE, "fn main|let mut rdr")
            .after_context(2)
            .before_context(2)
            .line_number(true)
            .expected_no_line_number(exp)
            .expected_with_line_number(exp_lines)
            .test();

        // after
        let exp = "\
34:fn main() {
46-    let stdin = io::stdin();
75-    let stdout = io::stdout();
--
156:    let mut rdr = snap::Reader::new(stdin.lock());
207-    let mut wtr = stdout.lock();
240-    io::copy(&mut rdr, &mut wtr).expect(\"I/O operation failed\");

byte count:307
";
        SearcherTester::new(CODE, "fn main|let mut rdr")
            .after_context(2)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();

        // before
        let exp = "\
20-use std::io;
33-
34:fn main() {
--
106-
107-    // Wrap the stdin reader in a Snappy reader.
156:    let mut rdr = snap::Reader::new(stdin.lock());

byte count:307
";
        SearcherTester::new(CODE, "fn main|let mut rdr")
            .before_context(2)
            .line_number(false)
            .expected_no_line_number(exp)
            .test();
    }

    #[test]
    fn scratch() {
        use crate::sinks;
        use crate::testutil::RegexMatcher;

        const SHERLOCK: &'static [u8] = b"\
For the Doctor Wat\xFFsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
be, to a very large extent, the result of luck. Sherlock Holmes
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.\
    ";

        let haystack = SHERLOCK;
        let matcher = RegexMatcher::new("Sherlock");
        let mut searcher = SearcherBuilder::new().line_number(true).build();
        searcher
            .search_reader(
                &matcher,
                haystack,
                sinks::Lossy(|n, line| {
                    print!("{}:{}", n, line);
                    Ok(true)
                }),
            )
            .unwrap();
    }

    // See: https://github.com/BurntSushi/ripgrep/issues/2260
    #[test]
    fn regression_2260() {
        use grep_regex::RegexMatcherBuilder;

        use crate::SearcherBuilder;

        let matcher = RegexMatcherBuilder::new()
            .line_terminator(Some(b'\n'))
            .build(r"^\w+$")
            .unwrap();
        let mut searcher = SearcherBuilder::new().line_number(true).build();

        let mut matched = false;
        searcher
            .search_slice(
                &matcher,
                b"GATC\n",
                crate::sinks::UTF8(|_, _| {
                    matched = true;
                    Ok(true)
                }),
            )
            .unwrap();
        assert!(matched);
    }
}
