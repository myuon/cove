//! Reproducing a run's Host API interactions: `cove replay`.
//!
//! ADR 0001: "Replay is deliberately limited to replaceable Host API
//! interactions. Cove does not require whole-language determinism to make
//! failures reproducible enough for testing and comparison." So a replay runs
//! the program's own computation for real and replaces only the boundary:
//! every host module is swapped for one that answers from the trace, in the
//! order the trace recorded, and calls nothing.
//!
//! The useful outcome of a replay is therefore not that it succeeded. It is a
//! divergence — the program asked for something the trace does not have, or
//! asked for it in a different order — because a divergence is the program
//! saying it would behave differently than it did. [`Divergence::report`] is
//! the part of this module that matters most.
//!
//! # Which backend a replay runs on
//!
//! Whichever `--backend <ast|lvm>` names, and when nobody names one, the
//! backend the trace says recorded it. ADR 0023 gave this command the flag
//! and ADR 0026 gave the file the answer; what follows is what the two mean
//! together here.
//!
//! Until ADR 0026 the default was whichever backend `cove run` ran a program
//! on, because an ordinary recording is that one's — but that was an
//! inference about how the file was probably produced rather than a fact read
//! out of it, and ADR 0023 said so in as many words. A trace header now
//! carries a
//! [`cove_runtime::RecordingBackend`], so the inference is gone: a replay
//! that names no backend runs on the backend that recorded, whichever one
//! that was, and **an ordinary replay is a same-backend replay because the
//! file said which backend that is** rather than because two defaults
//! happened to agree.
//!
//! The flag still wins over the file. Replaying an interpreter recording on
//! the lowered backend is what issue #140 called the interesting direction —
//! driven by a file rather than by a host, does this backend ask for the same
//! calls, in the same order, with the same arguments? — and a file that could
//! forbid it would take that question away. What the file buys instead is
//! that the crossing is now visible: [`provenance`] names both evaluators, in
//! the summary and at the end of every divergence report, and says whether
//! they are one or two.
//!
//! That distinction is the whole reason the field is worth a format version.
//! Nothing is known to diverge across the two — `tests/differential.rs`
//! compares the same tape, every host call's module, operation, arguments and
//! outcome, over the whole corpus — but "nothing is known to diverge" is a
//! weaker sentence than "these two ran the same way", and a replay that can
//! read the recording backend is finally able to say the second one.
//!
//! What the tape does when it runs out did not need deciding: it was decided
//! already, and by the tape. [`Divergence`] is a property of this module
//! rather than of an evaluator — `Unexpected` is "the program made a call
//! after the trace ran out of them", and `Tape::answer` produces it from what
//! the trace holds and what it was asked for, with no evaluator involved.
//! Both evaluators reach the tape through the one [`HostApi`] boundary they
//! share, so a lowered replay that runs off the end of the tape gets the
//! answer an interpreted replay already got.
//!
//! The rule that a run either finishes on the backend it named or fails
//! before any side effect applies here in the only form it can. A replay
//! makes no host call at all, so it has no side effect for a failure to come
//! before; what it has instead is a verdict, and the verdict is the whole
//! output. A replay that quietly finished on the interpreter would be
//! reporting "replayed", or a divergence, about a backend nobody asked for.
//! So the lowering happens before the tape is built, and a gap in it stops
//! the command with the gap pointed at in source.
//!
//! One order is not the program's to choose. ADR 0008 runs each spawned task
//! on a thread of its own, so the order in which two concurrent tasks reach
//! the host is the scheduler's, and a trace records the order one run
//! happened to take. Replaying a program whose tasks call hosts concurrently
//! can therefore diverge on order alone; that is the truth about the program
//! rather than a defect in the replay, and it is why a scope's contract is
//! the set of effects it produces and never their sequence.

use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use cove_runtime::host::{HostApi, HostRegistry};
use cove_runtime::interp::Interpreter;
use cove_runtime::runtime::Runtime;
use cove_runtime::schema::ModuleSchema;
use cove_runtime::Transfer;
use cove_runtime::{
    value_to_json, Budget, Cancellation, Grants, Limits, Lvm, RecordingBackend, ResourceHandle,
    RunOutcome, RuntimeError, Value, ValueCapture,
};

use crate::trace::{self, Outcome, Trace};
use crate::{Backend, CliError};

/// One recorded call a replay can answer.
struct Step {
    module: String,
    op: String,
    /// The call as source would write it, for a report.
    shown: String,
    /// The canonical encoding of each recorded argument, or `None` for one
    /// the trace does not carry.
    args: Vec<Option<String>>,
    answer: Answer,
}

/// What a replay hands back for one recorded call.
enum Answer {
    /// The value the host produced.
    ///
    /// Held as a `Transfer` rather than a `Value` because a host is shared
    /// across task threads, and a `Transfer` is precisely the form of a value
    /// that may cross a task boundary.
    Value(Transfer),
    /// The runtime error the host refused with, reproduced as itself.
    Error(String),
    /// Nothing, and why.
    None(String),
}

impl Step {
    /// Builds one step from a recorded call.
    fn of(call: &trace::HostCall) -> Step {
        let answer = match &call.outcome {
            Some(Outcome::Value(value)) => match &value.value {
                // The trace format encodes only values that may cross a task
                // boundary, so this conversion holds for every trace this
                // build writes. It is checked rather than assumed because the
                // trace is a file, and a file can say anything.
                Ok(value) => match Transfer::of(value) {
                    Ok(transfer) => Answer::Value(transfer),
                    Err(_) => Answer::None(
                        "the trace records a value that cannot cross a task boundary".to_string(),
                    ),
                },
                Err(missing) => Answer::None(missing.reason()),
            },
            Some(Outcome::Error(message)) => Answer::Error(message.clone()),
            Some(Outcome::NotRecordable) => Answer::None(format!(
                "the Host API schema declares `{}` not recordable, so the trace records no result: \
                 answering it with a value would keep running a program that had ended",
                call.qualified()
            )),
            None => Answer::None(
                "the trace records no result, because the call never reached a host".to_string(),
            ),
        };
        Step {
            module: call.module.clone(),
            op: call.op.clone(),
            shown: call.shown(),
            args: call.args.iter().map(|arg| arg.canonical()).collect(),
            answer,
        }
    }
}

/// The recorded calls, in order, and how far a replay has read into them.
struct Tape {
    steps: Vec<Step>,
    next: usize,
    /// How many arguments the trace did not carry, so could not be checked.
    unchecked_args: usize,
    divergence: Option<Divergence>,
}

impl Tape {
    fn new(steps: Vec<Step>) -> Tape {
        Tape {
            steps,
            next: 0,
            unchecked_args: 0,
            divergence: None,
        }
    }

    /// Answers one call from the trace, or records why it cannot.
    fn answer(&mut self, module: &str, op: &str, args: &[Value]) -> Result<Value, RuntimeError> {
        let asked = shown_call(module, op, args);
        let Some(step) = self.steps.get(self.next) else {
            return Err(self.diverge(Divergence::Unexpected {
                total: self.steps.len(),
                asked,
            }));
        };
        if step.module != module || step.op != op || !arguments_match(step, args) {
            let recorded = step.shown.clone();
            return Err(self.diverge(Divergence::Mismatch {
                at: self.next + 1,
                recorded,
                asked,
            }));
        }
        self.unchecked_args += step.args.iter().filter(|arg| arg.is_none()).count();
        self.next += 1;
        match &self.steps[self.next - 1].answer {
            Answer::Value(transfer) => Ok(transfer.clone().into_value()),
            Answer::Error(message) => Err(RuntimeError::new(message.clone()).with_rule(
                "`cove replay` reproduces what the host answered, including a refusal.",
            )),
            Answer::None(reason) => {
                let (at, recorded, reason) = (self.next, asked, reason.clone());
                Err(self.diverge(Divergence::Unanswerable {
                    at,
                    recorded,
                    reason,
                }))
            }
        }
    }

    /// Records `divergence` and turns it into the error that stops the run.
    ///
    /// Only the first divergence is kept: everything after it is a
    /// consequence of it, and a report that listed the consequences would
    /// bury the cause.
    fn diverge(&mut self, divergence: Divergence) -> RuntimeError {
        let error = RuntimeError::new(divergence.headline())
            .with_rule(
                "`cove replay` answers every Host API call from the trace, in the recorded order.",
            )
            .with_help("run `cove trace <file>` to see what the trace records");
        if self.divergence.is_none() {
            self.divergence = Some(divergence);
        }
        error
    }
}

/// Whether the arguments the program passed are the ones the trace recorded.
///
/// An argument the trace does not carry — redacted, or of a type the format
/// cannot record — cannot disagree, so it does not count as a difference. The
/// replay counts it instead, and says how many it could not check.
fn arguments_match(step: &Step, args: &[Value]) -> bool {
    if step.args.len() != args.len() {
        return false;
    }
    step.args
        .iter()
        .zip(args)
        .all(|(recorded, actual)| match recorded {
            Some(recorded) => recorded == &value_to_json(actual, ValueCapture::Full),
            None => true,
        })
}

/// `module.op(arg, ...)`, the way source writes the call.
fn shown_call(module: &str, op: &str, args: &[Value]) -> String {
    let args: Vec<String> = args.iter().map(trace::show_value).collect();
    format!("{module}.{op}({})", args.join(", "))
}

/// Why a replay could not reproduce the recorded run.
pub(crate) enum Divergence {
    /// The program made a call after the trace ran out of them.
    Unexpected { total: usize, asked: String },
    /// The program's next call is not the trace's next call.
    Mismatch {
        at: usize,
        recorded: String,
        asked: String,
    },
    /// The trace's next call is the right one and cannot be answered.
    Unanswerable {
        at: usize,
        recorded: String,
        reason: String,
    },
    /// The program stopped before using every recorded call.
    Unused {
        used: usize,
        total: usize,
        next: String,
    },
    /// The program made every recorded call and then ended differently than
    /// the run that was recorded did.
    Ended {
        recorded: RunOutcome,
        recorded_message: Option<String>,
        replayed: RunOutcome,
        replayed_message: Option<String>,
    },
}

impl Divergence {
    /// The one-line form, for the runtime error that stops the run.
    fn headline(&self) -> String {
        match self {
            Divergence::Unexpected { total, asked } => format!(
                "replay diverged: the program asked for `{asked}` after the trace's {total} recorded call(s) were all used"
            ),
            Divergence::Mismatch { at, .. } => {
                format!("replay diverged at recorded call {at}")
            }
            Divergence::Unanswerable { at, .. } => {
                format!("replay cannot answer recorded call {at}")
            }
            Divergence::Unused { used, total, .. } => format!(
                "replay diverged: the program made {used} of the trace's {total} recorded call(s)"
            ),
            Divergence::Ended {
                recorded, replayed, ..
            } => format!(
                "replay diverged: the recorded run ended `{}` and this one ended `{}`",
                recorded.as_str(),
                replayed.as_str()
            ),
        }
    }

    /// The full report, which is what a replay is for.
    pub(crate) fn report(&self) -> String {
        let mut out = String::new();
        match self {
            Divergence::Unexpected { total, asked } => {
                out.push_str(
                    "divergence: the program asked for a host call the trace does not have\n",
                );
                out.push_str(&format!(
                    "  the trace records  {total} call(s), all of them used\n"
                ));
                out.push_str(&format!("  the program asked  {asked}\n"));
            }
            Divergence::Mismatch {
                at,
                recorded,
                asked,
            } => {
                out.push_str("divergence: the program asked for a different host call\n");
                out.push_str(&format!("  at recorded call   {at}\n"));
                out.push_str(&format!("  the trace records  {recorded}\n"));
                out.push_str(&format!("  the program asked  {asked}\n"));
            }
            Divergence::Unanswerable {
                at,
                recorded,
                reason,
            } => {
                out.push_str("divergence: the trace has no result for a call the program made\n");
                out.push_str(&format!("  at recorded call   {at}\n"));
                out.push_str(&format!("  the program asked  {recorded}\n"));
                out.push_str(&format!("  why                {reason}\n"));
            }
            Divergence::Unused { used, total, next } => {
                out.push_str("divergence: the program stopped before the trace did\n");
                out.push_str(&format!("  the trace records  {total} call(s)\n"));
                out.push_str(&format!("  the program made   {used}\n"));
                out.push_str(&format!("  the next recorded  {next}\n"));
            }
            Divergence::Ended {
                recorded,
                recorded_message,
                replayed,
                replayed_message,
            } => {
                out.push_str("divergence: the program ended differently than it did\n");
                out.push_str(&format!(
                    "  the trace records  {}\n",
                    ended(*recorded, recorded_message.as_deref())
                ));
                out.push_str(&format!(
                    "  the program ended  {}\n",
                    ended(*replayed, replayed_message.as_deref())
                ));
            }
        }
        out.push_str(
            "  rule               a replay answers every Host API call from the trace, in the\n\
             \x20                    recorded order; the program's own computation runs for real,\n\
             \x20                    so a divergence means it took a different path than it did\n",
        );
        out
    }
}

/// The line a divergence report ends with: which backend read the tape,
/// which one wrote it, and therefore what a divergence can be blamed on.
///
/// Kept out of [`Divergence`] on purpose. A divergence is a fact about a
/// program and a file — the program asked for something the file has not got,
/// or asked in another order — and every one of them is produced by [`Tape`]
/// from those two things alone, with no evaluator involved. Giving the report
/// a backend would make a backend-independent judgement look like a
/// backend-specific one. What is backend-specific is the caveat, so the
/// caveat is what carries the backend.
///
/// Two sentences rather than one, because since ADR 0026 there are two cases
/// and they say opposite things. A same-backend replay is the strong one: the
/// two runs were the same run in every respect this command can name, so a
/// divergence is the program's. A cross-backend replay is the caveat ADR 0023
/// had to write for every replay, now written only for the replays it is true
/// of.
fn provenance(replayed: Backend, recorded: RecordingBackend) -> String {
    if replayed.recording() == recorded {
        format!(
            "  note               this replay ran on `{replayed}`, which is the backend that\n\
             \x20                    recorded the trace, so a divergence is the program's rather\n\
             \x20                    than the two backends'\n"
        )
    } else {
        format!(
            "  note               this replay ran on `{replayed}` and the trace was recorded on\n\
             \x20                    `{recorded}`, so a divergence here could be the two backends'\n\
             \x20                    rather than the program's\n"
        )
    }
}

/// The one thing a replay keeps from its own trace: how the run it just made
/// ended.
///
/// A replay writes no trace file, but the runtime records a run's terminal
/// event whether anyone is listening or not — so listening for that one event
/// is how the replayed ending is classified by exactly the rule that
/// classified the recorded one, rather than by a second copy of the rule kept
/// in step by hand.
#[derive(Default)]
struct EndingSink(Mutex<Option<(RunOutcome, Option<String>)>>);

impl cove_runtime::TraceSink for EndingSink {
    fn record(&self, event: cove_runtime::TraceEvent) {
        let cove_runtime::TraceEvent::RunEnded { outcome, message } = event else {
            return;
        };
        // A sink must not panic, and a poisoned lock is not a reason to fail
        // the replay it is only observing.
        if let Ok(mut ending) = self.0.lock() {
            *ending = Some((outcome, message));
        }
    }

    /// Nothing here reads a host call, so the boundary need not describe the
    /// values one carried.
    fn is_recording(&self) -> bool {
        false
    }
}

impl EndingSink {
    fn ending(&self) -> Option<(RunOutcome, Option<String>)> {
        self.0.lock().ok().and_then(|ending| ending.clone())
    }
}

/// How a run ended, for a divergence report: the classification, and what it
/// said if it said anything.
fn ended(outcome: RunOutcome, message: Option<&str>) -> String {
    match message {
        Some(message) => format!("{} — {message}", outcome.as_str()),
        None => outcome.as_str().to_string(),
    }
}

/// A host module that answers from a trace instead of from the world.
///
/// It declares itself out of the real module's own schema entry, so the
/// registry gates, checks the arguments and the arity, and counts
/// irreversible writes exactly as it does for a real run — a replay that
/// skipped those checks would be reproducing a different boundary than the
/// one that was recorded.
struct ReplayHost {
    declared: &'static ModuleSchema,
    tape: Arc<Mutex<Tape>>,
}

impl HostApi for ReplayHost {
    fn module_schema(&self) -> ModuleSchema {
        *self.declared
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        self.tape
            .lock()
            .expect("the replay tape is not shared across threads that panic")
            .answer(self.declared.name, op, &args)
    }

    /// Answers an operation on a handle from the trace, the handle included.
    ///
    /// A handle is a name, so a replay reproduces one by handing the recorded
    /// name back: the handle this receives came out of the trace, and
    /// matching it against the recorded first argument is what says the
    /// program reached for the same resource it reached for before.
    fn call_resource(
        &self,
        handle: &ResourceHandle,
        op: &str,
        args: Vec<Value>,
        _back: &mut dyn cove_runtime::Reentry,
    ) -> Result<Value, RuntimeError> {
        let op = format!("{}.{op}", handle.type_name);
        let mut asked = Vec::with_capacity(args.len() + 1);
        asked.push(Value::from_resource(handle.clone()));
        asked.extend(args);
        self.tape
            .lock()
            .expect("the replay tape is not shared across threads that panic")
            .answer(self.declared.name, &op, &asked)
    }
}

/// `cove replay <trace> <run-name> [--backend <ast|lvm>]`.
pub(crate) fn cmd_replay(args: &[String]) -> Result<(), CliError> {
    // `--backend` comes out first, by the one function `cove generate` reads
    // it with, so the flag means the same thing and refuses an unknown value
    // with the same sentence wherever it is written. An unknown value is
    // refused here, before the file is opened, because a typo is a typo
    // whatever the file turns out to say. What is *not* settled here is the
    // default: this is the one command with somewhere better than
    // `Backend::default_for_a_run` to look for one, and it has to read the
    // trace before it can look there. Everything still beginning with `--`
    // afterwards is a flag this command does not have, which is what it has
    // always said about one.
    let (chosen, positional) = crate::split_backend_if_named(args)?;
    if let Some(flag) = positional.iter().find(|arg| arg.starts_with("--")) {
        return Err(CliError::Message(format!(
            "unknown `cove replay` flag `{flag}`"
        )));
    }
    let [trace_path, name] = positional.as_slice() else {
        return Err(CliError::Message(
            "`cove replay` takes the path of a trace and the name of a `[run.<name>]` table".into(),
        ));
    };

    let trace_path = Path::new(trace_path.as_str());
    let trace = Trace::read(trace_path).map_err(CliError::Message)?;

    let (sources, package, program) = crate::load(None)?;
    let Some(run) = package.config.runs.get(name.as_str()) else {
        let known: Vec<&str> = package.config.runs.keys().map(String::as_str).collect();
        return Err(CliError::Message(format!(
            "cove.toml has no `[run.{name}]` table\n  known runs: {}",
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            }
        )));
    };
    let Some((module, entry)) = run.entry_parts() else {
        return Err(CliError::Message(format!(
            "`[run.{name}] entry` must be a qualified function such as `hello.main`, found `{}`",
            run.entry
        )));
    };
    if program.lookup_fn(module, entry).is_none() {
        return Err(CliError::Message(format!(
            "`[run.{name}] entry` refers to `{}`, which this package does not declare",
            run.entry
        )));
    }
    // A trace recorded from one entry cannot reproduce another: the recorded
    // calls are that entry's calls, in that entry's order.
    if trace.header.entry != run.entry {
        return Err(CliError::Message(format!(
            "`{}` was recorded from `{}`, but `[run.{name}] entry` is `{}`",
            trace_path.display(),
            trace.header.entry,
            run.entry
        )));
    }
    if trace.header.values == ValueCapture::Redacted {
        return Err(CliError::Message(format!(
            "`{}` was recorded with `--trace-values redacted`, so it carries no values to replay\n  \
             record it again without that flag to replay it",
            trace_path.display()
        )));
    }

    // The default a replay has that no other command has: the file. Since
    // ADR 0026 a header names the backend that wrote it, so a replay nobody
    // gave a flag to runs on that one, and `Backend::default_for_a_run` is
    // not consulted here at all — there is nothing left for it to infer. A
    // flag still wins, and the two may disagree on purpose.
    let recorded = trace.header.backend;
    let backend = chosen.unwrap_or_else(|| Backend::of_recording(recorded));

    let sources = Arc::new(sources);
    let program = Arc::new(program);

    // The lowering happens here, before the tape exists and before a host is
    // registered — the same place `crate::execute_entry` lowers, for the same
    // reason read across to a command that calls no host: a failure must be
    // the command's whole answer, not a footnote under a verdict about the
    // program. What arrives is a list of diagnostics rather than one refusal,
    // because the lowering reports every gap in what the entry reaches rather
    // than the first; a replay that cannot be run is still a replay that
    // reads no tape.
    let lowered = match backend {
        Backend::Ast => None,
        Backend::Lvm => {
            let ir = cove_lir::lower_entry(
                &program,
                &sources,
                &cove_sema::HostSchemas::new(),
                module,
                entry,
            )
            .map_err(|items| CliError::Diagnostics {
                items,
                sources: Arc::clone(&sources),
            })?;
            Some(Arc::new(ir))
        }
    };

    let steps: Vec<Step> = trace.dispatched_calls().into_iter().map(Step::of).collect();
    let recorded_calls = steps.len();
    let tape = Arc::new(Mutex::new(Tape::new(steps)));

    let mut hosts = HostRegistry::new(Grants::new(run.allow.clone()));
    for module in cove_runtime::shipped_schema() {
        hosts.register(Box::new(ReplayHost {
            declared: module,
            tape: Arc::clone(&tape),
        }));
    }
    // The run's own configured limits still apply: a replay is that run,
    // with its boundary answered from a file.
    hosts.set_budget(Budget::with_cancellation(
        Limits {
            fuel: run.fuel,
            deadline: run.deadline,
            max_host_calls: run.max_host_calls,
            max_call_depth: None,
            max_tasks: run.max_tasks,
        },
        Cancellation::new(),
    ));

    let program_args: Vec<Rc<str>> = trace
        .header
        .args
        .iter()
        .map(|arg| arg.as_str().into())
        .collect();
    let ending = Arc::new(EndingSink::default());
    let runtime = Runtime::new(Arc::clone(&program), sources.clone(), Arc::new(hosts))
        .with_trace(ending.clone());
    // One entry, one backend of two, and the same three arguments whichever
    // it is: `run_entry` is the seam both are reached through, and a replay
    // reaches it exactly as a run does. What differs between two replays is
    // which evaluator is built; the tape they read is the same object,
    // reached through the one `HostApi` boundary both share.
    let outcome = match &lowered {
        Some(ir) => Lvm::new(&runtime, runtime.hosts(), ir).run_entry(module, entry, program_args),
        None => Interpreter::new(&runtime).run_entry(module, entry, program_args),
    };

    let (used, unchecked, divergence) = {
        let mut tape = tape
            .lock()
            .expect("the replay tape is not shared across threads that panic");
        let divergence = tape.divergence.take().or_else(|| {
            // The other direction: the program finished having made fewer
            // calls than the trace recorded. Only a run that finished can be
            // judged this way — a run that failed stopped for its own reason.
            if outcome.is_ok() && tape.next < tape.steps.len() {
                Some(Divergence::Unused {
                    used: tape.next,
                    total: tape.steps.len(),
                    next: tape.steps[tape.next].shown.clone(),
                })
            } else {
                None
            }
        });
        (tape.next, tape.unchecked_args, divergence)
    };

    // The last thing compared, because it is the last thing that happens: a
    // run that diverged earlier has already been reported for the call it
    // diverged on, and reporting its ending too would bury that cause under a
    // consequence of it. Only the classification is compared. A message is
    // the runtime's own sentence about a stop, and a replay held to its exact
    // wording would call a reworded diagnostic a divergence; the report
    // prints both so a reader can see what each run said.
    let divergence = divergence.or_else(|| {
        let (recorded, recorded_message) = trace.run_outcome()?;
        let (replayed, replayed_message) = ending.ending()?;
        (recorded != replayed).then(|| Divergence::Ended {
            recorded,
            recorded_message: recorded_message.map(str::to_string),
            replayed,
            replayed_message,
        })
    });

    if let Some(divergence) = divergence {
        eprint!("{}", divergence.report());
        // Which backend read the tape and which one wrote it, said where it
        // is most worth knowing: a divergence is the program saying it would
        // behave differently only if the two runs were the same run in every
        // other way, and since ADR 0026 the file is what settles whether
        // they were.
        eprint!("{}", provenance(backend, recorded));
        return Err(CliError::Diverged);
    }

    let value = match outcome {
        Ok(value) => value,
        Err(error) => {
            return Err(CliError::Diagnostics {
                sources,
                items: vec![error.to_diagnostic()],
            })
        }
    };

    println!("replayed {} from {}", run.entry, trace_path.display());
    if backend.recording() == recorded {
        println!("  backend     {backend}, which is the backend that recorded this trace");
    } else {
        println!(
            "  backend     {backend}, and this trace was recorded on {recorded}; this is a\n              cross-backend replay, so a difference could be the two backends'"
        );
    }
    println!("  host calls  {used} of {recorded_calls} recorded call(s), answered from the trace");
    if unchecked > 0 {
        println!("  unchecked   {unchecked} argument(s) the trace does not carry, so not compared");
    }
    println!(
        "  note        no host was called, so nothing outside this process changed; the\n              program's own computation ran for real"
    );
    crate::report_exit(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::Missing;
    use cove_runtime::host::Console;

    const HEADER: &str = r#"{"event":"trace_header","version":4,"backend":"lvm","values":"full","entry":"a.b","args":[]}"#;

    /// A `console.println("hi")` that answered `Ok(())`.
    fn println_line(text: &str) -> String {
        format!(
            r#"{{"event":"host_call","task":0,"module":"console","op":"println","capability":"console","wait_ns":0,"granted":true,"args":[{{"type":"string","value":"{text}"}}],"outcome":{{"kind":"value","value":{{"type":"enum","name":"Result","case":"Ok","payload":[{{"type":"unit"}}]}}}}}}"#
        )
    }

    fn tape_of(lines: &[String]) -> Arc<Mutex<Tape>> {
        let mut text = vec![HEADER.to_string()];
        text.extend(lines.iter().cloned());
        let trace = Trace::read_str(&text.join("\n")).expect("the trace reads");
        let steps = trace.dispatched_calls().into_iter().map(Step::of).collect();
        Arc::new(Mutex::new(Tape::new(steps)))
    }

    /// A registry whose `console` answers from `tape`, gated exactly as a
    /// real run's registry is.
    fn registry(tape: &Arc<Mutex<Tape>>) -> HostRegistry {
        let mut hosts = HostRegistry::new(Grants::new(["console"]));
        for module in cove_runtime::shipped_schema() {
            hosts.register(Box::new(ReplayHost {
                declared: module,
                tape: Arc::clone(tape),
            }));
        }
        hosts
    }

    #[test]
    fn a_matching_call_is_answered_from_the_trace_without_calling_a_host() {
        let tape = tape_of(&[println_line("hi")]);
        let hosts = registry(&tape);
        let value = hosts
            .call("console", "println", vec![Value::string("hi")])
            .expect("the recorded call is answered");
        assert_eq!(trace::show_value(&value), "Ok(())");
        assert_eq!(
            tape.lock()
                .expect("the replay tape is not shared across threads that panic")
                .next,
            1
        );
        assert!(tape
            .lock()
            .expect("the replay tape is not shared across threads that panic")
            .divergence
            .is_none());
        // The real `Console` would have written to its output; this one has
        // no output to write to, which is the point.
        assert!(Console::new(Vec::new(), Vec::new())
            .module_schema()
            .operations
            .iter()
            .any(|op| op.name == "println"));
    }

    /// The first direction: the program asks for something the trace has not
    /// got.
    #[test]
    fn asking_for_a_call_the_trace_does_not_have_diverges() {
        let tape = tape_of(&[]);
        let hosts = registry(&tape);
        let error = hosts
            .call("console", "println", vec![Value::string("hi")])
            .expect_err("an unrecorded call diverges");
        assert!(error.message.contains("were all used"), "{}", error.message);
        let report = tape
            .lock()
            .expect("the replay tape is not shared across threads that panic")
            .divergence
            .as_ref()
            .unwrap()
            .report();
        assert!(report.contains("the trace records  0 call(s)"), "{report}");
        assert!(
            report.contains(r#"the program asked  console.println("hi")"#),
            "{report}"
        );
    }

    #[test]
    fn asking_with_different_arguments_diverges_and_shows_both_calls() {
        let tape = tape_of(&[println_line("hi")]);
        let hosts = registry(&tape);
        hosts
            .call("console", "println", vec![Value::string("bye")])
            .expect_err("a different argument diverges");
        let report = tape
            .lock()
            .expect("the replay tape is not shared across threads that panic")
            .divergence
            .as_ref()
            .unwrap()
            .report();
        assert!(report.contains("at recorded call   1"), "{report}");
        assert!(
            report.contains(r#"the trace records  console.println("hi")"#),
            "{report}"
        );
        assert!(
            report.contains(r#"the program asked  console.println("bye")"#),
            "{report}"
        );
    }

    #[test]
    fn asking_out_of_order_diverges_against_the_call_the_trace_expected_next() {
        let tape = tape_of(&[println_line("one"), println_line("two")]);
        let hosts = registry(&tape);
        hosts
            .call("console", "println", vec![Value::string("two")])
            .expect_err("the recorded order is `one` first");
        let report = tape
            .lock()
            .expect("the replay tape is not shared across threads that panic")
            .divergence
            .as_ref()
            .unwrap()
            .report();
        assert!(
            report.contains(r#"the trace records  console.println("one")"#),
            "{report}"
        );
    }

    /// The other direction: the trace has calls the program never made.
    #[test]
    fn leaving_recorded_calls_unused_is_the_other_direction_of_divergence() {
        let tape = tape_of(&[println_line("one"), println_line("two")]);
        let hosts = registry(&tape);
        hosts
            .call("console", "println", vec![Value::string("one")])
            .expect("the first recorded call is answered");
        let tape = tape
            .lock()
            .expect("the replay tape is not shared across threads that panic");
        assert_eq!(tape.next, 1);
        let divergence = Divergence::Unused {
            used: tape.next,
            total: tape.steps.len(),
            next: tape.steps[tape.next].shown.clone(),
        };
        let report = divergence.report();
        assert!(report.contains("the program made   1"), "{report}");
        assert!(report.contains("the trace records  2 call(s)"), "{report}");
        assert!(
            report.contains(r#"the next recorded  console.println("two")"#),
            "{report}"
        );
    }

    /// `process.exit` is the shipped operation the schema declares not
    /// recordable, and a replay must say so rather than invent a `Unit`.
    #[test]
    fn a_call_the_trace_could_not_record_a_result_for_cannot_be_answered() {
        let tape = tape_of(&[r#"{"event":"host_call","task":0,"module":"process","op":"exit","capability":"process","wait_ns":0,"granted":true,"args":[{"type":"int","value":0}],"outcome":{"kind":"not_recordable"}}"#.to_string()]);
        let mut hosts = HostRegistry::new(Grants::new(["process"]));
        for module in cove_runtime::shipped_schema() {
            hosts.register(Box::new(ReplayHost {
                declared: module,
                tape: Arc::clone(&tape),
            }));
        }
        hosts
            .call("process", "exit", vec![Value::int(0)])
            .expect_err("a call with no recorded result cannot be answered");
        let report = tape
            .lock()
            .expect("the replay tape is not shared across threads that panic")
            .divergence
            .as_ref()
            .unwrap()
            .report();
        assert!(report.contains("not recordable"), "{report}");
        assert!(
            report.contains("would keep running a program that had ended"),
            "{report}"
        );
    }

    #[test]
    fn a_recorded_runtime_error_is_reproduced_as_the_same_error() {
        let tape = tape_of(&[r#"{"event":"host_call","task":0,"module":"console","op":"println","capability":"console","wait_ns":0,"granted":true,"args":[],"outcome":{"kind":"error","message":"console: broken pipe"}}"#.to_string()]);
        let hosts = registry(&tape);
        let error = hosts
            .call("console", "println", Vec::new())
            .expect_err("the recorded refusal is reproduced");
        assert_eq!(error.message, "console: broken pipe");
        assert!(
            tape.lock()
                .expect("the replay tape is not shared across threads that panic")
                .divergence
                .is_none(),
            "a refusal is not a divergence"
        );
    }

    #[test]
    fn a_redacted_argument_cannot_be_compared_and_is_counted_instead() {
        let tape = tape_of(&[r#"{"event":"host_call","task":0,"module":"console","op":"println","capability":"console","wait_ns":0,"granted":true,"args":[{"type":"redacted","of":"String"}],"outcome":{"kind":"value","value":{"type":"enum","name":"Result","case":"Ok","payload":[{"type":"unit"}]}}}"#.to_string()]);
        let hosts = registry(&tape);
        hosts
            .call("console", "println", vec![Value::string("anything")])
            .expect("a redacted argument cannot disagree");
        assert_eq!(
            tape.lock()
                .expect("the replay tape is not shared across threads that panic")
                .unchecked_args,
            1
        );
    }

    #[test]
    fn a_missing_result_reports_why_it_is_missing() {
        assert!(Missing::Redacted("String".into())
            .reason()
            .contains("--trace-values redacted"));
    }

    /// A replay is gated by the same grants the run had, so a call the run
    /// was never granted is refused before the tape is consulted.
    #[test]
    fn an_ungranted_call_is_refused_by_the_registry_not_by_the_tape() {
        let tape = tape_of(&[println_line("hi")]);
        let hosts = registry(&tape);
        hosts
            .call("files", "read", vec![Value::string("a.txt")])
            .expect_err("`files` was not granted");
        assert!(tape
            .lock()
            .expect("the replay tape is not shared across threads that panic")
            .divergence
            .is_none());
        assert_eq!(
            tape.lock()
                .expect("the replay tape is not shared across threads that panic")
                .next,
            0
        );
    }
}
