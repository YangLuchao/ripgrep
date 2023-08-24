use std::collections::HashMap;

use grep_matcher::{Captures, Match, Matcher};
use pcre2::bytes::{CaptureLocations, Regex, RegexBuilder};

use crate::error::Error;

/// 用于配置PCRE2正则表达式编译的构建器。
#[derive(Clone, Debug)]
pub struct RegexMatcherBuilder {
    builder: RegexBuilder,
    case_smart: bool,
    word: bool,
    fixed_strings: bool,
    whole_line: bool,
}

impl RegexMatcherBuilder {
    /// 使用默认配置创建一个新的匹配器构建器。
    pub fn new() -> RegexMatcherBuilder {
        RegexMatcherBuilder {
            builder: RegexBuilder::new(),
            case_smart: false,
            word: false,
            fixed_strings: false,
            whole_line: false,
        }
    }

    /// 将给定的模式编译为使用当前配置的 PCRE 匹配器。
    ///
    /// 如果编译模式时出现问题，将返回错误。
    pub fn build(&self, pattern: &str) -> Result<RegexMatcher, Error> {
        self.build_many(&[pattern])
    }

    /// 将所有给定的模式编译为一个单一的正则表达式，当其中至少一个模式匹配时，正则表达式匹配。
    ///
    /// 如果构建正则表达式时出现问题，将返回错误。
    pub fn build_many<P: AsRef<str>>(
        &self,
        patterns: &[P],
    ) -> Result<RegexMatcher, Error> {
        let mut builder = self.builder.clone();
        let mut pats = Vec::with_capacity(patterns.len());
        for p in patterns.iter() {
            pats.push(if self.fixed_strings {
                format!("(?:{})", pcre2::escape(p.as_ref()))
            } else {
                format!("(?:{})", p.as_ref())
            });
        }
        let mut singlepat = pats.join("|");
        if self.case_smart && !has_uppercase_literal(&singlepat) {
            builder.caseless(true);
        }
        if self.whole_line {
            singlepat = format!(r"(?m:^)(?:{})(?m:$)", singlepat);
        } else if self.word {
            // 当启用 whole_line 选项时，我们使此选项与 word 选项互斥，因为当 whole_line 启用时，
            // 所有必需的匹配都在单词边界上。因此，这个额外的部分是严格多余的。
            singlepat = format!(r"(?<!\w)(?:{})(?!\w)", singlepat);
        }
        log::trace!("最终正则表达式: {:?}", singlepat);
        builder.build(&singlepat).map_err(Error::regex).map(|regex| {
            let mut names = HashMap::new();
            for (i, name) in regex.capture_names().iter().enumerate() {
                if let Some(ref name) = *name {
                    names.insert(name.to_string(), i);
                }
            }
            RegexMatcher { regex, names }
        })
    }

    /// 启用大小写不敏感匹配。
    ///
    /// 如果 `utf` 选项也被设置，那么将使用 Unicode 大小写折叠来确定大小写不敏感。
    /// 当未设置 `utf` 选项时，仅考虑标准 ASCII 大小写不敏感。
    ///
    /// 此选项对应于 `i` 标志。
    pub fn caseless(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.builder.caseless(yes);
        self
    }

    /// 是否启用 "智能大小写"。
    ///
    /// 当启用智能大小写时，构建器将根据模式的编写方式自动启用大小写不敏感匹配。
    /// 具体来说，当以下两个条件都被认为为真时，将启用大小写不敏感模式：
    ///
    /// 1. 模式中至少包含一个字面字符。例如，`a\w` 包含一个字面字符（`a`），
    ///    但 `\w` 不包含字面字符。
    /// 2. 在模式的字面字符中，没有一个被认为是 Unicode 大写。例如，`foo\pL` 没有
    ///    大写字面字符，但 `Foo\pL` 有。
    ///
    /// 请注意，这个实现并不完美。也就是说，`\p{Ll}` 将阻止大小写不敏感匹配，即使它是元序列的一部分。
    /// 这个错误可能永远不会被修复。
    pub fn case_smart(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.case_smart = yes;
        self
    }

    /// 启用 "点对所有" 匹配。
    ///
    /// 启用时，模式中的 `.` 元字符将匹配任何字符，包括 `\n`。
    /// 禁用时（默认情况下），`.` 将匹配除了 `\n` 之外的任何字符。
    ///
    /// 此选项对应于 `s` 标志。
    pub fn dotall(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.builder.dotall(yes);
        self
    }

    /// 在模式中启用 "扩展" 模式，其中忽略空格。
    ///
    /// 此选项对应于 `x` 标志。
    pub fn extended(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.builder.extended(yes);
        self
    }

    /// 启用多行匹配模式。
    ///
    /// 启用时，`^` 和 `$` 锚将在主题字符串的开头和结尾以及行的开头和结尾匹配。
    /// 禁用时，`^` 和 `$` 锚只会在主题字符串的开头和结尾匹配。
    ///
    /// 此选项对应于 `m` 标志。
    pub fn multi_line(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.builder.multi_line(yes);
        self
    }

    /// 启用将 CRLF 视为行终止符。
    ///
    /// 启用时，`^` 和 `$` 等锚点将匹配以下任何一个作为行终止符：`\r`、`\n` 或 `\r\n`。
    ///
    /// 默认情况下，此选项已禁用，只有 `\n` 被识别为行终止符。
    pub fn crlf(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.builder.crlf(yes);
        self
    }

    /// 要求所有匹配都发生在单词边界上。
    ///
    /// 启用此选项与在模式两侧放置 `\b` 断言略有不同。特别是，`\b` 断言要求其中一侧匹配
    /// 单词字符，另一侧匹配非单词字符。相反，此选项只要求其中一侧匹配非单词字符。
    ///
    /// 例如，`\b-2\b` 不会匹配 `foo -2 bar`，因为 `-` 不是单词字符。
    /// 然而，当启用 `word` 选项时，`-2` 将匹配 `foo -2 bar` 中的 `-2`。
    pub fn word(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.word = yes;
        self
    }

    /// 是否将模式视为字面字符串。当启用此选项时，所有字符，包括通常是正则表达式元字符的字符，
    /// 都会被字面匹配。
    pub fn fixed_strings(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.fixed_strings = yes;
        self
    }

    /// 每个模式是否应该匹配整行。这相当于在模式周围添加 `(?m:^)` 和 `(?m:$)`。
    pub fn whole_line(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.whole_line = yes;
        self
    }

    /// 启用 Unicode 匹配模式。
    ///
    /// 启用时，以下模式变得支持 Unicode：`\b`、`\B`、`\d`、`\D`、`\s`、`\S`、`\w`、`\W`。
    ///
    /// 启用此选项还意味着启用 UTF 匹配模式。不可能在不启用 UTF 匹配模式的情况下启用 Unicode 匹配模式。
    ///
    /// 默认情况下，此选项已禁用。
    pub fn ucp(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.builder.ucp(yes);
        self
    }

    /// 启用 UTF 匹配模式。
    ///
    /// 启用时，字符被视为由组成单个码点的代码单元序列，而不是作为单个字节。例如，这将导致
    /// `.` 匹配任何单个 UTF-8 编码的码点，而当禁用时，`.` 将匹配任何单个字节
    /// （除了 `\n`，无论是否启用了 "点对所有" 模式）。
    ///
    /// 请注意，当启用 UTF 匹配模式时，每次搜索都会进行 UTF-8 验证检查，这可能会影响性能。
    /// 可以通过 `disable_utf_check` 选项禁用 UTF-8 检查，但启用 UTF 匹配模式并搜索无效的 UTF-8 是未定义的行为。
    ///
    /// 默认情况下，此选项已禁用。
    pub fn utf(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.builder.utf(yes);
        self
    }

    /// 此选项现已弃用，并且不起作用。
    ///
    /// 以前，此选项允许禁用 PCRE2 的 UTF-8 有效性检查，如果主串无效的 UTF-8，可能会导致未定义的行为。
    /// 但是，在 10.34 版本中，PCRE2 引入了一个新选项 `PCRE2_MATCH_INVALID_UTF`，
    /// 该 crate 总是设置此选项。当启用此选项时，PCRE2 声称在主串无效的 UTF-8 时不会有未定义的行为。
    ///
    /// 因此，此 crate 并未暴露禁用 UTF-8 检查的功能。
    #[deprecated(
        since = "0.2.4",
        note = "因为新的 PCRE2 特性，现在已经是无操作"
    )]
    pub fn disable_utf_check(&mut self) -> &mut RegexMatcherBuilder {
        self
    }

    /// 如果可用，启用 PCRE2 的 JIT 并在不可用时返回错误。
    ///
    /// 通常情况下，这会大大加速匹配。缺点是可能会增加模式编译时间。
    ///
    /// 如果 JIT 不可用，或者 JIT 编译返回错误，则正则表达式编译将失败并返回相应的错误。
    ///
    /// 默认情况下，此选项已禁用，并且始终会覆盖 `jit_if_available`。
    pub fn jit(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.builder.jit(yes);
        self
    }

    /// 如果可用，启用 PCRE2 的 JIT。
    ///
    /// 通常情况下，这会大大加速匹配。缺点是可能会增加模式编译时间。
    ///
    /// 如果 JIT 不可用，或者 JIT 编译返回错误，则将发出带有错误的调试消息，正则表达式
    /// 将静默地回退到非 JIT 匹配。
    ///
    /// 默认情况下，此选项已禁用，并且始终会覆盖 `jit`。
    pub fn jit_if_available(&mut self, yes: bool) -> &mut RegexMatcherBuilder {
        self.builder.jit_if_available(yes);
        self
    }

    /// 设置 PCRE2 JIT 栈的最大大小，以字节为单位。如果未启用 JIT，则此选项无效。
    ///
    /// 当给出 `None` 时，不会创建自定义的 JIT 栈，而是使用默认的 JIT 栈。当使用默认值时，
    /// 其最大大小为 32 KB。
    ///
    /// 当设置此选项时，将使用给定的最大大小创建新的 JIT 栈。
    ///
    /// 增大栈大小对于较大的正则表达式很有用。
    ///
    /// 默认情况下，此选项设置为 `None`。
    pub fn max_jit_stack_size(
        &mut self,
        bytes: Option<usize>,
    ) -> &mut RegexMatcherBuilder {
        self.builder.max_jit_stack_size(bytes);
        self
    }
}
/// 使用 PCRE2 实现的 `Matcher` 特性的实现。
#[derive(Clone, Debug)]
pub struct RegexMatcher {
    regex: Regex,
    names: HashMap<String, usize>,
}

impl RegexMatcher {
    /// 从给定的模式使用默认配置创建新的匹配器。
    pub fn new(pattern: &str) -> Result<RegexMatcher, Error> {
        RegexMatcherBuilder::new().build(pattern)
    }
}

impl Matcher for RegexMatcher {
    type Captures = RegexCaptures;
    type Error = Error;

    fn find_at(
        &self,
        haystack: &[u8],
        at: usize,
    ) -> Result<Option<Match>, Error> {
        Ok(self
            .regex
            .find_at(haystack, at)
            .map_err(Error::regex)?
            .map(|m| Match::new(m.start(), m.end())))
    }

    fn new_captures(&self) -> Result<RegexCaptures, Error> {
        Ok(RegexCaptures::new(self.regex.capture_locations()))
    }

    fn capture_count(&self) -> usize {
        self.regex.captures_len()
    }

    fn capture_index(&self, name: &str) -> Option<usize> {
        self.names.get(name).map(|i| *i)
    }

    fn try_find_iter<F, E>(
        &self,
        haystack: &[u8],
        mut matched: F,
    ) -> Result<Result<(), E>, Error>
    where
        F: FnMut(Match) -> Result<bool, E>,
    {
        for result in self.regex.find_iter(haystack) {
            let m = result.map_err(Error::regex)?;
            match matched(Match::new(m.start(), m.end())) {
                Ok(true) => continue,
                Ok(false) => return Ok(Ok(())),
                Err(err) => return Ok(Err(err)),
            }
        }
        Ok(Ok(()))
    }

    fn captures_at(
        &self,
        haystack: &[u8],
        at: usize,
        caps: &mut RegexCaptures,
    ) -> Result<bool, Error> {
        Ok(self
            .regex
            .captures_read_at(&mut caps.locs, haystack, at)
            .map_err(Error::regex)?
            .is_some())
    }
}

/// 表示匹配中每个捕获组的偏移量。
///
/// 第一个，或 `0` 号捕获组，总是对应整个匹配，并且在发生匹配时保证存在。
/// 下一个捕获组，在索引 `1` 处，对应正则表达式中出现的左括号位置排序的第一个捕获组。
///
/// 注意，并非所有捕获组在匹配中都保证存在。例如，在正则表达式 `(?P<foo>\w)|(?P<bar>\W)` 中，
/// `foo` 或 `bar` 只会在任何给定的匹配中设置一个。
///
/// 要通过名称访问捕获组，您需要首先使用相应匹配器的 `capture_index` 方法找到组的索引，
/// 然后将该索引与 `RegexCaptures::get` 一起使用。
#[derive(Clone, Debug)]
pub struct RegexCaptures {
    /// 位置信息存储。
    locs: CaptureLocations,
}

impl Captures for RegexCaptures {
    fn len(&self) -> usize {
        self.locs.len()
    }

    fn get(&self, i: usize) -> Option<Match> {
        self.locs.get(i).map(|(s, e)| Match::new(s, e))
    }
}

impl RegexCaptures {
    pub(crate) fn new(locs: CaptureLocations) -> RegexCaptures {
        RegexCaptures { locs }
    }
}

/// 确定模式中是否包含应该取消智能大小写选项效果的大写字符。
///
/// 理想情况下，我们应该能够检查 AST 以正确处理诸如 `\p{Ll}` 和 `\p{Lu}`（应被视为明确大小写）的情况，
/// 但 PCRE 不提供足够的细节来进行此类分析。目前，我们的“足够好”的解决方案是对输入模式进行半幼稚扫描，
/// 并忽略在反斜杠 `\` 后的所有字符。这至少让我们支持最常见的情况，如 `foo\w` 和 `foo\S`，以直观的方式。
fn has_uppercase_literal(pattern: &str) -> bool {
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if c.is_uppercase() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use grep_matcher::{LineMatchKind, Matcher};

    // 测试启用单词匹配是否起作用，并演示它与用 `\b` 包围正则表达式的区别。
    #[test]
    fn word() {
        let matcher =
            RegexMatcherBuilder::new().word(true).build(r"-2").unwrap();
        assert!(matcher.is_match(b"abc -2 foo").unwrap());

        let matcher =
            RegexMatcherBuilder::new().word(false).build(r"\b-2\b").unwrap();
        assert!(!matcher.is_match(b"abc -2 foo").unwrap());
    }

    // 测试启用 CRLF 是否允许 `$` 在行尾匹配。
    #[test]
    fn line_terminator_crlf() {
        // 测试在 `\n` 行终止符情况下 `$` 的正常用法。
        let matcher = RegexMatcherBuilder::new()
            .multi_line(true)
            .build(r"abc$")
            .unwrap();
        assert!(matcher.is_match(b"abc\n").unwrap());

        // 测试 `$` 在 `\r\n` 边界正常情况下不匹配。
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

    // 测试查找候选行是否按预期工作。
    #[test]
    fn candidate_lines() {
        fn is_confirmed(m: LineMatchKind) -> bool {
            match m {
                LineMatchKind::Confirmed(_) => true,
                _ => false,
            }
        }

        let matcher = RegexMatcherBuilder::new().build(r"\wfoo\s").unwrap();
        let m = matcher.find_candidate_line(b"afoo ").unwrap().unwrap();
        assert!(is_confirmed(m));
    }
}
