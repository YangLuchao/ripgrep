use regex_syntax::ast::{self, Ast};

/// 用于分析正则表达式的AST结果（例如，用于支持智能大小写）。
#[derive(Clone, Debug)]
pub(crate) struct AstAnalysis {
    /// 当且仅当正则表达式中出现大写字母时为true。
    any_uppercase: bool,
    /// 当且仅当正则表达式包含任何字面值时为true。
    any_literal: bool,
}

impl AstAnalysis {
    /// 通过对“pattern”的AST进行分析，返回一个`AstAnalysis`值。
    ///
    /// 如果“pattern”不是有效的正则表达式，则返回`None`。
    #[cfg(test)]
    pub(crate) fn from_pattern(pattern: &str) -> Option<AstAnalysis> {
        regex_syntax::ast::parse::Parser::new()
            .parse(pattern)
            .map(|ast| AstAnalysis::from_ast(&ast))
            .ok()
    }

    /// 在给定AST的基础上执行AST分析。
    pub(crate) fn from_ast(ast: &Ast) -> AstAnalysis {
        let mut analysis = AstAnalysis::new();
        analysis.from_ast_impl(ast);
        analysis
    }

    /// 当且仅当模式中存在大写字母的字面值时返回true。
    ///
    /// 例如，像`\pL`这样的模式不包含大写字母字面值，即使`L`是大写字母，`\pL`类包含大写字母。
    pub(crate) fn any_uppercase(&self) -> bool {
        self.any_uppercase
    }

    /// 当且仅当正则表达式包含任何字面值时返回true。
    ///
    /// 例如，像`\pL`这样的模式报告为`false`，但是像`\pLfoo`这样的模式报告为`true`。
    pub(crate) fn any_literal(&self) -> bool {
        self.any_literal
    }

    /// 创建一个具有初始配置的新的`AstAnalysis`值。
    fn new() -> AstAnalysis {
        AstAnalysis { any_uppercase: false, any_literal: false }
    }

    fn from_ast_impl(&mut self, ast: &Ast) {
        if self.done() {
            return;
        }
        match *ast {
            Ast::Empty(_) => {}
            Ast::Flags(_)
            | Ast::Dot(_)
            | Ast::Assertion(_)
            | Ast::Class(ast::Class::Unicode(_))
            | Ast::Class(ast::Class::Perl(_)) => {}
            Ast::Literal(ref x) => {
                self.from_ast_literal(x);
            }
            Ast::Class(ast::Class::Bracketed(ref x)) => {
                self.from_ast_class_set(&x.kind);
            }
            Ast::Repetition(ref x) => {
                self.from_ast_impl(&x.ast);
            }
            Ast::Group(ref x) => {
                self.from_ast_impl(&x.ast);
            }
            Ast::Alternation(ref alt) => {
                for x in &alt.asts {
                    self.from_ast_impl(x);
                }
            }
            Ast::Concat(ref alt) => {
                for x in &alt.asts {
                    self.from_ast_impl(x);
                }
            }
        }
    }

    fn from_ast_class_set(&mut self, ast: &ast::ClassSet) {
        if self.done() {
            return;
        }
        match *ast {
            ast::ClassSet::Item(ref item) => {
                self.from_ast_class_set_item(item);
            }
            ast::ClassSet::BinaryOp(ref x) => {
                self.from_ast_class_set(&x.lhs);
                self.from_ast_class_set(&x.rhs);
            }
        }
    }

    fn from_ast_class_set_item(&mut self, ast: &ast::ClassSetItem) {
        if self.done() {
            return;
        }
        match *ast {
            ast::ClassSetItem::Empty(_)
            | ast::ClassSetItem::Ascii(_)
            | ast::ClassSetItem::Unicode(_)
            | ast::ClassSetItem::Perl(_) => {}
            ast::ClassSetItem::Literal(ref x) => {
                self.from_ast_literal(x);
            }
            ast::ClassSetItem::Range(ref x) => {
                self.from_ast_literal(&x.start);
                self.from_ast_literal(&x.end);
            }
            ast::ClassSetItem::Bracketed(ref x) => {
                self.from_ast_class_set(&x.kind);
            }
            ast::ClassSetItem::Union(ref union) => {
                for x in &union.items {
                    self.from_ast_class_set_item(x);
                }
            }
        }
    }

    fn from_ast_literal(&mut self, ast: &ast::Literal) {
        self.any_literal = true;
        self.any_uppercase = self.any_uppercase || ast.c.is_uppercase();
    }

    /// 当且仅当属性无论看到什么其他AST都永远不会改变时返回true。
    fn done(&self) -> bool {
        self.any_uppercase && self.any_literal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analysis(pattern: &str) -> AstAnalysis {
        AstAnalysis::from_pattern(pattern).unwrap()
    }

    #[test]
    fn various() {
        let x = analysis("");
        assert!(!x.any_uppercase);
        assert!(!x.any_literal);

        let x = analysis("foo");
        assert!(!x.any_uppercase);
        assert!(x.any_literal);

        let x = analysis("Foo");
        assert!(x.any_uppercase);
        assert!(x.any_literal);

        let x = analysis("foO");
        assert!(x.any_uppercase);
        assert!(x.any_literal);

        let x = analysis(r"foo\\");
        assert!(!x.any_uppercase);
        assert!(x.any_literal);

        let x = analysis(r"foo\w");
        assert!(!x.any_uppercase);
        assert!(x.any_literal);

        let x = analysis(r"foo\S");
        assert!(!x.any_uppercase);
        assert!(x.any_literal);

        let x = analysis(r"foo\p{Ll}");
        assert!(!x.any_uppercase);
        assert!(x.any_literal);

        let x = analysis(r"foo[a-z]");
        assert!(!x.any_uppercase);
        assert!(x.any_literal);

        let x = analysis(r"foo[A-Z]");
        assert!(x.any_uppercase);
        assert!(x.any_literal);

        let x = analysis(r"foo[\S\t]");
        assert!(!x.any_uppercase);
        assert!(x.any_literal);

        let x = analysis(r"foo\\S");
        assert!(x.any_uppercase);
        assert!(x.any_literal);

        let x = analysis(r"\p{Ll}");
        assert!(!x.any_uppercase);
        assert!(!x.any_literal);

        let x = analysis(r"aBc\w");
        assert!(x.any_uppercase);
        assert!(x.any_literal);

        let x = analysis(r"a\u0061");
        assert!(!x.any_uppercase);
        assert!(x.any_literal);
    }
}
