//! What the eight-byte frame makes of a program, checked against both of the
//! backends that already exist.
//!
//! Every case here is three-way: the same checked program runs on the tree
//! walk, on the `Vm`, and on the [`FrameVm`], and all three must agree about
//! the value or the error, the message, and the span the error points at.
//! ADR 0012 ranks the specification above the oracle above a backend, so a
//! test that said what the frame *should* answer would be a test of what
//! somebody expected; what these say is that the three must not disagree.
//!
//! Issue #212's correctness list is the outline: `i64::MIN` and `i64::MAX`,
//! overflow and division by zero, canonical `Bool` and the rejection of a
//! non-canonical word, `Float` bit patterns including NaN payloads, nested
//! calls and the call-depth limit, and a stack that grows past its initial
//! capacity while indices into it stay valid.
//!
//! # The five that are `#[ignore]`d, and what runs them
//!
//! Five cases here drive the `benches/` package, and a bench row is written
//! to be *measured*: `benches/arith` and every row shaped like it run
//! 2,000,000 turns, because that is the number the figures in
//! `docs/VM_ARCHITECTURE.md` were taken at. Running one of those on the tree
//! walk, on the `Vm`, and on the `FrameVm` — unoptimised, which is what a
//! test binary is — costs between thirty and fifty seconds each, and
//! together they were the whole of `cove-runtime`'s test time and about two
//! fifths of the workspace's.
//!
//! Shortening the rows was refused rather than not considered. The turn count
//! is what the published measurement means, so a row that ran fewer turns
//! under test would be a different program from the one the numbers are
//! about, and the claim these cases make — that the three backends agree
//! about *the programs that get benchmarked* — is the one that would be lost.
//! Bounding the run with fuel was refused for the same kind of reason: two of
//! the five count something over a whole run, and a prefix of a run does not
//! answer a question about its total.
//!
//! So they are `#[ignore]`d and CI runs them, which is the split ADR 0012's
//! ranking permits: nothing here is the specification, and none of the five
//! is the only thing standing between a wrong frame and a green suite — the
//! forty-three cases that stay are the ones written against the invariants
//! directly. `cargo test --workspace --lib -- --ignored` is what runs them,
//! and it is worth running by hand after a change to `frame.rs` rather than
//! waiting for CI to say so.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cove_diag::{SourceMap, Span};
use cove_ir::Program as Ir;
use cove_schema::{Effect, HostType, ModuleSchema, OperationSchema};
use cove_sema::config::Config;
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program as Checked;

use super::*;
use crate::budget::{Budget, Cancellation, Limits};
use crate::clock::{Clock, VirtualTime};
use crate::host::{Console, Grants, HostApi};
use crate::interp::Interpreter;
use crate::trace::{TraceEvent, TraceSink};
use crate::vm::Vm;

// ------------------------------------------------------------- the harness

/// What one backend made of one program: the value it answered rendered so
/// it can be compared, or the error with the span it points at.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Answered(String),
    Raised { message: String, span: Option<Span> },
}

fn outcome(answer: Result<Value, RuntimeError>) -> Outcome {
    match answer {
        Ok(value) => Outcome::Answered(format!("{value:?}")),
        Err(error) => Outcome::Raised {
            message: error.message.clone(),
            span: error.span,
        },
    }
}

fn hosts(limits: Option<Limits>, cancellation: Option<Cancellation>) -> Arc<HostRegistry> {
    let mut hosts = HostRegistry::new(Grants::new(vec!["console", "clock"]));
    hosts.register(Box::new(Console::new(std::io::sink(), std::io::sink())));
    hosts.register(Box::new(Clock::virtual_clock(VirtualTime::new())));
    match (limits, cancellation) {
        (Some(limits), Some(flag)) => hosts.set_budget(Budget::with_cancellation(limits, flag)),
        (Some(limits), None) => hosts.set_budget(Budget::new(limits)),
        (None, Some(flag)) => hosts.set_budget(Budget::with_cancellation(Limits::default(), flag)),
        (None, None) => {}
    }
    Arc::new(hosts)
}

// -------------------------------------------------------- a Host call fixture

/// A host module built only for this file's Host-call tests, over the
/// arguments `Operands::boundary` admits and nothing else: a zero-argument
/// operation, one operation per scalar kind `cove_schema::HostType` has --
/// which is `Int` and `Bool`; it has no `Float` at all, so a `Float`
/// argument is exercised through `many` instead, which takes `Any` -- one for
/// a `String`, a variadic one that counts what it was given regardless of
/// kind, and one that always fails. None of the shipped modules exercises all
/// of these through operations this small, and the shipped ones' own
/// arguments are mostly `String`.
const ECHO_SCHEMA: ModuleSchema = ModuleSchema {
    name: "echo",
    capability: "echo",
    operations: &[
        OperationSchema {
            name: "unit",
            params: &[],
            variadic: false,
            result: HostType::Result(&HostType::Unit, &HostType::Error),
            capability: "echo",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "int",
            params: &[HostType::Int],
            variadic: false,
            result: HostType::Result(&HostType::Int, &HostType::Error),
            capability: "echo",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "bool",
            params: &[HostType::Bool],
            variadic: false,
            result: HostType::Result(&HostType::Bool, &HostType::Error),
            capability: "echo",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "text",
            params: &[HostType::String],
            variadic: false,
            result: HostType::Result(&HostType::String, &HostType::Error),
            capability: "echo",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "many",
            params: &[HostType::Any],
            variadic: true,
            result: HostType::Result(&HostType::Int, &HostType::Error),
            capability: "echo",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "fails",
            params: &[],
            variadic: false,
            result: HostType::Result(&HostType::Unit, &HostType::Error),
            capability: "echo",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ],
    types: &[],
    resources: &[],
};

/// The host module [`ECHO_SCHEMA`] declares. `unit()` answers `Ok(())`;
/// `int`, `bool` and `text` answer `Ok` of the one argument they were given,
/// unchanged; `many(...)` answers how many arguments it was given, whatever
/// kind each one is; `fails()` always answers a declared failure.
struct Echo;

impl HostApi for Echo {
    fn module_schema(&self) -> ModuleSchema {
        ECHO_SCHEMA
    }

    fn call(&self, op: &str, mut args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "unit" => Ok(Value::ok(Value::unit())),
            "int" | "bool" | "text" => {
                let value = args.pop().expect("this operation takes one argument");
                Ok(Value::ok(value))
            }
            "many" => Ok(Value::ok(Value::int(args.len() as i64))),
            "fails" => Ok(Value::err(Value::error("echo.fails always fails"))),
            _ => unreachable!("checked by `HostRegistry::call`"),
        }
    }
}

/// A registry holding only [`Echo`], granted the `echo` capability when
/// `grant` is true and not otherwise -- the fixture the capability-denied
/// test runs the same program through twice.
fn echo_hosts(grant: bool) -> Arc<HostRegistry> {
    let capabilities: Vec<&str> = if grant { vec!["echo"] } else { Vec::new() };
    let mut hosts = HostRegistry::new(Grants::new(capabilities));
    hosts.register(Box::new(Echo));
    Arc::new(hosts)
}

/// [`agree`], with [`echo_hosts`] in place of the shared console-and-clock
/// registry [`hosts`] builds -- every Host-call test below runs its program
/// through this rather than through `agree`, because `agree` has no way to
/// grant a capability `hosts` does not already grant, or to withhold one it
/// does.
fn agree_with_echo(source: &str, grant: bool) -> Outcome {
    let ready = ready(source);
    let run = |backend: &str| -> Outcome {
        let hosts = echo_hosts(grant);
        let runtime = Runtime::new(ready.checked.clone(), ready.sources.clone(), hosts.clone());
        match backend {
            "ast" => {
                outcome(Interpreter::new(&runtime).run_entry(&ready.module, "main", Vec::new()))
            }
            "vm" => outcome(Vm::new(&runtime, &hosts, &ready.ir).run_entry(
                &ready.module,
                "main",
                Vec::new(),
            )),
            _ => outcome(FrameVm::new(&runtime, &hosts, &ready.ir).run_entry(
                &ready.module,
                "main",
                Vec::new(),
            )),
        }
    };
    let ast = run("ast");
    let vm = run("vm");
    let frame = run("frame");
    assert_eq!(ast, vm, "the oracle and the VM disagreed for:\n{source}");
    assert_eq!(
        vm, frame,
        "the VM and the 8-byte frame disagreed for:\n{source}"
    );
    frame
}

/// One program, checked and lowered, ready to be run three ways.
struct Ready {
    sources: Arc<SourceMap>,
    checked: Arc<Checked>,
    ir: Arc<Ir>,
    module: String,
}

impl Ready {
    fn on_ast(&self) -> Outcome {
        let hosts = hosts(None, None);
        let runtime = Runtime::new(self.checked.clone(), self.sources.clone(), hosts);
        outcome(Interpreter::new(&runtime).run_entry(&self.module, "main", Vec::new()))
    }

    fn on_vm(&self) -> Outcome {
        let hosts = hosts(None, None);
        let runtime = Runtime::new(self.checked.clone(), self.sources.clone(), hosts.clone());
        outcome(Vm::new(&runtime, &hosts, &self.ir).run_entry(&self.module, "main", Vec::new()))
    }

    fn on_frame(&self) -> Outcome {
        self.on_frame_with(None, None).0
    }

    fn on_frame_with(
        &self,
        limits: Option<Limits>,
        cancellation: Option<Cancellation>,
    ) -> (Outcome, u64, u64) {
        let hosts = hosts(limits, cancellation);
        let runtime = Runtime::new(self.checked.clone(), self.sources.clone(), hosts.clone());
        let mut frame = FrameVm::new(&runtime, &hosts, &self.ir);
        let answered = frame.run_entry(&self.module, "main", Vec::new());
        (
            outcome(answered),
            frame.instructions(),
            frame.materialized(),
        )
    }

    /// The same run, reporting what the traced heap did as well.
    fn on_frame_measured(&self) -> (Outcome, u64, HeapStats) {
        let hosts = hosts(None, None);
        let runtime = Runtime::new(self.checked.clone(), self.sources.clone(), hosts.clone());
        let mut frame = FrameVm::new(&runtime, &hosts, &self.ir);
        let answered = frame.run_entry(&self.module, "main", Vec::new());
        (outcome(answered), frame.materialized(), frame.heap_stats())
    }

    /// The same run with the traced heap collecting at **every** safepoint,
    /// reporting what the collections found.
    ///
    /// `scope` is the mutation knob: [`RootScope::EveryWord`] is what a run
    /// does, and the other two are the two halves of the rooting removed one
    /// at a time. Stress is on for `crate::slot::HandleHeap::stress`'s reason
    /// — which safepoint a collection lands on is otherwise an accident of
    /// what the program allocated, and a rooting test that depends on that
    /// accident is a test that passes by luck.
    fn on_frame_collecting(&self, scope: RootScope) -> Collected {
        self.on_frame_collecting_with(scope, FieldMap::TheLoweredType)
    }

    /// The same, with the third mutation knob: whether a field read's bit
    /// comes from the lowered type at all. See [`FieldMap`].
    fn on_frame_collecting_with(&self, scope: RootScope, fields: FieldMap) -> Collected {
        let hosts = hosts(None, None);
        let runtime = Runtime::new(self.checked.clone(), self.sources.clone(), hosts.clone());
        let mut frame = FrameVm::new(&runtime, &hosts, &self.ir);
        frame.stress();
        if fields == FieldMap::Dropped {
            frame.without_the_field_map();
        }
        frame.scope = scope;
        let answered = frame.run_entry(&self.module, "main", Vec::new());
        let (collections, roots_yielded, expansions) = frame.collections();
        Collected {
            outcome: outcome(answered),
            collections,
            roots_yielded,
            expansions,
            marked: frame.marked,
            most_roots_at_once: frame.most_roots_at_once,
            most_expansions_at_once: frame.most_expansions_at_once,
            rooted_outside_the_stack: frame.rooted_outside_the_stack(),
            allocated_objects: frame.heap_stats().allocated_objects,
            freed_objects: frame.heap_stats().freed_objects,
        }
    }

    fn on_vm_with(&self, limits: Option<Limits>, cancellation: Option<Cancellation>) -> Outcome {
        let hosts = hosts(limits, cancellation);
        let runtime = Runtime::new(self.checked.clone(), self.sources.clone(), hosts.clone());
        outcome(Vm::new(&runtime, &hosts, &self.ir).run_entry(&self.module, "main", Vec::new()))
    }

    fn vm_instructions(&self) -> u64 {
        let hosts = hosts(None, None);
        let runtime = Runtime::new(self.checked.clone(), self.sources.clone(), hosts.clone());
        let mut vm = Vm::new(&runtime, &hosts, &self.ir);
        let _ = vm.run_entry(&self.module, "main", Vec::new());
        vm.instructions()
    }

    fn admitted(&self) -> Result<FunctionId, Refused> {
        admits(&self.ir, &self.module, "main")
    }
}

/// What one run's collections did, which is how a rooting claim about a whole
/// program is checked rather than asserted.
#[derive(Debug)]
struct Collected {
    outcome: Outcome,
    /// How many collections actually ran. Asserted nonzero everywhere, because
    /// a rooting test over a run that never collected is vacuous.
    collections: u64,
    /// ADR 0028 decision 8's first multiplicity, summed: root storage
    /// locations yielded.
    roots_yielded: u64,
    /// Its third, summed: objects the mark phase expanded.
    expansions: u64,
    /// Objects the mark phase found live, summed over the same collections.
    /// Equal to `expansions` whatever the graph, which is what "expanded once"
    /// means.
    marked: u64,
    /// The most root storage locations any single collection yielded.
    most_roots_at_once: u64,
    /// The most objects any single collection expanded.
    most_expansions_at_once: u64,
    rooted_outside_the_stack: usize,
    allocated_objects: u64,
    freed_objects: u64,
}

/// Parses, checks and lowers `source` as module `m`.
fn ready(source: &str) -> Ready {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("m/main.cove");
    let file = sources.add(path.clone(), source);
    let ast = match cove_syntax::parse_file(&sources, file) {
        Ok(ast) => ast,
        Err(items) => panic!("the source parses:\n{}", rendered(&sources, &items)),
    };
    let package = Package {
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
    };
    prepared(sources, package, "m")
}

/// The same, for one module of the `benches/` package.
fn bench(name: &str) -> Ready {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benches");
    let mut sources = SourceMap::new();
    let mut package = match cove_sema::package::load(&root, &mut sources) {
        Ok(package) => package,
        Err(items) => panic!("the benches package loads:\n{}", rendered(&sources, &items)),
    };
    let module = package
        .modules
        .remove(name)
        .unwrap_or_else(|| panic!("`benches/{name}` is a module of the package"));
    package.modules = BTreeMap::from([(name.to_string(), module)]);
    prepared(sources, package, name)
}

fn prepared(sources: SourceMap, package: Package, module: &str) -> Ready {
    // `ECHO_SCHEMA` is added to every checker this file builds, whether or
    // not a given source uses it, the same way the shipped schemas are
    // always there: registering a description does not grant a capability or
    // require a call, so a program that never writes `use echo` checks
    // exactly as it did before this line existed.
    let checked = match cove_sema::Compiler::new()
        .with_host_schema(ECHO_SCHEMA)
        .compile(&package)
    {
        Ok(program) => program,
        Err(items) => panic!("the source checks:\n{}", rendered(&sources, &items)),
    };
    let ir = match cove_ir::lower::lower(&checked) {
        Ok(program) => program,
        Err(why) => panic!("the program lowers, but stopped at {why}"),
    };
    cove_ir::lower::validate(&ir)
        .unwrap_or_else(|why| panic!("the lowering holds the invariants: {why}"));
    Ready {
        sources: Arc::new(sources),
        checked: Arc::new(checked),
        ir: Arc::new(ir),
        module: module.to_string(),
    }
}

fn rendered(sources: &SourceMap, items: &[cove_diag::Diagnostic]) -> String {
    items
        .iter()
        .map(|item| cove_diag::render(sources, item))
        .collect::<Vec<_>>()
        .join("\n")
}

/// **The differential test.** Runs `source` on all three backends and asserts
/// they agree about the answer, the message, and the span.
fn agree(source: &str) -> Ready {
    let ready = ready(source);
    // On a stack the runtime sized: the oracle is a recursive tree walker and
    // a test thread's stack is not one it chose. The other two run beside it
    // so that the three see the same conditions.
    let (ast, vm, frame) =
        crate::on_cove_stack(|| (ready.on_ast(), ready.on_vm(), ready.on_frame()))
            .expect("a thread to run Cove on");
    assert_eq!(ast, vm, "the oracle and the VM disagreed for:\n{source}");
    assert_eq!(
        vm, frame,
        "the VM and the 8-byte frame disagreed for:\n{source}"
    );
    ready
}

/// A `main` written around `body`, answering `ty`.
fn main_of(ty: &str, body: &str) -> String {
    format!("export fn main() -> {ty} {{\n{body}\n}}\n")
}

// ------------------------------------------------------- the two bench rows

/// The rows this backend runs.
///
/// `arith` and `call` are Phase A's, whose frames hold no reference at all;
/// `field` and `method` are Phase B's, whose frames do. The pair matters more
/// than either: `call` minus `arith` prices a call over a frame of scalars and
/// `method` minus `field` prices one over a frame that has to be walked, and
/// the difference between those two differences is what rooting costs a call.
const ADMITTED_ROWS: [&str; 4] = ["arith", "call", "field", "method"];

/// Issue #212's first acceptance criterion, and #162's Design B beside it:
/// `benches/arith`, `benches/call`, `benches/field` and `benches/method`
/// execute over one `Vec<u64>` frame stack.
///
/// The whole entry, not a hand-written loop: this consumes the
/// `cove_ir::Program` `cove run --backend vm` consumes, and answers what the
/// other two backends answer.
#[test]
#[ignore = "runs the `benches/` rows at their measured 2,000,000 turns; see the module docs"]
fn the_admitted_bench_rows_run_on_the_frame_and_agree_with_both_backends() {
    for name in ADMITTED_ROWS {
        let ready = bench(name);
        ready
            .admitted()
            .unwrap_or_else(|why| panic!("`benches/{name}` is admitted, but: {why}"));
        let (ast, vm, frame) =
            crate::on_cove_stack(|| (ready.on_ast(), ready.on_vm(), ready.on_frame()))
                .expect("a thread to run Cove on");
        assert_eq!(ast, vm, "`benches/{name}`: the oracle and the VM disagreed");
        assert_eq!(
            vm, frame,
            "`benches/{name}`: the VM and the 8-byte frame disagreed"
        );
        assert!(
            matches!(&frame, Outcome::Answered(rendered) if rendered.contains("Ok")),
            "`benches/{name}` answered {frame:?}"
        );
    }
}

/// The two rows run the same instructions on both backends.
///
/// An exact count is the cheapest control there is and the one thing a
/// rebuild cannot move. It has to be equal rather than merely close: the
/// frame executes the same lowered code, charges the same block extents, and
/// therefore does the same amount of program. Issue #126 proved a count is
/// necessary and not sufficient — three changes with identical counts summed
/// to 19% slower — which is why the wall-clock comparison is beside it and
/// not instead of it.
#[test]
#[ignore = "runs the `benches/` rows at their measured 2,000,000 turns; see the module docs"]
fn the_frame_executes_exactly_the_instructions_the_vm_executes() {
    for name in ADMITTED_ROWS {
        let ready = bench(name);
        let (_, instructions, _) = ready.on_frame_with(None, None);
        assert_eq!(
            instructions,
            ready.vm_instructions(),
            "`benches/{name}` ran a different number of instructions on the two backends"
        );
        // Printed rather than only compared, because the absolute is what
        // `docs/VM_ARCHITECTURE.md` quotes beside a wall-clock ratio and a
        // number nobody can read out of a run is a number that drifts.
        println!("benches/{name}: {instructions} instructions on both backends");
    }
}

/// Issue #212's hard constraint, as a number rather than as a sentence.
///
/// Eight `Value` operations for a run of two million turns, all of them in the
/// nine instructions after the loop: `assertEqual`'s two arguments and its
/// answer, the `try`, the `pop`, `Ok`'s one argument and its answer, and the
/// `return`. None in the loop.
///
/// **The number is the same for `field` and `method` as for `arith`**, and
/// that is the Phase B claim: a loop that builds a struct, reads two of its
/// fields and writes one of them, two million times, still constructs no
/// `Value` at all. `const` and `scalar-to-value` no longer materialise
/// anything, because a constant that is one of decision 1's four kinds *is*
/// eight bytes and a settled `Int` is the same eight bytes on both sides of
/// the conversion.
#[test]
#[ignore = "runs the `benches/` rows at their measured 2,000,000 turns; see the module docs"]
fn the_hot_path_performs_no_value_operation() {
    for name in ADMITTED_ROWS {
        let ready = bench(name);
        let (_, materialized, heap) = ready.on_frame_measured();
        assert_eq!(
            materialized, 8,
            "`benches/{name}` materialized {materialized} value(s), and the epilogue is worth 8"
        );
        // And what the traced heap did, which is the other half of the same
        // claim: the two rooted rows allocate an object a turn and keep one
        // alive, and the two scalar rows own no heap at all.
        match name {
            "field" | "method" => {
                assert_eq!(
                    heap.allocated_objects, 2_000_001,
                    "`benches/{name}` builds one `Cursor` and copies it once a turn"
                );
                assert_eq!(
                    heap.peak_bytes, 16,
                    "the live set is one two-word object, whatever the row allocated"
                );
                assert_eq!(heap.live_objects, 1);
                assert!(
                    heap.collections > 30_000,
                    "`benches/{name}` collected {} time(s), and two million allocations \
                     against a floor of sixty-four is about thirty-one thousand",
                    heap.collections
                );
                // Everything the row allocated was either reclaimed or is
                // live, except what it allocated after the last collection —
                // which the pacing floor bounds at sixty-four however long the
                // run was.
                let outstanding = heap.allocated_objects - heap.freed_objects - heap.live_objects;
                assert!(
                    outstanding < 64,
                    "`benches/{name}` left {outstanding} object(s) unaccounted for, and the \
                     pacing floor is sixty-four allocations"
                );
            }
            _ => assert_eq!(
                heap.allocated_objects, 0,
                "`benches/{name}` holds no reference anywhere, so it owns no heap"
            ),
        }
    }
}

/// `benches/pure` is recursive `fib`, which is calls and branches and
/// nothing else, so the frame runs it too — and it is the row that says most
/// about what a call costs.
#[test]
#[ignore = "runs the `benches/` rows at their measured 2,000,000 turns; see the module docs"]
fn pure_runs_on_the_frame_as_well() {
    let ready = bench("pure");
    ready
        .admitted()
        .unwrap_or_else(|why| panic!("`benches/pure` is admitted, but: {why}"));
    let (vm, frame) = crate::on_cove_stack(|| (ready.on_vm(), ready.on_frame()))
        .expect("a thread to run Cove on");
    assert_eq!(vm, frame);
}

/// The deepest the one stack gets on the three admitted rows, so that
/// `docs/VM_ARCHITECTURE.md`'s "maximum stack capacity" figure is a number a
/// test reads rather than one somebody worked out once.
///
/// `benches/pure` is the deep one — `fib(20)` stands twenty frames — and it
/// is two orders of magnitude below [`INITIAL_WORDS`]. So the reservation is
/// never exceeded on any row this backend runs, which is the other half of
/// "calls and returns allocate nothing after warm capacity": here the
/// capacity is warm before the first call rather than after it.
#[test]
fn no_admitted_row_grows_the_one_stack_past_its_reservation() {
    for name in ["pure", "arith", "call"] {
        let ready = bench(name);
        let hosts = hosts(None, None);
        let runtime = Runtime::new(ready.checked.clone(), ready.sources.clone(), hosts.clone());
        let high = crate::on_cove_stack(|| {
            let mut frame = FrameVm::new(&runtime, &hosts, &ready.ir);
            frame
                .run_entry(&ready.module, "main", Vec::new())
                .expect("it answers");
            frame.high_water_words()
        })
        .expect("a thread to run Cove on");
        assert!(
            high < INITIAL_WORDS,
            "`benches/{name}` reached {high} word(s), and the reservation is {INITIAL_WORDS}"
        );
    }
}

/// Every other benchmark entry is refused, by name, before it runs.
///
/// Named individually rather than counted, because a refusal that changed
/// which construct it was about would otherwise go unnoticed.
///
/// `hostheavy`'s own reason moved once `Inst::CallHost` gained an
/// `admits_function` arm of its own: it no longer names the call, which is
/// now fine, but `var lastSeen = clock.now()` -- a Host call's answer stored
/// in a local -- which was always the real reason and was hidden behind the
/// call itself being refused first.
#[test]
fn the_other_bench_rows_are_refused_by_name() {
    for (name, expected) in [
        ("hostheavy", "a general value slot"),
        ("arrayget", "a collection"),
        ("chars", "a builtin method"),
        ("callback", "a builtin method"),
    ] {
        let ready = bench(name);
        let refused = ready
            .admitted()
            .expect_err(&format!("`benches/{name}` is refused"));
        assert!(
            refused.what.contains(expected),
            "`benches/{name}` was refused for `{}`, and the reason expected was `{expected}`",
            refused.what
        );
        assert!(refused.span.is_some(), "a refusal points at source");
    }
}

// ------------------------------------------------------------ Int semantics

/// Full 64-bit `Int`: both ends of the range survive a frame slot, a call,
/// and a return.
#[test]
fn the_extremes_of_int_survive_a_frame() {
    // Cove has no negative literal — `-x` is unary minus over a general
    // value, which this backend refuses — so the negative ends of the range
    // are reached by subtraction, which is `IntOp::Sub` over words.
    for literal in [
        "0 - 9223372036854775807 - 1",
        "9223372036854775807",
        "0",
        "0 - 1",
    ] {
        agree(&format!(
            "fn through(value: Int) -> Int {{\n  value\n}}\n\n{}",
            main_of("Int", &format!("  through({literal})"))
        ));
    }
}

/// Overflow and division by zero are the language's rules and not a
/// backend's, so all three must raise the same sentence at the same span.
#[test]
fn arithmetic_that_has_no_answer_raises_identically() {
    for (body, expected) in [
        ("  9223372036854775807 + one()", "addition"),
        ("  (0 - 9223372036854775807 - 1) - one()", "subtraction"),
        ("  9223372036854775807 * two()", "multiplication"),
        ("  (0 - 9223372036854775807 - 1) / minusOne()", "division"),
        ("  one() / zero()", "division"),
        ("  one() % zero()", "remainder"),
    ] {
        let ready = agree(&format!(
            "fn one() -> Int {{\n  1\n}}\n\nfn two() -> Int {{\n  2\n}}\n\n\
             fn zero() -> Int {{\n  0\n}}\n\nfn minusOne() -> Int {{\n  0 - 1\n}}\n\n{}",
            main_of("Int", body)
        ));
        let Outcome::Raised { message, span } = ready.on_frame() else {
            panic!("`{body}` raises");
        };
        assert!(
            message.contains(expected),
            "`{body}` raised `{message}`, which does not name {expected}"
        );
        assert!(span.is_some(), "a runtime error points at source");
    }
}

/// `i64::MIN / -1` and `i64::MIN % -1` are the two the frame must not
/// confuse with division by zero: `checked_div` answers `None` for both, and
/// they are different failures with different messages.
#[test]
fn the_two_failures_checked_div_cannot_tell_apart_stay_apart() {
    let ready = agree(
        "fn minusOne() -> Int {\n  0 - 1\n}\n\n\
         export fn main() -> Int {\n  (0 - 9223372036854775807 - 1) % minusOne()\n}\n",
    );
    let Outcome::Raised { message, .. } = ready.on_frame() else {
        panic!("it raises");
    };
    assert!(
        message.contains("remainder") && !message.contains("zero"),
        "it raised `{message}`"
    );
}

// ----------------------------------------------------------- Bool semantics

/// A `Bool` word is canonical, and the three backends agree about every
/// comparison that makes one.
#[test]
fn a_bool_is_canonical_through_a_frame() {
    for expr in [
        "1 < 2", "2 < 1", "1 == 1", "1 != 1", "1 <= 1", "1 >= 2", "1 > 0",
    ] {
        agree(&format!(
            "fn through(value: Bool) -> Bool {{\n  value\n}}\n\n{}",
            main_of("Bool", &format!("  through({expr})"))
        ));
    }
}

/// A non-canonical `Bool` word is a broken invariant of this backend and is
/// refused as one.
///
/// Issue #212 asks for the rejection to be proved rather than asserted, so
/// this reaches it the only way a program cannot: by writing the word
/// directly. Nothing a lowering emits can produce it — a comparison answers
/// `i64::from(..)` and a literal answers `of_bool` — which is exactly why a
/// `Value` an embedder saw carrying one would be the invariant already
/// broken.
#[test]
#[should_panic(expected = "holds 0 or 1, and this one holds 2")]
fn a_non_canonical_bool_word_is_a_broken_invariant() {
    let _ = Word::canonical_bool(2);
}

/// And the two canonical patterns are not.
#[test]
fn the_two_canonical_bool_words_round_trip() {
    assert_eq!(Word::of_bool(false), 0);
    assert_eq!(Word::of_bool(true), 1);
    assert!(!Word::canonical_bool(0));
    assert!(Word::canonical_bool(1));
}

// ---------------------------------------------------------- Float semantics

/// The bit patterns issue #212 asks about, and the ones that break a codec
/// that goes through anything but the bits.
const FLOAT_PATTERNS: &[u64] = &[
    0x0000_0000_0000_0000, // +0.0
    0x8000_0000_0000_0000, // -0.0, which `==` cannot tell from +0.0
    0x3FF0_0000_0000_0000, // 1.0
    0xBFF0_0000_0000_0000, // -1.0
    0x7FF0_0000_0000_0000, // +inf
    0xFFF0_0000_0000_0000, // -inf
    0x7FF8_0000_0000_0000, // a quiet NaN with an empty payload
    0x7FF8_0000_DEAD_BEEF, // a quiet NaN carrying a payload
    0xFFF8_0000_DEAD_BEEF, // the same, signed
    0x7FF0_0000_0000_0001, // a signalling NaN, which an `f64` round trip can quiet
    0x000F_FFFF_FFFF_FFFF, // the largest subnormal
    0x0000_0000_0000_0001, // the smallest subnormal
    0x7FEF_FFFF_FFFF_FFFF, // f64::MAX
    0xFFFF_FFFF_FFFF_FFFF, // every bit set
];

/// ADR 0028 decision 1: a `Float` word is "the full IEEE-754 64-bit bit
/// pattern, every pattern including every NaN".
///
/// `benches/arith` and `benches/call` do not exercise `Float`, and neither
/// does anything else this backend admits: `cove_ir::Scalar` is `Int | Bool`
/// today, so a `Float` is still lowered as a general value and [`admits`]
/// refuses every function holding one. Issue #212 asks for a focused
/// mechanism test in that case, and this is it — the codec, over the
/// patterns that break a codec written through anything but the bits.
#[test]
fn every_float_bit_pattern_round_trips_through_a_word() {
    for &bits in FLOAT_PATTERNS {
        let word = Word::of_float(Word::float(bits));
        assert_eq!(
            word, bits,
            "the pattern {bits:#018x} did not survive the codec; it came back {word:#018x}"
        );
    }
}

/// And the *frame* is 64-bit clean, which the codec alone does not say.
///
/// The same patterns are carried through a real lowered Cove function — a
/// parameter word, a call, a return word — as the `Int` they bit-identically
/// are, because that is the widest thing this backend can be handed today.
/// What it proves is the storage: a frame slot, an argument becoming a
/// parameter, and a returned answer all keep all 64 bits.
#[test]
fn every_float_bit_pattern_survives_a_real_frame() {
    let ready = ready(
        "fn through(value: Int) -> Int {\n  value\n}\n\n\
         export fn main() -> Int {\n  through(through(through(0)))\n}\n",
    );
    let hosts = hosts(None, None);
    let runtime = Runtime::new(ready.checked.clone(), ready.sources.clone(), hosts.clone());
    let id = ready.ir.function_named("m", "through").expect("it lowered");
    for &bits in FLOAT_PATTERNS {
        let mut frame = FrameVm::new(&runtime, &hosts, &ready.ir);
        let answer = frame
            .call_for_test(id, &[bits])
            .expect("`through` answers its argument");
        assert_eq!(
            answer, bits,
            "the pattern {bits:#018x} did not survive a frame; it came back {answer:#018x}"
        );
        assert_eq!(
            Word::of_float(Word::float(answer)),
            bits,
            "and reading it back as a `Float` did not either"
        );
    }
}

// ------------------------------------------------------ calls and recursion

/// Nested direct calls: parameters, locals and temporaries share one
/// numbering from one base, and a callee cannot reach a caller's locals.
#[test]
fn nested_calls_keep_each_frames_locals_to_itself() {
    agree(
        "fn inner(a: Int, b: Int) -> Int {\n  var t = a * 100\n  t = t + b\n  t\n}\n\n\
         fn middle(a: Int) -> Int {\n  var t = a + 1\n  var u = inner(t, a)\n  u + t\n}\n\n\
         fn outer(a: Int) -> Int {\n  var t = a + 2\n  t + middle(t) + t\n}\n\n\
         export fn main() -> Int {\n  outer(3) + outer(4) + outer(5)\n}\n",
    );
}

/// Recursion, and the answer three backends must agree on.
#[test]
fn recursion_answers_the_same_thing_on_all_three() {
    agree(
        "fn fib(n: Int) -> Int {\n  if n < 2 {\n    n\n  } else {\n    fib(n - 1) + fib(n - 2)\n  }\n}\n\n\
         export fn main() -> Int {\n  fib(18)\n}\n",
    );
}

/// The hard call-depth ceiling, which is `crate::interp::MAX_CALL_DEPTH` and
/// the interpreter's own sentence on every backend.
#[test]
fn the_call_depth_limit_stops_the_frame_where_it_stops_the_vm() {
    let ready = ready(
        "fn down(n: Int) -> Int {\n  if n == 0 {\n    0\n  } else {\n    down(n - 1) + 1\n  }\n}\n\n\
         export fn main() -> Int {\n  down(100000)\n}\n",
    );
    let (vm, frame) = crate::on_cove_stack(|| (ready.on_vm(), ready.on_frame()))
        .expect("a thread to run Cove on");
    assert_eq!(vm, frame, "the two backends stopped differently");
    let Outcome::Raised { message, .. } = frame else {
        panic!("a runaway recursion stops");
    };
    assert!(
        message.contains(&format!("call depth limit of {MAX_CALL_DEPTH}")),
        "it stopped with `{message}`"
    );
}

/// A budget's `max_call_depth` stops it lower down, in the budget's words.
#[test]
fn a_budgets_call_depth_limit_stops_the_frame_in_the_budgets_words() {
    let ready = ready(
        "fn down(n: Int) -> Int {\n  if n == 0 {\n    0\n  } else {\n    down(n - 1) + 1\n  }\n}\n\n\
         export fn main() -> Int {\n  down(64)\n}\n",
    );
    let limits = Limits {
        max_call_depth: Some(8),
        ..Limits::default()
    };
    let vm = ready.on_vm_with(Some(limits.clone()), None);
    let (frame, _, _) = ready.on_frame_with(Some(limits), None);
    assert_eq!(vm, frame, "the two backends stopped differently");
    assert!(matches!(frame, Outcome::Raised { .. }), "it stopped");
}

/// Issue #212 asks that indices survive a `Vec` reallocation, which is the
/// one thing an index-based frame has to be right about.
///
/// A `FrameVm` starts with an empty `Vec<u64>` and every frame it opens
/// grows it, so a recursion 200 deep reallocates it many times over — and
/// every standing frame's base is an index into the vector that just moved.
/// A pointer-based frame would be reading freed memory here.
#[test]
fn a_frame_base_survives_the_stack_growing_under_it() {
    let ready = ready(
        "fn down(n: Int, carried: Int) -> Int {\n  \
         if n == 0 {\n    carried\n  } else {\n    down(n - 1, carried + n) - 1 + 1\n  }\n}\n\n\
         export fn main() -> Int {\n  down(200, 0)\n}\n",
    );
    let (ast, frame) = crate::on_cove_stack(|| (ready.on_ast(), ready.on_frame()))
        .expect("a thread to run Cove on");
    assert_eq!(ast, frame);
    let hosts = hosts(None, None);
    let runtime = Runtime::new(ready.checked.clone(), ready.sources.clone(), hosts.clone());
    let mut frame = FrameVm::new(&runtime, &hosts, &ready.ir);
    frame
        .run_entry("m", "main", Vec::new())
        .expect("it answers");
    assert!(
        frame.high_water_words() > 200,
        "200 standing frames occupy more than 200 words, and the stack reached {}",
        frame.high_water_words()
    );
}

// -------------------------------------------------- mixed-kind parameters

/// A struct with one field, small enough that a value parameter costs exactly
/// one word — used throughout this section so a call's argument count is
/// easy to read off the source.
const CELL_DECL: &str = "struct Cell {\n  n: Int\n}\n\n";

/// A scalar parameter before a value one. The frame map used to call the
/// whole reference region one range and require the words a call pushed to
/// line up with it; a scalar parameter always could, because a scalar
/// parameter was slot 0, but the value parameter one word over used to have
/// nowhere of its own to be `admits` could show was safe.
#[test]
fn a_scalar_parameter_before_a_value_one_is_admitted_and_agrees() {
    agree(&format!(
        "{CELL_DECL}fn combine(a: Int, cell: Cell) -> Int {{\n  a + cell.n\n}}\n\n\
         export fn main() -> Int {{\n  combine(3, Cell(n: 4))\n}}\n"
    ));
}

/// The same, in the other order: a value parameter before a scalar one, which
/// used to be refused outright — a value parameter used to have to be the
/// only kind a function took.
#[test]
fn a_value_parameter_before_a_scalar_one_is_admitted_and_agrees() {
    agree(&format!(
        "{CELL_DECL}fn combine(cell: Cell, a: Int) -> Int {{\n  cell.n + a\n}}\n\n\
         export fn main() -> Int {{\n  combine(Cell(n: 4), 3)\n}}\n"
    ));
}

/// A value parameter beside a scalar *local* the function declares itself —
/// the shape the third of the three retired refusals named, by asking
/// whether `function.scalar_frame_size != 0` stood beside a value parameter
/// rather than asking about another parameter at all.
#[test]
fn a_value_parameter_beside_the_functions_own_scalar_locals_is_admitted_and_agrees() {
    agree(&format!(
        "{CELL_DECL}fn scan(cell: Cell) -> Int {{\n  var total = 0\n  var i = 0\n  \
         while i < cell.n {{\n    total = total + i\n    i = i + 1\n  }}\n  total\n}}\n\n\
         export fn main() -> Int {{\n  scan(Cell(n: 5))\n}}\n"
    ));
}

/// A receiver is parameter 0 like any other parameter, so a method that takes
/// one more parameter beside `self` is the same mixed shape under a
/// different syntax: `self` is `SlotKind::Value` and `by` is
/// `SlotKind::Scalar`, one slot apart.
#[test]
fn a_receiver_taking_method_with_a_scalar_parameter_beside_it_is_admitted_and_agrees() {
    agree(
        "struct Cursor {\n  at: Int\n  step: Int\n}\n\n\
         impl Cursor {\n  fn advance(self, by: Int) -> Int {\n    self.at + by\n  }\n}\n\n\
         export fn main() -> Int {\n  var cursor = Cursor(at: 0, step: 1)\n  \
         cursor.advance(5) + cursor.advance(2)\n}\n",
    );
}

/// Eighty parameters, alternating `Int` and `Cell`, so `wide`'s own frame
/// numbers a reference pattern that crosses the boundary between the
/// bitmap's first and second limb rather than standing entirely inside one —
/// `slots[63]` and `slots[64]` are on opposite sides of it. `main` keeps two
/// scalars of its own before the call, so `wide`'s frame does not even open
/// limb-aligned: the shift [`Bitmap::write_frame`] has to apply is `main`'s
/// own width, not zero, which is what makes this a real frame rather than a
/// synthetic one built to land on the boundary.
fn wide_mixed_params_source() -> String {
    let params: Vec<String> = (0..80)
        .map(|i| {
            if i % 2 == 0 {
                format!("a{i}: Int")
            } else {
                format!("c{i}: Cell")
            }
        })
        .collect();
    let args: Vec<String> = (0..80)
        .map(|i| {
            if i % 2 == 0 {
                format!("{i}")
            } else {
                format!("Cell(n: {i})")
            }
        })
        .collect();
    // One statement per parameter rather than one chained expression: the
    // parser bounds how deep source may nest, and eighty terms of `a + b + c`
    // nest that deep on their own without saying anything more about mixed
    // parameters than eighty assignments do.
    let adds: String = (0..80)
        .map(|i| {
            if i % 2 == 0 {
                format!("  total = total + a{i}\n")
            } else {
                format!("  total = total + c{i}.n\n")
            }
        })
        .collect();
    format!(
        "{CELL_DECL}fn wide({}) -> Int {{\n  var total = 0\n  var i = 0\n  \
         while i < 3 {{\n    total = total + i\n    i = i + 1\n  }}\n{adds}  total\n}}\n\n\
         export fn main() -> Int {{\n  var seed = 3\n  var extra = seed + 1\n  \
         wide({}) + extra\n}}\n",
        params.join(", "),
        args.join(", "),
    )
}

/// `wide`'s frame is opened at a shifted base, over eighty words whose
/// reference pattern crosses the boundary between the bitmap's first and
/// second limb, and both backends still agree about what it answers — the
/// case `Bitmap::write_frame`'s own unit tests exercise synthetically, now
/// exercised by a real call.
#[test]
fn a_frame_whose_reference_pattern_straddles_a_limb_boundary_is_admitted_and_agrees() {
    agree(&wide_mixed_params_source());
}

/// The same frame, standing while a collection runs at every safepoint its
/// own loop takes: every `Cell` argument is found through its scattered slot
/// and every `Int` argument beside it is left alone, or the run panics on the
/// heap's own "names a swept object" rather than merely answering the wrong
/// number — proof that the scattered references are all roots and the
/// scalars between them are not walked.
#[test]
fn a_collection_over_a_straddling_frame_finds_every_scattered_reference() {
    let ready = ready(&wide_mixed_params_source());
    let (vm, collected) = crate::on_cove_stack(|| {
        (
            ready.on_vm(),
            ready.on_frame_collecting(RootScope::EveryWord),
        )
    })
    .expect("a thread to run Cove on");
    assert_eq!(collected.outcome, vm, "the VM and the frame disagreed");
    assert!(collected.collections > 0, "{collected:?}");
}

// ------------------------------------------------------- fuel and stopping

/// A run that exceeds its fuel budget stops, on the frame as on the VM, and
/// the two stop at the same place because they charge the same block extents
/// against the same [`SAFEPOINT_INTERVAL`] and [`BACK_EDGE_FUEL`].
///
/// ADR 0024's four constants are untouched by this backend: it reads them,
/// it does not restate them, and `Vm` reads the same two.
#[test]
fn a_fuel_limit_stops_the_frame_where_it_stops_the_vm() {
    let ready = ready(
        "export fn main() -> Int {\n  var total = 0\n  var i = 0\n  \
         while i < 1000000 {\n    total = total + i\n    i = i + 1\n  }\n  total\n}\n",
    );
    let limits = Limits {
        fuel: Some(10_000),
        ..Limits::default()
    };
    let vm = ready.on_vm_with(Some(limits.clone()), None);
    let (frame, _, _) = ready.on_frame_with(Some(limits), None);
    assert_eq!(vm, frame, "the two backends stopped differently");
    assert!(matches!(frame, Outcome::Raised { .. }), "the run stopped");
}

/// The same run without a limit finishes, so the test above is about the
/// limit rather than about the loop.
#[test]
fn the_same_loop_finishes_without_a_limit() {
    let ready = ready(
        "export fn main() -> Int {\n  var total = 0\n  var i = 0\n  \
         while i < 1000000 {\n    total = total + i\n    i = i + 1\n  }\n  total\n}\n",
    );
    assert_eq!(ready.on_vm(), ready.on_frame());
    assert!(matches!(ready.on_frame(), Outcome::Answered(_)));
}

/// Cancelling the run stops the frame's loop, which is the other half of
/// what a safepoint is for.
///
/// The flag is raised before the run begins, so what this proves is that the
/// entry itself is a safepoint — ADR 0024's "a run cancelled before it began
/// stops before its first instruction" — and that the frame asks the same
/// question the VM asks.
#[test]
fn a_cancelled_run_stops_on_the_frame_as_it_does_on_the_vm() {
    let ready = ready(
        "export fn main() -> Int {\n  var total = 0\n  var i = 0\n  \
         while i < 1000000 {\n    total = total + i\n    i = i + 1\n  }\n  total\n}\n",
    );
    let flag = Cancellation::new();
    flag.cancel();
    let vm = ready.on_vm_with(None, Some(flag.clone()));
    let (frame, _, _) = ready.on_frame_with(None, Some(flag));
    assert_eq!(vm, frame, "the two backends stopped differently");
    assert!(matches!(frame, Outcome::Raised { .. }), "the run stopped");
}

/// Pending fuel is never lost: what the frame charged and had not handed over
/// is spent at the end of the run, so a run's `fuel_spent` is its whole
/// instruction count and not the part that happened to fall before the last
/// safepoint.
#[test]
fn no_pending_fuel_is_lost_at_the_end_of_a_run() {
    let ready = ready("export fn main() -> Int {\n  1 + 2\n}\n");
    let hosts = hosts(Some(Limits::default()), None);
    let runtime = Runtime::new(ready.checked.clone(), ready.sources.clone(), hosts.clone());
    let mut frame = FrameVm::new(&runtime, &hosts, &ready.ir);
    frame
        .run_entry("m", "main", Vec::new())
        .expect("it answers");
    let instructions = frame.instructions();
    let spent = hosts
        .with_budget(|budget| budget.fuel_spent())
        .expect("a budget was installed");
    assert_eq!(
        spent,
        instructions * INSTRUCTION_FUEL,
        "the run charged {instructions} instruction(s) and handed over {spent} fuel"
    );
}

// ------------------------------------------------------------------- traces

/// A run of the frame is bracketed by the two source-level events ADR 0019
/// keeps on every backend, and ends with the same `RunEnded` the other two
/// end with, so `cove trace` reads it the same way.
#[test]
fn a_frame_run_emits_the_events_a_vm_run_emits() {
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);
    impl TraceSink for Recorder {
        fn record(&self, event: TraceEvent) {
            self.0
                .lock()
                .expect("no test panics while tracing")
                .push(format!("{event:?}"));
        }
    }
    fn names(events: &[String]) -> Vec<String> {
        events
            .iter()
            .map(|event| {
                event
                    .split(|c: char| !c.is_alphanumeric())
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .collect()
    }

    let ready = ready("export fn main() -> Int {\n  1 + 2\n}\n");

    let recorded = Arc::new(Recorder::default());
    let mut hosts = HostRegistry::new(Grants::new(Vec::<&str>::new()));
    hosts.set_trace(recorded.clone());
    let hosts = Arc::new(hosts);
    let runtime = Runtime::new(ready.checked.clone(), ready.sources.clone(), hosts.clone())
        .with_trace(recorded.clone());
    FrameVm::new(&runtime, &hosts, &ready.ir)
        .run_entry("m", "main", Vec::new())
        .expect("it answers");
    let on_frame = names(&recorded.0.lock().expect("no panic").clone());

    let recorded = Arc::new(Recorder::default());
    let mut hosts = HostRegistry::new(Grants::new(Vec::<&str>::new()));
    hosts.set_trace(recorded.clone());
    let hosts = Arc::new(hosts);
    let runtime = Runtime::new(ready.checked.clone(), ready.sources.clone(), hosts.clone())
        .with_trace(recorded.clone());
    Vm::new(&runtime, &hosts, &ready.ir)
        .run_entry("m", "main", Vec::new())
        .expect("it answers");
    let on_vm = names(&recorded.0.lock().expect("no panic").clone());

    assert_eq!(
        on_frame, on_vm,
        "the two backends recorded different events for the same run"
    );
}

// ------------------------------------------------------------- Host calls

/// A Host call with no arguments, and its answer returned directly -- so the
/// only instruction between `call-host` and `return` is nothing at all, which
/// is what proves `leaves_a_boundary_value` recognises `call-host` on its own
/// and not only beside a `try`.
#[test]
fn a_host_call_with_no_arguments_agrees_on_all_three() {
    agree_with_echo(
        "use echo\n\nexport fn main() -> Result<Unit, Error> {\n  echo.unit()\n}\n",
        true,
    );
}

/// A Host call with several arguments, of every kind this backend admits at
/// once -- an `Int`, a `Bool`, a `Float` and a `String` -- and its answer
/// discarded rather than used: `try` unwraps it and the bare `Int` statement
/// after is a `pop`.
#[test]
fn a_host_call_with_several_arguments_of_every_admitted_kind_agrees_and_is_discarded() {
    agree_with_echo(
        "use echo\n\nexport fn main() -> Result<Unit, Error> {\n  \
         echo.many(1, true, 3.14, \"text\")?\n  Ok(())\n}\n",
        true,
    );
}

/// One Host call per scalar kind, each answer returned directly -- so each is
/// both an argument-kind test and an answer-used test. `Float` has no
/// dedicated operation because `cove_schema::HostType` has no `Float` case at
/// all; `echo.many` still takes one and answers how many arguments it saw,
/// which is what proves the argument crossed.
#[test]
fn a_host_call_takes_each_scalar_kind_and_a_string() {
    agree_with_echo(
        "use echo\n\nexport fn main() -> Result<Int, Error> {\n  echo.int(42)\n}\n",
        true,
    );
    agree_with_echo(
        "use echo\n\nexport fn main() -> Result<Bool, Error> {\n  echo.bool(true)\n}\n",
        true,
    );
    agree_with_echo(
        "use echo\n\nexport fn main() -> Result<Int, Error> {\n  echo.many(3.14)\n}\n",
        true,
    );
    agree_with_echo(
        "use echo\n\nexport fn main() -> Result<String, Error> {\n  echo.text(\"hello\")\n}\n",
        true,
    );
}

/// A Host call whose answer is a declared failure, taken with `?` -- the
/// `try` that follows `call-host` leaves this frame with the error rather
/// than the payload, the same as `try` after a `make-builtin` already does.
#[test]
fn a_host_calls_failure_is_taken_with_try() {
    agree_with_echo(
        "use echo\n\nexport fn main() -> Result<Unit, Error> {\n  echo.fails()?\n  Ok(())\n}\n",
        true,
    );
}

/// A Host call the program has no capability for is refused, and the refusal
/// is the *host's* -- `HostRegistry::dispatch`'s own message, not
/// `Refused`'s -- so it reads identically whichever backend raised it. This
/// is a run-time failure and not an `admits` refusal: nothing about a Host
/// call's grant is knowable before the run, the same as it is not knowable
/// for `Vm` or the tree walk.
#[test]
fn a_host_call_with_no_capability_is_refused_identically_on_all_three() {
    let outcome = agree_with_echo(
        "use echo\n\nexport fn main() -> Result<Unit, Error> {\n  echo.unit()\n}\n",
        false,
    );
    let Outcome::Raised { message, .. } = outcome else {
        panic!("a call with no capability does not answer: {outcome:?}");
    };
    assert!(
        message.contains("requires the `echo` capability"),
        "it failed with `{message}`"
    );
}

/// A collection taken in the middle of a Host call's own argument crossing
/// does not lose the string -- `benches`' none of this backend's rows makes a
/// Host call, so nothing but a dedicated test exercises
/// `FrameVm::call_host`'s own use of `FrameVm::with_roots` the way
/// `a_collection_taken_in_the_middle_of_a_boundary_crossing_does_not_lose_the_string`
/// exercises `Inst::Return`'s.
///
/// The string is built with `{0}` so that it is a `concat`'s answer and not a
/// `Str` constant, for `a_collection_taken_in_the_middle_of_a_boundary_crossing_does_not_lose_the_string`'s
/// own reason: a constant is rooted by `FrameVm::string_constants` whether or
/// not the crossing itself roots it, which would prove nothing. `echo.text`
/// answers the same string back, so a lost root would show up as a wrong or a
/// panicking answer rather than silently.
#[test]
fn a_collection_taken_in_the_middle_of_a_host_calls_argument_crossing_does_not_lose_the_string() {
    let ready = ready(
        "use echo\n\nexport fn main() -> Result<String, Error> {\n  \
         echo.text(\"0123456789abcdefghijklmnopqrstuvwxyz {0}\")\n}\n",
    );
    let hosts = echo_hosts(true);
    let runtime = Runtime::new(ready.checked.clone(), ready.sources.clone(), hosts.clone());
    let mut frame = FrameVm::new(&runtime, &hosts, &ready.ir);
    frame.stress();
    let answered = frame.run_entry(&ready.module, "main", Vec::new());
    assert_eq!(
        outcome(answered),
        Outcome::Answered(format!(
            "{:?}",
            Value::ok(Value::string("0123456789abcdefghijklmnopqrstuvwxyz 0"))
        )),
        "the crossing lost or corrupted the string"
    );
    let (collections, _, _) = frame.collections();
    assert!(
        collections > 0,
        "the run collected {collections} time(s), so nothing here proves a collection ran \
         mid-crossing"
    );
}

/// A struct argument to a Host call is refused, and the refusal names the
/// same thing `Inst::MakeBuiltin`'s does -- an argument the 8-byte frame
/// cannot read as a value -- because `Inst::CallHost` is admitted through the
/// same `Operands::boundary` check and a struct is refused by it everywhere
/// else.
#[test]
fn a_host_calls_struct_argument_is_refused_and_names_what_it_saw() {
    let ready = ready(
        "use echo\n\nstruct Cell {\n  at: Int\n}\n\nexport fn main() -> Result<Unit, Error> {\n  \
         var cell = Cell(at: 1)\n  echo.many(cell)?\n  Ok(())\n}\n",
    );
    let refused = match ready.admitted() {
        Ok(id) => panic!("a struct argument to a Host call is refused, and it admitted {id:?}"),
        Err(refused) => refused,
    };
    assert!(
        refused
            .what
            .contains("whose arguments the 8-byte frame cannot read as values"),
        "it was refused for `{}`",
        refused.what
    );
}

/// The fuel a Host call costs is the same on both backends, in the shape
/// `the_frame_spends_the_fuel_the_vm_spends` already uses: one run of each
/// backend against a budget with no limit, comparing what the budget reports
/// spent at the end.
#[test]
fn the_frame_spends_the_fuel_the_vm_spends_over_a_host_call() {
    let ready = ready("use echo\n\nexport fn main() -> Result<Unit, Error> {\n  echo.unit()\n}\n");
    let spent = |on_frame: bool| {
        let mut hosts = HostRegistry::new(Grants::new(vec!["echo"]));
        hosts.register(Box::new(Echo));
        hosts.set_budget(Budget::new(Limits::default()));
        let hosts = Arc::new(hosts);
        let runtime = Runtime::new(ready.checked.clone(), ready.sources.clone(), hosts.clone());
        if on_frame {
            let mut frame = FrameVm::new(&runtime, &hosts, &ready.ir);
            frame
                .run_entry(&ready.module, "main", Vec::new())
                .expect("it answers");
        } else {
            let mut vm = Vm::new(&runtime, &hosts, &ready.ir);
            vm.run_entry(&ready.module, "main", Vec::new())
                .expect("it answers");
        }
        hosts
            .with_budget(|budget| budget.fuel_spent())
            .expect("a budget was installed")
    };
    let (on_vm, on_frame) = (spent(false), spent(true));
    assert_eq!(
        on_vm, on_frame,
        "the host call spent {on_vm} fuel on the VM and {on_frame} on the 8-byte frame"
    );
}

// ---------------------------------------------------------------- refusals

/// A construct outside the subset is refused before any side effect, and the
/// refusal names it.
#[test]
fn an_unsupported_construct_is_refused_before_the_run_begins() {
    for (source, expected) in [
        (
            "export fn main() -> Duration {\n  500ms\n}\n",
            "a `Duration`",
        ),
        (
            "export fn main() -> Int {\n  \"abc\".length()\n}\n",
            "a builtin method",
        ),
        (
            "export fn main() -> Int {\n  let f: fn(Int) -> Int = fn(x) {\n    x\n  }\n  f(1)\n}\n",
            "a closure",
        ),
        (
            "export fn main() -> Float {\n  1.5 + 2.5\n}\n",
            "an operator over a general value",
        ),
    ] {
        let ready = ready(source);
        let refused = match ready.admitted() {
            Ok(id) => panic!("`{source}` is refused, and it admitted {id:?}"),
            Err(refused) => refused,
        };
        assert!(
            refused.what.contains(expected),
            "`{source}` was refused for `{}`, and `{expected}` was expected",
            refused.what
        );
    }
}

/// A refused entry fails rather than running somewhere else. ADR 0019's rule
/// for the VM is the rule here: no silent fallback, ever.
#[test]
fn a_refused_entry_raises_rather_than_falling_back() {
    let ready = ready("export fn main() -> Duration {\n  500ms\n}\n");
    let Outcome::Raised { message, .. } = ready.on_frame() else {
        panic!("a refused entry does not answer");
    };
    assert!(
        message.contains("the 8-byte frame cannot run"),
        "it failed with `{message}`"
    );
    // And the VM runs it, so the refusal is this backend's and not the
    // program's.
    assert!(matches!(ready.on_vm(), Outcome::Answered(_)));
}

/// Every one of the eight `IntOp`s, and every branch instruction the loop
/// can be lowered to, over both signs and both zeroes.
#[test]
fn every_admitted_operator_agrees_on_all_three() {
    for op in ["+", "-", "*", "/", "%"] {
        for (a, b) in [
            ("7", "3"),
            ("0 - 7", "3"),
            ("7", "0 - 3"),
            ("0 - 7", "0 - 3"),
            ("0", "5"),
        ] {
            agree(&main_of("Int", &format!("  ({a}) {op} ({b})")));
        }
    }
    for op in ["==", "!=", "<", "<=", ">", ">="] {
        for (a, b) in [("1", "2"), ("2", "1"), ("2", "2"), ("0 - 1", "1")] {
            agree(&main_of("Bool", &format!("  ({a}) {op} ({b})")));
        }
    }
    // `&&` and `||` short-circuit, so they lower to the two conditional
    // jumps rather than to an operator.
    agree(&main_of("Bool", "  1 < 2 && 3 < 4"));
    agree(&main_of("Bool", "  1 > 2 || 3 < 4"));
    agree(&main_of("Bool", "  1 > 2 && 3 < 4"));
    agree(&main_of("Bool", "  1 < 2 || 3 > 4"));
}

// ------------------------------------------ what a rooted frame is made of

/// `benches/field`'s loop, short enough to run with a collection at every
/// safepoint.
///
/// One struct in one frame slot, two field reads and one field write a turn.
/// The write allocates — a struct is a value, so writing a field is a copy;
/// see `crate::slot::HandleHeap::copy_replacing` — so the loop feeds the
/// collector as well as the frame.
const A_STRUCT_IN_A_SLOT: &str = "\
struct Cell {
  at: Int
  step: Int
}

export fn main() -> Int {
  var cell = Cell(at: 0, step: 1)
  var total = 0
  while cell.at < 200 {
    total = total + cell.step
    cell.at = cell.at + cell.step
  }
  total
}
";

/// The same loop with the read behind a call, which is `benches/method`'s
/// shape.
///
/// The call is what puts a handle in a word that is in **no frame at all**:
/// the caller pushed it and the callee's base has not moved under it yet, and
/// the call is a safepoint in between.
const A_STRUCT_HANDED_TO_A_CALL: &str = "\
struct Cell {
  at: Int
  step: Int
}

fn reach(cell: Cell) -> Int {
  cell.at
}

export fn main() -> Int {
  var cell = Cell(at: 0, step: 1)
  var total = 0
  while reach(cell) < 200 {
    total = total + cell.step
    cell.at = cell.at + cell.step
  }
  total
}
";

/// A struct that is **only** an argument: built at the call site, handed over,
/// and named by nothing else.
///
/// The difference from [`A_STRUCT_HANDED_TO_A_CALL`] is what roots it. There,
/// the caller's own slot holds the same handle, so the operand word is a
/// second location for something already rooted. Here the operand word is the
/// only location there is, and the call takes a safepoint while it is the
/// only one.
const A_STRUCT_THAT_IS_ONLY_AN_ARGUMENT: &str = "\
struct Cell {
  at: Int
  step: Int
}

fn reach(cell: Cell) -> Int {
  cell.at
}

export fn main() -> Int {
  var total = 0
  var i = 0
  while i < 200 {
    total = total + reach(Cell(at: i, step: 1))
    i = i + 1
  }
  total
}
";

/// A struct whose first field is another struct, so that one of its words is
/// a handle and the other is not.
const A_STRUCT_INSIDE_A_STRUCT: &str = "\
struct Inner {
  n: Int
}

struct Outer {
  inner: Inner
  n: Int
}

export fn main() -> Int {
  var outer = Outer(inner: Inner(n: 5), n: 1)
  var total = 0
  var i = 0
  while i < 200 {
    total = total + outer.inner.n
    i = i + 1
  }
  total
}
";

/// A nested struct read out and handed straight to a call, with nothing else
/// naming it.
///
/// **This is the shape Phase C's static map admits and Phase B refused.**
/// `Outer(...).inner` pops the outer — so the outer is garbage the moment the
/// read is done — and pushes the inner into an operand word that is the only
/// place it stands. The call under it is a safepoint. So the bit that word
/// carries is the whole of what keeps the inner alive, and that bit comes from
/// `cove_ir::StructType`'s answer about `Outer.inner`.
///
/// Phase B could not admit this at all: `pushed_kinds` had no reading for
/// `Inst::GetFieldAt`, because only the object knew what a field read pushed
/// and `admits` runs before there is an object.
const A_NESTED_STRUCT_READ_AND_HANDED_OVER: &str = "\
struct Inner {
  n: Int
}

struct Outer {
  inner: Inner
  n: Int
}

fn take(inner: Inner) -> Int {
  inner.n
}

export fn main() -> Int {
  var total = 0
  var i = 0
  while i < 200 {
    total = total + take(Outer(inner: Inner(n: 1), n: 2).inner)
    i = i + 1
  }
  total
}
";

/// The same read stored into a local instead of passed, which is the other
/// half of what a field read's kind being static buys: a `store-local` is
/// admitted only where the instruction that pushed the word says it is a
/// reference, and a field read now can.
const A_NESTED_STRUCT_READ_INTO_A_SLOT: &str = "\
struct Inner {
  n: Int
}

struct Outer {
  inner: Inner
  n: Int
}

export fn main() -> Int {
  var outer = Outer(inner: Inner(n: 5), n: 1)
  var total = 0
  var i = 0
  while i < 200 {
    var held = outer.inner
    total = total + held.n
    i = i + 1
  }
  total
}
";

/// The same loop with no bound on it, for a test about where a fuel limit
/// stops rather than about what the loop answers.
const A_STRUCT_LOOP_WITHOUT_END: &str = "\
struct Cell {
  at: Int
  step: Int
}

export fn main() -> Int {
  var cell = Cell(at: 0, step: 1)
  var total = 0
  while cell.at < 1000000 {
    total = total + cell.step
    cell.at = cell.at + cell.step
  }
  total
}
";

/// A struct's own fuel is the VM's.
///
/// `Vm` charges a struct's width beside `make-struct` and beside `set-field`,
/// because building and copying one are proportional to it and ADR 0019 asks
/// that such work is charged proportionally. This backend charges the same
/// width at the same two instructions, so a fuel limit stops the two in the
/// same turn of the same loop with the same message and the same span.
///
/// Equality here is what the stop-parity tests rest on. ADR 0019 makes fuel
/// backend-specific and says why; what is asserted is not that it *must* be
/// equal but that it *is*, because a limit that stopped the two backends in
/// different turns would make every comparison of their answers a comparison
/// of two different programs.
#[test]
fn a_fuel_limit_stops_a_struct_loop_where_it_stops_the_vm() {
    let ready = ready(A_STRUCT_LOOP_WITHOUT_END);
    let limits = Limits {
        fuel: Some(10_000),
        ..Limits::default()
    };
    let vm = ready.on_vm_with(Some(limits.clone()), None);
    let (frame, _, _) = ready.on_frame_with(Some(limits), None);
    assert_eq!(vm, frame, "the two backends stopped differently");
    assert!(matches!(frame, Outcome::Raised { .. }), "the run stopped");
}

/// And the fuel each backend spends over a whole run of each admitted row is
/// the same number.
#[test]
#[ignore = "runs the `benches/` rows at their measured 2,000,000 turns; see the module docs"]
fn the_frame_spends_the_fuel_the_vm_spends() {
    for name in ADMITTED_ROWS {
        let ready = bench(name);
        let spent = |on_frame: bool| {
            let hosts = hosts(Some(Limits::default()), None);
            let runtime = Runtime::new(ready.checked.clone(), ready.sources.clone(), hosts.clone());
            if on_frame {
                let mut frame = FrameVm::new(&runtime, &hosts, &ready.ir);
                frame
                    .run_entry(name, "main", Vec::new())
                    .expect("it answers");
            } else {
                let mut vm = Vm::new(&runtime, &hosts, &ready.ir);
                vm.run_entry(name, "main", Vec::new()).expect("it answers");
            }
            hosts
                .with_budget(|budget| budget.fuel_spent())
                .expect("a budget was installed")
        };
        let (on_vm, on_frame) =
            crate::on_cove_stack(|| (spent(false), spent(true))).expect("a thread to run Cove on");
        assert_eq!(
            on_vm, on_frame,
            "`benches/{name}` spent {on_vm} fuel on the VM and {on_frame} on the 8-byte frame"
        );
    }
}

/// A struct program that raises fails identically on all three backends —
/// same message, same span.
///
/// The overflow is *inside a field write*, which is the one arithmetic Phase B
/// added a road to: the addend is read out of a heap object through the
/// reference map and the answer is written back into a copy of it. Nothing
/// about that road is allowed to change what the failure says or where it
/// points, and `agree` compares both.
#[test]
fn a_struct_program_that_raises_agrees_on_message_and_span() {
    agree(
        "struct Cell {\n  at: Int\n}\n\nexport fn main() -> Int {\n  \
         var cell = Cell(at: 9223372036854775807)\n  cell.at = cell.at + 1\n  cell.at\n}\n",
    );
    agree(
        "struct Cell {\n  at: Int\n  step: Int\n}\n\nexport fn main() -> Int {\n  \
         var cell = Cell(at: 7, step: 0)\n  cell.at = cell.at / cell.step\n  cell.at\n}\n",
    );
}

/// **The positive half of the rooting proof**: a struct standing in a frame
/// slot survives every collection of a run that collects at every safepoint,
/// and the answer is the one the other two backends give.
///
/// Nothing here is a claim about pacing. `collections` is asserted nonzero and
/// `freed_objects` is asserted nonzero, so the heap really did sweep and the
/// object really did have to be found — a run that collected nothing would
/// pass a rooting test by not testing it.
#[test]
fn a_struct_in_a_frame_slot_survives_every_collection_of_a_run() {
    let ready = ready(A_STRUCT_IN_A_SLOT);
    let (vm, collected) = crate::on_cove_stack(|| {
        (
            ready.on_vm(),
            ready.on_frame_collecting(RootScope::EveryWord),
        )
    })
    .expect("a thread to run Cove on");

    assert_eq!(
        collected.outcome, vm,
        "the VM and the 8-byte frame disagreed about a rooted loop"
    );
    assert!(
        collected.collections > 0,
        "the run collected {} time(s), so it proves nothing about rooting",
        collected.collections
    );
    assert!(
        collected.freed_objects > 0,
        "the sweep reclaimed nothing, so the object that survived did not have to"
    );
    assert!(
        collected.expansions > 0,
        "no object was ever expanded, so no root was ever followed"
    );
    assert!(
        collected.allocated_objects > 200,
        "the loop turns two hundred times and writes a field each turn, and a \
         field write is a copy: {collected:?}"
    );
    assert!(
        collected.roots_yielded >= collected.expansions,
        "a walk cannot expand more objects than it yielded locations: {collected:?}"
    );
    assert_eq!(
        collected.expansions, collected.marked,
        "decision 8's third multiplicity: an object is expanded once for every \
         time it is found live, and no more"
    );
}

/// **The mutation.** Take the frame's own words out of the walk and the
/// struct in slot 0 is swept out from under the loop that is using it.
///
/// The failing assertion is `crate::slot::HandleHeap`'s own: reading a word of
/// a swept object panics rather than answering whatever is there, so the run
/// dies at the first `cell.at` after the first collection. That is ADR 0028
/// decision 8's "a handle slot is a root according to the frame reference map"
/// removed, and what it costs.
#[test]
#[should_panic(expected = "names a swept object")]
fn a_value_slot_is_a_root_across_the_loop_it_lives_in() {
    let ready = ready(A_STRUCT_IN_A_SLOT);
    let _ = ready.on_frame_collecting(RootScope::WithoutFrameSlots);
}

/// **The other mutation.** Take the operand words out of the walk and the
/// argument of a call is swept between the caller pushing it and the callee's
/// frame arriving under it.
///
/// This is the half a *static* stack map would have to cover and the half the
/// frame's own reference map does not: the word is above the caller's frame
/// and below a callee that does not exist yet, and `Inst::Call` takes a
/// safepoint there because ADR 0024 says every call is one.
#[test]
#[should_panic(expected = "names a swept object")]
fn a_call_argument_is_a_root_before_the_callee_has_a_frame() {
    let ready = ready(A_STRUCT_THAT_IS_ONLY_AN_ARGUMENT);
    let _ = ready.on_frame_collecting(RootScope::WithoutOperands);
}

/// The control for the mutation above: with the operand words in the walk,
/// the same two programs run and agree with the VM.
#[test]
fn a_struct_handed_to_a_call_survives_the_call_that_is_a_safepoint() {
    for source in [A_STRUCT_HANDED_TO_A_CALL, A_STRUCT_THAT_IS_ONLY_AN_ARGUMENT] {
        let ready = ready(source);
        let (vm, collected) = crate::on_cove_stack(|| {
            (
                ready.on_vm(),
                ready.on_frame_collecting(RootScope::EveryWord),
            )
        })
        .expect("a thread to run Cove on");

        assert_eq!(collected.outcome, vm, "the VM and the frame disagreed");
        assert!(
            collected.collections > 0 && collected.freed_objects > 0,
            "{collected:?}"
        );
    }
}

/// ADR 0028 decision 8's first and third multiplicities, told apart.
///
/// At the safepoint a call takes, one struct stands in the caller's frame slot
/// **and** in the word that is about to be the callee's parameter. Those are
/// two root storage locations and one object: the walk yields both, because a
/// bit is a location and de-duplicating handles is not its job, and the mark
/// phase expands the object they share once.
///
/// The second multiplicity — real graph edges counted once each — does not
/// arise, and `crate::slot`'s module documentation says why: it exists for the
/// comparison against `Rc::strong_count`, there is no such comparison in a
/// traced heap, and that is the whole reason a bitmap over words is sound
/// where a shadow stack over `Value` would not be.
#[test]
fn a_reference_in_a_slot_and_in_an_operand_is_two_locations_and_one_expansion() {
    let ready = ready(A_STRUCT_HANDED_TO_A_CALL);
    let collected = crate::on_cove_stack(|| ready.on_frame_collecting(RootScope::EveryWord))
        .expect("a thread to run Cove on");

    assert!(
        collected.most_roots_at_once >= 2,
        "no collection ever saw two root locations at once, so this says nothing \
         about the first multiplicity: {collected:?}"
    );
    assert_eq!(
        collected.most_expansions_at_once, 1,
        "the two locations name one object, and one object is expanded once: {collected:?}"
    );
    assert_eq!(
        collected.expansions, collected.marked,
        "an object is expanded exactly once per collection that finds it live"
    );
}

/// The shadow-root stack is empty for the whole of an admitted run, and that
/// is a **finding** rather than an omission.
///
/// ADR 0028 decision 8 lists four coherent temporary-rooting mechanisms.
/// `crate::slot` chose the second — an explicit shadow stack — and showed the
/// third, "the dispatch discipline guarantees that a collection can occur only
/// when every live handle has been returned to a mapped VM slot", to be *false*
/// for `Vm` at five named places. It is true here by construction, because a
/// one-stack backend has nowhere else to put an operand: every handle a run of
/// this backend holds is a word of `words`, and every word of `words` is in the
/// walk.
///
/// It stops being free the moment an aggregate crosses decision 5's boundary,
/// which is Phase C's, and the mechanism is wired and empty rather than absent
/// so that this test can say so.
#[test]
fn nothing_is_rooted_outside_the_one_stack() {
    for source in [
        A_STRUCT_IN_A_SLOT,
        A_STRUCT_HANDED_TO_A_CALL,
        A_STRUCT_THAT_IS_ONLY_AN_ARGUMENT,
        A_STRUCT_INSIDE_A_STRUCT,
    ] {
        let ready = ready(source);
        let collected = crate::on_cove_stack(|| ready.on_frame_collecting(RootScope::EveryWord))
            .expect("a thread to run Cove on");
        assert_eq!(
            collected.rooted_outside_the_stack, 0,
            "a handle stood outside the one stack, which the admitted subset has no way to do"
        );
    }
}

/// A word's kind can come from neither the frame map nor the instruction that
/// pushed it *without help*, and this is that case: reading `outer.inner`
/// pushes a reference and reading `outer.n` pushes scalar bits, and it is one
/// opcode either way.
///
/// **Phase B could only ask the object.** Phase C's `Inst::GetFieldAt` names
/// the `cove_ir::StructType` the checker settled, so the answer is the
/// declared field's `SlotKind` and the bitmap is written from a fact that
/// existed before the run. The `debug_assert` in the dispatch loop reads the
/// object's own map beside it on every field read of this test, so what runs
/// here is the two answers agreeing rather than one of them being taken on
/// trust.
#[test]
fn a_field_read_takes_its_kind_from_the_type_the_instruction_names() {
    let ready = ready(A_STRUCT_INSIDE_A_STRUCT);
    let (ast, vm, collected) = crate::on_cove_stack(|| {
        (
            ready.on_ast(),
            ready.on_vm(),
            ready.on_frame_collecting(RootScope::EveryWord),
        )
    })
    .expect("a thread to run Cove on");

    assert_eq!(ast, vm, "the oracle and the VM disagreed");
    assert_eq!(
        collected.outcome, vm,
        "the VM and the frame disagreed about a struct inside a struct"
    );
    assert!(
        collected.collections > 0,
        "the run never collected, so the nested edge was never walked"
    );
    assert_eq!(
        collected.most_expansions_at_once, 2,
        "the outer object and the inner one it names are two expansions of one \
         collection, which is the nested edge being followed: {collected:?}"
    );
}

/// **The third mutation.** Empty the map a field read reads its bit out of,
/// and the struct a field read just produced is swept while it is the only
/// thing there is.
///
/// The failing assertion is `crate::slot::HandleHeap`'s own — reading a word
/// of a swept object panics with `names a swept object` rather than answering
/// whatever is there — and it fires inside `take`, on `inner.n`, at the first
/// collection after the call that handed the argument over.
///
/// What it removes is exactly Phase C's change and nothing else. The frame map
/// still names every value slot, the operand words are still all in the walk,
/// and `A_NESTED_STRUCT_READ_AND_HANDED_OVER` is a program where neither of
/// those helps: the outer object was popped by the read itself, so the inner
/// stands in one operand word and in nothing else.
#[test]
#[should_panic(expected = "names a swept object")]
fn a_field_reads_bit_comes_from_the_lowered_type() {
    let ready = ready(A_NESTED_STRUCT_READ_AND_HANDED_OVER);
    let _ = ready.on_frame_collecting_with(RootScope::EveryWord, FieldMap::Dropped);
}

/// The control for the mutation above, and the coverage the widening is taken
/// on.
///
/// Both programs are shapes Phase B refused — a field read feeding a call and
/// a field read feeding a `store-local` — and both are admitted because
/// `Inst::GetFieldAt` now names the type whose field it reads. So the run
/// agrees with the VM and with the tree walk, it collects, and what it
/// reclaims is the outer objects the reads threw away.
#[test]
fn a_nested_struct_read_into_a_slot_is_rooted() {
    // The second flag is whether the program throws objects away: the first
    // builds a fresh `Outer` a turn and abandons it at the read, and the
    // second builds one and holds it, so only the first has a sweep to make.
    for (source, sweeps) in [
        (A_NESTED_STRUCT_READ_AND_HANDED_OVER, true),
        (A_NESTED_STRUCT_READ_INTO_A_SLOT, false),
    ] {
        let ready = ready(source);
        let (ast, vm, collected) = crate::on_cove_stack(|| {
            (
                ready.on_ast(),
                ready.on_vm(),
                ready.on_frame_collecting(RootScope::EveryWord),
            )
        })
        .expect("a thread to run Cove on");

        assert_eq!(ast, vm, "the oracle and the VM disagreed");
        assert_eq!(
            collected.outcome, vm,
            "the VM and the frame disagreed about a field read that outlives its struct"
        );
        assert!(
            collected.collections > 0,
            "the run never collected, so it says nothing about rooting: {collected:?}"
        );
        assert_eq!(
            collected.freed_objects > 0,
            sweeps,
            "a program that abandons an object a turn sweeps and one that holds \
             its two does not: {collected:?}"
        );
        assert!(
            collected.expansions > 0,
            "no object was ever expanded, so no root was ever followed: {collected:?}"
        );
    }
}

/// The invariant ADR 0028 decision 1 states for any physical arrangement: **a
/// slot the layout calls scalar is never reachable by a walk that treats it as
/// a reference.**
///
/// A word holding the exact eight bytes of a live handle, with its bit clear,
/// is not yielded. Nothing about the bits is different from the word beside it;
/// the bitmap is the only difference, which is the point.
#[test]
fn a_scalar_word_that_looks_like_a_reference_is_not_walked() {
    let mut refs = Bitmap::with_capacity(64);
    let words = [0x0000_0001_0000_0002u64, 0x0000_0001_0000_0002u64];
    refs.write(0, false);
    refs.write(1, true);
    let temps = TempRoots::new();
    let constants = Vec::new();
    let roots = FrameRoots {
        words: &words,
        refs: &refs,
        temps: &temps,
        constants: &constants,
        range: 0..words.len(),
    };
    let mut seen = Vec::new();
    roots.walk(&mut |handle| seen.push(handle));
    assert_eq!(
        seen,
        vec![Handle::from_slot(words[1])],
        "the walk yielded the scalar word, whose bits are the reference word's"
    );
}

/// A bitmap skips sixty-four words at a time where a limb is empty, and finds
/// every set bit where one is not.
#[test]
fn a_bitmap_finds_every_reference_and_nothing_else() {
    let mut refs = Bitmap::with_capacity(256);
    for at in 0..200 {
        refs.write(at, at % 37 == 0);
    }
    let mut seen = Vec::new();
    refs.for_each(0..200, &mut |at| seen.push(at));
    assert_eq!(seen, vec![0, 37, 74, 111, 148, 185]);

    // And a range that starts and ends inside a limb.
    let mut seen = Vec::new();
    refs.for_each(38..149, &mut |at| seen.push(at));
    assert_eq!(seen, vec![74, 111, 148]);
}

/// A `FrameMap` for a frame of `width` words whose reference slots are `refs`,
/// relative to the frame's own slot 0.
///
/// `FrameMap::of` reads this off a real `cove_ir::Function`; these two tests
/// want to drive [`Bitmap::write_frame`] directly, at a plain pattern and at
/// one that straddles a limb boundary, without lowering a program to get a
/// map from.
fn map_of(width: usize, refs: std::ops::Range<usize>) -> FrameMap {
    let mut template = vec![0u64; width.div_ceil(64)];
    for at in refs {
        template[at / 64] |= 1u64 << (at % 64);
    }
    FrameMap {
        width: width as u32,
        template,
    }
}

/// Opening a frame writes the whole reference range in one pass, and the
/// scalars beside it are cleared rather than left as whatever the last frame
/// at that depth said.
///
/// The clearing is load-bearing: a return writes no bit, so a word reused by
/// the next frame at the same depth would keep the previous frame's answer
/// about it if opening did not overwrite one.
#[test]
fn opening_a_frame_writes_every_bit_of_its_window() {
    let mut refs = Bitmap::with_capacity(256);
    refs.write_frame(0, &map_of(130, 0..130));
    refs.write_frame(4, &map_of(8, 2..5));
    let mut seen = Vec::new();
    refs.for_each(4..12, &mut |at| seen.push(at));
    assert_eq!(
        seen,
        vec![6, 7, 8],
        "the frame at 4 says words 6, 7 and 8 are references and the rest are not"
    );
}

/// The same, for a frame that straddles a limb boundary and a reference range
/// that straddles it too.
///
/// One read-modify-write per limb is only correct if the mask is right at both
/// ends of every limb it touches, and a frame entirely inside one limb — which
/// is every frame this backend actually opens — exercises neither end.
#[test]
fn a_frame_that_straddles_a_limb_is_written_correctly() {
    let mut refs = Bitmap::with_capacity(512);
    // Everything set, so that the clearing half has something to clear.
    refs.write_frame(0, &map_of(256, 0..256));
    // A frame of 100 words at 30, whose words 20..50 are references: absolute
    // 50..80, which spans the boundary at 64.
    refs.write_frame(30, &map_of(100, 20..50));
    let mut seen = Vec::new();
    refs.for_each(0..256, &mut |at| seen.push(at));
    let expected: Vec<usize> = (0..30).chain(50..80).chain(130..256).collect();
    assert_eq!(seen, expected);
}

/// A template whose reference bits are scattered rather than one range — the
/// pattern a mixed-parameter frame now produces — is shifted into place
/// exactly as a contiguous one is, including across the limb boundary at 64.
#[test]
fn a_scattered_template_straddling_a_limb_is_written_correctly() {
    let mut refs = Bitmap::with_capacity(256);
    refs.write_frame(0, &map_of(256, 0..256));
    let mut template = vec![0u64; 100usize.div_ceil(64)];
    // Slots 20, 21, 49 and 50 of a hundred-word frame opened at 30: absolute
    // 50, 51, 79 and 80, which spans the boundary at 64 with a gap on both
    // sides of it.
    for at in [20, 21, 49, 50] {
        template[at / 64] |= 1u64 << (at % 64);
    }
    refs.write_frame(
        30,
        &FrameMap {
            width: 100,
            template,
        },
    );
    let mut seen = Vec::new();
    refs.for_each(0..256, &mut |at| seen.push(at));
    let expected: Vec<usize> = (0..30).chain([50, 51, 79, 80]).chain(130..256).collect();
    assert_eq!(seen, expected);
}

// ------------------------------------- the per-`pc` operand-kind simulation

/// A struct whose fields are computed rather than written as constants.
///
/// `Cursor(at: i, step: 1)` is the shape the peephole this replaced named as
/// what it could not read: `i` is a scalar local, so its word is pushed by a
/// `load-scalar` and moved across by a `scalar-to-value`, and two instructions
/// stand where the peephole counted one operand.
const A_STRUCT_BUILT_FROM_A_LOADED_WORD: &str = "\
struct Cursor {
  at: Int
  step: Int
}

export fn main() -> Int {
  var here = Cursor(at: 0, step: 1)
  var total = 0
  var i = 0
  while i < 60 {
    here = Cursor(at: i, step: here.step)
    total = total + here.at + here.step
    i = i + 1
  }
  total
}
";

/// The shape the peephole refused is admitted, and all three backends agree
/// about what it answers.
///
/// This is the widening, and it is taken only because the test below runs the
/// same program through the collector at every safepoint. `admits` is asserted
/// as well as the answer, because a program that agreed by being refused and
/// raising on both sides would agree for the wrong reason.
#[test]
fn a_struct_built_from_a_loaded_word_is_admitted_and_agrees() {
    let ready = agree(A_STRUCT_BUILT_FROM_A_LOADED_WORD);
    ready
        .admitted()
        .expect("the simulation reads every word this builds a struct out of");
}

/// The control: the same program with the walk whole, through the collector at
/// every safepoint, agreeing with the `Vm`.
///
/// The cursor stands in a value slot across the loop's back edge — the next
/// turn reads `here.step` out of it — and the back edge is a safepoint, so
/// every turn puts the rooting to the test. The assertions on `collections`
/// and `freed_objects` are what stop this passing by never collecting: a turn
/// abandons the cursor the turn before it, and the sweep is asserted to have
/// reclaimed some.
#[test]
fn a_struct_built_from_a_loaded_word_is_rooted_in_its_slot() {
    let ready = ready(A_STRUCT_BUILT_FROM_A_LOADED_WORD);
    let whole = ready.on_frame_collecting(RootScope::EveryWord);
    assert!(
        matches!(whole.outcome, Outcome::Answered(_)),
        "the walk whole, it answers: {:?}",
        whole.outcome
    );
    assert_eq!(whole.outcome, ready.on_vm(), "and it agrees with the VM");
    assert!(
        whole.collections > 0,
        "the stressed run collected {} time(s)",
        whole.collections
    );
    assert!(
        whole.freed_objects > 0,
        "the loop abandons a cursor a turn and the sweep reclaimed {} of them",
        whole.freed_objects
    );
}

/// **The mutation for the widening.** Drop the frame's own words from the walk
/// and the cursor the simulation admitted is swept out from under the loop
/// that is still using it.
///
/// This is `a_value_slot_is_a_root_across_the_loop_it_lives_in` asked about the
/// shape Phase D added, and it is asked separately because the widening is only
/// worth taking if the program it admits is rooted for a reason a test can
/// break. It dies on `crate::slot::HandleHeap`'s own use-after-free message
/// rather than on an assertion anybody wrote, under
/// `HandleHeap::stress`, so neither direction depends on when a collection
/// happens to land.
#[test]
#[should_panic(expected = "names a swept object")]
fn the_widened_shapes_struct_is_a_root_in_its_slot() {
    let ready = ready(A_STRUCT_BUILT_FROM_A_LOADED_WORD);
    let _ = ready.on_frame_collecting(RootScope::WithoutFrameSlots);
}

/// A constant is named as the constant it is, wherever it stands.
///
/// This is the half of the replacement that is *stricter*, and it is the more
/// important half. The peephole read `Inst::Dup` as `Kind::Reference`
/// unconditionally, because the one instruction it could see says nothing about
/// what it copies — so a `dup` over the word this test looks at would have been
/// called a handle, and `store-local` is admitted exactly where the word pushed
/// is one. That is a wrong *acceptance*: a non-handle would have gone into a
/// slot the frame map calls a reference, which is the invariant ADR 0028
/// decision 1 states for any physical arrangement, from the other side.
///
/// The dispatch loop was never wrong about it — `Inst::Dup` copies the *bit* —
/// so nothing that ran was unsound. The check was. Simulating the stack gives
/// `dup` the kind of the word it actually copies, which is what this asserts
/// the simulation knows about the word underneath one.
#[test]
fn a_constant_is_not_read_as_a_handle() {
    let ready = ready(A_STRUCT_BUILT_FROM_A_LOADED_WORD);
    let id = ready.admitted().expect("it is admitted");
    let function = ready.ir.function(id);
    let operands = simulate(&ready.ir, function);
    let at = function
        .code
        .iter()
        .position(|inst| matches!(inst, Inst::Const(_)))
        .expect("the program loads a constant");
    assert_ne!(
        operands.top(at + 1, 1),
        Some(vec![Kind::Reference]),
        "a constant is not a handle, and the peephole this replaced called \
         `dup` over one a handle"
    );
}

/// **The widening is not vacuous**, and this is what says so from the program
/// rather than from a claim about deleted code.
///
/// The peephole assumed the `count` instructions before a `make-struct` are its
/// `count` operands. The two before the one in the loop are `load` and
/// `get-field-at`, which between them leave **one**: the read consumes the
/// object the load pushed. So the window was misaligned, the kinds it derived
/// from it were `[Reference, Int]` where the operands are `[Int, Int]`, and the
/// `make-struct` was refused for disagreeing with a type it agrees with.
///
/// That is a sharper statement than "it could not read the window", and it is
/// the one this program supports: a misaligned window does not only *fail* to
/// name the operands, it names something else. The initializer above it is the
/// aligned case — two constants, two operands — so the same instruction is
/// admitted twice for two different reasons and the program is a fair test
/// rather than a rigged one.
///
/// Asserted through `cove_ir::lower::stack_shape`, which is the same
/// description [`simulate`] counts with, so this is the peephole's assumption
/// tested against the one authority on what an instruction does.
#[test]
fn the_peepholes_window_was_not_this_programs_operands() {
    let ready = ready(A_STRUCT_BUILT_FROM_A_LOADED_WORD);
    let id = ready.admitted().expect("it is admitted");
    let function = ready.ir.function(id);
    // Two `make-struct`s: the initializer, which is two constants and which
    // the peephole read perfectly well, and the one in the loop, which is the
    // widened shape. It is the second that has to be unreadable, and the first
    // being readable is why the program is a fair test rather than a rigged
    // one — the same instruction is admitted twice for two different reasons.
    let windows: Vec<(usize, Vec<Inst>, i64)> = function
        .code
        .iter()
        .enumerate()
        .filter_map(|(pc, inst)| match inst {
            Inst::MakeStruct(of) => {
                let fields = ready.ir.struct_type(*of).fields.len();
                let window = function.code[pc - fields..pc].to_vec();
                // What the window actually leaves on the value stack, which is
                // what the peephole assumed was `fields`.
                let left: i64 = window
                    .iter()
                    .map(|inst| {
                        let shape = cove_ir::lower::stack_shape(&ready.ir.structs, *inst);
                        i64::from(shape.values.1) - i64::from(shape.values.0)
                    })
                    .sum();
                Some((fields, window, left))
            }
            _ => None,
        })
        .collect();
    assert_eq!(windows.len(), 2, "the program builds a cursor twice");
    assert!(
        windows
            .iter()
            .any(|(fields, _, left)| *left != *fields as i64),
        "every window leaves as many value operands as the type has fields, so the \
         peephole's window was these structs' operands after all and nothing here was \
         widened: {windows:?}"
    );
}

// --------------------------------------------------------------- strings

/// A `String` constant, returned. The smallest program `Kind::Str` exists
/// for: `Inst::Const` pushes the `Handle` `FrameVm::new` allocated once, and
/// `Inst::Return` materialises it -- the one case `crossed_at_boundary` is
/// for and the six instructions the module docs list becoming seven.
#[test]
fn a_string_constant_is_returned() {
    agree(&main_of("String", "  \"hello\""));
}

/// Every operator `==`, `!=`, `<`, `<=`, `>` and `>=` lowers to `Inst::Binary`
/// for two `String`s, and this is `every_admitted_operator_agrees_on_all_three`
/// for that operand kind: equal, unequal-and-ordered, ordered across a
/// difference in *length*, and ordered across a multi-byte character, whose
/// first byte is what a byte-wise comparison has to get right for `café` to
/// stand after `cafe` the way `str`'s own `Ord` says it does.
#[test]
fn a_string_comparison_agrees_on_every_operator_the_lowering_emits() {
    for op in ["==", "!=", "<", "<=", ">", ">="] {
        for (a, b) in [
            (r#""abc""#, r#""abc""#),
            (r#""abc""#, r#""abd""#),
            (r#""ab""#, r#""abc""#),
            (r#""cafe""#, r#""café""#),
        ] {
            agree(&main_of("Bool", &format!("  {a} {op} {b}")));
        }
    }
}

/// `concat` over one of each of the four kinds it renders, and the rendering
/// is asserted against the `Vm` and the tree walk rather than against a
/// literal answer here -- so a drift in `write_float` or in how a `Bool` or a
/// literal segment renders would show up as a disagreement rather than as
/// nothing at all. The nested `"inner"` is itself a `String` operand, and a
/// literal segment of the outer string is a `Str` constant beside it, so this
/// is five operands and four kinds rather than four operands and four kinds.
#[test]
fn an_interpolated_string_renders_a_string_an_int_a_bool_and_a_float() {
    agree(&main_of(
        "String",
        r#"  "n={1} b={true} f={3.5} s={"inner"} and a literal tail""#,
    ));
}

/// A `String` survives a call that is a safepoint, held in a frame slot the
/// whole time, and the comparison at the end is what proves it rather than
/// merely running to completion: the left side is `Kind::Reference`, loaded
/// back out of the slot, and the right side is a fresh `Kind::Str` literal --
/// exactly the asymmetric case `Inst::Binary`'s admission and its
/// `debug_assert` both exist for.
#[test]
fn a_string_held_in_a_frame_slot_across_a_call_survives_and_compares_equal() {
    agree(
        "fn helper(n: Int) -> Int {\n  n + 1\n}\n\n\
         export fn main() -> Bool {\n  \
         var s = \"a string held across a call\"\n  \
         var n = helper(41)\n  \
         s == \"a string held across a call\" && n == 42\n\
         }\n",
    );
}

/// A struct with both a `String` field and an `Int` field, the `Int` field
/// rewritten two hundred times -- which is two hundred `copy_replacing`
/// allocations, each one carrying the `String` field's handle into a fresh
/// object -- and the label read back out at the end, agreeing with the `Vm`.
/// Rooting a string nested in a struct is the field's `Part::Nested`
/// unchanged from what a struct field of another struct already needed;
/// nothing here is new machinery, and this is the coverage that says so.
#[test]
fn a_string_held_in_a_struct_field_survives_two_hundred_field_writes() {
    agree(
        "struct Holder {\n  label: String\n  count: Int\n}\n\n\
         export fn main() -> Bool {\n  \
         var h = Holder(label: \"held in a struct field\", count: 0)\n  \
         var i = 0\n  \
         while i < 200 {\n    h.count = h.count + 1\n    i = i + 1\n  }\n  \
         h.label == \"held in a struct field\" && h.count == 200\n\
         }\n",
    );
}

/// A string whose length is not a multiple of eight bytes, so its tail's last
/// word is zero-padded past the string's own end, and one whose length is
/// exactly a multiple of eight, so the tail has no partial word at all --
/// `Shape::Str`'s packing exercised at both sides of the boundary a `%8`
/// could get wrong.
#[test]
fn a_strings_tail_is_packed_at_the_word_boundary_and_just_past_it() {
    agree(&main_of("String", "  \"1234567\""));
    agree(&main_of("String", "  \"12345678\""));
}

/// A non-ASCII string, so `Shape::Str`'s length word and its packing are
/// counting *bytes* -- fourteen of them for `café日本語`, since `é` is two
/// UTF-8 bytes and each of `日`, `本` and `語` is three -- and not the seven
/// `char`s the string holds. `str::as_bytes` is what both
/// `pack_string_words`'s packing and `crate::slot::string_bytes`'s unpacking
/// agree the layout means, and a packing that counted characters instead
/// would either truncate this string or read past its own tail.
#[test]
fn a_non_ascii_strings_tail_is_packed_by_bytes_not_by_characters() {
    agree(&main_of("String", "  \"café日本語\""));
}

/// **The positive half of a string's own rooting proof**, mirroring
/// `a_struct_in_a_frame_slot_survives_every_collection_of_a_run` for the kind
/// Phase C's struct test could not exercise. `s` is built with `{0}` in it so
/// that it is a `concat`'s answer rather than a `Str` constant -- a constant
/// would already be a permanent root of `FrameRoots::constants` and prove
/// nothing about the frame slot -- so `s` is kept alive by nothing but the
/// slot it stands in. The loop beside it allocates two hundred throwaway
/// `Junk` structs that reach nothing, so the run collects and frees real
/// garbage while `s` is the one live object the walk has to find through the
/// slot alone, and the closing comparison against a fresh literal is what
/// proves it survived with its bytes intact rather than merely surviving a
/// `Handle` that happened not to be reused yet.
#[test]
fn a_string_kept_alive_by_nothing_but_a_frame_slot_survives_every_collection() {
    let ready = ready(A_STRING_IN_A_SLOT);
    let (vm, collected) = crate::on_cove_stack(|| {
        (
            ready.on_vm(),
            ready.on_frame_collecting(RootScope::EveryWord),
        )
    })
    .expect("a thread to run Cove on");
    assert_eq!(
        collected.outcome, vm,
        "the VM and the 8-byte frame disagreed about a rooted string"
    );
    assert!(
        collected.collections > 0,
        "the run collected {} time(s), so it proves nothing about rooting",
        collected.collections
    );
    assert!(
        collected.freed_objects > 0,
        "the sweep reclaimed nothing, so the string that survived did not have to"
    );
}

/// **The mutation.** Take the frame's own words out of the walk and the
/// string in slot 0 is swept out from under the comparison that is still
/// using it -- `crate::slot::HandleHeap`'s own use-after-free message, dying
/// inside `FrameVm::compare_string_handles`'s `debug_assert` or, in release,
/// inside the `HandleHeap::word` the comparison reads bytes through. This is
/// ADR 0028 decision 8's "a handle slot is a root according to the frame
/// reference map" removed, and what it costs a `String` exactly as it costs a
/// struct.
#[test]
#[should_panic(expected = "names a swept object")]
fn dropping_the_frame_slots_from_the_walk_sweeps_the_string_that_lived_there() {
    let ready = ready(A_STRING_IN_A_SLOT);
    let _ = ready.on_frame_collecting(RootScope::WithoutFrameSlots);
}

const A_STRING_IN_A_SLOT: &str = "\
struct Junk {
  at: Int
}

export fn main() -> Bool {
  var s = \"kept alive by the frame slot and nothing else {0}\"
  var i = 0
  while i < 200 {
    var junk = Junk(at: i)
    i = junk.at + 1
  }
  s == \"kept alive by the frame slot and nothing else 0\"
}
";

/// **The dangerous half of the crossing, forced to happen rather than merely
/// possible.** The string is built with `{0}` in it so that it is a
/// `concat`'s answer and not a `Str` constant: a constant is a permanent root
/// of `FrameRoots::constants` from the moment `FrameVm::new` allocates it, so
/// returning one would survive any collection whether or not the crossing
/// itself rooted it, which would make the test vacuous. A `concat`'s answer
/// has no such standing root -- it is a fresh object named only by the word
/// `Inst::Return` is about to pop -- so its survival here is the crossing's
/// own rooting and nothing else's.
///
/// The string is also long enough that its tail is several words, and
/// `FrameVm::materialise_str` takes a GC safepoint before every one of them
/// through `FrameVm::string_word` -- so with `HandleHeap::stress` on, a real
/// collection runs *inside* `Inst::Return`'s handling, after the handle has
/// already been popped off the one stack and is a bare Rust local until
/// `FrameVm::pop_boundary_value`'s `with_root` registers it. The run
/// answering the right bytes rather than panicking on a swept object is what
/// says the root held for the whole stretch and not merely up to the first
/// safepoint.
#[test]
fn a_collection_taken_in_the_middle_of_a_boundary_crossing_does_not_lose_the_string() {
    let ready = ready(&main_of(
        "String",
        "  \"0123456789abcdefghijklmnopqrstuvwxyz {0}\"",
    ));
    let collected = crate::on_cove_stack(|| ready.on_frame_collecting(RootScope::EveryWord))
        .expect("a thread to run Cove on");
    assert_eq!(
        collected.outcome,
        Outcome::Answered(format!(
            "{:?}",
            Value::string("0123456789abcdefghijklmnopqrstuvwxyz 0")
        )),
        "the crossing lost or corrupted the string"
    );
    assert!(
        collected.collections > 0,
        "the run collected {} time(s), so nothing here proves a collection ran mid-crossing",
        collected.collections
    );
}

/// A program that is still refused, and the refusal names the operand it saw
/// rather than only naming `concat`: a struct interpolated has no `Display`
/// this backend can reuse without materialising an aggregate it does not
/// admit, so `Inst::Concat`'s own check refuses it by the same
/// `Kind::Reference` the boundary refuses everywhere else.
#[test]
fn string_interpolation_over_a_struct_is_refused_and_names_a_heap_object() {
    let ready = ready(
        "struct Cell {\n  at: Int\n}\n\nexport fn main() -> String {\n  \
         var cell = Cell(at: 1)\n  \"{cell}\"\n}\n",
    );
    let refused = match ready.admitted() {
        Ok(id) => panic!("interpolating a struct is refused, and it admitted {id:?}"),
        Err(refused) => refused,
    };
    assert!(
        refused.what.contains("a heap object"),
        "it was refused for `{}`, and `a heap object` was expected",
        refused.what
    );
}
