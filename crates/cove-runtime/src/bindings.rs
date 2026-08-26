//! Working out, before the program runs, which frame index a name reference
//! will denote.
//!
//! The interpreter finds a local by name: it scans the bindings its call has
//! declared in reverse and compares strings. That search always answers, and
//! for almost every reference it answers the same index every time it is
//! asked, because the frame layout of a call is decided by the *shape* of the
//! body rather than by what the body computed. This pass works that index out
//! once, from the shape alone, so the search does not have to.
//!
//! # This is an accelerator and nothing else
//!
//! A reference resolves here only when this pass can prove it denotes exactly
//! one entry of an environment's frame, at an index that is the same on
//! every run of that body. Everything else answers [`None`], and the
//! interpreter searches as it always did. Being unresolved costs a scan;
//! being *wrong* would cost the meaning of the program, so every rule below
//! that says "then resolve nothing" is a rule about what this pass declines
//! to claim rather than a limitation to be tightened later.
//!
//! Two things keep the claim honest. `Env::at` checks that the binding at
//! the index carries the name the reference was written with, so a wrong
//! index falls back rather than reading a stranger's cell. And an id written
//! twice with two different answers is poisoned to "unresolved" rather than
//! keeping either, so a body reachable under two frame layouts resolves to
//! neither.
//!
//! # The layout being predicted
//!
//! [`Interpreter::invoke_body`] declares into the frame, in this order:
//! `self` when the target has a receiver, then one binding per declared
//! parameter in declaration order — variadic, `var` alias, ordinary, and
//! defaulted parameters each declare exactly one. The body's block then
//! declares locals as it executes. Captures are not in the frame, which is
//! what makes the parameter indices static: a closure's captures are decided
//! when it is created, so their number is a run-time fact.
//!
//! So index 0 is `self` in a method and parameter 0 in a free function.
//!
//! # Recursion
//!
//! The walk recurses once per level of expression nesting, like the parser
//! that built the tree and the numbering pass that ran over it. The parser's
//! `MAX_NESTING_DEPTH` therefore bounds it, and a walker spends less stack
//! per level than the parser did.
//!
//! [`Interpreter::invoke_body`]: crate::interp::Interpreter

use cove_diag::{FileId, Span};
use cove_schema::builtins::NONE_CASE;
use cove_sema::resolve::Program;
use cove_syntax::ast::{
    Arg, Block, Expr, ExprId, ExprKind, FnDecl, ItemKind, Param, Pattern, PatternKind, Stmt,
    StmtKind, StrPart,
};

/// What one expression id was resolved to.
///
/// The three states are distinct because a second, disagreeing answer has to
/// be distinguishable from a first one: an id that was never a name reference
/// and an id this pass declined to resolve both read as "no index", but only
/// the second may not be overwritten by a later index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    /// No name reference was recorded at this id.
    Unseen,
    /// The reference denotes this index of its call's frame.
    At(u32),
    /// The reference was seen and not resolved, or was resolved twice to two
    /// different indices.
    Unresolved,
}

/// Where each name reference of a program was resolved to, when it could be.
///
/// Keyed by file and then by [`ExprId`], because ids are dense within a file:
/// a lookup is two bounds-checked loads rather than a hash. An id belongs to
/// one file, so the file has to be carried alongside it, and the span a
/// reference already holds is where it comes from.
///
/// A default `Bindings` resolves nothing, which is the identity of an
/// accelerator: every reference falls back and the program means the same.
#[derive(Default)]
pub struct Bindings {
    /// One table per [`FileId`], each indexed by [`ExprId`].
    ///
    /// Both are grown to what the walk actually saw rather than sized from
    /// the source map: a file holding no function body needs no table, and a
    /// file's expressions past its last name reference need no entries.
    files: Vec<Vec<Slot>>,
}

impl Bindings {
    /// Resolves every function body `program` can run.
    ///
    /// That is every declared function, method, associated function, and
    /// test, plus every trait default body — and, from inside those, every
    /// lambda body, trailing-closure block, and local `fn` body, each of
    /// which is a frame of its own.
    pub fn of(program: &Program) -> Bindings {
        let mut resolver = Resolver::default();
        for module in program.modules.values() {
            for entry in module.functions.values() {
                resolver.declaration(&entry.decl);
            }
            for entry in module.methods.values() {
                resolver.declaration(&entry.decl);
            }
            // A trait's default body is recorded as a method of every type
            // that inherits it, so the loop above already reached it — under
            // the same receiver and the same parameters each time, which is
            // why the answers agree rather than poisoning each other. It is
            // walked here as well so that a default no conformance inherited
            // is still resolved.
            for entry in module.traits.values() {
                for method in &entry.decl.methods {
                    if let Some(body) = &method.default {
                        resolver.body(method.receiver.is_some(), &method.params, body);
                    }
                }
            }
        }
        resolver.bindings
    }

    /// The frame index the reference at `id` denotes, if it denotes one.
    ///
    /// `span` is read for its file alone: an id numbers an expression within
    /// the file it was parsed from and means nothing outside it.
    pub fn frame_index(&self, span: Span, id: ExprId) -> Option<u32> {
        match self.files.get(span.file.0 as usize)?.get(id.0 as usize)? {
            Slot::At(index) => Some(*index),
            Slot::Unseen | Slot::Unresolved => None,
        }
    }

    /// Records what the walk worked out about one reference.
    ///
    /// A second answer that agrees changes nothing; a second answer that
    /// disagrees leaves the id unresolved for good. Disagreement means one
    /// body is reachable under two frame layouts, and the interpreter has no
    /// way to tell which one it is running.
    fn record(&mut self, file: FileId, id: ExprId, slot: Slot) {
        if id == ExprId::UNSET {
            return;
        }
        let file = file.0 as usize;
        if self.files.len() <= file {
            self.files.resize_with(file + 1, Vec::new);
        }
        let table = &mut self.files[file];
        let index = id.0 as usize;
        if table.len() <= index {
            table.resize(index + 1, Slot::Unseen);
        }
        table[index] = match table[index] {
            Slot::Unseen => slot,
            existing if existing == slot => existing,
            _ => Slot::Unresolved,
        };
    }
}

/// The frame one body is predicted to build, as the walk stands in it.
///
/// `live` holds the names of every binding declared and not yet left, in
/// declaration order — the same order and the same entries the interpreter's
/// `frame` holds — so a name's position in it *is* the frame index that
/// binding will occupy. Every declaration appends exactly one entry, and
/// leaving a block truncates both lists to where it began, which is what
/// keeps the two in step.
#[derive(Default)]
struct Frame<'a> {
    live: Vec<&'a str>,
    /// One entry per open block scope: where it begins in `live`.
    marks: Vec<usize>,
}

impl<'a> Frame<'a> {
    fn push(&mut self) {
        self.marks.push(self.live.len());
    }

    fn pop(&mut self) {
        if let Some(mark) = self.marks.pop() {
            self.live.truncate(mark);
        }
    }

    fn declare(&mut self, name: &'a str) {
        self.live.push(name);
    }

    /// The index a reference to `name` denotes here.
    ///
    /// The search is from the end, so the latest declaration of a shadowed
    /// name wins — which is what a reverse scan of the frame answers, and so
    /// what the interpreter would have found.
    ///
    /// A frame with more than `u32::MAX` bindings has no index this can
    /// report, and answers unresolved rather than a truncated one.
    fn index_of(&self, name: &str) -> Option<u32> {
        let found = self.live.iter().rposition(|declared| *declared == name)?;
        u32::try_from(found).ok()
    }
}

/// The walk that fills a [`Bindings`] in, one body at a time.
///
/// Its every step mirrors a step the interpreter takes, and the comments
/// below say which: where they disagree, this pass is wrong and the
/// interpreter is right.
#[derive(Default)]
struct Resolver<'a> {
    bindings: Bindings,
    /// The body being walked. A nested body swaps this out for one of its
    /// own and puts it back afterwards, because a nested body is a separate
    /// call with a separate frame.
    frame: Frame<'a>,
}

impl<'a> Resolver<'a> {
    fn declaration(&mut self, decl: &'a FnDecl) {
        self.body(decl.receiver.is_some(), &decl.params, &decl.body);
    }

    /// Resolves one body as a frame of its own.
    ///
    /// `self` comes first when there is a receiver, then one binding per
    /// parameter. A parameter's default is resolved *before* that parameter
    /// is declared and after the ones to its left, because the callee
    /// evaluates it there: `fn f(a: Int, b: Int = a)` reads the `a` it was
    /// passed.
    fn body(&mut self, has_receiver: bool, params: &'a [Param], block: &'a Block) {
        let enclosing = std::mem::take(&mut self.frame);
        if has_receiver {
            self.frame.declare("self");
        }
        for param in params {
            if let Some(default) = &param.default {
                self.expr(default);
            }
            self.frame.declare(param.name.node.as_str());
        }
        self.block(block);
        self.frame = enclosing;
    }

    /// A block is a scope: what it declares is discarded when it ends, and
    /// the indices it used are used again by whatever the enclosing scope
    /// declares next.
    fn block(&mut self, block: &'a Block) {
        self.frame.push();
        for stmt in &block.statements {
            self.stmt(stmt);
        }
        if let Some(tail) = &block.tail {
            self.expr(tail);
        }
        self.frame.pop();
    }

    fn stmt(&mut self, stmt: &'a Stmt) {
        match &stmt.kind {
            // The value is resolved first and the name declared after, so
            // `let x = x` reads the outer `x`, as the interpreter does.
            StmtKind::Let { name, value, .. } => {
                self.expr(value);
                self.frame.declare(name.node.as_str());
            }
            StmtKind::Expr(value) => self.expr(value),
            // A local `fn` becomes a closure, and the closure is built before
            // the name is declared — so its body cannot see itself and cannot
            // recurse. Its body is resolved first, in a frame of its own, for
            // exactly that reason: declaring the name first would resolve the
            // recursive call this language does not have. It is a closure and
            // never a method, so it has no receiver whatever the parser
            // accepted.
            //
            // The interpreter refuses every other item inside a body, so
            // there is nothing else here to resolve.
            StmtKind::Item(item) => {
                if let ItemKind::Fn(decl) = &item.kind {
                    self.body(false, &decl.params, &decl.body);
                    self.frame.declare(decl.name.node.as_str());
                }
            }
        }
    }

    /// Resolves one expression, and its children in the order they are
    /// evaluated.
    ///
    /// Written without a catch-all arm, as the numbering pass is: a variant
    /// added to the tree later stops compiling here rather than quietly
    /// leaving a body half-resolved.
    fn expr(&mut self, expr: &'a Expr) {
        match &expr.kind {
            ExprKind::Ident(name) => {
                let slot = match self.frame.index_of(name) {
                    Some(index) => Slot::At(index),
                    // A name this body did not declare is a capture, a
                    // module declaration, a host name, or a mistake. None of
                    // those is in the frame.
                    None => Slot::Unresolved,
                };
                self.bindings.record(expr.span.file, expr.id, slot);
            }
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Duration(_)
            | ExprKind::Unit
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
                    self.trailing(trailing);
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
            ExprKind::Try(inner) | ExprKind::Await(inner) => self.expr(inner),
            ExprKind::Block(block) => self.block(block),
            // A condition is evaluated in the enclosing scope; each branch
            // opens its own.
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
                    // A `None` binder binds, or matches a case and binds
                    // nothing, depending on the value the arm is tried
                    // against. Every index after it in the arm is therefore
                    // a run-time fact, so nothing in the arm is resolved —
                    // including the arm's body, which is inside that scope.
                    if binds_none(&arm.pattern) {
                        continue;
                    }
                    self.frame.push();
                    self.pattern(&arm.pattern);
                    self.expr(&arm.body);
                    self.frame.pop();
                }
            }
            // The iterable is evaluated once, in the enclosing scope. The
            // binding is then declared in a scope of its own, one fresh cell
            // per iteration, with the body's own block inside it.
            ExprKind::For {
                binding,
                iterable,
                body,
            } => {
                self.expr(iterable);
                self.frame.push();
                self.frame.declare(binding.node.as_str());
                self.block(body);
                self.frame.pop();
            }
            ExprKind::While { condition, body } => {
                self.expr(condition);
                self.block(body);
            }
            ExprKind::Return(value) | ExprKind::Break(value) => {
                if let Some(value) = value {
                    self.expr(value);
                }
            }
            // A lambda is a call of its own: its parameters are its frame's
            // first bindings, and every other name its body mentions is a
            // capture, which is not in the frame at all.
            ExprKind::Lambda {
                is_async: _,
                params,
                body,
            } => self.body(false, params, body),
            // The scope's name is a binding of its own, declared in a scope
            // wrapping the body's block.
            ExprKind::Scope { name, body } => {
                self.frame.push();
                self.frame.declare(name.node.as_str());
                self.block(body);
                self.frame.pop();
            }
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

    fn args(&mut self, args: &'a [Arg]) {
        for arg in args {
            self.expr(&arg.value);
        }
    }

    /// A trailing block is a closure argument, so it is a frame of its own
    /// with no parameters. A trailing anything else is an ordinary
    /// expression of this frame, which is what the interpreter makes of it.
    fn trailing(&mut self, expr: &'a Expr) {
        match &expr.kind {
            ExprKind::Block(block) => self.body(false, &[], block),
            _ => self.expr(expr),
        }
    }

    /// Declares a pattern's binders in the order the interpreter declares
    /// them: left to right, depth first, with a literal's expression
    /// evaluated where it is written and so after the binders to its left.
    fn pattern(&mut self, pattern: &'a Pattern) {
        match &pattern.kind {
            PatternKind::Wildcard => {}
            PatternKind::Binding(name) => self.frame.declare(name.as_str()),
            PatternKind::Literal(value) => self.expr(value),
            PatternKind::Variant { path: _, payload } => {
                for pattern in payload {
                    self.pattern(pattern);
                }
            }
        }
    }
}

/// Whether any part of `pattern` writes `None` as a binder.
///
/// `None` in a pattern is read as the `Option` case when the value is an
/// `Option` and as a name to bind when it is not, so a pattern holding one
/// declares a binding on some values and not on others.
fn binds_none(pattern: &Pattern) -> bool {
    match &pattern.kind {
        PatternKind::Binding(name) => name == NONE_CASE.name,
        PatternKind::Wildcard | PatternKind::Literal(_) => false,
        PatternKind::Variant { path: _, payload } => payload.iter().any(binds_none),
    }
}

/// How many local reads took the resolved path and how many fell back to the
/// search.
///
/// Debug builds only. The counters exist to prove that the hot path is the
/// resolved one, which is a claim about the interpreter rather than about a
/// program, so it is enough to be able to check it where assertions are
/// already on; a release build has neither the counters nor the atomics that
/// maintain them.
///
/// A read counts here only when it *was* a local read — when a place was
/// found, one way or the other. A name that turned out to be a function, a
/// type, or a host module was never in the frame and is neither a resolution
/// nor a fallback.
///
/// One run's counters, not one thread's: every task of a run shares the
/// [`Runtime`](crate::Runtime) that holds them, so what they report is what
/// the whole run did.
#[cfg(debug_assertions)]
#[derive(Default)]
pub struct ResolutionStats {
    resolved: std::sync::atomic::AtomicU64,
    fell_back: std::sync::atomic::AtomicU64,
}

#[cfg(debug_assertions)]
impl ResolutionStats {
    /// Records a local read that the resolved index answered.
    pub fn note_resolved(&self) {
        self.resolved
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Records a local read that the name search answered.
    pub fn note_fallback(&self) {
        self.fell_back
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// What the counters stand at.
    ///
    /// The two are read separately, so a run still in progress on another
    /// thread may be counted between them. Reading them after a run has
    /// finished, which is what a test does, has no such gap.
    pub fn counts(&self) -> ResolutionCounts {
        ResolutionCounts {
            resolved: self.resolved.load(std::sync::atomic::Ordering::Relaxed),
            fell_back: self.fell_back.load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

/// What [`ResolutionStats`] counted.
#[cfg(debug_assertions)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResolutionCounts {
    /// Local reads answered by a resolved frame index.
    pub resolved: u64,
    /// Local reads answered by the name search instead.
    pub fell_back: u64,
}

#[cfg(debug_assertions)]
impl ResolutionCounts {
    /// The share of local reads that took the resolved path, or `None` when
    /// there were no local reads to take it.
    pub fn resolved_fraction(&self) -> Option<f64> {
        let total = self.resolved + self.fell_back;
        (total > 0).then(|| self.resolved as f64 / total as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cove_diag::SourceMap;
    use cove_sema::config::Config;
    use cove_sema::package::{Module, Package, Unit};
    use cove_syntax::ast::{Item, SourceUnit};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// A package of one module holding `source`, with the file it was parsed
    /// from kept so a test can read spans back out of it.
    struct Resolved {
        sources: SourceMap,
        unit: SourceUnit,
        bindings: Bindings,
    }

    impl Resolved {
        fn of(source: &str) -> Resolved {
            Resolved::built(source, |_| {})
        }

        /// The same, with a chance to edit the tree before it is resolved,
        /// for a shape the parser does not write today.
        fn built(source: &str, edit: impl FnOnce(&mut SourceUnit)) -> Resolved {
            let mut sources = SourceMap::new();
            let path = PathBuf::from("test/main.cove");
            let file = sources.add(path.clone(), source);
            let mut ast = cove_syntax::parse_file(&sources, file).expect("test source parses");
            edit(&mut ast);
            let mut modules = BTreeMap::new();
            modules.insert(
                "test".to_string(),
                Module {
                    name: "test".to_string(),
                    dir: PathBuf::from("test"),
                    units: vec![Unit {
                        file,
                        path,
                        ast: ast.clone(),
                    }],
                },
            );
            let package = Package {
                root: PathBuf::new(),
                config: Config::default(),
                modules,
            };
            let program = cove_sema::resolve::resolve(&package).expect("test source resolves");
            Resolved {
                sources,
                unit: ast,
                bindings: Bindings::of(&program),
            }
        }

        /// What every reference written as `name` resolved to, in source
        /// order: `Some(index)` for a resolved one and `None` otherwise.
        fn of_name(&self, name: &str) -> Vec<Option<u32>> {
            let mut found = Vec::new();
            for expr in idents(&self.unit) {
                if self.text(expr.span) == name {
                    found.push(self.bindings.frame_index(expr.span, expr.id));
                }
            }
            found
        }

        fn text(&self, span: cove_diag::Span) -> &str {
            &self.sources.get(span.file).text[span.start as usize..span.end as usize]
        }
    }

    /// Every identifier expression of a unit, in source order.
    ///
    /// Written as its own walk rather than reusing the resolver's, so that a
    /// reference the resolver never visits is still found here and shows up
    /// as unresolved.
    fn idents(unit: &SourceUnit) -> Vec<&Expr> {
        let mut found = Vec::new();
        for item in &unit.items {
            item_idents(item, &mut found);
        }
        found
    }

    fn item_idents<'a>(item: &'a Item, found: &mut Vec<&'a Expr>) {
        match &item.kind {
            ItemKind::Fn(decl) => {
                param_idents(&decl.params, found);
                block_idents(&decl.body, found);
            }
            ItemKind::Struct(_) | ItemKind::Enum(_) | ItemKind::TypeAlias(_) => {}
            ItemKind::Trait(decl) => {
                for method in &decl.methods {
                    param_idents(&method.params, found);
                    if let Some(body) = &method.default {
                        block_idents(body, found);
                    }
                }
            }
            ItemKind::Impl(block) => {
                for item in &block.items {
                    item_idents(item, found);
                }
            }
        }
    }

    fn param_idents<'a>(params: &'a [Param], found: &mut Vec<&'a Expr>) {
        for param in params {
            if let Some(default) = &param.default {
                expr_idents(default, found);
            }
        }
    }

    fn block_idents<'a>(block: &'a Block, found: &mut Vec<&'a Expr>) {
        for stmt in &block.statements {
            match &stmt.kind {
                StmtKind::Let { value, .. } => expr_idents(value, found),
                StmtKind::Expr(value) => expr_idents(value, found),
                StmtKind::Item(item) => item_idents(item, found),
            }
        }
        if let Some(tail) = &block.tail {
            expr_idents(tail, found);
        }
    }

    fn pattern_idents<'a>(pattern: &'a Pattern, found: &mut Vec<&'a Expr>) {
        match &pattern.kind {
            PatternKind::Wildcard | PatternKind::Binding(_) => {}
            PatternKind::Literal(value) => expr_idents(value, found),
            PatternKind::Variant { path: _, payload } => {
                for pattern in payload {
                    pattern_idents(pattern, found);
                }
            }
        }
    }

    fn expr_idents<'a>(expr: &'a Expr, found: &mut Vec<&'a Expr>) {
        match &expr.kind {
            ExprKind::Ident(_) => found.push(expr),
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Duration(_)
            | ExprKind::Unit
            | ExprKind::Continue => {}
            ExprKind::Str(parts) => {
                for part in parts {
                    if let StrPart::Interpolation(inner) = part {
                        expr_idents(inner, found);
                    }
                }
            }
            ExprKind::ArrayLit(elements) => {
                for element in elements {
                    expr_idents(element, found);
                }
            }
            ExprKind::Field { base, .. } => expr_idents(base, found),
            ExprKind::Call {
                callee,
                args,
                trailing,
                ..
            } => {
                expr_idents(callee, found);
                for arg in args {
                    expr_idents(&arg.value, found);
                }
                if let Some(trailing) = trailing {
                    expr_idents(trailing, found);
                }
            }
            ExprKind::Unary { operand, .. } => expr_idents(operand, found),
            ExprKind::Binary { lhs, rhs, .. } => {
                expr_idents(lhs, found);
                expr_idents(rhs, found);
            }
            ExprKind::Assign { target, value, .. } => {
                expr_idents(target, found);
                expr_idents(value, found);
            }
            ExprKind::Try(inner) | ExprKind::Await(inner) => expr_idents(inner, found),
            ExprKind::Block(block) => block_idents(block, found),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                expr_idents(condition, found);
                block_idents(then_branch, found);
                if let Some(else_branch) = else_branch {
                    expr_idents(else_branch, found);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                expr_idents(scrutinee, found);
                for arm in arms {
                    pattern_idents(&arm.pattern, found);
                    expr_idents(&arm.body, found);
                }
            }
            ExprKind::For { iterable, body, .. } => {
                expr_idents(iterable, found);
                block_idents(body, found);
            }
            ExprKind::While { condition, body } => {
                expr_idents(condition, found);
                block_idents(body, found);
            }
            ExprKind::Return(value) | ExprKind::Break(value) => {
                if let Some(value) = value {
                    expr_idents(value, found);
                }
            }
            ExprKind::Lambda { params, body, .. } => {
                param_idents(params, found);
                block_idents(body, found);
            }
            ExprKind::Scope { body, .. } => block_idents(body, found),
            ExprKind::Range { start, end, .. } => {
                expr_idents(start, found);
                expr_idents(end, found);
            }
        }
    }

    #[test]
    fn a_parameter_of_a_free_function_is_index_zero() {
        let resolved = Resolved::of("export fn main(a: Int, b: Int) -> Int {\n  a + b\n}\n");
        assert_eq!(resolved.of_name("a"), vec![Some(0)]);
        assert_eq!(resolved.of_name("b"), vec![Some(1)]);
    }

    /// Hazard: a method's frame starts with `self`, so its first parameter
    /// is one further along than a free function's.
    #[test]
    fn a_method_binds_self_at_index_zero_and_its_first_parameter_at_one() {
        let resolved = Resolved::of(
            "export struct Counter {\n  count: Int\n}\n\nimpl Counter {\n  export fn plus(self, step: Int) -> Int {\n    self.count + step\n  }\n}\n",
        );
        assert_eq!(resolved.of_name("self"), vec![Some(0)]);
        assert_eq!(resolved.of_name("step"), vec![Some(1)]);
    }

    /// Hazard: an associated function has no receiver, so its parameters
    /// start where a method's `self` would have been.
    #[test]
    fn an_associated_function_has_no_receiver_to_make_room_for() {
        let resolved = Resolved::of(
            "export struct Counter {\n  count: Int\n}\n\nimpl Counter {\n  export fn of(start: Int) -> Counter {\n    Counter(count: start)\n  }\n}\n",
        );
        assert_eq!(resolved.of_name("start"), vec![Some(0)]);
    }

    #[test]
    fn a_local_takes_the_index_after_the_parameters() {
        let resolved = Resolved::of(
            "export fn main(a: Int) -> Int {\n  let doubled = a * 2\n  doubled + a\n}\n",
        );
        assert_eq!(resolved.of_name("doubled"), vec![Some(1)]);
    }

    /// Hazard: `let x = 1` and `let x = 2` are two cells, so they are two
    /// indices, and the value expression of the second sees the first.
    #[test]
    fn shadowing_in_one_block_makes_a_second_index() {
        let resolved =
            Resolved::of("export fn main() -> Int {\n  let x = 1\n  let x = x + 1\n  x\n}\n");
        // The `x` inside the second `let`, then the `x` that reads it.
        assert_eq!(resolved.of_name("x"), vec![Some(0), Some(1)]);
    }

    /// Hazard: a block's indices are handed back when it ends, so what the
    /// enclosing scope declares next reuses them.
    #[test]
    fn a_block_hands_its_indices_back_when_it_ends() {
        let resolved = Resolved::of(
            "export fn main() -> Int {\n  {\n    let inner = 1\n    inner\n  }\n  let after = 2\n  after\n}\n",
        );
        assert_eq!(resolved.of_name("inner"), vec![Some(0)]);
        assert_eq!(resolved.of_name("after"), vec![Some(0)]);
    }

    /// Hazard: a local `fn` is a closure built before its name is declared,
    /// so its body cannot see itself. Resolving the name it is bound to
    /// would make it recursive.
    #[test]
    fn a_local_fn_does_not_see_itself() {
        let resolved = Resolved::of(
            "export fn main() -> Int {\n  fn step(n: Int) -> Int {\n    step(n)\n  }\n  step(1)\n}\n",
        );
        // The recursive call is unresolved; the call after the declaration
        // resolves to the closure's own cell.
        assert_eq!(resolved.of_name("step"), vec![None, Some(0)]);
        assert_eq!(resolved.of_name("n"), vec![Some(0)]);
    }

    /// Hazard: a lambda is its own frame. Its parameters are its own first
    /// bindings, and a name it did not declare is a capture, which does not
    /// live in the frame at all.
    #[test]
    fn a_lambda_resolves_its_parameters_and_not_its_captures() {
        let resolved = Resolved::of(
            "export fn main() -> Int {\n  let base = 1\n  let add = fn(n: Int) {\n    n + base\n  }\n  add(2)\n}\n",
        );
        assert_eq!(resolved.of_name("n"), vec![Some(0)]);
        assert_eq!(resolved.of_name("base"), vec![None]);
        assert_eq!(resolved.of_name("add"), vec![Some(1)]);
    }

    /// A trailing block is a closure too, with no parameters, so its own
    /// locals start at index zero and the enclosing frame's names are
    /// captures.
    #[test]
    fn a_trailing_closure_is_a_frame_of_its_own() {
        let resolved = Resolved::of(
            "use console.println\n\nexport fn main() {\n  let outer = 1\n  println(\"x\") {\n    let inner = 2\n    inner + outer\n  }\n}\n",
        );
        assert_eq!(resolved.of_name("inner"), vec![Some(0)]);
        assert_eq!(resolved.of_name("outer"), vec![None]);
    }

    /// The arms of a `match` each open a scope of their own, so what one
    /// binds is gone before the next is tried.
    #[test]
    fn each_match_arm_declares_over_the_last() {
        let resolved = Resolved::of(
            "export fn main(value: Option<Int>) -> Int {\n  match value {\n    Some(n) => n\n    other => other\n  }\n}\n",
        );
        assert_eq!(resolved.of_name("value"), vec![Some(0)]);
        assert_eq!(resolved.of_name("n"), vec![Some(1)]);
        assert_eq!(resolved.of_name("other"), vec![Some(1)]);
    }

    /// Hazard: a `None` *binder* binds, or matches the `Option` case and
    /// binds nothing, depending on the value the arm is tried against. Every
    /// index after it in that arm is therefore a run-time fact, and none of
    /// them is claimed — while the arm beside it, which binds no `None`,
    /// resolves as ever.
    ///
    /// The parser writes `None` as a variant pattern with no payload, since
    /// it begins with a capital, so this shape is built by hand: the rule
    /// belongs to the interpreter, which reads a `Binding` of that name as a
    /// case or as a name depending on the value, and a resolver that only
    /// held while the parser happened not to produce one would be a trap for
    /// whoever changes the parser.
    #[test]
    fn an_arm_binding_none_resolves_nothing_inside_itself() {
        let source = "export fn main(value: Option<Int>, fallback: Int) -> Int {\n  match value {\n    Some(n) => fallback\n    other => fallback\n  }\n}\n";

        // As written, both arms resolve `fallback` to the parameter.
        let plain = Resolved::of(source);
        assert_eq!(plain.of_name("fallback"), vec![Some(1), Some(1)]);

        // With the second arm's binder renamed to `None`, that arm claims
        // nothing and the first is untouched.
        let edited = Resolved::built(source, |unit| {
            second_arm(unit).pattern.kind = PatternKind::Binding(NONE_CASE.name.to_string());
        });
        assert_eq!(edited.of_name("fallback"), vec![Some(1), None]);
    }

    /// A `None` binder anywhere in an arm's pattern stops the whole arm, not
    /// just what follows it.
    ///
    /// Built by hand and checked against the rule directly, because a
    /// `match` on a payload that also binds its case twice is not a program
    /// the resolver accepts.
    #[test]
    fn a_none_binder_inside_a_variant_pattern_still_stops_its_arm() {
        let span = Span::new(FileId(0), 0, 0);
        let case = |payload: &str| Pattern {
            kind: PatternKind::Variant {
                path: vec![cove_diag::Spanned::new("Some".to_string(), span)],
                payload: vec![Pattern {
                    kind: PatternKind::Binding(payload.to_string()),
                    span,
                }],
            },
            span,
        };
        assert!(binds_none(&case(NONE_CASE.name)));
        assert!(!binds_none(&case("n")));
    }

    /// The second arm of the `match` that is `main`'s whole body.
    fn second_arm(unit: &mut SourceUnit) -> &mut cove_syntax::ast::MatchArm {
        let ItemKind::Fn(decl) = &mut unit.items[0].kind else {
            panic!("the test source declares one function");
        };
        let tail = decl.body.tail.as_mut().expect("its body is one expression");
        let ExprKind::Match { arms, .. } = &mut tail.kind else {
            panic!("that expression is a `match`");
        };
        &mut arms[1]
    }

    /// Hazard: a `for` binding is a fresh cell per iteration, declared in a
    /// scope of its own before the body's block opens.
    #[test]
    fn a_for_binding_sits_between_the_enclosing_scope_and_the_body() {
        let resolved = Resolved::of(
            "export fn main() -> Int {\n  let total = 0\n  for item in 0..<3 {\n    let seen = item\n    total + seen\n  }\n  total\n}\n",
        );
        assert_eq!(resolved.of_name("item"), vec![Some(1)]);
        assert_eq!(resolved.of_name("seen"), vec![Some(2)]);
        assert_eq!(resolved.of_name("total"), vec![Some(0), Some(0)]);
    }

    #[test]
    fn a_scope_name_is_a_binding_like_any_other() {
        let resolved = Resolved::of(
            "export fn main() -> Int {\n  let before = 1\n  scope work {\n    let inside = before\n    inside\n  }\n}\n",
        );
        assert_eq!(resolved.of_name("inside"), vec![Some(2)]);
        assert_eq!(resolved.of_name("before"), vec![Some(0)]);
    }

    /// A parameter default is evaluated by the callee, in the frame it is
    /// declared into, so it reads the parameters to its left.
    #[test]
    fn a_parameter_default_reads_the_parameters_before_it() {
        let resolved =
            Resolved::of("export fn main(a: Int = 1, b: Int = a) -> Int {\n  a + b\n}\n");
        assert_eq!(resolved.of_name("a"), vec![Some(0), Some(0)]);
    }

    /// Hazard: a host module is shadowed by a local of the same name only
    /// from the point the local is declared, so a call written before it
    /// must stay unresolved and reach the host.
    #[test]
    fn a_host_name_used_before_a_local_shadows_it_is_unresolved() {
        let resolved = Resolved::of(
            "export fn main() {\n  console.println(\"a\")\n  let console = 1\n  console\n}\n",
        );
        assert_eq!(resolved.of_name("console"), vec![None, Some(0)]);
    }

    #[test]
    fn an_id_from_another_file_resolves_to_nothing() {
        let resolved = Resolved::of("export fn main(a: Int) -> Int {\n  a\n}\n");
        let elsewhere = Span::new(FileId(9), 0, 1);
        assert_eq!(resolved.bindings.frame_index(elsewhere, ExprId(0)), None);
    }

    #[test]
    fn an_unnumbered_expression_resolves_to_nothing() {
        let resolved = Resolved::of("export fn main(a: Int) -> Int {\n  a\n}\n");
        let span = idents(&resolved.unit)[0].span;
        assert_eq!(resolved.bindings.frame_index(span, ExprId::UNSET), None);
    }

    /// Two answers for one id cannot both be kept, and keeping either would
    /// be a guess about which body is running.
    #[test]
    fn a_second_disagreeing_answer_poisons_the_first() {
        let mut bindings = Bindings::default();
        let span = Span::new(FileId(0), 0, 1);
        bindings.record(span.file, ExprId(0), Slot::At(3));
        assert_eq!(bindings.frame_index(span, ExprId(0)), Some(3));
        bindings.record(span.file, ExprId(0), Slot::At(4));
        assert_eq!(bindings.frame_index(span, ExprId(0)), None);
        bindings.record(span.file, ExprId(0), Slot::At(3));
        assert_eq!(bindings.frame_index(span, ExprId(0)), None);
    }
}
