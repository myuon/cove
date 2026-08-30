//! The builtin methods and the builtin associated functions, the constructors
//! for `Result` and `Option`, interpolation and array literals, and
//! assertions.
//!
//! What these have in common is that the VM implements none of them:
//! `CallBuiltin` and its neighbours call the functions the interpreter calls,
//! so what is under test is that the instruction reaches them with the
//! arguments the interpreter would have passed.

use super::*;

#[test]
fn builtin_methods_answer_what_the_interpreter_answers() {
    assert_eq!(expression("Int", "[1, 2, 3].length()"), "Int(3)");
    assert_eq!(expression("Int", "[1, 2, 3].get(1).unwrapOr(0)"), "Int(2)");
    assert_eq!(expression("Int", "[1, 2, 3].get(9).unwrapOr(0)"), "Int(0)");
    assert_eq!(expression("Int", "\"hello\".chars().length()"), "Int(5)");
    assert_eq!(
        expression("String", "\"hello\".chars().get(1).unwrapOr(\"\")"),
        "Str(\"e\")"
    );
    assert_eq!(expression("Int", "\"hello\".length()"), "Int(5)");
}

/// A builtin's own failure is the interpreter's, because it is the same
/// call.
#[test]
fn a_builtin_fails_the_way_the_interpreter_fails() {
    assert_eq!(
        agree_main(
            "Int",
            "  let least = -9223372036854775807 - 1\n  least.abs()"
        )
        .error()
        .message,
        "`Int` abs overflowed"
    );
    assert_eq!(
        refused_unchecked("Int", "[1, 2, 3].get(1, 2).unwrapOr(0)"),
        "`Array.get` takes 1 argument(s), but 2 were given"
    );
}

// ----------------------------------------------- results and options

#[test]
fn the_builtin_constructors_build_what_the_interpreter_builds() {
    let source = concat!(
        "fn nothing() -> Option<Int> {\n  None\n}\n\n",
        "export fn main() -> String {\n",
        "  let good: Result<Int, Error> = Ok(1)\n",
        "  let bad: Result<Int, Error> = Err(Error(message: \"no\"))\n",
        "  let there = Some(2)\n",
        "  let boom = Error(message: \"boom\")\n",
        "  \"{good} {bad} {there} {nothing()} {boom}\"\n",
        "}\n"
    );
    assert_eq!(
        agree(source).value(),
        "Str(\"Ok(1) Err(no) Some(2) None boom\")"
    );
}

/// `?` on both of the types it is defined over, taking both paths.
#[test]
fn a_question_mark_opens_or_returns() {
    let ok = "fn answer() -> Result<Int, Error> {\n  Ok(7)\n}\n\nexport fn main() -> Result<Int, Error> {\n  let n = answer()?\n  Ok(n + 1)\n}\n";
    assert_eq!(
        agree(ok).value(),
        "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Int(8)] })"
    );

    let err = "fn answer() -> Result<Int, Error> {\n  Err(Error(message: \"no\"))\n}\n\nexport fn main() -> Result<Int, Error> {\n  let n = answer()?\n  Ok(n + 1)\n}\n";
    assert_eq!(
        agree(err).value(),
        "Enum(EnumValue { type_name: \"Result\", case: \"Err\", payload: [Struct(StructValue { type_name: \"Error\", fields: [(\"message\", Str(\"no\"))], opaque: false })] })"
    );

    let some = "fn answer() -> Option<Int> {\n  Some(7)\n}\n\nexport fn main() -> Option<Int> {\n  let n = answer()?\n  Some(n + 1)\n}\n";
    assert_eq!(
        agree(some).value(),
        "Enum(EnumValue { type_name: \"Option\", case: \"Some\", payload: [Int(8)] })"
    );

    let none = "fn answer() -> Option<Int> {\n  None\n}\n\nexport fn main() -> Option<Int> {\n  let n = answer()?\n  Some(n + 1)\n}\n";
    assert_eq!(
        agree(none).value(),
        "Enum(EnumValue { type_name: \"Option\", case: \"None\", payload: [] })"
    );
}

// ------------------------------------------------------- rendering

#[test]
fn interpolation_renders_every_part_left_to_right() {
    assert_eq!(
        expression("String", "\"a{1 + 2}b{true}c\""),
        "Str(\"a3btruec\")"
    );
    assert_eq!(
        agree(&format!(
            "{CURSOR}export fn main() -> String {{\n  \"{{Cursor(at: 1, step: 2)}}\"\n}}\n"
        ))
        .value(),
        "Str(\"Cursor(at: 1, step: 2)\")"
    );
}

#[test]
fn an_array_is_built_left_to_right() {
    assert_eq!(
        expression("String", "\"{[1 + 1, 2 + 2, 3 + 3]}\""),
        "Str(\"[2, 4, 6]\")"
    );
    assert_eq!(expression("Int", "[].length()"), "Int(0)");
}

// ------------------------------------------------------ assertions

#[test]
fn a_holding_assertion_answers_ok_on_both() {
    assert_eq!(
        agree_main(
            "Result<Unit, Error>",
            "  assert(1 < 2)?\n  assertEqual(1 + 1, 2)?\n  Ok(())"
        )
        .value(),
        "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Unit] })"
    );
}

// ----------------------------------- associated functions of builtins

#[test]
fn builtin_associated_functions_answer_what_the_interpreter_answers() {
    assert_eq!(expression("Int", "Vector.of(1, 2, 3).length()"), "Int(3)");
    assert_eq!(expression("Int", "Vector.of().length()"), "Int(0)");
    assert_eq!(expression("Int", "Set.of(3, 1, 2).length()"), "Int(3)");
    assert_eq!(
        expression("Int", "Map.of(MapEntry(key: \"a\", value: 1)).length()"),
        "Int(1)"
    );
    assert_eq!(
        expression("String", "\"{Int.parse(\"12\")}\""),
        "Str(\"Ok(12)\")"
    );
    assert_eq!(
        expression("String", "\"{Int.parse(\"twelve\")}\""),
        "Str(\"Err(`twelve` is not an Int)\")"
    );
    assert_eq!(
        expression("String", "\"{Float.parse(\"1.5\")}\""),
        "Str(\"Ok(1.5)\")"
    );
    assert_eq!(
        expression("String", "\"{Float.parse(\"x\")}\""),
        "Str(\"Err(`x` is not a Float)\")"
    );
}

/// A name a builtin type has no associated function for fails through the
/// one dispatch both backends make.
#[test]
fn an_unknown_associated_function_fails_the_way_the_interpreter_fails() {
    assert_eq!(
        refused_unchecked(
            "Int",
            "Vector.of(1).length() + Int.parse(\"1\", \"2\").unwrapOr(0)"
        ),
        "`Int.parse` takes 1 argument(s), but 2 were given"
    );
}

/// `MapEntry` is the one builtin struct a program builds by calling its
/// name, and its two fields are read back like any other struct's.
#[test]
fn a_map_entry_is_built_and_read_like_a_struct() {
    assert_eq!(
        expression("String", "\"{MapEntry(key: \"a\", value: 1)}\""),
        "Str(\"MapEntry(key: a, value: 1)\")"
    );
    assert_eq!(
        expression("String", "MapEntry(key: \"a\", value: 1).key"),
        "Str(\"a\")"
    );
    assert_eq!(
        expression("Int", "MapEntry(key: \"a\", value: 1).value"),
        "Int(1)"
    );
}

/// A failing assertion quotes its condition identically on both backends.
///
/// `assert` and `assertEqual` are builtins because their failure names
/// the condition in the words the test was written in. The interpreter
/// reads the argument's span out of the `SourceMap`; the VM reads the
/// same span out of `cove_ir::Function::arg_spans` and the same text out
/// of the same map, so the two messages are compared byte for byte here
/// rather than merely both being failures.
#[test]
fn a_failing_assertion_quotes_its_condition_on_both_backends() {
    let (sources, checked) = checked_module(
        "export fn main() -> Result<Unit, Error> {\n  assert(1 > 2)?\n  Ok(())\n}\n",
    );
    let (interpreted, lowered) = on_both(&checked, &sources, "m", None);
    assert!(
        interpreted.value().contains("assertion failed: `1 > 2`"),
        "{}",
        interpreted.value()
    );
    assert_eq!(interpreted.value(), lowered.value());

    let (equal_sources, equal_checked) = checked_module(
        "export fn main() -> Result<Unit, Error> {\n  assertEqual(1 + 1, 3)?\n  Ok(())\n}\n",
    );
    let (interpreted, lowered) = on_both(&equal_checked, &equal_sources, "m", None);
    assert!(
        interpreted
            .value()
            .contains("assertion failed: `1 + 1` is `2`, expected `3`"),
        "{}",
        interpreted.value()
    );
    assert_eq!(interpreted.value(), lowered.value());
}
