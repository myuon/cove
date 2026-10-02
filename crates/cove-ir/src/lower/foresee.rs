//! The standard-library functions a slice's bodies will call without naming
//! them, read off the checked source before anything is lowered.
//!
//! [`super::lower_roots`] seeds its slice with the checker's call graph, and
//! the graph has no edge for a call nothing *wrote*: `xs.push(x)` on a
//! `Vector` is a call to `std.vector.push` only because
//! [`cove_schema::builtins::standard_binding`] says so, and an `Int` piece of
//! an interpolation is a call to `std.int.renderInto` only because
//! `interpolate` chooses it. The slice used to learn each of those by
//! lowering the whole package, finding the call it had to leave out, and
//! lowering the whole package again — three times over for a program such as
//! covefmt, whose second round wanted `std.string.length` and whose third
//! wanted what *that* calls.
//!
//! This asks the same questions those places ask, of the same facts, before
//! the first round: for every body the seed holds, the methods of builtin
//! receivers that are bindings, the associated functions of builtin types
//! that are bindings, and the appends and the `Int` rendering an
//! interpolation makes; and then the same of every body it found, together
//! with the calls those bodies write, which the graph does have.
//!
//! # A guess, and what makes it safe to guess
//!
//! Nothing here decides what the slice is. It is asked to *find* more of it
//! sooner, and the lowering still closes the slice against what it emits:
//! [`super::lower_roots`] keeps going while a round wants anything, and then
//! checks that every declaration the guess added was asked for by a body the
//! seed reaches — so a guess that was too wide is thrown away, not lowered,
//! and the program is the one the rounds without it would have produced. A
//! question this does not ask — a method of a type parameter that becomes a
//! `String` in one instantiation, a `Float` piece, a `dyn` dispatch — is left
//! to the rounds, exactly as before.

use std::collections::HashSet;

use cove_schema::builtins::FreeBuiltinKind;
use cove_sema::resolve::{CallPrecision, FnKey, Program as Checked};
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Block, Expr, ExprKind, StmtKind, StrPart};

use super::dispatch::{DURATION_TEXT, DYNAMIC_RENDER, FLOAT_TEXT};
use super::interpolate::{BYTE_APPEND, INT_RENDERING, TEXT_APPEND};
use super::{collections, methods, Plan};
use crate::FunctionId;

/// The declarations the bodies of `from`, and the bodies of what this finds,
/// will call on their own account; none of them is in `known`.
///
/// `from` is either the seed the call graph closed, whose own calls by name
/// are already in it, or what a round found missing, whose calls by name are
/// not: `follow` says to read those from the graph too.
pub(super) fn library_calls(
    checked: &Checked,
    plan: &Plan<'_>,
    from: &HashSet<FunctionId>,
    known: &HashSet<FunctionId>,
    follow: bool,
) -> HashSet<FunctionId> {
    let mut found: HashSet<FunctionId> = HashSet::new();
    let mut pending: Vec<FunctionId> = from.iter().copied().collect();
    pending.sort_unstable();
    let mut scanned: HashSet<FunctionId> = HashSet::new();
    while let Some(id) = pending.pop() {
        if !scanned.insert(id) {
            continue;
        }
        let mut calls = Vec::new();
        let decl = plan.decls[id.index()].decl;
        let mut scan = Scan {
            checked,
            plan,
            module: &plan.decls[id.index()].module,
            calls: &mut calls,
        };
        for param in &decl.params {
            if let Some(default) = &param.default {
                scan.expr(default);
            }
        }
        scan.block(&decl.body);
        // What a declaration this found calls by name: the graph's precise
        // edges, which is all a library body's own calls are. The
        // approximate ones are a guess about a receiver's type, and the
        // scan above already read the type.
        if follow || found.contains(&id) {
            let held = &plan.decls[id.index()];
            let node = match held.name.split_once('.') {
                Some((type_name, method)) => (
                    held.module.to_string(),
                    FnKey::Method(type_name.to_string(), method.to_string()),
                ),
                None => (held.module.to_string(), FnKey::Fn(held.name.to_string())),
            };
            if let Some(edges) = checked.call_graph.get(&node) {
                for (callee, precision) in edges {
                    if *precision == CallPrecision::Exact {
                        if let Some(callee) = plan.id_of(callee) {
                            calls.push(Callee::Id(callee));
                        }
                    }
                }
            }
        }
        for callee in calls {
            let callee = match callee {
                Callee::Id(callee) => Some(callee),
                Callee::Named(module, function) => plan.resolve(checked, module, function),
            };
            let Some(callee) = callee else {
                continue;
            };
            if known.contains(&callee) || from.contains(&callee) || !found.insert(callee) {
                continue;
            }
            pending.push(callee);
        }
    }
    found
}

/// A call one body makes: a declaration the graph named, or a library
/// function by module and name.
enum Callee {
    Id(FunctionId),
    Named(&'static str, &'static str),
}

struct Scan<'a> {
    checked: &'a Checked,
    plan: &'a Plan<'a>,
    /// The module the body is written in.
    module: &'a str,
    calls: &'a mut Vec<Callee>,
}

impl Scan<'_> {
    fn ty(&self, expr: &Expr) -> Option<&Ty> {
        self.checked.facts.ty(expr.span.file, expr.id)
    }

    fn block(&mut self, block: &Block) {
        for stmt in &block.statements {
            match &stmt.kind {
                StmtKind::Let { value, .. } => self.expr(value),
                StmtKind::Expr(expr) => self.expr(expr),
                // A nested declaration is a declaration of its own, and the
                // rounds find what it calls.
                StmtKind::Item(_) => {}
            }
        }
        if let Some(tail) = &block.tail {
            self.expr(tail);
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Duration(_)
            | ExprKind::Unit
            | ExprKind::Ident(_)
            | ExprKind::Continue => {}
            ExprKind::Str(parts) => self.string(parts),
            ExprKind::ArrayLit(items) => items.iter().for_each(|item| self.expr(item)),
            ExprKind::Field { base, .. } => self.expr(base),
            ExprKind::Call {
                callee,
                args,
                trailing,
                ..
            } => {
                self.call(expr, callee, args);
                self.expr(callee);
                for arg in args {
                    self.expr(&arg.value);
                }
                if let Some(trailing) = trailing {
                    self.expr(trailing);
                }
            }
            ExprKind::Unary { operand, .. } => self.expr(operand),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Assign { target, value, .. } => {
                self.expr(target);
                self.expr(value);
            }
            ExprKind::Try(inner) | ExprKind::Await(inner) => self.expr(inner),
            ExprKind::Block(block) => self.block(block),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.expr(condition);
                self.block(then_branch);
                if let Some(otherwise) = else_branch {
                    self.expr(otherwise);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    self.expr(&arm.body);
                }
            }
            ExprKind::For { iterable, body, .. } => {
                self.expr(iterable);
                self.block(body);
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
            ExprKind::Lambda { params, body, .. } => {
                for param in params {
                    if let Some(default) = &param.default {
                        self.expr(default);
                    }
                }
                self.block(body);
            }
            ExprKind::Scope { body, .. } => self.block(body),
            ExprKind::Range { start, end, .. } => {
                self.expr(start);
                self.expr(end);
            }
        }
    }

    /// `base.name(...)`, asked the questions `Body::call_through` asks, in
    /// its order, as far as a binding.
    fn call(&mut self, expr: &Expr, callee: &Expr, args: &[Arg]) {
        if matches!(self.ty(callee), Some(Ty::Fn(_))) {
            return;
        }
        let (base, name) = match &callee.kind {
            ExprKind::Field { base, name } => (base, name.node.as_str()),
            ExprKind::Ident(name) => return self.assertion(name, args),
            _ => return,
        };
        // A declared method, which the graph has only when the receiver's
        // module is one the caller can see: a method of a type the standard
        // library declares, such as `StringBuilder.appendByte`, is not.
        if let Some(target) = self.checked.facts.target(expr.span.file, expr.id) {
            if let Some(id) = self.plan.method(target) {
                self.calls.push(Callee::Id(id));
            }
            return;
        }
        match self.ty(base) {
            // A name that is not a value: `Duration.millis(n)`.
            None => {
                let ExprKind::Ident(head) = &base.kind else {
                    return;
                };
                let Some(ty) = self.ty(expr) else {
                    return;
                };
                // `Vector.of(1, 2)` builds the collection in place, and so
                // does an empty `Set.of()` or `Map.of()`; any other is the
                // binding below. `Body::keyed_of`.
                if name == "of"
                    && collections::namespace_of(head, ty)
                    && (head == "Vector" || args.is_empty())
                {
                    return;
                }
                if !methods::associated(head, name, ty) {
                    return;
                }
                if let Some(binding) =
                    cove_schema::builtins::standard_associated_binding(head, name)
                {
                    self.calls
                        .push(Callee::Named(binding.module, binding.function));
                }
            }
            Some(Ty::Dyn(_) | Ty::Param(_)) => {}
            Some(ty) => {
                if name == "snapshot" && args.is_empty() {
                    return;
                }
                let Some(receiver) = methods::receiver_name(ty) else {
                    return;
                };
                if let Some(binding) = cove_schema::builtins::standard_binding(receiver, name) {
                    self.calls
                        .push(Callee::Named(binding.module, binding.function));
                }
            }
        }
    }

    /// `assertEqual(found, expected)`, whose failure message is an assembly
    /// as an interpolation is: `Body::assertion_message`.
    fn assertion(&mut self, name: &str, args: &[Arg]) {
        if self.plan.resolve(self.checked, self.module, name).is_some() {
            return;
        }
        let Some(schema) = cove_schema::builtins::free_builtin(name) else {
            return;
        };
        if schema.kind != FreeBuiltinKind::Assertion || schema.arity() != 2 || args.len() != 2 {
            return;
        }
        // The opening and the middle are longer than a byte, and the closing
        // backquote is one.
        self.literal("assertion failed: `");
        self.literal("`");
        for arg in args {
            self.piece(&arg.value);
        }
    }

    /// An interpolation's appends, as `Body::string_expr` and
    /// `Body::append_piece` make them; a literal with no piece is a constant
    /// and calls nothing.
    fn string(&mut self, parts: &[StrPart]) {
        let literal_only = parts.iter().all(|part| matches!(part, StrPart::Text(_)));
        for part in parts {
            match part {
                StrPart::Text(text) if !literal_only => self.literal(text),
                StrPart::Text(_) => {}
                StrPart::Interpolation(inner) => {
                    self.piece(inner);
                    self.expr(inner);
                }
            }
        }
    }

    /// `Body::append_literal`'s call.
    fn literal(&mut self, text: &str) {
        match text.len() {
            0 => {}
            1 => self.named(BYTE_APPEND),
            _ => self.named(TEXT_APPEND),
        }
    }

    /// `Body::append_piece`'s call, for the types whose call is one function
    /// of the standard library; a walk or a box is left to the rounds.
    fn piece(&mut self, piece: &Expr) {
        match self.ty(piece) {
            Some(Ty::Str) => self.named(TEXT_APPEND),
            Some(Ty::Int) => self.named(INT_RENDERING),
            Some(Ty::Float) => self.named(FLOAT_TEXT),
            Some(Ty::Duration) => self.named(DURATION_TEXT),
            // A box: `Body::render_erased`.
            Some(Ty::Any | Ty::Dyn(_)) => self.named(DYNAMIC_RENDER),
            // A type parameter is whatever an instantiation makes it.
            None | Some(Ty::Param(_)) => {}
            // Anything else is a walk, and a walk asks for its three leaves
            // before it is composed: `Body::render_leaves`. What else the
            // walk reaches — a float, a box — is left to the rounds.
            Some(_) => {
                self.named(TEXT_APPEND);
                self.named(BYTE_APPEND);
                self.named(INT_RENDERING);
            }
        }
    }

    fn named(&mut self, (module, function): (&'static str, &'static str)) {
        self.calls.push(Callee::Named(module, function));
    }
}
