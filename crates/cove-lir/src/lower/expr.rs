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
//! Only two places are not a run of this frame: what a `var` parameter names
//! — an address, which no instruction can offset, so a write to a field
//! through one is a load, a write and a store back — and a field of a heap
//! object, which is what a broken recursive layout needs.
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
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, BinaryOp, Block, Expr, ExprKind, StrPart, Type, UnaryOp};

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
    /// This is what a `var` parameter names. There is no instruction that
    /// adds an offset to an address — a place is the address of the *first*
    /// word of a value location — so a field of one is reached by loading
    /// the whole value, working on the words, and storing them back.
    Through {
        addr: Slot,
        /// The layout of the whole value the address names.
        whole: LayoutId,
        /// The field's word offset within it.
        at: u32,
        layout: LayoutId,
    },
    /// A field of a heap object: what a broken recursive layout needs.
    ///
    /// `obj` holds the reference and `at` is a payload word index. A
    /// [`Shape::Boxed`](crate::Shape::Boxed) object keeps the value it holds
    /// inline after one word naming its layout, so a field of a boxed
    /// `Node` is payload word `1 + Field::at` — the same arithmetic as an
    /// inline field, done against an object's payload instead of the frame.
    Field {
        obj: Slot,
        at: u32,
        layout: LayoutId,
    },
}

impl Place {
    fn layout(&self) -> LayoutId {
        match self {
            Place::Here { layout, .. }
            | Place::Through { layout, .. }
            | Place::Field { layout, .. } => *layout,
        }
    }
}

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
                generics,
                args,
                trailing,
            } => self.call(expr, callee, generics, args, trailing.is_some()),
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

            ExprKind::Await(_) => self.gap("`await`", expr),
            ExprKind::Lambda { .. } => self.gap("a lambda", expr),
            ExprKind::Scope { .. } => self.gap("`scope`", expr),
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
    fn unit_value(&mut self, span: Span) -> Val {
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
        if let Some(ty) = self.ty(expr).cloned() {
            if shapes::case_at(self.checked, self.module, &ty, name).is_some() {
                return self.enum_case(expr, &ty, name, &[]);
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
                    let value = self.describe_itself(value, inner.span);
                    pieces.push(value);
                }
            }
        }

        let slots: Vec<Slot> = pieces.iter().map(|piece| piece.slot).collect();
        let args = self.pool.args.intern(slots);
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

    /// Boxes a value that is not one word, so that an operand list can carry
    /// it.
    ///
    /// A builtin is handed slots and nothing else: [`Inst::CallBuiltin`] has
    /// no place to say how wide each operand is or what its words mean. A
    /// scalar and a reference are self-describing enough — the slot's own
    /// `Repr` says which, and an object's header says what it is — but an
    /// inline struct, enum or range is a run of words with nothing attached,
    /// so it goes in a box that carries its layout.
    ///
    /// This is the one place the model costs an allocation where the
    /// predecessor did not, and it is a finding rather than a design: the
    /// alternative is a per-operand layout on the instruction.
    pub(super) fn describe_itself(&mut self, value: Val, span: Span) -> Val {
        if self.width(value.layout) == 1 {
            return value;
        }
        let dst = self.temp(shapes::REF);
        self.emit(
            Inst::Box {
                dst: dst.slot,
                src: value.slot,
                layout: value.layout,
            },
            span,
        );
        self.release(value, span);
        dst
    }

    // ---- operators -------------------------------------------------------

    fn unary(&mut self, expr: &Expr, op: UnaryOp, operand: &Expr) -> Val {
        let a = self.expr(operand);
        let dst = self.answer_of(expr);
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
        let b = self.expr(rhs);
        let dst = self.answer_of(expr);
        // A value the instruction set cannot compare in one step is compared
        // by walking it, which is a call rather than an instruction.
        if arith_of(op).is_none() && !self.is_scalar(a.layout) {
            let equal = op == BinaryOp::Eq;
            self.compare_values(expr, equal, lhs, dst.slot, &a, &b);
            self.release(b, expr.span);
            self.release(a, expr.span);
            return dst;
        }
        let operand = self.frame.repr(a.slot);
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
                    let whole = self.layout_of(expr);
                    return Some(Place::Through {
                        addr: slot,
                        whole,
                        at: 0,
                        layout: whole,
                    });
                }
                Some(Place::Here { slot, layout })
            }
            ExprKind::Field { base, name } => {
                let outer = self.place_of(base)?;
                // A recursive layout is one reference, and what it names
                // holds the type's own inline words. Reaching a field of one
                // is the same offset arithmetic against the object's
                // payload, which begins one word after the layout the box
                // records.
                let held = outer.layout();
                let (inside, shift) = match self.pool.shapes.unboxed(held) {
                    Some(inline) => (inline, 1),
                    None => (held, 0),
                };
                let field = match self.field_of(inside, &name.node) {
                    Some(field) => field,
                    None => {
                        let named = self.pool.shapes.layout(held).name.clone();
                        self.errors.push(gap::gap(
                            &format!("a field of `{named}`, which is not a struct here"),
                            expr.span,
                        ));
                        return None;
                    }
                };
                Some(match (outer, shift) {
                    (Place::Here { slot, .. }, 0) => Place::Here {
                        slot: slot + field.at,
                        layout: field.layout,
                    },
                    (Place::Here { slot, .. }, _) => Place::Field {
                        obj: slot,
                        at: 1 + field.at,
                        layout: field.layout,
                    },
                    (
                        Place::Through {
                            addr, whole, at, ..
                        },
                        0,
                    ) => Place::Through {
                        addr,
                        whole,
                        at: at + field.at,
                        layout: field.layout,
                    },
                    (Place::Field { obj, at, .. }, 0) => Place::Field {
                        obj,
                        at: at + field.at,
                        layout: field.layout,
                    },
                    _ => {
                        // A box reached through an address, or a box inside
                        // a box: both need the reference in a slot of its
                        // own before the field can be named, and finding a
                        // place must not be the thing that allocates one.
                        self.errors.push(gap::gap(
                            "a field of a recursive value reached through another one",
                            expr.span,
                        ));
                        return None;
                    }
                })
            }
            _ => None,
        }
    }

    /// The value a place holds.
    ///
    /// A run of this frame is answered as it stands, borrowed: the words are
    /// already there and reading them is not an event. What is behind an
    /// address has to be brought over, and the whole value comes with it
    /// because an address cannot be offset.
    fn read_place(&mut self, place: Place, span: Span) -> Val {
        match place {
            Place::Here { slot, layout } => Val::borrowed(slot, layout),
            Place::Field { obj, at, layout } => {
                let dst = self.temp(layout);
                self.emit(
                    Inst::LoadField {
                        dst: dst.slot,
                        obj,
                        at,
                        layout,
                    },
                    span,
                );
                dst
            }
            // An address names the *first* word of a value location, so a
            // field at offset 0 is at the address itself however wide the
            // whole value is: the load reads the field's words and stops.
            Place::Through {
                addr,
                at: 0,
                layout,
                ..
            } => {
                let dst = self.temp(layout);
                self.emit(
                    Inst::Load {
                        dst: dst.slot,
                        addr,
                        layout,
                    },
                    span,
                );
                dst
            }
            Place::Through {
                addr,
                whole,
                at,
                layout,
            } => {
                let held = self.temp(whole);
                self.emit(
                    Inst::Load {
                        dst: held.slot,
                        addr,
                        layout: whole,
                    },
                    span,
                );
                let dst = self.temp(layout);
                self.copy(dst.slot, held.slot + at, layout, span);
                self.release(held, span);
                dst
            }
        }
    }

    /// Writes a value into a place.
    ///
    /// A write through an address updates the caller's own words, which is
    /// the aliasing `var` specifies and needs no copy back. A write to a
    /// *field* through one is a load, a write into the words and a store —
    /// because a place is the address of the first word of a value location
    /// and no instruction offsets one.
    fn write_place(&mut self, place: Place, value: &Val, span: Span) {
        match place {
            Place::Here { slot, layout } => self.copy(slot, value.slot, layout, span),
            Place::Field { obj, at, layout } => {
                self.emit(
                    Inst::StoreField {
                        obj,
                        at,
                        src: value.slot,
                        layout,
                    },
                    span,
                );
            }
            Place::Through {
                addr,
                at: 0,
                layout,
                ..
            } => {
                self.emit(
                    Inst::Store {
                        addr,
                        src: value.slot,
                        layout,
                    },
                    span,
                );
            }
            Place::Through {
                addr,
                whole,
                at,
                layout,
            } => {
                let held = self.temp(whole);
                self.emit(
                    Inst::Load {
                        dst: held.slot,
                        addr,
                        layout: whole,
                    },
                    span,
                );
                self.copy(held.slot + at, value.slot, layout, span);
                self.emit(
                    Inst::Store {
                        addr,
                        src: held.slot,
                        layout: whole,
                    },
                    span,
                );
                self.release(held, span);
            }
        }
    }

    // ---- assignment -------------------------------------------------------

    fn assign(&mut self, op: Option<BinaryOp>, target: &Expr, value: &Expr, span: Span) {
        let Some(place) = self.place_of(target) else {
            // A field target has already said what stopped it, and saying
            // so twice about one assignment buries the sentence that names
            // the work.
            if !matches!(target.kind, ExprKind::Field { .. }) {
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
                let source = self.expr(value);
                let num = num_of(self.frame.repr(slot));
                self.emit(
                    Inst::Arith {
                        num,
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
                let source = self.expr(value);
                let dst = self.temp(held.layout);
                let num = num_of(self.frame.repr(held.slot));
                self.emit(
                    Inst::Arith {
                        num,
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
        if self.is_namespace(base) {
            if let Some(ty) = self.ty(expr).cloned() {
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
        let (inside, shift) = match self.pool.shapes.unboxed(obj.layout) {
            Some(inline) => (inline, 1),
            None => (obj.layout, 0),
        };
        let Some(field) = self.field_of(inside, name) else {
            let held = self.pool.shapes.layout(obj.layout).name.clone();
            self.release(obj, expr.span);
            return self.gap(
                &format!("a field of `{held}`, which is not a struct here"),
                expr,
            );
        };
        let dst = self.temp(field.layout);
        if shift == 0 {
            self.copy(dst.slot, obj.slot + field.at, field.layout, expr.span);
        } else {
            self.emit(
                Inst::LoadField {
                    dst: dst.slot,
                    obj: obj.slot,
                    at: 1 + field.at,
                    layout: field.layout,
                },
                expr.span,
            );
        }
        self.release(obj, expr.span);
        dst
    }

    /// `Point(x: 1, y: 2)`: the fields, written where the value is.
    ///
    /// Every field is evaluated before anything is stored, in source order,
    /// because an initializer's arguments are ordinary expressions and one
    /// of them may do something the next one sees.
    fn struct_literal(&mut self, expr: &Expr, ty: &Ty, args: &[Arg]) -> Val {
        let Some(declared) = shapes::struct_fields(self.checked, self.module, ty) else {
            self.errors.push(super::describe(ty, expr.span));
            return self.dead(expr);
        };
        let Some(layout) = self.layout(ty, expr.span) else {
            return self.dead(expr);
        };
        let inline = self.pool.shapes.unboxed(layout).unwrap_or(layout);
        let Some(fields) = self.fields_of(inline) else {
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
        let dst = self.temp(inline);
        for (value, field) in &fitted {
            self.copy(dst.slot + field.at, value.slot, field.layout, expr.span);
        }
        for (value, _) in fitted.into_iter().rev() {
            self.release(value, expr.span);
        }
        self.enclose(dst, layout, expr.span)
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
            self.errors.push(super::describe(ty, expr.span));
            return self.dead(expr);
        };
        let Some(layout) = self.layout(ty, expr.span) else {
            return self.dead(expr);
        };
        if args.len() != payload.len() {
            return self.gap("a case given a payload of another length", expr);
        }

        let inline = self.pool.shapes.unboxed(layout).unwrap_or(layout);
        let Some((parts, _)) = self.case_of(inline, index) else {
            return self.gap("a case of something that is not an enum", expr);
        };
        let mut held = Vec::with_capacity(args.len());
        for ((arg, ty), part) in args.iter().zip(&payload).zip(&parts) {
            let value = self.expr(&arg.value);
            let value = self.erase(value, &arg.value, ty);
            held.push(self.fit(value, part.layout, arg.value.span));
        }
        let dst = self.temp(inline);
        self.write_case(dst.slot, inline, index, &held, expr.span);
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        self.enclose(dst, layout, expr.span)
    }

    /// Puts a freshly built value in the box its layout says it lives in.
    ///
    /// A declaration whose layout contained itself is one heap address
    /// wherever it is mentioned, so the words are built in the frame and
    /// then moved into an object of the layout the cycle was broken with.
    /// A declaration that is not recursive is already what it is, and this
    /// answers it unchanged.
    fn enclose(&mut self, built: Val, layout: LayoutId, span: Span) -> Val {
        if built.layout == layout {
            return built;
        }
        let dst = self.temp(layout);
        self.emit(
            Inst::Box {
                dst: dst.slot,
                src: built.slot,
                layout: built.layout,
            },
            span,
        );
        self.release(built, span);
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
        let Some(ty) = self.owned_ty(inner) else {
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
            self.errors.push(super::describe(&ty, expr.span));
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

        let layout = self.layout_of(expr);
        let dst = self.temp(layout);
        if let Some(part) = carrying.first() {
            self.copy(dst.slot, subject.slot + 1 + part.at, part.layout, expr.span);
        } else {
            self.emit(Inst::Unit { dst: dst.slot }, expr.span);
        }
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
        let Some((depth, element)) = self.loops.last().map(|it| (it.depth, it.element)) else {
            self.errors.push(gap::gap("a `break` outside a loop", span));
            return;
        };
        self.leave_turn(depth, element, span);
        let at = self.emit(Inst::Jump { to: PENDING }, span);
        self.loops
            .last_mut()
            .expect("the loop was found above")
            .breaks
            .push(at);
    }

    fn continue_expr(&mut self, span: Span) {
        let Some((head, depth, element)) =
            self.loops.last().map(|it| (it.head, it.depth, it.element))
        else {
            self.errors
                .push(gap::gap("a `continue` outside a loop", span));
            return;
        };
        self.leave_turn(depth, element, span);
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
    fn leave_turn(&mut self, depth: usize, element: Option<Dest>, span: Span) {
        let clears = self.frame.refs_within(depth);
        self.clear(&clears, span);
        if let Some(element) = element {
            self.zero(element.slot, element.layout, span);
        }
    }

    // ---- calls -------------------------------------------------------------

    /// A call, whatever it turns out to be a call to.
    ///
    /// Which declaration a name reaches was settled twice over — by
    /// resolution, and by the checker recording the call's type — so this
    /// reads those answers in the order the language resolves them: a value
    /// in the frame, then a declaration of the package, then a type's
    /// initializer or a case of an enum, then a host operation. That order
    /// is the interpreter's, and it is what keeps a local named `println`
    /// from becoming a host call.
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
        match &callee.kind {
            ExprKind::Ident(name) if self.frame.lookup(name).is_none() => {
                self.call_named(expr, name, args)
            }
            ExprKind::Ident(_) => self.gap("a call through a function value", expr),
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
        if let Some(Ty::Dyn(trait_name)) = self.ty(base).cloned() {
            return self.call_dyn(expr, base, &trait_name, name, args);
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
        if let Some(ty) = self.ty(expr).cloned() {
            // `Ok(v)`, `Err(e)`, `Some(v)`: the language's own cases, which
            // are written unqualified because there is nothing else they
            // could name.
            if shapes::case_at(self.checked, self.module, &ty, name).is_some() {
                return self.enum_case(expr, &ty, name, args);
            }
            // `Point(x: 1, y: 2)`, and `Error("...")`, which is the same
            // shape with the field declared by the language rather than by
            // a module.
            if matches!(ty, Ty::Struct(..) | Ty::Error) {
                return self.struct_literal(expr, &ty, args);
            }
        }
        if let Some(module) = self.host_module_of(name) {
            return self.call_host(expr, &module, name, args);
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
            return self.call_host(expr, head, name, args);
        }
        if let Some(ty) = self.ty(expr).cloned() {
            if shapes::case_at(self.checked, self.module, &ty, name).is_some() {
                return self.enum_case(expr, &ty, name, args);
            }
            // `Vector.of(1, 2)`: an associated function of a builtin type,
            // written through the type's own name rather than through a
            // value.
            if name == "of" && collections::namespace_of(head, &ty) {
                return self.vector_of(expr, args);
            }
            // `Int.parse(text)`, `Duration.millis(n)`: the rest of them,
            // which the machine performs rather than the instruction set.
            if methods::associated(head, name, &ty) {
                return self.call_associated(expr, head, name, args);
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
        for arg in args {
            let what = if arg.label.is_some() {
                "a labelled argument to a host operation"
            } else if arg.is_var {
                "a `var` argument to a host operation"
            } else if arg.spread {
                "a spread argument to a host operation"
            } else {
                continue;
            };
            return self.gap(what, expr);
        }
        let Some(result) = self.host_result(expr) else {
            return self.dead(expr);
        };
        let op = self.pool.host_op(HostOp {
            module: module.into(),
            operation: operation.into(),
            result,
        });

        let mut held = Vec::with_capacity(args.len());
        for arg in args {
            let value = self.expr(&arg.value);
            let value = self.describe_itself(value, arg.value.span);
            held.push(value);
        }
        let slots: Vec<Slot> = held.iter().map(|value| value.slot).collect();
        let list = self.pool.args.intern(slots);
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

    /// The layout a host operation's answer is written into.
    ///
    /// It is the schema's result type, read where the checker recorded it
    /// rather than out of the schema a second time: the checker resolved the
    /// operation against the schemas this compilation was given, which
    /// includes an embedder's, and re-reading only the shipped ones would
    /// answer for fewer programs than the checker did.
    ///
    /// The one type that has no layout is the one a schema declared `Any`,
    /// which the checker records as an unconstrained unknown. That is
    /// erasure rather than abstention — `docs/LINEAR_VM.md` separates the
    /// two — and erasure is a box. Nothing else at a host call site is
    /// unconstrained, because a host schema has no type parameters to leave
    /// open.
    fn host_result(&mut self, expr: &Expr) -> Option<LayoutId> {
        let ty = self.owned_ty(expr)?;
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
            Place::Field { obj, at, .. } => {
                let dst = self.temp(shapes::ADDR);
                self.emit(
                    Inst::AddrOfField {
                        dst: dst.slot,
                        obj,
                        at,
                    },
                    place.span,
                );
                dst
            }
            // A `var` parameter passed straight on is the same address: the
            // callee writes the storage the original caller named, which is
            // what makes the alias reach through however many frames pass it
            // along.
            Place::Through { addr, at: 0, .. } => Val::borrowed(addr, shapes::ADDR),
            Place::Through { .. } => self.gap(
                "a `var` argument naming a field of another `var` parameter, \
                 which would need an address an instruction can offset",
                place,
            ),
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

fn compare_of(repr: Repr) -> Compare {
    match repr {
        Repr::Float => Compare::Float,
        Repr::Bool => Compare::Bool,
        Repr::Ref => Compare::Str,
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
