//! The execution backend [ADR 0034](../../../../docs/adr/0034-one-physical-word-stack.md)
//! decides, being built alongside the one it replaces.
//!
//! [`docs/LINEAR_VM.md`](../../../../docs/LINEAR_VM.md) is the design. It is a
//! clean-room replacement rather than a renovation: nothing here is derived
//! from [`crate::vm`], [`crate::frame`] or `cove_ir`, and this module imports
//! from none of them. Those are frozen — fixed only where a fix keeps the
//! oracle and the differential gate usable — and deleted at the cutover, when
//! `cove-lir` and `lvm` take the names `cove-ir` and `vm`.
//!
//! The memory and the dispatch loop exist; the boundary above them — the
//! type an embedder holds, which materialises a public `Value` on the way in
//! and out — does not yet. That is why the whole module is allowed to be dead
//! code: every item below is reached from its own tests and from nothing else.
//! The allowance comes out with that boundary, and it is deliberately one line
//! in one place rather than an attribute per item, so that removing it is a
//! single edit whose failure lists exactly what is still unused.
#![allow(dead_code)]

pub(crate) mod boundary;
#[cfg(test)]
mod differential;
pub(crate) mod exec;
pub(crate) mod mem;
