//! The runtime's half of [ADR 0055]'s native boundary.
//!
//! `cove-native` emits machine code and calls back through a table of function
//! pointers. This module is what those pointers point at, and what enters the
//! code in the first place. It is the other side of the inversion that crate's
//! documentation describes: the dependency edge runs one way, from here to
//! there, and the calls run both.
//!
//! # It is the experiment's half, not the tier's
//!
//! ADR 0055's native tier is not selectable yet and nothing here makes it so.
//! `cove run` does not reach this module, [`Vm::invoke`](crate::Vm::invoke) does
//! not consult it, and no default build compiles a code generator — which is why
//! this file names [`Entry`] and never names a `Jit`. What it exists for is the
//! comparison `crates/cove-bench/src/bin/native_compare.rs` runs: two code
//! generators over identical optimized IR, on *real* data, with the VM beside
//! them as the oracle. That needs the real heap and real calls, and those are
//! here.
//!
//! # What compiled code is allowed to do, and what it hands back
//!
//! Three things, and every one of them is a helper rather than emitted code,
//! because ADR 0055 says "Runtime operations whose correctness already lives in
//! Rust … remain runtime helpers initially":
//!
//! - **a safepoint** — [`safepoint`] below, which is
//!   [`Machine::safepoint`] and therefore [ADR 0040]'s three-step order in that
//!   order: cancellation and task-local stops, then fuel and deadline
//!   accounting, then the collector rendezvous;
//! - **a call** — [`call`] below, which opens the callee's frame with
//!   `encoded::open_frame` and runs it on whichever tier it is on. Compiled code
//!   does not open a frame, copy an argument or choose a tier;
//! - **leaving** — a [`Raise`] the compiled code names and this builds, which is
//!   how a `+` that overflowed in machine code produces the *same sentence* the
//!   encoded tier's does.
//!
//! Everything else — an `Array` element, a `String` byte, an object's length, an
//! enum's switch — is emitted code, and that is deliberate: an operation that is
//! one identical helper call in both arms cannot tell the two code generators
//! apart, so a comparison over a subset made entirely of helper calls would
//! measure nothing.
//!
//! # Why the aliasing discipline is written down
//!
//! A helper is reached as `extern "C" fn(*mut NativeCtx, …)`, finds the runtime
//! through [`NativeCtx::host`], and may call *back* into compiled code — a
//! native function calling a native function goes out through the call helper
//! and in through an entry point. So a helper that held a `&mut` to the bridge
//! across that call would have two live `&mut` to one place, which is undefined
//! behaviour whether or not the second one is used.
//!
//! The rule below is therefore: **a helper reaches the bridge through a raw
//! pointer, and every borrow it takes ends before compiled code runs again.**
//! Each one is written in a block of its own for that reason, and the blocks are
//! not tidiness.
//!
//! [ADR 0040]: ../../../../../docs/adr/0040-a-bound-outlives-its-backend.md
//! [ADR 0055]: ../../../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md

use std::ffi::c_void;

use cove_ir::{ArgsId, FunctionId, Slot, StrId};
use cove_native::{Entry, NativeCtx, NativeHelpers, Outcome, Raise};

use super::{divided_by_zero, null_object, overflowed, Frame, Machine, Overflow};
use crate::budget::Meter;
use crate::error::RuntimeError;

/// Which functions have machine code, and where each one's starts.
///
/// ADR 0055's `Program + FunctionId -> encoded entry | native entry` table, as
/// the one question the runtime asks of it. It is a trait rather than a map
/// because the map belongs to whoever compiled the code: a `Jit` owns the pages
/// its entries point into, so it — and not this crate — has to outlive them.
///
/// `None` is not a failure. It is the ordinary answer, and it means the callee
/// runs on the encoded VM, which is a complete execution path and not a
/// fallback.
pub trait Tiered {
    /// The compiled entry point of `id`, if it has one.
    fn entry(&self, id: FunctionId) -> Option<Entry>;
}

/// Nothing is compiled.
///
/// What a run with no code generator consults, so that the native path can be
/// written, compiled and read in a build that has no executable memory at all.
pub struct NothingCompiled;

impl Tiered for NothingCompiled {
    fn entry(&self, _: FunctionId) -> Option<Entry> {
        None
    }
}

/// How a run divided between the two tiers.
///
/// ADR 0055: "The run reports how many calls and functions used each tier, so a
/// benchmark cannot present a mixed run as fully native." This is that report,
/// for calls; how many *functions* were compiled is the compiler's own count and
/// belongs beside it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tiers {
    /// Calls entered through a compiled entry point, the outermost included.
    pub native: u64,
    /// Calls run by the encoded dispatch loop.
    pub encoded: u64,
}

/// What one native call chain shares: the runtime, the tier table, and the heap
/// table compiled code reads.
///
/// Reached from a helper as `*mut Bridge` through [`NativeCtx::host`]. See the
/// module's note on why that is a raw pointer and stays one.
struct Bridge<'m, 'a> {
    /// The machine, as a raw pointer *on purpose*.
    ///
    /// A helper opens a frame, which needs `&mut Machine`, and may then enter
    /// compiled code which reaches a helper that needs it again. Holding a
    /// `&mut` across that would be two live `&mut` to one machine, so the borrow
    /// is taken where it is used and dropped before anything else runs.
    machine: *mut Machine<'a>,
    budget: &'m Meter,
    entries: &'m dyn Tiered,
    /// One pointer per committed heap chunk, which is
    /// [`NativeCtx::chunks`]. Owned by the [`Session`] and borrowed here.
    ///
    /// **Its capacity is reserved for the whole heap and it never reallocates**,
    /// and that is load-bearing rather than an optimisation: every `NativeCtx` in
    /// a native call chain holds `chunks.as_ptr()`, and a `Vec` that grew would
    /// leave every one of them but the innermost pointing at a freed table.
    /// Reserving the spine's length costs one allocation of eight bytes per
    /// 64-KiB chunk the heap *could* hold, and makes the address a constant.
    ///
    /// It lives in the session rather than here for a measured reason: building
    /// it is an allocation and a walk, and a bridge is made once per *call*.
    /// Built per call, the covefmt scenario measured the allocator rather than
    /// the code — 4 KiB reserved and cleared for every one of twenty thousand
    /// calls. A raw pointer rather than a `&mut` for [`Bridge::machine`]'s
    /// reason.
    chunks: *mut Vec<*mut u64>,
    /// The error a helper is leaving with.
    ///
    /// A callee's failure is a whole `RuntimeError` — a span, a rule, a call
    /// chain — and compiled code has nowhere to put one: [`Raise`] names errors
    /// and carries at most two numbers. So the helper keeps it here and answers
    /// [`Raise::Called`], and whoever entered the outermost compiled function
    /// takes it back out.
    left: Option<RuntimeError>,
    tiers: Tiers,
}

impl<'m, 'a> Bridge<'m, 'a> {
    fn new(
        machine: &mut Machine<'a>,
        budget: &'m Meter,
        entries: &'m dyn Tiered,
        chunks: &mut Vec<*mut u64>,
    ) -> Self {
        machine.mem.chunk_bases(chunks);
        Bridge {
            machine,
            budget,
            entries,
            chunks,
            left: None,
            tiers: Tiers::default(),
        }
    }

    /// The chunk table, as compiled code is given it.
    ///
    /// # Safety
    ///
    /// The session that owns the table outlives every bridge over it.
    unsafe fn table(&self) -> *const *mut u64 {
        (*self.chunks).as_ptr()
    }
}

/// Re-publishes both pointers compiled code re-loads, after anything that could
/// have moved either.
///
/// The stack's `Vec` reallocates when a frame is pushed, and the heap commits a
/// chunk when it grows — so a helper that ran a callee has to hand back the
/// current stack pointer and the current chunk table. The table's *address* does
/// not change, for the reason [`Bridge::chunks`] gives, but its contents do.
///
/// # Safety
///
/// `ctx` and `host` are the pointers the helper was reached with.
unsafe fn republish(ctx: *mut NativeCtx, host: *mut Bridge<'_, '_>) {
    let machine = (*host).machine;
    let chunks = (*host).chunks;
    // Extended rather than rebuilt, and that is a fact about the heap rather
    // than a shortcut: a committed chunk is never replaced and never moves, so
    // every entry already in the table is still right and the only thing that
    // can have changed is that there are more of them. See `Words::bases`.
    (*machine).mem.chunk_bases(&mut *chunks);
    (*ctx).words = (*machine).mem.words_ptr();
    (*ctx).chunks = (*chunks).as_ptr();
}

/// The safepoint helper: [ADR 0040]'s three steps, in that order, and none of
/// them emitted.
///
/// `work` is the static IR-instruction count of the blocks executed since the
/// last safepoint. It is added to [`Machine::bulk_work`] and *not* to
/// `Machine::instructions`, which is ADR 0055's "It must not silently label
/// statically counted IR in native blocks as dispatched instructions": the
/// opcode counter stays a count of opcodes the dispatch loop dispatched, and
/// what fuel is stated in is `Machine::work`, which is the sum.
///
/// # Safety
///
/// `ctx` is the pointer the entry point was called with, and `ctx.host` is the
/// [`Bridge`] that entered it.
unsafe extern "C" fn safepoint(ctx: *mut NativeCtx, pc: u32, work: u64) -> bool {
    let host = (*ctx).host.cast::<Bridge>();
    let machine = (*host).machine;
    let budget = (*host).budget;
    let stopped = {
        let machine = &mut *machine;
        machine.bulk_work += work;
        // The frame's program counter has to be current before the collector
        // walks it, which is what `sync` is for and why it is here rather than
        // in emitted code.
        machine.sync(pc as usize);
        let id = machine
            .frames
            .last()
            .expect("a native frame is executing")
            .function;
        machine.safepoint(budget, id, pc as usize).err()
    };
    match stopped {
        None => {
            republish(ctx, host);
            true
        }
        Some(error) => {
            (*host).left = Some(error);
            false
        }
    }
}

/// The call helper: one `Inst::Call`, handed over whole.
///
/// See [`cove_native::CallFn`] for the signature and
/// [`cove_native::abi`](cove_native::abi) for why the frame is not opened in
/// emitted code. What happens here is exactly `encoded.rs`'s `CALL` arm and its
/// `RETURN`, split at the point the two tiers differ:
///
/// 1. the unpaid work is charged, because a call may allocate and an allocation
///    may collect — so this is a safepoint whether the callee reaches one or not;
/// 2. `open_frame` opens the callee's frame and copies its arguments, which is
///    the *same function* the dispatch loop calls;
/// 3. the callee runs on whichever tier it is on;
/// 4. the answer is copied into the caller's `dst` at the callee's return width,
///    which is the half of `encoded.rs`'s `RETURN` arm that belongs to the
///    caller.
///
/// # Safety
///
/// As [`safepoint`]. `base`, `callee`, `args` and `dst` were checked by
/// `cove_native`'s subset predicate before a byte of code was emitted.
unsafe extern "C" fn call(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    callee: u32,
    args: u32,
    dst: u32,
) -> u32 {
    let host = (*ctx).host.cast::<Bridge>();
    let machine = (*host).machine;
    let budget = (*host).budget;
    let callee = FunctionId(callee);

    // A call is a safepoint. The work went into `pending_work` before the
    // hand-over and is charged here, so nothing is counted twice and nothing is
    // dropped if the safepoint stops the run.
    let work = (*ctx).pending_work;
    (*ctx).pending_work = 0;

    // Step 1 and step 2, in one borrow that ends before anything can run Cove
    // code again. See the module's aliasing note.
    let opened = {
        let machine = &mut *machine;
        machine.bulk_work += work;
        machine.sync(pc as usize);
        let caller = machine
            .frames
            .last()
            .copied()
            .expect("a native frame is executing");
        debug_assert_eq!(
            machine.mem.stack_index(caller.base) as u64,
            base,
            "compiled code and the frame stack disagree about which frame is calling"
        );
        let span = machine.span(caller.function, pc as usize);
        machine
            .safepoint(budget, caller.function, pc as usize)
            .and_then(|()| {
                super::encoded::open_frame(
                    machine,
                    budget,
                    caller.base,
                    span,
                    callee,
                    ArgsId(args),
                    None,
                )
            })
            .map(|callee_base| (caller.base, callee_base))
    };
    let (caller_base, callee_base) = match opened {
        Ok(bases) => bases,
        Err(error) => {
            (*host).left = Some(error);
            return Outcome::Raised.abi();
        }
    };

    // The callee's frame joins the stack before it runs, whichever tier runs it:
    // a frame is what roots the callee's reference slots, and `open_frame` has
    // already written the arguments into them.
    {
        let machine = &mut *machine;
        machine.frames.push(Frame {
            function: callee,
            base: callee_base,
            pc: 0,
            dst: dst as Slot,
        });
    }

    let entry = (*host).entries.entry(callee);
    let answered = match entry {
        Some(entry) => {
            (*host).tiers.native += 1;
            enter(host, entry, callee, callee_base)
        }
        None => {
            (*host).tiers.encoded += 1;
            let machine = &mut *machine;
            let floor = machine.frames.len() - 1;
            match machine.code() {
                // The dispatch loop pops the callee's frame and its words on the
                // way out, and hands back the answer — which is `encoded.rs`'s
                // `RETURN` arm reaching `None` for its caller, and is the same
                // shape `Machine::run` is handed an answer in.
                Ok(code) => machine.drive_from(&code, budget, floor),
                Err(error) => Err(error),
            }
        }
    };

    let outcome = match answered {
        Ok(words) => {
            let machine = &mut *machine;
            for (at, word) in words.iter().enumerate() {
                machine.mem.set_slot(caller_base, dst + at as u32, *word);
            }
            Outcome::Returned.abi()
        }
        Err(error) => {
            (*host).left = Some(error);
            Outcome::Raised.abi()
        }
    };
    // Whatever the callee did, the pointers compiled code cached before the
    // hand-over are not to be used after it.
    republish(ctx, host);
    outcome
}

/// Runs the frame on top of the stack as compiled code, and answers its words.
///
/// The frame is already pushed and its arguments are already in it, which is what
/// makes this the same entry for the outermost call and for a native-to-native
/// one: ADR 0055's "No parameters are passed in registers."
///
/// # Safety
///
/// `host` is a live [`Bridge`], `entry` is a finalized entry point of a function
/// `cove_native` compiled for `callee`, and `base` is that call's frame.
unsafe fn enter(
    host: *mut Bridge<'_, '_>,
    entry: Entry,
    callee: FunctionId,
    base: u64,
) -> Result<Vec<u64>, RuntimeError> {
    let machine = (*host).machine;
    let index = {
        let machine = &mut *machine;
        // A word index rather than an address, because the `Vec` behind the words
        // moves and the index does not. See `cove_native::abi`.
        machine.mem.stack_index(base) as u64
    };
    let mut ctx = {
        let machine = &mut *machine;
        NativeCtx::new(host.cast::<c_void>(), machine.mem.words_ptr()).over_heap((*host).table())
    };
    // Safety: the context is this call's, the frame at `index` is the callee's,
    // and the code was emitted for exactly `Entry`'s shape.
    let outcome = entry(&mut ctx, index);

    let machine = &mut *machine;
    // Pending work is charged on every exit — a return, a raise and a stop
    // alike, which is ADR 0055's own requirement.
    machine.bulk_work += ctx.pending_work;
    ctx.pending_work = 0;
    match outcome {
        Outcome::Returned => {
            let width = machine.width(machine.program.function(callee).returns);
            let words = machine
                .mem
                .read_words(base + u64::from(ctx.return_slot), width);
            machine.frames.pop();
            machine.mem.pop_frame(base);
            Ok(words)
        }
        Outcome::Raised => Err(raised(machine, (*host).left.take(), callee, &ctx)),
        Outcome::Stopped => Err((*host).left.take().unwrap_or_else(|| {
            RuntimeError::new("a native safepoint stopped the run and said nothing about why")
        })),
    }
}

/// The runtime error a compiled function named.
///
/// This is the half of the boundary `cove_native`'s [`Raise`] exists for: that
/// crate names the operation and the kind, and the sentence is built *here*, out
/// of the same `overflowed`, `divided_by_zero` and `null_object` the encoded tier
/// calls. A second copy of "`Int` addition overflowed" in a code generator would
/// be a second source of truth for a rule of the language, and the differential
/// corpus is an expensive place to find out the two had drifted.
///
/// The span is `fail!`'s: `machine.sync(pc)` and then the instruction's own span,
/// which is why [`NativeCtx::raise_pc`] is carried at all.
fn raised(
    machine: &mut Machine<'_>,
    left: Option<RuntimeError>,
    id: FunctionId,
    ctx: &NativeCtx,
) -> RuntimeError {
    // A callee's error is already whole, and it already has its own span.
    if let Some(error) = left {
        return error;
    }
    let pc = ctx.raise_pc as usize;
    machine.sync(pc);
    let error = match ctx.raise() {
        Some(Raise::AddOverflowed) => overflowed("addition"),
        Some(Raise::SubOverflowed) => overflowed("subtraction"),
        Some(Raise::MulOverflowed) => overflowed("multiplication"),
        Some(Raise::DivOverflowed) => overflowed("division"),
        Some(Raise::RemOverflowed) => overflowed("remainder"),
        Some(Raise::DurationOverflowed) => overflowed("duration arithmetic"),
        Some(Raise::DividedByZero) => divided_by_zero("division"),
        Some(Raise::RemainderByZero) => divided_by_zero("remainder"),
        Some(Raise::Trapped) => {
            RuntimeError::new(machine.program.string(StrId(ctx.raise_detail)).to_string())
        }
        Some(Raise::NullObject) => null_object(),
        // `Machine::element`'s sentence and its rule, word for word.
        Some(Raise::IndexOutOfRange) => RuntimeError::new(format!(
            "index {} is outside a collection of {}",
            ctx.raise_a, ctx.raise_b
        ))
        .with_rule("An index outside a collection is a broken invariant."),
        // `encoded.rs`'s `BYTE_AT` refusal, which names the last legal offset and
        // so does the subtraction here rather than in emitted code.
        Some(Raise::ByteOffset) => RuntimeError::new(format!(
            "`byteAt` is `{}`, and a byte offset into this string is 0 to {}",
            ctx.raise_a,
            ctx.raise_b - 1
        )),
        // `Raise::Called` with nothing stashed, or a code no code generator
        // emits. Both are this crate's mistake rather than the program's, and
        // both are reported rather than papered over.
        Some(Raise::Called) | None => RuntimeError::new(format!(
            "compiled code raised `{}`, which the runtime has no error for",
            ctx.raise_code
        )),
    };
    error.at(machine.span(id, pc))
}

/// One function, entered many times over arguments that were converted once.
///
/// The shape the comparison harness needs and the reason this is not simply a
/// second `Vm::invoke`: `covefmt.wantsASpaceBetween` is called three hundred
/// thousand times over one `String` and one `Array<Token>`, and converting a
/// public [`Value`](crate::Value) into words allocates a fresh object every time.
/// So the references are converted once and **held in a frame**, which is what
/// keeps them alive: a `Repr::Ref` slot of a live frame is a root, and a raw
/// `u64` a benchmark happens to be holding is not.
///
/// That holder frame is the whole design. It stays at the bottom of the stack for
/// the session's life, every call goes on top of it, and the session is what owns
/// the borrow of the machine so that it cannot be dropped while the words are
/// still being used.
pub struct Session<'v, 'a> {
    machine: &'v mut Machine<'a>,
    budget: &'v Meter,
    id: FunctionId,
    /// The words the arguments became, in parameter order.
    arguments: Vec<u64>,
    /// The holder frame's base. Its slots `[0, arguments.len())` hold
    /// `arguments`, which is what roots the objects they name.
    holder: u64,
    tiers: Tiers,
    /// The heap's chunk table, reserved once. See [`Bridge::chunks`].
    chunks: Vec<*mut u64>,
}

impl<'v, 'a> Session<'v, 'a> {
    /// Prepares `id` to be called with `arguments` as its parameter words.
    ///
    /// The stack is cleared exactly as [`Machine::run`] clears it, and for the
    /// same reason: a previous call that was stopped where it stood left its
    /// frames standing.
    pub(crate) fn open(
        machine: &'v mut Machine<'a>,
        budget: &'v Meter,
        id: FunctionId,
        arguments: Vec<u64>,
    ) -> Result<Session<'v, 'a>, RuntimeError> {
        let function = machine.program.function(id);
        let span = function.span;
        let size = function.frame_size();
        let chunks = Vec::with_capacity(machine.mem.chunk_capacity());
        machine.literals().map_err(|error| error.at(span))?;
        machine.give_cells_back(0);
        machine.frames.clear();
        machine.mem.reset_stack();
        machine.temps.clear();
        let holder = machine
            .mem
            .push_frame(size)
            .map_err(|Overflow| machine.too_deep(span))?;
        for (slot, word) in arguments.iter().enumerate() {
            machine.mem.set_slot(holder, slot as u32, *word);
        }
        machine.frames.push(Frame {
            function: id,
            base: holder,
            pc: 0,
            dst: 0,
        });
        Ok(Session {
            machine,
            budget,
            id,
            arguments,
            holder,
            tiers: Tiers::default(),
            chunks,
        })
    }

    /// The words the arguments became, in parameter order.
    ///
    /// A caller varies the scalars among them between calls — which `l` and `r`
    /// of `wantsASpaceBetween` are — and leaves the references alone.
    pub fn arguments(&self) -> &[u64] {
        &self.arguments
    }

    /// One call, on the tier `entries` decides, answering the callee's words.
    ///
    /// `entries` answering `None` for this function is how a caller asks for the
    /// encoded tier; [`NothingCompiled`] is the one that always does.
    ///
    /// A fresh frame goes on top of the holder every time, exactly as a real call
    /// would: `push_frame` zeroes it, so no word of a previous call is readable
    /// here and nothing a previous call left keeps an object alive.
    pub fn call(
        &mut self,
        entries: &dyn Tiered,
        arguments: &[u64],
    ) -> Result<Vec<u64>, RuntimeError> {
        let function = self.machine.program.function(self.id);
        let span = function.span;
        let size = function.frame_size();
        let base = self
            .machine
            .mem
            .push_frame(size)
            .map_err(|Overflow| self.machine.too_deep(span))?;
        for (slot, word) in arguments.iter().enumerate() {
            self.machine.mem.set_slot(base, slot as u32, *word);
        }
        self.machine.frames.push(Frame {
            function: self.id,
            base,
            pc: 0,
            dst: 0,
        });
        let floor = self.machine.frames.len() - 1;
        debug_assert_eq!(
            self.machine.frames.first().map(|frame| frame.base),
            Some(self.holder),
            "the holder frame is still at the bottom, so the references are still rooted"
        );
        let answer = match entries.entry(self.id) {
            Some(entry) => {
                let mut bridge = Bridge::new(self.machine, self.budget, entries, &mut self.chunks);
                bridge.tiers.native += 1;
                let held: *mut Bridge = &mut bridge;
                // Safety: `held` is this stack frame's bridge and outlives the
                // call below; `entry` was compiled for `self.id`, whose frame is
                // the one just pushed.
                let answer = unsafe { enter(held, entry, self.id, base) };
                // Added rather than assigned: a bridge counts one call chain and
                // a session makes many, and `= bridge.tiers` reported the last
                // chain's count as the session's.
                self.tiers.native += bridge.tiers.native;
                self.tiers.encoded += bridge.tiers.encoded;
                answer
            }
            None => {
                self.tiers.encoded += 1;
                let code = self.machine.code()?;
                self.machine.drive_from(&code, self.budget, floor)
            }
        };
        if answer.is_err() {
            // A failure leaves its frames standing, which is what an error's
            // call chain is read out of — but this session is going to be called
            // again, so the stack is put back to the holder here.
            self.machine.frames.truncate(floor);
            self.machine.mem.pop_frame(base);
        }
        debug_assert_eq!(
            self.machine.frames.len(),
            floor,
            "a call left the stack where it found it"
        );
        answer
    }

    /// How the calls made through this session divided between the two tiers.
    pub fn tiers(&self) -> Tiers {
        self.tiers
    }

    /// How many instructions the *encoded* tier has dispatched over this
    /// session.
    ///
    /// `Machine::instructions`, which counts dispatched opcodes and nothing
    /// else. ADR 0055: a native block's statically counted IR work "must not
    /// silently label statically counted IR in native blocks as dispatched
    /// instructions", so this does not grow when compiled code runs and a
    /// reader may take the difference between two tiers' figures as exactly
    /// that — the instructions the VM dispatched, and no others.
    pub fn instructions(&self) -> u64 {
        self.machine.instructions
    }

    /// How many collections have run over this session.
    ///
    /// The one number that says whether "the frame is canonical and the existing
    /// root walk works" was *exercised* rather than merely argued: a scenario
    /// that never collected has not tested it, and a report that did not say so
    /// would be claiming it had.
    pub fn collections(&self) -> u64 {
        self.machine.collected.collections
    }

    /// How many objects this session's run has handed out, reuse counted each
    /// time.
    ///
    /// What a caller that needs a collection to happen asks: which calls
    /// allocated. A native tier that never allocates cannot be the thing that
    /// triggers a collection, so finding the calls that do is how the collector
    /// path gets exercised on purpose rather than by luck.
    pub fn allocations(&self) -> u64 {
        self.machine.mem.allocations()
    }
}

/// The helpers, as the table a code generator binds.
///
/// One table, and both arms are given the same one: a helper that differed
/// between two code generators would be a difference in the *runtime* presented
/// as a difference in the code they emit.
pub fn helpers() -> NativeHelpers {
    NativeHelpers { safepoint, call }
}
