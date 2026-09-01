//! Enums: one word of case index, then the payload of the case it is in.

use std::sync::Arc;

use super::{checked, listing};
use crate::layout::{Case, Shape};
use crate::lower::lower;
use crate::repr::Repr;

#[test]
fn a_case_with_no_payload_is_an_index_and_nothing_else() {
    assert_eq!(
        listing(
            "enum Verdict { Keep, Drop }\nfn keep() -> Verdict { Verdict.Keep }",
            "keep"
        ),
        "\
fn0 m.keep(0) -> ref
  frame 3: s0:ref s1:ref s2:int
     0  alloc s1:ref m.Verdict<enum>
     1  int s2:int 0
     2  set-word s1:ref +0 s2:int
     3  move s0:ref s1:ref
     4  clear s1:ref
     5  return s0:ref
"
    );
}

#[test]
fn a_case_with_a_payload_writes_the_index_then_the_words() {
    // The payload is evaluated before the object exists, so nothing is
    // half-built across the expression that fills it; the index goes in
    // first because it is what the collector reads to decide which of the
    // remaining words are references.
    assert_eq!(
        listing(
            "enum Shape { Circle(Int), Square(Int, Int) }\n\
             fn unit_square() -> Shape { Shape.Square(1, 1) }",
            "unit_square"
        ),
        "\
fn0 m.unit_square(0) -> ref
  frame 5: s0:ref s1:int s2:int s3:ref s4:int
     0  int s1:int 1
     1  int s2:int 1
     2  alloc s3:ref m.Shape<enum>
     3  int s4:int 1
     4  set-word s3:ref +0 s4:int
     5  set-word s3:ref +1 s1:int
     6  set-word s3:ref +2 s2:int
     7  move s0:ref s3:ref
     8  clear s3:ref
     9  return s0:ref
"
    );
}

#[test]
fn option_and_result_are_enums_with_the_cases_the_design_fixes() {
    // `docs/LINEAR_VM.md` fixes the order: `None` then `Some`, `Ok` then
    // `Err`. It is a number in the heap, so it is written down where a
    // reader can check it rather than derived from a table that might be
    // reordered.
    let program = lower(&checked(
        "fn some() -> Option<Int> { Some(1) }\nfn ok() -> Result<Int, String> { Ok(1) }",
    ))
    .expect("the program lowers");
    let shape = |name: &str| {
        program
            .layouts
            .iter()
            .find(|layout| &*layout.name == name)
            .map(|layout| layout.shape.clone())
            .unwrap_or_else(|| panic!("`{name}` was declared"))
    };
    assert_eq!(
        shape("Option"),
        Shape::Enum {
            cases: vec![
                Case {
                    name: Arc::from("None"),
                    payload: vec![],
                },
                Case {
                    name: Arc::from("Some"),
                    payload: vec![Repr::Int],
                },
            ],
        }
    );
    assert_eq!(
        shape("Result"),
        Shape::Enum {
            cases: vec![
                Case {
                    name: Arc::from("Ok"),
                    payload: vec![Repr::Int],
                },
                Case {
                    name: Arc::from("Err"),
                    payload: vec![Repr::Ref],
                },
            ],
        }
    );
}

#[test]
fn one_layout_per_payload_word_rather_than_per_instantiation() {
    // A layout describes a family: `Option<String>` and `Option<Point>` are
    // one, because a reference is a reference and what an element actually
    // is is a question its own object answers. `Option<Int>` is another,
    // because the word is not a reference and the boundary has to know.
    let program = lower(&checked(
        "struct Point { x: Int, y: Int }\n\
         fn a() -> Option<String> { Some(\"s\") }\n\
         fn b() -> Option<Point> { Some(Point(x: 0, y: 0)) }\n\
         fn c() -> Option<Int> { Some(1) }\n\
         fn d() -> Option<Duration> { Some(1s) }",
    ))
    .expect("the program lowers");
    let options: Vec<&Shape> = program
        .layouts
        .iter()
        .filter(|layout| &*layout.name == "Option")
        .map(|layout| &layout.shape)
        .collect();
    let payloads: Vec<Vec<Repr>> = options
        .iter()
        .map(|shape| match shape {
            Shape::Enum { cases } => cases[1].payload.clone(),
            other => panic!("an `Option` is an enum, not {other:?}"),
        })
        .collect();
    assert_eq!(
        payloads,
        vec![vec![Repr::Ref], vec![Repr::Int], vec![Repr::Duration]]
    );
}

#[test]
fn an_error_is_a_struct_the_language_declares() {
    assert_eq!(
        listing(
            "fn boom() -> Result<Int, Error> { Err(Error(\"boom\")) }",
            "boom"
        ),
        "\
fn0 m.boom(0) -> ref
  frame 4: s0:ref s1:ref s2:ref s3:int
     0  str s1:ref \"boom\"
     1  alloc s2:ref Error<struct>
     2  set-word s2:ref +0 s1:ref
     3  clear s1:ref
     4  alloc s1:ref Result<enum>
     5  int s3:int 1
     6  set-word s1:ref +0 s3:int
     7  set-word s1:ref +1 s2:ref
     8  clear s2:ref
     9  move s0:ref s1:ref
    10  clear s1:ref
    11  return s0:ref
"
    );
}

#[test]
fn a_question_mark_reads_the_case_and_leaves_with_a_fresh_failure() {
    // The failing side builds this function's own `Err` rather than passing
    // the one it found along: the value `?` was applied to is a `Result` of
    // some other pair of words, and two `Result`s whose words differ are two
    // layouts. Handing the caller the object as it stands would give it one
    // whose header names the wrong one.
    //
    // Nothing is cleared on that side, and nothing needs to be: the frame
    // ends at the `Return`, so a slot that still holds a reference retains
    // it for no instructions at all.
    assert_eq!(
        listing(
            "fn add_one(x: Result<Int, String>) -> Result<Int, String> {\n  let n = x?\n  Ok(n + 1)\n}",
            "add_one"
        ),
        "\
fn0 m.add_one(1) -> ref
  frame 8: s0!:ref s1:ref s2:int s3:int s4:bool s5:ref s6:ref s7:int
     0  get-word s2:int s0:ref +0
     1  int s3:int 0
     2  eq.int s4:bool s2:int s3:int
     3  branch-false s4:bool 6
     4  get-word s2:int s0:ref +1
     5  jump 12
     6  get-word s5:ref s0:ref +1
     7  alloc s6:ref Result<enum>
     8  int s3:int 1
     9  set-word s6:ref +0 s3:int
    10  set-word s6:ref +1 s5:ref
    11  return s6:ref
    12  int s3:int 1
    13  add.int s7:int s2:int s3:int
    14  alloc s5:ref Result<enum>
    15  int s3:int 0
    16  set-word s5:ref +0 s3:int
    17  set-word s5:ref +1 s7:int
    18  move s1:ref s5:ref
    19  clear s5:ref
    20  return s1:ref
"
    );
}

#[test]
fn a_question_mark_on_an_option_leaves_with_a_fresh_none() {
    // `Some` is case 1 of an `Option` where `Ok` is case 0 of a `Result`, so
    // the index being tested is looked up rather than written down. The
    // failure carries no payload, so the object is an index and nothing
    // else.
    assert_eq!(
        listing(
            "fn add_one(x: Option<Int>) -> Option<Int> {\n  let n = x?\n  Some(n + 1)\n}",
            "add_one"
        ),
        "\
fn0 m.add_one(1) -> ref
  frame 7: s0!:ref s1:ref s2:int s3:int s4:bool s5:ref s6:int
     0  get-word s2:int s0:ref +0
     1  int s3:int 1
     2  eq.int s4:bool s2:int s3:int
     3  branch-false s4:bool 6
     4  get-word s2:int s0:ref +1
     5  jump 10
     6  alloc s5:ref Option<enum>
     7  int s3:int 0
     8  set-word s5:ref +0 s3:int
     9  return s5:ref
    10  int s3:int 1
    11  add.int s6:int s2:int s3:int
    12  alloc s5:ref Option<enum>
    13  int s3:int 1
    14  set-word s5:ref +0 s3:int
    15  set-word s5:ref +1 s6:int
    16  move s1:ref s5:ref
    17  clear s5:ref
    18  return s1:ref
"
    );
}

#[test]
fn a_question_mark_on_a_temporary_clears_it_on_the_way_through() {
    // The value `?` read is dead once its payload has been taken out of it,
    // and it is a temporary rather than a binding, so the clear is at that
    // last use rather than at the end of a scope.
    assert_eq!(
        listing(
            "fn one() -> Option<Int> { Some(1) }\n\
             fn twice() -> Option<Int> {\n  let n = one()?\n  Some(n + n)\n}",
            "twice"
        ),
        "\
fn1 m.twice(0) -> ref
  frame 7: s0:ref s1:ref s2:int s3:int s4:bool s5:ref s6:int
     0  call s1:ref m.one ()
     1  get-word s2:int s1:ref +0
     2  int s3:int 1
     3  eq.int s4:bool s2:int s3:int
     4  branch-false s4:bool 7
     5  get-word s2:int s1:ref +1
     6  jump 11
     7  alloc s5:ref Option<enum>
     8  int s3:int 0
     9  set-word s5:ref +0 s3:int
    10  return s5:ref
    11  clear s1:ref
    12  add.int s3:int s2:int s2:int
    13  alloc s1:ref Option<enum>
    14  int s6:int 1
    15  set-word s1:ref +0 s6:int
    16  set-word s1:ref +1 s3:int
    17  move s0:ref s1:ref
    18  clear s1:ref
    19  return s0:ref
"
    );
}
