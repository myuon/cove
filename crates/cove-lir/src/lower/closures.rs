//! Function values: what a lambda becomes, and what a call through one is.
//!
//! A closure is two things, and keeping them apart is most of this module.
//!
//! - The **environment** is a heap object of a [`Shape::Closure`](crate::Shape::Closure)
//!   layout. Payload word 0 is the callee's [`FunctionId`], and the captures
//!   follow it **inline, each at its own layout's width** — the same
//!   arrangement the parameters of a frame are under, in a payload instead of
//!   a frame. There is one such layout per lowered lambda, because word 0 is
//!   *that* lambda's id and the captures after it are the ones *that* body
//!   reads.
//! - The **location** holding a function value is one [`Repr::Ref`] word, and
//!   there is one layout for every signature. A reference is a reference:
//!   `fn(Int) -> Int` and `fn(String) -> Bool` are one family for exactly the
//!   reason `Array<Int>` and `Array<String>` are, and which environment a
//!   word names is a question the object's own header answers.
//!
//! # A lambda is a `Function` like any other
//!
//! It is numbered after every declaration, its parameters occupy its frame
//! from slot 0 in declaration order, and its captures follow them — which is
//! what [`crate::Capture::slot`] records, so the machine copies them into the
//! frame beside the arguments and the body reads them as ordinary slots. The
//! collector needs no second story for a capture: it is a slot of a live
//! frame, and a `Repr::Ref` slot of a live frame is a root.
//!
//! A lambda is named after the body that wrote it — `f#0`, and `f#0#0` for one
//! nested in that. `#` is not a name character, so a synthesised name cannot
//! collide with a declaration's however a module is written.
//!
//! # Captures are by value, at creation time
//!
//! The oracle pins this: `Interpreter::make_closure` reads each captured
//! binding's *value* out of the environment that made the closure, and the
//! checker binds a capture read-only so nothing can write back through one.
//! So the environment holds copies, taken by [`Inst::StoreField`] at the
//! moment the closure is built, and a later write to the binding is not seen
//! by the body.
//!
//! Which names are captured is the same over-approximation the oracle makes:
//! every name the body *mentions* that the enclosing frame binds. A name the
//! body also binds for itself is captured too and then shadowed, which costs
//! one word and cannot be wrong; missing one would leave the body unable to
//! reach a value that was in scope when it was written.
//!
//! # A declared function used as a value is the same object with no captures
//!
//! `let g = add` allocates an environment naming `add` and holding nothing.
//! The alternative — a second representation for a function value that is
//! known statically — would mean [`Inst::CallClosure`] had two things to
//! read, and every place that holds a function value would have to know which
//! of the two it had. One shape costs one allocation at the point the name is
//! read, and it is what makes `xs.map(double)` and `xs.map(fn(x) { ... })` the
//! same lowering.

use std::collections::BTreeSet;
use std::sync::Arc;

use cove_diag::Span;
use cove_sema::typeck::{FnTy, Ty};
use cove_syntax::ast::{Arg, Block, Expr, ExprKind, ItemKind, Param, Pattern, PatternKind, Stmt};
use cove_syntax::ast::{FnDecl, StmtKind, StrPart};

use super::frame::{Frame, Val};
use super::gap;
use super::shapes::{self, CLOSURE_CALLEE, CLOSURE_CAPTURES};
use super::{Body, Dest};
use crate::inst::{Inst, Len, Slot};
use crate::layout::LayoutId;
use crate::program::{Capture, Function, FunctionId};
use crate::repr::RefMap;

/// One captured binding: the name the body reads it by, where it is in the
/// frame that is making the closure, and what it is.
type Captured = (Arc<str>, Slot, LayoutId);

impl Body<'_> {
    // ---- making one -------------------------------------------------------

    /// `fn(x) { ... }`: a `Function` of its own, and an environment naming it.
    pub(super) fn lambda(
        &mut self,
        expr: &Expr,
        is_async: bool,
        params: &[Param],
        body: &Block,
    ) -> Val {
        if is_async {
            return self.gap("an `async` function value", expr);
        }
        for param in params {
            // ADR 0032 already refuses the variadic one in the checker, so a
            // valid program never brings one here; the other two are this
            // lowering's own work, and each is named as the source writes it.
            let what = if param.is_var {
                "a function value with a `var` parameter"
            } else if param.variadic {
                "a function value with a variadic parameter"
            } else if param.default.is_some() {
                "a function value with a parameter default"
            } else {
                continue;
            };
            return self.gap(what, expr);
        }
        let Some(Ty::Fn(func)) = self.settled_ty(expr) else {
            return self.gap("a function value the checker gave no function type", expr);
        };
        if func.params.len() != params.len() {
            return self.gap(
                "a function value whose written parameters and function type disagree",
                expr,
            );
        }
        let Some((param_layouts, returns)) = self.signature(&func, expr.span) else {
            return self.dead(expr);
        };
        let Some(captured) = self.captured_by(body, expr) else {
            return self.dead(expr);
        };

        // The number is taken before the body is lowered, because that body
        // may make a closure of its own and the inner one has to be numbered
        // after the outer.
        let at = self.pool.appended.len();
        let id = FunctionId((self.plan.decls.len() + at) as u32);
        self.pool.appended.push(None);
        let name: Arc<str> = Arc::from(format!("{}#{}", self.name, self.lambdas));
        self.lambdas += 1;

        let lowered = self.lambda_body(
            name,
            &param_layouts,
            returns,
            &func,
            params,
            body,
            &captured,
        );
        self.pool.appended[at] = Some(lowered);
        self.close_over(id, &captured, expr.span)
    }

    /// A declared function written where a value goes.
    ///
    /// An environment naming it and holding nothing, which is what makes a
    /// call through it the same instruction a lambda's call is.
    pub(super) fn function_value(&mut self, expr: &Expr, id: FunctionId) -> Val {
        // A declaration named where a value goes is exactly what the
        // checker's call graph does not record — there is no call site — so
        // this is one of the places a slice learns it was too small.
        if !self.reached(id) {
            return self.dead(expr);
        }
        // A generic declaration is not one function, so there is not one
        // environment to name it with either. Which instantiation a value
        // stands for is decided where the value is *made*, and the function
        // type the place states is what would say — a reading this lowering
        // has not been taught, and one nothing in the corpus asks for.
        //
        // Nothing reaches it today: `cove::type::mismatch` refuses
        // `let g: fn(Int) -> Int = id`, because `id`'s type is `fn(T) -> T`.
        // It is named rather than left out because the alternative below is
        // `dead(expr)` with nothing reported, and a silent wrong answer is
        // the one outcome this crate must not have.
        if !self.plan.decls[id.index()].decl.generics.is_empty() {
            let named = self.plan.decls[id.index()].name.clone();
            return self.gap(
                &format!("`{named}`, a generic declaration used as a function value"),
                expr,
            );
        }
        let Some(shape) = self.plan.shape(id) else {
            // The declaration itself is a gap, already reported where it is
            // written.
            return self.dead(expr);
        };
        // A function type says what a call passes and not whether it aliases:
        // `Signature::as_value` drops `var`, so `fn bump(var n: Int)` reads as
        // `fn(Int)`. A call through the value would then copy an `Int` into a
        // parameter the callee reads as an address. The two disagree about
        // what a word is, and that is a fact about the language rather than
        // about this lowering — so it is named here rather than emitted.
        if shape.params.contains(&shapes::ADDR) {
            let named = self.plan.decls[id.index()].name.clone();
            return self.gap(
                &format!("`{named}`, which takes a `var` parameter, used as a function value"),
                expr,
            );
        }
        self.close_over(id, &[], expr.span)
    }

    /// The environment object: the callee, then the captures, inline.
    ///
    /// The object is allocated before anything is written into it, and its
    /// payload is zeroed — so a collection that happens between the
    /// allocation and the last store reads null out of a capture word rather
    /// than whatever preceded the object in the heap. Every capture is a slot
    /// of this frame and so is a root of it, and the environment itself is in
    /// a `Repr::Ref` slot from the allocation onwards.
    fn close_over(&mut self, id: FunctionId, captured: &[Captured], span: Span) -> Val {
        let layouts: Vec<LayoutId> = captured.iter().map(|(_, _, layout)| *layout).collect();
        let named = self.callee_name(id);
        let object = self.pool.shapes.closure_of(&named, id, layouts);
        let held = self.pool.shapes.function_value();
        let dst = self.temp(held);
        self.emit(
            Inst::Alloc {
                dst: dst.slot,
                layout: object,
                len: Len::Fixed,
            },
            span,
        );
        let word = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: word.slot,
                value: id.0 as i64,
            },
            span,
        );
        self.emit(
            Inst::StoreField {
                obj: dst.slot,
                at: CLOSURE_CALLEE,
                src: word.slot,
                layout: shapes::INT,
            },
            span,
        );
        self.give_back(word.slot, word.layout);
        let mut at = CLOSURE_CAPTURES;
        for (_, slot, layout) in captured {
            self.emit(
                Inst::StoreField {
                    obj: dst.slot,
                    at,
                    src: *slot,
                    layout: *layout,
                },
                span,
            );
            at += self.width(*layout);
        }
        dst
    }

    /// What the body an environment names is called, for the layout's own
    /// name.
    ///
    /// A [`Shape::Closure`](crate::Shape::Closure) layout is one per lowered
    /// body, so it is an identity in the way `m.Point` is rather than a
    /// family in the way `Array` is — and a listing that called every one of
    /// them `closure` would say nothing about which body a call reaches.
    fn callee_name(&self, id: FunctionId) -> String {
        let at = id.index();
        if let Some(decl) = self.plan.decls.get(at) {
            return format!("{}.{}", decl.module, decl.name);
        }
        match &self.pool.appended[at - self.plan.decls.len()] {
            Some(held) => format!("{}.{}", held.module, held.name),
            // Only reachable from a lambda naming itself before its own body
            // has been lowered, which nothing does: the body is lowered
            // first and the environment is built from the way back out.
            None => id.to_string(),
        }
    }

    /// The bindings of this frame the lambda's body can read.
    ///
    /// Alphabetical, because the order decides only the layout of the
    /// environment and a deterministic one that does not depend on where in a
    /// frame a binding happens to sit is what makes two lowerings of one
    /// program agree.
    ///
    /// A `var` parameter is the one binding that cannot be captured here. The
    /// oracle captures the *value* behind one, and the word this frame holds
    /// is an address — so the capture would have to be a load, of a layout
    /// the frame does not record for an `Addr` slot. It is named rather than
    /// approximated.
    fn captured_by(&mut self, body: &Block, expr: &Expr) -> Option<Vec<Captured>> {
        let mut mentioned = BTreeSet::new();
        mention_block(body, &mut mentioned);
        let mut held = Vec::new();
        for name in &mentioned {
            let Some((slot, layout)) = self.frame.lookup(name) else {
                continue;
            };
            if layout == shapes::ADDR {
                self.errors.push(gap::gap(
                    &format!("a function value capturing `{name}`, a `var` parameter"),
                    expr.span,
                ));
                return None;
            }
            held.push((Arc::from(name.as_str()), slot, layout));
        }
        Some(held)
    }

    /// Lowers a lambda's body into a [`Function`].
    ///
    /// Its frame is a declaration's with one region more: the parameters from
    /// slot 0 in declaration order, then the captures, then the answer and
    /// everything the body needs. The captures are *before* the answer
    /// because `docs/LINEAR_VM.md` says they follow the parameters, and the
    /// machine copies them in beside the arguments.
    #[allow(clippy::too_many_arguments)]
    fn lambda_body(
        &mut self,
        name: Arc<str>,
        param_layouts: &[LayoutId],
        returns: LayoutId,
        func: &FnTy,
        params: &[Param],
        body: &Block,
        captured: &[Captured],
    ) -> Function {
        let mut frame = Frame::new();
        let mut param_slots = Vec::with_capacity(param_layouts.len());
        for layout in param_layouts {
            let words = self.pool.shapes.words(*layout).to_vec();
            param_slots.push(frame.param(&words));
        }
        let mut held = Vec::with_capacity(captured.len());
        for (capture, _, layout) in captured {
            let words = self.pool.shapes.words(*layout).to_vec();
            held.push(Capture {
                name: capture.clone(),
                slot: frame.param(&words),
                layout: *layout,
            });
        }
        let answer = Dest {
            slot: {
                let words = self.pool.shapes.words(returns).to_vec();
                frame.alloc(&words)
            },
            layout: returns,
        };

        let mut inner = Body {
            checked: self.checked,
            sources: self.sources,
            plan: self.plan,
            pool: &mut *self.pool,
            errors: &mut *self.errors,
            wanted: &mut *self.wanted,
            module: self.module,
            name,
            lambdas: 0,
            frame,
            code: Vec::new(),
            spans: Vec::new(),
            loops: Vec::new(),
            held: Vec::new(),
            answer,
            returns: func.ret.clone(),
            // A lambda written inside a generic body is lowered once per
            // instantiation of it, so it is inside the same substitution: its
            // captures may be of the enclosing body's type parameters and its
            // own facts are recorded in their terms.
            generics: self.generics.clone(),
            args: self.args.clone(),
        };
        inner.frame.push_scope();
        // The captures are bound first, so a parameter or a `let` of the same
        // name shadows one: `Frame::lookup` searches a scope backwards, and
        // the over-approximating capture walk means a name the body binds for
        // itself may well be in both lists.
        for capture in &held {
            inner
                .frame
                .bind(&capture.name, capture.slot, capture.layout);
        }
        for (index, param) in params.iter().enumerate() {
            inner
                .frame
                .bind(&param.name.node, param_slots[index], param_layouts[index]);
        }
        inner.block(body, Some(answer));
        let clears = inner.frame.pop_scope();
        inner.clear(&clears, body.span);
        inner.emit(Inst::Return { src: answer.slot }, body.span);

        let reprs = inner.frame.reprs().to_vec();
        Function {
            module: Arc::from(self.module),
            name: inner.name.clone(),
            params: param_layouts.to_vec(),
            refs: RefMap::of(&reprs),
            reprs,
            returns,
            captures: held,
            code: inner.code,
            spans: inner.spans,
            span: body.span,
            is_async: false,
        }
    }

    // ---- calling one ------------------------------------------------------

    /// `g(1)`: a call through a value rather than through a name.
    ///
    /// The callee is evaluated first and held in a `Repr::Ref` slot for the
    /// length of the call, because [`Inst::CallClosure`] reads the object to
    /// find out what it is entering — and a slot of a live frame is what
    /// keeps that object reachable while it does.
    ///
    /// What the arguments are is read off the function type the checker
    /// settled, because that is the only thing a call site holds: the callee
    /// is a word, and which body it names is not known until the machine
    /// reads it.
    pub(super) fn call_value(&mut self, expr: &Expr, callee: &Expr, args: &[Arg]) -> Val {
        let Some(Ty::Fn(func)) = self.settled_ty(callee) else {
            return self.dead(expr);
        };
        if func.is_async {
            return self.gap("a call through an `async` function value", expr);
        }
        for arg in args {
            let what = if arg.spread {
                "a spread argument to a function value"
            } else if arg.is_var {
                "a `var` argument to a function value"
            } else if arg.label.is_some() {
                "a labelled argument to a function value"
            } else {
                continue;
            };
            return self.gap(what, expr);
        }
        if args.len() != func.params.len() {
            return self.gap(
                "a call through a function value that leaves a parameter to its default",
                expr,
            );
        }
        let Some((params, returns)) = self.signature(&func, expr.span) else {
            return self.dead(expr);
        };

        let closure = self.expr(callee);
        let mut held = Vec::with_capacity(args.len());
        for (index, arg) in args.iter().enumerate() {
            let value = self.expr(&arg.value);
            let value = self.erase(value, &arg.value, &func.params[index]);
            held.push(self.fit(value, params[index], arg.value.span));
        }
        let list = self.pool.args.intern(held.iter().map(Val::arg).collect());
        let dst = self.temp(returns);
        self.emit(
            Inst::CallClosure {
                dst: dst.slot,
                closure: closure.slot,
                args: list,
            },
            expr.span,
        );
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        self.release(closure, expr.span);
        dst
    }

    /// One [`Inst::CallClosure`] against a closure whose signature the caller
    /// has already read: the sequence loops build their own operand lists.
    pub(super) fn call_closure(
        &mut self,
        dst: Slot,
        closure: Slot,
        operands: Vec<crate::program::Arg>,
        span: Span,
    ) {
        let args = self.pool.args.intern(operands);
        self.emit(Inst::CallClosure { dst, closure, args }, span);
    }

    /// The layouts a function type's parameters and answer occupy.
    pub(super) fn signature(
        &mut self,
        func: &FnTy,
        span: Span,
    ) -> Option<(Vec<LayoutId>, LayoutId)> {
        let mut params = Vec::with_capacity(func.params.len());
        for ty in &func.params {
            params.push(self.layout(ty, span)?);
        }
        let returns = self.layout(&func.ret, span)?;
        Some((params, returns))
    }

    /// The function type of a callback argument, as the checker settled it.
    pub(super) fn callback(&mut self, arg: &Expr) -> Option<Arc<FnTy>> {
        match self.settled_ty(arg)? {
            Ty::Fn(func) => Some(func),
            other => {
                self.errors.push(gap::gap(
                    &format!("a callback of type `{other}`, which is not a function"),
                    arg.span,
                ));
                None
            }
        }
    }
}

// ------------------------------------------------------------ free names

/// Every name a block can read from the environment around it.
///
/// This is `cove_runtime::interp::mention_block`, which is the oracle's own
/// answer to the same question, written again here because a lowering may not
/// depend on a runtime. It **over-approximates**: a name the body binds for
/// itself is listed too, and so is one that only ever appears where a
/// namespace goes. Both are safe — the first is captured and then shadowed,
/// and the second is not a binding of the enclosing frame, so it is dropped
/// when the list is looked up. Missing one would not be.
fn mention_block(block: &Block, out: &mut BTreeSet<String>) {
    for stmt in &block.statements {
        mention_stmt(stmt, out);
    }
    if let Some(tail) = &block.tail {
        mention_expr(tail, out);
    }
}

fn mention_stmt(stmt: &Stmt, out: &mut BTreeSet<String>) {
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
            ItemKind::Struct(_)
            | ItemKind::Enum(_)
            | ItemKind::Trait(_)
            | ItemKind::TypeAlias(_) => {}
        },
    }
}

fn mention_fn(decl: &FnDecl, out: &mut BTreeSet<String>) {
    mention_params(&decl.params, out);
    mention_block(&decl.body, out);
}

/// A default argument is evaluated by the callee, so the names it reads
/// belong to the body.
fn mention_params(params: &[Param], out: &mut BTreeSet<String>) {
    for param in params {
        if let Some(default) = &param.default {
            mention_expr(default, out);
        }
    }
}

fn mention_expr(expr: &Expr, out: &mut BTreeSet<String>) {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Duration(_)
        | ExprKind::Unit
        | ExprKind::Continue => {}
        ExprKind::Ident(name) => {
            out.insert(name.clone());
        }
        ExprKind::Str(parts) => {
            for part in parts {
                if let StrPart::Interpolation(inner) = part {
                    mention_expr(inner, out);
                }
            }
        }
        ExprKind::ArrayLit(items) => {
            for item in items {
                mention_expr(item, out);
            }
        }
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
            if let Some(otherwise) = else_branch {
                mention_expr(otherwise, out);
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
        ExprKind::Return(value) | ExprKind::Break(value) => {
            if let Some(value) = value {
                mention_expr(value, out);
            }
        }
        // A lambda inside a lambda reads the outer one's environment, so its
        // free names are free here too — that is what makes the inner one's
        // captures reachable at all.
        ExprKind::Lambda { params, body, .. } => {
            mention_params(params, out);
            mention_block(body, out);
        }
        ExprKind::Scope { body, .. } => mention_block(body, out),
        ExprKind::Range { start, end, .. } => {
            mention_expr(start, out);
            mention_expr(end, out);
        }
    }
}

fn mention_pattern(pattern: &Pattern, out: &mut BTreeSet<String>) {
    match &pattern.kind {
        PatternKind::Wildcard | PatternKind::Binding(_) => {}
        PatternKind::Literal(expr) => mention_expr(expr, out),
        PatternKind::Variant { payload, .. } => {
            for part in payload {
                mention_pattern(part, out);
            }
        }
    }
}
