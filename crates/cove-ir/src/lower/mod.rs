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
//! [`Inst::GetFieldAt`]: crate::Inst::GetFieldAt
//! [`Inst::IntBinary`]: crate::Inst::IntBinary
//! [`Inst::JumpIfFalseScalar`]: crate::Inst::JumpIfFalseScalar
//! [`Inst::LoadScalar`]: crate::Inst::LoadScalar
//! [`Inst::ReturnScalar`]: crate::Inst::ReturnScalar
//! [`Inst::ScalarConst`]: crate::Inst::ScalarConst
//! [`Inst::ScalarToValue`]: crate::Inst::ScalarToValue
//! [`Inst::StoreScalar`]: crate::Inst::StoreScalar
//! [`Inst::ValueToScalar`]: crate::Inst::ValueToScalar
//! [`SlotKind::Scalar`]: crate::SlotKind::Scalar

mod body;
mod call;
mod convention;
mod dispatch;
mod expr;
mod fuel;
mod index;
mod scan;
mod task;
mod validate;

#[cfg(test)]
mod tests;

use cove_diag::FileId;
use cove_diag::Span;
use cove_sema::resolve::Program as Checked;

use crate::{FunctionId, Program, Unsupported};

use index::{Instance, Key, Lowering};

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
///
/// [`Inst::Call`]: crate::Inst::Call
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
