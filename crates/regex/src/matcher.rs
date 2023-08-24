use std::sync::Arc;

use {
    grep_matcher::{
        ByteSet, Captures, LineMatchKind, LineTerminator, Match, Matcher,
        NoError,
    },
    regex_automata::{
        meta::Regex, util::captures::Captures as AutomataCaptures, Input,
        PatternID,
    },
};

use crate::{
    config::{Config, ConfiguredHIR},
    error::Error,
    literal::InnerLiterals,
    word::WordMatcher,
};
/// 用于使用正则表达式构建`Matcher`的构建器。
///
/// 此构建器重新导出了 regex crate 构建器中的许多相同选项，此外还包括一些其他选项，如智能大小写、单词匹配以及设置行终止符的能力，这些选项可能会启用某些类型的优化。
///
/// 支持的语法在 regex crate 中有文档记录：
/// <https://docs.rs/regex/#syntax>.
#[derive(Clone, Debug)]
pub struct RegexMatcherBuilder {
    config: Config,
}

impl Default for RegexMatcherBuilder {
    /// 默认情况下，创建一个新的`RegexMatcherBuilder`实例。
    fn default() -> RegexMatcherBuilder {
        RegexMatcherBuilder::new()
    }
}

impl RegexMatcherBuilder {
    /// 用于配置正则匹配器的新构建器。
    pub fn new() -> RegexMatcherBuilder {
        RegexMatcherBuilder { config: Config::default() }
    }

    /// 使用当前配置为提供的模式构建一个新的匹配器。
    ///
    /// 支持的语法在 regex crate 中有文档记录：
    /// <https://docs.rs/regex/#syntax>.
    pub fn build(&self, pattern: &str) -> Result<RegexMatcher, Error> {
        self.build_many(&[pattern])
    }

    /// 使用当前配置为提供的模式构建一个新的匹配器。
    /// 所得到的匹配器的行为就好像所有给定的模式被连接成一个单一的选择。也就是说，
    /// 当给定的模式之一匹配时，它报告匹配。
    pub fn build_many<P: AsRef<str>>(
        &self,
        patterns: &[P],
    ) -> Result<RegexMatcher, Error> {
        let chir = self.config.build_many(patterns)?;
        let matcher = RegexMatcherImpl::new(chir)?;
        let (chir, re) = (matcher.chir(), matcher.regex());
        log::trace!("final regex: {:?}", chir.hir().to_string());

        let non_matching_bytes = chir.non_matching_bytes();
        // 如果我们可以从正则表达式中提取一些字面量，那么我们可能能够构建一个更快的正则表达式，
        // 它可以快速识别候选的匹配行。正则表达式引擎会自行处理其所能处理的情况，
        // 但是当设置了行终止符时，我们可以专门多做一些工作。
        // 例如，对于像 `\w+foo\w+` 这样的正则表达式，我们可以搜索 `foo`，
        // 并且在找到匹配时，寻找包含 `foo` 的行，然后仅在该行上运行原始正则表达式。
        // （在这种情况下，正则表达式引擎很可能会为我们处理此情况，因为它非常简单，但是这个思想适用。）
        let fast_line_regex = InnerLiterals::new(chir, re).one_regex()?;

        // 我们在这里覆盖行终止符，以防配置的HIR不支持它。
        let mut config = self.config.clone();
        config.line_terminator = chir.line_terminator();
        Ok(RegexMatcher {
            config,
            matcher,
            fast_line_regex,
            non_matching_bytes,
        })
    }

    /// 从字面量的简单选择中构建一个新的匹配器。
    ///
    /// 根据构建器设置的配置，这可能能够构建一个比通过使用 `|` 连接模式并调用 `build` 更快的匹配器。
    pub fn build_literals<B: AsRef<str>>(
        &self,
        literals: &[B],
    ) -> Result<RegexMatcher, Error> {
        self.build_many(literals)
    }

    /// 设置大小写不敏感（`i`）标志的值。
    ///
    /// 启用时，模式中的字母将匹配大写和小写变体。
    pub fn case_insensitive(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.config.case_insensitive = yes;
        self
    }

    /// 是否启用 "智能大小写"。
    ///
    /// 启用智能大小写时，构建器将根据模式的编写方式自动启用大小写不敏感匹配。
    /// 换句话说，仅当以下两个条件都满足时，大小写不敏感模式才会启用：
    ///
    /// 1. 模式中包含至少一个字面字符。例如，`a\w` 包含一个字面字符（`a`），
    ///    但 `\w` 不包含字面字符。
    /// 2. 在模式的字面字符中，没有一个被认为是 Unicode 大写字符。
    ///    例如，`foo\pL` 中没有大写字面字符，但 `Foo\pL` 中有。
    pub fn case_smart(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.config.case_smart = yes;
        self
    }

    /// 设置多行匹配（`m`）标志的值。
    ///
    /// 启用时，`^` 匹配行的开头，`$` 匹配行的结尾。
    /// 默认情况下，它们匹配输入的开头/结尾。
    pub fn multi_line(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.config.multi_line = yes;
        self
    }

    /// 设置任意字符（`s`）标志的值，
    /// 当 `s` 被设置时，`.` 可以匹配任何字符；当它未设置时（默认），`.` 可以匹配除换行符以外的任何字符。
    ///
    /// 注意："匹配任何字符" 意味着 "任何字节"（当禁用 Unicode 时），
    /// 而在启用 Unicode 时，它意味着 "任何 Unicode 标量值的任何有效 UTF-8 编码"。
    pub fn dot_matches_new_line(
        &mut self,
        yes: bool,
    ) -> &mut RegexMatcherBuilder {
        self.config.dot_matches_new_line = yes;
        self
    }

    /// 设置贪婪转换 (`U`) 标志的值。
    ///
    /// 启用时，`a*` 是懒惰的（尝试找到最短匹配），而 `a*?` 是贪婪的（尝试找到最长匹配）。
    /// 默认情况下，`a*` 是贪婪的，`a*?` 是懒惰的。
    pub fn swap_greed(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.config.swap_greed = yes;
        self
    }

    /// 设置忽略空格 (`x`) 标志的值。
    ///
    /// 启用时，诸如换行符和空格之类的空白将在模式的表达式之间被忽略，
    /// 并且 `#` 可以用于从当前位置开始的下一个换行符之前的注释。
    pub fn ignore_whitespace(
        &mut self,
        yes: bool,
    ) -> &mut RegexMatcherBuilder {
        self.config.ignore_whitespace = yes;
        self
    }
    /// 设置 Unicode (`u`) 标志的值。
    ///
    /// 默认情况下启用。当禁用时，字符类如 `\w` 仅匹配 ASCII 单词字符，
    /// 而不是所有的 Unicode 单词字符。
    pub fn unicode(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.config.unicode = yes;
        self
    }

    /// 是否支持八进制语法。
    ///
    /// 八进制语法是在正则表达式中表示 Unicode 代码点的一种较少知名的方式。
    /// 例如，`a`、`\x61`、`\u0061` 和 `\141` 都是等价的正则表达式，
    /// 其中最后一个示例展示了八进制语法。
    ///
    /// 虽然支持八进制语法本身不是问题，但它确实使得生成良好的错误消息变得更困难。
    /// 也就是说，在基于 PCRE 的正则表达式引擎中，例如 `\0` 的语法会调用反向引用，
    /// 而在 Rust 的正则表达式引擎中明确不支持这一点。
    /// 但是，许多用户期望它被支持。因此，当禁用八进制支持时，错误消息将明确提到不支持反向引用。
    ///
    /// 默认情况下禁用八进制语法。
    pub fn octal(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.config.octal = yes;
        self
    }

    /// 设置编译后正则表达式的大致大小限制。
    ///
    /// 这大致对应于单个编译程序占用的字节数。
    /// 如果程序超过此数量，将返回编译错误。
    pub fn size_limit(&mut self, bytes: usize) -> &mut RegexMatcherBuilder {
        self.config.size_limit = bytes;
        self
    }

    /// 设置 DFA 使用的缓存的大致大小。
    ///
    /// 这大致对应于 DFA 在搜索时将使用的字节数。
    ///
    /// 请注意，这是*每个线程*的限制。
    /// 没有设置全局限制的方式。
    /// 特别地，如果一个正则表达式在多个线程中同时使用，
    /// 那么每个线程可能使用多达此处指定的字节数。
    pub fn dfa_size_limit(
        &mut self,
        bytes: usize,
    ) -> &mut RegexMatcherBuilder {
        self.config.dfa_size_limit = bytes;
        self
    }

    /// 设置此解析器的嵌套限制。
    ///
    /// 嵌套限制控制允许抽象语法树深度有多深。
    /// 如果 AST 超过给定的限制（例如，有太多嵌套的分组），
    /// 则解析器将返回错误。
    ///
    /// 此限制的目的是作为一种启发式措施，
    /// 以防止对 `Ast` 进行显式递归进行结构归纳的消费者出现堆栈溢出。
    /// 虽然此 crate 从不这样做（而是使用恒定的堆栈空间，并将调用堆栈移至堆），
    /// 但其他 crate 可能会这样做。
    ///
    /// 此限制在整个 Ast 被解析之前不会被检查。
    /// 因此，如果调用方想要对使用的堆空间量设置限制，
    /// 那么他们应该对具体模式字符串的长度（以字节为单位）设置限制。
    /// 特别地，这是可行的，因为此解析器实现将限制自己的堆空间，与模式字符串的长度成比例。
    ///
    /// 注意，嵌套限制为 `0` 将对大多数模式返回嵌套限制错误，
    /// 但不是所有模式。例如，`a` 允许但 `ab` 不允许，
    /// 因为 `ab` 需要连接，这会导致嵌套深度为 `1`。
    /// 一般来说，嵌套限制不是在具体语法中明显体现的东西，因此不应以粒度方式使用。
    pub fn nest_limit(&mut self, limit: u32) -> &mut RegexMatcherBuilder {
        self.config.nest_limit = limit;
        self
    }

    /// 为匹配器设置 ASCII 行终止符。
    ///
    /// 设置行终止符的目的是启用某些优化，可以使面向行的搜索更快。
    /// 即，当启用行终止符时，生成的匹配器保证永远不会产生包含行终止符的匹配。
    /// 由于此保证，使用生成的匹配器的用户无需逐行执行慢速搜索以进行行定向搜索。
    ///
    /// 如果由于模式的编写方式，无法保证不匹配行终止符，
    /// 则在尝试构建匹配器时构建器将返回错误。
    /// 例如，模式 `a\sb` 将被转换，以便永远无法匹配 `a\nb`（当 `\n` 是行终止符时），
    /// 但模式 `a\nb` 将返回错误，因为无法轻松删除 `\n`，而不改变模式的基本意图。
    ///
    /// 如果给定的行终止符不是 ASCII 字节（`<=127`），则构建器在构建匹配器时将返回错误。
    pub fn line_terminator(
        &mut self,
        line_term: Option<u8>,
    ) -> &mut RegexMatcherBuilder {
        self.config.line_terminator = line_term.map(LineTerminator::byte);
        self
    }

    /// 将行终止符设置为 `\r\n`，并在正则表达式模式中启用 CRLF 匹配。
    ///
    /// 该方法设置两个不同的设置：
    ///
    /// 1. 它使匹配器的行终止符为 `\r\n`。
    ///    也就是说，这会阻止匹配器产生包含 `\r` 或 `\n` 的匹配。
    /// 2. 它为 `^` 和 `$` 启用了 CRLF 模式。
    ///    这意味着行锚点将将 `\r` 和 `\n` 都视为行终止符，
    ///    但永远不会在 `\r` 和 `\n` 之间匹配。
    ///
    /// 请注意，如果您不希望设置行终止符，但仍希望 `$` 匹配 `\r\n` 行终止符，
    /// 则可以调用 `crlf(true)`，然后调用 `line_terminator(None)`。
    /// 顺序很重要，因为 `crlf` 设置行终止符，但 `line_terminator` 不会影响 `crlf` 设置。
    pub fn crlf(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        if yes {
            self.config.line_terminator = Some(LineTerminator::crlf());
        } else {
            self.config.line_terminator = None;
        }
        self.config.crlf = yes;
        self
    }

    /// 要求所有匹配都发生在单词边界上。
    ///
    /// 启用此选项与在模式两侧放置 `\b` 断言略有不同。
    /// 特别地，`\b` 断言要求其一侧匹配单词字符，而另一侧匹配非单词字符。
    /// 相反，此选项仅要求其一侧匹配非单词字符。
    ///
    /// 例如，`\b-2\b` 不会匹配 `foo -2 bar`，因为 `-` 不是单词字符。
    /// 但是，启用此 `word` 选项的 `-2` 将匹配 `foo -2 bar` 中的 `-2`。
    pub fn word(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.config.word = yes;
        self
    }

    /// 是否应将模式视为固定字符串。当启用此选项时，
    /// 所有字符，包括通常会成为特殊正则元字符的字符，都会被字面匹配。
    pub fn fixed_strings(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.config.fixed_strings = yes;
        self
    }

    /// 是否每个模式应完整匹配一整行。这等效于在模式周围使用 `(?m:^)` 和 `(?m:$)`。
    pub fn whole_line(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.config.whole_line = yes;
        self
    }
}
/// 使用 Rust 的标准正则库实现的 `Matcher` 特性。
#[derive(Clone, Debug)]
pub struct RegexMatcher {
    /// 调用者指定的配置。
    config: Config,
    /// 底层匹配器实现。
    matcher: RegexMatcherImpl,
    /// 一个永不报告错误的负面结果但可能会报告错误的正面结果，
    /// 被认为可以更快地匹配比 `regex`。通常，这是一个单一的字面值或多个字面值的交替。
    fast_line_regex: Option<Regex>,
    /// 一组永远不会出现在匹配中的字节。
    non_matching_bytes: ByteSet,
}

impl RegexMatcher {
    /// 使用默认配置从给定的模式创建新的匹配器。
    pub fn new(pattern: &str) -> Result<RegexMatcher, Error> {
        RegexMatcherBuilder::new().build(pattern)
    }

    /// 使用默认配置从给定的模式创建新的匹配器，但匹配以 `\n` 终止的行。
    ///
    /// 这是一个方便的构造函数，
    /// 用于使用 `RegexMatcherBuilder` 并将其
    /// [`line_terminator`](RegexMatcherBuilder::method.line_terminator) 设置为 `\n`。
    /// 使用此构造函数的目的是允许带有特殊优化以加快面向行的搜索速度。
    /// 这些类型的优化仅在匹配不跨越多行时适用。
    /// 出于这个原因，如果给定的模式包含字面量 `\n`，则此构造函数将返回错误。
    /// 其他用途的 `\n`（比如在 `\s` 中）会被透明地删除。
    pub fn new_line_matcher(pattern: &str) -> Result<RegexMatcher, Error> {
        RegexMatcherBuilder::new().line_terminator(Some(b'\n')).build(pattern)
    }
}

/// 匹配器类型的封装，我们在 `RegexMatcher` 中使用它。
#[derive(Clone, Debug)]
enum RegexMatcherImpl {
    /// 用于所有正则表达式的标准匹配器。
    Standard(StandardMatcher),
    /// 仅在单词边界匹配的匹配器。
    /// 这会将正则表达式转换为 `(^|\W)(...)($|\W)`，而不是更直观的 `\b(...)\b`。
    /// 因此，`WordMatcher` 提供了自己的 `Matcher` 实现，以封装其对捕获组的使用，
    /// 以使它们对调用者不可见。
    Word(WordMatcher),
}

impl RegexMatcherImpl {
    /// 根据配置创建实现 `Matcher` 特性的新实现。
    fn new(mut chir: ConfiguredHIR) -> Result<RegexMatcherImpl, Error> {
        // 当设置 whole_line 时，即使请求了单词匹配，我们也不使用单词匹配器。
        // 为什么？因为 `(?m:^)(pat)(?m:$)` 意味着单词匹配。
        Ok(if chir.config().word && !chir.config().whole_line {
            RegexMatcherImpl::Word(WordMatcher::new(chir)?)
        } else {
            if chir.config().whole_line {
                chir = chir.into_whole_line();
            }
            RegexMatcherImpl::Standard(StandardMatcher::new(chir)?)
        })
    }

    /// 返回所使用的底层正则表达式对象。
    fn regex(&self) -> &Regex {
        match *self {
            RegexMatcherImpl::Word(ref x) => x.regex(),
            RegexMatcherImpl::Standard(ref x) => &x.regex,
        }
    }

    /// 返回用于搜索的底层正则表达式的 HIR。
    fn chir(&self) -> &ConfiguredHIR {
        match *self {
            RegexMatcherImpl::Word(ref x) => x.chir(),
            RegexMatcherImpl::Standard(ref x) => &x.chir,
        }
    }
}

// 此实现只是根据内部匹配器实现进行调度，
// 除了行终止符优化，这可能通过 `fast_line_regex` 执行。
impl Matcher for RegexMatcher {
    // 指定与 RegexMatcher 关联的捕获类型和错误类型。
    type Captures = RegexCaptures;
    type Error = NoError;

    // 在给定位置查找匹配项。
    fn find_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<Option<Match>, NoError> {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.find_at(haystack, at),
            Word(ref m) => m.find_at(haystack, at),
        }
    }

    // 创建新的捕获对象。
    fn new_captures(&self) -> Result<RegexCaptures, NoError> {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.new_captures(),
            Word(ref m) => m.new_captures(),
        }
    }

    // 获取匹配中捕获组的数量。
    fn capture_count(&self) -> usize {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.capture_count(),
            Word(ref m) => m.capture_count(),
        }
    }

    // 获取给定名称的捕获组索引。
    fn capture_index(&self, name: &str) -> Option<usize> {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.capture_index(name),
            Word(ref m) => m.capture_index(name),
        }
    }

    // 在整个文本中查找匹配项。
    fn find(&self, haystack: &[u8]) -> Result<Option<Match>, NoError> {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.find(haystack),
            Word(ref m) => m.find(haystack),
        }
    }

    // 使用迭代器查找所有匹配项，并对每个匹配项执行指定的操作。
    fn find_iter<F>(&self, haystack: &[u8], matched: F) -> Result<(), NoError>
    where
        F: FnMut(Match) -> bool,
    {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.find_iter(haystack, matched),
            Word(ref m) => m.find_iter(haystack, matched),
        }
    }

    // 使用尝试迭代器查找所有匹配项，并对每个匹配项执行指定的操作。
    fn try_find_iter<F, E>(
        &self,
        haystack: &[u8],
        matched: F,
    ) -> Result<Result<(), E>, NoError>
    where
        F: FnMut(Match) -> Result<bool, E>,
    {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.try_find_iter(haystack, matched),
            Word(ref m) => m.try_find_iter(haystack, matched),
        }
    }

    // 查找并捕获匹配项的捕获组。
    fn captures(
        &self,
        haystack: &[u8],
        caps: &mut RegexCaptures,
    ) -> Result<bool, NoError> {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.captures(haystack, caps),
            Word(ref m) => m.captures(haystack, caps),
        }
    }

    // 使用迭代器查找并捕获所有匹配项的捕获组。
    fn captures_iter<F>(
        &self,
        haystack: &[u8],
        caps: &mut RegexCaptures,
        matched: F,
    ) -> Result<(), NoError>
    where
        F: FnMut(&RegexCaptures) -> bool,
    {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.captures_iter(haystack, caps, matched),
            Word(ref m) => m.captures_iter(haystack, caps, matched),
        }
    }

    // 使用尝试迭代器查找并捕获所有匹配项的捕获组。
    fn try_captures_iter<F, E>(
        &self,
        haystack: &[u8],
        caps: &mut RegexCaptures,
        matched: F,
    ) -> Result<Result<(), E>, NoError>
    where
        F: FnMut(&RegexCaptures) -> Result<bool, E>,
    {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.try_captures_iter(haystack, caps, matched),
            Word(ref m) => m.try_captures_iter(haystack, caps, matched),
        }
    }

    // 在给定位置查找并捕获匹配项的捕获组。
    fn captures_at(
        &self,
        haystack: &[u8],
        at: usize,
        caps: &mut RegexCaptures,
    ) -> Result<bool, NoError> {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.captures_at(haystack, at, caps),
            Word(ref m) => m.captures_at(haystack, at, caps),
        }
    }

    // 替换匹配项并将结果写入指定的缓冲区。
    fn replace<F>(
        &self,
        haystack: &[u8],
        dst: &mut Vec<u8>,
        append: F,
    ) -> Result<(), NoError>
    where
        F: FnMut(Match, &mut Vec<u8>) -> bool,
    {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.replace(haystack, dst, append),
            Word(ref m) => m.replace(haystack, dst, append),
        }
    }

    // 替换匹配项的捕获组并将结果写入指定的缓冲区。
    fn replace_with_captures<F>(
        &self,
        haystack: &[u8],
        caps: &mut RegexCaptures,
        dst: &mut Vec<u8>,
        append: F,
    ) -> Result<(), NoError>
    where
        F: FnMut(&Self::Captures, &mut Vec<u8>) -> bool,
    {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => {
                m.replace_with_captures(haystack, caps, dst, append)
            }
            Word(ref m) => {
                m.replace_with_captures(haystack, caps, dst, append)
            }
        }
    }

    // 检查整个文本是否匹配正则表达式。
    fn is_match(&self, haystack: &[u8]) -> Result<bool, NoError> {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.is_match(haystack),
            Word(ref m) => m.is_match(haystack),
        }
    }

    // 检查指定位置的文本是否匹配正则表达式。
    fn is_match_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<bool, NoError> {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.is_match_at(haystack, at),
            Word(ref m) => m.is_match_at(haystack, at),
        }
    }

    // 查找最短匹配长度。
    fn shortest_match(
        &self,
        haystack: &[u8],
    ) -> Result<Option<usize>, NoError> {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.shortest_match(haystack),
            Word(ref m) => m.shortest_match(haystack),
        }
    }

    // 查找给定位置的最短匹配长度。
    fn shortest_match_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<Option<usize>, NoError> {
        use self::RegexMatcherImpl::*;
        match self.matcher {
            Standard(ref m) => m.shortest_match_at(haystack, at),
            Word(ref m) => m.shortest_match_at(haystack, at),
        }
    }

    // 获取非匹配字节集合。
    fn non_matching_bytes(&self) -> Option<&ByteSet> {
        Some(&self.non_matching_bytes)
    }

    // 获取行终止符设置。
    fn line_terminator(&self) -> Option<LineTerminator> {
        self.config.line_terminator
    }

    // 查找候选行匹配类型。
    fn find_candidate_line(
        &self,
        haystack: &[u8],
    ) -> Result<Option<LineMatchKind>, NoError> {
        Ok(match self.fast_line_regex {
            Some(ref regex) => {
                let input = Input::new(haystack);
                regex
                    .search_half(&input)
                    .map(|hm| LineMatchKind::Candidate(hm.offset()))
            }
            None => {
                self.shortest_match(haystack)?.map(LineMatchKind::Confirmed)
            }
        })
    }
}

/// 标准正则表达式匹配器的实现。
#[derive(Clone, Debug)]
struct StandardMatcher {
    /// 从调用者提供的模式编译而来的正则表达式。
    regex: Regex,
    /// 产生此正则表达式的 HIR。
    ///
    /// 我们将其放在 `Arc` 中，因为在它到达这里的时候，它将不会发生变化。
    /// 并且因为克隆和丢弃 `Hir` 在某种程度上是昂贵的，因为它具有深层递归表示。
    chir: Arc<ConfiguredHIR>,
}

impl StandardMatcher {
    // 创建一个新的标准匹配器。
    fn new(chir: ConfiguredHIR) -> Result<StandardMatcher, Error> {
        let chir = Arc::new(chir);
        let regex = chir.to_regex()?; // 将 HIR 转换为正则表达式。
        Ok(StandardMatcher { regex, chir })
    }
}

impl Matcher for StandardMatcher {
    // 指定与标准匹配器关联的捕获类型和错误类型。
    type Captures = RegexCaptures;
    type Error = NoError;

    // 在给定位置查找匹配项。
    fn find_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<Option<Match>, NoError> {
        // 创建一个输入范围，从指定位置到文本末尾。
        let input = Input::new(haystack).span(at..haystack.len());
        // 使用正则表达式在输入范围内查找匹配项。
        Ok(self.regex.find(input).map(|m| Match::new(m.start(), m.end())))
    }

    // 创建新的捕获对象。
    fn new_captures(&self) -> Result<RegexCaptures, NoError> {
        // 使用正则表达式创建新的捕获对象。
        Ok(RegexCaptures::new(self.regex.create_captures()))
    }

    // 获取匹配中捕获组的数量。
    fn capture_count(&self) -> usize {
        // 获取正则表达式中捕获组的数量。
        self.regex.captures_len()
    }

    // 获取给定名称的捕获组索引。
    fn capture_index(&self, name: &str) -> Option<usize> {
        // 使用正则表达式获取给定名称的捕获组索引。
        self.regex.group_info().to_index(PatternID::ZERO, name)
    }

    // 使用尝试迭代器查找所有匹配项，并对每个匹配项执行指定的操作。
    fn try_find_iter<F, E>(
        &self,
        haystack: &[u8],
        mut matched: F,
    ) -> Result<Result<(), E>, NoError>
    where
        F: FnMut(Match) -> Result<bool, E>,
    {
        // 使用正则表达式的迭代器查找所有匹配项。
        for m in self.regex.find_iter(haystack) {
            match matched(Match::new(m.start(), m.end())) {
                Ok(true) => continue,
                Ok(false) => return Ok(Ok(())),
                Err(err) => return Ok(Err(err)),
            }
        }
        Ok(Ok(()))
    }

    // 在给定位置查找并捕获匹配项的捕获组。
    fn captures_at(
        &self,
        haystack: &[u8],
        at: usize,
        caps: &mut RegexCaptures,
    ) -> Result<bool, NoError> {
        // 创建一个输入范围，从指定位置到文本末尾。
        let input = Input::new(haystack).span(at..haystack.len());
        // 获取捕获对象的可变引用，并使用正则表达式在输入范围内搜索并捕获。
        let caps = caps.captures_mut();
        self.regex.search_captures(&input, caps);
        Ok(caps.is_match())
    }

    // 查找给定位置的最短匹配长度。
    fn shortest_match_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<Option<usize>, NoError> {
        // 创建一个输入范围，从指定位置到文本末尾。
        let input = Input::new(haystack).span(at..haystack.len());
        // 使用正则表达式在输入范围内查找最短匹配。
        Ok(self.regex.search_half(&input).map(|hm| hm.offset()))
    }
}
/// 表示匹配中每个捕获组的偏移量。
///
/// 第一个，或 `0` 号捕获组，始终对应于整个匹配，并且在发生匹配时保证存在。
/// 下一个捕获组，位于索引 `1` 处，对应于正则表达式中第一个捕获组，其顺序与左圆括号出现的位置有关。
///
/// 需要注意的是，并非所有捕获组在匹配中都保证存在。例如，在正则表达式 `(?P<foo>\w)|(?P<bar>\W)` 中，
/// 在任何给定的匹配中，只会设置 `foo` 或 `bar` 中的一个。
///
/// 要通过名称访问捕获组，首先需要使用相应匹配器的 `capture_index` 方法找到组的索引，
/// 然后将该索引与 `RegexCaptures::get` 一起使用。
#[derive(Clone, Debug)]
pub struct RegexCaptures {
    /// 存储捕获组的地方。
    caps: AutomataCaptures,
    /// 这些捕获组的行为就好像捕获组从给定偏移量开始。当设置为 `0` 时，这没有影响，
    /// 捕获组按照正常方式进行索引。
    ///
    /// 当构建包装任意正则表达式的匹配器时，这很有用。例如，`WordMatcher` 接受现有的正则表达式 `re`，
    /// 并创建 `(?:^|\W)(re)(?:$|\W)`，但会隐藏从调用者处包装了正则表达式的事实。
    /// 为了实现这一点，匹配器和捕获组必须像 `(re)` 是第 `0` 号捕获组一样进行操作。
    offset: usize,
}

impl Captures for RegexCaptures {
    // 返回捕获组的数量。
    fn len(&self) -> usize {
        self.caps
            .group_info()
            .all_group_len()
            .checked_sub(self.offset)
            .unwrap()
    }

    // 获取给定索引处的匹配项。
    fn get(&self, i: usize) -> Option<Match> {
        let actual = i.checked_add(self.offset).unwrap();
        self.caps.get_group(actual).map(|sp| Match::new(sp.start, sp.end))
    }
}

impl RegexCaptures {
    // 创建一个新的 RegexCaptures 实例。
    pub(crate) fn new(caps: AutomataCaptures) -> RegexCaptures {
        RegexCaptures::with_offset(caps, 0)
    }

    // 创建一个带偏移量的 RegexCaptures 实例。
    pub(crate) fn with_offset(
        caps: AutomataCaptures,
        offset: usize,
    ) -> RegexCaptures {
        RegexCaptures { caps, offset }
    }

    // 获取可变引用的捕获对象。
    pub(crate) fn captures_mut(&mut self) -> &mut AutomataCaptures {
        &mut self.caps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grep_matcher::{LineMatchKind, Matcher};

    // 测试启用单词匹配时的行为，以及演示它与在正则表达式周围使用 `\b` 的区别。
    #[test]
    fn word() {
        let matcher =
            RegexMatcherBuilder::new().word(true).build(r"-2").unwrap();
        assert!(matcher.is_match(b"abc -2 foo").unwrap());

        let matcher =
            RegexMatcherBuilder::new().word(false).build(r"\b-2\b").unwrap();
        assert!(!matcher.is_match(b"abc -2 foo").unwrap());
    }

    // 测试启用换行终止符时阻止其匹配穿过该行终止符。
    #[test]
    fn line_terminator() {
        // 这个例子工作，因为没有指定行终止符。
        let matcher = RegexMatcherBuilder::new().build(r"abc\sxyz").unwrap();
        assert!(matcher.is_match(b"abc\nxyz").unwrap());

        // 这个例子不工作。
        let matcher = RegexMatcherBuilder::new()
            .line_terminator(Some(b'\n'))
            .build(r"abc\sxyz")
            .unwrap();
        assert!(!matcher.is_match(b"abc\nxyz").unwrap());
    }

    // 确保如果设置了行终止符并且无法修改正则表达式以删除行终止符，则构建器返回错误。
    #[test]
    fn line_terminator_error() {
        assert!(RegexMatcherBuilder::new()
            .line_terminator(Some(b'\n'))
            .build(r"a\nz")
            .is_err())
    }

    // 测试启用 CRLF 时允许 `$` 在行末匹配。
    #[test]
    fn line_terminator_crlf() {
        // 测试在带有 `\n` 行终止符的情况下正常使用 `$`。
        let matcher = RegexMatcherBuilder::new()
            .multi_line(true)
            .build(r"abc$")
            .unwrap();
        assert!(matcher.is_match(b"abc\n").unwrap());

        // 测试 `$` 在 `\r\n` 边界处通常不匹配。
        let matcher = RegexMatcherBuilder::new()
            .multi_line(true)
            .build(r"abc$")
            .unwrap();
        assert!(!matcher.is_match(b"abc\r\n").unwrap());

        // 现在检查 CRLF 处理。
        let matcher = RegexMatcherBuilder::new()
            .multi_line(true)
            .crlf(true)
            .build(r"abc$")
            .unwrap();
        assert!(matcher.is_match(b"abc\r\n").unwrap());
    }

    // 测试智能大小写是否起作用。
    #[test]
    fn case_smart() {
        let matcher =
            RegexMatcherBuilder::new().case_smart(true).build(r"abc").unwrap();
        assert!(matcher.is_match(b"ABC").unwrap());

        let matcher =
            RegexMatcherBuilder::new().case_smart(true).build(r"aBc").unwrap();
        assert!(!matcher.is_match(b"ABC").unwrap());
    }

    // 测试找到候选行是否按预期工作。
    // FIXME: 一旦内部文字提取工作，重新启用此测试。
    #[test]
    #[ignore]
    fn candidate_lines() {
        fn is_confirmed(m: LineMatchKind) -> bool {
            match m {
                LineMatchKind::Confirmed(_) => true,
                _ => false,
            }
        }
        fn is_candidate(m: LineMatchKind) -> bool {
            match m {
                LineMatchKind::Candidate(_) => true,
                _ => false,
            }
        }

        // 没有设置行终止符时，我们无法应用任何优化，因此会得到一个已确认的匹配。
        let matcher = RegexMatcherBuilder::new().build(r"\wfoo\s").unwrap();
        let m = matcher.find_candidate_line(b"afoo ").unwrap().unwrap();
        assert!(is_confirmed(m));

        // 有行终止符和一个专门制作以具有易于检测的内部文字为特点的正则表达式时，
        // 我们可以应用一个快速查找候选匹配项的优化。
        let matcher = RegexMatcherBuilder::new()
            .line_terminator(Some(b'\n'))
            .build(r"\wfoo\s")
            .unwrap();
        let m = matcher.find_candidate_line(b"afoo ").unwrap().unwrap();
        assert!(is_candidate(m));
    }
}
