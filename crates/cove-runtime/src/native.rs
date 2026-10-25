//! The native tier, owned for one lowered `Program`.
//!
//! [ADR 0055] divides the native tier in two. `cove-native` emits machine code
//! for one function and knows nothing else; `vm::exec::native` is the runtime's
//! half of the boundary that code calls back through. What neither of them is, is
//! **the thing that decides which functions to compile and keeps the code alive**
//! — and until [issue #369](https://github.com/myuon/cove/issues/369) nothing
//! was, because the only caller was a comparison harness that built its own.
//!
//! This is that thing: one [`NativeProgram`] per lowered program, compiled
//! eagerly, finalized once, and then immutable for the life of the run.
//!
//! # Why eagerly, and why one finalize
//!
//! ADR 0056 measured the template compiler at **5.4 µs a function**, and drew the
//! consequence itself: "which makes ADR 0055's 'compile every supported reachable
//! function eagerly' cheap enough not to need a hotness counter — a thousand
//! functions is five milliseconds". So there is no hotness counter here, no
//! on-stack replacement and no deoptimization, which is ADR 0055's "Compile
//! functions, not traces" taken literally.
//!
//! One finalize matters more than it looks. A mapping is writable *or*
//! executable and never both, and the template compiler's `Jit::finalize` is the
//! flip — named without a link, because it exists only under a code generator's
//! feature and an intra-doc link to an item a default build does not have is a
//! broken link. Compiling on first call would mean flipping a page while Cove
//! frames stand on it, so everything is compiled before the first entry and
//! nothing is compiled after — which is also why a run cannot "rebuild or
//! refinalize the JIT", a property issue #369 asks for in as many words.
//!
//! # What a refusal is, and what it is not
//!
//! A function the code generator will not take runs on the **encoded** tier.
//! That is not a fallback: the encoded VM is a complete execution path and the
//! semantic reference, and ADR 0055's prohibition is on falling back to the
//! *tree-walking interpreter*, which nothing here can reach. Every refusal is
//! recorded with one stable reason and the first instruction that could not be
//! lowered, so a reader is told which family to build next rather than a number.
//!
//! # Compilation is not execution
//!
//! [`NativeProgram::compile_time`] is reported apart from anything a run
//! measures, because issue #369 requires it: a tier that is fast to run and slow
//! to build is a different trade from one that is neither, and a single wall-clock
//! figure covering both would hide which.
//!
//! [ADR 0055]: ../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use cove_ir::{FunctionId, Program};
use cove_native::{ResumePoints, Unavailable, WindowCode};

use crate::vm::exec::native::{EntryCell, Tiered};
use crate::NativeEntry;

/// When a function is compiled: before the run, or once it has been called
/// often enough to be worth it ([ADR 0123]).
///
/// [ADR 0123]: ../../../docs/adr/0123-the-native-tier-compiles-what-is-hot.md
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TierUp {
    /// Every supported function, before the first instruction runs.
    ///
    /// What `--backend native` named asks for, and what a test of the tier
    /// wants: every function the code generator takes runs as machine code from
    /// its first call, so a fixture called once still tests the code generator.
    Eager,
    /// A function is compiled at the call that makes this many calls to it that
    /// found no machine code, and that call and every later one enter the
    /// compiled code — except a function whose body has a loop, which is
    /// compiled at its first call, because one call of it may run for as long
    /// as the loop does and there is no on-stack replacement to rescue it.
    ///
    /// Counted per function over the life of the [`NativeProgram`], so the runs
    /// that share one — the cases of a property, the files of one `cove fmt` —
    /// share the count. The count is taken where a call already asks the table
    /// for an entry, never at a dispatch. A function running when it becomes
    /// hot finishes on the tier it started on: there is no on-stack replacement.
    AtCall(NonZeroU32),
}

/// The call count [`TierUp::HOT`] compiles a function at.
///
/// Chosen by measurement — see [ADR 0123] for the table it was chosen from.
///
/// [ADR 0123]: ../../../docs/adr/0123-the-native-tier-compiles-what-is-hot.md
pub const HOT_CALLS: u32 = 16;

impl TierUp {
    /// The policy a run that named no backend compiles under.
    pub const HOT: TierUp = match NonZeroU32::new(HOT_CALLS) {
        Some(calls) => TierUp::AtCall(calls),
        None => panic!("a hot function has been called at least once"),
    };
}

/// A function's call count once it has been decided: compiled, refused, or a
/// stub. Nothing counts past it.
const SETTLED: u32 = u32::MAX;

/// Every compiled function of one lowered program, and the pages they live in.
///
/// ADR 0055's `Program + FunctionId -> encoded entry | native entry`, as the type
/// that owns both halves of the answer. It is [`Tiered`], which is the one
/// question the runtime asks of it, and it must **outlive every run that was
/// given it**: dropping it would drop the mappings its entries point into.
///
/// Build it with [`compile()`], or [`compile_tiered`] for a table that compiles
/// as functions become hot, and hand it to
/// [`Vm::with_native`](crate::Vm::with_native).
///
/// # What changes after it is built
///
/// Under [`TierUp::AtCall`], an entry that was `None` becomes `Some` once, when
/// its function becomes hot, and never changes again; its machine code is in a
/// mapping of its own that is never written after it is made executable. So
/// ADR 0055's "code is immutable for the lifetime of its Program" holds of every
/// function's code. What grows is the set of functions that have code, and the
/// figures that count them.
pub struct NativeProgram {
    /// One entry per `FunctionId`, empty for a function that runs encoded.
    entries: Box<[EntryCell]>,
    /// Where each compiled function may be re-entered part-way through, by
    /// `FunctionId` — what lets a run yield inside compiled code and resume on
    /// another thread ([ADR 0085]). Set before its entry is.
    ///
    /// [ADR 0085]: ../../../docs/adr/0085-compiled-frames-resume-where-they-yielded.md
    resumes: Box<[OnceLock<ResumePoints>]>,
    /// Calls to each function that found no machine code, until it is
    /// [`SETTLED`]. Unused under [`TierUp::Eager`].
    ///
    /// A load and a store rather than an atomic add: two runs on two threads
    /// may lose an increment between them, which delays a compilation by a
    /// call and changes no answer, and a locked add on every such call would
    /// be paid by every run on one thread.
    calls: Box<[AtomicU32]>,
    policy: TierUp,
    /// The program, kept to compile from as functions become hot. `None` for a
    /// table compiled eagerly, which never compiles again.
    program: Option<Arc<Program>>,
    /// By function, `cove_ir::verify::places::loaded_uses`, where the
    /// preparation this table was compiled over had already taken it. See
    /// `cove_native::supported_given`.
    loaded: Option<Loaded>,
    reachable: usize,
    stubs: usize,
    /// Whether the code was compiled against the counting helpers. See
    /// [`compile_counting`].
    counts_helpers: bool,
    /// Everything compiling writes: the code generator and the figures.
    state: Mutex<State>,
}

/// By function, the pcs of the loads and stores through an address that may
/// have been loaded out of memory: what the encoded machine's preparation
/// already holds, shared with the native tier's subset.
pub(crate) type Loaded = Arc<[Box<[usize]>]>;

/// What compiling a [`NativeProgram`] writes, behind its lock.
struct State {
    /// The code generator, kept because it owns the pages — and, under
    /// [`TierUp::AtCall`], because it compiles the next hot function.
    #[cfg(feature = "template")]
    jit: cove_native::template::Jit,
    /// One row per refused function, in the order they were refused: by
    /// `FunctionId` for an eager table, by when each became hot otherwise.
    refusals: Vec<Refused>,
    compiled: usize,
    code_bytes: u64,
    /// How much of `code_bytes` is [ADR 0062] buffer windows, by pattern, and
    /// how many windows of each pattern were emitted.
    ///
    /// [Issue #423](https://github.com/myuon/cove/issues/423) asks for it, and
    /// the whole-program total above is why: before this, a report could say a
    /// program grew by 39.8% and nothing could say what of. See [`WindowCode`]
    /// for why the bytes are optional and the sites are not.
    ///
    /// [ADR 0062]: ../../../docs/adr/0062-an-append-is-ensure-store-commit.md
    windows: WindowCode,
    compile: Duration,
}

/// One function that has no machine code, and why.
///
/// The row ADR 0055 asks a native run to record, and issue #369 to rank: "one
/// stable refusal reason per refused function", plus the first instruction that
/// could not be lowered. The dynamic call count that ranks them is *not* here,
/// because it is a fact about a run rather than about a program —
/// [`Vm::refused_calls`](crate::Vm::refused_calls) is where it comes from, and
/// [`Refused::id`] is what joins the two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    /// Which function, and the key into a run's dynamic call counts.
    pub id: FunctionId,
    /// Its qualified name, as a report prints it.
    pub name: String,
    /// The stable reason, as `cove_native::Reason` words it.
    ///
    /// A `String` rather than that enum so that this type exists in a build with
    /// no code generator — the CLI has to be able to *name* the report it cannot
    /// produce. The sentences are the enum's `Display` and are stable in the
    /// sense the ADR means: a family a reader can sort by and act on, not an
    /// instruction.
    pub reason: String,
    /// The pc of the first instruction that could not be lowered, if the refusal
    /// was about an instruction at all.
    pub at: Option<u32>,
    /// That instruction's opcode, by the name `cove_ir::bytecode::Op` gives it.
    ///
    /// The same spelling `encoded.rs`'s own refusal prints, so a reader meeting
    /// one here and one there is meeting one name.
    pub instruction: Option<String>,
    /// What that instruction was *about*, when its opcode is an aggregate.
    ///
    /// See [`Blocked`]. `None` for every opcode that names one operation.
    pub blocked: Option<Blocked>,
    /// Every distinct thing that blocks this function, with how many instructions
    /// each accounts for, most-frequent first.
    pub blockers: Vec<(Blocker, u32)>,
}

/// One distinct thing that blocks a function, as [`Refused::blockers`] groups
/// them.
///
/// The same pair [`Refused::instruction`] and [`Refused::blocked`] name for the
/// *first* blocker, carried so the whole set can be grouped and counted: two
/// instructions with the same opcode and the same subject are one blocker
/// however many pcs they sit at, and two `Alloc`s of different layouts are two.
///
/// A `String` inside for the reason [`Refused::reason`] is: this type has to
/// exist in a build with no code generator, because the CLI must be able to
/// name the report it cannot produce.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Blocker {
    /// The opcode, by the name `cove_ir::bytecode::Op` gives it. Same spelling
    /// as `Refused::instruction`.
    pub instruction: Option<String>,
    /// What that opcode was about, for the aggregate opcodes. See
    /// [`Blocked`].
    pub blocked: Option<Blocked>,
}

/// What a blocking instruction operates on, when naming the opcode is not enough.
///
/// An opcode is the unit [`Refused::instruction`] reports and it is the right one
/// for deciding *whether* to lower a family. It is the wrong one for deciding
/// *what to build*, because the `Alloc` opcodes at the top of a ranked table are
/// an aggregate: every layout a program declares. `IntrinsicCall` was the other,
/// every builtin the language had, until ADR 0073 deleted it with the last
/// variant.
///
/// So this is the second key a census groups by, and it exists for exactly the
/// aggregates. Everything else — `LoadField`, `Str`, `RunFinish` — names
/// one operation already, and inventing a subject for it would add a column that
/// repeats the opcode.
///
/// It is a `String` inside rather than a `cove_ir` or `cove_native` type, for the
/// reason [`Refused::reason`] is: this type has to exist in a build with no code
/// generator, because the CLI must be able to name the report it cannot produce.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Blocked {
    /// [`Inst::Alloc`](cove_ir::Inst::Alloc)'s layout: what is being allocated.
    ///
    /// Both halves are carried because neither is the other. The name is the
    /// family — `covefmt.Token`, `Array<Int>` — and is what a reader recognises;
    /// the shape is `cove_ir::Shape`'s variant, and is what says how much work
    /// lowering it would be. A hundred distinct `Struct` names are one lowering;
    /// one `Vector` is another.
    Allocation {
        /// `Layout::name`, which is qualified for a declared type.
        name: String,
        /// `Shape`'s variant name, with nothing of its contents.
        shape: String,
    },
}

impl Tiered for NativeProgram {
    fn entry(&self, id: FunctionId) -> Option<NativeEntry> {
        self.entries.get(id.index())?.get()
    }

    fn counts_helpers(&self) -> bool {
        self.counts_helpers
    }

    fn table(&self) -> Option<&[EntryCell]> {
        Some(&self.entries)
    }

    fn resume(&self, id: FunctionId) -> Option<&ResumePoints> {
        self.resumes.get(id.index())?.get()
    }

    #[inline]
    fn missed(&self, id: FunctionId) -> Option<NativeEntry> {
        let TierUp::AtCall(hot) = self.policy else {
            return None;
        };
        let calls = self.calls.get(id.index())?;
        let seen = calls.load(Ordering::Relaxed);
        if seen == SETTLED {
            return None;
        }
        let seen = seen + 1;
        // A function with a loop is compiled at its first call: one call of it
        // may run for as long as the loop does, and with no on-stack
        // replacement a later call is too late for that one (ADR 0123).
        if seen >= hot.get() || (seen == 1 && self.loops(id)) {
            return self.tier_up(id);
        }
        calls.store(seen, Ordering::Relaxed);
        None
    }
}

impl NativeProgram {
    /// Functions the lowering reached and gave a body.
    ///
    /// `cove_ir::lower_entry` slices a package by the checker's call graph and
    /// closes the slice against what the lowering names, so "reachable" is not a
    /// second analysis performed here — it is what a lowered program *is*, less
    /// the stubs.
    ///
    /// **Stubs are not counted, and that is the honest reading rather than the
    /// flattering one.** A stub is a declaration `cove_ir::lower` left a
    /// stand-in for because no path the slice followed needs its body, and a
    /// program of this shape has hundreds of them — the example package's other
    /// `[run.<name>]` entries, and every `test fn` beside them. Counted as
    /// reachable they would put the compiled share at a sixth of its true value;
    /// counted as refusals they would fill the ranked table with rows whose
    /// dynamic call count is nought. They are reported by
    /// [`NativeProgram::stubs`], apart, where a reader can see that the
    /// denominator excludes them.
    pub fn reachable(&self) -> usize {
        self.reachable
    }

    /// Declarations the lowering left as stubs, which are neither reachable nor
    /// refused. See [`NativeProgram::reachable`].
    pub fn stubs(&self) -> usize {
        self.stubs
    }

    /// When this table compiles a function.
    pub fn policy(&self) -> TierUp {
        self.policy
    }

    /// Functions that have machine code.
    pub fn compiled(&self) -> usize {
        self.state().compiled
    }

    /// Functions that run on the encoded tier because the code generator
    /// refused them.
    pub fn refused(&self) -> usize {
        self.state().refusals.len()
    }

    /// Reachable functions that have not been asked about yet: under
    /// [`TierUp::AtCall`], those that have not been called often enough to be
    /// compiled. Nought for an eager table.
    ///
    /// **Not a refusal.** Such a function may be one the code generator would
    /// take; it runs on the encoded tier because compiling it was not expected
    /// to pay, and a report that counted it as refused would blame the code
    /// generator for a policy.
    pub fn not_hot(&self) -> usize {
        let state = self.state();
        self.reachable - state.compiled - state.refusals.len()
    }

    /// Every refused function: in `FunctionId` order for an eager table, in the
    /// order they became hot otherwise.
    ///
    /// Unranked on purpose: ranking needs a run's dynamic call counts, which this
    /// type does not have and must not pretend to. A copy, because a table that
    /// compiles as functions become hot may refuse another while it is read.
    pub fn refusals(&self) -> Vec<Refused> {
        self.state().refusals.clone()
    }

    /// Bytes of machine code emitted, over every compiled function.
    pub fn code_bytes(&self) -> u64 {
        self.state().code_bytes
    }

    /// Which part of [`code_bytes`](Self::code_bytes) is ADR 0062's buffer
    /// windows, and how many of each pattern were emitted.
    ///
    /// The bytes are `None` from a code generator that cannot attribute them,
    /// which is a fact about the generator rather than about the program —
    /// [`WindowCode`] says which and why. The sites are counted either way.
    pub fn window_code(&self) -> WindowCode {
        self.state().windows
    }

    /// What compiling cost, which is never part of what executing cost.
    ///
    /// Under [`TierUp::AtCall`] the compiling happens *during* the runs, at the
    /// calls that made a function hot, and this is its sum so far: a caller
    /// that timed a run and wants its execution alone subtracts it.
    pub fn compile_time(&self) -> Duration {
        self.state().compile
    }

    /// The lock on what compiling writes. A poisoned lock is taken anyway:
    /// nothing a panic could have left half-written there is read unchecked.
    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Whether `id`'s body has a loop: an instruction that may transfer control
    /// to itself or to one before it.
    ///
    /// Asked once per function, at its first call that found no machine code,
    /// so it is one pass over a body that is about to run anyway.
    fn loops(&self, id: FunctionId) -> bool {
        let Some(program) = self.program.as_deref() else {
            return false;
        };
        let mut back = false;
        for (pc, inst) in program.function(id).code.iter().enumerate() {
            inst.targets(program, &mut |to| back |= to as usize <= pc);
            if back {
                return true;
            }
        }
        false
    }

    /// Compiles `id`, which has just become hot, and answers its entry — or
    /// `None` if the code generator refused it, which is then recorded.
    ///
    /// Out of line and cold, because a function reaches it once.
    #[cold]
    #[inline(never)]
    fn tier_up(&self, id: FunctionId) -> Option<NativeEntry> {
        let program = self.program.as_deref()?;
        let mut state = self.state();
        // Another run may have compiled it while this one waited for the lock.
        if let Some(entry) = self.entry(id) {
            return Some(entry);
        }
        let calls = &self.calls[id.index()];
        if calls.load(Ordering::Relaxed) == SETTLED {
            return None;
        }
        calls.store(SETTLED, Ordering::Relaxed);
        let (entry, resumes) = compile_one(&mut state, program, id, loaded_of(&self.loaded, id))?;
        // The resume points first: a run that finds the entry may yield inside
        // the code at once, and its resume asks for them.
        let _ = self.resumes[id.index()].set(resumes);
        self.entries[id.index()].set(entry);
        Some(entry)
    }
}

/// Compiles every supported function of `program`, or says why this host cannot.
///
/// The whole table, eagerly, then one `finalize`: [`TierUp::Eager`]. See
/// [`Unavailable`] for what a refusal of the *host* is: a capability
/// diagnostic, never an attempted fallback.
///
/// Three ways it can answer `Err`, and all three are the host's rather than the
/// program's: this build has no code generator, this architecture is not x86-64,
/// or the operating system would not hand out an executable mapping. A *program*
/// never makes this fail — a function the generator refuses is recorded and runs
/// encoded.
pub fn compile(program: &Program) -> Result<NativeProgram, Unavailable> {
    compile_with(Source::Borrowed(program), false, TierUp::Eager, None, None)
}

/// [`compile()`] under `policy`: under [`TierUp::AtCall`], nothing is compiled
/// here and each function is compiled at the call that makes it hot.
///
/// Takes the program by `Arc` because a table that compiles later keeps it.
/// `counting` is [`compile_counting`]'s question.
pub fn compile_tiered(
    program: Arc<Program>,
    policy: TierUp,
    counting: bool,
) -> Result<NativeProgram, Unavailable> {
    compile_with(Source::Shared(program), counting, policy, None, None)
}

/// [`compile_tiered`] over a program the encoded machine has already prepared:
/// verified — or found not to verify — and its loaded addresses taken, so that
/// neither is asked twice (issue #713).
pub(crate) fn compile_prepared(
    program: &Arc<Program>,
    policy: TierUp,
    counting: bool,
    verified: bool,
    loaded: Loaded,
) -> Result<NativeProgram, Unavailable> {
    compile_with(
        Source::Shared(Arc::clone(program)),
        counting,
        policy,
        Some(verified),
        Some(loaded),
    )
}

/// The program a table is compiled from: borrowed for an eager table, which
/// never reads it again, and shared for one that compiles as functions become
/// hot.
enum Source<'p> {
    Borrowed(&'p Program),
    Shared(Arc<Program>),
}

impl Source<'_> {
    fn program(&self) -> &Program {
        match self {
            Source::Borrowed(program) => program,
            Source::Shared(program) => program,
        }
    }
}

/// Whether this build and this host have a native tier at all, answered
/// without a program.
///
/// [`compile()`] over the empty program: the same three host refusals it can
/// answer — no code generator in this build, not x86-64, no executable mapping
/// — and nothing a program could add, because a program never makes
/// [`compile()`] fail. What a command asks when nobody named a backend
/// ([ADR 0122]): the fastest tier this host has, which is this one where it
/// answers `Ok` and the encoded VM where it does not.
///
/// [ADR 0122]: ../../../docs/adr/0122-with-no-backend-a-command-runs-on-the-fastest-tier-this-host-has.md
pub fn available() -> Result<(), Unavailable> {
    compile(&Program::default()).map(|_| ())
}

/// [`compile()`], binding helpers that count each call compiled code makes into
/// the runtime.
///
/// What a run that wants a [`BoundaryReport`](crate::BoundaryReport)'s
/// native-to-runtime counts compiles with. The code is the code [`compile()`]
/// emits — the only difference is the helper addresses it loads — and each
/// helper is one counter write in front of the production body; see
/// [`native_helpers_counting`](crate::native_helpers_counting). A run that did not
/// ask for the counts should not use this: it is a table that pays for a question
/// nobody asked.
pub fn compile_counting(program: &Program) -> Result<NativeProgram, Unavailable> {
    compile_with(Source::Borrowed(program), true, TierUp::Eager, None, None)
}

// The template arm answers a byte `Inst::RunFind` in emitted code when the
// matcher finishes within this many turns, which is a promise that the
// helper's matcher would have finished in its first step, with no poll —
// true only while it is the stride `encoded::run_find_bytes` steps by.
#[cfg(feature = "template")]
const _: () = assert!(cove_native::template::SHORT_FIND_TURNS as u64 == crate::SAFEPOINT_STRIDE);

/// The table for `source` under `policy`, with `verified` the answer
/// `cove_ir::verify` already gave and `loaded` the loaded addresses already
/// taken, or `None` to ask them here.
#[cfg(feature = "template")]
fn compile_with(
    source: Source<'_>,
    counting: bool,
    policy: TierUp,
    verified: Option<bool>,
    loaded: Option<Loaded>,
) -> Result<NativeProgram, Unavailable> {
    use crate::wallclock::Instant;

    let program = source.program();
    let helpers = match counting {
        false => crate::native_helpers(),
        true => crate::native_helpers_counting(),
    };
    // Direct native-to-native calls are on, which is PR #368 and what issue #369
    // says to measure: "Direct native-to-native calls from PR #368 are enabled."
    //
    // And a direct call opens its frame in emitted code when it fits (ADR 0079)
    // — except in a table that counts its helper calls, whose whole point is to
    // see every call reach `open`.
    let jit = cove_native::template::Jit::new(helpers)?;
    let jit = match counting {
        false => jit.opening_frames_inline(),
        true => jit.calling_directly(),
    };
    let functions = program.functions.len();
    let mut state = State {
        jit,
        refusals: Vec::new(),
        compiled: 0,
        code_bytes: 0,
        windows: WindowCode::default(),
        compile: Duration::ZERO,
    };
    let entries: Box<[EntryCell]> = (0..functions).map(|_| EntryCell::default()).collect();
    let resumes: Box<[OnceLock<ResumePoints>]> = (0..functions).map(|_| OnceLock::new()).collect();
    let calls: Box<[AtomicU32]> = (0..functions).map(|_| AtomicU32::new(0)).collect();
    let stubs = program
        .functions
        .iter()
        .filter(|function| function.is_stub())
        .count();
    // One clock over the whole of the eager loop rather than one per function:
    // what issue #369 asks to keep apart from execution is the *total*.
    let started = Instant::now();
    // The code generator reads the program as freely as the encoder does, so
    // it is handed only a program `cove_ir::verify` accepts (ADR 0120). One
    // that does not verify compiles nothing: every function is refused, and
    // the encoded machine it would fall back to refuses the program itself
    // before its first instruction. A preparation that has already asked says
    // so, and is not asked again (issue #713).
    let verified = verified.unwrap_or_else(|| cove_ir::verify(program).is_ok());
    let eager = !verified || policy == TierUp::Eager;
    if eager {
        let mut done = Vec::new();
        for at in 0..functions {
            let id = FunctionId(at as u32);
            calls[at].store(SETTLED, Ordering::Relaxed);
            if !verified {
                state.refusals.push(Refused {
                    id,
                    name: program.function(id).qualified(),
                    reason: "the program does not verify".to_string(),
                    at: None,
                    instruction: None,
                    blocked: None,
                    blockers: Vec::new(),
                });
                continue;
            }
            // A stub is a declaration with no body in this slice, so it is
            // neither compiled nor refused. See `NativeProgram::reachable` for
            // why counting it either way would misreport the run.
            if program.function(id).is_stub() {
                continue;
            }
            match state.jit.compile_given(program, id, loaded_of(&loaded, id)) {
                Some(compiled) => {
                    state.code_bytes += u64::from(compiled.code_bytes);
                    state.windows.add(&compiled.windows);
                    done.push((id, compiled));
                }
                None => state.refusals.push(refused_row(program, id)),
            }
        }
        // Before any entry, and once. Nothing is compiled after this line, so
        // no page is ever made writable again while a Cove frame stands on it.
        state.jit.finalize()?;
        state.compiled = done.len();
        for (id, compiled) in done {
            let _ = resumes[id.index()].set(state.jit.resume_points(compiled));
            entries[id.index()].set(state.jit.entry(compiled));
        }
    } else {
        // Nothing compiled yet, and a stub never is.
        for (at, function) in program.functions.iter().enumerate() {
            if function.is_stub() {
                calls[at].store(SETTLED, Ordering::Relaxed);
            }
        }
    }
    state.compile = started.elapsed();
    Ok(NativeProgram {
        entries,
        resumes,
        calls,
        policy,
        program: match (eager, source) {
            (false, Source::Shared(program)) => Some(program),
            // A borrowed program cannot be kept, so a table over one is
            // compiled eagerly whatever it was asked: `compile` and
            // `compile_counting` are the two that borrow, and both ask for it.
            _ => None,
        },
        loaded,
        reachable: functions - stubs,
        stubs,
        counts_helpers: counting,
        state: Mutex::new(state),
    })
}

/// `id`'s loaded addresses, where they were handed over.
fn loaded_of(loaded: &Option<Loaded>, id: FunctionId) -> Option<&[usize]> {
    loaded.as_deref()?.get(id.index()).map(|pcs| &pcs[..])
}

/// Compiles one function that has become hot, into its own mapping, and makes
/// that mapping executable — or records why it was refused.
///
/// A page that is flipped here holds only this function, which nothing can
/// have entered yet, so no page under a running Cove frame is ever writable:
/// the invariant the eager table keeps by finalizing once, kept per mapping.
#[cfg(feature = "template")]
fn compile_one(
    state: &mut State,
    program: &Program,
    id: FunctionId,
    loaded: Option<&[usize]>,
) -> Option<(NativeEntry, ResumePoints)> {
    use crate::wallclock::Instant;

    if program.function(id).is_stub() {
        return None;
    }
    let started = Instant::now();
    let answer = match state.jit.compile_given(program, id, loaded) {
        Some(compiled) => match state.jit.finalize() {
            Ok(()) => {
                state.compiled += 1;
                state.code_bytes += u64::from(compiled.code_bytes);
                state.windows.add(&compiled.windows);
                Some((state.jit.entry(compiled), state.jit.resume_points(compiled)))
            }
            // The host would not make the page executable. The function runs
            // encoded, and says why.
            Err(unavailable) => {
                state.refusals.push(Refused {
                    id,
                    name: program.function(id).qualified(),
                    reason: unavailable.to_string(),
                    at: None,
                    instruction: None,
                    blocked: None,
                    blockers: Vec::new(),
                });
                None
            }
        },
        None => {
            state.refusals.push(refused_row(program, id));
            None
        }
    };
    state.compile += started.elapsed();
    answer
}

/// One refused function's row: the reason, the pc, the opcode, and every
/// distinct blocker behind it.
#[cfg(feature = "template")]
fn refused_row(program: &Program, id: FunctionId) -> Refused {
    let function = program.function(id);
    let refusal = cove_native::refusal(program, function)
        .expect("a function with no compiled code was refused for a reason");
    let instruction = refusal.at.and_then(|pc| opcode_at(function, pc));
    let blocked = refusal.at.and_then(|pc| blocked_on(program, function, pc));
    Refused {
        id,
        name: function.qualified(),
        reason: refusal.reason.to_string(),
        at: refusal.at,
        instruction,
        blocked,
        blockers: grouped_blockers(program, function),
    }
}

/// Every distinct blocker of `function`, most-frequent first.
///
/// `cove_native::blockers` answers one `Refusal` per refused instruction —
/// including repeats, when a body is refused at the same opcode more than once
/// — and this is where they are grouped into the set [`Refused::blockers`]
/// reports: the same `(instruction, blocked)` pair [`refused_row`]'s own first
/// blocker is named by, counted.
///
/// Sorted descending by count and then ascending by the [`Blocker`] itself, so
/// the table is the same table twice for one run — `Coverage::taken`'s sort has
/// the same tie-break rationale, over a different key.
#[cfg(feature = "template")]
fn grouped_blockers(program: &Program, function: &cove_ir::Function) -> Vec<(Blocker, u32)> {
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<Blocker, u32> = BTreeMap::new();
    for refusal in cove_native::blockers(program, function) {
        let instruction = refusal.at.and_then(|pc| opcode_at(function, pc));
        let blocked = refusal.at.and_then(|pc| blocked_on(program, function, pc));
        *counts
            .entry(Blocker {
                instruction,
                blocked,
            })
            .or_insert(0) += 1;
    }
    let mut rows: Vec<(Blocker, u32)> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows
}

/// What the instruction at `pc` operates on, for the opcodes that aggregate.
///
/// See [`Blocked`] for why only those. The `None` arm is the ordinary answer and
/// not a failure: most opcodes are their own subject.
#[cfg(feature = "template")]
fn blocked_on(program: &Program, function: &cove_ir::Function, pc: u32) -> Option<Blocked> {
    use cove_ir::Inst;
    match function.code.get(pc as usize)? {
        Inst::Alloc { layout, .. } => {
            let held = program.layout(*layout);
            Some(Blocked::Allocation {
                name: allocation_name(program, *layout),
                // The variant and none of its contents: a `Shape::Struct`'s
                // fields are the layout's own description and the name already
                // says which struct this is.
                shape: shape_name(&held.shape).to_string(),
            })
        }
        _ => None,
    }
}

/// What a layout is called in the census, with its element where it has one.
///
/// `Layout::name` alone is not enough for the collection shapes, and it is the one
/// place this matters. `cove_ir::lower` names **every** `Array<T>` layout `Array`
/// and every `Vector<T>`'s element store `Vector` — the element is in the shape,
/// not in the name — so a census keyed on the name alone collapses `Array<Int>`
/// and `Array<covefmt.Token>` into one row. Those are not one row for the purpose
/// the table exists for: the element's width is most of what lowering an
/// allocation of it costs.
///
/// So the element is appended, one level deep and no further. One level, because
/// the element of an element is the *element's* business and a fully expanded
/// `Vector<Array<Token>>` is a type signature rather than a table cell.
#[cfg(feature = "template")]
fn allocation_name(program: &Program, layout: cove_ir::LayoutId) -> String {
    use cove_ir::Shape;
    let held = program.layout(layout);
    let named = |elem: cove_ir::LayoutId| format!("{}<{}>", held.name, program.layout(elem).name);
    match &held.shape {
        Shape::Elements { elem, .. } | Shape::Vector { elem } | Shape::Members { elem } => {
            named(*elem)
        }
        Shape::Shared { value } => named(*value),
        Shape::Entries { key, value } => format!(
            "{}<{}, {}>",
            held.name,
            program.layout(*key).name,
            program.layout(*value).name
        ),
        _ => held.name.to_string(),
    }
}

/// `Shape`'s variant name, which `Debug` would print with its contents.
///
/// Written out rather than derived from `Debug`, because a `Shape::Struct`'s
/// `Debug` is every field it has and a table cell is one word. A `match` also
/// means a new shape is a compile error here rather than a silently different
/// string.
#[cfg(feature = "template")]
fn shape_name(shape: &cove_ir::Shape) -> &'static str {
    use cove_ir::Shape;
    match shape {
        Shape::Free => "Free",
        Shape::Word(_) => "Word",
        Shape::Struct { .. } => "Struct",
        Shape::Enum { .. } => "Enum",
        Shape::Str => "Str",
        Shape::Bytes => "Bytes",
        Shape::IdentityTable => "IdentityTable",
        Shape::Elements { .. } => "Elements",
        Shape::Vector { .. } => "Vector",
        Shape::ByteBuffer => "ByteBuffer",
        Shape::Members { .. } => "Members",
        Shape::Entries { .. } => "Entries",
        Shape::Closure { .. } => "Closure",
        Shape::Shared { .. } => "Shared",
        Shape::Boxed => "Boxed",
    }
}

/// The opcode of `function`'s instruction at `pc`, by the name `Op` gives it.
///
/// Through the encoder rather than through `Inst`'s own `Debug`, because the
/// encoder is what settles the *name*: one `Inst::Arith` is several opcodes, and
/// `Arith(Int, Add)` is the spelling `encoded.rs`'s refusal already prints for
/// the same instruction. An instruction with no encoding — which
/// `encoded::prepare` would have refused the program for — answers `None` rather
/// than a guess.
#[cfg(feature = "template")]
fn opcode_at(function: &cove_ir::Function, pc: u32) -> Option<String> {
    use cove_ir::bytecode::{encode, Op};
    let inst = function.code.get(pc as usize)?;
    let held = encode(inst, pc).ok()?;
    Op::from_number(held.opcode()).map(|op| format!("{op:?}"))
}

/// Never reached without a code generator: no table is ever built.
#[cfg(not(feature = "template"))]
fn compile_one(
    _state: &mut State,
    _program: &Program,
    _id: FunctionId,
    _loaded: Option<&[usize]>,
) -> Option<(NativeEntry, ResumePoints)> {
    None
}

/// Native execution is not in this build.
///
/// ADR 0055's adoption gate asks that "a build without the native feature has no
/// executable-memory dependency", so the code generator is an optional feature,
/// off by default in this crate (ADR 0076 turns it on by default only in the
/// `cove` binary). Selecting the native tier without it is therefore a
/// **capability diagnostic** and not a silent fallback — the same answer the
/// x86-64-only generator gives on another architecture, and for the same reason.
#[cfg(not(feature = "template"))]
fn compile_with(
    _source: Source<'_>,
    _counting: bool,
    _policy: TierUp,
    _verified: Option<bool>,
    _loaded: Option<Loaded>,
) -> Result<NativeProgram, Unavailable> {
    Err(Unavailable::new(
        "this build has no native code generator: it was built without the `template` \
         feature, which the `cove` binary turns on by default",
    ))
}

#[cfg(all(test, feature = "template"))]
mod tests {
    use super::*;
    use cove_ir::{Function, Inst, Layout, LayoutId, Len, RefMap, Repr, Shape};
    use std::sync::Arc;

    /// **An allocation census row names the family and the shape.**
    ///
    /// It is here rather than in `tests/native_tier.rs` because of what happened
    /// to the case that used to be there: `Inst::Alloc` became a lowered
    /// instruction, so no function is refused at one any more and the row the
    /// fixture used to produce stopped existing. The *report* is a fact about a
    /// program and an operand bound could still refuse an allocation, so the arm
    /// stays — and a test of it that depends on which instructions today's subset
    /// happens to lower is a test that retires itself.
    ///
    /// Both halves are asserted because neither is the other, which is
    /// [`Blocked::Allocation`]'s own note: `cove_ir::lower` names every
    /// `Array<T>` layout `Array`, so a census keyed on the name alone collapses
    /// `Array<Int>` and `Array<covefmt.Token>` into one row.
    #[test]
    fn an_allocation_census_row_names_the_family_and_the_shape() {
        let span = cove_diag::Span::new(cove_diag::FileId(0), 0, 0);
        let reprs = vec![Repr::Ref];
        let code = vec![
            Inst::Alloc {
                dst: 0,
                layout: LayoutId(2),
                len: Len::Count(3),
            },
            Inst::Return { src: 0 },
        ];
        let function = Function {
            module: Arc::from("m"),
            name: Arc::from("f"),
            params: Vec::new(),
            spans: vec![span; code.len()],
            refs: RefMap::of(&reprs),
            reprs,
            returns: LayoutId(2),
            captures: Vec::new(),
            code,
            locals: Vec::new(),
            inlined: Vec::new(),
            span,
            is_async: false,
            stub: false,
        };
        let program = Program {
            functions: vec![function],
            layouts: vec![
                Layout::free(),
                Layout::word("Int", Repr::Int),
                Layout::object(
                    "Array",
                    Shape::Elements {
                        elem: LayoutId(1),
                        growable: false,
                    },
                ),
            ],
            ..Program::default()
        };
        let function = program.function(FunctionId(0));
        assert_eq!(
            blocked_on(&program, function, 0),
            Some(Blocked::Allocation {
                // The element is beside the family, because `Array` alone would
                // be every array the program has.
                name: "Array<Int>".to_string(),
                shape: "Elements".to_string(),
            })
        );
        // And an instruction that is its own subject carries no second key.
        assert_eq!(blocked_on(&program, function, 1), None);
    }
}
