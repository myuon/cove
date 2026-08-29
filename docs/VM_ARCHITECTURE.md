# The VM: what is built, and what is being tried

> Working document for [issue #116](https://github.com/myuon/cove/issues/116).
> [ADR 0019](adr/0019-executable-ir-and-vm.md) decided that a VM exists and
> that the tree-walking interpreter stays the semantic oracle. Nothing here
> revisits that. This is about the VM's *shape*, which ADR 0019 deliberately
> did not fix, and it stays a working document until a measurement makes one
> of its choices worth an ADR.

## Two things, and they are not the same

**The prototype** is what `crates/cove-runtime/src/vm.rs` is today. It removed
recursive AST evaluation and it is faster — 1.3× to 4.2× depending on what a
program spends its time on. But it is still shaped like a flattened tree
walker: an operand is a general `Value`, a frame is a window into a `Vec` of
them, and the instruction set only recently began to say what it is operating
on.

**The target** is a typed stack machine: a slot whose type the checker proved
holds that type's own representation, a call follows a written convention over
a contiguous frame, and the heap has a layout the VM owns rather than a Rust
object graph it borrows.

The distinction matters because the prototype's numbers are being read as if
they were the ceiling of a dedicated VM, and they are not. They are the ceiling
of *this* VM. Issue #116 exists to measure the other one.

## Where the prototype's remaining cost is

Measured on `benches/arith`, a loop of integer arithmetic that allocates
nothing at all, VM backend, release build with symbols:

```
 69.70%  Vm::execute            the dispatch loop
 10.61%  interp::binary
  5.30%  drop_in_place<Value>
  0.00%  <malloc>
```

and on `benches/chars`, which allocates per character:

```
 31.39%  Vm::execute
 31.14%  <malloc>
  9.73%  builtins::call_method
  7.06%  Vec construction       an argument vector per builtin call
```

Two facts explain most of both:

```
size_of::<Value>()                = 40
needs_drop::<Value>()             = true
size_of::<Result<Value, RuntimeError>>() = 120
```

A loop that adds two integers moves 40 bytes per push, runs drop glue per pop
even for an `Int` that owns nothing, and — before typed instructions — passed
two 40-byte values into a function that returned 120 bytes to answer with an
`i64`.

`interp::binary` is down since typed instructions landed. The other two are
representation, and representation is what #116 proposed to change — see
"The slice, measured" below, where they are gone.

## The calling convention

This section describes what is built today. Where the target changes it, the
change is named.

### The stack

Three contiguous vectors for the whole run, shared by every frame: a
`Vec<Value>`; beside it a `Vec<i64>` for the slots and operands the checker
proved are `Int` or `Bool`; and beside that a `Vec<Place>` for the slots and
operands that name storage rather than hold a value, which is what a `var`
parameter has. A frame is a window into all three:

```rust
struct Frame {
    function: FunctionId,
    return_pc: usize,    // the instruction after the caller's `Call`
    base: usize,         // where this frame's value slots begin
    scalar_base: usize,  // where its scalar slots begin
    place_base: usize,   // where its place slots begin
}
```

A frame's slots are `stack[base .. base + value_frame_size]`,
`scalars[scalar_base .. scalar_base + scalar_frame_size]` and
`places[place_base .. place_base + place_frame_size]`, and its operands sit
above them on each. The three are numbered separately, so which stack a slot
lives in is decided by which instruction addresses it rather than by anything
read at run time. Nothing is allocated per call: the frame is five words pushed
onto a `Vec<Frame>`, and the slots are stack that already exists.

### Argument placement

Arguments are pushed onto the *caller's* operand stacks, left to right, and
become the callee's first slots without moving. So `base` is the caller's
value-operand top, read from the other side, and `scalar_base` and
`place_base` are its scalar- and place-operand tops read the same way.

Which of the three stacks an argument travels on is the callee's declaration,
published as `Function::params`. Two of the three are the checker's answer
about a type: a parameter it settled as `Int` or `Bool` is a scalar slot, so
its argument is pushed onto the scalar stack and becomes that slot, and
everything else is a value slot as before. The third is not a question about a
type at all. A parameter written `var` names the caller's storage whatever its
type is, so it is a place slot, and its argument is a place the caller built
and pushed. `Call` carries the three counts rather than looking them up,
because the lowering has to place a recursive call's arguments before the
callee it is inside exists, and because the depth simulation is a function of
one instruction with no function table beside it. `validate` reconciles the
counts with the callee's own `params`, which is what makes this an invariant
rather than an agreement.

Slot order is fixed at lowering and is dense within each stack: `self` when the
function has a receiver, then each declared parameter in declaration order,
then locals and temporaries in the order the body declares them, each drawing
a number from the stack it lives in. `value_frame_size`,
`scalar_frame_size` and `place_frame_size` are the high-water marks of that,
not the totals, because a block's slots are released at its end and a later
sibling block reuses the numbers. The third is zero for almost every function:
only a `var` parameter and a `var self` receiver take a place slot, and
nothing a body declares takes one at all.

### A call whose target is not known

Two calls do not name a function. `call-value` reaches whatever callable
stands on the stack, and `call-dyn` reaches whichever implementation of a
trait method the receiver's concrete type carries.

`call-dyn` covers three static types, because the oracle covers them with one
code path: a `dyn Trait`, a type parameter bounded by the trait, and the rigid
`Self` of that trait's own default body. `Interpreter::eval_method_call` reads
the concrete value's type name and looks the method up from there, whichever
of the three the receiver was written as, so the lowering takes from the
static type only the trait the call goes through. That is not a convenience:
without it a dispatch through a `dyn` could not reach a method a conformance
left to the trait's default, since such a body is checked once with `self`
typed as `Self: Trait` and every call it makes on `self` goes through the
bound. Both take the same
convention, and for the same reason rather than by coincidence: nothing at
either call site knows which body it will enter, so neither can have placed
its arguments by that body's `Function::params`. Every argument travels on
the value stack and the answer comes back on it, and the lowering numbers a
*second specialisation* of any declaration reached that way — one function
under the convention its own signature names, one under this one. A
declaration both roads reach is one function, because it is one key.

`call-dyn` differs from `call-value` in where the answer comes from. There,
the callee is a value the program built and popped; here, the candidates were
settled when the call was lowered and the instruction names the list —
`cove_ir::Dispatch`, one entry per type that conforms, keyed by the qualified
name that type's values carry. What happens at run time is a scan of that
list for the receiver's own type name. It is a scan rather than a map because
a trait has as many implementations as the package wrote `impl` blocks for it,
which is a handful.

The list is every conformance the *package* declares, deliberately not the
ones the calling module can see. `tests/e2e/outline_dyn_field` is the shape
that forces it: `lib` declares the trait and holds a `dyn Summary` in a field,
`plugin` supplies the conformance, and `lib` never imports `plugin`. The type
standing in that field is one the calling module cannot name, which is what
ADR 0015 calls capability-open — so bounding the candidates by visibility
would leave out exactly the case dynamic dispatch exists for.

The receiver is the first argument rather than a fourth operand: it is `self`,
it becomes the callee's slot 0, and what the instruction does to it is unwrap
it, because the implementation runs on the concrete value and not on the
`dyn Trait` wrapper. `crate::interp::dyn_receiver` is that step and both
backends take it there.

### Where a trait object is made

`make-dyn` is the language's one implicit conversion, and it is the one place
a Cove value's runtime representation depends on its static type. The
interpreter makes it in `Interpreter::coerce`, walking the *written* type at
the moment of the conversion; the VM makes it at an instruction whose walk
happened when the type was lowered. Both end in
`crate::interp::as_dyn`, so neither can build a different wrapper — including
the rule that a value already wrapped is left alone, which is what keeps
`dyn Trait` from nesting.

Four sites convert, because four are where a type is *written*: a parameter,
including one left to its default; an annotated `let`; a struct's field; and a
declared return type. Three of the four are the callee's or the constructor's
rather than the caller's, which is where the interpreter makes them too — a
call knows nothing about the callee's annotations, and a `call-value` or a
`call-dyn` knows nothing about the callee at all.

The instruction carries a *depth* rather than a path, because the walk into
`Array<dyn T>` and `Option<dyn T>` does not branch on which of the two it is
taking: `crate::interp::coerce_inside` reaches into an array's elements and an
option's payload and leaves everything else alone, so the value is what says
which kind a layer was. The lowering counts a layer only for those two heads,
which is what keeps a `Map<K, dyn V>` — whose elements the interpreter does
not convert — from being reached by counting. A `dyn` written somewhere the
walk cannot reach is refused rather than converted.

### What a place is

A place is an index into the value stack together with the field positions to
walk from what stands there — `bump(var total)` builds one naming `total`'s
slot with no path, and `bump(var c.hits)` builds one naming `c`'s slot with
one step on the end. Reading through it clones what it names, which is the
value-semantics rule; writing through it calls `Rc::make_mut` at every struct
step, which is what makes sharing a copied struct unobservable and is the same
call the interpreter's `Place::with_mut` makes at the same steps.

It has to be an alias and not a copy that is written back, because the two are
observably different. `two(var x, var x)` answers 11 on the oracle: both
parameters name one cell, so the second parameter's `+= 10` is applied to what
the first one's `+= 1` left. Copy-in/copy-out would answer 10.

An index rather than a pointer, because the value stack is one `Vec` that
grows: a push can move every element, and an index names the same slot before
and after where a pointer would name freed memory. It is also the only form a
safe Rust program could hold, since a borrow of the stack would stop the VM
from touching the stack. The index is absolute rather than relative to a
frame, because a place travels — it is built in the caller's frame and read
and written in the callee's, where `base` is a different number.

**What makes it valid is that nothing a lowered program can build outlives the
frame it was built in.** A frame's slots are live from the call that opened the
window to the return that truncates it, and a place is built by an instruction
of some frame and consumed by an instruction of that frame or of one it called.
No call answers a place, and no value contains one.

This paragraph used to reach that conclusion the other way round — a callee
cannot outlive its caller, because nothing lowers a closure and a closure is
the construct that could, since it can be returned. A closure lowers now, and
the answer is that **a closure captures the value a place names, never the
place**. The lowering reads a captured `var` parameter with a `place-read`,
which is the read `Env::captures` makes in the interpreter, and the oracle
agrees that it is a read: a closure over a `var` binding still answers what
the binding held when the closure was written, after the binding has been
assigned to. So `make-closure` takes values off the value stack, a
`Value::Closure` holds `Value`s, and the place stack is still something only
a call's arguments travel on. Both `cove_ir::Inst::PlaceLocal` and
`cove_runtime::vm::Place` say so where they are defined.

A callback a host runs re-entrantly does not break it either, and for a
different reason: such a call opens its frame *above* the frames that are
standing and truncates back to where it found them, so a place standing in one
of those frames still points where it pointed. "Host calls and reentry", below,
is where that convention is written down.

The path is a `Vec<u32>` of field positions, which is what the interpreter's
own `Place::steps` is and costs what it costs there: an empty path allocates
nothing, and appending a step to a non-empty one copies it. A fixed-size
inline path would make a place `Copy` and a refinement free, and it was not
taken because the bound could not be enforced where a bound has to be
enforced. The depth a place reaches is the sum of the static appends along a
chain of calls, and the lowering of a callee cannot see what depth its callers
will hand it; so the bound would have to be checked at run time, and a program
that exceeded it would fail on this backend and answer on the oracle. That is
the one difference between the two that is not allowed to exist.

One binding has to move for any of this to work. `bump(var total: Int)` roots
a place at `total`, and a place cannot address the scalar stack, so a binding
a place is rooted at is kept on the value stack even where the checker settled
it as `Int`. The lowering walks a body once before it emits anything and
collects the names used as the root of a `var` argument or of a `freeze`
receiver; a binding of one of those names is a value slot. It is a set of
names rather than of bindings, so it over-approximates across shadowing —
`bump(var total)` written anywhere in a body puts every `total` the body
declares on the value stack. That costs a slot its representation and can cost
nothing else, because both representations hold the same value.

`freeze` is the case the place model is really needed for. It consumes
uniquely owned storage, so `builtins::freeze` counts the handles and refuses
when the count is not one — and a *read* of the receiver would be the second
handle, produced by the very instruction that was arranging for the count to
be taken. So `Inst::Freeze` takes the place, exactly as the interpreter runs
it inside `place.with_mut`. `push`, the other `var self` builtin, needs
nothing of the kind: it mutates through a handle, so a copy of the handle does
as well as the original.

### Return

`Function::returns` says which stack a call leaves its answer on, and it is
the same question asked of the declared return type: a function the checker
settled as answering `Int` or `Bool` answers on the scalar stack, and every
one of its returns is a `ReturnScalar`. Mixing the two in one function is a
`validate` failure, because a caller reads exactly the stack the convention
named and nothing tells it which of two a given return happened to use.

No call answers a place, and `validate` refuses a function that says it does:
a place is what a parameter can be and not what a value can be, so there is no
third return instruction for one to end in.

Either return pops its answer, truncates *all three* stacks to the frame's
bases, pops the frame, and pushes the answer onto whichever stack it came off
— which is now the caller's. Truncating to the bases is what discards the
callee's slots and its arguments together, since they are the same storage.

A whole run still answers a `Value`, because that is the language the
embedding API speaks. So the entry's arguments cross into their stacks on the
way in and a scalar answer becomes the `Value` it stands for on the way out,
at the return that has no caller. That is the only place either conversion
happens.

### Recursion and depth

`MAX_CALL_DEPTH` bounds the frame stack, and the host's own `max_call_depth`
limit is checked in the same order the interpreter checks it, with its message.
Neither is a proof obligation; both are runtime controls.

### Errors and unwinding

There is no unwinding. A `RuntimeError` returns through Rust's `?`, carrying
the span of the instruction that raised it (`Function::span_at`). A frame that
is abandoned leaves its slots on the stack until the run ends, which is
sound because the run is ending.

*Target:* this is the one place the convention is thinner than it should be. A
run that continues past a caught failure — which Cove has no construct for
today — would need the stack truncated to the failing frame's base.

### Fuel, cancellation, and safepoints

Fuel is charged, and instructions counted, once per basic block, on arriving at
the block's head: at the entry, at a taken jump, at the fall-through of one not
taken, at a callee's first instruction, at the caller's resumption after a
return, and at the fall-through of a `?`. Those are every arrival there is, and
`cove_ir::Function::block_fuel` records how far the straight line from each head
runs, so the total charged for a path is identical to what per-instruction
charging gave — which `--stats` confirms unchanged for every benchmark. An
operation whose cost is not constant is still charged proportionally where it
happens.

The counts are extents, not a partition, and they overlap. That is the part a
reader gets wrong, so it is worth saying why. Cutting the code at every head and
letting the pieces tile it loses instructions: an `if` with no `else` inside a
loop *falls* into the join its own conditional jump also targets, and a fall
announces nothing, so charging a head only where something jumps to it never
charges that join. The first attempt did exactly that, and `arith` came back
4.6% short of the instructions it had really run. An extent runs from its head
to the first instruction at or after it that control can leave from, so the
extent above a join reaches past it and pays for the walk. `validate` refuses a
table that is not that, from both sides.

Safepoints are at: entering the entry, every call, every return, a `?` that
failed, every back edge
at which enough fuel has gathered, and `SAFEPOINT_INTERVAL`, which is read where
the charge is made rather than per instruction — so what it bounds is the fuel
standing when a straight line is entered, and the work between two safepoints is
that plus one straight line, which is bounded by the length of the function's
code.

A back edge asks one question on one schedule. A loop notices any stop — a
bounded call's flag, the run's cancellation, its deadline, its fuel — at the
first back edge at which `BACK_EDGE_FUEL` (64) of fuel has gathered since the
last safepoint, so it stops within 63 fuel plus one turn rather than within
one turn; a loop whose turn charges C fuel stops within ceil(64 / C) turns.
One fact narrows what that gives up: the run's own cancellation was never on
the eager schedule to begin with, because it is read inside `Budget::safepoint`,
which the gathered schedule already gated.

The second fact that used to narrow it is gone. `self.stops` is pushed only by
`Reentry::call_until`, and this backend answered such a call without running any
Cove code, so no VM run could have a flag in that list while a loop turned.
Closures lower now, so a `clock.timeout` around a Cove callback puts a flag
there and the callback's own loops are what it has to stop — on this schedule,
within 63 fuel plus one turn of noticing. That is the same bound the run's
cancellation has always had, and it is now the bound a bounded call has too.

**The 63 fuel applies to a loop that calls nothing.** A call is an
unconditional safepoint, so a loop whose body reaches any Cove function — a
method, a `lock`, an operator that lowered to a call — is asked at every turn
and stops at the first one after the flag goes up. The gathered schedule is
what a loop that computes and never calls gets, which is `benches/arith`'s
shape and is why the constant exists at all.

### What each stop mode may run past it, and what it may leave behind

Everything above says where the checks are. What a program can be told is a
different question, and it is
[issue #120](https://github.com/myuon/cove/issues/120)'s: for each way a run
can be stopped, how much work may still happen, and what a host or a surviving
caller may see afterwards.
`crates/cove-runtime/tests/responsiveness.rs` measures every figure below and
asserts it as a maximum. Nothing here is a number a comment claims.

**Fuel is a backend-specific work budget, and only its *effect* is strict.**
A run that passes its limit stops at the safepoint that discovers it and never
later; that much holds on both backends and is what ADR 0019 said fuel exists
for. What does not hold is that the limit bounds the work, in two different
directions at once. A run may overspend its limit — by one gathering plus one
turn, which for a loop charging 13 fuel a turn is a bound of 77 and measures 9
to 44 over three limits — because fuel is counted where it is charged and
compared where it is spent. And a run may be charged for work it did not do,
which is the other question:

**A block is refused entire, and a prefix that would have fitted does not
run.** The charge happens on arriving at a head, and the safepoint that may
refuse happens after the charge, so a straight line whose whole extent exceeds
what is left is stopped before its first instruction. Four hundred assignments
in a row lower to one extent of 1,606 instructions; with a fuel limit of 803
the VM stops, executes none of them, and reports `fuel_spent` of 1,606. The
tree walk answers `Ok(400)` for the same program under the same limit, because
its schedule is calls, back edges and `await`, and a straight line reaches none
of the three — it charges nothing for that line at all. So neither backend
stops in the middle of a straight line. One refuses the whole of it and the
other never measures it, which is a difference in outcome and not only in
`fuel_spent`. [ADR 0024](adr/0024-a-stop-is-a-bound-not-a-point.md) is where
that is decided rather than discovered.

**Every stop mode has a maximum, and they are not all the same maximum.**
`G` below is one gathering — `BACK_EDGE_FUEL` on the VM, and one safepoint's
`SAFEPOINT_FUEL` on the tree walk, which gathers nothing — and `T` is what one
turn of the loop in question charges.

| stop | measured at | maximum after it becomes true |
| ---- | ----------- | ----------------------------- |
| the run's cancellation | `Budget::safepoint`, and every Host call | `G + T` of Cove work; no Host effect |
| a task's own cancellation | `Vm::safepoint`, and every Host call | `G + T` of Cove work; no Host effect |
| a bounded call's flag | `Vm::safepoint`, and every Host call | `G + T` of Cove work; no Host effect |
| the deadline, with no fuel limit | every safepoint, and every Host call | one safepoint; the VM stops before its first instruction |
| the deadline, beside a fuel limit | every `DEADLINE_CHECK_INTERVAL`th safepoint | `64 × (G + T)` |
| fuel | a safepoint, and nowhere else | `G + T` of overspend, and one refused extent |
| `max_host_calls` | every Host call, before it | nothing: the call that would pass it does not happen |
| `max_call_depth` | every call | nothing: the call that would pass it does not happen |
| the concurrency limit | every `spawn`, before the thread | nothing: the thread is not taken |

The deadline's two rows are the same code reading two schedules.
`Budget::safepoint` reads the clock at every safepoint when no fuel limit is
set, because then nothing else bounds the run; with one set it reads every
`DEADLINE_CHECK_INTERVAL`th, because `Instant::now` is a system call and fuel
is already bounding the loop. Measured on a loop with no Host call in it — a
Host call would be stopped by the clock the boundary reads, rather than on the
schedule under test — that is 0 and 4,099 fuel on the VM, against a bound of
4,928, and 10 and 640 on the tree walk against 1,280.

**A Host call is not a safepoint, and it is not meant to become one.** What
stands in front of it is `Budget::charge_host_call`, which refuses a call from
a run that was cancelled, past its deadline, or over `max_host_calls`, and
`crate::interp::stopped_here`, which refuses one from a cancelled task or from
inside a bounded call that has been asked to stop. Between them that is every
stop that is a *flag*. Fuel is not one, and is the one thing a Host call does
not ask about, because a flag costs an atomic load and a budget has to be
measured — measuring is exactly what charging by the block exists to stop
doing, and putting it back at every Host call would put it back on
`benches/hostheavy`'s path for a bound that `max_host_calls` already states
exactly. So the honest sentence is: **`max_host_calls` bounds effects, fuel
bounds work, and the deadline bounds time**, and each is checked where it can
be.

That has a consequence worth stating plainly rather than leaving to be
discovered. Every Host call in one straight line is made before the charge
that line incurred is measured. Forty Host calls with no branch between them
all happen under a fuel limit of one on the VM, which then stops at the return
having charged 286 against a budget of 1. The tree walk reaches no safepoint
between the entry and the return either, so any limit that lets it in at all
lets all forty through and it answers. The bound is `SAFEPOINT_INTERVAL` of
standing fuel plus one extent — finite, known before the run, and much larger
than zero.

**What a stop may leave behind.** Three things, and the third is the one a
program can be written around.

*Host effects already performed stay performed.* Nothing is undone, and the
Host API schema's `Effect::IrreversibleWrite` is the field that says which
ones could not be. What the contract promises is only that no *further* effect
follows a raised flag, which is measured at zero on both backends for a
cancelled run, a cancelled task, and a stopped bounded call.

*No value is half written.* A stop is taken at a safepoint, and a safepoint
stands between two instructions, so there is no torn struct and no half-built
array. Cove's value semantics then decide the rest: a stopped call's writes to
its own locals go with its frame, and what a surviving caller sees is only
writes to storage they share — a `Shared` cell, a `Vector` the caller also
holds, a file. Those are whole turns of a loop. A counter in a `Shared` cell,
incremented once a turn in a body stopped from turn three, comes back holding
exactly four on both backends. Four rather than more because `lock` is a call
and a call is an unconditional safepoint, so that loop is asked every turn and
the gathered back-edge schedule never comes into it.

*A stop is at an expression, not at a statement.* A call is a safepoint, so
`f(a) + g(b)` can stop between the two calls, with `f`'s effects made, `g`'s
not made, and the addition never performed. There is no statement-level
atomicity in either backend and none is claimed.

**Pending fuel is never lost.** The VM charges a block at a time and spends
what it has charged at a safepoint, so every exit that reaches no further
safepoint is an exit its last charge could go out with. Two did.
`Budget::safepoint` read the cancellation flag before adding the fuel it had
been handed, and returned without it; and a run or a task that ended by
raising, by being stopped, or by having its callback abandoned reached no
safepoint at all, so `Vm::spend_pending_fuel` now runs where a run ends and
where a task's thread ends. The invariant that catches both is that a VM run's
`fuel_spent` is never below the instructions it charged for, and it is
asserted over the return path, the `?` path, a raised error, a cancelled run,
an exhausted budget, an abandoned re-entrant callback, and a cancelled task's
own thread. Before the fix a run that divided by zero after fifty-six
instructions reported nought.

**Both backends are held to the same bound and not to the same point.** They
stop at different source operations for the same program under the same
limits, they spend different fuel for the same work, and under a fuel limit
one can stop where the other answers. What they are held to is the shape of
the table above: a maximum in each backend's own units, the same stop reported
as the same `RunOutcome` in the same words, and no Host effect after a raised
flag on either. ADR 0024 decides that, and
`crates/cove-runtime/tests/responsiveness.rs` runs every case on both.

### Host calls and reentry

A Host call goes through the same `HostRegistry` the interpreter uses, so the
grant check, the budget charge, the trace event, and the wait accounting are
the same code and cannot drift. One thing the registry cannot do is asked
either side of it, in `crate::interp::stopped_here`, which both backends call
at the same point: a `Budget` is shared by every task of a run, so it holds the
run's cancellation and not this task's, and not the flag of a bounded call this
thread is inside. Those two are read here, before the call is dispatched, so
that a cancelled task and a stopped `clock.timeout` body perform no further
effect. "What each stop mode may run past it" is where that bound is stated
and why fuel is not on the same schedule.

Reentry — a host running a Cove closure, and
the same thing a higher-order builtin such as `Result.mapError` does — enters
the dispatch loop again rather than jumping inside the one that is running,
because the instruction that made the host call has not finished: its operands
are on the stacks below and its frame's slots are live.

**The convention is that a re-entrant call opens its frame at the top of the
three stacks as they stand, and leaves them exactly as it found them.** The
arguments are pushed as a caller's would be and become the callee's first
slots; the return truncates them away and the answer is handed back in Rust
rather than left standing. The frame stack grows above the frame the
interrupted instruction belongs to and comes back down to it, which is what
`Vm::execute`'s `floor` parameter is: the loop answers when the frame stack
falls back to the depth it started at, instead of when it empties.

**A failure leaves nothing behind**, and that is the one place this differs
from the outer run. There, an abandoned frame's slots stay on the stack until
the run ends, which is sound because the run is ending; here it is not, because
a host may catch what the callback failed with and carry on — `clock.timeout`
is the one that does. So the three stacks and the frame stack are restored to
what they were, and a host that continues continues onto the stacks it
interrupted.

What this relies on is that the outer loop's own state survives it. The
dispatch loop keeps the running function, its code, its block table and its
frame in locals; a `Frame` is five words copied out of the frame stack and a
`Place` is an index rather than a pointer, so a nested run that pushes and pops
frames, and that may reallocate any of the three `Vec`s, leaves every one of
those still meaning what it meant. That is the same property that makes a place
survive an ordinary call, read from the other side.

Everything the loop accounts is still accounted, because it *is* the loop: fuel
per block, the depth limit and the host's own `max_call_depth` through
`Vm::enter`, and every safepoint the callee reaches asking what a safepoint
asks. One safepoint is paid twice — `Vm::enter` takes one because a call is
one, and `Vm::execute` takes one on entering the frame it was handed — which
costs a lock and changes no answer.

### Tasks, and the second VM each one gets

ADR 0008 runs a spawned task on a thread of its own and gives it an evaluator
of its own to run it with. Here that is a second `Vm`, built on the new thread
by `Vm::for_task`, over the same `Runtime` and the same `cove_ir::Program`.
Everything else it works with is built there rather than carried across: three
stacks, a frame stack, a heap, and the `Value` a constant stands for. None of
those could cross — every one is `Rc`-based or is a `Vec` this thread owns —
and none of them needs to.

**What crosses is the program, and the body.** The program had to become
shareable for any of this to work, and that was the one real obstacle. A
lowered closure's body is a `FunctionId`, which means a position in one
program's `functions`; lowering the program a second time on the receiving
thread would hand it ids that happened to line up, which is not an invariant
anything states. So the IR holds every string as an `Arc<str>`, a `Program` is
`Send + Sync`, and a `Vm` takes a share of the handle rather than a bare
reference. A `FunctionId` then means the same function on the far side because
it indexes the same `functions`. The one place that costs anything is the
boundary where an `Arc<str>` becomes the `Rc<str>` a `Value` carries: the
constants are turned into their values once per VM, in `Vm::constants`, where
they used to be turned into them once per load.

The body crosses as a `cove_runtime::task::Transfer`, which *is* the
task-safety rule rather than a second statement of it: the Card lets a value
cross exactly when copying it is the whole of transferring it, which is also
what a thread requires. That walk is the interpreter's, unchanged, and so is
everything else either backend decides about a task — what a scope does with a
child that failed, what a `spawn` charges against the concurrency limit, what a
trace records, how large a stack the new thread gets. `cove_runtime::task`
holds all of it behind a trait naming the two things that genuinely differ:
which evaluator runs a body, and which timing contexts a wait is charged
against. A concurrency rule stated twice would be one that drifts, and a
disagreement between the backends about concurrency is the kind that does not
reproduce.

Six instructions, in one dispatch arm calling an `#[inline(never)]` helper:
`enter-scope`, `leave-scope`, `cancel-scope`, `spawn`, `await`, `cancel`. What
is written in the VM is the stack discipline and one thing more, which is the
thing a stack machine has to answer and a tree walk does not.

**A scope has to be left however the frame that opened it goes away.** The tree
walk gets this for free: `Interpreter::eval_scope` wraps the body, so a
`return`, a failing `?`, a `break`, and a raised error all pass back through it
and reach `leave_scope`. Here they do not. A `return` is an instruction that
pops a frame; a `?` that failed leaves the frame from inside `Inst::Try`; a
`break` is a jump. So the scopes a VM has open are a stack of its own, each
recorded with the frame depth it was entered at, and the two ways out are split
by which of them the exit crosses:

- an exit that leaves the **frame** is noticed by `Vm::leave`, which is the one
  place a frame is popped, and which cancels whatever that frame had open. The
  same call covers a run that ended by raising and a re-entrant callback a host
  abandoned, at the two other places the stacks are unwound.
- an exit that stays **inside** the frame is a `break` or a `continue`, and the
  lowering emits a `cancel-scope` for each scope it leaves — the same shape as
  the `pop`, `scalar-pop` and `place-pop` a `break` out of a half-evaluated
  expression already emits.

`leave-scope` itself needs no arm of its own for the failure a child produces,
because the language already has the construct that means it. A child whose
value is `Err(...)` returns that failure from the enclosing call, which is what
`?` does — so `leave-scope` answers a `Result` and the lowering writes a `try`
after it. A child that *raised* is not a value and never reaches that `try`; it
propagates as the error it is.

One shape is refused and is a wall rather than unfinished work. That return of
a child's failure happens whatever the enclosing function declared, so
`fn f() -> Int { scope s { ... } }` answers `Err(boom)` on the oracle. A
function the checker settled as answering `Int` or `Bool` returns on the scalar
stack and every one of its returns is a `return-scalar`; there is no stack for
that failure to come back on. The lowering refuses such a scope rather than
approximating it.

Fuel, the deadline, the run's cancellation, the task's own cancellation, the
call-depth limit and the trace are accounted inside a task exactly as outside
one, because the same `Vm` code accounts them: a task's VM reaches the run's
one budget through the same `HostRegistry`, and `Vm::safepoint` reads the
task's flag beside the bounded calls this thread is inside.
`tests/e2e:fail_max_tasks` is in the corpus because a run's concurrency limit
has to stop it identically on both backends, and it does.

### An `async fn` answers a task, and the frame is where that happens

ADR 0008 gives a thread to `spawn` and not to every `async fn`. So an
`async fn` runs its body at the call site like any other function, and what a
call to one answers is a handle that is already settled: the body has run
whether or not anything awaits it, and only `await` produces the value.
`Interpreter::invoke` says that in one line, by wrapping the result of the
whole call in `Task::settled`.

The VM cannot wrap at the call site, and the reason is the same one that fixes
a closure's calling convention: nothing at a `call-value` knows which function
it will reach, and an `async fn` used as a value — `f: async fn(Int) ->
Result<Int, Error>` — is called through one. So `cove_ir::Function` carries
`answers_a_task`, the call sites that open a frame read it, and `Vm::leave`
wraps where the frame closes. That catches all three ways a body can end: a
`return`, the last instruction, and a `?` that failed, which leaves the frame
from inside `Inst::Try` and reaches no return instruction at all.

Which frames are async is a `Vec<usize>` of depths beside the frame stack
rather than a field on `Frame`. A frame is five words copied into a local of
the dispatch loop and read by every instruction that addresses a slot, so a
sixth field is register pressure in the loop `benches/arith` spends its run in
— the reason `return_pc` became a `u32` when the place model landed. What
`leave` pays instead is one length check on a vector that is empty in every
program with no `async fn` in it, which is the same shape the open scopes use.

Two things follow from a task being a value. An `async fn` answers on the value
stack whatever its declared return type was, so `async fn f() -> Int` is
`SlotKind::Value` and its call site expects a value: a `Task<Int>` is not an
`Int`, and only `await` makes one. And a lowered `async` lambda's closure value
carries `is_async`, read off the lowered function, so a host that receives one
reads the same field it would read off a closure the interpreter built.

The entry is the one caller that does not `await`. An async entry hands back a
handle the host cannot settle, so `Interpreter::enter` settles it there and
`Vm::run` does the same, through the same `task::settle`.

### `Shared`, and the one closure that is not called through a value

`Shared` is the other half of ADR 0008, and the reason the type exists: it is
the one value that crosses a task boundary by sharing rather than by copying.
`Shared(value)` is an ordinary `make-builtin` — `builtins::call_constructor`
already refuses a payload that may not cross — and `lock` is one instruction
over `SharedCell::lock`, which holds the cell for the whole of the closure,
converts its contents into the locking task's own `Value` on the way in, and
converts back whatever the closure left in it. All of that is the oracle's,
unchanged.

What is not is the call itself. A closure written `fn(var value)` does not
receive a copy of the cell's contents; it *names* them, so that
`value.record(false)` decides what the cell holds afterwards. Every argument of
a `call-value` travels on the value stack and a place cannot, which is why a
`var` parameter on a lambda was refused, and the note refusing it named
`shared.lock(fn(var value) { ... })` as the one place such a lambda is written.

So `lock` makes the call itself. It stands the contents in a value slot of the
*locking* frame, hands the closure a place rooted there, and reads that slot
back when the closure returns — which is `Interpreter::call_shared_method`'s
`Place::binding(value, true)` and its `place.read` afterwards, said as a stack
discipline. The slot survives the call because `Vm::leave` truncates to the
callee's base, which is above it. A closure written without `var` takes its
argument on the value stack like any other and the cell keeps what it had,
which is what the oracle does with one; which of the two a closure is comes off
its own `params` rather than being assumed.

That is the first parameter of a lowered function that is a place without the
declaration having said so, and it moves one number. The captures of a closure
stand in the value slots straight after its parameters, and a place parameter
takes no value slot — so `cove_ir::Function::capture_base` says where they
begin, and `load-capture` reads that rather than `arity`. The two are the same
number for every closure but this one.

What keeps such a closure away from a `call-value` is not a run-time check. The
lowering builds one only as the direct argument of the `lock` that consumes it,
where the very next instruction takes it off the stack, so it never becomes a
value the program can name. `validate` allows exactly that shape — a first
parameter that is a place, everything after it on the value stack — and refuses
the rest. The cost is that a `lock` whose closure is *not* written at the call
is refused, which is narrower than the oracle, and no program in the corpus
writes one.

### Trace

`EntryEnter` and `EntryExit` are recorded with the CPU/wait split. Instructions
are not traced: an instruction-level trace would be a different artifact and
ADR 0019 does not propose one.

`TaskSpawned`, `TaskCompleted` and `TaskCancelled` are recorded where the
interpreter records them, because they are recorded by the same code: at the
`spawn` before the thread exists, on the task's own thread as it ends, and at
the join that learns a cancellation actually stopped something.

### What a trace says on both backends, and what it does not

`crates/cove-cli/tests/differential.rs` now records a trace of every corpus
case on both backends and compares the two files, so the question of which
parts of a trace are the program's and which are the backend's has an answer
made of measurements rather than of expectations. Most of a trace is the
program's. The entry's module and function, every host call's task, module,
operation, capability, grant, arguments and outcome, each task's id, parent
and scope, how the run ended and with what message, and what the run allocated
are all compared exactly and all agree over the ninety-three cases that lower.
Two of those were expected to need normalizing and did not. Task ids agree
because both backends draw them from the one counter the `Runtime` holds, so
there is no renumbering to undo; and no trace event carries `fuel_spent`,
which ADR 0019 makes backend-specific, so the one figure that could not have
been compared never reaches the artifact.

What a trace does not say the same way on both backends is when a collection
happened, what it found live, and in what order two tasks' events were
written. The first two are the collector's, and the section on the heap
figures below is where they are argued; the practical consequence is that
`heap_collected` is dropped from the comparison whole, because both what it
reports and where it stands in the sequence move with the safepoint — the same
program collects after 64 allocations on the interpreter and after 66 on the
VM, and runs one of its collections a host call earlier here than there. What
survives is `heap_summary`, whose `allocated`, `allocated_bytes` and
`collections` are compared and agree, and whose `live_bytes` and `peak_bytes`
are not, because a live set is a question about a root set and the two
backends have different ones.

That `collections` agrees is worth recording, because this document predicted
it would not: a run that allocates identically, it says below, can collect five
times on one backend and six on the other. Over the whole corpus it never
does. The prediction is still right about the mechanism and wrong about this
corpus — the threshold counts allocations, the two backends allocate the same
objects, and the VM's overshoot therefore moves the boundary between two
collections without removing a boundary. Losing one would take an overshoot
large enough to swallow a whole threshold between two safepoints, and nothing
written so far allocates that fast. The figure is compared rather than trusted
now, so if something ever does, a test says so.

The ordering is not the collector's but ADR 0008's. Every spawned task runs on
a thread of its own and every event goes to the one sink the run shares, so
which of two tasks appears first in the file is which of two threads reached
the lock first. The interpreter alone wrote a differently interleaved trace of
`tests/e2e:gc_tasks` on each of thirty consecutive runs, and wrote the same
trace every time once the file was read per task. So the comparison reads per
task, which drops the interleaving and keeps every task's own order, and this
is a fact about the runtime rather than about either backend: a comparison
that failed on the interleaving would fail the oracle against itself.

One thing is given up rather than normalized, and it is worth naming. A task
that a scope cancels may or may not have reached its next host call before the
cancellation landed, which is the scheduler's answer and not the program's:
`tests/e2e:fail_max_tasks` records `task_completed` for its first task in three
of twenty runs on the interpreter and `task_cancelled` in the other seventeen,
and `examples:callbacks` flips the same way on the VM, losing the `clock.every`
call the cancelled task would have made along with it. Such a task is compared
only by the `task_spawned` that made it, so a backend that always cancelled
where the other always completed would not be caught by this comparison. What
would catch it is everything the task's work reaches — what it printed, what
it left in the filesystem, what the entry answered — all of which the same
harness compares whether or not a trace was written. The two cases the rule
applies to are named in the harness's summary on every run, so the cost is
printed rather than quiet.

## What the target changes

### Compact typed slots

A slot whose type the checker proved holds that type's representation directly:
an `Int` is an integer word, a `Bool` is a word, a heap value is a reference the
VM owns. Only a slot whose type is genuinely dynamic carries a tag.

The point is negative rather than positive: `arith`'s loop should not clone,
drop, or match a general `Value` to add two integers.

**Chosen, by the measurement below: separate typed storage.** A slot the
checker settled as `Int` or `Bool` lives in a second stack of `i64` beside
the value stack, and a frame is a window into both. It was the smaller of the
two changes to try — nothing about the dynamic representation moves, so
everything the VM already ran keeps running unchanged — and on `arith` it
removed the whole of the cost it was aimed at.

What the dynamic representation itself is — a tag beside a payload, an
aligned tagged pointer, NaN boxing — was the question left open here. It has
since been audited and measured, and it is large enough to have its own
section: "The value representation, audited", below.

### Frame metadata

The frame layout is where precise GC roots come from. A function's metadata
should carry, per slot, whether it holds a reference — so a collection scans
the slots that can hold one rather than inspecting every scalar as a `Value`.

That metadata exists now, and it turned out to need no per-slot list at all:
`cove_ir::Function::value_frame_size` and `scalar_frame_size` number the two
stacks separately, so a scalar slot is never a number in the value stack's
space to begin with. A frame's whole value window is its root set, with
nothing inside it to skip, because a scalar slot holds no reference by
construction and was never counted there. It is derived from the checker's
facts rather than invented, which is also what a future JIT would need.

The place window did not change that. A place holds an index into the value
stack and a path of field positions, so whatever it reaches is already
reachable from the value stack's own window, and `place_frame_size` is neither
a root set nor a hole in one. What a *moving* collector would have to know is
that a place is an index into the storage it is moving, which is a different
statement and belongs with the collector that makes it.

### The root set, now that something reads it

Everything above used to be a statement about where the roots *are* rather
than code that reads them, because nothing read them: this backend allocated
and collected nothing, which issue #119 recorded as the hard blocker on making
it the default. `cove_runtime::vm::StackRoots` is now that code, and what it
reads is what the paragraphs above said it would.

It walks two things. The value stack up to its current length, which is every
frame's slots and every frame's operands at once — they share one vector, so
there is nothing to slice per frame and nothing to skip inside one, and a
closure's captures are value slots that the call copied into the window and so
are in it already. And the task scopes the VM has entered and not left, which
is the one thing a `Vm` holds that need not be reachable from a slot; a
scope's value *is* also an ordinary slot of the frame that opened it, so
walking the list is very nearly redundant, but "very nearly" is not an
invariant anything enforces and the list is empty in every program that writes
no `scope`.

Nothing else the struct holds is a `Value` or contains one, and the type's own
documentation goes through the fields one at a time rather than asserting it,
because a root missed there is a use-after-sweep of a Cove value and the next
reader needs to see that the list was checked rather than guessed. The scalar
stack is `i64`s. The place stack is indices. The constant pool holds values,
and is deliberately *not* walked: `constant` builds a unit, a boolean, an
integer, a float, a duration or a string and nothing else, so no entry can
reach a `Vector`; walking it would be safe but would put every constant string
into this backend's live-bytes figure and not the other's, which is exactly
the kind of difference #119 exists to remove.

### Collection is non-moving

Nothing is relocated. That is what the Language Card says — "a precise,
non-moving mark-and-sweep collector" — and what ADR 0011 narrows to a heap per
task; this backend does not change it, and the collector it calls is the same
`cove_runtime::heap` the interpreter calls, so there was never a second answer
to give.

It is worth saying what a moving collector would owe, because the place model
already half-states it and half is not enough. Moving what a *slot* holds
would cost a place nothing: a place names the slot, not the value, so a slot
rewritten in place is a slot the index still finds. Moving the *stack* would
be another matter, and it is the one this backend would have to pay for.
Every place standing in the place stack is an absolute index into the value
stack, and every place a frame handed a callee is one too, so a collector that
compacted or relocated the value stack would have to rewrite each of them, and
would have to be able to find them — which the place stack makes possible and
which nothing else about the arrangement guarantees. The paths would survive
untouched, since a field position is a position in a struct rather than an
address. This is written down here because it is a bill that has not been
paid rather than a problem that has been solved.

### Where a collection happens, and why it is safe there

At safepoints, and nowhere else. The list is unchanged — entering the entry,
every call, every return, every back edge with enough fuel gathered, the
per-block charge past `SAFEPOINT_INTERVAL`, an `await`, and a `?` that failed
— with the heap asked last, after the stops, because a run that is ending has
no use for a collection.

The rule that makes those points safe is not "every live value is on a stack",
and it is worth being exact about that, because the obvious reading of the
frame convention suggests it and the obvious reading is wrong. During an
instruction the dispatch loop holds `Value`s in Rust locals — a popped
receiver, the `Vec<Value>` `Vm::take` drained for a host call, the failure a
`?` is about to leave a frame with — and none of those is on a stack any walk
reaches. They are safe because the collector counts references: for every
shared allocation it can see, it compares the references it can reach against
`Rc::strong_count`, and a shortfall means a reference is held somewhere it
cannot read, which makes that allocation and everything it holds a root. A
Rust local *is* a reference. So a value the loop has taken off the stack is a
value whose count does not add up, and is rooted for that reason.

This is the same rule that roots the interpreter's evaluator temporaries;
ADR 0011 calls them "values being evaluated" and neither backend would be
sound without it. What it changes is which question a safepoint has to answer.
Not "is everything on the stack" — it need not be — but "is anything walked
twice", since a reference counted twice conceals exactly the shortfall the
rule depends on. `Vm::collect_if_due` goes through the safepoints one at a
time and says what each holds; the two that are worth naming here are the
return, which takes its safepoint *before* popping the answer so the answer is
still an operand, and the host call, where the arguments and the receiver are
in Rust locals below whatever frames a re-entrant callback pushes above them.

### What the heap figures mean, on both backends

The same thing, which they did not before. A VM heap was never swept, so what
it reported was everything the run had ever allocated; and the entry's heap
was never retired at all, so the totals a `Vm` answered with came only from
spawned tasks — `cove run --backend vm --stats` reported no allocation for a
program with no `spawn`, and `cove-bench`'s `heap_peak_bytes` was zero for
every VM row because it is only written inside a collection. All three are
gone: a safepoint collects, `Vm::retire_heap` sweeps once more before folding
the counters, and `Vm::run` retires the entry's heap and emits the
`heap_summary` event that `Interpreter::enter` has always emitted.

One of those three is fixed without being visible where it was named, and
that is worth recording rather than claiming more than was measured.
`heap_peak_bytes` is still zero on every `cove-bench` row — but it is zero on
the *interpreter's* rows too, and was before this change, because no program
under `benches/` builds a `Vector` at all. The suite measures dispatch,
arithmetic, field reads and host calls, and allocates nothing the collector
manages. So the figure is verified on the corpus instead: `cove run --stats`
over `tests/e2e:gc_*` reports the same allocation, the same bytes, and the
same live set on both backends, where before the VM reported the totals of a
heap nobody had swept.

Two figures still differ between the backends, and they differ for reasons
that are worth keeping rather than papering over.

**How many collections a run took.** A collection happens at a safepoint where
enough has been allocated since the last one, and the two backends put
safepoints in different places: the interpreter takes one at every loop turn,
this one at the first back edge with `BACK_EDGE_FUEL` gathered. So the VM asks
the question less often and can overshoot the threshold further before it
asks, and a run that allocates identically can collect five times here and six
times there. That is a schedule, not a semantics, and the differential tests
compare what a run allocated and what it was left holding rather than how many
sweeps it took to get there.

That last sentence is now less true than it was, in the direction nobody
expected. The differential compares the number of sweeps too, because when the
traces were first compared the number turned out to agree on every case in the
corpus; what it still does not compare is where each sweep fell, which is the
part of the schedule that does move. "What a trace says on both backends, and
what it does not", above, is why.

**The peak live set.** This one differs by more than a margin, and in a
direction that is easy to misread as a leak. A `var v` declared inside a loop
body is out of the interpreter's environment chain by the time that turn's
safepoint is reached, and is still the VM frame's slot until something writes
the slot again — a frame's window is sized once, per function, rather than
opened and closed per block. So a churn loop reports a peak of zero on the
oracle and of one vector here. Both are true statements about what was live
when the collection ran; what they are not is the same statement.

### What the collector costs

A collector that runs costs something, and the suite above cannot say what,
for the reason just given: nothing under `benches/` allocates. Every row moved
by less than 1.7% against `92e4569`, measured interleaved, three iterations
per round and three rounds; the only figure outside that is `startup` at 4.0%
on the AST backend and 3.2% here, which is process exec time and moves by that
much between two runs of the same binary. `size_of::<Inst>()` is still 16, a
`Frame` is still 32 bytes, and `Vm::execute`'s `match` has the same arms it
had — nothing in the dispatch loop changed, which is why nothing in the
dispatch loop moved.

So the cost was measured on a program written to pay it: three hundred
thousand cycles built and abandoned, and nothing else. Median of five,
interleaved, release, same machine.

```text
                        execute      peak RSS   collections   reclaimed
VM at 92e4569           140.8 ms      103 MB             0           0
VM here                 177.6 ms      3.3 MB         4,688     300,000
interpreter (before)      494 ms                   unchanged
interpreter (here)        502 ms                   unchanged
```

Twenty-six percent, and 90.6 ms of the 177.6 is pause time the heap reports
itself, which is the honest way to read it: over half the added cost is the
mark and the sweep and the rest is the safepoint asking. The comparison is not
like for like and should not be read as one — the faster of the two numbers
belongs to a run that ended holding three hundred thousand objects it could
not reach, which is a thirty-one-fold difference in peak memory and is the
whole reason for the change. The interpreter's 1.6% is noise; nothing on its
path moved.

The VM with a collector is still 2.8x faster than the interpreter on that
program.

### A VM-owned heap

Today a heap value is a Rust object graph reached through `Rc`, and the target
has been a layout the VM owns, reached through a stable handle — goals 4 and 5
of #116. Before building it, the two benchmarks that allocate were profiled on
the VM as it stands, release build with symbols, self time. (The `chars`
profile at the top of this document is the prototype's; these are the current
one's.)

```text
benches/chars                    benches/arrayget
34.9%  <malloc>                  41.1%  <malloc>
27.6%  Vm::execute               22.1%  Vm::execute
 9.6%  builtins::call_method      8.7%  builtins::call_method
 4.8%  drop_in_place<Value>       3.5%  Vec construction
 3.2%  Vec construction           3.0%  drop_in_place<Value>
 1.6%  Value::clone               2.6%  Value::some
 1.6%  Value::some                2.2%  Value::clone
```

Allocation is the largest single cost in both, which is the complete reverse of
`arith`, where dispatch is 85% and malloc is zero. But a profile says *where*,
and where is not a heap layout. Two specific sites account for it.

**A `Some(x)` costs two allocations.** `Value::some` builds
`Value::Enum(Box::new(EnumValue { type_name, case, payload: vec![value] }))` —
a `Box` for the `EnumValue` and a `Vec` for a payload of one — plus two lookups
through a thread-local cache for the two names. `benches/arrayget`'s own doc
comment already says why that matters: "`Option` is built here more than
anywhere else in an ordinary program, because this is how every indexed read
answers." Its loop calls `get` and `unwrapOr` two million times.

**A builtin call builds an argument vector.** `Vm::take` is
`self.stack.drain(at..).collect()`, one `Vec<Value>` allocated and dropped per
builtin call. ADR 0019 named that cost in the tree walk and it is still here in
the VM. `arrayget` pays two of them a turn.

Four allocations per iteration of a loop that reads an array element, which is
eight million of them for the run.

A VM-owned heap would give the VM a layout it controls, and it is still the
right long-term shape for #116's goals 4 and 5. But it is not what these
numbers ask for next, and building it first would be answering a question the
profile did not ask. What the profile asks for is that a two-word `Some` stop
costing two allocations and two name lookups, and that a builtin call stop
allocating a vector in order to hand a function three values. Both are bounded
changes inside the representation that already exists.

It is worth naming what a VM-owned heap would *not* fix, so this is not
over-read in the other direction. It would not remove the `Option` allocation
on its own, because that allocation is caused by the shape of `EnumValue`
rather than by who owns the memory underneath it: a `Some` built out of a `Box`
and a one-element `Vec` costs two allocations whoever is holding the arena.

So the heap design stays open, and it should be decided after those two rather
than before them. What is still allocating once they are gone is the evidence a
heap layout would actually be answering.

## The slice, and the gate

The smallest end-to-end thing worth measuring is `benches/arith` running with
typed integer and boolean instructions over compact scalar slots, with no
general `Value` operation in its hot loop, and with fuel, cancellation, trace,
diagnostics, and depth accounting unchanged — and agreeing with the AST
backend.

It is measured against both the AST backend and the prototype VM, on the same
machine and build, by: wall time, dynamic instruction count, time in dispatch,
time in clone and drop, and allocation behaviour.

**The gate:** expand the architecture across the language only if that slice
shows a material improvement attributable to it, or if profiling names a
bounded follow-up that would. If it does not, stop and record which cost
dominates instead.

That gate is the reason this document exists before the code does.

## The slice, measured

Built and measured on one machine, `--release`, `cove-bench --iterations 15`,
mean wall time:

```text
cpu    Intel(R) Core(TM) i7-10700K CPU @ 3.80GHz
os     macOS 26.5.2 (x86_64)
rustc  1.93.1 (01f6ddf75 2026-02-11)
```

"Prototype" is the VM at the commit before typed slots; "typed" is the VM with
them. The three benchmarks below are the slice and the two controls it does not
target.

| bench    | AST     | prototype VM | typed VM | typed vs prototype | typed vs AST |
| -------- | ------- | ------------ | -------- | ------------------ | ------------ |
| `arith`  |  458.6 ms |  263.2 ms |  106.5 ms | **2.47×** | 4.37× |
| `field`  |  867.0 ms |  649.6 ms |  604.7 ms | 1.07× | 1.45× |
| `method` | 2882.4 ms | 1047.7 ms | 1007.9 ms | 1.04× | 2.93× |

The whole suite as it stands now, `cove-bench --iterations 15`, mean wall time
on the same machine:

| bench       | AST       | VM       | ratio |
| ----------- | --------: | -------: | ----: |
| `pure`      |   15.2 ms |   2.3 ms | **6.62×** |
| `call`      | 1479.5 ms | 254.5 ms | 5.81× |
| `arith`     |  429.5 ms |  86.5 ms | 4.97× |
| `method`    | 2792.2 ms | 815.7 ms | 3.42× |
| `chars`     | 1744.9 ms | 819.8 ms | 2.13× |
| `arrayget`  | 1355.9 ms | 668.7 ms | 2.03× |
| `field`     |  846.2 ms | 433.1 ms | 1.95× |
| `hostheavy` |    4.8 ms |   3.9 ms | 1.25× |

That is a re-measurement, and how much of it to believe is bounded by its own
control. The change under "One acquisition of three was buying nothing"
below moved `pure`, `call` and `method`, and the previous reading of this
table was 2.8, 279.4 and 912.9 ms against 2.3, 254.5 and 815.7 here. But the
AST column moved too, by −3.0% to −9.5%, on code nothing has touched — so
somewhere between three and nine points of every VM figure here belongs to
the session rather than to the change, and the attribution that does not have
that problem is the before-and-after in that section, measured through one
binary in one sitting.

This table held worse numbers for a while, and the difference was real
rather than drift. When closures, dynamic dispatch and tasks had been lowered
the VM was 3.5% to 19% slower than at `c8450e7` — `pure` 19%, `arith` 16.5%,
`call` 14.5%, and the collection-shaped ones least — while the AST column
moved between −1.7% and +2.7%, which is the control that says the machine did
not change under them. And the instruction counts did not move at all:
`arith` runs the same 31,142,877 instructions it ran before, `field`
47,428,595, `method` 59,428,598. **The same instructions ran and they got
slower**, which is the signature of a dispatch loop carrying more than it
uses. That is the cost the place model paid once, measured and brought back
down; three further capabilities each measured within their own noise and
together they did not, which is
[issue #126](https://github.com/myuon/cove/issues/126) and the process gap it
also names — a per-change gate against the immediate parent cannot see an
accumulation.

Most of it has since been given back, and where it was is not where it was
looked for: the dispatch body's size was the minority of it and an `Inst`
kept alive across the dispatch was the majority. "What the three capabilities
after it cost, and where the cost actually was", below, has the attribution
and the two variants that did not work. What is left against `c8450e7` is
`call` at about +5%, which is not attributed to anything, and everything else
at or under the ±6% band `arith` is known to move in for layout alone.

`hostheavy` is the floor and should be: it is host dispatch, which both
backends reach through the same registry, so there is nothing there for an
execution model to win.

Dynamic instruction count, from `cove run --stats`:

| bench    | prototype  | typed      |
| -------- | ---------- | ---------- |
| `arith`  | 31,142,876 | 31,142,877 |
| `field`  | 43,428,594 | 53,714,311 |
| `method` | 55,428,597 | 65,714,314 |

`arith`'s loop is the same nineteen instructions it was; the one added
instruction is the `scalar-to-value` that hands `total` to `assertEqual`
after the loop has ended. `field` and `method` run *more* instructions —
their operands are struct fields and call results, so every typed operator is
now bracketed by boundary instructions — and are faster anyway, because a
boundary instruction is cheaper than the `Value` traffic it replaced.

The profile of `arith` on the typed VM, same sampling as above:

```
 85.07%  Vm::execute            the dispatch loop
  4.22%  HostRegistry::with_budget
  3.67%  Vm::back_edge
  0.00%  drop_in_place<Value>
  0.00%  Value::clone
  0.00%  <malloc>
```

Against the prototype's profile of the same program on the same machine —
70.21% dispatch, 12.81% `drop_in_place<Value>`, 3.34% `Value::clone`, 6.24%
`promised_int`, 0.00% malloc — the whole of the representation cost is gone
rather than reduced: 16.2% of the run was cloning and dropping a `Value` and
now none of it is. No `Value` clone or drop remains in `arith`'s loop at all,
which `the_arith_bench_loop_builds_no_value_it_does_not_use` asserts
statically as well: every instruction between the loop's test and its jump
back takes nothing off the value stack and puts nothing back.

**The gate passes.** The improvement is 2.47× on the slice, it is
attributable to the architecture rather than to a peephole — the instruction
count did not move — and the two controls the slice does not target did not
regress.

What dominates now is dispatch, which was always the other half of the
prototype's profile and is now 85% of a run 2.5× shorter. That is the next
thing to be measured against, not this one.

Both follow-ups the profile named have since been taken, and they were one
change. A parameter's slot and a return value's stack are now the declared
type's to decide, which is the calling convention described above; and an
`Int`-typed `if`/`else`, `match`, or block is lowered in scalar position
rather than built as a `Value` in each branch for a boundary instruction to
unwrap again — the second is what the first needs to be worth anything, since
a function whose body is a tail `if` would otherwise have paid back at its
return what its parameters saved. `benches/pure`'s `fib` and `benches/call`'s
`identity` hold no boundary instruction at all now, where `fib` had eight of
twenty-four.

What the convention does not reach is what still lowers on the value path
either way. `!` and unary `-` have no scalar form, and a builtin method or a
host operation answers a `Value` whatever its type, because both are the
interpreter's own code and the interpreter speaks `Value`. Each is a boundary
instruction where a value really does meet something general, which is what the
boundary is for. `&&` and `||` are lowered on the scalar stack, but only where
it pays: with two operands, the scalar form costs one boundary per operand that
is not already scalar and the value form costs one per operand that is, plus one
for the answer if a scalar is what was wanted. So the threshold differs by
position — wanted as a scalar, one already-scalar operand is enough; wanted as a
value, both must be.

The one shape a profile does still name is the struct field read. A field
answers a `Value`, so `benches/field` pays a `value-to-scalar` per field it
reads into arithmetic — five per loop turn — and `benches/method`'s `position`
method pays one before its own `return-scalar`. That is the largest remaining
boundary traffic in the suite, and nothing above addresses it.

## What each change bought

The suite table above is the end of a progression, and the steps of it are
separable. Every number below is VM-only, on the same machine and the same 15
iterations. "typed slots" is commit `5c04da2`, "convention" is `ba86c24`, and
"block charging" is `f44131b`.

| bench       | typed slots | convention | block charging |
| ----------- | ----------: | ---------: | -------------: |
| `pure`      |     3.5 ms |     2.6 ms |     2.6 ms |
| `call`      |   326.7 ms |   280.6 ms |   260.1 ms |
| `arith`     |   106.7 ms |    98.7 ms |    82.8 ms |
| `method`    |  1002.2 ms |  1005.0 ms |   947.7 ms |
| `chars`     |   872.2 ms |   847.8 ms |   832.3 ms |
| `arrayget`  |   729.5 ms |   707.9 ms |   677.5 ms |
| `field`     |   606.0 ms |   602.3 ms |   542.5 ms |
| `hostheavy` |     3.8 ms |     3.8 ms |     3.8 ms |

One cell of that has to be read with its caveat rather than at face value.
`arith` ran the *same* 31,142,877 instructions before and after the calling
convention, so its 106.7 ms to 98.7 ms is not attributable to that change, and
code layout is the likeliest explanation for it. The two changes `arith`
genuinely responds to are the typed slots, measured above, and the block
charging in the last column.

The calling convention, unlike the typed slots, really is partly a reduction in
the number of instructions run — and only where it acts. From `cove run
--stats`:

| bench      | before     | after      |
| ---------- | ---------: | ---------: |
| `arith`    | 31,142,877 | 31,142,877 |
| `call`     | 41,142,877 | 37,142,877 |
| `pure`     |    328,367 |    229,862 |
| `method`   | 65,714,314 | 65,714,314 |
| `field`    | 53,714,311 | 53,714,311 |
| `chars`    | 41,856,022 | 41,856,022 |
| `arrayget` | 42,000,027 | 42,000,027 |

`pure` runs 30% fewer instructions and `call` 9.7% fewer; nothing else moves,
because nothing else calls a function whose parameters or answer are `Int`.
Block charging then left every one of these counts unchanged, which is the
control that says it moved a charge rather than the work.

The shape of the whole thing is three different kinds of saving. The typed-slot
change made the same instructions cheaper, the calling convention removed
instructions, and block charging removed bookkeeping that was never about the
instruction it stood in front of. Each benchmark responded to the ones it was
shaped to respond to, and to no others.

### What the batching itself costs

The table above says what block charging *bought*. It does not say what it
costs, and issue #120 asked for that half too: the `block_fuel` table's space,
and what a program pays for a charge that did not cover enough instructions to
be worth making. Both are measured here, on the same machine as the ablation
study below and in the same way — `cove run <bench> --backend vm --stats`,
fifteen times, the median of `execute=`. Three brackets of the shipped build
agreed to 0.18% on `arith` and 0.37% on `field`, so the machine was quiet.

The comparison is against per-instruction charging restored: the whole
`block_fuel` mechanism removed — the table reads, the charge at every arrival,
the `blocks` local — and `self.instructions += 1; self.fuel += 1;` and the
interval compare put back at the top of the dispatch loop. Both builds run the
same instruction counts, which is the control that says the ablation moved a
schedule rather than the work.

| bench     | block charging | per-instruction | block charging is |
| --------- | -------------: | --------------: | ----------------: |
| `arith`   |        87.2 ms |        102.3 ms |      14.8% faster |
| `field`   |       447.5 ms |        470.8 ms |       5.0% faster |
| `call`    |       280.9 ms |        293.2 ms |       4.2% faster |
| `pure`    |        2.82 ms |         2.86 ms |       1.4% faster |
| `branchy` |       249.6 ms |        293.6 ms |      15.0% faster |

`branchy` is not in `benches/`. It is a loop of eight one-statement `if`s
written for this measurement, to be the case block charging should be worst
at, and it is not one: it is where block charging wins by the most after
`arith`.

**Nothing measured pays for a charge it did not need**, and the reason is
visible in the compression. Counting the charges an instrumented build makes
against the instructions it executes gives the average extent each charge
covered:

| bench      |    charges |  instructions | instructions per charge |
| ---------- | ---------: | ------------: | ----------------------: |
| `pure`     |     76,621 |       229,862 |                     3.0 |
| `call`     | 10,000,003 |    37,142,877 |                     3.7 |
| `method`   | 14,000,005 |    59,428,598 |                     4.2 |
| `arith`    |  6,000,003 |    31,142,877 |                     5.2 |
| `branchy`  | 20,000,002 |   114,000,014 |                     5.7 |
| `hostheavy`|      6,003 |        38,019 |                     6.3 |
| `chars`    |  6,048,003 |    41,856,022 |                     6.9 |
| `field`    |  6,000,003 |    47,428,595 |                     7.9 |
| `arrayget` |  4,000,003 |    42,000,027 |                    10.5 |

Two of those instruction counts are lower than the table under "What each
change bought" says, and the difference is the fused typed field read landing
between the two measurements: `field` went from 53,714,311 to 47,428,595 and
`method` from 65,714,314 to 59,428,598, and nothing else moved. The earlier
table is left as it was measured.

A charge is one bounds-checked load from a second array, one add and one
compare, against the add and compare per instruction it replaces, so the
crossover is near one instruction per charge. The lowest measured is three,
and it is `pure` and `call` rather than `branchy` — what shortens an extent is
a *call*, which ends a line at both ends, and not a branch. `pure`'s 1.4% on a
2.8 ms run is inside its own 3.8% spread and is the honest reading of "free".

**The space is one `u32` per instruction**, which `lower::validate` enforces
from both sides, and `size_of::<Inst>()` is 16, so the table is 25% on top of
the code and nothing else. Every benchmark in `benches/` lowers to 318
instructions between them, or 1,272 bytes of table. The largest single program
in this repository, `examples/callbacks`, is 333 instructions in 19 functions:
5,328 bytes of code and 1,332 bytes of table. A program would have to be four
hundred times the size of anything here before the table reached a megabyte.

Two of the four figures in this section are the record's rather than mine, and
they are the two the ablation study already had: removing per-instruction fuel
and its interval compare from the pre-batching build measured +7.1% on `arith`
and +3.5% on `field`, and block charging as built recovered 11.4% and 6.9%
against the commit before it. Everything above — the five-benchmark
comparison, the charge compression, and the space — was measured for issue
#120.

## What the dispatch loop is made of

Each row below was removed *alone* from commit `ba86c24`, with the rest left as
it was, and the run measured. Several of them are unsound and none of them was
a proposal: the point is attribution, not a shipping change.

| removed                                       | `arith` | `field` |
| --------------------------------------------- | ------: | ------: |
| the instruction counter                       |   +1.9% |   −0.2% |
| per-instruction fuel and its interval compare |   +7.1% |   +3.5% |
| the back edge's cancellation poll             |   −5.8% |   +1.0% |
| the whole back-edge check                     |  +10.8% |   +5.1% |
| the instruction fetch's bounds check          |   −6.3% |   +0.4% |
| the frame slots' bounds checks                |   +1.0% |   −0.1% |
| the scalar operand stack's checks             |   +4.2% |   −0.2% |
| the span computed for every `Int` operator    |   +4.4% |   +0.8% |
| all of them together                          |  +35.8% |   +9.1% |

Three things are needed to read that honestly.

**Two ablations made `arith` slower, reproducibly.** Removing the fetch's bounds
check cost 6.3%, and 4.6% when the measurement was repeated on a clean rebuild.
The cause is code layout: perturbing a forty-arm `match` moves branch-target
alignment and the dispatch body's cache footprint, and `arith`'s loop is small
enough to feel it. The consequence is that any single `arith` delta below
roughly ±6% is not separable from layout, so `field` — whose baselines
bracketed at 0.5% to 2.1%, and whose deltas move monotonically — is the
benchmark to trust for a small effect. Two baselines bracketed the whole study
at 0.01% on `arith` and the untouched interpreter moved at most 1.3%, so the
machine was quiet; layout is the explanation, not noise.

**`arith` is superadditive and `field` is not.** The individual `arith` savings
sum to 23.1% — counting the whole back-edge check rather than the poll inside
it — against the 35.8% measured with all of them removed together, over
half again as much; the same sum on `field` is additive to within 2%. Whatever
the mechanism — a loop body that crosses some threshold once enough
per-instruction work is gone — `arith`'s floor is not reachable by any subset
of these summed.

**`field` responds to almost nothing but fuel and the back edge.** 8.6 of its
9.1 points are those two. It spends its time in the value stack and the heap,
not in dispatch bookkeeping.

The sizes measured alongside belong with the table, since several rows are
about what a hot path carries: `size_of::<Span>()` is 12,
`size_of::<Result<i64, RuntimeError>>()` is 120, and
`size_of::<Result<(), RuntimeError>>()` is 120 against 8 for
`Result<(), Box<RuntimeError>>`.

### What the place model cost, and what it cost it on

The place instructions were measured against that sensitivity rather than
assumed to be under it, because the first version of them was not. Adding
them cost `arith` 14.9%, `pure` 10.5% and `call` 9.3% — on programs that
execute no place instruction at all, and while the AST column, which is
untouched code, stayed within 2% of itself. Every number here is VM-only,
`cove-bench --iterations 15`, on the machine and build the tables above were
measured on, against commit `aff6550` re-measured in the same session.

| bench       | `aff6550` | first version | as landed |
| ----------- | --------: | ------------: | --------: |
| `pure`      |    2.5 ms |        2.8 ms |    2.6 ms |
| `call`      |  260.6 ms |      288.3 ms |  268.8 ms |
| `arith`     |   81.4 ms |       94.3 ms |   86.8 ms |
| `method`    |  861.5 ms |      906.9 ms |  891.1 ms |
| `chars`     |  800.6 ms |      807.9 ms |  816.1 ms |
| `arrayget`  |  674.2 ms |      667.2 ms |  663.1 ms |
| `field`     |  430.5 ms |      451.4 ms |  447.1 ms |
| `hostheavy` |    3.7 ms |        3.8 ms |    3.8 ms |

Three things were wrong with the first version, and all three were about
carrying the capability rather than about using it.

`Inst::Call` gained a third argument count, and three `u32` counts beside a
`FunctionId` made `size_of::<Inst>()` 24 where it had been 16 — 50% more of
every function's code array, for a number bounded by the parameters a
declaration writes. The counts are `u16` now and the lowering refuses a
declaration with more parameters than that holds; `Inst` is 16 again.

`Frame` gained a fifth field and would have gone from 32 bytes to 40. A frame
is copied into a local of the dispatch loop and read by every instruction
that addresses a slot, so its width is register pressure in exactly the loop
`arith` spends its run in. `return_pc` is a `u32` now — an instruction index
is a `u32` everywhere else in the IR — and the frame is 32 bytes again.

The largest of the three was the seven new arms in `Vm::execute`'s `match`.
They are one arm now, calling an `#[inline(never)]` function, and that alone
was worth about six points of `arith`. It is the ablation study's own finding
read from the other side: a change that alters nothing a program executes can
still cost it several percent, because the dispatch body's footprint is a
cost every program pays.

What is left is `arith` at +6.7% and everything else at or below +3.9%, with
`arrayget` at −1.6%. `arith` runs 31,142,877 instructions and spends
31,142,877 fuel on both builds — identical, which is what says the remaining
difference is per-instruction cost and not work — and the same source change
measured through the `cove` binary rather than through `cove-bench` shows
+4.5% rather than +6.7%, which is what says the remaining cost is layout. It
is at the edge of the ±6% band this section already established for `arith`
and is not separable from it by anything measured here.

### What the three capabilities after it cost, and where the cost actually was

[Issue #126](https://github.com/myuon/cove/issues/126) is the same shape one
step later. Closures, dynamic dispatch and tasks each measured within their
own threshold against their own parent, and against `c8450e7` the three
together cost 3.5% to 19% on programs that execute none of their
instructions. The three things the place model had gone wrong on were checked
first and none of them had: `size_of::<Inst>()` is 16 at both commits,
`Frame` is 32 bytes at both, and `benches/arith`'s own arms — `load-scalar`,
`store-scalar`, `int-binary`, `jump-if-false-scalar`, and `charge` — are
byte-for-byte the same code. What changed was that `Vm::execute`'s `match`
went from 39 arms to 48.

Two things were found, and the larger of them was not the one that was
expected.

**The five new arms written inline were worth about three points.**
`call-resource`, `snapshot`, `spread-argument`, `make-range` and
`make-host-enum` were about ninety-five lines inside the `match`; the other
four of the nine already delegated to an `#[inline(never)]` helper. Putting
those five behind one helper too — `Vm::cold_inst`, which is grouped by what
the loop pays to carry them rather than by what they do — took `arith` from
96.3 ms to 93.2 ms.

**The four that already delegated were worth about nine, and not for
existing.** Each was handed the `Inst` the `match` had just dispatched on.
An `Inst` is two words, and one that has to survive the dispatch is one the
register allocator has to keep somewhere; with five callers wanting it, it
went to the stack. The disassembly says so plainly. At `c8450e7` the loop
loads the fields it wants straight out of the code array into registers —
tag, byte, `u32`, `u64`, four loads and no stores — and keeps `pc` in `%r15`.
At the regressed commit it loads both words, *stores both to the stack*, and
spills `pc` beside them, on every instruction dispatched; each arm then
reloads what it wanted. Changing the five helpers to take `running` and `pc`
— both already live — and read `running.code[pc]` for themselves lets the
instruction die at the `match`, and the loop goes back to loading only the
tag byte and letting each arm fetch its own payload. That alone took `arith`
from 96.3 ms to 87.1 ms; with the five inline arms collapsed as well, to
84.8 ms against 82.7 ms for `c8450e7`.

Medians of fifteen `execute=` times through the `cove` binary rather than
`cove-bench`, which reproduces the regression to under a percent and takes
seconds; `c8450e7` was re-measured between every variant and read 82.4 ms to
83.5 ms throughout, so the machine did not drift under them.

| variant                                     | `arith` | `call` |
| ------------------------------------------- | ------: | -----: |
| `c8450e7`                                   | 82.7 ms | 262 ms |
| the regression, as found                    | 96.3 ms | 291 ms |
| five inline arms behind one helper          | 93.2 ms | 288 ms |
| helpers take `running` and `pc`, not `Inst` | 87.1 ms | 283 ms |
| both                                        | 84.8 ms | 274 ms |

**The function did not get bigger, which is the negative result worth
keeping.** `Vm::execute` disassembles to 5,684 instructions at the regressed
commit against 5,957 at `c8450e7` — *smaller*, while running 16% slower on
`arith`. So "a bigger dispatch body costs every program" is not what happened
here, however well it described the place model. Body size did cost something
— the three points the five inline arms gave back are exactly that, and the
function is 4,532 instructions now — but it was the minority of it. What cost
more was a value the loop had to keep alive across the dispatch, which is
invisible in an arm count and invisible in a line count, and which two of the
four delegating arms introduced while doing the thing that was supposed to be
the remedy. Delegating an instruction is not free by construction; it is free
only if the call takes nothing the loop would not otherwise be holding.

Two variants were tried and discarded. Dropping the `blocks` local from the
loop and reading `running.block_fuel` at each jump — two fewer live registers
in exchange for a load — moved `arith` from 84.6 ms to 84.4 ms and `call`
from 274.0 ms to 276.3 ms, which is nothing and slightly the wrong way.
Stripping the per-call work places and tasks added, as an unsound ablation
rather than a proposal — the `async_frames` check, the place window's base
and its `resize` — made `call` *worse*, 278.6 ms against 274.0 ms. So the
`call` residual is not the instructions the `Call` arm gained, and it is not
attributed here at all.

What is left, `cove-bench --iterations 5` averaged over two rounds against
`c8450e7` measured in the same session: `arith` +2.3%, `field` +0.9%,
`method` +1.7%, `chars` −0.5%, `arrayget` −1.8%, `call` +5.6%. Everything but
`call` is inside the ±6% band this section established for `arith` or below
its own noise; `call` is not, and 61% of what it lost is still lost. `pure`
and `hostheavy` are 2.6 ms and 3.8 ms and say nothing at that size either
way.

## The value representation, audited

Issue #116 asked that no representation be chosen without a measurement and an
audit. This is the audit, and the measurement it turned out to need.

`cove_runtime::Value` is a twenty-two-variant enum and `needs_drop::<Value>()`
is true. It was 40 bytes — the number the top of this document reads the
prototype's cost through. The audit's first question was what made it 40, and
the answer needed no cleverness to read: an enum is as wide as its widest
variant, and exactly one variant was wide.

```text
40  Value
32  (Rc<str>, Rc<str>)            Value::HostFn { module, op }
24  (i64, i64, bool)              Value::Range
16  Rc<str>, Rc<[Value]>          Str, Array — fat pointers
 8  Rc<String>, Rc<Vec<Value>>    the thin equivalents
```

Every `Value` either backend moved was sized by the variant that names
`console.println`. Commit `c8450e7` put that variant behind one pointer, and
`size_of::<Value>()` is 24.

A prediction failed on the way there, and it belongs on the record with
everything else. The expected ladder was 40 → 32 → 24: `Range`'s
`(i64, i64, bool)` would set the width at 32 once `HostFn` had gone, and a
second commit boxing `Range` would take it the rest of the way. There is no 32
step. `bool` has a niche, rustc puts the discriminant inside it, and the whole
of `Range` fits in 24 including its tag — the same 24 a fat pointer needs. The
second commit was written and measured anyway, it bought nothing, and it was
dropped.

Going 24 → 16 means making `Str` and `Array` hold thin pointers, which is 284
call sites and either an extra dereference on every read (`Rc<String>`) or
hand-written unsafe to avoid one. Rather than pay that to find out what it was
worth, the width itself was measured: a padding variant that is never
constructed was added to `Value` to widen it, and the suite run at each width.

| bench | 24 | 32 | 40 | per +8 bytes |
| --- | ---: | ---: | ---: | ---: |
| VM `field` | 436.8 ms | 440.0 ms | 453.7 ms | +1.9% |
| VM `method` | 861.7 ms | 881.9 ms | 893.3 ms | +1.8% |
| VM `chars` | 810.0 ms | 805.4 ms | 822.5 ms | +0.8% |
| VM `arrayget` | 658.2 ms | 650.7 ms | 667.2 ms | +0.7% |
| VM `arith` | 82.4 ms | 82.4 ms | 82.0 ms | −0.3% |
| AST `arith` | 424.7 ms | 447.8 ms | 459.7 ms | +4.1% |
| AST `call` | 1534.1 ms | 1544.2 ms | 1592.7 ms | +1.9% |
| AST `field` | 857.6 ms | 857.7 ms | 886.7 ms | +1.7% |

The 40 column is a control, and it is what makes the rest readable: padding to
40 reproduces the real 40-byte `Value` measured before `c8450e7` — VM `arith`
82.0 against 82.6, AST `arith` 459.7 against 462.4, AST `call` 1592.7 against
1605.9. A padding variant that nothing constructs measures width and nothing
else, and it lands where the real thing landed. The 32 column is noisier than
the endpoints and several of its entries are negative, so the honest reading of
the table is the 24-to-40 span rather than either single step.

So eight bytes of `Value` width is worth roughly 0.7% to 1.9% on the VM, and it
is worth least on `chars` and `arrayget` — which are exactly the two benchmarks
that would pay the extra indirection a thin pointer costs. **24 → 16 is not
taken.** The gain is about a percent, and it is smallest on the two programs
where the cost of getting it would be largest.

That settles what the last eight bytes are worth. It does not settle the
question #116 actually asked, which is whether a `Value` could be one word
rather than two — and that one is settled by semantics rather than by
measurement. `docs/LANGUAGE_REFERENCE.md` says `Int` is a 64-bit signed
integer, and `docs/LANGUAGE_CARD.md` says integer overflow is a broken
invariant rather than a wrapped result. An 8-byte value has no room for a full
64-bit integer *and* a tag. NaN boxing gives about 51 bits of payload and an
aligned tagged pointer gives 61 or 62; both would have to box integers outside
their range, which would make arithmetic near `i64::MAX` allocate. That is a
change to what `Int` costs, in a language whose Card promises what `Int` *is*.
So neither is rejected here for being slow or unportable. They are rejected
because Cove's `Int` does not fit in them, and the floor for this language is 16
bytes: a tag beside a word.

A tag beside a payload at 16 bytes is the third of the three and it remains
available. It is not taken either, for the reason two paragraphs above: the
measured gain from the last eight bytes is about one percent, and it lands
where the cost of getting it would land.

The conclusion is the one the audit was for. The value representation is not
where the remaining cost is. It was worth exactly one boxed variant, that
variant was a mistake rather than a design, and everything past it is either a
percent or impossible.

## What was built from it, and what was not

**Built.** Block charging, worth 11.4% on `arith` and 6.9% on `field`. Then one
schedule for the back edge, another 4.3% and 1.1%. Together they are the two
largest individual rows of the table above, and both were *placement* rather
than substance: the same total is charged, at fewer points.

**Measured and not built: reading the failing operator's span only on failure.**
It bought 0.2% on `arith` and −1.9% on `field`, both inside the noise, against
the 4.4% the same thing was worth when it was ablated. That is the most
interesting negative result here. The span was expensive only while it stood
next to the per-instruction bookkeeping, and once that bookkeeping was gone it
was not standing in front of anything. A cost measured by ablation is a cost *in
that arrangement*, and rearranging the surroundings can retire it without anyone
touching it.

**Measured and not worth building: an unchecked instruction fetch, and
unchecked frame-slot indexing.** The first is reproducibly slower, and the
second is worth 1.0% on `arith` and nothing on `field`. `validate` proves both
of those bounds already, so the proof is available and buying it a second time
is not worth the `unsafe`.

**Named and not taken.** `Result<(), RuntimeError>` is 120 bytes, and it is what
`Vm::charge`, `Vm::back_edge`, and `Vm::safepoint` all return, on every taken
jump, every fall-through, every call, and every return, on a path where the
answer is always `Ok`. Boxing the error's payload would make it 8. It is a large
mechanical change across the workspace, and it is the one remaining `Result`
with a hot reader.

A typed field read stood in this paragraph too, as the other thing named and
not taken: a struct field answers a `Value`, so every field used as an `Int`
crossed a boundary to become one, five times a turn in `benches/field`. It has
since been taken — one instruction that reads the field out of the struct by
reference and converts it, where the two it replaces built a `Value` for the
next instruction to discard. `field` lost 11.7% of its instructions and 18% of
its time, and `method` 9.6% and 3%. It is written here rather than left in the
list because the list is what was named from a profile, and this is what came
of one of them.

## What the measurement itself costs

[Issue #123](https://github.com/myuon/cove/issues/123) asks for one workload
under five configurations — production; instruction statistics off with fuel
unchanged; both off, for attribution only; a sampling profiler attached; and
trace off against trace on — and for wall-time distributions rather than
single runs. Two of those five did not exist when it asked, and the reason is
the thing being measured. `Vm::charge` adds a block's length to `self.fuel`
and to `self.instructions` in the same two lines, which is most of why
charging by the block is cheap, so there was no configuration in which one of
them was off and the other was on.

### The mechanism that turns a thing off can cost more than the thing

Two mechanisms could build that configuration, and the choice between them is
not a detail of the measurement — it is the first result.

A compile-time removal costs nothing at run time and changes the binary, which
matters here more than it would elsewhere: this document has established twice
that a change altering nothing a program executes can still move `arith` by
several percent, because the dispatch body's footprint and its branch-target
alignment are costs every program pays. A runtime flag leaves the binary alone
and puts a branch on the path, and the branch is the same shape as the
increment it guards.

Both were built and both were measured. The flag is a `bool` on the `Vm`, read
from the environment once at construction so that one binary gives both
halves, and tested in `Vm::charge` around the increment. Fifteen runs of
`cove run <bench> --backend vm --stats`, medians of `execute=`, interleaved
over three rounds:

| build                          | `arith` | `field` |
| ------------------------------ | ------: | ------: |
| production                     | 84.7 ms | 447.9 ms |
| the flag, counting             | 86.6 ms | 448.4 ms |
| the flag, not counting         | 88.7 ms | 449.3 ms |
| the counter removed at compile time | 84.1 ms | 441.7 ms |

**The flag recovers nothing, and on `arith` it costs 2.4% to have the counter
switched off.** That last figure reproduced on all three rounds and it is
within one binary, so it is not layout; what it is has not been established
here, and it is reported because it was measured rather than because it is
understood. What the table does establish is the shape of the answer: the
branch costs what the increment costs, so a flag would put the whole of the
mechanism on every production run in order to save a figure that is worth
between 0.7% and 1.9% on the benchmark most sensitive to it.

**So no flag is shipped**, and configuration 2 is a build made to be measured
and thrown away. The patch that makes it, and eight others, are
`scripts/ablate/*.patch`, with `scripts/ablate/run.sh` to apply each one,
build it, and put the binary somewhere a measurement can name. They are
patches rather than a cargo feature for the reason above: a feature would be
carried by the production binary, and this is a measurement rather than a
capability. They are expected to rot, and `git apply` refuses loudly when they
do, which is the wanted behaviour — an ablation that applied to code it was
not written for would measure something nobody named.

### What each part of the bookkeeping costs

Every row below was removed *alone* from the shipped build, and the shipped
build was measured on both sides of the study: it read 84.6 ms and 84.8 ms on
`arith`, 448.4 ms and 447.4 ms on `field`, and 232.2 ms and 232.0 ms on
`call`, so the machine did not move under it. Medians of fifteen, through the
`cove` binary. A positive number is what the removal saved.

| removed                                                 | `arith` | `field` | `call` |
| ------------------------------------------------------- | ------: | ------: | -----: |
| the block instruction counter                            |   +0.7% |   +1.4% |  +0.2% |
| fuel accumulation and its interval compare               |   +4.7% |   +2.8% |  −2.1% |
| both of those — configuration 3                          |   +4.6% |   +4.2% |  −2.6% |
| the back edge's whole check                              |  +11.4% |   +7.0% |  +1.4% |
| the safepoint at every call and every return             |   −0.9% |   +0.9% | +38.7% |
| the budget's lock and accounting inside every safepoint  |   +7.1% |   +5.8% | +40.3% |
| the two stop flags a safepoint reads                     |   −9.1% |   −0.5% |  +2.5% |
| the collection a safepoint asks about                    |   −8.5% |   −1.7% |  −6.0% |

Four readings, and the fourth is the one worth the section.

**The instruction counter is nearly free and its three measurements do not
agree.** It is 1.9% on `arith` in issue #126's ablation, 2.9% in a round of
this study taken before the change below landed, and 0.7% here; on `field`,
which this document has established as the benchmark to trust for a small
effect, it is 1.4%. The honest statement is that it is worth something under
two percent and that no single measurement of it is separable from code
layout. It is not what makes `--stats` cost anything.

**Two removals made things slower, and both are pure deletions.** Removing the
stop-flag read from `Vm::safepoint` costs `arith` 9.1%, and removing the
collection question costs it 8.5% and `call` 6.0%. Neither can be a real cost
of the code that was deleted. This is the same layout sensitivity the
dispatch-loop study found and the same size, and it is kept here rather than
tidied away because a reader who sees only the positive rows will believe the
study is more precise than it is. What follows from it is that `stopped_here`
and `Heap::should_collect` are free within anything measurable here, which is
the useful half of a result that looks like nonsense.

**Configuration 3 is not the floor and does not sum.** Removing the charge
entirely also stops the back edge from ever firing, because what a back edge
reads is the fuel the charge accumulates — so the third row should be at least
the fourth and it is less than half of it. `arith` is superadditive in the
other direction too, which this document has recorded before. No subset of
these rows adds up to another one, and the table should be read as eight
separate statements rather than as a decomposition.

**What dominates is none of the above.** It is the mutex a safepoint takes to
reach the run's `Budget`, and it does not show on `arith` because `arith`'s
loop calls nothing. On `benches/call`, which calls one function per turn, the
whole of the budget's lock and accounting is 40.3% of the run. The profile
says the same thing in different words — `samply` at 5 kHz, self time, on the
symbol-bearing build:

```text
benches/arith                             benches/call
 74.66%  Vm::execute                       45.99%  Vm::execute
  7.53%  HostRegistry::with_budget         16.02%  HostRegistry::with_budget
  4.79%  pthread_mutex_lock                10.34%  pthread_mutex_lock
  4.11%  pthread_mutex_unlock              10.08%  pthread_mutex_unlock
  2.05%  interp::stopped_here               5.43%  Vm::leave
```

A `Budget` lives behind `Mutex<Option<Budget>>` on the `HostRegistry`, because
every task of a run shares one and a task runs on a thread of its own. A call
is an unconditional safepoint and so is a return, so a loop that calls one
function pays for two acquisitions a turn, and the two of them together cost
more than the dispatch of the whole loop body.

### One acquisition of three was buying nothing, and is gone

`Vm::enter` took the lock twice. Once to read `Limits::max_call_depth`, and
once again inside the safepoint that follows. The first of the two is what
this change removes, and what makes it removable is that the answer cannot
have changed: a `Budget` is installed with `HostRegistry::set_budget`, which
needs `&mut HostRegistry`, and a `Vm` holds the registry by shared reference
for as long as it exists — so no budget can be installed or replaced while a
run is in progress, and the limit a call reads is the limit the previous call
read. `Vm::host_call_depth_limit` asks once, through the lock, and answers
from a field afterwards. It asks on the first call rather than in `Vm::new`
because `Vm` is public and nothing about constructing one promises that a
budget has been installed yet.

Nothing else moves. The order the three checks are made in is the order
`Interpreter::invoke` makes them and is unchanged, the lock is still taken to
build the error when the limit is exceeded, and a `Vm` with no budget behind
its registry still has no limit, which is what `with_budget` answering `None`
has always meant.

| bench    | before   | after    |          |
| -------- | -------: | -------: | -------- |
| `call`   | 279.0 ms | 232.6 ms | **16.6% faster** |
| `method` | 911.8 ms | 822.9 ms | 9.8% faster |
| `pure`   |  2.82 ms |  2.33 ms | 17.2% faster |
| `arith`  |  88.4 ms |  85.2 ms | 3.7% faster |
| `field`  | 442.2 ms | 445.7 ms | 0.8% slower |

Medians of fifteen, with the parent measured on both sides. The last two rows
are the control and should be read as one: neither loop calls a Cove function,
so neither can respond to this, and both are inside the band `arith` is known
to move in for layout alone. An ablation that removed the read entirely,
rather than remembering it, measured 16.7% and 9.4% on the first two — so the
change recovers the whole of what was there to recover, which is what says
nothing was left behind.

The interpreter does the same thing at the same point and was not changed.
Nothing here measures the oracle, and a change to it would have to be measured
before it could be claimed; the site is `Interpreter::invoke`'s own
`max_call_depth` read, for whoever asks the question next.

What is left on the call path is two acquisitions a turn, one at the call and
one at the return, and they are still the largest single cost `benches/call`
has. That is a target rather than a finding, and "The cliffs" below says what
it would take.

### The profiler, the trace, and `--stats` itself

`CARGO_PROFILE_RELEASE_DEBUG=1` is a configuration change and it is named
here because it is one, but it is not a measurable one: the symbol-bearing
build read 86.1 ms against 85.4 ms on `arith`, 440.0 ms against 444.1 ms on
`field`, and 231.8 ms against 233.1 ms on `call` — under a percent in both
directions and not separable from layout. So a profiled run and a production
run are the same program, which is what makes the profiles above readable
beside the times.

Attaching `samply` is a different matter, and it has to be read in the right
units. The section the profiler is watching gets 6.3% slower at its default
1 kHz and 6.8% at 10 kHz, which is small; the *process* goes from 93.9 ms to
1,359 ms, which is fourteen times longer and is almost entirely samply's own
setup and symbolication rather than anything the program did. A reader who
times the wrapper will conclude the profiler costs 1,265 ms per run and will
be wrong by two orders of magnitude.

| configuration        | `arith`, `execute=` |
| -------------------- | ------------------: |
| no profiler          |             86.0 ms |
| `samply -r 1000`     |             91.4 ms |
| `samply -r 10000`    |             91.8 ms |

The trace is the configuration with the largest spread between two programs,
and the reason is that **the VM has no trace-disabled branch in its dispatch
loop at all**. Instructions are not traced — ADR 0019 does not propose an
instruction-level trace and this backend does not record one — so what a trace
costs is paid per Host call and nowhere else. Process wall time, medians of
fifteen:

| configuration                | `arith` | `hostheavy` |
| ---------------------------- | ------: | ----------: |
| neither `--stats` nor a trace | 93.1 ms |     14.2 ms |
| `--stats`                     | 93.3 ms |     16.5 ms |
| `--stats --trace`             | 93.4 ms |     52.8 ms |

That has a consequence for this whole document, and it is worth stating
plainly. **Every `execute=` figure recorded anywhere above comes from a run
with `--stats`, and `--stats` is not the production configuration.** A run
that asks for neither a trace nor statistics installs a `NullSink` and the
registry then knows that nothing will read a description of a call's values;
`--stats` installs a composite sink instead, and describing every Host call's
arguments and result for a sink that discards them costs `hostheavy` 16.8%.
On the benchmarks the tables above are made of it costs nothing measurable,
because `arith`, `field`, `call`, `method`, `pure`, `chars` and `arrayget`
make no Host call between them. `hostheavy` is the one benchmark whose
`--stats` time should not be read as its production time, and the figure that
should be is the 14.2 ms above.

## The calling-convention matrix

Issue #123's second half asks what the typed three-stack convention costs at
each of its boundaries: a settled scalar local, the same local rooted for a
`var` argument, a static declared call, a declared function used as a value, a
closure call, a captured scalar, a scalar crossing to generic `Value`, and a
Host callback. `benches/convention/main.cove` is those eight, and
`cove-bench --matrix` runs them.

### What is held constant, and how that was checked

Every row is `benches/arith`'s loop: two million turns, `% 7`, the same
285,715, which every row asserts and which is the check that they did the same
arithmetic. What differs between two rows is one thing — how the turn's `i`
reaches the `%` that consumes it — and the rows are written out in full beside
each other rather than parameterized, so the difference is visible by reading
them.

What is *not* held constant is the instruction count, and it must not be: a
call is more instructions than no call, and a boundary is an instruction. So
the count is not a control on the matrix. It is the decomposition of it, and
`--matrix` prints it beside each row's time for that reason. A row that cost
more without running more instructions, or that ran more instructions than its
route explains, is the row to look at.

There are nine rows for eight questions. `conv_fresh` is a control rather than
one of the eight: `conv_host`'s callback has to be written at its call site,
because it reads the turn's `i` and a lambda captures a snapshot, so that row
builds a closure every turn as well as crossing the Host boundary.
`conv_fresh` is everything `conv_host` does except leave the VM.

### The matrix

`cove-bench --matrix --iterations 9`, VM backend, on the machine and build the
tables above were measured on. `ns/turn` is the median divided by the two
million turns.

| row            |   median |    min |    max | vs base | instructions | per turn | ns/turn |
| -------------- | -------: | -----: | -----: | ------: | -----------: | -------: | ------: |
| `conv_local`   |  93.7 ms |  93.1  |  94.3  |   1.00× |   35,142,879 |     17.6 |    46.8 |
| `conv_var`     | 121.3 ms | 120.4  | 123.1  |   1.30× |   39,142,890 |     19.6 |    60.7 |
| `conv_static`  | 237.3 ms | 232.8  | 244.1  |   2.53× |   37,142,877 |     18.6 |   118.6 |
| `conv_generic` | 279.9 ms | 270.5  | 281.9  |   2.99× |   41,142,877 |     20.6 |   140.0 |
| `conv_closure` | 312.6 ms | 307.0  | 317.7  |   3.34× |   43,142,879 |     21.6 |   156.3 |
| `conv_fnvalue` | 313.6 ms | 305.4  | 316.0  |   3.35× |   43,142,879 |     21.6 |   156.8 |
| `conv_capture` | 362.7 ms | 357.7  | 372.2  |   3.87× |   53,142,883 |     26.6 |   181.4 |
| `conv_fresh`   | 799.3 ms | 784.3  | 804.7  |   8.53× |   47,142,877 |     23.6 |   399.6 |
| `conv_host`    | 2553  ms | 2508   | 2562   |  27.25× |   51,142,877 |     25.6 |  1276.6 |

Ordered by cost rather than by the order the issue names them, because what
the table is for is the distance between two neighbours.

### What each row runs, rather than what it costs

The counts below come from a scratch build and their wall times must not be
read beside them: the instrument keeps a histogram keyed by the instruction's
discriminant and updates four high-water marks on every dispatch, which makes
the same programs six times slower. `scripts/ablate/instrument.patch` is that
build, and issue #123 is explicit that the two must not be mixed. Everything
per turn, over two million turns; the allocations are the process's own,
counted by a `GlobalAlloc` wrapper the same patch installs, and are the run's
alone rather than the process's.

| row            | `scalar-to-value` | `value-to-scalar` | allocations | bytes | peak value / scalar / place stack | peak frames |
| -------------- | ----------------: | ----------------: | ----------: | ----: | --------------------------------: | ----------: |
| `conv_local`   |                 0 |                 0 |           0 |     0 |                         2 / 5 / 0 |           1 |
| `conv_var`     |                 1 |                 1 |           0 |     0 |                         3 / 4 / 2 |           2 |
| `conv_static`  |                 0 |                 0 |           0 |     0 |                         2 / 4 / 0 |           2 |
| `conv_generic` |                 1 |                 1 |           0 |     0 |                         2 / 4 / 0 |           2 |
| `conv_closure` |                 1 |                 1 |           0 |     0 |                         3 / 4 / 0 |           2 |
| `conv_fnvalue` |                 1 |                 1 |           0 |     0 |                         3 / 4 / 0 |           2 |
| `conv_capture` |                 2 |                 3 |           0 |     0 |                         4 / 5 / 0 |           2 |
| `conv_fresh`   |                 1 |                 1 |           4 |   208 |                         3 / 4 / 0 |           2 |
| `conv_host`    |                 1 |                 1 |          14 |   469 |                         3 / 4 / 0 |           2 |

Two of the five things #123 asked to have recorded turn out to be non-answers,
and both are worth recording as such.

**Nothing but the last two rows allocates at all.** Not one allocation in two
million turns for the seven rows above them, over the whole run and not per
turn. A boundary instruction converts between an `i64` and a `Value::Int`,
which owns nothing, so crossing costs instructions and never the heap. The
allocation question the matrix was expected to answer is answered by the two
rows that build a closure, and by nothing else.

**Nothing grows.** The value stack never exceeds four slots, the scalar stack
five, the place stack two, and the frame stack two, on any row. There is no
frame allocation to measure because a frame is five words pushed onto a `Vec`
that reached its size in the first turn, and there is no stack growth to
measure because a loop that calls one function of fixed arity has a fixed high
water mark. "Frame and stack growth" is a real question about a recursive
program and is not a question about any of these.

**Clone and drop activity is not separately instrumented and was read from the
profiles instead.** A counter on `Value::clone` would mean hand-writing a
twenty-two-variant `Clone`, and what it would count is dominated by clones
that own nothing and cost a move. What the profiles say is that
`drop_in_place<Value>` and `Value::clone` together are 0.0% of `conv_local`,
1.1% of `conv_static`, 3.2% of `conv_var`, and 10.5% of `conv_capture` —
which tracks the `value-to-scalar` column and not the wall-time column, and
is the reason the next section separates the two.

## The cliffs

A cliff here is a row that costs disproportionately more than its neighbour
for a reason about representation rather than about work. Four of the eight
gaps qualify, and the largest of them is not about representation at all,
which is the result this whole exercise turns on.

**A call costs 2.53× a loop turn, for one more instruction, and the reason is
the budget's lock.** `conv_static` runs 18.6 instructions a turn against
`conv_local`'s 17.6 and takes 71.8 ns longer. Nothing about representation
changed between the two rows: the argument is a scalar slot, the answer comes
back on the scalar stack, and the count says so by not moving. What happens is
two safepoints, and each is an acquisition of the mutex the run's `Budget`
lives behind. The ablation above puts 40.3% of `benches/call` there and the
profile puts 36% there, and one of the three acquisitions has since been
removed, which is where `call`'s 16.6% came from.

This is the largest cliff in the matrix that anything could be done about, and
what it would take is written down rather than done. `Budget::safepoint`
needs, per safepoint, to add to `fuel_spent`, to read the run's cancellation,
to compare against a fuel limit, and — every `DEADLINE_CHECK_INTERVAL`th time,
or every time when no fuel limit is set — to read the clock. Of those, the
cancellation is already an atomic flag a `Vm` could hold a clone of, `limits`
and `started_at` are immutable for the run, and `fuel_spent` and
`safepoints_since_deadline_check` are counters that could be `AtomicU64`. So
the lock is not protecting anything that needs a lock; it is protecting a
struct that happens to be `&mut`. Making `Budget::safepoint` take `&self` over
atomics would remove the acquisition without changing the schedule, which is
the property that matters — the schedule is what ADR 0024 and
`crates/cove-runtime/tests/responsiveness.rs` fix, and a change that moved it
would be a different proposal needing a different ADR. The ceiling is the
ablation's 40.3% on `call` and 5.8% on `field`. **This belongs to
[#116](https://github.com/myuon/cove/issues/116)**, whose calling convention
is required to cover "fuel, cancellation, trace" and which is where a call's
per-call cost is decided.

**A local rooted for a `var` argument costs 1.30×, and the whole of it is
representation.** `conv_var` is `conv_local` with one line added *outside* the
loop — `root(var v)`, once, after it has finished — and the loop body is
identical text. It runs two more instructions a turn and takes 13.9 ns longer,
and the two extra instructions are a `scalar-to-value` and a `value-to-scalar`
per turn: the binding is on the value stack for the whole body, because a
place cannot address the scalar stack, so every read and write of it crosses.
The profile shows `drop_in_place<Value>` and `Value::clone` appearing at 3.2%
where `conv_local` has neither.

The lowering's over-approximation is not the cause and narrowing it would not
help. It collects the *names* used as the root of a `var` argument before it
emits anything, so `root(var v)` written anywhere in a body demotes every `v`
the body declares; but here there is one `v` and it really is the one that is
rooted. What would fix this is a place that can name a scalar slot, which is a
change to what a place is. **This belongs to
[#116](https://github.com/myuon/cove/issues/116)**, under the calling
convention and the typed frame layout, and the size of the prize is the 30% in
this row.

**Reaching a function through a value costs 1.32× reaching it directly, and a
lambda costs nothing over a declaration.** `conv_fnvalue` and `conv_closure`
are 313.6 ms and 312.6 ms, which is the same number: one is
`let f: fn(Int) -> Int = identity` and the other is
`let f: fn(Int) -> Int = fn(n) { n }`, they lower to the same 21.6
instructions a turn, and they run in the same time. That is a negative result
and it is a good one — it says the second specialisation a declaration gets
when it is reached through a value is not costing anything a lambda does not
also pay.

What the 1.32× over `conv_static` is, is the general convention: nothing at a
`call-value` knows which function it will enter, so the argument travels on the
value stack and the answer comes back on it, which is one `scalar-to-value`
and one `value-to-scalar` a turn plus the indirection. `conv_generic` isolates
the first part of that from the second. It is a *static* call to
`fn generic<T>(value: T) -> T`, so it has the same two boundary instructions
and none of the indirection, and it costs 140.0 ns a turn against
`conv_static`'s 118.6 and `conv_closure`'s 156.3. **So two boundary crossings
are 21.4 ns a turn and the indirection at a `call-value` is another 16.3.**
Neither is a cliff. They are what the convention costs where it does not know
the callee, which is the case it exists for, and they are smaller than the
lock at the same call.

**A captured scalar costs 1.16× the closure that reads no capture, and it is
the largest thing in the matrix that a typed capture area would fix.**
`conv_capture` adds one `load-capture` and the addition that consumes it, and
runs five boundary instructions a turn against `conv_closure`'s two: two
`scalar-to-value` and three `value-to-scalar`, because the parameter, the
capture and the answer all cross. `drop_in_place<Value>` and `Value::clone`
are 10.5% of the run. A closure's captures are value slots by construction —
the call copies them out of the closure into the frame's value window — so a
captured `Int` has no scalar representation available to it at all. **This
belongs to [#116](https://github.com/myuon/cove/issues/116)** as well, beside
the typed frame layout: a capture area numbered like the two stacks it sits
between would remove the crossing, and the prize is the 16%.

**A Host callback costs 27×, and two thirds of it is allocation.** This is the
cliff by an order of magnitude and it is the only row in the matrix that
allocates: fourteen allocations and 469 bytes a turn. `conv_fresh` splits it.
Building a closure per turn and calling it in the VM is four allocations and
208 bytes, and costs 399.6 ns a turn against `conv_closure`'s 156.3 — so a
`Value::Closure` is four allocations, and 243 ns a turn is what building and
dropping one costs. The remaining 877 ns and ten allocations a turn are the
Host boundary itself: the argument vector `Vm::take` drains for the call, the
`Result` the operation answers with, the reentry that enters `Vm::execute`
again to run the body, and the clock the boundary reads on every call. The
profile agrees about where it is rather than about what it is —
`nanov2_malloc_type` and `_nanov2_free` are 32.8% of the row between them,
`Vm::execute` is 8.6%, `HostRegistry::call_with` 4.5%, the two clock reads
6.7%, and `Vec::from_iter` 3.0%.

**This belongs to [#109](https://github.com/myuon/cove/issues/109)** and it is
the same two allocation sites that document already names as the next
measurement: the argument vector allocated per builtin and Host call, and the
enum payload that makes a two-word `Ok(x)` cost a `Box` and a `Vec`. This row
is the third witness for both, and the first one that puts a number on what
they cost *together* on a program shaped to pay them. It also adds a site the
earlier list did not have, which is the closure value itself at four
allocations; that one is #109's too, since what it is is a representation.

Nothing here is fixed, and one thing is worth saying about why. Each of the
four is a change to what a slot, a capture, a closure or a payload *is*, and
this document's own history says what happens when those are changed one at a
time against their immediate parent. #116 and #109 are where they are decided
together, and both now have a measured ceiling to decide against.

## What is settled and what is open

**Settled by measurement.** Typed scalar slots, the calling convention,
per-block charging in both directions — what it bought and what it costs — the
fused typed field read, and `Value` at 24 bytes. Every
one of those was taken because a number said to, and two of them — the fused
field read and the 24-byte `Value` — were named as open in an earlier section
of this document before they were.

Three more since, all from issue #123. That the instrumentation the dispatch
path carries is nearly free and that a switch to turn it off would not be:
the counter is worth under two percent on the benchmark most sensitive to it,
and a runtime flag around it recovers none of that, because the branch costs
what the increment costs. That the largest cost on a call path is not the
frame, the arguments or the representation but the mutex the run's `Budget`
lives behind — 40.3% of `benches/call`, of which one acquisition in three has
been removed for 16.6%. And that a boundary instruction is cheap: two
crossings are 21.4 ns a turn and allocate nothing at all, which is the answer
to a question this document had been asking in the other direction.

**Settled by evidence that was missing.** What a trace says. It was the last
of issue #111's three blockers and it was a gap rather than a suspicion: the
differential harness compared what a program answered and never what it did at
the Host API boundary. It now compares the recording both backends write, and
the source-level half of it — the entry, the host calls with their arguments
and results, the task identities, the ending — agrees exactly. What had to be
normalized away is wall time, where a collection fell, what it found live, and
the order two threads reached one sink, and each of those is the backend's or
the scheduler's rather than the program's.

**Settled by writing the contract down.** What each way of stopping a run may
let it do first. It was a claim that the batching was not observable, which was
too strong; it is now a maximum per stop mode, measured by
`crates/cove-runtime/tests/responsiveness.rs` and decided, as far as the two
backends' agreement goes, by
[ADR 0024](adr/0024-a-stop-is-a-bound-not-a-point.md). Writing it down found
three things wrong: a host polling from inside a cancelled task was told the
task was fine, a Host effect could follow a raised stop flag, and a stopped run
lost the fuel it had gathered since its last safepoint.

**Settled by semantics.** The 16-byte floor. It follows from `Int` being a full
64 bits with overflow a broken invariant, and no measurement can move it,
because it was never a question about speed. And the root set: it is the value
stack up to its length and the open task scopes, which follows from the two
stacks being numbered separately and from a place being an index, and no
measurement can move that either.

**Open, and what would settle each.** The inline representation of `Option`,
`Result`, and small enum payloads — settled by building one and measuring
`arrayget` and `chars`, and now `benches/convention`'s `conv_host` too, which
pays for the `Result` a Host operation answers with two million times. The
argument vector allocated per builtin call — the same benchmarks, the same
way. The closure value at four allocations, which the matrix added to that
list. The heap layout the VM owns — settled by what is still allocating once
those are gone, which is exactly the evidence it does not have yet. A moving
collector — not yet asked for by anything measured here, and what it would owe
is now written down rather than assumed, under "Collection is non-moving"
above.

Everything on that list is downstream of the same allocation sites. That
makes them the next measurement whether or not a VM-owned heap is ever built.

Beside them, and separately, four cliffs the calling-convention matrix
measured: the two mutex acquisitions still on every call, a `var`-rooted
local that cannot live on the scalar stack, a closure's captures that cannot
either, and the Host boundary at twenty-seven times a loop turn. "The cliffs"
above says what each would take and which of
[#109](https://github.com/myuon/cove/issues/109) and
[#116](https://github.com/myuon/cove/issues/116) it belongs to. None is
fixed, and the reason is this document's own history: each is a change to
what a slot, a capture, a closure or a payload *is*, and changing those one
at a time against their immediate parent is what
[#126](https://github.com/myuon/cove/issues/126) is.
