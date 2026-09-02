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

// ---- an `async fn` ------------------------------------------------------

/// An `async fn` is a `Function` like any other, and the task is made where
/// it is *called*.
///
/// The body is not different for being written `async`: it answers an `Int`,
/// it is entered by an ordinary `call`, and its frame holds nothing a
/// scheduler reads. What the word buys is the instruction after the call —
/// `settled`, which is the handle around a value the call has already
/// produced.
///
/// That is `Interpreter::call_target` taken apart: it runs the body and wraps
/// what came back in `crate::task::Task::settled`. The `async` survives into
/// the listing as the `async` on the header, which is a record of how the
/// declaration was written and nothing the machine reads.
#[test]
fn an_async_declaration_is_an_ordinary_function() {
    assert_eq!(
        listing(
            "async fn g() -> Int { 1 }\nfn f() -> Int { await g() }",
            "g"
        ),
        "\
fn1 m.g() -> Int async
  frame 2: s0:int s1:int
     0  int s1:int 1
     1  copy s0:int s1:int Int
     2  return s0:int
"
    );
}

/// A call to one is a call and a handle, and an `await` of the handle is the
/// value.
///
/// Three instructions where the oracle has three steps, in the same order:
/// run the body, wrap what it produced, settle it. Nothing waits, because
/// there is nothing to wait for — the body finished at instruction 0, on this
/// task's own stack, and `settled` is not a `spawn`.
///
/// The `await` is not folded into the call even though it stands next to it.
/// A call is lowered one way wherever it is written, so `let t = g()` and
/// `await g()` are the same three instructions with the middle one's answer
/// living longer.
#[test]
fn a_call_to_an_async_declaration_answers_a_settled_task() {
    assert_eq!(
        listing(
            "async fn g() -> Int { 1 }\nfn f() -> Int { await g() }",
            "f"
        ),
        "\
fn0 m.f() -> Int
  frame 3: s0:int s1:int s2:task
     0  call s1:int m.g ()
     1  settled s2:task s1:int Int
     2  await s1:int s2:task Int
     3  copy s0:int s1:int Int
     4  return s0:int
"
    );
}

/// An `async` lambda is an ordinary closure, and the `async` is on the call.
///
/// The environment, the `call-closure` and the `clear` are what any lambda
/// gets. `settled` is the only difference between this listing and the one a
/// lambda without the word would produce, which is the whole claim: `async`
/// says nothing about a body and everything about what a call to it answers.
///
/// The oracle agrees by the same route — `Interpreter::call_closure` hands
/// the closure's own `is_async` to `call_target`, which is the same function
/// a declared `async fn` goes through.
#[test]
fn an_async_function_value_is_an_ordinary_closure() {
    assert_eq!(
        listing(
            "fn f() -> Int {\n  let g = async fn() { 1 }\n  await g()\n}",
            "f"
        ),
        "\
fn0 m.f() -> Int
  frame 4: s0:int s1:ref s2:int s3:task
     0  alloc s1:ref closure m.f#0<closure>
     1  int s2:int 1
     2  store-field s1:ref +0 s2:int Int
     3  call-closure s2:int s1:ref ()
     4  settled s3:task s2:int Int
     5  await s2:int s3:task Int
     6  copy s0:int s2:int Int
     7  clear s1:ref fn
     8  return s0:int
"
    );
}
