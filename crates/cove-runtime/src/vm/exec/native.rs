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
use std::sync::atomic::{AtomicU64, Ordering};

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

/// Where a call's answer goes: the run of words its caller named for it.
///
/// [ADR 0057]: the destination of a call is settled by the lowering, before the
/// call is made — it is `Inst::Call`'s `dst` — so there is nothing for a return
/// path to decide and nothing for it to allocate. It is carried **as two
/// indices and never as a pointer**, for the reason `cove_native::abi` gives
/// about `base`: the stack's `Vec` reallocates under a `push_frame`, so a
/// pointer at the destination taken before the callee's frame was opened would
/// be dangling by the time the callee returned. A frame's linear address is
/// relative to a segment origin that is fixed for the life of the task, and a
/// slot within it is a number, so neither moves.
///
/// [ADR 0057]: ../../../../../docs/adr/0057-a-native-call-returns-into-the-destination-its-caller-named.md
#[derive(Clone, Copy)]
struct Destination {
    /// The *caller's* frame, as `Memory` addresses one.
    base: u64,
    /// The slot of it the answer's words begin at.
    slot: u32,
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
/// 4. the answer is published into the caller's `dst` at the callee's return
///    width, which is the half of `encoded.rs`'s `RETURN` arm that belongs to
///    the caller. [`Destination`] is that `dst`, handed *down* rather than
///    applied afterwards: a native callee is given the two indices and writes
///    the words itself, before its frame is removed, and nothing is allocated to
///    carry them. An encoded callee still hands its answer back as a `Vec`,
///    because that is the convention of the tier that runs it, and the helper
///    publishes that.
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
    call_body::<0>(ctx, base, pc, callee, args, dst)
}

/// The call helper's body, over a compile-time mask of *extra* work.
///
/// `MASK == 0` is the helper above and nothing else: every `MASK & …` below is
/// a constant, so a mask of zero folds each one away and the production path is
/// this function with no ablation in it at all. That is deliberate and it is the
/// discipline [issue #365](https://github.com/myuon/cove/issues/365) asks for —
/// the baseline row of the decomposition is *the same function* the runtime
/// uses, not a copy of it that might have drifted.
///
/// # Why every variant adds work rather than taking it away
///
/// A decomposition wants the cost of one component. Two ways to get it: leave
/// the component out and difference, or do it **twice** and difference. Every
/// variant here is the second kind, and the reason is correctness: a call whose
/// frame was not zeroed, whose arguments were not copied or whose depth was not
/// admitted computes a different program, and a performance number from a run
/// that computed the wrong thing is worse than no number. Doing a component
/// twice is idempotent for every one of them — a second `span` lookup answers
/// the same span, a second `admit_frame` the same admission, a second argument
/// copy the same words, a second `pop_frame` the same truncation — so every
/// variant answers exactly what the production helper answers, and the harness
/// checks that against the VM on every call.
///
/// What it costs is that each figure is a **lower bound**: the second instance
/// of a component runs with the first one's cache lines and branch history
/// already warm. Where the number matters that is said again beside it.
///
/// # Safety
///
/// As [`call`].
#[inline(always)]
unsafe fn call_body<const MASK: u64>(
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

    if MASK & ablate::AGAIN_HOP != 0 {
        // One more six-argument C-ABI indirect call and its return, into a
        // helper that does nothing: the *shape* of the tier hop with none of the
        // runtime in it. See [`nothing`].
        let hop = std::hint::black_box(nothing as unsafe extern "C" fn(_, _, _, _, _, _) -> u32);
        std::hint::black_box(hop(ctx, base, pc, callee.0, args, dst));
    }
    if MASK & ablate::AGAIN_MEDIATION != 0 {
        mediation_again(ctx, host, base, pc, callee, args, dst);
    }
    let capacity_before = if MASK & ablate::CENSUS != 0 {
        (*machine).mem.stack_capacity()
    } else {
        0
    };

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
        if MASK & ablate::AGAIN_SPAN != 0 {
            again_span(machine, caller.function, pc as usize);
        }
        if MASK & ablate::AGAIN_ADMIT != 0 {
            again_admit(machine, budget, span);
        }
        if MASK & ablate::AGAIN_SAFEPOINT != 0 {
            again_safepoint(machine, budget, caller.function, pc as usize);
        }
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
            if MASK & ablate::CENSUS != 0 {
                STOPS.fetch_add(1, Ordering::Relaxed);
            }
            (*host).left = Some(error);
            return Outcome::Raised.abi();
        }
    };
    if MASK & ablate::CENSUS != 0 {
        census(&mut *machine, callee, ArgsId(args), capacity_before);
    }
    if MASK & ablate::AGAIN_PUSH_POP != 0 {
        again_push_pop(&mut *machine, callee);
    }
    if MASK & ablate::AGAIN_ZERO != 0 {
        again_zero(&mut *machine, callee, callee_base);
    }
    if MASK & ablate::AGAIN_ARG_LOOKUP != 0 {
        again_arg_lookup(&*machine, callee, ArgsId(args));
    }
    if MASK & ablate::AGAIN_ARG_COPY != 0 {
        again_arg_copy(
            &mut *machine,
            callee,
            ArgsId(args),
            caller_base,
            callee_base,
        );
    }
    if MASK & ablate::AGAIN_OPEN_FRAME != 0 {
        again_open_frame(&mut *machine, budget, callee, ArgsId(args), caller_base);
    }

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
        if MASK & ablate::AGAIN_FRAMES != 0 {
            again_frames(machine, callee, callee_base, dst);
        }
    }

    let into = Destination {
        base: caller_base,
        slot: dst,
    };
    let entry = (*host).entries.entry(callee);
    let answered = match entry {
        Some(entry) => {
            (*host).tiers.native += 1;
            enter::<MASK>(host, entry, callee, callee_base, into)
        }
        None => {
            (*host).tiers.encoded += 1;
            let machine = &mut *machine;
            let floor = machine.frames.len() - 1;
            let answered = match machine.code() {
                // The dispatch loop pops the callee's frame and its words on the
                // way out, and hands back the answer — which is `encoded.rs`'s
                // `RETURN` arm reaching `None` for its caller, and is the same
                // shape `Machine::run` is handed an answer in.
                Ok(code) => machine.drive_from(&code, budget, floor),
                Err(error) => Err(error),
            };
            // The one return path that still materialises a run of words, and it
            // is the *VM's* convention rather than this boundary's: `RETURN` at
            // the floor is how the encoded tier hands a value to a host, and
            // teaching it to write a destination instead is a change to the
            // calling convention of the tier that is not being changed. ADR 0057
            // allows it in as many words — "a native/VM boundary may materialise
            // slot frames" — and it is 0.8% of the calls this path measures.
            let published = answered.map(|words| {
                for (at, word) in words.iter().enumerate() {
                    machine
                        .mem
                        .set_slot(into.base, into.slot + at as u32, *word);
                }
            });
            if MASK & ablate::AGAIN_FLOOR_VEC != 0 && published.is_ok() {
                again_floor_vec(machine, callee, into);
            }
            published
        }
    };

    let outcome = match answered {
        Ok(()) => Outcome::Returned.abi(),
        Err(error) => {
            (*host).left = Some(error);
            Outcome::Raised.abi()
        }
    };
    if MASK & ablate::AGAIN_REPUBLISH != 0 {
        // One more `republish`: the chunk-table walk and the two stores compiled
        // code re-reads after every helper.
        republish(ctx, host);
    }
    // Whatever the callee did, the pointers compiled code cached before the
    // hand-over are not to be used after it.
    republish(ctx, host);
    outcome
}

/// Runs the frame on top of the stack as compiled code, publishing its answer
/// into `into`.
///
/// The frame is already pushed and its arguments are already in it, which is what
/// makes this the same entry for the outermost call and for a native-to-native
/// one: ADR 0055's "No parameters are passed in registers."
///
/// `into` is where the answer goes and it is the caller's to choose, which is
/// ADR 0057: it is handed to the callee as two indices and **the callee writes
/// it**, out of its own slot and into the destination the lowering settled,
/// before its frame is removed. Nothing is allocated to carry the words, nothing
/// copies them twice, and nothing is published unless the outcome is
/// [`Outcome::Returned`] — the only path that writes the destination is the
/// callee's return path, where no safepoint intervenes.
///
/// # Safety
///
/// `host` is a live [`Bridge`], `entry` is a finalized entry point of a function
/// `cove_native` compiled for `callee`, and `base` is that call's frame. `into`
/// names a run of `Function::returns`' width in a frame that outlives the call —
/// in practice the caller's own, which is below `base` and cannot overlap it.
unsafe fn enter<const MASK: u64>(
    host: *mut Bridge<'_, '_>,
    entry: Entry,
    callee: FunctionId,
    base: u64,
    into: Destination,
) -> Result<(), RuntimeError> {
    let machine = (*host).machine;
    let index = {
        let machine = &mut *machine;
        // A word index rather than an address, because the `Vec` behind the words
        // moves and the index does not. See `cove_native::abi`.
        machine.mem.stack_index(base) as u64
    };
    if MASK & ablate::AGAIN_CTX != 0 {
        again_ctx(host, &mut *machine, base, into);
    }
    let (mut ctx, into_index) = {
        let machine = &mut *machine;
        let ctx = NativeCtx::new(host.cast::<c_void>(), machine.mem.words_ptr())
            .over_heap((*host).table());
        // The destination as the callee is given it: a word index, taken *after*
        // the frame was pushed and stable whatever a later `push_frame` does to
        // the `Vec`. This is the line ADR 0057's "never pointers" is about.
        (ctx, machine.mem.stack_index(into.base) as u64)
    };
    // Safety: the context is this call's, the frame at `index` is the callee's,
    // the destination is `into`'s width of words outside that frame, and the code
    // was emitted for exactly `Entry`'s shape.
    let outcome = entry(&mut ctx, index, into_index, into.slot);

    let machine = &mut *machine;
    // Pending work is charged on every exit — a return, a raise and a stop
    // alike, which is ADR 0055's own requirement.
    machine.bulk_work += ctx.pending_work;
    ctx.pending_work = 0;
    match outcome {
        // The answer is already where it belongs: the callee wrote it before it
        // returned, so all that is left is to take its frame away.
        Outcome::Returned => {
            machine.frames.pop();
            machine.mem.pop_frame(base);
            if MASK & ablate::AGAIN_POP != 0 {
                again_pop(machine, base);
            }
            Ok(())
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
    /// Where an outermost call publishes its answer.
    ///
    /// A call inside a Cove program has a destination its lowering settled, and
    /// [`Destination`] is how it reaches the callee. A call made *from Rust* has
    /// none, so the session provides one: a run of the return width, pushed once
    /// above the holder and below every call's frame, never popped, and read out
    /// after the call returns.
    ///
    /// It is not a [`Frame`], so it is not walked by the collector — a reference
    /// answer sitting here roots nothing. That is exactly the `Vec<u64>` this
    /// replaced: a run of words a benchmark holds was never a root either (see
    /// this type's own documentation), and the caller reads the words out before
    /// anything else can run.
    result: u64,
    /// How wide an answer is, which is static: `Function::returns`.
    width: u32,
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
        let returns = function.returns;
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
        // See [`Session::result`]. One word at least, even for a `Unit` return,
        // so that the address names a word of this segment rather than the first
        // word of whatever frame is pushed next.
        let width = machine.width(returns);
        let result = machine
            .mem
            .push_frame(width.max(1))
            .map_err(|Overflow| machine.too_deep(span))?;
        Ok(Session {
            machine,
            budget,
            id,
            arguments,
            holder,
            result,
            width,
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
                let into = Destination {
                    base: self.result,
                    slot: 0,
                };
                let published = {
                    let mut bridge =
                        Bridge::new(self.machine, self.budget, entries, &mut self.chunks);
                    bridge.tiers.native += 1;
                    let held: *mut Bridge = &mut bridge;
                    // Safety: `held` is this stack frame's bridge and outlives
                    // the call below; `entry` was compiled for `self.id`, whose
                    // frame is the one just pushed; and `into` is the session's
                    // own result run, which is below that frame and outlives it.
                    let published = unsafe { enter::<0>(held, entry, self.id, base, into) };
                    // Added rather than assigned: a bridge counts one call chain
                    // and a session makes many, and `= bridge.tiers` reported the
                    // last chain's count as the session's.
                    self.tiers.native += bridge.tiers.native;
                    self.tiers.encoded += bridge.tiers.encoded;
                    published
                };
                // The one `Vec` a session still builds, and it is the boundary's
                // rather than the call's: a Rust caller asked for the words.
                published.map(|()| self.machine.mem.read_words(self.result, self.width))
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

/// Which component of the call path a variant of the helper does **twice**.
///
/// [Issue #365](https://github.com/myuon/cove/issues/365) asks for the per-call
/// cost of the native call path attributed to its parts rather than to a
/// suspect named first. This module is how that attribution is taken, and the
/// shape of it is the whole argument for believing the numbers:
///
/// - the baseline arm is [`helpers`] itself, so nothing has to be assumed about
///   an instrumented copy being like the real thing — mask zero *is* the real
///   thing, because the one call helper body folds every branch these constants
///   guard;
/// - every variant **adds** one instance of one component rather than removing
///   it, so every variant computes the program the production helper computes
///   and the harness can keep checking every answer against the VM's. A frame
///   that was not zeroed or an argument that was not copied would be a
///   different program, and a number from a run that computed the wrong thing
///   looks like a number;
/// - each component is idempotent in the state it touches: the same span, the
///   same admission, the same words into the same slots, the same truncation to
///   the same base.
///
/// The cost of that shape is that each figure is a **lower bound** on the
/// component's real cost, because the second instance runs with the first one's
/// cache lines and branch history warm. It also means two variants may overlap
/// — [`ablate::AGAIN_OPEN_FRAME`] contains [`ablate::AGAIN_PUSH_POP`] and
/// [`ablate::AGAIN_ARG_COPY`] —
/// and that is on purpose: a sum of parts that misses a term still adds up, and
/// the containing variant is the cross-check that finds it.
///
/// [`ablate::CENSUS`] is not a timing variant. It is the pass that *counts*: how wide
/// the frames are, how many parameter words are copied, and how often
/// `push_frame`'s `Vec::resize` reallocates rather than fitting — a question a
/// timer cannot answer, because the answer is "almost never" and the almost is
/// the point.
pub mod ablate {
    /// `Machine::span` for the call, a second time.
    pub const AGAIN_SPAN: u64 = 1 << 0;
    /// `Machine::admit_frame`, a second time.
    pub const AGAIN_ADMIT: u64 = 1 << 1;
    /// `Machine::safepoint` — ADR 0040's three steps — a second time.
    pub const AGAIN_SAFEPOINT: u64 = 1 << 2;
    /// `Memory::push_frame` at the callee's width, and the `pop_frame` undoing
    /// it.
    pub const AGAIN_PUSH_POP: u64 = 1 << 3;
    /// `push_frame`'s zero fill, over the callee frame's non-parameter words.
    pub const AGAIN_ZERO: u64 = 1 << 4;
    /// `open_frame`'s two program lookups, arity compare and per-parameter
    /// `copy_slots`.
    pub const AGAIN_ARG_COPY: u64 = 1 << 5;
    /// The same, short of the copies: the two program lookups, the arity compare
    /// and the per-parameter width read.
    ///
    /// [`AGAIN_ARG_COPY`] less this one is the words themselves, which is the
    /// question of whether an argument copy is the *copy* or the looking up of
    /// what to copy — and they point at different fixes.
    pub const AGAIN_ARG_LOOKUP: u64 = 1 << 15;
    /// The whole of `open_frame`, into a duplicate frame that is then dropped.
    pub const AGAIN_OPEN_FRAME: u64 = 1 << 6;
    /// One more `Frame` pushed onto the frame stack and popped off it.
    pub const AGAIN_FRAMES: u64 = 1 << 7;
    /// One more `NativeCtx` and one more pair of `stack_index` derivations.
    pub const AGAIN_CTX: u64 = 1 << 8;
    /// One more `Memory::pop_frame`, to the base it is already at.
    pub const AGAIN_POP: u64 = 1 << 9;
    /// One more `republish` of the words pointer and the chunk table.
    pub const AGAIN_REPUBLISH: u64 = 1 << 10;
    /// The whole Rust-side mediation again, short of entering the callee.
    pub const AGAIN_MEDIATION: u64 = 1 << 11;
    /// One more owned `Vec` of the answer's width, on the encoded floor only.
    pub const AGAIN_FLOOR_VEC: u64 = 1 << 12;
    /// One more six-argument C-ABI indirect call into a helper that returns.
    pub const AGAIN_HOP: u64 = 1 << 13;
    /// Count what the call path did instead of adding to what it costs.
    pub const CENSUS: u64 = 1 << 14;
}

/// The helpers with a call that does `MASK`'s components twice.
///
/// See [`ablate`]. `helpers_ablated::<0>()` is [`helpers`]: the same helper body,
/// instantiated at a mask that folds every ablation away.
pub fn helpers_ablated<const MASK: u64>() -> NativeHelpers {
    NativeHelpers {
        safepoint,
        call: call_ablated::<MASK>,
    }
}

/// [`call_body`] at a mask, as something a code generator can bind.
///
/// # Safety
///
/// As [`call`].
unsafe extern "C" fn call_ablated<const MASK: u64>(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    callee: u32,
    args: u32,
    dst: u32,
) -> u32 {
    call_body::<MASK>(ctx, base, pc, callee, args, dst)
}

/// One component of the call path, again, and each one out of line.
///
/// `#[inline(never)]` on every one of them is method rather than style, and it
/// was arrived at by getting it wrong first. Inlined into [`call_body`], a
/// duplicated argument copy measured **22.6 ns** while the whole of
/// `open_frame` — which contains that copy, and an admission and a frame push
/// besides — measured 21.7. A part cannot cost more than the whole that holds
/// it, so the extra was not the component: it was three more inlined copies of
/// `copy_slots` and its debug assertions sitting in a function the measured loop
/// runs a hundred thousand times, paid in instruction fetch. Out of line, a
/// variant costs its component and one direct call, and the call is what the
/// `+ one C-ABI hop` row bounds.
///
/// The arguments that go through [`std::hint::black_box`] are there to stop the
/// optimiser commoning a second pure read with the first — a second `span` of
/// the same function at the same pc is exactly the expression an optimiser is
/// entitled to compute once, and a variant that measured nothing would report a
/// zero rather than a refusal.
///
/// A whole `Result<(), RuntimeError>` is never given to `black_box`, only
/// `is_err()` of one: holding the `Result` forces the error's own bytes into
/// memory on a path that never has an error, which measures the holding.
///
/// # Safety
///
/// Each takes what its caller already holds; none dereferences anything the
/// caller did not.
#[inline(never)]
fn again_span(machine: &Machine<'_>, id: FunctionId, pc: usize) {
    let id = std::hint::black_box(id);
    let pc = std::hint::black_box(pc);
    std::hint::black_box(machine.span(id, pc).start);
}

/// See [`again_span`].
#[inline(never)]
fn again_admit(machine: &Machine<'_>, budget: &Meter, span: cove_diag::Span) {
    let span = std::hint::black_box(span);
    std::hint::black_box(machine.admit_frame(budget, span).is_err());
}

/// See [`again_span`].
#[inline(never)]
fn again_safepoint(machine: &mut Machine<'_>, budget: &Meter, id: FunctionId, pc: usize) {
    let id = std::hint::black_box(id);
    let pc = std::hint::black_box(pc);
    std::hint::black_box(machine.safepoint(budget, id, pc).is_err());
}

/// `Memory::push_frame` at the callee's width, and the `pop_frame` undoing it.
///
/// See [`again_span`].
#[inline(never)]
fn again_push_pop(machine: &mut Machine<'_>, callee: FunctionId) {
    let size = machine.program.function(callee).frame_size();
    if let Ok(duplicate) = machine.mem.push_frame(size) {
        machine.mem.pop_frame(duplicate);
    }
}

/// `push_frame`'s zero fill, over the words of the callee's frame that are not
/// its parameters.
///
/// Those are the words `push_frame` zeroed and nothing has written since, so
/// writing zeroes over them a second time is the same frame. The parameters are
/// left alone because zeroing *them* would be a different program.
///
/// See [`again_span`].
#[inline(never)]
fn again_zero(machine: &mut Machine<'_>, callee: FunctionId, callee_base: u64) {
    let function = machine.program.function(callee);
    let size = function.frame_size();
    let params: u32 = function
        .params
        .iter()
        .map(|layout| machine.width(*layout))
        .sum();
    machine
        .mem
        .clear_words(callee_base + u64::from(params), size - params);
}

/// The looking up, without the copying: `Program::function`,
/// `Program::arg_list`, the arity compare and one width read per parameter.
///
/// See [`again_span`].
#[inline(never)]
fn again_arg_lookup(machine: &Machine<'_>, callee: FunctionId, args: ArgsId) {
    let program = machine.program;
    let target = program.function(std::hint::black_box(callee));
    let list = program.arg_list(std::hint::black_box(args));
    std::hint::black_box(list.len() == target.params.len());
    let mut at = 0;
    for (arg, layout) in list.iter().zip(&target.params) {
        at += machine.width(*layout);
        std::hint::black_box(arg.slot);
    }
    std::hint::black_box(at);
}

/// `open_frame`'s argument loop, again and word for word.
///
/// Copying the same words to the same slots is the frame the callee already has.
///
/// See [`again_span`].
#[inline(never)]
fn again_arg_copy(
    machine: &mut Machine<'_>,
    callee: FunctionId,
    args: ArgsId,
    caller_base: u64,
    callee_base: u64,
) {
    let program = machine.program;
    let target = program.function(callee);
    let list = program.arg_list(args);
    if list.len() != target.params.len() {
        return;
    }
    let mut at = 0;
    for (arg, layout) in list.iter().zip(&target.params) {
        let width = machine.width(*layout);
        machine.mem.copy_slots(
            callee_base + u64::from(at),
            caller_base + u64::from(arg.slot),
            width,
        );
        at += width;
    }
}

/// The whole of `open_frame` again, into a frame above the callee's which is
/// then dropped.
///
/// The cross-check on the four parts above, because a sum of parts that misses a
/// term still adds up. The span it is given is the callee's declaration, which is
/// a field read rather than a lookup, so this variant does not quietly contain
/// [`again_span`] as well.
///
/// See [`again_span`].
#[inline(never)]
fn again_open_frame(
    machine: &mut Machine<'_>,
    budget: &Meter,
    callee: FunctionId,
    args: ArgsId,
    caller_base: u64,
) {
    let span = machine.program.function(callee).span;
    if let Ok(duplicate) =
        super::encoded::open_frame(machine, budget, caller_base, span, callee, args, None)
    {
        machine.mem.pop_frame(duplicate);
    }
}

/// One more `Frame` onto the stack of frames, and off it again: the bookkeeping
/// either tier does to make a callee's slots walkable.
///
/// See [`again_span`].
#[inline(never)]
fn again_frames(machine: &mut Machine<'_>, callee: FunctionId, callee_base: u64, dst: u32) {
    machine.frames.push(Frame {
        function: callee,
        base: callee_base,
        pc: 0,
        dst: dst as Slot,
    });
    machine.frames.pop();
}

/// One more `NativeCtx` and one more pair of index derivations: what this tier
/// builds to hand a compiled callee its frame and its destination.
///
/// # Safety
///
/// As [`enter`]: `host` is a live [`Bridge`].
#[inline(never)]
unsafe fn again_ctx(
    host: *mut Bridge<'_, '_>,
    machine: &mut Machine<'_>,
    base: u64,
    into: Destination,
) {
    let ctx =
        NativeCtx::new(host.cast::<c_void>(), machine.mem.words_ptr()).over_heap((*host).table());
    std::hint::black_box(&ctx);
    std::hint::black_box(machine.mem.stack_index(base) as u64);
    std::hint::black_box(machine.mem.stack_index(into.base) as u64);
}

/// The truncation again, to the base it is already at: `pop_frame` alone, apart
/// from the `push_frame` that pairs with it.
///
/// See [`again_span`].
#[inline(never)]
fn again_pop(machine: &mut Machine<'_>, base: u64) {
    machine.mem.pop_frame(base);
}

/// One more owned run of words of exactly the width the encoded floor
/// materialises, read out of the destination the answer is now in: the
/// allocation and the copy, and nothing else.
///
/// See [`again_span`].
#[inline(never)]
fn again_floor_vec(machine: &mut Machine<'_>, callee: FunctionId, into: Destination) {
    let width = machine.width(machine.program.function(callee).returns);
    std::hint::black_box(machine.mem.read_words(into.base, width));
}

/// A call helper that does nothing, for [`ablate::AGAIN_HOP`].
///
/// Six integer arguments in registers, a `ret`, and an answer compiled code
/// ignores — which is the hop without the runtime: the argument set-up the
/// generated call sequence pays, the indirect `call`, the callee's own entry and
/// return. It is a *floor* on the hop and not the hop: the generated code around
/// the real helper is emitted once per call site and is not doubled by this, and
/// the entry into compiled code — its seven pushes and seven pops — is not here
/// either. Part 2 of #365 is what measures those, by removing them.
///
/// # Safety
///
/// Reads nothing through any of its arguments.
unsafe extern "C" fn nothing(
    _ctx: *mut NativeCtx,
    _base: u64,
    _pc: u32,
    _callee: u32,
    _args: u32,
    _dst: u32,
) -> u32 {
    Outcome::Returned.abi()
}

/// Everything the call helper does in Rust, a second time, short of entering the
/// callee.
///
/// [`ablate::AGAIN_MEDIATION`]: the charge, the `sync`, the frame the caller is
/// read out of, the span, the safepoint, `open_frame`, the frame pushed for the
/// callee, the context and the two indices a compiled callee is handed, the pop
/// and the republish. The callee is *not* entered, because entering it twice
/// would run the program twice; what is left out is therefore exactly the two
/// hops — the generated call sequence and the entry into compiled code — and
/// that is the one term of the decomposition this cannot reach.
///
/// The duplicate frame is opened above the caller's and dropped again, so
/// nothing the callee or the caller can read is different afterwards. A stop the
/// duplicate safepoint reports is ignored: the real safepoint that follows it
/// reports the same stop, from the same meter, and acts on it. A workload that
/// stops is therefore not one to take these numbers from, and [`Census::stops`]
/// is what says whether the measured one did.
///
/// # Safety
///
/// As [`call`].
#[inline(never)]
unsafe fn mediation_again(
    ctx: *mut NativeCtx,
    host: *mut Bridge<'_, '_>,
    base: u64,
    pc: u32,
    callee: FunctionId,
    args: u32,
    dst: u32,
) {
    let machine = (*host).machine;
    let budget = (*host).budget;
    let opened = {
        let machine = &mut *machine;
        // The charge itself is not repeated: `bulk_work` is fuel, and doubling a
        // charge would move a safepoint rather than cost one add.
        machine.sync(pc as usize);
        let caller = machine
            .frames
            .last()
            .copied()
            .expect("a native frame is executing");
        debug_assert_eq!(machine.mem.stack_index(caller.base) as u64, base);
        let span = machine.span(caller.function, pc as usize);
        std::hint::black_box(
            machine
                .safepoint(budget, caller.function, pc as usize)
                .is_err(),
        );
        super::encoded::open_frame(
            machine,
            budget,
            caller.base,
            span,
            callee,
            ArgsId(args),
            None,
        )
        .map(|callee_base| (caller.base, callee_base))
    };
    let Ok((caller_base, callee_base)) = opened else {
        return;
    };
    {
        let machine = &mut *machine;
        machine.frames.push(Frame {
            function: callee,
            base: callee_base,
            pc: 0,
            dst: dst as Slot,
        });
        let held = NativeCtx::new(host.cast::<c_void>(), machine.mem.words_ptr())
            .over_heap((*host).table());
        std::hint::black_box(&held);
        std::hint::black_box(machine.mem.stack_index(callee_base) as u64);
        std::hint::black_box(machine.mem.stack_index(caller_base) as u64);
        machine.frames.pop();
        machine.mem.pop_frame(callee_base);
    }
    republish(ctx, host);
}

/// What the call path did, counted rather than timed.
///
/// The questions a timer cannot answer, and one of them is why this exists:
/// `push_frame` grows a `Vec`, and how often that `Vec` *reallocates* rather
/// than fitting inside the capacity it already has is the difference between a
/// term of the decomposition and a rounding error. See [`ablate::CENSUS`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Census {
    /// Calls the helper opened a frame for.
    pub calls: u64,
    /// Words of callee frame `push_frame` was asked for, summed.
    pub frame_words: u64,
    /// Parameter words copied into those frames, summed.
    pub param_words: u64,
    /// Parameters copied, summed — so that the per-parameter figure is a
    /// measured average and not an assumed one.
    pub params: u64,
    /// Calls whose `push_frame` made the stack's `Vec` reallocate.
    pub reallocations: u64,
    /// The deepest the frame stack got.
    pub deepest: u64,
    /// Calls the safepoint or the frame refused.
    ///
    /// A run with any is a run whose ablation numbers are not to be read: a
    /// duplicated safepoint swallows the stop it saw. Nought is the only figure
    /// that makes the rest of the table meaningful.
    pub stops: u64,
}

static CALLS: AtomicU64 = AtomicU64::new(0);
static FRAME_WORDS: AtomicU64 = AtomicU64::new(0);
static PARAM_WORDS: AtomicU64 = AtomicU64::new(0);
static PARAMS: AtomicU64 = AtomicU64::new(0);
static REALLOCATIONS: AtomicU64 = AtomicU64::new(0);
static DEEPEST: AtomicU64 = AtomicU64::new(0);
static STOPS: AtomicU64 = AtomicU64::new(0);

/// Counts one call. See [`ablate::CENSUS`].
fn census(machine: &mut Machine<'_>, callee: FunctionId, args: ArgsId, capacity_before: usize) {
    let program = machine.program;
    let target = program.function(callee);
    let size = target.frame_size();
    let params: u32 = target
        .params
        .iter()
        .map(|layout| machine.width(*layout))
        .sum();
    CALLS.fetch_add(1, Ordering::Relaxed);
    FRAME_WORDS.fetch_add(u64::from(size), Ordering::Relaxed);
    PARAM_WORDS.fetch_add(u64::from(params), Ordering::Relaxed);
    PARAMS.fetch_add(program.arg_list(args).len() as u64, Ordering::Relaxed);
    if machine.mem.stack_capacity() != capacity_before {
        REALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
    DEEPEST.fetch_max(machine.frames.len() as u64, Ordering::Relaxed);
}

/// What the census has counted since [`census_reset`].
pub fn census_taken() -> Census {
    Census {
        calls: CALLS.load(Ordering::Relaxed),
        frame_words: FRAME_WORDS.load(Ordering::Relaxed),
        param_words: PARAM_WORDS.load(Ordering::Relaxed),
        params: PARAMS.load(Ordering::Relaxed),
        reallocations: REALLOCATIONS.load(Ordering::Relaxed),
        deepest: DEEPEST.load(Ordering::Relaxed),
        stops: STOPS.load(Ordering::Relaxed),
    }
}

/// Forgets what the census counted, so that one pass is one figure.
pub fn census_reset() {
    for counter in [
        &CALLS,
        &FRAME_WORDS,
        &PARAM_WORDS,
        &PARAMS,
        &REALLOCATIONS,
        &DEEPEST,
        &STOPS,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}
