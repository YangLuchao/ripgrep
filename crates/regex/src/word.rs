use std::{
    collections::HashMap,
    panic::{RefUnwindSafe, UnwindSafe},
    sync::Arc,
};

use {
    grep_matcher::{Match, Matcher, NoError},
    regex_automata::{
        meta::Regex, util::captures::Captures, util::pool::Pool, Input,
        PatternID,
    },
};

use crate::{config::ConfiguredHIR, error::Error, matcher::RegexCaptures};

type PoolFn =
    Box<dyn Fn() -> Captures + Send + Sync + UnwindSafe + RefUnwindSafe>;
/// 实现“单词匹配”语义的匹配器。
#[derive(Debug)]
pub(crate) struct WordMatcher {
    /// 大致匹配正则表达式，即 `(?:^|\W)(<原始模式>)(?:$|\W)`。
    regex: Regex,
    /// 生成上述正则表达式的 HIR。我们不保留 `original` 正则表达式的 HIR。
    ///
    /// 我们将其放入 `Arc` 中，因为在它到达这里时，不会再更改。
    /// 并且由于其深度递归表示，克隆和丢弃 `Hir` 相对较昂贵。
    chir: Arc<ConfiguredHIR>,
    /// 用户提供的原始正则表达式，我们在快速路径中使用它来尝试检测匹配，然后再退回到较慢的引擎。
    original: Regex,
    /// 从捕获组名称到捕获组索引的映射。
    names: HashMap<String, usize>,
    /// 用于查找内部组的匹配偏移量的可重用缓冲区的线程安全池。
    caps: Arc<Pool<Captures, PoolFn>>,
}

impl Clone for WordMatcher {
    fn clone(&self) -> WordMatcher {
        // 我们手动实现 Clone，以便获得一个新的 Pool，以便它可以设置自己的线程所有者。
        // 这允许每个使用 `caps` 的线程进入快速路径。
        //
        // 请注意，克隆正则表达式是“便宜”的，因为它在内部使用引用计数。
        let re = self.regex.clone();
        WordMatcher {
            regex: self.regex.clone(),
            chir: Arc::clone(&self.chir),
            original: self.original.clone(),
            names: self.names.clone(),
            caps: Arc::new(Pool::new(Box::new(move || re.create_captures()))),
        }
    }
}

impl WordMatcher {
    /// 从给定的模式创建一个新的匹配器，该匹配器仅生成被视为“单词”的匹配项。
    ///
    /// 给定的选项用于在内部构建正则表达式。
    pub(crate) fn new(chir: ConfiguredHIR) -> Result<WordMatcher, Error> {
        let original = chir.clone().into_anchored().to_regex()?;
        let chir = Arc::new(chir.into_word()?);
        let regex = chir.to_regex()?;
        let caps = Arc::new(Pool::new({
            let regex = regex.clone();
            Box::new(move || regex.create_captures()) as PoolFn
        }));

        let mut names = HashMap::new();
        let it = regex.group_info().pattern_names(PatternID::ZERO);
        for (i, optional_name) in it.enumerate() {
            if let Some(name) = optional_name {
                names.insert(name.to_string(), i.checked_sub(1).unwrap());
            }
        }
        Ok(WordMatcher { regex, chir, original, names, caps })
    }

    /// 返回用于在词边界进行匹配的基础正则表达式。
    ///
    /// 原始正则表达式位于索引为 1 的捕获组中。
    pub(crate) fn regex(&self) -> &Regex {
        &self.regex
    }

    /// 返回用于在词边界进行匹配的基础 HIR。
    pub(crate) fn chir(&self) -> &ConfiguredHIR {
        &self.chir
    }

    /// 尝试对一部分（但希望是大部分）常见情况进行快速匹配确认。
    /// 当找到匹配时返回 Ok(Some(..))。当确定没有匹配时返回 Ok(None)。
    /// 当无法检测到是否存在匹配时返回 Err(())。
    fn fast_find(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<Option<Match>, ()> {
        // 这有点复杂。整体上的目标是避免运行更慢的正则表达式引擎来提取捕获组。
        // 请记住，我们的单词正则表达式如下所示：
        //
        //     (^|\W)(<原始正则表达式>)(\W|$)
        //
        // 我们想要的是 <原始正则表达式> 的匹配偏移量。因此，在易于常见情况下，
        // 原始正则表达式将位于两个在 \W 类中的代码点之间。
        // 因此，我们在这里的方法是查找整体单词正则表达式的匹配，
        // 剥离掉两端的 \W 并检查原始正则表达式是否匹配剩余部分。
        // 如果匹配，我们将确保获得正确的匹配。
        //
        // 这仅在我们知道匹配位于两个 \W 代码点之间时才有效。
        // 这仅在既没有 ^ 也没有 $ 匹配时发生。
        // 这又仅在匹配位于文本开头或末尾时发生。在这两种情况下，我们宣布失败，
        // 并退回到较慢的实现。
        //
        // 我们不能在此处处理 ^/$ 的原因是我们无法对原始模式做出任何假设。
        // （尝试取消注释下面的 ^/$ 检查，然后运行测试以查看示例。）
        //
        // 注（2023-07-31）：在修复 #2574 之后，此逻辑似乎仍然不正确。正则表达式的组合很困难。
        let input = Input::new(haystack).span(at..haystack.len());
        let mut cand = match self.regex.find(input) {
            None => return Ok(None),
            Some(m) => Match::new(m.start(), m.end()),
        };
        if cand.start() == 0 || cand.end() == haystack.len() {
            return Err(());
        }
        // 我们解码匹配前后的字符。
        // 如果任一字符是单词字符，那么意味着 ^/$ 匹配而不是 \W。
        // 在这种情况下，我们退回到较慢的引擎。
        let (ch, slen) = bstr::decode_utf8(&haystack[cand]);
        if ch.map_or(true, regex_syntax::is_word_character) {
            return Err(());
        }
        let (ch, elen) = bstr::decode_last_utf8(&haystack[cand]);
        if ch.map_or(true, regex_syntax::is_word_character) {
            return Err(());
        }
        let new_start = cand.start() + slen;
        let new_end = cand.end() - elen;
        // 这发生在原始正则表达式可以匹配空字符串的情况下。
        // 在这种情况下，只需放弃，而不是尝试在这里正确处理它，因为它可能是病态情况。
        if new_start > new_end {
            return Err(());
        }
        cand = cand.with_start(new_start).with_end(new_end);
        if self.original.is_match(&haystack[cand]) {
            Ok(Some(cand))
        } else {
            Err(())
        }
    }
}

impl Matcher for WordMatcher {
    type Captures = RegexCaptures;
    type Error = NoError;

    // 在给定位置尝试查找匹配项。
    fn find_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<Option<Match>, NoError> {
        // 为了确保易于正确实现，我们在这里提取捕获组，而不是调用 `find_at`。
        // 实际匹配位于捕获组 `1` 而不是 `0`。我们*可以*在这里使用 `find_at`，
        // 然后在事后修剪匹配项，但那会更难以正确实现，并且不清楚是否值得。
        //
        // 好吧，事实证明确实值得！但它非常棘手。请参见 `fast_find` 的详细信息。
        // 实际上，这让我们能够在绝大多数情况下跳过运行较慢的正则表达式引擎来提取捕获组。
        // 然而，我认为完全正确需要较慢的引擎。
        match self.fast_find(haystack, at) {
            Ok(Some(m)) => return Ok(Some(m)),
            Ok(None) => return Ok(None),
            Err(()) => {}
        }

        let input = Input::new(haystack).span(at..haystack.len());
        let mut caps = self.caps.get();
        self.regex.search_captures(&input, &mut caps);
        Ok(caps.get_group(1).map(|sp| Match::new(sp.start, sp.end)))
    }

    // 创建一个新的捕获组实例。
    fn new_captures(&self) -> Result<RegexCaptures, NoError> {
        Ok(RegexCaptures::with_offset(self.regex.create_captures(), 1))
    }

    // 返回捕获组的数量。
    fn capture_count(&self) -> usize {
        self.regex.captures_len().checked_sub(1).unwrap()
    }

    // 返回给定名称的捕获组索引。
    fn capture_index(&self, name: &str) -> Option<usize> {
        self.names.get(name).map(|i| *i)
    }

    // 在给定位置尝试获取捕获结果，更新到提供的捕获组实例中。
    fn captures_at(
        &self,
        haystack: &[u8],
        at: usize,
        caps: &mut RegexCaptures,
    ) -> Result<bool, NoError> {
        let input = Input::new(haystack).span(at..haystack.len());
        let caps = caps.captures_mut();
        self.regex.search_captures(&input, caps);
        Ok(caps.is_match())
    }

    // 我们故意不实现其他方法，如 find_iter 或 captures_iter。
    // 实际上，通过实现上述的 find_at 和 captures_at 方法，保证了这些迭代器方法的正确性。
}

#[cfg(test)]
mod tests {
    use super::WordMatcher;
    use crate::config::Config;
    use grep_matcher::{Captures, Match, Matcher};

    fn matcher(pattern: &str) -> WordMatcher {
        let chir = Config::default().build_many(&[pattern]).unwrap();
        WordMatcher::new(chir).unwrap()
    }

    fn find(pattern: &str, haystack: &str) -> Option<(usize, usize)> {
        matcher(pattern)
            .find(haystack.as_bytes())
            .unwrap()
            .map(|m| (m.start(), m.end()))
    }

    fn find_by_caps(pattern: &str, haystack: &str) -> Option<(usize, usize)> {
        let m = matcher(pattern);
        let mut caps = m.new_captures().unwrap();
        if !m.captures(haystack.as_bytes(), &mut caps).unwrap() {
            None
        } else {
            caps.get(0).map(|m| (m.start(), m.end()))
        }
    }

    // Test that the standard `find` API reports offsets correctly.
    #[test]
    fn various_find() {
        assert_eq!(Some((0, 3)), find(r"foo", "foo"));
        assert_eq!(Some((0, 3)), find(r"foo", "foo("));
        assert_eq!(Some((1, 4)), find(r"foo", "!foo("));
        assert_eq!(None, find(r"foo", "!afoo("));

        assert_eq!(Some((0, 3)), find(r"foo", "foo☃"));
        assert_eq!(None, find(r"foo", "fooб"));

        assert_eq!(Some((0, 4)), find(r"foo5", "foo5"));
        assert_eq!(None, find(r"foo", "foo5"));

        assert_eq!(Some((1, 4)), find(r"foo", "!foo!"));
        assert_eq!(Some((1, 5)), find(r"foo!", "!foo!"));
        assert_eq!(Some((0, 5)), find(r"!foo!", "!foo!"));

        assert_eq!(Some((0, 3)), find(r"foo", "foo\n"));
        assert_eq!(Some((1, 4)), find(r"foo", "!foo!\n"));
        assert_eq!(Some((1, 5)), find(r"foo!", "!foo!\n"));
        assert_eq!(Some((0, 5)), find(r"!foo!", "!foo!\n"));

        assert_eq!(Some((1, 6)), find(r"!?foo!?", "!!foo!!"));
        assert_eq!(Some((0, 5)), find(r"!?foo!?", "!foo!"));
        assert_eq!(Some((2, 5)), find(r"!?foo!?", "a!foo!a"));

        assert_eq!(Some((2, 7)), find(r"!?foo!?", "##!foo!\n"));
        assert_eq!(Some((3, 8)), find(r"!?foo!?", "##\n!foo!##"));
        assert_eq!(Some((3, 8)), find(r"!?foo!?", "##\n!foo!\n##"));
        assert_eq!(Some((3, 7)), find(r"f?oo!?", "##\nfoo!##"));
        assert_eq!(Some((2, 5)), find(r"(?-u)foo[^a]*", "#!foo☃aaa"));
    }

    // See: https://github.com/BurntSushi/ripgrep/issues/389
    #[test]
    fn regression_dash() {
        assert_eq!(Some((0, 2)), find(r"-2", "-2"));
    }

    // Test that the captures API also reports offsets correctly, just as
    // find does. This exercises a different path in the code since captures
    // are handled differently.
    #[test]
    fn various_captures() {
        assert_eq!(Some((0, 3)), find_by_caps(r"foo", "foo"));
        assert_eq!(Some((0, 3)), find_by_caps(r"foo", "foo("));
        assert_eq!(Some((1, 4)), find_by_caps(r"foo", "!foo("));
        assert_eq!(None, find_by_caps(r"foo", "!afoo("));

        assert_eq!(Some((0, 3)), find_by_caps(r"foo", "foo☃"));
        assert_eq!(None, find_by_caps(r"foo", "fooб"));
        // assert_eq!(Some((0, 3)), find_by_caps(r"foo", "fooб"));

        // See: https://github.com/BurntSushi/ripgrep/issues/389
        assert_eq!(Some((0, 2)), find_by_caps(r"-2", "-2"));
    }

    // Test that the capture reporting methods work as advertised.
    #[test]
    fn capture_indexing() {
        let m = matcher(r"(a)(?P<foo>b)(c)");
        assert_eq!(4, m.capture_count());
        assert_eq!(Some(2), m.capture_index("foo"));

        let mut caps = m.new_captures().unwrap();
        assert_eq!(4, caps.len());

        assert!(m.captures(b"abc", &mut caps).unwrap());
        assert_eq!(caps.get(0), Some(Match::new(0, 3)));
        assert_eq!(caps.get(1), Some(Match::new(0, 1)));
        assert_eq!(caps.get(2), Some(Match::new(1, 2)));
        assert_eq!(caps.get(3), Some(Match::new(2, 3)));
        assert_eq!(caps.get(4), None);

        assert!(m.captures(b"#abc#", &mut caps).unwrap());
        assert_eq!(caps.get(0), Some(Match::new(1, 4)));
        assert_eq!(caps.get(1), Some(Match::new(1, 2)));
        assert_eq!(caps.get(2), Some(Match::new(2, 3)));
        assert_eq!(caps.get(3), Some(Match::new(3, 4)));
        assert_eq!(caps.get(4), None);
    }
}
