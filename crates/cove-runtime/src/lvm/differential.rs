//! The linear-memory backend against the semantic oracle.
//!
//! [ADR 0034](../../../../docs/adr/0034-one-physical-word-stack.md) keeps the
//! tree-walking interpreter as the definition of what a Cove program means,
//! and makes the replacement's completion conditional on agreeing with it.
//! This is where that agreement is checked at the level of one source program
//! at a time, from the source text through the checker, the lowering and the
//! machine, and compared against the same source run on the interpreter.
//!
//! It is deliberately not a listing test. `cove-lir`'s own suite pins what
//! each construct lowers to, and this asks a different question: whatever it
//! lowered to, does running it answer what the language says? A case here
//! that fails while the listing tests pass is a machine bug; the other way
//! round is a lowering bug; both failing is a shared misreading of the
//! checker.
//!
//! The corpus is small because the lowering's scope is. It grows with the
//! lowering, and the whole `tests/e2e` corpus joins it once there is a
//! boundary and a Host call to run it through.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_sema::config::Config;
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program as Checked;

use crate::host::{Grants, HostRegistry};
use crate::interp::Interpreter;
use crate::lvm::Lvm;
use crate::runtime::Runtime;
use crate::value::Value;

/// Parses, resolves and checks one module called `m`.
fn checked(source: &str) -> (Arc<SourceMap>, Arc<Checked>) {
    let mut sources = SourceMap::new();
    let file = sources.add("m/main.cove", source.to_string());
    let ast = match cove_syntax::parse_file(&sources, file) {
        Ok(ast) => ast,
        Err(items) => panic!("the source parses:\n{}", rendered(&sources, &items)),
    };
    let package = Package {
        root: PathBuf::from("."),
        config: Config::default(),
        modules: BTreeMap::from([(
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
        )]),
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
/// Through [`Lvm`] rather than through [`crate::lvm::exec::Machine`], because
/// the question this file asks is about the language and the language's
/// answer includes the boundary: the same argument check, the same
/// materialisation, the same terminal event. A comparison that skipped them
/// would be comparing the loop against the whole of the oracle.
fn on_the_machine(source: &str, name: &str, args: Vec<Value>) -> Answer {
    let (sources, checked) = checked(source);
    let program = lowered(&checked);
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(checked.clone(), sources, hosts.clone());
    said(Lvm::new(&runtime, &hosts, &program).invoke("m", name, args))
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
    let program = lowered(&checked);
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(checked.clone(), sources, hosts.clone());
    said(Lvm::new(&runtime, &hosts, &program).run_entry("m", name, Vec::new()))
}

fn lowered(checked: &Checked) -> cove_lir::Program {
    match cove_lir::lower(checked) {
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
fn on_a_deep_stack(f: impl FnOnce() -> Answer + Send + 'static) -> Answer {
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
/// oracle in `Display for Value`, the machine in `lvm::builtins` — because
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
