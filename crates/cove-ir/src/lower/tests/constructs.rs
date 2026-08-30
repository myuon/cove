use super::*;

// ------------------------------------------------ one construct each

/// A lambda is a [`Function`] like any other, and what the environment
/// around it handed over is the list beside it.
///
/// The order of the three is the whole story. `adder` reads `by` — off
/// the scalar stack, because that is where its own parameter lives, and
/// across to the value stack, because a capture is a value — and then
/// `make-closure` pairs it with the name. `main` pushes the argument,
/// then the callee above it, and `call-value` takes both. And the
/// closure's own body reaches `by` by index rather than by slot,
/// although the slot is `arity + 0` and a `load 1` would have found it.
#[test]
fn a_lambda_is_a_function_over_the_values_it_captured() {
    let source = "fn adder(by: Int) -> fn(Int) -> Int {\n  \
                  fn(n: Int) {\n    n + by\n  }\n}\n\n\
                  fn main() -> Int {\n  let add = adder(3)\n  add(4)\n}\n";
    assert_eq!(
        listing(source, "adder"),
        "fn m.adder arity=1 frame=0/1 params=[Int] -> value\n\
         \x20  0  load-scalar 0\n\
         \x20  1  scalar-to-value Int\n\
         \x20  2  make-closure m.<closure 0> captures=1\n\
         \x20  3  return\n"
    );
    assert_eq!(
        listing(source, "main"),
        "fn m.main arity=0 frame=1/0 -> Int\n\
         \x20  0  scalar-const 3\n\
         \x20  1  call m.adder argc=0/1\n\
         \x20  2  store 0\n\
         \x20  3  const Int(4)\n\
         \x20  4  load 0\n\
         \x20  5  call-value argc=1\n\
         \x20  6  value-to-scalar\n\
         \x20  7  return-scalar\n"
    );
    assert_eq!(
        listing(source, "<closure 0>"),
        "fn m.<closure 0> arity=1 frame=2/0 params=[value] captures=[by] -> value\n\
         \x20  0  load 0\n\
         \x20  1  value-to-scalar\n\
         \x20  2  capture 0\n\
         \x20  3  value-to-scalar\n\
         \x20  4  int Add\n\
         \x20  5  scalar-to-value Int\n\
         \x20  6  return\n"
    );
}

/// A declared function used as a value is a closure over nothing, and it
/// is not the function a direct call reaches.
///
/// `twice` is called both ways here. The direct call takes its argument
/// on the scalar stack and answers there, because the checker settled
/// `Int` at both ends; the specialisation a closure is made of takes it
/// on the value stack and answers there, because `call-value` reads
/// exactly those and has no callee to have asked. The body is the same
/// body, lowered twice, with the boundary instructions in the second one
/// where the first needed none — which is what a binding the checker
/// abstained about already gets.
#[test]
fn a_function_used_as_a_value_is_a_closure_over_nothing() {
    let source = "fn twice(n: Int) -> Int {\n  n * 2\n}\n\n\
                  fn f() -> Int {\n  let g = twice\n  twice(1) + g(2)\n}\n";
    assert_eq!(
        specialisation(source, "twice", 1),
        "fn m.twice arity=1 frame=0/1 params=[Int] -> Int\n\
         \x20  0  load-scalar 0\n\
         \x20  1  scalar-const 2\n\
         \x20  2  int Mul\n\
         \x20  3  return-scalar\n"
    );
    let program = lower(&checked(source)).expect("the program lowers");
    validate(&program).expect("the lowering holds the VM's invariants");
    let boxed = program
        .functions
        .iter()
        .position(|function| {
            &*function.name == "twice" && matches!(function.returns, SlotKind::Value)
        })
        .expect("`twice` is lowered a second time, as a value");
    assert_eq!(
        crate::render(&program, FunctionId(boxed as u32)),
        "fn m.twice arity=1 frame=1/0 params=[value] -> value\n\
         \x20  0  load 0\n\
         \x20  1  value-to-scalar\n\
         \x20  2  scalar-const 2\n\
         \x20  3  int Mul\n\
         \x20  4  scalar-to-value Int\n\
         \x20  5  return\n"
    );
}

/// `f(x) { ... }` is sugar: the trailing closure is the last positional
/// argument and is lowered exactly where a written one would be.
///
/// The two listings are the same listing but for the constant, which is
/// the assertion: `Interpreter::eval_args` pushes the trailing one onto
/// the end of the evaluated arguments with no label, no `var` and no
/// spread, and that is all this reproduces.
#[test]
fn a_trailing_closure_lands_where_a_written_argument_would() {
    let apply = "fn apply(v: Int, t: fn() -> Int) -> Int {\n  v + t()\n}\n\n";
    let trailing = listing(
        &format!("{apply}fn f() -> Int {{\n  apply(5) {{ 3 }}\n}}\n"),
        "f",
    );
    let written = listing(
        &format!("{apply}fn f() -> Int {{\n  apply(5, fn() {{ 3 }})\n}}\n"),
        "f",
    );
    assert_eq!(trailing, written);
    assert_eq!(
        trailing,
        "fn m.f arity=0 frame=0/0 -> Int\n\
         \x20  0  scalar-const 5\n\
         \x20  1  make-closure m.<closure 0> captures=0\n\
         \x20  2  call m.apply argc=1/1 -> scalar\n\
         \x20  3  return-scalar\n"
    );
}

/// The oracle captures by value at creation time, and a `var` binding is
/// no exception: `place-read` is what `Env::captures` calls, and it is
/// what keeps a place from leaving the frame that built it.
#[test]
fn a_var_parameter_is_captured_as_the_value_its_place_names() {
    let source = "fn g(var total: Int) -> fn() -> Int {\n  \
                  fn() {\n    total\n  }\n}\n";
    assert_eq!(
        listing(source, "g"),
        "fn m.g arity=1 frame=0/0/1 params=[place] -> value\n\
         \x20  0  load-place 0\n\
         \x20  1  place-read\n\
         \x20  2  make-closure m.<closure 0> captures=1\n\
         \x20  3  return\n"
    );
}

/// A name the module answers is not a capture. `Env::captures` walks
/// bindings, so a declaration, a type, and a host module resolve where
/// they always did — and a lambda that mentions one holds nothing for
/// it.
#[test]
fn only_a_binding_is_captured() {
    let source = "fn twice(n: Int) -> Int {\n  n * 2\n}\n\n\
                  fn f() -> fn(Int) -> Int {\n  fn(n: Int) {\n    twice(n)\n  }\n}\n";
    assert_eq!(
        listing(source, "f"),
        "fn m.f arity=0 frame=0/0 -> value\n\
         \x20  0  make-closure m.<closure 0> captures=0\n\
         \x20  1  return\n"
    );
}

/// A lambda inside a lambda captures out of the outer one's captures as
/// well as out of its parameters, and in the order `Env::captures`
/// produces them: the captures the enclosing body was handed first, its
/// own bindings after.
#[test]
fn a_nested_lambda_captures_out_of_the_captures_it_stands_in() {
    let source = "fn f(a: Int) -> fn(Int) -> fn() -> Int {\n  \
                  fn(b: Int) {\n    fn() {\n      a + b\n    }\n  }\n}\n";
    assert_eq!(
        listing(source, "<closure 0>"),
        "fn m.<closure 0> arity=1 frame=2/0 params=[value] captures=[a] -> value\n\
         \x20  0  capture 0\n\
         \x20  1  load 0\n\
         \x20  2  make-closure m.<closure 1> captures=2\n\
         \x20  3  return\n"
    );
    assert_eq!(
        listing(source, "<closure 1>"),
        "fn m.<closure 1> arity=0 frame=2/0 captures=[a, b] -> value\n\
         \x20  0  capture 0\n\
         \x20  1  value-to-scalar\n\
         \x20  2  capture 1\n\
         \x20  3  value-to-scalar\n\
         \x20  4  int Add\n\
         \x20  5  scalar-to-value Int\n\
         \x20  6  return\n"
    );
}

/// A name declared twice is captured once, holding what the innermost
/// declaration holds — which is `Env::captures` overwriting the value it
/// recorded and keeping the position it recorded it at.
#[test]
fn a_shadowed_name_is_captured_once_and_at_its_latest_binding() {
    let source = "fn f() -> fn() -> Int {\n  \
                  let a = 1\n  let b = 2\n  let a = 3\n  \
                  fn() {\n    a + b\n  }\n}\n";
    assert_eq!(
        listing(source, "f"),
        "fn m.f arity=0 frame=0/3 -> value\n\
         \x20  0  scalar-const 1\n\
         \x20  1  store-scalar 0\n\
         \x20  2  scalar-const 2\n\
         \x20  3  store-scalar 1\n\
         \x20  4  scalar-const 3\n\
         \x20  5  store-scalar 2\n\
         \x20  6  load-scalar 2\n\
         \x20  7  scalar-to-value Int\n\
         \x20  8  load-scalar 1\n\
         \x20  9  scalar-to-value Int\n\
         \x20 10  make-closure m.<closure 0> captures=2\n\
         \x20 11  return\n"
    );
}

#[test]
fn every_literal_loads_one_constant() {
    assert_eq!(
        listing(
            "fn f() -> Int {\n  let a = 1\n  let b = 1.5\n  let c = true\n  let d = 250ms\n  let e = ()\n  let g = \"hi\"\n  a\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=4/2 -> Int\n\
         \x20  0  scalar-const 1\n\
         \x20  1  store-scalar 0\n\
         \x20  2  const Float(1.5)\n\
         \x20  3  store 0\n\
         \x20  4  scalar-const 1\n\
         \x20  5  store-scalar 1\n\
         \x20  6  const Duration(250000000)\n\
         \x20  7  store 1\n\
         \x20  8  const Unit\n\
         \x20  9  store 2\n\
         \x20 10  const Str(\"hi\")\n\
         \x20 11  store 3\n\
         \x20 12  load-scalar 0\n\
         \x20 13  return-scalar\n"
    );
}

#[test]
fn an_interpolated_string_renders_its_parts_left_to_right() {
    assert_eq!(
        listing("fn f(n: Int) -> String {\n  \"tick {n}!\"\n}\n", "f"),
        "fn m.f arity=1 frame=0/1 params=[Int] -> value\n\
         \x20  0  const Str(\"tick \")\n\
         \x20  1  load-scalar 0\n\
         \x20  2  scalar-to-value Int\n\
         \x20  3  const Str(\"!\")\n\
         \x20  4  concat 3\n\
         \x20  5  return\n"
    );
}

#[test]
fn a_string_with_nothing_interpolated_is_one_constant() {
    assert_eq!(
        listing("fn f() -> String {\n  \"tick\"\n}\n", "f"),
        "fn m.f arity=0 frame=0/0 -> value\n\
         \x20  0  const Str(\"tick\")\n\
         \x20  1  return\n"
    );
}

/// An assignment written as a statement stores, and stops there.
///
/// The store is the whole of what an assignment does, so the `()` it
/// would answer is not built and there is nothing for a `Pop` to take
/// away. `x += 3` still reads the slot, adds, and writes it back, because
/// lowering for effect removes a value and never an operation.
#[test]
fn an_assignment_written_as_a_statement_builds_no_value() {
    assert_eq!(
        listing(
            "fn f() -> Int {\n  var x = 1\n  x = 2\n  x += 3\n  x\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=0/1 -> Int\n\
         \x20  0  scalar-const 1\n\
         \x20  1  store-scalar 0\n\
         \x20  2  scalar-const 2\n\
         \x20  3  store-scalar 0\n\
         \x20  4  load-scalar 0\n\
         \x20  5  scalar-const 3\n\
         \x20  6  int Add\n\
         \x20  7  store-scalar 0\n\
         \x20  8  load-scalar 0\n\
         \x20  9  return-scalar\n"
    );
}

/// An assignment whose value is read still answers `()`.
///
/// A block's tail is its value, so this one is lowered for value and the
/// `()` an assignment means is built exactly as it was. Both halves of
/// the rule are golden, because the saving is only correct if this is
/// unchanged.
#[test]
fn an_assignment_whose_value_is_read_still_answers_unit() {
    assert_eq!(
        listing("fn f() -> Unit {\n  var x = 1\n  x = 2\n}\n", "f"),
        "fn m.f arity=0 frame=0/1 -> value\n\
         \x20  0  scalar-const 1\n\
         \x20  1  store-scalar 0\n\
         \x20  2  scalar-const 2\n\
         \x20  3  store-scalar 0\n\
         \x20  4  const Unit\n\
         \x20  5  return\n"
    );
}

#[test]
fn operands_are_evaluated_left_to_right() {
    assert_eq!(
        listing(
            "fn f(a: Int, b: Int) -> Bool {\n  a * b / a % b - a + b != a\n}\n",
            "f"
        ),
        "fn m.f arity=2 frame=0/2 params=[Int, Int] -> Bool\n\
         \x20  0  load-scalar 0\n\
         \x20  1  load-scalar 1\n\
         \x20  2  int Mul\n\
         \x20  3  load-scalar 0\n\
         \x20  4  int Div\n\
         \x20  5  load-scalar 1\n\
         \x20  6  int Rem\n\
         \x20  7  load-scalar 0\n\
         \x20  8  int Sub\n\
         \x20  9  load-scalar 1\n\
         \x20 10  int Add\n\
         \x20 11  load-scalar 0\n\
         \x20 12  int Ne\n\
         \x20 13  return-scalar\n"
    );
}

/// The operator carries a type only where the checker settled one.
///
/// Three additions in one listing: two `Int`, two `Float`, and two
/// `Duration`. Only the first is integer arithmetic, so only the first
/// is `int Add`; the other two keep the operator that looks at what it
/// was handed, because `Float` and `Duration` are not `Int` and the rule
/// is not "a number". Reading all three from one function is what makes
/// the rule visible rather than three tests that each happen to agree.
///
/// An operand the checker *abstained* about is not written here because
/// it cannot be: `Ty::Unknown` accompanies a diagnostic, and a program
/// with one does not reach the lowering. The rule that it is not `Int`
/// is stated where it is read, in `Body::is_int`.
#[test]
fn an_addition_is_typed_only_where_the_checker_settled_int() {
    assert_eq!(
        listing(
            "fn f(a: Int, b: Int, c: Float, d: Float, e: Duration, g: Duration) -> Duration {\n  let n = a + b\n  let x = c + d\n  e + g\n}\n",
            "f"
        ),
        "fn m.f arity=6 frame=5/3 params=[Int, Int, value, value, value, value] -> value\n\
         \x20  0  load-scalar 0\n\
         \x20  1  load-scalar 1\n\
         \x20  2  int Add\n\
         \x20  3  store-scalar 2\n\
         \x20  4  load 0\n\
         \x20  5  load 1\n\
         \x20  6  binary Add\n\
         \x20  7  store 4\n\
         \x20  8  load 2\n\
         \x20  9  load 3\n\
         \x20 10  binary Add\n\
         \x20 11  return\n"
    );
}

/// A field of a receiver the checker settled is read by position.
///
/// Both fields, so that the position is read as a position rather than
/// as a zero that happens to be right.
#[test]
fn a_field_of_a_settled_struct_is_read_by_position() {
    assert_eq!(
        listing(
            "struct P {\n  x: Int\n  y: Int\n}\n\nfn f(p: P) -> Int {\n  p.x + p.y\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=1/0 params=[value] -> Int\n\
         \x20  0  load 0\n\
         \x20  1  get-field-at-scalar 0\n\
         \x20  2  load 0\n\
         \x20  3  get-field-at-scalar 1\n\
         \x20  4  int Add\n\
         \x20  5  return-scalar\n"
    );
}

/// A `Bool` field is branched on directly where its receiver's position
/// and its own type are both settled: `Inst::GetFieldAtScalar` puts it on
/// the scalar stack and `Inst::JumpIfFalseScalar` reads it there, with no
/// `Value` built for a condition that is never wanted as one.
#[test]
fn a_bool_field_as_a_condition_never_builds_a_value() {
    assert_eq!(
        listing(
            "struct P {\n  ready: Bool\n}\n\nfn f(p: P) -> Int {\n  if p.ready {\n    1\n  } else {\n    0\n  }\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=1/0 params=[value] -> Int\n\
         \x20  0  load 0\n\
         \x20  1  get-field-at-scalar 0\n\
         \x20  2  jump-if-false-scalar 5\n\
         \x20  3  scalar-const 1\n\
         \x20  4  jump 6\n\
         \x20  5  scalar-const 0\n\
         \x20  6  return-scalar\n"
    );
}

/// `MapEntry` is a builtin, not a struct this package declares, so its
/// fields have a settled type but no knowable position — the same reason
/// [`Inst::GetFieldAt`] declines it. The fusion answers only where both
/// halves are settled, so an `Int` field still lowers to
/// [`Inst::GetField`] rather than guessing a position for it.
#[test]
fn a_field_of_an_unsettled_position_still_lowers_by_name() {
    assert_eq!(
        listing(
            "fn f() -> Int {\n  MapEntry(key: \"a\", value: 1).value + 1\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=0/0 -> Int\n\
         \x20  0  const Str(\"a\")\n\
         \x20  1  const Int(1)\n\
         \x20  2  make-builtin MapEntry argc=2\n\
         \x20  3  get-field value\n\
         \x20  4  value-to-scalar\n\
         \x20  5  scalar-const 1\n\
         \x20  6  int Add\n\
         \x20  7  return-scalar\n"
    );
}

/// A name a builtin type and a declared type both answer to reaches the
/// one the receiver's type names, and both are written here.
///
/// This used to refuse the whole program: a name reached two answers and
/// the lowering had no way to choose, so declaring `Box.length` anywhere
/// in a package stopped `[1, 2, 3].length()` lowering everywhere in it.
/// The checker settles the receiver's type, which is the only thing that
/// ever decided it.
#[test]
fn a_name_a_builtin_and_a_declared_type_share_reaches_what_the_receiver_names() {
    assert_eq!(
        listing(
            "struct Box {\n  n: Int\n}\n\nimpl Box {\n  fn length(self) -> Int {\n    self.n\n  }\n}\n\nfn f(b: Box) -> Int {\n  b.length() + [1, 2, 3].length()\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=1/0 params=[value] -> Int\n\
         \x20  0  load 0\n\
         \x20  1  call m.Box.length argc=1/0 -> scalar\n\
         \x20  2  const Int(1)\n\
         \x20  3  const Int(2)\n\
         \x20  4  const Int(3)\n\
         \x20  5  make-array 3\n\
         \x20  6  call-builtin length argc=0\n\
         \x20  7  value-to-scalar\n\
         \x20  8  int Add\n\
         \x20  9  return-scalar\n"
    );
}

#[test]
fn a_unary_operator_applies_to_what_was_pushed() {
    assert_eq!(
        listing(
            "fn f(b: Bool) -> Int {\n  if !b {\n    return -1\n  }\n  0\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=0/1 params=[Bool] -> Int\n\
         \x20  0  load-scalar 0\n\
         \x20  1  scalar-to-value Bool\n\
         \x20  2  unary Not\n\
         \x20  3  jump-if-false 8\n\
         \x20  4  const Int(1)\n\
         \x20  5  unary Neg\n\
         \x20  6  value-to-scalar\n\
         \x20  7  return-scalar\n\
         \x20  8  scalar-const 0\n\
         \x20  9  return-scalar\n"
    );
}

#[test]
fn and_and_or_short_circuit_through_jumps() {
    assert_eq!(
        listing("fn f(a: Bool, b: Bool) -> Bool {\n  a && b || a\n}\n", "f"),
        "fn m.f arity=2 frame=0/2 params=[Bool, Bool] -> Bool\n\
         \x20  0  load-scalar 0\n\
         \x20  1  jump-if-false-scalar 4\n\
         \x20  2  load-scalar 1\n\
         \x20  3  jump 5\n\
         \x20  4  scalar-const 0\n\
         \x20  5  jump-if-true-scalar 8\n\
         \x20  6  load-scalar 0\n\
         \x20  7  jump 9\n\
         \x20  8  scalar-const 1\n\
         \x20  9  return-scalar\n"
    );
}

/// The scalar form of `&&`/`||` is declined where neither operand is
/// already on the scalar stack: `MapEntry`'s fields are settled types but
/// not positions — [`Inst::GetFieldAt`] is for a struct this package
/// declares, and a builtin one still reads by name — so both operands
/// here cost a `ValueToScalar` to reach the scalar stack, which is
/// exactly what the value form's own single `ValueToScalar` on the
/// answer is cheaper than.
#[test]
fn and_over_two_values_still_lowers_through_jumps() {
    assert_eq!(
        listing(
            "fn f(s: MapEntry<String, Bool>, t: MapEntry<String, Bool>) -> Bool {\n  s.value && t.value\n}\n",
            "f"
        ),
        "fn m.f arity=2 frame=2/0 params=[value, value] -> Bool\n\
         \x20  0  load 0\n\
         \x20  1  get-field value\n\
         \x20  2  jump-if-false 6\n\
         \x20  3  load 1\n\
         \x20  4  get-field value\n\
         \x20  5  jump 7\n\
         \x20  6  const Bool(false)\n\
         \x20  7  value-to-scalar\n\
         \x20  8  return-scalar\n"
    );
}

/// A `&&` over scalar `Bool` parameters used as an `if` condition is
/// branched on directly, with no `Value` built for it at all.
#[test]
fn and_of_scalar_bools_as_a_condition_never_builds_a_value() {
    assert_eq!(
        listing(
            "fn f(a: Bool, b: Bool) -> Int {\n  if a && b {\n    1\n  } else {\n    0\n  }\n}\n",
            "f"
        ),
        "fn m.f arity=2 frame=0/2 params=[Bool, Bool] -> Int\n\
         \x20  0  load-scalar 0\n\
         \x20  1  jump-if-false-scalar 4\n\
         \x20  2  load-scalar 1\n\
         \x20  3  jump 5\n\
         \x20  4  scalar-const 0\n\
         \x20  5  jump-if-false-scalar 8\n\
         \x20  6  scalar-const 1\n\
         \x20  7  jump 9\n\
         \x20  8  scalar-const 0\n\
         \x20  9  return-scalar\n"
    );
}

#[test]
fn a_block_with_no_tail_is_unit() {
    assert_eq!(
        listing("fn f() -> Unit {\n  let a = 1\n}\n", "f"),
        "fn m.f arity=0 frame=0/1 -> value\n\
         \x20  0  scalar-const 1\n\
         \x20  1  store-scalar 0\n\
         \x20  2  const Unit\n\
         \x20  3  return\n"
    );
}

/// A block whose tail is an `if`/`else` keeps what both branches build.
///
/// This is the other half of the rule. The block is the function's body
/// and its value is what the function returns, so the `if` is lowered for
/// value, and so is each of its branches — both of which are blocks with
/// no tail, which is what a `const Unit` in a listing means.
#[test]
fn a_block_whose_tail_is_an_if_else_still_builds_both_values() {
    assert_eq!(
        listing(
            "fn f(n: Int) -> Unit {\n  {\n    if n < 2 {\n      let a = 1\n    } else {\n      let b = 2\n    }\n  }\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=0/2 params=[Int] -> value\n\
         \x20  0  load-scalar 0\n\
         \x20  1  scalar-const 2\n\
         \x20  2  int Lt\n\
         \x20  3  jump-if-false-scalar 8\n\
         \x20  4  scalar-const 1\n\
         \x20  5  store-scalar 1\n\
         \x20  6  const Unit\n\
         \x20  7  jump 11\n\
         \x20  8  scalar-const 2\n\
         \x20  9  store-scalar 1\n\
         \x20 10  const Unit\n\
         \x20 11  return\n"
    );
}

/// `let x = if c { 1 } else { 2 }` reads the `if`, so both branches
/// answer and the store takes whichever one ran.
#[test]
fn a_let_of_an_if_else_stores_the_branch_that_ran() {
    assert_eq!(
        listing(
            "fn f(n: Int) -> Int {\n  let x = if n < 2 {\n    1\n  } else {\n    2\n  }\n  x\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=0/2 params=[Int] -> Int\n\
         \x20  0  load-scalar 0\n\
         \x20  1  scalar-const 2\n\
         \x20  2  int Lt\n\
         \x20  3  jump-if-false-scalar 6\n\
         \x20  4  scalar-const 1\n\
         \x20  5  jump 7\n\
         \x20  6  scalar-const 2\n\
         \x20  7  store-scalar 1\n\
         \x20  8  load-scalar 1\n\
         \x20  9  return-scalar\n"
    );
}

#[test]
fn an_if_with_an_else_joins_both_branches() {
    assert_eq!(
        listing(
            "fn f(n: Int) -> Int {\n  if n < 2 {\n    n\n  } else {\n    n - 1\n  }\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=0/1 params=[Int] -> Int\n\
         \x20  0  load-scalar 0\n\
         \x20  1  scalar-const 2\n\
         \x20  2  int Lt\n\
         \x20  3  jump-if-false-scalar 6\n\
         \x20  4  load-scalar 0\n\
         \x20  5  jump 9\n\
         \x20  6  load-scalar 0\n\
         \x20  7  scalar-const 1\n\
         \x20  8  int Sub\n\
         \x20  9  return-scalar\n"
    );
}

/// An `if` with no `else` written as a statement builds nothing.
///
/// It is `()` however it goes — there is no second branch to give the
/// missing case a value — so as a statement there is no value to build in
/// either direction, and its branch is lowered for effect too.
#[test]
fn an_if_with_no_else_written_as_a_statement_builds_no_value() {
    assert_eq!(
        listing(
            "fn f(n: Int) -> Int {\n  var t = 0\n  if n < 2 {\n    t = 1\n  }\n  t\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=0/2 params=[Int] -> Int\n\
         \x20  0  scalar-const 0\n\
         \x20  1  store-scalar 1\n\
         \x20  2  load-scalar 0\n\
         \x20  3  scalar-const 2\n\
         \x20  4  int Lt\n\
         \x20  5  jump-if-false-scalar 8\n\
         \x20  6  scalar-const 1\n\
         \x20  7  store-scalar 1\n\
         \x20  8  load-scalar 1\n\
         \x20  9  return-scalar\n"
    );
}

/// The same `if` whose value is read is still `()` however it goes.
#[test]
fn an_if_with_no_else_whose_value_is_read_is_still_unit() {
    assert_eq!(
        listing(
            "fn f(n: Int) -> Unit {\n  var t = 0\n  if n < 2 {\n    t = 1\n  }\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=0/2 params=[Int] -> value\n\
         \x20  0  scalar-const 0\n\
         \x20  1  store-scalar 1\n\
         \x20  2  load-scalar 0\n\
         \x20  3  scalar-const 2\n\
         \x20  4  int Lt\n\
         \x20  5  jump-if-false-scalar 8\n\
         \x20  6  scalar-const 1\n\
         \x20  7  store-scalar 1\n\
         \x20  8  const Unit\n\
         \x20  9  return\n"
    );
}

/// A `while` written as a statement builds nothing, in the body or at the
/// end.
///
/// A loop is `()` however it leaves, so its body's value is never wanted
/// and neither is its own here: four instructions of test, four of body,
/// and the jump back, with no `Unit` anywhere in it.
#[test]
fn a_while_loop_tests_at_the_top_and_jumps_back() {
    assert_eq!(
        listing(
            "fn f() -> Int {\n  var i = 0\n  while i < 3 {\n    i += 1\n  }\n  i\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=0/1 -> Int\n\
         \x20  0  scalar-const 0\n\
         \x20  1  store-scalar 0\n\
         \x20  2  load-scalar 0\n\
         \x20  3  scalar-const 3\n\
         \x20  4  int Lt\n\
         \x20  5  jump-if-false-scalar 11\n\
         \x20  6  load-scalar 0\n\
         \x20  7  scalar-const 1\n\
         \x20  8  int Add\n\
         \x20  9  store-scalar 0\n\
         \x20 10  jump 2\n\
         \x20 11  load-scalar 0\n\
         \x20 12  return-scalar\n"
    );
}

/// The source every variadic test below is a call written into.
const VARIADIC: &str = "fn join(sep: String, items: String...) -> Int {\n  items.length()\n}\n";

/// One `join(...)` call, lowered as the body of `f`.
fn variadic_call(call: &str) -> String {
    listing(
        &format!("{VARIADIC}\nfn f() -> Int {{\n  {call}\n}}\n"),
        "f",
    )
}

/// A variadic parameter is one value slot, and the arguments that fill
/// it are collected into it at the call site.
///
/// This is the whole of the change: the callee still receives exactly
/// one argument per parameter — `argc=2/0` for two parameters — so the
/// calling convention does not move at all. `make-array` is where three
/// arguments become the two the callee is called with.
#[test]
fn a_variadic_call_collects_its_arguments_into_one() {
    assert_eq!(
        variadic_call("join(\"-\", \"a\", \"b\")"),
        "fn m.f arity=0 frame=0/0 -> Int\n\
         \x20  0  const Str(\"-\")\n\
         \x20  1  const Str(\"a\")\n\
         \x20  2  const Str(\"b\")\n\
         \x20  3  make-array 2\n\
         \x20  4  call m.join argc=2/0 -> scalar\n\
         \x20  5  return-scalar\n"
    );
}

/// A variadic parameter given nothing is an empty `Array`, not a missing
/// argument.
///
/// `Interpreter::assign_labels` leaves its slot empty and `rest` empty,
/// and `bind_params` builds `Value::Array` out of the two, so the callee
/// is still called with one argument for every parameter.
#[test]
fn a_variadic_parameter_given_nothing_is_an_empty_array() {
    assert_eq!(
        variadic_call("join(\"-\")"),
        "fn m.f arity=0 frame=0/0 -> Int\n\
         \x20  0  const Str(\"-\")\n\
         \x20  1  make-array 0\n\
         \x20  2  call m.join argc=2/0 -> scalar\n\
         \x20  3  return-scalar\n"
    );
    assert_eq!(
        listing(
            "fn count(items: Int...) -> Int {\n  items.length()\n}\n\nfn f() -> Int {\n  count()\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=0/0 -> Int\n\
         \x20  0  make-array 0\n\
         \x20  1  call m.count argc=1/0 -> scalar\n\
         \x20  2  return-scalar\n"
    );
}

/// A variadic parameter is a value slot even where its element type is
/// one the scalar stack holds.
///
/// `items: Int...` is an `Array<Int>` inside the body, and `params=[value]`
/// is what says the lowering read that rather than the `Int` the checker
/// recorded. `cove_sema::facts::Signature::params` answers what a *call*
/// supplies, which for a variadic parameter is its element type, so asking
/// the signature here would have numbered the slot in the scalar stack and
/// the callee would have loaded a word where an array was pushed.
#[test]
fn a_variadic_parameter_of_ints_is_still_a_value_slot() {
    assert_eq!(
        listing(
            "fn count(items: Int...) -> Int {\n  items.length()\n}\n\nfn f() -> Int {\n  count(1, 2)\n}\n",
            "count"
        ),
        "fn m.count arity=1 frame=1/0 params=[value] -> Int\n\
         \x20  0  load 0\n\
         \x20  1  call-builtin length argc=0\n\
         \x20  2  value-to-scalar\n\
         \x20  3  return-scalar\n"
    );
}

/// A label on the variadic parameter, written in its own place, is one
/// element.
///
/// `assign_labels` puts a labelled argument in that parameter's slot and
/// `bind_params` makes it the array's first element; no positional
/// argument may follow a label, so there is nothing else for the array
/// to hold. The call `join("-", "a", items: "b")` is the one where that
/// stops being true: the interpreter answers `["b", "a"]`, the labelled
/// argument before the one that fell past it, and pushing them as
/// written would have them the other way round. It is refused by name.
#[test]
fn a_labelled_variadic_argument_is_one_element() {
    assert_eq!(
        variadic_call("join(\"-\", items: \"a\")"),
        "fn m.f arity=0 frame=0/0 -> Int\n\
         \x20  0  const Str(\"-\")\n\
         \x20  1  const Str(\"a\")\n\
         \x20  2  make-array 1\n\
         \x20  3  call m.join argc=2/0 -> scalar\n\
         \x20  4  return-scalar\n"
    );
    assert_eq!(
        refused(&format!(
            "{VARIADIC}\nfn f() -> Int {{\n  join(\"-\", \"a\", items: \"b\")\n}}\n"
        )),
        "a call to `join` that labels its variadic parameter and passes more"
    );
}

/// A `...` passes an existing sequence where a variadic parameter's
/// elements would go.
///
/// `bind_params` reads an `Array`'s elements or a `Vector`'s and extends
/// the array it is building with them, so the callee still receives one
/// `Array` and the calling convention does not move. The call with no
/// spread in it still lowers to the single `make-array` it always did.
#[test]
fn a_spread_argument_extends_the_array_a_variadic_parameter_receives() {
    assert_eq!(
        variadic_call("join(\"-\", ...[\"a\", \"b\"])"),
        "fn m.f arity=0 frame=0/0 -> Int\n\
         \x20  0  const Str(\"-\")\n\
         \x20  1  make-array 0\n\
         \x20  2  const Str(\"a\")\n\
         \x20  3  const Str(\"b\")\n\
         \x20  4  make-array 2\n\
         \x20  5  spread-argument\n\
         \x20  6  call m.join argc=2/0 -> scalar\n\
         \x20  7  return-scalar\n"
    );
}

/// A spread mixed with ordinary arguments builds the array in runs.
///
/// Each run of ordinary arguments is one `make-array`, each spread is
/// its own value, and `spread-argument` joins each piece to what came
/// before — which is the order `bind_params` walks `rest` in.
#[test]
fn a_spread_mixed_with_ordinary_arguments_builds_the_array_in_runs() {
    assert_eq!(
        variadic_call("join(\"-\", \"w\", ...[\"a\"], \"z\")"),
        "fn m.f arity=0 frame=0/0 -> Int\n\
         \x20  0  const Str(\"-\")\n\
         \x20  1  make-array 0\n\
         \x20  2  const Str(\"w\")\n\
         \x20  3  make-array 1\n\
         \x20  4  spread-argument\n\
         \x20  5  const Str(\"a\")\n\
         \x20  6  make-array 1\n\
         \x20  7  spread-argument\n\
         \x20  8  const Str(\"z\")\n\
         \x20  9  make-array 1\n\
         \x20 10  spread-argument\n\
         \x20 11  call m.join argc=2/0 -> scalar\n\
         \x20 12  return-scalar\n"
    );
}

/// Everywhere a variadic parameter is not, a `...` is a marking the
/// interpreter ignores, and this refuses rather than reproducing it.
///
/// `println(...["a"])` hands `console.println` one `Array` and fails
/// against the schema; `k(...[1, 2, 3])` binds the whole array to `k`'s
/// one parameter; and `join("-", items: ...["a"])` makes the array one
/// element long, because `bind_params` reads a labelled variadic slot
/// through `value_of` and never looks at `spread`. None of the three is
/// a spread doing anything, so none of them lowers.
#[test]
fn a_spread_that_collects_nothing_is_refused() {
    assert_eq!(
        refused(
            "use console.println\n\nfn f() -> Result<Unit, Error> {\n  println(...[\"a\"])\n}\n"
        ),
        "a `...` spread argument to `println`, which collects nothing"
    );
    assert_eq!(
        refused(
            "fn k(a: Array<Int>) -> Int {\n  a.length()\n}\n\nfn f() -> Int {\n  k(...[1, 2, 3])\n}\n"
        ),
        "a `...` spread argument to `k`, which collects nothing"
    );
    assert_eq!(
        refused(&format!(
            "{VARIADIC}\nfn f() -> Int {{\n  join(\"-\", items: ...[\"a\"])\n}}\n"
        )),
        "a `...` spread argument to `join`, which collects nothing"
    );
}

/// A call that leaves a parameter to its default reaches a function
/// whose prologue computes it.
///
/// The default is evaluated by the callee — `bind_params` reaches
/// `None => match &param.default` inside the frame it is filling — so
/// the caller pushes only what it wrote and the callee's first
/// instructions are the ones the interpreter runs at the same moment.
/// The supplied parameter is slot 0 and the defaulted one slot 1,
/// because a specialisation numbers the supplied parameters first and
/// that is the whole of what keeps the calling convention where it was.
#[test]
fn a_parameter_left_to_its_default_is_computed_by_the_callee() {
    let source = "fn g(a: Int, b: Int = 2) -> Int {\n  a + b\n}\n\nfn f() -> Int {\n  g(1)\n}\n";
    assert_eq!(
        listing(source, "f"),
        "fn m.f arity=0 frame=0/0 -> Int\n\
         \x20  0  scalar-const 1\n\
         \x20  1  call m.g argc=0/1 -> scalar\n\
         \x20  2  return-scalar\n"
    );
    assert_eq!(
        specialisation(source, "g", 1),
        "fn m.g arity=1 frame=0/2 params=[Int] -> Int\n\
         \x20  0  scalar-const 2\n\
         \x20  1  store-scalar 1\n\
         \x20  2  load-scalar 0\n\
         \x20  3  load-scalar 1\n\
         \x20  4  int Add\n\
         \x20  5  return-scalar\n"
    );
}

/// Two call sites that supply different parameters are two functions,
/// and two that supply the same ones are one.
#[test]
fn a_supplied_set_is_what_a_call_numbers() {
    let source = "fn g(a: Int, b: Int = 2) -> Int {\n  a + b\n}\n\nfn f() -> Int {\n  g(1) + g(1, 3) + g(4)\n}\n";
    let program = lower(&checked(source)).expect("the program lowers");
    validate(&program).expect("the lowering holds the VM's invariants");
    assert_eq!(
        program
            .functions
            .iter()
            .filter(|function| &*function.name == "g")
            .map(|function| function.arity)
            .collect::<Vec<_>>(),
        [2, 1]
    );
}

/// A label may skip a parameter, because the one it skips has a default.
///
/// `assign_labels` matches a label to the parameter of that name and
/// only refuses one whose parameter stands before a parameter already
/// filled, so `measure(3, prefix: "d")` fills the first and the third.
/// The arguments still reach the callee in declaration order, which is
/// what lets them be pushed as written.
#[test]
fn a_label_may_skip_a_parameter_that_has_a_default() {
    let source = "fn measure(value: Int, unit: String = \"m\", prefix: String = \"length\") -> String {\n  \"{prefix}: {value}{unit}\"\n}\n\nfn f() -> String {\n  measure(3, prefix: \"d\")\n}\n";
    assert_eq!(
        listing(source, "f"),
        "fn m.f arity=0 frame=0/0 -> value\n\
         \x20  0  scalar-const 3\n\
         \x20  1  const Str(\"d\")\n\
         \x20  2  call m.measure argc=1/1\n\
         \x20  3  return\n"
    );
    assert_eq!(
        specialisation(source, "measure", 2),
        "fn m.measure arity=2 frame=2/1 params=[Int, value] -> value\n\
         \x20  0  const Str(\"m\")\n\
         \x20  1  store 1\n\
         \x20  2  load 0\n\
         \x20  3  const Str(\": \")\n\
         \x20  4  load-scalar 0\n\
         \x20  5  scalar-to-value Int\n\
         \x20  6  load 1\n\
         \x20  7  concat 4\n\
         \x20  8  return\n"
    );
}

/// A default may read a parameter declared before it, and refuses on one
/// declared after it.
///
/// `bind_params` declares each parameter as its own turn comes, so the
/// environment a default is evaluated in holds the parameters before it
/// and not the ones after. `fn f(a: Int = b, b: Int = 1)` is therefore
/// ``cannot find `b` in this scope`` on the interpreter, whatever the
/// call site supplies. A specialisation names its parameters in the same
/// order for the same reason, so the name is not one this body can
/// resolve and the lowering says so rather than reading a slot nothing
/// has written.
#[test]
fn a_default_reads_the_parameters_before_it_and_no_others() {
    assert_eq!(
        specialisation(
            "fn g(a: Int, b: Int = a * 2) -> Int {\n  b\n}\n\nfn f() -> Int {\n  g(3)\n}\n",
            "g",
            1
        ),
        "fn m.g arity=1 frame=0/2 params=[Int] -> Int\n\
         \x20  0  load-scalar 0\n\
         \x20  1  scalar-const 2\n\
         \x20  2  int Mul\n\
         \x20  3  store-scalar 1\n\
         \x20  4  load-scalar 1\n\
         \x20  5  return-scalar\n"
    );
    assert_eq!(
        refused("fn g(a: Int = b, b: Int = 1) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g()\n}\n"),
        "`b`, a name the lowering cannot resolve"
    );
}

/// A default before a variadic parameter is evaluated by the callee like
/// any other.
///
/// The two rules meet without either giving way: the call supplies the
/// variadic parameter, because an empty `Array` is an argument, and it
/// does not supply `sep`, so the specialisation it reaches takes one
/// argument and computes the other.
#[test]
fn a_default_before_a_variadic_parameter_is_computed_by_the_callee() {
    let source = "fn join(sep: String = \"-\", items: String...) -> Int {\n  items.length()\n}\n\nfn f() -> Int {\n  join()\n}\n";
    assert_eq!(
        listing(source, "f"),
        "fn m.f arity=0 frame=0/0 -> Int\n\
         \x20  0  make-array 0\n\
         \x20  1  call m.join argc=1/0 -> scalar\n\
         \x20  2  return-scalar\n"
    );
    assert_eq!(
        specialisation(source, "join", 1),
        "fn m.join arity=1 frame=2/0 params=[value] -> Int\n\
         \x20  0  const Str(\"-\")\n\
         \x20  1  store 1\n\
         \x20  2  load 0\n\
         \x20  3  call-builtin length argc=0\n\
         \x20  4  value-to-scalar\n\
         \x20  5  return-scalar\n"
    );
}

/// A range used as a value builds one, from two bounds on the scalar
/// stack.
///
/// The bounds are the checker's own answer about them — `a range runs
/// between two `Int`s` is what it checks each against — so they are
/// pushed the way every other settled operand is, and `make-range` is
/// where the two words become the one `Value` a `Range` is.
#[test]
fn a_range_used_as_a_value_is_built_from_two_scalar_bounds() {
    assert_eq!(
        listing("fn f() -> Range {\n  0..<3\n}\n", "f"),
        "fn m.f arity=0 frame=0/0 -> value\n\
         \x20  0  scalar-const 0\n\
         \x20  1  scalar-const 3\n\
         \x20  2  make-range ..<\n\
         \x20  3  return\n"
    );
}

/// `..` and `..<` are one instruction apart, and the difference is the
/// flag rather than the bounds.
///
/// It is not normalised away, because it is observable: `Value::eq_value`
/// compares it, `Display` writes the operator back out, and `0..<3` and
/// `0..2` are two values that yield the same integers.
#[test]
fn an_inclusive_range_value_differs_only_in_the_flag() {
    let inclusive = listing("fn f() -> Range {\n  0..3\n}\n", "f");
    let exclusive = listing("fn f() -> Range {\n  0..<3\n}\n", "f");
    assert!(inclusive.contains("   2  make-range ..\n"), "{inclusive}");
    assert_eq!(
        inclusive.replace("make-range ..", "make-range ..<"),
        exclusive
    );
}

/// A `Range` bound to a name, and asked one of its builtin methods.
///
/// The bounds need not be constants: a parameter the checker settled as
/// `Int` is already on the scalar stack, so it is loaded from there and
/// nothing crosses a boundary on the way in. `length()` is
/// `cove_schema::builtins::RANGE`'s own method and reaches
/// `builtins::call_method`, which is the interpreter's, so the two
/// backends compute it with one piece of code.
#[test]
fn a_range_can_be_bound_and_asked_its_methods() {
    assert_eq!(
        listing(
            "fn f(n: Int) -> Int {\n  let r = 1..n\n  r.length()\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=1/1 params=[Int] -> Int\n\
         \x20  0  scalar-const 1\n\
         \x20  1  load-scalar 0\n\
         \x20  2  make-range ..\n\
         \x20  3  store 0\n\
         \x20  4  load 0\n\
         \x20  5  call-builtin length argc=0\n\
         \x20  6  value-to-scalar\n\
         \x20  7  return-scalar\n"
    );
}

/// A range header never asks `iter-items` for anything.
///
/// It builds no value at all — not even the `Range` `make-range` exists
/// to build: the bounds go into two hidden slots and the loop counts
/// between them, which is faster than materialising every element and
/// answers exactly what walking the range's items would.
#[test]
fn a_for_over_a_range_counts_between_two_hidden_slots() {
    let listed = listing(
        "fn f() -> Int {\n  var t = 0\n  for i in 0..<3 {\n    t += i\n  }\n  t\n}\n",
        "f",
    );
    assert!(!listed.contains("iter-items"), "{listed}");
    assert!(!listed.contains("make-range"), "{listed}");
    assert_eq!(
        listing(
            "fn f() -> Int {\n  var t = 0\n  for i in 0..<3 {\n    t += i\n  }\n  t\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=3/1 -> Int\n\
         \x20  0  scalar-const 0\n\
         \x20  1  store-scalar 0\n\
         \x20  2  const Int(0)\n\
         \x20  3  store 0\n\
         \x20  4  const Int(3)\n\
         \x20  5  store 1\n\
         \x20  6  load 0\n\
         \x20  7  load 1\n\
         \x20  8  binary Lt\n\
         \x20  9  jump-if-false 22\n\
         \x20 10  load 0\n\
         \x20 11  store 2\n\
         \x20 12  load-scalar 0\n\
         \x20 13  load 2\n\
         \x20 14  value-to-scalar\n\
         \x20 15  int Add\n\
         \x20 16  store-scalar 0\n\
         \x20 17  load 0\n\
         \x20 18  const Int(1)\n\
         \x20 19  binary Add\n\
         \x20 20  store 0\n\
         \x20 21  jump 6\n\
         \x20 22  load-scalar 0\n\
         \x20 23  return-scalar\n"
    );
}

/// `a..b` yields `b` and `a..<b` stops before it, which is the one
/// difference between the two headers.
#[test]
fn an_inclusive_range_tests_with_le() {
    let inclusive = listing(
        "fn f() -> Int {\n  var t = 0\n  for i in 0..3 {\n    t += i\n  }\n  t\n}\n",
        "f",
    );
    let exclusive = listing(
        "fn f() -> Int {\n  var t = 0\n  for i in 0..<3 {\n    t += i\n  }\n  t\n}\n",
        "f",
    );
    assert!(inclusive.contains("   8  binary Le\n"), "{inclusive}");
    assert_eq!(inclusive.replace("binary Le", "binary Lt"), exclusive);
}

/// A `for` over a sequence asks `iter-items` what it walks it as, once,
/// and walks the `Array` that comes back by index.
///
/// The instruction is what makes the loop right for a `Map` and a `Set`
/// as well: they answer neither `length()` nor `get(i)`, and the walk
/// never asks them to, because what it walks is the `Array` of their
/// items rather than the collection itself.
#[test]
fn a_for_over_a_sequence_asks_for_its_items_and_walks_them_by_index() {
    assert_eq!(
        listing(
            "fn f(items: Array<Int>) -> Int {\n  var t = 0\n  for item in items {\n    t += item\n  }\n  t\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=5/1 params=[value] -> Int\n\
         \x20  0  scalar-const 0\n\
         \x20  1  store-scalar 0\n\
         \x20  2  load 0\n\
         \x20  3  iter-items\n\
         \x20  4  store 1\n\
         \x20  5  load 1\n\
         \x20  6  call-builtin length argc=0\n\
         \x20  7  store 2\n\
         \x20  8  const Int(0)\n\
         \x20  9  store 3\n\
         \x20 10  load 3\n\
         \x20 11  load 2\n\
         \x20 12  binary Lt\n\
         \x20 13  jump-if-false 29\n\
         \x20 14  load 1\n\
         \x20 15  load 3\n\
         \x20 16  call-builtin get argc=1\n\
         \x20 17  try\n\
         \x20 18  store 4\n\
         \x20 19  load-scalar 0\n\
         \x20 20  load 4\n\
         \x20 21  value-to-scalar\n\
         \x20 22  int Add\n\
         \x20 23  store-scalar 0\n\
         \x20 24  load 3\n\
         \x20 25  const Int(1)\n\
         \x20 26  binary Add\n\
         \x20 27  store 3\n\
         \x20 28  jump 10\n\
         \x20 29  load-scalar 0\n\
         \x20 30  return-scalar\n"
    );
}

/// `break` leaves the loop and `continue` lands where the cursor is
/// advanced, so skipping the rest of a body still makes progress.
#[test]
fn break_leaves_the_loop_and_continue_reaches_the_next_iteration() {
    assert_eq!(
        listing(
            "fn f() -> Int {\n  var i = 0\n  while i < 10 {\n    i += 1\n    if i == 2 {\n      continue\n    }\n    if i == 5 {\n      break\n    }\n  }\n  i\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=0/1 -> Int\n\
         \x20  0  scalar-const 0\n\
         \x20  1  store-scalar 0\n\
         \x20  2  load-scalar 0\n\
         \x20  3  scalar-const 10\n\
         \x20  4  int Lt\n\
         \x20  5  jump-if-false-scalar 21\n\
         \x20  6  load-scalar 0\n\
         \x20  7  scalar-const 1\n\
         \x20  8  int Add\n\
         \x20  9  store-scalar 0\n\
         \x20 10  load-scalar 0\n\
         \x20 11  scalar-const 2\n\
         \x20 12  int Eq\n\
         \x20 13  jump-if-false-scalar 15\n\
         \x20 14  jump 2\n\
         \x20 15  load-scalar 0\n\
         \x20 16  scalar-const 5\n\
         \x20 17  int Eq\n\
         \x20 18  jump-if-false-scalar 20\n\
         \x20 19  jump 21\n\
         \x20 20  jump 2\n\
         \x20 21  load-scalar 0\n\
         \x20 22  return-scalar\n"
    );
}

#[test]
fn a_call_reaches_a_declaration_and_a_function_reaches_itself() {
    assert_eq!(
        listing(
            "fn fib(n: Int) -> Int {\n  if n < 2 {\n    n\n  } else {\n    fib(n - 1) + fib(n - 2)\n  }\n}\n",
            "fib"
        ),
        "fn m.fib arity=1 frame=0/1 params=[Int] -> Int\n\
         \x20  0  load-scalar 0\n\
         \x20  1  scalar-const 2\n\
         \x20  2  int Lt\n\
         \x20  3  jump-if-false-scalar 6\n\
         \x20  4  load-scalar 0\n\
         \x20  5  jump 15\n\
         \x20  6  load-scalar 0\n\
         \x20  7  scalar-const 1\n\
         \x20  8  int Sub\n\
         \x20  9  call m.fib argc=0/1 -> scalar\n\
         \x20 10  load-scalar 0\n\
         \x20 11  scalar-const 2\n\
         \x20 12  int Sub\n\
         \x20 13  call m.fib argc=0/1 -> scalar\n\
         \x20 14  int Add\n\
         \x20 15  return-scalar\n"
    );
}

#[test]
fn arguments_are_pushed_left_to_right() {
    assert_eq!(
        listing(
            "fn g(a: Int, b: Int) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g(1, 2)\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=0/0 -> Int\n\
         \x20  0  scalar-const 1\n\
         \x20  1  scalar-const 2\n\
         \x20  2  call m.g argc=0/2 -> scalar\n\
         \x20  3  return-scalar\n"
    );
}

/// The convention itself, in the smallest program that states it: a
/// parameter the checker settled travels on the scalar stack and becomes
/// the callee's scalar slot, and the answer comes back the same way.
/// Nothing crosses between the stacks, on either side of the call.
#[test]
fn a_settled_parameter_and_a_settled_answer_travel_on_the_scalar_stack() {
    let source =
        "fn identity(value: Int) -> Int {\n  value\n}\n\nfn f() -> Int {\n  identity(1)\n}\n";
    assert_eq!(
        listing(source, "identity"),
        "fn m.identity arity=1 frame=0/1 params=[Int] -> Int\n\
         \x20  0  load-scalar 0\n\
         \x20  1  return-scalar\n"
    );
    assert_eq!(
        listing(source, "f"),
        "fn m.f arity=0 frame=0/0 -> Int\n\
         \x20  0  scalar-const 1\n\
         \x20  1  call m.identity argc=0/1 -> scalar\n\
         \x20  2  return-scalar\n"
    );
}

/// Each argument goes to the stack its own parameter names, and within
/// each stack they land in the order that stack's slots are numbered in
/// — which is why nothing has to be moved once they are pushed.
///
/// `g`'s frame is one value slot and one scalar slot, and `tag` is value
/// slot 0 while `n` is scalar slot 0: the numbering is dense inside each
/// stack and says nothing about the other.
#[test]
fn an_argument_travels_on_the_stack_its_own_type_names() {
    let source = "fn g(n: Int, tag: String, k: Int) -> String {\n  tag\n}\n\nfn f() -> String {\n  g(1, \"a\", 2)\n}\n";
    assert_eq!(
        listing(source, "g"),
        "fn m.g arity=3 frame=1/2 params=[Int, value, Int] -> value\n\
         \x20  0  load 0\n\
         \x20  1  return\n"
    );
    assert_eq!(
        listing(source, "f"),
        "fn m.f arity=0 frame=0/0 -> value\n\
         \x20  0  scalar-const 1\n\
         \x20  1  const Str(\"a\")\n\
         \x20  2  scalar-const 2\n\
         \x20  3  call m.g argc=1/2\n\
         \x20  4  return\n"
    );
}

/// A receiver is pushed first because it is the first thing `params`
/// names, and it goes to its own stack like any other argument — which
/// is the value stack, because a method is declared on a struct or an
/// enum.
#[test]
fn a_receiver_is_the_first_argument_and_travels_on_its_own_stack() {
    let source = "struct P {\n  x: Int\n}\n\nimpl P {\n  fn plus(self, by: Int) -> Int {\n    self.x + by\n  }\n}\n\nfn f(p: P) -> Int {\n  p.plus(by: 2)\n}\n";
    assert_eq!(
        listing(source, "P.plus"),
        "fn m.P.plus arity=2 frame=1/1 params=[value, Int] receiver -> Int\n\
         \x20  0  load 0\n\
         \x20  1  get-field-at-scalar 0\n\
         \x20  2  load-scalar 0\n\
         \x20  3  int Add\n\
         \x20  4  return-scalar\n"
    );
    assert_eq!(
        listing(source, "f"),
        "fn m.f arity=1 frame=1/0 params=[value] -> Int\n\
         \x20  0  load 0\n\
         \x20  1  scalar-const 2\n\
         \x20  2  call m.P.plus argc=1/1 -> scalar\n\
         \x20  3  return-scalar\n"
    );
}

/// A scalar answer crosses only where something on the other stack reads
/// it: one boundary instruction where a value is wanted, the scalar
/// stack's own discard where nothing is, and neither where a scalar was
/// wanted anyway.
#[test]
fn a_scalar_answer_crosses_only_where_a_value_reads_it() {
    let source =
        "fn g() -> Int {\n  1\n}\n\nfn f() -> String {\n  g()\n  let n = g() + 1\n  \"{g()}\"\n}\n";
    assert_eq!(
        listing(source, "f"),
        "fn m.f arity=0 frame=0/1 -> value\n\
         \x20  0  call m.g argc=0/0 -> scalar\n\
         \x20  1  scalar-pop\n\
         \x20  2  call m.g argc=0/0 -> scalar\n\
         \x20  3  scalar-const 1\n\
         \x20  4  int Add\n\
         \x20  5  store-scalar 0\n\
         \x20  6  call m.g argc=0/0 -> scalar\n\
         \x20  7  scalar-to-value Int\n\
         \x20  8  concat 1\n\
         \x20  9  return\n"
    );
}

/// A `Bool` a call left on the scalar stack is branched on where it
/// stands, rather than moved across to be tested.
#[test]
fn a_bool_a_call_answered_is_tested_where_it_stands() {
    let source = "fn big(n: Int) -> Bool {\n  n > 2\n}\n\nfn f(n: Int) -> Int {\n  if big(n) {\n    1\n  } else {\n    0\n  }\n}\n";
    assert_eq!(
        listing(source, "f"),
        "fn m.f arity=1 frame=0/1 params=[Int] -> Int\n\
         \x20  0  load-scalar 0\n\
         \x20  1  call m.big argc=0/1 -> scalar\n\
         \x20  2  jump-if-false-scalar 5\n\
         \x20  3  scalar-const 1\n\
         \x20  4  jump 6\n\
         \x20  5  scalar-const 0\n\
         \x20  6  return-scalar\n"
    );
}

const STRUCT_AND_METHOD: &str = "struct P {\n  x: Int\n  y: Int\n}\n\nimpl P {\n  fn sum(self) -> Int {\n    self.x + self.y\n  }\n}\n\nfn f() -> Int {\n  let p = P(x: 1, y: 2)\n  p.sum() + p.x\n}\n";

#[test]
fn a_struct_is_built_in_declaration_order_and_read_by_field() {
    assert_eq!(
        listing(STRUCT_AND_METHOD, "f"),
        "fn m.f arity=0 frame=1/0 -> Int\n\
         \x20  0  const Int(1)\n\
         \x20  1  const Int(2)\n\
         \x20  2  make-struct m.P fields=x,y\n\
         \x20  3  store 0\n\
         \x20  4  load 0\n\
         \x20  5  call m.P.sum argc=1/0 -> scalar\n\
         \x20  6  load 0\n\
         \x20  7  get-field-at-scalar 0\n\
         \x20  8  int Add\n\
         \x20  9  return-scalar\n"
    );
}

#[test]
fn a_method_takes_its_receiver_in_slot_zero() {
    assert_eq!(
        listing(STRUCT_AND_METHOD, "P.sum"),
        "fn m.P.sum arity=1 frame=1/0 params=[value] receiver -> Int\n\
         \x20  0  load 0\n\
         \x20  1  get-field-at-scalar 0\n\
         \x20  2  load 0\n\
         \x20  3  get-field-at-scalar 1\n\
         \x20  4  int Add\n\
         \x20  5  return-scalar\n"
    );
}

/// `snapshot` splits in two by the receiver's type, not by its name.
///
/// A struct with an `impl Snapshot for Type` is an ordinary method call:
/// the checker recorded which declaration it reaches, so it lowers to a
/// `Call` before the name is asked about at all. Everything a
/// conformance is not consulted about is the `snapshot` instruction.
#[test]
fn snapshot_is_a_call_where_a_conformance_answers_and_an_instruction_where_none_does() {
    assert_eq!(
        listing(
            "struct B {\n  n: Int\n}\n\nimpl Snapshot for B {\n  fn snapshot(self) -> B {\n    B(n: self.n)\n  }\n}\n\nfn f(b: B) -> B {\n  b.snapshot()\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=1/0 params=[value] -> value\n\
         \x20  0  load 0\n\
         \x20  1  call m.B.snapshot argc=1/0\n\
         \x20  2  return\n"
    );
    assert_eq!(
        listing(
            "fn f(v: Vector<Int>) -> Vector<Int> {\n  v.snapshot()\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=1/0 params=[value] -> value\n\
         \x20  0  load 0\n\
         \x20  1  snapshot\n\
         \x20  2  return\n"
    );
}

/// A `Vector` whose elements dispatch is refused, and so is a receiver
/// the checker settled nothing about.
///
/// `Interpreter::snapshot` walks a `Vector` one element at a time and
/// sends each struct to its own conformance. An instruction cannot run a
/// whole Cove function in the middle of itself, so the refusal is here,
/// before the run, rather than a failure during one.
#[test]
fn snapshot_refuses_where_it_would_have_to_reach_a_conformance() {
    assert_eq!(
        refused(
            "struct B {\n  n: Int\n}\n\nimpl Snapshot for B {\n  fn snapshot(self) -> B {\n    B(n: self.n)\n  }\n}\n\nfn f(v: Vector<B>) -> Vector<B> {\n  v.snapshot()\n}\n"
        ),
        "`snapshot` on a `Vector<B>`, which a conformance answers"
    );
}

/// A type a host module declares is built here rather than asked for
/// across the boundary.
///
/// `Interpreter::init_host_type` is `init_struct` with the field names
/// read from a `TypeSchema`: the same `assign_labels`, one value per
/// field, and an ordinary `Value::Struct` named `{module}.{Name}`. So it
/// lowers to `make-struct`, the same instruction a declared struct's
/// initializer lowers to, and no `call-host` is emitted for it.
#[test]
fn a_type_a_host_declares_is_built_like_any_other_struct() {
    assert_eq!(
        listing(
            "use http\n\nfn f() -> Int {\n  http.Response(status: 200, body: \"ok\").status\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=0/0 -> Int\n\
         \x20  0  const Int(200)\n\
         \x20  1  const Str(\"ok\")\n\
         \x20  2  make-struct http.Response fields=status,body\n\
         \x20  3  get-field status\n\
         \x20  4  value-to-scalar\n\
         \x20  5  return-scalar\n"
    );
}

#[test]
fn a_host_operation_is_called_through_its_module() {
    assert_eq!(
        listing(
            "use console.println\nuse clock\n\nfn f() -> Result<Unit, Error> {\n  let at = clock.now()\n  println(\"at {at}\")?\n  Ok(())\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=1/0 -> value\n\
         \x20  0  call-host clock.now argc=0\n\
         \x20  1  store 0\n\
         \x20  2  const Str(\"at \")\n\
         \x20  3  load 0\n\
         \x20  4  concat 2\n\
         \x20  5  call-host console.println argc=1\n\
         \x20  6  try\n\
         \x20  7  pop\n\
         \x20  8  const Unit\n\
         \x20  9  make-builtin Ok argc=1\n\
         \x20 10  return\n"
    );
}

#[test]
fn a_resource_operation_is_called_through_the_handle_it_stands_on() {
    assert_eq!(
        listing(
            "use http\n\nfn f() -> Result<Unit, Error> {\n  let server = http.listen(0)?\n  server.close()\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=1/0 -> value\n\
         \x20  0  const Int(0)\n\
         \x20  1  call-host http.listen argc=1\n\
         \x20  2  try\n\
         \x20  3  store 0\n\
         \x20  4  load 0\n\
         \x20  5  call-resource close argc=0\n\
         \x20  6  return\n"
    );
}

#[test]
fn a_resource_operation_takes_its_handle_below_its_arguments() {
    assert_eq!(
        listing(
            "use files\n\nfn f(line: String) -> Result<Unit, Error> {\n  let writer = files.create(\"notes.txt\")?\n  writer.writeLine(line)\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=2/0 params=[value] -> value\n\
         \x20  0  const Str(\"notes.txt\")\n\
         \x20  1  call-host files.create argc=1\n\
         \x20  2  try\n\
         \x20  3  store 1\n\
         \x20  4  load 1\n\
         \x20  5  load 0\n\
         \x20  6  call-resource writeLine argc=1\n\
         \x20  7  return\n"
    );
}

#[test]
fn a_builtin_method_takes_its_receiver_below_its_arguments() {
    assert_eq!(
        listing(
            "fn f(items: Array<Int>) -> Int {\n  items.get(0).unwrapOr(7)\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=1/0 params=[value] -> Int\n\
         \x20  0  load 0\n\
         \x20  1  const Int(0)\n\
         \x20  2  call-builtin get argc=1\n\
         \x20  3  const Int(7)\n\
         \x20  4  call-builtin unwrapOr argc=1\n\
         \x20  5  value-to-scalar\n\
         \x20  6  return-scalar\n"
    );
}

#[test]
fn a_free_builtin_is_built_from_its_arguments() {
    assert_eq!(
        listing(
            "fn f() -> Result<Unit, Error> {\n  assertEqual(1, 1)?\n  Ok(())\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=0/0 -> value\n\
         \x20  0  const Int(1)\n\
         \x20  1  const Int(1)\n\
         \x20  2  make-builtin assertEqual argc=2\n\
         \x20  3  try\n\
         \x20  4  pop\n\
         \x20  5  const Unit\n\
         \x20  6  make-builtin Ok argc=1\n\
         \x20  7  return\n"
    );
}

/// An assertion carries the spans of its arguments, and nothing else
/// does.
///
/// A failing `assert` quotes the source text of its condition — that is
/// what makes it a builtin rather than a library function — and an
/// instruction's own span covers the whole call, so the argument's span
/// is recorded beside the instruction. A constructor quotes nothing, so
/// it carries nothing: a span no diagnostic reads would be a cost with
/// no reader.
#[test]
fn an_assertion_carries_the_spans_of_its_arguments() {
    let source = "fn f() -> Result<Unit, Error> {\n  assertEqual(1 + 1, 3)?\n  Ok(())\n}\n";
    let program = lower(&checked(source)).expect("it lowers");
    validate(&program).expect("it holds the invariants");
    let function = program.function(program.function_named("m", "f").expect("`f` is lowered"));
    let made: Vec<usize> = function
        .code
        .iter()
        .enumerate()
        .filter(|(_, inst)| matches!(inst, Inst::MakeBuiltin { .. }))
        .map(|(pc, _)| pc)
        .collect();
    let [assertion, constructor] = made[..] else {
        panic!("the body builds the assertion and the `Ok`: {made:?}");
    };
    let quoted: Vec<&str> = function
        .arg_spans_at(assertion)
        .iter()
        .map(|span| &source[span.start as usize..span.end as usize])
        .collect();
    assert_eq!(quoted, ["1 + 1", "3"]);
    assert!(function.arg_spans_at(constructor).is_empty());
}

/// `None` is the one builtin case written as a bare name rather than as
/// a call, so it is the one that builds from no arguments.
#[test]
fn none_is_built_from_nothing() {
    assert_eq!(
        listing("fn f() -> Option<Int> {\n  None\n}\n", "f"),
        "fn m.f arity=0 frame=0/0 -> value\n\
         \x20  0  make-builtin None argc=0\n\
         \x20  1  return\n"
    );
}

#[test]
fn an_array_literal_collects_its_elements_left_to_right() {
    assert_eq!(
        listing("fn f() -> Array<Int> {\n  [1, 2, 3]\n}\n", "f"),
        "fn m.f arity=0 frame=0/0 -> value\n\
         \x20  0  const Int(1)\n\
         \x20  1  const Int(2)\n\
         \x20  2  const Int(3)\n\
         \x20  3  make-array 3\n\
         \x20  4  return\n"
    );
}

#[test]
fn a_question_mark_opens_what_it_is_given() {
    assert_eq!(
        listing(
            "fn f(v: Option<Int>) -> Option<Int> {\n  Some(v? + 1)\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=1/0 params=[value] -> value\n\
         \x20  0  load 0\n\
         \x20  1  try\n\
         \x20  2  value-to-scalar\n\
         \x20  3  scalar-const 1\n\
         \x20  4  int Add\n\
         \x20  5  scalar-to-value Int\n\
         \x20  6  make-builtin Some argc=1\n\
         \x20  7  return\n"
    );
}

#[test]
fn a_return_ends_the_function_where_it_is_written() {
    assert_eq!(
        listing(
            "fn f(n: Int) -> Int {\n  if n < 0 {\n    return 0\n  }\n  return n\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=0/1 params=[Int] -> Int\n\
         \x20  0  load-scalar 0\n\
         \x20  1  scalar-const 0\n\
         \x20  2  int Lt\n\
         \x20  3  jump-if-false-scalar 6\n\
         \x20  4  scalar-const 0\n\
         \x20  5  return-scalar\n\
         \x20  6  load-scalar 0\n\
         \x20  7  return-scalar\n"
    );
}
