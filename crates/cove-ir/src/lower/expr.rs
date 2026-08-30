//! Statements, expressions, control flow and `match`: the constructs whose
//! lowering their own shape decides.
//!
//! [`Position`] is the argument every one of them threads. An expression
//! lowered for its value leaves one operand and one lowered for its effect
//! leaves none, and the constructs with an inside — a block, an `if`/`else`
//! and a `match` — hand the position down to their tails, branches and arms
//! rather than taking it themselves, so the saving is taken where the value
//! would have been built rather than where it would have been thrown away.
//!
//! What a call means is not decided here: `super::call` and
//! `super::dispatch` are which function a call reaches, and `super::task`
//! is what a `scope` and a `spawn` are.

use cove_diag::Span;
use cove_schema::builtins;
use cove_syntax::ast::{
    BinaryOp as SourceBinary, Block, Expr, ExprKind, ItemKind, MatchArm, Param, Pattern,
    PatternKind, Stmt, StmtKind, StrPart, UnaryOp as SourceUnary,
};

use crate::{BinaryOp, Const, Inst, Scalar, SlotKind, UnaryOp, Unsupported};

use super::body::{binary_op, branch_on, int_result, Body, Depth, LoopFrame, Position};
use super::call::Args;
use super::convention::store_slot;
use super::index::{dyn_shape, reject_dyn, Instance, LambdaSite};
use super::scan::mentioned_names;

/// Which kind of `for` header a loop is walking.
#[derive(Clone, Copy)]
enum Header {
    /// `a..b` and `a..<b`: the cursor is the value the binding takes, and
    /// `limit` is the bound it is tested against.
    Range { limit: u32, inclusive: bool },
    /// Anything else: the cursor is an index into `sequence`, whose length
    /// was read once into `length`.
    Sequence { sequence: u32, length: u32 },
}

impl<'a, 'l> Body<'a, 'l> {
    // ---------------------------------------------------------- statements

    /// A block, lowered in the position it was written in.
    ///
    /// A block's value is its tail's, so the position is handed to the tail:
    /// lowered for effect a block builds no `Unit` at all, and lowered in
    /// scalar position its tail leaves its value on the scalar stack. Its
    /// statements are unaffected — they were already lowered for their
    /// effect, whichever position the block itself is in.
    ///
    /// The slots the block declared are released at its end, so a later
    /// sibling block reuses the numbers and each frame size stays a
    /// high-water mark rather than a total.
    pub(super) fn block_at(
        &mut self,
        block: &'a Block,
        position: Position,
    ) -> Result<(), Unsupported> {
        let mark = self.scope();
        for statement in &block.statements {
            self.statement(statement)?;
        }
        match &block.tail {
            Some(tail) => self.expr_at(tail, position)?,
            None => self.unit_at(position, block.span),
        }
        self.release(mark);
        Ok(())
    }

    fn statement(&mut self, statement: &'a Stmt) -> Result<(), Unsupported> {
        match &statement.kind {
            StmtKind::Let {
                name, ty, value, ..
            } => {
                if let Some(ty) = ty {
                    reject_dyn(ty, "a `dyn` binding")?;
                }
                // The value is lowered before the name exists, which is what
                // makes `let x = x` read the outer `x`.
                //
                // Where the binding lives is settled by the same fact every
                // typed instruction is settled by: the type the checker gave
                // what it is declared from. An abstention keeps the slot a
                // `Value`, and the whole function then reads as it always did.
                //
                // An annotation that converts settles it instead: what the
                // binding holds is the trait object the conversion makes,
                // which is a `Value` whatever the value it was declared from
                // was.
                let converts = ty.as_ref().is_some_and(|ty| dyn_shape(ty).is_some());
                let kind = match converts {
                    true => SlotKind::Value,
                    false => self.slot_kind(value),
                };
                match kind {
                    SlotKind::Scalar(_) => self.expr_scalar(value)?,
                    SlotKind::Value => self.expr(value)?,
                    // `slot_kind` answers about a type and never says
                    // `Place`.
                    SlotKind::Place => unreachable!("a `let` does not declare a place"),
                }
                if let Some(ty) = ty {
                    // `eval_block_body` converts the value against the
                    // annotation before it declares the name, so this stands
                    // between the value and the store the same way.
                    let module = self.module;
                    self.coerce_to(module, ty, statement.span);
                }
                let slot = self.declare(Some(name.node.as_str()), kind);
                self.emit(store_slot(kind, slot), statement.span);
                Ok(())
            }
            StmtKind::Expr(expr) => {
                // A statement is the one place a value is definitely
                // unwanted, so it is where lowering for effect starts.
                self.effect(expr)?;
                Ok(())
            }
            StmtKind::Item(item) => Err(Unsupported::new(
                match item.kind {
                    ItemKind::Fn(_) => "a function declared inside a function body",
                    _ => "a type declared inside a function body",
                },
                statement.span,
            )),
        }
    }

    // --------------------------------------------------------- expressions

    /// Lowers one expression, which leaves exactly one value on the stack.
    pub(super) fn expr(&mut self, expr: &'a Expr) -> Result<(), Unsupported> {
        self.expr_at(expr, Position::Value)
    }

    /// Lowers one expression whose value nobody reads, which leaves nothing
    /// on the stack.
    ///
    /// Everything the expression does still happens; only its value goes
    /// missing. See `Position` for why that is worth a second entry point.
    pub(super) fn effect(&mut self, expr: &'a Expr) -> Result<(), Unsupported> {
        self.expr_at(expr, Position::Effect)
    }

    /// Lowers one expression so that what it computed is on the scalar
    /// stack.
    ///
    /// Called only where [`Body::scalar_of`] settled a type, so what arrives
    /// is what the instruction reading it was promised. An expression the
    /// scalar stack has no instructions for is lowered exactly as it always
    /// was and moved across by one [`Inst::ValueToScalar`] — a boundary
    /// rather than a second lowering of the language.
    ///
    /// The three constructs with an inside are not moved across: a block, an
    /// `if`/`else`, and a `match` hand [`Position::Scalar`] to their tails,
    /// branches, and arms, so that an integer is left where an integer was
    /// wanted rather than built as a `Value` in each branch and unwrapped
    /// again afterwards. That is the same reasoning [`Position::Effect`]
    /// reaches inside them for.
    pub(super) fn expr_scalar(&mut self, expr: &'a Expr) -> Result<(), Unsupported> {
        let span = expr.span;
        match &expr.kind {
            ExprKind::Int(value) if self.scalar_of(expr) == Some(Scalar::Int) => {
                self.emit(Inst::ScalarConst(*value), span);
            }
            ExprKind::Bool(value) if self.scalar_of(expr) == Some(Scalar::Bool) => {
                self.emit(Inst::ScalarConst(i64::from(*value)), span);
            }
            ExprKind::Ident(name) => match self.scalar_binding(name) {
                Some((slot, _)) => self.emit(Inst::LoadScalar(slot), span),
                None => return self.moved_to_scalar(expr),
            },
            ExprKind::Binary { op, lhs, rhs } => {
                // `&&`/`||` wanted as a scalar: the scalar form costs
                // `2 - k` boundaries where `k` operands are already on the
                // scalar stack, the value form costs `k + 1` (one per
                // already-scalar operand, plus one to move the answer
                // across), so the scalar form wins as soon as `k >= 1`.
                if matches!(op, SourceBinary::And | SourceBinary::Or)
                    && self.scalar_of(expr) == Some(Scalar::Bool)
                    && (self.on_scalar_stack(lhs) || self.on_scalar_stack(rhs))
                {
                    return self.and_or_scalar(*op, lhs, rhs, span);
                }
                let inst = binary_op(*op).map(|op| self.binary_inst(op, lhs, rhs));
                let Some(inst @ Inst::IntBinary(_)) = inst else {
                    return self.moved_to_scalar(expr);
                };
                // `binary_inst` answered `IntBinary` only because the checker
                // settled both operands as `Int`, so both hold this
                // function's precondition and neither needs asking again.
                self.expr_scalar(lhs)?;
                self.expr_scalar(rhs)?;
                self.emit(inst, span);
            }
            ExprKind::Call {
                callee,
                generics: _,
                args,
                trailing,
            } => {
                // Deliberately not through `moved_to_scalar`: a call to a
                // function whose answer already arrives on the scalar stack
                // would be moved off it and straight back on again, which is
                // the pair of instructions this whole convention exists to
                // stop emitting. Only a call that landed on the value stack
                // crosses.
                if self
                    .call(expr.id, callee, args, trailing.as_deref(), span)?
                    .is_none()
                {
                    self.emit(Inst::ValueToScalar, span);
                }
            }
            ExprKind::Block(_) | ExprKind::Match { .. } => {
                return self.expr_at(expr, Position::Scalar)
            }
            // An `if` with no `else` answers `()`, which the scalar stack
            // does not hold, so only the two-branch form takes the position.
            ExprKind::If { else_branch, .. } if else_branch.is_some() => {
                return self.expr_at(expr, Position::Scalar)
            }
            // `Inst::GetFieldAtScalar` where the receiver's position and the
            // field's own type are both settled — see `Body::scalar_field`.
            // Anything else falls to `moved_to_scalar`, exactly where
            // `Inst::GetFieldAt` is not emitted either.
            ExprKind::Field { base, name } => match self.scalar_field(expr, base, &name.node) {
                Some(index) => {
                    self.expr(base)?;
                    self.emit(Inst::GetFieldAtScalar(index), span);
                }
                None => return self.moved_to_scalar(expr),
            },
            _ => return self.moved_to_scalar(expr),
        }
        Ok(())
    }

    /// Lowers one expression the way it has always been lowered, and moves
    /// what it produced onto the scalar stack.
    fn moved_to_scalar(&mut self, expr: &'a Expr) -> Result<(), Unsupported> {
        self.expr(expr)?;
        self.emit(Inst::ValueToScalar, expr.span);
        Ok(())
    }

    /// Lowers a condition and answers whether it left its `Bool` on the
    /// scalar stack.
    fn condition(&mut self, condition: &'a Expr) -> Result<bool, Unsupported> {
        if self.scalar_of(condition) == Some(Scalar::Bool) && self.on_scalar_stack(condition) {
            self.expr_scalar(condition)?;
            return Ok(true);
        }
        self.expr(condition)?;
        Ok(false)
    }

    /// Lowers one expression in the position it was written in.
    ///
    /// Six constructs take the position themselves, because each of them
    /// either builds its `Unit` here — an assignment, a `while`, a `for`, an
    /// `if` with no `else` — or has an inside the position should reach: an
    /// `if`/`else`, a `Block`, and a `Match` hand it to each branch, tail,
    /// and arm. Everything else answers a value it computed, and the only
    /// honest way to want nothing from it is to take that value off again,
    /// which is the `Pop` below.
    fn expr_at(&mut self, expr: &'a Expr, position: Position) -> Result<(), Unsupported> {
        let span = expr.span;
        // The scalar position reaches only the three constructs with an
        // inside; everything else is a leaf, and a leaf's scalar lowering is
        // [`Body::expr_scalar`]'s rather than a second copy of it here.
        if position == Position::Scalar
            && !matches!(
                expr.kind,
                ExprKind::Block(_)
                    | ExprKind::Match { .. }
                    | ExprKind::If {
                        else_branch: Some(_),
                        ..
                    }
            )
        {
            return self.expr_scalar(expr);
        }
        match &expr.kind {
            ExprKind::Int(value) => self.constant(Const::Int(*value), span),
            ExprKind::Float(value) => self.constant(Const::Float(*value), span),
            ExprKind::Bool(value) => self.constant(Const::Bool(*value), span),
            ExprKind::Duration(value) => self.constant(Const::Duration(*value), span),
            ExprKind::Unit => self.constant(Const::Unit, span),
            ExprKind::Str(parts) => self.string(parts, span)?,
            ExprKind::Ident(name) => self.ident(name, span)?,
            ExprKind::ArrayLit(items) => {
                for item in items {
                    self.expr(item)?;
                }
                self.emit(Inst::MakeArray(items.len() as u32), span);
            }
            ExprKind::Field { base, name } => self.field(base, &name.node, span)?,
            ExprKind::Call {
                callee,
                generics: _,
                args,
                trailing,
            } => {
                // A call to a function whose return type the checker settled
                // leaves its answer on the scalar stack, so what a reader of
                // this position needs is on the other one: one boundary
                // instruction where a value is wanted, and the scalar
                // stack's own discard where nothing is.
                if let Some(what) = self.call(expr.id, callee, args, trailing.as_deref(), span)? {
                    if position == Position::Effect {
                        self.emit(Inst::ScalarPop, span);
                        return Ok(());
                    }
                    self.emit(Inst::ScalarToValue(what), span);
                }
            }
            ExprKind::Unary { op, operand } => {
                self.expr(operand)?;
                let op = match op {
                    SourceUnary::Not => UnaryOp::Not,
                    SourceUnary::Neg => UnaryOp::Neg,
                };
                self.emit(Inst::Unary(op), span);
            }
            ExprKind::Binary { op, lhs, rhs } => self.binary(expr, *op, lhs, rhs, span)?,
            ExprKind::Assign { op, target, value } => {
                return self.assign(*op, target, value, position, span)
            }
            ExprKind::Try(inner) => {
                self.expr(inner)?;
                self.emit(Inst::Try, span);
            }
            ExprKind::Block(block) => return self.block_at(block, position),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                return self.conditional(
                    condition,
                    then_branch,
                    else_branch.as_deref(),
                    position,
                    span,
                )
            }
            ExprKind::While { condition, body } => {
                return self.while_loop(condition, body, position, span)
            }
            ExprKind::For {
                binding,
                iterable,
                body,
            } => return self.for_loop(binding.node.as_str(), iterable, body, position, span),
            ExprKind::Return(value) => match (self.returns, value) {
                // Every return of a function leaves on the stack that
                // function's `returns` names, because a caller reads exactly
                // that one and nothing tells it which of two a given return
                // used.
                (SlotKind::Scalar(_), Some(value)) => {
                    self.expr_scalar(value)?;
                    self.emit(Inst::ReturnScalar, span);
                }
                // `return` with no value answers `()`, and no scalar stack
                // holds one. The checker compares a `return`'s operand
                // against the declared type, so a checked program whose
                // return type is `Int` or `Bool` has no such `return`;
                // lowering it as the untyped one rather than inventing a
                // scalar is what makes `validate` refuse the pair and say so
                // instead of the VM reading a word that was never written.
                (SlotKind::Scalar(_), None) | (SlotKind::Value, None) => {
                    self.constant(Const::Unit, span);
                    self.emit_dyn_return(span);
                    self.emit(Inst::Return, span);
                }
                (SlotKind::Value, Some(value)) => {
                    self.expr(value)?;
                    self.emit_dyn_return(span);
                    self.emit(Inst::Return, span);
                }
                // `slot_kind_of` never answers `Place` about a return type,
                // so no function's `returns` is one.
                (SlotKind::Place, _) => {
                    unreachable!("a function does not answer a place")
                }
            },
            ExprKind::Break(value) => {
                // The operand is evaluated for its effects and discarded: a
                // loop is `()` however it leaves, so there is nowhere for a
                // value to go.
                if let Some(value) = value {
                    self.effect(value)?;
                }
                self.leave_loop(true, span)?;
            }
            ExprKind::Continue => self.leave_loop(false, span)?,
            ExprKind::Range {
                start,
                end,
                inclusive_end,
            } => self.range(start, end, *inclusive_end, span)?,
            ExprKind::Lambda {
                is_async,
                params,
                body,
            } => self.lambda(expr, *is_async, params, body, span, false)?,
            ExprKind::Match { scrutinee, arms } => {
                return self.match_expr(scrutinee, arms, position, span)
            }
            ExprKind::Scope { name, body } => return self.scope_expr(name, body, position, span),
            ExprKind::Await(inner) => {
                self.expr(inner)?;
                self.emit(Inst::Await, span);
            }
        }
        if position == Position::Effect {
            // A value was computed and nothing reads it. Where control cannot
            // reach here — after a `return`, a `break`, or a `continue` —
            // `emit` writes nothing, so a diverging expression costs no `Pop`
            // either.
            self.emit(Inst::Pop, span);
        }
        Ok(())
    }

    /// A string literal, and the interpolations written inside it.
    ///
    /// A literal with nothing interpolated is one `Const::Str`: there is no
    /// rendering to do, so there is nothing for a `Concat` to do either.
    fn string(&mut self, parts: &'a [StrPart], span: Span) -> Result<(), Unsupported> {
        let interpolated = parts
            .iter()
            .any(|part| matches!(part, StrPart::Interpolation(_)));
        if !interpolated {
            let mut text = String::new();
            for part in parts {
                if let StrPart::Text(literal) = part {
                    text.push_str(literal);
                }
            }
            self.constant(Const::Str(text.into()), span);
            return Ok(());
        }
        for part in parts {
            match part {
                StrPart::Text(literal) => self.constant(Const::Str(literal.as_str().into()), span),
                StrPart::Interpolation(expr) => self.expr(expr)?,
            }
        }
        self.emit(Inst::Concat(parts.len() as u32), span);
        Ok(())
    }

    /// `a..b` and `a..<b`, built as the value it is.
    ///
    /// A range is an ordinary Cove value — `Interpreter::eval`'s
    /// `ExprKind::Range` arm evaluates one like any other expression, and
    /// says so — so it can be bound, passed, compared, rendered, and used as
    /// a `Map` key. [`Body::for_loop`] is the one place that never builds
    /// one: a `for` over a range walks between two bounds it keeps in hidden
    /// slots, so there is no `Range` in a loop at all, and that stays true.
    ///
    /// The bounds go onto the scalar stack, which is where the checker's own
    /// answer puts them: it checks each against `Ty::Int`, so
    /// [`Body::scalar_of`] settles both, and a settled `Int` operand belongs
    /// on that stack the way every other one does. Where it settled
    /// something else — which a checked program has no way to write, since
    /// the expectation is what makes a non-`Int` bound a diagnostic — this
    /// refuses rather than moving a `Value` across a boundary that promised
    /// an `Int` and was handed something else.
    fn range(
        &mut self,
        start: &'a Expr,
        end: &'a Expr,
        inclusive_end: bool,
        span: Span,
    ) -> Result<(), Unsupported> {
        if self.scalar_of(start) != Some(Scalar::Int) || self.scalar_of(end) != Some(Scalar::Int) {
            return Err(Unsupported::new(
                "a range whose bounds the checker did not settle as `Int`",
                span,
            ));
        }
        self.expr_scalar(start)?;
        self.expr_scalar(end)?;
        self.emit(Inst::MakeRange { inclusive_end }, span);
        Ok(())
    }

    /// A bare name.
    ///
    /// A local wins over everything else, which is what lets a `let http`
    /// shadow the host module of that name — and what leaves an `http.fetch`
    /// written above the `let` still reaching the host.
    pub(super) fn ident(&mut self, name: &str, span: Span) -> Result<(), Unsupported> {
        if let Some((slot, what)) = self.scalar_binding(name) {
            // A scalar slot read where a `Value` is wanted is the boundary in
            // the outward direction, and the instruction carries the tag the
            // word itself does not.
            self.emit(Inst::LoadScalar(slot), span);
            self.emit(Inst::ScalarToValue(what), span);
            return Ok(());
        }
        if let Some(binding) = self.binding(name) {
            let (slot, kind) = (binding.slot, binding.kind);
            match kind {
                // A `var` parameter's slot holds a place, not the value, so
                // reading the parameter is loading the place and reading
                // through it — which is where the caller's own storage is.
                SlotKind::Place => {
                    self.emit(Inst::LoadPlace(slot), span);
                    self.emit(Inst::PlaceRead, span);
                }
                // A capture reaches here too, and is not asked about: it is
                // a slot of this frame like any other, filled by the call
                // rather than by the body. `Function::captures` is where
                // that arrangement is written down.
                _ => self.emit(Inst::LoadLocal(slot), span),
            }
            return Ok(());
        }
        if name == builtins::NONE_CASE.name {
            // `None` is the one builtin case written as a bare name rather
            // than as a call, so it is built here rather than at a call.
            let none = self.outer.name(name);
            self.emit(
                Inst::MakeBuiltin {
                    name: none,
                    argc: 0,
                },
                span,
            );
            return Ok(());
        }
        if let Some(key) = self.outer.function_of(self.module, name) {
            // A closure over nothing. `Interpreter::eval_ident` builds one
            // with `captures: Vec::new()`, because a declaration reads no
            // environment — the whole of what makes a function a value is
            // that it can be called through one.
            //
            // The specialisation it names is not the one a direct call
            // reaches. A call through a value puts every argument on the
            // value stack and reads the answer off it, and a convention is
            // what a slot number means, so the body is lowered a second time
            // under that convention rather than called under a convention
            // nothing at the call site could have known.
            let params = self.outer.declaration(key).decl.params.len();
            let function = self.outer.number(Instance::Declared {
                key,
                supplied: vec![true; params],
                as_value: true,
            });
            self.emit(
                Inst::MakeClosure {
                    function,
                    captures: 0,
                },
                span,
            );
            return Ok(());
        }
        if self.outer.struct_of(self.module, name).is_some()
            || self.outer.declares_enum(self.module, name)
            || builtins::is_builtin_type(name)
        {
            return Err(Unsupported::new(
                format!("`{name}`, a type used as a value"),
                span,
            ));
        }
        if self.outer.imported_module(self.module, name).is_some()
            || self.outer.is_host_module(self.module, name)
            || self.outer.host_item(self.module, name).is_some()
        {
            return Err(Unsupported::new(
                format!("`{name}`, a module or a host operation used as a value"),
                span,
            ));
        }
        Err(Unsupported::new(
            format!("`{name}`, a name the lowering cannot resolve"),
            span,
        ))
    }

    /// `fn(x) { ... }`: a function of its own, and the values it is handed.
    ///
    /// Two things happen here and the order is the whole of the semantics.
    /// The captures are worked out first, from what this body has live and
    /// what the lambda's own body mentions, which is `Env::captures` asked
    /// at lowering time; then each of them is *read* onto the value stack,
    /// left to right, and [`Inst::MakeClosure`] pairs them with their names.
    ///
    /// Reading is the point. The oracle captures by value at creation time —
    /// a closure over a `var` binding still answers what the binding held
    /// when the lambda was written, after the binding has been assigned to —
    /// so a capture whose binding is a scalar slot crosses to the value
    /// stack here, and a capture whose binding is a `var` parameter is the
    /// value the place names rather than the place. See
    /// [`Inst::PlaceLocal`], where that second one is what keeps a place
    /// from outliving the frame that built it.
    ///
    /// A name the lambda mentions that this body has no binding for is not a
    /// capture at all: a declaration, a type, and a host module resolve in
    /// the module rather than in the environment, exactly as they do in the
    /// interpreter, where `Env::captures` only walks bindings.
    pub(super) fn lambda(
        &mut self,
        expr: &'a Expr,
        is_async: bool,
        params: &'a [Param],
        body: &'a Block,
        span: Span,
        aliases_first_param: bool,
    ) -> Result<(), Unsupported> {
        let mentioned = mentioned_names(body);
        // Outermost first, one entry per name, and the *latest* binding of a
        // name that is declared twice — which is `Env::captures`'s walk,
        // where a repeated name overwrites the value it recorded and keeps
        // the position it recorded it at.
        let mut captured: Vec<(&'a str, u32)> = Vec::new();
        for (at, binding) in self.live.iter().enumerate() {
            let Some(name) = binding.name else { continue };
            if !mentioned.contains(name) {
                continue;
            }
            match captured.iter_mut().find(|(held, _)| *held == name) {
                Some(slot) => slot.1 = at as u32,
                None => captured.push((name, at as u32)),
            }
        }
        if captured.len() >= u16::MAX as usize {
            // `Inst::MakeClosure` holds the count in a `u16` for the reason
            // `Inst::Call` holds its counts in one. Nothing writes a lambda
            // with this many free names; the check is what makes the width a
            // fact.
            return Err(Unsupported::new(
                "a closure with more than 65534 captures",
                span,
            ));
        }
        let names: Vec<&'a str> = captured.iter().map(|(name, _)| *name).collect();
        let mut capture_kinds: Vec<SlotKind> = Vec::with_capacity(captured.len());
        for (_, at) in &captured {
            let binding = &self.live[*at as usize];
            let (slot, kind) = (binding.slot, binding.kind);
            match kind {
                // The value the place names, not the place: `Env::captures`
                // calls `place.read`. What the callee gets is a value, so
                // that is the kind recorded for it.
                SlotKind::Place => {
                    self.emit(Inst::LoadPlace(slot), span);
                    self.emit(Inst::PlaceRead, span);
                    capture_kinds.push(SlotKind::Value);
                }
                // A capture travels as a `Value` whatever it will land in,
                // because a closure holds `(name, Value)` pairs on both
                // backends and a host reads them. The kind recorded here is
                // where the *call* will put it; see
                // `Function::captures`.
                SlotKind::Scalar(what) => {
                    self.emit(Inst::LoadScalar(slot), span);
                    self.emit(Inst::ScalarToValue(what), span);
                    capture_kinds.push(SlotKind::Scalar(what));
                }
                SlotKind::Value => {
                    self.emit(Inst::LoadLocal(slot), span);
                    capture_kinds.push(SlotKind::Value);
                }
            }
        }
        let count = names.len() as u16;
        let module = self.module;
        let function = self.outer.number_lambda(
            LambdaSite {
                module,
                params,
                body,
                span,
                captures: names,
                capture_kinds,
                is_async,
                aliases_first_param,
            },
            (span.file, expr.id),
        );
        self.emit(
            Inst::MakeClosure {
                function,
                captures: count,
            },
            span,
        );
        Ok(())
    }

    /// `base.name` written where a value is wanted.
    ///
    /// A head that is not a local may be a *name* rather than a value, and
    /// `Interpreter::eval_field` answers those before it evaluates anything:
    /// `Status.Confirmed` is a case of an enum, `console.println` is a host
    /// operation, and `booking.Status` is a declaration reached through the
    /// module that exports it. Only the first of the three has a lowering,
    /// and the other two are named rather than read as a field of a value
    /// they are not.
    fn field(&mut self, base: &'a Expr, name: &str, span: Span) -> Result<(), Unsupported> {
        // `http.Method.Get`: a case of an enum a host declares, reached
        // through the module that declares it. The head is a `Field` rather
        // than an `Ident` here, because two names stand between the module
        // and the case, and neither of them is a value: the interpreter
        // answers `http.Method` as a `Value::Type` and then reads the case
        // off it, so nothing this builds was ever a field of anything.
        if let ExprKind::Field {
            base: inner,
            name: type_name,
        } = &base.kind
        {
            if let ExprKind::Ident(head) = &inner.kind {
                if self.lookup(head).is_none() && self.outer.is_host_module(self.module, head) {
                    if let Some(declared) = cove_schema::hosts::module(head)
                        .and_then(|schema| schema.declared_type(&type_name.node))
                    {
                        // A schema's `cases` is empty for a struct, so this
                        // asks whether the type is an enum at all. Whether it
                        // declares *this* case is settled where the
                        // interpreter settles it, at run time, for the reason
                        // `Body::make_enum` gives.
                        if !declared.cases.is_empty() {
                            let ty = self.outer.name(&format!("{head}.{}", type_name.node));
                            let case = self.outer.name(name);
                            self.emit(Inst::MakeHostEnum { ty, case }, span);
                            return Ok(());
                        }
                    }
                }
            }
        }
        if let ExprKind::Ident(head) = &base.kind {
            if self.lookup(head).is_none() {
                if let Some((owner, _)) = self.outer.enum_of(self.module, head) {
                    // `Status.Confirmed`: a case written without a call, so
                    // its payload is empty. Whether the enum declares such a
                    // case is settled where the interpreter settles it — in
                    // `enum_case`, at run time — because a case that does not
                    // exist is a failure with a message rather than a shape
                    // the lowering could produce something else for.
                    return self.make_enum(owner, head, name, Args::new(&[], None), span);
                }
                if self.outer.is_host_module(self.module, head) {
                    return Err(Unsupported::new(
                        format!("`{head}.{name}`, a host operation used as a value"),
                        span,
                    ));
                }
                if self.outer.imported_module(self.module, head).is_some() {
                    return Err(Unsupported::new(
                        format!("`{head}.{name}`, a declaration named through its module"),
                        span,
                    ));
                }
            }
        }
        let inst = self.field_inst(base, name);
        self.expr(base)?;
        self.emit(inst, span);
        Ok(())
    }

    /// `&&` and `||` short-circuit, so they lower to a jump: there is no
    /// instruction for them, and an operator that evaluated both sides would
    /// be a different language.
    fn binary(
        &mut self,
        expr: &'a Expr,
        op: SourceBinary,
        lhs: &'a Expr,
        rhs: &'a Expr,
        span: Span,
    ) -> Result<(), Unsupported> {
        match op {
            SourceBinary::And | SourceBinary::Or => {
                // `&&`/`||` wanted as a value: the scalar form costs
                // `(2 - k) + 1` boundaries where `k` operands are already on
                // the scalar stack (both operands moved across, plus the
                // answer moved back), the value form costs `k`, so the
                // scalar form only wins where `k == 2` — both operands
                // already scalar, nothing but the answer crosses.
                if self.scalar_of(expr) == Some(Scalar::Bool)
                    && self.on_scalar_stack(lhs)
                    && self.on_scalar_stack(rhs)
                {
                    self.and_or_scalar(op, lhs, rhs, span)?;
                    self.emit(Inst::ScalarToValue(Scalar::Bool), span);
                    return Ok(());
                }
                let short = self.label();
                let end = self.label();
                self.expr(lhs)?;
                if op == SourceBinary::And {
                    self.jump(Inst::JumpIfFalse, short, span);
                } else {
                    self.jump(Inst::JumpIfTrue, short, span);
                }
                self.expr(rhs)?;
                self.jump(Inst::Jump, end, span);
                self.bind(short);
                // The side that short-circuited is the answer: `&&` that
                // stopped is `false` and `||` that stopped is `true`.
                self.constant(Const::Bool(op == SourceBinary::Or), span);
                self.bind(end);
                Ok(())
            }
            _ => {
                let op = binary_op(op).expect("`&&` and `||` are the two handled above");
                let inst = self.binary_inst(op, lhs, rhs);
                if let Inst::IntBinary(typed) = inst {
                    // The typed operator lives on the scalar stack, so its
                    // operands are lowered onto it and its answer is moved
                    // back only because a value is what was asked for here.
                    // Where a scalar is what was asked for, `expr_scalar`
                    // emits the same three instructions and no fourth.
                    self.expr_scalar(lhs)?;
                    self.expr_scalar(rhs)?;
                    self.emit(inst, span);
                    self.emit(Inst::ScalarToValue(int_result(typed)), span);
                    return Ok(());
                }
                self.expr(lhs)?;
                self.expr(rhs)?;
                self.emit(inst, span);
                Ok(())
            }
        }
    }

    /// `&&`/`||` lowered entirely on the scalar stack.
    ///
    /// The same shape as `binary` above with every instruction replaced by
    /// its scalar counterpart: the jump pops the scalar stack instead of the
    /// value stack, and the side that short-circuited is answered as a
    /// scalar rather than a `Const`. The short-circuiting side is still the
    /// answer for the same reason it always was — `&&` that stopped is
    /// `false` and `||` that stopped is `true` — this only changes which
    /// stack that answer is written to.
    fn and_or_scalar(
        &mut self,
        op: SourceBinary,
        lhs: &'a Expr,
        rhs: &'a Expr,
        span: Span,
    ) -> Result<(), Unsupported> {
        let short = self.label();
        let end = self.label();
        self.expr_scalar(lhs)?;
        if op == SourceBinary::And {
            self.jump(Inst::JumpIfFalseScalar, short, span);
        } else {
            self.jump(Inst::JumpIfTrueScalar, short, span);
        }
        self.expr_scalar(rhs)?;
        self.jump(Inst::Jump, end, span);
        self.bind(short);
        // The side that short-circuited is the answer: `&&` that stopped is
        // `false` and `||` that stopped is `true`.
        self.emit(Inst::ScalarConst(i64::from(op == SourceBinary::Or)), span);
        self.bind(end);
        Ok(())
    }

    /// `place = value` and `place += value`, which produce `()`.
    ///
    /// A compound assignment reads the place, then evaluates the right-hand
    /// side, then combines them — the order the interpreter reads them in.
    ///
    /// The store is the whole of what an assignment does, so lowered for
    /// effect it ends there and the `()` it would have answered is not built.
    fn assign(
        &mut self,
        op: Option<SourceBinary>,
        target: &'a Expr,
        value: &'a Expr,
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        if self.written_through_a_place(target) {
            return self.assign_through_place(op, target, value, position, span);
        }
        if matches!(target.kind, ExprKind::Field { .. }) {
            return self.assign_field(op, target, value, position, span);
        }
        let ExprKind::Ident(name) = &target.kind else {
            return Err(Unsupported::new("assignment to this place", span));
        };
        let Some(binding) = self.binding(name) else {
            return Err(Unsupported::new(
                format!("assignment to `{name}`, which is not a local"),
                span,
            ));
        };
        let (slot, kind) = (binding.slot, binding.kind);
        match op {
            None => match kind {
                SlotKind::Scalar(_) => self.expr_scalar(value)?,
                SlotKind::Value => self.expr(value)?,
                SlotKind::Place => {
                    unreachable!("a place binding is written by `assign_through_place`")
                }
            },
            Some(op) => {
                let Some(op) = binary_op(op) else {
                    return Err(Unsupported::new("this compound assignment", span));
                };
                // The place read is the left operand, so the type the checker
                // settled for it is what says whether this is integer
                // arithmetic — the same question `a + b` asks, asked of the
                // two expressions this form writes as one.
                let inst = self.binary_inst(op, target, value);
                match (kind, inst) {
                    // Read, combine, and write again without ever leaving the
                    // scalar stack. This is `i += 1` inside a loop, which is
                    // the case the whole arrangement exists for.
                    (SlotKind::Scalar(_), Inst::IntBinary(_)) => {
                        self.emit(Inst::LoadScalar(slot), target.span);
                        self.expr_scalar(value)?;
                        self.emit(inst, span);
                    }
                    (SlotKind::Scalar(what), _) => {
                        self.emit(Inst::LoadScalar(slot), target.span);
                        self.emit(Inst::ScalarToValue(what), target.span);
                        self.expr(value)?;
                        self.emit(inst, span);
                        self.emit(Inst::ValueToScalar, span);
                    }
                    (SlotKind::Value, Inst::IntBinary(typed)) => {
                        self.emit(Inst::LoadLocal(slot), target.span);
                        self.emit(Inst::ValueToScalar, target.span);
                        self.expr_scalar(value)?;
                        self.emit(inst, span);
                        self.emit(Inst::ScalarToValue(int_result(typed)), span);
                    }
                    (SlotKind::Value, _) => {
                        self.emit(Inst::LoadLocal(slot), target.span);
                        self.expr(value)?;
                        self.emit(inst, span);
                    }
                    (SlotKind::Place, _) => {
                        unreachable!("a place binding is written by `assign_through_place`")
                    }
                }
            }
        }
        self.emit(store_slot(kind, slot), span);
        self.unit_at(position, span);
        Ok(())
    }

    /// An assignment written through a place: `n = 1` where `n` is a `var`
    /// parameter, and `a.b.c = 1` wherever the path is longer than one step.
    ///
    /// The place is built twice for a compound assignment, once for the read
    /// and once for the write, rather than duplicated on the place stack.
    /// Building it is a slot load and a run of `place-field`s, none of which
    /// can have an effect and none of which can fail, so doing it twice is
    /// the same program — and the alternative would be an instruction whose
    /// only reader is this one lowering.
    ///
    /// The read happens before the right-hand side is evaluated, which is
    /// the order `Interpreter::eval`'s `ExprKind::Assign` arm reads in:
    /// `place.read(span)?` and then `self.eval(env, value)?`.
    fn assign_through_place(
        &mut self,
        op: Option<SourceBinary>,
        target: &'a Expr,
        value: &'a Expr,
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        if !self.is_a_place(target) {
            return Err(Unsupported::new("assignment to this place", span));
        }
        match op {
            None => {
                self.place(target)?;
                self.expr(value)?;
            }
            Some(op) => {
                let Some(op) = binary_op(op) else {
                    return Err(Unsupported::new("this compound assignment", span));
                };
                // The place read is the left operand, so the type the checker
                // settled for it is what says whether this is integer
                // arithmetic — the same question `a + b` asks.
                let inst = self.binary_inst(op, target, value);
                // The place the write will consume, built below the one the
                // read consumes: `place-read` takes the top of the place
                // stack and `place-write` takes what was under it.
                self.place(target)?;
                self.place(target)?;
                self.emit(Inst::PlaceRead, target.span);
                if let Inst::IntBinary(typed) = inst {
                    // A place is read and written as a `Value` whatever it
                    // holds, so this is the boundary in both directions
                    // around one typed operator — the same shape a written
                    // struct field has, and for the same reason.
                    self.emit(Inst::ValueToScalar, target.span);
                    self.expr_scalar(value)?;
                    self.emit(inst, span);
                    self.emit(Inst::ScalarToValue(int_result(typed)), span);
                } else {
                    self.expr(value)?;
                    self.emit(inst, span);
                }
            }
        }
        self.emit(Inst::PlaceWrite, span);
        self.unit_at(position, span);
        Ok(())
    }

    /// `place.field = value`, and the compound forms.
    ///
    /// The base must be a local. A struct is a value and a local is the only
    /// holder of its own, so writing a field is reading the struct, replacing
    /// the field, and storing the struct back — which is what
    /// [`crate::Inst::SetField`] does and why it is a whole-value update
    /// rather than a mutation through a place. A deeper path than one field is
    /// refused rather than rebuilt: it would need the intermediate struct put
    /// back too, and nothing in the subset produces one.
    ///
    /// `target` is the whole `place.field`, because that is what the
    /// instructions reading the struct point at: a diagnostic about the read
    /// is about the place, not about the name below it.
    fn assign_field(
        &mut self,
        op: Option<SourceBinary>,
        target: &'a Expr,
        value: &'a Expr,
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        let ExprKind::Field { base, name: field } = &target.kind else {
            unreachable!("`assign` dispatches here only for a field")
        };
        let field = field.node.as_str();
        let place = target.span;
        let ExprKind::Ident(name) = &base.kind else {
            return Err(Unsupported::new(
                "assignment to a field of anything but a local",
                span,
            ));
        };
        let Some(slot) = self.lookup(name) else {
            return Err(Unsupported::new(
                format!("assignment to a field of `{name}`, which is not a local"),
                span,
            ));
        };
        // The write goes by name whatever the checker settled: `SetField`
        // puts a value back where a name stands, and only the read has a
        // position to take instead.
        let named = self.outer.name(field);
        self.emit(Inst::LoadLocal(slot), place);
        match op {
            None => self.expr(value)?,
            Some(op) => {
                let Some(op) = binary_op(op) else {
                    return Err(Unsupported::new("this compound assignment", span));
                };
                let read = self.field_inst(base, field);
                let inst = self.binary_inst(op, target, value);
                self.emit(Inst::Dup, place);
                self.emit(read, place);
                if let Inst::IntBinary(typed) = inst {
                    // A field is a `Value` wherever it is read from, so this
                    // is the boundary in both directions around one typed
                    // operator. A struct's fields are not slots and this
                    // slice does not make them one.
                    self.emit(Inst::ValueToScalar, place);
                    self.expr_scalar(value)?;
                    self.emit(inst, span);
                    self.emit(Inst::ScalarToValue(int_result(typed)), span);
                } else {
                    self.expr(value)?;
                    self.emit(inst, span);
                }
            }
        }
        self.emit(Inst::SetField(named), span);
        self.emit(Inst::StoreLocal(slot), span);
        self.unit_at(position, span);
        Ok(())
    }

    /// `if` and `else`.
    ///
    /// An `if` with no `else` is `()` however it goes, including when the
    /// branch that ran produced something: there is no second branch to give
    /// the missing case a value, so the branch that ran does not get to
    /// supply one either. Its branch is therefore lowered for effect in both
    /// positions, and only the `()` at the join depends on which one this is.
    ///
    /// An `if` with an `else` is worth something, so the position reaches
    /// inside it: both branches are lowered in the position the `if` is in,
    /// and lowering for effect saves whatever each branch would have built.
    fn conditional(
        &mut self,
        condition: &'a Expr,
        then_branch: &'a Block,
        else_branch: Option<&'a Expr>,
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        let branch = branch_on(self.condition(condition)?);
        match else_branch {
            Some(else_branch) => {
                let otherwise = self.label();
                let end = self.label();
                self.jump(branch, otherwise, condition.span);
                self.block_at(then_branch, position)?;
                self.jump(Inst::Jump, end, span);
                self.bind(otherwise);
                self.expr_at(else_branch, position)?;
                self.bind(end);
            }
            None => {
                let end = self.label();
                self.jump(branch, end, condition.span);
                self.block_at(then_branch, Position::Effect)?;
                self.bind(end);
                self.unit_at(position, span);
            }
        }
        Ok(())
    }

    /// `while`, which is `()` however it leaves — so its body's value is
    /// never wanted, and lowered for effect the loop builds nothing at all.
    fn while_loop(
        &mut self,
        condition: &'a Expr,
        body: &'a Block,
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        let base = self.depth.unwrap_or(Depth::EMPTY);
        let top = self.label();
        let end = self.label();
        self.bind(top);
        let branch = branch_on(self.condition(condition)?);
        self.jump(branch, end, condition.span);
        self.loops.push(LoopFrame {
            break_to: end,
            continue_to: top,
            scopes: self.open_scopes,
            depth: base,
        });
        let lowered = self.block_at(body, Position::Effect);
        self.loops.pop();
        lowered?;
        self.jump(Inst::Jump, top, span);
        self.bind(end);
        self.unit_at(position, span);
        Ok(())
    }

    /// `for`, over a range written in the header or over a sequence.
    ///
    /// The iterable is evaluated once, in the enclosing scope, and the
    /// binding is declared in the scope the body sees — the two halves of
    /// what the interpreter does around `iterable_items`.
    ///
    /// A range header never builds a range value. [`Inst::MakeRange`] makes
    /// one, and a range written anywhere else is lowered through it, but a
    /// `for` has nothing to do with the value: it wants the integers between
    /// two bounds, so the bounds go into two hidden slots and the loop counts
    /// between them. Building a `Range` here and taking it apart again would
    /// be a value made for one instruction to discard, which is what
    /// `a_for_over_a_range_counts_between_two_hidden_slots` pins.
    ///
    /// Anything else is asked once, by `iter-items`, for the items a
    /// `for` walks it as — the elements of a sequence, the `MapEntry` of each
    /// pair of a `Map`, a `Set`'s elements in ascending order — and what
    /// comes back is always an `Array`, so the loop walks it by index with
    /// its length read once. Asking once is what makes iterating a `Vector`
    /// the body appends walk the same elements the interpreter's snapshot
    /// holds.
    fn for_loop(
        &mut self,
        binding: &'a str,
        iterable: &'a Expr,
        body: &'a Block,
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        let base = self.depth.unwrap_or(Depth::EMPTY);
        let mark = self.scope();

        let (cursor, header) = match &iterable.kind {
            ExprKind::Range {
                start,
                end,
                inclusive_end,
            } => {
                let cursor = self.declare(None, SlotKind::Value);
                let limit = self.declare(None, SlotKind::Value);
                self.expr(start)?;
                self.emit(Inst::StoreLocal(cursor), start.span);
                self.expr(end)?;
                self.emit(Inst::StoreLocal(limit), end.span);
                (
                    cursor,
                    Header::Range {
                        limit,
                        inclusive: *inclusive_end,
                    },
                )
            }
            _ => {
                let sequence = self.declare(None, SlotKind::Value);
                let length = self.declare(None, SlotKind::Value);
                let cursor = self.declare(None, SlotKind::Value);
                self.expr(iterable)?;
                self.emit(Inst::IterItems, iterable.span);
                self.emit(Inst::StoreLocal(sequence), iterable.span);
                self.emit(Inst::LoadLocal(sequence), iterable.span);
                let name = self.outer.name("length");
                self.emit(Inst::CallBuiltin { name, argc: 0 }, iterable.span);
                self.emit(Inst::StoreLocal(length), iterable.span);
                self.constant(Const::Int(0), iterable.span);
                self.emit(Inst::StoreLocal(cursor), iterable.span);
                (cursor, Header::Sequence { sequence, length })
            }
        };

        // The binding belongs to the scope the body sees, and the body's own
        // block opens a scope inside this one.
        // A `for` binding is read-only, which is what the interpreter
        // declares one as.
        let element = self.declare(Some(binding), SlotKind::Value);

        let top = self.label();
        let next = self.label();
        let end = self.label();
        self.bind(top);
        self.emit(Inst::LoadLocal(cursor), span);
        match header {
            Header::Range { limit, inclusive } => {
                self.emit(Inst::LoadLocal(limit), span);
                // `a..b` yields `b`, and `a..<b` stops before it. Comparing
                // rather than adding one to the bound is what keeps a range
                // ending at the largest `Int` from overflowing.
                self.emit(
                    Inst::Binary(if inclusive {
                        BinaryOp::Le
                    } else {
                        BinaryOp::Lt
                    }),
                    span,
                );
            }
            Header::Sequence { length, .. } => {
                self.emit(Inst::LoadLocal(length), span);
                self.emit(Inst::Binary(BinaryOp::Lt), span);
            }
        }
        self.jump(Inst::JumpIfFalse, end, span);
        match header {
            Header::Range { .. } => self.emit(Inst::LoadLocal(cursor), span),
            Header::Sequence { sequence, .. } => {
                self.emit(Inst::LoadLocal(sequence), span);
                self.emit(Inst::LoadLocal(cursor), span);
                let get = self.outer.name("get");
                self.emit(Inst::CallBuiltin { name: get, argc: 1 }, span);
                // An indexed read answers an `Option`, and the test above
                // has already put the cursor below the length, so what comes
                // back is a `Some`. `Try` is the instruction that opens one,
                // and it is used here rather than `unwrapOr` because there is
                // no element value the lowering could invent as a fallback.
                self.emit(Inst::Try, span);
            }
        }
        self.emit(Inst::StoreLocal(element), span);

        self.loops.push(LoopFrame {
            break_to: end,
            continue_to: next,
            scopes: self.open_scopes,
            depth: base,
        });
        let lowered = self.block_at(body, Position::Effect);
        self.loops.pop();
        lowered?;

        // `continue` lands here, so that skipping the rest of a body still
        // advances the cursor.
        self.bind(next);
        self.emit(Inst::LoadLocal(cursor), span);
        self.constant(Const::Int(1), span);
        self.emit(Inst::Binary(BinaryOp::Add), span);
        self.emit(Inst::StoreLocal(cursor), span);
        self.jump(Inst::Jump, top, span);

        self.bind(end);
        self.release(mark);
        self.unit_at(position, span);
        Ok(())
    }

    /// Leaves the nearest enclosing loop.
    fn leave_loop(&mut self, breaking: bool, span: Span) -> Result<(), Unsupported> {
        let Some(frame) = self.loops.last() else {
            return Err(Unsupported::new(
                if breaking {
                    "a `break` outside a loop"
                } else {
                    "a `continue` outside a loop"
                },
                span,
            ));
        };
        let target = if breaking {
            frame.break_to
        } else {
            frame.continue_to
        };
        let base = frame.depth;
        // Every task scope written between here and the loop is left without
        // reaching the `leave-scope` below its body, and leaving a scope waits
        // for or cancels its children whichever way it is left. The oracle
        // reaches the same place through `Interpreter::leave_scope`, whose
        // early branch cancels for a `Break` exactly as it does for a `return`
        // or an error.
        let scopes = self.open_scopes - frame.scopes;
        for _ in 0..scopes {
            self.emit(Inst::CancelScope, span);
        }
        // Whatever the half-evaluated expression around this left on any of
        // the stacks goes with it, so the loop's exit is reached at the
        // depths the loop runs at. A place can be standing for the reason a
        // value can: `f(var x, if c { break } else { 1 })` has pushed one
        // before it evaluates the argument the `break` is written in.
        if let Some(depth) = self.depth {
            for _ in base.values..depth.values {
                self.emit(Inst::Pop, span);
            }
            for _ in base.scalars..depth.scalars {
                self.emit(Inst::ScalarPop, span);
            }
            for _ in base.places..depth.places {
                self.emit(Inst::PlacePop, span);
            }
        }
        self.jump(Inst::Jump, target, span);
        Ok(())
    }

    // ------------------------------------------------------------- `match`

    /// `match subject { pattern => body ... }`.
    ///
    /// The subject is evaluated once and stays on the stack while the arms
    /// are tried, because [`Inst::TestCase`] and [`Inst::GetPayload`] peek:
    /// an arm that does not match has to leave the value for the next one.
    /// The arm that does match pops it before its body runs, and the value
    /// no arm covered is what [`Inst::NoMatch`] reports.
    ///
    /// Arms are tried in the order they are written and the first that
    /// matches is the only one that runs, which is what `ExprKind::Match`
    /// does; an arm's binders live in a scope of its own, released when the
    /// arm ends, exactly as a block's slots are.
    ///
    /// A `match`'s value is the value of the arm that ran, so the position it
    /// is lowered in is every arm's position: a `match` written as a
    /// statement builds nothing in any of them, and one written as an
    /// expression is unchanged.
    fn match_expr(
        &mut self,
        scrutinee: &'a Expr,
        arms: &'a [MatchArm],
        position: Position,
        span: Span,
    ) -> Result<(), Unsupported> {
        self.expr(scrutinee)?;
        // The depth the subject alone stands at. Every failed test gets back
        // down to it before it jumps, so the next arm begins where this one
        // began and `validate`'s simulation sees one depth per instruction.
        let subject = self.depth.map_or(0, |depth| depth.values);
        let end = self.label();
        for arm in arms {
            let mark = self.scope();
            let next = self.label();
            self.pattern(&arm.pattern, next, subject)?;
            self.emit(Inst::Pop, arm.span);
            self.expr_at(&arm.body, position)?;
            self.release(mark);
            self.jump(Inst::Jump, end, arm.span);
            self.bind(next);
        }
        // Exhaustiveness is the checker's to prove and it does not prove it
        // yet, so a subject no arm covered stops the run rather than
        // answering. Where an arm matches everything, no jump reaches here
        // and `emit` writes nothing.
        self.emit(Inst::NoMatch, span);
        self.bind(end);
        Ok(())
    }

    /// One pattern, against the value on top of the stack.
    ///
    /// The value stays where it is: a test peeks and a binder copies, so what
    /// this leaves behind is what it was given, plus the payloads a nested
    /// pattern is still standing on. A test that fails discards those and
    /// jumps to `next`, so the arm after this one starts at `subject` — the
    /// depth the whole `match` runs its arms at.
    ///
    /// The rules are `Interpreter::match_pattern`'s, one for one, with one
    /// exception it names: a pattern that binds a different number of values
    /// than its case carries is a run-time error there, and here it is a
    /// `get-payload` past the end of the payload. `cove-sema` refuses such a
    /// pattern — `cove::type::payload_arity` — so no checked program reaches
    /// either, and reproducing the message would be reproducing it for a
    /// program that cannot exist.
    fn pattern(
        &mut self,
        pattern: &'a Pattern,
        next: usize,
        subject: u32,
    ) -> Result<(), Unsupported> {
        let span = pattern.span;
        match &pattern.kind {
            // Matches anything and binds nothing, so there is nothing to
            // emit: falling through is the match.
            PatternKind::Wildcard => Ok(()),
            PatternKind::Binding(name) => self.binder(name, next, subject, span),
            PatternKind::Literal(expr) => {
                // The same equality `==` is, because it is the same
                // comparison: `match_pattern` asks `eq_value`, which is what
                // `binary` answers `==` with once both sides are one type —
                // and the checker refuses a literal pattern of another type
                // before either backend sees it.
                self.emit(Inst::Dup, span);
                self.expr(expr)?;
                self.emit(Inst::Binary(BinaryOp::Eq), span);
                self.test(next, subject, span);
                Ok(())
            }
            PatternKind::Variant { path, payload } => {
                let case = self.outer.name(&case_tested(path));
                self.emit(Inst::TestCase(case), span);
                self.test(next, subject, span);
                // Each payload is matched against its own pattern, on top of
                // the value it came out of, which is how `Ok(Some(x))` reads
                // two levels down. The payload is dropped once its pattern is
                // done with it, leaving the enum it belongs to on top.
                for (index, sub) in payload.iter().enumerate() {
                    self.emit(Inst::GetPayload(index as u32), span);
                    self.pattern(sub, next, subject)?;
                    self.emit(Inst::Pop, span);
                }
                Ok(())
            }
        }
    }

    /// A binder: `other` binds the value, and `None` does not.
    ///
    /// `match_pattern` reads a binder named exactly `None` as a case test
    /// whenever the value it is given is an `Option`, and as a name
    /// otherwise. Which of the two it is therefore depends on the value
    /// rather than on the pattern, so both are lowered and the run picks:
    /// `Option` declares `Some` and `None` and nothing else, so a value that
    /// is neither is not an `Option`, and the name binds.
    ///
    /// Today's parser reaches none of this — a pattern whose name begins with
    /// an uppercase letter is a variant, and `None` does — so what is lowered
    /// here is the oracle's rule rather than a program's shape. It is
    /// reproduced anyway because the oracle is what a backend is answerable
    /// to, and a rule a backend quietly did not have is the kind of
    /// difference the differential tests exist to make impossible.
    ///
    /// The two tests name the type by its short name, which is what a pattern
    /// writes and what `match_pattern` compares a *variant* against; the
    /// binder rule compares the whole type name instead, so a declared enum
    /// that a module named `Option` and gave a case called `None` would be
    /// read as the builtin here and as a name there. That program cannot be
    /// written: the pattern it would need is one the parser makes a variant.
    fn binder(
        &mut self,
        name: &'a str,
        next: usize,
        subject: u32,
        span: Span,
    ) -> Result<(), Unsupported> {
        if name != builtins::NONE_CASE.name {
            self.emit(Inst::Dup, span);
            let slot = self.declare(Some(name), SlotKind::Value);
            self.emit(Inst::StoreLocal(slot), span);
            return Ok(());
        }
        let matched = self.label();
        let none = self.outer.name(&qualified_case(
            builtins::OPTION.name,
            builtins::NONE_CASE.name,
        ));
        self.emit(Inst::TestCase(none), span);
        self.jump(Inst::JumpIfTrue, matched, span);
        let some = self.outer.name(&qualified_case(
            builtins::OPTION.name,
            builtins::SOME_CASE.name,
        ));
        self.emit(Inst::TestCase(some), span);
        let bind_it = self.label();
        self.jump(Inst::JumpIfFalse, bind_it, span);
        self.fail_arm(next, subject, span);
        self.bind(bind_it);
        self.emit(Inst::Dup, span);
        let slot = self.declare(Some(name), SlotKind::Value);
        self.emit(Inst::StoreLocal(slot), span);
        self.bind(matched);
        Ok(())
    }

    /// Consumes the `Bool` a test pushed and leaves for the next arm when it
    /// is false.
    ///
    /// A test written at the top of a pattern can jump straight there,
    /// because the subject is all that stands on the stack. One written
    /// inside a payload cannot: the payloads it is standing on have to come
    /// off first, and a conditional jump has nowhere to put them.
    fn test(&mut self, next: usize, subject: u32, span: Span) {
        if self.depth.map(|depth| depth.values) == Some(subject + 1) {
            self.jump(Inst::JumpIfFalse, next, span);
            return;
        }
        let matched = self.label();
        self.jump(Inst::JumpIfTrue, matched, span);
        self.fail_arm(next, subject, span);
        self.bind(matched);
    }

    /// Leaves a half-matched pattern for the arm after it.
    ///
    /// Whatever the pattern was standing on goes with it, so the next arm is
    /// reached at the depth the arms run at — the same thing
    /// [`Body::leave_loop`] does for a `break` written inside a half-
    /// evaluated expression.
    fn fail_arm(&mut self, next: usize, subject: u32, span: Span) {
        if let Some(depth) = self.depth {
            for _ in subject..depth.values {
                self.emit(Inst::Pop, span);
            }
        }
        self.jump(Inst::Jump, next, span);
    }
}

/// The name a [`Inst::TestCase`] carries for one pattern path.
///
/// `match_pattern` tests the case name, and — when the path has two or more
/// segments — the enum's own short type name as well, so that
/// `Status.Confirmed` does not match another enum's `Confirmed`. One
/// instruction carries one name, so the two are written as one: a case alone
/// where the pattern named one, and `Type.Case` where it named both. Neither
/// a case name nor a type's short name can contain a `.`, so the pair reads
/// back unambiguously.
///
/// The segments before the last two are not tested, for the reason the
/// interpreter does not test them: `booking.Status.Confirmed` says which
/// module the enum was reached through, and a value carries the module that
/// *declares* it, which are two different questions.
fn case_tested(path: &[cove_syntax::ast::Ident]) -> String {
    let Some(case) = path.last() else {
        // A path with no segments cannot be written, and a test that names
        // nothing matches nothing, which is what `match_pattern` answers for
        // one.
        return String::new();
    };
    if path.len() < 2 {
        return case.node.clone();
    }
    qualified_case(&path[path.len() - 2].node, &case.node)
}

/// A case name written with the short name of the type that declares it,
/// which is the pair [`Inst::TestCase`] tests both halves of.
fn qualified_case(type_name: &str, case: &str) -> String {
    format!("{type_name}.{case}")
}
