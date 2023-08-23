/*!
ripgrep，作为一个库。

这个库旨在为组成ripgrep核心搜索例程的crate提供一个高级外观。然而，目前还没有高级文档指导用户如何将所有组件组合在一起。

组成crate中的每个公共API项都有文档，但示例很少。

计划有一本教程和指南。
*/

pub extern crate grep_cli as cli;
pub extern crate grep_matcher as matcher;
#[cfg(feature = "pcre2")]
pub extern crate grep_pcre2 as pcre2;
pub extern crate grep_printer as printer;
pub extern crate grep_regex as regex;
pub extern crate grep_searcher as searcher;
