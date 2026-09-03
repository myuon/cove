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
//! about, and there is nothing but a host that can put a value at an erased
//! position from outside. An embedder's module is not a lesser kind of host,
//! so one is declared here and handed to the checker, the lowering and the
//! registry alike.
//!
//! One of its operations takes a callback, because that is where the shipped
//! `clock.timeout` differs from every other `Any` in the corpus and the
//! difference decides an answer: what goes into the box is a value that just
//! left this machine, so the family it left with is known and does not have
//! to be searched for. `oracle.bounded` is that operation reduced to the part
//! that matters — run the callback, wrap whatever it answered in `Ok`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_schema::{Effect, HostSchemas, HostType, ModuleSchema, OperationSchema};
use cove_sema::config::Config;
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program as Checked;

use crate::error::RuntimeError;
use crate::host::{Grants, HostApi, HostRegistry, Reentry};
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
            name: "nested",
            params: &[HostType::Int],
            variadic: false,
            result: HostType::Result(&HostType::Any, &HostType::Error),
            capability: "oracle",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        // `clock.timeout` with the clock taken out: the parameter is the
        // callback — an `Any` in a parameter position accepts every value —
        // and the result is the body's answer wrapped in an `Ok` the host
        // built. What crosses back in at the erased position is therefore a
        // value this run produced a moment earlier.
        OperationSchema {
            name: "bounded",
            params: &[HostType::Any],
            variadic: false,
            result: HostType::Result(&HostType::Any, &HostType::Error),
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

    /// `bounded` is the one operation that runs Cove, so it is the one that
    /// needs the way back. Everything else is [`HostApi::call`], which the
    /// default forwards to.
    fn call_with(
        &self,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        match op {
            // `Ok` whether the body succeeded or not, exactly as
            // `clock.timeout` wraps its body: the host's own `Result` is the
            // host's failure, and the body's is the body's.
            "bounded" => Ok(Value::ok(back.call(&args[0], Vec::new())?)),
            _ => self.call(op, args),
        }
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "number" => Ok(Value::int(
                args[0].as_int().expect("the schema declares an `Int`") * 10,
            )),
            "text" => Ok(Value::string("erased")),
            // The shape `clock.timeout { http.fetch(..) }` has and the
            // reason `runner.cove` writes a two-deep annotation: what the
            // schema promised to carry is itself a `Result`, so the box
            // holds an enum rather than a word.
            "nested" => {
                let n = args[0].as_int().expect("the schema declares an `Int`");
                if n < 0 {
                    Ok(Value::ok(Value::err(Value::error("inner"))))
                } else {
                    Ok(Value::ok(Value::ok(Value::int(n))))
                }
            }
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

/// The shape `examples/covecheck`'s `main.cove` is written in: a binding
/// whose annotation says what a schema's `Any` was carrying, and a `?` that
/// answers it.
///
/// Nothing at the `?` says anything — `clock.timeout` declares
/// `Result<Any, Error>` and this declares the same — so what says is the
/// annotation, several lines earlier, carried here by the checker. The box
/// is opened where the value is *used*, at the type the checker settled for
/// that use.
#[test]
fn an_annotation_says_what_a_question_mark_on_an_erased_result_answers() {
    assert_eq!(
        agree(
            "use oracle.attempt\n\
             export fn main() -> Result<Int, Error> {\n  \
               let bounded: Result<Int, Error> = attempt(41)\n  \
               let n = bounded?\n  \
               Ok(n + 1)\n\
             }"
        ),
        Answer::Value("Ok(43)".to_string())
    );
}

/// The shape `examples/covecheck`'s `runner.cove` is written in: a schema's
/// `Any` carrying a `Result` of its own, and a `match` inside a `match`.
///
/// This is the nesting, and what it pins is that no whole-value conversion
/// is needed for one. The outer `match` reads an ordinary enum — the box is
/// one word *inside* it — and the inner `match`'s subject is the box, opened
/// at the type the annotation gave it. One level at each level's use.
#[test]
fn an_annotation_says_what_a_result_inside_an_erased_result_is() {
    let source = "use oracle.nested\n\
                  export fn main() -> Result<Int, Error> {\n  \
                    let answer: Result<Result<Int, Error>, Error> = nested(7)\n  \
                    match answer {\n    \
                      Ok(inner) => match inner {\n      \
                        Ok(n) => Ok(n * 2)\n      \
                        Err(inside) => Ok(inside.message.length())\n    \
                      }\n    \
                      Err(outside) => Err(outside)\n  \
                    }\n\
                  }";
    assert_eq!(agree(source), Answer::Value("Ok(14)".to_string()));
    assert_eq!(
        agree(&source.replace("nested(7)", "nested(0 - 1)")),
        Answer::Value("Ok(5)".to_string())
    );
}

/// The same program with one thing changed, which is the thing that used to
/// decide whether it ran.
///
/// A box carries the layout of what was put in it, and what puts one there
/// on a Host answer is a search of the program's *families* for one the
/// value fits. `Result<Int, Error>` and `Result<Any, Error>` both fit
/// `Ok(7)` — a `Shape::Boxed` position admits everything — so which is found
/// used to depend on which the lowering happened to intern first. Above it
/// is `Result<Int, Error>`, because that is what `main` returns; here `main`
/// answers a `String`, so the only reason the described family is in the
/// table at all is the `unbox` the inner `match` needed, and it is interned
/// after the erasing one.
///
/// The search prefers the family that describes the value, so both run. See
/// `boundary::Precision`.
#[test]
fn the_family_a_box_records_is_the_one_that_describes_the_value() {
    let source = "use oracle.nested\n\
                  export fn main() -> Result<String, Error> {\n  \
                    let answer: Result<Result<Int, Error>, Error> = nested(7)\n  \
                    match answer {\n    \
                      Ok(inner) => match inner {\n      \
                        Ok(n) => Ok(\"{n}\")\n      \
                        Err(inside) => Ok(inside.message)\n    \
                      }\n    \
                      Err(outside) => Err(outside)\n  \
                    }\n\
                  }";
    assert_eq!(agree(source), Answer::Value("Ok(7)".to_string()));
}

/// The shape `examples/covecheck`'s `runner.cove` is written in, with the
/// thing that used to decide whether it ran: **another family that describes
/// the same value, interned first.**
///
/// `Err(Error("no"))` is a `Result<String, Error>` here and it is also a
/// perfectly good `Result<Int, Error>`, which is what `count` puts in the
/// table one entry earlier. Both describe the value exactly, so
/// `boundary::Precision` cannot choose between them and neither can anything
/// else that reads the value alone — and they are different runs of words, so
/// the box that records the wrong one traps at the `Unbox` the inner `match`
/// emits.
///
/// The helper is called `count` and not something else on purpose: which of
/// the two is interned first is what used to decide whether this program ran,
/// and this name is one that puts `Result<Int, Error>` first. That the name
/// of a function nothing in the program depends on could decide the answer is
/// the whole complaint.
///
/// What decides it is that the value in the box is the *callback's* answer.
/// It left this machine one instruction earlier at the layout the callback's
/// return type fixed, so the family is a static fact rather than a search;
/// `exec::Machine::callback_answer` is where the way out writes it down and
/// `boundary::held_layout` is what prefers it.
#[test]
fn a_box_built_from_a_callback_answer_records_the_family_the_callback_returned() {
    let source = "use oracle.bounded\n\
                  fn count() -> Result<Int, Error> {\n  \
                    Ok(1)\n\
                  }\n\
                  fn inner(n: Int) -> Result<String, Error> {\n  \
                    if n < 0 {\n    \
                      return Err(Error(\"no\"))\n  \
                    }\n  \
                    Ok(\"yes\")\n\
                  }\n\
                  export fn main() -> Result<Int, Error> {\n  \
                    let n = count()?\n  \
                    let answer: Result<Result<String, Error>, Error> = bounded(fn() {\n    \
                      inner(0 - n)\n  \
                    })\n  \
                    match answer {\n    \
                      Ok(held) => match held {\n      \
                        Ok(text) => Ok(text.length())\n      \
                        Err(inside) => Ok(inside.message.length())\n    \
                      }\n    \
                      Err(outside) => Err(outside)\n  \
                    }\n\
                  }";
    assert_eq!(agree(source), Answer::Value("Ok(2)".to_string()));
    assert_eq!(
        agree(&source.replace("inner(0 - n)", "inner(n)")),
        Answer::Value("Ok(3)".to_string())
    );
}
