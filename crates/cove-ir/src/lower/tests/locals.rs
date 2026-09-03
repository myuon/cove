//! What the frame's values were called, and over which instructions.
//!
//! [`crate::Local`] is a side table and nothing runs it, so what these pin is
//! not a behaviour but an *answer*: the one a debugger gives when a person
//! stopped at a breakpoint asks what a slot holds. Issue #241 is that
//! question, and until this table the only truthful answer was `s7:int = 3` —
//! true of the machine, and a lie about the program as often as a slot is
//! reused.
//!
//! The listing is the same one every other case here pins, because the names
//! are printed beside the frame they are of: a range that ended in the wrong
//! place is then read against the code it is a range over.

use cove_sema::HostSchemas;

use super::{checked, listing};
use crate::{lower, Function};

/// The lowered `m.name`, for a case that asks the table a question rather
/// than reading it.
fn function(source: &str, name: &str) -> Function {
    let (sources, held) = checked(source);
    let program = lower(&held, &sources, &HostSchemas::new()).expect("the program lowers");
    program
        .functions
        .iter()
        .find(|f| &*f.module == "m" && &*f.name == name)
        .unwrap_or_else(|| panic!("`{name}` was lowered"))
        .clone()
}

/// A parameter is named. It is positional everywhere else in a lowered
/// program — [`crate::Function::params`] is layouts and nothing more — so
/// without this the debugger that can name every local still cannot say
/// which argument a caller passed where.
///
/// Its range is the body's: the parameters are bound before the first
/// instruction and the scope that has them ends where the body does.
#[test]
fn a_parameter_is_named_from_the_first_instruction_of_the_body() {
    assert_eq!(
        listing("fn area(w: Int, h: Int) -> Int { w * h }", "area"),
        "\
fn0 m.area(Int Int) -> Int
  frame 4: s0!:int s1!:int s2:int s3:int
  local w -> s0:Int [0, 2)
  local h -> s1:Int [0, 2)
     0  mul.int s3:int s0:int s1:int
     1  copy s2:int s3:int Int
     2  return s2:int
"
    );
}

/// **The case the table exists for.** Two names, one slot, and nothing in
/// the frame that can tell them apart: `reprs` says `s2` holds an `int` for
/// the whole function, and it holds `a` over one stretch of the code and `b`
/// over another because the lowering handed the dead run to the next value
/// of the same shape.
///
/// The two ranges are disjoint, and they must be: the run was given back
/// before it was handed on, so there is no pc at which both names denote it.
#[test]
fn a_slot_two_scopes_reused_carries_a_name_over_each_of_its_two_lives() {
    assert_eq!(
        listing(
            "fn f() -> Int {\n\
             \x20 var t = 0\n\
             \x20 {\n\
             \x20   let a = 1\n\
             \x20   t = t + a\n\
             \x20 }\n\
             \x20 {\n\
             \x20   let b = 2\n\
             \x20   t = t + b\n\
             \x20 }\n\
             \x20 t\n\
             }",
            "f"
        ),
        "\
fn0 m.f() -> Int
  frame 4: s0:int s1:int s2:int s3:int
  local t -> s1:Int [1, 8)
  local a -> s2:Int [2, 4)
  local b -> s2:Int [5, 7)
     0  int s1:int 0
     1  int s2:int 1
     2  add.int s3:int s1:int s2:int
     3  copy s1:int s3:int Int
     4  int s2:int 2
     5  add.int s3:int s1:int s2:int
     6  copy s1:int s3:int Int
     7  copy s0:int s1:int Int
     8  return s0:int
"
    );
}

/// Shadowing is recorded rather than resolved: `let x` twice is two locals,
/// of two slots, whose ranges overlap. Both are kept because the first is
/// still what the frame holds at the pcs before the second — and the second
/// is what the initialiser `x + 1` reads, so a table that had dropped one
/// would have dropped the one the source means.
#[test]
fn a_shadowing_declaration_is_a_second_local_beside_the_one_it_shadows() {
    assert_eq!(
        listing("fn f() -> Int {\n  let x = 1\n  let x = x + 1\n  x\n}", "f"),
        "\
fn0 m.f() -> Int
  frame 4: s0:int s1:int s2:int s3:int
  local x -> s1:Int [1, 4)
  local x -> s3:Int [3, 4)
     0  int s1:int 1
     1  int s2:int 1
     2  add.int s3:int s1:int s2:int
     3  copy s0:int s3:int Int
     4  return s0:int
"
    );
}

/// And the rule for reading two of them is the last match, which is the rule
/// the lowering's own scope search follows: a shadowing declaration wins from
/// the pc it is made at, and the shadowed one is the answer before that.
#[test]
fn the_last_local_that_matches_is_the_one_the_name_denotes() {
    let f = function("fn f() -> Int {\n  let x = 1\n  let x = x + 1\n  x\n}", "f");
    assert_eq!(f.local_at("x", 1).map(|local| local.slot), Some(1));
    assert_eq!(f.local_at("x", 3).map(|local| local.slot), Some(3));
    assert_eq!(f.local_at("y", 3), None);
}

/// A local's range ends where its scope does, not where the function does:
/// the block's `b` is unanswerable at the instruction after the block, which
/// is the whole point of carrying a range rather than a name per slot.
#[test]
fn a_local_is_not_bound_past_the_scope_that_declared_it() {
    let f = function(
        "fn f() -> Int {\n  let a = 1\n  {\n    let b = 2\n    a + b\n  }\n}",
        "f",
    );
    assert!(f.local_at("b", 2).is_some(), "inside the block");
    assert_eq!(f.local_at("b", 4), None, "at the copy after it");
    assert!(
        f.local_at("a", 4).is_some(),
        "where the enclosing one still is"
    );
}

/// A `break` does not end a range. It leaves the scope without ending it —
/// the lowering says as much in `Frame::refs_within` — and `[from, to)` is
/// an interval of program counters rather than a list of the ways out: every
/// pc inside the loop's body is one the element is live at, and the pc the
/// `break` jumps to is outside the interval already.
#[test]
fn a_break_leaves_the_element_s_range_rather_than_cutting_it_short() {
    let f = function(
        "fn f(xs: Array<Int>) -> Int {\n\
         \x20 var t = 0\n\
         \x20 for x in xs {\n\
         \x20   if x > 2 {\n\
         \x20     break\n\
         \x20   }\n\
         \x20   t = t + x\n\
         \x20 }\n\
         \x20 t\n\
         }",
        "f",
    );
    let x = f
        .local_at("x", 12)
        .expect("the element is named in the body");
    let leaves = match f.code[13] {
        crate::Inst::Jump { to } => to,
        ref other => panic!("the `break` is a jump, not a {other:?}"),
    };
    assert!(x.to <= leaves, "the break lands past the element's range");
    assert_eq!(f.local_at("x", leaves), None);
}
