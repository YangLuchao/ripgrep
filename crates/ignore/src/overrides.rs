/*!
覆盖模块提供了一种指定一组覆盖通配符的方法。这提供了类似于命令行工具中的 `--include` 或 `--exclude` 的功能。
*/

use std::path::Path;

use crate::gitignore::{self, Gitignore, GitignoreBuilder};
use crate::{Error, Match};

/// Glob 表示覆盖匹配器中的单个通配符。
///
/// 这用于报告最高优先级匹配的有关信息。
///
/// 请注意，并非所有匹配必然对应于特定的通配符。例如，如果有一个或多个白名单通配符，并且文件路径不匹配集合中的任何通配符，则文件路径被视为已被忽略。
///
/// 生命周期 `'a` 引用了生成此通配符的匹配器的生命周期。
#[derive(Clone, Debug)]
pub struct Glob<'a>(GlobInner<'a>);

#[derive(Clone, Debug)]
enum GlobInner<'a> {
    /// 没有匹配的通配符，但文件路径仍应被忽略。
    UnmatchedIgnore,
    /// 有匹配的通配符。
    Matched(&'a gitignore::Glob),
}

impl<'a> Glob<'a> {
    fn unmatched() -> Glob<'a> {
        Glob(GlobInner::UnmatchedIgnore)
    }
}

/// 管理由最终用户显式提供的一组覆盖通配符。
#[derive(Clone, Debug)]
pub struct Override(Gitignore);

impl Override {
    /// 返回一个永远不会匹配任何文件路径的空匹配器。
    pub fn empty() -> Override {
        Override(Gitignore::empty())
    }

    /// 返回此覆盖集的目录。
    ///
    /// 所有匹配都是相对于此路径进行的。
    pub fn path(&self) -> &Path {
        self.0.path()
    }

    /// 仅当此匹配器为空时返回 true。
    ///
    /// 当匹配器为空时，它永远不会匹配任何文件路径。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// 返回忽略通配符的总数。
    pub fn num_ignores(&self) -> u64 {
        self.0.num_whitelists()
    }

    /// 返回白名单通配符的总数。
    pub fn num_whitelists(&self) -> u64 {
        self.0.num_ignores()
    }

    /// 返回给定文件路径是否在此覆盖匹配器的模式中匹配。
    ///
    /// 如果 `is_dir` 为 true，则表示路径引用目录；否则为文件。
    ///
    /// 如果没有覆盖通配符，则始终返回 `Match::None`。
    ///
    /// 如果至少有一个白名单覆盖并且 `is_dir` 为 false，则永远不会返回 `Match::None`，因为非匹配被解释为已被忽略。
    ///
    /// 给定的路径与相对于构建覆盖匹配器时给定的路径进行匹配。具体来说，在匹配 `path` 之前，将去除其前缀（由给定目录的公共后缀确定）。如果没有公共后缀/前缀重叠，则假定 `path` 位于覆盖集的根路径相同的目录中。
    pub fn matched<'a, P: AsRef<Path>>(
        &'a self,
        path: P,
        is_dir: bool,
    ) -> Match<Glob<'a>> {
        if self.is_empty() {
            return Match::None;
        }
        let mat = self.0.matched(path, is_dir).invert();
        if mat.is_none() && self.num_whitelists() > 0 && !is_dir {
            return Match::Ignore(Glob::unmatched());
        }
        mat.map(move |giglob| Glob(GlobInner::Matched(giglob)))
    }
}

/// 构建一组覆盖通配符的匹配器。
#[derive(Clone, Debug)]
pub struct OverrideBuilder {
    builder: GitignoreBuilder,
}

impl OverrideBuilder {
    /// 创建一个新的覆盖匹配器构建器。
    ///
    /// 匹配相对于提供的目录路径进行。
    pub fn new<P: AsRef<Path>>(path: P) -> OverrideBuilder {
        OverrideBuilder { builder: GitignoreBuilder::new(path) }
    }

    /// 从迄今为止添加的通配符构建一个新的覆盖匹配器。
    ///
    /// 一旦构建了匹配器，就不能再向其中添加新的通配符。
    pub fn build(&self) -> Result<Override, Error> {
        Ok(Override(self.builder.build()?))
    }

    /// 向覆盖集中添加一个通配符。
    ///
    /// 此处添加的通配符与 `gitignore` 文件中的单行完全相同，`!` 的含义被倒置：也就是说，以 `!` 开头的通配符将会被忽略。没有 `!`，则通配符的所有匹配都被视为白名单匹配。
    pub fn add(&mut self, glob: &str) -> Result<&mut OverrideBuilder, Error> {
        self.builder.add_line(None, glob)?;
        Ok(self)
    }

    /// 切换通配符是否应该进行不区分大小写的匹配。
    ///
    /// 更改此选项后，只有在更改后添加的通配符才会受到影响。
    ///
    /// 默认情况下，此选项被禁用。
    pub fn case_insensitive(
        &mut self,
        yes: bool,
    ) -> Result<&mut OverrideBuilder, Error> {
        // TODO: This should not return a `Result`. Fix this in the next semver
        // release.
        self.builder.case_insensitive(yes)?;
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::{Override, OverrideBuilder};

    const ROOT: &'static str = "/home/andrew/foo";

    fn ov(globs: &[&str]) -> Override {
        let mut builder = OverrideBuilder::new(ROOT);
        for glob in globs {
            builder.add(glob).unwrap();
        }
        builder.build().unwrap()
    }

    #[test]
    fn empty() {
        let ov = ov(&[]);
        assert!(ov.matched("a.foo", false).is_none());
        assert!(ov.matched("a", false).is_none());
        assert!(ov.matched("", false).is_none());
    }

    #[test]
    fn simple() {
        let ov = ov(&["*.foo", "!*.bar"]);
        assert!(ov.matched("a.foo", false).is_whitelist());
        assert!(ov.matched("a.foo", true).is_whitelist());
        assert!(ov.matched("a.rs", false).is_ignore());
        assert!(ov.matched("a.rs", true).is_none());
        assert!(ov.matched("a.bar", false).is_ignore());
        assert!(ov.matched("a.bar", true).is_ignore());
    }

    #[test]
    fn only_ignores() {
        let ov = ov(&["!*.bar"]);
        assert!(ov.matched("a.rs", false).is_none());
        assert!(ov.matched("a.rs", true).is_none());
        assert!(ov.matched("a.bar", false).is_ignore());
        assert!(ov.matched("a.bar", true).is_ignore());
    }

    #[test]
    fn precedence() {
        let ov = ov(&["*.foo", "!*.bar.foo"]);
        assert!(ov.matched("a.foo", false).is_whitelist());
        assert!(ov.matched("a.baz", false).is_ignore());
        assert!(ov.matched("a.bar.foo", false).is_ignore());
    }

    #[test]
    fn gitignore() {
        let ov = ov(&["/foo", "bar/*.rs", "baz/**"]);
        assert!(ov.matched("bar/lib.rs", false).is_whitelist());
        assert!(ov.matched("bar/wat/lib.rs", false).is_ignore());
        assert!(ov.matched("wat/bar/lib.rs", false).is_ignore());
        assert!(ov.matched("foo", false).is_whitelist());
        assert!(ov.matched("wat/foo", false).is_ignore());
        assert!(ov.matched("baz", false).is_ignore());
        assert!(ov.matched("baz/a", false).is_whitelist());
        assert!(ov.matched("baz/a/b", false).is_whitelist());
    }

    #[test]
    fn allow_directories() {
        // This tests that directories are NOT ignored when they are unmatched.
        let ov = ov(&["*.rs"]);
        assert!(ov.matched("foo.rs", false).is_whitelist());
        assert!(ov.matched("foo.c", false).is_ignore());
        assert!(ov.matched("foo", false).is_ignore());
        assert!(ov.matched("foo", true).is_none());
        assert!(ov.matched("src/foo.rs", false).is_whitelist());
        assert!(ov.matched("src/foo.c", false).is_ignore());
        assert!(ov.matched("src/foo", false).is_ignore());
        assert!(ov.matched("src/foo", true).is_none());
    }

    #[test]
    fn absolute_path() {
        let ov = ov(&["!/bar"]);
        assert!(ov.matched("./foo/bar", false).is_none());
    }

    #[test]
    fn case_insensitive() {
        let ov = OverrideBuilder::new(ROOT)
            .case_insensitive(true)
            .unwrap()
            .add("*.html")
            .unwrap()
            .build()
            .unwrap();
        assert!(ov.matched("foo.html", false).is_whitelist());
        assert!(ov.matched("foo.HTML", false).is_whitelist());
        assert!(ov.matched("foo.htm", false).is_ignore());
        assert!(ov.matched("foo.HTM", false).is_ignore());
    }

    #[test]
    fn default_case_sensitive() {
        let ov =
            OverrideBuilder::new(ROOT).add("*.html").unwrap().build().unwrap();
        assert!(ov.matched("foo.html", false).is_whitelist());
        assert!(ov.matched("foo.HTML", false).is_ignore());
        assert!(ov.matched("foo.htm", false).is_ignore());
        assert!(ov.matched("foo.HTM", false).is_ignore());
    }
}
