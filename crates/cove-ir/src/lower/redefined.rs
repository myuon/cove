//! Dropping the clears a redefinition of the same words, or a `return`,
//! makes pointless.
//!
//! [`Inst::Clear`] turns a dead reference slot into null so that the
//! collector, which reads a frame through a [`RefMap`](crate::RefMap) that
//! cannot change with the program counter, stops seeing an object the
//! program has finished with. A slot the lowering reuses is cleared at the
//! end of one value's life and written again at the start of the next, and
//! when the second comes straight after the first the null is never seen by
//! anybody: not by the program, which writes the slot before it reads it, and
//! not by the collector, which cannot run in between. Issue #514's F7 is this
//! pass, and it is a fact about the finished code rather than about any one
//! body — nothing here knows which function it is looking at.
//!
//! A `return` ends the life of every word of the frame at once, so it ends a
//! window as a redefinition does: a clear followed by nothing but quiet work
//! and then the function's exit is a null nobody will read either. That is
//! issue #514's F7b, and it is the general case of what [`super::tails`] does
//! for a clear *immediately* before a `return`.
//!
//! # The rule
//!
//! A clear of the words `W` is dropped when **every path** from it reaches a
//! *redefinition* of `W` or a *`return`* through a *window* of instructions
//! that are each *quiet*, and nothing else:
//!
//! - **A redefinition** definitely writes every word of `W` — the narrow
//!   answer [`Flow::writes`] gives, so a closure call's guessed width never
//!   counts — reads none of them, and is one of the quiet instructions below,
//!   or [`Inst::DynChild`] or [`Inst::DynOpen`], which write a view and do
//!   not allocate, or another [`Inst::Clear`] of the same words, which writes
//!   the null this one would have.
//! - **A `return`** ends the window when the answer it carries away shares no
//!   word with `W` — the read every instruction in the window is asked about,
//!   with [`Flow::reads`] giving the answer's run at
//!   [`Function::returns`](crate::Function::returns)' width. Where they share
//!   one, the clear is part of the answer and stays.
//! - **A quiet instruction** is one on a fixed list of instructions that
//!   neither allocate, nor call, nor park, nor write the heap; that reads no
//!   word of `W`; and whose every destination word is *not a root* — no
//!   [`Repr::Ref`](crate::Repr::Ref) word of the frame. Branches and jumps
//!   are on the list, so a window may fork, and then each arm has to end in
//!   a redefinition of its own.
//! - A path that traps, loops back to a program counter the walk is already
//!   inside, or meets anything else keeps the clear. A trap is kept on
//!   purpose: it leaves the frame standing, because the refusal's call chain
//!   is read out of the frames, and nothing here shows that no reader of a
//!   standing frame looks at `W`.
//!
//! Two more conditions keep the edit invisible to the two readers that are
//! not the program. A clear whose words some [`Inst::AddrOfSlot`] of the
//! function can reach is kept, because an address writes and reads a word
//! without naming it — the same run [`super::frees`] holds unknown. And a
//! clear whose words a named [`Local`](crate::Local) denotes at any program
//! counter in the window is kept, because a debugger stopped there would
//! print the old value where it used to print null.
//!
//! # Why the collector cannot tell
//!
//! The collector reads the static map at a safepoint. Removing the clear
//! changes what it would read in exactly one place — the words `W`, at the
//! boundaries inside the window — and the question is whether a collection
//! can happen there and see the difference.
//!
//! **Alone, it cannot.** A task that is the only one running collects only
//! inside an instruction that allocates, and the window holds none: a
//! quiet instruction does not allocate and does not call anything that
//! could, and neither does a redefinition.
//!
//! **Beside another task, the encoded loop polls at any boundary**, because
//! its safepoint is counted in instructions and not placed at particular
//! ones, so a collection another task started can find this one parked
//! inside the window. That is why a quiet instruction may not write a root or
//! the heap. Then the collector's view of this frame at every boundary of
//! the window — every root word, and every object they reach — is exactly
//! its view at the boundary *before the clear* in the program as it was:
//! only scalar words have changed since. So a collection in the window finds
//! what it would have found had this task parked one boundary earlier, which
//! was always possible, and no collection can retain anything the original
//! program could not have had retained. The compiled tier takes a safepoint
//! at fewer places than the encoded loop — back edges, calls and allocations
//! — and is covered by the same argument a fortiori.
//!
//! # Why a `return` ends the window
//!
//! The argument above covers every boundary *up to* the `return`. What is
//! left is the `return` itself and what comes after it, and after it there
//! is nothing: the frame is popped, and a popped frame is not read by
//! anybody.
//!
//! - **The encoded tier.** `RETURN` copies the answer's words into the
//!   caller's destination — a word copy, which allocates nothing and polls
//!   nothing — or, for the outermost frame of a call, reads them out; then
//!   `Memory::pop_frame` truncates the stack and the frame is gone from
//!   `machine.frames`. There is no safepoint between the copy and the pop,
//!   so the last boundary a collection can see this frame at is the one
//!   before the `return`, which is inside the window.
//! - **The compiled tier.** The emitted `return` stores the answer's words
//!   into the caller's destination and leaves; the frame comes off in
//!   `close` for a call compiled code made itself and in `enter` for one the
//!   encoded tier made. Both charge the pending work and neither polls:
//!   `close` is documented as a charge point that is deliberately not a
//!   safepoint, and `enter` moves the dispatch loop's threshold and pops. So
//!   the frame is never presented to a collection after its last compiled
//!   boundary either.
//! - **Nobody reads the words afterwards.** The collector's roots are the
//!   frames in `machine.frames`, which no longer holds this one; the caller
//!   reads its own frame, which ends where this one began; and
//!   `Memory::push_frame` zeroes the words on the way back up, so what the
//!   dropped clear left behind never becomes a stale root of the next frame
//!   built there. That is the same three facts [`super::tails`] rests on.
//!
//! The named-local condition still applies up to and including the
//! `return`'s own program counter, because a debugger can stop there.
//!
//! This is the conservative rule issue #514 chose, and it is conservative on
//! purpose. A window that writes a root — a `load-field` of another
//! reference, a `dyn.child` into another view, a clear of another slot —
//! would let the collector see a frame the original never presented, and
//! whether the object `W` held is then reachable from elsewhere is a
//! question this pass does not try to answer. Those clears stay.
//!
//! # Renumbering
//!
//! [`super::dropping`] does it, as for the two passes before this one: a
//! target that landed on a dropped clear lands on the instruction after it,
//! which is where control would have gone having done the store this pass
//! found unobservable.

use crate::inst::{Inst, Slot};
use crate::program::{Function, Program};

use super::dropping;
use super::frees::Flow;

/// Drops every clear whose words are redefined, or whose frame is popped,
/// before anything could observe them being null.
pub(super) fn drop_clears_before_redefinition(program: &mut Program) {
    let dropped: Vec<Vec<bool>> = program
        .functions
        .iter()
        .map(|function| redefined(function, program))
        .collect();
    let Program {
        functions, tables, ..
    } = program;
    for (function, dropped) in functions.iter_mut().zip(&dropped) {
        dropping::rewrite(function, tables, dropped);
    }
}

/// How many instructions the walk from one clear may look at before it keeps
/// the clear. The windows this pass is for are a handful long; a walk that
/// needs more than this is not looking at one.
const WINDOW: usize = 64;

/// Which of a function's instructions are clears that a redefinition or a
/// `return` makes pointless.
fn redefined(function: &Function, program: &Program) -> Vec<bool> {
    let mut dropped = vec![false; function.code.len()];
    let Some(flow) = Flow::of(function, program) else {
        return dropped;
    };
    // One table of answers for the whole function, reset between clears by
    // the walk that wrote it, so that a body with many clears does not pay
    // its own length once per clear.
    let mut state = vec![Seen::No; function.code.len()];
    for (at, inst) in function.code.iter().enumerate() {
        let Inst::Clear { slot, layout } = *inst else {
            continue;
        };
        let words = Words {
            from: slot as usize,
            to: slot as usize + flow.width(layout) as usize,
        };
        if words.to > flow.size || (words.from..words.to).any(|word| flow.addressed[word]) {
            continue;
        }
        let mut walk = Walk {
            flow: &flow,
            function,
            words,
            state: &mut state,
            touched: Vec::new(),
            budget: WINDOW,
        };
        let mut next = Vec::new();
        flow.successors(at, &mut |to| next.push(to));
        dropped[at] = !next.is_empty() && next.into_iter().all(|to| walk.reaches(to));
        for pc in walk.touched {
            state[pc] = Seen::No;
        }
    }
    dropped
}

/// A half-open run of frame words.
#[derive(Clone, Copy)]
struct Words {
    from: usize,
    to: usize,
}

impl Words {
    /// Whether the run `slot..slot + width` shares a word with this one.
    fn meets(self, slot: Slot, width: u32) -> bool {
        let (slot, end) = (slot as usize, slot as usize + width as usize);
        slot < self.to && self.from < end
    }

    /// Whether the run `slot..slot + width` holds every word of this one.
    fn within(self, slot: Slot, width: u32) -> bool {
        let (slot, end) = (slot as usize, slot as usize + width as usize);
        slot <= self.from && self.to <= end
    }
}

/// Where the walk from one clear has been.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seen {
    No,
    /// On the path being walked: arriving here again is a loop that never
    /// redefined the words.
    Walking,
    /// Every path on from here ends the window: it redefines the words or
    /// returns without reading them.
    Ends,
    Fails,
}

struct Walk<'w, 'p> {
    flow: &'w Flow<'p>,
    function: &'w Function,
    words: Words,
    state: &'w mut [Seen],
    /// Every program counter [`Walk::state`] holds an answer for.
    touched: Vec<usize>,
    /// How many more instructions the walk may look at.
    budget: usize,
}

impl Walk<'_, '_> {
    /// Whether every path from the boundary before `pc` redefines the words,
    /// or returns without reading them, through quiet instructions alone.
    ///
    /// Recursive over the window, and bounded by [`WINDOW`] instructions in
    /// all, so that a long run of scalar arithmetic costs neither depth nor
    /// time quadratic in a body's length. A walk that runs out keeps the
    /// clear.
    fn reaches(&mut self, pc: usize) -> bool {
        match self.state[pc] {
            Seen::Ends => return true,
            Seen::Fails | Seen::Walking => return false,
            Seen::No => {}
        }
        if self.budget == 0 {
            return false;
        }
        self.budget -= 1;
        self.touched.push(pc);
        // A debugger stopped here prints what a name denotes, and the name
        // would show the old value where it used to show null.
        if self.named_at(pc) {
            self.state[pc] = Seen::Fails;
            return false;
        }
        let inst = &self.function.code[pc];
        let words = self.words;
        let mut read = false;
        self.flow
            .reads(inst, &mut |slot, width| read |= words.meets(slot, width));
        if read {
            self.state[pc] = Seen::Fails;
            return false;
        }
        // A `return` that does not carry the words away — the read above —
        // pops the frame they are in, and a popped frame is nobody's root.
        if self.redefines(inst) || matches!(inst, Inst::Return { .. }) {
            self.state[pc] = Seen::Ends;
            return true;
        }
        if !self.quiet(inst) {
            self.state[pc] = Seen::Fails;
            return false;
        }
        self.state[pc] = Seen::Walking;
        let mut next = Vec::new();
        self.flow.successors(pc, &mut |to| next.push(to));
        // No successor is a trap here — a `return` was answered above — or a
        // fall off the end, which the verifier refuses; neither ends the
        // window.
        let all = !next.is_empty() && next.into_iter().all(|to| self.reaches(to));
        self.state[pc] = if all { Seen::Ends } else { Seen::Fails };
        all
    }

    /// Whether a named local that shares a word with the cleared run is bound
    /// at `pc`, in this body or in one expanded into it.
    fn named_at(&self, pc: usize) -> bool {
        let words = self.words;
        let flow = self.flow;
        self.function
            .locals
            .iter()
            .chain(self.function.inlined.iter().flat_map(|held| &held.locals))
            .any(|local| {
                (local.from as usize) <= pc
                    && pc < local.to as usize
                    && words.meets(local.slot, flow.width(local.layout))
            })
    }

    /// Whether `inst` writes every cleared word, without allocating or
    /// calling anything that could.
    fn redefines(&self, inst: &Inst) -> bool {
        let writer = matches!(
            inst,
            Inst::Clear { .. } | Inst::DynChild { .. } | Inst::DynOpen { .. }
        ) || listed(inst);
        if !writer {
            return false;
        }
        let words = self.words;
        let mut all = false;
        self.flow.writes(inst, false, &mut |slot, width| {
            all |= words.within(slot, width)
        });
        all
    }

    /// Whether `inst` leaves the collector's view of the frame and the heap
    /// exactly as it found it: on the list, and writing no root word.
    fn quiet(&self, inst: &Inst) -> bool {
        if !listed(inst) {
            return false;
        }
        let refs = &self.function.refs;
        let mut rooted = false;
        self.flow.writes(inst, true, &mut |slot, width| {
            rooted |= (slot..slot.saturating_add(width)).any(|word| refs.is_ref(word));
        });
        !rooted
    }
}

/// The instructions that neither allocate, nor call, nor park, nor write the
/// heap: what a quiet instruction may be, before the question of which words
/// it writes is asked.
///
/// A list and not a rule, so that an instruction added to the IR is outside
/// it until someone has asked that question of it.
fn listed(inst: &Inst) -> bool {
    matches!(
        inst,
        Inst::Unit { .. }
            | Inst::Bool { .. }
            | Inst::Int { .. }
            | Inst::Tag { .. }
            | Inst::Float { .. }
            | Inst::Neg { .. }
            | Inst::Not { .. }
            | Inst::Arith { .. }
            | Inst::Cmp { .. }
            | Inst::ArithImm { .. }
            | Inst::CmpImm { .. }
            | Inst::CmpBranch { .. }
            | Inst::CmpImmBranch { .. }
            | Inst::Convert { .. }
            | Inst::FloatAbs { .. }
            | Inst::FloatMinMax { .. }
            | Inst::FloatRound { .. }
            | Inst::FloatSqrt { .. }
            | Inst::Copy { .. }
            | Inst::LoadField { .. }
            | Inst::LoadElem { .. }
            | Inst::Len { .. }
            | Inst::Jump { .. }
            | Inst::BranchFalse { .. }
            | Inst::Switch { .. }
            | Inst::DynKind { .. }
            | Inst::DynSameType { .. }
            | Inst::DynSameObject { .. }
            | Inst::DynRead { .. }
            | Inst::DynCase { .. }
            | Inst::DynCount { .. }
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::inst::{ArithOp, Len, Num};
    use crate::layout::{Layout, LayoutId, Shape};
    use crate::program::Local;
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

    /// Slot 0 an `Int` answer, slot 1 the `String` that is cleared, slot 2
    /// another reference, slots 3 and 4 scalars, slots 5 and 6 a pair.
    fn reprs() -> Vec<Repr> {
        vec![
            Repr::Int,
            Repr::Ref,
            Repr::Ref,
            Repr::Int,
            Repr::Bool,
            Repr::Ref,
            Repr::Ref,
        ]
    }

    fn function(code: Vec<Inst>) -> Function {
        let reprs = reprs();
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

    fn ran_over(function: Function) -> Vec<Inst> {
        let mut program = Program {
            functions: vec![function],
            layouts: layouts(),
            strings: vec![Arc::from("s")],
            str_layout: STR,
            ..Program::default()
        };
        drop_clears_before_redefinition(&mut program);
        program.function(FunctionId(0)).code.clone()
    }

    fn ran(code: Vec<Inst>) -> Vec<Inst> {
        ran_over(function(code))
    }

    /// `dst = s2.field`, a load that writes a reference without allocating.
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

    fn allocated(dst: Slot) -> Inst {
        Inst::Alloc {
            dst,
            layout: STR,
            len: Len::Count(1),
        }
    }

    fn scalar() -> Inst {
        Inst::Arith {
            num: Num::Int,
            op: ArithOp::Add,
            dst: 3,
            a: 3,
            b: 3,
        }
    }

    fn done() -> Inst {
        Inst::Return { src: 0 }
    }

    /// A trap whose three sentences are the `String` at slot 2.
    fn trapped() -> Inst {
        Inst::Trap {
            message: 2,
            rule: 2,
            help: 2,
        }
    }

    /// The shape the pass is for: the slot is written straight after it was
    /// cleared, so the null is never there to be seen.
    #[test]
    fn a_clear_the_next_instruction_overwrites_goes() {
        assert_eq!(
            ran(vec![allocated(1), clear(1), load(1), done()]),
            [allocated(1), load(1), done()]
        );
    }

    /// Scalar work in between leaves the collector's view of the frame as it
    /// was at the clear, so it does not keep the clear either.
    #[test]
    fn scalar_work_between_the_clear_and_the_write_does_not_keep_it() {
        assert_eq!(
            ran(vec![
                allocated(1),
                clear(1),
                scalar(),
                scalar(),
                load(1),
                done()
            ]),
            [allocated(1), scalar(), scalar(), load(1), done()]
        );
    }

    /// A read of the words before they are written is the program seeing
    /// the null.
    #[test]
    fn a_clear_whose_slot_is_read_first_stays() {
        let code = vec![
            allocated(1),
            clear(1),
            Inst::Copy {
                dst: 2,
                src: 1,
                layout: STR,
            },
            load(1),
            done(),
        ];
        assert_eq!(ran(code.clone()), code);
    }

    /// An allocation between them can collect while the slot still names
    /// the dead object: that is the safepoint the rule keeps the clear
    /// across.
    #[test]
    fn an_allocation_between_them_keeps_the_clear() {
        let code = vec![allocated(1), clear(1), allocated(2), load(1), done()];
        assert_eq!(ran(code.clone()), code);
    }

    /// And a write of another root, allocating or not, shows the collector a
    /// frame it was never shown before: the conservative rule keeps the
    /// clear rather than ask whether the object is reachable from elsewhere.
    #[test]
    fn a_root_written_between_them_keeps_the_clear() {
        let code = vec![allocated(1), clear(1), load(5), load(1), done()];
        assert_eq!(ran(code.clone()), code);
        let code = vec![allocated(1), clear(1), clear(2), load(1), done()];
        assert_eq!(ran(code.clone()), code);
    }

    /// An allocation *as* the redefinition is the same safepoint.
    #[test]
    fn an_allocating_redefinition_keeps_the_clear() {
        let code = vec![allocated(1), clear(1), allocated(1), done()];
        assert_eq!(ran(code.clone()), code);
    }

    /// Every way on has to end the window: one arm that traps without
    /// writing the words keeps the clear, and two arms that both write them
    /// drop it. A later clear of the same words writes the same null and
    /// counts — and is itself dropped, because the `return` after it ends its
    /// own window.
    #[test]
    fn every_arm_of_a_branch_has_to_end_the_window() {
        let kept = vec![
            allocated(1),
            clear(1),
            Inst::BranchFalse { cond: 4, to: 5 },
            load(1),
            done(),
            trapped(),
        ];
        assert_eq!(ran(kept.clone()), kept);
        assert_eq!(
            ran(vec![
                allocated(1),
                clear(1),
                Inst::BranchFalse { cond: 4, to: 5 },
                load(1),
                done(),
                clear(1),
                done(),
            ]),
            [
                allocated(1),
                Inst::BranchFalse { cond: 4, to: 4 },
                load(1),
                done(),
                done(),
            ]
        );
    }

    /// Issue #514's F7b: a `return` pops the frame, so a clear that nothing
    /// but quiet work stands between and the function's exit is a null
    /// nobody reads.
    #[test]
    fn a_return_after_quiet_work_ends_the_window() {
        assert_eq!(
            ran(vec![allocated(1), clear(1), scalar(), scalar(), done()]),
            [allocated(1), scalar(), scalar(), done()]
        );
    }

    /// And every arm counts: one that redefines the words and one that
    /// returns without reading them both end the window.
    #[test]
    fn a_branch_whose_arms_redefine_or_return_drops_the_clear() {
        assert_eq!(
            ran(vec![
                allocated(1),
                clear(1),
                Inst::BranchFalse { cond: 4, to: 5 },
                load(1),
                done(),
                scalar(),
                done(),
            ]),
            [
                allocated(1),
                Inst::BranchFalse { cond: 4, to: 4 },
                load(1),
                done(),
                scalar(),
                done(),
            ]
        );
    }

    /// A `return` that carries the cleared words away is the caller reading
    /// them: the clear is part of the answer.
    #[test]
    fn a_return_that_answers_the_words_keeps_the_clear() {
        let code = vec![allocated(1), clear(1), scalar(), Inst::Return { src: 1 }];
        let mut answering = function(code.clone());
        answering.returns = STR;
        assert_eq!(ran_over(answering), code);
    }

    /// The window up to the `return` is still a window: an allocation in it
    /// is a safepoint with the slot holding the dead object, and a root
    /// written in it is a frame the collector was never shown.
    #[test]
    fn what_keeps_a_clear_before_a_redefinition_keeps_it_before_a_return() {
        let code = vec![allocated(1), clear(1), allocated(2), done()];
        assert_eq!(ran(code.clone()), code);
        let code = vec![allocated(1), clear(1), load(5), done()];
        assert_eq!(ran(code.clone()), code);
    }

    /// A trap is not a `return`: it leaves the frame standing, and the clear
    /// before it stays.
    #[test]
    fn a_trap_keeps_the_clear() {
        let code = vec![allocated(1), clear(1), scalar(), trapped()];
        assert_eq!(ran(code.clone()), code);
    }

    /// A name bound to the words at the `return` itself is something a
    /// debugger stopped there would print.
    #[test]
    fn a_name_bound_at_the_return_keeps_the_clear() {
        let code = vec![allocated(1), clear(1), scalar(), done()];
        let mut named = function(code.clone());
        named.locals = vec![Local {
            name: Arc::from("x"),
            slot: 1,
            layout: STR,
            from: 3,
            to: 4,
        }];
        assert_eq!(ran_over(named), code);
    }

    /// A loop that comes back round without writing the words never
    /// redefines them.
    #[test]
    fn a_loop_that_never_writes_the_words_keeps_the_clear() {
        let code = vec![
            allocated(1),
            clear(1),
            scalar(),
            Inst::BranchFalse { cond: 4, to: 2 },
            done(),
        ];
        assert_eq!(ran(code.clone()), code);
    }

    /// A write of one word of a two-word clear leaves the other one holding
    /// what the clear would have zeroed; a write of both does not.
    #[test]
    fn a_write_of_part_of_the_words_keeps_the_clear() {
        let pair = Inst::Clear {
            slot: 5,
            layout: PAIR,
        };
        let code = vec![allocated(5), pair.clone(), load(5), done()];
        assert_eq!(ran(code.clone()), code);
        let whole = Inst::Copy {
            dst: 5,
            src: 1,
            layout: PAIR,
        };
        assert_eq!(
            ran(vec![allocated(5), pair, whole.clone(), done()]),
            [allocated(5), whole, done()]
        );
    }

    /// A name bound to the slot inside the window is something a debugger
    /// would print, and it used to print null.
    #[test]
    fn a_name_bound_in_the_window_keeps_the_clear() {
        let code = vec![allocated(1), clear(1), scalar(), load(1), done()];
        let mut named = function(code.clone());
        named.locals = vec![Local {
            name: Arc::from("x"),
            slot: 1,
            layout: STR,
            from: 2,
            to: 4,
        }];
        assert_eq!(ran_over(named), code);
    }

    /// A slot whose address the function formed can be read through the
    /// address without being named.
    #[test]
    fn a_slot_whose_address_was_taken_keeps_the_clear() {
        let code = vec![
            Inst::AddrOfSlot { dst: 3, slot: 1 },
            allocated(1),
            clear(1),
            load(1),
            done(),
        ];
        assert_eq!(ran(code.clone()), code);
    }
}
