use std::cell::{Cell, RefCell};
use std::cmp;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use bstr::ByteSlice;
use grep_matcher::{Match, Matcher};
use grep_searcher::{
    LineStep, Searcher, Sink, SinkContext, SinkContextKind, SinkFinish,
    SinkMatch,
};
use termcolor::{ColorSpec, NoColor, WriteColor};

use crate::color::ColorSpecs;
use crate::counter::CounterWriter;
use crate::stats::Stats;
use crate::util::{
    find_iter_at_in_context, trim_ascii_prefix, trim_line_terminator,
    PrinterPath, Replacer, Sunk,
};
/// 标准打印机的配置。
///
/// 这个结构由`StandardBuilder`进行操作，然后由实际的实现进行引用。
/// 一旦构建了打印机，配置就被冻结，不能再更改。
#[derive(Debug, Clone)]
struct Config {
    colors: ColorSpecs,                      // 颜色规范配置
    stats: bool,                             // 统计信息
    heading: bool,                           // 标题行
    path: bool,                              // 路径显示
    only_matching: bool,                     // 仅显示匹配内容
    per_match: bool,                         // 每次匹配
    per_match_one_line: bool,                // 每次匹配一行
    replacement: Arc<Option<Vec<u8>>>,       // 替换内容
    max_columns: Option<u64>,                // 最大列数
    max_columns_preview: bool,               // 最大列数预览
    max_matches: Option<u64>,                // 最大匹配数
    column: bool,                            // 列显示
    byte_offset: bool,                       // 字节偏移
    trim_ascii: bool,                        // 裁剪 ASCII
    separator_search: Arc<Option<Vec<u8>>>,  // 搜索分隔符
    separator_context: Arc<Option<Vec<u8>>>, // 上下文分隔符
    separator_field_match: Arc<Vec<u8>>,     // 匹配字段分隔符
    separator_field_context: Arc<Vec<u8>>,   // 上下文字段分隔符
    separator_path: Option<u8>,              // 路径分隔符
    path_terminator: Option<u8>,             // 路径终止符
}

impl Default for Config {
    fn default() -> Config {
        Config {
            colors: ColorSpecs::default(),
            stats: false,
            heading: false,
            path: true,
            only_matching: false,
            per_match: false,
            per_match_one_line: false,
            replacement: Arc::new(None),
            max_columns: None,
            max_columns_preview: false,
            max_matches: None,
            column: false,
            byte_offset: false,
            trim_ascii: false,
            separator_search: Arc::new(None),
            separator_context: Arc::new(Some(b"--".to_vec())),
            separator_field_match: Arc::new(b":".to_vec()),
            separator_field_context: Arc::new(b"-".to_vec()),
            separator_path: None,
            path_terminator: None,
        }
    }
}

/// "标准"类似于grep的打印机的构建器。
///
/// 构建器允许配置打印机的行为。可配置的行为包括但不限于限制匹配数、调整分隔符、执行模式替换、记录统计信息和设置颜色。
///
/// 某些配置选项，例如行号或上下文行的显示，直接从`grep_searcher::Searcher`的配置中获取。
///
/// 一旦构建了`Standard`打印机，其配置就不能再更改。
#[derive(Clone, Debug)]
pub struct StandardBuilder {
    config: Config, // 配置
}

impl StandardBuilder {
    /// 返回一个新的构建器，用于配置标准打印机。
    pub fn new() -> StandardBuilder {
        StandardBuilder { config: Config::default() }
    }

    /// 使用任何`termcolor::WriteColor`的实现构建一个打印机。
    ///
    /// 此处使用的`WriteColor`实现控制在使用`color_specs`方法配置颜色时是否使用颜色。
    ///
    /// 为了最大程度的可移植性，调用者通常应在适当的情况下使用`termcolor::StandardStream`或`termcolor::BufferedStandardStream`，
    /// 在Windows上自动启用颜色。
    ///
    /// 然而，调用者还可以使用`termcolor::Ansi`或`termcolor::NoColor`包装器提供任意的写入器，它们分别始终通过ANSI转义启用颜色或始终禁用颜色。
    ///
    /// 作为方便起见，调用者可以使用`build_no_color`来自动选择`termcolor::NoColor`包装器，以避免需要显式从`termcolor`导入。
    pub fn build<W: WriteColor>(&self, wtr: W) -> Standard<W> {
        Standard {
            config: self.config.clone(),
            wtr: RefCell::new(CounterWriter::new(wtr)),
            matches: vec![],
        }
    }

    /// 从任何`io::Write`的实现构建一个打印机，并永远不会发出任何颜色，无论用户的颜色规范设置如何。
    ///
    /// 这是一个方便的例程，用于`StandardBuilder::build(termcolor::NoColor::new(wtr))`。
    pub fn build_no_color<W: io::Write>(
        &self,
        wtr: W,
    ) -> Standard<NoColor<W>> {
        self.build(NoColor::new(wtr))
    }

    /// 设置用于在此打印机中着色的用户颜色规范。
    ///
    /// [`UserColorSpec`](struct.UserColorSpec.html)可以根据颜色规范格式的字符串构建。
    /// 有关格式的详细信息，请参阅`UserColorSpec`类型的文档。
    /// 然后可以从零个或多个`UserColorSpec`生成[`ColorSpecs`](struct.ColorSpecs.html)。
    ///
    /// 无论此处提供了哪些颜色规范，是否实际使用颜色取决于提供给`build`的`WriteColor`实现。
    /// 例如，如果将`termcolor::NoColor`提供给`build`，则无论此处提供的颜色规范如何，都不会打印颜色。
    ///
    /// 这将完全覆盖先前的颜色规范。这不会添加到此构建器上先前提供的任何颜色规范。
    pub fn color_specs(&mut self, specs: ColorSpecs) -> &mut StandardBuilder {
        self.config.colors = specs;
        self
    }

    /// 启用对各种聚合统计信息的收集。
    ///
    /// 当启用此选项时（默认情况下禁用），将为`build`返回的所有`Standard`打印机的使用收集统计信息，
    /// 包括但不限于总匹配数、总搜索字节数和总打印字节数。
    ///
    /// 聚合统计信息可以通过sink的[`StandardSink::stats`](struct.StandardSink.html#method.stats)方法访问。
    ///
    /// 启用此选项时，为了计算某些统计信息，此打印机可能需要额外的工作，这可能会导致搜索时间更长。
    ///
    /// 有关可用统计信息的完整说明，请参阅[`Stats`](struct.Stats.html)。
    pub fn stats(&mut self, yes: bool) -> &mut StandardBuilder {
        self.config.stats = yes;
        self
    }

    /// 启用在打印机中使用“标题”。
    ///
    /// 启用此选项时，如果向打印机提供了文件路径，则文件路径将在显示任何匹配项之前单独打印在自己的行上。
    /// 如果标题不是打印机发出的第一件事，则在标题之前打印行终止符。
    ///
    /// 默认情况下，此选项被禁用。禁用时，打印机不会显示任何标题，而是将文件路径（如果有的话）打印在与每个匹配（或上下文）行相同的行上。
    pub fn heading(&mut self, yes: bool) -> &mut StandardBuilder {
        self.config.heading = yes;
        self
    }

    /// 启用时，如果向打印机提供了路径，则路径将显示在输出中（要么作为标题，要么作为每个匹配行的前缀）。
    /// 禁用时，即使向打印机提供了路径，输出中也永远不会包含路径。
    ///
    /// 默认情况下启用此选项。
    pub fn path(&mut self, yes: bool) -> &mut StandardBuilder {
        self.config.path = yes;
        self
    }

    /// 仅打印特定匹配项，而不是包含每个匹配项的整行。
    /// 每个匹配项都在自己的行上打印。当启用多行搜索时，跨越多行的匹配项会被打印，以便仅显示每行匹配部分。
    pub fn only_matching(&mut self, yes: bool) -> &mut StandardBuilder {
        self.config.only_matching = yes;
        self
    }

    /// 为每个匹配项打印至少一行。
    ///
    /// 这类似于`only_matching`选项，不同之处在于每个匹配项都打印整行。这通常与`column`选项一起使用，
    /// 后者将在每行匹配的起始列号显示出来。
    ///
    /// 当启用多行模式时，每个匹配项都会被打印，包括匹配中的每行。与单行匹配一样，如果一行包含多个匹配项（即使只是部分匹配），
    /// 那么该行会参与每次匹配的打印，假设它是该匹配中的第一行。在多行模式下，列号仅指示匹配的起始位置。
    /// 多行匹配中的后续行始终具有列号`1`。
    ///
    /// 当匹配包含多行时，启用`per_match_one_line`将导致仅打印匹配中的每个第一行。
    pub fn per_match(&mut self, yes: bool) -> &mut StandardBuilder {
        self.config.per_match = yes;
        self
    }

    /// 当启用`per_match`时，每个匹配项最多打印一行。
    ///
    /// 默认情况下，启用`per_match`时，将打印找到的每行中的每行。然而，有时这是不可取的，例如，
    /// 当您只想要每个匹配项的一行时。
    ///
    /// 这仅适用于启用多行匹配，因为否则，匹配保证仅跨足够一行。
    ///
    /// 默认情况下禁用此选项。
    pub fn per_match_one_line(&mut self, yes: bool) -> &mut StandardBuilder {
        self.config.per_match_one_line = yes;
        self
    }

    /// 设置将用于替换找到的每个匹配项的字节。
    ///
    /// 给定的替换字节可能包含对捕获组的引用，捕获组可以是索引形式（例如，`$2`），
    /// 也可以引用原始模式中存在的命名捕获组（例如，`$foo`）。
    ///
    /// 有关完整格式的文档，请参阅`Capture`特征的
    /// [grep-printer](https://docs.rs/grep-printer) crate中的`interpolate`方法。
    pub fn replacement(
        &mut self,
        replacement: Option<Vec<u8>>,
    ) -> &mut StandardBuilder {
        self.config.replacement = Arc::new(replacement);
        self
    }

    /// 设置每行打印的最大列数。单个列根据字节定义。
    ///
    /// 如果发现的行超过此最大值，则会用指示省略了行的消息替换它。
    ///
    /// 默认情况下，不指定限制，这意味着无论有多长，每个匹配或上下文行都会被打印出来。
    pub fn max_columns(&mut self, limit: Option<u64>) -> &mut StandardBuilder {
        self.config.max_columns = limit;
        self
    }

    /// 当启用时，如果行发现超过配置的最大列限制（以字节为单位），则会打印长行的预览。
    ///
    /// 预览将对应于行的前`N`个*图形簇*，其中`N`是由`max_columns`配置的限制。
    ///
    /// 如果未设置限制，则启用此选项不会产生任何效果。
    ///
    /// 默认情况下禁用此选项。
    pub fn max_columns_preview(&mut self, yes: bool) -> &mut StandardBuilder {
        self.config.max_columns_preview = yes;
        self
    }

    /// 设置要打印的最大匹配行数。
    ///
    /// 如果启用多行搜索且匹配跨越多行，则无论其跨足够多少行，该匹配仅计数一次以强制执行此限制。
    pub fn max_matches(&mut self, limit: Option<u64>) -> &mut StandardBuilder {
        self.config.max_matches = limit;
        self
    }
    /// 打印行中第一个匹配项的列号。
    ///
    /// 此选项适用于与`per_match`一起使用，后者将为每个匹配项打印一行以及该匹配项的起始偏移量。
    ///
    /// 列号以从正在打印的行的开头的字节计算。
    ///
    /// 默认情况下，此选项被禁用。
    pub fn column(&mut self, yes: bool) -> &mut StandardBuilder {
        self.config.column = yes;
        self
    }

    /// 打印每行打印的开始处的绝对字节偏移量。
    ///
    /// 绝对字节偏移从每次搜索的开始开始，以零为基础。
    ///
    /// 如果设置了`only_matching`选项，则会打印每个匹配项开始处的绝对字节偏移量。
    pub fn byte_offset(&mut self, yes: bool) -> &mut StandardBuilder {
        self.config.byte_offset = yes;
        self
    }

    /// 启用时，在写入之前，所有行都将删除前缀ASCII空格。
    ///
    /// 默认情况下，此选项被禁用。
    pub fn trim_ascii(&mut self, yes: bool) -> &mut StandardBuilder {
        self.config.trim_ascii = yes;
        self
    }

    /// 设置在搜索结果集之间使用的分隔符。
    ///
    /// 当设置了此分隔符时，仅在前一个搜索已经打印了结果的情况下，
    /// 才会在单个搜索的结果之前立即打印在自己的行上。
    /// 实际上，这允许在搜索结果集之间显示一个分隔符，该分隔符不会出现在所有搜索结果的开头或结尾。
    ///
    /// 为了复制经典grep格式，通常将其设置为`--`（与上下文分隔符相同），当且仅当请求上下文行时，才会这样做，否则禁用。
    ///
    /// 默认情况下，此选项被禁用。
    pub fn separator_search(
        &mut self,
        sep: Option<Vec<u8>>,
    ) -> &mut StandardBuilder {
        self.config.separator_search = Arc::new(sep);
        self
    }

    /// 设置在不连续的搜索上下文运行之间使用的分隔符，
    /// 但仅当搜索器配置为报告上下文行时。
    ///
    /// 无论是否为空，分隔符始终会单独打印在自己的行上。
    ///
    /// 如果未设置分隔符，则在发生上下文中断时不会打印任何内容。
    ///
    /// 默认情况下，将其设置为`--`。
    pub fn separator_context(
        &mut self,
        sep: Option<Vec<u8>>,
    ) -> &mut StandardBuilder {
        self.config.separator_context = Arc::new(sep);
        self
    }

    /// 设置用于匹配行中发射字段之间的分隔符。
    ///
    /// 例如，当搜索器启用行号时，此打印机将在每个匹配行之前打印行号。
    /// 这里给出的字节将在行号之后但匹配行之前写入。
    ///
    /// 默认情况下，将其设置为`:`。
    pub fn separator_field_match(
        &mut self,
        sep: Vec<u8>,
    ) -> &mut StandardBuilder {
        self.config.separator_field_match = Arc::new(sep);
        self
    }

    /// 设置用于上下文行中发射字段之间的分隔符。
    ///
    /// 例如，当搜索器启用行号时，此打印机将在每个上下文行之前打印行号。
    /// 这里给出的字节将在行号之后但上下文行之前写入。
    ///
    /// 默认情况下，将其设置为`-`。
    pub fn separator_field_context(
        &mut self,
        sep: Vec<u8>,
    ) -> &mut StandardBuilder {
        self.config.separator_field_context = Arc::new(sep);
        self
    }

    /// 设置在打印文件路径时使用的路径分隔符。
    ///
    /// 当打印机配置了文件路径，并且当找到匹配项时，该文件路径将被打印（根据其他配置设置，将其作为标题或作为每个匹配项或上下文行的前缀）。
    /// 通常，通过发出文件路径来完成打印。但是，此设置提供了使用与当前环境不同的路径分隔符的能力。
    ///
    /// 此选项的典型用途是允许Windows上的cygwin用户将路径分隔符设置为`/`，而不是使用系统默认值`\`。
    pub fn separator_path(&mut self, sep: Option<u8>) -> &mut StandardBuilder {
        self.config.separator_path = sep;
        self
    }

    /// 设置路径终止符。
    ///
    /// 路径终止符是在此打印机发出的每个文件路径之后打印的字节。
    ///
    /// 如果未设置路径终止符（默认情况下），则路径将由换行符终止（对于启用`heading`时），
    /// 或者由匹配项或上下文字段分隔符终止（例如，`:`或`-`）。
    pub fn path_terminator(
        &mut self,
        terminator: Option<u8>,
    ) -> &mut StandardBuilder {
        self.config.path_terminator = terminator;
        self
    }
}
/// 标准打印机，实现类似grep的格式化，包括颜色支持。
///
/// 可以使用`Standard::new`或`Standard::new_no_color`构造函数之一创建默认打印机。
/// 然而，有许多选项可以配置此打印机的输出。这些选项可以使用[`StandardBuilder`](struct.StandardBuilder.html)进行配置。
///
/// 此类型是针对`W`进行泛型化，其中`W`表示`termcolor::WriteColor` trait的任何实现。
/// 如果不需要颜色，则可以使用`new_no_color`构造函数，或者可以使用`termcolor::NoColor`适配器来包装任何`io::Write`实现，而不启用任何颜色。
#[derive(Debug)]
pub struct Standard<W> {
    config: Config,
    wtr: RefCell<CounterWriter<W>>,
    matches: Vec<Match>,
}

impl<W: WriteColor> Standard<W> {
    /// 返回一个具有默认配置的标准打印机，将匹配项写入给定的写入器。
    ///
    /// 写入器应该是`termcolor::WriteColor` trait的实现，而不仅仅是`io::Write`的裸实现。
    /// 要使用正常的`io::Write`实现（同时牺牲颜色），可以使用`new_no_color`构造函数。
    pub fn new(wtr: W) -> Standard<W> {
        StandardBuilder::new().build(wtr)
    }
}

impl<W: io::Write> Standard<NoColor<W>> {
    /// 返回一个具有默认配置的标准打印机，将匹配项写入给定的写入器。
    ///
    /// 写入器可以是`io::Write`的任何实现。使用此构造函数，打印机将永远不会发出颜色。
    pub fn new_no_color(wtr: W) -> Standard<NoColor<W>> {
        StandardBuilder::new().build_no_color(wtr)
    }
}

impl<W: WriteColor> Standard<W> {
    /// 返回标准打印机的`Sink`实现。
    ///
    /// 这不会将打印机与文件路径关联，这意味着此实现永远不会随匹配项一起打印文件路径。
    pub fn sink<'s, M: Matcher>(
        &'s mut self,
        matcher: M,
    ) -> StandardSink<'static, 's, M, W> {
        let stats = if self.config.stats { Some(Stats::new()) } else { None };
        let needs_match_granularity = self.needs_match_granularity();
        StandardSink {
            matcher: matcher,
            standard: self,
            replacer: Replacer::new(),
            path: None,
            start_time: Instant::now(),
            match_count: 0,
            after_context_remaining: 0,
            binary_byte_offset: None,
            stats: stats,
            needs_match_granularity: needs_match_granularity,
        }
    }

    /// 返回与文件路径关联的`Sink`实现。
    ///
    /// 当打印机与路径关联时，根据其配置，它可能会打印路径以及找到的匹配项。
    pub fn sink_with_path<'p, 's, M, P>(
        &'s mut self,
        matcher: M,
        path: &'p P,
    ) -> StandardSink<'p, 's, M, W>
    where
        M: Matcher,
        P: ?Sized + AsRef<Path>,
    {
        if !self.config.path {
            return self.sink(matcher);
        }
        let stats = if self.config.stats { Some(Stats::new()) } else { None };
        let ppath = PrinterPath::with_separator(
            path.as_ref(),
            self.config.separator_path,
        );
        let needs_match_granularity = self.needs_match_granularity();
        StandardSink {
            matcher: matcher,
            standard: self,
            replacer: Replacer::new(),
            path: Some(ppath),
            start_time: Instant::now(),
            match_count: 0,
            after_context_remaining: 0,
            binary_byte_offset: None,
            stats: stats,
            needs_match_granularity: needs_match_granularity,
        }
    }

    /// 当且仅当打印机的配置要求我们在搜索器报告的行中找到每个单独的匹配项时，返回true。
    ///
    /// 我们关心这种区别，因为找到每个单独的匹配项的成本更高，因此仅在需要时才执行此操作。
    fn needs_match_granularity(&self) -> bool {
        let supports_color = self.wtr.borrow().supports_color();
        let match_colored = !self.config.colors.matched().is_none();

        // 上色需要识别每个单独的匹配项。
        (supports_color && match_colored)
        // 列特性需要找到第一个匹配项的位置。
        || self.config.column
        // 需要找到每个匹配项以执行替换。
        || self.config.replacement.is_some()
        // 每个匹配项发出一行需要找到每个匹配项。
        || self.config.per_match
        // 仅匹配项需要找到每个匹配项。
        || self.config.only_matching
        // 计算某些统计信息需要找到每个匹配项。
        || self.config.stats
    }
}

impl<W> Standard<W> {
    /// 当且仅当此打印机在先前的任何搜索期间已将至少一个字节写入基础写入器时，返回true。
    pub fn has_written(&self) -> bool {
        self.wtr.borrow().total_count() > 0
    }

    /// 返回基础写入器的可变引用。
    pub fn get_mut(&mut self) -> &mut W {
        self.wtr.get_mut().get_mut()
    }

    /// 消耗此打印机并返回基础写入器的所有权。
    pub fn into_inner(self) -> W {
        self.wtr.into_inner().into_inner()
    }
}
/// 与标准打印机关联的匹配器和可选文件路径的`Sink`实现。
///
/// 可以通过[`Standard::sink`](struct.Standard.html#method.sink)或
/// [`Standard::sink_with_path`](struct.Standard.html#method.sink_with_path)方法创建`Sink`，
/// 具体取决于是否要在打印机的输出中包含文件路径。
///
/// 构建`StandardSink`是廉价的，调用者应该为每个搜索创建一个新的`Sink`。
/// 在搜索完成后，调用者可以查询此`Sink`以获取诸如是否发生匹配或是否找到二进制数据（如果是，则其偏移量）的信息。
///
/// 此类型在几个类型参数上是泛型的：
///
/// * `'p` 指的是文件路径的生命周期，如果提供了文件路径。当未提供文件路径时，这是`'static`。
/// * `'s` 指的是此类型借用的[`Standard`](struct.Standard.html)打印机的生命周期。
/// * `M` 指的是由`grep_searcher::Searcher`使用的匹配器类型，该匹配器将结果报告给此`Sink`。
/// * `W` 指的是将打印机的输出写入的底层写入器。
#[derive(Debug)]
pub struct StandardSink<'p, 's, M: Matcher, W> {
    matcher: M,
    standard: &'s mut Standard<W>,
    replacer: Replacer<M>,
    path: Option<PrinterPath<'p>>,
    start_time: Instant,
    match_count: u64,
    after_context_remaining: u64,
    binary_byte_offset: Option<u64>,
    stats: Option<Stats>,
    needs_match_granularity: bool,
}

impl<'p, 's, M: Matcher, W: WriteColor> StandardSink<'p, 's, M, W> {
    /// 当且仅当此打印机在先前的搜索中接收到匹配时，返回true。
    ///
    /// 这不受此`Sink`之前的搜索结果的影响。
    pub fn has_match(&self) -> bool {
        self.match_count > 0
    }

    /// 返回向此`Sink`报告的总匹配次数。
    ///
    /// 这对应于在先前的搜索上调用`Sink::matched`的次数。
    ///
    /// 这不受此`Sink`之前的搜索结果的影响。
    pub fn match_count(&self) -> u64 {
        self.match_count
    }

    /// 如果在先前的搜索中找到了二进制数据，则返回发现二进制数据的偏移量。
    ///
    /// 返回的偏移量是相对于搜索的所有字节的绝对偏移量。
    ///
    /// 这不受此`Sink`之前的搜索结果的影响。例如，如果先前的搜索在之前的搜索中找到了二进制数据，但先前的搜索未找到二进制数据，则返回`None`。
    pub fn binary_byte_offset(&self) -> Option<u64> {
        self.binary_byte_offset
    }

    /// 返回对由打印机为此`Sink`执行的所有搜索生成的统计信息的引用。
    ///
    /// 仅当通过[`StandardBuilder`](struct.StandardBuilder.html)配置请求了统计信息时，才会返回统计信息。
    pub fn stats(&self) -> Option<&Stats> {
        self.stats.as_ref()
    }

    /// 执行匹配器在给定字节上，并在当前配置需要匹配粒度时记录匹配位置。
    fn record_matches(
        &mut self,
        searcher: &Searcher,
        bytes: &[u8],
        range: std::ops::Range<usize>,
    ) -> io::Result<()> {
        self.standard.matches.clear();
        if !self.needs_match_granularity {
            return Ok(());
        }
        // 如果打印需要知道每个单独的匹配项的位置，则立即计算并存储这些位置以备后用。
        // 虽然这会为存储匹配项添加额外的复制，但我们会将其分摊到分配中，
        // 并且这极大地简化了打印逻辑，以至于我们永远不会超过一个搜索来查找匹配项（对于替换，我们还需要额外的搜索来执行实际替换）。
        let matches = &mut self.standard.matches;
        find_iter_at_in_context(
            searcher,
            &self.matcher,
            bytes,
            range.clone(),
            |m| {
                let (s, e) = (m.start() - range.start, m.end() - range.start);
                matches.push(Match::new(s, e));
                true
            },
        )?;
        // 不要报告出现在字节末尾的空匹配项。
        if !matches.is_empty()
            && matches.last().unwrap().is_empty()
            && matches.last().unwrap().start() >= range.end
        {
            matches.pop().unwrap();
        }
        Ok(())
    }

    /// 如果配置指定了替换，则执行替换，如果必要，延迟分配内存。
    ///
    /// 要访问替换的结果，请使用`replacer.replacement()`。
    fn replace(
        &mut self,
        searcher: &Searcher,
        bytes: &[u8],
        range: std::ops::Range<usize>,
    ) -> io::Result<()> {
        self.replacer.clear();
        if self.standard.config.replacement.is_some() {
            let replacement = (*self.standard.config.replacement)
                .as_ref()
                .map(|r| &*r)
                .unwrap();
            self.replacer.replace_all(
                searcher,
                &self.matcher,
                bytes,
                range,
                replacement,
            )?;
        }
        Ok(())
    }

    /// 返回true，如果此打印机应该退出。
    ///
    /// 这实现了在看到一定数量的匹配项后退出的逻辑。
    /// 在大多数情况下，逻辑是简单的，但我们必须允许所有“after”上下文行在达到限制后打印。
    fn should_quit(&self) -> bool {
        let limit = match self.standard.config.max_matches {
            None => return false,
            Some(limit) => limit,
        };
        if self.match_count < limit {
            return false;
        }
        self.after_context_remaining == 0
    }

    /// 返回当前匹配计数是否超过了配置的限制。
    /// 如果没有限制，则始终返回false。
    fn match_more_than_limit(&self) -> bool {
        let limit = match self.standard.config.max_matches {
            None => return false,
            Some(limit) => limit,
        };
        self.match_count > limit
    }
}
impl<'p, 's, M: Matcher, W: WriteColor> Sink for StandardSink<'p, 's, M, W> {
    // 定义错误类型为io::Error
    type Error = io::Error;

    // 当匹配项被找到时调用的方法
    fn matched(
        &mut self,
        searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, io::Error> {
        // 增加匹配计数
        self.match_count += 1;
        // 当超过匹配计数时，剩余的上下文行不应该被重置，而应该递减。
        // 这避免了显示比配置的限制更多的匹配项的错误。
        // 主要思想是'matched'可能在打印上下文行时再次被调用。
        // 在这种情况下，我们应该将其视为上下文行，而不是打印目的终止的匹配行。
        if self.match_more_than_limit() {
            self.after_context_remaining =
                self.after_context_remaining.saturating_sub(1);
        } else {
            self.after_context_remaining = searcher.after_context() as u64;
        }

        // 记录匹配项的位置
        self.record_matches(
            searcher,
            mat.buffer(),
            mat.bytes_range_in_buffer(),
        )?;
        // 执行替换
        self.replace(searcher, mat.buffer(), mat.bytes_range_in_buffer())?;

        // 如果启用了统计信息
        if let Some(ref mut stats) = self.stats {
            stats.add_matches(self.standard.matches.len() as u64);
            stats.add_matched_lines(mat.lines().count() as u64);
        }
        // 处理二进制数据
        if searcher.binary_detection().convert_byte().is_some() {
            if self.binary_byte_offset.is_some() {
                return Ok(false);
            }
        }

        // 调用StandardImpl中的方法，处理匹配项的打印
        StandardImpl::from_match(searcher, self, mat).sink()?;
        Ok(!self.should_quit())
    }

    // 当上下文行被找到时调用的方法
    fn context(
        &mut self,
        searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, io::Error> {
        self.standard.matches.clear();
        self.replacer.clear();

        // 处理上下文行，根据种类递减剩余的上下文行
        if ctx.kind() == &SinkContextKind::After {
            self.after_context_remaining =
                self.after_context_remaining.saturating_sub(1);
        }
        // 如果反转匹配，记录上下文行中的匹配位置
        if searcher.invert_match() {
            self.record_matches(searcher, ctx.bytes(), 0..ctx.bytes().len())?;
            self.replace(searcher, ctx.bytes(), 0..ctx.bytes().len())?;
        }
        // 处理二进制数据
        if searcher.binary_detection().convert_byte().is_some() {
            if self.binary_byte_offset.is_some() {
                return Ok(false);
            }
        }

        // 调用StandardImpl中的方法，处理上下文行的打印
        StandardImpl::from_context(searcher, self, ctx).sink()?;
        Ok(!self.should_quit())
    }

    // 当上下文分隔发生时调用的方法
    fn context_break(
        &mut self,
        searcher: &Searcher,
    ) -> Result<bool, io::Error> {
        // 调用StandardImpl中的方法，写上下文分隔符
        StandardImpl::new(searcher, self).write_context_separator()?;
        Ok(true)
    }

    // 当发现二进制数据时调用的方法
    fn binary_data(
        &mut self,
        _searcher: &Searcher,
        binary_byte_offset: u64,
    ) -> Result<bool, io::Error> {
        self.binary_byte_offset = Some(binary_byte_offset);
        Ok(true)
    }

    // 当开始新的搜索时调用的方法
    fn begin(&mut self, _searcher: &Searcher) -> Result<bool, io::Error> {
        // 重置计数器，初始化状态
        self.standard.wtr.borrow_mut().reset_count();
        self.start_time = Instant::now();
        self.match_count = 0;
        self.after_context_remaining = 0;
        self.binary_byte_offset = None;
        // 如果配置的最大匹配数为0，则返回false
        if self.standard.config.max_matches == Some(0) {
            return Ok(false);
        }
        Ok(true)
    }

    // 当搜索结束时调用的方法
    fn finish(
        &mut self,
        searcher: &Searcher,
        finish: &SinkFinish,
    ) -> Result<(), io::Error> {
        // 如果有二进制数据的偏移量，则写入相应的消息
        if let Some(offset) = self.binary_byte_offset {
            StandardImpl::new(searcher, self).write_binary_message(offset)?;
        }
        // 如果启用了统计信息
        if let Some(stats) = self.stats.as_mut() {
            stats.add_elapsed(self.start_time.elapsed());
            stats.add_searches(1);
            if self.match_count > 0 {
                stats.add_searches_with_match(1);
            }
            stats.add_bytes_searched(finish.byte_count());
            stats.add_bytes_printed(self.standard.wtr.borrow().count());
        }
        Ok(())
    }
}

/// 标准打印机的实际实现。将搜索器、`Sink`实现以及匹配信息连接在一起。
///
/// 每次报告匹配项或上下文行时都会初始化一个StandardImpl。
#[derive(Debug)]
struct StandardImpl<'a, M: Matcher, W> {
    searcher: &'a Searcher,
    sink: &'a StandardSink<'a, 'a, M, W>,
    sunk: Sunk<'a>,
    /// 如果且仅如果我们正在以彩色打印匹配项，则设置为true。
    in_color_match: Cell<bool>,
}

impl<'a, M: Matcher, W: WriteColor> StandardImpl<'a, M, W> {
    /// 将自身与搜索器捆绑在一起，并返回Sink的核心实现。
    fn new(
        searcher: &'a Searcher,
        sink: &'a StandardSink<'_, '_, M, W>,
    ) -> StandardImpl<'a, M, W> {
        StandardImpl {
            searcher: searcher,
            sink: sink,
            sunk: Sunk::empty(),
            in_color_match: Cell::new(false),
        }
    }

    /// 将自身与搜索器捆绑在一起，并返回用于处理匹配行的Sink的核心实现。
    fn from_match(
        searcher: &'a Searcher,
        sink: &'a StandardSink<'_, '_, M, W>,
        mat: &'a SinkMatch<'a>,
    ) -> StandardImpl<'a, M, W> {
        let sunk = Sunk::from_sink_match(
            mat,
            &sink.standard.matches,
            sink.replacer.replacement(),
        );
        StandardImpl { sunk: sunk, ..StandardImpl::new(searcher, sink) }
    }

    /// 将自身与搜索器捆绑在一起，并返回用于处理上下文行的Sink的核心实现。
    fn from_context(
        searcher: &'a Searcher,
        sink: &'a StandardSink<'_, '_, M, W>,
        ctx: &'a SinkContext<'a>,
    ) -> StandardImpl<'a, M, W> {
        let sunk = Sunk::from_sink_context(
            ctx,
            &sink.standard.matches,
            sink.replacer.replacement(),
        );
        StandardImpl { sunk: sunk, ..StandardImpl::new(searcher, sink) }
    }

    /// 执行打印操作，根据配置进行不同类型的打印。
    fn sink(&self) -> io::Result<()> {
        self.write_search_prelude()?;
        if self.sunk.matches().is_empty() {
            if self.multi_line() && !self.is_context() {
                self.sink_fast_multi_line()
            } else {
                self.sink_fast()
            }
        } else {
            if self.multi_line() && !self.is_context() {
                self.sink_slow_multi_line()
            } else {
                self.sink_slow()
            }
        }
    }

    /// 快速打印匹配项（限制为一行），通过避免在给定的SinkMatch中的行中检测每个单独的匹配项。
    /// 只有在配置不要求匹配粒度且搜索器不在多行模式下时才应使用此方法。
    fn sink_fast(&self) -> io::Result<()> {
        debug_assert!(self.sunk.matches().is_empty());
        debug_assert!(!self.multi_line() || self.is_context());

        self.write_prelude(
            self.sunk.absolute_byte_offset(),
            self.sunk.line_number(),
            None,
        )?;
        self.write_line(self.sunk.bytes())
    }

    /// 通过避免在给定的SinkMatch中的行中检测每个单独的匹配项，快速打印匹配项（可能跨越多行）。
    /// 只有在配置不要求匹配粒度且搜索器在多行模式下时才应使用此方法。
    fn sink_fast_multi_line(&self) -> io::Result<()> {
        debug_assert!(self.sunk.matches().is_empty());
        // 实际上，这不是使用此方法的必需不变式，
        // 但是如果我们进入此处并且禁用了多行模式，
        // 则仍应视为错误，因为我们应该使用matched_fast而不是此方法。
        debug_assert!(self.multi_line());

        let line_term = self.searcher.line_terminator().as_byte();
        let mut absolute_byte_offset = self.sunk.absolute_byte_offset();
        for (i, line) in self.sunk.lines(line_term).enumerate() {
            self.write_prelude(
                absolute_byte_offset,
                self.sunk.line_number().map(|n| n + i as u64),
                None,
            )?;
            absolute_byte_offset += line.len() as u64;

            self.write_line(line)?;
        }
        Ok(())
    }

    /// 根据打印机的配置，打印匹配行，需要查找每个单独的匹配项（例如，用于着色）。
    fn sink_slow(&self) -> io::Result<()> {
        debug_assert!(!self.sunk.matches().is_empty());
        debug_assert!(!self.multi_line() || self.is_context());

        if self.config().only_matching {
            for &m in self.sunk.matches() {
                self.write_prelude(
                    self.sunk.absolute_byte_offset() + m.start() as u64,
                    self.sunk.line_number(),
                    Some(m.start() as u64 + 1),
                )?;

                let buf = &self.sunk.bytes()[m];
                self.write_colored_line(&[Match::new(0, buf.len())], buf)?;
            }
        } else if self.config().per_match {
            for &m in self.sunk.matches() {
                self.write_prelude(
                    self.sunk.absolute_byte_offset() + m.start() as u64,
                    self.sunk.line_number(),
                    Some(m.start() as u64 + 1),
                )?;
                self.write_colored_line(&[m], self.sunk.bytes())?;
            }
        } else {
            self.write_prelude(
                self.sunk.absolute_byte_offset(),
                self.sunk.line_number(),
                Some(self.sunk.matches()[0].start() as u64 + 1),
            )?;
            self.write_colored_line(self.sunk.matches(), self.sunk.bytes())?;
        }
        Ok(())
    }

    /// 根据打印机的配置，慢速打印匹配行（可能跨越多行）。
    fn sink_slow_multi_line(&self) -> io::Result<()> {
        debug_assert!(!self.sunk.matches().is_empty());
        debug_assert!(self.multi_line());

        if self.config().only_matching {
            return self.sink_slow_multi_line_only_matching();
        } else if self.config().per_match {
            return self.sink_slow_multi_per_match();
        }

        let line_term = self.searcher.line_terminator().as_byte();
        let bytes = self.sunk.bytes();
        let matches = self.sunk.matches();
        let mut midx = 0;
        let mut count = 0;
        let mut stepper = LineStep::new(line_term, 0, bytes.len());
        while let Some((start, end)) = stepper.next(bytes) {
            let line = Match::new(start, end);
            self.write_prelude(
                self.sunk.absolute_byte_offset() + line.start() as u64,
                self.sunk.line_number().map(|n| n + count),
                Some(matches[0].start() as u64 + 1),
            )?;
            count += 1;
            if self.exceeds_max_columns(&bytes[line]) {
                self.write_exceeded_line(bytes, line, matches, &mut midx)?;
            } else {
                self.write_colored_matches(bytes, line, matches, &mut midx)?;
                self.write_line_term()?;
            }
        }
        Ok(())
    }

    /// 根据打印机的配置，以only_matching模式慢速打印跨越多行的匹配行。
    fn sink_slow_multi_line_only_matching(&self) -> io::Result<()> {
        let line_term = self.searcher.line_terminator().as_byte();
        let spec = self.config().colors.matched();
        let bytes = self.sunk.bytes();
        let matches = self.sunk.matches();
        let mut midx = 0;
        let mut count = 0;
        let mut stepper = LineStep::new(line_term, 0, bytes.len());
        while let Some((start, end)) = stepper.next(bytes) {
            let mut line = Match::new(start, end);
            self.trim_line_terminator(bytes, &mut line);
            self.trim_ascii_prefix(bytes, &mut line);
            while !line.is_empty() {
                if matches[midx].end() <= line.start() {
                    if midx + 1 < matches.len() {
                        midx += 1;
                        continue;
                    } else {
                        break;
                    }
                }
                let m = matches[midx];

                if line.start() < m.start() {
                    let upto = cmp::min(line.end(), m.start());
                    line = line.with_start(upto);
                } else {
                    let upto = cmp::min(line.end(), m.end());
                    self.write_prelude(
                        self.sunk.absolute_byte_offset() + m.start() as u64,
                        self.sunk.line_number().map(|n| n + count),
                        Some(m.start() as u64 + 1),
                    )?;

                    let this_line = line.with_end(upto);
                    line = line.with_start(upto);
                    if self.exceeds_max_columns(&bytes[this_line]) {
                        self.write_exceeded_line(
                            bytes, this_line, matches, &mut midx,
                        )?;
                    } else {
                        self.write_spec(spec, &bytes[this_line])?;
                        self.write_line_term()?;
                    }
                }
            }
            count += 1;
        }
        Ok(())
    }
    /// 在多个匹配项每行一行地慢速打印匹配行。
    fn sink_slow_multi_per_match(&self) -> io::Result<()> {
        let line_term = self.searcher.line_terminator().as_byte();
        let spec = self.config().colors.matched();
        let bytes = self.sunk.bytes();

        // 遍历每个匹配项
        for &m in self.sunk.matches() {
            let mut count = 0;
            let mut stepper = LineStep::new(line_term, 0, bytes.len());

            // 遍历每一行
            while let Some((start, end)) = stepper.next(bytes) {
                let mut line = Match::new(start, end);

                // 如果行的起始大于等于匹配项的结束，退出循环
                if line.start() >= m.end() {
                    break;
                }
                // 如果行的结束小于等于匹配项的起始，增加计数继续下一行
                else if line.end() <= m.start() {
                    count += 1;
                    continue;
                }
                // 写入匹配行的开头
                self.write_prelude(
                    self.sunk.absolute_byte_offset() + line.start() as u64,
                    self.sunk.line_number().map(|n| n + count),
                    Some(m.start().saturating_sub(line.start()) as u64 + 1),
                )?;
                count += 1;

                // 如果行超过最大列数，调用写入行超过最大列数的函数
                if self.exceeds_max_columns(&bytes[line]) {
                    self.write_exceeded_line(bytes, line, &[m], &mut 0)?;
                    continue;
                }

                // 去除行末尾的换行符和空格前缀
                self.trim_line_terminator(bytes, &mut line);
                self.trim_ascii_prefix(bytes, &mut line);

                // 遍历行中的每个字符
                while !line.is_empty() {
                    // 如果匹配项的结束在行的起始之前，直接写入行的内容
                    if m.end() <= line.start() {
                        self.write(&bytes[line])?;
                        line = line.with_start(line.end());
                    }
                    // 如果行的起始在匹配项的起始之前，写入起始到匹配项起始之间的部分
                    else if line.start() < m.start() {
                        let upto = cmp::min(line.end(), m.start());
                        self.write(&bytes[line.with_end(upto)])?;
                        line = line.with_start(upto);
                    }
                    // 如果行的起始在匹配项的起始之后，在行中写入匹配项范围内的内容
                    else {
                        let upto = cmp::min(line.end(), m.end());
                        self.write_spec(spec, &bytes[line.with_end(upto)])?;
                        line = line.with_start(upto);
                    }
                }
                // 写入行结束符
                self.write_line_term()?;
                // 当配置为每个匹配项一行时，打印第一行后就退出循环
                if self.config().per_match_one_line {
                    break;
                }
            }
        }
        Ok(())
    }

    /// 写入匹配行的开头部分，根据配置和参数可能包括文件路径、行号等。
    #[inline(always)]
    fn write_prelude(
        &self,
        absolute_byte_offset: u64,
        line_number: Option<u64>,
        column: Option<u64>,
    ) -> io::Result<()> {
        let sep = self.separator_field();

        // 如果不需要标题，写入文件路径字段
        if !self.config().heading {
            self.write_path_field(sep)?;
        }
        // 如果有行号，写入行号字段
        if let Some(n) = line_number {
            self.write_line_number(n, sep)?;
        }
        // 如果有列号，且配置允许显示列号，写入列号字段
        if let Some(n) = column {
            if self.config().column {
                self.write_column_number(n, sep)?;
            }
        }
        // 如果配置要求显示字节偏移，写入字节偏移字段
        if self.config().byte_offset {
            self.write_byte_offset(absolute_byte_offset, sep)?;
        }
        Ok(())
    }

    /// 写入一行文本。
    #[inline(always)]
    fn write_line(&self, line: &[u8]) -> io::Result<()> {
        // 如果超过了最大列数，调用写入超过最大列数的函数
        if self.exceeds_max_columns(line) {
            let range = Match::new(0, line.len());
            self.write_exceeded_line(
                line,
                range,
                self.sunk.matches(),
                &mut 0,
            )?;
        } else {
            // 写入去除空格前缀后的行内容
            self.write_trim(line)?;
            // 如果行没有换行符，写入行结束符
            if !self.has_line_terminator(line) {
                self.write_line_term()?;
            }
        }
        Ok(())
    }

    fn write_colored_line(
        &self,
        matches: &[Match],
        bytes: &[u8],
    ) -> io::Result<()> {
        // If we know we aren't going to emit color, then we can go faster.
        let spec = self.config().colors.matched();
        if !self.wtr().borrow().supports_color() || spec.is_none() {
            return self.write_line(bytes);
        }

        let line = Match::new(0, bytes.len());
        if self.exceeds_max_columns(bytes) {
            self.write_exceeded_line(bytes, line, matches, &mut 0)
        } else {
            self.write_colored_matches(bytes, line, matches, &mut 0)?;
            self.write_line_term()?;
            Ok(())
        }
    }

    /// 写入带有适当颜色的`bytes`部分，针对每个`match`在`bytes`中的匹配，从`match_index`开始。
    ///
    /// 这会处理去除前导空格，并且*永远不会*打印行结束符。如果匹配项超过了`line`所指定的范围，那么只会打印`line`内的匹配部分（如果有的话）。
    fn write_colored_matches(
        &self,
        bytes: &[u8],
        mut line: Match,
        matches: &[Match],
        match_index: &mut usize,
    ) -> io::Result<()> {
        // 去除行末尾的换行符和空格前缀
        self.trim_line_terminator(bytes, &mut line);
        self.trim_ascii_prefix(bytes, &mut line);

        // 如果没有匹配项，直接写入行内容
        if matches.is_empty() {
            self.write(&bytes[line])?;
            return Ok(());
        }

        // 遍历行中的每个字符
        while !line.is_empty() {
            // 如果当前匹配项已经在行的起始之前，切换到下一个匹配项
            if matches[*match_index].end() <= line.start() {
                if *match_index + 1 < matches.len() {
                    *match_index += 1;
                    continue;
                } else {
                    // 结束颜色匹配，写入剩余行内容
                    self.end_color_match()?;
                    self.write(&bytes[line])?;
                    break;
                }
            }

            let m = matches[*match_index];
            // 如果行的起始在当前匹配项之前，写入行的内容并结束颜色匹配
            if line.start() < m.start() {
                let upto = cmp::min(line.end(), m.start());
                self.end_color_match()?;
                self.write(&bytes[line.with_end(upto)])?;
                line = line.with_start(upto);
            }
            // 如果行的起始在当前匹配项之后，写入匹配项范围内的内容并开始颜色匹配
            else {
                let upto = cmp::min(line.end(), m.end());
                self.start_color_match()?;
                self.write(&bytes[line.with_end(upto)])?;
                line = line.with_start(upto);
            }
        }
        // 结束颜色匹配
        self.end_color_match()?;
        Ok(())
    }

    /// 写入超过最大列数的行的内容，从`bytes`中的`line`开始，使用`matches`中的匹配项，从`match_index`开始。
    fn write_exceeded_line(
        &self,
        bytes: &[u8],
        mut line: Match,
        matches: &[Match],
        match_index: &mut usize,
    ) -> io::Result<()> {
        // 如果配置为最大列数预览，仅打印指定最大列数内的内容
        if self.config().max_columns_preview {
            let original = line;
            let end = bytes[line]
                .grapheme_indices()
                .map(|(_, end, _)| end)
                .take(self.config().max_columns.unwrap_or(0) as usize)
                .last()
                .unwrap_or(0)
                + line.start();
            line = line.with_end(end);

            // 写入指定列数内的内容并调整匹配项
            self.write_colored_matches(bytes, line, matches, match_index)?;

            if matches.is_empty() {
                self.write(
                    b" [... Omit the content at the end of the long line]",
                )?;
            } else {
                // 计算剩余匹配项数量并写入省略信息
                let remaining = matches
                    .iter()
                    .filter(|m| {
                        m.start() >= line.end() && m.start() < original.end()
                    })
                    .count();
                let tense =
                    if remaining == 1 { "匹配" } else { "匹配项" };
                write!(
                    self.wtr().borrow_mut(),
                    " [... 还有{}个{}]",
                    remaining,
                    tense,
                )?;
            }
            self.write_line_term()?;
            return Ok(());
        }
        if self.sunk.original_matches().is_empty() {
            if self.is_context() {
                self.write(b"[Long context lines are omitted]")?;
            } else {
                self.write(b"[Omit long matching lines]")?;
            }
        } else {
            if self.config().only_matching {
                if self.is_context() {
                    self.write(b"[Long context lines are omitted]")?;
                } else {
                    self.write(b"[Omit long matching lines]")?;
                }
            } else {
                // 写入省略信息
                write!(
                    self.wtr().borrow_mut(),
                    "[省略行，带有{}个匹配项]",
                    self.sunk.original_matches().len(),
                )?;
            }
        }
        // 写入行结束符
        self.write_line_term()?;
        Ok(())
    }
    /// 如果此打印机与文件路径关联，那么将写入该路径到底层写入器，然后跟随行终止符。
    /// （如果设置了路径终止符，则使用该终止符代替行终止符。）
    fn write_path_line(&self) -> io::Result<()> {
        if let Some(path) = self.path() {
            // 写入文件路径，使用路径颜色，并根据配置写入终止符
            self.write_spec(self.config().colors.path(), path.as_bytes())?;
            if let Some(term) = self.config().path_terminator {
                self.write(&[term])?;
            } else {
                self.write_line_term()?;
            }
        }
        Ok(())
    }

    /// 如果此打印机与文件路径关联，那么将写入该路径到底层写入器，然后跟随给定的字段分隔符。
    /// （如果设置了路径终止符，则使用该终止符代替字段分隔符。）
    fn write_path_field(&self, field_separator: &[u8]) -> io::Result<()> {
        if let Some(path) = self.path() {
            // 写入文件路径，使用路径颜色，并根据配置写入终止符或字段分隔符
            self.write_spec(self.config().colors.path(), path.as_bytes())?;
            if let Some(term) = self.config().path_terminator {
                self.write(&[term])?;
            } else {
                self.write(field_separator)?;
            }
        }
        Ok(())
    }

    /// 写入搜索开头的内容。
    fn write_search_prelude(&self) -> io::Result<()> {
        // 检查是否已经写入了当前搜索的内容
        let this_search_written = self.wtr().borrow().count() > 0;
        if this_search_written {
            return Ok(());
        }

        // 如果以前有写入过内容，根据配置写入分隔符
        if let Some(ref sep) = *self.config().separator_search {
            let ever_written = self.wtr().borrow().total_count() > 0;
            if ever_written {
                self.write(sep)?;
                self.write_line_term()?;
            }
        }

        // 如果配置允许标题，写入文件路径行
        if self.config().heading {
            self.write_path_line()?;
        }
        Ok(())
    }

    /// 写入二进制文件警告消息。
    fn write_binary_message(&self, offset: u64) -> io::Result<()> {
        // 如果没有匹配项，不需要写入警告消息
        if self.sink.match_count == 0 {
            return Ok(());
        }

        let bin = self.searcher.binary_detection();
        if let Some(byte) = bin.quit_byte() {
            if let Some(path) = self.path() {
                // 写入文件路径，使用路径颜色，然后写入分隔符
                self.write_spec(self.config().colors.path(), path.as_bytes())?;
                self.write(b": ")?;
            }
            // 格式化二进制文件警告消息，写入底层写入器
            let remainder = format!(
            "警告：在匹配后停止搜索二进制文件（在偏移 {} 附近找到字节 {:?}）\n",
            offset,
            [byte].as_bstr(),
        );
            self.write(remainder.as_bytes())?;
        } else if let Some(byte) = bin.convert_byte() {
            if let Some(path) = self.path() {
                // 写入文件路径，使用路径颜色，然后写入分隔符
                self.write_spec(self.config().colors.path(), path.as_bytes())?;
                self.write(b": ")?;
            }
            // 格式化二进制文件匹配消息，写入底层写入器
            let remainder = format!(
                "二进制文件匹配（在偏移 {} 附近找到字节 {:?}）\n",
                offset,
                [byte].as_bstr(),
            );
            self.write(remainder.as_bytes())?;
        }
        Ok(())
    }

    /// 写入上下文分隔符。
    fn write_context_separator(&self) -> io::Result<()> {
        if let Some(ref sep) = *self.config().separator_context {
            // 根据配置写入上下文分隔符并写入行终止符
            self.write(sep)?;
            self.write_line_term()?;
        }
        Ok(())
    }

    /// 写入行号。
    fn write_line_number(
        &self,
        line_number: u64,
        field_separator: &[u8],
    ) -> io::Result<()> {
        let n = line_number.to_string();
        // 写入行号，使用行号颜色，并写入字段分隔符
        self.write_spec(self.config().colors.line(), n.as_bytes())?;
        self.write(field_separator)?;
        Ok(())
    }

    /// 写入列号。
    fn write_column_number(
        &self,
        column_number: u64,
        field_separator: &[u8],
    ) -> io::Result<()> {
        let n = column_number.to_string();
        // 写入列号，使用列号颜色，并写入字段分隔符
        self.write_spec(self.config().colors.column(), n.as_bytes())?;
        self.write(field_separator)?;
        Ok(())
    }

    /// 写入字节偏移量。
    fn write_byte_offset(
        &self,
        offset: u64,
        field_separator: &[u8],
    ) -> io::Result<()> {
        let n = offset.to_string();
        // 写入字节偏移量，使用列号颜色，并写入字段分隔符
        self.write_spec(self.config().colors.column(), n.as_bytes())?;
        self.write(field_separator)?;
        Ok(())
    }

    /// 写入行终止符。
    fn write_line_term(&self) -> io::Result<()> {
        self.write(self.searcher.line_terminator().as_bytes())
    }

    /// 根据颜色规范写入指定内容。
    fn write_spec(&self, spec: &ColorSpec, buf: &[u8]) -> io::Result<()> {
        let mut wtr = self.wtr().borrow_mut();
        // 设置颜色并写入内容，然后重置颜色
        wtr.set_color(spec)?;
        wtr.write_all(buf)?;
        wtr.reset()?;
        Ok(())
    }

    /// 开始颜色匹配。
    fn start_color_match(&self) -> io::Result<()> {
        // 如果已经在颜色匹配中，直接返回
        if self.in_color_match.get() {
            return Ok(());
        }
        // 设置匹配颜色，并标记为颜色匹配中
        self.wtr().borrow_mut().set_color(self.config().colors.matched())?;
        self.in_color_match.set(true);
        Ok(())
    }

    /// 结束颜色匹配。
    fn end_color_match(&self) -> io::Result<()> {
        // 如果不在颜色匹配中，直接返回
        if !self.in_color_match.get() {
            return Ok(());
        }
        // 重置颜色，并标记为颜色匹配结束
        self.wtr().borrow_mut().reset()?;
        self.in_color_match.set(false);
        Ok(())
    }

    /// 写入修剪后的内容。
    fn write_trim(&self, buf: &[u8]) -> io::Result<()> {
        // 如果不需要修剪，直接写入内容
        if !self.config().trim_ascii {
            return self.write(buf);
        }
        // 修剪内容并写入
        let mut range = Match::new(0, buf.len());
        self.trim_ascii_prefix(buf, &mut range);
        self.write(&buf[range])
    }

    /// 写入内容。
    fn write(&self, buf: &[u8]) -> io::Result<()> {
        self.wtr().borrow_mut().write_all(buf)
    }

    /// 修剪行终止符，更新行范围。
    fn trim_line_terminator(&self, buf: &[u8], line: &mut Match) {
        trim_line_terminator(&self.searcher, buf, line);
    }

    /// 检查内容是否包含行终止符。
    fn has_line_terminator(&self, buf: &[u8]) -> bool {
        self.searcher.line_terminator().is_suffix(buf)
    }

    /// 检查是否为上下文模式。
    fn is_context(&self) -> bool {
        self.sunk.context_kind().is_some()
    }

    /// 返回与此打印机关联的基础配置。
    fn config(&self) -> &'a Config {
        &self.sink.standard.config
    }

    /// 返回正在写入的基础写入器。
    fn wtr(&self) -> &'a RefCell<CounterWriter<W>> {
        &self.sink.standard.wtr
    }

    /// 返回与此打印机关联的路径（如果存在）。
    fn path(&self) -> Option<&'a PrinterPath<'a>> {
        self.sink.path.as_ref()
    }

    /// 根据匹配或上下文行返回相应的字段分隔符。
    fn separator_field(&self) -> &[u8] {
        if self.is_context() {
            &self.config().separator_field_context
        } else {
            &self.config().separator_field_match
        }
    }

    /// 检查给定行是否超过了设置的最大列数。
    fn exceeds_max_columns(&self, line: &[u8]) -> bool {
        self.config().max_columns.map_or(false, |m| line.len() as u64 > m)
    }

    /// 检查搜索器是否可能报告跨多行的匹配。
    ///
    /// 注意，这不仅检查搜索器是否在多行模式中，还检查匹配器是否可以跨多行匹配。
    /// 即使搜索器启用了多行模式，如果匹配器无法跨多行匹配，我们也不需要多行处理。
    fn multi_line(&self) -> bool {
        self.searcher.multi_line_with_matcher(&self.sink.matcher)
    }

    /// 修剪内容中前缀的 ASCII 空格，并更新对应的范围。
    ///
    /// 只要看到非空白字符或行终止符，就会停止修剪前缀。
    fn trim_ascii_prefix(&self, slice: &[u8], range: &mut Match) {
        if !self.config().trim_ascii {
            return;
        }
        let lineterm = self.searcher.line_terminator();
        *range = trim_ascii_prefix(lineterm, slice, *range)
    }
}

#[cfg(test)]
mod tests {
    use grep_matcher::LineTerminator;
    use grep_regex::{RegexMatcher, RegexMatcherBuilder};
    use grep_searcher::SearcherBuilder;
    use termcolor::{Ansi, NoColor};

    use super::{ColorSpecs, Standard, StandardBuilder};

    const SHERLOCK: &'static str = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
be, to a very large extent, the result of luck. Sherlock Holmes
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.\
";

    #[allow(dead_code)]
    const SHERLOCK_CRLF: &'static str = "\
For the Doctor Watsons of this world, as opposed to the Sherlock\r
Holmeses, success in the province of detective work must always\r
be, to a very large extent, the result of luck. Sherlock Holmes\r
can extract a clew from a wisp of straw or a flake of cigar ash;\r
but Doctor Watson has to have it taken out for him and dusted,\r
and exhibited clearly, with a label attached.\
";

    fn printer_contents(printer: &mut Standard<NoColor<Vec<u8>>>) -> String {
        String::from_utf8(printer.get_mut().get_ref().to_owned()).unwrap()
    }

    fn printer_contents_ansi(printer: &mut Standard<Ansi<Vec<u8>>>) -> String {
        String::from_utf8(printer.get_mut().get_ref().to_owned()).unwrap()
    }

    #[test]
    fn reports_match() {
        let matcher = RegexMatcher::new("Sherlock").unwrap();
        let mut printer = StandardBuilder::new().build(NoColor::new(vec![]));
        let mut sink = printer.sink(&matcher);
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(&matcher, SHERLOCK.as_bytes(), &mut sink)
            .unwrap();
        assert!(sink.has_match());

        let matcher = RegexMatcher::new("zzzzz").unwrap();
        let mut printer = StandardBuilder::new().build(NoColor::new(vec![]));
        let mut sink = printer.sink(&matcher);
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(&matcher, SHERLOCK.as_bytes(), &mut sink)
            .unwrap();
        assert!(!sink.has_match());
    }

    #[test]
    fn reports_binary() {
        use grep_searcher::BinaryDetection;

        let matcher = RegexMatcher::new("Sherlock").unwrap();
        let mut printer = StandardBuilder::new().build(NoColor::new(vec![]));
        let mut sink = printer.sink(&matcher);
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(&matcher, SHERLOCK.as_bytes(), &mut sink)
            .unwrap();
        assert!(sink.binary_byte_offset().is_none());

        let matcher = RegexMatcher::new(".+").unwrap();
        let mut printer = StandardBuilder::new().build(NoColor::new(vec![]));
        let mut sink = printer.sink(&matcher);
        SearcherBuilder::new()
            .line_number(false)
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .build()
            .search_reader(&matcher, &b"abc\x00"[..], &mut sink)
            .unwrap();
        assert_eq!(sink.binary_byte_offset(), Some(3));
    }

    #[test]
    fn reports_stats() {
        use std::time::Duration;

        let matcher = RegexMatcher::new("Sherlock|opposed").unwrap();
        let mut printer =
            StandardBuilder::new().stats(true).build(NoColor::new(vec![]));
        let stats = {
            let mut sink = printer.sink(&matcher);
            SearcherBuilder::new()
                .line_number(false)
                .build()
                .search_reader(&matcher, SHERLOCK.as_bytes(), &mut sink)
                .unwrap();
            sink.stats().unwrap().clone()
        };
        let buf = printer_contents(&mut printer);

        assert!(stats.elapsed() > Duration::default());
        assert_eq!(stats.searches(), 1);
        assert_eq!(stats.searches_with_match(), 1);
        assert_eq!(stats.bytes_searched(), SHERLOCK.len() as u64);
        assert_eq!(stats.bytes_printed(), buf.len() as u64);
        assert_eq!(stats.matched_lines(), 2);
        assert_eq!(stats.matches(), 3);
    }

    #[test]
    fn reports_stats_multiple() {
        use std::time::Duration;

        let matcher = RegexMatcher::new("Sherlock|opposed").unwrap();
        let mut printer =
            StandardBuilder::new().stats(true).build(NoColor::new(vec![]));
        let stats = {
            let mut sink = printer.sink(&matcher);
            SearcherBuilder::new()
                .line_number(false)
                .build()
                .search_reader(&matcher, SHERLOCK.as_bytes(), &mut sink)
                .unwrap();
            SearcherBuilder::new()
                .line_number(false)
                .build()
                .search_reader(&matcher, &b"zzzzzzzzzz"[..], &mut sink)
                .unwrap();
            SearcherBuilder::new()
                .line_number(false)
                .build()
                .search_reader(&matcher, SHERLOCK.as_bytes(), &mut sink)
                .unwrap();
            sink.stats().unwrap().clone()
        };
        let buf = printer_contents(&mut printer);

        assert!(stats.elapsed() > Duration::default());
        assert_eq!(stats.searches(), 3);
        assert_eq!(stats.searches_with_match(), 2);
        assert_eq!(stats.bytes_searched(), 10 + 2 * SHERLOCK.len() as u64);
        assert_eq!(stats.bytes_printed(), buf.len() as u64);
        assert_eq!(stats.matched_lines(), 4);
        assert_eq!(stats.matches(), 6);
    }

    #[test]
    fn context_break() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer = StandardBuilder::new()
            .separator_context(Some(b"--abc--".to_vec()))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .before_context(1)
            .after_context(1)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
--abc--
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn context_break_multiple_no_heading() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer = StandardBuilder::new()
            .separator_search(Some(b"--xyz--".to_vec()))
            .separator_context(Some(b"--abc--".to_vec()))
            .build(NoColor::new(vec![]));

        SearcherBuilder::new()
            .line_number(false)
            .before_context(1)
            .after_context(1)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();
        SearcherBuilder::new()
            .line_number(false)
            .before_context(1)
            .after_context(1)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
--abc--
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.
--xyz--
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
--abc--
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn context_break_multiple_heading() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer = StandardBuilder::new()
            .heading(true)
            .separator_search(Some(b"--xyz--".to_vec()))
            .separator_context(Some(b"--abc--".to_vec()))
            .build(NoColor::new(vec![]));

        SearcherBuilder::new()
            .line_number(false)
            .before_context(1)
            .after_context(1)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();
        SearcherBuilder::new()
            .line_number(false)
            .before_context(1)
            .after_context(1)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
--abc--
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.
--xyz--
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
--abc--
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn path() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer =
            StandardBuilder::new().path(false).build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:For the Doctor Watsons of this world, as opposed to the Sherlock
5:but Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn separator_field() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer = StandardBuilder::new()
            .separator_field_match(b"!!".to_vec())
            .separator_field_context(b"^^".to_vec())
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .before_context(1)
            .after_context(1)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
sherlock!!For the Doctor Watsons of this world, as opposed to the Sherlock
sherlock^^Holmeses, success in the province of detective work must always
--
sherlock^^can extract a clew from a wisp of straw or a flake of cigar ash;
sherlock!!but Doctor Watson has to have it taken out for him and dusted,
sherlock^^and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn separator_path() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer = StandardBuilder::new()
            .separator_path(Some(b'Z'))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink_with_path(&matcher, "books/sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
booksZsherlock:For the Doctor Watsons of this world, as opposed to the Sherlock
booksZsherlock:but Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn path_terminator() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer = StandardBuilder::new()
            .path_terminator(Some(b'Z'))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink_with_path(&matcher, "books/sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
books/sherlockZFor the Doctor Watsons of this world, as opposed to the Sherlock
books/sherlockZbut Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn heading() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer =
            StandardBuilder::new().heading(true).build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
sherlock
For the Doctor Watsons of this world, as opposed to the Sherlock
but Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn no_heading() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer =
            StandardBuilder::new().heading(false).build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
sherlock:For the Doctor Watsons of this world, as opposed to the Sherlock
sherlock:but Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn no_heading_multiple() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer =
            StandardBuilder::new().heading(false).build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let matcher = RegexMatcher::new("Sherlock").unwrap();
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
sherlock:For the Doctor Watsons of this world, as opposed to the Sherlock
sherlock:but Doctor Watson has to have it taken out for him and dusted,
sherlock:For the Doctor Watsons of this world, as opposed to the Sherlock
sherlock:be, to a very large extent, the result of luck. Sherlock Holmes
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn heading_multiple() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer =
            StandardBuilder::new().heading(true).build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let matcher = RegexMatcher::new("Sherlock").unwrap();
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink_with_path(&matcher, "sherlock"),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
sherlock
For the Doctor Watsons of this world, as opposed to the Sherlock
but Doctor Watson has to have it taken out for him and dusted,
sherlock
For the Doctor Watsons of this world, as opposed to the Sherlock
be, to a very large extent, the result of luck. Sherlock Holmes
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn trim_ascii() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer = StandardBuilder::new()
            .trim_ascii(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                "   Watson".as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
Watson
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn trim_ascii_multi_line() {
        let matcher = RegexMatcher::new("(?s:.{0})Watson").unwrap();
        let mut printer = StandardBuilder::new()
            .trim_ascii(true)
            .stats(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .multi_line(true)
            .build()
            .search_reader(
                &matcher,
                "   Watson".as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
Watson
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn trim_ascii_with_line_term() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer = StandardBuilder::new()
            .trim_ascii(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .before_context(1)
            .build()
            .search_reader(
                &matcher,
                "\n   Watson".as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1-
2:Watson
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn line_number() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer = StandardBuilder::new().build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:For the Doctor Watsons of this world, as opposed to the Sherlock
5:but Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn line_number_multi_line() {
        let matcher = RegexMatcher::new("(?s)Watson.+Watson").unwrap();
        let mut printer = StandardBuilder::new().build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .multi_line(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:For the Doctor Watsons of this world, as opposed to the Sherlock
2:Holmeses, success in the province of detective work must always
3:be, to a very large extent, the result of luck. Sherlock Holmes
4:can extract a clew from a wisp of straw or a flake of cigar ash;
5:but Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn column_number() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer =
            StandardBuilder::new().column(true).build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
16:For the Doctor Watsons of this world, as opposed to the Sherlock
12:but Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn column_number_multi_line() {
        let matcher = RegexMatcher::new("(?s)Watson.+Watson").unwrap();
        let mut printer =
            StandardBuilder::new().column(true).build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .multi_line(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
16:For the Doctor Watsons of this world, as opposed to the Sherlock
16:Holmeses, success in the province of detective work must always
16:be, to a very large extent, the result of luck. Sherlock Holmes
16:can extract a clew from a wisp of straw or a flake of cigar ash;
16:but Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn byte_offset() {
        let matcher = RegexMatcher::new("Watson").unwrap();
        let mut printer = StandardBuilder::new()
            .byte_offset(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
258:but Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn byte_offset_multi_line() {
        let matcher = RegexMatcher::new("(?s)Watson.+Watson").unwrap();
        let mut printer = StandardBuilder::new()
            .byte_offset(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .multi_line(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
0:For the Doctor Watsons of this world, as opposed to the Sherlock
65:Holmeses, success in the province of detective work must always
129:be, to a very large extent, the result of luck. Sherlock Holmes
193:can extract a clew from a wisp of straw or a flake of cigar ash;
258:but Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn max_columns() {
        let matcher = RegexMatcher::new("ash|dusted").unwrap();
        let mut printer = StandardBuilder::new()
            .max_columns(Some(63))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
[Omitted long matching line]
but Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn max_columns_preview() {
        let matcher = RegexMatcher::new("exhibited|dusted").unwrap();
        let mut printer = StandardBuilder::new()
            .max_columns(Some(46))
            .max_columns_preview(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
but Doctor Watson has to have it taken out for [... omitted end of long line]
and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn max_columns_with_count() {
        let matcher = RegexMatcher::new("cigar|ash|dusted").unwrap();
        let mut printer = StandardBuilder::new()
            .stats(true)
            .max_columns(Some(63))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
[Omitted long line with 2 matches]
but Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn max_columns_with_count_preview_no_match() {
        let matcher = RegexMatcher::new("exhibited|has to have it").unwrap();
        let mut printer = StandardBuilder::new()
            .stats(true)
            .max_columns(Some(46))
            .max_columns_preview(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
but Doctor Watson has to have it taken out for [... 0 more matches]
and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn max_columns_with_count_preview_one_match() {
        let matcher = RegexMatcher::new("exhibited|dusted").unwrap();
        let mut printer = StandardBuilder::new()
            .stats(true)
            .max_columns(Some(46))
            .max_columns_preview(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
but Doctor Watson has to have it taken out for [... 1 more match]
and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn max_columns_with_count_preview_two_matches() {
        let matcher =
            RegexMatcher::new("exhibited|dusted|has to have it").unwrap();
        let mut printer = StandardBuilder::new()
            .stats(true)
            .max_columns(Some(46))
            .max_columns_preview(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
but Doctor Watson has to have it taken out for [... 1 more match]
and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn max_columns_multi_line() {
        let matcher = RegexMatcher::new("(?s)ash.+dusted").unwrap();
        let mut printer = StandardBuilder::new()
            .max_columns(Some(63))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .multi_line(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
[Omitted long matching line]
but Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn max_columns_multi_line_preview() {
        let matcher =
            RegexMatcher::new("(?s)clew|cigar ash.+have it|exhibited")
                .unwrap();
        let mut printer = StandardBuilder::new()
            .stats(true)
            .max_columns(Some(46))
            .max_columns_preview(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .multi_line(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
can extract a clew from a wisp of straw or a f [... 1 more match]
but Doctor Watson has to have it taken out for [... 0 more matches]
and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn max_matches() {
        let matcher = RegexMatcher::new("Sherlock").unwrap();
        let mut printer = StandardBuilder::new()
            .max_matches(Some(1))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn max_matches_context() {
        // after context: 1
        let matcher = RegexMatcher::new("Doctor Watsons").unwrap();
        let mut printer = StandardBuilder::new()
            .max_matches(Some(1))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .after_context(1)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
";
        assert_eq_printed!(expected, got);

        // after context: 4
        let mut printer = StandardBuilder::new()
            .max_matches(Some(1))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .after_context(4)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
be, to a very large extent, the result of luck. Sherlock Holmes
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);

        // after context: 1, max matches: 2
        let matcher = RegexMatcher::new("Doctor Watsons|but Doctor").unwrap();
        let mut printer = StandardBuilder::new()
            .max_matches(Some(2))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .after_context(1)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
--
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);

        // after context: 4, max matches: 2
        let mut printer = StandardBuilder::new()
            .max_matches(Some(2))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .after_context(4)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
be, to a very large extent, the result of luck. Sherlock Holmes
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn max_matches_multi_line1() {
        let matcher = RegexMatcher::new("(?s:.{0})Sherlock").unwrap();
        let mut printer = StandardBuilder::new()
            .max_matches(Some(1))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .multi_line(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn max_matches_multi_line2() {
        let matcher =
            RegexMatcher::new(r"(?s)Watson.+?(Holmeses|clearly)").unwrap();
        let mut printer = StandardBuilder::new()
            .max_matches(Some(1))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .multi_line(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn only_matching() {
        let matcher = RegexMatcher::new("Doctor Watsons|Sherlock").unwrap();
        let mut printer = StandardBuilder::new()
            .only_matching(true)
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:9:Doctor Watsons
1:57:Sherlock
3:49:Sherlock
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn only_matching_multi_line1() {
        let matcher =
            RegexMatcher::new(r"(?s:.{0})(Doctor Watsons|Sherlock)").unwrap();
        let mut printer = StandardBuilder::new()
            .only_matching(true)
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .multi_line(true)
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:9:Doctor Watsons
1:57:Sherlock
3:49:Sherlock
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn only_matching_multi_line2() {
        let matcher =
            RegexMatcher::new(r"(?s)Watson.+?(Holmeses|clearly)").unwrap();
        let mut printer = StandardBuilder::new()
            .only_matching(true)
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .multi_line(true)
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:16:Watsons of this world, as opposed to the Sherlock
2:16:Holmeses
5:12:Watson has to have it taken out for him and dusted,
6:12:and exhibited clearly
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn only_matching_max_columns() {
        let matcher = RegexMatcher::new("Doctor Watsons|Sherlock").unwrap();
        let mut printer = StandardBuilder::new()
            .only_matching(true)
            .max_columns(Some(10))
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:9:[Omitted long matching line]
1:57:Sherlock
3:49:Sherlock
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn only_matching_max_columns_preview() {
        let matcher = RegexMatcher::new("Doctor Watsons|Sherlock").unwrap();
        let mut printer = StandardBuilder::new()
            .only_matching(true)
            .max_columns(Some(10))
            .max_columns_preview(true)
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:9:Doctor Wat [... 0 more matches]
1:57:Sherlock
3:49:Sherlock
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn only_matching_max_columns_multi_line1() {
        // The `(?s:.{0})` trick fools the matcher into thinking that it
        // can match across multiple lines without actually doing so. This is
        // so we can test multi-line handling in the case of a match on only
        // one line.
        let matcher =
            RegexMatcher::new(r"(?s:.{0})(Doctor Watsons|Sherlock)").unwrap();
        let mut printer = StandardBuilder::new()
            .only_matching(true)
            .max_columns(Some(10))
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .multi_line(true)
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:9:[Omitted long matching line]
1:57:Sherlock
3:49:Sherlock
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn only_matching_max_columns_preview_multi_line1() {
        // The `(?s:.{0})` trick fools the matcher into thinking that it
        // can match across multiple lines without actually doing so. This is
        // so we can test multi-line handling in the case of a match on only
        // one line.
        let matcher =
            RegexMatcher::new(r"(?s:.{0})(Doctor Watsons|Sherlock)").unwrap();
        let mut printer = StandardBuilder::new()
            .only_matching(true)
            .max_columns(Some(10))
            .max_columns_preview(true)
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .multi_line(true)
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:9:Doctor Wat [... 0 more matches]
1:57:Sherlock
3:49:Sherlock
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn only_matching_max_columns_multi_line2() {
        let matcher =
            RegexMatcher::new(r"(?s)Watson.+?(Holmeses|clearly)").unwrap();
        let mut printer = StandardBuilder::new()
            .only_matching(true)
            .max_columns(Some(50))
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .multi_line(true)
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:16:Watsons of this world, as opposed to the Sherlock
2:16:Holmeses
5:12:[Omitted long matching line]
6:12:and exhibited clearly
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn only_matching_max_columns_preview_multi_line2() {
        let matcher =
            RegexMatcher::new(r"(?s)Watson.+?(Holmeses|clearly)").unwrap();
        let mut printer = StandardBuilder::new()
            .only_matching(true)
            .max_columns(Some(50))
            .max_columns_preview(true)
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .multi_line(true)
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:16:Watsons of this world, as opposed to the Sherlock
2:16:Holmeses
5:12:Watson has to have it taken out for him and dusted [... 0 more matches]
6:12:and exhibited clearly
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn per_match() {
        let matcher = RegexMatcher::new("Doctor Watsons|Sherlock").unwrap();
        let mut printer = StandardBuilder::new()
            .per_match(true)
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:9:For the Doctor Watsons of this world, as opposed to the Sherlock
1:57:For the Doctor Watsons of this world, as opposed to the Sherlock
3:49:be, to a very large extent, the result of luck. Sherlock Holmes
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn per_match_multi_line1() {
        let matcher =
            RegexMatcher::new(r"(?s:.{0})(Doctor Watsons|Sherlock)").unwrap();
        let mut printer = StandardBuilder::new()
            .per_match(true)
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .multi_line(true)
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:9:For the Doctor Watsons of this world, as opposed to the Sherlock
1:57:For the Doctor Watsons of this world, as opposed to the Sherlock
3:49:be, to a very large extent, the result of luck. Sherlock Holmes
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn per_match_multi_line2() {
        let matcher =
            RegexMatcher::new(r"(?s)Watson.+?(Holmeses|clearly)").unwrap();
        let mut printer = StandardBuilder::new()
            .per_match(true)
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .multi_line(true)
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:16:For the Doctor Watsons of this world, as opposed to the Sherlock
2:1:Holmeses, success in the province of detective work must always
5:12:but Doctor Watson has to have it taken out for him and dusted,
6:1:and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn per_match_multi_line3() {
        let matcher =
            RegexMatcher::new(r"(?s)Watson.+?Holmeses|always.+?be").unwrap();
        let mut printer = StandardBuilder::new()
            .per_match(true)
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .multi_line(true)
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:16:For the Doctor Watsons of this world, as opposed to the Sherlock
2:1:Holmeses, success in the province of detective work must always
2:58:Holmeses, success in the province of detective work must always
3:1:be, to a very large extent, the result of luck. Sherlock Holmes
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn per_match_multi_line1_only_first_line() {
        let matcher =
            RegexMatcher::new(r"(?s:.{0})(Doctor Watsons|Sherlock)").unwrap();
        let mut printer = StandardBuilder::new()
            .per_match(true)
            .per_match_one_line(true)
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .multi_line(true)
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:9:For the Doctor Watsons of this world, as opposed to the Sherlock
1:57:For the Doctor Watsons of this world, as opposed to the Sherlock
3:49:be, to a very large extent, the result of luck. Sherlock Holmes
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn per_match_multi_line2_only_first_line() {
        let matcher =
            RegexMatcher::new(r"(?s)Watson.+?(Holmeses|clearly)").unwrap();
        let mut printer = StandardBuilder::new()
            .per_match(true)
            .per_match_one_line(true)
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .multi_line(true)
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:16:For the Doctor Watsons of this world, as opposed to the Sherlock
5:12:but Doctor Watson has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn per_match_multi_line3_only_first_line() {
        let matcher =
            RegexMatcher::new(r"(?s)Watson.+?Holmeses|always.+?be").unwrap();
        let mut printer = StandardBuilder::new()
            .per_match(true)
            .per_match_one_line(true)
            .column(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .multi_line(true)
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:16:For the Doctor Watsons of this world, as opposed to the Sherlock
2:58:Holmeses, success in the province of detective work must always
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn replacement_passthru() {
        let matcher = RegexMatcher::new(r"Sherlock|Doctor (\w+)").unwrap();
        let mut printer = StandardBuilder::new()
            .replacement(Some(b"doctah $1 MD".to_vec()))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .passthru(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:For the doctah Watsons MD of this world, as opposed to the doctah  MD
2-Holmeses, success in the province of detective work must always
3:be, to a very large extent, the result of luck. doctah  MD Holmes
4-can extract a clew from a wisp of straw or a flake of cigar ash;
5:but doctah Watson MD has to have it taken out for him and dusted,
6-and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn replacement() {
        let matcher = RegexMatcher::new(r"Sherlock|Doctor (\w+)").unwrap();
        let mut printer = StandardBuilder::new()
            .replacement(Some(b"doctah $1 MD".to_vec()))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:For the doctah Watsons MD of this world, as opposed to the doctah  MD
3:be, to a very large extent, the result of luck. doctah  MD Holmes
5:but doctah Watson MD has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    // This is a somewhat weird test that checks the behavior of attempting
    // to replace a line terminator with something else.
    //
    // See: https://github.com/BurntSushi/ripgrep/issues/1311
    #[test]
    fn replacement_multi_line() {
        let matcher = RegexMatcher::new(r"\n").unwrap();
        let mut printer = StandardBuilder::new()
            .replacement(Some(b"?".to_vec()))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .multi_line(true)
            .build()
            .search_reader(
                &matcher,
                "hello\nworld\n".as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "1:hello?world?\n";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn replacement_multi_line_diff_line_term() {
        let matcher = RegexMatcherBuilder::new()
            .line_terminator(Some(b'\x00'))
            .build(r"\n")
            .unwrap();
        let mut printer = StandardBuilder::new()
            .replacement(Some(b"?".to_vec()))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_terminator(LineTerminator::byte(b'\x00'))
            .line_number(true)
            .multi_line(true)
            .build()
            .search_reader(
                &matcher,
                "hello\nworld\n".as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "1:hello?world?\x00";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn replacement_multi_line_combine_lines() {
        let matcher = RegexMatcher::new(r"\n(.)?").unwrap();
        let mut printer = StandardBuilder::new()
            .replacement(Some(b"?$1".to_vec()))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .multi_line(true)
            .build()
            .search_reader(
                &matcher,
                "hello\nworld\n".as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "1:hello?world?\n";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn replacement_max_columns() {
        let matcher = RegexMatcher::new(r"Sherlock|Doctor (\w+)").unwrap();
        let mut printer = StandardBuilder::new()
            .max_columns(Some(67))
            .replacement(Some(b"doctah $1 MD".to_vec()))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:[Omitted long line with 2 matches]
3:be, to a very large extent, the result of luck. doctah  MD Holmes
5:but doctah Watson MD has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn replacement_max_columns_preview1() {
        let matcher = RegexMatcher::new(r"Sherlock|Doctor (\w+)").unwrap();
        let mut printer = StandardBuilder::new()
            .max_columns(Some(67))
            .max_columns_preview(true)
            .replacement(Some(b"doctah $1 MD".to_vec()))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:For the doctah Watsons MD of this world, as opposed to the doctah   [... 0 more matches]
3:be, to a very large extent, the result of luck. doctah  MD Holmes
5:but doctah Watson MD has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn replacement_max_columns_preview2() {
        let matcher =
            RegexMatcher::new("exhibited|dusted|has to have it").unwrap();
        let mut printer = StandardBuilder::new()
            .max_columns(Some(43))
            .max_columns_preview(true)
            .replacement(Some(b"xxx".to_vec()))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(false)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
but Doctor Watson xxx taken out for him and [... 1 more match]
and xxx clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn replacement_only_matching() {
        let matcher = RegexMatcher::new(r"Sherlock|Doctor (\w+)").unwrap();
        let mut printer = StandardBuilder::new()
            .only_matching(true)
            .replacement(Some(b"doctah $1 MD".to_vec()))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:doctah Watsons MD
1:doctah  MD
3:doctah  MD
5:doctah Watson MD
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn replacement_per_match() {
        let matcher = RegexMatcher::new(r"Sherlock|Doctor (\w+)").unwrap();
        let mut printer = StandardBuilder::new()
            .per_match(true)
            .replacement(Some(b"doctah $1 MD".to_vec()))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1:For the doctah Watsons MD of this world, as opposed to the doctah  MD
1:For the doctah Watsons MD of this world, as opposed to the doctah  MD
3:be, to a very large extent, the result of luck. doctah  MD Holmes
5:but doctah Watson MD has to have it taken out for him and dusted,
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn invert() {
        let matcher = RegexMatcher::new(r"Sherlock").unwrap();
        let mut printer = StandardBuilder::new().build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .invert_match(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
2:Holmeses, success in the province of detective work must always
4:can extract a clew from a wisp of straw or a flake of cigar ash;
5:but Doctor Watson has to have it taken out for him and dusted,
6:and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn invert_multi_line() {
        let matcher = RegexMatcher::new(r"(?s:.{0})Sherlock").unwrap();
        let mut printer = StandardBuilder::new().build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .multi_line(true)
            .line_number(true)
            .invert_match(true)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
2:Holmeses, success in the province of detective work must always
4:can extract a clew from a wisp of straw or a flake of cigar ash;
5:but Doctor Watson has to have it taken out for him and dusted,
6:and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn invert_context() {
        let matcher = RegexMatcher::new(r"Sherlock").unwrap();
        let mut printer = StandardBuilder::new().build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .invert_match(true)
            .before_context(1)
            .after_context(1)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1-For the Doctor Watsons of this world, as opposed to the Sherlock
2:Holmeses, success in the province of detective work must always
3-be, to a very large extent, the result of luck. Sherlock Holmes
4:can extract a clew from a wisp of straw or a flake of cigar ash;
5:but Doctor Watson has to have it taken out for him and dusted,
6:and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn invert_context_multi_line() {
        let matcher = RegexMatcher::new(r"(?s:.{0})Sherlock").unwrap();
        let mut printer = StandardBuilder::new().build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .multi_line(true)
            .line_number(true)
            .invert_match(true)
            .before_context(1)
            .after_context(1)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1-For the Doctor Watsons of this world, as opposed to the Sherlock
2:Holmeses, success in the province of detective work must always
3-be, to a very large extent, the result of luck. Sherlock Holmes
4:can extract a clew from a wisp of straw or a flake of cigar ash;
5:but Doctor Watson has to have it taken out for him and dusted,
6:and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn invert_context_only_matching() {
        let matcher = RegexMatcher::new(r"Sherlock").unwrap();
        let mut printer = StandardBuilder::new()
            .only_matching(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .invert_match(true)
            .before_context(1)
            .after_context(1)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1-Sherlock
2:Holmeses, success in the province of detective work must always
3-Sherlock
4:can extract a clew from a wisp of straw or a flake of cigar ash;
5:but Doctor Watson has to have it taken out for him and dusted,
6:and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn invert_context_only_matching_multi_line() {
        let matcher = RegexMatcher::new(r"(?s:.{0})Sherlock").unwrap();
        let mut printer = StandardBuilder::new()
            .only_matching(true)
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .multi_line(true)
            .line_number(true)
            .invert_match(true)
            .before_context(1)
            .after_context(1)
            .build()
            .search_reader(
                &matcher,
                SHERLOCK.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "\
1-Sherlock
2:Holmeses, success in the province of detective work must always
3-Sherlock
4:can extract a clew from a wisp of straw or a flake of cigar ash;
5:but Doctor Watson has to have it taken out for him and dusted,
6:and exhibited clearly, with a label attached.
";
        assert_eq_printed!(expected, got);
    }

    #[test]
    fn regression_search_empty_with_crlf() {
        let matcher =
            RegexMatcherBuilder::new().crlf(true).build(r"x?").unwrap();
        let mut printer = StandardBuilder::new()
            .color_specs(ColorSpecs::default_with_color())
            .build(Ansi::new(vec![]));
        SearcherBuilder::new()
            .line_terminator(LineTerminator::crlf())
            .build()
            .search_reader(&matcher, &b"\n"[..], printer.sink(&matcher))
            .unwrap();

        let got = printer_contents_ansi(&mut printer);
        assert!(!got.is_empty());
    }

    #[test]
    fn regression_after_context_with_match() {
        let haystack = "\
a
b
c
d
e
d
e
d
e
d
e
";

        let matcher = RegexMatcherBuilder::new().build(r"d").unwrap();
        let mut printer = StandardBuilder::new()
            .max_matches(Some(1))
            .build(NoColor::new(vec![]));
        SearcherBuilder::new()
            .line_number(true)
            .after_context(2)
            .build()
            .search_reader(
                &matcher,
                haystack.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();

        let got = printer_contents(&mut printer);
        let expected = "4:d\n5-e\n6:d\n";
        assert_eq_printed!(expected, got);
    }
}
