//! The linear-memory backend against the semantic oracle.
//!
//! [ADR 0034](../../../../docs/adr/0034-one-physical-word-stack.md) keeps the
//! tree-walking interpreter as the definition of what a Cove program means,
//! and makes the replacement's completion conditional on agreeing with it.
//! This is where that agreement is checked at the level of one source program
//! at a time, from the source text through the checker, the lowering and the
//! machine, and compared against the same source run on the interpreter.
//!
//! It is deliberately not a listing test. `cove-ir`'s own suite pins what
//! each construct lowers to, and this asks a different question: whatever it
//! lowered to, does running it answer what the language says? A case here
//! that fails while the listing tests pass is a machine bug; the other way
//! round is a lowering bug; both failing is a shared misreading of the
//! checker.
//!
//! The corpus here is written by hand rather than drawn from `tests/e2e`:
//! whether the lowering covers the repository's own programs is
//! `cove-cli/tests/vm_coverage.rs`'s question, and it answers that the
//! corpus lowers, runs, and agrees with the oracle. What is added here is
//! smaller and more deliberate — a case chosen to pin one construct's
//! agreement rather than to widen coverage.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_sema::config::Config;
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program as Checked;

use crate::host::{Grants, HostRegistry};
use crate::interp::Interpreter;
use crate::runtime::Runtime;
use crate::value::Value;
use crate::vm::Vm;

/// Parses, resolves and checks one module called `m`.
fn checked(source: &str) -> (Arc<SourceMap>, Arc<Checked>) {
    let mut sources = SourceMap::new();
    let file = sources.add("m/main.cove", source.to_string());
    let ast = match cove_syntax::parse_file(&sources, file) {
        Ok(ast) => ast,
        Err(items) => panic!("the source parses:\n{}", rendered(&sources, &items)),
    };
    let mut modules = BTreeMap::from([(
        "m".to_string(),
        Module {
            name: "m".to_string(),
            dir: PathBuf::from("m"),
            units: vec![Unit {
                file,
                path: PathBuf::from("m/main.cove"),
                ast,
            }],
        },
    )]);
    for (name, module) in cove_sema::stdlib::attach(&mut sources).expect("stdlib parses") {
        modules.insert(name, module);
    }
    let package = Package {
        root: PathBuf::from("."),
        config: Config::default(),
        modules,
    };
    match cove_sema::Compiler::new().compile(&package) {
        Ok(program) => (Arc::new(sources), Arc::new(program)),
        Err(items) => panic!("the source checks:\n{}", rendered(&sources, &items)),
    }
}

fn rendered(sources: &SourceMap, items: &[cove_diag::Diagnostic]) -> String {
    items
        .iter()
        .map(|item| cove_diag::render(sources, item))
        .collect::<Vec<_>>()
        .join("\n")
}

/// [`checked`], with `probe` added to the standard-library module `library` as
/// a file of its own.
///
/// For a case that needs a standard-library body the library does not have: a
/// file in a library module is a library file, privileged and blamed as one,
/// by the same questions the checker, the lowering and the oracle each ask.
fn checked_with_probe(source: &str, library: &str, probe: &str) -> (Arc<SourceMap>, Arc<Checked>) {
    let mut sources = SourceMap::new();
    let file = sources.add("m/main.cove", source.to_string());
    let ast = match cove_syntax::parse_file(&sources, file) {
        Ok(ast) => ast,
        Err(items) => panic!("the source parses:\n{}", rendered(&sources, &items)),
    };
    let mut modules = BTreeMap::from([(
        "m".to_string(),
        Module {
            name: "m".to_string(),
            dir: PathBuf::from("m"),
            units: vec![Unit {
                file,
                path: PathBuf::from("m/main.cove"),
                ast,
            }],
        },
    )]);
    for (name, module) in cove_sema::stdlib::attach(&mut sources).expect("stdlib parses") {
        modules.insert(name, module);
    }
    let path = PathBuf::from("std/probe.cove");
    let file = sources.add_library(path.clone(), probe);
    let ast = match cove_syntax::parse_file(&sources, file) {
        Ok(ast) => ast,
        Err(items) => panic!("the probe parses:\n{}", rendered(&sources, &items)),
    };
    modules
        .get_mut(library)
        .unwrap_or_else(|| panic!("the standard library has `{library}`"))
        .units
        .push(Unit { file, path, ast });
    let package = Package {
        root: PathBuf::from("."),
        config: Config::default(),
        modules,
    };
    match cove_sema::Compiler::new().compile(&package) {
        Ok(program) => (Arc::new(sources), Arc::new(program)),
        Err(items) => panic!("the probe checks:\n{}", rendered(&sources, &items)),
    }
}

/// What one backend answered, reduced to what both can be asked for.
///
/// The message rather than the whole [`crate::RuntimeError`], because the two
/// backends legitimately differ in the span they attach to a fault today and
/// the message is the part the language decides. Spans join the comparison
/// when the lowering carries them through every construct.
#[derive(Debug, PartialEq)]
enum Answer {
    Value(String),
    Failed(String),
}

/// Runs `m.<name>` on the interpreter.
fn on_the_oracle(source: &str, name: &str, args: Vec<Value>) -> Answer {
    let (sources, program) = checked(source);
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(program.clone(), sources, hosts.clone());
    said(Interpreter::new(&runtime).invoke("m", name, args))
}

/// Runs `m.<name>` on the linear-memory machine.
///
/// Through [`Vm`] rather than through [`crate::vm::exec::Machine`], because
/// the question this file asks is about the language and the language's
/// answer includes the boundary: the same argument check, the same
/// materialisation, the same terminal event. A comparison that skipped them
/// would be comparing the loop against the whole of the oracle.
fn on_the_machine(source: &str, name: &str, args: Vec<Value>) -> Answer {
    let (sources, checked) = checked(source);
    let program = lowered(&sources, &checked);
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(checked.clone(), sources, hosts.clone());
    said(Vm::new(&runtime, &hosts, &program).invoke("m", name, args))
}

/// [`on_the_machine`], but the run's heap is bounded to `heap_words` rather
/// than [`super::DEFAULT_HEAP_WORDS`], and the answer comes back with how
/// many collections the run actually did.
///
/// Issue #242 is why this exists beside `on_the_machine` rather than as a
/// parameter on it: the collector's unit tests build their programs with
/// `mem`'s own `Build` helper, so they establish that the collector is
/// correct given a correct set of roots but never that *lowering* produced
/// one. Nothing here checks that directly either — what it does is run a
/// program the lowering actually produced, over a heap small enough that a
/// collection is not a possibility this run happens to avoid, and hold the
/// answer to the oracle, which has no collector to agree or disagree with.
/// The collection count is what tells a caller the small heap did its job
/// rather than merely being unused.
fn on_the_machine_with_heap_words(
    source: &str,
    name: &str,
    args: Vec<Value>,
    heap_words: usize,
) -> (Answer, u64) {
    let (sources, checked) = checked(source);
    let program = lowered(&sources, &checked);
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(checked.clone(), sources, hosts.clone());
    let mut vm = Vm::with_heap_words(&runtime, &hosts, &program, heap_words);
    let answer = said(vm.invoke("m", name, args));
    (answer, vm.collections())
}

/// Runs `m.<name>` as an entry on the interpreter, with no process
/// arguments.
fn entry_on_the_oracle(source: &str, name: &str) -> Answer {
    let (sources, program) = checked(source);
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(program.clone(), sources, hosts.clone());
    said(Interpreter::new(&runtime).run_entry("m", name, Vec::new()))
}

/// Runs `m.<name>` as an entry on the linear-memory machine.
fn entry_on_the_machine(source: &str, name: &str) -> Answer {
    let (sources, checked) = checked(source);
    let program = lowered(&sources, &checked);
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(checked.clone(), sources, hosts.clone());
    said(Vm::new(&runtime, &hosts, &program).run_entry("m", name, Vec::new()))
}

fn lowered(sources: &SourceMap, checked: &Checked) -> cove_ir::Program {
    match cove_ir::lower(checked, sources, &cove_schema::HostSchemas::new()) {
        Ok(program) => program,
        Err(items) => panic!(
            "the program lowers:\n{}",
            items
                .iter()
                .map(|item| item.message.clone())
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

/// What a backend said, in the form the two can be compared in.
fn said(outcome: Result<Value, crate::error::RuntimeError>) -> Answer {
    match outcome {
        Ok(value) => Answer::Value(format!("{value}")),
        Err(error) => Answer::Failed(error.message),
    }
}

/// Runs `f` on a thread with the stack the interpreter documents.
///
/// The oracle is a tree walk, so its depth is the native stack's. The machine
/// is not, and does not need this — but both go through it, because a case
/// that only ran one of them on a large stack would be comparing two runs
/// under different conditions and calling the difference a backend fault.
///
/// [`Answer`] is `String`s, which is what makes this possible: a [`Value`] is
/// `Rc`-based and cannot leave the thread that built it, so the comparison is
/// of what each backend *said* rather than of what it holds.
///
/// Generic over what `f` answers rather than fixed to [`Answer`]: the GC
/// cases below also want `Vm::collections()` back, a plain `u64` that
/// crosses a thread boundary for free, so it travels out beside the answer
/// instead of through a second call that would need its own machine.
fn on_a_deep_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(crate::interp::STACK_SIZE)
        .spawn(f)
        .expect("a thread for the run")
        .join()
        .expect("the run did not panic")
}

/// Asserts that the two backends answer `m.<name>` the same way.
#[track_caller]
fn agree(source: &str, name: &str, args: Vec<Value>) -> Answer {
    // The arguments are rebuilt on each thread rather than sent, for the
    // reason `on_a_deep_stack` gives: an `Rc` does not cross a thread.
    let described = args.iter().map(Described::of).collect::<Vec<_>>();
    let oracle = {
        let (source, name, described) = (source.to_string(), name.to_string(), described.clone());
        on_a_deep_stack(move || {
            on_the_oracle(
                &source,
                &name,
                described.iter().map(Described::value).collect(),
            )
        })
    };
    let machine = {
        let (source, name, described) = (source.to_string(), name.to_string(), described);
        on_a_deep_stack(move || {
            on_the_machine(
                &source,
                &name,
                described.iter().map(Described::value).collect(),
            )
        })
    };
    assert_eq!(
        machine, oracle,
        "the machine and the interpreter do not agree about `{name}`"
    );
    oracle
}

/// [`agree`], but the machine runs over a heap bounded to `heap_words`, and
/// the answer comes back with how many collections it took to produce.
///
/// A caller asserts a floor on the count itself — a test that would pass
/// whether or not a collection occurred proves nothing about the roots the
/// lowering produced, which is the whole reason this file has a GC-stress
/// section at all.
#[track_caller]
fn agree_under_heap_pressure(
    source: &str,
    name: &str,
    args: Vec<Value>,
    heap_words: usize,
) -> (Answer, u64) {
    let described = args.iter().map(Described::of).collect::<Vec<_>>();
    let oracle = {
        let (source, name, described) = (source.to_string(), name.to_string(), described.clone());
        on_a_deep_stack(move || {
            on_the_oracle(
                &source,
                &name,
                described.iter().map(Described::value).collect(),
            )
        })
    };
    let (machine, collections) = {
        let (source, name, described) = (source.to_string(), name.to_string(), described);
        on_a_deep_stack(move || {
            on_the_machine_with_heap_words(
                &source,
                &name,
                described.iter().map(Described::value).collect(),
                heap_words,
            )
        })
    };
    assert_eq!(
        machine, oracle,
        "the machine and the interpreter do not agree about `{name}` under heap pressure"
    );
    (oracle, collections)
}

/// Asserts that the two backends answer the entry `m.<name>` the same way.
///
/// The other way in, and it is a different question: [`agree`] compares what
/// a *host* gets when it invokes a declaration, and this compares what a
/// *command* gets when it runs an entry. The entry-shape rule — no
/// parameters, or one `Array<String>` — is the language's, so the two must
/// refuse the same shapes in the same words.
#[track_caller]
fn entry_agrees(source: &str, name: &str) -> Answer {
    let oracle = {
        let (source, name) = (source.to_string(), name.to_string());
        on_a_deep_stack(move || entry_on_the_oracle(&source, &name))
    };
    let machine = {
        let (source, name) = (source.to_string(), name.to_string());
        on_a_deep_stack(move || entry_on_the_machine(&source, &name))
    };
    assert_eq!(
        machine, oracle,
        "the machine and the interpreter do not agree about the entry `{name}`"
    );
    oracle
}

/// An argument, in a form that can cross a thread.
///
/// A [`Value`] cannot: it is `Rc`-based on purpose, because a Cove value is
/// reachable from one task at a time. The cases here pass scalars, so
/// describing one and rebuilding it on the far side costs nothing and keeps
/// the two runs from having to share anything.
#[derive(Clone)]
enum Described {
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl Described {
    fn of(value: &Value) -> Described {
        if let Some(n) = value.as_int() {
            Described::Int(n)
        } else if let Some(x) = value.as_float() {
            Described::Float(x)
        } else if let Some(b) = value.as_bool() {
            Described::Bool(b)
        } else {
            panic!("this fixture passes scalars")
        }
    }

    fn value(&self) -> Value {
        match self {
            Described::Int(n) => Value::int(*n),
            Described::Float(x) => Value::float(*x),
            Described::Bool(b) => Value::bool(*b),
        }
    }
}

#[test]
fn arithmetic_agrees() {
    let source = "
export fn f(a: Int, b: Int) -> Int {
  (a + b) * (a - b) / 2
}
";
    assert_eq!(
        agree(source, "f", vec![Value::int(9), Value::int(4)]),
        Answer::Value("32".to_string())
    );
}

#[test]
fn a_fault_agrees_word_for_word() {
    let source = "
export fn f(a: Int, b: Int) -> Int {
  a / b
}
";
    assert_eq!(
        agree(source, "f", vec![Value::int(1), Value::int(0)]),
        Answer::Failed("`Int` division by zero".to_string())
    );
}

#[test]
fn an_overflow_agrees() {
    let source = "
export fn f(a: Int) -> Int {
  a + 1
}
";
    assert_eq!(
        agree(source, "f", vec![Value::int(i64::MAX)]),
        Answer::Failed("`Int` addition overflowed".to_string())
    );
}

#[test]
fn short_circuiting_agrees() {
    // The right-hand side divides by zero, so the answer says whether it was
    // evaluated. Both backends must decline to.
    let source = "
export fn f(n: Int) -> Bool {
  n == 0 || 10 / n > 0
}
";
    assert_eq!(
        agree(source, "f", vec![Value::int(0)]),
        Answer::Value("true".to_string())
    );
    assert_eq!(
        agree(source, "f", vec![Value::int(5)]),
        Answer::Value("true".to_string())
    );
}

#[test]
fn a_conditional_agrees() {
    let source = "
export fn f(n: Int) -> Int {
  if n < 0 {
    0 - n
  } else {
    n
  }
}
";
    for n in [-7, 0, 7] {
        agree(source, "f", vec![Value::int(n)]);
    }
}

#[test]
fn a_loop_agrees() {
    let source = "
export fn f(n: Int) -> Int {
  var total = 0
  var i = 0
  while i < n {
    total = total + i
    i = i + 1
  }
  total
}
";
    assert_eq!(
        agree(source, "f", vec![Value::int(10)]),
        Answer::Value("45".to_string())
    );
}

#[test]
fn break_and_continue_agree() {
    let source = "
export fn f(n: Int) -> Int {
  var total = 0
  var i = 0
  while true {
    i = i + 1
    if i > n {
      break
    }
    if i % 2 == 0 {
      continue
    }
    total = total + i
  }
  total
}
";
    assert_eq!(
        agree(source, "f", vec![Value::int(10)]),
        Answer::Value("25".to_string())
    );
}

#[test]
fn recursion_agrees() {
    let source = "
export fn fib(n: Int) -> Int {
  if n < 2 {
    n
  } else {
    fib(n - 1) + fib(n - 2)
  }
}
";
    assert_eq!(
        agree(source, "fib", vec![Value::int(20)]),
        Answer::Value("6765".to_string())
    );
}

#[test]
fn floats_agree() {
    let source = "
export fn f(x: Float, y: Float) -> Float {
  x * y + x / y
}
";
    agree(source, "f", vec![Value::float(3.5), Value::float(1.25)]);
}

#[test]
fn a_bool_answer_agrees() {
    let source = "
export fn f(a: Int, b: Int) -> Bool {
  !(a >= b) && a != 0
}
";
    for (a, b) in [(1, 2), (2, 1), (0, 1)] {
        agree(source, "f", vec![Value::int(a), Value::int(b)]);
    }
}

#[test]
fn a_unit_answer_agrees() {
    let source = "
export fn f(n: Int) {
  var seen = n
  seen = seen + 1
}
";
    assert_eq!(
        agree(source, "f", vec![Value::int(1)]),
        Answer::Value("()".to_string())
    );
}

/// A call chain deep enough that the machine's frames are a stack rather than
/// a special case, and shallow enough that the oracle's own depth limit is
/// not what is being measured.
#[test]
fn nested_calls_agree() {
    let source = "
fn a(n: Int) -> Int { b(n) + 1 }
fn b(n: Int) -> Int { c(n) * 2 }
fn c(n: Int) -> Int { n - 3 }
export fn f(n: Int) -> Int { a(n) + b(n) + c(n) }
";
    assert_eq!(
        agree(source, "f", vec![Value::int(10)]),
        // c(10) = 7, b(10) = 14, a(10) = 15.
        Answer::Value("36".to_string())
    );
}

/// The way a command speaks to a program, on both backends.
#[test]
fn an_entry_that_takes_no_arguments_agrees() {
    let source = "
export fn main() -> Int {
  var total = 0
  var i = 0
  while i < 5 {
    total = total + i * i
    i = i + 1
  }
  total
}
";
    assert_eq!(
        entry_agrees(source, "main"),
        Answer::Value("30".to_string())
    );
}

/// The entry-shape rule is the language's, so both refuse in the same words.
#[test]
fn an_entry_of_the_wrong_shape_is_refused_the_same_way() {
    let source = "
export fn main(a: Int, b: Int) -> Int { a + b }
";
    assert_eq!(
        entry_agrees(source, "main"),
        Answer::Failed("entry `m.main` declares 2 parameters".to_string())
    );
}

/// A declaration the package does not have, asked for both ways.
#[test]
fn a_name_the_package_does_not_declare_is_refused_the_same_way() {
    let source = "
export fn f(n: Int) -> Int { n }
";
    assert_eq!(
        entry_agrees(source, "g"),
        Answer::Failed("this package does not declare `m.g`".to_string())
    );
    assert_eq!(
        agree(source, "g", vec![Value::int(1)]),
        Answer::Failed("this package does not declare `m.g`".to_string())
    );
}

/// An invocation is held to the declaration before anything runs, by the
/// check both backends share.
#[test]
fn an_argument_the_declaration_does_not_admit_is_refused_the_same_way() {
    let source = "
export fn f(n: Int) -> Int { n }
";
    let Answer::Failed(message) = agree(source, "f", vec![Value::float(1.5)]) else {
        panic!("a `Float` is not an `Int`");
    };
    assert!(
        message.contains("Int"),
        "the refusal names the declared type: {message}"
    );
}

#[test]
fn a_string_literal_agrees() {
    let source = r#"
export fn f() -> String {
  "hello"
}
"#;
    assert_eq!(
        agree(source, "f", vec![]),
        Answer::Value("hello".to_string())
    );
}

#[test]
fn interpolation_agrees() {
    let source = r#"
export fn f(n: Int, x: Float, b: Bool) -> String {
  "n={n} x={x} b={b} done"
}
"#;
    agree(
        source,
        "f",
        vec![Value::int(-3), Value::float(2.5), Value::bool(true)],
    );
}

/// An `Error` renders as the message it carries rather than as the struct it
/// happens to be, on both backends. The two say so in two places — the
/// oracle in `Display for Value`, the machine in `vm::intrinsics` — because
/// one reads a materialised tree and the other reads the heap, and this is
/// what keeps the two copies in step.
#[test]
fn an_error_renders_as_its_message() {
    let source = r#"
export fn f() -> String {
  let e = Error("boom")
  "{e}"
}
"#;
    assert_eq!(
        agree(source, "f", vec![]),
        Answer::Value("boom".to_string())
    );
}

#[test]
fn a_struct_field_agrees() {
    let source = "
struct Point { x: Int, y: Int }
export fn f(a: Int, b: Int) -> Int {
  let p = Point(x: a, y: b)
  p.x * p.y
}
";
    assert_eq!(
        agree(source, "f", vec![Value::int(6), Value::int(7)]),
        Answer::Value("42".to_string())
    );
}

#[test]
fn a_struct_renders_the_same_way() {
    let source = r#"
struct Point { x: Int, y: Int }
export fn f() -> String {
  "{Point(x: 1, y: 2)}"
}
"#;
    agree(source, "f", vec![]);
}

#[test]
fn an_option_agrees() {
    let source = "
export fn f(n: Int) -> Int {
  let found = if n > 0 { Some(n * 2) } else { None }
  match found {
    Some(v) => v
    None => -1
  }
}
";
    for n in [-1, 0, 21] {
        agree(source, "f", vec![Value::int(n)]);
    }
}

#[test]
fn a_declared_enum_agrees() {
    let source = r#"
enum Shape {
  Dot
  Line(Int)
  Box(Int, Int)
}
export fn area(n: Int) -> Int {
  let s = if n == 0 { Shape.Dot } else if n == 1 { Shape.Line(4) } else { Shape.Box(3, n) }
  match s {
    Shape.Dot => 0
    Shape.Line(len) => len
    Shape.Box(w, h) => w * h
  }
}
"#;
    for n in [0, 1, 5] {
        agree(source, "area", vec![Value::int(n)]);
    }
}

#[test]
fn a_nested_pattern_agrees() {
    let source = "
export fn f(n: Int) -> Int {
  let v = if n > 0 { Some(Some(n)) } else { Some(None) }
  match v {
    Some(Some(x)) => x
    Some(None) => 0
    None => -1
  }
}
";
    for n in [-1, 3] {
        agree(source, "f", vec![Value::int(n)]);
    }
}

#[test]
fn propagation_agrees() {
    let source = "
fn half(n: Int) -> Result<Int, Error> {
  if n % 2 == 0 {
    Ok(n / 2)
  } else {
    Err(Error(\"odd\"))
  }
}
export fn f(n: Int) -> Result<Int, Error> {
  let a = half(n)?
  let b = half(a)?
  Ok(b)
}
";
    for n in [8, 6, 3] {
        agree(source, "f", vec![Value::int(n)]);
    }
}

/// A `var` parameter names the caller's own binding. `two(var x, var x)`
/// answering 11 rather than 10 is the observable difference, and only
/// aliasing gives that answer.
#[test]
fn a_var_parameter_aliases_the_caller() {
    let source = "
fn bump(var n: Int) {
  n = n + 1
}
export fn f() -> Int {
  var total = 10
  bump(var total)
  total
}
";
    assert_eq!(agree(source, "f", vec![]), Answer::Value("11".to_string()));
}

/// A field of a `var` parameter is a place of its own: it is written in
/// place, and it can be passed on as a `var` argument of its own.
///
/// Both of those needed an instruction that offsets an address. Without one a
/// place could only ever be the *first* word of a value location, so
/// `p.y = 7` through a `var p: Point` had to load both words, write one and
/// store both back — the same answer on one thread, and not what the address
/// was for — and `bump(var p.y)` could not be lowered at all, because there
/// was no way to form the address to pass.
#[test]
fn a_field_of_a_var_parameter_is_written_and_passed_on_in_place() {
    let source = "
struct Point { x: Int, y: Int }

fn bump(var n: Int) {
  n = n + 1
}

fn shift(var p: Point) {
  p.y = 7
  bump(var p.y)
}

export fn f() -> Int {
  var here = Point(x: 1, y: 2)
  shift(var here)
  here.x * 100 + here.y
}
";
    assert_eq!(agree(source, "f", vec![]), Answer::Value("108".to_string()));
}

/// A whole struct crosses into a builtin as an argument: `contains` and
/// `indexOf` compare it against elements of the same width, and `push` and
/// `set` store both of its words.
///
/// All four refused until an argument carried its layout — a call said where
/// the `Point` began and never that it was two words, and the honest answer
/// was to refuse rather than to compare or store the first word of it.
#[test]
fn a_struct_crosses_into_a_sequence_builtin_whole() {
    let source = "
struct Point { x: Int, y: Int }

export fn f() -> Int {
  let items = [Point(x: 1, y: 2), Point(x: 3, y: 4)]
  var found = 0
  if items.contains(Point(x: 3, y: 4)) {
    found = found + 1000
  }
  if items.contains(Point(x: 3, y: 9)) {
    found = found + 2000
  }
  found = found + items.indexOf(Point(x: 3, y: 4)).unwrapOr(-1) * 100
  var v = items.toVector()
  v.push(Point(x: 5, y: 6))
  v.set(0, Point(x: 7, y: 8))
  found + v.length() * 10 + v.get(0).unwrapOr(Point(x: 0, y: 0)).y
}
";
    assert_eq!(
        agree(source, "f", vec![]),
        Answer::Value("1138".to_string())
    );
}

/// A loop that builds a string a turn, in a heap far too small to hold every
/// one of them. It finishes only because the lowering clears the slot each
/// turn, so a turn's string is unreachable by the next.
#[test]
fn a_loop_that_allocates_agrees() {
    let source = r#"
export fn f(n: Int) -> Int {
  var i = 0
  var last = 0
  while i < n {
    let text = "turn {i} of {n}"
    last = i
    i = i + 1
  }
  last
}
"#;
    assert_eq!(
        agree(source, "f", vec![Value::int(500)]),
        Answer::Value("499".to_string())
    );
}

/// ADR 0014's rule, which is about a declaration rather than about a value:
/// an opaque type renders as its name and nothing else, because a rendering
/// is read by whoever the string reaches and its fields are the declaring
/// module's own business.
#[test]
fn an_opaque_struct_renders_as_its_name() {
    let source = r#"
export opaque struct Token { id: Int, secret: String }
export fn f() -> String {
  "{Token(id: 1, secret: "hunter2")}"
}
"#;
    assert_eq!(
        agree(source, "f", vec![]),
        Answer::Value("Token".to_string())
    );
}

#[test]
fn an_array_agrees() {
    let source = "
fn at(xs: Array<Int>, i: Int) -> Int {
  match xs.get(i) {
    Some(v) => v
    None => 0
  }
}
export fn f(n: Int) -> Int {
  let xs = [n, n + 1, n + 2]
  at(xs, 0) + at(xs, 2) + at(xs, 9) + xs.length()
}
";
    assert_eq!(
        agree(source, "f", vec![Value::int(10)]),
        // 10 + 12, an out-of-range `get` answering `None`, and the length.
        Answer::Value("25".to_string())
    );
}

#[test]
fn an_array_renders_the_same_way() {
    let source = r#"
export fn f() -> String {
  "{[1, 2, 3]}"
}
"#;
    agree(source, "f", vec![]);
}

#[test]
fn a_for_over_an_array_agrees() {
    let source = "
export fn f(n: Int) -> Int {
  var total = 0
  for x in [n, n * 2, n * 3] {
    total = total + x
  }
  total
}
";
    assert_eq!(
        agree(source, "f", vec![Value::int(4)]),
        Answer::Value("24".to_string())
    );
}

#[test]
fn a_for_over_a_range_agrees() {
    let source = "
export fn f(n: Int) -> Int {
  var total = 0
  for i in 0..<n {
    total = total + i
  }
  for i in 0..n {
    total = total + i
  }
  total
}
";
    for n in [0, 1, 10] {
        agree(source, "f", vec![Value::int(n)]);
    }
}

/// An empty or reversed range iterates zero times, and both backends have to
/// agree that it does rather than each having its own answer for it.
#[test]
fn an_empty_range_agrees() {
    let source = "
export fn f(n: Int) -> Int {
  var turns = 0
  for i in n..<0 {
    turns = turns + 1
  }
  turns
}
";
    for n in [3, 0, -2] {
        agree(source, "f", vec![Value::int(n)]);
    }
}

/// A `continue` on the last turn of a loop used to leave the element binding
/// holding its object for the rest of the frame. The lowering clears it on
/// every way out of a turn, and this walks a large array under a heap that
/// cannot hold it all.
#[test]
fn a_loop_that_skips_still_releases_its_element() {
    let source = r#"
export fn f(n: Int) -> Int {
  var kept = 0
  var i = 0
  while i < n {
    let text = "element {i}"
    if i % 3 == 0 {
      i = i + 1
      continue
    }
    kept = kept + 1
    i = i + 1
  }
  kept
}
"#;
    assert_eq!(
        agree(source, "f", vec![Value::int(300)]),
        Answer::Value("200".to_string())
    );
}

/// A `Vector` and a `Range` leaving a program are the two families the
/// boundary has to read out of more than one word: a vector's length is its
/// header's and not its store's, and a range is three words in the heap and a
/// range to a reader. Answering either as the representation it has — the
/// spare room included, or `Range(start: 0, end: 3, inclusive: false)` — would
/// be a different answer from the oracle's, which is what this asks about.
#[test]
fn a_compound_answer_agrees() {
    let source = "
export fn ints() -> Vector<Int> {
  var v = Vector.of(1)
  v.push(2)
  v.push(3)
  v
}
export fn exclusive() -> Range { 0..<3 }
export fn inclusive() -> Range { 0..3 }
";
    assert_eq!(
        agree(source, "ints", vec![]),
        Answer::Value("[1, 2, 3]".to_string())
    );
    assert_eq!(
        agree(source, "exclusive", vec![]),
        Answer::Value("0..<3".to_string())
    );
    assert_eq!(
        agree(source, "inclusive", vec![]),
        Answer::Value("0..3".to_string())
    );
}

#[test]
fn a_vector_agrees() {
    let source = "
export fn f(n: Int) -> Int {
  var v = Vector.of(n)
  v.push(n + 1)
  v.push(n + 2)
  var total = 0
  for x in v {
    total = total + x
  }
  total + v.length()
}
";
    assert_eq!(
        agree(source, "f", vec![Value::int(1)]),
        Answer::Value("9".to_string())
    );
}

/// A `Vector` shares storage that can be mutated, so a copy of one is an
/// alias and mutation through either is visible through the other. That is
/// the opposite of a struct's rule, and it is what `is` asks about.
#[test]
fn a_vector_copy_is_an_alias() {
    let source = "
export fn f() -> Int {
  var a = Vector.of(1)
  var b = a
  b.push(2)
  a.length()
}
";
    assert_eq!(agree(source, "f", vec![]), Answer::Value("2".to_string()));
}

#[test]
fn identity_agrees() {
    let source = "
export fn f() -> Bool {
  let a = Vector.of(1)
  let b = a
  let c = Vector.of(1)
  (a is b) && !(a is c)
}
";
    assert_eq!(
        agree(source, "f", vec![]),
        Answer::Value("true".to_string())
    );
}

#[test]
fn structural_equality_agrees() {
    let source = r#"
struct P { x: Int, y: String }
export fn f() -> Bool {
  let a = P(x: 1, y: "one")
  let b = P(x: 1, y: "one")
  let c = P(x: 2, y: "one")
  (a == b) && (a != c) && ([1, 2] == [1, 2]) && ([1, 2] != [1, 3])
}
"#;
    assert_eq!(
        agree(source, "f", vec![]),
        Answer::Value("true".to_string())
    );
}

#[test]
fn enum_equality_agrees() {
    let source = "
export fn f(n: Int) -> Bool {
  let a = if n > 0 { Some(n) } else { None }
  let b = Some(1)
  a == b
}
";
    for n in [0, 1, 2] {
        agree(source, "f", vec![Value::int(n)]);
    }
}

/// A `Range` renders as the operator it was written with. `1..3` and `1..<4`
/// cover the same values and are two renderings, because `==` on ranges
/// compares the bounds a program wrote rather than the set they describe.
#[test]
fn a_range_renders_as_it_was_written() {
    let source = r#"
export fn f(n: Int) -> String {
  "{0..<n} and {0..n}"
}
"#;
    for n in [0, 3] {
        agree(source, "f", vec![Value::int(n)]);
    }
}

#[test]
fn a_string_method_agrees() {
    let source = r#"
export fn f(s: String) -> String {
  "{s.length()} {s.toUpper()} {s.trim()} {s.contains("b")} {s.replace("b", "z")}"
}
"#;
    for s in ["  abc  ", "", "aβc"] {
        let source = source.to_string();
        let value = Value::string(s);
        let described = value.as_str().map(|t| t.to_string()).expect("a string");
        let oracle = {
            let (source, described) = (source.clone(), described.clone());
            on_a_deep_stack(move || on_the_oracle(&source, "f", vec![Value::string(described)]))
        };
        let machine = {
            let (source, described) = (source.clone(), described);
            on_a_deep_stack(move || on_the_machine(&source, "f", vec![Value::string(described)]))
        };
        assert_eq!(machine, oracle, "the machine and the interpreter disagree");
    }
}

/// **`String.indexOf` answers a character position, against `str::find` and
/// `chars().count()` over a corpus.**
///
/// [ADR 0064](../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)
/// moved the search into `std.string.indexOf`, which is one `core.stringFind`
/// — ADR 0065's byte run search — and then a walk of the text in front of what
/// it found. The instruction answers a **byte** offset; the method answers a
/// **character** position. That conversion is the whole of what this body adds
/// over `std.string.contains`, and it is the one thing the two evaluators
/// cannot catch between them: both run the same Cove, so both are wrong
/// together. **So the oracle here is Rust's**, exactly as
/// `tests/e2e/values_string_index_of` reasoned its lines from the encoding and
/// then checked them against it — `s.find(n).map(|b| s[..b].chars().count())`,
/// the two halves written out.
///
/// The corpus is every character-boundary substring of each haystack, which is
/// a needle at every position and of every length including the empty one,
/// plus a fixed set of needles from other alphabets that are mostly
/// near-misses: `z` in a Greek haystack, `é` in a Japanese one, a lone U+0082
/// whose single byte `あ` ends with. The haystacks cross the widths on purpose
/// and several of them *begin* with a multi-byte character, because a prefix
/// of one is exactly what makes the byte offset and the character position
/// two different numbers.
///
/// **That difference is counted rather than assumed.** A corpus that drifted
/// to ASCII would agree with a body that forwarded the instruction's byte
/// offset unchanged, and would say nothing at all; the assertion at the end is
/// how many pairs actually disagree about the two numbers, and it is in the
/// hundreds.
///
/// One compiled program for the whole corpus rather than one per pair: the
/// pairs go in as an `Array<String>` and the answers come back as one string,
/// because compiling the standard library several hundred times is the only
/// expensive part of asking this.
#[test]
fn an_index_is_a_character_position_against_rusts_own() {
    /// The oracle: the byte offset `str::find` answers, converted to the
    /// character position `String.length` counts in.
    fn oracle(haystack: &str, needle: &str) -> String {
        match haystack.find(needle) {
            Some(byte) => format!("Some({})", haystack[..byte].chars().count()),
            None => "None".to_string(),
        }
    }

    // Widths one through four, alone and mixed, with several haystacks whose
    // first character is not ASCII. None of them holds a quote or a brace, so
    // nothing below has to be escaped into a Cove literal.
    const HAYSTACKS: &[&str] = &[
        "",
        "a",
        "abc",
        "aaab",
        "banana",
        "abcabcabd",
        "é",
        "éa",
        "aé",
        "ééé",
        "é😀abc",
        "abcé😀",
        "αβγδε",
        "ααββγγ",
        "日本語テスト",
        "あいうあいう",
        "héllo wörld",
        "😀😁😂",
        "😀a😀a😀",
        "aéb😀cあd",
        "xyé😀zw",
        "é😀あいabc",
        // Long multi-byte prefixes in front of ASCII tails, which is where the
        // two numbers are furthest apart: every substring of the tail is found
        // at a byte offset several times its character position.
        "ééééabcd",
        "😀😀😀abcd",
        "あいうえおabc",
        "αβγδεabcd",
        "日本語abcde",
        "😀é日αabcd",
    ];

    // Needles from elsewhere, so that a miss is asked for as often as a hit.
    // U+0082 is `C2 82` and `あ` is `E3 81 82`: the two share their last byte
    // and nothing else, which is the miss a search of single bytes would get
    // wrong.
    let control = char::from_u32(130).expect("a code point").to_string();
    let foreign: Vec<String> = ["z", "zz", "é", "è", "😀", "😁", "あ", "ア", "γ", "ab", "ba"]
        .iter()
        .map(|each| each.to_string())
        .chain([control])
        .collect();

    let mut pairs: Vec<(String, String)> = Vec::new();
    for haystack in HAYSTACKS {
        let bounds: Vec<usize> = haystack
            .char_indices()
            .map(|(at, _)| at)
            .chain([haystack.len()])
            .collect();
        for (at, from) in bounds.iter().enumerate() {
            for to in &bounds[at..] {
                pairs.push((haystack.to_string(), haystack[*from..*to].to_string()));
            }
        }
        for needle in &foreign {
            pairs.push((haystack.to_string(), needle.clone()));
        }
    }
    assert!(pairs.len() > 900, "{} pairs is not a corpus", pairs.len());

    let source = r#"
export fn f(pairs: Array<String>) -> String {
  var out = ""
  var i = 0
  while i + 1 < pairs.length() {
    let haystack = pairs.get(i).unwrapOr("")
    let needle = pairs.get(i + 1).unwrapOr("")
    out = "{out} {haystack.indexOf(needle)}"
    i = i + 2
  }
  out
}
"#;
    let flattened: Vec<String> = pairs
        .iter()
        .flat_map(|(haystack, needle)| [haystack.clone(), needle.clone()])
        .collect();
    let want: String = pairs
        .iter()
        .map(|(haystack, needle)| format!(" {}", oracle(haystack, needle)))
        .collect();

    // Both backends, and both against Rust. The two are asserted against each
    // other as every case here does; the equality after it is what neither of
    // them could supply. `agree` itself is not used because it passes scalars
    // — an `Rc`-based `Value` does not cross a thread — so the array is
    // rebuilt on each side out of the `Vec<String>`, which does, exactly as
    // `a_string_method_agrees` rebuilds its receiver.
    let built = |held: Vec<String>| vec![Value::array(held.into_iter().map(Value::string))];
    let oracle_says = {
        let (source, held) = (source.to_string(), flattened.clone());
        on_a_deep_stack(move || on_the_oracle(&source, "f", built(held)))
    };
    let machine_says = {
        let (source, held) = (source.to_string(), flattened.clone());
        on_a_deep_stack(move || on_the_machine(&source, "f", built(held)))
    };
    assert_eq!(
        machine_says, oracle_says,
        "the machine and the interpreter do not agree about `indexOf`"
    );
    assert_eq!(
        machine_says,
        Answer::Value(want),
        "an answer disagrees with `str::find` and `chars().count()`"
    );

    // A corpus that had drifted to ASCII would pass everything above against a
    // body that answered the instruction's byte offset unchanged. This is how
    // many of the pairs the two numbers are actually different for.
    let differing = pairs
        .iter()
        .filter(|(haystack, needle)| match haystack.find(needle.as_str()) {
            Some(byte) => haystack[..byte].chars().count() != byte,
            None => false,
        })
        .count();
    assert!(
        differing > 200,
        "only {differing} pair(s) have a byte offset that is not the character \
         position, which is too few to be testing the conversion"
    );
}

/// **`String.indexOf` lowers to one run search and a walk, and to no
/// `IntrinsicCall` at all.**
///
/// This is the structural half of the migration, asserted rather than
/// eyeballed. [ADR 0064](../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
/// Decision 8 says the variant dies in the stage that migrates its last
/// producer, and a body that still reached a runtime arm would pass every
/// semantic case above while migrating nothing.
///
/// Three claims, and each would fail differently:
///
/// - **no `Inst::IntrinsicCall` anywhere in the lowered program**, and so no
///   `IntrinsicSite` either — the static count `--boundary` reports for
///   `String.indexOf` is gone because there is nothing to count;
/// - **exactly one `Inst::RunFind`**, which is the `core.stringFind`
///   ADR 0065 already added, expanded at the one call site. One and not two:
///   a body that searched again to count would be a second search;
/// - **no run instruction but that search and the byte loads the walk makes.**
///   The migration adds no primitive: `RunFind` for the search, `RunLoad`
///   over `Storage::PackedBytes` for `byteAt`, and `Len` for `byteLength`,
///   which are what `std.string.contains` and `std.string.length` were
///   already built from. A `RunSlice` here would be the temporary `String`
///   the walk exists not to build.
#[test]
fn an_index_of_lowers_to_a_run_find_and_a_walk_and_no_builtin_call() {
    let source = r#"
export fn f(s: String, needle: String) -> Option<Int> {
  s.indexOf(needle)
}
"#;
    let (sources, checked) = checked(source);
    // From the one entry, as `cove run` lowers it, so that what is walked
    // below is this program's own reachable code and not every standard-library
    // body in the package.
    let program = cove_ir::lower_entry(
        &checked,
        &sources,
        &cove_schema::HostSchemas::new(),
        "m",
        "f",
    )
    .expect("the program lowers");

    for function in &program.functions {
        assert!(
            function
                .code
                .iter()
                .all(|inst| !matches!(inst, cove_ir::Inst::IntrinsicCall { .. })),
            "`{}.{}` makes a builtin call: {:?}",
            function.module,
            function.name,
            function.code
        );
    }
    assert!(
        program.intrinsic_sites.is_empty(),
        "a program whose only builtin is `indexOf` names {} intrinsic site(s)",
        program.intrinsic_sites.len()
    );

    let mut finds = 0;
    let mut loads = 0;
    let mut lengths = 0;
    let mut other: Vec<String> = Vec::new();
    for function in &program.functions {
        for inst in &function.code {
            match inst {
                cove_ir::Inst::RunFind { storage, .. } => {
                    assert_eq!(*storage, cove_ir::Storage::PackedBytes, "{inst:?}");
                    finds += 1;
                }
                cove_ir::Inst::RunLoad { storage, .. } => {
                    assert_eq!(*storage, cove_ir::Storage::PackedBytes, "{inst:?}");
                    loads += 1;
                }
                cove_ir::Inst::Len { .. } => lengths += 1,
                cove_ir::Inst::RunSlice { .. }
                | cove_ir::Inst::RunCopy { .. }
                | cove_ir::Inst::RunStore { .. }
                | cove_ir::Inst::RunFinish { .. } => other.push(format!("{inst:?}")),
                _ => {}
            }
        }
    }
    assert_eq!(finds, 1, "one run search and not two");
    assert_eq!(loads, 1, "one byte load, which is the walk's `byteAt`");
    assert_eq!(lengths, 0, "the walk is bounded by the search's own answer");
    assert!(
        other.is_empty(),
        "`indexOf` lowers to a run instruction it has no business in: {other:?}"
    );
}

#[test]
fn a_scalar_method_agrees() {
    let source = r#"
export fn f(n: Int, x: Float) -> String {
  "{n.abs()} {n.min(3)} {n.toFloat()} {x.round()} {x.format(2)}"
}
"#;
    for (n, x) in [(-5, 1.25_f64), (7, -0.5)] {
        agree(source, "f", vec![Value::int(n), Value::float(x)]);
    }
}

#[test]
fn a_parse_agrees_both_ways() {
    let source = r#"
export fn f(s: String) -> String {
  match Int.parse(s) {
    Ok(n) => "ok {n}"
    Err(e) => "err {e}"
  }
}
"#;
    for text in ["42", "no"] {
        let source = source.to_string();
        let text = text.to_string();
        let oracle = {
            let (source, text) = (source.clone(), text.clone());
            on_a_deep_stack(move || on_the_oracle(&source, "f", vec![Value::string(text)]))
        };
        let machine = {
            let (source, text) = (source.clone(), text);
            on_a_deep_stack(move || on_the_machine(&source, "f", vec![Value::string(text)]))
        };
        assert_eq!(machine, oracle, "the machine and the interpreter disagree");
    }
}

#[test]
fn a_duration_reads_and_builds_the_same_way() {
    let source = r#"
export fn f() -> String {
  let d = 1500ms
  "{d.millis()} {d.seconds()} {Duration.seconds(2)} {Duration.millis(1500)}"
}
"#;
    agree(source, "f", vec![]);
}

#[test]
fn an_option_method_agrees() {
    let source = "
export fn f(n: Int) -> Int {
  let v = if n > 0 { Some(n) } else { None }
  var total = 0
  if v.isSome() { total = total + 1 }
  if v.isNone() { total = total + 10 }
  total + v.unwrapOr(100)
}
";
    for n in [0, 5] {
        agree(source, "f", vec![Value::int(n)]);
    }
}

/// A layout is an identity, not a shape. Two modules each declaring a
/// same-shaped `Point` are two types, and a dispatch that treated them as one
/// would reach the wrong conformance. The name a layout carries is qualified
/// for that reason, and a rendering shortens it — which is what the oracle
/// does with the same string.
#[test]
fn two_modules_may_each_declare_a_point() {
    let source = r#"
struct Point { x: Int }
export fn f() -> String {
  "{Point(x: 1)}"
}
"#;
    assert_eq!(
        agree(source, "f", vec![]),
        Answer::Value("Point(x: 1)".to_string())
    );
}

#[test]
fn a_method_agrees() {
    let source = "
struct Counter { n: Int }
impl Counter {
  fn doubled(self) -> Int {
    self.n * 2
  }
  fn make(n: Int) -> Counter {
    Counter(n: n)
  }
}
export fn f(n: Int) -> Int {
  Counter.make(n).doubled()
}
";
    assert_eq!(
        agree(source, "f", vec![Value::int(21)]),
        Answer::Value("42".to_string())
    );
}

#[test]
fn dynamic_dispatch_agrees() {
    let source = r#"
trait Describe {
  fn describe(self) -> String
}
struct Dot { n: Int }
struct Tag { name: String }
impl Describe for Dot {
  fn describe(self) -> String { "dot {self.n}" }
}
impl Describe for Tag {
  fn describe(self) -> String { "tag {self.name}" }
}
fn show(it: dyn Describe) -> String {
  it.describe()
}
export fn f(pick: Bool) -> String {
  if pick { show(Dot(n: 3)) } else { show(Tag(name: "x")) }
}
"#;
    for pick in [true, false] {
        agree(source, "f", vec![Value::bool(pick)]);
    }
}

// `Set` and `Map` cases wait on the lowering. The machine implements both —
// construction, lookup, the immutable updates and the ordering — and nothing
// emits the calls yet, so a case here would fail on the gap rather than on a
// disagreement and would say nothing about either half.

/// `"a" < "b"` compares bytes. It used to compare *nothing*: an ordering
/// operator on a value the instruction set cannot compare in one step was
/// routed to the structural-equality walk, which answers whether two values
/// are the same, so `<` came out as `!=` and `sorted` over strings returned
/// its input reversed. A wrong answer is worse than a gap.
#[test]
fn strings_order_by_their_bytes() {
    let source = r#"
export fn f() -> String {
  let strings = ["pear", "apple", "fig"]
  "{strings.sorted(by: fn(a, b) { a < b })} {"a" < "b"} {"b" <= "a"} {"a" >= "a"}"
}
"#;
    assert_eq!(
        agree(source, "f", vec![]),
        Answer::Value("[apple, fig, pear] true false true".to_string())
    );
}

// ---- `Shared` -------------------------------------------------------------

/// A `lock` whose closure wrote `var` mutates the value where it lies.
#[test]
fn a_lock_that_aliases_agrees() {
    let source = r#"
struct Metrics { requests: Int, failures: Int }
impl Metrics {
  fn record(var self, failed: Bool) {
    self.requests += 1
    if failed { self.failures += 1 }
  }
}
export fn f() -> String {
  let metrics = Shared(Metrics(requests: 0, failures: 0))
  metrics.lock(fn(var value) { value.record(true) })
  metrics.lock(fn(var value) { value.record(false) })
  metrics.lock(fn(value) { "{value.requests} {value.failures}" })
}
"#;
    assert_eq!(agree(source, "f", vec![]), Answer::Value("2 1".to_string()));
}

/// A `lock` whose closure did not write `var` is handed a copy, and what it
/// does to the copy is not stored back.
///
/// `Interpreter::call_shared_method` reads the same question off the same
/// place — the written lambda's first parameter — so the two backends have to
/// answer it the same way, and this is where that is asked.
#[test]
fn a_lock_that_copies_agrees() {
    let source = r#"
struct Counter { n: Int }
export fn f() -> Int {
  let cell = Shared(Counter(n: 1))
  cell.lock(fn(value) { value })
  cell.lock(fn(value) { value.n })
}
"#;
    assert_eq!(agree(source, "f", vec![]), Answer::Value("1".to_string()));
}

/// A cell wrapping a scalar, and a `lock` that answers a value of its own.
#[test]
fn a_lock_answering_a_value_agrees() {
    let source = r#"
export fn f() -> Int {
  let cell = Shared(1)
  cell.lock(fn(var value) { value = value + 41 })
  cell.lock(fn(value) { value })
}
"#;
    assert_eq!(agree(source, "f", vec![]), Answer::Value("42".to_string()));
}

/// A cell inside a cell nests, which is why the reentrancy refusal is per
/// cell rather than per task.
#[test]
fn two_cells_nest_and_agree() {
    let source = r#"
export fn f() -> Int {
  let outer = Shared(1)
  let inner = Shared(2)
  outer.lock(fn(var a) {
    inner.lock(fn(var b) {
      b = b + a
    })
    a = 10
  })
  outer.lock(fn(a) { a }) + inner.lock(fn(b) { b })
}
"#;
    assert_eq!(agree(source, "f", vec![]), Answer::Value("13".to_string()));
}

/// A task that asks for a cell it is already inside is refused, in the
/// oracle's words.
///
/// [ADR 0037](../../../../docs/adr/0037-a-cycle-through-a-cell-is-an-ordinary-cycle.md)
/// kept this rule and said why: a cycle in the heap is the collector's, and a
/// live lock state is nobody's. So it is one of the two questions `lock` used
/// to answer together, and the only one still answered here.
#[test]
fn a_reentrant_lock_is_refused_in_the_same_words() {
    let source = r#"
export fn f() -> Int {
  let cell = Shared(1)
  cell.lock(fn(var a) {
    cell.lock(fn(var b) { b = b + 1 })
    a
  })
}
"#;
    assert_eq!(
        agree(source, "f", vec![]),
        Answer::Failed(
            "this task already holds this `Shared`, so `lock` would wait for itself".to_string()
        )
    );
}

/// A cell that comes to hold a handle to itself runs, and is an ordinary
/// object-graph cycle.
///
/// This is the one case here that is **not** `agree`, and the reason is
/// [ADR 0037](../../../../docs/adr/0037-a-cycle-through-a-cell-is-an-ordinary-cycle.md).
/// The ADR replaced ADR 0011's amendment, which had `lock` refuse the one
/// cycle it could see; the oracle and the frozen predecessor still make that
/// refusal and keep it until they are deleted, and this backend does not. So
/// asking the two to agree would be asking the machine to reconstruct a walk
/// the ADR says not to reconstruct.
///
/// What is reclaimed and when is `cove_runtime::vm::cell`'s to show, because
/// it can run the collection; what this shows is that the program *runs*,
/// which is the half a source-level test can see.
#[test]
fn a_cell_may_come_to_hold_itself() {
    let source = r#"
struct Node { cell: Option<Shared<Node>>, n: Int }
export fn f() -> Int {
  let n = Shared(Node(cell: None, n: 7))
  n.lock(fn(var value) {
    value = Node(cell: Some(n), n: 8)
  })
  n.lock(fn(value) { value.n })
}
"#;
    assert_eq!(
        on_a_deep_stack(move || on_the_machine(source, "f", vec![])),
        Answer::Value("8".to_string())
    );
}

// ---- `async fn` -----------------------------------------------------------

/// An `async fn` is called like any other function, and `await` is what
/// reads its value.
#[test]
fn an_async_call_and_its_await_agree() {
    let source = r#"
async fn answer() -> Int { 7 }
export fn f() -> Int {
  await answer()
}
"#;
    assert_eq!(agree(source, "f", vec![]), Answer::Value("7".to_string()));
}

/// The body runs **at the call**, so a call nobody awaited has still run.
///
/// This is the sentence `crate::task::Task::settled` is written around, and
/// it is the one an implementation could most easily get wrong in the
/// direction of laziness: nothing here reads the handle, and the effect has
/// happened all the same.
#[test]
fn an_async_call_that_is_never_awaited_has_still_run() {
    let source = r#"
async fn bump(cell: Shared<Int>) -> Int {
  cell.lock(fn(var n) {
    n = n + 1
    n
  })
}
export fn f() -> Int {
  let cell = Shared(0)
  let ignored = bump(cell)
  cell.lock(fn(n) { n })
}
"#;
    assert_eq!(agree(source, "f", vec![]), Answer::Value("1".to_string()));
}

/// A body runs at most once and is awaited at most once, so awaiting the same
/// handle twice answers the same value and repeats no effect.
#[test]
fn awaiting_an_async_call_twice_repeats_no_effect() {
    let source = r#"
async fn bump(cell: Shared<Int>) -> Int {
  cell.lock(fn(var n) {
    n = n + 1
    n
  })
}
export fn f() -> Int {
  let cell = Shared(0)
  let handle = bump(cell)
  let a = await handle
  let b = await handle
  a + b + cell.lock(fn(n) { n })
}
"#;
    assert_eq!(agree(source, "f", vec![]), Answer::Value("3".to_string()));
}

/// A body that raises fails at the call, not at an `await` that never came.
///
/// The other half of "the body runs at the call": nothing here awaits, and
/// the fault still leaves the enclosing function — because there is no thread
/// and no deferral for it to be waiting in.
#[test]
fn an_async_body_that_raises_fails_at_the_call() {
    let source = r#"
async fn boom(n: Int) -> Int { 1 / n }
export fn f() -> Int {
  let ignored = boom(0)
  7
}
"#;
    assert_eq!(
        agree(source, "f", vec![]),
        Answer::Failed("`Int` division by zero".to_string())
    );
}

/// An `async` lambda is a function value like any other, and a call through
/// it answers a task.
#[test]
fn a_call_through_an_async_function_value_agrees() {
    let source = r#"
export fn f() -> Int {
  let g = async fn(n: Int) { n * 2 }
  await g(4)
}
"#;
    assert_eq!(agree(source, "f", vec![]), Answer::Value("8".to_string()));
}

/// And a declared `async fn` used as one behaves the same, which is what
/// makes the two spellings one thing.
#[test]
fn a_declared_async_fn_used_as_a_value_agrees() {
    let source = r#"
async fn twice(n: Int) -> Int { n * 2 }
export fn f() -> Int {
  let g = twice
  await g(4)
}
"#;
    assert_eq!(agree(source, "f", vec![]), Answer::Value("8".to_string()));
}

/// An `async fn` entry answers its value rather than a handle, because the
/// host awaits the entry it chose.
///
/// It is the one place `Function::returns` being `T` rather than `Task<T>`
/// is visible from outside: no task is ever made, and the value that comes
/// out is the one the oracle produces by settling the handle it made.
#[test]
fn an_async_entry_answers_its_value() {
    let source = r#"
export async fn main() -> Int {
  7
}
"#;
    assert_eq!(entry_agrees(source, "main"), Answer::Value("7".to_string()));
}

/// A child the body **awaited** is not waited for a second time when the
/// scope is left, so the answer the body computed from its failure survives.
///
/// `crate::task::wait_for_children` skips a child that is no longer running,
/// and that first line is a language decision rather than an economy: a task
/// the body awaited has already handed its value to the program, and the
/// program has already decided what to do with one. A scope exit that
/// reported it again would replace the recovery with the failure it recovered
/// from, and there would be no way to handle a failed child at all.
#[test]
fn a_failing_child_the_body_awaited_is_not_reported_again_at_the_scope_exit() {
    let source = r#"
fn fails(name: String) -> Result<Int, Error> {
  Err(Error(name))
}
export async fn main() -> Result<Int, Error> {
  scope s {
    let only = s.spawn { fails("only") }
    let answered: Result<Int, Error> = only.await()
    match answered {
      Ok(n) => Ok(n)
      Err(reason) => Ok(reason.message.length())
    }
  }
}
"#;
    assert_eq!(
        entry_agrees(source, "main"),
        Answer::Value("Ok(4)".to_string())
    );
}

/// And what *is* left to report is the child nothing read.
///
/// Both children fail and the body awaits only the first, so the first is the
/// body's own business and the second is the failure sitting unread in a
/// handle nobody awaited — which is the case the rule exists for. The answer
/// is therefore the *second*, even though the first failed earlier and the
/// body saw it.
///
/// `examples/tasks` is this program with two `http.fetch`es in it, and the
/// machine used to answer the first because leaving a scope examined every
/// child rather than every child still running.
#[test]
fn a_scope_exit_reports_the_child_the_body_never_awaited() {
    let source = r#"
fn fails(name: String) -> Result<Int, Error> {
  Err(Error(name))
}
export async fn main() -> Result<Int, Error> {
  scope s {
    let first = s.spawn { fails("first") }
    let second = s.spawn { fails("second") }
    let answered: Result<Int, Error> = first.await()
    match answered {
      Ok(n) => Ok(n)
      Err(reason) => Ok(reason.message.length())
    }
  }
}
"#;
    assert_eq!(
        entry_agrees(source, "main"),
        Answer::Value("Err(second)".to_string())
    );
}

// # GC stress: a program that runs through a collection
//
// Issue #242. Everything above this point runs comfortably inside
// `DEFAULT_HEAP_WORDS` — no case in this file, and none of the 116 programs
// `cove-cli/tests/vm_coverage.rs` runs, has ever forced a collection. The
// collector's own unit tests in `vm::mem` are the hard concurrent cases, but
// they build their programs with `Build`, which writes the root set by hand.
// What none of that exercises is whether *lowering* produced a correct root
// set — `Function::refs`, the static bitmap the collector trusts completely
// and never narrows except where the lowering places `Inst::Clear`.
//
// The three cases below run real source through the checker and the
// lowering, over `Vm::with_heap_words`' small budget rather than the
// default, and hold the answer to the oracle — which has no collector at
// all, so any disagreement is the reference map or a `Clear` placement, not
// a difference of opinion about what the collector itself should do. Each
// asserts a floor on `Vm::collections()` too: a case that would pass whether
// or not a collection happened proves nothing, which is this issue's own
// diagnosis of the state before it.

/// Allocates in a loop and keeps almost none of it: a `Vector<String>` root
/// stands for the whole loop, but only the last three iterations ever push
/// into it, so most of what the loop allocates is garbage by the next turn.
///
/// This is the shape most corpus programs already have — a short-lived
/// allocation inside a loop — which is exactly why it matters that this one
/// collects and the corpus programs never do.
#[test]
fn a_loop_that_keeps_almost_nothing_survives_a_collection() {
    let source = r#"
export fn keeps_a_little(n: Int) -> Int {
  var kept: Vector<String> = Vector.of()
  var i = 0
  while i < n {
    let text = "turn {i} of {n}"
    if i >= n - 3 {
      kept.push(text)
    }
    i += 1
  }
  kept.length()
}
"#;
    let (answer, collections) =
        agree_under_heap_pressure(source, "keeps_a_little", vec![Value::int(4000)], 1 << 12);
    assert_eq!(answer, Answer::Value("3".to_string()));
    assert!(
        collections >= 3,
        "expected several collections over a heap this small, got {collections}"
    );
}

/// Allocates in a loop and keeps *all* of it: a `Vector<Int>` root grows for
/// the whole loop, so its backing storage is reallocated by `push` more than
/// once and a collection must find the growing object's current address
/// correctly, not the one it had when it was last live at a safepoint.
///
/// This is `mem`'s `an_interior_address_survives_a_collection` unit test's
/// question, asked of a program the lowering actually produced instead of
/// one `Build` wrote by hand.
#[test]
fn a_growing_vector_survives_a_collection() {
    // `v`'s own reallocations are unrelated garbage: `Vector.push` doubles
    // its backing store, and when it does, the old store — dead the instant
    // the copy finishes — is the only garbage this loop would otherwise
    // produce. A geometric series of doublings turns out to be close to the
    // worst case for a bump allocator that only ever collects on a failed
    // allocation and retries once: growing `v` on every turn puts almost
    // all of the heap's peak demand into one final doubling, which is
    // either comfortably under budget or permanently over it — there is no
    // heap size in between that fails once and then succeeds. Growing it
    // only on every fourth turn keeps its final size, and so that one
    // allocation's demand, well under the budget; `pad`, discarded every
    // turn regardless, is what forces a collection on a schedule of its
    // own — regular, small, and frequent enough that several run before `v`
    // is done growing.
    let source = r#"
export fn keeps_a_lot(n: Int) -> Int {
  var v: Vector<Int> = Vector.of()
  var sum = 0
  var i = 0
  while i < n {
    if i % 4 == 0 {
      v.push(i)
    }
    sum += i
    let pad = "padding-{i}-{i}-{i}-{i}-{i}-{i}-{i}-{i}"
    i += 1
  }
  sum + v.length()
}
"#;
    let (answer, collections) =
        agree_under_heap_pressure(source, "keeps_a_lot", vec![Value::int(3000)], 1 << 12);
    assert_eq!(answer, Answer::Value("4499250".to_string()));
    assert!(
        collections >= 3,
        "expected several collections over a heap this small, got {collections}"
    );
}

/// A `split` that collects partway through keeps the parts it has made.
///
/// This was a unit test of the Rust arm — `split_holds_the_array_it_is_filling`
/// in `vm::intrinsics::text` — which filled a heap with dead strings so that
/// the array `make::strings` was filling had to survive a collection. The arm
/// is gone: `std.string.split` is a Cove body that slices each part into a
/// local and publishes it into a `Vector` the frame holds, so the question
/// moved from "is the temporary root pushed" to "does the lowering keep the
/// vector and the slice live across the allocation that collects", and that
/// is asked of a body the lowering produced, under a heap small enough that
/// collections land inside the loop. Every part of the last split is joined
/// back and compared, so a part freed under the vector would show as the
/// wrong bytes rather than pass unnoticed.
#[test]
fn a_split_that_collects_keeps_the_parts_it_has_made() {
    let source = r#"
export fn splits_under_pressure(n: Int) -> String {
  var last = "x".split(",")
  var count = 0
  var i = 0
  while i < n {
    let line = "a{i},b,c,d,e,f,g,h,i,j"
    let parts = line.split(",")
    count += parts.length()
    last = parts
    i += 1
  }
  "{count} {"|".join(last)}"
}
"#;
    let (answer, collections) = agree_under_heap_pressure(
        source,
        "splits_under_pressure",
        vec![Value::int(2000)],
        1 << 12,
    );
    assert_eq!(
        answer,
        Answer::Value("20000 a1999|b|c|d|e|f|g|h|i|j".to_string())
    );
    assert!(
        collections >= 3,
        "expected several collections over a heap this small, got {collections}"
    );
}

/// A closure's captured environment stays live and correctly rooted across a
/// collection that happens after it was created and before it is called.
///
/// `greet` closes over `greeting`, a heap string, and nothing else in the
/// function ever reads `greeting` again until `greet()` runs at the very
/// end — every collection the loop below forces happens while the only path
/// back to `greeting` is through the closure's own environment.
#[test]
fn a_captured_environment_survives_a_collection() {
    let source = r#"
export fn closure_survives(n: Int) -> Int {
  let greeting = "hello-{n}"
  let greet = fn() { greeting.length() }
  var i = 0
  while i < n {
    let junk = "junk-{i}-{i}-{i}"
    i += 1
  }
  greet()
}
"#;
    let (answer, collections) =
        agree_under_heap_pressure(source, "closure_survives", vec![Value::int(6000)], 1 << 12);
    assert_eq!(answer, Answer::Value("10".to_string()));
    assert!(
        collections >= 3,
        "expected several collections over a heap this small, got {collections}"
    );
}

/// A budget small enough that even a collection cannot free enough words:
/// allocation must fail cleanly, as a [`crate::RuntimeError`] with a span,
/// rather than panic.
///
/// The oracle has no heap budget and does not fail this program at all, so
/// this is not an `agree`-shaped case — it runs the machine alone and checks
/// the one property that is this backend's to keep: an exhausted heap is a
/// runtime error, not a crash.
#[test]
fn an_exhausted_heap_fails_cleanly_with_a_span() {
    let source = r#"
export fn f(n: Int) -> Int {
  var kept: Vector<String> = Vector.of()
  var i = 0
  while i < n {
    kept.push("turn {i} of {n}")
    i += 1
  }
  kept.length()
}
"#;
    let (sources, checked) = checked(source);
    let program = lowered(&sources, &checked);
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(checked.clone(), sources, hosts.clone());
    let mut vm = Vm::with_heap_words(&runtime, &hosts, &program, 64);
    let outcome = vm.invoke("m", "f", vec![Value::int(100_000)]);
    let error = outcome.expect_err("a heap this small cannot hold what this loop keeps");
    assert_eq!(error.message, "this run has no memory left");
    assert!(
        error.span.is_some(),
        "an exhausted heap should point at the allocation that could not be served"
    );
}

/// **A fault inside the standard library is blamed on its caller, on both
/// evaluators, whether the machine expanded the library body or called it.**
///
/// ADR 0058's "Fallibility preserves the source call site's blame".
/// `RuntimeError::with_chain` is the one place the rule is, and it reads only
/// spans; what this pins is that the spans each evaluator hands it make the
/// rule come out the same. The fixtures are chosen for the one difference that
/// could break that on the machine: `abs` is a small leaf, and `appendByte` a
/// `var self` leaf, so `cove_ir::lower::inline` expands both into their callers
/// and the call site is recovered from [`cove_ir::program::Inlined`];
/// `appendByteBelow` is a standard-library body the inliner cannot expand,
/// because it calls itself, so its call site is a real frame's. The probe is
/// installed for exactly that: since the builder's methods are expanded, no
/// standard-library body that can fault is left a call. The assertions about
/// which is which are made first, so a change to the inliner that moved a
/// fixture fails here by name rather than quietly testing one path twice.
#[test]
fn a_fault_in_the_standard_library_is_blamed_on_its_caller() {
    let source = "
use std.stringbuilder
use std.stringbuilder.StringBuilder

export fn viaAbs(n: Int) -> Int {
  n.abs()
}

export fn viaAppendByte(value: Int) -> Int {
  var out = StringBuilder.withCapacity(4)
  out.appendByte(value)
  out.length()
}

export fn viaAppendByteBelow(value: Int) -> Int {
  var out = StringBuilder.withCapacity(4)
  stringbuilder.appendByteBelow(var out, value, 0)
  out.length()
}
";
    let probe = "
/// `appendByte`, `depth` frames down a recursion no expansion can reach.
export fn appendByteBelow(var out: StringBuilder, value: Int, depth: Int) {
  if depth > 0 {
    appendByteBelow(var out, value, depth - 1)
  } else {
    out.appendByte(value)
  }
}
";
    let (sources, checked) = checked_with_probe(source, "std.stringbuilder", probe);
    let program = lowered(&sources, &checked);
    let callee = |module: &str, name: &str| {
        program
            .function_named(module, name)
            .or_else(|| {
                program
                    .functions
                    .iter()
                    .position(|f| &*f.module == module && f.name.ends_with(name))
                    .map(|at| cove_ir::FunctionId(at as u32))
            })
            .unwrap_or_else(|| panic!("`{module}.{name}` is lowered"))
    };
    let caller = |name: &str| program.function(callee("m", name));
    let expands = |via: &str, module: &str, name: &str| {
        let target = callee(module, name);
        let held = caller(via);
        held.inlined.iter().any(|record| record.callee == target)
            && !held
                .code
                .iter()
                .any(|inst| matches!(inst, cove_ir::Inst::Call { callee, .. } if *callee == target))
    };

    assert!(
        expands("viaAbs", "std.int", "abs"),
        "`abs` is expanded into its caller, which is what makes this an inlined case"
    );
    assert!(
        expands("viaAppendByte", "std.stringbuilder", "appendByte"),
        "`appendByte` is expanded into its caller, `var self` and all"
    );
    let below = callee("std.stringbuilder", "appendByteBelow");
    assert!(
        caller("viaAppendByteBelow")
            .code
            .iter()
            .any(|inst| matches!(
                inst,
                cove_ir::Inst::Call { callee, .. } if *callee == below
            )),
        "`appendByteBelow` is called through a frame, which is what makes this the framed case"
    );

    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(checked.clone(), Arc::clone(&sources), hosts.clone());
    let blame = |error: crate::error::RuntimeError| {
        (
            error.message.clone(),
            error.span,
            error.library_sites().to_vec(),
            error.chain().to_vec(),
        )
    };
    for (name, arg, called) in [
        ("viaAbs", i64::MIN, "n.abs()"),
        ("viaAppendByte", 300, "out.appendByte(value)"),
        (
            "viaAppendByteBelow",
            300,
            "stringbuilder.appendByteBelow(var out, value, 0)",
        ),
    ] {
        let oracle = blame(
            Interpreter::new(&runtime)
                .invoke("m", name, vec![Value::int(arg)])
                .expect_err("the oracle refuses"),
        );
        let machine = blame(
            Vm::new(&runtime, &hosts, &program)
                .invoke("m", name, vec![Value::int(arg)])
                .expect_err("the machine refuses"),
        );
        assert_eq!(machine, oracle, "`{name}`: the two evaluators blame alike");

        let (message, span, library, chain) = oracle;
        let span = span.expect("a fault carries a span");
        assert!(
            !sources.is_library(span.file),
            "`{name}` ({message}): the primary span is the caller's, not the library's"
        );
        assert_eq!(
            &sources.get(span.file).text[span.start as usize..span.end as usize],
            called,
            "`{name}`: and it is the call that reached the library"
        );
        assert!(
            !library.is_empty() && library.iter().all(|site| sources.is_library(site.file)),
            "`{name}`: the library's own line is kept as context: {library:?}"
        );
        assert!(
            chain.is_empty(),
            "`{name}` is the entry, so nothing called it: {chain:?}"
        );
    }
}

/// A core intrinsic, called where only the standard library may call one,
/// answers alike on both evaluators — and on the machine it is the instruction
/// it names rather than a builtin call.
///
/// ADR 0058's `core.byteLength(text)` has no public caller until a method moves
/// onto it, so this installs one: a second file in `std.string`, which is a
/// standard-library module by name and therefore privileged by the same
/// question the checker, the lowering and the oracle each ask. The text has a
/// two-byte character in it, so a length counted in characters would not pass.
#[test]
fn a_core_intrinsic_agrees_and_is_an_instruction() {
    const PROBE: &str = "\
/// The bytes of `text`, through the core intrinsic.
export fn probeBytes(text: String) -> Int {
  core.byteLength(text)
}
";
    const MAIN: &str = "\
use std.string

export fn main(text: String) -> Int {
  string.probeBytes(text) + 1
}
";
    fn probed() -> (Arc<SourceMap>, Arc<Checked>) {
        let mut sources = SourceMap::new();
        let file = sources.add("m/main.cove", MAIN.to_string());
        let ast = cove_syntax::parse_file(&sources, file).expect("the program parses");
        let mut modules = BTreeMap::from([(
            "m".to_string(),
            Module {
                name: "m".to_string(),
                dir: PathBuf::from("m"),
                units: vec![Unit {
                    file,
                    path: PathBuf::from("m/main.cove"),
                    ast,
                }],
            },
        )]);
        for (name, module) in cove_sema::stdlib::attach(&mut sources).expect("stdlib parses") {
            modules.insert(name, module);
        }
        let path = PathBuf::from("std/string_probe.cove");
        let file = sources.add_library(path.clone(), PROBE);
        let ast = cove_syntax::parse_file(&sources, file).expect("the probe parses");
        modules
            .get_mut("std.string")
            .expect("the standard library has `std.string`")
            .units
            .push(Unit { file, path, ast });
        let package = Package {
            root: PathBuf::from("."),
            config: Config::default(),
            modules,
        };
        match cove_sema::Compiler::new().compile(&package) {
            Ok(program) => (Arc::new(sources), Arc::new(program)),
            Err(items) => panic!("the probe checks:\n{}", rendered(&sources, &items)),
        }
    }

    let (sources, program) = probed();
    let ir = lowered(&sources, &program);
    let probe = ir
        .functions
        .iter()
        .find(|f| &*f.module == "std.string" && &*f.name == "probeBytes")
        .expect("the probe was lowered");
    assert!(
        probe
            .code
            .iter()
            .any(|inst| matches!(inst, cove_ir::Inst::Len { .. })),
        "`core.byteLength` is a `len`: {:?}",
        probe.code
    );
    let main = ir
        .functions
        .iter()
        .find(|f| &*f.module == "m" && &*f.name == "main")
        .expect("the caller was lowered");
    for f in [probe, main] {
        assert!(
            f.code
                .iter()
                .all(|inst| !matches!(inst, cove_ir::Inst::IntrinsicCall { .. })),
            "`{}.{}` makes no builtin call: {:?}",
            f.module,
            f.name,
            f.code
        );
    }

    for (module, name, arg) in [
        ("std.string", "probeBytes", "h\u{e9}llo"),
        ("m", "main", "caf\u{e9}"),
    ] {
        let oracle = on_a_deep_stack(move || {
            let (sources, program) = probed();
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts);
            said(Interpreter::new(&runtime).invoke(module, name, vec![Value::string(arg)]))
        });
        let machine = on_a_deep_stack(move || {
            let (sources, program) = probed();
            let ir = lowered(&sources, &program);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts.clone());
            said(Vm::new(&runtime, &hosts, &ir).invoke(module, name, vec![Value::string(arg)]))
        });
        assert_eq!(machine, oracle, "`{module}.{name}` answers alike");
        let want = arg.len() as i64 + i64::from(module == "m");
        assert_eq!(oracle, Answer::Value(want.to_string()));
    }
}

/// `String.sliceBytes`, a standard-library body since ADR 0058, answers every
/// range of a string alike on both evaluators — and what both answer is Rust's
/// own reading of the same bytes, in the five sentences the method refuses
/// with, in the order it asks them.
///
/// Every `from` and `to` from one before the string to one past it, over text
/// with a one-, two-, three- and four-byte character, so each continuation byte
/// is a refused end and each boundary an answered one.
#[test]
fn every_byte_range_agrees_and_is_rusts_reading() {
    const TEXT: &str = "aé→😀z";
    let source = format!(
        "
export fn cuts() -> String {{
  let text = \"{TEXT}\"
  var out: Vector<String> = Vector.of()
  var from = -1
  while from <= text.byteLength() + 1 {{
    var to = -1
    while to <= text.byteLength() + 1 {{
      match text.sliceBytes(from, to) {{
        Ok(part) => out.push(\"[{{part}}]\")
        Err(error) => out.push(error.message)
      }}
      to += 1
    }}
    from += 1
  }}
  \"\\n\".join(out.freeze())
}}
"
    );
    let len = TEXT.len() as i64;
    let mut want = Vec::new();
    for from in -1..=len + 1 {
        for to in -1..=len + 1 {
            let range = |name: &str, value: i64| {
                format!("`{name}` is `{value}`, and a byte offset into this string is 0 to {len}")
            };
            let inside = |name: &str, value: i64| {
                format!(
                    "`{name}` is `{value}`, which is inside a character rather than at the start \
                     of one"
                )
            };
            want.push(if from < 0 || from > len {
                range("from", from)
            } else if to < 0 || to > len {
                range("to", to)
            } else if from > to {
                format!("`from` is `{from}` and `to` is `{to}`, so this range runs backwards")
            } else if !TEXT.is_char_boundary(from as usize) {
                inside("from", from)
            } else if !TEXT.is_char_boundary(to as usize) {
                inside("to", to)
            } else {
                format!("[{}]", &TEXT[from as usize..to as usize])
            });
        }
    }
    assert_eq!(
        agree(&source, "cuts", Vec::new()),
        Answer::Value(want.join("\n"))
    );
}

/// **An expanded standard-library body that writes through a `var` parameter
/// answers what the call answered**, including where the caller hands it the
/// place it writes a second time.
///
/// `bumpThenAdd(var a, a)` copies `a` into `by` before `x = x + 1` writes `a`,
/// so a call answers `3` and leaves `a` at `2`. An expansion reads a parameter
/// it never writes where the caller has it, and would read `a` *after* the
/// write and answer `4`. `a = bumpBefore(var a, 10)` answers the `1` it read
/// before writing `11`; an expansion that assembled that answer straight in `a`
/// would have the write land on it, which today's lowering does not risk — it
/// hands the call a temporary — and `cove_ir`'s `inlining` tests pin by hand.
/// `cove_ir::lower::inline` keeps a
/// call's order for exactly the callees a call like these reaches, and this is
/// the oracle's word that it does. Probes, because no body the standard library
/// has takes a `var` and another argument of the same type.
#[test]
fn an_expanded_var_body_reads_its_arguments_before_it_writes_through_them() {
    let source = "
use std.int

export fn twice(start: Int) -> Int {
  var a = start
  let b = int.bumpThenAdd(var a, a)
  a * 100 + b
}

export fn overwrites(start: Int) -> Int {
  var a = start
  a = int.bumpBefore(var a, 10)
  a
}

export fn apart(start: Int) -> Int {
  var a = start
  let by = 5
  let b = int.bumpThenAdd(var a, by)
  a * 100 + b
}
";
    let probe = "
/// `x` raised by one, and then `by` added to what it became.
export fn bumpThenAdd(var x: Int, by: Int) -> Int {
  x = x + 1
  x + by
}

/// What `x` was, after raising it by `by`.
export fn bumpBefore(var x: Int, by: Int) -> Int {
  let before = x
  x = x + by
  before
}
";
    let (sources, checked) = checked_with_probe(source, "std.int", probe);
    let program = lowered(&sources, &checked);
    let probed = |name: &str| {
        program
            .functions
            .iter()
            .position(|f| &*f.module == "std.int" && &*f.name == name)
            .map(|at| cove_ir::FunctionId(at as u32))
            .unwrap_or_else(|| panic!("`{name}` is lowered"))
    };
    for (name, called) in [
        ("twice", "bumpThenAdd"),
        ("overwrites", "bumpBefore"),
        ("apart", "bumpThenAdd"),
    ] {
        let target = probed(called);
        let f = program
            .functions
            .iter()
            .find(|f| &*f.module == "m" && &*f.name == name)
            .expect("the caller is lowered");
        assert!(
            f.inlined.iter().any(|record| record.callee == target)
                && !f.code.iter().any(
                    |inst| matches!(inst, cove_ir::Inst::Call { callee, .. } if *callee == target)
                ),
            "`{name}`: `{called}` is expanded, which is what this is about: {:?}",
            f.code
        );
    }

    for (name, want) in [("twice", 203), ("overwrites", 1), ("apart", 207)] {
        let oracle = on_a_deep_stack(move || {
            let (sources, program) = checked_with_probe(source, "std.int", probe);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts);
            said(Interpreter::new(&runtime).invoke("m", name, vec![Value::int(1)]))
        });
        let machine = on_a_deep_stack(move || {
            let (sources, program) = checked_with_probe(source, "std.int", probe);
            let ir = lowered(&sources, &program);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts.clone());
            said(Vm::new(&runtime, &hosts, &ir).invoke("m", name, vec![Value::int(1)]))
        });
        assert_eq!(
            oracle,
            Answer::Value(want.to_string()),
            "`{name}` on the oracle"
        );
        assert_eq!(machine, oracle, "`{name}` answers alike");
    }
}

/// ADR 0059's keyed core intrinsics answer alike on both evaluators, across
/// every family a key may be — and on the machine each is the instruction or
/// the static intrinsic the lowering chose for the key's layout.
///
/// `core.order` over the scalars, `String`s that differ in their ninth byte,
/// payload-free enums declared in and out of case-name order, an `Option`, a
/// struct, arrays, sets, maps and ranges; `core.admitKey` of a key it removes
/// and of two it refuses, one nested; a literal's duplicate, which
/// `std.set.of` refuses in Cove; and
/// `core.memberAt` and `core.entryAt` over one-word members and a map of
/// two-word values. Each answer is also the one the language's order gives,
/// written out, so an agreement on a wrong order would not pass either.
#[test]
fn the_keyed_core_intrinsics_agree_across_families() {
    let probe = "\
/// A two-word key.
struct ProbePoint {
  x: Int
  y: Int
}

/// Declared out of case-name order.
enum ProbeColor {
  Red
  Green
  Blue
}

/// Declared in case-name order.
enum ProbeFruit {
  Apple
  Banana
  Cherry
}

/// A key that nests a `Float`.
struct ProbeReading {
  weight: Float
}

/// `core.order` across families, one family a line.
export fn probeOrders() -> String {
  let least = -9223372036854775807 - 1
  let ints = \"{core.order(1, 2)} {core.order(2, 2)} {core.order(3, 2)} {core.order(least, 9223372036854775807)}\"
  let durations = \"{core.order(Duration.millis(-5), Duration.seconds(1))} {core.order(Duration.seconds(1), Duration.millis(1000))}\"
  let bools = \"{core.order(false, true)} {core.order(true, true)} {core.order(true, false)}\"
  let strings = \"{core.order(\"\", \"\")} {core.order(\"ab\", \"abc\")} {core.order(\"b\", \"abc\")} {core.order(\"Z\", \"a\")} {core.order(\"h\u{e9}llo\", \"hello\")} {core.order(\"abcdefghi\", \"abcdefghj\")} {core.order(\"abcdefghj\", \"abcdefghi\")} {core.order(\"abcdefgh\", \"abcdefgh\")}\"
  let units = \"{core.order((), ())}\"
  let colors = \"{core.order(ProbeColor.Red, ProbeColor.Blue)} {core.order(ProbeColor.Green, ProbeColor.Red)} {core.order(ProbeColor.Blue, ProbeColor.Blue)}\"
  let fruits = \"{core.order(ProbeFruit.Apple, ProbeFruit.Cherry)} {core.order(ProbeFruit.Cherry, ProbeFruit.Banana)} {core.order(ProbeFruit.Banana, ProbeFruit.Banana)}\"
  let nothing: Option<Int> = None
  let options = \"{core.order(Some(1), nothing)} {core.order(Some(1), Some(2))}\"
  let points = \"{core.order(ProbePoint(x: 1, y: 2), ProbePoint(x: 1, y: 3))} {core.order(ProbePoint(x: 2, y: 0), ProbePoint(x: 1, y: 9))} {core.order(ProbePoint(x: 1, y: 2), ProbePoint(x: 1, y: 2))}\"
  let empty: Array<Int> = []
  let arrays = \"{core.order([1, 2], [1])} {core.order(empty, [0])} {core.order([1, 2], [1, 2])}\"
  let sets = \"{core.order(Set.of(1, 2), Set.of(1, 3))} {core.order(Set.of(2), Set.of(1, 3))}\"
  let maps = \"{core.order(Map.of(MapEntry(key: \"a\", value: 1)), Map.of(MapEntry(key: \"a\", value: 2)))}\"
  let ranges = \"{core.order(0..<3, 0..3)} {core.order(1..<2, 0..3)}\"
  \"{ints} | {durations} | {bools} | {strings} | {units} | {colors} | {fruits} | {options} | {points} | {arrays} | {sets} | {maps} | {ranges}\"
}

/// A key the admission removes: nothing it can hold is refused.
export fn probeAdmitted(key: Int) -> Int {
  core.admitKey(key, \"Map.get\", \"map key\")
  core.admitKey(Set.of([key], [2]), \"Set.contains\", \"set element\")
  key
}

/// A `Float` key, refused.
export fn probeFloat() -> Int {
  core.admitKey(1.5, \"Map.get\", \"map key\")
  1
}

/// A key nesting a `Float`, refused with its path.
export fn probeNested() -> Int {
  core.admitKey([ProbeReading(weight: 1.5)], \"Set.contains\", \"set element\")
  1
}

/// A literal's duplicate, refused by `std.set.of` itself.
export fn probeDuplicate() -> Int {
  let twice = of(ProbePoint(x: 1, y: 2), ProbePoint(x: 1, y: 2))
  twice.length()
}

/// The members and entries of sorted runs, read by position.
export fn probeElements() -> String {
  let small = Set.of(3, 1, 2)
  let wide = Set.of(ProbePoint(x: 2, y: 0), ProbePoint(x: 1, y: 9))
  let byName = Map.of(
    MapEntry(key: \"b\", value: ProbePoint(x: 5, y: 6)),
    MapEntry(key: \"a\", value: ProbePoint(x: 7, y: 8)),
  )
  let first = core.entryAt(byName, 0)
  let last = core.entryAt(byName, 1)
  \"{core.memberAt(small, 0)} {core.memberAt(small, 2)} {core.memberAt(wide, 0)} {first.key} {first.value} {last.key} {last.value}\"
}
";
    let source = "\
use std.set

export fn main() -> Int {
  set.probeAdmitted(1)
}
";
    let (sources, checked) = checked_with_probe(source, "std.set", probe);
    let program = lowered(&sources, &checked);
    let function = |name: &str| {
        program
            .functions
            .iter()
            .find(|f| &*f.module == "std.set" && &*f.name == name)
            .unwrap_or_else(|| panic!("`{name}` is lowered"))
    };
    let intrinsics = |name: &str| -> Vec<cove_ir::Intrinsic> {
        function(name)
            .code
            .iter()
            .filter_map(|inst| match inst {
                cove_ir::Inst::IntrinsicCall { site, .. } => {
                    Some(program.intrinsic_site(*site).intrinsic)
                }
                _ => None,
            })
            .collect()
    };
    let orders = |name: &str| -> Vec<cove_ir::Compare> {
        function(name)
            .code
            .iter()
            .filter_map(|inst| match inst {
                cove_ir::Inst::Cmp {
                    on,
                    op: cove_ir::CmpOp::Order,
                    ..
                } => Some(*on),
                _ => None,
            })
            .collect()
    };

    // One instruction wherever one orders the key as a key is ordered, and a
    // walk `cove_ir::lower::synth` composed wherever one does not.
    //
    // **The probe reaches the intrinsic zero times**, which is the number
    // that moved. It used to reach it seventeen: the unit, the three colours
    // — declared out of case-name order, where the three fruits are not —
    // the two options, the three points, the three arrays, the two sets, the
    // map and the two ranges. Every one of those is a layout the lowering
    // knows, so every one of them is now a function it wrote, and ADR 0064's
    // Decision 4 leaves the intrinsic for a box alone. This probe holds none,
    // so it holds none of the intrinsic either.
    //
    // The comparison counts below are a fence and no longer a statement about
    // the short circuit on its own. `lower::inline` expands the small walks
    // into the caller, so what they see is the short circuit's instructions
    // and the inlined walks' together — six `Int`s and three `Bool`s and
    // three `Tag`s of short circuit, and the rest composed. The short circuit
    // is pinned where it *can* be seen alone, in
    // `cove_ir::lower::tests::synthesis`'
    // `a_key_one_instruction_orders_is_not_a_function`, which asserts that a
    // key one instruction orders is not a function at all.
    let compares = orders("probeOrders");
    let count = |on: cove_ir::Compare| compares.iter().filter(|c| **c == on).count();
    assert_eq!(count(cove_ir::Compare::Int), 28, "{compares:?}");
    assert_eq!(count(cove_ir::Compare::Bool), 5, "{compares:?}");
    assert_eq!(count(cove_ir::Compare::Str), 8, "{compares:?}");
    assert_eq!(count(cove_ir::Compare::Tag), 5, "{compares:?}");
    let walks = intrinsics("probeOrders")
        .into_iter()
        .filter(|intrinsic| *intrinsic == cove_ir::Intrinsic::ValueOrder)
        .count();
    assert_eq!(walks, 0, "a layout the lowering knows is a walk it wrote");
    assert_eq!(
        intrinsics("probeAdmitted"),
        Vec::new(),
        "an admission that cannot refuse is removed"
    );
    assert_eq!(
        intrinsics("probeFloat"),
        vec![cove_ir::Intrinsic::ValueAdmitKey]
    );
    assert!(intrinsics("probeNested").contains(&cove_ir::Intrinsic::ValueAdmitKey));
    // The duplicate's refusal is `std.set.of`'s own Cove since ADR 0067, so
    // what the probe reaches is no intrinsic at all: the sentence is an
    // interpolation of a layout the lowering knows, which is a walk it wrote.
    assert_eq!(intrinsics("probeDuplicate"), Vec::new());
    assert!(function("probeElements")
        .code
        .iter()
        .any(|inst| matches!(inst, cove_ir::Inst::LoadElem { .. })));

    let wanted = [
        (
            "probeOrders",
            Answer::Value(
                "-1 0 1 -1 | -1 0 | -1 0 1 | 0 -1 1 -1 1 -1 1 0 | 0 | 1 -1 0 | -1 1 0 | 1 -1 | \
                 -1 1 0 | 1 -1 0 | -1 1 | -1 | -1 1"
                    .to_string(),
            ),
        ),
        ("probeAdmitted", Answer::Value("1".to_string())),
        (
            "probeFloat",
            Answer::Failed("`Map.get` cannot use a `Float` as a map key".to_string()),
        ),
        (
            "probeNested",
            Answer::Failed(
                "`Set.contains` cannot use a `Float` inside `[0].weight` as a set element"
                    .to_string(),
            ),
        ),
        (
            "probeDuplicate",
            Answer::Failed(
                "`Set.of` was given the element `ProbePoint(x: 1, y: 2)` more than once"
                    .to_string(),
            ),
        ),
        (
            "probeElements",
            Answer::Value(
                "1 3 ProbePoint(x: 1, y: 9) a ProbePoint(x: 7, y: 8) b ProbePoint(x: 5, y: 6)"
                    .to_string(),
            ),
        ),
    ];
    for (name, want) in wanted {
        let args = if name == "probeAdmitted" {
            vec![1]
        } else {
            vec![]
        };
        let oracle = {
            let args = args.clone();
            on_a_deep_stack(move || {
                let (sources, program) = checked_with_probe(source, "std.set", probe);
                let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
                let runtime = Runtime::new(program, sources, hosts);
                said(Interpreter::new(&runtime).invoke(
                    "std.set",
                    name,
                    args.into_iter().map(Value::int).collect(),
                ))
            })
        };
        let machine = on_a_deep_stack(move || {
            let (sources, program) = checked_with_probe(source, "std.set", probe);
            let ir = lowered(&sources, &program);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts.clone());
            said(Vm::new(&runtime, &hosts, &ir).invoke(
                "std.set",
                name,
                args.into_iter().map(Value::int).collect(),
            ))
        });
        assert_eq!(oracle, want, "`{name}` on the oracle");
        assert_eq!(machine, oracle, "`{name}` answers alike");
    }
}

/// `Float.format` in Cove answers what Rust's `format!("{:.*}")` answered, on
/// thousands of binary64 values the corpus did not choose.
///
/// `tests/e2e/values_float_format` and `values_float_roundtrip` are the
/// contract, and they are rows a person picked: the ties, the extremes, the
/// carries. This is the other half — the property over values nobody picked,
/// with the arm the body replaced as the oracle, since the arm is gone from the
/// runtime but `format!` is still in the toolchain. A body that was exact on
/// every row a reader thought of and wrong on some exponent nobody did would
/// pass the corpus and fail here.
///
/// The values are three populations, because uniform random bits are mostly
/// huge or tiny and say little about the numbers programs format: raw bit
/// patterns over the whole range (NaNs and infinities included); integers times
/// small powers of two, which is where exact ties live; and ordinary decimals
/// a program writes, `n / 10^j`. Each is formatted at a digit count drawn from
/// `0..=17`, after thirteen edges — both zeros, both infinities, a NaN, the
/// least subnormal, the least normal, the largest finite and four ties — at
/// four digit counts each. The linear-memory backend is asked all of them and
/// the oracle a prefix that holds every edge, because the tree-walking
/// interpreter runs the same body a few hundred times slower, and what is asked
/// of it is agreement rather than coverage.
#[test]
fn a_float_formats_as_rusts_formatter_did_across_random_binary64() {
    let source = "
export fn formatted(xs: Array<Float>, ds: Array<Int>) -> String {
  var out: Vector<String> = Vector.of()
  var at = 0
  for x in xs {
    let digits = match ds.get(at) {
      Some(n) => n
      None => 0
    }
    out.push(x.format(digits))
    at += 1
  }
  \"\\n\".join(out.freeze())
}
";
    // xorshift, seeded, so a failure names a value that can be looked at again.
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    // The edges first, at every digit count, so that the prefix the
    // interpreter is asked holds them too: random bits almost never make a
    // zero, and a body that mishandled one — reading a limb nought never
    // pushed, which one evaluator refuses and the other answers — passed
    // twenty thousand random rows on the machine before the corpus's
    // `special.zero.*` rows caught it on the oracle.
    let edges = [
        0.0,
        -0.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        f64::from_bits(1),
        f64::MIN_POSITIVE,
        f64::MAX,
        0.5,
        -0.5,
        2.5,
        0.125,
        1.0,
    ];
    let mut cases: Vec<(u64, i64)> = Vec::new();
    for x in edges {
        for digits in [0, 1, 2, 17] {
            cases.push((f64::to_bits(x), digits));
        }
    }
    for round in 0..20_000 {
        let digits = (next() % 18) as i64;
        let x = match round % 3 {
            0 => f64::from_bits(next()),
            1 => {
                let m = (next() >> 11) as f64;
                let shift = (next() % 80) as i32 - 60;
                let signed = if next() % 2 == 0 { m } else { -m };
                signed * 2f64.powi(shift)
            }
            _ => {
                let n = (next() % 10_000_000) as f64;
                let places = (next() % 8) as i32;
                n / 10f64.powi(places)
            }
        };
        cases.push((x.to_bits(), digits));
    }
    let expected = cases
        .iter()
        .map(|(bits, digits)| format!("{:.*}", *digits as usize, f64::from_bits(*bits)))
        .collect::<Vec<_>>();

    let formatted = |cases: Vec<(u64, i64)>, on_machine: bool| -> String {
        on_a_deep_stack(move || {
            let xs = Value::array(
                cases
                    .iter()
                    .map(|(bits, _)| Value::float(f64::from_bits(*bits))),
            );
            let ds = Value::array(cases.iter().map(|(_, digits)| Value::int(*digits)));
            let (sources, checked) = checked(source);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let answer = if on_machine {
                let program = lowered(&sources, &checked);
                let runtime = Runtime::new(checked, sources, hosts.clone());
                Vm::new(&runtime, &hosts, &program).invoke("m", "formatted", vec![xs, ds])
            } else {
                let runtime = Runtime::new(checked, sources, hosts);
                Interpreter::new(&runtime).invoke("m", "formatted", vec![xs, ds])
            };
            match answer {
                Ok(value) => value.to_string(),
                Err(error) => panic!("the body answers: {}", error.message),
            }
        })
    };

    let machine = formatted(cases.clone(), true);
    let lines = machine.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), cases.len());
    let mut wrong = Vec::new();
    for (at, (line, want)) in lines.iter().zip(&expected).enumerate() {
        if *line != want {
            wrong.push(format!(
                "{:#018x} at {} digits: {line} against {want}",
                cases[at].0, cases[at].1
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} disagree with `format!`, first: {:?}",
        wrong.len(),
        cases.len(),
        &wrong[..wrong.len().min(5)]
    );

    let prefix = cases[..300].to_vec();
    let oracle = formatted(prefix, false);
    assert_eq!(
        oracle.lines().collect::<Vec<_>>(),
        lines[..300].to_vec(),
        "the interpreter formats the first 300 as the machine does"
    );
}

/// A refusal a Cove body worded arrives whole on both evaluators — all three
/// sentences, not the message alone.
///
/// [ADR 0067][adr] gives `Inst::Trap` three slots and the standard library one
/// primitive to stop a run with, `core.refuse`. The three sentences are what a
/// diagnostic prints, so the three are what has to agree: a `rule:` the
/// machine dropped would be a refusal that reads differently depending on
/// which evaluator ran it, and `said` above compares messages only, which is
/// why this case does not go through it.
///
/// Two probes and two questions. The first quotes a value the run computed,
/// which is the whole reason the sentences are slots and not a `StrId` — no
/// lowering could have written `7` down. The second says a message and nothing
/// else: an **empty sentence is an absent one**, so what must come back is
/// `None` rather than a blank `rule:` line, and this is the only place that
/// distinction is asserted about both evaluators at once.
///
/// It also pins the lowering: one `Inst::Trap` and **no** `IntrinsicCall`. The
/// six operations that waited on this — `split`, `replace`, `parseRadix`,
/// `format`, a byte range and a duplicate key — migrate onto exactly that, and
/// a `core.refuse` that lowered to an intrinsic would be one more variant
/// rather than the end of six.
///
/// [adr]: ../../../../docs/adr/0067-a-trap-carries-the-sentence-it-was-handed.md
#[test]
fn a_refusal_a_cove_body_words_arrives_whole_on_both_evaluators() {
    let probe = "\
/// Three sentences, one of them quoting a value the run computed.
export fn probeRefusal(at: Int) -> Int {
  core.refuse(
    \"`probe` cannot use `{at}`\",
    \"A probe refuses an argument it was given, and says which.\",
    \"pass an argument the probe admits\",
  )
  1
}

/// A message and nothing else, which is two empty sentences.
export fn probeMessageOnly() -> Int {
  core.refuse(\"`probe` refuses, and has only this to say\", \"\", \"\")
  1
}
";
    let source = "\
use std.set

export fn main() -> Int {
  set.probeMessageOnly()
}
";
    let (sources, checked) = checked_with_probe(source, "std.set", probe);
    let program = lowered(&sources, &checked);
    let function = |name: &str| {
        program
            .functions
            .iter()
            .find(|f| &*f.module == "std.set" && &*f.name == name)
            .unwrap_or_else(|| panic!("`{name}` is lowered"))
    };
    for name in ["probeRefusal", "probeMessageOnly"] {
        let code = &function(name).code;
        assert!(
            code.iter()
                .any(|inst| matches!(inst, cove_ir::Inst::Trap { .. })),
            "`{name}` stops the run with a trap"
        );
        assert!(
            !code
                .iter()
                .any(|inst| matches!(inst, cove_ir::Inst::IntrinsicCall { .. })),
            "`{name}` reaches no intrinsic: a refusal primitive that was one \
             would be a variant rather than the end of six"
        );
    }

    /// `probeRefusal` is given the number its refusal quotes; the other
    /// probe takes none. A `fn` rather than a closure because both threads
    /// below need it and a `Value` cannot be sent to either.
    fn args_for(name: &str) -> Vec<Value> {
        match name {
            "probeRefusal" => vec![Value::int(7)],
            _ => Vec::new(),
        }
    }

    // The three sentences, as a diagnostic would print them, off each
    // evaluator in turn. Taken apart inside the thread because a
    // `RuntimeError` holds more than three strings and only the strings need
    // to cross.
    let sentences = |name: &'static str| -> (String, Option<String>, Option<String>) {
        let said = |error: crate::error::RuntimeError| {
            (
                error.message,
                error.rule.map(|rule| rule.to_string()),
                error.help.map(|help| help.to_string()),
            )
        };
        let oracle = on_a_deep_stack(move || {
            let (sources, checked) = checked_with_probe(source, "std.set", probe);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(checked, sources, hosts);
            said(
                Interpreter::new(&runtime)
                    .invoke("std.set", name, args_for(name))
                    .expect_err("the probe refuses"),
            )
        });
        let machine = on_a_deep_stack(move || {
            let (sources, checked) = checked_with_probe(source, "std.set", probe);
            let ir = lowered(&sources, &checked);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(checked, sources, hosts.clone());
            said(
                Vm::new(&runtime, &hosts, &ir)
                    .invoke("std.set", name, args_for(name))
                    .expect_err("the probe refuses"),
            )
        });
        assert_eq!(
            machine, oracle,
            "the machine and the interpreter word `{name}` alike"
        );
        oracle
    };

    assert_eq!(
        sentences("probeRefusal"),
        (
            "`probe` cannot use `7`".to_string(),
            Some("A probe refuses an argument it was given, and says which.".to_string()),
            Some("pass an argument the probe admits".to_string()),
        )
    );
    assert_eq!(
        sentences("probeMessageOnly"),
        (
            "`probe` refuses, and has only this to say".to_string(),
            None,
            None,
        ),
        "an empty sentence is an absent one on both evaluators, not a blank line"
    );
}

/// ADR 0062's append — `core.vectorEnsure`, `core.vectorStore` at the length,
/// `core.vectorCommit` — answers alike on both evaluators, through growths and
/// for an ensure the machine refuses; and the oracle's staged suffix is what
/// makes it a model of the protocol rather than of a push: an element written
/// and not committed is in no length, and a commit of what was never written
/// is refused.
#[test]
fn the_append_protocol_agrees_and_the_oracle_publishes_only_what_was_written() {
    let probe = "\
/// Five pushes onto a vector with room for one, then an element replaced.
export fn probePushes() -> String {
  let xs: Vector<Int> = core.vectorWithCapacity(1)
  var n = 1
  while n <= 5 {
    let at = core.vectorLength(xs)
    core.vectorEnsure(xs, 1)
    core.vectorStore(xs, at, n * 10)
    core.vectorCommit(xs, 1)
    n = n + 1
  }
  core.vectorStore(xs, 0, 7)
  \"{core.vectorLength(xs)} {core.vectorFinish(xs)}\"
}

/// An ensure of a negative room.
export fn probeNegative() -> Int {
  let xs: Vector<Int> = core.vectorWithCapacity(1)
  core.vectorEnsure(xs, -1)
  core.vectorLength(xs)
}
";
    let source = "\
use std.set

export fn main() -> Int {
  1
}
";
    let wanted = [
        (
            "probePushes",
            Answer::Value("5 [7, 20, 30, 40, 50]".to_string()),
        ),
        (
            "probeNegative",
            Answer::Failed(
                "`growableEnsure` was asked for room for -1 unit(s), and room is never negative"
                    .to_string(),
            ),
        ),
    ];
    for (name, want) in wanted {
        let oracle = on_a_deep_stack(move || {
            let (sources, program) = checked_with_probe(source, "std.set", probe);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts);
            said(Interpreter::new(&runtime).invoke("std.set", name, vec![]))
        });
        let machine = on_a_deep_stack(move || {
            let (sources, program) = checked_with_probe(source, "std.set", probe);
            let ir = lowered(&sources, &program);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts.clone());
            said(Vm::new(&runtime, &hosts, &ir).invoke("std.set", name, vec![]))
        });
        assert_eq!(oracle, want, "`{name}` on the oracle");
        assert_eq!(machine, oracle, "`{name}` answers alike");
    }

    // What no verified lowering can express, so the oracle alone: the
    // reservation rule refuses a read of the length between the write and
    // the commit, and a commit with no write.
    let unlowerable = "\
/// The length before and after the commit of a written element.
export fn probeInvisible() -> String {
  let xs: Vector<Int> = core.vectorWithCapacity(2)
  let at = core.vectorLength(xs)
  core.vectorEnsure(xs, 1)
  core.vectorStore(xs, at, 9)
  let before = core.vectorLength(xs)
  core.vectorCommit(xs, 1)
  \"{before} {core.vectorLength(xs)} {core.vectorFinish(xs)}\"
}

/// A commit of an element nobody wrote.
export fn probeUnwritten() -> Int {
  let xs: Vector<Int> = core.vectorWithCapacity(2)
  core.vectorEnsure(xs, 1)
  core.vectorCommit(xs, 1)
  core.vectorLength(xs)
}
";
    let wanted = [
        ("probeInvisible", Answer::Value("0 1 [9]".to_string())),
        (
            "probeUnwritten",
            Answer::Failed(
                "`growableCommit` would publish 1 unit(s) onto a length of 0 with 0 written \
                 above it, and a commit publishes only units its window wrote"
                    .to_string(),
            ),
        ),
    ];
    for (name, want) in wanted {
        let oracle = on_a_deep_stack(move || {
            let (sources, program) = checked_with_probe(source, "std.set", unlowerable);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts);
            said(Interpreter::new(&runtime).invoke("std.set", name, vec![]))
        });
        assert_eq!(oracle, want, "`{name}` on the oracle");
    }
}

/// **A truncate answers and refuses alike on both evaluators.**
///
/// `Vector.pop` and `Vector.remove` compute the new length from the length
/// they have just read, so no Cove program can reach the truncate's own
/// refusal — which is why it is reached here through a probe, the way
/// `growableEnsure`'s negative room is. The message is the thing under test:
/// the tree-walking oracle implements a truncate with Rust's `Vec::truncate`
/// and the machine implements it with a clear and a length write, and two
/// implementations that refuse the same program in different words are two
/// languages (#378, Q9).
///
/// The lowering half is `probeTruncates`, which walks every length a truncate
/// may be given — the one it already has, one below it, and nought — and
/// finishes what is left, so a no-op that cleared, or a full truncate that
/// kept a length, shows up in the answer rather than only in the heap.
#[test]
fn a_truncate_answers_and_refuses_alike() {
    let probe = "\
/// Three pushed, then the whole ladder of lengths a truncate may be given.
export fn probeTruncates() -> String {
  let xs: Vector<Int> = core.vectorWithCapacity(4)
  var n = 1
  while n <= 3 {
    let at = core.vectorLength(xs)
    core.vectorEnsure(xs, 1)
    core.vectorStore(xs, at, n * 10)
    core.vectorCommit(xs, 1)
    n = n + 1
  }
  core.vectorTruncate(xs, 3)
  let same = core.vectorLength(xs)
  core.vectorTruncate(xs, 1)
  let shrunk = core.vectorLength(xs)
  core.vectorTruncate(xs, 0)
  \"{same} {shrunk} {core.vectorLength(xs)} {core.vectorFinish(xs)}\"
}

/// A truncate to a length above the one the vector has.
export fn probeRaises() -> Int {
  let xs: Vector<Int> = core.vectorWithCapacity(2)
  core.vectorTruncate(xs, 1)
  core.vectorLength(xs)
}

/// A truncate to a length below zero.
export fn probeNegative() -> Int {
  let xs: Vector<Int> = core.vectorWithCapacity(2)
  core.vectorTruncate(xs, -1)
  core.vectorLength(xs)
}
";
    let source = "\
use std.set

export fn main() -> Int {
  1
}
";
    let wanted = [
        ("probeTruncates", Answer::Value("3 1 0 []".to_string())),
        (
            "probeRaises",
            Answer::Failed(
                "`growableTruncate` would take a length of 0 to 1, and a truncate only lowers a \
                 length"
                    .to_string(),
            ),
        ),
        (
            "probeNegative",
            Answer::Failed(
                "`growableTruncate` would take a length of 0 to -1, and a truncate only lowers \
                 a length"
                    .to_string(),
            ),
        ),
    ];
    for (name, want) in wanted {
        let oracle = on_a_deep_stack(move || {
            let (sources, program) = checked_with_probe(source, "std.set", probe);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts);
            said(Interpreter::new(&runtime).invoke("std.set", name, vec![]))
        });
        let machine = on_a_deep_stack(move || {
            let (sources, program) = checked_with_probe(source, "std.set", probe);
            let ir = lowered(&sources, &program);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts.clone());
            said(Vm::new(&runtime, &hosts, &ir).invoke("std.set", name, vec![]))
        });
        assert_eq!(oracle, want, "`{name}` on the oracle");
        assert_eq!(machine, oracle, "`{name}` answers alike");
    }
}

/// **Two handles on one vector, and the one that did not shrink it reads the
/// length and the elements across the truncate.**
///
/// A `Vector` is an owner every copy of it shares, so what an alias may
/// observe is the old committed length or the new one and nothing between:
/// there is no moment at which the elements have been cleared and the length
/// has not, because the truncate that does both is one instruction. The alias
/// is read *after* two truncates of different shapes — a `pop`, which vacates
/// the last element, and a `remove`, which moves the tail down over the hole
/// with a `run-copy` and then vacates the last — so a length that was lowered
/// without the move, or a move without the lowering, answers different
/// elements here.
#[test]
fn an_alias_reads_the_length_and_the_elements_a_truncate_left() {
    let source = "
export fn f() -> String {
  var a = Vector.of(1, 2, 3)
  let b = a
  let popped = a.pop()
  let removed = a.remove(0)
  \"{popped} {removed} {b.length()} {b} {b.get(0)} {b.get(1)}\"
}
";
    assert_eq!(
        agree(source, "f", vec![]),
        Answer::Value("Some(3) Some(1) 1 [2] Some(2) None".to_string())
    );
}

/// ADR 0062's append over a byte buffer — `core.bytesEnsure`, `core.bytesStore`
/// or `core.bytesCopy` at the length, `core.bytesCommit` — answers alike on
/// both evaluators: whole strings and single bytes through growths, a
/// character of several bytes appended a byte at a time, and the refusals the
/// machine words for a byte that is not one, a negative room and a run that is
/// not text at its finish. The oracle's staged suffix is what makes it a model
/// of the protocol: bytes written and not committed are in no length, a commit
/// of what was never written is refused, and so is a copy anywhere but the end
/// of what is staged.
#[test]
fn the_byte_append_protocol_agrees_and_the_oracle_publishes_only_what_was_written() {
    let probe = "\
/// Text and bytes onto a buffer with room for one, through `appendText`,
/// `appendByteInto` and the builder, and an interpolation of both.
export fn probeAppends() -> String {
  let buffer = core.bytesAllocate(1)
  var n = 0
  while n < 4 {
    appendText(buffer, \"hé\")
    appendByteInto(buffer, 108 + n)
    n = n + 1
  }
  // `ö` a byte at a time.
  appendByteInto(buffer, 195)
  appendByteInto(buffer, 182)
  appendText(buffer, \"\")
  let length = core.bytesLength(buffer)
  var out = StringBuilder.withCapacity(0)
  out.append(\"<\")
  out.appendByte(33)
  out.append(core.bytesFinish(buffer))
  \"{out.length()} {length} {out.finish()}!\"
}

/// A value that is not a byte.
export fn probeNotAByte() -> Int {
  let buffer = core.bytesAllocate(4)
  appendByteInto(buffer, 256)
  core.bytesLength(buffer)
}

/// An ensure of a negative room.
export fn probeNegative() -> Int {
  let buffer = core.bytesAllocate(4)
  core.bytesEnsure(buffer, -1)
  core.bytesLength(buffer)
}

/// Half a character, which is caught at the finish and not at the append.
export fn probeHalf() -> String {
  let buffer = core.bytesAllocate(4)
  appendText(buffer, \"a\")
  appendByteInto(buffer, 195)
  core.bytesFinish(buffer)
}
";
    let source = "\
use std.stringbuilder

export fn main() -> Int {
  1
}
";
    let wanted = [
        (
            "probeAppends",
            Answer::Value("20 18 <!h\u{e9}lh\u{e9}mh\u{e9}nh\u{e9}o\u{f6}!".to_string()),
        ),
        (
            "probeNotAByte",
            Answer::Failed("`runStore`'s value is `256`, and a byte is 0 to 255".to_string()),
        ),
        (
            "probeNegative",
            Answer::Failed(
                "`growableEnsure` was asked for room for -1 unit(s), and room is never negative"
                    .to_string(),
            ),
        ),
        (
            "probeHalf",
            Answer::Failed("this string's bytes are not valid UTF-8".to_string()),
        ),
    ];
    for (name, want) in wanted {
        let oracle = on_a_deep_stack(move || {
            let (sources, program) = checked_with_probe(source, "std.stringbuilder", probe);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts);
            said(Interpreter::new(&runtime).invoke("std.stringbuilder", name, vec![]))
        });
        let machine = on_a_deep_stack(move || {
            let (sources, program) = checked_with_probe(source, "std.stringbuilder", probe);
            let ir = lowered(&sources, &program);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts.clone());
            said(Vm::new(&runtime, &hosts, &ir).invoke("std.stringbuilder", name, vec![]))
        });
        assert_eq!(oracle, want, "`{name}` on the oracle");
        assert_eq!(machine, oracle, "`{name}` answers alike");
    }

    // What no verified lowering can express, so the oracle alone.
    let unlowerable = "\
/// The length before and after the commit of written bytes.
export fn probeInvisible() -> String {
  let buffer = core.bytesAllocate(2)
  let at = core.bytesLength(buffer)
  core.bytesEnsure(buffer, 2)
  core.bytesStore(buffer, at, 104)
  core.bytesCopy(buffer, at + 1, \"i\", 0, 1)
  let before = core.bytesLength(buffer)
  core.bytesCommit(buffer, 2)
  \"{before} {core.bytesLength(buffer)} {core.bytesFinish(buffer)}\"
}

/// A commit of bytes nobody wrote.
export fn probeUnwritten() -> Int {
  let buffer = core.bytesAllocate(2)
  core.bytesEnsure(buffer, 1)
  core.bytesCommit(buffer, 1)
  core.bytesLength(buffer)
}

/// A copy past the end of what is staged.
export fn probeGap() -> Int {
  let buffer = core.bytesAllocate(2)
  core.bytesEnsure(buffer, 2)
  core.bytesCopy(buffer, 1, \"i\", 0, 1)
  core.bytesLength(buffer)
}
";
    let wanted = [
        ("probeInvisible", Answer::Value("0 2 hi".to_string())),
        (
            "probeUnwritten",
            Answer::Failed(
                "`growableCommit` would publish 1 unit(s) onto a length of 0 with 0 written \
                 above it, and a commit publishes only units its window wrote"
                    .to_string(),
            ),
        ),
        (
            "probeGap",
            Answer::Failed(
                "`runCopy` writes 1 byte(s) to 1 of a buffer whose room begins at 0".to_string(),
            ),
        ),
    ];
    for (name, want) in wanted {
        let oracle = on_a_deep_stack(move || {
            let (sources, program) = checked_with_probe(source, "std.stringbuilder", unlowerable);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts);
            said(Interpreter::new(&runtime).invoke("std.stringbuilder", name, vec![]))
        });
        assert_eq!(oracle, want, "`{name}` on the oracle");
    }
}

/// `StringBuilder.appendSlice` decides its range in Cove and stops the run in
/// `String.sliceBytes`' words — the same words, on both evaluators, and the
/// same words `sliceBytes` itself answers.
///
/// ADR 0062 moved the range policy out of `core.bytesExtend`, which both
/// checked and copied, into `std.stringbuilder`'s `appendRange`, so that the
/// copy underneath is a write already known to be legal and can be the write
/// half of a reservation window. The risk that move carries is not the copy: it
/// is that a refusal changes. There are three copies of the rule now — the five
/// questions in Cove, `crate::builtins`' `wrong_byte_range` for the oracle and
/// `vm::intrinsics::text::refuse_byte_range` for the machine — and this is what
/// holds them together.
///
/// Every way a range can be wrong is here, and `sliceBytes`' own `Err` message
/// for the same range is compared against the sentence the builder stopped
/// with, rather than a literal being written twice: a paraphrase in either
/// copy fails here, and so does a reordering, because `"héllo"` at `(4, 3)` is
/// wrong in one way and `(2, 3)` in another.
#[test]
fn an_append_slice_refuses_a_range_in_slice_bytes_words_on_both_evaluators() {
    let probe = "\
/// The range appended to a builder, which stops the run when it is not one.
fn probeAppended(from: Int, to: Int) -> String {
  var out = StringBuilder.withCapacity(2)
  out.append(\"<\")
  out.appendSlice(\"héllo\", from, to)
  out.finish()
}

/// What `String.sliceBytes` says about the same range, which is what the
/// builder has to have said.
fn probeSaid(from: Int, to: Int) -> String {
  match \"héllo\".sliceBytes(from, to) {
    Ok(part) => part
    Err(error) => error.message
  }
}

/// A legal range: the copy runs and nothing is refused.
export fn probeOk() -> String {
  probeAppended(1, 3)
}

export fn probeFromBelow() -> String {
  probeAppended(-1, 1)
}

export fn probeToPastTheEnd() -> String {
  probeAppended(0, 7)
}

export fn probeBackwards() -> String {
  probeAppended(4, 3)
}

export fn probeFromInsideACharacter() -> String {
  probeAppended(2, 3)
}

export fn probeToInsideACharacter() -> String {
  probeAppended(1, 2)
}

export fn probeSaidOk() -> String {
  probeSaid(1, 3)
}

export fn probeSaidFromBelow() -> String {
  probeSaid(-1, 1)
}

export fn probeSaidToPastTheEnd() -> String {
  probeSaid(0, 7)
}

export fn probeSaidBackwards() -> String {
  probeSaid(4, 3)
}

export fn probeSaidFromInsideACharacter() -> String {
  probeSaid(2, 3)
}

export fn probeSaidToInsideACharacter() -> String {
  probeSaid(1, 2)
}
";
    let source = "\
use std.stringbuilder

export fn main() -> Int {
  1
}
";
    let answered = |name: &'static str| {
        let oracle = on_a_deep_stack(move || {
            let (sources, program) = checked_with_probe(source, "std.stringbuilder", probe);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts);
            said(Interpreter::new(&runtime).invoke("std.stringbuilder", name, vec![]))
        });
        let machine = on_a_deep_stack(move || {
            let (sources, program) = checked_with_probe(source, "std.stringbuilder", probe);
            let ir = lowered(&sources, &program);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts.clone());
            said(Vm::new(&runtime, &hosts, &ir).invoke("std.stringbuilder", name, vec![]))
        });
        assert_eq!(machine, oracle, "`{name}` answers alike");
        oracle
    };

    // A legal range copies, and `sliceBytes` answers the same bytes.
    assert_eq!(answered("probeOk"), Answer::Value("<é".to_string()));
    assert_eq!(answered("probeSaidOk"), Answer::Value("é".to_string()));

    for (appended, sliced) in [
        ("probeFromBelow", "probeSaidFromBelow"),
        ("probeToPastTheEnd", "probeSaidToPastTheEnd"),
        ("probeBackwards", "probeSaidBackwards"),
        ("probeFromInsideACharacter", "probeSaidFromInsideACharacter"),
        ("probeToInsideACharacter", "probeSaidToInsideACharacter"),
    ] {
        let Answer::Value(message) = answered(sliced) else {
            panic!("`{sliced}` answers the message rather than raising");
        };
        assert_eq!(
            answered(appended),
            Answer::Failed(message),
            "`{appended}` stops the run in `{sliced}`'s words"
        );
    }
}

/// #378 P4-5's keyed construction intrinsics answer alike on both evaluators:
/// a vector with exact room, ranges of an old set or map copied onto it around
/// a pushed unit, and the keyed finish into the new run — over one-word
/// members, `String` members and a map of two-word values. The old run is
/// untouched, and the machine lowers each to run instructions.
#[test]
fn the_keyed_construction_intrinsics_agree() {
    let probe = "\
/// A two-word value.
struct ProbeSpot {
  x: Int
  y: Int
}

/// One unit appended, as `std.vector.push` writes it: ADR 0062's ensure, store
/// and commit.
fn probePush<T>(items: Vector<T>, value: T) {
  let at = core.vectorLength(items)
  core.vectorEnsure(items, 1)
  core.vectorStore(items, at, value)
  core.vectorCommit(items, 1)
}

/// A range of a set appended, as `std.set`'s `copyFromSet` writes it: ADR
/// 0062's ensure, copy and commit.
fn probeCopySet<T>(items: Vector<T>, run: Set<T>, from: Int, count: Int) {
  let at = core.vectorLength(items)
  core.vectorEnsure(items, count)
  core.vectorCopyFromSet(items, at, run, from, count)
  core.vectorCommit(items, count)
}

/// `probeCopySet` over a map's entries, as `std.map`'s `copyFromMap` writes it.
fn probeCopyMap<K, V>(
  items: Vector<MapEntry<K, V>>,
  run: Map<K, V>,
  from: Int,
  count: Int,
) {
  let at = core.vectorLength(items)
  core.vectorEnsure(items, count)
  core.vectorCopyFromMap(items, at, run, from, count)
  core.vectorCommit(items, count)
}

/// `Set<Int>`: a member pushed between two ranges of the old run.
export fn probeSetInts() -> String {
  let old = Set.of(1, 3, 4)
  let out: Vector<Int> = core.vectorWithCapacity(4)
  probeCopySet(out, old, 0, 1)
  probePush(out, 2)
  probeCopySet(out, old, 1, 2)
  let built = core.setFinish(out)
  \"{built} {built.length()} {old}\"
}

/// `Set<String>`: a member at either end, and an empty range.
export fn probeSetStrings() -> String {
  let old = Set.of(\"b\", \"c\")
  let low: Vector<String> = core.vectorWithCapacity(3)
  probePush(low, \"a\")
  probeCopySet(low, old, 0, 2)
  let high: Vector<String> = core.vectorWithCapacity(3)
  probeCopySet(high, old, 0, 2)
  probePush(high, \"d\")
  probeCopySet(high, old, 2, 0)
  let empty: Vector<String> = core.vectorWithCapacity(0)
  \"{core.setFinish(low)} {core.setFinish(high)} {core.setFinish(empty)}\"
}

/// `Map<String, ProbeSpot>`: an entry replaced by skipping the old one.
export fn probeMap() -> String {
  let old = Map.of(
    MapEntry(key: \"a\", value: ProbeSpot(x: 1, y: 2)),
    MapEntry(key: \"b\", value: ProbeSpot(x: 3, y: 4)),
    MapEntry(key: \"c\", value: ProbeSpot(x: 5, y: 6)),
  )
  let out: Vector<MapEntry<String, ProbeSpot>> = core.vectorWithCapacity(3)
  probeCopyMap(out, old, 0, 1)
  probePush(out, MapEntry(key: \"b\", value: ProbeSpot(x: 9, y: 9)))
  probeCopyMap(out, old, 2, 1)
  let built = core.mapFinish(out)
  \"{built} {built.length()} {old}\"
}
";
    let source = "\
use std.set

export fn main() -> Int {
  1
}
";
    let (sources, checked) = checked_with_probe(source, "std.set", probe);
    let program = lowered(&sources, &checked);
    for name in ["probeSetInts", "probeSetStrings", "probeMap"] {
        let function = program
            .functions
            .iter()
            .find(|f| &*f.module == "std.set" && &*f.name == name)
            .unwrap_or_else(|| panic!("`{name}` is lowered"));
        let count = |keep: fn(&cove_ir::Inst) -> bool| {
            function.code.iter().filter(|inst| keep(inst)).count()
        };
        let finishes = count(|inst| matches!(inst, cove_ir::Inst::RunFinish { .. }));
        let copies = count(|inst| matches!(inst, cove_ir::Inst::RunCopy { .. }));
        assert!(
            finishes >= 1 && copies >= 1,
            "`{name}`: {:?}",
            function.code
        );
    }

    let wanted = [
        ("probeSetInts", "{1, 2, 3, 4} 4 {1, 3, 4}"),
        ("probeSetStrings", "{a, b, c} {b, c, d} {}"),
        (
            "probeMap",
            "{a: ProbeSpot(x: 1, y: 2), b: ProbeSpot(x: 9, y: 9), c: ProbeSpot(x: 5, y: 6)} 3 \
             {a: ProbeSpot(x: 1, y: 2), b: ProbeSpot(x: 3, y: 4), c: ProbeSpot(x: 5, y: 6)}",
        ),
    ];
    for (name, want) in wanted {
        let oracle = on_a_deep_stack(move || {
            let (sources, program) = checked_with_probe(source, "std.set", probe);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts);
            said(Interpreter::new(&runtime).invoke("std.set", name, vec![]))
        });
        let machine = on_a_deep_stack(move || {
            let (sources, program) = checked_with_probe(source, "std.set", probe);
            let ir = lowered(&sources, &program);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(program, sources, hosts.clone());
            said(Vm::new(&runtime, &hosts, &ir).invoke("std.set", name, vec![]))
        });
        assert_eq!(
            oracle,
            Answer::Value(want.to_string()),
            "`{name}` on the oracle"
        );
        assert_eq!(machine, oracle, "`{name}` answers alike");
    }
}
