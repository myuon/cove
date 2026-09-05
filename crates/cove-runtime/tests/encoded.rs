//! What the machine executes, and the four things the encoding promises.
//!
//! [Issue #245](https://github.com/myuon/cove/issues/245)'s Phase 5 made
//! [ADR 0041](../../../docs/adr/0041-a-slot-number-fits-in-sixteen-bits.md)'s
//! fixed-width form the only thing a run executes and deleted the `Inst`
//! dispatch loop that had stood beside it. Phase 3 asked for a comparison of
//! *values and errors, source spans, instruction and fuel counts, and
//! trace/replay*; each of the four is a test here rather than a paragraph in
//! a report, because a paragraph is true of the tree somebody measured and a
//! test is true of the tree somebody pushed.
//!
//! **The program is the benchmark itself**, `include_str!`d out of
//! `benches/arith/main.cove` rather than retyped. A copy would agree with the
//! machine about a program the benchmark no longer is.
//!
//! # What the four are compared against, now that there is one loop
//!
//! Through Phase 4 each of these ran the same program on both loops and
//! compared them. That comparison is gone with the loop, and what replaced
//! it is not weaker in the place it mattered:
//!
//! - **Values, errors and spans** are held to the **tree-walking oracle**,
//!   which ADR 0034 makes the definition of what a Cove program means. That
//!   was always the stronger comparison — two loops agreeing with each other
//!   and not with the language would have passed the old one — and
//!   `crates/cove-cli/tests/differential.rs` applies it to the whole corpus.
//! - **Instruction and fuel counts** are held to *each other* and to a
//!   pinned figure. `fuel_spent` and `instructions` are asserted **equal**,
//!   not merely plausible: ADR 0041's encoding is 1:1, so one encoded
//!   instruction has to be one instruction and one unit of fuel, and
//!   fourteen million of each agreeing exactly is what
//!   [ADR 0040](../../../docs/adr/0040-a-bound-outlives-its-backend.md)'s
//!   bounds are stated in. The oracle cannot answer this — it counts no
//!   instructions — so the number itself is pinned, and a lowering change
//!   that halved the work has to say so here.
//!
//! The span test is the one the encoding's whole 1:1 argument rests on.
//! `Function::spans` is a parallel array indexed by pc and no instruction
//! carries a span, so "bytecode pc is IR pc" is only true if a failure in the
//! machine reports the span the oracle reports from the same source.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cove_diag::SourceMap;
use cove_ir::bytecode::Op;
use cove_ir::{CmpOp, Compare};
use cove_runtime::interp::Interpreter;
use cove_runtime::trace::{TraceEvent, TraceSink};
use cove_runtime::{Budget, Grants, HostRegistry, Limits, Runtime, RuntimeError, Value, Vm};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::Compiler;

/// The benchmark, as the benchmark. See this file's own docs.
const ARITH: &str = include_str!("../../../benches/arith/main.cove");

/// A program that fails at a known instruction.
///
/// `i += 1` on `Int`'s largest value lowers to `add.int.imm`, one instruction
/// whose span is the whole of what the test reads — small enough that a
/// disagreement about where the failure happened cannot be a disagreement
/// about which instruction failed.
const OVERFLOWS: &str = "\
export fn main() -> Result<Unit, Error> {
  var i = 9223372036854775807
  i += 1
  Ok(())
}
";

// ------------------------------------------------------- the four comparisons

/// Values and errors: the benchmark answers what it is meant to.
#[test]
fn arith_answers_what_the_benchmark_asserts() {
    let ran = run(ARITH);
    assert_eq!(described(&ran.answer), "Ok(Ok(()))");
}

/// Instruction and fuel counts: exactly equal, and the figure itself.
#[test]
fn one_encoded_instruction_is_one_unit_of_fuel() {
    let ran = run(ARITH);
    // The accounting ADR 0024 and the immediate forms of #244 are both stated
    // against. Asserted rather than inferred, because it is the property the
    // 1:1 encoding has to preserve and nothing else in this crate checks that
    // an *encoded* instruction costs one.
    assert_eq!(ran.fuel_spent, ran.instructions);
    // And the figure itself, so that a lowering change that halved the work
    // is not silently accepted by a test comparing two counters that would
    // move together.
    assert_eq!(ran.instructions, 14_285_740);
}

/// Source spans: a failing program points where the oracle points.
#[test]
fn a_failure_points_at_the_source_the_oracle_points_at() {
    let ran = run(OVERFLOWS);
    let oracle = on_the_oracle(OVERFLOWS);
    let (left, right) = (failure(&oracle), failure(&ran.answer));
    assert_eq!(right.message, left.message);
    assert_eq!(right.message, "`Int` addition overflowed");
    // The span, and not only that there is one: `Function::spans` is indexed
    // by pc and the encoding is 1:1, so this is where "bytecode pc is IR pc"
    // is either true or a claim.
    assert_eq!(right.span, left.span);
    assert!(
        right.span.is_some(),
        "an overflow reports where it happened"
    );
}

/// Trace and replay: the recording a run writes.
#[test]
fn the_run_writes_the_recording_a_run_writes() {
    let ran = run(ARITH);
    // A trace is pinned by shape: an entry entered, an entry left, what the
    // heap did, and how the run ended.
    assert_eq!(
        steady(&ran.events),
        vec![
            "EntryEnter { module: \"m\", function: \"main\" }".to_string(),
            "EntryExit { module: \"m\", function: \"main\" }".to_string(),
            "HeapSummary { collections: 0, allocated_words: Some(0), capacity_words: Some(0) }"
                .to_string(),
            "RunEnded { outcome: Success, message: None }".to_string(),
        ]
    );
}

// ------------------------------------------- families that were once refused

/// The program Phase 3 refused runs, and answers what the oracle answers.
///
/// `total = total + i` is `add.int`, a slot-operand family Phase 3 did not
/// implement, and this test asserted the refusal until Phase 4 built it. It is
/// kept, inverted, because a refusal that became an answer is the whole of
/// what that phase did — and because the program is still the smallest one
/// that distinguishes the two arithmetic families.
#[test]
fn the_family_phase_three_refused_now_runs() {
    let source = "\
export fn main() -> Result<Unit, Error> {
  var total = 0
  var i = 0
  while i < 3 {
    total = total + i
    i += 1
  }
  assertEqual(total, 3)?
  Ok(())
}
";
    let ran = run(source);
    assert_eq!(described(&ran.answer), described(&on_the_oracle(source)));
    assert_eq!(described(&ran.answer), "Ok(Ok(()))");
}

/// The heap, the collector's roots, and a closure call.
///
/// One program rather than three, because what is being checked is not that
/// each instruction works — `differential.rs` runs the whole corpus for that —
/// but that a run mixing allocation, field access, element access and a
/// closure call reaches the answer the oracle reaches, having charged itself
/// one unit of fuel per instruction while doing it.
#[test]
fn the_heap_and_a_closure_answer_what_the_oracle_answers() {
    let source = "\
struct Point {
  x: Int
  y: Int
}

export fn main() -> Result<Unit, Error> {
  var points = Vector.of()
  var i = 0
  while i < 64 {
    points.push(Point(x: i, y: i * 2))
    i += 1
  }
  let doubled = points.map(fn(p) { p.x + p.y })
  var total = 0
  var at = 0
  while at < doubled.length() {
    total += doubled.get(at).unwrapOr(0)
    at += 1
  }
  assertEqual(total, 6048)?
  assertEqual(points.length(), 64)?
  Ok(())
}
";
    let ran = run(source);
    assert_eq!(described(&ran.answer), described(&on_the_oracle(source)));
    assert_eq!(described(&ran.answer), "Ok(Ok(()))");
    assert_eq!(ran.fuel_spent, ran.instructions);
}

/// A failure inside a call points at the place the oracle points at.
///
/// The span is the interesting half: the failure happens in a callee, so what
/// is compared is that the loop kept `pc` truthful across a frame push and
/// reported the *callee's* span rather than the call site's.
#[test]
fn a_failure_inside_a_call_reports_the_same_place_on_both_paths() {
    let source = "\
fn divide(a: Int, b: Int) -> Int {
  a / b
}

export fn main() -> Result<Unit, Error> {
  let answer = divide(1, 0)
  assertEqual(answer, 0)?
  Ok(())
}
";
    let ran = run(source);
    let oracle = on_the_oracle(source);
    let (left, right) = (failure(&oracle), failure(&ran.answer));
    assert_eq!(right.message, left.message);
    assert_eq!(right.span, left.span);
    assert!(right.span.is_some(), "a division by zero reports where");
}

/// The comparison opcodes no corpus program reaches, reached.
///
/// `crates/cove-cli/tests/bytecode_corpus.rs` reports that 84 of the 100
/// opcodes appear in a program the repository keeps, and names the sixteen
/// that do not. Twelve of those sixteen cannot appear in a valid program at
/// all — eight are orderings of a `Bool` or an identity, which
/// `cove_ir::verify` refuses, and four have no emission site in the lowering.
/// **Four are legal, emittable, and simply absent from the corpus**, so the
/// differential harness cannot cover them however many programs it runs.
///
/// Three of the four are here, with the encoding asserted rather than
/// assumed: the test lowers the fixture, encodes it, and checks the opcodes
/// are present before running it. Without that it would be a test that
/// passed whatever the lowering chose to emit. The fourth,
/// `Cmp(Identity, Ne)`, has no source form this fixture could reach.
#[test]
fn the_comparisons_no_corpus_program_reaches_run() {
    let source = "\
export fn main() -> Result<Unit, Error> {
  let a = 1.5
  let b = 0.5
  let yes = true
  let no = false
  assertEqual(a > b, true)?
  assertEqual(\"beta\" > \"alpha\", true)?
  assertEqual(yes != no, true)?
  Ok(())
}
";
    let (sources, checked) = check(source);
    let program = cove_ir::lower(&checked, &sources, &cove_sema::HostSchemas::new())
        .expect("the fixture lowers");
    let encoded = cove_ir::bytecode::encode_program(&program).expect("it encodes");
    let reached: std::collections::BTreeSet<u8> = encoded
        .functions
        .iter()
        .flatten()
        .map(|held| held.opcode())
        .collect();
    for op in [
        Op::Cmp(Compare::Float, CmpOp::Gt),
        Op::Cmp(Compare::Str, CmpOp::Gt),
        Op::Cmp(Compare::Bool, CmpOp::Ne),
    ] {
        assert!(
            reached.contains(&op.number()),
            "this fixture is meant to reach `{op:?}` and does not"
        );
    }

    let ran = run(source);
    assert_eq!(described(&ran.answer), described(&on_the_oracle(source)));
    assert_eq!(described(&ran.answer), "Ok(Ok(()))");
}

// ------------------------------------------------------------------ the harness

struct Ran {
    answer: Result<Value, RuntimeError>,
    instructions: u64,
    fuel_spent: u64,
    events: Vec<TraceEvent>,
}

/// What the machine made of one program.
fn run(source: &str) -> Ran {
    let (sources, checked) = check(source);
    let lowered = Arc::new(
        cove_ir::lower(&checked, &sources, &cove_sema::HostSchemas::new())
            .expect("the fixture lowers"),
    );
    let recorded = Arc::new(Recorded::default());
    let mut hosts = HostRegistry::new(Grants::new(Vec::<&str>::new()));
    hosts.set_budget(Budget::new(Limits::default()));
    let hosts = Arc::new(hosts);
    let runtime = Runtime::new(
        Arc::clone(&checked),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    )
    .with_trace(Arc::clone(&recorded) as Arc<dyn TraceSink>);

    let mut vm = Vm::new(&runtime, &hosts, &lowered);
    let answer = vm.run_entry("m", "main", Vec::new());
    let instructions = vm.instructions();
    let fuel_spent = hosts
        .with_budget(|budget| budget.fuel_spent())
        .expect("a budget was installed");
    let events = recorded.0.lock().unwrap().clone();
    Ran {
        answer,
        instructions,
        fuel_spent,
        events,
    }
}

/// The same program on the tree-walking oracle, which ADR 0034 makes the
/// definition of what it means.
///
/// The comparison the two loops used to make of each other is made here
/// instead, against the thing that decides. It is only ever asked about the
/// small fixtures: the oracle counts no instructions and charges fuel on its
/// own schedule, so the counts are pinned rather than compared, and running
/// `arith`'s fourteen million instructions through a tree walker would buy
/// nothing this file does not already assert.
fn on_the_oracle(source: &str) -> Result<Value, RuntimeError> {
    let (sources, checked) = check(source);
    let mut hosts = HostRegistry::new(Grants::new(Vec::<&str>::new()));
    hosts.set_budget(Budget::new(Limits::default()));
    let hosts = Arc::new(hosts);
    let runtime = Runtime::new(
        Arc::clone(&checked),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    Interpreter::new(&runtime).run_entry("m", "main", Vec::new())
}

/// Parses and checks `source` as the one module `m`.
fn check(source: &str) -> (Arc<SourceMap>, Arc<cove_sema::resolve::Program>) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("m/main.cove");
    let file = sources.add(path.clone(), source);
    let ast = cove_syntax::parse_file(&sources, file).expect("the fixture parses");
    let package = Package {
        root: PathBuf::from("."),
        config: Default::default(),
        modules: BTreeMap::from([(
            "m".to_string(),
            Module {
                name: "m".to_string(),
                dir: PathBuf::from("m"),
                units: vec![Unit { file, path, ast }],
            },
        )]),
    };
    let checked = Compiler::new()
        .compile(&package)
        .expect("the fixture checks");
    (Arc::new(sources), Arc::new(checked))
}

/// A run's answer in words that do not name a runtime representation.
fn described(answer: &Result<Value, RuntimeError>) -> String {
    match answer {
        Ok(value) => format!("Ok({value})"),
        Err(error) => format!("Err({})", error.message),
    }
}

/// The failure a run stopped with, for a test that is about the failure.
fn failure(answer: &Result<Value, RuntimeError>) -> &RuntimeError {
    match answer {
        Ok(value) => panic!("this program fails, and it answered {value}"),
        Err(error) => error,
    }
}

/// The events with every measured duration dropped.
///
/// A trace carries what a run *spent*, which is a clock reading and differs
/// between any two runs of anything. What is compared is the rest: which
/// events, in which order, saying what. Dropping the durations here rather
/// than rounding them is deliberate — a tolerance is a number somebody has to
/// keep right, and the two paths make no claim about being equally fast.
fn steady(events: &[TraceEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| match event {
            TraceEvent::EntryExit {
                module, function, ..
            } => format!("EntryExit {{ module: {module:?}, function: {function:?} }}"),
            TraceEvent::HeapSummary {
                collections,
                allocated_words,
                capacity_words,
                ..
            } => format!(
                "HeapSummary {{ collections: {collections}, allocated_words: {allocated_words:?}, capacity_words: {capacity_words:?} }}"
            ),
            other => format!("{other:?}"),
        })
        .collect()
}

/// Every event a run wrote, in order.
#[derive(Default)]
struct Recorded(Mutex<Vec<TraceEvent>>);

impl TraceSink for Recorded {
    fn record(&self, event: TraceEvent) {
        self.0.lock().unwrap().push(event);
    }
}
