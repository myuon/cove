//! What an operator instruction answers, and what it answers when it fails.
//!
//! Both halves of the operator story are here: the untyped `Binary`, which
//! decides from the values it was handed, and the instructions the checker's
//! types let the lowering specialise to — `Int`, `Float`, `FieldAt`,
//! `CallMethodAt`. A specialised instruction is only right if it answers what
//! the general one answered, so both are asserted against the same oracle
//! rather than against each other.

use super::*;

/// Every operator the IR carries, on every type the language defines it
/// for.
///
/// One test rather than one per operator, because what is being checked
/// is the mapping from the IR's operator to the interpreter's: a
/// `Sub` that reached `binary` as `Add` still answers a number, so only
/// running every one of them against the oracle catches it.
#[test]
fn every_operator_answers_what_the_interpreter_answers() {
    let cases: &[(&str, &str, &str)] = &[
        ("Int", "7 + 5", "Int(12)"),
        ("Int", "7 - 5", "Int(2)"),
        ("Int", "7 * 5", "Int(35)"),
        ("Int", "7 / 5", "Int(1)"),
        ("Int", "7 % 5", "Int(2)"),
        ("Int", "-7", "Int(-7)"),
        ("Float", "7.5 + 0.25", "Float(7.75)"),
        ("Float", "7.5 - 0.25", "Float(7.25)"),
        ("Float", "7.5 * 2.0", "Float(15.0)"),
        ("Float", "7.5 / 2.0", "Float(3.75)"),
        ("Float", "7.5 % 2.0", "Float(1.5)"),
        ("Float", "-7.5", "Float(-7.5)"),
        ("Duration", "1ms + 500us", "Duration(1500000)"),
        ("Duration", "1ms - 500us", "Duration(500000)"),
        ("Duration", "-1ms", "Duration(-1000000)"),
        ("Bool", "7 == 7", "Bool(true)"),
        ("Bool", "7 != 7", "Bool(false)"),
        ("Bool", "7 < 5", "Bool(false)"),
        ("Bool", "7 <= 7", "Bool(true)"),
        ("Bool", "7 > 5", "Bool(true)"),
        ("Bool", "7 >= 8", "Bool(false)"),
        ("Bool", "\"a\" < \"b\"", "Bool(true)"),
        ("Bool", "\"a\" == \"a\"", "Bool(true)"),
        ("Bool", "1ms > 999us", "Bool(true)"),
        ("Bool", "0.5 <= 0.5", "Bool(true)"),
        ("Bool", "!true", "Bool(false)"),
        ("Bool", "true && false", "Bool(false)"),
        ("Bool", "true || false", "Bool(true)"),
        ("Bool", "[1, 2] == [1, 2]", "Bool(true)"),
    ];
    for (ty, expr, expected) in cases {
        assert_eq!(&expression(ty, expr), expected, "for `{expr}`");
    }
}

/// The failures arithmetic can have, in the words the interpreter reports
/// them in.
///
/// A mixed-type comparison is not here because no checked program has
/// one: `cove-sema` refuses `1 == "a"` before either backend sees it, and
/// so refuses every other operator applied across two types. What is left
/// that a checked program can still do is overflow and divide by zero,
/// and both are reported by the one `binary` both backends call.
#[test]
fn arithmetic_fails_the_way_the_interpreter_fails() {
    let most_negative = "  let least = -9223372036854775807 - 1\n";
    let cases: &[(&str, &str)] = &[
        (
            "  let big = 9223372036854775807\n  big + 1",
            "`Int` addition overflowed",
        ),
        (
            "  let least = -9223372036854775807 - 1\n  least - 1",
            "`Int` subtraction overflowed",
        ),
        (
            "  let big = 9223372036854775807\n  big * 2",
            "`Int` multiplication overflowed",
        ),
        ("  let zero = 0\n  1 / zero", "`Int` division by zero"),
        ("  let zero = 0\n  1 % zero", "`Int` remainder by zero"),
    ];
    for (body, message) in cases {
        assert_eq!(
            &agree_main("Int", body).error().message,
            message,
            "for:\n{body}"
        );
    }
    assert_eq!(
        agree_main("Int", &format!("{most_negative}  -least"))
            .error()
            .message,
        "`Int` negation overflowed"
    );
    assert_eq!(
        agree_main("Int", &format!("{most_negative}  least / -1"))
            .error()
            .message,
        "`Int` division overflowed"
    );
}

// ---------------------------------------- the instructions with a type

/// Every operator the checker settles as `Int`, answered by the typed
/// instruction and by the interpreter, message for message.
///
/// The point of specialising is that nothing about the program changed,
/// so the assertion is the same one every other test here makes: the
/// oracle's answer. What is different is only which instruction produced
/// it, and `an_int_operator_lowers_to_the_typed_instruction` is what
/// pins that, because a specialisation that silently stopped happening
/// would pass this test forever.
#[test]
fn every_int_operator_answers_what_the_interpreter_answers() {
    let cases: &[(&str, &str, &str)] = &[
        ("Int", "a + b", "Int(12)"),
        ("Int", "a - b", "Int(2)"),
        ("Int", "a * b", "Int(35)"),
        ("Int", "a / b", "Int(1)"),
        ("Int", "a % b", "Int(2)"),
        ("Bool", "a == b", "Bool(false)"),
        ("Bool", "a != b", "Bool(true)"),
        ("Bool", "a < b", "Bool(false)"),
        ("Bool", "a <= b", "Bool(false)"),
        ("Bool", "a > b", "Bool(true)"),
        ("Bool", "a >= b", "Bool(true)"),
    ];
    for (ty, expr, expected) in cases {
        let body = format!("  let a = 7\n  let b = 5\n  {expr}");
        assert_eq!(
            &agree_main(ty, &body).value().to_string(),
            expected,
            "for `{expr}`"
        );
        assert!(
            main_of(&format!("export fn main() -> {ty} {{\n{body}\n}}\n"))
                .lines()
                .any(|line| line.contains("  int ")),
            "`{expr}` lowers to the typed operator"
        );
    }
}

/// The failures `Int` has, raised by the typed instruction in the words
/// the interpreter raises them in.
///
/// Overflow at each of the three limits, and division and remainder by
/// zero. `arithmetic_fails_the_way_the_interpreter_fails` asserts the
/// same messages; this asserts them of the instruction that carries the
/// type, which is a different instruction reaching the same helpers, and
/// checks that it is the one that ran.
#[test]
fn the_typed_operator_fails_the_way_the_interpreter_fails() {
    let cases: &[(&str, &str)] = &[
        (
            "  let big = 9223372036854775807\n  let one = 1\n  big + one",
            "`Int` addition overflowed",
        ),
        (
            "  let least = -9223372036854775807 - 1\n  let one = 1\n  least - one",
            "`Int` subtraction overflowed",
        ),
        (
            "  let big = 9223372036854775807\n  let two = 2\n  big * two",
            "`Int` multiplication overflowed",
        ),
        (
            "  let least = -9223372036854775807 - 1\n  let minus = -1\n  least / minus",
            "`Int` division overflowed",
        ),
        (
            "  let one = 1\n  let zero = 0\n  one / zero",
            "`Int` division by zero",
        ),
        (
            "  let one = 1\n  let zero = 0\n  one % zero",
            "`Int` remainder by zero",
        ),
    ];
    for (body, message) in cases {
        assert_eq!(
            &agree_main("Int", body).error().message,
            message,
            "for:\n{body}"
        );
        let listing = main_of(&format!("export fn main() -> Int {{\n{body}\n}}\n"));
        assert!(
            listing.lines().any(|line| line.contains("  int ")),
            "the failure came from the typed operator:\n{listing}"
        );
    }
}

/// A `Float` operator is not an `Int` operator, so it keeps the untyped
/// instruction and keeps agreeing.
///
/// This is the other half of the rule, and the half a mistake would show
/// in: specialising on a type the checker did not settle is how a backend
/// starts answering a different program.
#[test]
fn float_arithmetic_keeps_the_untyped_operator_and_still_agrees() {
    let cases: &[(&str, &str, &str)] = &[
        ("Float", "a + b", "Float(7.75)"),
        ("Float", "a - b", "Float(7.25)"),
        ("Float", "a * b", "Float(1.875)"),
        ("Float", "a / b", "Float(30.0)"),
        ("Bool", "a > b", "Bool(true)"),
    ];
    for (ty, expr, expected) in cases {
        let body = format!("  let a = 7.5\n  let b = 0.25\n  {expr}");
        assert_eq!(
            &agree_main(ty, &body).value().to_string(),
            expected,
            "for `{expr}`"
        );
        let listing = main_of(&format!("export fn main() -> {ty} {{\n{body}\n}}\n"));
        assert!(
            listing.lines().any(|line| line.contains("  binary ")),
            "`{expr}` keeps the untyped operator:\n{listing}"
        );
        assert!(
            !listing.lines().any(|line| line.contains("  int ")),
            "`{expr}` is not integer arithmetic:\n{listing}"
        );
    }
}

/// A `Duration` is neither operand of an `Int` operator, and mixing one
/// with an `Int` is the arithmetic the checker allows across two types.
#[test]
fn duration_arithmetic_keeps_the_untyped_operator_and_still_agrees() {
    let body = "  let a = 1ms\n  let b = 500us\n  a - b";
    assert_eq!(
        agree_main("Duration", body).value().to_string(),
        "Duration(500000)"
    );
    let listing = main_of(&format!("export fn main() -> Duration {{\n{body}\n}}\n"));
    assert!(
        !listing.lines().any(|line| line.contains("  int ")),
        "a `Duration` is not an `Int`:\n{listing}"
    );
}

/// A field read by position answers what the same read by name answers.
///
/// Both programs are written, because the property is that the two are
/// one program: the position is where the name stands, and a struct's
/// fields stand in declaration order wherever one is built.
#[test]
fn a_field_read_by_position_answers_what_a_read_by_name_answers() {
    let source = "struct Point {\n  x: Int\n  y: Int\n}\n\n\
         export fn main() -> Int {\n\
         \x20 var p = Point(x: 3, y: 4)\n\
         \x20 p.y = p.y + p.x\n\
         \x20 p.x + p.y\n\
         }\n";
    assert_eq!(agree(source).value().to_string(), "Int(10)");
    let listing = main_of(source);
    // Both fields are `Int`, so a read of either is fused straight to
    // the scalar stack rather than stopping at `get-field-at`.
    assert!(
        listing
            .lines()
            .any(|line| line.contains("get-field-at-scalar 0"))
            && listing
                .lines()
                .any(|line| line.contains("get-field-at-scalar 1")),
        "both fields are read by position:\n{listing}"
    );
    assert!(
        !listing.lines().any(|line| line.contains("  get-field ")),
        "nothing is left reading by name:\n{listing}"
    );
}

/// A method a builtin type also names now lowers, because the checker
/// recorded which of the two the call reaches.
///
/// `Array` has a `length` and so does this `Box`, and both are called in
/// one program. Until the lowering could read the checker's answer this
/// refused to lower at all, so the assertion that matters is that there
/// is an answer to compare.
#[test]
fn a_declared_method_a_builtin_also_names_lowers_and_agrees() {
    let source = "struct Box {\n  items: Array<Int>\n}\n\n\
         impl Box {\n\
         \x20 /// Doc.\n\
         \x20 fn length(self) -> Int {\n\
         \x20   99\n\
         \x20 }\n\
         }\n\n\
         export fn main() -> Int {\n\
         \x20 let b = Box(items: [1, 2, 3])\n\
         \x20 b.length() + [1, 2, 3].length()\n\
         }\n";
    // 99 from the declaration and 3 from the builtin: a call reaching the
    // wrong one of the two would answer 6 or 198 rather than fail.
    assert_eq!(agree(source).value().to_string(), "Int(102)");
    let listing = main_of(source);
    assert!(
        listing
            .lines()
            .any(|line| line.contains("call m.Box.length")),
        "the declared method is called:\n{listing}"
    );
    assert!(
        listing
            .lines()
            .any(|line| line.contains("call-builtin length")),
        "the builtin is called:\n{listing}"
    );
}

/// A failure carries the span of the instruction that raised it, so a
/// diagnostic points at the same source it points at today.
#[test]
fn a_failure_points_at_the_operator_that_raised_it() {
    let source = "export fn main() -> Int {\n  let zero = 0\n  1 / zero\n}\n";
    let error = agree(source).error().clone();
    let span = error.span.expect("a runtime error points at source");
    assert_eq!(&source[span.start as usize..span.end as usize], "1 / zero");
}
