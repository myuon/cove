//! `map`, `filter`, `fold` and `sorted`: the sequence methods that take a
//! closure.
//!
//! # A builtin never calls back into Cove
//!
//! `docs/LINEAR_VM.md` gives the reason and this module is the consequence. A
//! builtin that invoked the closure itself would have to re-enter the
//! dispatch loop from inside a Rust function, which puts a Rust frame under
//! every Cove frame the closure creates — and gives back the property the
//! loop was built to have, that how deep a Cove program may nest is decided
//! by the reserved stack region and not by how large a Rust frame the
//! interpreter compiled to. A `map` over a `map` over a `map` would be three
//! Rust frames deep before the program did anything.
//!
//! So each of the four **lowers to a loop in the IR**, and the closure's
//! calls are [`Inst::CallClosure`] frames like any other: depth, the
//! collector's roots and a stack overflow all work without a second story.
//! `cove_runtime::lvm::builtins` stays a library over words with nothing in
//! it that can call anything.
//!
//! # What the loops promise, and where it comes from
//!
//! `cove_runtime::builtins::walk_with` is the oracle, and the four promises
//! it states are the ones these loops keep:
//!
//! - **the elements are taken once, before the first call.** For an `Array`
//!   the object *is* the snapshot, because an array cannot change; for a
//!   `Vector` the copy is `Vector.toArray`, taken by the caller before the
//!   walk begins, so a callback that reaches the vector finds neither a live
//!   borrow nor a walk that changes under it.
//! - **every element is visited once, front to back**, in the receiver's own
//!   order — one counter, ascending.
//! - **each answers an `Array`** whichever receiver it was called on, except
//!   `fold`, which answers the accumulator.
//! - **each is empty-safe by construction.** An empty receiver is a `count`
//!   of zero, the test fails on the first turn, and the answer is the empty
//!   array the loop allocated — or, for `fold`, the initial value nothing
//!   overwrote, or, for `sorted`, the copy no pass ever ran over.
//!
//! `sorted` is the fourth and the one that is not a single counter: it is a
//! bottom-up stable merge over two runs, and `Body::walk_sorted` says why it
//! is written out rather than handed over.
//!
//! # A callback that fails takes the whole call with it
//!
//! The oracle builds its answer to the side and returns it only on success,
//! so that no half-built array is ever reachable. Here that costs nothing to
//! arrange, because there is nothing to arrange: a failure inside the closure
//! is a runtime error, and a runtime error ends the task. The half-filled
//! object is in a slot of a frame that is being unwound, and no Cove
//! expression exists that could observe it. The receiver is never written
//! through on any path either — every one of the three builds a new object
//! and none of them stores into the elements it is walking.
//!
//! # The element and the turn's answer are cleared per turn
//!
//! This is the same discipline a `for` is under and for the same reason: one
//! location holds the element for every turn, so a walk over a large sequence
//! must hold one element at a time rather than every element it has reached.
//! The `Clear` is emitted at the end of the body rather than left to the next
//! turn's overwrite, because the next turn is not a promise the lowering is
//! allowed to rely on — the last turn of a loop has no next one.

use cove_diag::Span;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Expr};

use super::frame::Val;
use super::shapes;
use super::{Body, PENDING};
use crate::inst::{ArithOp, CmpOp, Compare, Inst, Len, Num, Pc, Slot};
use crate::layout::LayoutId;

/// The locations a walk over a run of words keeps for as long as it runs.
///
/// They are the loop's own, so the closure can do what it likes to the names
/// around it without the walk noticing.
struct Walk {
    count: Val,
    index: Val,
    one: Val,
    /// Where the end of a turn goes back to: the step, which is above the
    /// test so that the counter is advanced once per turn and the first turn
    /// jumps over it.
    step: Pc,
    /// The branch that leaves, with nowhere to go yet.
    exit: Pc,
}

impl Body<'_> {
    /// One of the four, over elements this caller has already settled.
    ///
    /// `obj` is a location the walk owns — an `Array` copied out of whatever
    /// named it, or the copy a `Vector` is walked through — and this ends its
    /// live range.
    pub(super) fn walk_with(
        &mut self,
        expr: &Expr,
        obj: Val,
        elem: &Ty,
        name: &str,
        args: &[Arg],
    ) -> Val {
        match name {
            "map" => self.walk_map(expr, obj, elem, &args[0].value),
            "filter" => self.walk_filter(expr, obj, elem, &args[0].value),
            "sorted" => self.walk_sorted(expr, obj, elem, &args[0].value),
            _ => self.walk_fold(expr, obj, elem, &args[0].value, &args[1].value),
        }
    }

    // ---- map ---------------------------------------------------------------

    /// `items.map(transform)`: an array of the same length, filled one call at
    /// a time.
    ///
    /// The answer is allocated before the first call, at the length the
    /// receiver has — which is what `map` answering one element per element
    /// means, and what makes the loop a store rather than an append.
    fn walk_map(&mut self, expr: &Expr, obj: Val, elem: &Ty, callback: &Expr) -> Val {
        let Some(answer) = self.settled_ty(expr) else {
            self.release(obj, expr.span);
            return self.dead(expr);
        };
        let Ty::Array(produced) = answer.clone() else {
            self.release(obj, expr.span);
            return self.gap("`map` answering something other than an `Array`", expr);
        };
        let (Some(result), Some(made), Some(element)) = (
            self.layout(&answer, expr.span),
            self.layout(&produced, expr.span),
            self.layout(elem, expr.span),
        ) else {
            self.release(obj, expr.span);
            return self.dead(expr);
        };
        let Some((closure, params, returns)) = self.callback_of(callback) else {
            self.release(obj, expr.span);
            return self.dead(expr);
        };
        if !self.callback_matches(callback, &params, returns, &[element], made) {
            self.release(closure, expr.span);
            self.release(obj, expr.span);
            return self.dead(expr);
        }
        let span = expr.span;

        let count = self.length_of(&obj, span);
        let kept = self.temp(result);
        self.emit(
            Inst::Alloc {
                dst: kept.slot,
                layout: result,
                len: Len::Slot(count.slot),
            },
            span,
        );
        let walk = self.open_walk(count, span);

        let element_at = self.temp(element);
        self.emit(
            Inst::LoadElem {
                dst: element_at.slot,
                obj: obj.slot,
                index: walk.index.slot,
                layout: element,
            },
            span,
        );
        let turn = self.temp(made);
        self.call_closure(turn.slot, closure.slot, vec![element_at.arg()], span);
        self.emit(
            Inst::StoreElem {
                obj: kept.slot,
                index: walk.index.slot,
                src: turn.slot,
                layout: made,
            },
            span,
        );
        self.end_turn(&[turn, element_at], span);
        self.close_walk(walk, span);

        self.give_back(turn.slot, turn.layout);
        self.give_back(element_at.slot, element_at.layout);
        self.release(closure, span);
        self.release(obj, span);
        kept
    }

    // ---- filter -------------------------------------------------------------

    /// `items.filter(keep)`: the elements the closure answered `true` for, in
    /// the order they were in.
    ///
    /// How many there will be is not known until the last call, and an
    /// `Array` object is as long as it was allocated. So the loop fills a run
    /// of the receiver's length — the most there can be — counts what it
    /// kept, and answers `Array.slice(0, kept)`, which is the language's own
    /// "a part of a sequence is a finished sequence". The words past the
    /// count are the zeroes the allocation left, so a reference among them
    /// reads null and the collector traces nothing from one.
    fn walk_filter(&mut self, expr: &Expr, obj: Val, elem: &Ty, callback: &Expr) -> Val {
        let Some(answer) = self.settled_ty(expr) else {
            self.release(obj, expr.span);
            return self.dead(expr);
        };
        let (Some(result), Some(element)) = (
            self.layout(&answer, expr.span),
            self.layout(elem, expr.span),
        ) else {
            self.release(obj, expr.span);
            return self.dead(expr);
        };
        let Some((closure, params, returns)) = self.callback_of(callback) else {
            self.release(obj, expr.span);
            return self.dead(expr);
        };
        if !self.callback_matches(callback, &params, returns, &[element], shapes::BOOL) {
            self.release(closure, expr.span);
            self.release(obj, expr.span);
            return self.dead(expr);
        }
        let span = expr.span;

        let count = self.length_of(&obj, span);
        let room = self.temp(result);
        self.emit(
            Inst::Alloc {
                dst: room.slot,
                layout: result,
                len: Len::Slot(count.slot),
            },
            span,
        );
        let taken = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: taken.slot,
                value: 0,
            },
            span,
        );
        let walk = self.open_walk(count, span);

        let element_at = self.temp(element);
        self.emit(
            Inst::LoadElem {
                dst: element_at.slot,
                obj: obj.slot,
                index: walk.index.slot,
                layout: element,
            },
            span,
        );
        let verdict = self.temp(shapes::BOOL);
        self.call_closure(verdict.slot, closure.slot, vec![element_at.arg()], span);
        let dropped = self.emit(
            Inst::BranchFalse {
                cond: verdict.slot,
                to: PENDING,
            },
            span,
        );
        self.emit(
            Inst::StoreElem {
                obj: room.slot,
                index: taken.slot,
                src: element_at.slot,
                layout: element,
            },
            span,
        );
        self.emit(
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: taken.slot,
                a: taken.slot,
                b: walk.one.slot,
            },
            span,
        );
        let rest = self.here();
        self.patch(dropped, rest);
        self.end_turn(&[element_at], span);
        self.close_walk(walk, span);

        self.give_back(verdict.slot, verdict.layout);
        self.give_back(element_at.slot, element_at.layout);
        self.release(closure, span);
        self.release(obj, span);

        let zero = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: zero.slot,
                value: 0,
            },
            span,
        );
        let answer = self.temp(result);
        self.emit_builtin(
            answer.slot,
            "Array",
            "slice",
            &[room.arg(), zero.arg(), taken.arg()],
            result,
            span,
        );
        self.give_back(zero.slot, zero.layout);
        self.give_back(taken.slot, taken.layout);
        self.release(room, span);
        answer
    }

    // ---- fold ---------------------------------------------------------------

    /// `items.fold(initial, step)`: one accumulator, threaded through every
    /// element.
    ///
    /// The accumulator is the call's **destination** as well as its first
    /// argument, so a turn is one instruction rather than a call and a copy.
    /// That is sound because the machine copies the arguments into the
    /// callee's frame on the way in and the answer back on the way out, so
    /// nothing reads the location between the two — and it is the same
    /// arrangement `n += 2` has, where the destination *is* the accumulator.
    ///
    /// An empty receiver answers `initial`, because nothing overwrote it.
    fn walk_fold(
        &mut self,
        expr: &Expr,
        obj: Val,
        elem: &Ty,
        initial: &Expr,
        callback: &Expr,
    ) -> Val {
        let Some(answer) = self.settled_ty(expr) else {
            self.release(obj, expr.span);
            return self.dead(expr);
        };
        let (Some(result), Some(element)) = (
            self.layout(&answer, expr.span),
            self.layout(elem, expr.span),
        ) else {
            self.release(obj, expr.span);
            return self.dead(expr);
        };
        let span = expr.span;

        // The arguments are evaluated in source order, because they are
        // ordinary expressions and the first may do something the second
        // sees.
        let start = self.expr(initial);
        let start = self.fit(start, result, initial.span);
        let total = self.temp(result);
        self.copy(total.slot, start.slot, result, initial.span);
        self.release(start, initial.span);

        let Some((closure, params, returns)) = self.callback_of(callback) else {
            self.release(total, span);
            self.release(obj, span);
            return self.dead(expr);
        };
        if !self.callback_matches(callback, &params, returns, &[result, element], result) {
            self.release(closure, span);
            self.release(total, span);
            self.release(obj, span);
            return self.dead(expr);
        }

        let count = self.length_of(&obj, span);
        let walk = self.open_walk(count, span);
        let element_at = self.temp(element);
        self.emit(
            Inst::LoadElem {
                dst: element_at.slot,
                obj: obj.slot,
                index: walk.index.slot,
                layout: element,
            },
            span,
        );
        self.call_closure(
            total.slot,
            closure.slot,
            vec![total.arg(), element_at.arg()],
            span,
        );
        self.end_turn(&[element_at], span);
        self.close_walk(walk, span);

        self.give_back(element_at.slot, element_at.layout);
        self.release(closure, span);
        self.release(obj, span);
        total
    }

    // ---- sorted ---------------------------------------------------------------

    /// `items.sorted(by)`: a **stable** sort under the caller's own ordering,
    /// as a bottom-up merge in the IR.
    ///
    /// # Why a merge, and why written out here
    ///
    /// `cove_runtime::builtins::merge_sort` is the oracle and gives both
    /// halves of the reason. `by` is a Cove closure, so it can fail, be
    /// cancelled or run out of fuel — and a builtin that called it would be
    /// the re-entry `docs/LINEAR_VM.md` asks this backend not to make. And
    /// `by` can contradict itself: the schema says an ordering where `by(a,b)`
    /// and `by(b,a)` are both true gets *some* permutation and no promise
    /// about which, so there is no invariant here to break and nothing to
    /// panic about. A merge answers a permutation whatever `by` does.
    ///
    /// Because the only promise on a contradictory ordering is "some
    /// permutation", the two backends are free to disagree about which one —
    /// and on a consistent ordering a stable sort is fully determined, so
    /// they agree there without being written the same way.
    ///
    /// # The shape
    ///
    /// Two runs of the receiver's length and passes of doubling width, which
    /// is `merge_sort` exactly:
    ///
    /// - `source` is `Array.slice(items, 0, len)` — the copy the sort works
    ///   in, made by the one builtin that already answers a part of a
    ///   sequence as a finished one. The receiver is never written through.
    /// - `merged` is an allocation of the same length, and the two are
    ///   **swapped** at the end of each pass rather than copied back.
    /// - A pass walks blocks of `width * 2`, merging the two runs inside
    ///   each. The right run's element is taken only when `by` says it comes
    ///   *strictly* before the left run's, which is what makes the sort
    ///   stable: equal elements meet with the earlier one on the left and the
    ///   earlier one is kept.
    /// - The tails are two loops, of which at most one runs.
    ///
    /// `while width < len` is the outer test, so a receiver of nothing or of
    /// one element makes no pass at all and answers the copy.
    ///
    /// # The elements are cleared per comparison
    ///
    /// `a` and `b` hold one element each for the length of one comparison,
    /// and both are cleared at the end of it — the same discipline every
    /// other walk's element is under, for the same reason: a sort of a large
    /// sequence must hold two elements at a time rather than every element it
    /// has compared.
    fn walk_sorted(&mut self, expr: &Expr, obj: Val, elem: &Ty, callback: &Expr) -> Val {
        let Some(answer) = self.settled_ty(expr) else {
            self.release(obj, expr.span);
            return self.dead(expr);
        };
        let (Some(result), Some(element)) = (
            self.layout(&answer, expr.span),
            self.layout(elem, expr.span),
        ) else {
            self.release(obj, expr.span);
            return self.dead(expr);
        };
        let Some((closure, params, returns)) = self.callback_of(callback) else {
            self.release(obj, expr.span);
            return self.dead(expr);
        };
        if !self.callback_matches(
            callback,
            &params,
            returns,
            &[element, element],
            shapes::BOOL,
        ) {
            self.release(closure, expr.span);
            self.release(obj, expr.span);
            return self.dead(expr);
        }
        let span = expr.span;

        let count = self.length_of(&obj, span);
        let zero = self.int(0, span);
        // The working copy. `Array.slice` is the language's own "a part of a
        // sequence is a finished sequence", and the whole of one is a part of
        // it — so the copy the sort needs is a call this lowering already
        // makes rather than a loop of its own.
        let source = self.temp(result);
        self.emit_builtin(
            source.slot,
            "Array",
            "slice",
            &[obj.arg(), zero.arg(), count.arg()],
            result,
            span,
        );
        self.release(obj, span);
        let merged = self.temp(result);
        self.emit(
            Inst::Alloc {
                dst: merged.slot,
                layout: result,
                len: Len::Slot(count.slot),
            },
            span,
        );

        let one = self.int(1, span);
        let width = self.int(1, span);
        let out = self.temp(shapes::INT);
        let start = self.temp(shapes::INT);
        let middle = self.temp(shapes::INT);
        let end = self.temp(shapes::INT);
        let left = self.temp(shapes::INT);
        let right = self.temp(shapes::INT);
        let more = self.temp(shapes::BOOL);
        let a = self.temp(element);
        let b = self.temp(element);

        // ---- one pass per doubling of `width`
        let pass = self.here();
        self.compare(CmpOp::Lt, more.slot, width.slot, count.slot, span);
        let done = self.branch(more.slot, span);
        self.set(out.slot, zero.slot, span);
        self.set(start.slot, zero.slot, span);

        // ---- one block per `width * 2` elements
        let block = self.here();
        self.compare(CmpOp::Lt, more.slot, start.slot, count.slot, span);
        let swap = self.branch(more.slot, span);
        self.add(middle.slot, start.slot, width.slot, span);
        self.add(end.slot, middle.slot, width.slot, span);
        self.clamp(middle.slot, count.slot, span);
        self.clamp(end.slot, count.slot, span);
        self.set(left.slot, start.slot, span);
        self.set(right.slot, middle.slot, span);

        // ---- the merge itself, while both runs have something left
        let merge = self.here();
        self.compare(CmpOp::Lt, more.slot, left.slot, middle.slot, span);
        let left_spent = self.branch(more.slot, span);
        self.compare(CmpOp::Lt, more.slot, right.slot, end.slot, span);
        let right_spent = self.branch(more.slot, span);
        self.load_elem(a.slot, source.slot, right.slot, element, span);
        self.load_elem(b.slot, source.slot, left.slot, element, span);
        // `by(right, left)`: the right run's element goes first only when it
        // comes strictly before the left run's, which is the oracle's own
        // operand order and is what makes the sort stable.
        self.call_closure(more.slot, closure.slot, vec![a.arg(), b.arg()], span);
        let take_left = self.branch(more.slot, span);
        self.store_elem(merged.slot, out.slot, a.slot, element, span);
        self.add(right.slot, right.slot, one.slot, span);
        let advance = self.emit(Inst::Jump { to: PENDING }, span);
        let otherwise = self.here();
        self.patch(take_left, otherwise);
        self.store_elem(merged.slot, out.slot, b.slot, element, span);
        self.add(left.slot, left.slot, one.slot, span);
        let taken = self.here();
        self.patch(advance, taken);
        self.add(out.slot, out.slot, one.slot, span);
        self.end_turn(&[b, a], span);
        self.emit(Inst::Jump { to: merge }, span);

        // ---- whichever run still has something, copied straight across
        let tails = self.here();
        self.patch(left_spent, tails);
        self.patch(right_spent, tails);
        self.tail(
            source.slot,
            merged.slot,
            left.slot,
            middle.slot,
            out.slot,
            one.slot,
            a,
            element,
            span,
        );
        self.tail(
            source.slot,
            merged.slot,
            right.slot,
            end.slot,
            out.slot,
            one.slot,
            a,
            element,
            span,
        );
        self.set(start.slot, end.slot, span);
        self.emit(Inst::Jump { to: block }, span);

        // ---- the pass is over: what was merged becomes what is read
        let swapping = self.here();
        self.patch(swap, swapping);
        let handle = self.temp(result);
        self.copy(handle.slot, source.slot, result, span);
        self.copy(source.slot, merged.slot, result, span);
        self.copy(merged.slot, handle.slot, result, span);
        self.release(handle, span);
        // The next pass merges runs twice as long.
        self.add(width.slot, width.slot, width.slot, span);
        self.emit(Inst::Jump { to: pass }, span);

        let end_at = self.here();
        self.patch(done, end_at);
        self.release(merged, span);
        self.release(closure, span);
        self.give_back(b.slot, b.layout);
        self.give_back(a.slot, a.layout);
        self.give_back(more.slot, more.layout);
        self.give_back(right.slot, right.layout);
        self.give_back(left.slot, left.layout);
        self.give_back(end.slot, end.layout);
        self.give_back(middle.slot, middle.layout);
        self.give_back(start.slot, start.layout);
        self.give_back(out.slot, out.layout);
        self.give_back(width.slot, width.layout);
        self.give_back(one.slot, one.layout);
        self.give_back(zero.slot, zero.layout);
        self.give_back(count.slot, count.layout);
        source
    }

    /// One of a merge's two tails: `while at < limit`, copy across and step.
    ///
    /// At most one of the two runs, because the merge above stopped when one
    /// of them was spent — but which one is a fact about the data, so both
    /// are emitted and the one that has nothing left tests once and leaves.
    #[allow(clippy::too_many_arguments)]
    fn tail(
        &mut self,
        source: Slot,
        merged: Slot,
        at: Slot,
        limit: Slot,
        out: Slot,
        one: Slot,
        held: Val,
        element: LayoutId,
        span: Span,
    ) {
        let more = self.temp(shapes::BOOL);
        let test = self.here();
        self.compare(CmpOp::Lt, more.slot, at, limit, span);
        let spent = self.branch(more.slot, span);
        self.load_elem(held.slot, source, at, element, span);
        self.store_elem(merged, out, held.slot, element, span);
        self.end_turn(&[held], span);
        self.add(at, at, one, span);
        self.add(out, out, one, span);
        self.emit(Inst::Jump { to: test }, span);
        let rest = self.here();
        self.patch(spent, rest);
        self.give_back(more.slot, more.layout);
    }

    // ---- the small instructions the merge is written in ------------------------

    /// A location holding a constant `Int`.
    fn int(&mut self, value: i64, span: Span) -> Val {
        let dst = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: dst.slot,
                value,
            },
            span,
        );
        dst
    }

    /// `dst = a op b`, as `Int`s.
    fn compare(&mut self, op: CmpOp, dst: Slot, a: Slot, b: Slot, span: Span) {
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op,
                dst,
                a,
                b,
            },
            span,
        );
    }

    /// A [`Inst::BranchFalse`] whose target is not known yet.
    fn branch(&mut self, cond: Slot, span: Span) -> Pc {
        self.emit(Inst::BranchFalse { cond, to: PENDING }, span)
    }

    /// `dst = a + b`, as `Int`s.
    pub(super) fn add(&mut self, dst: Slot, a: Slot, b: Slot, span: Span) {
        self.emit(
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst,
                a,
                b,
            },
            span,
        );
    }

    /// `dst = src`, one `Int` word.
    fn set(&mut self, dst: Slot, src: Slot, span: Span) {
        self.copy(dst, src, shapes::INT, span);
    }

    /// `if dst > limit { dst = limit }`: the `min` a block's bounds need.
    fn clamp(&mut self, dst: Slot, limit: Slot, span: Span) {
        let over = self.temp(shapes::BOOL);
        self.compare(CmpOp::Gt, over.slot, dst, limit, span);
        let within = self.branch(over.slot, span);
        self.set(dst, limit, span);
        let rest = self.here();
        self.patch(within, rest);
        self.give_back(over.slot, over.layout);
    }

    fn load_elem(&mut self, dst: Slot, obj: Slot, index: Slot, layout: LayoutId, span: Span) {
        self.emit(
            Inst::LoadElem {
                dst,
                obj,
                index,
                layout,
            },
            span,
        );
    }

    fn store_elem(&mut self, obj: Slot, index: Slot, src: Slot, layout: LayoutId, span: Span) {
        self.emit(
            Inst::StoreElem {
                obj,
                index,
                src,
                layout,
            },
            span,
        );
    }

    // ---- the shared loop -----------------------------------------------------

    /// How many elements the walk will visit, read once before it begins.
    fn length_of(&mut self, obj: &Val, span: Span) -> Val {
        let count = self.temp(shapes::INT);
        self.emit(
            Inst::Len {
                dst: count.slot,
                obj: obj.slot,
            },
            span,
        );
        count
    }

    /// The counter, the step and the test, in the order a `for` over a
    /// sequence already puts them.
    ///
    /// The step is above the test and the first turn jumps over it, so the
    /// end of a turn has one place to go back to and the counter is advanced
    /// exactly once per turn.
    fn open_walk(&mut self, count: Val, span: Span) -> Walk {
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
        let exit = self.emit(
            Inst::BranchFalse {
                cond: more.slot,
                to: PENDING,
            },
            span,
        );
        self.give_back(more.slot, more.layout);
        Walk {
            count,
            index,
            one,
            step,
            exit,
        }
    }

    /// Ends the live range of everything one turn was holding.
    ///
    /// A `for` binding gets this and so does a walk's element, because it is
    /// the same lowering: one location per turn, cleared at the end of it, so
    /// a walk over a large sequence holds one element at a time.
    fn end_turn(&mut self, held: &[Val], span: Span) {
        for value in held {
            if self.holds_ref(value.layout) {
                self.zero(value.slot, value.layout, span);
            }
        }
    }

    /// The back edge, and the one way out.
    fn close_walk(&mut self, walk: Walk, span: Span) {
        self.emit(Inst::Jump { to: walk.step }, span);
        let end = self.here();
        self.patch(walk.exit, end);
        self.give_back(walk.one.slot, walk.one.layout);
        self.give_back(walk.index.slot, walk.index.layout);
        self.give_back(walk.count.slot, walk.count.layout);
    }

    // ---- the callback --------------------------------------------------------

    /// The closure, and the layouts a call to it passes and answers.
    ///
    /// The value is evaluated here rather than by the caller, because it is
    /// the call's argument and the language evaluates a call's arguments in
    /// source order — the receiver first, then this.
    fn callback_of(&mut self, callback: &Expr) -> Option<(Val, Vec<LayoutId>, LayoutId)> {
        let func = self.callback(callback)?;
        let (params, returns) = self.signature(&func, callback.span)?;
        let value = self.expr(callback);
        Some((value, params, returns))
    }

    /// Whether the closure takes and answers what the loop will hand it.
    ///
    /// The checker unified the callback's parameters with the element type
    /// and its answer with what the method produces, so a valid program
    /// always agrees. It is checked rather than assumed because what the loop
    /// emits is a run of words at a width, and the machine copies each
    /// argument at the *callee's* parameter width — so a disagreement here
    /// would be a frame filled from the wrong number of words rather than a
    /// diagnostic.
    fn callback_matches(
        &mut self,
        callback: &Expr,
        params: &[LayoutId],
        returns: LayoutId,
        wanted: &[LayoutId],
        produces: LayoutId,
    ) -> bool {
        if params == wanted && returns == produces {
            return true;
        }
        let held = self.pool.shapes.layout(returns).name.clone();
        let want = self.pool.shapes.layout(produces).name.clone();
        self.errors.push(super::gap::gap(
            &format!(
                "a callback taking {} value(s) and answering a `{held}` where the walk hands it \
                 {} and wants a `{want}`",
                params.len(),
                wanted.len()
            ),
            callback.span,
        ));
        false
    }
}
