//! Numbering every expression in a file.
//!
//! The parser leaves every [`Expr`] holding [`ExprId::UNSET`], and this pass
//! replaces those with `0`, `1`, … in one walk of the finished tree. Keeping
//! it separate from the parser is what makes the ids a property of the file:
//! they follow source order, not the order the parser happened to build
//! things in, so re-parsing the same text gives the same numbers and a
//! parser change that reorders its own work does not renumber anything.
//!
//! The walk is deliberately exhaustive and deliberately written without a
//! catch-all arm. Every variant of [`ExprKind`] is named here, so a variant
//! added to the tree later stops compiling in this file rather than quietly
//! leaving its children unnumbered — which is the one failure this pass can
//! have, and the one that would be hardest to notice downstream.
//!
//! Recursion here is bounded by the parser's `MAX_NESTING_DEPTH`: a tree only
//! exists because the parser built it, and a walker spends less stack per
//! level than the parser did.

use crate::ast::{
    Arg, Block, Expr, ExprId, ExprKind, Item, ItemKind, MatchArm, Param, Pattern, PatternKind,
    SourceUnit, Stmt, StmtKind, StrPart,
};

/// Assigns every expression in `unit` an id unique within it.
///
/// Ids are handed out in pre-order and in source order, starting at
/// [`ExprId`]`(0)`: an expression is numbered before its children, and a
/// child before anything to its right. So the ids of one unit are exactly
/// `0..n` for `n` expressions, with no gaps and no repeats, which is what
/// lets a later pass index a `Vec` of `n` entries by them.
///
/// The numbering starts from zero for each unit, so ids from two files are
/// not comparable. See [`ExprId`].
///
/// Running this twice over the same unit assigns the same ids again, so it is
/// idempotent rather than merely repeatable.
pub fn number_unit(unit: &mut SourceUnit) {
    let mut numberer = Numberer { next: 0 };
    for item in &mut unit.items {
        numberer.item(item);
    }
}

/// The one counter the walk hands out ids from.
struct Numberer {
    /// The id the next expression visited will be given.
    next: u32,
}

impl Numberer {
    /// Numbers everything an item can hold.
    ///
    /// A struct, an enum, and a type alias hold types and nothing else, and a
    /// type holds no expression: the parser gives a function type's
    /// parameters no default, which is the only place inside a type an
    /// expression could otherwise appear.
    fn item(&mut self, item: &mut Item) {
        match &mut item.kind {
            ItemKind::Fn(decl) => {
                self.params(&mut decl.params);
                self.block(&mut decl.body);
            }
            ItemKind::Struct(_) | ItemKind::Enum(_) | ItemKind::TypeAlias(_) => {}
            ItemKind::Trait(decl) => {
                for method in &mut decl.methods {
                    self.params(&mut method.params);
                    if let Some(body) = &mut method.default {
                        self.block(body);
                    }
                }
            }
            ItemKind::Impl(block) => {
                for item in &mut block.items {
                    self.item(item);
                }
            }
        }
    }

    /// Numbers the default values of a parameter list.
    ///
    /// A default is an ordinary expression evaluated at the call site, so it
    /// is numbered like one, and it comes before the body because that is
    /// where it is written.
    fn params(&mut self, params: &mut [Param]) {
        for param in params {
            if let Some(default) = &mut param.default {
                self.expr(default);
            }
        }
    }

    fn block(&mut self, block: &mut Block) {
        for stmt in &mut block.statements {
            self.stmt(stmt);
        }
        if let Some(tail) = &mut block.tail {
            self.expr(tail);
        }
    }

    fn stmt(&mut self, stmt: &mut Stmt) {
        match &mut stmt.kind {
            StmtKind::Let { value, .. } => self.expr(value),
            StmtKind::Expr(value) => self.expr(value),
            StmtKind::Item(item) => self.item(item),
        }
    }

    /// Gives `expr` the next id, then numbers its children left to right.
    fn expr(&mut self, expr: &mut Expr) {
        expr.id = ExprId(self.next);
        self.next += 1;
        match &mut expr.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Duration(_)
            | ExprKind::Unit
            | ExprKind::Ident(_)
            | ExprKind::Continue => {}
            ExprKind::Str(parts) => {
                for part in parts {
                    match part {
                        StrPart::Text(_) => {}
                        StrPart::Interpolation(inner) => self.expr(inner),
                    }
                }
            }
            ExprKind::ArrayLit(elements) => {
                for element in elements {
                    self.expr(element);
                }
            }
            ExprKind::Field { base, name: _ } => self.expr(base),
            ExprKind::Call {
                callee,
                generics: _,
                args,
                trailing,
            } => {
                self.expr(callee);
                self.args(args);
                if let Some(trailing) = trailing {
                    self.expr(trailing);
                }
            }
            ExprKind::Unary { op: _, operand } => self.expr(operand),
            ExprKind::Binary { op: _, lhs, rhs } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Assign {
                op: _,
                target,
                value,
            } => {
                self.expr(target);
                self.expr(value);
            }
            ExprKind::Try(inner) => self.expr(inner),
            ExprKind::Await(inner) => self.expr(inner),
            ExprKind::Block(block) => self.block(block),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.expr(condition);
                self.block(then_branch);
                if let Some(else_branch) = else_branch {
                    self.expr(else_branch);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    self.arm(arm);
                }
            }
            ExprKind::For {
                binding: _,
                iterable,
                body,
            } => {
                self.expr(iterable);
                self.block(body);
            }
            ExprKind::While { condition, body } => {
                self.expr(condition);
                self.block(body);
            }
            ExprKind::Return(value) => self.optional(value),
            ExprKind::Break(value) => self.optional(value),
            ExprKind::Lambda {
                is_async: _,
                params,
                body,
            } => {
                self.params(params);
                self.block(body);
            }
            ExprKind::Scope { name: _, body } => self.block(body),
            ExprKind::Range {
                start,
                end,
                inclusive_end: _,
            } => {
                self.expr(start);
                self.expr(end);
            }
        }
    }

    fn optional(&mut self, value: &mut Option<Box<Expr>>) {
        if let Some(value) = value {
            self.expr(value);
        }
    }

    fn args(&mut self, args: &mut [Arg]) {
        for arg in args {
            self.expr(&mut arg.value);
        }
    }

    /// Numbers a match arm: its pattern first, then its body.
    ///
    /// A literal pattern holds a real expression, and it is numbered for the
    /// same reason every other expression is — a pass keyed by id must be
    /// total over the tree, and an unnumbered corner of it would read as
    /// another expression's entry.
    fn arm(&mut self, arm: &mut MatchArm) {
        self.pattern(&mut arm.pattern);
        self.expr(&mut arm.body);
    }

    fn pattern(&mut self, pattern: &mut Pattern) {
        match &mut pattern.kind {
            PatternKind::Wildcard | PatternKind::Binding(_) => {}
            PatternKind::Literal(value) => self.expr(value),
            PatternKind::Variant { path: _, payload } => {
                for pattern in payload {
                    self.pattern(pattern);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cove_diag::{SourceMap, Span};
    use std::collections::HashSet;

    /// A file that puts an expression in each of the corners this pass is
    /// most likely to miss: a parameter default, a string interpolation, a
    /// nested local `fn`, a match arm and its literal pattern, a trailing
    /// closure, and a range.
    const AWKWARD: &str = r#"/// Doc.
export fn main(limit: Int = 1 + 1) -> Result<Unit, Error> {
  let name = "a{limit}b"
  fn helper(x: Int) -> Int {
    x + 1
  }
  for i in 0..<limit {
    match i {
      0 => helper(i)
      other => other
    }
  }
  console.println("{name}") {
  }
  Ok(())
}
"#;

    fn parse(source: &str) -> SourceUnit {
        parse_with(source).0
    }

    /// Collects every expression in a unit, by a walk written independently
    /// of the one under test so that a shared omission cannot hide.
    fn collect(unit: &SourceUnit) -> Vec<Expr> {
        let mut found = Vec::new();
        for item in &unit.items {
            collect_item(item, &mut found);
        }
        found
    }

    fn collect_item(item: &Item, found: &mut Vec<Expr>) {
        match &item.kind {
            ItemKind::Fn(decl) => {
                collect_params(&decl.params, found);
                collect_block(&decl.body, found);
            }
            ItemKind::Struct(_) | ItemKind::Enum(_) | ItemKind::TypeAlias(_) => {}
            ItemKind::Trait(decl) => {
                for method in &decl.methods {
                    collect_params(&method.params, found);
                    if let Some(body) = &method.default {
                        collect_block(body, found);
                    }
                }
            }
            ItemKind::Impl(block) => {
                for item in &block.items {
                    collect_item(item, found);
                }
            }
        }
    }

    fn collect_params(params: &[Param], found: &mut Vec<Expr>) {
        for param in params {
            if let Some(default) = &param.default {
                collect_expr(default, found);
            }
        }
    }

    fn collect_block(block: &Block, found: &mut Vec<Expr>) {
        for stmt in &block.statements {
            match &stmt.kind {
                StmtKind::Let { value, .. } => collect_expr(value, found),
                StmtKind::Expr(value) => collect_expr(value, found),
                StmtKind::Item(item) => collect_item(item, found),
            }
        }
        if let Some(tail) = &block.tail {
            collect_expr(tail, found);
        }
    }

    fn collect_pattern(pattern: &Pattern, found: &mut Vec<Expr>) {
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Binding(_) => {}
            PatternKind::Literal(value) => collect_expr(value, found),
            PatternKind::Variant { path: _, payload } => {
                for pattern in payload {
                    collect_pattern(pattern, found);
                }
            }
        }
    }

    fn collect_expr(expr: &Expr, found: &mut Vec<Expr>) {
        found.push(expr.clone());
        match &expr.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Duration(_)
            | ExprKind::Unit
            | ExprKind::Ident(_)
            | ExprKind::Continue => {}
            ExprKind::Str(parts) => {
                for part in parts {
                    if let StrPart::Interpolation(inner) = part {
                        collect_expr(inner, found);
                    }
                }
            }
            ExprKind::ArrayLit(elements) => {
                for element in elements {
                    collect_expr(element, found);
                }
            }
            ExprKind::Field { base, .. } => collect_expr(base, found),
            ExprKind::Call {
                callee,
                args,
                trailing,
                ..
            } => {
                collect_expr(callee, found);
                for arg in args {
                    collect_expr(&arg.value, found);
                }
                if let Some(trailing) = trailing {
                    collect_expr(trailing, found);
                }
            }
            ExprKind::Unary { operand, .. } => collect_expr(operand, found),
            ExprKind::Binary { lhs, rhs, .. } => {
                collect_expr(lhs, found);
                collect_expr(rhs, found);
            }
            ExprKind::Assign { target, value, .. } => {
                collect_expr(target, found);
                collect_expr(value, found);
            }
            ExprKind::Try(inner) | ExprKind::Await(inner) => collect_expr(inner, found),
            ExprKind::Block(block) => collect_block(block, found),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                collect_expr(condition, found);
                collect_block(then_branch, found);
                if let Some(else_branch) = else_branch {
                    collect_expr(else_branch, found);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                collect_expr(scrutinee, found);
                for arm in arms {
                    collect_pattern(&arm.pattern, found);
                    collect_expr(&arm.body, found);
                }
            }
            ExprKind::For { iterable, body, .. } => {
                collect_expr(iterable, found);
                collect_block(body, found);
            }
            ExprKind::While { condition, body } => {
                collect_expr(condition, found);
                collect_block(body, found);
            }
            ExprKind::Return(value) | ExprKind::Break(value) => {
                if let Some(value) = value {
                    collect_expr(value, found);
                }
            }
            ExprKind::Lambda { params, body, .. } => {
                collect_params(params, found);
                collect_block(body, found);
            }
            ExprKind::Scope { body, .. } => collect_block(body, found),
            ExprKind::Range { start, end, .. } => {
                collect_expr(start, found);
                collect_expr(end, found);
            }
        }
    }

    /// The source text a span covers.
    fn snippet(sources: &SourceMap, span: Span) -> &str {
        &sources.get(span.file).text[span.start as usize..span.end as usize]
    }

    /// The ids of every expression whose source text is exactly `text`.
    fn ids_of(unit: &SourceUnit, sources: &SourceMap, text: &str) -> Vec<ExprId> {
        collect(unit)
            .iter()
            .filter(|expr| snippet(sources, expr.span) == text)
            .map(|expr| expr.id)
            .collect()
    }

    fn parse_with(source: &str) -> (SourceUnit, SourceMap) {
        let mut sources = SourceMap::new();
        let file = sources.add("test.cove", source.to_string());
        let unit = crate::parse_file(&sources, file).expect("source parses");
        (unit, sources)
    }

    #[test]
    fn every_expression_is_numbered() {
        let unit = parse(AWKWARD);
        let found = collect(&unit);
        assert!(!found.is_empty(), "the source has expressions");
        for expr in &found {
            assert_ne!(
                expr.id,
                ExprId::UNSET,
                "expression {:?} was left unnumbered",
                expr.kind
            );
        }
    }

    /// Checks completeness without trusting either hand-written walk.
    ///
    /// The derived `Debug` reaches every field of every node there is, so an
    /// id left unset shows up in its text wherever it is hiding, and the
    /// number of ids in that text is the number of expressions the tree
    /// actually holds — which is what says the walk above visits all of them.
    #[test]
    fn no_unset_id_survives_anywhere_in_the_tree() {
        let unit = parse(AWKWARD);
        let debug = format!("{unit:?}");
        let unset = format!("{:?}", ExprId::UNSET);
        assert!(
            !debug.contains(&unset),
            "an expression somewhere still holds {unset}"
        );
        assert_eq!(
            debug.matches("ExprId(").count(),
            collect(&unit).len(),
            "the walk in this test visits every expression the tree holds"
        );
    }

    #[test]
    fn the_ids_are_exactly_zero_to_n_without_gaps_or_duplicates() {
        let unit = parse(AWKWARD);
        let mut ids: Vec<u32> = collect(&unit).iter().map(|expr| expr.id.0).collect();
        let count = ids.len();
        ids.sort_unstable();
        assert_eq!(ids, (0..count as u32).collect::<Vec<_>>());
        assert_eq!(
            ids.iter().collect::<HashSet<_>>().len(),
            count,
            "no id is used twice"
        );
    }

    #[test]
    fn numbering_is_deterministic() {
        let first = collect(&parse(AWKWARD));
        let second = collect(&parse(AWKWARD));
        let first: Vec<_> = first.iter().map(|expr| (expr.id, expr.span)).collect();
        let second: Vec<_> = second.iter().map(|expr| (expr.id, expr.span)).collect();
        assert_eq!(first, second);
    }

    #[test]
    fn numbering_twice_changes_nothing() {
        let mut unit = parse(AWKWARD);
        let before: Vec<_> = collect(&unit).iter().map(|expr| expr.id).collect();
        number_unit(&mut unit);
        let after: Vec<_> = collect(&unit).iter().map(|expr| expr.id).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn the_easy_to_miss_corners_are_numbered() {
        let (unit, sources) = parse_with(AWKWARD);

        // A parameter default.
        assert_eq!(ids_of(&unit, &sources, "1 + 1").len(), 1);
        // A string interpolation: the expression inside the braces, not the
        // literal that holds it.
        assert_eq!(ids_of(&unit, &sources, "limit").len(), 2);
        // A match arm body, and the literal pattern beside it.
        assert_eq!(ids_of(&unit, &sources, "helper(i)").len(), 1);
        assert_eq!(ids_of(&unit, &sources, "0").len(), 2);
        // A trailing closure.
        assert_eq!(ids_of(&unit, &sources, "{\n  }").len(), 1);
        // The body of a nested local `fn`.
        assert_eq!(ids_of(&unit, &sources, "x + 1").len(), 1);

        for text in ["1 + 1", "limit", "helper(i)", "0", "{\n  }", "x + 1"] {
            for id in ids_of(&unit, &sources, text) {
                assert_ne!(id, ExprId::UNSET, "`{text}` was left unnumbered");
            }
        }
    }

    #[test]
    fn a_lambda_parameter_default_is_numbered() {
        let (unit, sources) =
            parse_with("export fn main() {\n  let f = fn(x = 7) {\n    x\n  }\n}\n");
        assert_eq!(ids_of(&unit, &sources, "7").len(), 1);
        for expr in collect(&unit) {
            assert_ne!(expr.id, ExprId::UNSET);
        }
    }

    #[test]
    fn ids_do_not_leak_across_files() {
        let mut sources = SourceMap::new();
        let first = sources.add("first.cove", "export fn a() {\n  1\n}\n".to_string());
        let second = sources.add("second.cove", "export fn b() {\n  2\n}\n".to_string());
        let first = crate::parse_file(&sources, first).expect("first parses");
        let second = crate::parse_file(&sources, second).expect("second parses");

        let first: Vec<_> = collect(&first).iter().map(|expr| expr.id).collect();
        let second: Vec<_> = collect(&second).iter().map(|expr| expr.id).collect();
        assert_eq!(first, vec![ExprId(0)]);
        assert_eq!(second, vec![ExprId(0)]);
    }

    #[test]
    fn parents_are_numbered_before_their_children() {
        let (unit, sources) = parse_with("export fn main() {\n  1 + 2\n}\n");
        let ids = |text| ids_of(&unit, &sources, text);
        assert_eq!(ids("1 + 2"), vec![ExprId(0)]);
        assert_eq!(ids("1"), vec![ExprId(1)]);
        assert_eq!(ids("2"), vec![ExprId(2)]);
    }
}
