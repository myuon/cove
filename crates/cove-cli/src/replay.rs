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

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use cove_runtime::host::{HostApi, HostRegistry};
use cove_runtime::interp::Interpreter;
use cove_runtime::schema::OperationSchema;
use cove_runtime::{
    value_to_json, Budget, Cancellation, Grants, Limits, RuntimeError, Value, ValueCapture,
};
use cove_sema::Capability;

use crate::trace::{self, Outcome, Trace};
use crate::CliError;

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
    Value(Value),
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
                Ok(value) => Answer::Value(value.clone()),
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
            Answer::Value(value) => Ok(value.clone()),
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
        }
        out.push_str(
            "  rule               a replay answers every Host API call from the trace, in the\n\
             \x20                    recorded order; the program's own computation runs for real,\n\
             \x20                    so a divergence means it took a different path than it did\n",
        );
        out
    }
}

/// A host module that answers from a trace instead of from the world.
///
/// It carries the real module's name, capability, and schema, so the registry
/// gates, checks arity, and counts irreversible writes exactly as it does for
/// a real run — a replay that skipped those checks would be reproducing a
/// different boundary than the one that was recorded.
struct ReplayHost {
    name: String,
    capability: Capability,
    operations: Vec<OperationSchema>,
    tape: Rc<RefCell<Tape>>,
}

impl HostApi for ReplayHost {
    fn name(&self) -> &str {
        &self.name
    }

    fn capability(&self) -> Capability {
        self.capability.clone()
    }

    fn schema(&self) -> &[OperationSchema] {
        &self.operations
    }

    fn call(&mut self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let name = self.name.clone();
        self.tape.borrow_mut().answer(&name, op, &args)
    }
}

/// `cove replay <trace> <run-name>`.
pub(crate) fn cmd_replay(args: &[String]) -> Result<(), CliError> {
    let positional: Vec<&String> = args.iter().filter(|arg| !arg.starts_with("--")).collect();
    if let Some(flag) = args.iter().find(|arg| arg.starts_with("--")) {
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

    let steps: Vec<Step> = trace.dispatched_calls().into_iter().map(Step::of).collect();
    let recorded_calls = steps.len();
    let tape = Rc::new(RefCell::new(Tape::new(steps)));

    let mut hosts = HostRegistry::new(Grants::new(run.allow.clone()));
    for module in cove_runtime::shipped_schema() {
        hosts.register(Box::new(ReplayHost {
            name: module.name,
            capability: module.capability,
            operations: module.operations,
            tape: Rc::clone(&tape),
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
        },
        Cancellation::new(),
    ));

    let program_args: Vec<Rc<str>> = trace
        .header
        .args
        .iter()
        .map(|arg| arg.as_str().into())
        .collect();
    let outcome = {
        let mut interpreter = Interpreter::new(&program, &sources, &mut hosts);
        interpreter.run_entry(module, entry, program_args)
    };

    let (used, unchecked, divergence) = {
        let mut tape = tape.borrow_mut();
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

    if let Some(divergence) = divergence {
        eprint!("{}", divergence.report());
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

    const HEADER: &str =
        r#"{"event":"trace_header","version":1,"values":"full","entry":"a.b","args":[]}"#;

    /// A `console.println("hi")` that answered `Ok(())`.
    fn println_line(text: &str) -> String {
        format!(
            r#"{{"event":"host_call","module":"console","op":"println","capability":"console","wait_ns":0,"granted":true,"args":[{{"type":"string","value":"{text}"}}],"outcome":{{"kind":"value","value":{{"type":"enum","name":"Result","case":"Ok","payload":[{{"type":"unit"}}]}}}}}}"#
        )
    }

    fn tape_of(lines: &[String]) -> Rc<RefCell<Tape>> {
        let mut text = vec![HEADER.to_string()];
        text.extend(lines.iter().cloned());
        let trace = Trace::read_str(&text.join("\n")).expect("the trace reads");
        let steps = trace.dispatched_calls().into_iter().map(Step::of).collect();
        Rc::new(RefCell::new(Tape::new(steps)))
    }

    /// A registry whose `console` answers from `tape`, gated exactly as a
    /// real run's registry is.
    fn registry(tape: &Rc<RefCell<Tape>>) -> HostRegistry {
        let mut hosts = HostRegistry::new(Grants::new(["console"]));
        for module in cove_runtime::shipped_schema() {
            hosts.register(Box::new(ReplayHost {
                name: module.name,
                capability: module.capability,
                operations: module.operations,
                tape: Rc::clone(tape),
            }));
        }
        hosts
    }

    #[test]
    fn a_matching_call_is_answered_from_the_trace_without_calling_a_host() {
        let tape = tape_of(&[println_line("hi")]);
        let mut hosts = registry(&tape);
        let value = hosts
            .call("console", "println", vec![Value::Str("hi".into())])
            .expect("the recorded call is answered");
        assert_eq!(trace::show_value(&value), "Ok(())");
        assert_eq!(tape.borrow().next, 1);
        assert!(tape.borrow().divergence.is_none());
        // The real `Console` would have written to its output; this one has
        // no output to write to, which is the point.
        assert!(Console::new(Vec::new())
            .schema()
            .iter()
            .any(|op| op.name == "println"));
    }

    /// The first direction: the program asks for something the trace has not
    /// got.
    #[test]
    fn asking_for_a_call_the_trace_does_not_have_diverges() {
        let tape = tape_of(&[]);
        let mut hosts = registry(&tape);
        let error = hosts
            .call("console", "println", vec![Value::Str("hi".into())])
            .expect_err("an unrecorded call diverges");
        assert!(error.message.contains("were all used"), "{}", error.message);
        let report = tape.borrow().divergence.as_ref().unwrap().report();
        assert!(report.contains("the trace records  0 call(s)"), "{report}");
        assert!(
            report.contains(r#"the program asked  console.println("hi")"#),
            "{report}"
        );
    }

    #[test]
    fn asking_with_different_arguments_diverges_and_shows_both_calls() {
        let tape = tape_of(&[println_line("hi")]);
        let mut hosts = registry(&tape);
        hosts
            .call("console", "println", vec![Value::Str("bye".into())])
            .expect_err("a different argument diverges");
        let report = tape.borrow().divergence.as_ref().unwrap().report();
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
        let mut hosts = registry(&tape);
        hosts
            .call("console", "println", vec![Value::Str("two".into())])
            .expect_err("the recorded order is `one` first");
        let report = tape.borrow().divergence.as_ref().unwrap().report();
        assert!(
            report.contains(r#"the trace records  console.println("one")"#),
            "{report}"
        );
    }

    /// The other direction: the trace has calls the program never made.
    #[test]
    fn leaving_recorded_calls_unused_is_the_other_direction_of_divergence() {
        let tape = tape_of(&[println_line("one"), println_line("two")]);
        let mut hosts = registry(&tape);
        hosts
            .call("console", "println", vec![Value::Str("one".into())])
            .expect("the first recorded call is answered");
        let tape = tape.borrow();
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
        let tape = tape_of(&[r#"{"event":"host_call","module":"process","op":"exit","capability":"process","wait_ns":0,"granted":true,"args":[{"type":"int","value":0}],"outcome":{"kind":"not_recordable"}}"#.to_string()]);
        let mut hosts = HostRegistry::new(Grants::new(["process"]));
        for module in cove_runtime::shipped_schema() {
            hosts.register(Box::new(ReplayHost {
                name: module.name,
                capability: module.capability,
                operations: module.operations,
                tape: Rc::clone(&tape),
            }));
        }
        hosts
            .call("process", "exit", vec![Value::Int(0)])
            .expect_err("a call with no recorded result cannot be answered");
        let report = tape.borrow().divergence.as_ref().unwrap().report();
        assert!(report.contains("not recordable"), "{report}");
        assert!(
            report.contains("would keep running a program that had ended"),
            "{report}"
        );
    }

    #[test]
    fn a_recorded_runtime_error_is_reproduced_as_the_same_error() {
        let tape = tape_of(&[r#"{"event":"host_call","module":"console","op":"println","capability":"console","wait_ns":0,"granted":true,"args":[],"outcome":{"kind":"error","message":"console: broken pipe"}}"#.to_string()]);
        let mut hosts = registry(&tape);
        let error = hosts
            .call("console", "println", Vec::new())
            .expect_err("the recorded refusal is reproduced");
        assert_eq!(error.message, "console: broken pipe");
        assert!(
            tape.borrow().divergence.is_none(),
            "a refusal is not a divergence"
        );
    }

    #[test]
    fn a_redacted_argument_cannot_be_compared_and_is_counted_instead() {
        let tape = tape_of(&[r#"{"event":"host_call","module":"console","op":"println","capability":"console","wait_ns":0,"granted":true,"args":[{"type":"redacted","of":"String"}],"outcome":{"kind":"value","value":{"type":"enum","name":"Result","case":"Ok","payload":[{"type":"unit"}]}}}"#.to_string()]);
        let mut hosts = registry(&tape);
        hosts
            .call("console", "println", vec![Value::Str("anything".into())])
            .expect("a redacted argument cannot disagree");
        assert_eq!(tape.borrow().unchecked_args, 1);
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
        let mut hosts = registry(&tape);
        hosts
            .call("files", "read", vec![Value::Str("a.txt".into())])
            .expect_err("`files` was not granted");
        assert!(tape.borrow().divergence.is_none());
        assert_eq!(tape.borrow().next, 0);
    }
}
