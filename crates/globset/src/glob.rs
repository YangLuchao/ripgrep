use std::fmt;
use std::hash;
use std::iter;
use std::ops::{Deref, DerefMut};
use std::path::{is_separator, Path};
use std::str;

use regex;
use regex::bytes::Regex;

use crate::{new_regex, Candidate, Error, ErrorKind};
/// 描述特定模式的匹配策略。
///
/// 这提供了一种更快速地确定模式是否与特定文件路径匹配的方法，以便在大量模式的情况下进行扩展。
/// 例如，如果许多模式的形式是 `*.ext`，则可以通过在哈希表中查找文件路径的扩展名来测试任何这些模式是否匹配。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MatchStrategy {
    /// 如果整个文件路径与此字面字符串匹配，则模式匹配。
    Literal(String),
    /// 如果文件路径的基名与此字面字符串匹配，则模式匹配。
    BasenameLiteral(String),
    /// 如果文件路径的扩展名与此字面字符串匹配，则模式匹配。
    Extension(String),
    /// 如果此前缀字面字符串是候选文件路径的前缀，则模式匹配。
    Prefix(String),
    /// 如果此前缀字面字符串是候选文件路径的前缀，则模式匹配。
    ///
    /// 例外情况：如果 `component` 为 true，则 `suffix` 必须出现在文件路径的开头或紧接着 `/` 后面。
    Suffix {
        /// 实际的后缀。
        suffix: String,
        /// 是否必须从路径组件的开头开始。
        component: bool,
    },
    /// 仅当给定的扩展名与文件路径的扩展名匹配时，模式才匹配。
    /// 请注意，这是一个必要但不充分的条件。
    /// 也就是说，如果扩展名匹配，则仍然需要进行完整的正则表达式搜索。
    RequiredExtension(String),
    /// 需要使用正则表达式进行匹配。
    Regex,
}

impl MatchStrategy {
    /// 根据给定的模式返回匹配策略。
    pub fn new(pat: &Glob) -> MatchStrategy {
        if let Some(lit) = pat.basename_literal() {
            MatchStrategy::BasenameLiteral(lit)
        } else if let Some(lit) = pat.literal() {
            MatchStrategy::Literal(lit)
        } else if let Some(ext) = pat.ext() {
            MatchStrategy::Extension(ext)
        } else if let Some(prefix) = pat.prefix() {
            MatchStrategy::Prefix(prefix)
        } else if let Some((suffix, component)) = pat.suffix() {
            MatchStrategy::Suffix { suffix: suffix, component: component }
        } else if let Some(ext) = pat.required_ext() {
            MatchStrategy::RequiredExtension(ext)
        } else {
            MatchStrategy::Regex
        }
    }
}

/// Glob 表示成功解析的 shell glob 模式。
///
/// 它不能直接用于匹配文件路径，但可以转换为正则表达式字符串或匹配器。
#[derive(Clone, Debug, Eq)]
pub struct Glob {
    glob: String,
    re: String,
    opts: GlobOptions,
    tokens: Tokens,
}

impl PartialEq for Glob {
    fn eq(&self, other: &Glob) -> bool {
        self.glob == other.glob && self.opts == other.opts
    }
}

impl hash::Hash for Glob {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.glob.hash(state);
        self.opts.hash(state);
    }
}

impl fmt::Display for Glob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.glob.fmt(f)
    }
}

impl str::FromStr for Glob {
    type Err = Error;

    fn from_str(glob: &str) -> Result<Self, Self::Err> {
        Self::new(glob)
    }
}

/// 用于单个模式的匹配器。
#[derive(Clone, Debug)]
pub struct GlobMatcher {
    /// 底层模式。
    pat: Glob,
    /// 模式，作为编译后的正则表达式。
    re: Regex,
}

impl GlobMatcher {
    /// 测试给定的路径是否与此模式匹配。
    pub fn is_match<P: AsRef<Path>>(&self, path: P) -> bool {
        self.is_match_candidate(&Candidate::new(path.as_ref()))
    }

    /// 测试给定的路径是否与此模式匹配。
    pub fn is_match_candidate(&self, path: &Candidate<'_>) -> bool {
        self.re.is_match(&path.path)
    }

    /// 返回用于编译此匹配器的 `Glob`。
    pub fn glob(&self) -> &Glob {
        &self.pat
    }
}

/// 用于单个模式的战略匹配器。
#[cfg(test)]
#[derive(Clone, Debug)]
struct GlobStrategic {
    /// 要使用的匹配策略。
    strategy: MatchStrategy,
    /// 模式，作为编译后的正则表达式。
    re: Regex,
}

#[cfg(test)]
impl GlobStrategic {
    /// 测试给定的路径是否与此模式匹配。
    fn is_match<P: AsRef<Path>>(&self, path: P) -> bool {
        self.is_match_candidate(&Candidate::new(path.as_ref()))
    }

    /// 测试给定的路径是否与此模式匹配。
    fn is_match_candidate(&self, candidate: &Candidate<'_>) -> bool {
        let byte_path = &*candidate.path;

        match self.strategy {
            MatchStrategy::Literal(ref lit) => lit.as_bytes() == byte_path,
            MatchStrategy::BasenameLiteral(ref lit) => {
                lit.as_bytes() == &*candidate.basename
            }
            MatchStrategy::Extension(ref ext) => {
                ext.as_bytes() == &*candidate.ext
            }
            MatchStrategy::Prefix(ref pre) => {
                starts_with(pre.as_bytes(), byte_path)
            }
            MatchStrategy::Suffix { ref suffix, component } => {
                if component && byte_path == &suffix.as_bytes()[1..] {
                    return true;
                }
                ends_with(suffix.as_bytes(), byte_path)
            }
            MatchStrategy::RequiredExtension(ref ext) => {
                let ext = ext.as_bytes();
                &*candidate.ext == ext && self.re.is_match(byte_path)
            }
            MatchStrategy::Regex => self.re.is_match(byte_path),
        }
    }
}

/// 用于模式的构建器。
///
/// 此构建器使得可以配置模式的匹配语义。例如，可以使匹配不区分大小写。
///
/// 生命周期 `'a` 是模式字符串的生命周期。
#[derive(Clone, Debug)]
pub struct GlobBuilder<'a> {
    /// 要编译的 glob 模式。
    glob: &'a str,
    /// 模式的选项。
    opts: GlobOptions,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct GlobOptions {
    /// 是否进行不区分大小写的匹配。
    case_insensitive: bool,
    /// 是否要求字面分隔符与文件路径中的分隔符匹配。例如，启用时，`*` 不会匹配 `/`。
    literal_separator: bool,
    /// 是否使用 `\` 来转义特殊字符。
    /// 例如，启用时，`\*` 将匹配字面 `*`。
    backslash_escape: bool,
    /// 是否删除替代中的空情况。
    /// 例如，启用时，`{,a}` 将匹配 "" 和 "a"。
    empty_alternates: bool,
}

impl GlobOptions {
    fn default() -> GlobOptions {
        GlobOptions {
            case_insensitive: false,
            literal_separator: false,
            backslash_escape: !is_separator('\\'),
            empty_alternates: false,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Tokens(Vec<Token>);

impl Deref for Tokens {
    type Target = Vec<Token>;
    fn deref(&self) -> &Vec<Token> {
        &self.0
    }
}

impl DerefMut for Tokens {
    fn deref_mut(&mut self) -> &mut Vec<Token> {
        &mut self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Token {
    Literal(char),
    Any,
    ZeroOrMore,
    RecursivePrefix,
    RecursiveSuffix,
    RecursiveZeroOrMore,
    Class { negated: bool, ranges: Vec<(char, char)> },
    Alternates(Vec<Tokens>),
}
impl Glob {
    /// 使用默认选项构建新模式。
    pub fn new(glob: &str) -> Result<Glob, Error> {
        GlobBuilder::new(glob).build()
    }

    /// 返回此模式的匹配器。
    pub fn compile_matcher(&self) -> GlobMatcher {
        let re = new_regex(&self.re).expect("正则表达式编译不应失败");
        GlobMatcher { pat: self.clone(), re: re }
    }

    /// 返回战略匹配器。
    ///
    /// 这并未公开，因为目前不清楚是否比仅为 *单个* 模式运行正则表达式要快。
    /// 如果更快，那么 GlobMatcher 应该自动执行。
    #[cfg(test)]
    fn compile_strategic_matcher(&self) -> GlobStrategic {
        let strategy = MatchStrategy::new(self);
        let re = new_regex(&self.re).expect("正则表达式编译不应失败");
        GlobStrategic { strategy, re }
    }

    /// 返回用于构建此模式的原始 glob 模式。
    pub fn glob(&self) -> &str {
        &self.glob
    }

    /// 返回此 glob 的正则表达式字符串。
    ///
    /// 请注意，用于 glob 的正则表达式旨在与任意字节 (`&[u8]`) 进行匹配，而不是 Unicode 字符串 (`&str`)。
    /// 特别是，glob 通常用于文件路径，其中通常无法保证文件路径本身是有效的 UTF-8 编码。
    /// 因此，调用者需要确保他们使用的是可以在任意字节上进行匹配的正则表达式 API。
    /// 例如，`regex` 库的 `Regex` API 不适用于此，因为它基于 `&str` 进行匹配，但其 `bytes::Regex` API 适用于此。
    pub fn regex(&self) -> &str {
        &self.re
    }

    /// 返回字面模式，仅当模式必须完全匹配整个路径时才返回。
    ///
    /// 这些模式的基本格式是 `{字面}`。
    fn literal(&self) -> Option<String> {
        if self.opts.case_insensitive {
            return None;
        }
        let mut lit = String::new();
        for t in &*self.tokens {
            match *t {
                Token::Literal(c) => lit.push(c),
                _ => return None,
            }
        }
        if lit.is_empty() {
            None
        } else {
            Some(lit)
        }
    }

    /// 如果此模式匹配的文件路径仅在文件路径具有返回的扩展名时才返回扩展名。
    ///
    /// 请注意，此处返回的扩展名与 std::path::Path::extension 返回的扩展名不同。
    /// 即，此处的扩展名包括 '.'。此外，路径如 `.rs` 被视为具有扩展名 `.rs`。
    fn ext(&self) -> Option<String> {
        if self.opts.case_insensitive {
            return None;
        }
        let start = match self.tokens.get(0) {
            Some(&Token::RecursivePrefix) => 1,
            Some(_) => 0,
            _ => return None,
        };
        match self.tokens.get(start) {
            Some(&Token::ZeroOrMore) => {
                // 如果没有递归前缀，则仅当 `*` 可以匹配 `/` 时，我们才允许 `*`。
                // 例如，如果 `*` 无法匹配 `/`，则 `*.c` 不会匹配 `foo/bar.c`。
                if start == 0 && self.opts.literal_separator {
                    return None;
                }
            }
            _ => return None,
        }
        match self.tokens.get(start + 1) {
            Some(&Token::Literal('.')) => {}
            _ => return None,
        }
        let mut lit = ".".to_string();
        for t in self.tokens[start + 2..].iter() {
            match *t {
                Token::Literal('.') | Token::Literal('/') => return None,
                Token::Literal(c) => lit.push(c),
                _ => return None,
            }
        }
        if lit.is_empty() {
            None
        } else {
            Some(lit)
        }
    }

    /// 类似于 `ext`，但即使它不足以暗示匹配，也会返回扩展名。
    /// 也就是说，如果返回了扩展名，则它是必要但不充分的条件。
    fn required_ext(&self) -> Option<String> {
        if self.opts.case_insensitive {
            return None;
        }
        // 我们不关心此模式的开头。我们唯一需要检查的就是它是否以 `.ext` 字面结尾。
        let mut ext: Vec<char> = vec![]; // 以相反的顺序构建
        for t in self.tokens.iter().rev() {
            match *t {
                Token::Literal('/') => return None,
                Token::Literal(c) => {
                    ext.push(c);
                    if c == '.' {
                        break;
                    }
                }
                _ => return None,
            }
        }
        if ext.last() != Some(&'.') {
            None
        } else {
            ext.reverse();
            Some(ext.into_iter().collect())
        }
    }

    /// 返回此模式的字面前缀，仅当整个模式匹配时字面前缀才匹配。
    fn prefix(&self) -> Option<String> {
        if self.opts.case_insensitive {
            return None;
        }
        let (end, need_sep) = match self.tokens.last() {
            Some(&Token::ZeroOrMore) => {
                if self.opts.literal_separator {
                    // 如果尾部的 `*` 不能匹配 `/`，则我们不能假定前缀的匹配对应于整个模式的匹配。
                    // 例如，启用 `literal_separator` 的情况下，`foo/*` 匹配 `foo/bar`，但不匹配 `foo/bar/baz`，
                    // 即使 `foo/bar/baz` 有一个 `foo/` 的字面前缀。
                    return None;
                }
                (self.tokens.len() - 1, false)
            }
            Some(&Token::RecursiveSuffix) => (self.tokens.len() - 1, true),
            _ => (self.tokens.len(), false),
        };
        let mut lit = String::new();
        for t in &self.tokens[0..end] {
            match *t {
                Token::Literal(c) => lit.push(c),
                _ => return None,
            }
        }
        if need_sep {
            lit.push('/');
        }
        if lit.is_empty() {
            None
        } else {
            Some(lit)
        }
    }

    /// 返回此模式的字面后缀，仅当整个模式匹配时字面后缀才匹配。
    ///
    /// 如果字面后缀必须匹配整个文件路径或位于 `/` 之前，则返回 true。
    /// 这种情况发生在模式类似于 `**/foo/bar`。换句话说，此模式匹配 `foo/bar` 和 `baz/foo/bar`，
    /// 但不匹配 `foofoo/bar`。在这种情况下，返回的后缀是 `/foo/bar`（但应与整个路径 `foo/bar` 匹配）。
    fn suffix(&self) -> Option<(String, bool)> {
        if self.opts.case_insensitive {
            return None;
        }
        let mut lit = String::new();
        let (start, entire) = match self.tokens.get(0) {
            Some(&Token::RecursivePrefix) => {
                // 仅当下一个标记是字面时，我们才关心是否跟随路径组件。
                if let Some(&Token::Literal(_)) = self.tokens.get(1) {
                    lit.push('/');
                    (1, true)
                } else {
                    (1, false)
                }
            }
            _ => (0, false),
        };
        let start = match self.tokens.get(start) {
            Some(&Token::ZeroOrMore) => {
                // 如果启用了 literal_separator，则 `*` 可以不必然地匹配一切，
                // 因此将后缀匹配报告为模式匹配是误报。
                if self.opts.literal_separator {
                    return None;
                }
                start + 1
            }
            _ => start,
        };
        for t in &self.tokens[start..] {
            match *t {
                Token::Literal(c) => lit.push(c),
                _ => return None,
            }
        }
        if lit.is_empty() || lit == "/" {
            None
        } else {
            Some((lit, entire))
        }
    }

    /// 如果此模式仅需要检查文件路径的基名，则返回仅基名匹配的标记。
    ///
    /// 例如，对于模式 `**/*.foo`，仅返回与 `*.foo` 对应的标记。
    ///
    /// 请注意，如果基名标记的任何匹配不对应于整个模式的匹配，则将返回 None。
    /// 例如，glob `foo` 仅在文件路径具有基名 `foo` 时匹配，但并不总是在文件路径具有基名 `foo` 时都匹配。
    /// 例如，`foo` 不匹配 `abc/foo`。
    fn basename_tokens(&self) -> Option<&[Token]> {
        if self.opts.case_insensitive {
            return None;
        }
        let start = match self.tokens.get(0) {
            Some(&Token::RecursivePrefix) => 1,
            _ => {
                // 没有任何内容来处理路径的父部分，因此不能假设仅在基名上进行匹配是正确的。
                return None;
            }
        };
        if self.tokens[start..].is_empty() {
            return None;
        }
        for t in &self.tokens[start..] {
            match *t {
                Token::Literal('/') => return None,
                Token::Literal(_) => {} // 可以
                Token::Any | Token::ZeroOrMore => {
                    if !self.opts.literal_separator {
                        // 在这种情况下，`*` 和 `?` 可以匹配路径分隔符，这意味着它可能会超出基名。
                        return None;
                    }
                }
                Token::RecursivePrefix
                | Token::RecursiveSuffix
                | Token::RecursiveZeroOrMore => {
                    return None;
                }
                Token::Class { .. } | Token::Alternates(..) => {
                    // 我们可以更聪明一些，但这两者之一都将阻止我们的字面优化，所以放弃。
                    return None;
                }
            }
        }
        Some(&self.tokens[start..])
    }

    /// 仅当模式完全匹配文件路径的基名 *且*是字面时，将模式作为字面返回。
    ///
    /// 这些模式的基本格式是 `**/{字面}`，其中 `{字面}` 不包含路径分隔符。
    fn basename_literal(&self) -> Option<String> {
        let tokens = match self.basename_tokens() {
            None => return None,
            Some(tokens) => tokens,
        };
        let mut lit = String::new();
        for t in tokens {
            match *t {
                Token::Literal(c) => lit.push(c),
                _ => return None,
            }
        }
        Some(lit)
    }
}
impl<'a> GlobBuilder<'a> {
    /// 创建给定模式的新构建器。
    ///
    /// 只有在调用 `build` 时，模式才会被编译。
    pub fn new(glob: &'a str) -> GlobBuilder<'a> {
        GlobBuilder { glob, opts: GlobOptions::default() }
    }

    /// 解析并构建模式。
    pub fn build(&self) -> Result<Glob, Error> {
        let mut p = Parser {
            glob: &self.glob,
            stack: vec![Tokens::default()],
            chars: self.glob.chars().peekable(),
            prev: None,
            cur: None,
            opts: &self.opts,
        };
        p.parse()?;
        if p.stack.is_empty() {
            Err(Error {
                glob: Some(self.glob.to_string()),
                kind: ErrorKind::UnopenedAlternates,
            })
        } else if p.stack.len() > 1 {
            Err(Error {
                glob: Some(self.glob.to_string()),
                kind: ErrorKind::UnclosedAlternates,
            })
        } else {
            let tokens = p.stack.pop().unwrap();
            Ok(Glob {
                glob: self.glob.to_string(),
                re: tokens.to_regex_with(&self.opts),
                opts: self.opts,
                tokens,
            })
        }
    }

    /// 切换模式是否进行大小写不敏感匹配。
    ///
    /// 默认情况下，此选项被禁用。
    pub fn case_insensitive(&mut self, yes: bool) -> &mut GlobBuilder<'a> {
        self.opts.case_insensitive = yes;
        self
    }

    /// 切换是否需要字面的 `/` 来匹配路径分隔符。
    ///
    /// 默认情况下，这是 false：`*` 和 `?` 将匹配 `/`。
    pub fn literal_separator(&mut self, yes: bool) -> &mut GlobBuilder<'a> {
        self.opts.literal_separator = yes;
        self
    }

    /// 当启用时，可以使用反斜杠（`\`）来转义 glob 模式中的特殊字符。
    /// 此外，在所有平台上都将防止将 `\` 解释为路径分隔符。
    ///
    /// 在 `\` 不是路径分隔符的平台上默认启用，在 `\` 是路径分隔符的平台上默认禁用。
    pub fn backslash_escape(&mut self, yes: bool) -> &mut GlobBuilder<'a> {
        self.opts.backslash_escape = yes;
        self
    }

    /// 切换在替代列表中是否接受空模式。
    ///
    /// 例如，如果设置了这个选项，那么 glob `foo{,.txt}` 将同时匹配 `foo` 和 `foo.txt`。
    ///
    /// 默认情况下，这是 false。
    pub fn empty_alternates(&mut self, yes: bool) -> &mut GlobBuilder<'a> {
        self.opts.empty_alternates = yes;
        self
    }
}

impl Tokens {
    /// 将此模式转换为保证是有效的正则表达式且将表示此 glob 模式的匹配语义和给定选项的字符串。
    fn to_regex_with(&self, options: &GlobOptions) -> String {
        let mut re = String::new();
        re.push_str("(?-u)");
        if options.case_insensitive {
            re.push_str("(?i)");
        }
        re.push('^');
        // 特殊情况。如果整个 glob 仅为 `**`，则应该匹配一切。
        if self.len() == 1 && self[0] == Token::RecursivePrefix {
            re.push_str(".*");
            re.push('$');
            return re;
        }
        self.tokens_to_regex(options, &self, &mut re);
        re.push('$');
        re
    }

    fn tokens_to_regex(
        &self,
        options: &GlobOptions,
        tokens: &[Token],
        re: &mut String,
    ) {
        for tok in tokens {
            match *tok {
                Token::Literal(c) => {
                    re.push_str(&char_to_escaped_literal(c));
                }
                Token::Any => {
                    if options.literal_separator {
                        re.push_str("[^/]");
                    } else {
                        re.push_str(".");
                    }
                }
                Token::ZeroOrMore => {
                    if options.literal_separator {
                        re.push_str("[^/]*");
                    } else {
                        re.push_str(".*");
                    }
                }
                Token::RecursivePrefix => {
                    re.push_str("(?:/?|.*/)");
                }
                Token::RecursiveSuffix => {
                    re.push_str("/.*");
                }
                Token::RecursiveZeroOrMore => {
                    re.push_str("(?:/|/.*/)");
                }
                Token::Class { negated, ref ranges } => {
                    re.push('[');
                    if negated {
                        re.push('^');
                    }
                    for r in ranges {
                        if r.0 == r.1 {
                            // 不是严格必要的，但更好看。
                            re.push_str(&char_to_escaped_literal(r.0));
                        } else {
                            re.push_str(&char_to_escaped_literal(r.0));
                            re.push('-');
                            re.push_str(&char_to_escaped_literal(r.1));
                        }
                    }
                    re.push(']');
                }
                Token::Alternates(ref patterns) => {
                    let mut parts = vec![];
                    for pat in patterns {
                        let mut altre = String::new();
                        self.tokens_to_regex(options, &pat, &mut altre);
                        if !altre.is_empty() || options.empty_alternates {
                            parts.push(altre);
                        }
                    }

                    // 可能会有一个空集，此时生成的交替 '()' 将是错误。
                    if !parts.is_empty() {
                        re.push_str("(?:");
                        re.push_str(&parts.join("|"));
                        re.push(')');
                    }
                }
            }
        }
    }
}

/// 将 Unicode 标量值转换为适用于在非 Unicode 正则表达式中用作字面的转义字符串。
fn char_to_escaped_literal(c: char) -> String {
    bytes_to_escaped_literal(&c.to_string().into_bytes())
}

/// 将任意字节序列转换为 UTF-8 字符串。所有非 ASCII 代码单元将转换为其转义形式。
fn bytes_to_escaped_literal(bs: &[u8]) -> String {
    let mut s = String::with_capacity(bs.len());
    for &b in bs {
        if b <= 0x7F {
            s.push_str(&regex::escape(&(b as char).to_string()));
        } else {
            s.push_str(&format!("\\x{:02x}", b));
        }
    }
    s
}
struct Parser<'a> {
    glob: &'a str,
    stack: Vec<Tokens>,
    chars: iter::Peekable<str::Chars<'a>>,
    prev: Option<char>,
    cur: Option<char>,
    opts: &'a GlobOptions,
}

impl<'a> Parser<'a> {
    fn error(&self, kind: ErrorKind) -> Error {
        Error { glob: Some(self.glob.to_string()), kind }
    }

    fn parse(&mut self) -> Result<(), Error> {
        while let Some(c) = self.bump() {
            match c {
                '?' => self.push_token(Token::Any)?,
                '*' => self.parse_star()?,
                '[' => self.parse_class()?,
                '{' => self.push_alternate()?,
                '}' => self.pop_alternate()?,
                ',' => self.parse_comma()?,
                '\\' => self.parse_backslash()?,
                c => self.push_token(Token::Literal(c))?,
            }
        }
        Ok(())
    }

    fn push_alternate(&mut self) -> Result<(), Error> {
        if self.stack.len() > 1 {
            return Err(self.error(ErrorKind::NestedAlternates));
        }
        Ok(self.stack.push(Tokens::default()))
    }

    fn pop_alternate(&mut self) -> Result<(), Error> {
        let mut alts = vec![];
        while self.stack.len() >= 2 {
            alts.push(self.stack.pop().unwrap());
        }
        self.push_token(Token::Alternates(alts))
    }

    fn push_token(&mut self, tok: Token) -> Result<(), Error> {
        if let Some(ref mut pat) = self.stack.last_mut() {
            return Ok(pat.push(tok));
        }
        Err(self.error(ErrorKind::UnopenedAlternates))
    }

    fn pop_token(&mut self) -> Result<Token, Error> {
        if let Some(ref mut pat) = self.stack.last_mut() {
            return Ok(pat.pop().unwrap());
        }
        Err(self.error(ErrorKind::UnopenedAlternates))
    }

    fn have_tokens(&self) -> Result<bool, Error> {
        match self.stack.last() {
            None => Err(self.error(ErrorKind::UnopenedAlternates)),
            Some(ref pat) => Ok(!pat.is_empty()),
        }
    }

    fn parse_comma(&mut self) -> Result<(), Error> {
        // 如果我们不在组替代中，那么不特殊处理逗号。否则，我们需要开始新的替代。
        if self.stack.len() <= 1 {
            self.push_token(Token::Literal(','))
        } else {
            Ok(self.stack.push(Tokens::default()))
        }
    }

    fn parse_backslash(&mut self) -> Result<(), Error> {
        if self.opts.backslash_escape {
            match self.bump() {
                None => Err(self.error(ErrorKind::DanglingEscape)),
                Some(c) => self.push_token(Token::Literal(c)),
            }
        } else if is_separator('\\') {
            // 将所有模式标准化为使用 / 作为分隔符。
            self.push_token(Token::Literal('/'))
        } else {
            self.push_token(Token::Literal('\\'))
        }
    }

    fn parse_star(&mut self) -> Result<(), Error> {
        let prev = self.prev;
        if self.peek() != Some('*') {
            self.push_token(Token::ZeroOrMore)?;
            return Ok(());
        }
        assert!(self.bump() == Some('*'));
        if !self.have_tokens()? {
            if !self.peek().map_or(true, is_separator) {
                self.push_token(Token::ZeroOrMore)?;
                self.push_token(Token::ZeroOrMore)?;
            } else {
                self.push_token(Token::RecursivePrefix)?;
                assert!(self.bump().map_or(true, is_separator));
            }
            return Ok(());
        }

        if !prev.map(is_separator).unwrap_or(false) {
            if self.stack.len() <= 1
                || (prev != Some(',') && prev != Some('{'))
            {
                self.push_token(Token::ZeroOrMore)?;
                self.push_token(Token::ZeroOrMore)?;
                return Ok(());
            }
        }
        let is_suffix = match self.peek() {
            None => {
                assert!(self.bump().is_none());
                true
            }
            Some(',') | Some('}') if self.stack.len() >= 2 => true,
            Some(c) if is_separator(c) => {
                assert!(self.bump().map(is_separator).unwrap_or(false));
                false
            }
            _ => {
                self.push_token(Token::ZeroOrMore)?;
                self.push_token(Token::ZeroOrMore)?;
                return Ok(());
            }
        };
        match self.pop_token()? {
            Token::RecursivePrefix => {
                self.push_token(Token::RecursivePrefix)?;
            }
            Token::RecursiveSuffix => {
                self.push_token(Token::RecursiveSuffix)?;
            }
            _ => {
                if is_suffix {
                    self.push_token(Token::RecursiveSuffix)?;
                } else {
                    self.push_token(Token::RecursiveZeroOrMore)?;
                }
            }
        }
        Ok(())
    }

    fn parse_class(&mut self) -> Result<(), Error> {
        fn add_to_last_range(
            glob: &str,
            r: &mut (char, char),
            add: char,
        ) -> Result<(), Error> {
            r.1 = add;
            if r.1 < r.0 {
                Err(Error {
                    glob: Some(glob.to_string()),
                    kind: ErrorKind::InvalidRange(r.0, r.1),
                })
            } else {
                Ok(())
            }
        }
        let mut ranges = vec![];
        let negated = match self.chars.peek() {
            Some(&'!') | Some(&'^') => {
                let bump = self.bump();
                assert!(bump == Some('!') || bump == Some('^'));
                true
            }
            _ => false,
        };
        let mut first = true;
        let mut in_range = false;
        loop {
            let c = match self.bump() {
                Some(c) => c,
                // 唯一能成功中断此循环的方式是观察到 ']'。
                None => return Err(self.error(ErrorKind::UnclosedClass)),
            };
            match c {
                ']' => {
                    if first {
                        ranges.push((']', ']'));
                    } else {
                        break;
                    }
                }
                '-' => {
                    if first {
                        ranges.push(('-', '-'));
                    } else if in_range {
                        // 不变式：只有在已经看到至少一个字符时，才设置 in_range。
                        let r = ranges.last_mut().unwrap();
                        add_to_last_range(&self.glob, r, '-')?;
                        in_range = false;
                    } else {
                        assert!(!ranges.is_empty());
                        in_range = true;
                    }
                }
                c => {
                    if in_range {
                        // 不变式：只有在已经看到至少一个字符时，才设置 in_range。
                        add_to_last_range(
                            &self.glob,
                            ranges.last_mut().unwrap(),
                            c,
                        )?;
                    } else {
                        ranges.push((c, c));
                    }
                    in_range = false;
                }
            }
            first = false;
        }
        if in_range {
            // 意味着类中的最后一个字符是 '-'，因此将其添加为字面值。
            ranges.push(('-', '-'));
        }
        self.push_token(Token::Class { negated, ranges })
    }

    fn bump(&mut self) -> Option<char> {
        self.prev = self.cur;
        self.cur = self.chars.next();
        self.cur
    }

    fn peek(&mut self) -> Option<char> {
        self.chars.peek().copied()
    }
}

#[cfg(test)]
fn starts_with(needle: &[u8], haystack: &[u8]) -> bool {
    needle.len() <= haystack.len() && needle == &haystack[..needle.len()]
}

#[cfg(test)]
fn ends_with(needle: &[u8], haystack: &[u8]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    needle == &haystack[haystack.len() - needle.len()..]
}

#[cfg(test)]
mod tests {
    use super::Token::*;
    use super::{Glob, GlobBuilder, Token};
    use crate::{ErrorKind, GlobSetBuilder};

    #[derive(Clone, Copy, Debug, Default)]
    struct Options {
        casei: Option<bool>,
        litsep: Option<bool>,
        bsesc: Option<bool>,
        ealtre: Option<bool>,
    }

    macro_rules! syntax {
        ($name:ident, $pat:expr, $tokens:expr) => {
            #[test]
            fn $name() {
                let pat = Glob::new($pat).unwrap();
                assert_eq!($tokens, pat.tokens.0);
            }
        };
    }

    macro_rules! syntaxerr {
        ($name:ident, $pat:expr, $err:expr) => {
            #[test]
            fn $name() {
                let err = Glob::new($pat).unwrap_err();
                assert_eq!(&$err, err.kind());
            }
        };
    }

    macro_rules! toregex {
        ($name:ident, $pat:expr, $re:expr) => {
            toregex!($name, $pat, $re, Options::default());
        };
        ($name:ident, $pat:expr, $re:expr, $options:expr) => {
            #[test]
            fn $name() {
                let mut builder = GlobBuilder::new($pat);
                if let Some(casei) = $options.casei {
                    builder.case_insensitive(casei);
                }
                if let Some(litsep) = $options.litsep {
                    builder.literal_separator(litsep);
                }
                if let Some(bsesc) = $options.bsesc {
                    builder.backslash_escape(bsesc);
                }
                if let Some(ealtre) = $options.ealtre {
                    builder.empty_alternates(ealtre);
                }
                let pat = builder.build().unwrap();
                assert_eq!(format!("(?-u){}", $re), pat.regex());
            }
        };
    }

    macro_rules! matches {
        ($name:ident, $pat:expr, $path:expr) => {
            matches!($name, $pat, $path, Options::default());
        };
        ($name:ident, $pat:expr, $path:expr, $options:expr) => {
            #[test]
            fn $name() {
                let mut builder = GlobBuilder::new($pat);
                if let Some(casei) = $options.casei {
                    builder.case_insensitive(casei);
                }
                if let Some(litsep) = $options.litsep {
                    builder.literal_separator(litsep);
                }
                if let Some(bsesc) = $options.bsesc {
                    builder.backslash_escape(bsesc);
                }
                if let Some(ealtre) = $options.ealtre {
                    builder.empty_alternates(ealtre);
                }
                let pat = builder.build().unwrap();
                let matcher = pat.compile_matcher();
                let strategic = pat.compile_strategic_matcher();
                let set = GlobSetBuilder::new().add(pat).build().unwrap();
                assert!(matcher.is_match($path));
                assert!(strategic.is_match($path));
                assert!(set.is_match($path));
            }
        };
    }

    macro_rules! nmatches {
        ($name:ident, $pat:expr, $path:expr) => {
            nmatches!($name, $pat, $path, Options::default());
        };
        ($name:ident, $pat:expr, $path:expr, $options:expr) => {
            #[test]
            fn $name() {
                let mut builder = GlobBuilder::new($pat);
                if let Some(casei) = $options.casei {
                    builder.case_insensitive(casei);
                }
                if let Some(litsep) = $options.litsep {
                    builder.literal_separator(litsep);
                }
                if let Some(bsesc) = $options.bsesc {
                    builder.backslash_escape(bsesc);
                }
                if let Some(ealtre) = $options.ealtre {
                    builder.empty_alternates(ealtre);
                }
                let pat = builder.build().unwrap();
                let matcher = pat.compile_matcher();
                let strategic = pat.compile_strategic_matcher();
                let set = GlobSetBuilder::new().add(pat).build().unwrap();
                assert!(!matcher.is_match($path));
                assert!(!strategic.is_match($path));
                assert!(!set.is_match($path));
            }
        };
    }

    fn s(string: &str) -> String {
        string.to_string()
    }

    fn class(s: char, e: char) -> Token {
        Class { negated: false, ranges: vec![(s, e)] }
    }

    fn classn(s: char, e: char) -> Token {
        Class { negated: true, ranges: vec![(s, e)] }
    }

    fn rclass(ranges: &[(char, char)]) -> Token {
        Class { negated: false, ranges: ranges.to_vec() }
    }

    fn rclassn(ranges: &[(char, char)]) -> Token {
        Class { negated: true, ranges: ranges.to_vec() }
    }

    syntax!(literal1, "a", vec![Literal('a')]);
    syntax!(literal2, "ab", vec![Literal('a'), Literal('b')]);
    syntax!(any1, "?", vec![Any]);
    syntax!(any2, "a?b", vec![Literal('a'), Any, Literal('b')]);
    syntax!(seq1, "*", vec![ZeroOrMore]);
    syntax!(seq2, "a*b", vec![Literal('a'), ZeroOrMore, Literal('b')]);
    syntax!(
        seq3,
        "*a*b*",
        vec![ZeroOrMore, Literal('a'), ZeroOrMore, Literal('b'), ZeroOrMore,]
    );
    syntax!(rseq1, "**", vec![RecursivePrefix]);
    syntax!(rseq2, "**/", vec![RecursivePrefix]);
    syntax!(rseq3, "/**", vec![RecursiveSuffix]);
    syntax!(rseq4, "/**/", vec![RecursiveZeroOrMore]);
    syntax!(
        rseq5,
        "a/**/b",
        vec![Literal('a'), RecursiveZeroOrMore, Literal('b'),]
    );
    syntax!(cls1, "[a]", vec![class('a', 'a')]);
    syntax!(cls2, "[!a]", vec![classn('a', 'a')]);
    syntax!(cls3, "[a-z]", vec![class('a', 'z')]);
    syntax!(cls4, "[!a-z]", vec![classn('a', 'z')]);
    syntax!(cls5, "[-]", vec![class('-', '-')]);
    syntax!(cls6, "[]]", vec![class(']', ']')]);
    syntax!(cls7, "[*]", vec![class('*', '*')]);
    syntax!(cls8, "[!!]", vec![classn('!', '!')]);
    syntax!(cls9, "[a-]", vec![rclass(&[('a', 'a'), ('-', '-')])]);
    syntax!(cls10, "[-a-z]", vec![rclass(&[('-', '-'), ('a', 'z')])]);
    syntax!(cls11, "[a-z-]", vec![rclass(&[('a', 'z'), ('-', '-')])]);
    syntax!(
        cls12,
        "[-a-z-]",
        vec![rclass(&[('-', '-'), ('a', 'z'), ('-', '-')]),]
    );
    syntax!(cls13, "[]-z]", vec![class(']', 'z')]);
    syntax!(cls14, "[--z]", vec![class('-', 'z')]);
    syntax!(cls15, "[ --]", vec![class(' ', '-')]);
    syntax!(cls16, "[0-9a-z]", vec![rclass(&[('0', '9'), ('a', 'z')])]);
    syntax!(cls17, "[a-z0-9]", vec![rclass(&[('a', 'z'), ('0', '9')])]);
    syntax!(cls18, "[!0-9a-z]", vec![rclassn(&[('0', '9'), ('a', 'z')])]);
    syntax!(cls19, "[!a-z0-9]", vec![rclassn(&[('a', 'z'), ('0', '9')])]);
    syntax!(cls20, "[^a]", vec![classn('a', 'a')]);
    syntax!(cls21, "[^a-z]", vec![classn('a', 'z')]);

    syntaxerr!(err_unclosed1, "[", ErrorKind::UnclosedClass);
    syntaxerr!(err_unclosed2, "[]", ErrorKind::UnclosedClass);
    syntaxerr!(err_unclosed3, "[!", ErrorKind::UnclosedClass);
    syntaxerr!(err_unclosed4, "[!]", ErrorKind::UnclosedClass);
    syntaxerr!(err_range1, "[z-a]", ErrorKind::InvalidRange('z', 'a'));
    syntaxerr!(err_range2, "[z--]", ErrorKind::InvalidRange('z', '-'));

    const CASEI: Options =
        Options { casei: Some(true), litsep: None, bsesc: None, ealtre: None };
    const SLASHLIT: Options =
        Options { casei: None, litsep: Some(true), bsesc: None, ealtre: None };
    const NOBSESC: Options = Options {
        casei: None,
        litsep: None,
        bsesc: Some(false),
        ealtre: None,
    };
    const BSESC: Options =
        Options { casei: None, litsep: None, bsesc: Some(true), ealtre: None };
    const EALTRE: Options = Options {
        casei: None,
        litsep: None,
        bsesc: Some(true),
        ealtre: Some(true),
    };

    toregex!(re_casei, "a", "(?i)^a$", &CASEI);

    toregex!(re_slash1, "?", r"^[^/]$", SLASHLIT);
    toregex!(re_slash2, "*", r"^[^/]*$", SLASHLIT);

    toregex!(re1, "a", "^a$");
    toregex!(re2, "?", "^.$");
    toregex!(re3, "*", "^.*$");
    toregex!(re4, "a?", "^a.$");
    toregex!(re5, "?a", "^.a$");
    toregex!(re6, "a*", "^a.*$");
    toregex!(re7, "*a", "^.*a$");
    toregex!(re8, "[*]", r"^[\*]$");
    toregex!(re9, "[+]", r"^[\+]$");
    toregex!(re10, "+", r"^\+$");
    toregex!(re11, "☃", r"^\xe2\x98\x83$");
    toregex!(re12, "**", r"^.*$");
    toregex!(re13, "**/", r"^.*$");
    toregex!(re14, "**/*", r"^(?:/?|.*/).*$");
    toregex!(re15, "**/**", r"^.*$");
    toregex!(re16, "**/**/*", r"^(?:/?|.*/).*$");
    toregex!(re17, "**/**/**", r"^.*$");
    toregex!(re18, "**/**/**/*", r"^(?:/?|.*/).*$");
    toregex!(re19, "a/**", r"^a/.*$");
    toregex!(re20, "a/**/**", r"^a/.*$");
    toregex!(re21, "a/**/**/**", r"^a/.*$");
    toregex!(re22, "a/**/b", r"^a(?:/|/.*/)b$");
    toregex!(re23, "a/**/**/b", r"^a(?:/|/.*/)b$");
    toregex!(re24, "a/**/**/**/b", r"^a(?:/|/.*/)b$");
    toregex!(re25, "**/b", r"^(?:/?|.*/)b$");
    toregex!(re26, "**/**/b", r"^(?:/?|.*/)b$");
    toregex!(re27, "**/**/**/b", r"^(?:/?|.*/)b$");
    toregex!(re28, "a**", r"^a.*.*$");
    toregex!(re29, "**a", r"^.*.*a$");
    toregex!(re30, "a**b", r"^a.*.*b$");
    toregex!(re31, "***", r"^.*.*.*$");
    toregex!(re32, "/a**", r"^/a.*.*$");
    toregex!(re33, "/**a", r"^/.*.*a$");
    toregex!(re34, "/a**b", r"^/a.*.*b$");
    toregex!(re35, "{a,b}", r"^(?:b|a)$");

    matches!(match1, "a", "a");
    matches!(match2, "a*b", "a_b");
    matches!(match3, "a*b*c", "abc");
    matches!(match4, "a*b*c", "a_b_c");
    matches!(match5, "a*b*c", "a___b___c");
    matches!(match6, "abc*abc*abc", "abcabcabcabcabcabcabc");
    matches!(match7, "a*a*a*a*a*a*a*a*a", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    matches!(match8, "a*b[xyz]c*d", "abxcdbxcddd");
    matches!(match9, "*.rs", ".rs");
    matches!(match10, "☃", "☃");

    matches!(matchrec1, "some/**/needle.txt", "some/needle.txt");
    matches!(matchrec2, "some/**/needle.txt", "some/one/needle.txt");
    matches!(matchrec3, "some/**/needle.txt", "some/one/two/needle.txt");
    matches!(matchrec4, "some/**/needle.txt", "some/other/needle.txt");
    matches!(matchrec5, "**", "abcde");
    matches!(matchrec6, "**", "");
    matches!(matchrec7, "**", ".asdf");
    matches!(matchrec8, "**", "/x/.asdf");
    matches!(matchrec9, "some/**/**/needle.txt", "some/needle.txt");
    matches!(matchrec10, "some/**/**/needle.txt", "some/one/needle.txt");
    matches!(matchrec11, "some/**/**/needle.txt", "some/one/two/needle.txt");
    matches!(matchrec12, "some/**/**/needle.txt", "some/other/needle.txt");
    matches!(matchrec13, "**/test", "one/two/test");
    matches!(matchrec14, "**/test", "one/test");
    matches!(matchrec15, "**/test", "test");
    matches!(matchrec16, "/**/test", "/one/two/test");
    matches!(matchrec17, "/**/test", "/one/test");
    matches!(matchrec18, "/**/test", "/test");
    matches!(matchrec19, "**/.*", ".abc");
    matches!(matchrec20, "**/.*", "abc/.abc");
    matches!(matchrec21, "**/foo/bar", "foo/bar");
    matches!(matchrec22, ".*/**", ".abc/abc");
    matches!(matchrec23, "test/**", "test/");
    matches!(matchrec24, "test/**", "test/one");
    matches!(matchrec25, "test/**", "test/one/two");
    matches!(matchrec26, "some/*/needle.txt", "some/one/needle.txt");

    matches!(matchrange1, "a[0-9]b", "a0b");
    matches!(matchrange2, "a[0-9]b", "a9b");
    matches!(matchrange3, "a[!0-9]b", "a_b");
    matches!(matchrange4, "[a-z123]", "1");
    matches!(matchrange5, "[1a-z23]", "1");
    matches!(matchrange6, "[123a-z]", "1");
    matches!(matchrange7, "[abc-]", "-");
    matches!(matchrange8, "[-abc]", "-");
    matches!(matchrange9, "[-a-c]", "b");
    matches!(matchrange10, "[a-c-]", "b");
    matches!(matchrange11, "[-]", "-");
    matches!(matchrange12, "a[^0-9]b", "a_b");

    matches!(matchpat1, "*hello.txt", "hello.txt");
    matches!(matchpat2, "*hello.txt", "gareth_says_hello.txt");
    matches!(matchpat3, "*hello.txt", "some/path/to/hello.txt");
    matches!(matchpat4, "*hello.txt", "some\\path\\to\\hello.txt");
    matches!(matchpat5, "*hello.txt", "/an/absolute/path/to/hello.txt");
    matches!(matchpat6, "*some/path/to/hello.txt", "some/path/to/hello.txt");
    matches!(
        matchpat7,
        "*some/path/to/hello.txt",
        "a/bigger/some/path/to/hello.txt"
    );

    matches!(matchescape, "_[[]_[]]_[?]_[*]_!_", "_[_]_?_*_!_");

    matches!(matchcasei1, "aBcDeFg", "aBcDeFg", CASEI);
    matches!(matchcasei2, "aBcDeFg", "abcdefg", CASEI);
    matches!(matchcasei3, "aBcDeFg", "ABCDEFG", CASEI);
    matches!(matchcasei4, "aBcDeFg", "AbCdEfG", CASEI);

    matches!(matchalt1, "a,b", "a,b");
    matches!(matchalt2, ",", ",");
    matches!(matchalt3, "{a,b}", "a");
    matches!(matchalt4, "{a,b}", "b");
    matches!(matchalt5, "{**/src/**,foo}", "abc/src/bar");
    matches!(matchalt6, "{**/src/**,foo}", "foo");
    matches!(matchalt7, "{[}],foo}", "}");
    matches!(matchalt8, "{foo}", "foo");
    matches!(matchalt9, "{}", "");
    matches!(matchalt10, "{,}", "");
    matches!(matchalt11, "{*.foo,*.bar,*.wat}", "test.foo");
    matches!(matchalt12, "{*.foo,*.bar,*.wat}", "test.bar");
    matches!(matchalt13, "{*.foo,*.bar,*.wat}", "test.wat");
    matches!(matchalt14, "foo{,.txt}", "foo.txt");
    nmatches!(matchalt15, "foo{,.txt}", "foo");
    matches!(matchalt16, "foo{,.txt}", "foo", EALTRE);

    matches!(matchslash1, "abc/def", "abc/def", SLASHLIT);
    #[cfg(unix)]
    nmatches!(matchslash2, "abc?def", "abc/def", SLASHLIT);
    #[cfg(not(unix))]
    nmatches!(matchslash2, "abc?def", "abc\\def", SLASHLIT);
    nmatches!(matchslash3, "abc*def", "abc/def", SLASHLIT);
    matches!(matchslash4, "abc[/]def", "abc/def", SLASHLIT); // differs
    #[cfg(unix)]
    nmatches!(matchslash5, "abc\\def", "abc/def", SLASHLIT);
    #[cfg(not(unix))]
    matches!(matchslash5, "abc\\def", "abc/def", SLASHLIT);

    matches!(matchbackslash1, "\\[", "[", BSESC);
    matches!(matchbackslash2, "\\?", "?", BSESC);
    matches!(matchbackslash3, "\\*", "*", BSESC);
    matches!(matchbackslash4, "\\[a-z]", "\\a", NOBSESC);
    matches!(matchbackslash5, "\\?", "\\a", NOBSESC);
    matches!(matchbackslash6, "\\*", "\\\\", NOBSESC);
    #[cfg(unix)]
    matches!(matchbackslash7, "\\a", "a");
    #[cfg(not(unix))]
    matches!(matchbackslash8, "\\a", "/a");

    nmatches!(matchnot1, "a*b*c", "abcd");
    nmatches!(matchnot2, "abc*abc*abc", "abcabcabcabcabcabcabca");
    nmatches!(matchnot3, "some/**/needle.txt", "some/other/notthis.txt");
    nmatches!(matchnot4, "some/**/**/needle.txt", "some/other/notthis.txt");
    nmatches!(matchnot5, "/**/test", "test");
    nmatches!(matchnot6, "/**/test", "/one/notthis");
    nmatches!(matchnot7, "/**/test", "/notthis");
    nmatches!(matchnot8, "**/.*", "ab.c");
    nmatches!(matchnot9, "**/.*", "abc/ab.c");
    nmatches!(matchnot10, ".*/**", "a.bc");
    nmatches!(matchnot11, ".*/**", "abc/a.bc");
    nmatches!(matchnot12, "a[0-9]b", "a_b");
    nmatches!(matchnot13, "a[!0-9]b", "a0b");
    nmatches!(matchnot14, "a[!0-9]b", "a9b");
    nmatches!(matchnot15, "[!-]", "-");
    nmatches!(matchnot16, "*hello.txt", "hello.txt-and-then-some");
    nmatches!(matchnot17, "*hello.txt", "goodbye.txt");
    nmatches!(
        matchnot18,
        "*some/path/to/hello.txt",
        "some/path/to/hello.txt-and-then-some"
    );
    nmatches!(
        matchnot19,
        "*some/path/to/hello.txt",
        "some/other/path/to/hello.txt"
    );
    nmatches!(matchnot20, "a", "foo/a");
    nmatches!(matchnot21, "./foo", "foo");
    nmatches!(matchnot22, "**/foo", "foofoo");
    nmatches!(matchnot23, "**/foo/bar", "foofoo/bar");
    nmatches!(matchnot24, "/*.c", "mozilla-sha1/sha1.c");
    nmatches!(matchnot25, "*.c", "mozilla-sha1/sha1.c", SLASHLIT);
    nmatches!(
        matchnot26,
        "**/m4/ltoptions.m4",
        "csharp/src/packages/repositories.config",
        SLASHLIT
    );
    nmatches!(matchnot27, "a[^0-9]b", "a0b");
    nmatches!(matchnot28, "a[^0-9]b", "a9b");
    nmatches!(matchnot29, "[^-]", "-");
    nmatches!(matchnot30, "some/*/needle.txt", "some/needle.txt");
    nmatches!(
        matchrec31,
        "some/*/needle.txt",
        "some/one/two/needle.txt",
        SLASHLIT
    );
    nmatches!(
        matchrec32,
        "some/*/needle.txt",
        "some/one/two/three/needle.txt",
        SLASHLIT
    );
    nmatches!(matchrec33, ".*/**", ".abc");
    nmatches!(matchrec34, "foo/**", "foo");

    macro_rules! extract {
        ($which:ident, $name:ident, $pat:expr, $expect:expr) => {
            extract!($which, $name, $pat, $expect, Options::default());
        };
        ($which:ident, $name:ident, $pat:expr, $expect:expr, $options:expr) => {
            #[test]
            fn $name() {
                let mut builder = GlobBuilder::new($pat);
                if let Some(casei) = $options.casei {
                    builder.case_insensitive(casei);
                }
                if let Some(litsep) = $options.litsep {
                    builder.literal_separator(litsep);
                }
                if let Some(bsesc) = $options.bsesc {
                    builder.backslash_escape(bsesc);
                }
                if let Some(ealtre) = $options.ealtre {
                    builder.empty_alternates(ealtre);
                }
                let pat = builder.build().unwrap();
                assert_eq!($expect, pat.$which());
            }
        };
    }

    macro_rules! literal {
        ($($tt:tt)*) => { extract!(literal, $($tt)*); }
    }

    macro_rules! basetokens {
        ($($tt:tt)*) => { extract!(basename_tokens, $($tt)*); }
    }

    macro_rules! ext {
        ($($tt:tt)*) => { extract!(ext, $($tt)*); }
    }

    macro_rules! required_ext {
        ($($tt:tt)*) => { extract!(required_ext, $($tt)*); }
    }

    macro_rules! prefix {
        ($($tt:tt)*) => { extract!(prefix, $($tt)*); }
    }

    macro_rules! suffix {
        ($($tt:tt)*) => { extract!(suffix, $($tt)*); }
    }

    macro_rules! baseliteral {
        ($($tt:tt)*) => { extract!(basename_literal, $($tt)*); }
    }

    literal!(extract_lit1, "foo", Some(s("foo")));
    literal!(extract_lit2, "foo", None, CASEI);
    literal!(extract_lit3, "/foo", Some(s("/foo")));
    literal!(extract_lit4, "/foo/", Some(s("/foo/")));
    literal!(extract_lit5, "/foo/bar", Some(s("/foo/bar")));
    literal!(extract_lit6, "*.foo", None);
    literal!(extract_lit7, "foo/bar", Some(s("foo/bar")));
    literal!(extract_lit8, "**/foo/bar", None);

    basetokens!(
        extract_basetoks1,
        "**/foo",
        Some(&*vec![Literal('f'), Literal('o'), Literal('o'),])
    );
    basetokens!(extract_basetoks2, "**/foo", None, CASEI);
    basetokens!(
        extract_basetoks3,
        "**/foo",
        Some(&*vec![Literal('f'), Literal('o'), Literal('o'),]),
        SLASHLIT
    );
    basetokens!(extract_basetoks4, "*foo", None, SLASHLIT);
    basetokens!(extract_basetoks5, "*foo", None);
    basetokens!(extract_basetoks6, "**/fo*o", None);
    basetokens!(
        extract_basetoks7,
        "**/fo*o",
        Some(&*vec![Literal('f'), Literal('o'), ZeroOrMore, Literal('o'),]),
        SLASHLIT
    );

    ext!(extract_ext1, "**/*.rs", Some(s(".rs")));
    ext!(extract_ext2, "**/*.rs.bak", None);
    ext!(extract_ext3, "*.rs", Some(s(".rs")));
    ext!(extract_ext4, "a*.rs", None);
    ext!(extract_ext5, "/*.c", None);
    ext!(extract_ext6, "*.c", None, SLASHLIT);
    ext!(extract_ext7, "*.c", Some(s(".c")));

    required_ext!(extract_req_ext1, "*.rs", Some(s(".rs")));
    required_ext!(extract_req_ext2, "/foo/bar/*.rs", Some(s(".rs")));
    required_ext!(extract_req_ext3, "/foo/bar/*.rs", Some(s(".rs")));
    required_ext!(extract_req_ext4, "/foo/bar/.rs", Some(s(".rs")));
    required_ext!(extract_req_ext5, ".rs", Some(s(".rs")));
    required_ext!(extract_req_ext6, "./rs", None);
    required_ext!(extract_req_ext7, "foo", None);
    required_ext!(extract_req_ext8, ".foo/", None);
    required_ext!(extract_req_ext9, "foo/", None);

    prefix!(extract_prefix1, "/foo", Some(s("/foo")));
    prefix!(extract_prefix2, "/foo/*", Some(s("/foo/")));
    prefix!(extract_prefix3, "**/foo", None);
    prefix!(extract_prefix4, "foo/**", Some(s("foo/")));

    suffix!(extract_suffix1, "**/foo/bar", Some((s("/foo/bar"), true)));
    suffix!(extract_suffix2, "*/foo/bar", Some((s("/foo/bar"), false)));
    suffix!(extract_suffix3, "*/foo/bar", None, SLASHLIT);
    suffix!(extract_suffix4, "foo/bar", Some((s("foo/bar"), false)));
    suffix!(extract_suffix5, "*.foo", Some((s(".foo"), false)));
    suffix!(extract_suffix6, "*.foo", None, SLASHLIT);
    suffix!(extract_suffix7, "**/*_test", Some((s("_test"), false)));

    baseliteral!(extract_baselit1, "**/foo", Some(s("foo")));
    baseliteral!(extract_baselit2, "foo", None);
    baseliteral!(extract_baselit3, "*foo", None);
    baseliteral!(extract_baselit4, "*/foo", None);
}
