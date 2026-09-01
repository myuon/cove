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

use crate::budget::{Budget, Limits};
use crate::host::{Grants, HostRegistry};
use crate::interp::Interpreter;
use crate::lvm::boundary;
use crate::lvm::exec::Machine;
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
    match Interpreter::new(&runtime).invoke("m", name, args) {
        Ok(value) => Answer::Value(format!("{value}")),
        Err(error) => Answer::Failed(error.message),
    }
}

/// Runs `m.<name>` on the linear-memory machine.
fn on_the_machine(source: &str, name: &str, args: Vec<Value>) -> Answer {
    let (_sources, checked) = checked(source);
    let program = match cove_lir::lower(&checked) {
        Ok(program) => program,
        Err(items) => panic!(
            "the program lowers:\n{}",
            items
                .iter()
                .map(|item| item.message.clone())
                .collect::<Vec<_>>()
                .join("\n")
        ),
    };
    let entry = program
        .function_named("m", name)
        .unwrap_or_else(|| panic!("`{name}` was lowered"));
    let function = program.function(entry);
    let returns = function.returns;
    let params: Vec<cove_lir::Repr> = function.reprs[..function.arity as usize].to_vec();

    let mut machine = Machine::new(&program, 1 << 16);
    let mut words = Vec::new();
    for (repr, value) in params.iter().zip(&args) {
        match boundary::from_value(&mut machine, *repr, value) {
            Ok(word) => words.push(word),
            Err(error) => return Answer::Failed(error.message),
        }
    }
    match machine.run(entry, &words, &Budget::new(Limits::default())) {
        Ok(word) => match boundary::to_value(&machine, returns, word) {
            Ok(value) => Answer::Value(format!("{value}")),
            Err(error) => Answer::Failed(error.message),
        },
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
