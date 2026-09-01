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
//!
//! # A reference's live range ends where the expression does
//!
//! Every consumer of a value calls [`Body::release`] rather than freeing the
//! slot behind it, and that is where [`Inst::Clear`] comes from. A static
//! reference map says which slots a collection *reads*; only the data can
//! say when the value in one stopped being needed, so a temporary that held
//! an object holds null from its last use onwards. Without it a body that
//! built one string per turn would retain every one of them until it
//! returned.

use cove_diag::Span;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, BinaryOp, Block, Expr, ExprKind, StrPart, Type, UnaryOp};

use super::frame::Val;
use super::gap;
use super::shapes;
use super::{Body, Loop, PENDING};
use crate::inst::{ArithOp, CmpOp, Compare, Inst, Len, Num, Slot};
use crate::program::{Builtin, HostOp};
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
            ExprKind::Str(parts) => self.string_expr(expr, parts),
            ExprKind::Ident(name) => self.name(expr, name),
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
            ExprKind::Match { scrutinee, arms } => {
                let repr = self.word(expr);
                let dst = self.frame.alloc(repr);
                self.match_expr(scrutinee, arms, span, Some(dst));
                Val::temp(dst)
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

            ExprKind::ArrayLit(_) => self.gap("an array literal", expr),
            ExprKind::Await(_) => self.gap("`await`", expr),
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

    /// A name: a local, a parameter, or the one case of an enum that is
    /// written as a bare word.
    fn name(&mut self, expr: &Expr, name: &str) -> Val {
        if let Some(slot) = self.frame.lookup(name) {
            // A `var` parameter names the caller's storage rather than
            // holding a value, so reading it is a read *through* the word.
            // Nothing else in the frame is an `Addr`, which is what makes
            // the slot's own kind enough to tell them apart.
            if self.frame.repr(slot) == Repr::Addr {
                let repr = self.word(expr);
                let dst = self.frame.alloc(repr);
                self.emit(Inst::Load { dst, addr: slot }, expr.span);
                return Val::temp(dst);
            }
            return Val::borrowed(slot);
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
    /// An interpolation is where a word has to become text, and that is not
    /// something the instruction set should grow a case for — what `{x}`
    /// puts in a string is a rule of the language, stated in the language
    /// reference and not in the IR. So the whole literal becomes one
    /// [`Inst::CallBuiltin`].
    ///
    /// # What the builtin must do
    ///
    /// `String.interpolate` takes any number of operands and answers one new
    /// `String`: each operand rendered as `Display for Value` renders it,
    /// joined in order. The machine reads each operand's [`Repr`] out of the
    /// frame, which is why there is one builtin rather than one per kind of
    /// word — the lowering has nothing to add that the slot table does not
    /// already say.
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
            let dst = self.frame.alloc(Repr::Ref);
            self.emit(Inst::Str { dst, text: id }, expr.span);
            return Val::temp(dst);
        }

        let mut pieces: Vec<Val> = Vec::with_capacity(parts.len());
        for part in parts {
            match part {
                StrPart::Text(literal) if literal.is_empty() => {}
                StrPart::Text(literal) => {
                    let id = self.string(literal);
                    let dst = self.frame.alloc(Repr::Ref);
                    self.emit(Inst::Str { dst, text: id }, expr.span);
                    pieces.push(Val::temp(dst));
                }
                StrPart::Interpolation(inner) => pieces.push(self.expr(inner)),
            }
        }

        let slots: Vec<Slot> = pieces.iter().map(|piece| piece.slot).collect();
        let args = self.pool.args.intern(slots);
        let builtin = self.pool.builtin(Builtin {
            receiver: "String".into(),
            operation: "interpolate".into(),
            result: Repr::Ref,
        });
        let dst = self.frame.alloc(Repr::Ref);
        self.emit(Inst::CallBuiltin { dst, builtin, args }, expr.span);
        for piece in pieces.into_iter().rev() {
            self.release(piece, expr.span);
        }
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
        self.release(a, expr.span);
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
            None if operand == Repr::Ref => {
                // A string compares by its bytes. Every other object the
                // language lets `==` see would compare by walking its
                // fields, which is a builtin this lowering has not been
                // taught; the instruction is emitted anyway so the listing
                // stays well formed, and the gap is what stops the program.
                if !matches!(self.ty(lhs), Some(Ty::Str)) {
                    self.errors
                        .push(gap::gap("a comparison of two heap values", expr.span));
                }
                Inst::Cmp {
                    on: Compare::Str,
                    op: cmp_of(op),
                    dst,
                    a: a.slot,
                    b: b.slot,
                }
            }
            None => Inst::Cmp {
                on: compare_of(operand),
                op: cmp_of(op),
                dst,
                a: a.slot,
                b: b.slot,
            },
        };
        self.emit(inst, expr.span);
        self.release(b, expr.span);
        self.release(a, expr.span);
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
        self.release(a, lhs.span);

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
        self.release(b, rhs.span);

        let end = self.here();
        match skip {
            Some(skip) => self.patch(skip, end),
            None => self.patch(branch, end),
        }
        Val::temp(dst)
    }

    // ---- assignment -------------------------------------------------------

    fn assign(&mut self, op: Option<BinaryOp>, target: &Expr, value: &Expr, span: Span) {
        match &target.kind {
            ExprKind::Ident(name) => self.assign_to_name(op, name, value, span),
            ExprKind::Field { base, name } => {
                self.assign_to_field(op, base, &name.node, value, span)
            }
            _ => {
                self.errors
                    .push(gap::gap("an assignment to an element", span));
                self.discard(value);
            }
        }
    }

    /// `name = value`, and its compound forms.
    fn assign_to_name(&mut self, op: Option<BinaryOp>, name: &str, value: &Expr, span: Span) {
        let Some(slot) = self.frame.lookup(name) else {
            self.errors.push(gap::gap(
                "an assignment to something outside the frame",
                span,
            ));
            self.discard(value);
            return;
        };
        // A `var` parameter is an alias for the caller's storage, so an
        // assignment to it is a write *through* the word. That is the whole
        // of what `var` means at run time, and it needs no copy back.
        if self.frame.repr(slot) == Repr::Addr {
            let repr = self.word(value);
            let source = self.compound(op, value, span, |body| {
                let dst = body.frame.alloc(repr);
                body.emit(Inst::Load { dst, addr: slot }, span);
                Val::temp(dst)
            });
            self.emit(
                Inst::Store {
                    addr: slot,
                    src: source.slot,
                },
                span,
            );
            self.release(source, span);
            return;
        }

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
        self.release(source, span);
    }

    /// `place.field = value`, and its compound forms.
    ///
    /// The object is evaluated first and held until the store is emitted,
    /// which is both the language's order — the place, then the value — and
    /// what keeps the object reachable while the value that is going into it
    /// is being built.
    fn assign_to_field(
        &mut self,
        op: Option<BinaryOp>,
        base: &Expr,
        name: &str,
        value: &Expr,
        span: Span,
    ) {
        let Some((obj, at, repr)) = self.place_of_field(base, name, span) else {
            self.discard(value);
            return;
        };
        let source = self.compound(op, value, span, |body| {
            let dst = body.frame.alloc(repr);
            body.emit(
                Inst::GetWord {
                    dst,
                    obj: obj.slot,
                    at,
                },
                span,
            );
            Val::temp(dst)
        });
        self.emit(
            Inst::SetWord {
                obj: obj.slot,
                at,
                src: source.slot,
            },
            span,
        );
        self.release(source, span);
        self.release(obj, span);
    }

    /// The value an assignment stores: the right-hand side, or the operator
    /// applied to what is already there.
    ///
    /// `current` reads the place, and it is called only for a compound
    /// assignment, so a plain `=` never emits a read of what it is about to
    /// overwrite.
    fn compound(
        &mut self,
        op: Option<BinaryOp>,
        value: &Expr,
        span: Span,
        current: impl FnOnce(&mut Self) -> Val,
    ) -> Val {
        let Some(op) = op else {
            return self.expr(value);
        };
        let Some(arith) = arith_of(op) else {
            self.errors
                .push(gap::gap("a compound assignment with this operator", span));
            return self.expr(value);
        };
        let held = current(self);
        let source = self.expr(value);
        let num = num_of(self.frame.repr(held.slot));
        let dst = self.frame.alloc(self.frame.repr(held.slot));
        self.emit(
            Inst::Arith {
                num,
                op: arith,
                dst,
                a: held.slot,
                b: source.slot,
            },
            span,
        );
        self.release(source, span);
        self.release(held, span);
        Val::temp(dst)
    }

    // ---- structs and enums -------------------------------------------------

    /// `base.name`, where `base` is a value: one word out of an object.
    ///
    /// Where `base` is not a value it is a namespace — an enum's own name,
    /// a host module — and the only one of those this lowering has been
    /// taught is the enum, whose cases are what `E.Case` names.
    fn field(&mut self, expr: &Expr, base: &Expr, name: &str) -> Val {
        if self.is_namespace(base) {
            if let Some(ty) = self.ty(expr).cloned() {
                if shapes::case_at(self.checked, self.module, &ty, name).is_some() {
                    return self.enum_case(expr, &ty, name, &[]);
                }
            }
            return self.gap("a name reached through a module", expr);
        }
        let Some(base_ty) = self.owned_ty(base) else {
            return self.dead(expr);
        };
        let Some((at, _)) = shapes::field_at(self.checked, self.module, &base_ty, name) else {
            self.errors.push(super::describe(&base_ty, base.span));
            return self.dead(expr);
        };
        let obj = self.expr(base);
        let repr = self.word(expr);
        let dst = self.frame.alloc(repr);
        self.emit(
            Inst::GetWord {
                dst,
                obj: obj.slot,
                at,
            },
            expr.span,
        );
        self.release(obj, expr.span);
        Val::temp(dst)
    }

    /// The object and the word an assignable field names.
    fn place_of_field(&mut self, base: &Expr, name: &str, span: Span) -> Option<(Val, u32, Repr)> {
        let base_ty = self.owned_ty(base)?;
        let Some((at, field_ty)) = shapes::field_at(self.checked, self.module, &base_ty, name)
        else {
            self.errors.push(super::describe(&base_ty, span));
            return None;
        };
        let Some(repr) = shapes::word_of(&field_ty) else {
            self.errors.push(super::describe(&field_ty, span));
            return None;
        };
        Some((self.expr(base), at, repr))
    }

    /// `Point(x: 1, y: 2)`: an object, then one store per field.
    ///
    /// The payload is zeroed by [`Inst::Alloc`], so a reference field of a
    /// half-built object is null rather than garbage if a collection happens
    /// between the allocation and the store that fills it.
    fn struct_literal(&mut self, expr: &Expr, ty: &Ty, args: &[Arg]) -> Val {
        let Some(fields) = shapes::struct_fields(self.checked, self.module, ty) else {
            self.errors.push(super::describe(ty, expr.span));
            return self.dead(expr);
        };
        let Some(layout) = self.layout(ty, expr.span) else {
            return self.dead(expr);
        };
        let Some(order) = self.labelled(args, &fields, expr) else {
            return self.dead(expr);
        };

        // Every field is evaluated before anything is stored, in source
        // order, because an initializer's arguments are ordinary
        // expressions and one of them may do something the next one sees.
        let mut held = Vec::with_capacity(args.len());
        for arg in args {
            held.push(self.expr(&arg.value));
        }
        let dst = self.frame.alloc(Repr::Ref);
        self.emit(
            Inst::Alloc {
                dst,
                layout,
                len: Len::Fixed,
            },
            expr.span,
        );
        for (value, at) in held.iter().zip(&order) {
            self.emit(
                Inst::SetWord {
                    obj: dst,
                    at: *at,
                    src: value.slot,
                },
                expr.span,
            );
        }
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        Val::temp(dst)
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

    /// One case of an enum: the case index, then the payload.
    ///
    /// Word 0 is the index, and words `1..` are that case's payload. The
    /// object is sized for the widest case, so which of its words are
    /// references depends on the case it is in — a fact about the object,
    /// which the collector answers by reading word 0.
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

        let mut held = Vec::with_capacity(args.len());
        for arg in args {
            held.push(self.expr(&arg.value));
        }
        let dst = self.frame.alloc(Repr::Ref);
        self.emit(
            Inst::Alloc {
                dst,
                layout,
                len: Len::Fixed,
            },
            expr.span,
        );
        let tag = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::Int {
                dst: tag,
                value: index as i64,
            },
            expr.span,
        );
        self.emit(
            Inst::SetWord {
                obj: dst,
                at: 0,
                src: tag,
            },
            expr.span,
        );
        self.frame.free(tag);
        for (at, value) in held.iter().enumerate() {
            self.emit(
                Inst::SetWord {
                    obj: dst,
                    at: 1 + at as u32,
                    src: value.slot,
                },
                expr.span,
            );
        }
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        Val::temp(dst)
    }

    /// Builds the enclosing function's own `Err` or `None`, for `?` to
    /// leave through.
    ///
    /// It is built here rather than passed along: the value `?` was applied
    /// to is a `Result` of some other pair of types, and two `Result`s whose
    /// words differ are two layouts. Reusing the object would hand the
    /// caller one whose header names the wrong one.
    pub(super) fn failure(&mut self, payload: Option<Slot>, span: Span) -> Option<Slot> {
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
        let dst = self.frame.alloc(Repr::Ref);
        self.emit(
            Inst::Alloc {
                dst,
                layout,
                len: Len::Fixed,
            },
            span,
        );
        let tag = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::Int {
                dst: tag,
                value: index as i64,
            },
            span,
        );
        self.emit(
            Inst::SetWord {
                obj: dst,
                at: 0,
                src: tag,
            },
            span,
        );
        self.frame.free(tag);
        if let Some(src) = payload {
            self.emit(
                Inst::SetWord {
                    obj: dst,
                    at: 1,
                    src,
                },
                span,
            );
        }
        Some(dst)
    }

    // ---- `?` ---------------------------------------------------------------

    /// `expr?`: the payload of the succeeding case, or the enclosing
    /// function's own failure.
    ///
    /// The case index is one word of the object, so the question is a read
    /// and a comparison. `Ok` is case 0 of a `Result` and `Some` is case 1
    /// of an `Option`, which is why the index being tested is looked up
    /// rather than written down.
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
        let tag = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::GetWord {
                dst: tag,
                obj: subject.slot,
                at: 0,
            },
            expr.span,
        );
        let wanted = self.frame.alloc(Repr::Int);
        self.emit(
            Inst::Int {
                dst: wanted,
                value: index as i64,
            },
            expr.span,
        );
        let ok = self.frame.alloc(Repr::Bool);
        self.emit(
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Eq,
                dst: ok,
                a: tag,
                b: wanted,
            },
            expr.span,
        );
        self.frame.free(wanted);
        self.frame.free(tag);
        let branch = self.emit(
            Inst::BranchFalse {
                cond: ok,
                to: PENDING,
            },
            expr.span,
        );
        self.frame.free(ok);

        let repr = self.word(expr);
        let dst = self.frame.alloc(repr);
        self.emit(
            Inst::GetWord {
                dst,
                obj: subject.slot,
                at: 1,
            },
            expr.span,
        );
        let carry_on = self.emit(Inst::Jump { to: PENDING }, expr.span);

        let failing = self.here();
        self.patch(branch, failing);
        // The failure carries the payload of the case it found, which for a
        // `Result` is the error and for an `Option` is nothing at all. The
        // frame ends at the `Return`, so nothing here is cleared: a slot
        // whose frame is gone retains nothing.
        let payload = match &ty {
            Ty::Result(_, err) => {
                let word = shapes::word_of(err).unwrap_or(Repr::Unit);
                let held = self.frame.alloc(word);
                self.emit(
                    Inst::GetWord {
                        dst: held,
                        obj: subject.slot,
                        at: 1,
                    },
                    expr.span,
                );
                Some(held)
            }
            _ => None,
        };
        if let Some(answer) = self.failure(payload, expr.span) {
            self.emit(Inst::Return { src: answer }, expr.span);
            self.frame.free(answer);
        }
        if let Some(payload) = payload {
            self.frame.free(payload);
        }

        let rest = self.here();
        self.patch(carry_on, rest);
        self.release(subject, expr.span);
        Val::temp(dst)
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
                match dst {
                    Some(dst) => {
                        let value = self.expr(otherwise);
                        self.store(dst, &value, otherwise);
                        self.release(value, otherwise.span);
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
        self.release(cond, condition.span);

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
                // The frame ends at the `Return`, so the slot is given back
                // without a `Clear`: there is nothing left to retain it.
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
            ExprKind::Field { base, name } if self.is_namespace(base) => {
                self.call_qualified(expr, base, &name.node, args)
            }
            ExprKind::Ident(_) => self.gap("a call through a function value", expr),
            ExprKind::Field { .. } => self.gap("a method call", expr),
            _ => self.gap("a call to something other than a declared function", expr),
        }
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
        }
        self.gap("a call to a method or an associated function", expr)
    }

    /// A call to a declared function of this package.
    fn call_declared(&mut self, expr: &Expr, id: crate::FunctionId, args: &[Arg]) -> Val {
        let Some((arity, returns)) = self.plan.boundary(id) else {
            // The declaration itself is a gap, already reported where it is
            // written. Saying so again at every call site would bury it.
            return self.dead(expr);
        };
        for arg in args {
            let what = if arg.label.is_some() {
                "a labelled argument"
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
        let mut bases = Vec::new();
        for arg in args {
            if arg.is_var {
                let (address, base) = self.address_of(&arg.value);
                held.push(address);
                bases.extend(base);
            } else {
                held.push(self.expr(&arg.value));
            }
        }
        let slots: Vec<Slot> = held.iter().map(|value| value.slot).collect();
        let list = self.pool.args.intern(slots);
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
            self.release(value, expr.span);
        }
        // The object an interior address points into is kept alive by this
        // slot for exactly the address's live range, and dies with it. The
        // heap does not move, so the address stays correct across a
        // collection for as long as it is live and no longer.
        for base in bases.into_iter().rev() {
            self.release(base, expr.span);
        }
        Val::temp(dst)
    }

    /// A call across the boundary.
    ///
    /// # What the runtime must implement
    ///
    /// A [`HostOp`] is the module and operation as the source writes them —
    /// `console`.`println`, `files`.`read` — and the boundary looks the pair
    /// up in the registry exactly as the interpreter does. The arguments are
    /// the slots in source order, materialised into public `Value`s by the
    /// kind each slot's [`Repr`] declares; a variadic operation is passed
    /// its arguments one per slot and the boundary is what collects them.
    ///
    /// [`HostOp::result`] is the word the host's answer is written back
    /// into, and it is the schema's declared result type as the checker
    /// read it. A schema that declared `Any` gives [`Repr::Ref`]: from that
    /// call onwards the program holds a value no schema described, so it is
    /// a box carrying its own tag rather than a bare word.
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
            held.push(self.expr(&arg.value));
        }
        let slots: Vec<Slot> = held.iter().map(|value| value.slot).collect();
        let list = self.pool.args.intern(slots);
        let dst = self.frame.alloc(result);
        self.emit(
            Inst::CallHost {
                dst,
                op,
                args: list,
            },
            expr.span,
        );
        for value in held.into_iter().rev() {
            self.release(value, expr.span);
        }
        Val::temp(dst)
    }

    /// The word a host operation's answer is written into.
    ///
    /// It is the schema's result type, read where the checker recorded it
    /// rather than out of the schema a second time: the checker resolved the
    /// operation against the schemas this compilation was given, which
    /// includes an embedder's, and re-reading only the shipped ones would
    /// answer for fewer programs than the checker did.
    ///
    /// The one type that has no word is the one a schema declared `Any`,
    /// which the checker records as an unconstrained unknown. That is
    /// erasure rather than abstention — `docs/LINEAR_VM.md` separates the
    /// two — and erasure is a `Ref` to a box. Nothing else at a host call
    /// site is unconstrained, because a host schema has no type parameters
    /// to leave open.
    fn host_result(&mut self, expr: &Expr) -> Option<Repr> {
        let ty = self.owned_ty(expr)?;
        if matches!(ty, Ty::Unknown(cove_sema::typeck::Unknown::Unconstrained)) {
            return Some(Repr::Ref);
        }
        match shapes::word_of(&ty) {
            Some(repr) => Some(repr),
            None => {
                self.errors.push(super::describe(&ty, expr.span));
                None
            }
        }
    }

    /// The address a `var` argument passes, and the object that address
    /// points into if it points into one.
    ///
    /// A place is one word. `var total` is the address of a slot of this
    /// frame; `var point.x` is the address of a word of an object, and the
    /// object has to be held in a reference slot for as long as the address
    /// can be used — which the caller does by releasing the second answer
    /// after the call.
    fn address_of(&mut self, place: &Expr) -> (Val, Option<Val>) {
        match &place.kind {
            ExprKind::Ident(name) => match self.frame.lookup(name) {
                // A `var` parameter is already an address, and passing it on
                // is passing the same one: the callee writes the storage the
                // original caller named, which is what makes the alias reach
                // through however many frames pass it along.
                Some(slot) if self.frame.repr(slot) == Repr::Addr => (Val::borrowed(slot), None),
                Some(slot) => {
                    let dst = self.frame.alloc(Repr::Addr);
                    self.emit(Inst::AddrOfSlot { dst, slot }, place.span);
                    (Val::temp(dst), None)
                }
                None => (
                    self.gap("a `var` argument naming something outside the frame", place),
                    None,
                ),
            },
            ExprKind::Field { base, name } => {
                let Some((obj, at, _)) = self.place_of_field(base, &name.node, place.span) else {
                    return (self.dead(place), None);
                };
                let dst = self.frame.alloc(Repr::Addr);
                self.emit(
                    Inst::AddrOfWord {
                        dst,
                        obj: obj.slot,
                        at,
                    },
                    place.span,
                );
                (Val::temp(dst), Some(obj))
            }
            _ => (
                self.gap("a `var` argument that is not a place", place),
                None,
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
