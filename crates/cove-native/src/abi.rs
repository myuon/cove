//! The boundary between compiled code and the runtime that called it.
//!
//! Everything in this module is compiled whether or not a code generator's
//! feature is on, and that is deliberate: the ABI is a contract between two
//! crates, not a Cranelift artefact. A build with no code generator still has the
//! declarations, so the runtime side can be written, read and type-checked
//! against them without pulling in an executable-memory dependency.
//!
//! # One frame, addressed by index
//!
//! A Cove value lives in a slot of a frame, and a frame is a run of `u64`
//! words in the task's stack segment — [ADR 0034]'s one linear address space,
//! which `cove_runtime::vm::mem` implements. Compiled code reads and writes
//! exactly those words. It has no frame of its own for Cove values, no
//! shadow copy, and no second representation: `Memory::slot(base, n)` and the
//! load this crate emits for slot `n` name the same word.
//!
//! **That word array is a `Vec<u64>` and it reallocates.**
//! `Memory::push_frame` calls `Vec::resize`, so the *address* of word zero is
//! not stable across anything that can open a frame. This is the single most
//! dangerous fact in the whole design, so the ABI is written so that the
//! dangerous thing is not expressible:
//!
//! - the entry point is handed a **word index**, not a pointer — the value
//!   `Memory::stack_index(base)` answers, which is relative to the segment's
//!   origin and is therefore unaffected by the `Vec` moving;
//! - the pointer to word zero lives in [`NativeCtx::words`], is re-loaded
//!   from there by the generated code, and is **re-derived after every call
//!   to a runtime helper**. A helper that grows the stack is required to
//!   store the new pointer back into that field before it returns.
//!
//! So a raw pointer into the words is never live across a call. It is live
//! only between one helper call and the next, which is a span in which
//! nothing can grow a `Vec` the runtime owns.
//!
//! # What the entry point is
//!
//! ```text
//! extern "C" fn(
//!     ctx: *mut NativeCtx,
//!     base: u64,
//!     return_base: u64,
//!     return_slot: u32,
//! ) -> Outcome
//! ```
//!
//! [`Entry`] is that signature. Four arguments and a small integer result,
//! and every one of the choices in it is load-bearing:
//!
//! - **`ctx` is a pointer to mutable per-call state**, not a set of separate
//!   arguments, because everything the callee has to tell the caller that is
//!   not "which of three ways did you leave" is a field of it: which runtime
//!   error to raise, how much unpaid work to charge. Adding one of those later
//!   costs a field rather than a new signature, and a signature is what both
//!   tiers are pinned to.
//! - **`base` is `u64` and is a word index**, for the reason above. It is not
//!   the linear address `Memory::push_frame` answers, and it is not a
//!   pointer.
//! - **`return_base` and `return_slot` are the destination**, and they are
//!   [ADR 0057]: the callee writes its answer into the run of words its
//!   caller named rather than reporting a slot for the caller to copy out of.
//!   They are indices for exactly the reason `base` is one — the stack `Vec`
//!   reallocates under a `push_frame`, so a *pointer* at the destination,
//!   taken before the callee's frame was opened, would be dangling by the
//!   time the callee returned — and they are two numbers rather than one
//!   because the caller's frame and the slot within it are what the lowering
//!   settled separately.
//! - **The result is a three-way [`Outcome`] rather than a `bool` or a
//!   `Result`.** "Returned", "raised" and "must stop" are three different
//!   things the caller does three different things with, and a `Result` is
//!   not an FFI type.
//! - **No parameters are passed in registers.** The callee's arguments are
//!   already in slots `[0, width)` of the frame `base` names, put there by
//!   whoever opened it — which is what `open_frame` in
//!   `cove_runtime::vm::exec::encoded` already does for an encoded callee.
//!   One convention serves both tiers, which is ADR 0055's "one slot ABI
//!   joins both tiers", and it is why a VM-to-native call needs no marshalling
//!   layer.
//!
//! # Values stay in the frame
//!
//! ADR 0055 permits compiled code to keep non-reference temporaries in
//! registers between safepoints. **This implementation does not.** Every IR
//! slot read is a load and every slot write is a store, so the frame is the
//! canonical home of every Cove value at every instruction boundary and not
//! merely at a safepoint.
//!
//! That is a deliberate first-slice choice and it is worth being explicit
//! about why, because it is the thing a later slice will want to change:
//! keeping the frame canonical everywhere makes the safepoint story free. A
//! safepoint has nothing to spill, the program counter it reports is the only
//! state that has to be synchronised, and a collector walking the frame at a
//! safepoint sees exactly what it sees for an encoded frame. Register
//! promotion is an optimisation to be measured later, and when it is added it
//! will bring the spill discipline with it.
//!
//! # References are live here, and the frame is why that is safe
//!
//! This slice compiles [`Repr::Ref`](cove_ir::Repr::Ref) slots: a `String` and
//! an `Array` reach a function as parameters, and reading an element out of one
//! is what the covefmt slice is for. So ADR 0055's "Collection uses the VM
//! stack as the first root map" is now load-bearing rather than vacuous.
//!
//! It is honoured by the rule above and by nothing else. **Neither code
//! generator keeps a Cove value in a register across an instruction
//! boundary**, so at every place a collection can happen — the safepoint
//! helper, the call helper, and now [`AllocFn`], [`BuiltinFn`] and [`BufferFn`],
//! which are all the calls either arm emits — every live reference is already in
//! the slot the frame's static `Function::refs` map names. The collector walks exactly what it
//! walks for an encoded frame, and there is no spill sequence, because there is
//! nothing anywhere else to spill.
//!
//! [`AllocFn`] is the one that made that argument load-bearing rather than
//! merely true. Before it, compiled code could not *cause* a collection: it
//! reached the collector only at a safepoint, where nothing had been half-built,
//! or through a call, where the callee's own frame was the thing at risk. An
//! allocation made from a compiled frame collects with that frame's own
//! references live in it, so the walk this section describes is now the thing
//! standing between an allocation and a swept object — and
//! `native_tier.rs`'s forced-collection cases are how that is checked rather
//! than argued.
//!
//! [`BufferFn`] is where that argument had to be made a second time and could
//! not be weakened. A growable buffer is a *stable owner over a replaceable
//! store* — [ADR 0052] — so between allocating the store and allocating the
//! owner there is a live object nothing in any frame names. The runtime holds it
//! with `Machine::push_temp`, and that is exactly why the whole of
//! `alloc-buffer` is one helper rather than two [`AllocFn`] calls with emitted
//! code in between: the half-built state has a root discipline of its own, and it
//! is not the frame's.
//!
//! The intermediate an instruction computes *inside* one template — the object
//! address a `load-elem` derives, the header word a `len` reads — is in a
//! register, and is not a root: it is a copy of the reference the slot it was
//! loaded from still holds, and no template contains a call, so no collection
//! can happen between the load and the last use. Register promotion across
//! instructions is what would end that argument, and neither arm does it.
//!
//! # The heap is addressed through a table of chunk bases
//!
//! A frame is a run of words in a `Vec` and one pointer reaches all of it. The
//! *heap* is not: `cove_runtime::vm::mem` holds it as a fixed spine of
//! separately allocated chunks, because the run's tasks all read it at once and
//! a growing `Vec` would move words out from under a reader. So a heap word is
//! two indexings rather than one, and compiled code does the same two:
//!
//! ```text
//! index = addr - HEAP_ORIGIN_WORDS
//! chunk = ctx.chunks[index >> HEAP_CHUNK_SHIFT]
//! word  = chunk[index & (HEAP_CHUNK_WORDS - 1)]
//! ```
//!
//! [`NativeCtx::chunks`] is that table: one pointer per *committed* chunk, in
//! order, which the runtime maintains and re-publishes for the same reason it
//! re-publishes [`NativeCtx::words`] — a helper may have allocated, and an
//! allocation may have committed a chunk the table did not have. The three
//! constants are declared here, beside the layout they describe, and the
//! runtime asserts they are its own.
//!
//! # An address names either region, and compiled code decides which
//!
//! A `Repr::Addr` slot holds a **linear word index** in the one address space
//! above: the address of the first word of a value location, which may be a slot
//! of a frame or a payload word of a heap object. `Memory::read`,
//! `Memory::write` and `Memory::copy_words` each begin with `is_stack(addr)` —
//! `addr < STACK_WORDS`, which is [`HEAP_ORIGIN_WORDS`] — and pick the region
//! from it. **Generated code makes the same comparison, inline**, and the reason
//! it is emitted rather than delegated to a helper is that the two arms of it
//! are four instructions and eleven: a call would cost more than either, and it
//! would end the span in which a cached [`NativeCtx::words`] is live, because a
//! helper is permitted to grow the stack and the generated code cannot tell that
//! this one would not.
//!
//! Resolving a stack address needs one number the frame pointer does not carry.
//! `base` is an index *relative to the task's segment origin*, and a linear
//! address is not, so the two cannot be subtracted from one another:
//!
//! ```text
//! stack word at addr = ctx.words[addr - ctx.stack_origin]
//! ```
//!
//! [`NativeCtx::stack_origin`] is that origin. It is fixed for the life of the
//! task — a segment is a reserved range of the index space, chosen when the task
//! attaches — so unlike the two pointers beside it, it is published once and
//! never re-published.
//!
//! An address is **not** a root. `Function::refs` names
//! [`Repr::Ref`](cove_ir::Repr::Ref) slots and nothing else, and ADR 0034 is why:
//! what an address points into is kept alive by the reference slot holding the
//! base object, and the lowering keeps that slot live for exactly the address's
//! live range. So admitting `Repr::Addr` into a compiled frame adds nothing to
//! the collector's walk, and compiled code must not weaken the invariant that
//! makes that true — which it cannot, because it neither allocates nor decides
//! where a `Clear` goes.
//!
//! # A literal's address is a run-time load, and it could not have been anything
//! else
//!
//! [ADR 0045] places every program literal in the heap before the run's first
//! instruction, so `Inst::Str` is "a load of a precomputed address" and the
//! encoded tier's arm is one table read. It is tempting to read that as *a
//! constant*, and to fold the address into the generated code as an immediate.
//!
//! It is not one, and the order the runtime does things in is why. A
//! `NativeProgram` is compiled and finalized **before the `Machine` exists** —
//! `cove_runtime::native::compile` takes a `Program` and nothing else, and the
//! literals are placed by `Machine::for_run`, which runs afterwards. At the
//! moment a function is compiled there is no heap, so there is no address to
//! fold. The table is per-*run*, not per-`Program`.
//!
//! So [`NativeCtx::literals`] is that table's base, published once per context
//! beside [`NativeCtx::stack_origin`] and never re-published, and `Inst::Str` is
//! two loads and a store: the table out of the context, the address out of the
//! table, the address into the slot. Nothing about it can fail — placement
//! failure is already a refusal before a frame exists — so there is no guard, no
//! safepoint and no helper.
//!
//! [ADR 0034]: ../../../../docs/adr/0034-one-physical-word-stack.md
//! [ADR 0052]: ../../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md
//! [ADR 0045]: ../../../../docs/adr/0045-a-literal-is-there-before-the-program-runs.md
//! [ADR 0057]: ../../../../docs/adr/0057-a-native-call-returns-into-the-destination-its-caller-named.md

use std::ffi::c_void;

/// The first word of the heap region, in the one linear address space.
///
/// `cove_runtime::vm::mem`'s `STACK_WORDS`: every stack segment, back to back,
/// and the heap above them. An address below it is a frame word and an address
/// at or above it is a heap word, which is the whole of the region decoder —
/// and it is why zero can mean null for a [`Repr::Ref`](cove_ir::Repr::Ref).
pub const HEAP_ORIGIN_WORDS: u64 = 1 << 32;

/// How many bits of a heap index name the word inside its chunk.
pub const HEAP_CHUNK_SHIFT: u32 = 13;

/// How many words one committed heap chunk holds.
pub const HEAP_CHUNK_WORDS: u64 = 1 << HEAP_CHUNK_SHIFT;

/// A compiled function's entry point.
///
/// `base` is the **word index** of the callee's frame within the task's stack
/// segment: the value `Memory::stack_index` answers, not the linear address
/// `Memory::push_frame` does. See the module documentation for why it is an
/// index.
///
/// `return_base` and `return_slot` are the same kind of number and name the
/// destination: on [`Outcome::Returned`] the callee has written
/// `Function::returns`' width of words at `return_base + return_slot`, and on
/// any other outcome it has written nothing there. A zero-width return writes
/// nothing at all, and `return_slot` may then be a slot the caller's frame does
/// not even have — the lowering gives a width-0 destination the next free slot
/// number — so the address is not to be formed unless there are words to store.
///
/// # Safety
///
/// The caller promises that `ctx` is a valid, uniquely borrowed
/// [`NativeCtx`]; that `ctx.words` points at the first word of the segment
/// `base` indexes into; that the frame at `base` is at least
/// `Function::frame_size()` words long; and that the parameters occupy
/// `[0, width)` of it in declaration order.
///
/// It promises one thing more, and it is the widest part of this contract: that
/// `return_base + return_slot` names `Function::returns`' width of words which
/// are **live for the whole call and do not overlap the frame at `base`**. In
/// practice they are the caller's own frame, which is below the callee's and
/// therefore cannot overlap it; the callee interleaves loads from its frame with
/// stores to the destination and does not prove the two runs are disjoint.
///
/// The callee promises to touch no word outside its frame and that destination.
pub type Entry = unsafe extern "C" fn(
    ctx: *mut NativeCtx,
    base: u64,
    return_base: u64,
    return_slot: u32,
) -> Outcome;

/// How a compiled function left.
///
/// `#[repr(u32)]` with the three values written out, because the generated
/// code materialises them as integer constants. [`Outcome::abi`] is the one
/// place that conversion is spelled, and the code generator uses it, so the
/// two cannot drift.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The function reached an [`Inst::Return`](cove_ir::Inst::Return).
    ///
    /// The answer is **already in the destination**: `Function::returns`' width
    /// of words at `return_base + return_slot`, written before the callee's
    /// frame was removed. There is nothing for the caller to copy — which is
    /// ADR 0057, and is why this variant carries no slot.
    Returned = 0,
    /// The function raised a runtime error.
    ///
    /// [`NativeCtx::raise`] says which. The error's *text* is the runtime's
    /// to produce: this crate names the operation and the kind, and
    /// `cove-runtime` turns that into the `RuntimeError` it already builds
    /// for the encoded tier. That is the only way the two tiers can be
    /// word-for-word identical without this crate depending on that one.
    Raised = 1,
    /// A safepoint helper answered "stop".
    ///
    /// Why — cancellation, a deadline, exhausted fuel — is a question the
    /// helper already knows the answer to, so it is not repeated here. The
    /// caller resumes its own stop path.
    Stopped = 2,
}

impl Outcome {
    /// The integer the generated code returns for this outcome.
    pub const fn abi(self) -> u32 {
        self as u32
    }
}

/// Which runtime error a compiled function raised.
///
/// One variant per *distinct message* the encoded tier produces for the
/// operations this slice lowers, and no more. `cove-runtime`'s `int_arith`
/// is the reference; the mapping is one-to-one and the comments name the
/// call each variant stands for.
///
/// The messages themselves are deliberately absent. They live in
/// `cove_runtime::vm::exec::{overflowed, divided_by_zero}` and they must
/// stay there: a second copy of "`Int` addition overflowed" in this crate
/// would be a second source of truth for a rule of the *language*, and the
/// differential corpus is an expensive place to find out that the two copies
/// drifted.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Raise {
    /// `overflowed("addition")`
    AddOverflowed = 1,
    /// `overflowed("subtraction")`
    SubOverflowed = 2,
    /// `overflowed("multiplication")`
    MulOverflowed = 3,
    /// `overflowed("division")` — reached only by `i64::MIN / -1`, because a
    /// zero divisor is [`Raise::DividedByZero`] and is tested first.
    DivOverflowed = 4,
    /// `overflowed("remainder")` — reached only by `i64::MIN % -1`.
    RemOverflowed = 5,
    /// `overflowed("duration arithmetic")`.
    ///
    /// One variant for addition, subtraction and multiplication together,
    /// because `int_arith`'s `named` closure answers the same string for all
    /// three. Division and remainder do *not* consult it — they name the
    /// operation whatever the destination's `Repr` is — which is why there is
    /// no duration variant for them.
    DurationOverflowed = 6,
    /// `divided_by_zero("division")`
    DividedByZero = 7,
    /// `divided_by_zero("remainder")`
    RemainderByZero = 8,
    /// [`Inst::Trap`](cove_ir::Inst::Trap): the message is
    /// `program.string(StrId(detail))`, where `detail` is
    /// [`NativeCtx::raise_detail`].
    Trapped = 9,
    /// `null_object()` — a reference read before it was given one.
    ///
    /// What `encoded.rs`'s `LEN`, `RUN_LOAD_BYTES` and `Machine::element` each answer
    /// for a zero address, in that one word: the message is one sentence with
    /// no operand in it, so this variant carries nothing.
    NullObject = 10,
    /// `Machine::element`'s "index {a} is outside a collection of {b}", where
    /// the two numbers are [`NativeCtx::raise_a`] and [`NativeCtx::raise_b`].
    IndexOutOfRange = 11,
    /// `encoded.rs`'s `RUN_LOAD_BYTES` refusal: "`byteAt` is `{a}`, and a byte offset
    /// into this string is 0 to `{b} - 1`".
    ///
    /// `raise_b` is the string's byte length rather than the last legal offset,
    /// because that is the number the encoded arm has in hand and subtracts
    /// from; doing the subtraction here would put the `- 1` in two places.
    ByteOffset = 12,
    /// A helper refused, and the runtime already holds the error.
    ///
    /// The one variant that names no message, because there is none to name:
    /// a helper this compiled code called failed, and what failed is a whole
    /// `RuntimeError` — a span, a rule, a call chain — that the runtime built
    /// and kept. Compiled code learns only that it must leave, and leaves; the
    /// caller re-raises what it stashed.
    ///
    /// Three helpers answer this way and the variant is deliberately one rather
    /// than three, because *the code carries no message*: there is nothing for a
    /// second number to distinguish. [`NativeHelpers::call`] ran a callee which
    /// failed; [`NativeHelpers::alloc`] could not allocate, or its safepoint said
    /// stop; [`NativeHelpers::builtin`] ran a builtin which refused. The name is
    /// the oldest of the three and has stayed, because what it says is still what
    /// happened: something this code *called* failed.
    ///
    /// This is the same division as every other variant here, taken to its
    /// end: this crate names errors and never builds one.
    Called = 13,
    /// `overflowed("negation")` — reached only by negating `i64::MIN`.
    ///
    /// Numbered after [`Raise::Called`] rather than beside the other overflows,
    /// because these numbers are an **ABI**: generated code stores one and
    /// [`Raise::from_abi`] reads it, so inserting a variant into the middle would
    /// renumber every one below it and a stale page of machine code would name a
    /// different error. The family a variant belongs to is a fact about its
    /// documentation; its number is a fact about two builds agreeing.
    ///
    /// `encoded.rs`'s `NEG_INT` arm is `checked_neg` and names the operation
    /// unconditionally, so there is no `Duration` variant beside this one the way
    /// there is for addition: negating a `Duration` that overflows still says
    /// "negation". `int_arith`'s `named` closure is what renames the other three,
    /// and `NEG_INT` does not go through `int_arith` at all.
    NegOverflowed = 14,
}

impl Raise {
    /// The integer the generated code stores in [`NativeCtx::raise_code`].
    pub const fn abi(self) -> u32 {
        self as u32
    }

    /// The raise a `raise_code` names, or `None` for a code no code
    /// generator in this crate emits.
    pub const fn from_abi(code: u32) -> Option<Self> {
        match code {
            1 => Some(Raise::AddOverflowed),
            2 => Some(Raise::SubOverflowed),
            3 => Some(Raise::MulOverflowed),
            4 => Some(Raise::DivOverflowed),
            5 => Some(Raise::RemOverflowed),
            6 => Some(Raise::DurationOverflowed),
            7 => Some(Raise::DividedByZero),
            8 => Some(Raise::RemainderByZero),
            9 => Some(Raise::Trapped),
            10 => Some(Raise::NullObject),
            11 => Some(Raise::IndexOutOfRange),
            12 => Some(Raise::ByteOffset),
            13 => Some(Raise::Called),
            14 => Some(Raise::NegOverflowed),
            _ => None,
        }
    }
}

/// What a safepoint helper is.
///
/// `pc` is the index of the IR instruction the safepoint stands in front of,
/// which is what a synchronised frame's program counter has to become. `work`
/// is the number of IR instructions executed since the last safepoint — see
/// [`NativeCtx::pending_work`] for what that number is and is not.
///
/// `false` means stop. The helper is where [ADR 0040]'s three-step order
/// lives, in that order and not this crate's: cancellation and task-local
/// stops, then fuel and deadline accounting, then the collector rendezvous.
/// None of those three is implemented in compiled code, and none of them
/// should be — they are Rust the runtime already has, and ADR 0055's
/// "Runtime operations whose correctness already lives in Rust … remain
/// runtime helpers initially" is exactly this.
///
/// A helper is permitted to grow the stack. If it does, it must store the new
/// pointer into [`NativeCtx::words`] before returning, because the generated
/// code re-loads that field after every call and trusts what it finds.
///
/// # Safety
///
/// `ctx` is the pointer the generated code was called with, so the helper is
/// reached with the same borrow the entry point holds.
///
/// [ADR 0040]: ../../../../docs/adr/0040-a-bound-outlives-its-backend.md
pub type SafepointFn = unsafe extern "C" fn(ctx: *mut NativeCtx, pc: u32, work: u64) -> bool;

/// What a call helper is.
///
/// [`Inst::Call`](cove_ir::Inst::Call), handed back to the runtime whole.
/// Compiled code does not open the callee's frame, copy its arguments or choose
/// its tier: the runtime's `open_frame` already does all three for an encoded
/// caller, and a second copy of the calling convention in a code generator —
/// in *two* code generators — is the thing this signature exists to prevent.
/// ADR 0055's entry table is the runtime's to consult, so a callee may be
/// encoded or native and the caller cannot tell.
///
/// - `base` is the **caller's** frame, as a word index, because the answer goes
///   into `dst` of it and the arguments are read out of it.
/// - `pc` is the index of the `call` instruction, which is the pc a
///   synchronised frame has to carry and the span a failure is reported at.
/// - `callee` is a `FunctionId` and `args` an `ArgsId`, both as the plain `u32`
///   the IR carries — this crate does not resolve either.
/// - `dst` is the caller's slot the answer's words go into, at the callee's
///   return width, which is the callee's declaration's to know. The helper turns
///   it into the destination it hands the callee — `base + dst` is
///   [`Entry`]'s `return_base + return_slot` — so `dst` is part of the ABI now
///   rather than something the runtime applies afterwards.
///
/// Six integer arguments, which is exactly what the System V ABI passes in
/// registers, and the template arm's call sequence depends on that.
///
/// The answer is an [`Outcome`] as a `u32`. [`Outcome::Returned`] means the
/// words are in `dst` already and compiled code carries on; the other two are
/// returned from the compiled function unchanged, so a raise or a stop from
/// eight frames down leaves through one `ret` per frame and no unwinding.
///
/// The helper is handed the unpaid work in [`NativeCtx::pending_work`] rather
/// than as a seventh argument, and it charges and clears it: a call may
/// allocate and an allocation may collect, so ADR 0055's "around allocation or
/// runtime calls which may collect" makes this a safepoint whether or not the
/// callee reaches one of its own.
///
/// # Safety
///
/// As [`SafepointFn`]: `ctx` is the pointer the entry point was called with.
/// The helper may grow the stack and may commit a heap chunk, so it must store
/// the current [`NativeCtx::words`] and [`NativeCtx::chunks`] before it
/// returns, and the generated code re-derives both afterwards.
pub type CallFn = unsafe extern "C" fn(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    callee: u32,
    args: u32,
    dst: u32,
) -> u32;

/// What a direct call was given: where the callee's code is, and the frame it is
/// to run in.
///
/// The answer of an [`OpenFn`], and the reason a direct call is possible at all.
/// Two words, so the System V ABI hands it back in two registers and generated
/// code reads it without a memory round trip.
///
/// `entry` is null when **the runtime has finished the call itself**, and that is
/// not a failure: a callee with no compiled code runs on the encoded VM, which is
/// a complete execution path, and the mediated helper is how it is reached. Then
/// `base` is the [`Outcome`] of the finished call rather than a frame, and there
/// is nothing for generated code to enter.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Opened {
    /// The callee's compiled entry point, or null when the call is already over.
    pub entry: Option<Entry>,
    /// The callee's frame as a **word index**, or the finished call's
    /// [`Outcome`] when `entry` is null.
    ///
    /// An index for [`Entry`]'s reason: the stack's `Vec` reallocated under the
    /// `push_frame` that made this frame, and will reallocate again under the
    /// next one.
    pub base: u64,
}

/// What opens a callee's frame for a call generated code will make itself.
///
/// [`CallFn`] hands the runtime one `Inst::Call` whole and gets back an outcome:
/// the runtime opens the frame, copies the arguments, chooses the tier, enters
/// the callee, publishes the answer and pops the frame. This splits that in half
/// at the one point a code generator can do better than a runtime, which is that
/// **it knows the callee's shape at compile time**.
///
/// What this does, and what it leaves to emitted code:
///
/// - it charges the unpaid work in [`NativeCtx::pending_work`] and takes
///   [ADR 0040]'s safepoint, exactly as [`CallFn`] does and in the same order.
///   A direct call is a safepoint for the same reason a mediated one is;
/// - it admits the frame against the embedder's call-depth limit and pushes it,
///   so a runaway recursion is refused by the same two checks — the configured
///   limit and the stack segment's own bound — as before;
/// - it pushes the callee's `Frame`, so the callee's reference slots are walkable
///   by the collector before a word of it runs;
/// - it does **not** copy the arguments. Their slots and widths are settled by
///   the lowering, so emitted code stores them itself;
/// - it does **not** enter the callee, and it does not pop anything. The entry it
///   answers is called by generated code, and [`CloseFn`] is the other half.
///
/// When the callee has no compiled entry, this is the mediated helper and nothing
/// else: the whole of [`CallFn`] runs, arguments and tier and answer and frame,
/// and the answer is `entry: None` with the outcome in [`Opened::base`]. A mixed
/// call keeps the path it had.
///
/// # Safety
///
/// As [`CallFn`]. On any answer but `entry: None`, the caller must enter the
/// entry it was given with the frame it was given, and must reach [`CloseFn`]
/// afterwards whatever that entry answered: the frame is on the stack until it
/// does.
///
/// [ADR 0040]: ../../../../docs/adr/0040-a-bound-outlives-its-backend.md
pub type OpenFn = unsafe extern "C" fn(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    callee: u32,
    args: u32,
    dst: u32,
) -> Opened;

/// What finishes a call generated code made itself.
///
/// The other half of [`OpenFn`], and it is a helper rather than emitted code for
/// one reason: the frame stack and the word stack are Rust `Vec`s, and only Rust
/// may change how long they are.
///
/// - it charges whatever the callee left in [`NativeCtx::pending_work`], which is
///   ADR 0055's "pending work charged on every exit" and is done on a return, a
///   raise and a stop alike;
/// - on [`Outcome::Returned`] it removes the callee's frame — the top one, whose
///   answer is already in the caller's destination, because the callee wrote it
///   there before returning;
/// - on any other outcome it leaves the frames standing, because that is what a
///   runtime error's call chain is read out of, and it builds the error the
///   callee named if the runtime is not already holding one.
///
/// It answers the outcome it was given, unchanged, so that generated code can
/// test it once.
///
/// Nothing is republished. A direct call hands the callee **the caller's own
/// context**, so every helper the callee reached stored the current words pointer
/// and chunk table into the very context the caller will re-read — which is the
/// difference from [`CallFn`], where the callee is given a context of its own and
/// the caller's is therefore stale when it returns.
///
/// # Safety
///
/// As [`CallFn`]. `outcome` is what the entry answered and `callee` is the
/// function it was.
pub type CloseFn = unsafe extern "C" fn(ctx: *mut NativeCtx, outcome: u32, callee: u32) -> u32;

/// What an allocation helper is.
///
/// [`Inst::Alloc`](cove_ir::Inst::Alloc), handed to the runtime whole, and
/// [ADR 0055]'s "Runtime operations whose correctness already lives in Rust …
/// remain runtime helpers initially" with allocation as the archetype. What
/// `Machine::allocate` already handles is the whole story and none of it is
/// emitted: the `i64`-to-`u32` conversion, `Layout::try_payload_words`' overflow
/// rejection, the bump allocation, the collect-and-retry when the first attempt
/// does not fit, and the one refusal — "this run has no memory left" — that every
/// way of failing converges on.
///
/// `layout` is a `LayoutId` and `len` the header's length field, as the plain
/// numbers the IR carries. `len` is `i64` and not `u32` for the reason
/// `Machine::allocate`'s own documentation gives: [`Len::Slot`](cove_ir::Len::Slot)
/// is a count the running program computed, so a negative one and one past what a
/// `u32` field can hold are both *that call's* to reject, and narrowing here would
/// be a second rejection with a different message. `Len::Fixed` passes nought.
///
/// The answer is the new object's **linear word address**, or **zero**. Zero is
/// not an address — the heap begins at [`HEAP_ORIGIN_WORDS`] and zero is what a
/// null reference is — so it needs no second output to be unambiguous, and it
/// means the runtime is holding a whole `RuntimeError` that compiled code must
/// leave with as [`Raise::Called`]. Two things fail this way and the caller cannot
/// tell them apart, which is deliberate: the allocation was refused, or the
/// safepoint in front of it said stop. [`CallFn`] already maps a stop to
/// [`Outcome::Raised`] with the error stashed, and both arrive at the same
/// `Err(error)` in the runtime's `enter`.
///
/// # It is a safepoint, and that is ADR 0055 rather than a choice
///
/// "Safepoints occur at least: … around allocation or runtime calls which may
/// collect." So this helper charges [`NativeCtx::pending_work`], synchronises the
/// frame's program counter and takes [ADR 0040]'s three steps in that order
/// *before* it allocates — which is more than `encoded.rs`'s `ALLOC` arm does,
/// because that arm leans on the dispatch loop's own stride and compiled code has
/// no loop to lean on.
///
/// # Safety
///
/// As [`SafepointFn`]. The helper collects, so every live reference must already
/// be in the slot the frame's `Function::refs` names — which both code generators
/// satisfy by never keeping a Cove value in a register across an instruction
/// boundary. It may grow the stack and may commit a heap chunk, so it stores the
/// current [`NativeCtx::words`] and [`NativeCtx::chunks`] before it returns and
/// the generated code re-derives both afterwards.
///
/// [ADR 0040]: ../../../../docs/adr/0040-a-bound-outlives-its-backend.md
/// [ADR 0055]: ../../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md
pub type AllocFn = unsafe extern "C" fn(ctx: *mut NativeCtx, pc: u32, layout: u32, len: i64) -> u64;

/// What a builtin helper is: one [`Inst::CallBuiltin`](cove_ir::Inst::CallBuiltin),
/// handed to the runtime whole.
///
/// [`CallFn`]'s relationship to [`OpenFn`], for builtins. A builtin whose fast
/// path is emitted still has cold paths whose *message* only the runtime can
/// build — a `Vector` that `freeze()` consumed, a receiver whose object is not the
/// shape the call site expected — and those messages name a rendered `Value`,
/// which this crate cannot see and must never learn to. So emitted code tests the
/// fast path's preconditions, and where one does not hold it calls this and the VM
/// produces exactly the sentence it always produced. That is [`OpenFn`]'s "a mixed
/// call keeps the path it had", one level down.
///
/// **It is not a way to lower a builtin.** A `call-builtin` whose only lowering
/// was this helper would be the encoded tier's `CALL_BUILTIN` arm reached through
/// one more indirection: it would present a run as more native than it is, and
/// — because the two code generators would emit the identical call — it would make
/// the comparison between them measure nothing. `subset::method_of` is what admits
/// a builtin, and it admits one only where a fast path is emitted for it.
///
/// `base` is the **caller's** frame as a word index, and `dst`, `builtin` and
/// `args` are the three operands of the instruction as the plain numbers the IR
/// carries. `base` is [`CallFn`]'s `base` and is there for the same two reasons:
/// six integer arguments are what the System V ABI passes in registers, and a
/// helper that is handed the frame it was called from can *check* it against the
/// frame stack rather than assume it. The helper uses the stack's own address —
/// `Machine::call_builtin` reads slots, which needs a linear address and not an
/// index — and asserts the two agree.
///
/// The answer is an [`Outcome`] as a `u32`, read exactly as [`CallFn`]'s is:
/// [`Outcome::Returned`] means the answer's words are in `dst` already.
///
/// # Safety
///
/// As [`AllocFn`]: a builtin may allocate, so this is a safepoint and every live
/// reference must be in its slot, and both republished pointers are re-derived by
/// the generated code afterwards.
pub type BuiltinFn = unsafe extern "C" fn(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    dst: u32,
    builtin: u32,
    args: u32,
) -> u32;

/// What the field-access cold path is: [`Inst::LoadField`](cove_ir::Inst::LoadField),
/// whose bound [`NativeCtx::fixed_payload_words`] could not answer, handed to the
/// runtime whole.
///
/// The emitted fast path answers `at + width <= words` itself, where `words` is
/// one table load away for every *fixed*-payload shape — see
/// [`NativeCtx::fixed_payload_words`]. What it cannot answer is the bound for a
/// *variable*-payload shape, because that depends on `Layout::payload_words`'
/// per-shape arithmetic, which is not worth emitting for a family the census
/// never reaches. This helper is that arithmetic, run once: it is
/// `Machine::checked` — the same bound, dynamic and exact — and then the copy
/// `encoded.rs`'s `LOAD_FIELD` arm makes.
///
/// Unlike [`AllocFn`] and [`BuiltinFn`] this is **not a safepoint**: neither the
/// bound check nor the copy it guards can allocate, so there is nothing to
/// charge and no cached pointer a call here could stale.
///
/// `addr` is the object's own linear address — not a frame slot, but the
/// *value* emitted code already holds in a register, exactly as
/// [`Inst::AddrOfSlot`](cove_ir::Inst::AddrOfSlot) forms one — and `into` is the
/// linear address the answer's words are copied to, which is the frame's own
/// address plus the destination slot, formed the same way. `at` is the field's
/// static payload-word offset and `width` its static width. There is no `base`:
/// both addresses are already resolved, so there is nothing left to resolve one
/// against.
///
/// The answer is an [`Outcome`] as a `u32`, read exactly as [`BuiltinFn`]'s is.
///
/// # Safety
///
/// As [`AllocFn`]: `ctx` is the pointer the entry point was called with.
pub type FieldLoadFn = unsafe extern "C" fn(
    ctx: *mut NativeCtx,
    pc: u32,
    addr: u64,
    at: u32,
    width: u32,
    into: u64,
) -> u32;

/// [`FieldLoadFn`], the other direction: [`Inst::StoreField`](cove_ir::Inst::StoreField)'s
/// cold path. `from` is the linear address the words are copied *out of*, in
/// place of `into`.
pub type FieldStoreFn = unsafe extern "C" fn(
    ctx: *mut NativeCtx,
    pc: u32,
    addr: u64,
    at: u32,
    width: u32,
    from: u64,
) -> u32;

/// Which of [ADR 0052]'s four growable-buffer instructions a [`BufferFn`] was
/// handed.
///
/// `#[repr(u32)]` with the values written out, for [`Outcome`]'s reason: the
/// generated code materialises them as integer constants and the runtime matches
/// on them, and [`BufferOp::abi`] is the one place that conversion is spelled.
///
/// They are one helper and one enum rather than four helpers because every
/// property the boundary cares about is the same for all four — each is a
/// safepoint, each may collect, each may grow the stack and commit a chunk, each
/// writes whatever it answers into the frame itself, and each answers an
/// [`Outcome`]. Four typedefs differing in one pair of `u32`s would be four
/// relocations, four emitters and four places for the safepoint discipline to
/// drift. They are also ADR 0052's four: a build that had three of them would be
/// a build that could allocate a builder it could not finish.
///
/// [ADR 0052]: ../../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BufferOp {
    /// [`Inst::AllocBuffer`](cove_ir::Inst::AllocBuffer). `a` is `dst` and `b` the
    /// slot holding the capacity.
    Alloc = 0,
    /// [`Inst::AppendByte`](cove_ir::Inst::AppendByte). `a` is the owner's slot
    /// and `b` the value's; there is no destination.
    AppendByte = 1,
    /// [`Inst::AppendBytes`](cove_ir::Inst::AppendBytes). `a` is the `ArgsId`
    /// whose four entries are `buffer`, `src`, `from` and `to`; `b` is unused.
    ///
    /// The operands are behind an argument list rather than in `a` and `b`
    /// because the instruction has four of them — see `Inst::AppendBytes`'s own
    /// note — and the helper resolves the list out of the program, exactly as
    /// `encoded.rs`'s arm does.
    AppendBytes = 2,
    /// [`Inst::FinishBuffer`](cove_ir::Inst::FinishBuffer). `a` is `dst` and `b`
    /// the owner's slot.
    Finish = 3,
}

impl BufferOp {
    /// The integer the generated code passes for this operation.
    pub const fn abi(self) -> u32 {
        self as u32
    }

    /// Which operation `code` names, or `None` for a number no arm emits.
    pub const fn from_abi(code: u32) -> Option<Self> {
        match code {
            0 => Some(BufferOp::Alloc),
            1 => Some(BufferOp::AppendByte),
            2 => Some(BufferOp::AppendBytes),
            3 => Some(BufferOp::Finish),
            _ => None,
        }
    }
}

/// What a growable-buffer helper is: one of [ADR 0052]'s four instructions,
/// handed to the runtime whole.
///
/// [`AllocFn`]'s relationship to `Inst::Alloc`, for the builder. ADR 0055's
/// "Runtime operations whose correctness already lives in Rust … remain runtime
/// helpers initially" with allocation as the archetype — and this family is that
/// sentence's next four cases, each for a reason of its own rather than by
/// analogy. They were measured against emitting a fast path, and each one lost:
///
/// - **`AllocBuffer` allocates twice, and nothing in a frame can name what the
///   first one answered.** `Machine::alloc_buffer` allocates the store, holds it
///   with `Machine::push_temp` across the *owner's* allocation, and only then
///   writes the store into the owner's payload. Emitted code could make both
///   calls through [`AllocFn`] — but between them the store is reachable from
///   nothing the collector walks, and the IR gives this instruction one
///   destination, so there is no `Repr::Ref` slot to put it in. `crate::abi`'s
///   "References are live here, and the frame is why that is safe" is the whole
///   of this tier's collector discipline and it is exactly what splitting this
///   would break. The temporary root exists because a Rust local is not a root;
///   a register in a compiled frame is not one either;
/// - **`AppendBytes` copies in bounded chunks with a safepoint between them**,
///   which is ADR 0052's "bulk work remains proportionally charged" and ADR
///   0040's stop bound. A generated fast path must not skip that. It could not
///   honour it either without emitting `Machine::copy_string_bytes` — a
///   byte-blending copy over the two-region address decode — around a poll, and
///   in front of all of it the eight refusals whose sentences name offsets and
///   lengths the runtime formats. So it is mediated, and the chunking stays where
///   it already works;
/// - **`FinishBuffer` validates and then relabels, and only the second half is
///   small.** The relabel is a header write and a free block, which is emittable;
///   the validation walks the live prefix through `std::str::from_utf8`, which is
///   not. Emitting the tail of an operation whose head is a helper call buys
///   nothing, because the call is already made;
/// - **`AppendByte` is three lines and is here anyway.** It is not on the census
///   — the corpus appends ranges, not bytes — but a subset that lowered the other
///   three would refuse a function for the one scalar append in it, which is a
///   refusal with no work behind it.
///
/// # It is a lowering, and [`BuiltinFn`] is not
///
/// [`BuiltinFn`]'s documentation says in as many words that it "is not a way to
/// lower a builtin", and the distinction is worth keeping sharp rather than
/// quietly crossing. That helper is the **cold path** of a builtin whose fast
/// path is emitted; a `call-builtin` reached only through it would present a run
/// as more native than it is *and* would make both code generators emit the
/// identical call, so the comparison between them would measure nothing.
///
/// This is the other kind, the kind [`AllocFn`] is: the operation's correctness
/// lives in Rust and stays there, and what the lowering buys is not a faster
/// append — it is that **the function around it compiles**. `covefmt.spacing` and
/// `covefmt.flattened` were refused whole for one `alloc-buffer` each; every
/// other instruction in them ran on the encoded tier because of it.
///
/// `base` is the caller's frame as a word index, `pc` the instruction's index,
/// and `a` and `b` the operands [`BufferOp`] names for each variant. Six integer
/// arguments, which is what the System V ABI passes in registers and what the
/// template arm's call sequence depends on. The answer is an [`Outcome`] as a
/// `u32`, read exactly as [`BuiltinFn`]'s is.
///
/// # Safety
///
/// As [`AllocFn`]: this is a safepoint, so every live reference must already be
/// in the slot the frame's `Function::refs` names, and both republished pointers
/// are re-derived by the generated code afterwards.
///
/// [ADR 0052]: ../../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md
pub type BufferFn =
    unsafe extern "C" fn(ctx: *mut NativeCtx, base: u64, pc: u32, op: u32, a: u32, b: u32) -> u32;

/// The runtime's side of the boundary, as function pointers.
///
/// This table is the whole reason `cove-native` does not depend on
/// `cove-runtime`; see the crate documentation for the inversion it buys.
/// It is supplied once, when a code generator is created, and the addresses
/// it holds are bound into the compiled code as ordinary relocations.
///
/// It is `Copy` and holds nothing but `fn` pointers, so it is shared by
/// value and there is no lifetime to thread through the code generator.
#[derive(Clone, Copy)]
pub struct NativeHelpers {
    /// See [`SafepointFn`].
    pub safepoint: SafepointFn,
    /// See [`CallFn`].
    pub call: CallFn,
    /// See [`OpenFn`]. A code generator that makes no direct call binds it and
    /// never reaches it.
    pub open: OpenFn,
    /// See [`CloseFn`].
    pub close: CloseFn,
    /// See [`AllocFn`].
    pub alloc: AllocFn,
    /// See [`BuiltinFn`].
    pub builtin: BuiltinFn,
    /// See [`BufferFn`].
    pub buffer: BufferFn,
    /// See [`FieldLoadFn`].
    pub field_load: FieldLoadFn,
    /// See [`FieldStoreFn`].
    pub field_store: FieldStoreFn,
}

/// The mutable state one native call reads and writes.
///
/// `#[repr(C)]` because the field offsets are compiled into machine code.
/// The code generator reads them with [`std::mem::offset_of`] from this very
/// declaration rather than from a table of numbers, so reordering the fields
/// is safe and adding one is safe; only changing what a field *means* is not.
#[repr(C)]
pub struct NativeCtx {
    /// Whatever the runtime needs to find itself again — in practice a
    /// `*mut Machine`.
    ///
    /// Opaque here on purpose. This crate cannot name `Machine`, must not
    /// grow a structural opinion about it, and does not dereference this: it
    /// passes the pointer to helpers and nothing else.
    pub host: *mut c_void,
    /// The first word of the task's stack segment.
    ///
    /// Re-loaded by the generated code at the start of every basic block and
    /// after every helper call, and cached in a register for no longer than
    /// that. See the module documentation: the `Vec` behind this pointer
    /// reallocates, so a helper that grows the stack must store the new
    /// pointer here before it returns.
    pub words: *mut u64,
    /// One pointer per committed heap chunk, in order.
    ///
    /// The heap's answer to [`NativeCtx::words`], and it is a table rather than
    /// a pointer for the reason the module documentation gives: the heap is a
    /// spine of separately allocated chunks, so there is no one pointer that
    /// reaches every heap word. `HEAP_CHUNK_WORDS` words per entry, and only
    /// the committed prefix is present — an entry is never null.
    ///
    /// Re-loaded wherever [`NativeCtx::words`] is, and for the same reason one
    /// step further out: a helper may have allocated, an allocation may have
    /// committed a chunk, and committing one may have moved the table.
    pub chunks: *const *mut u64,
    /// Every program literal's heap address, in `StrId` order.
    ///
    /// `Machine::literal_addrs`, as a pointer to its first word — the `Arc<[u64]>`
    /// [ADR 0045] places before the run's first instruction. `Inst::Str` reads
    /// `literals[text]` out of it, which is the same load the encoded tier's `STR`
    /// arm makes.
    ///
    /// Published **once**, as [`NativeCtx::stack_origin`] is, and for a stronger
    /// reason than that one: the table is placed before any frame exists, is never
    /// written again, and is shared by every task of the run. Nothing a helper does
    /// can move it, so it is not re-derived after a call and generated code may
    /// cache it for as long as it likes.
    ///
    /// It is a run-time load rather than a compile-time immediate because the
    /// *compiler* runs first; see the module documentation's "A literal's address
    /// is a run-time load".
    ///
    /// Null for a caller whose compiled code holds no literal, which is
    /// [`NativeCtx::chunks`]' rule and is loud for the same reason: a table that
    /// was never published is a null dereference where a dangling one would be a
    /// wrong address. [`NativeCtx::over_literals`] is how a caller with literals
    /// says so.
    ///
    /// [ADR 0045]: ../../../../docs/adr/0045-a-literal-is-there-before-the-program-runs.md
    pub literals: *const u64,
    /// Every layout's fixed payload width, in `LayoutId` order — `0` where
    /// there is none.
    ///
    /// `Layout::fixed_payload_words` answers `Some` for every shape whose
    /// heap object has a payload width that does not depend on its runtime
    /// header — `Word`, `Struct`, `Enum`, `Vector`, `ByteBuffer`, `Shared` and
    /// `Closure` — and `None` for the rest (`Free`, `Str`, `Bytes`,
    /// `Elements`, `Members`, `Entries`, `Boxed`), whose width is `len`-
    /// dependent arithmetic `Layout::payload_words` performs at run time.
    /// This table is the `Some` half of that, one `u32` per layout, with `0`
    /// standing in for `None` — which is safe because a `0`-word fixed object
    /// does not exist and a genuine `0` would refuse every field access
    /// anyway.
    ///
    /// [`Inst::LoadField`](cove_ir::Inst::LoadField) and
    /// [`Inst::StoreField`](cove_ir::Inst::StoreField) are the readers: the
    /// object's own header names a `LayoutId`, this table answers that
    /// layout's fixed width in one load, and a field whose `at + width` fits
    /// is answered without leaving compiled code. A `0` entry always fails
    /// that comparison for a non-empty field, so a variable-payload object
    /// takes [`FieldLoadFn`]/[`FieldStoreFn`]'s cold path without this arm
    /// ever asking which shape it is.
    ///
    /// Published **once**, exactly as [`NativeCtx::literals`] is and for the
    /// same reason: the table is derived from the program's own layout table,
    /// which a run never changes, so nothing republishes it and a caller may
    /// cache it for as long as it likes.
    ///
    /// Null for a caller whose compiled code loads no field, [`NativeCtx::literals`]'s
    /// rule. [`NativeCtx::over_payload_words`] is how a caller with fields says so.
    pub fixed_payload_words: *const u32,
    /// The linear address of word zero of the task's stack segment.
    ///
    /// What [`NativeCtx::words`] points *at*, as a number in the one address
    /// space — `cove_runtime::vm::mem`'s `segment_origin(at)`. It is what turns a
    /// `Repr::Addr` word that names the stack into an index into `words`; see the
    /// module documentation's "An address names either region".
    ///
    /// Unlike the two pointers above it is published **once** and never
    /// re-published, because a task's segment is chosen when it attaches and does
    /// not move: the `Vec` inside the segment reallocates, which is what makes
    /// `words` unstable, and the segment's place in the index space is not that
    /// `Vec`.
    pub stack_origin: u64,
    /// IR instructions executed since the last safepoint and not yet charged.
    ///
    /// Written on every exit — a return, a raise and a stop alike — which is
    /// ADR 0055's "pending work charged on every exit". At a stop it is zero,
    /// because the safepoint that said stop was handed the charge before it
    /// answered.
    ///
    /// It is a count of *IR instructions*, statically accumulated per basic
    /// block, and it is therefore not the encoded tier's `fuel_spent` and
    /// must never be reported as it. ADR 0055 says so twice: fuel is
    /// backend-specific, and "aggregated static IR-work counts … are a
    /// different metric with a different name".
    pub pending_work: u64,
    /// Which error, when the outcome is [`Outcome::Raised`]. See
    /// [`Raise::from_abi`].
    pub raise_code: u32,
    /// The one number a raise carries: a `StrId` for [`Raise::Trapped`], and
    /// unused by every other variant.
    pub raise_detail: u32,
    /// The IR instruction a raise happened at.
    ///
    /// The span every runtime error carries is `Function::span_at(pc)`, which
    /// is the *program's* fact and so the runtime's to look up — but only
    /// compiled code knows which instruction it was executing. `encoded.rs`
    /// has the answer in a local; here it has to be stored, and it is stored
    /// on the raising path only, so the ordinary path pays nothing for it.
    pub raise_pc: u32,
    /// The first of the two numbers an out-of-range refusal names.
    ///
    /// The offending index for [`Raise::IndexOutOfRange`], the offending byte
    /// offset for [`Raise::ByteOffset`], and unused by everything else. Signed,
    /// because the refusal is *for* a negative one as much as for a large one
    /// and the message prints what it was given.
    pub raise_a: i64,
    /// The second: the collection's length, or the string's.
    pub raise_b: i64,
}

impl NativeCtx {
    /// A context over `words`, for `host`.
    ///
    /// The answer fields start at values no exit leaves behind —
    /// `raise_code` at zero, which [`Raise::from_abi`] rejects — so a test
    /// that reads one without the matching [`Outcome`] reads an obvious
    /// wrong answer rather than a plausible stale one.
    /// The heap table starts empty — a null pointer and no entries — because a
    /// caller with no heap is a caller whose compiled code touches none, and a
    /// null that is dereferenced is a loud failure where a dangling table would
    /// be a quiet one. [`NativeCtx::over_heap`] is how a caller with a heap
    /// says so.
    ///
    /// `stack_origin` is a parameter and not a builder for the reason the field
    /// itself gives: a wrong one is not a crash but a *wrong address*, resolved
    /// into the wrong task's segment, and there is no value it could default to
    /// that would be right for every caller. Zero is right for the first segment
    /// and for a test that owns its own words, and saying so is one argument.
    pub fn new(host: *mut c_void, words: *mut u64, stack_origin: u64) -> Self {
        NativeCtx {
            host,
            words,
            chunks: std::ptr::null(),
            literals: std::ptr::null(),
            fixed_payload_words: std::ptr::null(),
            stack_origin,
            pending_work: 0,
            raise_code: 0,
            raise_detail: 0,
            raise_pc: u32::MAX,
            raise_a: 0,
            raise_b: 0,
        }
    }

    /// The same context, over the heap `chunks` describes.
    ///
    /// See [`NativeCtx::chunks`]: the table has to stay valid, and to be
    /// re-published by any helper that could have grown it, for as long as the
    /// compiled code is running.
    pub fn over_heap(mut self, chunks: *const *mut u64) -> Self {
        self.chunks = chunks;
        self
    }

    /// The same context, over the literal addresses `literals` begins.
    ///
    /// See [`NativeCtx::literals`]. Unlike [`NativeCtx::over_heap`]'s table this
    /// one never has to be re-published, because nothing a run does changes it —
    /// which is why it is a builder rather than a field a helper writes.
    pub fn over_literals(mut self, literals: *const u64) -> Self {
        self.literals = literals;
        self
    }

    /// The same context, over the table `fixed_payload_words` begins.
    ///
    /// See [`NativeCtx::fixed_payload_words`]. [`NativeCtx::over_literals`]'s
    /// reason: the table is derived once from the program's layouts and never
    /// changes, so it is a builder rather than a field a helper republishes.
    pub fn over_payload_words(mut self, fixed_payload_words: *const u32) -> Self {
        self.fixed_payload_words = fixed_payload_words;
        self
    }

    /// Which error was raised, if the outcome was [`Outcome::Raised`].
    pub const fn raise(&self) -> Option<Raise> {
        Raise::from_abi(self.raise_code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbers are the ABI, so they are asserted rather than assumed.
    ///
    /// A code generator in this crate writes them and a runtime in another
    /// crate reads them, and the two are linked only by these integers. A
    /// variant inserted in the middle of either enum would renumber the rest
    /// silently, which is a wrong runtime error rather than a build failure.
    #[test]
    fn the_abi_numbers_are_fixed() {
        assert_eq!(Outcome::Returned.abi(), 0);
        assert_eq!(Outcome::Raised.abi(), 1);
        assert_eq!(Outcome::Stopped.abi(), 2);

        for (code, raise) in [
            (1, Raise::AddOverflowed),
            (2, Raise::SubOverflowed),
            (3, Raise::MulOverflowed),
            (4, Raise::DivOverflowed),
            (5, Raise::RemOverflowed),
            (6, Raise::DurationOverflowed),
            (7, Raise::DividedByZero),
            (8, Raise::RemainderByZero),
            (9, Raise::Trapped),
            (10, Raise::NullObject),
            (11, Raise::IndexOutOfRange),
            (12, Raise::ByteOffset),
            (13, Raise::Called),
            (14, Raise::NegOverflowed),
        ] {
            assert_eq!(raise.abi(), code);
            assert_eq!(Raise::from_abi(code), Some(raise));
        }
        assert_eq!(Raise::from_abi(0), None);
        assert_eq!(Raise::from_abi(15), None);

        for (code, op) in [
            (0, BufferOp::Alloc),
            (1, BufferOp::AppendByte),
            (2, BufferOp::AppendBytes),
            (3, BufferOp::Finish),
        ] {
            assert_eq!(op.abi(), code);
            assert_eq!(BufferOp::from_abi(code), Some(op));
        }
        assert_eq!(BufferOp::from_abi(4), None);
    }

    /// Zero is not a raise, which is what makes a fresh context's
    /// `raise_code` readable as "nothing was raised".
    #[test]
    fn a_fresh_context_has_raised_nothing() {
        let mut words = [0u64; 4];
        let ctx = NativeCtx::new(std::ptr::null_mut(), words.as_mut_ptr(), 0);
        assert_eq!(ctx.raise(), None);
        assert_eq!(ctx.pending_work, 0);
        assert_eq!(ctx.stack_origin, 0);
    }

    /// Both tables start null, and both builders publish only their own.
    ///
    /// A context whose compiled code holds no literal must not be given a
    /// plausible-looking table, and publishing one must not disturb the other —
    /// which is a thing to assert rather than to read, because the two fields are
    /// adjacent and are written by two one-line builders.
    #[test]
    fn a_fresh_context_has_no_tables() {
        let mut words = [0u64; 4];
        let ctx = NativeCtx::new(std::ptr::null_mut(), words.as_mut_ptr(), 0);
        assert!(ctx.chunks.is_null());
        assert!(ctx.literals.is_null());
        assert!(ctx.fixed_payload_words.is_null());

        let addrs = [7u64, 9];
        let ctx = ctx.over_literals(addrs.as_ptr());
        assert!(ctx.chunks.is_null());
        assert_eq!(unsafe { *ctx.literals.add(1) }, 9);
        assert!(ctx.fixed_payload_words.is_null());

        let widths = [0u32, 3];
        let ctx = ctx.over_payload_words(widths.as_ptr());
        assert_eq!(unsafe { *ctx.fixed_payload_words.add(1) }, 3);
    }
}
