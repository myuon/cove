//! The boundary between compiled code and the runtime that called it.
//!
//! Everything in this module is compiled whether or not the `native` feature
//! is on, and that is deliberate: the ABI is a contract between two crates,
//! not a Cranelift artefact. A build with no code generator still has the
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
//! # There are no references here yet
//!
//! This slice compiles only scalar slots — [`cove_ir::Repr::Unit`],
//! [`Bool`](cove_ir::Repr::Bool), [`Int`](cove_ir::Repr::Int),
//! [`Float`](cove_ir::Repr::Float), [`Duration`](cove_ir::Repr::Duration) and
//! [`Tag`](cove_ir::Repr::Tag). A function with a
//! [`Repr::Ref`](cove_ir::Repr::Ref) slot in its frame is refused outright.
//!
//! **So there is nothing to spill, and that is why the collector question
//! does not arise — not because it has been answered.** A compiled function
//! here holds no reference in a register because it holds no reference at
//! all. The moment allocation, object fields or strings are lowered, ADR
//! 0055's "Collection uses the VM stack as the first root map" becomes real
//! work: every live reference materialised in its slot before the safepoint,
//! and every reference reloaded after it. None of that is implemented, and
//! nothing here should be read as evidence that it is.
//!
//! [ADR 0034]: ../../../../docs/adr/0034-one-physical-word-stack.md

use std::ffi::c_void;

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
}

impl NativeCtx {
    /// A context over `words`, for `host`.
    ///
    /// The three answer fields start at values no exit leaves behind —
    /// `raise_code` at zero, which [`Raise::from_abi`] rejects — so a test
    /// that reads one without the matching [`Outcome`] reads an obvious
    /// wrong answer rather than a plausible stale one.
    pub fn new(host: *mut c_void, words: *mut u64) -> Self {
        NativeCtx {
            host,
            words,
            pending_work: 0,
            return_slot: u32::MAX,
            raise_code: 0,
            raise_detail: 0,
        }
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
        ] {
            assert_eq!(raise.abi(), code);
            assert_eq!(Raise::from_abi(code), Some(raise));
        }
        assert_eq!(Raise::from_abi(0), None);
        assert_eq!(Raise::from_abi(10), None);
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
