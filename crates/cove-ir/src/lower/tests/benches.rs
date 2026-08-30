use super::*;

// ------------------------------------------------------------ benches

/// ADR 0012's benchmark package is the target, and seven of its entries
/// lower.
///
/// `callback` is here because it is the one entry whose body reaches the
/// evaluator from inside a builtin: `filter` over a closure. A lowering that
/// refused it would leave issue #193's row measurable on one backend only,
/// and a benchmark that runs on one backend cannot be read against the
/// other.
#[test]
fn seven_of_the_bench_entries_lower_and_validate() {
    for name in [
        "pure",
        "hostheavy",
        "arith",
        "arrayget",
        "call",
        "chars",
        "callback",
    ] {
        let program = match lower(&bench(name)) {
            Ok(program) => program,
            Err(why) => panic!("`benches/{name}` lowers, but stopped at {why}"),
        };
        assert!(
            program.function_named(name, "main").is_some(),
            "`benches/{name}` lowers its entry"
        );
        validate(&program)
            .unwrap_or_else(|why| panic!("`benches/{name}` holds the invariants: {why}"));
    }
}

/// `benches/arith`'s loop, which is what lowering for effect was measured
/// on.
///
/// Every statement in it is one of the three that build nothing now: two
/// compound assignments and an `if` with no `else`. Nineteen instructions
/// run on an iteration that takes the branch and fifteen on one that does
/// not, where before it was twenty-five and nineteen — six of them a
/// `const Unit` and the `pop` that took it away again.
#[test]
fn the_arith_bench_loop_builds_no_value_it_does_not_use() {
    let program = lower(&bench("arith")).expect("`benches/arith` lowers");
    validate(&program).expect("it holds the invariants");
    let id = program
        .function_named("arith", "main")
        .expect("its entry is lowered");
    assert_eq!(
        crate::render(&program, id),
        "fn arith.main arity=0 frame=0/2 -> value\n\
         \x20  0  scalar-const 0\n\
         \x20  1  store-scalar 0\n\
         \x20  2  scalar-const 0\n\
         \x20  3  store-scalar 1\n\
         \x20  4  load-scalar 1\n\
         \x20  5  scalar-const 2000000\n\
         \x20  6  int Lt\n\
         \x20  7  jump-if-false-scalar 23\n\
         \x20  8  load-scalar 1\n\
         \x20  9  scalar-const 7\n\
         \x20 10  int Rem\n\
         \x20 11  scalar-const 0\n\
         \x20 12  int Eq\n\
         \x20 13  jump-if-false-scalar 18\n\
         \x20 14  load-scalar 0\n\
         \x20 15  scalar-const 1\n\
         \x20 16  int Add\n\
         \x20 17  store-scalar 0\n\
         \x20 18  load-scalar 1\n\
         \x20 19  scalar-const 1\n\
         \x20 20  int Add\n\
         \x20 21  store-scalar 1\n\
         \x20 22  jump 4\n\
         \x20 23  load-scalar 0\n\
         \x20 24  scalar-to-value Int\n\
         \x20 25  const Int(285715)\n\
         \x20 26  make-builtin assertEqual argc=2\n\
         \x20 27  try\n\
         \x20 28  pop\n\
         \x20 29  const Unit\n\
         \x20 30  make-builtin Ok argc=1\n\
         \x20 31  return\n"
    );

    // The hot loop, from the test at its top to the jump back, holds no
    // instruction that reads or writes the value stack. The
    // `assertEqual` below it is the boundary, and it is outside the loop.
    let function = program.function(id);
    for inst in &function.code[4..=22] {
        let shape = stack_shape(&program.constants, *inst);
        assert_eq!(
            (shape.values.0, shape.values.1),
            (0, 0),
            "`arith`'s loop runs no general `Value` operation, and {inst:?} is one"
        );
    }
}

/// The other two lower through the instruction that writes a field, and
/// it is one construct they share: `cursor.at += cursor.step`.
///
/// [`crate::Inst::SetField`] is what they reach, so this asserts that
/// they reach it rather than only that they lowered — a lowering that
/// arrived at the same answer some other way would be a different
/// program with the same result.
#[test]
fn field_and_method_lower_through_a_written_field() {
    for name in ["field", "method"] {
        let program = lower(&bench(name)).unwrap_or_else(|why| {
            panic!("`benches/{name}` lowers, but: {why}");
        });
        validate(&program).expect("it holds the invariants");
        let id = program
            .function_named(name, "main")
            .expect("its entry is lowered");
        let listing = crate::render(&program, id);
        assert!(
            listing.contains("set-field at"),
            "`benches/{name}` writes a field:\n{listing}"
        );
    }
}

/// A compound write reads the field, computes, and writes it back, and
/// the struct it writes back to is the one it read from.
#[test]
fn a_compound_field_write_reads_the_field_it_writes() {
    let program = lower(&checked(
        "struct P {\n  x: Int\n}\n\nexport fn f() -> Int {\n  var p = P(x: 1)\n  p.x += 2\n  p.x\n}\n",
    ))
    .expect("it lowers");
    validate(&program).expect("it holds the invariants");
    let id = program.function_named("m", "f").expect("`f` is lowered");
    assert_eq!(
        crate::render(&program, id),
        "fn m.f arity=0 frame=1/0 -> Int\n\
         \x20  0  const Int(1)\n\
         \x20  1  make-struct m.P fields=x\n\
         \x20  2  store 0\n\
         \x20  3  load 0\n\
         \x20  4  dup\n\
         \x20  5  get-field-at 0\n\
         \x20  6  value-to-scalar\n\
         \x20  7  scalar-const 2\n\
         \x20  8  int Add\n\
         \x20  9  scalar-to-value Int\n\
         \x20 10  set-field x\n\
         \x20 11  store 0\n\
         \x20 12  load 0\n\
         \x20 13  get-field-at-scalar 0\n\
         \x20 14  return-scalar\n"
    );
}

/// `push` needs no place: `Value::Vector` is a handle, so the receiver of
/// a `var` binding is read like any other value's and handed to
/// `Inst::CallBuiltin` exactly as a non-mutating method would be.
#[test]
fn push_on_a_var_binding_lowers_like_any_other_builtin_method() {
    assert_eq!(
        listing(
            "fn f() -> Int {\n  var v = Vector.of()\n  v.push(1)\n  v.length()\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=1/0 -> Int\n\
         \x20  0  call-assoc Vector.of argc=0\n\
         \x20  1  store 0\n\
         \x20  2  load 0\n\
         \x20  3  const Int(1)\n\
         \x20  4  call-builtin push argc=1\n\
         \x20  5  pop\n\
         \x20  6  load 0\n\
         \x20  7  call-builtin length argc=0\n\
         \x20  8  value-to-scalar\n\
         \x20  9  return-scalar\n"
    );
}

/// A field path is still a place: `Place::field` in
/// `crates/cove-runtime/src/interp.rs` steps from a place to a place, and
/// `Body::is_a_place` mirrors it, so `s.items.push` reaches the same
/// fall-through `v.push` above does.
#[test]
fn push_through_a_var_struct_field_lowers() {
    assert_eq!(
        listing(
            "struct S {\n  items: Vector<Int>\n}\n\nfn f() -> Int {\n  var s = S(items: Vector.of())\n  s.items.push(1)\n  s.items.length()\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=1/0 -> Int\n\
         \x20  0  call-assoc Vector.of argc=0\n\
         \x20  1  make-struct m.S fields=items\n\
         \x20  2  store 0\n\
         \x20  3  load 0\n\
         \x20  4  get-field-at 0\n\
         \x20  5  const Int(1)\n\
         \x20  6  call-builtin push argc=1\n\
         \x20  7  pop\n\
         \x20  8  load 0\n\
         \x20  9  get-field-at 0\n\
         \x20 10  call-builtin length argc=0\n\
         \x20 11  value-to-scalar\n\
         \x20 12  return-scalar\n"
    );
}

// `push` on a read-only place and `push` on a temporary were two tests
// here. Both are `cove::type::` errors since ADR 0021, so neither
// program reaches this pass and `cove-sema` is where they are pinned.

/// `freeze` takes the place and not a read of it, which is the whole
/// difference between it and `push`: the uniqueness check has to see the
/// caller's own handle exactly once, and `place-read` would be a second
/// one.
#[test]
fn freeze_takes_the_place_and_not_a_read_of_it() {
    assert_eq!(
        listing(
            "fn f() -> Int {\n  var v = Vector.of()\n  let frozen = v.freeze()\n  frozen.length()\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=2/0 -> Int\n\
         \x20  0  call-assoc Vector.of argc=0\n\
         \x20  1  store 0\n\
         \x20  2  place 0\n\
         \x20  3  freeze\n\
         \x20  4  store 1\n\
         \x20  5  load 1\n\
         \x20  6  call-builtin length argc=0\n\
         \x20  7  value-to-scalar\n\
         \x20  8  return-scalar\n"
    );
}

/// A receiver that is not a place at all keeps the ordinary builtin
/// lowering, exactly as `Interpreter::call_builtin_method` falls through
/// to `builtins::call_method` for one: the temporary holds the only
/// handle there is, so the count is right without a place.
#[test]
fn freeze_on_a_temporary_reads_it_like_any_other_builtin() {
    assert_eq!(
        listing(
            "fn f() -> Int {\n  Vector.of(1).freeze().length()\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=0/0 -> Int\n\
         \x20  0  const Int(1)\n\
         \x20  1  call-assoc Vector.of argc=1\n\
         \x20  2  call-builtin freeze argc=0\n\
         \x20  3  call-builtin length argc=0\n\
         \x20  4  value-to-scalar\n\
         \x20  5  return-scalar\n"
    );
}
