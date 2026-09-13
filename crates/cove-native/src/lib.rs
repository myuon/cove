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
//! # `native` is off by default
//!
//! ADR 0055's adoption gate asks that "a build without the native feature has
//! no executable-memory dependency". That is a claim about the dependency
//! graph, so the *whole Cranelift edge* is optional and off by default; see
//! this crate's manifest. Without the feature, what remains is
//! [`mod@abi`] — the declarations the runtime side is written against — and
//! `cargo tree` shows no `cranelift-jit`, which is the crate that maps an
//! executable page.
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
//! **A function containing any other instruction compiles to `None`.** Not
//! partly: `Jit::compile` answers `None` and the caller runs the whole
//! function on the encoded VM, which is ADR 0055's "The initial
//! implementation does not split one function into native and interpreted
//! regions." Calls are absent from that list, so this slice compiles leaf
//! functions only — the entry table that makes VM-to-native and
//! native-to-native calls work is the next slice's, and nothing here
//! prejudges it.
//!
//! `Jit` and `Jit::compile` are named above without links on purpose: they
//! exist only under the `native` feature, and an intra-doc link to an item a
//! default build does not have is a broken link — which `RUSTDOCFLAGS="-D
//! warnings"` turns into a failed `cargo doc`, as it did once while this was
//! being written.
//!
//! [ADR 0054]: ../../../docs/adr/0054-a-comparison-that-only-feeds-a-branch-is-the-branch.md
//! [ADR 0055]: ../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md

pub mod abi;

pub use abi::{Entry, NativeCtx, NativeHelpers, Outcome, Raise, SafepointFn};

#[cfg(feature = "native")]
mod compile;

#[cfg(feature = "native")]
pub use compile::{Compiled, Jit, Unavailable};
