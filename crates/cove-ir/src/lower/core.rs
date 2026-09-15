//! The standard library's core intrinsics, lowered to the instructions that
//! perform them.
//!
//! [ADR 0058](../../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md)
//! moves a collection's public algorithm into Cove and keeps beneath it only
//! the smallest representation-dependent operation, which the standard library
//! spells `core.<name>(...)` — `cove_schema::builtins::CORE_INTRINSICS` is the
//! table. The checker admits such a call only inside a standard-library module,
//! and this is the lowering's half: each entry becomes run instructions in the
//! frame the call is written in, and never an [`Inst::CallBuiltin`] that names
//! a method — the keyed walks below are the one exception, and they name a
//! static intrinsic identity. A core intrinsic is not a name for the machine to
//! dispatch on; it is the operation the name stands for.
//!
//! The string builder is the one core type here with no public method of its
//! own. `std.stringbuilder`'s `StringBuilder` wraps a `ByteBuffer` — ADR 0052's
//! growable packed byte run — and its five operations are the five
//! `core.bytes*` intrinsics: a byte `growable-alloc`, `growable-push` and
//! `growable-extend`, a `run-finish` into `String`, and a field read of the
//! owner's length word. The owner is a reference, so nothing needs an address:
//! a `var self` builder is a `var` slot holding that reference, the body loads
//! it, and every append writes *through* it — which is why a growth that
//! replaces the store beneath the owner is visible to every frame naming the
//! builder.
//!
//! A keyed collection's search is the one family here with runtime calls
//! beneath it (ADR 0059, #378 Phase 4). Its element reads are run instructions
//! like every other — `core.memberAt` and `core.entryAt` are a `load-elem` of
//! the sorted run itself — but the order a search steps by and the admission
//! of a key are layout-directed walks no instruction performs for a struct, an
//! array or a set. So `core.order` is one `cmp` of `CmpOp::Order` where the
//! key is a scalar, a `String` or a case index in name order, and a
//! [`Inst::CallBuiltin`] of `Intrinsic::ValueOrder` otherwise;
//! `core.admitKey` is nothing at all where the key's layout cannot hold a
//! refused part, and `Intrinsic::ValueAdmitKey` where it can; and
//! `core.refuseDuplicate` is always `Intrinsic::ValueRefuseDuplicate`. Those
//! three are the only calls this file emits, and each is a static identity
//! rather than a name.
//!
//! So nothing downstream of this file learns that a public method moved. The
//! verifier, both encoders and the native code generators see run instructions
//! — a `len`, a word `growable-push`, a `load-elem` or `store-elem` of a store, a
//! word `run-finish` into an `Array`, a `Set` or a `Map`, a word or byte
//! `run-slice`, a word `run-copy` out of a sorted run, a word `growable-truncate` — and never the
//! name of the method above
//! them; a function the standard library wraps around a single one of them is
//! small enough that `super::inline` expands it where it is called.

use cove_diag::Span;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Expr};

use super::frame::Val;
use super::shapes::{self, BUFFER_LEN, VECTOR_LEN, VECTOR_STORE};
use super::{Body, Dest};
use crate::inst::{ArithOp, CmpOp, Compare, Inst, Len, Num, Slot, Storage, Validation};
use crate::intrinsic::Intrinsic;
use crate::layout::{LayoutId, Shape};
use crate::program::{Arg as Operand, Builtin};
use crate::repr::Repr;

/// How deep a key's layout may nest for `Body::always_admitted` to remove
/// the admission of it.
///
/// Well inside the runtime's bound on how deep a key is walked (128 steps,
/// of which a level of nesting takes at most two), so a layout this shallow
/// has no value the admission would stop for its depth.
const ADMITTED_DEPTH: usize = 48;

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
            ("vectorTruncate", [items, len]) => {
                self.core_vector_truncate(expr, &items.value, &len.value, want)
            }
            ("vectorMove", [items, to, from, count]) => self.core_vector_move(
                expr,
                &items.value,
                [&to.value, &from.value, &count.value],
                want,
            ),
            ("stringSlice", [text, from, count]) => {
                self.core_string_slice(expr, &text.value, &from.value, &count.value, want)
            }
            ("bytesAllocate", [capacity]) => self.core_bytes_allocate(expr, &capacity.value, want),
            ("bytesPush", [buffer, byte]) => {
                self.core_bytes_push(expr, &buffer.value, &byte.value, want)
            }
            ("bytesExtend", [buffer, text, from, to]) => self.core_bytes_extend(
                expr,
                &buffer.value,
                [&text.value, &from.value, &to.value],
                want,
            ),
            ("bytesFinish", [buffer]) => self.core_bytes_finish(expr, &buffer.value, want),
            ("bytesLength", [buffer]) => self.core_bytes_length(expr, &buffer.value, want),
            ("arrayLength", [items]) => self.core_array_length(expr, &items.value, want),
            ("vectorLength", [items]) => self.core_vector_length(expr, &items.value, want),
            ("order", [a, b]) => self.core_order(expr, &a.value, &b.value, want),
            ("admitKey", [key, method, role]) => {
                self.core_admit_key(expr, &key.value, [&method.value, &role.value], want)
            }
            ("refuseDuplicate", [key, method, role]) => {
                self.core_refuse_duplicate(expr, &key.value, [&method.value, &role.value], want)
            }
            ("memberAt", [members, at]) => {
                self.core_member_at(expr, &members.value, &at.value, want)
            }
            ("entryAt", [entries, at]) => self.core_entry_at(expr, &entries.value, &at.value, want),
            ("vectorWithCapacity", [capacity]) => {
                self.core_vector_with_capacity(expr, &capacity.value, want)
            }
            ("extendFromSet" | "extendFromMap", [out, run, from, count]) => self.core_extend_keyed(
                expr,
                &out.value,
                [&run.value, &from.value, &count.value],
                want,
            ),
            ("setFinish" | "mapFinish", [run]) => self.core_keyed_finish(expr, &run.value, want),
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

    /// `core.vectorTruncate(items, len)`: the vector's length lowered to `len`,
    /// and the elements above it cleared.
    ///
    /// One [`Inst::GrowableTruncate`] over [`Storage::Words`] of the element,
    /// then the `()` the call answers, written where the surrounding form asked
    /// for it as [`Body::core_vector_push`]'s is. `len` is the body's to have
    /// computed from the length it read; a `len` above it is refused.
    fn core_vector_truncate(
        &mut self,
        expr: &Expr,
        items: &Expr,
        len: &Expr,
        want: Option<Dest>,
    ) -> Val {
        let Some(elem) = self.vector_element(items) else {
            return self.dead(expr);
        };
        let owner = self.expr(items);
        let to = self.expr(len);
        self.emit(
            Inst::GrowableTruncate {
                owner: owner.slot,
                len: to.slot,
                storage: Storage::Words(elem),
            },
            expr.span,
        );
        self.release(to, expr.span);
        self.release(owner, expr.span);
        self.unit_answer(expr, want)
    }

    /// `core.vectorMove(items, to, from, count)`: `count` elements of the
    /// vector's store moved from `from` to `to`, as memmove.
    ///
    /// [`Inst::LoadField`] of the store and one word [`Inst::RunCopy`] of the
    /// store into itself, then the `()`. The bounds the copy checks are the
    /// store's capacity, so this is a vector write only where the body has held
    /// both ranges inside `items.length()` first — `std.vector.remove` moves
    /// the tail above the index it takes out.
    fn core_vector_move(
        &mut self,
        expr: &Expr,
        items: &Expr,
        [to, from, count]: [&Expr; 3],
        want: Option<Dest>,
    ) -> Val {
        let Some(elem) = self.vector_element(items) else {
            return self.dead(expr);
        };
        let owner = self.expr(items);
        let at = self.expr(to);
        let source = self.expr(from);
        let many = self.expr(count);
        let store = self.vector_store(owner.slot, expr.span);
        let row = self.pool.args.intern(vec![
            store.arg(),
            at.arg(),
            store.arg(),
            source.arg(),
            many.arg(),
        ]);
        self.emit(
            Inst::RunCopy {
                args: row,
                storage: Storage::Words(elem),
            },
            expr.span,
        );
        self.release(store, expr.span);
        self.release(many, expr.span);
        self.release(source, expr.span);
        self.release(at, expr.span);
        self.release(owner, expr.span);
        self.unit_answer(expr, want)
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

    /// `core.arrayLength(items)`: the array's header length, which counts its
    /// elements.
    ///
    /// [`Inst::Len`], as [`Body::core_byte_length`] is. The array's layout is
    /// declared on the way, because meeting a value of the type is what
    /// declares it.
    fn core_array_length(&mut self, expr: &Expr, items: &Expr, want: Option<Dest>) -> Val {
        if self.array_element(items).is_none() {
            return self.dead(expr);
        }
        let obj = self.expr(items);
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

    /// `core.vectorLength(items)`: payload word 0 of the vector's header.
    ///
    /// [`VECTOR_LEN`], read as [`Body::core_bytes_length`] reads a byte run's
    /// owner — never the store's header length, which is the capacity.
    fn core_vector_length(&mut self, expr: &Expr, items: &Expr, want: Option<Dest>) -> Val {
        if self.vector_element(items).is_none() {
            return self.dead(expr);
        }
        let obj = self.expr(items);
        let dst = self.answer_at(want, shapes::INT);
        self.emit(
            Inst::LoadField {
                dst: dst.slot,
                obj: obj.slot,
                at: VECTOR_LEN,
                layout: shapes::INT,
            },
            expr.span,
        );
        self.release(obj, expr.span);
        dst
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

    /// `core.stringSlice(text, from, count)`: a fresh `String` of the `count`
    /// bytes of `text` from `from`.
    ///
    /// One [`Inst::RunSlice`] over [`Storage::PackedBytes`], whose row's `dst`
    /// is [`shapes::STR`] — the answer is allocated as a `String` and is one
    /// the moment the copy has filled it. `std.string.sliceBytes` has held the
    /// range inside the string and both ends at character boundaries before it
    /// asks, which is the whole of why the instruction validates nothing.
    fn core_string_slice(
        &mut self,
        expr: &Expr,
        text: &Expr,
        from: &Expr,
        count: &Expr,
        want: Option<Dest>,
    ) -> Val {
        let src = self.expr(text);
        let at = self.expr(from);
        let many = self.expr(count);
        let dst = self.answer_at(want, shapes::STR);
        let row = self.pool.args.intern(vec![
            Operand {
                slot: dst.slot,
                layout: shapes::STR,
            },
            src.arg(),
            at.arg(),
            many.arg(),
        ]);
        self.emit(
            Inst::RunSlice {
                args: row,
                storage: Storage::PackedBytes,
            },
            expr.span,
        );
        self.release(many, expr.span);
        self.release(at, expr.span);
        self.release(src, expr.span);
        dst
    }

    /// `core.bytesAllocate(capacity)`: an empty byte [`Inst::GrowableAlloc`].
    ///
    /// Both layouts it allocates — the owner and its store — are program-wide
    /// constants the machine already holds, so there is nothing for the call to
    /// say beyond the capacity. That is a hint: a store too small for what is
    /// appended grows, and one larger than the final length gives the tail back
    /// at [`Inst::RunFinish`], so no value here changes what a program answers.
    fn core_bytes_allocate(&mut self, expr: &Expr, capacity: &Expr, want: Option<Dest>) -> Val {
        let capacity = self.expr(capacity);
        let dst = self.answer_at(want, shapes::BYTE_BUFFER);
        self.emit(
            Inst::GrowableAlloc {
                dst: dst.slot,
                capacity: capacity.slot,
                storage: Storage::PackedBytes,
            },
            expr.span,
        );
        self.release(capacity, expr.span);
        dst
    }

    /// `core.bytesPush(buffer, byte)`: one byte [`Inst::GrowablePush`], then
    /// the `()`.
    ///
    /// The instruction writes no destination, so the unit is written where the
    /// surrounding form asked for it — [`Body::unit_answer`] — and a value that
    /// is not a byte stops the run in the machine's own words.
    fn core_bytes_push(
        &mut self,
        expr: &Expr,
        buffer: &Expr,
        byte: &Expr,
        want: Option<Dest>,
    ) -> Val {
        let owner = self.expr(buffer);
        let value = self.expr(byte);
        self.emit(
            Inst::GrowablePush {
                owner: owner.slot,
                src: value.slot,
                storage: Storage::PackedBytes,
            },
            expr.span,
        );
        self.release(value, expr.span);
        self.release(owner, expr.span);
        self.unit_answer(expr, want)
    }

    /// `core.bytesExtend(buffer, text, from, to)`: one byte
    /// [`Inst::GrowableExtend`], then the `()`.
    ///
    /// Four operands and an encoded instruction with room for three, so the row
    /// goes in the argument pool the way a call's does — `[owner, src, from,
    /// to]`, in that order, each carrying its own layout so the bytecode
    /// verifier checks them by the rule it checks a call's arguments by.
    ///
    /// The range is checked by the machine, in `String.sliceBytes`'s words, and a
    /// range that fails stops the run. Nothing is checked here: the machine is
    /// already holding the header the bounds are read from, and ADR 0052
    /// requires the refusal to be the same refusal in the same sentence.
    fn core_bytes_extend(
        &mut self,
        expr: &Expr,
        buffer: &Expr,
        [text, from, to]: [&Expr; 3],
        want: Option<Dest>,
    ) -> Val {
        let owner = self.expr(buffer);
        let src = self.expr(text);
        let start = self.expr(from);
        let end = self.expr(to);
        let row = self
            .pool
            .args
            .intern(vec![owner.arg(), src.arg(), start.arg(), end.arg()]);
        self.emit(
            Inst::GrowableExtend {
                args: row,
                storage: Storage::PackedBytes,
            },
            expr.span,
        );
        self.release(end, expr.span);
        self.release(start, expr.span);
        self.release(src, expr.span);
        self.release(owner, expr.span);
        self.unit_answer(expr, want)
    }

    /// `core.bytesFinish(buffer)`: a byte [`Inst::RunFinish`] into `String`,
    /// validated as UTF-8.
    ///
    /// The live prefix is validated once and its store is relabelled down from
    /// the capacity to the logical length, so the `String` this answers *is* the
    /// bytes that were appended and not a copy of them. The owner is emptied by
    /// the instruction, which is why `cove_sema::unique` records this call as a
    /// consuming transition and has proved that nothing else holds the buffer.
    fn core_bytes_finish(&mut self, expr: &Expr, buffer: &Expr, want: Option<Dest>) -> Val {
        let owner = self.expr(buffer);
        let dst = self.answer_at(want, shapes::STR);
        self.emit(
            Inst::RunFinish {
                dst: dst.slot,
                owner: owner.slot,
                target: shapes::STR,
                validation: Validation::Utf8,
                storage: Storage::PackedBytes,
            },
            expr.span,
        );
        self.release(owner, expr.span);
        dst
    }

    /// `core.bytesLength(buffer)`: payload word 0 of the owner.
    ///
    /// [`BUFFER_LEN`], where a `Vector` keeps its own length and read exactly as
    /// that one is. What it must *not* read is the store's header length: that
    /// is the capacity, and ADR 0052's "capacity is not an Array length" is this
    /// distinction holding.
    fn core_bytes_length(&mut self, expr: &Expr, buffer: &Expr, want: Option<Dest>) -> Val {
        let obj = self.expr(buffer);
        let dst = self.answer_at(want, shapes::INT);
        self.emit(
            Inst::LoadField {
                dst: dst.slot,
                obj: obj.slot,
                at: BUFFER_LEN,
                layout: shapes::INT,
            },
            expr.span,
        );
        self.release(obj, expr.span);
        dst
    }

    /// `core.order(a, b)`: `-1`, `0` or `1` as `a` sorts before, equal to or
    /// after `b` in the order a key is kept in.
    ///
    /// One [`Inst::Cmp`] of [`CmpOp::Order`] where the key's layout is one a
    /// comparison instruction orders exactly as `key::order` does — see
    /// [`Body::ordered_by`] — and otherwise one [`Inst::CallBuiltin`] of
    /// [`Intrinsic::ValueOrder`], the layout-directed walk.
    fn core_order(&mut self, expr: &Expr, a: &Expr, b: &Expr, want: Option<Dest>) -> Val {
        let Some(ty) = self.settled_ty(a) else {
            return self.dead(expr);
        };
        let Some(layout) = self.layout(&ty, a.span) else {
            return self.dead(expr);
        };
        let left = self.expr(a);
        let right = self.expr(b);
        let dst = self.answer_at(want, shapes::INT);
        match self.ordered_by(layout) {
            Some(on) => {
                self.emit(
                    Inst::Cmp {
                        on,
                        op: CmpOp::Order,
                        dst: dst.slot,
                        a: left.slot,
                        b: right.slot,
                    },
                    expr.span,
                );
            }
            None => self.intrinsic_call(
                Intrinsic::ValueOrder,
                shapes::INT,
                dst.slot,
                &[&left, &right],
                expr.span,
            ),
        }
        self.release(right, expr.span);
        self.release(left, expr.span);
        dst
    }

    /// The comparison that orders a value of `layout` exactly as a key is
    /// ordered, where one instruction can.
    ///
    /// `key::order` ranks an `Int` and a `Duration` by their signed words, a
    /// `Bool` `false` first, and a `String` by its bytes — which are
    /// [`Compare::Int`], [`Compare::Bool`] and [`Compare::Str`]. It ranks an
    /// enum's cases by their *names*, and a payload-free enum's word is its
    /// case *index*, so [`Compare::Tag`] is the order only where the cases were
    /// declared in ascending name order; any other enum is a walk. A `Unit`
    /// has one value and no comparison instruction admits it, so it is a walk
    /// too, and so is everything wider than a word.
    fn ordered_by(&self, layout: LayoutId) -> Option<Compare> {
        match &self.pool.shapes.layout(layout).shape {
            Shape::Word(Repr::Int | Repr::Duration) => Some(Compare::Int),
            Shape::Word(Repr::Bool) => Some(Compare::Bool),
            Shape::Str => Some(Compare::Str),
            Shape::Enum { cases, payload } if payload.is_empty() => cases
                .windows(2)
                .all(|pair| pair[0].name < pair[1].name)
                .then_some(Compare::Tag),
            _ => None,
        }
    }

    /// `core.admitKey(key, method, role)`: the refusal of a key the language
    /// does not admit, in `method`'s words.
    ///
    /// Nothing at all where `key`'s layout cannot hold a part the admission
    /// refuses — [`Body::always_admitted`] — and there the call answers a `()`
    /// only if something reads one. Otherwise one [`Inst::CallBuiltin`] of
    /// [`Intrinsic::ValueAdmitKey`] over the key and the two names.
    fn core_admit_key(
        &mut self,
        expr: &Expr,
        key: &Expr,
        [method, role]: [&Expr; 2],
        want: Option<Dest>,
    ) -> Val {
        let Some(ty) = self.settled_ty(key) else {
            return self.dead(expr);
        };
        let Some(layout) = self.layout(&ty, key.span) else {
            return self.dead(expr);
        };
        if self.always_admitted(layout) {
            let held = self.expr(key);
            self.release(held, expr.span);
            return match want {
                Some(_) => self.unit_answer(expr, want),
                None => self.temp(shapes::UNIT),
            };
        }
        self.keyed_refusal(Intrinsic::ValueAdmitKey, expr, key, [method, role], want)
    }

    /// `core.refuseDuplicate(key, method, role)`: one [`Inst::CallBuiltin`]
    /// of [`Intrinsic::ValueRefuseDuplicate`], which always raises.
    fn core_refuse_duplicate(
        &mut self,
        expr: &Expr,
        key: &Expr,
        names: [&Expr; 2],
        want: Option<Dest>,
    ) -> Val {
        self.keyed_refusal(Intrinsic::ValueRefuseDuplicate, expr, key, names, want)
    }

    /// One [`Inst::CallBuiltin`] of a keyed refusal over a key and the two
    /// names its message is written with, answering `()`.
    fn keyed_refusal(
        &mut self,
        intrinsic: Intrinsic,
        expr: &Expr,
        key: &Expr,
        [method, role]: [&Expr; 2],
        want: Option<Dest>,
    ) -> Val {
        let held = self.expr(key);
        let method = self.expr(method);
        let role = self.expr(role);
        let dst = self.answer_at(want, shapes::UNIT);
        self.intrinsic_call(
            intrinsic,
            shapes::UNIT,
            dst.slot,
            &[&held, &method, &role],
            expr.span,
        );
        self.release(role, expr.span);
        self.release(method, expr.span);
        self.release(held, expr.span);
        dst
    }

    /// One [`Inst::CallBuiltin`] of `intrinsic` over `args`, answering a
    /// value of `result` into `dst`.
    fn intrinsic_call(
        &mut self,
        intrinsic: Intrinsic,
        result: LayoutId,
        dst: Slot,
        args: &[&Val],
        span: Span,
    ) {
        let builtin = self.pool.builtin(Builtin { intrinsic, result });
        let args = self
            .pool
            .args
            .intern(args.iter().map(|arg| arg.arg()).collect());
        self.emit(Inst::CallBuiltin { dst, builtin, args }, span);
    }

    /// Whether every value of `layout` is one the key admission admits, so
    /// that asking it could never refuse.
    ///
    /// The admission refuses a `Float`, a `Vector`, a closure, a task, a
    /// shared cell and anything that holds one, and looks inside a box at
    /// whatever it was given — so a word of those, a box, and every shape
    /// that is not a key's are `false`. An `Int`, a `Bool`, a `Duration`, a
    /// `Unit` and a `String` are `true`; an array, a struct and an enum are
    /// what their parts are; a set's members and a map's keys are keys by
    /// construction, so a set is `true` and a map is what its values are.
    ///
    /// The walk also refuses a layout that holds itself, and one nested deeper
    /// than [`ADMITTED_DEPTH`]: the admission stops a value nested past the
    /// machine's depth bound with a refusal of its own, and only a layout of
    /// bounded depth is one no value of can reach it.
    fn always_admitted(&self, layout: LayoutId) -> bool {
        fn walk(body: &Body<'_>, layout: LayoutId, path: &mut Vec<LayoutId>) -> bool {
            if path.contains(&layout) || path.len() >= ADMITTED_DEPTH {
                return false;
            }
            path.push(layout);
            let admitted = match &body.pool.shapes.layout(layout).shape {
                Shape::Word(repr) => {
                    matches!(repr, Repr::Unit | Repr::Bool | Repr::Int | Repr::Duration)
                }
                Shape::Str | Shape::Members { .. } => true,
                Shape::Elements {
                    elem,
                    growable: false,
                } => walk(body, *elem, path),
                Shape::Entries { value, .. } => walk(body, *value, path),
                Shape::Struct { fields, .. } => {
                    fields.iter().all(|field| walk(body, field.layout, path))
                }
                Shape::Enum { cases, .. } => cases
                    .iter()
                    .all(|case| case.parts.iter().all(|part| walk(body, part.layout, path))),
                _ => false,
            };
            path.pop();
            admitted
        }
        walk(self, layout, &mut Vec::new())
    }

    /// `core.memberAt(members, at)`: the member at `at` of a set's sorted run.
    ///
    /// One [`Inst::LoadElem`] of the set object itself at the member's layout:
    /// a `Set` is a run of members laid end to end, so the machine's element
    /// bound — the header's length — is the set's own length.
    fn core_member_at(
        &mut self,
        expr: &Expr,
        members: &Expr,
        at: &Expr,
        want: Option<Dest>,
    ) -> Val {
        let Some(ty) = self.settled_ty(members) else {
            return self.dead(expr);
        };
        let Ty::Set(elem) = &ty else {
            self.errors.push(super::gap::gap(
                "`core.memberAt` over something that is not a `Set`",
                members.span,
            ));
            return self.dead(expr);
        };
        let elem = (**elem).clone();
        if self.layout(&ty, members.span).is_none() {
            return self.dead(expr);
        }
        let Some(layout) = self.layout(&elem, members.span) else {
            return self.dead(expr);
        };
        self.load_keyed_element(expr, members, at, layout, want)
    }

    /// `core.entryAt(entries, at)`: the entry at `at` of a map's sorted run, as
    /// a `MapEntry`.
    ///
    /// One [`Inst::LoadElem`] of the map object itself at the `MapEntry<K, V>`
    /// layout, whose two fields are the key's words and then the value's —
    /// which is exactly how a map lays an entry out.
    fn core_entry_at(&mut self, expr: &Expr, entries: &Expr, at: &Expr, want: Option<Dest>) -> Val {
        let Some(ty) = self.settled_ty(entries) else {
            return self.dead(expr);
        };
        let Ty::Map(key, value) = &ty else {
            self.errors.push(super::gap::gap(
                "`core.entryAt` over something that is not a `Map`",
                entries.span,
            ));
            return self.dead(expr);
        };
        let entry = Ty::MapEntry(key.clone(), value.clone());
        if self.layout(&ty, entries.span).is_none() {
            return self.dead(expr);
        }
        let Some(layout) = self.layout(&entry, entries.span) else {
            return self.dead(expr);
        };
        self.load_keyed_element(expr, entries, at, layout, want)
    }

    /// One [`Inst::LoadElem`] of element `at` of the sorted run `run` is, at
    /// `layout`.
    fn load_keyed_element(
        &mut self,
        expr: &Expr,
        run: &Expr,
        at: &Expr,
        layout: LayoutId,
        want: Option<Dest>,
    ) -> Val {
        let obj = self.expr(run);
        let index = self.expr(at);
        let dst = self.answer_at(want, layout);
        self.emit(
            Inst::LoadElem {
                dst: dst.slot,
                obj: obj.slot,
                index: index.slot,
                layout,
            },
            expr.span,
        );
        self.release(index, expr.span);
        self.release(obj, expr.span);
        dst
    }

    /// `core.vectorWithCapacity(capacity)`: an empty `Vector` whose store has
    /// room for exactly `capacity` elements.
    ///
    /// [`Body::vector_of_elements`]' two allocations and two field writes with
    /// no copy between them: an [`Inst::Alloc`] of the store at `capacity`, an
    /// `Int` nought, the header's [`Inst::Alloc`], and its length and store
    /// fields. The store is zeroed by its allocation, so every unit above the
    /// length traces as null until something writes it, and it is held in a
    /// slot of its own across the header's allocation, which may collect. A
    /// negative capacity is refused by the store's allocation, in the words
    /// every allocation is.
    fn core_vector_with_capacity(
        &mut self,
        expr: &Expr,
        capacity: &Expr,
        want: Option<Dest>,
    ) -> Val {
        let Some(ty) = self.settled_ty(expr) else {
            return self.dead(expr);
        };
        let Ty::Vector(elem) = &ty else {
            self.errors.push(super::gap::gap(
                "`core.vectorWithCapacity` answering something that is not a `Vector`",
                expr.span,
            ));
            return self.dead(expr);
        };
        let (Some(vector), Some(element)) =
            (self.layout(&ty, expr.span), self.layout(elem, expr.span))
        else {
            return self.dead(expr);
        };
        let store_layout = self.pool.shapes.store_of(element);
        let room = self.expr(capacity);
        let store = self.temp(shapes::REF);
        self.emit(
            Inst::Alloc {
                dst: store.slot,
                layout: store_layout,
                len: Len::Slot(room.slot),
            },
            expr.span,
        );
        self.release(room, expr.span);
        let zero = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: zero.slot,
                value: 0,
            },
            expr.span,
        );
        let dst = self.answer_at(want, vector);
        self.emit(
            Inst::Alloc {
                dst: dst.slot,
                layout: vector,
                len: Len::Fixed,
            },
            expr.span,
        );
        self.emit(
            Inst::StoreField {
                obj: dst.slot,
                at: VECTOR_LEN,
                src: zero.slot,
                layout: shapes::INT,
            },
            expr.span,
        );
        self.emit(
            Inst::StoreField {
                obj: dst.slot,
                at: VECTOR_STORE,
                src: store.slot,
                layout: shapes::REF,
            },
            expr.span,
        );
        self.give_back(zero.slot, zero.layout);
        self.release(store, expr.span);
        dst
    }

    /// `core.extendFromSet(out, items, from, count)` and
    /// `core.extendFromMap(out, entries, from, count)`: `count` units of a
    /// sorted run from `from`, appended to a vector whose store has room.
    ///
    /// [`Inst::LoadField`] of the store and of the length, one word
    /// [`Inst::RunCopy`] out of the keyed run into the store at the length, an
    /// `Int` add, and the length written back: ADR 0058's run copy and commit,
    /// with no ensure, because the one body that calls this allocated the room
    /// with `core.vectorWithCapacity` (#378, P4-5). The copy is the ensure's
    /// guard: its destination bound is the store's capacity, so a range with no
    /// room is refused before anything is written, and the length is raised only
    /// after the copy has written every unit below it, in the same block with
    /// nothing between that could reach the vector. The unit is the vector's
    /// element — a member, or a `MapEntry` a map's entry is word for word — and
    /// the machine holds the source to being that run.
    fn core_extend_keyed(
        &mut self,
        expr: &Expr,
        out: &Expr,
        [run, from, count]: [&Expr; 3],
        want: Option<Dest>,
    ) -> Val {
        let Some(elem) = self.vector_element(out) else {
            return self.dead(expr);
        };
        let Some(ty) = self.settled_ty(run) else {
            return self.dead(expr);
        };
        if !matches!(ty, Ty::Set(_) | Ty::Map(..)) || self.layout(&ty, run.span).is_none() {
            return self.gap(
                "a keyed extend from something that is not a `Set` or a `Map`",
                expr,
            );
        }
        let owner = self.expr(out);
        let src = self.expr(run);
        let at = self.expr(from);
        let many = self.expr(count);
        let store = self.vector_store(owner.slot, expr.span);
        let len = self.temp(shapes::INT);
        self.emit(
            Inst::LoadField {
                dst: len.slot,
                obj: owner.slot,
                at: VECTOR_LEN,
                layout: shapes::INT,
            },
            expr.span,
        );
        let row = self.pool.args.intern(vec![
            store.arg(),
            len.arg(),
            src.arg(),
            at.arg(),
            many.arg(),
        ]);
        self.emit(
            Inst::RunCopy {
                args: row,
                storage: Storage::Words(elem),
            },
            expr.span,
        );
        self.emit(
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: len.slot,
                a: len.slot,
                b: many.slot,
            },
            expr.span,
        );
        self.emit(
            Inst::StoreField {
                obj: owner.slot,
                at: VECTOR_LEN,
                src: len.slot,
                layout: shapes::INT,
            },
            expr.span,
        );
        self.give_back(len.slot, len.layout);
        self.release(store, expr.span);
        self.release(many, expr.span);
        self.release(at, expr.span);
        self.release(src, expr.span);
        self.release(owner, expr.span);
        self.unit_answer(expr, want)
    }

    /// `core.setFinish(run)` and `core.mapFinish(run)`: the vector's store
    /// relabelled into the `Set` of its elements or the `Map` of its
    /// `MapEntry`s, and the vector consumed.
    ///
    /// One [`Inst::RunFinish`] over [`Storage::Words`] of the element, with
    /// [`Validation::None`] — [`Body::core_vector_finish`] with a keyed target,
    /// which `crate::verify` admits where the element is the set's member or
    /// the map's entry word for word. The run is ascending and distinct because
    /// the body built it so; nothing here sorts (ADR 0059).
    fn core_keyed_finish(&mut self, expr: &Expr, run: &Expr, want: Option<Dest>) -> Val {
        let Some(elem) = self.vector_element(run) else {
            return self.dead(expr);
        };
        let Some(ty) = self.settled_ty(expr) else {
            return self.dead(expr);
        };
        if !matches!(ty, Ty::Set(_) | Ty::Map(..)) {
            return self.gap("a keyed finish answering neither a `Set` nor a `Map`", expr);
        }
        let Some(target) = self.layout(&ty, expr.span) else {
            return self.dead(expr);
        };
        let owner = self.expr(run);
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

    /// The `()` a core intrinsic that writes no destination answers, in the
    /// location the surrounding form asked for.
    ///
    /// [`Body::unit_value`] with a destination, which is the whole difference:
    /// a push or an append is a statement in almost every program that writes
    /// one, and a `Unit` written into a temporary that is then copied into the
    /// location the answer belongs in is an instruction per append that nothing
    /// reads. `crates/cove-cli/tests/copies.rs` counts every one of those.
    pub(super) fn unit_answer(&mut self, expr: &Expr, want: Option<Dest>) -> Val {
        let dst = self.answer_at(want, shapes::UNIT);
        self.emit(Inst::Unit { dst: dst.slot }, expr.span);
        dst
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
