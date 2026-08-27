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
representation, and representation is what #116 proposes to change.

## The calling convention

This section describes what is built today. Where the target changes it, the
change is named.

### The stack

One contiguous `Vec<Value>` for the whole run, shared by every frame. A frame
is three numbers:

```rust
struct Frame {
    function: FunctionId,
    return_pc: usize,   // the instruction after the caller's `Call`
    base: usize,        // where this frame's slots begin
}
```

A frame's slots are `stack[base .. base + frame_size]` and its operands sit
above them. Nothing is allocated per call: the frame is three words pushed onto
a `Vec<Frame>`, and the slots are stack that already exists.

### Argument placement

Arguments are pushed onto the *caller's* operand stack, left to right, and
become the callee's first slots without moving. So `base` is the caller's
operand top, read from the other side.

Slot order is fixed at lowering: `self` when the function has a receiver, then
each declared parameter in declaration order, then locals and temporaries in
the order the body declares them. `frame_size` is the high-water mark of that,
not the total, because a block's slots are released at its end and a later
sibling block reuses the numbers.

### Return

`Return` pops the value, truncates the stack to `base`, pops the frame, and
pushes the value onto what is now the caller's operand stack. Truncating to
`base` is what discards the callee's slots and its arguments together, since
they are the same storage.

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

Fuel is charged per instruction, and proportionally where an operation's cost
is not constant. A safepoint spends the accumulated fuel against the run's
shared budget and checks the deadline.

Safepoints are at: entering the entry, every call, every return, every back
edge, and every 1024 instructions of straight-line code.

A back edge reads this thread's stop flags every time — cancelling a task stops
its loop at the next turn, exactly as on the interpreter — but spends fuel only
once 64 has gathered, because spending takes a lock the tasks share and a tight
loop takes a back edge every turn.

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

**Not yet chosen, and not to be chosen without measurement:** whether the
physical slot is one uniform compact word or separate typed storage; whether
the dynamic representation is a tag beside a payload, an aligned tagged
pointer, or NaN boxing. NaN boxing in particular has to be weighed against
Cove's `Int` being a full 64 bits, which it does not fit alongside a tag.

### Frame metadata

The frame layout is where precise GC roots come from. A function's metadata
should carry, per slot, whether it holds a reference — so a collection scans
the slots that can hold one rather than inspecting every scalar as a `Value`.

That metadata is also what a future JIT would need, which is a reason to derive
it from the checker's facts rather than invent it twice.

### A VM-owned heap

Today a heap value is a Rust object graph reached through `Rc`. The target is a
layout the VM owns, reached through a stable handle. That is the largest of
these changes and the one most entangled with the embedding API, so the
embedding API should stop exposing the representation before it moves.

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
