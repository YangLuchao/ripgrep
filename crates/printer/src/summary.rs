use std::cell::RefCell;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use grep_matcher::Matcher;
use grep_searcher::{Searcher, Sink, SinkFinish, SinkMatch};
use termcolor::{ColorSpec, NoColor, WriteColor};

use crate::color::ColorSpecs;
use crate::counter::CounterWriter;
use crate::stats::Stats;
use crate::util::{find_iter_at_in_context, PrinterPath};
/// 概要打印机的配置。
///
/// 这由 `SummaryBuilder` 进行操作，然后由实际实现引用。一旦打印机构建完成，配置就被冻结，无法更改。
#[derive(Debug, Clone)]
struct Config {
    kind: SummaryKind,             // 概要类型
    colors: ColorSpecs,            // 颜色规范
    stats: bool,                   // 是否收集统计信息
    path: bool,                    // 是否包含文件路径
    max_matches: Option<u64>,      // 最大匹配数限制
    exclude_zero: bool,            // 是否排除零匹配结果
    separator_field: Arc<Vec<u8>>, // 字段分隔符
    separator_path: Option<u8>,    // 文件路径分隔符
    path_terminator: Option<u8>,   // 文件路径终止符
}

impl Default for Config {
    fn default() -> Config {
        Config {
            kind: SummaryKind::Count,
            colors: ColorSpecs::default(),
            stats: false,
            path: true,
            max_matches: None,
            exclude_zero: true,
            separator_field: Arc::new(b":".to_vec()),
            separator_path: None,
            path_terminator: None,
        }
    }
}

/// 概要输出类型（如果有的话）。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SummaryKind {
    /// 计数模式
    Count,
    /// 计数匹配模式
    CountMatches,
    /// 包含匹配的路径模式
    PathWithMatch,
    /// 不包含匹配的路径模式
    PathWithoutMatch,
    /// 静默模式
    Quiet,
}

impl SummaryKind {
    /// 判断该输出模式是否需要文件路径。
    ///
    /// 当输出模式需要文件路径时，概要打印机会在每次搜索开始时报告错误，如果缺少文件路径。
    fn requires_path(&self) -> bool {
        use self::SummaryKind::*;

        match *self {
            PathWithMatch | PathWithoutMatch => true,
            Count | CountMatches | Quiet => false,
        }
    }

    /// 判断该输出模式是否需要统计信息，无论是否已启用。
    fn requires_stats(&self) -> bool {
        use self::SummaryKind::*;

        match *self {
            CountMatches => true,
            Count | PathWithMatch | PathWithoutMatch | Quiet => false,
        }
    }

    /// 判断是否在找到第一个匹配后可以退出的输出模式。
    fn quit_early(&self) -> bool {
        use self::SummaryKind::*;

        match *self {
            PathWithMatch | Quiet => true,
            Count | CountMatches | PathWithoutMatch => false,
        }
    }
}

/// 概要打印机的构建器。
#[derive(Clone, Debug)]
pub struct SummaryBuilder {
    config: Config,
}

impl SummaryBuilder {
    /// 创建一个新的概要打印机构建器。
    pub fn new() -> SummaryBuilder {
        SummaryBuilder { config: Config::default() }
    }

    /// 用任何实现了 `termcolor::WriteColor` 的对象构建打印机。
    pub fn build<W: WriteColor>(&self, wtr: W) -> Summary<W> {
        Summary {
            config: self.config.clone(),
            wtr: RefCell::new(CounterWriter::new(wtr)),
        }
    }

    /// 从任何实现了 `io::Write` 的对象构建打印机，不使用颜色。
    pub fn build_no_color<W: io::Write>(&self, wtr: W) -> Summary<NoColor<W>> {
        self.build(NoColor::new(wtr))
    }

    /// 设置打印机的输出模式。
    pub fn kind(&mut self, kind: SummaryKind) -> &mut SummaryBuilder {
        self.config.kind = kind;
        self
    }

    /// 设置用于颜色的用户颜色规范。
    pub fn color_specs(&mut self, specs: ColorSpecs) -> &mut SummaryBuilder {
        self.config.colors = specs;
        self
    }

    /// 启用/禁用收集统计信息。
    pub fn stats(&mut self, yes: bool) -> &mut SummaryBuilder {
        self.config.stats = yes;
        self
    }

    /// 启用/禁用路径的显示。
    pub fn path(&mut self, yes: bool) -> &mut SummaryBuilder {
        self.config.path = yes;
        self
    }

    /// 设置最大匹配数限制。
    pub fn max_matches(&mut self, limit: Option<u64>) -> &mut SummaryBuilder {
        self.config.max_matches = limit;
        self
    }

    /// 启用/禁用排除零匹配结果。
    pub fn exclude_zero(&mut self, yes: bool) -> &mut SummaryBuilder {
        self.config.exclude_zero = yes;
        self
    }

    /// 设置字段分隔符。
    pub fn separator_field(&mut self, sep: Vec<u8>) -> &mut SummaryBuilder {
        self.config.separator_field = Arc::new(sep);
        self
    }

    /// 设置文件路径分隔符。
    pub fn separator_path(&mut self, sep: Option<u8>) -> &mut SummaryBuilder {
        self.config.separator_path = sep;
        self
    }

    /// 设置文件路径终止符。
    pub fn path_terminator(
        &mut self,
        terminator: Option<u8>,
    ) -> &mut SummaryBuilder {
        self.config.path_terminator = terminator;
        self
    }
}
/// 概要打印机，用于输出搜索的聚合结果。
///
/// 聚合结果通常对应于文件路径和/或找到的匹配数。
///
/// 可以使用 `Summary::new` 或 `Summary::new_no_color` 构造函数创建默认打印机。
/// 然而，有许多选项可以配置此打印机的输出。这些选项可以使用 [`SummaryBuilder`](struct.SummaryBuilder.html) 进行配置。
///
/// 此类型对 `W` 泛型，表示任何实现 `termcolor::WriteColor` 特质的类型。
#[derive(Debug)]
pub struct Summary<W> {
    config: Config,                 // 概要配置
    wtr: RefCell<CounterWriter<W>>, // 写入器
}

impl<W: WriteColor> Summary<W> {
    /// 返回一个具有默认配置的概要打印机，将匹配项写入给定的写入器。
    ///
    /// 写入器应该是 `termcolor::WriteColor` 的实现，而不仅仅是 `io::Write` 的实现。
    /// 要使用普通的 `io::Write` 实现（同时放弃颜色），请使用 `new_no_color` 构造函数。
    ///
    /// 默认配置使用 `Count` 概要模式。
    pub fn new(wtr: W) -> Summary<W> {
        SummaryBuilder::new().build(wtr)
    }
}

impl<W: io::Write> Summary<NoColor<W>> {
    /// 返回一个具有默认配置的概要打印机，将匹配项写入给定的写入器。
    ///
    /// 写入器可以是任何 `io::Write` 的实现。使用此构造函数，打印机将永远不会输出颜色。
    ///
    /// 默认配置使用 `Count` 概要模式。
    pub fn new_no_color(wtr: W) -> Summary<NoColor<W>> {
        SummaryBuilder::new().build_no_color(wtr)
    }
}

impl<W: WriteColor> Summary<W> {
    /// 返回一个与概要打印机关联的 `Sink` 实现。
    ///
    /// 这不会关联打印机与文件路径，这意味着此实现永远不会打印文件路径。
    /// 如果此概要打印机的输出模式在没有文件路径的情况下不合理（例如 `PathWithMatch` 或 `PathWithoutMatch`），
    /// 则使用此 sink 执行的任何搜索将立即以错误退出。
    pub fn sink<'s, M: Matcher>(
        &'s mut self,
        matcher: M,
    ) -> SummarySink<'static, 's, M, W> {
        let stats = if self.config.stats || self.config.kind.requires_stats() {
            Some(Stats::new())
        } else {
            None
        };
        SummarySink {
            matcher: matcher,
            summary: self,
            path: None,
            start_time: Instant::now(),
            match_count: 0,
            binary_byte_offset: None,
            stats: stats,
        }
    }

    /// 返回一个与文件路径关联的 `Sink` 实现。
    ///
    /// 当打印机与路径关联时，根据其配置，它可能会打印路径。
    pub fn sink_with_path<'p, 's, M, P>(
        &'s mut self,
        matcher: M,
        path: &'p P,
    ) -> SummarySink<'p, 's, M, W>
    where
        M: Matcher,
        P: ?Sized + AsRef<Path>,
    {
        if !self.config.path && !self.config.kind.requires_path() {
            return self.sink(matcher);
        }
        let stats = if self.config.stats || self.config.kind.requires_stats() {
            Some(Stats::new())
        } else {
            None
        };
        let ppath = PrinterPath::with_separator(
            path.as_ref(),
            self.config.separator_path,
        );
        SummarySink {
            matcher: matcher,
            summary: self,
            path: Some(ppath),
            start_time: Instant::now(),
            match_count: 0,
            binary_byte_offset: None,
            stats: stats,
        }
    }
}

impl<W> Summary<W> {
    /// 当且仅当此打印机已在之前的任何搜索中写入至少一个字节到底层写入器时返回 true。
    pub fn has_written(&self) -> bool {
        self.wtr.borrow().total_count() > 0
    }

    /// 返回对底层写入器的可变引用。
    pub fn get_mut(&mut self) -> &mut W {
        self.wtr.get_mut().get_mut()
    }

    /// 消耗此打印机并返回底层写入器的所有权。
    pub fn into_inner(self) -> W {
        self.wtr.into_inner().into_inner()
    }
}
/// 与匹配器和可选文件路径关联的概要打印机的 `Sink` 实现。
///
/// 此类型对几个类型参数进行了泛型处理：
///
/// * `'p` 表示文件路径的生命周期，如果提供了文件路径。当没有提供文件路径时，为 `'static`。
/// * `'s` 表示此类型借用的 [`Summary`](struct.Summary.html) 打印机的生命周期。
/// * `M` 表示由 `grep_searcher::Searcher` 使用的匹配器的类型，该匹配器向此 sink 报告结果。
/// * `W` 表示此打印机将其输出写入的底层写入器。
#[derive(Debug)]
pub struct SummarySink<'p, 's, M: Matcher, W> {
    matcher: M,                      // 匹配器实例
    summary: &'s mut Summary<W>,     // 概要打印机引用
    path: Option<PrinterPath<'p>>,   // 可选的文件路径
    start_time: Instant,             // 记录开始时间
    match_count: u64,                // 匹配项计数
    binary_byte_offset: Option<u64>, // 二进制数据的偏移量
    stats: Option<Stats>,            // 统计信息
}

impl<'p, 's, M: Matcher, W: WriteColor> SummarySink<'p, 's, M, W> {
    /// 当且仅当此打印机在先前搜索中接收到匹配项时返回 true。
    ///
    /// 这不受先前搜索结果的影响。
    pub fn has_match(&self) -> bool {
        match self.summary.config.kind {
            SummaryKind::PathWithoutMatch => self.match_count == 0,
            _ => self.match_count > 0,
        }
    }

    /// 如果在先前搜索中发现了二进制数据，返回首次检测到二进制数据的偏移量。
    ///
    /// 返回的偏移量是相对于搜索的整个字节集的绝对偏移量。
    ///
    /// 这不受先前搜索结果的影响。例如，如果前一个搜索发现了二进制数据，但前一个搜索未发现二进制数据，则返回 `None`。
    pub fn binary_byte_offset(&self) -> Option<u64> {
        self.binary_byte_offset
    }

    /// 返回对此打印机为在此 sink 上执行的所有搜索生成的统计信息的引用。
    ///
    /// 仅在通过 [`SummaryBuilder`](struct.SummaryBuilder.html) 配置请求统计信息时才返回统计信息。
    pub fn stats(&self) -> Option<&Stats> {
        self.stats.as_ref()
    }

    /// 当且仅当搜索器可能跨多行报告匹配项时，返回 true。
    ///
    /// 请注意，这不仅仅返回搜索器是否处于多行模式，还检查匹配器是否可以跨多行匹配。
    /// 即使搜索器启用了多行模式，如果匹配器无法跨多行匹配，我们也不需要进行多行处理。
    fn multi_line(&self, searcher: &Searcher) -> bool {
        searcher.multi_line_with_matcher(&self.matcher)
    }

    /// 当且仅当此打印机应该退出时，返回 true。
    ///
    /// 这实现了在看到一定数量的匹配项后退出的逻辑。在大多数情况下，逻辑很简单，
    /// 但我们必须允许在达到限制后继续打印所有“之后”的上下文行。
    fn should_quit(&self) -> bool {
        let limit = match self.summary.config.max_matches {
            None => return false,
            Some(limit) => limit,
        };
        self.match_count >= limit
    }

    /// 如果此打印机关联了文件路径，则将该路径写入底层写入器，然后加上行终止符。
    /// （如果设置了路径终止符，则使用路径终止符而不是行终止符。）
    fn write_path_line(&self, searcher: &Searcher) -> io::Result<()> {
        if let Some(ref path) = self.path {
            self.write_spec(
                self.summary.config.colors.path(),
                path.as_bytes(),
            )?;
            if let Some(term) = self.summary.config.path_terminator {
                self.write(&[term])?;
            } else {
                self.write_line_term(searcher)?;
            }
        }
        Ok(())
    }

    /// 如果此打印机关联了文件路径，则将该路径写入底层写入器，然后加上字段分隔符。
    /// （如果设置了路径终止符，则使用路径终止符而不是字段分隔符。）
    fn write_path_field(&self) -> io::Result<()> {
        if let Some(ref path) = self.path {
            self.write_spec(
                self.summary.config.colors.path(),
                path.as_bytes(),
            )?;
            if let Some(term) = self.summary.config.path_terminator {
                self.write(&[term])?;
            } else {
                self.write(&self.summary.config.separator_field)?;
            }
        }
        Ok(())
    }

    /// 写入给定搜索器上配置的行终止符。
    fn write_line_term(&self, searcher: &Searcher) -> io::Result<()> {
        self.write(searcher.line_terminator().as_bytes())
    }

    /// 使用给定的样式写入给定的字节。
    fn write_spec(&self, spec: &ColorSpec, buf: &[u8]) -> io::Result<()> {
        self.summary.wtr.borrow_mut().set_color(spec)?;
        self.write(buf)?;
        self.summary.wtr.borrow_mut().reset()?;
        Ok(())
    }

    /// 写入所有给定的字节。
    fn write(&self, buf: &[u8]) -> io::Result<()> {
        self.summary.wtr.borrow_mut().write_all(buf)
    }
}

impl<'p, 's, M: Matcher, W: WriteColor> Sink for SummarySink<'p, 's, M, W> {
    type Error = io::Error;

    /// 处理匹配项的输出操作，同时更新统计信息。
    fn matched(
        &mut self,
        searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, io::Error> {
        // 判断是否允许多行匹配
        let is_multi_line = self.multi_line(searcher);
        // 计算当前匹配项数量
        let sink_match_count = if self.stats.is_none() && !is_multi_line {
            1
        } else {
            // 这个步骤获取搜索器可以提供的尽可能多的字节。
            // 虽然不能保证完全持有获取匹配项所需的上下文（由于回溯的存在），
            // 但在实践中确实能够正常工作。
            let buf = mat.buffer();
            let range = mat.bytes_range_in_buffer();
            let mut count = 0;
            find_iter_at_in_context(
                searcher,
                &self.matcher,
                buf,
                range,
                |_| {
                    count += 1;
                    true
                },
            )?;
            count
        };
        // 更新匹配项计数
        if is_multi_line {
            self.match_count += sink_match_count;
        } else {
            self.match_count += 1;
        }
        // 更新统计信息
        if let Some(ref mut stats) = self.stats {
            stats.add_matches(sink_match_count);
            stats.add_matched_lines(mat.lines().count() as u64);
        } else if self.summary.config.kind.quit_early() {
            // 如果不在统计模式下且打印机设置为在匹配前退出，则返回 false
            return Ok(false);
        }
        // 判断是否应该继续打印
        Ok(!self.should_quit())
    }

    /// 开始搜索操作，初始化相关计数和状态。
    fn begin(&mut self, _searcher: &Searcher) -> Result<bool, io::Error> {
        // 检查是否需要文件路径，如果需要但未提供，则返回错误
        if self.path.is_none() && self.summary.config.kind.requires_path() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "output kind {:?} requires a file path",
                    self.summary.config.kind,
                ),
            ));
        }
        // 重置输出计数和起始时间
        self.summary.wtr.borrow_mut().reset_count();
        self.start_time = Instant::now();
        self.match_count = 0;
        self.binary_byte_offset = None;
        // 如果设置了最大匹配数量为 0，则返回 false
        if self.summary.config.max_matches == Some(0) {
            return Ok(false);
        }
        // 返回 true 表示搜索开始
        Ok(true)
    }

    /// 结束搜索操作，更新统计信息并根据配置输出结果。
    fn finish(
        &mut self,
        searcher: &Searcher,
        finish: &SinkFinish,
    ) -> Result<(), io::Error> {
        // 更新二进制数据偏移量
        self.binary_byte_offset = finish.binary_byte_offset();
        // 更新统计信息
        if let Some(ref mut stats) = self.stats {
            stats.add_elapsed(self.start_time.elapsed());
            stats.add_searches(1);
            if self.match_count > 0 {
                stats.add_searches_with_match(1);
            }
            stats.add_bytes_searched(finish.byte_count());
            stats.add_bytes_printed(self.summary.wtr.borrow().count());
        }
        // 如果我们的二进制检测方法要求在发现二进制数据后退出，则即使在发现二进制数据之前找到匹配项，
        // 我们也不应该输出任何结果。这里的意图是将 BinaryDetection::quit 作为一种过滤器。
        // 否则，我们可能会呈现一个具有较小匹配项数量的匹配文件，而实际匹配项数量可能要多得多，这可能会产生误导。
        //
        // 如果我们的二进制检测方法是将二进制数据转换，则不会退出，因此会搜索整个文件内容。
        //
        // 在这里存在一个不幸的不一致性。即，当使用 Quiet 或 PathWithMatch 时，打印机可以在看到第一个匹配项后退出，
        // 而这可能在看到二进制数据之前很长时间。这意味着使用 PathWithMatch 可以打印路径，而使用 Count
        // 可能根本不会打印路径，因为存在二进制数据。
        //
        // 除非显著影响 Quiet 或 PathWithMatch 的性能，否则无法修复此问题，因此我们接受这个 bug。
        if self.binary_byte_offset.is_some()
            && searcher.binary_detection().quit_byte().is_some()
        {
            // 将匹配项计数设置为 0。尽管报告的统计信息仍然包含匹配项计数，但“官方”的匹配项计数应为零。
            self.match_count = 0;
            return Ok(());
        }

        // 判断是否应该显示匹配项计数
        let show_count =
            !self.summary.config.exclude_zero || self.match_count > 0;
        // 根据配置输出结果
        match self.summary.config.kind {
            SummaryKind::Count => {
                if show_count {
                    self.write_path_field()?;
                    self.write(self.match_count.to_string().as_bytes())?;
                    self.write_line_term(searcher)?;
                }
            }
            SummaryKind::CountMatches => {
                if show_count {
                    let stats = self
                        .stats
                        .as_ref()
                        .expect("CountMatches should enable stats tracking");
                    self.write_path_field()?;
                    self.write(stats.matches().to_string().as_bytes())?;
                    self.write_line_term(searcher)?;
                }
            }
            SummaryKind::PathWithMatch => {
                if self.match_count > 0 {
                    self.write_path_line(searcher)?;
                }
            }
            SummaryKind::PathWithoutMatch => {
                if self.match_count == 0 {
                    self.write_path_line(searcher)?;
                }
            }
            SummaryKind::Quiet => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use grep_regex::RegexMatcher;
    use grep_searcher::SearcherBuilder;
    use termcolor::NoColor;

    use super::{Summary, SummaryBuilder, SummaryKind};

    const SHERLOCK: &'static [u8] = b"\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
be, to a very large extent, the result of luck. Sherlock Holmes
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.
";

    fn printer_contents(printer: &mut Summary<NoColor<Vec<u8>>>) -> String {
        String::from_utf8(printer.get_mut().get_ref().to_owned()).unwrap()
    }

    #[test]
    fn path_with_match_error() {
        let matcher = RegexMatcher::new(r"Watson").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::PathWithMatch)
            .build_no_color(vec![]);
        let res = SearcherBuilder::new().build().search_reader(
            &matcher,
            SHERLOCK,
            printer.sink(&matcher),
        );
        assert!(res.is_err());
    }

    #[test]
    fn path_without_match_error() {
        let matcher = RegexMatcher::new(r"Watson").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::PathWithoutMatch)
            .build_no_color(vec![]);
        let res = SearcherBuilder::new().build().search_reader(
            &matcher,
            SHERLOCK,
            printer.sink(&matcher),
        );
        assert!(res.is_err());
    }

    #[test]
    fn count_no_path() {
        let matcher = RegexMatcher::new(r"Watson").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::Count)
            .build_no_color(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(&matcher, SHERLOCK, printer.sink(&matcher))
            .unwrap();

        let got = printer_contents(&mut printer);
        assert_eq_printed!("2\n", got);
    }

    #[test]
    fn count_no_path_even_with_path() {
        let matcher = RegexMatcher::new(r"Watson").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::Count)
            .path(false)
            .build_no_color(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(
                &matcher,
                SHERLOCK,
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        assert_eq_printed!("2\n", got);
    }

    #[test]
    fn count_path() {
        let matcher = RegexMatcher::new(r"Watson").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::Count)
            .build_no_color(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(
                &matcher,
                SHERLOCK,
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        assert_eq_printed!("sherlock:2\n", got);
    }

    #[test]
    fn count_path_with_zero() {
        let matcher = RegexMatcher::new(r"NO MATCH").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::Count)
            .exclude_zero(false)
            .build_no_color(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(
                &matcher,
                SHERLOCK,
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        assert_eq_printed!("sherlock:0\n", got);
    }

    #[test]
    fn count_path_without_zero() {
        let matcher = RegexMatcher::new(r"NO MATCH").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::Count)
            .exclude_zero(true)
            .build_no_color(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(
                &matcher,
                SHERLOCK,
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        assert_eq_printed!("", got);
    }

    #[test]
    fn count_path_field_separator() {
        let matcher = RegexMatcher::new(r"Watson").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::Count)
            .separator_field(b"ZZ".to_vec())
            .build_no_color(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(
                &matcher,
                SHERLOCK,
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        assert_eq_printed!("sherlockZZ2\n", got);
    }

    #[test]
    fn count_path_terminator() {
        let matcher = RegexMatcher::new(r"Watson").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::Count)
            .path_terminator(Some(b'\x00'))
            .build_no_color(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(
                &matcher,
                SHERLOCK,
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        assert_eq_printed!("sherlock\x002\n", got);
    }

    #[test]
    fn count_path_separator() {
        let matcher = RegexMatcher::new(r"Watson").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::Count)
            .separator_path(Some(b'\\'))
            .build_no_color(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(
                &matcher,
                SHERLOCK,
                printer.sink_with_path(&matcher, "/home/andrew/sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        assert_eq_printed!("\\home\\andrew\\sherlock:2\n", got);
    }

    #[test]
    fn count_max_matches() {
        let matcher = RegexMatcher::new(r"Watson").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::Count)
            .max_matches(Some(1))
            .build_no_color(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(&matcher, SHERLOCK, printer.sink(&matcher))
            .unwrap();

        let got = printer_contents(&mut printer);
        assert_eq_printed!("1\n", got);
    }

    #[test]
    fn count_matches() {
        let matcher = RegexMatcher::new(r"Watson|Sherlock").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::CountMatches)
            .build_no_color(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(
                &matcher,
                SHERLOCK,
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        assert_eq_printed!("sherlock:4\n", got);
    }

    #[test]
    fn path_with_match_found() {
        let matcher = RegexMatcher::new(r"Watson").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::PathWithMatch)
            .build_no_color(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(
                &matcher,
                SHERLOCK,
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        assert_eq_printed!("sherlock\n", got);
    }

    #[test]
    fn path_with_match_not_found() {
        let matcher = RegexMatcher::new(r"ZZZZZZZZ").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::PathWithMatch)
            .build_no_color(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(
                &matcher,
                SHERLOCK,
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        assert_eq_printed!("", got);
    }

    #[test]
    fn path_without_match_found() {
        let matcher = RegexMatcher::new(r"ZZZZZZZZZ").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::PathWithoutMatch)
            .build_no_color(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(
                &matcher,
                SHERLOCK,
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        assert_eq_printed!("sherlock\n", got);
    }

    #[test]
    fn path_without_match_not_found() {
        let matcher = RegexMatcher::new(r"Watson").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::PathWithoutMatch)
            .build_no_color(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(
                &matcher,
                SHERLOCK,
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        assert_eq_printed!("", got);
    }

    #[test]
    fn quiet() {
        let matcher = RegexMatcher::new(r"Watson|Sherlock").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::Quiet)
            .build_no_color(vec![]);
        let match_count = {
            let mut sink = printer.sink_with_path(&matcher, "sherlock");
            SearcherBuilder::new()
                .build()
                .search_reader(&matcher, SHERLOCK, &mut sink)
                .unwrap();
            sink.match_count
        };

        let got = printer_contents(&mut printer);
        assert_eq_printed!("", got);
        // There is actually more than one match, but Quiet should quit after
        // finding the first one.
        assert_eq!(1, match_count);
    }

    #[test]
    fn quiet_with_stats() {
        let matcher = RegexMatcher::new(r"Watson|Sherlock").unwrap();
        let mut printer = SummaryBuilder::new()
            .kind(SummaryKind::Quiet)
            .stats(true)
            .build_no_color(vec![]);
        let match_count = {
            let mut sink = printer.sink_with_path(&matcher, "sherlock");
            SearcherBuilder::new()
                .build()
                .search_reader(&matcher, SHERLOCK, &mut sink)
                .unwrap();
            sink.match_count
        };

        let got = printer_contents(&mut printer);
        assert_eq_printed!("", got);
        // There is actually more than one match, and Quiet will usually quit
        // after finding the first one, but since we request stats, it will
        // mush on to find all matches.
        assert_eq!(3, match_count);
    }
}
