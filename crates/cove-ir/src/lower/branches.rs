//! Fusing a comparison with the branch that is all it feeds.
//!
//! [ADR 0054](../../../../docs/adr/0054-a-comparison-that-only-feeds-a-branch-is-the-branch.md)
//! decides this. A comparison in this instruction set answers a `Bool` into a
//! slot, and almost every comparison a program writes is the condition of an
//! `if` or a `while` — so the slot is read once, by the `branch-false` on the
//! very next line, and never again. `examples/covefmt` spends 26.4% of its
//! executed instructions in `branch-false` alone, and 18.8% of the whole run
//! is the second half of a pair like that.
//!
//! [`Inst::CmpBranch`] and [`Inst::CmpImmBranch`] are those two instructions
//! as one, and they are *exactly* those two: the `Bool` is still written to
//! `dst`, and then the branch is taken when it is false. Nothing here asks
//! whether anything reads `dst`, which is why nothing here can get that
//! question wrong.
//!
//! # It is a peephole over finished code
//!
//! Recognising the pair after lowering keeps every lowering that produces a
//! comparison — `if`, `while`, `&&`, a `match` guard — exactly as it was, and
//! means a form this pass cannot see is a *missed fusion* rather than a wrong
//! one. It is also the only place the question can be asked: the pair is a
//! fact about two adjacent instructions and about every branch target in the
//! function, and neither is knowable while the code is still being emitted.
//!
//! It runs **after** [`super::tails`] and [`super::frees`] for the same
//! reason it runs at all: those two *delete* instructions, so what is
//! adjacent to what is not settled until they have. A dropped clear between a
//! comparison and its branch is a pair this pass can fuse and could not have
//! seen earlier.
//!
//! # The one condition that is not about the two instructions
//!
//! A `branch-false` may itself be the target of a jump from somewhere else —
//! the second arm of an `else if`, a loop's `continue` — and then it must keep
//! its own instruction, because control arrives at it without having run the
//! comparison above. So every [`Inst::Jump`], [`Inst::BranchFalse`] and
//! [`Inst::Switch`] of the function is walked first and its targets collected,
//! and a branch that is named by any of them is left alone.
//!
//! In `covefmt` that is 27 million executions of a `branch-false` that cannot
//! be fused, and ADR 0054 records how the condition announced itself: a first
//! count that missed it attributed more executions to `eq.int.imm` than
//! `eq.int.imm` has. Getting it wrong here is a *wrong program* and not a slow
//! one — a comparison that falls through into nothing, and an arm of an `if`
//! that is entered on the strength of whatever the last comparison left in the
//! slot.
//!
//! # Deleting the branch renumbers the function
//!
//! Which is [`super::dropping`]'s, the same machinery [`super::tails`] and
//! [`super::frees`] use: every jump, branch, switch table, local range and
//! inlined range that named a counter past the deleted instruction moves back
//! by one. A second renumbering here would be a second thing to get right.
//!
//! The fused instruction is written in at the comparison's own counter and the
//! branch is marked dropped, so the rewrite maps the deleted counter to the
//! instruction *after* it — and a target that pointed at the branch is exactly
//! what condition three above refuses, so no target lands on a counter that
//! means something different afterwards.

use crate::inst::{Inst, Pc};
use crate::program::{Function, Program, Table};

use super::dropping;

/// Fuses every comparison whose only consumer is the branch beside it.
pub(super) fn fuse_comparisons_into_branches(program: &mut Program) {
    let Program {
        functions, tables, ..
    } = program;
    for function in functions.iter_mut() {
        fuse(function, tables);
    }
}

/// Rewrites one function with the pairs it holds fused.
fn fuse(function: &mut Function, tables: &mut [Table]) {
    let targeted = targeted(function, tables);
    let mut dropped = vec![false; function.code.len()];
    // A pair at `pc` consumes `pc + 1`, so the walk skips past a branch it has
    // just absorbed rather than reading it as the head of another pair. It
    // cannot be one — a `branch-false` is not a comparison — but the skip is
    // what keeps that a fact about the loop rather than about the match below.
    let mut pc = 0;
    while pc + 1 < function.code.len() {
        let Some(fused) = fusion(&function.code, pc, &targeted) else {
            pc += 1;
            continue;
        };
        function.code[pc] = fused;
        dropped[pc + 1] = true;
        pc += 2;
    }
    dropping::rewrite(function, tables, &dropped);
}

/// The fused instruction the pair at `pc` becomes, where there is one.
///
/// The four conditions of ADR 0054's decision, in the order they are cheapest
/// to refuse in.
fn fusion(code: &[Inst], pc: usize, targeted: &[bool]) -> Option<Inst> {
    let Inst::BranchFalse { cond, to } = code[pc + 1] else {
        return None;
    };
    if targeted[pc + 1] {
        return None;
    }
    match code[pc] {
        Inst::Cmp { on, op, dst, a, b } if dst == cond => Some(Inst::CmpBranch {
            on,
            op,
            dst,
            a,
            b,
            target: to,
        }),
        // The immediate narrows to the thirty-two bits the fused encoding
        // gives it, or the pair stays as it is. A comparison against a wider
        // constant is a missed fusion; one that silently lost the high bits
        // would be a wrong answer.
        Inst::CmpImm { op, dst, a, value } if dst == cond => Some(Inst::CmpImmBranch {
            op,
            dst,
            a,
            value: i32::try_from(value).ok()?,
            target: to,
        }),
        _ => None,
    }
}

/// Which of a function's counters are named as a target by something.
///
/// Every jump, every branch — fused or not, because a run of this pass over
/// code it has already fused must answer the same — and every switch table,
/// read through the [`Inst::Switch`] that names it for the reason
/// [`super::dropping`] gives: one table per switch site, so walking the
/// instructions reaches each of this function's exactly once.
///
/// One longer than the code, so that a target one past the last instruction —
/// which nothing lowers, and which the verifier refuses — is recorded rather
/// than a panic.
fn targeted(function: &Function, tables: &[Table]) -> Vec<bool> {
    let last = function.code.len();
    let mut held = vec![false; last + 1];
    let mut mark = |to: Pc| held[(to as usize).min(last)] = true;
    for inst in &function.code {
        match *inst {
            Inst::Jump { to } | Inst::BranchFalse { to, .. } => mark(to),
            Inst::CmpBranch { target, .. } | Inst::CmpImmBranch { target, .. } => mark(target),
            Inst::Switch { table, .. } => {
                if let Some(table) = tables.get(table.index()) {
                    for target in &table.targets {
                        mark(*target);
                    }
                    mark(table.default);
                }
            }
            _ => {}
        }
    }
    held
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cove_diag::Span;

    use super::*;
    use crate::inst::{CmpOp, Compare};
    use crate::layout::{Layout, LayoutId, Shape};
    use crate::program::Local;
    use crate::repr::{RefMap, Repr};

    const INT: LayoutId = LayoutId(0);
    const STR: LayoutId = LayoutId(1);

    fn layouts() -> Vec<Layout> {
        vec![
            Layout::word("Int", Repr::Int),
            Layout::object("String", Shape::Str),
        ]
    }

    fn span() -> Span {
        Span::new(cove_diag::FileId(0), 0, 0)
    }

    /// The pass's answer for one function, built by hand.
    fn ran(reprs: Vec<Repr>, code: Vec<Inst>) -> Program {
        ran_with(reprs, code, Vec::new(), Vec::new())
    }

    fn ran_with(
        reprs: Vec<Repr>,
        code: Vec<Inst>,
        tables: Vec<Table>,
        locals: Vec<Local>,
    ) -> Program {
        let function = Function {
            module: Arc::from("m"),
            name: Arc::from("f"),
            params: Vec::new(),
            spans: (0..code.len()).map(|_| span()).collect(),
            refs: RefMap::of(&reprs),
            reprs,
            returns: INT,
            captures: Vec::new(),
            code,
            locals,
            inlined: Vec::new(),
            span: span(),
            is_async: false,
            stub: false,
        };
        let mut program = Program {
            functions: vec![function],
            layouts: layouts(),
            str_layout: STR,
            tables,
            ..Program::default()
        };
        fuse_comparisons_into_branches(&mut program);
        program
    }

    fn code(program: &Program) -> &[Inst] {
        &program.function(crate::FunctionId(0)).code
    }

    fn cmp(dst: u32, a: u32, b: u32) -> Inst {
        Inst::Cmp {
            on: Compare::Int,
            op: CmpOp::Lt,
            dst,
            a,
            b,
        }
    }

    /// The whole of what this pass is for: two instructions become one, and
    /// the one says what both said.
    #[test]
    fn a_comparison_and_the_branch_beside_it_become_one_instruction() {
        let ran = ran(
            vec![Repr::Bool, Repr::Int, Repr::Int],
            vec![
                cmp(0, 1, 2),
                Inst::BranchFalse { cond: 0, to: 3 },
                Inst::Int { dst: 1, value: 7 },
                Inst::Return { src: 1 },
            ],
        );
        assert_eq!(
            code(&ran),
            [
                Inst::CmpBranch {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 0,
                    a: 1,
                    b: 2,
                    // One counter back: the branch it replaced is gone.
                    target: 2,
                },
                Inst::Int { dst: 1, value: 7 },
                Inst::Return { src: 1 },
            ]
        );
    }

    /// The immediate form, which is the one 5.8% of `covefmt`'s instructions
    /// are.
    #[test]
    fn an_immediate_comparison_fuses_the_same_way() {
        let ran = ran(
            vec![Repr::Bool, Repr::Int],
            vec![
                Inst::CmpImm {
                    op: CmpOp::Eq,
                    dst: 0,
                    a: 1,
                    value: 32,
                },
                Inst::BranchFalse { cond: 0, to: 3 },
                Inst::Int { dst: 1, value: 7 },
                Inst::Return { src: 1 },
            ],
        );
        assert_eq!(
            code(&ran)[0],
            Inst::CmpImmBranch {
                op: CmpOp::Eq,
                dst: 0,
                a: 1,
                value: 32,
                target: 2,
            }
        );
    }

    /// The condition that is not about the two instructions, and the one
    /// whose failure is a wrong program: control can arrive at the branch
    /// without having run the comparison.
    #[test]
    fn a_branch_that_is_a_jump_target_keeps_its_own_instruction() {
        let held = vec![
            cmp(0, 1, 2),
            Inst::BranchFalse { cond: 0, to: 4 },
            Inst::Int { dst: 1, value: 7 },
            // The second arm of an `else if`: the jump lands on the branch
            // itself, so the branch has to still be there.
            Inst::Jump { to: 1 },
            Inst::Return { src: 1 },
        ];
        let ran = ran(vec![Repr::Bool, Repr::Int, Repr::Int], held.clone());
        assert_eq!(code(&ran), held);
    }

    /// A switch's targets are in the program's table rather than on the
    /// instruction, and they name counters just as a jump does.
    #[test]
    fn a_branch_a_switch_dispatches_to_keeps_its_own_instruction() {
        let held = vec![
            cmp(0, 1, 2),
            Inst::BranchFalse { cond: 0, to: 4 },
            Inst::Int { dst: 1, value: 7 },
            Inst::Switch {
                on: 1,
                table: crate::TableId(0),
            },
            Inst::Return { src: 1 },
        ];
        let ran = ran_with(
            vec![Repr::Bool, Repr::Int, Repr::Int],
            held.clone(),
            vec![Table {
                targets: vec![1],
                default: 4,
            }],
            Vec::new(),
        );
        assert_eq!(code(&ran), held);
    }

    /// The write is kept, so a slot something else reads afterwards is a slot
    /// that still holds the answer — which is the whole reason the fused form
    /// carries no condition.
    #[test]
    fn a_comparison_whose_answer_is_read_again_still_fuses() {
        let ran = ran(
            vec![Repr::Bool, Repr::Int, Repr::Int, Repr::Bool],
            vec![
                cmp(0, 1, 2),
                Inst::BranchFalse { cond: 0, to: 3 },
                Inst::Not { dst: 3, a: 0 },
                Inst::Return { src: 1 },
            ],
        );
        assert!(matches!(code(&ran)[0], Inst::CmpBranch { dst: 0, .. }));
        assert_eq!(code(&ran)[1], Inst::Not { dst: 3, a: 0 });
    }

    /// An immediate the fused encoding's thirty-two bits cannot hold leaves
    /// the pair as it is. A fusion that dropped the high bits would be a
    /// wrong answer rather than a missed saving.
    #[test]
    fn an_immediate_wider_than_thirty_two_bits_is_left_unfused() {
        let held = vec![
            Inst::CmpImm {
                op: CmpOp::Lt,
                dst: 0,
                a: 1,
                value: i64::from(i32::MAX) + 1,
            },
            Inst::BranchFalse { cond: 0, to: 3 },
            Inst::Int { dst: 1, value: 7 },
            Inst::Return { src: 1 },
        ];
        let left = ran(vec![Repr::Bool, Repr::Int], held.clone());
        assert_eq!(code(&left), held);
        // And the widest one that does fit still fuses, so the bound is the
        // field's and not one off it.
        let widest = ran(
            vec![Repr::Bool, Repr::Int],
            vec![
                Inst::CmpImm {
                    op: CmpOp::Lt,
                    dst: 0,
                    a: 1,
                    value: i64::from(i32::MAX),
                },
                Inst::BranchFalse { cond: 0, to: 3 },
                Inst::Int { dst: 1, value: 7 },
                Inst::Return { src: 1 },
            ],
        );
        assert!(matches!(
            code(&widest)[0],
            Inst::CmpImmBranch {
                value: i32::MAX,
                ..
            }
        ));
    }

    /// A branch whose condition is not the slot the comparison wrote is two
    /// instructions that happen to be adjacent.
    #[test]
    fn a_branch_on_some_other_slot_is_not_a_pair() {
        let held = vec![
            cmp(0, 1, 2),
            Inst::BranchFalse { cond: 3, to: 3 },
            Inst::Int { dst: 1, value: 7 },
            Inst::Return { src: 1 },
        ];
        let ran = ran(
            vec![Repr::Bool, Repr::Int, Repr::Int, Repr::Bool],
            held.clone(),
        );
        assert_eq!(code(&ran), held);
    }

    /// The renumbering, which is the half of this pass that is not the
    /// recognition: a loop's back edge names a counter past the deleted
    /// branch and has to move with it, and so does a local's live range.
    #[test]
    fn a_fusion_inside_a_loop_moves_the_back_edge_and_the_names() {
        let ran = ran_with(
            vec![Repr::Bool, Repr::Int, Repr::Int],
            vec![
                // 0  the loop's test
                cmp(0, 1, 2),
                // 1  out of the loop
                Inst::BranchFalse { cond: 0, to: 4 },
                // 2  the body
                Inst::Int { dst: 1, value: 7 },
                // 3  the back edge, past the branch that goes
                Inst::Jump { to: 0 },
                // 4
                Inst::Return { src: 1 },
            ],
            Vec::new(),
            vec![Local {
                name: Arc::from("n"),
                slot: 1,
                layout: INT,
                from: 2,
                to: 5,
            }],
        );
        assert_eq!(
            code(&ran),
            [
                Inst::CmpBranch {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 0,
                    a: 1,
                    b: 2,
                    target: 3,
                },
                Inst::Int { dst: 1, value: 7 },
                Inst::Jump { to: 0 },
                Inst::Return { src: 1 },
            ]
        );
        let local = &ran.function(crate::FunctionId(0)).locals[0];
        assert_eq!((local.from, local.to), (1, 4));
    }

    /// Two pairs in a row, which is what an `&&` chain is: the second
    /// comparison is not read as the branch of the first pair, and both fuse.
    #[test]
    fn two_pairs_in_a_row_both_fuse() {
        let ran = ran(
            vec![Repr::Bool, Repr::Int, Repr::Int],
            vec![
                cmp(0, 1, 2),
                Inst::BranchFalse { cond: 0, to: 5 },
                cmp(0, 2, 1),
                Inst::BranchFalse { cond: 0, to: 5 },
                Inst::Int { dst: 1, value: 7 },
                Inst::Return { src: 1 },
            ],
        );
        assert!(matches!(code(&ran)[0], Inst::CmpBranch { target: 3, .. }));
        assert!(matches!(code(&ran)[1], Inst::CmpBranch { target: 3, .. }));
        assert_eq!(code(&ran).len(), 4);
    }
}
