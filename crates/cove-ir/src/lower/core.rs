//! The standard library's core intrinsics, lowered to the instructions that
//! perform them.
//!
//! [ADR 0058](../../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md)
//! moves a collection's public algorithm into Cove and keeps beneath it only
//! the smallest representation-dependent operation, which the standard library
//! spells `core.<name>(...)` — `cove_schema::builtins::CORE_INTRINSICS` is the
//! table. The checker admits such a call only inside a standard-library module,
//! and this is the lowering's half: each entry becomes run instructions in the
//! frame the call is written in, and never an [`Inst::IntrinsicCall`] that names
//! a method — the keyed walks below are the one exception, and they name a
//! static intrinsic identity. A core intrinsic is not a name for the machine to
//! dispatch on; it is the operation the name stands for.
//!
//! The string builder is the one core type here with no public method of its
//! own. `std.stringbuilder`'s `StringBuilder` wraps a `ByteBuffer` — ADR 0052's
//! growable packed byte run — and its operations are the `core.bytes*`
//! intrinsics: a byte `growable-alloc`; ADR 0062's `growable-ensure`, a
//! `run-store` or `run-copy` into the store, and `growable-commit`, which
//! `appendByte`, `append` and `appendSlice` are written over; a `run-finish`
//! into `String`; and a field read of the owner's length word. The owner is a
//! reference, so nothing needs an address: a `var self` builder is a `var` slot
//! holding that reference, the body loads it, and every append writes *through*
//! it — which is why a growth that replaces the store beneath the owner is
//! visible to every frame naming the builder.
//!
//! A keyed collection's search is the one family here with runtime calls
//! beneath it (ADR 0059, #378 Phase 4). Its element reads are run instructions
//! like every other — `core.memberAt` and `core.entryAt` are a `load-elem` of
//! the sorted run itself — but the order a search steps by and the admission
//! of a key are layout-directed walks no instruction performs for a struct, an
//! array or a set. So `core.order` is one `cmp` of `CmpOp::Order` where the
//! key is a scalar, a `String` or a case index in name order, a call into the
//! walk `super::synth` composes out of the key's layout where the layout is
//! known, and a [`Inst::IntrinsicCall`] of `Intrinsic::ValueOrder` only where
//! the key is erased (ADR 0064, Decisions 3 and 4);
//! `core.admitKey` is nothing at all where the key's layout cannot hold a
//! refused part, a call into the walk `super::synth` composes out of it where
//! it can and the layout says which values, and an
//! `Intrinsic::ValueAdmitKey` where the key is erased or holds itself.
//! `core.refuseDuplicate` stood beside them until ADR 0067 gave the standard
//! library [`Inst::Trap`]'s three slots: a duplicate in `Set.of` or `Map.of` is
//! refused by `std.set` and `std.map` themselves, through `core.refuse`.
//!
//! `core.refuseByteRange` is a fourth of the same kind, and for the same
//! reason: `std.stringbuilder`'s `appendRange` decides a byte range in Cove and
//! has nothing to raise with, so ADR 0062 gives it
//! `Intrinsic::StringRefuseByteRange`, which never answers. Those four are the
//! only calls this file emits, and each is a static identity rather than a
//! name.
//!
//! So nothing downstream of this file learns that a public method moved. The
//! verifier, both encoders and the native code generators see run instructions
//! — a `len`, a word `growable-ensure` and `growable-commit` around a
//! `store-elem`, a `load-elem` or `store-elem` of a store, a
//! word `run-finish` into an `Array`, a `Set` or a `Map`, a word or byte
//! `run-slice`, a word `run-copy` out of a sorted run, a word `growable-truncate` — and never the
//! name of the method above
//! them; a function the standard library wraps around a single one of them is
//! small enough that `super::inline` expands it where it is called.

use cove_diag::Span;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Expr, ExprKind};

use super::frame::Val;
use super::shapes::{self, BUFFER_LEN, BUFFER_STORE, VECTOR_LEN, VECTOR_STORE};
use super::{synth, Body, Dest};
use crate::inst::{CmpOp, Inst, Len, Slot, Storage, Validation};
use crate::intrinsic::Intrinsic;
use crate::layout::LayoutId;
use crate::program::{Arg as Operand, IntrinsicSite};

/// Which half of ADR 0062's reservation [`Body::core_vector_reserve`] emits.
#[derive(Clone, Copy)]
enum Reserve {
    Ensure,
    Commit,
}

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
            (
                "vectorEnsure" | "vectorStore" | "vectorCommit" | "vectorCopyFromSet"
                | "vectorCopyFromMap" | "bytesEnsure" | "bytesStore" | "bytesCopy" | "bytesCommit",
                _,
            ) => {
                if self.core_statement(expr, name, args) {
                    self.unit_answer(expr, want)
                } else {
                    self.dead(expr)
                }
            }
            ("vectorLoad", [items, index]) => {
                self.core_vector_load(expr, &items.value, &index.value, want)
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
            ("stringFind", [text, needle, from]) => {
                self.core_string_find(expr, &text.value, &needle.value, &from.value, want)
            }
            ("bytesAllocate", [capacity]) => self.core_bytes_allocate(expr, &capacity.value, want),
            ("refuseByteRange", [text, from, to]) => {
                self.core_refuse_byte_range(expr, &text.value, [&from.value, &to.value], want)
            }
            ("refuse", [message, rule, help]) => {
                self.core_refuse(expr, [&message.value, &rule.value, &help.value], want)
            }
            ("bytesFinish", [buffer]) => self.core_bytes_finish(expr, &buffer.value, want),
            ("bytesLength", [buffer]) => self.core_bytes_length(expr, &buffer.value, want),
            ("arrayLength", [items]) => self.core_array_length(expr, &items.value, want),
            ("vectorLength", [items]) => self.core_vector_length(expr, &items.value, want),
            ("order", [a, b]) => self.core_order(expr, &a.value, &b.value, want),
            ("admitKey", [key, method, role]) => {
                self.core_admit_key(expr, &key.value, [&method.value, &role.value], want)
            }
            ("memberAt", [members, at]) => {
                self.core_member_at(expr, &members.value, &at.value, want)
            }
            ("entryAt", [entries, at]) => self.core_entry_at(expr, &entries.value, &at.value, want),
            ("vectorWithCapacity", [capacity]) => {
                self.core_vector_with_capacity(expr, &capacity.value, want)
            }
            ("setFinish" | "mapFinish", [run]) => self.core_keyed_finish(expr, &run.value, want),
            ("setSlice", [items, from, count]) => {
                self.core_set_slice(expr, &items.value, &from.value, &count.value, want)
            }
            ("dynamicOpen", [value]) => self.core_dynamic_open(expr, &value.value, want),
            ("dynamicKind", [view]) => self.core_dynamic_observe(
                expr,
                &view.value,
                shapes::INT,
                |dst, view| Inst::DynKind { dst, view },
                want,
            ),
            ("dynamicSameType", [a, b]) => self.core_dynamic_pair(
                expr,
                &a.value,
                &b.value,
                |dst, a, b| Inst::DynSameType { dst, a, b },
                want,
            ),
            ("dynamicSameObject", [a, b]) => self.core_dynamic_pair(
                expr,
                &a.value,
                &b.value,
                |dst, a, b| Inst::DynSameObject { dst, a, b },
                want,
            ),
            ("dynamicBool", [view]) => {
                self.core_dynamic_read(expr, &view.value, shapes::BOOL, want)
            }
            ("dynamicInt", [view]) => self.core_dynamic_read(expr, &view.value, shapes::INT, want),
            ("dynamicFloat", [view]) => {
                self.core_dynamic_read(expr, &view.value, shapes::FLOAT, want)
            }
            ("dynamicDuration", [view]) => {
                self.core_dynamic_read(expr, &view.value, shapes::DURATION, want)
            }
            ("dynamicString", [view]) => {
                self.core_dynamic_read(expr, &view.value, shapes::STR, want)
            }
            ("dynamicCase", [view]) => self.core_dynamic_observe(
                expr,
                &view.value,
                shapes::INT,
                |dst, view| Inst::DynCase { dst, view },
                want,
            ),
            ("dynamicChildCount", [view]) => self.core_dynamic_observe(
                expr,
                &view.value,
                shapes::INT,
                |dst, view| Inst::DynCount { dst, view },
                want,
            ),
            ("dynamicChild", [view, index]) => {
                self.core_dynamic_child(expr, &view.value, &index.value, want)
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

    /// Whether `expr` is a call of one of ADR 0062's protocol statements —
    /// `core.vectorEnsure`, `core.vectorStore`, `core.vectorCommit`, and their
    /// byte members `core.bytesEnsure`, `core.bytesStore`, `core.bytesCopy` and
    /// `core.bytesCommit` — written where nothing reads its answer, and if it
    /// is, its instructions.
    ///
    /// Asked by [`Body::discard`], so that a statement costs no `()`. That is
    /// not only a dispatch saved: [`crate::legalize`]'s windows admit nothing
    /// between the ensure and the store read, or between the write and the
    /// commit, that is not a constant or a clear, so a `unit` after either of
    /// the first two would leave `std.vector.push` a run of primitives that no
    /// backend treats as one step. The questions are
    /// [`Body::call_qualified`]'s, in its order, over the source's shape: the
    /// call is `core.<name>(...)`, in a standard-library module, with `core`
    /// no local's name.
    pub(super) fn discarded_core_statement(&mut self, expr: &Expr) -> bool {
        let ExprKind::Call {
            callee,
            args,
            trailing: None,
            ..
        } = &expr.kind
        else {
            return false;
        };
        let ExprKind::Field { base, name } = &callee.kind else {
            return false;
        };
        let ExprKind::Ident(head) = &base.kind else {
            return false;
        };
        if head != cove_schema::builtins::CORE_NAMESPACE
            || !cove_sema::stdlib::is_library_module(self.module)
            || self.frame.lookup(head).is_some()
            || !matches!(
                name.node.as_str(),
                "vectorEnsure"
                    | "vectorStore"
                    | "vectorCommit"
                    | "bytesEnsure"
                    | "bytesStore"
                    | "bytesCopy"
                    | "bytesCommit"
            )
        {
            return false;
        }
        self.core_statement(expr, &name.node, args);
        true
    }

    /// A protocol statement's instructions, and nothing for its `()`: whoever
    /// reads the answer writes it. `false` where the call could not be lowered,
    /// which has already been reported.
    fn core_statement(&mut self, expr: &Expr, name: &str, args: &[Arg]) -> bool {
        match (name, args) {
            ("vectorEnsure", [items, additional]) => {
                self.core_vector_reserve(expr, &items.value, &additional.value, Reserve::Ensure)
            }
            ("vectorCommit", [items, count]) => {
                self.core_vector_reserve(expr, &items.value, &count.value, Reserve::Commit)
            }
            ("vectorStore", [items, index, value]) => {
                self.core_vector_store(expr, &items.value, &index.value, &value.value)
            }
            ("bytesEnsure", [buffer, additional]) => self.reserve(
                expr,
                &buffer.value,
                &additional.value,
                Storage::PackedBytes,
                Reserve::Ensure,
            ),
            ("bytesCommit", [buffer, count]) => self.reserve(
                expr,
                &buffer.value,
                &count.value,
                Storage::PackedBytes,
                Reserve::Commit,
            ),
            ("bytesStore", [buffer, at, byte]) => {
                self.core_bytes_store(expr, &buffer.value, &at.value, &byte.value)
            }
            ("bytesCopy", [buffer, at, text, from, count]) => self.core_bytes_copy(
                expr,
                &buffer.value,
                [&at.value, &text.value, &from.value, &count.value],
            ),
            ("vectorCopyFromSet" | "vectorCopyFromMap", [out, at, run, from, count]) => self
                .core_vector_copy_from_keyed(
                    expr,
                    &out.value,
                    [&at.value, &run.value, &from.value, &count.value],
                ),
            _ => {
                self.gap(&format!("`core.{name}`"), expr);
                false
            }
        }
    }

    /// `core.vectorEnsure(items, additional)` or `core.vectorCommit(items,
    /// count)`: one [`Inst::GrowableEnsure`] or [`Inst::GrowableCommit`] over
    /// [`Storage::Words`] of the element's layout.
    ///
    /// [ADR 0062](../../../../docs/adr/0062-an-append-is-ensure-store-commit.md)'s
    /// two halves of an append. Neither writes a slot. `std.vector.push` is
    /// `let at = core.vectorLength(items)`, an ensure of one, the
    /// [`Body::core_vector_store`] at `at`, and a commit of one, and the
    /// constants are lowered as they are written — an `int` into a temporary
    /// just before each — which is the shape [`crate::legalize`] recognises.
    fn core_vector_reserve(
        &mut self,
        expr: &Expr,
        items: &Expr,
        count: &Expr,
        which: Reserve,
    ) -> bool {
        let Some(elem) = self.vector_element(items) else {
            return false;
        };
        self.reserve(expr, items, count, Storage::Words(elem), which)
    }

    /// One half of a reservation over `storage`: [`Body::core_vector_reserve`]
    /// for a vector, and `core.bytesEnsure` or `core.bytesCommit` for a byte
    /// buffer, which have no element to find first.
    fn reserve(
        &mut self,
        expr: &Expr,
        owner: &Expr,
        count: &Expr,
        storage: Storage,
        which: Reserve,
    ) -> bool {
        let owner = self.expr(owner);
        let many = self.expr(count);
        let inst = match which {
            Reserve::Ensure => Inst::GrowableEnsure {
                owner: owner.slot,
                additional: many.slot,
                storage,
            },
            Reserve::Commit => Inst::GrowableCommit {
                owner: owner.slot,
                count: many.slot,
                storage,
            },
        };
        self.emit(inst, expr.span);
        // An ensure's count is not given back. `crate::verify`'s reservation
        // rule refuses a write to it before the commit, and the commit's own
        // constant is the next `Int` temporary asked for — which, handed the
        // same slot, would be exactly that write. An `Int` holds nothing to
        // clear, so keeping it is one frame word per written ensure.
        if !matches!(which, Reserve::Ensure) {
            self.release(many, expr.span);
        }
        self.release(owner, expr.span);
        true
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
    /// [`Inst::LoadField`] of the store and [`Inst::StoreElem`] into it, and no
    /// `()`: [`Body::core_statement`] writes one only where it is read. Bounded
    /// as [`Body::core_vector_load`] is, by the capacity, and so a vector write
    /// only where the caller held `index` below the length first — or, in a
    /// push, at the length itself, into the room an ensure made.
    fn core_vector_store(&mut self, expr: &Expr, items: &Expr, index: &Expr, value: &Expr) -> bool {
        let Some(elem) = self.vector_element(items) else {
            return false;
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
        true
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
    /// for it. `len` is the body's to have computed from the length it read; a
    /// `len` above it is refused.
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
    /// It is not a word `growable-alloc` and an append window, which would
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

    /// `core.stringFind(text, needle, from)`: the first byte offset at or
    /// after `from` where `needle` occurs in `text`, or -1.
    ///
    /// One [`Inst::RunFind`] over [`Storage::PackedBytes`], whose row's `dst`
    /// is [`shapes::INT`] — the answer is an offset and not a run, which is
    /// the one way this row differs from [`Inst::RunSlice`]'s. Both lengths
    /// come from the two objects' headers, so the row is four operands and
    /// carries no count.
    ///
    /// `std.string.contains` is the only caller today and passes zero for
    /// `from`, which is why the instruction may treat a `from` outside the
    /// text as a broken invariant rather than as an answer.
    fn core_string_find(
        &mut self,
        expr: &Expr,
        text: &Expr,
        needle: &Expr,
        from: &Expr,
        want: Option<Dest>,
    ) -> Val {
        let haystack = self.expr(text);
        let sought = self.expr(needle);
        let at = self.expr(from);
        let dst = self.answer_at(want, shapes::INT);
        let row = self.pool.args.intern(vec![
            Operand {
                slot: dst.slot,
                layout: shapes::INT,
            },
            haystack.arg(),
            sought.arg(),
            at.arg(),
        ]);
        self.emit(
            Inst::RunFind {
                args: row,
                storage: Storage::PackedBytes,
            },
            expr.span,
        );
        self.release(at, expr.span);
        self.release(sought, expr.span);
        self.release(haystack, expr.span);
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

    /// `core.bytesStore(buffer, at, byte)`: the buffer's store, and one byte
    /// [`Inst::RunStore`] into it at `at`.
    ///
    /// [`Body::core_vector_store`] for a byte run, and for the same reason
    /// written as a statement with no `()`: `std.stringbuilder`'s
    /// `appendByteInto` is a length read, an ensure of one, this at that length
    /// and a commit of one, which is [`crate::legalize`]'s byte push. Bounded by
    /// the store's capacity, so a byte write only into the room an ensure made;
    /// a value that is not a byte stops the run in the instruction's words.
    fn core_bytes_store(&mut self, expr: &Expr, buffer: &Expr, at: &Expr, byte: &Expr) -> bool {
        let owner = self.expr(buffer);
        let index = self.expr(at);
        let src = self.expr(byte);
        let store = self.buffer_store(owner.slot, expr.span);
        self.emit(
            Inst::RunStore {
                run: store.slot,
                index: index.slot,
                src: src.slot,
                storage: Storage::PackedBytes,
            },
            expr.span,
        );
        self.release(store, expr.span);
        self.release(src, expr.span);
        self.release(index, expr.span);
        self.release(owner, expr.span);
        true
    }

    /// `core.bytesCopy(buffer, at, text, from, count)`: the buffer's store, and
    /// one byte [`Inst::RunCopy`] of `count` bytes of `text` from `from` into it
    /// at `at`.
    ///
    /// The row is the copy's own five operands, `[store, at, text, from,
    /// count]`, and the store is read after every operand is evaluated, so
    /// that nothing but a constant lands between it and the copy: `appendText`
    /// is a length read, an ensure of the text's length, this at the length and
    /// from `0`, and a commit of the same count, which is
    /// [`crate::legalize`]'s byte append. The copy's bounds are the store's
    /// capacity and the text's length; a whole string's ends are character
    /// boundaries, so it asks nothing about them.
    fn core_bytes_copy(
        &mut self,
        expr: &Expr,
        buffer: &Expr,
        [at, text, from, count]: [&Expr; 4],
    ) -> bool {
        let owner = self.expr(buffer);
        let index = self.expr(at);
        let src = self.expr(text);
        let start = self.expr(from);
        let many = self.expr(count);
        let store = self.buffer_store(owner.slot, expr.span);
        let row = self.pool.args.intern(vec![
            store.arg(),
            index.arg(),
            src.arg(),
            start.arg(),
            many.arg(),
        ]);
        self.emit(
            Inst::RunCopy {
                args: row,
                storage: Storage::PackedBytes,
            },
            expr.span,
        );
        self.release(store, expr.span);
        self.release(many, expr.span);
        self.release(start, expr.span);
        self.release(src, expr.span);
        self.release(index, expr.span);
        self.release(owner, expr.span);
        true
    }

    /// The store a buffer's bytes are in, held in a reference slot of its own
    /// for [`Body::vector_store`]'s reason.
    fn buffer_store(&mut self, owner: Slot, span: Span) -> Val {
        let store = self.temp(shapes::REF);
        self.emit(
            Inst::LoadField {
                dst: store.slot,
                obj: owner,
                at: BUFFER_STORE,
                layout: shapes::REF,
            },
            span,
        );
        store
    }

    /// `core.refuseByteRange(text, from, to)`: one [`Inst::IntrinsicCall`] of
    /// [`Intrinsic::StringRefuseByteRange`], which always raises.
    ///
    /// [`Body::keyed_refusal`]'s shape over a receiver and two offsets instead
    /// of a key and two names. ADR 0062 takes the range policy out of the copy:
    /// `std.stringbuilder`'s `appendRange` asks `String.sliceBytes`' five
    /// questions in Cove and reaches this only when one of them has already
    /// failed, so the sentence is written once, here, and the copy beneath it
    /// validates nothing and can be the write half of a reservation window.
    fn core_refuse_byte_range(
        &mut self,
        expr: &Expr,
        text: &Expr,
        [from, to]: [&Expr; 2],
        want: Option<Dest>,
    ) -> Val {
        let src = self.expr(text);
        let start = self.expr(from);
        let end = self.expr(to);
        let dst = self.answer_at(want, shapes::UNIT);
        self.intrinsic_call(
            Intrinsic::StringRefuseByteRange,
            shapes::UNIT,
            dst.slot,
            &[&src, &start, &end],
            expr.span,
        );
        self.release(end, expr.span);
        self.release(start, expr.span);
        self.release(src, expr.span);
        dst
    }

    /// `core.refuse(message, rule, help)`: one [`Inst::Trap`], whose three
    /// slots are exactly these three arguments.
    ///
    /// [`Body::core_refuse_byte_range`]'s doc above says which of the five
    /// things is wrong in `String.sliceBytes`'s words — that `appendRange`
    /// has nothing to raise with was the standard library's problem in
    /// general, not only there, and this is what answers it: any body may
    /// build the three sentences of a refusal at run time out of values it
    /// computed and stop the run with them, which is
    /// [issue 461](https://github.com/myuon/cove/issues/461).
    ///
    /// Unlike `core_refuse_byte_range`'s [`Inst::IntrinsicCall`], a `Trap` is
    /// a terminator — nothing runs after it, ever, so nothing is emitted
    /// after it either. `dst` is answered the same way a diverging `return`
    /// or `break` answers one, a location nothing will write, so the
    /// surrounding form still has something to hold.
    fn core_refuse(
        &mut self,
        expr: &Expr,
        [message, rule, help]: [&Expr; 3],
        want: Option<Dest>,
    ) -> Val {
        let message = self.expr(message);
        let rule = self.expr(rule);
        let help = self.expr(help);
        let dst = self.answer_at(want, shapes::UNIT);
        self.emit(
            Inst::Trap {
                message: message.slot,
                rule: rule.slot,
                help: help.slot,
            },
            expr.span,
        );
        self.release(help, expr.span);
        self.release(rule, expr.span);
        self.release(message, expr.span);
        dst
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
    /// [`synth::ordered_by`] — one [`Inst::Call`] into the function
    /// `super::synth` composes out of the layout where the layout is known
    /// and wider than that, and one [`Inst::IntrinsicCall`] of
    /// [`Intrinsic::ValueOrder`] where the key is a box, which is ADR 0064's
    /// Decision 4 and the only dynamic layout left.
    ///
    /// **The first of the three is the one to be careful with.** It is what
    /// makes `cq` — every one of whose map keys is a `String` — execute
    /// `Value.order` at no site and on no turn, and a synthesis that
    /// displaced it with a call into a one-instruction function would be a
    /// regression on the one program in this repository that exercises the
    /// path at all. So it is asked first, and it is asked through the same
    /// function the walk itself asks.
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
        match synth::ordered_by(&self.pool.shapes, layout) {
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
            None if self.is_boxed(layout) => self.intrinsic_call(
                Intrinsic::ValueOrder,
                shapes::INT,
                dst.slot,
                &[&left, &right],
                expr.span,
            ),
            None => {
                let decls = self.plan.decls.len();
                let callee = synth::function_for(
                    synth::Operation::Order,
                    layout,
                    self.pool,
                    decls,
                    expr.span,
                );
                let args = self.pool.args.intern(vec![left.arg(), right.arg()]);
                self.emit(
                    Inst::Call {
                        dst: dst.slot,
                        callee,
                        args,
                    },
                    expr.span,
                );
            }
        }
        self.release(right, expr.span);
        self.release(left, expr.span);
        dst
    }

    /// `core.admitKey(key, method, role)`: the refusal of a key the language
    /// does not admit, in `method`'s words.
    ///
    /// Three ways, and [`synth::admission`] is the one question that decides
    /// between them — asked here and inside the walk, of one table, for
    /// [`synth::ordered_by`]'s reason.
    ///
    /// - [`synth::Admission::Always`]: **nothing at all.** No value of the
    ///   layout is ever refused, so there is nothing to ask, and the call
    ///   answers a `()` only if something reads one. Every key `covefmt` and
    ///   `cq` use is a `String` or an `Int`, so this arm is why neither
    ///   program reaches `Value.admitKey` at a single site.
    /// - [`synth::Admission::Decided`] of a composite: one [`Inst::Call`] of
    ///   the walk `super::synth` composes out of the layout (ADR 0064,
    ///   Decision 3), which reads whatever decides and hands the key on only
    ///   where the answer is that it is refused.
    /// - otherwise one [`Inst::IntrinsicCall`] of
    ///   [`Intrinsic::ValueAdmitKey`] over the key and the two names: a box,
    ///   a layout that holds itself, and the scalars and handles the language
    ///   refuses outright, which are one value and not a walk.
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
        let admission = synth::admission(self.pool.shapes.all(), layout);
        if admission == synth::Admission::Always {
            let held = self.expr(key);
            self.release(held, expr.span);
            return match want {
                Some(_) => self.unit_answer(expr, want),
                None => self.temp(shapes::UNIT),
            };
        }
        let shape = self.pool.shapes.layout(layout).shape.clone();
        if admission == synth::Admission::Decided
            && synth::walks(synth::Operation::Admission, &shape)
        {
            return self.admit_by_walk(expr, key, [method, role], layout, want);
        }
        self.keyed_refusal(Intrinsic::ValueAdmitKey, expr, key, [method, role], want)
    }

    /// The walk `super::synth` composes out of `layout`, and the intrinsic
    /// under the one branch its answer decides.
    ///
    /// ```text
    ///   call     refused, refuses<Mark#16>(key)
    ///   branch-false refused -> past
    ///   intrinsic-call Value.admitKey(key, method, role)
    /// past:
    /// ```
    ///
    /// The intrinsic stays **here**, at the site, in this frame, over this
    /// key — which is where it was before this migration and is the whole of
    /// why the diagnostic does not move. A refusal names the path from the
    /// key to the part that is wrong and is blamed on the caller by reading
    /// the live frames (ADR 0058); a fallback raised from inside the walk
    /// would have added a frame and a second `in the standard library` label
    /// pointing at the line the first already pointed at. So the walk answers
    /// a bit and this decides what to do with it.
    ///
    /// The answer's sense is `true` for "ask the runtime", because the
    /// instruction set has a `branch-false` and no `branch-true`, and the
    /// `()` is written before the branch so that both paths leave it written.
    fn admit_by_walk(
        &mut self,
        expr: &Expr,
        key: &Expr,
        [method, role]: [&Expr; 2],
        layout: LayoutId,
        want: Option<Dest>,
    ) -> Val {
        let held = self.expr(key);
        let method = self.expr(method);
        let role = self.expr(role);
        let dst = self.answer_at(want, shapes::UNIT);
        self.emit(Inst::Unit { dst: dst.slot }, expr.span);
        let decls = self.plan.decls.len();
        let callee = synth::function_for(
            synth::Operation::Admission,
            layout,
            self.pool,
            decls,
            expr.span,
        );
        let refused = self.temp(shapes::BOOL);
        let args = self.pool.args.intern(vec![held.arg()]);
        self.emit(
            Inst::Call {
                dst: refused.slot,
                callee,
                args,
            },
            expr.span,
        );
        let branch = self.emit(
            Inst::BranchFalse {
                cond: refused.slot,
                to: super::PENDING,
            },
            expr.span,
        );
        self.intrinsic_call(
            Intrinsic::ValueAdmitKey,
            shapes::UNIT,
            dst.slot,
            &[&held, &method, &role],
            expr.span,
        );
        let past = self.here();
        self.patch(branch, past);
        self.release(refused, expr.span);
        self.release(role, expr.span);
        self.release(method, expr.span);
        self.release(held, expr.span);
        dst
    }

    /// One [`Inst::IntrinsicCall`] of a keyed refusal over a key and the two
    /// names its message is written with, answering `()`.
    ///
    /// `Value.admitKey`'s, and once `Value.refuseDuplicate`'s too.
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

    /// One [`Inst::IntrinsicCall`] of `intrinsic` over `args`, answering a
    /// value of `result` into `dst`.
    pub(super) fn intrinsic_call(
        &mut self,
        intrinsic: Intrinsic,
        result: LayoutId,
        dst: Slot,
        args: &[&Val],
        span: Span,
    ) {
        let site = self
            .pool
            .intrinsic_site(IntrinsicSite { intrinsic, result });
        let args = self
            .pool
            .args
            .intern(args.iter().map(|arg| arg.arg()).collect());
        self.emit(Inst::IntrinsicCall { dst, site, args }, span);
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

    /// `core.setSlice(items, from, count)`: a fresh `Array` of the `count`
    /// members of `items` from `from`.
    ///
    /// One [`Inst::RunSlice`] whose source is the set itself — its run of
    /// members, which the machine reads at the member's stride — answering the
    /// `Array<T>` layout, declared here by asking for it. `std.set.toArray`
    /// asks for the whole set, so there is no range policy above it.
    fn core_set_slice(
        &mut self,
        expr: &Expr,
        items: &Expr,
        from: &Expr,
        count: &Expr,
        want: Option<Dest>,
    ) -> Val {
        let Some(ty) = self.settled_ty(items) else {
            return self.dead(expr);
        };
        let Ty::Set(of) = ty.clone() else {
            return self.gap("`core.setSlice` over something that is not a `Set`", expr);
        };
        let (Some(_), Some(elem), Some(target)) = (
            self.layout(&ty, items.span),
            self.layout(&of, items.span),
            self.layout(&Ty::Array(of), expr.span),
        ) else {
            return self.dead(expr);
        };
        let src = self.expr(items);
        let at = self.expr(from);
        let many = self.expr(count);
        let dst = self.answer_at(want, target);
        self.run_slice_words(dst.slot, target, elem, &src, &at, &many, expr.span);
        self.release(many, expr.span);
        self.release(at, expr.span);
        self.release(src, expr.span);
        dst
    }

    /// `core.vectorWithCapacity(capacity)`: an empty word [`Inst::GrowableAlloc`].
    ///
    /// [`Body::core_bytes_allocate`] over words, and the same one instruction:
    /// the owner and the store are both derived from the element the storage
    /// names — the program's [`crate::Shape::Vector`] of it and the growable
    /// [`crate::Shape::Elements`] of it — so there is nothing for the call to
    /// say beyond the capacity and the element.
    ///
    /// It was five instructions until then: an [`Inst::Alloc`] of the store at
    /// the capacity, an `Int` nought, the header's [`Inst::Alloc`], and its
    /// length and store fields. The last of those is why this changed and not
    /// only how much it costs. A plain [`Inst::StoreField`] at a growable
    /// owner's length offset is the instruction
    /// [ADR 0062](../../../../docs/adr/0062-an-append-is-ensure-store-commit.md)
    /// spent a stage removing from every appending body, and a construction
    /// that writes a constant nought there is harmless only because the
    /// constant is nought — a rule a reader has to check rather than one the
    /// instruction set states. There is now no field store at that offset
    /// anywhere in the standard library, which
    /// `no_field_store_publishes_a_growable_owner_s_length` holds.
    ///
    /// The capacity is still allocated exactly, and that is worth saying
    /// because the byte member does not: `Machine::alloc_buffer` raises a small
    /// capacity to a floor and `Machine::alloc_vector` does not, which is the
    /// two floors' own contract — a byte store below eight buys nothing because
    /// eight bytes are one word, and an element floor is the floor of the first
    /// *growth*. This intrinsic's callers are `std.map` and `std.set` sizing an
    /// output vector to the elements they are about to push into it, so a floor
    /// here is spare room nothing fills: it was measured at 666,740 allocated
    /// words on cq, 6.94% of the run's, for no growth avoided.
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
        // Interned although the instruction does not name it: the machine finds
        // the store by the element, out of the program's layout table, so a
        // program whose only vector of this element is built here still has to
        // declare the shape of the run it is built in.
        self.pool.shapes.store_of(element);
        let room = self.expr(capacity);
        let dst = self.answer_at(want, vector);
        self.emit(
            Inst::GrowableAlloc {
                dst: dst.slot,
                capacity: room.slot,
                storage: Storage::Words(element),
            },
            expr.span,
        );
        self.release(room, expr.span);
        dst
    }

    /// `core.vectorCopyFromSet(out, at, items, from, count)` and
    /// `core.vectorCopyFromMap(out, at, entries, from, count)`: `count` units of
    /// a sorted run from `from`, written into `out`'s store at `at`.
    ///
    /// [`Body::core_bytes_copy`] over words: the store's [`Inst::LoadField`] and
    /// one word [`Inst::RunCopy`] of `[store, at, src, from, count]`, with the
    /// store read after every operand so that only a constant can land between
    /// it and the copy. **It publishes nothing.**
    ///
    /// This replaced `core.extendFromSet`, whose instructions were the store
    /// and the length read, the copy, an `Int` add and the length written back
    /// with a plain [`Inst::StoreField`] — an unrestricted store to a growable
    /// owner's length word, which
    /// [ADR 0062](../../../../docs/adr/0062-an-append-is-ensure-store-commit.md)
    /// forbids, and which no backend could run as one step. What `std.map` and
    /// `std.set` write around this now is the length, an ensure, this and a
    /// commit, which is the append window every other appending body in the
    /// standard library is.
    ///
    /// The bound is the copy's own: its destination is the *store*, so the
    /// range is held to the capacity, and a body that did not reserve the room
    /// is refused with nothing written and the length unchanged. The unit is
    /// the vector's element — a member, or a `MapEntry` a map's entry is word
    /// for word — and the machine holds the source to being that run.
    fn core_vector_copy_from_keyed(
        &mut self,
        expr: &Expr,
        out: &Expr,
        [at, run, from, count]: [&Expr; 4],
    ) -> bool {
        let Some(elem) = self.vector_element(out) else {
            return false;
        };
        let Some(ty) = self.settled_ty(run) else {
            return false;
        };
        if !matches!(ty, Ty::Set(_) | Ty::Map(..)) || self.layout(&ty, run.span).is_none() {
            self.gap(
                "a keyed copy from something that is not a `Set` or a `Map`",
                expr,
            );
            return false;
        }
        let owner = self.expr(out);
        let index = self.expr(at);
        let src = self.expr(run);
        let start = self.expr(from);
        let many = self.expr(count);
        let store = self.vector_store(owner.slot, expr.span);
        let row = self.pool.args.intern(vec![
            store.arg(),
            index.arg(),
            src.arg(),
            start.arg(),
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
        self.release(start, expr.span);
        self.release(src, expr.span);
        self.release(index, expr.span);
        self.release(owner, expr.span);
        true
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

    /// `core.dynamicOpen(value)`: [`Inst::DynOpen`] of the box `value` is.
    ///
    /// **Only a box.** ADR 0068's Decision 5 keeps every statically known
    /// layout on its synthesized fast path, so a call over anything whose
    /// layout this lowering knows is a gap here rather than a reflection:
    /// opening it would route a known layout through the dynamic view, which
    /// is the structural regression the ADR's gate names. A value whose type
    /// is erased — `dyn Trait`, a Host `Any` — is one reference word to a box,
    /// and that is the only operand the instruction takes.
    fn core_dynamic_open(&mut self, expr: &Expr, value: &Expr, want: Option<Dest>) -> Val {
        let Some(ty) = self.settled_ty(value) else {
            return self.dead(expr);
        };
        let Some(layout) = self.layout(&ty, value.span) else {
            return self.dead(expr);
        };
        if !self.is_boxed(layout) {
            return self.gap(
                &format!(
                    "`core.dynamicOpen` of a `{ty}`, whose layout is known — ADR 0068 reflects \
                     only on an erased value"
                ),
                expr,
            );
        }
        let src = self.expr(value);
        let dst = self.answer_at(want, shapes::DYNAMIC_VIEW);
        self.emit(
            Inst::DynOpen {
                dst: dst.slot,
                src: src.slot,
            },
            expr.span,
        );
        self.release(src, expr.span);
        dst
    }

    /// One of ADR 0068's observations that reads one view and writes one word
    /// of `answer`: `core.dynamicKind`, `core.dynamicCase` and
    /// `core.dynamicChildCount`.
    fn core_dynamic_observe(
        &mut self,
        expr: &Expr,
        view: &Expr,
        answer: LayoutId,
        inst: fn(Slot, Slot) -> Inst,
        want: Option<Dest>,
    ) -> Val {
        let held = self.expr(view);
        let dst = self.answer_at(want, answer);
        self.emit(inst(dst.slot, held.slot), expr.span);
        self.release(held, expr.span);
        dst
    }

    /// `core.dynamicBool`, `core.dynamicInt`, `core.dynamicFloat`,
    /// `core.dynamicDuration` and `core.dynamicString`: one [`Inst::DynRead`]
    /// into a word of `answer`, whose `Repr` is which of the five it is.
    fn core_dynamic_read(
        &mut self,
        expr: &Expr,
        view: &Expr,
        answer: LayoutId,
        want: Option<Dest>,
    ) -> Val {
        self.core_dynamic_observe(
            expr,
            view,
            answer,
            |dst, view| Inst::DynRead { dst, view },
            want,
        )
    }

    /// `core.dynamicSameType(a, b)` and `core.dynamicSameObject(a, b)`: one
    /// [`Inst::DynSameType`] or [`Inst::DynSameObject`] over two views,
    /// answering a `Bool`.
    fn core_dynamic_pair(
        &mut self,
        expr: &Expr,
        a: &Expr,
        b: &Expr,
        inst: impl FnOnce(Slot, Slot, Slot) -> Inst,
        want: Option<Dest>,
    ) -> Val {
        let left = self.expr(a);
        let right = self.expr(b);
        let dst = self.answer_at(want, shapes::BOOL);
        self.emit(inst(dst.slot, left.slot, right.slot), expr.span);
        self.release(right, expr.span);
        self.release(left, expr.span);
        dst
    }

    /// `core.dynamicChild(view, index)`: one [`Inst::DynChild`], answering a
    /// view in the location the surrounding form asked for.
    ///
    /// The answer may be the same location as the view it is projected from
    /// — `view = core.dynamicChild(view, 0)` is how a walk descends — and the
    /// machine reads the whole parent before it writes the child, so that is
    /// not a hazard to arrange around here.
    fn core_dynamic_child(
        &mut self,
        expr: &Expr,
        view: &Expr,
        index: &Expr,
        want: Option<Dest>,
    ) -> Val {
        let parent = self.expr(view);
        let at = self.expr(index);
        let dst = self.answer_at(want, shapes::DYNAMIC_VIEW);
        self.emit(
            Inst::DynChild {
                dst: dst.slot,
                view: parent.slot,
                index: at.slot,
            },
            expr.span,
        );
        self.release(at, expr.span);
        self.release(parent, expr.span);
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
