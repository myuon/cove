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
//! The corpus here is written by hand rather than drawn from `tests/e2e`:
//! whether the lowering covers the repository's own programs is
//! `cove-cli/tests/lvm_coverage.rs`'s question, and it answers that the
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
    let program = lowered(&sources, &checked);
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
    let program = lowered(&sources, &checked);
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(checked.clone(), sources, hosts.clone());
    said(Lvm::new(&runtime, &hosts, &program).run_entry("m", name, Vec::new()))
}

fn lowered(sources: &SourceMap, checked: &Checked) -> cove_lir::Program {
    match cove_lir::lower(checked, sources, &cove_schema::HostSchemas::new()) {
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
/// What is reclaimed and when is `cove_runtime::lvm::cell`'s to show, because
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
