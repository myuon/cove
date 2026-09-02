//! `Shared(value)` and `lock`: the one handle two tasks reach one value
//! through.
//!
//! [ADR 0008](../../../../docs/adr/0008-concurrent-task-execution.md) makes
//! `Shared<T>` the value that crosses a task boundary by *sharing* rather than
//! by copying, and issue #240's Q1 says where it lives here: **an ordinary
//! Cove-owned object in the run's one heap**, whose lock is one of its own
//! words. So a cell needs no store of its own, nothing keyed by address, and
//! no lifetime running beside the collector's.
//!
//! # `Shared(value)` is an allocation and a store
//!
//! There is no instruction for it, and that is worth saying rather than
//! leaving to be noticed. A cell is a heap object whose payload is a lock word
//! and then the wrapped value inline, [`Inst::Alloc`] zeroes a payload — which
//! is exactly "no task holds this cell" — and [`Inst::StoreField`] writes a
//! run of words at a static offset. The two already say the whole of it, and
//! an instruction that meant *those two* would be a third spelling of a thing
//! the IR can express twice.
//!
//! # `lock` is two instructions and a call between them
//!
//! `docs/LINEAR_VM.md`: *"`Shared.lock` is the same shape again: acquire,
//! call, release, with the release an obligation on every exit path exactly as
//! `Clear` is."* The reason is the one **a builtin never calls back into
//! Cove** gives — a builtin that ran the closure itself would put a Rust frame
//! under every Cove frame the closure made — so the call is an ordinary
//! [`Inst::CallClosure`] and the acquire and the release are
//! [`Inst::SharedLock`] and [`Inst::SharedUnlock`] around it.
//!
//! The lowering owes the release on every path it can leave by, and between
//! the two there is exactly one instruction that can leave: the call. A
//! runtime error is not a jump this crate emits — it unwinds the machine — so
//! that path is the machine's to answer, once, for every cell the task was
//! holding, exactly as it is for a task scope. There is nothing else in the
//! region: the receiver and the closure are both evaluated before the cell is
//! taken, which is also the order the oracle evaluates them in.
//!
//! # What the closure is handed
//!
//! `cell.lock(fn(var value) { ... })` gets the **address** of the cell's own
//! value words and writes where they lie; nothing is copied in or out. A
//! closure written without `var` gets a copy, and what it does to the copy is
//! not stored back — which is `Interpreter::call_shared_method` exactly: it
//! reads `params.first().is_var` off the written lambda and passes an
//! `ArgSlot::Alias` or an `ArgSlot::Value` accordingly, and the value it
//! stores back afterwards is the place it made, untouched on the second path.
//!
//! # A cycle through a cell is an ordinary cycle
//!
//! [ADR 0037](../../../../docs/adr/0037-a-cycle-through-a-cell-is-an-ordinary-cycle.md)
//! decides it, and what it means here is that nothing in this module looks at
//! what the closure left. A cell that comes to hold a handle to itself is a
//! cycle in the run's one traced heap, and the collector that ADR 0011's
//! amendment deferred is the collector that is running.
//!
//! Reentrant locking is a different question and is still refused: a task
//! asking for a cell it already holds is a live lock state, which no collector
//! can answer. That refusal is the machine's, in
//! [`crate::Inst::SharedLock`]'s arm, because the state word is what knows.

use cove_diag::Span;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Expr, ExprKind};

use super::frame::Val;
use super::shapes;
use super::Body;
use crate::inst::{Inst, Len};
use crate::layout::{LayoutId, SHARED_VALUE};

impl Body<'_> {
    /// `Shared(value)`: a cell holding a copy of `value`'s words.
    ///
    /// The payload is zeroed by the allocation, so the lock word says "no task
    /// holds this cell" before anything is written into it — and a collection
    /// between the two reads null out of a reference word of a half-built
    /// cell rather than whatever preceded the object in the heap.
    pub(super) fn shared_new(&mut self, expr: &Expr, ty: &Ty, args: &[Arg]) -> Val {
        if args.len() != 1 || args[0].label.is_some() || args[0].spread || args[0].is_var {
            return self.gap("a `Shared` that is not given one plain value", expr);
        }
        let Ty::Shared(inner) = ty else {
            return self.gap("a `Shared` the checker settled no cell type for", expr);
        };
        let inner = (**inner).clone();
        let (Some(cell), Some(value)) =
            (self.layout(ty, expr.span), self.layout(&inner, expr.span))
        else {
            return self.dead(expr);
        };

        let held = self.expr(&args[0].value);
        let held = self.erase(held, &args[0].value, &inner);
        let held = self.fit(held, value, args[0].value.span);
        let dst = self.temp(cell);
        self.emit(
            Inst::Alloc {
                dst: dst.slot,
                layout: cell,
                len: Len::Fixed,
            },
            expr.span,
        );
        self.emit(
            Inst::StoreField {
                obj: dst.slot,
                at: SHARED_VALUE,
                src: held.slot,
                layout: value,
            },
            expr.span,
        );
        self.release(held, expr.span);
        dst
    }

    /// A method written on a `Shared`, which is `lock` and nothing else.
    ///
    /// `cove-sema` says so — *"`lock` is a `Shared`'s only operation: every
    /// access to the value it holds is scoped, so there is no `get` and no
    /// `set`"* — so a checked program never writes another name here, and one
    /// that somehow did is named as work rather than refused in the checker's
    /// words.
    pub(super) fn shared_method(
        &mut self,
        expr: &Expr,
        base: &Expr,
        name: &str,
        args: &[Arg],
    ) -> Val {
        if name != "lock" {
            return self.gap(&format!("`Shared.{name}`"), expr);
        }
        self.shared_lock(expr, base, args)
    }

    /// `cell.lock(fn(var value) { ... })`: acquire, call, release.
    fn shared_lock(&mut self, expr: &Expr, base: &Expr, args: &[Arg]) -> Val {
        let span = expr.span;
        if args.len() != 1 {
            return self.gap("a `lock` that is not given one closure", expr);
        }
        let Some(Ty::Shared(inner)) = self.settled_ty(base) else {
            return self.gap("a `lock` on something that is not a `Shared`", expr);
        };
        let inner = (*inner).clone();
        let Some(value) = self.layout(&inner, base.span) else {
            return self.dead(expr);
        };
        let Some(func) = self.callback(&args[0].value) else {
            return self.dead(expr);
        };
        let Some((params, returns)) = self.signature(&func, args[0].value.span) else {
            return self.dead(expr);
        };
        let answer = self.layout_of(expr);
        // The checker derived the closure's parameter from what the cell
        // wraps and the call's type from what the closure answers, so a valid
        // program always agrees. It is checked rather than assumed for
        // `Body::callback_matches`'s reason: what is emitted is a run of words
        // at a width, and a disagreement would be a frame filled from the
        // wrong number of words rather than a diagnostic.
        if params != [value] || returns != answer {
            return self.gap(
                "a `lock` closure that does not take what the cell holds and answer what the \
                 call was settled at",
                expr,
            );
        }

        // Both operands are evaluated before the cell is taken, which is the
        // order `Interpreter::call_shared_method` evaluates them in: the
        // receiver, then `eval_args`, then `SharedCell::lock`. It is also what
        // leaves the held region with no instruction in it that can jump.
        let cell = self.expr(base);
        let (closure, aliases) = self.lock_closure(&args[0].value, value);

        self.emit(Inst::SharedLock { cell: cell.slot }, span);
        let addr = self.temp(shapes::ADDR);
        self.emit(
            Inst::AddrOfField {
                dst: addr.slot,
                obj: cell.slot,
                at: SHARED_VALUE,
            },
            span,
        );
        let operand = match aliases {
            true => Val::borrowed(addr.slot, shapes::ADDR),
            false => self.load_wrapped(&addr, value, span),
        };
        let dst = self.temp(answer);
        self.call_closure(dst.slot, closure.slot, vec![operand.arg()], span);

        // The copy the closure was handed, then the address it was reached
        // through, and only then the cell — an address into an object is live
        // for exactly as long as the lock that made it safe to hold.
        self.release(operand, span);
        self.release(addr, span);
        self.emit(Inst::SharedUnlock { cell: cell.slot }, span);
        self.release(closure, span);
        self.release(cell, span);
        dst
    }

    /// The value a closure that did not write `var` is handed: a copy of the
    /// cell's words, which nothing stores back.
    fn load_wrapped(&mut self, addr: &Val, value: LayoutId, span: Span) -> Val {
        let dst = self.temp(value);
        self.emit(
            Inst::Load {
                dst: dst.slot,
                addr: addr.slot,
                layout: value,
            },
            span,
        );
        dst
    }

    /// The closure a `lock` runs, and whether it takes the cell's value by
    /// alias.
    ///
    /// A written `fn(var value) { ... }` is the only thing that aliases, and
    /// the *written* list is the only thing that says so: the function type
    /// the checker settled drops `var`, which is why
    /// `Interpreter::call_shared_method` reads the same question off the same
    /// place — `params.first().is_some_and(|param| param.is_var)` of the
    /// lambda's own parameters.
    ///
    /// Anything else — a name bound to a closure, a declared function used as
    /// a value — is an ordinary function value and is handed a copy, which is
    /// the answer the oracle gives for the same expression.
    fn lock_closure(&mut self, callback: &Expr, behind: LayoutId) -> (Val, bool) {
        let ExprKind::Lambda {
            is_async: false,
            params,
            body,
        } = &callback.kind
        else {
            return (self.expr(callback), false);
        };
        if !params.first().is_some_and(|param| param.is_var) {
            return (self.expr(callback), false);
        }
        (self.aliasing_lambda(callback, params, body, behind), true)
    }
}
