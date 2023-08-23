/*!
globset crate 提供了跨平台的单个 glob 和 glob 集合匹配功能。

Glob 集合匹配是将一个或多个 glob 模式与单个候选路径同时匹配的过程，并返回所有匹配的 glob。例如，给定以下一组 glob：

```ignore
*.rs
src/lib.rs
src/**/foo.rs
```

以及路径 `src/bar/baz/foo.rs`，那么该集合将报告第一个和第三个 glob 匹配。

# 示例：匹配单个 glob

这个示例展示了如何将单个 glob 与单个文件路径匹配。

```
# fn example() -> Result<(), globset::Error> {
use globset::Glob;

let glob = Glob::new("*.rs")?.compile_matcher();

assert!(glob.is_match("foo.rs"));
assert!(glob.is_match("foo/bar.rs"));
assert!(!glob.is_match("Cargo.toml"));
# Ok(()) } example().unwrap();
```

# 示例：配置 glob 匹配器

这个示例展示了如何使用 `GlobBuilder` 配置匹配的语义方面。在这个示例中，我们阻止通配符匹配路径分隔符。

```
# fn example() -> Result<(), globset::Error> {
use globset::GlobBuilder;

let glob = GlobBuilder::new("*.rs")
    .literal_separator(true).build()?.compile_matcher();

assert!(glob.is_match("foo.rs"));
assert!(!glob.is_match("foo/bar.rs")); // 不再匹配
assert!(!glob.is_match("Cargo.toml"));
# Ok(()) } example().unwrap();
```

# 示例：同时匹配多个 glob

这个示例展示了如何同时匹配多个 glob 模式。

```
# fn example() -> Result<(), globset::Error> {
use globset::{Glob, GlobSetBuilder};

let mut builder = GlobSetBuilder::new();
// 可以使用 GlobBuilder 配置每个 glob 的匹配语义。
builder.add(Glob::new("*.rs")?);
builder.add(Glob::new("src/lib.rs")?);
builder.add(Glob::new("src/**/foo.rs")?);
let set = builder.build()?;

assert_eq!(set.matches("src/bar/baz/foo.rs"), vec![0, 2]);
# Ok(()) } example().unwrap();
```

# 语法

支持标准的 Unix 风格的 glob 语法：

* `?` 匹配任意单个字符。（如果启用了 `literal_separator` 选项，则 `?` 将永远不会匹配路径分隔符。）
* `*` 匹配零个或多个字符。（如果启用了 `literal_separator` 选项，则 `*` 将永远不会匹配路径分隔符。）
* `**` 递归匹配目录，但只在三种情况下合法。首先，如果 glob 以 <code>\*\*&#x2F;</code> 开头，则匹配所有目录。例如，<code>\*\*&#x2F;foo</code> 匹配 `foo` 和 `bar/foo`，但不匹配 `foo/bar`。其次，如果 glob 以 <code>&#x2F;\*\*</code> 结尾，则匹配所有子条目。例如，<code>foo&#x2F;\*\*</code> 匹配 `foo/a` 和 `foo/a/b`，但不匹配 `foo`。第三，如果 glob 在模式中的任何位置包含 <code>&#x2F;\*\*&#x2F;</code>，则匹配零个或多个目录。在其他任何地方使用 `**` 都是非法的（注意，glob `**` 是允许的，表示“匹配一切”）。
* `{a,b}` 匹配 `a` 或 `b`，其中 `a` 和 `b` 是任意的 glob 模式。（注意：目前不允许嵌套 `{...}`。）
* `[ab]` 匹配字符 `a` 或 `b`，其中 `a` 和 `b` 是字符。使用 `[!ab]` 匹配除 `a` 和 `b` 之外的任何字符。
* 元字符（如 `*` 和 `?`）可以通过字符类表示法进行转义。例如，`[*]` 匹配 `*`。
* 在启用反斜杠转义时，反斜杠（`\`）将转义 glob 中的所有元字符。如果在非元字符之前出现，斜杠将被忽略。`\\` 将匹配文字 `\\`。请注意，默认情况下只在 Unix 平台上启用此模式，但可以通过 `Glob` 的 `backslash_escape` 设置在任何平台上启用。

`GlobBuilder` 可以用于防止通配符匹配路径分隔符，或者启用大小写不敏感匹配。
*/

#![deny(missing_docs)]

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::error::Error as StdError;
use std::fmt;
use std::hash;
use std::path::Path;
use std::str;

use aho_corasick::AhoCorasick;
use bstr::{ByteSlice, ByteVec, B};
use regex::bytes::{Regex, RegexBuilder, RegexSet};

use crate::glob::MatchStrategy;
pub use crate::glob::{Glob, GlobBuilder, GlobMatcher};
use crate::pathutil::{file_name, file_name_ext, normalize_path};

mod glob;
mod pathutil;

#[cfg(feature = "serde1")]
mod serde_impl;

#[cfg(feature = "log")]
macro_rules! debug {
    ($($token:tt)*) => (::log::debug!($($token)*);)
}

#[cfg(not(feature = "log"))]
macro_rules! debug {
    ($($token:tt)*) => {};
}
/// 表示解析 glob 模式时可能发生的错误。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Error {
    /// 调用者提供的原始 glob。
    glob: Option<String>,
    /// 错误的类型。
    kind: ErrorKind,
}

/// 解析 glob 模式时可能发生的错误类型。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    /// **已弃用**。
    ///
    /// 这个错误以前用于与 git 的 glob 规范保持一致，但是该规范现在接受所有使用 `**` 的情况。
    /// 当 `**` 不与路径分隔符相邻，也不在 glob 的开头/结尾时，它现在被视为两个连续的 `*` 模式。
    /// 因此，不再使用此错误。
    InvalidRecursive,
    /// 当字符类（例如，`[abc]`）没有闭合时发生。
    UnclosedClass,
    /// 当字符类中的范围（例如，`[a-z]`）无效时发生。例如，如果范围的开始字符比结束字符大。
    InvalidRange(char, char),
    /// 当找到一个没有匹配的 `}`。
    UnopenedAlternates,
    /// 当找到一个没有匹配的 `{`。
    UnclosedAlternates,
    /// 当一个交替组嵌套在另一个交替组内部时发生，例如，`{{a,b},{c,d}}`。
    NestedAlternates,
    /// 当在 glob 的末尾找到未转义的 '\' 时发生。
    DanglingEscape,
    /// 与解析或编译正则表达式相关的错误。
    Regex(String),
    /// 提示不应该穷尽地进行解构。
    ///
    /// 此枚举可能会增加其他变体，因此这确保客户端不依赖于穷尽匹配。
    /// （否则，添加新的变体可能会破坏现有代码。）
    #[doc(hidden)]
    __Nonexhaustive,
}

impl StdError for Error {
    fn description(&self) -> &str {
        self.kind.description()
    }
}

impl Error {
    /// 返回导致此错误的 glob，如果存在的话。
    pub fn glob(&self) -> Option<&str> {
        self.glob.as_ref().map(|s| &**s)
    }

    /// 返回此错误的类型。
    pub fn kind(&self) -> &ErrorKind {
        &self.kind
    }
}

impl ErrorKind {
    fn description(&self) -> &str {
        match *self {
            ErrorKind::InvalidRecursive => {
                "无效的 ** 使用；必须是一个路径组件"
            }
            ErrorKind::UnclosedClass => "未闭合的字符类；缺少 ']'",
            ErrorKind::InvalidRange(_, _) => "无效的字符范围",
            ErrorKind::UnopenedAlternates => {
                "未打开的交替组；缺少 '{' \
                （可以用 '[}]' 转义 '}' 吗？）"
            }
            ErrorKind::UnclosedAlternates => {
                "未闭合的交替组；缺少 '}' \
                （可以用 '[{]' 转义 '{' 吗？）"
            }
            ErrorKind::NestedAlternates => "不允许嵌套的交替组",
            ErrorKind::DanglingEscape => "悬空的 '\\'",
            ErrorKind::Regex(ref err) => err,
            ErrorKind::__Nonexhaustive => unreachable!(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.glob {
            None => self.kind.fmt(f),
            Some(ref glob) => {
                write!(f, "解析 glob '{}' 时发生错误：{}", glob, self.kind)
            }
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ErrorKind::InvalidRecursive
            | ErrorKind::UnclosedClass
            | ErrorKind::UnopenedAlternates
            | ErrorKind::UnclosedAlternates
            | ErrorKind::NestedAlternates
            | ErrorKind::DanglingEscape
            | ErrorKind::Regex(_) => write!(f, "{}", self.description()),
            ErrorKind::InvalidRange(s, e) => {
                write!(f, "无效的范围；'{}' > '{}'", s, e)
            }
            ErrorKind::__Nonexhaustive => unreachable!(),
        }
    }
}

fn new_regex(pat: &str) -> Result<Regex, Error> {
    RegexBuilder::new(pat)
        .dot_matches_new_line(true)
        .size_limit(10 * (1 << 20))
        .dfa_size_limit(10 * (1 << 20))
        .build()
        .map_err(|err| Error {
            glob: Some(pat.to_string()),
            kind: ErrorKind::Regex(err.to_string()),
        })
}

fn new_regex_set<I, S>(pats: I) -> Result<RegexSet, Error>
where
    S: AsRef<str>,
    I: IntoIterator<Item = S>,
{
    RegexSet::new(pats).map_err(|err| Error {
        glob: None,
        kind: ErrorKind::Regex(err.to_string()),
    })
}

type Fnv = hash::BuildHasherDefault<fnv::FnvHasher>;

/// GlobSet 表示一组可以在单次匹配中一起匹配的 glob。
#[derive(Clone, Debug)]
pub struct GlobSet {
    len: usize,
    strats: Vec<GlobSetMatchStrategy>,
}
impl GlobSet {
    /// 创建一个空的 `GlobSet`。一个空集合不会匹配任何内容。
    #[inline]
    pub fn empty() -> GlobSet {
        GlobSet { len: 0, strats: vec![] }
    }

    /// 如果这个集合为空，即不匹配任何内容，则返回 true。
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 返回这个集合中的 glob 数量。
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// 如果这个集合中的任何一个 glob 匹配给定的路径，则返回 true。
    pub fn is_match<P: AsRef<Path>>(&self, path: P) -> bool {
        self.is_match_candidate(&Candidate::new(path.as_ref()))
    }

    /// 如果这个集合中的任何一个 glob 匹配给定的路径，则返回 true。
    ///
    /// 这个方法接受 Candidate 作为输入，可以用于摊销准备路径匹配的成本。
    pub fn is_match_candidate(&self, path: &Candidate<'_>) -> bool {
        if self.is_empty() {
            return false;
        }
        for strat in &self.strats {
            if strat.is_match(path) {
                return true;
            }
        }
        false
    }

    /// 返回与给定路径匹配的每个 glob 模式的序列号。
    pub fn matches<P: AsRef<Path>>(&self, path: P) -> Vec<usize> {
        self.matches_candidate(&Candidate::new(path.as_ref()))
    }

    /// 返回与给定路径匹配的每个 glob 模式的序列号。
    ///
    /// 这个方法接受 Candidate 作为输入，可以用于摊销准备路径匹配的成本。
    pub fn matches_candidate(&self, path: &Candidate<'_>) -> Vec<usize> {
        let mut into = vec![];
        if self.is_empty() {
            return into;
        }
        self.matches_candidate_into(path, &mut into);
        into
    }

    /// 将与给定路径匹配的每个 glob 模式的序列号添加到提供的 Vec 中。
    ///
    /// 在匹配开始之前，`into` 将被清空，并在匹配结束后包含一组序列号（按升序排列）。
    /// 如果没有匹配的 glob，则 `into` 将为空。
    pub fn matches_into<P: AsRef<Path>>(
        &self,
        path: P,
        into: &mut Vec<usize>,
    ) {
        self.matches_candidate_into(&Candidate::new(path.as_ref()), into);
    }

    /// 将与给定路径匹配的每个 glob 模式的序列号添加到提供的 Vec 中。
    ///
    /// 在匹配开始之前，`into` 将被清空，并在匹配结束后包含一组序列号（按升序排列）。
    /// 如果没有匹配的 glob，则 `into` 将为空。
    ///
    /// 这个方法接受 Candidate 作为输入，可以用于摊销准备路径匹配的成本。
    pub fn matches_candidate_into(
        &self,
        path: &Candidate<'_>,
        into: &mut Vec<usize>,
    ) {
        into.clear();
        if self.is_empty() {
            return;
        }
        for strat in &self.strats {
            strat.matches_into(path, into);
        }
        into.sort();
        into.dedup();
    }

    fn new(pats: &[Glob]) -> Result<GlobSet, Error> {
        if pats.is_empty() {
            return Ok(GlobSet { len: 0, strats: vec![] });
        }
        let mut lits = LiteralStrategy::new();
        let mut base_lits = BasenameLiteralStrategy::new();
        let mut exts = ExtensionStrategy::new();
        let mut prefixes = MultiStrategyBuilder::new();
        let mut suffixes = MultiStrategyBuilder::new();
        let mut required_exts = RequiredExtensionStrategyBuilder::new();
        let mut regexes = MultiStrategyBuilder::new();
        for (i, p) in pats.iter().enumerate() {
            match MatchStrategy::new(p) {
                MatchStrategy::Literal(lit) => {
                    lits.add(i, lit);
                }
                MatchStrategy::BasenameLiteral(lit) => {
                    base_lits.add(i, lit);
                }
                MatchStrategy::Extension(ext) => {
                    exts.add(i, ext);
                }
                MatchStrategy::Prefix(prefix) => {
                    prefixes.add(i, prefix);
                }
                MatchStrategy::Suffix { suffix, component } => {
                    if component {
                        lits.add(i, suffix[1..].to_string());
                    }
                    suffixes.add(i, suffix);
                }
                MatchStrategy::RequiredExtension(ext) => {
                    required_exts.add(i, ext, p.regex().to_owned());
                }
                MatchStrategy::Regex => {
                    debug!("glob converted to regex: {:?}", p);
                    regexes.add(i, p.regex().to_owned());
                }
            }
        }
        debug!(
            "built glob set; {} literals, {} basenames, {} extensions, \
                {} prefixes, {} suffixes, {} required extensions, {} regexes",
            lits.0.len(),
            base_lits.0.len(),
            exts.0.len(),
            prefixes.literals.len(),
            suffixes.literals.len(),
            required_exts.0.len(),
            regexes.literals.len()
        );
        Ok(GlobSet {
            len: pats.len(),
            strats: vec![
                GlobSetMatchStrategy::Extension(exts),
                GlobSetMatchStrategy::BasenameLiteral(base_lits),
                GlobSetMatchStrategy::Literal(lits),
                GlobSetMatchStrategy::Suffix(suffixes.suffix()),
                GlobSetMatchStrategy::Prefix(prefixes.prefix()),
                GlobSetMatchStrategy::RequiredExtension(
                    required_exts.build()?,
                ),
                GlobSetMatchStrategy::Regex(regexes.regex_set()?),
            ],
        })
    }
}

impl Default for GlobSet {
    /// 创建一个默认的空 GlobSet。
    fn default() -> Self {
        GlobSet::empty()
    }
}

/// GlobSetBuilder 用于构建一组可以同时匹配文件路径的模式。
#[derive(Clone, Debug)]
pub struct GlobSetBuilder {
    pats: Vec<Glob>,
}

impl GlobSetBuilder {
    /// 创建一个新的 GlobSetBuilder。GlobSetBuilder 可用于添加新的模式。
    /// 一旦所有模式都被添加，应该调用 `build` 来生成一个 `GlobSet`，
    /// 然后可以用于匹配。
    pub fn new() -> GlobSetBuilder {
        GlobSetBuilder { pats: vec![] }
    }

    /// 从到目前为止添加的所有 glob 模式构建一个新的匹配器。
    ///
    /// 一旦构建了匹配器，就不能再向其中添加新的模式。
    pub fn build(&self) -> Result<GlobSet, Error> {
        GlobSet::new(&self.pats)
    }

    /// 将一个新的模式添加到这个集合中。
    pub fn add(&mut self, pat: Glob) -> &mut GlobSetBuilder {
        self.pats.push(pat);
        self
    }
}
/// 用于匹配的候选路径。
///
/// 该库中的所有 glob 匹配都基于 `Candidate` 值进行。
/// 构建候选项的成本非常小，因此在将单个路径与多个 glob 或 glob 集合进行匹配时，
/// 调用者可能会发现将成本分摊会更有益。
#[derive(Clone)]
pub struct Candidate<'a> {
    path: Cow<'a, [u8]>,
    basename: Cow<'a, [u8]>,
    ext: Cow<'a, [u8]>,
}

impl<'a> std::fmt::Debug for Candidate<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Candidate")
            .field("path", &self.path.as_bstr())
            .field("basename", &self.basename.as_bstr())
            .field("ext", &self.ext.as_bstr())
            .finish()
    }
}

impl<'a> Candidate<'a> {
    /// 从给定路径创建一个新的用于匹配的候选项。
    pub fn new<P: AsRef<Path> + ?Sized>(path: &'a P) -> Candidate<'a> {
        let path = normalize_path(Vec::from_path_lossy(path.as_ref()));
        let basename = file_name(&path).unwrap_or(Cow::Borrowed(B("")));
        let ext = file_name_ext(&basename).unwrap_or(Cow::Borrowed(B("")));
        Candidate { path: path, basename: basename, ext: ext }
    }

    fn path_prefix(&self, max: usize) -> &[u8] {
        if self.path.len() <= max {
            &*self.path
        } else {
            &self.path[..max]
        }
    }

    fn path_suffix(&self, max: usize) -> &[u8] {
        if self.path.len() <= max {
            &*self.path
        } else {
            &self.path[self.path.len() - max..]
        }
    }
}

#[derive(Clone, Debug)]
enum GlobSetMatchStrategy {
    Literal(LiteralStrategy),
    BasenameLiteral(BasenameLiteralStrategy),
    Extension(ExtensionStrategy),
    Prefix(PrefixStrategy),
    Suffix(SuffixStrategy),
    RequiredExtension(RequiredExtensionStrategy),
    Regex(RegexSetStrategy),
}

impl GlobSetMatchStrategy {
    fn is_match(&self, candidate: &Candidate<'_>) -> bool {
        use self::GlobSetMatchStrategy::*;
        match *self {
            Literal(ref s) => s.is_match(candidate),
            BasenameLiteral(ref s) => s.is_match(candidate),
            Extension(ref s) => s.is_match(candidate),
            Prefix(ref s) => s.is_match(candidate),
            Suffix(ref s) => s.is_match(candidate),
            RequiredExtension(ref s) => s.is_match(candidate),
            Regex(ref s) => s.is_match(candidate),
        }
    }

    fn matches_into(
        &self,
        candidate: &Candidate<'_>,
        matches: &mut Vec<usize>,
    ) {
        use self::GlobSetMatchStrategy::*;
        match *self {
            Literal(ref s) => s.matches_into(candidate, matches),
            BasenameLiteral(ref s) => s.matches_into(candidate, matches),
            Extension(ref s) => s.matches_into(candidate, matches),
            Prefix(ref s) => s.matches_into(candidate, matches),
            Suffix(ref s) => s.matches_into(candidate, matches),
            RequiredExtension(ref s) => s.matches_into(candidate, matches),
            Regex(ref s) => s.matches_into(candidate, matches),
        }
    }
}

#[derive(Clone, Debug)]
struct LiteralStrategy(BTreeMap<Vec<u8>, Vec<usize>>);

impl LiteralStrategy {
    fn new() -> LiteralStrategy {
        LiteralStrategy(BTreeMap::new())
    }

    fn add(&mut self, global_index: usize, lit: String) {
        self.0.entry(lit.into_bytes()).or_insert(vec![]).push(global_index);
    }

    fn is_match(&self, candidate: &Candidate<'_>) -> bool {
        self.0.contains_key(candidate.path.as_bytes())
    }

    #[inline(never)]
    fn matches_into(
        &self,
        candidate: &Candidate<'_>,
        matches: &mut Vec<usize>,
    ) {
        if let Some(hits) = self.0.get(candidate.path.as_bytes()) {
            matches.extend(hits);
        }
    }
}
#[derive(Clone, Debug)]
struct BasenameLiteralStrategy(BTreeMap<Vec<u8>, Vec<usize>>);

impl BasenameLiteralStrategy {
    fn new() -> BasenameLiteralStrategy {
        BasenameLiteralStrategy(BTreeMap::new())
    }

    fn add(&mut self, global_index: usize, lit: String) {
        self.0.entry(lit.into_bytes()).or_insert(vec![]).push(global_index);
    }

    fn is_match(&self, candidate: &Candidate<'_>) -> bool {
        if candidate.basename.is_empty() {
            return false;
        }
        self.0.contains_key(candidate.basename.as_bytes())
    }

    #[inline(never)]
    fn matches_into(
        &self,
        candidate: &Candidate<'_>,
        matches: &mut Vec<usize>,
    ) {
        if candidate.basename.is_empty() {
            return;
        }
        if let Some(hits) = self.0.get(candidate.basename.as_bytes()) {
            matches.extend(hits);
        }
    }
}

#[derive(Clone, Debug)]
struct ExtensionStrategy(HashMap<Vec<u8>, Vec<usize>, Fnv>);

impl ExtensionStrategy {
    fn new() -> ExtensionStrategy {
        ExtensionStrategy(HashMap::with_hasher(Fnv::default()))
    }

    fn add(&mut self, global_index: usize, ext: String) {
        self.0.entry(ext.into_bytes()).or_insert(vec![]).push(global_index);
    }

    fn is_match(&self, candidate: &Candidate<'_>) -> bool {
        if candidate.ext.is_empty() {
            return false;
        }
        self.0.contains_key(candidate.ext.as_bytes())
    }

    #[inline(never)]
    fn matches_into(
        &self,
        candidate: &Candidate<'_>,
        matches: &mut Vec<usize>,
    ) {
        if candidate.ext.is_empty() {
            return;
        }
        if let Some(hits) = self.0.get(candidate.ext.as_bytes()) {
            matches.extend(hits);
        }
    }
}

#[derive(Clone, Debug)]
struct PrefixStrategy {
    matcher: AhoCorasick,
    map: Vec<usize>,
    longest: usize,
}

impl PrefixStrategy {
    fn is_match(&self, candidate: &Candidate<'_>) -> bool {
        let path = candidate.path_prefix(self.longest);
        for m in self.matcher.find_overlapping_iter(path) {
            if m.start() == 0 {
                return true;
            }
        }
        false
    }

    fn matches_into(
        &self,
        candidate: &Candidate<'_>,
        matches: &mut Vec<usize>,
    ) {
        let path = candidate.path_prefix(self.longest);
        for m in self.matcher.find_overlapping_iter(path) {
            if m.start() == 0 {
                matches.push(self.map[m.pattern()]);
            }
        }
    }
}

#[derive(Clone, Debug)]
struct SuffixStrategy {
    matcher: AhoCorasick,
    map: Vec<usize>,
    longest: usize,
}

impl SuffixStrategy {
    fn is_match(&self, candidate: &Candidate<'_>) -> bool {
        let path = candidate.path_suffix(self.longest);
        for m in self.matcher.find_overlapping_iter(path) {
            if m.end() == path.len() {
                return true;
            }
        }
        false
    }

    fn matches_into(
        &self,
        candidate: &Candidate<'_>,
        matches: &mut Vec<usize>,
    ) {
        let path = candidate.path_suffix(self.longest);
        for m in self.matcher.find_overlapping_iter(path) {
            if m.end() == path.len() {
                matches.push(self.map[m.pattern()]);
            }
        }
    }
}

#[derive(Clone, Debug)]
struct RequiredExtensionStrategy(HashMap<Vec<u8>, Vec<(usize, Regex)>, Fnv>);

impl RequiredExtensionStrategy {
    fn is_match(&self, candidate: &Candidate<'_>) -> bool {
        if candidate.ext.is_empty() {
            return false;
        }
        match self.0.get(candidate.ext.as_bytes()) {
            None => false,
            Some(regexes) => {
                for &(_, ref re) in regexes {
                    if re.is_match(candidate.path.as_bytes()) {
                        return true;
                    }
                }
                false
            }
        }
    }

    #[inline(never)]
    fn matches_into(
        &self,
        candidate: &Candidate<'_>,
        matches: &mut Vec<usize>,
    ) {
        if candidate.ext.is_empty() {
            return;
        }
        if let Some(regexes) = self.0.get(candidate.ext.as_bytes()) {
            for &(global_index, ref re) in regexes {
                if re.is_match(candidate.path.as_bytes()) {
                    matches.push(global_index);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
struct RegexSetStrategy {
    matcher: RegexSet,
    map: Vec<usize>,
}

impl RegexSetStrategy {
    fn is_match(&self, candidate: &Candidate<'_>) -> bool {
        self.matcher.is_match(candidate.path.as_bytes())
    }

    fn matches_into(
        &self,
        candidate: &Candidate<'_>,
        matches: &mut Vec<usize>,
    ) {
        for i in self.matcher.matches(candidate.path.as_bytes()) {
            matches.push(self.map[i]);
        }
    }
}

#[derive(Clone, Debug)]
struct MultiStrategyBuilder {
    literals: Vec<String>,
    map: Vec<usize>,
    longest: usize,
}

impl MultiStrategyBuilder {
    fn new() -> MultiStrategyBuilder {
        MultiStrategyBuilder { literals: vec![], map: vec![], longest: 0 }
    }

    fn add(&mut self, global_index: usize, literal: String) {
        if literal.len() > self.longest {
            self.longest = literal.len();
        }
        self.map.push(global_index);
        self.literals.push(literal);
    }

    fn prefix(self) -> PrefixStrategy {
        PrefixStrategy {
            matcher: AhoCorasick::new(&self.literals).unwrap(),
            map: self.map,
            longest: self.longest,
        }
    }

    fn suffix(self) -> SuffixStrategy {
        SuffixStrategy {
            matcher: AhoCorasick::new(&self.literals).unwrap(),
            map: self.map,
            longest: self.longest,
        }
    }

    fn regex_set(self) -> Result<RegexSetStrategy, Error> {
        Ok(RegexSetStrategy {
            matcher: new_regex_set(self.literals)?,
            map: self.map,
        })
    }
}

#[derive(Clone, Debug)]
struct RequiredExtensionStrategyBuilder(
    HashMap<Vec<u8>, Vec<(usize, String)>>,
);

impl RequiredExtensionStrategyBuilder {
    fn new() -> RequiredExtensionStrategyBuilder {
        RequiredExtensionStrategyBuilder(HashMap::new())
    }

    fn add(&mut self, global_index: usize, ext: String, regex: String) {
        self.0
            .entry(ext.into_bytes())
            .or_insert(vec![])
            .push((global_index, regex));
    }

    fn build(self) -> Result<RequiredExtensionStrategy, Error> {
        let mut exts = HashMap::with_hasher(Fnv::default());
        for (ext, regexes) in self.0.into_iter() {
            exts.insert(ext.clone(), vec![]);
            for (global_index, regex) in regexes {
                let compiled = new_regex(&regex)?;
                exts.get_mut(&ext).unwrap().push((global_index, compiled));
            }
        }
        Ok(RequiredExtensionStrategy(exts))
    }
}

/// 转义给定的 glob 模式中的特殊字符。
///
/// 转义通过在特殊字符周围加上方括号来实现。例如，`*` 变成了 `[*]`。
pub fn escape(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            // 注意，! 不需要转义，因为它只在方括号内部是特殊字符
            '?' | '*' | '[' | ']' => {
                escaped.push('[');
                escaped.push(c);
                escaped.push(']');
            }
            c => {
                escaped.push(c);
            }
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::{GlobSet, GlobSetBuilder};
    use crate::glob::Glob;

    #[test]
    fn set_works() {
        let mut builder = GlobSetBuilder::new();
        builder.add(Glob::new("src/**/*.rs").unwrap());
        builder.add(Glob::new("*.c").unwrap());
        builder.add(Glob::new("src/lib.rs").unwrap());
        let set = builder.build().unwrap();

        assert!(set.is_match("foo.c"));
        assert!(set.is_match("src/foo.c"));
        assert!(!set.is_match("foo.rs"));
        assert!(!set.is_match("tests/foo.rs"));
        assert!(set.is_match("src/foo.rs"));
        assert!(set.is_match("src/grep/src/main.rs"));

        let matches = set.matches("src/lib.rs");
        assert_eq!(2, matches.len());
        assert_eq!(0, matches[0]);
        assert_eq!(2, matches[1]);
    }

    #[test]
    fn empty_set_works() {
        let set = GlobSetBuilder::new().build().unwrap();
        assert!(!set.is_match(""));
        assert!(!set.is_match("a"));
    }

    #[test]
    fn default_set_is_empty_works() {
        let set: GlobSet = Default::default();
        assert!(!set.is_match(""));
        assert!(!set.is_match("a"));
    }

    #[test]
    fn escape() {
        use super::escape;
        assert_eq!("foo", escape("foo"));
        assert_eq!("foo[*]", escape("foo*"));
        assert_eq!("[[][]]", escape("[]"));
        assert_eq!("[*][?]", escape("*?"));
        assert_eq!("src/[*][*]/[*].rs", escape("src/**/*.rs"));
        assert_eq!("bar[[]ab[]]baz", escape("bar[ab]baz"));
        assert_eq!("bar[[]!![]]!baz", escape("bar[!!]!baz"));
    }
}
