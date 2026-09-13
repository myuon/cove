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
//! extern "C" fn(ctx: *mut NativeCtx, base: u64) -> Outcome
//! ```
//!
//! [`Entry`] is that signature. Two arguments and a small integer result,
//! and every one of the four choices in it is load-bearing:
//!
//! - **`ctx` is a pointer to mutable per-call state**, not a set of separate
//!   arguments, because everything the callee has to tell the caller that is
//!   not "which of three ways did you leave" is a field of it: which slot
//!   holds the answer, which runtime error to raise, how much unpaid work to
//!   charge. Adding one of those later costs a field rather than a new
//!   signature, and a signature is what both tiers are pinned to.
//! - **`base` is `u64` and is a word index**, for the reason above. It is not
//!   the linear address `Memory::push_frame` answers, and it is not a
//!   pointer.
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
//! boundary**, so at the two places a collection can happen — the safepoint
//! helper and the call helper, which are the only calls either arm emits —
//! every live reference is already in the slot the frame's static
//! `Function::refs` map names. The collector walks exactly what it walks for
//! an encoded frame, and there is no spill sequence, because there is nothing
//! anywhere else to spill.
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
//! [ADR 0034]: ../../../../docs/adr/0034-one-physical-word-stack.md

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
/// # Safety
///
/// The caller promises that `ctx` is a valid, uniquely borrowed
/// [`NativeCtx`]; that `ctx.words` points at the first word of the segment
/// `base` indexes into; that the frame at `base` is at least
/// `Function::frame_size()` words long; and that the parameters occupy
/// `[0, width)` of it in declaration order. The callee promises to touch no
/// word outside that frame.
pub type Entry = unsafe extern "C" fn(ctx: *mut NativeCtx, base: u64) -> Outcome;

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
    /// The answer is at slot [`NativeCtx::return_slot`] of the frame, and it
    /// is `Function::returns`' width wide. The caller copies it out, exactly
    /// as the encoded `RETURN` arm copies `width` words from `base + src` to
    /// `caller_base + dst`.
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
    /// What `encoded.rs`'s `LEN`, `BYTE_AT` and `Machine::element` each answer
    /// for a zero address, in that one word: the message is one sentence with
    /// no operand in it, so this variant carries nothing.
    NullObject = 10,
    /// `Machine::element`'s "index {a} is outside a collection of {b}", where
    /// the two numbers are [`NativeCtx::raise_a`] and [`NativeCtx::raise_b`].
    IndexOutOfRange = 11,
    /// `encoded.rs`'s `BYTE_AT` refusal: "`byteAt` is `{a}`, and a byte offset
    /// into this string is 0 to `{b} - 1`".
    ///
    /// `raise_b` is the string's byte length rather than the last legal offset,
    /// because that is the number the encoded arm has in hand and subtracts
    /// from; doing the subtraction here would put the `- 1` in two places.
    ByteOffset = 12,
    /// A callee raised, and the runtime already holds the error.
    ///
    /// The one variant that names no message, because there is none to name:
    /// [`NativeHelpers::call`] ran a callee which failed, and what failed is a
    /// whole `RuntimeError` — a span, a rule, a call chain — that the runtime
    /// built and kept. Compiled code learns only that it must leave, and
    /// leaves; the caller re-raises what it stashed.
    ///
    /// This is the same division as every other variant here, taken to its
    /// end: this crate names errors and never builds one.
    Called = 13,
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
///   return width, which is the callee's declaration's to know.
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
    /// Which slot holds the answer, when the outcome is
    /// [`Outcome::Returned`].
    ///
    /// A run-time field rather than a compile-time fact about the function,
    /// because a function may have several `Inst::Return`s naming different
    /// slots. The width is static — `Function::returns` — and is the
    /// caller's to look up.
    pub return_slot: u32,
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
    /// The three answer fields start at values no exit leaves behind —
    /// `raise_code` at zero, which [`Raise::from_abi`] rejects — so a test
    /// that reads one without the matching [`Outcome`] reads an obvious
    /// wrong answer rather than a plausible stale one.
    /// The heap table starts empty — a null pointer and no entries — because a
    /// caller with no heap is a caller whose compiled code touches none, and a
    /// null that is dereferenced is a loud failure where a dangling table would
    /// be a quiet one. [`NativeCtx::over_heap`] is how a caller with a heap
    /// says so.
    pub fn new(host: *mut c_void, words: *mut u64) -> Self {
        NativeCtx {
            host,
            words,
            chunks: std::ptr::null(),
            pending_work: 0,
            return_slot: u32::MAX,
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
        ] {
            assert_eq!(raise.abi(), code);
            assert_eq!(Raise::from_abi(code), Some(raise));
        }
        assert_eq!(Raise::from_abi(0), None);
        assert_eq!(Raise::from_abi(14), None);
    }

    /// Zero is not a raise, which is what makes a fresh context's
    /// `raise_code` readable as "nothing was raised".
    #[test]
    fn a_fresh_context_has_raised_nothing() {
        let mut words = [0u64; 4];
        let ctx = NativeCtx::new(std::ptr::null_mut(), words.as_mut_ptr());
        assert_eq!(ctx.raise(), None);
        assert_eq!(ctx.pending_work, 0);
    }
}
