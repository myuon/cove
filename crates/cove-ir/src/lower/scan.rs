//! The walk over a body's source that happens before it emits anything.
//!
//! [`mentioned_names`] answers a question about a *whole* body that the
//! statement in the middle of it cannot answer for itself: what a lambda
//! captures, which is `crate::interp`'s `mention_block` read at lowering
//! time.
//!
//! It over-approximates on purpose and says why, and it is written out over
//! every `ExprKind` rather than defaulting, because a form it forgot to walk
//! is not a compile error: it is a capture that goes missing.
//!
//! There was a second walk here until issue #162, and what it did is worth
//! recording as gone. `var_argument_roots` collected every name a body used
//! as the root of a `var` argument, because a place could only address the
//! value stack and so a binding one was rooted at had to be kept there even
//! where the checker had settled it as `Int`. `Inst::PlaceScalar` is what
//! removed the need for it: a place names a slot in whichever region the
//! slot lives in, so nothing has to move to be named.

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
