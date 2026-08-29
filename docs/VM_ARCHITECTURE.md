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

Safepoints are at: entering the entry, every call, every return, every back edge
at which enough fuel has gathered, and `SAFEPOINT_INTERVAL`, which is read where
the charge is made rather than per instruction — so what it bounds is the fuel
standing when a straight line is entered, and the work between two safepoints is
that plus one straight line, which is bounded by the length of the function's
code.

A back edge asks one question on one schedule. A loop notices any stop — a
bounded call's flag, the run's cancellation, its deadline, its fuel — at the
first back edge at which `BACK_EDGE_FUEL` (64) of fuel has gathered since the
last safepoint, so it stops within 63 fuel plus one turn rather than within one
turn; a loop whose turn charges C fuel stops within ceil(64 / C) turns. One
fact narrows what that gives up: the run's own cancellation was never on the
eager schedule to begin with, because it is read inside `Budget::safepoint`,
which the gathered schedule already gated.

The second fact that used to narrow it is gone. `self.stops` is pushed only by
`Reentry::call_until`, and this backend answered such a call without running any
Cove code, so no VM run could have a flag in that list while a loop turned.
Closures lower now, so a `clock.timeout` around a Cove callback puts a flag
there and the callback's own loops are what it has to stop — on this schedule,
within 63 fuel plus one turn of noticing. That is the same bound the run's
cancellation has always had, and it is now the bound a bounded call has too.

### Host calls and reentry

A Host call goes through the same `HostRegistry` the interpreter uses, so the
grant check, the budget charge, the trace event, and the wait accounting are
the same code and cannot drift. Reentry — a host running a Cove closure, and
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
| `call`      | 1531.2 ms | 298.1 ms | **5.14×** |
| `pure`      |   15.7 ms |   3.1 ms | 5.07× |
| `arith`     |  436.3 ms |  96.0 ms | 4.55× |
| `method`    | 2821.4 ms | 963.6 ms | 2.93× |
| `chars`     | 1809.6 ms | 841.4 ms | 2.15× |
| `arrayget`  | 1402.9 ms | 680.9 ms | 2.06× |
| `field`     |  874.5 ms | 464.2 ms | 1.88× |
| `hostheavy` |    5.1 ms |   4.1 ms | 1.24× |

Those numbers are worse than the ones this table held before closures,
dynamic dispatch, and tasks were lowered, and the difference is real rather
than drift. Against that earlier measurement the VM is 3.5% to 19% slower —
`pure` 19%, `arith` 16.5%, `call` 14.5%, and the collection-shaped ones least
— while the AST column moved between −1.7% and +2.7%, which is the control
that says the machine did not change under them. And the instruction counts
did not move at all: `arith` runs the same 31,142,877 instructions it ran
before, `field` 47,428,595, `method` 59,428,598. **The same instructions ran
and they got slower**, which is the signature of a dispatch loop carrying
more than it uses. That is the cost the place model paid once, measured and
brought back down; three further capabilities each measured within their own
noise and together they did not. It is recorded in
[issue #126](https://github.com/myuon/cove/issues/126) rather than chased
here.

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

## What is settled and what is open

**Settled by measurement.** Typed scalar slots, the calling convention,
per-block charging, the fused typed field read, and `Value` at 24 bytes. Every
one of those was taken because a number said to, and two of them — the fused
field read and the 24-byte `Value` — were named as open in an earlier section
of this document before they were.

**Settled by semantics.** The 16-byte floor. It follows from `Int` being a full
64 bits with overflow a broken invariant, and no measurement can move it,
because it was never a question about speed.

**Open, and what would settle each.** The inline representation of `Option`,
`Result`, and small enum payloads — settled by building one and measuring
`arrayget` and `chars`. The argument vector allocated per builtin call — the
same two benchmarks, the same way. The heap layout the VM owns — settled by
what is still allocating once those two are gone, which is exactly the evidence
it does not have yet. A moving collector and precise roots — not yet asked for
by anything measured here, and the frame metadata it would need already exists,
for the reason given under "Frame metadata" above.

Everything on that open list is downstream of the same two allocation sites.
That makes them the next measurement whether or not a VM-owned heap is ever
built.
