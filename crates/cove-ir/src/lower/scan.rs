//! The two walks over a body's source that happen before it emits anything.
//!
//! Each answers a question about a *whole* body that the statement in the
//! middle of it cannot answer for itself. [`mentioned_names`] is what a
//! lambda captures, which is `crate::interp`'s `mention_block` read at
//! lowering time; [`var_argument_roots`] is which names a place is rooted
//! at, which decides where a binding lives and is settled by a `bump(var
//! total)` written after the `var total = 0` it is about.
//!
//! Both over-approximate on purpose and each says why. Both are written out
//! over every `ExprKind` rather than defaulting, because a form either
//! forgot to walk is not a compile error: it is a capture that goes missing,
//! or a binding left on the stack a place cannot address.

use std::collections::BTreeSet;

use cove_syntax::ast::{
    Block, Expr, ExprKind, FnDecl, ItemKind, Param, Pattern, PatternKind, StmtKind, StrPart,
};

/// Every name a lambda's body can read from the environment around it.
///
/// This is `crate::interp`'s `mention_block`, and it has to stay that
/// function: what a closure captures is what its body mentions intersected
/// with what is live, and a set that differed from the interpreter's would
/// be a closure holding a different list. The set over-approximates on
/// purpose — a name the body binds for itself is listed too — because
/// capturing a name the body never reads costs one value and missing one
/// leaves the body unable to resolve it. Over-approximating here is *also*
/// harmless in a second way the interpreter does not need: a mentioned name
/// that no binding answers is simply not a capture, since only bindings are
/// walked.
///
/// The borrows are the source's, so the set is `&str` where the
/// interpreter's is `String`. Nothing else differs.
pub(super) fn mentioned_names(block: &Block) -> BTreeSet<&str> {
    let mut found = BTreeSet::new();
    mention_block(block, &mut found);
    found
}

fn mention_block<'a>(block: &'a Block, out: &mut BTreeSet<&'a str>) {
    for stmt in &block.statements {
        match &stmt.kind {
            StmtKind::Let { value, .. } => mention_expr(value, out),
            StmtKind::Expr(expr) => mention_expr(expr, out),
            StmtKind::Item(item) => match &item.kind {
                ItemKind::Fn(decl) => mention_fn(decl, out),
                ItemKind::Impl(block) => {
                    for item in &block.items {
                        if let ItemKind::Fn(decl) = &item.kind {
                            mention_fn(decl, out);
                        }
                    }
                }
                // A trait's default bodies are reached through the
                // conformances resolution recorded them under, not through
                // this closure's environment.
                ItemKind::Struct(_)
                | ItemKind::Enum(_)
                | ItemKind::Trait(_)
                | ItemKind::TypeAlias(_) => {}
            },
        }
    }
    if let Some(tail) = &block.tail {
        mention_expr(tail, out);
    }
}

fn mention_fn<'a>(decl: &'a FnDecl, out: &mut BTreeSet<&'a str>) {
    mention_params(&decl.params, out);
    mention_block(&decl.body, out);
}

/// A default argument is evaluated by the callee, so the names it reads
/// belong to the body.
fn mention_params<'a>(params: &'a [Param], out: &mut BTreeSet<&'a str>) {
    for param in params {
        if let Some(default) = &param.default {
            mention_expr(default, out);
        }
    }
}

fn mention_expr<'a>(expr: &'a Expr, out: &mut BTreeSet<&'a str>) {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Duration(_)
        | ExprKind::Unit => {}
        ExprKind::Str(parts) => {
            for part in parts {
                if let StrPart::Interpolation(inner) = part {
                    mention_expr(inner, out);
                }
            }
        }
        ExprKind::Ident(name) => {
            out.insert(name.as_str());
        }
        ExprKind::ArrayLit(items) => {
            for item in items {
                mention_expr(item, out);
            }
        }
        // A field name is not a binding; only the base can read one.
        ExprKind::Field { base, .. } => mention_expr(base, out),
        ExprKind::Call {
            callee,
            args,
            trailing,
            ..
        } => {
            mention_expr(callee, out);
            for arg in args {
                mention_expr(&arg.value, out);
            }
            if let Some(trailing) = trailing {
                mention_expr(trailing, out);
            }
        }
        ExprKind::Unary { operand, .. } => mention_expr(operand, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            mention_expr(lhs, out);
            mention_expr(rhs, out);
        }
        ExprKind::Assign { target, value, .. } => {
            mention_expr(target, out);
            mention_expr(value, out);
        }
        ExprKind::Try(inner) | ExprKind::Await(inner) => mention_expr(inner, out),
        ExprKind::Block(block) => mention_block(block, out),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            mention_expr(condition, out);
            mention_block(then_branch, out);
            if let Some(branch) = else_branch {
                mention_expr(branch, out);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            mention_expr(scrutinee, out);
            for arm in arms {
                mention_pattern(&arm.pattern, out);
                mention_expr(&arm.body, out);
            }
        }
        ExprKind::For { iterable, body, .. } => {
            mention_expr(iterable, out);
            mention_block(body, out);
        }
        ExprKind::While { condition, body } => {
            mention_expr(condition, out);
            mention_block(body, out);
        }
        ExprKind::Return(inner) | ExprKind::Break(inner) => {
            if let Some(inner) = inner {
                mention_expr(inner, out);
            }
        }
        ExprKind::Continue => {}
        ExprKind::Lambda { params, body, .. } => {
            mention_params(params, out);
            mention_block(body, out);
        }
        // The scope name is bound by the `scope`, so it shadows anything the
        // surrounding environment holds under that name.
        ExprKind::Scope { body, .. } => mention_block(body, out),
        ExprKind::Range { start, end, .. } => {
            mention_expr(start, out);
            mention_expr(end, out);
        }
    }
}

/// Pattern bindings are binders, so only a literal pattern reads a name.
fn mention_pattern<'a>(pattern: &'a Pattern, out: &mut BTreeSet<&'a str>) {
    match &pattern.kind {
        PatternKind::Wildcard | PatternKind::Binding(_) => {}
        PatternKind::Literal(expr) => mention_expr(expr, out),
        PatternKind::Variant { payload, .. } => {
            for sub in payload {
                mention_pattern(sub, out);
            }
        }
    }
}

/// Every name a body uses as the root of a place, collected before anything
/// is lowered.
///
/// A place is an index into the *value* stack, so a binding a place can be
/// rooted at has to live there — which is a fact about the whole body and
/// not about the statement that declares the binding, since
/// `bump(var total)` is written after `var total = 0` and decides where
/// `total` lives. So the body is walked once first, and
/// [`Body::rooted`] is what the walk found.
///
/// Two forms root a place: a `var` argument, whose root is the name at the
/// bottom of the `a.b.c` it is written as, and the receiver of `freeze`,
/// which is the one builtin that needs the storage handle where it lies
/// rather than a read of it. A `var self` receiver roots one too and is not
/// collected here, because a method that declares one is declared on a
/// struct or an enum and a binding of such a type is a value slot already;
/// `Body::place` refuses rather than guessing if that ever stops being true.
///
/// The answer is a set of *names*, which over-approximates across shadowing
/// on purpose — see [`Body::rooted`] for why that is free.
///
/// [`Body::rooted`]: crate::lower::body::Body::rooted
pub(super) fn var_argument_roots(body: &Block) -> BTreeSet<&str> {
    let mut found = BTreeSet::new();
    walk_block(body, &mut found);
    found
}

/// The name at the bottom of a place expression, or `None` where the
/// expression does not name one.
///
/// `a`, `a.b`, and `a.b.c` all root at `a`, which is `Place::field` carrying
/// its base's identity down unchanged, read here as a question about syntax.
fn place_root(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Ident(name) => Some(name),
        ExprKind::Field { base, .. } => place_root(base),
        _ => None,
    }
}

fn walk_block<'a>(block: &'a Block, found: &mut BTreeSet<&'a str>) {
    for statement in &block.statements {
        match &statement.kind {
            StmtKind::Let { value, .. } => walk_expr(value, found),
            StmtKind::Expr(expr) => walk_expr(expr, found),
            StmtKind::Item(_) => {}
        }
    }
    if let Some(tail) = &block.tail {
        walk_expr(tail, found);
    }
}

/// Walks every expression a body holds, recording the roots as it goes.
///
/// Exhaustive over [`ExprKind`] rather than defaulting, because a form this
/// forgot to walk would be a `var` argument the pre-pass did not see and a
/// binding left on the scalar stack that a place then could not address.
/// `Body::place` would refuse such a program rather than mislower it, but a
/// refusal for a construct the corpus writes is a coverage loss, so the
/// match is written out.
fn walk_expr<'a>(expr: &'a Expr, found: &mut BTreeSet<&'a str>) {
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
                    walk_expr(inner, found);
                }
            }
        }
        ExprKind::ArrayLit(items) => {
            for item in items {
                walk_expr(item, found);
            }
        }
        ExprKind::Field { base, .. } => walk_expr(base, found),
        ExprKind::Call {
            callee,
            args,
            trailing,
            ..
        } => {
            // `x.freeze()` is a call whose callee is `x.freeze`, so the
            // receiver is inside the callee rather than beside it.
            if let ExprKind::Field { base, name } = &callee.kind {
                if name.node == "freeze" {
                    if let Some(root) = place_root(base) {
                        found.insert(root);
                    }
                }
            }
            walk_expr(callee, found);
            for arg in args {
                if arg.is_var {
                    if let Some(root) = place_root(&arg.value) {
                        found.insert(root);
                    }
                }
                walk_expr(&arg.value, found);
            }
            if let Some(trailing) = trailing {
                walk_expr(trailing, found);
            }
        }
        ExprKind::Unary { operand, .. } => walk_expr(operand, found),
        ExprKind::Binary { lhs, rhs, .. } => {
            walk_expr(lhs, found);
            walk_expr(rhs, found);
        }
        ExprKind::Assign { target, value, .. } => {
            walk_expr(target, found);
            walk_expr(value, found);
        }
        ExprKind::Try(inner) | ExprKind::Await(inner) => walk_expr(inner, found),
        ExprKind::Block(block) => walk_block(block, found),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            walk_expr(condition, found);
            walk_block(then_branch, found);
            if let Some(branch) = else_branch {
                walk_expr(branch, found);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_expr(scrutinee, found);
            for arm in arms {
                walk_expr(&arm.body, found);
            }
        }
        ExprKind::For { iterable, body, .. } => {
            walk_expr(iterable, found);
            walk_block(body, found);
        }
        ExprKind::While { condition, body } => {
            walk_expr(condition, found);
            walk_block(body, found);
        }
        ExprKind::Return(value) | ExprKind::Break(value) => {
            if let Some(value) = value {
                walk_expr(value, found);
            }
        }
        ExprKind::Lambda { body, .. } | ExprKind::Scope { body, .. } => walk_block(body, found),
        ExprKind::Range { start, end, .. } => {
            walk_expr(start, found);
            walk_expr(end, found);
        }
    }
}
