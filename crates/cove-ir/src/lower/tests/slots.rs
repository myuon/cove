use super::*;

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
