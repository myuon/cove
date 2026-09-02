//! A value a Host schema declared `Any`, on both backends.
//!
//! `docs/LINEAR_VM.md` gives an intentionally erased value one
//! representation — "one `Ref` word naming a `Boxed` object" — and
//! `cove-lir`'s own suite pins what each construct lowers to. This asks the
//! other half of the question: whatever it lowered to, does running it answer
//! what the language says?
//!
//! It needs a module of its own rather than a case in [`super::differential`]
//! because a schema that declares `Any` in a *result* is what the case is
//! about, and no shipped module declares one a program can reach without also
//! handing the host a closure — which is a boundary neither side crosses yet.
//! An embedder's module is not a lesser kind of host, so one is declared here
//! and handed to the checker, the lowering and the registry alike.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_schema::{Effect, HostSchemas, HostType, ModuleSchema, OperationSchema};
use cove_sema::config::Config;
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program as Checked;

use crate::error::RuntimeError;
use crate::host::{Grants, HostApi, HostRegistry};
use crate::interp::Interpreter;
use crate::lvm::Lvm;
use crate::runtime::Runtime;
use crate::value::Value;

/// A host whose answers are the program's business and not its own.
///
/// `number` and `text` both declare [`HostType::Any`], so what comes back is
/// a value no schema described: the checker records an unconstrained unknown,
/// the lowering reads the schema and boxes, and what is in the box is decided
/// here at run time. `attempt` is the same fact nested one deep —
/// `Result<Any, Error>`, which is the shape `clock.timeout` declares.
const ORACLE: ModuleSchema = ModuleSchema {
    name: "oracle",
    capability: "oracle",
    operations: &[
        OperationSchema {
            name: "number",
            params: &[HostType::Int],
            variadic: false,
            result: HostType::Any,
            capability: "oracle",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "text",
            params: &[],
            variadic: false,
            result: HostType::Any,
            capability: "oracle",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "attempt",
            params: &[HostType::Int],
            variadic: false,
            result: HostType::Result(&HostType::Any, &HostType::Error),
            capability: "oracle",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ],
    types: &[],
    resources: &[],
};

struct Oracle;

impl HostApi for Oracle {
    fn module_schema(&self) -> ModuleSchema {
        ORACLE
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "number" => Ok(Value::int(
                args[0].as_int().expect("the schema declares an `Int`") * 10,
            )),
            "text" => Ok(Value::string("erased")),
            "attempt" => {
                let n = args[0].as_int().expect("the schema declares an `Int`");
                if n < 0 {
                    Ok(Value::err(Value::error("no")))
                } else {
                    Ok(Value::ok(Value::int(n + 1)))
                }
            }
            _ => unreachable!("checked by `HostRegistry::call`"),
        }
    }
}

fn schemas() -> HostSchemas {
    HostSchemas::new().with(ORACLE)
}

fn hosts() -> Arc<HostRegistry> {
    let mut held = HostRegistry::new(Grants::new(vec!["oracle"]));
    held.register(Box::new(Oracle));
    Arc::new(held)
}

/// Parses, resolves and checks one module called `m`, against [`ORACLE`] as
/// well as the shipped schemas.
fn checked(source: &str) -> (Arc<SourceMap>, Arc<Checked>) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("m/main.cove");
    let file = sources.add(path.clone(), source.to_string());
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
                units: vec![Unit { file, path, ast }],
            },
        )]),
    };
    match cove_sema::Compiler::new()
        .with_schemas(schemas())
        .compile(&package)
    {
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
#[derive(Debug, PartialEq)]
enum Answer {
    Value(String),
    Failed(String),
}

fn said(outcome: Result<Value, RuntimeError>) -> Answer {
    match outcome {
        Ok(value) => Answer::Value(format!("{value}")),
        Err(error) => Answer::Failed(error.message),
    }
}

fn on_the_oracle(source: &str) -> Answer {
    let (sources, program) = checked(source);
    let hosts = hosts();
    let runtime = Runtime::new(program.clone(), sources, hosts.clone());
    said(Interpreter::new(&runtime).run_entry("m", "main", Vec::new()))
}

fn on_the_machine(source: &str) -> Answer {
    let (sources, checked) = checked(source);
    let program = match cove_lir::lower(&checked, &sources, &schemas()) {
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
    let hosts = hosts();
    let runtime = Runtime::new(checked.clone(), sources, hosts.clone());
    said(Lvm::new(&runtime, &hosts, &program).run_entry("m", "main", Vec::new()))
}

/// Runs `f` on a thread with the stack the interpreter documents, for
/// [`super::differential`]'s reason: the oracle is a tree walk and an `Rc`
/// does not cross a thread, so only what each backend *said* comes back.
fn on_a_deep_stack(f: impl FnOnce() -> Answer + Send + 'static) -> Answer {
    std::thread::Builder::new()
        .stack_size(crate::interp::STACK_SIZE)
        .spawn(f)
        .expect("a thread for the run")
        .join()
        .expect("the run did not panic")
}

#[track_caller]
fn agree(source: &str) -> Answer {
    let oracle = {
        let source = source.to_string();
        on_a_deep_stack(move || on_the_oracle(&source))
    };
    let machine = {
        let source = source.to_string();
        on_a_deep_stack(move || on_the_machine(&source))
    };
    assert_eq!(
        machine, oracle,
        "the machine and the interpreter do not agree about:\n{source}"
    );
    oracle
}

/// The whole of the shape `benches/convention`'s `hostReentry` is written in,
/// with the closure taken out: a host answer a schema declared `Any`, bound
/// to a name, and arithmetic on it afterwards.
#[test]
fn arithmetic_on_a_value_a_schema_declared_any() {
    assert_eq!(
        agree(
            "use oracle.number\n\
             export fn main() -> Result<Int, Error> {\n  \
               let v = number(4)\n  \
               Ok(v % 7 + 1)\n\
             }"
        ),
        Answer::Value("Ok(6)".to_string())
    );
}

/// `?` on a `Result<Any, Error>`, both ways it can go.
///
/// The `Ok` carries a box and the `Err` carries an `Error`, so the two cases
/// of one enum have payloads of different families — which is exactly what a
/// payload region with a static reference map has to get right.
#[test]
fn a_question_mark_on_a_result_whose_ok_is_erased() {
    assert_eq!(
        agree(
            "use oracle.attempt\n\
             export fn main() -> Result<Int, Error> {\n  \
               let v = attempt(41)?\n  \
               Ok(v + 1)\n\
             }"
        ),
        Answer::Value("Ok(43)".to_string())
    );
    assert_eq!(
        agree(
            "use oracle.attempt\n\
             export fn main() -> Result<Int, Error> {\n  \
               let v = attempt(-1)?\n  \
               Ok(v + 1)\n\
             }"
        ),
        Answer::Value("Err(no)".to_string())
    );
}

/// An erased value read rather than computed with: the box is handed over
/// whole, and the boundary looks through it.
///
/// Nothing here says what is inside, and nothing has to — interpolation
/// renders whatever the value is, so this is the one use of an erased value
/// that needs no type at all.
#[test]
fn an_erased_value_renders_as_what_is_in_the_box() {
    assert_eq!(
        agree(
            "use oracle.text\n\
             use oracle.number\n\
             export fn main() -> Result<String, Error> {\n  \
               Ok(\"{text()} {number(3)}\")\n\
             }"
        ),
        Answer::Value("Ok(erased 30)".to_string())
    );
}

/// Where the two backends stop at the same place and do not say the same
/// words, which is written down rather than claimed away.
///
/// The box holds a `String` and the program does arithmetic on it. Both
/// backends fail, at the same expression, and neither produces a value — but
/// the oracle fails *at the operator*, where it can name both operand types
/// and the operator, and the machine fails one instruction earlier at the
/// `Unbox`, which knows the layout it found and not what asked for it.
///
/// `cove_lir::Inst::Trap` carries a static message and `Inst::Unbox` raises
/// its own, so closing this means an instruction that can build a message out
/// of two layout names and an operator — which is a change to the instruction
/// set, not to this file. Until then this is the divergence, pinned so that
/// it is a decision rather than a surprise.
#[test]
fn the_two_backends_word_a_failed_unboxing_differently() {
    let source = "use oracle.text\n\
                  export fn main() -> Result<Int, Error> {\n  \
                    Ok(text() + 1)\n\
                  }";
    let oracle = {
        let source = source.to_string();
        on_a_deep_stack(move || on_the_oracle(&source))
    };
    let machine = {
        let source = source.to_string();
        on_a_deep_stack(move || on_the_machine(&source))
    };
    assert_eq!(
        oracle,
        Answer::Failed("`+` is not defined for `String` and `Int`".to_string())
    );
    assert_eq!(
        machine,
        Answer::Failed("this value is not of the type it is being read as".to_string())
    );
}
