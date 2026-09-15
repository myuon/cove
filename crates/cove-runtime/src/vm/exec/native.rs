//! The runtime's half of [ADR 0055]'s native boundary.
//!
//! `cove-native` emits machine code and calls back through a table of function
//! pointers. This module is what those pointers point at, and what enters the
//! code in the first place. It is the other side of the inversion that crate's
//! documentation describes: the dependency edge runs one way, from here to
//! there, and the calls run both.
//!
//! # It is the tier's half now, and it was the experiment's first
//!
//! This module was written for the comparison
//! `crates/cove-bench/src/bin/native_compare.rs` runs — two code generators over
//! identical optimized IR, on *real* data, with the VM beside them as the oracle
//! — and it said so, because `cove run` did not reach it and the dispatch loop
//! consulted no tier table.
//!
//! [Issue #369](https://github.com/myuon/cove/issues/369) changed that. The tier
//! is selectable as `cove run --backend native`, `crate::native` owns the
//! compiled table for one program, and **the encoded `CALL` arm asks that table**
//! — [`from_encoded`] below is the hop it added, and it is the one thing that
//! makes coverage compositional rather than whatever one entry point happens to
//! reach without going back through the VM.
//!
//! Two things it did *not* change, and both are load-bearing. The VM is still the
//! default: nothing here is reached by a run that did not ask for it, and what
//! such a run pays is `Machine::tiered`'s single `Option` test at a `call`. And
//! this file still names [`Entry`] and never names a `Jit`: which code generator
//! emitted the machine code is not something the boundary knows, which is why it
//! compiles in a build that has none.
//!
//! # What compiled code is allowed to do, and what it hands back
//!
//! Five things, and every one of them is a helper rather than emitted code,
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
//! - **an allocation** — [`alloc`] below, which is `Machine::allocate` whole:
//!   the length conversion, the payload-word overflow rejection, the
//!   collect-and-retry and the one refusal an exhausted heap raises. It is the
//!   archetype of the sentence above, and it is also the first thing compiled
//!   code can do that *causes* a collection;
//! - **a builtin the emitted fast path could not take** — [`builtin`] below,
//!   which is `Machine::call_builtin` whole. It is the cold half of a builtin
//!   whose fast path *is* emitted, and it exists because the refusals those cold
//!   paths produce name a rendered `Value`, which `cove-native` cannot see;
//! - **leaving** — a [`Raise`] the compiled code names and this builds, which is
//!   how a `+` that overflowed in machine code produces the *same sentence* the
//!   encoded tier's does.
//!
//! Everything else — an `Array` element, a `String` byte, an object's length, an
//! enum's switch, a `Vector.push` into spare capacity — is emitted code, and that
//! is deliberate: an operation that is one identical helper call in both arms
//! cannot tell the two code generators apart, so a comparison over a subset made
//! entirely of helper calls would measure nothing. That is why [`builtin`] is
//! reachable only from a cold path and never as a lowering of its own.
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

use cove_ir::{ArgsId, BuiltinId, FunctionId, LayoutId, Slot, StrId};
use cove_native::{BufferOp, Entry, NativeCtx, NativeHelpers, Opened, Outcome, Raise};

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

/// How a run divided between the two tiers, one counter per transition.
///
/// ADR 0055: "The run reports how many calls and functions used each tier, so a
/// benchmark cannot present a mixed run as fully native." This is that report,
/// for calls; how many *functions* were compiled is the compiler's own count and
/// belongs beside it.
///
/// # Why a transition and not a tier
///
/// Two counters — native calls and encoded calls — cannot say the one thing the
/// report exists to say. A run in which every compiled function is only ever
/// reached *from* compiled code has full native coverage of its own subtree and
/// none of the program; a run in which the encoded tier enters compiled code is
/// compositional. Both answer the same pair of totals. So the counter is the
/// **edge** rather than the node, and the four edges of
/// [issue #369](https://github.com/myuon/cove/issues/369)'s table are four
/// fields.
///
/// Native-to-native is two fields because there are two protocols and they cost
/// differently: `direct` is the `open`/entry/`close` sequence generated code
/// performs itself, and `mediated` is the one [`CallFn`](cove_native::CallFn)
/// helper doing all of it. A code generator emitting no direct call leaves the
/// first at nought, and a reader who could not tell the two apart would read a
/// mediated run as if PR #368 were in it.
///
/// The two `host_to_*` fields are not Cove calls at all: they are the outermost
/// frame, entered from Rust by [`Session::call`] or by a run. They are here
/// because a total that left them out would not add up to the calls that were
/// made, and they are named apart because "the VM called native code" is the
/// claim this issue is about and the outermost entry is not evidence for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tiers {
    /// An encoded caller called a function with no compiled entry.
    pub vm_to_vm: u64,
    /// An encoded caller entered a compiled callee.
    ///
    /// The transition this issue exists for. Before it, a compiled function
    /// called from an encoded one stayed encoded and coverage was not
    /// compositional.
    pub vm_to_native: u64,
    /// Compiled code called a function with no compiled entry, which the
    /// dispatch loop then ran.
    pub native_to_vm: u64,
    /// Compiled code entered a compiled callee itself: `open`, the entry, and
    /// `close`.
    pub native_to_native_direct: u64,
    /// Compiled code entered a compiled callee through the one mediated call
    /// helper.
    pub native_to_native_mediated: u64,
    /// A Rust caller entered a compiled outermost frame.
    pub host_to_native: u64,
    /// A Rust caller entered an outermost frame the dispatch loop ran.
    pub host_to_vm: u64,
}

impl Tiers {
    /// Calls entered through a compiled entry point, the outermost included.
    pub fn native(self) -> u64 {
        self.vm_to_native
            + self.native_to_native_direct
            + self.native_to_native_mediated
            + self.host_to_native
    }

    /// Calls run by the encoded dispatch loop.
    pub fn encoded(self) -> u64 {
        self.vm_to_vm + self.native_to_vm + self.host_to_vm
    }

    /// Cove calls this run made, the outermost entry excluded.
    pub fn calls(self) -> u64 {
        self.vm_to_vm
            + self.vm_to_native
            + self.native_to_vm
            + self.native_to_native_direct
            + self.native_to_native_mediated
    }
}

/// The tier table one run consults, and what consulting it counted.
///
/// ADR 0055's `Program + FunctionId -> encoded entry | native entry`, installed
/// on the [`Machine`] for the length of a run so that **the encoded `CALL` arm
/// asks the same table the call helper does**. Until it did, a compiled function
/// called from an encoded one stayed encoded.
///
/// # It is boxed, and that is load-bearing
///
/// A helper reaches the machine through a raw pointer, for the reason this
/// module's header gives, and it reaches this through a second one. Those two
/// pointers must name **disjoint** memory: `republish` writes the chunk table
/// while `Memory` is borrowed, and a nested call reaches the same table again
/// while an outer one is still inside `enter`. Keeping this in a `Box` puts it
/// outside the `Machine`'s own allocation, so neither of those is two borrows of
/// one place. A field of `Machine` would have been.
///
/// It is also what lets the chunk table be reserved **once** and shared by every
/// bridge of a nested chain — see [`Tiering::chunks`].
pub(crate) struct Tiering {
    /// The table itself, as a raw pointer.
    ///
    /// A raw pointer rather than a `&'a dyn Tiered` because the two installers
    /// have two different lifetimes. A run installs a table that outlives its
    /// `Vm`; [`Session::call`] installs one **per call**, chosen by its caller
    /// after the session was opened, and a session's borrow of the machine
    /// began before that table existed. One field cannot be both, and a second
    /// lifetime parameter on `Machine` would be paid for by every signature in
    /// the crate.
    ///
    /// # Safety
    ///
    /// Whoever installs it keeps the table alive until it is taken out again.
    /// The two installers are [`crate::Vm::with_native`], where the table
    /// outlives the machine, and [`Session::call`], which takes it back before
    /// it returns.
    entries: *const dyn Tiered,
    /// Which transitions the calls of this run took.
    counts: Tiers,
    /// Dynamic calls to each function that did **not** go native, by
    /// `FunctionId`.
    ///
    /// [Issue #369](https://github.com/myuon/cove/issues/369) asks for refusals
    /// "ordered by dynamic calls prevented from going native, not only by
    /// function count", and this is that number: one entry per function,
    /// incremented wherever a call found no compiled entry — the encoded `CALL`
    /// arm and the call helper alike.
    ///
    /// It is counted per **callee** on purpose, and that is what answers the
    /// issue's "if a refused caller contains calls to compiled callees, count
    /// the calls that VM-to-native recovers rather than attributing its whole
    /// subtree to the refusal". A compiled callee reached from a refused caller
    /// is a `vm_to_native` and is charged to nobody's refusal; only the call
    /// that actually stayed in the VM is charged, and only to the function that
    /// could not be compiled.
    refused: Vec<u64>,
    /// One pointer per committed heap chunk, which is
    /// [`NativeCtx::chunks`].
    ///
    /// **Its capacity is reserved for the whole heap and it never reallocates**,
    /// and that is load-bearing rather than an optimisation: every `NativeCtx` in
    /// a native call chain holds `chunks.as_ptr()`, and a `Vec` that grew would
    /// leave every one of them but the innermost pointing at a freed table.
    /// Reserving the spine's length costs one allocation of eight bytes per
    /// 64-KiB chunk the heap *could* hold, and makes the address a constant.
    ///
    /// It lives here rather than in a [`Bridge`] for a measured reason: building
    /// it is an allocation and a walk, and a bridge is made once per *call*.
    /// Built per call, the covefmt scenario measured the allocator rather than
    /// the code — 4 KiB reserved and cleared for every one of twenty thousand
    /// calls.
    chunks: Vec<*mut u64>,
}

impl Tiering {
    /// A tier over `entries`, for a program of `functions` functions.
    ///
    /// # Safety
    ///
    /// `entries` stays alive and unmoved until this is taken out of the machine
    /// it is installed on.
    pub(crate) unsafe fn new(
        entries: *const (dyn Tiered + 'static),
        functions: usize,
        chunk_capacity: usize,
    ) -> Tiering {
        Tiering {
            entries,
            counts: Tiers::default(),
            refused: vec![0; functions],
            chunks: Vec::with_capacity(chunk_capacity),
        }
    }

    /// Points this tier at `entries`, keeping the counts it has taken.
    ///
    /// What [`Session::call`] does between calls: a session is asked for the
    /// encoded tier and then for a compiled one, over the same machine, and the
    /// counts are the session's rather than one call's.
    ///
    /// # Safety
    ///
    /// As [`Tiering::new`].
    pub(crate) unsafe fn aim(&mut self, entries: *const (dyn Tiered + 'static)) {
        self.entries = entries;
    }

    /// Which transitions this run's calls took.
    pub(crate) fn counts(&self) -> Tiers {
        self.counts
    }

    /// Dynamic calls to each function that stayed on the encoded tier.
    pub(crate) fn refused_calls(&self) -> &[u64] {
        &self.refused
    }

    /// Charges the outermost frame of a run to the encoded tier.
    ///
    /// See `Machine::run`: a run always enters its entry through the dispatch
    /// loop, and this is the line that makes a report say so instead of leaving the
    /// entry out of the totals.
    pub(crate) fn entered_encoded(&mut self) {
        self.counts.host_to_vm += 1;
    }

    /// The compiled entry of `callee`, with the **encoded caller's** transition
    /// charged.
    ///
    /// The other end of [`Bridge::entry_of`], and the two ask one table: that is
    /// the whole of "the encoded `CALL` path must consult the same tier table as
    /// the native path".
    ///
    /// # Safety
    ///
    /// The table this was aimed at is still alive.
    pub(crate) unsafe fn crossing(&mut self, callee: FunctionId) -> Option<Entry> {
        let entry = (*self.entries).entry(callee);
        match entry {
            Some(_) => self.counts.vm_to_native += 1,
            None => {
                self.counts.vm_to_vm += 1;
                charge_refusal(self, callee);
            }
        }
        entry
    }
}

/// What one native call chain shares: the runtime and the tier it was entered
/// under.
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
    /// The installed [`Tiering`]: the entry table, the chunk table and the
    /// counters, all three.
    ///
    /// A raw pointer for [`Bridge::machine`]'s reason and one more: a nested call
    /// reaches the same tier while an outer one is still inside [`enter`], so a
    /// `&mut` held across the entry would be two live borrows of one place. It
    /// names a `Box`'s contents, which is why it does not alias the machine —
    /// see [`Tiering`]'s own note.
    tier: *mut Tiering,
    /// The error a helper is leaving with.
    ///
    /// A callee's failure is a whole `RuntimeError` — a span, a rule, a call
    /// chain — and compiled code has nowhere to put one: [`Raise`] names errors
    /// and carries at most two numbers. So the helper keeps it here and answers
    /// [`Raise::Called`], and whoever entered the outermost compiled function
    /// takes it back out.
    left: Option<RuntimeError>,
}

impl<'m, 'a> Bridge<'m, 'a> {
    /// A bridge over `machine`, under the tier `tier` names.
    ///
    /// # Safety
    ///
    /// `machine` is a live, uniquely reachable machine and `tier` a live
    /// [`Tiering`] outside its allocation; both outlive the bridge.
    unsafe fn over(machine: *mut Machine<'a>, budget: &'m Meter, tier: *mut Tiering) -> Self {
        (*machine).mem.chunk_bases(&mut (*tier).chunks);
        Bridge {
            machine,
            budget,
            tier,
            left: None,
        }
    }

    /// The chunk table, as compiled code is given it.
    ///
    /// # Safety
    ///
    /// The tier that owns the table outlives every bridge over it.
    unsafe fn table(&self) -> *const *mut u64 {
        (*self.tier).chunks.as_ptr()
    }

    /// The compiled entry of `callee`, and the transition counted.
    ///
    /// The *same* question the encoded `CALL` arm asks, of the *same* table —
    /// which is the whole of what makes coverage compositional. `crossed` is the
    /// transition to charge when there is an entry and `stayed` the one when
    /// there is not.
    ///
    /// # Safety
    ///
    /// As [`Bridge::over`].
    unsafe fn entry_of(
        &self,
        callee: FunctionId,
        crossed: fn(&mut Tiers),
        stayed: fn(&mut Tiers),
    ) -> Option<Entry> {
        let tier = self.tier;
        let entry = (*(*tier).entries).entry(callee);
        match entry {
            Some(_) => crossed(&mut (*tier).counts),
            None => {
                stayed(&mut (*tier).counts);
                charge_refusal(&mut *tier, callee);
            }
        }
        entry
    }
}

/// `entries` as a raw pointer whose lifetime the type no longer carries.
///
/// [`Tiering::entries`] is a raw pointer for the reason that field gives — a run
/// installs a table that outlives its machine and [`Session::call`] installs one
/// per call — and a raw pointer to a trait object still has a lifetime in its
/// *type*. One field cannot hold both, so the field holds the erased form and
/// this is the one line that erases it.
///
/// A `transmute` of two raw pointers of the same shape, which is exactly what it
/// is for: nothing about the value changes and the lifetime was never encoded in
/// the bytes.
///
/// # Safety
///
/// The caller keeps `entries` alive and unmoved for as long as the pointer is
/// installed. Both installers say how they do it.
pub(crate) unsafe fn erase<'x>(entries: &'x dyn Tiered) -> *const (dyn Tiered + 'static) {
    std::mem::transmute::<*const (dyn Tiered + 'x), *const (dyn Tiered + 'static)>(entries)
}

/// Charges one dynamic call to `callee`'s refusal. See [`Tiering::refused`].
fn charge_refusal(tier: &mut Tiering, callee: FunctionId) {
    if let Some(count) = tier.refused.get_mut(callee.index()) {
        *count += 1;
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
    let tier = (*host).tier;
    // Extended rather than rebuilt, and that is a fact about the heap rather
    // than a shortcut: a committed chunk is never replaced and never moves, so
    // every entry already in the table is still right and the only thing that
    // can have changed is that there are more of them. See `Words::bases`.
    (*machine).mem.chunk_bases(&mut (*tier).chunks);
    (*ctx).words = (*machine).mem.words_ptr();
    (*ctx).chunks = (*tier).chunks.as_ptr();
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

/// The allocation helper: one `Inst::Alloc`, handed over whole.
///
/// See [`cove_native::AllocFn`] for the signature and for why the answer is an
/// address or a zero. What happens here is `encoded.rs`'s
/// `ALLOC_FIXED | ALLOC_IMM | ALLOC_SLOT` arm with one thing in front of it:
///
/// 1. the unpaid work is charged and [ADR 0040]'s three steps are taken, in that
///    order, because ADR 0055 says a safepoint occurs "around allocation or
///    runtime calls which may collect". The encoded arm does *not* do this — it
///    leans on the dispatch loop's own `SAFEPOINT_STRIDE`, and compiled code has
///    no dispatch loop to lean on;
/// 2. `machine.sync(pc)` — the same line the encoded arm has, and for the same
///    reason: `Machine::allocate` may collect, and a collection walks this
///    frame;
/// 3. `Machine::allocate`, whole. Not one part of it: the `u32` conversion, the
///    `try_payload_words` overflow rejection, the collect-and-retry and the
///    single "this run has no memory left" refusal are that function's and stay
///    there, which is ADR 0055's "runtime operations remain runtime helpers
///    initially" with allocation as the archetype.
///
/// # Safety
///
/// As [`safepoint`].
///
/// [ADR 0040]: ../../../../../docs/adr/0040-a-bound-outlives-its-backend.md
unsafe extern "C" fn alloc(ctx: *mut NativeCtx, pc: u32, layout: u32, len: i64) -> u64 {
    let host = (*ctx).host.cast::<Bridge>();
    let machine = (*host).machine;
    let budget = (*host).budget;

    // An allocation is a safepoint. The work went into `pending_work` before the
    // hand-over and is charged here, so nothing is counted twice and nothing is
    // dropped if the safepoint stops the run.
    let work = (*ctx).pending_work;
    (*ctx).pending_work = 0;

    // One borrow that ends before compiled code runs again; see the module's
    // aliasing note.
    let answered = {
        let machine = &mut *machine;
        machine.bulk_work += work;
        machine.sync(pc as usize);
        let id = machine
            .frames
            .last()
            .expect("a native frame is executing")
            .function;
        machine
            .safepoint(budget, id, pc as usize)
            .and_then(|()| machine.allocate(LayoutId(layout), len))
            // The span is the *allocating* instruction's, which is what the
            // encoded arm's `fail!` attaches through its own `sync`. Compiled
            // code knows the pc and the runtime knows the span, which is the
            // division every raise here makes.
            .map_err(|error| error.at(machine.span(id, pc as usize)))
    };
    // The allocation may have committed a heap chunk and the safepoint may have
    // grown the stack, so both pointers compiled code cached are stale.
    republish(ctx, host);
    match answered {
        Ok(addr) => addr,
        Err(error) => {
            (*host).left = Some(error);
            // Zero is not an address — the heap begins at `HEAP_ORIGIN_WORDS` —
            // so it needs no second output to mean "I am holding the error".
            0
        }
    }
}

/// The field-load helper: one [`Inst::LoadField`](cove_ir::Inst::LoadField),
/// whose bound the emitted table lookup could not answer.
///
/// See [`cove_native::FieldLoadFn`] for what this is *for*: emitted code answers
/// its own object's field bound in one table load for every *fixed*-payload
/// shape, and falls here for a *variable* one — `Any`, in practice, which is
/// `Shape::Boxed`. What runs here is `encoded.rs`'s `LOAD_FIELD` arm exactly:
/// `Machine::checked`, dynamic and exact, and then the copy — both `addr` and
/// `into` arrive as linear addresses emitted code already formed, so there is
/// no slot to resolve against a frame here.
///
/// Unlike [`alloc`] and [`builtin`] this is **not a safepoint**. Neither the
/// bound check nor the copy it guards can allocate, so there is no unpaid work
/// to publish and no cached pointer for [`republish`] to fix.
///
/// # Safety
///
/// As [`safepoint`]. `into` is a valid destination of `width` words, which
/// `crate::subset`'s `supported` bounded.
unsafe extern "C" fn field_load(
    ctx: *mut NativeCtx,
    pc: u32,
    addr: u64,
    at: u32,
    width: u32,
    into: u64,
) -> u32 {
    let host = (*ctx).host.cast::<Bridge>();
    let machine = (*host).machine;

    let answered = {
        let machine = &mut *machine;
        machine.sync(pc as usize);
        let function = machine
            .frames
            .last()
            .expect("a native frame is executing")
            .function;
        machine
            .checked(addr, at, width)
            .map(|()| {
                machine
                    .mem
                    .copy_words(into, machine.mem.payload_addr(addr, at), width);
            })
            .map_err(|error| error.at(machine.span(function, pc as usize)))
    };
    match answered {
        Ok(()) => Outcome::Returned.abi(),
        Err(error) => {
            (*host).left = Some(error);
            Outcome::Raised.abi()
        }
    }
}

/// [`field_load`], the other direction: one
/// [`Inst::StoreField`](cove_ir::Inst::StoreField). See
/// [`cove_native::FieldStoreFn`]. `from` is the linear address the words are
/// copied out of, in place of `into`.
///
/// # Safety
///
/// As [`field_load`].
unsafe extern "C" fn field_store(
    ctx: *mut NativeCtx,
    pc: u32,
    addr: u64,
    at: u32,
    width: u32,
    from: u64,
) -> u32 {
    let host = (*ctx).host.cast::<Bridge>();
    let machine = (*host).machine;

    let answered = {
        let machine = &mut *machine;
        machine.sync(pc as usize);
        let function = machine
            .frames
            .last()
            .expect("a native frame is executing")
            .function;
        machine
            .checked(addr, at, width)
            .map(|()| {
                machine
                    .mem
                    .copy_words(machine.mem.payload_addr(addr, at), from, width);
            })
            .map_err(|error| error.at(machine.span(function, pc as usize)))
    };
    match answered {
        Ok(()) => Outcome::Returned.abi(),
        Err(error) => {
            (*host).left = Some(error);
            Outcome::Raised.abi()
        }
    }
}

/// The builtin helper: one `Inst::CallBuiltin`, handed over whole.
///
/// See [`cove_native::BuiltinFn`] for what this is *for*, which is the half of it
/// that matters: it is the cold path of a builtin whose fast path emitted code
/// takes, and it exists because the messages those cold paths produce name a
/// rendered `Value` that `cove-native` cannot see. It is `encoded.rs`'s
/// `CALL_BUILTIN` arm and nothing else — the same `Machine::call_builtin`, the same
/// operand buffer, the same dispatch by two strings — so the sentence a refusal
/// produces is the one the VM has always produced, rather than a second copy of it
/// in a code generator.
///
/// A builtin may allocate and an allocation may collect, so this is a safepoint for
/// exactly [`alloc`]'s reason and takes the same three steps in the same order.
///
/// # Safety
///
/// As [`safepoint`].
unsafe extern "C" fn builtin(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    dst: u32,
    builtin: u32,
    args: u32,
) -> u32 {
    let host = (*ctx).host.cast::<Bridge>();
    let machine = (*host).machine;
    let budget = (*host).budget;

    let work = (*ctx).pending_work;
    (*ctx).pending_work = 0;

    let answered = {
        let machine = &mut *machine;
        machine.bulk_work += work;
        machine.sync(pc as usize);
        let frame = *machine.frames.last().expect("a native frame is executing");
        debug_assert_eq!(
            machine.mem.stack_index(frame.base) as u64,
            base,
            "compiled code and the frame stack disagree about which frame the builtin is in"
        );
        machine
            .safepoint(budget, frame.function, pc as usize)
            // The frame's *address* rather than its index, which is the one thing
            // emitted code would have had to compute and the reason `base` is not
            // an argument: the top frame is this call's, and the runtime is
            // already holding it.
            .and_then(|()| {
                machine.call_builtin(frame.base, dst as Slot, BuiltinId(builtin), ArgsId(args))
            })
            .map_err(|error| error.at(machine.span(frame.function, pc as usize)))
    };
    republish(ctx, host);
    match answered {
        Ok(()) => Outcome::Returned.abi(),
        Err(error) => {
            (*host).left = Some(error);
            Outcome::Raised.abi()
        }
    }
}

/// The growable-buffer helper: one of [ADR 0052]'s four, handed over whole.
///
/// See [`cove_native::BufferFn`] for why each of the four is the helper rather
/// than a fast path and a cold one — the short of it is one rooting discipline
/// that is not the frame's, one chunked safepoint contract, and one UTF-8 walk.
/// What happens here is `encoded.rs`'s `ALLOC_BUFFER`, `APPEND_BYTE`,
/// `APPEND_BYTES` and `FINISH_BUFFER` arms, and *is* those arms: each one reads
/// its operands out of the frame and calls the same `Machine` method the
/// dispatch loop calls, so there is no second copy of ADR 0052's capacity
/// arithmetic, growth policy, chunking or relabel anywhere.
///
/// Every one of them may allocate and an allocation may collect, so this is a
/// safepoint for exactly [`alloc`]'s reason and takes the same three steps in the
/// same order.
///
/// # The span is attached the way `fail!` attaches it
///
/// `append_bytes` builds its own refusals with a span already on them, and the
/// rest answer a bare `RuntimeError`. `RuntimeError::at` is `get_or_insert`, so
/// one `map_err` here is what the encoded arms' `fail!` is: the instruction's
/// span where the error has none, and the error's own where it has one.
///
/// # Safety
///
/// As [`safepoint`].
///
/// [ADR 0052]: ../../../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md
unsafe extern "C" fn buffer(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    op: u32,
    a: u32,
    b: u32,
) -> u32 {
    let host = (*ctx).host.cast::<Bridge>();
    let machine = (*host).machine;
    let budget = (*host).budget;

    let work = (*ctx).pending_work;
    (*ctx).pending_work = 0;

    let answered = {
        let machine = &mut *machine;
        machine.bulk_work += work;
        machine.sync(pc as usize);
        let frame = *machine.frames.last().expect("a native frame is executing");
        debug_assert_eq!(
            machine.mem.stack_index(frame.base) as u64,
            base,
            "compiled code and the frame stack disagree about which frame the buffer op is in"
        );
        let op = BufferOp::from_abi(op).expect("a code generator emitted a buffer op that is one");
        machine
            .safepoint(budget, frame.function, pc as usize)
            .and_then(|()| {
                // The frame's *address* rather than its index, for [`builtin`]'s
                // reason: the runtime is already holding the top frame and a slot
                // read needs a linear address.
                let base = frame.base;
                match op {
                    BufferOp::Alloc => {
                        let capacity = machine.mem.slot(base, b as Slot) as i64;
                        let owner = machine.alloc_buffer(capacity)?;
                        machine.mem.set_slot(base, a as Slot, owner);
                        Ok(())
                    }
                    BufferOp::AppendByte => {
                        let owner = machine.mem.slot(base, a as Slot);
                        let value = machine.mem.slot(base, b as Slot) as i64;
                        machine.append_byte(owner, value)
                    }
                    // The one that is not a `Machine` method, because its four
                    // operands, its eight refusals and its chunk loop are a page of
                    // code `encoded.rs` already keeps out of its dispatch loop.
                    // Called rather than copied, for that whole page's worth of
                    // reasons.
                    BufferOp::AppendBytes => {
                        let program = machine.program;
                        let args = program.arg_list(ArgsId(a));
                        super::encoded::append_bytes(
                            machine,
                            program,
                            budget,
                            base,
                            args,
                            frame.function,
                            pc as usize,
                        )
                    }
                    BufferOp::Finish => {
                        let owner = machine.mem.slot(base, b as Slot);
                        let text = machine.finish_buffer(owner)?;
                        machine.mem.set_slot(base, a as Slot, text);
                        Ok(())
                    }
                }
            })
            .map_err(|error| error.at(machine.span(frame.function, pc as usize)))
    };
    republish(ctx, host);
    match answered {
        Ok(()) => Outcome::Returned.abi(),
        Err(error) => {
            (*host).left = Some(error);
            Outcome::Raised.abi()
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
        // The caller now waits on this call, so its pc becomes the address it
        // resumes at — see [`suspended_at_the_call`].
        suspended_at_the_call(machine, pc);
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
    // The same table the encoded `CALL` arm asks, and the transition charged to
    // whichever of the two it turned out to be.
    let entry = (*host).entry_of(
        callee,
        |counts| counts.native_to_native_mediated += 1,
        |counts| counts.native_to_vm += 1,
    );
    let answered = match entry {
        Some(entry) => enter::<MASK>(host, entry, callee, callee_base, into),
        None => {
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
        let ctx = NativeCtx::new(
            host.cast::<c_void>(),
            machine.mem.words_ptr(),
            machine.mem.stack_origin(),
        )
        .over_heap((*host).table())
        .over_literals(machine.literals_ptr())
        .over_payload_words(machine.fixed_payload_words_ptr());
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

/// **A VM-to-native call**: the encoded `CALL` arm's callee, entered as machine
/// code.
///
/// This is the transition
/// [issue #369](https://github.com/myuon/cove/issues/369) exists for. Before it,
/// the dispatch loop consulted no tier table, so a compiled function called from
/// an encoded one stayed encoded and native coverage was whatever one entry point
/// happened to reach without ever going back through the VM. It is not
/// compositional until this exists, and the issue says so.
///
/// # What it does not do
///
/// Almost everything. `encoded::dispatch`'s `CALL` arm has already called the
/// *same* `open_frame` it always called, so the arguments are settled the same
/// way, at the same widths, out of the same slots, with the same arity refusal
/// and the same two depth checks. `dst` is the destination the same lowering
/// settled. What is left is the three steps that differ:
///
/// 1. the callee's `Frame` joins the stack, which is what makes its reference
///    slots walkable — the same push `entered!` makes;
/// 2. [`enter`] runs the compiled code, handed the frame and
///    [`Destination`]'s two indices;
/// 3. on a return the frame comes off and the caller carries on dispatching at
///    the instruction after the call, because the answer is already in `dst`.
///
/// Nothing is rebuilt and nothing is refinalized: the tier was installed before
/// the run and the entry is a pointer into a page that was made executable once.
///
/// # Why it is out of line
///
/// `encoded::dispatch`'s own note: "nothing whose cost a program does not pay
/// belongs inside `dispatch`". A run with no tier installed never reaches this,
/// and what it pays for the possibility is the one `Option` test
/// [`Machine::tiered`](Machine) makes.
///
/// A failure leaves its frames standing, because that is what the error's call
/// chain is read out of, and the caller's `fail!` adds the call's span only if
/// the error does not already carry one of its own.
#[inline(never)]
pub(super) fn from_encoded(
    machine: &mut Machine<'_>,
    budget: &Meter,
    entry: Entry,
    callee: FunctionId,
    callee_base: u64,
    caller_base: u64,
    dst: Slot,
) -> Result<(), RuntimeError> {
    // The tier is a `Box`, so this address is outside the `Machine` and the two
    // raw pointers below name disjoint memory. That is the reason it is boxed.
    let tier: *mut Tiering = machine
        .tier
        .as_deref_mut()
        .expect("a VM-to-native call is reached only with a tier installed");
    machine.frames.push(Frame {
        function: callee,
        base: callee_base,
        pc: 0,
        dst,
    });
    let into = Destination {
        base: caller_base,
        slot: dst,
    };
    let held: *mut Machine = machine;
    // Safety: `held` is the machine this borrow names and is not used through the
    // reference again while the bridge is live; `tier` is the installed tiering,
    // which the installer keeps alive for the run; the frame at `callee_base` is
    // the callee's, with its arguments in place from `open_frame`; and `into` is
    // the caller's own frame, which is below the callee's and cannot overlap it.
    unsafe {
        let mut bridge = Bridge::over(held, budget, tier);
        let out: *mut Bridge = &mut bridge;
        enter::<0>(out, entry, callee, callee_base, into)
    }
}

/// Marks the compiled frame on top of the stack as suspended at the call at `pc`,
/// once the callee's frame is open and just before it is pushed.
///
/// **A frame waiting on a call holds the pc it resumes at, one past the call.**
/// That is the encoded tier's convention — `entered!` syncs a `pc` that has
/// already moved past the `call` — and it is what every reader of a suspended
/// frame assumes: `Machine::call_chain` and the debugger's backtrace both read
/// such a frame at `pc - 1`. Compiled code hands a helper the pc of the
/// instruction it is on, and the helper syncs that for its safepoint and for a
/// refusal raised before the callee's frame exists, where the caller is still the
/// innermost frame and is read at its pc exactly. Left there once the callee runs,
/// it read one instruction early: an error raised below a compiled call named the
/// instruction *before* the call as its call site, which the blame for a fault in
/// a standard-library body — ADR 0058 — then reported as the primary span.
fn suspended_at_the_call(machine: &mut Machine<'_>, pc: u32) {
    machine.sync(pc as usize + 1);
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
        // Not renamed by a `Duration` destination, because `encoded.rs`'s
        // `NEG_INT` arm is not renamed by one either: it calls `overflowed`
        // directly rather than going through `int_arith`'s `named` closure.
        Some(Raise::NegOverflowed) => overflowed("negation"),
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
    /// The tier this session installs on the machine for the length of one call.
    ///
    /// Held here between calls rather than built per call, because the counts are
    /// the *session's*: a session is asked for the encoded tier and then for a
    /// compiled one over the same machine, and the two answers are compared.
    /// See [`Tiering`] for why installing it at all is what makes a VM-to-native
    /// call reachable from inside a session.
    tier: Option<Box<Tiering>>,
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
            tier: None,
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
    ///
    /// # The table is installed, not only asked
    ///
    /// `entries` is put on the machine for the length of the call, so that the
    /// **encoded `CALL` arm inside this call asks the same table**. That is what
    /// makes a VM-to-native call reachable from a session: a caller the table
    /// refuses runs on the dispatch loop, and a compiled callee it calls is
    /// entered as machine code rather than dispatched. It is taken back off
    /// before this returns, because the table is the caller's and the next call
    /// may name a different one.
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
        let functions = self.machine.program.functions.len();
        let capacity = self.machine.mem.chunk_capacity();
        // Safety: `entries` outlives this call — it is the caller's — and the
        // tier is taken back off the machine before this returns, so the erased
        // pointer is never installed for longer than the borrow it came from.
        let table = unsafe { erase(entries) };
        let mut tier = match self.tier.take() {
            Some(mut held) => {
                unsafe { held.aim(table) };
                held
            }
            None => Box::new(unsafe { Tiering::new(table, functions, capacity) }),
        };
        let entry = entries.entry(self.id);
        match entry {
            Some(_) => tier.counts.host_to_native += 1,
            None => {
                tier.counts.host_to_vm += 1;
                charge_refusal(&mut tier, self.id);
            }
        }
        self.machine.tier = Some(tier);
        let answer = match entry {
            Some(entry) => {
                let into = Destination {
                    base: self.result,
                    slot: 0,
                };
                let machine: *mut Machine = self.machine;
                let held: *mut Tiering = self
                    .machine
                    .tier
                    .as_deref_mut()
                    .expect("the tier was just installed");
                // Safety: the machine and the tier outlive the call below; `entry`
                // was compiled for `self.id`, whose frame is the one just pushed;
                // and `into` is the session's own result run, which is below that
                // frame and outlives it.
                let published = unsafe {
                    let mut bridge = Bridge::over(machine, self.budget, held);
                    let out: *mut Bridge = &mut bridge;
                    enter::<0>(out, entry, self.id, base, into)
                };
                // The one `Vec` a session still builds, and it is the boundary's
                // rather than the call's: a Rust caller asked for the words.
                published.map(|()| self.machine.mem.read_words(self.result, self.width))
            }
            None => {
                let code = self.machine.code();
                match code {
                    Ok(code) => self.machine.drive_from(&code, self.budget, floor),
                    Err(error) => Err(error),
                }
            }
        };
        self.tier = self.machine.tier.take();
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
    ///
    /// Every transition of every call, the sessions's own outermost entries
    /// included — see [`Tiers`] for why an edge is counted rather than a tier.
    pub fn tiers(&self) -> Tiers {
        self.tier
            .as_deref()
            .map_or_else(Tiers::default, Tiering::counts)
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
    NativeHelpers {
        safepoint,
        call,
        open,
        close,
        alloc,
        builtin,
        buffer,
        field_load,
        field_store,
    }
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
        open,
        close,
        alloc,
        builtin,
        buffer,
        field_load,
        field_store,
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

/// The open half of a direct call: [ADR 0055]'s "Direct native-to-native calls",
/// which it names as a later optimisation of the first tier and this is.
///
/// [`call`] does ten things and generated code waits for all of them. This does
/// the ones only the runtime can do and hands back the two facts emitted code
/// needs to do the rest itself — where the callee's code is, and where its frame
/// is. What moves into generated code is the argument copy, which the lowering
/// settled and a code generator therefore knows statically; what stays here is
/// everything that touches a `Vec` the runtime owns or an account it keeps.
///
/// # What it does not skip, and why that is the whole of the claim
///
/// Issue #365's decomposition measured the tier hop at 1.3% of the native arm and
/// the *safepoint* at 9.1%, so a direct call that dropped the poll would be a
/// speedup that was really a missing check. This takes it, in
/// [ADR 0040]'s order, with the same charge, at the same point in the call:
///
/// 1. the unpaid work goes from [`NativeCtx::pending_work`] into `bulk_work`;
/// 2. the frame's program counter is synchronised, so a collection walks a
///    current frame;
/// 3. cancellation, then fuel and the deadline, then the collector rendezvous;
/// 4. `admit_frame` against the embedder's `max_call_depth`, and `push_frame`,
///    whose `Overflow` is the stack segment's own bound. **Both** of the two
///    checks a runaway recursion is refused by are still here and still in that
///    order;
/// 5. the callee's `Frame`, pushed before a word of the callee runs, because a
///    frame is what makes the callee's reference slots walkable.
///
/// What it leaves out of `open_frame` is the argument copy and the arity compare
/// — the compare because a code generator that emitted a direct call has already
/// made it, and a mismatch falls back to [`call`] and its `wrong_arity`.
///
/// # A mixed call keeps the path it had
///
/// A callee with no compiled entry is not this function's business, and it does
/// not try: it calls [`call`] — the whole mediated helper, unchanged, arguments
/// and tier and answer and frame — and answers `entry: None` with the outcome.
/// That is the constraint #365 sets, and it is met by *calling the old path*
/// rather than by reimplementing it.
///
/// # Safety
///
/// As [`call`].
///
/// [ADR 0040]: ../../../../../docs/adr/0040-a-bound-outlives-its-backend.md
/// [ADR 0055]: ../../../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md
unsafe extern "C" fn open(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    callee: u32,
    args: u32,
    dst: u32,
) -> Opened {
    let host = (*ctx).host.cast::<Bridge>();
    let machine = (*host).machine;
    let budget = (*host).budget;
    let id = FunctionId(callee);

    // Asked without charging a transition: the mediated path below charges its
    // own, and a direct call charges `native_to_native_direct` once the frame is
    // open. Counting here as well would count a mixed call twice.
    let Some(entry) = (*(*(*host).tier).entries).entry(id) else {
        // The mediated helper, whole. A mixed call is the path it always was.
        return Opened {
            entry: None,
            base: u64::from(call(ctx, base, pc, callee, args, dst)),
        };
    };

    let work = (*ctx).pending_work;
    (*ctx).pending_work = 0;
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
            .and_then(|()| machine.admit_frame(budget, span))
            .and_then(|()| {
                let size = machine.program.function(id).frame_size();
                // `open_frame`'s own line, and its bare error: the two refusals
                // are told apart by their class and not by a span, and the
                // mediated path attaches none here either.
                machine
                    .mem
                    .push_frame(size)
                    .map_err(|Overflow| machine.too_deep_error())
            })
    };
    let callee_base = match opened {
        Ok(base) => base,
        Err(error) => {
            (*host).left = Some(error);
            return Opened {
                entry: None,
                base: u64::from(Outcome::Raised.abi()),
            };
        }
    };
    let index = {
        let machine = &mut *machine;
        // As in [`call_body`]: see [`suspended_at_the_call`].
        suspended_at_the_call(machine, pc);
        machine.frames.push(Frame {
            function: id,
            base: callee_base,
            pc: 0,
            dst: dst as Slot,
        });
        (*(*host).tier).counts.native_to_native_direct += 1;
        // Taken after the frame was pushed, and an index rather than a pointer,
        // which is ADR 0057's rule and is why a `push_frame` below this one is
        // harmless.
        machine.mem.stack_index(callee_base) as u64
    };
    // `push_frame` may have moved the words, and generated code is about to store
    // the arguments through the pointer it finds here.
    republish(ctx, host);
    Opened {
        entry: Some(entry),
        base: index,
    }
}

/// The close half of a direct call: the frame comes off, and the error the callee
/// named is built.
///
/// See [`cove_native::CloseFn`]. This is the half of [`enter`] that is not the
/// entry: the charge on every exit, the frame removed on a return and left
/// standing on anything else, and the callee's [`Raise`] turned into the sentence
/// the encoded tier would have produced.
///
/// Nothing is republished, and that is a property of the direct path rather than
/// an omission. A direct call hands the callee the *caller's* context, so every
/// helper the callee reached — its own safepoints, its own calls — stored the
/// current words pointer and chunk table into the context the caller will re-read.
/// `pop_frame` is a truncation and moves nothing. [`call`] republishes because the
/// callee it entered was given a context of its own.
///
/// # Safety
///
/// As [`call`]. The top frame is the callee's, which is what [`open`] left.
unsafe extern "C" fn close(ctx: *mut NativeCtx, outcome: u32, callee: u32) -> u32 {
    let host = (*ctx).host.cast::<Bridge>();
    let machine = &mut *(*host).machine;
    // Pending work is charged on every exit — a return, a raise and a stop alike,
    // which is ADR 0055's own requirement and `enter`'s own line.
    machine.bulk_work += (*ctx).pending_work;
    (*ctx).pending_work = 0;
    if outcome == Outcome::Returned.abi() {
        // The answer is already where it belongs: the callee wrote it before it
        // returned, so all that is left is to take its frame away.
        let frame = machine
            .frames
            .pop()
            .expect("a direct call's callee has a frame");
        machine.mem.pop_frame(frame.base);
    } else if outcome == Outcome::Raised.abi() {
        // A failure leaves its frames standing, because that is what the error's
        // call chain is read out of. The error is whole by the time it is stashed,
        // and if the runtime is already holding one it is the callee's and this
        // hands it straight back.
        let error = raised(machine, (*host).left.take(), FunctionId(callee), &*ctx);
        (*host).left = Some(error);
    }
    outcome
}

/// One component of the call path, again, and each one out of line./// One component of the call path, again, and each one out of line.
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
    let ctx = NativeCtx::new(
        host.cast::<c_void>(),
        machine.mem.words_ptr(),
        machine.mem.stack_origin(),
    )
    .over_heap((*host).table())
    .over_literals(machine.literals_ptr())
    .over_payload_words(machine.fixed_payload_words_ptr());
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
        let held = NativeCtx::new(
            host.cast::<c_void>(),
            machine.mem.words_ptr(),
            machine.mem.stack_origin(),
        )
        .over_heap((*host).table())
        .over_literals(machine.literals_ptr())
        .over_payload_words(machine.fixed_payload_words_ptr());
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
