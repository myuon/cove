//! Methods, traits, and how a call finds the body it runs.
//!
//! Two shapes are pinned here and they are the point of the module: a call
//! the checker resolved is one [`crate::Inst::Call`] and costs nothing, and a
//! call through a `dyn Trait` is the object being asked what it is.

use super::{checked, listing, refused};
use crate::layout::Shape;
use crate::lower::lower;

// ---- a method is an ordinary function -----------------------------------

#[test]
fn a_method_is_a_function_whose_slot_0_is_the_receiver() {
    // Nothing about a method needs a second calling convention: a call
    // supplies the receiver first, so it is slot 0 and the written
    // parameters follow. `self` is read the way any binding is read.
    let source = "struct Point { x: Int, y: Int }\n\
                  impl Point {\n\
                    fn scaled(self, by: Int) -> Point { Point(x: self.x * by, y: self.y * by) }\n\
                  }\n\
                  fn twice(p: Point) -> Point { p.scaled(by: 2) }";
    assert_eq!(
        listing(source, "Point.scaled"),
        "\
fn1 m.Point.scaled(2) -> ref
  frame 7: s0!:ref s1!:int s2:ref s3:int s4:int s5:int s6:ref
     0  get-word s3:int s0:ref +0
     1  mul.int s4:int s3:int s1:int
     2  get-word s3:int s0:ref +1
     3  mul.int s5:int s3:int s1:int
     4  alloc s6:ref Point<struct>
     5  set-word s6:ref +0 s4:int
     6  set-word s6:ref +1 s5:int
     7  move s2:ref s6:ref
     8  clear s6:ref
     9  return s2:ref
"
    );
    // And the call site is a `Call` like any other. The label `by:` moves
    // nothing: the checker already refused a label out of declaration order,
    // so the arguments line up with the parameters one for one.
    assert_eq!(
        listing(source, "twice"),
        "\
fn0 m.twice(1) -> ref
  frame 4: s0!:ref s1:ref s2:int s3:ref
     0  int s2:int 2
     1  call s3:ref m.Point.scaled (s0:ref s2:int)
     2  move s1:ref s3:ref
     3  clear s3:ref
     4  return s1:ref
"
    );
}

#[test]
fn a_method_is_named_type_dot_method_in_the_module_that_writes_it() {
    // The name a diagnostic reads and the name `function_named` answers to
    // are one name. A module cannot declare a type and a free function of
    // one name, and a `.` is not a name character, so the two namings cannot
    // collide.
    let program = lower(&checked(
        "struct Point { x: Int }\n\
         impl Point {\n  fn get(self) -> Int { self.x }\n  fn zero() -> Int { 0 }\n}\n\
         fn go(p: Point) -> Int { p.get() }",
    ))
    .expect("the program lowers");
    let id = program
        .function_named("m", "Point.get")
        .expect("a method answers to its qualified name");
    assert_eq!(program.function(id).qualified(), "m.Point.get");
    assert!(program.function_named("m", "Point.zero").is_some());
    assert!(program.function_named("m", "get").is_none());
}

#[test]
fn var_self_makes_slot_0_the_address_of_the_callers_storage() {
    // `var self` is a `var` parameter written in the receiver position, so
    // it needs no rule of its own: slot 0 holds an address, reading `self`
    // is a `Load` through it, and a write to a field of `self` reaches the
    // caller's object with no copy back.
    let source = "struct Tally { n: Int }\n\
                  impl Tally {\n  fn bump(var self) { self.n = self.n + 1 }\n}\n\
                  fn go(var t: Tally) { t.bump() }";
    assert_eq!(
        listing(source, "Tally.bump"),
        "\
fn1 m.Tally.bump(1) -> unit
  frame 8: s0!:addr s1:unit s2:ref s3:ref s4:int s5:int s6:int s7:unit
     0  load s2:ref s0:addr
     1  load s3:ref s0:addr
     2  get-word s4:int s3:ref +0
     3  clear s3:ref
     4  int s5:int 1
     5  add.int s6:int s4:int s5:int
     6  set-word s2:ref +0 s6:int
     7  clear s2:ref
     8  unit s7:unit
     9  move s1:unit s7:unit
    10  return s1:unit
"
    );
    // A caller that is itself holding an address passes the same one on,
    // which is what makes the alias reach through however many frames pass
    // it along.
    assert_eq!(
        listing(source, "go"),
        "\
fn0 m.go(1) -> unit
  frame 3: s0!:addr s1:unit s2:unit
     0  call s2:unit m.Tally.bump (s0:addr)
     1  move s1:unit s2:unit
     2  return s1:unit
"
    );
}

#[test]
fn an_associated_function_takes_no_receiver_and_evaluates_no_base() {
    // `Point.origin()` is written through a name that is not a value, so
    // there is nothing to evaluate in front of the `.`: the checker recorded
    // which declaration it names and the arity is the written parameters
    // alone.
    assert_eq!(
        listing(
            "struct Point { x: Int }\n\
             impl Point {\n  fn origin() -> Point { Point(x: 0) }\n}\n\
             fn go() -> Int { Point.origin().x }",
            "go"
        ),
        "\
fn0 m.go(0) -> int
  frame 3: s0:int s1:ref s2:int
     0  call s1:ref m.Point.origin ()
     1  get-word s2:int s1:ref +0
     2  clear s1:ref
     3  move s0:int s2:int
     4  return s0:int
"
    );
}

#[test]
fn a_conformance_method_is_an_ordinary_call() {
    // `impl Snapshot for Booking` supplies a method of `Booking` like any
    // other, and `b.snapshot()` resolves to it while the program is being
    // checked. Nothing here knows the trait was `Snapshot`: a conformance
    // that calls a declared method is an ordinary call.
    assert_eq!(
        listing(
            "struct Booking { id: Int }\n\
             impl Snapshot for Booking {\n  fn snapshot(self) -> Booking { Booking(id: self.id) }\n}\n\
             fn copy(b: Booking) -> Booking { b.snapshot() }",
            "copy"
        ),
        "\
fn0 m.copy(1) -> ref
  frame 3: s0!:ref s1:ref s2:ref
     0  call s2:ref m.Booking.snapshot (s0:ref)
     1  move s1:ref s2:ref
     2  clear s2:ref
     3  return s1:ref
"
    );
}

// ---- erasure -------------------------------------------------------------

/// A trait, two conforming types, and a function that takes the trait
/// object. Two implementors rather than one, because a dispatch table with a
/// single entry says nothing about how the entries are chosen.
const DISPLAY: &str = "trait Display { fn describe(self) -> String }\n\
                       struct Point { x: Int }\n\
                       impl Display for Point { fn describe(self) -> String { \"point\" } }\n\
                       enum Verdict { Keep, Drop }\n\
                       impl Display for Verdict { fn describe(self) -> String { \"verdict\" } }\n";

#[test]
fn a_concrete_value_is_erased_where_a_dyn_type_is_written() {
    // A parameter is one of the four places the language's one implicit
    // conversion happens, and it is where the oracle's `coerce` runs too.
    // The box costs an allocation on a path that was never going to be fast
    // and buys one word per value everywhere else.
    let source = format!(
        "{DISPLAY}fn show(it: dyn Display) -> String {{ \"x\" }}\n\
         fn go() -> String {{ show(Point(x: 1)) }}"
    );
    assert_eq!(
        listing(&source, "go"),
        "\
fn0 m.go(0) -> ref
  frame 4: s0:ref s1:int s2:ref s3:ref
     0  int s1:int 1
     1  alloc s2:ref Point<struct>
     2  set-word s2:ref +0 s1:int
     3  box s3:ref s2:ref ref
     4  clear s2:ref
     5  call s2:ref m.show (s3:ref)
     6  clear s3:ref
     7  move s0:ref s2:ref
     8  clear s2:ref
     9  return s0:ref
"
    );
}

#[test]
fn a_struct_field_written_dyn_erases_what_is_stored_in_it() {
    let source = format!("{DISPLAY}struct Row {{ shown: dyn Display }}\nfn wrap(p: Point) -> Row {{ Row(shown: p) }}");
    assert_eq!(
        listing(&source, "wrap"),
        "\
fn0 m.wrap(1) -> ref
  frame 4: s0!:ref s1:ref s2:ref s3:ref
     0  box s2:ref s0:ref ref
     1  alloc s3:ref Row<struct>
     2  set-word s3:ref +0 s2:ref
     3  clear s2:ref
     4  move s1:ref s3:ref
     5  clear s3:ref
     6  return s1:ref
"
    );
}

#[test]
fn every_erased_value_of_a_program_shares_one_layout() {
    // A layout describes a family. What is inside a box is a question the
    // box answers, from the `Repr` tag in its own payload word 0, so a
    // layout per trait would be a runtime type universe keyed by a static
    // name.
    let source = format!(
        "{DISPLAY}trait Loud {{ fn shout(self) -> String }}\n\
         impl Loud for Point {{ fn shout(self) -> String {{ \"P\" }} }}\n\
         fn a(it: dyn Display) -> String {{ \"a\" }}\n\
         fn b(it: dyn Loud) -> String {{ \"b\" }}\n\
         fn go(p: Point) -> String {{ a(p) }}"
    );
    let program = lower(&checked(&source)).expect("the program lowers");
    let boxed: Vec<&crate::Layout> = program
        .layouts
        .iter()
        .filter(|layout| layout.shape == Shape::Boxed)
        .collect();
    assert_eq!(boxed.len(), 1, "two traits are still one boxed family");
    assert_eq!(&*boxed[0].name, "Dyn");
}

// ---- dynamic dispatch -----------------------------------------------------

#[test]
fn a_dyn_call_asks_the_object_which_implementation_it_is() {
    // The static type says only which trait the method comes from. The
    // object's header says which type it is, and the lowering knows every
    // type that conforms because ADR 0006 makes conformance explicit — so a
    // dispatch is one `LayoutOf` and one indexed jump over a table built
    // from the trait's conformances.
    //
    // The three instructions before it are the box being looked through, and
    // they are a test rather than an unconditional read because a value
    // whose static type is `dyn Trait` may never have been through an
    // erasure point. The oracle's `dyn_receiver` answers the same way.
    //
    // The default is not a formality even though the table covers every
    // declared conformance: the index came out of a heap object, and the
    // machine does not take the lowering's word for what is in it.
    let source = format!("{DISPLAY}fn show(it: dyn Display) -> String {{ it.describe() }}");
    assert_eq!(
        listing(&source, "show"),
        "\
fn0 m.show(1) -> ref
  frame 7: s0!:ref s1:ref s2:ref s3:int s4:int s5:bool s6:ref
     0  layout-of s3:int s0:ref
     1  int s4:int 2
     2  eq.int s5:bool s3:int s4:int
     3  branch-false s5:bool 6
     4  get-word s2:ref s0:ref +1
     5  jump 7
     6  move s2:ref s0:ref
     7  layout-of s3:int s2:ref
     8  switch s3:int [13 13 13 9 11] else 13
     9  call s6:ref m.Point.describe (s2:ref)
    10  jump 14
    11  call s6:ref m.Verdict.describe (s2:ref)
    12  jump 14
    13  trap \"no implementation of `Display.describe` for this value\"
    14  clear s2:ref
    15  move s1:ref s6:ref
    16  clear s6:ref
    17  return s1:ref
"
    );
}

#[test]
fn a_dispatch_declares_every_implementors_layout() {
    // Meeting a value of a type is what declares its layout, and a dispatch
    // is a place the machine has to be able to recognise every one of them —
    // including a type the rest of the program never builds.
    let source = format!("{DISPLAY}fn show(it: dyn Display) -> String {{ it.describe() }}");
    let program = lower(&checked(&source)).expect("the program lowers");
    let names: Vec<&str> = program.layouts.iter().map(|layout| &*layout.name).collect();
    assert!(names.contains(&"Point"), "{names:?}");
    assert!(names.contains(&"Verdict"), "{names:?}");
}

#[test]
fn a_dyn_receiver_and_the_object_behind_it_are_both_cleared() {
    // Two references are held across a dispatch and neither outlives it: the
    // box dies the instant the concrete object is out of it, and the object
    // dies when the answer is written. A static reference map cannot say
    // either, which is why both are in the data.
    let source = format!(
        "{DISPLAY}fn go(p: Point) -> String {{\n  let it: dyn Display = p\n  it.describe()\n}}"
    );
    assert_eq!(
        listing(&source, "go"),
        "\
fn0 m.go(1) -> ref
  frame 8: s0!:ref s1:ref s2:ref s3:ref s4:int s5:int s6:bool s7:ref
     0  box s2:ref s0:ref ref
     1  layout-of s4:int s2:ref
     2  int s5:int 2
     3  eq.int s6:bool s4:int s5:int
     4  branch-false s6:bool 7
     5  get-word s3:ref s2:ref +1
     6  jump 8
     7  move s3:ref s2:ref
     8  layout-of s4:int s3:ref
     9  switch s4:int [14 14 14 10 12] else 14
    10  call s7:ref m.Point.describe (s3:ref)
    11  jump 15
    12  call s7:ref m.Verdict.describe (s3:ref)
    13  jump 15
    14  trap \"no implementation of `Display.describe` for this value\"
    15  clear s3:ref
    16  move s1:ref s7:ref
    17  clear s7:ref
    18  clear s2:ref
    19  return s1:ref
"
    );
}

// ---- what is left ---------------------------------------------------------

#[test]
fn a_trait_declaration_is_an_interface_rather_than_code() {
    // A trait with no default body produces nothing of its own: every method
    // it declares is supplied by a conformance, and a conformance's method is
    // an ordinary method of the type.
    let program = lower(&checked(&format!(
        "{DISPLAY}fn go(p: Point) -> String {{ p.describe() }}"
    )))
    .expect("a trait declaration is not a gap");
    assert!(program.function_named("m", "Point.describe").is_some());
    assert!(program.function_named("m", "Display.describe").is_none());
}

#[test]
fn a_trait_methods_default_body_is_the_one_thing_left() {
    // The checker checks a default body once, against a rigid `Self` bounded
    // by the trait, rather than once per conformance — and records no
    // signature for it, because `record_signature` runs from `check_body`
    // and a default body does not go through one. So there is no boundary to
    // read: not the trait method's, which was never recorded, and not the
    // conforming type's, which is the same declaration under another name.
    //
    // It is reported once however many types inherit it, because the span is
    // the trait's and one place is what a reader needs.
    assert_eq!(
        refused(
            "trait Display {\n  fn describe(self) -> String\n  fn shout(self) -> String { self.describe() }\n}\n\
             struct Point { x: Int }\n\
             impl Display for Point { fn describe(self) -> String { \"point\" } }\n\
             enum Verdict { Keep }\n\
             impl Display for Verdict { fn describe(self) -> String { \"verdict\" } }\n\
             fn go(p: Point) -> String { p.shout() }"
        ),
        vec!["not yet lowered: `Display.shout`, a trait method's default body"]
    );
}

#[test]
fn a_trait_method_the_type_overrides_needs_no_default() {
    // The default body is still written, and still checked, but nothing
    // inherits it — so there is nothing left over to lower and no gap.
    let program = lower(&checked(
        "trait Display {\n  fn describe(self) -> String { \"?\" }\n}\n\
         struct Point { x: Int }\n\
         impl Display for Point { fn describe(self) -> String { \"point\" } }\n\
         fn go(p: Point) -> String { p.describe() }",
    ))
    .expect("an overridden default is not a gap");
    assert!(program.function_named("m", "Point.describe").is_some());
}

#[test]
fn a_declared_return_type_written_dyn_erases_the_body_s_answer() {
    // The answer word is taken before any temporary and is never handed to
    // anything else, so writing into it is the function's own tail and
    // nothing else — which is what lets the erasure sit on the same `Move`
    // the tail was going to be.
    let source = format!("{DISPLAY}fn wrap(p: Point) -> dyn Display {{ p }}");
    assert_eq!(
        listing(&source, "wrap"),
        "\
fn0 m.wrap(1) -> ref
  frame 2: s0!:ref s1:ref
     0  box s1:ref s0:ref ref
     1  return s1:ref
"
    );
    // And a `return` is the same place written another way.
    let source = format!("{DISPLAY}fn wrap(p: Point) -> dyn Display {{ return p }}");
    assert_eq!(
        listing(&source, "wrap"),
        "\
fn0 m.wrap(1) -> ref
  frame 4: s0!:ref s1:ref s2:ref s3:unit
     0  box s2:ref s0:ref ref
     1  return s2:ref
     2  return s1:ref
"
    );
}
