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
//!
//! # An unbounded instantiation is neither
//!
//! There is a third code here and it is deliberately not a gap, because a
//! later task does not remove it. `fn f<T>(x: T) { f(Cell(x)) }` checks —
//! [ADR 0035](../../../../docs/adr/0035-a-value-type-may-not-contain-itself.md)
//! is about a *declaration*'s layout and `Cell<Cell<Int>>` is finite — and it
//! asks for `f<Int>`, `f<Cell<Int>>`, `f<Cell<Cell<Int>>>` and so on without
//! end. Every one of them is a different width, so monomorphisation cannot
//! answer with one copy, and `docs/LINEAR_VM.md` says why nothing else can
//! answer at all: a frame's per-slot `Repr` map is static.
//!
//! So this is a program the backend declines, and it is the one thing in this
//! crate that is. It is written down as its own code rather than as a gap
//! because a reader who meets it has something to do about it — the language
//! already has `dyn Trait` for the case where one copy of the code is
//! wanted — and because a compiler that does not terminate is worse than one
//! that refuses.

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

/// A generic that instantiates itself at a type built from its own
/// parameter, which has no finite monomorphisation.
pub(crate) const INSTANTIATION_DEPTH: &str = "cove::lower::instantiation_depth";

/// How many instantiations may be open at once.
///
/// The bound exists because monomorphisation is the only representation this
/// machine admits and a chain that grows a type at every step has no fixed
/// point. Where to put it is a judgement rather than a derivation, and this
/// is the one made: **eight**, because an instantiation asked for from inside
/// an instantiation asked for from inside an instantiation is already a shape
/// no program in the corpus writes — the deepest chain there is one — and
/// eight leaves that room several times over while still answering a runaway
/// in the time it takes to lower eight functions.
///
/// The number is not part of the language, the IR or any public API, for the
/// reason `docs/LINEAR_VM.md` gives about the stack limit: it is what this
/// implementation can afford, and a program that depends on it is depending
/// on the wrong thing.
pub(crate) const MAX_DEPTH: usize = 8;

/// The instantiation chain ran past [`MAX_DEPTH`], named step by step.
///
/// The chain rather than the count, because the count says only that
/// something grew and the chain says what grew it: the first line a reader
/// needs is which call made the type one deeper than the one that called it.
pub(crate) fn too_deep(chain: &[String], span: Span) -> Diagnostic {
    Diagnostic::error(
        INSTANTIATION_DEPTH,
        format!(
            "this call instantiates a generic more than {MAX_DEPTH} deep, so there is no finite \
             set of functions to lower it to:\n  {}",
            chain.join("\n  ")
        ),
    )
    .at(span)
    .rule("A generic is lowered to one function per instantiation, because a value's width depends on its type argument and a frame's reference map is static.")
    .help("break the chain, or take the argument as a `dyn Trait`, which is one function for every type that conforms to it")
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
