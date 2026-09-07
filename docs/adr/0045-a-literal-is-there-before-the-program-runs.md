# ADR 0045: A literal is there before the program runs

- Status: Accepted
- Date: 2026-09-07
- Decides: where a string literal's object lives, when it is built, and how
  the collector knows not to take it — for
  [issue #281](https://github.com/myuon/cove/issues/281)
- Supersedes nothing. In particular it does **not** supersede
  [ADR 0034](0034-one-physical-word-stack.md)'s rule that there are two
  regions and no third; the argument that it need not is most of this record

## Context

`Inst::Str` does not load a literal. It says so itself:

> The object is allocated on first use and shared afterwards: a string
> literal in a loop allocates once for the run, not once per turn.

So executing it means a branch — has this one been built? — and, the first
time, a heap allocation and a UTF-8 copy. `Machine::interned` is a
`Vec<u64>` with one slot per `StrId`, filled lazily, and it is walked as a
GC root for as long as the machine lives.

Three things follow, and only the first is the one the issue named.

**A literal costs a check on every use.** Small, and on the path a loop
takes.

**A literal is allocated once per task, not once per run.** `interned` lives
on the `Machine`, and there is one `Machine` per task. The code says this is
deliberate and says why:

> The interned strings are the one that looks like an economy and is not. A
> literal is an *object*, and an object belongs to the heap both tasks
> address; interning it twice costs one object per literal per task and buys
> a table with no lock on the path a literal in a loop takes. Sharing it
> would put a lock between every `Inst::Str` and its answer.

That reasoning is exactly right about *lazily* built objects, and it is the
cost this ADR removes rather than the argument it disagrees with.

**A literal is retained because the table roots it**, which the same doc
notes: "a string mentioned once and never reached again is retained".

## The decision

**Every string literal is built into the heap before the program runs, at
the base of the heap region, and is never collected.**

Concretely:

1. When a `Machine` is constructed — the same moment the program is encoded
   and verified — each entry of `Program::strings` is allocated in `StrId`
   order, before any instruction executes and therefore before any other
   object exists.
2. The allocator records where that finished: one word, `statics`, the first
   address after the last of them.
3. `Inst::Str` becomes a load of a precomputed address. No branch, no
   allocation, no copy.
4. The collector does not sweep an object below `statics`. One comparison,
   in the same shape as the region decoder it sits beside.

### It is not a third region, and that is the load-bearing claim

ADR 0034 says there are two regions and that a third needs an ADR that
supersedes it and shows the one memory cannot do the job. This ADR does not,
because the one memory can.

A literal's object is an ordinary heap object: the same one-word header of
`(LayoutId, len)`, the same `Shape::Str`, the same payload of UTF-8 bytes
eight to a word. It is at an ordinary heap address, above `STACK_WORDS`,
allocated by the ordinary bump. **Nothing about it is a different kind of
thing in a different kind of place**; what is different is only *when* it was
allocated and that nothing reclaims it.

"Static" here names a lifetime, not a region. The address space still has a
stack below `STACK_WORDS` and a heap above it, `is_stack` is still the whole
of the region decoder, and a reference to a literal is a `Repr::Ref` holding
a heap address that no code can tell from any other.

The issue's own sketch draws a third region — `static | heap → | ← stack` —
and this ADR declines that drawing. It would invert the existing layout, need
a second range check everywhere one is done today, and buy nothing the base
of the heap does not already give: the heap grows upward from a fixed low
boundary, so its base is exactly the immovable place a static wants to be.

### How the collector keeps them, without a table

`Space::collect` decides an address is an object it owns with one range test:

```rust
fn reachable(addr: u64, bump: u64) -> bool {
    !is_stack(addr) && addr < bump
}
```

A literal is below `bump`, so tracing *through* a reference to one already
works, unchanged. What must change is that a sweep may not reclaim one, and
that is `addr >= statics` — one comparison against one word the allocator
already has room for.

**No table, and specifically not the one that exists today.** `interned` is
a root list, and a root list is exactly the "GC side table" issue #281 rules
out and ADR 0041 rules out. It goes. The range check replaces it, and the
replacement is cheaper in the only place that matters: a collection no longer
walks a list of every literal in the program to mark objects that a
comparison could have kept.

### Addressing them

The encoded instruction keeps a dense `StrId`, and the machine holds
`statics: Vec<u64>` — the address of each, in `StrId` order, filled at
construction.

That is a table, and it is the kind ADR 0041 permits — "an index into
immutable program metadata" — rather than the kind it forbids. It is not
consulted by the collector, holds no GC state, and cannot be written after
construction. The alternative, baking an address into the encoded
instruction, is rejected: it would make the encoded form depend on the memory
layout, so a program could not be encoded without knowing where it would run.

### Tasks share them

Because they are built before any task exists and never change, every task of
a run addresses the same objects. No lock, because there is nothing to
synchronise: an immutable object at a fixed address needs no more agreement
than the `Program` itself does.

This is the part worth more than the branch. The current design pays one
object per literal *per task*, and the comment defending it is right that
sharing *lazily built* objects would need a lock. Building them eagerly is
what removes the lock from the question.

## What it costs

**Every literal is built, including the ones a run never reaches.** Today an
unused literal costs nothing; here it costs its bytes and the moment it takes
to write them. That is a real regression in one direction, and the ADR does
not pretend otherwise.

It is accepted on the grounds that a program's literals are a bounded,
usually small share of its source, and that the cost is paid once per run
rather than once per task. **It is not accepted on the grounds that it is
obviously small** — issue #281 asks for startup and artifact-size to be
measured, and this ADR holds the implementation to that: if a corpus program
measurably starts slower, that number is the thing to argue with, not this
paragraph.

## Why strings and not numbers

ADR 0041 rejected a constant pool once already, for `Int` and `Float`:

> #244's immediates exist precisely so a loop's literal is not fetched from
> anywhere, and this would put it back.

That reasoning does not reach here, and the difference is not a matter of
degree. An `Int` literal is an immediate: it is *in* the instruction, and a
pool would add an indirection that does not exist. A string literal is
already an object at an address, already reached by an indirection, and
already fetched from somewhere. This ADR removes a branch and an allocation
from that path; it adds nothing to it.

## What this does not decide

- **Serialized artifacts.** Issue #281 asks that an artifact carry enough to
  rebuild its pool. Nothing serializes a `Program` today — `cove build`
  embeds source and lowers again — so there is no artifact to make
  self-contained, and inventing one to satisfy the criterion would be
  building the thing rather than the reason for it. When an artifact exists,
  its constants travel with its function table, for the reason function ids
  do: both are local to one linked program.
- **Interning at run time.** A string a program computes is an ordinary heap
  object with an ordinary lifetime, and stays one.
- **Any other constant.** Layout tables and instruction metadata are not Cove
  values and are already outside this question. `Inst::Trap` and
  `Inst::ScopeEnter` carry a `StrId` and never allocate — they read the
  program's text directly — so they are untouched.
- **Unifying the regions.** ADR 0034 reserves the freedom to make the stack
  and heap one allocation later. A literal at the base of the heap is
  compatible with that and does not advance it.

## Consequences

**`Inst::Str` becomes what it claims to be.** Its doc comment currently
explains an allocation strategy; it will name an address.

**A collection gets cheaper by exactly the literals.** The root walk loses
`interned`, and the sweep gains one comparison.

**The listing keeps the text.** `str s1:ref "hello"` is what 34 golden
listings across ten files assert and what the playground's highlighter
matches. The instruction still prints its text — a reader wants the string,
not its index — so the printed form need not move, and this ADR asks that it
not move without a reason of its own.

**The verifier keeps its bound.** `StrId` is already checked against
`Program::strings` in both verifiers, the same way a `FunctionId` is checked
against the function table. That check is unchanged and is what makes an
absent constant a verification failure rather than a wild address.
