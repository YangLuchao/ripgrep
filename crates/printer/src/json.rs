use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

use grep_matcher::{Match, Matcher};
use grep_searcher::{
    Searcher, Sink, SinkContext, SinkContextKind, SinkFinish, SinkMatch,
};
use serde_json as json;

use crate::counter::CounterWriter;
use crate::jsont;
use crate::stats::Stats;
use crate::util::find_iter_at_in_context;
/// JSON 打印机的配置。
///
/// 这由 JSONBuilder 进行配置，然后由实际实现引用。构建打印机后，配置将被冻结，无法更改。
#[derive(Debug, Clone)]
struct Config {
    pretty: bool,             // 是否美化输出
    max_matches: Option<u64>, // 最大匹配数量
    always_begin_end: bool,   // 是否总是输出开始和结束信息
}

impl Default for Config {
    fn default() -> Config {
        Config { pretty: false, max_matches: None, always_begin_end: false }
    }
}

/// 用于构建 JSON 行打印机的构建器。
///
/// 该构建器允许配置打印机的行为。与标准打印机相比，JSON 打印机的配置选项较少，因为它是一种结构化格式，
/// 打印机总是尝试找到尽可能多的信息。
///
/// 某些配置选项（例如是否包括行号或是否显示上下文行）直接从 `grep_searcher::Searcher` 的配置中获取。
///
/// 一旦构建了 `JSON` 打印机，其配置就无法更改。
#[derive(Clone, Debug)]
pub struct JSONBuilder {
    config: Config,
}

impl JSONBuilder {
    /// 返回一个用于配置 JSON 打印机的新构建器。
    pub fn new() -> JSONBuilder {
        JSONBuilder { config: Config::default() }
    }

    /// 创建一个将结果写入给定写入器的 JSON 打印机。
    pub fn build<W: io::Write>(&self, wtr: W) -> JSON<W> {
        JSON {
            config: self.config.clone(),
            wtr: CounterWriter::new(wtr),
            matches: vec![],
        }
    }

    /// 以漂亮的格式打印 JSON。
    ///
    /// 启用此选项将不再产生“JSON 行”格式，因为打印的每个 JSON 对象可能跨多行。
    ///
    /// 默认情况下，此选项被禁用。
    pub fn pretty(&mut self, yes: bool) -> &mut JSONBuilder {
        self.config.pretty = yes;
        self
    }

    /// 设置要打印的最大匹配数量。
    ///
    /// 如果启用了多行搜索，并且匹配跨越多行，则为了执行此限制，将仅计数一次该匹配，
    /// 而不管它跨越多少行。
    pub fn max_matches(&mut self, limit: Option<u64>) -> &mut JSONBuilder {
        self.config.max_matches = limit;
        self
    }

    /// 当启用时，即使没有找到匹配，也始终会发出“begin”和“end”消息。
    ///
    /// 当禁用时，只有在至少有一个“match”或“context”消息时才会显示“begin”和“end”消息。
    ///
    /// 默认情况下，此选项被禁用。
    pub fn always_begin_end(&mut self, yes: bool) -> &mut JSONBuilder {
        self.config.always_begin_end = yes;
        self
    }
}
/// JSON打印机，以JSON行格式输出结果。
///
/// 此类型是泛型的，基于`W`，表示标准库`io::Write` trait的任何实现。
///
/// # 格式
///
/// 本节描述了此打印机使用的JSON格式。
///
/// 要跳过繁琐的内容，请查看末尾的
/// [示例](#example)。
///
/// ## 概述
///
/// 此打印机的格式是[JSON行](https://jsonlines.org/)格式。具体来说，此打印机会发出一系列消息，
/// 其中每个消息都被编码为单行上的单个JSON值。有四种不同类型的消息（此数量可能随时间扩展）：
///
/// * **begin** - 表示正在搜索文件的消息。
/// * **end** - 表示文件搜索已完成的消息。此消息还包括有关搜索的摘要统计信息。
/// * **match** - 表示找到匹配项的消息。这包括匹配的文本和偏移量。
/// * **context** - 表示找到上下文行的消息。这包括行的文本，以及如果搜索是反转的，则包括任何匹配信息。
///
/// 每个消息都以相同的信封格式进行编码，其中包含表示消息类型的标签以及有效载荷的对象：
///
/// ```json
/// {
///     "type": "{begin|end|match|context}",
///     "data": { ... }
/// }
/// ```
///
/// 消息本身在信封的`data`键中进行编码。
///
/// ## 文本编码
///
/// 在描述每种消息格式之前，我们首先必须简要讨论文本编码，因为它影响每种消息类型。
/// 特别地，JSON只能以UTF-8、UTF-16或UTF-32进行编码。对于此打印机，我们只需要考虑UTF-8。
/// 问题在于搜索不仅限于UTF-8，这又意味着可能报告包含无效UTF-8的匹配项。
/// 此外，此打印机还可能打印文件路径，文件路径的编码本身不能保证是有效的UTF-8。
/// 因此，此打印机必须以某种方式处理无效UTF-8的存在。打印机可以完全默默地忽略这种情况，
/// 或者甚至将无效UTF-8丢失地转码为有效UTF-8，方法是将所有无效序列替换为Unicode替换字符。
/// 但是，这会阻止此格式的消费者以非丢失的方式访问原始数据。
///
/// 因此，此打印机将正常发出有效的UTF-8编码字节作为JSON字符串，否则将以base64编码非有效UTF-8的数据。
/// 为了传达是否发生此过程，字符串使用`text`键表示有效的UTF-8文本，而任意字节使用`bytes`键表示。
///
/// 例如，当消息中包含路径时，如果且仅如果路径是有效的UTF-8，则格式如下：
///
/// ```json
/// {
///     "path": {
///         "text": "/home/ubuntu/lib.rs"
///     }
/// }
/// ```
///
/// 如果我们的路径是`/home/ubuntu/lib\xFF.rs`，其中`\xFF`字节使其无效的UTF-8，
/// 则路径将被编码如下：
///
/// ```json
/// {
///     "path": {
///         "bytes": "L2hvbWUvdWJ1bnR1L2xpYv8ucnM="
///     }
/// }
/// ```
///
/// 此相同的表示法也用于报告匹配项。
///
/// 打印机保证在底层字节为有效UTF-8时使用`text`字段。
///
/// ## 传输格式
///
/// 本节记录了此打印机发出的传输格式，从四种消息类型开始。
///
/// 每个消息都具有自己的格式，并且包含在指示消息类型的信封中。信封具有以下字段：
///
/// * **type** - 表示此消息类型的字符串。可能是四种可能字符串之一：`begin`、`end`、`match`或`context`。
///   此列表可能会随时间扩展。
/// * **data** - 实际消息数据。此字段的格式取决于`type`的值。可能的消息格式为
///   [`begin`](#message-begin)、
///   [`end`](#message-end)、
///   [`match`](#message-match)、
///   [`context`](#message-context)。
///
/// #### 消息：**begin**
///
/// 此消息表示搜索已经开始。它具有以下字段：
///
/// * **path** - 一个
///   [任意数据对象](#object-arbitrary-data)
///   表示与搜索对应的文件路径，如果存在的话。如果没有可用的文件路径，则此字段为`null`。
///
/// #### 消息：**end**
///
/// 此消息表示搜索已经完成。它具有以下字段：
///
/// * **path** - 一个
///   [任意数据对象](#object-arbitrary-data)
///   表示与搜索对应的文件路径，如果存在的话。如果没有可用的文件路径，则此字段为`null`。
/// * **binary_offset** - 数据搜索中检测到二进制数据的位置对应的绝对偏移量。
///   如果未检测到二进制数据（或者禁用了二进制检测），则此字段为`null`。
/// * **stats** - 包含先前搜索的摘要统计信息的[`stats`对象](#object-stats)。
///
/// #### 消息：**match**
///
/// 此消息表示找到了匹配项。匹配项通常对应于一行文本，尽管如果搜索可以在多行上发出匹配项，它也可以对应于多行。它具有以下字段：
///
/// * **path** - 一个
///   [任意数据对象](#object-arbitrary-data)
///   表示与搜索对应的文件路径，如果存在的话。如果没有可用的文件路径，则此字段为`null`。
/// * **lines** - 一个
///   [任意数据对象](#object-arbitrary-data)
///   表示此匹配项中包含的一个或多个行。
/// * **line_number** - 如果搜索器已配置为报告行号，则对应于`lines`中第一行的行号。如果没有可用的行号，则为`null`。
/// * **absolute_offset** - 对应于数据搜索中`lines`的开头的绝对字节偏移量。
/// * **submatches** - 一个包含与`lines`中的匹配项对应的[`submatch`对象](#object-submatch)数组。
///   包含在每个`submatch`中的偏移量对应于`lines`中的字节偏移量。
///   （如果`lines`是base64编码的，则字节偏移量对应于base64解码后的数据。）
///   `submatch`对象保证按其起始偏移量排序。请注意，此数组可能为空，例如在搜索报告反转匹配时。
///
/// #### 消息：**context**
///
/// 此消息表示找到了上下文行。上下文行是不包含匹配项的行，但通常与包含匹配项的行相邻。
/// 上下文行的报告方式由搜索器确定。它具有与[`match`](#message-match)中完全相同字段的字段，
/// 详细信息见下面的描述：
///
/// * **path** - 一个
///   [任意数据对象](#object-arbitrary-data)
///   表示与搜索对应的文件路径，如果存在的话。如果没有可用的文件路径，则此字段为`null`。
/// * **lines** - 一个
///   [任意数据对象](#object-arbitrary-data)
///   表示此上下文中包含的一个或多个行。这包括行终止符，如果存在的话。
/// * **line_number** - 如果搜索器已配置为报告行号，则对应于`lines`中第一行的行号。如果没有可用的行号，则为`null`。
/// * **absolute_offset** - 对应于数据搜索中`lines`的开头的绝对字节偏移量。
/// * **submatches** - 一个包含与`lines`中的匹配项对应的[`submatch`对象](#object-submatch)数组。
///   包含在每个`submatch`中的偏移量对应于`lines`中的字节偏移量。
///   （如果`lines`是base64编码的，则字节偏移量对应于base64解码后的数据。）
///   `submatch`对象保证按其起始偏移量排序。请注意，此数组可能是非空的，例如在搜索报告反转匹配时，
///   原匹配器可以匹配上下文行中的内容。
///
/// #### 对象：**submatch**
///
/// 此对象描述在`match`或`context`消息中找到的子匹配项。
/// `start`和`end`字段指示匹配项在哪个半开区间内发生（`start`包括在内，但`end`不包括在内）。
/// 保证`start <= end`。它具有以下字段：
///
/// * **match** - 一个
///   [任意数据对象](#object-arbitrary-data)
///   对应于此子匹配项中的文本。
/// * **start** - 表示此匹配项的起始字节偏移量。此偏移量通常是在父对象的数据中报告的。
///   例如，[`match`](#message-match)或[`context`](#message-context)消息中的`lines`字段。
/// * **end** - 表示此匹配项的结束字节偏移量。此偏移量通常是在父对象的数据中报告的。
///   例如，[`match`](#message-match)或[`context`](#message-context)消息中的`lines`字段。
///
/// #### 对象：**stats**
///
/// 此对象包含在消息中，包含有关搜索的摘要统计信息。
/// 它具有以下字段：
///
/// * **elapsed** - 一个描述执行搜索所经过的时间长度的[`duration`对象](#object-duration)。
/// * **searches** - 运行的搜索数量。对于此打印机，此值始终为`1`。
///   （实现可能会发出使用相同`stats`对象的其他消息类型，表示多个搜索的摘要统计信息。）
/// * **searches_with_match** - 运行的已找到至少一个匹配项的搜索数量。永远不会超过`searches`。
/// * **bytes_searched** - 已搜索的总字节数。
/// * **bytes_printed** - 已打印的总字节数。包括此打印机发出的所有内容。
/// * **matched_lines** - 参与匹配项的总行数。当匹配项可能包含多行时，这包括每个匹配项的所有行。
/// * **matches** - 总匹配项数量。每行可能有多个匹配项。当匹配项可能包含多行时，每个匹配项仅计算一次，
///   不管它跨越多少行。
///
/// #### 对象：**duration**
///
/// 此对象包括几个字段，用于描述时间长度。其中的`secs`和`nanos`字段可以在支持的系统上组合使用，
/// 以提供纳秒精度。它具有以下字段：
///
/// * **secs** - 整数秒数，表示此时间长度的长度。
/// * **nanos** - 由纳秒表示的时间长度的小数部分。如果不支持纳秒精度，则通常会将其四舍五入为最接近的纳秒数。
/// * **human** - 一个人类可读的字符串，描述时间长度。字符串的格式本身未指定。
///
/// #### 对象：**任意数据**
///
/// 此对象在需要将任意数据表示为JSON值时使用。此对象包含两个字段，通常只有一个字段存在：
///
/// * **text** - 一个普通的JSON字符串，UTF-8编码。仅在底层数据为有效UTF-8时填充此字段。
/// * **bytes** - 一个普通的JSON字符串，是底层字节的base64编码。
///
/// 更多关于此表示的动机信息，请参见上面的[文本编码](#text-encoding)部分。
///
/// ## 示例
///
/// 本节显示一个包含所有消息类型的小例子。
///
/// 这是我们要搜索的文件，位于`/home/andrew/sherlock`：
///
/// ```text
/// 对于这个世界上的华生医生，与福尔摩斯相反，在侦探工作的领域里，成功在很大程度上
/// 取决于运气。福尔摩斯可以从一根稻草或一片雪茄灰中找出线索；但是华生医生必须让别人替他拿出来，
/// 擦拭干净，并清楚地展示出来，并附有标签。
/// ```
///
/// 使用启用了行号的标准打印机搜索`华生`并带有`before_context`为`1`，结果类似于：
///
/// ```text
/// sherlock:1:对于这个世界上的华生医生，与福尔摩斯相反，在侦探工作的领域里，成功在很大程度上
/// --
/// sherlock-4-福尔摩斯可以从一根稻草或一片雪茄灰中找出线索；但是华生医生必须让别人替他拿出来，
/// sherlock:5:擦拭干净，并清楚地展示出来，并附有标签。
/// ```
///
/// 下面是相同搜索使用上述描述的JSON传输格式的结果，我们为了说明的目的显示了半格式化的JSON（而不是严格的JSON行格式）：
///
/// ```json
/// {
///   "type": "begin",
///   "data": {
///     "path": {"text": "/home/andrew/sherlock"}
///   }
/// }
/// {
///   "type": "match",
///   "data": {
///     "path": {"text": "/home/andrew/sherlock"},
///     "lines": {"text": "对于这个世界上的华生医生，与福尔摩斯相反，在侦探工作的领域里，成功在很大程度上\n"},
///     "line_number": 1,
///     "absolute_offset": 0,
///     "submatches": [
///       {"match": {"text": "华生"}, "start": 10, "end": 12}
///     ]
///   }
/// }
/// {
///   "type": "context",
///   "data": {
///     "path": {"text": "/home/andrew/sherlock"},
///     "lines": {"text": "福尔摩斯可以从一根稻草或一片雪茄灰中找出线索；但是华生医生必须让别人替他拿出来，\n"},
///     "line_number": 4,
///     "absolute_offset": 185,
///     "submatches": []
///   }
/// }
/// {
///   "type": "match",
///   "data": {
///     "path": {"text": "/home/andrew/sherlock"},
///     "lines": {"text": "擦拭干净，并清楚地展示出来，并附有标签。\n"},
///     "line_number": 5,
///     "absolute_offset": 251,
///     "submatches": [
///       {"match": {"text": "华生"}, "start": 6, "end": 8}
///     ]
///   }
/// }
/// {
///   "type": "end",
///   "data": {
///     "path": {"text": "/home/andrew/sherlock"},
///     "binary_offset": null,
///     "stats": {
///       "elapsed": {"secs": 0, "nanos": 36296, "human": "0.0000s"},
///       "searches": 1,
///       "searches_with_match": 1,
///       "bytes_searched": 367,
///       "bytes_printed": 1151,
///       "matched_lines": 2,
///       "matches": 2
///     }
///   }
/// }
/// ```
#[derive(Debug)]
pub struct JSON<W> {
    config: Config,
    wtr: CounterWriter<W>,
    matches: Vec<Match>,
}
impl<W: io::Write> JSON<W> {
    /// 创建一个具有默认配置的 JSON 行打印机，将匹配项写入给定的写入器。
    pub fn new(wtr: W) -> JSON<W> {
        JSONBuilder::new().build(wtr)
    }

    /// 返回 JSON 打印机的 `Sink` 实现。
    ///
    /// 这不会将打印机与文件路径关联，这意味着此实现永远不会连同匹配项一起打印文件路径。
    pub fn sink<'s, M: Matcher>(
        &'s mut self,
        matcher: M,
    ) -> JSONSink<'static, 's, M, W> {
        JSONSink {
            matcher: matcher,
            json: self,
            path: None,
            start_time: Instant::now(),
            match_count: 0,
            after_context_remaining: 0,
            binary_byte_offset: None,
            begin_printed: false,
            stats: Stats::new(),
        }
    }

    /// 返回与文件路径关联的 `Sink` 实现。
    ///
    /// 当打印机与路径关联时，根据其配置，它可能会打印连同找到的匹配项一起的路径。
    pub fn sink_with_path<'p, 's, M, P>(
        &'s mut self,
        matcher: M,
        path: &'p P,
    ) -> JSONSink<'p, 's, M, W>
    where
        M: Matcher,
        P: ?Sized + AsRef<Path>,
    {
        JSONSink {
            matcher: matcher,
            json: self,
            path: Some(path.as_ref()),
            start_time: Instant::now(),
            match_count: 0,
            after_context_remaining: 0,
            binary_byte_offset: None,
            begin_printed: false,
            stats: Stats::new(),
        }
    }

    /// 写入给定消息，后面跟一个换行符。换行符的确定是从给定搜索器的配置中获取的。
    fn write_message(
        &mut self,
        message: &jsont::Message<'_>,
    ) -> io::Result<()> {
        if self.config.pretty {
            json::to_writer_pretty(&mut self.wtr, message)?;
        } else {
            json::to_writer(&mut self.wtr, message)?;
        }
        self.wtr.write(&[b'\n'])?;
        Ok(())
    }
}

impl<W> JSON<W> {
    /// 如果且仅如果此打印机在以前的任何搜索期间向底层写入器写入了至少一个字节，
    /// 则返回 true。
    pub fn has_written(&self) -> bool {
        self.wtr.total_count() > 0
    }

    /// 返回对底层写入器的可变引用。
    pub fn get_mut(&mut self) -> &mut W {
        self.wtr.get_mut()
    }

    /// 消耗此打印机并返回底层写入器的所有权。
    pub fn into_inner(self) -> W {
        self.wtr.into_inner()
    }
}

/// 与 JSON 打印机关联的 `Sink` 的实现，关联了匹配器和可选的 JSON 打印机的文件路径。
///
/// 此类型是对一些类型参数的泛型：
///
/// * `'p` 指的是文件路径的生命周期，如果提供了文件路径的话。当没有提供文件路径时，这将是 `'static`。
/// * `'s` 指的是此类型借用的 [`JSON`](struct.JSON.html) 打印机的生命周期。
/// * `M` 是由 `grep_searcher::Searcher` 使用的报告结果给此 sink 的匹配器的类型。
/// * `W` 是此打印机正在将其输出写入的底层写入器的类型。
#[derive(Debug)]
pub struct JSONSink<'p, 's, M: Matcher, W> {
    matcher: M,
    json: &'s mut JSON<W>,
    path: Option<&'p Path>,
    start_time: Instant,
    match_count: u64,
    after_context_remaining: u64,
    binary_byte_offset: Option<u64>,
    begin_printed: bool,
    stats: Stats,
}

impl<'p, 's, M: Matcher, W: io::Write> JSONSink<'p, 's, M, W> {
    /// 如果且仅如果此打印机在先前的搜索中收到匹配项，则返回 true。
    ///
    /// 这不受先前搜索结果影响。
    pub fn has_match(&self) -> bool {
        self.match_count > 0
    }

    /// 返回向此 sink 报告的匹配总数。
    ///
    /// 这对应于调用 `Sink::matched` 的次数。
    pub fn match_count(&self) -> u64 {
        self.match_count
    }

    /// 如果在先前搜索中找到了二进制数据，则返回二进制数据首次检测到的偏移量。
    ///
    /// 返回的偏移量是相对于整个搜索的绝对偏移量。
    ///
    /// 这不受先前搜索结果影响。例如，如果前一次搜索发现了二进制数据，
    /// 而上一次搜索未发现二进制数据，则返回 `None`。
    pub fn binary_byte_offset(&self) -> Option<u64> {
        self.binary_byte_offset
    }

    /// 返回对打印机为此 sink 执行的所有搜索生成的统计信息的引用。
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// 在给定字节上执行匹配器并记录匹配项位置，如果当前配置要求匹配粒度的话。
    fn record_matches(
        &mut self,
        searcher: &Searcher,
        bytes: &[u8],
        range: std::ops::Range<usize>,
    ) -> io::Result<()> {
        self.json.matches.clear();
        // 如果打印需要知道每个单独匹配项的位置，则现在计算并存储这些位置以备后用。
        // 虽然这会为匹配项存储添加额外的副本，但我们对其进行了分摊分配，
        // 并且这极大地简化了打印逻辑，以至于可以确保我们不会多次搜索以查找匹配项。
        let matches = &mut self.json.matches;
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
            && matches.last().unwrap().start() >= bytes.len()
        {
            matches.pop().unwrap();
        }
        Ok(())
    }

    /// 如果此打印机应退出，则返回 true。
    ///
    /// 这实现了在达到一定数量的匹配项后退出的逻辑。
    /// 在大多数情况下，逻辑是简单的，但我们必须允许在达到限制后继续打印所有“after”上下文行。
    fn should_quit(&self) -> bool {
        let limit = match self.json.config.max_matches {
            None => return false,
            Some(limit) => limit,
        };
        if self.match_count < limit {
            return false;
        }
        self.after_context_remaining == 0
    }

    /// 返回当前匹配计数是否超过配置的限制。
    /// 如果没有限制，这将始终返回 false。
    fn match_more_than_limit(&self) -> bool {
        let limit = match self.json.config.max_matches {
            None => return false,
            Some(limit) => limit,
        };
        self.match_count > limit
    }

    /// 写入“begin”消息。
    fn write_begin_message(&mut self) -> io::Result<()> {
        if self.begin_printed {
            return Ok(());
        }
        let msg = jsont::Message::Begin(jsont::Begin { path: self.path });
        self.json.write_message(&msg)?;
        self.begin_printed = true;
        Ok(())
    }
}
impl<'p, 's, M: Matcher, W: io::Write> Sink for JSONSink<'p, 's, M, W> {
    type Error = io::Error;

    // 当匹配项被找到时调用，用于处理匹配项的输出
    fn matched(
        &mut self,
        searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, io::Error> {
        self.write_begin_message()?;

        self.match_count += 1;
        // 当匹配计数超过限制时，剩余上下文行的数量不应该重置，而是减少。
        // 这避免了一个 bug，该 bug 会显示多于配置限制的匹配项。
        // 主要思想是，在打印 after 上下文行时，可能会再次调用 'matched'。
        // 在这种情况下，我们应该将其视为上下文行，而不是匹配行，用于终止。
        if self.match_more_than_limit() {
            self.after_context_remaining =
                self.after_context_remaining.saturating_sub(1);
        } else {
            self.after_context_remaining = searcher.after_context() as u64;
        }

        self.record_matches(
            searcher,
            mat.buffer(),
            mat.bytes_range_in_buffer(),
        )?;
        self.stats.add_matches(self.json.matches.len() as u64);
        self.stats.add_matched_lines(mat.lines().count() as u64);

        let submatches = SubMatches::new(mat.bytes(), &self.json.matches);
        let msg = jsont::Message::Match(jsont::Match {
            path: self.path,
            lines: mat.bytes(),
            line_number: mat.line_number(),
            absolute_offset: mat.absolute_byte_offset(),
            submatches: submatches.as_slice(),
        });
        self.json.write_message(&msg)?;
        Ok(!self.should_quit())
    }

    // 当上下文被找到时调用，用于处理上下文的输出
    fn context(
        &mut self,
        searcher: &Searcher,
        ctx: &SinkContext<'_>,
    ) -> Result<bool, io::Error> {
        self.write_begin_message()?;
        self.json.matches.clear();

        if ctx.kind() == &SinkContextKind::After {
            self.after_context_remaining =
                self.after_context_remaining.saturating_sub(1);
        }
        let submatches = if searcher.invert_match() {
            self.record_matches(searcher, ctx.bytes(), 0..ctx.bytes().len())?;
            SubMatches::new(ctx.bytes(), &self.json.matches)
        } else {
            SubMatches::empty()
        };
        let msg = jsont::Message::Context(jsont::Context {
            path: self.path,
            lines: ctx.bytes(),
            line_number: ctx.line_number(),
            absolute_offset: ctx.absolute_byte_offset(),
            submatches: submatches.as_slice(),
        });
        self.json.write_message(&msg)?;
        Ok(!self.should_quit())
    }

    // 当搜索开始时调用，用于初始化计数和配置
    fn begin(&mut self, _searcher: &Searcher) -> Result<bool, io::Error> {
        self.json.wtr.reset_count();
        self.start_time = Instant::now();
        self.match_count = 0;
        self.after_context_remaining = 0;
        self.binary_byte_offset = None;
        if self.json.config.max_matches == Some(0) {
            return Ok(false);
        }

        if !self.json.config.always_begin_end {
            return Ok(true);
        }
        self.write_begin_message()?;
        Ok(true)
    }

    // 当搜索结束时调用，用于输出搜索统计信息和结束消息
    fn finish(
        &mut self,
        _searcher: &Searcher,
        finish: &SinkFinish,
    ) -> Result<(), io::Error> {
        if !self.begin_printed {
            return Ok(());
        }

        self.binary_byte_offset = finish.binary_byte_offset();
        self.stats.add_elapsed(self.start_time.elapsed());
        self.stats.add_searches(1);
        if self.match_count > 0 {
            self.stats.add_searches_with_match(1);
        }
        self.stats.add_bytes_searched(finish.byte_count());
        self.stats.add_bytes_printed(self.json.wtr.count());

        let msg = jsont::Message::End(jsont::End {
            path: self.path,
            binary_offset: finish.binary_byte_offset(),
            stats: self.stats.clone(),
        });
        self.json.write_message(&msg)?;
        Ok(())
    }
}

// SubMatches 表示一系列在连续字节范围内的匹配项。
//
// 一个更简单的表示方法只是简单地 `Vec<SubMatch>`，但常见情况是每个字节范围只有一个匹配项，
// 我们在这里使用一个固定大小的数组来进行优化，而不需要任何分配。
enum SubMatches<'a> {
    Empty,
    Small([jsont::SubMatch<'a>; 1]),
    Big(Vec<jsont::SubMatch<'a>>),
}

impl<'a> SubMatches<'a> {
    /// 从一组匹配项和这些匹配项应用的对应字节创建一组新的匹配范围。
    fn new(bytes: &'a [u8], matches: &[Match]) -> SubMatches<'a> {
        if matches.len() == 1 {
            let mat = matches[0];
            SubMatches::Small([jsont::SubMatch {
                m: &bytes[mat],
                start: mat.start(),
                end: mat.end(),
            }])
        } else {
            let mut match_ranges = vec![];
            for &mat in matches {
                match_ranges.push(jsont::SubMatch {
                    m: &bytes[mat],
                    start: mat.start(),
                    end: mat.end(),
                });
            }
            SubMatches::Big(match_ranges)
        }
    }

    /// 创建一个空的匹配范围集合。
    fn empty() -> SubMatches<'static> {
        SubMatches::Empty
    }

    /// 将此匹配范围集合作为切片返回。
    fn as_slice(&self) -> &[jsont::SubMatch<'_>] {
        match *self {
            SubMatches::Empty => &[],
            SubMatches::Small(ref x) => x,
            SubMatches::Big(ref x) => x,
        }
    }
}
#[cfg(test)]
mod tests {
    use grep_matcher::LineTerminator;
    use grep_regex::{RegexMatcher, RegexMatcherBuilder};
    use grep_searcher::SearcherBuilder;

    use super::{JSONBuilder, JSON};

    // 常量：福尔摩斯短文本
    const SHERLOCK: &'static [u8] = b"\
    For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
be, to a very large extent, the result of luck. Sherlock Holmes
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.
    ";

    // 获取打印机内容的辅助函数
    fn printer_contents(printer: &mut JSON<Vec<u8>>) -> String {
        String::from_utf8(printer.get_mut().to_owned()).unwrap()
    }

    // 测试：二进制数据检测
    #[test]
    fn binary_detection() {
        use grep_searcher::BinaryDetection;

        // 二进制文本
        const BINARY: &'static [u8] = b"\
       For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
be, to a very large extent, the result of luck. Sherlock Holmes
can extract a clew \x00 from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.\
        ";

        // 正则匹配器
        let matcher = RegexMatcher::new(r"Watson").unwrap();
        // 构建 JSON 打印机
        let mut printer = JSONBuilder::new().build(vec![]);
        // 创建搜索器
        SearcherBuilder::new()
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .heap_limit(Some(80))
            .build()
            .search_reader(&matcher, BINARY, printer.sink(&matcher))
            .unwrap();
        // 获取打印机输出内容
        let got = printer_contents(&mut printer);

        assert_eq!(got.lines().count(), 3);
        let last = got.lines().last().unwrap();
        assert!(last.contains(r#""binary_offset":212,"#));
    }

    // 测试：最大匹配项数限制
    #[test]
    fn max_matches() {
        let matcher = RegexMatcher::new(r"Watson").unwrap();
        let mut printer =
            JSONBuilder::new().max_matches(Some(1)).build(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(&matcher, SHERLOCK, printer.sink(&matcher))
            .unwrap();
        let got = printer_contents(&mut printer);

        assert_eq!(got.lines().count(), 3);
    }

    // 测试：最大匹配项数限制（包含上下文）
    #[test]
    fn max_matches_after_context() {
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
        let matcher = RegexMatcher::new(r"d").unwrap();
        let mut printer =
            JSONBuilder::new().max_matches(Some(1)).build(vec![]);
        SearcherBuilder::new()
            .after_context(2)
            .build()
            .search_reader(
                &matcher,
                haystack.as_bytes(),
                printer.sink(&matcher),
            )
            .unwrap();
        let got = printer_contents(&mut printer);

        assert_eq!(got.lines().count(), 5);
    }

    // 测试：无匹配项
    #[test]
    fn no_match() {
        let matcher = RegexMatcher::new(r"DOES NOT MATCH").unwrap();
        let mut printer = JSONBuilder::new().build(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(&matcher, SHERLOCK, printer.sink(&matcher))
            .unwrap();
        let got = printer_contents(&mut printer);

        assert!(got.is_empty());
    }

    // 测试：总是输出起始和结束信息（无匹配项）
    #[test]
    fn always_begin_end_no_match() {
        let matcher = RegexMatcher::new(r"DOES NOT MATCH").unwrap();
        let mut printer =
            JSONBuilder::new().always_begin_end(true).build(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(&matcher, SHERLOCK, printer.sink(&matcher))
            .unwrap();
        let got = printer_contents(&mut printer);

        assert_eq!(got.lines().count(), 2);
        assert!(got.contains("begin") && got.contains("end"));
    }

    // 测试：缺少 CRLF
    #[test]
    fn missing_crlf() {
        let haystack = "test\r\n".as_bytes();

        let matcher = RegexMatcherBuilder::new().build("test").unwrap();
        let mut printer = JSONBuilder::new().build(vec![]);
        SearcherBuilder::new()
            .build()
            .search_reader(&matcher, haystack, printer.sink(&matcher))
            .unwrap();
        let got = printer_contents(&mut printer);
        assert_eq!(got.lines().count(), 3);
        assert!(
            got.lines().nth(1).unwrap().contains(r"test\r\n"),
            r"missing 'test\r\n' in '{}'",
            got.lines().nth(1).unwrap(),
        );

        let matcher =
            RegexMatcherBuilder::new().crlf(true).build("test").unwrap();
        let mut printer = JSONBuilder::new().build(vec![]);
        SearcherBuilder::new()
            .line_terminator(LineTerminator::crlf())
            .build()
            .search_reader(&matcher, haystack, printer.sink(&matcher))
            .unwrap();
        let got = printer_contents(&mut printer);
        assert_eq!(got.lines().count(), 3);
        assert!(
            got.lines().nth(1).unwrap().contains(r"test\r\n"),
            r"missing 'test\r\n' in '{}'",
            got.lines().nth(1).unwrap(),
        );
    }
}
