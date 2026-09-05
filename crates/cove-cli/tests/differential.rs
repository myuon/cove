//! The whole corpus, run on the linear-memory backend and on the oracle,
//! compared answer for answer.
//!
//! [ADR 0034](../../../docs/adr/0034-one-physical-word-stack.md) makes the
//! linear-memory backend the thing a `cove` command runs a program on, and
//! keeps the tree-walking interpreter as the definition of what a Cove
//! program means. Its sixth completion condition is what this file is: the
//! full differential corpus agrees with the tree-walking oracle, *including
//! values, errors, source spans and trace events*. Every program the
//! repository keeps under `tests/e2e/` and `examples/` — every `[run.<name>]`
//! table of every `cove.toml` there — is lowered, run on both against the
//! same deterministic fakes, and compared.
//!
//! # Two runs, one oracle
//!
//! [Issue #245](https://github.com/myuon/cove/issues/245)'s Phase 5 cut
//! production execution over to
//! [ADR 0041](../../../docs/adr/0041-a-slot-number-fits-in-sixteen-bits.md)'s
//! sixteen-byte encoding and deleted the `Inst` dispatch loop, so there is
//! one thing here that executes a lowered program and it is the encoded one.
//! Through Phase 4 this file ran a third pass to compare the two loops'
//! answers; that comparison went with the loop.
//!
//! What did not change is what it is compared *against*. Every pass here is
//! held to the **oracle**, never to another pass: two backends that agreed
//! with each other and not with the language would pass a test that compared
//! them, and ADR 0034 makes the tree-walking interpreter the definition of
//! what a Cove program means precisely so that there is one thing to be
//! right about. Deleting a path therefore weakened nothing — the comparison
//! that remains is the one that was always doing the work.
//!
//! ADR 0034's sixth completion condition is what is applied: values, errors,
//! source spans and whole traces. A `spawn` pushes a child into a table, a
//! `scope.leave` joins threads in an order, and a failure leaves the frames,
//! the cells and the scopes where a host that catches it will find them.
//! None of that is visible in an answer; the trace and the console are where
//! it shows, which is why this harness rather than a survey is the arbiter.
//!
//! # Why this stands beside `vm_coverage.rs`
//!
//! Two harnesses walk this corpus and they ask different questions, so
//! neither is the other's superset.
//!
//! `vm_coverage.rs` is the roadmap and the ratchet. It takes `benches/` in
//! as well, so it covers more programs, and it compares what both evaluators
//! can be asked for cheaply — the value or the failure's message, both
//! console streams, how the run ended, and the files the run wrote. It
//! reports rather than stops, so a program that lowers and then lies is a
//! recorded finding and the survey goes on past it.
//!
//! This one is the richer comparison, and it is the one ADR 0034's sixth
//! condition names. It compares the failure's *span* and the whole *trace* —
//! every host call's arguments and outcome, every task's spawn and end, the
//! entry's two events and the run's terminal one — as the JSONL lines a
//! `--trace` file would hold rather than as events. Those are the two things
//! a survey that also runs `benches/` cannot afford, for the reason
//! [`discover`] gives. That one compares more programs; this one compares
//! more of each program.
//!
//! # Every case that checks, lowers
//!
//! The predecessor had an admission predicate. Its lowering answered "would
//! you run this?" and refused what it did not cover, so most of a corpus
//! never reached it, and the honest measurement was a count of what did: this
//! file carried a floor under that count and a register naming every
//! construct a refusal was allowed to be for, so that a language feature the
//! checker accepted and the backend refused could not arrive unnoticed.
//!
//! ADR 0034 deletes the predicate. There is no `Unsupported`, no `admits`,
//! and no vocabulary of refusals — a construct the lowering has not been
//! taught is a bug in the lowering, answered as `Vec<Diagnostic>` like any
//! other compiler fault. So "which constructs may this backend refuse" is not
//! a question that exists any more, and neither a floor nor a register of
//! refusals has anything left to be about.
//!
//! The ratchet did not go with them; it got stronger. Every case that checks
//! must lower, and the only floor that is honest for a backend with no
//! admission predicate is zero. A floor at ninety-six out of a hundred and
//! twenty-two was the strongest claim available while the question existed;
//! with the question gone, anything short of all of them is a bug carrying
//! its own diagnostic, and the assertion says exactly that. A case that does
//! not lower fails this test with what the lowering said printed beside it,
//! rather than being counted as coverage of anything.
//!
//! What is kept is the count of cases whose *package* does not check.
//! `tests/e2e/` holds those on purpose, each pinning a check-time diagnostic,
//! and a program `cove check` refuses never reaches an evaluator at all — so
//! they are counted apart rather than folded into anything this backend did
//! or did not do.
//!
//! # A case is a program, not a package
//!
//! `tests/e2e/` is seventy unrelated programs sharing one package for the
//! convenience of the harness that runs them, so a case is measured as the
//! program it is rather than as the package it sits in — and it is measured
//! that way twice over.
//!
//! Checking is sliced by module: a case is parsed and checked as its entry's
//! module plus the modules that module's `use` declarations reach,
//! transitively. That slicing is not a workaround but the corpus's own
//! shape: `tests/e2e/` keeps a dozen modules that deliberately do not check,
//! each pinning a check-time diagnostic, and a package holding one of those
//! does not check as a whole.
//!
//! Lowering is sliced by reachability: `cove_ir::lower_entry` lowers what
//! the entry can reach and nothing else, so what is measured here is the
//! program that entry is. This is the same call `cove run` makes, with the
//! same entry and the same `cove_sema::HostSchemas`, so what this harness
//! measures and what the CLI runs are one program rather than two that could
//! drift. It verifies what it emitted before it answers, so there is no
//! separate validation step for this file to make.
//!
//! # What is compared, and what is not
//!
//! The value the entry answered or the structured error it failed with, every
//! line written to either of the fake console's streams in order, how the run
//! ended, the fake filesystem as the run left it, and the trace the run
//! wrote. Fuel is not compared: it is each evaluator's own work counter,
//! charged at safepoints each puts where its own execution model has one, and
//! an instruction is not an AST node, so there is no honest mapping between
//! the two figures.
//!
//! An error's source position is compared exactly, and this is the claim in
//! this file that most had to be re-established rather than inherited.
//! `cove_runtime::vm::differential` compares messages and not spans, saying
//! the two evaluators "legitimately differ in the span they attach to a fault
//! today". Over this corpus they do not: twelve cases fail with a span and
//! all twelve name the same file and the same two byte offsets on both sides.
//! The weaker property is the one to assert when it is the true one, and here
//! it is not.
//!
//! The hosts are the deterministic fakes `examples.rs` and `cove-bench`
//! already run against — a console that is a buffer, a virtual clock that
//! moves only when something moves it, an in-memory filesystem seeded from
//! the package's own `files/`, recorded documents, http, and rows — so
//! nothing here reaches the network or a real clock, and every answer is the
//! same on every machine.
//!
//! Budgets come from `[run.<name>]` except fuel and the deadline, which are
//! left off on purpose: fuel is per-evaluator for the reason above, and a
//! deadline is wall-clock, so bounding either would make the two disagree by
//! construction rather than by fault. No case in the corpus sets one today.
//!
//! # The trace, and what a normalization is allowed to drop
//!
//! ADR 0034 asks for trace events by name, so every case is run with a
//! `cove_runtime::trace::JsonlSink` on both sides and the two recordings are
//! compared. The recording rather than the events: the JSONL is what `cove
//! trace` reads and what `cove replay` consumes, so comparing the lines
//! compares the artifact somebody else's program is handed, and a field that
//! stops being written is a change to that artifact whether or not the event
//! behind it still exists.
//!
//! A field dropped because it differed is exactly how a real divergence
//! hides, so nothing below is dropped for differing. Each exclusion is a
//! property of an evaluator rather than of the program, each was established
//! by running this corpus rather than by assuming, and [`Trace::of`] is where
//! each one is made and argued. Every one of them was re-established against
//! the linear-memory backend when the predecessor was deleted; none was
//! inherited, and one — what `heap_summary` is worth comparing — did not
//! survive the re-measurement and is now argued the other way.
//!
//! What survives the normalization is compared exactly and agrees over the
//! whole corpus: `trace_header`'s version, capture mode, entry and arguments;
//! `entry_enter` and `entry_exit`'s module and function; every `host_call`'s
//! task, module, operation, capability, grant, arguments and outcome;
//! `task_spawned`'s id, parent and scope; the `task_completed` of every task
//! neither run cancelled; `heap_summary`'s presence, its position in the
//! sequence and the keys it carries; and `run_ended`'s outcome and message.
//! The task ids themselves agree because both draw them from the one counter
//! `cove_runtime::runtime::Runtime` holds, so there was no renumbering to
//! normalize.
//!
//! No trace event carries a fuel figure. The per-evaluator counter reaches
//! `cove run --stats` and never the trace, so there was nothing here to
//! exclude for it.
//!
//! # Reading the coverage summary
//!
//! ```console
//! $ cargo test -p cove-cli --test differential -- --nocapture
//! ```
//!
//! The summary is printed on every run and repeated in the message of either
//! assertion that fails, so a failing run carries it without being asked.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use cove_runtime::budget::{Budget, Cancellation, Limits};
use cove_runtime::clock::{Clock, VirtualTime};
use cove_runtime::database::Database;
use cove_runtime::error::RuntimeError;
use cove_runtime::files::{Files, Tree};
use cove_runtime::host::{Console, Documents, Env, Grants, HostRegistry};
use cove_runtime::http::Http;
use cove_runtime::interp::Interpreter;
use cove_runtime::process::{Process, ProcessLog};
use cove_runtime::runtime::{Runtime, ENTRY_TASK};
use cove_runtime::trace::{
    JsonlSink, RecordingBackend, RunOutcome, TraceHeader, TraceSink, ValueCapture,
};
use cove_runtime::value::Value;
use cove_runtime::Vm;
use cove_sema::config::RunConfig;
use cove_sema::HostSchemas;

#[path = "support/mod.rs"]
mod support;
use support::{Case, ModuleIndex, Prepared};

// ------------------------------------------------------------------ the test

/// Every case in the corpus, on both evaluators.
///
/// One `#[test]` rather than one per case: the corpus is discovered rather
/// than declared, so there is nothing to hang a test attribute on, and a
/// single run is what makes the coverage summary a summary.
#[test]
fn the_backend_and_the_oracle_agree_over_the_whole_corpus() {
    // Everything happens on the stack the runtime sizes: the interpreter is
    // a recursive tree walker, a test thread's stack is not one it chose, and
    // every `Value` either evaluator builds belongs to the thread that built
    // it. The lowered program could cross — a `cove_ir::Program` is shared
    // by every thread of a run, which is what lets a spawned task run one —
    // but it has no reason to, since what it is for is on the far side. Only
    // the report comes back out.
    let report = cove_runtime::on_cove_stack(run_the_corpus).expect("a thread to run Cove on");
    let summary = report.summary();
    print!("{summary}");

    assert!(
        report.disagreements.is_empty(),
        "{} case(s) answered differently on the two evaluators:\n\n{}\n{summary}",
        report.disagreements.len(),
        report.disagreements.join("\n")
    );
    assert!(
        report.not_lowered.is_empty(),
        "{} case(s) checked and then did not lower, and the floor for that is \
         zero:\n\n{}\n{summary}",
        report.not_lowered.len(),
        report
            .not_lowered
            .iter()
            .map(|(case, why)| format!("{case}: {why}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// Discovers the corpus, and runs every case of it.
fn run_the_corpus() -> Report {
    let mut report = Report::default();
    let cases = discover();
    assert!(!cases.is_empty(), "the corpus is empty");
    report.cases = cases.len();

    // One index per package rather than one per case: `tests/e2e` holds
    // seventy cases and a hundred modules, and what each module reaches is a
    // fact about the package that does not change between two of them.
    let mut indexes: BTreeMap<PathBuf, ModuleIndex> = BTreeMap::new();

    for case in cases {
        let index = indexes
            .entry(case.root.clone())
            .or_insert_with(|| ModuleIndex::of(&case.root));

        // A package that does not check has no program in it to lower or to
        // run, and neither does a case whose entry names no module or
        // function this package declares. `tests/e2e` keeps the first kind
        // on purpose — each pins a check-time diagnostic — so both are
        // counted apart rather than reported as anything this backend did
        // or did not cover.
        let Ok(prepared) = Prepared::of(&case, index) else {
            report.unchecked.push(case.name.clone());
            continue;
        };

        let (module, entry) = prepared.entry();
        // The same call `cove run` makes, with the same entry: what is
        // lowered is what this entry reaches, so the harness and the CLI mean
        // one thing by "the program this entry is". There is no separate
        // validation step to make — the lowering verifies what it emitted
        // before it answers — and no admission predicate to consult, so a
        // program that checks and does not lower is a bug reported as
        // diagnostics rather than a refusal to be counted.
        let program = match cove_ir::lower_entry(
            &prepared.checked,
            &prepared.sources,
            &HostSchemas::new(),
            module,
            entry,
        ) {
            Ok(program) => program,
            Err(items) => {
                report.not_lowered.push((
                    case.name.clone(),
                    items
                        .iter()
                        .map(|item| format!("[{}] {}", item.code, item.message))
                        .collect::<Vec<_>>()
                        .join("; "),
                ));
                continue;
            }
        };
        report.lowered.push(case.name.clone());

        let oracle = run_on_ast(&case, &prepared, module, entry);
        let backend = run_on_vm(&case, &prepared, &program, module, entry);
        if !oracle.trace.cancelled.is_empty() || !backend.trace.cancelled.is_empty() {
            report.races.push(case.name.clone());
        }
        if oracle != backend {
            report
                .disagreements
                .push(disagreement(&case.name, "vm", &oracle, &backend));
        }
    }
    report
}

// -------------------------------------------------------------- the corpora
//
// `Case`, `ModuleIndex` and `Prepared` — discovering a `[run.<name>]` case
// and parsing and checking the modules its entry reaches — are
// `tests/support/mod.rs`'s, shared with `vm_coverage.rs` so that there is
// one description of how a corpus case is loaded rather than two that could
// drift.

/// Every case of every corpus, in a fixed order.
///
/// The corpora are `tests/e2e/` and `examples/`, and a case is a
/// `[run.<name>]` table of any `cove.toml` inside them — including the ones
/// an own-package `tests/e2e` case brings, which are packages of their own
/// exactly as `tests/e2e.rs` treats them.
///
/// # Why `benches/` is not one of them
///
/// It was, and it was 78 of the 340 seconds this test then spent running
/// programs — a figure taken against the predecessor, on a corpus that is not
/// this one, and quoted here as the reason a decision was made rather than as
/// a measurement of what runs today. What has not changed is why: a benchmark
/// is sized to be measurable in an optimized build — `benches/arith` turns a
/// loop two million times — and this test runs unoptimized, twice per case.
/// Nothing about agreement needs two million turns to establish; the first
/// one settles it and the rest are the same instruction again. Left out, the
/// whole corpus is under thirty seconds.
///
/// The coverage did not go anywhere. `cove-bench` runs every benchmark on
/// `ast` and on `vm` and each row asserts its own answer, so an evaluator
/// that disagreed would fail the assertion on the side that was wrong — and
/// it runs them optimized, on every push. What is given up is the console
/// comparison this harness makes and that one does not, and a benchmark
/// writes almost nothing to the console.
///
/// `vm_coverage.rs` makes the opposite choice, and it costs it: those two
/// million turns are eighty seconds of its run. It pays them because what it
/// measures is how much of the corpus this backend runs at all, and a
/// benchmark it has never executed end to end is where a fault in the
/// dispatch loop shows first. This file is not asking that question — it is
/// asking whether two evaluators say the same thing, and the answer to that
/// does not get truer on the millionth turn.
fn discover() -> Vec<Case> {
    let root = support::repo_root();
    let mut roots = vec![root.join("tests/e2e")];
    roots.extend(support::nested_packages(&root.join("tests/e2e")));
    roots.push(root.join("examples"));

    roots
        .iter()
        .flat_map(|package| support::cases_of(&root, package))
        .collect()
}

// ------------------------------------------------------------ the two runs

/// What one backend made of one case: everything the run can be observed by.
///
/// `Eq` is not derived beside `PartialEq` because [`Trace`] equality is not
/// transitive: whether two traces may be compared over a given task depends
/// on what both of them did with that task, which is a pairwise question.
#[derive(PartialEq)]
struct Ran {
    /// The value the entry answered, rendered, or the structured error it
    /// failed with. Rendered rather than carried because a [`Value`] is
    /// `Rc`-based and belongs to the run that made it.
    answer: String,
    /// Every line written to the fake console's output stream, in the order
    /// they were written.
    console: Vec<String>,
    /// Every line written to the fake console's diagnostic stream, in the
    /// order they were written.
    ///
    /// Kept apart from `console` rather than merged into it, because a
    /// program that wrote a line to the other stream on one backend would
    /// otherwise agree with itself: two streams compared as one are one
    /// stream again the moment it matters.
    diagnostics: Vec<String>,
    /// How the run ended, classified exactly as `run_entry` classifies it for
    /// the run's terminal trace event.
    outcome: RunOutcome,
    /// The fake filesystem as the run left it. A program told to write a file
    /// says on the console that it did, and the console line is not the file.
    files: BTreeMap<String, String>,
    /// The trace the run wrote, normalized. What a program did at the Host
    /// API boundary and what its tasks did is not visible in anything above:
    /// a run that made a call it should not have made still answers the same
    /// value and prints the same line.
    trace: Trace,
}

/// Runs the case on the interpreter, which is the oracle.
fn run_on_ast(case: &Case, prepared: &Prepared, module: &str, entry: &str) -> Ran {
    let (fakes, hosts) = Fakes::build(case, module, entry, RecordingBackend::Ast);
    // The trace reaches the run through two doors and both must be the same
    // sink: `HostRegistry` records the host calls and `Runtime` records
    // everything else, exactly as `cove run --trace` wires them.
    let sink = Arc::clone(&fakes.sink);
    let runtime = Runtime::new(
        prepared.checked.clone(),
        prepared.sources.clone(),
        Arc::new(hosts),
    )
    .with_trace(sink);
    let answer = Interpreter::new(&runtime).run_entry(module, entry, arguments(case));
    fakes.observed(answer)
}

/// Runs the same case on the linear-memory backend, over the program it was
/// lowered to.
///
/// Through [`Vm`] rather than through anything below it, because what is
/// being compared is what the language says and the language's answer
/// includes the boundary: the same entry-shape check, the same
/// materialisation of the answer, the same terminal trace events. Comparing
/// the dispatch loop against the whole of the oracle would be comparing two
/// different things.
fn run_on_vm(
    case: &Case,
    prepared: &Prepared,
    program: &cove_ir::Program,
    module: &str,
    entry: &str,
) -> Ran {
    let (fakes, hosts) = Fakes::build(case, module, entry, RecordingBackend::Vm);
    let sink = Arc::clone(&fakes.sink);
    let hosts = Arc::new(hosts);
    let runtime = Runtime::new(
        prepared.checked.clone(),
        prepared.sources.clone(),
        hosts.clone(),
    )
    .with_trace(sink);
    let answer = Vm::new(&runtime, &hosts, program).run_entry(module, entry, arguments(case));
    fakes.observed(answer)
}

/// The process arguments the entry is handed, as either evaluator takes
/// them.
fn arguments(case: &Case) -> Vec<Rc<str>> {
    case.args.iter().map(|arg| arg.as_str().into()).collect()
}

/// What a run can be observed through, kept where the test can read it back
/// once the run is over.
struct Fakes {
    console: Buffer,
    diagnostics: Buffer,
    files: Tree,
    /// The trace, as the JSONL a `--trace` file would hold.
    trace: Buffer,
    /// The sink writing into `trace`, which the run's `Runtime` needs as well
    /// as its `HostRegistry`.
    sink: Arc<dyn TraceSink>,
}

impl Fakes {
    /// The hosts one run is given, and the handles onto the ones that record
    /// what it did: both of the console's streams and the filesystem.
    ///
    /// Every host is registered whether or not this case reaches it, exactly
    /// as `cove run` registers them: the grants are what decide, so a
    /// capability a program reaches for without holding is refused with the
    /// reason rather than with a missing module.
    fn build(
        case: &Case,
        module: &str,
        entry: &str,
        backend: RecordingBackend,
    ) -> (Fakes, HostRegistry) {
        let console = Buffer::default();
        let diagnostics = Buffer::default();
        let files = Files::in_memory(seeded_files(&case.root));
        let tree = files.tree();

        let mut hosts = HostRegistry::new(Grants::new(case.run.allow.clone()));
        // Two buffers, because the host has two streams: one buffer would
        // make a line that moved from the one to the other invisible here,
        // which is the only kind of disagreement the second stream adds.
        hosts.register(Box::new(Console::new(console.clone(), diagnostics.clone())));
        hosts.register(Box::new(Env::new(BTreeMap::new())));
        hosts.register(Box::new(Documents::in_memory(seeded_documents(&case.root))));
        hosts.register(Box::new(Clock::virtual_clock(VirtualTime::new())));
        hosts.register(Box::new(Database::recorded(BTreeMap::new())));
        hosts.register(Box::new(Http::recorded(BTreeMap::new(), Vec::new())));
        hosts.register(Box::new(Process::recorded(
            case.args.clone(),
            BTreeMap::new(),
            ProcessLog::new(),
        )));
        hosts.register(Box::new(files));
        // Full capture, because a redacted trace records a value's type and
        // not the value, and the arguments a program passes a host are the
        // half of a host call this test most wants compared. Nothing here
        // reaches a real host, so there is no secret to redact.
        let trace = Buffer::default();
        let sink: Arc<dyn TraceSink> = Arc::new(JsonlSink::new(
            trace.clone(),
            TraceHeader {
                // Each side records itself, exactly as `cove run --trace`
                // does, so the recording each evaluator produces is the
                // recording it would hand a person. See [`Trace::of`] for why
                // this is then the one field the comparison removes outright
                // rather than standing a placeholder over.
                backend,
                values: ValueCapture::Full,
                entry: format!("{module}.{entry}"),
                args: case.args.clone(),
            },
        ));
        hosts.set_trace(Arc::clone(&sink));
        hosts.set_budget(Budget::with_cancellation(
            limits(&case.run),
            Cancellation::new(),
        ));

        (
            Fakes {
                console,
                diagnostics,
                files: tree,
                trace,
                sink,
            },
            hosts,
        )
    }

    /// What the run left behind, beside what it answered.
    fn observed(self, answer: Result<Value, RuntimeError>) -> Ran {
        let outcome = match &answer {
            Ok(value) if value.is_err() => RunOutcome::Error,
            Ok(_) => RunOutcome::Success,
            Err(error) => error.outcome,
        };
        Ran {
            answer: describe(&answer),
            console: self.console.lines(),
            diagnostics: self.diagnostics.lines(),
            outcome,
            files: self.files.files(),
            trace: Trace::of(&self.trace.lines()),
        }
    }
}

/// The budgets a case runs under.
///
/// Everything `[run.<name>]` sets except fuel and the deadline. Fuel is each
/// evaluator's own counter — an instruction is not an AST node, and the two
/// charge at safepoints they put where their own execution models have one —
/// and a deadline is wall-clock, so either one would make the two stop at
/// different points by construction rather than by fault. What is left counts
/// things both count the same way: host calls, and tasks.
fn limits(run: &RunConfig) -> Limits {
    Limits {
        fuel: None,
        deadline: None,
        max_host_calls: run.max_host_calls,
        max_call_depth: None,
        max_tasks: run.max_tasks,
    }
}

/// One run's answer, rendered so that two of them can be compared and either
/// of them read.
///
/// A failure is rendered by its structure rather than by its message alone:
/// what it said, how it classified itself, which capability the boundary
/// refused, the rule it cited, and where in the source it points. ADR 0034's
/// sixth condition names source spans, and the strongest form of that claim
/// is the one the corpus supports: over the twelve cases that fail with a
/// span, the two evaluators point at the same file and the same two byte
/// offsets.
fn describe(answer: &Result<Value, RuntimeError>) -> String {
    match answer {
        Ok(value) => format!("value {value:?}"),
        Err(error) => format!(
            "failed {:?}: {}\n    rule: {:?}\n    help: {:?}\n    denied: {:?}\n    at: {:?}",
            error.outcome,
            error.message,
            error.rule,
            error.help,
            error.denied_capability,
            error.span,
        ),
    }
}

/// The message a disagreement is reported with: both sides, in full.
///
/// ADR 0012 presumes the oracle right, so the interpreter's answer is named
/// first and named as the oracle. Which side is wrong is still a judgement,
/// and the message is what somebody makes it from.
fn disagreement(name: &str, which_backend: &str, oracle: &Ran, backend: &Ran) -> String {
    let mut out = format!("{name}: the two evaluators did not agree\n");
    let mut side = |which: &str, ran: &Ran| {
        let _ = write!(
            out,
            "  {which}:\n    outcome: {:?}\n    {}\n",
            ran.outcome, ran.answer
        );
        let _ = writeln!(out, "    console: {:?}", ran.console);
        if !ran.diagnostics.is_empty() {
            let _ = writeln!(out, "    diagnostics: {:?}", ran.diagnostics);
        }
        if !ran.files.is_empty() {
            let _ = writeln!(out, "    files: {:?}", ran.files);
        }
        for (task, events) in &ran.trace.tasks {
            let _ = writeln!(out, "    trace of task {task}:");
            for line in events {
                let _ = writeln!(out, "      {line}");
            }
        }
    };
    side("ast (the oracle)", oracle);
    side(which_backend, backend);
    out
}

// -------------------------------------------------------------- the trace

/// One run's trace, normalized so that two evaluators' recordings of one
/// program can be compared.
///
/// Held per task rather than as the one interleaved file the run wrote. Every
/// event is produced by whichever task made it and written by the one sink
/// the run shares, so the order two tasks' events reach that sink is the
/// order two threads happened to get there — ADR 0008 gives every spawned
/// task a thread of its own, and nothing in the language fixes which of them
/// writes first. Grouping by task drops exactly that and keeps everything
/// else: within one task the order is the program's, and it is compared.
///
/// This is not a normalization that could be argued either way. Running the
/// interpreter against itself thirty times over, `tests/e2e:gc_tasks`,
/// `tests/e2e:tasks_shared` and `examples:tasks` each wrote a differently
/// interleaved file every time, and every one of them writes the same file
/// once it is read per task. A comparison that failed on the interleaving
/// would be reporting the scheduler, and it would fail the oracle against
/// itself as readily as it would fail one evaluator against the other.
struct Trace {
    /// Each task's own events, in the order that task produced them, keyed by
    /// the task's id. The entry is `cove_runtime::runtime::ENTRY_TASK`, which
    /// is the convention every event that names a task already uses.
    tasks: BTreeMap<u64, Vec<String>>,
    /// The tasks this run cancelled. See [`Trace::eq`] for what that costs.
    cancelled: BTreeSet<u64>,
}

impl Trace {
    /// Reads the JSONL a run wrote, and normalizes it.
    ///
    /// # Every `Duration` is blanked
    ///
    /// `cpu`, `wait` and `pause` are wall time. Two runs of one program on
    /// one evaluator do not agree on any of them either, so comparing them
    /// would report the machine. The keys are kept and only the figures go,
    /// so a `_ns` field that stopped being written is still a difference.
    ///
    /// # `heap_collected` is dropped whole
    ///
    /// The event says when a collection happened and what it found, and only
    /// one of the two evaluators writes it: `cove_runtime::interp` records
    /// one per sweep of its `Rc`-ed object heap, and `cove_runtime::vm`
    /// records none, ever. Its four figures are objects allocated, objects
    /// freed, objects live and bytes live, which is a vocabulary the
    /// linear-memory heap does not have — its heap is a run of eight-byte
    /// words and an inline struct is words in it and no object at all.
    ///
    /// That is a categorical property of the two collectors rather than a
    /// figure that came out differently, and the corpus shows how far apart
    /// they are: `tests/e2e:gc_churn` writes eight of these events on the
    /// interpreter and none on the linear-memory backend, and
    /// `tests/e2e:gc_tasks` writes thirty-two and none. Comparing the event
    /// at all would report one collector's schedule against the other's
    /// silence.
    ///
    /// The predecessor dropped this event too, and the argument it dropped it
    /// under was a narrower one — both evaluators wrote the event and put
    /// their safepoints in different places, so the same collection landed at
    /// a different point in the sequence. That argument was about a backend
    /// that no longer exists. The event is still dropped, and the reason it
    /// is dropped is now larger rather than the same.
    ///
    /// # `heap_summary`'s every figure is blanked, and every key is kept
    ///
    /// This is the one exclusion in this file that the predecessor's version
    /// of it would not recognize. That one compared `allocated`,
    /// `allocated_bytes` and `collections` exactly and dropped `live_bytes`
    /// and `peak_bytes` with an argument for each. None of it survives, and
    /// what killed it is [issue #240](https://github.com/myuon/cove/issues/240):
    /// the event was widened because the two evaluators do not have the same
    /// kind of heap and neither family of figures can be derived from the
    /// other. The interpreter counts objects and the bytes they asked for and
    /// leaves the word half `null`; the linear-memory backend counts words
    /// and leaves the object half and `pause_ns` `null`. There is no field on
    /// the line both of them fill with a measurement of the same thing.
    ///
    /// `collections` is the near miss and the reason to say this plainly. It
    /// is the one figure neither side may leave out — a collection is a
    /// collection whatever the heap holds — so it looks comparable. It is
    /// not: a collection runs when a heap crosses a threshold measured in
    /// that heap's own unit, and the two heaps are different sizes in
    /// different units. Measured over this corpus, the linear-memory backend
    /// reports `"collections":0` for every one of the ninety-seven cases,
    /// because `cove_runtime::vm`'s `DEFAULT_HEAP_WORDS` is four mebiwords
    /// and nothing in the corpus fills it, while the interpreter reports
    /// between one and thirty-two. Thirty of the ninety-seven differ and the
    /// other sixty-seven agree only in reporting nothing. Comparing it would
    /// be asserting a fact about a heap size neither the language nor the
    /// program chose.
    ///
    /// So no figure on this line is compared, and that is written down here
    /// rather than left to be inferred from a passing test: what is compared
    /// is that the event was written, where in the task's sequence it stands,
    /// and which keys it carries. Those are real. The event is the run's
    /// second-to-last on both sides, after `entry_exit` and before
    /// `run_ended`, and a backend that stopped writing one or started writing
    /// a different set of fields fails here. Two answers to two different
    /// questions are not a comparison, and pretending otherwise would be the
    /// dishonest half of this whole file.
    ///
    /// The predecessor's figures are worth keeping as figures rather than
    /// silently reattributed. `tests/e2e:gc_churn` peaked at 120 bytes on the
    /// interpreter and 216 on the predecessor, and that pair was the argument
    /// for dropping `peak_bytes`. On the interpreter it still peaks at 120;
    /// the linear-memory backend writes `"peak_bytes":null` and the run it
    /// describes is `"allocated_words":4534,"capacity_words":4534`. The old
    /// number is not this backend's and no reading of it makes it so.
    ///
    /// # `trace_header`'s `backend` is dropped, and it is the only field of
    /// that event that is
    ///
    /// ADR 0026 put the recording backend in the header so that `cove replay`
    /// can tell a same-backend replay from a cross-backend one. It is the one
    /// field in the format that is *about* the evaluator rather than about
    /// the program: the two sides differ in it by construction, and would
    /// differ in it for a program with no instructions in it at all. Every
    /// other header field — the version, the capture mode, the entry, the
    /// entry's arguments — is compared exactly and agrees, so a header that
    /// stopped saying one of those is still a difference.
    ///
    /// This is the exception that proves the rule stated at the top of this
    /// file rather than a hole in it. Nothing here is dropped for having
    /// differed; this is dropped for being, by its own definition, the answer
    /// to "which evaluator is this", asked of a harness whose whole job is to
    /// run one program on both.
    fn of(lines: &[String]) -> Trace {
        let mut tasks: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        let mut cancelled = BTreeSet::new();
        for line in lines {
            if event(line) == "heap_collected" {
                continue;
            }
            // A `heap_summary` is blanked instead of having its durations
            // blanked, rather than as well: every figure on that line goes,
            // `pause_ns` among them, so running both over it would be one
            // placeholder written over another.
            let mut normalized = if event(line) == "heap_summary" {
                blank_measures(line)
            } else {
                blank_durations(line)
            };
            if event(line) == "trace_header" {
                normalized = without_string(&normalized, "backend");
            }
            if event(line) == "task_cancelled" {
                cancelled.insert(number(line, "id").unwrap_or(ENTRY_TASK));
            }
            tasks.entry(whose(line)).or_default().push(normalized);
        }
        Trace { tasks, cancelled }
    }
}

/// Two traces of one program, compared task by task.
///
/// # A task either run cancelled is compared only by its `task_spawned`
///
/// Cancellation is asynchronous: a scope that ends asks its children to stop
/// and lands wherever each thread happened to be, so how far a cancelled task
/// got is the scheduler's answer and not the program's. This is measured
/// rather than assumed, and it was measured again against the linear-memory
/// backend rather than carried over.
///
/// Take the rule out and the corpus holds exactly two cases that move.
/// `tests/e2e:fail_max_tasks` disagrees in twenty runs of twenty and
/// `examples:callbacks` in fourteen of the same twenty — which on its own
/// would read as a systematic divergence rather than a race, since the
/// interpreter cancels both of `fail_max_tasks`'s children every time and the
/// linear-memory backend completes both of them every time. Then run the
/// *oracle against itself*, with both sides of this harness on the
/// interpreter and nothing else changed, and the same two cases disagree with
/// themselves: `fail_max_tasks` in two runs of thirteen, `callbacks` in five,
/// and six of the thirteen clean through. The fact is not one either
/// evaluator holds itself to, so holding one to the other's version of it
/// would be a test that fails at random. That the two land on opposite sides
/// of the race almost every time is a difference in scheduling luck, not in
/// what the program means.
///
/// So what is compared for such a task is that it was spawned, with the same
/// id, by the same parent, into the same scope. What is given up with the
/// rest is real and is worth naming: a backend that always cancelled where
/// the interpreter always completed would not be caught here, and the
/// paragraph above is a reminder that this corpus is close to that shape.
/// What catches it instead is that the entry's own trace is compared exactly,
/// and a task's work reaches the entry — through what it printed, what it
/// left in the filesystem, and what the entry answered, all of which this
/// harness compares whether or not a trace was written. [`Report::races`]
/// names every case this rule applied to, so the loss is printed rather than
/// silent.
impl PartialEq for Trace {
    fn eq(&self, other: &Trace) -> bool {
        let ids: BTreeSet<u64> = self
            .tasks
            .keys()
            .chain(other.tasks.keys())
            .copied()
            .collect();
        let raced: BTreeSet<u64> = self.cancelled.union(&other.cancelled).copied().collect();
        ids.into_iter().all(|id| {
            let mine = self.tasks.get(&id).map(Vec::as_slice).unwrap_or_default();
            let theirs = other.tasks.get(&id).map(Vec::as_slice).unwrap_or_default();
            if raced.contains(&id) {
                spawn_of(mine) == spawn_of(theirs)
            } else {
                mine == theirs
            }
        })
    }
}

/// The `task_spawned` line of one task's events, which is all that is
/// compared for a task a run cancelled.
fn spawn_of(events: &[String]) -> Option<&String> {
    events.iter().find(|line| event(line) == "task_spawned")
}

/// Which task produced an event.
///
/// A `task` field answers directly. A task's own lifecycle events are its
/// own, including the `task_spawned` the parent recorded and the
/// `task_cancelled` the joining scope did: what they say is about the task
/// they name, and keeping them with it is what lets one task's whole life be
/// compared as one sequence. Everything else — the header, the entry's two
/// events, the summary, the ending — belongs to the entry, which is the
/// convention the trace format already uses for the entry's own host calls.
fn whose(line: &str) -> u64 {
    if let Some(task) = number(line, "task") {
        return task;
    }
    match event(line) {
        "task_spawned" | "task_completed" | "task_cancelled" => {
            number(line, "id").unwrap_or(ENTRY_TASK)
        }
        _ => ENTRY_TASK,
    }
}

/// The `event` name of one trace line.
fn event(line: &str) -> &str {
    let Some(at) = line.find("\"event\":\"") else {
        return "";
    };
    let rest = &line[at + "\"event\":\"".len()..];
    &rest[..rest.find('"').unwrap_or(rest.len())]
}

/// The integer under a top-level `key` of one trace line.
fn number(line: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let at = line.find(&needle)? + needle.len();
    let rest = &line[at..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The line without `key` and the quoted string under it.
///
/// One field is removed outright rather than blanked, and it is
/// `trace_header`'s `backend`: see [`Trace::of`]. Everything else this file
/// declines to compare keeps its key and loses its figure, because a key that
/// stopped being written is a change to the artifact and should still fail.
fn without_string(line: &str, key: &str) -> String {
    let needle = format!("\"{key}\":\"");
    let Some(at) = line.find(&needle) else {
        return line.to_string();
    };
    let rest = &line[at + needle.len()..];
    let Some(end) = rest.find('"') else {
        return line.to_string();
    };
    let (mut head, mut tail) = (&line[..at], &rest[end + 1..]);
    // One of the two commas around the field goes with it, whichever side it
    // is on, so that what is left is still a JSON object.
    if let Some(rest) = tail.strip_prefix(',') {
        tail = rest;
    } else {
        head = head.strip_suffix(',').unwrap_or(head);
    }
    format!("{head}{tail}")
}

/// The `heap_summary` line with every figure replaced by a placeholder, and
/// every key left where it was.
///
/// The same treatment [`blank_durations`] gives a `_ns` field and for the same
/// reason: what is being kept is the shape of the line rather than the numbers
/// on it, so a field that stopped being written is still a difference while a
/// field the two heaps count differently is not. [`Trace::of`] is where the
/// argument for why every one of these figures is the machine's rather than
/// the program's is made.
///
/// A figure here is an integer or `null`, since
/// [issue #240](https://github.com/myuon/cove/issues/240) made every one of
/// them optional so that a machine can say it did not count something, and
/// both read the same way once the value is only being stood over.
fn blank_measures(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    // Every key on this line but `event` carries a figure, and `event` carries
    // a string, so the split is on `":` — a quoted key followed by a value
    // that is not a string.
    while let Some(at) = rest.find("\":") {
        let (head, tail) = rest.split_at(at + "\":".len());
        if tail.starts_with('"') {
            out.push_str(head);
            rest = tail;
            // Past the opening quote, so the string's own contents cannot be
            // mistaken for another key.
            let end = rest[1..].find('"').map(|i| i + 2).unwrap_or(rest.len());
            out.push_str(&rest[..end]);
            rest = &rest[end..];
            continue;
        }
        out.push_str(head);
        out.push_str("<heap>");
        let end = tail
            .find(|c: char| !c.is_ascii_digit() && !c.is_ascii_alphabetic())
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// The line with the figure under every `_ns` key replaced by a placeholder.
fn blank_durations(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find("_ns\":") {
        let (head, tail) = rest.split_at(at + "_ns\":".len());
        out.push_str(head);
        out.push_str("<wall clock>");
        let end = tail
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

// -------------------------------------------------------------- the fakes

/// One of a `console`'s streams, which a run writes to and this test reads
/// back.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Buffer {
    fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().expect("no run panics while printing"))
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("no run panics while printing")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The package's own `files/` directory, read into the in-memory filesystem
/// the run is given.
///
/// Reads answer what the case's fixtures actually hold, so a case that reads
/// a file is compared having read it; writes land in memory, so a run cannot
/// change the repository it was read out of.
fn seeded_files(root: &Path) -> BTreeMap<String, String> {
    let mut seeded = BTreeMap::new();
    read_tree(&root.join("files"), String::new(), &mut seeded);
    seeded
}

/// The package's own `documents/`, read the same way and for the same reason.
fn seeded_documents(root: &Path) -> BTreeMap<String, String> {
    let mut seeded = BTreeMap::new();
    read_tree(&root.join("documents"), String::new(), &mut seeded);
    seeded
}

/// Every readable file below `dir`, keyed by its `/`-separated path from it.
fn read_tree(dir: &Path, prefix: String, into: &mut BTreeMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let key = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            read_tree(&path, key, into);
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            into.insert(key, text);
        }
    }
}

// ------------------------------------------------------------- the summary

/// What the whole corpus came to.
#[derive(Default)]
struct Report {
    cases: usize,
    lowered: Vec<String>,
    /// Each case that checked and then did not lower, and the diagnostics the
    /// lowering answered with. The assertion over this is that it is empty:
    /// see the module docs for why zero is the only honest floor.
    not_lowered: Vec<(String, String)>,
    /// Cases whose package does not check, which have no program to run.
    unchecked: Vec<String>,
    /// Cases in which either run cancelled a task, so that task's own trace
    /// was compared only by the `task_spawned` that made it. Printed rather
    /// than kept quiet: this is the one place the trace comparison gives
    /// something up, and a reader of the summary should be able to see how
    /// much. [`Trace::eq`] is the argument for why.
    races: Vec<String>,
    disagreements: Vec<String>,
}

impl Report {
    /// The coverage summary: how much of the corpus reached a comparison, and
    /// what did not.
    ///
    /// A case that checked and then did not lower is printed with the
    /// diagnostics the lowering answered with, because there is no taxonomy
    /// left to group such a case under — the lowering has no admission
    /// predicate and no vocabulary of refusals, so the diagnostic is the whole
    /// of what can be said about it. The list is expected to stay empty; it is
    /// printed rather than only asserted so that a failing run carries the
    /// reason without being asked.
    fn summary(&self) -> String {
        let mut out = format!(
            "\ndifferential coverage over {} corpus case(s):\n  \
             {:>3} lowered, ran, and agree with the oracle\n  \
             {:>3} checked and did not lower\n  \
             {:>3} do not check, so there is nothing to run\n",
            self.cases,
            self.lowered.len(),
            self.not_lowered.len(),
            self.unchecked.len(),
        );

        if !self.not_lowered.is_empty() {
            out.push_str("\nwhat checked and then did not lower:\n");
            for (case, why) in &self.not_lowered {
                let _ = writeln!(out, "       {case}");
                let _ = writeln!(out, "         {why}");
            }
        }
        if !self.races.is_empty() {
            out.push_str(
                "\nwhere a cancelled task's own trace is a race, so only its spawn is compared:\n",
            );
            for case in &self.races {
                let _ = writeln!(out, "       {case}");
            }
        }
        if !self.lowered.is_empty() {
            out.push_str("\nwhat is compared, in full:\n");
            for case in &self.lowered {
                let _ = writeln!(out, "       {case}");
            }
        }
        out
    }
}
