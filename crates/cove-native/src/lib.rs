//! Cove's native execution tier: one [`cove_ir::Function`] in, one run of
//! machine code out.
//!
//! [ADR 0055] decides that Cove compiles **optimized executable IR one
//! function at a time**, with Cranelift as the first code generator and
//! Cove's IR and runtime ABI — not Cranelift's API — as the semantic
//! boundary. This crate is that lowering and nothing else. It does not
//! execute a program, own a heap, decide which functions to compile, or know
//! that a VM exists.
//!
//! # The dependency edge runs one way, and the calls run both
//!
//! Compiled code cannot be self-contained. Allocation, collection, Host
//! calls, strings, buffers, runtime errors and the safepoint's three-step
//! stop order are all correctness that already lives in Rust, and ADR 0055
//! keeps them there: "Native code replaces dispatch and scalar execution
//! before it duplicates mature runtime machinery." So the code this crate
//! emits has to call *into* `cove-runtime`.
//!
//! But `cove-runtime` is what will select, cache and enter a compiled
//! function, so `cove-runtime` depends on this crate. A Cargo dependency
//! edge back would be a cycle, and cargo would refuse it.
//!
//! The inversion is therefore explicit rather than accidental: the runtime's
//! half of the boundary arrives as **a table of function pointers**
//! ([`NativeHelpers`]) handed to `Jit::new`, and the code generator binds
//! those addresses into the compiled code as ordinary relocations. There is
//! no trait object to dispatch through at run time, no `dyn` in the hot path,
//! and — the point — no Cargo dependency on `cove-runtime` anywhere in this
//! crate's manifest. Two crates call each other; one of them knows the
//! other's name.
//!
//! What that costs is that the helpers are `extern "C"` functions taking a
//! `*mut c_void` host pointer rather than `&mut Machine`, and that the errors
//! they stand for are named by the small [`Raise`] enum rather than
//! constructed here. Both of those are the price of the inversion and both
//! are cheap; the alternative — moving the runtime's allocation and stop
//! machinery into this crate so that the edge could run the other way — is
//! not.
//!
//! # Two code generators, both off by default
//!
//! ADR 0055's adoption gate asks that "a build without the native feature has
//! no executable-memory dependency". That is a claim about the dependency
//! graph, so every code generator's edge is optional and off by default; see
//! this crate's manifest. Without them, what remains is
//! [`mod@abi`] — the declarations the runtime side is written against — and
//! `cargo tree` shows no `cranelift-jit`, which is the crate that maps an
//! executable page.
//!
//! The `cranelift` feature is the code generator ADR 0055 names. The
//! `template` feature is a second one, a hand-written x86-64 template
//! compiler, and it exists to be *measured against* the first: which code
//! generator Cove adopts is a question a comparison on identical optimized IR
//! answers and an argument does not. Both arms compile exactly the same subset
//! of the IR — a comparison over two different subsets would not be one — and
//! both are entered through the same [`Entry`] over the same [`NativeCtx`].
//!
//! ADR 0056 then decided between them on the measurements, and the template arm
//! won: `cove_runtime::native` compiles with that one, and
//! `cove run --backend native` runs what it emits. The Cranelift arm is kept for
//! the comparison ADR 0056 keeps, and nothing selects it as a tier.
//!
//! # What it compiles today, and what it refuses
//!
//! Everything here is the *first* bullet list of ADR 0055's adoption gate and
//! deliberately not one instruction more:
//!
//! - integer and Boolean constants ([`Inst::Int`](cove_ir::Inst::Int),
//!   [`Inst::Bool`](cove_ir::Inst::Bool));
//! - integer arithmetic ([`Inst::Arith`](cove_ir::Inst::Arith),
//!   [`Inst::ArithImm`](cove_ir::Inst::ArithImm)), with the encoded tier's
//!   overflow and division behaviour exactly;
//! - comparisons ([`Inst::Cmp`](cove_ir::Inst::Cmp),
//!   [`Inst::CmpImm`](cove_ir::Inst::CmpImm)) and [ADR 0054]'s fused
//!   [`Inst::CmpBranch`](cove_ir::Inst::CmpBranch) and
//!   [`Inst::CmpImmBranch`](cove_ir::Inst::CmpImmBranch);
//! - slot copies ([`Inst::Copy`](cove_ir::Inst::Copy));
//! - [`Inst::Jump`](cove_ir::Inst::Jump) and
//!   [`Inst::BranchFalse`](cove_ir::Inst::BranchFalse);
//! - [`Inst::Return`](cove_ir::Inst::Return);
//! - [`Inst::Trap`](cove_ir::Inst::Trap).
//!
//! And then, for the second raced slice, exactly what
//! `examples/covefmt`'s `wantsASpaceBetween` and `byteOfPunct` need and not one
//! instruction more — which was settled by lowering the two and reading the
//! listing, not by guessing:
//!
//! - [`Repr::Ref`](cove_ir::Repr::Ref) frame slots, and so `String` and `Array`
//!   parameters;
//! - [`Inst::Tag`](cove_ir::Inst::Tag) and
//!   [`Inst::Switch`](cove_ir::Inst::Switch), which are an enum's discriminant
//!   and the `match` over it;
//! - [`Compare::Tag`](cove_ir::Compare::Tag) equality, which is
//!   `token.kind == Kind.Punct`;
//! - [`Inst::Not`](cove_ir::Inst::Not);
//! - [`Inst::Len`](cove_ir::Inst::Len),
//!   [`Inst::LoadElem`](cove_ir::Inst::LoadElem) and
//!   a byte [`Inst::RunLoad`](cove_ir::Inst::RunLoad), which are the three heap reads —
//!   each with the bounds check, the tag range and the null refusal
//!   `cove_runtime::vm::exec::encoded` performs, because a native `load-elem`
//!   that skips a check the VM makes is a wrong answer and not a fast one;
//! - [`Inst::Call`](cove_ir::Inst::Call), which is handed to
//!   [`NativeHelpers::call`] whole.
//!
//! **A function containing any other instruction compiles to `None`.** Not
//! partly: `Jit::compile` answers `None` and the caller runs the whole
//! function on the encoded VM, which is ADR 0055's "The initial
//! implementation does not split one function into native and interpreted
//! regions."
//!
//! Allocation was deliberately not on that list while the two arms were being
//! raced. ADR 0055 keeps it a runtime helper, so it is *one identical call in
//! both arms* — which cannot separate two code generators, and a subset made of
//! such calls would have measured the runtime and reported it as a
//! code-generator difference.
//!
//! ADR 0056 settled the race, and that reason stopped applying with it. Since
//! then the subset has taken [`Inst::Alloc`](cove_ir::Inst::Alloc) and [ADR
//! 0052]'s four growable-buffer instructions — see [`AllocFn`] and [`GrowableFn`]
//! — and what they buy is not a faster allocation but a **compiled function
//! around one**. A body refused for its single `growable-alloc` ran every other
//! instruction it had on the encoded tier.
//!
//! `Jit` and `Jit::compile` are named above without links on purpose: they
//! exist only under a code generator's feature, and an intra-doc link to an
//! item a default build does not have is a broken link — which `RUSTDOCFLAGS="-D
//! warnings"` turns into a failed `cargo doc`, as it did once while this was
//! being written.
//!
//! [ADR 0052]: ../../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md
//! [ADR 0054]: ../../../docs/adr/0054-a-comparison-that-only-feeds-a-branch-is-the-branch.md
//! [ADR 0055]: ../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md

pub mod abi;

pub use abi::{
    AllocFn, BuiltinFn, CallFn, CloseFn, Entry, GrowableFn, GrowableOp, NativeCtx, NativeHelpers,
    OpenFn, Opened, Outcome, Raise, RunCopyFn, RunOp, SafepointFn, HEAP_CHUNK_SHIFT,
    HEAP_CHUNK_WORDS, HEAP_ORIGIN_WORDS,
};

/// Native execution is not available here.
///
/// ADR 0055's "Executable memory is optional, not assumed": a target that
/// prohibits or cannot provide executable memory, or that a code generator has
/// not been written for, produces a capability diagnostic. It does not produce
/// an attempted fallback to something else — the caller's fallback is the
/// encoded VM, which is a complete execution path and not a fallback at all.
///
/// One type for both arms, and declared whether or not either is compiled: a
/// refusal is a fact about the *host*, and the two arms refuse different hosts
/// for the same reason.
#[derive(Debug)]
pub struct Unavailable(String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native execution is unavailable: {}", self.0)
    }
}

impl std::error::Error for Unavailable {}

impl Unavailable {
    /// A refusal naming `reason`.
    ///
    /// Public because the *runtime* refuses hosts this crate cannot see. A
    /// build with no code generator has nothing here that could refuse
    /// anything, and "this build has no code generator" is exactly the
    /// capability diagnostic ADR 0055 asks for — so the type has to be
    /// constructible from outside, or a second type would say the same thing
    /// in different words.
    pub fn new(reason: impl Into<String>) -> Unavailable {
        Unavailable(reason.into())
    }
}

#[cfg(any(feature = "cranelift", feature = "template"))]
pub mod subset;

#[cfg(any(feature = "cranelift", feature = "template"))]
pub use subset::{blockers, refusal, supported, Reason, Refusal};

#[cfg(feature = "cranelift")]
mod compile;

#[cfg(feature = "cranelift")]
pub use compile::{Compiled, Jit};

#[cfg(feature = "template")]
pub mod template;
