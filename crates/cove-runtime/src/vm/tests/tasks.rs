//! Scopes, spawned tasks, `async fn`, and `Shared`.
//!
//! A scope's lowering is asserted about as often as its answer is, because
//! leaving one is not a single instruction: it has to be left on the way out
//! of every path through it, including the `break` that cancels it.

use super::*;

/// Two tasks in one scope, awaited, on a VM each.
///
/// `agree` is the whole assertion: what the two backends answer and what
/// they printed. A task's own VM is a second one over the same program,
/// so a body that reaches a declared function is reaching the function
/// the same `FunctionId` names on the spawning thread.
#[test]
fn two_tasks_in_one_scope_answer_what_the_interpreter_answers() {
    assert_eq!(
        value_of(
            "Result<Int, Error>",
            "fn twice(n: Int) -> Int {\n  n * 2\n}\n",
            "  scope work {\n    let a = work.spawn { twice(3) }\n    let b = work.spawn { twice(4) }\n    Ok(await a + await b)\n  }"
        ),
        "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Int(14)] })"
    );
}

/// "Leaving the scope waits for or cancels its child tasks." A task
/// nobody awaited has still run by the time the scope is left, so its
/// line is printed before whatever follows the scope.
#[test]
fn leaving_a_scope_waits_for_a_task_nobody_awaited() {
    let outcome = agree_main(
        "Result<Unit, Error>",
        "  scope work {\n    let quiet = work.spawn { println(\"from the task\") }\n  }\n  println(\"after the scope\")?\n  Ok(())",
    );
    assert_eq!(outcome.output, "from the task\nafter the scope\n");
}

/// A child whose value is `Err(...)` returns that failure from the call
/// the scope was written in, exactly as `?` would — which is why the
/// lowering writes a `try` after every `leave-scope`.
#[test]
fn a_child_whose_value_failed_returns_that_failure_from_the_call() {
    let outcome = agree(
        "use console.println\n\nfn boom() -> Result<Int, Error> {\n  Err(Error(\"boom\"))\n}\n\n             export fn main() -> Result<Unit, Error> {\n  scope work {\n    let t = work.spawn { boom() }\n  }\n               println(\"never\")?\n  Ok(())\n}\n",
    );
    assert_eq!(
        outcome.value(),
        "Enum(EnumValue { type_name: \"Result\", case: \"Err\", payload: [Struct(StructValue { type_name: \"Error\", fields: [(\"message\", Str(\"boom\"))], opaque: false })] })"
    );
    assert_eq!(outcome.output, "");
}

/// A child that *raised* is not a value, so it never reaches that `try`:
/// it propagates as the error it is, pointing where it was raised.
#[test]
fn a_child_that_raised_propagates_as_the_error_it_is() {
    let (sources, checked) = checked_module(
        "use console.println\n\nexport fn main() -> Result<Unit, Error> {\n               scope work {\n    let t = work.spawn { 1 / 0 }\n  }\n  Ok(())\n}\n",
    );
    let (interpreted, lowered) = on_both(&checked, &sources, "m", None);
    assert_eq!(interpreted.error().message, "`Int` division by zero");
    assert_eq!(lowered.error().message, interpreted.error().message);
    assert_eq!(lowered.error().span, interpreted.error().span);
}

/// A `break` leaves a scope without reaching its `leave-scope`, so the
/// lowering emits the `cancel-scope` that does what leaving one does.
/// The oracle cancels there too — `Interpreter::leave_scope`'s early
/// branch treats a `Break` the way it treats a `return`.
#[test]
fn a_break_out_of_a_scope_cancels_it() {
    let outcome = agree_main(
        "Result<Unit, Error>",
        "  var i = 0\n  while i < 3 {\n    scope work {\n      let t = work.spawn { i }\n      if i == 1 {\n        break\n      }\n      println(\"turn {await t}\")?\n    }\n    i += 1\n  }\n  println(\"done\")?\n  Ok(())",
    );
    assert_eq!(outcome.output, "turn 0\ndone\n");
    assert!(
        main_of(
            "export fn main() -> Result<Unit, Error> {\n  while true {\n    scope work {\n      break\n    }\n  }\n  Ok(())\n}\n"
        )
        .contains("cancel-scope"),
        "a `break` written inside a scope cancels it"
    );
}

/// The whole shape of a scope, in one listing: the scope is opened, bound
/// to a slot, read back as the receiver of the `spawn`, and left through
/// a `leave-scope` and the `try` that turns a failed child into a return.
#[test]
fn a_scope_with_a_spawn_lowers_to_the_scope_and_the_try_that_leaves_it() {
    assert_eq!(
        main_of(
            "export fn main() -> Result<Unit, Error> {\n  scope work {\n    let t = work.spawn { 1 }\n    await t\n  }\n  Ok(())\n}\n"
        ),
        concat!(
            "fn m.main arity=0 frame=2/0 -> value\n",
            "   0  enter-scope work\n",
            "   1  store 0\n",
            "   2  load 0\n",
            "   3  make-closure m.<closure 0> captures=0\n",
            "   4  spawn\n",
            "   5  store 1\n",
            "   6  load 1\n",
            "   7  await\n",
            "   8  leave-scope\n",
            "   9  try\n",
            "  10  pop\n",
            "  11  const Unit\n",
            "  12  make-builtin Ok argc=1\n",
            "  13  return\n",
        )
    );
}

/// The task-safety rule at the boundary is `crate::task`'s, so a capture
/// that may not cross is refused at the `spawn` in the same words on
/// either backend — including the path that names how it was reached.
#[test]
fn a_capture_that_may_not_cross_is_refused_at_the_spawn() {
    let (sources, checked) = checked_module(
        "export fn main() -> Result<Unit, Error> {\n  var items = Vector.of(1)\n               scope work {\n    let t = work.spawn { items.length() }\n  }\n  Ok(())\n}\n",
    );
    let (interpreted, lowered) = on_both(&checked, &sources, "m", None);
    assert_eq!(
        interpreted.error().message,
        "`spawn` cannot capture `items`, which is a `Vector`"
    );
    assert_eq!(lowered.error().message, interpreted.error().message);
    assert_eq!(lowered.error().span, interpreted.error().span);
}

/// A scope inside a task is a scope like any other: the child's VM has
/// its own frame stack, so the depth a scope is registered at is that
/// VM's own and a nested `spawn` reaches the scope its own body opened.
#[test]
fn a_task_may_open_a_scope_of_its_own() {
    assert_eq!(
        value_of(
            "Result<Int, Error>",
            "fn inner() -> Result<Int, Error> {\n  scope deep {\n    let t = deep.spawn { 6 }\n    Ok(await t)\n  }\n}\n",
            "  scope outer {\n    let a = outer.spawn { inner() }\n    await a\n  }"
        ),
        "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Int(6)] })"
    );
}

/// A run stopped at its concurrency limit is stopped identically on both
/// backends: the charge happens in `crate::task::spawn_into`, before the
/// task has an id, an event, or a thread.
#[test]
fn a_spawn_past_the_concurrency_limit_is_refused_the_same_way() {
    let (sources, checked) = checked_module(
        "export fn main() -> Result<Unit, Error> {\n  scope work {\n                 let a = work.spawn { 1 }\n    let b = work.spawn { 2 }\n  }\n  Ok(())\n}\n",
    );
    let limits = Limits {
        max_tasks: Some(1),
        ..Limits::default()
    };
    let (interpreted, lowered) = on_both(&checked, &sources, "m", Some(limits));
    assert!(
        interpreted
            .error()
            .message
            .contains("concurrency limit of 1 task(s) exceeded"),
        "{:?}",
        interpreted.error()
    );
    assert_eq!(lowered.error().message, interpreted.error().message);
    assert_eq!(lowered.error().span, interpreted.error().span);
}

// ------------------------------------------------------- `async fn`

/// An `async fn` runs its body at the call site and hands back a handle
/// that is already settled, so the body has run whether or not anything
/// awaits it, and awaiting twice repeats no effect.
#[test]
fn an_async_fn_runs_at_the_call_and_settles_once() {
    let outcome = agree(
        "use console.println\n\nasync fn answer() -> Result<Int, Error> {\n               println(\"ran\")?\n  Ok(7)\n}\n\n             export fn main() -> Result<Unit, Error> {\n  let t = answer()\n               println(\"{await t?} {await t?}\")?\n  Ok(())\n}\n",
    );
    assert_eq!(outcome.output, "ran\n7 7\n");
}

/// A `?` that failed leaves the body without reaching a `return`, and the
/// failure is still wrapped: `Vm::leave` is where the frame closes,
/// whichever of the three ways a body ended.
#[test]
fn a_question_mark_inside_an_async_fn_settles_the_task_it_failed_with() {
    let outcome = agree(
        "use console.println\n\nfn boom() -> Result<Int, Error> {\n  Err(Error(\"boom\"))\n}\n\n             async fn load() -> Result<Int, Error> {\n  let n = boom()?\n               println(\"never\")?\n  Ok(n)\n}\n\n             export fn main() -> Result<Unit, Error> {\n  let v = await load()\n               println(\"{v}\")?\n  Ok(())\n}\n",
    );
    assert_eq!(outcome.output, "Err(boom)\n");
}

/// `async fn f() -> Int` answers a `Task<Int>`, which is a value — so the
/// call leaves it on the value stack whatever the checker settled about
/// the declared return type, and only `await` produces the `Int`.
#[test]
fn an_async_fn_that_declares_an_int_still_answers_on_the_value_stack() {
    assert_eq!(
        value_of(
            "Int",
            "async fn twice(n: Int) -> Int {\n  n * 2\n}\n",
            "  await twice(4) + await twice(10)"
        ),
        "Int(28)"
    );
}

/// An `async` lambda is the same rule read off the closure rather than
/// off a declaration, which is why the VM wraps where the frame closes
/// rather than at the call site: nothing at a `call-value` knows which
/// function it will reach.
#[test]
fn an_async_lambda_called_through_a_value_answers_a_task() {
    let outcome = agree(
        "use console.println\n\n             fn run(f: async fn(Int) -> Result<Int, Error>) -> Result<Int, Error> {\n  await f(3)\n}\n\n             export fn main() -> Result<Unit, Error> {\n               let n = run(async fn(x) {\n    println(\"in the lambda\")?\n    Ok(x * 2)\n  })?\n               println(\"{n}\")?\n  Ok(())\n}\n",
    );
    assert_eq!(outcome.output, "in the lambda\n6\n");
}

// ---------------------------------------------------------- `Shared`

/// A `var` closure names the cell's contents, so what it leaves behind is
/// what the cell holds afterwards. That is the whole reason `Inst::Lock`
/// makes its own call rather than going through `Inst::CallValue`.
#[test]
fn a_var_lock_closure_changes_what_the_cell_holds() {
    let outcome = agree(
        "use console.println\n\nstruct C {\n  n: Int\n}\n\n             export fn main() -> Result<Unit, Error> {\n  let c = Shared(C(n: 1))\n               let by = 4\n  c.lock(fn(var v) {\n    v.n += by\n  })\n               c.lock(fn(v) {\n    println(\"n={v.n}\")\n  })?\n  Ok(())\n}\n",
    );
    assert_eq!(outcome.output, "n=5\n");
}

/// A closure written without `var` receives a copy, so the cell keeps
/// what it had — which is what the oracle does with one, and is why the
/// convention is read off the callee's own `params` rather than assumed.
#[test]
fn a_lock_closure_without_var_leaves_the_cell_alone() {
    let outcome = agree(
        "use console.println\n\nstruct C {\n  n: Int\n}\n\n             export fn main() -> Result<Unit, Error> {\n  let c = Shared(C(n: 1))\n               c.lock(fn(v) {\n    println(\"saw {v.n}\")\n  })?\n               c.lock(fn(v) {\n    println(\"still {v.n}\")\n  })?\n  Ok(())\n}\n",
    );
    assert_eq!(outcome.output, "saw 1\nstill 1\n");
}

/// The closure answers the `lock`, and it may capture — which is what
/// `Function::capture_base` is for: a place parameter takes no value
/// slot, so the captures begin one slot earlier than `arity` would say.
#[test]
fn a_lock_answers_what_its_closure_answered_and_may_capture() {
    assert_eq!(
        value_of(
            "Result<Int, Error>",
            "struct C {\n  n: Int\n}\n",
            "  let c = Shared(C(n: 0))\n  let by = 5\n  Ok(c.lock(fn(var v) {\n    v.n += by\n    v.n * 2\n  }))"
        ),
        "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Int(10)] })"
    );
}

/// "There is no `get` and no `set`: every access is scoped." A `lock`
/// taken by a task that already holds the cell would wait for itself, and
/// `SharedCell::lock` says so instead — the same words on either backend,
/// because it is the same code.
#[test]
fn a_reentrant_lock_is_refused_the_same_way() {
    let (sources, checked) = checked_module(
        "use console.println\n\nexport fn main() -> Result<Unit, Error> {\n               let c = Shared(1)\n  c.lock(fn(v) {\n    c.lock(fn(w) {\n      println(\"never\")\n    })\n  })?\n  Ok(())\n}\n",
    );
    let (interpreted, lowered) = on_both(&checked, &sources, "m", None);
    assert_eq!(
        interpreted.error().message,
        "this task already holds this `Shared`, so `lock` would wait for itself"
    );
    assert_eq!(lowered.error().message, interpreted.error().message);
    assert_eq!(lowered.error().span, interpreted.error().span);
}

/// The point of the type: two tasks reach one cell, and `lock` holds it
/// for the whole of the closure, so no count is lost to a
/// read-modify-write that raced.
#[test]
fn two_tasks_recording_through_one_shared_lose_no_count() {
    let outcome = agree(
        "use console.println\n\nstruct C {\n  n: Int\n}\n\n             export fn main() -> Result<Unit, Error> {\n  let c = Shared(C(n: 0))\n               scope work {\n    let a = work.spawn { for i in 0..<50 { c.lock(fn(var v) { v.n += 1 }) } }\n                 let b = work.spawn { for i in 0..<50 { c.lock(fn(var v) { v.n += 1 }) } }\n                 await a\n    await b\n  }\n               c.lock(fn(v) {\n    println(\"n={v.n}\")\n  })?\n  Ok(())\n}\n",
    );
    assert_eq!(outcome.output, "n=100\n");
}

/// The listing, because the convention is the point: the cell and the
/// closure stand on the stack and `lock` does the rest, and the closure
/// the lowering made for it takes a place where every other closure takes
/// a value.
#[test]
fn a_lock_lowers_to_the_cell_the_closure_and_one_instruction() {
    assert_eq!(
        main_of(
            "export fn main() -> Result<Unit, Error> {\n  let c = Shared(1)\n  c.lock(fn(var v) {\n    v = 2\n  })\n  Ok(())\n}\n"
        ),
        concat!(
            "fn m.main arity=0 frame=1/0 -> value\n",
            "   0  const Int(1)\n",
            "   1  make-builtin Shared argc=1\n",
            "   2  store 0\n",
            "   3  load 0\n",
            "   4  make-closure m.<closure 0> captures=0\n",
            "   5  lock\n",
            "   6  pop\n",
            "   7  const Unit\n",
            "   8  make-builtin Ok argc=1\n",
            "   9  return\n",
        )
    );
}
