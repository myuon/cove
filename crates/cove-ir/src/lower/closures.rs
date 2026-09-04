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

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use cove_diag::Span;
use cove_sema::typeck::{FnTy, Ty};
use cove_syntax::ast::{Arg, Block, Expr, ExprKind, ItemKind, Param, Pattern, PatternKind, Stmt};
use cove_syntax::ast::{FnDecl, StmtKind, StrPart};

use super::frame::{Frame, Val};
use super::gap;
use super::shapes::{self, CLOSURE_CALLEE, CLOSURE_CAPTURES};
use super::{Body, Dest};
use crate::inst::{Inst, Len, Pc, Slot};
use crate::layout::LayoutId;
use crate::program::{Capture, Function, FunctionId};
use crate::repr::RefMap;

/// One captured binding: the name the body reads it by, and the location in
/// the frame that is making the closure whose words go into the environment.
///
/// A location rather than a slot and a layout, because one of them is a
/// temporary this lowering made: the value behind a `var` parameter has to be
/// loaded out of the caller's storage before it can be stored, and whoever
/// asked for the list is what ends its live range.
type Captured = (Arc<str>, Val);

impl Body<'_> {
    // ---- making one -------------------------------------------------------

    /// `fn(x) { ... }`: a `Function` of its own, and an environment naming it.
    pub(super) fn lambda(&mut self, expr: &Expr, params: &[Param], body: &Block) -> Val {
        // The `async` a lambda was written with is not read here, and there
        // is no parameter for it: it is on the function type the checker
        // settled at this expression, which `Body::lambda_of` reads for
        // everything else about the boundary too. `FnTy::ret` is `R` rather
        // than `Task<R>`, so the body becomes a `Function` answering `R`
        // exactly as a declared `async fn` does, and `Body::call_value` is
        // what makes the task. See `Boundary::is_async`.
        //
        // The one caller that makes no task is a **host**. A Cove callback a
        // host runs is awaited by the oracle before its value reaches the
        // host, so a host handed `R` has been handed what the oracle would
        // have handed it — which is why an `async` handler stored in an
        // `http.Route` needs nothing said about it here.
        if let Some(what) = written_as_a_value(params) {
            return self.gap(what, expr);
        }
        self.lambda_of(expr, params, body, None)
    }

    /// `cell.lock(fn(var value) { ... })`: the one lambda whose first
    /// parameter may be written `var`.
    ///
    /// `behind` is the layout of the value the parameter aliases, which is
    /// what the cell wraps. The parameter's slot is a [`shapes::ADDR`] word
    /// holding the address of the cell's own value words, so the body writes
    /// where the value lies and nothing is copied in or out.
    ///
    /// # Why this does not weaken the refusal above
    ///
    /// [`written_as_a_value`] refuses a `var` parameter because a function
    /// *type* drops `var` — `Signature::as_value` turns `fn bump(var n: Int)`
    /// into `fn(Int)` — so a call *through a value* would copy a word into a
    /// parameter the callee reads as an address. The two would disagree about
    /// what a word is.
    ///
    /// There is no such call here. The environment this allocates is consumed
    /// by the [`Inst::CallClosure`] the `lock` lowering emits in the same
    /// breath, and that call site is the one that formed the address it
    /// passes. The closure never becomes a value some other call reaches
    /// through, so the disagreement the refusal exists to prevent has nowhere
    /// to happen — and every other question a function value's parameter list
    /// is asked is still asked here, of every parameter including this one.
    pub(super) fn aliasing_lambda(
        &mut self,
        expr: &Expr,
        params: &[Param],
        body: &Block,
        behind: LayoutId,
    ) -> Val {
        let Some((first, rest)) = params.split_first() else {
            return self.gap("a `lock` closure that takes no parameter", expr);
        };
        if let Some(what) = written_as_a_value(rest) {
            return self.gap(what, expr);
        }
        if first.variadic {
            return self.gap(A_VARIADIC_PARAMETER, expr);
        }
        if first.default.is_some() {
            return self.gap(A_PARAMETER_DEFAULT, expr);
        }
        self.lambda_of(expr, params, body, Some(behind))
    }

    /// The whole of a lambda, once its parameter list has been admitted.
    ///
    /// `alias` is `Some` for the one list that admits a `var` first
    /// parameter; see [`Body::aliasing_lambda`].
    fn lambda_of(
        &mut self,
        expr: &Expr,
        params: &[Param],
        body: &Block,
        alias: Option<LayoutId>,
    ) -> Val {
        let Some(Ty::Fn(func)) = self.settled_ty(expr) else {
            return self.gap("a function value the checker gave no function type", expr);
        };
        if func.params.len() != params.len() {
            return self.gap(
                "a function value whose written parameters and function type disagree",
                expr,
            );
        }
        let Some((mut param_layouts, returns)) = self.signature(&func, expr.span) else {
            return self.dead(expr);
        };
        // A `var` parameter is an address, and the function type the checker
        // settled does not say so — it says what a call *passes*. The written
        // list is what says, and the `lock` path is the only one that reads
        // it, so this is where the two are put back together.
        if alias.is_some() {
            param_layouts[0] = shapes::ADDR;
        }
        let Some(captured) = self.captured_by(body, expr.span) else {
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
            alias,
        );
        self.pool.appended[at] = Some(lowered);
        let dst = self.close_over(id, &captured, expr.span);
        // The loads a `var` capture needed are done with once the environment
        // holds their words. Every other capture is a binding of this frame
        // and is not this expression's to end, which is what `Val::temp`
        // records and `Body::release` reads.
        for (_, value) in captured.into_iter().rev() {
            self.release(value, expr.span);
        }
        dst
    }

    /// `fn double(n: Int) -> Int { ... }` written inside a body: the closure
    /// that body makes, and the name it binds to it.
    ///
    /// A local `fn` being a closure is not an inference about how one could
    /// be lowered. `cove_sema::resolve` says it in as many words; the checker
    /// declares the name with the signature's `as_value()` and pushes a
    /// capture floor over the block, so the names around it are captures and
    /// a capture is read-only; and the oracle reaches
    /// `Interpreter::make_closure` for one and declares what comes back.
    /// This is the fourth record of one decision rather than a fifth
    /// decision.
    ///
    /// # Its boundary is the recorded signature, not a settled type
    ///
    /// That is the whole of what it does not share with [`Body::lambda`]. A
    /// lambda's parameters and answer are read off the function type the
    /// checker settled *at the expression*, and a declaration is not an
    /// expression — there is no place for a type to have been settled at.
    /// `Checker::record_signature` is called for a local `fn` for exactly
    /// this reason, so the boundary is read the way every other
    /// declaration's is: [`Facts::signature`](cove_sema::Facts::signature),
    /// keyed by the declaration's own span.
    ///
    /// # It is named after the body that wrote it
    ///
    /// `main#0`, as a lambda written in the same place would be, rather than
    /// `double`. Two blocks of one body may each declare a `fn` of one name,
    /// so the declaration's own name is not an identity here — and a
    /// synthesised name that can collide is worse than one that does not read
    /// as the source.
    ///
    /// # Recursion is not arranged, because the oracle does not arrange it
    ///
    /// `make_closure` reads the captures out of the environment *before* the
    /// name is declared in it, so a local `fn` that calls itself does not find
    /// itself. [`Body::captured_by`] runs against this frame at the same
    /// moment, before the binding below, and answers the same way.
    pub(super) fn local_fn(&mut self, decl: &FnDecl) {
        let span = decl.span;
        // A declaration inside a body is still a declaration, and this is the
        // one thing a declaration can say that a function *value* has no way
        // to carry: which instantiation a generic stands for. It is not a
        // local `fn`'s own question — `Body::lambda` and
        // `Body::function_value` name it too — so it is named as the source
        // writes it.
        //
        // `async` was here and is not: a local `async fn` is a function value
        // whose *type* carries the `async`, and a call through the value is
        // where the task is made. See `Body::lambda`.
        if !decl.generics.is_empty() {
            self.errors
                .push(gap::gap("a generic function declared inside a body", span));
            return;
        }
        if let Some(what) = written_as_a_value(&decl.params) {
            self.errors.push(gap::gap(what, span));
            return;
        }
        // Read out of the facts rather than through `self`, because the walk
        // below writes into this body while it is still holding the
        // signature — and the facts outlive both.
        let checked = self.checked;
        let Some(signature) = checked.facts.signature(span.file, span) else {
            self.errors.push(gap::gap(
                "a function declared inside a body the checker recorded no signature for",
                span,
            ));
            return;
        };
        // The `async` is carried so that `Body::lambda_body` can record it,
        // and for nothing else: `Body::signature` asks this type only for
        // widths, and the flag a *call* reads is the one on the type the
        // checker settled at the call site.
        let func = FnTy {
            is_async: decl.is_async,
            params: signature.params.clone(),
            ret: signature.ret.clone(),
        };
        let Some((param_layouts, returns)) = self.signature(&func, span) else {
            return;
        };
        let Some(captured) = self.captured_by(&decl.body, span) else {
            return;
        };

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
            &decl.params,
            &decl.body,
            &captured,
            None,
        );
        self.pool.appended[at] = Some(lowered);
        let value = self.close_over(id, &captured, span);
        for (_, held) in captured.into_iter().rev() {
            self.release(held, span);
        }

        // The environment is a temporary until the name is given to it, and
        // then it is the scope's: the same handover a `let` makes of an
        // initialiser's run, for the same reason — nothing else can observe
        // the temporary, so the binding is the value rather than a copy of
        // it.
        let layout = value.layout;
        self.forget(value.slot);
        let width = self.width(layout);
        self.frame.own(value.slot, layout, width);
        let at = self.here();
        self.frame.bind(&decl.name.node, value.slot, layout, at);
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
        let layouts: Vec<LayoutId> = captured.iter().map(|(_, held)| held.layout).collect();
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
        for (_, held) in captured {
            self.emit(
                Inst::StoreField {
                    obj: dst.slot,
                    at,
                    src: held.slot,
                    layout: held.layout,
                },
                span,
            );
            at += self.width(held.layout);
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
    /// A `var` parameter is captured as the **value behind the address**, at
    /// the layout the parameter was declared at.
    ///
    /// The oracle is what fixes that: `Env::captures` calls `Place::read` on
    /// every binding it captures, and reading an alias place is reading the
    /// storage it names. So the environment holds a copy taken at creation
    /// time, exactly as it does for an ordinary binding, and the load is the
    /// one instruction that difference costs.
    ///
    /// # ADR 0001 and the oracle disagree about whether this is a program
    ///
    /// ADR 0001 says *"a `var` parameter cannot be stored or captured beyond
    /// the call"*, and nothing enforces the second half: the checker accepts
    /// `scan.word("true").mapError { scan.fail(...) }` and the oracle runs
    /// it. What the oracle runs, though, is a *copy* — no alias outlives the
    /// call, because the address was read through before the closure existed
    /// — so the rule's purpose holds even where its letter does not.
    ///
    /// This lowers what the oracle runs. ADR 0012 puts the oracle above a
    /// backend, and a backend that refused a program the oracle answers would
    /// be deciding a language question that belongs to the checker. If the
    /// sentence is to be enforced it is `cove-sema` that has to enforce it,
    /// and then this arm becomes unreachable rather than wrong.
    fn captured_by(&mut self, body: &Block, span: Span) -> Option<Vec<Captured>> {
        let mut mentioned = BTreeSet::new();
        mention_block(body, &mut mentioned);
        let mut held = Vec::new();
        for name in &mentioned {
            let Some((slot, layout)) = self.frame.lookup(name) else {
                continue;
            };
            let name: Arc<str> = Arc::from(name.as_str());
            if layout != shapes::ADDR {
                held.push((name, Val::borrowed(slot, layout)));
                continue;
            }
            let Some(behind) = self.aliases.get(&slot).copied() else {
                // Nothing else in a frame is an `Addr` a name is bound to, so
                // this is a `var` parameter whose declared type had no layout
                // — which was reported where the boundary was read.
                self.errors.push(gap::gap(
                    &format!("a function value capturing `{name}`, whose type has no layout"),
                    span,
                ));
                return None;
            };
            let value = self.temp(behind);
            self.emit(
                Inst::Load {
                    dst: value.slot,
                    addr: slot,
                    layout: behind,
                },
                span,
            );
            held.push((name, value));
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
        alias: Option<LayoutId>,
    ) -> Function {
        let mut frame = Frame::new();
        let mut param_slots = Vec::with_capacity(param_layouts.len());
        for layout in param_layouts {
            let words = self.pool.shapes.words(*layout).to_vec();
            param_slots.push(frame.param(&words));
        }
        // What the aliasing parameter names, at the width it was declared —
        // the same record a declaration's `var` parameter leaves, and for the
        // same reader: a capture is the one place with no expression the
        // checker recorded a type at.
        let mut aliased = HashMap::new();
        if let (Some(behind), Some(slot)) = (alias, param_slots.first()) {
            aliased.insert(*slot, behind);
        }
        let mut held = Vec::with_capacity(captured.len());
        for (capture, value) in captured {
            let words = self.pool.shapes.words(value.layout).to_vec();
            held.push(Capture {
                name: capture.clone(),
                slot: frame.param(&words),
                layout: value.layout,
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
            scopes: Vec::new(),
            held: Vec::new(),
            answer,
            returns: func.ret.clone(),
            // A lambda written inside a generic body is lowered once per
            // instantiation of it, so it is inside the same substitution: its
            // captures may be of the enclosing body's type parameters and its
            // own facts are recorded in their terms.
            generics: self.generics.clone(),
            args: self.args.clone(),
            // A lambda binds no `var` parameter — `Body::lambda` names one as
            // a gap — and a capture *of* one is a copy by the time the body
            // reads it, so nothing in this frame is an address a name is
            // bound to. The one exception is the closure a `Shared.lock`
            // runs, whose first parameter aliases the cell's value; see
            // [`Body::aliasing_lambda`].
            aliases: aliased,
        };
        inner.frame.push_scope();
        // The captures are bound first, so a parameter or a `let` of the same
        // name shadows one: `Frame::lookup` searches a scope backwards, and
        // the over-approximating capture walk means a name the body binds for
        // itself may well be in both lists.
        let start = inner.here();
        for capture in &held {
            inner
                .frame
                .bind(&capture.name, capture.slot, capture.layout, start);
        }
        for (index, param) in params.iter().enumerate() {
            inner.frame.bind(
                &param.name.node,
                param_slots[index],
                param_layouts[index],
                start,
            );
        }
        inner.block(body, Some(answer));
        let end = inner.here();
        let clears = inner.frame.pop_scope(end);
        inner.clear(&clears, body.span);
        inner.emit(Inst::Return { src: answer.slot }, body.span);

        let reprs = inner.frame.reprs().to_vec();
        let mut locals = inner.frame.locals();
        // A capture's slot is taken with `frame.param(&words)` above, the
        // same call a parameter's slot is taken with, so it is never freed
        // or cleared for the same reason: see `Frame::close_whole_function`.
        let never_freed: Vec<Slot> = param_slots
            .iter()
            .copied()
            .chain(held.iter().map(|capture| capture.slot))
            .collect();
        let end = inner.code.len() as Pc;
        Frame::close_whole_function(&mut locals, &never_freed, end);
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
            locals,
            span: body.span,
            // A record of how it was written, as `Function::is_async` is
            // for a declaration. The dispatch loop never reads it — what a
            // body does is not changed by the word, and the flag a call site
            // reads is the one on the function type it holds — but the
            // *boundary* does: `vm::boundary` builds a `Closure` value from
            // this field, so a host asking a callback whether it is `async`
            // is told what the source said. Leaving it `false` here was
            // harmless only while no lambda could be one.
            is_async: func.is_async,
            // A lambda is always a lowered body, never a stand-in: there is
            // no name to leave a stub for, since nothing outside the
            // function that wrote it could call it by one.
            stub: false,
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
        // A call through an `async` function value answers a settled task,
        // for the same reason a call to a declared `async fn` does and by the
        // same route: `Interpreter::call_closure` hands the closure's own
        // `is_async` to `call_target`, which wraps what the body produced.
        if func.is_async {
            return self.as_task(dst, expr.span);
        }
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

/// What stops a written parameter list from becoming a closure's, if
/// anything does.
///
/// One answer for a lambda and for a local `fn`, because what a closure's
/// frame can hold is a fact about the closure rather than about which of the
/// two spellings wrote it. ADR 0032 already refuses the variadic one in the
/// checker, so a valid program never brings one here; the other two are this
/// lowering's own work, and each is named as the source writes it.
fn written_as_a_value(params: &[Param]) -> Option<&'static str> {
    for param in params {
        let what = if param.is_var {
            A_VAR_PARAMETER
        } else if param.variadic {
            A_VARIADIC_PARAMETER
        } else if param.default.is_some() {
            A_PARAMETER_DEFAULT
        } else {
            continue;
        };
        return Some(what);
    }
    None
}

/// The three refusals, written once.
///
/// [`Body::aliasing_lambda`] asks two of them of a parameter the list above
/// would have refused for the third, so the words have to be the same words:
/// a reader who meets one of these has met the same sentence wherever it came
/// from.
const A_VAR_PARAMETER: &str = "a function value with a `var` parameter";
const A_VARIADIC_PARAMETER: &str = "a function value with a variadic parameter";
const A_PARAMETER_DEFAULT: &str = "a function value with a parameter default";

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
