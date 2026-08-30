//! Lowering a checked program to the executable IR, and the validation that
//! stands between the two.
//!
//! What this lowers is decided by [ADR 0019](../../../docs/adr/0019-executable-ir-and-vm.md):
//! everything it covers becomes instructions, and everything it does not is
//! named as [`Unsupported`] rather than approximated. A VM that quietly
//! finished a run somewhere else would be a VM whose measurements are about a
//! mixture, so a construct with no lowering stops the lowering and says what
//! it was.
//!
//! # The unit that is lowered is the unit that is run
//!
//! [`lower_entry`] lowers what one entry can reach and nothing else, because
//! an entry is what a run is. Reachability is not derived separately: a body
//! reaches exactly the functions it emits a `Call` to, so numbering a call's
//! target when the call is emitted *is* the closure, and the worklist is
//! empty when nothing new was named.
//!
//! [`lower`] is the same loop seeded with every declaration instead of one,
//! so a whole-package listing and an entry's program are two seeds of one
//! lowering rather than two lowerings that could drift.
//!
//! # An expression is lowered for its value or for its effect
//!
//! `Position` below is the distinction. A statement's value is read by nothing,
//! and `()` is a value here — an assignment, a loop, and an `if` with no
//! `else` all answer one — so lowering every expression the same way builds
//! a `Unit` for a `Pop` to take away again. That was six of the twenty-five
//! instructions `benches/arith` ran per iteration. Lowering for effect emits
//! neither, and reaches inside a block, an `if`/`else`, and a `match` so that
//! the saving is taken where the value would have been built.
//!
//! It changes nothing about what a program means: the value of a block, of an
//! `if` used as an expression, and of a `match` used as an expression are
//! what they were, and only a value nobody reads stops being built.
//! [`validate`]'s depth simulation is what catches a mistake in it.
//!
//! # A settled type is an instruction, and an abstention is not
//!
//! `cove-sema` publishes what it worked out about every expression, and this
//! pass reads it rather than guessing from the shape of the source. Three
//! things follow from it, and nothing else does:
//!
//! - An operator over two operands the checker settled as `Int` lowers to
//!   [`Inst::IntBinary`], which needs no look at what it was handed.
//! - A field of a receiver whose type the checker settled lowers to
//!   [`Inst::GetFieldAt`], which is an index rather than a name to scan for.
//! - A method call the checker recorded a declaration for calls it, so a
//!   name a builtin type and a declared type both answer to is no longer a
//!   refusal.
//!
//! The rule the first two share is that a type must be *settled*.
//! `Ty::Unknown` is the checker saying it did not prove this and no fact at
//! all is the expression never having been walked; neither is `Int`, and
//! both lower to the untyped instruction. Specialising on either would be
//! this pass deciding something the checker declined to, which is the one
//! thing ADR 0019 says a lowering does not do.
//!
//! # A settled type is also where the value is kept
//!
//! The same rule, asked of a binding rather than of an operator, decides
//! which of the VM's two stacks its slot lives in. A local declared from
//! something the checker settled as `Int` or `Bool` is an `i64` in the
//! scalar stack — [`SlotKind::Scalar`] — and everything else is the `Value`
//! it always was. It is one rule and not two: `Body::scalar_of` is
//! `Body::is_int` asked about both scalar types, and an abstention answers
//! both the same way.
//!
//! [`Inst::IntBinary`] reads and writes that stack, because two `i64` in and
//! one out is the whole of what it does, and [`Inst::ScalarConst`],
//! [`Inst::LoadScalar`], [`Inst::StoreScalar`] and
//! [`Inst::JumpIfFalseScalar`] are what let a loop over integers stay in it.
//! [`Inst::ScalarToValue`] and [`Inst::ValueToScalar`] are the boundary, and
//! the lowering spends one only where an expression really does cross:
//! `Body::on_scalar_stack` is what keeps a condition the value stack
//! computed from being moved across just to be tested.
//!
//! # A signature is where the value is kept too
//!
//! The same rule again, asked of a declaration's boundary rather than of a
//! binding, decides the calling convention. A parameter the checker settled
//! as `Int` or `Bool` is a scalar slot, so its argument is pushed onto the
//! scalar stack and *becomes* that slot without moving, exactly as a value
//! argument becomes a value slot; and a function whose return type the
//! checker settled leaves its answer on the scalar stack and ends in
//! [`Inst::ReturnScalar`]. [`Function::params`] and [`Function::returns`]
//! are that convention written down, and `validate` is where a call and its
//! callee are made to agree about it.
//!
//! It is read from `Facts::signature` rather than derived from the
//! annotations here, for the reason everything else is: two readings that
//! could disagree is what `Facts` exists to prevent. A declaration the
//! checker recorded nothing for keeps the convention every function had
//! before — every argument on the value stack, the answer on the value
//! stack — because an abstention is not a settled type here either.
//!
//! What is still deliberately not scalar is a struct's field, which is not a
//! slot at all.
//!
//! # What the interpreter decides and this reproduces
//!
//! `crates/cove-runtime/src/interp.rs` is the oracle, and seven of its rules
//! are most of the difficulty here:
//!
//! - **A name resolves in declaration order.** A reference written before a
//!   `let` in the same block does not see it, so a `let`'s value is lowered
//!   *before* its name is declared and `let x = x` reads the outer `x`.
//! - **Shadowing makes a new slot.** `Env::declare` pushes; it never
//!   overwrites. Two `let x`s are two slots, and a reference reaches the
//!   later one because a lookup scans from the top.
//! - **A block's slots are released when the block ends**, so a later sibling
//!   block reuses the same numbers and each of `value_frame_size` and
//!   `scalar_frame_size` is a high-water mark rather than a count of
//!   declarations.
//! - **A `for` binding lives in the scope its body sees**, and the iterable
//!   is evaluated in the enclosing one.
//! - **Evaluation is left to right everywhere**: arguments, operands, array
//!   elements, and struct fields.
//! - **A struct's fields are pushed in declaration order.** A call whose
//!   labels stand in declaration order fills the parameters in increasing
//!   order, which is what makes pushing the arguments left to right the same
//!   as pushing them in declaration order. `cove-sema` is what holds a
//!   program to that (ADR 0021); `arguments_in_order` below states the same
//!   rule as this pass's own invariant, because it is what the calling
//!   convention is built on and a lowering that assumed it silently would be
//!   assuming it.
//! - **A default argument is evaluated by the callee**, in an environment
//!   holding the parameters declared before it. `bind_params` walks the
//!   parameters in order and reaches `None => match &param.default` inside
//!   the frame it is filling, so a default may read an earlier parameter and
//!   cannot read a later one. A call that leaves a parameter out therefore
//!   reaches a *specialisation*: an ordinary function whose arity is what
//!   that call site supplies and whose prologue computes the rest, which is
//!   what `Instance` below is the key of.
//! - **A `match` arm is a scope, and the first that matches is the only one
//!   that runs.** `match_pattern` tests and binds as it walks, and the arm
//!   that does not match releases what it bound — so an arm's slots behave
//!   the way a block's do, and a subject no arm covers stops the run.
//!
//! # What is not lowered
//!
//! A `snapshot` a declared conformance would have to answer from inside a
//! container, a task scope in a function that answers on the scalar stack, a
//! `lock` whose closure is not written at the call, assignment to a field of
//! anything but a local, and any call whose callee is neither a name nor a
//! field of one. Each is reported in the words a Cove programmer writes it
//! in.
//!
//! # What is refused because the program is wrong
//!
//! Two of the refusals are not about this pass being unfinished. A write to
//! a `let` binding, and a method call by a name whose answer nothing has
//! settled, are reported because the alternative is a backend that accepts
//! what the oracle refuses or that guesses which of two targets was meant.
//! ADR 0012 ranks the oracle above a backend, so refusing to lower is the
//! answer and approximating is not.
//!
//! The second of those two is now narrow. A call the checker recorded a
//! declaration for is that declaration's, so a name two types share stops
//! being ambiguous the moment the receiver's type is known; what is left is
//! a call the checker recorded nothing for, where a name is still all there
//! is.
//!
//! [`Function::params`]: crate::Function::params
//! [`Function::returns`]: crate::Function::returns

mod body;
mod convention;
mod expr;
mod fuel;
mod index;
mod scan;
mod validate;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use cove_diag::FileId;
use cove_diag::Span;
use cove_schema::builtins;
use cove_schema::hosts;
use cove_schema::TypeSchema;
use cove_sema::resolve::Program as Checked;
use cove_sema::typeck::Ty;
use cove_syntax::ast::{Arg, Block, Expr, ExprId, ExprKind, StructDecl};

use crate::{FunctionId, Inst, Program, Scalar, SlotKind, Unsupported};

use body::{Body, Position};
use convention::{reject_parameter, scalar_of_ty, slot_kind_of};
use index::{reject_dyn, Instance, Key, Lowering};

pub use fuel::block_fuel;
pub use validate::validate;

/// A lowered program and the function to start it at.
///
/// The id is here because the lowering already knows it — the entry is the
/// first function it numbers — and a caller that looked it up again by name
/// would be asking a question this pass has already answered.
#[derive(Debug)]
pub struct Lowered {
    pub program: Program,
    pub entry: FunctionId,
}

/// Lowers what the entry `module.name` can reach, and nothing else.
///
/// The unit being run is an entry, so the unit being lowered is an entry.
/// A construct the lowering does not cover refuses this program only if the
/// entry can reach it: a closure in a module this entry neither imports nor
/// calls is not part of the program this entry is, and refusing for it would
/// be refusing for a run that cannot happen.
///
/// What it *can* reach is what the lowering emits. A body reaches exactly
/// the functions it emits a [`Inst::Call`] to, so the closure needs no
/// separate pass: the entry is numbered, its body is lowered, every call
/// numbers a target that was not numbered yet, and the work ends when a body
/// names nothing new. Recursion and a cycle of mutual recursion end there
/// too, because a declaration is numbered once.
///
/// A name this package does not declare is reported rather than panicked on,
/// since the caller that chose it — a `[run.<name>]` table — is a file a
/// person edits.
pub fn lower_entry(checked: &Checked, module: &str, name: &str) -> Result<Lowered, Unsupported> {
    let mut lowering = Lowering::index(checked);
    let Some(key) = lowering.entry_point(module, name) else {
        return Err(Unsupported::new(
            format!("`{module}.{name}`, which this package does not declare"),
            // A name that was looked for and not found has no declaration to
            // underline, and inventing one would point a reader at source
            // that has nothing to do with it.
            Span::new(FileId(0), 0, 0),
        ));
    };
    let entry = lowering.number(Instance::whole(
        key,
        lowering.declaration(key).decl.params.len(),
    ));
    Ok(Lowered {
        program: lowering.reachable()?,
        entry,
    })
}

/// Lowers every function of a checked program.
///
/// This is [`lower_entry`]'s loop seeded with every declaration rather than
/// with one, so there is a single lowering and a whole-package listing is
/// what it produces when nothing is left out. Seeding numbers everything
/// before any body is lowered, so a call reaches a declaration written later
/// in the package and a function reaches itself. The order is the checker's
/// own — modules by name, then free functions by name, then methods by type
/// and name — which is what makes a listing stable enough for a golden test.
///
/// One unsupported construct anywhere fails the whole program, which is what
/// a whole-package listing means: everything the package declares is part of
/// it, whether or not an entry reaches it.
pub fn lower(program: &Checked) -> Result<Program, Unsupported> {
    let mut lowering = Lowering::index(program);
    for index in 0..lowering.catalog.len() {
        let key = Key(index);
        lowering.number(Instance::whole(
            key,
            lowering.declaration(key).decl.params.len(),
        ));
    }
    lowering.reachable()
}

impl<'a, 'l> Body<'a, 'l> {
    /// Lowers a call, answering where it left its result.
    ///
    /// `Some` means the scalar stack, which is what a call to a function
    /// whose return type the checker settled as `Int` or `Bool` leaves it
    /// on; `None` means the value stack, which is what every other call
    /// leaves it on. The answer is threaded up rather than asked about
    /// afterwards because only the path that resolved the callee knows it —
    /// a builtin, a host operation, a constructor, and a declared function
    /// are four different answers reached through four different lookups.
    fn call(
        &mut self,
        id: ExprId,
        callee: &'a Expr,
        written: &'a [Arg],
        trailing: Option<&'a Expr>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        let args = Args::new(written, trailing);
        match &callee.kind {
            ExprKind::Ident(name) => self.call_named(name, args, span),
            ExprKind::Field { base, name } => self.call_qualified(id, base, &name.node, args, span),
            _ => Err(Unsupported::new("a call through a value", callee.span)),
        }
    }

    /// `f(...)`, where `f` is a bare name.
    ///
    /// The order is the interpreter's: a local first — which is what makes a
    /// binding shadow a declaration — then a declared function, a struct
    /// initializer, an imported host operation, and a free builtin.
    fn call_named(
        &mut self,
        name: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        if self.lookup(name).is_some() {
            return self.call_through_value(name, args, span);
        }
        if let Some(key) = self.outer.function_of(self.module, name) {
            return self.call_declared(key, None, args, span);
        }
        if let Some((owner, decl)) = self.outer.struct_of(self.module, name) {
            return on_the_value_stack(self.make_struct(owner, decl, args, span));
        }
        if self.outer.declares_enum(self.module, name) {
            return Err(Unsupported::new(
                format!("`{name}`, which names an enum"),
                span,
            ));
        }
        if let Some(module) = self.outer.host_item(self.module, name) {
            return on_the_value_stack(self.call_host(module, name, args, span));
        }
        if name == builtins::MAP_ENTRY.name {
            return on_the_value_stack(self.make_map_entry(args, span));
        }
        if let Some(schema) = builtins::free_builtin(name) {
            return on_the_value_stack(self.make_builtin(schema.name, args, span));
        }
        Err(Unsupported::new(
            format!("a call to `{name}`, which the lowering cannot resolve"),
            span,
        ))
    }

    /// `f(...)`, where `f` is a local holding a callable value.
    ///
    /// The arguments go on the value stack left to right and the callee on
    /// top of them, which is the one place an operand is not in source
    /// order — see [`Inst::CallValue`] for why that is unobservable and what
    /// it buys.
    ///
    /// Nothing here knows what `f` holds, so nothing here can put an
    /// argument anywhere but the value stack, and nothing can put a label on
    /// one either: `bind_params` matches a label against the callee's own
    /// parameter names, which are a run-time fact about the closure. A
    /// labelled argument is therefore refused rather than lowered as a
    /// positional one, which is the direction a second backend is allowed to
    /// be wrong in.
    fn call_through_value(
        &mut self,
        name: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        plain_arguments(args, name)?;
        if let Some(arg) = args.iter().find(|arg| arg.label.is_some()) {
            return Err(Unsupported::new(
                format!("a labelled argument to `{name}`, which is called through a value"),
                arg.span,
            ));
        }
        if args.len() >= u16::MAX as usize {
            return Err(Unsupported::new(
                format!("a call to `{name}` with more than 65534 arguments"),
                span,
            ));
        }
        let argc = args.len() as u16;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        self.ident(name, span)?;
        self.emit(Inst::CallValue { argc }, span);
        // On the value stack, because a call through a value has no callee
        // to read a convention off.
        Ok(None)
    }

    /// `head.name(...)`, where `head` may be a host module, an enum, a
    /// struct, or a module imported whole — and is a receiver when it is
    /// none of those.
    fn call_qualified(
        &mut self,
        id: ExprId,
        base: &'a Expr,
        name: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        if let ExprKind::Ident(head) = &base.kind {
            if self.lookup(head).is_none() {
                if self.outer.is_host_module(self.module, head) {
                    return on_the_value_stack(self.call_host(head, name, args, span));
                }
                if let Some((owner, decl)) = self.outer.enum_of(self.module, head) {
                    // A case wins over an associated function of the same
                    // name, so naming a case never changes meaning when an
                    // `impl` block is added — which is the order
                    // `Interpreter::eval_call` asks in.
                    let is_case = decl.cases.iter().any(|case| case.name.node == name);
                    if !is_case {
                        if let Some(key) = self.outer.method_of(owner, head, name) {
                            return self.call_declared(key, None, args, span);
                        }
                    }
                    return on_the_value_stack(self.make_enum(owner, head, name, args, span));
                }
                if let Some((owner, _)) = self.outer.struct_of(self.module, head) {
                    if let Some(key) = self.outer.method_of(owner, head, name) {
                        return self.call_declared(key, None, args, span);
                    }
                }
                if let Some(owner) = self.outer.imported_module(self.module, head) {
                    if let Some(key) = self.outer.exported_function(owner, name) {
                        return self.call_declared(key, None, args, span);
                    }
                    if let Some(decl) = self.outer.exported_struct(owner, name) {
                        return on_the_value_stack(self.make_struct(owner, decl, args, span));
                    }
                    return Err(Unsupported::new(
                        format!("`{head}.{name}`, which module `{owner}` does not export"),
                        span,
                    ));
                }
                if builtins::is_builtin_type(head) {
                    return on_the_value_stack(self.call_builtin_assoc(head, name, args, span));
                }
            }
        }
        self.method_call(id, base, name, args, span)
    }

    /// A call to a function this package declares, with the receiver a
    /// method needs pushed first.
    ///
    /// Each argument is lowered into the stack its own parameter's slot kind
    /// names, and nothing is moved afterwards: the arguments already stand in
    /// declaration order — `arguments_in_order` refuses a call whose
    /// arguments do not — so within each stack they land in exactly the
    /// order that stack's slots are numbered in, and *become* those slots.
    ///
    /// Answers where the call left its result, which is the callee's
    /// `returns` read from the same signature the callee's own lowering
    /// reads. Both sides of a call therefore agree by construction rather
    /// than by convention, and `validate` says so out loud.
    fn call_declared(
        &mut self,
        key: Key,
        receiver: Option<&'a Expr>,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        let declared = self.outer.declaration(key);
        let decl = declared.decl;
        let what = declared.name.clone();

        for (at, param) in decl.params.iter().enumerate() {
            reject_parameter(param, at + 1 == decl.params.len())?;
        }
        let names: Vec<&str> = decl
            .params
            .iter()
            .map(|param| param.name.node.as_str())
            .collect();
        // `reject_parameter` has already refused a variadic parameter that
        // is not the last one, so the last one is the only one there can be.
        let variadic = decl.params.last().is_some_and(|param| param.variadic);
        let assigned = arguments_in_order(&names, args, &what, variadic, span)?;

        // Which parameters this call site hands an argument, which is what
        // decides *which* function it calls: a parameter left out is one the
        // callee computes, so a callee that computes one is a different
        // function from a callee that is given it. A variadic parameter is
        // always supplied, because the leftovers are collected here into the
        // one `Array` it receives and an empty one is an argument like any
        // other.
        let mut supplied: Vec<bool> = assigned.slots.iter().map(Option::is_some).collect();
        if variadic {
            supplied[names.len() - 1] = true;
        }
        for (at, param) in decl.params.iter().enumerate() {
            if supplied[at] {
                continue;
            }
            if param.default.is_none() {
                return Err(Unsupported::new(
                    format!(
                        "a call to `{what}` that does not supply one argument for every parameter"
                    ),
                    span,
                ));
            }
            if param.is_var {
                // `bind_params` would bind the default's *value* here rather
                // than an alias, so the parameter the body writes through
                // would name storage no caller owns. Nothing writes one, and
                // refusing says so rather than lowering a place that names
                // nothing.
                return Err(Unsupported::new(
                    format!(
                        "a call to `{what}` that leaves the `var` parameter `{}` to a default",
                        param.name.node
                    ),
                    span,
                ));
            }
        }

        // The same signature the callee's own lowering reads, so the two
        // cannot disagree about where an argument goes; a declaration the
        // checker recorded nothing about falls back to the convention every
        // function had before, on both sides at once.
        let signature = self.outer.signature(key);
        // `Inst::Call` holds each count in a `u16` — see its doc comment for
        // what that buys — so a declaration with more parameters than that
        // is refused here rather than counted into a number that cannot
        // hold it. Nothing writes one; the check is what makes the width a
        // fact rather than an assumption.
        if decl.params.len() >= u16::MAX as usize {
            return Err(Unsupported::new(
                format!("a call to `{what}`, which has more than 65534 parameters"),
                span,
            ));
        }
        let mut value_argc: u16 = 0;
        let mut scalar_argc: u16 = 0;
        let mut place_argc: u16 = 0;
        let mut into = |kind: SlotKind| match kind {
            SlotKind::Value => value_argc += 1,
            SlotKind::Scalar(_) => scalar_argc += 1,
            SlotKind::Place => place_argc += 1,
        };

        match (decl.receiver, receiver) {
            (Some(declared), Some(expr)) => {
                if declared.is_var {
                    // A `var self` receiver is a place, and it is the one
                    // the method writes through. That it *is* a writable
                    // place is `cove-sema`'s to say and it has said it — see
                    // ADR 0021 — so this builds the place and nothing else.
                    into(SlotKind::Place);
                    self.place(expr)?;
                } else {
                    let kind = signature
                        .and_then(|signature| signature.receiver.as_ref())
                        .map_or(SlotKind::Value, slot_kind_of);
                    into(kind);
                    match kind {
                        SlotKind::Scalar(_) => self.expr_scalar(expr)?,
                        SlotKind::Value => self.expr(expr)?,
                        SlotKind::Place => {
                            unreachable!("only a `var self` receiver is a place")
                        }
                    }
                }
            }
            (Some(_), None) => {
                return Err(Unsupported::new(
                    format!("a call to the method `{what}` with no receiver"),
                    span,
                ))
            }
            (None, Some(_)) => {
                return Err(Unsupported::new(
                    format!("a call to `{what}`, which takes no receiver"),
                    span,
                ))
            }
            (None, None) => {}
        }
        // Every parameter but a variadic one takes at most one argument, and
        // `arguments_in_order` has already refused a call whose arguments do
        // not fill the parameters in increasing order — so pushing them in
        // the order the parameters are declared is pushing them in the order
        // they are written, and the one a parameter was left to its default
        // is simply not there.
        let fixed = names.len() - usize::from(variadic);
        for at in 0..fixed {
            let Some(position) = assigned.slots[at] else {
                continue;
            };
            let arg = args.at(position);
            // A `...` here fills one parameter's slot, and `bind_params`
            // reads that slot through `value_of` without looking at
            // `spread` — the whole array becomes the argument. Refused
            // rather than reproduced: see `no_spread_here`.
            if arg.spread {
                return Err(no_spread_here(&what, arg.span));
            }
            // The marking is at both ends and has to agree at both, which is
            // what `bind_params` checks at run time and what this checks
            // before the run.
            let declared_var = decl.params[at].is_var;
            if declared_var != arg.is_var {
                return Err(var_marking_disagrees(
                    &what,
                    &decl.params[at].name.node,
                    declared_var,
                    arg.span,
                ));
            }
            if declared_var {
                // A `var` argument names the caller's own place, and that it
                // is one, and a writable one, is `cove-sema`'s to say — see
                // ADR 0021.
                into(SlotKind::Place);
                self.place(arg.value)?;
                continue;
            }
            let kind = signature
                .and_then(|signature| signature.params.get(at))
                .map_or(SlotKind::Value, slot_kind_of);
            into(kind);
            match kind {
                SlotKind::Scalar(_) => self.expr_scalar(arg.value)?,
                SlotKind::Value => self.expr(arg.value)?,
                SlotKind::Place => unreachable!("only a `var` parameter is a place"),
            }
        }
        if variadic {
            // The arguments left over are the elements of the one `Array`
            // the callee receives, so they are pushed left to right and
            // collected here rather than passed as arguments of their own.
            // That is the whole of the change at a call site: the callee
            // still gets one argument per parameter and the calling
            // convention does not move.
            //
            // They go onto the value stack whatever the checker settled
            // about each of them, because an `Array` holds `Value`s and
            // `Inst::MakeArray` reads that stack. Zero of them is an empty
            // `Array`, which is what `bind_params` builds when
            // `assign_labels` left it nothing.
            //
            // A label written in the variadic parameter's own place is one
            // element rather than a pile of leftovers, which is what
            // `bind_params` makes of `slots[index]`; a call that writes both
            // was refused above. `bind_params` reads that one through
            // `value_of` and never looks at its `spread`, so a `...` written
            // there is a marking nothing acts on and is refused rather than
            // spread.
            if let Some(position) = assigned.slots[names.len() - 1] {
                if args.at(position).spread {
                    return Err(no_spread_here(&what, args.at(position).span));
                }
            }
            let elements: Vec<CallArg<'a>> = assigned.slots[names.len() - 1]
                .into_iter()
                .chain(assigned.rest.iter().copied())
                .map(|position| args.at(position))
                .collect();
            if let Some(arg) = elements.iter().find(|arg| arg.is_var) {
                return Err(Unsupported::new(
                    format!("a `var` argument to `{what}`, which takes values"),
                    arg.span,
                ));
            }
            self.variadic_array(&elements, span)?;
            into(SlotKind::Value);
        }
        // An `async fn` answers a settled task whatever its return type
        // says, and a task is a value: `async fn f() -> Int` leaves a
        // `Task<Int>` on the value stack, and only `await` produces the
        // `Int`. `declared_function` settles `returns` the same way, and
        // `validate` reconciles the two.
        let answer = match decl.is_async {
            true => None,
            false => signature.and_then(|signature| scalar_of_ty(&signature.ret)),
        };
        // This is the whole of the reachability rule: the call being emitted
        // is what makes the target part of the program, so the target is
        // numbered here and nowhere else.
        let function = self.outer.number(Instance::Declared {
            key,
            supplied,
            as_value: false,
        });
        self.emit(
            Inst::Call {
                function,
                value_argc,
                scalar_argc,
                place_argc,
                returns_scalar: answer.is_some(),
            },
            span,
        );
        Ok(answer)
    }

    /// The one `Array` a variadic parameter receives, built out of the
    /// arguments that were left over.
    ///
    /// Without a `...` this is `Inst::MakeArray` over the elements as
    /// written, which is what it has always been. A `...` passes an existing
    /// sequence where those elements would go, so the array is built in runs
    /// instead: `MakeArray` for each run of ordinary arguments, the spread's
    /// own value for each `...`, and `Inst::SpreadArgument` to join each
    /// piece to what came before. The empty array it starts from is what
    /// `bind_params` starts from too, and a call with no leftovers at all
    /// still lowers to the single `MakeArray` it did before.
    ///
    /// The pieces are appended as each is produced rather than after all of
    /// them are, which is one instruction's worth of difference from
    /// `eval_args`: the interpreter evaluates every argument and then reads
    /// them in `bind_params`, so a spread of something that is neither an
    /// `Array` nor a `Vector` is reported after the arguments to its right
    /// have run. The checker reports that spread before either backend sees
    /// it — ``` `...` spreads an `Array` or a `Vector`, but found `Int` ```
    /// is a check-time diagnostic — so the order is unobservable in a
    /// checked program, and stating it is cheaper than an instruction that
    /// would have to carry which of its operands were spreads.
    fn variadic_array(&mut self, elements: &[CallArg<'a>], span: Span) -> Result<(), Unsupported> {
        if !elements.iter().any(|arg| arg.spread) {
            for arg in elements {
                self.expr(arg.value)?;
            }
            self.emit(Inst::MakeArray(elements.len() as u32), span);
            return Ok(());
        }
        self.emit(Inst::MakeArray(0), span);
        let mut at = 0;
        while at < elements.len() {
            if elements[at].spread {
                self.expr(elements[at].value)?;
                // The argument's own span, because this is the instruction
                // that reports a spread of something that is neither
                // sequence and `bind_params` reports it at `arg.span`.
                self.emit(Inst::SpreadArgument, elements[at].span);
                at += 1;
                continue;
            }
            let from = at;
            while at < elements.len() && !elements[at].spread {
                at += 1;
            }
            for arg in &elements[from..at] {
                self.expr(arg.value)?;
            }
            self.emit(Inst::MakeArray((at - from) as u32), span);
            // Appending an `Array` this instruction just built, which cannot
            // be the failure the span above is for.
            self.emit(Inst::SpreadArgument, span);
        }
        Ok(())
    }

    /// `console.println(...)` and `clock.now()`.
    fn call_host(
        &mut self,
        module: &str,
        op: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<(), Unsupported> {
        if let Some(declared) = hosts::module(module).and_then(|schema| schema.declared_type(op)) {
            return self.make_host_type(module, declared, args, span);
        }
        plain_arguments(args, op)?;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        let module = self.outer.name(module);
        let op = self.outer.name(op);
        self.emit(
            Inst::CallHost {
                module,
                op,
                argc: args.len() as u32,
            },
            span,
        );
        Ok(())
    }

    /// `http.Route(method: ..., path: ..., handler: ...)`: one value of a
    /// type a host module declares.
    ///
    /// This crosses no boundary. `Interpreter::init_host_type` is
    /// `init_struct` with the field names read from a `TypeSchema` instead of
    /// from a declaration — the same `assign_labels`, the same one value per
    /// field, and an ordinary `Value::Struct` whose `type_name` is
    /// `{module}.{Name}` and whose `opaque` is false — so it lowers to the
    /// instruction that builds an ordinary struct, with the qualified name
    /// the schema spells and the fields in the order the schema declares
    /// them. `is_opaque` answers false for it in the VM for the reason it
    /// answers false in the interpreter: no module of this package declares
    /// `Route`.
    ///
    /// An enum a host declares is not written this way — `http.Method.Get`
    /// is a case, and `init_host_type` reports the call as an error — so a
    /// call that names one is refused rather than built.
    ///
    /// Which types a module declares is read from the schema this crate can
    /// see, where the interpreter reads it from the `HostRegistry` the run
    /// was given. The two answer differently only for a registry that left a
    /// module out, which no runner builds: `cove run`, `cove test`, and
    /// `embed` each register every host and let the grants decide what a
    /// *call* may do. `Vm`'s own test harness registers every host for that
    /// reason and no other.
    fn make_host_type(
        &mut self,
        module: &str,
        declared: &'static TypeSchema,
        args: Args<'a>,
        span: Span,
    ) -> Result<(), Unsupported> {
        if declared.is_enum() {
            return Err(Unsupported::new(
                format!(
                    "`{module}.{}`, which is a host enum and not a function",
                    declared.name
                ),
                span,
            ));
        }
        let names: Vec<&str> = declared.fields.iter().map(|field| field.name).collect();
        every_argument_supplied(&names, args, declared.name, span)?;
        plain_arguments(args, declared.name)?;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        let ty = self.outer.name(&format!("{module}.{}", declared.name));
        let fields = self.outer.name(&names.join(","));
        self.emit(Inst::MakeStruct { ty, fields }, span);
        Ok(())
    }

    /// `Ok(...)`, `Err(...)`, `Some(...)`, `Error(...)`, `Shared(...)`,
    /// `assert(...)`, and `assertEqual(...)`, which is every free builtin
    /// there is.
    ///
    /// The two assertions carry their arguments' spans as well as their own.
    /// A failing `assert` quotes the source text of its condition — that is
    /// what makes it a builtin rather than a library function — and the
    /// instruction's own span covers the whole call, so the argument's span
    /// is recorded beside it in [`crate::Function::arg_spans`]. The
    /// interpreter reads exactly these spans, out of the same `SourceMap`.
    fn make_builtin(&mut self, name: &str, args: Args<'a>, span: Span) -> Result<(), Unsupported> {
        // `Shared` is here rather than beside the three `Result`/`Option`
        // constructors because it is the one that can refuse its payload:
        // what a cell wraps must be task-safe, since a `Shared` is reachable
        // from every task it was given to. `builtins::call_constructor` makes
        // that check, so both backends refuse the same payloads in the same
        // words.
        if !matches!(
            name,
            "Ok" | "Err" | "Some" | "Error" | "Shared" | "assert" | "assertEqual"
        ) {
            return Err(Unsupported::new(format!("`{name}`"), span));
        }
        let quotes_its_arguments = matches!(name, "assert" | "assertEqual");
        plain_arguments(args, name)?;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        let name = self.outer.name(name);
        let pc = self.code.len();
        self.emit(
            Inst::MakeBuiltin {
                name,
                argc: args.len() as u32,
            },
            span,
        );
        // `emit` keeps nothing where control cannot arrive, so the spans are
        // recorded against the instruction that was actually written.
        if quotes_its_arguments && self.code.len() > pc {
            self.arg_spans.insert(
                pc as u32,
                args.iter().map(|arg| arg.value.span).collect::<Vec<_>>(),
            );
        }
        Ok(())
    }

    /// `Cursor(at: 0, step: 1)`: a synthesized labelled call, whose values
    /// are pushed in the order the fields were declared.
    fn make_struct(
        &mut self,
        owner: &str,
        decl: &'a StructDecl,
        args: Args<'a>,
        span: Span,
    ) -> Result<(), Unsupported> {
        for field in &decl.fields {
            reject_dyn(&field.ty, "a `dyn` field")?;
        }
        let names: Vec<&str> = decl
            .fields
            .iter()
            .map(|field| field.name.node.as_str())
            .collect();
        every_argument_supplied(&names, args, &decl.name.node, span)?;
        plain_arguments(args, &decl.name.node)?;
        for (at, arg) in args.iter().enumerate() {
            self.expr(arg.value)?;
            // Each field's value is converted against the type the field was
            // written with, in the module that declares the struct rather
            // than the one initializing it — which is what
            // `Interpreter::init_struct` passes to `coerce`. The `at`th
            // argument fills the `at`th field because
            // `every_argument_supplied` accepted this call: every field has
            // an argument and the arguments fill them in increasing order.
            self.coerce_to(owner, &decl.fields[at].ty, arg.span);
        }
        let ty = self.outer.name(&format!("{owner}.{}", decl.name.node));
        let fields = self.outer.name(&names.join(","));
        self.emit(Inst::MakeStruct { ty, fields }, span);
        Ok(())
    }

    /// `Status.Confirmed` and `Json.Text(t)`: one case of a declared enum.
    ///
    /// The instruction carries the *qualified* type name, because that is
    /// what a case value holds — two modules may each declare a `Status`, and
    /// `Interpreter::enum_case` writes `{module}.{Enum}` into the value so
    /// that they stay two types.
    ///
    /// Whether the enum declares this case, and whether the payload is the
    /// length the case carries, are not asked here. `enum_case` asks them
    /// when the value is built and reports each in its own words, and the VM
    /// calls that same function; asking twice would be a second place for the
    /// answer to be written down.
    fn make_enum(
        &mut self,
        owner: &str,
        enum_name: &str,
        case: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<(), Unsupported> {
        plain_arguments(args, case)?;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        let ty = self.outer.name(&format!("{owner}.{enum_name}"));
        let case = self.outer.name(case);
        self.emit(
            Inst::MakeEnum {
                ty,
                case,
                argc: args.len() as u32,
            },
            span,
        );
        Ok(())
    }

    /// `Vector.of(...)`, `Int.parse(text)`, and the rest of
    /// `builtins::call_associated`.
    ///
    /// The arguments are pushed in the order they are written and nothing
    /// else is checked: the interpreter reaches these through `plain_values`,
    /// which reads an argument's value and never its label, so a variadic
    /// like `Vector.of` and a fixed one like `Int.parse` are the same shape
    /// here and their arity is the callee's to complain about.
    ///
    /// A name the type has no associated function for is emitted too, for the
    /// reason a missing enum case is: the failure belongs to the call, and
    /// the one function both backends dispatch through is where it is worded.
    fn call_builtin_assoc(
        &mut self,
        ty: &str,
        name: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<(), Unsupported> {
        plain_arguments(args, name)?;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        let ty = self.outer.name(ty);
        let name = self.outer.name(name);
        self.emit(
            Inst::CallBuiltinAssoc {
                ty,
                name,
                argc: args.len() as u32,
            },
            span,
        );
        Ok(())
    }

    /// `MapEntry(key: k, value: v)`, the one pair a `Map` is built from.
    ///
    /// It is a builtin *struct* rather than an associated function — nothing
    /// is called on the name, and `init_map_entry` builds a `StructValue`
    /// exactly as a declared struct's synthesized initializer does — so it
    /// lowers to the builtin that builds one, with its two fields pushed in
    /// declaration order. `assign_labels` is what the interpreter puts them
    /// in that order with, and [`arguments_in_order`] is the same rule read
    /// at lowering time.
    fn make_map_entry(&mut self, args: Args<'a>, span: Span) -> Result<(), Unsupported> {
        let names: Vec<&str> = builtins::MAP_ENTRY
            .fields
            .iter()
            .map(|field| field.name)
            .collect();
        every_argument_supplied(&names, args, builtins::MAP_ENTRY.name, span)?;
        plain_arguments(args, builtins::MAP_ENTRY.name)?;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        let name = self.outer.name(builtins::MAP_ENTRY.name);
        self.emit(
            Inst::MakeBuiltin {
                name,
                argc: args.len() as u32,
            },
            span,
        );
        Ok(())
    }

    /// The trait a call to `method` on a value of the type parameter
    /// `param` goes through, qualified, and nothing when no bound declares
    /// one.
    ///
    /// `Interpreter::eval_method_call` draws no distinction between a
    /// receiver whose static type is a trait object, a bounded type
    /// parameter, or the rigid `Self` of a trait's default body: it reads
    /// the concrete value's own type name and looks the method up from
    /// there. So this pass draws none either, and all it needs from the
    /// static type is which trait the call goes through.
    ///
    /// The first bound that declares the name is the one, which is the
    /// choice `cove_sema`'s `bound_method` makes over the same list in the
    /// same order.
    ///
    /// A parameter neither the declaration nor a trait default put in scope
    /// answers `None` — a type parameter of an `impl` block or of the struct
    /// it extends, and every parameter in scope around a lambda, which has
    /// no declaration of its own to have written one. The caller then falls
    /// through to the refusal it had, which is the honest answer: a receiver
    /// this pass cannot name a trait for is one it cannot collect the
    /// candidates of.
    fn bound_of(&self, param: &str, method: &str) -> Option<Arc<str>> {
        let written: Vec<&str> = match param {
            "Self" => self.self_bound.into_iter().collect(),
            _ => self
                .generics
                .iter()
                .find(|generic| generic.name.node == param)
                .map(|generic| generic.bounds.iter().map(|b| b.node.as_str()).collect())
                .unwrap_or_default(),
        };
        for bound in written {
            let qualified = self.outer.trait_named(self.module, bound);
            let declares = qualified.rsplit_once('.').is_some_and(|(module, short)| {
                self.outer
                    .checked
                    .modules
                    .get(module)
                    .and_then(|resolved| resolved.traits.get(short))
                    .is_some_and(|entry| entry.method(method).is_some())
            });
            if declares {
                return Some(qualified);
            }
        }
        None
    }

    /// `value.label()` where the receiver's static type says which trait the
    /// method comes from and nothing about which implementation: the one
    /// call in the language whose target is not knowable before the run.
    ///
    /// Three static types say that and are therefore one call here, as they
    /// are one call in `Interpreter::eval_method_call`: a `dyn Trait`, a
    /// type parameter bounded by the trait, and the rigid `Self` of that
    /// trait's own default body. What finds the implementation in each is
    /// the concrete value the receiver turns out to be — a run-time fact,
    /// and therefore a run-time lookup.
    /// [`Inst::CallDyn`] is that lookup. It is an instruction of its own
    /// rather than an [`Inst::Call`] with a target guessed at, which is what
    /// [issue #116](https://github.com/myuon/cove/issues/116) asks for: an
    /// operation whose target is not statically known says so, instead of
    /// hiding behind one that looks static.
    ///
    /// The arity is the *trait's*, not any one implementation's. A
    /// conformance's method must match the signature its trait declares —
    /// `cove_sema`'s `signature_difference` compares the receiver, the
    /// parameter names, the parameter types and the return type — so the
    /// trait's own declaration is the one thing every candidate agrees about
    /// and is what a call site can place its arguments by.
    fn call_dyn(
        &mut self,
        trait_name: &str,
        receiver: &'a Expr,
        name: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        // `Ty::Dyn` carries the trait qualified by the module that declares
        // it, which is the same name `Interpreter::coerce` builds and the
        // same one `Dispatch` is keyed by.
        let declared = trait_name.rsplit_once('.').and_then(|(module, short)| {
            let resolved = self.outer.checked.modules.get(module)?;
            resolved.traits.get(short)?.method(name)
        });
        let Some(declared) = declared else {
            return Err(Unsupported::new(
                format!("a call to `{name}`, which `{trait_name}` does not declare here"),
                span,
            ));
        };
        // A call through a `dyn` supplies a count and nothing else, exactly
        // as a call through a value does: there is no supplied-set for a
        // specialisation to be keyed by, and no callee in reach for a label
        // to be matched against. Both shapes are refused rather than
        // rearranged.
        plain_arguments(args, name)?;
        if let Some(arg) = args.iter().find(|arg| arg.label.is_some()) {
            return Err(Unsupported::new(
                format!("a labelled argument to `{name}`, which is called through a `dyn`"),
                arg.span,
            ));
        }
        if args.len() != declared.params.len() {
            return Err(Unsupported::new(
                format!(
                    "a call to `{name}` through `dyn {trait_name}` that supplies {} of its {} argument(s)",
                    args.len(),
                    declared.params.len()
                ),
                span,
            ));
        }
        if args.len() >= u16::MAX as usize {
            return Err(Unsupported::new(
                format!("a call to `{name}` with more than 65534 arguments"),
                span,
            ));
        }
        let site = self.outer.dispatch_site(trait_name, name);
        // The receiver first and then the arguments, left to right:
        // `Interpreter::eval_method_call` resolves the receiver before
        // `eval_args`, and the order two effects happen in is observable.
        self.expr(receiver)?;
        for arg in args.iter() {
            self.expr(arg.value)?;
        }
        self.emit(
            Inst::CallDyn {
                site,
                argc: args.len() as u16 + 1,
            },
            span,
        );
        // On the value stack, because no candidate's own signature is one
        // this call could have read a convention off.
        Ok(None)
    }

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
    fn scope_expr(
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
        self.emit(Inst::Try, span);
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
    fn spawn(
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
    fn lock(
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
    fn task_op(
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

    /// `receiver.name(...)`, where the receiver is a value.
    ///
    /// The interpreter tries a declared method of the receiver's *runtime*
    /// type first and falls back to the builtin table, so which of the two
    /// applies is a fact about the receiver — and the receiver's type is
    /// what the checker settled. Two answers follow from it, and the second
    /// is as much of the point as the first:
    ///
    /// - Where the checker recorded the declaration this call reaches, that
    ///   is the declaration, and nothing about the name is asked.
    /// - Where it settled the receiver's type and recorded no declaration,
    ///   this call reaches none: it is a builtin method, and a declared type
    ///   answering to the same name somewhere in the package is not what it
    ///   could have meant.
    ///
    /// Together those are why `impl Box { fn length(self) }` and
    /// `[1, 2, 3].length()` can now be written in one program. Both used to
    /// refuse — the first because a builtin shares the name, the second
    /// because a declared type does — and a name was all there was to tell
    /// them apart, which is not enough.
    ///
    /// A receiver the checker abstained about, or one it never walked, is
    /// still resolved by name and still refuses what a name cannot settle.
    /// Guessing there is the one mistake a second backend must not make:
    /// `[1, 2, 3].length()` is the builtin's `3` on the oracle, and a `Call`
    /// to a declared `Box.length` is a different program.
    fn method_call(
        &mut self,
        id: ExprId,
        receiver: &'a Expr,
        name: &str,
        args: Args<'a>,
        span: Span,
    ) -> Result<Option<Scalar>, Unsupported> {
        // Before anything is asked about the name, because a recorded target
        // makes every one of those questions moot: which types declare this
        // name, whether a builtin shares it, and whether the builtin that
        // shares it writes through its receiver are all questions about
        // *which* declaration is meant, and the checker has answered that.
        let recorded = self.target(id, span);
        if let Some(key) = recorded.and_then(|target| self.declared_by(target)) {
            return self.call_declared(key, Some(receiver), args, span);
        }
        // A `dyn Trait` receiver dispatches from the value it carries rather
        // than from its static type, which is the whole of what makes the
        // dispatch dynamic. Asked before every question below for the reason
        // `Interpreter::eval_method_call` unwraps its receiver before it asks
        // any of its own: none of them is a question about a trait object,
        // and the checker records no target for one — a call through a trait
        // reaches a declaration the call site cannot name.
        if let Some(Ty::Dyn(trait_name)) = self.settled(receiver) {
            // `Facts::ty` holds the type as the checker held it while it
            // walked this body, and there a trait the module declares is
            // named bare while an imported one already carries the module it
            // came from — `cove_sema`'s `qualified_name`, which is what
            // `Signature` publishes after applying it and what a `dyn` value
            // carries. `trait_named` applies the same rule from the same
            // tables, and leaves a name that already carries a module alone.
            let qualified = self.outer.trait_named(self.module, trait_name);
            return self.call_dyn(&qualified, receiver, name, args, span);
        }
        // A bounded type parameter is the same call. The checker resolves
        // the *signature* through the bound and the run resolves the
        // *implementation* through the value, exactly as it does for a
        // trait object, and `Interpreter::eval_method_call` runs one code
        // path for both. `Self` inside a trait's default body is the same
        // again, which is why a dispatch through a `dyn` to a method the
        // conformance did not write reaches this.
        if let Some(Ty::Param(param)) = self.settled(receiver) {
            if let Some(qualified) = self.bound_of(param, name) {
                return self.call_dyn(&qualified, receiver, name, args, span);
            }
        }
        // A resource handle's methods belong to the host that issued it, so
        // they are dispatched through the boundary rather than looked up in
        // the package — the rule `Interpreter::call_builtin_method` states
        // and dispatches by, asked here of what the checker settled instead
        // of what a value turned out to be. It is asked before any of the
        // questions below for the reason the interpreter asks it before its
        // own: none of them is about a handle, and a name a host answers is
        // not a name this package or the builtins have to share.
        if self.resource_op(receiver, name) {
            plain_arguments(args, name)?;
            // The receiver first and then the arguments, left to right:
            // `Interpreter::eval_method_call` evaluates the receiver before
            // `eval_args`, and the order two effects happen in is observable.
            self.expr(receiver)?;
            for arg in args.iter() {
                self.expr(arg.value)?;
            }
            let op = self.outer.name(name);
            self.emit(
                Inst::CallResource {
                    op,
                    argc: args.len() as u32,
                },
                span,
            );
            // On the value stack, whatever the schema says the operation
            // answers, exactly as `Inst::CallHost` leaves a host call's
            // answer.
            return Ok(None);
        }
        // The operations of a task scope and of a task handle, dispatched by
        // the type the checker settled for the receiver where
        // `Interpreter::call_task_method` dispatches by the value's own kind.
        // Asked before the builtins for the reason the interpreter asks them
        // before its own: a scope and a handle are runtime values that no
        // declaration and no builtin answers for, and `spawn`, `await` and
        // `cancel` are not names a builtin shares.
        match self.settled(receiver) {
            Some(Ty::Scope) if name == "spawn" => return self.spawn(receiver, args, span),
            Some(Ty::Task(_)) if name == "await" => {
                return self.task_op(receiver, Inst::Await, "await", args, span)
            }
            Some(Ty::Task(_)) if name == "cancel" => {
                return self.task_op(receiver, Inst::Cancel, "cancel", args, span)
            }
            Some(Ty::Shared(_)) if name == "lock" => return self.lock(receiver, args, span),
            _ => {}
        }
        if name == "await" {
            return Err(Unsupported::new("an `await`", span));
        }
        if name == "snapshot" {
            // A struct or an enum with an `impl Snapshot for Type` never
            // reaches here: the checker recorded which declaration that call
            // means, and the recorded target above took it. What is left is
            // the half of the trait no conformance answers for, and it is
            // emitted only where the checker settled a type that cannot
            // reach one — see `snapshot_without_a_conformance`, which is
            // where the receiver decides and not the name.
            let Some(ty) = self.settled(receiver) else {
                return Err(Unsupported::new(
                    "`snapshot` on a receiver whose type nothing settled",
                    span,
                ));
            };
            if !snapshot_without_a_conformance(ty) {
                return Err(Unsupported::new(
                    format!("`snapshot` on a `{}`, which a conformance answers", ty),
                    span,
                ));
            }
            if !args.is_empty() {
                // `snapshot` takes none, and `Interpreter::eval_method_call`
                // says so before it reads the receiver; refusing keeps the
                // instruction's shape a fact rather than something a call
                // site could vary.
                return Err(Unsupported::new("`snapshot` given arguments", span));
            }
            self.expr(receiver)?;
            self.emit(Inst::Snapshot, span);
            return Ok(None);
        }
        // `freeze` is the one builtin that needs the place rather than a read
        // of it. `builtins::freeze` counts the handles to the storage and
        // refuses when the count is not one, and a read of the receiver would
        // be the second handle — which is why
        // `Interpreter::call_builtin_method` runs it inside `place.with_mut`
        // and why `Inst::Freeze` takes a place.
        //
        // A receiver that is not a place at all falls through to the ordinary
        // builtin lowering below, exactly as it does in the interpreter:
        // `Vector.of(1).freeze()` has no place, and `builtins::call_method`'s
        // own `freeze` arm answers it from the temporary — which holds the
        // only handle there is. `push` falls through as well, whatever its
        // receiver: it mutates through the handle a `Vector` is, so there is
        // nothing to write back to the receiver's slot. That the receiver of
        // a mutating method is a place, and a writable one, is `cove-sema`'s
        // to say and it has said it (ADR 0021).
        if name == "freeze" && self.is_a_place(receiver) {
            if !args.is_empty() {
                // `freeze()` takes none, and the checker says so before this
                // does; refusing keeps the instruction's shape a fact rather
                // than something a call site could vary.
                return Err(Unsupported::new("`freeze` given arguments", span));
            }
            self.place(receiver)?;
            self.emit(Inst::Freeze, span);
            return Ok(None);
        }
        // Which types declare a method of this name is a question for the
        // shared table rather than for a list written here, so a builtin
        // that gains a method gains this refusal with it.
        let builtin_method = builtins::builtins()
            .iter()
            .any(|schema| schema.method(name).is_some());
        // Only the methods this module could be handed a receiver for, and
        // only where a name is still all there is to go on. A receiver whose
        // type the checker settled and recorded no target for has already
        // been decided about — the target above would have named a
        // declaration if the call reached one — so there is no candidate
        // here and a name two types share stops being ambiguous.
        //
        // Three cases are not that. `Unknown` is the checker saying it did
        // not prove this and `Never` is a receiver that produces no value,
        // so neither settles which method a call reaches; a receiver the
        // checker never walked settles nothing either. And a target it
        // *did* record that this pass could not find a declaration for is
        // an answer nobody here can act on, which leaves the name.
        let by_name_is_all_there_is = recorded.is_some()
            || matches!(
                self.settled(receiver),
                None | Some(Ty::Unknown(_)) | Some(Ty::Never)
            );
        let candidates: Vec<Key> = if by_name_is_all_there_is {
            self.outer
                .by_name
                .get(name)
                .map(|all| {
                    all.iter()
                        .copied()
                        .filter(|key| self.outer.could_dispatch(self.module, *key))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        if !candidates.is_empty() {
            if candidates.len() > 1 {
                return Err(Unsupported::new(
                    format!("a call to `{name}`, which more than one type declares"),
                    span,
                ));
            }
            if builtin_method {
                return Err(Unsupported::new(
                    format!(
                        "a call to `{name}`, which a builtin type and a declared type both have"
                    ),
                    span,
                ));
            }
            let key = candidates[0];
            return self.call_declared(key, Some(receiver), args, span);
        }
        if builtin_method {
            plain_arguments(args, name)?;
            self.expr(receiver)?;
            for arg in args.iter() {
                self.expr(arg.value)?;
            }
            let name = self.outer.name(name);
            self.emit(
                Inst::CallBuiltin {
                    name,
                    argc: args.len() as u32,
                },
                span,
            );
            // A builtin method answers on the value stack, whatever its type:
            // `call_method` is the interpreter's and hands back a `Value`.
            return Ok(None);
        }
        Err(Unsupported::new(
            format!("a call to `{name}`, which no declared type and no builtin has"),
            span,
        ))
    }
}

/// Which parameter each argument fills, refusing every shape whose answer
/// the lowering would have to rearrange.
///
/// This is `assign_labels` in the interpreter, asked before the run instead
/// of during it. That function matches a positional argument to the next
/// parameter not yet filled, matches a label to the parameter of that name,
/// refuses a label whose parameter stands before one already filled, and
/// refuses a positional argument after a labelled one. What survives is a
/// call whose arguments fill parameters in strictly increasing order — which
/// is what makes pushing them left to right the same as pushing them in
/// declaration order.
///
/// A parameter no argument fills is left to its default, and a default is
/// evaluated by the callee, so it is not this function's business beyond
/// saying which ones they are. `Body::call_declared` reads that from
/// [`Arguments::slots`] and specialises the callee; a parameter with no
/// default to fall back on is what is reported here, in the words
/// `bind_params` reports it in.
///
/// `variadic` says the last parameter takes every argument left over, which
/// changes two of the three questions and neither of the others. There is no
/// longer a most: `assign_labels` puts a positional argument past the last
/// parameter into `rest` rather than reporting one too many. And a variadic
/// parameter is never missing, since one given nothing is an empty `Array`.
///
/// The out-of-order case is the checker's, not this pass's. `cove-sema`
/// reports `cove::type::label_order` for a label whose parameter stands
/// before one an earlier argument already filled, so no checked program
/// reaches here with one — and this still refuses it, because
/// [`Arguments::slots`] is read by call sites that push arguments left to
/// right and the property they rely on is worth stating where it is relied
/// on rather than assumed from somewhere else. ADR 0021 is why the two are
/// not the same kind of statement: the checker's is a language rule and this
/// is an invariant of a calling convention.
///
/// The surprising case is the one refused by name. `assign_labels` will
/// accept `f(1, 2, items: 3)` and bind `items` to `[3, 2]` — the labelled
/// argument first and the ones that fell into `rest` after it, which is also
/// what the checker's `match_arguments` does — and a lowering that pushed
/// those left to right would have them the other way round. Rather than
/// rearrange them, a variadic parameter that is written with a label *and*
/// collects leftovers is reported.
fn arguments_in_order(
    names: &[&str],
    args: Args<'_>,
    what: &str,
    variadic: bool,
    span: Span,
) -> Result<Arguments, Unsupported> {
    let mut slots: Vec<Option<usize>> = vec![None; names.len()];
    let mut rest: Vec<usize> = Vec::new();
    let mut next = 0usize;
    let mut labelled = false;
    for (position, arg) in args.iter().enumerate() {
        match &arg.label {
            Some(label) => {
                labelled = true;
                let Some(index) = names.iter().position(|name| *name == label.node) else {
                    return Err(Unsupported::new(
                        format!("`{what}`, which has no parameter labelled `{}`", label.node),
                        arg.span,
                    ));
                };
                if index < next {
                    return Err(Unsupported::new(
                        format!(
                            "a call to `{what}` whose arguments do not stand in declaration order"
                        ),
                        arg.span,
                    ));
                }
                slots[index] = Some(position);
                next = index + 1;
            }
            None => {
                if labelled {
                    return Err(Unsupported::new(
                        format!(
                            "a call to `{what}` with a positional argument after a labelled one"
                        ),
                        arg.span,
                    ));
                }
                if variadic && next + 1 >= names.len() {
                    rest.push(position);
                } else if next < names.len() {
                    slots[next] = Some(position);
                    next += 1;
                } else {
                    return Err(Unsupported::new(
                        format!("a call to `{what}` with more arguments than it has parameters"),
                        arg.span,
                    ));
                }
            }
        }
    }
    if variadic && !rest.is_empty() && slots[names.len() - 1].is_some() {
        return Err(Unsupported::new(
            format!("a call to `{what}` that labels its variadic parameter and passes more"),
            span,
        ));
    }
    Ok(Arguments { slots, rest })
}

/// A call's arguments: the ones written inside the parentheses, and the
/// trailing closure written after them.
///
/// `f(x) { ... }` is sugar and nothing more. `Interpreter::eval_args`
/// evaluates the written arguments left to right and then pushes the
/// trailing one on the end as an unlabelled, non-`var`, non-spread argument
/// — so a trailing closure *is* the last positional argument, and the whole
/// of what this type does is let every path that reads a call's arguments
/// say so once instead of each of them taking a second parameter it would
/// have to remember to use.
///
/// The parser has already built the block as an `ExprKind::Lambda` with no
/// parameters, so the value is an ordinary expression here and lowers
/// through the ordinary lambda path.
#[derive(Clone, Copy)]
struct Args<'a> {
    written: &'a [Arg],
    trailing: Option<&'a Expr>,
}

/// One argument of a call, whichever side of the parentheses it was written
/// on.
///
/// A written one is its [`Arg`] read field by field; a trailing one is the
/// expression with the four answers `eval_args` gives it — no label, not
/// `var`, not a spread, and its own span.
#[derive(Clone, Copy)]
struct CallArg<'a> {
    label: Option<&'a cove_syntax::ast::Ident>,
    is_var: bool,
    spread: bool,
    value: &'a Expr,
    span: Span,
}

impl<'a> Args<'a> {
    /// The arguments written inside the parentheses, and the trailing
    /// closure when one was written.
    fn new(written: &'a [Arg], trailing: Option<&'a Expr>) -> Args<'a> {
        Args { written, trailing }
    }

    fn len(self) -> usize {
        self.written.len() + usize::from(self.trailing.is_some())
    }

    fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The argument at `position`, where the trailing closure is the one
    /// past the written ones.
    fn at(self, position: usize) -> CallArg<'a> {
        match self.written.get(position) {
            Some(arg) => CallArg {
                label: arg.label.as_ref(),
                is_var: arg.is_var,
                spread: arg.spread,
                value: &arg.value,
                span: arg.span,
            },
            None => {
                let trailing = self
                    .trailing
                    .expect("a position past the written arguments is the trailing closure");
                CallArg {
                    label: None,
                    is_var: false,
                    spread: false,
                    value: trailing,
                    span: trailing.span,
                }
            }
        }
    }

    fn iter(self) -> impl Iterator<Item = CallArg<'a>> {
        (0..self.len()).map(move |position| self.at(position))
    }
}

/// Which argument fills each parameter, and which arguments a variadic
/// parameter collects.
struct Arguments {
    /// For each parameter, the position of the argument that fills it, or
    /// `None` where the call left it to its default.
    slots: Vec<Option<usize>>,
    /// The positions of the arguments that fell past the last parameter, in
    /// the order they are written, which a variadic parameter collects.
    rest: Vec<usize>,
}

/// The rule [`arguments_in_order`] states, for a call that admits no default
/// and no variadic parameter.
///
/// A struct's synthesized initializer, `MapEntry`, and a type a host module
/// declares are all of that shape: every field is written or the call is
/// wrong, and the interpreter says so with its own words rather than with a
/// default it does not have.
fn every_argument_supplied(
    names: &[&str],
    args: Args<'_>,
    what: &str,
    span: Span,
) -> Result<(), Unsupported> {
    let assigned = arguments_in_order(names, args, what, false, span)?;
    if assigned.slots.iter().any(Option::is_none) {
        return Err(Unsupported::new(
            format!("a call to `{what}` that does not supply one argument for every parameter"),
            span,
        ));
    }
    Ok(())
}

/// Whether `Interpreter::snapshot` answers about a value of this type
/// without reaching a declared conformance.
///
/// The interpreter dispatches a `Value::Struct`, a `Value::Enum` and a
/// `Value::Dyn` to an `impl Snapshot for Type`, and answers everything else
/// itself. An instruction cannot run a whole Cove function in the middle of
/// itself, so the VM covers the second half and this is the question that
/// decides which half a receiver is in.
///
/// An `Array`, a `Map` and a `Set` are cloned rather than walked — each is
/// immutable, so there is nothing inside one for a copy to separate — and
/// that is why their element types are not asked about. A `Vector` *is*
/// walked, one element at a time, so a `Vector<T>` is only in this half when
/// `T` is: a `Vector<Booking>` dispatches once per element and is refused.
///
/// An abstention is not an answer, and neither is [`Ty::Unknown`]. Both are
/// refused by the caller, which asks for a settled type before asking this.
fn snapshot_without_a_conformance(ty: &Ty) -> bool {
    match ty {
        Ty::Unit
        | Ty::Bool
        | Ty::Int
        | Ty::Float
        | Ty::Str
        | Ty::Duration
        | Ty::Range
        | Ty::Array(_)
        | Ty::Map(_, _)
        | Ty::Set(_) => true,
        Ty::Vector(inner) => snapshot_without_a_conformance(inner),
        _ => false,
    }
}

/// A `var` argument written where the parameter is not declared `var`, or a
/// parameter declared `var` given an argument that is not.
///
/// The interpreter refuses both at run time, in `bind_params`, and it
/// refuses them because the marking is deliberately at both ends: "A `var`
/// parameter is a non-escaping inout alias, marked at both the declaration
/// and the call site." A checked program should not reach either message,
/// and a backend that quietly accepted one would be more permissive than the
/// oracle.
fn var_marking_disagrees(what: &str, param: &str, declared_var: bool, span: Span) -> Unsupported {
    Unsupported::new(
        match declared_var {
            true => format!("a call to `{what}`, whose parameter `{param}` is declared `var` and whose argument is not written `var`"),
            false => format!("a call to `{what}`, whose parameter `{param}` is not declared `var` and whose argument is written `var`"),
        },
        span,
    )
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

/// Neither marking a call site can write, at a call this backend does not
/// route through a declared function's parameters.
///
/// A struct initializer, a host operation, an enum case, a builtin, and a
/// builtin's associated function all take values. None of them declares a
/// `var` parameter, so `var` written at one is a program the interpreter
/// refuses too; and none of them collects a variadic parameter's elements,
/// so a `...` written at one is a marking the interpreter *ignores* — which
/// is refused here instead. See [`no_spread_here`].
fn plain_arguments(args: Args<'_>, what: &str) -> Result<(), Unsupported> {
    if let Some(arg) = args.iter().find(|arg| arg.is_var) {
        return Err(Unsupported::new(
            format!("a `var` argument to `{what}`, which takes values"),
            arg.span,
        ));
    }
    if let Some(arg) = args.iter().find(|arg| arg.spread) {
        return Err(no_spread_here(what, arg.span));
    }
    Ok(())
}

/// A `...` written where nothing collects the elements it would spread.
///
/// Only a variadic parameter of a declared function does. Everywhere else
/// the interpreter reads the argument's *value* and never its `spread` flag:
/// `println(...["a"])` hands `console.println` one `Array` and fails against
/// the schema, and `k(...[1, 2, 3])` binds the whole array to `k`'s one
/// parameter. Refusing rather than reproducing that is the direction a
/// second backend is allowed to be wrong in, and it keeps the flag from
/// being carried through paths that would have to ignore it.
fn no_spread_here(what: &str, span: Span) -> Unsupported {
    Unsupported::new(
        format!("a `...` spread argument to `{what}`, which collects nothing"),
        span,
    )
}

/// A call that answers on the value stack, whatever it produced.
///
/// Everything a call can lower to other than [`Inst::Call`] hands back a
/// `Value`: a builtin method, a host operation, a struct initializer, an
/// enum case, an assertion, and a builtin type's associated function are all
/// the interpreter's own code, and the interpreter speaks `Value`. Saying so
/// through one function keeps `Body::call_declared` the only place where a
/// call's answer can be anything else.
fn on_the_value_stack(lowered: Result<(), Unsupported>) -> Result<Option<Scalar>, Unsupported> {
    lowered.map(|()| None)
}
