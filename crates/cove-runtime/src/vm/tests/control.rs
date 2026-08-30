//! `if`, `while`, `for`, `break` and `continue`, and what a construct answers
//! when it is lowered for its value rather than for its effect.
//!
//! The stack discipline is as much the subject as the answer is: a `break`
//! out of a half-evaluated expression, a loop whose body is entered zero
//! times, and a recursive call that must not see the caller's scalar slots
//! are each a way to leave one of the two stacks holding something the next
//! instruction did not expect.

use super::*;

#[test]
fn an_if_answers_the_branch_that_ran() {
    assert_eq!(expression("Int", "if 1 < 2 { 10 } else { 20 }"), "Int(10)");
    assert_eq!(expression("Int", "if 1 > 2 { 10 } else { 20 }"), "Int(20)");
    assert_eq!(
        expression("Unit", "if 1 < 2 {\n    let seen = 1\n  }"),
        "Unit"
    );
}

#[test]
fn a_while_loop_counts_and_leaves() {
    assert_eq!(
        agree_main(
            "Int",
            "  var total = 0\n  var i = 0\n  while i < 5 {\n    total += i\n    i += 1\n  }\n  total"
        )
        .value(),
        "Int(10)"
    );
}

/// A frame whose slots are in both stacks answers what the interpreter
/// answers.
///
/// `total` and `i` are `Int` and live in the scalar stack; `label` is a
/// `String` and stays where every slot used to be. The two windows are
/// numbered independently, so `label`'s value slot and `total`'s scalar
/// slot can share a number without naming the same storage, which is
/// what `cove_ir::lower::validate` proved and what this runs.
#[test]
fn a_frame_with_slots_in_both_stacks_answers_what_the_interpreter_answers() {
    assert_eq!(
        agree_main(
            "String",
            "  let label = \"n=\"\n  var total = 0\n  var i = 0\n  while i < 5 {\n    total += i\n    i += 1\n  }\n  \"{label}{total}\""
        )
        .value(),
        "Str(\"n=10\")"
    );
}

/// A `Bool` the checker settled is a scalar too, and the jump reads it
/// where it stands.
#[test]
fn a_settled_bool_is_a_scalar_slot_and_a_condition_reads_it_there() {
    for (n, expected) in [(20, "Int(1)"), (2, "Int(2)")] {
        assert_eq!(
            agree_main(
                "Int",
                &format!("  let n = {n}\n  let big = n > 10\n  if big {{\n    1\n  }} else {{\n    2\n  }}")
            )
            .value(),
            expected
        );
    }
}

/// A `break` written inside a half-evaluated scalar expression takes what
/// it left on the scalar stack with it.
///
/// The loop's exit is reached at the depths the loop runs at, on both
/// stacks: `total +` has already pushed `total`, so leaving without
/// discarding it would reach the instruction after the loop one scalar
/// deep and `validate` would have refused the function. This runs it.
#[test]
fn a_break_inside_a_half_evaluated_scalar_expression_leaves_nothing_behind() {
    assert_eq!(
        agree_main(
            "Int",
            "  var total = 0\n  var i = 0\n  while i < 10 {\n    i += 1\n    total += if i == 3 {\n      break\n    } else {\n      i\n    }\n  }\n  total"
        )
        .value(),
        "Int(3)"
    );
}

/// Each call opens its own scalar window, so a recursion's scalar locals
/// do not reach each other.
#[test]
fn recursion_gives_every_frame_its_own_scalar_slots() {
    assert_eq!(
        agree(
            "fn down(n: Int) -> Int {\n  let here = n * 2\n  if n <= 0 {\n    0\n  } else {\n    here + down(n - 1)\n  }\n}\n\nexport fn main() -> Int {\n  down(4)\n}\n"
        )
        .value(),
        "Int(20)"
    );
}

/// A `for` walks every collection the language has, and walks each one
/// the way the interpreter does.
///
/// All five are here rather than a sequence and a range, because the two
/// that are not indexable are exactly the two an index walk was wrong
/// about: a `Map` answers neither `length()` nor `get(i)`, and a `Set`
/// answers `length()` but not `get(i)`. `iter-items` asks the oracle's
/// own iteration what the loop walks, so what the VM walks is what the
/// interpreter walks by construction, and these assert it.
#[test]
fn a_for_walks_every_collection_the_way_the_interpreter_walks_it() {
    // A range, which builds no value: the loop counts between its bounds.
    assert_eq!(
        agree_main(
            "Int",
            "  var total = 0\n  for i in 0..<5 {\n    total += i\n  }\n  total"
        )
        .value(),
        "Int(10)"
    );
    assert_eq!(
        agree_main(
            "Int",
            "  var total = 0\n  for i in 0..5 {\n    total += i\n  }\n  total"
        )
        .value(),
        "Int(15)"
    );
    // An `Array` and a `Vector`, the two sequences.
    assert_eq!(
        agree_main(
            "Int",
            "  var total = 0\n  for n in [3, 4, 5] {\n    total += n\n  }\n  total"
        )
        .value(),
        "Int(12)"
    );
    assert_eq!(
        agree_main(
            "Int",
            "  var total = 0\n  for n in Vector.of(3, 4, 5) {\n    total += n\n  }\n  total"
        )
        .value(),
        "Int(12)"
    );
    // A `Set`, in ascending element order rather than in the order it was
    // written, which is why the elements are joined rather than added.
    assert_eq!(
        agree_main(
            "String",
            "  var joined = \"\"\n  for n in Set.of(3, 1, 2) {\n    joined = \"{joined}{n}\"\n  }\n  joined"
        )
        .value(),
        "Str(\"123\")"
    );
    // A `Map`, as the `MapEntry` of each pair in ascending key order. The
    // binding's `key` and `value` are read in the body, because that
    // shape is what the interpreter binds and a loop that bound anything
    // else would still count two iterations.
    assert_eq!(
        agree_main(
            "String",
            "  var pairs = \"\"\n  let ages = Map.of(MapEntry(key: \"b\", value: 2), MapEntry(key: \"a\", value: 1))\n  for entry in ages {\n    pairs = \"{pairs}{entry.key}={entry.value};\"\n  }\n  pairs"
        )
        .value(),
        "Str(\"a=1;b=2;\")"
    );
}

/// A `Range` used as a value, everywhere a value can be used.
///
/// The oracle is `Interpreter::eval`'s `ExprKind::Range` arm, which
/// evaluates a range like any other expression, and `agree_main` is what
/// asserts against it. `inclusive_end` is what these are really about:
/// it survives into `Display`, into `Value::eq_value`, and into
/// `MapKey::Range`, so `0..<3` and `0..2` yield the same integers and
/// are three different times not the same value.
#[test]
fn a_range_is_a_value_the_way_the_interpreter_makes_one() {
    // Rendered with the operator it was written with.
    assert_eq!(expression("String", "\"{0..<3}\""), "Str(\"0..<3\")");
    assert_eq!(expression("String", "\"{0..3}\""), "Str(\"0..3\")");
    assert_eq!(expression("String", "\"{-2..<-1}\""), "Str(\"-2..<-1\")");
    // Its own methods, which are `cove_schema::builtins::RANGE`'s and
    // reach the same `builtins::call_method` the interpreter reaches.
    assert_eq!(expression("Int", "(0..<3).length()"), "Int(3)");
    assert_eq!(expression("Int", "(0..3).length()"), "Int(4)");
    assert_eq!(expression("Bool", "(3..<3).isEmpty()"), "Bool(true)");
    assert_eq!(expression("Bool", "(0..<3).contains(3)"), "Bool(false)");
    assert_eq!(expression("Bool", "(0..3).contains(3)"), "Bool(true)");
    // Compared by the bounds it was written with, end included.
    assert_eq!(expression("Bool", "0..<3 == 0..<3"), "Bool(true)");
    assert_eq!(expression("Bool", "0..<3 == 0..3"), "Bool(false)");
    assert_eq!(expression("Bool", "0..<3 == 0..<2"), "Bool(false)");
    // Bound, passed, and iterated as the value it is rather than as a
    // header the loop took apart.
    assert_eq!(
        agree_main(
            "Int",
            "  let span = 0..<4\n  var total = 0\n  for i in span {\n    total += i\n  }\n  total"
        )
        .value(),
        "Int(6)"
    );
    // And used as a `Map` key, which is where `MapKey::Range` orders by
    // the same flag.
    assert_eq!(
        expression(
            "Int",
            "Map.of(MapEntry(key: 0..<3, value: 1), MapEntry(key: 0..3, value: 2)).length()"
        ),
        "Int(2)"
    );
}

/// The instruction a range value lowers to, which no outcome can show.
///
/// The bounds travel on the scalar stack because the checker settled
/// both as `Int`, so the listing holds no `const` and no boundary
/// instruction on the way in — see `cove_ir::Inst::MakeRange`.
#[test]
fn a_range_value_takes_its_bounds_off_the_scalar_stack() {
    let listed = main_of("export fn main() -> Range {\n  let n = 3\n  0..<n\n}\n");
    assert!(listed.contains("make-range ..<"), "{listed}");
    assert!(!listed.contains("value-to-scalar"), "{listed}");
    assert!(listed.contains("scalar-const 0"), "{listed}");
    assert!(listed.contains("load-scalar 0"), "{listed}");
}

/// An empty collection is walked zero times, whatever it is empty of.
///
/// Zero is the length the loop's first test reads, so the body never
/// runs and nothing is bound — and that has to hold for the collections
/// whose emptiness `iter-items` reports as an empty `Array` rather than
/// as a zero `length()`.
#[test]
fn an_empty_collection_is_walked_zero_times() {
    let cases: &[&str] = &["[]", "Vector.of()", "Set.of()", "Map.of()", "0..<0"];
    for iterable in cases {
        assert_eq!(
            agree_main(
                "Int",
                &format!(
                    "  var seen = 0\n  for item in {iterable} {{\n    seen += 1\n  }}\n  seen"
                )
            )
            .value(),
            "Int(0)",
            "for `{iterable}`"
        );
    }
}

#[test]
fn break_and_continue_leave_and_skip() {
    assert_eq!(
        agree_main(
            "Int",
            "  var total = 0\n  for i in 0..<10 {\n    if i == 5 {\n      break\n    }\n    total += i\n  }\n  total"
        )
        .value(),
        "Int(10)"
    );
    assert_eq!(
        agree_main(
            "Int",
            "  var total = 0\n  for i in 0..<10 {\n    if i % 2 == 0 {\n      continue\n    }\n    total += i\n  }\n  total"
        )
        .value(),
        "Int(25)"
    );
    assert_eq!(
        agree_main(
            "Int",
            "  var total = 0\n  var i = 0\n  while true {\n    i += 1\n    if i > 3 {\n      break\n    }\n    total += i\n  }\n  total"
        )
        .value(),
        "Int(6)"
    );
}

// --------------------------------- lowered for value, lowered for effect

/// An `if`/`else` used as an expression answers the branch that ran.
///
/// `cove_ir::lower` lowers an expression whose value nobody reads for its
/// effect, and reaches inside a block, an `if`/`else`, and a `match` to do
/// it. What those constructs *mean* is not allowed to change, so the
/// oracle is asked: the same `if` is read into a `let`, nested as a
/// block's tail, and written as the right-hand side of an assignment, and
/// both backends have to agree about every one of them.
#[test]
fn an_if_else_used_as_an_expression_answers_what_the_interpreter_answers() {
    let source = "export fn main() -> Result<Unit, Error> {\n  let a = if 1 < 2 {\n    10\n  } else {\n    20\n  }\n  let b = {\n    if a == 10 {\n      a + 1\n    } else {\n      a - 1\n    }\n  }\n  var c = 0\n  if b == 11 {\n    c = if a == 10 {\n      5\n    } else {\n      6\n    }\n  }\n  let d = if a == 10 {\n    let ignored = 1\n  } else {\n    let ignored = 2\n  }\n  assertEqual(a, 10)?\n  assertEqual(b, 11)?\n  assertEqual(c, 5)?\n  assertEqual(d, ())?\n  Ok(())\n}\n";
    assert_eq!(
        agree(source).value(),
        "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Unit] })"
    );
}

/// A `match` used as an expression answers the arm that ran, and one
/// written as a statement still runs it.
#[test]
fn a_match_used_as_an_expression_answers_what_the_interpreter_answers() {
    let source = "enum Shape {\n  Dot\n  Line(Int)\n}\n\nexport fn main() -> Result<Unit, Error> {\n  let n = match Shape.Line(3) {\n    Shape.Dot => 0\n    Shape.Line(k) => k * 2\n  }\n  let m = {\n    match Shape.Dot {\n      Shape.Dot => n + 1\n      Shape.Line(k) => k\n    }\n  }\n  var seen = 0\n  match Shape.Line(5) {\n    Shape.Dot => seen = 1\n    Shape.Line(k) => seen = k\n  }\n  assertEqual(n, 6)?\n  assertEqual(m, 7)?\n  assertEqual(seen, 5)?\n  Ok(())\n}\n";
    assert_eq!(
        agree(source).value(),
        "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Unit] })"
    );
}

/// A block used as an expression answers its tail, and a block with no
/// tail answers `()`.
#[test]
fn a_block_used_as_an_expression_answers_what_the_interpreter_answers() {
    let source = "export fn main() -> Result<Unit, Error> {\n  let a = {\n    let x = 1\n    let y = 2\n    x + y\n  }\n  let b = {\n    let z = a\n  }\n  var t = 0\n  {\n    t = a * 2\n  }\n  assertEqual(a, 3)?\n  assertEqual(b, ())?\n  assertEqual(t, 6)?\n  Ok(())\n}\n";
    assert_eq!(
        agree(source).value(),
        "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Unit] })"
    );
}

/// A statement lowered for its effect still does everything it did.
///
/// Lowering for effect removes a value and never an operation, so the
/// loops still count, the assignments still write, and the `if` with no
/// `else` still takes the branch it was going to take. The oracle is what
/// says so.
#[test]
fn a_statement_lowered_for_its_effect_still_runs_everything_in_it() {
    let source = "export fn main() -> Result<Unit, Error> {\n  var total = 0\n  var i = 0\n  while i < 10 {\n    if i % 3 == 0 {\n      total += i\n    }\n    i += 1\n  }\n  for j in 0..<4 {\n    total += j\n  }\n  assertEqual(total, 24)?\n  Ok(())\n}\n";
    assert_eq!(
        agree(source).value(),
        "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Unit] })"
    );
}

#[test]
fn an_early_return_ends_the_function_where_it_is_written() {
    let source = "fn first(items: Array<Int>) -> Int {\n  for n in items {\n    if n > 2 {\n      return n\n    }\n  }\n  0\n}\n\nexport fn main() -> Int {\n  first([1, 2, 3, 4])\n}\n";
    assert_eq!(agree(source).value(), "Int(3)");
}
