//! The block table the VM charges fuel from.
//!
//! A pass over finished code rather than something the lowering threads
//! through itself: a block's boundaries are readable from the instructions
//! alone, so deriving them here keeps every emitter of a jump from having to
//! know that the VM charges by the block.

use crate::Inst;

/// Whether an instruction is the last one of a straight line: after it,
/// control is somewhere the next index does not name.
///
/// The five jumps go elsewhere or fall through, [`Inst::Call`],
/// [`Inst::CallValue`] and [`Inst::CallDyn`] run a whole callee in between,
/// [`Inst::Try`] may leave the frame instead of continuing, and
/// [`Inst::Return`], [`Inst::ReturnScalar`] and [`Inst::NoMatch`] do not
/// continue at all.
pub(super) fn ends_a_block(inst: Inst) -> bool {
    matches!(
        inst,
        Inst::Jump(_)
            | Inst::JumpIfFalse(_)
            | Inst::JumpIfTrue(_)
            | Inst::JumpIfFalseScalar(_)
            | Inst::JumpIfTrueScalar(_)
            | Inst::Call { .. }
            | Inst::CallValue { .. }
            | Inst::CallDyn { .. }
            | Inst::Try { .. }
            | Inst::Return
            | Inst::ReturnScalar
            | Inst::NoMatch
    )
}

/// How many instructions run from each index control can *arrive* at before
/// it can go somewhere else, and 0 at every index it cannot arrive at.
///
/// This is [`Function::block_fuel`], and it is a pass over finished code
/// rather than something the lowering threads through itself: a block's
/// boundaries are readable from the instructions alone, so deriving them here
/// keeps every emitter of a jump from having to know that the VM charges by
/// the block.
///
/// # Arrival, not partition
///
/// The obvious reading — cut the code at every head and let the pieces tile
/// it — is wrong, and wrong in a way that silently loses instructions. An
/// `if` with no `else` inside a loop lowers to a body that *falls* into the
/// join its own conditional jump also targets. The join is a head, because a
/// jump lands on it; but control also reaches it by walking off the end of
/// the block above, and nothing about that walk announces itself. A VM that
/// charged a head only where it jumped to one would never charge that join,
/// and the instructions after it would run for free.
///
/// So a count here is an *extent* and the counts overlap: `block_fuel[h]` is
/// how far the straight line beginning at `h` runs — to the first instruction
/// at or after `h` that ends a block, inclusive. Falling from one head
/// into another is then already paid for, because the extent of the first
/// reaches past the second and out to the same terminator. Jumping to the
/// second pays for the second alone. Both are exact, which is the whole
/// requirement: the instructions charged for a path are the instructions that
/// ran on it.
///
/// A head is the entry, every jump target, and the index after every
/// instruction that ends a block — including after a return, which control
/// never reaches, so that every index has an answer rather than a hole.
///
/// A jump target outside the code, or a straight line that runs off the end,
/// is answered rather than reported. Both are [`validate`]'s to refuse, and
/// this has to answer something for the malformed function it is asked about
/// first.
///
/// [`Function::block_fuel`]: crate::Function::block_fuel
/// [`validate`]: crate::lower::validate
pub fn block_fuel(code: &[Inst]) -> Vec<u32> {
    if code.is_empty() {
        return Vec::new();
    }
    let mut head = vec![false; code.len()];
    head[0] = true;
    for (pc, inst) in code.iter().enumerate() {
        match *inst {
            Inst::Jump(to)
            | Inst::JumpIfFalse(to)
            | Inst::JumpIfTrue(to)
            | Inst::JumpIfFalseScalar(to)
            | Inst::JumpIfTrueScalar(to) => {
                if let Some(target) = head.get_mut(to as usize) {
                    *target = true;
                }
            }
            _ => {}
        }
        if ends_a_block(*inst) {
            if let Some(next) = head.get_mut(pc + 1) {
                *next = true;
            }
        }
    }
    let mut fuel = vec![0u32; code.len()];
    for (at, slot) in fuel.iter_mut().enumerate() {
        if !head[at] {
            continue;
        }
        let mut end = at;
        while end + 1 < code.len() && !ends_a_block(code[end]) {
            end += 1;
        }
        *slot = (end - at + 1) as u32;
    }
    fuel
}
