//! The standard library's core intrinsics, lowered to the instructions that
//! perform them.
//!
//! [ADR 0058](../../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md)
//! moves a collection's public algorithm into Cove and keeps beneath it only
//! the smallest representation-dependent operation, which the standard library
//! spells `core.<name>(...)` — `cove_schema::builtins::CORE_INTRINSICS` is the
//! table. The checker admits such a call only inside a standard-library module,
//! and this is the lowering's half: each entry becomes run instructions in the
//! frame the call is written in, and **never** an [`Inst::CallBuiltin`]. A core
//! intrinsic is not a name for the machine to dispatch on; it is the operation
//! the name stands for.
//!
//! So nothing downstream of this file learns that a public method moved. The
//! verifier, both encoders and the native code generators see run instructions
//! — a `len`, a word `growable-push`, a `load-elem` or `store-elem` of a store, a
//! word `run-finish`, a word `run-slice` — and never the name of the method above
//! them; a function the standard library wraps around a single one of them is
//! small enough that `super::inline` expands it where it is called.

use cove_diag::Span;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Expr};

use super::frame::Val;
use super::shapes::{self, VECTOR_LEN, VECTOR_STORE};
use super::{Body, Dest};
use crate::inst::{Inst, Len, Slot, Storage, Validation};
use crate::layout::LayoutId;
use crate::program::Arg as Operand;

impl Body<'_> {
    /// `core.name(args)`, written in a standard-library module.
    ///
    /// Asked by [`Body::call_qualified`] before anything else a qualified
    /// name could be, and only for a module `cove_sema::stdlib` declares —
    /// the same question the checker asked before it typed the call, so a
    /// program the checker admitted cannot reach the gap below with a name it
    /// declares.
    pub(super) fn core_call(
        &mut self,
        expr: &Expr,
        name: &str,
        args: &[Arg],
        want: Option<Dest>,
    ) -> Val {
        match (name, args) {
            ("byteLength", [text]) => self.core_byte_length(expr, &text.value, want),
            ("vectorPush", [items, value]) => {
                self.core_vector_push(expr, &items.value, &value.value, want)
            }
            ("vectorLoad", [items, index]) => {
                self.core_vector_load(expr, &items.value, &index.value, want)
            }
            ("vectorStore", [items, index, value]) => {
                self.core_vector_store(expr, &items.value, &index.value, &value.value, want)
            }
            ("vectorFinish", [items]) => self.core_vector_finish(expr, &items.value, want),
            ("arraySlice", [items, from, count]) => {
                self.core_array_slice(expr, &items.value, &from.value, &count.value, want)
            }
            ("vectorSlice", [items, from, count]) => {
                self.core_vector_slice(expr, &items.value, &from.value, &count.value, want)
            }
            ("arrayToVector", [items]) => self.core_array_to_vector(expr, &items.value, want),
            _ => self.gap(&format!("`core.{name}`"), expr),
        }
    }

    /// The element layout of the `Vector<T>` `items` is, with the vector's
    /// own layout declared on the way.
    ///
    /// Declaring the vector's layout is not incidental: `Body::vector_method`
    /// says why meeting a value of the type is what declares it, and a growth
    /// allocates its larger store in the family the old one was.
    fn vector_element(&mut self, items: &Expr) -> Option<LayoutId> {
        let ty = self.settled_ty(items)?;
        let Ty::Vector(elem) = &ty else {
            self.errors.push(super::gap::gap(
                "a vector core intrinsic over something that is not a `Vector`",
                items.span,
            ));
            return None;
        };
        let elem = (**elem).clone();
        self.layout(&ty, items.span)?;
        self.layout(&elem, items.span)
    }

    /// `core.vectorPush(items, value)`: one element onto the end of the
    /// vector's growable run.
    ///
    /// One [`Inst::GrowablePush`] over [`Storage::Words`] of the element's
    /// layout, and then the `()` the call answers. The instruction writes no
    /// destination, so the unit is written separately into the location the
    /// surrounding form asked for, which is `Body::unit_answer`'s reason: a
    /// unit built in a temporary and copied out is a copy per push.
    fn core_vector_push(
        &mut self,
        expr: &Expr,
        items: &Expr,
        value: &Expr,
        want: Option<Dest>,
    ) -> Val {
        let Some(elem) = self.vector_element(items) else {
            return self.dead(expr);
        };
        let owner = self.expr(items);
        let src = self.expr(value);
        self.emit(
            Inst::GrowablePush {
                owner: owner.slot,
                src: src.slot,
                storage: Storage::Words(elem),
            },
            expr.span,
        );
        self.release(src, expr.span);
        self.release(owner, expr.span);
        self.unit_answer(expr, want)
    }

    /// The store a vector's elements are in: payload word 1 of the owner, in a
    /// reference location of its own.
    ///
    /// Held in a slot rather than read through, because an element read or
    /// write is a read or write of *that* object, and the collector has to see
    /// it held for as long as it is being used — `Body::vector_parts`' reason.
    fn vector_store(&mut self, owner: Slot, span: Span) -> Val {
        let store = self.temp(shapes::REF);
        self.emit(
            Inst::LoadField {
                dst: store.slot,
                obj: owner,
                at: VECTOR_STORE,
                layout: shapes::REF,
            },
            span,
        );
        store
    }

    /// `core.vectorLoad(items, index)`: the element at `index` of the vector's
    /// store.
    ///
    /// [`Inst::LoadField`] of the store and [`Inst::LoadElem`] out of it — #378's
    /// Q11 default, two instructions rather than a composite. The bound
    /// `LoadElem` checks is the **store's** header length, which is the capacity
    /// and not the logical length: an index in `[length, capacity)` reads spare
    /// room, which is zeroed. So this is only a vector read where the caller has
    /// already held `index` below `items.length()`, which is what every standard
    /// library body that calls it does first; a program cannot call it at all.
    fn core_vector_load(
        &mut self,
        expr: &Expr,
        items: &Expr,
        index: &Expr,
        want: Option<Dest>,
    ) -> Val {
        let Some(elem) = self.vector_element(items) else {
            return self.dead(expr);
        };
        let owner = self.expr(items);
        let at = self.expr(index);
        let store = self.vector_store(owner.slot, expr.span);
        let dst = self.answer_at(want, elem);
        self.emit(
            Inst::LoadElem {
                dst: dst.slot,
                obj: store.slot,
                index: at.slot,
                layout: elem,
            },
            expr.span,
        );
        self.release(store, expr.span);
        self.release(at, expr.span);
        self.release(owner, expr.span);
        dst
    }

    /// `core.vectorStore(items, index, value)`: `value` written over the element
    /// at `index` of the vector's store.
    ///
    /// [`Inst::LoadField`] of the store and [`Inst::StoreElem`] into it, then the
    /// `()` the call answers. Bounded as [`Body::core_vector_load`] is, by the
    /// capacity, and so a vector write only where the caller held `index` below
    /// the length first.
    fn core_vector_store(
        &mut self,
        expr: &Expr,
        items: &Expr,
        index: &Expr,
        value: &Expr,
        want: Option<Dest>,
    ) -> Val {
        let Some(elem) = self.vector_element(items) else {
            return self.dead(expr);
        };
        let owner = self.expr(items);
        let at = self.expr(index);
        let src = self.expr(value);
        let store = self.vector_store(owner.slot, expr.span);
        self.emit(
            Inst::StoreElem {
                obj: store.slot,
                index: at.slot,
                src: src.slot,
                layout: elem,
            },
            expr.span,
        );
        self.release(store, expr.span);
        self.release(src, expr.span);
        self.release(at, expr.span);
        self.release(owner, expr.span);
        self.unit_answer(expr, want)
    }

    /// `core.vectorFinish(items)`: the vector's store, relabelled to the `Array`
    /// of its live prefix, and the vector consumed.
    ///
    /// One [`Inst::RunFinish`] over [`Storage::Words`] of the element, with
    /// [`Validation::None`] and a `target` of the `Array<T>` layout, which is
    /// declared here by asking for it: the relabelled store is traced by that
    /// layout's reference map from then on. That the owner has no second holder
    /// is `cove_sema::unique`'s proof at the program's own `.freeze()` call; this
    /// lowering records nothing and asks nothing.
    fn core_vector_finish(&mut self, expr: &Expr, items: &Expr, want: Option<Dest>) -> Val {
        let Some(elem) = self.vector_element(items) else {
            return self.dead(expr);
        };
        let Some(ty) = self.settled_ty(items) else {
            return self.dead(expr);
        };
        let Ty::Vector(of) = ty else {
            return self.dead(expr);
        };
        let Some(target) = self.layout(&Ty::Array(of), expr.span) else {
            return self.dead(expr);
        };
        let owner = self.expr(items);
        let dst = self.answer_at(want, target);
        self.emit(
            Inst::RunFinish {
                dst: dst.slot,
                owner: owner.slot,
                target,
                validation: Validation::None,
                storage: Storage::Words(elem),
            },
            expr.span,
        );
        self.release(owner, expr.span);
        dst
    }

    /// One word [`Inst::RunSlice`]: `dst` becomes a fresh `target` run holding
    /// `count` elements of `elem` copied out of `src` from `from`.
    ///
    /// The one place a lowering builds the instruction's row, so the order —
    /// `dst`, `src`, `from`, `count` — and the rule that `dst`'s layout is the
    /// answer's are written once. Every caller has already decided the range:
    /// the bounds the machine checks are the source's header length, which for
    /// a vector's store is its capacity.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_slice_words(
        &mut self,
        dst: Slot,
        target: LayoutId,
        elem: LayoutId,
        src: &Val,
        from: &Val,
        count: &Val,
        span: Span,
    ) {
        let row = self.pool.args.intern(vec![
            Operand {
                slot: dst,
                layout: target,
            },
            src.arg(),
            from.arg(),
            count.arg(),
        ]);
        self.emit(
            Inst::RunSlice {
                args: row,
                storage: Storage::Words(elem),
            },
            span,
        );
    }

    /// The element `Ty` of the `Array<T>` `items` is, and the layouts of the
    /// array and of the element.
    fn array_element(&mut self, items: &Expr) -> Option<(Ty, LayoutId, LayoutId)> {
        let ty = self.settled_ty(items)?;
        let Ty::Array(elem) = &ty else {
            self.errors.push(super::gap::gap(
                "an array core intrinsic over something that is not an `Array`",
                items.span,
            ));
            return None;
        };
        let elem = (**elem).clone();
        let array = self.layout(&ty, items.span)?;
        let element = self.layout(&elem, items.span)?;
        Some((elem, array, element))
    }

    /// `core.arraySlice(items, from, count)`: a fresh `Array` of the `count`
    /// elements of `items` from `from`.
    ///
    /// One [`Inst::RunSlice`] whose source is the array itself and whose answer
    /// is the array's own layout. `std.array.slice` has clamped the range into
    /// the array before it asks, which is the policy this has none of.
    fn core_array_slice(
        &mut self,
        expr: &Expr,
        items: &Expr,
        from: &Expr,
        count: &Expr,
        want: Option<Dest>,
    ) -> Val {
        let Some((_, array, elem)) = self.array_element(items) else {
            return self.dead(expr);
        };
        let src = self.expr(items);
        let at = self.expr(from);
        let many = self.expr(count);
        let dst = self.answer_at(want, array);
        self.run_slice_words(dst.slot, array, elem, &src, &at, &many, expr.span);
        self.release(many, expr.span);
        self.release(at, expr.span);
        self.release(src, expr.span);
        dst
    }

    /// `core.vectorSlice(items, from, count)`: a fresh `Array` of the `count`
    /// elements of the vector from `from`.
    ///
    /// [`Inst::LoadField`] of the store and one [`Inst::RunSlice`] out of it,
    /// answering the `Array<T>` layout, which is declared here by asking for it.
    /// Bounded, as [`Body::core_vector_load`] is, by the store's capacity: the
    /// body that calls it — `std.vector.slice`, `std.vector.toArray` — holds the
    /// range inside `items.length()` first.
    fn core_vector_slice(
        &mut self,
        expr: &Expr,
        items: &Expr,
        from: &Expr,
        count: &Expr,
        want: Option<Dest>,
    ) -> Val {
        let Some(elem) = self.vector_element(items) else {
            return self.dead(expr);
        };
        let Some(Ty::Vector(of)) = self.settled_ty(items) else {
            return self.dead(expr);
        };
        let Some(target) = self.layout(&Ty::Array(of), expr.span) else {
            return self.dead(expr);
        };
        let owner = self.expr(items);
        let at = self.expr(from);
        let many = self.expr(count);
        let store = self.vector_store(owner.slot, expr.span);
        let dst = self.answer_at(want, target);
        self.run_slice_words(dst.slot, target, elem, &store, &at, &many, expr.span);
        self.release(store, expr.span);
        self.release(many, expr.span);
        self.release(at, expr.span);
        self.release(owner, expr.span);
        dst
    }

    /// `core.arrayToVector(items)`: a fresh `Vector` over a copy of the array's
    /// elements.
    fn core_array_to_vector(&mut self, expr: &Expr, items: &Expr, want: Option<Dest>) -> Val {
        let Some((elem, _, _)) = self.array_element(items) else {
            return self.dead(expr);
        };
        let src = self.expr(items);
        let answer = self.vector_of_elements(&src, &elem, want, expr.span);
        self.release(src, expr.span);
        match answer {
            Some(dst) => dst,
            None => self.dead(expr),
        }
    }

    /// A fresh `Vector<elem>` whose store is a copy of the fixed run `src` —
    /// `Array.toVector`, and the second half of a vector's `snapshot()`.
    ///
    /// The exact construction the runtime's `Array.toVector` made, in
    /// instructions this lowering already has: the length, a store of exactly
    /// that many elements, one [`Inst::RunCopy`] of whole elements into it, and
    /// the two-word header [`Body::vector_of`] builds. No spare room is
    /// allocated, so the allocations and the words they take are the builtin's.
    /// It is not a word `growable-alloc` and `growable-extend`, which would
    /// raise an empty or short store to the growable floor and so change what a
    /// program allocates for nothing a `toVector` needs.
    ///
    /// The source is read by the copy **before** the header is written into the
    /// answer, so an answer location that is the source's own is not read after
    /// it is overwritten; and the store is held in a slot of its own across the
    /// header's allocation, which may collect.
    pub(super) fn vector_of_elements(
        &mut self,
        src: &Val,
        elem: &Ty,
        want: Option<Dest>,
        span: Span,
    ) -> Option<Val> {
        let vector = self.layout(&Ty::Vector(Box::new(elem.clone())), span)?;
        let element = self.layout(elem, span)?;
        let store_layout = self.pool.shapes.store_of(element);
        let len = self.temp(shapes::INT);
        self.emit(
            Inst::Len {
                dst: len.slot,
                obj: src.slot,
            },
            span,
        );
        let store = self.temp(shapes::REF);
        self.emit(
            Inst::Alloc {
                dst: store.slot,
                layout: store_layout,
                len: Len::Slot(len.slot),
            },
            span,
        );
        let zero = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: zero.slot,
                value: 0,
            },
            span,
        );
        let row = self.pool.args.intern(vec![
            store.arg(),
            zero.arg(),
            src.arg(),
            zero.arg(),
            len.arg(),
        ]);
        self.emit(
            Inst::RunCopy {
                args: row,
                storage: Storage::Words(element),
            },
            span,
        );
        self.give_back(zero.slot, zero.layout);
        let dst = self.answer_at(want, vector);
        self.emit(
            Inst::Alloc {
                dst: dst.slot,
                layout: vector,
                len: Len::Fixed,
            },
            span,
        );
        self.emit(
            Inst::StoreField {
                obj: dst.slot,
                at: VECTOR_LEN,
                src: len.slot,
                layout: shapes::INT,
            },
            span,
        );
        self.emit(
            Inst::StoreField {
                obj: dst.slot,
                at: VECTOR_STORE,
                src: store.slot,
                layout: shapes::REF,
            },
            span,
        );
        self.give_back(len.slot, len.layout);
        self.release(store, span);
        Some(dst)
    }

    /// `core.byteLength(text)`: the string object's header length, which is
    /// its length in bytes.
    ///
    /// [`Inst::Len`] and nothing else. The header word it reads is the one an
    /// `Array`'s `length()` reads, and for a `String` the count in it is bytes —
    /// so the machine's `LEN` arm, with its null refusal, is already the whole
    /// of this operation, and the native code generators already emit it.
    fn core_byte_length(&mut self, expr: &Expr, text: &Expr, want: Option<Dest>) -> Val {
        let obj = self.expr(text);
        let dst = self.answer_at(want, shapes::INT);
        self.emit(
            Inst::Len {
                dst: dst.slot,
                obj: obj.slot,
            },
            expr.span,
        );
        self.release(obj, expr.span);
        dst
    }
}
