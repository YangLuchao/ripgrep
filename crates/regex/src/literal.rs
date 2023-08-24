use {
    regex_automata::meta::Regex,
    regex_syntax::hir::{
        self,
        literal::{Literal, Seq},
        Hir,
    },
};

use crate::{config::ConfiguredHIR, error::Error};
/// 一种封装了来自正则表达式的“内部”文字提取的类型。
///
/// 它使用许多启发式方法，尝试从正则表达式中提取出用于构建更容易优化的简化正则表达式的文字。
///
/// 这种技术背后的主要思想是，ripgrep 搜索单独的行而不是跨行搜索。（除非启用了 -U/--multiline 选项。）
/// 也就是说，我们可以从正则表达式中提取文字，搜索这些文字，找到包含该文字的行的边界，然后只对该行运行原始正则表达式。
/// 这在吞吐量导向的搜索中总体效果非常好，因为它可能使 ripgrep 能够在快速向量化的文字查找例程中花费更多时间，而不是在（更慢的）正则表达式引擎中。
///
/// 这种优化在早期阶段更为重要，但自那以来，Rust 的正则表达式引擎实际上已经在某种程度上支持了自己的（尽管有限的）内部文字优化。
/// 因此，这种技术的适用性不如过去那么大了。
///
/// 一个很好的例子是正则表达式 `\s+(Sherlock|[A-Z]atso[a-z]|Moriarty)\s+`。
/// 特别是 `atso` 前面的 `[A-Z]` 部分会阻止正则表达式引擎自身的内部文字优化生效。
/// 在旧的实现中（ripgrep <=13），此特定正则表达式中也没有提取任何内部文字。
/// 因此，这种特定的实现在从旧实现和正则表达式引擎自身的优化（理论上仍然可以改进）中都有所改进。
#[derive(Clone, Debug)]
pub(crate) struct InnerLiterals {
    seq: Seq,
}

impl InnerLiterals {
    /// 从给定的配置好的HIR表达式创建一组内部文字。
    ///
    /// 如果没有配置行终止符，则始终会拒绝提取文字，因为内部文字优化可能无效。
    ///
    /// 注意，这需要实际用于搜索的实际正则表达式，因为它会查询有关已编译正则表达式的一些状态。
    /// 该状态可能会影响内部文字提取。
    pub(crate) fn new(chir: &ConfiguredHIR, re: &Regex) -> InnerLiterals {
        // 如果没有配置行终止符，则在此级别上内部文字优化无效。
        if chir.config().line_terminator.is_none() {
            log::trace!("跳过内部文字提取，未设置行终止符");
            return InnerLiterals::none();
        }
        // 如果我们认为正则表达式已经加速，则让正则表达式引擎自行处理。我们将跳过内部文字优化。
        if re.is_accelerated() {
            log::trace!("跳过内部文字提取，已存在的正则表达式被认为已经加速",);
            return InnerLiterals::none();
        }
        // 在这种情况下，我们几乎可以肯定正则表达式引擎将尽最大努力处理它，即使它没有被报告为已加速。
        // 如果是字面值的交替，则在此级别上内部文字优化无效。
        if chir.hir().properties().is_alternation_literal() {
            log::trace!(
                "跳过内部文字提取，发现字面值的交替，推迟到正则表达式引擎",
            );
            return InnerLiterals::none();
        }
        let seq = Extractor::new().extract_untagged(chir.hir());
        InnerLiterals { seq }
    }

    /// 返回一个无限的内部文字集，以便永远不会生成匹配器。
    pub(crate) fn none() -> InnerLiterals {
        InnerLiterals { seq: Seq::infinite() }
    }

    /// 如果认为有利于这样做（通过各种可疑的启发式方法），
    /// 则会返回一个单一的正则表达式模式，该模式匹配生成这些文字集的正则表达式所匹配的语言的子集。
    /// 这里的想法是，此方法返回的模式要便宜得多。即通常是单个文字或文字的交替。
    pub(crate) fn one_regex(&self) -> Result<Option<Regex>, Error> {
        let Some(lits) = self.seq.literals() else { return Ok(None) };
        if lits.is_empty() {
            return Ok(None);
        }
        let mut alts = vec![];
        for lit in lits.iter() {
            alts.push(Hir::literal(lit.as_bytes()));
        }
        let hir = Hir::alternation(alts);
        log::debug!("提取的快速行正则表达式: {:?}", hir.to_string());

        let re = Regex::builder()
            .configure(Regex::config().utf8_empty(false))
            .build_from_hir(&hir)
            .map_err(Error::regex)?;
        Ok(Some(re))
    }
}

/// 内部文字提取器。
///
/// 这是来自regex-syntax的提取器的一种精简版本。主要区别在于，我们尝试在遍历HIR时识别出“最佳”一组所需的文字。
#[derive(Debug)]
struct Extractor {
    limit_class: usize,
    limit_repeat: usize,
    limit_literal_len: usize,
    limit_total: usize,
}

impl Extractor {
    /// 使用默认配置创建一个新的内部字面量提取器。
    fn new() -> Extractor {
        Extractor {
            limit_class: 10,
            limit_repeat: 10,
            limit_literal_len: 100,
            limit_total: 64,
        }
    }

    /// 在顶层执行提取器并返回一个无标记的字面量序列。
    fn extract_untagged(&self, hir: &Hir) -> Seq {
        let mut seq = self.extract(hir);
        log::trace!("提取的内部字面量：{:?}", seq.seq);
        seq.seq.optimize_for_prefix_by_preference();
        log::trace!("优化后的提取的内部字面量：{:?}", seq.seq);
        if !seq.is_good() {
            log::trace!("丢弃内部字面量，因为它们可能很慢");
            seq.make_infinite();
        }
        seq.seq
    }

    /// 执行提取器并返回一个字面量序列。
    fn extract(&self, hir: &Hir) -> TSeq {
        use regex_syntax::hir::HirKind::*;

        match *hir.kind() {
            Empty | Look(_) => TSeq::singleton(self::Literal::exact(vec![])),
            Literal(hir::Literal(ref bytes)) => {
                let mut seq =
                    TSeq::singleton(self::Literal::exact(bytes.to_vec()));
                self.enforce_literal_len(&mut seq);
                seq
            }
            Class(hir::Class::Unicode(ref cls)) => {
                self.extract_class_unicode(cls)
            }
            Class(hir::Class::Bytes(ref cls)) => self.extract_class_bytes(cls),
            Repetition(ref rep) => self.extract_repetition(rep),
            Capture(hir::Capture { ref sub, .. }) => self.extract(sub),
            Concat(ref hirs) => self.extract_concat(hirs.iter()),
            Alternation(ref hirs) => self.extract_alternation(hirs.iter()),
        }
    }

    /// 从给定的连接中提取序列。通过交叉乘积组合每个子Hir表达式的序列。
    ///
    /// 一旦交叉乘积变成仅包含不精确字面量的序列，此操作将提前终止。
    fn extract_concat<'a, I: Iterator<Item = &'a Hir>>(&self, it: I) -> TSeq {
        let mut seq = TSeq::singleton(self::Literal::exact(vec![]));
        let mut prev: Option<TSeq> = None;
        for hir in it {
            // 如果序列中的每个元素都是不精确的，那么交叉乘积将始终是无操作。因此，我们无法再添加任何内容，可以提前退出。
            if seq.is_inexact() {
                // 如果连接中有空序列，那么连接将永远无法匹配。因此我们可以立即退出。
                if seq.is_empty() {
                    return seq;
                }
                if seq.is_really_good() {
                    return seq;
                }
                prev = Some(match prev {
                    None => seq,
                    Some(prev) => prev.choose(seq),
                });
                seq = TSeq::singleton(self::Literal::exact(vec![]));
                seq.make_not_prefix();
            }
            // 注意，'cross'还根据我们是否提取前缀或后缀进行调度。
            seq = self.cross(seq, self.extract(hir));
        }
        if let Some(prev) = prev {
            prev.choose(seq)
        } else {
            seq
        }
    }

    /// 从给定的选择中提取序列。
    ///
    /// 一旦联合操作变成无限序列，此操作将提前终止。
    fn extract_alternation<'a, I: Iterator<Item = &'a Hir>>(
        &self,
        it: I,
    ) -> TSeq {
        let mut seq = TSeq::empty();
        for hir in it {
            // 一旦序列是无限的，每个随后的联合操作都将始终导致无限序列。因此，它永远不会改变，我们可以提前终止。
            if !seq.is_finite() {
                break;
            }
            seq = self.union(seq, &mut self.extract(hir));
        }
        seq
    }

    /// 从给定的重复中提取字面量序列。我们尽力而为，以下是一些示例：
    ///
    ///   'a*'    => [不精确(a), 精确("")]
    ///   'a*?'   => [精确(""), 不精确(a)]
    ///   'a+'    => [不精确(a)]
    ///   'a{3}'  => [精确(aaa)]
    ///   'a{3,5} => [不精确(aaa)]
    ///
    /// 关键在于确保我们在添加的每个字面量上正确地设置'inexact' vs 'exact'属性。
    /// 例如，'a*' 给了我们一个不精确的 'a' 和一个精确的空字符串，这意味着正则表达式 'ab*c' 将导致提取 [不精确(ab), 精确(ac)] 字面量，这实际上可能是一个比只有 'a' 更好的预过滤器。
    fn extract_repetition(&self, rep: &hir::Repetition) -> TSeq {
        let mut subseq = self.extract(&rep.sub);
        match *rep {
            hir::Repetition { min: 0, max, greedy, .. } => {
                // 当 'max=1' 时，我们可以保留精确性，因为 'a?' 等同于 'a|'。类似地，下面的 'a??' 等同于 '|a'。
                if max != Some(1) {
                    subseq.make_inexact();
                }
                let mut empty = TSeq::singleton(Literal::exact(vec![]));
                if !greedy {
                    std::mem::swap(&mut subseq, &mut empty);
                }
                self.union(subseq, &mut empty)
            }
            hir::Repetition { min, max: Some(max), .. } if min == max => {
                assert!(min > 0); // 在上面处理过了
                let limit =
                    u32::try_from(self.limit_repeat).unwrap_or(u32::MAX);
                let mut seq = TSeq::singleton(Literal::exact(vec![]));
                for _ in 0..std::cmp::min(min, limit) {
                    if seq.is_inexact() {
                        break;
                    }
                    seq = self.cross(seq, subseq.clone());
                }
                if usize::try_from(min).is_err() || min > limit {
                    seq.make_inexact();
                }
                seq
            }
            hir::Repetition { min, max: Some(max), .. } if min < max => {
                assert!(min > 0); // 在上面处理过了
                let limit =
                    u32::try_from(self.limit_repeat).unwrap_or(u32::MAX);
                let mut seq = TSeq::singleton(Literal::exact(vec![]));
                for _ in 0..std::cmp::min(min, limit) {
                    if seq.is_inexact() {
                        break;
                    }
                    seq = self.cross(seq, subseq.clone());
                }
                seq.make_inexact();
                seq
            }
            hir::Repetition { .. } => {
                subseq.make_inexact();
                subseq
            }
        }
    }

    /// 如果给定的Unicode类小到足够处理，则将其转换为字面量序列。
    /// 如果类太大，则返回一个无限序列。
    fn extract_class_unicode(&self, cls: &hir::ClassUnicode) -> TSeq {
        if self.class_over_limit_unicode(cls) {
            return TSeq::infinite();
        }
        let mut seq = TSeq::empty();
        for r in cls.iter() {
            for ch in r.start()..=r.end() {
                seq.push(Literal::from(ch));
            }
        }
        self.enforce_literal_len(&mut seq);
        seq
    }

    /// 如果给定的字节类小到足够处理，则将其转换为字面量序列。
    /// 如果类太大，则返回一个无限序列。
    fn extract_class_bytes(&self, cls: &hir::ClassBytes) -> TSeq {
        if self.class_over_limit_bytes(cls) {
            return TSeq::infinite();
        }
        let mut seq = TSeq::empty();
        for r in cls.iter() {
            for b in r.start()..=r.end() {
                seq.push(Literal::from(b));
            }
        }
        self.enforce_literal_len(&mut seq);
        seq
    }

    /// 如果给定的Unicode类超出了此提取器的配置限制，则返回true。
    fn class_over_limit_unicode(&self, cls: &hir::ClassUnicode) -> bool {
        let mut count = 0;
        for r in cls.iter() {
            if count > self.limit_class {
                return true;
            }
            count += r.len();
        }
        count > self.limit_class
    }
    /// 如果给定的字节类超出了此提取器的配置限制，则返回true。
    fn class_over_limit_bytes(&self, cls: &hir::ClassBytes) -> bool {
        let mut count = 0;
        for r in cls.iter() {
            if count > self.limit_class {
                return true;
            }
            count += r.len();
        }
        count > self.limit_class
    }

    /// 计算两个序列的交叉乘积，如果结果在配置限制内。否则，将 `seq2` 设为无限，然后将无限序列与 `seq1` 进行交叉。
    fn cross(&self, mut seq1: TSeq, mut seq2: TSeq) -> TSeq {
        if !seq2.prefix {
            return seq1.choose(seq2);
        }
        if seq1
            .max_cross_len(&seq2)
            .map_or(false, |len| len > self.limit_total)
        {
            seq2.make_infinite();
        }
        seq1.cross_forward(&mut seq2);
        assert!(seq1.len().map_or(true, |x| x <= self.limit_total));
        self.enforce_literal_len(&mut seq1);
        seq1
    }

    /// 如果结果在配置限制内，将两个序列合并。否则，将 `seq2` 设为无限，然后将无限序列与 `seq1` 进行合并。
    fn union(&self, mut seq1: TSeq, seq2: &mut TSeq) -> TSeq {
        if seq1.max_union_len(seq2).map_or(false, |len| len > self.limit_total)
        {
            // 我们尝试修剪字面量序列，以查看是否可以为更多字面量腾出空间。
            // 我们的想法是，如果我们可以通过修剪序列中已经存在的字面量来添加更多字面量并保留有限序列，那么我们宁愿这样做。
            // 否则，我们将与一个无限序列进行合并，这会影响一切，实际上会停止字面量提取。
            //
            // 为什么我们在这里保留了4个字节？这有点是一个抽象泄漏。在下游，这些字面量可能最终会被输入到 Teddy 算法中，
            // 该算法支持搜索长度为4的字面量。所以这就是我们选择这个数字的原因。可以说，这应该是一个可调参数，但似乎有点棘手来描述。
            // 而且我还不确定这是否是正确处理修剪字面量序列的方法。
            seq1.keep_first_bytes(4);
            seq2.keep_first_bytes(4);
            seq1.dedup();
            seq2.dedup();
            if seq1
                .max_union_len(seq2)
                .map_or(false, |len| len > self.limit_total)
            {
                seq2.make_infinite();
            }
        }
        seq1.union(seq2);
        assert!(seq1.len().map_or(true, |x| x <= self.limit_total));
        seq1
    }

    /// 对给定序列应用字面量长度限制。如果序列中没有一个字面量超过限制，则不进行操作。
    fn enforce_literal_len(&self, seq: &mut TSeq) {
        seq.keep_first_bytes(self.limit_literal_len);
    }
}

#[derive(Clone, Debug)]
struct TSeq {
    seq: Seq,
    prefix: bool,
}

#[allow(dead_code)]
impl TSeq {
    fn empty() -> TSeq {
        TSeq { seq: Seq::empty(), prefix: true }
    }

    fn infinite() -> TSeq {
        TSeq { seq: Seq::infinite(), prefix: true }
    }

    fn singleton(lit: Literal) -> TSeq {
        TSeq { seq: Seq::singleton(lit), prefix: true }
    }

    fn new<I, B>(it: I) -> TSeq
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        TSeq { seq: Seq::new(it), prefix: true }
    }

    fn literals(&self) -> Option<&[Literal]> {
        self.seq.literals()
    }

    fn push(&mut self, lit: Literal) {
        self.seq.push(lit);
    }

    fn make_inexact(&mut self) {
        self.seq.make_inexact();
    }

    fn make_infinite(&mut self) {
        self.seq.make_infinite();
    }

    fn cross_forward(&mut self, other: &mut TSeq) {
        assert!(other.prefix);
        self.seq.cross_forward(&mut other.seq);
    }

    fn union(&mut self, other: &mut TSeq) {
        self.seq.union(&mut other.seq);
    }

    fn dedup(&mut self) {
        self.seq.dedup();
    }

    fn sort(&mut self) {
        self.seq.sort();
    }

    fn keep_first_bytes(&mut self, len: usize) {
        self.seq.keep_first_bytes(len);
    }

    fn is_finite(&self) -> bool {
        self.seq.is_finite()
    }

    fn is_empty(&self) -> bool {
        self.seq.is_empty()
    }

    fn len(&self) -> Option<usize> {
        self.seq.len()
    }

    fn is_exact(&self) -> bool {
        self.seq.is_exact()
    }

    fn is_inexact(&self) -> bool {
        self.seq.is_inexact()
    }

    fn max_union_len(&self, other: &TSeq) -> Option<usize> {
        self.seq.max_union_len(&other.seq)
    }

    fn max_cross_len(&self, other: &TSeq) -> Option<usize> {
        assert!(other.prefix);
        self.seq.max_cross_len(&other.seq)
    }

    fn min_literal_len(&self) -> Option<usize> {
        self.seq.min_literal_len()
    }

    fn max_literal_len(&self) -> Option<usize> {
        self.seq.max_literal_len()
    }

    // Below are methods specific to a TSeq that aren't just forwarding calls
    // to a Seq method.

    /// Tags this sequence as "not a prefix." When this happens, this sequence
    /// can't be crossed as a suffix of another sequence.
    fn make_not_prefix(&mut self) {
        self.prefix = false;
    }

    /// Returns true if it's believed that the sequence given is "good" for
    /// acceleration. This is useful for determining whether a sequence of
    /// literals has any shot of being fast.
    fn is_good(&self) -> bool {
        if self.has_poisonous_literal() {
            return false;
        }
        let Some(min) = self.min_literal_len() else { return false };
        let Some(len) = self.len() else { return false };
        // If we have some very short literals, then let's require that our
        // sequence is itself very small.
        if min <= 1 {
            return len <= 3;
        }
        min >= 2 && len <= 64
    }

    /// Returns true if it's believed that the sequence given is "really
    /// good" for acceleration. This is useful for short circuiting literal
    /// extraction.
    fn is_really_good(&self) -> bool {
        if self.has_poisonous_literal() {
            return false;
        }
        let Some(min) = self.min_literal_len() else { return false };
        let Some(len) = self.len() else { return false };
        min >= 3 && len <= 8
    }

    /// Returns true if the given sequence contains a poisonous literal.
    fn has_poisonous_literal(&self) -> bool {
        let Some(lits) = self.literals() else { return false };
        lits.iter().any(is_poisonous)
    }

    /// Compare the two sequences and return the one that is believed to be best
    /// according to a hodge podge of heuristics.
    fn choose(self, other: TSeq) -> TSeq {
        let (seq1, seq2) = (self, other);
        if !seq1.is_finite() {
            return seq2;
        } else if !seq2.is_finite() {
            return seq1;
        }
        if seq1.has_poisonous_literal() {
            return seq2;
        } else if seq2.has_poisonous_literal() {
            return seq1;
        }
        let Some(min1) = seq1.min_literal_len() else { return seq2 };
        let Some(min2) = seq2.min_literal_len() else { return seq1 };
        if min1 < min2 {
            return seq2;
        } else if min2 < min1 {
            return seq1;
        }
        // OK because we know both sequences are finite, otherwise they wouldn't
        // have a minimum literal length.
        let len1 = seq1.len().unwrap();
        let len2 = seq2.len().unwrap();
        if len1 < len2 {
            return seq2;
        } else if len2 < len1 {
            return seq1;
        }
        // We could do extra stuff like looking at a background frequency
        // distribution of bytes and picking the one that looks more rare, but for
        // now we just pick one.
        seq1
    }
}

impl FromIterator<Literal> for TSeq {
    fn from_iter<T: IntoIterator<Item = Literal>>(it: T) -> TSeq {
        TSeq { seq: Seq::from_iter(it), prefix: true }
    }
}

/// Returns true if it is believe that this literal is likely to match very
/// frequently, and is thus not a good candidate for a prefilter.
fn is_poisonous(lit: &Literal) -> bool {
    use regex_syntax::hir::literal::rank;

    lit.is_empty() || (lit.len() == 1 && rank(lit.as_bytes()[0]) >= 250)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(pattern: impl AsRef<str>) -> Seq {
        let pattern = pattern.as_ref();
        let hir = regex_syntax::ParserBuilder::new()
            .utf8(false)
            .build()
            .parse(pattern)
            .unwrap();
        Extractor::new().extract_untagged(&hir)
    }

    #[allow(non_snake_case)]
    fn E(x: &str) -> Literal {
        Literal::exact(x.as_bytes())
    }

    #[allow(non_snake_case)]
    fn I(x: &str) -> Literal {
        Literal::inexact(x.as_bytes())
    }

    fn seq<I: IntoIterator<Item = Literal>>(it: I) -> Seq {
        Seq::from_iter(it)
    }

    fn inexact<I>(it: I) -> Seq
    where
        I: IntoIterator<Item = Literal>,
    {
        Seq::from_iter(it)
    }

    fn exact<B: AsRef<[u8]>, I: IntoIterator<Item = B>>(it: I) -> Seq {
        Seq::new(it)
    }

    #[test]
    fn various() {
        assert_eq!(e(r"foo"), seq([E("foo")]));
        assert_eq!(e(r"[a-z]foo[a-z]"), seq([I("foo")]));
        assert_eq!(e(r"[a-z](foo)(bar)[a-z]"), seq([I("foobar")]));
        assert_eq!(e(r"[a-z]([a-z]foo)(bar[a-z])[a-z]"), seq([I("foobar")]));
        assert_eq!(e(r"[a-z]([a-z]foo)([a-z]foo)[a-z]"), seq([I("foo")]));
        assert_eq!(e(r"(\d{1,3}\.){3}\d{1,3}"), seq([I(".")]));
        assert_eq!(e(r"[a-z]([a-z]foo){3}[a-z]"), seq([I("foo")]));
        assert_eq!(e(r"[a-z](foo[a-z]){3}[a-z]"), seq([I("foo")]));
        assert_eq!(e(r"[a-z]([a-z]foo[a-z]){3}[a-z]"), seq([I("foo")]));
        assert_eq!(
            e(r"[a-z]([a-z]foo){3}(bar[a-z]){3}[a-z]"),
            seq([I("foobar")])
        );
    }

    // These test that some of our suspicious heuristics try to "pick better
    // literals."
    #[test]
    fn heuristics() {
        // Here, the first literals we stumble across are {ab, cd, ef}. But we
        // keep going and our heuristics decide that {hiya} is better. (And it
        // should be, since it's just one literal and it's longer.)
        assert_eq!(e(r"[a-z]+(ab|cd|ef)[a-z]+hiya[a-z]+"), seq([I("hiya")]));
        // But here, the first alternation becomes "good enough" that literal
        // extraction short circuits early. {hiya} is probably still a better
        // choice here, but {abc, def, ghi} is not bad.
        assert_eq!(
            e(r"[a-z]+(abc|def|ghi)[a-z]+hiya[a-z]+"),
            seq([I("abc"), I("def"), I("ghi")])
        );
    }

    #[test]
    fn literal() {
        assert_eq!(exact(["a"]), e("a"));
        assert_eq!(exact(["aaaaa"]), e("aaaaa"));
        assert_eq!(exact(["A", "a"]), e("(?i-u)a"));
        assert_eq!(exact(["AB", "Ab", "aB", "ab"]), e("(?i-u)ab"));
        assert_eq!(exact(["abC", "abc"]), e("ab(?i-u)c"));

        assert_eq!(Seq::infinite(), e(r"(?-u:\xFF)"));
        assert_eq!(exact([b"Z"]), e(r"Z"));

        assert_eq!(exact(["☃"]), e("☃"));
        assert_eq!(exact(["☃"]), e("(?i)☃"));
        assert_eq!(exact(["☃☃☃☃☃"]), e("☃☃☃☃☃"));

        assert_eq!(exact(["Δ"]), e("Δ"));
        assert_eq!(exact(["δ"]), e("δ"));
        assert_eq!(exact(["Δ", "δ"]), e("(?i)Δ"));
        assert_eq!(exact(["Δ", "δ"]), e("(?i)δ"));

        assert_eq!(exact(["S", "s", "ſ"]), e("(?i)S"));
        assert_eq!(exact(["S", "s", "ſ"]), e("(?i)s"));
        assert_eq!(exact(["S", "s", "ſ"]), e("(?i)ſ"));

        let letters = "ͱͳͷΐάέήίΰαβγδεζηθικλμνξοπρςστυφχψωϊϋ";
        assert_eq!(exact([letters]), e(letters));
    }

    #[test]
    fn class() {
        assert_eq!(exact(["a", "b", "c"]), e("[abc]"));
        assert_eq!(exact(["a1b", "a2b", "a3b"]), e("a[123]b"));
        assert_eq!(exact(["δ", "ε"]), e("[εδ]"));
        assert_eq!(exact(["Δ", "Ε", "δ", "ε", "ϵ"]), e(r"(?i)[εδ]"));
    }

    #[test]
    fn look() {
        assert_eq!(exact(["ab"]), e(r"a\Ab"));
        assert_eq!(exact(["ab"]), e(r"a\zb"));
        assert_eq!(exact(["ab"]), e(r"a(?m:^)b"));
        assert_eq!(exact(["ab"]), e(r"a(?m:$)b"));
        assert_eq!(exact(["ab"]), e(r"a\bb"));
        assert_eq!(exact(["ab"]), e(r"a\Bb"));
        assert_eq!(exact(["ab"]), e(r"a(?-u:\b)b"));
        assert_eq!(exact(["ab"]), e(r"a(?-u:\B)b"));

        assert_eq!(exact(["ab"]), e(r"^ab"));
        assert_eq!(exact(["ab"]), e(r"$ab"));
        assert_eq!(exact(["ab"]), e(r"(?m:^)ab"));
        assert_eq!(exact(["ab"]), e(r"(?m:$)ab"));
        assert_eq!(exact(["ab"]), e(r"\bab"));
        assert_eq!(exact(["ab"]), e(r"\Bab"));
        assert_eq!(exact(["ab"]), e(r"(?-u:\b)ab"));
        assert_eq!(exact(["ab"]), e(r"(?-u:\B)ab"));

        assert_eq!(exact(["ab"]), e(r"ab^"));
        assert_eq!(exact(["ab"]), e(r"ab$"));
        assert_eq!(exact(["ab"]), e(r"ab(?m:^)"));
        assert_eq!(exact(["ab"]), e(r"ab(?m:$)"));
        assert_eq!(exact(["ab"]), e(r"ab\b"));
        assert_eq!(exact(["ab"]), e(r"ab\B"));
        assert_eq!(exact(["ab"]), e(r"ab(?-u:\b)"));
        assert_eq!(exact(["ab"]), e(r"ab(?-u:\B)"));

        assert_eq!(seq([I("aZ"), E("ab")]), e(r"^aZ*b"));
    }

    #[test]
    fn repetition() {
        assert_eq!(Seq::infinite(), e(r"a?"));
        assert_eq!(Seq::infinite(), e(r"a??"));
        assert_eq!(Seq::infinite(), e(r"a*"));
        assert_eq!(Seq::infinite(), e(r"a*?"));
        assert_eq!(inexact([I("a")]), e(r"a+"));
        assert_eq!(inexact([I("a")]), e(r"(a+)+"));

        assert_eq!(exact(["ab"]), e(r"aZ{0}b"));
        assert_eq!(exact(["aZb", "ab"]), e(r"aZ?b"));
        assert_eq!(exact(["ab", "aZb"]), e(r"aZ??b"));
        assert_eq!(inexact([I("aZ"), E("ab")]), e(r"aZ*b"));
        assert_eq!(inexact([E("ab"), I("aZ")]), e(r"aZ*?b"));
        assert_eq!(inexact([I("aZ")]), e(r"aZ+b"));
        assert_eq!(inexact([I("aZ")]), e(r"aZ+?b"));

        assert_eq!(exact(["aZZb"]), e(r"aZ{2}b"));
        assert_eq!(inexact([I("aZZ")]), e(r"aZ{2,3}b"));

        assert_eq!(Seq::infinite(), e(r"(abc)?"));
        assert_eq!(Seq::infinite(), e(r"(abc)??"));

        assert_eq!(inexact([I("a"), E("b")]), e(r"a*b"));
        assert_eq!(inexact([E("b"), I("a")]), e(r"a*?b"));
        assert_eq!(inexact([I("ab")]), e(r"ab+"));
        assert_eq!(inexact([I("a"), I("b")]), e(r"a*b+"));

        assert_eq!(inexact([I("a"), I("b"), E("c")]), e(r"a*b*c"));
        assert_eq!(inexact([I("a"), I("b"), E("c")]), e(r"(a+)?(b+)?c"));
        assert_eq!(inexact([I("a"), I("b"), E("c")]), e(r"(a+|)(b+|)c"));
        // A few more similarish but not identical regexes. These may have a
        // similar problem as above.
        assert_eq!(Seq::infinite(), e(r"a*b*c*"));
        assert_eq!(inexact([I("a"), I("b"), I("c")]), e(r"a*b*c+"));
        assert_eq!(inexact([I("a"), I("b")]), e(r"a*b+c"));
        assert_eq!(inexact([I("a"), I("b")]), e(r"a*b+c*"));
        assert_eq!(inexact([I("ab"), E("a")]), e(r"ab*"));
        assert_eq!(inexact([I("ab"), E("ac")]), e(r"ab*c"));
        assert_eq!(inexact([I("ab")]), e(r"ab+"));
        assert_eq!(inexact([I("ab")]), e(r"ab+c"));

        assert_eq!(inexact([I("z"), E("azb")]), e(r"z*azb"));

        let expected =
            exact(["aaa", "aab", "aba", "abb", "baa", "bab", "bba", "bbb"]);
        assert_eq!(expected, e(r"[ab]{3}"));
        let expected = inexact([
            I("aaa"),
            I("aab"),
            I("aba"),
            I("abb"),
            I("baa"),
            I("bab"),
            I("bba"),
            I("bbb"),
        ]);
        assert_eq!(expected, e(r"[ab]{3,4}"));
    }

    #[test]
    fn concat() {
        assert_eq!(exact(["abcxyz"]), e(r"abc()xyz"));
        assert_eq!(exact(["abcxyz"]), e(r"(abc)(xyz)"));
        assert_eq!(exact(["abcmnoxyz"]), e(r"abc()mno()xyz"));
        assert_eq!(Seq::infinite(), e(r"abc[a&&b]xyz"));
        assert_eq!(exact(["abcxyz"]), e(r"abc[a&&b]*xyz"));
    }

    #[test]
    fn alternation() {
        assert_eq!(exact(["abc", "mno", "xyz"]), e(r"abc|mno|xyz"));
        assert_eq!(
            inexact([E("abc"), I("mZ"), E("mo"), E("xyz")]),
            e(r"abc|mZ*o|xyz")
        );
        assert_eq!(exact(["abc", "xyz"]), e(r"abc|M[a&&b]N|xyz"));
        assert_eq!(exact(["abc", "MN", "xyz"]), e(r"abc|M[a&&b]*N|xyz"));

        assert_eq!(exact(["aaa"]), e(r"(?:|aa)aaa"));
        assert_eq!(Seq::infinite(), e(r"(?:|aa)(?:aaa)*"));
        assert_eq!(Seq::infinite(), e(r"(?:|aa)(?:aaa)*?"));

        assert_eq!(Seq::infinite(), e(r"a|b*"));
        assert_eq!(inexact([E("a"), I("b")]), e(r"a|b+"));

        assert_eq!(inexact([I("a"), E("b"), E("c")]), e(r"a*b|c"));

        assert_eq!(Seq::infinite(), e(r"a|(?:b|c*)"));

        assert_eq!(inexact([I("a"), I("b"), E("c")]), e(r"(a|b)*c|(a|ab)*c"));

        assert_eq!(
            exact(["abef", "abgh", "cdef", "cdgh"]),
            e(r"(ab|cd)(ef|gh)")
        );
        assert_eq!(
            exact([
                "abefij", "abefkl", "abghij", "abghkl", "cdefij", "cdefkl",
                "cdghij", "cdghkl",
            ]),
            e(r"(ab|cd)(ef|gh)(ij|kl)")
        );
    }

    #[test]
    fn impossible() {
        // N.B. The extractor in this module "optimizes" the sequence and makes
        // it infinite if it isn't "good." An empty sequence (generated by a
        // concatenantion containing an expression that can never match) is
        // considered "not good." Since infinite sequences are not actionably
        // and disable optimizations, this winds up being okay.
        //
        // The literal extractor in regex-syntax doesn't combine these two
        // steps and makes the caller choose to optimize. That is, it returns
        // the sequences as they are. Which in this case, for some of the tests
        // below, would be an empty Seq and not an infinite Seq.
        assert_eq!(Seq::infinite(), e(r"[a&&b]"));
        assert_eq!(Seq::infinite(), e(r"a[a&&b]"));
        assert_eq!(Seq::infinite(), e(r"[a&&b]b"));
        assert_eq!(Seq::infinite(), e(r"a[a&&b]b"));
        assert_eq!(exact(["a", "b"]), e(r"a|[a&&b]|b"));
        assert_eq!(exact(["a", "b"]), e(r"a|c[a&&b]|b"));
        assert_eq!(exact(["a", "b"]), e(r"a|[a&&b]d|b"));
        assert_eq!(exact(["a", "b"]), e(r"a|c[a&&b]d|b"));
        assert_eq!(Seq::infinite(), e(r"[a&&b]*"));
        assert_eq!(exact(["MN"]), e(r"M[a&&b]*N"));
    }

    // This tests patterns that contain something that defeats literal
    // detection, usually because it would blow some limit on the total number
    // of literals that can be returned.
    //
    // The main idea is that when literal extraction sees something that
    // it knows will blow a limit, it replaces it with a marker that says
    // "any literal will match here." While not necessarily true, the
    // over-estimation is just fine for the purposes of literal extraction,
    // because the imprecision doesn't matter: too big is too big.
    //
    // This is one of the trickier parts of literal extraction, since we need
    // to make sure all of our literal extraction operations correctly compose
    // with the markers.
    //
    // Note that unlike in regex-syntax, some of these have "inner" literals
    // extracted where a prefix or suffix would otherwise not be found.
    #[test]
    fn anything() {
        assert_eq!(Seq::infinite(), e(r"."));
        assert_eq!(Seq::infinite(), e(r"(?s)."));
        assert_eq!(Seq::infinite(), e(r"[A-Za-z]"));
        assert_eq!(Seq::infinite(), e(r"[A-Z]"));
        assert_eq!(Seq::infinite(), e(r"[A-Z]{0}"));
        assert_eq!(Seq::infinite(), e(r"[A-Z]?"));
        assert_eq!(Seq::infinite(), e(r"[A-Z]*"));
        assert_eq!(Seq::infinite(), e(r"[A-Z]+"));
        assert_eq!(seq([I("1")]), e(r"1[A-Z]"));
        assert_eq!(seq([I("1")]), e(r"1[A-Z]2"));
        assert_eq!(seq([E("123")]), e(r"[A-Z]+123"));
        assert_eq!(seq([I("123")]), e(r"[A-Z]+123[A-Z]+"));
        assert_eq!(Seq::infinite(), e(r"1|[A-Z]|3"));
        assert_eq!(seq([E("1"), I("2"), E("3")]), e(r"1|2[A-Z]|3"),);
        assert_eq!(seq([E("1"), I("2"), E("3")]), e(r"1|[A-Z]2[A-Z]|3"),);
        assert_eq!(seq([E("1"), E("2"), E("3")]), e(r"1|[A-Z]2|3"),);
        assert_eq!(seq([E("1"), I("2"), E("4")]), e(r"1|2[A-Z]3|4"),);
        assert_eq!(seq([E("2")]), e(r"(?:|1)[A-Z]2"));
        assert_eq!(inexact([I("a")]), e(r"a.z"));
    }

    #[test]
    fn empty() {
        assert_eq!(Seq::infinite(), e(r""));
        assert_eq!(Seq::infinite(), e(r"^"));
        assert_eq!(Seq::infinite(), e(r"$"));
        assert_eq!(Seq::infinite(), e(r"(?m:^)"));
        assert_eq!(Seq::infinite(), e(r"(?m:$)"));
        assert_eq!(Seq::infinite(), e(r"\b"));
        assert_eq!(Seq::infinite(), e(r"\B"));
        assert_eq!(Seq::infinite(), e(r"(?-u:\b)"));
        assert_eq!(Seq::infinite(), e(r"(?-u:\B)"));
    }

    #[test]
    fn crazy_repeats() {
        assert_eq!(Seq::infinite(), e(r"(?:){4294967295}"));
        assert_eq!(Seq::infinite(), e(r"(?:){64}{64}{64}{64}{64}{64}"));
        assert_eq!(Seq::infinite(), e(r"x{0}{4294967295}"));
        assert_eq!(Seq::infinite(), e(r"(?:|){4294967295}"));

        assert_eq!(
            Seq::infinite(),
            e(r"(?:){8}{8}{8}{8}{8}{8}{8}{8}{8}{8}{8}{8}{8}{8}")
        );
        let repa = "a".repeat(100);
        assert_eq!(
            inexact([I(&repa)]),
            e(r"a{8}{8}{8}{8}{8}{8}{8}{8}{8}{8}{8}{8}{8}{8}")
        );
    }

    #[test]
    fn optimize() {
        // This gets a common prefix that isn't too short.
        let s = e(r"foobarfoobar|foobar|foobarzfoobar|foobarfoobar");
        assert_eq!(seq([I("foobar")]), s);

        // This also finds a common prefix, but since it's only one byte, it
        // prefers the multiple literals.
        let s = e(r"abba|akka|abccba");
        assert_eq!(exact(["abba", "akka", "abccba"]), s);

        let s = e(r"sam|samwise");
        assert_eq!(seq([E("sam")]), s);

        // The empty string is poisonous, so our seq becomes infinite, even
        // though all literals are exact.
        let s = e(r"foobarfoo|foo||foozfoo|foofoo");
        assert_eq!(Seq::infinite(), s);

        // A space is also poisonous, so our seq becomes infinite. But this
        // only gets triggered when we don't have a completely exact sequence.
        // When the sequence is exact, spaces are okay, since we presume that
        // any prefilter will match a space more quickly than the regex engine.
        // (When the sequence is exact, there's a chance of the prefilter being
        // used without needing the regex engine at all.)
        let s = e(r"foobarfoo|foo| |foofoo");
        assert_eq!(Seq::infinite(), s);
    }
}
