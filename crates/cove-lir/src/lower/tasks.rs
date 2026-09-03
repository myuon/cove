//! `scope`, `spawn`, `await` and `cancel`.
//!
//! The Language Card states the whole contract in one sentence — *concurrent
//! work belongs to a task scope, and leaving the scope waits for or cancels
//! its child tasks* — and everything here is that sentence taken apart into
//! the places a lowering has to put it.
//!
//! # A scope has two exits and the lowering owes both
//!
//! One is where the `scope` is written: the body reached its end, and
//! [`Inst::ScopeLeave`] waits for every child that the body did not already
//! await. The other is anywhere at all: a `return`, a `?`, a `break` or a
//! `continue` that leaves the body part way through, and that one is
//! [`Inst::ScopeCancel`].
//!
//! The second is the reason [`Body::scopes`] exists, and it is the same
//! obligation [`Inst::Clear`] is under for a different reason. A static
//! reference map cannot say when a value stopped being needed; a flat
//! instruction stream cannot say which scopes a jump is leaving. Both answers
//! are put in the data, by the one pass that knows.
//!
//! A runtime error is the third way out and is deliberately **not** here. It
//! is not a jump the lowering emits — it unwinds the machine — so the machine
//! is what cancels and joins on that path, once, for every scope the task had
//! open. Emitting something per instruction that could fail would be paying
//! on every instruction for the path that ends the run.
//!
//! # What a failing child does, and why it is a value rather than a jump
//!
//! `crate::task::wait_for_children` in the runtime is the oracle, and it
//! distinguishes two failures. A child that **raised** propagates as itself,
//! which is a runtime error and not a value, so [`Inst::ScopeLeave`] simply
//! fails with it. A child whose value was `Err(e)` returns that error *from
//! the enclosing function*, exactly as `?` would — and where that goes is a
//! fact about the function the scope was written in rather than about the
//! scope. So the instruction answers a `Bool` and a payload, and the branch,
//! the `Err` it is wrapped in and the `Return` are emitted here, beside the
//! ones `?` emits, out of [`Body::failure`].
//!
//! # A `spawn` orders nothing
//!
//! ADR 0008's amendment is explicit that whether the child has run an
//! instruction by the time the parent's next statement runs is the operating
//! system's answer. Nothing here waits, and nothing here is a scheduling
//! policy: `await`, leaving a scope, and a `Shared`'s lock are the orderings
//! a program can ask for, and a program asks for them by writing them.

use cove_diag::Span;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Block, Expr, Ident};

use super::frame::Val;
use super::{gap, shapes, Body, Dest, OpenScope, PENDING};
use crate::inst::Inst;
use crate::layout::LayoutId;

impl Body<'_> {
    // ---- the scope itself --------------------------------------------------

    /// `scope name { ... }`, whose value is the value of its block.
    ///
    /// The name is bound in a scope of its own outside the block's, which is
    /// where the oracle binds it: `eval_scope` pushes an environment, declares
    /// the handle in it, and evaluates the block — which pushes another. So a
    /// binding the block makes shadows the scope's name and the scope's name
    /// outlives every one of them.
    pub(super) fn scope_expr(&mut self, expr: &Expr, name: &Ident, body: &Block) -> Val {
        let span = expr.span;
        let layout = self.layout_of(expr);
        let dst = self.temp(layout);

        let handle = self.temp(shapes::SCOPE);
        let named = self.string(&name.node);
        self.emit(
            Inst::ScopeEnter {
                dst: handle.slot,
                name: named,
            },
            span,
        );

        self.frame.push_scope();
        self.frame.bind(&name.node, handle.slot, shapes::SCOPE);
        self.scopes.push(OpenScope {
            slot: handle.slot,
            loops: self.loops.len(),
            can_fail: false,
        });
        self.scoped_block(body, Some(Dest::of(&dst)));
        // Asked *after* the body, because what a scope's children answer is
        // something the body says and the header does not.
        let open = self.scopes.pop().expect("this scope was pushed");
        let clears = self.frame.pop_scope();
        self.clear(&clears, span);

        // The body reached its end, so this is the exit that waits rather
        // than the one that cancels.
        let failure = if open.can_fail {
            match self.child_failure_layout(span) {
                Some(failure) => failure,
                None => return self.dead(expr),
            }
        } else {
            // No child of this scope answers a `Result`, so `failed` is
            // false by construction: the machine reads a child's answer
            // layout and reports a failure only from an enum with an `Err`
            // case. The instruction still needs a layout for the location it
            // would have written, and `Unit` is the honest one — nothing.
            shapes::UNIT
        };
        let failed = self.temp(shapes::BOOL);
        let error = self.temp(failure);
        self.emit(
            Inst::ScopeLeave {
                scope: handle.slot,
                failed: failed.slot,
                error: error.slot,
                layout: failure,
            },
            span,
        );
        if open.can_fail {
            let carry_on = self.emit(
                Inst::BranchFalse {
                    cond: failed.slot,
                    to: PENDING,
                },
                span,
            );
            // A child's `Err` leaves the enclosing function the way `?` does,
            // so it is built the way `?` builds one. Every scope *outside*
            // this one is left on the way, because this is a `return`.
            let payload = Val::borrowed(error.slot, failure);
            if let Some(answer) = self.failure(Some(payload), span) {
                self.leave_open_scopes(0, span);
                self.emit(Inst::Return { src: answer.slot }, span);
                self.give_back(answer.slot, answer.layout);
            }
            let after = self.here();
            self.patch(carry_on, after);
        }

        self.give_back(failed.slot, failed.layout);
        // The error location is written only on the path that has already
        // returned, so on this one it is still the zeroes the frame was
        // given. It is released rather than merely given back all the same:
        // whether a location was written is a fact about a path, and the run
        // going back on a free list is a fact about the rest of the body.
        self.release(error, span);
        self.give_back(handle.slot, handle.layout);
        dst
    }

    /// The layout of the `Err` payload a failing child would be wrapped in.
    ///
    /// It is the *enclosing function's*, not the child's: what a scope does
    /// with a child whose value was `Err(e)` is return `Err(e)` from the
    /// function the scope was written in, so the words have to fit that
    /// function's own failure. The machine holds the child's answer to this
    /// same layout and refuses a disagreement, which is where a child
    /// answering a `Result` of some other error type is caught.
    ///
    /// A function that answers neither a `Result` nor an `Option` has no
    /// failure to return, and the oracle answers one anyway — a `Value::err`
    /// out of a function typed `Unit`, which nothing downstream can read as
    /// what it is. That is a gap rather than an invention here.
    fn child_failure_layout(&mut self, span: Span) -> Option<LayoutId> {
        let returns = self.returns.clone();
        let Ty::Result(_, error) = &returns else {
            self.errors.push(gap::gap(
                "a `scope` in a function that does not answer a `Result`, which has no failure a \
                 child could be passed on through",
                span,
            ));
            return None;
        };
        self.layout(error, span)
    }

    /// Cancels every scope open at or inside `from`, innermost first.
    ///
    /// What a jump out of a scope's body owes it. `from` is `0` for a
    /// `return` — which leaves every scope this body has open — and the count
    /// of scopes that were open outside the loop for a `break` or a
    /// `continue`, which leaves only the ones the loop's own turn opened.
    ///
    /// Innermost first, because that is the order the bodies would have
    /// finished in: an inner scope is inside the outer one's body, and the
    /// outer one cannot be waited for until the inner one has been.
    pub(super) fn leave_open_scopes(&mut self, from: usize, span: Span) {
        let leaving: Vec<u32> = self.scopes[from..]
            .iter()
            .rev()
            .map(|scope| scope.slot)
            .collect();
        for scope in leaving {
            self.emit(Inst::ScopeCancel { scope }, span);
        }
    }

    /// How many scopes were open outside the innermost loop, which is how far
    /// a `break` or a `continue` has to cancel back to.
    pub(super) fn scopes_outside_this_loop(&self) -> usize {
        let depth = self.loops.len();
        self.scopes
            .iter()
            .take_while(|scope| scope.loops < depth)
            .count()
    }

    // ---- the handle's operations -------------------------------------------

    /// `tasks.spawn { ... }`: a thread for the body, and the handle the scope
    /// now owns.
    pub(super) fn scope_method(
        &mut self,
        expr: &Expr,
        base: &Expr,
        name: &str,
        args: &[Arg],
    ) -> Val {
        if name != "spawn" {
            return self.gap(&format!("`TaskScope.{name}`"), expr);
        }
        if args.len() != 1 {
            return self.gap("a `spawn` that is not given one closure", expr);
        }
        let Some(Ty::Task(produced)) = self.ty(expr) else {
            return self.gap("a `spawn` the checker did not settle a task type for", expr);
        };
        let Some(answer) = self.layout(&produced, expr.span) else {
            return self.dead(expr);
        };
        // The same question `Checker::spawned` asks: only a child whose value
        // is a `Result` can hand the scope a failure to pass on.
        if matches!(produced.as_ref(), Ty::Result(..)) {
            if let Some(open) = self.scopes.last_mut() {
                open.can_fail = true;
            }
        }
        let scope = self.expr(base);
        let closure = self.expr(&args[0].value);
        let dst = self.temp(shapes::TASK);
        self.emit(
            Inst::Spawn {
                dst: dst.slot,
                scope: scope.slot,
                closure: closure.slot,
                answer,
            },
            expr.span,
        );
        self.release(closure, expr.span);
        self.release(scope, expr.span);
        dst
    }

    /// `task.await()` and `task.cancel()`.
    pub(super) fn task_method(
        &mut self,
        expr: &Expr,
        base: &Expr,
        name: &str,
        args: &[Arg],
    ) -> Val {
        if !args.is_empty() {
            return self.gap(&format!("`Task.{name}` given arguments"), expr);
        }
        match name {
            "await" => self.settle(expr, base),
            "cancel" => {
                let task = self.expr(base);
                self.emit(Inst::Cancel { task: task.slot }, expr.span);
                self.release(task, expr.span);
                self.unit_value(expr.span)
            }
            _ => self.gap(&format!("`Task.{name}`"), expr),
        }
    }

    /// The handle a call to an `async fn` answers, around the value the call
    /// already produced.
    ///
    /// # Where the task starts, and what joins it
    ///
    /// It starts and finishes at the [`Inst::Call`] above this one, on this
    /// task's own stack, before this instruction runs — and nothing joins it,
    /// because there is nothing to join. `Interpreter::call_target` is the
    /// oracle and it says so in as many words: the body runs at the call
    /// site, and the handle it hands back is settled on creation.
    ///
    /// So an `async fn` belongs to **no scope**. That is not a gap ADR 0008
    /// left open — it is what the ADR decides, in the sentence that gives a
    /// thread to `spawn` "which is where the language says concurrency
    /// begins". A scope waits for or cancels its children, and a call that
    /// has already finished has nothing for either to do.
    ///
    /// What is left over is the *handle*, and it is a value: it can be stored
    /// in a `Vector<Task<T>>`, returned, or awaited twice. So it needs a name
    /// the machine can hold, and the scheduler table is where a `Repr::Task`
    /// word's name lives whether a thread ever existed or not.
    ///
    /// # Why the call is not folded into an immediate `await`
    ///
    /// `await f()` could skip the handle entirely, and the answer would be
    /// the same one — an `await` of a settled task is the value. It is not
    /// done, because then a call would be lowered two ways depending on what
    /// its result was written next to, and the two would have to be shown to
    /// agree at every other place a call can appear. One shape is what makes
    /// `let t = f()` and `await f()` the same thing said twice.
    pub(super) fn as_task(&mut self, value: Val, span: Span) -> Val {
        let dst = self.temp(shapes::TASK);
        self.emit(
            Inst::Settled {
                dst: dst.slot,
                src: value.slot,
                answer: value.layout,
            },
            span,
        );
        self.release(value, span);
        dst
    }

    /// `await task`, and the postfix `task.await()` that means the same
    /// thing.
    pub(super) fn settle(&mut self, expr: &Expr, task: &Expr) -> Val {
        if !matches!(self.ty(task), Some(Ty::Task(_))) {
            return self.gap("an `await` of something that is not a task", expr);
        }
        let answer = self.layout_of(expr);
        let handle = self.expr(task);
        let dst = self.temp(answer);
        self.emit(
            Inst::Await {
                dst: dst.slot,
                task: handle.slot,
                answer,
            },
            expr.span,
        );
        self.release(handle, expr.span);
        dst
    }
}
