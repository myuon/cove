//! The methods of the types the language ships.
//!
//! Two things are asserted here that the other cases do not reach. One is
//! that the operands of a call are the receiver and then the arguments in
//! source order, and that the receiver's slot is cleared at the call rather
//! than left set for the rest of the frame. The other is the layout table: a
//! builtin that builds an `Option`, an `Array` or a `Result` finds the family
//! by searching it, so a family the program never otherwise mentions would be
//! a refusal at run time and the listing is not where that shows.

use super::{checked, listing, refused};
use crate::layout::Shape;
use crate::lower::lower;
use crate::repr::Repr;

#[test]
fn a_string_method_is_one_call_over_the_receiver_and_its_arguments() {
    // `length()` counts characters rather than bytes, so it is not the
    // header's length and not an `Inst::Len`. Every operation of a `String`
    // is the machine's for a reason of that kind, which is why there is no
    // arm here that reads a word out of the object instead.
    assert_eq!(
        listing(
            "fn parts(s: String) -> Array<String> { s.split(\",\") }",
            "parts"
        ),
        "\
fn0 m.parts(1) -> ref
  frame 4: s0!:ref s1:ref s2:ref s3:ref
     0  str s2:ref \",\"
     1  call-builtin s3:ref String.split (s0:ref s2:ref)
     2  clear s2:ref
     3  move s1:ref s3:ref
     4  clear s3:ref
     5  return s1:ref
"
    );
}

#[test]
fn a_receiver_is_cleared_at_the_call_that_read_it() {
    // The receiver is a reference like any other operand and dies with the
    // call: `clear s2` at pc 4 is the separator, and `clear s1` the string
    // the split was called on. Without them a body that split one string per
    // turn would hold every one of them until it returned.
    //
    // `trim()` at pc 3 reuses `s2`, because the slot went back on the free
    // list its own `Repr` draws from. A slot is only ever handed to a later
    // value of the same kind, which is what keeps one bit per slot right at
    // every program counter.
    assert_eq!(
        listing(
            "fn tidy(rows: Array<String>) -> String { \" \".join(rows).trim() }",
            "tidy"
        ),
        "\
fn0 m.tidy(1) -> ref
  frame 4: s0!:ref s1:ref s2:ref s3:ref
     0  str s2:ref \" \"
     1  call-builtin s3:ref String.join (s2:ref s0:ref)
     2  clear s2:ref
     3  call-builtin s2:ref String.trim (s3:ref)
     4  clear s3:ref
     5  move s1:ref s2:ref
     6  clear s2:ref
     7  return s1:ref
"
    );
}

#[test]
fn a_scalar_method_is_a_call_over_words() {
    // Nothing allocates and nothing is a reference, so there is nothing to
    // clear: the whole of what these say is which operation the machine
    // performs and in which order it reads its operands.
    assert_eq!(
        listing("fn nearer(a: Int, b: Int) -> Int { a.min(b) }", "nearer"),
        "\
fn0 m.nearer(2) -> int
  frame 4: s0!:int s1!:int s2:int s3:int
     0  call-builtin s3:int Int.min (s0:int s1:int)
     1  move s2:int s3:int
     2  return s2:int
"
    );
    assert_eq!(
        listing("fn show(x: Float) -> String { x.abs().format(2) }", "show"),
        "\
fn0 m.show(1) -> ref
  frame 5: s0!:float s1:ref s2:float s3:int s4:ref
     0  call-builtin s2:float Float.abs (s0:float)
     1  int s3:int 2
     2  call-builtin s4:ref Float.format (s2:float s3:int)
     3  move s1:ref s4:ref
     4  clear s4:ref
     5  return s1:ref
"
    );
}

#[test]
fn an_associated_function_has_no_receiver_and_a_reader_has_one() {
    // The six `Duration` unit names are each both a reader and a builder,
    // and the language spells them the same. The machine tells them apart by
    // the `Repr` of operand 0 — `Repr::Duration` is the receiver of a reader,
    // and anything else is the count of a builder — so the listing is where
    // that promise is kept or broken.
    //
    // pc 2 is a builder and its one operand is `s3:int`; pc 0 and pc 3 are
    // readers and their one operand is a `duration`. Neither can be mistaken
    // for the other, and neither reading is inferred from a word.
    assert_eq!(
        listing(
            "fn total(d: Duration) -> Int { d.millis() + Duration.seconds(2).millis() }",
            "total"
        ),
        "\
fn0 m.total(1) -> int
  frame 6: s0!:duration s1:int s2:int s3:int s4:duration s5:int
     0  call-builtin s2:int Duration.millis (s0:duration)
     1  int s3:int 2
     2  call-builtin s4:duration Duration.seconds (s3:int)
     3  call-builtin s3:int Duration.millis (s4:duration)
     4  add.int s5:int s2:int s3:int
     5  move s1:int s5:int
     6  return s1:int
"
    );
    // A parser is written on the type's name too, and answers the `Result`
    // the checker settled for the call.
    assert_eq!(
        listing(
            "fn read(t: String) -> Result<Int, Error> { Int.parse(t) }",
            "read"
        ),
        "\
fn0 m.read(1) -> ref
  frame 3: s0!:ref s1:ref s2:ref
     0  call-builtin s2:ref Int.parse (s0:ref)
     1  move s1:ref s2:ref
     2  clear s2:ref
     3  return s1:ref
"
    );
}

#[test]
fn the_families_an_answer_is_built_as_are_interned_where_it_is_emitted() {
    // `builtins::make` searches the layout table for the family a builtin's
    // answer belongs to, so a program that never otherwise mentions one has
    // nowhere to put the answer. `indexOf` answers an `Option<Int>` and
    // `split` an `Array<String>`, and neither is written anywhere in these
    // programs.
    let program = lower(&checked(
        "fn at(s: String) -> Option<Int> { s.indexOf(\"x\") }",
    ))
    .expect("the program lowers");
    let shapes: Vec<&Shape> = program.layouts.iter().map(|layout| &layout.shape).collect();
    assert!(
        shapes.contains(&&Shape::Enum {
            cases: vec![
                crate::Case {
                    name: "None".into(),
                    payload: vec![],
                },
                crate::Case {
                    name: "Some".into(),
                    payload: vec![Repr::Int],
                },
            ],
        }),
        "{shapes:?}"
    );

    // A `Result` needs two. The machine builds the `Error` that carries a
    // failure's message itself, and the `Result` layout describes its `Err`
    // word as a reference without saying what is behind it — so the `Error`
    // is interned beside the `Result` rather than left to the one program in
    // ten that happens to write `Error("...")` of its own.
    for source in [
        "fn read(t: String) -> Result<Int, Error> { Int.parse(t) }",
        "fn read(t: String) -> Result<Float, Error> { Float.parse(t) }",
        "fn read(c: Int) -> Result<String, Error> { String.fromCodePoint(c) }",
        "fn read(x: Float) -> Result<Int, Error> { x.toInt() }",
    ] {
        let program = lower(&checked(source)).expect("the program lowers");
        let named: Vec<&str> = program.layouts.iter().map(|layout| &*layout.name).collect();
        assert!(named.contains(&"Result"), "{source}: {named:?}");
        assert!(named.contains(&"Error"), "{source}: {named:?}");
    }
}

#[test]
fn an_option_is_a_case_index_rather_than_a_call() {
    // `isSome()` is the question a `match` already asks of the object: word
    // 0 is the case index, and `Some` is case 1 of an `Option`. A builtin
    // for it would be a call into the runtime to read one word the
    // instruction set reads on its own.
    //
    // The receiver dies at the `get-word` that read its case: the answer is
    // an `Int` from that instruction onwards, and holding the object past it
    // would retain whatever its payload names.
    assert_eq!(
        listing("fn ok(t: String) -> Bool { Int.parse(t).isOk() }", "ok"),
        "\
fn0 m.ok(1) -> bool
  frame 6: s0!:ref s1:bool s2:ref s3:int s4:int s5:bool
     0  call-builtin s2:ref Int.parse (s0:ref)
     1  get-word s3:int s2:ref +0
     2  clear s2:ref
     3  int s4:int 0
     4  eq.int s5:bool s3:int s4:int
     5  move s1:bool s5:bool
     6  return s1:bool
"
    );
    // `isNone` asks about the other case rather than negating the first, so
    // the two differ by the integer they compare against and nothing else.
    let text = listing("fn empty(o: Option<Int>) -> Bool { o.isNone() }", "empty");
    assert!(text.contains("     1  int s3:int 0\n"), "{text}");
}

#[test]
fn unwrap_or_is_a_case_test_and_a_branch() {
    // The fallback is evaluated before the branch and whichever way the
    // branch goes, because it is an ordinary argument: the language
    // evaluates a call's arguments before the call, and one of them may do
    // something. `int s5 0` at pc 3 is that, and it is above the test.
    //
    // The `clear` at pc 11 is after both paths have joined, which is where
    // the `Option` stops being read: the succeeding path reads its payload
    // at pc 8 and the other does not touch it at all.
    assert_eq!(
        listing(
            "fn at(s: String) -> Int { s.indexOf(\"x\").unwrapOr(0) }",
            "at"
        ),
        "\
fn0 m.at(1) -> int
  frame 9: s0!:ref s1:int s2:int s3:ref s4:ref s5:int s6:int s7:int s8:bool
     0  str s3:ref \"x\"
     1  call-builtin s4:ref String.indexOf (s0:ref s3:ref)
     2  clear s3:ref
     3  int s5:int 0
     4  get-word s6:int s4:ref +0
     5  int s7:int 1
     6  eq.int s8:bool s6:int s7:int
     7  branch-false s8:bool 10
     8  get-word s2:int s4:ref +1
     9  jump 11
    10  move s2:int s5:int
    11  clear s4:ref
    12  move s1:int s2:int
    13  return s1:int
"
    );
}

#[test]
fn a_builtin_is_named_once_however_often_the_program_calls_it() {
    // The pool interns, so the table is as long as the operations a program
    // performs rather than as long as the call sites that perform them. What
    // is asserted here is also the contract the machine is written against:
    // the pair is the receiver and the operation as the language reference
    // writes them, and the `Repr` is what the answer is written into.
    let program = lower(&checked(
        "fn all(s: String, x: Int, f: Float, d: Duration) -> String {\n  \
           let a = s.toUpper()\n  let b = x.abs()\n  let c = f.round()\n  \
           let e = d.hours()\n  s.toUpper()\n}",
    ))
    .expect("the program lowers");
    let named: Vec<(&str, &str, Repr)> = program
        .builtins
        .iter()
        .map(|builtin| (&*builtin.receiver, &*builtin.operation, builtin.result))
        .collect();
    assert_eq!(
        named,
        vec![
            ("String", "toUpper", Repr::Ref),
            ("Int", "abs", Repr::Int),
            ("Float", "round", Repr::Float),
            ("Duration", "hours", Repr::Int),
        ]
    );
}

#[test]
fn an_operation_the_machine_does_not_have_names_itself() {
    // A gap names the operation rather than the shape of the syntax, because
    // the message is what says where the next piece of work is: `Range` has
    // no builtin at all, `mapError` takes a closure and a closure-taking
    // method lowers to a loop in the IR rather than to a builtin that calls
    // back, and `snapshot` is a method of every builtin type and of none of
    // the machine's arms.
    assert_eq!(
        refused("fn has(r: Range) -> Bool { r.contains(1) }"),
        vec!["not yet lowered: `Range.contains`"]
    );
    assert_eq!(
        refused("fn wrap(r: Result<Int, Error>) -> Result<Int, Error> { r.mapError(fn(e) { e }) }"),
        vec!["not yet lowered: `Result.mapError`"]
    );
    assert_eq!(
        refused("fn copy(s: String) -> String { s.snapshot() }"),
        vec!["not yet lowered: `String.snapshot`"]
    );
    assert_eq!(
        refused("fn copy(b: Bool) -> Bool { b.snapshot() }"),
        vec!["not yet lowered: `Bool.snapshot`"]
    );
}
