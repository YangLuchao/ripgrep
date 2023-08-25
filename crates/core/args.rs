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
/// 基于命令行配置，ripgrep 应该执行的命令。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Command {
    /// 使用单个线程进行搜索。
    Search,
    /// 使用可能的多个线程进行搜索。
    SearchParallel,
    /// 命令行参数表明应该进行搜索，但是 ripgrep 知道永远找不到匹配（例如，没有给定模式或--max-count=0）。
    SearchNever,
    /// 显示将被搜索的文件，但不实际搜索它们，并且使用单个线程。
    Files,
    /// 显示将被搜索的文件，但不实际搜索它们，并且使用可能的多个线程进行目录遍历。
    FilesParallel,
    /// 列出所有已配置的文件类型定义，包括默认文件类型和添加到命令行的其他文件类型。
    Types,
    /// 打印所使用的 PCRE2 版本。
    PCRE2Version,
}

impl Command {
    /// 仅当此命令需要执行搜索时，返回 true。
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

/// 在 ripgrep 中使用的主要配置对象。它提供了一个高级方便的接口来处理提供的命令行参数。
///
/// `Args` 对象在克隆时成本较低，并且可以同时从多个线程中使用。
#[derive(Clone, Debug)]
pub struct Args(Arc<ArgsImp>);

#[derive(Clone, Debug)]
struct ArgsImp {
    /// 用于提取 CLI 参数的中至低级别例程。
    matches: ArgMatches,
    /// 我们要执行的命令。
    command: Command,
    /// 要使用的线程数。这部分基于可用线程，部分基于请求的线程数，部分基于正在运行的命令。
    threads: usize,
    /// 从模式构建的匹配器。
    ///
    /// 重要的是仅构建一次此匹配器，因为构建它涉及到正则表达式编译和各种类型的分析。
    /// 也就是说，如果需要许多这样的匹配器（例如，每个线程一个），最好是构建一次，然后进行克隆。
    matcher: PatternMatcher,
    /// 在命令行中提供的路径。这是保证非空的。（如果没有提供路径，则会创建一个默认路径。）
    paths: Vec<PathBuf>,
    /// 如果且仅如果 `paths` 必须用单个默认路径填充。
    using_default_path: bool,
}

impl Args {
    /// 解析此进程的命令行参数。
    ///
    /// 如果发生 CLI 使用错误，则退出进程并打印用法或错误消息。同样，如果用户请求 ripgrep 的版本，则打印版本并退出。
    ///
    /// 同时，初始化全局日志记录器。
    pub fn parse() -> Result<Args> {
        // 我们解析 CLI 上给出的参数。这不包括来自配置的参数。
        // 我们使用 CLI 参数作为初始配置，同时尝试解析配置文件。
        // 如果配置文件存在并且有参数，那么我们重新解析 argv，否则我们只使用这里的匹配项。
        let early_matches = ArgMatches::new(clap_matches(env::args_os())?);
        set_messages(!early_matches.is_present("no-messages"));
        set_ignore_messages(!early_matches.is_present("no-ignore-messages"));

        if let Err(err) = Logger::init() {
            return Err(format!("初始化日志记录器失败：{}", err).into());
        }
        if early_matches.is_present("trace") {
            log::set_max_level(log::LevelFilter::Trace);
        } else if early_matches.is_present("debug") {
            log::set_max_level(log::LevelFilter::Debug);
        } else {
            log::set_max_level(log::LevelFilter::Warn);
        }

        let matches = early_matches.reconfigure()?;
        // 如果我们从配置文件引入了额外的参数，则日志级别可能已更改，因此重新检查并设置适当的日志级别。
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

    /// 返回对命令行参数的直接访问。
    fn matches(&self) -> &ArgMatches {
        &self.0.matches
    }

    /// 返回从模式构建的匹配器生成器。
    fn matcher(&self) -> &PatternMatcher {
        &self.0.matcher
    }

    /// 返回在命令行参数中找到的路径。这是保证非空的。
    /// 在没有提供显式参数的情况下，会自动提供单个默认路径。
    fn paths(&self) -> &[PathBuf] {
        &self.0.paths
    }

    /// 如果且仅如果 `paths` 必须用默认路径填充，这仅在没有路径作为命令行参数给出时发生。
    pub fn using_default_path(&self) -> bool {
        self.0.using_default_path
    }

    /// 返回应该用于格式化搜索结果输出的打印机。
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
    /// 为支持多线程打印并具有颜色支持，创建一个新的缓冲区写入器。
    pub fn buffer_writer(&self) -> Result<BufferWriter> {
        let mut wtr = BufferWriter::stdout(self.matches().color_choice());
        wtr.separator(self.matches().file_separator()?);
        Ok(wtr)
    }

    /// 返回 ripgrep 应该运行的高级命令。
    pub fn command(&self) -> Command {
        self.0.command
    }

    /// 构建一个路径打印机，可用于仅打印文件路径，带有可选的颜色支持。
    ///
    /// 打印机将路径写入给定的写入器。
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

    /// 仅当 ripgrep 应该“安静”时，返回 true。
    pub fn quiet(&self) -> bool {
        self.matches().is_present("quiet")
    }

    /// 当找到第一个匹配后，仅在需要时返回 true。
    pub fn quit_after_match(&self) -> Result<bool> {
        Ok(self.matches().is_present("quiet") && self.stats()?.is_none())
    }

    /// 构建一个用于执行搜索的工作器。
    ///
    /// 搜索结果将写入给定的写入器。
    pub fn search_worker<W: WriteColor>(
        &self,
        wtr: W,
    ) -> Result<SearchWorker<W>> {
        let matches = self.matches();
        let matcher = self.matcher().clone();
        let printer = self.printer(wtr)?;
        let searcher = matches.searcher(self.paths())?;
        let mut builder = SearchWorkerBuilder::new();
        builder
            .json_stats(matches.is_present("json"))
            .preprocessor(matches.preprocessor())?
            .preprocessor_globs(matches.preprocessor_globs()?)
            .search_zip(matches.is_present("search-zip"))
            .binary_detection_implicit(matches.binary_detection_implicit())
            .binary_detection_explicit(matches.binary_detection_explicit());
        Ok(builder.build(matcher, searcher, printer))
    }

    /// 当且仅当请求了统计信息时，为跟踪统计信息提供零值。
    ///
    /// 当此函数返回 `Stats` 值时，可以保证搜索工作器也将被配置为跟踪统计信息。
    pub fn stats(&self) -> Result<Option<Stats>> {
        Ok(if self.command().is_search() && self.matches().stats() {
            Some(Stats::new())
        } else {
            None
        })
    }

    /// 返回用于构建主题的构建器。主题表示要搜索的单个单元。通常，这对应于文件或流，如标准输入。
    pub fn subject_builder(&self) -> SubjectBuilder {
        let mut builder = SubjectBuilder::new();
        builder.strip_dot_prefix(self.using_default_path());
        builder
    }

    /// 使用启用基于命令行配置的颜色支持的标准输出写入器执行给定函数。
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
    /// 如果在读取和解析类型定义时出现问题，则返回错误。
    pub fn type_defs(&self) -> Result<Vec<FileTypeDef>> {
        Ok(self.matches().types()?.definitions().to_vec())
    }

    /// 返回不使用附加线程的 walker。
    pub fn walker(&self) -> Result<Walk> {
        Ok(self
            .matches()
            .walker_builder(self.paths(), self.0.threads)?
            .build())
    }

    /// 当且仅当需要 `stat` 相关的排序时，返回 true。
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

    /// 如果指定了排序器，则按照 `stat` 相关的排序方式对主题进行排序，
    /// 但仅在排序需要 `stat` 调用时进行。
    /// 非 `stat` 相关的排序在文件遍历期间处理。
    ///
    /// 此函数假设已知需要 `stat` 相关的排序，并且不再次检查。
    /// 由于此函数消耗主题迭代器，因此是一个阻塞函数。
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

    /// 返回可能使用附加线程的并行 walker。
    pub fn walker_parallel(&self) -> Result<WalkParallel> {
        Ok(self
            .matches()
            .walker_builder(self.paths(), self.0.threads)?
            .build_parallel())
    }
}

/// `ArgMatches` 包装了 `clap::ArgMatches` 并为解析的参数提供了语义含义。
#[derive(Clone, Debug)]
struct ArgMatches(clap::ArgMatches<'static>);

/// 输出格式。通常，这对应于 ripgrep 用于显示搜索结果的打印机。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputKind {
    /// 类似于经典 grep 或 ack 的格式。
    Standard,
    /// 显示匹配的文件并可能在每个文件中显示匹配的数量。
    Summary,
    /// 以 JSON 行格式发出匹配信息。
    JSON,
}

/// 排序标准，如果存在的话。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SortBy {
    /// 是否反转排序标准（即降序）。
    reverse: bool,
    /// 实际的排序标准。
    kind: SortByKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SortByKind {
    /// 没有排序。
    None,
    /// 按路径排序。
    Path,
    /// 按最后修改时间排序。
    LastModified,
    /// 按最后访问时间排序。
    LastAccessed,
    /// 按创建时间排序。
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

    /// 尝试检查所选的排序标准是否受支持。如果不受支持，则返回错误。
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

    /// 仅在步行阶段适用时加载排序器。
    ///
    /// 特别地，涉及 `stat` 调用的排序不会加载，因为步行固有地假定父目录知道其所有子属性，但 `stat` 不是这样工作的。
    fn configure_builder_sort(self, builder: &mut WalkBuilder) {
        use SortByKind::*;
        match self.kind {
            Path if self.reverse => {
                builder.sort_by_file_name(|a, b| a.cmp(b).reverse());
            }
            Path => {
                builder.sort_by_file_name(|a, b| a.cmp(b));
            }
            // 这些使用 `stat` 调用，将在 Args::sort_by_stat() 中进行排序
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
/// 搜索器将使用的编码模式。
#[derive(Clone, Debug)]
enum EncodingMode {
    /// 强制使用显式编码，但 BOM 嗅探会覆盖它。
    Some(Encoding),
    /// 仅使用 BOM 嗅探来自动检测编码。
    Auto,
    /// 不使用显式编码并禁用所有 BOM 嗅探。
    /// 这将始终导致搜索原始字节，而不考虑它们的实际编码。
    Disabled,
}

/// 从 clap 的解析结果创建 ArgMatches。
impl ArgMatches {
    fn new(clap_matches: clap::ArgMatches<'static>) -> ArgMatches {
        ArgMatches(clap_matches)
    }

    /// 运行 clap 并返回匹配的结果，如果存在配置文件则使用它。
    /// 如果 clap 确定用户提供的参数存在问题（或给出了 --help 或 --version），则会打印错误/用法/版本并退出进程。
    ///
    /// 如果没有来自环境的其他参数（例如配置文件），则返回给定的匹配结果。
    fn reconfigure(self) -> Result<ArgMatches> {
        // 如果最终用户说不使用配置文件，则尊重。
        if self.is_present("no-config") {
            log::debug!("由于 --no-config 存在，不读取配置文件");
            return Ok(self);
        }
        // 如果用户希望 ripgrep 使用配置文件，则首先从中解析参数。
        let mut args = config::args();
        if args.is_empty() {
            return Ok(self);
        }
        let mut cliargs = env::args_os();
        if let Some(bin) = cliargs.next() {
            args.insert(0, bin);
        }
        args.extend(cliargs);
        log::debug!("最终的 argv: {:?}", args);
        Ok(ArgMatches(clap_matches(args)?))
    }

    /// 将解析 CLI 参数的结果转换为 ripgrep 的高级配置结构。
    fn to_args(self) -> Result<Args> {
        // 由于这些可能很大，我们只计算一次。
        let patterns = self.patterns()?;
        let matcher = self.matcher(&patterns)?;
        let mut paths = self.paths();
        let using_default_path = if paths.is_empty() {
            paths.push(self.path_default());
            true
        } else {
            false
        };
        // 现在确定我们将使用的线程数以及要运行的命令。
        let is_one_search = self.is_one_search(&paths);
        let threads = if is_one_search { 1 } else { self.threads()? };
        if threads == 1 {
            log::debug!("以单线程模式运行");
        } else {
            log::debug!("以 {threads} 线程运行以实现并行性");
        }
        let command = if self.is_present("pcre2-version") {
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

/// 将命令行参数转换为 ripgrep 使用的各种数据结构的高级方法。
///
/// 方法按字母顺序排列。
impl ArgMatches {
    /// 返回应该用于搜索的匹配器。
    ///
    /// 如果构建匹配器时出现问题（例如语法错误），则返回错误。
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

    /// 返回应该用于使用引擎的匹配器搜索。
    ///
    /// 如果构建匹配器时出现问题（例如语法错误），则返回错误。
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
            "pcre2" => {
                Err(From::from("在此版本的 ripgrep 中不可用 PCRE2 引擎"))
            }
            "auto" => {
                let rust_err = match self.matcher_rust(patterns) {
                    Ok(matcher) => {
                        return Ok(PatternMatcher::RustRegex(matcher));
                    }
                    Err(err) => err,
                };
                log::debug!(
                    "在混合模式下构建 Rust 正则表达式时出现错误：\n{}",
                    rust_err,
                );

                let pcre_err = match self.matcher_engine("pcre2", patterns) {
                    Ok(matcher) => return Ok(matcher),
                    Err(err) => err,
                };
                Err(From::from(format!(
                    "无法使用默认正则引擎或 PCRE2 编译正则表达式。\n\n\
                    默认正则引擎错误：\n{}\n{}\n{}\n\n\
                    PCRE2 正则引擎错误：\n{}",
                    "~".repeat(79),
                    rust_err,
                    "~".repeat(79),
                    pcre_err,
                )))
            }
            _ => Err(From::from(format!("无法识别的正则引擎 '{}'", engine))),
        }
    }

    /// 使用 Rust 的正则引擎构建匹配器。
    ///
    /// 如果构建匹配器时出现问题（例如正则语法错误），则返回错误。
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
            // 我们不需要在多行模式中设置这个，因为多行匹配器不使用与行终止符相关的优化。
            // 此外，与 --null-data 一起使用的多行正则表达式应该可以显式地匹配 NUL 字节，否则这将禁止。
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

    /// 使用 PCRE2 构建匹配器。
    ///
    /// 如果构建匹配器时出现问题（例如正则语法错误），则返回错误。
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
        // 不知何故，JIT 在 32 位系统上在正则表达式编译过程中出现了 "no more memory" 错误。所以在那里不使用它。
        if cfg!(target_pointer_width = "64") {
            builder
                .jit_if_available(true)
                // PCRE2 文档说 32KB 是默认值，而 1MB 对任何情况都足够了。但让我们将其增加到 10MB。
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

    /// 构建一个将结果写入给定写入器的 JSON 打印机。
    fn printer_json<W: io::Write>(&self, wtr: W) -> Result<JSON<W>> {
        let mut builder = JSONBuilder::new();
        builder
            .pretty(false)
            .max_matches(self.max_count()?)
            .always_begin_end(false);
        Ok(builder.build(wtr))
    }

    /// 构建一个将结果写入给定写入器的标准打印机。
    ///
    /// 给定的路径用于配置打印机的各个方面。
    ///
    /// 如果 `separator_search` 为 true，则返回的打印机将在适当时（例如启用上下文时）负责打印每组搜索结果之间的分隔符。
    /// 当设置为 false 时，调用者需要负责处理分隔符。
    ///
    /// 实际上，在单线程情况下，我们希望打印机处理分隔符，而在多线程情况下不希望。
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

    /// 构建一个将结果写入给定写入器的摘要打印机。
    ///
    /// 给定的路径用于配置打印机的各个方面。
    ///
    /// 如果输出格式不是 `OutputKind::Summary`，则会 panic。
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

    /// 从命令行参数构建一个搜索器。
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
            EncodingMode::Auto => {} // 搜索器的默认值
            EncodingMode::Disabled => {
                builder.bom_sniffing(false);
            }
        }
        Ok(builder.build())
    }

    /// 返回递归遍历目录的构建器，同时遵守忽略规则。
    ///
    /// 如果解析构建器所需的 CLI 参数存在问题，则返回错误。
    fn walker_builder(
        &self,
        paths: &[PathBuf],
        threads: usize,
    ) -> Result<WalkBuilder> {
        let mut builder = WalkBuilder::new(&paths[0]);
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

/// 将命令行参数转换为各种类型的数据结构的中级方法。
///
/// 方法按字母顺序排列。
impl ArgMatches {
    /// 返回在通过递归目录遍历隐式搜索的文件上执行的二进制检测形式。
    fn binary_detection_implicit(&self) -> BinaryDetection {
        // 检查是否存在 "text" 或 "null-data" 标志。
        let none = self.is_present("text") || self.is_present("null-data");
        // 检查是否存在 "binary" 标志，或者未受限制的计数是否大于等于 3。
        let convert =
            self.is_present("binary") || self.unrestricted_count() >= 3;
        // 根据条件选择适当的 BinaryDetection 操作。
        if none {
            BinaryDetection::none()
        } else if convert {
            BinaryDetection::convert(b'\x00')
        } else {
            BinaryDetection::quit(b'\x00')
        }
    }

    /// 返回在通过用户在特定文件、文件或标准输入上调用 ripgrep 显式搜索的文件上执行的二进制检测形式。
    ///
    /// 一般来说，这不应该是 BinaryDetection::quit，因为它充当过滤器（但一旦看到 NUL 字节就立即退出），
    /// 我们不应过滤掉用户想要显式搜索的文件。
    fn binary_detection_explicit(&self) -> BinaryDetection {
        // 检查是否存在 "text" 或 "null-data" 标志。
        let none = self.is_present("text") || self.is_present("null-data");
        // 根据条件选择适当的 BinaryDetection 操作。
        if none {
            BinaryDetection::none()
        } else {
            BinaryDetection::convert(b'\x00')
        }
    }

    /// 根据命令行配置判断是否永远不会显示匹配结果。
    fn can_never_match(&self, patterns: &[String]) -> bool {
        // 检查是否未提供搜索模式，或者最大计数为 0。
        patterns.is_empty() || self.max_count().ok() == Some(Some(0))
    }

    /// 判断是否应忽略大小写。
    ///
    /// 如果存在 "--case-sensitive"，则不应忽略大小写，即使存在 "--ignore-case"。
    fn case_insensitive(&self) -> bool {
        // 检查是否存在 "--ignore-case"，且不存在 "--case-sensitive"。
        self.is_present("ignore-case") && !self.is_present("case-sensitive")
    }

    /// 判断是否启用了智能大小写。
    ///
    /// 如果存在 "--ignore-case" 或 "--case-sensitive" 中的任意一个，智能大小写将被禁用。
    fn case_smart(&self) -> bool {
        // 检查是否存在 "--smart-case"，且不存在 "--ignore-case" 和 "--case-sensitive"。
        self.is_present("smart-case")
            && !self.is_present("ignore-case")
            && !self.is_present("case-sensitive")
    }

    /// 根据命令行参数和环境返回用户的颜色选择。
    fn color_choice(&self) -> ColorChoice {
        // 获取用户对颜色的偏好，若未提供则默认为 "auto"。
        let preference = match self.value_of_lossy("color") {
            None => "auto".to_string(),
            Some(v) => v,
        };
        // 根据偏好选择相应的颜色模式。
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

    /// 返回用户在命令行界面上提供的颜色规范。
    ///
    /// 若解析规范出现问题，则返回错误。
    fn color_specs(&self) -> Result<ColorSpecs> {
        // 使用默认颜色规范集合开始。
        let mut specs = default_color_specs();
        // 遍历解析用户提供的颜色规范。
        for spec_str in self.values_of_lossy_vec("colors") {
            specs.push(spec_str.parse()?);
        }
        Ok(ColorSpecs::new(&specs))
    }

    /// 判断是否应显示列号。
    fn column(&self) -> bool {
        // 检查是否存在 "--no-column" 标志。
        if self.is_present("no-column") {
            return false;
        }
        // 检查是否存在 "--column" 或 "--vimgrep" 标志。
        self.is_present("column") || self.is_present("vimgrep")
    }

    /// 返回命令行中的上下文设置。
    ///
    /// 若上下文设置未提供，则返回 0。
    ///
    /// 若解析用户提供的整数值出现问题，则返回错误。
    fn contexts(&self) -> Result<(usize, usize)> {
        // 获取上下文设置，若未提供则默认为 0。
        let both = self.usize_of("context")?.unwrap_or(0);
        let after = self.usize_of("after-context")?.unwrap_or(both);
        let before = self.usize_of("before-context")?.unwrap_or(both);
        Ok((before, after))
    }

    /// 返回未转义的上下文分隔符的 UTF-8 字节。
    ///
    /// 若未提供分隔符，则返回默认的 "--"。
    /// 若传递了 "--no-context-separator"，则返回 None。
    fn context_separator(&self) -> Option<Vec<u8>> {
        // 检查是否存在 "--no-context-separator" 标志。
        let nosep = self.is_present("no-context-separator");
        let sep = self.value_of_os("context-separator");
        // 根据条件返回适当的上下文分隔符。
        match (nosep, sep) {
            (true, _) => None,
            (false, None) => Some(b"--".to_vec()),
            (false, Some(sep)) => Some(cli::unescape_os(&sep)),
        }
    }

    /// 判断是否从命令行传递了 "-c/--count" 或 "--count-matches" 标志。
    ///
    /// 若同时传递了 "--count-matches" 和 "--invert-match"，则视为同时传递了 "-c" 和 "--invert-match"（即 rg 将根据现有行为计算反转匹配的数量）。
    fn counts(&self) -> (bool, bool) {
        let count = self.is_present("count");
        let count_matches = self.is_present("count-matches");
        let invert_matches = self.is_present("invert-match");
        let only_matching = self.is_present("only-matching");
        if count_matches && invert_matches {
            // 将 "-v --count-matches" 视为 "-v -c"。
            (true, false)
        } else if count && only_matching {
            // 将 "-c --only-matching" 视为 "--count-matches"。
            (false, true)
        } else {
            (count, count_matches)
        }
    }

    /// 将 dfa-size-limit 参数选项解析为字节数。
    fn dfa_size_limit(&self) -> Result<Option<usize>> {
        let r = self.parse_human_readable_size("dfa-size-limit")?;
        u64_to_usize("dfa-size-limit", r)
    }

    /// 返回要使用的编码模式。
    ///
    /// 仅当显式指定了编码时，此方法返回一个编码。否则，如果设置为自动模式，则 Searcher 将对 UTF-16 进行 BOM 侦测和无缝转码。如果禁用，则不会进行 BOM 侦测和转码。
    fn encoding(&self) -> Result<EncodingMode> {
        // 检查是否存在 "--no-encoding" 标志。
        if self.is_present("no-encoding") {
            return Ok(EncodingMode::Auto);
        }

        // 获取编码标签。
        let label = match self.value_of_lossy("encoding") {
            None => return Ok(EncodingMode::Auto),
            Some(label) => label,
        };

        // 根据标签选择适当的编码模式。
        if label == "auto" {
            return Ok(EncodingMode::Auto);
        } else if label == "none" {
            return Ok(EncodingMode::Disabled);
        }

        Ok(EncodingMode::Some(Encoding::new(&label)?))
    }

    /// 根据 CLI 配置返回文件分隔符。
    fn file_separator(&self) -> Result<Option<Vec<u8>>> {
        // 仅当输出格式为标准 grep-line 时使用文件分隔符。
        if self.output_kind() != OutputKind::Standard {
            return Ok(None);
        }

        // 获取上下文设置。
        let (ctx_before, ctx_after) = self.contexts()?;
        // 根据条件返回适当的文件分隔符。
        Ok(if self.heading() {
            Some(b"".to_vec())
        } else if ctx_before > 0 || ctx_after > 0 {
            self.context_separator()
        } else {
            None
        })
    }

    /// 判断是否应将匹配结果与文件名标题分组。
    fn heading(&self) -> bool {
        // 检查是否存在 "--no-heading" 或 "--vimgrep" 标志。
        if self.is_present("no-heading") || self.is_present("vimgrep") {
            false
        } else {
            // 若为终端输出或存在 "--heading" 或 "--pretty" 标志，则返回 true。
            cli::is_tty_stdout()
                || self.is_present("heading")
                || self.is_present("pretty")
        }
    }
    /// 返回 true 当且仅当应搜索隐藏的文件/目录。
    fn hidden(&self) -> bool {
        // 检查是否存在 "hidden" 标志，或未受限制的计数是否大于等于 2。
        self.is_present("hidden") || self.unrestricted_count() >= 2
    }

    /// 返回 true 当且仅当应在处理忽略文件时忽略大小写。
    fn ignore_file_case_insensitive(&self) -> bool {
        self.is_present("ignore-file-case-insensitive")
    }

    /// 返回命令行上提供的所有忽略文件路径。
    fn ignore_paths(&self) -> Vec<PathBuf> {
        let paths = match self.values_of_os("ignore-file") {
            None => return vec![],
            Some(paths) => paths,
        };
        paths.map(|p| Path::new(p).to_path_buf()).collect()
    }

    /// 返回 true 当且仅当 ripgrep 以确切搜索一项的方式调用。
    fn is_one_search(&self, paths: &[PathBuf]) -> bool {
        if paths.len() != 1 {
            return false;
        }
        self.is_only_stdin(paths) || paths[0].is_file()
    }

    /// 返回 true 当且仅当我们只搜索单个内容且该内容为标准输入。
    fn is_only_stdin(&self, paths: &[PathBuf]) -> bool {
        paths == [Path::new("-")]
    }

    /// 返回 true 当且仅当应显示行号。
    fn line_number(&self, paths: &[PathBuf]) -> bool {
        // 如果输出类型为摘要，则不显示行号。
        if self.output_kind() == OutputKind::Summary {
            return false;
        }
        if self.is_present("no-line-number") {
            return false;
        }
        if self.output_kind() == OutputKind::JSON {
            return true;
        }

        // 一些情况下可以暗示计数行号。特别是，通常情况下，在终端输出以供人类阅读时，会默认显示行号，除了一个有趣的情况：当我们只搜索标准输入时。这使得管道工作正常。
        (cli::is_tty_stdout() && !self.is_only_stdin(paths))
            || self.is_present("line-number")
            || self.is_present("column")
            || self.is_present("pretty")
            || self.is_present("vimgrep")
    }

    /// 每行允许的最大列数。
    ///
    /// 如果提供了 `0`，则返回 `None`。
    fn max_columns(&self) -> Result<Option<u64>> {
        Ok(self.usize_of_nonzero("max-columns")?.map(|n| n as u64))
    }

    /// 返回 true 当且仅当应为超过最大列限制的行显示预览。
    fn max_columns_preview(&self) -> bool {
        self.is_present("max-columns-preview")
    }

    /// 允许的最大匹配数。
    fn max_count(&self) -> Result<Option<u64>> {
        Ok(self.usize_of("max-count")?.map(|n| n as u64))
    }

    /// 将 max-filesize 参数选项解析为字节数。
    fn max_file_size(&self) -> Result<Option<u64>> {
        self.parse_human_readable_size("max-filesize")
    }

    /// 返回是否应尝试使用内存映射。
    fn mmap_choice(&self, paths: &[PathBuf]) -> MmapChoice {
        // 安全性：内存映射很难以不受限制地封装到不会同时否定使用内存映射的某些好处的可移植方式中。对于 ripgrep 的用途，我们从不对内存映射进行突变，并且通常不在取决于不可变性的数据结构中存储内存映射的内容。一般来说，最坏的情况是可能出现 SIGBUS（如果在读取时底层文件被截断），这会导致 ripgrep 中止。应该对此推理持怀疑态度。
        let maybe = unsafe { MmapChoice::auto() };
        let never = MmapChoice::never();
        if self.is_present("no-mmap") {
            never
        } else if self.is_present("mmap") {
            maybe
        } else if paths.len() <= 10 && paths.iter().all(|p| p.is_file()) {
            // 如果只搜索少数路径且所有路径都是文件，则内存映射可能更快。
            maybe
        } else {
            never
        }
    }

    /// 返回是否应忽略忽略文件。
    fn no_ignore(&self) -> bool {
        self.is_present("no-ignore") || self.unrestricted_count() >= 1
    }

    /// 返回是否应忽略 .ignore 文件。
    fn no_ignore_dot(&self) -> bool {
        self.is_present("no-ignore-dot") || self.no_ignore()
    }

    /// 返回是否应忽略本地排除（ignore）文件。
    fn no_ignore_exclude(&self) -> bool {
        self.is_present("no-ignore-exclude") || self.no_ignore()
    }

    /// 返回是否应忽略显式提供的忽略文件。
    fn no_ignore_files(&self) -> bool {
        // 这里不看 no-ignore，因为 --no-ignore 明确文档化为不覆盖 --ignore-file。我们可以改变这一点，但这将是相当严重的破坏性变更。
        self.is_present("no-ignore-files")
    }

    /// 返回是否应忽略全局忽略文件。
    fn no_ignore_global(&self) -> bool {
        self.is_present("no-ignore-global") || self.no_ignore()
    }

    /// 返回是否应忽略父级忽略文件。
    fn no_ignore_parent(&self) -> bool {
        self.is_present("no-ignore-parent") || self.no_ignore()
    }

    /// 返回是否应忽略 VCS 忽略文件。
    fn no_ignore_vcs(&self) -> bool {
        self.is_present("no-ignore-vcs") || self.no_ignore()
    }

    /// 确定我们应生成的输出类型。
    fn output_kind(&self) -> OutputKind {
        if self.is_present("quiet") {
            // 虽然在安静模式下我们在技术上不会打印结果（或汇总结果），但我们仍然支持 --stats 标志，而这些统计信息目前是由 Summary 打印机计算的。
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

    /// 从命令行标志构建 glob 覆盖集。
    fn overrides(&self) -> Result<Override> {
        let globs = self.values_of_lossy_vec("glob");
        let iglobs = self.values_of_lossy_vec("iglob");
        if globs.is_empty() && iglobs.is_empty() {
            return Ok(Override::empty());
        }

        let mut builder = OverrideBuilder::new(current_dir()?);
        // 通过 --glob-case-insensitive 让所有 glob 忽略大小写。
        if self.is_present("glob-case-insensitive") {
            builder.case_insensitive(true).unwrap();
        }
        for glob in globs {
            builder.add(&glob)?;
        }
        // 这只会为后续 glob 启用大小写不敏感性。
        builder.case_insensitive(true).unwrap();
        for glob in iglobs {
            builder.add(&glob)?;
        }
        Ok(builder.build()?)
    }

    /// 返回 ripgrep 应搜索的所有文件路径。
    ///
    /// 如果未提供路径，则返回一个空列表。
    fn paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = match self.values_of_os("path") {
            None => vec![],
            Some(paths) => paths.map(|p| Path::new(p).to_path_buf()).collect(),
        };
        // 如果存在 --file、--files 或 --regexp，则第一个路径始终在 `pattern` 中。
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
    /// 返回 ripgrep 应搜索的默认路径。仅当 ripgrep 没有通过至少一个文件路径作为位置参数提供时才应使用此方法。
    fn path_default(&self) -> PathBuf {
        // 检查是否存在标志 "file"，且其中是否包含标准输入 "-"。
        let file_is_stdin = self
            .values_of_os("file")
            .map_or(false, |mut files| files.any(|f| f == "-"));

        // 确定是否应在当前工作目录中搜索。
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

    /// 返回未转义的路径分隔符，作为单个字节，如果存在的话。
    ///
    /// 如果提供的路径分隔符超过一个字节，则返回错误。
    fn path_separator(&self) -> Result<Option<u8>> {
        let sep = match self.value_of_os("path-separator") {
            None => return Ok(None),
            Some(sep) => cli::unescape_os(&sep),
        };
        if sep.is_empty() {
            Ok(None)
        } else if sep.len() > 1 {
            Err(From::from(format!(
                "路径分隔符必须恰好为一个字节，但给定的分隔符为 {} 字节：{}\n\
             在 Windows 上的某些 shell 中，'/' 会自动展开。请使用 '//' 代替。",
                sep.len(),
                cli::escape(&sep),
            )))
        } else {
            Ok(Some(sep[0]))
        }
    }

    /// 返回应用于路径的字节终止符。
    ///
    /// 通常，只有在提供了 --null 标志时，才会将其设置为 `\x00`；否则为 `None`。
    fn path_terminator(&self) -> Option<u8> {
        if self.is_present("null") {
            Some(b'\x00')
        } else {
            None
        }
    }

    /// 返回未转义的字段上下文分隔符。如果未指定，则默认使用 '-'。
    fn field_context_separator(&self) -> Vec<u8> {
        match self.value_of_os("field-context-separator") {
            None => b"-".to_vec(),
            Some(sep) => cli::unescape_os(&sep),
        }
    }

    /// 返回未转义的字段匹配分隔符。如果未指定，则默认使用 ':'。
    fn field_match_separator(&self) -> Vec<u8> {
        match self.value_of_os("field-match-separator") {
            None => b":".to_vec(),
            Some(sep) => cli::unescape_os(&sep),
        }
    }

    /// 从命令行参数获取所有可用的模式序列。这包括读取 -e/--regexp 和 -f/--file 标志。
    ///
    /// 如果任何模式无效，将返回错误。
    fn patterns(&self) -> Result<Vec<String>> {
        if self.is_present("files") || self.is_present("type-list") {
            return Ok(vec![]);
        }
        let mut pats = vec![];

        // 检查是否存在 "regexp" 标志。
        match self.values_of_os("regexp") {
            None => {
                // 如果未提供 "file" 标志，且未提供 "pattern" 参数，则检查是否提供了 "pattern" 参数，并将其添加到 pats 中。
                if self.values_of_os("file").is_none() {
                    if let Some(os_pat) = self.value_of_os("pattern") {
                        pats.push(self.pattern_from_os_str(os_pat)?);
                    }
                }
            }
            Some(os_pats) => {
                // 如果存在 "file" 参数，遍历 "regexp" 参数，并将每个模式添加到 pats 中。
                for os_pat in os_pats {
                    pats.push(self.pattern_from_os_str(os_pat)?);
                }
            }
        }

        // 如果提供了 "file" 参数，遍历 "file" 参数，并将每个模式添加到 pats 中。
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

    /// 将 OsStr 模式转换为 String 模式。如果设置了 -F/--fixed-strings 标志，则会对模式进行转义。
    ///
    /// 如果模式无效的 UTF-8，则返回错误。
    fn pattern_from_os_str(&self, pat: &OsStr) -> Result<String> {
        let s = cli::pattern_from_os(pat)?;
        Ok(self.pattern_from_str(s))
    }

    /// 将 &str 模式转换为 String 模式。如果设置了 -F/--fixed-strings 标志，则会对模式进行转义。
    fn pattern_from_str(&self, pat: &str) -> String {
        self.pattern_from_string(pat.to_string())
    }

    /// 根据需要对给定的模式进行额外处理（例如转义元字符或将其转换为行正则表达式）。
    fn pattern_from_string(&self, pat: String) -> String {
        if pat.is_empty() {
            // 这通常只是一个空字符串，这本身就有效，但如果将模式连接在备选组的集合中，则最终会得到 `foo|`，在 Rust 的正则表达式引擎中当前是无效的。
            "(?:)".to_string()
        } else {
            pat
        }
    }

    /// 如果指定了预处理器命令，则返回预处理器命令的路径。
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

    /// 构建应应用于 --pre 标志的文件过滤器的 glob 集。如果不存在 --pre-globs，则始终返回空的 glob 集。
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

    /// 将 regex-size-limit 参数选项解析为字节计数。
    fn regex_size_limit(&self) -> Result<Option<usize>> {
        let r = self.parse_human_readable_size("regex-size-limit")?;
        u64_to_usize("regex-size-limit", r)
    }

    /// 如果存在替换字符串，则返回其 UTF-8 字节。
    fn replacement(&self) -> Option<Vec<u8>> {
        self.value_of_lossy("replace").map(|s| s.into_bytes())
    }

    /// 基于命令行参数返回排序标准。
    fn sort_by(&self) -> Result<SortBy> {
        // 对于向后兼容性，继续支持已弃用的 --sort-files 标志。
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

    /// 返回 true 当且仅当应跟踪搜索的聚合统计信息。
    ///
    /// 通常，仅在通过命令行参数显式请求（通过 --stats 标志），或者通过输出格式隐式请求（例如 JSON Lines 格式），才启用此功能。
    fn stats(&self) -> bool {
        self.output_kind() == OutputKind::JSON || self.is_present("stats")
    }

    /// 当输出格式为 `Summary` 时，返回要显示的摘要输出类型。
    ///
    /// 如果输出格式不是 `Summary`，则返回 `None`。
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

    /// 返回用于并行性的线程数。
    fn threads(&self) -> Result<usize> {
        if self.sort_by()?.kind != SortByKind::None {
            return Ok(1);
        }
        let threads = self.usize_of("threads")?.unwrap_or(0);
        let available =
            std::thread::available_parallelism().map_or(1, |n| n.get());
        Ok(if threads == 0 { cmp::min(12, available) } else { threads })
    }

    /// 从命令行标志构建文件类型匹配器。
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

    /// 返回 "unrestricted" 标志被提供的次数。
    fn unrestricted_count(&self) -> u64 {
        self.occurrences_of("unrestricted")
    }

    /// 返回 true 当且仅当应启用 Unicode 模式。
    fn unicode(&self) -> bool {
        // 默认情况下启用 Unicode 模式，因此仅当明确使用 --no-unicode 标志时才禁用它。
        !(self.is_present("no-unicode") || self.is_present("no-pcre2-unicode"))
    }

    /// 返回 true 当且仅当应该发出包含每个匹配的文件名。
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
/// 大部分以下方法直接调度到底层的 clap 方法。对于本应获取单个值的方法，将获取所有值并返回最后一个值。 (Clap 返回第一个值。)
/// 我们只定义所需的方法。
impl ArgMatches {
    /// 检查命令行是否存在给定名称的标志。
    fn is_present(&self, name: &str) -> bool {
        self.0.is_present(name)
    }

    /// 返回给定名称的标志出现次数。
    fn occurrences_of(&self, name: &str) -> u64 {
        self.0.occurrences_of(name)
    }

    /// 返回给定名称的标志的值作为字符串，使用损失y进行转换。
    fn value_of_lossy(&self, name: &str) -> Option<String> {
        self.0.value_of_lossy(name).map(|s| s.into_owned())
    }

    /// 返回给定名称的标志的值作为字符串向量，使用损失y进行转换。
    fn values_of_lossy(&self, name: &str) -> Option<Vec<String>> {
        self.0.values_of_lossy(name)
    }

    /// 返回给定名称的标志的值作为 OsStr。
    fn value_of_os(&self, name: &str) -> Option<&OsStr> {
        self.0.value_of_os(name)
    }

    /// 返回给定名称的标志的值作为 OsStr 值的迭代器。
    fn values_of_os(&self, name: &str) -> Option<clap::OsValues<'_>> {
        self.0.values_of_os(name)
    }
}

/// 检查由构建 Rust 正则表达式匹配器引发的错误，如果认为它对应于另一个引擎可以处理的语法错误，则添加一条建议使用引擎标志的消息。
fn suggest(msg: String) -> String {
    if let Some(pcre_msg) = suggest_pcre2(&msg) {
        return pcre_msg;
    }
    msg
}

/// 检查由构建 Rust 正则表达式匹配器引发的错误，如果认为它对应于 PCRE2 可以处理的语法错误，则添加一条消息以建议使用 -P/--pcre2。
fn suggest_pcre2(msg: &str) -> Option<String> {
    #[cfg(feature = "pcre2")]
    fn suggest(msg: &str) -> Option<String> {
        if !msg.contains("backreferences") && !msg.contains("look-around") {
            None
        } else {
            Some(format!(
                "{}

考虑使用 --pcre2 标志启用 PCRE2，它可以处理反向引用和环视。",
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

/// 在构建 Rust 正则表达式匹配器引发错误后进行检查，如果认为它对应于 PCRE2 可以处理的多行语法错误，则添加一条消息以建议使用 --multiline 标志（或简写的 -U）启用多行模式。
fn suggest_multiline(msg: String) -> String {
    if msg.contains("the literal") && msg.contains("not allowed") {
        format!(
            "{}

考虑使用 --multiline 标志（或简写的 -U）启用多行模式。启用多行模式后，可以匹配换行字符。",
            msg
        )
    } else {
        msg
    }
}

/// 将解析可读文件大小的结果转换为 `usize`，如果类型不适合，则失败。
fn u64_to_usize(arg_name: &str, value: Option<u64>) -> Result<Option<usize>> {
    use std::usize;

    let value = match value {
        None => return Ok(None),
        Some(value) => value,
    };
    if value <= usize::MAX as u64 {
        Ok(Some(value as usize))
    } else {
        Err(From::from(format!("数字对于 {} 来说过大", arg_name)))
    }
}

/// 根据可选参数进行排序。
//
/// 如果发现参数为 `None`，则两个条目比较相等。
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

/// 如果给定的参数成功解析，则返回 clap 匹配对象。
///
/// 否则，如果发生错误，则将其返回，除非错误对应于 `--help` 或 `--version` 请求。在这种情况下，打印相应的输出并成功退出当前进程。
fn clap_matches<I, T>(args: I) -> Result<clap::ArgMatches<'static>>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let err = match app::app().get_matches_from_safe(args) {
        Ok(matches) => return Ok(matches),
        Err(err) => err,
    };
    if err.use_stderr() {
        return Err(err.into());
    }
    // 显式地忽略 write! 返回的任何错误。此时最可能的错误是断开的管道错误，在这种情况下，我们希望忽略它并静默退出。
    //
    // (这是这个辅助函数的目的。clap 在处理这种情况的功能上会引发 panic。)
    let _ = write!(io::stdout(), "{}", err);
    process::exit(0);
}

/// 尝试发现当前工作目录。这主要只是委托给标准库，但是如果 ripgrep 在一个不再存在的目录中，则这些操作将失败。我们尝试一些后备机制，例如查询 PWD 环境变量，但否则返回错误。
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
    Err(format!("无法获取当前工作目录：{} --- 您的 CWD 是否被删除？", err,)
        .into())
}

/// 尝试为每个 `Subject` 向量中的 `Subject` 分配一个时间戳，以帮助排序 Subjects 按时间排序。
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
