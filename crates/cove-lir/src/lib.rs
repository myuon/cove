//! The executable IR Cove is lowered to: one linear memory, one word stack,
//! one slot numbering.
//!
//! [ADR 0034](../../../docs/adr/0034-one-physical-word-stack.md) decides what
//! this is, and [`docs/LINEAR_VM.md`](../../../docs/LINEAR_VM.md) writes the
//! design out at the level an implementer needs. The short version:
//!
//! - There is **one linear memory**, addressed in eight-byte words. The stack
//!   region is reserved at `[0, STACK_WORDS)`; the heap region begins at
//!   `STACK_WORDS`. An address is a word index into that one space, so
//!   nothing changes value when the two regions are later placed in one
//!   block.
//! - There is **one stack region**. A frame is a run of words, named by a
//!   `frame_base`, and a slot is `memory[frame_base + slot]`. Parameters,
//!   locals, temporaries and captures share the numbering.
//! - A word is **untagged**. What it means comes from [`Repr`], and the only
//!   question the collector asks the static side is which slots are
//!   [`Repr::Ref`].
//! - The IR is a **register machine**. Every instruction names its operands
//!   and destination by slot; there is no operand stack.
//! - A **place is a one-word address**. There is no place object, no place
//!   stack and no table of places.
//! - The public `Value` is a **boundary**, not a store. It is materialised at
//!   Host calls, entry results and trace captures, and nowhere else.
//!
//! # This is not a continuation of anything
//!
//! It is a clean-room replacement. No instruction, storage region, admission
//! predicate or naming convention is carried over from the IR it replaces,
//! and nothing here exists to be compatible with it. In particular there is
//! no `Unsupported`: a lowering that met something it had not been taught
//! would be a bug in the lowering, not a program the backend declines. The
//! predecessor's per-refusal extension mechanism is exactly what ADR 0034
//! forbids reconstructing.
//!
//! # This is a lowering, not a second source of truth
//!
//! `cove-sema` has already answered what every reference denotes and what
//! every expression's type is. Nothing here re-derives that; it records the
//! answers in a shape the machine can act on without asking again. Where the
//! two could disagree, the checker is right by construction, because the
//! lowering reads its answers rather than recomputing them.

pub mod inst;
pub mod layout;
pub mod lower;
pub mod print;
pub mod program;
pub mod repr;
pub mod verify;

pub use inst::{ArithOp, CmpOp, Compare, Convert, Inst, Len, Num, Pc, Slot};
pub use layout::{Case, Field, Layout, LayoutId, Shape};
pub use lower::lower;
pub use program::{
    ArgsId, Builtin, BuiltinId, Capture, Function, FunctionId, HostOp, HostOpId, Program, StrId,
    Table, TableId,
};
pub use repr::{RefMap, Repr};
pub use verify::{verify, Invalid};
