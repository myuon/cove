//! Closures: what a capture holds, what a closure renders and compares as,
//! and what happens when a builtin calls one back.
//!
//! The callback cases cross the boundary twice — the VM calls a builtin,
//! which calls a Cove closure, which runs on the VM — so a failure inside one
//! has to arrive at the caller as its own failure rather than as the
//! builtin's.

use super::*;

/// A closure holds what it captured and answers with it, however far the
/// value has travelled from the frame that made it.
#[test]
fn a_returned_closure_still_holds_what_it_captured() {
    assert_eq!(
        value_of(
            "Int",
            "fn adder(by: Int) -> fn(Int) -> Int {\n  fn(n: Int) {\n    n + by\n  }\n}\n",
            "  let add = adder(3)\n  add(4) + add(10)"
        ),
        "Int(20)"
    );
}

/// The oracle captures by value at creation time, so assigning to the
/// binding afterwards does not change what the closure sees — including
/// where the binding is a `var` one, which is the case the place model
/// rests on.
#[test]
fn a_capture_is_the_value_the_binding_held_when_the_closure_was_written() {
    assert_eq!(
        value_of(
            "Int",
            "",
            "  var b = 10\n  let g = fn() {\n    b\n  }\n  b = 20\n  g() + b"
        ),
        "Int(30)"
    );
}

/// A closure is a value: it is bound, passed, returned, and called, and
/// each of those is the same value on both backends.
#[test]
fn a_closure_is_passed_and_called_like_any_other_value() {
    assert_eq!(
        value_of(
            "Int",
            "fn apply(v: Int, t: fn(Int) -> Int) -> Int {\n  t(v)\n}\n",
            "  let double = fn(n: Int) {\n    n * 2\n  }\n  apply(5, double) + apply(5, fn(n: Int) { n + 1 })"
        ),
        "Int(16)"
    );
}

/// A declared function used as a value is a closure over nothing, and
/// calling it through the value reaches the same body a direct call
/// does.
#[test]
fn a_function_used_as_a_value_answers_what_a_direct_call_answers() {
    assert_eq!(
        value_of(
            "Int",
            "fn twice(n: Int) -> Int {\n  n * 2\n}\n",
            "  let g = twice\n  twice(3) + g(4)"
        ),
        "Int(14)"
    );
}

/// A closure renders and compares the way the oracle's does: `<fn>`, and
/// equal to nothing including itself.
#[test]
fn a_closure_renders_and_compares_as_the_oracle_says() {
    let outcome = agree_main(
        "Result<Unit, Error>",
        "  let h = fn() {\n    1\n  }\n  let i = fn() {\n    1\n  }\n  println(\"{h} {h == h} {h == i}\")?\n  Ok(())",
    );
    assert_eq!(outcome.output, "<fn> false false\n");
}

/// A higher-order builtin runs its callback by entering the loop again,
/// and the run carries on afterwards exactly where it was.
///
/// `mapError` is the one the language has, and it reads the callback's
/// arity to decide whether to hand it the error it replaces — which is
/// why a lowered closure has to answer that question the way an
/// interpreted one does.
#[test]
fn a_builtin_runs_a_callback_and_the_run_continues() {
    let outcome = agree_main(
        "Result<Unit, Error>",
        "  let n = 7\n  \
         let a = Int.parse(\"x\").mapError {\n    Error(\"bad {n}\")\n  }\n  \
         let b = Int.parse(\"3\").mapError {\n    Error(\"unreached\")\n  }\n  \
         println(\"{a} {b} {n}\")?\n  Ok(())",
    );
    assert_eq!(outcome.output, "Err(bad 7) Ok(3) 7\n");
}

/// A callback that fails carries its failure out, and the failure is the
/// one the callback raised rather than anything the boundary added.
#[test]
fn a_callback_that_fails_ends_the_run_with_its_own_failure() {
    let (sources, checked) = checked_module(
        "use console.println\n\nexport fn main() -> Result<Int, Error> {\n  \
         Int.parse(\"x\").mapError {\n    Error(\"{1 / 0}\")\n  }\n}\n",
    );
    let (interpreted, lowered) = on_both(&checked, &sources, "m", None);
    assert_eq!(interpreted.error().message, "`Int` division by zero");
    assert_eq!(lowered.error().message, interpreted.error().message);
    assert_eq!(lowered.error().span, interpreted.error().span);
}

/// The four higher-order sequence methods, on both sequences, on both
/// backends.
///
/// `mapError` above enters the loop again once per value; these enter it
/// once per element and once per comparison, which is the first thing in
/// the language that re-enters an evaluator in a loop from inside one
/// instruction. Both `Callable` implementations answer the same walk.
#[test]
fn a_sequence_walks_the_way_the_interpreter_walks() {
    let outcome = agree_main(
        "Result<Unit, Error>",
        "  let fixed = [3, 1, 2]\n  \
         var growable = Vector.of(3, 1, 2)\n  \
         println(\"{fixed.map(fn(n) { n * 2 })} {growable.map(fn(n) { n * 2 })}\")?\n  \
         println(\"{fixed.filter(fn(n) { n > 1 })} {growable.filter(fn(n) { n > 1 })}\")?\n  \
         println(\"{fixed.fold(0, fn(t, n) { t + n })} {growable.fold(0, fn(t, n) { t + n })}\")?\n  \
         println(\"{fixed.sorted(by: fn(a, b) { a < b })} {growable.sorted(by: fn(a, b) { a < b })}\")?\n  \
         println(\"{fixed} {growable}\")?\n  Ok(())",
    );
    assert_eq!(
        outcome.output,
        "[6, 2, 4] [6, 2, 4]\n[3, 2] [3, 2]\n6 6\n[1, 2, 3] [1, 2, 3]\n[3, 1, 2] [3, 1, 2]\n"
    );
}

/// A sort long enough that the merge runs more than one pass, so the
/// order the comparisons are made in is what the two backends have to
/// agree about and not only the answer.
///
/// The comparison prints, which is what makes the order observable: a
/// merge that took its runs in a different order on one backend would
/// print a different sequence of pairs even where the sorted array came
/// out the same.
#[test]
fn a_sort_makes_the_same_comparisons_in_the_same_order_on_both() {
    let outcome = agree_main(
        "Result<Unit, Error>",
        "  let items = [5, 3, 8, 1, 9, 2, 7, 4]\n  \
         let sorted = items.sorted(by: fn(a, b) {\n    \
         let said = println(\"{a}?{b}\")\n    a < b\n  })\n  \
         println(\"{sorted}\")?\n  Ok(())",
    );
    assert!(
        outcome.output.ends_with("[1, 2, 3, 4, 5, 7, 8, 9]\n"),
        "{}",
        outcome.output
    );
    assert_eq!(outcome.output.lines().count(), 18);
}

/// A comparison that fails stops both backends at the same byte, with
/// nothing half-sorted answered.
#[test]
fn a_comparison_that_fails_stops_both_backends_alike() {
    let (sources, checked) = checked_module(
        "use console.println\n\nexport fn main() -> Result<Array<Int>, Error> {\n  \
         let zero = 0\n  Ok([3, 1, 2].sorted(by: fn(a, b) { a / zero < b }))\n}\n",
    );
    let (interpreted, lowered) = on_both(&checked, &sources, "m", None);
    assert_eq!(interpreted.error().message, "`Int` division by zero");
    assert_eq!(lowered.error().message, interpreted.error().message);
    assert_eq!(lowered.error().span, interpreted.error().span);
}
