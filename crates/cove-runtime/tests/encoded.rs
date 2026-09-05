//! The encoded execution path, held to the enum path it will one day replace.
//!
//! [Issue #245](https://github.com/myuon/cove/issues/245)'s Phase 3 executes
//! the `arith` benchmark from
//! [ADR 0041](../../../docs/adr/0041-a-slot-number-fits-in-sixteen-bits.md)'s
//! fixed-width encoding, and asks for a comparison of *values and errors,
//! source spans, instruction and fuel counts, and trace/replay*. Each of the
//! four is a test here rather than a paragraph in a report, because a
//! paragraph is true of the tree somebody measured and a test is true of the
//! tree somebody pushed.
//!
//! **The program is the benchmark itself**, `include_str!`d out of
//! `benches/arith/main.cove` rather than retyped. A copy would agree with the
//! encoded path about a program the benchmark no longer is.
//!
//! Two of the four are worth saying why they are strong.
//!
//! `fuel_spent` and `instructions` are asserted *equal*, not merely
//! plausible. ADR 0041's encoding is 1:1 — one `Inst` is one `EncodedInst` —
//! so one encoded instruction has to be one instruction and one unit of fuel,
//! and fourteen million of each agreeing exactly is a check that the two
//! loops took the same branches at every one of them. A path that folded two
//! instructions into one, or that charged a safepoint on a different stride,
//! would be found here and nowhere else in this file.
//!
//! The span test is the one the encoding's whole 1:1 argument rests on.
//! `Function::spans` is a parallel array indexed by pc and neither loop
//! carries a span in the instruction, so "bytecode pc is IR pc" is only true
//! if a failure reports the same span through both. It is asserted on a
//! program built to fail, because `arith` succeeds and a passing program
//! reads no span at all.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cove_diag::SourceMap;
use cove_runtime::trace::{TraceEvent, TraceSink};
use cove_runtime::{Budget, Grants, HostRegistry, Limits, Runtime, RuntimeError, Value, Vm};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::Compiler;

/// The benchmark, as the benchmark. See this file's own docs.
const ARITH: &str = include_str!("../../../benches/arith/main.cove");

/// A program that fails at an instruction both paths run.
///
/// `i += 1` on `Int`'s largest value lowers to `add.int.imm`, which is one of
/// the opcodes the encoded path implements, so both loops reach the same
/// overflow at the same pc. That is what makes it a span comparison rather
/// than a comparison of one path's failure with the other's refusal.
const OVERFLOWS: &str = "\
export fn main() -> Result<Unit, Error> {
  var i = 9223372036854775807
  i += 1
  Ok(())
}
";

// ------------------------------------------------------- the four comparisons

/// Values and errors: the benchmark answers the same thing.
#[test]
fn arith_answers_what_it_answers_on_the_enum_path() {
    let enumerated = run(ARITH, Path::Enum);
    let encoded = run(ARITH, Path::Encoded);
    assert_eq!(described(&encoded.answer), described(&enumerated.answer));
    assert_eq!(described(&enumerated.answer), "Ok(Ok(()))");
}

/// Instruction and fuel counts: exactly equal, both of them.
#[test]
fn arith_dispatches_and_is_charged_for_the_same_instructions() {
    let enumerated = run(ARITH, Path::Enum);
    let encoded = run(ARITH, Path::Encoded);
    assert_eq!(encoded.instructions, enumerated.instructions);
    assert_eq!(encoded.fuel_spent, enumerated.fuel_spent);
    // One instruction is one unit of fuel on both, which is the accounting
    // ADR 0024 and the immediate forms of #244 are both stated against. It is
    // asserted rather than inferred because two equal-but-wrong counters
    // would satisfy the two lines above.
    assert_eq!(enumerated.fuel_spent, enumerated.instructions);
    assert_eq!(encoded.fuel_spent, encoded.instructions);
    // And the figure itself, so that a lowering change that halved the work
    // is not silently accepted by a test that only compares two paths with
    // each other.
    assert_eq!(encoded.instructions, 14_285_740);
}

/// Source spans: a failing program points at the same place through both.
#[test]
fn a_failure_points_at_the_same_source_through_both_paths() {
    let enumerated = run(OVERFLOWS, Path::Enum);
    let encoded = run(OVERFLOWS, Path::Encoded);
    let (left, right) = (failure(&enumerated.answer), failure(&encoded.answer));
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

/// Trace and replay: the recording both paths write agrees.
#[test]
fn both_paths_write_the_same_recording() {
    let enumerated = run(ARITH, Path::Enum);
    let encoded = run(ARITH, Path::Encoded);
    assert_eq!(steady(&encoded.events), steady(&enumerated.events));
    // A trace that agreed because it was empty would prove nothing, so the
    // shape is pinned as well: an entry entered, an entry left, what the heap
    // did, and how the run ended.
    assert_eq!(
        steady(&enumerated.events),
        vec![
            "EntryEnter { module: \"m\", function: \"main\" }".to_string(),
            "EntryExit { module: \"m\", function: \"main\" }".to_string(),
            "HeapSummary { collections: 0, allocated_words: Some(0), capacity_words: Some(0) }"
                .to_string(),
            "RunEnded { outcome: Success, message: None }".to_string(),
        ]
    );
}

// ------------------------------------------------------------ what is refused

/// A program the encoded path does not implement is refused, by name, before
/// anything runs.
#[test]
fn an_unimplemented_opcode_is_refused_and_never_handed_back() {
    // `total + i` between two slots is `add.int`, a different family from the
    // `add.int.imm` the encoded path implements, so this is refused — while
    // `i += 1`, one line below it, is not. One function, so that the pc the
    // scan stops at is not a question about declaration order.
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
    let refused = prepared(source, Path::Encoded);
    let error = refused.expect_err("this program is refused");
    assert_eq!(
        error.message,
        "the encoded execution path does not run `Arith(Int, Add)` yet"
    );
    // Pointed at source, and at the instruction, so that a reader is told
    // which family to build next rather than a number.
    assert!(error.span.is_some(), "a refusal points at the source");
    assert!(
        error
            .help
            .as_deref()
            .unwrap_or_default()
            .contains("add.int s"),
        "a refusal names the instruction: {:?}",
        error.help
    );
    // And the same program runs on the ordinary path, which is what makes the
    // refusal a statement about this phase rather than about the program.
    assert_eq!(described(&run(source, Path::Enum).answer), "Ok(Ok(()))");
}

// ------------------------------------------------------------------ the harness

#[derive(Clone, Copy, PartialEq, Eq)]
enum Path {
    Enum,
    Encoded,
}

struct Ran {
    answer: Result<Value, RuntimeError>,
    instructions: u64,
    fuel_spent: u64,
    events: Vec<TraceEvent>,
}

/// What one path made of one program.
fn run(source: &str, path: Path) -> Ran {
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

    let mut vm = match path {
        Path::Enum => Vm::new(&runtime, &hosts, &lowered),
        Path::Encoded => {
            Vm::encoded(&runtime, &hosts, &lowered).expect("this program encodes and is covered")
        }
    };
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

/// The same setup, stopping at whether the encoded path will take the program.
fn prepared(source: &str, path: Path) -> Result<(), RuntimeError> {
    let (sources, checked) = check(source);
    let lowered = Arc::new(
        cove_ir::lower(&checked, &sources, &cove_sema::HostSchemas::new())
            .expect("the fixture lowers"),
    );
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(
        Arc::clone(&checked),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    match path {
        Path::Enum => Ok(()),
        Path::Encoded => Vm::encoded(&runtime, &hosts, &lowered).map(|_| ()),
    }
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
