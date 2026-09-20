//! What the scalar and control-flow slice of the native tier actually does,
//! run — once, for whichever code generator is asked for.
//!
//! # One suite, two arms
//!
//! There are two code generators in this crate and the reason for the second is
//! to measure it against the first. So the expectations are written once, over
//! the [`Arm`] trait, and each arm's test file is a list of one-line wrappers:
//! two arms that agreed with two different suites would not have been compared.
//!
//! # Why the expectations are literals
//!
//! These tests cannot compare against `Machine`, because `cove-native` does
//! not depend on `cove-runtime` and must not — see that crate's documentation
//! for the inversion. So the encoded tier's behaviour is written out here as
//! literal expectations, and **every one of them cites the arm of
//! `crates/cove-runtime/src/vm/exec/encoded.rs` it mirrors, by file and
//! line**, so that a reader can check the mirror rather than trust it.
//!
//! The line numbers are as of the commit that added this file. They are a
//! pointer to an arm, not a contract; the arm's *name* is given beside every
//! one of them because a name survives an edit above it.
//!
//! A differential test against the real VM is the right test and it is the
//! next slice's, once the tier is reachable from a `Machine`. Until then this
//! is the strongest thing available, and it is stronger than nothing by
//! exactly the amount of trouble a wrong `+` would cause later.

#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use cove_diag::{FileId, Span};
use cove_ir::{
    Arg, ArgsId, ArithOp, CaseId, CmpOp, Compare, Function, FunctionId, Inst, Layout, LayoutId,
    Len, Num, Program, RefMap, Repr, SiteId, Slot, Storage, StrId, Table, TableId, Validation,
};
use cove_native::{
    Entry, GrowableOp, IntrinsicCode, IntrinsicProtocol, NativeCtx, NativeHelpers, Opened, Outcome,
    Raise, RunOp, WindowCode,
};
use cove_native::{HEAP_CHUNK_SHIFT, HEAP_CHUNK_WORDS, HEAP_ORIGIN_WORDS};

// --- the safepoint helper -----------------------------------------------------

thread_local! {
    /// Every safepoint this thread's compiled code has taken, as
    /// `(pc, work_since_last)`.
    ///
    /// A thread-local rather than a static, because `cargo test` runs these
    /// in parallel and each test body is one thread. The helper is a plain
    /// `extern "C"` function and therefore cannot close over a recorder, so
    /// the recorder has to be reachable from a bare function — which is what
    /// a thread-local is for.
    pub static POLLS: RefCell<Vec<(u32, u64)>> = const { RefCell::new(Vec::new()) };
    /// How many safepoints to allow before answering "stop".
    pub static POLLS_ALLOWED: Cell<usize> = const { Cell::new(usize::MAX) };
    /// What `NativeCtx::poll_at` is published as — the unpaid work a backedge
    /// must have gathered before it calls the helper at all.
    ///
    /// Nought, the default, is "poll at every backedge", which is what every
    /// case written before the threshold existed asserts and what a context
    /// that was published no budget gets. A case that wants the stride tested
    /// sets it with [`poll_at`].
    ///
    /// The runtime publishes what is *left* of `SAFEPOINT_STRIDE` and
    /// republishes it at every charge point; this double publishes one number
    /// and leaves it there, which is the same arithmetic when nothing but
    /// compiled code is charging: the accumulator is cleared at each poll, so
    /// a fixed threshold is a fixed stride.
    pub static POLL_AT: Cell<u64> = const { Cell::new(0) };
    /// A word of the frame the safepoint helper reads, by index into `words`.
    ///
    /// How the claim in `cove_native::abi` — "at a safepoint every live reference
    /// is already in the slot the frame's static map names" — is *tested* rather
    /// than argued: the helper is the collector's stand-in, and what it can see
    /// is exactly what a root walk can see. If an arm kept a reference only in a
    /// register across the call, the word this reads would be stale or zero.
    pub static WATCHED: Cell<Option<usize>> = const { Cell::new(None) };
    /// What [`WATCHED`] held at each safepoint, in order.
    pub static WATCHED_SAW: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// The runtime's half of the boundary, as a test double.
///
/// A real one is where [ADR 0040]'s three-step order lives — cancellation and
/// task-local stops, then fuel and deadline accounting, then the collector
/// rendezvous. This one records and optionally stops, which is the whole of
/// what compiled code can observe about it.
///
/// [ADR 0040]: ../../../../docs/adr/0040-a-bound-outlives-its-backend.md
unsafe extern "C" fn safepoint(ctx: *mut NativeCtx, pc: u32, work: u64) -> bool {
    POLLS.with(|polls| polls.borrow_mut().push((pc, work)));
    if let Some(at) = WATCHED.with(Cell::get) {
        // Safety: the caller set `at` to an index inside the `words` it handed
        // the entry point.
        let word = (*ctx).words.add(at).read();
        WATCHED_SAW.with(|saw| saw.borrow_mut().push(word));
    }
    let taken = POLLS.with(|polls| polls.borrow().len());
    taken < POLLS_ALLOWED.with(Cell::get)
}

/// One call compiled code made through [`CallFn`](cove_native::CallFn).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Called {
    pub base: u64,
    pub pc: u32,
    pub callee: u32,
    pub args: u32,
    pub dst: u32,
    /// The unpaid work the caller published before handing over.
    ///
    /// A call is a safepoint — a callee may allocate and an allocation may
    /// collect — so the work goes over with it, and this is what says the arm
    /// published it rather than dropping it.
    pub work: u64,
}

thread_local! {
    /// Every call this thread's compiled code has made, in order.
    pub static CALLS: RefCell<Vec<Called>> = const { RefCell::new(Vec::new()) };
    /// What the next call answers, and the one after it: an [`Outcome`] as its
    /// ABI number, taken from the front.
    ///
    /// Empty means "return", which is what a leaf answer is. A script is how a
    /// raise and a stop coming *out of a callee* are tested without a runtime to
    /// raise one.
    pub static CALL_ANSWERS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

/// The runtime's call helper, as a test double.
///
/// A real one opens the callee's frame with the runtime's own `open_frame` and
/// runs it on whichever tier it is on. This one records the hand-over and writes
/// one word — `callee * 1000 + dst`, a number no other part of a frame holds —
/// into the destination slot, which is enough to say that the caller named the
/// slot the IR named and that the answer arrived where the next instruction
/// reads it.
///
/// # Safety
///
/// `ctx` is the pointer the entry point was called with, and `base + dst` is a
/// slot of the caller's frame, which `crate::subset`'s `supported` bounded.
unsafe extern "C" fn call(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    callee: u32,
    args: u32,
    dst: u32,
) -> u32 {
    let work = (*ctx).pending_work;
    CALLS.with(|calls| {
        calls.borrow_mut().push(Called {
            base,
            pc,
            callee,
            args,
            dst,
            work,
        })
    });
    // A real helper charges what it was handed, so a real one clears it.
    (*ctx).pending_work = 0;
    let scripted = CALL_ANSWERS.with(|answers| {
        let mut answers = answers.borrow_mut();
        if answers.is_empty() {
            None
        } else {
            Some(answers.remove(0))
        }
    });
    match scripted {
        None | Some(0) => {
            let at = (*ctx).words.add((base + u64::from(dst)) as usize);
            at.write(u64::from(callee) * 1000 + u64::from(dst));
            Outcome::Returned.abi()
        }
        Some(other) => other,
    }
}

/// The runtime's open half, as a test double: the runtime finishes the call.
///
/// A direct call asks where the callee's code is and this answers *nowhere* —
/// `entry: None`, which is the ordinary answer for a callee the tier has not
/// compiled and means the runtime made the whole call itself. So it does: the
/// same recording and the same one recognisable word [`call`] writes, and that
/// call's outcome in [`Opened::base`].
///
/// Every expectation in this file therefore holds whether the arm under test
/// emitted a direct call or a mediated one, which is what lets the one arm that
/// can emit both be held to the same suite twice. A double that handed back a
/// real entry would need a real callee and a real frame to give it, and that is
/// a test about the *direct* protocol rather than about the slice — it lives in
/// `tests/template.rs`, beside the only arm that emits one.
///
/// # Safety
///
/// As [`call`].
unsafe extern "C" fn open(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    callee: u32,
    args: u32,
    dst: u32,
) -> Opened {
    Opened {
        entry: None,
        base: u64::from(call(ctx, base, pc, callee, args, dst)),
    }
}

/// The runtime's close half, as a test double, and it is a tripwire.
///
/// This file's [`open`] never opens a frame, so nothing in this file can reach
/// this: a direct call that got here would be one whose caller entered a callee
/// that was never handed to it. Answering something would let that pass quietly.
///
/// # Safety
///
/// Reads nothing through any of its arguments.
unsafe extern "C" fn close(_ctx: *mut NativeCtx, outcome: u32, callee: u32) -> u32 {
    panic!(
        "the close helper was reached for callee {callee} with outcome {outcome}, and this \
         suite's `open` never opens a frame for one"
    )
}

// --- the allocation helper ----------------------------------------------------

/// How many words apart two test allocations are placed.
///
/// Generous, so that a case can write a payload without working out whether it
/// has run into the next object: a header and sixty-three payload words is wider
/// than anything here allocates.
pub const ALLOC_STRIDE: u64 = 64;

thread_local! {
    /// Every allocation this thread's compiled code has asked for, as
    /// `(pc, layout, len)`.
    ///
    /// The three numbers are the whole of what [`cove_native::AllocFn`] carries,
    /// so a case reads them to say the arm handed over what the instruction said
    /// — including that `Len::Fixed` is nought and `Len::Slot` is the *word* and
    /// not a narrowed copy of it.
    pub static ALLOCS: RefCell<Vec<(u32, u32, i64)>> = const { RefCell::new(Vec::new()) };
    /// The heap word index the next test allocation is placed at.
    pub static ALLOC_AT: Cell<u64> = const { Cell::new(1) };
    /// How many allocations to answer before refusing.
    ///
    /// The refusal a real allocation makes when nothing this run holds can be
    /// reclaimed — "this run has no memory left" — which the helper reports as a
    /// zero and compiled code has to leave with as [`Raise::Called`].
    pub static ALLOCS_ALLOWED: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// The runtime's allocation helper, as a test double.
///
/// A real one is `Machine::allocate` behind [ADR 0040]'s three steps; this one
/// bumps a pointer into the case's own [`Heap`] and writes the header
/// `mem::header` would have written, which is exactly as much of it as compiled
/// code can observe.
///
/// It **also reads [`WATCHED`]**, and that is the point of it rather than a
/// convenience. An allocation is the first thing compiled code does that can
/// cause a collection, so a collector walking this frame is the thing standing
/// between a half-built object and a swept one — and what the walk can see is
/// exactly what this can see. A case that watches a reference slot and allocates
/// is asserting `cove_native::abi`'s "every live reference is already in the slot
/// the frame's static map names", rather than believing it.
///
/// # Safety
///
/// `ctx` is the pointer the entry point was called with, and `ctx.chunks` is the
/// table of a [`Heap`] with room at [`ALLOC_AT`].
///
/// [ADR 0040]: ../../../../docs/adr/0040-a-bound-outlives-its-backend.md
unsafe extern "C" fn alloc(ctx: *mut NativeCtx, pc: u32, layout: u32, len: i64) -> u64 {
    ALLOCS.with(|held| held.borrow_mut().push((pc, layout, len)));
    if let Some(at) = WATCHED.with(Cell::get) {
        // Safety: the caller set `at` to an index inside the `words` it handed
        // the entry point.
        let word = (*ctx).words.add(at).read();
        WATCHED_SAW.with(|saw| saw.borrow_mut().push(word));
    }
    if ALLOCS.with(|held| held.borrow().len()) > ALLOCS_ALLOWED.with(Cell::get) {
        // A real one stashes a whole `RuntimeError` and answers nought; there is
        // no error to stash here, and nought is the whole of the ABI.
        return 0;
    }
    let at = ALLOC_AT.with(Cell::get);
    ALLOC_AT.with(|held| held.set(at + ALLOC_STRIDE));
    // `mem::header`: the layout in the high half and the length field in the low
    // one. The length is narrowed the way `Machine::allocate`'s `u32::try_from`
    // narrows it, and a case that wants the refusal asks for it with
    // [`ALLOCS_ALLOWED`] instead of an unrepresentable length.
    let chunk = *(*ctx).chunks.add((at >> HEAP_CHUNK_SHIFT) as usize);
    chunk
        .add((at & (HEAP_CHUNK_WORDS - 1)) as usize)
        .write((u64::from(layout) << 32) | (len as u64 & u64::from(u32::MAX)));
    HEAP_ORIGIN_WORDS + at
}

pub fn allocations() -> Vec<(u32, u32, i64)> {
    ALLOCS.with(|held| held.borrow().clone())
}

/// Forgets every allocation and puts the bump pointer back.
pub fn forget_allocations() {
    ALLOCS.with(|held| held.borrow_mut().clear());
    ALLOC_AT.with(|held| held.set(1));
    ALLOCS_ALLOWED.with(|held| held.set(usize::MAX));
}

/// Refuses every allocation after the first `allowed`. See [`ALLOCS_ALLOWED`].
pub fn allocations_allowed(allowed: usize) {
    ALLOCS_ALLOWED.with(|held| held.set(allowed));
}

// --- the intrinsic helper -----------------------------------------------------

/// One builtin compiled code handed back through
/// [`IntrinsicFn`](cove_native::IntrinsicFn).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mediated {
    pub base: u64,
    pub pc: u32,
    pub dst: u32,
    pub site: u32,
    pub args: u32,
    /// The unpaid work the caller published before handing over.
    pub work: u64,
}

thread_local! {
    /// Every builtin this thread's compiled code handed to the runtime, in order.
    pub static MEDIATED: RefCell<Vec<Mediated>> = const { RefCell::new(Vec::new()) };
    /// What the next mediated builtin answers, taken from the front.
    pub static MEDIATED_ANSWERS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

/// The runtime's intrinsic helper, as a test double.
///
/// A real one is `Machine::call_intrinsic`, whole. This one records the hand-over
/// and writes one word into `dst` — `builtin * 1000 + dst`, a number no other
/// part of a frame holds — so a case can say the cold path was taken *and* that
/// the answer landed where the instruction said.
///
/// # Safety
///
/// As [`alloc`]. `base` indexes into the words the entry point was given and
/// `dst` is a slot of the frame there.
unsafe extern "C" fn intrinsic(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    dst: u32,
    site: u32,
    args: u32,
) -> u32 {
    MEDIATED.with(|held| {
        held.borrow_mut().push(Mediated {
            base,
            pc,
            dst,
            site,
            args,
            work: (*ctx).pending_work,
        })
    });
    (*ctx).pending_work = 0;
    let answer = MEDIATED_ANSWERS.with(|held| {
        let mut held = held.borrow_mut();
        (!held.is_empty()).then(|| held.remove(0))
    });
    match answer {
        Some(outcome) if outcome != Outcome::Returned.abi() => outcome,
        _ => {
            (*ctx)
                .words
                .add((base + u64::from(dst)) as usize)
                .write(u64::from(site) * 1000 + u64::from(dst));
            Outcome::Returned.abi()
        }
    }
}

pub fn mediated() -> Vec<Mediated> {
    MEDIATED.with(|held| held.borrow().clone())
}

pub fn forget_mediated() {
    MEDIATED.with(|held| held.borrow_mut().clear());
    MEDIATED_ANSWERS.with(|held| held.borrow_mut().clear());
}

/// Scripts what the next mediated builtins answer.
pub fn mediated_answers(outcomes: &[Outcome]) {
    MEDIATED_ANSWERS
        .with(|held| *held.borrow_mut() = outcomes.iter().map(|outcome| outcome.abi()).collect());
}

// --- the growable-buffer helper -----------------------------------------------

/// One growable-run operation compiled code handed back through
/// [`GrowableFn`](cove_native::GrowableFn).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Built {
    pub base: u64,
    pub pc: u32,
    pub op: u32,
    pub a: u32,
    pub b: u32,
    /// The unpaid work the caller published before handing over.
    pub work: u64,
}

thread_local! {
    /// Every buffer operation this thread's compiled code handed over, in order.
    pub static BUILT: RefCell<Vec<Built>> = const { RefCell::new(Vec::new()) };
    /// What the next one answers, taken from the front.
    pub static BUILT_ANSWERS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    /// The capacity the next ensure that answers `Returned` leaves its owner's
    /// store with, if a case asked for one. See [`room_on_ensure`].
    pub static ROOM_ON_ENSURE: Cell<Option<u32>> = const { Cell::new(None) };
}

/// The runtime's growable-buffer helper, as a test double.
///
/// A real one is `Machine::alloc_buffer`, `Machine::finish_buffer`,
/// `Machine::ensure_growable` or `Machine::commit_growable`, whole. This one
/// records
/// the hand-over and writes one word — `op * 1000 + a`, a number no other part of
/// a frame holds — into the destination *of the two operations that have one*.
///
/// That last clause is the part worth stating. `a` is `dst` for
/// [`GrowableOp::Alloc`], [`GrowableOp::Finish`] and [`GrowableOp::FinishWords`];
/// it is the owner's slot for every other member — [`GrowableOp::EnsureBytes`],
/// [`GrowableOp::CommitBytes`] and the rest — which the real helper reads and
/// never writes. A double that wrote through it in every case would be asserting
/// a store the runtime does not make.
///
/// # Safety
///
/// As [`alloc`]. `base` indexes into the words the entry point was given.
unsafe extern "C" fn growable(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    op: u32,
    a: u32,
    b: u32,
) -> u32 {
    BUILT.with(|held| {
        held.borrow_mut().push(Built {
            base,
            pc,
            op,
            a,
            b,
            work: (*ctx).pending_work,
        })
    });
    (*ctx).pending_work = 0;
    let answer = BUILT_ANSWERS.with(|held| {
        let mut held = held.borrow_mut();
        (!held.is_empty()).then(|| held.remove(0))
    });
    match answer {
        Some(outcome) if outcome != Outcome::Returned.abi() => outcome,
        _ => {
            let ensure = op == GrowableOp::EnsureBytes.abi() || op == GrowableOp::EnsureWords.abi();
            let room = ROOM_ON_ENSURE.with(|room| if ensure { room.take() } else { None });
            if let Some(capacity) = room {
                // The store's header length is its capacity: rewritten in place,
                // over test heap words a case has left free behind it.
                let owner = (*ctx).words.add((base + u64::from(a)) as usize).read();
                let store = heap_word_ptr(ctx, owner + 2).read();
                let header = heap_word_ptr(ctx, store);
                header.write((header.read() >> 32 << 32) | u64::from(capacity));
            }
            if op == GrowableOp::Alloc.abi()
                || op == GrowableOp::Finish.abi()
                || op == GrowableOp::FinishWords.abi()
            {
                (*ctx)
                    .words
                    .add((base + u64::from(a)) as usize)
                    .write(u64::from(op) * 1000 + u64::from(a));
            }
            Outcome::Returned.abi()
        }
    }
}

pub fn built() -> Vec<Built> {
    BUILT.with(|held| held.borrow().clone())
}

pub fn forget_built() {
    BUILT.with(|held| held.borrow_mut().clear());
    BUILT_ANSWERS.with(|held| held.borrow_mut().clear());
    ROOM_ON_ENSURE.with(|room| room.set(None));
}

/// Makes the next ensure the double answers `Returned` leave its owner's store
/// with `capacity` units of room, as a growth would: the one thing a case needs
/// of the runtime to see a window rejoin its store row after a cold ensure.
pub fn room_on_ensure(capacity: u32) {
    ROOM_ON_ENSURE.with(|room| room.set(Some(capacity)));
}

/// Scripts what the next buffer operations answer.
pub fn built_answers(outcomes: &[Outcome]) {
    BUILT_ANSWERS
        .with(|held| *held.borrow_mut() = outcomes.iter().map(|outcome| outcome.abi()).collect());
}

// --- the run-copy helper ------------------------------------------------------

/// One run copy compiled code handed back through
/// [`RunCopyFn`](cove_native::RunCopyFn).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Copied {
    pub base: u64,
    pub pc: u32,
    pub args: u32,
    /// Which [`cove_native::RunOp`] it was, as the integer the arm passed.
    pub kind: u32,
    pub elem: u32,
    /// The unpaid work the caller published before handing over.
    pub work: u64,
}

thread_local! {
    /// Every run copy this thread's compiled code handed over, in order.
    pub static COPIED: RefCell<Vec<Copied>> = const { RefCell::new(Vec::new()) };
    /// What the next one answers, taken from the front.
    pub static COPIED_ANSWERS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

/// The runtime's run-copy helper, as a test double.
///
/// A real one is `encoded::run_copy_bytes` or `encoded::run_copy_words`, whole.
/// This one records the hand-over and writes nothing: the instruction has no
/// destination slot — it writes into the object `dst` names — so a double that
/// wrote into the frame would be asserting a store the runtime does not make.
///
/// # Safety
///
/// As [`alloc`].
unsafe extern "C" fn run_copy(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    args: u32,
    kind: u32,
    elem: u32,
) -> u32 {
    COPIED.with(|held| {
        held.borrow_mut().push(Copied {
            base,
            pc,
            args,
            kind,
            elem,
            work: (*ctx).pending_work,
        })
    });
    (*ctx).pending_work = 0;
    let answer = COPIED_ANSWERS.with(|held| {
        let mut held = held.borrow_mut();
        (!held.is_empty()).then(|| held.remove(0))
    });
    answer.unwrap_or(Outcome::Returned.abi())
}

pub fn copied() -> Vec<Copied> {
    COPIED.with(|held| held.borrow().clone())
}

pub fn forget_copied() {
    COPIED.with(|held| held.borrow_mut().clear());
    COPIED_ANSWERS.with(|held| held.borrow_mut().clear());
}

/// Scripts what the next run copies answer.
pub fn copied_answers(outcomes: &[Outcome]) {
    COPIED_ANSWERS
        .with(|held| *held.borrow_mut() = outcomes.iter().map(|outcome| outcome.abi()).collect());
}

// --- the field-access cold path ------------------------------------------------

/// One field access compiled code handed back through
/// [`FieldLoadFn`](cove_native::FieldLoadFn)/[`FieldStoreFn`](cove_native::FieldStoreFn).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fielded {
    pub pc: u32,
    pub addr: u64,
    pub at: u32,
    pub width: u32,
    /// `into` for a load, `from` for a store — both linear addresses.
    pub other: u64,
}

thread_local! {
    /// Every field access this thread's compiled code handed to the runtime, in
    /// order.
    pub static FIELDED: RefCell<Vec<Fielded>> = const { RefCell::new(Vec::new()) };
    /// What the next one answers, taken from the front.
    pub static FIELDED_ANSWERS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

/// The heap word at linear address `addr`, as [`alloc`]'s own chunk arithmetic.
///
/// # Safety
///
/// `ctx.chunks` is the table of a [`Heap`] with a chunk committed at `addr`.
unsafe fn heap_word_ptr(ctx: *mut NativeCtx, addr: u64) -> *mut u64 {
    let index = addr - HEAP_ORIGIN_WORDS;
    let chunk = *(*ctx).chunks.add((index >> HEAP_CHUNK_SHIFT) as usize);
    chunk.add((index & (HEAP_CHUNK_WORDS - 1)) as usize)
}

/// The stack word at linear address `addr`: [`NativeCtx::words`] and
/// [`NativeCtx::stack_origin`], the way every stack address in this suite
/// resolves one.
///
/// # Safety
///
/// `addr` is inside the segment `ctx.words` was given.
unsafe fn stack_word_ptr(ctx: *mut NativeCtx, addr: u64) -> *mut u64 {
    (*ctx).words.add((addr - (*ctx).stack_origin) as usize)
}

/// The runtime's field-load helper, as a test double.
///
/// A real one is `Machine::checked` and a copy; this one records the hand-over
/// and writes a recognizable word per word of the answer — `addr * 1000 + at *
/// 10 + word`, a number no other part of a frame holds — into `into`, exactly
/// as [`builtin`] writes one recognizable word into `dst`.
///
/// # Safety
///
/// `into` is `width` words of the segment `ctx.words` was given.
unsafe extern "C" fn field_load(
    ctx: *mut NativeCtx,
    pc: u32,
    addr: u64,
    at: u32,
    width: u32,
    into: u64,
) -> u32 {
    FIELDED.with(|held| {
        held.borrow_mut().push(Fielded {
            pc,
            addr,
            at,
            width,
            other: into,
        })
    });
    let answer = FIELDED_ANSWERS.with(|held| {
        let mut held = held.borrow_mut();
        (!held.is_empty()).then(|| held.remove(0))
    });
    match answer {
        Some(outcome) if outcome != Outcome::Returned.abi() => outcome,
        _ => {
            for word in 0..u64::from(width) {
                let value = addr * 1000 + u64::from(at) * 10 + word;
                stack_word_ptr(ctx, into + word).write(value);
            }
            Outcome::Returned.abi()
        }
    }
}

/// [`field_load`], the other direction. A real store has no answer to leave —
/// the runtime only reads what compiled code already wrote — so this records
/// the hand-over and nothing more.
///
/// # Safety
///
/// As [`field_load`].
unsafe extern "C" fn field_store(
    _ctx: *mut NativeCtx,
    pc: u32,
    addr: u64,
    at: u32,
    width: u32,
    from: u64,
) -> u32 {
    FIELDED.with(|held| {
        held.borrow_mut().push(Fielded {
            pc,
            addr,
            at,
            width,
            other: from,
        })
    });
    let answer = FIELDED_ANSWERS.with(|held| {
        let mut held = held.borrow_mut();
        (!held.is_empty()).then(|| held.remove(0))
    });
    match answer {
        Some(outcome) if outcome != Outcome::Returned.abi() => outcome,
        _ => Outcome::Returned.abi(),
    }
}

pub fn fielded() -> Vec<Fielded> {
    FIELDED.with(|held| held.borrow().clone())
}

pub fn forget_fielded() {
    FIELDED.with(|held| held.borrow_mut().clear());
    FIELDED_ANSWERS.with(|held| held.borrow_mut().clear());
}

/// Scripts what the next field accesses answer.
pub fn fielded_answers(outcomes: &[Outcome]) {
    FIELDED_ANSWERS
        .with(|held| *held.borrow_mut() = outcomes.iter().map(|outcome| outcome.abi()).collect());
}

thread_local! {
    /// Every string order this thread's compiled code handed to the runtime,
    /// as the two words, in order.
    pub static ORDERED: RefCell<Vec<(u64, u64)>> = const { RefCell::new(Vec::new()) };
}

/// The runtime's string-order helper, as a test double that is not a stub.
///
/// `Machine::order_strings` compares two string objects' bytes and reads a null
/// address as the empty string; this does the same over a [`Heap`] — the header's
/// low half is the byte count and the payload is eight bytes a word, least
/// significant first — through [`heap_word_ptr`], and records the hand-over. It
/// touches nothing else in `ctx`, which is the leaf contract seen from the
/// callee's side.
///
/// # Safety
///
/// `a` and `b` are each nought or the address of a string object in the heap
/// `ctx.chunks` names.
unsafe extern "C" fn order_str(ctx: *mut NativeCtx, a: u64, b: u64) -> i64 {
    ORDERED.with(|held| held.borrow_mut().push((a, b)));
    let bytes = |addr: u64| -> Vec<u8> {
        if addr == 0 {
            return Vec::new();
        }
        let len = (*heap_word_ptr(ctx, addr) & 0xffff_ffff) as usize;
        (0..len)
            .map(|at| (*heap_word_ptr(ctx, addr + 1 + (at / 8) as u64) >> ((at % 8) * 8)) as u8)
            .collect()
    };
    match bytes(a).cmp(&bytes(b)) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

pub fn ordered() -> Vec<(u64, u64)> {
    ORDERED.with(|held| held.borrow().clone())
}

pub fn forget_ordered() {
    ORDERED.with(|held| held.borrow_mut().clear());
}

pub fn helpers() -> NativeHelpers {
    NativeHelpers {
        safepoint,
        call,
        open,
        close,
        alloc,
        intrinsic,
        growable,
        run_copy,
        field_load,
        field_store,
        order_str,
    }
}

pub fn calls() -> Vec<Called> {
    CALLS.with(|calls| calls.borrow().clone())
}

pub fn forget_calls() {
    CALLS.with(|calls| calls.borrow_mut().clear());
    CALL_ANSWERS.with(|answers| answers.borrow_mut().clear());
}

/// Scripts what the next calls answer. See [`CALL_ANSWERS`].
pub fn calls_answer(outcomes: &[Outcome]) {
    CALL_ANSWERS.with(|answers| {
        *answers.borrow_mut() = outcomes.iter().map(|outcome| outcome.abi()).collect()
    });
}

pub fn polls() -> Vec<(u32, u64)> {
    POLLS.with(|polls| polls.borrow().clone())
}

/// Publishes `at` as the poll threshold for the next entry. See [`POLL_AT`].
pub fn poll_at(at: u64) {
    POLL_AT.with(|poll_at| poll_at.set(at));
}

pub fn forget_polls() {
    POLLS.with(|polls| polls.borrow_mut().clear());
    POLLS_ALLOWED.with(|allowed| allowed.set(usize::MAX));
    POLL_AT.with(|poll_at| poll_at.set(0));
    WATCHED.with(|watched| watched.set(None));
    WATCHED_SAW.with(|saw| saw.borrow_mut().clear());
}

/// Asks the safepoint and allocation helpers to read word `at` of the frame's
/// segment.
pub fn watch(at: usize) {
    WATCHED.with(|watched| watched.set(Some(at)));
}

/// Stops watching, so that a later case in this thread is not handed a reading it
/// did not ask for.
pub fn watch_nothing() {
    WATCHED.with(|watched| watched.set(None));
    WATCHED_SAW.with(|saw| saw.borrow_mut().clear());
}

pub fn watched() -> Vec<u64> {
    WATCHED_SAW.with(|saw| saw.borrow().clone())
}

// --- the arm under test -------------------------------------------------------

/// One code generator, as the suite below uses it.
///
/// The four operations every arm has: make one, compile a function, make the
/// code executable, and take an entry point. Nothing else is asked of an arm,
/// and nothing in the suite names a concrete one.
pub trait Arm {
    /// What this arm's `compile` answers. Both arms' are `Copy` and carry the
    /// `FunctionId` and the code size; the suite reads only the size.
    type Handle: Copy;

    fn new(helpers: NativeHelpers) -> Self;
    fn compile(&mut self, program: &Program, id: FunctionId) -> Option<Self::Handle>;
    fn finalize(&mut self);
    fn entry(&self, handle: Self::Handle) -> Entry;
    /// How many bytes of machine code a compiled function is, which both arms'
    /// handles carry: what a case comparing the code two shapes cost reads.
    fn code_bytes(handle: Self::Handle) -> u32;
    /// What the compiled function's ADR 0062 buffer windows were, by pattern.
    fn window_code(handle: Self::Handle) -> WindowCode;
    /// What the compiled function's ADR 0064 mediated intrinsic calls were, by
    /// variant.
    fn intrinsic_code(handle: Self::Handle) -> IntrinsicCode;
    /// Whether this arm can say how many *bytes* a window was.
    ///
    /// The sites are a fact about the IR and both arms count them the same; the
    /// bytes are a fact about a code generator's layout and only one arm has
    /// them. A constant rather than a suite that shrugs at whichever answer it
    /// gets: "this arm attributes bytes" and "this arm does not" are both claims
    /// worth a failing test if they stop being true. See
    /// [`WindowCode`](cove_native::WindowCode).
    const ATTRIBUTES_WINDOWS: bool;
    /// Whether this arm can say how many *bytes* a mediated intrinsic call was.
    ///
    /// A second constant rather than a reuse of the one above, and not from
    /// symmetry: attribution is a property of the *method that emits the thing*,
    /// not of the arm. The template arm can charge a window because its
    /// `Emit::window` is contiguous and an intrinsic call because its
    /// `Emit::intrinsic_call` is, which are two facts about two methods, and one
    /// of them could stop being true on its own — a cold half moved out of line
    /// would do it. Two constants let a test say which. See
    /// [`IntrinsicCode`](cove_native::IntrinsicCode).
    const ATTRIBUTES_INTRINSIC_CALLS: bool;
}

// --- building a program by hand ----------------------------------------------

// `LayoutId(0)` is `LayoutId::FREE` and names no value, so the table below
// starts with it and nothing here uses it.
pub const INT: LayoutId = LayoutId(1);
pub const BOOL: LayoutId = LayoutId(2);
pub const UNIT: LayoutId = LayoutId(3);
/// Two `Int` words inline, for the only multi-word thing this slice moves.
pub const PAIR: LayoutId = LayoutId(4);
pub const DURATION: LayoutId = LayoutId(5);
/// An `Int` and a reference inline, which is a `struct Pair { n: Int, s: String }`.
pub const REF_PAIR: LayoutId = LayoutId(6);
/// One `Repr::Tag` word, which is what an enum with no payload is.
pub const TAG: LayoutId = LayoutId(7);
/// One reference word.
pub const REF: LayoutId = LayoutId(8);
/// An `Int` and a `Repr::Host` word inline, which no arm lowers.
pub const HOST_PAIR: LayoutId = LayoutId(9);
/// A family of *no* words, which is what an empty struct lowers to.
///
/// It is the zero-width return ADR 0057 says writes nothing, and it is a real
/// shape rather than an invented one: `Unit` is one word in this IR —
/// `Layout::word("Unit", Repr::Unit)` above — and `struct Empty {}` is none. The
/// lowering gives a width-0 destination the next free slot number, which may be
/// one the frame does not have, so a return that formed the address anyway would
/// be writing into whatever is above the frame.
pub const EMPTY: LayoutId = LayoutId(10);
/// A `Vector<Int>`, whose elements are one word each.
///
/// [`Shape::Vector`](cove_ir::Shape::Vector) is two payload words — the element
/// count and the store — and the elements live in the store rather than here,
/// which is what makes `is` defined for a `Vector` and why a `push` that grows
/// replaces a word of this object instead of moving it.
pub const VECTOR: LayoutId = LayoutId(12);
/// A `Vector<Pair>`, whose elements are **two** words each.
///
/// The stride case: `set_payload_run` writes `stride` words at
/// `len * stride`, so a one-word element cannot tell a lowering that multiplied
/// from one that did not.
pub const PAIR_VECTOR: LayoutId = LayoutId(13);
/// The store of a `Vector<Int>`: a growable run of elements.
///
/// What the emitted push reads out of it is its header's length — the capacity —
/// and nothing else, so this is here to be *named* by an object rather than to be
/// told apart from `Elements`' other form.
pub const STORE: LayoutId = LayoutId(14);

/// `Option<Int>`, what `Vector<Int>.set` answered while it was a builtin this
/// crate lowered by name.
///
/// Built by [`program`] with `cove_ir::enum_layout`, so its `Some`/`None` tag
/// values and `Some`'s one-part payload offset are whatever that function
/// answered — not `0`/`1` assumed. `Vector.set` is `std.vector.set` since ADR
/// 0058, and the layout stays so that every id after it stays where it is.
pub const OPTION_INT: LayoutId = LayoutId(15);
/// `Option<Pair>`, the stride-two case of [`OPTION_INT`].
pub const OPTION_PAIR: LayoutId = LayoutId(16);

/// One [`Repr::Addr`] word, which is the whole of what a place is.
///
/// `Inst::AddrOfSlot`'s destination and `Inst::Load`'s address: ADR 0034's "There
/// is no place object, no place stack and no table of places", so a `var`
/// parameter is an ordinary slot holding a linear word index.
pub const ADDR: LayoutId = LayoutId(11);

/// `Any`: one reference to a [`Shape::Boxed`](cove_ir::Shape::Boxed) object,
/// whose payload width is `1 + len` — a run-time fact of the object's own
/// header, so `Layout::fixed_payload_words` answers `None` for it and
/// [`NativeCtx::fixed_payload_words`]'s table holds `0` at this index. It is
/// the one variable-payload shape a field-access case needs, because it is the
/// one the corpus this slice was widened by actually reaches.
pub const BOXED: LayoutId = LayoutId(17);

/// `Array<Int>`: a [`Shape::Elements`](cove_ir::Shape::Elements) that is not
/// growable — what `Vector<Int>.freeze()` answers, and what
/// `subset::method_of`'s `("Vector", "freeze")` arm has to find in the
/// program's own layout table to admit the call at all.
pub const ARRAY_INT: LayoutId = LayoutId(18);
/// `Array<Pair>`, the stride-two case of [`ARRAY_INT`].
pub const ARRAY_PAIR: LayoutId = LayoutId(19);
/// `Set<Int>`: what a keyed finish of `Int` elements relabels its store to
/// (#378, P4-5).
pub const SET_INT: LayoutId = LayoutId(20);
/// `MapEntry<Int, Int>`: two `Int` fields, `key` at word 0 and `value` at
/// word 1 — word for word one entry of [`MAP_INT`].
pub const ENTRY_INT: LayoutId = LayoutId(21);
/// `Map<Int, Int>`, whose entry [`ENTRY_INT`] is.
pub const MAP_INT: LayoutId = LayoutId(22);
/// `Vector<MapEntry<Int, Int>>`, the owner a map's next run is built in.
pub const ENTRY_VECTOR: LayoutId = LayoutId(23);
/// A `ByteBuffer`: ADR 0052's owner of a packed byte run, and the program's
/// `buffer_layout` — which is what a byte push's fast path compares the owner's
/// header with.
pub const BUFFER: LayoutId = LayoutId(24);
/// The packed store a [`BUFFER`] owns, whose header length is its capacity in
/// bytes. The program's `bytes_layout`.
pub const BYTES: LayoutId = LayoutId(25);

pub fn span() -> Span {
    Span::new(FileId(0), 0, 0)
}

pub fn function(reprs: Vec<Repr>, returns: LayoutId, code: Vec<Inst>) -> Function {
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

pub fn program(function: Function) -> Program {
    let mut layouts = vec![
        Layout::free(),
        Layout::word("Int", Repr::Int),
        Layout::word("Bool", Repr::Bool),
        Layout::word("Unit", Repr::Unit),
        Layout::inline(
            "Pair",
            cove_ir::Shape::Struct {
                fields: Vec::new(),
                opaque: false,
            },
            vec![Repr::Int, Repr::Int],
        ),
        Layout::word("Duration", Repr::Duration),
        Layout::inline(
            "RefPair",
            cove_ir::Shape::Struct {
                fields: Vec::new(),
                opaque: false,
            },
            vec![Repr::Int, Repr::Ref],
        ),
        Layout::word("Kind", Repr::Tag),
        Layout::word("Ref", Repr::Ref),
        Layout::inline(
            "HostPair",
            cove_ir::Shape::Struct {
                fields: Vec::new(),
                opaque: false,
            },
            vec![Repr::Int, Repr::Host],
        ),
        Layout::inline(
            "Empty",
            cove_ir::Shape::Struct {
                fields: Vec::new(),
                opaque: false,
            },
            Vec::new(),
        ),
        Layout::word("Addr", Repr::Addr),
        Layout::object("Vector", cove_ir::Shape::Vector { elem: INT }),
        Layout::object("Vector", cove_ir::Shape::Vector { elem: PAIR }),
        Layout::object(
            "Vector",
            cove_ir::Shape::Elements {
                elem: INT,
                growable: true,
            },
        ),
    ];
    // `Option<Int>` and `Option<Pair>`, at `OPTION_INT` and `OPTION_PAIR`
    // below. Built with `cove_ir::enum_layout`, the same function the real
    // lowering calls, rather than written out by hand, so that a case reading
    // a tag value or a payload offset reads the lowering's own.
    for elem in [INT, PAIR] {
        let (cases, payload) = cove_ir::enum_layout(
            &[(Arc::from("Some"), vec![elem]), (Arc::from("None"), vec![])],
            &layouts,
        );
        let mut words = vec![Repr::Tag];
        words.extend_from_slice(&payload);
        layouts.push(Layout::inline(
            "Option",
            cove_ir::Shape::Enum { cases, payload },
            words,
        ));
    }
    layouts.push(Layout::object("Any", cove_ir::Shape::Boxed));
    layouts.push(Layout::object(
        "Array",
        cove_ir::Shape::Elements {
            elem: INT,
            growable: false,
        },
    ));
    layouts.push(Layout::object(
        "Array",
        cove_ir::Shape::Elements {
            elem: PAIR,
            growable: false,
        },
    ));
    layouts.push(Layout::object("Set", cove_ir::Shape::Members { elem: INT }));
    let (fields, words) = cove_ir::struct_layout(
        &[(Arc::from("key"), INT), (Arc::from("value"), INT)],
        &layouts,
    );
    layouts.push(Layout::inline(
        "MapEntry",
        cove_ir::Shape::Struct {
            fields,
            opaque: false,
        },
        words,
    ));
    layouts.push(Layout::object(
        "Map",
        cove_ir::Shape::Entries {
            key: INT,
            value: INT,
        },
    ));
    layouts.push(Layout::object(
        "Vector",
        cove_ir::Shape::Vector { elem: ENTRY_INT },
    ));
    layouts.push(Layout::object("ByteBuffer", cove_ir::Shape::ByteBuffer));
    layouts.push(Layout::object("Bytes", cove_ir::Shape::Bytes));
    Program {
        functions: vec![function],
        layouts,
        buffer_layout: BUFFER,
        bytes_layout: BYTES,
        // `ArgsId(0)` is the empty argument list, which is what a call in these
        // tests hands over: the double records the hand-over and does not read
        // the list, and a table with nothing in it would panic the subset
        // predicate that bounds every argument's slot.
        args: vec![Vec::new()],
        ..Program::default()
    }
}

/// The same program, with a table of string literals.
///
/// `Inst::Str` names a `StrId`, and `supported` bounds that id against this very
/// table — so a case that emits one has to declare the strings, exactly as a case
/// that calls a builtin has to declare the builtin. The *text* is never read by
/// either arm: what a literal lowers to is a load of
/// `NativeCtx::literals[text]`, and what the table holds is the address the
/// runtime placed. `run_with_literals` is where those addresses come from.
pub fn program_with_strings(function: Function, strings: &[&str]) -> Program {
    Program {
        strings: strings.iter().map(|text| Arc::from(*text)).collect(),
        ..program(function)
    }
}

/// The same program, with switch tables.
pub fn program_with_tables(function: Function, tables: Vec<Table>) -> Program {
    Program {
        tables,
        ..program(function)
    }
}

/// The same program, with one non-empty argument list at `ArgsId(1)`.
pub fn program_with_args(function: Function, args: Vec<Arg>) -> Program {
    let mut held = program(function);
    held.args.push(args);
    held
}

/// The same program, with one builtin at `SiteId(0)` and its operands at
/// `ArgsId(1)`.
///
/// `receiver` and `operation` are resolved to the [`cove_ir::Intrinsic`]
/// they name — see [`cove_ir::IntrinsicSite`] — so it is the *variant* that
/// decides whether the tier lowers this call at all, and a case that passes
/// one no native arm handles should be refused rather than compiled. That is
/// what `an_intrinsic_call_is_handed_over_by_its_effects` checks with them.
pub fn program_with_intrinsic(
    function: Function,
    receiver: &str,
    operation: &str,
    result: LayoutId,
    args: Vec<Arg>,
) -> Program {
    let mut held = program_with_args(function, args);
    let intrinsic = cove_ir::Intrinsic::from_names(receiver, operation)
        .unwrap_or_else(|| panic!("`{receiver}.{operation}` has no `Intrinsic`"));
    held.intrinsic_sites
        .push(cove_ir::IntrinsicSite { intrinsic, result });
    held
}

/// The linear address of word zero of the segment every case runs over.
///
/// **Not zero, and that is the whole point of it.** It is
/// [`NativeCtx::stack_origin`], and an arm that resolved a stack address as
/// though the segment began at address zero — which is what a lowering that
/// confused the linear address with the word index would do — would read a word
/// a million places away from the one the VM reads. Zero would have let that
/// through, exactly as a `base` of zero would have let an address formed as if
/// the frame began at word zero through, which is why no case here uses one.
///
/// `1 << 20` is a real segment origin: `cove_runtime::vm::mem`'s `SEGMENT_WORDS`
/// is that, so this is the second task's segment.
pub const SEGMENT_ORIGIN: u64 = 1 << 20;

/// How many words of destination an entry is given.
///
/// One more than the widest return in this file, so that every case can assert
/// the answer *and* that the word past it was left alone — which is what says a
/// return of `width` words wrote `width` words.
pub const DESTINATION_WORDS: usize = 4;

/// What an unwritten word of a destination holds.
///
/// A number no lowering in this crate produces and no fixture computes, so a
/// destination word that still holds it is a word nothing wrote. Zero would not
/// do: a `Bool` answer is zero and so is a frame nobody touched.
pub const UNWRITTEN: u64 = 0x5555_aaaa_5555_aaaa;

/// What one entry into compiled code answered.
pub struct Answer {
    pub outcome: Outcome,
    /// The destination, as the entry left it: [`DESTINATION_WORDS`] words of
    /// which the first `Function::returns`' width are the answer and the rest
    /// are [`UNWRITTEN`].
    ///
    /// ADR 0057 — the callee writes its answer into the run its caller named, so
    /// this is what a case reads instead of a reported slot. It is a stronger
    /// thing to read: a slot number says where the answer would have been copied
    /// from, and this is the copy.
    pub returned: Vec<u64>,
    pub raise: Option<Raise>,
    pub raise_detail: u32,
    /// Which instruction raised, and the two numbers an out-of-range message
    /// names. See [`NativeCtx::raise_pc`].
    pub raise_pc: u32,
    pub raise_a: i64,
    pub raise_b: i64,
    pub pending_work: u64,
}

/// Compiles the one function of `program` and enters it over `words`.
///
/// `base` is deliberately not zero in every caller: it is a *word index* into
/// the segment, so a frame that does not begin at word zero is the case that
/// catches an address formed as if it did.
pub fn run<A: Arm>(program: &Program, words: &mut [u64], base: u64) -> Answer {
    let mut jit = A::new(helpers());
    let compiled = jit
        .compile(program, FunctionId(0))
        .expect("the function is inside the slice");
    jit.finalize();
    enter(&jit, compiled, words, base)
}

/// A heap for the tests, in the shape compiled code addresses one.
///
/// One `Vec<u64>` per chunk and a table of their base pointers, which is what
/// [`NativeCtx::chunks`] is. The real heap's chunks are `AtomicU64` and are
/// committed by the allocator; these are plain words and all of them exist,
/// which is a difference no emitted instruction can see — a `Relaxed` load of an
/// `AtomicU64` is a plain load, and the table says nothing about how a chunk
/// came to be there.
///
/// It holds more than one chunk on purpose. The whole of the chunk arithmetic is
/// only exercised by an object whose words are *not* all in chunk zero, and an
/// object that straddles a boundary is the case a single-chunk heap would have
/// let through.
pub struct Heap {
    held: Vec<Vec<u64>>,
    table: Vec<*mut u64>,
}

impl Heap {
    pub fn new(chunks: usize) -> Heap {
        let mut held: Vec<Vec<u64>> = (0..chunks)
            .map(|_| vec![0u64; HEAP_CHUNK_WORDS as usize])
            .collect();
        let table = held.iter_mut().map(|chunk| chunk.as_mut_ptr()).collect();
        Heap { held, table }
    }

    /// The linear address of heap word `index`, which is what a `Repr::Ref` slot
    /// holds.
    pub fn addr(&self, index: u64) -> u64 {
        HEAP_ORIGIN_WORDS + index
    }

    pub fn set(&mut self, index: u64, word: u64) {
        let chunk = (index / HEAP_CHUNK_WORDS) as usize;
        let at = (index % HEAP_CHUNK_WORDS) as usize;
        self.held[chunk][at] = word;
    }

    pub fn get(&self, index: u64) -> u64 {
        let chunk = (index / HEAP_CHUNK_WORDS) as usize;
        let at = (index % HEAP_CHUNK_WORDS) as usize;
        self.held[chunk][at]
    }

    /// An object's header at `index`, and its payload from `index + 1`.
    ///
    /// `mem::header`: the layout in the high half and the length field in the
    /// low one. The length means whatever the layout says — a byte count for a
    /// string, an element count for an array.
    pub fn object(&mut self, index: u64, layout: LayoutId, len: u32) -> u64 {
        self.set(index, (u64::from(layout.0) << 32) | u64::from(len));
        self.addr(index)
    }

    pub fn table(&self) -> *const *mut u64 {
        self.table.as_ptr()
    }
}

pub fn run_over<A: Arm>(program: &Program, words: &mut [u64], base: u64, heap: &Heap) -> Answer {
    let mut jit = A::new(helpers());
    let compiled = jit
        .compile(program, FunctionId(0))
        .expect("the function is inside the slice");
    jit.finalize();
    enter_over(&jit, compiled, words, base, heap.table())
}

/// The same, over a heap **and** a table of literal addresses.
///
/// `literals` is `NativeCtx::literals`: one heap address per `StrId`, in order,
/// which is what `Machine::place_literals` builds before a run's first
/// instruction. A case gives the addresses of objects it put in `heap` itself, so
/// that what an `Inst::Str` stores is a reference a later `len` or `byte-at` can
/// actually follow.
pub fn run_with_literals<A: Arm>(
    program: &Program,
    words: &mut [u64],
    base: u64,
    heap: &Heap,
    literals: &[u64],
) -> Answer {
    let mut jit = A::new(helpers());
    let compiled = jit
        .compile(program, FunctionId(0))
        .expect("the function is inside the slice");
    jit.finalize();
    // The payload table too, so that a case with a field read — ADR 0062's push
    // window reads an owner's length and store — is comparable across the arms.
    // A program with no field access never reads it, so publishing it changes
    // nothing for the cases written before it was.
    let payload_words: Vec<u32> = program
        .layouts
        .iter()
        .map(|layout| layout.fixed_payload_words(&program.layouts).unwrap_or(0))
        .collect();
    enter_with_tables(
        &jit,
        compiled,
        words,
        base,
        heap.table(),
        literals,
        &payload_words,
    )
}

/// [`run_over`], with `NativeCtx::fixed_payload_words` published from
/// `program`'s own layouts — `Layout::fixed_payload_words`, one per layout,
/// computed the way `Machine::for_run` computes it, so a field-access case
/// does not write the table out by hand.
pub fn run_with_fields<A: Arm>(
    program: &Program,
    words: &mut [u64],
    base: u64,
    heap: &Heap,
) -> Answer {
    let mut jit = A::new(helpers());
    let compiled = jit
        .compile(program, FunctionId(0))
        .expect("the function is inside the slice");
    jit.finalize();
    let payload_words: Vec<u32> = program
        .layouts
        .iter()
        .map(|layout| layout.fixed_payload_words(&program.layouts).unwrap_or(0))
        .collect();
    enter_with_tables(
        &jit,
        compiled,
        words,
        base,
        heap.table(),
        &[],
        &payload_words,
    )
}

pub fn enter<A: Arm>(jit: &A, compiled: A::Handle, words: &mut [u64], base: u64) -> Answer {
    enter_over(jit, compiled, words, base, std::ptr::null())
}

/// Enters `compiled` over `words` and a destination of the suite's own.
///
/// The destination is [`DESTINATION_WORDS`] words *past* everything `words`
/// holds, reached as `return_base + return_slot` with a slot of one — so that the
/// two numbers are added rather than one of them being ignored, and so that the
/// word before the run is a guard this function checks itself. The segment the
/// entry is given is therefore a copy of `words` with that run appended, and the
/// prefix is copied back before this returns: a case reads its frame out of
/// `words` exactly as it did before the destination existed.
pub fn enter_over<A: Arm>(
    jit: &A,
    compiled: A::Handle,
    words: &mut [u64],
    base: u64,
    chunks: *const *mut u64,
) -> Answer {
    enter_with_literals(jit, compiled, words, base, chunks, &[])
}

/// [`enter_over`], with a literal-address table published beside the heap.
///
/// An empty `literals` publishes a null table, which is what a caller whose
/// compiled code holds no `Inst::Str` gets — and is loud rather than plausible if
/// an arm ever reads it anyway.
pub fn enter_with_literals<A: Arm>(
    jit: &A,
    compiled: A::Handle,
    words: &mut [u64],
    base: u64,
    chunks: *const *mut u64,
    literals: &[u64],
) -> Answer {
    enter_with_tables::<A>(jit, compiled, words, base, chunks, literals, &[])
}

/// [`enter_with_literals`], with `NativeCtx::fixed_payload_words` published too.
///
/// An empty `payload_words` publishes a null table — [`enter_with_literals`]'s
/// rule for `literals`, and safe for the same reason: a program with no
/// `Inst::LoadField`/`Inst::StoreField` never reads it.
pub fn enter_with_tables<A: Arm>(
    jit: &A,
    compiled: A::Handle,
    words: &mut [u64],
    base: u64,
    chunks: *const *mut u64,
    literals: &[u64],
    payload_words: &[u32],
) -> Answer {
    let mut held: Vec<u64> = words.to_vec();
    let guard = held.len() as u64;
    held.extend([UNWRITTEN; DESTINATION_WORDS + 1]);
    let mut ctx = NativeCtx::new(std::ptr::null_mut(), held.as_mut_ptr(), SEGMENT_ORIGIN)
        .over_heap(chunks)
        .over_literals(match literals.is_empty() {
            true => std::ptr::null(),
            false => literals.as_ptr(),
        })
        .over_payload_words(match payload_words.is_empty() {
            true => std::ptr::null(),
            false => payload_words.as_ptr(),
        })
        .polling_at(POLL_AT.with(Cell::get));
    let entry = jit.entry(compiled);
    // Safety: `ctx.words` is `held`, `base` indexes into it, the functions below
    // are all built with frames that fit inside the prefix, and the destination
    // is `DESTINATION_WORDS` words that no frame overlaps.
    let outcome = unsafe { entry(&mut ctx, base, guard, 1) };
    assert_eq!(
        held[guard as usize], UNWRITTEN,
        "the word before the destination is not the destination's, and a return \
         that wrote it formed the address without the slot"
    );
    let returned = held[guard as usize + 1..].to_vec();
    words.copy_from_slice(&held[..words.len()]);
    Answer {
        outcome,
        returned,
        raise: ctx.raise(),
        raise_detail: ctx.raise_detail,
        raise_pc: ctx.raise_pc,
        raise_a: ctx.raise_a,
        raise_b: ctx.raise_b,
        pending_work: ctx.pending_work,
    }
}

/// Whether the one function of `program` is inside the slice at all.
pub fn compiles<A: Arm>(program: &Program) -> bool {
    let mut jit = A::new(helpers());
    jit.compile(program, FunctionId(0)).is_some()
}

// --- a loop ------------------------------------------------------------------

/// `total = 0; i = 1; while i <= n { total += i; i += 1 }; return total`
///
/// Slots: `s0` the bound, `s1` the running total, `s2` the counter, `s3` the
/// condition.
///
/// ```text
/// 0: int    s1 = 0
/// 1: int    s2 = 1
/// 2: le.int s3 = s2, s0
/// 3: branch-false s3 -> 7
/// 4: add.int s1 = s1, s2
/// 5: add.int.imm s2 = s2, 1
/// 6: jump -> 2
/// 7: return s1
/// ```
pub fn summing_loop() -> Program {
    program(function(
        vec![Repr::Int, Repr::Int, Repr::Int, Repr::Bool],
        INT,
        vec![
            Inst::Int { dst: 1, value: 0 },
            Inst::Int { dst: 2, value: 1 },
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Le,
                dst: 3,
                a: 2,
                b: 0,
            },
            Inst::BranchFalse { cond: 3, to: 7 },
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: 1,
                a: 1,
                b: 2,
            },
            Inst::ArithImm {
                op: ArithOp::Add,
                dst: 2,
                a: 2,
                value: 1,
            },
            Inst::Jump { to: 2 },
            Inst::Return { src: 1 },
        ],
    ))
}

/// The loop runs, answers, and polls exactly once per backedge.
///
/// The answer mirrors `encoded.rs`'s `int_op!` (`ADD_INT`, line 1112) and
/// `arith_imm!` (`ADD_INT_IMM`, line 1155), its `cmp_int!` (`LE_INT`) and its
/// `BRANCH_FALSE` arm at line 1191 — which tests the *word* against zero,
/// which is why a `Bool` slot holding one is taken as true.
///
/// The poll count is the interesting half. There is one backedge, the `jump`
/// at pc 6, and the loop body executes ten times for `n = 10`, so the
/// safepoint helper is entered ten times and not eleven: the iteration that
/// fails the condition leaves through the forward branch, which is not a
/// backedge and is not a safepoint.
pub fn a_loop_answers_and_polls_once_per_backedge<A: Arm>() {
    forget_polls();
    let mut words = vec![0u64; 16];
    // A frame at word 4, not word 0, so a base the code ignored would read
    // zeroes and answer zero.
    words[4] = 10;
    let answer = run::<A>(&summing_loop(), &mut words, 4);

    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[4 + 1], 55);
    assert_eq!(
        answer.returned,
        [55, UNWRITTEN, UNWRITTEN, UNWRITTEN],
        "one word of answer into the destination, and nothing past it"
    );
    assert_eq!(words[4 + 2], 11, "the counter is left one past the bound");

    let polls = polls();
    assert_eq!(polls.len(), 10, "one safepoint per backedge, ten backedges");
    assert!(
        polls.iter().all(|(pc, _)| *pc == 2),
        "a backedge's safepoint reports the pc the frame is about to resume at, \
         which is the jump's target: {polls:?}"
    );
}

/// The work charge is the static instruction count of the blocks actually
/// entered, and it is reset at every safepoint.
///
/// Spelled out rather than merely "nonzero", because ADR 0055's block
/// accounting is the part with no other test. The blocks of
/// [`summing_loop`] are `[0,2)`, `[2,4)`, `[4,7)` and `[7,8)` — four leaders:
/// the entry, the `jump`'s target, the `branch-false`'s fall-through, and the
/// `branch-false`'s target — so they are 2, 2, 3 and 1 instructions long.
///
/// The first backedge is reached having entered blocks 0, 2 and 4: 2 + 2 + 3
/// = 7. Every later one is reached from the safepoint that reset the
/// accumulator, through blocks 2 and 4: 2 + 3 = 5. The return is reached
/// after the last reset through blocks 2 and 7: 2 + 1 = 3, and that is what
/// is left pending, which is ADR 0055's "pending work charged on every exit".
pub fn the_work_charge_is_the_static_block_count<A: Arm>() {
    forget_polls();
    let mut words = vec![0u64; 8];
    words[0] = 3;
    let answer = run::<A>(&summing_loop(), &mut words, 0);

    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(polls(), vec![(2, 7), (2, 5), (2, 5)]);
    assert_eq!(answer.pending_work, 3);
}

/// A backedge below the threshold falls through, and the work it did not pay
/// for is still there at the next one.
///
/// ADR 0060's whole claim, in the one fixture whose block lengths are already
/// written down. [`summing_loop`]'s blocks are 2, 2, 3 and 1, so a turn is
/// 5 and the first backedge is reached at 7. With a threshold of 16:
///
/// | turn | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 |
/// | ---- | - | - | - | - | - | - | - | - | - | -- |
/// | work at the backedge | 7 | 12 | **17** | 5 | 10 | 15 | **20** | 5 | 10 | 15 |
///
/// Two polls in ten turns rather than ten, and both of them at or past the
/// threshold. The second number of each is what makes this a bound and not a
/// count: **no interval exceeds the threshold plus one turn**, which is
/// ADR 0040's `S + T` with `S` the published stride and `T` a turn, and the
/// reason the interval cannot drift is that the accumulator is *not* cleared
/// by a fall-through.
///
/// The 15 left after the tenth turn is not lost either: the loop leaves
/// through blocks 2 and 7, so 15 + 2 + 1 = 18 is pending at the return, which
/// is ADR 0055's "pending work charged on every exit" doing the work a poll
/// that never came would have done.
pub fn a_backedge_polls_only_once_the_threshold_is_reached<A: Arm>() {
    forget_polls();
    poll_at(16);
    let mut words = vec![0u64; 8];
    words[0] = 10;
    let answer = run::<A>(&summing_loop(), &mut words, 0);

    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[1], 55, "the loop still answers what it answered");
    assert_eq!(
        polls(),
        vec![(2, 17), (2, 20)],
        "two polls in ten backedges, each carrying everything since the last"
    );
    for (_, work) in polls() {
        assert!(
            (16..=16 + 5).contains(&work),
            "no interval is shorter than the threshold or longer than it plus \
             one turn of the loop: {work}"
        );
    }
    assert_eq!(
        answer.pending_work, 18,
        "the work below the threshold is charged at the exit instead"
    );
}

/// A threshold of nought is a poll at every backedge, which is what a context
/// that was published no budget holds.
///
/// The default matters more than it looks: `NativeCtx::poll_at` is a field an
/// embedder's own harness could forget to publish, and forgetting it has to
/// cost a poll that was not needed rather than skip one that was. This is the
/// same assertion as [`a_loop_answers_and_polls_once_per_backedge`] made
/// against the threshold set explicitly, so that the default is pinned as a
/// *value* and not only as whatever `NativeCtx::new` happens to leave.
pub fn a_threshold_of_nothing_polls_at_every_backedge<A: Arm>() {
    forget_polls();
    poll_at(0);
    let mut words = vec![0u64; 8];
    words[0] = 3;
    let answer = run::<A>(&summing_loop(), &mut words, 0);

    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(polls(), vec![(2, 7), (2, 5), (2, 5)]);
    assert_eq!(answer.pending_work, 3);
}

/// A stop is answered at the poll that is due, not at the backedge that is not.
///
/// The threshold is in front of the helper, so a run that is asked to stop is
/// stopped by the *first poll that happens* — and that is the thing a reader
/// should be able to check rather than infer, because it is the half of
/// ADR 0060 that could have gone wrong silently. With a threshold of 16 the
/// third backedge is the first poll, so a helper allowed one safepoint stops
/// the run there: three turns of work, and nothing pending, because the charge
/// went over before the helper answered.
pub fn a_stop_is_taken_at_the_first_poll_past_the_threshold<A: Arm>() {
    forget_polls();
    poll_at(16);
    POLLS_ALLOWED.with(|allowed| allowed.set(1));
    let mut words = vec![0u64; 8];
    words[0] = 1_000_000;
    let answer = run::<A>(&summing_loop(), &mut words, 0);

    assert_eq!(answer.outcome, Outcome::Stopped);
    assert_eq!(
        polls(),
        vec![(2, 17)],
        "the first poll past the threshold is the one that stopped it"
    );
    assert_eq!(
        answer.pending_work, 0,
        "the charge went to the helper before it answered, so nothing is pending"
    );
    assert_eq!(
        words[2], 4,
        "three turns ran and the fourth did not: the counter is one past them"
    );
}

/// A safepoint that answers "stop" stops the run, and leaves nothing pending.
///
/// This is the only test in which the helper returns `false`, and it is also
/// the only one that proves the frame pointer is re-derived after a call:
/// compiled code that had cached a pointer across the helper would still
/// answer here, so what this really pins is the *outcome* of a stop and the
/// accounting at it. ADR 0040's bounded stop is the helper's, and this is
/// the compiled side of it: leave at once, and do not charge twice.
pub fn a_safepoint_can_stop_the_run<A: Arm>() {
    forget_polls();
    POLLS_ALLOWED.with(|allowed| allowed.set(3));
    let mut words = vec![0u64; 8];
    words[0] = 1_000_000;
    let answer = run::<A>(&summing_loop(), &mut words, 0);

    assert_eq!(answer.outcome, Outcome::Stopped);
    assert_eq!(polls().len(), 3);
    assert_eq!(
        answer.pending_work, 0,
        "the charge went to the helper before it answered, so nothing is pending"
    );
}

/// ADR 0057: a zero-width return writes nothing at all.
///
/// `struct Empty {}` is no words, and the lowering gives a width-0 destination
/// the next free slot number — `call s2:m.Empty` in a two-slot frame is what
/// `cove_ir` emits — so the destination may not be a word of anything. A return
/// that formed the address and stored a word would be writing above the caller's
/// frame, which is the callee's own frame while the callee is still running.
///
/// The `Return` here names `src: 1` for the same reason: a zero-width value's
/// slot is one past everything the frame holds, and `crate::subset`'s `run`
/// admits it because zero words at the end of a frame are inside it.
pub fn a_zero_width_return_writes_nothing<A: Arm>() {
    forget_polls();
    let held = program(function(
        vec![Repr::Int, Repr::Int],
        EMPTY,
        vec![Inst::Int { dst: 0, value: 7 }, Inst::Return { src: 2 }],
    ));
    assert!(compiles::<A>(&held));
    // A word at the slot the zero-width return names, which is one past the
    // frame. It is here so that a return which wrote one word anyway would copy
    // *this* into the destination and be caught: with nothing there, the word
    // such a return copies is whatever lies past the frame, and the suite's own
    // guard word is exactly `UNWRITTEN` — so the assertion below would hold for
    // the wrong reason. It was checked by making the arm write one word, which
    // is how the coincidence was found.
    let mut words = vec![0u64, 0, 0xdead_beef_dead_beef, 0];
    let answer = run::<A>(&held, &mut words, 0);

    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[0], 7, "the body ran");
    assert_eq!(
        answer.returned, [UNWRITTEN; DESTINATION_WORDS],
        "a zero-width return wrote no word of the destination"
    );
}

/// ADR 0057: a raise and a stop publish no result.
///
/// "Semantically" is the whole of it — a destination nothing may read is free to
/// hold anything — but neither arm writes it at all, and that is the easiest
/// version of the rule to keep true and the easiest to check. Both ways out are
/// checked in one function because they are one claim about the two.
pub fn leaving_publishes_no_destination<A: Arm>() {
    // A raise: the addition overflows before the return is reached.
    let (answer, _) = arith::<A>(ArithOp::Add, i64::MAX, 1);
    assert_eq!(answer.outcome, Outcome::Raised);
    assert_eq!(
        answer.returned, [UNWRITTEN; DESTINATION_WORDS],
        "a raise wrote no word of the destination"
    );

    // A stop: the safepoint helper answers `false` on the fourth poll.
    forget_polls();
    POLLS_ALLOWED.with(|allowed| allowed.set(3));
    let mut words = vec![0u64; 8];
    words[0] = 1_000_000;
    let answer = run::<A>(&summing_loop(), &mut words, 0);
    assert_eq!(answer.outcome, Outcome::Stopped);
    assert_eq!(
        answer.returned, [UNWRITTEN; DESTINATION_WORDS],
        "a stop wrote no word of the destination"
    );
}

// --- arithmetic --------------------------------------------------------------

/// `s2 = s0 op s1; return s2`, over `Int` slots.
pub fn binary(op: ArithOp) -> Program {
    program(function(
        vec![Repr::Int, Repr::Int, Repr::Int],
        INT,
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
    ))
}

pub fn arith<A: Arm>(op: ArithOp, a: i64, b: i64) -> (Answer, Vec<u64>) {
    forget_polls();
    let mut words = vec![a as u64, b as u64, 0];
    let answer = run::<A>(&binary(op), &mut words, 0);
    (answer, words)
}

/// The ordinary answers, including the two rounding questions `sdiv` and
/// `srem` could get wrong.
///
/// `encoded.rs`'s `int_op!` (line 877) calls `int_arith`
/// (`crates/cove-runtime/src/vm/exec.rs:3705`), whose `Div` and `Rem` arms are
/// `checked_div` and `checked_rem` — Rust's truncating division, so `-7 / 2`
/// is `-3` and `-7 % 2` is `-1`. Cranelift's `sdiv` and `srem` are the same
/// two, which is why this passes; it is here because "the same two" is a
/// claim and not an axiom.
pub fn integer_arithmetic_answers_what_the_vm_answers<A: Arm>() {
    for (op, a, b, expected) in [
        (ArithOp::Add, 2i64, 3i64, 5i64),
        (ArithOp::Add, i64::MAX, -1, i64::MAX - 1),
        (ArithOp::Sub, 2, 3, -1),
        (ArithOp::Mul, -6, 7, -42),
        (ArithOp::Div, 7, 2, 3),
        (ArithOp::Div, -7, 2, -3),
        (ArithOp::Rem, 7, 2, 1),
        (ArithOp::Rem, -7, 2, -1),
        (ArithOp::Rem, 7, -2, 1),
    ] {
        let (answer, words) = arith::<A>(op, a, b);
        assert_eq!(answer.outcome, Outcome::Returned, "{op:?} {a} {b}");
        assert_eq!(words[2] as i64, expected, "{op:?} {a} {b}");
        assert_eq!(
            answer.returned[0] as i64, expected,
            "the destination holds what the slot holds: {op:?} {a} {b}"
        );
    }
}

/// Every way `int_arith` can fail, and the error it names.
///
/// This is the test the whole slice is worth having. `int_arith`
/// (`crates/cove-runtime/src/vm/exec.rs:3713`) is `checked_add`,
/// `checked_sub`, `checked_mul`, and a zero test *before* `checked_div` and
/// `checked_rem`. A native `+` that wrapped where this raises would be a
/// silent wrong answer, and `Raise` is how the runtime is told which of
/// `overflowed`/`divided_by_zero` to build — so the mapping is asserted, not
/// assumed.
///
/// The two `i64::MIN` rows are the ones easiest to get wrong in either
/// direction: `checked_div(i64::MIN, -1)` is `None`, which `int_arith` reports
/// as an *overflow* of division rather than as a division by zero, and
/// Cranelift's `sdiv` would have trapped on it — a machine signal, not a Cove
/// error — if the lowering had not branched around it.
///
/// The zero-first order matters too: `int_arith`'s `Div` arm tests `b == 0`
/// before `checked_div`, so `i64::MIN / 0` is "by zero" and not "overflowed".
pub fn every_arithmetic_failure_is_the_vms<A: Arm>() {
    for (op, a, b, expected) in [
        (ArithOp::Add, i64::MAX, 1i64, Raise::AddOverflowed),
        (ArithOp::Add, i64::MIN, -1, Raise::AddOverflowed),
        (ArithOp::Sub, i64::MIN, 1, Raise::SubOverflowed),
        (ArithOp::Sub, i64::MAX, -1, Raise::SubOverflowed),
        (ArithOp::Mul, i64::MAX, 2, Raise::MulOverflowed),
        (ArithOp::Mul, i64::MIN, -1, Raise::MulOverflowed),
        (ArithOp::Div, 1, 0, Raise::DividedByZero),
        (ArithOp::Div, 0, 0, Raise::DividedByZero),
        (ArithOp::Div, i64::MIN, 0, Raise::DividedByZero),
        (ArithOp::Div, i64::MIN, -1, Raise::DivOverflowed),
        (ArithOp::Rem, 1, 0, Raise::RemainderByZero),
        (ArithOp::Rem, i64::MIN, 0, Raise::RemainderByZero),
        (ArithOp::Rem, i64::MIN, -1, Raise::RemOverflowed),
    ] {
        let (answer, _) = arith::<A>(op, a, b);
        assert_eq!(answer.outcome, Outcome::Raised, "{op:?} {a} {b}");
        assert_eq!(answer.raise, Some(expected), "{op:?} {a} {b}");
        assert_eq!(
            answer.pending_work, 2,
            "the whole block is charged at its entry — both the arithmetic and \
             the return it never reached — so a raise leaves that much pending"
        );
    }
}

/// `Inst::ArithImm` is the same arithmetic with the operand in the
/// instruction, and it fails the same way.
///
/// `encoded.rs`'s `arith_imm!` (line 902) is `int_op!` with
/// `held.payload()` where the second slot read was, calling the same
/// `int_arith`. The IR says the same thing — `Inst::ArithImm`'s own
/// documentation: "The same arithmetic `Inst::Arith` does — the same overflow,
/// the same division and remainder by zero, the same `Duration` naming".
pub fn an_immediate_operand_fails_the_same_way<A: Arm>() {
    for (op, a, value, expected) in [
        (ArithOp::Add, i64::MAX, 1i64, Some(Raise::AddOverflowed)),
        (ArithOp::Sub, i64::MIN, 1, Some(Raise::SubOverflowed)),
        (ArithOp::Mul, i64::MAX, 2, Some(Raise::MulOverflowed)),
        (ArithOp::Div, 1, 0, Some(Raise::DividedByZero)),
        (ArithOp::Rem, 1, 0, Some(Raise::RemainderByZero)),
        (ArithOp::Div, i64::MIN, -1, Some(Raise::DivOverflowed)),
        (ArithOp::Add, 40, 2, None),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Int],
            INT,
            vec![
                Inst::ArithImm {
                    op,
                    dst: 1,
                    a: 0,
                    value,
                },
                Inst::Return { src: 1 },
            ],
        ));
        let mut words = vec![a as u64, 0];
        let answer = run::<A>(&held, &mut words, 0);
        match expected {
            Some(raise) => {
                assert_eq!(answer.outcome, Outcome::Raised, "{op:?} {a} {value}");
                assert_eq!(answer.raise, Some(raise), "{op:?} {a} {value}");
            }
            None => {
                assert_eq!(answer.outcome, Outcome::Returned);
                assert_eq!(words[1], 42);
            }
        }
    }
}

/// `s1 = -s0; return s1`, over `Int` slots of the given `Repr`.
///
/// `dst` is the second slot so that the source is still readable afterwards,
/// which is what lets the negation case assert the operand was not clobbered.
pub fn negation(dst: Repr) -> Program {
    program(function(
        vec![Repr::Int, dst],
        match dst {
            Repr::Duration => DURATION,
            _ => INT,
        },
        vec![
            Inst::Neg {
                num: Num::Int,
                dst: 1,
                a: 0,
            },
            Inst::Return { src: 1 },
        ],
    ))
}

pub fn negate<A: Arm>(a: i64) -> (Answer, Vec<u64>) {
    forget_polls();
    let mut words = vec![a as u64, 0];
    let answer = run::<A>(&negation(Repr::Int), &mut words, 0);
    (answer, words)
}

/// `Inst::Neg` over `Num::Int` is `checked_neg`, and its answers are the VM's.
///
/// `encoded.rs`'s `NEG_INT` arm (line 1144) reads the word as `i64`, calls
/// `checked_neg`, and stores the answer — so zero negates to zero rather than to
/// a negative zero, and `i64::MAX` to `i64::MIN + 1`.
pub fn negation_answers_what_the_vm_answers<A: Arm>() {
    for (a, expected) in [
        (0i64, 0i64),
        (1, -1),
        (-1, 1),
        (42, -42),
        (i64::MAX, i64::MIN + 1),
        (i64::MIN + 1, i64::MAX),
    ] {
        let (answer, words) = negate::<A>(a);
        assert_eq!(answer.outcome, Outcome::Returned, "-({a})");
        assert_eq!(words[1] as i64, expected, "-({a})");
        assert_eq!(
            words[0] as i64, a,
            "the operand is not the destination and was not written: -({a})"
        );
        assert_eq!(
            answer.returned[0] as i64, expected,
            "the destination holds what the slot holds: -({a})"
        );
    }
}

/// Negating `i64::MIN` raises, and it raises the VM's error.
///
/// The one input `checked_neg` answers `None` for. `encoded.rs`'s `NEG_INT` arm
/// reports it as `overflowed("negation")` — **not** renamed by a `Duration`
/// destination, because that arm calls `overflowed` directly instead of going
/// through `int_arith`'s `named` closure, which is the asymmetry a
/// reimplementation tidies up by accident. Both destinations are asserted here for
/// that reason.
///
/// It is the case the whole lowering is worth having: a native `-` that wrapped
/// where this raises would be a silent wrong answer, and the template arm's
/// `jno` and the Cranelift arm's comparison against `i64::MIN` are two different
/// ways to get it wrong.
pub fn negating_the_least_int_raises<A: Arm>() {
    for dst in [Repr::Int, Repr::Duration] {
        forget_polls();
        let mut words = vec![i64::MIN as u64, 0];
        let answer = run::<A>(&negation(dst), &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Raised, "-(i64::MIN) into {dst:?}");
        assert_eq!(
            answer.raise,
            Some(Raise::NegOverflowed),
            "and `overflowed(\"negation\")` whatever the destination is: {dst:?}"
        );
        assert_eq!(
            answer.returned, [UNWRITTEN; DESTINATION_WORDS],
            "a raise wrote no word of the destination: {dst:?}"
        );
        assert_eq!(
            answer.pending_work, 2,
            "the whole block is charged at its entry — the negation and the return \
             it never reached"
        );
    }
}

/// A `Duration` destination renames the overflow, and only for the three
/// operations that consult the name.
///
/// `encoded.rs`'s `int_op!` asks `machine.repr(id, a!()) ==
/// Some(Repr::Duration)` and hands the answer to `int_arith`, whose `named`
/// closure is called by the `Add`, `Sub` and `Mul` arms and **not** by `Div`
/// or `Rem` (`crates/cove-runtime/src/vm/exec.rs:3713`). So a duration
/// division that overflows is still `overflowed("division")`, and that
/// asymmetry is the whole of what this test is for — it is the sort of thing a
/// reimplementation tidies up by accident.
pub fn a_duration_destination_renames_only_three_overflows<A: Arm>() {
    for (op, a, b, expected) in [
        (ArithOp::Add, i64::MAX, 1i64, Raise::DurationOverflowed),
        (ArithOp::Sub, i64::MIN, 1, Raise::DurationOverflowed),
        (ArithOp::Mul, i64::MAX, 2, Raise::DurationOverflowed),
        (ArithOp::Div, i64::MIN, -1, Raise::DivOverflowed),
        (ArithOp::Rem, i64::MIN, -1, Raise::RemOverflowed),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Int, Repr::Duration],
            DURATION,
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
        ));
        let mut words = vec![a as u64, b as u64, 0];
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Raised, "{op:?}");
        assert_eq!(answer.raise, Some(expected), "{op:?}");
    }
}

// --- fused comparisons, which are ADR 0054's -------------------------------

/// `if !(s0 op s1) goto 3; s3 = 10; return s3; s3 = 20; return s3`, with the
/// comparison fused into the branch.
pub fn fused(op: CmpOp) -> Program {
    program(function(
        vec![Repr::Int, Repr::Int, Repr::Bool, Repr::Int],
        INT,
        vec![
            Inst::CmpBranch {
                on: Compare::Int,
                op,
                dst: 2,
                a: 0,
                b: 1,
                target: 3,
            },
            Inst::Int { dst: 3, value: 10 },
            Inst::Return { src: 3 },
            Inst::Int { dst: 3, value: 20 },
            Inst::Return { src: 3 },
        ],
    ))
}

/// `Inst::CmpBranch` branches *and* writes the `Bool`, both ways.
///
/// [ADR 0054] is explicit that the fusion "is exactly the two instructions it
/// replaces, in that order, with no condition attached — the `Bool` is still
/// written to `dst`", and `encoded.rs`'s `cmp_int_branch!` macro (line 1197
/// onward, `LT_INT_BRANCH` and its siblings) does the store before the test.
/// So the write is asserted on both paths and not only on the one that falls
/// through: a lowering that skipped it on the taken path would pass a test
/// that only looked at the answer.
///
/// [ADR 0054]: ../../../../docs/adr/0054-a-comparison-that-only-feeds-a-branch-is-the-branch.md
pub fn a_fused_comparison_branches_and_writes_its_bool<A: Arm>() {
    for (op, a, b, answered, flag) in [
        (CmpOp::Lt, 1i64, 2i64, 10i64, 1u64),
        (CmpOp::Lt, 2, 1, 20, 0),
        (CmpOp::Lt, 2, 2, 20, 0),
        (CmpOp::Ge, 2, 2, 10, 1),
        (CmpOp::Eq, -3, -3, 10, 1),
        (CmpOp::Ne, -3, -3, 20, 0),
        (CmpOp::Gt, i64::MIN, i64::MAX, 20, 0),
        (CmpOp::Le, i64::MIN, i64::MAX, 10, 1),
    ] {
        forget_polls();
        let mut words = vec![a as u64, b as u64, 0xdead, 0];
        let answer = run::<A>(&fused(op), &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned, "{op:?} {a} {b}");
        assert_eq!(words[3] as i64, answered, "{op:?} {a} {b}");
        assert_eq!(
            words[2], flag,
            "the fused comparison still writes its `Bool`: {op:?} {a} {b}"
        );
    }
}

/// `Inst::CmpImmBranch` is the same, with the right operand in the
/// instruction.
///
/// `encoded.rs`'s `cmp_imm_branch!` (line 1040) is `cmp_int_branch!` reading
/// `held.lo() as i32` where the second slot was — which is why the IR's
/// immediate is an `i32` here and an `i64` on `Inst::CmpImm`. The negative
/// row is the one that matters: a lowering that zero-extended the immediate
/// instead of sign-extending it would answer the opposite.
pub fn a_fused_immediate_comparison_branches_and_writes_its_bool<A: Arm>() {
    for (op, a, value, answered, flag) in [
        (CmpOp::Lt, 1i64, 5i32, 10i64, 1u64),
        (CmpOp::Lt, 5, 5, 20, 0),
        (CmpOp::Ge, 5, 5, 10, 1),
        (CmpOp::Gt, 0, -1, 10, 1),
        (CmpOp::Lt, 0, -1, 20, 0),
        (CmpOp::Eq, -7, -7, 10, 1),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Int, Repr::Bool, Repr::Int],
            INT,
            vec![
                Inst::CmpImmBranch {
                    op,
                    dst: 2,
                    a: 0,
                    value,
                    target: 3,
                },
                Inst::Int { dst: 3, value: 10 },
                Inst::Return { src: 3 },
                Inst::Int { dst: 3, value: 20 },
                Inst::Return { src: 3 },
            ],
        ));
        let mut words = vec![a as u64, 0, 0xdead, 0];
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned, "{op:?} {a} {value}");
        assert_eq!(words[3] as i64, answered, "{op:?} {a} {value}");
        assert_eq!(words[2], flag, "{op:?} {a} {value}");
    }
}

/// An unfused `Inst::Cmp` and `Inst::CmpImm` write the same `Bool`.
///
/// `encoded.rs`'s `cmp_int!` (line 921) and `cmp_imm!` (line 913):
/// `compare(op, x.cmp(&y))` stored as `answer as u64`, so one or zero and
/// never a mask. A lowering that stored a comparison's raw result without
/// narrowing it would put something other than 0 or 1 in a `Bool` slot, and
/// then `branch-false` — which tests the whole word — would still work while
/// `Bool` equality would not.
pub fn a_comparison_writes_one_or_zero<A: Arm>() {
    for (op, a, b, flag) in [
        (CmpOp::Lt, 1i64, 2i64, 1u64),
        (CmpOp::Lt, 2, 1, 0),
        (CmpOp::Ge, -1, -1, 1),
        (CmpOp::Ne, -1, -1, 0),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Int, Repr::Bool],
            BOOL,
            vec![
                Inst::Cmp {
                    on: Compare::Int,
                    op,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        ));
        let mut words = vec![a as u64, b as u64, 0xdead];
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(words[2], flag, "{op:?} {a} {b}");

        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Bool],
            BOOL,
            vec![
                Inst::CmpImm {
                    op,
                    dst: 1,
                    a: 0,
                    value: b,
                },
                Inst::Return { src: 1 },
            ],
        ));
        let mut words = vec![a as u64, 0xdead];
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(words[1], flag, "imm {op:?} {a} {b}");
    }
}

// --- copies, traps, refusals -------------------------------------------------

/// `Inst::Copy` moves a layout's whole run of words, and moves rather than
/// smears.
///
/// `encoded.rs`'s `COPY` arm (line 1082) is `Memory::copy_slots`, which is a
/// `copy_within` — a `memmove` — and its documentation says why: "two slots
/// of one frame may overlap and a lowering is free to emit that rather than
/// having to prove it does not". The second row here is that overlap, and a
/// forward run of load-store pairs would answer `[7, 7, 7]` for it.
pub fn a_copy_moves_every_word_and_does_not_smear<A: Arm>() {
    for (dst, src, before, after) in [
        (2u32, 0u32, [7u64, 9, 0, 0], [7u64, 9, 7, 9]),
        (1, 0, [7, 9, 0, 0], [7, 7, 9, 0]),
        (0, 1, [7, 9, 11, 0], [9, 11, 11, 0]),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int; 4],
            PAIR,
            vec![
                Inst::Copy {
                    dst,
                    src,
                    layout: PAIR,
                },
                Inst::Return { src: dst },
            ],
        ));
        let mut words = before.to_vec();
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(words, after.to_vec(), "copy {src} -> {dst}");
        assert_eq!(
            answer.returned,
            [
                after[dst as usize],
                after[dst as usize + 1],
                UNWRITTEN,
                UNWRITTEN
            ],
            "two words of answer into the destination, and nothing past them"
        );
    }
}

/// `Inst::Trap` leaves with the message's `StrId` and nothing else.
///
/// `encoded.rs`'s `TRAP` arm (line 1870) is
/// `fail!(RuntimeError::new(program.string(StrId(held.lo())).to_string()))`.
/// The lookup is the runtime's, so what crosses the boundary is the id: this
/// crate has no strings table and must not grow one, which is the same rule
/// that keeps the overflow messages out of `Raise`.
pub fn a_trap_names_its_message_by_id<A: Arm>() {
    forget_polls();
    let held = program(function(
        vec![Repr::Int],
        UNIT,
        vec![
            Inst::Int { dst: 0, value: 1 },
            Inst::Trap { message: StrId(37) },
        ],
    ));
    let mut words = vec![0u64];
    let answer = run::<A>(&held, &mut words, 0);

    assert_eq!(answer.outcome, Outcome::Raised);
    assert_eq!(answer.raise, Some(Raise::Trapped));
    assert_eq!(answer.raise_detail, 37);
    assert_eq!(answer.pending_work, 2, "both instructions of the block");
}

/// A function holding anything outside the slice compiles to `None`, whole.
///
/// ADR 0055: "A function containing an operation the native lowering does not
/// yet support runs entirely on the encoded VM. The initial implementation
/// does not split one function into native and interpreted regions." So the
/// answer is `None` and not a partly compiled entry — including for
/// `Inst::Unit`, which is one store and is refused anyway, because the line
/// is drawn at the adoption gate's list and not at what happens to be easy.
pub fn anything_outside_the_slice_refuses_the_whole_function<A: Arm>() {
    let refused: Vec<(&str, Program)> = vec![
        (
            "float negation, which `Num::Int` negation being lowered does not admit",
            program(function(
                vec![Repr::Float],
                INT,
                vec![
                    Inst::Neg {
                        num: Num::Float,
                        dst: 0,
                        a: 0,
                    },
                    Inst::Return { src: 0 },
                ],
            )),
        ),
        (
            "float arithmetic",
            program(function(
                vec![Repr::Float, Repr::Float],
                INT,
                vec![
                    Inst::Arith {
                        num: Num::Float,
                        op: ArithOp::Add,
                        dst: 1,
                        a: 0,
                        b: 0,
                    },
                    Inst::Return { src: 1 },
                ],
            )),
        ),
        (
            "a float comparison",
            program(function(
                vec![Repr::Float, Repr::Bool],
                BOOL,
                vec![
                    Inst::Cmp {
                        on: Compare::Float,
                        op: CmpOp::Lt,
                        dst: 1,
                        a: 0,
                        b: 0,
                    },
                    Inst::Return { src: 1 },
                ],
            )),
        ),
        (
            "an ordered `Bool` comparison, which the VM answers with a runtime error",
            program(function(
                vec![Repr::Bool, Repr::Bool],
                BOOL,
                vec![
                    Inst::Cmp {
                        on: Compare::Bool,
                        op: CmpOp::Lt,
                        dst: 1,
                        a: 0,
                        b: 0,
                    },
                    Inst::Return { src: 1 },
                ],
            )),
        ),
        (
            "a frame holding a host handle, however scalar its instructions are",
            program(function(
                vec![Repr::Int, Repr::Host],
                INT,
                vec![Inst::Int { dst: 0, value: 1 }, Inst::Return { src: 0 }],
            )),
        ),
        // The two `Repr` checks overlap in any *real* program — a function that
        // copies a layout holding a host handle has a `Repr::Host` slot to copy
        // it into, so the frame check above would already have refused it. This
        // row exercises the layout check on its own, which is why its frame is
        // scalar and the program is one no lowering would emit: a check that is
        // only ever reached behind another check is a check nothing tests.
        (
            "a copy of a layout holding a host handle",
            program(function(
                vec![Repr::Int, Repr::Int, Repr::Int, Repr::Int],
                INT,
                vec![
                    Inst::Copy {
                        dst: 2,
                        src: 0,
                        layout: HOST_PAIR,
                    },
                    Inst::Return { src: 2 },
                ],
            )),
        ),
        ("a stub, which stands in for a body no lowering lowered", {
            let mut held = program(function(
                vec![Repr::Int],
                INT,
                vec![Inst::Int { dst: 0, value: 1 }, Inst::Return { src: 0 }],
            ));
            held.functions[0].stub = true;
            held
        }),
        (
            "a jump past the end, which the IR verifier refuses too",
            program(function(
                vec![Repr::Int],
                INT,
                vec![Inst::Int { dst: 0, value: 1 }, Inst::Jump { to: 9 }],
            )),
        ),
        (
            "a body whose last instruction is not a terminator",
            program(function(
                vec![Repr::Int],
                INT,
                vec![Inst::Int { dst: 0, value: 1 }],
            )),
        ),
    ];
    for (what, held) in refused {
        assert!(!compiles::<A>(&held), "should have refused: {what}");
    }
}

/// A `Unit` constant is one word of nought, and a slot past the frame refuses.
///
/// `encoded.rs`'s `CONST_UNIT` arm is `set_word_at(base + dst, 0)`, so this is
/// `Inst::Bool`'s emitter with the constant already chosen. It is asserted rather
/// than assumed because a `Unit` word is *zero* and so is a fresh frame: a
/// fixture that read an untouched slot would agree with an arm that emitted
/// nothing at all. So the slot is written first and the instruction has to put it
/// back.
pub fn a_unit_constant_is_a_zero_word<A: Arm>() {
    let held = program(function(
        vec![Repr::Int, Repr::Unit],
        UNIT,
        vec![
            // A number no `Unit` is, so a `const-unit` that stored nothing leaves
            // it behind and the assertion below reads it.
            Inst::Int {
                dst: 1,
                value: 0x5555_aaaa,
            },
            Inst::Unit { dst: 1 },
            Inst::Return { src: 1 },
        ],
    ));
    let mut words = vec![0u64, 0];
    let answer = run::<A>(&held, &mut words, 0);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[1], 0, "the slot the constant was stored over");
    assert_eq!(answer.returned[0], 0, "and the answer at the boundary");

    // A frame that does not begin at word zero, and then a slot the frame does
    // not have — which is `Reason::Operands` and a refusal.
    let mut words = vec![9, 9, 0, 0];
    let answer = run::<A>(&held, &mut words, 2);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[3], 0);
    assert_eq!(&words[..2], &[9, 9]);

    let past = program(function(
        vec![Repr::Int, Repr::Unit],
        UNIT,
        vec![Inst::Unit { dst: 9 }, Inst::Return { src: 1 }],
    ));
    assert!(!compiles::<A>(&past), "a slot past the end of the frame");
}

/// ADR 0059's three-way order writes the `Int` `-1`, `0` or `1`, over an
/// `Int`, a `Bool` and a case index alike.
///
/// `encoded.rs`'s `ORDER_INT | ORDER_BOOL | ORDER_TAG` arm is one signed
/// comparison of the two words and a subtraction of its two flags, stored as
/// a whole word — so `-1` is all ones, and a lowering that widened a flag
/// byte with a sign, or subtracted in eight bits, would write `255` or a mask.
/// The `String` order goes through a helper and has cases of its own below.
pub fn a_three_way_order_writes_minus_one_zero_or_one<A: Arm>() {
    for (on, repr, a, b, answer) in [
        (Compare::Int, Repr::Int, 1i64, 2i64, -1i64),
        (Compare::Int, Repr::Int, 2, 2, 0),
        (Compare::Int, Repr::Int, 3, 2, 1),
        (Compare::Int, Repr::Int, i64::MIN, i64::MAX, -1),
        (Compare::Int, Repr::Int, i64::MAX, i64::MIN, 1),
        (Compare::Int, Repr::Int, -1, 1, -1),
        (Compare::Bool, Repr::Bool, 0, 1, -1),
        (Compare::Bool, Repr::Bool, 1, 1, 0),
        (Compare::Tag, Repr::Tag, 2, 0, 1),
    ] {
        forget_polls();
        let held = program(function(
            vec![repr, repr, Repr::Int],
            INT,
            vec![
                Inst::Cmp {
                    on,
                    op: CmpOp::Order,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        ));
        let mut words = vec![a as u64, b as u64, 0xdead];
        let outcome = run::<A>(&held, &mut words, 0);
        assert_eq!(outcome.outcome, Outcome::Returned, "{on:?} {a} {b}");
        assert_eq!(words[2] as i64, answer, "{on:?} {a} {b}");
    }
}

/// A string object of `text`'s bytes at heap word `index`, as `Machine::new_string`
/// lays one out: the byte count in the header, eight bytes a payload word, least
/// significant first, and a zero tail.
pub fn a_string(heap: &mut Heap, index: u64, text: &[u8]) -> u64 {
    let addr = heap.object(index, LayoutId(0), text.len() as u32);
    for (word, chunk) in text.chunks(8).enumerate() {
        let mut packed = 0u64;
        for (at, byte) in chunk.iter().enumerate() {
            packed |= u64::from(*byte) << (at * 8);
        }
        heap.set(index + 1 + word as u64, packed);
    }
    addr
}

/// Two string orders — `a` against `b` into `s2`, then `b` against `a` into `s3`
/// — and their sum into `s4`, answering `s2`. Slots `s0` and `s1` are the two
/// strings.
///
/// The second order and the add read and write the frame **after** a call of the
/// leaf helper with nothing re-derived in between, which is the part of the leaf
/// contract an arm could get wrong without any answer changing on the first
/// instruction alone.
pub fn ordering_strings() -> Program {
    let order = |dst, a, b| Inst::Cmp {
        on: Compare::Str,
        op: CmpOp::Order,
        dst,
        a,
        b,
    };
    program(function(
        vec![Repr::Ref, Repr::Ref, Repr::Int, Repr::Int, Repr::Int],
        INT,
        vec![
            order(2, 0, 1),
            order(3, 1, 0),
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: 4,
                a: 2,
                b: 3,
            },
            Inst::Return { src: 2 },
        ],
    ))
}

/// The strings [`a_string_order_is_the_runtimes_leaf`] orders, placed in two
/// chunks so a string straddling the boundary is among them, and the address of
/// each — nought for the null reference.
pub fn ordered_strings(heap: &mut Heap) -> Vec<(&'static str, u64)> {
    let straddle = HEAP_CHUNK_WORDS - 1;
    vec![
        ("null", 0),
        ("empty", a_string(heap, 40, b"")),
        ("a", a_string(heap, 42, b"a")),
        ("ab", a_string(heap, 44, b"ab")),
        ("abc", a_string(heap, 46, b"abc")),
        ("abcdefghi", a_string(heap, 48, b"abcdefghi")),
        ("abcdefghj", a_string(heap, 52, b"abcdefghj")),
        ("abcdefgh", a_string(heap, 56, b"abcdefgh")),
        ("h\u{e9}llo", a_string(heap, 60, "h\u{e9}llo".as_bytes())),
        ("hello", a_string(heap, 64, b"hello")),
        ("Z", a_string(heap, 68, b"Z")),
        ("straddling", a_string(heap, straddle, b"abcdefghijklmnop")),
    ]
}

/// **A `String`'s three-way order is the runtime's leaf helper, called once per
/// instruction with the two words, its answer stored whole — and no safepoint
/// or re-derived frame around it.**
///
/// Every pair of an equal string, a prefix, a difference in the ninth byte, a
/// two-byte character against its ASCII neighbour, an upper-case letter, the empty
/// string, a string straddling a chunk boundary and the null reference, which
/// `encoded.rs`'s `ORDER_STR` reads as the empty string rather than refusing — so
/// a null against the empty string is `0` and not a raise. `-1` is all ones, so an
/// arm that stored the answer's low byte or widened it without a sign would write
/// `255` or `0xffff_ffff` into `s2`, and the sum in `s4` is `0` only if the second
/// call and the add used a frame the first call did not stale.
pub fn a_string_order_is_the_runtimes_leaf<A: Arm>() {
    let mut heap = Heap::new(2);
    let strings = ordered_strings(&mut heap);
    let order = |x: &str, y: &str| -> i64 {
        let text = |name: &str| -> Vec<u8> {
            match name {
                "null" | "empty" => Vec::new(),
                "straddling" => b"abcdefghijklmnop".to_vec(),
                other => other.as_bytes().to_vec(),
            }
        };
        match text(x).cmp(&text(y)) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }
    };
    let held = ordering_strings();
    for (x, a) in &strings {
        for (y, b) in &strings {
            forget_polls();
            forget_ordered();
            let mut words = vec![*a, *b, 0xdead, 0xdead, 0xdead];
            let answer = run_over::<A>(&held, &mut words, 0, &heap);
            assert_eq!(answer.outcome, Outcome::Returned, "{x} against {y}");
            assert_eq!(words[2] as i64, order(x, y), "{x} against {y}");
            assert_eq!(words[3] as i64, order(y, x), "{y} against {x}");
            assert_eq!(words[4], 0, "{x} and {y}: the frame after the leaf call");
            assert_eq!(
                ordered(),
                vec![(*a, *b), (*b, *a)],
                "{x} against {y}: one hand-over per instruction, with the two words"
            );
            assert!(
                polls().is_empty(),
                "{x} against {y}: a leaf is no safepoint"
            );
        }
    }
    forget_ordered();
}

/// A `String`'s order is the only `String` comparison in the slice: equality and
/// the ordered forms `cmp_str!` answers copy both strings out, and still refuse
/// the function.
pub fn only_a_strings_order_is_in_the_slice<A: Arm>() {
    assert!(compiles::<A>(&ordering_strings()));
    for op in [
        CmpOp::Eq,
        CmpOp::Ne,
        CmpOp::Lt,
        CmpOp::Le,
        CmpOp::Gt,
        CmpOp::Ge,
    ] {
        let held = program(function(
            vec![Repr::Ref, Repr::Ref, Repr::Bool],
            BOOL,
            vec![
                Inst::Cmp {
                    on: Compare::Str,
                    op,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        ));
        assert!(!compiles::<A>(&held), "`Str` {op:?} is outside the slice");
    }
}

/// A `Bool` equality *is* inside the slice, which is the other side of the
/// refusal above.
///
/// `encoded.rs` groups `EQ_BOOL | EQ_REF | EQ_TAG => cmp_word!(true)` and
/// refuses the ordered forms beside them at `not_ordered!()` (line 1213). The
/// admitted half is a word comparison; only the refused half is a runtime
/// error.
pub fn a_bool_equality_is_inside_the_slice<A: Arm>() {
    forget_polls();
    let held = program(function(
        vec![Repr::Bool, Repr::Bool, Repr::Bool],
        BOOL,
        vec![
            Inst::Cmp {
                on: Compare::Bool,
                op: CmpOp::Eq,
                dst: 2,
                a: 0,
                b: 1,
            },
            Inst::Return { src: 2 },
        ],
    ));
    let mut words = vec![1, 1, 0xdead];
    let answer = run::<A>(&held, &mut words, 0);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[2], 1);
}

/// `Inst::Bool` writes a word, which is `encoded.rs`'s
/// `CONST_BOOL | CONST_INT | CONST_FLOAT` arm (line 1057) storing
/// `held.payload()`.
pub fn a_boolean_constant_is_a_word<A: Arm>() {
    for (value, word) in [(true, 1u64), (false, 0)] {
        forget_polls();
        let held = program(function(
            vec![Repr::Bool],
            BOOL,
            vec![Inst::Bool { dst: 0, value }, Inst::Return { src: 0 }],
        ));
        let mut words = vec![0xdead];
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(words[0], word);
    }
}

/// Two functions in one code generator, both entered, and the second one
/// compiled after the first was defined.
///
/// Not a correctness question so much as a shape one: ADR 0055 makes "a
/// function the unit of compilation, caching and selection", so a code
/// generator that could only hold one would be the wrong shape however well
/// it worked.
pub fn one_code_generator_holds_many_functions<A: Arm>() {
    forget_polls();
    let mut jit = A::new(helpers());
    let doubling = program(function(
        vec![Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: 1,
                a: 0,
                b: 0,
            },
            Inst::Return { src: 1 },
        ],
    ));
    let first = jit.compile(&doubling, FunctionId(0)).expect("inside");
    let second = jit.compile(&summing_loop(), FunctionId(0)).expect("inside");
    jit.finalize();

    let mut words = vec![21u64, 0];
    assert_eq!(enter(&jit, first, &mut words, 0).outcome, Outcome::Returned);
    assert_eq!(words[1], 42);

    let mut words = vec![0u64; 8];
    words[0] = 4;
    assert_eq!(
        enter(&jit, second, &mut words, 0).outcome,
        Outcome::Returned
    );
    assert_eq!(words[1], 10);
}

// --- the covefmt slice -------------------------------------------------------
//
// Everything below was added for the second raced slice: `covefmt.byteOfPunct`
// and `covefmt.wantsASpaceBetween`, which between them need an `Array` element
// read, an enum's tag and switch, a tag comparison, `String.byteAt`, a length,
// and calls. Every expectation cites the arm of
// `crates/cove-runtime/src/vm/exec/encoded.rs` it mirrors, as the ones above do.

/// A reference slot is inside the slice, which is the other half of the
/// `Repr::Host` refusal above.
///
/// It is worth asserting on its own because it was a refusal one slice ago, and
/// what changed is not a code generator but the collector argument: the frame is
/// the canonical home of every value at every instruction boundary, so a
/// `Repr::Ref` slot is a root the existing walk already finds. See
/// `cove_native::abi`'s "References are live here".
pub fn a_reference_slot_is_inside_the_slice<A: Arm>() {
    let held = program(function(
        vec![Repr::Ref, Repr::Ref],
        REF,
        vec![
            Inst::Copy {
                dst: 1,
                src: 0,
                layout: REF,
            },
            Inst::Return { src: 1 },
        ],
    ));
    assert!(compiles::<A>(&held));
    let mut words = vec![0u64, 0, 0xfeed_beef, 0];
    let answer = run::<A>(&held, &mut words, 2);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[3], 0xfeed_beef, "the reference was copied as a word");
    assert_eq!(
        answer.returned,
        [0xfeed_beef, UNWRITTEN, UNWRITTEN, UNWRITTEN],
        "a reference return is one word — the address, not the object"
    );
}

/// `encoded.rs`'s `FUNC_REF | CONST_TAG` arm (line 1071): a case index is
/// written by the same store a constant is.
pub fn a_tag_is_the_case_index_as_a_word<A: Arm>() {
    let held = program(function(
        vec![Repr::Tag],
        TAG,
        vec![
            Inst::Tag {
                dst: 0,
                layout: TAG,
                case: CaseId(3),
            },
            Inst::Return { src: 0 },
        ],
    ));
    let mut words = vec![0u64; 4];
    let answer = run::<A>(&held, &mut words, 3);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[3], 3);
}

/// `encoded.rs`'s `EQ_TAG`/`NE_TAG` arms (line 1143), which are `cmp_word!`:
/// two case indices compare as the words they are.
pub fn a_tag_comparison_is_a_word_comparison<A: Arm>() {
    for (a, b, same) in [(1u64, 1u64, 1u64), (1, 2, 0), (0, 0, 1)] {
        let held = program(function(
            vec![Repr::Tag, Repr::Tag, Repr::Bool],
            BOOL,
            vec![
                Inst::Cmp {
                    on: Compare::Tag,
                    op: CmpOp::Eq,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        ));
        let mut words = vec![a, b, 0xdead];
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(words[2], same, "{a} == {b}");
    }
}

/// `encoded.rs`'s `NOT` arm (line 1170), which tests the whole *word* against
/// zero rather than its low byte.
pub fn not_tests_the_whole_word<A: Arm>() {
    for (given, answer) in [(0u64, 1u64), (1, 0), (0x100, 0), (u64::MAX, 0)] {
        let held = program(function(
            vec![Repr::Bool, Repr::Bool],
            BOOL,
            vec![Inst::Not { dst: 1, a: 0 }, Inst::Return { src: 1 }],
        ));
        let mut words = vec![given, 0xdead];
        assert_eq!(run::<A>(&held, &mut words, 0).outcome, Outcome::Returned);
        assert_eq!(words[1], answer, "!{given:#x}");
    }
}

/// `encoded.rs`'s `LEN` arm (line 1620): the header's length field, and a null
/// reference refused first.
pub fn a_len_reads_the_header_and_refuses_null<A: Arm>() {
    let held = program(function(
        vec![Repr::Ref, Repr::Int],
        INT,
        vec![Inst::Len { dst: 1, obj: 0 }, Inst::Return { src: 1 }],
    ));

    // An object in the *second* chunk, so the chunk arithmetic is exercised
    // rather than a heap whose every word is in chunk zero.
    let at = HEAP_CHUNK_WORDS + 17;
    let mut heap = Heap::new(2);
    let addr = heap.object(at, INT, 4242);
    let mut words = vec![addr, 0];
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[1], 4242);

    // The *high* half of the header is the layout and is not read into the
    // answer. A `String`'s byte length is this same read since ADR 0058 moved
    // `String.byteLength` onto `core.byteLength`, so a count that happened to
    // share its word with a wide layout number is what this pins.
    let addr = heap.object(at, PAIR, 0x1234_5678);
    let mut words = vec![addr, 0];
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[1], 0x1234_5678);

    let mut words = vec![0u64, 0];
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Raised);
    assert_eq!(answer.raise, Some(Raise::NullObject));
    assert_eq!(answer.raise_pc, 0);
}

/// `encoded.rs`'s `STR` arm: the address of a placed literal, into a slot.
///
/// The whole of what is emitted is `literals[text]`, so what this has to say is
/// that the *right* entry is read — which is why the table holds three distinct
/// addresses and the fixture asks for the middle one. A lowering that dropped the
/// displacement, or scaled it by one instead of eight, answers a neighbour's
/// address and passes every test whose table has one row.
///
/// **The answer is returned across the boundary and not only left in a slot.**
/// `Answer::returned` is the destination the caller named, which is what a real
/// VM-to-native crossing reads; a reference that was correct in the frame and
/// wrong in the destination would be a string that vanished at a tier boundary.
///
/// The address is then *followed*, by a `len` of the object it names, because an
/// `Inst::Str` that answered a plausible number rather than a reference would
/// look right in a word comparison and wrong the moment anything read through it.
pub fn a_literal_is_the_address_the_run_placed<A: Arm>() {
    // The address, answered as a `Repr::Ref` and nothing more. It is a separate
    // program from the one below on purpose: an arm that read the *wrong* entry
    // answers a word that is not an address at all, and a fixture that followed it
    // in the same breath would crash before it could say which word it got.
    let address = program_with_strings(
        function(
            vec![Repr::Ref],
            REF,
            vec![
                Inst::Str {
                    dst: 0,
                    text: StrId(1),
                },
                Inst::Return { src: 0 },
            ],
        ),
        &["a", "bc", "def"],
    );
    // And the address *followed*: an `Inst::Str` that answered a plausible number
    // rather than a reference would look right in a word comparison and wrong the
    // moment anything read through it.
    let followed = program_with_strings(
        function(
            vec![Repr::Ref, Repr::Int],
            INT,
            vec![
                Inst::Str {
                    dst: 0,
                    text: StrId(1),
                },
                Inst::Len { dst: 1, obj: 0 },
                Inst::Return { src: 1 },
            ],
        ),
        &["a", "bc", "def"],
    );

    // Three objects, one per literal, in the *second* chunk so that the address
    // is one the chunk arithmetic has to resolve rather than a small number. The
    // table holds three distinct addresses and the fixtures ask for the middle
    // one: a lowering that dropped the displacement, or scaled it by one instead
    // of eight, answers a neighbour and passes every test whose table has one row.
    let mut heap = Heap::new(2);
    let first = heap.object(HEAP_CHUNK_WORDS + 4, INT, 1);
    let second = heap.object(HEAP_CHUNK_WORDS + 8, INT, 2);
    let third = heap.object(HEAP_CHUNK_WORDS + 12, INT, 3);
    let literals = [first, second, third];

    // `Answer::returned` is the destination the caller named, which is what a real
    // VM-to-native crossing reads: a reference that was right in the frame and
    // wrong in the destination would be a string that vanished at a tier boundary.
    let mut words = vec![0u64];
    let answer = run_with_literals::<A>(&address, &mut words, 0, &heap, &literals);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        words[0], second,
        "the second literal's address, not a neighbour's"
    );
    assert_eq!(
        answer.returned[0], second,
        "and the same address across the boundary"
    );

    let mut words = vec![0u64, 0];
    let answer = run_with_literals::<A>(&followed, &mut words, 0, &heap, &literals);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[1], 2, "the length of the object that address names");

    // The same code over a *different* table answers differently, which is what
    // says the address is read at run time. It is the assertion an immediate — had
    // one been possible — would have failed.
    let moved = [third, first, second];
    let mut words = vec![0u64];
    let answer = run_with_literals::<A>(&address, &mut words, 0, &heap, &moved);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[0], first);

    // A frame that does not begin at word zero, which is where a lowering that
    // stored the address through the wrong base would show.
    let mut words = vec![9, 9, 9, 9, 0, 0];
    let answer = run_with_literals::<A>(&followed, &mut words, 4, &heap, &literals);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[4], second);
    assert_eq!(words[5], 2);
    assert_eq!(
        &words[..4],
        &[9, 9, 9, 9],
        "and nothing below the frame moved"
    );
}

/// A literal whose `StrId` the program has no string for refuses the function.
///
/// `cove_ir::verify` refuses one too, so this is unreachable for a lowered
/// program — and it is bounded here anyway, because the alternative to refusing
/// is reading past the end of `NativeCtx::literals` into whatever the allocator
/// put there. A refusal is a function on the encoded tier; a read past the table
/// is a wrong address that nothing would report.
pub fn a_literal_past_the_table_refuses_the_function<A: Arm>() {
    let inside = program_with_strings(
        function(
            vec![Repr::Ref],
            REF,
            vec![
                Inst::Str {
                    dst: 0,
                    text: StrId(1),
                },
                Inst::Return { src: 0 },
            ],
        ),
        &["a", "b"],
    );
    assert!(compiles::<A>(&inside));

    let past = program_with_strings(
        function(
            vec![Repr::Ref],
            REF,
            vec![
                Inst::Str {
                    dst: 0,
                    text: StrId(2),
                },
                Inst::Return { src: 0 },
            ],
        ),
        &["a", "b"],
    );
    assert!(
        !compiles::<A>(&past),
        "a `StrId` this program has no string for"
    );
}

/// The byte buffer's two mediated instructions, each handed to the runtime whole
/// with its operands.
///
/// There is no fast path to check here and that is the design — see
/// [`cove_native::GrowableFn`] for why each of the two is the helper and not
/// half of one. So what a case can say is the three things that *are* emitted
/// code: the operation and its two operands reach the helper unchanged; the
/// unpaid work is published and the accumulator cleared, because both are
/// safepoints; and the answer the helper wrote is in the frame afterwards.
///
/// What fills a buffer between them is ADR 0062's window, whose rows are emitted
/// fast paths with this helper as their cold half:
/// [`a_push_window_with_room_writes_the_unit_and_commits_it`] and the cases
/// around it are where those are held.
///
/// The two run in **one body**, in the order a builder is used, with an
/// instruction between them, so the pcs are two different numbers neither of
/// which is one — an emitter that dropped `self.pc` is a wrong pc rather than a
/// coincidence.
pub fn a_growable_buffer_is_handed_to_the_runtime_whole<A: Arm>() {
    forget_built();
    // `s0` is the owner, `s1` the capacity and `s2` the answer.
    let held = program(function(
        vec![Repr::Ref, Repr::Int, Repr::Ref],
        REF,
        vec![
            Inst::GrowableAlloc {
                dst: 0,
                capacity: 1,
                storage: Storage::PackedBytes,
            },
            // Between the two, so that neither pc is one and a dropped `self.pc`
            // cannot pass by coincidence.
            Inst::Unit { dst: 2 },
            Inst::RunFinish {
                dst: 2,
                owner: 0,
                target: REF,
                validation: Validation::Utf8,
                storage: Storage::PackedBytes,
            },
            Inst::Return { src: 2 },
        ],
    ));

    let heap = Heap::new(1);
    let mut words = vec![0u64, 3, 0];
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        built(),
        vec![
            Built {
                base: 0,
                pc: 0,
                op: GrowableOp::Alloc.abi(),
                a: 0,
                b: 1,
                // The block is four instructions and the charge is made at block
                // entry, so the first hand-over carries the whole of it and every
                // one after it carries nought.
                work: 4,
            },
            Built {
                base: 0,
                pc: 2,
                op: GrowableOp::Finish.abi(),
                a: 2,
                b: 0,
                work: 0,
            },
        ],
        "the two operations, in order, with their operands"
    );
    // What the double wrote, in the two slots that have a destination.
    assert_eq!(words[0], u64::from(GrowableOp::Alloc.abi()) * 1000);
    assert_eq!(words[2], u64::from(GrowableOp::Finish.abi()) * 1000 + 2);
    assert_eq!(words[1], 3, "and the operand slot was not written");
    assert_eq!(answer.returned[0], words[2], "the answer at the boundary");
    assert_eq!(
        answer.pending_work, 0,
        "every hand-over is a safepoint, so the accumulator was cleared at each"
    );

    // A frame that does not begin at word zero: `base` is a word index and the
    // helper reads its operands through it.
    forget_built();
    let mut words = vec![9, 9, 9, 9, 0, 3, 0];
    let answer = run_over::<A>(&held, &mut words, 4, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        built().iter().map(|row| row.base).collect::<Vec<_>>(),
        vec![4, 4],
        "the frame the operands are read out of"
    );
    assert_eq!(words[4], u64::from(GrowableOp::Alloc.abi()) * 1000);
    assert_eq!(words[6], u64::from(GrowableOp::Finish.abi()) * 1000 + 2);
    assert_eq!(&words[..4], &[9, 9, 9, 9]);
}

/// A buffer operation the runtime refused leaves with *that* outcome.
///
/// The helper answers an [`Outcome`] and every field it needs — the raise code,
/// the pc, the two numbers — is already written by the time it does, so emitted
/// code tests the outcome once and returns it unchanged. It must not turn a
/// `Stopped` into a `Raised` on the way out, which is what a lowering that
/// answered a constant would do, so both are checked.
pub fn a_buffer_op_the_runtime_refused_leaves_with_that_outcome<A: Arm>() {
    for outcome in [Outcome::Raised, Outcome::Stopped] {
        forget_built();
        built_answers(&[outcome]);
        let held = program(function(
            vec![Repr::Ref, Repr::Int],
            REF,
            vec![
                Inst::Int { dst: 1, value: 8 },
                Inst::GrowableAlloc {
                    dst: 0,
                    capacity: 1,
                    storage: Storage::PackedBytes,
                },
                Inst::Return { src: 0 },
            ],
        ));
        let heap = Heap::new(1);
        let mut words = vec![0u64, 0];
        let answer = run_over::<A>(&held, &mut words, 0, &heap);
        assert_eq!(answer.outcome, outcome);
        assert_eq!(built().len(), 1, "and it left at the first refusal");
        assert_eq!(
            answer.returned[0], UNWRITTEN,
            "nothing was published to the destination"
        );
    }
}

/// The byte buffer's mediated pair is admitted together, and each is refused at
/// a slot the frame does not have.
pub fn a_growable_buffer_is_admitted_as_a_family<A: Arm>() {
    let one = |inst: Inst| {
        program(function(
            vec![Repr::Ref, Repr::Int],
            REF,
            vec![inst, Inst::Return { src: 0 }],
        ))
    };
    for inst in [
        Inst::GrowableAlloc {
            dst: 0,
            capacity: 1,
            storage: Storage::PackedBytes,
        },
        Inst::RunFinish {
            dst: 0,
            owner: 0,
            target: REF,
            validation: Validation::Utf8,
            storage: Storage::PackedBytes,
        },
    ] {
        assert!(compiles::<A>(&one(inst.clone())), "{inst:?}");
        // The same instruction at a slot the frame does not have is `Operands`
        // rather than `Instruction`, and is refused.
        let past = match inst {
            Inst::GrowableAlloc { .. } => Inst::GrowableAlloc {
                dst: 9,
                capacity: 1,
                storage: Storage::PackedBytes,
            },
            _ => Inst::RunFinish {
                dst: 0,
                owner: 9,
                target: REF,
                validation: Validation::Utf8,
                storage: Storage::PackedBytes,
            },
        };
        assert!(
            !compiles::<A>(&one(past)),
            "a slot past the end of the frame"
        );
    }

    // A word allocation is the same helper, handed over whole for the same
    // reason — `core.vectorWithCapacity` since the growable allocation admitted
    // words — and it is admitted on its own rather than paired with a finish: a
    // vector allocated in one function is frozen in another as often as not.
    let words = Storage::Words(INT);
    assert!(compiles::<A>(&one(Inst::GrowableAlloc {
        dst: 0,
        capacity: 1,
        storage: words,
    })));
    // The element layout is read off the instruction, so one past the table is
    // refused for the reason a slot past the frame is.
    assert!(!compiles::<A>(&one(Inst::GrowableAlloc {
        dst: 0,
        capacity: 1,
        storage: Storage::Words(LayoutId(u32::MAX)),
    })));
    assert!(!compiles::<A>(&one(Inst::GrowableAlloc {
        dst: 9,
        capacity: 1,
        storage: words,
    })));
    // A word finish is admitted too, but as an emitted fast path of its own —
    // `every_cold_path_of_a_freeze_goes_to_the_runtime` is where that is
    // asserted — and a word finish into anything but the fixed run of its
    // element is refused.
    assert!(!compiles::<A>(&one(Inst::RunFinish {
        dst: 0,
        owner: 0,
        target: REF,
        validation: Validation::None,
        storage: words,
    })));
}

/// A run copy's five operands, as `ArgsId(1)`: `dst` in slot 0, `dst_at` in 1,
/// `src` in 2, `src_at` in 3 and `count` in 4.
pub fn run_copy_row() -> Vec<Arg> {
    vec![
        Arg {
            slot: 0,
            layout: REF,
        },
        Arg {
            slot: 1,
            layout: INT,
        },
        Arg {
            slot: 2,
            layout: REF,
        },
        Arg {
            slot: 3,
            layout: INT,
        },
        Arg {
            slot: 4,
            layout: INT,
        },
    ]
}

/// Two run copies — one of each storage — and a return, over [`run_copy_row`].
pub fn run_copies() -> Program {
    program_with_args(
        function(
            vec![Repr::Ref, Repr::Int, Repr::Ref, Repr::Int, Repr::Int],
            INT,
            vec![
                Inst::RunCopy {
                    args: ArgsId(1),
                    storage: Storage::PackedBytes,
                },
                Inst::RunCopy {
                    args: ArgsId(1),
                    storage: Storage::Words(PAIR),
                },
                Inst::Return { src: 4 },
            ],
        ),
        run_copy_row(),
    )
}

/// ADR 0058's `run-copy`, over both storages, handed to the runtime whole.
///
/// [`cove_native::RunCopyFn`] is the reason there is no fast path to check. What
/// a case can say is what *is* emitted code: the argument list and the storage
/// reach the helper unchanged — a byte copy as `words = 0` with nought for the
/// element, a word copy as `words = 1` with its element's `LayoutId` — the pc is
/// each instruction's own, the unpaid work is published and cleared because the
/// hand-over is a safepoint, and nothing is written into the frame, because the
/// instruction writes into the object `dst` names and not into a slot.
pub fn a_run_copy_is_handed_to_the_runtime_whole<A: Arm>() {
    forget_copied();
    let held = run_copies();
    let heap = Heap::new(1);
    let frame = [7u64, 1, 9, 2, 3];
    let mut words = frame.to_vec();
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        copied(),
        vec![
            Copied {
                base: 0,
                pc: 0,
                args: 1,
                kind: RunOp::CopyBytes.abi(),
                elem: 0,
                // The block is three instructions, charged at its entry, so the
                // first hand-over carries all of it.
                work: 3,
            },
            Copied {
                base: 0,
                pc: 1,
                args: 1,
                kind: RunOp::CopyWords.abi(),
                elem: PAIR.0,
                work: 0,
            },
        ],
        "both copies, in order, with their storage"
    );
    assert_eq!(words, frame, "and not a word of the frame was written");
    assert_eq!(answer.returned[0], 3, "the count, returned after both");
    assert_eq!(answer.pending_work, 0);

    // A frame that does not begin at word zero: `base` is a word index.
    forget_copied();
    let mut words = vec![5, 5, 7, 1, 9, 2, 3];
    let answer = run_over::<A>(&held, &mut words, 2, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        copied().iter().map(|row| row.base).collect::<Vec<_>>(),
        vec![2, 2]
    );
    assert_eq!(&words[..2], &[5, 5]);
}

/// A run copy the runtime refused — or whose poll said stop — leaves with *that*
/// outcome, and runs nothing after it.
pub fn a_run_copy_the_runtime_refused_leaves_with_that_outcome<A: Arm>() {
    for outcome in [Outcome::Raised, Outcome::Stopped] {
        forget_copied();
        copied_answers(&[outcome]);
        let heap = Heap::new(1);
        let mut words = vec![7u64, 1, 9, 2, 3];
        let answer = run_over::<A>(&run_copies(), &mut words, 0, &heap);
        assert_eq!(answer.outcome, outcome);
        assert_eq!(copied().len(), 1, "and it left at the first refusal");
        assert_eq!(
            answer.returned[0], UNWRITTEN,
            "nothing was published to the destination"
        );
    }
}

/// A run copy is admitted over both storages with five one-word operands the
/// frame has, and refused otherwise.
///
/// `cove_ir::verify` holds the row to that shape, so the refusals are unreachable
/// for a lowered program — and they are bounded here anyway, because what the
/// helper does with a short row is index past the end of it, and what it does with
/// an element layout the table does not have is read past the table.
pub fn a_run_copy_is_admitted_with_five_one_word_operands<A: Arm>() {
    let one = |storage: Storage, row: Vec<Arg>| {
        program_with_args(
            function(
                vec![Repr::Ref, Repr::Int, Repr::Ref, Repr::Int, Repr::Int],
                INT,
                vec![
                    Inst::RunCopy {
                        args: ArgsId(1),
                        storage,
                    },
                    Inst::Return { src: 4 },
                ],
            ),
            row,
        )
    };
    for storage in [
        Storage::PackedBytes,
        Storage::Words(INT),
        Storage::Words(PAIR),
    ] {
        assert!(
            compiles::<A>(&one(storage, run_copy_row())),
            "{storage:?} is admitted"
        );
        let mut short = run_copy_row();
        short.pop();
        assert!(
            !compiles::<A>(&one(storage, short)),
            "{storage:?}: four operands is not a `run-copy` row"
        );
        let mut past = run_copy_row();
        past[4].slot = 9;
        assert!(
            !compiles::<A>(&one(storage, past)),
            "{storage:?}: an operand at a slot the frame does not have"
        );
        let mut wide = run_copy_row();
        wide[1].layout = PAIR;
        assert!(
            !compiles::<A>(&one(storage, wide)),
            "{storage:?}: an operand that is two words"
        );
    }
    assert!(
        !compiles::<A>(&one(Storage::Words(LayoutId(9_999)), run_copy_row())),
        "an element layout the program does not have"
    );
}

/// A word truncate — `Vector.pop` and `Vector.remove`'s last step — handed to
/// the growable helper whole as [`GrowableOp::TruncateWords`], with the owner's
/// slot and the length's, at its own pc; admitted over words with both slots in
/// the frame, and refused at a slot the frame does not have or over bytes.
pub fn a_word_truncate_is_handed_to_the_runtime_whole<A: Arm>() {
    let one = |owner: u32, storage: Storage| {
        program(function(
            vec![Repr::Ref, Repr::Int],
            INT,
            vec![
                Inst::GrowableTruncate {
                    owner,
                    len: 1,
                    storage,
                },
                Inst::Return { src: 1 },
            ],
        ))
    };
    forget_built();
    let heap = Heap::new(1);
    let mut words = vec![7u64, 2];
    let answer = run_over::<A>(&one(0, Storage::Words(PAIR)), &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        built(),
        vec![Built {
            base: 0,
            pc: 0,
            op: GrowableOp::TruncateWords.abi(),
            a: 0,
            b: 1,
            work: 2,
        }]
    );
    assert_eq!(answer.returned[0], 2);
    for outcome in [Outcome::Raised, Outcome::Stopped] {
        forget_built();
        built_answers(&[outcome]);
        let mut words = vec![7u64, 2];
        let answer = run_over::<A>(&one(0, Storage::Words(PAIR)), &mut words, 0, &heap);
        assert_eq!(answer.outcome, outcome);
        assert_eq!(answer.returned[0], UNWRITTEN);
    }
    assert!(compiles::<A>(&one(0, Storage::Words(INT))));
    assert!(
        !compiles::<A>(&one(9, Storage::Words(INT))),
        "a slot the frame does not have"
    );
    assert!(
        !compiles::<A>(&one(0, Storage::PackedBytes)),
        "no byte member"
    );
}

/// A run slice's four operands, as `ArgsId(1)`: `dst` in slot 3, `src` in 0,
/// `from` in 1 and `count` in 2 — the destination last in the frame and first in
/// the row, so an arm that confused the row's order with the frame's is caught.
pub fn run_slice_row() -> Vec<Arg> {
    vec![
        Arg {
            slot: 3,
            layout: STORE,
        },
        Arg {
            slot: 0,
            layout: REF,
        },
        Arg {
            slot: 1,
            layout: INT,
        },
        Arg {
            slot: 2,
            layout: INT,
        },
    ]
}

/// One word run slice and a return of its destination, over [`run_slice_row`].
pub fn run_slices() -> Program {
    program_with_args(
        function(
            vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Ref],
            REF,
            vec![
                Inst::RunSlice {
                    args: ArgsId(1),
                    storage: Storage::Words(PAIR),
                },
                Inst::Return { src: 3 },
            ],
        ),
        run_slice_row(),
    )
}

/// ADR 0058's `run-slice`, handed to the runtime whole on the run-copy helper.
///
/// [`a_run_copy_is_handed_to_the_runtime_whole`]'s case for the slice: the row
/// and the element reach the helper unchanged as [`RunOp::SliceWords`], at the
/// instruction's own pc, with the unpaid work published — and the destination is
/// read back out of the frame *after* the call, so the answer is whatever the
/// helper left in the slot. The double writes nothing, so that is the word that
/// was there; a real helper writes the fresh run, and an arm that had cached the
/// slot across the call would answer the stale word either way, which is why the
/// frame is given a word there to go stale.
pub fn a_run_slice_is_handed_to_the_runtime_whole<A: Arm>() {
    forget_copied();
    let held = run_slices();
    let heap = Heap::new(1);
    let frame = [9u64, 2, 3, 77];
    let mut words = frame.to_vec();
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        copied(),
        vec![Copied {
            base: 0,
            pc: 0,
            args: 1,
            kind: RunOp::SliceWords.abi(),
            elem: PAIR.0,
            work: 2,
        }],
        "the slice, with its storage"
    );
    assert_eq!(words, frame, "the double wrote nothing");
    assert_eq!(
        answer.returned[0], 77,
        "the destination, read after the call"
    );

    // A refusal leaves with that outcome and answers nothing.
    for outcome in [Outcome::Raised, Outcome::Stopped] {
        forget_copied();
        copied_answers(&[outcome]);
        let mut words = frame.to_vec();
        let answer = run_over::<A>(&held, &mut words, 0, &heap);
        assert_eq!(answer.outcome, outcome);
        assert_eq!(answer.returned[0], UNWRITTEN);
    }

    // And the byte member, which is the same helper with `RunOp::SliceBytes` and
    // no element: nought, and unread.
    forget_copied();
    let bytes = run_byte_slices();
    let mut words = frame.to_vec();
    let answer = run_over::<A>(&bytes, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        copied(),
        vec![Copied {
            base: 0,
            pc: 0,
            args: 1,
            kind: RunOp::SliceBytes.abi(),
            elem: 0,
            work: 2,
        }],
        "the byte slice, with its storage"
    );
    assert_eq!(
        answer.returned[0], 77,
        "the destination, read after the call"
    );
}

/// One byte run slice and a return of its destination, over [`run_slice_row`].
pub fn run_byte_slices() -> Program {
    program_with_args(
        function(
            vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Ref],
            REF,
            vec![
                Inst::RunSlice {
                    args: ArgsId(1),
                    storage: Storage::PackedBytes,
                },
                Inst::Return { src: 3 },
            ],
        ),
        run_slice_row(),
    )
}

/// A run slice is admitted over words with four one-word operands the frame has,
/// and refused otherwise.
pub fn a_run_slice_is_admitted_with_four_one_word_operands<A: Arm>() {
    let one = |storage: Storage, row: Vec<Arg>| {
        program_with_args(
            function(
                vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Ref],
                REF,
                vec![
                    Inst::RunSlice {
                        args: ArgsId(1),
                        storage,
                    },
                    Inst::Return { src: 3 },
                ],
            ),
            row,
        )
    };
    for storage in [Storage::Words(INT), Storage::Words(PAIR)] {
        assert!(
            compiles::<A>(&one(storage, run_slice_row())),
            "{storage:?} is admitted"
        );
        let mut short = run_slice_row();
        short.pop();
        assert!(
            !compiles::<A>(&one(storage, short)),
            "{storage:?}: three operands is not a `run-slice` row"
        );
        let mut past = run_slice_row();
        past[0].slot = 9;
        assert!(
            !compiles::<A>(&one(storage, past)),
            "{storage:?}: a destination at a slot the frame does not have"
        );
        let mut wide = run_slice_row();
        wide[2].layout = PAIR;
        assert!(
            !compiles::<A>(&one(storage, wide)),
            "{storage:?}: an operand that is two words"
        );
    }
    // The byte member, `String.sliceBytes`' copy, admitted by the same bounds.
    assert!(
        compiles::<A>(&one(Storage::PackedBytes, run_slice_row())),
        "the byte member is admitted"
    );
    let mut short = run_slice_row();
    short.pop();
    assert!(
        !compiles::<A>(&one(Storage::PackedBytes, short)),
        "bytes: three operands is not a `run-slice` row"
    );
    assert!(
        !compiles::<A>(&one(Storage::Words(LayoutId(9_999)), run_slice_row())),
        "an element layout the program does not have"
    );
}

/// One `intrinsic-call` of `receiver.operation` over two references, answering
/// into slot 1, and returned.
pub fn intrinsic_calling(receiver: &str, operation: &str) -> Program {
    program_with_intrinsic(
        function(
            vec![Repr::Ref, Repr::Int, Repr::Ref],
            INT,
            vec![
                Inst::IntrinsicCall {
                    dst: 1,
                    site: SiteId(0),
                    args: ArgsId(1),
                },
                Inst::Return { src: 1 },
            ],
        ),
        receiver,
        operation,
        INT,
        vec![
            Arg {
                slot: 0,
                layout: REF,
            },
            Arg {
                slot: 2,
                layout: REF,
            },
        ],
    )
}

/// One intrinsic of each effect class, as the pair of names that resolves to it:
/// a plain call (`String.contains`: neither collects nor raises), a raise
/// (`Any.equals`: a walk too deep to finish) and a safepoint (`String.trim`:
/// allocates the string it answers).
pub const INTRINSIC_CLASSES: [(&str, &str); 3] = [
    ("String", "contains"),
    ("Any", "equals"),
    ("String", "trim"),
];

/// **An `intrinsic-call` is handed over with the protocol its effects ask for.**
///
/// One call of each class in [`INTRINSIC_CLASSES`], with the helper double
/// scripted to answer `Returned` and then `Raised`, and what the arm did around
/// the call read back from what the double saw and what the entry left:
///
/// - the hand-over is the instruction's own numbers — the frame, the pc, the
///   destination, the site and the argument list;
/// - the unpaid work is **published before the call only for a safepoint**, and
///   is charged exactly once either way: on the call for a safepoint, on the exit
///   for anything else;
/// - a `Raised` answer **leaves only where the protocol tests the outcome**. A
///   plain call does not read it, so the function returns — which is the whole
///   saving, and which the runtime's helper makes unobservable by never answering
///   anything but `Returned` for such an intrinsic.
///
pub fn an_intrinsic_call_is_handed_over_by_its_effects<A: Arm>() {
    let base = 3u64;
    for (receiver, operation) in INTRINSIC_CLASSES {
        let intrinsic = cove_ir::Intrinsic::from_names(receiver, operation)
            .unwrap_or_else(|| panic!("`{receiver}.{operation}` is an intrinsic"));
        let program = intrinsic_calling(receiver, operation);
        let protocol = IntrinsicProtocol::of(intrinsic);
        for scripted in [Outcome::Returned, Outcome::Raised] {
            forget_mediated();
            mediated_answers(&[scripted]);
            let mut words = vec![0u64; 8];
            words[base as usize + 1] = 0xfeed;
            let answer = run::<A>(&program, &mut words, base);
            let what = format!("`{intrinsic}` answering {scripted:?}");

            let handed = mediated();
            assert_eq!(handed.len(), 1, "{what}: one hand-over");
            let handed = handed[0];
            assert_eq!(
                (handed.base, handed.pc, handed.dst, handed.site, handed.args),
                (base, 0, 1, 0, 1),
                "{what}: the instruction's own operands"
            );
            // The block is the call and the return: two instructions.
            let charge = 2;
            assert_eq!(
                handed.work,
                if protocol.safepoint { charge } else { 0 },
                "{what}: the work is published before a safepoint and only then"
            );
            assert_eq!(
                answer.pending_work,
                if protocol.safepoint { 0 } else { charge },
                "{what}: and charged once, on the exit, where it was not"
            );

            let leaves = scripted == Outcome::Raised && protocol.tests_outcome();
            assert_eq!(
                answer.outcome,
                if leaves {
                    Outcome::Raised
                } else {
                    Outcome::Returned
                },
                "{what}: the outcome is read where the protocol says it can differ"
            );
            if !leaves {
                // The double writes `site * 1000 + dst` when it answers
                // `Returned`, and writes nothing when it does not.
                let expected = if scripted == Outcome::Returned {
                    1
                } else {
                    0xfeed
                };
                assert_eq!(answer.returned[0], expected, "{what}: the answer");
            }
        }
    }
}

/// An `intrinsic-call` with a site the program does not have is refused, and so
/// is one whose argument list is not there: bounded like any other operand.
pub fn an_intrinsic_call_out_of_bounds_refuses_the_function<A: Arm>() {
    let (receiver, operation) = INTRINSIC_CLASSES[0];
    assert!(compiles::<A>(&intrinsic_calling(receiver, operation)));
    let mut no_site = intrinsic_calling(receiver, operation);
    no_site.intrinsic_sites.clear();
    assert!(!compiles::<A>(&no_site), "a site the program does not have");
    let mut no_args = intrinsic_calling(receiver, operation);
    no_args.args.truncate(1);
    assert!(
        !compiles::<A>(&no_args),
        "an argument list the program does not have"
    );
}

/// One `intrinsic-call` per pair in `names`, each answering into slot 1, and
/// then a return.
///
/// [`intrinsic_calling`] for more than one call, with site `n` being `names[n]`
/// because [`cove_ir::IntrinsicSite`]s are pushed in order. The argument list is
/// the same one for every call — the double does not read it — so what differs
/// between two of these calls is the variant and nothing else, which is what a
/// case counting sites per variant wants to be able to say.
pub fn intrinsic_calling_each(names: &[(&str, &str)]) -> Program {
    let mut code: Vec<Inst> = names
        .iter()
        .enumerate()
        .map(|(at, _)| Inst::IntrinsicCall {
            dst: 1,
            site: SiteId(at as u32),
            args: ArgsId(1),
        })
        .collect();
    code.push(Inst::Return { src: 1 });
    let mut held = program_with_args(
        function(vec![Repr::Ref, Repr::Int, Repr::Ref], INT, code),
        vec![
            Arg {
                slot: 0,
                layout: REF,
            },
            Arg {
                slot: 2,
                layout: REF,
            },
        ],
    );
    for (receiver, operation) in names {
        let intrinsic = cove_ir::Intrinsic::from_names(receiver, operation)
            .unwrap_or_else(|| panic!("`{receiver}.{operation}` has no `Intrinsic`"));
        held.intrinsic_sites.push(cove_ir::IntrinsicSite {
            intrinsic,
            result: INT,
        });
    }
    held
}

/// What a variant's row of an [`IntrinsicCode`] would be if it were the only
/// variant with a site: a table of noughts with `sites` at its own index.
fn only_variant(receiver: &str, operation: &str, sites: u64) -> Vec<u64> {
    let intrinsic = cove_ir::Intrinsic::from_names(receiver, operation)
        .unwrap_or_else(|| panic!("`{receiver}.{operation}` is an intrinsic"));
    let mut wanted = vec![0; cove_ir::intrinsic::COUNT];
    wanted[intrinsic.index()] = sites;
    wanted
}

/// **An intrinsic call's machine code is charged to its own variant, and to
/// nothing else.**
///
/// [ADR 0064](https://github.com/myuon/cove/blob/main/docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
/// Decision 7 attribution, held to the same three things
/// [`a_windows_machine_code_is_charged_to_its_pattern`] holds its own to, one
/// level over:
///
/// - a function with one `intrinsic-call` charges **one site** to that call's
///   variant and none to the other thirty, and a function with no intrinsic call
///   in it charges nothing anywhere — so the count follows the IR and not the
///   shape of the body;
/// - where the bytes are attributed they are **positive and no more than the
///   whole function**, which is the arithmetic a reader of the report assumes;
/// - **whether they are attributed at all is the arm's and not the program's**,
///   which is the claim `ATTRIBUTES_INTRINSIC_CALLS` exists to make failable.
///
/// One case per member of [`INTRINSIC_CLASSES`], because the three differ in
/// exactly the thing that decides how much code a site is — the protocol — and a
/// charge taken across the wrong span would show up on the safepoint class and
/// on no other.
pub fn an_intrinsic_calls_machine_code_is_charged_to_its_variant<A: Arm>() {
    let compiled = |program: &Program| {
        let mut jit = A::new(helpers());
        jit.compile(program, FunctionId(0))
            .expect("the function is inside the slice")
    };
    for (receiver, operation) in INTRINSIC_CLASSES {
        let what = format!("`{receiver}.{operation}`");
        let handle = compiled(&intrinsic_calling(receiver, operation));
        let code = A::intrinsic_code(handle);
        assert_eq!(
            code.sites.to_vec(),
            only_variant(receiver, operation, 1),
            "{what}: one site, its own"
        );
        assert_eq!(code.total_sites(), 1, "{what}");
        assert_eq!(
            code.bytes.is_some(),
            A::ATTRIBUTES_INTRINSIC_CALLS,
            "{what}: whether the bytes are attributed is the arm's, not the program's"
        );
        let Some(bytes) = code.bytes else {
            continue;
        };
        let whole = u64::from(A::code_bytes(handle));
        let at = cove_ir::Intrinsic::from_names(receiver, operation)
            .expect("an intrinsic")
            .index();
        let charged = bytes[at];
        println!("{what}: {charged} of {whole} byte(s) are the call");
        assert!(charged > 0, "{what}: a call site that emitted nothing");
        assert!(
            charged <= whole,
            "{what}: {charged} charged out of a function of {whole}"
        );
        assert_eq!(
            bytes.iter().sum::<u64>(),
            charged,
            "{what}: another variant was charged"
        );
        assert_eq!(
            code.total_bytes(),
            Some(charged),
            "{what}: the total is the sum of the rows"
        );
    }

    // And a body with no `intrinsic-call` in it charges nothing, because there
    // is no site in it to charge.
    let nothing = program(function(
        vec![Repr::Int],
        INT,
        vec![Inst::Return { src: 0 }],
    ));
    let code = A::intrinsic_code(compiled(&nothing));
    assert_eq!(code.total_sites(), 0, "a function with no intrinsic call");
    assert_eq!(
        code.bytes,
        Some([0; cove_ir::intrinsic::COUNT]),
        "a function with no intrinsic call has nought bytes of intrinsic call, \
         which is a measurement and not an absence"
    );
}

/// **Every site is counted, and every site of one variant is charged to one
/// row.**
///
/// The case above would pass a `charge` that ran once per *function* rather than
/// once per site, which is the way a per-variant table most plausibly goes
/// wrong: three calls in one body, two of them the same variant, and the table
/// has to read two and one rather than one and one.
///
/// Where the bytes are attributed, the repeated variant's row is held to
/// **exactly twice** what the same variant's single-site function charges. That
/// is an equality rather than an inequality because this arm's encoder does not
/// choose between encodings — every immediate it lays down for a site is a fixed
/// width, whatever the pc, the destination or the site number is — so two sites
/// of one variant are two identical spans. If that ever stops being true this
/// fails, which is the point: a byte figure whose size depends on where in a
/// function a call sits is one a report cannot divide by the sites.
pub fn every_intrinsic_call_site_is_counted<A: Arm>() {
    let compiled = |program: &Program| {
        let mut jit = A::new(helpers());
        jit.compile(program, FunctionId(0))
            .expect("the function is inside the slice")
    };
    let (repeated, once) = (INTRINSIC_CLASSES[0], INTRINSIC_CLASSES[2]);
    let handle = compiled(&intrinsic_calling_each(&[repeated, once, repeated]));
    let code = A::intrinsic_code(handle);

    let mut wanted = only_variant(repeated.0, repeated.1, 2);
    let at_once = cove_ir::Intrinsic::from_names(once.0, once.1)
        .expect("an intrinsic")
        .index();
    wanted[at_once] = 1;
    assert_eq!(
        code.sites.to_vec(),
        wanted,
        "two of one variant and one of another"
    );
    assert_eq!(code.total_sites(), 3, "three sites, three charges");

    assert_eq!(
        code.bytes.is_some(),
        A::ATTRIBUTES_INTRINSIC_CALLS,
        "whether the bytes are attributed is the arm's, not the program's"
    );
    let Some(bytes) = code.bytes else {
        return;
    };
    let at_repeated = cove_ir::Intrinsic::from_names(repeated.0, repeated.1)
        .expect("an intrinsic")
        .index();
    let one_site = A::intrinsic_code(compiled(&intrinsic_calling(repeated.0, repeated.1)))
        .bytes
        .expect("this arm attributes bytes");
    let alone = one_site[at_repeated];
    println!(
        "`{}.{}`: {} byte(s) for two sites, {alone} for one",
        repeated.0, repeated.1, bytes[at_repeated]
    );
    assert_eq!(
        bytes[at_repeated],
        2 * alone,
        "two sites of one variant are two identical spans"
    );
    assert!(bytes[at_once] > 0, "the third variant charged nothing");
    assert_eq!(
        code.total_bytes(),
        Some(bytes[at_repeated] + bytes[at_once]),
        "the total is the sum of the rows, and no other row was charged"
    );
    assert!(
        code.total_bytes() <= Some(u64::from(A::code_bytes(handle))),
        "more was charged to the calls than the function is"
    );
}

// --- allocation ---------------------------------------------------------------

/// One `Inst::Alloc` of each of `Len`'s three forms, answering into slot 1.
pub fn allocating(len: Len) -> Program {
    program(function(
        vec![Repr::Int, Repr::Ref],
        REF,
        vec![
            Inst::Alloc {
                dst: 1,
                layout: STORE,
                len,
            },
            Inst::Return { src: 1 },
        ],
    ))
}

/// `encoded.rs`'s `ALLOC_FIXED | ALLOC_IMM | ALLOC_SLOT` arm: the runtime
/// allocates and the address it answered lands in the destination.
///
/// The three `Len` forms are three opcodes in the encoded tier and one helper
/// call here, so what this asserts is the *conversion*: `Len::Fixed` hands over
/// nought, `Len::Count` hands over its immediate, and `Len::Slot` hands over the
/// whole word the program computed — **not** a narrowed copy of it, which is why
/// one of the rows is a count no `u32` holds. `Machine::allocate` is what refuses
/// that one, through the same "this run has no memory left" an exhausted heap
/// raises, and refusing it here instead would be a second refusal with a
/// different message.
pub fn an_allocation_hands_the_layout_and_the_length_over_whole<A: Arm>() {
    let rows: [(Len, i64, i64); 4] = [
        // `Len::Fixed`: the layout fixes the size, so the count is nought.
        (Len::Fixed, 0, 0),
        (Len::Count(7), 0, 7),
        (Len::Slot(0), 5, 5),
        // A word no `u32` holds, handed over as it lies.
        (Len::Slot(0), -1, -1),
    ];
    for (len, word, expected) in rows {
        forget_allocations();
        let held = allocating(len);
        let heap = Heap::new(2);
        let mut words = vec![word as u64, 0];
        let answer = run_over::<A>(&held, &mut words, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Returned, "for {len:?}");
        assert_eq!(
            allocations(),
            vec![(0, STORE.0, expected)],
            "the pc, the layout and the length, for {len:?}"
        );
        // The double bumps from word one, so the first object of a case is there.
        assert_eq!(words[1], HEAP_ORIGIN_WORDS + 1, "the address it answered");
        assert_eq!(answer.returned[0], HEAP_ORIGIN_WORDS + 1);
        // The helper is a safepoint, so the block's static work went over with it
        // and the accumulator was cleared: what the return published is the work
        // of the instructions *after* the allocation, which is the `return`.
        assert_eq!(
            answer.pending_work, 0,
            "the work was published to the helper and the accumulator cleared"
        );
    }
}

/// An allocation the runtime refuses leaves as [`Raise::Called`].
///
/// Zero is not an address, so it is the whole of the ABI for "I could not, and I
/// am holding the sentence" — see [`cove_native::AllocFn`]. What a case can check
/// is that compiled code *left*, that it named the variant whose message the
/// runtime owns, and that it named the instruction: the span a "this run has no
/// memory left" carries is `Function::span_at(pc)`, and only compiled code knows
/// which pc it was.
pub fn an_allocation_the_runtime_refuses_leaves_as_called<A: Arm>() {
    forget_allocations();
    allocations_allowed(0);
    let held = allocating(Len::Count(3));
    let heap = Heap::new(2);
    let mut words = vec![0u64, UNWRITTEN];
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Raised);
    assert_eq!(answer.raise, Some(Raise::Called));
    assert_eq!(answer.raise_pc, 0, "the allocating instruction");
    assert_eq!(
        words[1], UNWRITTEN,
        "and nothing was stored, because there was no address to store"
    );
    forget_allocations();
}

/// **A reference is in its slot when the allocation helper runs.**
///
/// `cove_native::abi`'s "References are live here, and the frame is why that is
/// safe", checked rather than argued. An allocation is the first thing compiled
/// code does that can *cause* a collection, and the collector's root walk sees
/// exactly what the helper sees — so the helper reads the reference slot and this
/// asserts what it found. An arm that had promoted the reference into a register
/// would leave the slot holding whatever it held before.
///
/// The reference is read again *after* the allocation and returned, so a slot that
/// was merely stale rather than wrong cannot pass either.
pub fn a_reference_is_in_its_slot_across_an_allocation<A: Arm>() {
    forget_allocations();
    forget_polls();
    let held = program(function(
        vec![Repr::Ref, Repr::Ref, Repr::Ref],
        REF,
        vec![
            // The reference arrives in slot 0 and is copied to slot 1, which is
            // where the walk has to find it.
            Inst::Copy {
                dst: 1,
                src: 0,
                layout: REF,
            },
            Inst::Alloc {
                dst: 2,
                layout: STORE,
                len: Len::Count(1),
            },
            Inst::Return { src: 1 },
        ],
    ));
    let at = HEAP_CHUNK_WORDS + 21;
    let mut heap = Heap::new(2);
    let object = heap.object(at, INT, 4242);
    // The frame is at word 3 of the segment, so the watched index is the frame's
    // own offset plus the slot — which is what makes a case that watched slot 1 of
    // word zero fail here.
    let base = 3;
    watch(base as usize + 1);
    let mut words = vec![UNWRITTEN, UNWRITTEN, UNWRITTEN, object, 0, 0];
    let answer = run_over::<A>(&held, &mut words, base, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        watched(),
        vec![object],
        "the helper — and so a collector — found the reference in its slot"
    );
    assert_eq!(
        answer.returned[0], object,
        "and it was still there afterwards"
    );
    watch_nothing();
    forget_allocations();
}

// --- growable owners ----------------------------------------------------------

/// How a case builds the owner a cold path is given: a [`Heap`] and where in it,
/// answering the header's address.
///
/// A named type rather than the signature written in place, because the rows
/// below are arrays of them and `clippy::type_complexity` is right that the
/// written-out form is unreadable there.
type Build = fn(&mut Heap, u64) -> u64;

/// A `ByteBuffer` owner and its packed store, in the shape `Machine::buffer`
/// reads them: payload word 0 the length in bytes, payload word 1 the store, and
/// the store's header length its capacity in bytes.
///
/// The store's payload is filled with `0xAA` bytes, so a write that cleared or
/// shifted the wrong bits shows as a changed neighbour rather than a nought that
/// happened to be there already.
pub fn a_byte_buffer(heap: &mut Heap, at: u64, len: u64, capacity: u32) -> u64 {
    let owner = heap.object(at, BUFFER, 0);
    let store = heap.object(at + 8, BYTES, capacity);
    for word in 0..u64::from(capacity).div_ceil(8) {
        heap.set(at + 8 + 1 + word, 0xAAAA_AAAA_AAAA_AAAA);
    }
    heap.set(at + 1, len);
    heap.set(at + 2, store);
    owner
}

/// A `Vector` header and its store, in the shape `vector()` reads them.
///
/// Two payload words: the element count and the store's address, which is
/// `Shape::Vector`'s layout. The store is an ordinary object whose header length
/// is the capacity *in elements*.
pub fn a_vector(heap: &mut Heap, at: u64, layout: LayoutId, len: u32, capacity: u32) -> u64 {
    let header = heap.object(at, layout, 0);
    let store = heap.object(at + 8, STORE, capacity);
    heap.set(at + 1, u64::from(len));
    heap.set(at + 2, store);
    header
}

// --- ADR 0062's buffer window -------------------------------------------------

/// One `growable-ensure` — or, when `commit`, one `growable-commit` — of
/// `storage`, then a `()`: slot 0 the owner, slot 1 the count, slot 2 the answer.
pub fn reserving(storage: Storage, commit: bool) -> Program {
    let inst = match commit {
        false => Inst::GrowableEnsure {
            owner: 0,
            additional: 1,
            storage,
        },
        true => Inst::GrowableCommit {
            owner: 0,
            count: 1,
            storage,
        },
    };
    program(function(
        vec![Repr::Ref, Repr::Int, Repr::Unit],
        UNIT,
        vec![inst, Inst::Unit { dst: 2 }, Inst::Return { src: 2 }],
    ))
}

/// One byte `run-store`, then a `()`: slot 0 the run, slot 1 the offset, slot 2
/// the value and slot 3 the answer.
pub fn storing_a_byte() -> Program {
    program(function(
        vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Unit],
        UNIT,
        vec![
            Inst::RunStore {
                run: 0,
                index: 1,
                src: 2,
                storage: Storage::PackedBytes,
            },
            Inst::Unit { dst: 3 },
            Inst::Return { src: 3 },
        ],
    ))
}

/// A whole push window over `storage`, as ADR 0062's `Vector.push` and
/// `appendByte` will lower: the length, a constant `1`, the ensure, the store read
/// after it, the one write at the length, a clear of the store, a second `1`, and
/// the commit.
///
/// Slot 0 is the owner, slot 1 the length, slot 2 the count, slot 3 the store,
/// slots 4.. the unit — one `Int` for a byte or an `Int` element, two for a
/// `Pair` — then the commit's count and the answer.
pub fn a_push_window(storage: Storage) -> Program {
    let (stride, write) = match storage {
        Storage::PackedBytes => (
            1,
            Inst::RunStore {
                run: 3,
                index: 1,
                src: 4,
                storage,
            },
        ),
        Storage::Words(elem) => (
            if elem == PAIR { 2 } else { 1 },
            Inst::StoreElem {
                obj: 3,
                index: 1,
                src: 4,
                layout: elem,
            },
        ),
    };
    let mut reprs = vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Ref];
    reprs.extend(std::iter::repeat_n(Repr::Int, stride as usize));
    let count = 4 + stride;
    reprs.push(Repr::Int);
    reprs.push(Repr::Unit);
    program(function(
        reprs,
        UNIT,
        vec![
            Inst::LoadField {
                dst: 1,
                obj: 0,
                at: 0,
                layout: INT,
            },
            Inst::Int { dst: 2, value: 1 },
            Inst::GrowableEnsure {
                owner: 0,
                additional: 2,
                storage,
            },
            Inst::LoadField {
                dst: 3,
                obj: 0,
                at: 1,
                layout: REF,
            },
            write,
            Inst::Clear {
                slot: 3,
                layout: REF,
            },
            Inst::Int {
                dst: count,
                value: 1,
            },
            Inst::GrowableCommit {
                owner: 0,
                count,
                storage,
            },
            Inst::Unit { dst: count + 1 },
            Inst::Return { src: count + 1 },
        ],
    ))
}

/// `Machine::ensure_growable` and `Machine::commit_growable` where the room is
/// there: an ensure does nothing and a commit writes `length + count`, and
/// **nothing is handed to the runtime** — over bytes and words, for a count of
/// nought, one, and exactly the room.
pub fn a_reservation_with_room_is_answered_in_emitted_code<A: Arm>() {
    let at = HEAP_CHUNK_WORDS + 33;
    let words = Storage::Words(INT);
    for (storage, build) in [
        (
            Storage::PackedBytes,
            (|heap, at| a_byte_buffer(heap, at, 10, 16)) as Build,
        ),
        (words, |heap, at| a_vector(heap, at, VECTOR, 10, 16)),
    ] {
        for commit in [false, true] {
            for count in [0u64, 1, 6] {
                forget_built();
                let what = format!("{storage:?}, commit {commit}, count {count}");
                let held = reserving(storage, commit);
                let mut heap = Heap::new(2);
                let owner = build(&mut heap, at);
                let mut frame = vec![owner, count, UNWRITTEN];
                let answer = run_over::<A>(&held, &mut frame, 0, &heap);
                assert_eq!(answer.outcome, Outcome::Returned, "{what}");
                assert!(built().is_empty(), "{what}: {:?}", built());
                let expected = if commit { 10 + count } else { 10 };
                assert_eq!(heap.get(at + 1), expected, "{what}: the length");
                assert_eq!(heap.get(at + 2), heap.addr(at + 8), "{what}: the store");
                assert_eq!(frame[2], 0, "{what}: the `unit` after it ran");
            }
        }
    }
    forget_built();
}

/// The cold paths of an ensure and a commit, each handed to the runtime whole:
/// a count past the room, a negative count, a consumed owner, an object of
/// another family, and a length word past the capacity. What is asserted is that
/// emitted code **did not try** — the operation went over at pc 0 with the
/// owner's slot and the count's, nothing in the heap was written, and the
/// instruction after it ran once the runtime answered.
pub fn every_cold_path_of_a_reservation_goes_to_the_runtime<A: Arm>() {
    let at = HEAP_CHUNK_WORDS + 33;
    let bytes: [(&str, u64, Build); 5] = [
        ("the count is past the room", 7, |heap, at| {
            a_byte_buffer(heap, at, 10, 16)
        }),
        ("the count is negative", u64::MAX, |heap, at| {
            a_byte_buffer(heap, at, 10, 16)
        }),
        ("`finish()` consumed the store", 1, |heap, at| {
            let owner = a_byte_buffer(heap, at, 0, 16);
            heap.set(at + 2, 0);
            owner
        }),
        ("the object is not a byte buffer", 1, |heap, at| {
            a_vector(heap, at, VECTOR, 0, 16)
        }),
        ("the length word is past the capacity", 0, |heap, at| {
            a_byte_buffer(heap, at, 1 << 40, 16)
        }),
    ];
    let words: [(&str, u64, Build); 5] = [
        ("the count is past the room", 7, |heap, at| {
            a_vector(heap, at, VECTOR, 10, 16)
        }),
        ("the count is negative", u64::MAX, |heap, at| {
            a_vector(heap, at, VECTOR, 10, 16)
        }),
        ("`freeze()` consumed the store", 1, |heap, at| {
            let owner = a_vector(heap, at, VECTOR, 0, 16);
            heap.set(at + 2, 0);
            owner
        }),
        ("the object is another vector", 1, |heap, at| {
            a_vector(heap, at, PAIR_VECTOR, 0, 16)
        }),
        ("the length word is past the capacity", 0, |heap, at| {
            let owner = a_vector(heap, at, VECTOR, 0, 16);
            heap.set(at + 1, 1 << 40);
            owner
        }),
    ];
    for (storage, rows) in [(Storage::PackedBytes, bytes), (Storage::Words(INT), words)] {
        for commit in [false, true] {
            let op = match (storage, commit) {
                (Storage::PackedBytes, false) => GrowableOp::EnsureBytes,
                (Storage::PackedBytes, true) => GrowableOp::CommitBytes,
                (Storage::Words(_), false) => GrowableOp::EnsureWords,
                (Storage::Words(_), true) => GrowableOp::CommitWords,
            };
            for (why, count, build) in rows {
                forget_built();
                let what = format!("{op:?}: {why}");
                let held = reserving(storage, commit);
                let mut heap = Heap::new(2);
                let owner = build(&mut heap, at);
                let before: Vec<u64> = (0..16).map(|word| heap.get(at + word)).collect();
                let mut frame = vec![owner, count, UNWRITTEN];
                let answer = run_over::<A>(&held, &mut frame, 0, &heap);
                assert_eq!(answer.outcome, Outcome::Returned, "{what}");
                assert_eq!(
                    built(),
                    vec![Built {
                        base: 0,
                        pc: 0,
                        op: op.abi(),
                        a: 0,
                        b: 1,
                        work: 3,
                    }],
                    "{what}: the runtime was handed the instruction, whole"
                );
                let after: Vec<u64> = (0..16).map(|word| heap.get(at + word)).collect();
                assert_eq!(before, after, "{what}: emitted code wrote nothing first");
                assert_eq!(frame[2], 0, "{what}: the instruction after it ran");
            }
        }
    }
    forget_built();
}

/// A byte `run-store` into a byte run: the byte blended into its word at every
/// offset inside a word that is easy to get wrong, the neighbours left as they
/// were, and nothing handed to the runtime.
pub fn a_byte_store_blends_the_byte<A: Arm>() {
    let at = HEAP_CHUNK_WORDS + 33;
    for index in [0u64, 7, 8, 15] {
        for value in [0u64, 0x5C, 255] {
            forget_built();
            let what = format!("{value} at byte {index}");
            let held = storing_a_byte();
            let mut heap = Heap::new(2);
            a_byte_buffer(&mut heap, at, 0, 16);
            let run = heap.addr(at + 8);
            let mut frame = vec![run, index, value, UNWRITTEN];
            let answer = run_over::<A>(&held, &mut frame, 0, &heap);
            assert_eq!(answer.outcome, Outcome::Returned, "{what}");
            assert!(built().is_empty(), "{what}: {:?}", built());
            for word in 0..2 {
                let expected = if word == index / 8 {
                    let shift = (index % 8) * 8;
                    (0xAAAA_AAAA_AAAA_AAAAu64 & !(0xFF << shift)) | (value << shift)
                } else {
                    0xAAAA_AAAA_AAAA_AAAA
                };
                assert_eq!(heap.get(at + 8 + 1 + word), expected, "{what}: word {word}");
            }
            assert_eq!(heap.get(at + 1), 0, "{what}: a store publishes nothing");
            assert_eq!(frame[3], 0, "{what}: the `unit` after it ran");
        }
    }
    forget_built();
}

/// The cold paths of a byte `run-store`: an offset at the end of the run, a
/// negative one, a value of `256` and of `-1`, and an object that is not a byte
/// run. Each goes over as [`GrowableOp::StoreBytes`] with the run's slot and the
/// offset's, and nothing is written first.
pub fn every_cold_path_of_a_byte_store_goes_to_the_runtime<A: Arm>() {
    let at = HEAP_CHUNK_WORDS + 33;
    for (why, index, value, owner_not_run) in [
        ("the offset is the run's length", 16u64, 7u64, false),
        ("the offset is negative", u64::MAX, 7, false),
        ("the value is 256", 0, 256, false),
        ("the value is negative", 0, u64::MAX, false),
        ("the object is a byte buffer, not its run", 0, 7, true),
    ] {
        forget_built();
        let held = storing_a_byte();
        let mut heap = Heap::new(2);
        let owner = a_byte_buffer(&mut heap, at, 0, 16);
        let run = if owner_not_run {
            owner
        } else {
            heap.addr(at + 8)
        };
        let before: Vec<u64> = (0..16).map(|word| heap.get(at + word)).collect();
        let mut frame = vec![run, index, value, UNWRITTEN];
        let answer = run_over::<A>(&held, &mut frame, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Returned, "{why}");
        assert_eq!(
            built(),
            vec![Built {
                base: 0,
                pc: 0,
                op: GrowableOp::StoreBytes.abi(),
                a: 0,
                b: 1,
                work: 3,
            }],
            "{why}: the runtime was handed the store, whole"
        );
        let after: Vec<u64> = (0..16).map(|word| heap.get(at + word)).collect();
        assert_eq!(before, after, "{why}: emitted code wrote nothing first");
        assert_eq!(frame[3], 0, "{why}: the instruction after it ran");
    }
    forget_built();
}

/// Each of the three refuses a null owner or run where it is read, as
/// `null_object()`, and a helper that raised leaves with that outcome.
pub fn a_window_instruction_refuses_null_and_leaves_when_refused<A: Arm>() {
    let held = [
        (reserving(Storage::PackedBytes, false), 3usize),
        (reserving(Storage::Words(INT), true), 3),
        (storing_a_byte(), 4),
    ];
    for (program, width) in &held {
        forget_built();
        let heap = Heap::new(2);
        let mut frame = vec![0u64; *width];
        frame[width - 1] = UNWRITTEN;
        let answer = run_over::<A>(program, &mut frame, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Raised);
        assert_eq!(answer.raise, Some(Raise::NullObject));
        assert_eq!(answer.raise_pc, 0);
        assert!(built().is_empty());
    }
    let at = HEAP_CHUNK_WORDS + 33;
    for outcome in [Outcome::Raised, Outcome::Stopped] {
        forget_built();
        built_answers(&[outcome]);
        let mut heap = Heap::new(2);
        let owner = a_byte_buffer(&mut heap, at, 10, 16);
        // Past the room, so the ensure is cold and the runtime answers.
        let mut frame = vec![owner, 7, UNWRITTEN];
        let answer = run_over::<A>(
            &reserving(Storage::PackedBytes, false),
            &mut frame,
            0,
            &heap,
        );
        assert_eq!(answer.outcome, outcome);
        assert_eq!(built().len(), 1);
        assert_eq!(frame[2], UNWRITTEN, "nothing after the ensure ran");
    }
    forget_built();
}

/// A whole push window, as a program will emit it: over a byte buffer, a vector
/// of one-word elements and a vector of two-word ones, with room, the unit lands
/// at the length and the length is one more — and nothing at all is handed to the
/// runtime, because every instruction of the window is emitted.
pub fn a_push_window_with_room_writes_the_unit_and_commits_it<A: Arm>() {
    let at = HEAP_CHUNK_WORDS + 33;
    for (storage, stride) in [
        (Storage::PackedBytes, 1u64),
        (Storage::Words(INT), 1),
        (Storage::Words(PAIR), 2),
    ] {
        forget_built();
        let what = format!("{storage:?}");
        let held = a_push_window(storage);
        let mut heap = Heap::new(2);
        let owner = match storage {
            Storage::PackedBytes => a_byte_buffer(&mut heap, at, 9, 16),
            Storage::Words(elem) => {
                let vector = if elem == PAIR { PAIR_VECTOR } else { VECTOR };
                a_vector(&mut heap, at, vector, 3, 8)
            }
        };
        let mut frame = vec![owner, UNWRITTEN, UNWRITTEN, UNWRITTEN];
        frame.extend((0..stride).map(|word| 0x41 + word));
        frame.push(UNWRITTEN);
        frame.push(UNWRITTEN);
        let answer = run_with_fields::<A>(&held, &mut frame, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Returned, "{what}");
        assert!(built().is_empty(), "{what}: {:?}", built());
        match storage {
            Storage::PackedBytes => {
                assert_eq!(heap.get(at + 1), 10, "{what}: the length");
                let word = heap.get(at + 8 + 2);
                assert_eq!((word >> 8) & 0xFF, 0x41, "{what}: byte 9");
                assert_eq!(word & 0xFF, 0xAA, "{what}: byte 8, untouched");
            }
            Storage::Words(_) => {
                assert_eq!(heap.get(at + 1), 4, "{what}: the length");
                for word in 0..stride {
                    assert_eq!(
                        heap.get(at + 8 + 1 + 3 * stride + word),
                        0x41 + word,
                        "{what}: element word {word}"
                    );
                }
            }
        }
        assert_eq!(frame[3], 0, "{what}: the store slot was cleared");
    }
    forget_built();
}

// --- ADR 0062's windows, as one fast path ------------------------------------

/// [`a_push_window`] with its first constant moved in front of the head: the same
/// instructions doing the same thing, which `cove_ir::legalize` does not recognise
/// — a push's count is written just before its ensure — so each arm emits it one
/// row at a time. What a case compares a recognised window with.
pub fn a_push_window_unrecognised(storage: Storage) -> Program {
    let mut held = a_push_window(storage);
    let code = &mut held.functions[0].code;
    let constant = code.remove(1);
    code.insert(0, constant);
    assert!(
        cove_ir::legalize::windows(&held, &held.functions[0]).is_empty(),
        "the rows are no window"
    );
    held
}

/// A whole append window over `storage`: the length, the ensure of a count the
/// frame holds, the store read, a nought offset, one `run-copy` at the length, a
/// clear of the store, and the commit of the same count.
///
/// Slot 0 is the owner, slot 1 the length, slot 2 the count, slot 3 the store,
/// slot 4 the run copied from, slot 5 the offset in it and slot 6 the answer; the
/// copy's operands are `ArgsId(1)`.
pub fn an_append_window(storage: Storage) -> Program {
    let arg = |slot, layout| Arg { slot, layout };
    let held = program_with_args(
        function(
            vec![
                Repr::Ref,
                Repr::Int,
                Repr::Int,
                Repr::Ref,
                Repr::Ref,
                Repr::Int,
                Repr::Unit,
            ],
            UNIT,
            vec![
                Inst::LoadField {
                    dst: 1,
                    obj: 0,
                    at: 0,
                    layout: INT,
                },
                Inst::GrowableEnsure {
                    owner: 0,
                    additional: 2,
                    storage,
                },
                Inst::LoadField {
                    dst: 3,
                    obj: 0,
                    at: 1,
                    layout: REF,
                },
                Inst::Int { dst: 5, value: 0 },
                Inst::RunCopy {
                    args: ArgsId(1),
                    storage,
                },
                Inst::Clear {
                    slot: 3,
                    layout: REF,
                },
                Inst::GrowableCommit {
                    owner: 0,
                    count: 2,
                    storage,
                },
                Inst::Unit { dst: 6 },
                Inst::Return { src: 6 },
            ],
        ),
        vec![
            arg(3, REF),
            arg(1, INT),
            arg(4, REF),
            arg(5, INT),
            arg(2, INT),
        ],
    );
    assert_eq!(
        cove_ir::legalize::windows(&held, &held.functions[0]).len(),
        1,
        "the rows are one window"
    );
    held
}

/// [`an_append_window`] with the run-copy's length operand handed a second slot
/// holding the same value, and a `copy` in front of the head that writes it: the
/// same semantics, which `cove_ir::legalize` does not recognise, so each arm
/// emits it one row at a time. What a case compares a recognised append window
/// with.
///
/// The refusal is `recognize`'s `n.slot != count` test, and nothing else. That
/// test asks that a run-copy's length *be* the slot the ensure reserved and the
/// commit publishes, not merely equal it at run time — a window's one capacity
/// question is only sound if the three rows name one count. The assertion
/// between the two edits below is what pins the reason: with the `copy` row
/// added and the operand still slot 2 the window is recognised, because a row in
/// front of the head is not a candidate window's.
pub fn an_append_window_unrecognised(storage: Storage) -> Program {
    let mut held = an_append_window(storage);
    let function = &mut held.functions[0];
    let doubled = function.reprs.len() as Slot;
    function.reprs.push(Repr::Int);
    function.refs = RefMap::of(&function.reprs);
    function.code.insert(
        0,
        Inst::Copy {
            dst: doubled,
            src: 2,
            layout: INT,
        },
    );
    function.spans.insert(0, span());
    assert_eq!(
        cove_ir::legalize::windows(&held, &held.functions[0]).len(),
        1,
        "a row in front of the head is not the window's"
    );
    held.args[ArgsId(1).index()][4].slot = doubled;
    assert!(
        cove_ir::legalize::windows(&held, &held.functions[0]).is_empty(),
        "the rows are no window"
    );
    held
}

/// The frame [`a_push_window`] is entered with: the owner, three unwritten slots,
/// the unit's `stride` words, and an unwritten second count and answer.
fn push_window_frame(owner: u64, stride: u64, unit: u64) -> Vec<u64> {
    let mut frame = vec![owner, UNWRITTEN, UNWRITTEN, UNWRITTEN];
    frame.extend((0..stride).map(|word| unit + word));
    frame.push(UNWRITTEN);
    frame.push(UNWRITTEN);
    frame
}

/// The owner a push window over `storage` is tested on: a byte buffer, or the
/// vector of the element, of `len` units in a store of `capacity`.
fn push_window_owner(heap: &mut Heap, at: u64, storage: Storage, len: u32, capacity: u32) -> u64 {
    match storage {
        Storage::PackedBytes => a_byte_buffer(heap, at, u64::from(len), capacity),
        Storage::Words(elem) => {
            let vector = if elem == PAIR { PAIR_VECTOR } else { VECTOR };
            a_vector(heap, at, vector, len, capacity)
        }
    }
}

/// **Every way a push window leaves its fast path is one of its rows**: the
/// ensure's cold half at the ensure's pc with the owner's slot and the count's,
/// the head's field read at the head's pc, and a byte store's cold half at the
/// write's pc with the store slot and the length slot. Nothing is ever handed
/// over as a whole push, and the frame holds exactly what the rows before the
/// hand-over wrote.
///
/// The one-block function charges its ten rows at entry, so the first hand-over
/// publishes ten.
pub fn every_cold_path_of_a_push_window_is_one_of_its_rows<A: Arm>() {
    let at = HEAP_CHUNK_WORDS + 33;
    for (storage, stride) in [
        (Storage::PackedBytes, 1u64),
        (Storage::Words(INT), 1),
        (Storage::Words(PAIR), 2),
    ] {
        let ensure = match storage {
            Storage::PackedBytes => GrowableOp::EnsureBytes,
            Storage::Words(_) => GrowableOp::EnsureWords,
        };
        let handed = |work| Built {
            base: 0,
            pc: 2,
            op: ensure.abi(),
            a: 0,
            b: 2,
            work,
        };
        let held = a_push_window(storage);
        let value = if storage == Storage::PackedBytes {
            0x41
        } else {
            0x41_0000
        };

        // No room, and the ensure refuses: nothing past the count was written.
        for consumed in [false, true] {
            forget_built();
            forget_fielded();
            built_answers(&[Outcome::Raised]);
            let what = format!("{storage:?}, consumed {consumed}");
            let mut heap = Heap::new(2);
            let owner = push_window_owner(&mut heap, at, storage, 8, 8);
            if consumed {
                heap.set(at + 2, 0);
            }
            let before: Vec<u64> = (0..48).map(|word| heap.get(at + word)).collect();
            let mut frame = push_window_frame(owner, stride, value);
            let answer = run_with_fields::<A>(&held, &mut frame, 0, &heap);
            assert_eq!(answer.outcome, Outcome::Raised, "{what}");
            assert_eq!(built(), vec![handed(10)], "{what}");
            assert!(fielded().is_empty(), "{what}: the head was emitted");
            assert_eq!(frame[1], 8, "{what}: the length the head read");
            assert_eq!(frame[2], 1, "{what}: the count");
            assert_eq!(frame[3], UNWRITTEN, "{what}: the store row was not reached");
            let after: Vec<u64> = (0..48).map(|word| heap.get(at + word)).collect();
            assert_eq!(before, after, "{what}: nothing was written into the heap");
        }

        // An ensure that returns having made room: the store row is rejoined,
        // and the unit lands at the length with no second hand-over.
        forget_built();
        let what = format!("{storage:?}, grown by the ensure");
        let mut heap = Heap::new(2);
        let owner = push_window_owner(&mut heap, at, storage, 8, 8);
        room_on_ensure(16);
        let mut frame = push_window_frame(owner, stride, value);
        let answer = run_with_fields::<A>(&held, &mut frame, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Returned, "{what}");
        assert_eq!(built(), vec![handed(10)], "{what}");
        assert_eq!(heap.get(at + 1), 9, "{what}: the length");
        match storage {
            Storage::PackedBytes => {
                assert_eq!(heap.get(at + 8 + 2) & 0xFF, 0x41, "{what}: byte 8");
            }
            Storage::Words(_) => {
                for word in 0..stride {
                    let into = at + 8 + 1 + 8 * stride + word;
                    assert_eq!(heap.get(into), value + word, "{what}: word {word}");
                }
            }
        }
        let count = 4 + stride as usize;
        assert_eq!(
            (frame[1], frame[2], frame[3], frame[count]),
            (8, 1, 0, 1),
            "{what}: the length, the count, the cleared store and the second count"
        );

        // An ensure that returns having made no room is asked again, through its
        // own helper, which is a safepoint every time.
        forget_built();
        built_answers(&[Outcome::Returned, Outcome::Raised]);
        let mut heap = Heap::new(2);
        let owner = push_window_owner(&mut heap, at, storage, 8, 8);
        heap.set(at + 2, 0);
        let mut frame = push_window_frame(owner, stride, value);
        let answer = run_with_fields::<A>(&held, &mut frame, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Raised, "{storage:?}, asked again");
        assert_eq!(
            built(),
            vec![handed(10), handed(0)],
            "{storage:?}, asked again"
        );

        // An owner of another family: the head is the field helper's, and the
        // ensure is what refuses it.
        forget_built();
        forget_fielded();
        built_answers(&[Outcome::Raised]);
        let what = format!("{storage:?}, another family");
        let mut heap = Heap::new(2);
        let other = match storage {
            Storage::Words(elem) if elem == INT => Storage::Words(PAIR),
            _ => Storage::Words(INT),
        };
        let owner = push_window_owner(&mut heap, at, other, 3, 8);
        let mut frame = push_window_frame(owner, stride, value);
        let answer = run_with_fields::<A>(&held, &mut frame, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Raised, "{what}");
        assert_eq!(
            fielded(),
            vec![Fielded {
                pc: 0,
                addr: owner,
                at: 0,
                width: 1,
                other: SEGMENT_ORIGIN + 1,
            }],
            "{what}: the head, whole"
        );
        assert_eq!(built(), vec![handed(10)], "{what}");
        assert_eq!(frame[1], owner * 1000, "{what}: what the field helper read");
        assert_eq!(frame[2], 1, "{what}: the count");

        // A null owner, refused at the head as its `load-field` refuses it.
        forget_built();
        forget_fielded();
        let heap = Heap::new(2);
        let mut frame = push_window_frame(0, stride, value);
        let answer = run_with_fields::<A>(&held, &mut frame, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Raised, "{storage:?}, null");
        assert_eq!(answer.raise, Some(Raise::NullObject), "{storage:?}, null");
        assert_eq!(answer.raise_pc, 0, "{storage:?}, null");
        assert!(
            built().is_empty() && fielded().is_empty(),
            "{storage:?}, null"
        );
        assert_eq!(frame[1], UNWRITTEN, "{storage:?}, null");
    }

    // A byte push's own two questions, each the byte store row's cold half with
    // the store already in its slot: a value that is not a byte, and a store
    // that is not a byte run.
    let held = a_push_window(Storage::PackedBytes);
    for (why, value, other_store, answers) in [
        ("256, refused", 256u64, false, vec![Outcome::Raised]),
        ("-1, answered", u64::MAX, false, vec![]),
        (
            "a store of another family",
            0x41,
            true,
            vec![Outcome::Raised],
        ),
    ] {
        forget_built();
        built_answers(&answers);
        let mut heap = Heap::new(2);
        let owner = a_byte_buffer(&mut heap, at, 3, 16);
        if other_store {
            heap.object(at + 8, STORE, 16);
        }
        let store = heap.get(at + 2);
        let mut frame = push_window_frame(owner, 1, value);
        let answer = run_with_fields::<A>(&held, &mut frame, 0, &heap);
        assert_eq!(
            built(),
            vec![Built {
                base: 0,
                pc: 4,
                op: GrowableOp::StoreBytes.abi(),
                a: 3,
                b: 1,
                work: 10,
            }],
            "{why}"
        );
        assert_eq!(frame[1], 3, "{why}: the length");
        match answers.is_empty() {
            false => {
                assert_eq!(answer.outcome, Outcome::Raised, "{why}");
                assert_eq!(frame[3], store, "{why}: the store row ran");
                assert_eq!(heap.get(at + 1), 3, "{why}: nothing committed");
            }
            // The double wrote nothing and answered: the rows after the store
            // ran, as they would have.
            true => {
                assert_eq!(answer.outcome, Outcome::Returned, "{why}");
                assert_eq!((frame[3], frame[5]), (0, 1), "{why}: the rows after it");
                assert_eq!(heap.get(at + 1), 4, "{why}: committed");
            }
        }
    }
    forget_built();
    forget_fielded();
}

/// **An append window is its ensure, its copy and its commit**: with room, the
/// copy is the one hand-over — at the write's pc, with the row's operands — and
/// the length is the commit's; without it, the ensure's cold half comes first,
/// and a returned one that made room is followed by the same copy.
pub fn an_append_window_is_an_ensure_a_copy_and_a_commit<A: Arm>() {
    let at = HEAP_CHUNK_WORDS + 33;
    for storage in [Storage::PackedBytes, Storage::Words(INT)] {
        let (ensure, kind, elem) = match storage {
            Storage::PackedBytes => (GrowableOp::EnsureBytes, RunOp::CopyBytes, 0),
            Storage::Words(elem) => (GrowableOp::EnsureWords, RunOp::CopyWords, elem.0),
        };
        let copy = |work| Copied {
            base: 0,
            pc: 4,
            args: 1,
            kind: kind.abi(),
            elem,
            work,
        };
        let handed = Built {
            base: 0,
            pc: 1,
            op: ensure.abi(),
            a: 0,
            b: 2,
            work: 9,
        };
        let held = an_append_window(storage);
        let owner = |heap: &mut Heap, len, capacity| match storage {
            Storage::PackedBytes => a_byte_buffer(heap, at, u64::from(len), capacity),
            Storage::Words(_) => a_vector(heap, at, VECTOR, len, capacity),
        };
        for (why, len, count, room, answers) in [
            ("with room", 3u32, 4u64, None, vec![]),
            ("of nothing", 8, 0, None, vec![]),
            ("past the room, refused", 6, 4, None, vec![Outcome::Raised]),
            (
                "of a negative count",
                3,
                u64::MAX,
                None,
                vec![Outcome::Raised],
            ),
            ("past the room, grown", 6, 4, Some(16), vec![]),
        ] {
            forget_built();
            forget_copied();
            built_answers(&answers);
            if let Some(capacity) = room {
                room_on_ensure(capacity);
            }
            let what = format!("{storage:?} {why}");
            let mut heap = Heap::new(2);
            let owner = owner(&mut heap, len, 8);
            let mut frame = vec![
                owner, UNWRITTEN, count, UNWRITTEN, 0x77, UNWRITTEN, UNWRITTEN,
            ];
            let answer = run_over::<A>(&held, &mut frame, 0, &heap);
            assert_eq!(frame[1], u64::from(len), "{what}: the length the head read");
            let fits = count <= u64::from(8 - len);
            match (fits, room, answers.is_empty()) {
                (true, _, _) => {
                    assert_eq!(answer.outcome, Outcome::Returned, "{what}");
                    assert!(built().is_empty(), "{what}: {:?}", built());
                    assert_eq!(copied(), vec![copy(9)], "{what}");
                    assert_eq!(heap.get(at + 1), u64::from(len) + count, "{what}");
                    assert_eq!((frame[3], frame[5]), (0, 0), "{what}: the store and z");
                }
                (false, Some(_), _) => {
                    assert_eq!(answer.outcome, Outcome::Returned, "{what}");
                    assert_eq!(built(), vec![handed], "{what}");
                    assert_eq!(copied(), vec![copy(0)], "{what}");
                    assert_eq!(heap.get(at + 1), u64::from(len) + count, "{what}");
                }
                (false, None, _) => {
                    assert_eq!(answer.outcome, Outcome::Raised, "{what}");
                    assert_eq!(built(), vec![handed], "{what}");
                    assert!(copied().is_empty(), "{what}");
                    assert_eq!(frame[3], UNWRITTEN, "{what}: the store row was not reached");
                    assert_eq!(heap.get(at + 1), u64::from(len), "{what}");
                }
            }
        }
        forget_built();
        forget_copied();
        let heap = Heap::new(2);
        let mut frame = vec![0, UNWRITTEN, 1, UNWRITTEN, 0x77, UNWRITTEN, UNWRITTEN];
        let answer = run_over::<A>(&held, &mut frame, 0, &heap);
        assert_eq!(answer.raise, Some(Raise::NullObject), "{storage:?}, null");
        assert_eq!(answer.raise_pc, 0, "{storage:?}, null");
        assert!(
            built().is_empty() && copied().is_empty(),
            "{storage:?}, null"
        );
    }
    forget_built();
    forget_copied();
}

/// **A window is less machine code than its rows.**
///
/// Two compilations of the same append or push — the window as ADR 0062 lowers
/// it, and the same rows unrecognised — measured in bytes of machine code and
/// printed, so that a change to the fast path shows its cost here. What is held
/// is the ordering that makes the window worth having: fewer bytes than its rows
/// emitted one at a time.
///
/// It is held as a *direction* and not as figures, so that it is not a tripwire
/// every code-generator change has to re-baseline. But the size of the effect is
/// half of what it says, and a direction does not carry it, so here it is —
/// whole-function bytes on the template arm, both sides over the same prologue
/// and epilogue:
///
/// | pattern | window | rows | difference |
/// | --- | ---: | ---: | ---: |
/// | `push.words`, `Vector<Int>` | 997 | 2,285 | +1,288 |
/// | `push.words`, `Vector<Pair>` | 1,081 | 2,369 | +1,288 |
/// | `push.byte` | 1,138 | 2,426 | +1,288 |
/// | `append.bytes` | 1,424 | 2,036 | +612 |
/// | `append.words` | 1,426 | 2,036 | +610 |
///
/// Sixteen bytes of each append's row figure are the `copy` row
/// [`an_append_window_unrecognised`] adds, which is the price of having an
/// unrecognised append to compare against at all: the same program with that row
/// and the operand left alone is 1,440 B and still one window. The other 596 B
/// and 594 B are the guards — an append's rows ask four capacity and family
/// questions where the window asks one. `Emit::window` is where that is written down, and
/// where it is the argument for keeping this code generator.
///
/// The five cases are why this is not three: an append is the pattern whose rows
/// are *least* larger, so a push-only case would have said the effect was twice
/// what it is on the shape that matters most to cq.
///
/// There was a third compilation until ADR 0062's last stage, the composite
/// `growable-push` this replaced, and the window was held to no more than twice
/// it. That baseline went with the instruction; #417 and #418 are where the
/// numbers it gave are recorded.
pub fn a_window_is_less_code_than_its_rows<A: Arm>() {
    use cove_ir::legalize::windows;
    let size = |program: &Program| {
        let mut jit = A::new(helpers());
        let compiled = jit
            .compile(program, FunctionId(0))
            .expect("the function is inside the slice");
        A::code_bytes(compiled)
    };
    for (what, storage, appends) in [
        ("Vector<Int>.push", Storage::Words(INT), false),
        ("Vector<Pair>.push", Storage::Words(PAIR), false),
        ("appendByte", Storage::PackedBytes, false),
        ("appendSlice", Storage::PackedBytes, true),
        ("keyed extend", Storage::Words(INT), true),
    ] {
        let (held, unrecognised) = match appends {
            false => (a_push_window(storage), a_push_window_unrecognised(storage)),
            true => (
                an_append_window(storage),
                an_append_window_unrecognised(storage),
            ),
        };
        // Without these two the case could pass by comparing two compilations
        // of the same thing: a perturbation that stopped unrecognising the rows
        // would make both sides a window and both sides equal.
        assert_eq!(
            windows(&held, &held.functions[0]).len(),
            1,
            "{what}: the window is one window"
        );
        assert!(
            windows(&unrecognised, &unrecognised.functions[0]).is_empty(),
            "{what}: the rows are no window"
        );
        let window = size(&held);
        let rows = size(&unrecognised);
        println!("{what}: window {window} bytes, rows {rows} bytes");
        assert!(
            window < rows,
            "{what}: {window} against {rows} for the rows"
        );
    }
}

/// **A window's machine code is charged to its own pattern, and to nothing
/// else.**
///
/// [Issue #423](https://github.com/myuon/cove/issues/423)'s attribution, held to
/// the three things that make it worth having:
///
/// - a function with a window charges **one site** to that window's pattern and
///   none to the other three, and the same function with its rows unrecognised
///   charges nothing anywhere — so the count follows `cove_ir::legalize` and not
///   the shape of the body;
/// - where the bytes are attributed they are **positive and no more than the
///   whole function**, which is the arithmetic a reader of the report assumes;
/// - the byte difference between the two compilations is **at least** what the
///   attributed window is smaller by. A window that got cheaper while the
///   function did not would mean bytes were being charged to a pattern that did
///   not emit them.
pub fn a_windows_machine_code_is_charged_to_its_pattern<A: Arm>() {
    use cove_ir::legalize::Pattern;
    let compiled = |program: &Program| {
        let mut jit = A::new(helpers());
        jit.compile(program, FunctionId(0))
            .expect("the function is inside the slice")
    };
    for (what, storage, pattern) in [
        ("Vector<Int>.push", Storage::Words(INT), Pattern::PushWords),
        ("appendByte", Storage::PackedBytes, Pattern::PushByte),
        ("appendSlice", Storage::PackedBytes, Pattern::AppendBytes),
        ("keyed extend", Storage::Words(INT), Pattern::AppendWords),
    ] {
        let held = match pattern {
            Pattern::PushWords | Pattern::PushByte => a_push_window(storage),
            _ => an_append_window(storage),
        };
        let handle = compiled(&held);
        let code = A::window_code(handle);
        let wanted: Vec<u64> = Pattern::ALL
            .iter()
            .map(|&each| u64::from(each == pattern))
            .collect();
        assert_eq!(code.sites.to_vec(), wanted, "{what}: one site, its own");
        assert_eq!(code.total_sites(), 1, "{what}");
        assert_eq!(
            code.bytes.is_some(),
            A::ATTRIBUTES_WINDOWS,
            "{what}: whether the bytes are attributed is the arm's, not the program's"
        );
        let Some(bytes) = code.bytes else {
            continue;
        };
        let whole = u64::from(A::code_bytes(handle));
        let charged = bytes[pattern.index()];
        println!("{what}: {charged} of {whole} byte(s) are the window");
        assert!(charged > 0, "{what}: a window that emitted nothing");
        assert!(
            charged <= whole,
            "{what}: {charged} charged out of a function of {whole}"
        );
        assert_eq!(
            bytes.iter().sum::<u64>(),
            charged,
            "{what}: another pattern was charged"
        );
    }

    // And a body whose rows `legalize` does not recognise charges nothing,
    // because there is no window in it to charge.
    for storage in [Storage::Words(INT), Storage::PackedBytes] {
        let code = A::window_code(compiled(&a_push_window_unrecognised(storage)));
        assert_eq!(code.total_sites(), 0, "{storage:?}: unrecognised rows");
        assert_eq!(
            code.bytes,
            Some([0; 4]),
            "{storage:?}: a function with no window has nought bytes of window,              which is a measurement and not an absence"
        );
    }
}

// --- Vector.freeze ---------------------------------------------------------

/// One `Vector.freeze() -> Array<T>`, answering into `dst`: what
/// `std.vector.freeze` is once the lowering has expanded it — a word
/// `run-finish` of `stride`-wide elements into the `Array` of them.
pub fn freezing(stride: u32) -> Program {
    let (elem, target) = if stride == 2 {
        (PAIR, ARRAY_PAIR)
    } else {
        (INT, ARRAY_INT)
    };
    program(function(
        vec![Repr::Ref, Repr::Ref],
        REF,
        vec![
            Inst::RunFinish {
                dst: 1,
                owner: 0,
                target,
                validation: Validation::None,
                storage: Storage::Words(elem),
            },
            Inst::Return { src: 1 },
        ],
    ))
}

/// `Machine::finish_words`: `Memory::relabel` turning the store
/// into the `Array<T>` it already holds, in place — nothing handed to the
/// runtime, the answer aliases the store, and the elements read back exactly
/// as a `Vector.get` would have answered them.
///
/// Four shapes of vector, each named by what it says about the lowering:
/// spare capacity, so the free block is written; exactly full, so `spare ==
/// 0` and it is not; empty; and a two-word element, the stride case
/// `Emit::vector_push`'s own note explains.
pub fn a_freeze_relabels_the_store_in_place<A: Arm>() {
    let cases: [(LayoutId, LayoutId, u32, u32, u32); 4] = [
        (VECTOR, ARRAY_INT, 1, 2, 5),
        (VECTOR, ARRAY_INT, 1, 4, 4),
        (VECTOR, ARRAY_INT, 1, 0, 3),
        (PAIR_VECTOR, ARRAY_PAIR, 2, 2, 3),
    ];
    for (vector, array, stride, len, capacity) in cases {
        forget_built();
        let held = freezing(stride);
        let at = HEAP_CHUNK_WORDS + 33;
        let mut heap = Heap::new(2);
        let header = a_vector(&mut heap, at, vector, len, capacity);
        let store = heap.addr(at + 8);
        for word in 0..(len * stride) {
            heap.set(at + 8 + 1 + u64::from(word), 900 + u64::from(word));
        }
        let mut words = vec![header, UNWRITTEN];
        let answer = run_over::<A>(&held, &mut words, 0, &heap);
        assert_eq!(
            answer.outcome,
            Outcome::Returned,
            "{len}/{capacity} at stride {stride}"
        );
        assert!(
            built().is_empty(),
            "relabel is O(1) and emitted whole: {:?}",
            built()
        );

        assert_eq!(
            words[1], store,
            "the answer aliases the store: `relabel` moves nothing"
        );

        let expect_header = (u64::from(array.0) << 32) | u64::from(len);
        assert_eq!(
            heap.get(at + 8),
            expect_header,
            "the store's own header now names the array and its length"
        );

        for word in 0..(len * stride) {
            assert_eq!(
                heap.get(at + 8 + 1 + u64::from(word)),
                900 + u64::from(word),
                "element word {word} reads back through the array exactly as \
                 it did through the vector"
            );
        }

        let spare = (capacity - len) * stride;
        if spare > 0 {
            assert_eq!(
                heap.get(at + 8 + 1 + u64::from(len * stride)),
                u64::from(spare - 1),
                "a free block of `spare - 1` payload words, layout `FREE` (0)"
            );
        }

        assert_eq!(heap.get(at + 1), 0, "the vector's own length word, cleared");
        assert_eq!(heap.get(at + 2), 0, "the vector's own store word, cleared");
    }
    forget_built();
}

/// A keyed finish — `core.setFinish` and `core.mapFinish`, #378 P4-5 — is the
/// same emitted relabel as a freeze, into the `Set` or the `Map` the store's
/// unit is: nothing handed to the runtime, the answer aliases the store, the
/// header names the keyed layout at the logical length and the spare room is a
/// free block.
pub fn a_keyed_finish_relabels_the_store_into_a_set_or_a_map<A: Arm>() {
    let cases: [(LayoutId, LayoutId, LayoutId, u32); 2] = [
        (VECTOR, INT, SET_INT, 1),
        (ENTRY_VECTOR, ENTRY_INT, MAP_INT, 2),
    ];
    for (vector, elem, target, stride) in cases {
        forget_built();
        let held = program(function(
            vec![Repr::Ref, Repr::Ref],
            REF,
            vec![
                Inst::RunFinish {
                    dst: 1,
                    owner: 0,
                    target,
                    validation: Validation::None,
                    storage: Storage::Words(elem),
                },
                Inst::Return { src: 1 },
            ],
        ));
        let (len, capacity) = (2u32, 5u32);
        let at = HEAP_CHUNK_WORDS + 33;
        let mut heap = Heap::new(2);
        let header = a_vector(&mut heap, at, vector, len, capacity);
        let store = heap.addr(at + 8);
        for word in 0..(len * stride) {
            heap.set(at + 8 + 1 + u64::from(word), 900 + u64::from(word));
        }
        let mut words = vec![header, UNWRITTEN];
        let answer = run_over::<A>(&held, &mut words, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Returned, "stride {stride}");
        assert!(built().is_empty(), "emitted whole: {:?}", built());
        assert_eq!(words[1], store, "the answer aliases the store");
        assert_eq!(
            heap.get(at + 8),
            (u64::from(target.0) << 32) | u64::from(len),
            "the store's header names the keyed run and its length"
        );
        for word in 0..(len * stride) {
            assert_eq!(
                heap.get(at + 8 + 1 + u64::from(word)),
                900 + u64::from(word)
            );
        }
        assert_eq!(
            heap.get(at + 8 + 1 + u64::from(len * stride)),
            u64::from((capacity - len) * stride - 1),
            "a free block of the spare room"
        );
        assert_eq!(heap.get(at + 1), 0, "the vector's length word, cleared");
        assert_eq!(heap.get(at + 2), 0, "the vector's store word, cleared");
    }
    forget_built();
}

/// The two cold paths of `Vector.freeze`, each handed to the runtime whole.
///
/// A store word of nought — a second finish — and an owner whose object is not
/// the vector the element layout implies: `cove_native`'s `WordFinish`, for a
/// push's reasons exactly. Each sentence is the runtime's, which this crate
/// cannot build, so the assertion is that emitted code **did not try**: the
/// finish went over as [`GrowableOp::FinishWords`] and what the runtime
/// answered landed in `dst`.
pub fn every_cold_path_of_a_freeze_goes_to_the_runtime<A: Arm>() {
    let at = HEAP_CHUNK_WORDS + 33;
    let rows: [(&str, Build); 2] = [
        ("`freeze()` consumed the store already", |heap, at| {
            let header = a_vector(heap, at, VECTOR, 0, 4);
            heap.set(at + 2, 0);
            header
        }),
        (
            "the object is not the layout the call site declared",
            |heap, at| a_vector(heap, at, PAIR_VECTOR, 0, 4),
        ),
    ];
    for (why, build) in rows {
        forget_built();
        let held = freezing(1);
        let mut heap = Heap::new(2);
        let header = build(&mut heap, at);
        let mut words = vec![header, UNWRITTEN];
        let answer = run_over::<A>(&held, &mut words, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Returned, "{why}");
        assert_eq!(
            built(),
            vec![Built {
                base: 0,
                pc: 0,
                op: GrowableOp::FinishWords.abi(),
                a: 1,
                b: 0,
                // The helper is a safepoint, so the block's static work — two
                // instructions — went over with the hand-over.
                work: 2,
            }],
            "{why}: the runtime was handed the finish, whole"
        );
        assert_eq!(
            words[1],
            u64::from(GrowableOp::FinishWords.abi()) * 1000 + 1,
            "{why}: the runtime's answer, in `dst`"
        );
    }
    forget_built();
}

/// `Machine::vector_run`'s null refusal — the one refusal of a finish this
/// crate can name, so it is emitted rather than handed over.
pub fn a_freeze_refuses_a_null_receiver<A: Arm>() {
    forget_built();
    let held = freezing(1);
    let heap = Heap::new(2);
    let mut words = vec![0u64, UNWRITTEN];
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Raised);
    assert_eq!(answer.raise, Some(Raise::NullObject));
    assert_eq!(answer.raise_pc, 0);
    assert!(
        built().is_empty(),
        "the null was refused here, not handed over"
    );
    forget_built();
}

/// `encoded.rs`'s `LOAD_ELEM` arm (line 1425), which is `Machine::element` and
/// then a copy at the element layout's stride.
///
/// The element is two words wide, so this is also the assertion that the stride
/// is the *element's* width and not one: `Machine::element` answers
/// `at as u32 * width`.
pub fn a_load_elem_strides_and_bounds_its_index<A: Arm>() {
    let held = program(function(
        vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Int],
        PAIR,
        vec![
            Inst::LoadElem {
                dst: 2,
                obj: 0,
                index: 1,
                layout: PAIR,
            },
            Inst::Return { src: 2 },
        ],
    ));

    // Three two-word elements, laid out so the last of them is in the next
    // chunk: an object that straddles a boundary is the case one chunk would
    // have let through.
    let at = HEAP_CHUNK_WORDS - 4;
    let build = || {
        let mut heap = Heap::new(2);
        heap.object(at, PAIR, 3);
        for word in 0..6u64 {
            heap.set(at + 1 + word, 100 + word);
        }
        heap
    };

    for (index, first, second) in [(0i64, 100u64, 101u64), (1, 102, 103), (2, 104, 105)] {
        let heap = build();
        let mut words = vec![heap.addr(at), index as u64, 0, 0];
        let answer = run_over::<A>(&held, &mut words, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Returned, "index {index}");
        assert_eq!((words[2], words[3]), (first, second), "index {index}");
        assert_eq!(
            answer.returned,
            [first, second, UNWRITTEN, UNWRITTEN],
            "index {index}"
        );
    }

    // `Machine::element`: "index {at} is outside a collection of {len}", for a
    // negative index as much as for a large one.
    for index in [3i64, -1, i64::MIN] {
        let heap = build();
        let mut words = vec![heap.addr(at), index as u64, 0, 0];
        let answer = run_over::<A>(&held, &mut words, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Raised, "index {index}");
        assert_eq!(answer.raise, Some(Raise::IndexOutOfRange));
        assert_eq!(answer.raise_a, index);
        assert_eq!(answer.raise_b, 3);
    }

    let heap = build();
    let mut words = vec![0u64, 0, 0, 0];
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.raise, Some(Raise::NullObject));
}

/// `encoded.rs`'s `STORE_ELEM` arm: `Machine::element`, and then a copy of the
/// element's words *into* the payload.
///
/// The mirror of [`a_load_elem_strides_and_bounds_its_index`], and the same two
/// things are asserted: the stride is the *element's* width — a two-word element
/// at index two lands at payload words four and five, not two and three — and the
/// index is bounded by one unsigned comparison, so a negative index is refused as
/// a large one.
///
/// The element is read back out of the heap rather than out of a slot, which is
/// what makes this a test of the store: a lowering that wrote the right words to
/// the wrong payload offset passes every assertion a frame can make.
pub fn a_store_elem_strides_and_bounds_its_index<A: Arm>() {
    let held = program(function(
        vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::StoreElem {
                obj: 0,
                index: 1,
                src: 2,
                layout: PAIR,
            },
            Inst::Return { src: 1 },
        ],
    ));

    // An object in the second chunk, for the chunk arithmetic, with room for three
    // two-word elements.
    let at = HEAP_CHUNK_WORDS + 41;
    let mut heap = Heap::new(2);
    let addr = heap.object(at, PAIR, 3);
    let mut words = vec![addr, 2, 0x1111, 0x2222];
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        (heap.get(at + 1 + 4), heap.get(at + 1 + 5)),
        (0x1111, 0x2222),
        "element two of a two-word run is payload words four and five"
    );
    for word in 0..4 {
        assert_eq!(
            heap.get(at + 1 + word),
            0,
            "payload word {word} belongs to elements nought and one"
        );
    }

    // `Machine::element`'s `at < 0 || at >= len`, which is one unsigned comparison.
    for index in [3i64, -1] {
        let mut words = vec![addr, index as u64, 0x3333, 0x4444];
        let answer = run_over::<A>(&held, &mut words, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Raised, "index {index}");
        assert_eq!(answer.raise, Some(Raise::IndexOutOfRange));
        assert_eq!(
            answer.raise_a, index,
            "the offending index, as it was given"
        );
        assert_eq!(answer.raise_b, 3, "and the collection's length");
    }

    // And the null receiver, which every reader and writer of an object refuses
    // first.
    let mut words = vec![0u64, 0, 0x5555, 0x6666];
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Raised);
    assert_eq!(answer.raise, Some(Raise::NullObject));
}

/// `encoded.rs`'s `RUN_LOAD_BYTES` arm (line 1456): a payload read, a shift and a mask,
/// eight bytes to a word and least-significant byte first.
pub fn a_byte_at_reads_one_byte_and_bounds_it<A: Arm>() {
    let held = program(function(
        vec![Repr::Ref, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::RunLoad {
                dst: 2,
                run: 0,
                index: 1,
                storage: Storage::PackedBytes,
            },
            Inst::Return { src: 2 },
        ],
    ));

    // Ten bytes over two payload words, and the object placed so the second of
    // them is in the next chunk.
    let bytes: [u8; 10] = [7, 8, 9, 10, 200, 0, 255, 1, 42, 43];
    let at = HEAP_CHUNK_WORDS - 2;
    let build = || {
        let mut heap = Heap::new(2);
        heap.object(at, INT, bytes.len() as u32);
        for (word, run) in bytes.chunks(8).enumerate() {
            let mut packed = 0u64;
            for (byte, value) in run.iter().enumerate() {
                packed |= u64::from(*value) << (byte * 8);
            }
            heap.set(at + 1 + word as u64, packed);
        }
        heap
    };

    for (offset, byte) in bytes.iter().enumerate() {
        let heap = build();
        let mut words = vec![heap.addr(at), offset as u64, 0xdead];
        let answer = run_over::<A>(&held, &mut words, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Returned, "byte {offset}");
        assert_eq!(words[2], u64::from(*byte), "byte {offset}");
    }

    for offset in [10i64, -1] {
        let heap = build();
        let mut words = vec![heap.addr(at), offset as u64, 0];
        let answer = run_over::<A>(&held, &mut words, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Raised, "byte {offset}");
        assert_eq!(answer.raise, Some(Raise::ByteOffset));
        assert_eq!(answer.raise_a, offset);
        assert_eq!(answer.raise_b, 10, "the length, not the last legal offset");
    }
}

/// `encoded.rs`'s `LOAD_FIELD` and `STORE_FIELD` arms (lines 1451/1464), for a
/// *fixed*-payload object.
///
/// `Struct` is one of `NativeCtx::fixed_payload_words`'s `Some` shapes, so
/// `PAIR`'s two words are answered from the table in one load and neither
/// direction ever reaches [`fielded`] — which is the assertion that matters
/// most here, because the whole point of the table is that a fixed shape never
/// leaves compiled code. The third case reads and writes both of `PAIR`'s
/// words in one field, which is [`Inst::Copy`]'s bound on a family that shares
/// nothing else with it.
pub fn a_field_access_reads_and_writes_a_fixed_object<A: Arm>() {
    forget_fielded();
    let held = program(function(
        vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::StoreField {
                obj: 0,
                at: 0,
                src: 1,
                layout: INT,
            },
            Inst::StoreField {
                obj: 0,
                at: 1,
                src: 2,
                layout: INT,
            },
            Inst::LoadField {
                dst: 3,
                obj: 0,
                at: 0,
                layout: INT,
            },
            Inst::LoadField {
                dst: 4,
                obj: 0,
                at: 1,
                layout: INT,
            },
            Inst::Return { src: 3 },
        ],
    ));
    let mut heap = Heap::new(1);
    let addr = heap.object(1, PAIR, 0);
    let mut words = vec![addr, 111, 222, 0, 0];
    let answer = run_with_fields::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[3], 111, "the first word read back");
    assert_eq!(words[4], 222, "the second word read back");
    assert_eq!(heap.get(2), 111, "the object's own payload word 0");
    assert_eq!(heap.get(3), 222, "the object's own payload word 1");
    assert!(
        fielded().is_empty(),
        "a `Struct` is a fixed shape, so the table answered it"
    );

    forget_fielded();
    let held = program(function(
        vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Int, Repr::Int],
        PAIR,
        vec![
            Inst::StoreField {
                obj: 0,
                at: 0,
                src: 1,
                layout: PAIR,
            },
            Inst::LoadField {
                dst: 3,
                obj: 0,
                at: 0,
                layout: PAIR,
            },
            Inst::Return { src: 3 },
        ],
    ));
    let mut heap = Heap::new(1);
    let addr = heap.object(1, PAIR, 0);
    let mut words = vec![addr, 5, 6, 0, 0];
    let answer = run_with_fields::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[3], 5);
    assert_eq!(words[4], 6);
    assert!(fielded().is_empty());
}

/// `Machine::checked`'s null refusal, which is the one precondition a field
/// access answers itself — see this module's note on `Inst::AddrOfField` in
/// `crate::subset` for why the rest of the bound is not named the same way.
///
/// The refusal happens before `NativeCtx::fixed_payload_words` is ever read, so
/// this is the one field-access case that does not need
/// [`run_with_fields`]: a null receiver never reaches the table.
pub fn a_field_access_refuses_a_null_receiver<A: Arm>() {
    let loads = program(function(
        vec![Repr::Ref, Repr::Int],
        INT,
        vec![
            Inst::LoadField {
                dst: 1,
                obj: 0,
                at: 0,
                layout: INT,
            },
            Inst::Return { src: 1 },
        ],
    ));
    let heap = Heap::new(1);
    let mut words = vec![0u64, 0];
    let answer = run_over::<A>(&loads, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Raised);
    assert_eq!(answer.raise, Some(Raise::NullObject));

    let stores = program(function(
        vec![Repr::Ref, Repr::Int, Repr::Unit],
        UNIT,
        vec![
            Inst::StoreField {
                obj: 0,
                at: 0,
                src: 1,
                layout: INT,
            },
            Inst::Unit { dst: 2 },
            Inst::Return { src: 2 },
        ],
    ));
    let mut words = vec![0u64, 5, 0];
    let answer = run_over::<A>(&stores, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Raised);
    assert_eq!(answer.raise, Some(Raise::NullObject));
}

/// A [`Shape::Boxed`](cove_ir::Shape::Boxed) receiver — `Any`, in practice —
/// which is the one *variable*-payload shape a program in this crate's corpus
/// actually reaches.
///
/// `NativeCtx::fixed_payload_words` holds `0` at `BOXED`'s index, so both
/// directions take the cold path unconditionally, however small `at + width`
/// is — this is the case the sentinel exists for. [`fielded`] is the proof:
/// both accesses reached [`crate::abi::FieldLoadFn`]/[`FieldStoreFn`] with the
/// object's own linear address and the field's static `at`/`width`, and the
/// load's synthetic answer — `addr * 1000 + at * 10 + word`, [`field_load`]'s
/// own recognizable number — landed exactly where the instruction said.
pub fn a_field_access_on_a_variable_payload_object_goes_to_the_runtime<A: Arm>() {
    forget_fielded();
    let held = program(function(
        vec![Repr::Ref, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::StoreField {
                obj: 0,
                at: 0,
                src: 1,
                layout: INT,
            },
            Inst::LoadField {
                dst: 2,
                obj: 0,
                at: 0,
                layout: INT,
            },
            Inst::Return { src: 2 },
        ],
    ));
    let mut heap = Heap::new(1);
    let addr = heap.object(1, BOXED, 3);
    let mut words = vec![addr, 99, 0];
    let answer = run_with_fields::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);

    let calls = fielded();
    assert_eq!(
        calls.len(),
        2,
        "both the store and the load went to the runtime"
    );
    assert_eq!(calls[0].addr, addr);
    assert_eq!(calls[0].at, 0);
    assert_eq!(calls[0].width, 1);
    assert_eq!(calls[1].addr, addr);
    assert_eq!(calls[1].at, 0);
    assert_eq!(calls[1].width, 1);
    assert_eq!(
        words[2],
        addr * 1000,
        "the double's synthetic answer landed in the dst slot"
    );
}

/// A field helper that refuses publishes the frame's unpaid work before leaving.
///
/// `native::call` charges `NativeCtx::pending_work` "on every exit — a return, a
/// raise and a stop alike", which is [ADR 0040]'s `S + T` bound: work that is
/// never published is never charged, and the bound is then computed from a number
/// that is short by a whole block.
///
/// This exit is the one that had to be written by hand. Every other hand-over —
/// `builtin_call`, `buffer_op`, `callee_direct` — publishes *before* the call and
/// clears the accumulator, because each of them is a safepoint. A field helper is
/// deliberately not one, since neither `FieldLoadFn` nor `FieldStoreFn` can
/// allocate, so it cannot publish early without putting a charge where there is
/// no safepoint. It publishes on the leaving path instead.
///
/// **Nothing else in this file observes it.** The cold-path cases above assert
/// the hand-over and the answer, both of which are right whether or not the work
/// survives, so the loss was invisible to every correctness test — which is why
/// this one asserts the number rather than the outcome.
///
/// The nine instructions before the access are what make the number non-zero:
/// each arm accumulates its block's static instruction count and hands it over at
/// a safepoint, so a refusal reached with nothing accumulated would pass with the
/// store removed.
///
/// [ADR 0040]: ../../../../docs/adr/0040-a-bound-outlives-its-backend.md
pub fn a_refused_field_access_publishes_its_unpaid_work<A: Arm>() {
    let mut code = Vec::new();
    // Nine instructions of accumulated work, none of which is a safepoint: no
    // backedge, no call, nothing that can allocate. So all of it is still in the
    // accumulator when the field access hands over.
    for step in 0..9 {
        code.push(Inst::Int {
            dst: 1,
            value: step,
        });
    }
    code.push(Inst::LoadField {
        dst: 2,
        obj: 0,
        at: 0,
        layout: INT,
    });
    code.push(Inst::Return { src: 2 });
    // The whole block, because both arms add a block's static instruction count
    // to the accumulator *at its head* rather than one instruction at a time —
    // so the `return` this access never reaches is counted too, and the number
    // an exit must publish is the block's, not the prefix that ran.
    let block = code.len() as u64;
    let held = program(function(vec![Repr::Ref, Repr::Int, Repr::Int], INT, code));

    let mut heap = Heap::new(1);
    // `BOXED` has a nought entry in the table, so this takes the cold path
    // whatever `at` is — and the double is scripted to refuse it.
    let addr = heap.object(1, BOXED, 3);

    forget_fielded();
    fielded_answers(&[Outcome::Raised]);
    let mut words = vec![addr, 0, 0];
    let answer = run_with_fields::<A>(&held, &mut words, 0, &heap);

    assert_eq!(answer.outcome, Outcome::Raised, "the double refused it");
    assert_eq!(
        fielded().len(),
        1,
        "the access reached the helper rather than being emitted"
    );
    // An exit that did not publish answers nought here, which is the defect this
    // case exists for.
    assert_eq!(
        answer.pending_work, block,
        "a refusing field helper left {block} of unpaid work unpublished, so \
         `native::call` would charge none of it"
    );
}

/// `encoded.rs`'s `SWITCH` arm (line 1234):
/// `targets.get(index).unwrap_or(&default)`.
///
/// The word above `u32::MAX` is the case that matters. It is what a switch reads
/// out of a slot the program filled, the machine does not take the lowering's
/// word for what is in it, and one arm reaches its jump table through an `i32`.
pub fn a_switch_takes_its_case_or_the_default<A: Arm>() {
    let held = program_with_tables(
        function(
            vec![Repr::Tag, Repr::Int],
            INT,
            vec![
                Inst::Switch {
                    on: 0,
                    table: TableId(0),
                },
                Inst::Int { dst: 1, value: 11 },
                Inst::Return { src: 1 },
                Inst::Int { dst: 1, value: 22 },
                Inst::Return { src: 1 },
                Inst::Int { dst: 1, value: 33 },
                Inst::Return { src: 1 },
            ],
        ),
        vec![Table {
            targets: vec![1, 3],
            default: 5,
        }],
    );
    for (index, answer) in [
        (0u64, 11u64),
        (1, 22),
        (2, 33),
        (1 << 32, 33),
        ((1 << 32) | 1, 33),
        (u64::MAX, 33),
    ] {
        let mut words = vec![index, 0];
        let left = run::<A>(&held, &mut words, 0);
        assert_eq!(left.outcome, Outcome::Returned, "case {index}");
        assert_eq!(words[1], answer, "case {index}");
    }
}

/// A call is handed to the runtime whole, and what comes back decides.
///
/// The helper is a double — see [`call`] — so what is asserted here is the
/// *protocol* and not a callee's answer: the caller's frame and the call's own
/// pc go over, the unpaid work goes with them, the answer lands in the slot the
/// IR named, and an outcome that is not `Returned` leaves the function carrying
/// that outcome.
pub fn a_call_hands_over_and_an_outcome_travels_out<A: Arm>() {
    let held = program_with_args(
        function(
            vec![Repr::Int, Repr::Int, Repr::Int],
            INT,
            vec![
                Inst::Int { dst: 0, value: 5 },
                Inst::Call {
                    dst: 2,
                    callee: FunctionId(0),
                    args: ArgsId(1),
                },
                Inst::Return { src: 2 },
            ],
        ),
        vec![Arg {
            slot: 0,
            layout: INT,
        }],
    );

    forget_polls();
    forget_calls();
    let mut words = vec![0u64; 7];
    let answer = run::<A>(&held, &mut words, 4);
    assert_eq!(answer.outcome, Outcome::Returned);
    // The double writes `callee * 1000 + dst`, which for `FunctionId(0)` into
    // slot 2 is 2 — a number no other word of this frame holds.
    assert_eq!(words[6], 2, "the answer landed in `dst`");
    assert_eq!(
        answer.returned,
        [2, UNWRITTEN, UNWRITTEN, UNWRITTEN],
        "and then out of `dst` into this function's own destination"
    );
    assert_eq!(
        calls(),
        vec![Called {
            base: 4,
            pc: 1,
            callee: 0,
            args: 1,
            dst: 2,
            // Three instructions in the one block, charged at its entry and
            // still unpaid when the call handed over.
            work: 3,
        }]
    );

    // A raise out of a callee leaves this function with the callee's outcome,
    // and does not overwrite what the helper recorded about it.
    forget_calls();
    calls_answer(&[Outcome::Raised]);
    let mut words = vec![0u64; 7];
    let answer = run::<A>(&held, &mut words, 4);
    assert_eq!(answer.outcome, Outcome::Raised);
    assert_eq!(answer.raise, None, "the runtime holds the error, not this");

    // And so does a stop.
    forget_calls();
    calls_answer(&[Outcome::Stopped]);
    let mut words = vec![0u64; 7];
    assert_eq!(run::<A>(&held, &mut words, 4).outcome, Outcome::Stopped);
}

/// A reference is in its slot at every safepoint, and that is checked rather
/// than reasoned about.
///
/// ADR 0055's "Collection uses the VM stack as the first root map" is only true
/// if every live reference is materialised in its Cove slot before a safepoint.
/// Neither arm register-promotes, so it *should* be true at every instruction
/// boundary — but "should" is what this test is for: the safepoint helper stands
/// where the collector stands and reads the frame word the reference lives in.
///
/// The loop is what makes there be safepoints at all: they go on backedges, and a
/// function with no loop reaches one only at a call. So this is a counting loop
/// that never touches the reference, which is the case a register allocator would
/// get wrong — a value nothing in the loop reads is exactly the one an optimiser
/// would be happiest to leave somewhere else.
pub fn a_reference_is_in_its_slot_at_every_safepoint<A: Arm>() {
    forget_polls();
    let held = program(function(
        vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Bool],
        INT,
        vec![
            Inst::Int { dst: 2, value: 0 },
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Lt,
                dst: 3,
                a: 2,
                b: 1,
            },
            Inst::BranchFalse { cond: 3, to: 5 },
            Inst::ArithImm {
                op: ArithOp::Add,
                dst: 2,
                a: 2,
                value: 1,
            },
            Inst::Jump { to: 1 },
            Inst::Return { src: 2 },
        ],
    ));

    // The frame does not begin at word zero, so a slot address formed as if it
    // did would read the wrong word and this test would notice.
    let base = 3usize;
    let sentinel = 0x1234_5678_9abc_def0u64;
    let mut words = vec![0u64; base + 4];
    words[base] = sentinel;
    words[base + 1] = 5;
    // Slot 0 of the frame, which is where the reference is and where the frame's
    // static `refs` map says a collector will look for it.
    watch(base);

    let answer = run::<A>(&held, &mut words, base as u64);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[base + 2], 5, "the loop ran");
    assert_eq!(
        words[base], sentinel,
        "the reference is still in its slot when the function leaves"
    );
    let saw = watched();
    assert_eq!(saw.len(), 5, "one safepoint per backedge");
    assert!(
        saw.iter().all(|word| *word == sentinel),
        "the reference was in its slot at every safepoint, and what was seen was {saw:?}"
    );
}

// --- the address family and `clear` ------------------------------------------

/// `encoded.rs`'s `CLEAR` arm (line 1138): `clear_words(base + slot, width)`.
///
/// The instruction whose whole purpose is what it stops happening — a reference
/// the frame no longer needs is not a root — so what is asserted is *zero*, at
/// every word the layout names and at no word beside them. A clear that missed a
/// word would leave a stale address for the collector to follow, and a clear that
/// wrote one too many would zero a live slot.
///
/// Three widths, because the bug in each direction is a different bug: one word
/// is a reference, two are a `struct { n: Int, s: String }` whose second word is
/// the one that matters, and none is an empty struct — for which `clear_words`
/// returns before it does anything and so must this.
pub fn a_clear_zeroes_the_words_its_layout_names<A: Arm>() {
    let held = |slot: Slot, layout: LayoutId| {
        program(function(
            vec![Repr::Int, Repr::Ref, Repr::Ref, Repr::Int],
            INT,
            vec![Inst::Clear { slot, layout }, Inst::Return { src: 3 }],
        ))
    };
    let full = [0x1111u64, 0x2222, 0x3333, 7];

    for (slot, layout, after) in [
        (1u32, REF, [0x1111u64, 0, 0x3333, 7]),
        (1, REF_PAIR, [0x1111, 0, 0, 7]),
        (1, EMPTY, [0x1111, 0x2222, 0x3333, 7]),
    ] {
        forget_polls();
        let mut words = full.to_vec();
        let answer = run::<A>(&held(slot, layout), &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned, "clear at {slot}");
        assert_eq!(words, after.to_vec(), "clear at {slot}");
    }

    // And at a frame that does not begin at word zero, because a clear whose
    // address was formed as if it did would zero somebody else's words.
    forget_polls();
    let mut words = vec![0xfeedu64, 0x1111, 0x2222, 0x3333, 7];
    let answer = run::<A>(&held(1, REF_PAIR), &mut words, 1);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words, vec![0xfeed, 0x1111, 0, 0, 7]);
}

/// `encoded.rs`'s `ADDR_OF_SLOT` arm (line 1699): `base + slot`, where `base` is
/// the frame's **linear address**.
///
/// The thing this catches is the one mistake the ABI's shape makes easy. The entry
/// point is handed the frame as a *segment-relative index*, because the stack's
/// `Vec` moves and the index does not; a `Repr::Addr` word is a linear address,
/// which is that index plus the segment's origin. An arm that wrote the index
/// would produce a word that is wrong by a million and that the VM would follow
/// into another task's segment — and it would be *right* on the first segment,
/// which is why [`SEGMENT_ORIGIN`] is not zero.
pub fn an_address_of_a_slot_is_the_linear_address_of_it<A: Arm>() {
    for (base, slot) in [(0u64, 0u32), (0, 2), (3, 1)] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Addr, Repr::Int, Repr::Int],
            ADDR,
            vec![Inst::AddrOfSlot { dst: 1, slot }, Inst::Return { src: 1 }],
        ));
        let mut words = vec![0u64; base as usize + 4];
        let answer = run::<A>(&held, &mut words, base);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(
            words[base as usize + 1],
            SEGMENT_ORIGIN + base + u64::from(slot),
            "the address of slot {slot} of the frame at {base}"
        );
    }
}

/// `encoded.rs`'s `ADDR_OF_PART` arm (line 1729), whose own comment is the whole
/// of it: "Arithmetic and nothing else."
///
/// A place is the address of the *first* word of a value location, so a part of
/// one is at a static word offset from it — and that holds whichever region the
/// address names, which is why one arm serves a field of a stack local and a field
/// of a heap object alike.
pub fn an_address_of_a_part_is_one_addition<A: Arm>() {
    for (addr, at) in [
        (SEGMENT_ORIGIN + 7, 0u32),
        (SEGMENT_ORIGIN + 7, 3),
        (HEAP_ORIGIN_WORDS + 9, 2),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Addr, Repr::Addr],
            ADDR,
            vec![
                Inst::AddrOfPart {
                    dst: 1,
                    addr: 0,
                    at,
                },
                Inst::Return { src: 1 },
            ],
        ));
        let mut words = vec![addr, 0];
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(words[1], addr + u64::from(at), "{addr} + {at}");
    }
}

/// `encoded.rs`'s `LOAD` and `STORE` arms (lines 1735 and 1740), which are
/// `Memory::copy_words` through an address — **and so are the `is_stack(addr)`
/// branch in front of it**.
///
/// One address type names two regions, and that is the whole of what these two
/// instructions are for: `bump(var total)` writes a local through the same
/// instruction pair that `piece.count = 1` writes a field with. So each direction
/// is asked twice, once at an address in this frame and once at an address in the
/// heap, and an arm that decoded the region wrongly reads or writes a word a
/// billion places from the right one.
///
/// The heap object straddles a chunk boundary for [`a_load_elem_strides_and_bounds_its_index`]'s
/// reason: a two-word run whose words are in different chunks is the case a
/// pointer formed once and reused would get wrong.
pub fn a_load_and_a_store_reach_either_region<A: Arm>() {
    // `s1 = *s0` at two words, and then the answer.
    let loads = program(function(
        vec![Repr::Addr, Repr::Int, Repr::Int],
        PAIR,
        vec![
            Inst::Load {
                dst: 1,
                addr: 0,
                layout: PAIR,
            },
            Inst::Return { src: 1 },
        ],
    ));
    // `*s0 = s1`, and an answer that says nothing, so what is asserted is the
    // words the store left behind.
    let stores = program(function(
        vec![Repr::Addr, Repr::Int, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::Int { dst: 3, value: 0 },
            Inst::Store {
                addr: 0,
                src: 1,
                layout: PAIR,
            },
            Inst::Return { src: 3 },
        ],
    ));

    // --- the stack ----------------------------------------------------------
    //
    // The address names word 4 of the segment, which is slot 4 of the frame at
    // zero: a load through it is a load of the frame's own words, which is what a
    // `var` parameter naming a caller's local is.
    forget_polls();
    let mut words = vec![SEGMENT_ORIGIN + 4, 0, 0, 0, 101, 102];
    let answer = run::<A>(&loads, &mut words, 0);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!((words[1], words[2]), (101, 102), "loaded off the stack");
    assert_eq!(answer.returned, [101, 102, UNWRITTEN, UNWRITTEN]);

    forget_polls();
    let mut words = vec![SEGMENT_ORIGIN + 4, 201, 202, 0, 0, 0];
    let answer = run::<A>(&stores, &mut words, 0);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!((words[4], words[5]), (201, 202), "stored onto the stack");
    assert_eq!(words[3], 0, "and nothing beside them");

    // A frame that does not begin at word zero, and an address into word zero of
    // the segment, below it: the caller's frame is where a `var` parameter's
    // target actually is.
    forget_polls();
    let mut words = vec![0, 0, SEGMENT_ORIGIN, 301, 302, 0, 0];
    let answer = run::<A>(&stores, &mut words, 2);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!((words[0], words[1]), (301, 302), "stored below the frame");

    // --- the heap -----------------------------------------------------------
    let at = HEAP_CHUNK_WORDS - 1;
    let build = || {
        let mut heap = Heap::new(2);
        heap.set(at, 401);
        heap.set(at + 1, 402);
        heap
    };

    forget_polls();
    let heap = build();
    let mut words = vec![heap.addr(at), 0, 0];
    let answer = run_over::<A>(&loads, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        (words[1], words[2]),
        (401, 402),
        "loaded across a chunk boundary"
    );

    forget_polls();
    let mut heap = build();
    let mut words = vec![heap.addr(at), 501, 502, 0];
    let answer = run_over::<A>(&stores, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        (heap.get(at), heap.get(at + 1)),
        (501, 502),
        "stored across a chunk boundary"
    );
    heap.set(at, 0);

    // --- a run that overlaps itself -----------------------------------------
    //
    // `Memory::copy_words` is a `memmove`, and an address formed by
    // `addr-of-slot` from *this* frame is what makes the overlapping case
    // reachable: a forward run of load-store pairs would smear `[601, 602]` into
    // `[601, 601]`.
    forget_polls();
    let overlapping = program(function(
        vec![Repr::Addr, Repr::Int, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::Int { dst: 3, value: 0 },
            Inst::AddrOfSlot { dst: 0, slot: 2 },
            Inst::Store {
                addr: 0,
                src: 1,
                layout: PAIR,
            },
            Inst::Return { src: 3 },
        ],
    ));
    let mut words = vec![0u64, 601, 602, 0];
    let answer = run::<A>(&overlapping, &mut words, 0);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        (words[2], words[3]),
        (601, 602),
        "the run moved rather than smearing"
    );
}
