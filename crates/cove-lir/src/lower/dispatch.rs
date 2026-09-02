//! A call to a declaration, and how it finds the body it runs.
//!
//! There are two answers and the checker says which one applies. A method
//! call on a value of a declared type resolved to one declaration while the
//! program was being checked, and
//! [`Facts::target`](cove_sema::Facts::target) names it — so it is an
//! ordinary [`Inst::Call`] and costs nothing at run time. A call on a
//! `dyn Trait` value resolved to a *trait method*, and which implementation
//! it reaches is a fact about the value rather than about the source.
//!
//! # A method is an ordinary function whose slot 0 is the receiver
//!
//! Nothing about a method needs a second calling convention. ADR 0034 says
//! parameters occupy slots `0..arity` in the order a call supplies them, and
//! a call supplies the receiver first, so the receiver is slot 0 and the
//! written parameters follow. `var self` makes that slot a [`Repr::Addr`],
//! which is the same word a `var` parameter holds and needs no rule of its
//! own: a write to a field of `self` reaches the caller's object with no copy
//! back.
//!
//! A lowered method is named `Type.method` in the module whose `impl` block
//! writes it, so [`crate::Program::function_named`] finds
//! `("geometry", "Point.scaled")` and a diagnostic reads
//! `geometry.Point.scaled`. A module cannot declare a type and a free
//! function of one name, and a `.` is not a name character, so the two
//! namings cannot collide.
//!
//! # Dynamic dispatch is the object answering what it is
//!
//! There is no dispatch instruction and no vtable. A `dyn Trait` value is a
//! reference; the object behind it names its own [`crate::LayoutId`] in its
//! header; and the lowering knows every type that conforms to the trait,
//! because ADR 0006 makes conformance explicit and therefore enumerable. So
//! a call site reads the layout with [`Inst::LayoutOf`] and hands it to an
//! [`Inst::Switch`] over a table built from the trait's conformances, with a
//! [`Inst::Trap`] for the entry that names no implementation.
//!
//! That is one indexed jump, and it is the control flow that was already
//! here. A dispatch instruction would have had to carry a trait, a method
//! and a resolution rule into the machine — a runtime type universe, which
//! is what ADR 0034 forbids reconstructing.
//!
//! # A `dyn` value is always a box, because a location has one width
//!
//! [`Body::erase`] boxes a concrete value where the type it is going into is
//! a `dyn Trait`: a parameter, a declared return type, a struct field, an
//! enum payload, an annotated `let`. That is exactly where the oracle's
//! `Interpreter::coerce` runs, and it is the language's one implicit
//! conversion.
//!
//! The predecessor had to tolerate a value that had missed one of those
//! points, because every value was one word and a bare `Point` fitted in a
//! `dyn Display` slot. It does not fit here: a location's width is its
//! layout's, and a two-word `Point` written into a one-word `dyn` location
//! is a fault the verifier names. So [`Body::store`] boxes on the way in
//! wherever the two disagree, and a dispatch can read the concrete layout
//! out of the box's own first payload word without asking whether there is
//! one.

use std::sync::Arc;

use cove_diag::Span;
use cove_sema::facts::MethodTarget;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Expr, FnDecl};

use super::frame::Val;
use super::gap;
use super::pattern::UNPLACED;
use super::shapes;
use super::{Body, CallShape, PENDING};
use crate::inst::{CmpOp, Compare, Inst, Len, Slot};
use crate::layout::LayoutId;
use crate::program::{FunctionId, Table};

/// What fills one written parameter of a call.
///
/// The three the language has, and the reason a call site can no longer line
/// its arguments up with a frame by counting them: one parameter may take any
/// number of arguments and another may take none.
enum Fill {
    /// The argument at this index of the call.
    From(usize),
    /// Every argument a variadic parameter collects, in source order.
    Collect(Vec<usize>),
    /// Nothing was written, so the declaration's own default answers.
    Default,
}

impl Body<'_> {
    // ---- a call the checker resolved ------------------------------------

    /// A call to a declared function, method or associated function.
    ///
    /// One path for all three, because there is one calling convention: the
    /// arguments are evaluated in source order into locations of their own,
    /// every one is held until the call is emitted — the list [`Inst::Call`]
    /// names has to be live all at once, because the machine copies each
    /// argument's words into the callee's frame — and the receiver, where
    /// there is one, is the first of them.
    ///
    /// `base` is the receiver's expression, and it is read only when the
    /// callee declares one. `Point.origin()` is written through a name that
    /// is not a value, so its `base` is a namespace and is never evaluated.
    pub(super) fn call_target(
        &mut self,
        expr: &Expr,
        id: FunctionId,
        base: Option<&Expr>,
        args: &[Arg],
    ) -> Val {
        // A slice that left the callee out is this crate's mistake and not
        // the program's, so it is recorded and corrected rather than
        // reported: see `Body::reached`.
        if !self.reached(id) {
            return self.dead(expr);
        }
        let Some(shape) = self.plan.shape(id) else {
            // The declaration itself is a gap, already reported where it is
            // written. Saying so again at every call site would bury it.
            return self.dead(expr);
        };
        let Some(fills) = self.assignment(&shape, id, args, expr) else {
            return self.dead(expr);
        };

        let held = self.operands(&shape, Some(id), base, args, &fills, expr.span);
        let list = self.pool.args.intern(held.iter().map(Val::arg).collect());
        let dst = self.temp(shape.returns);
        self.emit(
            Inst::Call {
                dst: dst.slot,
                callee: id,
                args: list,
            },
            expr.span,
        );
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        dst
    }

    /// Which argument fills each written parameter.
    ///
    /// This is `interp::assign_labels` and the head of `interp::bind_params`,
    /// which are the one place the language says how a call lines up with a
    /// declaration. It used to be a count — a call whose argument list was as
    /// long as the parameter list lined up one for one, and anything else was
    /// a gap — and a count stopped being enough the moment a parameter could
    /// take any number of arguments or none.
    ///
    /// The rules are the oracle's, and every one of them the checker has
    /// already refused a program for breaking; they are followed rather than
    /// re-decided.
    ///
    /// - A **labelled** argument names its parameter, which must not be one
    ///   an earlier argument already reached.
    /// - A **positional** argument fills the next parameter — unless the last
    ///   one is variadic and the next is it, in which case it and every
    ///   argument after it are collected.
    /// - A parameter nothing filled takes its **default**, which the callee
    ///   evaluates.
    fn assignment(
        &mut self,
        shape: &CallShape,
        callee: FunctionId,
        args: &[Arg],
        expr: &Expr,
    ) -> Option<Vec<Fill>> {
        let written = shape.written();
        let variadic = shape.variadic.then(|| written - 1);
        let names = self.parameter_names(callee);

        let mut filled: Vec<Option<usize>> = vec![None; written];
        let mut collected: Vec<usize> = Vec::new();
        let mut next = 0usize;
        for (index, arg) in args.iter().enumerate() {
            let at = match &arg.label {
                Some(label) => match names.iter().position(|name| *name == label.node) {
                    Some(at) => at,
                    None => {
                        self.errors.push(gap::gap(
                            "a call with a label the declaration has no parameter for",
                            arg.span,
                        ));
                        return None;
                    }
                },
                None if variadic.is_some_and(|at| next >= at) => {
                    collected.push(index);
                    continue;
                }
                None if next < written => next,
                None => {
                    self.errors.push(gap::gap(
                        "a call with more arguments than the declaration takes",
                        arg.span,
                    ));
                    return None;
                }
            };
            if at < next || filled[at].is_some() {
                self.errors.push(gap::gap(
                    "a call whose labels do not follow the declaration's order",
                    arg.span,
                ));
                return None;
            }
            if variadic == Some(at) {
                collected.push(index);
            } else {
                filled[at] = Some(index);
            }
            next = at + 1;
        }

        let mut fills = Vec::with_capacity(written);
        for (at, held) in filled.into_iter().enumerate() {
            if variadic == Some(at) {
                fills.push(Fill::Collect(std::mem::take(&mut collected)));
                continue;
            }
            match held {
                Some(index) => fills.push(Fill::From(index)),
                None if self.has_default(callee, at) => fills.push(Fill::Default),
                None => {
                    self.errors.push(gap::gap(
                        "a call that leaves a parameter with no argument and no default",
                        expr.span,
                    ));
                    return None;
                }
            }
        }
        Some(fills)
    }

    /// The declaration a [`FunctionId`] names, where this lowering has one.
    ///
    /// A lambda is numbered past the declarations and has none, and nothing
    /// calls one through here: a call through a value is
    /// [`Body::call_value`], whose parameters a function type describes and
    /// which therefore has neither a default nor a variadic to read.
    fn declaration(&self, callee: FunctionId) -> Option<&FnDecl> {
        declared(self.plan, callee)
    }

    /// The names the declaration gives its written parameters, which are what
    /// a label is.
    fn parameter_names(&self, callee: FunctionId) -> Vec<String> {
        self.declaration(callee)
            .map(|decl| {
                decl.params
                    .iter()
                    .map(|param| param.name.node.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether written parameter `at` answers something when a call says
    /// nothing about it.
    fn has_default(&self, callee: FunctionId, at: usize) -> bool {
        self.declaration(callee)
            .and_then(|decl| decl.params.get(at))
            .is_some_and(|param| param.default.is_some())
    }

    /// The value a parameter's default answers, **evaluated in the callee's
    /// scope**.
    ///
    /// `interp::bind_params` is the rule: *"Default arguments are evaluated
    /// by the callee"*, with `env` the callee's own environment and the
    /// parameters before this one already declared in it. So the default may
    /// read them, and it reads names against the module the declaration was
    /// written in rather than the one the call site is in.
    ///
    /// Both of those are arranged here rather than by lowering a function of
    /// its own and calling it. The words are the caller's — the argument has
    /// to end up in the caller's frame either way — and what the callee's
    /// scope decides is only which *names* the default can see:
    ///
    /// - [`Frame::push_isolated_scope`](super::frame::Frame::push_isolated_scope)
    ///   opens a scope a lookup does not see past, so a `let x` around the
    ///   call site cannot shadow the module-level `x` the declaration meant;
    /// - the receiver and the parameters before this one are bound in it, at
    ///   the locations the call has already evaluated them into;
    /// - and `Body::module` is the callee's for the length of the walk, so
    ///   `Body::layout`, `Plan::resolve` and the host-module questions all
    ///   answer as they would inside the declaration.
    ///
    /// An extra frame would have been the other way, and it would have cost a
    /// call per omitted argument and a synthesised `Function` per defaulted
    /// parameter for exactly this.
    fn default_of(
        &mut self,
        shape: &CallShape,
        callee: Option<FunctionId>,
        at: usize,
        receiver: Option<&Val>,
        held: &[Option<Val>],
        span: Span,
    ) -> Val {
        let want = shape.param(at);
        let Some(callee) = callee else {
            self.errors.push(gap::gap(
                "a call through a value that leaves a parameter to its default",
                span,
            ));
            return self.temp(want);
        };
        // Read out of the plan rather than through `self`, because the walk
        // below writes into this body while it is still holding the callee's
        // syntax — and the syntax outlives both.
        let plan = self.plan;
        let Some(decl) = declared(plan, callee) else {
            self.errors.push(gap::gap(
                "a default of a declaration this lowering cannot read",
                span,
            ));
            return self.temp(want);
        };
        let Some(default) = decl.params[at].default.as_ref() else {
            self.errors
                .push(gap::gap("a parameter with no default to evaluate", span));
            return self.temp(want);
        };
        let names: Vec<&str> = decl
            .params
            .iter()
            .take(at)
            .map(|param| param.name.node.as_str())
            .collect();
        let module: &str = &plan.decls[callee.index()].module;

        let outer = std::mem::replace(&mut self.module, module);
        self.frame.push_isolated_scope();
        if let (true, Some(value)) = (shape.receiver, receiver) {
            self.frame.bind("self", value.slot, value.layout);
        }
        for (name, value) in names.iter().zip(held) {
            if let Some(value) = value {
                self.frame.bind(name, value.slot, value.layout);
            }
        }
        let value = self.expr(default);
        let ty = shape.ty(at).clone();
        let value = self.erase(value, default, &ty);
        let value = self.fit(value, want, default.span);
        let clears = self.frame.pop_scope();
        self.clear(&clears, default.span);
        self.module = outer;
        value
    }

    /// The locations a call passes, in the order the callee's frame wants
    /// them.
    ///
    /// The supplied arguments are evaluated first, in source order — which is
    /// parameter order, because the checker refuses a label out of
    /// declaration order and a positional argument after a labelled one. The
    /// defaults follow, in parameter order, which is where the oracle
    /// evaluates them: a call site evaluates what it wrote and then
    /// `bind_params` fills in the rest.
    fn operands(
        &mut self,
        shape: &CallShape,
        callee: Option<FunctionId>,
        base: Option<&Expr>,
        args: &[Arg],
        fills: &[Fill],
        span: Span,
    ) -> Vec<Val> {
        let mut held: Vec<Option<Val>> = vec![None; fills.len()];
        let mut receiver = None;
        if let (true, Some(base)) = (shape.receiver, base) {
            if shape.params[0] == shapes::ADDR {
                // `var self`: the method names the caller's storage, so the
                // receiver word is its address rather than a copy.
                receiver = Some(self.address_of(base));
            } else {
                let value = self.expr(base);
                let value = self.erase(value, base, &shape.types[0]);
                receiver = Some(self.fit(value, shape.params[0], base.span));
            }
        }
        for (at, fill) in fills.iter().enumerate() {
            match fill {
                Fill::From(index) => {
                    let arg = &args[*index];
                    if arg.is_var {
                        held[at] = Some(self.address_of(&arg.value));
                        continue;
                    }
                    let value = self.expr(&arg.value);
                    let value = self.erase(value, &arg.value, shape.ty(at));
                    held[at] = Some(self.fit(value, shape.param(at), arg.value.span));
                }
                Fill::Collect(indices) => {
                    held[at] = Some(self.collected(shape, at, args, indices, span));
                }
                Fill::Default => {}
            }
        }
        // The defaults are evaluated after every written argument, because
        // that is the order the oracle runs them in: the call site's
        // expressions are all evaluated before control reaches the callee,
        // and `bind_params` is where a default is first asked for.
        for (at, fill) in fills.iter().enumerate() {
            let Fill::Default = fill else { continue };
            held[at] = Some(self.default_of(shape, callee, at, receiver.as_ref(), &held, span));
        }

        let mut passed = Vec::with_capacity(shape.params.len());
        passed.extend(receiver);
        for value in held {
            passed.push(value.expect("every parameter was filled, defaulted or collected"));
        }
        passed
    }

    // ---- a variadic parameter ---------------------------------------------

    /// The `Array<T>` a variadic parameter is handed.
    ///
    /// `interp::bind_params` says what one collects: every argument no
    /// earlier parameter took, in source order, with a spread argument
    /// contributing the elements of the sequence it names rather than the
    /// sequence itself — and the whole of it bound as an **immutable
    /// `Array<T>`** inside the body however the arguments were written.
    ///
    /// So the caller builds the array, and nothing about the callee's frame
    /// changes: a variadic parameter is one ordinary location holding one
    /// ordinary array, and the calling convention has nothing to say about
    /// how it was filled.
    ///
    /// With no spread the length is a fact the lowering knows, and this is an
    /// array literal by another spelling. With one it is not, so the length
    /// is counted first — one for each plain argument and one [`Inst::Len`]
    /// per spread — and each spread is walked into the run.
    fn collected(
        &mut self,
        shape: &CallShape,
        at: usize,
        args: &[Arg],
        indices: &[usize],
        span: Span,
    ) -> Val {
        let array = shape.param(at);
        let Some(elem) = self.element_layout(array) else {
            let named = self.pool.shapes.layout(array).name.clone();
            self.errors.push(gap::gap(
                &format!("a variadic parameter of `{named}`, which is not a sequence"),
                span,
            ));
            return self.temp(array);
        };

        // Every argument in source order first, because they are ordinary
        // expressions and one of them may do something the next one sees.
        let mut held = Vec::with_capacity(indices.len());
        for index in indices {
            let arg = &args[*index];
            let value = self.expr(&arg.value);
            if arg.spread {
                held.push((self.spread_source(value, &arg.value), true));
                continue;
            }
            let value = self.erase(value, &arg.value, shape.ty(at));
            held.push((self.fit(value, elem, arg.value.span), false));
        }

        if held.is_empty() {
            // A variadic parameter given no arguments is an empty
            // `Array<T>`, which is what `bind_params` binds and what the
            // schema says leaves nothing for a default to answer. There is
            // nothing to count and nothing to step.
            let dst = self.temp(array);
            self.emit(
                Inst::Alloc {
                    dst: dst.slot,
                    layout: array,
                    len: Len::Count(0),
                },
                span,
            );
            return dst;
        }

        let plain = held.iter().filter(|(_, spread)| !spread).count() as u32;
        let spreads = held.iter().any(|(_, spread)| *spread);
        // A run of a length the lowering knows is asked for by that number; a
        // run whose length is the sum of what the spreads turn out to hold is
        // asked for by the word the sum was added up in.
        let counted = spreads.then(|| {
            let total = self.temp(shapes::INT);
            self.emit(
                Inst::Int {
                    dst: total.slot,
                    value: plain as i64,
                },
                span,
            );
            let length = self.temp(shapes::INT);
            for (value, spread) in &held {
                if !spread {
                    continue;
                }
                self.emit(
                    Inst::Len {
                        dst: length.slot,
                        obj: value.slot,
                    },
                    span,
                );
                self.add(total.slot, total.slot, length.slot, span);
            }
            self.give_back(length.slot, length.layout);
            total
        });

        let dst = self.temp(array);
        self.emit(
            Inst::Alloc {
                dst: dst.slot,
                layout: array,
                len: match &counted {
                    Some(total) => Len::Slot(total.slot),
                    None => Len::Count(plain),
                },
            },
            span,
        );
        let one = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: one.slot,
                value: 1,
            },
            span,
        );
        let index = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: index.slot,
                value: 0,
            },
            span,
        );
        for (value, spread) in &held {
            if *spread {
                self.spread_into(dst.slot, index.slot, one.slot, value, elem, span);
                continue;
            }
            self.emit(
                Inst::StoreElem {
                    obj: dst.slot,
                    index: index.slot,
                    src: value.slot,
                    layout: elem,
                },
                span,
            );
            self.add(index.slot, index.slot, one.slot, span);
        }
        self.give_back(index.slot, index.layout);
        self.give_back(one.slot, one.layout);
        if let Some(total) = counted {
            self.give_back(total.slot, total.layout);
        }
        for (value, _) in held.into_iter().rev() {
            self.release(value, span);
        }
        dst
    }

    /// The `Array` a spread argument expands, whatever sequence was written.
    ///
    /// A `Vector` is copied out with `Vector.toArray` first, which is the
    /// clone `bind_params` makes of `storage.elements` for the same reason:
    /// what is spread is the elements the vector had, and one object to walk
    /// is one walk to write.
    fn spread_source(&mut self, value: Val, from: &Expr) -> Val {
        let Some(Ty::Vector(elem)) = self.ty(from).cloned() else {
            return value;
        };
        let Some(array) = self.vector_snapshot(&value, &elem, from.span) else {
            return value;
        };
        self.release(value, from.span);
        array
    }

    /// Copies every element of `source` into `dst` from `index` onwards,
    /// advancing `index` as it goes.
    ///
    /// One walk per spread rather than one over a joined list, because what
    /// is being joined is runs of words at a stride and there is no
    /// instruction that copies a run of elements between two objects.
    fn spread_into(
        &mut self,
        dst: Slot,
        index: Slot,
        one: Slot,
        source: &Val,
        elem: LayoutId,
        span: Span,
    ) {
        let count = self.temp(shapes::INT);
        self.emit(
            Inst::Len {
                dst: count.slot,
                obj: source.slot,
            },
            span,
        );
        let at = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: at.slot,
                value: 0,
            },
            span,
        );
        let more = self.temp(shapes::BOOL);
        let held = self.temp(elem);
        let test = self.here();
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Lt,
                dst: more.slot,
                a: at.slot,
                b: count.slot,
            },
            span,
        );
        let done = self.emit(
            Inst::BranchFalse {
                cond: more.slot,
                to: PENDING,
            },
            span,
        );
        self.emit(
            Inst::LoadElem {
                dst: held.slot,
                obj: source.slot,
                index: at.slot,
                layout: elem,
            },
            span,
        );
        self.emit(
            Inst::StoreElem {
                obj: dst,
                index,
                src: held.slot,
                layout: elem,
            },
            span,
        );
        // One location holds one element per turn, so it is cleared at the
        // end of every turn — the discipline every other walk is under.
        if self.holds_ref(elem) {
            self.zero(held.slot, elem, span);
        }
        self.add(at.slot, at.slot, one, span);
        self.add(index, index, one, span);
        self.emit(Inst::Jump { to: test }, span);
        let rest = self.here();
        self.patch(done, rest);
        self.give_back(held.slot, held.layout);
        self.give_back(more.slot, more.layout);
        self.give_back(at.slot, at.layout);
        self.give_back(count.slot, count.layout);
    }

    /// `value.method(...)` or `Type.associated(...)`, where the checker
    /// recorded which declaration it reaches.
    ///
    /// The receiver's type is what decided it, and that is the one thing a
    /// pass reading the source cannot work out: `Array` and a declared
    /// `Point` may both have a `length`, and only the checker knows which
    /// one this is. So the target is read rather than resolved again.
    pub(super) fn call_declared_method(
        &mut self,
        expr: &Expr,
        target: &MethodTarget,
        base: &Expr,
        args: &[Arg],
    ) -> Val {
        let Some(id) = self.plan.method(target) else {
            return self.gap(
                &format!(
                    "`{}.{}.{}`, which the checker resolved but this lowering has no function for",
                    target.module, target.type_name, target.method
                ),
                expr,
            );
        };
        self.call_target(expr, id, Some(base), args)
    }

    // ---- a call through a trait object -----------------------------------

    /// `value.method(...)` where `value` is a `dyn Trait`.
    ///
    /// The static type says which trait the method comes from and nothing
    /// else, so the implementation is found from the value: it is one
    /// reference to a box, and the box's first payload word is the
    /// [`LayoutId`] of what it holds. That word goes straight into an
    /// [`Inst::Switch`] over a table the lowering builds from the trait's
    /// declared conformances.
    ///
    /// Each arm opens the box into a receiver of *its own* concrete layout,
    /// because that is what varies between the arms: a conforming `Point` is
    /// two words and a conforming `Name` is one, and the callee's first
    /// parameter is whichever the arm is for.
    pub(super) fn call_dyn(
        &mut self,
        expr: &Expr,
        base: &Expr,
        trait_name: &str,
        method: &str,
        args: &[Arg],
    ) -> Val {
        let Some((trait_module, trait_short)) =
            shapes::declaring(self.checked, self.module, trait_name)
        else {
            return self.gap("a `dyn` value of a trait this lowering cannot find", expr);
        };
        if self
            .layout(&Ty::Dyn(Arc::from(trait_name)), expr.span)
            .is_none()
        {
            return self.dead(expr);
        }
        let Some(arms) = self.conformances(&trait_module, &trait_short, method, expr) else {
            return self.dead(expr);
        };
        // Every conformance implements the trait's own signature, so all of
        // them agree on what a call passes and what it answers. The first is
        // read for both; a table with no entry at all is a call that can only
        // trap, and then the call site's own recorded type is the answer.
        let shape = arms.first().and_then(|(_, id)| self.plan.shape(*id));
        // A `var self` trait method never arrives here: a dispatch hands the
        // arm the concrete value, and which storage a trait object names is
        // a question of its own — which `cove::type::dyn_mutating_method`
        // already refuses to let a program ask.
        if let Some(shape) = &shape {
            if args.len() != shape.written() || shape.variadic {
                return self.gap(
                    "a call through a trait object that does not pass one argument per parameter",
                    expr,
                );
            }
        }

        let receiver = self.expr(base);
        let tag = self.temp(shapes::INT);
        self.emit(
            Inst::LoadField {
                dst: tag.slot,
                obj: receiver.slot,
                at: 0,
                layout: shapes::INT,
            },
            expr.span,
        );

        let held = match &shape {
            Some(shape) => {
                let fills: Vec<Fill> = (0..args.len()).map(Fill::From).collect();
                self.operands(shape, None, None, args, &fills, expr.span)
            }
            None => Vec::new(),
        };
        let returns = shape
            .as_ref()
            .map_or_else(|| self.layout_of(expr), |it| it.returns);
        let dst = self.temp(returns);
        let switch = self.emit(
            Inst::Switch {
                on: tag.slot,
                table: UNPLACED,
            },
            expr.span,
        );
        self.give_back(tag.slot, tag.layout);

        // One arm per conformance, laid out in one run, and a trap after
        // them. The trap is where the switch sends a layout no conformance
        // claims, and it is not a formality even for a table that covers
        // every declared one: the index came out of a heap object, and the
        // machine does not take the lowering's word for what is in it.
        let mut entries = Vec::with_capacity(arms.len());
        let mut ends = Vec::with_capacity(arms.len());
        for (layout, callee) in &arms {
            entries.push(self.here());
            let concrete = self.temp(*layout);
            self.emit(
                Inst::Unbox {
                    dst: concrete.slot,
                    src: receiver.slot,
                    layout: *layout,
                },
                expr.span,
            );
            let mut passed = Vec::with_capacity(held.len() + 1);
            passed.push(concrete.arg());
            passed.extend(held.iter().map(Val::arg));
            let list = self.pool.args.intern(passed);
            self.emit(
                Inst::Call {
                    dst: dst.slot,
                    callee: *callee,
                    args: list,
                },
                expr.span,
            );
            self.release(concrete, expr.span);
            ends.push(self.emit(Inst::Jump { to: PENDING }, expr.span));
        }
        let trap = self.here();
        let message = self.string(&format!(
            "no implementation of `{trait_short}.{method}` for this value"
        ));
        self.emit(Inst::Trap { message }, expr.span);

        let width = arms
            .iter()
            .map(|(layout, _)| layout.0 + 1)
            .max()
            .unwrap_or(0);
        let mut targets = vec![trap; width as usize];
        for ((layout, _), entry) in arms.iter().zip(entries) {
            targets[layout.index()] = entry;
        }
        let table = self.pool.table(Table {
            targets,
            default: trap,
        });
        self.place_table(switch, table);

        let end = self.here();
        for at in ends {
            self.patch(at, end);
        }
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        self.release(receiver, expr.span);
        dst
    }

    /// Every type that conforms to `trait_module.trait_name`, as the layout
    /// its values have and the function that answers `method` for it.
    ///
    /// ADR 0006 makes conformance explicit and forbids a blanket
    /// implementation, which is what makes this list complete: a trait's
    /// implementors are a fact the package states rather than one a pass
    /// infers. The orphan rule puts the `impl` block in the trait's module or
    /// the type's, so the search is over the package's conformances rather
    /// than over one module's.
    ///
    /// Meeting the call site is what declares each implementor's layout —
    /// the same rule a struct literal follows — because a dispatch is a place
    /// the machine has to be able to recognise every one of them.
    fn conformances(
        &mut self,
        trait_module: &str,
        trait_name: &str,
        method: &str,
        expr: &Expr,
    ) -> Option<Vec<(LayoutId, FunctionId)>> {
        let mut conforming: Vec<(String, String)> = Vec::new();
        for resolved in self.checked.modules.values() {
            for conformance in resolved.conformances.values() {
                if conformance.trait_module == trait_module
                    && conformance.trait_name == trait_name
                    && conformance.methods.contains(method)
                {
                    conforming.push((
                        conformance.type_module.clone(),
                        conformance.type_name.clone(),
                    ));
                }
            }
        }
        conforming.sort();
        conforming.dedup();

        let mut arms = Vec::with_capacity(conforming.len());
        for (type_module, type_name) in conforming {
            let Some(callee) = self.plan.method_of(&type_module, &type_name, method) else {
                // A conformance the package declares and no declaration
                // answers. Named rather than passed over silently: a
                // dispatch table with a hole in it is a `Switch` whose arm
                // traps, and this is the one place that could build one.
                self.errors.push(gap::gap(
                    &format!(
                        "`{type_module}.{type_name}.{method}`, a conformance this lowering \
                         has no function for"
                    ),
                    expr.span,
                ));
                return None;
            };
            // A dispatch is where the slice is widest: which conformance
            // runs is decided by the value, so every one of them is
            // reachable and none of them is named by a call site the
            // checker's graph could follow.
            if !self.reached(callee) {
                return None;
            }
            // An implementation that is itself a gap — a trait method's
            // default body, today — leaves the table with an arm that
            // cannot be called. The gap was reported where the declaration
            // is, and a table built around it would be a call this lowering
            // knows nothing about the shape of.
            self.plan.shape(callee)?;
            let ty = self.declared_type(&type_module, &type_name)?;
            let layout = self.layout(&ty, expr.span)?;
            arms.push((layout, callee));
        }
        Some(arms)
    }

    /// The type `module.name` denotes, as the checker writes one.
    ///
    /// The name is qualified whichever module reads it, because a bare name
    /// only means something inside the module that declares it and a
    /// dispatch table is built from the package's conformances.
    fn declared_type(&self, module: &str, name: &str) -> Option<Ty> {
        let resolved = self.checked.modules.get(module)?;
        let qualified: Arc<str> = Arc::from(format!("{module}.{name}"));
        if resolved.structs.contains_key(name) {
            return Some(Ty::Struct(qualified, Vec::new()));
        }
        resolved
            .enums
            .contains_key(name)
            .then(|| Ty::Enum(qualified, Vec::new()))
    }

    // ---- erasure ----------------------------------------------------------

    /// Boxes a value where the type it is going into is a `dyn Trait`.
    ///
    /// This is the language's one implicit conversion, and it happens where a
    /// type is *written*: a parameter, a declared return type, a struct
    /// field, an enum payload, an annotated `let`. A value already erased is
    /// left alone, so `dyn Trait` does not nest — the same idempotence the
    /// oracle's `interp::as_dyn` has, for the same reason.
    ///
    /// A diverging expression is left alone too: nothing was written to its
    /// location, so there are no words to put in a box.
    pub(super) fn erase(&mut self, value: Val, from: &Expr, into: &Ty) -> Val {
        if !matches!(into, Ty::Dyn(_)) {
            return value;
        }
        if matches!(self.ty(from), Some(Ty::Dyn(_)) | Some(Ty::Never) | None) {
            return value;
        }
        // Interning the box's family here is what lets the machine find it:
        // the layout table is where a shape is looked up, and a program that
        // erases a value but never says so declares nowhere to put one.
        let Some(boxed) = self.layout(into, from.span) else {
            return value;
        };
        let dst = self.temp(boxed);
        self.emit(
            Inst::Box {
                dst: dst.slot,
                src: value.slot,
                layout: value.layout,
            },
            from.span,
        );
        self.release(value, from.span);
        dst
    }

    /// The `dyn Trait` an annotation writes, when it writes one.
    ///
    /// The trait's name is resolved to the module that declares it, because
    /// that is what makes a `dyn Display` written in two modules one type
    /// rather than two.
    pub(super) fn written_dyn(&self, ty: &cove_syntax::ast::Type) -> Option<Ty> {
        let cove_syntax::ast::TypeKind::Dyn(name) = &ty.kind else {
            return None;
        };
        let (module, short) = shapes::declaring(self.checked, self.module, &name.node)?;
        Some(Ty::Dyn(Arc::from(format!("{module}.{short}"))))
    }
}

/// The declaration a [`FunctionId`] names, held apart from [`Body`] so that a
/// body can read the callee's syntax while it is writing into its own frame.
fn declared<'a>(plan: &'a super::Plan<'a>, callee: FunctionId) -> Option<&'a FnDecl> {
    plan.decls.get(callee.index()).map(|decl| decl.decl)
}
