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

/// A capture the checker settled as `Bool` takes a scalar slot too, and the
/// call puts the tag back where the closure's list had one.
///
/// `Function::capture_kinds` is what says which stack a capture lands in,
/// and the value that travels is a `Value` either way — so this is the test
/// that a `Bool` capture comes back a `Bool` and not an `Int`, which the two
/// renderings would disagree about.
#[test]
fn a_bool_capture_lands_in_a_scalar_slot_and_keeps_its_tag() {
    assert_eq!(
        agree(
            "export fn main() -> String {\n  let on = true\n  let f: fn() -> Bool = fn() {\n    on\n  }\n  \"{f()} {f()}\"\n}\n"
        )
        .value(),
        "Str(\"true true\")"
    );
}

/// Captures of both kinds in one closure land on their own stacks, in the
/// order the closure lists them, and the body reads each where it was put.
///
/// The two counters `Vm::enter_value_call` fills the frame with are what
/// this exercises: the value captures are dense from `capture_base` and the
/// scalar ones dense from scalar slot 0, whatever order the two are
/// interleaved in.
#[test]
fn a_closure_over_a_scalar_and_a_value_fills_both_windows() {
    assert_eq!(
        agree(
            "export fn main() -> String {\n  let n = 2\n  let label = \"x\"\n  let flag = true\n  let other = \"y\"\n  let f: fn(Int) -> String = fn(k) {\n    \"{label}{other}{n + k} {flag}\"\n  }\n  \"{f(1)} {f(10)}\"\n}\n"
        )
        .value(),
        "Str(\"xy3 true xy12 true\")"
    );
}

/// One lambda, two lowerings of the body around it, and one set of capture
/// slots between them.
///
/// `crate::lower` numbers a lambda by its syntactic site, so a declaration
/// reached both directly and through a value lowers *two* specialisations of
/// itself — its own convention, where an `Int` parameter is a scalar slot,
/// and the general one, where every argument is a value — while the lambda
/// inside it stays one function with one `capture_kinds`. So the two
/// `make-closure` sites can disagree about the representation the capture
/// had where it stood, and the callee's answer is the one that counts.
///
/// That is sound because what travels is a `Value` on both roads and the
/// checker's type is the same on both, so a disagreement costs a conversion
/// and cannot cost an answer. This is the program that has both roads in it.
#[test]
fn one_lambda_reached_from_two_specialisations_of_its_body_agrees_with_itself() {
    assert_eq!(
        agree(
            "fn adder(by: Int) -> fn(Int) -> Int {\n  fn(n: Int) {\n    n + by\n  }\n}\n\nexport fn main() -> Int {\n  let direct = adder(3)\n  let indirect: fn(Int) -> fn(Int) -> Int = adder\n  let viaValue = indirect(30)\n  direct(1) + viaValue(1)\n}\n"
        )
        .value(),
        "Int(35)"
    );
}

/// A closure over a `var` parameter of a settled scalar type captures the
/// value the place named, and that value is a capture like any other.
///
/// The read is a `place-read`, which answers a `Value` whatever stack the
/// place is rooted at, so the capture's kind is the value stack — which is
/// what `Body::lambda` records for a `SlotKind::Place` binding.
#[test]
fn a_capture_read_through_a_place_is_a_value_capture() {
    assert_eq!(
        agree(
            "fn watch(var n: Int) -> Int {\n  let f: fn(Int) -> Int = fn(k) {\n    n + k\n  }\n  n += 100\n  f(1) + n\n}\n\nexport fn main() -> Int {\n  var x = 7\n  watch(var x)\n}\n"
        )
        .value(),
        "Int(115)"
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
