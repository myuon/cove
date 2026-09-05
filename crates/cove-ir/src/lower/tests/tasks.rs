//! `scope`, `spawn`, `await` and `cancel`.
//!
//! The one construct whose lowering is decided by two things at once: what
//! the Language Card says a scope does, and where a flat instruction stream
//! can put it. `cove_ir::lower::tasks` is the prose; these are the
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
    let text = listing(
        "fn go() -> Result<Unit, Error> {\n  scope tasks {\n    let t = tasks.spawn { Ok(1) }\n  }\n  Ok(())\n}",
        "go",
    );
    // A child that answers a `Result` is one that can hand the scope a
    // failure, so leaving it branches and the arm that follows is what `?`
    // emits: the enclosing function's own `Err`, built here because which
    // `Err` to build is a fact about *this* function and not about the scope.
    assert!(text.contains("scope.enter"), "{text}");
    assert!(text.contains("spawn "), "{text}");
    assert!(text.contains("scope.leave"), "{text}");
    assert!(text.contains("branch-false"), "{text}");
    // The `clear` after the `spawn` is the closure temporary's, and it is why
    // the machine's scheduler table holds the environment's address until the
    // task is joined: from here on nothing in this frame names it, and ADR
    // 0008's amendment says the child has not necessarily read it yet.
    let spawn = text.find("spawn ").expect("a spawn");
    assert!(text[spawn..].contains("clear"), "{text}");
}

/// `await` and `cancel` are one instruction each, and neither is a builtin.
///
/// A task's operations are the scheduler's rather than the heap's, so they
/// are not entries of `cove_runtime::vm::builtins`: what they do is start,
/// join and flag a thread, none of which is a walk over words.
///
/// `cancel` asks and answers `()`. Whether the task stopped or had already
/// finished is known only where something waits for it, which is the `await`
/// two lines below.
#[test]
fn awaiting_and_cancelling_are_one_instruction_each() {
    let text = listing(
        "fn go() -> Result<Int, Error> {\n  scope tasks {\n    let t = tasks.spawn { 1 }\n    t.cancel()\n    Ok(await t)\n  }\n}",
        "go",
    );
    assert!(text.contains("await "), "{text}");
    assert!(text.contains("cancel "), "{text}");
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
    let text = listing(
        "fn go(early: Bool) -> Result<Int, Error> {\n  scope tasks {\n    let t = tasks.spawn { 1 }\n    if early { return Ok(0) }\n    Ok(await t)\n  }\n}",
        "go",
    );
    assert!(text.contains("scope.cancel"), "{text}");
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
     2  return s0:int Int
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
     0  call s1:int m.g () Int
     1  settled s2:task s1:int Int
     2  await s1:int s2:task Int
     3  copy s0:int s1:int Int
     4  return s0:int Int
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
  local g -> s1:fn [3, 7)
     0  alloc s1:ref closure m.f#0<closure>
     1  int s2:int 1
     2  store-field s1:ref +0 s2:int Int
     3  call-closure s2:int s1:ref ()
     4  settled s3:task s2:int Int
     5  await s2:int s3:task Int
     6  copy s0:int s2:int Int
     7  return s0:int Int
"
    );
}
