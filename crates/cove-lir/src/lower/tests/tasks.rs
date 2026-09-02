//! `scope`, `spawn`, `await` and `cancel`.
//!
//! The one construct whose lowering is decided by two things at once: what
//! the Language Card says a scope does, and where a flat instruction stream
//! can put it. `cove_lir::lower::tasks` is the prose; these are the
//! listings.

use super::listing;

/// The Language Card's sentence, as instructions.
///
/// A scope is entered, a closure is built and spawned into it, and leaving
/// the scope waits for the task the body never awaited. `scope.leave` answers
/// a `Bool` and a payload rather than jumping, and what follows the branch is
/// what `?` emits: the enclosing function's own `Err`, built here because
/// which `Err` to build is a fact about *this* function and not about the
/// scope.
///
/// The `clear` after the `spawn` is the closure temporary's, and it is why
/// the machine's scheduler table holds the environment's address until the
/// task is joined: from here on nothing in this frame names it, and ADR
/// 0008's amendment says the child has not necessarily read it yet.
#[test]
fn a_scope_spawns_a_task_and_waits_for_it_where_it_is_left() {
    assert_eq!(
        listing(
            "fn go() -> Result<Unit, Error> {\n  scope tasks {\n    let t = tasks.spawn { 1 }\n  }\n  Ok(())\n}",
            "go"
        ),
        "\
fn0 m.go() -> Result
  frame 12: s0:int s1:unit s2:ref s3:unit s4:scope s5:ref s6:int s7:task s8:bool s9:int s10:unit s11:ref
     0  scope.enter s4:scope \"tasks\"
     1  alloc s5:ref closure m.go#0<closure>
     2  int s6:int 1
     3  store-field s5:ref +0 s6:int Int
     4  spawn s7:task s4:scope s5:ref Int
     5  clear s5:ref fn
     6  unit s3:unit
     7  scope.leave s4:scope s8:bool s5:ref Error
     8  branch-false s8:bool 13
     9  int s9:int 1
    10  clear s10:unit Unit
    11  copy s11:ref s5:ref Error
    12  return s9:int
    13  clear s5:ref Error
    14  unit s3:unit
    15  int s9:int 0
    16  clear s11:ref <ref>
    17  copy s10:unit s3:unit Unit
    18  copy s0:int s9:int Result
    19  clear s9:int Result
    20  return s0:int
"
    );
}

/// `await` and `cancel` are one instruction each, and neither is a builtin.
///
/// A task's operations are the scheduler's rather than the heap's, so they
/// are not entries of `cove_runtime::lvm::builtins`: what they do is start,
/// join and flag a thread, none of which is a walk over words.
///
/// `cancel` asks and answers `()`. Whether the task stopped or had already
/// finished is known only where something waits for it, which is the `await`
/// two lines below.
#[test]
fn awaiting_and_cancelling_are_one_instruction_each() {
    assert_eq!(
        listing(
            "fn go() -> Result<Int, Error> {\n  scope tasks {\n    let t = tasks.spawn { 1 }\n    t.cancel()\n    Ok(await t)\n  }\n}",
            "go"
        ),
        "\
fn0 m.go() -> Result
  frame 15: s0:int s1:int s2:ref s3:int s4:int s5:ref s6:scope s7:ref s8:int s9:task s10:unit s11:int s12:int s13:ref s14:bool
     0  scope.enter s6:scope \"tasks\"
     1  alloc s7:ref closure m.go#0<closure>
     2  int s8:int 1
     3  store-field s7:ref +0 s8:int Int
     4  spawn s9:task s6:scope s7:ref Int
     5  clear s7:ref fn
     6  cancel s9:task
     7  unit s10:unit
     8  await s8:int s9:task Int
     9  int s11:int 0
    10  clear s13:ref <ref>
    11  copy s12:int s8:int Int
    12  copy s3:int s11:int Result
    13  clear s11:int Result
    14  scope.leave s6:scope s14:bool s7:ref Error
    15  branch-false s14:bool 20
    16  int s11:int 1
    17  clear s12:int Int
    18  copy s13:ref s7:ref Error
    19  return s11:int
    20  clear s7:ref Error
    21  copy s0:int s3:int Result
    22  clear s3:int Result
    23  return s0:int
"
    );
}

/// A `return` out of a scope's body cancels it on the way, and the `scope`
/// keeps its own `scope.leave` for the path that reaches the end.
///
/// Leaving a scope waits for or cancels its child tasks *whichever way it is
/// left*, and a flat instruction stream has no way to say which scopes a jump
/// is leaving — so the lowering says it, once per open scope, in the same
/// place it emits the `Clear`s a jump owes. A `?`, a `break` and a `continue`
/// go through the same list.
#[test]
fn a_return_inside_a_scope_cancels_it_on_the_way_out() {
    assert_eq!(
        listing(
            "fn go(early: Bool) -> Result<Int, Error> {\n  scope tasks {\n    let t = tasks.spawn { 1 }\n    if early { return Ok(0) }\n    Ok(await t)\n  }\n}",
            "go"
        ),
        "\
fn0 m.go(Bool) -> Result
  frame 16: s0!:bool s1:int s2:int s3:ref s4:int s5:int s6:ref s7:scope s8:ref s9:int s10:task s11:int s12:int s13:ref s14:unit s15:bool
     0  scope.enter s7:scope \"tasks\"
     1  alloc s8:ref closure m.go#0<closure>
     2  int s9:int 1
     3  store-field s8:ref +0 s9:int Int
     4  spawn s10:task s7:scope s8:ref Int
     5  clear s8:ref fn
     6  branch-false s0:bool 13
     7  int s9:int 0
     8  int s11:int 0
     9  clear s13:ref <ref>
    10  copy s12:int s9:int Int
    11  scope.cancel s7:scope
    12  return s11:int
    13  await s9:int s10:task Int
    14  int s11:int 0
    15  clear s13:ref <ref>
    16  copy s12:int s9:int Int
    17  copy s4:int s11:int Result
    18  clear s11:int Result
    19  scope.leave s7:scope s15:bool s8:ref Error
    20  branch-false s15:bool 25
    21  int s11:int 1
    22  clear s12:int Int
    23  copy s13:ref s8:ref Error
    24  return s11:int
    25  clear s8:ref Error
    26  copy s1:int s4:int Result
    27  clear s4:int Result
    28  return s1:int
"
    );
}
