//! Dropping the clears of words an earlier clear already made null.
//!
//! [`super::frees`] drops a clear whose words nothing has written since the
//! frame was built, and says nothing about what a *clear* leaves behind — on
//! purpose, because it may be about to remove that clear, and then the word
//! keeps what it had. So a clear that follows another clear of the same words,
//! with no write between them on any path, survives it: the second store
//! writes zero over zero. The lowering makes that shape wherever a slot is
//! cleared on every way out of a region and again where the region's paths
//! meet — a loop that clears its scratch slot at the end of each turn, and a
//! clear of the same slot after the loop's exit, is the common case. Issue
//! #514's step (a)(i) is this pass.
//!
//! # The rule
//!
//! A forward *must* analysis over the finished code, one bit per frame word:
//! *known null* on the way into an instruction when it is null on **every**
//! path that reaches it.
//!
//! - **At entry**, every word is null except the parameters and the captures,
//!   which the caller wrote — the same entry [`super::frees`] starts from, and
//!   for the same reason: `Memory::push_frame` zeroes the frame it reserves.
//! - **[`Inst::Clear`] and [`Inst::Unit`]** make their words null. A unit is
//!   the zero word, which is why `frees` treats it as a clear too.
//! - **[`Inst::Copy`]** carries each source word's answer to its destination:
//!   a copy of null is null.
//! - **Everything else** that may write a word makes it unknown, using the
//!   *wide* answer [`Flow::writes`] gives, so a closure call's guessed width
//!   over-estimates rather than under-estimates.
//! - **A word some [`Inst::AddrOfSlot`] can reach** is unknown everywhere,
//!   because an address writes it without naming it — the run `frees` holds
//!   unknown, from the same [`Flow`].
//! - **The merge** is an intersection: a word is known null at a join only if
//!   it is on every edge in, and a back edge is iterated to a fixpoint.
//!
//! A clear or a unit all of whose words are known null on the way in is
//! dropped.
//!
//! # Why this changes nothing anybody can see
//!
//! A dropped instruction wrote zero over words that were zero on every path
//! reaching it, so the frame after it is the frame before it, bit for bit, at
//! every boundary of every run. There is no window to reason about and no
//! trade to make with the collector: it reads what it read before at every
//! safepoint, whichever task collects and whenever. A debugger printing a
//! named local reads the same null. That is what distinguishes this from
//! [`super::redefined`], whose edit *does* change a word at some boundaries
//! and has to argue that no collection can land on one.
//!
//! The analysis is not disturbed by its own edits either. The transfer of a
//! clear it drops is "these words are null", and they were already null on
//! the way in, so the transfer of the dropped clear and of its absence are
//! the same function: the fixpoint over the rewritten code is the fixpoint
//! over the code as it arrived, and one walk decides the whole body.
//!
//! # Why it runs after `redefined`
//!
//! Each of the clear-dropping passes decides over the code the one before it
//! left, so each is sound whatever runs before it. What would not be sound is
//! deciding this pass and [`super::redefined`] in one walk over the same code:
//! `redefined` counts another clear of the same words as a redefinition that
//! ends a clear's window, and this pass drops the second of two clears
//! because of the first, so between them they could drop both — the first
//! because the second follows, the second because the first precedes. Run in
//! sequence that cannot happen, and running this one last means the clear a
//! drop here rests on is one every other pass has already decided to keep.
//!
//! # Renumbering
//!
//! [`super::dropping`] does it, as for the passes before this one.

use crate::inst::Inst;
use crate::program::{Function, Program};

use super::dropping;
use super::frees::Flow;

/// Drops every clear, and every unit, whose words are null on every path
/// into it.
pub(super) fn drop_clears_of_null_words(program: &mut Program) {
    let dropped: Vec<Vec<bool>> = program
        .functions
        .iter()
        .map(|function| nulled(function, program))
        .collect();
    let Program {
        functions, tables, ..
    } = program;
    for (function, dropped) in functions.iter_mut().zip(&dropped) {
        dropping::rewrite(function, tables, dropped);
    }
}

/// Which of a function's clears and units write zero over words that are
/// already zero.
fn nulled(function: &Function, program: &Program) -> Vec<bool> {
    let mut dropped = vec![false; function.code.len()];
    let Some(flow) = Flow::of(function, program) else {
        return dropped;
    };
    let null = known_null(&flow, function, program);
    for (at, inst) in function.code.iter().enumerate() {
        let (slot, width) = match *inst {
            Inst::Clear { slot, layout } => (slot as usize, flow.width(layout) as usize),
            Inst::Unit { dst } => (dst as usize, 1),
            _ => continue,
        };
        if width == 0 || slot + width > flow.size {
            continue;
        }
        dropped[at] = null[at][slot..slot + width].iter().all(|word| *word);
    }
    dropped
}

/// Whether each word is null on every path into each instruction.
///
/// Every program counter but the entry starts at *null* — the top of this
/// lattice — and is only ever lowered, by a merge or by an instruction that
/// may write the word, so the worklist terminates. A program counter no path
/// reaches keeps the top, which is vacuously true of it.
fn known_null(flow: &Flow<'_>, function: &Function, program: &Program) -> Vec<Vec<bool>> {
    let code = &function.code;
    let mut into = vec![vec![true; flow.size]; code.len()];

    // The caller's writes, which happen before the first instruction.
    let params = function.param_words(&program.layouts) as usize;
    for word in into[0].iter_mut().take(params) {
        *word = false;
    }
    for capture in &function.captures {
        let from = (capture.slot as usize).min(flow.size);
        let to = (from + flow.width(capture.layout) as usize).min(flow.size);
        for word in &mut into[0][from..to] {
            *word = false;
        }
    }
    for (word, null) in into[0].iter_mut().enumerate() {
        if flow.addressed[word] {
            *null = false;
        }
    }

    let mut queue: Vec<usize> = (0..code.len()).rev().collect();
    let mut queued = vec![true; code.len()];
    while let Some(pc) = queue.pop() {
        queued[pc] = false;
        let out = step(flow, &code[pc], &into[pc]);
        flow.successors(pc, &mut |to| {
            let mut moved = false;
            for (word, null) in into[to].iter_mut().enumerate() {
                if *null && !out[word] {
                    *null = false;
                    moved = true;
                }
            }
            if moved && !queued[to] {
                queued[to] = true;
                queue.push(to);
            }
        });
    }
    into
}

/// Sets the run of `width` words beginning at `slot`, stopping at the end of
/// the frame.
fn set(slot: usize, width: usize, value: bool, out: &mut [bool]) {
    let last = (slot + width).min(out.len());
    for word in &mut out[slot.min(last)..last] {
        *word = value;
    }
}

/// One instruction's transfer: which words are null after it.
fn step(flow: &Flow<'_>, inst: &Inst, into: &[bool]) -> Vec<bool> {
    let mut out = into.to_vec();
    match *inst {
        Inst::Clear { slot, layout } => {
            set(slot as usize, flow.width(layout) as usize, true, &mut out)
        }
        Inst::Unit { dst } => set(dst as usize, 1, true, &mut out),
        // A copy of null is null; a copy of anything else is not known to be.
        Inst::Copy { dst, src, layout } => {
            for at in 0..flow.width(layout) as usize {
                let held = into.get(src as usize + at).copied().unwrap_or(false);
                if let Some(word) = out.get_mut(dst as usize + at) {
                    *word = held;
                }
            }
        }
        _ => flow.writes(inst, true, &mut |slot, width| {
            set(slot as usize, width as usize, false, &mut out)
        }),
    }
    // A word an address can reach is written by instructions that do not name
    // it, so it is never known to be anything.
    for (word, null) in out.iter_mut().enumerate() {
        if flow.addressed[word] {
            *null = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::inst::{Len, Slot};
    use crate::layout::{Layout, LayoutId, Shape};
    use crate::repr::{RefMap, Repr};
    use crate::FunctionId;
    use cove_diag::Span;

    /// One word of `Int`, one `String` reference, and a two-word inline pair
    /// of references.
    const INT: LayoutId = LayoutId(0);
    const STR: LayoutId = LayoutId(1);
    const PAIR: LayoutId = LayoutId(2);

    fn layouts() -> Vec<Layout> {
        vec![
            Layout::word("Int", Repr::Int),
            Layout::object("String", Shape::Str),
            Layout::inline(
                "Pair",
                Shape::Struct {
                    fields: Vec::new(),
                    opaque: false,
                },
                vec![Repr::Ref, Repr::Ref],
            ),
        ]
    }

    fn span() -> Span {
        Span::new(cove_diag::FileId(0), 0, 0)
    }

    /// Slot 0 an `Int` answer, slots 1 and 2 references, slot 3 a scalar,
    /// slot 4 a `Bool`, slots 5 and 6 a pair.
    fn function(code: Vec<Inst>) -> Function {
        let reprs = vec![
            Repr::Int,
            Repr::Ref,
            Repr::Ref,
            Repr::Int,
            Repr::Bool,
            Repr::Ref,
            Repr::Ref,
        ];
        Function {
            module: Arc::from("m"),
            name: Arc::from("f"),
            params: Vec::new(),
            spans: (0..code.len()).map(|_| span()).collect(),
            refs: RefMap::of(&reprs),
            reprs,
            returns: INT,
            captures: Vec::new(),
            code,
            locals: Vec::new(),
            inlined: Vec::new(),
            span: span(),
            is_async: false,
            stub: false,
        }
    }

    fn program(function: Function) -> Program {
        Program {
            functions: vec![function],
            layouts: layouts(),
            strings: vec![Arc::from("s")],
            str_layout: STR,
            ..Program::default()
        }
    }

    fn ran_over(function: Function) -> Vec<Inst> {
        let mut program = program(function);
        drop_clears_of_null_words(&mut program);
        program.function(FunctionId(0)).code.clone()
    }

    fn ran(code: Vec<Inst>) -> Vec<Inst> {
        ran_over(function(code))
    }

    fn allocated(dst: Slot) -> Inst {
        Inst::Alloc {
            dst,
            layout: STR,
            len: Len::Count(1),
        }
    }

    /// `dst = s2.field`, a load that writes a reference.
    fn load(dst: Slot) -> Inst {
        Inst::LoadField {
            dst,
            obj: 2,
            at: 0,
            layout: STR,
        }
    }

    fn clear(slot: Slot) -> Inst {
        Inst::Clear { slot, layout: STR }
    }

    fn done() -> Inst {
        Inst::Return { src: 0 }
    }

    /// The shape the pass is for: the second of two clears of one slot, with
    /// nothing between them, writes zero over zero.
    #[test]
    fn a_second_clear_of_the_same_words_goes() {
        assert_eq!(
            ran(vec![allocated(1), clear(1), clear(1), done()]),
            [allocated(1), clear(1), done()]
        );
    }

    /// And the first stays: it is the one that turns what the allocation
    /// wrote into null.
    #[test]
    fn a_clear_of_a_word_something_wrote_stays() {
        let code = vec![allocated(1), clear(1), done()];
        assert_eq!(ran(code.clone()), code);
    }

    /// A write between the two makes the second a clear of something.
    #[test]
    fn a_write_between_the_two_keeps_the_second() {
        let code = vec![allocated(1), clear(1), load(1), clear(1), done()];
        assert_eq!(ran(code.clone()), code);
    }

    /// The merge is an intersection: null on one way in is not null.
    #[test]
    fn a_word_null_on_one_arm_only_is_not_null() {
        let code = vec![
            allocated(1),
            Inst::BranchFalse { cond: 4, to: 3 },
            clear(1),
            clear(1),
            done(),
        ];
        assert_eq!(ran(code.clone()), code);
    }

    /// Null on every way in is, and the jump that named the dropped clear
    /// lands on what followed it.
    #[test]
    fn a_word_null_on_every_arm_is_null() {
        assert_eq!(
            ran(vec![
                allocated(1),
                Inst::BranchFalse { cond: 4, to: 4 },
                clear(1),
                Inst::Jump { to: 5 },
                clear(1),
                clear(1),
                done(),
            ]),
            [
                allocated(1),
                Inst::BranchFalse { cond: 4, to: 4 },
                clear(1),
                Inst::Jump { to: 5 },
                clear(1),
                done(),
            ]
        );
    }

    /// `std.dynamic.tracked`'s path scan, which is issue #514's case: a loop
    /// that clears its scratch slot at the end of every turn, and a clear of
    /// the same slot where it exits. The slot is null on the way in from the
    /// entry and on the way round from the end of a turn, so the exit's clear
    /// goes and the turn's stays.
    #[test]
    fn a_loop_that_clears_every_turn_leaves_its_exit_clear_nothing_to_do() {
        assert_eq!(
            ran(vec![
                Inst::BranchFalse { cond: 4, to: 4 },
                load(1),
                clear(1),
                Inst::Jump { to: 0 },
                clear(1),
                done(),
            ]),
            [
                Inst::BranchFalse { cond: 4, to: 4 },
                load(1),
                clear(1),
                Inst::Jump { to: 0 },
                done(),
            ]
        );
    }

    /// The same loop without the clear at the end of a turn carries what it
    /// loaded round the back edge and out of the exit, so the exit's clear is
    /// the one that frees it.
    #[test]
    fn a_loop_that_carries_a_reference_out_keeps_its_exit_clear() {
        let code = vec![
            Inst::BranchFalse { cond: 4, to: 3 },
            load(1),
            Inst::Jump { to: 0 },
            clear(1),
            done(),
        ];
        assert_eq!(ran(code.clone()), code);
    }

    /// A parameter is written by the caller before the first instruction.
    #[test]
    fn a_parameter_is_not_null_at_entry() {
        let mut f = function(vec![clear(1), done()]);
        f.params = vec![INT, STR];
        let code = f.code.clone();
        assert_eq!(ran_over(f), code);
    }

    /// A slot whose address was taken is written by stores that do not name
    /// it, so it is never known to be null — not even after a clear of it.
    #[test]
    fn a_slot_whose_address_was_taken_is_never_null() {
        let code = vec![
            Inst::AddrOfSlot { dst: 3, slot: 1 },
            clear(1),
            clear(1),
            done(),
        ];
        assert_eq!(ran(code.clone()), code);
    }

    /// A copy of null is null, and a copy of anything else is not.
    #[test]
    fn a_copy_carries_null_and_only_null() {
        let copy = Inst::Copy {
            dst: 2,
            src: 1,
            layout: STR,
        };
        assert_eq!(
            ran(vec![copy.clone(), clear(2), done()]),
            [copy.clone(), done()]
        );
        let code = vec![allocated(1), copy, clear(2), done()];
        assert_eq!(ran(code.clone()), code);
    }

    /// A clear goes only when *every* word it zeroes is null.
    #[test]
    fn a_clear_wider_than_what_is_null_stays() {
        let pair = Inst::Clear {
            slot: 5,
            layout: PAIR,
        };
        let code = vec![load(6), pair.clone(), done()];
        assert_eq!(ran(code.clone()), code);
        assert_eq!(
            ran(vec![load(6), clear(6), pair, done()]),
            [load(6), clear(6), done()]
        );
    }

    /// A unit is the zero word, so it is a clear for this pass as for
    /// `frees`: the second of two goes.
    #[test]
    fn a_unit_over_a_unit_goes() {
        let one = Inst::Int { dst: 3, value: 1 };
        let unit = Inst::Unit { dst: 3 };
        assert_eq!(
            ran(vec![one.clone(), unit.clone(), unit.clone(), done()]),
            [one, unit, done()]
        );
    }

    /// Run after `redefined`, as the lowering runs them: that pass drops the
    /// first of two clears because the second redefines its words, and this
    /// one then sees a clear of a word something wrote and keeps it. Decided
    /// together over the same code, the two would have dropped both.
    #[test]
    fn after_redefined_one_of_two_clears_survives() {
        let mut program = program(function(vec![
            allocated(1),
            clear(1),
            clear(1),
            allocated(2),
            done(),
        ]));
        super::super::redefined::drop_clears_before_redefinition(&mut program);
        drop_clears_of_null_words(&mut program);
        assert_eq!(
            program.function(FunctionId(0)).code,
            [allocated(1), clear(1), allocated(2), done()]
        );
    }
}
