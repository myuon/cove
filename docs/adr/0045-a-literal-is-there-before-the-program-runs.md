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

1. The **entry's** machine — `Machine::for_run`, once per run — allocates
   each entry of `Program::strings` in `StrId` order, before any instruction
   executes and therefore before any other object exists.
2. Two things record it, and they are deliberately two:

   ```text
   Space.static_end: u64                 // the immortal floor
   Machine.literal_addrs: Arc<[u64]>     // StrId -> address
   ```

   `Space` is one per run and shared by every task; `static_end` therefore
   belongs to it, because what it bounds is the heap the run shares.
   `literal_addrs` is immutable program metadata built once and handed on.
3. `Machine::for_task` **receives** `literal_addrs`. A spawned task does not
   allocate a literal, does not copy the table, and does not place an object
   anywhere: it clones an `Arc` and addresses what the run already built.
4. `Inst::Str` becomes a load of a precomputed address. No branch, no
   allocation, no copy.
5. A sweep, and the free-list rebuild that follows it, **begin at
   `static_end`** rather than at the base of the heap.

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
works, unchanged. What must change is that a sweep may not reclaim one.

That is stated as a **floor rather than a test**: a sweep and the free-list
rebuild after it start at `static_end` and walk upward. Nothing below it is
visited, so nothing below it can be freed, coalesced, or relabelled — and the
invariant is a property of where the walk begins rather than a comparison
every object has to pass and a reader has to trust is never skipped.

**No table, and specifically not the one that exists today.** `interned` is
a root list, and a root list is exactly the "GC side table" issue #281 rules
out and ADR 0041 rules out. It goes. The range check replaces it, and the
replacement is cheaper in the only place that matters: a collection no longer
walks a list of every literal in the program to mark objects that a
comparison could have kept.

### Addressing them

The encoded instruction keeps a dense `StrId`, and the machine holds
`literal_addrs: Arc<[u64]>` — the address of each, in `StrId` order, built
once by `Machine::for_run` and shared with every task the run spawns.

That is a table, and it is the kind ADR 0041 permits — "an index into
immutable program metadata" — rather than the kind it forbids. It is not
consulted by the collector, holds no GC state, and cannot be written after
construction. The alternative, baking an address into the encoded
instruction, is rejected: it would make the encoded form depend on the memory
layout, so a program could not be encoded without knowing where it would run.

### Tasks share them

Because they are built by the entry's machine before it executes an
instruction — and therefore before any `spawn` can have happened — and never
change afterwards, every task of a run addresses the same objects. No lock, because there is nothing to
synchronise: an immutable object at a fixed address needs no more agreement
than the `Program` itself does.

This is the part worth more than the branch. The current design pays one
object per literal *per task*, and the comment defending it is right that
sharing *lazily built* objects would need a lock. Building them eagerly is
what removes the lock from the question.

## Building them can fail, and that is a semantic change

`Vm::new` and `Machine::for_run` cannot fail today. Placing every literal
before the program runs introduces an allocation that can, and the
consequence is not only a startup cost: **a run can now fail before it
begins, because of a literal it would never have reached.** Under lazy
allocation an enormous unused string cost nothing and could not stop
anything.

That is accepted as this design's price rather than hidden. A program whose
literals do not fit in its heap is a program that cannot be relied on to run,
and finding that out at the start is better than finding it out at whichever
loop iteration first reaches the string. But it is observable, and it is not
what happens today, so it is decided here rather than discovered later.

The contract:

- **The constructor stays infallible.** The failure is held on the machine,
  exactly as the encoded and verified program already is — preparation that
  cannot fail early keeps its answer until someone asks.
- **It is returned before a frame exists.** `run_entry` and `invoke` answer it
  as their first act, before pushing anything. So a failed run has no stack to
  unwind and no partial state to describe, and a host sees the same shape of
  `Err` it would see from a run that failed on its first instruction.
- **The message is the one a full heap already gives** — `"this run has no
  memory left"` — because that is what happened, and inventing a second
  wording for the same exhaustion would make two errors out of one condition.
- **It carries the entry's span**, the one a host asked to run. A literal has
  no source position of its own worth naming here: the program is what could
  not be started, not the string.
- **It charges no fuel.** Fuel meters what a program executed, and this
  program executed nothing. Charging for it would make a budget's meaning
  depend on how many literals a source file happens to contain.

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
- **A general `load.const`.** A string is the only heap-shaped constant the
  language has, so `Inst::Str` changes meaning — from *build one* to *load the
  address of the one that is there* — and keeps its name and its printed
  form. Generalising an instruction to a second kind of constant that does
  not exist would be building the shape rather than the thing. When one
  appears, that is when the two have something to share.
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
