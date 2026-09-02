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

use std::sync::Arc;
use std::time::{Duration, Instant};

use cove_diag::Span;
use cove_lir::{
    ArgsId, ArithOp, BuiltinId, CmpOp, Compare, Convert, FunctionId, HostOpId, Inst, LayoutId, Len,
    Num, Program, Repr, Shape, Slot, StrId,
};

use crate::budget::{Cancellation, Meter};
use crate::error::RuntimeError;
use crate::host::{HostRegistry, Reentry, ResourceHandle};
use crate::lvm::builtins::operand::Operand;
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
    /// The host resources this run has been handed, in the table a
    /// [`Repr::Host`] word indexes.
    ///
    /// [`Repr::Host`]'s own documentation is what fixes the shape — *an index
    /// into the run's host resource table* — and this is that table. It is
    /// here and not in the heap because
    /// [ADR 0031](../../../../docs/adr/0031-a-host-handle-is-not-a-vm-handle.md)
    /// draws exactly this line: a host resource handle is a name the *host*
    /// minted for something the host owns, and a heap object is a reference
    /// into storage this run allocated. Only the second is the VM's. Making a
    /// resource an object in the traced heap would put a collection in charge
    /// of a lifetime [ADR 0013](../../../../docs/adr/0013-host-resource-handles.md)
    /// gives to the host, and would mean sweeping something whose `close` the
    /// program had not written.
    ///
    /// It is not a second value store either, which is the other thing
    /// ADR 0034 and ADR 0031 forbid. Nothing a Cove program can write down
    /// may be put in it: an entry is an [`Arc`] of a [`ResourceHandle`] — a
    /// module, a type name, a number and a flag — and the only two operations
    /// over it are [`Machine::resource`] and [`Machine::resource_word`],
    /// neither of which can be handed a Cove value. A value that wanted to
    /// avoid having a heap representation could not hide here.
    ///
    /// **The word is one past the index, so zero is no resource.** Frames are
    /// zeroed on entry, so a `Host` slot that has not been written yet reads
    /// zero exactly as a `Ref` slot reads null; a table indexed straight by
    /// the word would answer an unwritten slot with whichever resource
    /// happened to be first.
    ///
    /// Nothing is ever removed. ADR 0013 says a closed resource's handle
    /// survives as a name for something that is gone, and that a host never
    /// reuses an identity — so an entry that outlived its resource is still
    /// the right answer to give, because the refusal a later call earns is
    /// the *host's* and can only be reached by handing the host the name.
    /// What that costs is one name per distinct resource this run was handed,
    /// which is the size of the table the host is keeping anyway.
    ///
    /// It is a field of the machine, which today is the run: a run has one
    /// machine as it has one [`Memory`]. When a run has task threads this
    /// moves where the object heap moves, and for the reason ADR 0013 gives
    /// rather than by analogy — a resource is owned by the *run*, not by the
    /// task or the scope that opened it, so a handle one task was given is a
    /// name every task of the run may hold.
    resources: Vec<Arc<ResourceHandle>>,
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
    /// Where the most recent assertion failed, and the message it produced.
    ///
    /// Written only by [`Inst::AssertFailed`], which the failing arm of a
    /// lowered assertion carries. A failed assertion is an ordinary `Err`
    /// from here on — the machine does not stop, and a program that handles
    /// it goes on running — so this is a record of what was seen and not a
    /// state the run is in. A test runner reads it to point at the assertion
    /// rather than at the test, and keeps the message so that it can tell
    /// the `Err` it is holding from a later, unrelated one.
    assertion_failure: Option<(Span, String)>,
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
            resources: Vec::new(),
            temps: Vec::new(),
            roots: Vec::new(),
            instructions: 0,
            host_wait: Duration::ZERO,
            collected: Collected::default(),
            assertion_failure: None,
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

    /// Where the most recent failed assertion was written, and the message
    /// it produced, or `None` when none has failed.
    pub(crate) fn assertion_failure(&self) -> Option<(Span, &str)> {
        self.assertion_failure
            .as_ref()
            .map(|(span, message)| (*span, message.as_str()))
    }

    /// Runs `entry` with `args` already in word form, answering the words of
    /// its result.
    ///
    /// The caller converts: this is below the boundary, and nothing here
    /// knows what a public `Value` is. `args` is the parameters' words
    /// flattened in declaration order — a `(Int, Point, Int)` list is four
    /// words — because that is what the frame they are written into is.
    pub(crate) fn run(
        &mut self,
        entry: FunctionId,
        args: &[u64],
        budget: &Meter,
    ) -> Result<Vec<u64>, RuntimeError> {
        let program = self.program;
        let function = program.function(entry);
        debug_assert_eq!(
            args.len(),
            function.param_words(&program.layouts) as usize,
            "an entry is called with its parameters' words"
        );

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
    fn dispatch(&mut self, budget: &Meter) -> Result<Vec<u64>, RuntimeError> {
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
                // ADR 0001's field-wise shallow copy, and the whole of it.
                // A value's words are where the value is, so copying one is
                // copying its run of words: a `Wrapper { p: Point, v: Vector }`
                // copies three, the `Point` becomes independent and the
                // `Vector` stays shared, and neither answer needed a policy.
                Inst::Copy { dst, src, layout } => {
                    let width = self.width(layout);
                    self.mem
                        .copy_words(base + dst as u64, base + src as u64, width);
                }
                // The one instruction whose whole purpose is what it stops
                // happening: a reference the frame no longer needs is not a
                // root, so the object it named is unreachable now rather
                // than when this frame returns.
                Inst::Clear { slot, layout } => {
                    let width = self.width(layout);
                    self.mem.clear_words(base + slot as u64, width);
                }

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
                // The words `Function::returns` describes, copied into the
                // caller's destination *location* — which is a base slot and
                // a width, like every other value location. The copy happens
                // before the frame is dropped, because the words are in it.
                Inst::Return { src } => {
                    let width = self.width(program.function(id).returns);
                    // The frame being left is what says where its answer
                    // goes. Keeping the destination with the callee rather
                    // than re-reading the caller's `Call` means a return
                    // touches one instruction, not two.
                    let done = self.frames.pop().expect("a frame is executing");
                    match self.frames.last() {
                        None => {
                            let answer = self.mem.read_words(base + src as u64, width);
                            self.mem.pop_frame(base);
                            return Ok(answer);
                        }
                        Some(caller) => {
                            id = caller.function;
                            let caller_base = caller.base;
                            pc = caller.pc as usize;
                            code = &program.function(id).code[..];
                            self.mem.copy_words(
                                caller_base + done.dst as u64,
                                base + src as u64,
                                width,
                            );
                            self.mem.pop_frame(base);
                            base = caller_base;
                        }
                    }
                }

                // ---- calls -------------------------------------------------
                Inst::Call { dst, callee, args } => {
                    let target = program.function(callee);
                    let list = program.arg_list(args);
                    if list.len() != target.params.len() {
                        fail!(wrong_arity(
                            target.qualified(),
                            target.params.len(),
                            list.len()
                        ));
                    }
                    let callee_base = match self.mem.push_frame(target.frame_size()) {
                        Ok(base) => base,
                        Err(Overflow) => fail!(self.too_deep_error()),
                    };
                    // Parameters occupy the callee's frame from slot 0 in
                    // declaration order, each taking the words its layout
                    // says. There is no argument buffer and no permutation
                    // into type groups: the callee's frame begins where this
                    // one ends, and the words are copied straight into it.
                    // The width is the *parameter's*, not the argument's,
                    // although an argument now carries a layout of its own
                    // and the verifier holds the two to be the same one. The
                    // frame being written is the callee's, and
                    // `Function::params` is the only fact about the callee
                    // here: a `CallClosure` does not know which function it
                    // is entering until it has read the object, so nothing
                    // static could be authoritative there, and one rule for
                    // both is worth more than the symmetry.
                    let mut at = 0;
                    for (arg, layout) in list.iter().zip(&target.params) {
                        let width = self.width(*layout);
                        self.mem
                            .copy_words(callee_base + at as u64, base + arg.slot as u64, width);
                        at += width;
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
                // A closure call is a frame like any other, and that is the
                // whole of it. The callee is not in the instruction — it is a
                // word of the object the slot names — and the captures follow
                // the arguments into the slots `Function::captures` names.
                // Nothing else differs from [`Inst::Call`]: no Rust frame is
                // added, so a `map` over a `map` over a `map` nests in the
                // reserved stack region and nowhere else, which is the
                // property `docs/LINEAR_VM.md` asks a closure-taking sequence
                // method to lower to a loop in order to keep.
                Inst::CallClosure { dst, closure, args } => {
                    let object = self.mem.slot(base, closure);
                    let callee = match self.callee_of(object) {
                        Ok(callee) => callee,
                        Err(error) => fail!(error),
                    };
                    let target = program.function(callee);
                    let list = program.arg_list(args);
                    if list.len() != target.params.len() {
                        fail!(wrong_arity(
                            target.qualified(),
                            target.params.len(),
                            list.len()
                        ));
                    }
                    let callee_base = match self.mem.push_frame(target.frame_size()) {
                        Ok(base) => base,
                        Err(Overflow) => fail!(self.too_deep_error()),
                    };
                    let mut at = 0;
                    for (arg, layout) in list.iter().zip(&target.params) {
                        let width = self.width(*layout);
                        self.mem
                            .copy_words(callee_base + at as u64, base + arg.slot as u64, width);
                        at += width;
                    }
                    // The object has to stay reachable across every one of
                    // these reads, and it does, for a reason rather than by
                    // luck: it is named by slot `closure` of a frame this has
                    // not left, the verifier holds that slot to `Repr::Ref`,
                    // and a `Repr::Ref` slot of a live frame is a root. So no
                    // temporary root is taken here — and nothing between the
                    // read and the last write allocates, so no collection can
                    // happen in the window at all.
                    //
                    // `Capture::slot` is read rather than re-derived from
                    // `arity + at`. The two agree, and the verifier is what
                    // says so: it refuses a capture naming a slot outside the
                    // frame or holding a different `Repr`.
                    //
                    // A capture is stored inline in the environment, each at
                    // its own width, so where one begins is the widths of the
                    // ones before it — the same arrangement the parameters
                    // are under, in a payload instead of a frame.
                    let mut held = 1;
                    for capture in &target.captures {
                        let width = self.width(capture.layout);
                        self.mem.copy_words(
                            callee_base + capture.slot as u64,
                            self.mem.payload_addr(object, held),
                            width,
                        );
                        held += width;
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
                // The one instruction that leaves the machine. Everything it
                // needs to read out of the frame is read before the call, so
                // that the frames are consistent for the length of it: a host
                // may collect through the boundary, and a boundary that had
                // been handed a stale program counter would walk this frame
                // to a slot the loop had already moved past.
                Inst::CallHost { dst, op, args } => {
                    self.sync(pc - 1);
                    let span = self.span(id, pc - 1);
                    match self.call_host(base, op, args, budget, span) {
                        Ok(words) => {
                            for (at, word) in words.iter().enumerate() {
                                self.mem.set_slot(base, dst + at as u32, *word);
                            }
                        }
                        Err(error) => fail!(error),
                    }
                }
                // The same boundary, addressed to a handle rather than to a
                // module. ADR 0013 gives the host the only record of what is
                // open, so which resource answers is the word in `receiver`
                // and nothing static.
                Inst::CallResource {
                    dst,
                    receiver,
                    op,
                    args,
                } => {
                    self.sync(pc - 1);
                    let span = self.span(id, pc - 1);
                    match self.call_resource(base, receiver, op, args, budget, span) {
                        Ok(words) => {
                            for (at, word) in words.iter().enumerate() {
                                self.mem.set_slot(base, dst + at as u32, *word);
                            }
                        }
                        Err(error) => fail!(error),
                    }
                }
                // Not a boundary. A builtin reads the words and the objects
                // the machine already holds, and answers one word; nothing
                // here is materialised into a `Value` on the way.
                Inst::CallBuiltin { dst, builtin, args } => {
                    self.sync(pc - 1);
                    match self.call_builtin(base, builtin, args) {
                        Ok(words) => {
                            // The answer is a value location like any other:
                            // `Builtin::result` names its layout, and an
                            // `Option<Int>` is two words rather than an
                            // address to two words somewhere else.
                            for (at, word) in words.iter().enumerate() {
                                self.mem.set_slot(base, dst + at as u32, *word);
                            }
                        }
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
                // A field of a *heap object* is a run of words at a static
                // offset, and its width is the layout the instruction names.
                // A field of an inline struct is not here at all: it is a
                // slot number the lowering computed, and reaching it costs
                // nothing.
                Inst::LoadField {
                    dst,
                    obj,
                    at,
                    layout,
                } => {
                    let addr = self.mem.slot(base, obj);
                    let width = self.width(layout);
                    match self.checked(addr, at, width) {
                        Ok(()) => self.mem.copy_words(
                            base + dst as u64,
                            self.mem.payload_addr(addr, at),
                            width,
                        ),
                        Err(error) => fail!(error),
                    }
                }
                Inst::StoreField {
                    obj,
                    at,
                    src,
                    layout,
                } => {
                    let addr = self.mem.slot(base, obj);
                    let width = self.width(layout);
                    match self.checked(addr, at, width) {
                        Ok(()) => self.mem.copy_words(
                            self.mem.payload_addr(addr, at),
                            base + src as u64,
                            width,
                        ),
                        Err(error) => fail!(error),
                    }
                }
                // The stride is the element layout's width, so an
                // `Array<Point>` is a run of two-word elements rather than a
                // run of addresses.
                Inst::LoadElem {
                    dst,
                    obj,
                    index,
                    layout,
                } => {
                    let addr = self.mem.slot(base, obj);
                    let at = self.mem.slot(base, index) as i64;
                    let width = self.width(layout);
                    match self.element(addr, at, width) {
                        Ok(at) => self.mem.copy_words(
                            base + dst as u64,
                            self.mem.payload_addr(addr, at),
                            width,
                        ),
                        Err(error) => fail!(error),
                    }
                }
                Inst::StoreElem {
                    obj,
                    index,
                    src,
                    layout,
                } => {
                    let addr = self.mem.slot(base, obj);
                    let at = self.mem.slot(base, index) as i64;
                    let width = self.width(layout);
                    match self.element(addr, at, width) {
                        Ok(at) => self.mem.copy_words(
                            self.mem.payload_addr(addr, at),
                            base + src as u64,
                            width,
                        ),
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
                // The other half of the header word `Len` reads. What an
                // object *is* is a question the object answers, and this is
                // that answer as an `Int`, so that a dispatch over it is an
                // ordinary `Switch` rather than an instruction of its own.
                Inst::LayoutOf { dst, obj } => {
                    let addr = self.mem.slot(base, obj);
                    if addr == 0 {
                        fail!(null_object());
                    }
                    let layout = self.mem.object_layout(addr).0 as i64;
                    self.mem.set_slot(base, dst, layout as u64);
                }

                // ---- places --------------------------------------------------
                Inst::AddrOfSlot { dst, slot } => self.mem.set_slot(base, dst, base + slot as u64),
                Inst::AddrOfField { dst, obj, at } => {
                    let addr = self.mem.slot(base, obj);
                    match self.checked(addr, at, 1) {
                        Ok(()) => {
                            let word = self.mem.payload_addr(addr, at);
                            self.mem.set_slot(base, dst, word);
                        }
                        Err(error) => fail!(error),
                    }
                }
                Inst::AddrOfElem {
                    dst,
                    obj,
                    index,
                    layout,
                } => {
                    let addr = self.mem.slot(base, obj);
                    let at = self.mem.slot(base, index) as i64;
                    let width = self.width(layout);
                    match self.element(addr, at, width) {
                        Ok(at) => {
                            let word = self.mem.payload_addr(addr, at);
                            self.mem.set_slot(base, dst, word);
                        }
                        Err(error) => fail!(error),
                    }
                }
                // A place is the address of the *first word* of a value
                // location, and its width is static — so a load and a store
                // through one move the words the layout says, and a nested
                // write through a `var` parameter updates the destination
                // words in place with nothing in between.
                // The one place instruction whose operand is a place. It
                // is arithmetic and nothing else: what an address names is a
                // value location, and a value location's parts are at static
                // offsets from its first word, so a field of a `var`
                // parameter is an addition rather than a load of the whole
                // value and a store of it back.
                Inst::AddrOfPart { dst, addr, at } => {
                    let word = self.mem.slot(base, addr);
                    self.mem.set_slot(base, dst, word + at as u64);
                }
                Inst::Load { dst, addr, layout } => {
                    let addr = self.mem.slot(base, addr);
                    let width = self.width(layout);
                    self.mem.copy_words(base + dst as u64, addr, width);
                }
                Inst::Store { addr, src, layout } => {
                    let addr = self.mem.slot(base, addr);
                    let width = self.width(layout);
                    self.mem.copy_words(addr, base + src as u64, width);
                }

                // ---- erasure ---------------------------------------------------
                // A box holds the layout of what it carries in payload word
                // 0 and that value's words after it, so a boxed `Point` is a
                // two-word payload rather than a reference to somewhere else
                // again. The header's length carries the width, because a
                // `Boxed` layout cannot know it.
                Inst::Box { dst, src, layout } => {
                    let width = self.width(layout);
                    self.sync(pc - 1);
                    let boxed = match self.allocate(self.boxed_layout(), width) {
                        Ok(addr) => addr,
                        Err(error) => fail!(error),
                    };
                    self.mem.set_payload(boxed, 0, layout.0 as u64);
                    self.mem
                        .copy_words(self.mem.payload_addr(boxed, 1), base + src as u64, width);
                    self.mem.set_slot(base, dst, boxed);
                }
                Inst::Unbox { dst, src, layout } => {
                    let addr = self.mem.slot(base, src);
                    if addr == 0 {
                        fail!(null_object());
                    }
                    if self.mem.payload(addr, 0) != layout.0 as u64 {
                        fail!(RuntimeError::new(
                            "this value is not of the type it is being read as"
                        ));
                    }
                    let width = self.width(layout);
                    self.mem
                        .copy_words(base + dst as u64, self.mem.payload_addr(addr, 1), width);
                }

                // ---- failure -----------------------------------------------------
                Inst::Trap { message } => {
                    let message = program.string(message).to_string();
                    fail!(RuntimeError::new(message))
                }
                // The only instruction that changes nothing the program can
                // read. `message` is the `String` the failing arm just
                // built, and it is read here rather than carried out in the
                // `Err`, because the `Err` is a value like any other and
                // this is the last moment anything knows which assertion it
                // came from.
                //
                // The bytes are copied. A run goes on after a failed
                // assertion — `?` propagates it, a test catches it — and the
                // object holding them is unreachable as soon as the arm
                // clears its slot, so a reference kept here would be a root
                // nothing walks.
                //
                // Lossily, which is the one place this crate reads a string
                // that way. Every string in the heap was written from valid
                // UTF-8, so bytes that are not are a bug in this machine;
                // the boundary answers that with an error because it is
                // handing the value to a program, and this is a report about
                // a failure that already happened. Stopping a run over the
                // rendering of its own diagnostic would lose the diagnostic.
                Inst::AssertFailed { message } => {
                    let addr = self.mem.slot(base, message);
                    let text = String::from_utf8_lossy(&self.string_bytes(addr)).into_owned();
                    self.assertion_failure = Some((self.span(id, pc - 1), text));
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

    /// How many words a value of `layout` occupies.
    ///
    /// The one question every move in the machine asks now: a value location
    /// is a base slot and a width, and this is the width. It is a table read
    /// rather than a walk, because [`cove_lir::Layout`] caches the flattened
    /// words for exactly the readers that are on this path.
    #[inline]
    fn width(&self, layout: LayoutId) -> u32 {
        self.program.layout(layout).width()
    }

    /// Allocates, collecting once if the first attempt does not fit.
    fn allocate(&mut self, layout: LayoutId, len: u32) -> Result<u64, RuntimeError> {
        let words = self
            .program
            .layout(layout)
            .payload_words(len, &self.program.layouts);
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
        // The host resource table is not among them and could not be. It
        // holds names rather than addresses, so there is nothing in it for a
        // mark to follow — and a `Repr::Host` word in a frame is not gathered
        // above, because `Function::refs` is `RefMap::of` the `Repr`s and
        // `Repr::Host::is_ref` is false.
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

    /// The resource a [`Repr::Host`] word names, or `None` for a word that
    /// names none.
    ///
    /// `None` is two things, and the caller is what tells them apart: the
    /// zero a frame's own zeroing leaves in a slot nothing has written, and a
    /// word from somewhere that is not this table. Both are questions about a
    /// value crossing rather than about the machine, so both are reported by
    /// [`crate::lvm::boundary`] and neither is decided here.
    pub(crate) fn resource(&self, word: u64) -> Option<&Arc<ResourceHandle>> {
        self.resources.get(word.checked_sub(1)? as usize)
    }

    /// The word naming `handle`, writing it into the table the first time
    /// this run is handed it.
    ///
    /// Interned rather than appended, so that one resource is one word for
    /// the length of a run. ADR 0013 says two handles are equal when they
    /// name the same resource, and a table that gave one resource two words
    /// would be a table on which comparing the words was not comparing the
    /// resources — which is the one thing an untagged word naming a resource
    /// has to get right. `task_safe` is not part of the comparison for the
    /// same reason it is not part of [`ResourceHandle::names_same`]: it is a
    /// fact about the kind, copied onto every handle of it, so two handles
    /// naming one resource cannot disagree about it.
    ///
    /// The scan is linear over the resources this run has been handed. That
    /// is the table the host is keeping too, at the size the host keeps it.
    pub(crate) fn resource_word(&mut self, handle: &ResourceHandle) -> u64 {
        if let Some(at) = self
            .resources
            .iter()
            .position(|held| held.names_same(handle))
        {
            return at as u64 + 1;
        }
        self.resources.push(Arc::new(handle.clone()));
        // One past the index, because a zeroed slot has to mean no resource.
        self.resources.len() as u64
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
        base: u64,
        op: HostOpId,
        args: ArgsId,
        budget: &Meter,
        span: Span,
    ) -> Result<Vec<u64>, RuntimeError> {
        let program = self.program;
        let op = program.host_op(op);
        let list = program.arg_list(args);

        // An argument names a value location, so each one materialises as
        // the whole of what is at it. That is why a struct or an enum
        // reaches a host as itself: it used to be boxed on the way in — a
        // slot said where an operand began and never how wide it was — and
        // the host was handed an erased value where the schema declared a
        // concrete one.
        let mut values = Vec::with_capacity(list.len());
        for arg in list {
            let words = self
                .mem
                .read_words(base + arg.slot as u64, self.width(arg.layout));
            values.push(
                boundary::to_value(self, arg.layout, &words).map_err(|error| error.at(span))?,
            );
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
        let result = op.result;
        boundary::from_value(self, result, &answer).map_err(|error| error.at(span))
    }

    /// The same, addressed to the resource the [`Repr::Host`] word in
    /// `receiver` names.
    ///
    /// Everything [`Machine::call_host`] does, through the one seam that
    /// differs: `HostRegistry::call_resource` rather than
    /// `HostRegistry::call_with`. The grant, the schema on both sides, the
    /// budget and the trace are the registry's on this path too — a resource
    /// operation is a Host API call and is charged and recorded as one — and
    /// this follows `crate::interp::Interpreter::call_host_resource`, which
    /// is the same three lines around the same call.
    ///
    /// The handle is looked up rather than materialised. ADR 0013 makes it a
    /// name the host minted, `Machine::resource` is the table that word
    /// indexes, and the registry takes it as the thing being addressed — so
    /// it is never one of `args`, and the arguments are what the host is
    /// handed.
    ///
    /// A zero word is refused rather than read through. `docs/LINEAR_VM.md`
    /// is explicit that the word is one past the index so that a slot nothing
    /// has written names no resource, and that zero *"earns the same refusal
    /// a null reference does"* — which is this one, because a `Host` slot
    /// read before it was given a handle is the same lowering bug reaching
    /// the machine.
    fn call_resource(
        &mut self,
        base: u64,
        receiver: Slot,
        op: HostOpId,
        args: ArgsId,
        budget: &Meter,
        span: Span,
    ) -> Result<Vec<u64>, RuntimeError> {
        let program = self.program;
        let op = program.host_op(op);
        let list = program.arg_list(args);

        let word = self.mem.slot(base, receiver);
        let Some(handle) = self.resource(word).cloned() else {
            return Err(null_object().at(span));
        };

        let mut values = Vec::with_capacity(list.len());
        for arg in list {
            let words = self
                .mem
                .read_words(base + arg.slot as u64, self.width(arg.layout));
            values.push(
                boundary::to_value(self, arg.layout, &words).map_err(|error| error.at(span))?,
            );
        }

        let hosts = self.hosts.ok_or_else(|| {
            RuntimeError::new(format!(
                "`{}` cannot be called, because this run has no host boundary",
                op.qualified()
            ))
            .at(span)
        })?;
        let started = Instant::now();
        let answer = hosts.call_resource(&handle, &op.operation, values, &mut Back { budget });
        self.host_wait += started.elapsed();
        let answer = answer.map_err(|error| error.at(span))?;
        let result = op.result;
        boundary::from_value(self, result, &answer).map_err(|error| error.at(span))
    }

    /// Reads the operand words out of the frame and hands them to the
    /// builtin.
    ///
    /// An operand is a value location: the layout the argument names and the
    /// words at its slot. Both halves are read here rather than in
    /// [`crate::lvm::builtins`] for the reason the boundary takes them here
    /// too — a word is untagged and where it came from is a fact about this
    /// frame, which a builtin has no business knowing about.
    ///
    /// The words are copied into one buffer and the operands point into it,
    /// so a builtin reads a whole `Point` without holding a frame and
    /// without the argument list having to promise that consecutive operands
    /// are adjacent, which it never could: the lowering places each argument
    /// where a run of the right shape was free.
    fn call_builtin(
        &mut self,
        base: u64,
        builtin: BuiltinId,
        args: ArgsId,
    ) -> Result<Vec<u64>, RuntimeError> {
        let program = self.program;
        let list = program.arg_list(args);
        let mut words: Vec<u64> = Vec::with_capacity(list.len());
        let mut runs = Vec::with_capacity(list.len());
        for arg in list {
            let from = words.len();
            let width = self.width(arg.layout);
            for at in 0..width {
                words.push(self.mem.slot(base, arg.slot + at));
            }
            runs.push((arg.layout, from, words.len()));
        }
        let operands: Vec<Operand> = runs
            .iter()
            .map(|(layout, from, to)| Operand {
                layout: *layout,
                words: &words[*from..*to],
            })
            .collect();
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

    /// The layout a [`Inst::Box`] allocates its object as.
    ///
    /// The program says, rather than this searching for a `Shape::Boxed`.
    /// A search has to answer something when it fails, and the answer it
    /// used to give — `LayoutId::FREE` — sized the object by the wrong
    /// shape, so a box of a two-word value was allocated one word short and
    /// the copy into it ran off the end of the heap.
    fn boxed_layout(&self) -> LayoutId {
        self.program.boxed_layout
    }

    /// Checks that `addr` is an object with a payload word `at`.
    ///
    /// A reference slot carries no layout, so the object is the only thing
    /// that can say how wide it is. The lowering computed `at` from the type
    /// the checker settled, so this should never refuse — and it is here
    /// because "should never" is not "cannot", and reading past an object
    /// into whatever follows it would be a silent wrong answer rather than a
    /// loud one.
    fn checked(&self, addr: u64, at: u32, width: u32) -> Result<(), RuntimeError> {
        if addr == 0 {
            return Err(null_object());
        }
        let layout = self.program.layout(self.mem.object_layout(addr));
        let words = layout.payload_words(self.mem.object_len(addr), &self.program.layouts);
        if at + width > words {
            return Err(RuntimeError::new(format!(
                "this reads word {at} of a `{}`, which has {words}",
                layout.name
            )));
        }
        Ok(())
    }

    /// The function the closure object at `addr` calls.
    ///
    /// Three things have to hold before a frame is pushed, and none of them
    /// is something a program can get wrong: the object has to be a closure's,
    /// the callee it names has to be one this program has, and the captures it
    /// holds have to be the ones that callee reads. The checker resolved the
    /// callee's type and the verifier holds the slot to `Repr::Ref`, so each
    /// of the three is a lowering bug — reported for the reason
    /// [`Machine::checked`] is, because the alternative is a frame whose
    /// capture slots hold whatever followed the object in the heap.
    ///
    /// The id comes from the object rather than from the layout, which carries
    /// one too. They agree — a layout is one per lowered lambda — and the
    /// object's word is the one [`Inst::CallClosure`] is defined in terms of.
    fn callee_of(&self, addr: u64) -> Result<FunctionId, RuntimeError> {
        if addr == 0 {
            return Err(null_object());
        }
        let program = self.program;
        let layout = program.layout(self.mem.object_layout(addr));
        let Shape::Closure { captures, .. } = &layout.shape else {
            // The oracle's words for a call of something that is not a
            // function, with the name the layout carries — which is the name
            // the declaration wrote, and so the one a `Value` of this object
            // would answer.
            return Err(RuntimeError::new(format!(
                "`{}` is not callable",
                layout.name
            )));
        };
        let word = self.mem.payload(addr, 0);
        let callee = u32::try_from(word)
            .ok()
            .map(FunctionId)
            .filter(|id| id.index() < program.functions.len())
            .ok_or_else(|| {
                RuntimeError::new(format!(
                    "this closure names function {word}, which this program has not"
                ))
            })?;
        let target = program.function(callee);
        if target.captures.len() != captures.len() {
            return Err(RuntimeError::new(format!(
                "this closure and `{}` disagree about its captures: {} held, {} read",
                target.qualified(),
                captures.len(),
                target.captures.len()
            )));
        }
        Ok(callee)
    }

    /// Turns a language-level index into a payload offset, at a stride of
    /// `width`.
    ///
    /// The header's length counts *elements*, not words, so an index is
    /// checked against it and then multiplied — which is what makes an
    /// `Array<Point>` a run of two-word elements and an out-of-range index on
    /// one say the same thing it says on an `Array<Int>`.
    fn element(&self, addr: u64, at: i64, width: u32) -> Result<u32, RuntimeError> {
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
        Ok(at as u32 * width)
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

    /// Re-labels the object at `addr`, releasing the `spare` words it gives
    /// up. See [`Memory::relabel`].
    pub(crate) fn relabel(&mut self, addr: u64, layout: LayoutId, len: u32, spare: u32) {
        let payload = self.payload_words(layout, len);
        self.mem.relabel(addr, layout, len, payload, spare);
    }

    /// The `words` payload words of the object at `addr`, from `at`.
    ///
    /// What a boundary reads when a value is inline in a payload: an array
    /// element, a capture, a struct field, the value inside a box. Nothing in
    /// ordinary execution calls it — a move inside the machine never leaves
    /// the memory.
    pub(crate) fn payload_run(&self, addr: u64, at: u32, words: u32) -> Vec<u64> {
        self.mem.read_words(self.mem.payload_addr(addr, at), words)
    }

    /// Writes `words` into the payload of the object at `addr`, from `at`.
    pub(crate) fn set_payload_run(&mut self, addr: u64, at: u32, words: &[u64]) {
        for (offset, word) in words.iter().enumerate() {
            self.mem.set_payload(addr, at + offset as u32, *word);
        }
    }

    /// How many words a value of `layout` occupies, for a caller outside the
    /// dispatch loop.
    pub(crate) fn words_of(&self, layout: LayoutId) -> u32 {
        self.width(layout)
    }

    /// How many payload words an object of `layout` with header length `len`
    /// occupies.
    pub(crate) fn payload_words(&self, layout: LayoutId, len: u32) -> u32 {
        self.program
            .layout(layout)
            .payload_words(len, &self.program.layouts)
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
/// runs closures — [`Inst::CallClosure`] pushes a frame like any other call —
/// but it hands none of them out: a `Shape::Closure` object is refused on its
/// way across [`boundary::to_value`], because a public `Value` has no way to
/// carry a body this backend could be asked to run. So a host is never holding
/// one of this machine's closures, and the call arm says what is missing
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
            "this host call cannot run {}, because the linear-memory backend hands no callback across the boundary",
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

/// A call passed a number of arguments the callee does not declare.
///
/// The verifier checks it, so this is a lowering bug that got past it. It is
/// reported rather than assumed because the alternative is a callee whose
/// remaining parameters hold whatever the frame was zeroed with — which is a
/// silent wrong answer instead of a loud one.
fn wrong_arity(callee: String, declared: usize, given: usize) -> RuntimeError {
    RuntimeError::new(format!(
        "this call passes {given} argument(s) to `{callee}`, which declares {declared}"
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
    use cove_lir::{Arg, ArgsId, Capture, Function, Layout, RefMap, Table, TableId};
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

        /// A family that lives in the heap, so a value of it is one address.
        pub(crate) fn layout(&mut self, name: &str, shape: Shape) -> LayoutId {
            self.push(Layout::object(name, shape))
        }

        /// A one-word family: the width-one case of the whole model.
        pub(crate) fn word(&mut self, name: &str, repr: Repr) -> LayoutId {
            self.push(Layout::word(name, repr))
        }

        /// A struct, laid out inline from its fields' layouts.
        ///
        /// The offsets are computed by `cove_lir::struct_layout` rather than
        /// written out, because they are not a choice a fixture gets to make:
        /// a fixture free to say where a field is could agree with a machine
        /// that had it wrong.
        pub(crate) fn structure(&mut self, name: &str, fields: &[(&str, LayoutId)]) -> LayoutId {
            let named: Vec<(Arc<str>, LayoutId)> = fields
                .iter()
                .map(|(name, id)| (Arc::from(*name), *id))
                .collect();
            let (fields, words) = cove_lir::struct_layout(&named, &self.program.layouts);
            self.push(Layout::inline(
                name,
                Shape::Struct {
                    fields,
                    opaque: false,
                },
                words,
            ))
        }

        /// An `export opaque struct`, which renders as its bare name.
        pub(crate) fn opaque(&mut self, name: &str, fields: &[(&str, LayoutId)]) -> LayoutId {
            let id = self.structure(name, fields);
            if let Shape::Struct { opaque, .. } = &mut self.program.layouts[id.index()].shape {
                *opaque = true;
            }
            id
        }

        /// An enum, laid out under the payload-agreement rule.
        pub(crate) fn enumeration(
            &mut self,
            name: &str,
            cases: &[(&str, Vec<LayoutId>)],
        ) -> LayoutId {
            let named: Vec<(Arc<str>, Vec<LayoutId>)> = cases
                .iter()
                .map(|(name, parts)| (Arc::from(*name), parts.clone()))
                .collect();
            let (cases, payload) = cove_lir::enum_layout(&named, &self.program.layouts);
            let mut words = vec![Repr::Int];
            words.extend_from_slice(&payload);
            self.push(Layout::inline(name, Shape::Enum { cases, payload }, words))
        }

        /// The layout a `Box` allocates, reserved the way the lowering
        /// reserves it: a fixture that had to remember to declare one would
        /// be a fixture that could forget, and forgetting sizes the object
        /// by the wrong shape.
        pub(crate) fn boxed(&mut self) -> LayoutId {
            self.seed();
            self.program.boxed_layout
        }

        /// `LayoutId(0)` is the sweeper's free block and `LayoutId(1)` is the
        /// box, exactly as `cove_lir::lower` reserves them.
        fn seed(&mut self) {
            if self.program.layouts.is_empty() {
                self.program.layouts.push(Layout::free());
                self.program
                    .layouts
                    .push(Layout::object("Any", Shape::Boxed));
                self.program.boxed_layout = LayoutId(1);
            }
        }

        fn push(&mut self, layout: Layout) -> LayoutId {
            self.seed();
            self.program.layouts.push(layout);
            LayoutId(self.program.layouts.len() as u32 - 1)
        }

        /// An argument list: where each value is, and the layout that says
        /// how wide it is.
        pub(crate) fn args(&mut self, args: &[(Slot, LayoutId)]) -> ArgsId {
            self.program.args.push(
                args.iter()
                    .map(|(slot, layout)| Arg {
                        slot: *slot,
                        layout: *layout,
                    })
                    .collect(),
            );
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
            params: &[LayoutId],
            reprs: &[Repr],
            returns: LayoutId,
            code: Vec<Inst>,
        ) -> FunctionId {
            let nowhere = Span::new(cove_diag::FileId(0), 0, 0);
            let spans = vec![nowhere; code.len()];
            self.program.functions.push(Function {
                module: Arc::from("t"),
                name: Arc::from(name),
                params: params.to_vec(),
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

        /// A function that reads captures: what a lowered lambda is.
        ///
        /// The slot each capture lands in is filled in here rather than
        /// written out per fixture, because it is not a choice a fixture gets
        /// to make: captures follow the parameters, so the first one begins
        /// where the parameters' words end and each one after it follows at
        /// its own width, and a fixture free to say otherwise could agree
        /// with a machine that had the rule wrong.
        pub(crate) fn lambda(
            &mut self,
            name: &str,
            params: &[LayoutId],
            reprs: &[Repr],
            returns: LayoutId,
            captures: &[LayoutId],
            code: Vec<Inst>,
        ) -> FunctionId {
            let mut slot: Slot = params
                .iter()
                .map(|id| self.program.layout(*id).width())
                .sum();
            let held: Vec<Capture> = captures
                .iter()
                .enumerate()
                .map(|(at, layout)| {
                    let capture = Capture {
                        name: Arc::from(format!("c{at}")),
                        slot,
                        layout: *layout,
                    };
                    slot += self.program.layout(*layout).width();
                    capture
                })
                .collect();
            let id = self.function(name, params, reprs, returns, code);
            self.program.functions[id.index()].captures = held;
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

        /// The one-word layout of `repr`, declared once per fixture.
        pub(crate) fn scalar(&mut self, repr: Repr) -> LayoutId {
            if let Some(at) = self
                .program
                .layouts
                .iter()
                .position(|layout| layout.shape == Shape::Word(repr))
            {
                return LayoutId(at as u32);
            }
            self.word(repr.name(), repr)
        }
    }

    pub(crate) fn budget() -> Meter {
        crate::budget::Budget::new(crate::budget::Limits::default()).meter()
    }

    /// The words a run of `entry` answers.
    ///
    /// A function answers a *value location*, which is a run of words, so
    /// this is the general shape of a result and [`run`] is the common case
    /// of it. A fixture that answers a `Point` reads two words here rather
    /// than one address naming two words somewhere else.
    fn run_words(
        program: &Program,
        entry: FunctionId,
        args: &[u64],
    ) -> Result<Vec<u64>, RuntimeError> {
        Machine::new(program, 1 << 16).run(entry, args, &budget())
    }

    /// The one word a run of `entry` answers.
    ///
    /// Most of what is under test here is one word wide — an `Int`, a `Bool`,
    /// a reference — and writing `[0]` at every one of those call sites would
    /// put the same unchecked index in fifty places. The assertion is what
    /// keeps it honest: a fixture whose answer stopped being one word fails
    /// here rather than quietly reporting its first word.
    fn run(program: &Program, entry: FunctionId, args: &[u64]) -> Result<u64, RuntimeError> {
        let words = run_words(program, entry, args)?;
        assert_eq!(words.len(), 1, "this fixture answers one word");
        Ok(words[0])
    }

    #[test]
    fn a_constant_comes_back() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let f = build.function(
            "answer",
            &[],
            &[Repr::Int],
            int,
            vec![Inst::Int { dst: 0, value: 42 }, Inst::Return { src: 0 }],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[]).unwrap() as i64, 42);
    }

    #[test]
    fn arithmetic_reads_and_writes_slots() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let f = build.function(
            "add",
            &[int, int],
            &[Repr::Int, Repr::Int, Repr::Int],
            int,
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
            let int = build.scalar(Repr::Int);
            let f = build.function(
                "fault",
                &[int, int],
                &[Repr::Int, Repr::Int, Repr::Int],
                int,
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
        let duration = build.scalar(Repr::Duration);
        let f = build.function(
            "late",
            &[duration, duration],
            &[Repr::Duration, Repr::Duration, Repr::Duration],
            duration,
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
        let int = build.scalar(Repr::Int);
        // fn abs(n) { if n < 0 { -n } else { n } }
        let f = build.function(
            "abs",
            &[int],
            &[Repr::Int, Repr::Int, Repr::Bool],
            int,
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
        let int = build.scalar(Repr::Int);
        // fn sum(n) { var t = 0; var i = 0; while i < n { t = t + i; i = i + 1 }; t }
        let f = build.function(
            "sum",
            &[int],
            &[Repr::Int, Repr::Int, Repr::Int, Repr::Bool, Repr::Int],
            int,
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
        let int = build.scalar(Repr::Int);
        let args = build.args(&[(3, int)]);
        // fn fact(n) { if n <= 1 { 1 } else { n * fact(n - 1) } }
        let f = build.function(
            "fact",
            &[int],
            &[Repr::Int, Repr::Int, Repr::Bool, Repr::Int, Repr::Int],
            int,
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
        let int = build.scalar(Repr::Int);
        let args = build.args(&[(0, int)]);
        let f = build.function(
            "forever",
            &[int],
            &[Repr::Int, Repr::Int],
            int,
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

    // ---- values that are more than one word ------------------------------

    /// `docs/LINEAR_VM.md` §1, in the IR it writes out.
    ///
    /// ~~~cove
    /// struct Point { x: Int, y: Int }
    /// var a = Point(x: 1, y: 2)
    /// var b = a
    /// b.x = 7
    /// ~~~
    ///
    /// `a` is at slots 0–1 and `b` at 2–3, and `b = a` is one `Copy` of two
    /// words. `a.x` is slot 0 and nothing touched it — not because a bit said
    /// the copy was unshared, but because the copy put `b`'s words where `b`
    /// is. There is no sharing bit, no copy-on-write and no write path to
    /// unshare; `b.x = 7` writes slot 2 and that is all of it.
    ///
    /// The answer is the four slots read as one `Pair`, which is the same
    /// claim from the other side: a value location is a base slot and a
    /// layout, so two adjacent `Point`s *are* a four-word value.
    #[test]
    fn a_copy_is_the_words_of_the_value() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        let pair = build.structure("Pair", &[("a", point), ("b", point)]);
        let f = build.function(
            "copy",
            &[],
            &[Repr::Int, Repr::Int, Repr::Int, Repr::Int, Repr::Int],
            pair,
            vec![
                Inst::Int { dst: 0, value: 1 },
                Inst::Int { dst: 1, value: 2 },
                Inst::Copy {
                    dst: 2,
                    src: 0,
                    layout: point,
                },
                Inst::Int { dst: 4, value: 7 },
                // `b.x` is slot 2 + 0: a field of an inline struct is
                // arithmetic the lowering did, not an instruction.
                Inst::Copy {
                    dst: 2,
                    src: 4,
                    layout: int,
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        assert_eq!(program.layout(point).words, vec![Repr::Int, Repr::Int]);
        assert_eq!(run_words(&program, f, &[]).unwrap(), vec![1, 2, 7, 2]);
    }

    /// `docs/LINEAR_VM.md` §3: `struct Wrapper { p: Point, v: Vector<Int> }`
    /// is `[p.x: Int, p.y: Int, v: Ref]`, and a copy copies all three words.
    ///
    /// Two answers fall out of that one copy and neither needed a policy. The
    /// `Point` words become independent, so writing `b.p.x` leaves `a.p.x`
    /// alone. The `Vector` address is duplicated, so both wrappers name one
    /// vector — which is ADR 0001 verbatim, because a `Vector`'s storage is
    /// shared and mutable by the language's own rule rather than by anything
    /// the representation decided.
    #[test]
    fn a_copied_wrapper_separates_its_point_and_shares_its_vector() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        let vector = build.layout("Vector", Shape::Vector { elem: int });
        let wrapper = build.structure("Wrapper", &[("p", point), ("v", vector)]);
        let both = build.structure("Both", &[("a", wrapper), ("b", wrapper)]);
        let f = build.function(
            "wrap",
            &[],
            &[
                Repr::Int,
                Repr::Int,
                Repr::Ref,
                Repr::Int,
                Repr::Int,
                Repr::Ref,
                Repr::Int,
            ],
            both,
            vec![
                Inst::Int { dst: 0, value: 1 },
                Inst::Int { dst: 1, value: 2 },
                Inst::Alloc {
                    dst: 2,
                    layout: vector,
                    len: Len::Fixed,
                },
                Inst::Copy {
                    dst: 3,
                    src: 0,
                    layout: wrapper,
                },
                Inst::Int { dst: 6, value: 7 },
                Inst::Copy {
                    dst: 3,
                    src: 6,
                    layout: int,
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        assert_eq!(
            program.layout(wrapper).words,
            vec![Repr::Int, Repr::Int, Repr::Ref]
        );
        let words = run_words(&program, f, &[]).unwrap();
        assert_eq!(words.len(), 6);
        assert_eq!(&words[..2], &[1, 2], "`a`'s point is where `a` is");
        assert_eq!(&words[3..5], &[7, 2], "`b`'s point is where `b` is");
        assert_ne!(words[2], 0, "the vector was allocated");
        assert_eq!(words[2], words[5], "and both wrappers name that one vector");
    }

    /// `docs/LINEAR_VM.md` §5: a parameter takes the words its layout says,
    /// from slot 0 onward in declaration order, so a `(Int, Point, Int)` list
    /// occupies slots 0, 1–2 and 3. Nothing is permuted into type groups,
    /// because there are no type groups.
    ///
    /// The answer is a `Point` too, and `Return` copies the two words its
    /// `Function::returns` describes into the caller's destination location.
    /// Neither direction allocates: a struct crosses a call as its words.
    #[test]
    fn a_struct_is_passed_and_returned_as_its_words() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        // fn shift(n: Int, p: Point, m: Int) -> Point
        let shift = build.function(
            "shift",
            &[int, point, int],
            &[
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Int,
            ],
            point,
            vec![
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 4,
                    a: 1,
                    b: 0,
                },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 5,
                    a: 2,
                    b: 3,
                },
                Inst::Return { src: 4 },
            ],
        );
        let args = build.args(&[(0, int), (1, point), (3, int)]);
        let main = build.function(
            "main",
            &[],
            &[
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Int,
            ],
            point,
            vec![
                Inst::Int { dst: 0, value: 10 },
                Inst::Int { dst: 1, value: 1 },
                Inst::Int { dst: 2, value: 2 },
                Inst::Int { dst: 3, value: 20 },
                Inst::Call {
                    dst: 4,
                    callee: shift,
                    args,
                },
                Inst::Return { src: 4 },
            ],
        );
        let program = build.done();
        let target = program.function(shift);
        assert_eq!(target.param_slot(0, &program.layouts), 0);
        assert_eq!(target.param_slot(1, &program.layouts), 1);
        assert_eq!(target.param_slot(2, &program.layouts), 3);
        assert_eq!(target.param_words(&program.layouts), 4);
        assert_eq!(run_words(&program, main, &[]).unwrap(), vec![11, 22]);
    }

    /// An `Array<Point>` is a run of two-word elements rather than a run of
    /// addresses, and the stride an element instruction uses is the element
    /// layout's width.
    ///
    /// The header's `len` counts *elements*, so an index is checked against
    /// three and then multiplied — which is why writing element 1 through an
    /// `AddrOfElem` leaves element 2 alone rather than smearing across it,
    /// and why index 3 is refused although the object holds six words.
    #[test]
    fn an_array_of_points_is_walked_at_a_two_word_stride() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        let points = build.layout(
            "Array",
            Shape::Elements {
                elem: point,
                growable: false,
            },
        );
        let two = build.structure("Two", &[("a", point), ("b", point)]);
        let reprs = &[
            Repr::Ref,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Addr,
        ];
        let walk = build.function(
            "walk",
            &[],
            reprs,
            two,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: points,
                    len: Len::Count(3),
                },
                // xs[0] = Point(1, 2)
                Inst::Int { dst: 1, value: 0 },
                Inst::Int { dst: 2, value: 1 },
                Inst::Int { dst: 3, value: 2 },
                Inst::StoreElem {
                    obj: 0,
                    index: 1,
                    src: 2,
                    layout: point,
                },
                // xs[1] = Point(3, 4)
                Inst::Int { dst: 1, value: 1 },
                Inst::Int { dst: 2, value: 3 },
                Inst::Int { dst: 3, value: 4 },
                Inst::StoreElem {
                    obj: 0,
                    index: 1,
                    src: 2,
                    layout: point,
                },
                // xs[2] = Point(5, 6)
                Inst::Int { dst: 1, value: 2 },
                Inst::Int { dst: 2, value: 5 },
                Inst::Int { dst: 3, value: 6 },
                Inst::StoreElem {
                    obj: 0,
                    index: 1,
                    src: 2,
                    layout: point,
                },
                // A place naming element 1, written through: two words at
                // one address, with nothing between the address and them.
                Inst::Int { dst: 1, value: 1 },
                Inst::AddrOfElem {
                    dst: 8,
                    obj: 0,
                    index: 1,
                    layout: point,
                },
                Inst::Int { dst: 2, value: 30 },
                Inst::Int { dst: 3, value: 40 },
                Inst::Store {
                    addr: 8,
                    src: 2,
                    layout: point,
                },
                Inst::LoadElem {
                    dst: 4,
                    obj: 0,
                    index: 1,
                    layout: point,
                },
                Inst::Int { dst: 1, value: 2 },
                Inst::LoadElem {
                    dst: 6,
                    obj: 0,
                    index: 1,
                    layout: point,
                },
                Inst::Return { src: 4 },
            ],
        );
        let past = build.function(
            "past",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: points,
                    len: Len::Count(3),
                },
                Inst::Int { dst: 1, value: 3 },
                Inst::LoadElem {
                    dst: 2,
                    obj: 0,
                    index: 1,
                    layout: point,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(
            run_words(&program, walk, &[]).unwrap(),
            vec![30, 40, 5, 6],
            "the write through element 1 left element 2 where it was"
        );
        let error = run(&program, past, &[]).unwrap_err();
        assert_eq!(error.message, "index 3 is outside a collection of 3");
    }

    /// An enum is a discriminant word and a payload region wide enough for
    /// every case, and the offsets are assigned so that **every case using a
    /// payload word agrees on its `Repr`**.
    ///
    /// `enum Msg { Text(Cell), Count(Int) }` therefore lays out as
    /// `[disc: Int, Ref, Int]`: `Count`'s `Int` cannot share `Text`'s
    /// reference word, so it takes a third. Two things follow, and this is
    /// both of them. Constructing a case zeroes the region it does not fill,
    /// so `Count`'s reference word reads null rather than whatever `Text`
    /// left there. And the collector never reads the discriminant to decide
    /// what to trace — the region's map is static, which is one fewer thing
    /// that can be wrong.
    ///
    /// The heap holds one cell and not two, so the second allocation is the
    /// question: it succeeds when the value was rebuilt as `Count`, because
    /// the word naming the first cell was zeroed and nothing reaches it, and
    /// it fails when the value is still `Text`, because that same word is
    /// traced and the cell is live.
    #[test]
    fn an_enums_payload_is_retained_by_its_static_map() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let cell = build.layout(
            "Cell",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        let msg = build.enumeration("Msg", &[("Text", vec![cell]), ("Count", vec![int])]);
        let boolean = build.scalar(Repr::Bool);
        let f = build.function(
            "held",
            &[boolean],
            &[
                Repr::Bool,
                // The `Msg` is at slots 1–3: a discriminant, `Text`'s
                // reference and `Count`'s integer.
                Repr::Int,
                Repr::Ref,
                Repr::Int,
                Repr::Ref,
                Repr::Int,
            ],
            int,
            vec![
                Inst::Alloc {
                    dst: 4,
                    layout: cell,
                    len: Len::Count(1200),
                },
                Inst::Int { dst: 1, value: 0 },
                Inst::Copy {
                    dst: 2,
                    src: 4,
                    layout: cell,
                },
                // The enum's payload word is now the only name for the cell.
                Inst::Clear {
                    slot: 4,
                    layout: cell,
                },
                Inst::BranchFalse { cond: 0, to: 8 },
                // Constructing `Count` zeroes the region it does not fill,
                // which is what leaves `Text`'s reference word null.
                Inst::Clear {
                    slot: 1,
                    layout: msg,
                },
                Inst::Int { dst: 1, value: 1 },
                Inst::Int { dst: 3, value: 5 },
                Inst::Alloc {
                    dst: 4,
                    layout: cell,
                    len: Len::Count(1200),
                },
                Inst::Int { dst: 5, value: 7 },
                Inst::Return { src: 5 },
            ],
        );
        let program = build.done();
        assert_eq!(
            program.layout(msg).words,
            vec![Repr::Int, Repr::Ref, Repr::Int]
        );

        let mut kept = Machine::new(&program, 2048);
        let error = kept.run(f, &[0], &budget()).unwrap_err();
        assert_eq!(error.message, "this run has no memory left");

        let mut dropped = Machine::new(&program, 2048);
        assert_eq!(dropped.run(f, &[1], &budget()).unwrap(), vec![7]);
        assert!(
            dropped.collected().collections > 0,
            "the second cell only fits after the first is reclaimed"
        );
    }

    /// A frame's map is a function of its `Repr`s, and a multiword value
    /// contributes its flattened per-word ones.
    ///
    /// `docs/LINEAR_VM.md` §6: a `Wrapper { p: Point, v: Vector }` at slot 5
    /// contributes `Int, Int, Ref`, so slot 7 is a root and 5 and 6 are not.
    /// Nothing about the value's *width* reaches the collector — it reads one
    /// bit per slot, as it did when every value was one word, and a wide
    /// value is simply several slots' worth of bits.
    ///
    /// The other half is that a slot the map does not name cannot hold a
    /// reference at all: the verifier holds every instruction to the `Repr`
    /// of the slot it names, so a program that put an address in slot 6 is
    /// not a program. That is what makes one static bit per slot sound, and
    /// it is why the dynamic half of this test reaches for `Clear` instead —
    /// a reference slot the map *does* name, emptied at its last use.
    #[test]
    fn a_frames_map_covers_a_multiword_value_word_by_word() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let cell = build.layout(
            "Cell",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        let wrapper = build.structure("Wrapper", &[("p", point), ("v", cell)]);
        assert_eq!(
            build.program.layout(wrapper).words,
            vec![Repr::Int, Repr::Int, Repr::Ref],
            "three words: the `Point` inline, and the cell's address"
        );

        // A `Wrapper` at slots 5-7, a scratch reference at slot 8, and the
        // answer at slot 9.
        let reprs = vec![
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Ref,
            Repr::Ref,
            Repr::Int,
        ];
        let f = build.function(
            "wrapper",
            &[],
            &reprs,
            int,
            vec![
                // The wrapper's `v`, which its own slot keeps alive.
                Inst::Alloc {
                    dst: 7,
                    layout: cell,
                    len: Len::Count(600),
                },
                // A second cell, named by a reference slot that is then
                // cleared — so the map still reads slot 8, and reads null.
                Inst::Alloc {
                    dst: 8,
                    layout: cell,
                    len: Len::Count(600),
                },
                Inst::Clear {
                    slot: 8,
                    layout: cell,
                },
                Inst::Int { dst: 5, value: 1 },
                Inst::Int { dst: 6, value: 2 },
                // A third cell fits only if the second was reclaimed, and the
                // first must survive to be written through afterwards.
                Inst::Alloc {
                    dst: 8,
                    layout: cell,
                    len: Len::Count(600),
                },
                Inst::Int { dst: 9, value: 0 },
                Inst::Int {
                    dst: 4,
                    value: 4242,
                },
                Inst::StoreElem {
                    obj: 7,
                    index: 9,
                    src: 4,
                    layout: int,
                },
                Inst::LoadElem {
                    dst: 9,
                    obj: 7,
                    index: 9,
                    layout: int,
                },
                Inst::Return { src: 9 },
            ],
        );
        let program = build.done();

        // The static half, which is the claim `docs/LINEAR_VM.md` makes.
        let refs = &program.function(f).refs;
        assert!(!refs.is_ref(5), "the `Point`'s x is not a root");
        assert!(!refs.is_ref(6), "the `Point`'s y is not a root");
        assert!(refs.is_ref(7), "the vector's address is");
        assert_eq!(refs.iter().collect::<Vec<_>>(), vec![7, 8]);

        // The dynamic half: two cells fit at a time and three do not, so the
        // run only finishes because the cleared slot stopped being a root —
        // and it finishes with the wrapper's own cell still there to write.
        let mut machine = Machine::new(&program, 1600);
        assert_eq!(machine.run(f, &[], &budget()).unwrap(), vec![4242]);
        assert!(
            machine.collected().collections > 0,
            "the third cell only fits after the cleared one is reclaimed"
        );
    }

    // ---- closures ------------------------------------------------------

    /// The layout of a lambda that reads `captures`.
    fn closure_layout(build: &mut Build, function: FunctionId, captures: &[LayoutId]) -> LayoutId {
        build.layout(
            "closure",
            Shape::Closure {
                function,
                captures: captures.to_vec(),
            },
        )
    }

    /// A closure's frame is a callee's frame with two writes rather than one:
    /// the arguments into the words the parameters occupy, and then the
    /// captures into the slots `Function::captures` names, which are the ones
    /// straight after.
    #[test]
    fn a_closure_call_copies_the_arguments_then_the_captures() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        // { it -> it + captured }
        let add = build.lambda(
            "lambda",
            &[int],
            &[Repr::Int, Repr::Int, Repr::Int],
            int,
            &[int],
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
        let layout = closure_layout(&mut build, add, &[int]);
        let args = build.args(&[(3, int)]);
        let main = build.function(
            "main",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout,
                    len: Len::Fixed,
                },
                Inst::Int {
                    dst: 1,
                    value: add.0 as i64,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: int,
                },
                Inst::Int { dst: 2, value: 10 },
                Inst::StoreField {
                    obj: 0,
                    at: 1,
                    src: 2,
                    layout: int,
                },
                Inst::Int { dst: 3, value: 5 },
                Inst::CallClosure {
                    dst: 4,
                    closure: 0,
                    args,
                },
                Inst::Return { src: 4 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, main, &[]).unwrap() as i64, 15);
    }

    /// A capture is copied into a `Repr::Ref` slot of the callee's frame, so
    /// it is a root of that frame like any other — which is what makes a
    /// closure need no second story for the collector.
    ///
    /// The captured object is reachable from nowhere else by the time the call
    /// happens: the caller cleared its own slot, and it is not a string, so the
    /// interned table is not quietly holding it either. The body then allocates
    /// until the heap has to be swept several times over before reading the
    /// capture back.
    #[test]
    fn a_capture_survives_a_collection_in_the_callee() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let cell = build.layout(
            "Cell",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        let body = build.lambda(
            "lambda",
            &[],
            &[
                Repr::Ref,
                Repr::Int,
                Repr::Int,
                Repr::Bool,
                Repr::Ref,
                Repr::Int,
                Repr::Int,
                Repr::Int,
            ],
            int,
            &[cell],
            vec![
                Inst::Int { dst: 2, value: 300 },
                Inst::Int { dst: 1, value: 0 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 3,
                    a: 1,
                    b: 2,
                },
                Inst::BranchFalse { cond: 3, to: 9 },
                Inst::Alloc {
                    dst: 4,
                    layout: cell,
                    len: Len::Count(64),
                },
                Inst::Clear {
                    slot: 4,
                    layout: cell,
                },
                Inst::Int { dst: 5, value: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 1,
                    a: 1,
                    b: 5,
                },
                Inst::Jump { to: 2 },
                Inst::Int { dst: 6, value: 0 },
                Inst::LoadElem {
                    dst: 7,
                    obj: 0,
                    index: 6,
                    layout: int,
                },
                Inst::Return { src: 7 },
            ],
        );
        let layout = closure_layout(&mut build, body, &[cell]);
        let none = build.args(&[]);
        let main = build.function(
            "main",
            &[],
            &[
                Repr::Ref,
                Repr::Ref,
                Repr::Int,
                Repr::Int,
                Repr::Int,
                Repr::Int,
            ],
            int,
            vec![
                Inst::Alloc {
                    dst: 1,
                    layout: cell,
                    len: Len::Count(1),
                },
                Inst::Int { dst: 3, value: 0 },
                Inst::Int {
                    dst: 4,
                    value: 4242,
                },
                Inst::StoreElem {
                    obj: 1,
                    index: 3,
                    src: 4,
                    layout: int,
                },
                Inst::Alloc {
                    dst: 0,
                    layout,
                    len: Len::Fixed,
                },
                Inst::Int {
                    dst: 2,
                    value: body.0 as i64,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 2,
                    layout: int,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 1,
                    src: 1,
                    layout: cell,
                },
                Inst::Clear {
                    slot: 1,
                    layout: cell,
                },
                Inst::CallClosure {
                    dst: 5,
                    closure: 0,
                    args: none,
                },
                Inst::Return { src: 5 },
            ],
        );
        let program = build.done();
        let mut machine = Machine::new(&program, 4096);
        assert_eq!(machine.run(main, &[], &budget()).unwrap(), vec![4242]);
        assert!(
            machine.collected().collections > 0,
            "the body is meant to allocate more than the heap holds"
        );
    }

    /// A closure that calls itself through its own capture nests until the
    /// reserved stack region is full, and stops there — with the message any
    /// other unbounded recursion gets, because it is the same event. No Rust
    /// frame is added per turn, so how deep this goes is `STACK_WORDS` and
    /// nothing else.
    #[test]
    fn a_closure_chain_is_bounded_by_the_stack_region() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        // What the closure captures is the closure, so the capture's layout
        // is one reference word rather than the callee's own `Int`.
        let held = build.word("captured", Repr::Ref);
        let none = build.args(&[]);
        // What it answers is never reached, because it never returns.
        let body = build.lambda(
            "lambda",
            &[],
            &[Repr::Ref, Repr::Int],
            int,
            &[held],
            vec![
                Inst::CallClosure {
                    dst: 1,
                    closure: 0,
                    args: none,
                },
                Inst::Return { src: 1 },
            ],
        );
        let layout = closure_layout(&mut build, body, &[held]);
        let main = build.function(
            "main",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout,
                    len: Len::Fixed,
                },
                Inst::Int {
                    dst: 1,
                    value: body.0 as i64,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: int,
                },
                // The closure captures itself, which is the shortest way to
                // write a call chain with no bound on it.
                Inst::StoreField {
                    obj: 0,
                    at: 1,
                    src: 0,
                    layout: held,
                },
                Inst::CallClosure {
                    dst: 2,
                    closure: 0,
                    args: none,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        let error = run(&program, main, &[]).unwrap_err();
        assert_eq!(error.message, "this call nests too deeply");
    }

    /// `fn main() { spin() }`, where `spin` is a closure whose body never
    /// leaves its loop.
    ///
    /// The caller is four instructions and the fifth enters the closure, so
    /// every safepoint after the first handful is one the closure's own frame
    /// is executing at.
    fn spinning_closure(build: &mut Build) -> FunctionId {
        let int = build.scalar(Repr::Int);
        let body = build.lambda(
            "lambda",
            &[],
            &[Repr::Int],
            int,
            &[],
            vec![Inst::Int { dst: 0, value: 0 }, Inst::Jump { to: 0 }],
        );
        let layout = closure_layout(build, body, &[]);
        let none = build.args(&[]);
        build.function(
            "main",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout,
                    len: Len::Fixed,
                },
                Inst::Int {
                    dst: 1,
                    value: body.0 as i64,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: int,
                },
                Inst::CallClosure {
                    dst: 2,
                    closure: 0,
                    args: none,
                },
                Inst::Return { src: 2 },
            ],
        )
    }

    /// The safepoint is a fact about the loop, not about which frame the loop
    /// is in: a run spinning inside a closure is cancelled within one stride
    /// exactly as one spinning in its entry is.
    #[test]
    fn a_cancelled_run_stops_at_a_safepoint_inside_a_closure() {
        let mut build = Build::default();
        let main = spinning_closure(&mut build);
        let program = build.done();
        let cancellation = Cancellation::new();
        let budget = crate::budget::Budget::with_cancellation(
            crate::budget::Limits::default(),
            cancellation.clone(),
        );
        cancellation.cancel();
        let mut machine = Machine::new(&program, 1 << 12);
        let error = machine.run(main, &[], &budget.meter()).unwrap_err();
        assert_eq!(error.message, "execution stopped: the run was cancelled");
        assert!(machine.instructions() <= SAFEPOINT_STRIDE + 1);
    }

    /// And fuel is charged at the same points, so a closure cannot spend a
    /// run's budget without the run noticing.
    #[test]
    fn fuel_runs_out_at_a_safepoint_inside_a_closure() {
        let mut build = Build::default();
        let main = spinning_closure(&mut build);
        let program = build.done();
        let budget = crate::budget::Budget::new(crate::budget::Limits {
            fuel: Some(2 * SAFEPOINT_STRIDE),
            ..Default::default()
        });
        let mut machine = Machine::new(&program, 1 << 12);
        let error = machine.run(main, &[], &budget.meter()).unwrap_err();
        assert_eq!(
            error.message,
            format!(
                "execution stopped: fuel budget of {} exhausted",
                2 * SAFEPOINT_STRIDE
            )
        );
        assert_eq!(machine.instructions(), 2 * SAFEPOINT_STRIDE);
    }

    /// The callee comes out of a heap object, so the machine checks that the
    /// object is one a call can be made through rather than reading its first
    /// word as a function id. Nothing a program can write reaches this; a
    /// lowering that did would otherwise push a frame for whichever function
    /// the object's first word happened to name.
    #[test]
    fn a_call_through_something_that_is_not_a_closure_is_refused() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int)]);
        let none = build.args(&[]);
        let main = build.function(
            "main",
            &[],
            &[Repr::Ref, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: point,
                    len: Len::Fixed,
                },
                Inst::CallClosure {
                    dst: 1,
                    closure: 0,
                    args: none,
                },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let error = run(&program, main, &[]).unwrap_err();
        assert_eq!(error.message, "`Point` is not callable");
    }

    /// A closure object whose captures are not the ones its callee reads is a
    /// lowering bug, and copying what it holds would fill the callee's capture
    /// slots from whatever follows the object in the heap.
    #[test]
    fn a_closure_whose_captures_do_not_match_its_callee_is_refused() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let none = build.args(&[]);
        let body = build.lambda(
            "lambda",
            &[],
            &[Repr::Int, Repr::Int],
            int,
            &[int, int],
            vec![Inst::Return { src: 0 }],
        );
        // One capture, against a callee that reads two.
        let layout = closure_layout(&mut build, body, &[int]);
        let main = build.function(
            "main",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout,
                    len: Len::Fixed,
                },
                Inst::Int {
                    dst: 1,
                    value: body.0 as i64,
                },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: int,
                },
                Inst::CallClosure {
                    dst: 2,
                    closure: 0,
                    args: none,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        let error = run(&program, main, &[]).unwrap_err();
        assert_eq!(
            error.message,
            "this closure and `t.lambda` disagree about its captures: 1 held, 2 read"
        );
    }

    /// `bump(var total)` adds to the caller's own binding rather than to a
    /// copy of it. A place is one word holding the address of that binding,
    /// and this is the whole of the mechanism.
    #[test]
    fn a_place_writes_the_callers_own_slot() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let unit = build.scalar(Repr::Unit);
        let place = build.scalar(Repr::Addr);
        let args = build.args(&[(1, place)]);
        let bump = build.function(
            "bump",
            &[place],
            &[Repr::Addr, Repr::Int, Repr::Int, Repr::Unit],
            unit,
            vec![
                Inst::Load {
                    dst: 1,
                    addr: 0,
                    layout: int,
                },
                Inst::Int { dst: 2, value: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 1,
                    a: 1,
                    b: 2,
                },
                Inst::Store {
                    addr: 0,
                    src: 1,
                    layout: int,
                },
                Inst::Unit { dst: 3 },
                Inst::Return { src: 3 },
            ],
        );
        let caller = build.function(
            "main",
            &[],
            &[Repr::Int, Repr::Addr, Repr::Unit],
            int,
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

    /// A field of a *heap object* is a load and a store; a field of an inline
    /// struct is not an instruction at all. This is the first kind, which is
    /// what a struct reaches by being the payload of an object.
    #[test]
    fn an_object_round_trips_through_its_fields() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        let f = build.function(
            "make",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Int],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: point,
                    len: Len::Fixed,
                },
                Inst::Int { dst: 1, value: 3 },
                Inst::StoreField {
                    obj: 0,
                    at: 0,
                    src: 1,
                    layout: int,
                },
                Inst::Int { dst: 1, value: 4 },
                Inst::StoreField {
                    obj: 0,
                    at: 1,
                    src: 1,
                    layout: int,
                },
                Inst::LoadField {
                    dst: 1,
                    obj: 0,
                    at: 0,
                    layout: int,
                },
                Inst::LoadField {
                    dst: 2,
                    obj: 0,
                    at: 1,
                    layout: int,
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
    ///
    /// The object reaches the slot it is read out of through a `Copy`, which
    /// is what leaves the machine to answer: `cove_lir::verify` refuses this
    /// statically wherever it can prove which layout a reference slot holds,
    /// and a slot written by a copy holds whatever the source held. Both
    /// checks are wanted — the static one catches the bug at lowering time,
    /// and this one catches it where the layout is not a static fact.
    #[test]
    fn a_field_past_the_object_is_refused() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let one = build.structure("One", &[("x", int)]);
        let held = build.layout("Held", Shape::Word(Repr::Ref));
        let f = build.function(
            "past",
            &[],
            &[Repr::Ref, Repr::Int, Repr::Ref],
            int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: one,
                    len: Len::Fixed,
                },
                Inst::Copy {
                    dst: 2,
                    src: 0,
                    layout: held,
                },
                Inst::LoadField {
                    dst: 1,
                    obj: 2,
                    at: 3,
                    layout: int,
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
    /// `Clear` the frame would hold every object it ever made; with it the
    /// heap stays flat, and this is the test that says so.
    #[test]
    fn clearing_a_slot_lets_the_collector_reclaim() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let cell = build.layout(
            "Cell",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        let f = build.function(
            "churn",
            &[int],
            &[Repr::Int, Repr::Int, Repr::Bool, Repr::Ref, Repr::Int],
            int,
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
                Inst::Clear {
                    slot: 3,
                    layout: cell,
                },
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
        assert_eq!(answer, vec![4000]);
        assert!(
            machine.collected().collections > 0,
            "the run should have had to collect"
        );
    }

    #[test]
    fn a_string_literal_is_allocated_once() {
        let mut build = Build::default().strings(&["hello"]);
        let bool_layout = build.scalar(Repr::Bool);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let f = build.function(
            "twice",
            &[],
            &[Repr::Ref, Repr::Ref, Repr::Bool],
            bool_layout,
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
        let bool_layout = build.scalar(Repr::Bool);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let f = build.function(
            "order",
            &[],
            &[Repr::Ref, Repr::Ref, Repr::Bool],
            bool_layout,
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

    /// A box carries the *layout* of what it holds in payload word 0, not a
    /// per-word `Repr`: erasure is where a value stops having a static width,
    /// so what the box has to record is the thing that says the width.
    #[test]
    fn a_box_answers_the_layout_it_holds_and_refuses_another() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let boolean = build.scalar(Repr::Bool);
        build.boxed();
        let good = build.function(
            "round-trip",
            &[int],
            &[Repr::Int, Repr::Ref, Repr::Int],
            int,
            vec![
                Inst::Box {
                    dst: 1,
                    src: 0,
                    layout: int,
                },
                Inst::Unbox {
                    dst: 2,
                    src: 1,
                    layout: int,
                },
                Inst::Return { src: 2 },
            ],
        );
        let wrong = build.function(
            "wrong-type",
            &[int],
            &[Repr::Int, Repr::Ref, Repr::Bool],
            boolean,
            vec![
                Inst::Box {
                    dst: 1,
                    src: 0,
                    layout: int,
                },
                Inst::Unbox {
                    dst: 2,
                    src: 1,
                    layout: boolean,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, good, &[7]).unwrap() as i64, 7);
        assert_eq!(
            run(&program, wrong, &[7]).unwrap_err().message,
            "this value is not of the type it is being read as"
        );
    }

    /// A boxed `Point` is a two-word payload rather than a reference to
    /// somewhere else again: the object holds the `LayoutId` in payload word
    /// 0 and the value's words after it, and the header's `len` is that
    /// value's width, because a `Boxed` layout cannot know it.
    ///
    /// So an `Unbox` at the wrong layout is refused for the same reason it is
    /// on a scalar — the word the box carries is a layout and the layouts do
    /// not match — and nothing about the width had to be guessed.
    #[test]
    fn a_box_holds_a_multiword_value_inline() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let point = build.structure("Point", &[("x", int), ("y", int)]);
        build.boxed();
        let round_trip = build.function(
            "round-trip",
            &[],
            &[Repr::Int, Repr::Int, Repr::Ref, Repr::Int, Repr::Int],
            point,
            vec![
                Inst::Int { dst: 0, value: 3 },
                Inst::Int { dst: 1, value: 4 },
                Inst::Box {
                    dst: 2,
                    src: 0,
                    layout: point,
                },
                Inst::Unbox {
                    dst: 3,
                    src: 2,
                    layout: point,
                },
                Inst::Return { src: 3 },
            ],
        );
        let width = build.function(
            "width",
            &[],
            &[Repr::Int, Repr::Int, Repr::Ref, Repr::Int],
            int,
            vec![
                Inst::Int { dst: 0, value: 3 },
                Inst::Int { dst: 1, value: 4 },
                Inst::Box {
                    dst: 2,
                    src: 0,
                    layout: point,
                },
                Inst::Len { dst: 3, obj: 2 },
                Inst::Return { src: 3 },
            ],
        );
        let wrong = build.function(
            "wrong-layout",
            &[],
            &[Repr::Int, Repr::Int, Repr::Ref, Repr::Int],
            int,
            vec![
                Inst::Int { dst: 0, value: 3 },
                Inst::Int { dst: 1, value: 4 },
                Inst::Box {
                    dst: 2,
                    src: 0,
                    layout: point,
                },
                Inst::Unbox {
                    dst: 3,
                    src: 2,
                    layout: int,
                },
                Inst::Return { src: 3 },
            ],
        );
        let program = build.done();
        assert_eq!(run_words(&program, round_trip, &[]).unwrap(), vec![3, 4]);
        assert_eq!(run(&program, width, &[]).unwrap(), 2);
        assert_eq!(
            run(&program, wrong, &[]).unwrap_err().message,
            "this value is not of the type it is being read as"
        );
    }

    #[test]
    fn a_switch_picks_a_case_and_falls_to_its_default() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let table = build.table(&[3, 5], 7);
        let f = build.function(
            "pick",
            &[int],
            &[Repr::Int, Repr::Int],
            int,
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
        let int = build.scalar(Repr::Int);
        build.program.host_ops.push(cove_lir::HostOp {
            resource: None,
            module: Arc::from("probe"),
            operation: Arc::from("double"),
            result: int,
        });
        let op = cove_lir::HostOpId(build.program.host_ops.len() as u32 - 1);
        let args = build.args(&[(0, int)]);
        build.function(
            "f",
            &[int],
            &[Repr::Int, Repr::Int],
            int,
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
        assert_eq!(machine.run(f, &[21], &budget()).unwrap(), vec![42]);
    }

    /// A string argument and a string answer, which is the case that
    /// allocates on both sides of the boundary.
    #[test]
    fn a_host_call_carries_strings_in_and_out() {
        let mut build = Build::default().strings(&["hey"]);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        build.program.host_ops.push(cove_lir::HostOp {
            resource: None,
            module: Arc::from("probe"),
            operation: Arc::from("shout"),
            result: str_layout,
        });
        let op = cove_lir::HostOpId(0);
        let args = build.args(&[(0, str_layout)]);
        let f = build.function(
            "f",
            &[],
            &[Repr::Ref, Repr::Ref],
            str_layout,
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
        let words = machine.run(f, &[], &budget()).unwrap();
        assert_eq!(
            String::from_utf8(machine.string_bytes(words[0])).unwrap(),
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
        let int = build.scalar(Repr::Int);
        build.program.host_ops.push(cove_lir::HostOp {
            resource: None,
            module: Arc::from("probe"),
            operation: Arc::from("double"),
            result: int,
        });
        let op = cove_lir::HostOpId(0);
        let args = build.args(&[(0, int)]);
        let f = build.function(
            "f",
            &[int],
            &[Repr::Int, Repr::Int],
            int,
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

    // ---- host resources ------------------------------------------------

    /// A host that issues a resource and takes one back.
    ///
    /// The two directions a `Repr::Host` word has to move, and nothing else:
    /// `open` answers a handle the way `files.open(path)` answers a
    /// `files.Reader`, and `read` is handed one back the way
    /// `files.read(reader)` is. It counts what it has opened, so two readers
    /// are two resources and `read` answering the id says which one arrived.
    #[derive(Default)]
    struct Vault {
        opened: std::sync::atomic::AtomicU64,
    }

    static VAULT_RESOURCES: &[cove_schema::ResourceSchema] = &[cove_schema::ResourceSchema {
        name: "Reader",
        task_safe: true,
        operations: &[],
    }];

    static VAULT_OPS: &[cove_schema::OperationSchema] = &[
        cove_schema::OperationSchema {
            name: "open",
            params: &[],
            variadic: false,
            result: cove_schema::HostType::Named("vault.Reader"),
            capability: "vault",
            effect: cove_schema::Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        cove_schema::OperationSchema {
            name: "read",
            params: &[cove_schema::HostType::Named("vault.Reader")],
            variadic: false,
            result: cove_schema::HostType::Int,
            capability: "vault",
            effect: cove_schema::Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ];

    impl crate::host::HostApi for Vault {
        fn module_schema(&self) -> cove_schema::ModuleSchema {
            cove_schema::ModuleSchema {
                name: "vault",
                capability: "vault",
                operations: VAULT_OPS,
                types: &[],
                resources: VAULT_RESOURCES,
            }
        }

        fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
            match op {
                // Counting upward and never reusing, which is the rule
                // ADR 0013 puts on an identity.
                "open" => {
                    let id = self
                        .opened
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                        + 1;
                    Ok(Value::from_resource(ResourceHandle::new(
                        "vault",
                        &VAULT_RESOURCES[0],
                        id,
                    )))
                }
                // The host recognises its own resource, which is the whole
                // point of the name crossing back: nothing about the word
                // reached here.
                "read" => Ok(Value::int(
                    args[0].resource().expect("the schema holds it").id as i64,
                )),
                other => Err(RuntimeError::new(format!("no `{other}` here"))),
            }
        }
    }

    fn vault() -> crate::host::HostRegistry {
        let mut hosts = crate::host::HostRegistry::new(crate::host::Grants::new(["vault"]));
        hosts.register(Box::new(Vault::default()));
        hosts
    }

    /// The two host operations, and the one-word family a handle occupies.
    fn vault_ops(build: &mut Build) -> (LayoutId, cove_lir::HostOpId, cove_lir::HostOpId) {
        let int = build.scalar(Repr::Int);
        let reader = build.word("vault.Reader", Repr::Host);
        build.program.host_ops.push(cove_lir::HostOp {
            resource: None,
            module: Arc::from("vault"),
            operation: Arc::from("open"),
            result: reader,
        });
        build.program.host_ops.push(cove_lir::HostOp {
            resource: None,
            module: Arc::from("vault"),
            operation: Arc::from("read"),
            result: int,
        });
        let ops = build.program.host_ops.len() as u32;
        (
            reader,
            cove_lir::HostOpId(ops - 2),
            cove_lir::HostOpId(ops - 1),
        )
    }

    /// A host operation whose result is a resource writes the word, not a
    /// boxed value: the answer is a value location of one `Repr::Host` slot,
    /// and `Inst::CallHost` copies its words into the frame exactly as it
    /// does for an `Int`. Nothing about the instruction knows a resource is
    /// different.
    #[test]
    fn a_host_call_answering_a_resource_writes_the_word() {
        let mut build = Build::default();
        let (reader, open, _) = vault_ops(&mut build);
        let args = build.args(&[]);
        let f = build.function(
            "f",
            &[],
            &[Repr::Host],
            reader,
            vec![
                Inst::CallHost {
                    dst: 0,
                    op: open,
                    args,
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        let hosts = vault();
        let mut machine = Machine::with_hosts(&program, 1 << 12, Some(&hosts));

        let answer = machine.run(f, &[], &budget()).unwrap();
        assert_eq!(
            answer.len(),
            1,
            "a handle is a name, and a name is one word"
        );
        assert_eq!(
            machine
                .resource(answer[0])
                .map(|handle| handle.to_string())
                .as_deref(),
            Some("vault.Reader#1"),
            "the word indexes the run's table, and the table holds the name"
        );
        assert_eq!(
            machine.allocated_words(),
            0,
            "a resource is not an object, so nothing was allocated to hold one"
        );
    }

    /// A resource goes back to the host that issued it, by the name it was
    /// issued under. The host is what recognises it; the word never left.
    #[test]
    fn a_resource_goes_back_to_the_host_that_issued_it() {
        let mut build = Build::default();
        let (reader, open, read) = vault_ops(&mut build);
        let int = build.scalar(Repr::Int);
        let none = build.args(&[]);
        let one = build.args(&[(0, reader)]);
        let f = build.function(
            "f",
            &[],
            &[Repr::Host, Repr::Host, Repr::Int],
            int,
            vec![
                Inst::CallHost {
                    dst: 0,
                    op: open,
                    args: none,
                },
                // A second resource, so that an answer of `1` is the first
                // reader rather than whatever the table happened to hold.
                Inst::CallHost {
                    dst: 1,
                    op: open,
                    args: none,
                },
                Inst::CallHost {
                    dst: 2,
                    op: read,
                    args: one,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        let hosts = vault();
        let mut machine = Machine::with_hosts(&program, 1 << 12, Some(&hosts));
        assert_eq!(machine.run(f, &[], &budget()).unwrap(), vec![1]);
    }

    /// A frame holding a resource across a collection keeps it, and the
    /// collector never sees it.
    ///
    /// Both halves are the claim. The static one is that `Function::refs` —
    /// which is `RefMap::of` the frame's `Repr`s — does not name the `Host`
    /// slot, so the one pass the collector makes over a frame does not read
    /// it. The dynamic one is that the run still gets the right resource back
    /// afterwards: the word is untouched, the table is not swept, and the
    /// handle it indexes is the one the host issued.
    #[test]
    fn a_resource_in_a_frame_survives_a_collection_and_is_not_a_root() {
        let mut build = Build::default();
        let (reader, open, read) = vault_ops(&mut build);
        let int = build.scalar(Repr::Int);
        let cell = build.layout(
            "Cell",
            Shape::Elements {
                elem: int,
                growable: false,
            },
        );
        let none = build.args(&[]);
        let one = build.args(&[(0, reader)]);
        let f = build.function(
            "f",
            &[],
            &[Repr::Host, Repr::Ref, Repr::Ref, Repr::Int],
            int,
            vec![
                Inst::CallHost {
                    dst: 0,
                    op: open,
                    args: none,
                },
                Inst::Alloc {
                    dst: 1,
                    layout: cell,
                    len: Len::Count(600),
                },
                // The cell's last use. A second one fits only if this one is
                // reclaimed, which is what makes the collection happen with
                // the resource live in slot 0.
                Inst::Clear {
                    slot: 1,
                    layout: cell,
                },
                Inst::Alloc {
                    dst: 2,
                    layout: cell,
                    len: Len::Count(600),
                },
                Inst::CallHost {
                    dst: 3,
                    op: read,
                    args: one,
                },
                Inst::Return { src: 3 },
            ],
        );
        let program = build.done();

        // The static half: the collector's one question about a slot, asked
        // of the map it actually reads.
        let refs = &program.function(f).refs;
        assert!(!refs.is_ref(0), "a host word is not a root");
        assert_eq!(refs.iter().collect::<Vec<_>>(), vec![1, 2]);

        let hosts = vault();
        let mut machine = Machine::with_hosts(&program, 1000, Some(&hosts));
        assert_eq!(machine.run(f, &[], &budget()).unwrap(), vec![1]);
        assert!(
            machine.collected().collections > 0,
            "the second cell only fits after the first is reclaimed"
        );
    }

    /// A run that will not stop on its own is stopped by its budget, and the
    /// stride is what bounds how long that takes.
    #[test]
    fn a_cancelled_run_stops_at_a_safepoint() {
        let mut build = Build::default();
        let int = build.scalar(Repr::Int);
        let f = build.function(
            "spin",
            &[],
            &[Repr::Int],
            int,
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
