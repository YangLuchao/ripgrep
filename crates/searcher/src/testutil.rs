use std::io::{self, Write};
use std::str;

use bstr::ByteSlice;
use grep_matcher::{
    LineMatchKind, LineTerminator, Match, Matcher, NoCaptures, NoError,
};
use regex::bytes::{Regex, RegexBuilder};

use crate::searcher::{BinaryDetection, Searcher, SearcherBuilder};
use crate::sink::{Sink, SinkContext, SinkFinish, SinkMatch};
/// 一个简单的正则表达式匹配器。
///
/// 这支持直接设置匹配器的行终止符配置，我们用于测试目的。
/// 也就是说，调用者明确确定是否启用行终止符优化。
/// （实际上，此优化是通过检查并可能修改正则表达式自身来自动检测的。）
#[derive(Clone, Debug)]
pub struct RegexMatcher {
    regex: Regex,
    line_term: Option<LineTerminator>,
    every_line_is_candidate: bool,
}

impl RegexMatcher {
    /// 创建一个新的正则表达式匹配器。
    pub fn new(pattern: &str) -> RegexMatcher {
        let regex = RegexBuilder::new(pattern)
            .multi_line(true) // 允许 ^ 和 $ 在 \n 边界处匹配
            .build()
            .unwrap();
        RegexMatcher {
            regex: regex,
            line_term: None,
            every_line_is_candidate: false,
        }
    }

    /// 强制设置此匹配器的行终止符。
    ///
    /// 默认情况下，此匹配器未设置行终止符。
    pub fn set_line_term(
        &mut self,
        line_term: Option<LineTerminator>,
    ) -> &mut RegexMatcher {
        self.line_term = line_term;
        self
    }

    /// 是否将每一行都作为候选项返回。
    ///
    /// 这会强制搜索器处理报告假阳性的情况。
    pub fn every_line_is_candidate(&mut self, yes: bool) -> &mut RegexMatcher {
        self.every_line_is_candidate = yes;
        self
    }
}

impl Matcher for RegexMatcher {
    type Captures = NoCaptures;
    type Error = NoError;

    fn find_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<Option<Match>, NoError> {
        Ok(self
            .regex
            .find_at(haystack, at)
            .map(|m| Match::new(m.start(), m.end())))
    }

    fn new_captures(&self) -> Result<NoCaptures, NoError> {
        Ok(NoCaptures::new())
    }

    fn line_terminator(&self) -> Option<LineTerminator> {
        self.line_term
    }

    fn find_candidate_line(
        &self,
        haystack: &[u8],
    ) -> Result<Option<LineMatchKind>, NoError> {
        if self.every_line_is_candidate {
            assert!(self.line_term.is_some());
            if haystack.is_empty() {
                return Ok(None);
            }
            // 使其变得有趣，并返回当前行中的最后一个字节。
            let i = haystack
                .find_byte(self.line_term.unwrap().as_byte())
                .map(|i| i)
                .unwrap_or(haystack.len() - 1);
            Ok(Some(LineMatchKind::Candidate(i)))
        } else {
            Ok(self.shortest_match(haystack)?.map(LineMatchKind::Confirmed))
        }
    }
}

/// 一个实现了 Sink 的实现，打印所有可用信息。
///
/// 这对于测试非常有用，因为它可以让我们轻松地确认是否正确地将数据传递给 Sink。
#[derive(Clone, Debug)]
pub struct KitchenSink(Vec<u8>);

impl KitchenSink {
    /// 创建一个包含“厨房中所有内容”的 Sink 实现的新实例。
    pub fn new() -> KitchenSink {
        KitchenSink(vec![])
    }

    /// 返回写入此 Sink 的数据。
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Sink for KitchenSink {
    type Error = io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, io::Error> {
        assert!(!mat.bytes().is_empty());
        assert!(mat.lines().count() >= 1);

        let mut line_number = mat.line_number();
        let mut byte_offset = mat.absolute_byte_offset();
        for line in mat.lines() {
            if let Some(ref mut n) = line_number {
                write!(self.0, "{}:", n)?;
                *n += 1;
            }

            write!(self.0, "{}:", byte_offset)?;
            byte_offset += line.len() as u64;
            self.0.write_all(line)?;
        }
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, io::Error> {
        assert!(!context.bytes().is_empty());
        assert!(context.lines().count() == 1);

        if let Some(line_number) = context.line_number() {
            write!(self.0, "{}-", line_number)?;
        }
        write!(self.0, "{}-", context.absolute_byte_offset)?;
        self.0.write_all(context.bytes())?;
        Ok(true)
    }

    fn context_break(
        &mut self,
        _searcher: &Searcher,
    ) -> Result<bool, io::Error> {
        self.0.write_all(b"--\n")?;
        Ok(true)
    }

    fn finish(
        &mut self,
        _searcher: &Searcher,
        sink_finish: &SinkFinish,
    ) -> Result<(), io::Error> {
        writeln!(self.0, "")?;
        writeln!(self.0, "byte count:{}", sink_finish.byte_count())?;
        if let Some(offset) = sink_finish.binary_byte_offset() {
            writeln!(self.0, "binary offset:{}", offset)?;
        }
        Ok(())
    }
}

/// 用于对搜索器进行测试的类型。
///
/// 搜索器代码具有许多不同的代码路径，主要用于优化各种不同的用例。搜索器的意图是根据配置选择最佳的代码路径，
/// 这意味着没有明显的直接方法来要求执行特定的代码路径。因此，这个测试器的目的是显式地检查尽可能多的有意义的代码路径。
///
/// 测试器通过假设您希望测试所有相关的代码路径来工作。可以通过各种构建器方法来逐渐缩减这些代码路径。
#[derive(Debug)]
pub struct SearcherTester {
    haystack: String,
    pattern: String,
    filter: Option<::regex::Regex>,
    print_labels: bool,
    expected_no_line_number: Option<String>,
    expected_with_line_number: Option<String>,
    expected_slice_no_line_number: Option<String>,
    expected_slice_with_line_number: Option<String>,
    by_line: bool,
    multi_line: bool,
    invert_match: bool,
    line_number: bool,
    binary: BinaryDetection,
    auto_heap_limit: bool,
    after_context: usize,
    before_context: usize,
    passthru: bool,
}

impl SearcherTester {
    /// 为测试搜索器创建一个新的测试器。
    pub fn new(haystack: &str, pattern: &str) -> SearcherTester {
        SearcherTester {
            haystack: haystack.to_string(), // 初始化被搜索的文本
            pattern: pattern.to_string(),   // 初始化搜索的模式
            filter: None,                   // 过滤器，默认为空
            print_labels: false,            // 是否打印测试标签，默认为 false
            expected_no_line_number: None,  // 期望的无行号搜索结果，默认为空
            expected_with_line_number: None, // 期望的带行号搜索结果，默认为空
            expected_slice_no_line_number: None, // 期望的无行号切片搜索结果，默认为空
            expected_slice_with_line_number: None, // 期望的带行号切片搜索结果，默认为空
            by_line: true,                         // 是否按行搜索，默认为 true
            multi_line: true, // 是否进行多行搜索，默认为 true
            invert_match: false, // 是否反向匹配，默认为 false
            line_number: true, // 是否显示行号，默认为 true
            binary: BinaryDetection::none(), // 二进制检测，默认不启用
            auto_heap_limit: true, // 是否自动设置堆限制，默认为 true
            after_context: 0, // 后置上下文，默认为 0
            before_context: 0, // 前置上下文，默认为 0
            passthru: false,  // 是否启用透传模式，默认为 false
        }
    }

    /// 执行测试。如果测试成功，函数返回成功。如果测试失败，函数会抛出带有信息的 panic。
    pub fn test(&self) {
        // 检查配置错误
        if self.expected_no_line_number.is_none() {
            panic!("必须提供不带行号的 'expected' 字符串");
        }
        if self.line_number && self.expected_with_line_number.is_none() {
            panic!("必须提供带行号的 'expected' 字符串，或者禁用带行号的测试");
        }

        let configs = self.configs(); // 获取测试配置
        if configs.is_empty() {
            panic!("测试配置导致没有任何测试被执行");
        }
        if self.print_labels {
            for config in &configs {
                let labels = vec![
                    format!("reader-{}", config.label),
                    format!("slice-{}", config.label),
                ];
                for label in &labels {
                    if self.include(label) {
                        println!("{}", label);
                    } else {
                        println!("{} (已忽略)", label);
                    }
                }
            }
        }
        for config in &configs {
            let label = format!("reader-{}", config.label);
            if self.include(&label) {
                let got = config.search_reader(&self.haystack); // 执行基于读取器的搜索
                assert_eq_printed!(config.expected_reader, got, "{}", label);
            }

            let label = format!("slice-{}", config.label);
            if self.include(&label) {
                let got = config.search_slice(&self.haystack); // 执行基于切片的搜索
                assert_eq_printed!(config.expected_slice, got, "{}", label);
            }
        }
    }

    /// 设置用于过滤要运行的测试的正则表达式模式。
    ///
    /// 默认情况下，没有设置过滤器。设置过滤器后，只会运行标签与给定模式匹配的测试配置。
    ///
    /// 这在调试测试时很有用，例如，当您想进行 printf 调试并且只想运行一个特定的测试配置时。
    #[allow(dead_code)]
    pub fn filter(&mut self, pattern: &str) -> &mut SearcherTester {
        self.filter = Some(::regex::Regex::new(pattern).unwrap());
        self
    }

    /// 当设置时，会在执行任何测试之前打印所有测试配置的标签。
    ///
    /// 请注意，在非失败的测试中查看这些标签，您需要使用 `cargo test -- --nocapture`。
    #[allow(dead_code)]
    pub fn print_labels(&mut self, yes: bool) -> &mut SearcherTester {
        self.print_labels = yes;
        self
    }

    /// 设置预期的搜索结果，不包含行号。
    pub fn expected_no_line_number(
        &mut self,
        exp: &str,
    ) -> &mut SearcherTester {
        self.expected_no_line_number = Some(exp.to_string());
        self
    }

    /// 设置预期的搜索结果，包含行号。
    pub fn expected_with_line_number(
        &mut self,
        exp: &str,
    ) -> &mut SearcherTester {
        self.expected_with_line_number = Some(exp.to_string());
        self
    }

    /// 设置预期的搜索结果，不包含行号，用于在切片上执行搜索。如果未提供，则使用 `expected_no_line_number`。
    pub fn expected_slice_no_line_number(
        &mut self,
        exp: &str,
    ) -> &mut SearcherTester {
        self.expected_slice_no_line_number = Some(exp.to_string());
        self
    }

    /// 设置预期的搜索结果，包含行号，用于在切片上执行搜索。如果未提供，则使用 `expected_with_line_number`。
    #[allow(dead_code)]
    pub fn expected_slice_with_line_number(
        &mut self,
        exp: &str,
    ) -> &mut SearcherTester {
        self.expected_slice_with_line_number = Some(exp.to_string());
        self
    }

    /// 是否测试带行号的搜索。
    ///
    /// 默认情况下，启用此选项。启用时，必须提供带行号的字符串，否则不需要提供预期字符串。
    pub fn line_number(&mut self, yes: bool) -> &mut SearcherTester {
        self.line_number = yes;
        self
    }

    /// 是否使用逐行搜索器进行搜索。
    ///
    /// 默认情况下，启用此选项。
    pub fn by_line(&mut self, yes: bool) -> &mut SearcherTester {
        self.by_line = yes;
        self
    }

    /// 是否使用多行搜索器进行搜索。
    ///
    /// 默认情况下，启用此选项。
    #[allow(dead_code)]
    pub fn multi_line(&mut self, yes: bool) -> &mut SearcherTester {
        self.multi_line = yes;
        self
    }

    /// 是否执行反向匹配搜索。
    ///
    /// 默认情况下，禁用此选项。
    pub fn invert_match(&mut self, yes: bool) -> &mut SearcherTester {
        self.invert_match = yes;
        self
    }

    /// 是否在所有搜索中启用二进制检测。
    ///
    /// 默认情况下，禁用此选项。
    pub fn binary_detection(
        &mut self,
        detection: BinaryDetection,
    ) -> &mut SearcherTester {
        self.binary = detection;
        self
    }
    /// 是否自动尝试测试堆限制设置。
    ///
    /// 默认情况下，其中一个测试配置包括将堆限制设置为正常操作的最小值，以检查一切是否在极端情况下都正常工作。
    /// 然而，在某些情况下，堆限制可能会（有意地）略微改变输出。例如，它可以影响执行二进制检测时搜索的字节数。
    /// 为了方便起见，可以禁用自动堆限制测试。
    pub fn auto_heap_limit(&mut self, yes: bool) -> &mut SearcherTester {
        self.auto_heap_limit = yes;
        self
    }

    /// 设置在“after”上下文中包含的行数。
    ///
    /// 默认值为 `0`，等效于不打印任何上下文。
    pub fn after_context(&mut self, lines: usize) -> &mut SearcherTester {
        self.after_context = lines;
        self
    }

    /// 设置在“before”上下文中包含的行数。
    ///
    /// 默认值为 `0`，等效于不打印任何上下文。
    pub fn before_context(&mut self, lines: usize) -> &mut SearcherTester {
        self.before_context = lines;
        self
    }

    /// 是否启用“passthru”功能。
    ///
    /// 启用 passthru 时，它会将所有不匹配的行实际上视为上下文行。换句话说，启用此功能类似于请求不受限制的前后上下文行数。
    ///
    /// 默认情况下，禁用此功能。
    pub fn passthru(&mut self, yes: bool) -> &mut SearcherTester {
        self.passthru = yes;
        self
    }

    /// 返回用于成功搜索所需的缓冲区的最小大小。
    ///
    /// 通常，这对应于最长行的最大长度（包括其终止符）。但如果启用了上下文设置，则这必须包括最长 N 行的总和。
    ///
    /// 请注意，这必须考虑测试是否使用了多行搜索，因为多行搜索需要将整个文本都放入内存中。
    fn minimal_heap_limit(&self, multi_line: bool) -> usize {
        if multi_line {
            1 + self.haystack.len()
        } else if self.before_context == 0 && self.after_context == 0 {
            1 + self.haystack.lines().map(|s| s.len()).max().unwrap_or(0)
        } else {
            let mut lens: Vec<usize> =
                self.haystack.lines().map(|s| s.len()).collect();
            lens.sort();
            lens.reverse();

            let context_count = if self.passthru {
                self.haystack.lines().count()
            } else {
                // 为什么这里加 2？首先，我们需要添加 1 以便至少搜索一行。
                // 我们再加 1 是因为在处理上下文时，实现有时会包含额外的一行。
                // 没有特别好的理由，只是为了保持实现的简单性。
                2 + self.before_context + self.after_context
            };

            // 我们对每一行都加上 1，因为 `str::lines` 不包括行终止符。
            lens.into_iter()
                .take(context_count)
                .map(|len| len + 1)
                .sum::<usize>()
        }
    }

    /// 如果且仅如果给定的标签应包含在执行 `test` 时中，返回 true。
    ///
    /// 包含由指定的过滤器决定。如果没有给定过滤器，则始终返回 `true`。
    fn include(&self, label: &str) -> bool {
        let re = match self.filter {
            None => return true,
            Some(ref re) => re,
        };
        re.is_match(label)
    }

    /// Configs 生成应该测试的所有搜索配置的集合。生成的配置基于此构建器中的配置。
    fn configs(&self) -> Vec<TesterConfig> {
        let mut configs = vec![];

        let matcher = RegexMatcher::new(&self.pattern);
        let mut builder = SearcherBuilder::new();
        builder
            .line_number(false)
            .invert_match(self.invert_match)
            .binary_detection(self.binary.clone())
            .after_context(self.after_context)
            .before_context(self.before_context)
            .passthru(self.passthru);

        if self.by_line {
            let mut matcher = matcher.clone();
            let mut builder = builder.clone();

            let expected_reader =
                self.expected_no_line_number.as_ref().unwrap().to_string();
            let expected_slice = match self.expected_slice_no_line_number {
                None => expected_reader.clone(),
                Some(ref e) => e.to_string(),
            };
            configs.push(TesterConfig {
                label: "byline-noterm-nonumber".to_string(),
                expected_reader: expected_reader.clone(),
                expected_slice: expected_slice.clone(),
                builder: builder.clone(),
                matcher: matcher.clone(),
            });

            if self.auto_heap_limit {
                builder.heap_limit(Some(self.minimal_heap_limit(false)));
                configs.push(TesterConfig {
                    label: "byline-noterm-nonumber-heaplimit".to_string(),
                    expected_reader: expected_reader.clone(),
                    expected_slice: expected_slice.clone(),
                    builder: builder.clone(),
                    matcher: matcher.clone(),
                });
                builder.heap_limit(None);
            }

            matcher.set_line_term(Some(LineTerminator::byte(b'\n')));
            configs.push(TesterConfig {
                label: "byline-term-nonumber".to_string(),
                expected_reader: expected_reader.clone(),
                expected_slice: expected_slice.clone(),
                builder: builder.clone(),
                matcher: matcher.clone(),
            });

            matcher.every_line_is_candidate(true);
            configs.push(TesterConfig {
                label: "byline-term-nonumber-candidates".to_string(),
                expected_reader: expected_reader.clone(),
                expected_slice: expected_slice.clone(),
                builder: builder.clone(),
                matcher: matcher.clone(),
            });
        }
        if self.by_line && self.line_number {
            let mut matcher = matcher.clone();
            let mut builder = builder.clone();

            let expected_reader =
                self.expected_with_line_number.as_ref().unwrap().to_string();
            let expected_slice = match self.expected_slice_with_line_number {
                None => expected_reader.clone(),
                Some(ref e) => e.to_string(),
            };

            builder.line_number(true);
            configs.push(TesterConfig {
                label: "byline-noterm-number".to_string(),
                expected_reader: expected_reader.clone(),
                expected_slice: expected_slice.clone(),
                builder: builder.clone(),
                matcher: matcher.clone(),
            });

            matcher.set_line_term(Some(LineTerminator::byte(b'\n')));
            configs.push(TesterConfig {
                label: "byline-term-number".to_string(),
                expected_reader: expected_reader.clone(),
                expected_slice: expected_slice.clone(),
                builder: builder.clone(),
                matcher: matcher.clone(),
            });

            matcher.every_line_is_candidate(true);
            configs.push(TesterConfig {
                label: "byline-term-number-candidates".to_string(),
                expected_reader: expected_reader.clone(),
                expected_slice: expected_slice.clone(),
                builder: builder.clone(),
                matcher: matcher.clone(),
            });
        }
        if self.multi_line {
            let mut builder = builder.clone();
            let expected_slice = match self.expected_slice_no_line_number {
                None => {
                    self.expected_no_line_number.as_ref().unwrap().to_string()
                }
                Some(ref e) => e.to_string(),
            };

            builder.multi_line(true);
            configs.push(TesterConfig {
                label: "multiline-nonumber".to_string(),
                expected_reader: expected_slice.clone(),
                expected_slice: expected_slice.clone(),
                builder: builder.clone(),
                matcher: matcher.clone(),
            });

            if self.auto_heap_limit {
                builder.heap_limit(Some(self.minimal_heap_limit(true)));
                configs.push(TesterConfig {
                    label: "multiline-nonumber-heaplimit".to_string(),
                    expected_reader: expected_slice.clone(),
                    expected_slice: expected_slice.clone(),
                    builder: builder.clone(),
                    matcher: matcher.clone(),
                });
                builder.heap_limit(None);
            }
        }
        if self.multi_line && self.line_number {
            let mut builder = builder.clone();
            let expected_slice = match self.expected_slice_with_line_number {
                None => self
                    .expected_with_line_number
                    .as_ref()
                    .unwrap()
                    .to_string(),
                Some(ref e) => e.to_string(),
            };

            builder.multi_line(true);
            builder.line_number(true);
            configs.push(TesterConfig {
                label: "multiline-number".to_string(),
                expected_reader: expected_slice.clone(),
                expected_slice: expected_slice.clone(),
                builder: builder.clone(),
                matcher: matcher.clone(),
            });

            builder.heap_limit(Some(self.minimal_heap_limit(true)));
            configs.push(TesterConfig {
                label: "multiline-number-heaplimit".to_string(),
                expected_reader: expected_slice.clone(),
                expected_slice: expected_slice.clone(),
                builder: builder.clone(),
                matcher: matcher.clone(),
            });
            builder.heap_limit(None);
        }
        configs
    }
}
#[derive(Debug)]
struct TesterConfig {
    label: String,            // 配置标签
    expected_reader: String,  // 预期的读取器搜索结果
    expected_slice: String,   // 预期的切片搜索结果
    builder: SearcherBuilder, // 搜索器构建器
    matcher: RegexMatcher,    // 正则表达式匹配器
}

impl TesterConfig {
    /// 使用读取器执行搜索。这会使用增量搜索策略，其中不一定需要一次性在内存中加载整个语料库。
    fn search_reader(&self, haystack: &str) -> String {
        let mut sink = KitchenSink::new(); // 创建一个 "KitchenSink" 实例来收集搜索结果
        let mut searcher = self.builder.build(); // 使用配置中的构建器创建搜索器实例
        let result = searcher.search_reader(
            &self.matcher,
            haystack.as_bytes(),
            &mut sink,
        ); // 执行搜索
        if let Err(err) = result {
            let label = format!("reader-{}", self.label);
            panic!("运行'{}'时出错: {}", label, err); // 如果出错，输出错误信息
        }
        String::from_utf8(sink.as_bytes().to_vec()).unwrap() // 将搜索结果转换为字符串返回
    }

    /// 使用切片执行搜索。这会使用一次性将整个语料库内容加载到内存中的搜索例程。
    fn search_slice(&self, haystack: &str) -> String {
        let mut sink = KitchenSink::new(); // 创建一个 "KitchenSink" 实例来收集搜索结果
        let mut searcher = self.builder.build(); // 使用配置中的构建器创建搜索器实例
        let result = searcher.search_slice(
            &self.matcher,
            haystack.as_bytes(),
            &mut sink,
        ); // 执行搜索
        if let Err(err) = result {
            let label = format!("slice-{}", self.label);
            panic!("运行'{}'时出错: {}", label, err); // 如果出错，输出错误信息
        }
        String::from_utf8(sink.as_bytes().to_vec()).unwrap() // 将搜索结果转换为字符串返回
    }
}

#[cfg(test)]
mod tests {
    use grep_matcher::{Match, Matcher};

    use super::*;

    fn m(start: usize, end: usize) -> Match {
        Match::new(start, end)
    }

    #[test]
    fn empty_line1() {
        let haystack = b"";
        let matcher = RegexMatcher::new(r"^$");

        assert_eq!(matcher.find_at(haystack, 0), Ok(Some(m(0, 0))));
    }

    #[test]
    fn empty_line2() {
        let haystack = b"\n";
        let matcher = RegexMatcher::new(r"^$");

        assert_eq!(matcher.find_at(haystack, 0), Ok(Some(m(0, 0))));
        assert_eq!(matcher.find_at(haystack, 1), Ok(Some(m(1, 1))));
    }

    #[test]
    fn empty_line3() {
        let haystack = b"\n\n";
        let matcher = RegexMatcher::new(r"^$");

        assert_eq!(matcher.find_at(haystack, 0), Ok(Some(m(0, 0))));
        assert_eq!(matcher.find_at(haystack, 1), Ok(Some(m(1, 1))));
        assert_eq!(matcher.find_at(haystack, 2), Ok(Some(m(2, 2))));
    }

    #[test]
    fn empty_line4() {
        let haystack = b"a\n\nb\n";
        let matcher = RegexMatcher::new(r"^$");

        assert_eq!(matcher.find_at(haystack, 0), Ok(Some(m(2, 2))));
        assert_eq!(matcher.find_at(haystack, 1), Ok(Some(m(2, 2))));
        assert_eq!(matcher.find_at(haystack, 2), Ok(Some(m(2, 2))));
        assert_eq!(matcher.find_at(haystack, 3), Ok(Some(m(5, 5))));
        assert_eq!(matcher.find_at(haystack, 4), Ok(Some(m(5, 5))));
        assert_eq!(matcher.find_at(haystack, 5), Ok(Some(m(5, 5))));
    }

    #[test]
    fn empty_line5() {
        let haystack = b"a\n\nb\nc";
        let matcher = RegexMatcher::new(r"^$");

        assert_eq!(matcher.find_at(haystack, 0), Ok(Some(m(2, 2))));
        assert_eq!(matcher.find_at(haystack, 1), Ok(Some(m(2, 2))));
        assert_eq!(matcher.find_at(haystack, 2), Ok(Some(m(2, 2))));
        assert_eq!(matcher.find_at(haystack, 3), Ok(None));
        assert_eq!(matcher.find_at(haystack, 4), Ok(None));
        assert_eq!(matcher.find_at(haystack, 5), Ok(None));
        assert_eq!(matcher.find_at(haystack, 6), Ok(None));
    }

    #[test]
    fn empty_line6() {
        let haystack = b"a\n";
        let matcher = RegexMatcher::new(r"^$");

        assert_eq!(matcher.find_at(haystack, 0), Ok(Some(m(2, 2))));
        assert_eq!(matcher.find_at(haystack, 1), Ok(Some(m(2, 2))));
        assert_eq!(matcher.find_at(haystack, 2), Ok(Some(m(2, 2))));
    }
}
