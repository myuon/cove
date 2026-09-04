//! Expressions.
//!
//! Every expression answers a value location, and the caller says whether it
//! wants one: [`Body::expr`] produces a value, [`Body::discard`] runs the
//! expression for its effects. The second is not the first with the answer
//! thrown away — an `if` with no `else` lowered for its effects writes no
//! `()` anywhere, and an assignment statement emits nothing beyond the
//! store.
//!
//! # A field is slot arithmetic, not an instruction
//!
//! A struct is its fields, in place, so `l.from.x` is `base + Field::at`
//! twice over and reading it emits *nothing at all*. [`Body::place_of`] is
//! where that arithmetic is done, and it answers a [`Place`] rather than a
//! slot because the same walk has to serve a read, a write and a `var`
//! argument.
//!
//! One place is not a run of this frame: what a `var` parameter names, which
//! is an address. A field of one is that address plus the field's offset,
//! which is the same arithmetic done to an address instead of to a slot
//! number.
//!
//! # Short-circuiting is control flow
//!
//! `&&` and `||` are not instructions and there is no `And` or `Or` to add.
//! Their meaning is that the right-hand side *may not run*, and an
//! instruction taking two operands has already run it. So both lower to a
//! branch over the right-hand side, which is also why one conditional branch
//! is enough for the whole language: `if`, `while` and both short-circuits
//! are the same jump with the condition arranged to suit.
//!
//! # A reference's live range ends where the expression does
//!
//! Every consumer of a value calls [`Body::release`] rather than freeing the
//! run behind it, and that is where [`Inst::Clear`] comes from. A static
//! reference map says which slots a collection *reads*; only the data can
//! say when the value in one stopped being needed, so a temporary that held
//! an object holds null from its last use onwards. Without it a body that
//! built one string per turn would retain every one of them until it
//! returned.

use cove_diag::Span;
use cove_schema::builtins::FreeBuiltinKind;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, BinaryOp, Block, Expr, ExprKind, StrPart, UnaryOp};

use super::collections;
use super::frame::Val;
use super::gap;
use super::methods;
use super::shapes;
use super::{Body, Dest, Loop, PENDING};
use crate::inst::{ArithOp, CmpOp, Compare, Inst, Num, Slot};
use crate::layout::LayoutId;
use crate::program::{Builtin, HostOp};
use crate::repr::Repr;

/// An assignable location, found by walking a chain of field accesses back
/// to whatever roots it.
#[derive(Clone, Copy, Debug)]
pub(super) enum Place {
    /// A run of the current frame: a local, a parameter, or a field of one.
    ///
    /// The whole of a struct's field arithmetic ends here. Nothing is
    /// emitted to reach one.
    Here { slot: Slot, layout: LayoutId },
    /// Words at a linear address, and a field's offset into them.
    ///
    /// This is what a `var` parameter names. The offset is carried rather
    /// than folded into the address as the place is built, because forming
    /// an address is an instruction and finding a place must not emit one:
    /// `p.y` is a place whether it is read, written or passed on, and only
    /// the three of those know which. Where an offset is needed as an
    /// address, [`Inst::AddrOfPart`] adds it.
    Through {
        addr: Slot,
        /// The field's word offset within the value the address names.
        at: u32,
        layout: LayoutId,
    },
}

impl Place {
    fn layout(&self) -> LayoutId {
        match self {
            Place::Here { layout, .. } | Place::Through { layout, .. } => *layout,
        }
    }
}

impl Body<'_> {
    /// Lowers `expr` and answers where its value ended up.
    ///
    /// Two steps, and the second is what makes erasure end somewhere. The
    /// first is [`Body::lowered`], which builds the value the construct
    /// produces; the second is [`Body::unerased`], which reconciles that value
    /// with the type the *checker* settled for this expression. They differ
    /// in exactly one situation and it is the one erasure creates: a
    /// location holds a box because that is what the thing that filled it
    /// had to say, and the program has since said what is in it.
    pub(super) fn expr(&mut self, expr: &Expr) -> Val {
        let value = self.lowered(expr);
        self.unerased(value, expr)
    }

    /// Opens an erased value at the type the checker settled for the place
    /// it stands in.
    ///
    /// `docs/LINEAR_VM.md` gives an intentionally erased value one
    /// representation and [`Body::unbox`] one rule for reading it: the
    /// layout is *"a type the source wrote at the place the value is being
    /// used"*, never one invented here. This is that rule at its most
    /// general position. What the source wrote may be several statements
    /// away — an annotation on a binding, a declared parameter the value was
    /// passed through — and the checker is what carried it here, so the type
    /// is read from [`Facts::ty`](cove_sema::facts::Facts::ty) rather than
    /// from an annotation this pass would have to re-resolve.
    ///
    /// Three things it deliberately does not do.
    ///
    /// It never *boxes*. Erasing is the language's implicit conversion and
    /// happens where a `dyn` or an `Any` is written, which is
    /// [`Body::erase`] and [`Body::fit`]; this is only the way back.
    ///
    /// It asks nothing of a value that is not a box, so the disagreement it
    /// closes is the one erasure opened and not a general re-typing of every
    /// expression. A `Result<Any, Error>` in a location of that layout stays
    /// where it is: the box is one word *inside* it, and opening it is the
    /// business of the use that reaches that word — the `?`, the arm of the
    /// `match` — each of which is an expression of its own and arrives here
    /// in its turn. That is what makes a nesting of any depth need no
    /// rebuilding: `Result<Result<T, E>, E>` is opened one level at each
    /// level's use.
    ///
    /// And it stops where the checker stopped. A type that is still `Any`
    /// or a `dyn` is a use that named nothing, and there is no static answer
    /// to give it — reading it would mean dispatching on what the box turned
    /// out to hold, which is the run-time type universe this backend does
    /// not have. Such a use stays whatever gap it already was.
    fn unerased(&mut self, value: Val, expr: &Expr) -> Val {
        if !self.is_boxed(value.layout) {
            return value;
        }
        let Some(ty) = self.ty(expr) else {
            return value;
        };
        // Four types that are not a description of what is in a box.
        // `Never` says the value was never produced, `Unknown` says the
        // checker declined, and a `Param` is a declaration's own word for a
        // type this instantiation was not given. An `Any` or a `dyn` *is* a
        // description, and it is the description this value already has.
        if matches!(
            ty,
            Ty::Any | Ty::Dyn(_) | Ty::Never | Ty::Unknown(_) | Ty::Param(_)
        ) {
            return value;
        }
        // Asked of the table directly rather than through `Body::layout`,
        // because a settled type with no layout is a gap the use itself
        // reports in its own words. Answering a second one here would name
        // the same type twice for one expression.
        let Some(want) = self.pool.shapes.of(self.checked, self.module, &ty) else {
            return value;
        };
        if want == value.layout {
            return value;
        }
        self.unbox(value, want, expr.span)
    }

    /// The value a construct produces, before the checker's type is
    /// reconciled with it. See [`Body::expr`].
    fn lowered(&mut self, expr: &Expr) -> Val {
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
            ExprKind::Str(parts) => self.string_expr(expr, parts),
            ExprKind::Ident(name) => self.name(expr, name),
            ExprKind::Unary { op, operand } => self.unary(expr, *op, operand),
            ExprKind::Binary { op, lhs, rhs } => self.binary(expr, *op, lhs, rhs),
            ExprKind::Assign { op, target, value } => {
                self.assign(*op, target, value, span);
                self.unit_value(span)
            }
            ExprKind::Block(block) => {
                let dst = self.answer_of(expr);
                self.scoped_block(block, Some(Dest::of(&dst)));
                dst
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let dst = self.answer_of(expr);
                self.if_expr(
                    condition,
                    then_branch,
                    else_branch.as_deref(),
                    span,
                    Some(Dest::of(&dst)),
                );
                dst
            }
            ExprKind::While { condition, body } => {
                self.while_expr(condition, body, span);
                self.unit_value(span)
            }
            ExprKind::Match { scrutinee, arms } => {
                let dst = self.answer_of(expr);
                self.match_expr(scrutinee, arms, span, Some(Dest::of(&dst)));
                dst
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
                args,
                trailing,
                ..
            } => self.call(expr, callee, args, trailing.as_deref()),
            ExprKind::Field { base, name } => self.field(expr, base, &name.node),
            ExprKind::Try(inner) => self.try_expr(expr, inner),

            ExprKind::ArrayLit(items) => self.array_literal(expr, items),
            ExprKind::Range {
                start,
                end,
                inclusive_end,
            } => self.range_literal(expr, start, end, *inclusive_end),
            ExprKind::For {
                binding,
                iterable,
                body,
            } => {
                self.for_expr(binding, iterable, body, span);
                self.unit_value(span)
            }

            ExprKind::Await(inner) => self.settle(expr, inner),
            // The `async` is on the function type the checker settled here,
            // and a call through the value is what reads it. See
            // `Body::lambda`.
            ExprKind::Lambda { params, body, .. } => self.lambda(expr, params, body),
            ExprKind::Scope { name, body } => self.scope_expr(expr, name, body),
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
            ExprKind::For {
                binding,
                iterable,
                body,
            } => self.for_expr(binding, iterable, body, expr.span),
            ExprKind::Match { scrutinee, arms } => {
                self.match_expr(scrutinee, arms, expr.span, None)
            }
            _ => {
                let value = self.expr(expr);
                self.release(value, expr.span);
            }
        }
    }

    // ---- literals -------------------------------------------------------

    /// A location of the layout `expr` answers, for a form that assembles
    /// its value rather than computing it in one instruction.
    fn answer_of(&mut self, expr: &Expr) -> Val {
        let layout = self.layout_of(expr);
        self.temp(layout)
    }

    /// A value that is entirely in the instruction: a location of the right
    /// layout and one instruction writing it.
    fn constant(&mut self, expr: &Expr, inst: impl FnOnce(Slot) -> Inst) -> Val {
        let dst = self.answer_of(expr);
        self.emit(inst(dst.slot), expr.span);
        dst
    }

    /// The `()` a form answers when its value was never computed from
    /// anything: an assignment, a loop.
    pub(super) fn unit_value(&mut self, span: Span) -> Val {
        let dst = self.temp(shapes::UNIT);
        self.emit(Inst::Unit { dst: dst.slot }, span);
        dst
    }

    /// A name: a local, a parameter, or the one case of an enum that is
    /// written as a bare word.
    fn name(&mut self, expr: &Expr, name: &str) -> Val {
        if let Some((slot, layout)) = self.frame.lookup(name) {
            // A `var` parameter names the caller's storage rather than
            // holding a value, so reading it is a read *through* the word.
            // Nothing else in the frame is an `Addr`, which is what makes
            // the location's own layout enough to tell them apart.
            if layout == shapes::ADDR {
                let want = self.layout_of(expr);
                let dst = self.temp(want);
                self.emit(
                    Inst::Load {
                        dst: dst.slot,
                        addr: slot,
                        layout: want,
                    },
                    expr.span,
                );
                return dst;
            }
            return Val::borrowed(slot, layout);
        }
        // `None` is a case, not a name to bind: it is the one case in the
        // language with no payload and no qualifier, so it is written where
        // a name would be.
        if let Some(ty) = self.ty(expr) {
            if shapes::case_at(self.checked, self.module, &ty, name).is_some() {
                return self.enum_case(expr, &ty, name, &[]);
            }
            // A declaration written where a value goes. It becomes an
            // environment with no captures, which is the other half of
            // `Body::lambda` — see `Body::function_value`.
            if matches!(ty, Ty::Fn(_)) {
                if let Some(id) = self.plan.resolve(self.checked, self.module, name) {
                    return self.function_value(expr, id);
                }
                return self.gap(
                    &format!("`{name}`, a host operation used as a function value"),
                    expr,
                );
            }
        }
        self.gap("a name that is not a local binding", expr)
    }

    // ---- strings ---------------------------------------------------------

    /// A string literal, and the interpolations in it.
    ///
    /// A literal with nothing interpolated is one [`Inst::Str`], and the
    /// object behind it is allocated once for the run: a literal in a loop
    /// costs no allocation per turn.
    ///
    /// An interpolation is where a value has to become text, and that is not
    /// something the instruction set should grow a case for — what `{x}`
    /// puts in a string is a rule of the language, stated in the language
    /// reference and not in the IR. So the whole literal becomes one
    /// [`Inst::CallBuiltin`].
    ///
    /// # What the builtin must do
    ///
    /// `String.interpolate` takes any number of operands and answers one new
    /// `String`: each operand rendered as `Display for Value` renders it,
    /// joined in order. Every operand is one word, because an operand that
    /// is not is boxed on the way in — a builtin receives slots and there is
    /// no channel on [`Inst::CallBuiltin`] for the layout of each, so a
    /// value whose width is not one has to carry its own description.
    ///
    /// The runs of literal text are operands too, as `Str` objects, so the
    /// pieces are one list and the join is one call. An empty run is left
    /// out: the parser leaves one wherever an interpolation sits at an end
    /// of the literal, and joining it would be an allocation and an argument
    /// per `"{x}"` in the program.
    fn string_expr(&mut self, expr: &Expr, parts: &[StrPart]) -> Val {
        let literal_only = parts.iter().all(|part| matches!(part, StrPart::Text(_)));
        if literal_only {
            let mut text = String::new();
            for part in parts {
                if let StrPart::Text(literal) = part {
                    text.push_str(literal);
                }
            }
            let id = self.string(&text);
            let dst = self.temp(shapes::STR);
            self.emit(
                Inst::Str {
                    dst: dst.slot,
                    text: id,
                },
                expr.span,
            );
            return dst;
        }

        let mut pieces: Vec<Val> = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                StrPart::Text(literal) if literal.is_empty() => {}
                StrPart::Text(literal) => {
                    let id = self.string(literal);
                    let dst = self.temp(shapes::STR);
                    self.emit(
                        Inst::Str {
                            dst: dst.slot,
                            text: id,
                        },
                        expr.span,
                    );
                    pieces.push(dst);
                }
                StrPart::Interpolation(inner) => {
                    let value = self.expr(inner);
                    pieces.push(value);
                }
            }
        }

        let args = self.pool.args.intern(pieces.iter().map(Val::arg).collect());
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
                args,
            },
            expr.span,
        );
        for piece in pieces.into_iter().rev() {
            self.release(piece, expr.span);
        }
        dst
    }

    // ---- operators -------------------------------------------------------

    fn unary(&mut self, expr: &Expr, op: UnaryOp, operand: &Expr) -> Val {
        let a = self.expr(operand);
        let dst = self.answer_of(expr);
        // `!` and `-` answer the type they were given, so the answer's own
        // layout is what an erased operand is opened to. Nothing is invented:
        // the destination came from the type the checker settled for the
        // whole expression, which is a type something written said.
        let a = if self.is_boxed(a.layout) && !self.is_boxed(dst.layout) {
            self.unbox(a, dst.layout, expr.span)
        } else {
            a
        };
        let repr = self.frame.repr(dst.slot);
        let inst = match op {
            UnaryOp::Not => Inst::Not {
                dst: dst.slot,
                a: a.slot,
            },
            UnaryOp::Neg => Inst::Neg {
                num: num_of(repr),
                dst: dst.slot,
                a: a.slot,
            },
        };
        self.emit(inst, expr.span);
        self.release(a, expr.span);
        dst
    }

    fn binary(&mut self, expr: &Expr, op: BinaryOp, lhs: &Expr, rhs: &Expr) -> Val {
        match op {
            BinaryOp::And => self.short_circuit(expr, lhs, rhs, true),
            BinaryOp::Or => self.short_circuit(expr, lhs, rhs, false),
            BinaryOp::Is => self.identity(expr, lhs, rhs),
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
        // An `Int` the source wrote on the right stays in the instruction.
        //
        // What it replaces is two instructions and a slot: `i < 2000000`
        // materialised the bound into a temporary and compared against it,
        // and because a `while`'s back edge lands on the *condition*, the
        // materialisation ran once per turn. Nothing read the temporary but
        // the compare that followed it.
        //
        // The left operand's `Repr` is what admits this, and it is the same
        // question that decides `Num` and `Compare` below: an `Int` or a
        // `Duration` word is what these two instructions read. Everything
        // else — a `Float`, a `Bool`, a `String`, an erased box that
        // `Body::opened` would have to open, a host handle — takes the path
        // below unchanged, which is why there is no immediate form to get
        // wrong for any of them.
        //
        // The right and only the right. `a - 1` and `1 - a` are different
        // questions, and `a % 7` and `7 % a` more so; a left-hand immediate
        // would be a second instruction family rather than a mirror of this
        // one, for a shape no program here writes.
        if matches!(self.frame.repr(a.slot), Repr::Int | Repr::Duration) {
            if let Some(value) = int_literal(rhs) {
                let dst = self.answer_of(expr);
                let inst = match arith_of(op) {
                    Some(op) => Inst::ArithImm {
                        op,
                        dst: dst.slot,
                        a: a.slot,
                        value,
                    },
                    // `operator` is reached for the eleven arithmetic and
                    // comparison operators only — `&&`, `||` and `is` are
                    // taken apart by `Body::binary` before this — so an
                    // operator that is not arithmetic here is a comparison.
                    None => Inst::CmpImm {
                        op: cmp_of(op),
                        dst: dst.slot,
                        a: a.slot,
                        value,
                    },
                };
                self.emit(inst, expr.span);
                self.release(a, expr.span);
                return dst;
            }
        }
        let b = self.expr(rhs);
        let (a, b) = self.opened(a, b, expr.span);
        let dst = self.answer_of(expr);
        // A value the instruction set cannot compare in one step is compared
        // by walking it, which is a call rather than an instruction.
        //
        // Only `==` and `!=` come here. A walk answers whether two values are
        // the same and cannot answer which is smaller, so an ordering
        // operator that reached it would be answered as an equality — and
        // this is written as a guard rather than assumed because it *was*
        // assumed: `"a" < "b"` lowered to `ne.str` and `sorted` over strings
        // quietly returned its input reversed. A wrong answer is worse than a
        // gap, so an ordering the instruction set cannot give is named.
        let ordering = matches!(
            op,
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
        );
        if arith_of(op).is_none() && !self.is_scalar(a.layout) && !ordering {
            let equal = op == BinaryOp::Eq;
            self.compare_values(expr, equal, lhs, dst.slot, &a, &b);
            self.release(b, expr.span);
            self.release(a, expr.span);
            return dst;
        }
        // A `String` is the one heap value the language orders, and it is
        // ordered by its bytes.
        if ordering && !self.is_scalar(a.layout) && !self.is_text(a.layout) {
            self.release(b, expr.span);
            self.release(a, expr.span);
            return self.gap("an ordering comparison of two heap values", expr);
        }
        let operand = self.frame.repr(a.slot);
        // A host resource handle is the one word that is neither a scalar the
        // machine computes with nor an address it can trace: it is an index
        // into the run's resource table, and whether two indices being equal
        // is two handles naming one resource is that table's question rather
        // than an instruction's. So no [`Inst::Cmp`] admits one — the
        // verifier says as much — and this names the work instead of emitting
        // one that would be a fault.
        if operand == Repr::Host {
            self.release(b, expr.span);
            self.release(a, expr.span);
            return self.gap("a comparison of two host resource handles", expr);
        }
        let inst = match arith_of(op) {
            Some(op) => Inst::Arith {
                num: num_of(operand),
                op,
                dst: dst.slot,
                a: a.slot,
                b: b.slot,
            },
            // `()` compares equal to `()` and there is nothing to look at,
            // so the answer is the instruction. Both operands were still
            // evaluated, because either of them may have done something.
            None if operand == Repr::Unit => Inst::Bool {
                dst: dst.slot,
                value: op == BinaryOp::Eq,
            },
            None => Inst::Cmp {
                on: compare_of(operand),
                op: cmp_of(op),
                dst: dst.slot,
                a: a.slot,
                b: b.slot,
            },
        };
        self.emit(inst, expr.span);
        self.release(b, expr.span);
        self.release(a, expr.span);
        dst
    }

    /// Opens an erased operand of a binary operator, at the type the other
    /// operand is.
    ///
    /// An operator compares or combines *words*, and an erased value is one
    /// address to an object that carries its own layout — so a box cannot be
    /// an operand and something has to say what is inside it. The operand
    /// beside it is what says. `v % 7` where `v` came back from a schema that
    /// declared `Any` is a remainder of two `Int`s or it is nothing, and the
    /// `7` is where that is written down.
    ///
    /// Where *both* are erased, nothing says, and this leaves them alone: the
    /// paths below answer for two references or name the gap, which is the
    /// right answer to "nothing here states a type" — and inventing one would
    /// be picking a type for a program that did not.
    ///
    /// The trap is [`Inst::Unbox`]'s. A box holding something else fails the
    /// run at this instruction, where the oracle fails at the operator with
    /// `` `%` is not defined for `String` and `Int` ``. Both stop, and the
    /// two do not say the same words: the machine knows which layout it
    /// found and not which operator asked, and `Inst::Trap` carries a static
    /// message. It is written down here rather than left to be discovered.
    fn opened(&mut self, a: Val, b: Val, span: Span) -> (Val, Val) {
        if a.layout == b.layout {
            return (a, b);
        }
        match (self.is_boxed(a.layout), self.is_boxed(b.layout)) {
            (true, false) => {
                let a = self.unbox(a, b.layout, span);
                (a, b)
            }
            (false, true) => {
                let b = self.unbox(b, a.layout, span);
                (a, b)
            }
            _ => (a, b),
        }
    }

    /// `&&` and `||`, which are a branch over the right-hand side.
    ///
    /// Both answer the left-hand side's word when it already settles the
    /// question, so the answer is written before the branch and the
    /// right-hand side overwrites it only when it runs. `conjunction` says
    /// which way round: `&&` skips the right-hand side when the left is
    /// false, `||` when it is true.
    fn short_circuit(&mut self, expr: &Expr, lhs: &Expr, rhs: &Expr, conjunction: bool) -> Val {
        let dst = self.temp(shapes::BOOL);
        let a = self.expr(lhs);
        self.store(Dest::of(&dst), &a, lhs);
        self.release(a, lhs.span);

        let branch = self.emit(
            Inst::BranchFalse {
                cond: dst.slot,
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
        self.store(Dest::of(&dst), &b, rhs);
        self.release(b, rhs.span);

        let end = self.here();
        match skip {
            Some(skip) => self.patch(skip, end),
            None => self.patch(branch, end),
        }
        dst
    }

    // ---- places -------------------------------------------------------

    /// The location an assignable expression names, as slot arithmetic.
    ///
    /// A field of an inline value is `base + Field::at`, and reaching one
    /// emits nothing at all. That is what `docs/LINEAR_VM.md` means by *"a
    /// field access on an inline struct is not an instruction"*.
    pub(super) fn place_of(&mut self, expr: &Expr) -> Option<Place> {
        match &expr.kind {
            ExprKind::Ident(name) => {
                let (slot, layout) = self.frame.lookup(name)?;
                if layout == shapes::ADDR {
                    // A `var` parameter: the words are the caller's, and the
                    // slot holds where they are.
                    return Some(Place::Through {
                        addr: slot,
                        at: 0,
                        layout: self.layout_of(expr),
                    });
                }
                Some(Place::Here { slot, layout })
            }
            ExprKind::Field { base, name } => {
                let outer = self.place_of(base)?;
                let held = outer.layout();
                let field = match self.field_of(held, &name.node) {
                    Some(field) => field,
                    // A field of an erased value is not a place. The words
                    // are inside a box on the heap rather than in this
                    // frame, so there is no slot arithmetic that reaches
                    // them and no address of one that could be written
                    // through — an assignment through a box would share
                    // mutation, which is what `docs/LINEAR_VM.md` refuses.
                    // Reading one is still ordinary: the caller opens the
                    // base as a *value* and takes the field out of the copy,
                    // which is [`Body::field`]. So this answers `None`
                    // without reporting, and whoever asked says what it
                    // wanted the place for.
                    None if self.is_boxed(held) => return None,
                    None => {
                        let named = self.pool.shapes.layout(held).name.clone();
                        self.errors.push(gap::gap(
                            &format!("a field of `{named}`, which is not a struct here"),
                            expr.span,
                        ));
                        return None;
                    }
                };
                Some(match outer {
                    Place::Here { slot, .. } => Place::Here {
                        slot: slot + field.at,
                        layout: field.layout,
                    },
                    Place::Through { addr, at, .. } => Place::Through {
                        addr,
                        at: at + field.at,
                        layout: field.layout,
                    },
                })
            }
            _ => None,
        }
    }

    /// The value a place holds.
    ///
    /// A run of this frame is answered as it stands, borrowed: the words are
    /// already there and reading them is not an event. What is behind an
    /// address has to be brought over, and one load brings exactly the
    /// field's words — the offset is in the address, not in what is read.
    fn read_place(&mut self, place: Place, span: Span) -> Val {
        match place {
            Place::Here { slot, layout } => Val::borrowed(slot, layout),
            Place::Through { addr, at, layout } => {
                let addr = self.part_address(addr, at, span);
                let dst = self.temp(layout);
                self.emit(
                    Inst::Load {
                        dst: dst.slot,
                        addr: addr.slot,
                        layout,
                    },
                    span,
                );
                self.release(addr, span);
                dst
            }
        }
    }

    /// The address of the part of the value at `addr` that begins `at` words
    /// into it.
    ///
    /// Offset zero is the address itself, borrowed: a place is the address of
    /// the *first* word of a value location, so the whole of a value and its
    /// first field are the same address and adding nothing to it would be an
    /// instruction that answers its own operand.
    fn part_address(&mut self, addr: Slot, at: u32, span: Span) -> Val {
        if at == 0 {
            return Val::borrowed(addr, shapes::ADDR);
        }
        let dst = self.temp(shapes::ADDR);
        self.emit(
            Inst::AddrOfPart {
                dst: dst.slot,
                addr,
                at,
            },
            span,
        );
        dst
    }

    /// Writes a value into a place.
    ///
    /// A write through an address updates the caller's own words, which is
    /// the aliasing `var` specifies and needs no copy back. A write to a
    /// *field* through one is the same store at an offset address: nothing is
    /// loaded, nothing is written back, and nothing else in the value is
    /// touched. It was a load of the whole value, a write into the words and
    /// a store of the whole value back, which is the same answer on one
    /// thread and not what an address was for.
    fn write_place(&mut self, place: Place, value: &Val, span: Span) {
        match place {
            Place::Here { slot, layout } => self.copy(slot, value.slot, layout, span),
            Place::Through { addr, at, layout } => {
                let addr = self.part_address(addr, at, span);
                self.emit(
                    Inst::Store {
                        addr: addr.slot,
                        src: value.slot,
                        layout,
                    },
                    span,
                );
                self.release(addr, span);
            }
        }
    }

    // ---- assignment -------------------------------------------------------

    fn assign(&mut self, op: Option<BinaryOp>, target: &Expr, value: &Expr, span: Span) {
        // A field target usually says what stopped it, and saying so twice
        // about one assignment buries the sentence that names the work. So
        // what decides whether this reports is whether anything was already
        // reported, rather than which shape the target had: a field of an
        // erased value is a place `Body::place_of` declines to form and
        // deliberately says nothing about, because reading one is fine and
        // only writing is not.
        let reported = self.errors.len();
        let Some(place) = self.place_of(target) else {
            if self.errors.len() == reported {
                self.errors.push(gap::gap(
                    "an assignment to something that is not a place",
                    span,
                ));
            }
            self.discard(value);
            return;
        };
        // A plain `=` to a run of this frame with an arithmetic operator is
        // the one shape that writes the destination directly, because the
        // destination *is* the accumulator: `n += 2` is one instruction
        // rather than a read, an add and a copy.
        if let (Some(op), Place::Here { slot, layout }) = (op, place) {
            if let (Some(arith), 1) = (arith_of(op), self.width(layout)) {
                let repr = self.frame.repr(slot);
                // `i += 1` is one instruction and no temporary at all, which
                // is what the accumulator being the destination was already
                // for. See `Body::operator` for why the `Repr` is the guard.
                if matches!(repr, Repr::Int | Repr::Duration) {
                    if let Some(literal) = int_literal(value) {
                        self.emit(
                            Inst::ArithImm {
                                op: arith,
                                dst: slot,
                                a: slot,
                                value: literal,
                            },
                            span,
                        );
                        return;
                    }
                }
                let source = self.expr(value);
                self.emit(
                    Inst::Arith {
                        num: num_of(repr),
                        op: arith,
                        dst: slot,
                        a: slot,
                        b: source.slot,
                    },
                    span,
                );
                self.release(source, span);
                return;
            }
        }
        let source = match op {
            None => {
                let held = self.expr(value);
                let into = place.layout();
                self.fit(held, into, span)
            }
            Some(op) => {
                let Some(arith) = arith_of(op) else {
                    self.errors
                        .push(gap::gap("a compound assignment with this operator", span));
                    let held = self.expr(value);
                    self.release(held, span);
                    return;
                };
                let held = self.read_place(place, span);
                let repr = self.frame.repr(held.slot);
                // The same rule one branch further out: a compound
                // assignment *through* an address reads, combines and stores
                // back, and the literal it combines with need not be read
                // from anywhere.
                if let (Repr::Int | Repr::Duration, Some(literal)) = (repr, int_literal(value)) {
                    let dst = self.temp(held.layout);
                    self.emit(
                        Inst::ArithImm {
                            op: arith,
                            dst: dst.slot,
                            a: held.slot,
                            value: literal,
                        },
                        span,
                    );
                    self.release(held, span);
                    dst
                } else {
                    let source = self.expr(value);
                    let dst = self.temp(held.layout);
                    self.emit(
                        Inst::Arith {
                            num: num_of(repr),
                            op: arith,
                            dst: dst.slot,
                            a: held.slot,
                            b: source.slot,
                        },
                        span,
                    );
                    self.release(source, span);
                    self.release(held, span);
                    dst
                }
            }
        };
        self.write_place(place, &source, span);
        self.release(source, span);
    }

    // ---- structs and enums -------------------------------------------------

    /// `base.name`, where `base` is a value.
    ///
    /// Where `base` is not a value it is a namespace — an enum's own name, a
    /// host module — and the only one of those this lowering has been taught
    /// is the enum, whose cases are what `E.Case` names.
    fn field(&mut self, expr: &Expr, base: &Expr, name: &str) -> Val {
        // `http.Method.Get` is reached through two names that are neither of
        // them values, and is asked before the one-name case below for that
        // reason: `Body::is_namespace` answers for a single name, and this is
        // the one shape in the language written with two.
        if let Some(value) = self.host_case(expr, base, name) {
            return value;
        }
        if self.is_namespace(base) {
            if let Some(ty) = self.ty(expr) {
                if shapes::case_at(self.checked, self.module, &ty, name).is_some() {
                    return self.enum_case(expr, &ty, name, &[]);
                }
            }
            return self.gap("a name reached through a module", expr);
        }
        // A chain of fields rooted at a binding is arithmetic, and reaching
        // the words it names emits nothing.
        if let Some(place) = self.place_of(expr) {
            return self.read_place(place, expr.span);
        }
        // Anything else — a field of a call's answer — has to have its base
        // evaluated first, and then the field is an offset into the
        // temporary it left behind.
        let obj = self.expr(base);
        let Some(field) = self.field_of(obj.layout, name) else {
            let held = self.pool.shapes.layout(obj.layout).name.clone();
            self.release(obj, expr.span);
            return self.gap(
                &format!("a field of `{held}`, which is not a struct here"),
                expr,
            );
        };
        let dst = self.temp(field.layout);
        self.copy(dst.slot, obj.slot + field.at, field.layout, expr.span);
        self.release(obj, expr.span);
        dst
    }

    /// `Point(x: 1, y: 2)`: the fields, written where the value is.
    ///
    /// Every field is evaluated before anything is stored, in source order,
    /// because an initializer's arguments are ordinary expressions and one
    /// of them may do something the next one sees.
    ///
    /// `http.Route(method: ..., path: ..., handler: ...)` is the same
    /// lowering, and that is not a convenience: `interp::init_host_type` is
    /// `interp::init_struct` "with the fields read from a schema instead of
    /// from a declaration", and a host type is ordinary data with no
    /// representation of its own. What the two spellings differ in is where
    /// the field names and types are read from, which is
    /// [`Body::initializer_fields`] and nothing else.
    fn struct_literal(&mut self, expr: &Expr, ty: &Ty, args: &[Arg]) -> Val {
        let Some(declared) = self.initializer_fields(ty) else {
            self.report(ty, expr.span);
            return self.dead(expr);
        };
        let Some(layout) = self.layout(ty, expr.span) else {
            return self.dead(expr);
        };
        let Some(fields) = self.fields_of(layout) else {
            return self.gap("an initializer for something that is not a struct", expr);
        };
        let Some(order) = self.labelled(args, &declared, expr) else {
            return self.dead(expr);
        };

        let mut held = Vec::with_capacity(args.len());
        for (arg, at) in args.iter().zip(&order) {
            let value = self.expr(&arg.value);
            // A field written `dyn Trait` is where a concrete value is
            // erased, exactly as a parameter written that way is.
            let value = self.erase(value, &arg.value, &declared[*at as usize].1);
            held.push(value);
        }
        let mut fitted = Vec::with_capacity(held.len());
        for (value, at) in held.into_iter().zip(&order) {
            let field = fields[*at as usize].clone();
            fitted.push((self.fit(value, field.layout, expr.span), field));
        }
        let dst = self.temp(layout);
        for (value, field) in &fitted {
            self.copy(dst.slot + field.at, value.slot, field.layout, expr.span);
        }
        for (value, _) in fitted.into_iter().rev() {
            self.release(value, expr.span);
        }
        dst
    }

    /// The fields an initializer fills, whoever declared them.
    ///
    /// A module's declaration and a host's schema are two records of one
    /// thing — a name and a type per field, in the order an initializer
    /// writes them — and this is the one place a caller has to know which of
    /// the two it is holding.
    fn initializer_fields(&mut self, ty: &Ty) -> Option<Vec<(std::sync::Arc<str>, Ty)>> {
        if let Ty::Host(qualified) = ty {
            return self.pool.shapes.host_fields(qualified);
        }
        shapes::struct_fields(self.checked, self.module, ty)
    }

    /// `http.Method.Get`: a case of an enum a host module declares.
    ///
    /// The oracle reaches it in two steps and this reads both of them at
    /// once. `http.Method` evaluates to a `Repr::Type` there, because the
    /// schema declares a type of that name and a type is not a value; and a
    /// field of a `Repr::Type` whose owner is a host module is
    /// `interp::host_enum_case`. Neither step is a fact about the frame, so
    /// there is nothing here to evaluate: the two names are read out of the
    /// syntax and the schema answers.
    ///
    /// What comes out is one discriminant word, because a schema gives its
    /// cases no payload to carry — the same word a declared case with an
    /// empty payload is, at the index
    /// [`Shapes::host_case`](super::shapes::Shapes::host_case) counts from
    /// the schema and [`Shapes::host_type`](super::shapes::Shapes::host_type)
    /// built the layout in.
    fn host_case(&mut self, expr: &Expr, base: &Expr, name: &str) -> Option<Val> {
        let ExprKind::Field {
            base: head,
            name: declared,
        } = &base.kind
        else {
            return None;
        };
        let ExprKind::Ident(module) = &head.kind else {
            return None;
        };
        // A binding of this frame shadows a module, exactly as it does
        // wherever else a name in front of a `.` is read.
        if self.frame.lookup(module).is_some() || !self.is_host_module(module) {
            return None;
        }
        let qualified = format!("{module}.{}", declared.node);
        let index = self.pool.shapes.host_case(&qualified, name)?;
        let layout = self.layout(&Ty::Host(qualified.into()), expr.span)?;
        let dst = self.temp(layout);
        self.write_case(dst.slot, layout, index, &[], expr.span);
        Some(dst)
    }

    /// Which field each argument of an initializer fills.
    ///
    /// Struct initialization is a synthesized labelled call, and a label is
    /// a parameter name: an unlabelled argument fills the next field, and a
    /// labelled one names its own. The checker has already refused a call
    /// that leaves a field unfilled or names one out of order, so this reads
    /// the answer rather than re-deciding it — and answers `None` where the
    /// two would have to disagree, which is a gap rather than a guess.
    fn labelled(
        &mut self,
        args: &[Arg],
        fields: &[(std::sync::Arc<str>, Ty)],
        expr: &Expr,
    ) -> Option<Vec<u32>> {
        let mut order = Vec::with_capacity(args.len());
        let mut next = 0usize;
        for arg in args {
            let at = match &arg.label {
                Some(label) => fields
                    .iter()
                    .position(|(name, _)| **name == *label.node.as_str())?,
                None => next,
            };
            if at >= fields.len() || at < next {
                return None;
            }
            order.push(at as u32);
            next = at + 1;
        }
        if order.len() != fields.len() {
            self.errors.push(gap::gap(
                "an initializer that leaves a field to its default",
                expr.span,
            ));
            return None;
        }
        Some(order)
    }

    /// One case of an enum: the discriminant, then the payload.
    ///
    /// Word 0 is the case index and the words after it are the payload
    /// region, which is wide enough for every case. The words this case does
    /// not fill are **zeroed**, so a reference word belonging to another case
    /// reads null and the collector — which never looks at the discriminant —
    /// traces nothing from it.
    fn enum_case(&mut self, expr: &Expr, ty: &Ty, case: &str, args: &[Arg]) -> Val {
        let Some((index, payload)) = shapes::case_at(self.checked, self.module, ty, case) else {
            self.report(ty, expr.span);
            return self.dead(expr);
        };
        let Some(layout) = self.layout(ty, expr.span) else {
            return self.dead(expr);
        };
        if args.len() != payload.len() {
            return self.gap("a case given a payload of another length", expr);
        }

        let Some((parts, _)) = self.case_of(layout, index) else {
            return self.gap("a case of something that is not an enum", expr);
        };
        let mut held = Vec::with_capacity(args.len());
        for ((arg, ty), part) in args.iter().zip(&payload).zip(&parts) {
            let value = self.expr(&arg.value);
            let value = self.erase(value, &arg.value, ty);
            held.push(self.fit(value, part.layout, arg.value.span));
        }
        let dst = self.temp(layout);
        self.write_case(dst.slot, layout, index, &held, expr.span);
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        dst
    }

    /// Writes case `index` of an enum-shaped layout into a location: the
    /// discriminant, then the payload words this case does not fill zeroed,
    /// then the parts.
    pub(super) fn write_case(
        &mut self,
        dst: Slot,
        layout: LayoutId,
        index: u32,
        parts: &[Val],
        span: Span,
    ) {
        let Some((placed, payload)) = self.case_of(layout, index) else {
            return;
        };
        self.emit(
            Inst::Int {
                dst,
                value: index as i64,
            },
            span,
        );
        let mut filled = vec![false; payload.len()];
        for part in &placed {
            let width = self.width(part.layout);
            for word in part.at..part.at + width {
                filled[word as usize] = true;
            }
        }
        for (word, held) in filled.iter().enumerate() {
            if *held {
                continue;
            }
            self.zero(dst + 1 + word as u32, shapes::scalar(payload[word]), span);
        }
        for (part, value) in placed.iter().zip(parts) {
            self.copy(dst + 1 + part.at, value.slot, part.layout, span);
        }
    }

    /// Builds the enclosing function's own `Err` or `None`, for `?` to leave
    /// through.
    ///
    /// It is built here rather than passed along: the value `?` was applied
    /// to is a `Result` of some other pair of types, and two `Result`s whose
    /// words differ are two layouts.
    pub(super) fn failure(&mut self, payload: Option<Val>, span: Span) -> Option<Val> {
        let ty = self.returns.clone();
        let case = match &ty {
            Ty::Result(..) => "Err",
            Ty::Option(_) => "None",
            _ => {
                self.errors.push(gap::gap(
                    "`?` in a function that answers neither a `Result` nor an `Option`",
                    span,
                ));
                return None;
            }
        };
        let (index, _) = shapes::case_at(self.checked, self.module, &ty, case)?;
        let layout = self.layout(&ty, span)?;
        let dst = self.temp(layout);
        let parts: Vec<Val> = payload.into_iter().collect();
        self.write_case(dst.slot, layout, index, &parts, span);
        Some(dst)
    }

    // ---- `?` ---------------------------------------------------------------

    /// `expr?`: the payload of the succeeding case, or the enclosing
    /// function's own failure.
    ///
    /// The discriminant is word 0 of the value, so the question is a
    /// comparison against the location itself: no read is needed, because
    /// the words are already in the frame.
    fn try_expr(&mut self, expr: &Expr, inner: &Expr) -> Val {
        let Some(ty) = self.settled_ty(inner) else {
            return self.dead(expr);
        };
        let carries = match &ty {
            Ty::Result(..) => "Ok",
            Ty::Option(_) => "Some",
            _ => {
                return self.gap(
                    "`?` on something that is neither a `Result` nor an `Option`",
                    expr,
                )
            }
        };
        let Some((index, _)) = shapes::case_at(self.checked, self.module, &ty, carries) else {
            self.report(&ty, expr.span);
            return self.dead(expr);
        };

        let subject = self.expr(inner);
        let Some((carrying, _)) = self.case_of(subject.layout, index) else {
            self.release(subject, expr.span);
            return self.gap("`?` on a value that is not an enum here", expr);
        };
        let wanted = self.temp(shapes::INT);
        self.emit(
            Inst::Int {
                dst: wanted.slot,
                value: index as i64,
            },
            expr.span,
        );
        let ok = self.temp(shapes::BOOL);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Eq,
                dst: ok.slot,
                a: subject.slot,
                b: wanted.slot,
            },
            expr.span,
        );
        self.give_back(wanted.slot, wanted.layout);
        let branch = self.emit(
            Inst::BranchFalse {
                cond: ok.slot,
                to: PENDING,
            },
            expr.span,
        );
        self.give_back(ok.slot, ok.layout);

        // The answer's layout is the payload's own, read off the case this
        // is unwrapping rather than off the type the checker settled for the
        // whole `?`. They are the same run of words wherever the checker
        // settled anything, and the case is the one that still answers where
        // it did not: `clock.timeout` declares `Result<Any, Error>`, so its
        // `Ok` carries a box and the checker records the `?` as an
        // unconstrained unknown. What is copied decides how wide the
        // destination is, and what is copied is the part.
        let dst = match carrying.first() {
            Some(part) => {
                let dst = self.temp(part.layout);
                self.copy(dst.slot, subject.slot + 1 + part.at, part.layout, expr.span);
                dst
            }
            None => {
                let dst = self.temp(shapes::UNIT);
                self.emit(Inst::Unit { dst: dst.slot }, expr.span);
                dst
            }
        };
        let carry_on = self.emit(Inst::Jump { to: PENDING }, expr.span);

        let failing = self.here();
        self.patch(branch, failing);
        // The failure carries the payload of the case it found, which for a
        // `Result` is the error and for an `Option` is nothing at all. The
        // frame ends at the `Return`, so nothing here is cleared: a slot
        // whose frame is gone retains nothing.
        let payload = match &ty {
            Ty::Result(..) => {
                let failing_case =
                    shapes::case_at(self.checked, self.module, &ty, "Err").map(|(at, _)| at);
                failing_case
                    .and_then(|at| self.case_of(subject.layout, at))
                    .and_then(|(parts, _)| parts.first().cloned())
                    .map(|part| Val::borrowed(subject.slot + 1 + part.at, part.layout))
            }
            _ => None,
        };
        if let Some(answer) = self.failure(payload, expr.span) {
            self.leave_open_scopes(0, expr.span);
            self.emit(Inst::Return { src: answer.slot }, expr.span);
            self.give_back(answer.slot, answer.layout);
        }

        let rest = self.here();
        self.patch(carry_on, rest);
        self.release(subject, expr.span);
        dst
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
        dst: Option<Dest>,
    ) {
        if let (Some(dst), None) = (dst, else_branch) {
            self.emit(Inst::Unit { dst: dst.slot }, span);
        }

        let cond = self.expr(condition);
        let branch = self.emit(
            Inst::BranchFalse {
                cond: cond.slot,
                to: PENDING,
            },
            condition.span,
        );
        self.release(cond, condition.span);

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
                match (&otherwise.kind, dst) {
                    // An `else { ... }` writes the destination the `then`
                    // side writes, rather than assembling its answer
                    // somewhere else and copying it in. The two sides of a
                    // join are the same event and cost the same.
                    (ExprKind::Block(block), _) => self.scoped_block(block, dst),
                    (_, Some(dst)) => {
                        let value = self.expr(otherwise);
                        self.store(dst, &value, otherwise);
                        self.release(value, otherwise.span);
                    }
                    (_, None) => self.discard(otherwise),
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
        self.release(cond, condition.span);

        self.loops.push(Loop {
            head,
            depth: self.frame.depth(),
            held: self.held.len(),
            breaks: Vec::new(),
            element: None,
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
                // A declared return type is a written type, so a `dyn Trait`
                // one erases here.
                let returns = self.returns.clone();
                let answer = self.erase(answer, value, &returns);
                // `return return x` leaves through the inner one, and the
                // outer has nothing to name.
                if !self.diverges(value) {
                    // Leaving a scope waits for or cancels its children
                    // whichever way it is left, and a `return` is one of the
                    // ways. The answer is evaluated first, because that is
                    // where the oracle evaluates it: `eval_scope` is handed a
                    // `Control::Return` that already carries a value.
                    self.leave_open_scopes(0, span);
                    self.emit(Inst::Return { src: answer.slot }, span);
                }
                // The frame ends at the `Return`, so the run is given back
                // without a `Clear`: there is nothing left to retain it.
                if answer.temp {
                    self.give_back(answer.slot, answer.layout);
                }
            }
            // A bare `return` only checks in a function answering `()`, and
            // the answer location holds one: a frame is zeroed on entry and
            // nothing but a `()` is ever written to a `Unit` slot.
            None => {
                let src = self.answer.slot;
                self.leave_open_scopes(0, span);
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
        let Some((depth, held, element)) =
            self.loops.last().map(|it| (it.depth, it.held, it.element))
        else {
            self.errors.push(gap::gap("a `break` outside a loop", span));
            return;
        };
        self.leave_turn(depth, held, element, span);
        let at = self.emit(Inst::Jump { to: PENDING }, span);
        self.loops
            .last_mut()
            .expect("the loop was found above")
            .breaks
            .push(at);
    }

    fn continue_expr(&mut self, span: Span) {
        let Some((head, depth, held, element)) = self
            .loops
            .last()
            .map(|it| (it.head, it.depth, it.held, it.element))
        else {
            self.errors
                .push(gap::gap("a `continue` outside a loop", span));
            return;
        };
        self.leave_turn(depth, held, element, span);
        self.emit(Inst::Jump { to: head }, span);
    }

    /// Ends the live range of everything a turn of a loop was holding, for a
    /// `break` or a `continue` that leaves it part way through.
    ///
    /// The scopes inside `depth` are left without being ended, and the loop
    /// goes on running or is about to be left, so a reference in one of them
    /// would be retained for the rest of the frame rather than for the rest
    /// of the turn.
    ///
    /// A `for` binding is not in any of those scopes — the loop owns the
    /// location, because the scope gives its slots back when it ends and the
    /// next turn writes them again — so it is cleared here beside them. A
    /// `continue` clears it too, and not only because the last turn of a loop
    /// may be the one that continues: the rule is that a binding dies when
    /// the turn it belonged to does, and a lowering that relied on the next
    /// turn's overwrite would be relying on there being one.
    ///
    /// # A temporary belongs to no scope, and was the hole
    ///
    /// `f(a, if c { b } else { break })` evaluates `a` into a temporary and
    /// then leaves the expression through the `break`, so nothing ever
    /// reaches the [`Body::release`] that would have ended `a`'s live range.
    /// The scopes hold nothing about it and the loop's element is not it, so
    /// the object stayed reachable from a slot of a live frame for as long as
    /// the frame lived — a leak rather than a crash, and one at every call
    /// site rather than only in a walk.
    ///
    /// [`Body::held`] is what answers it, and `held` is the mark the loop
    /// took when it began: everything above it is a temporary this turn made
    /// and nothing above it is read once the jump lands. The ones below are
    /// the loop's own machinery — the array a `for` walks is read again at
    /// the end the `break` jumps to — and an enclosing expression's, which
    /// the enclosing expression still owns.
    ///
    /// They are cleared before the bindings, which is the reverse of the
    /// order they were made in.
    fn leave_turn(&mut self, depth: usize, held: usize, element: Option<Dest>, span: Span) {
        // A scope this turn opened is left the same way a `return` leaves
        // one, and for the same reason: the jump is about to land somewhere
        // the scope's name no longer reaches, and leaving a scope waits for
        // or cancels its children whichever way it is left. Only the turn's
        // own scopes — one opened outside the loop is still open after the
        // `break` lands.
        let outside = self.scopes_outside_this_loop();
        self.leave_open_scopes(outside, span);
        let temporaries: Vec<(Slot, LayoutId)> = self.held[held.min(self.held.len())..]
            .iter()
            .rev()
            .copied()
            .collect();
        self.clear(&temporaries, span);
        let clears = self.frame.refs_within(depth);
        self.clear(&clears, span);
        // Guarded, as every other clear in this lowering is. A `Clear` of a
        // scalar zeroes a word the collector never reads and the next turn
        // never sees, so it is an instruction that does nothing — and the
        // rule the guard states is `Inst::Clear`'s own: it is emitted only
        // where the slot would otherwise retain something. Every other path
        // that ends a live range already asks — `Body::release`,
        // `Frame::pop_scope`, `walks::end_turn`, which does this same job on
        // the path a turn ends normally — and `self.held` arrives here
        // pre-filtered because `Body::hold` only records what holds a
        // reference. This one path did not ask, so `break` out of a `for`
        // over an `Array<Int>` emitted a `clear` on an `Int`.
        if let Some(element) = element {
            if self.holds_ref(element.layout) {
                self.zero(element.slot, element.layout, span);
            }
        }
    }

    // ---- calls -------------------------------------------------------------

    /// A call, and the one thing a trailing lambda is.
    ///
    /// `f(x) { ... }` and `tasks.spawn { ... }` are sugar. The parser has
    /// already built the block as a parameterless [`ExprKind::Lambda`], and
    /// `interp::eval_args` evaluates the written arguments in source order
    /// and then pushes that one on the end — unlabelled, not `var`, not
    /// spread. So the whole of it here is appending an [`Arg`] and lowering
    /// the call it was written on: [`Body::call_written`] sees an ordinary
    /// argument list and no path in it needs a rule for the shape the
    /// argument arrived in.
    ///
    /// It is done once, in front of that walk, rather than in each of its
    /// arms: which callee may take a trailing lambda is the checker's
    /// question and it has already answered it, so a second list of the
    /// forms that take one would be a second answer that could drift.
    fn call(&mut self, expr: &Expr, callee: &Expr, args: &[Arg], trailing: Option<&Expr>) -> Val {
        let Some(closure) = trailing else {
            return self.call_written(expr, callee, args);
        };
        let mut written = args.to_vec();
        written.push(Arg {
            label: None,
            is_var: false,
            spread: false,
            value: closure.clone(),
            span: closure.span,
        });
        self.call_written(expr, callee, &written)
    }

    /// A call whose arguments are all written out, whatever it turns out to
    /// be a call to.
    ///
    /// Which declaration a name reaches was settled twice over — by
    /// resolution, and by the checker recording the call's type — so this
    /// reads those answers in the order the language resolves them: a value
    /// in the frame, then a declaration of the package, then a type's
    /// initializer or a case of an enum, then a host operation. That order
    /// is the interpreter's, and it is what keeps a local named `println`
    /// from becoming a host call.
    ///
    /// The type arguments a call *writes* are not among what this reads, and
    /// that is not an omission. The checker resolved `f<Int>(x)` before this
    /// crate saw it and settled every type at the call site with the argument
    /// already applied, so which instantiation the call reaches is read off
    /// the facts rather than off the annotation —
    /// see [`Body::instantiation`].
    fn call_written(&mut self, expr: &Expr, callee: &Expr, args: &[Arg]) -> Val {
        // A callee the checker gave a function type to is a value, and a call
        // through one is an [`Inst::CallClosure`] whatever its shape. The
        // question is asked of the *checker's* answer rather than of the
        // source, because that is the one place both `g(1)` and
        // `handlers.get(0)(1)` are already settled — and it is asked first
        // because the arms below resolve names, which a value has already
        // stopped being. A method call is not among them: the checker takes
        // its own `Field` arm for one and never types `xs.map` on its own.
        if matches!(self.ty(callee), Some(Ty::Fn(_))) {
            return self.call_value(expr, callee, args);
        }
        match &callee.kind {
            ExprKind::Ident(name) if self.frame.lookup(name).is_none() => {
                self.call_named(expr, name, args)
            }
            ExprKind::Field { base, name } => self.call_through(expr, base, &name.node, args),
            _ => self.gap("a call to something other than a declared function", expr),
        }
    }

    /// A call written with a `.`: a method, an associated function, a
    /// builtin's operation, an enum's case, or a host module's.
    ///
    /// The checker settled which of those it is, so the order here reads its
    /// answers rather than guessing from the shape of the source. A recorded
    /// [`MethodTarget`](cove_sema::facts::MethodTarget) names a declaration
    /// of this package and is asked first, because it is the one answer
    /// nothing else can produce: the receiver's type decided it, and `Array`
    /// and a declared `Point` may both declare a `length`.
    fn call_through(&mut self, expr: &Expr, base: &Expr, name: &str, args: &[Arg]) -> Val {
        if let Some(target) = self.checked.facts.target(expr.span.file, expr.id).cloned() {
            return self.call_declared_method(expr, &target, base, args);
        }
        // A `dyn Trait` receiver names no declaration, because which one it
        // reaches is a fact about the value rather than about the source.
        if let Some(Ty::Dyn(trait_name)) = self.ty(base) {
            return self.call_dyn(expr, base, &trait_name, name, args);
        }
        // A receiver the declaration wrote as a bounded type parameter names
        // no declaration either, and for the same reason — but this body is
        // lowered for one type argument, so there is exactly one
        // implementation and it is found statically. That is the whole of
        // what a bound costs here: no dictionary, no vtable, and one
        // `Inst::Call`.
        if matches!(self.raw_ty(base), Some(Ty::Param(_))) {
            if let Some(id) = self.conformance(base, name) {
                return self.call_target(expr, id, Some(base), args);
            }
        }
        if self.is_namespace(base) {
            return self.call_qualified(expr, base, name, args);
        }
        self.call_builtin_method(expr, base, name, args)
    }

    /// A call written as a bare name.
    fn call_named(&mut self, expr: &Expr, name: &str, args: &[Arg]) -> Val {
        if let Some(id) = self.plan.resolve(self.checked, self.module, name) {
            return self.call_declared(expr, id, args);
        }
        if let Some(ty) = self.ty(expr) {
            // `Ok(v)`, `Err(e)`, `Some(v)`: the language's own cases, which
            // are written unqualified because there is nothing else they
            // could name.
            if shapes::case_at(self.checked, self.module, &ty, name).is_some() {
                return self.enum_case(expr, &ty, name, args);
            }
            // `Point(x: 1, y: 2)`, and `Error("...")` and
            // `MapEntry(key: k, value: v)`, which are the same shape with the
            // fields declared by the language rather than by a module.
            if matches!(ty, Ty::Struct(..) | Ty::Error | Ty::MapEntry(..)) {
                return self.struct_literal(expr, &ty, args);
            }
            // `Shared(value)`, which is an initializer of a type the language
            // declares rather than a free builtin — the same arm the three
            // above are, with the one difference that a cell's payload is one
            // unlabelled value and its object carries a lock word in front of
            // it.
            if matches!(ty, Ty::Shared(_)) {
                return self.shared_new(expr, &ty, args);
            }
        }
        if let Some(module) = self.host_module_of(name) {
            return self.call_host(expr, &module, name, args);
        }
        // The builtins that are called on nothing, asked of the shared table
        // once and asked last, which is where the interpreter asks: a
        // declaration, a case, an initializer and a host item all win over
        // one of these names, so a package that declares its own `assert`
        // gets its own.
        //
        // Only the assertions are here. The constructors — `Ok`, `Err`,
        // `Some`, `Error`, `Shared` — are cases and initializers of types the
        // language declares, and the arms above already build them from the
        // type the checker settled, which is where a case belongs.
        if let Some(schema) = cove_schema::builtins::free_builtin(name) {
            if schema.kind == FreeBuiltinKind::Assertion {
                return self.assertion(expr, schema, args);
            }
        }
        self.gap(
            "a call to a declaration that is not a function of this package",
            expr,
        )
    }

    /// A call written through a name that is not a value: `console.println`,
    /// `Verdict.Drop`.
    fn call_qualified(&mut self, expr: &Expr, base: &Expr, name: &str, args: &[Arg]) -> Val {
        let ExprKind::Ident(head) = &base.kind else {
            return self.gap("a call reached through an expression", expr);
        };
        if self.is_host_module(head) {
            // `http.Route(method: ..., path: ...)` initializes a type the
            // host declares; anything else is one of its operations. The
            // oracle asks in that order and for the reason it gives: a type
            // is not callable, so the two cannot be confused — and a schema
            // is the only thing that says which of them a name is.
            //
            // Asked before the boundary rather than at it, because an
            // initializer never reaches the boundary. Its labels are field
            // names, its answer is a run of words this side lays out, and
            // `Body::crossable` — which refuses a label, since a host
            // operation's parameters have positions and no names — was
            // refusing a call that was never a host call.
            if let Some(ty) = self.ty(expr) {
                if self
                    .pool
                    .shapes
                    .host_fields(&format!("{head}.{name}"))
                    .is_some()
                {
                    return self.struct_literal(expr, &ty, args);
                }
            }
            return self.call_host(expr, head, name, args);
        }
        if let Some(ty) = self.ty(expr) {
            if shapes::case_at(self.checked, self.module, &ty, name).is_some() {
                return self.enum_case(expr, &ty, name, args);
            }
            // `Vector.of(1, 2)`, `Set.of(1, 2)`, `Map.of(...)`: an
            // associated function of a builtin type, written through the
            // type's own name rather than through a value.
            if name == "of" && collections::namespace_of(head, &ty) {
                return self.collection_of(expr, head, args);
            }
            // `Int.parse(text)`, `Duration.millis(n)`: the rest of them,
            // which the machine performs rather than the instruction set.
            if methods::associated(head, name, &ty) {
                return self.call_associated(expr, head, name, args);
            }
        }
        // `forager.decide(view, observation)`, `lib.Box(item: ...)`: a module
        // `use` imported whole, reached through the name it is visible under.
        // `ResolvedModule::module_imports` is the fact, and this is the third
        // consumer of it: the checker reads it in `typeck::qualified_key`,
        // the oracle in `interp::imported_module`, and this is the
        // lowering's own reading — a fourth reading, the predecessor's own,
        // was deleted with it at the cutover.
        //
        // Its two halves are the oracle's two: an exported function, then an
        // exported struct's initializer. Whether the owner exports the name
        // is not asked again — resolution refused a qualified reach for one
        // it does not — so this is `Plan::resolve` and `Body::struct_literal`
        // exactly as an unqualified call to either would be.
        if let Some(owner) = self
            .checked
            .modules
            .get(self.module)
            .and_then(|resolved| resolved.module_imports.get(head))
            .cloned()
        {
            if let Some(id) = self.plan.resolve(self.checked, &owner, name) {
                return self.call_declared(expr, id, args);
            }
            if let Some(ty) = self.ty(expr) {
                if matches!(ty, Ty::Struct(..)) {
                    return self.struct_literal(expr, &ty, args);
                }
            }
        }
        self.gap("a call to a method or an associated function", expr)
    }

    /// A call to a declared function of this package.
    fn call_declared(&mut self, expr: &Expr, id: crate::FunctionId, args: &[Arg]) -> Val {
        self.call_target(expr, id, None, args)
    }

    /// A call across the boundary.
    ///
    /// # What the runtime must implement
    ///
    /// A [`HostOp`] is the module and operation as the source writes them —
    /// `console`.`println`, `files`.`read` — and the boundary looks the pair
    /// up in the registry exactly as the interpreter does. The arguments are
    /// the locations in source order, materialised into public `Value`s;
    /// every one is a single word, because an operand that is not is boxed
    /// on the way in and the box carries its own layout.
    ///
    /// [`HostOp::result`] is the layout the host's answer is written back
    /// into, and it is the schema's declared result type as the checker read
    /// it. A schema that declared `Any` gives a boxed layout: from that call
    /// onwards the program holds a value no schema described, so it is a box
    /// carrying its own description rather than a bare run of words.
    fn call_host(&mut self, expr: &Expr, module: &str, operation: &str, args: &[Arg]) -> Val {
        let Some(result) = self.host_result(expr, module, None, operation) else {
            return self.dead(expr);
        };
        if let Some(bad) = self.crossable(args) {
            return self.refused(bad, expr, result);
        }
        let op = self.pool.host_op(HostOp {
            module: module.into(),
            operation: operation.into(),
            resource: None,
            result,
        });

        let mut held = Vec::with_capacity(args.len());
        for arg in args {
            held.push(self.expr(&arg.value));
        }
        let list = self.pool.args.intern(held.iter().map(Val::arg).collect());
        let dst = self.temp(result);
        self.emit(
            Inst::CallHost {
                dst: dst.slot,
                op,
                args: list,
            },
            expr.span,
        );
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        dst
    }

    /// `writer.writeLine(line)`: a call across the boundary addressed to a
    /// resource the host keeps.
    ///
    /// # Why the receiver is not an argument
    ///
    /// ADR 0013 makes a handle a *name* and gives the host the only record of
    /// what is open, so which resource answers is decided by the word in the
    /// receiver and never by the module the source wrote in front of it.
    /// `HostRegistry::call_resource` says the same shape: it takes the handle
    /// as the thing being addressed and hands the host only what follows.
    /// So [`Inst::CallResource`] names the receiver as an operand of its own
    /// — an [`crate::Arg`] is a location the boundary materialises, and
    /// materialising a name into a `Value` in order to take it apart again
    /// would make the argument list something other than the arguments.
    ///
    /// Everything else is [`Body::call_host`]. The operation is one entry of
    /// the same table, carrying the resource kind so that `files.write` and
    /// `files.Writer.write` are two entries; the result layout is the one the
    /// checker settled; and the arguments have to cross the boundary under
    /// the same rules, because the boundary is the same boundary.
    pub(super) fn call_resource(
        &mut self,
        expr: &Expr,
        base: &Expr,
        qualified: &str,
        operation: &str,
        args: &[Arg],
    ) -> Val {
        // A host type that is plain *data* has fields rather than
        // operations, and the checker has already refused a method call on
        // one — but which of the two a name is is the schema's answer and
        // not this lowering's, so it is read rather than assumed.
        let named = format!("`{qualified}.{operation}`");
        let Some((module, kind)) = qualified.rsplit_once('.') else {
            return self.gap(
                &format!("{named}, an operation of a host type with no module"),
                expr,
            );
        };
        if !self.pool.shapes.is_resource(module, kind) {
            return self.gap(
                &format!("{named}, an operation of a host type the host does not keep"),
                expr,
            );
        }
        let Some(result) = self.host_result(expr, module, Some(kind), operation) else {
            return self.dead(expr);
        };
        if let Some(bad) = self.crossable(args) {
            return self.refused(bad, expr, result);
        }
        let op = self.pool.host_op(HostOp {
            module: module.into(),
            operation: operation.into(),
            resource: Some(kind.into()),
            result,
        });

        let receiver = self.expr(base);
        let mut held = Vec::with_capacity(args.len());
        for arg in args {
            held.push(self.expr(&arg.value));
        }
        let list = self.pool.args.intern(held.iter().map(Val::arg).collect());
        let dst = self.temp(result);
        self.emit(
            Inst::CallResource {
                dst: dst.slot,
                receiver: receiver.slot,
                op,
                args: list,
            },
            expr.span,
        );
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        self.release(receiver, expr.span);
        dst
    }

    /// A host call this lowering will not emit, answering a location of the
    /// layout the call would have written.
    ///
    /// The layout matters even though nothing is emitted. A gap answers a
    /// value the rest of the walk goes on reading, and answering the wrong
    /// width makes every form built over it raise a second complaint about
    /// the first one's consequence — `timeout(1s) { .. }?` used to report
    /// the closure, then that `?` had no enum, then that the `let` had no
    /// type. One mistake is one diagnostic, and the schema still says what
    /// the answer would have been.
    fn refused(&mut self, what: &str, expr: &Expr, result: LayoutId) -> Val {
        self.errors.push(gap::gap(what, expr.span));
        self.temp(result)
    }

    /// What stops an argument from crossing the boundary, if anything does.
    ///
    /// One answer for both directions a host call is addressed, because what
    /// may cross is a fact about the boundary rather than about which of the
    /// two found the callee.
    fn crossable(&mut self, args: &[Arg]) -> Option<&'static str> {
        for arg in args {
            let what = if arg.label.is_some() {
                "a labelled argument to a host operation"
            } else if arg.is_var {
                "a `var` argument to a host operation"
            } else if arg.spread {
                "a spread argument to a host operation"
            } else {
                // A function value is *not* refused here. `clock.timeout(500ms)
                // { ... }` lowers as an ordinary last argument: the location is
                // one `Repr::Ref` word naming the closure's environment, and
                // the boundary follows the word rather than copying anything
                // out of it. A host that keeps the callback keeps the object
                // alive itself, so this lowering's ordinary clear at the
                // argument's last use is right whichever order they happen in.
                continue;
            };
            return Some(what);
        }
        None
    }

    /// The layout a host operation's answer is written into.
    ///
    /// It is the schema's result type, read **out of the schema**, because
    /// the type the checker recorded cannot say the one thing this has to
    /// know. `cove_schema::HostType::Any` becomes an unconstrained unknown
    /// there, and so does a type parameter nothing settled: one value for
    /// two facts, and only one of them is a value with a representation.
    /// `docs/LINEAR_VM.md` separates them — "a value whose type is
    /// *intentionally* erased ... is one `Ref` word naming a `Boxed`
    /// object", and "a `Ty::Unknown` is not that" — so the erased one is a
    /// box and the other is a compile error. The schema is where the
    /// difference still exists, and
    /// [`Shapes::host_layout`](super::shapes::Shapes::host_layout) is what
    /// reads it.
    ///
    /// Which schemas: the ones this compilation was given, which is the set
    /// the checker resolved the call against and includes an embedder's. A
    /// call this lowering cannot find a schema for is not refused on that
    /// account — the checker settled a type for it, and that type is the
    /// answer, exactly as it was before there was anything else to ask. The
    /// one way that happens is an embedding that handed the lowering a
    /// smaller set than the checker read, and there the unconstrained
    /// unknown is taken at face value and boxed: a host call site is the
    /// only place in this lowering where an unknown is anything but an
    /// error, and a *host call* is what this is.
    fn host_result(
        &mut self,
        expr: &Expr,
        module: &str,
        resource: Option<&str>,
        operation: &str,
    ) -> Option<LayoutId> {
        if let Some(declared) = self
            .pool
            .shapes
            .declared_result(module, resource, operation)
        {
            if let Some(id) = self
                .pool
                .shapes
                .host_layout(self.checked, self.module, declared)
            {
                return Some(id);
            }
        }
        let ty = self.settled_ty(expr)?;
        if matches!(ty, Ty::Unknown(cove_sema::typeck::Unknown::Unconstrained)) {
            return Some(self.pool.shapes.any());
        }
        self.layout(&ty, expr.span)
    }

    /// The address a `var` argument passes.
    ///
    /// A place is one word. `var total` is the address of a run of this
    /// frame, and `var point.x` is the address of the word inside it that
    /// the field arithmetic already found — an inline field needs no
    /// indirection to name, so both are one [`Inst::AddrOfSlot`].
    pub(super) fn address_of(&mut self, place: &Expr) -> Val {
        let Some(found) = self.place_of(place) else {
            return self.gap("a `var` argument that is not a place", place);
        };
        match found {
            Place::Here { slot, .. } => {
                let dst = self.temp(shapes::ADDR);
                self.emit(
                    Inst::AddrOfSlot {
                        dst: dst.slot,
                        slot,
                    },
                    place.span,
                );
                dst
            }
            // A `var` parameter passed straight on is the same address: the
            // callee writes the storage the original caller named, which is
            // what makes the alias reach through however many frames pass it
            // along. A field of one is that address plus the field's offset,
            // which is the same statement one word further in.
            Place::Through { addr, at, .. } => self.part_address(addr, at, place.span),
        }
    }

    // ---- reading the module's namespaces --------------------------------

    /// Whether a name in front of a `.` is a namespace rather than a value.
    ///
    /// The question is answered the way the interpreter answers it: a name
    /// the frame binds is a value, and anything else in that position is an
    /// enum's own name, a host module, or a module imported whole.
    fn is_namespace(&self, base: &Expr) -> bool {
        match &base.kind {
            ExprKind::Ident(name) => self.frame.lookup(name).is_none(),
            _ => false,
        }
    }

    fn is_host_module(&self, name: &str) -> bool {
        self.checked
            .modules
            .get(self.module)
            .is_some_and(|resolved| resolved.host_uses.contains(name))
    }

    /// The host module an unqualified name was imported from, as
    /// `use console.println` imports `println`.
    fn host_module_of(&self, name: &str) -> Option<String> {
        self.checked
            .modules
            .get(self.module)?
            .host_items
            .get(name)
            .cloned()
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

/// Which reading a one-word comparison gives its operands.
///
/// Read by [`Body::assertion`] too: `assertEqual` compares the way `==`
/// does, and one function is what says so.
pub(super) fn compare_of(repr: Repr) -> Compare {
    match repr {
        Repr::Float => Compare::Float,
        Repr::Bool => Compare::Bool,
        Repr::Ref => Compare::Str,
        _ => Compare::Int,
    }
}

/// The `Int` word an expression **is**, where the source wrote one.
///
/// This is what decides that an operand stays in the instruction rather than
/// becoming an [`Inst::Int`] into a temporary that only the next instruction
/// reads. It answers about the *syntax*, not about a value some analysis
/// worked out: the literal is there in the source, and what is being kept is
/// the fact that it was.
///
/// Three shapes, and the third is the only judgement call:
///
/// - an `Int` literal;
/// - a `Duration` literal, whose word is nanoseconds and so is an `i64` —
///   only the boundary cares what the word is called;
/// - a negation of one of those. `-1` is parsed as a `Neg` of `1` and not as
///   a literal, so without this line `i > -1` would materialise a `1`,
///   negate it into a second temporary, and compare against that — three
///   instructions where the source wrote one number. This is the whole of
///   the constant folding here, and it is deliberately the whole: it looks
///   through a lowering artifact to a literal that is unambiguously present,
///   and it does not evaluate anything. `1 + 1` is not a literal and is not
///   folded.
///
/// [`i64::MIN`] cannot be written — the lexer refuses the magnitude — but
/// `checked_neg` is what says so here rather than a comment, because a
/// wrapping negation would silently answer the wrong constant if it ever
/// could.
pub(super) fn int_literal(expr: &Expr) -> Option<i64> {
    match &expr.kind {
        ExprKind::Int(value) => Some(*value),
        ExprKind::Duration(nanos) => Some(*nanos),
        ExprKind::Unary {
            op: UnaryOp::Neg,
            operand,
        } => int_literal(operand)?.checked_neg(),
        _ => None,
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
