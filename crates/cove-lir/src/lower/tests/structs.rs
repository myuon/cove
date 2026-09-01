//! Structs: a layout per declaration, and one word per field.

use std::sync::Arc;

use super::{checked, listing};
use crate::layout::{Field, Shape};
use crate::lower::lower;
use crate::repr::Repr;

#[test]
fn a_struct_literal_allocates_and_then_fills() {
    // Every field is evaluated before anything is stored, because an
    // initializer's arguments are ordinary expressions and one of them may
    // do something the next one sees. The object is allocated once they are
    // all in hand, so nothing is half-built across a call.
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn origin() -> Point { Point(x: 1, y: 2) }",
            "origin"
        ),
        "\
fn0 m.origin(0) -> ref
  frame 4: s0:ref s1:int s2:int s3:ref
     0  int s1:int 1
     1  int s2:int 2
     2  alloc s3:ref m.Point<struct>
     3  set-word s3:ref +0 s1:int
     4  set-word s3:ref +1 s2:int
     5  move s0:ref s3:ref
     6  clear s3:ref
     7  return s0:ref
"
    );
}

#[test]
fn a_field_read_is_one_word_out_of_the_object() {
    // The field a name denotes is decided from the layout, statically, so
    // there is nothing to look up at run time; and a field holding a
    // reference is cleared once the value built out of it is finished with.
    assert_eq!(
        listing(
            "struct User { name: String, age: Int }\n\
             fn older(u: User) -> User { User(name: u.name, age: u.age + 1) }",
            "older"
        ),
        "\
fn0 m.older(1) -> ref
  frame 7: s0!:ref s1:ref s2:ref s3:int s4:int s5:int s6:ref
     0  get-word s2:ref s0:ref +0
     1  get-word s3:int s0:ref +1
     2  int s4:int 1
     3  add.int s5:int s3:int s4:int
     4  alloc s6:ref m.User<struct>
     5  set-word s6:ref +0 s2:ref
     6  set-word s6:ref +1 s5:int
     7  clear s2:ref
     8  move s1:ref s6:ref
     9  clear s6:ref
    10  return s1:ref
"
    );
}

#[test]
fn a_field_assignment_is_one_word_in() {
    // A `var` parameter names the caller's storage, so reading `p` is a
    // `Load` through the address and writing a field of it is a `SetWord`
    // into the object that address names. The object is held in a reference
    // slot for the read and the write, and cleared as soon as the store is
    // done — not for the rest of the frame.
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\nfn move_x(var p: Point, by: Int) { p.x += by }",
            "move_x"
        ),
        "\
fn0 m.move_x(2) -> unit
  frame 7: s0!:addr s1!:int s2:unit s3:ref s4:int s5:int s6:unit
     0  load s3:ref s0:addr
     1  get-word s4:int s3:ref +0
     2  add.int s5:int s4:int s1:int
     3  set-word s3:ref +0 s5:int
     4  clear s3:ref
     5  unit s6:unit
     6  move s2:unit s6:unit
     7  return s2:unit
"
    );
}

#[test]
fn an_interior_address_holds_its_base_for_exactly_its_own_live_range() {
    // This is the case `docs/LINEAR_VM.md` singles out: the object an
    // interior address points into is kept alive by the lowering rather than
    // by the collector, which needs no interior-pointer logic and never
    // moves an object.
    //
    // So `s2` holds the object for as long as the address in `s3` can be
    // used — the `AddrOfWord`, the call, and nothing after it — and both are
    // cleared together the instant the call returns. Retaining the base for
    // the rest of the frame instead would hold it across everything a
    // long-running body does afterwards.
    assert_eq!(
        listing(
            "struct Point { x: Int, y: Int }\n\
             fn bump(var n: Int) { n += 1 }\n\
             fn go(var p: Point) -> Int {\n  bump(var p.x)\n  p.x\n}",
            "go"
        ),
        "\
fn1 m.go(1) -> int
  frame 6: s0!:addr s1:int s2:ref s3:addr s4:unit s5:int
     0  load s2:ref s0:addr
     1  addr-of-word s3:addr s2:ref +0
     2  call s4:unit m.bump (s3:addr)
     3  clear s3:addr
     4  clear s2:ref
     5  load s2:ref s0:addr
     6  get-word s5:int s2:ref +0
     7  clear s2:ref
     8  move s1:int s5:int
     9  return s1:int
"
    );
}

#[test]
fn a_declared_struct_is_one_layout_with_its_fields_in_declaration_order() {
    // A declaration is a shape rather than a function, so it produces no
    // code and one entry in the layout table. The name is the type's own,
    // without the module that declares it, because that is what a value of
    // it is shown as and what an incoming value is matched to a family by.
    let program = lower(&checked(
        "struct Point { x: Int, y: Int }\n\
         fn a() -> Point { Point(x: 1, y: 2) }\n\
         fn b() -> Point { Point(x: 3, y: 4) }",
    ))
    .expect("the program lowers");
    let points: Vec<&crate::Layout> = program
        .layouts
        .iter()
        .filter(|layout| &*layout.name == "m.Point")
        .collect();
    assert_eq!(points.len(), 1, "two literals of one struct are one layout");
    assert_eq!(
        points[0].shape,
        Shape::Struct {
            fields: vec![
                Field {
                    name: Arc::from("x"),
                    repr: Repr::Int,
                },
                Field {
                    name: Arc::from("y"),
                    repr: Repr::Int,
                },
            ],
            opaque: false,
        }
    );
    // A struct of scalars is a leaf, so a collection walking one reads no
    // word of it at all.
    assert!(!points[0].may_hold_refs());
}

/// An `export opaque struct` is the same shape as any other; what differs is
/// the one thing a rendering is allowed to say about it. The layout carries
/// that because nothing downstream can derive it: by the time a value is a
/// word, the declaration is gone.
#[test]
fn an_opaque_declaration_is_recorded_in_its_layout() {
    let source = "
export opaque struct Token { id: Int }
export fn make(n: Int) -> Token {
  Token(id: n)
}
";
    let program = lower(&checked(source)).expect("the program lowers");
    let token = program
        .layouts
        .iter()
        .find(|layout| &*layout.name == "m.Token")
        .expect("the struct has a layout");
    assert!(matches!(token.shape, Shape::Struct { opaque: true, .. }));
}
