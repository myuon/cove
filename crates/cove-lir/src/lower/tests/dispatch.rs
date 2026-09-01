//! `impl`, traits, and dispatch through a `dyn` value.

use super::listing;

/// This is the language's one implicit conversion, and it happens where a
/// type is *written*: a parameter, a declared return type, a struct field,
/// an enum payload, an annotated `let`.
#[test]
fn erasure_boxes_a_concrete_value_where_a_dyn_type_is_written() {
    assert_eq!(
        listing(
            "trait Show { fn show(self) -> String }\nstruct Point { x: Int, y: Int }\nimpl Show for Point { fn show(self) -> String { \"point\" } }\nstruct Name { text: String }\nimpl Show for Name { fn show(self) -> String { self.text } }\nfn take(v: dyn Show) -> String { v.show() }\nfn f() -> String { take(Point(x: 1, y: 2)) }",
            "f"
        ),
        "\
fn0 m.f() -> String
  frame 7: s0:ref s1:int s2:int s3:int s4:int s5:ref s6:ref
     0  int s1:int 1
     1  int s2:int 2
     2  copy s3:int s1:int Int
     3  copy s4:int s2:int Int
     4  box s5:ref s3:int m.Point
     5  call s6:ref m.take (s5:ref)
     6  clear s5:ref Dyn
     7  copy s0:ref s6:ref String
     8  clear s6:ref String
     9  return s0:ref
"
    );
}

/// There is no dispatch instruction and no vtable. A `dyn` value is one
/// reference to a box, and the box's first payload word is the `LayoutId`
/// of what it holds — so the call site reads that word and hands it to an
/// ordinary `Switch` over a table built from the trait's conformances.
//
/// Each arm opens the box into a receiver of *its own* concrete layout,
/// because that is what varies between them: a `Point` is two words and a
/// `Name` is one, and the callee's first parameter is whichever the arm is
/// for.
#[test]
fn a_dyn_call_switches_on_the_layout_the_box_records() {
    assert_eq!(
        listing(
            "trait Show { fn show(self) -> String }\nstruct Point { x: Int, y: Int }\nimpl Show for Point { fn show(self) -> String { \"point\" } }\nstruct Name { text: String }\nimpl Show for Name { fn show(self) -> String { self.text } }\nfn take(v: dyn Show) -> String { v.show() }",
            "take"
        ),
        "\
fn0 m.take(Dyn) -> String
  frame 7: s0!:ref s1:ref s2:int s3:ref s4:ref s5:int s6:int
     0  load-field s2:int s0:ref +0 Int
     1  switch s2:int [9 9 9 9 9 9 9 9 9 9 9 2 6] else 9
     2  unbox s4:ref s0:ref m.Name
     3  call s3:ref m.Name.show (s4:ref)
     4  clear s4:ref m.Name
     5  jump 10
     6  unbox s5:int s0:ref m.Point
     7  call s3:ref m.Point.show (s5:int)
     8  jump 10
     9  trap \"no implementation of `Show.show` for this value\"
    10  copy s1:ref s3:ref String
    11  clear s3:ref String
    12  return s1:ref
"
    );
}

/// A declared return type is a written type, so the answer location is
/// the box and the value is put in one on its way there.
#[test]
fn a_body_declared_dyn_erases_its_tail() {
    assert_eq!(
        listing(
            "trait Show { fn show(self) -> String }\nstruct Point { x: Int, y: Int }\nimpl Show for Point { fn show(self) -> String { \"point\" } }\nstruct Name { text: String }\nimpl Show for Name { fn show(self) -> String { self.text } }\nfn mk() -> dyn Show { Point(x: 1, y: 2) }",
            "mk"
        ),
        "\
fn0 m.mk() -> Dyn
  frame 6: s0:ref s1:int s2:int s3:int s4:int s5:ref
     0  int s1:int 1
     1  int s2:int 2
     2  copy s3:int s1:int Int
     3  copy s4:int s2:int Int
     4  box s5:ref s3:int m.Point
     5  copy s0:ref s5:ref Dyn
     6  clear s5:ref Dyn
     7  return s0:ref
"
    );
}

#[test]
fn a_dyn_struct_field_holds_the_box() {
    assert_eq!(
        listing(
            "trait Show { fn show(self) -> String }\nstruct Point { x: Int, y: Int }\nimpl Show for Point { fn show(self) -> String { \"point\" } }\nstruct Name { text: String }\nimpl Show for Name { fn show(self) -> String { self.text } }\nstruct Holder { it: dyn Show }\nfn f() -> String {\n  let h = Holder(it: Name(text: \"n\"))\n  h.it.show()\n}",
            "f"
        ),
        "\
fn0 m.f() -> String
  frame 7: s0:ref s1:ref s2:ref s3:int s4:ref s5:int s6:int
     0  str s1:ref \"n\"
     1  copy s2:ref s1:ref String
     2  clear s1:ref String
     3  box s1:ref s2:ref m.Name
     4  clear s2:ref m.Name
     5  copy s2:ref s1:ref Dyn
     6  clear s1:ref Dyn
     7  load-field s3:int s2:ref +0 Int
     8  switch s3:int [16 16 16 16 16 16 16 16 16 16 9 13] else 16
     9  unbox s4:ref s2:ref m.Name
    10  call s1:ref m.Name.show (s4:ref)
    11  clear s4:ref m.Name
    12  jump 17
    13  unbox s5:int s2:ref m.Point
    14  call s1:ref m.Point.show (s5:int)
    15  jump 17
    16  trap \"no implementation of `Show.show` for this value\"
    17  copy s0:ref s1:ref String
    18  clear s1:ref String
    19  clear s2:ref m.Holder
    20  return s0:ref
"
    );
}
