//! What a run sent across each boundary, counted apart.
//!
//! [ADR 0058]'s first phase ends with "Report emitted IR, mediated intrinsics,
//! encoded VM instructions, native-to-VM crossings and native-to-runtime calls
//! separately", and its gates list what a performance report includes: "emitted,
//! mediated and encoded instruction counts" and "VM-to-native, native-to-VM,
//! direct-native and runtime-helper crossings". [`BoundaryReport`] is those five
//! quantities as one value, so that every later phase of the ADR is measured
//! against the same five numbers rather than against whichever one a reader
//! happened to print.
//!
//! # Five quantities, because no two of them are one
//!
//! - **emitted IR** is static: the instructions the lowering left in the
//!   program after optimization, and how many of them are `IntrinsicCall` sites.
//!   It is a fact about the program and answers the same whatever runs it;
//! - **mediated intrinsics** are dynamic: each `IntrinsicCall` that reached
//!   `Machine::call_intrinsic`, by [`Intrinsic`], split by the tier that made the
//!   call. A native fast path that answered in emitted code never reaches the
//!   runtime and is *not* counted — which is the point: a mediated call is the
//!   one that crossed;
//! - **encoded instructions** are what the dispatch loop ran, which is
//!   `Machine::instructions` and not a second counter beside it. Since
//!   [ADR 0062] fused a window's rows behind its head that is a count of
//!   *semantic* instructions — fuel's — and the report says apart how many
//!   dispatches they took and how many windows of each pattern ran fused;
//! - **tier crossings** are [`Tiers`], unchanged;
//! - **native-to-runtime calls** are one counter per [`NativeHelpers`] field.
//!
//! # Free when it is off
//!
//! Nothing here is on the dispatch loop. What a run that did not ask pays is:
//!
//! - one `Option` test at the top of `Machine::call_intrinsic`, which is already a
//!   Rust call that dispatches on the intrinsic — the same shape `Machine::tiered`
//!   puts at a `call` — and a second one right after `intrinsics::call` returns,
//!   for [ADR 0064]'s per-variant allocations, words and examined work: the
//!   first test's `Some` arm is what reads the allocation counters before the
//!   call, so the second reads them again and charges the difference rather
//!   than reading them unconditionally, and the work is a number the arm
//!   already reported and the machine has already charged;
//! - one `Option` test in the native `intrinsic` helper, which is already a call
//!   out of compiled code into that same function;
//! - and nothing at all in the other eight helpers: those are counted by a
//!   **second helper table**, [`helpers_counting`], which a run that wants the
//!   counts compiles against and a run that does not never binds. That is
//!   `ablate::CENSUS`'s discipline — the production helpers are the same
//!   function bodies they were, not a copy with a branch in them.
//!
//! A fused arm pays one `Option` test per window it runs or declines, and only
//! where it already stands: the count is taken out of line, as
//! `Machine::count_intrinsic`'s is, and nothing on the path of an unfused
//! instruction reads it. [`Windows`]' census adds no test the fast paths did
//! not already have a branch for — a decline is a `return` that was there — and
//! [`BoundaryReport::growths`] adds one to the runtime's `grow`, which is out of
//! line and is entered once per reallocation.
//!
//! [ADR 0058]: ../../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md
//! [ADR 0062]: ../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
//! [ADR 0064]: ../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md
//! [`NativeHelpers`]: cove_native::NativeHelpers
//! [`helpers_counting`]: crate::native_helpers_counting

use std::collections::HashMap;
use std::fmt;

use cove_ir::bytecode::Op;
use cove_ir::legalize::Pattern;
use cove_ir::{FunctionId, Inst, Intrinsic, Program, SiteId};
use cove_native::{GrowableOp, RunOp};

use crate::vm::exec::native::Tiers;

/// Calls compiled code made into the runtime, one counter per helper.
///
/// One field per [`NativeHelpers`](cove_native::NativeHelpers) field, in that
/// struct's order. A helper that itself falls back to another — `open` for a
/// callee with no compiled entry runs the whole mediated `call` — is counted once,
/// as the helper compiled code called: this is a count of *crossings*, not of
/// runtime work.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HelperCalls {
    /// [`SafepointFn`](cove_native::SafepointFn): ADR 0040's three steps.
    pub safepoint: u64,
    /// [`CallFn`](cove_native::CallFn): the whole mediated call.
    pub call: u64,
    /// [`OpenFn`](cove_native::OpenFn): the open half of a direct call.
    pub open: u64,
    /// [`CloseFn`](cove_native::CloseFn): the close half of a direct call.
    pub close: u64,
    /// [`AllocFn`](cove_native::AllocFn): one `Inst::Alloc`.
    pub alloc: u64,
    /// [`IntrinsicFn`](cove_native::IntrinsicFn): one intrinsic call.
    pub intrinsic: u64,
    /// [`GrowableFn`](cove_native::GrowableFn): one growable-run operation.
    pub growable: u64,
    /// The same calls, by the [`GrowableOp`] each one named, indexed by
    /// [`GrowableOp::abi`]. They sum to [`growable`](Self::growable).
    ///
    /// One helper serves every growable operation, and "a million growable
    /// calls" does not say whether they were pushes that found a full store or
    /// whole appends handed over: which operation is what issue #409's work is
    /// measured against, and it is a field rather than a patched binary.
    pub growable_ops: [u64; GROWABLE_OPS],
    /// [`RunCopyFn`](cove_native::RunCopyFn): one run copy, whole.
    pub run_copy: u64,
    /// The same calls, by the [`RunOp`] each one named, indexed by
    /// [`RunOp::abi`], for [`growable_ops`](Self::growable_ops)' reason.
    pub run_copy_ops: [u64; RUN_OPS],
    /// [`FieldLoadFn`](cove_native::abi::FieldLoadFn): a field bound the emitted
    /// table could not answer.
    pub field_load: u64,
    /// [`FieldStoreFn`](cove_native::abi::FieldStoreFn): the same, storing.
    pub field_store: u64,
    /// [`OrderStrFn`](cove_native::abi::OrderStrFn): a `String` key's
    /// three-way order, the one leaf helper.
    pub order_str: u64,
}

impl HelperCalls {
    /// Every helper call, whichever helper.
    pub fn total(self) -> u64 {
        self.rows().iter().map(|(_, calls)| calls).sum()
    }

    /// Each helper's name, as `NativeHelpers` spells the field, and its count.
    pub fn rows(self) -> [(&'static str, u64); 11] {
        [
            ("safepoint", self.safepoint),
            ("call", self.call),
            ("open", self.open),
            ("close", self.close),
            ("alloc", self.alloc),
            ("intrinsic", self.intrinsic),
            ("growable", self.growable),
            ("run_copy", self.run_copy),
            ("field_load", self.field_load),
            ("field_store", self.field_store),
            ("order_str", self.order_str),
        ]
    }

    /// Charges one `growable` call of the operation numbered `op`. A number no
    /// arm emits is still a call, so it is charged to the total and to no row.
    pub(crate) fn charge_growable(&mut self, op: u32) {
        self.growable += 1;
        if let Some(count) = self.growable_ops.get_mut(op as usize) {
            *count += 1;
        }
    }

    /// Charges one `run_copy` call of the operation numbered `op`, as
    /// [`charge_growable`](Self::charge_growable) does.
    pub(crate) fn charge_run_copy(&mut self, op: u32) {
        self.run_copy += 1;
        if let Some(count) = self.run_copy_ops.get_mut(op as usize) {
            *count += 1;
        }
    }

    /// Each `growable` operation compiled code called for, with its count, in
    /// [`GrowableOp::abi`] order.
    pub fn growable_rows(self) -> Vec<(GrowableOp, u64)> {
        (0..GROWABLE_OPS as u32)
            .filter_map(GrowableOp::from_abi)
            .map(|op| (op, self.growable_ops[op.abi() as usize]))
            .collect()
    }

    /// Each `run_copy` operation, with its count, in [`RunOp::abi`] order.
    pub fn run_copy_rows(self) -> Vec<(RunOp, u64)> {
        (0..RUN_OPS as u32)
            .filter_map(RunOp::from_abi)
            .map(|op| (op, self.run_copy_ops[op.abi() as usize]))
            .collect()
    }
}

/// How many [`GrowableOp`]s there are: one past the largest ABI number.
///
/// A constant here rather than on the enum, because the enum is the ABI and a
/// count is only this report's business. A test holds the two together.
pub const GROWABLE_OPS: usize = 10;

/// How many [`RunOp`]s there are, for [`GROWABLE_OPS`]' reason.
pub const RUN_OPS: usize = 5;

/// How many [`Decline`]s there are, for [`GROWABLE_OPS`]' reason: the length of
/// [`Decline::ALL`], so that a reason added to the enum without a column here
/// fails a test rather than being recorded nowhere.
pub const DECLINES: usize = Decline::ALL.len();

/// Why a fused arm handed a window back to the rows it is still encoded as.
///
/// [ADR 0062]'s three fast paths each ask, up front and before they write
/// anything, every question a row of the window would refuse on; any answer but
/// the common one writes nothing, answers `0`, and the window runs as the
/// primitives it never stopped being. That is correct whatever the answer was,
/// which is why nothing had to say which answer it had been — and the ADR left
/// "nothing counts how often a fast path declines in a real run" open for
/// exactly that reason.
///
/// A count with no reason beside it cannot say whether the next window worth
/// teaching is the one that came too near a safepoint, the one whose store had
/// no room, or the one whose shape the decoder does not know; these nine are
/// the answers the three fast paths actually distinguish, one per family of
/// questions rather than one per `return`, because a reader asks what was
/// wrong with the window and not which line said so.
///
/// Nine and not eight because a reason is also a *price*, and the census was
/// wrong about one of them: [`Decline::Safepoint`] and [`Decline::Charge`] are
/// asked by neighbouring lines of the same arm and cost opposite amounts — the
/// first loses the window's whole dispatch saving, the second loses nothing at
/// all. Summed into one column they read as the same event. They are not.
///
/// [ADR 0062]: ../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decline {
    /// The entry test, and only it: at the head, the longest window's tail
    /// would reach `next_check`, so no window may run without a question
    /// falling inside it and the rows must dispatch one by one.
    ///
    /// This is the expensive decline — the only one that costs the window's
    /// whole dispatch saving, because nothing fuses after it. Every other
    /// reason here refuses a *part* and the head still carries its rows to the
    /// commit through `fused_tail`, which is why an append's bulk-charge
    /// refusal, once counted in this column, is now [`Decline::Charge`]'s.
    Safepoint,
    /// The rows behind the head, or the argument list of a copy among them, are
    /// not the window's shape — a grammar the decoder does not know.
    Shape,
    /// The owner's run is missing, has been consumed, is cut short of its
    /// payload by a chunk boundary, or is not the layout family the window's
    /// storage names.
    Owner,
    /// The store's run is missing, has been consumed, or is not the family the
    /// window's storage expects of it.
    Store,
    /// A copy's source run is missing, is not a family the element reads as
    /// units of, or its element is no words wide.
    Source,
    /// A value or a copy range the fast path does not admit: a byte over
    /// `0xFF`, a negative count or offset, or a range that runs past the
    /// source's length.
    Range,
    /// A chunk boundary of the heap cuts the source short of the range the copy
    /// was asked for, so the units are not one slice to read.
    Chunk,
    /// The copy is longer than one window may charge at once: `count` over
    /// `BULK_CHUNK_BYTES`, or over `BULK_CHUNK_WORDS` elements of its stride.
    Bulk,
    /// An append's copy would carry the charged work across the safepoint
    /// stride, so the copy is left to the row whose own poll falls where the
    /// stride asks for one.
    ///
    /// It declines the copy and not the window: the head still runs the rows
    /// through `fused_tail`, so this costs nothing in dispatches. That is the
    /// whole reason it is a column rather than a share of
    /// [`Decline::Safepoint`], whose every window costs the dispatch saving it
    /// was fused for. The two were one row once and the measurement is what
    /// settled it: on cq's `append.bytes` all 5,815 declines under that name
    /// were this one and cost nothing, while covefmt's `push.words` 36,432
    /// were the entry test and cost a window each. Reconciling a dispatch
    /// count against that row meant going back to the source to find out which
    /// kind it held.
    Charge,
}

impl Decline {
    /// Every reason, in [`Decline::index`] order.
    pub const ALL: [Decline; 9] = [
        Decline::Safepoint,
        Decline::Shape,
        Decline::Owner,
        Decline::Store,
        Decline::Source,
        Decline::Range,
        Decline::Chunk,
        Decline::Bulk,
        Decline::Charge,
    ];

    /// Where this reason is in [`Decline::ALL`], for a table of counts, as
    /// [`Pattern::index`] is.
    pub fn index(self) -> usize {
        match self {
            Decline::Safepoint => 0,
            Decline::Shape => 1,
            Decline::Owner => 2,
            Decline::Store => 3,
            Decline::Source => 4,
            Decline::Range => 5,
            Decline::Chunk => 6,
            Decline::Bulk => 7,
            Decline::Charge => 8,
        }
    }

    /// What a report calls it, as [`Pattern::name`] does.
    pub fn name(self) -> &'static str {
        match self {
            Decline::Safepoint => "safepoint",
            Decline::Shape => "shape",
            Decline::Owner => "owner",
            Decline::Store => "store",
            Decline::Source => "source",
            Decline::Range => "range",
            Decline::Chunk => "chunk",
            Decline::Bulk => "bulk",
            Decline::Charge => "charge",
        }
    }
}

/// What became of one window whose head a fused arm ran.
///
/// The three answers are what [ADR 0062]'s arms actually do, and they are
/// three rather than two because the middle one is neither a fast path nor a
/// decline: the fast path asked its questions, liked the answers, wrote the
/// rows before the ensure — and then found no room, or a unit a chunk boundary
/// cuts in two, and finished the window the ordinary way.
///
/// [ADR 0062]: ../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The fast path ran the whole window in its one call, the room already
    /// there.
    Fast,
    /// The fast path's in-place write did not apply — no room, or a chunk
    /// boundary cutting the units in two — and the window finished the ordinary
    /// way: inside `fused_push` after its ensure for a push, and through
    /// `fused_tail` after the ensure for an append.
    Slow,
    /// The fast path declined before it wrote anything, and the window ran as
    /// the rows it is still encoded as.
    Declined(Decline),
}

/// What each pattern's windows did, indexed by [`Pattern::index`].
///
/// [ADR 0062]'s `fusions` says how many windows a fused head ran through to
/// their commit; it cannot tell a fast path from [`Outcome::Slow`]'s row-by-row
/// completion, and it says nothing at all about a window that declined. This
/// does both, so that a change to a fast path is priced by a counting run
/// rather than by a third binary.
///
/// # `slow` is not a reallocation, and [`BoundaryReport::growths`] is
///
/// A window that took the slow completion is one whose fast path found no room
/// *or* whose units a chunk boundary cut in two, and the ensure it then made
/// may find the capacity already sufficient. A growth is counted where the
/// reallocation happens instead — in the runtime's `grow` — so it also counts
/// the growths an unfused ensure made, which no window here ever saw. Read the
/// two together and neither as the other.
///
/// # A declined window may still have fused, and one reason says which
///
/// [`BoundaryReport::fusions`] counts a window whose rows reached their commit
/// in the head's one dispatch, and a window the fast path declined does that
/// too — `fused_tail` runs its rows there. So `fast + slow` is *short* of
/// `fusions` by every window that declined and fused anyway, and [`run`](Self::run)
/// is *above* it by the windows that did not fuse at all.
///
/// The windows that did not fuse at all are exactly one column:
/// [`Decline::Safepoint`], the entry test, which refuses the window before it
/// begins. Every other reason — [`Decline::Charge`], which refuses only an
/// append's copy, as much as a shape or a store the fast path would not touch
/// — leaves the head to run its rows to the commit. So the gap is not a gap
/// but an identity, per pattern `p`:
///
/// ```text
/// fusions[p] == run(p) - declined[p][Decline::Safepoint.index()]
/// ```
///
/// It was hand-waved before the charge had a column of its own, because the
/// safepoint row held both kinds and the arithmetic only came out for
/// `push.words`, which has no copy to charge for: covefmt's counting run shows
/// 5,438,985 push.words heads less 36,432 safepoint declines being its
/// 5,402,553 fusions, while its byte-append row sat 2,860 above the same
/// subtraction — 2,860 charge declines, read as safepoints. Split, the
/// identity holds for all four patterns on both counting runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Windows {
    /// Windows the pattern's fast path ran whole, in its one call.
    pub fast: [u64; 4],
    /// Windows whose fast path wrote the rows before the ensure and then
    /// finished the ordinary way.
    pub slow: [u64; 4],
    /// Windows the fast path declined before writing anything, by
    /// [`Decline::index`].
    pub declined: [[u64; DECLINES]; 4],
}

impl Windows {
    /// The heads of `pattern` a fused arm ran: the three outcomes summed.
    pub fn run(&self, pattern: Pattern) -> u64 {
        let at = pattern.index();
        self.fast[at] + self.slow[at] + self.declined[at].iter().sum::<u64>()
    }
}

/// One intrinsic's row: where the program names it, and how often each tier
/// asked the runtime to perform it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntrinsicCalls {
    /// The operation.
    pub intrinsic: Intrinsic,
    /// `IntrinsicCall` instructions naming it, over every function with a body.
    pub sites: u64,
    /// Calls the encoded dispatch loop made.
    pub encoded: u64,
    /// Calls compiled code made through the `intrinsic` helper.
    pub native: u64,
    /// Objects the heap handed out across every call of this variant, from
    /// either tier — [ADR 0064]'s Decision 7, which the opcode table's
    /// `OPCODE_FLOOR` leaves off any variant that ran fewer than a thousand
    /// times. This row has no such floor.
    ///
    /// [ADR 0064]: ../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md
    pub allocations: u64,
    /// Words the heap handed out across every call of this variant, for
    /// [`allocations`](Self::allocations)'s reason.
    pub words: u64,
    /// Units this variant examined across every call of it, from either
    /// tier — the proportional work [ADR 0064]'s Decision 7 asks for, and the
    /// figure that answers "what did this variant actually walk?" where the
    /// call count only answers how often it was asked.
    ///
    /// **The unit is the storage run's, so a text variant counts bytes.**
    /// That is what `Machine::bulk_work` already counts for an
    /// `Inst::RunCopy` — bytes over a `Storage::PackedBytes`, words over a
    /// `Storage::Words` — and a `String` is a packed byte run. A walk over a
    /// value counts one per scalar, field or element visited. Summing the
    /// column across variants is therefore summing two units, and the row is
    /// the thing to read.
    ///
    /// Thirteen of the 26 variants can be non-zero here, which is exactly the
    /// set that declares `Effects::BULK_WORK`; the other thirteen examine
    /// nothing proportional and report nought. It was eighteen of 31 before
    /// ADR 0064's Phase 1 took `String.length`, then `String.endsWith`, then
    /// `String.startsWith` out of the enum, ADR 0065 took `String.contains`
    /// out of it in turn, and ADR 0064's fifth migration took `String.indexOf`
    /// — both numbers fall as the enum does, and this is the first migration
    /// after which they are equal.
    ///
    /// [ADR 0064]: ../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md
    pub work: u64,
}

impl IntrinsicCalls {
    /// Calls that reached the runtime, from either tier.
    pub fn calls(self) -> u64 {
        self.encoded + self.native
    }
}

/// The lowered program, counted statically.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Emitted {
    /// Functions with a body. Stubs are not counted, for
    /// [`NativeProgram::reachable`](crate::NativeProgram::reachable)'s reason.
    pub functions: usize,
    /// IR instructions in those functions, after optimization.
    pub instructions: u64,
    /// How many of them are `IntrinsicCall`.
    pub intrinsic_sites: u64,
    /// How many of them are an `Inst::Call` to a standard-library function:
    /// the library calls the lowering left calls rather than expanding.
    ///
    /// [ADR 0058] makes a thin library wrapper a mandatory expansion, so this
    /// is what says whether one was missed; what remains is the library's
    /// larger algorithms and the bodies `lower::inline` may not expand.
    ///
    /// [ADR 0058]: ../../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md
    pub library_call_sites: u64,
}

/// The `Call`s into standard-library functions a run made, by the tier that
/// made them.
///
/// Counted where a call already reaches Rust — `Machine::tiered` for the
/// encoded tier, and the counting `call` and `open` helpers for compiled code
/// — so an uncounted run pays nothing for it. A closure call whose body is the
/// library's is one too, because it opens the same frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LibraryCalls {
    /// Calls the encoded dispatch loop made.
    pub encoded: u64,
    /// Calls compiled code made, through the `call` or `open` helper. `None`
    /// when compiled code ran against the production helpers, which count
    /// nothing, or when no native tier was installed.
    pub native: Option<u64>,
}

impl Emitted {
    /// Counts `program`, and the `IntrinsicCall` sites of each builtin by
    /// `SiteId`.
    fn of(program: &Program) -> (Emitted, Vec<u64>) {
        let mut emitted = Emitted::default();
        let mut sites = vec![0u64; program.intrinsic_sites.len()];
        for function in program.functions.iter().filter(|f| !f.is_stub()) {
            emitted.functions += 1;
            emitted.instructions += function.code.len() as u64;
            for inst in &function.code {
                match inst {
                    Inst::IntrinsicCall { site, .. } => {
                        emitted.intrinsic_sites += 1;
                        if let Some(count) = sites.get_mut(site.index()) {
                            *count += 1;
                        }
                    }
                    Inst::Call { callee, .. } if program.function(*callee).is_library() => {
                        emitted.library_call_sites += 1;
                    }
                    _ => {}
                }
            }
        }
        (emitted, sites)
    }
}

/// The five quantities of ADR 0058's boundary, for one run.
///
/// Emitted IR (static), mediated intrinsics by the tier that called them,
/// instructions the encoded VM dispatched, tier crossings, and calls compiled code
/// made into each runtime helper — reported apart because no two of them are one
/// number. Taken with [`Vm::boundary`](crate::Vm::boundary) after
/// [`Vm::count_boundary`](crate::Vm::count_boundary), or with
/// [`NativeSession::count_boundary`](crate::NativeSession::count_boundary) and
/// [`NativeSession::take_boundary`](crate::NativeSession::take_boundary).
///
/// # Free when it is off
///
/// Nothing is on the dispatch loop. A run that did not ask pays one `Option` test
/// at the top of `Machine::call_intrinsic` — already a Rust call that dispatches on
/// the intrinsic — a second one right after `intrinsics::call` returns, for the
/// per-variant allocations, words and work this report also carries, and one in the
/// native `intrinsic` helper, which is already a call out of compiled code into
/// that function. The per-helper counts cost such a run nothing at
/// all, because they are a second helper table,
/// [`native_helpers_counting`](crate::native_helpers_counting), which only
/// [`compile_native_counting`](crate::compile_native_counting) binds:
/// `ablate::CENSUS`'s discipline, where the production helpers stay the bodies
/// they were.
///
/// The dynamic counts are the **entry task's**, as `Machine::instructions` and
/// [`Tiers`] are: a spawned task runs on a machine of its own, with no tier and no
/// counters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoundaryReport {
    /// The program, counted statically.
    pub emitted: Emitted,
    /// Every intrinsic the program names, sorted by dynamic calls descending,
    /// then by sites descending, then by name.
    pub intrinsics: Vec<IntrinsicCalls>,
    /// Instructions the encoded dispatch loop ran while counting: semantic
    /// instructions, a fused window's rows each counted, as fuel counts them.
    pub encoded_instructions: u64,
    /// Turns of the dispatch loop those instructions took: fewer by every row a
    /// fused head ran after itself.
    pub encoded_dispatches: u64,
    /// Windows a fused head ran through to their commit, by
    /// [`Pattern::index`].
    pub fusions: [u64; 4],
    /// What each window a fused head named did: [ADR 0062]'s census, beside
    /// `fusions` rather than instead of it, because `fusions` is the number the
    /// ADR published and this one is finer.
    ///
    /// [ADR 0062]: ../../../../docs/adr/0062-an-append-is-ensure-store-commit.md
    pub windows: Windows,
    /// Stores a growth **actually reallocated**, indexed 0 for
    /// [`Storage::PackedBytes`](cove_ir::Storage::PackedBytes) and 1 for
    /// [`Storage::Words`](cove_ir::Storage::Words).
    ///
    /// Counted where the reallocation happens and not where a window ends, for
    /// the reason [`Windows`] gives: this also counts the growths an ensure
    /// outside any window made, and [`Windows::slow`] counts a window that
    /// finished the ordinary way whether or not its store moved.
    pub growths: [u64; 2],
    /// The `Call`s into the standard library, by the tier that made them.
    pub library_calls: LibraryCalls,
    /// How the calls divided between the tiers, or `None` when no native tier was
    /// installed.
    pub tiers: Option<Tiers>,
    /// Calls compiled code made into the runtime, or `None` when there was no
    /// native tier or its table was compiled against the production helpers,
    /// which count nothing. See
    /// [`compile_native_counting`](crate::compile_native_counting).
    pub helpers: Option<HelperCalls>,
}

impl BoundaryReport {
    /// Every mediated call, from either tier.
    pub fn mediated(&self) -> u64 {
        self.intrinsics.iter().map(|row| row.calls()).sum()
    }

    /// The row for `intrinsic`, if the program names it.
    pub fn intrinsic(&self, intrinsic: Intrinsic) -> Option<IntrinsicCalls> {
        self.intrinsics
            .iter()
            .copied()
            .find(|row| row.intrinsic == intrinsic)
    }
}

/// The counters a counting run keeps on its machine.
///
/// A `Box` on the machine, for [`Tiering`](super::exec::native::Tiering)'s reason:
/// a helper reaches the machine through one raw pointer, and this is written from
/// inside helpers.
pub(crate) struct Counting {
    /// `IntrinsicCall`s that reached `Machine::call_intrinsic`, by `SiteId`, from
    /// either tier.
    sites: Vec<u64>,
    /// The ones among them the native `intrinsic` helper made.
    from_native: Vec<u64>,
    /// Objects the heap handed out while running the calls at each `SiteId`,
    /// charged by `Machine::charge_intrinsic_allocations` as the difference
    /// of `Machine::allocations()` read before and after `intrinsics::call`.
    allocations: Vec<u64>,
    /// Words the heap handed out at each `SiteId`, for
    /// [`allocations`](Self::allocations)'s reason.
    words: Vec<u64>,
    /// Units the calls at each `SiteId` reported having examined, which
    /// `Machine::charge_intrinsic_costs` is handed after `Machine` has
    /// already charged the same number to its work total. Unlike the two
    /// above it is not a difference of counters: an arm reports it, through
    /// `Machine::examined`.
    examined: Vec<u64>,
    /// Whether each function, by `FunctionId`, is the standard library's.
    library: Vec<bool>,
    /// Frames the encoded tier opened for a library function.
    library_encoded: u64,
    /// Frames compiled code opened for one, through the counting helpers.
    library_native: u64,
    /// Native-to-runtime calls, which only [`helpers_counting`]'s table writes.
    ///
    /// [`helpers_counting`]: crate::native_helpers_counting
    pub(crate) helpers: HelperCalls,
    /// `Machine::instructions` when counting began, so the report is of what
    /// happened since.
    instructions_at: u64,
    /// Instructions a fused head ran after itself, with no dispatch of their
    /// own.
    folded: u64,
    /// Windows run fused through their commit, by [`Pattern::index`].
    fusions: [u64; 4],
    /// What each window a fused head named did, by pattern and outcome.
    windows: Windows,
    /// Growths that reallocated, by storage kind, as
    /// [`BoundaryReport::growths`] is indexed.
    growths: [u64; 2],
    /// The tier counts when counting began, for the same reason.
    tiers_at: Tiers,
}

impl Counting {
    pub(crate) fn new(program: &Program, instructions: u64, tiers: Tiers) -> Counting {
        Counting {
            sites: vec![0; program.intrinsic_sites.len()],
            from_native: vec![0; program.intrinsic_sites.len()],
            allocations: vec![0; program.intrinsic_sites.len()],
            words: vec![0; program.intrinsic_sites.len()],
            examined: vec![0; program.intrinsic_sites.len()],
            library: program
                .functions
                .iter()
                .map(cove_ir::Function::is_library)
                .collect(),
            library_encoded: 0,
            library_native: 0,
            helpers: HelperCalls::default(),
            instructions_at: instructions,
            folded: 0,
            fusions: [0; 4],
            windows: Windows::default(),
            growths: [0; 2],
            tiers_at: tiers,
        }
    }

    /// `rows` instructions the fused head `head` ran after itself, and one
    /// window of its pattern if they reached the commit.
    pub(crate) fn fused(&mut self, head: u8, rows: usize, whole: bool) {
        self.folded += rows as u64;
        let pattern = Op::from_number(head).and_then(Op::pattern);
        if let (true, Some(pattern)) = (whole, pattern) {
            self.fusions[pattern.index()] += 1;
        }
    }

    /// What became of the window whose head is the opcode `head`: one outcome,
    /// charged to the head's [`Pattern`].
    ///
    /// The pattern is found the way [`Counting::fused`] finds it, so the census
    /// and `fusions` name a window the same way rather than each deciding for
    /// itself. A head that names no pattern records nothing: there is no column
    /// for it, and a total that quietly held one would be a total of something
    /// else.
    pub(crate) fn window(&mut self, head: u8, outcome: Outcome) {
        let Some(pattern) = Op::from_number(head).and_then(Op::pattern) else {
            return;
        };
        let at = pattern.index();
        match outcome {
            Outcome::Fast => self.windows.fast[at] += 1,
            Outcome::Slow => self.windows.slow[at] += 1,
            Outcome::Declined(why) => self.windows.declined[at][why.index()] += 1,
        }
    }

    /// One growth that reallocated a store of `storage`.
    pub(crate) fn growth(&mut self, storage: cove_ir::Storage) {
        let at = match storage {
            cove_ir::Storage::PackedBytes => 0,
            cove_ir::Storage::Words(_) => 1,
        };
        self.growths[at] += 1;
    }

    /// One `IntrinsicCall` of `builtin`, whichever tier made it.
    pub(crate) fn intrinsic(&mut self, site: SiteId) {
        if let Some(count) = self.sites.get_mut(site.index()) {
            *count += 1;
        }
    }

    /// One call the encoded tier made to `callee`, counted if it is the
    /// library's.
    pub(crate) fn encoded_call(&mut self, callee: FunctionId) {
        if self.library.get(callee.index()).copied().unwrap_or(false) {
            self.library_encoded += 1;
        }
    }

    /// One call compiled code made to `callee`, counted if it is the library's.
    pub(crate) fn native_call(&mut self, callee: FunctionId) {
        if self.library.get(callee.index()).copied().unwrap_or(false) {
            self.library_native += 1;
        }
    }

    /// One of those, made by the native `intrinsic` helper.
    pub(crate) fn native_intrinsic(&mut self, site: SiteId) {
        if let Some(count) = self.from_native.get_mut(site.index()) {
            *count += 1;
        }
    }

    /// What one call at `site` cost: `allocations` objects, `words` words,
    /// and `examined` units of whatever it walked, whichever tier made the
    /// call. Charged once per call, from
    /// `Machine::charge_intrinsic_costs` — so a call that allocated nothing
    /// and examined nothing charges `0` rather than nothing at all, and the
    /// row still exists for [`Counting::report`] to sum.
    ///
    /// The first two are a difference of the machine's own counters taken
    /// across `intrinsics::call`; the third is not a difference at all but
    /// what the arm reported through `Machine::examined`, which the machine
    /// has already added to its work total by the time this is called. The
    /// three travel together because they are charged at one point and cost
    /// one `Option` test between them.
    pub(crate) fn intrinsic_cost(
        &mut self,
        site: SiteId,
        allocations: u64,
        words: u64,
        examined: u64,
    ) {
        if let Some(total) = self.allocations.get_mut(site.index()) {
            *total += allocations;
        }
        if let Some(total) = self.words.get_mut(site.index()) {
            *total += words;
        }
        if let Some(total) = self.examined.get_mut(site.index()) {
            *total += examined;
        }
    }

    /// The report, over `program` and the machine's current counts.
    ///
    /// `tiers` is `None` for a run with no native tier, and `helpers_counted`
    /// whether the table it ran was compiled against the counting helpers.
    pub(crate) fn report(
        &self,
        program: &Program,
        instructions: u64,
        tiers: Option<Tiers>,
        helpers_counted: bool,
    ) -> BoundaryReport {
        let (emitted, sites) = Emitted::of(program);
        // Several `SiteId`s may name one intrinsic — one per result layout —
        // and a reader asks about the operation, so they are summed.
        let mut rows: HashMap<Intrinsic, IntrinsicCalls> = HashMap::new();
        for (at, builtin) in program.intrinsic_sites.iter().enumerate() {
            let native = self.from_native.get(at).copied().unwrap_or(0);
            let all = self.sites.get(at).copied().unwrap_or(0);
            let row = rows
                .entry(builtin.intrinsic)
                .or_insert_with(|| IntrinsicCalls {
                    intrinsic: builtin.intrinsic,
                    sites: 0,
                    encoded: 0,
                    native: 0,
                    allocations: 0,
                    words: 0,
                    work: 0,
                });
            row.sites += sites.get(at).copied().unwrap_or(0);
            row.native += native;
            row.encoded += all.saturating_sub(native);
            row.allocations += self.allocations.get(at).copied().unwrap_or(0);
            row.words += self.words.get(at).copied().unwrap_or(0);
            row.work += self.examined.get(at).copied().unwrap_or(0);
        }
        let mut intrinsics: Vec<IntrinsicCalls> = rows
            .into_values()
            .filter(|row| row.sites > 0 || row.calls() > 0)
            .collect();
        intrinsics.sort_by(|a, b| {
            b.calls()
                .cmp(&a.calls())
                .then_with(|| b.sites.cmp(&a.sites))
                .then_with(|| a.intrinsic.to_string().cmp(&b.intrinsic.to_string()))
        });
        let tiers = tiers.map(|now| since(now, self.tiers_at));
        let native_counted = tiers.is_some() && helpers_counted;
        BoundaryReport {
            emitted,
            intrinsics,
            encoded_instructions: instructions.saturating_sub(self.instructions_at),
            encoded_dispatches: instructions
                .saturating_sub(self.instructions_at)
                .saturating_sub(self.folded),
            fusions: self.fusions,
            windows: self.windows,
            growths: self.growths,
            library_calls: LibraryCalls {
                encoded: self.library_encoded,
                native: native_counted.then_some(self.library_native),
            },
            helpers: (tiers.is_some() && helpers_counted).then_some(self.helpers),
            tiers,
        }
    }
}

/// `now` less `then`, field by field.
fn since(now: Tiers, then: Tiers) -> Tiers {
    Tiers {
        vm_to_vm: now.vm_to_vm.saturating_sub(then.vm_to_vm),
        vm_to_native: now.vm_to_native.saturating_sub(then.vm_to_native),
        native_to_vm: now.native_to_vm.saturating_sub(then.native_to_vm),
        native_to_native_direct: now
            .native_to_native_direct
            .saturating_sub(then.native_to_native_direct),
        native_to_native_mediated: now
            .native_to_native_mediated
            .saturating_sub(then.native_to_native_mediated),
        host_to_native: now.host_to_native.saturating_sub(then.host_to_native),
        host_to_vm: now.host_to_vm.saturating_sub(then.host_to_vm),
    }
}

/// The report as text, one quantity to a block, in the CLI's coverage style.
///
/// Every line begins `boundary:` or is an indented row under one, so a reader can
/// `grep` a run's stderr for the whole of it.
///
/// # Reading `buffer windows` against `growths`
///
/// The two blocks count different things in different places and the second is
/// not a column of the first. A window's `slow` says its fast path wrote the
/// rows before the ensure and then finished the ordinary way — which the ensure
/// may do without reallocating anything — while `growths` is taken where a
/// store is actually replaced, and so also holds the growths of every ensure
/// outside a window. See [`Windows`].
impl fmt::Display for BoundaryReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let emitted = self.emitted;
        writeln!(
            f,
            "boundary: emitted IR, {} instruction(s) in {} function(s), {} of them `IntrinsicCall` site(s)",
            thousands(emitted.instructions),
            emitted.functions,
            thousands(emitted.intrinsic_sites)
        )?;
        let fused: Vec<String> = Pattern::ALL
            .iter()
            .map(|pattern| {
                format!(
                    "{} {}",
                    pattern.name(),
                    thousands(self.fusions[pattern.index()])
                )
            })
            .collect();
        writeln!(
            f,
            "boundary: encoded VM, {} instruction(s) in {} dispatch(es); windows fused: {}",
            thousands(self.encoded_instructions),
            thousands(self.encoded_dispatches),
            fused.join(", ")
        )?;
        let windows = self.windows;
        let heads: u64 = Pattern::ALL
            .iter()
            .map(|&pattern| windows.run(pattern))
            .sum();
        writeln!(
            f,
            "boundary: buffer windows, {} head(s) a fused arm ran: what each did",
            thousands(heads)
        )?;
        writeln!(
            f,
            "  {:>14} {:>14} {:>14}  pattern",
            "fast", "slow", "declined"
        )?;
        // A pattern no head of which ran is left out, as an operation no call
        // named is left out of the helper table above.
        for pattern in Pattern::ALL.into_iter().filter(|&p| windows.run(p) > 0) {
            let at = pattern.index();
            writeln!(
                f,
                "  {:>14} {:>14} {:>14}  {}",
                thousands(windows.fast[at]),
                thousands(windows.slow[at]),
                thousands(windows.declined[at].iter().sum::<u64>()),
                pattern.name()
            )?;
        }
        for pattern in Pattern::ALL {
            let at = pattern.index();
            let why: Vec<String> = Decline::ALL
                .iter()
                .filter(|why| windows.declined[at][why.index()] > 0)
                .map(|why| {
                    format!(
                        "{} {}",
                        why.name(),
                        thousands(windows.declined[at][why.index()])
                    )
                })
                .collect();
            if !why.is_empty() {
                writeln!(
                    f,
                    "  declines, by reason: {} {}",
                    pattern.name(),
                    why.join(", ")
                )?;
            }
        }
        writeln!(
            f,
            "boundary: growths, {} store(s) reallocated: packed bytes {}, words {}",
            thousands(self.growths[0] + self.growths[1]),
            thousands(self.growths[0]),
            thousands(self.growths[1])
        )?;
        let library = self.library_calls;
        writeln!(
            f,
            "boundary: standard library, {} `Call` site(s) left unexpanded; {} call(s) made \
             from encoded, {} from native",
            thousands(emitted.library_call_sites),
            thousands(library.encoded),
            match library.native {
                Some(native) => thousands(native),
                None => "uncounted".to_string(),
            }
        )?;
        match self.tiers {
            Some(tiers) => writeln!(
                f,
                "boundary: crossings, VM->VM {}, VM->native {}, native->VM {}, \
                 native->native direct {}, native->native mediated {}",
                thousands(tiers.vm_to_vm),
                thousands(tiers.vm_to_native),
                thousands(tiers.native_to_vm),
                thousands(tiers.native_to_native_direct),
                thousands(tiers.native_to_native_mediated)
            )?,
            None => writeln!(f, "boundary: crossings, none: no native tier was installed")?,
        }
        match (self.tiers, self.helpers) {
            (Some(_), Some(helpers)) => {
                writeln!(
                    f,
                    "boundary: native -> runtime helper calls, {} in all",
                    thousands(helpers.total())
                )?;
                for (name, calls) in helpers.rows() {
                    writeln!(f, "  {name:<12} {:>14}", thousands(calls))?;
                    // The operations under the two helpers that serve several,
                    // indented once more and only where one ran, so a report
                    // of a run that never grew anything reads as it did.
                    let ops: Vec<(String, u64)> = match name {
                        "growable" => helpers
                            .growable_rows()
                            .into_iter()
                            .map(|(op, calls)| (format!("{op:?}"), calls))
                            .collect(),
                        "run_copy" => helpers
                            .run_copy_rows()
                            .into_iter()
                            .map(|(op, calls)| (format!("{op:?}"), calls))
                            .collect(),
                        _ => Vec::new(),
                    };
                    for (op, calls) in ops.into_iter().filter(|(_, calls)| *calls > 0) {
                        writeln!(f, "    {op:<14} {:>12}", thousands(calls))?;
                    }
                }
            }
            (Some(_), None) => writeln!(
                f,
                "boundary: native -> runtime helper calls were not counted: the table was \
                 compiled against the production helpers"
            )?,
            (None, _) => {}
        }
        writeln!(
            f,
            "boundary: mediated intrinsics, {} call(s) that reached the runtime \
             (a native fast path that answered in emitted code is not one)",
            thousands(self.mediated())
        )?;
        writeln!(
            f,
            "  {:>14} {:>14} {:>7} {:>11} {:>12} {:>14}  intrinsic",
            "from encoded", "from native", "sites", "allocs", "words", "work"
        )?;
        for row in &self.intrinsics {
            writeln!(
                f,
                "  {:>14} {:>14} {:>7} {:>11} {:>12} {:>14}  {}",
                thousands(row.encoded),
                thousands(row.native),
                row.sites,
                thousands(row.allocations),
                thousands(row.words),
                thousands(row.work),
                row.intrinsic
            )?;
        }
        Ok(())
    }
}

/// `n` with a separator every three digits, as the CLI's coverage report prints
/// its counts.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two counts are the enums' lengths: every number below them names an
    /// operation, and the first number past them names none. An operation added
    /// to the ABI without raising the count fails here rather than being charged
    /// to the total and to no row.
    #[test]
    fn the_operation_counts_are_the_abi_enums_lengths() {
        assert!((0..GROWABLE_OPS as u32).all(|code| GrowableOp::from_abi(code).is_some()));
        assert_eq!(GrowableOp::from_abi(GROWABLE_OPS as u32), None);
        assert!((0..RUN_OPS as u32).all(|code| RunOp::from_abi(code).is_some()));
        assert_eq!(RunOp::from_abi(RUN_OPS as u32), None);
        assert_eq!(DECLINES, Decline::ALL.len());
    }

    /// Every reason sits at its own index and answers to its own name, which is
    /// what makes [`Windows::declined`] a table a reader can index by
    /// [`Decline::index`] and print by [`Decline::name`]. A reason added to the
    /// enum and forgotten in `ALL` — or given another's index, which would
    /// charge two reasons to one column — fails here.
    #[test]
    fn every_decline_is_at_its_own_index_under_its_own_name() {
        for (at, why) in Decline::ALL.into_iter().enumerate() {
            assert_eq!(why.index(), at, "{why:?}");
        }
        let mut names: Vec<&str> = Decline::ALL.iter().map(|why| why.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), DECLINES, "the names are distinct");
    }

    /// A window is charged to its pattern and its outcome, and
    /// [`Windows::run`] is the three summed — the heads of that pattern a fused
    /// arm ran.
    #[test]
    fn a_window_is_charged_to_its_pattern_and_its_outcome() {
        let mut counting = Counting::new(&Program::default(), 0, Tiers::default());
        let head = Op::FusedPushWords.number();
        counting.window(head, Outcome::Fast);
        counting.window(head, Outcome::Fast);
        counting.window(head, Outcome::Slow);
        counting.window(head, Outcome::Declined(Decline::Safepoint));
        counting.window(Op::Return.number(), Outcome::Fast);
        counting.growth(cove_ir::Storage::PackedBytes);
        counting.growth(cove_ir::Storage::Words(cove_ir::LayoutId(0)));
        counting.growth(cove_ir::Storage::Words(cove_ir::LayoutId(0)));
        let report = counting.report(&Program::default(), 0, None, false);
        let at = Pattern::PushWords.index();
        assert_eq!(report.windows.fast[at], 2);
        assert_eq!(report.windows.slow[at], 1);
        assert_eq!(report.windows.declined[at][Decline::Safepoint.index()], 1);
        assert_eq!(report.windows.run(Pattern::PushWords), 4);
        // A head that names no pattern has no column, so it is not counted at
        // all rather than counted somewhere.
        assert_eq!(report.windows.run(Pattern::PushByte), 0);
        assert_eq!(report.growths, [1, 2]);
    }

    /// A charge lands on the total and on its operation's row, and the rows sum
    /// to the total.
    #[test]
    fn a_charge_is_counted_in_all_and_by_operation() {
        let mut calls = HelperCalls::default();
        calls.charge_growable(GrowableOp::EnsureWords.abi());
        calls.charge_growable(GrowableOp::EnsureWords.abi());
        calls.charge_growable(GrowableOp::Alloc.abi());
        calls.charge_run_copy(RunOp::CopyBytes.abi());
        assert_eq!(calls.growable, 3);
        assert_eq!(calls.run_copy, 1);
        let rows = calls.growable_rows();
        assert_eq!(rows.iter().map(|(_, n)| n).sum::<u64>(), calls.growable);
        assert!(rows.contains(&(GrowableOp::EnsureWords, 2)));
        assert!(rows.contains(&(GrowableOp::Alloc, 1)));
        assert!(calls.run_copy_rows().contains(&(RunOp::CopyBytes, 1)));
    }

    /// The printed report says each quantity once and sorts intrinsics by calls.
    #[test]
    fn a_report_prints_each_quantity_apart() {
        let report = BoundaryReport {
            emitted: Emitted {
                functions: 3,
                instructions: 12_345,
                intrinsic_sites: 4,
                library_call_sites: 2,
            },
            intrinsics: vec![
                IntrinsicCalls {
                    intrinsic: Intrinsic::StringJoin,
                    sites: 1,
                    encoded: 1_000,
                    native: 7,
                    allocations: 1_007,
                    words: 5_035,
                    work: 128_440,
                },
                IntrinsicCalls {
                    intrinsic: Intrinsic::StringFromCodePoint,
                    sites: 3,
                    encoded: 0,
                    native: 0,
                    allocations: 0,
                    words: 0,
                    work: 0,
                },
            ],
            encoded_instructions: 1_234_567,
            encoded_dispatches: 1_234_000,
            fusions: [80, 0, 3, 0],
            windows: Windows {
                fast: [70, 0, 2, 0],
                slow: [10, 0, 1, 0],
                declined: [
                    [5, 0, 0, 0, 0, 0, 0, 0, 0],
                    [0; DECLINES],
                    [1, 0, 0, 0, 0, 0, 0, 2, 3],
                    [0; DECLINES],
                ],
            },
            growths: [4, 6],
            library_calls: LibraryCalls {
                encoded: 9,
                native: Some(1_001),
            },
            tiers: Some(Tiers {
                vm_to_native: 2,
                ..Tiers::default()
            }),
            helpers: Some(HelperCalls {
                intrinsic: 7,
                growable: 3,
                growable_ops: [1, 0, 0, 0, 0, 2, 0, 0, 0, 0],
                ..HelperCalls::default()
            }),
        };
        assert_eq!(report.mediated(), 1_007);
        let text = report.to_string();
        assert!(text.contains("emitted IR, 12,345 instruction(s) in 3 function(s), 4 of them"));
        assert!(text.contains(
            "encoded VM, 1,234,567 instruction(s) in 1,234,000 dispatch(es); windows fused: \
             push.words 80, push.byte 0, append.bytes 3, append.words 0"
        ));
        assert!(text.contains(
            "standard library, 2 `Call` site(s) left unexpanded; 9 call(s) made from encoded, \
             1,001 from native"
        ));
        assert!(text.contains("buffer windows, 94 head(s) a fused arm ran"));
        assert!(text.contains("\n              70             10              5  push.words\n"));
        assert!(text.contains("\n               2              1              6  append.bytes\n"));
        assert!(text.contains("\n  declines, by reason: push.words safepoint 5\n"));
        assert!(
            text.contains("\n  declines, by reason: append.bytes safepoint 1, bulk 2, charge 3\n")
        );
        assert!(
            !text.contains("  push.byte\n"),
            "a pattern no head of which ran is left out of the census table"
        );
        assert!(
            text.contains("boundary: growths, 10 store(s) reallocated: packed bytes 4, words 6")
        );
        assert!(text.contains("VM->native 2,"));
        assert!(text.contains("helper calls, 10 in all"));
        assert!(text.contains("\n    Alloc                     1\n"));
        assert!(text.contains("\n    EnsureWords               2\n"));
        assert!(
            !text.contains("    Finish "),
            "an operation that never ran is left out"
        );
        assert!(text.contains(
            "           1,000              7       1       1,007        5,035        128,440  \
             String.join"
        ));
        assert!(text.contains(
            "               0              0       3           0            0              0  \
             String.fromCodePoint"
        ));
    }

    /// A loop that calls an intrinsic that allocates (`String.join` builds
    /// the string it answers) and one that does not (`Any.equals` walks two
    /// values together and answers one `Bool` word), each several times over,
    /// so both [`Counting`]'s wiring and the reconciliation test below have
    /// more than one call and more than one site to work with.
    ///
    /// **The reading one has moved three times, and each move is a
    /// migration.** It was `String.length` until ADR 0064 made the count
    /// `std.string.length`; it was then `String.contains` until ADR 0065 gave
    /// that a run search to stand on; it was then `String.indexOf`, until ADR
    /// 0064's next migration wrote that over the same run search. There is no
    /// `Text` intrinsic left that reads without allocating — the readers that
    /// remain all build the string or array they answer — so the sample is
    /// `Any.equals`, which is the shape for the same three reasons each of the
    /// others was: it reads the values it was handed, it charges what it
    /// walked, and it allocates nothing, because the answer is one word.
    const JOIN_AND_COMPARE: &str = "
export fn main() -> Int {
  let left = [1, 2, 3]
  let right = [1, 2, 3]
  let other = [1, 9, 3]
  var total = 0
  var i = 0
  while i < 50 {
    if left == right {
      total = total + 1
    }
    if left == other {
      total = total + 1
    }
    let joined = \",\".join([\"a\", \"b\", \"c\"])
    total = total + joined.byteLength()
    i = i + 1
  }
  total
}
";

    /// [ADR 0064]'s Decision 7 asks that allocations and allocated words be
    /// attributed per variant, and this is the property worth pinning about
    /// that attribution: it is not just present, it tells two operations
    /// apart. `String.join` allocates the string it hands back, and
    /// `Any.equals` only reads the values it is given, so a run of both
    /// must show one row with allocations and one row without — from the
    /// real machinery in `Machine::call_intrinsic`, not from calling
    /// [`Counting::intrinsic_allocated`] directly, which would only prove the
    /// bookkeeping adds correctly and not that it is wired to anything.
    ///
    /// [ADR 0064]: ../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md
    #[test]
    fn an_allocating_intrinsics_row_carries_allocations_and_a_reading_ones_does_not() {
        use crate::vm::debug::tests::World;

        let world = World::new(JOIN_AND_COMPARE);
        let mut vm = world.plain();
        vm.count_boundary();
        vm.run_entry("m", "main", Vec::new()).expect("it answers");
        let boundary = vm.boundary().expect("count_boundary was called");

        let join = boundary
            .intrinsic(Intrinsic::StringJoin)
            .expect("the program calls String.join");
        assert!(join.calls() > 0, "{join:?}");
        assert!(
            join.allocations > 0,
            "String.join allocates the string it answers: {join:?}"
        );
        assert!(join.words > 0, "{join:?}");
        // The per-variant total is a subset of the whole run's, never past
        // it — the sanity Decision 7's own measurement leans on.
        assert!(join.allocations <= vm.allocations(), "{join:?}");
        assert!(join.words <= vm.allocated_words(), "{join:?}");

        let compared = boundary
            .intrinsic(Intrinsic::AnyEquals)
            .expect("the program compares two arrays");
        assert!(compared.calls() > 0, "{compared:?}");
        assert_eq!(
            compared.allocations, 0,
            "Any.equals walks its operands and answers one word: {compared:?}"
        );
        assert_eq!(compared.words, 0, "{compared:?}");
    }

    /// **The totals must reconcile exactly with the opcode and site
    /// profile.** [ADR 0064]'s Decision 7 says so in those words, for the
    /// same reason `vm_coverage.rs`'s ratchets are compared as sets rather
    /// than as counts: a total that merely matches in aggregate could still
    /// be attributing the right number of allocations to the wrong variant.
    /// So this checks it per variant, from one run watched by both a
    /// [`Profiler`](crate::vm::profile::Profiler) and boundary counting at
    /// once — the same instructions, read two ways — rather than trusting
    /// that two separate runs of the same program would have counted the
    /// same thing.
    ///
    /// For every `Intrinsic` the boundary report names, this sums the
    /// profiler's own per-instruction cost over every `IntrinsicCall` site
    /// naming that variant, and checks the sum against the boundary row's
    /// calls, allocations and words. `program.intrinsic_site` is what turns a
    /// profiled `(FunctionId, pc)` back into the `Intrinsic` an
    /// `IntrinsicCall` there names, exactly as `Counting::report` does.
    ///
    /// [ADR 0064]: ../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md
    #[test]
    fn the_boundary_report_and_the_profile_reconcile_by_variant() {
        use crate::vm::debug::tests::World;
        use crate::vm::profile::Profiler;

        let world = World::new(JOIN_AND_COMPARE);
        let profiler = Profiler::new();
        let mut vm = world.watched(&profiler);
        vm.count_boundary();
        vm.run_entry("m", "main", Vec::new()).expect("it answers");
        let boundary = vm.boundary().expect("count_boundary was called");
        assert!(
            !boundary.intrinsics.is_empty(),
            "the program names some intrinsic"
        );

        let program = world.program();
        let rows = profiler.rows();
        for row in &boundary.intrinsics {
            let mut calls = 0u64;
            let mut allocations = 0u64;
            let mut words = 0u64;
            let mut work = 0u64;
            for ((id, pc), cost) in &rows {
                let Some(function) = program.functions.get(id.index()) else {
                    continue;
                };
                let Some(Inst::IntrinsicCall { site, .. }) = function.code.get(*pc as usize) else {
                    continue;
                };
                if program.intrinsic_site(*site).intrinsic != row.intrinsic {
                    continue;
                }
                calls += cost.ran;
                allocations += cost.allocations;
                words += cost.words;
                work += cost.work;
            }
            assert_eq!(calls, row.calls(), "{:?}: {row:?}", row.intrinsic);
            assert_eq!(allocations, row.allocations, "{:?}: {row:?}", row.intrinsic);
            assert_eq!(words, row.words, "{:?}: {row:?}", row.intrinsic);
            assert_eq!(work, row.work, "{:?}: {row:?}", row.intrinsic);
        }
        // And the work is not vacuously nought on both sides: this program
        // calls `String.contains` and `String.join`, and both walk bytes.
        assert!(
            boundary.intrinsics.iter().any(|row| row.work > 0),
            "a program that walks strings examines something: {:?}",
            boundary.intrinsics
        );
    }

    /// Ten `String.toUpper` calls over a string of `characters` ASCII
    /// characters, so that two runs of it differ in exactly the one thing the
    /// charge is supposed to be proportional to.
    ///
    /// The receiver is a literal rather than something the program builds,
    /// because anything that built it would call intrinsics of its own and
    /// the two runs would then differ in more than the receiver's length.
    ///
    /// **The operation has moved three times and this is the first move that
    /// changed its effect class.** It was `String.length` until ADR 0064 moved
    /// the count out of the intrinsics, `String.contains` until ADR 0065 gave
    /// that a run search, and then `String.indexOf` until the migration that
    /// wrote that over the same search. No reading, non-allocating `Text`
    /// intrinsic is left, so this is a reading one that *does* allocate —
    /// which costs the case nothing, because what it pins is the work column
    /// and not the allocation column. If anything the charge is sharper:
    /// `toUpper` walks the whole receiver by construction, where `indexOf`
    /// charged the receiver's length as an upper bound and needed a needle
    /// that was nowhere in it to make the bound exact.
    fn maps_over(characters: usize) -> String {
        let text = "a".repeat(characters);
        format!(
            "
export fn main() -> Int {{
  let text = \"{text}\"
  var total = 0
  var i = 0
  while i < 10 {{
    total = total + text.toUpper().byteLength()
    i = i + 1
  }}
  total
}}
"
        )
    }

    /// **The charge is proportional to what the call examined.**
    ///
    /// This is the property the whole of [ADR 0064]'s Decision 7 exists for,
    /// and the one a regression would silently undo: before it, an
    /// `IntrinsicCall` was charged one unit of work whatever it walked, so
    /// 10,000 `String.length` calls over ten characters and over 100,000
    /// characters spent `fuel_spent` 160,027 against 160,026 — the same fuel
    /// for 383 times the wall clock. That measurement was taken while
    /// `String.length` was still an intrinsic; ADR 0064 moved it into
    /// `std.string` and this case became `String.contains`, and ADR 0065 has
    /// since moved that one too, and `String.indexOf` after it. The case below
    /// is `String.toUpper`, which is charged the same way — a reading whose
    /// charge is the receiver's byte length.
    ///
    /// Two runs of the same shape over receivers a hundred times apart,
    /// making the same number of calls, from the real machinery. The call
    /// counts are asserted equal first: a work column that rose because the
    /// program made more calls would say nothing about proportionality, and
    /// the equality is what rules that out.
    ///
    /// [ADR 0064]: ../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md
    #[test]
    fn the_work_a_variant_is_charged_scales_with_what_it_examined() {
        use crate::vm::debug::tests::World;

        let row = |source: &str| {
            let world = World::new(source);
            let mut vm = world.plain();
            vm.count_boundary();
            vm.run_entry("m", "main", Vec::new()).expect("it answers");
            vm.boundary()
                .expect("count_boundary was called")
                .intrinsic(Intrinsic::StringToUpper)
                .expect("the program calls String.toUpper")
        };

        let short = row(&maps_over(10));
        let long = row(&maps_over(1_000));

        assert_eq!(
            short.calls(),
            long.calls(),
            "the two runs make the same calls: {short:?} against {long:?}"
        );
        assert!(short.calls() >= 10, "{short:?}");
        // A hundred times the bytes. Exactly, because the receivers are ASCII
        // literals and the unit is bytes — which is the other half of what
        // this pins: a charge in *characters* would be the same two numbers
        // here, and one in words an eighth of them.
        assert_eq!(short.work, short.calls() * 10, "{short:?}");
        assert_eq!(long.work, long.calls() * 1_000, "{long:?}");
    }

    /// Ten `==` comparisons of two four-field structs that `differ` in the
    /// first field or not at all.
    ///
    /// A struct is compared field by field in declaration order, so the two
    /// programs differ in how far the walk gets and in nothing else: the same
    /// declaration, the same number of comparisons, the same layouts.
    fn equals_four_fields(differ: bool) -> String {
        let first = if differ { 9 } else { 1 };
        format!(
            "
struct Quad {{
  a: Int
  b: Int
  c: Int
  d: Int
}}

export fn main() -> Int {{
  let x = Quad(a: 1, b: 2, c: 3, d: 4)
  let y = Quad(a: {first}, b: 2, c: 3, d: 4)
  var total = 0
  var i = 0
  while i < 10 {{
    if x == y {{
      total = total + 1
    }}
    i = i + 1
  }}
  total
}}
"
        )
    }

    /// **An early exit is charged what it did, not what it was handed.**
    ///
    /// The charge is made *in* the walk — `equal::value` reports one unit
    /// per value it reaches — rather than computed from the operands' width
    /// before the comparison begins. The two are the same number only when
    /// the walk runs to the end, and the difference is what makes the column
    /// a measurement rather than a second rendering of the layout table.
    ///
    /// Two structs that differ in their first field are one struct and one
    /// field. Two equal ones are the struct and all four of its fields. The
    /// exact multiples are asserted rather than an inequality, because an
    /// inequality would hold just as well for a charge that was merely noisy.
    #[test]
    fn an_early_exit_is_charged_less_than_a_whole_walk() {
        use crate::vm::debug::tests::World;

        let row = |source: &str| {
            let world = World::new(source);
            let mut vm = world.plain();
            vm.count_boundary();
            vm.run_entry("m", "main", Vec::new()).expect("it answers");
            vm.boundary()
                .expect("count_boundary was called")
                .intrinsic(Intrinsic::AnyEquals)
                .expect("comparing two structs calls Any.equals")
        };

        let early = row(&equals_four_fields(true));
        let whole = row(&equals_four_fields(false));

        assert_eq!(
            early.calls(),
            whole.calls(),
            "the two runs make the same calls: {early:?} against {whole:?}"
        );
        assert!(early.calls() >= 10, "{early:?}");
        assert_eq!(early.work, early.calls() * 2, "{early:?}");
        assert_eq!(whole.work, whole.calls() * 5, "{whole:?}");
        assert!(early.work < whole.work);
    }
}
