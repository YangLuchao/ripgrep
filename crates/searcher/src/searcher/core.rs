use std::cmp;

use bstr::ByteSlice;

use crate::line_buffer::BinaryDetection;
use crate::lines::{self, LineStep};
use crate::searcher::{Config, Range, Searcher};
use crate::sink::{
    Sink, SinkContext, SinkContextKind, SinkError, SinkFinish, SinkMatch,
};
use grep_matcher::{LineMatchKind, Matcher};

enum FastMatchResult {
    Continue,
    Stop,
    SwitchToSlow,
}

#[derive(Debug)]
pub struct Core<'s, M: 's, S> {
    config: &'s Config,
    matcher: M,
    searcher: &'s Searcher,
    sink: S,
    binary: bool,
    pos: usize,
    absolute_byte_offset: u64,
    binary_byte_offset: Option<usize>,
    line_number: Option<u64>,
    last_line_counted: usize,
    last_line_visited: usize,
    after_context_left: usize,
    has_sunk: bool,
    has_matched: bool,
}

impl<'s, M: Matcher, S: Sink> Core<'s, M, S> {
    /// 创建一个新的 `Core` 实例，用于搜索匹配项。
    pub fn new(
        searcher: &'s Searcher,
        matcher: M,
        sink: S,
        binary: bool,
    ) -> Core<'s, M, S> {
        // 根据配置确定是否记录行号。
        let line_number =
            if searcher.config.line_number { Some(1) } else { None };
        // 创建 `Core` 结构体实例。
        let core = Core {
            config: &searcher.config,
            matcher: matcher,
            searcher: searcher,
            sink: sink,
            binary: binary,
            pos: 0,
            absolute_byte_offset: 0,
            binary_byte_offset: None,
            line_number: line_number,
            last_line_counted: 0,
            last_line_visited: 0,
            after_context_left: 0,
            has_sunk: false,
            has_matched: false,
        };
        // 根据匹配器类型和配置判断是否需要切换到慢速行搜索。
        if !core.searcher.multi_line_with_matcher(&core.matcher) {
            if core.is_line_by_line_fast() {
                log::trace!("searcher core: will use fast line searcher");
            } else {
                log::trace!("searcher core: will use slow line searcher");
            }
        }
        core
    }

    /// 获取当前位置。
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// 设置当前位置。
    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
    }

    /// 获取二进制数据的字节偏移量。
    pub fn binary_byte_offset(&self) -> Option<u64> {
        self.binary_byte_offset.map(|offset| offset as u64)
    }

    /// 获取匹配器的引用。
    pub fn matcher(&self) -> &M {
        &self.matcher
    }

    /// 处理匹配的数据，并将结果传递给接收器。
    pub fn matched(
        &mut self,
        buf: &[u8],
        range: &Range,
    ) -> Result<bool, S::Error> {
        self.sink_matched(buf, range)
    }

    /// 处理二进制数据，并将结果传递给接收器。
    pub fn binary_data(
        &mut self,
        binary_byte_offset: u64,
    ) -> Result<bool, S::Error> {
        self.sink.binary_data(&self.searcher, binary_byte_offset)
    }

    /// 开始搜索操作，并将结果传递给接收器。
    pub fn begin(&mut self) -> Result<bool, S::Error> {
        self.sink.begin(&self.searcher)
    }

    /// 结束搜索操作，并将结果传递给接收器。
    pub fn finish(
        &mut self,
        byte_count: u64,
        binary_byte_offset: Option<u64>,
    ) -> Result<(), S::Error> {
        self.sink.finish(
            &self.searcher,
            &SinkFinish { byte_count, binary_byte_offset },
        )
    }

    /// 逐行匹配文本数据。
    pub fn match_by_line(&mut self, buf: &[u8]) -> Result<bool, S::Error> {
        if self.is_line_by_line_fast() {
            match self.match_by_line_fast(buf)? {
                FastMatchResult::SwitchToSlow => self.match_by_line_slow(buf),
                FastMatchResult::Continue => Ok(true),
                FastMatchResult::Stop => Ok(false),
            }
        } else {
            self.match_by_line_slow(buf)
        }
    }

    /// 执行 `roll` 操作，用于更新状态并返回消耗的字节数。
    pub fn roll(&mut self, buf: &[u8]) -> usize {
        // 计算消耗的字节数。
        let consumed = if self.config.max_context() == 0 {
            buf.len()
        } else {
            // 在具有上下文的情况下，计算上一个行的位置。
            let context_start = lines::preceding(
                buf,
                self.config.line_term.as_byte(),
                self.config.max_context(),
            );
            let consumed = cmp::max(context_start, self.last_line_visited);
            consumed
        };
        // 更新状态和位置信息。
        self.count_lines(buf, consumed);
        self.absolute_byte_offset += consumed as u64;
        self.last_line_counted = 0;
        self.last_line_visited = 0;
        self.set_pos(buf.len() - consumed);
        consumed
    }

    /// 检测二进制数据并处理，决定是否继续搜索。
    pub fn detect_binary(
        &mut self,
        buf: &[u8],
        range: &Range,
    ) -> Result<bool, S::Error> {
        // 如果已经存在二进制字节偏移量，则根据配置决定是否继续搜索。
        if self.binary_byte_offset.is_some() {
            return Ok(self.config.binary.quit_byte().is_some());
        }
        // 获取二进制数据的字节值。
        let binary_byte = match self.config.binary.0 {
            BinaryDetection::Quit(b) => b,
            BinaryDetection::Convert(b) => b,
            _ => return Ok(false),
        };
        // 在给定的范围内寻找二进制字节。
        if let Some(i) = buf[*range].find_byte(binary_byte) {
            let offset = range.start() + i;
            self.binary_byte_offset = Some(offset);
            if !self.binary_data(offset as u64)? {
                return Ok(true);
            }
            Ok(self.config.binary.quit_byte().is_some())
        } else {
            Ok(false)
        }
    }

    /// 处理文本数据的上文并将结果传递给接收器。
    pub fn before_context_by_line(
        &mut self,
        buf: &[u8],
        upto: usize,
    ) -> Result<bool, S::Error> {
        // 根据配置判断是否需要处理上文。
        if self.config.before_context == 0 {
            return Ok(true);
        }
        // 计算上文的范围。
        let range = Range::new(self.last_line_visited, upto);
        if range.is_empty() {
            return Ok(true);
        }
        // 计算上文的开始位置。
        let before_context_start = range.start()
            + lines::preceding(
                &buf[range],
                self.config.line_term.as_byte(),
                self.config.before_context - 1,
            );

        let range = Range::new(before_context_start, range.end());
        // 创建 LineStep 实例，用于逐行遍历上文数据。
        let mut stepper = LineStep::new(
            self.config.line_term.as_byte(),
            range.start(),
            range.end(),
        );
        while let Some(line) = stepper.next_match(buf) {
            if !self.sink_break_context(line.start())? {
                return Ok(false);
            }
            if !self.sink_before_context(buf, &line)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// 处理文本数据的下文并将结果传递给接收器。
    pub fn after_context_by_line(
        &mut self,
        buf: &[u8],
        upto: usize,
    ) -> Result<bool, S::Error> {
        // 根据配置判断是否需要处理下文。
        if self.after_context_left == 0 {
            return Ok(true);
        }
        // 计算下文的范围。
        let range = Range::new(self.last_line_visited, upto);
        // 创建 LineStep 实例，用于逐行遍历下文数据。
        let mut stepper = LineStep::new(
            self.config.line_term.as_byte(),
            range.start(),
            range.end(),
        );
        while let Some(line) = stepper.next_match(buf) {
            if !self.sink_after_context(buf, &line)? {
                return Ok(false);
            }
            if self.after_context_left == 0 {
                break;
            }
        }
        Ok(true)
    }

    /// 处理文本数据的其他上下文并将结果传递给接收器。
    pub fn other_context_by_line(
        &mut self,
        buf: &[u8],
        upto: usize,
    ) -> Result<bool, S::Error> {
        // 计算范围，用于遍历其他上下文数据。
        let range = Range::new(self.last_line_visited, upto);
        // 创建 LineStep 实例，用于逐行遍历其他上下文数据。
        let mut stepper = LineStep::new(
            self.config.line_term.as_byte(),
            range.start(),
            range.end(),
        );
        while let Some(line) = stepper.next_match(buf) {
            if !self.sink_other_context(buf, &line)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
    /// 通过逐行方式慢速匹配文本数据。
    fn match_by_line_slow(&mut self, buf: &[u8]) -> Result<bool, S::Error> {
        debug_assert!(!self.searcher.multi_line_with_matcher(&self.matcher));

        // 定义范围，表示当前搜索的数据范围。
        let range = Range::new(self.pos(), buf.len());
        // 创建 LineStep 实例，用于逐行遍历数据。
        let mut stepper = LineStep::new(
            self.config.line_term.as_byte(),
            range.start(),
            range.end(),
        );
        while let Some(line) = stepper.next_match(buf) {
            let matched = {
                // 剥离行终止符是为了防止某些正则表达式在行末空白位置匹配。例如，正则表达式 `(?m)^$`
                // 可以在字符串 `a\n` 中的位置 (2, 2) 处匹配。
                let slice = lines::without_terminator(
                    &buf[line],
                    self.config.line_term,
                );
                match self.matcher.shortest_match(slice) {
                    Err(err) => return Err(S::Error::error_message(err)),
                    Ok(result) => result.is_some(),
                }
            };
            self.set_pos(line.end());
            let success = matched != self.config.invert_match;
            if success {
                self.has_matched = true;
                if !self.before_context_by_line(buf, line.start())? {
                    return Ok(false);
                }
                if !self.sink_matched(buf, &line)? {
                    return Ok(false);
                }
            } else if self.after_context_left >= 1 {
                if !self.sink_after_context(buf, &line)? {
                    return Ok(false);
                }
            } else if self.config.passthru {
                if !self.sink_other_context(buf, &line)? {
                    return Ok(false);
                }
            }
            if self.config.stop_on_nonmatch && !success && self.has_matched {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// 通过逐行方式快速匹配文本数据。
    fn match_by_line_fast(
        &mut self,
        buf: &[u8],
    ) -> Result<FastMatchResult, S::Error> {
        use FastMatchResult::*;

        debug_assert!(!self.config.passthru);
        while !buf[self.pos()..].is_empty() {
            if self.config.stop_on_nonmatch && self.has_matched {
                return Ok(SwitchToSlow);
            }
            if self.config.invert_match {
                if !self.match_by_line_fast_invert(buf)? {
                    return Ok(Stop);
                }
            } else if let Some(line) = self.find_by_line_fast(buf)? {
                self.has_matched = true;
                if self.config.max_context() > 0 {
                    if !self.after_context_by_line(buf, line.start())? {
                        return Ok(Stop);
                    }
                    if !self.before_context_by_line(buf, line.start())? {
                        return Ok(Stop);
                    }
                }
                self.set_pos(line.end());
                if !self.sink_matched(buf, &line)? {
                    return Ok(Stop);
                }
            } else {
                break;
            }
        }
        if !self.after_context_by_line(buf, buf.len())? {
            return Ok(Stop);
        }
        self.set_pos(buf.len());
        Ok(Continue)
    }

    /// 通过快速方式反向匹配逐行文本数据。
    #[inline(always)]
    fn match_by_line_fast_invert(
        &mut self,
        buf: &[u8],
    ) -> Result<bool, S::Error> {
        assert!(self.config.invert_match);

        let invert_match = match self.find_by_line_fast(buf)? {
            None => {
                let range = Range::new(self.pos(), buf.len());
                self.set_pos(range.end());
                range
            }
            Some(line) => {
                let range = Range::new(self.pos(), line.start());
                self.set_pos(line.end());
                range
            }
        };
        if invert_match.is_empty() {
            return Ok(true);
        }
        self.has_matched = true;
        if !self.after_context_by_line(buf, invert_match.start())? {
            return Ok(false);
        }
        if !self.before_context_by_line(buf, invert_match.start())? {
            return Ok(false);
        }
        let mut stepper = LineStep::new(
            self.config.line_term.as_byte(),
            invert_match.start(),
            invert_match.end(),
        );
        while let Some(line) = stepper.next_match(buf) {
            if !self.sink_matched(buf, &line)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// 通过快速方式查找匹配逐行文本数据。
    #[inline(always)]
    fn find_by_line_fast(
        &self,
        buf: &[u8],
    ) -> Result<Option<Range>, S::Error> {
        debug_assert!(!self.searcher.multi_line_with_matcher(&self.matcher));
        debug_assert!(self.is_line_by_line_fast());

        let mut pos = self.pos();
        while !buf[pos..].is_empty() {
            match self.matcher.find_candidate_line(&buf[pos..]) {
                Err(err) => return Err(S::Error::error_message(err)),
                Ok(None) => return Ok(None),
                Ok(Some(LineMatchKind::Confirmed(i))) => {
                    let line = lines::locate(
                        buf,
                        self.config.line_term.as_byte(),
                        Range::zero(i).offset(pos),
                    );
                    // 如果匹配超出了缓冲区末尾，则不将其报告为匹配。
                    if line.start() == buf.len() {
                        pos = buf.len();
                        continue;
                    }
                    return Ok(Some(line));
                }
                Ok(Some(LineMatchKind::Candidate(i))) => {
                    let line = lines::locate(
                        buf,
                        self.config.line_term.as_byte(),
                        Range::zero(i).offset(pos),
                    );
                    // 在这里剥离行终止符是为了与逐行匹配的语义相匹配。
                    // 也就是说，正则表达式 `(?m)^$` 可以在行终止符之后的最终位置匹配，这在逐行匹配中是不合理的。
                    let slice = lines::without_terminator(
                        &buf[line],
                        self.config.line_term,
                    );
                    match self.matcher.is_match(slice) {
                        Err(err) => return Err(S::Error::error_message(err)),
                        Ok(true) => return Ok(Some(line)),
                        Ok(false) => {
                            pos = line.end();
                            continue;
                        }
                    }
                }
            }
        }
        Ok(None)
    }
    /// 下沉匹配结果到输出，处理匹配的行文本。
    #[inline(always)]
    fn sink_matched(
        &mut self,
        buf: &[u8],
        range: &Range,
    ) -> Result<bool, S::Error> {
        // 检测是否为二进制模式并检测二进制数据。
        if self.binary && self.detect_binary(buf, range)? {
            return Ok(false);
        }
        // 下沉之前检查上下文中断，如果上一行和当前行不连续则可能插入一个上下文中断。
        if !self.sink_break_context(range.start())? {
            return Ok(false);
        }
        // 统计行数以更新行号。
        self.count_lines(buf, range.start());
        let offset = self.absolute_byte_offset + range.start() as u64;
        let linebuf = &buf[*range];
        // 传递匹配结果到下游处理器。
        let keepgoing = self.sink.matched(
            &self.searcher,
            &SinkMatch {
                line_term: self.config.line_term,
                bytes: linebuf,
                absolute_byte_offset: offset,
                line_number: self.line_number,
                buffer: buf,
                bytes_range_in_buffer: range.start()..range.end(),
            },
        )?;
        if !keepgoing {
            return Ok(false);
        }
        self.last_line_visited = range.end();
        self.after_context_left = self.config.after_context;
        self.has_sunk = true;
        Ok(true)
    }

    /// 下沉前上下文数据到输出。
    fn sink_before_context(
        &mut self,
        buf: &[u8],
        range: &Range,
    ) -> Result<bool, S::Error> {
        // 检测是否为二进制模式并检测二进制数据。
        if self.binary && self.detect_binary(buf, range)? {
            return Ok(false);
        }
        // 统计行数以更新行号。
        self.count_lines(buf, range.start());
        let offset = self.absolute_byte_offset + range.start() as u64;
        // 传递上下文数据到下游处理器。
        let keepgoing = self.sink.context(
            &self.searcher,
            &SinkContext {
                #[cfg(test)]
                line_term: self.config.line_term,
                bytes: &buf[*range],
                kind: SinkContextKind::Before,
                absolute_byte_offset: offset,
                line_number: self.line_number,
            },
        )?;
        if !keepgoing {
            return Ok(false);
        }
        self.last_line_visited = range.end();
        self.has_sunk = true;
        Ok(true)
    }

    /// 下沉后上下文数据到输出。
    fn sink_after_context(
        &mut self,
        buf: &[u8],
        range: &Range,
    ) -> Result<bool, S::Error> {
        assert!(self.after_context_left >= 1);

        // 检测是否为二进制模式并检测二进制数据。
        if self.binary && self.detect_binary(buf, range)? {
            return Ok(false);
        }
        // 统计行数以更新行号。
        self.count_lines(buf, range.start());
        let offset = self.absolute_byte_offset + range.start() as u64;
        // 传递上下文数据到下游处理器。
        let keepgoing = self.sink.context(
            &self.searcher,
            &SinkContext {
                #[cfg(test)]
                line_term: self.config.line_term,
                bytes: &buf[*range],
                kind: SinkContextKind::After,
                absolute_byte_offset: offset,
                line_number: self.line_number,
            },
        )?;
        if !keepgoing {
            return Ok(false);
        }
        self.last_line_visited = range.end();
        self.after_context_left -= 1;
        self.has_sunk = true;
        Ok(true)
    }

    /// 下沉其他上下文数据到输出。
    fn sink_other_context(
        &mut self,
        buf: &[u8],
        range: &Range,
    ) -> Result<bool, S::Error> {
        // 检测是否为二进制模式并检测二进制数据。
        if self.binary && self.detect_binary(buf, range)? {
            return Ok(false);
        }
        // 统计行数以更新行号。
        self.count_lines(buf, range.start());
        let offset = self.absolute_byte_offset + range.start() as u64;
        // 传递上下文数据到下游处理器。
        let keepgoing = self.sink.context(
            &self.searcher,
            &SinkContext {
                #[cfg(test)]
                line_term: self.config.line_term,
                bytes: &buf[*range],
                kind: SinkContextKind::Other,
                absolute_byte_offset: offset,
                line_number: self.line_number,
            },
        )?;
        if !keepgoing {
            return Ok(false);
        }
        self.last_line_visited = range.end();
        self.has_sunk = true;
        Ok(true)
    }

    /// 下沉上下文中断数据到输出。
    fn sink_break_context(
        &mut self,
        start_of_line: usize,
    ) -> Result<bool, S::Error> {
        let is_gap = self.last_line_visited < start_of_line;
        let any_context =
            self.config.before_context > 0 || self.config.after_context > 0;

        if !any_context || !self.has_sunk || !is_gap {
            Ok(true)
        } else {
            self.sink.context_break(&self.searcher)
        }
    }

    /// 计算行数以更新行号。
    fn count_lines(&mut self, buf: &[u8], upto: usize) {
        if let Some(ref mut line_number) = self.line_number {
            if self.last_line_counted >= upto {
                return;
            }
            let slice = &buf[self.last_line_counted..upto];
            let count = lines::count(slice, self.config.line_term.as_byte());
            *line_number += count;
            self.last_line_counted = upto;
        }
    }

    /// 判断是否可以使用快速逐行匹配。
    fn is_line_by_line_fast(&self) -> bool {
        debug_assert!(!self.searcher.multi_line_with_matcher(&self.matcher));

        if self.config.passthru {
            return false;
        }
        if self.config.stop_on_nonmatch && self.has_matched {
            return false;
        }
        if let Some(line_term) = self.matcher.line_terminator() {
            if line_term == self.config.line_term {
                return true;
            }
        }
        if let Some(non_matching) = self.matcher.non_matching_bytes() {
            // 如果行终止符是 CRLF，则实际上无需关心正则表达式是否能匹配 `\r`。
            // 也就是说，`\r` 既不是必需的，也不足以终止一行。总是需要 `\n`。
            if non_matching.contains(self.config.line_term.as_byte()) {
                return true;
            }
        }
        false
    }
}
