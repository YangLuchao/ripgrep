use std::cmp;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::SystemTime;

use clap;
use grep::cli;
use grep::matcher::LineTerminator;
#[cfg(feature = "pcre2")]
use grep::pcre2::{
    RegexMatcher as PCRE2RegexMatcher,
    RegexMatcherBuilder as PCRE2RegexMatcherBuilder,
};
use grep::printer::{
    default_color_specs, ColorSpecs, JSONBuilder, Standard, StandardBuilder,
    Stats, Summary, SummaryBuilder, SummaryKind, JSON,
};
use grep::regex::{
    RegexMatcher as RustRegexMatcher,
    RegexMatcherBuilder as RustRegexMatcherBuilder,
};
use grep::searcher::{
    BinaryDetection, Encoding, MmapChoice, Searcher, SearcherBuilder,
};
use ignore::overrides::{Override, OverrideBuilder};
use ignore::types::{FileTypeDef, Types, TypesBuilder};
use ignore::{Walk, WalkBuilder, WalkParallel};
use log;
use termcolor::{BufferWriter, ColorChoice, WriteColor};

use crate::app;
use crate::config;
use crate::logger::Logger;
use crate::messages::{set_ignore_messages, set_messages};
use crate::path_printer::{PathPrinter, PathPrinterBuilder};
use crate::search::{
    PatternMatcher, Printer, SearchWorker, SearchWorkerBuilder,
};
use crate::subject::{Subject, SubjectBuilder};
use crate::Result;

/// ripgrep根据命令行配置执行的命令。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// 只使用一个线程进行搜索。
    Search,
    /// 可能使用多个线程进行搜索。
    SearchParallel,
    /// 命令行参数建议应该进行搜索，但是 ripgrep知道永远找不到匹配(例如，没有给定的模式或——max-count=0)。
    SearchNever,
    /// 显示要搜索的文件，但不实际搜索它们，并只使用一个线程。
    Files,
    /// 显示要搜索的文件，但不实际搜索它们，并使用可能多个线程执行目录遍历。
    FilesParallel,
    /// 列出配置的所有文件类型定义，包括默认文件类型和添加到命令行中的任何其他文件类型。
    Types,
    /// 打印正在使用的PCRE2版本。
    PCRE2Version,
}

impl Command {
    /// 当且仅当此命令需要执行搜索时返回true.
    fn is_search(&self) -> bool {
        use self::Command::*;

        match *self {
            Search | SearchParallel => true,
            SearchNever | Files | FilesParallel | Types | PCRE2Version => {
                false
            }
        }
    }
}

/// 在ripgrep中使用的主配置对象。它为所提供的命令行参数提供了一个高级方便的接口。
///
/// ' Args '对象克隆起来很便宜，并且可以在多个线程同时使用。
#[derive(Clone, Debug)]
pub struct Args(Arc<ArgsImp>);

#[derive(Clone, Debug)]
struct ArgsImp {
    /// 用于提取CLI参数的中低级例程。
    matches: ArgMatches,
    /// 我们要执行的命令。
    command: Command,
    /// 要使用的线程数。这部分是基于可用的线程，部分是基于请求的线程数，部分是基于我们正在运行的命令。
    threads: usize,
    ///从模式构建的匹配器。
    ///
    /// 重要的是只构建一次，因为构建它需要通过regex编译和各种类型的分析。也就是说，如果您需要很多这些(例如，每个线程一个)，最好构建一次，然后克隆它。
    matcher: PatternMatcher,
    /// 在命令行提供的路径。这保证是非空的。（如果没有提供路径，则创建一个默认路径。）
    paths: Vec<PathBuf>,
    /// 当且仅当需要使用单个默认路径填充 `paths` 时，返回 `true`。
    using_default_path: bool,
}

impl Args {
    /// 解析此进程的命令行参数。
    ///
    /// 如果发生 CLI 使用错误，那么退出进程并打印使用说明或错误消息。
    /// 同样，如果用户请求 ripgrep 的版本，那么打印版本信息并退出。
    ///
    /// 此外，初始化全局日志记录器。
    pub fn parse() -> Result<Args> {
        // 我们解析 CLI 上给出的参数。这不包括来自配置的参数。
        // 我们使用 CLI 参数作为初始配置，同时尝试解析配置文件。
        // 如果配置文件存在并包含参数，则重新解析 argv，否则我们只是使用这里的匹配项。
        let early_matches: ArgMatches =
            ArgMatches::new(clap_matches(env::args_os())?);
        set_messages(!early_matches.is_present("no-messages"));
        set_ignore_messages(!early_matches.is_present("no-ignore-messages"));

        if let Err(err) = Logger::init() {
            return Err(format!("无法初始化日志记录器：{}", err).into());
        }
        if early_matches.is_present("trace") {
            log::set_max_level(log::LevelFilter::Trace);
        } else if early_matches.is_present("debug") {
            log::set_max_level(log::LevelFilter::Debug);
        } else {
            log::set_max_level(log::LevelFilter::Warn);
        }

        // 运行 clap 并返回匹配的结果，如果存在配置文件则使用配置文件
        let matches: ArgMatches = early_matches.reconfigure()?;
        // 如果我们从配置文件中引入了额外的参数，日志级别可能已经发生变化，因此重新检查并根据需要设置日志级别。
        if matches.is_present("trace") {
            log::set_max_level(log::LevelFilter::Trace);
        } else if matches.is_present("debug") {
            log::set_max_level(log::LevelFilter::Debug);
        } else {
            log::set_max_level(log::LevelFilter::Warn);
        }
        set_messages(!matches.is_present("no-messages"));
        set_ignore_messages(!matches.is_present("no-ignore-messages"));
        matches.to_args()
    }

    /// 返回直接访问命令行参数。
    fn matches(&self) -> &ArgMatches {
        &self.0.matches
    }

    /// 返回来自模式的匹配器构建器。
    fn matcher(&self) -> &PatternMatcher {
        &self.0.matcher
    }

    /// 返回在命令行参数中找到的路径。保证非空。如果没有提供显式参数，则会自动提供单个默认路径。
    fn paths(&self) -> &[PathBuf] {
        &self.0.paths
    }

    /// 当且仅当 `paths` 必须使用默认路径填充时返回 true，这仅在未将路径作为命令行参数给出时发生。
    pub fn using_default_path(&self) -> bool {
        self.0.using_default_path
    }

    /// 返回应用于格式化搜索结果输出的打印机。
    ///
    /// 返回的打印机将结果写入给定的写入器。
    fn printer<W: WriteColor>(&self, wtr: W) -> Result<Printer<W>> {
        match self.matches().output_kind() {
            OutputKind::Standard => {
                let separator_search = self.command() == Command::Search;
                self.matches()
                    .printer_standard(self.paths(), wtr, separator_search)
                    .map(Printer::Standard)
            }
            OutputKind::Summary => self
                .matches()
                .printer_summary(self.paths(), wtr)
                .map(Printer::Summary),
            OutputKind::JSON => {
                self.matches().printer_json(wtr).map(Printer::JSON)
            }
        }
    }
}

/// 从命令行参数构建 ripgrep 使用的数据结构的高级公共例程。
impl Args {
    /// 创建一个带有颜色支持的多线程打印缓冲区写入器。
    pub fn buffer_writer(&self) -> Result<BufferWriter> {
        let mut wtr = BufferWriter::stdout(self.matches().color_choice());
        wtr.separator(self.matches().file_separator()?);
        Ok(wtr)
    }

    /// 返回 ripgrep 应该运行的高级命令。
    pub fn command(&self) -> Command {
        self.0.command
    }

    /// 构建一个可用于仅打印文件路径的路径打印机，支持可选的颜色。
    ///
    /// 打印机将文件路径打印到给定的写入器。
    pub fn path_printer<W: WriteColor>(
        &self,
        wtr: W,
    ) -> Result<PathPrinter<W>> {
        let mut builder = PathPrinterBuilder::new();
        builder
            .color_specs(self.matches().color_specs()?)
            .separator(self.matches().path_separator()?)
            .terminator(self.matches().path_terminator().unwrap_or(b'\n'));
        Ok(builder.build(wtr))
    }

    /// 当且仅当 ripgrep 应该"安静"时返回 true。
    pub fn quiet(&self) -> bool {
        self.matches().is_present("quiet")
    }

    /// 当且仅当搜索在找到第一个匹配后应该退出时返回 true。
    pub fn quit_after_match(&self) -> Result<bool> {
        Ok(self.matches().is_present("quiet") && self.stats()?.is_none())
    }

    /// 构建一个用于执行搜索的工作者。
    ///
    /// 搜索结果将写入给定的写入器。
    pub fn search_worker<W: WriteColor>(
        &self,
        wtr: W,
    ) -> Result<SearchWorker<W>> {
        let matches: &ArgMatches = self.matches();
        let matcher: PatternMatcher = self.matcher().clone();
        let printer: Printer<W> = self.printer(wtr)?;
        let searcher: Searcher = matches.searcher(self.paths())?;
        let mut builder: SearchWorkerBuilder = SearchWorkerBuilder::new();
        builder
            .json_stats(matches.is_present("json"))
            .preprocessor(matches.preprocessor())?
            .preprocessor_globs(matches.preprocessor_globs()?)
            .search_zip(matches.is_present("search-zip"))
            .binary_detection_implicit(matches.binary_detection_implicit())
            .binary_detection_explicit(matches.binary_detection_explicit());
        Ok(builder.build(matcher, searcher, printer))
    }

    /// 当且仅当已请求统计信息时，返回一个零值以跟踪统计信息。
    ///
    /// 当返回一个 `Stats` 值时，可以保证搜索工作者也会被配置为跟踪统计信息。
    pub fn stats(&self) -> Result<Option<Stats>> {
        Ok(if self.command().is_search() && self.matches().stats() {
            Some(Stats::new())
        } else {
            None
        })
    }

    /// 返回一个用于构建主题的构建器。一个主题表示要搜索的单个单位。通常，这对应于文件或流，如 stdin。
    pub fn subject_builder(&self) -> SubjectBuilder {
        let mut builder = SubjectBuilder::new();
        builder.strip_dot_prefix(self.using_default_path());
        builder
    }

    /// 使用启用基于命令行配置的颜色支持的写入器执行给定函数。
    pub fn stdout(&self) -> cli::StandardStream {
        let color = self.matches().color_choice();
        if self.matches().is_present("line-buffered") {
            cli::stdout_buffered_line(color)
        } else if self.matches().is_present("block-buffered") {
            cli::stdout_buffered_block(color)
        } else {
            cli::stdout(color)
        }
    }

    /// 返回编译到 ripgrep 中的类型定义。
    ///
    /// 如果读取和解析类型定义时出现问题，将返回错误。
    pub fn type_defs(&self) -> Result<Vec<FileTypeDef>> {
        Ok(self.matches().types()?.definitions().to_vec())
    }

    /// 返回一个从不使用额外线程的 walker 执行器。
    pub fn walker(&self) -> Result<Walk> {
        Ok(self
            .matches()
            .walker_builder(self.paths(), self.0.threads)?
            .build())
    }

    /// 当且仅当需要 `stat` 相关的排序时返回 true。
    pub fn needs_stat_sort(&self) -> bool {
        return self.matches().sort_by().map_or(
            false,
            |sort_by| match sort_by.kind {
                SortByKind::LastModified
                | SortByKind::Created
                | SortByKind::LastAccessed => sort_by.check().is_ok(),
                _ => false,
            },
        );
    }

    /// 如果指定了排序器，则对主题进行排序，但仅在排序需要 `stat` 调用时进行。
    ///
    /// 此函数假设已知需要 `stat` 相关的排序，并且不再次检查该条件。
    ///
    /// 重要的是满足该前提条件，因为此函数会消耗主题迭代器，因此是一个阻塞函数。
    pub fn sort_by_stat<I>(&self, subjects: I) -> Vec<Subject>
    where
        I: Iterator<Item = Subject>,
    {
        let sorter = match self.matches().sort_by() {
            Ok(v) => v,
            Err(_) => return subjects.collect(),
        };
        use SortByKind::*;
        let mut keyed = match sorter.kind {
            LastModified => load_timestamps(subjects, |m| m.modified()),
            LastAccessed => load_timestamps(subjects, |m| m.accessed()),
            Created => load_timestamps(subjects, |m| m.created()),
            _ => return subjects.collect(),
        };
        keyed.sort_by(|a, b| sort_by_option(&a.0, &b.0, sorter.reverse));
        keyed.into_iter().map(|v| v.1).collect()
    }

    /// 返回一个可能使用额外线程的并行 walker。
    pub fn walker_parallel(&self) -> Result<WalkParallel> {
        Ok(self
            .matches()
            .walker_builder(self.paths(), self.0.threads)?
            .build_parallel())
    }
}

/// ' ArgMatches '封装了' clap::ArgMatches '，并为解析后的参数提供了语义含义。
#[derive(Clone, Debug)]
struct ArgMatches(clap::ArgMatches<'static>);

/// The output format. Generally, this corresponds to the printer that ripgrep
/// uses to show search results.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputKind {
    /// Classic grep-like or ack-like format.
    Standard,
    /// Show matching files and possibly the number of matches in each file.
    Summary,
    /// Emit match information in the JSON Lines format.
    JSON,
}

/// The sort criteria, if present.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SortBy {
    /// Whether to reverse the sort criteria (i.e., descending order).
    reverse: bool,
    /// The actual sorting criteria.
    kind: SortByKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SortByKind {
    /// No sorting at all.
    None,
    /// Sort by path.
    Path,
    /// Sort by last modified time.
    LastModified,
    /// Sort by last accessed time.
    LastAccessed,
    /// Sort by creation time.
    Created,
}

impl SortBy {
    fn asc(kind: SortByKind) -> SortBy {
        SortBy { reverse: false, kind }
    }

    fn desc(kind: SortByKind) -> SortBy {
        SortBy { reverse: true, kind }
    }

    fn none() -> SortBy {
        SortBy::asc(SortByKind::None)
    }

    /// Try to check that the sorting criteria selected is actually supported.
    /// If it isn't, then an error is returned.
    fn check(&self) -> Result<()> {
        match self.kind {
            SortByKind::None | SortByKind::Path => {}
            SortByKind::LastModified => {
                env::current_exe()?.metadata()?.modified()?;
            }
            SortByKind::LastAccessed => {
                env::current_exe()?.metadata()?.accessed()?;
            }
            SortByKind::Created => {
                env::current_exe()?.metadata()?.created()?;
            }
        }
        Ok(())
    }

    /// Load sorters only if they are applicable at the walk stage.
    ///
    /// In particular, sorts that involve `stat` calls are not loaded because
    /// the walk inherently assumes that parent directories are aware of all its
    /// decendent properties, but `stat` does not work that way.
    fn configure_builder_sort(self, builder: &mut WalkBuilder) {
        use SortByKind::*;
        match self.kind {
            Path if self.reverse => {
                builder.sort_by_file_name(|a, b| a.cmp(b).reverse());
            }
            Path => {
                builder.sort_by_file_name(|a, b| a.cmp(b));
            }
            // these use `stat` calls and will be sorted in Args::sort_by_stat()
            LastModified | LastAccessed | Created | None => {}
        };
    }
}

impl SortByKind {
    fn new(kind: &str) -> SortByKind {
        match kind {
            "none" => SortByKind::None,
            "path" => SortByKind::Path,
            "modified" => SortByKind::LastModified,
            "accessed" => SortByKind::LastAccessed,
            "created" => SortByKind::Created,
            _ => SortByKind::None,
        }
    }
}

/// Encoding mode the searcher will use.
#[derive(Clone, Debug)]
enum EncodingMode {
    /// Use an explicit encoding forcefully, but let BOM sniffing override it.
    Some(Encoding),
    /// Use only BOM sniffing to auto-detect an encoding.
    Auto,
    /// Use no explicit encoding and disable all BOM sniffing. This will
    /// always result in searching the raw bytes, regardless of their
    /// true encoding.
    Disabled,
}

impl ArgMatches {
    /// 从clap的解析结果创建ArgMatches。
    fn new(clap_matches: clap::ArgMatches<'static>) -> ArgMatches {
        ArgMatches(clap_matches)
    }

    /// 运行 clap 并返回匹配的结果，如果存在配置文件则使用配置文件。
    /// 如果 clap 发现用户提供的参数存在问题（或者给出了 --help 或 --version），则会打印错误、用法或版本信息，
    /// 然后进程将退出。
    ///
    /// 如果没有来自环境的附加参数（例如，配置文件），则给定的匹配将原样返回。
    fn reconfigure(self) -> Result<ArgMatches> {
        // 如果最终用户选择不使用配置文件，则尊重其选择。
        if self.is_present("no-config") {
            log::debug!("因为存在 --no-config，不读取配置文件");
            return Ok(self);
        }
        // 如果用户希望 ripgrep 使用配置文件，则首先从中解析参数。
        let mut args: Vec<OsString> = config::args();
        if args.is_empty() {
            return Ok(self);
        }
        let mut cliargs: env::ArgsOs = env::args_os();
        if let Some(bin) = cliargs.next() {
            args.insert(0, bin);
        }
        args.extend(cliargs);
        log::debug!("最终的 argv: {:?}", args);
        Ok(ArgMatches(clap_matches(args)?))
    }

    /// 将解析 CLI 参数的结果转换为 ripgrep 的高级配置结构。
    fn to_args(self) -> Result<Args> {
        // 因为这些可能会很大，所以我们只计算一次。
        let patterns: Vec<String> = self.patterns()?;
        let matcher: PatternMatcher = self.matcher(&patterns)?;
        let mut paths: Vec<PathBuf> = self.paths();
        let using_default_path: bool = if paths.is_empty() {
            paths.push(self.path_default());
            true
        } else {
            false
        };
        // 现在确定我们将使用的线程数和要运行的命令。
        let is_one_search: bool = self.is_one_search(&paths);
        let threads: usize = if is_one_search { 1 } else { self.threads()? };
        if threads == 1 {
            log::debug!("运行在单线程模式");
        } else {
            log::debug!("运行使用 {threads} 个线程以实现并行");
        }
        let command: Command = if self.is_present("pcre2-version") {
            Command::PCRE2Version
        } else if self.is_present("type-list") {
            Command::Types
        } else if self.is_present("files") {
            if threads == 1 {
                Command::Files
            } else {
                Command::FilesParallel
            }
        } else if self.can_never_match(&patterns) {
            Command::SearchNever
        } else if threads == 1 {
            Command::Search
        } else {
            Command::SearchParallel
        };
        Ok(Args(Arc::new(ArgsImp {
            matches: self,
            command,
            threads,
            matcher,
            paths,
            using_default_path,
        })))
    }
}

/// High level routines for converting command line arguments into various
/// data structures used by ripgrep.
///
/// Methods are sorted alphabetically.
impl ArgMatches {
    /// Return the matcher that should be used for searching.
    ///
    /// If there was a problem building the matcher (e.g., a syntax error),
    /// then this returns an error.
    fn matcher(&self, patterns: &[String]) -> Result<PatternMatcher> {
        if self.is_present("pcre2") {
            self.matcher_engine("pcre2", patterns)
        } else if self.is_present("auto-hybrid-regex") {
            self.matcher_engine("auto", patterns)
        } else {
            let engine = self.value_of_lossy("engine").unwrap();
            self.matcher_engine(&engine, patterns)
        }
    }

    /// Return the matcher that should be used for searching using engine
    /// as the engine for the patterns.
    ///
    /// If there was a problem building the matcher (e.g., a syntax error),
    /// then this returns an error.
    fn matcher_engine(
        &self,
        engine: &str,
        patterns: &[String],
    ) -> Result<PatternMatcher> {
        match engine {
            "default" => {
                let matcher = match self.matcher_rust(patterns) {
                    Ok(matcher) => matcher,
                    Err(err) => {
                        return Err(From::from(suggest(err.to_string())));
                    }
                };
                Ok(PatternMatcher::RustRegex(matcher))
            }
            #[cfg(feature = "pcre2")]
            "pcre2" => {
                let matcher = self.matcher_pcre2(patterns)?;
                Ok(PatternMatcher::PCRE2(matcher))
            }
            #[cfg(not(feature = "pcre2"))]
            "pcre2" => Err(From::from(
                "PCRE2 is not available in this build of ripgrep",
            )),
            "auto" => {
                let rust_err = match self.matcher_rust(patterns) {
                    Ok(matcher) => {
                        return Ok(PatternMatcher::RustRegex(matcher));
                    }
                    Err(err) => err,
                };
                log::debug!(
                    "error building Rust regex in hybrid mode:\n{}",
                    rust_err,
                );

                let pcre_err = match self.matcher_engine("pcre2", patterns) {
                    Ok(matcher) => return Ok(matcher),
                    Err(err) => err,
                };
                Err(From::from(format!(
                    "regex could not be compiled with either the default \
                     regex engine or with PCRE2.\n\n\
                     default regex engine error:\n{}\n{}\n{}\n\n\
                     PCRE2 regex engine error:\n{}",
                    "~".repeat(79),
                    rust_err,
                    "~".repeat(79),
                    pcre_err,
                )))
            }
            _ => Err(From::from(format!(
                "unrecognized regex engine '{}'",
                engine
            ))),
        }
    }

    /// Build a matcher using Rust's regex engine.
    ///
    /// If there was a problem building the matcher (such as a regex syntax
    /// error), then an error is returned.
    fn matcher_rust(&self, patterns: &[String]) -> Result<RustRegexMatcher> {
        let mut builder = RustRegexMatcherBuilder::new();
        builder
            .case_smart(self.case_smart())
            .case_insensitive(self.case_insensitive())
            .multi_line(true)
            .unicode(self.unicode())
            .octal(false)
            .fixed_strings(self.is_present("fixed-strings"))
            .whole_line(self.is_present("line-regexp"))
            .word(self.is_present("word-regexp"));
        if self.is_present("multiline") {
            builder.dot_matches_new_line(self.is_present("multiline-dotall"));
            if self.is_present("crlf") {
                builder.crlf(true).line_terminator(None);
            }
        } else {
            builder.line_terminator(Some(b'\n')).dot_matches_new_line(false);
            if self.is_present("crlf") {
                builder.crlf(true);
            }
            // We don't need to set this in multiline mode since mulitline
            // matchers don't use optimizations related to line terminators.
            // Moreover, a mulitline regex used with --null-data should
            // be allowed to match NUL bytes explicitly, which this would
            // otherwise forbid.
            if self.is_present("null-data") {
                builder.line_terminator(Some(b'\x00'));
            }
        }
        if let Some(limit) = self.regex_size_limit()? {
            builder.size_limit(limit);
        }
        if let Some(limit) = self.dfa_size_limit()? {
            builder.dfa_size_limit(limit);
        }
        match builder.build_many(patterns) {
            Ok(m) => Ok(m),
            Err(err) => Err(From::from(suggest_multiline(err.to_string()))),
        }
    }

    /// Build a matcher using PCRE2.
    ///
    /// If there was a problem building the matcher (such as a regex syntax
    /// error), then an error is returned.
    #[cfg(feature = "pcre2")]
    fn matcher_pcre2(&self, patterns: &[String]) -> Result<PCRE2RegexMatcher> {
        let mut builder = PCRE2RegexMatcherBuilder::new();
        builder
            .case_smart(self.case_smart())
            .caseless(self.case_insensitive())
            .multi_line(true)
            .fixed_strings(self.is_present("fixed-strings"))
            .whole_line(self.is_present("line-regexp"))
            .word(self.is_present("word-regexp"));
        // For whatever reason, the JIT craps out during regex compilation with
        // a "no more memory" error on 32 bit systems. So don't use it there.
        if cfg!(target_pointer_width = "64") {
            builder
                .jit_if_available(true)
                // The PCRE2 docs say that 32KB is the default, and that 1MB
                // should be big enough for anything. But let's crank it to
                // 10MB.
                .max_jit_stack_size(Some(10 * (1 << 20)));
        }
        if self.unicode() {
            builder.utf(true).ucp(true);
        }
        if self.is_present("multiline") {
            builder.dotall(self.is_present("multiline-dotall"));
        }
        if self.is_present("crlf") {
            builder.crlf(true);
        }
        Ok(builder.build_many(patterns)?)
    }

    /// Build a JSON printer that writes results to the given writer.
    fn printer_json<W: io::Write>(&self, wtr: W) -> Result<JSON<W>> {
        let mut builder = JSONBuilder::new();
        builder
            .pretty(false)
            .max_matches(self.max_count()?)
            .always_begin_end(false);
        Ok(builder.build(wtr))
    }

    /// Build a Standard printer that writes results to the given writer.
    ///
    /// The given paths are used to configure aspects of the printer.
    ///
    /// If `separator_search` is true, then the returned printer will assume
    /// the responsibility of printing a separator between each set of
    /// search results, when appropriate (e.g., when contexts are enabled).
    /// When it's set to false, the caller is responsible for handling
    /// separators.
    ///
    /// In practice, we want the printer to handle it in the single threaded
    /// case but not in the multi-threaded case.
    fn printer_standard<W: WriteColor>(
        &self,
        paths: &[PathBuf],
        wtr: W,
        separator_search: bool,
    ) -> Result<Standard<W>> {
        let mut builder = StandardBuilder::new();
        builder
            .color_specs(self.color_specs()?)
            .stats(self.stats())
            .heading(self.heading())
            .path(self.with_filename(paths))
            .only_matching(self.is_present("only-matching"))
            .per_match(self.is_present("vimgrep"))
            .per_match_one_line(true)
            .replacement(self.replacement())
            .max_columns(self.max_columns()?)
            .max_columns_preview(self.max_columns_preview())
            .max_matches(self.max_count()?)
            .column(self.column())
            .byte_offset(self.is_present("byte-offset"))
            .trim_ascii(self.is_present("trim"))
            .separator_search(None)
            .separator_context(self.context_separator())
            .separator_field_match(self.field_match_separator())
            .separator_field_context(self.field_context_separator())
            .separator_path(self.path_separator()?)
            .path_terminator(self.path_terminator());
        if separator_search {
            builder.separator_search(self.file_separator()?);
        }
        Ok(builder.build(wtr))
    }

    /// Build a Summary printer that writes results to the given writer.
    ///
    /// The given paths are used to configure aspects of the printer.
    ///
    /// This panics if the output format is not `OutputKind::Summary`.
    fn printer_summary<W: WriteColor>(
        &self,
        paths: &[PathBuf],
        wtr: W,
    ) -> Result<Summary<W>> {
        let mut builder = SummaryBuilder::new();
        builder
            .kind(self.summary_kind().expect("summary format"))
            .color_specs(self.color_specs()?)
            .stats(self.stats())
            .path(self.with_filename(paths))
            .max_matches(self.max_count()?)
            .exclude_zero(!self.is_present("include-zero"))
            .separator_field(b":".to_vec())
            .separator_path(self.path_separator()?)
            .path_terminator(self.path_terminator());
        Ok(builder.build(wtr))
    }

    /// Build a searcher from the command line parameters.
    fn searcher(&self, paths: &[PathBuf]) -> Result<Searcher> {
        let (ctx_before, ctx_after) = self.contexts()?;
        let line_term = if self.is_present("crlf") {
            LineTerminator::crlf()
        } else if self.is_present("null-data") {
            LineTerminator::byte(b'\x00')
        } else {
            LineTerminator::byte(b'\n')
        };
        let mut builder = SearcherBuilder::new();
        builder
            .line_terminator(line_term)
            .invert_match(self.is_present("invert-match"))
            .line_number(self.line_number(paths))
            .multi_line(self.is_present("multiline"))
            .before_context(ctx_before)
            .after_context(ctx_after)
            .passthru(self.is_present("passthru"))
            .memory_map(self.mmap_choice(paths))
            .stop_on_nonmatch(self.is_present("stop-on-nonmatch"));
        match self.encoding()? {
            EncodingMode::Some(enc) => {
                builder.encoding(Some(enc));
            }
            EncodingMode::Auto => {} // default for the searcher
            EncodingMode::Disabled => {
                builder.bom_sniffing(false);
            }
        }
        Ok(builder.build())
    }

    /// 返回一个构建器，用于在遵循忽略规则的同时递归地遍历目录。
    ///
    /// 如果在构建器构造所需的 CLI 参数解析过程中出现问题，那么将返回错误。
    fn walker_builder(
        &self,
        paths: &[PathBuf],
        threads: usize,
    ) -> Result<WalkBuilder> {
        let mut builder: WalkBuilder = WalkBuilder::new(&paths[0]);
        for path in &paths[1..] {
            builder.add(path);
        }
        if !self.no_ignore_files() {
            for path in self.ignore_paths() {
                if let Some(err) = builder.add_ignore(path) {
                    ignore_message!("{}", err);
                }
            }
        }
        builder
            .max_depth(self.usize_of("max-depth")?)
            .follow_links(self.is_present("follow"))
            .max_filesize(self.max_file_size()?)
            .threads(threads)
            .same_file_system(self.is_present("one-file-system"))
            .skip_stdout(!self.is_present("files"))
            .overrides(self.overrides()?)
            .types(self.types()?)
            .hidden(!self.hidden())
            .parents(!self.no_ignore_parent())
            .ignore(!self.no_ignore_dot())
            .git_global(!self.no_ignore_vcs() && !self.no_ignore_global())
            .git_ignore(!self.no_ignore_vcs())
            .git_exclude(!self.no_ignore_vcs() && !self.no_ignore_exclude())
            .require_git(!self.is_present("no-require-git"))
            .ignore_case_insensitive(self.ignore_file_case_insensitive());
        if !self.no_ignore() && !self.no_ignore_dot() {
            builder.add_custom_ignore_filename(".rgignore");
        }
        self.sort_by()?.configure_builder_sort(&mut builder);
        Ok(builder)
    }
}

/// Mid level routines for converting command line arguments into various types
/// of data structures.
///
/// Methods are sorted alphabetically.
impl ArgMatches {
    /// Returns the form of binary detection to perform on files that are
    /// implicitly searched via recursive directory traversal.
    fn binary_detection_implicit(&self) -> BinaryDetection {
        let none = self.is_present("text") || self.is_present("null-data");
        let convert =
            self.is_present("binary") || self.unrestricted_count() >= 3;
        if none {
            BinaryDetection::none()
        } else if convert {
            BinaryDetection::convert(b'\x00')
        } else {
            BinaryDetection::quit(b'\x00')
        }
    }

    /// Returns the form of binary detection to perform on files that are
    /// explicitly searched via the user invoking ripgrep on a particular
    /// file or files or stdin.
    ///
    /// In general, this should never be BinaryDetection::quit, since that acts
    /// as a filter (but quitting immediately once a NUL byte is seen), and we
    /// should never filter out files that the user wants to explicitly search.
    fn binary_detection_explicit(&self) -> BinaryDetection {
        let none = self.is_present("text") || self.is_present("null-data");
        if none {
            BinaryDetection::none()
        } else {
            BinaryDetection::convert(b'\x00')
        }
    }

    /// Returns true if the command line configuration implies that a match
    /// can never be shown.
    fn can_never_match(&self, patterns: &[String]) -> bool {
        patterns.is_empty() || self.max_count().ok() == Some(Some(0))
    }

    /// Returns true if and only if case should be ignore.
    ///
    /// If --case-sensitive is present, then case is never ignored, even if
    /// --ignore-case is present.
    fn case_insensitive(&self) -> bool {
        self.is_present("ignore-case") && !self.is_present("case-sensitive")
    }

    /// Returns true if and only if smart case has been enabled.
    ///
    /// If either --ignore-case of --case-sensitive are present, then smart
    /// case is disabled.
    fn case_smart(&self) -> bool {
        self.is_present("smart-case")
            && !self.is_present("ignore-case")
            && !self.is_present("case-sensitive")
    }

    /// Returns the user's color choice based on command line parameters and
    /// environment.
    fn color_choice(&self) -> ColorChoice {
        let preference = match self.value_of_lossy("color") {
            None => "auto".to_string(),
            Some(v) => v,
        };
        if preference == "always" {
            ColorChoice::Always
        } else if preference == "ansi" {
            ColorChoice::AlwaysAnsi
        } else if preference == "auto" {
            if cli::is_tty_stdout() || self.is_present("pretty") {
                ColorChoice::Auto
            } else {
                ColorChoice::Never
            }
        } else {
            ColorChoice::Never
        }
    }

    /// Returns the color specifications given by the user on the CLI.
    ///
    /// If the was a problem parsing any of the provided specs, then an error
    /// is returned.
    fn color_specs(&self) -> Result<ColorSpecs> {
        // Start with a default set of color specs.
        let mut specs = default_color_specs();
        for spec_str in self.values_of_lossy_vec("colors") {
            specs.push(spec_str.parse()?);
        }
        Ok(ColorSpecs::new(&specs))
    }

    /// Returns true if and only if column numbers should be shown.
    fn column(&self) -> bool {
        if self.is_present("no-column") {
            return false;
        }
        self.is_present("column") || self.is_present("vimgrep")
    }

    /// Returns the before and after contexts from the command line.
    ///
    /// If a context setting was absent, then `0` is returned.
    ///
    /// If there was a problem parsing the values from the user as an integer,
    /// then an error is returned.
    fn contexts(&self) -> Result<(usize, usize)> {
        let both = self.usize_of("context")?.unwrap_or(0);
        let after = self.usize_of("after-context")?.unwrap_or(both);
        let before = self.usize_of("before-context")?.unwrap_or(both);
        Ok((before, after))
    }

    /// Returns the unescaped context separator in UTF-8 bytes.
    ///
    /// If one was not provided, the default `--` is returned.
    /// If --no-context-separator is passed, None is returned.
    fn context_separator(&self) -> Option<Vec<u8>> {
        let nosep = self.is_present("no-context-separator");
        let sep = self.value_of_os("context-separator");
        match (nosep, sep) {
            (true, _) => None,
            (false, None) => Some(b"--".to_vec()),
            (false, Some(sep)) => Some(cli::unescape_os(&sep)),
        }
    }

    /// Returns whether the -c/--count or the --count-matches flags were
    /// passed from the command line.
    ///
    /// If --count-matches and --invert-match were passed in, behave
    /// as if --count and --invert-match were passed in (i.e. rg will
    /// count inverted matches as per existing behavior).
    fn counts(&self) -> (bool, bool) {
        let count = self.is_present("count");
        let count_matches = self.is_present("count-matches");
        let invert_matches = self.is_present("invert-match");
        let only_matching = self.is_present("only-matching");
        if count_matches && invert_matches {
            // Treat `-v --count-matches` as `-v -c`.
            (true, false)
        } else if count && only_matching {
            // Treat `-c --only-matching` as `--count-matches`.
            (false, true)
        } else {
            (count, count_matches)
        }
    }

    /// Parse the dfa-size-limit argument option into a byte count.
    fn dfa_size_limit(&self) -> Result<Option<usize>> {
        let r = self.parse_human_readable_size("dfa-size-limit")?;
        u64_to_usize("dfa-size-limit", r)
    }

    /// Returns the encoding mode to use.
    ///
    /// This only returns an encoding if one is explicitly specified. Otherwise
    /// if set to automatic, the Searcher will do BOM sniffing for UTF-16
    /// and transcode seamlessly. If disabled, no BOM sniffing nor transcoding
    /// will occur.
    fn encoding(&self) -> Result<EncodingMode> {
        if self.is_present("no-encoding") {
            return Ok(EncodingMode::Auto);
        }

        let label = match self.value_of_lossy("encoding") {
            None => return Ok(EncodingMode::Auto),
            Some(label) => label,
        };

        if label == "auto" {
            return Ok(EncodingMode::Auto);
        } else if label == "none" {
            return Ok(EncodingMode::Disabled);
        }

        Ok(EncodingMode::Some(Encoding::new(&label)?))
    }

    /// Return the file separator to use based on the CLI configuration.
    fn file_separator(&self) -> Result<Option<Vec<u8>>> {
        // File separators are only used for the standard grep-line format.
        if self.output_kind() != OutputKind::Standard {
            return Ok(None);
        }

        let (ctx_before, ctx_after) = self.contexts()?;
        Ok(if self.heading() {
            Some(b"".to_vec())
        } else if ctx_before > 0 || ctx_after > 0 {
            self.context_separator()
        } else {
            None
        })
    }

    /// Returns true if and only if matches should be grouped with file name
    /// headings.
    fn heading(&self) -> bool {
        if self.is_present("no-heading") || self.is_present("vimgrep") {
            false
        } else {
            cli::is_tty_stdout()
                || self.is_present("heading")
                || self.is_present("pretty")
        }
    }

    /// Returns true if and only if hidden files/directories should be
    /// searched.
    fn hidden(&self) -> bool {
        self.is_present("hidden") || self.unrestricted_count() >= 2
    }

    /// Returns true if ignore files should be processed case insensitively.
    fn ignore_file_case_insensitive(&self) -> bool {
        self.is_present("ignore-file-case-insensitive")
    }

    /// Return all of the ignore file paths given on the command line.
    fn ignore_paths(&self) -> Vec<PathBuf> {
        let paths = match self.values_of_os("ignore-file") {
            None => return vec![],
            Some(paths) => paths,
        };
        paths.map(|p| Path::new(p).to_path_buf()).collect()
    }

    /// Returns true if and only if ripgrep is invoked in a way where it knows
    /// it search exactly one thing.
    fn is_one_search(&self, paths: &[PathBuf]) -> bool {
        if paths.len() != 1 {
            return false;
        }
        self.is_only_stdin(paths) || paths[0].is_file()
    }

    /// Returns true if and only if we're only searching a single thing and
    /// that thing is stdin.
    fn is_only_stdin(&self, paths: &[PathBuf]) -> bool {
        paths == [Path::new("-")]
    }

    /// Returns true if and only if we should show line numbers.
    fn line_number(&self, paths: &[PathBuf]) -> bool {
        if self.output_kind() == OutputKind::Summary {
            return false;
        }
        if self.is_present("no-line-number") {
            return false;
        }
        if self.output_kind() == OutputKind::JSON {
            return true;
        }

        // A few things can imply counting line numbers. In particular, we
        // generally want to show line numbers by default when printing to a
        // tty for human consumption, except for one interesting case: when
        // we're only searching stdin. This makes pipelines work as expected.
        (cli::is_tty_stdout() && !self.is_only_stdin(paths))
            || self.is_present("line-number")
            || self.is_present("column")
            || self.is_present("pretty")
            || self.is_present("vimgrep")
    }

    /// The maximum number of columns allowed on each line.
    ///
    /// If `0` is provided, then this returns `None`.
    fn max_columns(&self) -> Result<Option<u64>> {
        Ok(self.usize_of_nonzero("max-columns")?.map(|n| n as u64))
    }

    /// Returns true if and only if a preview should be shown for lines that
    /// exceed the maximum column limit.
    fn max_columns_preview(&self) -> bool {
        self.is_present("max-columns-preview")
    }

    /// The maximum number of matches permitted.
    fn max_count(&self) -> Result<Option<u64>> {
        Ok(self.usize_of("max-count")?.map(|n| n as u64))
    }

    /// Parses the max-filesize argument option into a byte count.
    fn max_file_size(&self) -> Result<Option<u64>> {
        self.parse_human_readable_size("max-filesize")
    }

    /// Returns whether we should attempt to use memory maps or not.
    fn mmap_choice(&self, paths: &[PathBuf]) -> MmapChoice {
        // SAFETY: Memory maps are difficult to impossible to encapsulate
        // safely in a portable way that doesn't simultaneously negate some of
        // the benfits of using memory maps. For ripgrep's use, we never mutate
        // a memory map and generally never store the contents of memory map
        // in a data structure that depends on immutability. Generally
        // speaking, the worst thing that can happen is a SIGBUS (if the
        // underlying file is truncated while reading it), which will cause
        // ripgrep to abort. This reasoning should be treated as suspect.
        let maybe = unsafe { MmapChoice::auto() };
        let never = MmapChoice::never();
        if self.is_present("no-mmap") {
            never
        } else if self.is_present("mmap") {
            maybe
        } else if paths.len() <= 10 && paths.iter().all(|p| p.is_file()) {
            // If we're only searching a few paths and all of them are
            // files, then memory maps are probably faster.
            maybe
        } else {
            never
        }
    }

    /// Returns true if ignore files should be ignored.
    fn no_ignore(&self) -> bool {
        self.is_present("no-ignore") || self.unrestricted_count() >= 1
    }

    /// Returns true if .ignore files should be ignored.
    fn no_ignore_dot(&self) -> bool {
        self.is_present("no-ignore-dot") || self.no_ignore()
    }

    /// Returns true if local exclude (ignore) files should be ignored.
    fn no_ignore_exclude(&self) -> bool {
        self.is_present("no-ignore-exclude") || self.no_ignore()
    }

    /// Returns true if explicitly given ignore files should be ignored.
    fn no_ignore_files(&self) -> bool {
        // We don't look at no-ignore here because --no-ignore is explicitly
        // documented to not override --ignore-file. We could change this, but
        // it would be a fairly severe breaking change.
        self.is_present("no-ignore-files")
    }

    /// Returns true if global ignore files should be ignored.
    fn no_ignore_global(&self) -> bool {
        self.is_present("no-ignore-global") || self.no_ignore()
    }

    /// Returns true if parent ignore files should be ignored.
    fn no_ignore_parent(&self) -> bool {
        self.is_present("no-ignore-parent") || self.no_ignore()
    }

    /// Returns true if VCS ignore files should be ignored.
    fn no_ignore_vcs(&self) -> bool {
        self.is_present("no-ignore-vcs") || self.no_ignore()
    }

    /// Determine the type of output we should produce.
    fn output_kind(&self) -> OutputKind {
        if self.is_present("quiet") {
            // While we don't technically print results (or aggregate results)
            // in quiet mode, we still support the --stats flag, and those
            // stats are computed by the Summary printer for now.
            return OutputKind::Summary;
        } else if self.is_present("json") {
            return OutputKind::JSON;
        }

        let (count, count_matches) = self.counts();
        let summary = count
            || count_matches
            || self.is_present("files-with-matches")
            || self.is_present("files-without-match");
        if summary {
            OutputKind::Summary
        } else {
            OutputKind::Standard
        }
    }

    /// Builds the set of glob overrides from the command line flags.
    fn overrides(&self) -> Result<Override> {
        let globs = self.values_of_lossy_vec("glob");
        let iglobs = self.values_of_lossy_vec("iglob");
        if globs.is_empty() && iglobs.is_empty() {
            return Ok(Override::empty());
        }

        let mut builder = OverrideBuilder::new(current_dir()?);
        // Make all globs case insensitive with --glob-case-insensitive.
        if self.is_present("glob-case-insensitive") {
            builder.case_insensitive(true).unwrap();
        }
        for glob in globs {
            builder.add(&glob)?;
        }
        // This only enables case insensitivity for subsequent globs.
        builder.case_insensitive(true).unwrap();
        for glob in iglobs {
            builder.add(&glob)?;
        }
        Ok(builder.build()?)
    }

    /// Return all file paths that ripgrep should search.
    ///
    /// If no paths were given, then this returns an empty list.
    fn paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = match self.values_of_os("path") {
            None => vec![],
            Some(paths) => paths.map(|p| Path::new(p).to_path_buf()).collect(),
        };
        // If --file, --files or --regexp is given, then the first path is
        // always in `pattern`.
        if self.is_present("file")
            || self.is_present("files")
            || self.is_present("regexp")
        {
            if let Some(path) = self.value_of_os("pattern") {
                paths.insert(0, Path::new(path).to_path_buf());
            }
        }
        paths
    }

    /// Return the default path that ripgrep should search. This should only
    /// be used when ripgrep is not otherwise given at least one file path
    /// as a positional argument.
    fn path_default(&self) -> PathBuf {
        let file_is_stdin = self
            .values_of_os("file")
            .map_or(false, |mut files| files.any(|f| f == "-"));
        let search_cwd = !cli::is_readable_stdin()
            || (self.is_present("file") && file_is_stdin)
            || self.is_present("files")
            || self.is_present("type-list")
            || self.is_present("pcre2-version");
        if search_cwd {
            Path::new("./").to_path_buf()
        } else {
            Path::new("-").to_path_buf()
        }
    }

    /// Returns the unescaped path separator as a single byte, if one exists.
    ///
    /// If the provided path separator is more than a single byte, then an
    /// error is returned.
    fn path_separator(&self) -> Result<Option<u8>> {
        let sep = match self.value_of_os("path-separator") {
            None => return Ok(None),
            Some(sep) => cli::unescape_os(&sep),
        };
        if sep.is_empty() {
            Ok(None)
        } else if sep.len() > 1 {
            Err(From::from(format!(
                "A path separator must be exactly one byte, but \
                 the given separator is {} bytes: {}\n\
                 In some shells on Windows '/' is automatically \
                 expanded. Use '//' instead.",
                sep.len(),
                cli::escape(&sep),
            )))
        } else {
            Ok(Some(sep[0]))
        }
    }

    /// Returns the byte that should be used to terminate paths.
    ///
    /// Typically, this is only set to `\x00` when the --null flag is provided,
    /// and `None` otherwise.
    fn path_terminator(&self) -> Option<u8> {
        if self.is_present("null") {
            Some(b'\x00')
        } else {
            None
        }
    }

    /// Returns the unescaped field context separator. If one wasn't specified,
    /// then '-' is used as the default.
    fn field_context_separator(&self) -> Vec<u8> {
        match self.value_of_os("field-context-separator") {
            None => b"-".to_vec(),
            Some(sep) => cli::unescape_os(&sep),
        }
    }

    /// Returns the unescaped field match separator. If one wasn't specified,
    /// then ':' is used as the default.
    fn field_match_separator(&self) -> Vec<u8> {
        match self.value_of_os("field-match-separator") {
            None => b":".to_vec(),
            Some(sep) => cli::unescape_os(&sep),
        }
    }

    /// Get a sequence of all available patterns from the command line.
    /// This includes reading the -e/--regexp and -f/--file flags.
    ///
    /// If any pattern is invalid UTF-8, then an error is returned.
    fn patterns(&self) -> Result<Vec<String>> {
        if self.is_present("files") || self.is_present("type-list") {
            return Ok(vec![]);
        }
        let mut pats = vec![];
        match self.values_of_os("regexp") {
            None => {
                if self.values_of_os("file").is_none() {
                    if let Some(os_pat) = self.value_of_os("pattern") {
                        pats.push(self.pattern_from_os_str(os_pat)?);
                    }
                }
            }
            Some(os_pats) => {
                for os_pat in os_pats {
                    pats.push(self.pattern_from_os_str(os_pat)?);
                }
            }
        }
        if let Some(paths) = self.values_of_os("file") {
            for path in paths {
                if path == "-" {
                    pats.extend(
                        cli::patterns_from_stdin()?
                            .into_iter()
                            .map(|p| self.pattern_from_string(p)),
                    );
                } else {
                    pats.extend(
                        cli::patterns_from_path(path)?
                            .into_iter()
                            .map(|p| self.pattern_from_string(p)),
                    );
                }
            }
        }
        Ok(pats)
    }

    /// Converts an OsStr pattern to a String pattern. The pattern is escaped
    /// if -F/--fixed-strings is set.
    ///
    /// If the pattern is not valid UTF-8, then an error is returned.
    fn pattern_from_os_str(&self, pat: &OsStr) -> Result<String> {
        let s = cli::pattern_from_os(pat)?;
        Ok(self.pattern_from_str(s))
    }

    /// Converts a &str pattern to a String pattern. The pattern is escaped
    /// if -F/--fixed-strings is set.
    fn pattern_from_str(&self, pat: &str) -> String {
        self.pattern_from_string(pat.to_string())
    }

    /// Applies additional processing on the given pattern if necessary
    /// (such as escaping meta characters or turning it into a line regex).
    fn pattern_from_string(&self, pat: String) -> String {
        if pat.is_empty() {
            // This would normally just be an empty string, which works on its
            // own, but if the patterns are joined in a set of alternations,
            // then you wind up with `foo|`, which is currently invalid in
            // Rust's regex engine.
            "(?:)".to_string()
        } else {
            pat
        }
    }

    /// Returns the preprocessor command if one was specified.
    fn preprocessor(&self) -> Option<PathBuf> {
        let path = match self.value_of_os("pre") {
            None => return None,
            Some(path) => path,
        };
        if path.is_empty() {
            return None;
        }
        Some(Path::new(path).to_path_buf())
    }

    /// Builds the set of globs for filtering files to apply to the --pre
    /// flag. If no --pre-globs are available, then this always returns an
    /// empty set of globs.
    fn preprocessor_globs(&self) -> Result<Override> {
        let globs = self.values_of_lossy_vec("pre-glob");
        if globs.is_empty() {
            return Ok(Override::empty());
        }
        let mut builder = OverrideBuilder::new(current_dir()?);
        for glob in globs {
            builder.add(&glob)?;
        }
        Ok(builder.build()?)
    }

    /// Parse the regex-size-limit argument option into a byte count.
    fn regex_size_limit(&self) -> Result<Option<usize>> {
        let r = self.parse_human_readable_size("regex-size-limit")?;
        u64_to_usize("regex-size-limit", r)
    }

    /// Returns the replacement string as UTF-8 bytes if it exists.
    fn replacement(&self) -> Option<Vec<u8>> {
        self.value_of_lossy("replace").map(|s| s.into_bytes())
    }

    /// Returns the sorting criteria based on command line parameters.
    fn sort_by(&self) -> Result<SortBy> {
        // For backcompat, continue supporting deprecated --sort-files flag.
        if self.is_present("sort-files") {
            return Ok(SortBy::asc(SortByKind::Path));
        }
        let sortby = match self.value_of_lossy("sort") {
            None => match self.value_of_lossy("sortr") {
                None => return Ok(SortBy::none()),
                Some(choice) => SortBy::desc(SortByKind::new(&choice)),
            },
            Some(choice) => SortBy::asc(SortByKind::new(&choice)),
        };
        Ok(sortby)
    }

    /// Returns true if and only if aggregate statistics for a search should
    /// be tracked.
    ///
    /// Generally, this is only enabled when explicitly requested by in the
    /// command line arguments via the --stats flag, but this can also be
    /// enabled implicitly via the output format, e.g., for JSON Lines.
    fn stats(&self) -> bool {
        self.output_kind() == OutputKind::JSON || self.is_present("stats")
    }

    /// When the output format is `Summary`, this returns the type of summary
    /// output to show.
    ///
    /// This returns `None` if the output format is not `Summary`.
    fn summary_kind(&self) -> Option<SummaryKind> {
        let (count, count_matches) = self.counts();
        if self.is_present("quiet") {
            Some(SummaryKind::Quiet)
        } else if count_matches {
            Some(SummaryKind::CountMatches)
        } else if count {
            Some(SummaryKind::Count)
        } else if self.is_present("files-with-matches") {
            Some(SummaryKind::PathWithMatch)
        } else if self.is_present("files-without-match") {
            Some(SummaryKind::PathWithoutMatch)
        } else {
            None
        }
    }

    /// Return the number of threads that should be used for parallelism.
    fn threads(&self) -> Result<usize> {
        if self.sort_by()?.kind != SortByKind::None {
            return Ok(1);
        }
        let threads = self.usize_of("threads")?.unwrap_or(0);
        let available =
            std::thread::available_parallelism().map_or(1, |n| n.get());
        Ok(if threads == 0 { cmp::min(12, available) } else { threads })
    }

    /// Builds a file type matcher from the command line flags.
    fn types(&self) -> Result<Types> {
        let mut builder = TypesBuilder::new();
        builder.add_defaults();
        for ty in self.values_of_lossy_vec("type-clear") {
            builder.clear(&ty);
        }
        for def in self.values_of_lossy_vec("type-add") {
            builder.add_def(&def)?;
        }
        for ty in self.values_of_lossy_vec("type") {
            builder.select(&ty);
        }
        for ty in self.values_of_lossy_vec("type-not") {
            builder.negate(&ty);
        }
        builder.build().map_err(From::from)
    }

    /// Returns the number of times the `unrestricted` flag is provided.
    fn unrestricted_count(&self) -> u64 {
        self.occurrences_of("unrestricted")
    }

    /// Returns true if and only if Unicode mode should be enabled.
    fn unicode(&self) -> bool {
        // Unicode mode is enabled by default, so only disable it when
        // --no-unicode is given explicitly.
        !(self.is_present("no-unicode") || self.is_present("no-pcre2-unicode"))
    }

    /// Returns true if and only if file names containing each match should
    /// be emitted.
    fn with_filename(&self, paths: &[PathBuf]) -> bool {
        if self.is_present("no-filename") {
            false
        } else {
            let path_stdin = Path::new("-");
            self.is_present("with-filename")
                || self.is_present("vimgrep")
                || paths.len() > 1
                || paths
                    .get(0)
                    .map_or(false, |p| p != path_stdin && p.is_dir())
        }
    }
}

/// Lower level generic helper methods for teasing values out of clap.
impl ArgMatches {
    /// Like values_of_lossy, but returns an empty vec if the flag is not
    /// present.
    fn values_of_lossy_vec(&self, name: &str) -> Vec<String> {
        self.values_of_lossy(name).unwrap_or_else(Vec::new)
    }

    /// Safely reads an arg value with the given name, and if it's present,
    /// tries to parse it as a usize value.
    ///
    /// If the number is zero, then it is considered absent and `None` is
    /// returned.
    fn usize_of_nonzero(&self, name: &str) -> Result<Option<usize>> {
        let n = match self.usize_of(name)? {
            None => return Ok(None),
            Some(n) => n,
        };
        Ok(if n == 0 { None } else { Some(n) })
    }

    /// Safely reads an arg value with the given name, and if it's present,
    /// tries to parse it as a usize value.
    fn usize_of(&self, name: &str) -> Result<Option<usize>> {
        match self.value_of_lossy(name) {
            None => Ok(None),
            Some(v) => v.parse().map(Some).map_err(From::from),
        }
    }

    /// Parses an argument of the form `[0-9]+(KMG)?`.
    ///
    /// If the aforementioned format is not recognized, then this returns an
    /// error.
    fn parse_human_readable_size(
        &self,
        arg_name: &str,
    ) -> Result<Option<u64>> {
        let size = match self.value_of_lossy(arg_name) {
            None => return Ok(None),
            Some(size) => size,
        };
        Ok(Some(cli::parse_human_readable_size(&size)?))
    }
}

/// The following methods mostly dispatch to the underlying clap methods
/// directly. Methods that would otherwise get a single value will fetch all
/// values and return the last one. (Clap returns the first one.) We only
/// define the ones we need.
impl ArgMatches {
    fn is_present(&self, name: &str) -> bool {
        self.0.is_present(name)
    }

    fn occurrences_of(&self, name: &str) -> u64 {
        self.0.occurrences_of(name)
    }

    fn value_of_lossy(&self, name: &str) -> Option<String> {
        self.0.value_of_lossy(name).map(|s| s.into_owned())
    }

    fn values_of_lossy(&self, name: &str) -> Option<Vec<String>> {
        self.0.values_of_lossy(name)
    }

    fn value_of_os(&self, name: &str) -> Option<&OsStr> {
        self.0.value_of_os(name)
    }

    fn values_of_os(&self, name: &str) -> Option<clap::OsValues<'_>> {
        self.0.values_of_os(name)
    }
}

/// Inspect an error resulting from building a Rust regex matcher, and if it's
/// believed to correspond to a syntax error that another engine could handle,
/// then add a message to suggest the use of the engine flag.
fn suggest(msg: String) -> String {
    if let Some(pcre_msg) = suggest_pcre2(&msg) {
        return pcre_msg;
    }
    msg
}

/// Inspect an error resulting from building a Rust regex matcher, and if it's
/// believed to correspond to a syntax error that PCRE2 could handle, then
/// add a message to suggest the use of -P/--pcre2.
fn suggest_pcre2(msg: &str) -> Option<String> {
    #[cfg(feature = "pcre2")]
    fn suggest(msg: &str) -> Option<String> {
        if !msg.contains("backreferences") && !msg.contains("look-around") {
            None
        } else {
            Some(format!(
                "{}

Consider enabling PCRE2 with the --pcre2 flag, which can handle backreferences
and look-around.",
                msg
            ))
        }
    }

    #[cfg(not(feature = "pcre2"))]
    fn suggest(_: &str) -> Option<String> {
        None
    }

    suggest(msg)
}

fn suggest_multiline(msg: String) -> String {
    if msg.contains("the literal") && msg.contains("not allowed") {
        format!(
            "{}

Consider enabling multiline mode with the --multiline flag (or -U for short).
When multiline mode is enabled, new line characters can be matched.",
            msg
        )
    } else {
        msg
    }
}

/// Convert the result of parsing a human readable file size to a `usize`,
/// failing if the type does not fit.
fn u64_to_usize(arg_name: &str, value: Option<u64>) -> Result<Option<usize>> {
    use std::usize;

    let value = match value {
        None => return Ok(None),
        Some(value) => value,
    };
    if value <= usize::MAX as u64 {
        Ok(Some(value as usize))
    } else {
        Err(From::from(format!("number too large for {}", arg_name)))
    }
}

/// Sorts by an optional parameter.
//
/// If parameter is found to be `None`, both entries compare equal.
fn sort_by_option<T: Ord>(
    p1: &Option<T>,
    p2: &Option<T>,
    reverse: bool,
) -> cmp::Ordering {
    match (p1, p2, reverse) {
        (Some(p1), Some(p2), true) => p1.cmp(&p2).reverse(),
        (Some(p1), Some(p2), false) => p1.cmp(&p2),
        _ => cmp::Ordering::Equal,
    }
}

/// 如果给定的参数成功解析，则返回一个 clap 的匹配对象。
///
/// 否则，如果发生错误，除非错误对应于 `--help` 或 `--version` 请求，否则将返回错误。
/// 在这种情况下，将打印相应的输出，并成功退出当前进程。
fn clap_matches<I, T>(args: I) -> Result<clap::ArgMatches<'static>>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let err: clap::Error = match app::app().get_matches_from_safe(args) {
        Ok(matches) => return Ok(matches),
        Err(err) => err,
    };
    if err.use_stderr() {
        return Err(err.into());
    }
    // 显式地忽略 write! 返回的任何错误。
    // 在这一点上，最可能的错误是损坏的管道错误，在这种情况下，我们希望忽略它并静默退出。
    //
    //（这就是此辅助函数的目的。clap 用于执行此操作的功能会在损坏的管道错误时 panic。）
    let _ = write!(io::stdout(), "{}", err);
    process::exit(0);
}

/// Attempts to discover the current working directory. This mostly just defers
/// to the standard library, however, such things will fail if ripgrep is in
/// a directory that no longer exists. We attempt some fallback mechanisms,
/// such as querying the PWD environment variable, but otherwise return an
/// error.
fn current_dir() -> Result<PathBuf> {
    let err = match env::current_dir() {
        Err(err) => err,
        Ok(cwd) => return Ok(cwd),
    };
    if let Some(cwd) = env::var_os("PWD") {
        if !cwd.is_empty() {
            return Ok(PathBuf::from(cwd));
        }
    }
    Err(format!(
        "failed to get current working directory: {} \
         --- did your CWD get deleted?",
        err,
    )
    .into())
}

/// Tries to assign a timestamp to every `Subject` in the vector to help with
/// sorting Subjects by time.
fn load_timestamps<G>(
    subjects: impl Iterator<Item = Subject>,
    get_time: G,
) -> Vec<(Option<SystemTime>, Subject)>
where
    G: Fn(&fs::Metadata) -> io::Result<SystemTime>,
{
    subjects
        .map(|s| (s.path().metadata().and_then(|m| get_time(&m)).ok(), s))
        .collect()
}
