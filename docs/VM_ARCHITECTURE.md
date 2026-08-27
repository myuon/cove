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

Two contiguous vectors for the whole run, shared by every frame: a `Vec<Value>`
and, beside it, a `Vec<i64>` for the slots and operands the checker proved are
`Int` or `Bool`. A frame is a window into both:

```rust
struct Frame {
    function: FunctionId,
    return_pc: usize,    // the instruction after the caller's `Call`
    base: usize,         // where this frame's value slots begin
    scalar_base: usize,  // where its scalar slots begin
}
```

A frame's slots are `stack[base .. base + value_frame_size]` and
`scalars[scalar_base .. scalar_base + scalar_frame_size]`, and its operands sit
above them on each. The two are numbered separately, so which stack a slot
lives in is decided by which instruction addresses it rather than by anything
read at run time. Nothing is allocated per call: the frame is four words pushed
onto a `Vec<Frame>`, and the slots are stack that already exists.

### Argument placement

Arguments are pushed onto the *caller's* operand stacks, left to right, and
become the callee's first slots without moving. So `base` is the caller's
value-operand top, read from the other side, and `scalar_base` is its
scalar-operand top read the same way.

Which of the two stacks an argument travels on is the callee's declared type,
resolved by the checker and published as `Function::params`: a parameter the
checker settled as `Int` or `Bool` is a scalar slot, so its argument is pushed
onto the scalar stack and becomes that slot, and everything else is a value
slot as before. `Call` carries the two counts rather than looking them up,
because the lowering has to place a recursive call's arguments before the
callee it is inside exists, and because the depth simulation is a function of
one instruction with no function table beside it. `validate` reconciles the
counts with the callee's own `params`, which is what makes this an invariant
rather than an agreement.

Slot order is fixed at lowering and is dense within each stack: `self` when the
function has a receiver, then each declared parameter in declaration order,
then locals and temporaries in the order the body declares them, each drawing
a number from the stack it lives in. `value_frame_size` and
`scalar_frame_size` are the high-water marks of that, not the totals, because a
block's slots are released at its end and a later sibling block reuses the
numbers.

### Return

`Function::returns` says which stack a call leaves its answer on, and it is
the same question asked of the declared return type: a function the checker
settled as answering `Int` or `Bool` answers on the scalar stack, and every
one of its returns is a `ReturnScalar`. Mixing the two in one function is a
`validate` failure, because a caller reads exactly the stack the convention
named and nothing tells it which of two a given return happened to use.

Either return pops its answer, truncates *both* stacks to the frame's bases,
pops the frame, and pushes the answer onto whichever stack it came off — which
is now the caller's. Truncating to the bases is what discards the callee's
slots and its arguments together, since they are the same storage.

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
turn; a loop whose turn charges C fuel stops within ceil(64 / C) turns. Two
facts narrow what that gives up, and both belong here. The run's own
cancellation was never on the eager schedule to begin with, because it is read
inside `Budget::safepoint`, which the gathered schedule already gated; and
`self.stops` is pushed only by `Reentry::call_until`, which this backend answers
without running any Cove code, so no VM run today can have a flag in that list
while a loop turns. It is a bound given up for speed all the same, and it will
start to matter when closures lower.

### Host calls and reentry

A Host call goes through the same `HostRegistry` the interpreter uses, so the
grant check, the budget charge, the trace event, and the wait accounting are
the same code and cannot drift. Reentry — a host running a Cove closure — is
not yet lowered, and the convention for it is unwritten.

### Trace

`EntryEnter` and `EntryExit` are recorded with the CPU/wait split. Instructions
are not traced: an instruction-level trace would be a different artifact and
ADR 0019 does not propose one.

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
| `pure`      |   15.7 ms |   2.6 ms | **5.93×** |
| `call`      | 1534.1 ms | 260.3 ms | 5.89× |
| `arith`     |  424.7 ms |  82.4 ms | 5.15× |
| `method`    | 2860.0 ms | 861.7 ms | 3.32× |
| `chars`     | 1841.8 ms | 810.0 ms | 2.27× |
| `arrayget`  | 1411.8 ms | 658.2 ms | 2.14× |
| `field`     |  857.6 ms | 436.8 ms | 1.96× |
| `hostheavy` |    5.0 ms |   3.7 ms | 1.36× |

Three of those ratios fell while both columns got faster, which is worth
saying because a ratio alone would read as a regression. `Value` at 24 bytes
is shared by the two backends, so the AST column moved with the VM column and
`arith`'s ratio went from 5.49 to 5.15 on a VM run that got *faster*. The
ratio is what the two backends cost each other; the milliseconds are what
Cove costs.

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
