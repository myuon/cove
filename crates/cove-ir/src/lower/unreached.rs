//! A form whose operand leaves before the form is reached.
//!
//! `!return x`, `f(1, break)`, `if continue { .. }`: the checker gives a
//! diverging expression the type `Never` and accepts it wherever a value of
//! any type goes, so a `return`, a `break`, a `continue` — or a form built
//! only out of them — may stand as the operand of anything. Everything the
//! surrounding form would do after evaluating that operand is unreachable.
//!
//! The form itself is still the wrong thing to lower. A diverging operand
//! answers [`Body::dead`]'s location, whose layout is `Never`'s one word of
//! `Unit`, and the form's instruction wants its operand's own type: `Not`
//! reading a `Unit` slot is a program the verifier rejects (issue 698), and
//! an argument of `Unit` passed where an `Int` goes is a conversion
//! [`Body::fit`] reports as a gap. Neither is a fact about the program —
//! the code is dead — so neither should be said.
//!
//! So the operands are lowered in the order the form evaluates them, up to
//! and including the first that diverges, and the form answers a location
//! nothing writes, which is what a diverging expression answers anywhere.
//! The operands before it are still lowered, because their effects happen:
//! `h(g(1), return x)` calls `g` before it returns, in the oracle and here.
//! The ones after it are not, because they never run.
//!
//! Only operands the form evaluates *unconditionally and first* are named
//! here. The right side of `&&` and `||`, the branches of an `if` and the
//! arms of a `match` run only on some paths, and a diverging one of those
//! is the ordinary early exit every form already handles — `store` and
//! `return_expr` decline to copy out of it.

use cove_sema::typeck::Ty;
use cove_syntax::ast::{BinaryOp, Expr, ExprKind, StrPart};

use super::frame::Val;
use super::shapes;
use super::Body;

impl Body<'_> {
    /// The location `expr` answers when one of the operands it evaluates
    /// first diverges, after lowering those operands; `None`, having lowered
    /// nothing, when none does.
    pub(super) fn unreached(&mut self, expr: &Expr) -> Option<Val> {
        let operands = self.evaluated_first(expr);
        let leaves = operands
            .iter()
            .position(|operand| self.leaves_early(operand))?;
        for operand in &operands[..=leaves] {
            let value = self.expr(operand);
            self.release(value, operand.span);
        }
        // Nothing reads the answer on a path that runs, but the surrounding
        // form may still be lowered around it, so it is a location of the
        // form's own type where that type has a layout. Where it has none —
        // `(return x).length()` is a call on a receiver of no type, and the
        // checker settles nothing for it — the location is `Never`'s, and
        // saying the type was never settled would be a gap in code that
        // never runs.
        let layout = self
            .ty(expr)
            .and_then(|ty| self.pool.shapes.of(self.checked, self.module, &ty))
            .unwrap_or(shapes::UNIT);
        Some(self.temp(layout))
    }

    /// Whether lowering `expr` leaves before it answers: it diverges, or
    /// one of the operands it evaluates first does, however deep.
    ///
    /// The depth matters because some forms lower an operand's operands
    /// themselves rather than through [`Body::expr`]: an `if` fuses a
    /// comparison condition into one branch instruction, so `if 1 < return
    /// x` never asks this of `1 < return x` unless the `if` does.
    pub(super) fn leaves_early(&self, expr: &Expr) -> bool {
        self.diverges(expr)
            || self
                .evaluated_first(expr)
                .into_iter()
                .any(|operand| self.leaves_early(operand))
    }

    /// The operands `expr` evaluates before anything else it does, in the
    /// order it evaluates them.
    fn evaluated_first<'e>(&self, expr: &'e Expr) -> Vec<&'e Expr> {
        match &expr.kind {
            ExprKind::Unary { operand, .. } => vec![operand],
            // The right side of a short circuit runs only when the left does
            // not decide the answer.
            ExprKind::Binary {
                op: BinaryOp::And | BinaryOp::Or,
                lhs,
                ..
            } => vec![lhs],
            ExprKind::Binary { lhs, rhs, .. } => vec![lhs, rhs],
            ExprKind::Field { base, .. } => vec![base],
            // The target of an assignment is a name or a field path, which
            // is read and written but evaluates nothing.
            ExprKind::Assign { value, .. } => vec![value],
            ExprKind::Call {
                callee,
                args,
                trailing,
                ..
            } => {
                let mut operands = Vec::with_capacity(args.len() + 2);
                if let Some(receiver) = self.receiver(callee) {
                    operands.push(receiver);
                }
                operands.extend(args.iter().map(|arg| &arg.value));
                operands.extend(trailing.as_deref());
                operands
            }
            ExprKind::ArrayLit(items) => items.iter().collect(),
            ExprKind::Str(parts) | ExprKind::Tagged { parts, .. } => parts
                .iter()
                .filter_map(|part| match part {
                    StrPart::Interpolation(inner) => Some(inner),
                    StrPart::Text(_) => None,
                })
                .collect(),
            ExprKind::Try(inner) | ExprKind::Await(inner) => vec![inner],
            ExprKind::If { condition, .. } | ExprKind::While { condition, .. } => {
                vec![condition]
            }
            ExprKind::Match { scrutinee, .. } => vec![scrutinee],
            ExprKind::For { iterable, .. } => vec![iterable],
            ExprKind::Range { start, end, .. } => vec![start, end],
            _ => Vec::new(),
        }
    }

    /// The value a call evaluates before its arguments: the closure a
    /// function value is, or the receiver of a method. A name that resolves
    /// to a declaration, a type or a module is not a value and is evaluated
    /// by nothing.
    fn receiver<'e>(&self, callee: &'e Expr) -> Option<&'e Expr> {
        if matches!(self.ty(callee), Some(Ty::Fn(_))) {
            return Some(callee);
        }
        match &callee.kind {
            ExprKind::Field { base, .. } if !self.is_namespace(base) && self.ty(base).is_some() => {
                Some(base)
            }
            _ => None,
        }
    }
}
