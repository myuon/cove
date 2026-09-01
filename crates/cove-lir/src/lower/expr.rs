//! Expressions.
//!
//! Every expression answers a slot, and the caller says whether it wants
//! one: [`Body::expr`] produces a value, [`Body::discard`] runs the
//! expression for its effects. The second is not the first with the answer
//! thrown away — an `if` with no `else` lowered for its effects writes no
//! `()` anywhere, and an assignment statement emits nothing beyond the
//! store.
//!
//! # Short-circuiting is control flow
//!
//! `&&` and `||` are not instructions and there is no `And` or `Or` to add.
//! Their meaning is that the right-hand side *may not run*, and an
//! instruction taking two operands has already run it. So both lower to a
//! branch over the right-hand side, which is also why one conditional branch
//! is enough for the whole language: `if`, `while` and both short-circuits
//! are the same jump with the condition arranged to suit.

use cove_diag::Span;
use cove_syntax::ast::{Arg, BinaryOp, Block, Expr, ExprKind, Type, UnaryOp};

use super::frame::Val;
use super::gap;
use super::{Body, Loop, PENDING};
use crate::inst::{ArithOp, CmpOp, Compare, Inst, Num, Slot};
use crate::repr::Repr;

impl Body<'_> {
    /// Lowers `expr` and answers where its value ended up.
    pub(super) fn expr(&mut self, expr: &Expr) -> Val {
        let span = expr.span;
        match &expr.kind {
            ExprKind::Int(value) => {
                let value = *value;
                self.constant(expr, |dst| Inst::Int { dst, value })
            }
            // A `Duration` is nanoseconds in a word, so a literal one is the
            // integer instruction writing a slot the frame calls a
            // `Duration`. Only the boundary cares which name the word has.
            ExprKind::Duration(nanos) => {
                let value = *nanos;
                self.constant(expr, |dst| Inst::Int { dst, value })
            }
            ExprKind::Float(value) => {
                let bits = value.to_bits();
                self.constant(expr, |dst| Inst::Float { dst, bits })
            }
            ExprKind::Bool(value) => {
                let value = *value;
                self.constant(expr, |dst| Inst::Bool { dst, value })
            }
            ExprKind::Unit => self.constant(expr, |dst| Inst::Unit { dst }),
            ExprKind::Ident(name) => match self.frame.lookup(name) {
                Some(slot) => Val::borrowed(slot),
                None => self.gap("a name that is not a local binding", expr),
            },
            ExprKind::Unary { op, operand } => self.unary(expr, *op, operand),
            ExprKind::Binary { op, lhs, rhs } => self.binary(expr, *op, lhs, rhs),
            ExprKind::Assign { op, target, value } => {
                self.assign(*op, target, value, span);
                self.unit_value(span)
            }
            ExprKind::Block(block) => {
                let repr = self.word(expr);
                let dst = self.frame.alloc(repr);
                self.scoped_block(block, Some(dst));
                Val::temp(dst)
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let repr = self.word(expr);
                let dst = self.frame.alloc(repr);
                self.if_expr(
                    condition,
                    then_branch,
                    else_branch.as_deref(),
                    span,
                    Some(dst),
                );
                Val::temp(dst)
            }
            ExprKind::While { condition, body } => {
                self.while_expr(condition, body, span);
                self.unit_value(span)
            }
            ExprKind::Return(value) => {
                self.return_expr(value.as_deref(), span);
                self.dead(expr)
            }
            ExprKind::Break(value) => {
                self.break_expr(value.as_deref(), span);
                self.dead(expr)
            }
            ExprKind::Continue => {
                self.continue_expr(span);
                self.dead(expr)
            }
            ExprKind::Call {
                callee,
                generics,
                args,
                trailing,
            } => self.call(expr, callee, generics, args, trailing.is_some()),

            ExprKind::Str(_) => self.gap("a string", expr),
            ExprKind::ArrayLit(_) => self.gap("an array literal", expr),
            ExprKind::Field { .. } => self.gap("a field access", expr),
            ExprKind::Try(_) => self.gap("`?`", expr),
            ExprKind::Await(_) => self.gap("`await`", expr),
            ExprKind::Match { .. } => self.gap("`match`", expr),
            ExprKind::For { .. } => self.gap("`for`", expr),
            ExprKind::Lambda { .. } => self.gap("a lambda", expr),
            ExprKind::Scope { .. } => self.gap("`scope`", expr),
            ExprKind::Range { .. } => self.gap("a range", expr),
        }
    }

    /// Lowers `expr` for what it does rather than for what it answers.
    ///
    /// The forms named here are the ones whose value is `()` and is
    /// manufactured rather than computed. Lowering them through
    /// [`Body::expr`] and dropping the answer would emit an instruction per
    /// statement whose only effect is to write a zero into a slot nothing
    /// reads.
    pub(super) fn discard(&mut self, expr: &Expr) {
        match &expr.kind {
            ExprKind::Assign { op, target, value } => self.assign(*op, target, value, expr.span),
            ExprKind::Block(block) => self.scoped_block(block, None),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => self.if_expr(
                condition,
                then_branch,
                else_branch.as_deref(),
                expr.span,
                None,
            ),
            ExprKind::While { condition, body } => self.while_expr(condition, body, expr.span),
            _ => {
                let value = self.expr(expr);
                self.frame.release(value);
            }
        }
    }

    // ---- literals -------------------------------------------------------

    /// A value that is entirely in the instruction: a slot of the right kind
    /// and one instruction writing it.
    fn constant(&mut self, expr: &Expr, inst: impl FnOnce(Slot) -> Inst) -> Val {
        let repr = self.word(expr);
        let dst = self.frame.alloc(repr);
        self.emit(inst(dst), expr.span);
        Val::temp(dst)
    }

    /// The `()` a form answers when its value was never computed from
    /// anything: an assignment, a loop.
    fn unit_value(&mut self, span: Span) -> Val {
        let dst = self.frame.alloc(Repr::Unit);
        self.emit(Inst::Unit { dst }, span);
        Val::temp(dst)
    }

    // ---- operators -------------------------------------------------------

    fn unary(&mut self, expr: &Expr, op: UnaryOp, operand: &Expr) -> Val {
        let a = self.expr(operand);
        let repr = self.word(expr);
        let dst = self.frame.alloc(repr);
        let inst = match op {
            UnaryOp::Not => Inst::Not { dst, a: a.slot },
            UnaryOp::Neg => Inst::Neg {
                num: num_of(repr),
                dst,
                a: a.slot,
            },
        };
        self.emit(inst, expr.span);
        self.frame.release(a);
        Val::temp(dst)
    }

    fn binary(&mut self, expr: &Expr, op: BinaryOp, lhs: &Expr, rhs: &Expr) -> Val {
        match op {
            BinaryOp::And => self.short_circuit(expr, lhs, rhs, true),
            BinaryOp::Or => self.short_circuit(expr, lhs, rhs, false),
            BinaryOp::Is => self.gap("`is`", expr),
            _ => self.operator(expr, op, lhs, rhs),
        }
    }

    /// An arithmetic or comparison operator: two operands, one instruction.
    ///
    /// Which numeric reading the instruction gives its operands comes from
    /// the operands' own `Repr`, which is the checker's answer written down.
    /// There is no coercion to decide about: the checker refuses a mixed
    /// pair outright — "arithmetic combines two values of the same type" —
    /// so no valid program reaches here needing an [`Inst::Convert`], and
    /// inventing one would be the lowering deciding something the language
    /// did not.
    fn operator(&mut self, expr: &Expr, op: BinaryOp, lhs: &Expr, rhs: &Expr) -> Val {
        let a = self.expr(lhs);
        let b = self.expr(rhs);
        let operand = self.frame.repr(a.slot);
        let repr = self.word(expr);
        let dst = self.frame.alloc(repr);
        let inst = match arith_of(op) {
            Some(op) => Inst::Arith {
                num: num_of(operand),
                op,
                dst,
                a: a.slot,
                b: b.slot,
            },
            // `()` compares equal to `()` and there is nothing to look at,
            // so the answer is the instruction. Both operands were still
            // evaluated, because either of them may have done something.
            None if operand == Repr::Unit => Inst::Bool {
                dst,
                value: op == BinaryOp::Eq,
            },
            None => Inst::Cmp {
                on: compare_of(operand),
                op: cmp_of(op),
                dst,
                a: a.slot,
                b: b.slot,
            },
        };
        self.emit(inst, expr.span);
        self.frame.release(b);
        self.frame.release(a);
        Val::temp(dst)
    }

    /// `&&` and `||`, which are a branch over the right-hand side.
    ///
    /// Both answer the left-hand side's word when it already settles the
    /// question, so the answer slot is written before the branch and the
    /// right-hand side overwrites it only when it runs. `conjunction` says
    /// which way round: `&&` skips the right-hand side when the left is
    /// false, `||` when it is true.
    fn short_circuit(&mut self, expr: &Expr, lhs: &Expr, rhs: &Expr, conjunction: bool) -> Val {
        let dst = self.frame.alloc(Repr::Bool);
        let a = self.expr(lhs);
        self.store(dst, &a, lhs);
        self.frame.release(a);

        let branch = self.emit(
            Inst::BranchFalse {
                cond: dst,
                to: PENDING,
            },
            expr.span,
        );
        // `||` wants the opposite polarity, and the instruction set carries
        // only one. Rather than add the other, the false case falls straight
        // into the right-hand side and the jump that skips it is the one
        // taken when the left-hand side already answered `true`.
        let skip = if conjunction {
            None
        } else {
            let skip = self.emit(Inst::Jump { to: PENDING }, expr.span);
            let rest = self.here();
            self.patch(branch, rest);
            Some(skip)
        };

        let b = self.expr(rhs);
        self.store(dst, &b, rhs);
        self.frame.release(b);

        let end = self.here();
        match skip {
            Some(skip) => self.patch(skip, end),
            None => self.patch(branch, end),
        }
        Val::temp(dst)
    }

    // ---- assignment -------------------------------------------------------

    fn assign(&mut self, op: Option<BinaryOp>, target: &Expr, value: &Expr, span: Span) {
        let ExprKind::Ident(name) = &target.kind else {
            self.errors
                .push(gap::gap("an assignment to a field or an element", span));
            self.discard(value);
            return;
        };
        let Some(slot) = self.frame.lookup(name) else {
            self.errors.push(gap::gap(
                "an assignment to something outside the frame",
                span,
            ));
            self.discard(value);
            return;
        };

        let source = self.expr(value);
        match op {
            None => {
                self.store(slot, &source, value);
            }
            Some(op) => match arith_of(op) {
                Some(op) => {
                    let num = num_of(self.frame.repr(slot));
                    self.emit(
                        Inst::Arith {
                            num,
                            op,
                            dst: slot,
                            a: slot,
                            b: source.slot,
                        },
                        span,
                    );
                }
                None => self
                    .errors
                    .push(gap::gap("a compound assignment with this operator", span)),
            },
        }
        self.frame.release(source);
    }

    // ---- control flow -------------------------------------------------------

    /// `if`, with or without an answer.
    ///
    /// `dst` is `None` where the value is not wanted, and the two shapes
    /// differ by more than that: an `if` with no `else` answers `()`
    /// whichever way it goes, so its answer is written once before the
    /// branch rather than on both paths.
    fn if_expr(
        &mut self,
        condition: &Expr,
        then_branch: &Block,
        else_branch: Option<&Expr>,
        span: Span,
        dst: Option<Slot>,
    ) {
        if let (Some(dst), None) = (dst, else_branch) {
            self.emit(Inst::Unit { dst }, span);
        }

        let cond = self.expr(condition);
        let branch = self.emit(
            Inst::BranchFalse {
                cond: cond.slot,
                to: PENDING,
            },
            condition.span,
        );
        self.frame.release(cond);

        // Only a branch that has somewhere to put an answer produces one.
        // Without an `else` the answer is already written, so the `then`
        // side runs for its effects.
        let taken = else_branch.and(dst);
        self.scoped_block(then_branch, taken);

        match else_branch {
            None => {
                let end = self.here();
                self.patch(branch, end);
            }
            Some(otherwise) => {
                let skip = self.emit(Inst::Jump { to: PENDING }, span);
                let alternative = self.here();
                self.patch(branch, alternative);
                match dst {
                    Some(dst) => {
                        let value = self.expr(otherwise);
                        self.store(dst, &value, otherwise);
                        self.frame.release(value);
                    }
                    None => self.discard(otherwise),
                }
                let end = self.here();
                self.patch(skip, end);
            }
        }
    }

    /// `while`, which answers `()` however it ends and so writes nothing.
    fn while_expr(&mut self, condition: &Expr, body: &Block, span: Span) {
        // The condition is inside the loop, because `continue` has to
        // re-decide whether there is another turn rather than assume one.
        let head = self.here();
        let cond = self.expr(condition);
        let branch = self.emit(
            Inst::BranchFalse {
                cond: cond.slot,
                to: PENDING,
            },
            condition.span,
        );
        self.frame.release(cond);

        self.loops.push(Loop {
            head,
            depth: self.frame.depth(),
            breaks: Vec::new(),
        });
        self.scoped_block(body, None);
        self.emit(Inst::Jump { to: head }, span);

        let end = self.here();
        self.patch(branch, end);
        let finished = self.loops.pop().expect("the loop was pushed above");
        for at in finished.breaks {
            self.patch(at, end);
        }
    }

    fn return_expr(&mut self, value: Option<&Expr>, span: Span) {
        match value {
            Some(value) => {
                let answer = self.expr(value);
                // `return return x` leaves through the inner one, and the
                // outer has no word to name.
                if !self.diverges(value) {
                    self.emit(Inst::Return { src: answer.slot }, span);
                }
                self.frame.release(answer);
            }
            // A bare `return` only checks in a function answering `()`, and
            // the answer slot holds one: a frame is zeroed on entry and
            // nothing but a `()` is ever written to a `Unit` slot.
            None => {
                let src = self.answer;
                self.emit(Inst::Return { src }, span);
            }
        }
    }

    fn break_expr(&mut self, value: Option<&Expr>, span: Span) {
        // `break expr` leaves the loop with `()` like any other `break`; the
        // expression is evaluated because it may do something, and its value
        // is dropped because there is nowhere for it to go.
        if let Some(value) = value {
            self.discard(value);
        }
        let Some(depth) = self.loops.last().map(|it| it.depth) else {
            self.errors.push(gap::gap("a `break` outside a loop", span));
            return;
        };
        let clears = self.frame.refs_within(depth);
        self.clear(&clears, span);
        let at = self.emit(Inst::Jump { to: PENDING }, span);
        self.loops
            .last_mut()
            .expect("the loop was found above")
            .breaks
            .push(at);
    }

    fn continue_expr(&mut self, span: Span) {
        let Some((head, depth)) = self.loops.last().map(|it| (it.head, it.depth)) else {
            self.errors
                .push(gap::gap("a `continue` outside a loop", span));
            return;
        };
        let clears = self.frame.refs_within(depth);
        self.clear(&clears, span);
        self.emit(Inst::Jump { to: head }, span);
    }

    // ---- calls -------------------------------------------------------------

    /// A call to a declared function.
    ///
    /// Which declaration it is was settled by the checker, and the way to
    /// read that answer is the type it did *not* record: a callee it gave a
    /// type to is a call through a value, and a callee it gave none to named
    /// a declaration. Asking that rather than matching on the callee's shape
    /// is what keeps a name that happens to look like a function from being
    /// mistaken for one.
    fn call(
        &mut self,
        expr: &Expr,
        callee: &Expr,
        generics: &[Type],
        args: &[Arg],
        trailing: bool,
    ) -> Val {
        if !generics.is_empty() {
            return self.gap("a call with explicit type arguments", expr);
        }
        if trailing {
            return self.gap("a call with a trailing lambda", expr);
        }
        if self.ty(callee).is_some() {
            return self.gap("a call through a function value", expr);
        }
        let ExprKind::Ident(name) = &callee.kind else {
            return self.gap("a call to something other than a declared function", expr);
        };
        let Some(id) = self.plan.resolve(self.checked, self.module, name) else {
            return self.gap(
                "a call to a declaration that is not a function of this package",
                expr,
            );
        };
        let Some((arity, returns)) = self.plan.boundary(id) else {
            // The declaration itself is a gap, already reported where it is
            // written. Saying so again at every call site would bury it.
            return self.dead(expr);
        };
        for arg in args {
            let what = if arg.label.is_some() {
                "a labelled argument"
            } else if arg.is_var {
                "a `var` argument"
            } else if arg.spread {
                "a spread argument"
            } else {
                continue;
            };
            return self.gap(what, expr);
        }
        if args.len() != arity {
            return self.gap("a call that leaves a parameter to its default", expr);
        }

        // Each argument is evaluated in source order into a temporary of its
        // own, and every one is held until the call is emitted: the list
        // `Call` names has to be live all at once, because the machine
        // copies it into the callee's frame.
        let mut held = Vec::with_capacity(args.len());
        for arg in args {
            held.push(self.expr(&arg.value));
        }
        let slots: Vec<Slot> = held.iter().map(|value| value.slot).collect();
        let list = self.args.intern(slots);
        let dst = self.frame.alloc(returns);
        self.emit(
            Inst::Call {
                dst,
                callee: id,
                args: list,
            },
            expr.span,
        );
        for value in held.into_iter().rev() {
            self.frame.release(value);
        }
        Val::temp(dst)
    }
}

/// Which numeric reading an instruction gives a word of this kind.
///
/// A `Duration` reads as an integer: nanoseconds add like integers, and only
/// the boundary cares that the answer is called a `Duration`. Nothing else
/// arrives here in a program that lowered without an error.
fn num_of(repr: Repr) -> Num {
    match repr {
        Repr::Float => Num::Float,
        _ => Num::Int,
    }
}

fn compare_of(repr: Repr) -> Compare {
    match repr {
        Repr::Float => Compare::Float,
        Repr::Bool => Compare::Bool,
        _ => Compare::Int,
    }
}

fn arith_of(op: BinaryOp) -> Option<ArithOp> {
    match op {
        BinaryOp::Add => Some(ArithOp::Add),
        BinaryOp::Sub => Some(ArithOp::Sub),
        BinaryOp::Mul => Some(ArithOp::Mul),
        BinaryOp::Div => Some(ArithOp::Div),
        BinaryOp::Rem => Some(ArithOp::Rem),
        _ => None,
    }
}

fn cmp_of(op: BinaryOp) -> CmpOp {
    match op {
        BinaryOp::Ne => CmpOp::Ne,
        BinaryOp::Lt => CmpOp::Lt,
        BinaryOp::Le => CmpOp::Le,
        BinaryOp::Gt => CmpOp::Gt,
        BinaryOp::Ge => CmpOp::Ge,
        _ => CmpOp::Eq,
    }
}
