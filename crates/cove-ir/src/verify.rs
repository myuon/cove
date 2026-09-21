//! A static check that a lowered program is well formed.
//!
//! The machine takes the lowering's word for a great deal: that a value
//! location fits the frame it is in, that a jump lands on an instruction,
//! that a call passes the layouts the callee declares, and — the one that
//! matters most — that a slot's [`Repr`] is what [`Function::refs`] says it
//! is. A collection walks frames using that map, so a lowering that wrote a
//! reference into a slot the map calls an `Int` would produce a dangling
//! reference at the next collection and a wrong answer some time after that.
//!
//! # A location agrees with its layout, word for word
//!
//! Every instruction that moves a value names the layout it is moving, and a
//! layout is a run of [`Repr`]s. So the check is not "the destination is a
//! reference" but "the destination's words *are* the layout's words, in
//! order". That is what makes the one-value-many-slots rule checkable: a
//! `Copy` of a three-word `Wrapper` into a location whose second word is a
//! `Float` is a fault here rather than a `Float` traced as a pointer later.
//!
//! # A width is checked, not assumed
//!
//! Two of those checks are about how far a run of words reaches, and they are
//! here because nothing downstream can make them. A value location has to fit
//! the frame it is in — `slot + width <= frame_size` — or a `Copy` near the
//! top of a frame reads or writes the frame above it, which was the shape of
//! five separate failures while this backend was being built and which
//! `Memory::copy_words` was left asserting about in a debug build. And a
//! field access has to fit the object it names, which this can say wherever
//! the object's layout is a static fact; where it is not, the machine's own
//! bounds check is what answers, from the header.
//!
//! This is where those assumptions are checked, once, before anything runs.
//! It is not a type checker: `cove-sema` already did that, and a failure here
//! is a bug in the lowering rather than a fault in the program. It exists so
//! that such a bug is a loud failure at lowering time instead of a quiet one
//! at collection time.

use crate::inst::{CmpOp, Compare, Inst, Len, Num, Slot};
use crate::intrinsic::{Carried, Category, Class};
use crate::layout::{LayoutId, Shape};
use crate::program::{Function, FunctionId, Program};
use crate::repr::{RefMap, Repr};

/// Whether a value of `shape` is a collection: a run of elements, a vector,
/// a set, a map, or a byte buffer or run.
///
/// A `StringBuilder` is not named: it is a standard-library struct over a
/// byte buffer, and no signature class matches a struct.
fn is_collection(shape: &Shape) -> bool {
    matches!(
        shape,
        Shape::Elements { .. }
            | Shape::Vector { .. }
            | Shape::Members { .. }
            | Shape::Entries { .. }
            | Shape::ByteBuffer
            | Shape::Bytes
    )
}

/// A way in which a lowered program is not well formed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invalid {
    /// The function the fault is in, as `module.name`.
    pub function: String,
    /// The instruction it is at, or `None` when the fault is the function's
    /// own — a frame whose reference map disagrees with its reprs, say.
    pub pc: Option<usize>,
    pub what: String,
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.pc {
            Some(pc) => write!(f, "{}+{pc}: {}", self.function, self.what),
            None => write!(f, "{}: {}", self.function, self.what),
        }
    }
}

/// Checks every function of `program`, reporting every fault rather than the
/// first: one lowering bug usually shows up in several places, and seeing
/// all of them is what says which one is the cause.
pub fn verify(program: &Program) -> Result<(), Vec<Invalid>> {
    let mut faults = Vec::new();
    for (index, function) in program.functions.iter().enumerate() {
        Check {
            program,
            function,
            id: FunctionId(index as u32),
            objects: Vec::new(),
            funcs: Vec::new(),
            faults: &mut faults,
        }
        .run();
    }
    if faults.is_empty() {
        Ok(())
    } else {
        Err(faults)
    }
}

/// Marks `width` words starting at `slot` as written by something
/// [`Check::slot_facts`] declines to guess about — the `Some(None)` case any
/// writer other than the one this fact is about produces.
fn poison<T>(seen: &mut [Option<Option<T>>], slot: Slot, width: u32) {
    for at in slot..slot.saturating_add(width) {
        if let Some(place) = seen.get_mut(at as usize) {
            *place = Some(None);
        }
    }
}

/// Marks `slot` as written by `id` — the answer stays `id` if every writer
/// [`Check::slot_facts`] has seen so far agrees, and becomes the poisoned
/// [`Option::None`] the moment two disagree.
fn identify<T: Copy + PartialEq>(seen: &mut [Option<Option<T>>], slot: Slot, id: T) {
    if let Some(place) = seen.get_mut(slot as usize) {
        *place = match *place {
            None => Some(Some(id)),
            Some(Some(held)) if held == id => Some(Some(id)),
            _ => Some(None),
        };
    }
}

struct Check<'a> {
    program: &'a Program,
    function: &'a Function,
    id: FunctionId,
    /// The layout of the object each reference slot holds, where the whole
    /// function agrees on one. See [`Check::slot_facts`].
    objects: Vec<Option<LayoutId>>,
    /// The callee each slot holds, where the whole function agrees on one and
    /// the writer was [`Inst::FuncRef`]. See [`Check::slot_facts`].
    funcs: Vec<Option<FunctionId>>,
    faults: &'a mut Vec<Invalid>,
}

impl Check<'_> {
    fn run(&mut self) {
        let (objects, funcs) = self.slot_facts();
        self.objects = objects;
        self.funcs = funcs;
        self.check_frame();
        for pc in 0..self.function.code.len() {
            self.check_inst(pc);
        }
        self.check_falls_off_the_end();
        self.check_reservations();
    }

    /// Which slots hold an object whose layout is a static fact, and which
    /// hold a callee whose id is one — one walk of the code answering both,
    /// because a slot is disqualified from the second the same way it is
    /// from the first.
    ///
    /// A `Repr::Ref` slot carries no layout — that is the point of the header
    /// — so in general only the machine can bound a field access. But a slot
    /// that is written by allocations alone, all naming one layout, holds
    /// either null or an object of that layout at every program counter: a
    /// slot's `Repr` is fixed for the whole function and a run is only ever
    /// reused by a location of the same words, so the *set* of layouts ever
    /// written into a slot bounds what it can hold without a walk of the
    /// control flow. One layout and no other writer is the case this can
    /// answer, and it is the common one — a lowering allocates an object and
    /// reads its fields in the same breath.
    ///
    /// The second answer is the same question about [`Inst::FuncRef`] instead
    /// of [`Inst::Alloc`]: a slot written by one, and by nothing else, holds
    /// that callee at every program counter. A temporary is given back to the
    /// pool once its value is stored, [`crate::lower::closures`] among its
    /// callers, so one slot number can hold two different closures' callees
    /// in one function — which is a second writer with a different id, and
    /// poisons the answer exactly as a second, different [`Inst::Alloc`]
    /// would.
    ///
    /// Anything else is `None`, which means the check the answer feeds is
    /// skipped rather than failed. A slot written by a call, a load or a copy
    /// holds whatever the callee or the source held, and this declines to
    /// guess.
    fn slot_facts(&self) -> (Vec<Option<LayoutId>>, Vec<Option<FunctionId>>) {
        // `Some(None)` is "written, by something that says no fact"; `None`
        // is "not written yet". The parameters and the captures are written
        // by the caller, so they start as the first.
        let mut objects: Vec<Option<Option<LayoutId>>> = vec![None; self.function.reprs.len()];
        let mut funcs: Vec<Option<Option<FunctionId>>> = vec![None; self.function.reprs.len()];
        let words = |id: LayoutId| {
            self.program
                .layouts
                .get(id.index())
                .map_or(1, |layout| layout.width())
        };
        for at in 0..self.function.param_words(&self.program.layouts) {
            poison(&mut objects, at, 1);
            poison(&mut funcs, at, 1);
        }
        for capture in &self.function.captures {
            poison(&mut objects, capture.slot, words(capture.layout));
            poison(&mut funcs, capture.slot, words(capture.layout));
        }
        for inst in &self.function.code {
            match *inst {
                // The three that say what they allocate. A `Clear` is not
                // among them and is not a writer either: it stores null, and
                // null is refused by the machine before a layout is asked
                // about. None of the three is `Inst::FuncRef`, so all three
                // poison the second answer the way any other writer does.
                Inst::Alloc { dst, layout, .. } => {
                    identify(&mut objects, dst, layout);
                    poison(&mut funcs, dst, 1);
                }
                Inst::Str { dst, .. } => {
                    identify(&mut objects, dst, self.program.str_layout);
                    poison(&mut funcs, dst, 1);
                }
                Inst::Box { dst, .. } => {
                    identify(&mut objects, dst, self.program.boxed_layout);
                    poison(&mut funcs, dst, 1);
                }
                // The one instruction that identifies a callee rather than a
                // layout. It is not an allocation, so it poisons the first
                // answer exactly as `Inst::Int` does.
                Inst::FuncRef { dst, callee } => {
                    poison(&mut objects, dst, 1);
                    identify(&mut funcs, dst, callee);
                }
                Inst::Clear { .. } | Inst::Jump { .. } | Inst::BranchFalse { .. } => {}
                Inst::Switch { .. } | Inst::Return { .. } | Inst::Trap { .. } => {}
                // Scheduler state, not objects. A `Repr::Task` and a
                // `Repr::Scope` word name a table entry, so there is no
                // layout for one of these to claim.
                Inst::ScopeEnter { dst, .. } => {
                    poison(&mut objects, dst, 1);
                    poison(&mut funcs, dst, 1);
                }
                Inst::Spawn { dst, .. } => {
                    poison(&mut objects, dst, 1);
                    poison(&mut funcs, dst, 1);
                }
                Inst::Settled { dst, .. } => {
                    poison(&mut objects, dst, 1);
                    poison(&mut funcs, dst, 1);
                }
                Inst::ScopeCancel { .. } | Inst::Cancel { .. } => {}
                // Neither writes a slot: what they change is the cell's own
                // lock word, which is not a location this frame numbers.
                Inst::SharedLock { .. } | Inst::SharedUnlock { .. } => {}
                Inst::ScopeLeave {
                    failed,
                    error,
                    layout,
                    ..
                } => {
                    poison(&mut objects, failed, 1);
                    poison(&mut funcs, failed, 1);
                    poison(&mut objects, error, words(layout));
                    poison(&mut funcs, error, words(layout));
                }
                Inst::Await { dst, answer, .. } => {
                    poison(&mut objects, dst, words(answer));
                    poison(&mut funcs, dst, words(answer));
                }
                // Writes nothing a program can read: what it writes is the
                // run's report of where an assertion failed.
                Inst::AssertFailed { .. } => {}
                Inst::Store { .. } | Inst::StoreField { .. } | Inst::StoreElem { .. } => {}
                Inst::Unit { dst } | Inst::Bool { dst, .. } => {
                    poison(&mut objects, dst, 1);
                    poison(&mut funcs, dst, 1);
                }
                Inst::Int { dst, .. } | Inst::Tag { dst, .. } | Inst::Float { dst, .. } => {
                    poison(&mut objects, dst, 1);
                    poison(&mut funcs, dst, 1);
                }
                Inst::Neg { dst, .. } | Inst::Not { dst, .. } => {
                    poison(&mut objects, dst, 1);
                    poison(&mut funcs, dst, 1);
                }
                Inst::Arith { dst, .. } | Inst::Cmp { dst, .. } => {
                    poison(&mut objects, dst, 1);
                    poison(&mut funcs, dst, 1);
                }
                Inst::ArithImm { dst, .. } | Inst::CmpImm { dst, .. } => {
                    poison(&mut objects, dst, 1);
                    poison(&mut funcs, dst, 1);
                }
                // A fused comparison writes the `Bool` its unfused pair
                // wrote, so it poisons `dst` exactly as the comparison does.
                Inst::CmpBranch { dst, .. } | Inst::CmpImmBranch { dst, .. } => {
                    poison(&mut objects, dst, 1);
                    poison(&mut funcs, dst, 1);
                }
                Inst::Convert { dst, .. } => {
                    poison(&mut objects, dst, 1);
                    poison(&mut funcs, dst, 1);
                }
                Inst::RunLoad { dst, .. } | Inst::Len { dst, .. } | Inst::LayoutOf { dst, .. } => {
                    poison(&mut objects, dst, 1);
                    poison(&mut funcs, dst, 1);
                }
                // ADR 0052's two poison `dst` exactly as `RunLoad` does rather
                // than `identify`ing it the way `Inst::Alloc` and `Inst::Str`
                // do: `GrowableAlloc` allocates the owner its storage implies —
                // `Program::buffer_layout` for bytes, the program's
                // `Shape::Vector` of the element for words — and `RunFinish`
                // answers the store its owner was holding, relabelled. Neither
                // is a layout this pass reasons about, only a `Repr`.
                Inst::GrowableAlloc { dst, .. } | Inst::RunFinish { dst, .. } => {
                    poison(&mut objects, dst, 1);
                    poison(&mut funcs, dst, 1);
                }
                // Writes no frame slot: `RunCopy` writes into the object its
                // `args` table's `dst` already names.
                Inst::RunCopy { .. } => {}
                // Writes the one its row names first: the fresh run's
                // address for a slice, the answer's offset for a search.
                // Poisoned rather than identified, as `GrowableAlloc` is — a
                // row this pass has not checked the shape of yet is not a place
                // to read a layout fact from, and a search's `dst` is an `Int`
                // and so is neither an object nor a function from then on.
                Inst::RunSlice { args, .. } | Inst::RunFind { args, .. } => {
                    if let Some(dst) = self
                        .program
                        .args
                        .get(args.index())
                        .and_then(|row| row.first())
                    {
                        poison(&mut objects, dst.slot, 1);
                        poison(&mut funcs, dst.slot, 1);
                    }
                }
                // Nor does a truncate: it lowers the owner's length word and
                // clears units of its store.
                Inst::GrowableTruncate { .. } => {}
                // Nor does ADR 0062's window: an ensure may replace the owner's
                // store word, a commit writes its length word, and a store writes
                // a byte of the run — none of them a word of this frame.
                Inst::GrowableEnsure { .. }
                | Inst::GrowableCommit { .. }
                | Inst::RunStore { .. } => {}
                // Forming the address of a slot is also a write to it, as
                // far as this is concerned: a `var` argument is that address
                // handed to a callee, and what the callee stores through it
                // lands in this frame. The checker holds the two to one type
                // and so to one layout, but a static claim about a slot
                // should not rest on an argument made somewhere else.
                Inst::AddrOfSlot { dst, slot } => {
                    poison(&mut objects, dst, 1);
                    poison(&mut funcs, dst, 1);
                    poison(&mut objects, slot, 1);
                    poison(&mut funcs, slot, 1);
                }
                Inst::AddrOfField { dst, .. } => {
                    poison(&mut objects, dst, 1);
                    poison(&mut funcs, dst, 1);
                }
                Inst::AddrOfElem { dst, .. } | Inst::AddrOfPart { dst, .. } => {
                    poison(&mut objects, dst, 1);
                    poison(&mut funcs, dst, 1);
                }
                Inst::Copy { dst, layout, .. }
                | Inst::Load { dst, layout, .. }
                | Inst::LoadField { dst, layout, .. }
                | Inst::LoadElem { dst, layout, .. }
                | Inst::Unbox { dst, layout, .. } => {
                    poison(&mut objects, dst, words(layout));
                    poison(&mut funcs, dst, words(layout));
                }
                Inst::Call { dst, callee, .. } => {
                    let width = match self.program.functions.get(callee.index()) {
                        Some(target) => words(target.returns),
                        None => 1,
                    };
                    poison(&mut objects, dst, width);
                    poison(&mut funcs, dst, width);
                }
                Inst::CallClosure { dst, result, .. } => {
                    poison(&mut objects, dst, words(result));
                    poison(&mut funcs, dst, words(result));
                }
                Inst::CallHost { dst, op, .. } | Inst::CallResource { dst, op, .. } => {
                    let width = match self.program.host_ops.get(op.index()) {
                        Some(op) => words(op.result),
                        None => 1,
                    };
                    poison(&mut objects, dst, width);
                    poison(&mut funcs, dst, width);
                }
                Inst::IntrinsicCall { dst, site, .. } => {
                    let width = match self.program.intrinsic_sites.get(site.index()) {
                        Some(builtin) => words(builtin.result),
                        None => 1,
                    };
                    poison(&mut objects, dst, width);
                    poison(&mut funcs, dst, width);
                }
            }
        }
        (
            objects.into_iter().map(Option::flatten).collect(),
            funcs.into_iter().map(Option::flatten).collect(),
        )
    }

    fn fault(&mut self, pc: Option<usize>, what: impl Into<String>) {
        self.faults.push(Invalid {
            function: self.function.qualified(),
            pc,
            what: what.into(),
        });
    }

    /// The frame's own invariants: the parameters fit, the answer's layout
    /// exists, the spans line up, the reference map is the one the reprs
    /// imply, and every name is of a location and a range this function has.
    fn check_frame(&mut self) {
        let size = self.function.frame_size();
        let mut at = 0;
        for (index, param) in self.function.params.clone().into_iter().enumerate() {
            if !self.layout_exists(None, param) {
                continue;
            }
            let width = self.program.layout(param).width();
            if !self.fits(None, at, param, &format!("parameter {index}")) {
                return;
            }
            at += width;
        }
        if !self.layout_exists(None, self.function.returns) {
            return;
        }
        if self.function.spans.len() != self.function.code.len() {
            self.fault(
                None,
                format!(
                    "has {} instructions but {} spans",
                    self.function.code.len(),
                    self.function.spans.len()
                ),
            );
        }
        let expected = RefMap::of(&self.function.reprs);
        if expected != self.function.refs {
            self.fault(
                None,
                "reference map disagrees with the frame's reprs, so a collection would \
                 scan the wrong slots"
                    .to_string(),
            );
        }
        for capture in self.function.captures.clone() {
            if !self.layout_exists(None, capture.layout) {
                continue;
            }
            let name = capture.name.clone();
            self.fits(
                None,
                capture.slot,
                capture.layout,
                &format!("capture `{name}`"),
            );
        }
        // Nothing runs a local — it is read when a person asks what a frame
        // holds — so what is checked is that it *names* something that
        // exists: a location the frame has, over a range of this function's
        // code. A local pointing past either would be a debugger's answer
        // about a slot or an instruction that is not there.
        for index in 0..self.function.locals.len() {
            // One `Local` at a time rather than `self.function.locals.clone()`:
            // the loop body needs `&mut self` for `fault`, which a borrow of
            // the table itself would still be holding, but a name and four
            // `Copy` fields cost far less than a second copy of the table.
            let local = self.function.locals[index].clone();
            let name = local.name;
            if self.layout_exists(None, local.layout) {
                self.fits(None, local.slot, local.layout, &format!("local `{name}`"));
            }
            if local.from > local.to {
                self.fault(
                    None,
                    format!(
                        "local `{name}` is bound at {} and freed at {}",
                        local.from, local.to
                    ),
                );
            } else if local.to as usize > self.function.code.len() {
                self.fault(
                    None,
                    format!(
                        "local `{name}` is live to {} and the function has {} instructions",
                        local.to,
                        self.function.code.len()
                    ),
                );
            }
        }
        // The same question about the same kind of table, asked of the bodies
        // an expansion wrote here. `Inlined` is `Local`'s shape and is read by
        // the same readers — an error's chain, a backtrace, a profile — so a
        // range or a slot that names nothing is the same fault, and it is one
        // nothing at run time would notice: an expansion is not executed
        // *through* its record, it is merely described by it.
        for index in 0..self.function.inlined.len() {
            let held = self.function.inlined[index].clone();
            let callee = held.callee.index();
            let name = match self.program.functions.get(callee) {
                Some(function) => function.qualified(),
                None => {
                    self.fault(
                        None,
                        format!("an expanded body names function {callee}, which is not one"),
                    );
                    continue;
                }
            };
            if held.from > held.to {
                self.fault(
                    None,
                    format!(
                        "the expansion of `{name}` runs from {} to {}",
                        held.from, held.to
                    ),
                );
            } else if held.to as usize > self.function.code.len() {
                self.fault(
                    None,
                    format!(
                        "the expansion of `{name}` ends at {} and the function has {} \
                         instructions",
                        held.to,
                        self.function.code.len()
                    ),
                );
            }
            for local in held.locals {
                let bound = local.name;
                if self.layout_exists(None, local.layout) {
                    self.fits(
                        None,
                        local.slot,
                        local.layout,
                        &format!("local `{bound}` of the expanded `{name}`"),
                    );
                }
                if local.from > local.to || local.to as usize > self.function.code.len() {
                    self.fault(
                        None,
                        format!(
                            "local `{bound}` of the expanded `{name}` is live from {} to {}, \
                             and the function has {} instructions",
                            local.from,
                            local.to,
                            self.function.code.len()
                        ),
                    );
                }
            }
        }
        let _ = size;
    }

    /// A function whose last instruction can fall through has nowhere to go.
    fn check_falls_off_the_end(&mut self) {
        let last = self.function.code.len().checked_sub(1);
        let ends = matches!(
            last.map(|pc| &self.function.code[pc]),
            Some(Inst::Return { .. } | Inst::Jump { .. } | Inst::Switch { .. } | Inst::Trap { .. })
        );
        if !ends {
            self.fault(
                last,
                "the last instruction can fall through, and there is nothing after it",
            );
        }
    }

    fn check_inst(&mut self, pc: usize) {
        let inst = self.function.code[pc].clone();
        let at = Some(pc);
        match inst {
            Inst::Unit { dst } => self.expect(at, dst, &[Repr::Unit]),
            Inst::Bool { dst, .. } => self.expect(at, dst, &[Repr::Bool]),
            Inst::Int { dst, .. } => self.expect(at, dst, &[Repr::Int, Repr::Duration]),
            // The one place a case index is written, and the only check that
            // it names a case of the enum it claims to. `Inst::Int` could
            // write the same word and be bounded against nothing.
            Inst::Tag { dst, layout, case } => {
                self.expect(at, dst, &[Repr::Tag]);
                if self.in_range(at, layout.index(), self.program.layouts.len(), "layout") {
                    match &self.program.layout(layout).shape {
                        crate::layout::Shape::Enum { cases, .. } => {
                            if case.index() >= cases.len() {
                                let count = cases.len();
                                self.fault(
                                    at,
                                    format!(
                                        "names {case} of {}, which has {count} case(s)",
                                        self.program.layout(layout).name
                                    ),
                                );
                            }
                        }
                        _ => self.fault(
                            at,
                            format!(
                                "writes a case of {}, which is not an enum",
                                self.program.layout(layout).name
                            ),
                        ),
                    }
                }
            }
            Inst::FuncRef { dst, callee } => {
                if !self.in_range(at, callee.index(), self.program.functions.len(), "function") {
                    return;
                }
                self.expect(at, dst, &[Repr::Int]);
            }
            Inst::Float { dst, .. } => self.expect(at, dst, &[Repr::Float]),
            Inst::Str { dst, text } => {
                self.expect(at, dst, &[Repr::Ref]);
                self.in_range(at, text.index(), self.program.strings.len(), "string");
            }
            Inst::Copy { dst, src, layout } => {
                if self.layout_exists(at, layout) {
                    self.fits(at, dst, layout, "the destination of a copy");
                    self.fits(at, src, layout, "the source of a copy");
                }
            }
            Inst::Clear { slot, layout } => {
                if self.layout_exists(at, layout) {
                    self.fits(at, slot, layout, "what a clear zeroes");
                }
            }
            Inst::Neg { num, dst, a } => {
                let want = Self::numeric(num);
                self.expect(at, dst, want);
                self.expect(at, a, want);
            }
            Inst::Arith { num, dst, a, b, .. } => {
                let want = Self::numeric(num);
                self.expect(at, dst, want);
                self.expect(at, a, want);
                self.expect(at, b, want);
            }
            // The same claims `Inst::Arith` makes, less the one about an
            // operand that is not there. `Num` is not a field: an immediate is
            // an `i64`, so the reading is the integer one, and a `Duration` is
            // nanoseconds and admitted for the same reason it is there.
            Inst::ArithImm { dst, a, .. } => {
                let want = Self::numeric(Num::Int);
                self.expect(at, dst, want);
                self.expect(at, a, want);
            }
            Inst::CmpImm { op, dst, a, .. } => {
                self.unordered(at, op);
                self.expect(at, dst, &[Repr::Bool]);
                self.expect(at, a, &[Repr::Int, Repr::Duration]);
            }
            // ADR 0059's three-way order answers an `Int`, and only over the
            // comparisons a key's order is one of: see `CmpOp::Order`.
            Inst::Cmp {
                on,
                op: CmpOp::Order,
                dst,
                a,
                b,
            } => {
                if matches!(on, Compare::Float | Compare::Identity) {
                    self.fault(
                        at,
                        format!("a three-way order over {on:?} is not an order a key has"),
                    );
                }
                self.expect(at, dst, &[Repr::Int]);
                let want = Self::compared(on);
                self.expect(at, a, want);
                self.expect(at, b, want);
            }
            Inst::Cmp { on, dst, a, b, .. } => {
                self.expect(at, dst, &[Repr::Bool]);
                let want = Self::compared(on);
                self.expect(at, a, want);
                self.expect(at, b, want);
            }
            Inst::Not { dst, a } => {
                self.expect(at, dst, &[Repr::Bool]);
                self.expect(at, a, &[Repr::Bool]);
            }
            Inst::Convert { to, dst, a } => {
                let (from, into) = match to {
                    crate::inst::Convert::IntToFloat => (Repr::Int, Repr::Float),
                    crate::inst::Convert::FloatToInt => (Repr::Float, Repr::Int),
                    crate::inst::Convert::DurationToInt => (Repr::Duration, Repr::Int),
                    crate::inst::Convert::IntToDuration => (Repr::Int, Repr::Duration),
                };
                self.expect(at, a, &[from]);
                self.expect(at, dst, &[into]);
            }
            Inst::Jump { to } => self.target(at, to),
            Inst::BranchFalse { cond, to } => {
                self.expect(at, cond, &[Repr::Bool]);
                self.target(at, to);
            }
            // Exactly `Inst::Cmp` and `Inst::CmpImm`'s claims, and exactly
            // `Inst::BranchFalse`'s — which is what "the fused instruction is
            // semantically the two it replaces" means to a verifier. `dst`
            // does not have to be asked about as a condition: it was just
            // required to be a `Bool` as the destination.
            Inst::CmpBranch {
                on,
                op,
                dst,
                a,
                b,
                target,
            } => {
                self.unordered(at, op);
                self.expect(at, dst, &[Repr::Bool]);
                let want = Self::compared(on);
                self.expect(at, a, want);
                self.expect(at, b, want);
                self.target(at, target);
            }
            Inst::CmpImmBranch {
                op, dst, a, target, ..
            } => {
                self.unordered(at, op);
                self.expect(at, dst, &[Repr::Bool]);
                self.expect(at, a, &[Repr::Int, Repr::Duration]);
                self.target(at, target);
            }
            Inst::Switch { on, table } => {
                // The discriminant of an enum location is its first word and
                // is an `Int`; so is the layout id a `dyn` dispatch switches
                // on. Nothing else is dispatched on, and a slot's `Repr` is
                // the strongest thing a static check has to say about which
                // word this is — a location's extent is a fact about the
                // instruction that produced the word, not about the frame.
                self.expect(at, on, &[Repr::Tag, Repr::Int]);
                if self.in_range(at, table.index(), self.program.tables.len(), "table") {
                    let table = self.program.table(table).clone();
                    for to in table.targets.iter().chain(std::iter::once(&table.default)) {
                        self.target(at, *to);
                    }
                }
            }
            Inst::Return { src } => {
                let returns = self.function.returns;
                if self.layout_exists(at, returns) {
                    self.fits(at, src, returns, "what is returned");
                }
            }
            Inst::Call { dst, callee, args } => {
                if !self.in_range(at, callee.index(), self.program.functions.len(), "function") {
                    return;
                }
                let target = self.program.function(callee);
                let returns = target.returns;
                let params = target.params.clone();
                let name = target.qualified();
                if self.layout_exists(at, returns) {
                    self.fits(at, dst, returns, "the destination of a call");
                }
                self.check_args(at, args, &params, &name);
            }
            // The callee is a word read out of an object, and the answer's
            // layout is not: the checker settled this call against the
            // callee's function type, so how wide the destination has to be
            // is as static here as at any other call. It is checked the same
            // way, from the layout the instruction carries.
            Inst::CallClosure {
                dst,
                closure,
                args,
                result,
            } => {
                self.expect(at, closure, &[Repr::Ref]);
                if self.layout_exists(at, result) {
                    self.fits(at, dst, result, "the answer of a closure call");
                }
                self.each_arg(at, args);
            }
            Inst::CallHost { dst, op, args } => {
                if self.in_range(at, op.index(), self.program.host_ops.len(), "host op") {
                    let result = self.program.host_op(op).result;
                    if self.layout_exists(at, result) {
                        self.fits(at, dst, result, "the answer of a host call");
                    }
                }
                self.each_arg(at, args);
            }
            // The receiver is a `Repr::Host` word and never an argument: the
            // registry takes the handle as the thing being addressed and the
            // host is handed only what follows it. Whether the word names a
            // resource this run holds is the machine's question, because a
            // handle is a name the *host* minted and nothing static can say
            // which one a slot will hold.
            Inst::CallResource {
                dst,
                receiver,
                op,
                args,
            } => {
                self.expect(at, receiver, &[Repr::Host]);
                if self.in_range(at, op.index(), self.program.host_ops.len(), "host op") {
                    let held = self.program.host_op(op).clone();
                    if held.resource.is_none() {
                        let named = held.qualified();
                        self.fault(
                            at,
                            format!(
                                "is addressed to a resource, but `{named}` names no resource kind"
                            ),
                        );
                    }
                    if self.layout_exists(at, held.result) {
                        self.fits(at, dst, held.result, "the answer of a host call");
                    }
                }
                self.each_arg(at, args);
            }
            Inst::IntrinsicCall { dst, site, args } => {
                if self.in_range(
                    at,
                    site.index(),
                    self.program.intrinsic_sites.len(),
                    "intrinsic site",
                ) {
                    let called = *self.program.intrinsic_site(site);
                    if self.layout_exists(at, called.result) {
                        self.fits(at, dst, called.result, "the answer of a builtin");
                        self.check_signature(at, called, args);
                    }
                }
                self.each_arg(at, args);
            }
            Inst::Alloc { dst, layout, len } => {
                self.expect(at, dst, &[Repr::Ref]);
                if self.layout_exists(at, layout) {
                    let described = self.program.layout(layout);
                    // A box's payload is one word of `LayoutId` and then the
                    // value that layout describes, so its width is in the
                    // header rather than in the shape — and `Alloc` sizes an
                    // object by its shape. Allocating one here would make a
                    // box of a two-word value one word short and the copy
                    // into it would run off the end of the object.
                    // `Inst::Box` is the only correct allocator for one,
                    // because it is the only one that is told what is going
                    // in.
                    if matches!(described.shape, Shape::Boxed) {
                        let name = described.name.clone();
                        self.fault(
                            at,
                            format!(
                                "allocates a `{name}`, whose width the header carries and \
                                 the shape does not; a box is allocated by `box`, which \
                                 knows what is going into it"
                            ),
                        );
                    }
                }
                if let Len::Slot(slot) = len {
                    self.expect(at, slot, &[Repr::Int]);
                }
            }
            Inst::LoadField {
                dst,
                obj,
                at: word,
                layout,
            } => {
                self.expect(at, obj, &[Repr::Ref]);
                if self.layout_exists(at, layout) {
                    self.fits(at, dst, layout, "what a field is read into");
                    self.reaches(at, obj, word, layout, "read");
                }
            }
            Inst::StoreField {
                obj,
                at: word,
                src,
                layout,
            } => {
                self.expect(at, obj, &[Repr::Ref]);
                if self.layout_exists(at, layout) {
                    self.fits(at, src, layout, "what a field is written from");
                    self.reaches(at, obj, word, layout, "written");
                }
                self.check_closure_callee(at, obj, word, src);
            }
            Inst::LoadElem {
                dst,
                obj,
                index,
                layout,
            } => {
                self.expect(at, obj, &[Repr::Ref]);
                self.expect(at, index, &[Repr::Int]);
                if self.layout_exists(at, layout) {
                    self.fits(at, dst, layout, "what an element is read into");
                }
            }
            Inst::StoreElem {
                obj,
                index,
                src,
                layout,
            } => {
                self.expect(at, obj, &[Repr::Ref]);
                self.expect(at, index, &[Repr::Int]);
                if self.layout_exists(at, layout) {
                    self.fits(at, src, layout, "what an element is written from");
                }
            }
            Inst::RunLoad {
                dst,
                run,
                index,
                storage,
            } => {
                self.expect(at, run, &[Repr::Ref]);
                self.expect(at, index, &[Repr::Int]);
                self.admit_storage(at, "loads a unit of", storage);
                self.expect(at, dst, &[Repr::Int]);
            }
            Inst::RunCopy { args, storage } => self.check_run_copy(at, args, storage),
            Inst::RunSlice { args, storage } => self.check_run_slice(at, args, storage),
            Inst::RunFind { args, storage } => self.check_run_find(at, args, storage),
            // The growable family's admission table. Phase 2 of ADR 0058
            // admitted `PackedBytes` for every member and nothing else, with a
            // byte finish a UTF-8 finish into `Program::str_layout`; Phase 3
            // admits `Words` for a push and a finish, which are what
            // `Vector.push` and `Vector.freeze` became.
            //
            // An allocation admits both storages, and there is nothing here to
            // hold the element to beyond existing: the two layouts a word
            // allocation needs are *derived* from it — the program's
            // `Shape::Vector` of the element and the growable `Shape::Elements`
            // of it — so what a lowering could get wrong is not a field this
            // instruction carries. A program whose table declares neither is
            // refused by the machine that looks them up, in the words the
            // lookup has.
            Inst::GrowableAlloc {
                dst,
                capacity,
                storage,
            } => {
                if let crate::Storage::Words(elem) = storage {
                    self.layout_exists(at, elem);
                }
                self.expect(at, dst, &[Repr::Ref]);
                self.expect(at, capacity, &[Repr::Int]);
            }
            // ADR 0062's window, admitted over both storages from the start: the
            // protocol is one for a vector and a byte buffer. What makes a window
            // sound is not checked here, one instruction at a time, but by
            // `Check::check_reservations` over the block.
            Inst::GrowableEnsure {
                owner,
                additional: count,
                storage,
            }
            | Inst::GrowableCommit {
                owner,
                count,
                storage,
            } => {
                if let crate::Storage::Words(elem) = storage {
                    self.layout_exists(at, elem);
                }
                self.expect(at, owner, &[Repr::Ref]);
                self.expect(at, count, &[Repr::Int]);
            }
            Inst::RunStore {
                run,
                index,
                src,
                storage,
            } => {
                self.expect(at, run, &[Repr::Ref]);
                self.expect(at, index, &[Repr::Int]);
                self.admit_storage(at, "stores a unit of", storage);
                self.expect(at, src, &[Repr::Int]);
            }
            // The one growable member admitted over words alone: a byte builder
            // has no operation that takes a byte back out.
            Inst::GrowableTruncate {
                owner,
                len,
                storage,
            } => {
                match storage {
                    crate::Storage::PackedBytes => self.fault(
                        at,
                        "truncates a run of packed bytes, and this instruction admits only words"
                            .to_string(),
                    ),
                    crate::Storage::Words(elem) => {
                        self.layout_exists(at, elem);
                    }
                }
                self.expect(at, owner, &[Repr::Ref]);
                self.expect(at, len, &[Repr::Int]);
            }
            Inst::RunFinish {
                dst,
                owner,
                target,
                validation,
                storage,
            } => {
                if let crate::Storage::Words(elem) = storage {
                    self.check_word_finish(at, elem, target, validation);
                } else {
                    if validation != crate::Validation::Utf8 {
                        self.fault(
                            at,
                            format!(
                                "finishes a run of packed bytes with validation \
                                 `{validation:?}`, and a byte run becomes a `String` only \
                                 through `Utf8`"
                            ),
                        );
                    }
                    if target != self.program.str_layout {
                        let named = self.name_of(target);
                        let string = self.name_of(self.program.str_layout);
                        self.fault(
                            at,
                            format!(
                                "finishes a run of packed bytes into `{named}`, and a byte \
                                 run finishes into `{string}`"
                            ),
                        );
                    }
                }
                self.expect(at, dst, &[Repr::Ref]);
                self.expect(at, owner, &[Repr::Ref]);
            }
            Inst::Len { dst, obj } => {
                self.expect(at, obj, &[Repr::Ref]);
                self.expect(at, dst, &[Repr::Int]);
            }
            Inst::LayoutOf { dst, obj } => {
                self.expect(at, obj, &[Repr::Ref]);
                self.expect(at, dst, &[Repr::Int]);
            }
            Inst::AddrOfSlot { dst, slot } => {
                self.expect(at, dst, &[Repr::Addr]);
                self.repr(at, slot);
            }
            Inst::AddrOfField { dst, obj, at: word } => {
                self.expect(at, dst, &[Repr::Addr]);
                self.expect(at, obj, &[Repr::Ref]);
                self.reaches_word(at, obj, word, 1, "addressed");
            }
            Inst::AddrOfElem {
                dst,
                obj,
                index,
                layout,
            } => {
                self.expect(at, dst, &[Repr::Addr]);
                self.expect(at, obj, &[Repr::Ref]);
                self.expect(at, index, &[Repr::Int]);
                self.layout_exists(at, layout);
            }
            // Nothing bounds `at` against the value the address names. A
            // frame records what each slot *holds* and not how far the value
            // an address points into reaches, so the extent is a fact about
            // the instruction that formed the address rather than about this
            // function — the same limit `Inst::Switch`'s operand is under.
            Inst::AddrOfPart { dst, addr, .. } => {
                self.expect(at, dst, &[Repr::Addr]);
                self.expect(at, addr, &[Repr::Addr]);
            }
            Inst::Load { dst, addr, layout } => {
                self.expect(at, addr, &[Repr::Addr]);
                if self.layout_exists(at, layout) {
                    self.fits(at, dst, layout, "what a load answers");
                }
            }
            Inst::Store { addr, src, layout } => {
                self.expect(at, addr, &[Repr::Addr]);
                if self.layout_exists(at, layout) {
                    self.fits(at, src, layout, "what a store writes");
                }
            }
            Inst::Box { dst, src, layout } => {
                self.expect(at, dst, &[Repr::Ref]);
                if self.layout_exists(at, layout) {
                    self.fits(at, src, layout, "what is boxed");
                }
            }
            Inst::Unbox { dst, src, layout } => {
                self.expect(at, src, &[Repr::Ref]);
                if self.layout_exists(at, layout) {
                    self.fits(at, dst, layout, "what a box is opened into");
                }
            }
            // ---- tasks ---------------------------------------------------
            Inst::ScopeEnter { dst, name } => {
                self.expect(at, dst, &[Repr::Scope]);
                self.in_range(at, name.index(), self.program.strings.len(), "string");
            }
            Inst::ScopeCancel { scope } => self.expect(at, scope, &[Repr::Scope]),
            // The error location is the *enclosing* function's `Err`
            // payload, not the child's answer: what a failing child gives
            // the scope is a value to pass on, and where it goes is decided
            // by the function the scope was written in. The machine holds
            // the child's own layout to this one and refuses a disagreement,
            // because a run of words copied at the wrong width is the one
            // fault this crate exists to make loud.
            Inst::ScopeLeave {
                scope,
                failed,
                error,
                layout,
            } => {
                self.expect(at, scope, &[Repr::Scope]);
                self.expect(at, failed, &[Repr::Bool]);
                if self.layout_exists(at, layout) {
                    self.fits(at, error, layout, "what a failing child leaves");
                }
            }
            Inst::Spawn {
                dst,
                scope,
                closure,
                answer,
            } => {
                self.expect(at, dst, &[Repr::Task]);
                self.expect(at, scope, &[Repr::Scope]);
                self.expect(at, closure, &[Repr::Ref]);
                self.layout_exists(at, answer);
            }
            Inst::Await { dst, task, answer } => {
                self.expect(at, task, &[Repr::Task]);
                if self.layout_exists(at, answer) {
                    self.fits(at, dst, answer, "what an await answers");
                }
            }
            Inst::Cancel { task } => self.expect(at, task, &[Repr::Task]),
            // The words go into an object of the same shape a spawned
            // task's answer goes into, so the same question is asked of
            // them: that the location they are read out of is as wide as
            // the layout says.
            Inst::Settled { dst, src, answer } => {
                self.expect(at, dst, &[Repr::Task]);
                if self.layout_exists(at, answer) {
                    self.fits(at, src, answer, "what a settled task answers");
                }
            }

            // ---- cells ---------------------------------------------------
            // A cell is an ordinary object in the run's heap, so the operand
            // is an ordinary `Repr::Ref` word. That the two come in pairs is
            // not checked here: which cells a path is holding is a fact about
            // control flow, and this is a fact about one instruction — the
            // same limit `Inst::ScopeCancel` is under.
            Inst::SharedLock { cell } | Inst::SharedUnlock { cell } => {
                self.expect(at, cell, &[Repr::Ref])
            }

            Inst::Trap { message } => {
                self.in_range(at, message.index(), self.program.strings.len(), "string");
            }
            Inst::AssertFailed { message } => {
                self.expect(at, message, &[Repr::Ref]);
            }
        }
    }

    fn numeric(num: Num) -> &'static [Repr] {
        match num {
            // A `Duration` is nanoseconds, and nanoseconds add like
            // integers. Only the boundary cares what the answer is called.
            Num::Int => &[Repr::Int, Repr::Duration],
            Num::Float => &[Repr::Float],
        }
    }

    /// What a comparison's two operands may hold.
    ///
    /// One table for `Inst::Cmp` and `Inst::CmpBranch` both, because the
    /// fused form compares what the comparison compares: two copies would be
    /// a place for the rule to be relaxed on one of them alone.
    fn compared(on: Compare) -> &'static [Repr] {
        match on {
            Compare::Int => &[Repr::Int, Repr::Duration],
            Compare::Float => &[Repr::Float],
            Compare::Bool => &[Repr::Bool],
            Compare::Str => &[Repr::Ref],
            // `is` compares words, and the only words whose identity is a
            // language-level question are references.
            Compare::Identity => &[Repr::Ref],
            Compare::Tag => &[Repr::Tag],
        }
    }

    /// Whether `layout` names an entry of the program's layout table.
    fn layout_exists(&mut self, at: Option<usize>, layout: LayoutId) -> bool {
        self.in_range(at, layout.index(), self.program.layouts.len(), "layout")
    }

    /// Whether the location at `slot` is a value of `layout`: it is inside
    /// the frame, and its words are the layout's words in order.
    ///
    /// This is the check the whole representation turns on. A location is a
    /// base slot and a layout, and the frame's per-slot reprs are what a
    /// collection reads — so a location whose words disagree with what is
    /// being moved into it is a reference the collector will miss or a
    /// scalar it will follow.
    fn fits(&mut self, at: Option<usize>, slot: Slot, layout: LayoutId, what: &str) -> bool {
        let words = self.program.layout(layout).words.clone();
        let name = self.program.layout(layout).name.clone();
        let size = self.function.frame_size();
        if slot as u64 + words.len() as u64 > size as u64 {
            self.fault(
                at,
                format!(
                    "{what} is `{name}`, {} words at slot {slot}, and the frame has {size}",
                    words.len()
                ),
            );
            return false;
        }
        for (offset, want) in words.iter().enumerate() {
            let found = self.function.reprs[slot as usize + offset];
            if found != *want {
                self.fault(
                    at,
                    format!(
                        "{what} is `{name}`, whose word {offset} is {want}, but slot {} holds \
                         {found}",
                        slot as usize + offset
                    ),
                );
                return false;
            }
        }
        true
    }

    /// What slot `slot` holds, reporting a slot outside the frame.
    fn repr(&mut self, at: Option<usize>, slot: Slot) -> Option<Repr> {
        match self.function.repr(slot) {
            Some(repr) => Some(repr),
            None => {
                let size = self.function.frame_size();
                self.fault(at, format!("names slot {slot}, outside a frame of {size}"));
                None
            }
        }
    }

    /// Refuses [`CmpOp::Order`] in a comparison that answers a `Bool`.
    ///
    /// The three-way order's answer is an `Int`, so a branch fused on it or an
    /// immediate form of it has no meaning, and the bytecode has no opcode for
    /// either: `crate::bytecode`'s encoder would refuse what this lets through.
    fn unordered(&mut self, at: Option<usize>, op: CmpOp) {
        if op == CmpOp::Order {
            self.fault(
                at,
                "a three-way order answers an `Int`, and only `cmp` carries one".to_string(),
            );
        }
    }

    fn expect(&mut self, at: Option<usize>, slot: Slot, want: &[Repr]) {
        let Some(found) = self.repr(at, slot) else {
            return;
        };
        if !want.contains(&found) {
            let names: Vec<&str> = want.iter().map(|repr| repr.name()).collect();
            self.fault(
                at,
                format!(
                    "slot {slot} holds {found}, but this wants {}",
                    names.join(" or ")
                ),
            );
        }
    }

    fn target(&mut self, at: Option<usize>, to: u32) {
        if to as usize >= self.function.code.len() {
            let len = self.function.code.len();
            self.fault(at, format!("jumps to {to}, past the {len} instructions"));
        }
    }

    fn in_range(&mut self, at: Option<usize>, index: usize, len: usize, what: &str) -> bool {
        if index >= len {
            self.fault(at, format!("names {what} {index}, and there are {len}"));
            false
        } else {
            true
        }
    }

    /// Whether a field access at word `word` of `obj` stays inside the
    /// object, where what `obj` holds is a static fact.
    ///
    /// The width is the layout being moved, so this is the whole run and not
    /// only its first word: reading a two-word `Point` out of the last word
    /// of an object reads one word of whatever the allocator put after it.
    fn reaches(&mut self, at: Option<usize>, obj: Slot, word: u32, layout: LayoutId, what: &str) {
        let width = self.program.layout(layout).width();
        self.reaches_word(at, obj, word, width, what);
    }

    /// The same, for a run of a width the caller already knows.
    ///
    /// Silent where the object's layout is not static, or where it is but the
    /// header's `len` is what decides how many payload words it has: a
    /// `Shape::Str` or a `Shape::Elements` object is as long as it was
    /// allocated, and only the machine has the header to ask. Those are the
    /// accesses the machine's own bounds check answers.
    fn reaches_word(&mut self, at: Option<usize>, obj: Slot, word: u32, width: u32, what: &str) {
        let Some(Some(id)) = self.objects.get(obj as usize).copied() else {
            return;
        };
        let described = self.program.layout(id);
        let Some(words) = described.fixed_payload_words(&self.program.layouts) else {
            return;
        };
        if word as u64 + width as u64 > words as u64 {
            let name = described.name.clone();
            self.fault(
                at,
                format!("{what} {width} word(s) at word {word} of a `{name}`, which has {words}"),
            );
        }
    }

    /// When `obj` is known to be a [`Shape::Closure`] and `word` is its
    /// callee field, checks that `src` is a known [`Inst::FuncRef`] naming
    /// the same callee the closure's own layout does.
    ///
    /// This is the comparison the module doc calls out: a closure's callee
    /// is carried twice, once in its [`Shape::Closure::function`] and once in
    /// the word [`Inst::FuncRef`] writes into its environment, and until this
    /// nothing checked the two agreed. It is silent whenever either half is
    /// not a static fact — `obj`'s layout from [`Check::objects`], `src`'s
    /// callee from [`Check::funcs`] — for the reason [`Check::slot_facts`]
    /// declines to guess there: a slot written by more than one thing, or by
    /// something this analysis was not taught, answers `None` rather than a
    /// wrong guess.
    fn check_closure_callee(&mut self, at: Option<usize>, obj: Slot, word: u32, src: Slot) {
        let Some(Some(layout_id)) = self.objects.get(obj as usize).copied() else {
            return;
        };
        let described = self.program.layout(layout_id);
        let Shape::Closure { function, .. } = &described.shape else {
            return;
        };
        // Payload word 0 is the callee's `FunctionId`; see `Shape::Closure`.
        if word != 0 {
            return;
        }
        let Some(Some(callee)) = self.funcs.get(src as usize).copied() else {
            return;
        };
        if callee != *function {
            let name = described.name.clone();
            // Symbolic, not `FunctionId`'s bare `Display` — the whole point
            // issue #275 makes of this message, which is otherwise the last
            // place in the crate a diagnostic still named a function by its
            // position in `Program::functions`. The fallback to the raw id
            // mirrors `print::name_of`'s for a `LayoutId`: this runs over a
            // program the verifier has not yet vouched for, so a fault about
            // a callee that is itself out of range should say so rather than
            // panic indexing into the table it is complaining about.
            let named = |id: FunctionId| match self.program.functions.get(id.index()) {
                Some(f) => format!("@{}", f.qualified()),
                None => id.to_string(),
            };
            self.fault(
                at,
                format!(
                    "stores {} into the callee field of a `{name}` closure, whose layout \
                     names {}",
                    named(callee),
                    named(*function)
                ),
            );
        }
    }

    /// Every argument is a value location of the layout it names, and that
    /// location is inside the frame.
    ///
    /// This is what an argument carrying its layout buys the verifier. It
    /// used to check only that the slot existed, because a slot was the whole
    /// of what an argument was — so a call passing the last slot of a frame
    /// as a two-word `Point` was checked by nothing, and the machine read the
    /// frame above it.
    /// Whether an `IntrinsicCall` passes what its intrinsic takes and names the
    /// answer it writes: the argument count, each argument's layout and the
    /// answer's layout, against [`crate::Intrinsic::signature`].
    ///
    /// ADR 0058 gives an intrinsic "a fixed operand and result shape", and
    /// this is where the shape is held to — once, before anything runs — so
    /// that the machine's arms read their operands without re-checking any of
    /// it on every call (#378, P5-3). An argument's *location* is
    /// [`Check::each_arg`]'s; this is about its family.
    ///
    /// It is also where ADR 0058's Phase 5 makes "a new collection
    /// `IntrinsicCall` a verification failure": a `Text` or `Scalar` intrinsic
    /// handed a collection is refused as that, by name, whatever its
    /// signature says — `String.join`'s `Array<String>` is the one collection
    /// a signature names, and it names it exactly.
    fn check_signature(
        &mut self,
        at: Option<usize>,
        called: crate::IntrinsicSite,
        args: crate::ArgsId,
    ) {
        let intrinsic = called.intrinsic;
        let signature = intrinsic.signature();
        if let Some(fault) = self.class_fault(signature.result, called.result) {
            self.fault(at, format!("the answer of `{intrinsic}` is {fault}"));
        }
        let Some(list) = self.program.args.get(args.index()) else {
            // `each_arg` reports a list that is not there.
            return;
        };
        let fixed = signature.operands.len();
        if list.len() != fixed {
            self.fault(
                at,
                format!(
                    "`{intrinsic}` takes {fixed} operand(s), and this call passes {}",
                    list.len()
                ),
            );
            return;
        }
        for (index, arg) in list.clone().into_iter().enumerate() {
            let class = signature.operands[index];
            if arg.layout.index() >= self.program.layouts.len() {
                // `each_arg` reports a layout that is not there.
                continue;
            }
            let described = self.program.layout(arg.layout);
            if intrinsic.category() != Category::Value
                && class != Class::Strings
                && class != Class::Buffer
                && is_collection(&described.shape)
            {
                let name = described.name.clone();
                self.fault(
                    at,
                    format!(
                        "operand {index} of `{intrinsic}` is the collection `{name}`, and a \
                         {:?} intrinsic takes none: a collection operation is a run instruction \
                         or the standard library's, not an intrinsic (ADR 0058)",
                        intrinsic.category()
                    ),
                );
            } else if let Some(fault) = self.class_fault(class, arg.layout) {
                self.fault(at, format!("operand {index} of `{intrinsic}` is {fault}"));
            }
        }
    }

    /// Why a value of `layout` is not a `class`, or `None` when it is one.
    fn class_fault(&self, class: Class, layout: LayoutId) -> Option<String> {
        let described = self.program.layout(layout);
        let word = |repr: Repr| described.shape == Shape::Word(repr);
        let fits = match class {
            Class::Value => true,
            Class::Buffer => described.shape == Shape::ByteBuffer,
            Class::Unit => word(Repr::Unit),
            Class::Bool => word(Repr::Bool),
            Class::Int => word(Repr::Int),
            Class::Float => word(Repr::Float),
            Class::Str => described.shape == Shape::Str,
            Class::Strings => self.is_strings(layout),
            Class::OptionOf(carried) => self.is_case_pair(
                layout,
                (cove_schema::builtins::SOME_CASE.name, Some(carried)),
                (cove_schema::builtins::NONE_CASE.name, None),
            ),
            Class::ResultOf(carried) => self.is_case_pair(
                layout,
                (cove_schema::builtins::OK_CASE.name, Some(carried)),
                (cove_schema::builtins::ERR_CASE.name, None),
            ),
        };
        (!fits).then(|| format!("`{}`, where its signature has {class}", described.name))
    }

    /// Whether `layout` is an `Array<String>`.
    fn is_strings(&self, layout: LayoutId) -> bool {
        match self.program.layout(layout).shape {
            Shape::Elements {
                elem,
                growable: false,
            } => {
                elem.index() < self.program.layouts.len()
                    && self.program.layout(elem).shape == Shape::Str
            }
            _ => false,
        }
    }

    /// Whether `layout` is an enum with a case `carrier` holding exactly one
    /// value of the carried class, and a case `other`.
    ///
    /// `other` is named and not described: `None` carries nothing and `Err`
    /// carries the machine's `Error`, and neither is a question a signature
    /// asks.
    fn is_case_pair(
        &self,
        layout: LayoutId,
        (carrier, carried): (&str, Option<Carried>),
        (other, _): (&str, Option<Carried>),
    ) -> bool {
        let Shape::Enum { cases, .. } = &self.program.layout(layout).shape else {
            return false;
        };
        let carries = |part: LayoutId| {
            part.index() < self.program.layouts.len()
                && matches!(
                    (carried, &self.program.layout(part).shape),
                    (Some(Carried::Int), Shape::Word(Repr::Int))
                        | (Some(Carried::Float), Shape::Word(Repr::Float))
                        | (Some(Carried::Str), Shape::Str)
                )
        };
        cases.iter().any(|case| {
            &*case.name == carrier && case.parts.len() == 1 && carries(case.parts[0].layout)
        }) && cases.iter().any(|case| &*case.name == other)
    }

    fn each_arg(&mut self, at: Option<usize>, args: crate::ArgsId) {
        if !self.in_range(at, args.index(), self.program.args.len(), "argument list") {
            return;
        }
        for (index, arg) in self.program.arg_list(args).to_vec().into_iter().enumerate() {
            if self.layout_exists(at, arg.layout) {
                self.fits(at, arg.slot, arg.layout, &format!("argument {index}"));
            }
        }
    }

    /// The same, where the callee declares what it takes: each argument's
    /// layout is the parameter's, and its location is a value of it.
    ///
    /// The layouts are compared rather than only the locations' words,
    /// because two layouts can have the same words and not be the same
    /// family — an `Error` and a `String` are both one `Repr::Ref` — and it
    /// is the argument's layout that the machine hands a builtin and a host.
    /// The copy into the callee's frame is made at the *parameter's* width:
    /// the frame being written is the callee's, and only `Function::params`
    /// is a fact about the callee. This is what makes the two agree.
    fn check_args(
        &mut self,
        at: Option<usize>,
        args: crate::ArgsId,
        want: &[LayoutId],
        name: &str,
    ) {
        if !self.in_range(at, args.index(), self.program.args.len(), "argument list") {
            return;
        }
        let passed = self.program.arg_list(args).to_vec();
        if passed.len() != want.len() {
            self.fault(
                at,
                format!(
                    "passes {} arguments to `{name}`, which declares {}",
                    passed.len(),
                    want.len()
                ),
            );
            return;
        }
        for (index, (arg, layout)) in passed.into_iter().zip(want).enumerate() {
            if !self.layout_exists(at, *layout) {
                continue;
            }
            if arg.layout != *layout {
                let passed = self.name_of(arg.layout);
                let declared = self.program.layout(*layout).name.clone();
                self.fault(
                    at,
                    format!(
                        "argument {index} of `{name}` is passed as a `{passed}`, and the \
                         parameter is a `{declared}`"
                    ),
                );
                continue;
            }
            self.fits(
                at,
                arg.slot,
                *layout,
                &format!("argument {index} of `{name}`"),
            );
        }
    }

    /// Refuses a run instruction over a storage ADR 0058's Phase 2 does not
    /// admit it for. Two are left: [`Inst::RunLoad`] and [`Inst::RunStore`],
    /// the unit load and the unit write, which have only their
    /// [`crate::Storage::PackedBytes`] member — a word run is read and written
    /// through [`Inst::LoadElem`] and [`Inst::StoreElem`], which already take a
    /// layout, so the word members of these two have had no producer to arrive
    /// with.
    ///
    /// Everything else in the family admits both storages now: the copy and the
    /// slice from the start, the finish with `Vector.freeze()`, ADR 0062's
    /// ensure and commit with the window, and the allocation with
    /// `core.vectorWithCapacity`. Until a member has its word producer, a
    /// [`crate::Storage::Words`] here is a lowering mistake the machine has no
    /// opcode for, not a unit it could load.
    fn admit_storage(&mut self, at: Option<usize>, what: &str, storage: crate::Storage) {
        if let crate::Storage::Words(layout) = storage {
            let name = self.name_of(layout);
            self.fault(
                at,
                format!(
                    "{what} a run of `{name}` words, and this instruction admits only packed bytes"
                ),
            );
        }
    }

    /// A word [`Inst::RunFinish`]: `Vector.freeze()`'s, which relabels a store
    /// of `elem` elements into the `Array` of them it already is — or a keyed
    /// finish, which relabels it into the `Set` or `Map` whose unit `elem` is.
    ///
    /// There is nothing to validate in a run of whole elements, so the
    /// validation is [`crate::Validation::None`]; and the target is the
    /// non-growable [`crate::Shape::Elements`] of the same element, a
    /// [`crate::Shape::Members`] of it, or a [`crate::Shape::Entries`] whose
    /// entry it is word for word ([`crate::layout::is_entry_of`]), because the
    /// relabelled store is traced by the target's reference map from then on
    /// and a finish into another family would have the collector follow the
    /// wrong words. That a keyed run is ascending and distinct is not a static
    /// fact: the standard-library body that built it established it, and the
    /// oracle asserts it under `debug_assertions` (#378, Q4.10).
    fn check_word_finish(
        &mut self,
        at: Option<usize>,
        elem: LayoutId,
        target: LayoutId,
        validation: crate::Validation,
    ) {
        if !self.layout_exists(at, elem) || !self.layout_exists(at, target) {
            return;
        }
        let name = self.name_of(elem);
        if validation != crate::Validation::None {
            self.fault(
                at,
                format!(
                    "finishes a run of `{name}` words with validation `{validation:?}`, and a \
                     word run has nothing to validate"
                ),
            );
        }
        let shape = &self.program.layout(target).shape;
        let fits = matches!(
            shape,
            Shape::Elements { elem: held, growable: false } if *held == elem
        ) || crate::layout::finishes_as_keyed_run_of(&self.program.layouts, shape, elem);
        if !fits {
            let named = self.name_of(target);
            self.fault(
                at,
                format!(
                    "finishes a run of `{name}` words into `{named}`, and a word run finishes \
                     into the fixed `Elements` of the same element, or the `Members` or \
                     `Entries` whose unit it is"
                ),
            );
        }
    }

    /// [`Inst::RunCopy`]'s five arguments — `dst`, `dst_at`, `src`, `src_at`,
    /// `count`, in that order — and its storage.
    ///
    /// Checked by `Repr` rather than by [`Self::check_args`]'s declared
    /// [`LayoutId`], because `dst` and `src` do not have one: a byte copy's
    /// `src` may be a `String` or another [`crate::Shape::Bytes`] run, a word
    /// copy's either end may be an `Array`'s elements or a `Vector`'s store,
    /// and which is a run-time fact rather than something a lowering could
    /// declare the way a call declares its parameters. What is static is that
    /// both are references, the other three are integers, and a
    /// [`crate::Storage::Words`] layout is one the table has — so that is what
    /// this asks.
    fn check_run_copy(&mut self, at: Option<usize>, args: crate::ArgsId, storage: crate::Storage) {
        if let crate::Storage::Words(layout) = storage {
            self.layout_exists(at, layout);
        }
        if !self.in_range(at, args.index(), self.program.args.len(), "argument list") {
            return;
        }
        const NAMES: [&str; 5] = ["dst", "dst_at", "src", "src_at", "count"];
        const WANTS: [Repr; 5] = [Repr::Ref, Repr::Int, Repr::Ref, Repr::Int, Repr::Int];
        let passed = self.program.arg_list(args).to_vec();
        if passed.len() != NAMES.len() {
            self.fault(
                at,
                format!(
                    "copies a run with {} argument(s), and this needs {} ({})",
                    passed.len(),
                    NAMES.len(),
                    NAMES.join(", ")
                ),
            );
            return;
        }
        for (arg, want) in passed.iter().zip(WANTS) {
            self.expect(at, arg.slot, &[want]);
        }
    }

    /// [`Inst::RunSlice`]'s four arguments — `dst`, `src`, `from`, `count`, in
    /// that order — and its storage.
    ///
    /// Checked by `Repr` for [`Self::check_run_copy`]'s reason, with one
    /// declared layout that is not a run-time fact: `dst`'s, which is what the
    /// answer is allocated as. For [`crate::Storage::Words`] it must be the
    /// fixed [`crate::Shape::Elements`] of the storage's element, because the
    /// fresh run is traced by that layout's reference map. For
    /// [`crate::Storage::PackedBytes`] it must be [`Program::str_layout`]: a byte
    /// slice answers a `String` and nothing else, because the one thing that
    /// makes its answer one — both ends at a character boundary of a valid
    /// `String` — is decided by `std.string.sliceBytes` before it asks, and a
    /// byte slice into a run under construction has no producer to admit.
    fn check_run_slice(&mut self, at: Option<usize>, args: crate::ArgsId, storage: crate::Storage) {
        if let crate::Storage::Words(elem) = storage {
            if !self.layout_exists(at, elem) {
                return;
            }
        }
        if !self.in_range(at, args.index(), self.program.args.len(), "argument list") {
            return;
        }
        const NAMES: [&str; 4] = ["dst", "src", "from", "count"];
        const WANTS: [Repr; 4] = [Repr::Ref, Repr::Ref, Repr::Int, Repr::Int];
        let passed = self.program.arg_list(args).to_vec();
        if passed.len() != NAMES.len() {
            self.fault(
                at,
                format!(
                    "slices a run with {} argument(s), and this needs {} ({})",
                    passed.len(),
                    NAMES.len(),
                    NAMES.join(", ")
                ),
            );
            return;
        }
        for (arg, want) in passed.iter().zip(WANTS) {
            self.expect(at, arg.slot, &[want]);
        }
        let target = passed[0].layout;
        if !self.layout_exists(at, target) {
            return;
        }
        match storage {
            crate::Storage::Words(elem) => {
                let fits = matches!(
                    self.program.layout(target).shape,
                    Shape::Elements { elem: held, growable: false } if held == elem
                );
                if !fits {
                    let name = self.name_of(elem);
                    let named = self.name_of(target);
                    self.fault(
                        at,
                        format!(
                            "slices a run of `{name}` words into `{named}`, and a word slice \
                             answers the fixed `Elements` of the same element"
                        ),
                    );
                }
            }
            crate::Storage::PackedBytes => {
                if target != self.program.str_layout {
                    let named = self.name_of(target);
                    let string = self.name_of(self.program.str_layout);
                    self.fault(
                        at,
                        format!(
                            "slices a run of packed bytes into `{named}`, and a byte slice \
                             answers `{string}`"
                        ),
                    );
                }
            }
        }
    }

    /// [`Inst::RunFind`]'s four arguments — `dst`, `haystack`, `needle`,
    /// `from`, in that order — and its storage.
    ///
    /// Checked by `Repr` for [`Self::check_run_copy`]'s reason: both runs'
    /// shapes are run-time facts, and what is static is that both are
    /// references, that `from` is an integer, and that `dst` — which this
    /// instruction *writes* — is an integer too, because the answer is a unit
    /// offset or -1 and not a run.
    ///
    /// **[`crate::Storage::Words`] is refused**, which is
    /// [ADR 0065](../../../docs/adr/0065-a-run-search-is-the-one-loop-that-stays-below.md)'s
    /// Decision 1 and the one rule this check has that the slice's does not.
    /// A sequence search is `Array.contains` and `Array.indexOf`, Cove loops
    /// over `==` since ADR 0058, and a search over a word run that wanted this
    /// instruction would get its own decision and its own measurement. The
    /// refusal is here rather than left to the encoder so that a lowering that
    /// asked for it is told what it did rather than told the instruction is
    /// too wide.
    fn check_run_find(&mut self, at: Option<usize>, args: crate::ArgsId, storage: crate::Storage) {
        if let crate::Storage::Words(elem) = storage {
            let name = self.name_of(elem);
            self.fault(
                at,
                format!(
                    "searches a run of `{name}` words, and a run search is over packed bytes \
                     alone"
                ),
            );
            return;
        }
        if !self.in_range(at, args.index(), self.program.args.len(), "argument list") {
            return;
        }
        const NAMES: [&str; 4] = ["dst", "haystack", "needle", "from"];
        const WANTS: [Repr; 4] = [Repr::Int, Repr::Ref, Repr::Ref, Repr::Int];
        let passed = self.program.arg_list(args).to_vec();
        if passed.len() != NAMES.len() {
            self.fault(
                at,
                format!(
                    "searches a run with {} argument(s), and this needs {} ({})",
                    passed.len(),
                    NAMES.len(),
                    NAMES.join(", ")
                ),
            );
            return;
        }
        for (arg, want) in passed.iter().zip(WANTS) {
            self.expect(at, arg.slot, &[want]);
        }
    }

    /// [ADR 0062]'s reservation rule: every [`Inst::GrowableEnsure`] opens a
    /// window, and a window that is written into is committed, block-locally,
    /// by the one write the room was made for.
    ///
    /// # Why a rule and not a runtime check
    ///
    /// A commit publishes units as value, and the machine cannot tell a
    /// written unit from a zeroed one: for a vector a zero is a null reference
    /// or a `0`, and for a byte buffer it is a NUL. So what the runtime checks
    /// of a commit is only that it stays inside the capacity, and what makes
    /// the published units the ones the program wrote is this. It is the
    /// provisional rule the composite `growable-extend`'s documentation wrote
    /// down for the day the instruction split, made precise:
    ///
    /// 1. **Facts.** `load-field a <- o +0` says `a` holds `o`'s length, an
    ///    `int n` that `n` holds a constant, and — only after an ensure on `o`
    ///    — `load-field s <- o +1` says `s` holds `o`'s store. A fact dies when
    ///    a slot it names is written, when anything outside the instructions
    ///    below runs, and at the end of the block.
    /// 2. **An ensure opens a reservation** on its owner for its count. A second
    ///    ensure while one is open is a fault.
    /// 3. **Inside the window** only instructions that neither call, allocate,
    ///    branch nor write the heap may run — constants, copies, clears,
    ///    arithmetic, comparisons, conversions and reads — and none of them may
    ///    write the owner's slot or the count's. After the write, only the ones
    ///    that cannot fail: a refusal between the write and the commit would
    ///    leave a written unit above the length, which a byte buffer's finish
    ///    relies on never happening.
    /// 4. **The write** is exactly one of: a `store-elem` of the storage's
    ///    element, or a byte `run-store`, into the store at the length, with a
    ///    count known to be `1`; or a `run-copy` of the storage into the store at
    ///    the length whose count is the reservation's.
    /// 5. **The commit** is on the reservation's owner, after the write, with
    ///    the reservation's count, and closes the window.
    /// 6. **A written window is committed before its block ends.** An unwritten
    ///    one may lapse there. A branch, a terminator and a branch target all end
    ///    a block, so a branch inside a written window is this fault rather than
    ///    clause 3's, and nothing can reach a commit along a path that did not
    ///    write.
    ///
    /// [ADR 0062]: ../../../docs/adr/0062-an-append-is-ensure-store-commit.md
    fn check_reservations(&mut self) {
        let function = self.function;
        let leaders = crate::flow::leaders(self.program, function);
        let mut facts = Facts::default();
        let mut open: Option<Reservation> = None;
        // Owners whose reservation was abandoned by a fault in this block, so
        // that the commit which follows is not a second report of one mistake.
        let mut abandoned: Vec<Slot> = Vec::new();
        for (pc, inst) in function.code.iter().enumerate() {
            if leaders[pc] {
                self.lapse(open.take());
                facts = Facts::default();
                abandoned.clear();
            }
            let at = Some(pc);
            let Some(window) = open.as_mut() else {
                match *inst {
                    Inst::GrowableEnsure {
                        owner,
                        additional,
                        storage,
                    } => {
                        open = Some(Reservation {
                            ensure: pc,
                            owner,
                            count: additional,
                            constant: facts.constant(additional),
                            storage,
                            stores: Vec::new(),
                            written: None,
                        });
                    }
                    Inst::GrowableCommit { owner, .. } => {
                        if !abandoned.contains(&owner) {
                            self.fault(
                                at,
                                format!(
                                    "commits onto slot {owner} with no reservation open on it \
                                     in this block"
                                ),
                            );
                        }
                        facts.lengths.clear();
                    }
                    _ if admitted_in_a_window(inst, false) => {
                        self.learn(inst, &mut facts, None);
                    }
                    _ => facts = Facts::default(),
                }
                continue;
            };
            let opened = window.ensure;
            match *inst {
                Inst::GrowableCommit {
                    owner,
                    count,
                    storage,
                } => {
                    let window = open.take().expect("a reservation is open");
                    if owner != window.owner || storage != window.storage {
                        self.fault(
                            at,
                            format!(
                                "commits onto slot {owner}, and the reservation open since \
                                 +{opened} is on slot {}",
                                window.owner
                            ),
                        );
                    } else if window.written.is_none() {
                        self.fault(
                            at,
                            format!(
                                "commits the reservation opened at +{opened}, and nothing was \
                                 written into it"
                            ),
                        );
                    } else if !window.counts(count, &facts) {
                        self.fault(
                            at,
                            format!(
                                "commits slot {count}, which is not known to hold the count \
                                 slot {} held when the reservation at +{opened} was made",
                                window.count
                            ),
                        );
                    }
                    // The length changed, whichever owner a length fact is about:
                    // two slots may name one owner.
                    facts.lengths.clear();
                }
                Inst::GrowableEnsure { .. } => {
                    self.fault(
                        at,
                        format!(
                            "opens a second reservation while the one opened at +{opened} is \
                             still open"
                        ),
                    );
                    abandoned.push(window.owner);
                    open = None;
                    facts = Facts::default();
                }
                Inst::StoreElem {
                    obj, index, layout, ..
                } => {
                    let fits = window.storage == crate::Storage::Words(layout);
                    let one = window.constant == Some(1);
                    self.window_write(pc, window, &facts, obj, index, fits, one);
                }
                Inst::RunStore {
                    run,
                    index,
                    storage,
                    ..
                } => {
                    let fits = window.storage == storage;
                    let one = window.constant == Some(1);
                    self.window_write(pc, window, &facts, run, index, fits, one);
                }
                Inst::RunCopy { args, storage } => {
                    match self.program.args.get(args.index()).map(Vec::as_slice) {
                        Some([dst, dst_at, _, _, count]) => {
                            let fits = window.storage == storage;
                            let all = window.counts(count.slot, &facts);
                            self.window_write(pc, window, &facts, dst.slot, dst_at.slot, fits, all);
                        }
                        // A row of the wrong shape is `check_run_copy`'s fault
                        // already; it is not also a write.
                        _ => {
                            abandoned.push(window.owner);
                            open = None;
                            facts = Facts::default();
                        }
                    }
                }
                _ if admitted_in_a_window(inst, window.written.is_some()) => {
                    let (owner, count) = (window.owner, window.count);
                    let mut held = None;
                    inst.writes(self.program, &mut |base, width| {
                        for slot in [owner, count] {
                            if (base..base.saturating_add(width)).contains(&slot) {
                                held.get_or_insert(slot);
                            }
                        }
                    });
                    match held {
                        Some(slot) => {
                            let role = if slot == owner { "owner" } else { "count" };
                            self.fault(
                                at,
                                format!(
                                    "writes slot {slot}, which holds the {role} of the \
                                     reservation opened at +{opened}"
                                ),
                            );
                            abandoned.push(owner);
                            open = None;
                            facts = Facts::default();
                        }
                        None => self.learn(inst, &mut facts, open.as_mut()),
                    }
                }
                // A branch or a terminator ends the block, and clause 6 is what
                // decides it: an unwritten reservation lapses there, and a
                // written one is a fault.
                _ if inst.ends_a_block() => {
                    self.lapse(open.take());
                    facts = Facts::default();
                }
                _ => {
                    // The variant, from `Debug` rather than `crate::print`: a
                    // listing reads the ids an instruction names, and this runs
                    // over code whose ids another check may just have refused.
                    let debug = format!("{inst:?}");
                    let name = debug.split([' ', '{']).next().unwrap_or("?").to_string();
                    let which = match window.written {
                        None => "neither calls, allocates, branches nor writes the heap",
                        Some(_) => "cannot fail, now that the reservation is written",
                    };
                    self.fault(
                        at,
                        format!(
                            "`{name}` inside the reservation opened at +{opened}, which admits \
                             only what {which}"
                        ),
                    );
                    abandoned.push(window.owner);
                    open = None;
                    facts = Facts::default();
                }
            }
        }
        self.lapse(open);
    }

    /// A reservation that reached the end of its block: a fault if it was
    /// written, and nothing if it was not.
    fn lapse(&mut self, open: Option<Reservation>) {
        if let Some(Reservation {
            ensure,
            owner,
            written: Some(written),
            ..
        }) = open
        {
            self.fault(
                Some(ensure),
                format!(
                    "opens a reservation on slot {owner} that is written at +{written} and not \
                     committed before its block ends"
                ),
            );
        }
    }

    /// The one write a reservation admits, into `store` at `index`: clause 4
    /// of [`Self::check_reservations`]. `fits` is whether its storage is the
    /// reservation's and `counts` whether its count is.
    #[allow(clippy::too_many_arguments)]
    fn window_write(
        &mut self,
        pc: usize,
        window: &mut Reservation,
        facts: &Facts,
        store: Slot,
        index: Slot,
        fits: bool,
        counts: bool,
    ) {
        let at = Some(pc);
        let (opened, owner) = (window.ensure, window.owner);
        if let Some(earlier) = window.written {
            self.fault(
                at,
                format!(
                    "writes into the reservation opened at +{opened} a second time, and it was \
                     written at +{earlier}"
                ),
            );
        } else if !fits {
            self.fault(
                at,
                format!(
                    "writes a unit of another storage into the reservation opened at +{opened}"
                ),
            );
        } else if !window.stores.contains(&store) {
            self.fault(
                at,
                format!(
                    "writes into slot {store}, which is not known to hold the store of slot \
                     {owner} read after the ensure at +{opened}"
                ),
            );
        } else if !facts.lengths.contains(&(index, owner)) {
            self.fault(
                at,
                format!(
                    "writes at slot {index}, which is not known to hold the length of slot \
                     {owner}"
                ),
            );
        } else if !counts {
            self.fault(
                at,
                format!(
                    "writes a count that is not known to be the one the reservation at \
                     +{opened} was made for"
                ),
            );
        }
        window.written = Some(pc);
    }

    /// Clause 1's facts, after `inst`: those naming a slot it writes forgotten,
    /// and the one it establishes, if any, learnt.
    fn learn(&self, inst: &Inst, facts: &mut Facts, window: Option<&mut Reservation>) {
        let mut written: Vec<(Slot, u32)> = Vec::new();
        inst.writes(self.program, &mut |base, width| written.push((base, width)));
        let hit = |slot: Slot| {
            written
                .iter()
                .any(|&(base, width)| (base..base.saturating_add(width)).contains(&slot))
        };
        facts
            .lengths
            .retain(|&(len, owner)| !hit(len) && !hit(owner));
        facts.constants.retain(|&(slot, _)| !hit(slot));
        let mut window = window;
        if let Some(window) = window.as_deref_mut() {
            window.stores.retain(|&store| !hit(store));
        }
        match *inst {
            Inst::Int { dst, value } => facts.constants.push((dst, value)),
            Inst::LoadField {
                dst,
                obj,
                at,
                layout,
            } if dst != obj
                && self
                    .program
                    .layouts
                    .get(layout.index())
                    .is_some_and(|held| held.width() == 1) =>
            {
                if at == GROWABLE_LEN {
                    facts.lengths.push((dst, obj));
                } else if at == GROWABLE_STORE {
                    if let Some(window) = window.filter(|window| window.owner == obj) {
                        window.stores.push(dst);
                    }
                }
            }
            _ => {}
        }
    }

    /// What a layout is called, or its id where the table is too short.
    fn name_of(&self, layout: LayoutId) -> String {
        match self.program.layouts.get(layout.index()) {
            Some(held) => held.name.to_string(),
            None => layout.to_string(),
        }
    }
}

/// Payload word 0 of a growable owner, its logical length: the lowering's
/// `VECTOR_LEN` and `BUFFER_LEN`, and the runtime's `GROWABLE_LEN`.
const GROWABLE_LEN: u32 = 0;

/// Payload word 1 of a growable owner, its store: the lowering's
/// `VECTOR_STORE`, and the runtime's `GROWABLE_STORE`.
const GROWABLE_STORE: u32 = 1;

/// What [`Check::check_reservations`] knows about the slots of the block it is
/// in, at one program counter.
#[derive(Default)]
struct Facts {
    /// `(a, o)`: slot `a` holds the length of the owner in slot `o`.
    lengths: Vec<(Slot, Slot)>,
    /// `(n, k)`: slot `n` holds the constant `k`.
    constants: Vec<(Slot, i64)>,
}

impl Facts {
    fn constant(&self, slot: Slot) -> Option<i64> {
        self.constants
            .iter()
            .rev()
            .find(|(held, _)| *held == slot)
            .map(|(_, value)| *value)
    }
}

/// An open [`Inst::GrowableEnsure`].
struct Reservation {
    /// Where it was opened.
    ensure: usize,
    owner: Slot,
    /// The slot the room was asked for by, which nothing inside the window may
    /// write.
    count: Slot,
    /// That slot's constant when the ensure ran, if it had one.
    constant: Option<i64>,
    storage: crate::Storage,
    /// Slots known to hold the owner's store, each read after the ensure.
    stores: Vec<Slot>,
    /// Where the one write was, once it has been.
    written: Option<usize>,
}

impl Reservation {
    /// Whether `slot` is known to hold this reservation's count: the slot
    /// itself, which the window may not write, or a slot holding the same
    /// constant.
    fn counts(&self, slot: Slot, facts: &Facts) -> bool {
        slot == self.count || (self.constant.is_some() && facts.constant(slot) == self.constant)
    }
}

/// Whether `inst` may run inside a reservation — before its write when
/// `written` is false, and after it when it is true. See
/// [`Check::check_reservations`], clause 3.
fn admitted_in_a_window(inst: &Inst, written: bool) -> bool {
    let cannot_fail = matches!(
        inst,
        Inst::Unit { .. }
            | Inst::Bool { .. }
            | Inst::Int { .. }
            | Inst::Tag { .. }
            | Inst::Float { .. }
            | Inst::Str { .. }
            | Inst::Copy { .. }
            | Inst::Clear { .. }
    );
    cannot_fail
        || (!written
            && matches!(
                inst,
                Inst::Arith { .. }
                    | Inst::ArithImm { .. }
                    | Inst::Cmp { .. }
                    | Inst::CmpImm { .. }
                    | Inst::Neg { .. }
                    | Inst::Not { .. }
                    | Inst::Convert { .. }
                    | Inst::LoadField { .. }
                    | Inst::LoadElem { .. }
                    | Inst::RunLoad { .. }
                    | Inst::Len { .. }
                    | Inst::LayoutOf { .. }
                    | Inst::Load { .. }
            ))
}

/// The id of the function being checked is carried so that a future fault
/// can name it by id as well as by name; nothing reads it yet.
impl Check<'_> {
    #[allow(dead_code)]
    fn id(&self) -> FunctionId {
        self.id
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cove_diag::{FileId, Span};

    use super::*;
    use crate::inst::{ArithOp, CmpOp, Compare, Inst, Num};
    use crate::layout::{Case, Field, Layout, Shape};
    use crate::program::{Arg, Function, HostOp, Local, Table, TableId};
    use crate::{ArgsId, HostOpId};

    const INT: LayoutId = LayoutId(0);
    const STR: LayoutId = LayoutId(1);
    const POINT: LayoutId = LayoutId(2);
    /// `[disc: Int, Ref]`, the shape an `Option<String>` has.
    const ANSWER: LayoutId = LayoutId(3);
    /// A second two-`Int` struct: the same words as [`POINT`] and a different
    /// family, which is what an argument's layout is checked against.
    const PAIR: LayoutId = LayoutId(4);
    const BOXED: LayoutId = LayoutId(5);
    /// A closure over nothing, whose layout says its callee is `FunctionId(1)`
    /// — the second function [`program`] is given, in the tests that need
    /// one. See [`Check::check_closure_callee`].
    const CLOSURE: LayoutId = LayoutId(6);
    /// A two-case enum, for the checks a case index needs an enum to make.
    const ENUM: LayoutId = LayoutId(7);
    /// `Array<Int>`: what a word finish of `Int` elements relabels its store to.
    const ARRAY_INT: LayoutId = LayoutId(8);
    /// `Set<Int>`: what a keyed finish of `Int` elements relabels its store to.
    const SET_INT: LayoutId = LayoutId(9);
    /// `MapEntry<Int, Int>`: a key at word 0 and a value at word 1.
    const ENTRY_INT: LayoutId = LayoutId(10);
    /// `Map<Int, Int>`, whose entry [`ENTRY_INT`] is.
    const MAP_INT: LayoutId = LayoutId(11);

    fn layouts() -> Vec<Layout> {
        vec![
            Layout::word("Int", Repr::Int),
            Layout::object("String", Shape::Str),
            Layout::inline(
                "Point",
                Shape::Struct {
                    fields: Vec::new(),
                    opaque: false,
                },
                vec![Repr::Int, Repr::Int],
            ),
            Layout::inline(
                "Option",
                Shape::Enum {
                    cases: Vec::new(),
                    payload: vec![Repr::Ref],
                },
                vec![Repr::Int, Repr::Ref],
            ),
            Layout::inline(
                "Pair",
                Shape::Struct {
                    fields: Vec::new(),
                    opaque: false,
                },
                vec![Repr::Int, Repr::Int],
            ),
            Layout::object("Any", Shape::Boxed),
            Layout::object(
                "closure g",
                Shape::Closure {
                    function: FunctionId(1),
                    captures: Vec::new(),
                },
            ),
            Layout::inline(
                "m.E",
                Shape::Enum {
                    cases: vec![
                        Case {
                            name: Arc::from("A"),
                            parts: Vec::new(),
                        },
                        Case {
                            name: Arc::from("B"),
                            parts: Vec::new(),
                        },
                    ],
                    payload: vec![Repr::Int],
                },
                vec![Repr::Tag, Repr::Int],
            ),
            Layout::object(
                "Array<Int>",
                Shape::Elements {
                    elem: INT,
                    growable: false,
                },
            ),
            Layout::object("Set<Int>", Shape::Members { elem: INT }),
            Layout::inline(
                "MapEntry",
                Shape::Struct {
                    fields: vec![
                        Field {
                            name: Arc::from("key"),
                            layout: INT,
                            at: 0,
                        },
                        Field {
                            name: Arc::from("value"),
                            layout: INT,
                            at: 1,
                        },
                    ],
                    opaque: false,
                },
                vec![Repr::Int, Repr::Int],
            ),
            Layout::object(
                "Map<Int, Int>",
                Shape::Entries {
                    key: INT,
                    value: INT,
                },
            ),
        ]
    }

    fn span() -> Span {
        Span::new(FileId(0), 0, 0)
    }

    fn function(reprs: Vec<Repr>, returns: LayoutId, code: Vec<Inst>) -> Function {
        Function {
            module: Arc::from("m"),
            name: Arc::from("f"),
            params: Vec::new(),
            spans: vec![span(); code.len()],
            refs: RefMap::of(&reprs),
            reprs,
            returns,
            captures: Vec::new(),
            code,
            locals: Vec::new(),
            inlined: Vec::new(),
            span: span(),
            is_async: false,
            stub: false,
        }
    }

    fn program(functions: Vec<Function>) -> Program {
        Program {
            functions,
            layouts: layouts(),
            str_layout: STR,
            boxed_layout: BOXED,
            ..Program::default()
        }
    }

    fn faults(program: &Program) -> Vec<String> {
        match verify(program) {
            Ok(()) => Vec::new(),
            Err(items) => items.into_iter().map(|item| item.what).collect(),
        }
    }

    /// A resource operation is addressed to a `Repr::Host` word, and the
    /// operation it names has to be one a resource answers.
    ///
    /// Neither is a fact about the *handle*: which resource a word names is
    /// the host's business and nothing static can say it. What is static is
    /// that the receiver holds a name at all and that the call site settled a
    /// resource kind, and both are lowering bugs rather than program faults.
    #[test]
    fn a_resource_call_is_addressed_to_a_host_word_and_names_a_resource() {
        let mut held = program(vec![function(
            vec![Repr::Int, Repr::Int],
            INT,
            vec![
                Inst::CallResource {
                    dst: 0,
                    receiver: 1,
                    op: HostOpId(0),
                    args: ArgsId(0),
                },
                Inst::Return { src: 0 },
            ],
        )]);
        held.args.push(Vec::new());
        held.host_ops.push(HostOp {
            module: Arc::from("files"),
            operation: Arc::from("write"),
            resource: None,
            result: INT,
        });
        assert_eq!(
            faults(&held),
            vec![
                "slot 1 holds int, but this wants host".to_string(),
                "is addressed to a resource, but `files.write` names no resource kind".to_string(),
            ]
        );
    }

    /// The same call, well formed.
    #[test]
    fn a_resource_call_that_names_a_kind_and_a_handle_is_well_formed() {
        let mut held = program(vec![function(
            vec![Repr::Int, Repr::Host],
            INT,
            vec![
                Inst::CallResource {
                    dst: 0,
                    receiver: 1,
                    op: HostOpId(0),
                    args: ArgsId(0),
                },
                Inst::Return { src: 0 },
            ],
        )]);
        held.args.push(Vec::new());
        held.host_ops.push(HostOp {
            module: Arc::from("files"),
            operation: Arc::from("write"),
            resource: Some(Arc::from("Writer")),
            result: INT,
        });
        assert_eq!(faults(&held), Vec::<String>::new());
        assert_eq!(held.host_op(HostOpId(0)).qualified(), "files.Writer.write");
    }

    #[test]
    fn a_well_formed_function_has_nothing_to_say_about_it() {
        let f = function(
            vec![Repr::Int, Repr::Int],
            POINT,
            vec![Inst::Return { src: 0 }],
        );
        assert_eq!(faults(&program(vec![f])), Vec::<String>::new());
    }

    #[test]
    fn a_copy_whose_destination_is_not_the_layout_s_words_is_a_fault() {
        // The whole representation turns on this: a location is a base slot
        // and a layout, and a copy of the wrong width is a reference the
        // collector will miss or a scalar it will follow.
        let f = function(
            vec![Repr::Int, Repr::Ref, Repr::Int, Repr::Int, Repr::Unit],
            INT,
            vec![
                Inst::Copy {
                    dst: 0,
                    src: 2,
                    layout: POINT,
                },
                Inst::Return { src: 4 },
            ],
        );
        assert_eq!(
            faults(&program(vec![f])),
            vec![
                "the destination of a copy is `Point`, whose word 1 is int, but slot 1 holds ref"
                    .to_string(),
                "what is returned is `Int`, whose word 0 is int, but slot 4 holds unit".to_string(),
            ]
        );
    }

    #[test]
    fn a_location_that_runs_off_the_end_of_the_frame_is_a_fault() {
        let f = function(
            vec![Repr::Int, Repr::Int],
            INT,
            vec![
                Inst::Copy {
                    dst: 1,
                    src: 0,
                    layout: POINT,
                },
                Inst::Return { src: 0 },
            ],
        );
        assert_eq!(
            faults(&program(vec![f])),
            vec!["the destination of a copy is `Point`, 2 words at slot 1, and the frame has 2"]
        );
    }

    #[test]
    fn a_reference_map_that_disagrees_with_the_reprs_is_a_fault() {
        // A collection walks frames using the map, so a lowering that wrote
        // a reference into a slot the map calls an `Int` would produce a
        // dangling reference at the next collection.
        let mut f = function(vec![Repr::Ref], STR, vec![Inst::Return { src: 0 }]);
        f.refs = RefMap::of(&[Repr::Int]);
        assert_eq!(
            faults(&program(vec![f])),
            vec![
                "reference map disagrees with the frame's reprs, so a collection would scan the \
                 wrong slots"
            ]
        );
    }

    /// A local names a location and a stretch of code, and both have to be
    /// there. Nothing runs one — it is read when a person asks what a frame
    /// holds — so the fault it prevents is not a wrong answer at a
    /// collection but a debugger reading a slot or an instruction that does
    /// not exist.
    #[test]
    fn a_local_outside_the_frame_or_past_the_last_instruction_is_a_fault() {
        let mut f = function(
            vec![Repr::Int, Repr::Int],
            INT,
            vec![Inst::Return { src: 0 }],
        );
        f.locals = vec![
            Local {
                name: Arc::from("wide"),
                slot: 1,
                layout: POINT,
                from: 0,
                to: 1,
            },
            Local {
                name: Arc::from("late"),
                slot: 0,
                layout: INT,
                from: 0,
                to: 4,
            },
        ];
        assert_eq!(
            faults(&program(vec![f])),
            vec![
                "local `wide` is `Point`, 2 words at slot 1, and the frame has 2".to_string(),
                "local `late` is live to 4 and the function has 1 instructions".to_string(),
            ]
        );
    }

    /// And a range has to be one: `[from, to)` is half-open, so `from > to`
    /// is not an empty binding but a table nothing can be read out of.
    #[test]
    fn a_local_bound_after_it_is_freed_is_a_fault() {
        let mut f = function(vec![Repr::Int], INT, vec![Inst::Return { src: 0 }]);
        f.locals = vec![Local {
            name: Arc::from("backwards"),
            slot: 0,
            layout: INT,
            from: 1,
            to: 0,
        }];
        assert_eq!(
            faults(&program(vec![f])),
            vec!["local `backwards` is bound at 1 and freed at 0"]
        );
    }

    #[test]
    fn a_call_whose_arguments_are_not_the_callee_s_parameters_is_a_fault() {
        let mut callee = function(
            vec![Repr::Int, Repr::Int, Repr::Int],
            INT,
            vec![Inst::Return { src: 2 }],
        );
        callee.params = vec![POINT];
        callee.name = Arc::from("g");
        let caller = function(
            vec![Repr::Int, Repr::Ref, Repr::Int],
            INT,
            vec![
                Inst::Call {
                    dst: 0,
                    callee: FunctionId(0),
                    args: crate::ArgsId(0),
                },
                Inst::Return { src: 0 },
            ],
        );
        let mut held = program(vec![callee, caller]);
        held.args = vec![vec![Arg {
            slot: 1,
            layout: POINT,
        }]];
        assert_eq!(
            faults(&held),
            vec![
                "argument 0 of `m.g` is `Point`, whose word 0 is int, but slot 1 holds ref"
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_call_that_passes_the_wrong_number_of_arguments_is_a_fault() {
        let mut callee = function(
            vec![Repr::Int, Repr::Int],
            INT,
            vec![Inst::Return { src: 1 }],
        );
        callee.params = vec![INT];
        callee.name = Arc::from("g");
        let caller = function(
            vec![Repr::Int],
            INT,
            vec![
                Inst::Call {
                    dst: 0,
                    callee: FunctionId(0),
                    args: crate::ArgsId(0),
                },
                Inst::Return { src: 0 },
            ],
        );
        let mut held = program(vec![callee, caller]);
        held.args = vec![Vec::new()];
        assert_eq!(
            faults(&held),
            vec!["passes 0 arguments to `m.g`, which declares 1"]
        );
    }

    /// The whole of why a discriminant is a `Repr` of its own.
    ///
    /// A tag and an `Int` are the same bits in the same kind of word, and
    /// before this they were the same *type*, so nothing stopped an enum's
    /// case index being added to. Nothing rejects it at run time either —
    /// the machine adds two words — so the only place it can be caught is
    /// here.
    #[test]
    fn a_tag_cannot_be_added_to() {
        let f = function(
            vec![Repr::Int, Repr::Ref, Repr::Tag],
            ANSWER,
            vec![
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 0,
                    a: 2,
                    b: 0,
                },
                Inst::Return { src: 0 },
            ],
        );
        assert_eq!(
            faults(&program(vec![f])),
            vec!["slot 2 holds tag, but this wants int or duration"]
        );
    }

    /// And cannot be ordered, or compared against an integer.
    ///
    /// Equality is refused with the rest: two tags are compared by
    /// dispatching on one, which is what [`Inst::Switch`] is, and a tag
    /// against an `Int` is the confusion this separation exists to name.
    #[test]
    fn a_tag_cannot_be_ordered_or_compared_as_an_integer() {
        let ordered = function(
            vec![Repr::Int, Repr::Ref, Repr::Tag, Repr::Bool],
            ANSWER,
            vec![
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 3,
                    a: 2,
                    b: 0,
                },
                Inst::Return { src: 0 },
            ],
        );
        assert_eq!(
            faults(&program(vec![ordered])),
            vec!["slot 2 holds tag, but this wants int or duration"]
        );

        let equal = function(
            vec![Repr::Int, Repr::Ref, Repr::Tag, Repr::Bool],
            ANSWER,
            vec![
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Eq,
                    dst: 3,
                    a: 2,
                    b: 0,
                },
                Inst::Return { src: 0 },
            ],
        );
        assert_eq!(
            faults(&program(vec![equal])),
            vec!["slot 2 holds tag, but this wants int or duration"]
        );
    }

    /// What a tag *is* accepted by: the instruction that writes one, and the
    /// one that dispatches on it. A test that only showed the refusals would
    /// pass if the whole family were rejected.
    #[test]
    fn a_tag_is_written_and_dispatched_on() {
        let f = function(
            vec![Repr::Int, Repr::Ref, Repr::Tag],
            ANSWER,
            vec![
                Inst::Tag {
                    dst: 2,
                    layout: ENUM,
                    case: crate::CaseId(1),
                },
                Inst::Switch {
                    on: 2,
                    table: TableId(0),
                },
                Inst::Return { src: 0 },
            ],
        );
        let mut held = program(vec![f]);
        held.tables = vec![Table {
            targets: vec![2, 2],
            default: 2,
        }];
        assert_eq!(faults(&held), Vec::<String>::new());
    }

    /// A case index is bounded against the enum the same instruction names,
    /// which is the check the untyped integer path had no way to make.
    #[test]
    fn a_tag_naming_a_case_the_enum_does_not_have_is_a_fault() {
        let f = function(
            vec![Repr::Int, Repr::Ref, Repr::Tag],
            ANSWER,
            vec![
                Inst::Tag {
                    dst: 2,
                    layout: ENUM,
                    case: crate::CaseId(7),
                },
                Inst::Return { src: 0 },
            ],
        );
        assert_eq!(
            faults(&program(vec![f])),
            vec!["names case7 of m.E, which has 2 case(s)"]
        );
    }

    #[test]
    fn a_switch_on_something_that_is_not_a_discriminant_word_is_a_fault() {
        // The discriminant of an enum location is its first word and is an
        // `Int`; so is the layout id a `dyn` dispatch switches on. A slot's
        // `Repr` is the strongest thing a static check has to say about
        // which word this is.
        let f = function(
            vec![Repr::Int, Repr::Ref],
            ANSWER,
            vec![
                Inst::Switch {
                    on: 1,
                    table: TableId(0),
                },
                Inst::Return { src: 0 },
            ],
        );
        let mut held = program(vec![f]);
        held.tables = vec![Table {
            targets: vec![1],
            default: 1,
        }];
        assert_eq!(
            faults(&held),
            vec!["slot 1 holds ref, but this wants tag or int"]
        );
    }

    #[test]
    fn a_jump_that_lands_past_the_last_instruction_is_a_fault() {
        let f = function(
            vec![Repr::Int],
            INT,
            vec![Inst::Jump { to: 9 }, Inst::Return { src: 0 }],
        );
        assert_eq!(
            faults(&program(vec![f])),
            vec!["jumps to 9, past the 2 instructions"]
        );
    }

    #[test]
    fn an_id_outside_its_table_is_a_fault() {
        let f = function(
            vec![Repr::Ref],
            STR,
            vec![
                Inst::Str {
                    dst: 0,
                    text: crate::StrId(3),
                },
                Inst::Return { src: 0 },
            ],
        );
        assert_eq!(
            faults(&program(vec![f])),
            vec!["names string 3, and there are 0"]
        );
    }

    #[test]
    fn a_body_whose_last_instruction_falls_through_is_a_fault() {
        let f = function(vec![Repr::Int], INT, vec![Inst::Int { dst: 0, value: 1 }]);
        assert_eq!(
            faults(&program(vec![f])),
            vec!["the last instruction can fall through, and there is nothing after it"]
        );
    }

    /// An argument used to be checked only for existing, because it was a
    /// slot and a slot cannot run off the end of anything. It carries the
    /// layout of the location it names now, so a two-word argument at the
    /// last slot of a frame is a fault here rather than a read of the frame
    /// above at run time.
    #[test]
    fn an_argument_that_runs_off_the_end_of_the_frame_is_a_fault() {
        let f = function(
            vec![Repr::Int, Repr::Int, Repr::Bool],
            INT,
            vec![
                Inst::IntrinsicCall {
                    dst: 0,
                    site: crate::SiteId(0),
                    args: crate::ArgsId(0),
                },
                Inst::Return { src: 0 },
            ],
        );
        let mut held = program(vec![f]);
        held.intrinsic_sites = vec![crate::IntrinsicSite {
            intrinsic: crate::Intrinsic::ValueOrder,
            result: INT,
        }];
        held.args = vec![vec![
            Arg {
                slot: 2,
                layout: POINT,
            },
            Arg {
                slot: 0,
                layout: INT,
            },
        ]];
        assert_eq!(
            faults(&held),
            vec!["argument 0 is `Point`, 2 words at slot 2, and the frame has 3"]
        );
    }

    /// A program with one function calling `intrinsic` over `args`, answering
    /// `result` into slot 0 of a frame of `reprs`.
    fn calling(
        intrinsic: crate::Intrinsic,
        result: LayoutId,
        reprs: Vec<Repr>,
        args: Vec<Arg>,
    ) -> Program {
        let f = function(
            reprs,
            result,
            vec![
                Inst::IntrinsicCall {
                    dst: 0,
                    site: crate::SiteId(0),
                    args: crate::ArgsId(0),
                },
                Inst::Return { src: 0 },
            ],
        );
        let mut held = program(vec![f]);
        held.intrinsic_sites = vec![crate::IntrinsicSite { intrinsic, result }];
        held.args = vec![args];
        held
    }

    /// An intrinsic's signature is held to at every call: how many operands,
    /// what each one is, and what the answer is (#378, P5-3). The machine's
    /// arms re-check none of the three.
    #[test]
    fn a_builtin_call_is_held_to_its_intrinsics_signature() {
        let string = |slot| Arg { slot, layout: STR };
        let int = |slot| Arg { slot, layout: INT };
        // Slot 0 holds the answer and slot 3 is the `Int` an operand fault is
        // made of. The one-operand sample below was `String.length` until ADR
        // 0064 moved it into `std.string`; `String.trim` is the same shape
        // with a `String` answer instead of an `Int` one, which is why slot 0
        // is a reference here. Nothing this test asserts is about the answer's
        // class — `String.indexOf` below is what checks that — so the swap
        // costs the case nothing.
        let reprs = || vec![Repr::Ref, Repr::Ref, Repr::Ref, Repr::Int];

        // `String.trim` over one `String`, answering a `String`: nothing.
        let held = calling(crate::Intrinsic::StringTrim, STR, reprs(), vec![string(1)]);
        assert_eq!(faults(&held), Vec::<String>::new());

        // One operand too many.
        let held = calling(
            crate::Intrinsic::StringTrim,
            STR,
            reprs(),
            vec![string(1), string(2)],
        );
        assert_eq!(
            faults(&held),
            vec!["`String.trim` takes 1 operand(s), and this call passes 2"]
        );

        // An `Int` where a `String` goes.
        let held = calling(crate::Intrinsic::StringTrim, STR, reprs(), vec![int(3)]);
        assert_eq!(
            faults(&held),
            vec!["operand 0 of `String.trim` is `Int`, where its signature has String"]
        );

        // The answer a `String.indexOf` writes is an `Option<Int>`, and the
        // fixture's only `Option` carries a reference.
        let held = calling(
            crate::Intrinsic::StringIndexOf,
            ANSWER,
            vec![Repr::Int, Repr::Ref, Repr::Ref, Repr::Ref],
            vec![string(2), string(3)],
        );
        assert_eq!(
            faults(&held),
            vec!["the answer of `String.indexOf` is `Option`, where its signature has Option<Int>"]
        );

        // A rendering takes a piece of any layout, and appends it to a byte
        // buffer and nothing else. No intrinsic takes a list of pieces any
        // more (#403), so a third operand is a count fault like any other.
        let point = |slot| Arg {
            slot,
            layout: POINT,
        };
        let held = calling(
            crate::Intrinsic::ValueRenderInto,
            INT,
            vec![Repr::Int, Repr::Int, Repr::Int, Repr::Ref],
            vec![point(1), string(3)],
        );
        assert_eq!(
            faults(&held),
            vec![
                "the answer of `Value.renderInto` is `Int`, where its signature has Unit",
                "operand 1 of `Value.renderInto` is `String`, where its signature has ByteBuffer",
            ]
        );
        let held = calling(
            crate::Intrinsic::ValueRenderInto,
            INT,
            vec![Repr::Int, Repr::Int, Repr::Int, Repr::Ref],
            vec![point(1), string(3), string(3)],
        );
        assert_eq!(
            faults(&held),
            vec![
                "the answer of `Value.renderInto` is `Int`, where its signature has Unit",
                "`Value.renderInto` takes 2 operand(s), and this call passes 3",
            ]
        );
    }

    /// A text or scalar intrinsic handed a collection is refused as that,
    /// which is what makes a new collection builtin a verification failure
    /// rather than a runtime arm (ADR 0058, Phase 5). A value intrinsic may
    /// be handed one: `==` on two arrays is a walk of both.
    #[test]
    fn a_collection_is_refused_by_a_text_or_scalar_intrinsic() {
        let array = |slot| Arg {
            slot,
            layout: ARRAY_INT,
        };
        let held = calling(
            crate::Intrinsic::StringTrim,
            STR,
            vec![Repr::Ref, Repr::Ref],
            vec![array(1)],
        );
        assert_eq!(
            faults(&held),
            vec![
                "operand 0 of `String.trim` is the collection `Array<Int>`, and a Text intrinsic \
                 takes none: a collection operation is a run instruction or the standard \
                 library's, not an intrinsic (ADR 0058)"
            ]
        );

        // `Array<Int>` is not the `Array<String>` `join` names.
        let held = calling(
            crate::Intrinsic::StringJoin,
            STR,
            vec![Repr::Ref, Repr::Ref, Repr::Ref],
            vec![
                Arg {
                    slot: 1,
                    layout: STR,
                },
                array(2),
            ],
        );
        assert_eq!(
            faults(&held),
            vec![
                "operand 1 of `String.join` is `Array<Int>`, where its signature has Array<String>"
            ]
        );

        let held = calling(
            crate::Intrinsic::ValueOrder,
            INT,
            vec![Repr::Int, Repr::Ref, Repr::Ref],
            vec![array(1), array(2)],
        );
        assert_eq!(faults(&held), Vec::<String>::new());
    }

    /// A closure call's destination is checked like every other call's.
    ///
    /// Which body the call enters is a run-time fact and how wide its answer
    /// is, is not: the checker settled the call against the callee's function
    /// type, so `Inst::CallClosure` carries the layout and this asks the same
    /// `fits` question of it. Before it did, a two-word answer written into
    /// the last slot of a frame was checked by nothing here, and the machine
    /// wrote the frame above it.
    #[test]
    fn a_closure_calls_answer_that_runs_off_the_end_of_the_frame_is_a_fault() {
        let mut held = program(vec![function(
            vec![Repr::Int, Repr::Ref, Repr::Int],
            INT,
            vec![
                Inst::CallClosure {
                    dst: 2,
                    closure: 1,
                    args: ArgsId(0),
                    result: POINT,
                },
                Inst::Return { src: 0 },
            ],
        )]);
        held.args.push(Vec::new());
        assert_eq!(
            faults(&held),
            vec![
                "the answer of a closure call is `Point`, 2 words at slot 2, and the frame has 3"
                    .to_string()
            ]
        );
    }

    /// And the same call whose destination is a location of that layout has
    /// nothing said about it. `Point` is two `Int` words and slots 0 and 1
    /// are two.
    #[test]
    fn a_closure_call_whose_answer_fits_its_destination_is_well_formed() {
        let mut held = program(vec![function(
            vec![Repr::Int, Repr::Int, Repr::Ref],
            INT,
            vec![
                Inst::CallClosure {
                    dst: 0,
                    closure: 2,
                    args: ArgsId(0),
                    result: POINT,
                },
                Inst::Return { src: 0 },
            ],
        )]);
        held.args.push(Vec::new());
        assert_eq!(faults(&held), Vec::<String>::new());
    }

    /// Two layouts can have the same words and not be the same family, and it
    /// is the argument's layout the machine hands a builtin and a host — so
    /// the layouts are compared and not only the locations' reprs.
    #[test]
    fn an_argument_passed_as_another_family_than_the_parameter_is_a_fault() {
        let mut callee = function(
            vec![Repr::Int, Repr::Int],
            INT,
            vec![Inst::Return { src: 0 }],
        );
        callee.params = vec![POINT];
        callee.name = Arc::from("g");
        let caller = function(
            vec![Repr::Int, Repr::Int],
            INT,
            vec![
                Inst::Call {
                    dst: 0,
                    callee: FunctionId(0),
                    args: crate::ArgsId(0),
                },
                Inst::Return { src: 0 },
            ],
        );
        let mut held = program(vec![callee, caller]);
        held.args = vec![vec![Arg {
            slot: 0,
            layout: PAIR,
        }]];
        assert_eq!(
            faults(&held),
            vec!["argument 0 of `m.g` is passed as a `Pair`, and the parameter is a `Point`"]
        );
    }

    /// A box's width is in the header its allocator writes and not in its
    /// shape, so `Alloc` would size one by the wrong thing: a box of a
    /// two-word value would be a word short and the copy into it would run
    /// off the end of the object. `Inst::Box` is the only correct allocator
    /// for one, because it is the only one that is told what is going in.
    #[test]
    fn allocating_a_box_by_its_shape_is_a_fault() {
        let f = function(
            vec![Repr::Ref],
            STR,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: BOXED,
                    len: Len::Fixed,
                },
                Inst::Return { src: 0 },
            ],
        );
        assert_eq!(
            faults(&program(vec![f])),
            vec![
                "allocates a `Any`, whose width the header carries and the shape does not; a box \
                 is allocated by `box`, which knows what is going into it"
            ]
        );
    }

    /// A field access is bounded against the object wherever the slot holding
    /// it is written by allocations alone, all naming one layout — which is
    /// what a lowering that allocates an object and reads its fields does.
    /// Without it a `Copy` at the top of a frame reads the frame above and
    /// the machine's own header check is the only thing left.
    #[test]
    fn a_field_past_an_object_of_a_known_layout_is_a_fault() {
        let f = function(
            vec![Repr::Ref, Repr::Int, Repr::Int],
            INT,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: POINT,
                    len: Len::Fixed,
                },
                Inst::LoadField {
                    dst: 1,
                    obj: 0,
                    at: 1,
                    layout: PAIR,
                },
                Inst::Return { src: 1 },
            ],
        );
        assert_eq!(
            faults(&program(vec![f])),
            vec!["read 2 word(s) at word 1 of a `Point`, which has 2"]
        );
    }

    /// And it says nothing where it cannot: a slot a copy wrote holds
    /// whatever the source held, and a `Shape::Str` object is as long as it
    /// was allocated. Both are the machine's to answer, from the header.
    #[test]
    fn a_field_of_an_object_whose_layout_is_not_static_is_left_to_the_machine() {
        let f = function(
            vec![Repr::Ref, Repr::Ref, Repr::Int],
            INT,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: POINT,
                    len: Len::Fixed,
                },
                Inst::Copy {
                    dst: 1,
                    src: 0,
                    layout: STR,
                },
                Inst::LoadField {
                    dst: 2,
                    obj: 1,
                    at: 9,
                    layout: INT,
                },
                Inst::Str {
                    dst: 0,
                    text: crate::StrId(0),
                },
                Inst::LoadField {
                    dst: 2,
                    obj: 0,
                    at: 9,
                    layout: INT,
                },
                Inst::Return { src: 2 },
            ],
        );
        let mut held = program(vec![f]);
        held.strings = vec![Arc::from("x")];
        // The first `LoadField` names a slot two allocations disagree about
        // and the second an object whose payload the header decides.
        assert_eq!(faults(&held), Vec::<String>::new());
    }

    /// A closure's callee is carried twice — once in
    /// [`Shape::Closure::function`], the typed fact, and once in the word
    /// [`Inst::FuncRef`] writes into its environment's callee field — and
    /// until [`Check::check_closure_callee`] nothing compared them. `m.g` is
    /// what [`CLOSURE`]'s layout says the environment holds; the body writes
    /// `m.f` into it instead.
    ///
    /// [Issue #275](https://github.com/myuon/cove/issues/275) is why the
    /// message names them `@m.f` and `@m.g` rather than `fn0` and `fn1`: a
    /// program's second function is given a name of its own, `g`, distinct
    /// from [`function`]'s hard-coded `f`, purely so this message has two
    /// different symbols to tell apart rather than `m.f` disagreeing with
    /// itself.
    #[test]
    fn a_closures_environment_naming_a_different_callee_than_its_layout_is_a_fault() {
        let f = function(
            vec![Repr::Ref, Repr::Int],
            INT,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: CLOSURE,
                    len: Len::Fixed,
                },
                Inst::FuncRef {
                    dst: 1,
                    callee: FunctionId(0),
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: INT,
                },
                Inst::Return { src: 1 },
            ],
        );
        let mut other = function(vec![Repr::Int], INT, vec![Inst::Return { src: 0 }]);
        other.name = Arc::from("g");
        assert_eq!(
            faults(&program(vec![f, other])),
            vec![
                "stores @m.f into the callee field of a `closure g` closure, whose layout names @m.g"
            ]
        );
    }

    /// The same shape, agreeing: `f#0`'s environment says `f#0`.
    #[test]
    fn a_closures_environment_naming_its_own_layouts_callee_is_well_formed() {
        let f = function(
            vec![Repr::Ref, Repr::Int],
            INT,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: CLOSURE,
                    len: Len::Fixed,
                },
                Inst::FuncRef {
                    dst: 1,
                    callee: FunctionId(1),
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: INT,
                },
                Inst::Return { src: 1 },
            ],
        );
        let other = function(vec![Repr::Int], INT, vec![Inst::Return { src: 0 }]);
        assert_eq!(faults(&program(vec![f, other])), Vec::<String>::new());
    }

    #[test]
    fn a_clear_agrees_with_the_layout_it_zeroes() {
        let f = function(
            vec![Repr::Int, Repr::Ref, Repr::Unit],
            INT,
            vec![
                Inst::Clear {
                    slot: 0,
                    layout: ANSWER,
                },
                Inst::Clear {
                    slot: 1,
                    layout: ANSWER,
                },
                Inst::Return { src: 0 },
            ],
        );
        // The first is right — `[Int, Ref]` is what an `Option` is — and the
        // second names the same layout one word along, where it is not.
        assert_eq!(
            faults(&program(vec![f])),
            vec!["what a clear zeroes is `Option`, whose word 0 is int, but slot 1 holds ref"]
        );
    }

    /// A run load is admitted over packed bytes and nothing else, until the
    /// word member has an opcode and a producer (ADR 0058, Phase 2): a
    /// `Storage::Words` here is refused by name, and the byte form beside it
    /// is well formed.
    #[test]
    fn a_run_load_over_words_is_a_fault() {
        let load = |storage| {
            function(
                vec![Repr::Ref, Repr::Int, Repr::Int],
                INT,
                vec![
                    Inst::RunLoad {
                        dst: 2,
                        run: 0,
                        index: 1,
                        storage,
                    },
                    Inst::Return { src: 2 },
                ],
            )
        };
        assert_eq!(
            faults(&program(vec![load(crate::Storage::PackedBytes)])),
            Vec::<String>::new()
        );
        assert_eq!(
            faults(&program(vec![load(crate::Storage::Words(INT))])),
            vec!["loads a unit of a run of `Int` words, and this instruction admits only packed bytes"]
        );
    }

    /// ADR 0058's admission table for the growable family and its finish: an
    /// allocation over either storage, a truncate over words alone, and a byte
    /// finish only as a UTF-8 finish into `String`. Each disallowed
    /// combination is refused by name, and the admitted form of each is well
    /// formed.
    #[test]
    fn a_growable_instruction_outside_the_admission_table_is_a_fault() {
        use crate::{Storage, Validation};
        let bytes = Storage::PackedBytes;
        let words = Storage::Words(INT);
        // s0: owner, s1: int, s2: ref answer.
        let one = |inst: Inst| {
            let mut held = program(vec![function(
                vec![Repr::Ref, Repr::Int, Repr::Ref],
                INT,
                vec![inst, Inst::Return { src: 1 }],
            )]);
            held.args.push(vec![
                Arg {
                    slot: 0,
                    layout: STR,
                },
                Arg {
                    slot: 2,
                    layout: STR,
                },
                Arg {
                    slot: 1,
                    layout: INT,
                },
                Arg {
                    slot: 1,
                    layout: INT,
                },
            ]);
            faults(&held)
        };
        let alloc = |storage| Inst::GrowableAlloc {
            dst: 0,
            capacity: 1,
            storage,
        };
        let truncate = |storage| Inst::GrowableTruncate {
            owner: 0,
            len: 1,
            storage,
        };
        let finish = |target, validation, storage| Inst::RunFinish {
            dst: 2,
            owner: 0,
            target,
            validation,
            storage,
        };
        let none: Vec<String> = Vec::new();
        assert_eq!(one(alloc(bytes)), none);
        assert_eq!(one(finish(STR, Validation::Utf8, bytes)), none);

        // An allocation admits both, since `core.vectorWithCapacity` lowers to
        // the word member: the owner and the store are derived from the
        // element, so there is nothing here to hold them to.
        assert_eq!(one(alloc(words)), none);
        // A truncate is the other way round: words and not bytes.
        assert_eq!(one(truncate(words)), none);
        assert_eq!(
            one(truncate(bytes)),
            vec!["truncates a run of packed bytes, and this instruction admits only words"]
        );
        // A word finish is admitted into the fixed run of its element, and
        // into the `Set` of it or the `Map` whose entry it is (#378, P4-5),
        // with nothing to validate, and refused into anything else.
        let refused_into = |unit: &str, named: &str| {
            vec![format!(
                "finishes a run of `{unit}` words into `{named}`, and a word run finishes into \
                 the fixed `Elements` of the same element, or the `Members` or `Entries` whose \
                 unit it is"
            )]
        };
        assert_eq!(one(finish(ARRAY_INT, Validation::None, words)), none);
        assert_eq!(one(finish(SET_INT, Validation::None, words)), none);
        assert_eq!(
            one(finish(MAP_INT, Validation::None, Storage::Words(ENTRY_INT))),
            none
        );
        assert_eq!(
            one(finish(MAP_INT, Validation::None, words)),
            refused_into("Int", "Map<Int, Int>")
        );
        assert_eq!(
            one(finish(SET_INT, Validation::None, Storage::Words(ENTRY_INT))),
            refused_into("MapEntry", "Set<Int>")
        );
        assert_eq!(
            one(finish(STR, Validation::None, words)),
            refused_into("Int", "String")
        );
        assert_eq!(
            one(finish(ARRAY_INT, Validation::Utf8, words)),
            vec![
                "finishes a run of `Int` words with validation `Utf8`, and a word run has \
                 nothing to validate"
            ]
        );
        assert_eq!(
            one(finish(STR, Validation::None, bytes)),
            vec![
                "finishes a run of packed bytes with validation `None`, and a byte run becomes \
                 a `String` only through `Utf8`"
            ]
        );
        assert_eq!(
            one(finish(POINT, Validation::Utf8, bytes)),
            vec!["finishes a run of packed bytes into `Point`, and a byte run finishes into `String`"]
        );
    }

    /// A run slice is admitted over words, answering the fixed run of its
    /// element, and over bytes, answering a `String`, from a row of four —
    /// `dst`, `src`, `from`, `count` — and each other shape is refused by name.
    #[test]
    fn a_run_slice_answers_the_fixed_run_of_its_element() {
        use crate::Storage;
        // s0: dst, s1: src, s2: from, s3: count.
        let one = |storage: Storage, row: Vec<Arg>| {
            let mut held = program(vec![function(
                vec![Repr::Ref, Repr::Ref, Repr::Int, Repr::Int],
                INT,
                vec![
                    Inst::RunSlice {
                        args: ArgsId(0),
                        storage,
                    },
                    Inst::Return { src: 2 },
                ],
            )]);
            held.args.push(row);
            faults(&held)
        };
        let row = |target| {
            vec![
                Arg {
                    slot: 0,
                    layout: target,
                },
                Arg {
                    slot: 1,
                    layout: ARRAY_INT,
                },
                Arg {
                    slot: 2,
                    layout: INT,
                },
                Arg {
                    slot: 3,
                    layout: INT,
                },
            ]
        };
        let words = Storage::Words(INT);
        assert_eq!(one(words, row(ARRAY_INT)), Vec::<String>::new());
        assert_eq!(
            one(words, row(STR)),
            vec![
                "slices a run of `Int` words into `String`, and a word slice answers the fixed \
                 `Elements` of the same element"
            ]
        );
        assert_eq!(one(Storage::PackedBytes, row(STR)), Vec::<String>::new());
        assert_eq!(
            one(Storage::PackedBytes, row(ARRAY_INT)),
            vec![
                "slices a run of packed bytes into `Array<Int>`, and a byte slice answers `String`"
            ]
        );
        let mut short = row(ARRAY_INT);
        short.pop();
        assert_eq!(
            one(words, short),
            vec!["slices a run with 3 argument(s), and this needs 4 (dst, src, from, count)"]
        );
    }

    /// The reservation rule's frame: `s0` an owner, `s1` its length, `s2` a
    /// count, `s3` its store, `s4..s6` a unit (two words for a `Point`), `s5` a
    /// second count, `s7` another reference, `s8` an `Int`, `s9` a `Bool`.
    fn window_reprs() -> Vec<Repr> {
        vec![
            Repr::Ref,
            Repr::Int,
            Repr::Int,
            Repr::Ref,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Ref,
            Repr::Int,
            Repr::Bool,
        ]
    }

    /// The faults of `code` as `f`, beside a `g` that answers an `Int`, with one
    /// argument row: `run-copy`'s `[s3, s1, s7, s8, s2]`.
    fn window_faults(code: Vec<Inst>) -> Vec<String> {
        let mut held = program(vec![
            function(window_reprs(), INT, code),
            function(
                vec![Repr::Int],
                INT,
                vec![Inst::Int { dst: 0, value: 0 }, Inst::Return { src: 0 }],
            ),
        ]);
        let arg = |slot, layout| Arg { slot, layout };
        held.args.push(vec![
            arg(3, STR),
            arg(1, INT),
            arg(7, STR),
            arg(8, INT),
            arg(2, INT),
        ]);
        held.args.push(Vec::new());
        faults(&held)
    }

    fn length() -> Inst {
        Inst::LoadField {
            dst: 1,
            obj: 0,
            at: GROWABLE_LEN,
            layout: INT,
        }
    }

    fn store() -> Inst {
        Inst::LoadField {
            dst: 3,
            obj: 0,
            at: GROWABLE_STORE,
            layout: STR,
        }
    }

    fn ensure(storage: crate::Storage) -> Inst {
        Inst::GrowableEnsure {
            owner: 0,
            additional: 2,
            storage,
        }
    }

    fn commit(count: Slot, storage: crate::Storage) -> Inst {
        Inst::GrowableCommit {
            owner: 0,
            count,
            storage,
        }
    }

    fn put(index: Slot, layout: LayoutId) -> Inst {
        Inst::StoreElem {
            obj: 3,
            index,
            src: 4,
            layout,
        }
    }

    /// A push of one element: the length before the ensure, the store after
    /// it, the write at the length, a clear of the store, and a commit of a
    /// second constant `1`.
    fn push_window(layout: LayoutId) -> Vec<Inst> {
        let words = crate::Storage::Words(layout);
        vec![
            length(),
            Inst::Int { dst: 2, value: 1 },
            ensure(words),
            store(),
            put(1, layout),
            Inst::Clear {
                slot: 3,
                layout: STR,
            },
            Inst::Int { dst: 5, value: 1 },
            commit(5, words),
            Inst::Return { src: 1 },
        ]
    }

    /// The shapes ADR 0062's producers will emit are accepted: a push over a
    /// one-word element and over a two-word one, a byte store, a bulk copy
    /// whose count is not a constant, and an ensure nothing was written into,
    /// which lapses at the end of its block.
    #[test]
    fn a_reservation_written_once_and_committed_is_well_formed() {
        let none = Vec::<String>::new();
        assert_eq!(window_faults(push_window(INT)), none);
        assert_eq!(window_faults(push_window(POINT)), none);

        let bytes = crate::Storage::PackedBytes;
        assert_eq!(
            window_faults(vec![
                length(),
                Inst::Int { dst: 2, value: 1 },
                ensure(bytes),
                store(),
                Inst::RunStore {
                    run: 3,
                    index: 1,
                    src: 4,
                    storage: bytes,
                },
                commit(2, bytes),
                Inst::Return { src: 1 },
            ]),
            none
        );

        // An append: the count is a length read at run time, so the copy and
        // the commit are held to the count's slot rather than to a constant.
        assert_eq!(
            window_faults(vec![
                Inst::Len { dst: 2, obj: 7 },
                length(),
                ensure(bytes),
                store(),
                Inst::Int { dst: 8, value: 0 },
                Inst::RunCopy {
                    args: ArgsId(0),
                    storage: bytes,
                },
                Inst::Clear {
                    slot: 3,
                    layout: STR,
                },
                commit(2, bytes),
                Inst::Return { src: 1 },
            ]),
            none
        );

        // An ensure that is never written lapses where its block ends.
        assert_eq!(
            window_faults(vec![
                Inst::BranchFalse { cond: 9, to: 3 },
                Inst::Int { dst: 2, value: 1 },
                ensure(bytes),
                Inst::Return { src: 1 },
            ]),
            none
        );
    }

    /// Each clause of the rule, broken once.
    #[test]
    fn a_reservation_that_breaks_a_clause_is_a_fault() {
        let words = crate::Storage::Words(INT);
        let with = |at: usize, inst: Inst| {
            let mut code = push_window(INT);
            code[at] = inst;
            window_faults(code)
        };
        let insert = |at: usize, inst: Inst| {
            let mut code = push_window(INT);
            code.insert(at, inst);
            window_faults(code)
        };
        let has = |faults: Vec<String>, want: &str| {
            assert!(
                faults.iter().any(|fault| fault.contains(want)),
                "wanted a fault containing {want:?}, got {faults:?}"
            );
        };

        // A write before the ensure is not the window's write, so the commit
        // has nothing to publish.
        let mut early = push_window(INT);
        early.swap(2, 4);
        early.swap(2, 3);
        has(window_faults(early), "nothing was written into it");

        // A commit with no write.
        has(
            with(4, Inst::Int { dst: 8, value: 0 }),
            "nothing was written",
        );

        // A commit of another count.
        has(
            with(6, Inst::Int { dst: 5, value: 2 }),
            "commits slot 5, which is not known to hold the count",
        );

        // A write somewhere other than at the length.
        has(with(4, put(8, INT)), "writes at slot 8");

        // A write into a store read before the ensure, which a growth may have
        // replaced.
        let mut stale = push_window(INT);
        stale.swap(2, 3);
        has(
            window_faults(stale),
            "not known to hold the store of slot 0",
        );

        // A write of one unit into a reservation of two.
        has(with(1, Inst::Int { dst: 2, value: 2 }), "writes a count");

        // A call, an allocation, a heap store and a branch inside the window.
        has(
            insert(
                4,
                Inst::Call {
                    dst: 8,
                    callee: FunctionId(1),
                    args: ArgsId(1),
                },
            ),
            "`Call` inside the reservation opened at +2",
        );
        has(
            insert(
                4,
                Inst::Alloc {
                    dst: 7,
                    layout: STR,
                    len: Len::Count(1),
                },
            ),
            "`Alloc` inside the reservation",
        );
        has(
            insert(
                4,
                Inst::StoreField {
                    obj: 7,
                    at: 0,
                    src: 8,
                    layout: INT,
                },
            ),
            "`StoreField` inside the reservation",
        );
        has(
            insert(5, Inst::BranchFalse { cond: 9, to: 6 }),
            "written at +4 and not committed before its block ends",
        );

        // After the write, only what cannot fail.
        has(
            insert(
                5,
                Inst::ArithImm {
                    op: ArithOp::Div,
                    dst: 8,
                    a: 8,
                    value: 1,
                },
            ),
            "cannot fail, now that the reservation is written",
        );

        // The owner's slot written inside the window.
        has(
            insert(
                3,
                Inst::LoadField {
                    dst: 0,
                    obj: 7,
                    at: 0,
                    layout: STR,
                },
            ),
            "holds the owner of the reservation opened at +2",
        );

        // A second ensure while one is open.
        has(insert(3, ensure(words)), "opens a second reservation");

        // A written window that reaches the end of its block uncommitted, and a
        // branch target between the write and the commit: the target ends the
        // block, so the commit after it has nothing open to commit.
        let mut target = push_window(INT);
        target.insert(0, Inst::BranchFalse { cond: 9, to: 7 });
        let faults = window_faults(target);
        has(
            faults.clone(),
            "written at +5 and not committed before its block ends",
        );
        has(faults, "commits onto slot 0 with no reservation open on it");
    }

    /// The instruction checks of the three: an ensure and a commit over either
    /// storage with an owner and an `Int` count, and a byte store — only a
    /// byte store — of an `Int` at an `Int` offset.
    #[test]
    fn a_buffer_primitive_is_checked_like_its_family() {
        let bytes = crate::Storage::PackedBytes;
        let words = crate::Storage::Words(INT);
        let one = |inst: Inst| {
            window_faults(vec![inst, Inst::Return { src: 1 }])
                .into_iter()
                // Each is alone in its block, so the rule has its own say about
                // a commit; what is asked here is the instruction's own check.
                .filter(|fault| !fault.contains("reservation"))
                .collect::<Vec<_>>()
        };
        let none = Vec::<String>::new();
        assert_eq!(one(ensure(bytes)), none);
        assert_eq!(one(ensure(words)), none);
        assert_eq!(one(commit(2, words)), none);
        assert_eq!(
            one(Inst::GrowableEnsure {
                owner: 1,
                additional: 3,
                storage: bytes,
            }),
            vec![
                "slot 1 holds int, but this wants ref".to_string(),
                "slot 3 holds ref, but this wants int".to_string(),
            ]
        );
        let byte = |storage| Inst::RunStore {
            run: 3,
            index: 1,
            src: 4,
            storage,
        };
        assert_eq!(one(byte(bytes)), none);
        assert_eq!(
            one(byte(words)),
            vec!["stores a unit of a run of `Int` words, and this instruction admits only packed bytes"]
        );
    }
}
