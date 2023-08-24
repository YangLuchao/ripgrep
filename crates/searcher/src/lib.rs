/*!
这个 crate 提供了基于行的搜索实现，支持可选的多行搜索。

# 简要概述

这个 crate 中的主要类型是 [`Searcher`](struct.Searcher.html)，可以通过
[`SearcherBuilder`](struct.SearcherBuilder.html) 进行配置和构建。
`Searcher` 负责从源（例如文件）中读取字节，使用 `Matcher`（例如正则表达式）
执行字节搜索，然后将搜索结果报告给 `Sink`（例如 stdout）。
`Searcher` 主要负责管理从源消耗字节，并以高效的方式对这些字节应用 `Matcher`。
`Searcher` 还负责反转搜索、计算行数、报告上下文行、检测二进制数据，
甚至决定是否使用内存映射。

`Matcher`（在 [`grep-matcher`](https://crates.io/crates/grep-matcher) crate 中定义）是描述泛型模式搜索的最底层的 trait。
接口本身与正则表达式的接口非常相似。
例如，[`grep-regex`](https://crates.io/crates/grep-regex) crate 提供了使用 Rust 的
[`regex`](https://crates.io/crates/regex) crate 实现的 `Matcher` trait。

最后，`Sink` 描述了调用者如何接收由 `Searcher` 产生的搜索结果。
这包括在搜索开始和结束时调用的例程，以及在 `Searcher` 找到匹配或上下文行时调用的例程。
`Sink` 的实现可以非常简单，也可以非常复杂，比如
[`grep-printer`](https://crates.io/crates/grep-printer) crate 中的 `Standard` 打印机，
它实际上实现了类似 grep 的输出。
此 crate 还在 [`sinks`](sinks/index.html) 子模块中提供了方便的 `Sink` 实现，可使用闭包轻松搜索。

# 示例

以下示例展示了如何执行搜索器并读取搜索结果，使用
[`UTF8`](sinks/struct.UTF8.html)
作为 `Sink` 的实现。

```
use std::error::Error;

use grep_matcher::Matcher;
use grep_regex::RegexMatcher;
use grep_searcher::Searcher;
use grep_searcher::sinks::UTF8;

const SHERLOCK: &'static [u8] = b"\
For the Doctor Watsons of this world, as opposed to the Sherlock
Holmeses, success in the province of detective work must always
be, to a very large extent, the result of luck. Sherlock Holmes
can extract a clew from a wisp of straw or a flake of cigar ash;
but Doctor Watson has to have it taken out for him and dusted,
and exhibited clearly, with a label attached.
";

# fn main() { example().unwrap() }
fn example() -> Result<(), Box<Error>> {
    let matcher = RegexMatcher::new(r"Doctor \w+")?;
    let mut matches: Vec<(u64, String)> = vec![];
    Searcher::new().search_slice(&matcher, SHERLOCK, UTF8(|lnum, line| {
        // 我们保证会找到匹配，所以 unwrap 是安全的。
        let mymatch = matcher.find(line.as_bytes())?.unwrap();
        matches.push((lnum, line[mymatch].to_string()));
        Ok(true)
    }))?;

    assert_eq!(matches.len(), 2);
    assert_eq!(
        matches[0],
        (1, "Doctor Watsons".to_string())
    );
    assert_eq!(
        matches[1],
        (5, "Doctor Watson".to_string())
    );
    Ok(())
}
```

另请参阅位于此 crate 根目录下的 `examples/search-stdin.rs`，这是一个类似的示例，
它在命令行上接受一个模式并在标准输入上进行搜索。
*/

#![deny(missing_docs)]

pub use crate::lines::{LineIter, LineStep};
pub use crate::searcher::{
    BinaryDetection, ConfigError, Encoding, MmapChoice, Searcher,
    SearcherBuilder,
};
pub use crate::sink::sinks;
pub use crate::sink::{
    Sink, SinkContext, SinkContextKind, SinkError, SinkFinish, SinkMatch,
};

#[macro_use]
mod macros;

mod line_buffer;
mod lines;
mod searcher;
mod sink;
#[cfg(test)]
mod testutil;
