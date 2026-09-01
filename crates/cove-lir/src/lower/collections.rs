//! Sequences, ranges, and the loop that walks them.
//!
//! Three families of value and one form: an `Array`, a `Vector`, a `Range`,
//! and the `for` that iterates any of them.
//!
//! # A loop walks what the oracle walks
//!
//! `cove_runtime::interp::items_of` is the one place the language says what a
//! `for` sees, and its own documentation says why: the predecessor lowered a
//! sequence to a `length()`/`get(i)` index walk, which is not what a `Map` or
//! a `Set` does and is not what iteration *means* — it means the elements the
//! collection had when the loop began. So the lowering here takes the same
//! two answers from it rather than re-deriving them:
//!
//! - **the order**, which for a sequence is its own and for a range is
//!   ascending from `start` to the end the range was written with; and
//! - **the snapshot**, which is that a loop walks the elements the value had
//!   at the top and not the ones it has part way through.
//!
//! An `Array` is immutable, so holding the object *is* the snapshot and a
//! walk over it costs nothing extra. A `Vector` can be pushed to from inside
//! the body it is being walked by, so the loop takes a copy first — one
//! [`Inst::CallBuiltin`] of `Vector.toArray`, which is the same copy
//! `items_of` makes when it clones the elements out.
//!
//! # The element binding is one slot, and it is cleared
//!
//! A loop binds one name per turn, and the value behind it is a reference
//! whenever the elements are objects. One slot holds it for every turn, and
//! the slot is cleared at the end of each turn — so a walk over a large array
//! holds one element at a time rather than every element it has reached.
//! That is the invariant the whole design was reviewed for, and it is why the
//! `Clear` is emitted at the end of the body rather than left to the next
//! turn's overwrite.

use cove_diag::Span;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Block, Expr, Ident};

use super::frame::Val;
use super::gap;
use super::shapes::{word_of, RANGE_END, RANGE_INCLUSIVE, RANGE_START, VECTOR_LEN, VECTOR_STORE};
use super::{Body, Loop, PENDING};
use crate::inst::{ArithOp, CmpOp, Compare, Inst, Len, Num, Slot};
use crate::program::Builtin;
use crate::repr::Repr;

impl Body<'_> {
    // ---- literals ---------------------------------------------------------

    /// `[a, b, c]`: an object of the right length, then one store per
    /// element.
    ///
    /// The elements are in the object rather than behind an indirection,
    /// because an `Array` cannot grow and so needs none of what an
    /// indirection buys. Every element is evaluated before anything is
    /// stored, in source order, because an element is an ordinary expression
    /// and one of them may do something the next one sees.
    pub(super) fn array_literal(&mut self, expr: &Expr, items: &[Expr]) -> Val {
        let Some(ty) = self.owned_ty(expr) else {
            return self.dead(expr);
        };
        let Some(layout) = self.layout(&ty, expr.span) else {
            return self.dead(expr);
        };

        let mut held = Vec::with_capacity(items.len());
        for item in items {
            held.push(self.expr(item));
        }
        let dst = self.frame.alloc(Repr::Ref);
        self.emit(
            Inst::Alloc {
                dst,
                layout,
                len: Len::Count(items.len() as u32),
            },
            expr.span,
        );
        // One index slot written again rather than one per element: the
        // frame should not grow with the length of a literal.
        let index = self.frame.alloc(Repr::Int);
        for (at, value) in held.iter().enumerate() {
            self.emit(
                Inst::Int {
                    dst: index,
                    value: at as i64,
                },
                expr.span,
            );
            self.emit(
                Inst::SetElem {
                    obj: dst,
                    index,
                    src: value.slot,
                },
                expr.span,
            );
        }
        self.frame.free(index);
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        Val::temp(dst)
    }

    /// `0..<n` and `0..n`: one object with the two bounds and which of them
    /// the end is.
    ///
    /// A range is a value like any other — it can be bound, passed, compared
    /// and iterated later — so it is built here rather than folded into the
    /// loop that usually consumes it. `docs/LINEAR_VM.md` gives it one layout
    /// for the whole program, which is what keeps `..` and `..<` one family
    /// rather than two.
    pub(super) fn range_literal(
        &mut self,
        expr: &Expr,
        start: &Expr,
        end: &Expr,
        inclusive: bool,
    ) -> Val {
        let Some(layout) = self.layout(&Ty::Range, expr.span) else {
            return self.dead(expr);
        };
        let a = self.expr(start);
        let b = self.expr(end);
        let dst = self.frame.alloc(Repr::Ref);
        self.emit(
            Inst::Alloc {
                dst,
                layout,
                len: Len::Fixed,
            },
            expr.span,
        );
        self.emit(
            Inst::SetWord {
                obj: dst,
                at: RANGE_START,
                src: a.slot,
            },
            expr.span,
        );
        self.emit(
            Inst::SetWord {
                obj: dst,
                at: RANGE_END,
                src: b.slot,
            },
            expr.span,
        );
        let flag = self.frame.alloc(Repr::Bool);
        self.emit(
            Inst::Bool {
                dst: flag,
                value: inclusive,
            },
            expr.span,
        );
        self.emit(
            Inst::SetWord {
                obj: dst,
                at: RANGE_INCLUSIVE,
                src: flag,
            },
            expr.span,
        );
        self.frame.free(flag);
        self.release(b, expr.span);
        self.release(a, expr.span);
        Val::temp(dst)
    }

    /// `Vector.of(a, b, c)`: a store holding the elements, and a header
    /// naming it.
    ///
    /// The two objects are the whole of what a vector is, and both are
    /// allocated here for the reason a struct literal's object is: the
    /// lowering knows the layouts and [`Inst::Alloc`] takes one. What the
    /// machine is asked for is growth, which no instruction expresses — see
    /// [`Body::vector_method`].
    ///
    /// The store is allocated at exactly the length of the literal. A vector
    /// with no spare room is not a special case: the first `push` grows it
    /// like any other full one.
    pub(super) fn vector_of(&mut self, expr: &Expr, args: &[Arg]) -> Val {
        let Some(ty) = self.owned_ty(expr) else {
            return self.dead(expr);
        };
        let Ty::Vector(elem) = ty.clone() else {
            return self.gap(
                "`Vector.of` answering something other than a `Vector`",
                expr,
            );
        };
        let (Some(elem), Some(layout)) = (word_of(&elem), self.layout(&ty, expr.span)) else {
            self.errors.push(super::describe(&ty, expr.span));
            return self.dead(expr);
        };
        if let Some(bad) = self.plain_arguments(args) {
            return self.gap(bad, expr);
        }
        let store_layout = self.pool.shapes.store_of(elem);

        let mut held = Vec::with_capacity(args.len());
        for arg in args {
            held.push(self.expr(&arg.value));
        }
        let store = self.frame.alloc(Repr::Ref);
        self.emit(
            Inst::Alloc {
                dst: store,
                layout: store_layout,
                len: Len::Count(args.len() as u32),
            },
            expr.span,
        );
        let index = self.frame.alloc(Repr::Int);
        for (at, value) in held.iter().enumerate() {
            self.emit(
                Inst::Int {
                    dst: index,
                    value: at as i64,
                },
                expr.span,
            );
            self.emit(
                Inst::SetElem {
                    obj: store,
                    index,
                    src: value.slot,
                },
                expr.span,
            );
        }
        let dst = self.frame.alloc(Repr::Ref);
        self.emit(
            Inst::Alloc {
                dst,
                layout,
                len: Len::Fixed,
            },
            expr.span,
        );
        self.emit(
            Inst::Int {
                dst: index,
                value: args.len() as i64,
            },
            expr.span,
        );
        self.emit(
            Inst::SetWord {
                obj: dst,
                at: VECTOR_LEN,
                src: index,
            },
            expr.span,
        );
        self.emit(
            Inst::SetWord {
                obj: dst,
                at: VECTOR_STORE,
                src: store,
            },
            expr.span,
        );
        self.frame.free(index);
        self.release(Val::temp(store), expr.span);
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        Val::temp(dst)
    }

    // ---- methods -----------------------------------------------------------

    /// `items.length()`, `items.isEmpty()`, `items.get(i)`.
    ///
    /// An `Array` keeps its elements in the object, so its length is the
    /// object's own header length and an element is one [`Inst::GetElem`].
    /// There is no element assignment beside them: an `Array` is immutable,
    /// and the growable sequence is a `Vector`.
    pub(super) fn array_method(
        &mut self,
        expr: &Expr,
        base: &Expr,
        elem: &Ty,
        name: &str,
        args: &[Arg],
    ) -> Val {
        match (name, args.len()) {
            ("length", 0) | ("isEmpty", 0) => {
                let obj = self.expr(base);
                let len = self.frame.alloc(Repr::Int);
                self.emit(
                    Inst::Len {
                        dst: len,
                        obj: obj.slot,
                    },
                    expr.span,
                );
                self.release(obj, expr.span);
                self.length_answer(expr, name, len)
            }
            ("get", 1) => {
                let obj = self.expr(base);
                let index = self.expr(&args[0].value);
                let len = self.frame.alloc(Repr::Int);
                self.emit(
                    Inst::Len {
                        dst: len,
                        obj: obj.slot,
                    },
                    expr.span,
                );
                let answer = self.element_option(expr, obj.slot, len, index.slot, elem);
                self.frame.free(len);
                self.release(index, expr.span);
                self.release(obj, expr.span);
                answer
            }
            _ if HANDED_OVER.contains(&("Array", name)) => {
                self.machine_call(expr, Some(base), "Array", name, args)
            }
            _ => self.gap(&format!("`Array.{name}`"), expr),
        }
    }

    /// `items.push(x)`, `items.length()`, `items.get(i)`, `items.toArray()`.
    ///
    /// Reading a vector is ordinary instructions — the length is payload word
    /// 0 and the elements are in the store payload word 1 names — and only
    /// the two operations that need a *new* object go to the machine:
    /// `push`, which replaces the store with a larger one when the old one is
    /// full, and `toArray`, which builds an immutable copy. Neither is
    /// something an instruction expresses, and both are one
    /// [`Inst::CallBuiltin`] whose first operand is the vector.
    pub(super) fn vector_method(
        &mut self,
        expr: &Expr,
        base: &Expr,
        elem: &Ty,
        name: &str,
        args: &[Arg],
    ) -> Val {
        match (name, args.len()) {
            ("length", 0) | ("isEmpty", 0) => {
                let obj = self.expr(base);
                let len = self.frame.alloc(Repr::Int);
                self.emit(
                    Inst::GetWord {
                        dst: len,
                        obj: obj.slot,
                        at: VECTOR_LEN,
                    },
                    expr.span,
                );
                self.release(obj, expr.span);
                self.length_answer(expr, name, len)
            }
            ("get", 1) => {
                let obj = self.expr(base);
                let index = self.expr(&args[0].value);
                let (len, store) = self.vector_parts(obj.slot, expr.span);
                let answer = self.element_option(expr, store, len, index.slot, elem);
                self.frame.free(len);
                self.release(Val::temp(store), expr.span);
                self.release(index, expr.span);
                self.release(obj, expr.span);
                answer
            }
            _ if HANDED_OVER.contains(&("Vector", name)) => {
                self.machine_call(expr, Some(base), "Vector", name, args)
            }
            _ => self.gap(&format!("`Vector.{name}`"), expr),
        }
    }

    /// The length itself, or whether it is zero.
    ///
    /// `isEmpty()` is `length() == 0` and is lowered as one, rather than as
    /// an operation of its own: the two questions differ by a comparison the
    /// instruction set already has.
    fn length_answer(&mut self, expr: &Expr, name: &str, len: Slot) -> Val {
        if name == "length" {
            return Val::temp(len);
        }
        let zero = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::Int {
                dst: zero,
                value: 0,
            },
            expr.span,
        );
        let dst = self.frame.alloc(Repr::Bool);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Eq,
                dst,
                a: len,
                b: zero,
            },
            expr.span,
        );
        self.frame.free(zero);
        self.frame.free(len);
        Val::temp(dst)
    }

    /// The two words a vector header holds: how many elements it has, and
    /// where they are.
    ///
    /// The store is answered in a reference slot of its own, because reading
    /// an element is a read of *that* object and the collector has to see it
    /// held for as long as it is being read from.
    fn vector_parts(&mut self, obj: Slot, span: Span) -> (Slot, Slot) {
        let len = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::GetWord {
                dst: len,
                obj,
                at: VECTOR_LEN,
            },
            span,
        );
        let store = self.frame.alloc(Repr::Ref);
        self.emit(
            Inst::GetWord {
                dst: store,
                obj,
                at: VECTOR_STORE,
            },
            span,
        );
        (len, store)
    }

    /// `Some(elements[index])`, or `None` when `index` is outside them.
    ///
    /// The `None` is the allocation itself. [`Inst::Alloc`] zeroes the
    /// payload and `None` is case 0 of an `Option`, so an object that nothing
    /// else is written into is already the answer for a bad index — which is
    /// also what makes a half-built object safe to meet a collection.
    ///
    /// A negative index and an index at or past the length are one case with
    /// one answer, which is the rule `get`, `set` and `remove` all share.
    fn element_option(
        &mut self,
        expr: &Expr,
        elements: Slot,
        len: Slot,
        index: Slot,
        elem: &Ty,
    ) -> Val {
        let Some(ty) = self.owned_ty(expr) else {
            return self.dead(expr);
        };
        let Some(layout) = self.layout(&ty, expr.span) else {
            return self.dead(expr);
        };
        let Some(word) = word_of(elem) else {
            self.errors.push(super::describe(elem, expr.span));
            return self.dead(expr);
        };
        let span = expr.span;

        let dst = self.frame.alloc(Repr::Ref);
        self.emit(
            Inst::Alloc {
                dst,
                layout,
                len: Len::Fixed,
            },
            span,
        );
        let bound = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::Int {
                dst: bound,
                value: 0,
            },
            span,
        );
        let ok = self.frame.alloc(Repr::Bool);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Ge,
                dst: ok,
                a: index,
                b: bound,
            },
            span,
        );
        let below = self.emit(
            Inst::BranchFalse {
                cond: ok,
                to: PENDING,
            },
            span,
        );
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Lt,
                dst: ok,
                a: index,
                b: len,
            },
            span,
        );
        let above = self.emit(
            Inst::BranchFalse {
                cond: ok,
                to: PENDING,
            },
            span,
        );
        self.frame.free(ok);

        let value = self.frame.alloc(word);
        self.emit(
            Inst::GetElem {
                dst: value,
                obj: elements,
                index,
            },
            span,
        );
        self.emit(
            Inst::Int {
                dst: bound,
                value: 1,
            },
            span,
        );
        self.emit(
            Inst::SetWord {
                obj: dst,
                at: 0,
                src: bound,
            },
            span,
        );
        self.emit(
            Inst::SetWord {
                obj: dst,
                at: 1,
                src: value,
            },
            span,
        );
        self.frame.free(bound);
        self.release(Val::temp(value), span);

        let rest = self.here();
        self.patch(below, rest);
        self.patch(above, rest);
        Val::temp(dst)
    }

    /// An immutable copy of a vector's elements, as an `Array`.
    ///
    /// This is `Vector.toArray`, and it is what a `for` over a vector walks:
    /// `items_of` clones the elements out before the first turn, so a body
    /// that pushes onto the vector it is walking sees the same elements it
    /// started with. The result layout is interned here so that the machine
    /// has one to build the copy as.
    fn vector_snapshot(&mut self, obj: Slot, elem: &Ty, span: Span) -> Option<Slot> {
        let array = Ty::Array(Box::new(elem.clone()));
        self.layout(&array, span)?;
        let dst = self.frame.alloc(Repr::Ref);
        self.emit_builtin(dst, "Vector", "toArray", &[obj], Repr::Ref, span);
        Some(dst)
    }

    // ---- `for` ---------------------------------------------------------------

    /// `for x in <iterable> { ... }`.
    ///
    /// Which walk it is comes from the iterable's type, and the three the
    /// oracle defines for a sequence and a range are two walks here: a range
    /// counts, and both sequences read elements out of a run of words. A
    /// `Map` and a `Set` iterate too, and are left as gaps rather than
    /// approximated — `items_of` says a map yields a `MapEntry` per pair and
    /// a set yields its elements in ascending order, and neither is an index
    /// walk.
    pub(super) fn for_expr(&mut self, binding: &Ident, iterable: &Expr, body: &Block, span: Span) {
        let Some(ty) = self.owned_ty(iterable) else {
            return;
        };
        if matches!(ty, Ty::Array(_) | Ty::Vector(_)) && self.layout(&ty, iterable.span).is_none() {
            return;
        }
        match &ty {
            Ty::Range => self.for_range(binding, iterable, body, span),
            Ty::Array(elem) => {
                let elem = (**elem).clone();
                let value = self.expr(iterable);
                let obj = self.own_iterable(value, span);
                self.for_elements(obj, &elem, binding, body, span);
            }
            // The copy is the snapshot, and it is taken before the first
            // turn: the body may push onto the very vector it is walking,
            // and `items_of` cloned the elements out for exactly that
            // reason.
            Ty::Vector(elem) => {
                let elem = (**elem).clone();
                let value = self.expr(iterable);
                let Some(snapshot) = self.vector_snapshot(value.slot, &elem, iterable.span) else {
                    self.release(value, iterable.span);
                    return;
                };
                self.release(value, iterable.span);
                self.for_elements(snapshot, &elem, binding, body, span);
            }
            _ => {
                self.errors.push(gap::gap(
                    &format!("`for` over a value of type `{ty}`"),
                    iterable.span,
                ));
                self.discard(iterable);
            }
        }
    }

    /// A reference slot the loop owns for as long as it runs.
    ///
    /// A `for` walks the value the iterable had at the top, so the handle is
    /// copied out of whatever named it: a binding the body reassigns must not
    /// change what the walk is walking. A temporary is already the loop's
    /// own, and taking it over rather than copying it saves a word.
    fn own_iterable(&mut self, value: Val, span: Span) -> Slot {
        if value.temp {
            return value.slot;
        }
        let slot = self.frame.alloc(Repr::Ref);
        self.emit(
            Inst::Move {
                dst: slot,
                src: value.slot,
            },
            span,
        );
        slot
    }

    /// A walk over a run of words: `Array`, and the copy a `Vector` is walked
    /// through.
    ///
    /// The counter and the length are the loop's own slots, so the body can
    /// do what it likes to the names around it without the walk noticing.
    fn for_elements(&mut self, obj: Slot, elem: &Ty, binding: &Ident, body: &Block, span: Span) {
        let Some(word) = word_of(elem) else {
            self.errors.push(super::describe(elem, span));
            return;
        };
        let count = self.frame.alloc(Repr::Int);
        self.emit(Inst::Len { dst: count, obj }, span);
        let index = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::Int {
                dst: index,
                value: 0,
            },
            span,
        );
        let one = self.frame.alloc(Repr::Int);
        self.emit(Inst::Int { dst: one, value: 1 }, span);

        // The step is above the test so that `continue` has somewhere to jump
        // to that is known before the body is lowered, and the first turn
        // jumps over it. A `continue` that went to the test instead would
        // test the same index again and never finish.
        let enter = self.emit(Inst::Jump { to: PENDING }, span);
        let step = self.here();
        self.emit(
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: index,
                a: index,
                b: one,
            },
            span,
        );
        let test = self.here();
        self.patch(enter, test);
        let more = self.frame.alloc(Repr::Bool);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Lt,
                dst: more,
                a: index,
                b: count,
            },
            span,
        );
        let branch = self.emit(
            Inst::BranchFalse {
                cond: more,
                to: PENDING,
            },
            span,
        );
        self.frame.free(more);

        let element = self.frame.alloc(word);
        self.emit(
            Inst::GetElem {
                dst: element,
                obj,
                index,
            },
            span,
        );
        self.turn(binding, element, body, step, branch, span);

        self.frame.free(element);
        self.frame.free(one);
        self.frame.free(index);
        self.frame.free(count);
        self.release(Val::temp(obj), span);
    }

    /// A walk over the integers a range yields.
    ///
    /// The end is normalised once, at the top: an inclusive range is an
    /// exclusive one whose end is a step further on, which is
    /// `RangeBounds::of` written as two instructions. An empty or reversed
    /// range then iterates zero times without a case of its own, because the
    /// first test already fails.
    fn for_range(&mut self, binding: &Ident, iterable: &Expr, body: &Block, span: Span) {
        let value = self.expr(iterable);
        let range = value.slot;
        let index = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::GetWord {
                dst: index,
                obj: range,
                at: RANGE_START,
            },
            span,
        );
        let limit = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::GetWord {
                dst: limit,
                obj: range,
                at: RANGE_END,
            },
            span,
        );
        let inclusive = self.frame.alloc(Repr::Bool);
        self.emit(
            Inst::GetWord {
                dst: inclusive,
                obj: range,
                at: RANGE_INCLUSIVE,
            },
            span,
        );
        // The bounds are out of the object, so the object itself is done
        // with: a loop that ran for a long time should not hold it.
        self.release(value, span);

        let one = self.frame.alloc(Repr::Int);
        self.emit(Inst::Int { dst: one, value: 1 }, span);
        let exclusive = self.emit(
            Inst::BranchFalse {
                cond: inclusive,
                to: PENDING,
            },
            span,
        );
        self.frame.free(inclusive);
        self.emit(
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: limit,
                a: limit,
                b: one,
            },
            span,
        );
        let bounded = self.here();
        self.patch(exclusive, bounded);

        let enter = self.emit(Inst::Jump { to: PENDING }, span);
        let step = self.here();
        self.emit(
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: index,
                a: index,
                b: one,
            },
            span,
        );
        let test = self.here();
        self.patch(enter, test);
        let more = self.frame.alloc(Repr::Bool);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Lt,
                dst: more,
                a: index,
                b: limit,
            },
            span,
        );
        let branch = self.emit(
            Inst::BranchFalse {
                cond: more,
                to: PENDING,
            },
            span,
        );
        self.frame.free(more);

        // The counter is the binding: a `for` binding is read-only, so
        // nothing in the body can move the walk, and a slot copied into
        // every turn would be a word written for nothing.
        self.turn(binding, index, body, step, branch, span);

        self.frame.free(one);
        self.frame.free(limit);
        self.frame.free(index);
    }

    /// One turn of a loop: the binding, the body, and the end of both.
    ///
    /// `element` already holds this turn's value. It is bound rather than
    /// owned by the scope, because the scope gives its slots back when it
    /// ends and the next turn writes this one again — so the loop owns the
    /// slot and the scope owns only the name.
    ///
    /// The `Clear` at the end is what keeps a walk over a large collection
    /// from holding more than one element at a time. It is the loop's own,
    /// for the same reason: a scope that gave the slot back could not emit
    /// it, and the next turn's overwrite is not a promise the lowering is
    /// allowed to rely on.
    fn turn(
        &mut self,
        binding: &Ident,
        element: Slot,
        body: &Block,
        step: crate::inst::Pc,
        branch: crate::inst::Pc,
        span: Span,
    ) {
        self.loops.push(Loop {
            head: step,
            depth: self.frame.depth(),
            breaks: Vec::new(),
            element: self.holds_reference(element).then_some(element),
        });
        self.frame.push_scope();
        self.frame.bind(&binding.node, element);
        self.block(body, None);
        let clears = self.frame.pop_scope();
        self.clear(&clears, body.span);
        if self.holds_reference(element) {
            self.emit(Inst::Clear { slot: element }, body.span);
        }
        self.emit(Inst::Jump { to: step }, span);

        let end = self.here();
        self.patch(branch, end);
        let finished = self.loops.pop().expect("the loop was pushed above");
        for at in finished.breaks {
            self.patch(at, end);
        }
    }

    /// Whether a slot holds something a collection would trace.
    pub(super) fn holds_reference(&self, slot: Slot) -> bool {
        matches!(self.frame.repr(slot), Repr::Ref | Repr::Addr)
    }

    // ---- comparing objects ----------------------------------------------------

    /// `==` and `!=` between two values that are references.
    ///
    /// A `String` compares by its bytes, which is one instruction. Everything
    /// else the language defines `==` for is compared by walking the two
    /// objects, and that walk is not an instruction: what `==` means for an
    /// array, a struct, an enum or a vector is a rule of the language, stated
    /// in the language reference, and the IR describes families rather than
    /// carrying a case per family.
    ///
    /// The receiver is `Any` rather than a type's name, because this is one
    /// rule over every value the language gives an equality rather than a
    /// method a type declares — `cove_runtime::builtins` has no entry for it
    /// either, and the oracle reaches it as an operator.
    ///
    /// # What the builtin must do
    ///
    /// `Any.equals` takes two operands and answers whether they are the
    /// same value, by the rule `Value::eq_value` states for the oracle: two
    /// objects of different layouts are not equal; a string compares by
    /// bytes; a struct field-wise; an enum by case and then payload-wise; an
    /// array element-wise; a vector by the elements its length names; a box
    /// by what it holds. A scalar word is compared as the `Repr` the layout
    /// declares for it, so two `Float` words compare as doubles and not as
    /// bits.
    ///
    /// `!=` is the same call and an [`Inst::Not`]: one builtin rather than a
    /// second one that answers the negation.
    pub(super) fn compare_objects(
        &mut self,
        expr: &Expr,
        equal: bool,
        lhs: &Expr,
        dst: Slot,
        a: Slot,
        b: Slot,
    ) {
        if matches!(self.ty(lhs), Some(Ty::Str)) {
            self.emit(
                Inst::Cmp {
                    on: Compare::Str,
                    op: if equal { CmpOp::Eq } else { CmpOp::Ne },
                    dst,
                    a,
                    b,
                },
                expr.span,
            );
            return;
        }
        let builtin = self.pool.builtin(Builtin {
            receiver: "Any".into(),
            operation: "equals".into(),
            result: Repr::Bool,
        });
        let args = self.pool.args.intern(vec![a, b]);
        self.emit(Inst::CallBuiltin { dst, builtin, args }, expr.span);
        if !equal {
            self.emit(Inst::Not { dst, a: dst }, expr.span);
        }
    }

    /// `a is b`: whether two handles are the same storage.
    ///
    /// It is one comparison of two words, because that is exactly the
    /// question: a `Vector` is a header object that growth does not move, so
    /// two words that are the same address are the same vector and two that
    /// are not are not. The checker admits `is` for `Vector` and refuses it
    /// everywhere else, so nothing else reaches here.
    pub(super) fn identity(&mut self, expr: &Expr, lhs: &Expr, rhs: &Expr) -> Val {
        let a = self.expr(lhs);
        let b = self.expr(rhs);
        let dst = self.frame.alloc(Repr::Bool);
        if matches!(self.ty(lhs), Some(Ty::Vector(_))) {
            self.emit(
                Inst::Cmp {
                    on: Compare::Identity,
                    op: CmpOp::Eq,
                    dst,
                    a: a.slot,
                    b: b.slot,
                },
                expr.span,
            );
        } else {
            self.errors.push(gap::gap(
                "`is` on something other than a `Vector`",
                expr.span,
            ));
        }
        self.release(b, expr.span);
        self.release(a, expr.span);
        Val::temp(dst)
    }
}

/// The operations of a sequence the machine performs rather than the
/// instruction set.
///
/// Each of them either builds an object whose family only the layout table
/// knows — `slice`, `toVector`, `toArray` — or walks the elements with the
/// language's own equality, which is not something an instruction expresses.
/// The four that take a closure are not here: a call through a function value
/// is a gap of its own, and adding them would report the wrong one.
///
/// It is a list rather than a fall-through, because what this lowering emits
/// is a contract the machine is written against: a name that reached the
/// machine by accident would be a runtime refusal where a gap should have
/// named the work.
const HANDED_OVER: &[(&str, &str)] = &[
    ("Array", "contains"),
    ("Array", "indexOf"),
    ("Array", "slice"),
    ("Array", "toVector"),
    ("Vector", "push"),
    ("Vector", "set"),
    ("Vector", "pop"),
    ("Vector", "remove"),
    ("Vector", "contains"),
    ("Vector", "indexOf"),
    ("Vector", "slice"),
    ("Vector", "freeze"),
    ("Vector", "toArray"),
];

/// Whether the checker knows `name` as a builtin type that is written as a
/// namespace: `Vector.of(1, 2)`.
///
/// Only the one this lowering has been taught, because the answer is used to
/// decide what to emit rather than to describe the language.
pub(super) fn namespace_of(head: &str, ty: &Ty) -> bool {
    head == "Vector" && matches!(ty, Ty::Vector(_))
}
