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

use crate::inst::{Compare, Inst, Len, Num, Slot};
use crate::layout::{LayoutId, Shape};
use crate::program::{Function, FunctionId, Program};
use crate::repr::{RefMap, Repr};

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

struct Check<'a> {
    program: &'a Program,
    function: &'a Function,
    id: FunctionId,
    /// The layout of the object each reference slot holds, where the whole
    /// function agrees on one. See [`Check::objects`].
    objects: Vec<Option<LayoutId>>,
    faults: &'a mut Vec<Invalid>,
}

impl Check<'_> {
    fn run(&mut self) {
        self.objects = self.objects();
        self.check_frame();
        for pc in 0..self.function.code.len() {
            self.check_inst(pc);
        }
        self.check_falls_off_the_end();
    }

    /// Which slots hold an object whose layout is a static fact.
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
    /// Anything else is `None`, which means the check is skipped rather than
    /// failed. A slot written by a call, a load or a copy holds whatever the
    /// callee or the source held, and this declines to guess.
    fn objects(&self) -> Vec<Option<LayoutId>> {
        // `Some(None)` is "written, by something that says no layout"; `None`
        // is "not written yet". The parameters and the captures are written
        // by the caller, so they start as the first.
        let mut seen: Vec<Option<Option<LayoutId>>> = vec![None; self.function.reprs.len()];
        let unknown = |seen: &mut Vec<Option<Option<LayoutId>>>, slot: Slot, width: u32| {
            for at in slot..slot.saturating_add(width) {
                if let Some(place) = seen.get_mut(at as usize) {
                    *place = Some(None);
                }
            }
        };
        let allocates = |seen: &mut Vec<Option<Option<LayoutId>>>, slot: Slot, id: LayoutId| {
            if let Some(place) = seen.get_mut(slot as usize) {
                *place = match *place {
                    None => Some(Some(id)),
                    Some(Some(held)) if held == id => Some(Some(id)),
                    _ => Some(None),
                };
            }
        };
        let words = |id: LayoutId| {
            self.program
                .layouts
                .get(id.index())
                .map_or(1, |layout| layout.width())
        };
        for at in 0..self.function.param_words(&self.program.layouts) {
            unknown(&mut seen, at, 1);
        }
        for capture in &self.function.captures {
            unknown(&mut seen, capture.slot, words(capture.layout));
        }
        for inst in &self.function.code {
            match *inst {
                // The three that say what they allocate. A `Clear` is not
                // among them and is not a writer either: it stores null, and
                // null is refused by the machine before a layout is asked
                // about.
                Inst::Alloc { dst, layout, .. } => allocates(&mut seen, dst, layout),
                Inst::Str { dst, .. } => allocates(&mut seen, dst, self.program.str_layout),
                Inst::Box { dst, .. } => allocates(&mut seen, dst, self.program.boxed_layout),
                Inst::Clear { .. } | Inst::Jump { .. } | Inst::BranchFalse { .. } => {}
                Inst::Switch { .. } | Inst::Return { .. } | Inst::Trap { .. } => {}
                // Scheduler state, not objects. A `Repr::Task` and a
                // `Repr::Scope` word name a table entry, so there is no
                // layout for one of these to claim.
                Inst::ScopeEnter { dst, .. } => unknown(&mut seen, dst, 1),
                Inst::Spawn { dst, .. } => unknown(&mut seen, dst, 1),
                Inst::Settled { dst, .. } => unknown(&mut seen, dst, 1),
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
                    unknown(&mut seen, failed, 1);
                    unknown(&mut seen, error, words(layout));
                }
                Inst::Await { dst, answer, .. } => unknown(&mut seen, dst, words(answer)),
                // Writes nothing a program can read: what it writes is the
                // run's report of where an assertion failed.
                Inst::AssertFailed { .. } => {}
                Inst::Store { .. } | Inst::StoreField { .. } | Inst::StoreElem { .. } => {}
                Inst::Unit { dst } | Inst::Bool { dst, .. } => unknown(&mut seen, dst, 1),
                Inst::Int { dst, .. } | Inst::Float { dst, .. } => unknown(&mut seen, dst, 1),
                Inst::Neg { dst, .. } | Inst::Not { dst, .. } => unknown(&mut seen, dst, 1),
                Inst::Arith { dst, .. } | Inst::Cmp { dst, .. } => unknown(&mut seen, dst, 1),
                Inst::ArithImm { dst, .. } | Inst::CmpImm { dst, .. } => unknown(&mut seen, dst, 1),
                Inst::Convert { dst, .. } => unknown(&mut seen, dst, 1),
                Inst::Len { dst, .. } | Inst::LayoutOf { dst, .. } => unknown(&mut seen, dst, 1),
                // Forming the address of a slot is also a write to it, as
                // far as this is concerned: a `var` argument is that address
                // handed to a callee, and what the callee stores through it
                // lands in this frame. The checker holds the two to one type
                // and so to one layout, but a static claim about a slot
                // should not rest on an argument made somewhere else.
                Inst::AddrOfSlot { dst, slot } => {
                    unknown(&mut seen, dst, 1);
                    unknown(&mut seen, slot, 1);
                }
                Inst::AddrOfField { dst, .. } => unknown(&mut seen, dst, 1),
                Inst::AddrOfElem { dst, .. } | Inst::AddrOfPart { dst, .. } => {
                    unknown(&mut seen, dst, 1)
                }
                Inst::Copy { dst, layout, .. }
                | Inst::Load { dst, layout, .. }
                | Inst::LoadField { dst, layout, .. }
                | Inst::LoadElem { dst, layout, .. }
                | Inst::Unbox { dst, layout, .. } => unknown(&mut seen, dst, words(layout)),
                Inst::Call { dst, callee, .. } => {
                    let width = match self.program.functions.get(callee.index()) {
                        Some(target) => words(target.returns),
                        None => 1,
                    };
                    unknown(&mut seen, dst, width);
                }
                Inst::CallClosure { dst, .. } => unknown(&mut seen, dst, 1),
                Inst::CallHost { dst, op, .. } | Inst::CallResource { dst, op, .. } => {
                    let width = match self.program.host_ops.get(op.index()) {
                        Some(op) => words(op.result),
                        None => 1,
                    };
                    unknown(&mut seen, dst, width);
                }
                Inst::CallBuiltin { dst, builtin, .. } => {
                    let width = match self.program.builtins.get(builtin.index()) {
                        Some(builtin) => words(builtin.result),
                        None => 1,
                    };
                    unknown(&mut seen, dst, width);
                }
            }
        }
        seen.into_iter().map(Option::flatten).collect()
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
            Inst::CmpImm { dst, a, .. } => {
                self.expect(at, dst, &[Repr::Bool]);
                self.expect(at, a, &[Repr::Int, Repr::Duration]);
            }
            Inst::Cmp { on, dst, a, b, .. } => {
                self.expect(at, dst, &[Repr::Bool]);
                let want: &[Repr] = match on {
                    Compare::Int => &[Repr::Int, Repr::Duration],
                    Compare::Float => &[Repr::Float],
                    Compare::Bool => &[Repr::Bool],
                    Compare::Str => &[Repr::Ref],
                    // `is` compares words, and the only words whose identity
                    // is a language-level question are references.
                    Compare::Identity => &[Repr::Ref],
                };
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
                };
                self.expect(at, a, &[from]);
                self.expect(at, dst, &[into]);
            }
            Inst::Jump { to } => self.target(at, to),
            Inst::BranchFalse { cond, to } => {
                self.expect(at, cond, &[Repr::Bool]);
                self.target(at, to);
            }
            Inst::Switch { on, table } => {
                // The discriminant of an enum location is its first word and
                // is an `Int`; so is the layout id a `dyn` dispatch switches
                // on. Nothing else is dispatched on, and a slot's `Repr` is
                // the strongest thing a static check has to say about which
                // word this is — a location's extent is a fact about the
                // instruction that produced the word, not about the frame.
                self.expect(at, on, &[Repr::Int]);
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
            Inst::CallClosure { dst, closure, args } => {
                self.expect(at, closure, &[Repr::Ref]);
                self.repr(at, dst);
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
            Inst::CallBuiltin { dst, builtin, args } => {
                if self.in_range(at, builtin.index(), self.program.builtins.len(), "builtin") {
                    let result = self.program.builtin(builtin).result;
                    if self.layout_exists(at, result) {
                        self.fits(at, dst, result, "the answer of a builtin");
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

    /// Every argument is a value location of the layout it names, and that
    /// location is inside the frame.
    ///
    /// This is what an argument carrying its layout buys the verifier. It
    /// used to check only that the slot existed, because a slot was the whole
    /// of what an argument was — so a call passing the last slot of a frame
    /// as a two-word `Point` was checked by nothing, and the machine read the
    /// frame above it.
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

    /// What a layout is called, or its id where the table is too short.
    fn name_of(&self, layout: LayoutId) -> String {
        match self.program.layouts.get(layout.index()) {
            Some(held) => held.name.to_string(),
            None => layout.to_string(),
        }
    }
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
    use crate::inst::Inst;
    use crate::layout::{Layout, Shape};
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
        assert_eq!(faults(&held), vec!["slot 1 holds ref, but this wants int"]);
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
                Inst::CallBuiltin {
                    dst: 0,
                    builtin: crate::BuiltinId(0),
                    args: crate::ArgsId(0),
                },
                Inst::Return { src: 0 },
            ],
        );
        let mut held = program(vec![f]);
        held.builtins = vec![crate::Builtin {
            receiver: Arc::from("Any"),
            operation: Arc::from("equals"),
            result: INT,
        }];
        held.args = vec![vec![Arg {
            slot: 2,
            layout: POINT,
        }]];
        assert_eq!(
            faults(&held),
            vec!["argument 0 is `Point`, 2 words at slot 2, and the frame has 3"]
        );
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
}
