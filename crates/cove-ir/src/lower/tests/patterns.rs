use super::*;

// ------------------------------------------------------ enums and match

const ENUM: &str = "enum E {\n  A\n  B(Int)\n}\n\n";

/// A case carries the qualified name of the enum it belongs to, and its
/// payload is pushed before it is built.
#[test]
fn an_enum_case_is_built_from_its_payload() {
    assert_eq!(
        listing(&format!("{ENUM}fn f() -> E {{\n  E.B(1)\n}}\n"), "f"),
        "fn m.f arity=0 frame=0/0 -> value\n\
         \x20  0  const Int(1)\n\
         \x20  1  make-enum m.E.B argc=1\n\
         \x20  2  return\n"
    );
}

/// A host's enum is reached through the module that declares it, and it
/// is not the same instruction: a host's case has a schema rather than a
/// declaration behind it, and it never carries a payload.
#[test]
fn a_case_of_an_enum_a_host_declares_names_the_module_that_declares_it() {
    assert_eq!(
        listing(
            "use http.listen\n\nfn f() -> http.Method {\n  http.Method.Get\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=0/0 -> value\n\
         \x20  0  make-host-enum http.Method.Get\n\
         \x20  1  return\n"
    );
}

/// A case that carries nothing is written without a call, and lowers to
/// the same instruction over no payload.
#[test]
fn a_case_that_carries_nothing_is_built_from_nothing() {
    assert_eq!(
        listing(&format!("{ENUM}fn f() -> E {{\n  E.A\n}}\n"), "f"),
        "fn m.f arity=0 frame=0/0 -> value\n\
         \x20  0  make-enum m.E.A argc=0\n\
         \x20  1  return\n"
    );
}

/// Two arms, tried in order over one subject that stays on the stack.
#[test]
fn a_match_tries_its_arms_in_order_over_one_subject() {
    assert_eq!(
        listing(
            &format!("{ENUM}fn f(e: E) -> Int {{\n  match e {{\n    E.A => 1\n    E.B(n) => n\n  }}\n}}\n"),
            "f"
        ),
        "fn m.f arity=1 frame=1/1 params=[value] -> Int\n\
         \x20  0  load 0\n\
         \x20  1  test-case E.A\n\
         \x20  2  jump-if-false 6\n\
         \x20  3  pop\n\
         \x20  4  scalar-const 1\n\
         \x20  5  jump 17\n\
         \x20  6  test-case E.B\n\
         \x20  7  jump-if-false 16\n\
         \x20  8  get-payload m.E.B 0\n\
         \x20  9  dup\n\
         \x20 10  value-to-scalar\n\
         \x20 11  store-scalar 1\n\
         \x20 12  pop\n\
         \x20 13  pop\n\
         \x20 14  load-scalar 1\n\
         \x20 15  jump 17\n\
         \x20 16  no-match\n\
         \x20 17  return-scalar\n"
    );
}

/// `Inst::GetPayload`'s position is checked against the case it names, the
/// way `Inst::GetFieldAt`'s is checked against its struct — the two share
/// the reason: a position past what the case declares is not a fact about
/// any program the checker accepted, but a mistake in whatever wrote this
/// instruction, and `validate` is what catches it rather than a backend at
/// run time.
#[test]
fn validate_refuses_a_payload_position_outside_its_case() {
    let mut program = lower(&checked(&format!(
        "{ENUM}fn f(e: E) -> Int {{\n  match e {{\n    E.A => 1\n    E.B(n) => n\n  }}\n}}\n"
    )))
    .expect("it lowers");
    let Inst::GetPayload { of, .. } = program.functions[0].code[8] else {
        panic!("instruction 8 is not `get-payload`");
    };
    program.functions[0].code[8] = Inst::GetPayload { of, at: 5 };
    assert_eq!(
        validate(&program).expect_err("a payload position outside its case is refused"),
        "m.f: 8: reads payload 5 of `m.E.B`, which has 1"
    );
}

/// An arm's binders are released when the arm ends, so a later arm reuses
/// the slots and the frame is as big as one arm needs rather than as big
/// as all of them.
#[test]
fn sibling_arms_reuse_the_slots_the_first_released() {
    assert_eq!(
        listing(
            "enum Pair {\n  L(Int)\n  R(Int)\n}\n\nfn f(p: Pair) -> Int {\n  match p {\n    Pair.L(x) => x\n    Pair.R(y) => y\n  }\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=1/1 params=[value] -> Int\n\
         \x20  0  load 0\n\
         \x20  1  test-case Pair.L\n\
         \x20  2  jump-if-false 11\n\
         \x20  3  get-payload m.Pair.L 0\n\
         \x20  4  dup\n\
         \x20  5  value-to-scalar\n\
         \x20  6  store-scalar 1\n\
         \x20  7  pop\n\
         \x20  8  pop\n\
         \x20  9  load-scalar 1\n\
         \x20 10  jump 22\n\
         \x20 11  test-case Pair.R\n\
         \x20 12  jump-if-false 21\n\
         \x20 13  get-payload m.Pair.R 0\n\
         \x20 14  dup\n\
         \x20 15  value-to-scalar\n\
         \x20 16  store-scalar 1\n\
         \x20 17  pop\n\
         \x20 18  pop\n\
         \x20 19  load-scalar 1\n\
         \x20 20  jump 22\n\
         \x20 21  no-match\n\
         \x20 22  return-scalar\n"
    );
}

/// A pattern nested two deep tests the payload it is standing on, and
/// leaves that payload behind when it is done with it.
#[test]
fn a_nested_pattern_matches_the_payload_it_stands_on() {
    assert_eq!(
        listing(
            "fn f(r: Result<Option<Int>, Error>) -> Int {\n  match r {\n    Ok(Some(x)) => x\n    _ => 0\n  }\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=1/1 params=[value] -> Int\n\
         \x20  0  load 0\n\
         \x20  1  test-case Ok\n\
         \x20  2  jump-if-false 17\n\
         \x20  3  get-payload 0\n\
         \x20  4  test-case Some\n\
         \x20  5  jump-if-true 8\n\
         \x20  6  pop\n\
         \x20  7  jump 17\n\
         \x20  8  get-payload 0\n\
         \x20  9  dup\n\
         \x20 10  value-to-scalar\n\
         \x20 11  store-scalar 1\n\
         \x20 12  pop\n\
         \x20 13  pop\n\
         \x20 14  pop\n\
         \x20 15  load-scalar 1\n\
         \x20 16  jump 20\n\
         \x20 17  pop\n\
         \x20 18  scalar-const 0\n\
         \x20 19  jump 20\n\
         \x20 20  return-scalar\n"
    );
}

/// An associated function of a builtin type reads its arguments and
/// nothing else, because there is no receiver to stand below them.
#[test]
fn an_associated_function_reads_its_arguments_alone() {
    assert_eq!(
        listing("fn f() -> Int {\n  Vector.of(1, 2).length()\n}\n", "f"),
        "fn m.f arity=0 frame=0/0 -> Int\n\
         \x20  0  const Int(1)\n\
         \x20  1  const Int(2)\n\
         \x20  2  call-assoc Vector.of argc=2\n\
         \x20  3  call-builtin length argc=0\n\
         \x20  4  value-to-scalar\n\
         \x20  5  return-scalar\n"
    );
}

/// `MapEntry` is a builtin struct, so its two fields are pushed in
/// declaration order and built by the builtin that builds one.
#[test]
fn a_map_entry_is_built_from_its_two_fields() {
    assert_eq!(
        listing(
            "fn f() -> String {\n  MapEntry(key: \"a\", value: 1).key\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=0/0 -> String\n\
         \x20  0  const Str(\"a\")\n\
         \x20  1  const Int(1)\n\
         \x20  2  make-builtin MapEntry argc=2\n\
         \x20  3  get-field key\n\
         \x20  4  return\n"
    );
}
