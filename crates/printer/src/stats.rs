use std::ops::{Add, AddAssign};
use std::time::Duration;

use crate::util::NiceDuration;

/// 在搜索结束时产生的汇总统计信息。
///
/// 当打印机报告统计信息时，它们对应于使用该打印机执行的所有搜索。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde1", derive(serde::Serialize))]
pub struct Stats {
    elapsed: NiceDuration,    // 经过的时间
    searches: u64,            // 执行的搜索次数
    searches_with_match: u64, // 执行搜索且至少找到一个匹配的次数
    bytes_searched: u64,      // 搜索的总字节数
    bytes_printed: u64,       // 打印的总字节数
    matched_lines: u64,       // 参与匹配的总行数
    matches: u64,             // 总匹配数
}

impl Add for Stats {
    type Output = Stats;

    fn add(self, rhs: Stats) -> Stats {
        self + &rhs
    }
}

impl<'a> Add<&'a Stats> for Stats {
    type Output = Stats;

    fn add(self, rhs: &'a Stats) -> Stats {
        Stats {
            elapsed: NiceDuration(self.elapsed.0 + rhs.elapsed.0),
            searches: self.searches + rhs.searches,
            searches_with_match: self.searches_with_match
                + rhs.searches_with_match,
            bytes_searched: self.bytes_searched + rhs.bytes_searched,
            bytes_printed: self.bytes_printed + rhs.bytes_printed,
            matched_lines: self.matched_lines + rhs.matched_lines,
            matches: self.matches + rhs.matches,
        }
    }
}

impl AddAssign for Stats {
    fn add_assign(&mut self, rhs: Stats) {
        *self += &rhs;
    }
}

impl<'a> AddAssign<&'a Stats> for Stats {
    fn add_assign(&mut self, rhs: &'a Stats) {
        self.elapsed.0 += rhs.elapsed.0;
        self.searches += rhs.searches;
        self.searches_with_match += rhs.searches_with_match;
        self.bytes_searched += rhs.bytes_searched;
        self.bytes_printed += rhs.bytes_printed;
        self.matched_lines += rhs.matched_lines;
        self.matches += rhs.matches;
    }
}

impl Stats {
    /// 返回一个用于跟踪搜索之间的汇总统计信息的新值。
    ///
    /// 所有统计信息都设置为 `0`。
    pub fn new() -> Stats {
        Stats::default()
    }

    /// 返回总经过的时间。
    pub fn elapsed(&self) -> Duration {
        self.elapsed.0
    }

    /// 返回执行的总搜索次数。
    pub fn searches(&self) -> u64 {
        self.searches
    }

    /// 返回找到至少一个匹配的总搜索次数。
    pub fn searches_with_match(&self) -> u64 {
        self.searches_with_match
    }

    /// 返回总搜索的字节数。
    pub fn bytes_searched(&self) -> u64 {
        self.bytes_searched
    }

    /// 返回总打印的字节数。
    pub fn bytes_printed(&self) -> u64 {
        self.bytes_printed
    }

    /// 返回参与匹配的总行数。
    ///
    /// 当匹配可能包含多行时，这包括每个匹配的每行。
    pub fn matched_lines(&self) -> u64 {
        self.matched_lines
    }

    /// 返回总匹配数。
    ///
    /// 一行中可能有多个匹配。
    pub fn matches(&self) -> u64 {
        self.matches
    }

    /// 增加经过的时间。
    pub fn add_elapsed(&mut self, duration: Duration) {
        self.elapsed.0 += duration;
    }

    /// 增加执行的搜索次数。
    pub fn add_searches(&mut self, n: u64) {
        self.searches += n;
    }

    /// 增加找到至少一个匹配的搜索次数。
    pub fn add_searches_with_match(&mut self, n: u64) {
        self.searches_with_match += n;
    }

    /// 增加总搜索的字节数。
    pub fn add_bytes_searched(&mut self, n: u64) {
        self.bytes_searched += n;
    }

    /// 增加总打印的字节数。
    pub fn add_bytes_printed(&mut self, n: u64) {
        self.bytes_printed += n;
    }

    /// 增加参与匹配的总行数。
    pub fn add_matched_lines(&mut self, n: u64) {
        self.matched_lines += n;
    }

    /// 增加总匹配数。
    pub fn add_matches(&mut self, n: u64) {
        self.matches += n;
    }
}
