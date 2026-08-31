use super::*;

use crate::Scalar;

// -------------------------------------------------------------- slots

#[test]
fn shadowing_declares_a_second_slot() {
    assert_eq!(
        listing(
            "fn f() -> Int {\n  let x = 1\n  let x = x + 1\n  x\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=2/0 -> Int\n\
         \x20  0  scalar-const 1\n\
         \x20  1  store-scalar 0\n\
         \x20  2  load-scalar 0\n\
         \x20  3  scalar-const 1\n\
         \x20  4  int Add\n\
         \x20  5  store-scalar 1\n\
         \x20  6  load-scalar 1\n\
         \x20  7  return-scalar\n"
    );
}

/// A block's slots are released at its end, so the block after it takes
/// the same numbers and the frame is as big as the deepest block rather
/// than as big as the whole body.
#[test]
fn sibling_blocks_reuse_the_slots_the_first_released() {
    assert_eq!(
        listing(
            "fn f() -> Int {\n  {\n    let a = 1\n    a\n  }\n  {\n    let b = 2\n    b\n  }\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=1/0 -> Int\n\
         \x20  0  scalar-const 1\n\
         \x20  1  store-scalar 0\n\
         \x20  2  load-scalar 0\n\
         \x20  3  scalar-to-value Int\n\
         \x20  4  pop\n\
         \x20  5  scalar-const 2\n\
         \x20  6  store-scalar 0\n\
         \x20  7  load-scalar 0\n\
         \x20  8  return-scalar\n"
    );
}

/// A frame size is the high-water mark: three bindings are live at once
/// inside the nested block, and one of them is the outer body's.
#[test]
fn the_frame_is_as_big_as_the_most_that_was_ever_live() {
    assert_eq!(
        listing(
            "fn f() -> Int {\n  let a = 1\n  {\n    let b = 2\n    let c = 3\n    b + c\n  }\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=3/0 -> Int\n\
         \x20  0  scalar-const 1\n\
         \x20  1  store-scalar 0\n\
         \x20  2  scalar-const 2\n\
         \x20  3  store-scalar 1\n\
         \x20  4  scalar-const 3\n\
         \x20  5  store-scalar 2\n\
         \x20  6  load-scalar 1\n\
         \x20  7  load-scalar 2\n\
         \x20  8  int Add\n\
         \x20  9  return-scalar\n"
    );
}

/// A name resolves in declaration order, so the value of a `let` is read
/// before the name it declares exists.
#[test]
fn let_x_equals_x_reads_the_outer_binding() {
    assert_eq!(
        listing(
            "fn f(x: Int) -> Int {\n  {\n    let x = x\n    x\n  }\n}\n",
            "f"
        ),
        "fn m.f arity=1 frame=2/0 params=[Int] -> Int\n\
         \x20  0  load-scalar 0\n\
         \x20  1  store-scalar 1\n\
         \x20  2  load-scalar 1\n\
         \x20  3  return-scalar\n"
    );
}

// ---------------------------------------------------- the per-slot layout

/// A scalar parameter takes the first free scalar-region number and a value
/// parameter the first free value-region number — that is
/// `Function::params` read as a convention. `Function::slots` has to name
/// the same kind at each of those numbers, because it is the same fact
/// stated for every slot rather than only for the ones a call fills in.
#[test]
fn the_slot_table_names_each_parameters_kind_at_the_number_the_convention_gives_it() {
    let program = lower(&checked("fn f(a: Int, b: String) -> Int {\n  a\n}\n")).expect("it lowers");
    validate(&program).expect("it holds the VM's invariants");
    let function = &program.functions[0];
    assert_eq!(
        function.params,
        vec![SlotKind::Scalar(Scalar::Int), SlotKind::Value]
    );
    // `a` is the first scalar parameter, so it takes scalar slot 0.
    assert_eq!(function.slots[0], SlotKind::Scalar(Scalar::Int));
    // `b` is the first value parameter, so it takes the value region's
    // first number, which is `value_origin()` and not 0 — the scalar
    // region comes first in the one numbering.
    assert_eq!(
        function.slots[function.value_origin() as usize],
        SlotKind::Value
    );
}

/// Two sibling blocks reuse the same scalar number the way
/// `sibling_blocks_reuse_the_slots_the_first_released` does, but the second
/// one declares a `Bool` where the first declared an `Int`. `Function::slots`
/// cannot name two kinds for one number, so `Body::allocate` skips the
/// mismatched number forward and the scalar frame ends up one slot wider
/// than the two `Int` version would have needed — see `Body::allocate` for
/// why a mismatch costs at most one slot.
#[test]
fn a_scalar_number_reused_for_an_int_and_then_a_bool_widens_the_frame_by_one() {
    let source = "fn f() -> Int {\n  {\n    let a = 1\n    a\n  }\n  {\n    let b = true\n    \
                  if b {\n      1\n    } else {\n      0\n    }\n  }\n}\n";
    let program = lower(&checked(source)).expect("it lowers");
    validate(&program).expect("it holds the VM's invariants");
    let function = &program.functions[0];
    // One `Int` slot for `a` and, since `b` could not share it, one more
    // slot for `b` — two rather than the one slot two same-kind sibling
    // blocks would have shared.
    assert_eq!(function.scalar_frame_size, 2);
    assert_eq!(function.slots.len(), 2);
    assert_eq!(function.slots[0], SlotKind::Scalar(Scalar::Int));
    assert_eq!(function.slots[1], SlotKind::Scalar(Scalar::Bool));
}

/// `slot_count` answers `slots.len()` now rather than the three frame sizes
/// added up directly, and `validate` is what holds the two equal — so this
/// checks the equality holds over every function a real package lowers, not
/// only over the hand-picked cases above.
#[test]
fn slot_count_is_the_three_frame_sizes_summed_over_the_examples_package() {
    let program = lower(&examples()).expect("the examples package lowers");
    validate(&program).expect("it holds the VM's invariants");
    for function in &program.functions {
        assert_eq!(
            function.slot_count(),
            function.scalar_frame_size + function.value_frame_size + function.place_frame_size,
            "{}.{} disagrees with itself about its own frame width",
            function.module,
            function.name
        );
    }
}
