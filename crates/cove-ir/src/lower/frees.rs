//! Dropping the clears that provably free nothing.
//!
//! [`Inst::Clear`] earns its store by turning a dead reference slot into
//! null, so that the collector — which reads a frame's slots through a
//! [`RefMap`](crate::RefMap) that cannot change with the program counter —
//! stops seeing an object the program has finished with. That is the whole
//! of what the instruction is for, and it means a clear is worth emitting
//! exactly when the word it zeroes is holding something a collection could
//! otherwise not reclaim.
//!
//! Two kinds of word are not:
//!
//! - **A word holding an interned string.** `Machine::interned` is
//!   `vec![0; program.strings.len()]`, filled by `Machine::intern` on first
//!   use and never emptied, and `Live::each_root` walks it *unconditionally*
//!   beside the frames. So the object an [`Inst::Str`] produces is a root of
//!   that machine from the moment it exists until the machine is dropped,
//!   and no slot holding it is what keeps it alive.
//! - **A word this frame has not written.** `Memory::push_frame` reserves
//!   the frame with `resize(…, 0)` — it says why in so many words: a
//!   `Repr::Ref` slot that has not been written must read as null, because
//!   the static map walks it anyway. Zeroing such a word writes zero over
//!   zero.
//!
//! # Three answers, not two
//!
//! `Null` and `Free` are separate lattice values, below `Unknown`, and the
//! difference between them is what a *read* would see. Zeroing a word that
//! is already zero changes nothing at all — the run is bit-identical with
//! the clear and without it — so a `Null` clear is dropped outright.
//! Dropping a `Free` one leaves the interned address where the program had
//! written null, which is invisible to the collector and visible to a load,
//! so that one carries a condition. See *Reading a word this pass stopped
//! zeroing*.
//!
//! # The analysis
//!
//! A forward *must* analysis over the function's control-flow graph, one
//! lattice value per frame word:
//!
//! - **At entry**, every word is `Null` except the parameters and the
//!   captures, which are `Unknown`. Those are written by the *caller*:
//!   `open_frame` copies each [`Arg`](crate::Arg)'s words into the run
//!   parameter *i* occupies before the callee's first instruction runs, and
//!   `enter_closure` and `Inst::CallClosure` copy the captures out of the
//!   closure object the same way. A pass that called every slot null at
//!   entry would be wrong for exactly the frames that were handed something.
//! - **The transfer** is the instruction's writes. [`Inst::Str`] makes its
//!   destination `Free`; [`Inst::Copy`] carries the source words' answer to
//!   the destination, because a copy of an interned address is that address;
//!   everything else that writes a word makes it `Unknown`.
//! - **[`Inst::Clear`] says nothing**, which is not an oversight. What it
//!   writes is null — but this pass may be about to remove it, and then the
//!   word keeps what it had. Leaving the answer alone is the one reading
//!   that stays true either way, and it is what lets the whole function be
//!   decided from one walk rather than from dropping a clear, recomputing,
//!   and repeating. It costs the second of two clears of the same slot,
//!   which is a different category from this one.
//! - **The merge is an intersection** — the higher of two answers — and a
//!   back edge is iterated rather than assumed: a loop can carry a value
//!   round to a program counter that looked safe on the first pass, and the
//!   worklist runs until nothing changes. A word is only ever raised, never
//!   lowered, so it terminates.
//!
//! # Addresses
//!
//! [`Inst::Store`] writes through an address, naming no slot, so a frame
//! word whose address has been taken can be written — and read, by
//! [`Inst::Load`] — without this pass seeing it, from this function or from
//! any callee it hands the address to.
//!
//! Of the four instructions that form an address only [`Inst::AddrOfSlot`]
//! names a frame slot. [`Inst::AddrOfField`] and [`Inst::AddrOfElem`] answer
//! `payload_addr(obj, …)`, which is in the heap, and no arithmetic on an
//! object's payload address reaches a frame. [`Inst::AddrOfPart`] is
//! `addr + at`, so it is a frame word exactly when the address it was given
//! already was one.
//!
//! So a frame word can be written through an address only if it lies in the
//! run some `AddrOfSlot` of *this* function named, and this pass holds those
//! words unknown and live for the whole body rather than invalidating the
//! frame at each store: a word no address names cannot be reached by one, at
//! any program point, from any thread. That last clause is why the answer is
//! a set and not a program point — [`Inst::Spawn`] hands a closure to a
//! thread that runs *concurrently* with the instructions after it, so
//! "unknown from here on" would not have been an answer.
//!
//! How wide that run is has to be bounded, because an `AddrOfPart` offsets
//! an address and a `Store` writes as many words as *its* layout says, and
//! either may be in a callee this frame handed the address to. The bound is
//! the widest layout the program declares: an `AddrOfSlot` names a value
//! location, a value location has a layout, and what an address formed from
//! it names stays inside it.
//!
//! That last clause is a belief about the lowering rather than something
//! checked here, and it is deliberately the same one the IR already rests
//! on. `crate::verify` declines to check `AddrOfPart`'s offset against the
//! value's extent in so many words — *what an address names is a fact about
//! the instruction that formed it* — and a store through an address that
//! left its value would already be writing another slot's words, which is
//! what makes [`RefMap`](crate::RefMap) correct in the first place.
//!
//! # Reading a word this pass stopped zeroing
//!
//! Dropping a `Free` clear changes what the slot holds — null becomes the
//! interned address — so it is sound only if nothing reads those words
//! before they are written again. The collector's read is the one this pass
//! is *about*, and holding an interned address is exactly what makes it
//! harmless. A read by the program is not, so it is checked rather than
//! assumed, with an ordinary backward liveness over the same graph: a `Free`
//! clear is dropped only if its words are also dead.
//!
//! The check is stated on the code as it arrived, and that is enough even
//! though this pass then removes some of the writes liveness counted. Take
//! any path from a dropped `Free` clear to a read of one of its words, with
//! no surviving write in between, and let `W` be the last write on that
//! path. `W` exists, because the words were dead. `W` is a clear this pass
//! dropped, so it is one of two things. A `Free` one: then its words were
//! dead after it too, and the tail of this path reads them without a write —
//! which is what dead denies. A `Null` one: then `W`'s words are provably
//! zero on *every* path to it, and nothing in this lattice ever lowers a
//! word to `Null` except the entry, so no path from the `Free` clear that
//! started this could have reached `W` at all. Neither is possible, so
//! neither is the read.
//!
//! # What the widths have to be careful about
//!
//! An instruction's destination is a *run* of words, and this pass asks two
//! different questions about that run: which words it *may* write, which has
//! to be an over-estimate, and which it *definitely* writes, which has to be
//! an under-estimate. They differ in one place. [`Inst::CallClosure`]'s
//! callee is a word in an object, so the answer's width is not a static
//! fact: the may-write is the widest answer any function in the program
//! returns, and the definitely-writes is one word, which every call writes.

use crate::inst::{Inst, Len, Slot};
use crate::layout::{Layout, LayoutId};
use crate::program::{Function, Program};

use super::dropping;

/// A word this frame has not written, which `Memory::push_frame` left zero.
const NULL: u8 = 0;
/// A word that is null, or holds an address `Machine::interned` roots for
/// the rest of the run. Clearing it releases nothing.
const FREE: u8 = 1;
/// Anything else.
const UNKNOWN: u8 = 2;

/// Writes `value` over the run of `width` words beginning at `slot`,
/// stopping at the end of the frame.
///
/// The clipping is defensive: `crate::verify` refuses a program whose
/// instruction names a run the frame does not hold, and this pass runs
/// before it.
fn fill<T: Copy>(state: &mut [T], slot: Slot, width: u32, value: T) {
    let last = (slot as usize + width as usize).min(state.len());
    for word in &mut state[(slot as usize).min(last)..last] {
        *word = value;
    }
}

/// Drops every clear whose words are already null or already rooted.
pub(super) fn drop_clears_that_free_nothing(program: &mut Program) {
    let dropped: Vec<Vec<bool>> = program
        .functions
        .iter()
        .map(|function| pointless(function, program))
        .collect();
    let Program {
        functions, tables, ..
    } = program;
    for (function, dropped) in functions.iter_mut().zip(&dropped) {
        dropping::rewrite(function, tables, dropped);
    }
}

/// Which of a function's instructions are clears that free nothing.
fn pointless(function: &Function, program: &Program) -> Vec<bool> {
    let mut dropped = vec![false; function.code.len()];
    let Some(flow) = Flow::of(function, program) else {
        return dropped;
    };
    let free = flow.free();
    let live = flow.live();
    for (at, inst) in function.code.iter().enumerate() {
        let Inst::Clear { slot, layout } = *inst else {
            continue;
        };
        let last = slot as usize + flow.width(layout) as usize;
        if last > flow.size {
            continue;
        }
        let words = slot as usize..last;
        let holds = words
            .clone()
            .map(|word| free[at][word])
            .max()
            .unwrap_or(UNKNOWN);
        let dead = !words.into_iter().any(|word| live[at][word]);
        // Zeroing words that are already zero changes nothing a run can see,
        // whoever reads them. Zeroing away an interned address does — the
        // word stops being null and starts being that address — so that one
        // is dropped only where nothing reads it.
        dropped[at] = holds == NULL || (holds == FREE && dead);
    }
    dropped
}

/// One function's control-flow graph, and what this pass needs to read off
/// the program to walk it.
struct Flow<'p> {
    program: &'p Program,
    function: &'p Function,
    /// How many words the frame has.
    size: usize,
    /// The frame words some [`Inst::AddrOfSlot`] of this function put within
    /// reach of an address. See the module documentation.
    addressed: Vec<bool>,
    /// The widest answer any function in the program returns, which is what
    /// bounds an [`Inst::CallClosure`]'s destination.
    widest: u32,
}

impl<'p> Flow<'p> {
    /// The graph for `function`, or `None` when this pass declines it.
    fn of(function: &'p Function, program: &'p Program) -> Option<Flow<'p>> {
        if function.code.is_empty() {
            return None;
        }
        let width = |id: LayoutId| program.layouts.get(id.index()).map_or(1, Layout::width);
        let size = function.reprs.len();
        // How far past the slot it names an address can reach: a value
        // location is one of the program's layouts, and what an address
        // formed from it names stays inside it — which `crate::verify`
        // records as a fact about the instruction that formed the address,
        // and is the same fact `Function::refs` already rests on. So the
        // widest layout the program declares bounds every address every
        // `Inst::AddrOfPart` and every `Inst::Store` in any callee can make
        // out of one, with no argument about which callee holds what.
        let reach = program
            .layouts
            .iter()
            .map(Layout::width)
            .max()
            .unwrap_or(1)
            .max(1);
        let mut addressed = vec![false; size];
        for inst in &function.code {
            if let Inst::AddrOfSlot { slot, .. } = *inst {
                fill(&mut addressed, slot, reach, true);
            }
        }
        let widest = program
            .functions
            .iter()
            .map(|target| width(target.returns))
            .max()
            .unwrap_or(1);
        Some(Flow {
            program,
            function,
            size,
            addressed,
            widest,
        })
    }

    fn width(&self, id: LayoutId) -> u32 {
        self.program
            .layouts
            .get(id.index())
            .map_or(1, Layout::width)
    }

    /// Where control can go from `pc`.
    fn successors(&self, pc: usize, f: &mut dyn FnMut(usize)) {
        let last = self.function.code.len() - 1;
        match self.function.code[pc] {
            Inst::Return { .. } | Inst::Trap { .. } => {}
            Inst::Jump { to } => f(to as usize),
            Inst::BranchFalse { to, .. } => {
                f(to as usize);
                if pc < last {
                    f(pc + 1);
                }
            }
            Inst::Switch { table, .. } => {
                let table = self.program.table(table);
                for &target in &table.targets {
                    f(target as usize);
                }
                f(table.default as usize);
            }
            _ => {
                if pc < last {
                    f(pc + 1);
                }
            }
        }
    }

    /// The words `inst` writes.
    ///
    /// `wide` picks the over-estimate where the two differ, which is the one
    /// place a destination's width is not a static fact. See the module
    /// documentation.
    fn writes(&self, inst: &Inst, wide: bool, f: &mut dyn FnMut(Slot, u32)) {
        let width = |id| self.width(id);
        match *inst {
            Inst::Unit { dst }
            | Inst::Bool { dst, .. }
            | Inst::Int { dst, .. }
            | Inst::Float { dst, .. }
            | Inst::Str { dst, .. }
            | Inst::Neg { dst, .. }
            | Inst::Not { dst, .. }
            | Inst::Arith { dst, .. }
            | Inst::Cmp { dst, .. }
            | Inst::ArithImm { dst, .. }
            | Inst::CmpImm { dst, .. }
            | Inst::Convert { dst, .. }
            | Inst::Len { dst, .. }
            | Inst::LayoutOf { dst, .. }
            | Inst::Alloc { dst, .. }
            | Inst::Box { dst, .. }
            | Inst::AddrOfSlot { dst, .. }
            | Inst::AddrOfField { dst, .. }
            | Inst::AddrOfElem { dst, .. }
            | Inst::AddrOfPart { dst, .. }
            | Inst::ScopeEnter { dst, .. }
            | Inst::Spawn { dst, .. } => f(dst, 1),
            Inst::Clear { slot, layout } => f(slot, width(layout)),
            Inst::Copy { dst, layout, .. }
            | Inst::Load { dst, layout, .. }
            | Inst::LoadField { dst, layout, .. }
            | Inst::LoadElem { dst, layout, .. }
            | Inst::Unbox { dst, layout, .. } => f(dst, width(layout)),
            Inst::Await { dst, answer, .. } | Inst::Settled { dst, answer, .. } => {
                f(dst, width(answer))
            }
            Inst::ScopeLeave {
                failed,
                error,
                layout,
                ..
            } => {
                f(failed, 1);
                f(error, width(layout));
            }
            Inst::Call { dst, callee, .. } => {
                let answer = match self.program.functions.get(callee.index()) {
                    Some(target) => width(target.returns),
                    None => 1,
                };
                f(dst, answer);
            }
            // The one destination whose width is a run-time fact: the callee
            // is a word in the closure object.
            Inst::CallClosure { dst, .. } => f(dst, if wide { self.widest } else { 1 }),
            Inst::CallHost { dst, op, .. } | Inst::CallResource { dst, op, .. } => {
                let answer = match self.program.host_ops.get(op.index()) {
                    Some(op) => width(op.result),
                    None => 1,
                };
                f(dst, answer);
            }
            Inst::CallBuiltin { dst, builtin, .. } => {
                let answer = match self.program.builtins.get(builtin.index()) {
                    Some(builtin) => width(builtin.result),
                    None => 1,
                };
                f(dst, answer);
            }
            // The ones that write no word of this frame. A store writes an
            // address's words, a scope instruction writes the scheduler's
            // table, a lock writes the cell's own word, and `AssertFailed`
            // writes the run's report of where an assertion failed.
            Inst::Store { .. }
            | Inst::StoreField { .. }
            | Inst::StoreElem { .. }
            | Inst::ScopeCancel { .. }
            | Inst::Cancel { .. }
            | Inst::SharedLock { .. }
            | Inst::SharedUnlock { .. }
            | Inst::AssertFailed { .. }
            | Inst::Jump { .. }
            | Inst::BranchFalse { .. }
            | Inst::Switch { .. }
            | Inst::Return { .. }
            | Inst::Trap { .. } => {}
        }
    }

    /// The words `inst` reads.
    fn reads(&self, inst: &Inst, f: &mut dyn FnMut(Slot, u32)) {
        let width = |id| self.width(id);
        let args = |id, f: &mut dyn FnMut(Slot, u32)| {
            for arg in self.program.arg_list(id) {
                f(arg.slot, width(arg.layout));
            }
        };
        match *inst {
            Inst::Unit { .. }
            | Inst::Bool { .. }
            | Inst::Int { .. }
            | Inst::Float { .. }
            | Inst::Str { .. }
            | Inst::Clear { .. }
            | Inst::Jump { .. }
            | Inst::Trap { .. }
            | Inst::ScopeEnter { .. } => {}
            Inst::Copy { src, layout, .. }
            | Inst::Box { src, layout, .. }
            | Inst::Settled {
                src,
                answer: layout,
                ..
            } => f(src, width(layout)),
            Inst::Unbox { src, .. } => f(src, 1),
            Inst::Neg { a, .. }
            | Inst::Not { a, .. }
            | Inst::ArithImm { a, .. }
            | Inst::CmpImm { a, .. }
            | Inst::Convert { a, .. } => f(a, 1),
            Inst::Arith { a, b, .. } | Inst::Cmp { a, b, .. } => {
                f(a, 1);
                f(b, 1);
            }
            Inst::BranchFalse { cond, .. } => f(cond, 1),
            Inst::Switch { on, .. } => f(on, 1),
            Inst::Return { src } => f(src, width(self.function.returns)),
            Inst::Call { args: list, .. } | Inst::CallHost { args: list, .. } => args(list, f),
            Inst::CallBuiltin { args: list, .. } => args(list, f),
            Inst::CallClosure {
                closure,
                args: list,
                ..
            } => {
                f(closure, 1);
                args(list, f);
            }
            Inst::CallResource {
                receiver,
                args: list,
                ..
            } => {
                f(receiver, 1);
                args(list, f);
            }
            Inst::Alloc { len, .. } => {
                if let Len::Slot(slot) = len {
                    f(slot, 1);
                }
            }
            Inst::LoadField { obj, .. }
            | Inst::Len { obj, .. }
            | Inst::LayoutOf { obj, .. }
            | Inst::AddrOfField { obj, .. } => f(obj, 1),
            Inst::StoreField {
                obj, src, layout, ..
            } => {
                f(obj, 1);
                f(src, width(layout));
            }
            Inst::LoadElem { obj, index, .. } | Inst::AddrOfElem { obj, index, .. } => {
                f(obj, 1);
                f(index, 1);
            }
            Inst::StoreElem {
                obj,
                index,
                src,
                layout,
            } => {
                f(obj, 1);
                f(index, 1);
                f(src, width(layout));
            }
            // Forming a slot's address is a read of it, and the widest one
            // this pass can name: what the address is for is unknown here.
            Inst::AddrOfSlot { slot, .. } => f(slot, 1),
            Inst::AddrOfPart { addr, .. } | Inst::Load { addr, .. } => f(addr, 1),
            Inst::Store { addr, src, layout } => {
                f(addr, 1);
                f(src, width(layout));
            }
            Inst::ScopeLeave { scope, .. } | Inst::ScopeCancel { scope } => f(scope, 1),
            Inst::Spawn { scope, closure, .. } => {
                f(scope, 1);
                f(closure, 1);
            }
            Inst::Await { task, .. } | Inst::Cancel { task } => f(task, 1),
            Inst::SharedLock { cell } | Inst::SharedUnlock { cell } => f(cell, 1),
            Inst::AssertFailed { message } => f(message, 1),
        }
    }

    /// What each word holds on the way *into* each instruction.
    ///
    /// The lattice is [`NULL`] below [`FREE`] below [`UNKNOWN`], the merge is
    /// the higher of two, and every program counter but the entry starts at
    /// [`NULL`] and descends. A word is only ever *raised* by a merge or by
    /// an instruction that writes it, so the worklist terminates.
    fn free(&self) -> Vec<Vec<u8>> {
        let code = &self.function.code;
        let mut into = vec![vec![NULL; self.size]; code.len()];

        // The caller's writes, which happen before the first instruction.
        let params = self.function.param_words(&self.program.layouts);
        for word in 0..params.min(self.size as u32) {
            into[0][word as usize] = UNKNOWN;
        }
        for capture in &self.function.captures {
            fill(
                &mut into[0],
                capture.slot,
                self.width(capture.layout),
                UNKNOWN,
            );
        }

        let mut queue: Vec<usize> = (0..code.len()).rev().collect();
        let mut queued = vec![true; code.len()];
        while let Some(pc) = queue.pop() {
            queued[pc] = false;
            let out = self.step(pc, &into[pc]);
            self.successors(pc, &mut |to| {
                let mut moved = false;
                for (word, holds) in into[to].iter_mut().enumerate() {
                    if *holds < out[word] {
                        *holds = out[word];
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

    /// One instruction's transfer over the lattice.
    fn step(&self, pc: usize, into: &[u8]) -> Vec<u8> {
        let mut out = into.to_vec();
        let inst = &self.function.code[pc];
        match *inst {
            // A clear says nothing here, and that is not an oversight. What
            // it writes is null — but this pass may be about to remove it,
            // and then the word keeps what it had. Leaving the answer alone
            // is the one reading that stays true either way, which is what
            // lets the whole function be decided from one walk rather than
            // from dropping a clear, recomputing, and repeating.
            Inst::Clear { .. } => {}
            // The object `Machine::intern` allocates is in
            // `Machine::interned` for the rest of the run, and
            // `Live::each_root` walks that table unconditionally.
            Inst::Str { dst, .. } => {
                if let Some(holds) = out.get_mut(dst as usize) {
                    *holds = FREE;
                }
            }
            // A copy of an interned address is that address, and a copy of
            // null is null.
            Inst::Copy { dst, src, layout } => {
                for at in 0..self.width(layout) as usize {
                    let (dst, src) = (dst as usize + at, src as usize + at);
                    let held = into.get(src).copied().unwrap_or(UNKNOWN);
                    if let Some(holds) = out.get_mut(dst) {
                        *holds = held;
                    }
                }
            }
            _ => self.writes(inst, true, &mut |slot, width| {
                fill(&mut out, slot, width, UNKNOWN);
            }),
        }
        // A word an address can reach is written by instructions that do not
        // name it, from this frame or from a callee or a child thread, so it
        // is unknown wherever it is asked about.
        for (word, holds) in out.iter_mut().enumerate() {
            if self.addressed[word] {
                *holds = UNKNOWN;
            }
        }
        out
    }

    /// Which words may be read, before being written again, after each
    /// instruction.
    fn live(&self) -> Vec<Vec<bool>> {
        let code = &self.function.code;
        let mut before: Vec<Vec<usize>> = vec![Vec::new(); code.len()];
        for pc in 0..code.len() {
            self.successors(pc, &mut |to| before[to].push(pc));
        }
        let mut out = vec![vec![false; self.size]; code.len()];
        let mut into = vec![vec![false; self.size]; code.len()];

        let mut queue: Vec<usize> = (0..code.len()).collect();
        let mut queued = vec![true; code.len()];
        while let Some(pc) = queue.pop() {
            queued[pc] = false;
            let inst = &code[pc];
            let mut state = out[pc].clone();
            // Only a write this pass is *sure* of may kill a word, so the
            // narrow answer is the one liveness asks for.
            self.writes(inst, false, &mut |slot, width| {
                fill(&mut state, slot, width, false);
            });
            self.reads(inst, &mut |slot, width| {
                fill(&mut state, slot, width, true);
            });
            // The other half of the same fact: an address is read through as
            // well as written through, and by code that names no slot.
            for (word, live) in state.iter_mut().enumerate() {
                if self.addressed[word] {
                    *live = true;
                }
            }
            if state == into[pc] {
                continue;
            }
            into[pc] = state;
            for &earlier in &before[pc] {
                let mut moved = false;
                for (word, live) in out[earlier].iter_mut().enumerate() {
                    if !*live && into[pc][word] {
                        *live = true;
                        moved = true;
                    }
                }
                if moved && !queued[earlier] {
                    queued[earlier] = true;
                    queue.push(earlier);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::inst::{CmpOp, Compare};
    use crate::layout::Shape;
    use crate::program::Capture;
    use crate::repr::{RefMap, Repr};
    use crate::{FunctionId, StrId};
    use cove_diag::Span;

    /// One word of `Int`, one `String` reference, and a two-word inline
    /// pair — which is the widest layout, and so the run an address reaches.
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

    fn function(reprs: Vec<Repr>, code: Vec<Inst>) -> Function {
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
            span: span(),
            is_async: false,
            stub: false,
        }
    }

    /// The pass's answer for one function, built by hand.
    fn ran(reprs: Vec<Repr>, code: Vec<Inst>) -> Vec<Inst> {
        ran_over(function(reprs, code))
    }

    fn ran_over(function: Function) -> Vec<Inst> {
        let mut program = Program {
            functions: vec![function],
            layouts: layouts(),
            strings: vec![Arc::from("s")],
            str_layout: STR,
            ..Program::default()
        };
        drop_clears_that_free_nothing(&mut program);
        program.function(FunctionId(0)).code.clone()
    }

    fn text() -> StrId {
        StrId(0)
    }

    /// The first of the two kinds of word a clear cannot free: one holding
    /// an object `Machine::interned` will root for the rest of the run.
    ///
    /// This is `cq.json.isSpace`, which does it four times a call.
    #[test]
    fn a_slot_holding_an_interned_string_is_not_cleared() {
        assert_eq!(
            ran(
                vec![Repr::Int, Repr::Ref, Repr::Ref, Repr::Bool],
                vec![
                    Inst::Str {
                        dst: 1,
                        text: text(),
                    },
                    Inst::Cmp {
                        on: Compare::Str,
                        op: CmpOp::Eq,
                        dst: 3,
                        a: 1,
                        b: 2,
                    },
                    Inst::Clear {
                        slot: 1,
                        layout: STR,
                    },
                    Inst::Return { src: 0 },
                ],
            ),
            [
                Inst::Str {
                    dst: 1,
                    text: text(),
                },
                Inst::Cmp {
                    on: Compare::Str,
                    op: CmpOp::Eq,
                    dst: 3,
                    a: 1,
                    b: 2,
                },
                Inst::Return { src: 0 },
            ]
        );
    }

    /// The second: a word `Memory::push_frame` left zero and this frame has
    /// not written since.
    #[test]
    fn a_slot_this_frame_has_not_written_is_not_cleared() {
        assert_eq!(
            ran(
                vec![Repr::Int, Repr::Ref],
                vec![
                    Inst::Clear {
                        slot: 1,
                        layout: STR,
                    },
                    Inst::Return { src: 0 },
                ],
            ),
            [Inst::Return { src: 0 }]
        );
    }

    /// And the clear that is worth its store stays, which is most of them.
    #[test]
    fn a_slot_an_allocation_wrote_is_cleared() {
        let code = vec![
            Inst::Alloc {
                dst: 1,
                layout: STR,
                len: Len::Count(1),
            },
            Inst::Clear {
                slot: 1,
                layout: STR,
            },
            Inst::Return { src: 0 },
        ];
        assert_eq!(ran(vec![Repr::Int, Repr::Ref], code.clone()), code);
    }

    /// A copy of an interned address is that address, so the answer travels
    /// with the words.
    #[test]
    fn a_copy_carries_the_answer_to_its_destination() {
        assert_eq!(
            ran(
                vec![Repr::Int, Repr::Ref, Repr::Ref],
                vec![
                    Inst::Str {
                        dst: 1,
                        text: text(),
                    },
                    Inst::Copy {
                        dst: 2,
                        src: 1,
                        layout: STR,
                    },
                    Inst::Clear {
                        slot: 2,
                        layout: STR,
                    },
                    Inst::Clear {
                        slot: 1,
                        layout: STR,
                    },
                    Inst::Return { src: 0 },
                ],
            ),
            [
                Inst::Str {
                    dst: 1,
                    text: text(),
                },
                Inst::Copy {
                    dst: 2,
                    src: 1,
                    layout: STR,
                },
                Inst::Return { src: 0 },
            ]
        );
    }

    /// A parameter is written by the *caller* — `open_frame` copies each
    /// argument's words into the run the parameter occupies before the first
    /// instruction runs — so it is not null at entry however little the body
    /// has done.
    #[test]
    fn a_parameter_is_not_null_at_entry() {
        let mut f = function(
            vec![Repr::Ref, Repr::Int],
            vec![
                Inst::Clear {
                    slot: 0,
                    layout: STR,
                },
                Inst::Return { src: 1 },
            ],
        );
        f.params = vec![STR];
        let code = f.code.clone();
        assert_eq!(ran_over(f), code);
    }

    /// A capture arrives the same way, out of the closure object rather than
    /// out of an argument list.
    #[test]
    fn a_capture_is_not_null_at_entry() {
        let mut f = function(
            vec![Repr::Int, Repr::Ref],
            vec![
                Inst::Clear {
                    slot: 1,
                    layout: STR,
                },
                Inst::Return { src: 0 },
            ],
        );
        f.captures = vec![Capture {
            name: Arc::from("held"),
            slot: 1,
            layout: STR,
        }];
        let code = f.code.clone();
        assert_eq!(ran_over(f), code);
    }

    /// The merge is an intersection and the back edge is iterated: on the
    /// first pass the clear looks like it follows nothing but the `Str`.
    #[test]
    fn a_loop_carries_what_it_allocated_round_to_the_clear() {
        let code = vec![
            Inst::Str {
                dst: 1,
                text: text(),
            },
            Inst::Clear {
                slot: 1,
                layout: STR,
            },
            Inst::Alloc {
                dst: 1,
                layout: STR,
                len: Len::Count(1),
            },
            Inst::BranchFalse { cond: 2, to: 1 },
            Inst::Return { src: 0 },
        ];
        assert_eq!(
            ran(vec![Repr::Int, Repr::Ref, Repr::Bool], code.clone()),
            code
        );
    }

    /// The same shape, with the loop carrying an interned address instead:
    /// the fixpoint says `Free` on every edge and the clear goes.
    #[test]
    fn a_loop_that_carries_an_interned_address_round_still_frees_nothing() {
        assert_eq!(
            ran(
                vec![Repr::Int, Repr::Ref, Repr::Bool],
                vec![
                    Inst::Str {
                        dst: 1,
                        text: text(),
                    },
                    Inst::Clear {
                        slot: 1,
                        layout: STR,
                    },
                    Inst::Str {
                        dst: 1,
                        text: text(),
                    },
                    Inst::BranchFalse { cond: 2, to: 1 },
                    Inst::Return { src: 0 },
                ],
            ),
            [
                Inst::Str {
                    dst: 1,
                    text: text(),
                },
                Inst::Str {
                    dst: 1,
                    text: text(),
                },
                Inst::BranchFalse { cond: 2, to: 1 },
                Inst::Return { src: 0 },
            ]
        );
    }

    /// A slot whose address this function formed is written by instructions
    /// that do not name it, so it is unknown wherever it is asked about —
    /// and the run is as wide as the widest layout the program declares,
    /// because an `Inst::AddrOfPart` offsets an address and an `Inst::Store`
    /// carries a layout's worth of words through one.
    #[test]
    fn a_slot_whose_address_was_taken_is_cleared() {
        let code = vec![
            Inst::AddrOfSlot { dst: 3, slot: 1 },
            Inst::Str {
                dst: 1,
                text: text(),
            },
            Inst::Str {
                dst: 2,
                text: text(),
            },
            Inst::Clear {
                slot: 1,
                layout: STR,
            },
            Inst::Clear {
                slot: 2,
                layout: STR,
            },
            Inst::Return { src: 0 },
        ];
        assert_eq!(
            ran(
                vec![Repr::Int, Repr::Ref, Repr::Ref, Repr::Addr],
                code.clone()
            ),
            // Slot 2 is inside the two-word run slot 1's address reaches, so
            // both stay; nothing here is outside it.
            code
        );
    }

    /// Dropping a clear leaves the interned address in the slot rather than
    /// null, so a clear whose words are read again stays — checked rather
    /// than assumed of the lowering.
    #[test]
    fn a_clear_whose_slot_is_read_again_stays() {
        let code = vec![
            Inst::Str {
                dst: 1,
                text: text(),
            },
            Inst::Clear {
                slot: 1,
                layout: STR,
            },
            Inst::Copy {
                dst: 2,
                src: 1,
                layout: STR,
            },
            Inst::Return { src: 0 },
        ];
        assert_eq!(
            ran(vec![Repr::Int, Repr::Ref, Repr::Ref], code.clone()),
            code
        );
    }

    /// A word that is *null* rather than merely free needs no such check:
    /// zeroing a word that is already zero changes nothing whoever reads it.
    #[test]
    fn a_clear_of_an_unwritten_slot_that_is_read_again_still_goes() {
        assert_eq!(
            ran(
                vec![Repr::Int, Repr::Ref, Repr::Ref],
                vec![
                    Inst::Clear {
                        slot: 1,
                        layout: STR,
                    },
                    Inst::Copy {
                        dst: 2,
                        src: 1,
                        layout: STR,
                    },
                    Inst::Return { src: 0 },
                ],
            ),
            [
                Inst::Copy {
                    dst: 2,
                    src: 1,
                    layout: STR,
                },
                Inst::Return { src: 0 },
            ]
        );
    }

    /// `Inst::CallClosure`'s callee is a word in an object, so how many
    /// words its destination takes is not a static fact. The widest answer
    /// any function in the program returns is what bounds it, and here that
    /// is the two-word `Pair` a second function answers — so the word *after*
    /// the destination is unknown too.
    #[test]
    fn a_closure_call_may_write_as_wide_as_the_widest_answer() {
        let mut wide = function(vec![Repr::Ref, Repr::Ref], vec![Inst::Return { src: 0 }]);
        wide.returns = PAIR;
        let caller = function(
            vec![Repr::Int, Repr::Ref, Repr::Ref, Repr::Ref],
            vec![
                Inst::Str {
                    dst: 2,
                    text: text(),
                },
                Inst::CallClosure {
                    dst: 1,
                    closure: 3,
                    args: crate::ArgsId(0),
                },
                Inst::Clear {
                    slot: 2,
                    layout: STR,
                },
                Inst::Return { src: 0 },
            ],
        );
        let code = caller.code.clone();
        let mut program = Program {
            functions: vec![caller, wide],
            layouts: layouts(),
            strings: vec![Arc::from("s")],
            args: vec![Vec::new()],
            str_layout: STR,
            ..Program::default()
        };
        drop_clears_that_free_nothing(&mut program);
        assert_eq!(program.function(FunctionId(0)).code, code);
    }

    /// And the same call with the destination one slot further away: the
    /// widest answer no longer reaches the cleared word.
    #[test]
    fn a_word_outside_that_run_is_still_answered() {
        let mut wide = function(vec![Repr::Ref, Repr::Ref], vec![Inst::Return { src: 0 }]);
        wide.returns = PAIR;
        let caller = function(
            vec![Repr::Int, Repr::Ref, Repr::Ref, Repr::Ref],
            vec![
                Inst::Str {
                    dst: 1,
                    text: text(),
                },
                Inst::CallClosure {
                    dst: 2,
                    closure: 3,
                    args: crate::ArgsId(0),
                },
                Inst::Clear {
                    slot: 1,
                    layout: STR,
                },
                Inst::Return { src: 0 },
            ],
        );
        let mut program = Program {
            functions: vec![caller, wide],
            layouts: layouts(),
            strings: vec![Arc::from("s")],
            args: vec![Vec::new()],
            str_layout: STR,
            ..Program::default()
        };
        drop_clears_that_free_nothing(&mut program);
        assert_eq!(
            program.function(FunctionId(0)).code,
            [
                Inst::Str {
                    dst: 1,
                    text: text(),
                },
                Inst::CallClosure {
                    dst: 2,
                    closure: 3,
                    args: crate::ArgsId(0),
                },
                Inst::Return { src: 0 },
            ]
        );
    }

    /// Every program counter follows the instructions that moved, which is
    /// [`super::dropping`]'s work and is exercised here because this pass
    /// drops clears in the middle of a body rather than only at its end.
    #[test]
    fn every_target_follows_the_instructions_that_moved() {
        assert_eq!(
            ran(
                vec![Repr::Int, Repr::Ref, Repr::Bool],
                vec![
                    // 0: over the clears below.
                    Inst::BranchFalse { cond: 2, to: 3 },
                    Inst::Clear {
                        slot: 1,
                        layout: STR,
                    },
                    Inst::Clear {
                        slot: 1,
                        layout: STR,
                    },
                    Inst::Int { dst: 0, value: 1 },
                    Inst::Return { src: 0 },
                ],
            ),
            [
                Inst::BranchFalse { cond: 2, to: 1 },
                Inst::Int { dst: 0, value: 1 },
                Inst::Return { src: 0 },
            ]
        );
    }
}
