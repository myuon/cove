//! Cove's native execution tier: one [`cove_ir::Function`] in, one run of
//! machine code out.
//!
//! [ADR 0055] decides that Cove compiles **optimized executable IR one
//! function at a time**, with Cove's IR and runtime ABI — not a code
//! generator's API — as the semantic boundary. This crate is that lowering and
//! nothing else. It does not execute a program, own a heap, decide which
//! functions to compile, or know that a VM exists.
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
//! # One code generator, off by default
//!
//! ADR 0055's adoption gate asks that "a build without the native feature has
//! no executable-memory dependency". That is a claim about the dependency
//! graph, so the code generator's edge is optional and off by default; see
//! this crate's manifest. Without it, what remains is [`mod@abi`] — the
//! declarations the runtime side is written against — and `cargo tree` names
//! nothing that maps an executable page.
//!
//! The `template` feature is the code generator: a hand-written x86-64
//! template compiler. It was not chosen by argument. ADR 0055 named Cranelift,
//! [ADR 0056] built both against the same lowered IR, the same ABI and one
//! shared subset predicate and raced them, and the hand-written arm won on
//! compile latency by 69×, on the stripped bundle by 140× and on the
//! dependency graph by 38 crates, with execution within a few per cent in both
//! directions. [ADR 0066] then retired the loser: the comparison has been made
//! and is recorded, and what remained of it was a second lowering per new IR
//! instruction that no distributed build contains.
//!
//! So `cove_runtime::native` compiles with this one, `cove run --backend
//! native` runs what it emits, and there is no second arm to select.
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
//! Allocation was deliberately not on that list while ADR 0056's two
//! candidates were being raced. ADR 0055 keeps it a runtime helper, so it was
//! *one identical call in both arms* — which cannot separate two code
//! generators, and a subset made of such calls would have measured the runtime
//! and reported it as a code-generator difference.
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
//! [ADR 0056]: ../../../docs/adr/0056-the-first-code-generator-is-the-one-that-was-cheaper-everywhere.md
//! [ADR 0066]: ../../../docs/adr/0066-a-comparison-ends-when-its-question-is-answered.md

pub mod abi;

pub use abi::{
    AllocFn, CallFn, CloseFn, Entry, GrowableFn, GrowableOp, IntrinsicFn, IntrinsicProtocol,
    NativeCtx, NativeHelpers, OpenFn, Opened, OrderStrFn, Outcome, Raise, RunCopyFn, RunOp,
    SafepointFn, HEAP_CHUNK_SHIFT, HEAP_CHUNK_WORDS, HEAP_ORIGIN_WORDS,
};

/// Native execution is not available here.
///
/// ADR 0055's "Executable memory is optional, not assumed": a target that
/// prohibits or cannot provide executable memory, or that a code generator has
/// not been written for, produces a capability diagnostic. It does not produce
/// an attempted fallback to something else — the caller's fallback is the
/// encoded VM, which is a complete execution path and not a fallback at all.
///
/// Declared whether or not a code generator is compiled, because a refusal is a
/// fact about the *host* and a build with no code generator refuses every host
/// there is.
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

/// Machine code charged to [ADR 0062]'s buffer windows, one row per
/// [`Pattern`](cove_ir::legalize::Pattern).
///
/// [Issue #423](https://github.com/myuon/cove/issues/423) asks for "generated
/// machine-code bytes attributable to each buffer-window shape", and this is
/// the carrier: one of these per compiled function, summed over a program by
/// whoever compiled it. Before it, code size was a whole-program total and
/// nothing said what any part of it was for — which is not enough to answer
/// whether ADR 0062's +39.8% on cq is the windows or the four standard-library
/// functions the ADR says it newly compiled.
///
/// # Why the bytes are an `Option` and the sites are not
///
/// A window is a **shape in the IR**. How many of each pattern a function
/// emitted is therefore a fact about `cove_ir::legalize` and the lowering
/// rather than about a code generator; `sites` is never in doubt.
///
/// The bytes are another matter, and not every code generator can honestly
/// report them. The template compiler emits a whole window — hot path, both
/// cold blocks and the join — as one contiguous run of its code buffer, so a
/// difference of two buffer lengths taken across the emission *is* that
/// window's machine code, to the byte. A generator that hands an IR-shaped
/// region to a backend which orders, merges and lays out blocks at the end of
/// the function has no range of its buffer corresponding to an IR window, and
/// can only answer `None` — not a zero, which a reader would take for "windows
/// cost this generator nothing".
///
/// **Today the one code generator answers `Some` and nothing produces the
/// `None`.** It was the retired Cranelift arm that produced it, and the arm is
/// gone; the variant is kept rather than collapsed because what it encodes is
/// "this figure may be unattributable", which is a property of a *layout* and
/// not of a particular backend, and because [ADR 0066] retires an arm without
/// deciding that no future one exists. Collapsing it is a change to the
/// boundary report and belongs with whatever asks for it.
///
/// A `None` is infectious through [`WindowCode::charge`] and
/// [`WindowCode::add`] for the same reason: a sum over *some* of a program's
/// windows reads as a sum over all of them.
///
/// [ADR 0062]: ../../../docs/adr/0062-an-append-is-ensure-store-commit.md
/// [ADR 0066]: ../../../docs/adr/0066-a-comparison-ends-when-its-question-is-answered.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowCode {
    /// Bytes of machine code emitted for each pattern's windows, indexed by
    /// [`Pattern::index`](cove_ir::legalize::Pattern::index), or `None` from a
    /// code generator that cannot attribute them. See this type's own note for
    /// which generator that is and why.
    pub bytes: Option<[u64; 4]>,
    /// How many windows of each pattern were emitted, indexed the same way.
    ///
    /// Carried beside the bytes rather than left for a reader to find
    /// elsewhere, because bytes *per window* is the number a code-size policy
    /// turns on, and a ratio taken from two figures in two reports is one a
    /// reader gets wrong.
    pub sites: [u64; 4],
}

impl Default for WindowCode {
    /// Nothing emitted yet, and the bytes attributable.
    ///
    /// `Some([0; 4])` rather than `None`: a function with no window in it has
    /// nought bytes of window code, and that is a measurement rather than an
    /// absence. A generator that cannot attribute anything says so by charging
    /// `None`, which is the first window's business and not this value's.
    fn default() -> WindowCode {
        WindowCode {
            bytes: Some([0; 4]),
            sites: [0; 4],
        }
    }
}

impl WindowCode {
    /// Records one emitted window of `pattern`, with the bytes it was where the
    /// code generator can say.
    ///
    /// `None` for the bytes counts the site and drops the whole byte table, for
    /// the reason on the type: a partial sum is indistinguishable from a total.
    pub fn charge(&mut self, pattern: cove_ir::legalize::Pattern, bytes: Option<u64>) {
        self.sites[pattern.index()] += 1;
        self.bytes = match (self.bytes, bytes) {
            (Some(mut rows), Some(count)) => {
                rows[pattern.index()] += count;
                Some(rows)
            }
            _ => None,
        };
    }

    /// Adds one function's windows to a running total.
    pub fn add(&mut self, other: &WindowCode) {
        for (at, sites) in self.sites.iter_mut().enumerate() {
            *sites += other.sites[at];
        }
        self.bytes = match (self.bytes, other.bytes) {
            (Some(mut rows), Some(theirs)) => {
                for (at, row) in rows.iter_mut().enumerate() {
                    *row += theirs[at];
                }
                Some(rows)
            }
            _ => None,
        };
    }

    /// Every pattern's bytes together, or `None` where they are not attributed.
    pub fn total_bytes(&self) -> Option<u64> {
        self.bytes.map(|rows| rows.iter().sum())
    }

    /// Every pattern's windows together.
    pub fn total_sites(&self) -> u64 {
        self.sites.iter().sum()
    }
}

/// Machine code charged to [ADR 0064]'s mediated intrinsic calls, one row per
/// [`Intrinsic`](cove_ir::Intrinsic) variant.
///
/// [ADR 0064]'s Decision 7 asks for "machine-code bytes attributable to
/// intrinsic calls, beside the window bytes ADR 0063 already reports", and this
/// is the carrier: one of these per compiled function, summed over a program by
/// whoever compiled it. It is [`WindowCode`] one level over, and deliberately
/// the same shape, because it is read for the same reason — a whole-program byte
/// count says how large a program's machine code is and nothing about what any
/// part of it is *for*, and ADR 0064's argument is that the call sequences
/// counted here are work the native tier is structurally unable to speed up.
/// What fraction of a program they are is the figure that turns that argument
/// into a measurement.
///
/// # Why the rows are per variant
///
/// Decision 7 asks for attribution "per variant" throughout — allocations,
/// allocated words and proportional work as well as these bytes — and ADR 0064's
/// own census says why: `String.length` is 405,588 of covefmt's 415,809 mediated
/// calls, and `Float.parse` 60,000 of cq's 140,092. A single total is therefore
/// a number about whichever one or two variants happen to dominate, wearing the
/// name of all 31. A migration is decided one variant at a time, so a report it
/// is judged against has to answer one variant at a time.
/// [`Intrinsic::index`](cove_ir::Intrinsic::index) numbers the rows and
/// `cove_ir::intrinsic::COUNT` is how many there are — taken from
/// `cove_ir::intrinsic::ALL` rather than written as a numeral here, because
/// Decision 1 says the variant set only shrinks and this table's width shrinks
/// with it.
///
/// # Why the bytes are an `Option` and the sites are not
///
/// The division [`WindowCode`] makes, for the reason it makes it, restated
/// rather than cross-referenced because the two halves are easy to conflate.
///
/// A site is an **instruction in the IR**. How many `Inst::IntrinsicCall`s of
/// each variant a function emitted code for is a fact about the lowering rather
/// than about a code generator, and `sites` is never in doubt.
///
/// The bytes are a fact about a code generator's layout, and not every
/// generator can honestly report them. The template compiler emits a whole
/// intrinsic call — the hand-over, the indirect call, and the outcome test with
/// the leave it guards — as one contiguous run of its code buffer, so a
/// difference of two buffer lengths taken across the emission *is* that call's
/// machine code, to the byte. A generator whose blocks are ordered, merged and
/// laid out at the end of the function has no range of its buffer that is one
/// call's, and can only answer `None` — not a zero, which a reader would take
/// for "intrinsic calls cost this generator nothing".
///
/// `None` has no producer today, for [`WindowCode`]'s reason and kept for
/// [`WindowCode`]'s reason; see that type's note.
///
/// A `None` is infectious through [`IntrinsicCode::charge`] and
/// [`IntrinsicCode::add`] for the reason it is infectious through
/// [`WindowCode`]'s: a sum over *some* of a program's call sites reads as a sum
/// over all of them.
///
/// [ADR 0064]: ../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntrinsicCode {
    /// Bytes of machine code emitted for each variant's call sites, indexed by
    /// [`Intrinsic::index`](cove_ir::Intrinsic::index), or `None` from a code
    /// generator that cannot attribute them. See this type's own note for which
    /// generator that is and why.
    ///
    /// What is counted is the **call sequence a site compiles to**, and not what
    /// the runtime does once it is entered: the hand-over of the frame, the pc,
    /// the destination, the site and the argument list, the indirect call
    /// itself, and the outcome test the intrinsic's effects ask for. The
    /// algorithm on the far side is Rust compiled once for the whole program,
    /// and charging a copy of it to each of a variant's sites is the one way
    /// this figure could be made to say something false.
    pub bytes: Option<[u64; cove_ir::intrinsic::COUNT]>,
    /// How many `Inst::IntrinsicCall` sites of each variant were emitted,
    /// indexed the same way.
    ///
    /// Carried beside the bytes rather than left for a reader to find
    /// elsewhere, for [`WindowCode::sites`]'s reason: bytes *per site* is the
    /// number that says whether a call sequence is large, and a ratio taken from
    /// two figures in two reports is one a reader gets wrong.
    ///
    /// These are **static sites and not dynamic calls**, which is the
    /// distinction ADR 0064's census draws in each of its two columns: covefmt
    /// has two `String.length` sites and makes 405,588 calls through them. Bytes
    /// divide by the sites, never by the calls — a byte of machine code is
    /// emitted once and executed as often as the program likes.
    pub sites: [u64; cove_ir::intrinsic::COUNT],
}

impl Default for IntrinsicCode {
    /// Nothing emitted yet, and the bytes attributable.
    ///
    /// `Some([0; _])` rather than `None`, for [`WindowCode`]'s reason: a
    /// function with no intrinsic call in it has nought bytes of intrinsic call,
    /// and that is a measurement rather than an absence. A generator that cannot
    /// attribute anything says so by charging `None`, which is the first site's
    /// business and not this value's.
    fn default() -> IntrinsicCode {
        IntrinsicCode {
            bytes: Some([0; cove_ir::intrinsic::COUNT]),
            sites: [0; cove_ir::intrinsic::COUNT],
        }
    }
}

impl IntrinsicCode {
    /// Records one emitted call site of `intrinsic`, with the bytes it was where
    /// the code generator can say.
    ///
    /// `None` for the bytes counts the site and drops the whole byte table, for
    /// the reason on the type: a partial sum is indistinguishable from a total.
    pub fn charge(&mut self, intrinsic: cove_ir::Intrinsic, bytes: Option<u64>) {
        self.sites[intrinsic.index()] += 1;
        self.bytes = match (self.bytes, bytes) {
            (Some(mut rows), Some(count)) => {
                rows[intrinsic.index()] += count;
                Some(rows)
            }
            _ => None,
        };
    }

    /// Adds one function's intrinsic calls to a running total.
    pub fn add(&mut self, other: &IntrinsicCode) {
        for (at, sites) in self.sites.iter_mut().enumerate() {
            *sites += other.sites[at];
        }
        self.bytes = match (self.bytes, other.bytes) {
            (Some(mut rows), Some(theirs)) => {
                for (at, row) in rows.iter_mut().enumerate() {
                    *row += theirs[at];
                }
                Some(rows)
            }
            _ => None,
        };
    }

    /// Every variant's bytes together, or `None` where they are not attributed.
    pub fn total_bytes(&self) -> Option<u64> {
        self.bytes.map(|rows| rows.iter().sum())
    }

    /// Every variant's call sites together.
    pub fn total_sites(&self) -> u64 {
        self.sites.iter().sum()
    }
}

#[cfg(feature = "template")]
pub mod subset;

#[cfg(feature = "template")]
pub use subset::{blockers, refusal, supported, Reason, Refusal};

#[cfg(feature = "template")]
pub mod template;
