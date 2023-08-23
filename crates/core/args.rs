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
        let color: ColorChoice = self.matches().color_choice();
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
/// 'ArgMatches'封装了'clap::ArgMatches'，并为解析后的参数提供了语义含义。
#[derive(Clone, Debug)]
struct ArgMatches(clap::ArgMatches<'static>);

/// 输出格式。通常，这与ripgrep用于显示搜索结果的打印机对应。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputKind {
    /// 类似于经典grep或ack格式。
    Standard,
    /// 显示匹配的文件以及可能在每个文件中的匹配数量。
    Summary,
    /// 以JSON Lines格式发出匹配信息。
    JSON,
}

/// 排序标准，如果存在的话。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SortBy {
    /// 是否反转排序标准（即，降序）。
    reverse: bool,
    /// 实际的排序标准。
    kind: SortByKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SortByKind {
    /// 根本不排序。
    None,
    /// 按路径排序。
    Path,
    /// 按上次修改时间排序。
    LastModified,
    /// 按上次访问时间排序。
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

    /// 尝试检查所选的排序标准是否实际支持。
    /// 如果不支持，则返回错误。
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
    /// 特别地，涉及`stat`调用的排序不会被加载，因为步行阶段本质上假设父目录了解其所有子项的属性，但`stat`并不是这样工作的。
    fn configure_builder_sort(self, builder: &mut WalkBuilder) {
        use SortByKind::*;
        match self.kind {
            Path if self.reverse => {
                builder.sort_by_file_name(|a, b| a.cmp(b).reverse());
            }
            Path => {
                builder.sort_by_file_name(|a, b| a.cmp(b));
            }
            // 这些使用`stat`调用，将在Args::sort_by_stat()中排序
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
    /// 强制使用显式编码，但BOM嗅探会覆盖它。
    Some(Encoding),
    /// 仅使用BOM嗅探进行自动检测编码。
    Auto,
    /// 不使用显式编码并禁用所有BOM嗅探。这将始终导致搜索原始字节，而不考虑其实际编码。
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
/// 将命令行参数转换为ripgrep使用的各种数据结构的高级方法。
///
/// 方法按字母顺序排序。
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

    /// 返回应该用于使用引擎的搜索的匹配器。
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
            "pcre2" => Err(From::from("在此构建的ripgrep中不可用PCRE2")),
            "auto" => {
                let rust_err = match self.matcher_rust(patterns) {
                    Ok(matcher) => {
                        return Ok(PatternMatcher::RustRegex(matcher));
                    }
                    Err(err) => err,
                };
                log::debug!(
                    "在混合模式下构建Rust正则表达式时出错:\n{}",
                    rust_err,
                );

                let pcre_err = match self.matcher_engine("pcre2", patterns) {
                    Ok(matcher) => return Ok(matcher),
                    Err(err) => err,
                };
                Err(From::from(format!(
                    "无法使用默认正则引擎或PCRE2编译正则表达式。\n\n\
                     默认正则引擎错误:\n{}\n{}\n{}\n\n\
                     PCRE2正则引擎错误:\n{}",
                    "~".repeat(79),
                    rust_err,
                    "~".repeat(79),
                    pcre_err,
                )))
            }
            _ => Err(From::from(format!("不识别的正则引擎'{}'", engine))),
        }
    }

    /// 使用Rust的正则引擎构建匹配器。
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
            // 此外，在使用--null-data的情况下，使用与原始字节匹配的多行正则表达式，
            // 而这通常会被禁止。
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

    /// 使用PCRE2构建匹配器。
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
        // 不知何故，JIT在32位系统上的正则表达式编译过程中会出现“没有更多内存”的错误。因此在那里不要使用它。
        if cfg!(target_pointer_width = "64") {
            builder
                .jit_if_available(true)
                // PCRE2文档中说32KB是默认值，而1MB对于任何情况都足够了。但我们将其增加到10MB。
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

    /// 构建一个将结果写入给定写入器的JSON打印机。
    fn printer_json<W: io::Write>(&self, wtr: W) -> Result<JSON<W>> {
        let mut builder = JSONBuilder::new();
        builder
            .pretty(false)
            .max_matches(self.max_count()?)
            .always_begin_end(false);
        Ok(builder.build(wtr))
    }

    /// 构建一个将结果写入给定写入器的标准输出打印机。
    ///
    /// 给定的路径用于配置打印机的各个方面。
    ///
    /// 如果`separator_search`为true，则返回的打印机将在适当时（例如启用上下文时）承担在每组搜索结果之间打印分隔符的责任。
    /// 当设置为false时，调用者负责处理分隔符。
    ///
    /// 在实际情况中，我们希望单线程情况下打印机处理，但多线程情况下不处理。
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

    /// 构建一个将结果写入给定写入器的汇总输出打印机。
    ///
    /// 给定的路径用于配置打印机的各个方面。
    ///
    /// 如果输出格式不是`OutputKind::Summary`，则此处会panic。
    fn printer_summary<W: WriteColor>(
        &self,
        paths: &[PathBuf],
        wtr: W,
    ) -> Result<Summary<W>> {
        let mut builder = SummaryBuilder::new();
        builder
            .kind(self.summary_kind().expect("汇总格式"))
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

    /// 从命令行参数构建搜索器。
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

/// 用于将命令行参数转换为各种类型数据结构的中层例程。
///
/// 方法按字母顺序排序。
impl ArgMatches {
    /// 返回对通过递归目录遍历隐式搜索的文件执行的二进制检测形式。
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

    /// 返回对通过用户在特定文件、文件或 stdin 上调用 ripgrep 时显式搜索的文件执行的二进制检测形式。
    ///
    /// 一般来说，这不应该是 BinaryDetection::quit，因为那 acts
    /// 作为过滤器（但在看到 NUL 字节后立即退出），我们不应该过滤掉用户想要显式搜索的文件。
    fn binary_detection_explicit(&self) -> BinaryDetection {
        let none = self.is_present("text") || self.is_present("null-data");
        if none {
            BinaryDetection::none()
        } else {
            BinaryDetection::convert(b'\x00')
        }
    }

    /// 如果命令行配置意味着永远无法显示匹配，则返回 true。
    fn can_never_match(&self, patterns: &[String]) -> bool {
        patterns.is_empty() || self.max_count().ok() == Some(Some(0))
    }

    /// 当且仅当应忽略大小写时，返回 true。
    ///
    /// 如果存在 --case-sensitive，则永远不会忽略大小写，即使存在 --ignore-case。
    fn case_insensitive(&self) -> bool {
        self.is_present("ignore-case") && !self.is_present("case-sensitive")
    }

    /// 当且仅当启用智能大小写时，返回 true。
    ///
    /// 如果存在 --ignore-case 或 --case-sensitive，则禁用智能大小写。
    fn case_smart(&self) -> bool {
        self.is_present("smart-case")
            && !self.is_present("ignore-case")
            && !self.is_present("case-sensitive")
    }

    /// 根据命令行参数和环境返回用户的颜色选择。
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

    /// 返回用户在 CLI 上指定的颜色规范。
    ///
    /// 如果解析所提供规范时出现问题，则返回错误。
    fn color_specs(&self) -> Result<ColorSpecs> {
        // 使用默认的颜色规范集。
        let mut specs = default_color_specs();
        for spec_str in self.values_of_lossy_vec("colors") {
            specs.push(spec_str.parse()?);
        }
        Ok(ColorSpecs::new(&specs))
    }

    /// 当且仅当应显示列号时，返回 true。
    fn column(&self) -> bool {
        if self.is_present("no-column") {
            return false;
        }
        self.is_present("column") || self.is_present("vimgrep")
    }

    /// 返回命令行中的 before 和 after 上下文。
    ///
    /// 如果上下文设置不存在，则返回 `0`。
    ///
    /// 如果出现问题，无法将用户的值解析为整数，则返回错误。
    fn contexts(&self) -> Result<(usize, usize)> {
        let both = self.usize_of("context")?.unwrap_or(0);
        let after = self.usize_of("after-context")?.unwrap_or(both);
        let before = self.usize_of("before-context")?.unwrap_or(both);
        Ok((before, after))
    }

    /// 返回未转义的上下文分隔符的 UTF-8 字节。
    ///
    /// 如果未提供分隔符，则返回默认值 `--`。
    /// 如果传递了 --no-context-separator，则返回 None。
    fn context_separator(&self) -> Option<Vec<u8>> {
        let nosep = self.is_present("no-context-separator");
        let sep = self.value_of_os("context-separator");
        match (nosep, sep) {
            (true, _) => None,
            (false, None) => Some(b"--".to_vec()),
            (false, Some(sep)) => Some(cli::unescape_os(&sep)),
        }
    }

    /// 返回是否从命令行传递了 -c/--count 或 --count-matches 标志。
    ///
    /// 如果传递了 --count-matches 和 --invert-match，则像传递了 --count 和 --invert-match 一样处理
    /// （即，rg 将根据现有行为计算反向匹配的数量）。
    fn counts(&self) -> (bool, bool) {
        let count = self.is_present("count");
        let count_matches = self.is_present("count-matches");
        let invert_matches = self.is_present("invert-match");
        let only_matching = self.is_present("only-matching");
        if count_matches && invert_matches {
            // 将 `-v --count-matches` 视为 `-v -c`。
            (true, false)
        } else if count && only_matching {
            // 将 `-c --only-matching` 视为 `--count-matches`。
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
    /// 仅在明确指定编码时才返回编码。否则，如果设置为 automatic，
    /// Searcher 将对 UTF-16 进行 BOM 嗅探和无缝转码。
    /// 如果禁用，将不进行 BOM 嗅探和转码。
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
    /// 根据 CLI 配置返回要使用的文件分隔符。
    fn file_separator(&self) -> Result<Option<Vec<u8>>> {
        // 文件分隔符仅用于标准的 grep-line 格式。
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

    /// 当且仅当匹配应与文件名标题分组时，返回 true。
    fn heading(&self) -> bool {
        if self.is_present("no-heading") || self.is_present("vimgrep") {
            false
        } else {
            cli::is_tty_stdout()
                || self.is_present("heading")
                || self.is_present("pretty")
        }
    }

    /// 当且仅当应搜索隐藏文件/目录时，返回 true。
    fn hidden(&self) -> bool {
        self.is_present("hidden") || self.unrestricted_count() >= 2
    }

    /// 当且仅当应在处理 ignore 文件时忽略大小写时，返回 true。
    fn ignore_file_case_insensitive(&self) -> bool {
        self.is_present("ignore-file-case-insensitive")
    }

    /// 返回命令行中给定的所有 ignore 文件路径。
    fn ignore_paths(&self) -> Vec<PathBuf> {
        let paths = match self.values_of_os("ignore-file") {
            None => return vec![],
            Some(paths) => paths,
        };
        paths.map(|p| Path::new(p).to_path_buf()).collect()
    }

    /// 当且仅当 ripgrep 以确切搜索一个内容的方式调用时，返回 true。
    fn is_one_search(&self, paths: &[PathBuf]) -> bool {
        if paths.len() != 1 {
            return false;
        }
        self.is_only_stdin(paths) || paths[0].is_file()
    }

    /// 当且仅当我们仅搜索单个内容且该内容为 stdin 时，返回 true。
    fn is_only_stdin(&self, paths: &[PathBuf]) -> bool {
        paths == [Path::new("-")]
    }

    /// 当且仅当我们应该显示行号时，返回 true。
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

        // 有几种情况可以暗示计数行号。特别是，通常在打印到终端以供人类使用时，默认情况下会显示行号，
        // 除了一个有趣的情况：当仅搜索 stdin 时。这使得管道按预期工作。
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

    /// 当且仅当超过最大列限制的行应显示预览时，返回 true。
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
        // 安全性：以便以一种不会完全否定使用内存映射好处的可移植方式安全地封装内存映射
        // 实际上是很难的。对于 ripgrep 的用途，我们永远不会修改内存映射，
        // 并且通常不会将内存映射的内容存储在依赖于不可变性的数据结构中。
        // 一般来说，最糟糕的情况就是会发生 SIGBUS（如果在读取时底层文件被截断），
        // 这将导致 ripgrep 中止。这种推理应被视为可疑的。
        let maybe = unsafe { MmapChoice::auto() };
        let never = MmapChoice::never();
        if self.is_present("no-mmap") {
            never
        } else if self.is_present("mmap") {
            maybe
        } else if paths.len() <= 10 && paths.iter().all(|p| p.is_file()) {
            // 如果只搜索了少数路径，并且所有路径都是文件，则内存映射可能更快。
            maybe
        } else {
            never
        }
    }

    /// 返回是否应忽略 ignore 文件。
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

    /// 返回是否应忽略显式给定的 ignore 文件。
    fn no_ignore_files(&self) -> bool {
        // 在这里不看 no-ignore，因为 --no-ignore 明确地不覆盖 --ignore-file。
        self.is_present("no-ignore-files")
    }

    /// 返回是否应忽略全局 ignore 文件。
    fn no_ignore_global(&self) -> bool {
        self.is_present("no-ignore-global") || self.no_ignore()
    }

    /// 返回是否应忽略父级 ignore 文件。
    fn no_ignore_parent(&self) -> bool {
        self.is_present("no-ignore-parent") || self.no_ignore()
    }

    /// 返回是否应忽略 VCS ignore 文件。
    fn no_ignore_vcs(&self) -> bool {
        self.is_present("no-ignore-vcs") || self.no_ignore()
    }

    /// 确定我们应生成的输出类型。
    fn output_kind(&self) -> OutputKind {
        if self.is_present("quiet") {
            // 虽然在安静模式下，我们从技术上不会打印结果（或聚合结果），
            // 但我们仍然支持 --stats 标志，而这些统计数据目前是由 Summary 打印机计算的。
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

    /// 从命令行标志构建 glob 重写集。
    fn overrides(&self) -> Result<Override> {
        let globs = self.values_of_lossy_vec("glob");
        let iglobs = self.values_of_lossy_vec("iglob");
        if globs.is_empty() && iglobs.is_empty() {
            return Ok(Override::empty());
        }

        let mut builder = OverrideBuilder::new(current_dir()?);
        // 通过 --glob-case-insensitive 让所有 glob 不区分大小写。
        if self.is_present("glob-case-insensitive") {
            builder.case_insensitive(true).unwrap();
        }
        for glob in globs {
            builder.add(&glob)?;
        }
        // 这仅为随后的 glob 启用不区分大小写。
        builder.case_insensitive(true).unwrap();
        for glob in iglobs {
            builder.add(&glob)?;
        }
        Ok(builder.build()?)
    }
    /// 返回 ripgrep 应搜索的所有文件路径。
    ///
    /// 如果没有给出路径，则返回一个空列表。
    fn paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = match self.values_of_os("path") {
            None => vec![],
            Some(paths) => paths.map(|p| Path::new(p).to_path_buf()).collect(),
        };
        // 如果给出了 --file、--files 或 --regexp，则第一个路径始终在 `pattern` 中。
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

    /// 返回 ripgrep 应搜索的默认路径。仅在 ripgrep 未以其他方式至少作为一个文件路径
    /// 给出为位置参数时才应使用此方法。
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

    /// 返回未转义的路径分隔符作为单个字节，如果存在的话。
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
                "路径分隔符必须正好为一个字节，但给定的分隔符为 {} 字节：{}\n\
                 在 Windows 的某些 shell 中，“/”会自动展开。请使用“//”代替。",
                sep.len(),
                cli::escape(&sep),
            )))
        } else {
            Ok(Some(sep[0]))
        }
    }

    /// 返回应该用于终止路径的字节。
    ///
    /// 通常情况下，只有在提供了 --null 标志时，这才会设置为 `\x00`，否则为 `None`。
    fn path_terminator(&self) -> Option<u8> {
        if self.is_present("null") {
            Some(b'\x00')
        } else {
            None
        }
    }

    /// 返回未转义的字段上下文分隔符。如果未指定分隔符，则使用 '-' 作为默认值。
    fn field_context_separator(&self) -> Vec<u8> {
        match self.value_of_os("field-context-separator") {
            None => b"-".to_vec(),
            Some(sep) => cli::unescape_os(&sep),
        }
    }

    /// 返回未转义的字段匹配分隔符。如果未指定分隔符，则使用 ':' 作为默认值。
    fn field_match_separator(&self) -> Vec<u8> {
        match self.value_of_os("field-match-separator") {
            None => b":".to_vec(),
            Some(sep) => cli::unescape_os(&sep),
        }
    }

    /// 从命令行获取所有可用的模式序列。这包括读取 -e/--regexp 和 -f/--file 标志。
    ///
    /// 如果任何模式无效的 UTF-8，则返回错误。
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

    /// 将 OsStr 模式转换为 String 模式。如果设置了 -F/--fixed-strings，则模式将被转义。
    ///
    /// 如果模式无效的 UTF-8，则返回错误。
    fn pattern_from_os_str(&self, pat: &OsStr) -> Result<String> {
        let s = cli::pattern_from_os(pat)?;
        Ok(self.pattern_from_str(s))
    }

    /// 将 &str 模式转换为 String 模式。如果设置了 -F/--fixed-strings，则模式将被转义。
    fn pattern_from_str(&self, pat: &str) -> String {
        self.pattern_from_string(pat.to_string())
    }

    /// 如果需要，对给定模式进行附加处理（例如转义元字符或将其转换为行正则表达式）。
    fn pattern_from_string(&self, pat: String) -> String {
        if pat.is_empty() {
            // 正常情况下这只是一个空字符串，可以单独使用，但是如果模式在交替集合中连接起来，
            // 那么最终会得到 `foo|`，这在 Rust 的正则表达式引擎中当前是无效的。
            "(?:)".to_string()
        } else {
            pat
        }
    }

    /// 如果指定了预处理器命令，则返回该命令。
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

    /// 构建用于过滤文件的 glob 集，以应用于 --pre 标志。
    /// 如果没有可用的 --pre-globs，则始终返回一个空的 glob 集。
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

    /// 如果存在，以 UTF-8 字节形式返回替换字符串。
    fn replacement(&self) -> Option<Vec<u8>> {
        self.value_of_lossy("replace").map(|s| s.into_bytes())
    }

    /// 基于命令行参数返回排序条件。
    fn sort_by(&self) -> Result<SortBy> {
        // 为了向后兼容，继续支持不推荐使用的 --sort-files 标志。
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
    /// 通常情况下，仅当通过命令行参数显式请求，通过 --stats 标志启用，但这也可以通过输出格式隐式启用，例如 JSON Lines。
    fn stats(&self) -> bool {
        self.output_kind() == OutputKind::JSON || self.is_present("stats")
    }

    /// 当输出格式为 `Summary` 时，返回摘要输出类型。
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

    /// 返回 `unrestricted` 标志提供的次数。
    fn unrestricted_count(&self) -> u64 {
        self.occurrences_of("unrestricted")
    }

    /// 返回 true 当且仅当应启用 Unicode 模式。
    fn unicode(&self) -> bool {
        // Unicode 模式默认已启用，因此仅在显式提供 --no-unicode 时禁用它。
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
/// 用于从 clap 中提取值的较低级通用辅助方法。
impl ArgMatches {
    /// 类似于 values_of_lossy，但如果标志不存在，则返回一个空的向量。
    fn values_of_lossy_vec(&self, name: &str) -> Vec<String> {
        self.values_of_lossy(name).unwrap_or_else(Vec::new)
    }

    /// 安全地读取具有给定名称的参数值，如果存在，则尝试将其解析为 usize 值。
    ///
    /// 如果数字为零，则视为不存在，返回 `None`。
    fn usize_of_nonzero(&self, name: &str) -> Result<Option<usize>> {
        let n = match self.usize_of(name)? {
            None => return Ok(None),
            Some(n) => n,
        };
        Ok(if n == 0 { None } else { Some(n) })
    }

    /// 安全地读取具有给定名称的参数值，如果存在，则尝试将其解析为 usize 值。
    fn usize_of(&self, name: &str) -> Result<Option<usize>> {
        match self.value_of_lossy(name) {
            None => Ok(None),
            Some(v) => v.parse().map(Some).map_err(From::from),
        }
    }

    /// 解析格式为 `[0-9]+(KMG)?` 的参数。
    ///
    /// 如果未识别上述格式，则返回错误。
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

/// 以下方法大多直接分派给底层 clap 方法。将只获取单个值的方法会获取所有值，并返回最后一个值。
/// （Clap 返回第一个值。）我们只定义了需要的方法。
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

/// 检查构建 Rust 正则匹配器时产生的错误，并且如果认为该错误对应于其他引擎可以处理的语法错误，
/// 则添加一条消息以建议使用引擎标志。
fn suggest(msg: String) -> String {
    if let Some(pcre_msg) = suggest_pcre2(&msg) {
        return pcre_msg;
    }
    msg
}

/// 检查构建 Rust 正则匹配器时产生的错误，并且如果认为该错误对应于 PCRE2 可以处理的语法错误，
/// 则添加一条消息以建议使用 -P/--pcre2。
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

fn suggest_multiline(msg: String) -> String {
    if msg.contains("the literal") && msg.contains("not allowed") {
        format!(
            "{}

考虑使用 --multiline 标志（或 -U 简写）启用多行模式。
启用多行模式后，可以匹配换行字符。",
            msg
        )
    } else {
        msg
    }
}

/// 将解析人类可读文件大小的结果转换为 `usize`，如果类型不适合，则失败。
fn u64_to_usize(arg_name: &str, value: Option<u64>) -> Result<Option<usize>> {
    use std::usize;

    let value = match value {
        None => return Ok(None),
        Some(value) => value,
    };
    if value <= usize::MAX as u64 {
        Ok(Some(value as usize))
    } else {
        Err(From::from(format!("数值过大，超出了 {}", arg_name)))
    }
}

/// 根据可选参数进行排序。
//
/// 如果找不到参数，两个条目都视为相等。
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

/// 如果解析成功，则返回 clap 的匹配对象。
///
/// 否则，如果发生错误，除非错误对应于 `--help` 或 `--version` 请求，
/// 否则将返回错误。在这种情况下，将打印相应的输出，并成功退出当前进程。
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

/// 尝试发现当前工作目录。这主要是转到标准库，但是在 ripgrep 所在目录不存在的情况下，此类操作将失败。
/// 我们尝试一些备用机制，例如查询 PWD 环境变量，但否则返回错误。
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
        "无法获取当前工作目录：{} \
         --- 您的 CWD 是否已删除？",
        err,
    )
    .into())
}

/// 尝试为向量中的每个 `Subject` 分配时间戳，以帮助按时间排序主题。
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
