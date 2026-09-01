//! The two things that stop a lowering, kept together so the temporary one
//! is easy to find and delete.
//!
//! There is no `Unsupported` here and no admission predicate: the target is
//! that **every valid checked program lowers**, so a construct the lowering
//! meets and cannot emit code for is a hole in this crate rather than a
//! program the backend declines. What follows are the only two ways `lower`
//! can answer `Err`, and they are different in kind.
//!
//! # A `Ty::Unknown` is a compile error
//!
//! It is the checker declining, and a program the checker declined about
//! should not have reached a backend at all. `docs/LINEAR_VM.md` states the
//! distinction the predecessor lost: erasure is for a type that is
//! *intentionally* erased — `dyn Trait`, a Host result a schema declared
//! `Any` — and an unknown is not that. Turning one into runtime dispatch
//! would make "the checker was unsure" and "the program said any" the same
//! thing, and only one of them is a program.
//!
//! # A gap is a promise, not a refusal
//!
//! [`gap`] marks a construct this lowering has not been *taught yet*. Every
//! one of them is scheduled to be removed by a later task — the heap, then
//! closures, then hosts — and none of them is a decision about the language.
//! They are all raised through this one function so that `grep` over this
//! crate answers what is left, and so that removing one is deleting a call
//! rather than unpicking a taxonomy.
//!
//! That is why a gap is not a variant of anything and carries no code of its
//! own beyond the shared one: a per-construct refusal code is what a
//! consumer starts matching on, and then the temporary thing has become an
//! interface.

use cove_diag::{Diagnostic, Span};
use cove_sema::typeck::Ty;

/// The construct is in the language but not yet in this lowering.
pub(crate) const NOT_YET_LOWERED: &str = "cove::lower::not_yet_lowered";

/// The checker settled no type here, so there is nothing to emit.
pub(crate) const UNKNOWN_TYPE: &str = "cove::lower::unknown_type";

/// Something this lowering has not been taught, named as the source writes
/// it.
pub(crate) fn gap(what: &str, span: Span) -> Diagnostic {
    Diagnostic::error(NOT_YET_LOWERED, format!("not yet lowered: {what}"))
        .at(span)
        .rule("Every valid checked program lowers; a construct this backend has not been taught is a gap in the backend.")
        .help("this is an internal gap in the linear-memory lowering, not a fault in the program")
}

/// The checker declined to type this expression, and a backend cannot make
/// up what it would not say.
pub(crate) fn unknown(ty: &Ty, span: Span) -> Diagnostic {
    Diagnostic::error(
        UNKNOWN_TYPE,
        format!("the type of this expression was never settled, so it cannot be lowered: `{ty}`"),
    )
    .at(span)
    .rule("A value's type decides the one word it occupies, and an unknown decides nothing.")
    .help("give this expression a type the checker can settle, with an annotation or a more specific argument")
}
