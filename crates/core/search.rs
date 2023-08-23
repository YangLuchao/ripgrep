use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use grep::cli;
use grep::matcher::Matcher;
#[cfg(feature = "pcre2")]
use grep::pcre2::RegexMatcher as PCRE2RegexMatcher;
use grep::printer::{Standard, Stats, Summary, JSON};
use grep::regex::RegexMatcher as RustRegexMatcher;
use grep::searcher::{BinaryDetection, Searcher};
use ignore::overrides::Override;
use serde_json as json;
use serde_json::json;
use termcolor::WriteColor;

use crate::subject::Subject;

/// 搜索工作者的配置。除了其他一些设置，配置主要控制如何向用户展示搜索结果的高级设置。
#[derive(Clone, Debug)]
struct Config {
    json_stats: bool,
    preprocessor: Option<PathBuf>,
    preprocessor_globs: Override,
    search_zip: bool,
    binary_implicit: BinaryDetection,
    binary_explicit: BinaryDetection,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            json_stats: false,
            preprocessor: None,
            preprocessor_globs: Override::empty(),
            search_zip: false,
            binary_implicit: BinaryDetection::none(),
            binary_explicit: BinaryDetection::none(),
        }
    }
}

/// 用于配置和构建搜索工作者的构建器。
#[derive(Clone, Debug)]
pub struct SearchWorkerBuilder {
    config: Config,
    command_builder: cli::CommandReaderBuilder,
    decomp_builder: cli::DecompressionReaderBuilder,
}

impl Default for SearchWorkerBuilder {
    fn default() -> SearchWorkerBuilder {
        SearchWorkerBuilder::new()
    }
}

impl SearchWorkerBuilder {
    /// 创建一个新的构建器，用于配置和构建搜索工作者。
    pub fn new() -> SearchWorkerBuilder {
        let mut cmd_builder = cli::CommandReaderBuilder::new();
        cmd_builder.async_stderr(true);

        let mut decomp_builder = cli::DecompressionReaderBuilder::new();
        decomp_builder.async_stderr(true);

        SearchWorkerBuilder {
            config: Config::default(),
            command_builder: cmd_builder,
            decomp_builder,
        }
    }

    /// 使用给定的搜索器、匹配器和打印机创建一个新的搜索工作者。
    pub fn build<W: WriteColor>(
        &self,
        matcher: PatternMatcher,
        searcher: Searcher,
        printer: Printer<W>,
    ) -> SearchWorker<W> {
        let config = self.config.clone();
        let command_builder = self.command_builder.clone();
        let decomp_builder = self.decomp_builder.clone();
        SearchWorker {
            config,
            command_builder,
            decomp_builder,
            matcher,
            searcher,
            printer,
        }
    }

    /// 强制使用 JSON 输出统计数据，即使底层打印机不是 JSON 打印机。
    ///
    /// 这在实现诸如 `--json --quiet` 这样的标志组合时很有用，它使用摘要打印机来实现
    /// `--quiet`，但仍希望以 JSON 格式输出摘要统计数据，因为有 `--json` 标志。
    pub fn json_stats(&mut self, yes: bool) -> &mut SearchWorkerBuilder {
        self.config.json_stats = yes;
        self
    }

    /// 设置预处理器命令的路径。
    ///
    /// 当设置了预处理器命令时，将不直接搜索文件，而是使用文件路径作为第一个参数运行给定的命令，
    /// 并搜索该命令的输出。
    pub fn preprocessor(
        &mut self,
        cmd: Option<PathBuf>,
    ) -> crate::Result<&mut SearchWorkerBuilder> {
        if let Some(ref prog) = cmd {
            let bin = cli::resolve_binary(prog)?;
            self.config.preprocessor = Some(bin);
        } else {
            self.config.preprocessor = None;
        }
        Ok(self)
    }

    /// 设置用于确定应该通过预处理器运行哪些文件的文件模式。
    ///
    /// 默认情况下，如果没有文件模式且设置了预处理器，则会将每个文件都通过预处理器运行。
    pub fn preprocessor_globs(
        &mut self,
        globs: Override,
    ) -> &mut SearchWorkerBuilder {
        self.config.preprocessor_globs = globs;
        self
    }

    /// 启用对常见压缩文件的解压缩和搜索。
    ///
    /// 启用后，如果识别出某个文件路径为压缩文件，则在搜索之前将其解压缩。
    ///
    /// 请注意，如果设置了预处理器命令，则该设置将被覆盖。
    pub fn search_zip(&mut self, yes: bool) -> &mut SearchWorkerBuilder {
        self.config.search_zip = yes;
        self
    }

    /// 设置递归目录搜索时应使用的二进制检测。
    ///
    /// 一般来说，这个二进制检测可能是 `BinaryDetection::quit`，如果我们想完全跳过二进制文件的话。
    ///
    /// 默认情况下，不执行任何二进制检测。
    pub fn binary_detection_implicit(
        &mut self,
        detection: BinaryDetection,
    ) -> &mut SearchWorkerBuilder {
        self.config.binary_implicit = detection;
        self
    }

    /// 设置应在搜索由最终用户明确提供的文件时使用的二进制检测。
    ///
    /// 一般来说，这个二进制检测不应该是 `BinaryDetection::quit`，因为我们永远不希望自动过滤最终用户提供的文件。
    ///
    /// 默认情况下，不执行任何二进制检测。
    pub fn binary_detection_explicit(
        &mut self,
        detection: BinaryDetection,
    ) -> &mut SearchWorkerBuilder {
        self.config.binary_explicit = detection;
        self
    }
}

/// 搜索执行的结果。
///
/// 一般来说，搜索的“结果”会发送给打印机，将结果写入底层写入器，如 stdout 或文件。但是，每个搜索还有一些可能对更高级别的例程有用的聚合统计或元数据。
#[derive(Clone, Debug, Default)]
pub struct SearchResult {
    has_match: bool,
    stats: Option<Stats>,
}

impl SearchResult {
    /// 搜索是否找到匹配项。
    pub fn has_match(&self) -> bool {
        self.has_match
    }

    /// 返回单个搜索的聚合统计信息，如果可用。
    ///
    /// 计算统计信息可能很昂贵，因此只有在打印机中显式启用时才会出现。
    pub fn stats(&self) -> Option<&Stats> {
        self.stats.as_ref()
    }
}

/// 用于搜索工作者的模式匹配器。
#[derive(Clone, Debug)]
pub enum PatternMatcher {
    RustRegex(RustRegexMatcher),
    #[cfg(feature = "pcre2")]
    PCRE2(PCRE2RegexMatcher),
}

/// 搜索工作者使用的打印机。
///
/// `W` 类型参数指的是底层写入器的类型。
#[derive(Debug)]
pub enum Printer<W> {
    /// 使用标准打印机，支持经典的类似 grep 的格式。
    Standard(Standard<W>),
    /// 使用摘要打印机，支持搜索结果的聚合显示。
    Summary(Summary<W>),
    /// JSON 打印机，以 JSON Lines 格式输出结果。
    JSON(JSON<W>),
}

impl<W: WriteColor> Printer<W> {
    fn print_stats(
        &mut self,
        total_duration: Duration,
        stats: &Stats,
    ) -> io::Result<()> {
        match *self {
            Printer::JSON(_) => self.print_stats_json(total_duration, stats),
            Printer::Standard(_) | Printer::Summary(_) => {
                self.print_stats_human(total_duration, stats)
            }
        }
    }

    fn print_stats_human(
        &mut self,
        total_duration: Duration,
        stats: &Stats,
    ) -> io::Result<()> {
        write!(
            self.get_mut(),
            "
{matches} 匹配项
{lines} 匹配行数
{searches_with_match} 包含匹配项的文件
{searches} 搜索的文件数
{bytes_printed} 打印的字节数
{bytes_searched} 搜索的字节数
{search_time:0.6} 秒用于搜索
{process_time:0.6} 秒
",
            matches = stats.matches(),
            lines = stats.matched_lines(),
            searches_with_match = stats.searches_with_match(),
            searches = stats.searches(),
            bytes_printed = stats.bytes_printed(),
            bytes_searched = stats.bytes_searched(),
            search_time = fractional_seconds(stats.elapsed()),
            process_time = fractional_seconds(total_duration)
        )
    }

    fn print_stats_json(
        &mut self,
        total_duration: Duration,
        stats: &Stats,
    ) -> io::Result<()> {
        // 我们特意匹配 grep-printer crate 中 JSON 打印机规定的格式。我们只是在其中添加了 'summary' 消息类型。
        let fractional = fractional_seconds(total_duration);
        json::to_writer(
            self.get_mut(),
            &json!({
                "type": "summary",
                "data": {
                    "stats": stats,
                    "elapsed_total": {
                        "secs": total_duration.as_secs(),
                        "nanos": total_duration.subsec_nanos(),
                        "human": format!("{:0.6}s", fractional),
                    },
                }
            }),
        )?;
        write!(self.get_mut(), "\n")
    }

    /// 返回对底层打印机的写入器的可变引用。
    pub fn get_mut(&mut self) -> &mut W {
        match *self {
            Printer::Standard(ref mut p) => p.get_mut(),
            Printer::Summary(ref mut p) => p.get_mut(),
            Printer::JSON(ref mut p) => p.get_mut(),
        }
    }
}

/// 执行搜索的工作者。
///
/// 执行多次搜索时，通常一个工作者会执行多个搜索，一般来说，它应该在单个线程中使用。当使用多个线程进行搜索时，最好为每个线程创建一个新的工作者。
#[derive(Debug)]
pub struct SearchWorker<W> {
    config: Config,
    command_builder: cli::CommandReaderBuilder,
    decomp_builder: cli::DecompressionReaderBuilder,
    matcher: PatternMatcher,
    searcher: Searcher,
    printer: Printer<W>,
}

impl<W: WriteColor> SearchWorker<W> {
    /// 在给定主题上执行搜索。
    pub fn search(&mut self, subject: &Subject) -> io::Result<SearchResult> {
        let bin = if subject.is_explicit() {
            self.config.binary_explicit.clone()
        } else {
            self.config.binary_implicit.clone()
        };
        let path = subject.path();
        log::trace!("{}: 二进制检测: {:?}", path.display(), bin);

        self.searcher.set_binary_detection(bin);
        if subject.is_stdin() {
            self.search_reader(path, &mut io::stdin().lock())
        } else if self.should_preprocess(path) {
            self.search_preprocessor(path)
        } else if self.should_decompress(path) {
            self.search_decompress(path)
        } else {
            self.search_path(path)
        }
    }

    /// 返回对底层打印机的可变引用。
    pub fn printer(&mut self) -> &mut Printer<W> {
        &mut self.printer
    }

    /// 将给定的统计信息以与此搜索器的打印机的格式一致的方式打印到底层写入器中。
    ///
    /// 虽然 `Stats` 自己包含了一个持续时间，但这仅对搜索所花费的时间进行了统计，而 `total_duration` 应该大致估计了 ripgrep 进程本身的生命周期。
    pub fn print_stats(
        &mut self,
        total_duration: Duration,
        stats: &Stats,
    ) -> io::Result<()> {
        if self.config.json_stats {
            self.printer().print_stats_json(total_duration, stats)
        } else {
            self.printer().print_stats(total_duration, stats)
        }
    }

    /// 如果且仅如果给定的文件路径应在搜索之前被解压缩，则返回 `true`。
    fn should_decompress(&self, path: &Path) -> bool {
        if !self.config.search_zip {
            return false;
        }
        self.decomp_builder.get_matcher().has_command(path)
    }

    /// 如果且仅如果给定的文件路径应由预处理器运行，则返回 `true`。
    fn should_preprocess(&self, path: &Path) -> bool {
        if !self.config.preprocessor.is_some() {
            return false;
        }
        if self.config.preprocessor_globs.is_empty() {
            return true;
        }
        !self.config.preprocessor_globs.matched(path, false).is_ignore()
    }

    /// 使用给定的匹配器、搜索器和打印机搜索给定文件路径。
    fn search_path(&mut self, path: &Path) -> io::Result<SearchResult> {
        use self::PatternMatcher::*;

        let (searcher, printer) = (&mut self.searcher, &mut self.printer);
        match self.matcher {
            RustRegex(ref m) => search_path(m, searcher, printer, path),
            #[cfg(feature = "pcre2")]
            PCRE2(ref m) => search_path(m, searcher, printer, path),
        }
    }

    /// 使用给定的匹配器、搜索器和打印机在给定读取器上执行搜索。
    fn search_reader<R: io::Read>(
        &mut self,
        path: &Path,
        rdr: &mut R,
    ) -> io::Result<SearchResult> {
        use self::PatternMatcher::*;

        let (searcher, printer) = (&mut self.searcher, &mut self.printer);
        match self.matcher {
            RustRegex(ref m) => search_reader(m, searcher, printer, path, rdr),
            #[cfg(feature = "pcre2")]
            PCRE2(ref m) => search_reader(m, searcher, printer, path, rdr),
        }
    }
}

/// 使用给定的匹配器、搜索器和打印机在给定文件路径上执行搜索。
fn search_path<M: Matcher, W: WriteColor>(
    matcher: M,
    searcher: &mut Searcher,
    printer: &mut Printer<W>,
    path: &Path,
) -> io::Result<SearchResult> {
    match *printer {
        Printer::Standard(ref mut p) => {
            let mut sink = p.sink_with_path(&matcher, path);
            searcher.search_path(&matcher, path, &mut sink)?;
            Ok(SearchResult {
                has_match: sink.has_match(),
                stats: sink.stats().map(|s| s.clone()),
            })
        }
        Printer::Summary(ref mut p) => {
            let mut sink = p.sink_with_path(&matcher, path);
            searcher.search_path(&matcher, path, &mut sink)?;
            Ok(SearchResult {
                has_match: sink.has_match(),
                stats: sink.stats().map(|s| s.clone()),
            })
        }
        Printer::JSON(ref mut p) => {
            let mut sink = p.sink_with_path(&matcher, path);
            searcher.search_path(&matcher, path, &mut sink)?;
            Ok(SearchResult {
                has_match: sink.has_match(),
                stats: Some(sink.stats().clone()),
            })
        }
    }
}

/// 使用给定的匹配器、搜索器和打印机在给定读取器上执行搜索。
fn search_reader<M: Matcher, R: io::Read, W: WriteColor>(
    matcher: M,
    searcher: &mut Searcher,
    printer: &mut Printer<W>,
    path: &Path,
    mut rdr: R,
) -> io::Result<SearchResult> {
    match *printer {
        Printer::Standard(ref mut p) => {
            let mut sink = p.sink_with_path(&matcher, path);
            searcher.search_reader(&matcher, &mut rdr, &mut sink)?;
            Ok(SearchResult {
                has_match: sink.has_match(),
                stats: sink.stats().map(|s| s.clone()),
            })
        }
        Printer::Summary(ref mut p) => {
            let mut sink = p.sink_with_path(&matcher, path);
            searcher.search_reader(&matcher, &mut rdr, &mut sink)?;
            Ok(SearchResult {
                has_match: sink.has_match(),
                stats: sink.stats().map(|s| s.clone()),
            })
        }
        Printer::JSON(ref mut p) => {
            let mut sink = p.sink_with_path(&matcher, path);
            searcher.search_reader(&matcher, &mut rdr, &mut sink)?;
            Ok(SearchResult {
                has_match: sink.has_match(),
                stats: Some(sink.stats().clone()),
            })
        }
    }
}

/// 将给定持续时间格式化为精确到小数点后六位的秒数。
fn fractional_seconds(duration: Duration) -> f64 {
    duration.as_secs() as f64 + f64::from(duration.subsec_nanos()) * 1e-9
}
