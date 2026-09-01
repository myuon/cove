//! `scope`, `spawn`, `lock`, `await` and `cancel`.
//!
//! A scope is an expression like any other block and its name is an ordinary
//! value slot for the length of it. What makes these four different from
//! every other call is that none of them is resolved against a declaration:
//! they are dispatched by the type the checker settled for the receiver,
//! where `Interpreter::call_task_method` dispatches by the value's own kind.
//! So there is no signature for a label to name and no supplied-set for a
//! specialisation to be keyed by, which is what [`task_arguments`] refuses
//! rather than reproduces.

use cove_diag::Span;
use cove_syntax::ast::{Block, Expr, ExprKind};

use crate::{Inst, Scalar, SlotKind, Unsupported};

use super::body::{Body, Position};
use super::call::{plain_arguments, Args};

impl<'a, 'l> Body<'a, 'l> {
    /// Lowers `scope name { ... }`.
    ///
    /// The Language Card's rule is the whole of it: leaving the scope waits
    /// for or cancels its child tasks. The scope's value is the value of its
    /// block, so a scope is an expression like any other block, and the name
    /// is an ordinary value slot for the length of it — `scope.spawn` reads
    /// its receiver the way every other method call does.
    ///
    /// The `try` written after the `leave-scope` is the whole of what a
    /// failed child does. `Interpreter::leave_scope` answers
    /// `Control::Return(Value::err(error))` for a child whose value was
    /// `Err`, which is what `?` already means here, so the instruction
    /// answers a `Result` and the `try` beside it turns one into the other.
    /// A child that *raised* never reaches the `try`: an error is not a
    /// value, and it propagates as itself.
    ///
    /// A function that answers on the scalar stack is refused rather than
    /// approximated. Every one of its returns is a `return-scalar`, and the
    /// value a failed child returns is a `Value`, so there is no stack for
    /// the failure to travel on. The oracle answers such a program — it
    /// returns an `Err` from a function declared `-> Int` — and this is one
    /// of the few places a backend is allowed to refuse what the oracle
    /// answers rather than reproduce it.
    pub(super) fn scope_expr(
        &mut self,
        name: &'a cove_syntax::ast::Ident,
        body: &'a Block,
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        if self.returns.is_scalar() {
            return Err(Unsupported::new(
                "a task scope in a function that answers an `Int` or a `Bool`",
                span,
            ));
        }
        let mark = self.scope();
        let named = self.outer.name(name.node.as_str());
        self.emit(Inst::EnterScope(named), span);
        let slot = self.declare(Some(name.node.as_str()), SlotKind::Value);
        self.emit(Inst::StoreLocal(slot), span);
        self.open_scopes += 1;
        let lowered = self.block_at(body, Position::Value);
        self.open_scopes -= 1;
        lowered?;
        self.emit(Inst::LeaveScope, span);
        // The scope's own block was lowered at `Position::Value` a few
        // lines up, whatever its tail's type is, so what this `Try` opens on
        // success is always a `SlotKind::Value` — the same reasoning the
        // `for`-loop's own `Try` carries for its `element` slot.
        self.emit(
            Inst::Try {
                payload: SlotKind::Value,
            },
            span,
        );
        self.release(mark);
        if position == Position::Effect {
            self.emit(Inst::Pop, span);
        }
        Ok(())
    }

    /// `scope.spawn { ... }`: the scope, then the work to run in it.
    ///
    /// The receiver first and then the argument, which is the order
    /// `Interpreter::eval_method_call` evaluates them in.
    pub(super) fn spawn(
        &mut self,
        receiver: &'a Expr,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        task_arguments(args, "spawn", 1, span)?;
        self.expr(receiver)?;
        self.expr(args.at(0).value)?;
        self.emit(Inst::Spawn, span);
        Ok(None)
    }

    /// `shared.lock(fn(var value) { ... })`: the cell, then the closure to
    /// run under its lock.
    ///
    /// The closure has to be written at the call, which is narrower than the
    /// oracle: `Interpreter::call_shared_method` takes whatever closure value
    /// it is handed. A `var` parameter names the cell's contents rather than
    /// receiving a copy of them, so it arrives on the place stack — and a
    /// lambda that is lowered as an ordinary value cannot have one, because
    /// every argument of an `Inst::CallValue` travels on the value stack.
    /// Lowering the lambda *here*, as the closure of this `lock`, is what
    /// makes the exception a property of one written site rather than of
    /// every closure a program could hand over.
    pub(super) fn lock(
        &mut self,
        receiver: &'a Expr,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        task_arguments(args, "lock", 1, span)?;
        let written = args.at(0).value;
        let ExprKind::Lambda {
            is_async,
            params,
            body,
        } = &written.kind
        else {
            return Err(Unsupported::new(
                "a `lock` whose closure is not written at the call",
                written.span,
            ));
        };
        if params.len() != 1 {
            return Err(Unsupported::new(
                format!(
                    "a `lock` whose closure takes {} parameter(s) rather than one",
                    params.len()
                ),
                written.span,
            ));
        }
        self.expr(receiver)?;
        self.lambda(written, *is_async, params, body, written.span, true)?;
        self.emit(Inst::Lock, span);
        Ok(None)
    }

    /// `task.await()` and `task.cancel()`, which take nothing.
    pub(super) fn task_op(
        &mut self,
        receiver: &'a Expr,
        inst: Inst,
        what: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        task_arguments(args, what, 0, span)?;
        self.expr(receiver)?;
        self.emit(inst, span);
        Ok(None)
    }
}

/// The arguments of a task operation, which takes a fixed number of plain
/// ones and nothing else.
///
/// `spawn`, `await`, `cancel` and `lock` are dispatched by the receiver's
/// kind rather than resolved against a declaration, so there is no signature
/// for a label to name and the interpreter reads one and ignores it. Refusing
/// is the direction a second backend is allowed to be wrong in.
fn task_arguments(args: Args<'_>, what: &str, takes: usize, span: Span) -> Result<(), Unsupported> {
    plain_arguments(args, what)?;
    if let Some(arg) = args.iter().find(|arg| arg.label.is_some()) {
        return Err(Unsupported::new(
            format!("a labelled argument to `{what}`, which takes none"),
            arg.span,
        ));
    }
    if args.len() != takes {
        return Err(Unsupported::new(
            format!(
                "a `{what}` given {} argument(s) where it takes {takes}",
                args.len()
            ),
            // The first argument where there is one, and the call itself
            // where there is none: a `spawn` given nothing has no argument
            // to point at, and `Args::at` would index past the end.
            args.iter().next().map_or(span, |arg| arg.span),
        ));
    }
    Ok(())
}
