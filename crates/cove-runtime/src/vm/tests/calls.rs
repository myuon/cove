//! Calls and the calling convention: recursion and its depth limit, variadic
//! parameters and spread arguments, defaults computed inside the callee, and
//! which declaration a method name reaches.
//!
//! Several of these assert the listing beside the answer, because an outcome
//! cannot show which frame a default was evaluated in, nor whether a method
//! call reached the builtin table or a declared `Call`.

use super::*;

/// Recursion, deep enough that the frame stack has to be a stack.
///
/// `fib(20)` is about 22,000 nested and sibling calls, which is the same
/// workload `benches/pure` measures — enough that a frame layout that
/// only worked one level deep could not answer it.
#[test]
fn recursion_answers_what_the_interpreter_answers() {
    let source = "fn fib(n: Int) -> Int {\n  if n < 2 {\n    n\n  } else {\n    fib(n - 1) + fib(n - 2)\n  }\n}\n\nexport fn main() -> Int {\n  fib(20)\n}\n";
    assert_eq!(agree(source).value(), "Int(6765)");
}

/// Recursion past the depth limit reports the limit rather than
/// exhausting anything.
#[test]
fn unbounded_recursion_reports_the_depth_limit() {
    let source =
        "fn down(n: Int) -> Int {\n  down(n + 1)\n}\n\nexport fn main() -> Int {\n  down(0)\n}\n";
    assert_eq!(
        agree(source).error().message,
        format!("call depth limit of {MAX_CALL_DEPTH} reached while calling `down`")
    );
}

/// A variadic parameter, answered against the interpreter for every
/// count of arguments a call can pass it.
///
/// The oracle is `Interpreter::bind_params`, whose variadic arm builds
/// `Value::Array` out of whatever `assign_labels` left in the
/// parameter's slot and in `rest`, and declares it immutable. So a
/// variadic parameter given nothing is an empty `Array` rather than a
/// missing argument, and one given a label is the one element that label
/// named.
#[test]
fn a_variadic_parameter_is_the_array_the_interpreter_binds() {
    let join = "fn join(sep: String, items: String...) -> String {\n  var text = \"\"\n  for item in items {\n    text = \"{text}{sep}{item}\"\n  }\n  \"{items.length()}{text}\"\n}\n\n";
    let cases: &[(&str, &str)] = &[
        ("join(\"-\", \"a\", \"b\", \"c\")", "Str(\"3-a-b-c\")"),
        ("join(\"-\", \"a\")", "Str(\"1-a\")"),
        ("join(\"-\")", "Str(\"0\")"),
        ("join(\"-\", items: \"a\")", "Str(\"1-a\")"),
        ("join(sep: \"-\", items: \"a\")", "Str(\"1-a\")"),
    ];
    for (call, expected) in cases {
        assert_eq!(
            agree(&format!(
                "{join}export fn main() -> String {{\n  {call}\n}}\n"
            ))
            .value(),
            *expected,
            "for `{call}`"
        );
    }
}

/// A variadic parameter whose elements are `Int`, which is the case a
/// signature read carelessly would get wrong.
///
/// `record_signature` stores a variadic parameter as the element type it
/// was written as, so `items: Int...` answers `Int` there while the body
/// sees `Array<Int>`. The listing is asserted beside the answer because
/// nothing about the answer would show which stack the slot was numbered
/// in — the callee would load a word where an array was pushed, and the
/// whole frame would be wrong from there.
#[test]
fn a_variadic_parameter_of_ints_arrives_as_an_array() {
    let source = "fn total(items: Int...) -> Int {\n  var sum = 0\n  for item in items {\n    sum += item\n  }\n  sum\n}\n\nexport fn main() -> Int {\n  total(1, 2, 3) + total()\n}\n";
    assert_eq!(agree(source).value(), "Int(6)");
    let listed = main_of(source);
    assert!(listed.contains("make-array 3"), "{listed}");
    assert!(listed.contains("make-array 0"), "{listed}");
    // One argument for one parameter, on the value stack, both times.
    assert_eq!(listed.matches("argc=1/0").count(), 2, "{listed}");
}

#[test]
fn a_method_name_a_builtin_and_a_declared_type_share_reaches_the_builtin() {
    let source = "struct Box {\n  n: Int\n}\n\nimpl Box {\n  fn length(self) -> Int {\n    self.n\n  }\n}\n\nexport fn main() -> Int {\n  [1, 2, 3].length()\n}\n";
    assert_eq!(agree(source).value(), "Int(3)");
    let listing = main_of(source);
    assert!(
        listing
            .lines()
            .any(|line| line.contains("call-builtin length")),
        "the array's `length` is the builtin's:\n{listing}"
    );
}

/// A declared method whose name no builtin has still lowers to a `Call`.
///
/// The refusal above is about the collision and not about declared
/// methods, and `benches/method` depends on this staying true.
#[test]
fn a_declared_method_no_builtin_shares_still_lowers() {
    assert_eq!(
        agree(
            "struct Box {\n  n: Int\n}\n\nimpl Box {\n  fn held(self) -> Int {\n    self.n\n  }\n}\n\nexport fn main() -> Int {\n  Box(n: 3).held()\n}\n"
        )
        .value(),
        "Int(3)"
    );
}

/// A `...` argument spreads an `Array` or a `Vector`, and both backends
/// see the same elements in the same order.
///
/// The four shapes `tests/e2e:fn_variadic` writes: no spread at all, one
/// on its own, one mixed with an ordinary argument, and one over a
/// `Vector` — whose elements are read as the vector holds them now,
/// which is the borrow `bind_params` takes at the same moment.
#[test]
fn a_spread_argument_passes_a_sequence_where_the_elements_would_go() {
    assert_eq!(
        agree(
            "fn join(sep: String, items: String...) -> String {\n  var text = \"\"\n  for item in items {\n    text = \"{text}{sep}{item}\"\n  }\n  text\n}\n\nexport fn main() -> String {\n  let ready = [\"x\", \"y\"]\n  \"{join(\"-\", \"a\")}|{join(\"-\", ...ready)}|{join(\"-\", \"w\", ...ready)}|{join(\"-\", ...Vector.of(\"v\"))}\"\n}\n"
        )
        .value(),
        "Str(\"-a|-x-y|-w-x-y|-v\")"
    );
}

/// A default argument is computed inside the callee's frame, and the
/// answer is the interpreter's.
///
/// The specialisation is what makes the calling convention survive a
/// call that skips a parameter: `measure(3, prefix: "d")` pushes two
/// arguments, the callee takes two, and the third parameter is a slot
/// the callee's own prologue writes. The listing is asserted beside the
/// answer because an outcome cannot show which frame the default was
/// evaluated in.
#[test]
fn a_parameter_left_to_its_default_is_computed_inside_the_callee() {
    let source = "fn measure(value: Int, unit: String = \"m\", prefix: String = \"length\") -> String {\n  \"{prefix}: {value}{unit}\"\n}\n\nexport fn main() -> String {\n  measure(3, prefix: \"d\")\n}\n";
    assert_eq!(agree(source).value(), "Str(\"d: 3m\")");
    assert_eq!(
        main_of(source),
        "fn m.main arity=0 frame=0/0 -> String\n\
         \x20  0  scalar-const 3\n\
         \x20  1  const Str(\"d\")\n\
         \x20  2  call m.measure argc=1/1\n\
         \x20  3  return\n"
    );
}

/// A default that reads an earlier parameter reads the argument this
/// call passed, and a recursive call that supplies it reaches a second
/// specialisation.
#[test]
fn a_default_reads_the_parameters_the_call_supplied() {
    assert_eq!(
        agree(
            "fn sumTo(n: Int, accumulated: Int = 0) -> Int {\n  if n == 0 {\n    accumulated\n  } else {\n    sumTo(n - 1, accumulated + n)\n  }\n}\n\nexport fn main() -> Int {\n  sumTo(10)\n}\n"
        )
        .value(),
        "Int(55)"
    );
}
