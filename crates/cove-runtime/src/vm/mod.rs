//! The execution backend [ADR 0034](../../../../docs/adr/0034-one-physical-word-stack.md)
//! decided on, and since the cutover, the only one: this is the production
//! path an embedder runs a Cove program on.
//!
//! [`docs/LINEAR_VM.md`](../../../../docs/LINEAR_VM.md) is the design. It was
//! written as a clean-room replacement rather than a renovation: nothing here
//! was derived from the backend it replaced, and at the cutover that backend
//! was deleted. `lvm` and `cove-lir` were transitional spellings, worn while
//! the predecessor still held these names; it is gone, and this module and
//! `cove-ir` have taken them.
//!
//! Three things leave this module: [`Vm`], the type an embedder holds,
//! [`PreparedProgram`], the part of one that a program's every run can share,
//! and [`exec::SAFEPOINT_STRIDE`], a number a test asserts a bound against.
//! Nothing else does, because what a caller can name is the whole of what
//! this boundary decides. A word, a layout, a [`mem::Memory`] and a
//! [`exec::Machine`] are the representation, and a representation that leaves
//! the crate is one that cannot be changed without changing somebody else's
//! code.
//!
//! # What a run owns, and what each of its tasks owns
//!
//! Issue #240's Q1 answers this and [`docs/LINEAR_VM.md`](../../../../docs/LINEAR_VM.md)'s
//! "Ownership" section writes it out. Five things, and which of them is a
//! value store is the whole of what ADR 0034 cares about:
//!
//! | | belongs to | where it is | a value store? |
//! |---|---|---|---|
//! | the object heap | the **run** | [`mem::Space`], one per run, behind an `Arc` | **yes**, and the only one |
//! | a stack segment | a **task** | `[k * SEGMENT_WORDS, (k+1) * SEGMENT_WORDS)` of the same address space | yes, and it is part of the same one |
//! | a `Shared` cell | the **run**'s heap | an ordinary object; its lock is one of its words ([`cell`]) | no — it *is* an object in the heap |
//! | a host resource | the **host** | a table of names, shared by every task | no — see ADR 0031 |
//! | a `Task`, a `TaskScope` | the **scheduler** | a table of control state, one per task | no |
//!
//! The last row is the one that has to be argued rather than asserted, because
//! a `Task` is a value a Cove program writes down. Its word is **one past an
//! index into a scheduler table**, the way a `Repr::Host` word is, and the
//! entry it names holds a task id, a scope name and position, a
//! `Cancellation`, and the *address* of the heap object holding its answer.
//! Every one of those but the last is scheduler bookkeeping that no Cove value
//! could be hidden in. The last is an address, which makes the table a **root
//! provider** and not a second store: the answer's words are in the run's
//! heap, in an object the spawning task allocated before the thread existed,
//! and the table names one the way `Machine::literal_addrs` names a literal's
//! address: an index into the run's own metadata, not a second store.
//! Nothing that wanted to dodge a heap representation could be put there,
//! which is the test ADR 0034 actually applies.
//!
//! # The scheduler's table is a *task's*, and the host's is the *run's*
//!
//! The two look alike — a word one past an index into a table of names — and
//! they are owned differently, for a reason each states about itself rather
//! than by analogy.
//!
//! A `Task` and a `TaskScope` may not cross a task boundary; the task-safety
//! rule says so, and `cove_sema`'s `task_safe_offender` is where it is
//! enforced. So a word formed in one task is only ever read in that task, the
//! tables are disjoint by construction, and there is nothing to share. That is
//! the same arithmetic that keeps two stack segments apart, and it is why
//! [`exec::Machine`] holds its own.
//!
//! A **host resource** does cross: ADR 0013 gives the host the record of what
//! is open, a resource declares its own task-safety in its schema, and a
//! task-safe one is copied into a spawned closure like any other value — as
//! its word. A table of one task's own would make that word an index into a
//! list the receiving task does not have. So the resource table is the run's,
//! behind a lock, and one resource is one word for the length of the run,
//! which is what ADR 0013's *"two handles are equal when they name the same
//! resource"* costs once there is more than one thread.
//!
//! There used to be a single `#![allow(dead_code)]` here, covering every
//! submodule beneath it. Its own comment was honest about what it was *for*
//! — several items below are reached only from their own `#[cfg(test)]`
//! code, and one line in one place meant removing it was a single edit whose
//! failure would list exactly what was still unused. What it did not say was
//! that it covered far more than those items, because a module-wide allow
//! does not distinguish "reached only by a test" from "reached by nothing at
//! all": [ADR 0043](../../../../docs/adr/0043-a-method-moves-if-it-is-total-and-takes-no-closure.md)'s
//! third migration condition is checked by deleting a builtin's dispatch arm
//! and asking clippy whether the implementation it leaves behind is now
//! unreachable, and inside this module clippy could never answer, allow or
//! no. It was removed for that reason (issue #274).
//!
//! What replaces it is one `#[cfg_attr(not(test), allow(dead_code))]` per
//! item that is genuinely reached only from this crate's own tests, each
//! with a comment saying so beside it. That form says under `cargo test`
//! exactly what the broad allow said all the time — nothing, because the
//! item is used — and only turns the lint off for the build that has no
//! caller, which is the build the check above runs against. A handful of
//! items had no caller at all, not even a test; those were deleted rather
//! than annotated, which is what removing the broad allow was for.

use std::rc::Rc;
use std::sync::Arc;

use cove_diag::Span;
use cove_ir::{Function, FunctionId, Program};

use crate::budget::{Budget, Limits, Meter, Stopped};
use crate::error::RuntimeError;
use crate::host::HostRegistry;
use crate::runtime::Runtime;
use crate::trace::{RunOutcome, Timing, TraceEvent};
use crate::vm::debug::Debugger;
use crate::vm::exec::native;
use crate::vm::exec::Machine;
// The public `Value` reaches this file for the one reason ADR 0034 allows it
// to reach any of them: this is a boundary. An entry's arguments and its
// answer are what a host hands in and reads back, and they are `Value`s on
// both sides of that line. Nothing here stores one — every value named below
// is on its way into [`boundary::from_value`] or out of
// [`boundary::to_value`].
use crate::value::Value;

pub(crate) mod boundary;
pub(crate) mod cell;
pub(crate) mod debug;
#[cfg(test)]
mod differential;
#[cfg(test)]
mod erasure;
pub(crate) mod exec;
pub(crate) mod mem;
mod parked;
pub mod profile;
pub(crate) mod render;
pub(crate) mod report;
mod sequences;
#[cfg(test)]
mod stops;

pub use parked::{OwnedVm, ParkedVm, Step, YieldRequest, YieldedVm};

/// The words a run's heap region may grow to, for every [`Vm`] [`Vm::new`]
/// builds. [`Vm::with_heap_words`] is the one way to build a run over a
/// different budget, and its own doc comment says who that is for.
///
/// Four mebiwords, thirty-two mebibytes. Reserved is not committed: the
/// backing store grows on demand, so a program that allocates nothing pays
/// nothing, and what the number buys is a run that fails with "this run has no
/// memory left" rather than taking the machine down with it. Like
/// [`mem::STACK_WORDS`] it is an implementation choice and not a language
/// fact.
const DEFAULT_HEAP_WORDS: usize = 1 << 22;

/// A lowered program, encoded and verified once, for any number of [`Vm`]s
/// to run.
///
/// Everything a run needs that is a function of the program alone: the
/// fixed-width instructions [ADR 0041](../../../../docs/adr/0041-a-slot-number-fits-in-sixteen-bits.md)
/// decides — or the refusal a program with none is answered with — and the
/// tables derived from its layouts. [`Vm::new`] builds these for every run;
/// [`Vm::with_prepared`] takes them from here, so an embedder that holds one
/// program and many runs of it (an isolate per request, a decision per
/// invocation) pays for the encoding and the verification once and keeps one
/// copy of the result in memory.
///
/// It owns the program rather than being handed it again by each `Vm`,
/// because the dispatch loop trusts an encoding's operands *because* they
/// were verified against one program: a preparation that could be paired
/// with another would be an unchecked read waiting for a caller to make it.
/// It owns it rather than borrowing it so that it can be kept where the
/// program is kept — beside it in a struct, which a borrow could not be.
///
/// Cheap to clone and `Send + Sync`: what it holds is immutable once built
/// and behind `Arc`s, the same `Arc`s a run already shares with its own
/// spawned tasks, so one preparation can serve runs on many threads.
#[derive(Clone)]
pub struct PreparedProgram {
    program: Arc<Program>,
    prepared: exec::Prepared,
    /// The program's machine code, when it was compiled with
    /// [`PreparedProgram::with_native`]: compiled once, for every run built
    /// from this preparation, on any thread ([ADR 0085]).
    ///
    /// [ADR 0085]: ../../../../docs/adr/0085-compiled-frames-resume-where-they-yielded.md
    native: Option<Arc<crate::native::NativeProgram>>,
}

impl PreparedProgram {
    /// `program`, encoded, verified, and its layout tables built.
    ///
    /// Infallible, for [`Vm::new`]'s reason: a program with no encoding is
    /// held here as the refusal, and each run built from this answers its
    /// first [`Vm::run_entry`] or [`Vm::invoke`] with it, before a frame is
    /// pushed.
    pub fn new(program: Arc<Program>) -> PreparedProgram {
        let prepared = exec::Prepared::of(&program);
        PreparedProgram {
            program,
            prepared,
            native: None,
        }
    }

    /// This preparation with the program's native tier compiled — once, for
    /// every [`OwnedVm`] built from it ([ADR 0085]).
    ///
    /// [`crate::compile_native`] over the program this holds, so the machine
    /// code and the encoding are of one program and cannot be paired with
    /// another's. The code is immutable once compiled and shared by `Arc`: a
    /// thousand isolates of one tenant hold one copy, and each run's frames,
    /// counters and budget are its own machine's.
    ///
    /// An `OwnedVm` built from the answer calls compiled code where the
    /// encoded tier would have called a compiled function, and its parkable
    /// runs yield inside compiled code at a backedge's safepoint as they yield
    /// in the dispatch loop (ADR 0084), resuming on whatever thread resumes
    /// them.
    ///
    /// # Errors
    ///
    /// [`Unavailable`](cove_native::Unavailable) where this host or build has
    /// no native tier — the capability diagnostic, never a fallback.
    ///
    /// [ADR 0085]: ../../../../docs/adr/0085-compiled-frames-resume-where-they-yielded.md
    pub fn with_native(mut self) -> Result<PreparedProgram, cove_native::Unavailable> {
        let native = crate::native::compile(&self.program)?;
        self.native = Some(Arc::new(native));
        Ok(self)
    }

    /// The program this prepared.
    pub fn program(&self) -> &Arc<Program> {
        &self.program
    }

    /// The machine code [`PreparedProgram::with_native`] compiled, if it was
    /// asked for.
    pub fn native(&self) -> Option<&crate::native::NativeProgram> {
        self.native.as_deref()
    }
}

/// One run of a lowered program.
///
/// This is the type above the machine: it holds the program, the memory the
/// run executes over, and the two things that make a run a run rather than a
/// dispatch loop — the boundary a `Value` crosses, and the accounting a
/// safepoint charges. The dispatch loop underneath knows nothing about any
/// of them.
///
/// The two ways in are the two the language has, and they are the same two
/// [`crate::interp::Interpreter`] offers. [`Vm::run_entry`] is how a
/// *command* speaks to a program: the arguments are process arguments, which
/// are strings. [`Vm::invoke`] is how an *application* does: the arguments
/// are values the host built, held to the types the checker resolved before
/// the first instruction runs. Everything below the two is one path.
pub struct Vm<'a> {
    runtime: &'a Runtime,
    program: &'a Program,
    machine: Machine<'a>,
    /// The run's accounting, in the handle a safepoint charges through.
    ///
    /// Taken once, where the run begins, for the reason [`Meter`] gives. A
    /// registry with no budget installed answers `None`, which has always
    /// meant no limit; a meter over default [`Limits`] is that, written down.
    ///
    /// It is the *run's*, not the registry's: [`Vm::invoke_within`] and its
    /// siblings replace it with the budget they were handed, and every host
    /// call, `spawn` and safepoint of the run — its tasks' included — is
    /// charged here. The registry is shared by every run over it at once
    /// (issue #577), so it holds no run's budget.
    budget: Meter,
    /// The entry a run parked at a host call is inside, while it is.
    ///
    /// What [`Vm::enter_with`] would have done after the run answered needs
    /// the entry's name, its declared result and the clock it started, and
    /// a parked run has left that function: so they wait here, and
    /// [`Vm::resume`] finishes the entry with them. `None` whenever the run
    /// is not parked, which for a `Vm` that is not inside an [`OwnedVm`] is
    /// always.
    parked: Option<Entered>,
}

/// An entry that has begun and not yet left: what [`Vm::left`] needs to say so.
struct Entered {
    module: String,
    function: String,
    returns: cove_ir::LayoutId,
    span: Span,
    timing: Timing,
    /// The machine's host wait when the entry began, so that the exit reports
    /// this entry's share of it.
    waited: std::time::Duration,
    /// The machine's descheduled time when the entry began: the time it
    /// spent yielded is neither its `cpu` nor its `wait` (ADR 0084).
    descheduled: std::time::Duration,
}

impl<'a> Vm<'a> {
    /// Turns on or off, for every run in this process, the audit of the names
    /// a rendering of an erased value reads: every box made is walked, and
    /// every nominal layout in it whose names `cove_ir`'s lowering did not
    /// place is recorded for [`Vm::unplaced_names`].
    ///
    /// For the survey that runs every program in the repository, which holds
    /// the lowering's choice of which layouts get names (ADR 0068's Phase
    /// 4b-ii) to the boxes the runs really made. A run with it off pays one
    /// relaxed load a box.
    #[doc(hidden)]
    pub fn audit_placed_names(on: bool) {
        exec::dynamic::audit_placed_names(on);
    }

    /// Every layout the audit [`Vm::audit_placed_names`] turns on found in a
    /// box without its names placed, since the last call — by name.
    #[doc(hidden)]
    pub fn unplaced_names() -> Vec<String> {
        exec::dynamic::unplaced_names()
    }

    /// A run of `program`, over `runtime`'s checked program and `hosts`.
    ///
    /// `program` is **encoded and verified here**, into the fixed-width
    /// form [ADR 0041](../../../../docs/adr/0041-a-slot-number-fits-in-sixteen-bits.md)
    /// decides and issue #245's Phase 5 made the only one a run executes.
    /// There is no second representation to choose and no flag that selects
    /// one: `Inst` is what the lowering produced and what a listing and the
    /// debugger show, and what runs is sixteen bytes per instruction whose
    /// operands were checked before the first of them ran.
    ///
    /// This stays infallible, and what that costs is stated where it is
    /// paid. A program with no encoding — one whose frame is wider than a
    /// sixteen-bit slot names, which `cove_ir::lower` already refuses with a
    /// diagnostic — is refused by [`Vm::run_entry`] and [`Vm::invoke`]
    /// before a frame is pushed, rather than by this constructor. The
    /// alternative was a `Result` at every call site for a failure the
    /// compiler in front of it has already made impossible.
    ///
    /// The heap budget is this module's `DEFAULT_HEAP_WORDS`. [`Vm::with_heap_words`]
    /// is the constructor for a caller that needs a different one.
    ///
    /// That work is a function of `program` alone, and this constructor
    /// does it again for every `Vm`. A caller that builds more than one run
    /// over the same program should prepare it once, with
    /// [`PreparedProgram::new`], and build each run with
    /// [`Vm::with_prepared`], which does none of it.
    pub fn new(runtime: &'a Runtime, hosts: &'a HostRegistry, program: &'a Program) -> Vm<'a> {
        Vm::with_heap_words(runtime, hosts, program, DEFAULT_HEAP_WORDS)
    }

    /// The same run, over a heap that may grow only to `heap_words` words
    /// rather than `DEFAULT_HEAP_WORDS`.
    ///
    /// This is deliberately not a [`Limits`] field. [ADR 0011](../../../../docs/adr/0011-garbage-collection.md)'s
    /// amendment retracted `Limits::max_memory` because a number that bounds
    /// only what one collector's table can see is not a memory ceiling; it
    /// is that instrument's readout wearing a ceiling's name. Nothing about
    /// the linear-memory backend changes that argument for an *embedder*: its
    /// heap is a fuller account of a run's Cove-owned values than the old
    /// per-task heap ever was, per ADR 0034, but a Host's own allocations,
    /// open resources and each task's stack region still sit outside it, so
    /// naming `heap_words` beside `deadline` and `max_host_calls` would still
    /// promise a bound this number cannot back.
    ///
    /// So it is a constructor argument, the heap's capacity named as what it
    /// is. A test reaches for it to force a small heap so a collection has
    /// something to be tested against; an embedder running many isolates
    /// reaches for [`OwnedVm::with_heap_words`](crate::OwnedVm::with_heap_words)
    /// to size each one ([ADR 0088](../../../../docs/adr/0088-an-embedder-sizes-the-heap-and-is-told-of-a-cancellation.md)).
    pub fn with_heap_words(
        runtime: &'a Runtime,
        hosts: &'a HostRegistry,
        program: &'a Program,
        heap_words: usize,
    ) -> Vm<'a> {
        Vm::assemble(
            runtime,
            hosts,
            program,
            &exec::Prepared::of(program),
            heap_words,
        )
    }

    /// A run of the program `prepared` holds, sharing its encoding and its
    /// tables with every other run built from it.
    ///
    /// [`Vm::new`] with the program-wide work already done: what this
    /// constructor pays is the run's own — its heap, its stack, its literals —
    /// and a clone of each `Arc` the preparation holds. It cannot be handed a
    /// preparation of a different program, because a [`PreparedProgram`]
    /// carries the program it prepared and this takes it from there.
    ///
    /// A program that does not encode is refused exactly as [`Vm::new`]
    /// refuses it: by [`Vm::run_entry`] and [`Vm::invoke`], before a frame is
    /// pushed. The refusal was found once, by [`PreparedProgram::new`], and
    /// every run built from it answers with the same one.
    pub fn with_prepared(
        runtime: &'a Runtime,
        hosts: &'a HostRegistry,
        prepared: &'a PreparedProgram,
    ) -> Vm<'a> {
        Vm::assemble(
            runtime,
            hosts,
            &prepared.program,
            &prepared.prepared,
            DEFAULT_HEAP_WORDS,
        )
    }

    /// The one place a `Vm` is put together: [`Vm::with_heap_words`] reaches
    /// it with a preparation of its own and [`Vm::with_prepared`] with a
    /// shared one.
    ///
    /// `prepared` must be [`exec::Prepared::of`] `program`, which both callers
    /// guarantee by construction — the first has just built it, the second
    /// takes both out of one [`PreparedProgram`]. Not public for that reason.
    fn assemble(
        runtime: &'a Runtime,
        hosts: &'a HostRegistry,
        program: &'a Program,
        prepared: &exec::Prepared,
        heap_words: usize,
    ) -> Vm<'a> {
        Vm {
            runtime,
            program,
            machine: Machine::for_run(program, prepared, heap_words, Some(hosts), Some(runtime)),
            budget: meter_of(hosts),
            parked: None,
        }
    }

    /// A run that enters compiled code wherever `entries` has any.
    ///
    /// [ADR 0055]'s native tier, made **explicitly selectable**: the VM stays the
    /// default and this is the one constructor that asks for anything else.
    /// `entries` is the entry table — `Program + FunctionId -> encoded entry |
    /// native entry` — and it is consulted at every Cove call, by the encoded
    /// dispatch loop and by compiled code alike. A function it refuses runs on
    /// the **encoded** tier, which is a complete execution path; nothing here can
    /// reach the tree-walking interpreter, which ADR 0055 forbids as a
    /// per-function fallback.
    ///
    /// A third constructor rather than a parameter on [`Vm::new`], for
    /// [`Vm::debugged`]'s reason: no existing caller has a table to name, and a
    /// parameter every caller passes `None` to is a question every caller is
    /// asked and none of them answers. It is also what keeps the default
    /// unchanged by construction rather than by a default value.
    ///
    /// The table has to outlive the `Vm`, which is what `'a` says: whoever
    /// compiled the code owns the pages its entries point into, so the compiler
    /// is built first and dropped last. Nothing here rebuilds or refinalizes it.
    ///
    /// [ADR 0055]: ../../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md
    pub fn with_native(
        runtime: &'a Runtime,
        hosts: &'a HostRegistry,
        program: &'a Program,
        entries: &'a dyn native::Tiered,
    ) -> Vm<'a> {
        let mut vm = Vm::new(runtime, hosts, program);
        // Safety: `entries` outlives the `Vm` and therefore the machine inside
        // it, which is exactly what `install_native` asks and what the `'a` on
        // the parameter promises.
        unsafe { vm.machine.install_native(entries) };
        vm
    }

    /// Moves this run onto a **later stack segment**, before it has run
    /// anything, and answers that segment's stack origin.
    ///
    /// A seam for `cove-runtime`'s own `tests/native_tier.rs` and for nothing
    /// else — `Machine::on_a_later_stack_segment`, which this forwards to, says
    /// why a test needs one. The short of it: the entry task is segment 0, where
    /// the stack origin is `0` and a frame's word index and a `Repr::Addr` word
    /// are the same number, so an address family that dropped the origin answers
    /// correctly there and only there.
    ///
    /// `#[doc(hidden)]` and behind the code generator's feature because it is not
    /// API: an embedder has no reason to choose a segment, a default build does
    /// not have this method at all, and no production path calls it. The
    /// alternative was a test that could reach a later segment only through a
    /// spawned task, which installs no tier and so would have compared the
    /// encoded VM with itself.
    ///
    /// The origin is answered so that the case can assert it is not nought: a
    /// test that quietly stayed on segment 0 would be the blind one again.
    ///
    /// # Panics
    ///
    /// If anything has already run on this `Vm`. See the machine's own method.
    #[cfg(feature = "template")]
    #[doc(hidden)]
    pub fn on_a_later_stack_segment(&mut self) -> u64 {
        self.machine.on_a_later_stack_segment()
    }

    /// How this run's calls divided between the tiers, one counter per
    /// transition.
    ///
    /// All nought for a run built any other way: the counters are the installed
    /// table's, and a run with no table makes no transition to count. See
    /// [`Tiers`](native::Tiers) for why the count is of edges rather than of
    /// tiers.
    pub fn tiers(&self) -> native::Tiers {
        self.machine.tiers()
    }

    /// Starts counting what this run sends across each boundary, from now.
    ///
    /// [ADR 0058]'s five quantities — emitted IR, mediated intrinsics, encoded
    /// instructions, tier crossings and native-to-runtime calls — read back with
    /// [`Vm::boundary`]. Off unless this is called, and what a run that never
    /// calls it pays is one `Option` test per builtin call; see
    /// [`BoundaryReport`](crate::BoundaryReport).
    ///
    /// The native-to-runtime counts need the native table to have been built by
    /// [`compile_native_counting`](crate::compile_native_counting). A table built
    /// by [`compile_native`](crate::compile_native) binds helpers that count
    /// nothing, and the report says so rather than printing zeroes.
    ///
    /// [ADR 0058]: ../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md
    pub fn count_boundary(&mut self) {
        let tiers = self.machine.tiers();
        self.machine.count_boundary(tiers);
    }

    /// What was counted since [`Vm::count_boundary`], or `None` if it was never
    /// called.
    pub fn boundary(&self) -> Option<crate::BoundaryReport> {
        self.machine.boundary(self.machine.tier.as_deref())
    }

    /// Dynamic calls to each function that stayed on the encoded tier, by
    /// `FunctionId`.
    ///
    /// What a refusal cost, measured rather than guessed at: ADR 0055's report
    /// asks for refusals ordered by the native work they prevented, and a
    /// function count cannot say that. Empty for a run with no table.
    pub fn refused_calls(&self) -> &[u64] {
        self.machine.refused_calls()
    }

    /// The same run, watched by `debugger`.
    ///
    /// A second constructor rather than a parameter on [`Vm::new`], for the
    /// reason the heap budget is not one either: no existing caller has a
    /// debugger to name, and a parameter every caller passes `None` to is a
    /// question every caller is asked and none of them answers.
    ///
    /// What it costs the run is stated where it is paid, in
    /// the debugger's own module: the machine asks before **every** instruction
    /// for as long as the debugger is installed, so a debugged run is slower
    /// by whatever the debugger does per instruction. A run built with
    /// [`Vm::new`] is unchanged — the loop's comparison is the same one it
    /// was, against the next safepoint.
    pub fn debugged(
        runtime: &'a Runtime,
        hosts: &'a HostRegistry,
        program: &'a Program,
        debugger: &'a dyn Debugger,
    ) -> Vm<'a> {
        let mut vm = Vm::new(runtime, hosts, program);
        vm.machine.watch(Some(debugger));
        vm
    }

    /// Runs `module.name` with the process arguments `args`.
    ///
    /// An entry takes either no parameters or one `Array<String>`, and that
    /// rule is the language's rather than a backend's — the oracle refuses
    /// the third shape in these words, at this span.
    pub fn run_entry(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        let outcome = self.enter(module, name, args);
        self.ended(outcome)
    }

    /// Calls `module.name` with values the host built.
    ///
    /// The arguments are held to what the checker resolved about the
    /// declaration — the shape it has to be callable at all, the count, and
    /// each value's type followed as deeply as the type goes — before
    /// anything runs. That check is the crate's own `invoke`, shared with the
    /// oracle so that a host that gets it wrong reads one answer and not one
    /// per backend.
    ///
    /// One refusal belongs to this backend rather than to the language, and
    /// it is about the *lowering* rather than about the program.
    /// [`cove_ir::lower_entry`] lowers what one entry can reach and nothing
    /// else, so a run built for one entry cannot invoke a function no path
    /// from that entry leads to. Saying the package does not declare it would
    /// be false and would send an embedder to the wrong file, so this says
    /// which of the two is missing and what to lower instead.
    pub fn invoke(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let outcome = self.invoke_checked(module, name, args);
        self.ended(outcome)
    }

    /// Calls `module.name` with values the host built, in a program that has
    /// no checked program behind it.
    ///
    /// [`Vm::invoke`] holds the arguments to what the *checker* resolved about
    /// the declaration, and so needs the checked program the [`Runtime`] was
    /// built over. A program read back from an image ([ADR 0077]'s
    /// `cove_ir::serial`, which is how `cove fmt` carries covefmt) has none:
    /// the front end ran when the toolchain was built, and what is left is the
    /// lowering. So this holds the call to what the lowering kept instead —
    /// the function's arity, and each argument converted at its parameter's
    /// layout by the same boundary every entry crosses, which refuses a value
    /// of the wrong shape before the first instruction runs. What is lost is
    /// only the checker's wording of the refusal, and the caller of this is
    /// the toolchain itself, handing a `String` to a function whose signature
    /// it built.
    ///
    /// A `Runtime` for such a run is built over an empty checked program;
    /// nothing on this path reads it.
    ///
    /// [ADR 0077]: ../../../../docs/adr/0077-cove-fmt-is-covefmt.md
    pub fn invoke_lowered(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let outcome = self.lowered_checked(module, name, args);
        self.ended(outcome)
    }

    /// The lowering's own check, and then the call.
    fn lowered_checked(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let id = self.lookup(module, name)?;
        let function = self.program.function(id);
        if function.arity() as usize != args.len() {
            return Err(RuntimeError::new(format!(
                "`{module}.{name}` takes {} argument(s), and was given {}",
                function.arity(),
                args.len()
            ))
            .at(function.span));
        }
        self.enter_with(module, name, id, args)
    }

    /// [`Vm::run_entry`], bounded by `budget` and by nothing else.
    ///
    /// The command-shaped way in, bounded the way [`Vm::invoke_within`]
    /// bounds the application-shaped one. Issue #152 is why both exist: an
    /// application that runs somebody else's Cove wants the *request*
    /// bounded, not the session, and a session is built once and invoked
    /// many times.
    pub fn run_entry_within(
        &mut self,
        budget: Budget,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        self.bind_budget(budget);
        let outcome = self.enter(module, name, args);
        self.ended(outcome)
    }

    /// [`Vm::invoke`], bounded by `budget` and by nothing else.
    ///
    /// The check runs before the budget is installed, so a call refused for a
    /// wrong argument spends none of the budget it was handed and leaves
    /// whatever bounded this backend where it was.
    pub fn invoke_within(
        &mut self,
        budget: Budget,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let outcome = self.checked_within(budget, module, name, args);
        self.ended(outcome)
    }

    /// The check, the budget, and then the call.
    fn checked_within(
        &mut self,
        budget: Budget,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        crate::invoke::check(self.runtime.program(), module, name, &args)?;
        self.bind_budget(budget);
        let id = self.lowered(module, name)?;
        self.enter_with(module, name, id, args)
    }

    /// Makes `budget` this run's, with its deadline clock starting now.
    ///
    /// The budget is held here, by the run, and not installed in the
    /// registry: a registry is shared by every run over it, and a budget
    /// installed there was one slot every concurrent run charged and
    /// replaced (issue #577). [`Budget::restart`] is why the deadline runs
    /// from the invocation rather than from wherever `budget` was built.
    fn bind_budget(&mut self, mut budget: Budget) {
        budget.restart();
        self.budget = budget.meter();
    }

    /// The accounting of this run — or of the last one, once it has
    /// answered: the host calls it made, and how long it took.
    ///
    /// This is where an embedder reads what an [`Vm::invoke_within`] spent.
    /// A backend built over a registry with a budget installed by
    /// [`HostRegistry::set_budget`] charges that budget, so there this and
    /// [`HostRegistry::with_budget`] read the same counters.
    pub fn meter(&self) -> &Meter {
        &self.budget
    }

    /// A native-tier session over `module.name`, with `args` converted once.
    ///
    /// [`Vm::invoke`] is the way an application calls a Cove function, and this
    /// is not a second one. It is the boundary ADR 0055's *comparison* needs,
    /// and the difference is the one thing about it worth knowing: `invoke`
    /// converts its arguments, runs the entry, converts the answer back and is
    /// over, and a benchmark that calls one function three hundred thousand
    /// times over the same `String` and the same `Array` cannot use it — the
    /// conversion allocates a fresh object every time, which would be the
    /// measurement.
    ///
    /// So a session converts the arguments once and holds the references in a
    /// frame, which is what keeps them alive; see
    /// [`Session`](crate::NativeSession). Each call is then an ordinary frame on
    /// top of that one, entered on whichever tier
    /// [`Tiered`](crate::Tiered) names.
    ///
    /// This is not how a *run* selects the tier — [`Vm::with_native`] is, and it
    /// is what `cove run --backend native` reaches. What a session adds is that
    /// the table is chosen **per call**, which is the whole of what a differential
    /// harness needs: the same function, the same arguments, once with
    /// [`NothingCompiled`](crate::NothingCompiled) and once with a compiled table,
    /// and the two answers compared. The table is installed on the machine for the
    /// length of each call, so a caller the table refuses runs on the dispatch
    /// loop and a compiled callee it calls is still entered as machine code.
    pub fn native_session(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<native::Session<'_, 'a>, RuntimeError> {
        crate::invoke::check(self.runtime.program(), module, name, &args)?;
        let id = self.lowered(module, name)?;
        let function = self.program.function(id);
        let words = self.words_of(function, &args).map_err(|error| {
            let span = self.program.function(id).span;
            error.at(span)
        })?;
        native::Session::open(&mut self.machine, &self.budget, id, words)
    }

    /// How many instructions this run has executed.
    pub fn instructions(&self) -> u64 {
        self.machine.instructions()
    }

    /// The work this machine has done, in the units its safepoint stride
    /// counts: one per instruction dispatched, one per word a bulk operation
    /// moved, and the IR work compiled code counted statically and paid at its
    /// polls — the coordinate ADR 0040's stop bounds and ADR 0084's yields are
    /// stated in. A test measurement and not an API: cumulative over every
    /// run this machine has made, and the entry task's own.
    #[cfg(test)]
    pub(crate) fn work(&self) -> u64 {
        self.machine.work()
    }

    /// Arms [`crate::vm::exec::Trips`] on this machine, for a test.
    #[cfg(test)]
    /// It also starts the log of the work at every safepoint the machine
    /// takes ([`Vm::safepoints`]).
    pub(crate) fn set_trips(&mut self, trips: Vec<(u64, crate::vm::exec::Trip)>) {
        self.machine.set_trips(trips);
        self.machine.trips.logging = true;
    }

    /// The trips raised so far, with the work at which each was.
    #[cfg(test)]
    pub(crate) fn raised(&self) -> &[(crate::vm::exec::Trip, u64)] {
        &self.machine.trips.raised
    }

    /// The work at every safepoint this machine has taken.
    #[cfg(test)]
    pub(crate) fn safepoints(&self) -> &[u64] {
        &self.machine.trips.safepoints
    }

    /// Words the heap region occupies, free blocks included.
    pub fn heap_words(&self) -> u64 {
        self.machine.heap_words()
    }

    /// Words handed out over the whole run, reuse counted each time.
    pub fn allocated_words(&self) -> u64 {
        self.machine.allocated_words()
    }

    /// Objects handed out over the whole run, reuse counted each time.
    ///
    /// The count beside [`Vm::allocated_words`]'s total, and it is public for a
    /// reason worth writing down: the same figure is in a `--profile` run's
    /// header, and [issue #369](https://github.com/myuon/cove/issues/369)'s
    /// measurement needs it for a run that **cannot be profiled**. ADR 0055
    /// refuses `--profile` beside `--backend native`, because the profiler counts
    /// dispatched opcodes and the native tier dispatches none — so a native run
    /// asked for its allocation count through the profiler would be the VM
    /// wearing the native tier's name. This accessor is a counter the allocator
    /// keeps whichever tier asked it for the memory, so an encoded run and a
    /// mixed one report it in the same words and neither has to be swapped for
    /// the other to read it. The tree-walking interpreter's heap is a set of
    /// objects rather than a run of words and reports its own figures through
    /// `Runtime::heap_stats`; this is the linear memory's.
    pub fn allocations(&self) -> u64 {
        self.machine.allocations()
    }

    /// How many collections this run's heap has done.
    ///
    /// [`Vm::live_words`] is `None` exactly when this is `0`: a heap that has
    /// never collected has nothing that measured what is live.
    pub fn collections(&self) -> u64 {
        self.machine.collected().collections
    }

    /// Words the most recent collection found alive, or `None` if the heap
    /// has never collected.
    ///
    /// [Issue #248](https://github.com/myuon/cove/issues/248) is why this
    /// exists as its own accessor rather than only inside the trace's
    /// `heap_summary` event: `Runtime::heap_stats` is filled in only by the
    /// tree-walking backend (see its doc comment), so a `Vm` embedder asking
    /// "does this run still hold what an early invocation allocated" has
    /// nothing else public to read. `heap_words` and `allocated_words`
    /// answer capacity and a monotonic total; this is the one that answers
    /// what is live right now, as of the last sweep.
    pub fn live_words(&self) -> Option<u64> {
        let collected = self.machine.collected();
        (collected.collections > 0).then_some(collected.live_words)
    }

    /// Where the most recent failed assertion was written, together with the
    /// message it produced, or `None` when no assertion has failed.
    ///
    /// The same answer [`crate::interp::Interpreter::assertion_failure`]
    /// gives, and it is here for the same caller: a test runner points at
    /// the assertion the way every other error points at source. An
    /// assertion that failed and was then handled inside the program is
    /// still recorded, which is why the message is part of the answer — a
    /// caller reports at this span only when the failure it is holding is
    /// this one.
    pub fn assertion_failure(&self) -> Option<(Span, &str)> {
        self.machine.assertion_failure()
    }

    /// The process arguments as the one value an entry may take them as.
    fn enter(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        let id = self.lookup(module, name)?;
        let function = self.program.function(id);
        let arguments = match function.arity() {
            0 => Vec::new(),
            1 => vec![Value::array(args.into_iter().map(Value::string))],
            other => {
                return Err(RuntimeError::new(format!(
                    "entry `{module}.{name}` declares {other} parameters"
                ))
                .at(function.span)
                .with_rule(
                    "An entry function takes either no parameters or one `Array<String>` of process arguments.",
                )
                .with_help(format!(
                    "write `fn {name}()` or `fn {name}(args: Array<String>)`"
                )));
            }
        };
        self.enter_with(module, name, id, arguments)
    }

    /// The check, and then the call.
    fn invoke_checked(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        crate::invoke::check(self.runtime.program(), module, name, &args)?;
        let id = self.lowered(module, name)?;
        self.enter_with(module, name, id, args)
    }

    /// The call itself, from the arguments in to the answer out.
    ///
    /// The one seam. [`Vm::run_entry`] reaches it having turned the process
    /// arguments into the array an entry declares, and [`Vm::invoke`]
    /// reaches it having held a host's own values to what the checker
    /// resolved; nothing below this line knows which of the two happened.
    fn enter_with(
        &mut self,
        module: &str,
        name: &str,
        id: FunctionId,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let function = self.program.function(id);
        let span = function.span;

        self.runtime.trace(TraceEvent::EntryEnter {
            module: module.to_string(),
            function: name.to_string(),
        });
        // Started here and not in `run_entry`, because what this measures is
        // the entry: the argument conversion is the entry's own boundary
        // crossing and the run is what follows it.
        let entered = Entered {
            module: module.to_string(),
            function: name.to_string(),
            returns: function.returns,
            span,
            timing: Timing::start(),
            waited: self.machine.host_wait(),
            descheduled: self.machine.descheduled(),
        };

        let answer = self
            .words_of(function, &args)
            .map_err(|e| e.at(span))
            .and_then(|words| self.machine.run(id, &words, &self.budget));
        if self.machine.is_suspended() {
            // The words are not an answer, and the entry has not left: the
            // rest of this function runs when the run does answer, from
            // [`Vm::resume`]. The value is never read — see `Vm::parkable`.
            debug_assert!(answer.as_ref().is_ok_and(Vec::is_empty));
            self.parked = Some(entered);
            return Ok(Value::unit());
        }
        self.left(entered, answer)
    }

    /// The answer's words as a value, and the two events an entry that has
    /// run ends with: what [`Vm::enter_with`] does once the run answers, there
    /// or — for a run that parked — in [`Vm::resume`].
    fn left(
        &mut self,
        entered: Entered,
        answer: Result<Vec<u64>, RuntimeError>,
    ) -> Result<Value, RuntimeError> {
        let Entered {
            module,
            function,
            returns,
            span,
            timing,
            waited,
            descheduled,
        } = entered;
        let outcome = answer.and_then(|answer| {
            boundary::to_value(&self.machine, returns, &answer).map_err(|e| e.at(span))
        });

        // Both events on every path, the way the oracle writes them: an entry
        // that failed still entered and still left, and a run that failed
        // still allocated. A trace that recorded the exit only for a run that
        // answered would be a trace whose shape depended on the answer.
        self.runtime.trace(TraceEvent::EntryExit {
            module,
            function,
            cpu: timing
                .elapsed()
                .saturating_sub(self.machine.host_wait().saturating_sub(waited))
                .saturating_sub(self.machine.descheduled().saturating_sub(descheduled)),
            wait: self.machine.host_wait().saturating_sub(waited),
        });
        self.summarize_heap();
        outcome
    }

    /// Runs `start` — one of this type's ways in — allowing its host calls
    /// to answer pending, and says whether the run answered or parked.
    ///
    /// [`OwnedVm`]'s parkable entries are this around the ordinary ones, so a
    /// parkable run checks, converts and traces exactly as a blocking one
    /// does. `None` is a parked run, whose entry is in `Vm::parked`; the
    /// value `start` returned for it is the placeholder [`Vm::enter_with`]
    /// leaves, and is dropped here unread.
    fn parkable(
        &mut self,
        start: impl FnOnce(&mut Self) -> Result<Value, RuntimeError>,
    ) -> Option<Result<Value, RuntimeError>> {
        self.machine.allow_parking(true);
        // A request raised for the run this machine last ran, too late for it
        // to see, is not a request for this one.
        self.machine.clear_yield_request();
        let outcome = start(self);
        self.answered(outcome)
    }

    /// Resumes the run parked at a host call with the host's answer, and runs
    /// it until it answers — `Some`, with the terminal events written — or
    /// parks again, `None`.
    fn resume(
        &mut self,
        answer: Result<Value, RuntimeError>,
    ) -> Option<Result<Value, RuntimeError>> {
        let words = self.machine.resume(answer, &self.budget);
        self.went_on(words)
    }

    /// Runs on a run that yielded at a safepoint, until it answers — `Some` —
    /// or parks or yields again, `None` (ADR 0084).
    fn resume_yielded(&mut self) -> Option<Result<Value, RuntimeError>> {
        let words = self.machine.resume_yielded(&self.budget);
        self.went_on(words)
    }

    /// Ends a run that yielded at a safepoint with `stopped`, as that
    /// safepoint would have: the entry's events are written and the run
    /// answers the budget's error.
    fn stop_yielded(&mut self, stopped: Stopped) -> Option<Result<Value, RuntimeError>> {
        let words = self.machine.stop_yielded(stopped, &self.budget);
        self.went_on(words)
    }

    /// What a resumed run came to: `None` if it left the thread again, and
    /// otherwise the entry finished with its words.
    fn went_on(
        &mut self,
        words: Result<Vec<u64>, RuntimeError>,
    ) -> Option<Result<Value, RuntimeError>> {
        if self.machine.is_suspended() {
            return None;
        }
        let entered = self
            .parked
            .take()
            .expect("a parked run's entry is kept until it answers");
        let outcome = self.left(entered, words);
        self.answered(outcome)
    }

    /// What a parkable run comes to: `None` while it is parked, and its
    /// outcome — with [`Vm::ended`]'s event written and parking turned off
    /// again — once it answers.
    fn answered(
        &mut self,
        outcome: Result<Value, RuntimeError>,
    ) -> Option<Result<Value, RuntimeError>> {
        if self.machine.is_suspended() {
            return None;
        }
        self.machine.allow_parking(false);
        Some(self.ended(outcome))
    }

    /// What this run's memory did, recorded once as the run ends.
    ///
    /// The word half of the event and none of the object half. Issue #240
    /// decided that `heap_summary` does not choose between the two — an
    /// inline struct is words here and no object at all on the oracle, so
    /// neither family's figures can be derived from the other's — and the
    /// rule that follows is that a machine leaves `None` in what it does not
    /// count rather than a zero that reads as a measurement.
    ///
    /// `live_words` is one of those. It is what the last collection found
    /// alive, so a run that never collected has nothing that measured it, and
    /// the figure is absent rather than nought. `capacity_words` is not: the
    /// heap region occupies what it occupies whether anything has been swept
    /// or not.
    ///
    /// There is no pause here, and that is the same rule again. This
    /// collector does not time itself yet, and a zero would say it stopped
    /// the world for no time at all.
    fn summarize_heap(&self) {
        self.runtime.trace(TraceEvent::HeapSummary {
            collections: self.collections(),
            object_count: None,
            allocated_bytes: None,
            live_bytes: None,
            peak_bytes: None,
            pause: None,
            allocated_words: Some(self.allocated_words()),
            capacity_words: Some(self.heap_words()),
            live_words: self.live_words(),
        });
    }

    /// The arguments in word form.
    ///
    /// Each conversion allocates and an allocation can collect, so an
    /// argument already converted is held as a temporary root until the frame
    /// that will own it exists. The roots are released here rather than after
    /// the run because nothing between this line and the write of the entry's
    /// frame allocates: [`Machine::run`] reserves stack words and copies the
    /// arguments into them, and the frame is a root from that moment on.
    /// Holding them for the length of the run instead would retain the
    /// entry's arguments past the point the lowering cleared their slots,
    /// which is exactly the retention the static reference map was careful
    /// not to be.
    fn words_of(&mut self, function: &Function, args: &[Value]) -> Result<Vec<u64>, RuntimeError> {
        let params = function.params.clone();
        let mark = self.machine.temps();
        let mut words = Vec::with_capacity(args.len());
        let mut failed = None;
        for (layout, value) in params.iter().zip(args) {
            // Each argument's own words, in declaration order, because that
            // is what the callee's frame is: parameters occupy it from slot 0
            // at their own widths, and a `(Int, Point, Int)` list is four
            // words rather than three slots.
            //
            // `from_value` releases its own temporary roots when it returns,
            // so every reference among the words is re-taken here and held
            // until the frame that will own it exists.
            match boundary::from_value(&mut self.machine, *layout, value) {
                Ok(written) => {
                    let reprs = self.program.layout(*layout).words.clone();
                    for (repr, word) in reprs.iter().zip(&written) {
                        if repr.is_ref() && *word != 0 {
                            self.machine.push_temp(*word);
                        }
                    }
                    words.extend_from_slice(&written);
                }
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
        self.machine.release_temps(mark);
        match failed {
            Some(error) => Err(error),
            None => Ok(words),
        }
    }

    fn lookup(&self, module: &str, name: &str) -> Result<FunctionId, RuntimeError> {
        self.program.function_named(module, name).ok_or_else(|| {
            RuntimeError::new(format!("this package does not declare `{module}.{name}`"))
        })
    }

    /// The same lookup, for a caller that has already established the package
    /// declares the function.
    ///
    /// [`crate::invoke::check`] has passed by the time this runs, so the
    /// package *does* declare it and the reader should not be told it does
    /// not. What is missing is the lowering, and the remedy is the caller's.
    fn lowered(&self, module: &str, name: &str) -> Result<FunctionId, RuntimeError> {
        self.program.function_named(module, name).ok_or_else(|| {
            RuntimeError::new(format!(
                "this run's lowering does not include `{module}.{name}`"
            ))
            .with_rule(
                "A run executes the functions one entry can reach, because that is what `lower_entry` lowers.",
            )
            .with_help(format!(
                "lower it too, by naming `{module}.{name}` as a root, and build the run on that program"
            ))
        })
    }

    /// Writes the run's terminal event, whichever way in produced it.
    ///
    /// Every path into a program passes through here, which is what makes
    /// "every run has one" true rather than a claim about the paths somebody
    /// remembered. An entry that answers `Err` is the program saying what it
    /// was written to say: a failure of the program's work and not of the
    /// run, which is why it is its own outcome rather than one more kind of
    /// stop.
    fn ended(&self, outcome: Result<Value, RuntimeError>) -> Result<Value, RuntimeError> {
        let (classification, message) = match &outcome {
            Ok(value) if value.is_err() => (
                RunOutcome::Error,
                crate::interp::returned_error_message(value),
            ),
            Ok(_) => (RunOutcome::Success, None),
            Err(error) => (error.outcome, Some(error.message.clone())),
        };
        self.runtime.trace(TraceEvent::RunEnded {
            outcome: classification,
            message,
        });
        outcome
    }
}

/// The meter a run charges through, over `hosts`'s budget or over none.
///
/// A registry with no budget installed answers `None`, which has always meant
/// no limit; a meter over default [`Limits`] is that, written down.
fn meter_of(hosts: &HostRegistry) -> Meter {
    hosts
        .budget_meter()
        .unwrap_or_else(|| Budget::new(Limits::default()).meter())
}
