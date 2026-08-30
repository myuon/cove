//! Declared enums, the cases they build, and `match`.

use super::*;

const STATUS: &str = "enum Status {\n  Confirmed\n  Pending(Int)\n}\n\n";

/// Every case of a declared enum, built and rendered.
#[test]
fn a_declared_enum_case_is_built_the_way_the_interpreter_builds_it() {
    assert_eq!(
        agree(&format!(
            "{STATUS}export fn main() -> String {{\n  \"{{Status.Confirmed}} {{Status.Pending(3)}}\"\n}}\n"
        ))
        .value(),
        "Str(\"Confirmed Pending(3)\")"
    );
}

/// A case carries the qualified name of the enum that declares it, which
/// is what keeps two modules' `Status` two types.
#[test]
fn a_case_carries_the_qualified_name_of_its_enum() {
    assert_eq!(
        agree(&format!(
            "{STATUS}export fn main() -> Status {{\n  Status.Pending(1)\n}}\n"
        ))
        .value(),
        "Enum(EnumValue { type_name: \"m.Status\", case: \"Pending\", payload: [Int(1)] })"
    );
}

/// An associated function declared in an `impl` block is a call, and a
/// case of the same enum is not — the order `Interpreter::eval_call`
/// asks in, reproduced.
#[test]
fn an_associated_function_of_an_enum_is_called_and_a_case_is_built() {
    let source = format!(
        "{STATUS}impl Status {{\n  fn start() -> Status {{\n    Status.Pending(0)\n  }}\n}}\n\nexport fn main() -> String {{\n  \"{{Status.start()}} {{Status.Confirmed}}\"\n}}\n"
    );
    assert_eq!(agree(&source).value(), "Str(\"Pending(0) Confirmed\")");
}

/// Every pattern form the language has, over one subject each.
#[test]
fn every_pattern_form_matches_what_the_interpreter_matches() {
    let variant = format!(
        "{STATUS}fn label(s: Status) -> String {{\n  match s {{\n    Status.Confirmed => \"yes\"\n    Status.Pending(n) => \"wait {{n}}\"\n  }}\n}}\n\nexport fn main() -> String {{\n  \"{{label(Status.Confirmed)}} {{label(Status.Pending(4))}}\"\n}}\n"
    );
    assert_eq!(agree(&variant).value(), "Str(\"yes wait 4\")");

    // A literal arm, a binder arm, and a `_` arm, in one `match` each.
    let literal = "fn name(n: Int) -> String {\n  match n {\n    1 => \"one\"\n    -2 => \"minus two\"\n    other => \"many {other}\"\n  }\n}\n\nexport fn main() -> String {\n  \"{name(1)} {name(-2)} {name(9)}\"\n}\n";
    assert_eq!(agree(literal).value(), "Str(\"one minus two many 9\")");

    let wildcard = "fn small(n: Int) -> Bool {\n  match n {\n    0 => true\n    _ => false\n  }\n}\n\nexport fn main() -> String {\n  \"{small(0)} {small(1)}\"\n}\n";
    assert_eq!(agree(wildcard).value(), "Str(\"true false\")");
}

/// `Ok(Some(x))`: a pattern two levels deep, matching and failing at each
/// of them.
#[test]
fn a_pattern_nested_two_deep_matches_and_fails_at_each_level() {
    let source = "fn opened(r: Result<Option<Int>, Error>) -> Int {\n  match r {\n    Ok(Some(x)) => x\n    Err(e) => -1\n    _ => 0\n  }\n}\n\nexport fn main() -> String {\n  let there: Result<Option<Int>, Error> = Ok(Some(7))\n  let nothing: Result<Option<Int>, Error> = Ok(None)\n  let bad: Result<Option<Int>, Error> = Err(Error(message: \"no\"))\n  \"{opened(there)} {opened(nothing)} {opened(bad)}\"\n}\n";
    assert_eq!(agree(source).value(), "Str(\"7 0 -1\")");
}

/// `None` written as a pattern is a case of `Option` and not a name, so
/// it matches the case and nothing else.
#[test]
fn none_written_as_a_pattern_is_a_case_and_not_a_name() {
    let source = "fn told(o: Option<Int>) -> Int {\n  match o {\n    None => -1\n    Some(n) => n\n  }\n}\n\nexport fn main() -> String {\n  \"{told(Some(5))} {told(None)}\"\n}\n";
    assert_eq!(agree(source).value(), "Str(\"5 -1\")");
}

/// The first arm that matches is the only one that runs, even where a
/// later one would have matched too.
#[test]
fn an_earlier_arm_wins_over_a_later_one_that_would_also_match() {
    let source = "fn which(n: Int) -> String {\n  match n {\n    1 => \"first\"\n    other => \"binder\"\n  }\n}\n\nexport fn main() -> String {\n  \"{which(1)} {which(2)}\"\n}\n";
    assert_eq!(agree(source).value(), "Str(\"first binder\")");
}

/// An arm's binder is released when the arm ends, so a name declared
/// outside the `match` is what a later reference reaches.
#[test]
fn a_binder_is_out_of_scope_after_its_arm() {
    let source = "export fn main() -> String {\n  let n = 1\n  let seen = match Some(9) {\n    Some(n) => n\n    None => 0\n  }\n  \"{seen} {n}\"\n}\n";
    assert_eq!(agree(source).value(), "Str(\"9 1\")");
}

/// A `match` on the result of a `match`, so that one nests inside
/// another's arm and inside another's subject.
#[test]
fn a_match_nests_in_another_matchs_arm_and_subject() {
    let source = "fn inner(n: Int) -> Option<Int> {\n  match n {\n    0 => None\n    other => Some(other * 2)\n  }\n}\n\nexport fn main() -> String {\n  let outer = match inner(3) {\n    Some(v) => match v {\n      6 => \"six\"\n      _ => \"other\"\n    }\n    None => \"none\"\n  }\n  let nested = match match inner(0) {\n    Some(v) => v\n    None => -1\n  } {\n    -1 => \"empty\"\n    _ => \"full\"\n  }\n  \"{outer} {nested}\"\n}\n";
    assert_eq!(agree(source).value(), "Str(\"six empty\")");
}

/// A subject no arm covers stops the run, in the interpreter's words.
///
/// Exhaustiveness is checked case by case rather than pattern by
/// pattern, so `Some(1)` covers `Some` as far as `cove-sema` is
/// concerned and a `Some(2)` reaches no arm at run time. That is what
/// makes `no-match` a thing a checked program can still arrive at.
#[test]
fn a_match_that_covers_nothing_stops_both_backends_the_same_way() {
    let source = "export fn main() -> String {\n  let o: Option<Int> = Some(2)\n  match o {\n    Some(1) => \"one\"\n    None => \"none\"\n  }\n}\n";
    let outcome = agree(source);
    assert_eq!(outcome.error().message, "no `match` arm covers `Some(2)`");
    assert_eq!(
        outcome.error().help.as_deref(),
        Some("add an arm for this case, or a `_` arm")
    );
}
