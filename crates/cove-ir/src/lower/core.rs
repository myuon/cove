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
//! verifier, both encoders and the native code generators see the same
//! instruction they already see for an `Array`'s length, and the function the
//! standard library wraps around it is small enough that `super::inline`
//! expands it where it is called.

use cove_diag::Span;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Expr};

use super::frame::Val;
use super::shapes::{self, VECTOR_STORE};
use super::{Body, Dest};
use crate::inst::{Inst, Slot, Storage};
use crate::layout::LayoutId;

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
