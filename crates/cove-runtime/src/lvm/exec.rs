//! The dispatch loop.
//!
//! One `Machine` runs one task's frames over one [`Memory`]. It is a
//! register machine: every instruction names its operands and its
//! destination by slot, and a slot is `memory[frame_base + slot]`.
//!
//! # There is no Rust recursion here
//!
//! A Cove call pushes a frame onto [`Machine::frames`] and continues the same
//! loop. Nothing about a call grows the native stack, so how deep a Cove
//! program may recurse is decided by [`STACK_WORDS`] alone rather than by how
//! large a Rust stack frame the dispatch loop happens to compile to — which
//! is a number that changes when an unrelated instruction is added.
//!
//! # There is no `Value` here
//!
//! Ordinary Cove-to-Cove execution moves words and heap objects. The public
//! `Value` is built at the boundary — a Host call, an entry's answer, a trace
//! capture — and nowhere else. There is no operand `Vec<Value>`, no argument
//! buffer, no spill area and no fallback path, which is what ADR 0034 asks
//! for and what the predecessor could not say.

use std::time::{Duration, Instant};

use cove_diag::Span;
use cove_lir::{
    ArgsId, ArithOp, BuiltinId, CmpOp, Compare, Convert, FunctionId, HostOpId, Inst, LayoutId, Len,
    Num, Program, Repr, Shape, Slot, StrId,
};

use crate::budget::{Cancellation, Meter};
use crate::error::RuntimeError;
use crate::host::{HostRegistry, Reentry};
use crate::lvm::mem::{Collected, Memory, Overflow, Roots};
use crate::lvm::{boundary, builtins};
use crate::runtime::ENTRY_TASK;
// The one import of the public `Value` outside `boundary`, and the one thing
// ADR 0034 allows it for: a host call's arguments and its answer exist as
// `Value`s for the length of the call and nowhere else. Nothing here stores
// one, and the two places it is named — the vector handed to the boundary,
// and the callee a way back is offered — are both in transit.
use crate::value::Value;

/// How many instructions run between two budget checks.
///
/// A budget check reads an atomic and a clock, and doing that per instruction
/// would cost more than most instructions do. Doing it per *call* would let a
/// tight arithmetic loop run unbounded. A fixed stride is the arrangement that
/// bounds both: the run notices a cancellation within a known number of
/// instructions, whatever it is doing.
pub(crate) const SAFEPOINT_STRIDE: u64 = 1024;

/// One live call.
///
/// The top of [`Machine::frames`] is the frame currently executing, not the
/// caller of it. That costs a write of `pc` before anything that can collect
/// or fail, and it buys a collector and an error reporter that need no
/// special case for "and also the one in the local variables".
struct Frame {
    function: FunctionId,
    /// The linear address of slot 0.
    base: u64,
    /// Where this frame resumes: the instruction after the call it is
    /// suspended at, or the one about to run.
    pc: u32,
    /// The slot of the *caller's* frame this call's answer is written to.
    dst: Slot,
}

/// The frames of one task, as the collector sees them.
///
/// A snapshot of addresses rather than a live view, because a collection
/// takes `&mut Memory` and reading a slot takes `&Memory`. Gathering first
/// costs one pass over the reference slots — which the collector was going to
/// make anyway — and keeps [`Memory`] free of any idea of what a frame is.
struct Held<'a>(&'a [u64]);

impl Roots for Held<'_> {
    fn each_root(&self, f: &mut dyn FnMut(u64)) {
        for &addr in self.0 {
            f(addr);
        }
    }
}

/// One task's execution over one linear memory.
pub(crate) struct Machine<'a> {
    program: &'a Program,
    /// The boundary a [`Inst::CallHost`] calls through, if this run has one.
    ///
    /// `None` is a machine with no host behind it — what a test that runs
    /// arithmetic drives, and the same state [`crate::host::NoReentry`]
    /// exists for on the other side of the boundary. A program that reaches
    /// a host call from one is told what is missing rather than being given a
    /// registry that answers nothing.
    hosts: Option<&'a HostRegistry>,
    mem: Memory,
    frames: Vec<Frame>,
    /// The string object for each [`StrId`], allocated on first use.
    ///
    /// A literal in a loop allocates once for the run rather than once per
    /// turn. The table is a root for as long as the machine lives, which is
    /// the price: a string mentioned once and never reached again is retained.
    /// That is the right trade for a *literal*, which the program named
    /// statically and can name again.
    interned: Vec<u64>,
    /// Objects a boundary conversion is holding and no frame names yet.
    ///
    /// A frame is a root because a static map says which of its slots are
    /// references. A half-built object is not: it is reachable only from a
    /// Rust local, which nothing walks, and the next allocation the
    /// conversion makes could collect it out from under itself. So the
    /// conversion says so, explicitly, for exactly as long as that is true —
    /// [`Machine::push_temp`] to take a root, [`Machine::release_temps`] to
    /// give every root back that was taken since a mark.
    ///
    /// It is a stack rather than a set because the discipline is lexical: a
    /// conversion that recurses takes a mark on the way in and releases to it
    /// on the way out, so nothing has to remember which root was whose.
    temps: Vec<u64>,
    /// Reused across collections so a collection does not allocate.
    roots: Vec<u64>,
    instructions: u64,
    /// How long this machine has spent inside host calls.
    ///
    /// The oracle charges the same measurement against every open timing
    /// context so that a run can separate its own work from what it spent
    /// waiting; this machine has one context, which is the run.
    host_wait: Duration,
    collected: Collected,
}

impl<'a> Machine<'a> {
    /// A machine with no host boundary, for a program that calls none.
    pub(crate) fn new(program: &'a Program, heap_words: usize) -> Machine<'a> {
        Machine::with_hosts(program, heap_words, None)
    }

    /// A machine that calls hosts through `hosts`.
    pub(crate) fn with_hosts(
        program: &'a Program,
        heap_words: usize,
        hosts: Option<&'a HostRegistry>,
    ) -> Machine<'a> {
        Machine {
            program,
            hosts,
            mem: Memory::new(heap_words),
            frames: Vec::new(),
            interned: vec![0; program.strings.len()],
            temps: Vec::new(),
            roots: Vec::new(),
            instructions: 0,
            host_wait: Duration::ZERO,
            collected: Collected::default(),
        }
    }

    /// How many instructions this machine has run.
    pub(crate) fn instructions(&self) -> u64 {
        self.instructions
    }

    /// What every collection so far has done.
    pub(crate) fn collected(&self) -> Collected {
        self.collected
    }

    /// Words the heap region occupies, free blocks included.
    pub(crate) fn heap_words(&self) -> u64 {
        self.mem.heap_words()
    }

    /// Words handed out over the whole run, reuse counted each time.
    pub(crate) fn allocated_words(&self) -> u64 {
        self.mem.allocated_words()
    }

    /// How long this machine has waited on hosts.
    pub(crate) fn host_wait(&self) -> Duration {
        self.host_wait
    }

    /// Runs `entry` with `args` already in word form, answering its word.
    ///
    /// The caller converts: this is below the boundary, and nothing here
    /// knows what a public `Value` is.
    pub(crate) fn run(
        &mut self,
        entry: FunctionId,
        args: &[u64],
        budget: &Meter,
    ) -> Result<u64, RuntimeError> {
        let program = self.program;
        let function = program.function(entry);
        debug_assert_eq!(args.len(), function.arity as usize);

        let base = self
            .mem
            .push_frame(function.frame_size())
            .map_err(|Overflow| self.too_deep(function.span))?;
        for (slot, word) in args.iter().enumerate() {
            self.mem.set_slot(base, slot as u32, *word);
        }
        self.frames.push(Frame {
            function: entry,
            base,
            pc: 0,
            dst: 0,
        });
        self.dispatch(budget)
    }

    /// The loop.
    ///
    /// `function`, `base` and `pc` are kept in locals rather than read out of
    /// the top frame on every instruction, and written back at the two points
    /// where something else looks: a collection, and a failure.
    fn dispatch(&mut self, budget: &Meter) -> Result<u64, RuntimeError> {
        let program = self.program;
        let top = self.frames.last().expect("run pushed a frame");
        let mut id = top.function;
        let mut base = top.base;
        let mut pc = top.pc as usize;
        let mut code = &program.function(id).code[..];

        loop {
            self.instructions += 1;
            if self.instructions.is_multiple_of(SAFEPOINT_STRIDE) {
                if let Err(stopped) = budget.safepoint(SAFEPOINT_STRIDE) {
                    self.sync(pc);
                    return Err(budget.to_runtime_error(stopped).at(self.span(id, pc)));
                }
            }

            let inst = &code[pc];
            pc += 1;

            macro_rules! fail {
                ($error:expr) => {{
                    self.sync(pc - 1);
                    return Err($error.at(self.span(id, pc - 1)));
                }};
            }

            match *inst {
                // ---- constants and moves -------------------------------
                Inst::Unit { dst } => self.mem.set_slot(base, dst, 0),
                Inst::Bool { dst, value } => self.mem.set_slot(base, dst, value as u64),
                Inst::Int { dst, value } => self.mem.set_slot(base, dst, value as u64),
                Inst::Float { dst, bits } => self.mem.set_slot(base, dst, bits),
                Inst::Str { dst, text } => {
                    self.sync(pc - 1);
                    match self.intern(text) {
                        Ok(addr) => self.mem.set_slot(base, dst, addr),
                        Err(error) => fail!(error),
                    }
                }
                Inst::Move { dst, src } => {
                    let word = self.mem.slot(base, src);
                    self.mem.set_slot(base, dst, word);
                }
                // The one instruction whose whole purpose is what it stops
                // happening: a reference the frame no longer needs is not a
                // root, so the object it named is unreachable now rather
                // than when this frame returns.
                Inst::Clear { slot } => self.mem.set_slot(base, slot, 0),

                // ---- scalar operations ----------------------------------
                Inst::Neg { num, dst, a } => {
                    let a = self.mem.slot(base, a);
                    let word = match num {
                        Num::Int => match (a as i64).checked_neg() {
                            Some(value) => value as u64,
                            None => fail!(overflowed("negation")),
                        },
                        Num::Float => (-f64::from_bits(a)).to_bits(),
                    };
                    self.mem.set_slot(base, dst, word);
                }
                Inst::Arith { num, op, dst, a, b } => {
                    let (x, y) = (self.mem.slot(base, a), self.mem.slot(base, b));
                    let word = match num {
                        Num::Int => {
                            // Which of the two the operands are decides only
                            // what the message calls the operation. The
                            // arithmetic is the same, because a `Duration` is
                            // nanoseconds and nanoseconds add like integers.
                            let duration = self.repr(id, dst) == Some(Repr::Duration);
                            match int_arith(op, x as i64, y as i64, duration) {
                                Ok(value) => value as u64,
                                Err(error) => fail!(error),
                            }
                        }
                        Num::Float => {
                            let (x, y) = (f64::from_bits(x), f64::from_bits(y));
                            float_arith(op, x, y).to_bits()
                        }
                    };
                    self.mem.set_slot(base, dst, word);
                }
                Inst::Cmp { on, op, dst, a, b } => {
                    let (x, y) = (self.mem.slot(base, a), self.mem.slot(base, b));
                    let answer = match on {
                        Compare::Int => compare(op, (x as i64).cmp(&(y as i64))),
                        Compare::Bool | Compare::Identity => match op {
                            CmpOp::Eq => x == y,
                            CmpOp::Ne => x != y,
                            // The verifier admits only `Eq` and `Ne` here;
                            // ordering a `Bool` or an identity is not a
                            // question the language asks.
                            _ => fail!(RuntimeError::new(
                                "this comparison is not defined for these operands"
                            )),
                        },
                        Compare::Float => {
                            let (x, y) = (f64::from_bits(x), f64::from_bits(y));
                            match op {
                                CmpOp::Eq => x == y,
                                CmpOp::Ne => x != y,
                                CmpOp::Lt => x < y,
                                CmpOp::Le => x <= y,
                                CmpOp::Gt => x > y,
                                CmpOp::Ge => x >= y,
                            }
                        }
                        Compare::Str => {
                            let ordering = self.compare_strings(x, y);
                            compare(op, ordering)
                        }
                    };
                    self.mem.set_slot(base, dst, answer as u64);
                }
                Inst::Not { dst, a } => {
                    let a = self.mem.slot(base, a);
                    self.mem.set_slot(base, dst, (a == 0) as u64);
                }
                Inst::Convert { to, dst, a } => {
                    let a = self.mem.slot(base, a);
                    let word = match to {
                        Convert::IntToFloat => (a as i64 as f64).to_bits(),
                        Convert::FloatToInt => f64::from_bits(a) as i64 as u64,
                    };
                    self.mem.set_slot(base, dst, word);
                }

                // ---- control flow ----------------------------------------
                Inst::Jump { to } => pc = to as usize,
                Inst::BranchFalse { cond, to } => {
                    if self.mem.slot(base, cond) == 0 {
                        pc = to as usize;
                    }
                }
                Inst::Switch { on, table } => {
                    let index = self.mem.slot(base, on) as usize;
                    let table = program.table(table);
                    pc = *table.targets.get(index).unwrap_or(&table.default) as usize;
                }
                Inst::Return { src } => {
                    let answer = self.mem.slot(base, src);
                    self.mem.pop_frame(base);
                    // The frame being left is what says where its answer
                    // goes. Keeping the destination with the callee rather
                    // than re-reading the caller's `Call` means a return
                    // touches one instruction, not two.
                    let done = self.frames.pop().expect("a frame is executing");
                    match self.frames.last() {
                        None => return Ok(answer),
                        Some(caller) => {
                            id = caller.function;
                            base = caller.base;
                            pc = caller.pc as usize;
                            code = &program.function(id).code[..];
                            self.mem.set_slot(base, done.dst, answer);
                        }
                    }
                }

                // ---- calls -------------------------------------------------
                Inst::Call { dst, callee, args } => {
                    let target = program.function(callee);
                    let list = program.arg_list(args);
                    let callee_base = match self.mem.push_frame(target.frame_size()) {
                        Ok(base) => base,
                        Err(Overflow) => fail!(self.too_deep_error()),
                    };
                    for (slot, src) in list.iter().enumerate() {
                        let word = self.mem.slot(base, *src);
                        self.mem.set_slot(callee_base, slot as u32, word);
                    }
                    self.sync(pc);
                    self.frames.push(Frame {
                        function: callee,
                        base: callee_base,
                        pc: 0,
                        dst,
                    });
                    id = callee;
                    base = callee_base;
                    pc = 0;
                    code = &program.function(id).code[..];
                }
                Inst::CallClosure { .. } => {
                    fail!(RuntimeError::new(
                        "this call is not lowered by the linear-memory backend yet"
                    ))
                }
                // The one instruction that leaves the machine. Everything it
                // needs to read out of the frame is read before the call, so
                // that the frames are consistent for the length of it: a host
                // may collect through the boundary, and a boundary that had
                // been handed a stale program counter would walk this frame
                // to a slot the loop had already moved past.
                Inst::CallHost { dst, op, args } => {
                    self.sync(pc - 1);
                    let span = self.span(id, pc - 1);
                    match self.call_host(id, base, op, args, budget, span) {
                        Ok(word) => self.mem.set_slot(base, dst, word),
                        Err(error) => fail!(error),
                    }
                }
                // Not a boundary. A builtin reads the words and the objects
                // the machine already holds, and answers one word; nothing
                // here is materialised into a `Value` on the way.
                Inst::CallBuiltin { dst, builtin, args } => {
                    self.sync(pc - 1);
                    match self.call_builtin(id, base, builtin, args) {
                        Ok(word) => self.mem.set_slot(base, dst, word),
                        Err(error) => fail!(error),
                    }
                }

                // ---- the heap ----------------------------------------------
                Inst::Alloc { dst, layout, len } => {
                    let len = match len {
                        Len::Fixed => 0,
                        Len::Count(n) => n,
                        Len::Slot(slot) => self.mem.slot(base, slot) as u32,
                    };
                    self.sync(pc - 1);
                    match self.allocate(layout, len) {
                        Ok(addr) => self.mem.set_slot(base, dst, addr),
                        Err(error) => fail!(error),
                    }
                }
                Inst::GetWord { dst, obj, at } => {
                    let addr = self.mem.slot(base, obj);
                    match self.checked(addr, at) {
                        Ok(()) => {
                            let word = self.mem.payload(addr, at);
                            self.mem.set_slot(base, dst, word);
                        }
                        Err(error) => fail!(error),
                    }
                }
                Inst::SetWord { obj, at, src } => {
                    let addr = self.mem.slot(base, obj);
                    let word = self.mem.slot(base, src);
                    match self.checked(addr, at) {
                        Ok(()) => self.mem.set_payload(addr, at, word),
                        Err(error) => fail!(error),
                    }
                }
                Inst::GetElem { dst, obj, index } => {
                    let addr = self.mem.slot(base, obj);
                    let at = self.mem.slot(base, index) as i64;
                    match self.element(addr, at) {
                        Ok(at) => {
                            let word = self.mem.payload(addr, at);
                            self.mem.set_slot(base, dst, word);
                        }
                        Err(error) => fail!(error),
                    }
                }
                Inst::SetElem { obj, index, src } => {
                    let addr = self.mem.slot(base, obj);
                    let at = self.mem.slot(base, index) as i64;
                    let word = self.mem.slot(base, src);
                    match self.element(addr, at) {
                        Ok(at) => self.mem.set_payload(addr, at, word),
                        Err(error) => fail!(error),
                    }
                }
                Inst::Len { dst, obj } => {
                    let addr = self.mem.slot(base, obj);
                    if addr == 0 {
                        fail!(null_object());
                    }
                    let len = self.mem.object_len(addr) as i64;
                    self.mem.set_slot(base, dst, len as u64);
                }

                // ---- places --------------------------------------------------
                Inst::AddrOfSlot { dst, slot } => self.mem.set_slot(base, dst, base + slot as u64),
                Inst::AddrOfWord { dst, obj, at } => {
                    let addr = self.mem.slot(base, obj);
                    match self.checked(addr, at) {
                        Ok(()) => {
                            let word = self.mem.payload_addr(addr, at);
                            self.mem.set_slot(base, dst, word);
                        }
                        Err(error) => fail!(error),
                    }
                }
                Inst::AddrOfElem { dst, obj, index } => {
                    let addr = self.mem.slot(base, obj);
                    let at = self.mem.slot(base, index) as i64;
                    match self.element(addr, at) {
                        Ok(at) => {
                            let word = self.mem.payload_addr(addr, at);
                            self.mem.set_slot(base, dst, word);
                        }
                        Err(error) => fail!(error),
                    }
                }
                Inst::Load { dst, addr } => {
                    let addr = self.mem.slot(base, addr);
                    let word = self.mem.read(addr);
                    self.mem.set_slot(base, dst, word);
                }
                Inst::Store { addr, src } => {
                    let addr = self.mem.slot(base, addr);
                    let word = self.mem.slot(base, src);
                    self.mem.write(addr, word);
                }

                // ---- erasure ---------------------------------------------------
                Inst::Box { dst, src, repr } => {
                    let word = self.mem.slot(base, src);
                    self.sync(pc - 1);
                    let boxed = match self.allocate(self.boxed_layout(), 0) {
                        Ok(addr) => addr,
                        Err(error) => fail!(error),
                    };
                    self.mem.set_payload(boxed, 0, repr.tag());
                    self.mem.set_payload(boxed, 1, word);
                    self.mem.set_slot(base, dst, boxed);
                }
                Inst::Unbox { dst, src, repr } => {
                    let addr = self.mem.slot(base, src);
                    if addr == 0 {
                        fail!(null_object());
                    }
                    let tag = self.mem.payload(addr, 0);
                    if Repr::from_tag(tag) != Some(repr) {
                        fail!(RuntimeError::new(
                            "this value is not of the type it is being read as"
                        ));
                    }
                    let word = self.mem.payload(addr, 1);
                    self.mem.set_slot(base, dst, word);
                }

                // ---- failure -----------------------------------------------------
                Inst::Trap { message } => {
                    let message = program.string(message).to_string();
                    fail!(RuntimeError::new(message))
                }
            }
        }
    }

    /// Writes the local program counter back into the top frame.
    ///
    /// Called before anything that reads the frames: a collection, which
    /// walks them for roots, and a failure, which reads a span out of one.
    fn sync(&mut self, pc: usize) {
        if let Some(frame) = self.frames.last_mut() {
            frame.pc = pc as u32;
        }
    }

    fn span(&self, id: FunctionId, pc: usize) -> Span {
        self.program.function(id).span_at(pc)
    }

    fn repr(&self, id: FunctionId, slot: Slot) -> Option<Repr> {
        self.program.function(id).repr(slot)
    }

    fn too_deep(&self, span: Span) -> RuntimeError {
        self.too_deep_error().at(span)
    }

    fn too_deep_error(&self) -> RuntimeError {
        RuntimeError::new("this call nests too deeply")
            .with_rule("A recursion that does not terminate is stopped rather than left to run.")
    }

    /// Allocates, collecting once if the first attempt does not fit.
    fn allocate(&mut self, layout: LayoutId, len: u32) -> Result<u64, RuntimeError> {
        let words = self.program.layout(layout).payload_words(len);
        if let Some(addr) = self.mem.alloc(layout, len, words) {
            return Ok(addr);
        }
        self.collect();
        self.mem
            .alloc(layout, len, words)
            .ok_or_else(|| RuntimeError::new("this run has no memory left"))
    }

    /// Marks from every live frame and every interned string, then sweeps.
    fn collect(&mut self) {
        let program = self.program;
        let mut roots = std::mem::take(&mut self.roots);
        roots.clear();
        for frame in &self.frames {
            let function = program.function(frame.function);
            for slot in function.refs.iter() {
                let word = self.mem.slot(frame.base, slot);
                if word != 0 {
                    roots.push(word);
                }
            }
        }
        roots.extend(self.interned.iter().copied().filter(|addr| *addr != 0));
        roots.extend(self.temps.iter().copied().filter(|addr| *addr != 0));
        self.collected = self.mem.collect(&program.layouts, &Held(&roots));
        self.roots = roots;
    }

    /// How many temporary roots are held, for a caller about to take more.
    ///
    /// The mark to hand back to [`Machine::release_temps`]. Taking it and
    /// releasing to it is the whole discipline: a conversion that recurses
    /// nests marks, and a conversion that fails releases on the way out
    /// because the caller that took the mark is the one that releases it.
    pub(crate) fn temps(&self) -> usize {
        self.temps.len()
    }

    /// Holds `addr` as a root until the mark it was taken after is released.
    ///
    /// What this is for is the window in which an object exists and nothing
    /// the collector walks names it: between the allocation of a struct and
    /// the write of its last field, the object is reachable only from a Rust
    /// local, and building one of those fields can allocate. Without a root
    /// here the collector would be right to free it, and the write that
    /// followed would land in a free block.
    pub(crate) fn push_temp(&mut self, addr: u64) {
        self.temps.push(addr);
    }

    /// Releases every temporary root taken since `mark`.
    ///
    /// The object is not freed by this; it stops being a root, which is what
    /// a root has to do the moment something else names it. Releasing rather
    /// than leaving them is what keeps this from becoming the retention the
    /// static reference map was careful not to be.
    pub(crate) fn release_temps(&mut self, mark: usize) {
        self.temps.truncate(mark);
    }

    /// Materialises the arguments, calls the host, and writes its answer
    /// back as a word.
    ///
    /// This follows [`crate::interp::Interpreter::call_host`] rather than
    /// inventing an order of its own, because what a host call does is a fact
    /// about the language and not about a backend. The registry is what
    /// charges [`crate::Budget::charge_host_call`], refuses an ungranted
    /// capability, holds the arguments and the answer to the operation's
    /// schema, and writes the `HostCall` trace event; a backend that repeated
    /// any of that would be a second opinion about a question that already has
    /// one. What is left for the machine is the three things only it can do:
    /// read the words out as the `Repr`s of the slots they came from, wait,
    /// and write the answer back.
    ///
    /// The run's own cancellation is not checked here. The oracle checks a
    /// *task's* flag and the flag of every bounded call its thread is inside,
    /// neither of which this machine has yet; the run's flag is read inside
    /// the boundary by `charge_host_call`, which is where it is read on every
    /// backend.
    fn call_host(
        &mut self,
        id: FunctionId,
        base: u64,
        op: HostOpId,
        args: ArgsId,
        budget: &Meter,
        span: Span,
    ) -> Result<u64, RuntimeError> {
        let program = self.program;
        let op = program.host_op(op);
        let function = program.function(id);
        let list = program.arg_list(args);

        let mut values = Vec::with_capacity(list.len());
        for slot in list {
            let repr = function.repr(*slot).ok_or_else(|| undeclared_slot(*slot))?;
            let word = self.mem.slot(base, *slot);
            values.push(boundary::to_value(self, repr, word).map_err(|error| error.at(span))?);
        }

        let hosts = self.hosts.ok_or_else(|| {
            RuntimeError::new(format!(
                "`{}.{}` cannot be called, because this run has no host boundary",
                op.module, op.operation
            ))
            .at(span)
        })?;
        let started = Instant::now();
        let answer = hosts.call_with(&op.module, &op.operation, values, &mut Back { budget });
        self.host_wait += started.elapsed();
        let answer = answer.map_err(|error| error.at(span))?;
        boundary::from_value(self, op.result, &answer).map_err(|error| error.at(span))
    }

    /// Reads the operand words and their `Repr`s and hands them to the
    /// builtin.
    ///
    /// The `Repr`s are read here rather than in [`crate::lvm::builtins`] for
    /// the same reason the boundary takes one: a word is untagged, and what
    /// says what it means is the slot it came out of. That is a fact about
    /// this frame, which the builtin has no business knowing about.
    fn call_builtin(
        &mut self,
        id: FunctionId,
        base: u64,
        builtin: BuiltinId,
        args: ArgsId,
    ) -> Result<u64, RuntimeError> {
        let program = self.program;
        let function = program.function(id);
        let list = program.arg_list(args);
        let mut operands = Vec::with_capacity(list.len());
        for slot in list {
            let repr = function.repr(*slot).ok_or_else(|| undeclared_slot(*slot))?;
            operands.push((repr, self.mem.slot(base, *slot)));
        }
        builtins::call(self, program.builtin(builtin), &operands)
    }

    /// The string object for `text`, allocated the first time it is asked for.
    fn intern(&mut self, text: StrId) -> Result<u64, RuntimeError> {
        if self.interned[text.index()] != 0 {
            return Ok(self.interned[text.index()]);
        }
        let bytes = self.program.string(text).clone();
        let addr = self.allocate(self.program.str_layout, bytes.len() as u32)?;
        self.write_bytes(addr, bytes.as_bytes());
        self.interned[text.index()] = addr;
        Ok(addr)
    }

    /// The layout a [`Inst::Box`] allocates.
    fn boxed_layout(&self) -> LayoutId {
        self.program
            .layouts
            .iter()
            .position(|layout| matches!(layout.shape, Shape::Boxed))
            .map(|at| LayoutId(at as u32))
            .unwrap_or(LayoutId::FREE)
    }

    /// Checks that `addr` is an object with a payload word `at`.
    ///
    /// A reference slot carries no layout, so the object is the only thing
    /// that can say how wide it is. The lowering computed `at` from the type
    /// the checker settled, so this should never refuse — and it is here
    /// because "should never" is not "cannot", and reading past an object
    /// into whatever follows it would be a silent wrong answer rather than a
    /// loud one.
    fn checked(&self, addr: u64, at: u32) -> Result<(), RuntimeError> {
        if addr == 0 {
            return Err(null_object());
        }
        let layout = self.program.layout(self.mem.object_layout(addr));
        let words = layout.payload_words(self.mem.object_len(addr));
        if at >= words {
            return Err(RuntimeError::new(format!(
                "this reads word {at} of a `{}`, which has {words}",
                layout.name
            )));
        }
        Ok(())
    }

    /// Turns a language-level index into a payload offset.
    fn element(&self, addr: u64, at: i64) -> Result<u32, RuntimeError> {
        if addr == 0 {
            return Err(null_object());
        }
        let len = self.mem.object_len(addr) as i64;
        if at < 0 || at >= len {
            return Err(
                RuntimeError::new(format!("index {at} is outside a collection of {len}"))
                    .with_rule("An index outside a collection is a broken invariant."),
            );
        }
        Ok(at as u32)
    }

    /// Orders two string objects by their bytes.
    fn compare_strings(&self, a: u64, b: u64) -> std::cmp::Ordering {
        self.string_bytes(a).cmp(&self.string_bytes(b))
    }

    /// The bytes of the string object at `addr`.
    ///
    /// A null address answers the empty string rather than failing: the one
    /// caller that can see one is the comparison, and two strings one of
    /// which does not exist is a lowering bug the verifier will catch
    /// elsewhere, not something to unwind a comparison for.
    pub(crate) fn string_bytes(&self, addr: u64) -> Vec<u8> {
        if addr == 0 {
            return Vec::new();
        }
        let len = self.mem.object_len(addr) as usize;
        let mut out = Vec::with_capacity(len);
        for at in 0..len.div_ceil(8) {
            let word = self.mem.payload(addr, at as u32);
            for byte in 0..8 {
                if out.len() == len {
                    break;
                }
                out.push((word >> (byte * 8)) as u8);
            }
        }
        out
    }

    /// A new string object holding `text`.
    ///
    /// Unlike [`Machine::intern`] this allocates every time. Interning is for
    /// a literal, which the program named statically and can name again; a
    /// string that arrived from outside has no such name and retaining every
    /// one a host ever answered would be a leak with a table in front of it.
    pub(crate) fn new_string(&mut self, text: &str) -> Result<u64, RuntimeError> {
        let addr = self.allocate(self.program.str_layout, text.len() as u32)?;
        self.write_bytes(addr, text.as_bytes());
        Ok(addr)
    }

    fn write_bytes(&mut self, addr: u64, bytes: &[u8]) {
        for (at, chunk) in bytes.chunks(8).enumerate() {
            let mut word = 0u64;
            for (byte, value) in chunk.iter().enumerate() {
                word |= (*value as u64) << (byte * 8);
            }
            self.mem.set_payload(addr, at as u32, word);
        }
    }

    /// The program this machine runs.
    pub(crate) fn program(&self) -> &'a Program {
        self.program
    }

    /// What the object at `addr` is, for a boundary that has to name it.
    pub(crate) fn object_layout(&self, addr: u64) -> LayoutId {
        self.mem.object_layout(addr)
    }

    /// The length field of the object at `addr`: elements, or a string's
    /// bytes.
    pub(crate) fn object_len(&self, addr: u64) -> u32 {
        self.mem.object_len(addr)
    }

    /// Payload word `at` of the object at `addr`.
    pub(crate) fn payload(&self, addr: u64, at: u32) -> u64 {
        self.mem.payload(addr, at)
    }

    /// Writes payload word `at` of the object at `addr`.
    pub(crate) fn set_payload(&mut self, addr: u64, at: u32, word: u64) {
        self.mem.set_payload(addr, at, word);
    }

    /// A new object of `layout` with header length `len`, collecting once if
    /// the first attempt does not fit.
    ///
    /// The payload is zeroed, so a reference field reads as null until it is
    /// written — which is what makes a half-built object safe to collect
    /// *through* once [`Machine::push_temp`] has made it safe to collect
    /// *around*.
    pub(crate) fn new_object(&mut self, layout: LayoutId, len: u32) -> Result<u64, RuntimeError> {
        self.allocate(layout, len)
    }
}

/// The way back a host is offered while the linear-memory backend runs.
///
/// A host that was handed a Cove callback calls it through this. This machine
/// has no closures yet — [`Inst::CallClosure`] is not implemented — so no
/// callback can reach a host from here and the call arm says what is missing
/// rather than pretending. What the other three answers are worth is not
/// conditional on that: a host that waits reads them to decide whether to keep
/// waiting, and answering "no limit, not cancelled" from a run that has both
/// would be worse than answering nothing.
struct Back<'m> {
    budget: &'m Meter,
}

impl Reentry for Back<'_> {
    fn call(&mut self, callee: &Value, _args: Vec<Value>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::new(format!(
            "this host call cannot run {}, because the linear-memory backend does not run closures yet",
            callee.type_name()
        )))
    }

    fn call_until(
        &mut self,
        callee: &Value,
        args: Vec<Value>,
        _stop: &Cancellation,
    ) -> Result<Value, RuntimeError> {
        self.call(callee, args)
    }

    /// The run's own flag. A task's and a bounded call's belong to a thread,
    /// and this machine runs one body on one thread with neither.
    fn is_cancelled(&self) -> bool {
        self.budget.is_cancelled()
    }

    fn time_left(&self) -> Option<Duration> {
        self.budget
            .limits()
            .deadline
            .map(|deadline| deadline.saturating_sub(self.budget.elapsed()))
    }

    /// There is one task here, and it is the entry's.
    fn task(&self) -> u64 {
        ENTRY_TASK
    }
}

fn compare(op: CmpOp, ordering: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering::*;
    match op {
        CmpOp::Eq => ordering == Equal,
        CmpOp::Ne => ordering != Equal,
        CmpOp::Lt => ordering == Less,
        CmpOp::Le => ordering != Greater,
        CmpOp::Gt => ordering == Greater,
        CmpOp::Ge => ordering != Less,
    }
}

/// `Int` arithmetic, with the language's messages.
///
/// The messages are the interpreter's, word for word, because overflow and
/// division by zero are rules of the language rather than of a backend. The
/// differential corpus compares them.
fn int_arith(op: ArithOp, a: i64, b: i64, duration: bool) -> Result<i64, RuntimeError> {
    let named = |what: &'static str| -> &'static str {
        if duration {
            "duration arithmetic"
        } else {
            what
        }
    };
    match op {
        ArithOp::Add => a
            .checked_add(b)
            .ok_or_else(|| overflowed(named("addition"))),
        ArithOp::Sub => a
            .checked_sub(b)
            .ok_or_else(|| overflowed(named("subtraction"))),
        ArithOp::Mul => a
            .checked_mul(b)
            .ok_or_else(|| overflowed(named("multiplication"))),
        ArithOp::Div => {
            if b == 0 {
                Err(divided_by_zero("division"))
            } else {
                a.checked_div(b).ok_or_else(|| overflowed("division"))
            }
        }
        ArithOp::Rem => {
            if b == 0 {
                Err(divided_by_zero("remainder"))
            } else {
                a.checked_rem(b).ok_or_else(|| overflowed("remainder"))
            }
        }
    }
}

fn float_arith(op: ArithOp, a: f64, b: f64) -> f64 {
    match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div => a / b,
        ArithOp::Rem => a % b,
    }
}

fn overflowed(operation: &str) -> RuntimeError {
    RuntimeError::new(format!("`Int` {operation} overflowed"))
        .with_rule("Integer overflow is a broken invariant, not a wrapped result.")
}

fn divided_by_zero(operation: &str) -> RuntimeError {
    RuntimeError::new(format!("`Int` {operation} by zero"))
        .with_rule("Division and remainder by zero are broken invariants.")
}

/// A call named a slot the function it is in does not have.
///
/// The verifier checks every slot an instruction names against the frame it
/// names it in, so this is a lowering bug that got past it rather than
/// anything a program can do. It is reported instead of assumed because the
/// alternative is reading a word that belongs to the next frame.
fn undeclared_slot(slot: Slot) -> RuntimeError {
    RuntimeError::new(format!(
        "this call names slot {slot}, which this frame has not"
    ))
}

/// A reference slot held null where an object was needed.
///
/// This is not a language-level `nil`: Cove has none. It is a lowering bug
/// reaching the machine, reported rather than read through.
fn null_object() -> RuntimeError {
    RuntimeError::new("this value was read before it was given one")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use cove_lir::{ArgsId, Capture, Function, Layout, RefMap, Table, TableId};
    use std::sync::Arc;

    /// Builds a program by hand.
    ///
    /// The lowering is a separate piece and a separate test suite. What is
    /// under test here is the machine, so its programs are written in the IR
    /// directly: a failure is then unambiguously the loop's, and a change to
    /// the lowering cannot quietly stop exercising an instruction.
    ///
    /// `pub(crate)` so that the boundary's and the builtins' tests write
    /// their fixtures the same way. A hand-written program is the only kind
    /// any of them uses, and having one builder is what keeps a fixture from
    /// being the thing under test.
    #[derive(Default)]
    pub(crate) struct Build {
        pub(crate) program: Program,
    }

    impl Build {
        pub(crate) fn strings(mut self, texts: &[&str]) -> Build {
            self.program.strings = texts.iter().map(|text| Arc::from(*text)).collect();
            self
        }

        pub(crate) fn layout(&mut self, name: &str, shape: Shape) -> LayoutId {
            if self.program.layouts.is_empty() {
                self.program.layouts.push(Layout::free());
            }
            self.program.layouts.push(Layout {
                name: Arc::from(name),
                shape,
            });
            LayoutId(self.program.layouts.len() as u32 - 1)
        }

        pub(crate) fn args(&mut self, slots: &[Slot]) -> ArgsId {
            self.program.args.push(slots.to_vec());
            ArgsId(self.program.args.len() as u32 - 1)
        }

        pub(crate) fn table(&mut self, targets: &[u32], default: u32) -> TableId {
            self.program.tables.push(Table {
                targets: targets.to_vec(),
                default,
            });
            TableId(self.program.tables.len() as u32 - 1)
        }

        pub(crate) fn function(
            &mut self,
            name: &str,
            arity: u32,
            reprs: &[Repr],
            returns: Repr,
            code: Vec<Inst>,
        ) -> FunctionId {
            let nowhere = Span::new(cove_diag::FileId(0), 0, 0);
            let spans = vec![nowhere; code.len()];
            self.program.functions.push(Function {
                module: Arc::from("t"),
                name: Arc::from(name),
                arity,
                reprs: reprs.to_vec(),
                refs: RefMap::of(reprs),
                returns,
                captures: Vec::<Capture>::new(),
                code,
                spans,
                span: nowhere,
                is_async: false,
            });
            let id = FunctionId(self.program.functions.len() as u32 - 1);
            self.program
                .by_name
                .insert((Arc::from("t"), Arc::from(name)), id);
            id
        }

        /// Checks the program the way the lowering must, so a malformed test
        /// fixture fails as a fixture rather than as a machine bug.
        pub(crate) fn done(self) -> Program {
            cove_lir::verify(&self.program).expect("a hand-written test program is well formed");
            self.program
        }

        /// A program of layouts and strings and no functions.
        ///
        /// What a boundary or a builtin test needs: both of them convert or
        /// read values rather than running code, and a function written only
        /// so that a program has one would be a fixture nothing reads.
        pub(crate) fn bare(mut self) -> Program {
            let str_layout = self.layout("String", Shape::Str);
            self.program.str_layout = str_layout;
            self.done()
        }
    }

    pub(crate) fn budget() -> Meter {
        crate::budget::Budget::new(crate::budget::Limits::default()).meter()
    }

    fn run(program: &Program, entry: FunctionId, args: &[u64]) -> Result<u64, RuntimeError> {
        Machine::new(program, 1 << 16).run(entry, args, &budget())
    }

    #[test]
    fn a_constant_comes_back() {
        let mut build = Build::default();
        let f = build.function(
            "answer",
            0,
            &[Repr::Int],
            Repr::Int,
            vec![Inst::Int { dst: 0, value: 42 }, Inst::Return { src: 0 }],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[]).unwrap() as i64, 42);
    }

    #[test]
    fn arithmetic_reads_and_writes_slots() {
        let mut build = Build::default();
        let f = build.function(
            "add",
            2,
            &[Repr::Int, Repr::Int, Repr::Int],
            Repr::Int,
            vec![
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[3, 4]).unwrap() as i64, 7);
    }

    /// The messages are the language's, not the backend's; the differential
    /// corpus compares them against the tree-walking oracle word for word.
    #[test]
    fn arithmetic_faults_say_what_the_oracle_says() {
        let cases: Vec<(ArithOp, i64, i64, &str)> = vec![
            (ArithOp::Div, 1, 0, "`Int` division by zero"),
            (ArithOp::Rem, 1, 0, "`Int` remainder by zero"),
            (ArithOp::Add, i64::MAX, 1, "`Int` addition overflowed"),
            (ArithOp::Mul, i64::MAX, 2, "`Int` multiplication overflowed"),
        ];
        for (op, a, b, message) in cases {
            let mut build = Build::default();
            let f = build.function(
                "fault",
                2,
                &[Repr::Int, Repr::Int, Repr::Int],
                Repr::Int,
                vec![
                    Inst::Arith {
                        num: Num::Int,
                        op,
                        dst: 2,
                        a: 0,
                        b: 1,
                    },
                    Inst::Return { src: 2 },
                ],
            );
            let program = build.done();
            let error = run(&program, f, &[a as u64, b as u64]).unwrap_err();
            assert_eq!(error.message, message);
        }
    }

    /// A `Duration` is nanoseconds and its arithmetic is an integer's, so the
    /// only thing that changes is what an overflow is called.
    #[test]
    fn a_duration_overflow_is_named_a_duration_overflow() {
        let mut build = Build::default();
        let f = build.function(
            "late",
            2,
            &[Repr::Duration, Repr::Duration, Repr::Duration],
            Repr::Duration,
            vec![
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        let error = run(&program, f, &[i64::MAX as u64, 1]).unwrap_err();
        assert_eq!(error.message, "`Int` duration arithmetic overflowed");
    }

    #[test]
    fn a_branch_takes_one_side() {
        let mut build = Build::default();
        // fn abs(n) { if n < 0 { -n } else { n } }
        let f = build.function(
            "abs",
            1,
            &[Repr::Int, Repr::Int, Repr::Bool],
            Repr::Int,
            vec![
                Inst::Int { dst: 1, value: 0 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::BranchFalse { cond: 2, to: 4 },
                Inst::Neg {
                    num: Num::Int,
                    dst: 0,
                    a: 0,
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[(-5i64) as u64]).unwrap() as i64, 5);
        assert_eq!(run(&program, f, &[5]).unwrap() as i64, 5);
    }

    #[test]
    fn a_loop_runs_to_its_bound() {
        let mut build = Build::default();
        // fn sum(n) { var t = 0; var i = 0; while i < n { t = t + i; i = i + 1 }; t }
        let f = build.function(
            "sum",
            1,
            &[Repr::Int, Repr::Int, Repr::Int, Repr::Bool, Repr::Int],
            Repr::Int,
            vec![
                Inst::Int { dst: 1, value: 0 },
                Inst::Int { dst: 2, value: 0 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 3,
                    a: 2,
                    b: 0,
                },
                Inst::BranchFalse { cond: 3, to: 8 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 1,
                    a: 1,
                    b: 2,
                },
                Inst::Int { dst: 4, value: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 2,
                    a: 2,
                    b: 4,
                },
                Inst::Jump { to: 2 },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[10]).unwrap() as i64, 45);
    }

    /// A call writes its arguments straight into the callee's slots and its
    /// answer straight into the caller's destination. There is no buffer
    /// between the two frames, and this is what says so.
    #[test]
    fn recursion_nests_frames_and_unwinds_them() {
        let mut build = Build::default();
        let args = build.args(&[3]);
        // fn fact(n) { if n <= 1 { 1 } else { n * fact(n - 1) } }
        let f = build.function(
            "fact",
            1,
            &[Repr::Int, Repr::Int, Repr::Bool, Repr::Int, Repr::Int],
            Repr::Int,
            vec![
                Inst::Int { dst: 1, value: 1 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Le,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::BranchFalse { cond: 2, to: 4 },
                Inst::Return { src: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Sub,
                    dst: 3,
                    a: 0,
                    b: 1,
                },
                Inst::Call {
                    dst: 4,
                    callee: FunctionId(0),
                    args,
                },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Mul,
                    dst: 4,
                    a: 0,
                    b: 4,
                },
                Inst::Return { src: 4 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[10]).unwrap() as i64, 3_628_800);
    }

    /// Depth is bounded by the reserved stack region, not by the Rust stack:
    /// a call does not recurse in the dispatch loop, so this returns an error
    /// rather than ending the process.
    #[test]
    fn an_unbounded_recursion_is_stopped() {
        let mut build = Build::default();
        let args = build.args(&[0]);
        let f = build.function(
            "forever",
            1,
            &[Repr::Int, Repr::Int],
            Repr::Int,
            vec![
                Inst::Call {
                    dst: 1,
                    callee: FunctionId(0),
                    args,
                },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let error = run(&program, f, &[0]).unwrap_err();
        assert_eq!(error.message, "this call nests too deeply");
    }

    /// `bump(var total)` adds to the caller's own binding rather than to a
    /// copy of it. A place is one word holding the address of that binding,
    /// and this is the whole of the mechanism.
    #[test]
    fn a_place_writes_the_callers_own_slot() {
        let mut build = Build::default();
        let args = build.args(&[1]);
        let bump = build.function(
            "bump",
            1,
            &[Repr::Addr, Repr::Int, Repr::Int, Repr::Unit],
            Repr::Unit,
            vec![
                Inst::Load { dst: 1, addr: 0 },
                Inst::Int { dst: 2, value: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 1,
                    a: 1,
                    b: 2,
                },
                Inst::Store { addr: 0, src: 1 },
                Inst::Unit { dst: 3 },
                Inst::Return { src: 3 },
            ],
        );
        let caller = build.function(
            "main",
            0,
            &[Repr::Int, Repr::Addr, Repr::Unit],
            Repr::Int,
            vec![
                Inst::Int { dst: 0, value: 10 },
                Inst::AddrOfSlot { dst: 1, slot: 0 },
                Inst::Call {
                    dst: 2,
                    callee: bump,
                    args,
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, caller, &[]).unwrap() as i64, 11);
    }

    #[test]
    fn an_object_round_trips_through_its_fields() {
        let mut build = Build::default();
        let point = build.layout(
            "Point",
            Shape::Struct {
                fields: vec![
                    cove_lir::Field {
                        name: Arc::from("x"),
                        repr: Repr::Int,
                    },
                    cove_lir::Field {
                        name: Arc::from("y"),
                        repr: Repr::Int,
                    },
                ],
                opaque: false,
            },
        );
        let f = build.function(
            "make",
            0,
            &[Repr::Ref, Repr::Int, Repr::Int],
            Repr::Int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: point,
                    len: Len::Fixed,
                },
                Inst::Int { dst: 1, value: 3 },
                Inst::SetWord {
                    obj: 0,
                    at: 0,
                    src: 1,
                },
                Inst::Int { dst: 1, value: 4 },
                Inst::SetWord {
                    obj: 0,
                    at: 1,
                    src: 1,
                },
                Inst::GetWord {
                    dst: 1,
                    obj: 0,
                    at: 0,
                },
                Inst::GetWord {
                    dst: 2,
                    obj: 0,
                    at: 1,
                },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Mul,
                    dst: 1,
                    a: 1,
                    b: 2,
                },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[]).unwrap() as i64, 12);
    }

    /// Reading past an object is a lowering bug, and the machine reports it
    /// rather than reading whatever follows the object in the heap.
    #[test]
    fn a_field_past_the_object_is_refused() {
        let mut build = Build::default();
        let one = build.layout(
            "One",
            Shape::Struct {
                fields: vec![cove_lir::Field {
                    name: Arc::from("x"),
                    repr: Repr::Int,
                }],
                opaque: false,
            },
        );
        let f = build.function(
            "past",
            0,
            &[Repr::Ref, Repr::Int],
            Repr::Int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: one,
                    len: Len::Fixed,
                },
                Inst::GetWord {
                    dst: 1,
                    obj: 0,
                    at: 3,
                },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let error = run(&program, f, &[]).unwrap_err();
        assert!(
            error.message.contains("word 3 of a `One`"),
            "{}",
            error.message
        );
    }

    /// The loop allocates in a loop, clearing the slot each turn. Without
    /// `Clear` the frame would hold every string it ever made; with it the
    /// heap stays flat, and this is the test that says so.
    #[test]
    fn clearing_a_slot_lets_the_collector_reclaim() {
        let mut build = Build::default();
        let cell = build.layout(
            "Cell",
            Shape::Elements {
                elem: Repr::Int,
                growable: false,
            },
        );
        let f = build.function(
            "churn",
            1,
            &[Repr::Int, Repr::Int, Repr::Bool, Repr::Ref, Repr::Int],
            Repr::Int,
            vec![
                Inst::Int { dst: 1, value: 0 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 2,
                    a: 1,
                    b: 0,
                },
                Inst::BranchFalse { cond: 2, to: 8 },
                Inst::Alloc {
                    dst: 3,
                    layout: cell,
                    len: Len::Count(64),
                },
                Inst::Clear { slot: 3 },
                Inst::Int { dst: 4, value: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 1,
                    a: 1,
                    b: 4,
                },
                Inst::Jump { to: 1 },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        // A heap far smaller than 4000 objects of 65 words: the run only
        // finishes because each turn's object is unreachable by the next.
        let mut machine = Machine::new(&program, 4096);
        let answer = machine.run(f, &[4000], &budget()).unwrap();
        assert_eq!(answer as i64, 4000);
        assert!(
            machine.collected().collections > 0,
            "the run should have had to collect"
        );
    }

    #[test]
    fn a_string_literal_is_allocated_once() {
        let mut build = Build::default().strings(&["hello"]);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let f = build.function(
            "twice",
            0,
            &[Repr::Ref, Repr::Ref, Repr::Bool],
            Repr::Bool,
            vec![
                Inst::Str {
                    dst: 0,
                    text: StrId(0),
                },
                Inst::Str {
                    dst: 1,
                    text: StrId(0),
                },
                Inst::Cmp {
                    on: Compare::Identity,
                    op: CmpOp::Eq,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[]).unwrap(), 1);
    }

    #[test]
    fn strings_compare_by_their_bytes() {
        let mut build = Build::default().strings(&["apple", "banana"]);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let f = build.function(
            "order",
            0,
            &[Repr::Ref, Repr::Ref, Repr::Bool],
            Repr::Bool,
            vec![
                Inst::Str {
                    dst: 0,
                    text: StrId(0),
                },
                Inst::Str {
                    dst: 1,
                    text: StrId(1),
                },
                Inst::Cmp {
                    on: Compare::Str,
                    op: CmpOp::Lt,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[]).unwrap(), 1);
    }

    #[test]
    fn a_box_answers_its_tag_and_refuses_another() {
        let mut build = Build::default();
        let boxed = build.layout("Any", Shape::Boxed);
        let _ = boxed;
        let good = build.function(
            "round-trip",
            1,
            &[Repr::Int, Repr::Ref, Repr::Int],
            Repr::Int,
            vec![
                Inst::Box {
                    dst: 1,
                    src: 0,
                    repr: Repr::Int,
                },
                Inst::Unbox {
                    dst: 2,
                    src: 1,
                    repr: Repr::Int,
                },
                Inst::Return { src: 2 },
            ],
        );
        let wrong = build.function(
            "wrong-type",
            1,
            &[Repr::Int, Repr::Ref, Repr::Bool],
            Repr::Bool,
            vec![
                Inst::Box {
                    dst: 1,
                    src: 0,
                    repr: Repr::Int,
                },
                Inst::Unbox {
                    dst: 2,
                    src: 1,
                    repr: Repr::Bool,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, good, &[7]).unwrap() as i64, 7);
        assert!(run(&program, wrong, &[7]).is_err());
    }

    #[test]
    fn a_switch_picks_a_case_and_falls_to_its_default() {
        let mut build = Build::default();
        let table = build.table(&[3, 5], 7);
        let f = build.function(
            "pick",
            1,
            &[Repr::Int, Repr::Int],
            Repr::Int,
            vec![
                Inst::Switch { on: 0, table },
                Inst::Int { dst: 1, value: 0 },
                Inst::Return { src: 1 },
                Inst::Int { dst: 1, value: 10 },
                Inst::Return { src: 1 },
                Inst::Int { dst: 1, value: 20 },
                Inst::Return { src: 1 },
                Inst::Int { dst: 1, value: 30 },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[0]).unwrap() as i64, 10);
        assert_eq!(run(&program, f, &[1]).unwrap() as i64, 20);
        assert_eq!(run(&program, f, &[9]).unwrap() as i64, 30);
    }

    // ---- the host boundary -------------------------------------------

    /// A host with one operation of each kind of argument the boundary has
    /// to move: a scalar in and out, and a string in and out.
    ///
    /// Written here rather than reused from a shipped module because what is
    /// under test is the *instruction*: `console.println` would drag in a
    /// grant table, an output stream and a schema written for a different
    /// purpose, and a failure would take a paragraph to attribute.
    struct Probe;

    static PROBE_OPS: &[cove_schema::OperationSchema] = &[
        cove_schema::OperationSchema {
            name: "double",
            params: &[cove_schema::HostType::Int],
            variadic: false,
            result: cove_schema::HostType::Int,
            capability: "probe",
            effect: cove_schema::Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        cove_schema::OperationSchema {
            name: "shout",
            params: &[cove_schema::HostType::String],
            variadic: false,
            result: cove_schema::HostType::String,
            capability: "probe",
            effect: cove_schema::Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ];

    impl crate::host::HostApi for Probe {
        fn module_schema(&self) -> cove_schema::ModuleSchema {
            cove_schema::ModuleSchema {
                name: "probe",
                capability: "probe",
                operations: PROBE_OPS,
                types: &[],
                resources: &[],
            }
        }

        fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
            match op {
                "double" => Ok(Value::int(
                    args[0].as_int().expect("the schema holds it") * 2,
                )),
                "shout" => Ok(Value::string(format!(
                    "{}!",
                    args[0].as_str().expect("the schema holds it")
                ))),
                other => Err(RuntimeError::new(format!("no `{other}` here"))),
            }
        }
    }

    fn probing(granted: bool) -> crate::host::HostRegistry {
        let grants = if granted {
            crate::host::Grants::new(["probe"])
        } else {
            crate::host::Grants::new(Vec::<String>::new())
        };
        let mut hosts = crate::host::HostRegistry::new(grants);
        hosts.register(Box::new(Probe));
        hosts
    }

    /// `fn f(n) { probe.double(n) }`, in the IR.
    fn calls_double(build: &mut Build) -> FunctionId {
        build.program.host_ops.push(cove_lir::HostOp {
            module: Arc::from("probe"),
            operation: Arc::from("double"),
            result: Repr::Int,
        });
        let op = cove_lir::HostOpId(build.program.host_ops.len() as u32 - 1);
        let args = build.args(&[0]);
        build.function(
            "f",
            1,
            &[Repr::Int, Repr::Int],
            Repr::Int,
            vec![Inst::CallHost { dst: 1, op, args }, Inst::Return { src: 1 }],
        )
    }

    #[test]
    fn a_host_call_moves_a_word_out_and_the_answer_back() {
        let mut build = Build::default();
        let f = calls_double(&mut build);
        let program = build.done();
        let hosts = probing(true);
        let mut machine = Machine::with_hosts(&program, 1 << 12, Some(&hosts));
        assert_eq!(machine.run(f, &[21], &budget()).unwrap() as i64, 42);
    }

    /// A string argument and a string answer, which is the case that
    /// allocates on both sides of the boundary.
    #[test]
    fn a_host_call_carries_strings_in_and_out() {
        let mut build = Build::default().strings(&["hey"]);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        build.program.host_ops.push(cove_lir::HostOp {
            module: Arc::from("probe"),
            operation: Arc::from("shout"),
            result: Repr::Ref,
        });
        let op = cove_lir::HostOpId(0);
        let args = build.args(&[0]);
        let f = build.function(
            "f",
            0,
            &[Repr::Ref, Repr::Ref],
            Repr::Ref,
            vec![
                Inst::Str {
                    dst: 0,
                    text: StrId(0),
                },
                Inst::CallHost { dst: 1, op, args },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let hosts = probing(true);
        let mut machine = Machine::with_hosts(&program, 1 << 12, Some(&hosts));
        let word = machine.run(f, &[], &budget()).unwrap();
        assert_eq!(
            String::from_utf8(machine.string_bytes(word)).unwrap(),
            "hey!"
        );
    }

    /// The boundary refuses an ungranted capability, and it is the boundary
    /// that does it: the machine passes the call on and reports what came
    /// back, classification included.
    #[test]
    fn an_ungranted_call_is_refused_at_the_boundary() {
        let mut build = Build::default();
        let f = calls_double(&mut build);
        let program = build.done();
        let hosts = probing(false);
        let mut machine = Machine::with_hosts(&program, 1 << 12, Some(&hosts));
        let error = machine.run(f, &[1], &budget()).unwrap_err();
        assert!(
            error.message.contains("probe"),
            "the refusal names the capability: {}",
            error.message
        );
        assert_eq!(error.denied_capability.as_deref(), Some("probe"));
        assert_eq!(error.outcome, crate::trace::RunOutcome::HostBoundary);
    }

    /// The host-call limit is charged inside the boundary, which is where the
    /// oracle charges it too — `Budget::charge_host_call`, once per call,
    /// before the host is reached.
    #[test]
    fn a_host_call_is_charged_the_way_the_oracle_charges_it() {
        let mut build = Build::default();
        build.program.host_ops.push(cove_lir::HostOp {
            module: Arc::from("probe"),
            operation: Arc::from("double"),
            result: Repr::Int,
        });
        let op = cove_lir::HostOpId(0);
        let args = build.args(&[0]);
        let f = build.function(
            "f",
            1,
            &[Repr::Int, Repr::Int],
            Repr::Int,
            vec![
                Inst::CallHost { dst: 1, op, args },
                Inst::CallHost { dst: 1, op, args },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let mut hosts = probing(true);
        let limits = crate::budget::Limits {
            max_host_calls: Some(1),
            ..Default::default()
        };
        let budget = crate::budget::Budget::new(limits);
        let meter = budget.meter();
        hosts.set_budget(budget);
        let mut machine = Machine::with_hosts(&program, 1 << 12, Some(&hosts));
        let error = machine.run(f, &[2], &meter).unwrap_err();
        assert_eq!(
            error.message,
            "execution stopped: host-call limit of 1 exceeded"
        );
        // Two, not one: the boundary counts the call it is about to make
        // and then refuses it for being past the limit. That is the shared
        // counter doing what it does for every backend, which is the point —
        // nothing here keeps a count of its own.
        assert_eq!(hosts.with_budget(|budget| budget.host_calls()), Some(2));
    }

    /// A machine with no host behind it says what is missing rather than
    /// answering as if the call had happened.
    #[test]
    fn a_host_call_with_no_boundary_says_what_is_missing() {
        let mut build = Build::default();
        let f = calls_double(&mut build);
        let program = build.done();
        let mut machine = Machine::new(&program, 1 << 12);
        let error = machine.run(f, &[1], &budget()).unwrap_err();
        assert_eq!(
            error.message,
            "`probe.double` cannot be called, because this run has no host boundary"
        );
    }

    /// A run that will not stop on its own is stopped by its budget, and the
    /// stride is what bounds how long that takes.
    #[test]
    fn a_cancelled_run_stops_at_a_safepoint() {
        let mut build = Build::default();
        let f = build.function(
            "spin",
            0,
            &[Repr::Int],
            Repr::Int,
            vec![Inst::Int { dst: 0, value: 0 }, Inst::Jump { to: 0 }],
        );
        let program = build.done();
        let cancellation = Cancellation::new();
        let budget = crate::budget::Budget::with_cancellation(
            crate::budget::Limits::default(),
            cancellation.clone(),
        );
        cancellation.cancel();
        let mut machine = Machine::new(&program, 1 << 12);
        assert!(machine.run(f, &[], &budget.meter()).is_err());
        assert!(machine.instructions() <= SAFEPOINT_STRIDE + 1);
    }
}
