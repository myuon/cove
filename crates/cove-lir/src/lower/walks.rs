//! `map`, `filter` and `fold`: the sequence methods that take a closure.
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
//! So each of the three **lowers to a loop in the IR**, and the closure's
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
//!   overwrote.
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
use crate::inst::{ArithOp, CmpOp, Compare, Inst, Len, Num, Pc};
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
    /// One of the three, over elements this caller has already settled.
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
        let Some(answer) = self.owned_ty(expr) else {
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
        let Some(answer) = self.owned_ty(expr) else {
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
        let Some(answer) = self.owned_ty(expr) else {
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
