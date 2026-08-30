//! Calling an exported function with arguments the host built.
//!
//! `crates/cove-runtime/tests/embedding.rs` proves a host outside this crate
//! can load Cove, register a module of its own, and impose its own limits.
//! What it cannot show is a host *saying* anything to the program beyond a
//! list of strings: `run_entry` takes the process arguments an entry may
//! declare, so `evaluate(pr: PullRequest) -> Decision` — the signature a rule
//! engine actually wants — was not callable from Rust at all. That is issue
//! #150, and `Interpreter::invoke` and `Vm::invoke` are the answer.
//!
//! Every case runs on **both backends** and asserts they agree, because the
//! differential harness cannot reach this path: it runs entries the way `cove
//! run` does, and no `[run.<name>]` table can hand a struct to a function.
//! ADR 0019's rule is that anything both backends answer they answer the same
//! way, and a new way in is exactly where that stops being free.
//!
//! The refusals are held to as tightly as the successes. A host that hands
//! over the wrong thing must be told which argument, what was expected there,
//! and what arrived — before the first instruction runs, and identically on
//! either backend. Where that matters most is the VM: the lowering has spent
//! the checker's answer by the time a parameter is a slot, so a `String`
//! placed in a slot the lowering made scalar is not a wrong answer but a
//! panic. The check is what makes `invoke` a `Result`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cove_diag::SourceMap;
use cove_runtime::interp::Interpreter;
use cove_runtime::trace::{RunOutcome, TraceEvent, TraceSink};
use cove_runtime::{Grants, HostRegistry, Runtime, RuntimeError, Value, Vm};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program;
use cove_sema::{Compiler, Config};

/// The package every case below invokes into.
///
/// Five callable declarations and three a host may not call, so that both
/// halves of what `invoke` decides are decided against one program. The three
/// are `identity`, `bump` and `joined`: a type parameter nothing can settle, a
/// `var` parameter that names a place in a caller's frame, and a variadic one
/// whose count is open.
const SOURCE: &str = "\
/// A note the host holds and hands over.
export struct Note {
  id: String
  tags: Array<String>
  weight: Int
}

/// What the rules made of one.
export enum Verdict {
  /// Worth keeping, as it stands.
  Keep
  /// Not worth keeping, and why.
  Drop(String)
}

/// The signature issue #150 says a host cannot call.
export fn judge(note: Note) -> Verdict {
  if note.weight > 10 {
    return Verdict.Drop(\"{note.id} is too heavy\")
  }
  if note.tags.isEmpty() {
    return Verdict.Drop(\"{note.id} has no tags\")
  }
  Verdict.Keep
}

/// Two arguments rather than one, so arity is checked against something.
export fn describe(note: Note, prefix: String) -> String {
  \"{prefix}:{note.id}:{note.weight}\"
}

/// One `Int`, which is the case the VM lowers to a scalar slot.
export fn doubled(n: Int) -> Int {
  n * 2
}

/// No parameters at all, which an invocation may still call.
export fn floor() -> Int {
  7
}

/// A parameter with a default, which a call site may omit.
export fn weighted(n: Int, by: Int = 3) -> Int {
  n * by
}

/// A type parameter no argument can settle.
export fn identity<T>(value: T) -> T {
  value
}

/// A `var` parameter, which aliases a place in the caller's frame.
export fn bump(var n: Int) -> Unit {
  n = n + 1
}

/// A variadic parameter, whose count is open.
export fn joined(parts: String...) -> Int {
  parts.length()
}

/// A declared enum as a parameter, so a case and a payload are checked.
export fn reason(verdict: Verdict) -> String {
  match verdict {
    Verdict.Keep => \"\"
    Verdict.Drop(why) => why
  }
}

/// A generic struct, whose field is recorded as the parameter it was written
/// as.
export struct Boxed<T> {
  held: T
}

/// One use of it, which is what the field is checked against.
export fn unbox(boxed: Boxed<Int>) -> Int {
  boxed.held
}
";

/// Which backend a case ran on, so a failure names one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Ast,
    Vm,
}

/// Both of them, for the cases that assert the two agree.
const BOTH: [Backend; 2] = [Backend::Ast, Backend::Vm];

/// What a case gets back: what the invocation answered, and every terminal
/// event the run wrote.
type Answered = (
    Result<Value, RuntimeError>,
    Vec<(RunOutcome, Option<String>)>,
);

/// Every `run_ended` event a run wrote, which is how a case asserts that a
/// refusal is still a run that ended.
#[derive(Default)]
struct Ended(Mutex<Vec<(RunOutcome, Option<String>)>>);

impl TraceSink for Ended {
    fn record(&self, event: TraceEvent) {
        if let TraceEvent::RunEnded { outcome, message } = event {
            self.0.lock().unwrap().push((outcome, message));
        }
    }
}

/// Parses and checks [`SOURCE`] as a one-module package written in memory.
fn checked() -> (Arc<SourceMap>, Arc<Program>) {
    let (sources, package) = packaged(SOURCE);
    match Compiler::new().compile(&package) {
        Ok(program) => (Arc::new(sources), Arc::new(program)),
        Err(items) => panic!(
            "the fixture checks:\n{}",
            items
                .iter()
                .map(|item| cove_diag::render(&sources, item))
                .collect::<Vec<_>>()
                .join("")
        ),
    }
}

fn packaged(text: &str) -> (SourceMap, Package) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("m/main.cove");
    let file = sources.add(path.clone(), text);
    let ast = cove_syntax::parse_file(&sources, file).expect("the fixture parses");
    (
        sources,
        Package {
            root: PathBuf::new(),
            config: Config::default(),
            modules: BTreeMap::from([(
                "m".to_string(),
                Module {
                    name: "m".to_string(),
                    dir: PathBuf::from("m"),
                    units: vec![Unit { file, path, ast }],
                },
            )]),
        },
    )
}

/// Invokes `m.name` with `args` on `backend`, and answers what it produced
/// beside every terminal event the run wrote.
///
/// The whole package is lowered rather than one entry, because a case invokes
/// several functions through one VM and `lower_entry` lowers what one entry
/// reaches. What that costs is a lowering of `identity`, `bump` and `joined`
/// as well, which is worth having: it says the three are refused by the check
/// and not by the lowering failing to produce them.
fn invoke(backend: Backend, name: &str, args: Vec<Value>) -> Answered {
    let (sources, program) = checked();
    on(backend, sources, program, name, args)
}

/// The same, over a program a caller built however it liked.
fn on(
    backend: Backend,
    sources: Arc<SourceMap>,
    program: Arc<Program>,
    name: &str,
    args: Vec<Value>,
) -> Answered {
    let ended = Arc::new(Ended::default());
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(program.clone(), sources, hosts.clone())
        .with_trace(Arc::clone(&ended) as Arc<dyn TraceSink>);
    // No `on_cove_stack` here, and deliberately: a `Value` is `Rc`-based and
    // cannot leave the thread that made it, so a case that wants to look at
    // one has to stay on the thread it ran on. These fixtures nest one call
    // deep, which is what makes that affordable; an embedder's program is not
    // and should take the thread `Interpreter::run_entry` documents.
    let answer = match backend {
        Backend::Ast => Interpreter::new(&runtime).invoke("m", name, args),
        Backend::Vm => {
            let ir = Arc::new(cove_ir::lower::lower(&program).expect("the fixture lowers"));
            Vm::new(&runtime, &hosts, &ir).invoke("m", name, args)
        }
    };
    let events = ended.0.lock().unwrap().clone();
    (answer, events)
}

/// The note a host builds, without naming a runtime representation to do it.
fn note(id: &str, tags: &[&str], weight: i64) -> Value {
    Value::structure(
        "m.Note",
        [
            ("id", Value::Str(id.into())),
            (
                "tags",
                Value::array(tags.iter().map(|tag| Value::Str((*tag).into()))),
            ),
            ("weight", Value::Int(weight)),
        ],
    )
}

/// Which case an answered enum is, and what it carried.
fn verdict(value: &Value) -> (String, Vec<String>) {
    let Value::Enum(case) = value else {
        panic!("expected a `Verdict`, found {value}");
    };
    assert_eq!(&*case.type_name, "m.Verdict");
    (
        case.case.to_string(),
        case.payload.iter().map(Value::to_string).collect(),
    )
}

/// The message a refusal carried, with the rule and help it quoted, so a case
/// can hold all three.
fn refusal(answer: Result<Value, RuntimeError>) -> (String, Option<String>, Option<String>) {
    let error = answer.expect_err("this invocation must be refused");
    (error.message, error.rule, error.help)
}

// ---------------------------------------------------------------- the point

/// What issue #150 asked for: a host builds a value of a type the package
/// declares, hands it to an exported function, and reads what came back.
#[test]
fn a_host_invokes_an_exported_function_with_a_value_it_built() {
    for backend in BOTH {
        let (answer, ended) = invoke(backend, "judge", vec![note("n-1", &["docs"], 3)]);
        let value = answer.unwrap_or_else(|e| panic!("{backend:?}: {}", e.message));
        assert_eq!(
            verdict(&value),
            ("Keep".to_string(), Vec::new()),
            "{backend:?}"
        );
        assert_eq!(
            ended,
            vec![(RunOutcome::Success, None)],
            "{backend:?}: an invocation is a run and ends like one"
        );

        let (answer, _) = invoke(backend, "judge", vec![note("n-2", &["docs"], 99)]);
        assert_eq!(
            verdict(&answer.expect("a heavy note is judged too")),
            ("Drop".to_string(), vec!["n-2 is too heavy".to_string()]),
            "{backend:?}"
        );
    }
}

/// The scalar case, which is the one that would not have been an error at all
/// without a check: `doubled(n: Int)` lowers to a scalar slot, and what a
/// host puts there is read as an `Int` whatever it was.
#[test]
fn a_scalar_parameter_takes_the_value_and_refuses_anything_else() {
    for backend in BOTH {
        let (answer, _) = invoke(backend, "doubled", vec![Value::Int(21)]);
        assert!(
            matches!(answer, Ok(Value::Int(42))),
            "{backend:?}: {answer:?}"
        );

        let (message, rule, help) =
            refusal(invoke(backend, "doubled", vec![Value::Str("21".into())]).0);
        assert_eq!(
            message, "`m.doubled` was given `String` as argument 1, but it declares `Int` there",
            "{backend:?}"
        );
        assert_eq!(help.as_deref(), Some("`m.doubled` declares fn(Int) -> Int"));
        assert!(rule.is_some_and(|rule| rule.contains("a value of each declared type")));
    }
}

/// A function with no parameters is still one an invocation may call, which
/// is what makes `invoke` a replacement for `run_entry` rather than a
/// companion to it for one shape only.
#[test]
fn a_function_with_no_parameters_is_invoked_with_no_arguments() {
    for backend in BOTH {
        let (answer, _) = invoke(backend, "floor", Vec::new());
        assert!(matches!(answer, Ok(Value::Int(7))), "{backend:?}");
    }
}

/// A struct of another declared type is refused by name, which is the
/// shallowest thing this check does and the first thing it does.
#[test]
fn a_struct_of_another_declared_type_is_refused_by_name() {
    for backend in BOTH {
        let wrong = Value::structure("m.Trinket", [("id", Value::Str("t".into()))]);
        let (message, _, _) = refusal(invoke(backend, "judge", vec![wrong]).0);
        assert_eq!(
            message,
            "`m.judge` was given `m.Trinket` as argument 1, but it declares `m.Note` there",
            "{backend:?}"
        );
    }
}

/// A struct of the *right* type carrying the wrong fields is refused too, and
/// this is the case that has to be refused rather than merely reported.
///
/// The lowering reads a declared struct's field by index, so a value carrying
/// nine of ten fields would have the VM read past its end and a value carrying
/// ten in another order would answer the wrong one silently. Neither is a
/// mistake a host would be told about by running.
#[test]
fn a_struct_carrying_the_wrong_fields_is_refused_by_shape() {
    for backend in BOTH {
        let missing = Value::structure(
            "m.Note",
            [("id", Value::Str("n-3".into())), ("weight", Value::Int(1))],
        );
        let (message, _, _) = refusal(invoke(backend, "judge", vec![missing]).0);
        assert_eq!(
            message,
            "`m.judge` was given a `m.Note` carrying `id`, `weight` as argument 1, but `m.Note` declares `id`, `tags`, `weight`, in that order",
            "{backend:?}"
        );

        let reordered = Value::structure(
            "m.Note",
            [
                ("id", Value::Str("n-3".into())),
                ("weight", Value::Int(1)),
                ("tags", Value::array([Value::Str("docs".into())])),
            ],
        );
        let (message, _, _) = refusal(invoke(backend, "judge", vec![reordered]).0);
        assert!(
            message.contains("carrying `id`, `weight`, `tags`"),
            "{backend:?}: {message}"
        );
    }
}

/// The declared type is followed all the way down — into a parameter's own
/// type arguments, and into a declared struct's fields — and the
/// disagreement is reported where it happens.
#[test]
fn a_declared_type_is_followed_into_what_it_contains() {
    for backend in BOTH {
        let wrong = Value::structure(
            "m.Note",
            [
                ("id", Value::Str("n-3".into())),
                (
                    "tags",
                    Value::array([Value::Str("docs".into()), Value::Int(2)]),
                ),
                ("weight", Value::Int(1)),
            ],
        );
        let (message, _, _) = refusal(invoke(backend, "judge", vec![wrong]).0);
        assert_eq!(
            message,
            "`m.judge` was given `Int` at `.tags[1]` of argument 1, but it declares `String` there",
            "{backend:?}"
        );

        let (message, _, _) = refusal(
            invoke(
                backend,
                "describe",
                vec![note("n-4", &["a"], 1), Value::Int(2)],
            )
            .0,
        );
        assert_eq!(
            message, "`m.describe` was given `Int` as argument 2, but it declares `String` there",
            "{backend:?}"
        );
    }
}

/// A declared enum is held to its declaration too: the case has to be one it
/// lists, and the payload has to be what that case carries.
///
/// An enum is safer than a struct — both backends read a case by name and
/// bounds-check a payload — so this is refused for a weaker reason: a case no
/// declaration lists matches no arm of any `match`, and what the host would
/// see is wherever the program gave up rather than what it did wrong.
#[test]
fn a_declared_enum_is_held_to_its_case_and_its_payload() {
    for backend in BOTH {
        let (answer, _) = invoke(
            backend,
            "reason",
            vec![Value::enumeration(
                "m.Verdict",
                "Drop",
                [Value::Str("x".into())],
            )],
        );
        assert!(
            matches!(&answer, Ok(Value::Str(text)) if &**text == "x"),
            "{backend:?}: {answer:?}"
        );

        let (message, _, _) = refusal(
            invoke(
                backend,
                "reason",
                vec![Value::enumeration("m.Verdict", "Shred", Vec::new())],
            )
            .0,
        );
        assert_eq!(
            message,
            "`m.reason` was given `m.Verdict.Shred` as argument 1, but it declares `m.Verdict` there",
            "{backend:?}"
        );

        let (message, _, _) = refusal(
            invoke(
                backend,
                "reason",
                vec![Value::enumeration("m.Verdict", "Drop", [Value::Int(1)])],
            )
            .0,
        );
        assert_eq!(
            message,
            "`m.reason` was given `Int` at `Drop(_)[0]` of argument 1, but it declares `String` there",
            "{backend:?}"
        );

        let (message, _, _) = refusal(
            invoke(
                backend,
                "reason",
                vec![Value::enumeration("m.Verdict", "Keep", [Value::Int(1)])],
            )
            .0,
        );
        assert_eq!(
            message,
            "`m.reason` was given `m.Verdict.Keep carrying 1 value` as argument 1, but it declares `m.Verdict.Keep carrying 0 values` there",
            "{backend:?}"
        );
    }
}

/// A generic struct's field is recorded as the type parameter it was written
/// as, so a use of it is checked against the argument the *use* supplied.
#[test]
fn a_generic_struct_is_checked_against_the_arguments_the_use_was_written_with() {
    for backend in BOTH {
        let boxed = |value| Value::structure("m.Boxed", [("held", value)]);
        let (answer, _) = invoke(backend, "unbox", vec![boxed(Value::Int(5))]);
        assert!(
            matches!(answer, Ok(Value::Int(5))),
            "{backend:?}: {answer:?}"
        );

        let (message, _, _) =
            refusal(invoke(backend, "unbox", vec![boxed(Value::Str("5".into()))]).0);
        assert_eq!(
            message,
            "`m.unbox` was given `String` at `.held` of argument 1, but it declares `Int` there",
            "{backend:?}"
        );
    }
}

// ------------------------------------------------------------- what refuses

/// Arity is the declared parameter count, exactly.
#[test]
fn too_few_or_too_many_arguments_are_refused_before_anything_runs() {
    for backend in BOTH {
        let (message, _, help) = refusal(invoke(backend, "describe", vec![Value::Int(1)]).0);
        assert_eq!(
            message, "`m.describe` takes 2 parameters, but 1 was given",
            "{backend:?}"
        );
        assert_eq!(
            help.as_deref(),
            Some("supply one value for each declared parameter")
        );

        let (message, _, _) = refusal(
            invoke(
                backend,
                "floor",
                vec![Value::Int(1), Value::Int(2), Value::Int(3)],
            )
            .0,
        );
        assert_eq!(
            message, "`m.floor` takes 0 parameters, but 3 were given",
            "{backend:?}"
        );
    }
}

/// A default is an expression a *call site* supplies, and a host is not one:
/// `weighted(n, by: 3)` may be written `weighted(4)` in Cove and may not be
/// invoked with one argument. The refusal says so rather than leaving a host
/// to infer it from a count.
#[test]
fn a_defaulted_parameter_must_still_be_supplied() {
    for backend in BOTH {
        let (message, _, help) = refusal(invoke(backend, "weighted", vec![Value::Int(4)]).0);
        assert_eq!(
            message, "`m.weighted` takes 2 parameters, but 1 was given",
            "{backend:?}"
        );
        assert_eq!(
            help.as_deref(),
            Some("`by` has a default, which a call site may omit and an invocation may not: supply one value for each declared parameter"),
            "{backend:?}"
        );

        let (answer, _) = invoke(backend, "weighted", vec![Value::Int(4), Value::Int(5)]);
        assert!(matches!(answer, Ok(Value::Int(20))), "{backend:?}");
    }
}

/// The three declaration shapes a host may not call at all, refused from the
/// declaration and therefore identically on both backends.
#[test]
fn a_type_parameter_a_var_and_a_variadic_are_refused_from_the_declaration() {
    for backend in BOTH {
        let (message, _, help) = refusal(invoke(backend, "identity", vec![Value::Int(1)]).0);
        assert_eq!(
            message,
            "`m.identity` declares the type parameter `T`, which an invocation cannot settle",
            "{backend:?}"
        );
        assert!(help.is_some_and(|help| help.contains("supplies values, not types")));

        let (message, _, help) = refusal(invoke(backend, "bump", vec![Value::Int(1)]).0);
        assert_eq!(
            message, "`m.bump` declares `var n`, which an invocation cannot supply",
            "{backend:?}"
        );
        assert!(help.is_some_and(|help| help.contains("has no frame")));

        let (message, _, _) = refusal(invoke(backend, "joined", vec![Value::Str("a".into())]).0);
        assert_eq!(
            message,
            "`m.joined` declares the variadic parameter `parts`, which an invocation cannot supply",
            "{backend:?}"
        );
    }
}

/// A name the package does not declare is refused in the words `run_entry`
/// already refuses one in.
#[test]
fn a_function_the_package_does_not_declare_is_refused_by_name() {
    for backend in BOTH {
        let (message, _, _) = refusal(invoke(backend, "absent", Vec::new()).0);
        assert_eq!(
            message, "this package does not declare `m.absent`",
            "{backend:?}"
        );
    }
}

/// A refused invocation is still a run, and ends with an event saying so.
///
/// This is what keeps `cove trace`'s promise true of the new way in: every
/// run has exactly one terminal event, whether it reached its first
/// instruction or not.
#[test]
fn a_refused_invocation_still_ends_the_run_it_never_started() {
    for backend in BOTH {
        let (_, ended) = invoke(backend, "doubled", vec![Value::Unit]);
        assert_eq!(ended.len(), 1, "{backend:?}");
        assert_eq!(ended[0].0, RunOutcome::Invariant, "{backend:?}");
        assert!(
            ended[0]
                .1
                .as_deref()
                .is_some_and(|message| message.contains("but it declares `Int` there")),
            "{backend:?}: {ended:?}"
        );
    }
}

// ------------------------------------------------- the two backend-specific

/// A VM built for one entry cannot invoke a function no path from that entry
/// reaches, and says which of the two things is missing.
///
/// `lower_entry` lowers what one entry reaches and nothing else, so this is a
/// shape an embedder will meet the first time it holds two entries. Telling
/// it that the package does not declare the function would be false and would
/// send it to the wrong file.
#[test]
fn a_vm_lowered_for_one_entry_says_which_of_the_two_is_missing() {
    let (sources, program) = checked();
    let ir = Arc::new(
        cove_ir::lower::lower_entry(&program, "m", "floor")
            .expect("the entry lowers")
            .program,
    );
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(program, sources, hosts.clone());
    let answer = Vm::new(&runtime, &hosts, &ir).invoke("m", "judge", vec![note("n-5", &["a"], 1)]);

    let (message, _, help) = refusal(answer);
    assert_eq!(message, "this run's lowering does not include `m.judge`");
    assert!(help.is_some_and(|help| help.contains("lower_entry(program, \"m\", \"judge\")")));
}

/// A program that was resolved but never checked has no signatures to hold an
/// invocation to, and is told so rather than being invoked unchecked.
///
/// `cove_sema::resolve::resolve` is a real path — an interpreted `cove build`
/// binary takes it — and on it `run_entry` still works, because the process
/// arguments are strings whatever the checker did or did not record. What
/// cannot work is holding a host's own value to a type nothing wrote down.
#[test]
fn a_program_that_was_resolved_but_not_checked_refuses_to_be_invoked() {
    let (sources, package) = packaged(SOURCE);
    let program = Arc::new(cove_sema::resolve::resolve(&package).expect("the fixture resolves"));
    let (answer, _) = on(
        Backend::Ast,
        Arc::new(sources),
        program,
        "doubled",
        vec![Value::Int(1)],
    );
    let (message, _, help) = refusal(answer);
    assert!(
        message.contains("this program was resolved but not checked"),
        "{message}"
    );
    assert!(help.is_some_and(|help| help.contains("Compiler::new().compile")));
}

/// The two backends answer the same thing, which is the whole of what ADR
/// 0019 asks of a way in that both of them have.
///
/// Every case above already runs on both. This one states the claim directly,
/// over every callable declaration and every refusal, so that a change that
/// made one backend answer differently fails here by name rather than in
/// whichever case happened to notice.
#[test]
fn both_backends_answer_one_invocation_the_same_way() {
    let calls: Vec<(&str, Vec<Value>)> = vec![
        ("judge", vec![note("n-6", &["docs"], 3)]),
        ("judge", vec![note("n-7", &[], 3)]),
        (
            "describe",
            vec![note("n-8", &["a"], 4), Value::Str("p".into())],
        ),
        ("doubled", vec![Value::Int(3)]),
        ("floor", Vec::new()),
        ("weighted", vec![Value::Int(2), Value::Int(5)]),
        ("doubled", vec![Value::Unit]),
        ("describe", vec![note("n-9", &["a"], 1)]),
        ("identity", vec![Value::Int(1)]),
        ("bump", vec![Value::Int(1)]),
        ("joined", vec![Value::Str("a".into())]),
        (
            "reason",
            vec![Value::enumeration(
                "m.Verdict",
                "Drop",
                [Value::Str("x".into())],
            )],
        ),
        (
            "reason",
            vec![Value::enumeration("m.Verdict", "Shred", Vec::new())],
        ),
        (
            "unbox",
            vec![Value::structure("m.Boxed", [("held", Value::Int(5))])],
        ),
        (
            "unbox",
            vec![Value::structure("m.Boxed", [("held", Value::Unit)])],
        ),
        (
            "judge",
            vec![Value::structure("m.Note", [("id", Value::Str("n".into()))])],
        ),
        ("absent", Vec::new()),
    ];
    for (name, args) in calls {
        let ast = described(invoke(Backend::Ast, name, args.clone()).0);
        let vm = described(invoke(Backend::Vm, name, args).0);
        assert_eq!(ast, vm, "the two backends disagree about `m.{name}`");
    }
}

/// One answer, as a string, so two backends' answers compare as one value.
fn described(answer: Result<Value, RuntimeError>) -> String {
    match answer {
        Ok(value) => format!("ok {value}"),
        Err(error) => format!("err {}", error.message),
    }
}
