//! `match`, and what each kind of pattern costs.

use super::{checked, listing};
use crate::lower::lower;

#[test]
fn a_match_over_an_enum_dispatches_on_the_case_index() {
    // One indexed jump rather than one comparison per case: the index is a
    // word of the object, so the arms are a table. The table has a default
    // even though the checker proved the `match` exhaustive, because the
    // index came out of the heap and the machine does not take the
    // lowering's word for what is in it.
    assert_eq!(
        listing(
            "enum Verdict { Keep, Drop(String) }\n\
             fn why(v: Verdict) -> String { match v { Verdict.Keep => \"kept\", Verdict.Drop(reason) => reason } }",
            "why"
        ),
        "\
fn0 m.why(1) -> ref
  frame 5: s0!:ref s1:ref s2:ref s3:int s4:ref
     0  get-word s3:int s0:ref +0
     1  switch s3:int [2 6] else 10
     2  str s4:ref \"kept\"
     3  move s2:ref s4:ref
     4  clear s4:ref
     5  jump 11
     6  get-word s4:ref s0:ref +1
     7  move s2:ref s4:ref
     8  clear s4:ref
     9  jump 11
    10  trap \"no `match` arm covers this value\"
    11  move s1:ref s2:ref
    12  clear s2:ref
    13  return s1:ref
"
    );
}

#[test]
fn a_payload_binding_is_the_word_it_was_read_into() {
    // The word the payload holds *is* what the name means, so the slot the
    // read went into becomes the binding rather than being copied out of —
    // and it belongs to the arm's scope, so a binding holding a reference is
    // cleared when the arm ends.
    assert_eq!(
        listing(
            "fn unwrap(x: Option<String>) -> String { match x { Some(s) => s, None => \"\" } }",
            "unwrap"
        ),
        "\
fn0 m.unwrap(1) -> ref
  frame 5: s0!:ref s1:ref s2:ref s3:int s4:ref
     0  get-word s3:int s0:ref +0
     1  switch s3:int [6 2] else 10
     2  get-word s4:ref s0:ref +1
     3  move s2:ref s4:ref
     4  clear s4:ref
     5  jump 11
     6  str s4:ref \"\"
     7  move s2:ref s4:ref
     8  clear s4:ref
     9  jump 11
    10  trap \"no `match` arm covers this value\"
    11  move s1:ref s2:ref
    12  clear s2:ref
    13  return s1:ref
"
    );
}

#[test]
fn an_arm_that_fails_after_reading_a_payload_clears_what_it_read() {
    // This is the reason an arm has a failure block of its own. Deciding
    // whether `Some("a")` matches means reading the payload into a
    // reference slot, and an arm that then fails jumps to the next candidate
    // without running its body — so the slot is cleared on that path as well
    // as on the one that matched.
    //
    // The two arms for `Some` are chained: the switch sends every `Some` to
    // the first, and the first sends what it does not match to the second.
    // Nothing is emitted twice.
    assert_eq!(
        listing(
            "fn label(x: Option<String>) -> String {\n\
               match x { Some(\"a\") => \"first\", Some(other) => other, None => \"none\" }\n\
             }",
            "label"
        ),
        "\
fn0 m.label(1) -> ref
  frame 7: s0!:ref s1:ref s2:ref s3:int s4:ref s5:ref s6:bool
     0  get-word s3:int s0:ref +0
     1  switch s3:int [18 2] else 22
     2  get-word s4:ref s0:ref +1
     3  str s5:ref \"a\"
     4  eq.str s6:bool s4:ref s5:ref
     5  clear s5:ref
     6  branch-false s6:bool 12
     7  clear s4:ref
     8  str s5:ref \"first\"
     9  move s2:ref s5:ref
    10  clear s5:ref
    11  jump 23
    12  clear s4:ref
    13  jump 14
    14  get-word s4:ref s0:ref +1
    15  move s2:ref s4:ref
    16  clear s4:ref
    17  jump 23
    18  str s4:ref \"none\"
    19  move s2:ref s4:ref
    20  clear s4:ref
    21  jump 23
    22  trap \"no `match` arm covers this value\"
    23  move s1:ref s2:ref
    24  clear s2:ref
    25  return s1:ref
"
    );
}

#[test]
fn a_match_over_anything_else_is_a_chain_of_comparisons() {
    // There is no index to switch on and the arms' literals are values
    // rather than a dense numbering, so each arm is a comparison and a
    // branch to the next.
    assert_eq!(
        listing(
            "fn name(n: Int) -> String { match n { 0 => \"zero\", 1 => \"one\", _ => \"many\" } }",
            "name"
        ),
        "\
fn0 m.name(1) -> ref
  frame 6: s0!:int s1:ref s2:ref s3:int s4:bool s5:ref
     0  int s3:int 0
     1  eq.int s4:bool s0:int s3:int
     2  branch-false s4:bool 7
     3  str s5:ref \"zero\"
     4  move s2:ref s5:ref
     5  clear s5:ref
     6  jump 19
     7  int s3:int 1
     8  eq.int s4:bool s0:int s3:int
     9  branch-false s4:bool 14
    10  str s5:ref \"one\"
    11  move s2:ref s5:ref
    12  clear s5:ref
    13  jump 19
    14  str s5:ref \"many\"
    15  move s2:ref s5:ref
    16  clear s5:ref
    17  jump 19
    18  trap \"no `match` arm covers this value\"
    19  move s1:ref s2:ref
    20  clear s2:ref
    21  return s1:ref
"
    );
}

#[test]
fn a_binding_over_a_string_holds_a_copy_that_dies_with_its_arm() {
    // A binding names the scrutinee, and the name is a slot of its own
    // rather than the scrutinee's: the arm's scope owns it, so it is given
    // back and cleared when the arm ends. The literal a comparison built is
    // dead the moment it has been compared, on either path, so it is cleared
    // there rather than held for the branch.
    assert_eq!(
        listing(
            "fn rank(s: String) -> Int { match s { \"a\" => 1, other => 2 } }",
            "rank"
        ),
        "\
fn0 m.rank(1) -> int
  frame 6: s0!:ref s1:int s2:int s3:ref s4:bool s5:int
     0  str s3:ref \"a\"
     1  eq.str s4:bool s0:ref s3:ref
     2  clear s3:ref
     3  branch-false s4:bool 7
     4  int s5:int 1
     5  move s2:int s5:int
     6  jump 13
     7  move s3:ref s0:ref
     8  int s5:int 2
     9  move s2:int s5:int
    10  clear s3:ref
    11  jump 13
    12  trap \"no `match` arm covers this value\"
    13  move s1:int s2:int
    14  return s1:int
"
    );
}

#[test]
fn a_pattern_inside_a_pattern_tests_from_the_outside_in() {
    // A nested case is the same two instructions one level down, and the
    // object read out of the outer payload is cleared on both ways out of
    // the test. The two `Ok` arms are one chain: the switch sends every `Ok`
    // to the first, and the first sends what it does not match to the
    // second.
    assert_eq!(
        listing(
            "fn depth(x: Result<Option<Int>, String>) -> Int {\n\
               match x { Ok(Some(n)) => n, Ok(None) => 0, Err(_) => 0 - 1 }\n\
             }",
            "depth"
        ),
        "\
fn0 m.depth(1) -> int
  frame 9: s0!:ref s1:int s2:int s3:int s4:ref s5:int s6:bool s7:ref s8:int
     0  get-word s3:int s0:ref +0
     1  switch s3:int [2 26] else 31
     2  get-word s4:ref s0:ref +1
     3  get-word s3:int s4:ref +0
     4  int s5:int 1
     5  eq.int s6:bool s3:int s5:int
     6  branch-false s6:bool 13
     7  clear s4:ref
     8  get-word s7:ref s0:ref +1
     9  get-word s3:int s7:ref +1
    10  clear s7:ref
    11  move s2:int s3:int
    12  jump 32
    13  clear s4:ref
    14  jump 15
    15  get-word s4:ref s0:ref +1
    16  get-word s3:int s4:ref +0
    17  int s5:int 0
    18  eq.int s6:bool s3:int s5:int
    19  branch-false s6:bool 24
    20  clear s4:ref
    21  int s3:int 0
    22  move s2:int s3:int
    23  jump 32
    24  clear s4:ref
    25  jump 31
    26  int s3:int 0
    27  int s5:int 1
    28  sub.int s8:int s3:int s5:int
    29  move s2:int s8:int
    30  jump 32
    31  trap \"no `match` arm covers this value\"
    32  move s1:int s2:int
    33  return s1:int
"
    );
}

#[test]
fn a_scrutinee_that_is_a_temporary_is_cleared_once_every_arm_has_run() {
    // The scrutinee is live for the whole `match`, because every arm reads
    // out of it, and dead the instant the answer has been assembled.
    assert_eq!(
        listing(
            "fn one() -> Option<Int> { Some(1) }\n\
             fn read() -> Int { match one() { Some(n) => n, None => 0 } }",
            "read"
        ),
        "\
fn1 m.read(0) -> int
  frame 4: s0:int s1:int s2:ref s3:int
     0  call s2:ref m.one ()
     1  get-word s3:int s2:ref +0
     2  switch s3:int [6 3] else 9
     3  get-word s3:int s2:ref +1
     4  move s1:int s3:int
     5  jump 10
     6  int s3:int 0
     7  move s1:int s3:int
     8  jump 10
     9  trap \"no `match` arm covers this value\"
    10  clear s2:ref
    11  move s0:int s1:int
    12  return s0:int
"
    );
}

#[test]
fn an_unmatched_scrutinee_reaches_a_trap() {
    // The trap is the switch's default and the end of a comparison chain,
    // and it is the same message on both. It is not a refusal to run the
    // program: the program ran, and this is what it did.
    let program = lower(&checked(
        "fn name(n: Int) -> Int { match n { 0 => 1, _ => 2 } }",
    ))
    .expect("the program lowers");
    assert!(program
        .strings
        .iter()
        .any(|text| &**text == "no `match` arm covers this value"));
}
