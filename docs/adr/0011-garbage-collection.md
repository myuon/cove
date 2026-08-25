# ADR 0011: Garbage collection

- Status: Accepted
- Date: 2026-08-25

## Context

The Language Card says: "Memory is managed by a precise, non-moving
mark-and-sweep collector." ADR 0001 says more — stack maps so the collector
does not treat integers as pointers, no finalizers, no compaction, no
generations, no concurrency, and heap fragmentation, pause time, allocation,
live heap size, and GC work all visible in traces.

None of it exists. Values are `Rc`, and a cycle leaks with no diagnostic. The
card states a mechanism the implementation does not have, which is the largest
remaining gap between the two.

Two things have changed since ADR 0001 wrote that sentence. Tasks now run on
threads, so "the heap" is no longer one thing an interpreter walks at leisure.
And `Transfer` already defines precisely which values cross a thread boundary
and what the receiving thread owns.

## Decision

Add a precise, non-moving mark-and-sweep collector, **per task**, over the
values a task owns.

### Why per task, not one shared heap

A shared heap needs every thread to reach a safepoint before a collection can
mark, which means stopping threads that are running Cove code — and the
runtime already has safepoints, so this is possible but it makes every task's
pause depend on every other task's behaviour.

Cove does not need it. The card's task-safety rule already says a value either
belongs to one task or is immutable and shared, and `Transfer` is that rule
made concrete: a value crossing a boundary is copied, so two tasks never own
the same mutable object. A per-task heap is therefore not an approximation of a
shared one — it is what the language's own rule already describes.

`Shared<T>` is the exception, as it is everywhere else. Its contents are
reachable from more than one task, so they are owned by the `Shared` cell
rather than by any task's heap, and they are collected when the cell is.

Concretely, a heap belongs to an `Interpreter`, and ADR 0008 gives each task
a thread with an interpreter of its own — so a heap is reached only from the
thread that owns it, and a collection needs no lock and no rendezvous.
Because a value crosses by copying, a task runs on its own fresh allocation
and the original stays behind, where the sending task's heap reclaims it like
anything else it stopped naming.

### What "precise" costs here

The collector must know which words are pointers. In a tree-walking
interpreter the roots are the interpreter's own structures — the environment
chain and the places being written — not a machine stack, so ADR 0001's stack
maps are not needed yet. They become necessary when a native backend exists,
and that is the right time to write them.

This is a real narrowing of ADR 0001 and the ADR should be read as scoped to
the interpreter until a backend exists.

### What it collects that `Rc` does not

Cycles. That is the whole point, and the test for this work is a program that
builds one and a heap that shrinks after it.

### Visibility

Allocation, live heap size, collection count, and pause time become trace
events, and `cove run --stats` reports them. ADR 0001 asks for exactly this,
and the trace's own "not carried by these events" block currently names
allocation and memory pressure as things it cannot report. That block should
get shorter.

Allocation is reported as a count on each collection and as a run total,
rather than as one event per allocated object. An event per allocation would
be most of a trace and would say less about pressure than the pair of numbers
that bracket it: what was allocated, and what survived.

### Budgets

`Limits` gains a memory budget, which ADR 0001 lists and ADR 0003 could not
implement because nothing accounted for allocation. Exceeding it stops the run
the way fuel does.

It bounds the run and not one task, as fuel and host calls do under ADR 0008:
each task reports what its own heap measured and the budget compares their
sum, so a run cannot stay under the limit by spreading the same memory over
more tasks.

### Scope

No finalizers, no compaction, no generations, no concurrent or incremental
collection, no weak references. Each is listed in ADR 0001 as out of scope and
each remains so.

## Consequences

`Rc` does not disappear from the runtime, and it does not stop being the
reclamation strategy either. It reclaims every acyclic value exactly as it
did, which is nearly every value a program makes; the collector exists for
the one thing it cannot do. The change is invisible to Cove programs except
that a cycle no longer leaks and memory becomes observable and limitable.

The heap cannot hold a strong reference to what it tracks, because `freeze()`
consumes *uniquely owned* vector storage and asks `Rc::strong_count` whether
the caller holds the only handle. A heap holding a strong reference would make
`freeze()` fail on every vector, which is observable Cove semantics. It holds
a `Weak` instead, which costs nothing: a cycle keeps itself alive, so a `Weak`
to a member of one always upgrades.

Startup gains a heap, and small programs do not measurably pay for it. A run
starts with an empty table and two counters, and a program that allocates no
collectable object is never collected: `cove run hello` reports zero
allocations, zero collections, and no measurable difference in wall time. The
cost is per-allocation and per-safepoint, which is a reason to measure a
collector in traces rather than assert it in a card — but not the reason this
ADR gave.

## What this leaves uncollected

A `Shared` cell's contents are a `Transfer`, and a `Transfer` cannot carry a
`Vector`, so a cell holds nothing collectable and nothing that can close a
cycle *within one*. The `Arc` frees the contents with the cell, which is what
"collected when the cell is" means in practice, and a collection never takes
a cell's lock — it could not, since `lock` holds the mutex for the whole of a
closure that reaches safepoints.

A cell may still hold *another cell*, including itself:

```cove
struct Node { cell: Option<Shared<Node>> }
let n = Shared(Node(cell: None))
n.lock(fn(var value) { value = Node(cell: Some(n)) })
```

That is an `Arc` cycle among cells, and nothing reclaims it. Cells are
reachable from every task that was given one and outlive all of them, so
collecting cycles among them needs a collector that stops every thread —
which this ADR rules out under "no concurrent collection". It is a real leak,
narrower than the one this ADR closes, and closing it is a decision for
whenever `Shared` grows enough use to make it worth stopping the world for.
