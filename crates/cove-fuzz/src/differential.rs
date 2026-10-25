//! The differential harness of [issue 681]'s priority 2: one program, every
//! way this build can run it, and the answers compared.
//!
//! [ADR 0113] records the decision; `docs/FUZZING.md` is the manual. In short:
//!
//! # The modes
//!
//! | Mode | What runs | Program |
//! |---|---|---|
//! | `ast` | the tree-walking [`Interpreter`] — ADR 0034's oracle | the checked program |
//! | `vm` | the encoded VM, through [`OwnedVm`], over a [`HEAP_WORDS`] heap | `cove_ir::lower_entry` |
//! | `native` | the same, with [`PreparedProgram::with_native`] — the template tier where it compiles a function, the encoded VM where it refuses one | the same |
//! | `vm-gc` | the encoded VM over a [`GC_HEAP_WORDS`] heap, so that a program that allocates at all collects | the same |
//! | `native-gc` | the native tier over the same small heap: compiled frames' roots at a collection | the same |
//! | `image` | the encoded VM over the program read back from `cove_ir::serial` — the path `cove fmt` runs | `serial::decode(serial::encode(..))` |
//!
//! `native` and `native-gc` are left out where this host or build has no
//! native tier, which is not a finding.
//!
//! # What is compared
//!
//! Every pair of modes, on:
//!
//! - **how the run ended** — [`RunOutcome`]: success, the entry returning
//!   `Err`, a trap, a refused host call, a limit;
//! - **what it answered** — the entry's value rendered with `{:?}` (always
//!   `()` or an `Err(Error { .. })`, since a generated `main` returns
//!   `Result<Unit, Error>`), or the [`RuntimeError`]'s message, rule, help,
//!   refused capability, and the span it is blamed on;
//! - **both console streams, byte for byte**, each captured into a buffer of
//!   its own, as `cove-cli/tests/differential.rs` keeps them apart.
//!
//! The span is compared because `cove-cli/tests/differential.rs` compares it
//! over the whole corpus and finds that the evaluators agree; the weaker
//! comparison `cove-runtime`'s own `vm/differential.rs` makes (messages only)
//! dates from before that was true. The library call sites a fault was moved
//! past and the call chain are **not** compared: the inliner decides which
//! frames exist on the lowered side, so how many call sites a chain has is a
//! fact about the lowering and not about the program.
//!
//! Work counts, collection counts, timings and the trace are not compared:
//! none of them is a property of the program.
//!
//! # A stop that belongs to a mode
//!
//! Some ways a run can end are about the mode and not about the program, and
//! two modes are not expected to agree on them:
//!
//! - the **deadline** — wall-clock, so the interpreter, several times slower
//!   than the VM, meets it first;
//! - the heap's **out-of-memory** refusal — a heap's capacity is a property of
//!   the mode: the interpreter has none, `vm-gc` has a small one, and the
//!   lowered program's literals live in it. Between two modes over the same
//!   capacity — `vm`, `native` and `image`, or `vm-gc` and `native-gc` — it
//!   is not a stop but an outcome, compared in full: one of them running out
//!   where the other did not is a leak or a lost collection;
//! - the **call-depth** limit, the VM's stack region running out, and the
//!   interpreter's own fixed limit of 256 frames — the inliner decides how many
//!   frames a call is on the lowered side, and the interpreter's limit is not
//!   the host's;
//! - **cancellation**, which nothing here does, but which is wall-clock if it
//!   happens.
//!
//! Such a run is *stopped*. A stopped run is compared only by its prefix:
//! what it printed on each stream must be a prefix of what the other run
//! printed (or the other way round, when both stopped). That is still a real
//! check — a stopped run that printed a line the other did not is a
//! disagreement — but its outcome and its answer are not compared.
//!
//! The host-call limit is not one of them: both evaluators count the same
//! calls, so a run stopped by it is compared in full.
//!
//! # Determinism
//!
//! A run is granted `console` and `clock`; the console writes into two
//! buffers, the clock is [`Clock::virtual_clock`] — it moves only when the
//! program sleeps, and every mode gets a fresh one at the same origin. No
//! other host is registered, so there is no randomness, no real time, no file
//! system and no network. The deadline is the one wall-clock dependence, and
//! it only ever makes a run *stopped*.
//!
//! # Floating point
//!
//! A `Float` is compared by what the program does with it: printed, it is
//! compared byte for byte, so every finite value and both zeros are compared
//! exactly — `-0.0` prints as `-0.0`, not `0.0` — and every NaN prints as
//! `NaN`, so **NaNs are equal whatever their sign and payload**. That is the
//! rule the language gives, because no operation reads a NaN's bits: there is
//! no `toBits`, `format` writes `NaN` "whichever sign bit it has", and `==`
//! against a NaN is `false` on every mode. A difference in a NaN's payload
//! is therefore not observable and not compared, and one in the sign of a zero
//! is, and is.
//!
//! # What agreement does not show
//!
//! Every mode shares the front end, the standard library's Cove source, and
//! — all but `ast` — the lowering. A bug in any of those is the same bug in
//! every mode and answers the same wrong thing everywhere. Agreement says the
//! modes are consistent; it is not evidence that any of them is right, and it
//! is not a safety guarantee.
//!
//! [issue 681]: https://github.com/myuon/cove/issues/681
//! [ADR 0113]: ../../../docs/adr/0113-differential-fuzzing-compares-six-modes-and-a-stopped-run-by-its-prefix.md

use std::fmt::Write as _;
use std::io::Write;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use cove_diag::SourceMap;
use cove_runtime::interp::Interpreter;
use cove_runtime::trace::RunOutcome;
use cove_runtime::{
    Budget, Cancellation, Clock, Console, GrantSource, Grants, HostRegistry, OwnedVm,
    PreparedProgram, Runtime, RuntimeError, Value, VirtualTime,
};

use crate::stats::{self, Classification};
use crate::{front, limits, on_stack, Lowered, ENTRY, HEAP_WORDS, MODULE};

/// The heap `vm-gc` and `native-gc` run over: 2^11 words, 16 KiB.
///
/// The collector is paced (ADR 0081): it collects once a run has allocated a
/// chunk's worth — 8,192 words — since the last collection, or when an
/// allocation does not fit the heap's capacity. Generated programs allocate
/// less than a chunk, so at 2^14 words or more almost none of them collected
/// at all (measured over sixty random inputs: four did); the capacity is what
/// has to be small. 2^11 still holds what a lowered program carries before
/// its first instruction — its literals, of which `std.float`'s table of
/// powers of five is the largest at about 1,200 words — and a program that
/// needs more stops with the out-of-memory refusal, which is a stop and not a
/// finding.
pub const GC_HEAP_WORDS: usize = 1 << 11;

/// One way of running a program.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// The tree-walking interpreter.
    Ast,
    /// The encoded VM.
    Vm,
    /// The native tier, falling back to the encoded VM per function.
    Native,
    /// The encoded VM over a small heap.
    VmGc,
    /// The native tier over a small heap.
    NativeGc,
    /// The encoded VM over the program read back from its serialised image.
    Image,
    /// The encoded VM again, over the same heap as [`Mode::Vm`], in a machine
    /// of its own: a run that answers differently the second time depends on
    /// something that is not the program. Only the IR harnesses run it.
    Again,
}

impl Mode {
    /// The mode's name, as reports print it.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Ast => "ast",
            Mode::Vm => "vm",
            Mode::Native => "native",
            Mode::VmGc => "vm-gc",
            Mode::NativeGc => "native-gc",
            Mode::Image => "image",
            Mode::Again => "vm-again",
        }
    }

    /// Where the mode sorts among the others, in declaration order: what a
    /// pair of modes is named by in the statistics.
    pub fn rank(self) -> u8 {
        self as u8
    }

    /// The capacity of the heap the mode runs over, or `None` for the
    /// interpreter, which has none.
    pub fn heap_words(self) -> Option<usize> {
        match self {
            Mode::Ast => None,
            Mode::Vm | Mode::Native | Mode::Image | Mode::Again => Some(HEAP_WORDS),
            Mode::VmGc | Mode::NativeGc => Some(GC_HEAP_WORDS),
        }
    }
}

/// What one mode's run of a program could be observed by.
#[derive(Clone, Debug)]
pub struct Observed {
    /// Which mode ran.
    pub mode: Mode,
    /// How the run ended.
    pub outcome: RunOutcome,
    /// The value, or the error's message, rule, help, refused capability and
    /// span, rendered.
    pub answer: String,
    /// What the run wrote to the console's output stream.
    pub stdout: Vec<u8>,
    /// What the run wrote to the console's diagnostic stream.
    pub stderr: Vec<u8>,
    /// Why this run ended in a way that belongs to the mode rather than to
    /// the program, if it did: see the module documentation.
    pub stopped: Option<&'static str>,
    /// Collections the run made, for the lowered modes; for a reader judging
    /// whether `vm-gc` did its job, and never compared.
    pub collections: Option<u64>,
    /// Calls that entered compiled code, for the lowered modes: nought on
    /// `native` means the native tier ran nothing of this program, and the
    /// comparison was of the encoded VM with itself. Never compared.
    pub native_calls: Option<u64>,
    /// Of [`Observed::collections`], those made while a compiled frame was on
    /// the stack ([`OwnedVm::collections_while_compiled`]): what says a run
    /// tested a compiled frame's roots, rather than collecting and running
    /// compiled code at different moments. Never compared.
    pub collections_while_compiled: Option<u64>,
    /// Instructions the encoded dispatch loop ran, for the lowered modes
    /// ([`OwnedVm::instructions`]). Above nought on `vm`, which never enters
    /// compiled code, it says the entry was accepted and its first instruction
    /// ran; on a native mode it leaves out what compiled code ran. Never
    /// compared.
    pub instructions: Option<u64>,
}

/// How one run ended, in the kinds a full comparison is told apart by
/// ([`Basis::Full`]).
///
/// **Success is [`RunOutcome::Success`] and nothing else**: the entry
/// returned, and what it returned is not an `Err`. A `main` that returned
/// `Err(..)` is [`End::Error`]; a guest trap is [`End::Trap`]; a run a limit
/// the host set ended — the heap running out, the host-call or concurrency
/// limit, the deadline, a call-depth limit — is [`End::Limit`]; anything else
/// — a refused host call, cancellation, the debugger — is [`End::Other`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum End {
    /// [`RunOutcome::Success`].
    Success,
    /// [`RunOutcome::Error`]: the entry returned `Err`, which is a failure the
    /// language expresses and not a stop.
    Error,
    /// [`RunOutcome::Invariant`] other than the heap running out: an overflow,
    /// a division by zero, an out-of-range slice, a failed assertion.
    Trap,
    /// A limit the host set.
    Limit,
    /// Anything else.
    Other,
}

impl End {
    /// The kind's name, as the statistics key it.
    pub fn name(self) -> &'static str {
        match self {
            End::Success => "success",
            End::Error => "error",
            End::Trap => "trap",
            End::Limit => "limit",
            End::Other => "other",
        }
    }

    /// How `o` ended.
    pub fn of(o: &Observed) -> End {
        match o.stopped {
            Some("cancelled") => return End::Other,
            Some(_) => return End::Limit,
            None => {}
        }
        match o.outcome {
            RunOutcome::Success => End::Success,
            RunOutcome::Error => End::Error,
            RunOutcome::Invariant => End::Trap,
            RunOutcome::HostCalls
            | RunOutcome::Concurrency
            | RunOutcome::Deadline
            | RunOutcome::CallDepth => End::Limit,
            RunOutcome::HostBoundary | RunOutcome::Cancelled | RunOutcome::Debugger => End::Other,
        }
    }
}

/// On what a pair of runs was compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Basis {
    /// Neither run stopped in a way that belongs to its mode, so the outcome,
    /// the answer and both streams were compared whole. `both` is the kind
    /// both runs ended in, or `None` when they ended in different kinds —
    /// which is always also a disagreement, since two kinds are two outcomes.
    Full {
        /// The kind both runs ended in, if it was one kind.
        both: Option<End>,
    },
    /// One of them stopped in a way that belongs to its mode, so only the
    /// prefix of each stream was compared. `common` is the bytes that
    /// comparison could have found different — the shorter stdout and the
    /// shorter stderr — and nought is an **empty** prefix comparison, one that
    /// compared no output at all.
    Prefix {
        /// The bytes of output the comparison covered.
        common: usize,
    },
}

/// Two runs of one program compared: what [`disagreement`] decides, kept with
/// what it was decided on, so that [`crate::stats`] counts the judgment the
/// harness made and not a second one beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pair {
    /// The earlier run's mode, in the order the runs were made.
    pub a: Mode,
    /// The later run's.
    pub b: Mode,
    /// On what they were compared.
    pub basis: Basis,
    /// Whether either run entered compiled code.
    pub native: bool,
    /// How they disagreed, if they did.
    pub disagreement: Option<String>,
}

impl Pair {
    /// Compared in full, and both runs ended in [`End::Success`].
    pub fn both_succeeded(&self) -> bool {
        self.basis
            == Basis::Full {
                both: Some(End::Success),
            }
    }
}

impl Observed {
    /// The observation as lines a person reads.
    pub fn describe(&self) -> String {
        let mut out = format!("{}: {}", self.mode.name(), self.outcome.as_str());
        if let Some(why) = self.stopped {
            let _ = write!(out, " (stopped: {why})");
        }
        if let Some(n) = self.collections {
            let _ = write!(out, " [{n} collections]");
        }
        if let Some(n) = self.native_calls.filter(|&n| n > 0) {
            let _ = write!(out, " [{n} calls entered compiled code]");
        }
        if let Some(n) = self.collections_while_compiled.filter(|&n| n > 0) {
            let _ = write!(out, " [{n} collections under a compiled frame]");
        }
        let _ = write!(out, "\n    answer: {}", self.answer.replace('\n', "\n    "));
        let _ = write!(
            out,
            "\n    stdout: {:?}",
            String::from_utf8_lossy(&self.stdout)
        );
        if !self.stderr.is_empty() {
            let _ = write!(
                out,
                "\n    stderr: {:?}",
                String::from_utf8_lossy(&self.stderr)
            );
        }
        out
    }
}

/// Every mode's run of one program, and the first disagreement among them.
#[derive(Debug)]
pub struct Report {
    /// One observation per mode that ran, in the order they ran.
    pub observed: Vec<Observed>,
    /// Modes that did not run, and why.
    pub skipped: Vec<(Mode, String)>,
    /// Every pair of [`Report::observed`], compared, in the order the
    /// comparison visits them; the first that disagreed is
    /// [`Report::mismatch`].
    pub pairs: Vec<Pair>,
    /// The first pair that disagreed, and how — or a finding that is no
    /// pair's, such as an image that does not read back.
    pub mismatch: Option<String>,
    /// What the native tier compiled: how many of the reachable functions,
    /// and which of the program's own it refused and why. A generator whose
    /// helpers the tier refuses is comparing the encoded VM with itself.
    pub native: Option<String>,
}

impl Report {
    /// The whole report, as lines a person reads.
    pub fn describe(&self) -> String {
        let mut out = String::new();
        for o in &self.observed {
            let _ = writeln!(out, "{}", o.describe());
        }
        for (mode, why) in &self.skipped {
            let _ = writeln!(out, "{}: skipped ({why})", mode.name());
        }
        if let Some(native) = &self.native {
            let _ = writeln!(out, "native tier: {native}");
        }
        match &self.mismatch {
            Some(why) => {
                let _ = writeln!(out, "MISMATCH: {why}");
            }
            None => {
                let _ = writeln!(out, "agree");
            }
        }
        out
    }
}

/// Generates a well-typed program from `data` with
/// [`generate::differential_from_bytes`](crate::generate::differential_from_bytes),
/// runs it in every mode, and compares them.
///
/// The input is counted in the session's statistics ([`crate::stats`])
/// before anything panics, so a failing input is in the last file written.
///
/// # Panics
///
/// If the program is refused by the front end, or if two modes disagree —
/// with the program's text and every mode's observation.
pub fn differential(data: &[u8]) {
    let (classification, finding) = differential_classified(data);
    stats::record(&classification, finding.is_some());
    if let Some(why) = finding {
        panic!("{why}");
    }
}

/// [`differential`] without the panic: the input's classification, and the
/// message it would have panicked with if it is a finding. For
/// [`differential`] itself and for `cove-fuzz stats`, which replays a corpus
/// and counts its findings rather than stopping at the first.
pub fn differential_classified(data: &[u8]) -> (Classification, Option<String>) {
    let program = crate::generate::differential_from_bytes(data);
    let source = program.clone();
    let ran = on_stack(move || crate::front_staged(&source).map(run_lowered));
    match ran {
        Ok((report, front)) => {
            let classification = stats::classify(TARGET, &report, front);
            let finding = report.mismatch.as_ref().map(|why| {
                format!(
                    "the modes disagree: {why}\n--- program ---\n{program}--- runs ---\n{}",
                    report.describe()
                )
            });
            (classification, finding)
        }
        Err((stage, why)) => (
            Classification::refused(TARGET, stage),
            Some(format!(
                "a generated program was refused:\n{why}\n--- program ---\n{program}"
            )),
        ),
    }
}

const TARGET: &str = "differential";
const SOURCE_TARGET: &str = "differential_source";

/// [`differential`] over a source rather than over the generator's choices:
/// what the `run` target is to `structured`. A source the front end refuses
/// is not a finding.
///
/// Counted in the session's statistics as [`differential`] is.
///
/// # Panics
///
/// If two modes disagree, with the source and every mode's observation.
pub fn differential_source(data: &[u8]) {
    let (classification, finding) = differential_source_classified(data);
    stats::record(&classification, finding.is_some());
    if let Some(why) = finding {
        panic!("{why}");
    }
}

/// [`differential_source`] without the panic; see [`differential_classified`].
pub fn differential_source_classified(data: &[u8]) -> (Classification, Option<String>) {
    let Ok(text) = std::str::from_utf8(data) else {
        return (Classification::not_utf8(SOURCE_TARGET), None);
    };
    let text = text.to_string();
    let source = text.clone();
    let ran = on_stack(move || crate::front_staged(&source).map(run_lowered));
    match ran {
        Ok((report, front)) => {
            let classification = stats::classify(SOURCE_TARGET, &report, front);
            let finding = report.mismatch.as_ref().map(|why| {
                format!(
                    "the modes disagree: {why}\n--- program ---\n{text}\n--- runs ---\n{}",
                    report.describe()
                )
            });
            (classification, finding)
        }
        Err((stage, _)) => (Classification::refused(SOURCE_TARGET, stage), None),
    }
}

/// [`run_every_mode`], with what the statistics need to know about the
/// lowered program before it is run: whether `cove_ir::verify` accepts it,
/// and whether it has the entry every mode is asked to run.
fn run_lowered(lowered: Lowered) -> (Report, stats::Front) {
    let front = stats::Front {
        verified: cove_ir::verify(&lowered.program).is_ok(),
        has_entry: lowered.program.function_named(MODULE, ENTRY).is_some(),
    };
    (run_every_mode(lowered), front)
}

/// Every mode's run of `source`, or the front end's refusal. For the
/// `cove-fuzz` binary's `diff` and `diff-generated` commands, and for tests.
pub fn compare(source: &str) -> Result<Report, String> {
    let source = source.to_string();
    on_stack(move || front(&source).map(run_every_mode))
}

/// Runs `lowered` in every mode this build has, and compares every pair.
fn run_every_mode(lowered: Lowered) -> Report {
    // A source with no `main` lowers to a program with no entry, which every
    // mode refuses alike and which has no image to write.
    let has_entry = lowered.program.function_named(MODULE, ENTRY).is_some();
    let image = has_entry.then(|| {
        cove_ir::serial::encode(&lowered.program, &lowered.sources, MODULE, ENTRY)
            .and_then(|bytes| cove_ir::serial::decode(&bytes).map_err(|why| why.to_string()))
    });
    let checked = Arc::new(lowered.checked);
    let sources = Arc::new(lowered.sources);
    let program = Arc::new(lowered.program);
    let encoded = PreparedProgram::new(Arc::clone(&program));
    let native = PreparedProgram::new(Arc::clone(&program)).with_native();
    let summary = native
        .as_ref()
        .ok()
        .and_then(PreparedProgram::native)
        .map(|n| {
            let refused: Vec<_> = n
                .refusals()
                .iter()
                .filter(|r| r.name.starts_with(MODULE))
                .map(|r| match &r.instruction {
                    Some(op) => format!("{} ({}: {op})", r.name, r.reason),
                    None => format!("{} ({})", r.name, r.reason),
                })
                .collect();
            format!(
                "compiled {} of {} reachable functions; refused of the program's own: [{}]",
                n.compiled(),
                n.reachable(),
                refused.join(", ")
            )
        });

    let mut observed = Vec::new();
    let mut skipped = Vec::new();
    observed.push(lowered_run(
        Mode::Vm,
        &checked,
        &sources,
        encoded.clone(),
        HEAP_WORDS,
    ));
    match &native {
        Ok(native) => {
            observed.push(lowered_run(
                Mode::Native,
                &checked,
                &sources,
                native.clone(),
                HEAP_WORDS,
            ));
        }
        Err(why) => skipped.push((Mode::Native, why.to_string())),
    }
    observed.push(lowered_run(
        Mode::VmGc,
        &checked,
        &sources,
        encoded,
        GC_HEAP_WORDS,
    ));
    match &native {
        Ok(native) => {
            observed.push(lowered_run(
                Mode::NativeGc,
                &checked,
                &sources,
                native.clone(),
                GC_HEAP_WORDS,
            ));
        }
        Err(why) => skipped.push((Mode::NativeGc, why.to_string())),
    }
    match image {
        None => skipped.push((Mode::Image, "the program has no entry".to_string())),
        Some(Ok(image)) => {
            let image_sources = Arc::new(image.sources);
            let prepared = PreparedProgram::new(Arc::new(image.program));
            observed.push(lowered_run(
                Mode::Image,
                &checked,
                &image_sources,
                prepared,
                HEAP_WORDS,
            ));
        }
        Some(Err(why)) => {
            // `encode` and `decode` are each other's inverse over what this
            // build lowered, so failing either is a finding of its own.
            let (pairs, _) = compare_all(&observed);
            return Report {
                observed,
                skipped,
                pairs,
                mismatch: Some(format!("the program's image does not read back: {why}")),
                native: summary,
            };
        }
    }

    // The interpreter has no heap capacity: an allocation the VM refused for
    // want of memory is one the interpreter would make, from the host's own
    // allocator, at whatever size the program asked for. So it does not run
    // a program that ran out of memory on the large heap.
    let ran_out = observed
        .iter()
        .any(|o| matches!(o.mode, Mode::Vm | Mode::Native) && o.stopped == Some(OUT_OF_MEMORY));
    if ran_out {
        skipped.push((
            Mode::Ast,
            "the program ran out of memory on the VM's heap, and the interpreter has none"
                .to_string(),
        ));
    } else {
        observed.insert(0, ast_run(&checked, &sources));
    }

    let (pairs, mismatch) = compare_all(&observed);
    Report {
        observed,
        skipped,
        pairs,
        mismatch,
        native: summary,
    }
}

pub(crate) const OUT_OF_MEMORY: &str = "out of memory";

/// Every pair of `observed` compared, and the first disagreement among them,
/// described.
pub(crate) fn compare_all(observed: &[Observed]) -> (Vec<Pair>, Option<String>) {
    let mut pairs = Vec::new();
    for (i, a) in observed.iter().enumerate() {
        for b in &observed[i + 1..] {
            pairs.push(compare_pair(a, b));
        }
    }
    let first = pairs.iter().find_map(|p| {
        p.disagreement
            .as_ref()
            .map(|why| format!("{} and {}: {why}", p.a.name(), p.b.name()))
    });
    (pairs, first)
}

/// How `a` and `b` disagree, if they do: [`compare_pair`]'s verdict.
pub fn disagreement(a: &Observed, b: &Observed) -> Option<String> {
    compare_pair(a, b).disagreement
}

/// Whether `a` and `b` are compared only by their prefixes, as
/// [`compare_pair`] judges it.
pub fn compared_by_prefix(a: &Observed, b: &Observed) -> bool {
    matches!(compare_pair(a, b).basis, Basis::Prefix { .. })
}

/// `a` and `b` compared under the rules the module documentation states.
///
/// The one place the harness decides both *whether* two runs agree and *on
/// what* they were compared. [`disagreement`] and [`compared_by_prefix`] are
/// its two halves, a [`Report`] keeps every pair's, and the statistics read
/// them from there, so a count cannot describe a different comparison from the
/// one that was made.
pub fn compare_pair(a: &Observed, b: &Observed) -> Pair {
    let native = a.native_calls.unwrap_or(0) > 0 || b.native_calls.unwrap_or(0) > 0;
    let (a_stopped, b_stopped) = stops_between(a, b);
    if a_stopped.is_some() || b_stopped.is_some() {
        let common = a.stdout.len().min(b.stdout.len()) + a.stderr.len().min(b.stderr.len());
        let mut disagreement = None;
        for (stream, x, y) in [
            ("stdout", &a.stdout, &b.stdout),
            ("stderr", &a.stderr, &b.stderr),
        ] {
            let (short, long, short_stopped) = if x.len() <= y.len() {
                (x, y, a_stopped.is_some())
            } else {
                (y, x, b_stopped.is_some())
            };
            // A run that went on printing past the point the other one
            // stopped at is fine; one that printed something else, or that
            // finished having printed less, is not.
            if !long.starts_with(short) || (!short_stopped && short.len() != long.len()) {
                disagreement = Some(format!("{stream} is not a prefix of the other's"));
                break;
            }
        }
        return Pair {
            a: a.mode,
            b: b.mode,
            basis: Basis::Prefix { common },
            native,
            disagreement,
        };
    }
    let disagreement = if a.outcome != b.outcome {
        Some(format!(
            "the run ended `{}` against `{}`",
            a.outcome.as_str(),
            b.outcome.as_str()
        ))
    } else if a.answer != b.answer {
        Some("the answers differ".to_string())
    } else if a.stdout != b.stdout {
        Some("stdout differs".to_string())
    } else if a.stderr != b.stderr {
        Some("stderr differs".to_string())
    } else {
        None
    };
    let (end_a, end_b) = (End::of(a), End::of(b));
    Pair {
        a: a.mode,
        b: b.mode,
        basis: Basis::Full {
            both: (end_a == end_b).then_some(end_a),
        },
        native,
        disagreement,
    }
}

/// Each of `a` and `b`'s stop, if it is one that belongs to the mode *for this
/// pair*.
///
/// Running out of memory is the mode's only when the two modes' heaps differ:
/// `vm`, `native` and `image` share a capacity, as `vm-gc` and `native-gc` do,
/// and one of a pair running out where the other did not is a leak or a lost
/// collection, and compared in full.
fn stops_between(a: &Observed, b: &Observed) -> (Option<&'static str>, Option<&'static str>) {
    let same_heap = a.mode.heap_words().is_some() && a.mode.heap_words() == b.mode.heap_words();
    let stopped = |o: &Observed| o.stopped.filter(|&s| !(same_heap && s == OUT_OF_MEMORY));
    (stopped(a), stopped(b))
}

/// The hosts one run gets, and the buffers its console writes into.
pub(crate) fn hosts(limits: cove_runtime::Limits) -> (HostRegistry, Buffer, Buffer) {
    let (out, err) = (Buffer::default(), Buffer::default());
    let mut hosts = HostRegistry::new(Grants::new(["console", "clock"]));
    hosts.register(Box::new(Console::new(out.clone(), err.clone())));
    hosts.register(Box::new(Clock::virtual_clock(VirtualTime::new())));
    hosts.set_grant_source(GrantSource::Sealed);
    hosts.set_budget(Budget::with_cancellation(limits, Cancellation::new()));
    (hosts, out, err)
}

fn lowered_run(
    mode: Mode,
    checked: &Arc<cove_sema::resolve::Program>,
    sources: &Arc<SourceMap>,
    prepared: PreparedProgram,
    heap_words: usize,
) -> Observed {
    let (hosts, out, err) = hosts(limits());
    let hosts = Arc::new(hosts);
    let runtime = Arc::new(Runtime::new(
        Arc::clone(checked),
        Arc::clone(sources),
        Arc::clone(&hosts),
    ));
    let mut vm = OwnedVm::with_heap_words(runtime, hosts, prepared, heap_words);
    let answer = vm.run_entry(MODULE, ENTRY, Vec::<Rc<str>>::new());
    let collections = vm.collections();
    let native_calls = vm.tiers().native();
    let under_compiled = vm.collections_while_compiled();
    let instructions = vm.instructions();
    drop(vm);
    let mut observed = observe(mode, answer, &out, &err);
    observed.collections = Some(collections);
    observed.native_calls = Some(native_calls);
    observed.collections_while_compiled = Some(under_compiled);
    observed.instructions = Some(instructions);
    observed
}

fn ast_run(checked: &Arc<cove_sema::resolve::Program>, sources: &Arc<SourceMap>) -> Observed {
    let (hosts, out, err) = hosts(limits());
    let runtime = Runtime::new(Arc::clone(checked), Arc::clone(sources), Arc::new(hosts));
    let answer = Interpreter::new(&runtime).run_entry(MODULE, ENTRY, Vec::<Rc<str>>::new());
    observe(Mode::Ast, answer, &out, &err)
}

pub(crate) fn observe(
    mode: Mode,
    answer: Result<Value, RuntimeError>,
    out: &Buffer,
    err: &Buffer,
) -> Observed {
    let (outcome, rendered, stopped) = match &answer {
        Ok(value) if value.is_err() => (RunOutcome::Error, format!("{value:?}"), None),
        Ok(value) => (RunOutcome::Success, format!("{value:?}"), None),
        Err(error) => (error.outcome, described(error), stop_of(error)),
    };
    Observed {
        mode,
        outcome,
        answer: rendered,
        stdout: out.bytes(),
        stderr: err.bytes(),
        stopped,
        collections: None,
        native_calls: None,
        collections_while_compiled: None,
        instructions: None,
    }
}

/// Whether `error` is a stop that belongs to the mode; see the module
/// documentation.
fn stop_of(error: &RuntimeError) -> Option<&'static str> {
    match error.outcome {
        RunOutcome::Deadline => return Some("deadline"),
        RunOutcome::CallDepth => return Some("call depth"),
        RunOutcome::Cancelled => return Some("cancelled"),
        _ => {}
    }
    let message = error.message.as_str();
    if message == "this run has no memory left" {
        Some(OUT_OF_MEMORY)
    } else if message == "this call nests too deeply" {
        Some("the VM's stack region")
    } else if message.starts_with("call depth limit of ") {
        Some("the interpreter's call depth")
    } else {
        None
    }
}

/// An error, as everything about it that is compared.
fn described(error: &RuntimeError) -> String {
    format!(
        "{}\n rule: {:?}\n help: {:?}\n denied: {:?}\n at: {:?}",
        error.message, error.rule, error.help, error.denied_capability, error.span
    )
}

/// A console stream, kept where the harness can read it back.
#[derive(Clone, Default)]
pub(crate) struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Buffer {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().map(|b| b.clone()).unwrap_or_default()
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut b) = self.0.lock() {
            b.extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// One run of a source in one mode over a heap of a chosen capacity, with
/// what the lowering and the native tier made of the functions it names: for
/// tests that need to know a run collected while a particular compiled frame
/// was live (`tests/gc_roots.rs`), which [`compare`]'s fixed heaps do not let
/// them choose.
#[derive(Debug)]
pub struct Probe {
    /// The run.
    pub observed: Observed,
    /// For each function asked about, by its name in `playground`: whether
    /// the lowering kept it as a function of its own (rather than writing its
    /// body into its callers), and whether the native tier compiled it — under
    /// [`Mode::Native`] only; `false` otherwise.
    pub functions: Vec<(String, bool, bool)>,
}

/// `source` run once in `mode` — [`Mode::Ast`], [`Mode::Vm`] or
/// [`Mode::Native`] — over a heap of `heap_words` words (ignored for the
/// interpreter), under the differential targets' limits.
///
/// # Errors
///
/// The front end's diagnostics, if it refuses the source; or a sentence if
/// `mode` is another mode, or is native on a host without the tier.
pub fn probe(
    source: &str,
    mode: Mode,
    heap_words: usize,
    functions: &[&str],
) -> Result<Probe, String> {
    let source = source.to_string();
    let functions: Vec<String> = functions.iter().map(|f| f.to_string()).collect();
    on_stack(move || {
        let lowered = front(&source)?;
        let checked = Arc::new(lowered.checked);
        let sources = Arc::new(lowered.sources);
        let program = Arc::new(lowered.program);
        let kept: Vec<Option<cove_ir::FunctionId>> = functions
            .iter()
            .map(|name| program.function_named(MODULE, name))
            .collect();
        let (observed, compiled) = match mode {
            Mode::Ast => (ast_run(&checked, &sources), vec![false; functions.len()]),
            Mode::Vm => (
                lowered_run(
                    mode,
                    &checked,
                    &sources,
                    PreparedProgram::new(Arc::clone(&program)),
                    heap_words,
                ),
                vec![false; functions.len()],
            ),
            Mode::Native => {
                let native = PreparedProgram::new(Arc::clone(&program))
                    .with_native()
                    .map_err(|why| why.to_string())?;
                let refused: Vec<cove_ir::FunctionId> = native
                    .native()
                    .map(|n| n.refusals().iter().map(|r| r.id).collect())
                    .unwrap_or_default();
                let compiled = kept
                    .iter()
                    .map(|id| id.is_some_and(|id| !refused.contains(&id)))
                    .collect();
                (
                    lowered_run(mode, &checked, &sources, native, heap_words),
                    compiled,
                )
            }
            other => {
                return Err(format!(
                    "`probe` runs ast, vm or native, not {}",
                    other.name()
                ))
            }
        };
        Ok(Probe {
            observed,
            functions: functions
                .into_iter()
                .zip(kept)
                .zip(compiled)
                .map(|((name, id), compiled)| (name, id.is_some(), compiled))
                .collect(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(
        mode: Mode,
        outcome: RunOutcome,
        stdout: &str,
        stopped: Option<&'static str>,
    ) -> Observed {
        Observed {
            mode,
            outcome,
            answer: "()".to_string(),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
            stopped,
            collections: None,
            native_calls: None,
            collections_while_compiled: None,
            instructions: None,
        }
    }

    #[test]
    fn two_finished_runs_are_compared_in_full() {
        let a = ran(Mode::Ast, RunOutcome::Success, "1\n", None);
        assert!(disagreement(&a, &ran(Mode::Vm, RunOutcome::Success, "1\n", None)).is_none());
        assert!(disagreement(&a, &ran(Mode::Vm, RunOutcome::Success, "2\n", None)).is_some());
        assert!(disagreement(&a, &ran(Mode::Vm, RunOutcome::Invariant, "1\n", None)).is_some());
        let mut b = ran(Mode::Vm, RunOutcome::Success, "1\n", None);
        b.stderr = b"x".to_vec();
        assert!(disagreement(&a, &b).is_some(), "the streams are separate");
        b.stderr.clear();
        b.answer = "Err".to_string();
        assert!(disagreement(&a, &b).is_some());
    }

    #[test]
    fn a_stopped_run_is_compared_by_its_prefix() {
        let done = ran(Mode::Vm, RunOutcome::Success, "1\n2\n", None);
        let early = ran(Mode::Ast, RunOutcome::Deadline, "1\n", Some("deadline"));
        assert!(disagreement(&early, &done).is_none());
        assert!(disagreement(&done, &early).is_none());
        // A stopped run that printed what the finished one did not.
        let wrong = ran(Mode::Ast, RunOutcome::Deadline, "3\n", Some("deadline"));
        assert!(disagreement(&wrong, &done).is_some());
        // A finished run cannot have printed less than one that stopped.
        let more = ran(
            Mode::VmGc,
            RunOutcome::Invariant,
            "1\n2\n3\n",
            Some(OUT_OF_MEMORY),
        );
        assert!(disagreement(&done, &more).is_some());
        // Two stopped runs: either may be the shorter.
        assert!(disagreement(&early, &more).is_none());
    }

    #[test]
    fn running_out_of_memory_is_compared_in_full_between_equal_heaps() {
        let oom = |mode| ran(mode, RunOutcome::Invariant, "1\n", Some(OUT_OF_MEMORY));
        let done = |mode| ran(mode, RunOutcome::Success, "1\n2\n", None);
        assert!(disagreement(&done(Mode::Vm), &oom(Mode::Native)).is_some());
        assert!(disagreement(&oom(Mode::VmGc), &done(Mode::NativeGc)).is_some());
        assert!(disagreement(&oom(Mode::Vm), &oom(Mode::Image)).is_none());
        // Different heaps: a stop, compared by its prefix.
        assert!(disagreement(&done(Mode::Vm), &oom(Mode::VmGc)).is_none());
        assert!(disagreement(&done(Mode::Ast), &oom(Mode::Vm)).is_none());
    }

    #[test]
    fn a_mode_specific_stop_is_recognised() {
        let oom = RuntimeError::new("this run has no memory left");
        assert_eq!(stop_of(&oom), Some(OUT_OF_MEMORY));
        let depth = RuntimeError::new("call depth limit of 256 reached while calling `f`");
        assert!(stop_of(&depth).is_some());
        let trap = RuntimeError::new("`Int` addition overflowed");
        assert_eq!(stop_of(&trap), None);
        let deadline = RuntimeError::new("late").with_outcome(RunOutcome::Deadline);
        assert_eq!(stop_of(&deadline), Some("deadline"));
    }

    /// The float rules, end to end: every NaN prints alike, the sign of a
    /// zero is printed, and every mode agrees on both.
    #[test]
    fn floats_are_compared_by_what_the_program_prints() {
        let report = compare(
            "use console.println\n\n/// m\nexport fn main() -> Result<Unit, Error> {\n  let n = 0.0 / 0.0\n  println(\"{n} {-n} {-0.0} {0.0} {n == n} {(-0.0) == 0.0}\")?\n  Ok(())\n}\n",
        )
        .expect("the program checks");
        assert!(report.mismatch.is_none(), "{}", report.describe());
        let ast = &report.observed[0];
        assert_eq!(ast.mode, Mode::Ast);
        assert_eq!(
            String::from_utf8_lossy(&ast.stdout),
            "NaN NaN -0.0 0.0 false true\n"
        );
    }
}
