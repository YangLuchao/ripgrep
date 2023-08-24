/*!
types 模块提供了一种将文件名的通配符关联到文件类型的方法。

这可以用于匹配特定类型的文件。例如，在默认提供的文件类型中，Rust 文件类型被定义为 `*.rs`，名称为 `rust`。
类似地，C 文件类型被定义为 `*.{c,h}`，名称为 `c`。

请注意，默认类型的集合可能会随时间而改变。

# 示例

这展示了如何使用此 crate 中定义的默认文件类型创建和使用简单的文件类型匹配器。

```
use ignore::types::TypesBuilder;

let mut builder = TypesBuilder::new();
builder.add_defaults();
builder.select("rust");
let matcher = builder.build().unwrap();

assert!(matcher.matched("foo.rs", false).is_whitelist());
assert!(matcher.matched("foo.c", false).is_ignore());
```

# 示例：否定

这与前一个示例类似，但显示了如何否定文件类型。也就是说，这将允许我们匹配与特定文件类型*不对应*的文件路径。

```
use ignore::types::TypesBuilder;

let mut builder = TypesBuilder::new();
builder.add_defaults();
builder.negate("c");
let matcher = builder.build().unwrap();

assert!(matcher.matched("foo.rs", false).is_none());
assert!(matcher.matched("foo.c", false).is_ignore());
```

# 示例：自定义文件类型定义

这展示了如何通过自己的定义扩展此库的默认文件类型定义。

```
use ignore::types::TypesBuilder;

let mut builder = TypesBuilder::new();
builder.add_defaults();
builder.add("foo", "*.foo");
// 添加文件类型定义的另一种方法。
// 当从最终用户那里接受输入时，这很有用。
builder.add_def("bar:*.bar");
// 注意：我们只选择了 `foo`，没有选择 `bar`。
builder.select("foo");
let matcher = builder.build().unwrap();

assert!(matcher.matched("x.foo", false).is_whitelist());
// 这会被忽略，因为我们只选择了 `foo` 文件类型。
assert!(matcher.matched("x.bar", false).is_ignore());
```

我们还可以基于其他定义添加文件类型定义。

```
use ignore::types::TypesBuilder;

let mut builder = TypesBuilder::new();
builder.add_defaults();
builder.add("foo", "*.foo");
builder.add_def("bar:include:foo,cpp");
builder.select("bar");
let matcher = builder.build().unwrap();

assert!(matcher.matched("x.foo", false).is_whitelist());
assert!(matcher.matched("y.cpp", false).is_whitelist());
```
*/

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use regex::Regex;
use thread_local::ThreadLocal;

use crate::default_types::DEFAULT_TYPES;
use crate::pathutil::file_name;
use crate::{Error, Match};
/// `Glob` 表示文件类型定义集合中的单个通配符。
///
/// 对于特定的文件类型可能会有多个通配符。
///
/// 该结构用于报告匹配的最高优先级通配符的相关信息。
///
/// 需要注意的是，并非所有的匹配都必然对应于特定的通配符。
/// 例如，如果存在一个或多个选择，并且文件路径不匹配这些选择中的任何一个，那么文件路径会被视为被忽略。
///
/// 生命周期 `'a` 指的是底层文件类型定义的生命周期，与文件类型匹配器的生命周期相对应。
#[derive(Clone, Debug)]
pub struct Glob<'a>(GlobInner<'a>);

#[derive(Clone, Debug)]
enum GlobInner<'a> {
    /// 没有匹配的通配符，但文件路径仍然应被忽略。
    UnmatchedIgnore,
    /// 有一个匹配的通配符。
    Matched {
        /// 提供匹配的文件类型定义。
        def: &'a FileTypeDef,
    },
}

impl<'a> Glob<'a> {
    fn unmatched() -> Glob<'a> {
        Glob(GlobInner::UnmatchedIgnore)
    }

    /// 返回匹配的文件类型定义，如果存在的话。当特定定义匹配文件路径时，总是存在一个文件类型定义。
    pub fn file_type_def(&self) -> Option<&FileTypeDef> {
        match self {
            Glob(GlobInner::UnmatchedIgnore) => None,
            Glob(GlobInner::Matched { def, .. }) => Some(def),
        }
    }
}

/// 单个文件类型定义。
///
/// 文件类型定义可以从文件类型匹配器中汇总获得。文件类型定义也会在其负责的匹配时报告。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileTypeDef {
    name: String,
    globs: Vec<String>,
}

impl FileTypeDef {
    /// 返回此文件类型的名称。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 返回用于识别此文件类型的通配符。
    pub fn globs(&self) -> &[String] {
        &self.globs
    }
}

/// `Types` 是一个文件类型匹配器。
#[derive(Clone, Debug)]
pub struct Types {
    /// 所有的文件类型定义，按名称按字典序排序。
    defs: Vec<FileTypeDef>,
    /// 用户进行的所有选择。
    selections: Vec<Selection<FileTypeDef>>,
    /// 我们的选择中是否至少有一个 Selection::Select。
    /// 当为 true 时，Match::None 转换为 Match::Ignore。
    has_selected: bool,
    /// 从集合中的通配符索引到两个索引的映射。
    /// 第一个索引是 `selections` 中的索引，第二个索引是对应的文件类型定义的通配符列表中的索引。
    glob_to_selection: Vec<(usize, usize)>,
    /// 所有通配符选择的集合，用于实际匹配。
    set: GlobSet,
    /// 匹配的临时存储。
    matches: Arc<ThreadLocal<RefCell<Vec<usize>>>>,
}

/// 指示特定文件类型的选择类型。
#[derive(Clone, Debug)]
enum Selection<T> {
    Select(String, T),
    Negate(String, T),
}

impl<T> Selection<T> {
    fn is_negated(&self) -> bool {
        match *self {
            Selection::Select(..) => false,
            Selection::Negate(..) => true,
        }
    }

    fn name(&self) -> &str {
        match *self {
            Selection::Select(ref name, _) => name,
            Selection::Negate(ref name, _) => name,
        }
    }

    fn map<U, F: FnOnce(T) -> U>(self, f: F) -> Selection<U> {
        match self {
            Selection::Select(name, inner) => {
                Selection::Select(name, f(inner))
            }
            Selection::Negate(name, inner) => {
                Selection::Negate(name, f(inner))
            }
        }
    }

    fn inner(&self) -> &T {
        match *self {
            Selection::Select(_, ref inner) => inner,
            Selection::Negate(_, ref inner) => inner,
        }
    }
}
impl Types {
    /// 创建一个新的文件类型匹配器，该匹配器不会匹配任何路径，并且不包含任何文件类型定义。
    pub fn empty() -> Types {
        Types {
            defs: vec![],
            selections: vec![],
            has_selected: false,
            glob_to_selection: vec![],
            set: GlobSetBuilder::new().build().unwrap(),
            matches: Arc::new(ThreadLocal::default()),
        }
    }

    /// 当且仅当此匹配器没有任何选择时返回 true。
    pub fn is_empty(&self) -> bool {
        self.selections.is_empty()
    }

    /// 返回此匹配器中使用的选择数量。
    pub fn len(&self) -> usize {
        self.selections.len()
    }

    /// 返回当前文件类型定义的集合。
    ///
    /// 定义和通配符已按顺序排序。
    pub fn definitions(&self) -> &[FileTypeDef] {
        &self.defs
    }

    /// 返回给定路径相对于此文件类型匹配器的匹配情况。
    ///
    /// 如果路径与所选文件类型匹配，则被视为白名单。
    /// 如果路径与否定文件类型匹配，则被视为被忽略。
    /// 如果至少选择了一个文件类型并且 `path` 不匹配，则路径也被视为被忽略。
    pub fn matched<'a, P: AsRef<Path>>(
        &'a self,
        path: P,
        is_dir: bool,
    ) -> Match<Glob<'a>> {
        // 文件类型不适用于目录，并且如果我们的通配符集为空，则无法执行任何操作。
        if is_dir || self.set.is_empty() {
            return Match::None;
        }
        // 我们只想匹配文件名，因此提取它。
        // 如果不存在文件名，则无法匹配它。
        let name = match file_name(path.as_ref()) {
            Some(name) => name,
            None if self.has_selected => {
                return Match::Ignore(Glob::unmatched());
            }
            None => {
                return Match::None;
            }
        };
        let mut matches = self.matches.get_or_default().borrow_mut();
        self.set.matches_into(name, &mut *matches);
        // 最高优先级的匹配是最后一个。
        if let Some(&i) = matches.last() {
            let (isel, _) = self.glob_to_selection[i];
            let sel = &self.selections[isel];
            let glob = Glob(GlobInner::Matched { def: sel.inner() });
            return if sel.is_negated() {
                Match::Ignore(glob)
            } else {
                Match::Whitelist(glob)
            };
        }
        if self.has_selected {
            Match::Ignore(Glob::unmatched())
        } else {
            Match::None
        }
    }
}

/// `TypesBuilder` 从一组文件类型定义和一组文件类型选择中构建文件类型匹配器。
pub struct TypesBuilder {
    types: HashMap<String, FileTypeDef>,
    selections: Vec<Selection<()>>,
}

impl TypesBuilder {
    /// 创建一个新的文件类型匹配器的构建器。
    ///
    /// 构建器最初不包含任何类型定义。
    /// 可以使用 `add_defaults` 添加一组默认类型定义，并使用 `select` 和 `negate` 添加其他类型定义。
    pub fn new() -> TypesBuilder {
        TypesBuilder { types: HashMap::new(), selections: vec![] }
    }

    /// 将当前一组文件类型定义 *以及* 选择构建为文件类型匹配器。
    pub fn build(&self) -> Result<Types, Error> {
        let defs = self.definitions();
        let has_selected = self.selections.iter().any(|s| !s.is_negated());

        let mut selections = vec![];
        let mut glob_to_selection = vec![];
        let mut build_set = GlobSetBuilder::new();
        for (isel, selection) in self.selections.iter().enumerate() {
            let def = match self.types.get(selection.name()) {
                Some(def) => def.clone(),
                None => {
                    let name = selection.name().to_string();
                    return Err(Error::UnrecognizedFileType(name));
                }
            };
            for (iglob, glob) in def.globs.iter().enumerate() {
                build_set.add(
                    GlobBuilder::new(glob)
                        .literal_separator(true)
                        .build()
                        .map_err(|err| Error::Glob {
                            glob: Some(glob.to_string()),
                            err: err.kind().to_string(),
                        })?,
                );
                glob_to_selection.push((isel, iglob));
            }
            selections.push(selection.clone().map(move |_| def));
        }
        let set = build_set
            .build()
            .map_err(|err| Error::Glob { glob: None, err: err.to_string() })?;
        Ok(Types {
            defs: defs,
            selections: selections,
            has_selected: has_selected,
            glob_to_selection: glob_to_selection,
            set: set,
            matches: Arc::new(ThreadLocal::default()),
        })
    }

    /// 返回当前文件类型定义的集合。
    ///
    /// 定义和通配符已排序。
    pub fn definitions(&self) -> Vec<FileTypeDef> {
        let mut defs = vec![];
        for def in self.types.values() {
            let mut def = def.clone();
            def.globs.sort();
            defs.push(def);
        }
        defs.sort_by(|def1, def2| def1.name().cmp(def2.name()));
        defs
    }

    /// 选择由 `name` 给出的文件类型。
    ///
    /// 如果 `name` 是 `all`，则选择所有当前已定义的文件类型。
    pub fn select(&mut self, name: &str) -> &mut TypesBuilder {
        if name == "all" {
            for name in self.types.keys() {
                self.selections.push(Selection::Select(name.to_string(), ()));
            }
        } else {
            self.selections.push(Selection::Select(name.to_string(), ()));
        }
        self
    }

    /// 忽略由 `name` 给出的文件类型。
    ///
    /// 如果 `name` 是 `all`，则否定所有当前已定义的文件类型。
    pub fn negate(&mut self, name: &str) -> &mut TypesBuilder {
        if name == "all" {
            for name in self.types.keys() {
                self.selections.push(Selection::Negate(name.to_string(), ()));
            }
        } else {
            self.selections.push(Selection::Negate(name.to_string(), ()));
        }
        self
    }

    /// 清除给定类型名称的任何文件类型定义。
    pub fn clear(&mut self, name: &str) -> &mut TypesBuilder {
        self.types.remove(name);
        self
    }

    /// 添加新的文件类型定义。`name` 可以是任意的，`glob` 应该是一个识别属于 `name` 类型的文件路径的通配符。
    ///
    /// 如果 `name` 是 `all`，或者包含任何不是 Unicode 字母或数字的字符，则会返回错误。
    pub fn add(&mut self, name: &str, glob: &str) -> Result<(), Error> {
        lazy_static::lazy_static! {
            static ref RE: Regex = Regex::new(r"^[\pL\pN]+$").unwrap();
        };
        if name == "all" || !RE.is_match(name) {
            return Err(Error::InvalidDefinition);
        }
        let (key, glob) = (name.to_string(), glob.to_string());
        self.types
            .entry(key)
            .or_insert_with(|| FileTypeDef {
                name: name.to_string(),
                globs: vec![],
            })
            .globs
            .push(glob);
        Ok(())
    }

    /// 以字符串形式添加新的文件类型定义。有两种有效格式：
    /// 1. `{name}:{glob}`。这定义了一个“根”定义，将给定的名称与给定的通配符关联起来。
    /// 2. `{name}:include:{逗号分隔的已定义名称列表}`。
    ///    这定义了一个“包含”定义，将给定的名称与给定现有类型的定义相关联。
    /// 名称不得包含任何不是 Unicode 字母或数字的字符。
    pub fn add_def(&mut self, def: &str) -> Result<(), Error> {
        let parts: Vec<&str> = def.split(':').collect();
        match parts.len() {
            2 => {
                let name = parts[0];
                let glob = parts[1];
                if name.is_empty() || glob.is_empty() {
                    return Err(Error::InvalidDefinition);
                }
                self.add(name, glob)
            }
            3 => {
                let name = parts[0];
                let types_string = parts[2];
                if name.is_empty()
                    || parts[1] != "include"
                    || types_string.is_empty()
                {
                    return Err(Error::InvalidDefinition);
                }
                let types = types_string.split(',');
                // 提前检查以确保所有指定的类型都存在，如果不存在，则提前失败。
                if types.clone().any(|t| !self.types.contains_key(t)) {
                    return Err(Error::InvalidDefinition);
                }
                for type_name in types {
                    let globs =
                        self.types.get(type_name).unwrap().globs.clone();
                    for glob in globs {
                        self.add(name, &glob)?;
                    }
                }
                Ok(())
            }
            _ => Err(Error::InvalidDefinition),
        }
    }

    /// 添加一组默认文件类型定义。
    pub fn add_defaults(&mut self) -> &mut TypesBuilder {
        static MSG: &'static str = "adding a default type should never fail";
        for &(names, exts) in DEFAULT_TYPES {
            for name in names {
                for ext in exts {
                    self.add(name, ext).expect(MSG);
                }
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::TypesBuilder;

    macro_rules! matched {
        ($name:ident, $types:expr, $sel:expr, $selnot:expr,
         $path:expr) => {
            matched!($name, $types, $sel, $selnot, $path, true);
        };
        (not, $name:ident, $types:expr, $sel:expr, $selnot:expr,
         $path:expr) => {
            matched!($name, $types, $sel, $selnot, $path, false);
        };
        ($name:ident, $types:expr, $sel:expr, $selnot:expr,
         $path:expr, $matched:expr) => {
            #[test]
            fn $name() {
                let mut btypes = TypesBuilder::new();
                for tydef in $types {
                    btypes.add_def(tydef).unwrap();
                }
                for sel in $sel {
                    btypes.select(sel);
                }
                for selnot in $selnot {
                    btypes.negate(selnot);
                }
                let types = btypes.build().unwrap();
                let mat = types.matched($path, false);
                assert_eq!($matched, !mat.is_ignore());
            }
        };
    }

    fn types() -> Vec<&'static str> {
        vec![
            "html:*.html",
            "html:*.htm",
            "rust:*.rs",
            "js:*.js",
            "py:*.py",
            "python:*.py",
            "foo:*.{rs,foo}",
            "combo:include:html,rust",
        ]
    }

    matched!(match1, types(), vec!["rust"], vec![], "lib.rs");
    matched!(match2, types(), vec!["html"], vec![], "index.html");
    matched!(match3, types(), vec!["html"], vec![], "index.htm");
    matched!(match4, types(), vec!["html", "rust"], vec![], "main.rs");
    matched!(match5, types(), vec![], vec![], "index.html");
    matched!(match6, types(), vec![], vec!["rust"], "index.html");
    matched!(match7, types(), vec!["foo"], vec!["rust"], "main.foo");
    matched!(match8, types(), vec!["combo"], vec![], "index.html");
    matched!(match9, types(), vec!["combo"], vec![], "lib.rs");
    matched!(match10, types(), vec!["py"], vec![], "main.py");
    matched!(match11, types(), vec!["python"], vec![], "main.py");

    matched!(not, matchnot1, types(), vec!["rust"], vec![], "index.html");
    matched!(not, matchnot2, types(), vec![], vec!["rust"], "main.rs");
    matched!(not, matchnot3, types(), vec!["foo"], vec!["rust"], "main.rs");
    matched!(not, matchnot4, types(), vec!["rust"], vec!["foo"], "main.rs");
    matched!(not, matchnot5, types(), vec!["rust"], vec!["foo"], "main.foo");
    matched!(not, matchnot6, types(), vec!["combo"], vec![], "leftpad.js");
    matched!(not, matchnot7, types(), vec!["py"], vec![], "index.html");
    matched!(not, matchnot8, types(), vec!["python"], vec![], "doc.md");

    #[test]
    fn test_invalid_defs() {
        let mut btypes = TypesBuilder::new();
        for tydef in types() {
            btypes.add_def(tydef).unwrap();
        }
        // Preserve the original definitions for later comparison.
        let original_defs = btypes.definitions();
        let bad_defs = vec![
            // Reference to type that does not exist
            "combo:include:html,qwerty",
            // Bad format
            "combo:foobar:html,rust",
            "",
        ];
        for def in bad_defs {
            assert!(btypes.add_def(def).is_err());
            // Ensure that nothing changed, even if some of the includes were valid.
            assert_eq!(btypes.definitions(), original_defs);
        }
    }
}
