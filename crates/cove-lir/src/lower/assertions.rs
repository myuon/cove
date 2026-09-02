//! `assert` and `assertEqual`, which are control flow rather than a call.
//!
//! `cove_schema::builtins` declares the two as free builtins of kind
//! [`FreeBuiltinKind::Assertion`]: a name a test calls, taking no receiver
//! and belonging to no module. The tree-walking oracle answers them with
//! `cove_runtime::builtins::call_assertion`, which is the definition of what
//! they mean, and this has to say the same thing.
//!
//! # They are lowered, not performed
//!
//! An assertion could have been an [`Inst::CallBuiltin`] — a receiver and an
//! operation the machine recognises — and it is not, because there is
//! nothing in one the language does not already have. `assertEqual` asks
//! whether two values are equal, which is `==`; it renders both when they
//! are not, which is `"{x}"`; and it answers `Ok(())` or
//! `Err(Error("..."))`, which are a case and an initializer. Every one of
//! those is already a lowering here, so the assertion is a `BranchFalse`
//! over two arms that build the two cases, and the machine learns nothing.
//!
//! What that buys is that the assertion's meaning stays in one place. A
//! builtin would have to answer equality over words, render a value out of
//! the heap, and construct a `Result` — three rules the language already
//! states, restated in the machine where the corpus is the only thing that
//! could catch them drifting. `docs/LINEAR_VM.md` says a builtin is "a
//! library over words with no reentry"; an assertion is not one of those, it
//! is a program.
//!
//! It also makes `assertEqual(total, 285715)?` an ordinary `?`. The value
//! the arms build *is* the `Result<Unit, Error>` the schema declares, in the
//! layout the checker settled for this call site, so nothing about `?` has
//! to know an assertion happened.
//!
//! # The source text is the whole reason these are builtins
//!
//! `call_assertion` takes the source text of the argument expressions, so a
//! failure says which condition failed in the words the test was written in.
//! Both assertions quote argument *zero* and neither quotes another, which
//! is what makes this cheap: one string, known where the call is lowered.
//!
//! The lowering reads it out of the [`SourceMap`](cove_diag::SourceMap) with
//! the argument's own span — the same span the instructions evaluating that
//! argument already carry — and interns it into the message. So the text is
//! a constant of the program rather than something the machine looks up, and
//! the machine needs no channel for it at all: `Inst::Str` was already
//! enough.
//!
//! The alternative was to hand the span to the machine and have it read the
//! text at run time, as the predecessor's `arg_spans` does. That is a second
//! place a span has to be right and a second reader of the source map, for a
//! string that never changes between two runs of the same program.

use cove_diag::Span;
use cove_schema::builtins::FreeBuiltinSchema;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Expr};

use crate::inst::{CmpOp, Inst, Slot};
use crate::layout::LayoutId;
use crate::program::Builtin;
use crate::repr::Repr;

use super::expr::compare_of;
use super::frame::Val;
use super::{gap, shapes, Body, PENDING};

impl Body<'_> {
    /// `assert(condition)` and `assertEqual(actual, expected)`.
    ///
    /// Which name this is and how many arguments it takes come from the
    /// shared table, so the arity enforced here is the arity `cove check`
    /// reported on.
    pub(super) fn assertion(
        &mut self,
        expr: &Expr,
        schema: &FreeBuiltinSchema,
        args: &[Arg],
    ) -> Val {
        for arg in args {
            let what = if arg.label.is_some() {
                "a labelled argument to an assertion"
            } else if arg.is_var {
                "a `var` argument to an assertion"
            } else if arg.spread {
                "a spread argument to an assertion"
            } else {
                continue;
            };
            return self.gap(what, expr);
        }
        if args.len() != schema.arity() {
            return self.gap(
                "an assertion passed a different number of arguments than it declares",
                expr,
            );
        }
        let Some(ty) = self.owned_ty(expr) else {
            return self.dead(expr);
        };
        let Some(layout) = self.layout(&ty, expr.span) else {
            return self.dead(expr);
        };
        // `Result<Unit, Error>` is what the table declares both of them
        // answer, and the two cases are read off the type the checker
        // settled here rather than assumed to be at any particular index.
        let (Some((ok, _)), Some((failed, _))) = (
            shapes::case_at(self.checked, self.module, &ty, "Ok"),
            shapes::case_at(self.checked, self.module, &ty, "Err"),
        ) else {
            return self.gap("an assertion whose answer is not a `Result`", expr);
        };

        // The answer's location is taken before anything else, so that both
        // arms write the same words and nothing an arm allocates can be
        // handed it.
        let dst = self.temp(layout);
        let mut held = Vec::with_capacity(args.len());
        for arg in args {
            held.push(self.expr(&arg.value));
        }
        let (cond, compared) = self.holds(expr, args, &held);

        let branch = self.emit(Inst::BranchFalse { cond, to: PENDING }, expr.span);
        self.write_ok(dst.slot, layout, ok, expr.span);
        let over = self.emit(Inst::Jump { to: PENDING }, expr.span);

        let at = self.here();
        self.patch(branch, at);
        self.write_failure(expr, schema, args, &held, dst.slot, layout, failed);
        let at = self.here();
        self.patch(over, at);

        // The operands outlive the branch, because the failing arm renders
        // them, so their live ranges end here — after the join, where both
        // paths meet, rather than in the arm that happened to read them.
        if let Some(compared) = compared {
            self.release(compared, expr.span);
        }
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        dst
    }

    /// The slot the branch reads, and the temporary it was computed into
    /// when there was one.
    ///
    /// `assert` is handed its condition already: the argument is a `Bool`
    /// and the branch reads it where it sits. `assertEqual` compares, and
    /// compares the way `==` does — the shared table says as much by naming
    /// one type parameter twice, so what it refuses is what `==` refuses.
    fn holds(&mut self, expr: &Expr, args: &[Arg], held: &[Val]) -> (Slot, Option<Val>) {
        let [a, b] = held else {
            return (held[0].slot, None);
        };
        if a.layout != b.layout {
            self.errors.push(gap::gap(
                "an `assertEqual` whose two values are laid out differently",
                expr.span,
            ));
            let dst = self.temp(shapes::BOOL);
            self.emit(
                Inst::Bool {
                    dst: dst.slot,
                    value: true,
                },
                expr.span,
            );
            return (dst.slot, Some(dst));
        }
        let dst = self.temp(shapes::BOOL);
        let repr = self.frame.repr(a.slot);
        if !self.is_scalar(a.layout) {
            // A value the instruction set cannot compare in one step is
            // compared by walking it, which is what `==` on one does.
            self.compare_values(expr, true, &args[0].value, dst.slot, a, b);
        } else if repr == Repr::Unit {
            // `()` compares equal to `()` and there is nothing to look at.
            self.emit(
                Inst::Bool {
                    dst: dst.slot,
                    value: true,
                },
                expr.span,
            );
        } else {
            self.emit(
                Inst::Cmp {
                    on: compare_of(repr),
                    op: CmpOp::Eq,
                    dst: dst.slot,
                    a: a.slot,
                    b: b.slot,
                },
                expr.span,
            );
        }
        (dst.slot, Some(dst))
    }

    /// `Ok(())`, which is what a holding assertion answers.
    fn write_ok(&mut self, dst: Slot, layout: LayoutId, case: u32, span: Span) {
        let unit = self.temp(shapes::UNIT);
        self.emit(Inst::Unit { dst: unit.slot }, span);
        self.write_case(dst, layout, case, &[unit], span);
        self.release(unit, span);
    }

    /// `Err(Error("assertion failed: ..."))`.
    ///
    /// A failing assertion is an *expected* failure, so it is an `Err` and
    /// not an [`Inst::Trap`]: a test that fails one reports it and the run
    /// goes on, which is what `?` after the call does with it.
    #[allow(clippy::too_many_arguments)]
    fn write_failure(
        &mut self,
        expr: &Expr,
        schema: &FreeBuiltinSchema,
        args: &[Arg],
        held: &[Val],
        dst: Slot,
        layout: LayoutId,
        case: u32,
    ) {
        let Some(message) = self.assertion_message(expr, schema, args, held) else {
            return;
        };
        // Where it failed, said once, to the only party that knows. The
        // `Err` this arm goes on to build carries the message and no
        // position — which is the right shape for the language, because a
        // test propagates it with `?` like any other expected failure — so
        // the span is recorded here instead, on the instruction that has it.
        // See [`Inst::AssertFailed`].
        self.emit(
            Inst::AssertFailed {
                message: message.slot,
            },
            expr.span,
        );
        let Some(error) = self.error_value(expr, message) else {
            return;
        };
        self.write_case(dst, layout, case, &[error], expr.span);
        self.release(error, expr.span);
    }

    /// What the failure says, worded as `call_assertion` words it.
    ///
    /// `assert` names the condition and nothing else, so its message is one
    /// interned string and the failing arm costs one instruction.
    /// `assertEqual` reports both values as well, since knowing only that
    /// they differ rarely explains why — and a value becomes text the one
    /// way the language has, which is the builtin `"{x}"` already goes
    /// through.
    fn assertion_message(
        &mut self,
        expr: &Expr,
        schema: &FreeBuiltinSchema,
        args: &[Arg],
        held: &[Val],
    ) -> Option<Val> {
        let quoted = self.source_text(args[0].value.span).to_string();
        if schema.arity() == 1 {
            return Some(self.text(&format!("assertion failed: `{quoted}`"), expr.span));
        }
        let opening = self.text(&format!("assertion failed: `{quoted}` is `"), expr.span);
        let between = self.text("`, expected `", expr.span);
        let closing = self.text("`", expr.span);
        let pieces = vec![
            opening.arg(),
            held[0].arg(),
            between.arg(),
            held[1].arg(),
            closing.arg(),
        ];
        let list = self.pool.args.intern(pieces);
        let builtin = self.pool.builtin(Builtin {
            receiver: "String".into(),
            operation: "interpolate".into(),
            result: shapes::STR,
        });
        let dst = self.temp(shapes::STR);
        self.emit(
            Inst::CallBuiltin {
                dst: dst.slot,
                builtin,
                args: list,
            },
            expr.span,
        );
        self.release(closing, expr.span);
        self.release(between, expr.span);
        self.release(opening, expr.span);
        Some(dst)
    }

    /// A location holding a string of the program's pool.
    fn text(&mut self, literal: &str, span: Span) -> Val {
        let id = self.string(literal);
        let dst = self.temp(shapes::STR);
        self.emit(
            Inst::Str {
                dst: dst.slot,
                text: id,
            },
            span,
        );
        dst
    }

    /// `Error(message)`: the one-field builtin struct, built where its
    /// fields are.
    fn error_value(&mut self, expr: &Expr, message: Val) -> Option<Val> {
        let layout = self.layout(&Ty::Error, expr.span)?;
        let fields = self.fields_of(layout)?;
        let field = fields.first()?.clone();
        let dst = self.temp(layout);
        self.copy(dst.slot + field.at, message.slot, field.layout, expr.span);
        self.release(message, expr.span);
        Some(dst)
    }
}
