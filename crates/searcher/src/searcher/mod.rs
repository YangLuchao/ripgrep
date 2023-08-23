use std::cell::RefCell;
use std::cmp;
use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use crate::line_buffer::{
    self, alloc_error, BufferAllocation, LineBuffer, LineBufferBuilder,
    LineBufferReader, DEFAULT_BUFFER_CAPACITY,
};
use crate::searcher::glue::{MultiLine, ReadByLine, SliceByLine};
use crate::sink::{Sink, SinkError};
use encoding_rs;
use encoding_rs_io::DecodeReaderBytesBuilder;
use grep_matcher::{LineTerminator, Match, Matcher};

pub use self::mmap::MmapChoice;

mod core;
mod glue;
mod mmap;
/// 由于我们希望在实践中用于任意范围，但是想要匹配器的 `Match` 类型的舒适性，所以我们使用此类型别名，因此为其提供一个更准确的名称。仅在搜索器的内部使用。
type Range = Match;

/// 搜索期间的二进制检测行为。
///
/// 二进制检测是一种根据启发式方法识别给定数据块是否为二进制数据的过程，然后根据该启发式方法的结果采取行动。进行二进制数据检测的动机是，二进制数据通常表示不希望使用文本模式进行搜索的数据。当然，也有许多情况不成立，这就是为什么默认情况下禁用二进制检测的原因。
///
/// 不幸的是，二进制检测的工作方式取决于执行的搜索类型：
///
/// 1. 使用固定大小缓冲区执行搜索时，二进制检测应用于填充缓冲区时的内容。必须直接对缓冲区应用二进制检测，因为二进制文件可能不包含行终止符，这可能导致内存使用量过大。
/// 2. 使用内存映射或从堆上读取数据执行搜索时，二进制检测仅保证应用于与匹配相对应的部分。当启用 `Quit` 时，会在数据的前几 KB 中搜索二进制数据。
#[derive(Clone, Debug, Default)]
pub struct BinaryDetection(line_buffer::BinaryDetection);

impl BinaryDetection {
    /// 不执行二进制检测。搜索器报告的数据可能包含任意字节。
    ///
    /// 这是默认值。
    pub fn none() -> BinaryDetection {
        BinaryDetection(line_buffer::BinaryDetection::None)
    }

    /// 通过查找给定字节执行二进制检测。
    ///
    /// 当使用固定大小缓冲区执行搜索时，始终搜索该缓冲区的内容以查找该字节的存在。如果找到它，则将视为底层数据为二进制数据，并且搜索将停止，就像到达了文件末尾一样。
    ///
    /// 当使用内存完全映射到内存中的内容进行搜索时，二进制检测更为保守。即只会检测内容开头的固定大小区域是否包含二进制数据。为了取得折衷，任何后续匹配（或上下文）行也会被搜索二进制数据。如果在任何时候检测到二进制数据，则搜索将停止，就像到达了文件末尾一样。
    pub fn quit(binary_byte: u8) -> BinaryDetection {
        BinaryDetection(line_buffer::BinaryDetection::Quit(binary_byte))
    }

    /// 通过查找给定字节执行二进制检测，并将其替换为搜索器配置的行终止符。
    /// （如果搜索器配置为使用 `CRLF` 作为行终止符，则此字节将被替换为 `LF`。）
    ///
    /// 当使用固定大小缓冲区执行搜索时，始终搜索该缓冲区的内容以查找该字节的存在，并将其替换为行终止符。实际上，保证在搜索过程中观察不到此字节。
    ///
    /// 当使用内存完全映射到内存中的内容进行搜索时，此设置不起作用并被忽略。
    pub fn convert(binary_byte: u8) -> BinaryDetection {
        BinaryDetection(line_buffer::BinaryDetection::Convert(binary_byte))
    }

    /// 如果此二进制检测使用“退出”策略，则返回将导致搜索退出的字节。在其他情况下，返回 `None`。
    pub fn quit_byte(&self) -> Option<u8> {
        match self.0 {
            line_buffer::BinaryDetection::Quit(b) => Some(b),
            _ => None,
        }
    }

    /// 如果此二进制检测使用“转换”策略，则返回将被替换为行终止符的字节。在其他情况下，返回 `None`。
    pub fn convert_byte(&self) -> Option<u8> {
        match self.0 {
            line_buffer::BinaryDetection::Convert(b) => Some(b),
            _ => None,
        }
    }
}
/// 用于搜索时的编码。
///
/// 可以使用编码配置 [`SearcherBuilder`](struct.SearchBuilder.html)，
/// 将源数据从编码转码为 UTF-8 后再进行搜索。
///
/// `Encoding` 的克隆始终是廉价的。
#[derive(Clone, Debug)]
pub struct Encoding(&'static encoding_rs::Encoding);

impl Encoding {
    /// 为指定的标签创建一个新的编码。
    ///
    /// 提供的编码标签通过编码标准中指定的可用选择集映射到编码。
    /// 如果给定的标签不对应有效的编码，则会返回错误。
    pub fn new(label: &str) -> Result<Encoding, ConfigError> {
        let label = label.as_bytes();
        match encoding_rs::Encoding::for_label_no_replacement(label) {
            Some(encoding) => Ok(Encoding(encoding)),
            None => {
                Err(ConfigError::UnknownEncoding { label: label.to_vec() })
            }
        }
    }
}

/// 搜索器的内部配置。这在多个与搜索相关的类型之间共享，但仅由 SearcherBuilder 写入。
#[derive(Clone, Debug)]
pub struct Config {
    /// 要使用的行终止符。
    line_term: LineTerminator,
    /// 是否反转匹配。
    invert_match: bool,
    /// 匹配后要包括的行数。
    after_context: usize,
    /// 匹配前要包括的行数。
    before_context: usize,
    /// 是否启用无限制的上下文。
    passthru: bool,
    /// 是否计数行号。
    line_number: bool,
    /// 要使用的最大堆内存量。
    ///
    /// 当未提供时，不会强制执行显式限制。当设置为 `0` 时，只有内存映射搜索策略可用。
    heap_limit: Option<usize>,
    /// 内存映射策略。
    mmap: MmapChoice,
    /// 二进制数据检测策略。
    binary: BinaryDetection,
    /// 是否允许跨多行进行匹配。
    multi_line: bool,
    /// 当存在时，会导致搜索器将所有输入从编码转码为 UTF-8 的编码。
    encoding: Option<Encoding>,
    /// 是否根据 BOM 自动进行转码。
    bom_sniffing: bool,
    /// 是否在找到匹配行后停止搜索非匹配行。
    stop_on_nonmatch: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            line_term: LineTerminator::default(),
            invert_match: false,
            after_context: 0,
            before_context: 0,
            passthru: false,
            line_number: true,
            heap_limit: None,
            mmap: MmapChoice::default(),
            binary: BinaryDetection::default(),
            multi_line: false,
            encoding: None,
            bom_sniffing: true,
            stop_on_nonmatch: false,
        }
    }
}

impl Config {
    /// 返回满足此配置的上下文所需的最大行数。
    ///
    /// 如果返回 `0`，则永远不需要上下文。
    fn max_context(&self) -> usize {
        cmp::max(self.before_context, self.after_context)
    }

    /// 根据此配置构建一个行缓冲区。
    fn line_buffer(&self) -> LineBuffer {
        let mut builder = LineBufferBuilder::new();
        builder
            .line_terminator(self.line_term.as_byte())
            .binary_detection(self.binary.0);

        if let Some(limit) = self.heap_limit {
            let (capacity, additional) = if limit <= DEFAULT_BUFFER_CAPACITY {
                (limit, 0)
            } else {
                (DEFAULT_BUFFER_CAPACITY, limit - DEFAULT_BUFFER_CAPACITY)
            };
            builder
                .capacity(capacity)
                .buffer_alloc(BufferAllocation::Error(additional));
        }
        builder.build()
    }
}

/// 构建搜索器时可能发生的错误。
///
/// 在从 `SearcherBuilder` 构造 `Searcher` 时出现此错误，当出现无意义的配置时。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigError {
    /// 表示堆限制配置阻止使用所有可能的搜索策略。例如，如果堆限制设置为 0 并且内存映射搜索被禁用或不可用。
    SearchUnavailable,
    /// 在匹配器报告的行终止符与搜索器中配置的行终止符不同时出现。
    MismatchedLineTerminators {
        /// 匹配器的行终止符。
        matcher: LineTerminator,
        /// 搜索器的行终止符。
        searcher: LineTerminator,
    },
    /// 在找不到特定标签的编码时出现。
    UnknownEncoding {
        /// 无法找到的提供的编码标签。
        label: Vec<u8>,
    },
    /// 提示不应全面解构。
    ///
    /// 此枚举可能会增加其他变体，因此这确保客户端不依赖于全面匹配。
    #[doc(hidden)]
    __Nonexhaustive,
}

impl ::std::error::Error for ConfigError {
    fn description(&self) -> &str {
        "grep-searcher 配置错误"
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ConfigError::SearchUnavailable => {
                write!(f, "grep 配置错误：无可用的搜索器")
            }
            ConfigError::MismatchedLineTerminators { matcher, searcher } => {
                write!(
                    f,
                    "grep 配置错误：不匹配的行终止符，匹配器为 {:?}，搜索器为 {:?}",
                    matcher, searcher
                )
            }
            ConfigError::UnknownEncoding { ref label } => write!(
                f,
                "grep 配置错误：未知的编码：{}",
                String::from_utf8_lossy(label),
            ),
            _ => panic!("BUG：找到了意外的变体"),
        }
    }
}

/// 用于配置搜索器的构建器。
///
/// 搜索构建器允许指定搜索器的配置，包括是否反转搜索或启用多行搜索等选项。
///
/// 构建了搜索器后，如果可能，最好重用该搜索器进行多次搜索。
#[derive(Clone, Debug)]
pub struct SearcherBuilder {
    config: Config,
}

impl Default for SearcherBuilder {
    fn default() -> SearcherBuilder {
        SearcherBuilder::new()
    }
}

impl SearcherBuilder {
    /// 使用默认配置创建新的搜索器构建器。
    pub fn new() -> SearcherBuilder {
        SearcherBuilder { config: Config::default() }
    }

    /// 使用给定的匹配器构建搜索器。
    pub fn build(&self) -> Searcher {
        let mut config = self.config.clone();
        if config.passthru {
            config.before_context = 0;
            config.after_context = 0;
        }

        let mut decode_builder = DecodeReaderBytesBuilder::new();
        decode_builder
            .encoding(self.config.encoding.as_ref().map(|e| e.0))
            .utf8_passthru(true)
            .strip_bom(self.config.bom_sniffing)
            .bom_override(true)
            .bom_sniffing(self.config.bom_sniffing);

        Searcher {
            config: config,
            decode_builder: decode_builder,
            decode_buffer: RefCell::new(vec![0; 8 * (1 << 10)]),
            line_buffer: RefCell::new(self.config.line_buffer()),
            multi_line_buffer: RefCell::new(vec![]),
        }
    }

    /// 设置搜索器使用的行终止符。
    ///
    /// 在使用搜索器时，如果提供的匹配器设置了行终止符，
    /// 则它必须与此行终止符相同。如果它们不相同，构建搜索器将返回错误。
    ///
    /// 默认情况下，设置为 `b'\n'`。
    pub fn line_terminator(
        &mut self,
        line_term: LineTerminator,
    ) -> &mut SearcherBuilder {
        self.config.line_term = line_term;
        self
    }

    /// 是否反转匹配，即报告不匹配的行而不是报告匹配的行。
    ///
    /// 默认情况下，此选项被禁用。
    pub fn invert_match(&mut self, yes: bool) -> &mut SearcherBuilder {
        self.config.invert_match = yes;
        self
    }

    /// 是否计数并包括匹配行的行号。
    ///
    /// 默认情况下，此选项已启用。计算行号会带来一些性能损失，
    /// 因此在不需要计算行号时可以禁用此选项。
    pub fn line_number(&mut self, yes: bool) -> &mut SearcherBuilder {
        self.config.line_number = yes;
        self
    }

    /// 是否启用多行搜索。
    ///
    /// 当启用多行搜索时，匹配可能跨多行。相反，当禁用多行搜索时，
    /// 任何匹配都不能跨越多于一行。
    ///
    /// **警告：** 多行搜索需要将整个要搜索的内容映射到内存中。
    /// 在搜索文件时，如果可能且启用了内存映射，将使用内存映射，
    /// 以避免使用程序的堆。但是，如果无法使用内存映射（例如搜索流，如 `stdin`，
    /// 或者需要进行转码），则会在开始搜索之前将流的整个内容读取到堆上。
    ///
    /// 默认情况下，此选项已禁用。
    pub fn multi_line(&mut self, yes: bool) -> &mut SearcherBuilder {
        self.config.multi_line = yes;
        self
    }

    /// 是否在每个匹配后包括固定数量的行上下文。
    ///
    /// 当将此设置为非零数字时，搜索器将在每个匹配后报告 `line_count` 个上下文行。
    ///
    /// 默认情况下，此设置为 `0`。
    pub fn after_context(
        &mut self,
        line_count: usize,
    ) -> &mut SearcherBuilder {
        self.config.after_context = line_count;
        self
    }

    /// 是否在每个匹配前包括固定数量的行上下文。
    ///
    /// 当将此设置为非零数字时，搜索器将在每个匹配前报告 `line_count` 个上下文行。
    ///
    /// 默认情况下，此设置为 `0`。
    pub fn before_context(
        &mut self,
        line_count: usize,
    ) -> &mut SearcherBuilder {
        self.config.before_context = line_count;
        self
    }

    /// 是否启用 "passthru" 特性。
    ///
    /// 启用 passthru 后，它实际上将所有不匹配的行视为上下文行。
    /// 换句话说，启用此选项类似于请求无限数量的前后上下文行。
    ///
    /// 当启用 passthru 模式时，任何 `before_context` 或 `after_context` 设置
    /// 都将被设置为 `0`，从而被忽略。
    ///
    /// 默认情况下，此选项已禁用。
    pub fn passthru(&mut self, yes: bool) -> &mut SearcherBuilder {
        self.config.passthru = yes;
        self
    }

    /// 设置搜索器使用的堆空间的近似限制量。
    ///
    /// 堆限制在两种情况下执行：
    ///
    /// * 使用固定大小缓冲区进行搜索时，堆限制控制允许的缓冲区大小。
    ///   假设上下文已禁用，此缓冲区的最小大小是正在搜索的内容中最长一行的长度（以字节为单位）。
    ///   如果任何行超过堆限制，将返回错误。
    /// * 执行多行搜索时，不能使用固定大小缓冲区。因此，唯一的选择是将整个内容读取到堆上，或使用内存映射。
    ///   在前一种情况下，此处设置的堆限制将受到执行。
    ///
    /// 如果将堆限制设置为 `0`，则不使用堆空间。如果没有其他可用的策略来在没有堆空间的情况下进行搜索
    /// （例如，禁用了内存映射），则搜索器将立即返回错误。
    ///
    /// 默认情况下，未设置限制。
    pub fn heap_limit(
        &mut self,
        bytes: Option<usize>,
    ) -> &mut SearcherBuilder {
        self.config.heap_limit = bytes;
        self
    }

    /// 设置内存映射使用的策略。
    ///
    /// 目前，只能使用两种策略：
    ///
    /// * **自动** - 搜索器将使用启发式方法（包括但不限于文件大小和平台）来确定是否使用内存映射。
    /// * **永不** - 永不使用内存映射。如果启用了多行搜索，则将在搜索开始之前将整个内容读取到堆上。
    ///
    /// 默认行为是 **永不**。一般来说，也许与常规智慧相反，内存映射不一定能够实现更快的搜索。
    /// 例如，根据平台的不同，在搜索大目录时，使用内存映射可能比使用普通的读取调用要慢得多，
    /// 这是由于管理内存映射的开销。
    ///
    /// 然而，在某些情况下，内存映射可能更快。在某些平台上，当搜索一个非常大的文件时，
    /// *已经在内存中* 的文件作为内存映射搜索，可能比使用普通的读取调用稍微快一些。
    ///
    /// 最后，内存映射在 Rust 中具有相当复杂的安全性问题。
    /// 如果不确定是否值得启用内存映射，那就不要费心思考它。
    ///
    /// **警告**：如果您的进程在搜索与内存映射支持的文件时，
    /// 同时截断了该文件，那么进程可能会因总线错误而终止。
    pub fn memory_map(
        &mut self,
        strategy: MmapChoice,
    ) -> &mut SearcherBuilder {
        self.config.mmap = strategy;
        self
    }

    /// 设置二进制数据检测策略。
    ///
    /// 二进制数据检测策略不仅决定搜索器如何检测二进制数据，还决定它在存在二进制数据时如何响应。
    /// 有关更多信息，请参阅 [`BinaryDetection`](struct.BinaryDetection.html) 类型。
    ///
    /// 默认情况下，二进制数据检测被禁用。
    pub fn binary_detection(
        &mut self,
        detection: BinaryDetection,
    ) -> &mut SearcherBuilder {
        self.config.binary = detection;
        self
    }

    /// 设置用于在搜索之前读取源数据的编码。
    ///
    /// 当提供编码时，源数据将在搜索之前 _无条件_ 地使用该编码进行转码，
    /// 除非存在 BOM。如果存在 BOM，则使用 BOM 指示的编码。如果转码过程遇到错误，
    /// 则将字节替换为 Unicode 替换代码点。
    ///
    /// 当未指定编码时（默认情况），则将使用 BOM 嗅探（如果默认情况下启用了它）来确定
    /// 源数据是否为 UTF-8 或 UTF-16，并将自动执行转码。如果找不到 BOM，则将源数据搜索为 UTF-8。
    /// 但是，只要源数据至少兼容 ASCII，就有可能进行有效的搜索。
    pub fn encoding(
        &mut self,
        encoding: Option<Encoding>,
    ) -> &mut SearcherBuilder {
        self.config.encoding = encoding;
        self
    }

    /// 启用基于 BOM 嗅探的自动转码。
    ///
    /// 当启用此选项且未设置显式编码时，此搜索器将尝试通过嗅探其字节顺序标记（BOM）来检测
    /// 要搜索的字节的编码。特别地，当启用此选项时，UTF-16 编码的文件将被无缝地搜索。
    ///
    /// 当禁用此选项且未设置显式编码时，源流的字节将保持不变，包括其 BOM（如果存在）。
    ///
    /// 默认情况下，此选项已启用。
    pub fn bom_sniffing(&mut self, yes: bool) -> &mut SearcherBuilder {
        self.config.bom_sniffing = yes;
        self
    }

    /// 在匹配行后找到一个不匹配的行时，停止搜索文件。
    ///
    /// 对于在预期所有匹配都在相邻行上的排序文件中进行搜索时，这很有用。
    pub fn stop_on_nonmatch(
        &mut self,
        stop_on_nonmatch: bool,
    ) -> &mut SearcherBuilder {
        self.config.stop_on_nonmatch = stop_on_nonmatch;
        self
    }
}

/// 一个搜索器执行针对一个指定 haystack 的搜索，并将结果写入调用方提供的 sink 中。
///
/// 匹配是通过实现 `Matcher` trait 来检测的，这必须由调用者在执行搜索时提供。
///
/// 在可能的情况下，应重复使用搜索器。
#[derive(Clone, Debug)]
pub struct Searcher {
    // 此搜索器的配置。
    //
    // 我们通过公共的 API 方法将大多数这些设置提供给 `Searcher` 的用户，
    // 这些设置在必要时可以在 `Sink` 的实现中查询。
    config: Config,
    // 用于构建流式阅读器的构建器，根据显式指定的编码或通过 BOM 嗅探自动检测编码。
    //
    // 当不需要转码时，构建的转码器将不会增加底层字节的额外开销。
    decode_builder: DecodeReaderBytesBuilder,
    // 用于转码临时空间的缓冲区。
    decode_buffer: RefCell<Vec<u8>>,
    // 用于行定向搜索的行缓冲区。
    //
    // 我们将其包装在 RefCell 中，以允许将 `Searcher` 的借用借给 sink。
    // 尽管我们仍然需要可变借用来执行搜索，但我们在静态上阻止调用者引起 RefCell 在运行时由于借用违规而引发 panic。
    line_buffer: RefCell<LineBuffer>,
    // 用于在执行多行搜索时存储读取器的内容的缓冲区。
    // 特别地，无法以增量方式执行多行搜索，需要一次性将整个 haystack 存储在内存中。
    multi_line_buffer: RefCell<Vec<u8>>,
}

impl Searcher {
    /// 使用默认配置创建新的搜索器。
    ///
    /// 要配置搜索器（例如，反转匹配，启用内存映射，启用上下文等），请使用
    /// [`SearcherBuilder`](struct.SearcherBuilder.html)。
    pub fn new() -> Searcher {
        SearcherBuilder::new().build()
    }

    /// 在具有给定路径的文件上执行搜索，并将结果写入给定的 sink。
    ///
    /// 如果启用了内存映射，并且搜索器在启发性地认为内存映射将有助于更快地运行搜索，
    /// 则将使用内存映射。因此，调用者应优先使用此方法或 `search_file`，而不是在可能的情况下使用更通用的 `search_reader`。
    pub fn search_path<P, M, S>(
        &mut self,
        matcher: M,
        path: P,
        write_to: S,
    ) -> Result<(), S::Error>
    where
        P: AsRef<Path>,
        M: Matcher,
        S: Sink,
    {
        let path = path.as_ref();
        let file = File::open(path).map_err(S::Error::error_io)?;
        self.search_file_maybe_path(matcher, Some(path), &file, write_to)
    }

    /// 在文件上执行搜索，并将结果写入给定的 sink。
    ///
    /// 如果启用了内存映射，并且搜索器在启发性地认为内存映射将有助于更快地运行搜索，
    /// 则将使用内存映射。因此，调用者应优先使用此方法或 `search_path`，而不是在可能的情况下使用更通用的 `search_reader`。
    pub fn search_file<M, S>(
        &mut self,
        matcher: M,
        file: &File,
        write_to: S,
    ) -> Result<(), S::Error>
    where
        M: Matcher,
        S: Sink,
    {
        self.search_file_maybe_path(matcher, None, file, write_to)
    }

    // 在文件上执行搜索，并将结果写入给定的 sink。
    //
    // 如果启用了内存映射，并且搜索器在启发性地认为内存映射将有助于更快地运行搜索，
    // 则将使用内存映射。因此，调用者应优先使用此方法或 `search_path`，而不是在可能的情况下使用更通用的 `search_reader`。
    fn search_file_maybe_path<M, S>(
        &mut self,
        matcher: M,
        path: Option<&Path>,
        file: &File,
        write_to: S,
    ) -> Result<(), S::Error>
    where
        M: Matcher,
        S: Sink,
    {
        if let Some(mmap) = self.config.mmap.open(file, path) {
            log::trace!("{:?}: 通过内存映射进行搜索", path);
            return self.search_slice(matcher, &mmap, write_to);
        }
        // 多行搜索的文件的快速路径，当不启用内存映射时。
        // 这将预先分配一个大致与文件大小相当的缓冲区，这在搜索任意 io::Read 时是不可能的。
        if self.multi_line_with_matcher(&matcher) {
            log::trace!("{:?}: 将整个文件读取到堆上以用于多行搜索", path);
            self.fill_multi_line_buffer_from_file::<S>(file)?;
            log::trace!("{:?}: 通过多行策略进行搜索", path);
            MultiLine::new(
                self,
                matcher,
                &*self.multi_line_buffer.borrow(),
                write_to,
            )
            .run()
        } else {
            log::trace!("{:?}: 使用通用读取器进行搜索", path);
            self.search_reader(matcher, file, write_to)
        }
    }

    /// 在任何实现了 `io::Read` 的内容上执行搜索，并将结果写入给定的 sink。
    ///
    /// 在可能的情况下，此实现将以增量方式搜索读取器，而不是将其读入内存。
    /// 在某些情况下，例如，如果启用了多行搜索，则无法进行增量搜索，并且需要在搜索开始之前完全读取给定的读取器并放入堆上。
    /// 因此，当启用多行搜索时，应尽量使用更高级的 API（例如，通过文件或文件路径进行搜索），
    /// 以便在可用并启用时可以使用内存映射。
    pub fn search_reader<M, R, S>(
        &mut self,
        matcher: M,
        read_from: R,
        write_to: S,
    ) -> Result<(), S::Error>
    where
        M: Matcher,
        R: io::Read,
        S: Sink,
    {
        self.check_config(&matcher).map_err(S::Error::error_config)?;

        let mut decode_buffer = self.decode_buffer.borrow_mut();
        let decoder = self
            .decode_builder
            .build_with_buffer(read_from, &mut *decode_buffer)
            .map_err(S::Error::error_io)?;

        if self.multi_line_with_matcher(&matcher) {
            log::trace!("通用读取器：将一切内容读取到堆上，用于多行搜索");
            self.fill_multi_line_buffer_from_reader::<_, S>(decoder)?;
            log::trace!("通用读取器：通过多行策略进行搜索");
            MultiLine::new(
                self,
                matcher,
                &*self.multi_line_buffer.borrow(),
                write_to,
            )
            .run()
        } else {
            let mut line_buffer = self.line_buffer.borrow_mut();
            let rdr = LineBufferReader::new(decoder, &mut *line_buffer);
            log::trace!("通用读取器：通过滚动缓冲区策略进行搜索");
            ReadByLine::new(self, matcher, rdr, write_to).run()
        }
    }

    /// 在给定的切片上执行搜索，并将结果写入给定的 sink。
    pub fn search_slice<M, S>(
        &mut self,
        matcher: M,
        slice: &[u8],
        write_to: S,
    ) -> Result<(), S::Error>
    where
        M: Matcher,
        S: Sink,
    {
        self.check_config(&matcher).map_err(S::Error::error_config)?;

        // 我们可以直接搜索切片，除非需要进行转码。
        if self.slice_needs_transcoding(slice) {
            log::trace!("切片读取器：需要转码，使用通用读取器");
            return self.search_reader(matcher, slice, write_to);
        }
        if self.multi_line_with_matcher(&matcher) {
            log::trace!("切片读取器：通过多行策略进行搜索");
            MultiLine::new(self, matcher, slice, write_to).run()
        } else {
            log::trace!("切片读取器：通过逐行切片策略进行搜索");
            SliceByLine::new(self, matcher, slice, write_to).run()
        }
    }

    /// 设置此搜索器上使用的二进制检测方法。
    pub fn set_binary_detection(&mut self, detection: BinaryDetection) {
        self.config.binary = detection.clone();
        self.line_buffer.borrow_mut().set_binary_detection(detection.0);
    }

    // 检查搜索器的配置和匹配器是否与彼此一致。
    fn check_config<M: Matcher>(&self, matcher: M) -> Result<(), ConfigError> {
        if self.config.heap_limit == Some(0) && !self.config.mmap.is_enabled()
        {
            return Err(ConfigError::SearchUnavailable);
        }
        let matcher_line_term = match matcher.line_terminator() {
            None => return Ok(()),
            Some(line_term) => line_term,
        };
        if matcher_line_term != self.config.line_term {
            return Err(ConfigError::MismatchedLineTerminators {
                matcher: matcher_line_term,
                searcher: self.config.line_term,
            });
        }
        Ok(())
    }

    // 返回 true 当且仅当给定的切片需要进行转码。
    fn slice_needs_transcoding(&self, slice: &[u8]) -> bool {
        self.config.encoding.is_some()
            || (self.config.bom_sniffing && slice_has_bom(slice))
    }
}

// 下面的方法允许查询搜索器的配置。
// 这些对于 `Sink` 的通用实现可能很有用，
// 在这些实现中，输出可以基于搜索器的配置进行调整。
impl Searcher {
    /// 返回此搜索器使用的行终止符。
    #[inline]
    pub fn line_terminator(&self) -> LineTerminator {
        self.config.line_term
    }

    /// 返回配置在此搜索器上的二进制检测类型。
    #[inline]
    pub fn binary_detection(&self) -> &BinaryDetection {
        &self.config.binary
    }

    /// 返回 true 当且仅当此搜索器配置为反转其搜索结果。
    /// 也就是说，匹配的行是不匹配搜索器的匹配器的行。
    #[inline]
    pub fn invert_match(&self) -> bool {
        self.config.invert_match
    }

    /// 返回 true 当且仅当此搜索器配置为计算行号。
    #[inline]
    pub fn line_number(&self) -> bool {
        self.config.line_number
    }

    /// 返回 true 当且仅当此搜索器配置为执行多行搜索。
    #[inline]
    pub fn multi_line(&self) -> bool {
        self.config.multi_line
    }

    /// 返回 true 当且仅当此搜索器配置为在找到不匹配行后停止。
    #[inline]
    pub fn stop_on_nonmatch(&self) -> bool {
        self.config.stop_on_nonmatch
    }

    /// 返回 true 当且仅当此搜索器将根据提供的匹配器选择多行策略。
    ///
    /// 这可能与 `multi_line` 的结果不同，当搜索器被配置为执行可以在多行上报告匹配的搜索时，
    /// 但匹配器保证永远不会在多行上产生匹配时，可能会有所不同。
    pub fn multi_line_with_matcher<M: Matcher>(&self, matcher: M) -> bool {
        if !self.multi_line() {
            return false;
        }
        if let Some(line_term) = matcher.line_terminator() {
            if line_term == self.line_terminator() {
                return false;
            }
        }
        if let Some(non_matching) = matcher.non_matching_bytes() {
            // 如果行终止符是 CRLF，则实际上无需关心正则表达式是否可以匹配 `\r`。
            // 也就是说，`\r` 既不是必要的也不足以终止一行。始终需要 `\n`。
            if non_matching.contains(self.line_terminator().as_byte()) {
                return false;
            }
        }
        true
    }

    /// 返回要报告的 "after" 上下文行数。当未启用上下文报告时，返回 `0`。
    #[inline]
    pub fn after_context(&self) -> usize {
        self.config.after_context
    }

    /// 返回要报告的 "before" 上下文行数。当未启用上下文报告时，返回 `0`。
    #[inline]
    pub fn before_context(&self) -> usize {
        self.config.before_context
    }

    /// 返回 true 当且仅当搜索器启用了 "passthru" 模式。
    #[inline]
    pub fn passthru(&self) -> bool {
        self.config.passthru
    }

    // 从给定的文件填充用于多行搜索的缓冲区。
    // 这会从文件中读取直到 EOF 或发生错误。如果内容超过配置的堆限制，则返回错误。
    fn fill_multi_line_buffer_from_file<S: Sink>(
        &self,
        file: &File,
    ) -> Result<(), S::Error> {
        assert!(self.config.multi_line);

        let mut decode_buffer = self.decode_buffer.borrow_mut();
        let mut read_from = self
            .decode_builder
            .build_with_buffer(file, &mut *decode_buffer)
            .map_err(S::Error::error_io)?;

        // 如果没有堆限制，那么我们可以推迟使用 std 的 read_to_end 实现。
        // fill_multi_line_buffer_from_reader 也会这样做，但由于我们有一个 File，
        // 所以我们可以在这里做一些更聪明的预分配。
        //
        // 如果我们正在进行转码，则我们的预分配可能不是精确的，但可能仍然比什么都不做要好。
        if self.config.heap_limit.is_none() {
            let mut buf = self.multi_line_buffer.borrow_mut();
            buf.clear();
            let cap =
                file.metadata().map(|m| m.len() as usize + 1).unwrap_or(0);
            buf.reserve(cap);
            read_from.read_to_end(&mut *buf).map_err(S::Error::error_io)?;
            return Ok(());
        }
        self.fill_multi_line_buffer_from_reader::<_, S>(read_from)
    }

    // 从给定的读取器中填充用于多行搜索的缓冲区。
    // 这会从读取器中读取直到 EOF 或发生错误。如果内容超过配置的堆限制，则返回错误。
    fn fill_multi_line_buffer_from_reader<R: io::Read, S: Sink>(
        &self,
        mut read_from: R,
    ) -> Result<(), S::Error> {
        assert!(self.config.multi_line);

        let mut buf = self.multi_line_buffer.borrow_mut();
        buf.clear();

        // 如果没有堆限制，那么我们可以推迟使用 std 的 read_to_end 实现...
        let heap_limit = match self.config.heap_limit {
            Some(heap_limit) => heap_limit,
            None => {
                read_from
                    .read_to_end(&mut *buf)
                    .map_err(S::Error::error_io)?;
                return Ok(());
            }
        };
        if heap_limit == 0 {
            return Err(S::Error::error_io(alloc_error(heap_limit)));
        }

        // ... 否则我们需要自己实现。这可能会比最优解慢得多，
        // 但在没有足够理由加快速度之前，我们不用担心内存安全。
        buf.resize(cmp::min(DEFAULT_BUFFER_CAPACITY, heap_limit), 0);
        let mut pos = 0;
        loop {
            let nread = match read_from.read(&mut buf[pos..]) {
                Ok(nread) => nread,
                Err(ref err) if err.kind() == io::ErrorKind::Interrupted => {
                    continue;
                }
                Err(err) => return Err(S::Error::error_io(err)),
            };
            if nread == 0 {
                buf.resize(pos, 0);
                return Ok(());
            }

            pos += nread;
            if buf[pos..].is_empty() {
                let additional = heap_limit - buf.len();
                if additional == 0 {
                    return Err(S::Error::error_io(alloc_error(heap_limit)));
                }
                let limit = buf.len() + additional;
                let doubled = 2 * buf.len();
                buf.resize(cmp::min(doubled, limit), 0);
            }
        }
    }
}
// 返回值为 true 当且仅当给定的切片以 UTF-8 或 UTF-16 BOM 开头。
// 这在搜索器中用于确定是否需要转码。
// 否则，直接搜索切片会更有优势。
fn slice_has_bom(slice: &[u8]) -> bool {
    let enc = match encoding_rs::Encoding::for_bom(slice) {
        None => return false,
        Some((enc, _)) => enc,
    };
    // UTF-16LE、UTF-16BE 和 UTF-8 是可能的 BOM 编码
    [encoding_rs::UTF_16LE, encoding_rs::UTF_16BE, encoding_rs::UTF_8]
        .contains(&enc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{KitchenSink, RegexMatcher};

    #[test]
    fn config_error_heap_limit() {
        // 创建一个 RegexMatcher 用于测试
        let matcher = RegexMatcher::new("");
        // 创建一个 KitchenSink 用于测试
        let sink = KitchenSink::new();
        // 创建一个 heap_limit 为 0 的 Searcher
        let mut searcher = SearcherBuilder::new().heap_limit(Some(0)).build();
        // 在空切片上进行搜索，预期会产生堆错误
        let res = searcher.search_slice(matcher, &[], sink);
        assert!(res.is_err());
    }

    #[test]
    fn config_error_line_terminator() {
        // 创建一个空的 RegexMatcher
        let mut matcher = RegexMatcher::new("");
        // 设置不匹配的行终止符
        matcher.set_line_term(Some(LineTerminator::byte(b'z')));

        // 创建一个 KitchenSink 用于测试
        let sink = KitchenSink::new();
        // 创建一个新的 Searcher
        let mut searcher = Searcher::new();
        // 在空切片上进行搜索，预期会产生配置错误
        let res = searcher.search_slice(matcher, &[], sink);
        assert!(res.is_err());
    }

    #[test]
    fn uft8_bom_sniffing() {
        // 参考：https://github.com/BurntSushi/ripgrep/issues/1638
        // ripgrep 必须像对待 utf-16 一样嗅探 utf-8 BOM
        // 创建一个匹配 "foo" 的 RegexMatcher
        let matcher = RegexMatcher::new("foo");
        // 创建一个带 utf-8 BOM 的字节数组
        let haystack: &[u8] = &[0xef, 0xbb, 0xbf, 0x66, 0x6f, 0x6f];

        // 创建一个 KitchenSink 用于测试
        let mut sink = KitchenSink::new();
        // 创建一个 Searcher
        let mut searcher = SearcherBuilder::new().build();

        // 在 haystack 中搜索 "foo"，并将结果写入 sink
        let res = searcher.search_slice(matcher, haystack, &mut sink);
        assert!(res.is_ok());

        // 将 sink 的字节转换为字符串并进行比较
        let sink_output = String::from_utf8(sink.as_bytes().to_vec()).unwrap();
        assert_eq!(sink_output, "1:0:foo\nbyte count:3\n");
    }
}
