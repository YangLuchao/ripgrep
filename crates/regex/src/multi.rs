use aho_corasick::{AhoCorasick, MatchKind};
use grep_matcher::{Match, Matcher, NoError};
use regex_syntax::hir::{Hir, HirKind};

use crate::error::Error;
use crate::matcher::RegexCaptures;

/// 一个用于替代多个字面值的匹配器。
///
/// 理想情况下，这种优化应该被推入正则表达式引擎中，但是要在那里正确实现这个优化需要进行相当多的重构。
/// 此外，将其放在上面一层让我们可以做一些诸如“如果我们只想要搜索字面值，那么根本不需要进行正则表达式解析”之类的事情。
#[derive(Clone, Debug)]
pub struct MultiLiteralMatcher {
    /// Aho-Corasick 自动机。
    ac: AhoCorasick,
}

impl MultiLiteralMatcher {
    /// 根据给定的字面值创建一个新的多字面值匹配器。
    pub fn new<B: AsRef<[u8]>>(
        literals: &[B],
    ) -> Result<MultiLiteralMatcher, Error> {
        let ac = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostFirst)
            .build(literals)
            .map_err(Error::generic)?;
        Ok(MultiLiteralMatcher { ac })
    }
}

impl Matcher for MultiLiteralMatcher {
    type Captures = RegexCaptures;
    type Error = NoError;

    // 在指定位置进行匹配。
    fn find_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<Option<Match>, NoError> {
        match self.ac.find(&haystack[at..]) {
            None => Ok(None),
            Some(m) => Ok(Some(Match::new(at + m.start(), at + m.end()))),
        }
    }

    // 创建一个新的 RegexCaptures 实例。
    fn new_captures(&self) -> Result<RegexCaptures, NoError> {
        Ok(RegexCaptures::simple())
    }

    // 返回捕获组的数量。
    fn capture_count(&self) -> usize {
        1
    }

    // 根据名称返回捕获组的索引，对于多字面值匹配器来说不适用。
    fn capture_index(&self, _: &str) -> Option<usize> {
        None
    }

    // 在指定位置进行捕获。
    fn captures_at(
        &self,
        haystack: &[u8],
        at: usize,
        caps: &mut RegexCaptures,
    ) -> Result<bool, NoError> {
        caps.set_simple(None);
        let mat = self.find_at(haystack, at)?;
        caps.set_simple(mat);
        Ok(mat.is_some())
    }

    // 我们特意不实现其他方法，如 find_iter。特别地，
    // 通过实现上面的 find_at 方法，可以保证迭代器方法的正确性。
}

/// 查看给定的 HIR 是否是简单的字面值替代，如果是，则返回字面值。否则，返回 None。
pub fn alternation_literals(expr: &Hir) -> Option<Vec<Vec<u8>>> {
    // 这是相当巧妙的，但基本上，如果 `is_alternation_literal` 为 true，
    // 那么我们可以对 HIR 的结构做出一些假设。这就是下面的 `unreachable!` 语句的合理性所在。

    if !expr.properties().is_alternation_literal() {
        return None;
    }
    let alts = match *expr.kind() {
        HirKind::Alternation(ref alts) => alts,
        _ => return None, // 一个字面值不值得
    };

    let mut lits = vec![];
    for alt in alts {
        let mut lit = vec![];
        match *alt.kind() {
            HirKind::Empty => {}
            HirKind::Literal(ref x) => lit.extend_from_slice(&x.0),
            HirKind::Concat(ref exprs) => {
                for e in exprs {
                    match *e.kind() {
                        HirKind::Literal(ref x) => lit.extend_from_slice(&x.0),
                        _ => unreachable!("expected literal, got {:?}", e),
                    }
                }
            }
            _ => unreachable!("expected literal or concat, got {:?}", alt),
        }
        lits.push(lit);
    }
    Some(lits)
}
