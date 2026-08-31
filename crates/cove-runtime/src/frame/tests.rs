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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cove_diag::{SourceMap, Span};
use cove_ir::Program as Ir;
use cove_sema::config::Config;
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program as Checked;

use super::*;
use crate::budget::{Budget, Cancellation, Limits};
use crate::clock::{Clock, VirtualTime};
use crate::host::{Console, Grants};
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
    let checked = match cove_sema::Compiler::new().compile(&package) {
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

/// Issue #212's first acceptance criterion: `benches/arith` and
/// `benches/call` execute over one `Vec<u64>` frame stack.
///
/// The whole entry, not a hand-written loop: this consumes the
/// `cove_ir::Program` `cove run --backend vm` consumes, and answers what the
/// other two backends answer.
#[test]
fn the_two_bench_rows_run_on_the_frame_and_agree_with_both_backends() {
    for name in ["arith", "call"] {
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
fn the_frame_executes_exactly_the_instructions_the_vm_executes() {
    for name in ["arith", "call"] {
        let ready = bench(name);
        let (_, instructions, _) = ready.on_frame_with(None, None);
        assert_eq!(
            instructions,
            ready.vm_instructions(),
            "`benches/{name}` ran a different number of instructions on the two backends"
        );
    }
}

/// Issue #212's hard constraint, as a number rather than as a sentence.
///
/// Eight `Value` operations for a run of two million turns, all of them in
/// the nine instructions after the loop — `scalar-to-value`, `const`,
/// `make-builtin assertEqual` over its two arguments, `try`, `pop`, `const
/// Unit`, `make-builtin Ok` over its one, and the `return` — and none of
/// them in the loop.
#[test]
fn the_hot_path_performs_no_value_operation() {
    for name in ["arith", "call"] {
        let ready = bench(name);
        let (_, _, materialized) = ready.on_frame_with(None, None);
        assert_eq!(
            materialized, 8,
            "`benches/{name}` materialized {materialized} value(s), and the epilogue is worth 8"
        );
    }
}

/// `benches/pure` is recursive `fib`, which is calls and branches and
/// nothing else, so the frame runs it too — and it is the row that says most
/// about what a call costs.
#[test]
fn pure_runs_on_the_frame_as_well() {
    let ready = bench("pure");
    ready
        .admitted()
        .unwrap_or_else(|why| panic!("`benches/pure` is admitted, but: {why}"));
    let (vm, frame) = crate::on_cove_stack(|| (ready.on_vm(), ready.on_frame()))
        .expect("a thread to run Cove on");
    assert_eq!(vm, frame);
}

/// Every other benchmark entry is refused, by name, before it runs.
///
/// Named individually rather than counted, because a refusal that changed
/// which construct it was about would otherwise go unnoticed.
#[test]
fn the_other_bench_rows_are_refused_by_name() {
    for (name, expected) in [
        ("hostheavy", "a Host call"),
        ("arrayget", "a collection"),
        ("chars", "a string"),
        ("field", "a struct"),
        ("method", "a struct"),
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

// ---------------------------------------------------------------- refusals

/// A construct outside the subset is refused before any side effect, and the
/// refusal names it.
#[test]
fn an_unsupported_construct_is_refused_before_the_run_begins() {
    for (source, expected) in [
        ("export fn main() -> String {\n  \"hello\"\n}\n", "a string"),
        (
            "struct P {\n  x: Int\n}\n\nexport fn main() -> Int {\n  P(x: 1).x\n}\n",
            "a struct",
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
    let ready = ready("export fn main() -> String {\n  \"hello\"\n}\n");
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
