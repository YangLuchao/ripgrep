/*!
这个 crate 提供了与 [`grep-searcher`](https://docs.rs/grep-searcher) crate 互操作的功能丰富且高效的打印机。

# 简要概述

[`Standard`](struct.Standard.html) 打印机以人类可读的格式显示结果，模仿了标准 grep 类似工具使用的格式。
其特性包括但不限于，跨平台终端着色、搜索与替换、多行结果处理以及报告摘要统计信息。

[`JSON`](struct.JSON.html) 打印机以机器可读的格式显示结果。为了方便搜索结果的流式处理，该格式使用
[JSON Lines](https://jsonlines.org/)，
通过在发现搜索结果时发出一系列消息来呈现搜索结果。

[`Summary`](struct.Summary.html) 打印机以人类可读的格式显示单个搜索的*聚合*结果，模仿了标准 grep 类似工具中发现的类似格式。
这个打印机对于显示匹配总数和/或打印包含或不包含匹配项的文件路径非常有用。

# 示例

这个示例展示了如何创建一个“标准”打印机并执行搜索。

```
use std::error::Error;

use grep_regex::RegexMatcher;
use grep_printer::Standard;
use grep_searcher::Searcher;

const SHERLOCK: &'static [u8] = b"\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
be, to a very large extent, the result of luck. Sherlock Holmes
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.
";

# fn main() { example().unwrap(); }
fn example() -> Result<(), Box<Error>> {
    let matcher = RegexMatcher::new(r"Sherlock")?;
    let mut printer = Standard::new_no_color(vec![]);
    Searcher::new().search_slice(&matcher, SHERLOCK, printer.sink(&matcher))?;

    // into_inner 给出了我们提供给 new_no_color 的底层写入器，它被包装在 termcolor::NoColor 中。
    // 因此，第二次的 into_inner 给出了实际的缓冲区。
    let output = String::from_utf8(printer.into_inner().into_inner())?;
    let expected = "\
1:For the Doctor Watsons of this world, as opposed to the Sherlock
3:be, to a very large extent, the result of luck. Sherlock Holmes
";
    assert_eq!(output, expected);
    Ok(())
}
```
*/

#![deny(missing_docs)]

pub use crate::color::{
    default_color_specs, ColorError, ColorSpecs, UserColorSpec,
};
#[cfg(feature = "serde1")]
pub use crate::json::{JSONBuilder, JSONSink, JSON};
pub use crate::standard::{Standard, StandardBuilder, StandardSink};
pub use crate::stats::Stats;
pub use crate::summary::{Summary, SummaryBuilder, SummaryKind, SummarySink};
pub use crate::util::PrinterPath;

// 用于执行搜索的最大字节数，以考虑前瞻。
//
// 这是一个不幸的权宜之计，因为 PCRE2 不提供一种在考虑前瞻的情况下搜索输入的子字符串的方法。
// 理论上，我们可以重构各种'grep'接口来考虑它，但这将是一个大的变化。因此，目前我们只允许PCRE2稍微查找一下匹配项，而不搜索整个剩余内容。
//
// 请注意，此权宜之计仅在多行模式下生效。
const MAX_LOOK_AHEAD: usize = 128;

#[macro_use]
mod macros;

mod color;
mod counter;
#[cfg(feature = "serde1")]
mod json;
#[cfg(feature = "serde1")]
mod jsont;
mod standard;
mod stats;
mod summary;
mod util;
