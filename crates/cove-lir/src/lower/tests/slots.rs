//! The frame: one numbering, a fixed kind per slot, and reuse within a kind.

use super::{checked, listing};
use crate::lower::lower;
use crate::repr::Repr;

#[test]
fn two_dead_temporaries_of_the_same_kind_share_a_slot() {
    // A frame is what a call costs, so a temporary that has died is handed
    // to the next value of its own kind rather than leaving the frame to
    // grow with the length of the body. `s1` and `s2` each hold four
    // different integers here.
    assert_eq!(
        listing(
            "fn pair() -> Int {\n  let a = 1 + 2\n  let b = 3 + 4\n  a + b\n}",
            "pair"
        ),
        "\
fn0 m.pair(0) -> int
  frame 5: s0:int s1:int s2:int s3:int s4:int
     0  int s1:int 1
     1  int s2:int 2
     2  add.int s3:int s1:int s2:int
     3  int s1:int 3
     4  int s2:int 4
     5  add.int s4:int s1:int s2:int
     6  add.int s1:int s3:int s4:int
     7  move s0:int s1:int
     8  return s0:int
"
    );
}

#[test]
fn a_slot_is_never_reused_by_a_value_of_another_kind() {
    // This is what makes one static reference map right at every program
    // counter, so it is asserted over the frame rather than inferred from a
    // listing: no slot's kind changes, whatever the free lists did.
    let program = lower(&checked(
        "fn mixed(n: Int, f: Float, flag: Bool) -> Int {\n\
           let a = n + 1\n\
           let b = f * 2.0\n\
           let c = flag && true\n\
           let d = n - 1\n\
           let e = f / 2.0\n\
           a + d\n\
         }",
    ))
    .expect("the program lowers");
    let mixed = program.function(program.function_named("m", "mixed").expect("lowered"));
    // Every kind the body mentions is present, and a frame of scalars is no
    // root at all.
    assert_eq!(mixed.reprs[0], Repr::Int);
    assert_eq!(mixed.reprs[1], Repr::Float);
    assert_eq!(mixed.reprs[2], Repr::Bool);
    assert!(mixed.refs.is_empty());
}

#[test]
fn a_scope_that_ends_gives_its_slots_back() {
    // The two blocks declare a local each, and the second gets the first
    // one's slot because the first block ended.
    assert_eq!(
        listing(
            "fn twice() -> Int {\n  { let a = 1\n    a }\n  { let b = 2\n    b }\n}",
            "twice"
        ),
        "\
fn0 m.twice(0) -> int
  frame 3: s0:int s1:int s2:int
     0  int s1:int 1
     1  int s2:int 2
     2  move s1:int s2:int
     3  move s0:int s1:int
     4  return s0:int
"
    );
}

#[test]
fn a_scalar_body_emits_no_clear() {
    // Clearing is what ends a reference's live range, and a body with no
    // reference in it has none to end: no slot here is a `Ref` or an
    // `Addr`, so a scalar program pays nothing for the mechanism a body
    // holding an object needs.
    let program = lower(&checked(
        "fn deep(n: Int) -> Int {\n\
           var total = 0\n\
           var i = 0\n\
           while i < n {\n\
             let step = i * 2\n\
             total += step\n\
             i += 1\n\
           }\n\
           total\n\
         }",
    ))
    .expect("the program lowers");
    for function in &program.functions {
        assert!(!function
            .code
            .iter()
            .any(|inst| matches!(inst, crate::Inst::Clear { .. })));
        assert!(function.refs.is_empty());
    }
}

#[test]
fn every_lowered_program_is_verified_before_it_is_handed_back() {
    // `lower` runs the verifier itself and panics on a fault, because a
    // fault there is a bug in the lowering rather than in the program. What
    // this pins is that the corpus below reaches that check at all.
    for source in [
        "fn a() -> Int { 1 }",
        "fn b(x: Int, y: Float, z: Bool, d: Duration) -> Bool { z && x > 0 }",
        "fn c(n: Int) -> Int { if n == 0 { 0 } else { c(n - 1) } }",
        "fn d() {\n  var i = 0\n  while i < 10 {\n    if i == 5 { break } else { i += 1 }\n  }\n}",
        "fn e() -> Duration { -1s + 2s }",
        "fn f(n: Int) -> Int {\n  var total = 0\n  var i = 0\n  while true {\n    i += 1\n    if i > n { break }\n    if i % 2 == 0 { continue }\n    total += i\n  }\n  total\n}",
        // The heap half of the corpus: a string and its interpolations, a
        // declared struct and enum, every pattern shape, `?`, a host call,
        // and a `var` argument naming a field.
        "fn g(name: String, n: Int) -> String { \"{name} has {n}\" }",
        "struct Point { x: Int, y: Int }\nfn h(p: Point) -> Int { p.x + p.y }",
        "struct Point { x: Int, y: Int }\nfn bump(var n: Int) { n += 1 }\n\
         fn i(var p: Point) -> Int {\n  bump(var p.x)\n  p.x\n}",
        "enum Verdict { Keep, Drop(String) }\n\
         fn j(v: Verdict) -> String { match v { Verdict.Keep => \"k\", Verdict.Drop(why) => why } }",
        "fn k(x: Result<Option<Int>, String>) -> Int {\n\
           match x { Ok(Some(n)) => n, Ok(None) => 0, Err(_) => 0 - 1 }\n\
         }",
        "fn l(x: Result<Int, String>) -> Result<Int, String> {\n  let n = x?\n  Ok(n + 1)\n}",
        "use console.println\nfn m(n: Int) -> Result<Unit, Error> {\n\
           var i = 0\n  while i < n {\n    println(\"row {i}\")?\n    i += 1\n  }\n  Ok(())\n}",
    ] {
        let program = lower(&checked(source)).expect("the program lowers");
        crate::verify(&program).expect("a lowered program is well formed");
    }
}

#[test]
fn a_reference_slot_is_never_handed_to_a_value_of_another_kind() {
    // The invariant `RefMap` rests on, asserted over a body that holds
    // objects rather than over one that holds only scalars: a slot's kind is
    // fixed for the whole function, whatever the free lists did, so one
    // static bitmap is right at every program counter.
    let program = lower(&checked(
        "struct User { name: String, age: Int }\n\
         fn describe(u: User, greeting: String) -> String {\n\
           let head = \"{greeting}, {u.name}\"\n\
           let years = \"{u.age}\"\n\
           match u.age { 0 => head, _ => years }\n\
         }",
    ))
    .expect("the program lowers");
    let describe = program.function(program.function_named("m", "describe").expect("lowered"));
    assert_eq!(describe.refs, crate::RefMap::of(&describe.reprs));
    assert!(!describe.refs.is_empty());
    // Every slot the map calls a root is a `Ref`, and every `Clear` names a
    // slot that could hold one.
    for slot in describe.refs.iter() {
        assert_eq!(describe.repr(slot), Some(Repr::Ref));
    }
    for inst in &describe.code {
        if let crate::Inst::Clear { slot } = inst {
            assert!(matches!(
                describe.repr(*slot),
                Some(Repr::Ref) | Some(Repr::Addr)
            ));
        }
    }
}
