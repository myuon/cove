use super::*;

use cove_sema::typeck::{Ty, Unknown};

use crate::lower::convention::slot_kind_of;
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

/// A parameter's slot is its declaration index, whichever kind it is —
/// that is `Function::params` read as a convention, `slots[0..arity]` is
/// `params` exactly. `Function::slots` has to name the same kind at each of
/// those numbers, because it is the same fact stated for every slot rather
/// than only for the ones a call fills in.
#[test]
fn the_slot_table_names_each_parameters_kind_at_the_number_the_convention_gives_it() {
    let program = lower(&checked("fn f(a: Int, b: String) -> Int {\n  a\n}\n")).expect("it lowers");
    validate(&program).expect("it holds the VM's invariants");
    let function = &program.functions[0];
    assert_eq!(
        function.params,
        vec![
            SlotKind::Scalar(Scalar::Int),
            SlotKind::Value(ValueKind::Str)
        ]
    );
    // `a` is declared first, so it takes slot 0; `b` is declared second, so
    // it takes slot 1 — regardless of which stack each lives on. `b`'s own
    // kind names `ValueKind::Str`, not merely `SlotKind::Value`: `String` is
    // the one refinement this table carries today, so the checker's answer
    // for `b`'s declared type survives into the layout rather than being
    // thrown away at the value/scalar split the way it was before.
    assert_eq!(function.slots[0], SlotKind::Scalar(Scalar::Int));
    assert_eq!(function.slots[1], SlotKind::Value(ValueKind::Str));
}

/// The motivating case: a parameter's slot is where its argument physically
/// arrives, not a number grouped by kind.
///
/// A call to `f` pushes `a` on the scalar stack, `b` on the value stack, and
/// `c` back on the scalar stack, in that order — declaration order, the only
/// order a caller can push arguments in. So the one numbering puts them at
/// slots 0, 1, and 2 respectively: `slots[0..arity]` is `params` exactly,
/// whatever kind each parameter is, rather than the two `Int`s sharing the
/// first two numbers the way a numbering grouped by region alone would put
/// them.
#[test]
fn mixed_kind_parameters_take_slots_in_declaration_order() {
    let program = lower(&checked(
        "fn f(a: Int, b: String, c: Int) -> Int {\n  a + c\n}\n",
    ))
    .expect("it lowers");
    validate(&program).expect("it holds the VM's invariants");
    let function = &program.functions[0];
    assert_eq!(
        function.params,
        vec![
            SlotKind::Scalar(Scalar::Int),
            SlotKind::Value(ValueKind::Str),
            SlotKind::Scalar(Scalar::Int),
        ]
    );
    assert_eq!(&function.slots[0..3], function.params.as_slice());
}

/// A capture carries its own slot, read straight out of the lowering rather
/// than counted from `n`'s — so the body's own reads of `by` name exactly
/// that number.
///
/// `n`, the closure's one parameter, is this specialisation's whole arity
/// block and takes slot 0; `by` is not a parameter of it, so it falls right
/// after, at the scalar region's first non-parameter slot, 1.
#[test]
fn a_closures_capture_lands_at_the_slot_it_records() {
    let source = "fn adder(by: Int) -> fn(Int) -> Int {\n  fn(n: Int) {\n    n + by\n  }\n}\n";
    let program = lower(&checked(source)).expect("it lowers");
    validate(&program).expect("it holds the VM's invariants");
    let closure = program
        .functions
        .iter()
        .find(|function| function.name.starts_with("<closure"))
        .expect("the lambda lowers to a function of its own");
    assert_eq!(closure.captures.len(), 1);
    let capture = &closure.captures[0];
    assert_eq!(&*capture.name, "by");
    assert_eq!(capture.kind, SlotKind::Scalar(Scalar::Int));
    assert_eq!(capture.slot, 1);
    assert!(
        closure.code.contains(&Inst::LoadScalar(capture.slot)),
        "the body reads `by` at the slot its capture records: {:?}",
        closure.code
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

// -------------------------------------------------- what a value slot says

/// A settled `String` binding's slot names [`ValueKind::Str`] — the
/// refinement `Body::slot_kind` and every other caller of `slot_kind_of`
/// read off the checker's own settlement, which is the one place this
/// information can come from: nothing later re-derives it.
#[test]
fn a_settled_string_binding_gets_the_refined_value_kind() {
    assert_eq!(slot_kind_of(&Ty::Str), SlotKind::Value(ValueKind::Str));
}

/// An unsettled binding — the checker declining, `Ty::Unknown` — keeps the
/// representation every slot had before `ValueKind` existed:
/// `SlotKind::Value(ValueKind::Unknown)`, not a refusal and not a guess.
/// `Ty::Unknown(Unknown::Recovery)` stands for any of `Unknown`'s cases
/// here, because `slot_kind_of` does not look inside one — an abstention is
/// an abstention whichever reason produced it.
#[test]
fn an_unsettled_binding_does_not_get_the_refined_value_kind() {
    assert_eq!(
        slot_kind_of(&Ty::Unknown(Unknown::Recovery)),
        SlotKind::Value(ValueKind::Unknown)
    );
}

/// A declared struct's own type is not further refined either.
/// [`ValueKind::Str`] is the one case this backend distinguishes today, and
/// everything else a non-scalar type can be — settled or not — folds into
/// the same [`ValueKind::Unknown`] an abstention gets, exactly as
/// `crate::lib`'s own doc comment on `ValueKind` argues: nothing downstream
/// reads a `StructId` or an `EnumId` out of this refinement yet, so
/// `slot_kind_of` does not carry one.
#[test]
fn a_declared_structs_type_does_not_get_the_refined_value_kind() {
    assert_eq!(
        slot_kind_of(&Ty::Struct("m.Cell".into(), Vec::new())),
        SlotKind::Value(ValueKind::Unknown)
    );
}

/// A closure's capture carries the same refinement its binding's slot does
/// — `Body::make_closure`'s capture-kind arm forwards the binding's own
/// `ValueKind` rather than widening it back to `Unknown` — so a captured
/// `String` is provably one inside the closure's own body, the same as a
/// parameter of that type would be.
#[test]
fn a_closures_string_capture_carries_the_refined_value_kind() {
    let source = "fn labelling(label: String) -> fn(Int) -> String {\n  fn(n: Int) {\n    \"{label}: {n}\"\n  }\n}\n";
    let program = lower(&checked(source)).expect("it lowers");
    validate(&program).expect("it holds the VM's invariants");
    let closure = program
        .functions
        .iter()
        .find(|function| function.name.starts_with("<closure"))
        .expect("the lambda lowers to a function of its own");
    assert_eq!(closure.captures.len(), 1);
    let capture = &closure.captures[0];
    assert_eq!(&*capture.name, "label");
    assert_eq!(capture.kind, SlotKind::Value(ValueKind::Str));
    assert!(
        closure.code.contains(&Inst::LoadLocal(capture.slot)),
        "the body reads `label` at the slot its capture records: {:?}",
        closure.code
    );
}

/// The value region's own version of
/// `a_scalar_number_reused_for_an_int_and_then_a_bool_widens_the_frame_by_one`:
/// a `String` and a struct sharing a sibling number cannot, because
/// `Function::slots` cannot name two `ValueKind`s for one number any more
/// than it can name two `Scalar`s for one — `Body::allocate`'s skip rule is
/// one rule over all three regions, and this is the value region actually
/// needing it for the first time. One extra slot, once, is the price: two
/// same-`ValueKind` sibling blocks would have shared the number instead.
#[test]
fn a_value_number_reused_for_a_string_and_then_a_struct_widens_the_frame_by_one() {
    let source = "struct Cell {\n  at: Int\n}\n\nfn f() -> Int {\n  {\n    let a = \"x\"\n    \
                  a\n  }\n  {\n    let b = Cell(at: 1)\n    b.at\n  }\n}\n";
    let program = lower(&checked(source)).expect("it lowers");
    validate(&program).expect("it holds the VM's invariants");
    let function = &program.functions[0];
    assert_eq!(function.value_frame_size, 2);
    assert_eq!(function.slots.len(), 2);
    assert_eq!(function.slots[0], SlotKind::Value(ValueKind::Str));
    assert_eq!(function.slots[1], SlotKind::Value(ValueKind::Unknown));
}
