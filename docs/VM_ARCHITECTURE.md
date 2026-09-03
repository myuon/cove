# The VM: what is built, and what is being tried

> **The backend this describes has been deleted.**
> [ADR 0034](adr/0034-one-physical-word-stack.md) replaced it, and at the
> cutover commit the crate then called `cove-ir`, its `Vm` and `FrameVm`, the
> `admits` predicate and the duplicate heap went with it. What runs a Cove
> program now is the linear-memory backend, and
> [docs/LINEAR_VM.md](LINEAR_VM.md) is its design. Read that one for how the
> machine works; read this one for what was measured on the way here.
>
> **`cove-ir`, `vm` and `Vm` below are the deleted backend's, not today's.**
> The replacement was built beside it under the transitional names `cove-lir`
> and `lvm`, and took the plain ones in the commit after the deletion. Every
> occurrence of them in this document predates that and has been left alone:
> nothing here has been reattributed to the backend that now answers to these
> names.
>
> It is kept, unrewritten, for two reasons.
>
> The first is that six accepted ADRs cite its sections **by name** — 0024 and
> 0030 the table of bounds, 0027 and 0029 the control-build methodology, 0028
> "The value representation, audited" and the safepoint list, 0033 the
> safepoint list again — and an accepted ADR is immutable. Deleting a section
> an ADR points at would break the pointer in the one direction the ADR
> convention cannot repair.
>
> The second is that its measurement sections are not about the backend at
> all. "What the measurement itself costs", "What `codegen-units = 1` was
> measured to be worth" and the control-build discipline are how this project
> takes a number, and `Cargo.toml`'s `bench-stable` profile, `cove-bench` and
> `scripts/vm-time.sh` all point here for them. Those are live.
>
> Every performance figure below was taken on the deleted backend. None of
> them has been reattributed and none should be read as a measurement of what
> runs a program today; `cove-bench` measures that one, and the seven-of-nine
> improvement it recorded at the cutover is where the comparison lives.
> The tense of the prose below is the tense it was written in.

> Written as the working document for
> [issue #116](https://github.com/myuon/cove/issues/116) and for
> [issue #109](https://github.com/myuon/cove/issues/109)'s representation
> gate. Both are closed — #116 because its gate passed and its ten acceptance
> criteria are met, #109 because its gate is passed and the answer was that
> width is not where the cost is. This is now the record of what they decided
> and the live list of what they did not, which "What is settled and what is
> open" at the end names issue by issue.
>
> [ADR 0019](adr/0019-executable-ir-and-vm.md) decided that a VM exists and
> that the tree-walking interpreter stays the semantic oracle, and
> [ADR 0022](adr/0022-the-vm-is-the-default-backend.md) made it the default.
> Nothing here revisits either. This is about the VM's *shape*, which ADR 0019
> deliberately did not fix, and it stays a working document until a
> measurement makes one of its remaining choices worth an ADR.

## Two things, and they are not the same

This section was written when the first was true of `vm.rs` and the second was
not. It is kept as the distinction #116's first acceptance criterion asked for,
in the tense it was written in; what is true of `vm.rs` today is most of the
target, and the parts that are not are named at the end of this document.

**The prototype** is what `crates/cove-runtime/src/vm.rs` was at
[PR #114](https://github.com/myuon/cove/pull/114). It removed recursive AST
evaluation and it was faster — 1.3× to 4.2× depending on what a program spends
its time on. But it was still shaped like a flattened tree walker: an operand
is a general `Value`, a frame is a window into a `Vec` of them, and the
instruction set only recently began to say what it is operating on.

**The target** is a typed stack machine: a slot whose type the checker proved
holds that type's own representation, a call follows a written convention over
a contiguous frame, and the heap has a layout the VM owns rather than a Rust
object graph it borrows.

The distinction matters because the prototype's numbers are being read as if
they were the ceiling of a dedicated VM, and they are not. They are the ceiling
of *this* VM. Issue #116 existed to measure the other one, and did: the suite
in "The slice, measured" is what it came back with, and the prototype's range
of 1.08× to 4.0× in ADR 0019 is 1.25× to 6.62× there.

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

A place is one slot — which stack, and where in it — together with the field
positions to walk from what stands there. `bump(var total)` builds one naming
`total`'s slot with no path, and `bump(var c.hits)` builds one naming `c`'s
slot with one step on the end. A slot of the *scalar* stack can be the root,
and then there is no path and there can never be one, because neither `Int`
nor `Bool` has a field; the place carries which of the two words it names,
because that stack keeps no tag and a read through the place has to put one
back. Reading through it clones what it names, which is the
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

**No binding has to move for any of this to work, and one used to.** This
paragraph read the other way until
[ADR 0027](adr/0027-a-place-and-a-capture-name-a-slot.md). A place could only
name the value stack, so a binding a place was rooted at was kept there even
where the checker had settled it as `Int` — and the lowering walked every body
once before it emitted anything to find out which bindings those were. It
collected *names*, not bindings, so it over-approximated across shadowing:
`bump(var total)` written anywhere in a body put every `total` the body
declared on the value stack.

What that cost was not the conversion at the place. It was a conversion on
every read and every write of the binding, for the whole of the body, whether
or not the place was ever built — 1.30× on `benches/convention`'s `conv_var`
row, whose one `root(var v)` is written *outside* the loop it is measured
against. "The cliffs" below is where that was attributed, and
`Inst::PlaceScalar` is what removed it: the place names the slot where the
binding already is. The pre-pass is deleted, and with it the only place in the
lowering where a name rather than a binding decided a layout fact.

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
of the three — it charges nothing for that line at all. So for a line of pure
work neither backend stops in the middle of it. One refuses the whole of it and
the other never measures it, which is a difference in outcome and not only in
`fuel_spent`. [ADR 0024](adr/0024-a-stop-is-a-bound-not-a-point.md) is where
that is decided rather than discovered.

The one place the VM does stop inside a straight line is a Host call, which
hands the standing charge over before it is dispatched — see "A Host call asks
every stop the run has" below, and
[ADR 0030](adr/0030-a-host-call-asks-the-fuel-limit.md).

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
| fuel | a safepoint, and every Host call | `G + T` of overspend, and one refused extent; no Host effect |
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

**A Host call asks every stop the run has.** What stands in front of it is
`Budget::charge_host_call`, which refuses a call from a run that was
cancelled, past its deadline, or over `max_host_calls`;
`crate::interp::stopped_here`, which refuses one from a cancelled task or from
inside a bounded call that has been asked to stop; and, on the VM,
`Vm::charge_at_host_boundary`, which hands the fuel charged since the last
safepoint to the run's `Meter` and asks whether the run may continue. The
first two are the flags, and each costs an atomic load. The third is a budget,
and it is there because a budget has to be *measured* — but measuring it costs
a `mem::take` and a relaxed `fetch_add` since issue #182 made the run's
counters atomics, on a boundary that already locks a mutex for
`charge_host_call`, reads the clock inside it, and reads it twice more to time
the wait.

[ADR 0030](adr/0030-a-host-call-asks-the-fuel-limit.md) is where that is
decided, and it decides one sentence: **no Host call begins once the fuel a
run has been charged has reached its limit.** It holds on both backends. On
the VM it holds because of the flush; on the tree walk it holds already,
because `Interpreter::charge_safepoint` hands `SAFEPOINT_FUEL` over in the
same call that charges it, so that backend holds no pending fuel and its
charged total cannot move while a straight line runs.

**That is a statement about the bound and not about the count**, and the
difference matters. What the two backends still do not share is what a limit
*admits*: a straight line of Host calls charges the VM about two fuel each and
charges the tree walk nothing at all, so forty Host calls with no branch
between them are all refused under a fuel limit of one on the VM and all
performed under any limit that lets the tree walk in at all. Neither is a Host
effect made *after* exhaustion. So the honest sentence is still:
**`max_host_calls` bounds effects, fuel bounds work, and the deadline bounds
time** — an embedder that wants a number sets `max_host_calls`, because how
many calls a unit of fuel buys is a property of the program and of the
backend.

Before ADR 0030 the VM's bound here was `SAFEPOINT_INTERVAL` of standing fuel
plus one block extent, divided by what a Host call charges. Issue #160
measured that at 300 effects for a straight line written to have them, at
every fuel limit tried, because what bounded it was the extent of the block
and not the limit. It is zero now.

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
effect. The other thing the registry cannot do is spend the fuel this backend
has charged and not yet handed over, and `Vm::charge_at_host_boundary` does
that at the same point, so a Host call is refused once the run has charged
past its limit. "What each stop mode may run past it" is where those bounds
are stated.

A re-entrant call meets the same boundary, because it runs on the same `Vm`:
the callback's Host calls reach `Vm::call_host` with the same pending fuel and
the same budget as the entry's do.

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
`Interpreter::call_target` says that in one line, by wrapping the result of the
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
are all compared exactly and all agree over the ninety-four cases that lower.
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

### What that rule does not survive, and what was built instead

The shortfall rule is a rule about `Rc`, and it stops being one the moment a
slot stops being a `Value`. [ADR 0028](adr/0028-five-representations-and-one-is-public.md)
decision 8 says so in as many words: "an index or offset copied into a Rust
local does not change `Rc::strong_count`, so the ADR does not claim that the
current shortfall collector survives such a handle untouched." A handle is
eight bytes of data. Copying one runs no destructor and tells nobody, so an
accounting rule that reads counts is blind to it, and an object owned by a VM
heap rather than by `Rc` is then swept out from under the local that names it.

The decision lists four mechanisms that would restore the invariant and asks
the prototype to choose and test one. `cove_runtime::slot` is that prototype
and the choice is the second: **an explicit temporary-root stack**, with the
push and the truncate paired by a scope the way `heap::SlotRoots` already
pairs them for the tree walk. Its module documentation carries the argument
against the other three; the shape of it is that reference-counted handles
reintroduce per-slot destructors — the cost ADR 0027 built the scalar stack to
remove — and that stack maps over Rust frames are what ADR 0011 ruled out in
advance.

The third mechanism deserves separate mention because it is the one that looks
free: a dispatch discipline under which a collection can happen only when
every live handle is back in a mapped slot. It is not free and it is not
nearly true. `Vm::collect_if_due`'s list of collection sites is exhaustive
about *where* a collection may happen — which is what issue #209 re-confirmed
— and says nothing about what is in a Rust local when one does. Its own text
names five places where something is: a failed `?`, a host call's arguments, a
`lock`'s closure, a scope being left, and a finished task's answer. Every one
of them is load-bearing on the rule above.

None of this is wired into this backend, and that is a finding rather than an
omission. A shadow-root stack over `Value` would be *unsound* here: a `Value`
in a Rust local is already accounted for by its own count, so registering it
in a second root list would yield one reference twice and conceal the very
shortfall that roots it — the failure #192 kept `Vm::arg_vectors` out of the
root set for. The stack is sound over a handle precisely because a handle is
not a counted reference, and it becomes available when a slot stops being a
`Value` and not before.

### Where the two heaps meet, and why that is one place

The paragraph above says the two accounting schemes must not overlap. There is
exactly one place they have to, and ADR 0028 decision 5 names it: a `Value` is
*materialized at the boundary*, so something has to read a VM-owned object and
build one. `slot::Machine::materialise` is that, and it is the temporary-root
stack's first caller that is not a test — a handle goes in, is a Rust local for
the whole crossing, and reading its parts is VM work that reaches safepoints.

The seam is safe because it is one-way and because it copies. No `Value`
stores a handle, so the shortfall rule's arithmetic can never see one; no
VM-owned object stores a `Value`, so the shadow-root stack can never yield
something `Rc` already counts. The two root sets are over disjoint universes,
and "yielded twice" is a question that cannot be asked across the seam. What
comes back is an owned `Value` in decision 5's sense — "not a window onto a
slot, a heap object or a dynamic value" — so from the moment it exists it is
rooted by its own count exactly as `Vm::take`'s argument vector is, and the
object it was made from may be swept without it noticing. That is also what
keeps the assumption under "Where a collection happens, and why it is safe
there" true once slots are handles: nothing outside the runtime is holding a
view into the heap when a collection runs, because a host is holding a copy.

What the boundary demonstrates that the rest of the slice could not is that
the discipline is load-bearing rather than merely available. Removing the push
from `with_root` sweeps the source object in the middle of materialising it,
and the next word read is a use-after-free; the positive and negative tests are
the same program either side of that one call. Nesting needs nothing added:
the root depth at each collection during a two-level materialisation is 1, 2,
2, 1, 2, 2, which is truncate-to-depth and not a case anybody wrote.

Still none of it is wired into `Vm`, for the reason above.

### What a reference map can say about a variable-length tail

Decision 2 asks an object's header for its payload layout "including a
variable-length tail where it has one", and a tail is the one part of an object
whose size the lowering does not know. The slice splits it the only way it can
be split: the layout carries the fixed part, a per-word reference map for that,
and **one** answer for the whole tail; the object's own header carries how many
tail words the allocation asked for.

So a reference map is two rules rather than a bitmap — a set of indices, and a
single bit for the tail — and that is what a variable length permits rather
than a compression of something better. A per-word map of a tail cannot be
written at lowering time, when the length is unknown, and it need not be: the
collector's question about a word is a yes-or-no, and every word of an array
answers it the same way. Both answers are exercised, and the scalar one is the
one worth checking, because a tail is where a walk that read the bits instead
of the map would guess in bulk.

The design consequence is that a tail is the first thing whose reference map
depends on the object and not only on its type. It depends on the *header*,
which is written at allocation and is never a value the program can see — a
weaker dependency than a niche enum layout would need, where one word is both a
value and the thing that says how to read a value. Decision 2 already calls a
niche "more complex because the reference map may have to interpret the word
according to the enum layout"; the tail is the measure of how much the weakest
version of that dependency still costs, which is a header.

Rooting needed nothing new, and that is what the scale was for. A tail of
handles is a run of *siblings* — the case where a second root is load-bearing,
since a nested object is already rooted by the parent that names it and an
argument is not. A spread call whose argument list is one array is that case at
a scale the program chooses rather than the frame, and truncate-to-depth covers
eight roots the way it covered two. The array is the crossing's argument
vector, so nothing roots it: it is swept at the first safepoint inside the
crossing while every element survives, which is what says the shadow stack and
not the tail's reference map is what holds them. Rooting them one at a time
sweeps the rest.

Decision 7's `Elements` guard fits an array's tail and is not needed for it.
The tail materialises as a copy, and `Value::items` answers a plain slice
because a materialisation's storage does sit still; a guard whose lifetime came
from the VM's heap instead would be the lazy window ADR 0028 refuses, since "a
lazy window keeps a `Value` alive against VM storage, which means a host
holding one constrains when a collection may run". What a tail does not reach
is the other half of decision 7: the five types whose identity is observable
are "materialized as handles rather than as copies", and a `Vector` living in
the handle heap could be neither — a copy is what decision 7 refuses for it,
and a handle inside a `Value` would join the two heaps whose disjointness the
section above rests on. Either those types stay in the counted heap and never
become tails, or the seam stops being one-way. Nothing decides that yet, and
the slice does not decide it by picking one.

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
heap layout would actually be answering. The two are
[#183](https://github.com/myuon/cove/issues/183) and
[#184](https://github.com/myuon/cove/issues/184); the heap itself has no issue,
deliberately, and "What is settled and what is open" says why.

### Both were taken, and this is what they were worth

The paragraphs above are left as they were written, because what they argued
is what was then done and the argument is the record of why. Both changes
landed together, each committed and measured separately against one fixed
baseline at `8638f0e` — the discipline
[#126](https://github.com/myuon/cove/issues/126) exists to enforce — at
fifteen samples a side, with a 95% percentile-bootstrap interval on the
median shift. Every figure below is a median with its interval and the sign
is the sign of the shift, so a negative number is faster.

| benchmark    | #183 alone, VM | #183 + #184, VM | #183 alone, AST | #183 + #184, AST |
| ------------ | -------------: | --------------: | --------------: | ---------------: |
| `arrayget`   |        −13.67% |         −37.98% |          −6.14% |           −6.90% |
| `chars`      |         −8.46% |         −32.93% |          −3.73% |           −5.73% |
| `hostheavy`  |         −6.41% |          −2.48% |          −4.68% |           −2.92% |
| `field`      |         −1.41% |          −1.59% |          −1.06% |           −0.83% |
| `method`     |         −2.15% |          +0.95% |          −2.13% |           −1.74% |
| `call`       |         −0.85% |          +0.21% |          −1.01% |           −1.66% |
| `arith`      |         +0.62% |          +3.55% |          −4.27% |           −4.29% |

**`arrayget` is 37 percent faster on the VM and the reason is arithmetic.**
Its loop allocated four times a turn: the `Box` and the `Vec` of the `Option`
that `get` answers with, and the argument vector for each of `get` and
`unwrapOr`. #183 removed the payload's `Vec` and #184 removed both argument
vectors, so one allocation a turn is left where there were four. `chars` is
the same shape and moved the same way.

**`arith` is the control and it says what it always says.** It allocates
nothing and calls no builtin, so neither change can touch what it executes;
it read +3.55% on the VM and −4.29% on the AST interpreter, both inside the
±6% layout band this document records, and both in the direction #179
predicts for a change that adds code to `vm.rs` without adding code to the
path. `method` at +0.95% and `call` at +0.21% on the VM are the same effect
at the same size: neither runs a builtin in its loop. **An interval says a
difference is real; it does not say the difference is your change**, and
these four rows are where that distinction is doing work.

The one figure worth reading twice is `hostheavy`, which was −6.41% after
#183 and −2.48% after both. Nothing in #184 could have made a Host call
slower — it does not touch the Host path, which still owns its argument
vector for the reason below. What moved is layout, and it moved the more
sensitive direction on the run that grew `vm.rs`.

**The Host boundary kept its allocation, deliberately.**
`HostModule::call` takes a `Vec<Value>` and is public API an embedder
implements; the argument vector is one of the fourteen allocations a turn of
`conv_host`, and breaking that signature is not paid for by one of fourteen.
`benches/convention`'s `conv_host` moved from 2583.6 ms to 2386.2 ms after
#183 — the `Result` a Host operation answers with, two million times — and
back to 2471.8 ms after #184, which is the same layout effect `hostheavy`
shows and not a cost of anything #184 did.

So what is still allocating is one `Box` per `Option` a program builds, the
Host boundary's argument vector and the `Result` it answers with, and the
closure value. That is the evidence a VM-owned heap would be answering, and
it is now on the record rather than predicted.

### And the closure, which was the weakest of the three

[#185](https://github.com/myuon/cove/issues/185) was filed with a caveat in
its title — only `conv_fresh` pays for building a closure — and the first
thing it asked for was whether any program does. The corpus answers yes, and
not through the shape that was looked for. No `.cove` file in `examples/`
writes a closure literal inside a loop body. What it writes is a small helper
that builds one in its own body and is called once per element:
`examples/life`'s `population(world, species)` is a `filter` callback over
one capture, called once per creature per tick from `resolve`'s
`for creature in world.creatures`, and `creatureNamed`, `hash`, `sightings`
and `examples/covecheck`'s `runCheck` are the same shape. The two closure
literals that *are* lexically inside a loop are `Shared.lock` callbacks, in
`tests/e2e/tasks_shared` and `examples/covecheck/runner_test.cove`.

What was taken is the cheap half. `Inst::MakeClosure` allocated one
`Rc<str>` per capture, every time, to carry names the VM never reads — a
lowered closure addresses captures by index, and only the interpreter's
`invoke_body` and `Transfer::convert`'s not-task-safe diagnostic read one.
They are now made once per program in `Vm::capture_names`, exactly as
`Vm::constants` already does for a constant string, and `conv_fresh` fell
from 706.3 ms to 623.4 ms against the same fixed baseline. Beside it
`conv_closure` did not move — 224.4 ms to 227.0 ms — which is the check that
what moved is the `make-closure` and not the call around it: **building and
dropping a closure went from 240.9 ns a turn to 198.2 ns.**

What was *not* taken is the representation. Dropping the names from
`Closure::captures` outright saves the same one allocation and no more,
because the pairs vector is one allocation either way, and it costs the two
readers above. One claim in the issue is wrong and worth recording as such: a
closure with no captures does not allocate a `Vec`, because `Vec::new()` and
a `collect()` from an empty iterator allocate nothing.

The gap that remains is a benchmark. `conv_fresh` is still the only row in
the suite that builds a closure per turn, and the shape the corpus actually
has — a callback per element, through a helper — has no row at all. One was
not added here for a reason about attribution rather than about effort: a
row the fixed baseline does not contain cannot be compared against it, and
this round's whole discipline is that every figure is measured against
`8638f0e`.

### The row that gap named, and the allocation it found

[#193](https://github.com/myuon/cove/issues/193) is that row and the thing it
found. `benches/callback` is `examples/life`'s `population()` reduced to the
mechanism — a helper that builds a closure over one capture and hands it to
`filter` — sized to make the same 2,000,000 entries into a body that
`benches/call` makes through the call instruction. It was committed and a
baseline recorded with it present *before* anything on the path it measures
was touched, which is the discipline the paragraph above declined to break.

What it prices is a route nothing else in the suite reaches:
`builtins::walk_with` re-entering the evaluator. `map`, `filter`, `fold` and
`sorted` were allocating a `Vec<Value>` for each element they visited — the
same shape #184 removed from the builtin path, on the one path #184's pool
did not reach. It reaches it now by being handed one level further down: the
callback's arguments go in the vector `Vm::borrow_args` already lent the
builtin, which is empty the moment the callback has been taken out of it, so
`Callable::call_value` drains it and gives it back rather than consuming one
per element.

**The allocation count is the result, and it is exact.** With a counting
global allocator (`scripts/ablate/instrument.patch`'s, applied to
`cove-bench`), `cove-bench --iterations 1` on this machine:

| row | before | after |
| --- | ---: | ---: |
| `callback`, VM | 2,500,119 allocations / 121,510,938 bytes | 500,119 / 73,510,938 |
| `callback`, AST | 22,125,128 / 1,723,320,934 | 20,125,128 / 1,675,320,934 |
| every other row, both backends | — | identical to the digit |

Two million fewer on each backend, which is one per element, and 48,000,000
fewer bytes, which is `size_of::<Value>()` times two million. The VM's row
allocates a fifth of what it did. The AST backend loses the same two million
out of twenty-two, because a slot there carries a label and a span beside its
value and one vector per call is still built for that.

**Timing, as within-build ratios, because #179 says an absolute is not
evidence here.** `cove-bench --iterations 15`, one session, base binary run
first and again last:

| ratio, VM | base | after | base re-run |
| --- | ---: | ---: | ---: |
| `callback ÷ call` | 2.412 | **1.792** | 2.385 |
| `callback ÷ arith` | 4.548 | **3.423** | 4.539 |
| `arrayget ÷ arith` | 4.968 | 4.989 | 4.929 |
| `chars ÷ arith` | 6.674 | 6.644 | 6.705 |

The two base columns are the same binary an hour apart and they bracket the
drift at about 1%; the ratio this change is about moves 25%. The two rows
below it are the control: neither runs a callback, and neither ratio moves.
Per invocation, the VM went from 192.2 ns to 144.0 ns with the base re-run at
190.3 ns — about 48 ns, which is a `malloc` and a `free`. The AST backend
moved `callback ÷ call` from 1.067 to 1.025 with the re-run at 1.071, and
717.8 ns against 752.2 and 754.2 ns per invocation.

Read as raw deltas against the recorded base, the same run says `callback`/VM
−25.11% [−25.63, −24.48] and `callback`/AST −4.58% [−4.88, −4.08], which
agrees. What the raw deltas also say, and what the ratios are here to
discount, is `pure`/VM +10.14% [+8.57, +10.92] on a 1.4 ms row that runs
`fib(20)` and reaches no builtin at all — and −5.28% on the next build, and
−3.99% on the base binary re-run. That is the layout band, on the smallest
row in the suite, behaving exactly as the section below predicts.

Every `fuel_spent` figure is identical across all four builds, on every row
and both backends, which is the check that nothing about what these
benchmarks *do* changed.

**And the other half of #183, measured the same way.** `Value::builtin_name`
was a thread-local `RefCell<Vec<(&str, Rc<str>)>>` scanned by string compare,
twice per `Some` — so a `Some(x)` paid two thread-local accesses, two
`RefCell` borrows and two linear walks to answer with the same two `Rc`s as
last time. The names are eight fields of a thread-local struct now, reached
once per construction. It changes no allocation count (the eight `Rc<str>`
are made once per thread either way, only eagerly rather than one at a time),
so what it buys is work removed rather than memory, and the within-build
ratios are the only thing that can say how much:

| ratio, VM | without | with | the two base runs |
| --- | ---: | ---: | ---: |
| `arrayget ÷ arith` | 4.989 | **4.593** | 4.968, 4.929 |
| `chars ÷ arith` | 6.644 | **6.336** | 6.674, 6.705 |

About −8% and −5% on the two rows that build an `Option` per turn, against an
`arith` that builds none. On the AST backend the same two ratios move 0.6% and
2.5% in opposite directions, which is nothing: the lookup it removes is a
smaller share of a tree walk's per-operation cost.

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

`cove_runtime::Value` holds twenty-two variants and `needs_drop::<Value>()`
is true. (It was a `pub` enum when this was audited; since ADR 0028 it is a
newtype over a private one, which changes none of the widths below.) It was 40 bytes — the number the top of this document reads the
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
`Interpreter::call_target` makes them and is unchanged, the lock is still taken to
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
before it could be claimed; the site is `Interpreter::call_target`'s own
`max_call_depth` read, for whoever asks the question next.

What is left on the call path is two acquisitions a turn, one at the call and
one at the return, and they are still the largest single cost `benches/call`
has. That was a target rather than a finding when this section was written.
The section below is what became of it.

### The other two acquisitions are gone, and the counters are atomics

[Issue #182](https://github.com/myuon/cove/issues/182) asked what the mutex was
protecting, and the answer was: nothing that needed a mutex. Per safepoint a
`Budget` adds to `fuel_spent`, reads the run's cancellation, compares against a
fuel limit fixed before the run began, and every `DEADLINE_CHECK_INTERVAL`th
time reads a clock that started before the run began. The cancellation was
already an atomic flag. `limits` and `started_at` are immutable for a run.
`fuel_spent` and the deadline tick were plain integers *because the struct
holding them was reached by `&mut`*, and for no other reason.

So they are `AtomicU64`s now, `Budget::safepoint` takes `&self`, and the whole
of the accounting lives in one `Arc<Accounting>` that every thread of the run
shares. `crate::budget::Meter` is the handle onto it: a backend takes one where
a run begins — `Vm::new`, `Interpreter::new`, and again in `invoke_within` and
`run_entry_within` after the budget they were handed is installed — and charges
through it at every safepoint after that with no lock at all.
`HostRegistry::with_budget` still exists and still locks; what is left behind it
is installing a budget, reading the counters back for `--stats`, and the two
charges that are not per-instruction (a host call, a spawn).

**Nothing about the schedule moves.** `SAFEPOINT_INTERVAL`, `BACK_EDGE_FUEL`,
`SAFEPOINT_FUEL` and `DEADLINE_CHECK_INTERVAL` are unchanged, which matters
because ADR 0024 states each stop as a bound in those constants' arithmetic. The
order of the three questions inside a safepoint is unchanged, so which stop is
reported is unchanged. Fuel is still counted before anything can refuse, which
is ADR 0024's "pending fuel is never lost". A Host call is still a stop point
for all three flags and for the deadline and `max_host_calls`, which is the
other half of the same decision — issue #120 found real faults in both of those
and `crates/cove-runtime/tests/responsiveness.rs` still measures them.

**The VM, medians of fifteen against a baseline recorded at `6d53791`, with the
95% percentile-bootstrap interval on the median shift:**

| bench       | at `6d53791` | with this |      shift |     95% interval |
| ----------- | -----------: | --------: | ---------: | ---------------: |
| `call`      |       236 ms |    153 ms | **-35.1%** | -35.5% to -34.2% |
| `method`    |       812 ms |    646 ms | **-20.4%** | -20.9% to -20.2% |
| `pure`      |      2.37 ms |   1.35 ms | **-43.2%** | -44.1% to -41.2% |
| `field`     |       432 ms |    423 ms |  **-2.0%** |   -2.4% to -0.9% |
| `arith`     |      86.0 ms |   77.8 ms |  **-9.6%** |  -10.1% to -9.2% |
| `arrayget`  |       666 ms |    652 ms |  **-2.2%** |   -2.7% to -1.6% |
| `chars`     |       818 ms |    805 ms |  **-1.6%** |   -2.2% to -0.9% |
| `hostheavy` |      3.80 ms |   3.89 ms |  **+2.4%** |     0.7% to 3.8% |

**The interpreter, the same run:**

| bench       | at `6d53791` | with this |      shift |     95% interval |
| ----------- | -----------: | --------: | ---------: | ---------------: |
| `call`      |      1531 ms |   1368 ms | **-10.7%** | -10.9% to -10.3% |
| `method`    |      2780 ms |   2589 ms |  **-6.9%** |   -7.4% to -6.6% |
| `pure`      |      15.5 ms |   14.1 ms |  **-9.4%** |   -9.8% to -8.4% |
| `field`     |       829 ms |    778 ms |  **-6.1%** |   -6.3% to -4.2% |
| `arith`     |       429 ms |    363 ms | **-15.4%** | -16.4% to -15.1% |
| `arrayget`  |      1492 ms |   1431 ms |  **-4.1%** |   -4.5% to -3.4% |
| `chars`     |      1916 ms |   1880 ms |  **-1.9%** |   -2.1% to -0.9% |
| `hostheavy` |      4.94 ms |   4.86 ms |  **-1.7%** |   -2.7% to -0.8% |

Four readings.

**`call` captured 35.1% of the ablation's 40.3% ceiling, and `pure` more than
that.** The ceiling was measured by removing the lock *and the accounting*
together, and the accounting is still here — a `fetch_add`, two compares, and
the branch that picks one safepoint in sixty-four to read a clock at. What the
gap between 35.1% and 40.3% prices is that remainder, which is the honest
reading of a partial capture and is why this section does not claim the whole
of it.

**`field` moved 2.0%, where the ceiling was 5.8%.** `field`'s loop calls
nothing, so the two acquisitions a call and a return cost were never its to
save; what it has is back edges, and a back edge already waited for
`BACK_EDGE_FUEL` to gather. That the interval excludes zero at all is the
useful part, and the size of it is inside the band this document records for
layout alone.

**The interpreter moved as much as the VM did, and it was not the target.**
Nothing about the tree walk changed except which side of the lock its
`charge_safepoint` and its `max_call_depth` read are on, and `arith` on the AST
backend is 15.4% faster for it. That is the same lock, at the same
schedule, on a backend that charges a fixed amount per safepoint rather than in
blocks — so it takes the lock *more* often per unit of work, and it is the row
that shows the acquisition's own cost most plainly.

**`hostheavy` on the VM went the other way, 2.4% slower with an interval of
0.7% to 3.8%.** It is the one benchmark dominated by the path that still locks,
so there is nothing here for it to win, and `host.rs` gained a method — which
[#179](https://github.com/myuon/cove/issues/179) says is enough on its own to
move a benchmark that never executes it. `startup` on the interpreter is the
other row that moved the wrong way, 2.8% with an interval of 1.5% to 6.1%, and
it times a process from `exec` to exit with a few milliseconds of Cove in it.
An interval says a difference is real; it does not say the difference is the
change. Both are recorded rather than explained away, and they are the rows a
reader should be most suspicious of.

The whole run is nineteen rows: fifteen improvements, two inside the noise, the
two above. The widest interval that did not clear zero is `startup` on the VM at
-3.4% to +1.9%, so a regression larger than that anywhere in the suite would
have been seen.

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

### A change to `vm.rs` moved a benchmark that cannot execute it

[Issue #179](https://github.com/myuon/cove/issues/179) says that the workspace
has no `[profile.release]`, so release builds with `codegen-units = 16` and no
LTO, and rustc partitions codegen units by module — which makes where code
lives a performance variable independent of what it does. That was reasoned
from the build configuration. It has since been observed directly, and the
instance is worth recording because it is cleaner than the ones that suggested
it.

Measuring [issue #160](https://github.com/myuon/cove/issues/160) meant building
one variant of `Vm`: a private method added to `vm.rs` and called from
`Vm::call_host` and `Vm::call_resource`, and nowhere else. Two `cove-bench
--iterations 15` runs of the unmodified build bracket one of the variant, all
three from the same session on the same machine:

| bench       | base | variant | base again | variant vs base | Host calls |
| ----------- | ---: | ------: | ---------: | --------------: | ---------: |
| `field`     |  432.40 ms | 457.91 ms | 433.63 ms | **+5.9%** | none |
| `method`    |  821.18 ms | 846.96 ms | 813.43 ms | +3.1% | none |
| `arith`     |   88.35 ms |  86.18 ms |  88.10 ms | −2.5% | none |
| `chars`     |  818.09 ms | 828.10 ms | 808.97 ms | +1.2% | none |
| `call`      |  239.51 ms | 240.35 ms | 238.92 ms | +0.4% | none |
| `pure`      |    2.34 ms |   2.32 ms |   2.30 ms | −0.7% | none |
| `hostloop`  |  663.54 ms | 666.07 ms | 643.22 ms | +0.4% | 1,000,000 |
| `hostheavy` |    3.79 ms |   4.07 ms |   3.82 ms | +7.4% | 4,001 |

**`field` is 5.9% slower on a code path it never reaches.** It makes no Host
call, so it never executes the added method, and it runs the same 47,428,595
instructions in all three builds. The two unmodified runs agree with each other
to 0.3%, so the machine did not move under them. What moved is the layout of a
module `field` spends its whole run inside.

The consequence for reading the two host benchmarks is the point. The change
is *about* Host calls, and the only two rows that make any are inside a band
that rows making none demonstrate is at least ±6% wide. So the honest bound on
what the change costs is "less than what adding a function to `vm.rs` costs
benchmarks that cannot call it", and no number smaller than that is available
from this build configuration. `hostloop`'s 1,000,000 Host calls put the
change's own cost at +2.5 ns a call against the two baselines' own 20 ns of
disagreement; `benches/convention`'s `conv_host`, corrected by its `conv_fresh`
control, puts it at +47 ns against a boundary that costs 887. Both are small
and neither is resolved.

This is the fourth time layout has been the answer — [#114](https://github.com/myuon/cove/issues/114)'s
cold match arms, [#126](https://github.com/myuon/cove/issues/126)'s spills, the
calling convention's unattributed 8 ms on `arith`, and now this — and it is the
first where the moved benchmark provably does not run the changed code at all.

### The layout band is much wider than it was thought to be

The section above bounds the band at "at least ±6%", from a build whose added
method `field` never executes. Issue #162's work needed a bound it could state
a design against, so it built the control the earlier measurement could not:
**the base commit, with one `Inst` variant added that is never emitted, never
executed, and reachable from no program.** The variant is matched in
`Vm::execute`'s dispatch group with an `unreachable!` body and in `validate`
with a bound check; nothing else differs from `2c19429`, and every benchmark
runs the same instructions it ran before.

`cove-bench --matrix --iterations 15` and `cove-bench --iterations 15`, base
binary and control binary, interleaved on one machine in one sitting:

| row / bench | base | control | control vs base | instructions |
| --- | ---: | ---: | ---: | ---: |
| `arith` (VM) | 80.53 ms | 99.46 ms | **+23.5%** | identical |
| `conv_var` | 112.64 ms | 125.73 ms | **+11.6%** | identical |
| `conv_local` | 86.32 ms | 91.87 ms | **+6.4%** | identical |
| `chars` (VM) | 566.31 ms | 578.37 ms | +2.1% | identical |
| `conv_host` | 2336.32 ms | 2410.71 ms | +3.2% | identical |
| `field` (VM) | 425.27 ms | 430.13 ms | +1.1% | identical |
| `method` (VM) | 651.16 ms | 653.84 ms | +0.4% | identical |
| `call` (VM) | 154.79 ms | 152.69 ms | −1.4% | identical |
| every AST row | — | — | −0.2% to +3.7% | identical |

**`arith` on the VM is 23.5% slower for a variant no program can reach.** That
is four times the ±6% the section above records and it is on the benchmark
this document has most often read a few percent off. The machine did not move
under it: the AST rows, which share the binary and none of the code, span
−0.2% to +3.7%, and a third run of the base binary agrees with the first two
to 1.4% on `conv_local`.

Three things follow, and they are the reading rules for anything measured on
this workspace until it has a `[profile.release]`:

- **A cross-build absolute is not evidence.** Not for a regression and not for
  an improvement. Two builds that differ by one enum variant differ by 23.5% on
  a benchmark neither of them changed.
- **A within-build ratio is.** Two rows of one matrix run in one binary share
  whatever layout that binary has. `conv_var ÷ conv_local` is 1.30× on the
  base, 1.37× on the control, and 1.00× after ADR 0027 — and the middle column
  is what says the third number is a change and not a build.
- **An instruction count is.** `--stats` counts what ran. #126 proved a count
  is not *sufficient* — three changes with identical counts summed to 19%
  slower — but a count that moved when it should not have, or did not move
  when it should have, is still the cheapest way to catch a mistake, and it is
  the only figure here that no rebuild can touch.

What this does not say is that the band is noise. It is a real cost paid by a
real build; a user running that binary is 23.5% slower on that program. What
it says is that the cost belongs to the *arrangement of the code*, which this
workspace does not control and does not measure, and so cannot be attributed
to a design being compared against another.

### How to read a measurement, now that the harness reports a spread

Everything above was read by eye. `cove-bench` reported `{min, mean, max}`, a
reader compared three numbers against a band held in memory, and the sentence
a refactor wants to write — "no statistically meaningful regression" — was not
a number this repository could produce. [Issue #179](https://github.com/myuon/cove/issues/179)
names that as its third item and it is now done, so the discipline the rest of
this section describes has a tool rather than only a habit.

**Every wall-time series now reports its quartiles and its own samples.** The
`wall_ns` object gained `p25`, `median`, `p75` and `iqr` beside the three
fields it always had, and a `samples` array holding every timing the run took.
The median rather than the mean because a wall-time series has a floor and no
ceiling: the failure mode is a sample that is too *large*, and the mean is the
statistic that moves furthest when one arrives. The interquartile range rather
than `max - min` because the range grows with the sample count — a longer
series has more chances to catch one bad run — and the middle half does not.
`crates/cove-bench/src/stats.rs` argues both at length, including what was
checked about the shape of these distributions and what that check cannot
settle.

**`--baseline <path>` compares this run against a recorded one.** The baseline
format is the harness's own output, so recording one is `cove-bench > file`.
For each row present in both, it prints the shift between the medians and a
95% percentile-bootstrap interval around it, and reads a verdict off the
interval: an interval excluding zero cleared the noise, one containing zero did
not.

That last case is the one this document has needed and been unable to state.
When the interval contains zero, **the interval's width is the honest bound**,
and it is what a "no regression" sentence should quote — not the shift, and
certainly not a claim that the change had no effect. It is the same move the
section above makes in prose when it says the honest bound on issue #160's cost
is "less than what adding a function to `vm.rs` costs benchmarks that cannot
call it"; the difference is that the harness now computes that bound instead of
a reader estimating it.

**Compare against a fixed commit, not the parent.** Unchanged, and the reason
is still #126: three changes each individually inside the noise summed to 19%.
`--baseline` is what makes it cheap — record the suite once on the commit being
measured against, keep the file, and pass it to every run after.

**The ±6% band is per benchmark and the harness now measures it rather than
recalling it.** This section established the band on `arith` by observing
benchmarks move on code they cannot execute, which is why `field`, `pure` and
`call` are the discriminating cases. That band was a remembered number applied
globally. A comparison's interval is derived from the two series being
compared, so a benchmark that is quiet gets a narrow interval and one that is
noisy gets a wide one, without anybody choosing a threshold. What the recorded
±6% is still needed for is the thing no run-to-run spread can see: layout
sensitivity is *systematic* between two builds, not noise within one, so two
tight series can disagree by 6% with both intervals narrow and neither wrong.
**An interval says the difference is real. It does not say the difference is
the change.** That distinction is what the rest of this section is about.

**Six samples is the floor, fifteen is the practice.** `--iterations` is the
sample count — there is no second flag — and below six samples a side the
harness reports the shift and refuses to call it, because no 95%
distribution-free statement about a median exists on fewer. CI still runs
`--iterations 1` and is unaffected: it asserts correctness, never a number, and
a series of one costs it exactly what it cost before.

### What `codegen-units = 1` was measured to be worth, and why the answer is nothing

The two sections above blame layout, and
[issue #179](https://github.com/myuon/cove/issues/179) names the fix its
reasoning implies: give the workspace a profile with `codegen-units = 1`, so
that a crate is one codegen unit and a module boundary stops being a place
rustc can decide to lay code out differently across. Option 2 of that issue
adds it as a *bench-only* profile — `[profile.bench-stable]`, `inherits =
"release"`, `codegen-units = 1`, nothing else, and no LTO, which was always
meant to be a separate measurement — so that `[profile.release]` stays at
Cargo's defaults and CI keeps the 137-second pipeline it was cut to.

It was built, and the control #179 asks for was run under it. **It does not
work, and the round is recorded here because a measured negative result is
worth more than the hypothesis it replaces.**

#### What was run

The same control as the section above: the current commit, built twice, the
second time with **one `Inst` variant added that no lowering emits, no program
reaches, and `Vm::execute` matches only with an `unreachable!` body**. Both
profiles got that pair, so there are four binaries; all four are byte-identical
across a reboot and across `-j 16`, `-j 4` and `-j 2`, so nothing below is
nondeterministic codegen. `fuel_spent` is identical on every row of all four —
every benchmark ran exactly the instructions it ran before.

Six `cove-bench --iterations 15` suites and six `--matrix --iterations 15`
runs, one machine, one sitting, arranged so each variant run is **bracketed**
by a run of its own base binary before and after it. Every figure below is the
variant against the mean of its two brackets, which is the only way to state
one at all — for the reason the next subsection gives.

#### The result

| row | release (`codegen-units = 16`) | bench-stable (`codegen-units = 1`) |
| --- | ---: | ---: |
| `arith` (VM) | **−1.00%** | **−6.01%** |
| `conv_var` | +2.18% | −6.78% |
| `conv_local` | +2.55% | −6.40% |
| `call` (VM) | +1.12% | −5.19% |
| `field` (VM) | −3.51% | −2.79% |
| `method` (VM) | −3.44% | −2.01% |
| `chars` (VM) | +0.65% (AST) | −2.15% |
| `pure` (VM) | −3.87% | −5.70% |
| **largest \|shift\| over 24–27 rows** | **3.87%** | **6.78%** |
| **band width** | **6.42 pp** | **9.53 pp** |

**The spurious shift is larger under `bench-stable`, not smaller.** Where the
default profile spread its 24 rows over 6.4 percentage points, one codegen unit
per crate spread 27 rows over 9.5, and the row #179 leads with — `arith` on the
VM — moved six times further under the profile that was supposed to hold it
still. The shape differs too, and the mechanism is legible: at 16 codegen units
a dead variant perturbs the unit it lands in and leaves the others alone, so the
shifts are small and mixed in sign; at one unit per crate it relays out the
whole crate at once, so nearly every row moves the same way together.

#### The control did not reproduce, and that is the more important finding

The section above records **+23.5% on `arith`** and **+11.6% on `conv_var`**
from exactly this control under exactly this default profile. This round, under
the same profile, the same kind of never-executed `Inst` variant moved `arith`
by **−1.00%** and `conv_var` by **+2.18%**.

So there was no +23.5% here to shrink. That does not make the earlier
measurement wrong — it was taken, and its instruction counts were checked, the
same way this one was. What it means is that **one dead variant is a single
draw from the layout distribution, not a measurement of it.** Two draws of the
same experiment, at different commits, returned +23.5% and −1.00%. A control
built from one perturbation can therefore say that layout sensitivity exists;
it cannot size it, and it cannot be used to score a profile against another,
which is what this round tried to do with it.

#### And the machine moved as much as the code did

Each arm's base binary was run twice, roughly forty minutes apart, with nothing
else on the machine. The same binary disagreed with itself by:

| | release | bench-stable |
| --- | ---: | ---: |
| `pure` (VM) | **−7.40%** | −6.57% |
| `field` (VM) | +4.93% | −1.19% (AST: −5.31%) |
| `method` (VM) | +3.24% | −1.05% |
| `arrayget` (VM) | +2.58% | +3.22% |
| largest \|shift\| | **7.40%** | 6.57% |

**The null is the size of the signal.** A binary compared against itself moved
up to 7.4%, which is more than either arm's variant moved against its bracket.
So neither arm's number above is separable from drift, and the honest statement
about `codegen-units = 1` is not "it is worse" but "**it is not better, and
this machine cannot currently resolve a difference smaller than about 7% between
two runs of anything, including one binary and itself.**"

That is a prerequisite result. Until the same-binary null is brought under a
percent or so, no layout experiment on this workspace can measure a layout
effect, because the thing being measured is smaller than the ruler.

#### What run-to-run spread says, and where the profile does help

Within a single run the harness's own quartiles are small and the two profiles
are the same: **median per-row IQR 1.09% of the median under `bench-stable`,
1.13% under release.** The profile buys nothing there either.

The one place it does help is the fastest rows, where a fixed cost is a larger
fraction of a short run: the **largest** per-row IQR across the timed rows is
**2.12% under `bench-stable` against 11.83% under release**, and `pure` on the
VM — the row that carries most of that — goes from 6.08% to 1.40%. So one
codegen unit does make a short benchmark's series tighter. It just does not
make two *builds* comparable, which is the entire thing #179 wanted.

This is also the clearest illustration of the rule the harness's own comparison
already states: an interval built from within-run samples can be narrow on both
sides and still be measuring a difference that is not the change. Under
`bench-stable`, `arith`/VM's comparison against its base reads
**−6.49% [−7.43, −5.84], "improvement"** — a confident, narrow interval, on a
benchmark whose only difference from its baseline is an enum variant it cannot
reach.

#### What it costs

Both binaries, `-j 4`, this machine:

| | release | bench-stable | |
| --- | ---: | ---: | ---: |
| from scratch, deps included | 34.2 s | 49.2 s | **+44%** |
| rebuild after a `vm.rs` edit | 17.2 s | 33.7 s | **+96%** |
| total CPU, from scratch | 120.3 s | 94.1 s | −22% |

The CPU column is the interesting one: one codegen unit does *less* total work
and still takes longer, because it cannot spread that work across cores. The
penalty is serialization, so it gets worse on a wider machine, not better.

#### The recommendation, and what CI does

**Do not adopt `bench-stable` as the baseline for implementation comparisons.**
It costs 44% to 96% more build time, it does not narrow the cross-build band,
and on this round's evidence it widens it. The profile stays defined so this
measurement can be reproduced and so the next person does not have to build it
again to find out; nothing in the workspace selects it.

`.github/workflows/ci.yml` is untouched and unaffected. It builds `--release`
and runs `cove-bench --iterations 1`, both deliberately, and a profile no step
names costs it nothing.

#### What this changes about how to read a measurement

The rule the section above states — **a cross-build absolute is not evidence on
this workspace** — is not narrowed by any of this. `bench-stable` does not earn
back cross-build absolutes and nothing here suggests a profile that would.

It is **widened**, in a direction that section did not reach. That rule is about
two builds. This round shows the weaker claim fails too: *a same-build absolute
taken forty minutes later is not evidence either*, because one binary moved 7.4%
against itself with nothing changed at all. So:

- **Bracket, do not pair.** A variant run must have a run of its base binary
  before it *and* after it, and the figure quoted is the variant against the
  mean of the two. A single base-then-variant pair cannot tell a change from the
  half-hour that passed between them. Every number in this section is bracketed;
  the sections above that quote a base run once should be read as the weaker
  evidence they are.
- **Quote the null beside the signal.** The two brackets' disagreement with each
  other is the measurement's own error bar, it costs one extra run, and where it
  is as large as the effect — as it is here — that is the result.
- **One perturbation does not size a band.** +23.5% and −1.00% are the same
  experiment at two commits. Sizing layout sensitivity needs several distinct
  dead variants per profile, interleaved with base runs, not one.

#### If the LTO question is asked later

Thin LTO was deliberately excluded so that two changes would not land in one
measurement, and it should stay excluded until the design above is fixed —
running it now would produce another single-draw number of the kind this round
has just shown is uninterpretable. What it would take: at least five *distinct*
never-executed perturbations per profile, each bracketed by base runs, with the
same-binary null reported per row, and the profiles compared on the **spread of
the perturbations** rather than on any one of them. That is roughly six hours of
wall time per profile on this machine, and the first thing it should establish
is whether the ±7% same-binary drift can be brought down at all — because if it
cannot, the experiment cannot resolve anything smaller and should not be run.

### The ±7% was a maximum over two dozen rows, and a row's own error bar is a quarter of it

[Issue #205](https://github.com/myuon/cove/issues/205) took the number the
section above ends on — one binary disagreeing with itself by 7.4% — and asked
what it is, what it is correlated with, and whether it can be brought down.
The first answer changes how every table above should be read, and it is not
about the machine at all.

#### What was run

Twenty-two `cove-bench --iterations 15` suites in one four-hour sitting,
nothing under test changing between any two of them:

- **Six back to back**, one release binary, no other work on the machine.
- **Six more of that same binary**, each preceded by a real incremental
  rebuild (`touch crates/cove-runtime/src/vm.rs`, then
  `cargo build --release -j 4 -p cove-cli -p cove-bench`, 16 s every time) —
  because the session that produced the 7.4% had builds in it and this one had
  to find out whether that mattered.
- **Ten of a second binary**, alternating the two sample orders the subsection
  after next is about, in the sequence `b r r b b r r b b r` so that neither
  arm sits at one end of the session.

Every suite is bracketed by a **direct measurement of the CPU's effective
clock**: a dependent `addq` chain, which retires at exactly one add per cycle
on this microarchitecture, so five hundred million adds timed give gigahertz
without needing root. Five before each suite and five after, 220 probes in
all. `sysctl vm.loadavg`, `machdep.xcpm.ratio_changes_total` and
`pmset -g therm` were recorded at the same points.

#### The machine, since this document has never said

An **Intel Core i7-10700K**, eight cores and sixteen threads, macOS 25G83,
32 GiB, and `vm.swapusage` total `0.00M`. `machdep.xcpm.hard_plimit_max_100mhz_ratio`
is 51 against a 3.8 GHz base, so the hardware is free to move between 0.8 and
5.1 GHz and turbo alone could account for far more than 7%. It did not.

| the clock probe, 220 samples over four hours | GHz |
| ------------------------------------------- | ---: |
| 1st percentile                               | 4.6162 |
| 25th                                         | 4.6730 |
| median                                       | 4.6808 |
| 75th                                         | 4.7053 |
| 99th                                         | 4.7450 |
| the single worst probe                       | 4.5069 |

**The middle half of the machine's clock spans 0.7%**, and `pmset -g therm`
reported `CPU_Speed_Limit` of 100 at every one of its 44 snapshots — no
thermal and no scheduler limit, ever. `powermetrics` needs root and was not
run, so there is **no package temperature and no per-core residency figure
here**; that is a gap in this measurement and is stated rather than guessed
at. What the probe does establish is that whatever moves the benchmarks, the
core they run on is not changing speed by anything like the amount the
benchmarks move.

#### What a row's disagreement with itself actually is

Twelve suites of one binary, 66 pairs per row, the shift between two runs'
medians:

| row | median | 90th | worst |
| --- | -----: | ---: | ----: |
| `arith` (VM)      | 0.25% | 0.53% |  0.84% |
| `field` (VM)      | 0.35% | 0.92% |  1.10% |
| `arrayget` (VM)   | 0.48% | 1.07% |  1.41% |
| `call` (VM)       | 0.55% | 1.79% |  2.72% |
| `chars` (VM)      | 0.78% | 1.42% |  2.42% |
| `pure` (VM)       | 0.84% | 2.43% |  3.20% |
| `arith` (AST)     | 1.20% | 2.58% |  3.29% |
| `hostheavy` (VM)  | 1.41% | 2.97% |  4.26% |
| `field` (AST)     | 1.65% | 3.35% |  4.70% |
| `startup` (VM)    | 1.66% | 3.85% |  5.63% |
| `startup` (AST)   | 2.18% | 5.13% |  6.97% |
| `benches` lowering| 2.28% | 9.29% | 12.01% |
| **all 21 rows pooled** | **0.78%** | **2.58%** | 12.01% |
| **without the lowering and the two `startup` rows** | **0.71%** | **2.18%** | 5.78% |

That is the number this repository did not have. **A row's honest error bar on
this machine is about 0.8% in the middle and 2.5% at the 90th percentile**,
and the quietest row in the suite, `arith` on the VM, never moved by more than
0.84% in 66 comparisons of one binary with itself.

#### It is not the gap between the runs, and it is not the builds

The 7.4% was framed as drift "over forty minutes". It is not.

| gap between the two suites | median | 90th | 99th | worst |
| --- | ---: | ---: | ---: | ---: |
| under 15 minutes  | 0.74% | 2.67% | 5.47% |  9.01% |
| 15 to 45 minutes  | 0.79% | 2.47% | 7.05% | 11.40% |
| over 45 minutes   | 0.78% | 2.66% | 6.05% | 12.01% |

**The three distributions are the same one.** Two suites nine minutes apart
disagree exactly as much as two suites two hours apart, so nothing here is
accumulating with time — not heat, not page-cache state, not uptime. The six
suites with a 16-second `cargo build -j 4` in front of them are not
distinguishable from the six without, either. Whatever this is, it is present
between any two runs and does not grow.

#### The 7.4% was `max over rows`, which is a different statistic

Take the same twelve suites of one binary and compute, for each of the 66
pairs, *the largest shift over the suite's rows* — which is what "disagreed
with itself by up to 7.40%" reports:

| statistic | over all 21 rows | over the 18 execution rows |
| --- | ---: | ---: |
| median | 3.99% | 2.89% |
| 90th percentile | 9.29% | 4.23% |
| worst | 12.01% | 5.78% |
| pairs reaching 7.4% or more | **14%** | **none** |

**7.4% is an ordinary draw from this null.** A maximum over two dozen rows is
a maximum of two dozen samples of a heavy-tailed thing, and its median is five
times the median of any one row. Nothing was wrong with the observation; what
was wrong was reading a suite-wide maximum as a per-row error bar. **It is
not, and no row should be compared against it.**

Which rows carry that maximum is the other half of the answer:

| row | how often it is the largest shift in a null pair |
| --- | ---: |
| `benches` lowering        | 36% |
| `startup` (AST)           | 20% |
| `startup` (VM)            | 12% |
| `hostheavy` (both)        | 14% |
| `field` (AST)             |  8% |
| `callback` (AST)          |  5% |
| the remaining fifteen rows |  6% between them |

The two worst are the two that are not really benchmarks of the runtime's
steady state. The lowering row times a **0.13 ms** operation, so fifteen
samples of it are two milliseconds of measurement; `startup` spawns a process
and pays whatever the operating system charges for that, and its 99th
percentile sample is **eighty times its median** — the page-cache effect ADR
0012 warned about, still there. **Neither is evidence of anything at the
few-percent level and neither ever was.**

#### The shape of a series, which `stats.rs` asked for and could not have

`crates/cove-bench/src/stats.rs` argues for the median from the shape of the
failure and then says, honestly, that the skew argument was not supported by
the only data available — three order statistics from nine-sample series. It
asks whoever next takes a run on a quiet machine to look at the real shape.
Ninety samples a row, pooled over six suites, each expressed against its own
row's median:

| row | 1st | 25th | 75th | 99th | worst |
| --- | --: | ---: | ---: | ---: | ----: |
| `field` (VM)    | −0.86% | −0.33% | +0.34% |  +2.77% |    +2.8% |
| `arith` (VM)    | −1.23% | −0.42% | +0.29% | +34.00% |     +34% |
| `pure` (VM)     | −5.21% | −0.97% | +1.73% | +15.89% |     +16% |
| `startup` (AST) | −7.40% | −1.63% | +18.9% |  +8131% |  +8,131% |

**It is a floor with a long right tail, and the tail is much longer than the
body.** The middle half of a good row spans less than a percent while its
worst sample is tens of percent above the median. So the median was the right
choice for the reason `stats.rs` gives — a decision must not move when one
sample arrives late — and the argument from skew, which that file declined to
rely on, turns out to hold after all. The interquartile range was the right
spread for the same reason: on `arith`/VM, `max − min` is 35% of the median
and the IQR is 0.7%.

#### Bracketing helps, and it helps less than it sounds like it should

The rule the section above adopted — base, variant, base again, quote the
variant against the mean of the two — was never measured. Over consecutive
triples of the twelve suites:

| what is quoted | median error | 90th | worst |
| --- | ---: | ---: | ---: |
| the pair, `B` against one `A` | 0.74% | 2.51% | 9.58% |
| the bracket, `B` against the mean of two `A`s | 0.64% | 2.06% | 9.74% |
| the bracket's own null, the two `A`s against each other | 0.71% | 2.43% | 11.40% |

**Averaging two base runs takes about 15% off the error**, which is roughly
what averaging two draws of anything does, and it does nothing at all to the
worst case. The value of the rule is the third row, not the second: the
bracket's real product is *an error bar that was measured in the same session
as the result*, and that is worth the extra run whatever the average does.

#### What was tried: taking the samples in a different order

`cove-bench` took every sample of one row before starting the next. So a row's
whole series was taken at one instant of a nine-and-a-half-minute suite, and
for the fastest rows that instant is very short indeed — fifteen samples of
`pure` on the VM are twenty milliseconds of measurement, and fifteen of the
lowering are two. Whatever the machine was doing then is the whole of the
row's answer, and nothing in the row's own spread can say so.

`--sample-order round-robin`, now the default, takes one sample of every row
per pass instead. The same rows run the same number of times, the suite takes
the same 564 seconds, and only *when* each sample is taken changes. Five
suites of each order, alternating, one binary:

| over the 18 rows the order governs | blocked | round-robin |
| --- | ---: | ---: |
| median disagreement between two suites | 0.61% | **0.45%** |
| 90th percentile | 1.97% | **1.67%** |
| worst | 4.40% | **3.62%** |
| rows that improved | — | **13 of 18** |

**A quarter of the noise, for nothing.** That is the honest size of it: it is
not a fix, and the sign test on thirteen of eighteen is not overwhelming
either. It is the default because it costs no time, no work, and no output
format — and because it removes a structural embarrassment rather than a
number, namely that a row could take its entire answer from one instant it
could not report.

Two things say not to claim more than that.

**The rows the flag cannot touch also "improved", and they cannot have.** The
lowering row and both `startup` rows are measured outside the loop the order
governs, and their null still came out 2× to 4× smaller in the round-robin
arm. That is two outlier suites landing in the other arm by luck — the
lowering read +8.66% and +6.91% in two blocked suites, `startup`/AST read
+15.37% in one. **Five suites an arm cannot resolve a heavy-tailed row**, so
the all-rows figure (0.80% → 0.51%) overstates what the change did, and the
eighteen-row figure is the one to read.

**The two orders report the same numbers, as far as this can tell.** The
median absolute difference between an arm's row medians is 0.73% and the worst
is 2.77% — the same size as the null itself, so there is no evidence that
round-robin measures anything different. A baseline recorded under one order
can be compared against a run under the other; it just has one more source of
disagreement in it than a same-order comparison does.

One detail is worth reading the other way round. `pure` on the VM went from a
within-run IQR of 1.86% to 2.87% while its *between*-run disagreement halved.
The series got wider and the answer got steadier, which is exactly what should
happen: a series spread over the suite starts including the variation a series
taken at one instant was blind to. **The wider interval is the more honest
one.**

#### The rule, narrowed

The section above says a same-build absolute taken forty minutes later is not
evidence. That was too strong, and it was too strong in a specific way: it
generalised a maximum over rows into a bound on every row. What this round
supports:

- **A row's error bar is about 0.8% at the median and 2.5% at the 90th
  percentile on this machine**, per row, per pair of runs. Not 7%. A
  difference of 3% on a single execution row, seen twice, is outside the null;
  the earlier rule would have thrown it away.
- **Never quote the suite's largest shift as an error bar.** Its median on a
  null is 4% and it reaches 15%. Quote the row.
- **The lowering row and both `startup` rows are not evidence** at anything
  under about 10%. They carry two thirds of every null maximum.
- **Bracket anyway.** Not because averaging the two base runs is worth much —
  it is worth 15% — but because the two base runs' disagreement is the only
  error bar measured in the same conditions as the result.
- **Time between runs is not a variable**, and neither is an incremental build
  between them. Both were measured and neither is.
- **This is the floor, and it is close.** The machine's own clock holds to
  0.7% through its middle half, `arith`/VM's null is 0.25%, and the remaining
  rows sit between that and the machine. There is no large remedy left to
  find here; what is left is arithmetic — more samples, more perturbations —
  and the reason to want it is layout, which is a property of the builds and
  not of the machine.

`--sample-order blocked` reproduces every "blocked" figure above, and nothing
selects it otherwise.

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

**That table was recorded before [#182](https://github.com/myuon/cove/issues/182)
removed the budget's mutex from the safepoint, and every row of it moved.** The
same command on the build that removed it:

| row            |   median |     min |     max | vs base | instructions | per turn | ns/turn |
| -------------- | -------: | ------: | ------: | ------: | -----------: | -------: | ------: |
| `conv_local`   |  84.2 ms |   83.3  |   88.3  |   1.00× |   35,142,879 |     17.6 |    42.1 |
| `conv_var`     | 113.3 ms |  112.0  |  115.4  |   1.35× |   39,142,890 |     19.6 |    56.7 |
| `conv_static`  | 153.5 ms |  152.0  |  158.0  |   1.82× |   37,142,877 |     18.6 |    76.8 |
| `conv_generic` | 185.7 ms |  184.1  |  210.1  |   2.21× |   41,142,877 |     20.6 |    92.8 |
| `conv_fnvalue` | 225.8 ms |  224.3  |  228.1  |   2.68× |   43,142,879 |     21.6 |   112.9 |
| `conv_closure` | 226.1 ms |  223.4  |  226.6  |   2.69× |   43,142,879 |     21.6 |   113.0 |
| `conv_capture` | 275.0 ms |  273.4  |  282.9  |   3.27× |   53,142,883 |     26.6 |   137.5 |
| `conv_fresh`   | 700.0 ms |  693.3  |  716.1  |   8.31× |   47,142,877 |     23.6 |   350.0 |
| `conv_host`    | 2418 ms  | 2373    | 2427    |  28.72× |   51,142,877 |     25.6 |  1209.2 |

Every instruction count is identical, which is the check that nothing about
what these rows *do* changed. What changed is the constant under every call and
every return.

The prose below is written against the recorded run and is left as written,
because it is about representation and the questions it answers do not move:
what a `var` root costs, what reaching a function through a value costs, what a
capture costs, what a Host callback costs. Two of its *numbers* now read
differently and are worth re-reading rather than patching. The boundary
decomposition inverts — two crossings are 16.0 ns a turn against the
indirection's 20.2, where they were 21.4 against 16.3 — and its closing
sentence, that both "are smaller than the lock at the same call", is no longer
a sentence about anything, because there is no lock at the call. Re-reading the
matrix against the new constant is its own exercise and belongs to whoever asks
the next question of it.

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

**A call cost 2.53× a loop turn, for one more instruction, and the reason was
the budget's lock — which is gone.** `conv_static` ran 18.6 instructions a turn
against `conv_local`'s 17.6 and took 71.8 ns longer. Nothing about
representation changed between the two rows: the argument is a scalar slot, the
answer comes back on the scalar stack, and the count says so by not moving.
What happened was two safepoints, and each was an acquisition of the mutex the
run's `Budget` lived behind. The ablation above put 40.3% of `benches/call`
there and the profile put 36% there.

This was the largest cliff in the matrix that anything could be done about, and
[#182](https://github.com/myuon/cove/issues/182) is where it was done: the
budget's counters are atomics, `Budget::safepoint` takes `&self`, a backend
takes a `Meter` where a run begins and charges through it with no lock, and the
schedule ADR 0024 and `crates/cove-runtime/tests/responsiveness.rs` fix is
untouched. `benches/call` is 35.1% faster for it and `benches/pure` is
43.2%; "The other two acquisitions are gone, and the counters are atomics"
above is the measurement, including what it did *not* capture of the 40.3%
ceiling and why. The isolated pair says the same thing without a benchmark
around it: `conv_static` against `conv_local` was 2.53× and 71.8 ns a turn, and
is 1.82× and 34.7 ns, on the same 18.6 instructions a turn against 17.6. So the
per-call constant is less than half what it was, and what is left of it is the
accounting itself — a `fetch_add` and two compares.

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
change to what a place is. **This belonged to
[#162](https://github.com/myuon/cove/issues/162)**, which inherited it from
#116 and is where one slot identity for parameters, locals, temporaries,
scalars, references and places is decided. The size of the prize is the 30% in
this row.

**It was taken in full.** `Inst::PlaceScalar` names a slot of the scalar stack
and the pre-pass is deleted; `conv_var` now runs `conv_local`'s instructions
exactly — 35,142,890 against 35,142,879, which is 17.6 a turn against 17.6 —
and is **1.00×** of it in the same binary, where it was 1.30× — and 1.37× in
a control build that changed nothing.
[ADR 0027](adr/0027-a-place-and-a-capture-name-a-slot.md) is the decision and
"The layout band is much wider than it was thought to be" below is why the
ratio is stated and the absolute is not.

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
belonged to [#162](https://github.com/myuon/cove/issues/162)** as well, beside
the typed frame layout: a capture area numbered like the two stacks it sits
between would remove the crossing, and the prize is the 16%.

**About half of it was taken, and what is left is not a capture cost.**
`Function::captures` pairs each capture's name with the stack its slot is in,
the call fills each capture into the slot its own kind names, and
`Inst::LoadCapture` is gone — a capture is read by the `load-scalar` or the
`load` every other binding of that kind is read by. `conv_capture` runs one
instruction a turn fewer, 25.6 against 26.6, and is **1.10×** of
`conv_closure` in the same binary where it was 1.23×, and 1.20× in a control
build that changed nothing. The four instructions a
turn that remain are two of work — the read and the addition that `+ zero`
*is* — and two of the general convention: a closure's parameter arrives on the
value stack and its answer leaves on it, because nothing at a `call-value`
knows which function it will enter. Neither is a capture, and `conv_closure`
does not pay them only because its body hands the parameter straight back.

The conversion is not gone, it has moved: what a closure holds is
`(name, Value)` pairs on both backends, because a host reads them and because
one lambda is one function however many specialisations of the body around it
are lowered, so a scalar capture is converted **once per call in place of once
per read**. ADR 0027 is where the soundness of that is argued, and there is a
test on each side of the boundary for the case that makes it necessary — a
declaration reached both directly and through a value, whose two
`make-closure` sites disagree about the representation the capture had where
it stood.

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

**This belongs to [#184](https://github.com/myuon/cove/issues/184) and
[#183](https://github.com/myuon/cove/issues/183)**, split out of #109 when it
closed, and they are the same two allocation sites this document already named
as the next measurement: the argument vector allocated per builtin and Host
call, and the enum payload that makes a two-word `Ok(x)` cost a `Box` and a
`Vec`. This row is the third witness for both, and the first one that puts a
number on what they cost *together* on a program shaped to pay them. It also
adds a site the earlier list did not have, which is the closure value itself at
four allocations; that one is [#185](https://github.com/myuon/cove/issues/185),
since what it is is a representation.

Nothing here is fixed, and one thing is worth saying about why. Each of the
four is a change to what a slot, a capture, a closure or a payload *is*, and
this document's own history says what happens when those are changed one at a
time against their immediate parent. #109 and #116 were where they were decided
together; each now has a narrower issue of its own, and every one of those
carries the measured ceiling it has to be decided against.

Three of them were then fixed, and the paragraph above is left as written
because its argument is why they were fixed the way they were: together, in
one sitting, each committed separately and each measured against one fixed
baseline rather than against the one before it. `conv_host` fell from
2583.6 ms to 2288.2 ms and `conv_fresh` from 706.3 ms to 623.4 ms across the
three; "Both were taken, and this is what they were worth" above has the
whole table and says which row moved for which reason, and which rows moved
for no reason anybody here can name.

## One slot identity, and what the two cliffs it owned were worth

[ADR 0027](adr/0027-a-place-and-a-capture-name-a-slot.md) is the decision and
issue [#162](https://github.com/myuon/cove/issues/162) is where it was asked
for. In one sentence: **a place names a slot rather than a stack, and a
capture takes the slot its own kind names.** Nothing about a slot's role —
parameter, local, temporary, capture, or the root of an alias — decides which
stack it lives in any more; only the checker's answer about its type does.

### What was measured, and against what

Every figure below is from one machine in one sitting, with three binaries
interleaved so that the machine is a control on itself:

- **base** — `origin/main` at `2c19429`, built once and kept;
- **control** — the same commit with one `Inst` variant added that is never
  emitted and never executed, which is the layout band made visible (see
  "The layout band is much wider than it was thought to be" above);
- **after** — this change.

The base binary was re-run three times across the session and agreed with
itself to 1.4% on `conv_local` and to 5.5% on the slowest AST row, so the
machine held. The control moved `arith` on the VM by 23.5% and `conv_local` by
6.4% for no behaviour at all, so **the cross-build absolutes below are
recorded and are not the evidence.** The evidence is the two columns after
them.

### The matrix

`cove-bench --matrix --iterations 15`, VM backend, medians:

| row | base | control | after | after ÷ base | instructions/turn, base → after |
| --- | ---: | ---: | ---: | ---: | ---: |
| `conv_local`   |   86.32 ms |   91.87 ms |   91.32 ms | +5.8% | 17.6 → 17.6 |
| `conv_var`     |  112.64 ms |  125.73 ms |   91.42 ms | **−18.8%** | 19.6 → **17.6** |
| `conv_static`  |  155.36 ms |  155.21 ms |  159.00 ms | +2.3% | 18.6 → 18.6 |
| `conv_generic` |  184.72 ms |  189.35 ms |  194.92 ms | +5.5% | 20.6 → 20.6 |
| `conv_fnvalue` |  225.36 ms |  233.99 ms |  240.30 ms | +6.6% | 21.6 → 21.6 |
| `conv_closure` |  226.73 ms |  234.02 ms |  244.23 ms | +7.7% | 21.6 → 21.6 |
| `conv_capture` |  279.66 ms |  279.66 ms |  267.65 ms | **−4.3%** | 26.6 → **25.6** |
| `conv_fresh`   |  622.34 ms |  629.78 ms |  622.76 ms | +0.1% | 23.6 → **24.6** |
| `conv_host`    | 2336.32 ms | 2410.71 ms | 2336.71 ms | +0.0% | 25.6 → **26.6** |

### The two ratios that are the result

| | base | control | after |
| --- | ---: | ---: | ---: |
| `conv_var` ÷ `conv_local` | 1.30× | 1.37× | **1.00×** |
| `conv_capture` ÷ `conv_closure` | 1.23× | 1.20× | **1.10×** |

**The `var`-rooted local is not a cliff any more; it is not a cost at all.**
`conv_var` and `conv_local` were always the same loop written twice, differing
in one line placed outside it, and they now run the same instructions — 17.6 a
turn each — and the same time. The 30% the assessment named was the whole of
what the representation was costing, and the whole of it is gone.

**Half of the captured scalar is gone and the other half is not a capture.**
`conv_capture` runs one instruction a turn fewer and is 1.10× of
`conv_closure`. The four instructions a turn that remain over `conv_closure`
are the read and the addition that `+ zero` *is*, plus the two the general
convention imposes on any closure body that does arithmetic: the parameter
arrives on the value stack and the answer leaves on it, because nothing at a
`call-value` knows which function it will enter. `conv_closure` escapes those
two only by handing its parameter straight back.

### The one row that went the other way, and why it was accepted

`conv_fresh` and `conv_host` each run **one more** instruction a turn. Both
build a closure per turn that captures the loop's `i` — a scalar — and whose
body does nothing with it but answer it. The capture is a word now, so
answering it is a `scalar-to-value` where before the capture was already a
value and answering it was nothing. That is the shape a capture's kind
following its *binding* rather than its *use* gets wrong, and ADR 0027 names
it under "What is not decided here". Neither row's wall time moved outside the
band, and the rows the trade is for moved the way it was made for.

### The suite

`cove-bench --iterations 15`, VM rows, against a base run taken in the same
session, with the control build's own movement beside it and the AST backend's
beside that. (Against the *recorded* base at `2c19429`, taken at the start of
the session, every row of every build is 2–8% higher and `startup` is 18–20%
higher on both backends: the machine drifted over the session, which is why
the base binary was re-run inside it and why that re-run is the column
compared against.)

| bench | control vs base | after vs base | the same benchmark on the AST backend, after vs base |
| --- | ---: | ---: | ---: |
| `arith` | **+23.5%** | +4.7% | +1.1% |
| `arrayget` | +1.6% | +4.6% | +3.8% |
| `pure` | +2.8% | +4.1% | +0.7% |
| `call` | −1.4% | +2.4% | −2.2% |
| `field` | +1.1% | +1.8% | +3.8% |
| `method` | +0.4% | +1.0% | −1.3% |
| `chars` | +2.1% | +0.4% | +2.8% |
| `hostheavy` | +0.6% | −0.6% | +5.3% |
| `startup` | −2.3% | +0.3% | +1.2% |

Not one of these benchmarks builds a place or reads a capture, and every one
of them runs exactly the instructions it ran before: every `fuel_spent` figure
is identical across all three builds, which is what the table is a control on
rather than a measurement of.

Two columns say what the middle one is worth. The control build, which changed
nothing a program can reach, moved `arith` five times as far as this change
did. And the AST backend — which shares the binary and runs none of the
changed code — moved by as much or more on four of the nine rows. There is no
attributable shift on any of them, in either direction.

## What a character costs, and what a receiver costs on top of it

[Issue #99](https://github.com/myuon/cove/issues/99) measured `examples/cq`
before any of the work above and reported two findings in one sentence:
per-character text processing costs about 1.4 µs, and a struct method call
doubles it. Both were the AST interpreter's, on a runtime where
`Value::Struct` was a `Box` and a non-mutating method copied its receiver.

Re-measured on the machine and build every table above was taken on, over the
same shape: 1,984,000 characters through `chars.get(i).unwrapOr("")` and one
comparison, with the length reached by the same route as the character, so the
three rows differ in that route and in nothing else. Medians of five runs of
`cove run --stats`; the first row is `benches/chars`.

| how the character is reached  |      AST |       VM | per char, VM | #99, then |
| ----------------------------- | -------: | -------: | -----------: | --------: |
| a local `Array<String>`       | 1,750 ms |   808 ms |      0.41 µs |   1.35 µs |
| the same through a field      | 2,412 ms |   856 ms |      0.43 µs |   1.87 µs |
| the same through a method     | 5,015 ms | 1,246 ms |      0.63 µs |   2.70 µs |

**A character costs 0.41 µs on the backend `cove run` uses, where it cost
1.35.** ADR 0022 made the VM the default, so the number the issue's headline
names is now 3.3× smaller than the number it names.

**A method call adds 54% where it doubled, and a field adds 6% where it added
38%.** Two calls a character is what the third row runs, so 54% over two calls
is 27% a call, and the mechanism the issue named for it is gone: `Value::Struct`
became an `Rc<StructValue>` under issue #104 for exactly this reason, and its
doc comment says so. The instruction counts say where what remains is. The
three rows run 21.1, 23.1 and 30.2 instructions a character, at 19.3, 18.7 and
20.8 ns an instruction — a per-instruction cost that barely moves. So a method
now costs the instructions a call is, plus the per-call constant "The cliffs"
priced at 71.8 ns and attributed to the budget's mutex rather than to the
convention or to the receiver. The receiver half of #99 is answered; what was
left of it was the cliff [#182](https://github.com/myuon/cove/issues/182)
owned, and that mutex is gone — the same constant is 34.7 ns now.

**What has not changed is that fuel does not track what is expensive.** The
issue observed `arith` at 32 M fuel/s against this loop's 7.5 M, on the AST
interpreter, and that ratio is intact there: 46.9 M against 11.5 M. On the VM
the gap is *wider*, 367 M against 61 M, because the arithmetic loop is what the
typed slots made cheap and a heap `Rc<str>` per character is what they did not
touch. `fuel_per_sec` is a rate of charging and not a rate of work, and this is
the sharpest case of it in the suite.

The application the issue was written about moved with the floor.
`cq revenue-summary` over a generated 100,000-record, 17 MB input costs 29.1 s
on the VM and 81.6 s on the interpreter, against the 90.8 s
`examples/cq/README.md` recorded and the 112 s the issue's text quotes: about
3,400 records a second where it was about 900. What has not moved is the shape
of the answer — the work is still per-character interpretation, and the
remaining `Option` per index and `Rc<str>` per character are the two the "open"
list below still names.

## One physical frame, measured

[Issue #162](https://github.com/myuon/cove/issues/162)'s title asked to unify
the VM's logical stack and frame layout.
[ADR 0027](adr/0027-a-place-and-a-capture-name-a-slot.md) unified a slot's
*identity* and said, under "What is not decided here", that a single physical
frame was "not built, not measured, and not refused".
[ADR 0028](adr/0028-five-representations-and-one-is-public.md) decision 1 then
decided what a slot *is* — eight bytes, untagged, one numbering, one base —
and left the same question open in the same words: "**the physical
arrangement is a measurement question and is not decided here.**"

[Issue #212](https://github.com/myuon/cove/issues/212) is that measurement.
`crates/cove-runtime/src/frame.rs` is the vehicle: one contiguous `Vec<u64>`,
one frame base, one index space for parameters, locals and temporaries,
running the same `cove_ir::Program` the `Vm` runs. It is Phase A of #197 and
**not a third permanent evaluator** — it admits a closed scalar subset,
refuses everything else by name before any side effect, and `cove run` cannot
select it.

### The answer

`benches/arith`, `benches/call` and `benches/pure` execute on it and agree with
both existing backends. Five `cove-bench --iterations 15` suites,
`--sample-order round-robin`, quiet machine; the ratio is the two rows of one
run, which is what ADR 0029 says is repeatable:

| row | VM | 8-byte frame | frame ÷ VM |
| --- | ---: | ---: | ---: |
| `pure` — nothing but calls | 1.482 ms | 0.936 ms | **0.63×** |
| `call` — a call per turn | 175.5 ms | 120.1 ms | **0.68×** |
| `arith` — the loop alone | 98.8 ms | 91.4 ms | **0.93×** |

The error bar is the five suites' disagreement with each other, per row. The
`frame ÷ VM` ratio came back

| row | the five ratios | band |
| --- | --- | ---: |
| `pure` | 0.605 0.632 0.634 0.635 0.637 | 3.2 pt |
| `call` | 0.681 0.682 0.684 0.684 0.686 | 0.5 pt |
| `arith` | 0.917 0.925 0.928 0.928 0.933 | 1.6 pt |

and every row's own null across the five was under 1.2%, except `pure`, which
times 1.5 ms and moved 4.3%. **Three of the five are one build and two are
another**, rebased onto four commits that changed only Markdown — so this
particular comparison also survived the one thing ADR 0029 says a comparison
usually does not, which is being taken from two builds. That is a bonus and
not the method: the method is that each ratio is two rows of one run.

Dynamic instruction counts are **exactly equal** on the two backends — 229,862
on `pure`, 31,142,877 on `arith`, 37,142,877 on `call` — because the frame
executes the same lowered code and charges the same block extents. So none of
the above is fewer instructions. It is the same instructions costing less.

**The order is the whole of the argument, and it is ADR 0019's order again.**
What gains most is what is most about calling, and what gains least is the one
row that never calls anything. The three stacks never cost `benches/arith`
anything, because its loop touches exactly one of them — so unifying them
returns 7%, which is the dispatch loop's own difference and not a
representation's.

### What a call costs, decomposed

Both rows turn 2,000,000 times and `call` runs three more instructions a turn
than `arith` — the call, the callee's body, the return. Subtracting one row
from the other prices a call and a return directly:

| | VM | 8-byte frame |
| --- | ---: | ---: |
| `arith`, per turn | 49.4 ns | 45.7 ns |
| `call`, per turn | 87.7 ns | 60.1 ns |
| **a call and a return** | **38.3 ns** | **14.4 ns** |
| the three instructions, at the row's own rate | 9.5 ns | 8.8 ns |
| **what the call costs beyond its instructions** | **28.8 ns** | **5.6 ns** |

`benches/pure` says the same thing from the other side and was not used to
derive it. `fib(20)` is 21,891 calls and 229,862 instructions, so a call is
67.7 ns on the VM and 42.8 ns on the frame — **24.9 ns saved per call**,
against the 23.9 ns the `call` row's subtraction gives. Two rows that share no
arithmetic agree on the size of the thing to within a nanosecond.

That is what one frame buys, and it is exactly what it was predicted to buy:
three `Vec::resize`s and three `Vec::truncate`s become one, three bases become
one, three counts on an `Inst::Call` become one, and a `Frame` of 32 bytes
becomes a `Call` of 12.

### Bytes per live frame, and the stack

A live frame is a 12-byte `Call` record plus eight bytes per slot, against the
VM's 32-byte `Frame` plus eight bytes per scalar slot plus twenty-four per
value slot:

| function | 8-byte frame | VM |
| --- | ---: | ---: |
| `arith.main` — two slots | 28 B | 48 B |
| `call.identity` — one slot | 20 B | 40 B |
| `pure.fib` — one slot | 20 B | 40 B |

The one stack reserves 4,096 words — 32 KB — when a `FrameVm` is built, and
no admitted row comes near it: `crates/cove-runtime/src/frame/tests.rs`
asserts that all three stay under the reservation, and `benches/pure`'s
twenty standing frames are the deepest of them.
`crates/cove-runtime/tests/frame_allocation.rs` counts the other half with a
global allocator: **ten thousand extra calls and ten thousand extra returns
reach the allocator zero times**, on the frame and on the `Vm` alike. The
per-call allocation figure was never the difference between the two
arrangements; the width of the window and what moving through it costs is.

`Value` operations in the hot path are **zero**, structurally rather than by
care. `frame::admits` refuses any function whose `value_frame_size` is
nonzero, so no frame word is ever a `Value` and there is no `Vec<Value>` frame
to be one. Six instructions materialise a `Value` at the boundary, they exist
for the `assertEqual(...)?` and `Ok(())` both benchmarks end with, and each
increments a counter: `benches/arith` and `benches/call` report **8** for a
run of two million turns, all eight in the epilogue.

### The reservation is a measurement fix, and this is what it fixed

The 32 KB reservation is not a capacity guess for a stack `arith` needs five
words of. It is there because without it the two loop rows came back
**bimodal across processes of one unchanged binary**.

Ten processes, same binary, quiet machine: `benches/arith` on the frame
measured 91.1 ms in seven of them and 110.7–112.2 ms in three, and
`benches/call` measured 119.4–120.5 ms and 142.6–143.6 ms in exactly the same
processes — always the two together, never one without the other. Each mode
was internally tight, under 2% over fifteen samples. The `Vm` rows measured in
those same processes held to 1.5% throughout, and `benches/pure` — whose hot
data is the frame *stack* rather than two locals inside one frame — did not
move at all.

So it was neither the machine nor the build. It was something a process
decides once and then keeps, that reaches a loop indexing two words of a small
heap buffer and does not reach a loop walking a deep one. Reserving 32 KB
takes that buffer out of the size class where a small allocation's placement
inside a cache line is decided by the allocator's history, and the modes went
with it: nine processes of the reserved build — three at fifteen iterations
and six at three — are all in the fast mode, with per-row spread of 1.5% and
3.0%.

**What is established is that the bimodality is gone, and not why it was
there.** Cache-line straddling of the word stack is the hypothesis the fix was
chosen from; it was not confirmed, because confirming it needs the address the
buffer actually landed at in each process and that was not recorded. A
symbol-level profile was attempted and is not here either: `samply
--save-only` writes an unsymbolicated profile, and the decomposition above is
arithmetic over measured medians and exact instruction counts, which needs no
sampler.

It belongs beside "The layout band is much wider than it was thought to be"
and ADR 0029's null study rather than as a correction to either. Those two
established that *code* layout is a performance variable this workspace does
not control, worth up to 23.5% between builds, and that one row's null within
a build is under 1%. This is a third thing: a **data** placement variable
worth about 20%, constant within a process and varying between processes of
one binary — which the null study could not have found, because it measured
the shipped binary, and the shipped binary's hot loop does not have a small
heap buffer at the centre of it.

The practical rule it adds to ADR 0029's: **a row that disagrees with itself
between processes of one binary is not a row about the code.** Run the binary
several times before quoting a ratio from it. The first suite taken here read
`arith` at 1.13× and would have been reported as a regression.

### What this says about #162

The physical-arrangement question #162's title asks is answered **yes, and the
prize is at the call.**

ADR 0027 recorded the surprise that neither of the two cliffs #116 handed to
#162 needed one physical stack — both were a slot's *role* deciding its
representation, and both were fixed by taking that decision away from the
role. That stands, and this adds the other half of the sentence: what one
physical frame is worth is not those cliffs at all. It is the **per-call
constant**, and it is 24 ns of it, which is 5× what a call costs beyond its
own instructions.

That is a bigger prize than the two cliffs were, and it is on a different
axis. A program that calls nothing gains 7%. `examples/cq` is a call per
record per field.

### What Phase A did not measure, and what Phase B owes

The subset is scalars only, so nothing below is measured and nothing below is
predicted from what is:

- **Heap-backed values.** PR #210 and #211's `slot.rs` — the VM-owned handle
  and its shadow root — is the mechanism, and it is Phase B. Every figure
  above is from a backend in which no frame word is ever a reference, so it
  says nothing about what a rooted frame costs to walk.
- **A `Value` in a frame slot at all.** `admits` refuses one, which is what
  makes the zero above structural. What a *mixed* frame costs — a word-wide
  slot stack with a GC bitmap, which is #162's Design B — is the next
  measurement and this one does not stand in for it.
- **`Float` in a slot.** ADR 0028 decision 1 puts it there and `cove_ir::Scalar`
  is still `Int | Bool`, so a `Float` is still a `Value` and this backend
  refuses every function holding one. What is proved is the word: all 64 bits
  survive the codec and a real frame, NaN payloads and both zeroes included.
- **Places, `var`, closures, `dyn`, enums, tasks, Host calls, strings,
  collections.** Refused by name. Six of the nine benchmark rows have no
  `frame` line for one of those reasons, and the harness prints which.
- **A second word.** Every value here is one slot. ADR 0028 allows a value
  location to span adjacent slots and a `Dynamic` to be two; nothing here
  builds or measures one.

## The mixed frame, measured

Phase A of [issue #212](https://github.com/myuon/cove/issues/212) ran three
rows over one contiguous `Vec<u64>`, and **no word of it was ever a
reference**: `frame::admits` refused any function with a nonzero
`value_frame_size`, which is what made its "no `Value` in the hot path" claim
structural rather than careful — and what made it silent about the question
this section answers.

Phase B is [issue #162](https://github.com/myuon/cove/issues/162)'s **Design B
proper**: a word-wide slot stack with a GC bitmap, over a VM-owned traced
object heap. `benches/field` and `benches/method` execute on it, agree with
both existing backends, and run **exactly the instruction counts the `Vm`
runs** — 47,428,595 and 59,428,598 — and spend exactly the fuel it spends.

[ADR 0033](adr/0033-an-identity-is-not-a-vm-heap-object.md) is what makes that
target legal, and it is what chose it. Clause 6 puts plain copyable aggregates
— strings, arrays, structs, ordinary enums — in the VM-owned handle heap, and
clause 3 puts the five identity-bearing kinds outside it. A struct is the
smallest of the aggregates, `benches/field` is a struct field read and write,
and neither it nor `benches/method` needs a `Vector`, a `Shared`, a `Task`, a
`TaskScope` or a `Resource`. **So the classification did not constrain what
could be targeted — it named the target.** Where it did bind is the boundary:
`make-builtin` refuses a reference argument, so nothing crossing out of this
backend carries a handle, which is clause 1 held by refusal rather than by
care.

### The bitmap, and how a word is known to be a reference

One bit per word of the one stack, packed sixty-four to a limb. It is the whole
of what a collection consults, and ADR 0028 decision 1's invariant — "a slot
the layout calls scalar must never be reachable by a walk that treats it as a
reference" — holds because the walk has nothing else it *could* read.

A bit is written by one of three authorities, and never by looking at the word:

| where the word is | what says whether it is a reference |
| --- | --- |
| a frame slot | the frame map, derived from `cove_ir::Function`'s two frame sizes; one read-modify-write per limb per call |
| an operand the scalar core, a `const` or a `make-struct` pushed | the instruction, which knows what it pushed |
| an operand a field read pushed | the **object's** reference map |

The third is the one that cannot be static: `get-field-at` is one instruction
whose answer is a handle for a struct-typed field and scalar bits for an `Int`
one, and only ADR 0028 decision 2's reference map knows which.

The first has a condition attached, and `admits` enforces it. The frame map
calls **every** value slot a reference, so a value slot holding anything else
would be scalar bits the walk reads as a handle — decision 1's invariant broken
from the other side. The lowering can produce one: ADR 0027 records that a
declaration reached through a value is lowered "with every argument on the
value stack", so a slot `cove_ir` calls `SlotKind::Value` may hold an `Int`. So
a `store-local`, and a value argument of a call, are admitted only where the
instruction that pushed the word says it is a reference.

**A pop writes no bit.** The word above the top is stale and is never read,
because the walk stops at `words.len()` and every push writes its own bit
before that word is inside the walk. So the bitmap costs a masked store per
push, one read-modify-write per limb per call, and nothing per pop or per
return.

### The answer

Six `cove-bench --iterations 15` suites, `--sample-order round-robin`, quiet
machine; **each ratio is the two rows of one run**, which is what ADR 0029 says
is repeatable, and the six suites are six processes of one binary, which is
what "The reservation is a measurement fix" says to take before quoting one.

| row | what it does | VM | mixed frame | frame ÷ VM |
| --- | --- | ---: | ---: | ---: |
| `pure` | nothing but calls, no reference anywhere | 1.48 ms | 1.33 ms | 0.90× |
| `arith` | the loop alone, no reference anywhere | 98.8 ms | 109.3 ms | **1.11×** |
| `call` | a call a turn, no reference anywhere | 175.2 ms | 169.2 ms | 0.96× |
| `field` | a struct in a frame slot, two field reads and a field write a turn | 477.9 ms | 234.0 ms | **0.49×** |
| `method` | the same, with both reads behind a call | 699.7 ms | 384.5 ms | **0.55×** |

The error bar is the six suites' disagreement with each other, per row, and
there is no bimodality left in it:

| row | the six ratios | band |
| --- | --- | ---: |
| `field` | 0.489 0.489 0.489 0.490 0.491 0.491 | 0.2 pt |
| `arith` | 1.103 1.105 1.105 1.106 1.108 1.110 | 0.7 pt |
| `pure` | 0.897 0.897 0.902 0.903 0.905 0.906 | 0.9 pt |
| `method` | 0.546 0.549 0.550 0.550 0.555 | 0.9 pt |
| `call` | 0.955 0.959 0.962 0.968 0.969 | 1.4 pt |

Dynamic instruction counts are **exactly equal** on the two backends —
47,428,595 on `field` and 59,428,598 on `method`, beside `arith`'s 31,142,877
and `call`'s 37,142,877 — and so is the fuel each run spends, which
`the_frame_spends_the_fuel_the_vm_spends` asserts for all four rows. So none of
the above is fewer instructions or a different amount of program.

**The result is two-sided, and the two sides are the whole of it.** A row whose
frame holds a reference runs at half the VM's time. A row whose frame holds
none runs *slower* than the VM, for the first time in this experiment. Neither
is a surprise once the other is stated: the bitmap is a second buffer the hot
loop writes on every push, and `arith` is the row that pushes most per unit of
work and has nothing for the bitmap to say about any of it.

### What a rooted frame costs to walk

Per instruction, which is the comparison the equal counts make available:

| row | VM | mixed frame | |
| --- | ---: | ---: | --- |
| `pure` | 6.42 ns | 5.78 ns | |
| `arith` | 3.17 ns | **3.51 ns** | the only row where the frame is slower |
| `call` | 4.72 ns | 4.56 ns | |
| `field` | 10.08 ns | 4.93 ns | |
| `method` | 11.77 ns | 6.47 ns | |

`arith` is the whole of the negative result, and it is where it should be. It
is the row with the most pushes per instruction and it has no reference in it
at all, so every bit it writes is a bit about a word that will never be one.
Phase A's build read the same row at 0.93× where this one reads 1.11× — a
comparison that crosses a build, which
[#179](https://github.com/myuon/cove/issues/179) and ADR 0029 say is worth an
indication rather than a measurement, and the indication is that the bitmap
costs a scalar loop something of the order of what one physical frame won it.

**A within-build price for the bitmap alone is not available**, because the
control for that is the same backend without one, and that is a different
build. What *is* within one build, and is the finding, is the trade: in one
binary, in one run, a loop with a reference in its frame gains 2× and a loop
with none loses 11%.

### Whether the per-call prize survives rooting

It does, and it is **larger**. Both pairs are two rows of one run, and the
second pair is the first with a struct in the frame instead of two scalars.

`call` differs from `arith` by one call a turn — 2,000,000 calls, three more
instructions each. `method` differs from `field` by putting each of two field
reads behind a call — 4,000,000 calls, three more instructions each.

| | VM | mixed frame | saved per call |
| --- | ---: | ---: | ---: |
| a call and a return, frame of scalars — `call` − `arith` | 38.2 ns | 30.0 ns | **8.2 ns** |
| a call and a return, frame with a reference in it — `method` − `field` | 55.8 ns | 37.7 ns | **18.1 ns** |

A call whose callee opens a frame the collector has to walk, and whose argument
is a handle standing for a moment in no frame at all, is cheaper on the mixed
frame than on the VM by **more than twice** what a scalar call is — in the same
binary, which is the only comparison worth anything here. So the per-call prize
is not what rooting spends. What the bitmap spends is per *word pushed*, which
is why the row it costs is the one that pushes and never calls.

`benches/pure` says the same from the other side and was not used to derive it:
21,891 calls, 67.4 ns each on the VM and 60.7 on the frame.

### Where the win on `field` actually comes from, which is not the frame

Half the VM's time on `field` is a large number and it would be wrong to credit
the frame layout with it. **It is the object header.**

A `Vm` struct is an `Rc<StructValue>` holding a `Vec<(Rc<str>, Value)>`: the
field *names* are in every object. Writing a field is `Rc::make_mut`, which
copies the cell and the vector — two allocations and two `Rc<str>` clones —
every turn of `benches/field`, because the local still holds the struct the
operand was cloned from. A traced-heap object is a layout id and a run of
words, and its names are in the layout, so the same write is two words into an
entry the free list handed back.

`crates/cove-runtime/tests/frame_allocation.rs` is where that stops being an
argument: ten thousand extra struct field writes reach the allocator **zero**
times on the frame and at least one per write on the `Vm`, counted with a
global allocator in one process.

That is ADR 0028 decision 2's "what it names carries a layout id, the object's
size, its reference map, its payload layout" *in VM-owned metadata* — priced on
one loop. The honest decomposition of `field`'s 0.49× is: an object header
instead of per-object field names, minus the bitmap's cost on the same loop's
pushes, plus whatever one physical frame was already worth. The first term is
the large one. The row does not separate them and this document does not claim
it does.

The live set is 16 bytes throughout — one `Cursor` of two words — against two
million objects allocated, so the collector's own work inside those 234 ms is
a sweep of a table the free list keeps at two entries and a walk of a frame of
three words, every sixty-four allocations, which is `crate::heap`'s own pacing
floor.

### The three multiplicities, and the shadow stack that stays empty

ADR 0028 decision 8 distinguishes three multiplicities and says they must not
be conflated. Under a bitmap over one stack:

1. **Root storage locations are yielded once** — one bit, one visit. A struct
   standing in a frame slot *and* in the operand word about to become a
   callee's parameter is **two locations and one object**, and both are
   yielded: de-duplicating handles is not the walk's job.
   `a_reference_in_a_slot_and_in_an_operand_is_two_locations_and_one_expansion`
   pins it, over a real run of a real loop, asserting that some collection saw
   at least two locations while no collection expanded more than one object.
2. **Real graph edges counted once each** does not arise, because there is no
   `Rc::strong_count` to compare against. That absence is exactly why a bitmap
   over words is sound where a shadow stack over `Value` would not be —
   [PR #210](https://github.com/myuon/cove/pull/210)'s finding, which ADR 0033
   preserves and this change does nothing to weaken. **Nothing in the frame's
   root set is a `Value`**, so the bitmap cannot become a second path to
   anything `crate::heap` already yields, which is #192's `arg_vectors` failure
   and ADR 0027's place exclusion, avoided by the two universes being disjoint
   rather than by anybody remembering.
3. **Objects are expanded once during marking** — asserted equal to the live
   set on every collection of every rooted row.

**The shadow-root stack is wired and stays empty**, and that is a finding
rather than an omission. Decision 8's third candidate mechanism — "the dispatch
discipline guarantees that a collection can occur only when every live handle
has been returned to a mapped VM slot" — is *false* for `Vm` at the five places
`crates/cove-runtime/src/slot.rs` names, and is **true here by construction**,
because a one-stack backend has nowhere else to put an operand. It stops being
free the moment an aggregate crosses decision 5's boundary, which is Phase C's,
so `TempRoots` is present and read by a test rather than absent:
`nothing_is_rooted_outside_the_one_stack`.

### Proving the rooting, which is two mutations

A rooting claim nobody can make fail is not a claim, so each half of the walk
is removed on its own and the run is made to die of it. Both run a real
program, under `HandleHeap::stress` so that neither depends on which safepoint
the pacing happened to choose, and both fail on the heap's own use-after-free
message rather than on an assertion somebody wrote.

| mutation | what it removes | what happens |
| --- | --- | --- |
| `a_value_slot_is_a_root_across_the_loop_it_lives_in` | the frame's own words | the struct in slot 0 is swept out from under the loop using it, and the next `cell.at` panics with `names a swept object` |
| `a_call_argument_is_a_root_before_the_callee_has_a_frame` | the operand words | the argument of a call is swept between the caller pushing it and the callee's base arriving under it, and the callee's first field read panics the same way |

The second is the half a *static* stack map would have to cover and the frame's
own reference map does not: the word is above the caller's frame and below a
callee that does not exist yet, and `Inst::Call` takes a safepoint there
because ADR 0024 says every call is one. Each mutation has a control that runs
the same program with the walk whole and agrees with the `Vm`, and every
positive test asserts that collections actually ran and that the sweep actually
reclaimed something — a rooting test over a run that collected nothing is
vacuous, and this document has been caught believing one before.

### What Phase B did not measure, and what Phase C owes

The subset gained the struct and nothing else, so nothing below is measured and
nothing below is predicted from what is:

- **A reference map in `cove_ir`.** An `Inst::MakeStruct` names its type and its
  field names and nothing else, and `cove_ir::Function` numbers slots without
  saying what is in one beyond `params` and two counts. So a backend that needs
  decision 2's reference map cannot read one off the lowering, and this one
  reads it off the *construction* instead — the `fields.len()` instructions
  before a `make-struct` are what pushed its words, and a type built two ways
  that disagree is refused by name. That is the largest single thing Phase C
  owes, and it is one thing rather than two: the same absence is why the frame
  map is derived at run time from two frame sizes instead of being lowered as
  one numbering.
  *Done, in "A struct's reference map is a property of the type" below — and
  it was two things rather than one. The frame map is still derived, and that
  section says why the per-field kind did not and could not close it.*
- **A `set-field` whose target type is known statically.** The field-position
  table is per field-name constant, so two admitted structs that put the same
  field name at different positions refuse the function that writes it. A
  per-instruction type map would settle it and is lowering work.
- **An aggregate at decision 5's boundary.** `crate::slot::Machine::materialise`
  exists, is tested, and is *not wired here*: `make-builtin` refuses a
  reference argument, so no struct crosses out of this backend. Decision 5's
  own cost — "the boundary can only get more expensive" — is therefore still
  unmeasured for anything larger than a word, and the copy a tail costs is the
  measurement #211 already named and nobody has taken.
- **The inbound half.** ADR 0033 clause 1 keeps it closed on purpose. Nothing
  here consumes a `Value` into the heap and nothing here needs to.
- **Every one of ADR 0033's identity obligations.** Clause 7 asks for one
  explicit handle kind and one reference-map entry per external identity class,
  and lifecycle tests for storage in a frame, storage in a heap object's field,
  a Host round trip, Host reentry, task exit and collection. **None of that is
  here**, and that is the ADR being followed rather than deferred: Phase B
  stayed inside the VM-owned traced heap, where a struct belongs, and did not
  bring a `Vector`, a `Shared`, a `Task`, a `TaskScope` or a `Resource` into
  it.
- **Arrays, strings and enums.** `crate::slot` has the variable-length tail and
  a `Shape::Str`; the frame has no `make-array`, no `concat` and no enum
  layout. `Map` and `Set` remain blocked on the `Part` that can say "a key",
  which ADR 0033 clause 6 leaves open.
- **A field write that does not copy.** A struct is a value, so writing a field
  is a copy; `Vm` reaches the same point holding an `Rc` and calls
  `Rc::make_mut`, which copies when another holder exists and mutates in place
  when none does. A traced heap keeps no count and cannot tell those apart, so
  it always copies — right in both cases, and strictly more work in one of
  them. Whether a uniqueness analysis or a real place model recovers the
  in-place write is a lowering question and is not asked here.
- **Places, `var` parameters, closures, `dyn`, tasks, Host calls.** Refused by
  name, as in Phase A. Four of the nine benchmark rows still have no `frame`
  line and the harness prints which construct stopped each.
- **The bitmap's alternatives.** A `Vec<bool>` would make a push a plain byte
  store instead of a read-modify-write and make the walk read a byte per word
  instead of skipping sixty-four at a time; a static per-`pc` operand map would
  move the cost out of the loop entirely at the price of a table the size of the
  instruction stream and a second thing for the lowering to keep true. Neither
  is built. What is measured is the form #162 names.

## A struct's reference map is a property of the type

Phase C is the first item on the list above, and it is not a change to the
physical arrangement at all. The stack, the bitmap, the calling convention, the
frame map and the boundary are Phase B's unchanged. What changed is where one
fact comes from.

**`cove_ir::StructType` carries one `SlotKind` per field**, in declaration
order, settled from the checker's answer about the declared field type through
`lower::convention::slot_kind_of` — the same function that decides a
parameter's slot, a local's and a return's. The checker already publishes a
struct's field types, as the `params` of the signature it synthesizes for the
initializer `Cursor(at: 0)`; this is that answer read once and written down,
not a second resolution of the same annotations.

Two instructions name a type instead of describing one. `Inst::MakeStruct` is
`MakeStruct(StructId)`, one id where it carried a type-name constant and a
field-name constant. `Inst::GetFieldAt` is `GetFieldAt { of, at }`, where `of`
is the type the checker settled for the receiver — the position always said
*where* the word is, and the type is what says what it **is**. Listings are
unchanged: `make-struct m.Cursor fields=at,step` and `get-field-at 0` read as
they did, because the renderer reads the same names out of the type.

### The third authority is static, and there are now two of it

| where the word is | Phase B | Phase C |
| --- | --- | --- |
| a frame slot | the frame map | unchanged |
| an operand the scalar core, a `const` or a `make-struct` pushed | the instruction | unchanged |
| an operand a field read pushed | the **object's** reference map, per execution | the **lowered type** the instruction names |

Phase B wrote that the third "cannot be static", and that was true of an IR
that recorded nothing about a field. It is a table lookup now: one indexed
load out of a `Vec<Vec<bool>>` built before the run, addressed by the
`StructId` the instruction carries and then by the position. What it replaced
was `HandleHeap::word_is_reference` — an object-table index, a layout id, and
`Vec::contains` over the layout's reference list — on the hot path of every
field read of `benches/field` and `benches/method`.

The object is still asked, under `debug_assert`, on every field read of every
debug build, so what a test run checks is the two answers **agreeing** rather
than one of them being trusted. `get-field-at-scalar` asks nothing at all: the
lowering emits it only where the checker settled the field's own type as `Int`
or `Bool`, so its answer is scalar by construction.

### The by-name refusal is gone, and it is gone by being unstateable

Phase B derived a struct's reference map from the `fields.len()` instructions
before each `make-struct`, so **a type built two ways that disagreed had no
single map and every function that built it was refused, by name.** That
refusal is not diagnosed differently here. A fact neither construction states
is a fact no two constructions can disagree about, so the case cannot arise
and there is no code for it.

What `admits` still asks about a `make-struct` is the other half, per site and
with its span: whether the words this site pushed **are** what the type says
its fields are. That is the question `store-local` already asks, and ADR 0027
is why it survives a static map — a declaration reached through a value is
lowered "with every argument on the value stack", so a word a value slot holds
may be an `Int`.

One by-name table is left, and it is a different one: `Inst::SetField` carries
a `Const::Name`, because the lowering writes a field by name whatever the
checker settled, so the frame backend asks every declared type where that name
stands and refuses a write two of them answer differently. `lower::expr::assign_field`
already resolves the base's type through `Body::field_of` and throws it away.
That is Phase D's and it is now the *only* place a struct field costs a name.

### What it widened, and the coverage it was widened on

One shape: **a struct-typed field read whose answer is then stored, passed, or
built with.** `pushed_kinds` had no reading for `Inst::GetFieldAt` while only
the object knew what it pushed, so `var inner = outer.inner` and
`take(outer.inner)` were both refused. Both are admitted now.

The widening is taken only because a test runs it.
`a_nested_struct_read_into_a_slot_is_rooted` runs both shapes against the `Vm`
and the tree walk, under `HandleHeap::stress`, and asserts that collections
ran and that the program that abandons an object a turn swept one.

### Proving it, which is a third mutation

`a_field_reads_bit_comes_from_the_lowered_type` empties the map a field read
reads its bit out of, and nothing else: the frame map still names every value
slot and every operand word is still in the walk.

| mutation | what it removes | what happens |
| --- | --- | --- |
| `a_field_reads_bit_comes_from_the_lowered_type` | the per-field kind a `get-field-at` writes its bit from | `Outer(inner: Inner(n: 1), n: 2).inner` pops the outer, so the inner stands in one operand word and in nothing else; the call under it is a safepoint, the inner is swept, and `inner.n` in the callee panics with `handle Handle { index: 0, generation: 1 } names a swept object` |

The program is chosen so that neither of Phase B's two halves can cover for
this one. The outer object is consumed by the read itself, so no frame slot
holds it; the inner exists only in the word the read pushed. Its control is
`a_nested_struct_read_into_a_slot_is_rooted`, which runs the same program with
the map whole and agrees with the `Vm`.

### The frame map, which one fix did not close

Phase B said the missing per-field kind was "one thing rather than two: the
same absence is why the frame map is derived at run time from two frame sizes
instead of being lowered as one numbering". **Having removed the absence, they
are two, and this is the negative result of Phase C.**

A struct's reference map really was missing from the IR: nothing said what a
field held, a backend had to invent an answer, and two inventions could
disagree. A frame's reference map is not missing. It is `value_frame_size` and
`scalar_frame_size`, which `cove_ir::Function` has always carried and which say
exactly which slots are references; `frame::FrameMap` is three additions over
them, computed once per function when a `FrameVm` is built and never during a
run. Putting a `Vec<SlotKind>` beside them would move where the addition
happens and change no answer.

What Phase B was pointing at is the **numbering**, and that is a different and
larger change: `Inst::LoadScalar` and `Inst::LoadLocal` address two spaces, and
merging them means renumbering every slot the lowering hands out and changing
what those two instructions' operands mean — in the `Vm`, which numbers three
stacks, as much as here. What it would buy is three named refusals: a function
taking both a value and a scalar parameter, a call passing both, and a value
parameter beside a scalar slot. It is Phase D's, as its own piece of work.

### The measurement

Six `cove-bench --iterations 15` suites, six processes of one binary, quiet
machine; **each ratio is the two rows of one run**, and the figure quoted is
the median of the six. Instruction counts are exact and are printed by
`the_frame_executes_exactly_the_instructions_the_vm_executes`, which asserts
they are equal on both backends before printing them.

| row | instructions | VM | mixed frame | frame ÷ VM | band over six |
| --- | ---: | ---: | ---: | ---: | ---: |
| `pure` | — | 1.50 ms | 1.34 ms | 0.894× | 8.1 pt |
| `arith` | 31,142,877 | 98.86 ms | 105.29 ms | **1.065×** | 6.7 pt |
| `call` | 37,142,877 | 177.24 ms | 166.28 ms | 0.935× | 5.3 pt |
| `field` | 47,428,595 | 483.27 ms | 207.59 ms | **0.428×** | 2.3 pt |
| `method` | 59,428,598 | 720.64 ms | 359.42 ms | **0.495×** | 5.0 pt |

The counts are Phase B's, to the instruction, and so is the allocation
behaviour: 2,000,001 traced objects on `field` and on `method`, none at all on
`arith` and `call`, and zero Rust allocations for ten thousand extra calls and
returns under `tests/frame_allocation.rs`. **Nothing about what these programs
do changed. Only where one fact is read from did.**

Per instruction, which the equal counts make available:

| row | VM | mixed frame |
| --- | ---: | ---: |
| `arith` | 3.17 ns | **3.38 ns** — still the only row where the frame is slower |
| `call` | 4.77 ns | 4.48 ns |
| `field` | 10.19 ns | 4.38 ns |
| `method` | 12.13 ns | 6.05 ns |

And the per-call prize, both pairs being two rows of one run:

| | VM | mixed frame | saved per call |
| --- | ---: | ---: | ---: |
| frame of scalars — `call` − `arith`, 2,000,000 calls | 39.2 ns | 30.5 ns | **8.7 ns** |
| frame with a reference — `method` − `field`, 4,000,000 calls | 59.3 ns | 38.0 ns | **21.3 ns** |

**The bands are wider than Phase B's** — 2.3 to 8.1 points against 0.2 to 1.4 —
and the reason is a process rather than a row. One of the six suites is the
high end of `arith`, `call`, `field` and `method` at once, which is the
process-level effect "The reservation is a measurement fix" describes and is
exactly why the number quoted is a median of six rather than a suite. `pure`'s
8.1 points is 8.1 points of a 1.5 ms row.

**`arith` did not stop being slower than the VM, and this change could not have
made it so.** `benches/arith` executes no `get-field-at` at all, so the only
thing Phase C touched is absent from it. The negative result Phase B recorded
stands as recorded: a frame with no reference in it pays the bitmap a bit per
word pushed and gets nothing back, and it is 6.5% slower than the `Vm` here.

#### Whether the static map is what moved `field`, which is an indication

`field` reads 0.428× where Phase B read 0.489–0.491×, and `method` 0.495×
where Phase B read 0.546–0.555×. **That comparison crosses a build, so it is an
indication and not a measurement**, for the reason ADR 0029 and
[#179](https://github.com/myuon/cove/issues/179) give and the reason Phase B
could not price the bitmap alone: the control for "the same backend that asks
the object instead" is a different binary.

What can be said is the shape of it. Against Phase B's recorded absolutes, in
the same rows:

| row | field reads per turn | frame, Phase B | frame, Phase C | change |
| --- | ---: | ---: | ---: | ---: |
| `field` | 2 | 234.0 ms | 207.59 ms | **−11.3%** |
| `method` | 2, both behind a call | 384.5 ms | 359.42 ms | **−6.5%** |
| `arith` | 0 | 109.3 ms | 105.29 ms | −3.7% |
| `call` | 0 | 169.2 ms | 166.28 ms | −1.7% |
| `pure` | 0 | 1.33 ms | 1.34 ms | +0.8% |

`arith`'s −3.7% is the size of the term a rebuild moves on its own, since
nothing in this change can reach that row. The two rows that read fields moved
three times and twice that. That is consistent with what was removed — an
object-table index, a layout id and a `Vec::contains` over the layout's
reference list, replaced by one indexed load, on every `get-field-at` — and it
is not proof of it. The `Vm` rows moved by −0.1% to +3.0% across the same two
builds, which is the other half of the same caveat.

### What Phase C did not do, and what Phase D owes

Phase C changed where one fact comes from. Everything Phase B listed and did
not build is still not built, and this adds three that Phase C either found or
sharpened:

- **One numbering in `cove_ir`.** The paragraph above: not a consequence of the
  per-field kind, and the three refusals it would remove are named there.
- **A `set-field` that names its type.** The last by-name table in the frame
  backend, and the fact it wants is already computed and discarded in
  `lower::expr::assign_field`.
- **A per-`pc` operand-kind analysis, to replace `frame::pushed_kinds`.** It is
  a peephole: the `count` instructions before a `make-struct` are `count`
  operands only where every operand took one instruction to compute.
  `Cursor(at: 0, step: 1)` satisfies that and `Cursor(at: i, step: 1)` does
  not, so the second is refused although every word in it is readable. The
  per-field kind could not fix this and was never going to: the type says what
  the *fields* are and this asks what the *stack* holds, which is a question
  about a program point. `cove_ir::lower::validate` already simulates operand
  *depths* over every path control can take; the same simulation carrying kinds
  is the answer, and the `make-builtin` boundary needs the exact scalar kind
  rather than only "is it a reference", so it is the wider of the two.
- **Everything else on Phase B's list**, unchanged and not restated here: an
  aggregate at decision 5's boundary, the inbound half ADR 0033 clause 1 keeps
  closed, every one of clause 7's identity obligations, arrays and strings and
  enums, a field write that does not copy, places and `var` and closures and
  `dyn` and tasks and Host calls, and the bitmap's alternatives.

## One slot numbering, and a simulation in place of a peephole

Phase D of [issue #212](https://github.com/myuon/cove/issues/212) is the first
of these phases to change the **production `Vm`**, and it is the item Phase C
separated out and handed on: the numbering.

[ADR 0028](adr/0028-five-representations-and-one-is-public.md) decision 1 says
"one logical frame, one slot numbering, one base", and closes the obvious
escape in the same paragraph — "a physically split realization is legal only if
it presents the same single logical numbering and derives every physical offset
from the one frame layout; **three independently numbered stacks and three
independent frame bases are not one logical frame.**" The lowering had three.
`load-scalar 0` and `load 0` named two different slots and both were called
slot 0, so there was no number in the IR that named *a slot of a frame*.

There is now, and it is the numbering the one-array backend already realized:
**the scalar region, then the value region, then the place region, from one
origin.**

### What changed in `cove_ir`

`Function` presents the numbering rather than three sizes: `slot_count`,
`scalar_origin`, `value_origin`, `place_origin`, and `region_of`, which answers
a new `Region { Value, Scalar, Place }`. `Region` is a `SlotKind` with the part
a *number* cannot answer taken off — a slot number says a slot is scalar, and
says nothing about whether the word is an `Int` or a `Bool`, because nothing
addressed by a number needs to know. **It is also the frame's reference map**:
`Region::Value` is a word a collector follows and the other two are words it
leaves alone.

The three `*_frame_size` fields keep their names and their values and become
the *widths of the three regions*. Nothing about a frame's shape moved; what
moved is that a slot now has one number, and the region is derived from the
number rather than the number from the region.

`Body::finish` is where the numbers are settled, and that is forced rather than
chosen. Both origins are **high-water marks** — a scope hands its slot numbers
back when it ends, so the widest a region ever got is settled by the last
statement as easily as by the first — so an emitter counts within its own
region, where the rule is local, and the one number every consumer sees is
written once, at the end, by adding the region's origin to the instructions
that carry a slot.

`validate` stopped asking three bounds and asks one question: is this a slot of
this frame, and is it in the region this instruction reads. **That is a check
the three bounds could not make.** Each number was in range of its own stack,
so a `store` reaching a scalar slot could only be caught where the value region
happened to be too narrow for the number, and could not be caught at all where
both were wide enough. `validate_refuses_a_slot_of_the_wrong_region` and
`validate_tells_the_two_regions_of_one_frame_apart` are the two halves of it.

A listing's header prints `frame=<scalar>/<value>[/<place>]` — the widths **in
the order the numbering runs them** — because that line is now what a reader
decodes a listing's slot numbers with. Every golden listing in `lower::tests`
moved with it, and that is the whole of what moved in them.

### What changed in the production `Vm`, and what it cost

The three stacks stay. What decision 1 requires of a split realization is that
every physical offset be *derived* from the one layout, and the derivation is a
subtraction of the region's origin — done **once per frame** rather than once
per access.

The scalar region begins at slot 0, so the scalar core is untouched:
`Inst::LoadScalar` is `scalars[scalar_base + slot]` exactly as it was, which is
the loop `benches/arith` spends its whole run in. For the value region,
`Vm::execute` keeps `values = frame.base.wrapping_sub(function.value_origin())`
as a loop local beside `frame`, recomputed at each of the six places the frame
changes, and `Inst::LoadLocal` is `stack[values + slot]` — one addition, which
is what it cost when a slot's number was its offset. `Frame` is 32 bytes as it
was; nothing was added to it.

**Instruction counts did not move and could not have.** A renumbering changes
the operand of an instruction and never which instruction it is:

| row | instructions, both backends |
| --- | ---: |
| `arith` | 31,142,877 |
| `call` | 37,142,877 |
| `field` | 47,428,595 |
| `method` | 59,428,598 |

which are Phase B's and Phase C's to the instruction, printed by
`the_frame_executes_exactly_the_instructions_the_vm_executes` after it asserts
the two backends agree. VM fuel is its instruction count, so no fuel moved
either, and ADR 0024's four constants are untouched. Allocations are unmoved as
well — 2,000,001 traced objects on `field` and on `method`, none at all on
`arith` and `call`, asserted exactly by
`the_hot_path_performs_no_value_operation`.

The **differential harness is the net here**, because a change to numbering
must not change what a program means: 129 corpus cases, 97 lowered and agreeing
on both backends, 2 refused, which is Phase B's and Phase C's number exactly.

### What the frame backend got out of it, which is a deletion

`FrameVm` had a second number live across the whole dispatch loop —
`values = base + map.values` — recomputed at every frame change, because the
lowering numbered two spaces and a value slot's number had to be read through
the frame map into one region from one base. **It is gone.** A slot's number
*is* its offset from the frame's base now, for `load-scalar` and `load` alike,
and `FrameMap` is what is left over: how wide a frame is, and which run of it a
collection follows, both read off `Function::slot_count` and
`Function::value_origin`.

That is the concrete thing #122's production split needed. There is nothing
left in this backend that translates a slot number.

### What one numbering did *not* buy, which Phase C expected it would

Phase C wrote that the numbering "would buy three named refusals": a function
taking both a value and a scalar parameter, a call passing both, and a value
parameter beside a scalar slot. **It bought none of them, and the reason is
worth writing down because it is a correction to what the previous phase
believed.**

Those refusals are not caused by two numberings. They are caused by the
**calling convention**: arguments are pushed in *declaration* order and become
the callee's first slots without moving, while the numbering groups slots by
region — so a function whose parameters are mixed has its second kind of
parameter pushed at a word whose number names a different slot. Closing that
needs the arguments permuted as a frame opens, or a convention that states each
argument's slot, and neither of those is a renumbering. One numbering was
*necessary* for the widening and is not *sufficient* for it.

Which region goes first is the choice of which of the two mixed shapes is
refused, and not of whether either is. The order chosen is the one the
one-array backend already measured, so the admitted set is exactly what it was:
the four benchmark rows still refused are refused for the same four reasons —
`hostheavy` "a Host call", `arrayget` "a collection", `chars` "a string",
`callback` "a builtin method".

### The peephole, replaced

Phase C's other named debt was `frame::pushed_kinds`, which read the `count`
instructions immediately before an instruction and called them its `count`
operands. That is right only where every operand took exactly one instruction
and that instruction left exactly one word on the value stack.

The shape Phase C named is `Cursor(at: i, step: 1)`, and looking at what the
lowering actually emits sharpens the complaint rather than confirming it. For
`Cursor(at: i, step: here.step)` the two instructions before the `make-struct`
are a `load` and a `get-field-at`, and the read *consumes* the object the load
pushed — so between them they leave **one** operand where the peephole counted
two.

**A misaligned window does not merely fail to name the operands; it names
something else.** The reading there is `[Reference, Int]` where the operands
are `[Int, Int]`, so the `make-struct` was refused for disagreeing with a type
it agrees with. And the same misalignment could as easily have derived kinds
that agreed *wrongly*. One reading already did: `Inst::Dup` was read as
`Kind::Reference` unconditionally, because the one instruction the peephole
could see says nothing about what it copies — so a `dup` over an `Int` fed to a
`store-local` would have been admitted while putting a non-handle into a slot
the frame map calls a reference, which is the invariant decision 1 states for
any physical arrangement, read from the other side. The dispatch loop was never
wrong about it — `Inst::Dup` copies the *bit* — so nothing that ran was
unsound. The check was.

`frame::Operands` is the replacement: one abstract word per value operand, at
every instruction, over every path control can take. A word is a `Kind` every
path agrees on or nothing at all, and the fixed point terminates because a word
only ever moves from a `Kind` to nothing and never back.

Its pop and push counts are `cove_ir::lower::stack_shape`, which the emitter
and `validate` already read. It is **exported rather than copied**, and that is
the point: the whole argument for there being one description of what an
instruction does is that two of them can come apart, so a third reader belongs
on the far side of the one description rather than beside a copy of it. It is
computed once per function in `admits` and once per function when a `FrameVm`
is built, so the one question a *run* puts to it — what a `make-builtin`'s
arguments are made of — is an index rather than a walk.

### What it widened, and the coverage the widening was taken on

**One shape: a struct built out of words that took more than one instruction to
compute.** `a_struct_built_from_a_loaded_word_is_admitted_and_agrees` runs it
against the tree walk and the `Vm` as well as the frame, and
`a_struct_built_from_a_loaded_word_is_rooted_in_its_slot` runs the same program
with the collector at **every** safepoint under `HandleHeap::stress`, asserting
that collections ran and that the sweep reclaimed the cursors the loop
abandons.

`the_widened_shapes_struct_is_a_root_in_its_slot` is the mutation: drop the
frame's own words from the walk and the cursor is swept out from under the loop
that reads it on the next turn. It dies on `crate::slot::HandleHeap`'s own
`names a swept object` rather than on an assertion anybody wrote.

`the_peepholes_window_was_not_this_programs_operands` is what says the widening
is not vacuous, and it says it as arithmetic over `stack_shape` rather than as
a claim about deleted code: the two instructions before the loop's
`make-struct` leave one value operand between them, and the type has two
fields. The initializer above it — two constants, two operands — is the aligned
case, so the same instruction is admitted twice for two different reasons and
the program is a fair test rather than a rigged one.

### The measurement

Six `cove-bench --iterations 15` suites, six processes of one binary, stacks
reserved, quiet machine; **each ratio is the two rows of one run**, and the
figure quoted is the median of the six.

| row | instructions | VM | mixed frame | frame ÷ VM | band over six |
| --- | ---: | ---: | ---: | ---: | ---: |
| `pure` | — | 1.56 ms | 1.39 ms | 0.899× | 4.4 pt |
| `arith` | 31,142,877 | 100.33 ms | 104.63 ms | **1.042×** | 7.7 pt |
| `call` | 37,142,877 | 177.16 ms | 168.78 ms | 0.953× | 4.0 pt |
| `field` | 47,428,595 | 475.54 ms | 202.04 ms | **0.424×** | 1.2 pt |
| `method` | 59,428,598 | 698.84 ms | 372.39 ms | 0.533× | 3.7 pt |

Per instruction, which the exactly equal counts make available:

| row | VM | mixed frame |
| --- | ---: | ---: |
| `arith` | 3.22 ns | **3.36 ns** — still the only row where the frame is slower |
| `call` | 4.77 ns | 4.54 ns |
| `field` | 10.03 ns | 4.26 ns |
| `method` | 11.76 ns | 6.27 ns |

And the per-call prize, each figure two rows of one run and the median of six:

| | VM | mixed frame | saved per call |
| --- | ---: | ---: | ---: |
| frame of scalars — `call` − `arith`, 2,000,000 calls | 38.4 ns | 30.9 ns | **8.0 ns** |
| frame with a reference — `method` − `field`, 4,000,000 calls | 55.8 ns | 42.1 ns | **13.6 ns** |

**`arith` is still the one row where the frame is slower**, at 1.042× against
Phase C's 1.065×, and nothing here could have moved it onto the other side.
`benches/arith` addresses only scalar slots, whose region begins at slot 0, so
its dispatch loop executes the same arithmetic on the same numbers it did
before. Phase B's negative result stands as recorded: a frame with no reference
in it pays the bitmap a bit per word pushed and gets nothing back.

Its band is the widest of the five, 7.7 points, and one suite is the whole of
it — five read 1.038 to 1.049 and the sixth read 1.115. That is the
process-level effect "The reservation is a measurement fix" describes, which is
why the number quoted is a median of six rather than a suite.

#### What moved against Phase C, which is an indication and not a measurement

Every row here is from a different binary from Phase C's, so the comparison
crosses a build and ADR 0029 and
[#179](https://github.com/myuon/cove/issues/179) are why that is not evidence.
Stated as an indication:

| row | Phase C | Phase D | change |
| --- | ---: | ---: | ---: |
| `pure` | 0.894× | 0.899× | +0.6% |
| `arith` | 1.065× | 1.042× | −2.2% |
| `call` | 0.935× | 0.953× | +1.9% |
| `field` | 0.428× | 0.424× | −0.9% |
| `method` | 0.495× | 0.533× | +7.7% |

The two rows that moved most moved in *opposite* directions, and the change
that could have reached either of them is the same change — one addition per
value-slot access on the `Vm` side taken out of the loop and folded into the
base, and one fewer number live across the frame backend's dispatch loop.
Neither row's movement is separable from the rebuild that produced it: `arith`
cannot have been touched at all on the frame side and moved 2.2%, which is the
size of the term a rebuild moves on its own. The per-call figures show the same
shape and the same caveat — 8.0 ns against Phase C's 8.7, and 13.6 ns against
21.3 — and the six suites' own spread on the second of those is 12.5 to 20.0
ns, which is most of the difference being claimed.

**What is measured is the standings inside this binary**, and they are the ones
in the first table.

### Bars

- `LOWERED_FLOOR` **97**; `REGISTERED_REFUSALS` exactly its two entries,
  compared as a whole in both directions. 129 corpus cases, 97 lowered, 2
  refused — Phase B's and Phase C's numbers exactly.
- The four frame rows still refused are refused for the same four reasons, by
  name and with a span.
- **ADR 0030 does not bind**, confirmed rather than assumed: this backend makes
  no Host call at all, and the `hostheavy` refusal is the harness saying so on
  every suite.
- ADR 0024's four constants untouched; `responsiveness.rs` green;
  `the_frame_spends_the_fuel_the_vm_spends` holds, which is the equal
  instruction counts read through the budget.
- `Value` stays 24 bytes — the `const _: () = assert!` in `value.rs` compiles.
- Diagnostics tested for message and span, including
  `a_struct_program_that_raises_agrees_on_message_and_span`.
- `cove check`, `cove test` (165 tests), `cove fmt --check`, and
  `cove api snapshot`, which does not move.

### What Phase D did not do, and what #122 is still waiting on

Two of the three things Phase C listed are built. What is left of that list and
what this added:

- **A `set-field` that names its type.** Untouched. `Inst::SetField` still
  carries a `Const::Name` and `frame::admits` still keeps a by-name table for
  it, which is the last one in this backend.
- **The argument permutation, or a convention that states each argument's
  slot.** This is the item the numbering was expected to close and did not, and
  it is what the two mixed-parameter refusals are actually waiting on. It is
  the first thing #122's production split will have to decide, because a
  one-stack production `Vm` has the same question to answer about every mixed
  call in the corpus rather than about the four rows a prototype admits.
- **A physically single frame in the production `Vm`.** Still three arrays and
  three bases, now derived from one layout. What #122 needs from `cove_ir` is
  built; what it needs from `cove_runtime` is the permutation above.
- **Everything else on Phase B's list**, unchanged and not restated: an
  aggregate at decision 5's boundary, the inbound half ADR 0033 clause 1 keeps
  closed, every one of clause 7's identity obligations, arrays and strings and
  enums, a field write that does not copy, places and `var` and closures and
  `dyn` and tasks and Host calls, and the bitmap's alternatives.

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
what the increment costs. That the largest cost on a call path was not the
frame, the arguments or the representation but the mutex the run's `Budget`
lived behind — 40.3% of `benches/call`, of which one acquisition in three was
removed for 16.6% and the other two for 35.1% more. And that a boundary
instruction is cheap: two crossings are 16.0 ns a turn and allocate nothing at
all, which is the answer to a question this document had been asking in the
other direction.

One more since, from issue #99 rather than #123, and it is a question closed
rather than a change taken. What a struct receiver costs a loop: it doubled the
loop it was in when a receiver was a `Box` copied per call, and it adds 27% a
call now that `Value::Struct` is an `Rc`, at a per-instruction cost within 11%
of the same loop with no call in it. "What a character costs, and what a
receiver costs on top of it" is the measurement; the residue is the per-call
constant the matrix already priced, which
[#182](https://github.com/myuon/cove/issues/182) has since removed the largest
part of, and is not about receivers.

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
stacks being numbered separately and from a place being a slot number rather
than a reference, and no measurement can move that either. ADR 0027 gave a
place a second stack to be rooted in and did not change that sentence: a place
rooted at a scalar slot reaches an `i64`, so it reaches nothing, and one rooted
at a value slot reaches what the value window already yields — which is also
why it must not be walked a second time.

**Open, and what would settle each.** Everything below was #109's or #116's
until both closed. Each is now a narrower issue carrying the measurement that
would decide it, which is the whole of what the two umbrellas had left.

The inline representation of `Option`, `Result`, and small enum payloads —
[#183](https://github.com/myuon/cove/issues/183) — settled by building one and
measuring `arrayget` and `chars`, and now `benches/convention`'s `conv_host`
too, which pays for the `Result` a Host operation answers with two million
times. That one now has a size as well as a benchmark: `chars` runs at a sixth
of `arith`'s fuel rate, and the two things it does that `arith` does not are
the `Option` per index and the `Rc<str>` per character. The argument vector
allocated per builtin call — [#184](https://github.com/myuon/cove/issues/184) —
the same benchmarks, the same way. The closure value at four allocations, which
the matrix added to that list — [#185](https://github.com/myuon/cove/issues/185),
and the honest caveat there is that only `conv_fresh` pays it.

**Those three were built and measured; "Both were taken, and this is what they
were worth" above is the result, and the paragraph they are named in is left
as it was written.** What is left of each is written there too: the Host
boundary still owns an argument vector, on purpose; a `Closure` is still a
vector of name-and-value pairs, because dropping the names saves nothing more
than interning them did; and a callback-per-element benchmark still does not
exist. The reading half
of a representation-independent embedding API —
[#186](https://github.com/myuon/cove/issues/186), **now built**, which is why
each of those three *was* a source break for embedders and the next one need
not be. A host reads a value through `Value::field`, `Value::case`,
`Value::payload`, `Value::items` and the rest, and names no variant in either
direction.

Worth recording precisely, because the honest version is narrower than the
headline. Each of the three stayed *readable* by accident or by shim rather
than by design: `Box<StructValue>` → `Rc<StructValue>` both deref to
`StructValue`, so #104's only churn in this repository was a `Box::new` at a
build site; and #183's `Payload` carries a hand-written `Deref<Target =
[Value]>` and `From<Vec<Value>>` for exactly this reason, which is why it
touched no test and no example. The readers are what makes the next one not
need a shim — and they draw their own line, since `Value::payload` answering a
`&[Value]` is a promise that a payload stays contiguous. The variants were still exposed when that was
written — nothing stopped a host matching on one, and sealing them is a larger
promise than adding readers was, which is the half of #186 that stayed open.
[#196](https://github.com/myuon/cove/issues/196) asked the question and
ADR 0028 decision 6 answered it: `Value` is now an abstract type, all
twenty-two variants sealed, and the exhaustive match a host loses comes back
as `ValueView`, which changes when the *language* gains a kind of value and
not when the runtime moves one. The heap layout the VM
owns — **not filed**, because it is settled by what is still allocating once
the three above are gone, and that is exactly the evidence it does not have
yet; this paragraph is its record. A moving collector — also not filed, not yet
asked for by anything measured here, and what it would owe is written down
rather than assumed, under "Collection is non-moving" above.

Everything in that first group is downstream of the same allocation sites. That
makes them the next measurement whether or not a VM-owned heap is ever built.

Beside them, and separately, three cliffs the calling-convention matrix
measured. Two were [#162](https://github.com/myuon/cove/issues/162)'s — a
`var`-rooted local that could not live on the scalar stack and a closure's
captures that could not either — and both are **answered**: a place names a
slot rather than a stack and a capture takes the slot its own kind names, which
is [ADR 0027](adr/0027-a-place-and-a-capture-name-a-slot.md) and which "One
slot identity, and what the two cliffs it owned were worth" above measures. The
first is gone entirely and the second is roughly halved, with the remainder
belonging to the general `call-value` convention rather than to a capture. The
third — the Host boundary at twenty-nine times a loop turn (#184 and #183) — is
not, and "The cliffs" above says what it would take.

What was still open of #162 when this was written is its *title*: there are
three stacks, three bases per frame and three counts on a call, and a single
physical frame is neither built nor refused. **It has since been built and
measured** — "One physical frame, measured" above is the result, and the short
version is that one frame is worth 24 ns a call, 0.68x on `benches/call` and
0.62x on `benches/pure`, and 0.93x on the one row that calls nothing. The
paragraph is left as it was written because what it says next is the part that
survived.

**And #162's Design B has since been built and measured too** — "The mixed
frame, measured" above — which is the half of the title that was about a *GC
bitmap* rather than about one stack. The short version is two-sided: a loop
whose frame holds a reference runs at 0.49x the VM and a call over such a frame
saves 18.1 ns against the VM's 55.8, so **the per-call prize survives rooting
and is more than twice what a scalar call's is**; and a loop whose frame holds
no reference at all runs at 1.11x, because the bitmap is a bit written per word
pushed and that row pushes most and has no reference to say anything about.
Which of those two a program is depends on the program, and this document does
not have a corpus that says which is typical.

**Two phases have landed since, and each answered a prediction wrongly enough
to be worth recording.** "A struct's reference map is a property of the type"
above gave `cove_ir` a per-field slot kind, so the bit a field read pushes now
comes from the lowered type rather than from how one instance happened to be
built — and the by-name refusal that guarded the old arrangement is gone by
being *unstateable* rather than by being diagnosed differently: two sites
cannot disagree about a fact neither states. But the prediction that the same
absence caused the frame map to be derived at run time was **wrong**. It was
one absence and two symptoms: a struct's map genuinely was missing from the IR;
a frame's is `value_frame_size` and `scalar_frame_size` and three additions
over them.

"One slot numbering, and a simulation in place of a peephole" then gave
`cove_ir` the one numbering ADR 0028's decision 1 decides, and found that it
bought **none** of the three refusals the previous phase had predicted it
would. Those belong to the calling convention, not to the numbering: arguments
are pushed in declaration order and become slots without moving, while the
numbering groups by region. Necessary, not sufficient — and that is the first
thing a production split has to decide, because a one-stack `Vm` meets it at
every mixed call in the corpus rather than at four benchmark rows.

That phase also found the operand check was not merely conservative but
**misaligned**: its window derived `[Reference, Int]` for operands that are
`[Int, Int]`, and it read `Inst::Dup` as a reference unconditionally, which is
a wrong *acceptance*. The dispatch loop was never wrong — it copies the bit.
The check was. An abstract simulation over `stack_shape` replaced it, and
`stack_shape` is exported rather than copied so there is one description with
three readers instead of two descriptions.

`arith` has stayed on the wrong side of 1x through all four phases, and each of
the last two said plainly that nothing it did could have moved it. That is the
honest shape of this result: **one frame is worth a great deal at a call and
something to pay for in a loop that never makes one.** ADR 0027's "What is not decided here" is the list. What the
two cliffs turned out to say about it is worth recording, because it was not
what anybody expected: **neither of them needed one physical stack.** Both were
a slot's *role* deciding its representation, and both were fixed by taking that
decision away from the role and leaving it with the checker.

The fourth of them — the mutex on every call and every return
([#182](https://github.com/myuon/cove/issues/182)) — is the one that is done,
and it is worth saying why it went first. It was the largest of the four and
the only one that was not a change to what anything *is*: a `Meter` charges the
same accounting on the same schedule and no slot, capture or payload moved. It
also had to go first for #162 to be measurable. A frame layout compared on
`benches/call` or `benches/pure` against a baseline that still took the lock
would have been credited with removing it; with the lock gone, what those two
benchmarks now measure of a calling convention is the calling convention.

One measurement constraint applies to all of them and did not exist when most
of this document was written.
[#179](https://github.com/myuon/cove/issues/179): the workspace has no
`[profile.release]`, so rustc partitions codegen units by module and where code
lives is a performance variable independent of what it does. "A change to
`vm.rs` moved a benchmark that cannot execute it" is the cleanest instance.
What survives it is a dynamic instruction count, an allocation count, and a
before-and-after through one binary in one sitting.
