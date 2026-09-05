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
     5  call s6:ref m.take (s5:Any) String
     6  clear s5:ref Any
     7  copy s0:ref s6:ref String
     8  return s0:ref String
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
fn0 m.take(Any) -> String
  frame 7: s0!:ref s1:ref s2:int s3:ref s4:ref s5:int s6:int
  local v -> s0:Any [0, 12)
     0  load-field s2:int s0:ref +0 Int
     1  switch s2:int [9 9 9 9 9 9 9 9 9 9 9 9 9 2 6] else 9
     2  unbox s4:ref s0:ref m.Name
     3  call s3:ref m.Name.show (s4:m.Name) String
     4  clear s4:ref m.Name
     5  jump 10
     6  unbox s5:int s0:ref m.Point
     7  call s3:ref m.Point.show (s5:m.Point) String
     8  jump 10
     9  trap \"no implementation of `Show.show` for this value\"
    10  copy s1:ref s3:ref String
    11  return s1:ref String
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
fn0 m.mk() -> Any
  frame 6: s0:ref s1:int s2:int s3:int s4:int s5:ref
     0  int s1:int 1
     1  int s2:int 2
     2  copy s3:int s1:int Int
     3  copy s4:int s2:int Int
     4  box s5:ref s3:int m.Point
     5  copy s0:ref s5:ref Any
     6  return s0:ref Any
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
  local h -> s2:m.Holder [5, 16)
     0  str s1:ref \"n\"
     1  copy s2:ref s1:ref String
     2  box s1:ref s2:ref m.Name
     3  copy s2:ref s1:ref Any
     4  clear s1:ref Any
     5  load-field s3:int s2:ref +0 Int
     6  switch s3:int [14 14 14 14 14 14 14 14 14 14 14 14 14 7 11] else 14
     7  unbox s4:ref s2:ref m.Name
     8  call s1:ref m.Name.show (s4:m.Name) String
     9  clear s4:ref m.Name
    10  jump 15
    11  unbox s5:int s2:ref m.Point
    12  call s1:ref m.Point.show (s5:m.Point) String
    13  jump 15
    14  trap \"no implementation of `Show.show` for this value\"
    15  copy s0:ref s1:ref String
    16  return s0:ref String
"
    );
}

/// A trait's default body is one function per conforming type, and it is
/// the monomorphisation machinery that makes it one.
///
/// The checker walks the body **once**, with `self` typed as a rigid
/// `Ty::Param("Self")` bounded by the trait, and `resolve::conform`
/// synthesises one `FnDecl` per conforming type carrying the trait method's
/// own span — so every fact in the body is recorded in terms of `Self` and
/// the substitution `Self := m.Receipt` completes it. That is exactly what a
/// generic declaration's `Ty::Param` needs, one parameter instead of
/// several.
///
/// `self.summarize()` inside it costs one ordinary [`crate::Inst::Call`]:
/// the checker recorded no target — which implementation it reaches is
/// decided by the type — and this body is lowered for *one* type, so the
/// bounded-call path finds `m.Receipt.summarize` statically. No dictionary,
/// no vtable.
#[test]
fn a_trait_method_s_default_body_is_lowered_once_per_conforming_type() {
    let source = "/// t\nexport trait Summary {\n  /// s\n  fn summarize(self) -> String\n\n  \
                  /// l\n  fn line(self) -> String {\n    \"- {self.summarize()}\"\n  }\n}\n\
                  /// b\nexport struct Booking { id: Int }\n\
                  /// r\nexport struct Receipt { cents: Int }\n\
                  impl Summary for Booking {\n  /// s\n  fn summarize(self) -> String { \"booking {self.id}\" }\n}\n\
                  impl Summary for Receipt {\n  /// s\n  fn summarize(self) -> String { \"receipt\" }\n}\n\
                  /// f\nexport fn f(b: Booking, r: Receipt) -> String { \"{b.line()}{r.line()}\" }\n";
    assert_eq!(
        listing(source, "Booking.line"),
        "\
fn1 m.Booking.line(m.Booking) -> String
  frame 5: s0!:int s1:ref s2:ref s3:ref s4:ref
  local self -> s0:m.Booking [0, 6)
     0  str s2:ref \"- \"
     1  call s3:ref m.Booking.summarize (s0:m.Booking) String
     2  call-builtin s4:ref String.interpolate (s2:String s3:String) String
     3  clear s3:ref String
     4  copy s1:ref s4:ref String
     5  return s1:ref String
"
    );
    assert_eq!(
        listing(source, "Receipt.line"),
        "\
fn3 m.Receipt.line(m.Receipt) -> String
  frame 5: s0!:int s1:ref s2:ref s3:ref s4:ref
  local self -> s0:m.Receipt [0, 6)
     0  str s2:ref \"- \"
     1  call s3:ref m.Receipt.summarize (s0:m.Receipt) String
     2  call-builtin s4:ref String.interpolate (s2:String s3:String) String
     3  clear s3:ref String
     4  copy s1:ref s4:ref String
     5  return s1:ref String
"
    );
}

/// A conformance that supplies its own body keeps it, and only the ones that
/// did not get the trait's.
#[test]
fn a_conformance_that_writes_its_own_body_does_not_get_the_default() {
    assert_eq!(
        listing(
            "/// t\nexport trait Summary {\n  /// s\n  fn summarize(self) -> String\n\n  \
             /// l\n  fn line(self) -> String {\n    \"- {self.summarize()}\"\n  }\n}\n\
             /// r\nexport struct Receipt { cents: Int }\n\
             impl Summary for Receipt {\n  /// s\n  fn summarize(self) -> String { \"receipt\" }\n\n  \
             /// l\n  fn line(self) -> String { \"  $ {self.summarize()}\" }\n}\n\
             /// f\nexport fn f(r: Receipt) -> String { r.line() }\n",
            "Receipt.line"
        ),
        "\
fn1 m.Receipt.line(m.Receipt) -> String
  frame 5: s0!:int s1:ref s2:ref s3:ref s4:ref
  local self -> s0:m.Receipt [0, 6)
     0  str s2:ref \"  $ \"
     1  call s3:ref m.Receipt.summarize (s0:m.Receipt) String
     2  call-builtin s4:ref String.interpolate (s2:String s3:String) String
     3  clear s3:ref String
     4  copy s1:ref s4:ref String
     5  return s1:ref String
"
    );
}
