//! Collections, ranges, and the loop that walks them.
//!
//! Five families of value and one form: an `Array`, a `Vector`, a `Set`, a
//! `Map`, a `Range`, and the `for` that iterates any of them.
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
//! walk over it costs nothing extra; a `Set` and a `Map` are immutable for
//! the same reason — `inserted` and `removed` answer new objects — so neither
//! is copied either. A `Vector` can be pushed to from inside the body it is
//! being walked by, so the loop takes a copy first — one
//! [`Inst::CallBuiltin`] of `Vector.toArray`, which is the same copy
//! `items_of` makes when it clones the elements out.
//!
//! # A range yields what it was written to yield, and never traps doing it
//!
//! An inclusive range is not turned into an exclusive one with a larger end.
//! `0...Int.MAX` has no exclusive equivalent — the end would be `Int.MAX + 1`
//! — and normalising it that way made the loop trap on an overflow the
//! language does not have. So `inclusive` is kept and the *comparison* is
//! chosen instead: a turn happens at `index` when `index < end`, or when
//! `index == end` and the range was written inclusive. Nothing adjusts a
//! bound, and the step is emitted only where `index < end` is already known,
//! so the counter never passes the end either.
//!
//! # The element binding is one location, and it is cleared
//!
//! A loop binds one name per turn, and the value behind it holds a reference
//! whenever the elements do. One location holds it for every turn, and it is
//! cleared at the end of each turn — so a walk over a large array holds one
//! element at a time rather than every element it has reached. That is the
//! invariant the whole design was reviewed for, and it is why the `Clear` is
//! emitted at the end of the body rather than left to the next turn's
//! overwrite.

use cove_diag::Span;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Block, Expr, Ident};

use super::frame::Val;
use super::gap;
use super::shapes::{self, RANGE_END, RANGE_INCLUSIVE, RANGE_START, VECTOR_LEN, VECTOR_STORE};
use super::{Body, Dest, Loop, PENDING};
use crate::inst::{ArithOp, CmpOp, Compare, Inst, Len, Num, Pc, Slot};
use crate::layout::LayoutId;
use crate::program::Builtin;

impl Body<'_> {
    // ---- literals ---------------------------------------------------------

    /// `[a, b, c]`: an object of the right length, then one store per
    /// element.
    ///
    /// The elements are in the object rather than behind an indirection,
    /// because an `Array` cannot grow and so needs none of what an
    /// indirection buys. A multiword element is inline there too: the stride
    /// is the element layout's width, so an `Array<Point>` is a run of
    /// two-word elements rather than a run of addresses.
    pub(super) fn array_literal(&mut self, expr: &Expr, items: &[Expr]) -> Val {
        let Some(ty) = self.settled_ty(expr) else {
            return self.dead(expr);
        };
        let Ty::Array(element) = ty.clone() else {
            return self.gap("an array literal of something that is not an `Array`", expr);
        };
        let (Some(layout), Some(elem)) = (
            self.layout(&ty, expr.span),
            self.layout(&element, expr.span),
        ) else {
            return self.dead(expr);
        };

        let mut held = Vec::with_capacity(items.len());
        for item in items {
            held.push(self.expr(item));
        }
        let dst = self.temp(shapes::REF);
        self.emit(
            Inst::Alloc {
                dst: dst.slot,
                layout,
                len: Len::Count(items.len() as u32),
            },
            expr.span,
        );
        // One index location written again rather than one per element: the
        // frame should not grow with the length of a literal.
        let index = self.temp(shapes::INT);
        for (at, value) in held.iter().enumerate() {
            self.emit(
                Inst::Int {
                    dst: index.slot,
                    value: at as i64,
                },
                expr.span,
            );
            self.emit(
                Inst::StoreElem {
                    obj: dst.slot,
                    index: index.slot,
                    src: value.slot,
                    layout: elem,
                },
                expr.span,
            );
        }
        self.give_back(index.slot, index.layout);
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        Val::temp(dst.slot, layout)
    }

    /// `0..<n` and `0..n`: three words with the two bounds and which of them
    /// the end is.
    ///
    /// A range is a value like any other — it can be bound, passed, compared
    /// and iterated later — so it is built here rather than folded into the
    /// loop that usually consumes it. `docs/LINEAR_VM.md` gives it one
    /// layout for the whole program, `Struct { start, end, inclusive }`,
    /// which is what keeps `..` and `..<` one family rather than two.
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
        let dst = self.temp(layout);
        self.copy(dst.slot + RANGE_START, a.slot, shapes::INT, expr.span);
        self.copy(dst.slot + RANGE_END, b.slot, shapes::INT, expr.span);
        self.emit(
            Inst::Bool {
                dst: dst.slot + RANGE_INCLUSIVE,
                value: inclusive,
            },
            expr.span,
        );
        self.release(b, expr.span);
        self.release(a, expr.span);
        dst
    }

    /// `Vector.of(...)`, `Set.of(...)`, `Map.of(...)`: the three collections
    /// a program writes through the type's own name.
    pub(super) fn collection_of(&mut self, expr: &Expr, head: &str, args: &[Arg]) -> Val {
        match head {
            "Vector" => self.vector_of(expr, args),
            "Set" => self.keyed_of(expr, args, "Set"),
            _ => self.keyed_of(expr, args, "Map"),
        }
    }

    /// `Set.of(a, b, c)` and `Map.of(MapEntry(key: k, value: v), ...)`.
    ///
    /// The operands are the elements — for a `Map`, the `MapEntry` values the
    /// literal built, which is the shape `cove_runtime::lvm::builtins::keyed`
    /// reads a pair out of. The machine places each one where it belongs as it
    /// arrives, so the run is sorted at every step and a duplicate is refused
    /// rather than collapsed; none of that is something an instruction
    /// expresses, so it is one [`Inst::CallBuiltin`].
    ///
    /// **A literal with nothing in it is allocated rather than called.** The
    /// machine refuses `Set.of()` and `Map.of()` because a word says nothing
    /// about its family and the element layout is what the collector traces
    /// by — so the empty one has to be built where the layout is known, which
    /// is here. That is the rule [`Body::vector_of`] already follows, said of
    /// the two families whose emptiness a call could not describe.
    fn keyed_of(&mut self, expr: &Expr, args: &[Arg], what: &str) -> Val {
        let Some(ty) = self.settled_ty(expr) else {
            return self.dead(expr);
        };
        let Some(layout) = self.layout(&ty, expr.span) else {
            return self.dead(expr);
        };
        if let Some(bad) = self.plain_arguments(args) {
            return self.gap(bad, expr);
        }

        let mut held = Vec::with_capacity(args.len());
        for arg in args {
            held.push(self.expr(&arg.value));
        }
        let dst = self.temp(layout);
        if args.is_empty() {
            self.emit(
                Inst::Alloc {
                    dst: dst.slot,
                    layout,
                    len: Len::Count(0),
                },
                expr.span,
            );
            return dst;
        }
        let passed: Vec<crate::program::Arg> = held.iter().map(Val::arg).collect();
        self.emit_builtin(dst.slot, what, "of", &passed, layout, expr.span);
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        dst
    }

    /// `Vector.of(a, b, c)`: a store holding the elements, and a header
    /// naming it.
    ///
    /// The two objects are the whole of what a vector is, and both are
    /// allocated here because the lowering knows the layouts and
    /// [`Inst::Alloc`] takes one. What the machine is asked for is growth,
    /// which no instruction expresses — see [`Body::vector_method`].
    ///
    /// The store is allocated at exactly the length of the literal. A vector
    /// with no spare room is not a special case: the first `push` grows it
    /// like any other full one.
    pub(super) fn vector_of(&mut self, expr: &Expr, args: &[Arg]) -> Val {
        let Some(ty) = self.settled_ty(expr) else {
            return self.dead(expr);
        };
        let Ty::Vector(element) = ty.clone() else {
            return self.gap(
                "`Vector.of` answering something other than a `Vector`",
                expr,
            );
        };
        let (Some(layout), Some(elem)) = (
            self.layout(&ty, expr.span),
            self.layout(&element, expr.span),
        ) else {
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
        let store = self.temp(shapes::REF);
        self.emit(
            Inst::Alloc {
                dst: store.slot,
                layout: store_layout,
                len: Len::Count(args.len() as u32),
            },
            expr.span,
        );
        let index = self.temp(shapes::INT);
        for (at, value) in held.iter().enumerate() {
            self.emit(
                Inst::Int {
                    dst: index.slot,
                    value: at as i64,
                },
                expr.span,
            );
            self.emit(
                Inst::StoreElem {
                    obj: store.slot,
                    index: index.slot,
                    src: value.slot,
                    layout: elem,
                },
                expr.span,
            );
        }
        let dst = self.temp(shapes::REF);
        self.emit(
            Inst::Alloc {
                dst: dst.slot,
                layout,
                len: Len::Fixed,
            },
            expr.span,
        );
        self.emit(
            Inst::Int {
                dst: index.slot,
                value: args.len() as i64,
            },
            expr.span,
        );
        self.emit(
            Inst::StoreField {
                obj: dst.slot,
                at: VECTOR_LEN,
                src: index.slot,
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
        self.give_back(index.slot, index.layout);
        self.release(store, expr.span);
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        Val::temp(dst.slot, layout)
    }

    // ---- methods -----------------------------------------------------------

    /// `items.length()`, `items.isEmpty()`, `items.get(i)`.
    ///
    /// An `Array` keeps its elements in the object, so its length is the
    /// object's own header length and an element is one [`Inst::LoadElem`].
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
            ("length", 0) | ("isEmpty", 0) => self.header_length(expr, base, name),
            ("get", 1) => {
                let Some(element) = self.layout(elem, expr.span) else {
                    return self.dead(expr);
                };
                let obj = self.expr(base);
                let index = self.expr(&args[0].value);
                let len = self.temp(shapes::INT);
                self.emit(
                    Inst::Len {
                        dst: len.slot,
                        obj: obj.slot,
                    },
                    expr.span,
                );
                let answer = self.element_option(expr, obj.slot, len.slot, index.slot, element);
                self.give_back(len.slot, len.layout);
                self.release(index, expr.span);
                self.release(obj, expr.span);
                answer
            }
            ("map", 1) | ("filter", 1) | ("sorted", 1) | ("fold", 2) => {
                let elem = elem.clone();
                let items = self.expr(base);
                let obj = self.own_iterable(items, expr.span);
                self.walk_with(expr, obj, &elem, name, args)
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
    /// something an instruction expresses.
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
                let len = self.temp(shapes::INT);
                self.emit(
                    Inst::LoadField {
                        dst: len.slot,
                        obj: obj.slot,
                        at: VECTOR_LEN,
                        layout: shapes::INT,
                    },
                    expr.span,
                );
                self.release(obj, expr.span);
                self.length_answer(expr, name, len)
            }
            ("get", 1) => {
                let Some(element) = self.layout(elem, expr.span) else {
                    return self.dead(expr);
                };
                let obj = self.expr(base);
                let index = self.expr(&args[0].value);
                let (len, store) = self.vector_parts(obj.slot, expr.span);
                let answer = self.element_option(expr, store.slot, len.slot, index.slot, element);
                self.give_back(len.slot, len.layout);
                self.release(store, expr.span);
                self.release(index, expr.span);
                self.release(obj, expr.span);
                answer
            }
            // The elements come out before the first call, because a
            // `Vector` shares its storage and the callback may reach the very
            // vector being walked. That is `Vector.toArray`, which is the
            // copy the oracle makes for the same reason at the same point.
            ("map", 1) | ("filter", 1) | ("sorted", 1) | ("fold", 2) => {
                let elem = elem.clone();
                let items = self.expr(base);
                let Some(snapshot) = self.vector_snapshot(&items, &elem, base.span) else {
                    self.release(items, expr.span);
                    return self.dead(expr);
                };
                self.release(items, expr.span);
                self.walk_with(expr, snapshot, &elem, name, args)
            }
            _ if HANDED_OVER.contains(&("Vector", name)) => {
                self.machine_call(expr, Some(base), "Vector", name, args)
            }
            _ => self.gap(&format!("`Vector.{name}`"), expr),
        }
    }

    /// `members.length()`, `members.isEmpty()`, and everything else a `Set`
    /// answers.
    ///
    /// A `Set` is a run of members in the object, so its length is the
    /// object's own header length and reading it is one [`Inst::Len`] — the
    /// same split an `Array` is under. The machine's table has a
    /// `Set.length` too and the two agree about the answer; what differs is
    /// that the lowering already knows where to read it.
    ///
    /// The rest go to the machine, because each of them is a binary search
    /// over the order [`cove_runtime::lvm::builtins::key`] defines or a run
    /// built sorted in one pass, and neither is something an instruction
    /// expresses.
    pub(super) fn set_method(&mut self, expr: &Expr, base: &Expr, name: &str, args: &[Arg]) -> Val {
        match (name, args.len()) {
            ("length", 0) | ("isEmpty", 0) => self.header_length(expr, base, name),
            _ if HANDED_OVER.contains(&("Set", name)) => {
                self.machine_call(expr, Some(base), "Set", name, args)
            }
            _ => self.gap(&format!("`Set.{name}`"), expr),
        }
    }

    /// `entries.length()`, `entries.isEmpty()`, and everything else a `Map`
    /// answers.
    ///
    /// The header's length counts *entries* rather than words, so the same
    /// [`Inst::Len`] a `Set` reads its member count with reads a map's entry
    /// count. See [`Body::set_method`] for why the rest are the machine's.
    pub(super) fn map_method(&mut self, expr: &Expr, base: &Expr, name: &str, args: &[Arg]) -> Val {
        match (name, args.len()) {
            ("length", 0) | ("isEmpty", 0) => self.header_length(expr, base, name),
            _ if HANDED_OVER.contains(&("Map", name)) => {
                self.machine_call(expr, Some(base), "Map", name, args)
            }
            _ => self.gap(&format!("`Map.{name}`"), expr),
        }
    }

    /// How many elements the receiver's own header says it holds, or whether
    /// that is zero.
    fn header_length(&mut self, expr: &Expr, base: &Expr, name: &str) -> Val {
        let obj = self.expr(base);
        let len = self.temp(shapes::INT);
        self.emit(
            Inst::Len {
                dst: len.slot,
                obj: obj.slot,
            },
            expr.span,
        );
        self.release(obj, expr.span);
        self.length_answer(expr, name, len)
    }

    /// The length itself, or whether it is zero.
    ///
    /// `isEmpty()` is `length() == 0` and is lowered as one, rather than as
    /// an operation of its own: the two questions differ by a comparison the
    /// instruction set already has.
    fn length_answer(&mut self, expr: &Expr, name: &str, len: Val) -> Val {
        if name == "length" {
            return len;
        }
        let zero = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: zero.slot,
                value: 0,
            },
            expr.span,
        );
        let dst = self.temp(shapes::BOOL);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Eq,
                dst: dst.slot,
                a: len.slot,
                b: zero.slot,
            },
            expr.span,
        );
        self.give_back(zero.slot, zero.layout);
        self.give_back(len.slot, len.layout);
        dst
    }

    /// The two words a vector header holds: how many elements it has, and
    /// where they are.
    ///
    /// The store is answered in a reference location of its own, because
    /// reading an element is a read of *that* object and the collector has to
    /// see it held for as long as it is being read from.
    fn vector_parts(&mut self, obj: Slot, span: Span) -> (Val, Val) {
        let len = self.temp(shapes::INT);
        self.emit(
            Inst::LoadField {
                dst: len.slot,
                obj,
                at: VECTOR_LEN,
                layout: shapes::INT,
            },
            span,
        );
        let store = self.temp(shapes::REF);
        self.emit(
            Inst::LoadField {
                dst: store.slot,
                obj,
                at: VECTOR_STORE,
                layout: shapes::REF,
            },
            span,
        );
        (len, store)
    }

    /// `Some(elements[index])`, or `None` when `index` is outside them.
    ///
    /// The `None` is written first, discriminant and zeroed payload, so an
    /// index outside the elements falls through to an answer that is already
    /// there. A negative index and an index at or past the length are one
    /// case with one answer, which is the rule `get`, `set` and `remove` all
    /// share.
    fn element_option(
        &mut self,
        expr: &Expr,
        elements: Slot,
        len: Slot,
        index: Slot,
        elem: LayoutId,
    ) -> Val {
        let Some(ty) = self.settled_ty(expr) else {
            return self.dead(expr);
        };
        let Some(layout) = self.layout(&ty, expr.span) else {
            return self.dead(expr);
        };
        let span = expr.span;

        let dst = self.temp(layout);
        self.write_case(dst.slot, layout, 0, &[], span);
        let Some((parts, _)) = self.case_of(layout, 1) else {
            return self.gap("`get` answering something that is not an `Option`", expr);
        };
        let Some(part) = parts.first().cloned() else {
            return self.gap("an `Option` whose `Some` carries nothing", expr);
        };

        let bound = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: bound.slot,
                value: 0,
            },
            span,
        );
        let ok = self.temp(shapes::BOOL);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Ge,
                dst: ok.slot,
                a: index,
                b: bound.slot,
            },
            span,
        );
        let below = self.emit(
            Inst::BranchFalse {
                cond: ok.slot,
                to: PENDING,
            },
            span,
        );
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Lt,
                dst: ok.slot,
                a: index,
                b: len,
            },
            span,
        );
        let above = self.emit(
            Inst::BranchFalse {
                cond: ok.slot,
                to: PENDING,
            },
            span,
        );
        self.give_back(ok.slot, ok.layout);

        self.emit(
            Inst::Int {
                dst: dst.slot,
                value: 1,
            },
            span,
        );
        self.emit(
            Inst::LoadElem {
                dst: dst.slot + 1 + part.at,
                obj: elements,
                index,
                layout: elem,
            },
            span,
        );
        self.give_back(bound.slot, bound.layout);

        let rest = self.here();
        self.patch(below, rest);
        self.patch(above, rest);
        dst
    }

    /// An immutable copy of a vector's elements, as an `Array`.
    ///
    /// This is `Vector.toArray`, and it is what a `for` over a vector walks:
    /// `items_of` clones the elements out before the first turn, so a body
    /// that pushes onto the vector it is walking sees the same elements it
    /// started with.
    pub(super) fn vector_snapshot(&mut self, vector: &Val, elem: &Ty, span: Span) -> Option<Val> {
        let array = Ty::Array(Box::new(elem.clone()));
        let layout = self.layout(&array, span)?;
        let dst = self.temp(layout);
        self.emit_builtin(dst.slot, "Vector", "toArray", &[vector.arg()], layout, span);
        Some(dst)
    }

    // ---- `for` ---------------------------------------------------------------

    /// `for x in <iterable> { ... }`.
    ///
    /// Which walk it is comes from the iterable's type, and the five the
    /// oracle defines are two walks here: a range counts, and everything else
    /// reads a run of words out of an object.
    ///
    /// A `Set` and a `Map` join the second because of what their layouts are.
    /// `interp::items_of` says a set yields its elements in ascending order
    /// and a map a `MapEntry` per pair in ascending key order — and a
    /// `Shape::Members` object *is* that run of elements, while a
    /// `Shape::Entries` object is that run of pairs, each of them the key's
    /// words then the value's, which is exactly the `MapEntry` struct's own
    /// layout. So both are the same [`Inst::LoadElem`] at the right width,
    /// and nothing is built per turn.
    ///
    /// Neither needs a snapshot. An `Array` is immutable, so holding the
    /// object *is* the snapshot; a `Set` and a `Map` are immutable for the
    /// same reason — `inserted` and `removed` are past participles and answer
    /// new objects. Only a `Vector` shares its storage, and only a `Vector`
    /// is copied first.
    pub(super) fn for_expr(&mut self, binding: &Ident, iterable: &Expr, body: &Block, span: Span) {
        let Some(ty) = self.settled_ty(iterable) else {
            return;
        };
        match &ty {
            Ty::Range => self.for_range(binding, iterable, body, span),
            Ty::Array(elem) | Ty::Set(elem) => {
                let elem = (**elem).clone();
                let Some(element) = self.layout(&elem, iterable.span) else {
                    return;
                };
                let value = self.expr(iterable);
                let obj = self.own_iterable(value, span);
                self.for_elements(obj, element, binding, body, span);
            }
            // One entry is the key's words then the value's, which is what
            // the `MapEntry` the checker settled for the binding is: one
            // load at that layout's width answers the pair whole.
            Ty::Map(key, value) => {
                let entry = Ty::MapEntry(key.clone(), value.clone());
                let Some(element) = self.layout(&entry, iterable.span) else {
                    return;
                };
                let held = self.expr(iterable);
                let obj = self.own_iterable(held, span);
                self.for_elements(obj, element, binding, body, span);
            }
            // The copy is the snapshot, and it is taken before the first
            // turn: the body may push onto the very vector it is walking,
            // and `items_of` cloned the elements out for exactly that
            // reason.
            Ty::Vector(elem) => {
                let elem = (**elem).clone();
                let Some(element) = self.layout(&elem, iterable.span) else {
                    return;
                };
                let value = self.expr(iterable);
                let Some(snapshot) = self.vector_snapshot(&value, &elem, iterable.span) else {
                    self.release(value, iterable.span);
                    return;
                };
                self.release(value, iterable.span);
                self.for_elements(snapshot, element, binding, body, span);
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

    /// A reference location the loop owns for as long as it runs.
    ///
    /// A `for` walks the value the iterable had at the top, so the handle is
    /// copied out of whatever named it: a binding the body reassigns must not
    /// change what the walk is walking. A temporary is already the loop's
    /// own, and taking it over rather than copying it saves a word.
    fn own_iterable(&mut self, value: Val, span: Span) -> Val {
        if value.temp {
            return value;
        }
        let held = self.temp(value.layout);
        self.copy(held.slot, value.slot, value.layout, span);
        held
    }

    /// A walk over a run of words: `Array`, and the copy a `Vector` is walked
    /// through.
    ///
    /// The counter and the length are the loop's own locations, so the body
    /// can do what it likes to the names around it without the walk noticing.
    fn for_elements(
        &mut self,
        obj: Val,
        elem: LayoutId,
        binding: &Ident,
        body: &Block,
        span: Span,
    ) {
        let count = self.temp(shapes::INT);
        self.emit(
            Inst::Len {
                dst: count.slot,
                obj: obj.slot,
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
        let one = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: one.slot,
                value: 1,
            },
            span,
        );

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
                dst: index.slot,
                a: index.slot,
                b: one.slot,
            },
            span,
        );
        let test = self.here();
        self.patch(enter, test);
        let more = self.temp(shapes::BOOL);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Lt,
                dst: more.slot,
                a: index.slot,
                b: count.slot,
            },
            span,
        );
        let branch = self.emit(
            Inst::BranchFalse {
                cond: more.slot,
                to: PENDING,
            },
            span,
        );
        self.give_back(more.slot, more.layout);

        let element = self.temp(elem);
        self.emit(
            Inst::LoadElem {
                dst: element.slot,
                obj: obj.slot,
                index: index.slot,
                layout: elem,
            },
            span,
        );
        self.open_turn(binding, element.slot, elem, step);
        self.block(body, None);
        self.close_turn(element.slot, elem, step, body.span, span);
        self.end_loop(&[branch]);

        self.give_back(element.slot, element.layout);
        self.give_back(one.slot, one.layout);
        self.give_back(index.slot, index.layout);
        self.give_back(count.slot, count.layout);
        self.release(obj, span);
    }

    /// A walk over the integers a range yields.
    ///
    /// The bound is never adjusted. A turn happens at `index` when
    /// `index < end`, and — when the range was written inclusive — also when
    /// `index == end`; the step runs only on the first of those, so the
    /// counter is incremented only where it is already known to be below the
    /// end. `0...Int.MAX` therefore yields every value up to `Int.MAX` and
    /// stops, rather than trapping on an overflow the language does not
    /// have.
    fn for_range(&mut self, binding: &Ident, iterable: &Expr, body: &Block, span: Span) {
        let value = self.expr(iterable);
        let range = value.slot;
        let index = self.temp(shapes::INT);
        self.copy(index.slot, range + RANGE_START, shapes::INT, span);
        let limit = self.temp(shapes::INT);
        self.copy(limit.slot, range + RANGE_END, shapes::INT, span);
        let inclusive = self.temp(shapes::BOOL);
        self.copy(inclusive.slot, range + RANGE_INCLUSIVE, shapes::BOOL, span);
        // The bounds are out of the value, so the range itself is done with:
        // a loop that ran for a long time should not hold it.
        self.release(value, span);

        let one = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: one.slot,
                value: 1,
            },
            span,
        );
        let more = self.temp(shapes::BOOL);

        let enter = self.emit(Inst::Jump { to: PENDING }, span);
        // `continue` comes here, and so does the end of every turn. The step
        // is guarded by the same `index < end` the entry test uses, which is
        // what keeps the counter from ever passing the end.
        let step = self.here();
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Lt,
                dst: more.slot,
                a: index.slot,
                b: limit.slot,
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
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: index.slot,
                a: index.slot,
                b: one.slot,
            },
            span,
        );
        let guard = self.here();
        self.patch(enter, guard);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Lt,
                dst: more.slot,
                a: index.slot,
                b: limit.slot,
            },
            span,
        );
        let to_edge = self.emit(
            Inst::BranchFalse {
                cond: more.slot,
                to: PENDING,
            },
            span,
        );

        // The counter is the binding: a `for` binding is read-only, so
        // nothing in the body can move the walk, and a location copied into
        // every turn would be words written for nothing.
        let body_at = self.here();
        self.open_turn(binding, index.slot, shapes::INT, step);
        self.block(body, None);
        self.close_turn(index.slot, shapes::INT, step, body.span, span);

        // The one turn `index == end` earns, and only when the range was
        // written inclusive.
        let edge = self.here();
        self.patch(to_edge, edge);
        let not_inclusive = self.emit(
            Inst::BranchFalse {
                cond: inclusive.slot,
                to: PENDING,
            },
            span,
        );
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Eq,
                dst: more.slot,
                a: index.slot,
                b: limit.slot,
            },
            span,
        );
        let past = self.emit(
            Inst::BranchFalse {
                cond: more.slot,
                to: PENDING,
            },
            span,
        );
        self.emit(Inst::Jump { to: body_at }, span);

        self.end_loop(&[done, not_inclusive, past]);

        self.give_back(more.slot, more.layout);
        self.give_back(one.slot, one.layout);
        self.give_back(inclusive.slot, inclusive.layout);
        self.give_back(limit.slot, limit.layout);
        self.give_back(index.slot, index.layout);
    }

    /// Opens one turn of a loop: the loop record, the scope the binding
    /// lives in, and the name.
    ///
    /// `element` already holds this turn's value. It is bound rather than
    /// owned by the scope, because the scope gives its slots back when it
    /// ends and the next turn writes them again — so the loop owns the
    /// location and the scope owns only the name.
    fn open_turn(&mut self, binding: &Ident, element: Slot, layout: LayoutId, step: Pc) {
        let holds = self.holds_ref(layout);
        self.loops.push(Loop {
            head: step,
            depth: self.frame.depth(),
            held: self.held.len(),
            breaks: Vec::new(),
            element: holds.then_some(Dest {
                slot: element,
                layout,
            }),
        });
        self.frame.push_scope();
        self.frame.bind(&binding.node, element, layout);
    }

    /// Closes one turn: the scope's clears, the element's, and the back edge.
    ///
    /// The `Clear` is what keeps a walk over a large collection from holding
    /// more than one element at a time. It is the loop's own: a scope that
    /// gave the location back could not emit it, and the next turn's
    /// overwrite is not a promise the lowering is allowed to rely on.
    fn close_turn(
        &mut self,
        element: Slot,
        layout: LayoutId,
        step: Pc,
        body_span: Span,
        span: Span,
    ) {
        let clears = self.frame.pop_scope();
        self.clear(&clears, body_span);
        if self.holds_ref(layout) {
            self.zero(element, layout, body_span);
        }
        self.emit(Inst::Jump { to: step }, span);
    }

    /// Ends a loop: every way out of it lands here.
    fn end_loop(&mut self, exits: &[Pc]) {
        let end = self.here();
        for at in exits {
            self.patch(*at, end);
        }
        let finished = self.loops.pop().expect("the loop was pushed above");
        for at in finished.breaks {
            self.patch(at, end);
        }
    }

    // ---- comparing values ----------------------------------------------------

    /// `==` and `!=` between two values the instruction set cannot compare
    /// in one step.
    ///
    /// A `String` compares by its bytes, which is one instruction. Everything
    /// else the language defines `==` for is compared by walking the two
    /// values, and that walk is not an instruction: what `==` means for an
    /// array, a struct, an enum or a vector is a rule of the language, stated
    /// in the language reference, and the IR describes families rather than
    /// carrying a case per family.
    ///
    /// # An inline value has to be boxed to be compared
    ///
    /// [`Inst::CallBuiltin`] hands the machine slot numbers and nothing else:
    /// there is no channel on it for the layout of each operand. A reference
    /// carries its description in the object's own header, so an array or a
    /// vector needs nothing; an *inline* struct, enum or range is a run of
    /// words with nothing attached, so it goes into a box that carries its
    /// layout. That is one allocation per comparison, and it is a
    /// consequence of the instruction's shape rather than of the
    /// representation.
    ///
    /// # What the builtin must do
    ///
    /// `Any.equals` takes two operands and answers whether they are the same
    /// value, by the rule `Value::eq_value` states for the oracle: two
    /// objects of different layouts are not equal; a string compares by
    /// bytes; a struct field-wise; an enum by case and then payload-wise; an
    /// array element-wise; a vector by the elements its length names; a box
    /// by what it holds.
    ///
    /// `!=` is the same call and an [`Inst::Not`]: one builtin rather than a
    /// second one that answers the negation.
    pub(super) fn compare_values(
        &mut self,
        expr: &Expr,
        equal: bool,
        lhs: &Expr,
        dst: Slot,
        a: &Val,
        b: &Val,
    ) {
        if matches!(self.ty(lhs), Some(Ty::Str)) {
            self.emit(
                Inst::Cmp {
                    on: Compare::Str,
                    op: if equal { CmpOp::Eq } else { CmpOp::Ne },
                    dst,
                    a: a.slot,
                    b: b.slot,
                },
                expr.span,
            );
            return;
        }
        let builtin = self.pool.builtin(Builtin {
            receiver: "Any".into(),
            operation: "equals".into(),
            result: shapes::BOOL,
        });
        let args = self.pool.args.intern(vec![a.arg(), b.arg()]);
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
        let dst = self.temp(shapes::BOOL);
        if matches!(self.ty(lhs), Some(Ty::Vector(_))) {
            self.emit(
                Inst::Cmp {
                    on: Compare::Identity,
                    op: CmpOp::Eq,
                    dst: dst.slot,
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
        dst
    }
}

/// The operations of a sequence the machine performs rather than the
/// instruction set.
///
/// Each of them either builds an object whose family only the layout table
/// knows — `slice`, `toVector`, `toArray` — or walks the elements with the
/// language's own equality, which is not something an instruction expresses.
/// The four that take a closure are not here and never will be. A builtin
/// that invoked the closure would re-enter the dispatch loop from inside a
/// Rust function, which is the one thing `docs/LINEAR_VM.md` asks this
/// backend not to do — so `map`, `filter`, `fold` and `sorted` are all loops
/// in the IR, in `cove_lir::lower::walks`.
///
/// A `Set` and a `Map` are here for the whole of their tables but `length`
/// and `isEmpty`: both are sorted runs, so every one of these is a binary
/// search over the order `cove_runtime::lvm::builtins::key` defines or a run
/// built sorted in one pass.
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
    ("Set", "contains"),
    ("Set", "inserted"),
    ("Set", "removed"),
    ("Set", "toArray"),
    ("Map", "get"),
    ("Map", "contains"),
    ("Map", "keys"),
    ("Map", "values"),
    ("Map", "inserted"),
    ("Map", "removed"),
];

/// Whether the checker knows `head` as a builtin type that is written as a
/// namespace, and `ty` as what its `of` answers: `Vector.of(1, 2)`,
/// `Set.of(1, 2)`, `Map.of(MapEntry(key: "a", value: 1))`.
///
/// Only the three this lowering has been taught, because the answer is used
/// to decide what to emit rather than to describe the language.
pub(super) fn namespace_of(head: &str, ty: &Ty) -> bool {
    matches!(
        (head, ty),
        ("Vector", Ty::Vector(_)) | ("Set", Ty::Set(_)) | ("Map", Ty::Map(..))
    )
}
