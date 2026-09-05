//! Dropping the clears that sit immediately before a `return`.
//!
//! [`Inst::Clear`] exists so that a dead reference slot holds null rather
//! than an address, because [`Function::refs`](crate::Function::refs) is a
//! fact about a *function* and cannot say when a value in one of its slots
//! stopped being needed. Every clear the lowering emits is therefore worth
//! its one store — except the ones a `return` was going to make pointless a
//! few instructions later.
//!
//! # Why the frame's own slots stop being roots at the `return`
//!
//! Three things have to hold together, and all three are already true:
//!
//! - `Inst::Return` pops the frame. `Memory::pop_frame` truncates the stack
//!   words *without* clearing them, and `Memory::push_frame` zeroes on the
//!   way back up — so a word this pass leaves behind can never become a
//!   phantom root of a later frame. The zeroing happens on the path that was
//!   going to write the words anyway.
//! - The collector's roots are the *frames on the stack*: `Live::each_root`
//!   walks `machine.frames` and reads `Function::refs` of each. A frame that
//!   has been popped is not in that list, so its slots are not roots
//!   whatever they hold.
//! - `Function::refs` is static, so a slot left holding a live address is
//!   *traced correctly* right up to the pop. Nothing is freed early and
//!   nothing dangles; what is given up is retention over the handful of
//!   instructions between where the clear stood and the `return`.
//!
//! So a clear in that position buys "a few instructions earlier" and nothing
//! else. A clear at the end of an **inner** scope is a different matter and
//! stays: there the frame goes on running, the retention is unbounded in the
//! length of the rest of the body, and that is the whole reason the
//! instruction exists.
//!
//! # The two conditions, and where they come from
//!
//! A clear may be dropped when it **is not read before the `return`** and
//! **does not overlap the value the `return` names**.
//!
//! The first is why this pass looks only at an unbroken run of clears ending
//! at an `Inst::Return`: a `Clear` reads nothing, so between a dropped clear
//! and the return there is no instruction at all that could observe the slot.
//! Anything else in the way ends the run.
//!
//! The second has to be checked rather than assumed, because a `Return`
//! carries `Function::returns` words away from `src` and a `Clear` zeroes
//! `layout` words at `slot`: two runs, either of which may be wider than one
//! word. Where they touch, the clear is part of the answer — dropping it
//! would hand the caller words the program meant to be null — so the run
//! ends there and that clear and everything before it stays. No lowering
//! this pass has been run over emits one; the check is what makes that a
//! fact about the code rather than a belief about the lowering.
//!
//! # Renumbering
//!
//! Dropping an instruction moves every instruction after it, and what that
//! costs is [`super::dropping`]'s: a target that landed inside a dropped run
//! becomes the `return` the run led into, which is the same place control
//! would have arrived at, having done the stores this pass decided were free
//! to skip.

use crate::inst::{Inst, Slot};
use crate::layout::{Layout, LayoutId};
use crate::program::{Function, Program, Table};

use super::dropping;

/// Drops every clear that a `return` was about to make pointless.
pub(super) fn drop_clears_before_return(program: &mut Program) {
    let Program {
        functions,
        tables,
        layouts,
        ..
    } = program;
    for function in functions.iter_mut() {
        trim(function, tables, layouts);
    }
}

/// Rewrites one function without the clears its returns render pointless.
fn trim(function: &mut Function, tables: &mut [Table], layouts: &[Layout]) {
    let dropped = pointless(function, layouts);
    dropping::rewrite(function, tables, &dropped);
}

/// Which of a function's instructions are clears a `return` makes pointless.
///
/// Walked backwards, because the question about an instruction is what
/// follows it: `leads_to` holds the slot the `Return` names while the walk is
/// inside a run of clears that reaches one, and `None` everywhere else.
fn pointless(function: &Function, layouts: &[Layout]) -> Vec<bool> {
    let width = |id: LayoutId| layouts.get(id.index()).map_or(1, Layout::width);
    let answer = width(function.returns);
    let mut dropped = vec![false; function.code.len()];
    let mut leads_to: Option<Slot> = None;
    for (at, inst) in function.code.iter().enumerate().rev() {
        match *inst {
            Inst::Return { src } => leads_to = Some(src),
            Inst::Clear { slot, layout } => {
                let Some(src) = leads_to else { continue };
                // Two half-open runs of words in the same frame overlap when
                // each begins before the other ends.
                if slot < src + answer && src < slot + width(layout) {
                    leads_to = None;
                } else {
                    dropped[at] = true;
                }
            }
            _ => leads_to = None,
        }
    }
    dropped
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::layout::Shape;
    use crate::program::{Local, TableId};
    use crate::repr::{RefMap, Repr};
    use cove_diag::Span;

    /// One word of `Int`, one `String` reference, and a two-word inline pair.
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

    fn ran(reprs: Vec<Repr>, returns: LayoutId, code: Vec<Inst>) -> Program {
        ran_with(reprs, returns, code, Vec::new(), Vec::new())
    }

    /// The pass's answer for one function, built by hand: a `Program` of one
    /// function, its switch tables, and the names bound in its frame.
    fn ran_with(
        reprs: Vec<Repr>,
        returns: LayoutId,
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
            returns,
            captures: Vec::new(),
            code,
            locals,
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
        drop_clears_before_return(&mut program);
        program
    }

    fn code(program: &Program) -> &[Inst] {
        &program.function(crate::FunctionId(0)).code
    }

    /// The whole of what this pass is for.
    #[test]
    fn a_run_of_clears_before_a_return_goes() {
        let ran = ran(
            vec![Repr::Int, Repr::Ref, Repr::Ref],
            INT,
            vec![
                Inst::Clear {
                    slot: 1,
                    layout: STR,
                },
                Inst::Clear {
                    slot: 2,
                    layout: STR,
                },
                Inst::Return { src: 0 },
            ],
        );
        assert_eq!(code(&ran), [Inst::Return { src: 0 }]);
    }

    /// A clear anything at all stands between and the `return` is a clear
    /// something may read, so the run ends at the first instruction that is
    /// not one.
    #[test]
    fn a_clear_the_return_does_not_immediately_follow_stays() {
        let ran = ran(
            vec![Repr::Int, Repr::Ref, Repr::Ref],
            INT,
            vec![
                Inst::Clear {
                    slot: 1,
                    layout: STR,
                },
                Inst::Copy {
                    dst: 0,
                    src: 1,
                    layout: INT,
                },
                Inst::Clear {
                    slot: 2,
                    layout: STR,
                },
                Inst::Return { src: 0 },
            ],
        );
        assert_eq!(
            code(&ran),
            [
                Inst::Clear {
                    slot: 1,
                    layout: STR,
                },
                Inst::Copy {
                    dst: 0,
                    src: 1,
                    layout: INT,
                },
                Inst::Return { src: 0 },
            ]
        );
    }

    /// The condition that is not about position: a clear whose words are the
    /// answer's words is part of the answer, and dropping it would hand the
    /// caller something the program said was null.
    ///
    /// The lowering emits none — 1,456 clears were dropped over the corpus
    /// and not one of them overlapped — so this is what says the pass would
    /// keep one rather than what says one exists.
    #[test]
    fn a_clear_that_overlaps_the_answer_stays_and_ends_the_run() {
        let ran = ran(
            // The answer is the two-word `Pair` at slot 1, and the clear
            // names its second word.
            vec![Repr::Ref, Repr::Ref, Repr::Ref, Repr::Ref],
            PAIR,
            vec![
                Inst::Clear {
                    slot: 3,
                    layout: STR,
                },
                Inst::Clear {
                    slot: 2,
                    layout: STR,
                },
                Inst::Return { src: 1 },
            ],
        );
        // The overlapping clear stays, and so does the one before it: the run
        // ends where the answer's words begin.
        assert_eq!(
            code(&ran),
            [
                Inst::Clear {
                    slot: 3,
                    layout: STR,
                },
                Inst::Clear {
                    slot: 2,
                    layout: STR,
                },
                Inst::Return { src: 1 },
            ]
        );
    }

    /// Every program counter follows, and a target that landed *inside* a
    /// dropped run lands on the `return` the run led into.
    ///
    /// This is not hypothetical: the corpus remaps 676 targets, because a
    /// mid-body `return` — a `?`, an early exit from a loop — shifts
    /// everything written after it.
    #[test]
    fn every_target_follows_the_instructions_that_moved() {
        let ran = ran_with(
            vec![Repr::Int, Repr::Ref],
            INT,
            vec![
                // 0: into the middle of the run below.
                Inst::Jump { to: 2 },
                // 1, 2: the run, dropped.
                Inst::Clear {
                    slot: 1,
                    layout: STR,
                },
                Inst::Clear {
                    slot: 1,
                    layout: STR,
                },
                // 3: the return it led into.
                Inst::Return { src: 0 },
                // 4, 5: code after it, which moves by two.
                Inst::Switch {
                    on: 0,
                    table: TableId(0),
                },
                Inst::Return { src: 0 },
            ],
            vec![Table {
                targets: vec![2, 5],
                default: 5,
            }],
            vec![Local {
                name: Arc::from("s"),
                slot: 1,
                layout: STR,
                from: 0,
                to: 2,
            }],
        );
        assert_eq!(
            code(&ran),
            [
                Inst::Jump { to: 1 },
                Inst::Return { src: 0 },
                Inst::Switch {
                    on: 0,
                    table: TableId(0),
                },
                Inst::Return { src: 0 },
            ]
        );
        assert_eq!(ran.table(TableId(0)).targets, vec![1, 3]);
        assert_eq!(ran.table(TableId(0)).default, 3);
        let local = &ran.function(crate::FunctionId(0)).locals[0];
        assert_eq!((local.from, local.to), (0, 1));
    }
}
