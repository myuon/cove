//! What both code generators compile, decided once.
//!
//! Two arms compile Cove IR in this crate — Cranelift under the `cranelift`
//! feature and a hand-written x86-64 template compiler under `template` — and
//! the
//! whole point of having two is to measure one against the other. A
//! measurement over two different subsets of the IR would not be that
//! measurement, so the subset is not written twice: [`supported`] is the one
//! predicate both arms ask, and [`leaders`] is the one block partition both
//! arms charge work over.
//!
//! [ADR 0055]: ../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md

use cove_ir::{ArithOp, CmpOp, Compare, Function, Inst, Num, Program, Repr, Slot};

use crate::abi::Raise;

/// The widest [`Inst::Copy`] this slice lowers, in words.
///
/// A copy is emitted as a run of loads and then a run of stores — see
/// each arm's `copy` for why it is in that order — so the code it produces is
/// linear in the width and there is no memmove helper to fall back to yet. A
/// bound is therefore worth having, and it is deliberately generous: sixteen
/// words is a wider inline value than anything the corpus lowers.
const MAX_COPY_WORDS: u32 = 16;

/// Whether a slot of this `Repr` is one this slice will touch.
///
/// The scalars, and deliberately not [`Repr::Ref`] — see [`crate::abi`]'s
/// "There are no references here yet". [`Repr::Addr`], [`Host`](Repr::Host),
/// [`Task`](Repr::Task) and [`Scope`](Repr::Scope) are excluded for a
/// different reason: they are not roots, so they are not a collector problem,
/// but every operation that produces or consumes one is a runtime call this
/// slice does not lower, so a frame holding one is a frame whose function
/// will be refused anyway.
///
/// [`Repr::Float`] is admitted although no float *operation* is lowered. A
/// float slot that is only copied is a run of bits like any other, and
/// refusing the whole function because one of its frame slots is a `Float`
/// would refuse it for a reason that is not true.
fn is_scalar(repr: Repr) -> bool {
    match repr {
        Repr::Unit | Repr::Bool | Repr::Int | Repr::Float | Repr::Duration | Repr::Tag => true,
        Repr::Ref | Repr::Addr | Repr::Host | Repr::Task | Repr::Scope => false,
    }
}

/// The byte offset of a slot from the frame's first word, if it fits the
/// `i32` displacement both arms address a frame with.
///
/// A frame is bounded by `cove_ir::MAX_FRAME_WORDS`, which is far inside
/// this, so the `None` is unreachable in practice. It is checked rather than
/// asserted because "unreachable in practice" is a claim about today's
/// constant.
pub(crate) fn slot_offset(slot: Slot) -> Option<i32> {
    i32::try_from(i64::from(slot) * 8).ok()
}

/// Whether a comparison is one this slice lowers.
///
/// [`Compare::Int`] takes all six operators, as
/// `encoded.rs`'s `cmp_int!` does. [`Compare::Bool`] takes equality only,
/// which is the same division `encoded.rs` makes at its `EQ_BOOL`/`NE_BOOL`
/// arms against the `LT_BOOL | LE_BOOL | GT_BOOL | GE_BOOL => not_ordered!()`
/// arm beside them — an ordered comparison of `Bool` is a runtime error, and
/// emitting one would be lowering a refusal. Refusing the function instead
/// leaves it to the tier that already has the message.
///
/// Everything else — [`Compare::Float`], [`Str`](Compare::Str),
/// [`Identity`](Compare::Identity), [`Tag`](Compare::Tag) — is outside the
/// slice. `Identity` and `Tag` would each be one integer comparison, but
/// `Identity` reads a [`Repr::Ref`] word and `Tag` a [`Repr::Tag`] one, and
/// this slice's claim that it never touches a reference is worth more than
/// two instructions.
fn comparison_supported(on: Compare, op: CmpOp) -> bool {
    match on {
        Compare::Int => true,
        Compare::Bool => matches!(op, CmpOp::Eq | CmpOp::Ne),
        Compare::Float | Compare::Str | Compare::Identity | Compare::Tag => false,
    }
}

/// Whether every part of `function` is inside this slice.
///
/// Called before lowering begins, which is what makes lowering infallible.
/// The instruction match here and each arm's `inst` are two halves of one
/// decision and have to agree: a form admitted here and not lowered there is
/// a panic, which is why that arm is `unreachable!` and says so.
pub(crate) fn supported(program: &Program, function: &Function) -> bool {
    if !function.reprs.iter().copied().all(is_scalar) {
        return false;
    }
    // The verifier requires it, and the lowering depends on it: a function
    // whose last instruction is not a terminator would fall off the end of
    // its last basic block, and there is nowhere for it to fall to.
    if !matches!(
        function.code.last(),
        Some(Inst::Return { .. } | Inst::Jump { .. } | Inst::Trap { .. })
    ) {
        return false;
    }
    function
        .code
        .iter()
        .all(|inst| inst_supported(program, function, inst))
}

fn inst_supported(program: &Program, function: &Function, inst: &Inst) -> bool {
    let slots = function.reprs.len();
    let end = function.code.len() as u32;
    let slot = |at: Slot| (at as usize) < slots && slot_offset(at).is_some();
    let run = |at: Slot, width: u32| {
        at.checked_add(width)
            .is_some_and(|last| (last as usize) <= slots)
            && slot_offset(at.saturating_add(width)).is_some()
    };
    match inst {
        Inst::Bool { dst, .. } | Inst::Int { dst, .. } => slot(*dst),
        Inst::Copy { dst, src, layout } => {
            let layout = program.layout(*layout);
            layout.width() <= MAX_COPY_WORDS
                && layout.words.iter().copied().all(is_scalar)
                && run(*dst, layout.width())
                && run(*src, layout.width())
        }
        Inst::Arith {
            num: Num::Int,
            dst,
            a,
            b,
            ..
        } => slot(*dst) && slot(*a) && slot(*b),
        Inst::ArithImm { dst, a, .. } => slot(*dst) && slot(*a),
        Inst::Cmp { on, op, dst, a, b } => {
            comparison_supported(*on, *op) && slot(*dst) && slot(*a) && slot(*b)
        }
        Inst::CmpImm { dst, a, .. } => slot(*dst) && slot(*a),
        Inst::CmpBranch {
            on,
            op,
            dst,
            a,
            b,
            target,
        } => comparison_supported(*on, *op) && slot(*dst) && slot(*a) && slot(*b) && *target < end,
        Inst::CmpImmBranch { dst, a, target, .. } => slot(*dst) && slot(*a) && *target < end,
        Inst::Jump { to } => *to < end,
        Inst::BranchFalse { cond, to } => slot(*cond) && *to < end,
        Inst::Return { src } => run(*src, program.layout(function.returns).width()),
        Inst::Trap { .. } => true,
        _ => false,
    }
}

/// Where a basic block begins, and how many instructions it holds.
///
/// A block is ADR 0055's unit of work accounting and safepoint placement, so
/// this is not only a code-generation convenience: the static instruction
/// count of a block is the charge added to the work accumulator when the
/// block is entered.
///
/// A leader is the first instruction, any branch or jump target, and the
/// instruction after any terminator. That last clause is what makes a
/// conditional branch's fall-through a block of its own, which Cranelift
/// needs because its `brif` names both successors explicitly.
pub(crate) fn leaders(function: &Function) -> Vec<Option<u32>> {
    let end = function.code.len();
    let mut leader = vec![false; end];
    if end > 0 {
        leader[0] = true;
    }
    fn mark(leader: &mut [bool], at: usize) {
        if at < leader.len() {
            leader[at] = true;
        }
    }
    for (pc, inst) in function.code.iter().enumerate() {
        match inst {
            Inst::Jump { to } => {
                mark(&mut leader, *to as usize);
                mark(&mut leader, pc + 1);
            }
            Inst::BranchFalse { to, .. } => {
                mark(&mut leader, *to as usize);
                mark(&mut leader, pc + 1);
            }
            Inst::CmpBranch { target, .. } | Inst::CmpImmBranch { target, .. } => {
                mark(&mut leader, *target as usize);
                mark(&mut leader, pc + 1);
            }
            Inst::Return { .. } | Inst::Trap { .. } => mark(&mut leader, pc + 1),
            _ => {}
        }
    }
    // Rewritten as "how long is the block starting here", counting forward to
    // the next leader, so the work charge is one lookup at block entry.
    let mut lengths = vec![None; end];
    let mut at = end;
    for pc in (0..end).rev() {
        if leader[pc] {
            lengths[pc] = Some((at - pc) as u32);
            at = pc;
        }
    }
    lengths
}

/// Which overflow `int_arith` would name for `op` writing to `dst`.
///
/// `int_arith`'s `named` closure answers "duration arithmetic" instead of the
/// operation's own name when the destination is a [`Repr::Duration`] — the
/// question `encoded.rs` asks as `machine.repr(id, a!()) ==
/// Some(Repr::Duration)`. Here the destination's `Repr` is a static fact, so
/// the question is asked once, at compile time, and the answer is a constant in
/// the generated code.
///
/// It is asked only of addition, subtraction and multiplication, because those
/// are the three arms of `int_arith` that call `named`. Division and remainder
/// name themselves whatever the destination is.
///
/// Shared by both arms for the same reason [`supported`] is: this is a rule of
/// the *language*, and two code generators disagreeing about it would be two
/// different languages.
pub(crate) fn overflow_of(function: &Function, op: ArithOp, dst: Slot) -> Raise {
    let duration = function.reprs.get(dst as usize) == Some(&Repr::Duration);
    match op {
        ArithOp::Add if duration => Raise::DurationOverflowed,
        ArithOp::Sub if duration => Raise::DurationOverflowed,
        ArithOp::Mul if duration => Raise::DurationOverflowed,
        ArithOp::Add => Raise::AddOverflowed,
        ArithOp::Sub => Raise::SubOverflowed,
        ArithOp::Mul => Raise::MulOverflowed,
        ArithOp::Div => Raise::DivOverflowed,
        ArithOp::Rem => Raise::RemOverflowed,
    }
}

/// Which "divided by zero" a division-shaped operation names.
///
/// `int_arith`'s `Div` and `Rem` arms test the divisor before they divide, and
/// they name the operation they are: `divided_by_zero("division")` and
/// `divided_by_zero("remainder")`.
pub(crate) fn by_zero_of(op: ArithOp) -> Raise {
    match op {
        ArithOp::Rem => Raise::RemainderByZero,
        _ => Raise::DividedByZero,
    }
}
