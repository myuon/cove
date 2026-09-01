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

use cove_sema::facts::MethodTarget;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Expr};

use super::frame::Val;
use super::pattern::UNPLACED;
use super::shapes;
use super::{Body, CallShape, PENDING};
use crate::inst::{Inst, Slot};
use crate::layout::LayoutId;
use crate::program::{FunctionId, Table};

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
        let Some(shape) = self.plan.shape(id) else {
            // The declaration itself is a gap, already reported where it is
            // written. Saying so again at every call site would bury it.
            return self.dead(expr);
        };
        for arg in args {
            if arg.spread {
                return self.gap("a spread argument", expr);
            }
        }
        if args.len() != shape.arity() {
            return self.gap("a call that leaves a parameter to its default", expr);
        }

        let held = self.operands(&shape, base, args);
        let slots: Vec<Slot> = held.iter().map(|value| value.slot).collect();
        let list = self.pool.args.intern(slots);
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

    /// The locations a call passes, in the order the callee's frame wants
    /// them.
    ///
    /// # A label is not a permutation
    ///
    /// An argument may be labelled — `point.scaled(by: 2.0)` — and nothing
    /// here reorders anything. The checker already refused a label out of
    /// declaration order, a positional argument after a labelled one, and a
    /// repeated label; and a call that leaves a parameter to its default is
    /// a gap above, on the count. What is left is a list that lines up with
    /// the parameters one for one, which is what a label was already
    /// promising.
    fn operands(&mut self, shape: &CallShape, base: Option<&Expr>, args: &[Arg]) -> Vec<Val> {
        let mut held = Vec::with_capacity(shape.params.len());
        if let (true, Some(base)) = (shape.receiver, base) {
            if shape.params[0] == shapes::ADDR {
                // `var self`: the method names the caller's storage, so the
                // receiver word is its address rather than a copy.
                held.push(self.address_of(base));
            } else {
                let value = self.expr(base);
                let value = self.erase(value, base, &shape.types[0]);
                held.push(self.fit(value, shape.params[0], base.span));
            }
        }
        let first = usize::from(shape.receiver);
        for (index, arg) in args.iter().enumerate() {
            let want = shape.params[first + index];
            if arg.is_var {
                held.push(self.address_of(&arg.value));
                continue;
            }
            let value = self.expr(&arg.value);
            let value = self.erase(value, &arg.value, &shape.types[first + index]);
            held.push(self.fit(value, want, arg.value.span));
        }
        held
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
            if args.len() != shape.arity() {
                return self.gap("a call that leaves a parameter to its default", expr);
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
            Some(shape) => self.operands(shape, None, args),
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
            let mut slots: Vec<Slot> = Vec::with_capacity(held.len() + 1);
            slots.push(concrete.slot);
            slots.extend(held.iter().map(|value| value.slot));
            let list = self.pool.args.intern(slots);
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
            let callee = self.plan.method_of(&type_module, &type_name, method)?;
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
