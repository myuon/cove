# ADR 0120: The VM runs only what verifies, and is safe on everything that does

- Status: Accepted
- Date: 2026-10-25
- Decides: the machine's contract over arbitrary IR — every `Program` is
  either refused before its first instruction or runs to `Ok` or
  `RuntimeError`, with no panic, abort, hang past its deadline or undefined
  behaviour; what `cove_ir::verify` adds to make that true (a layouts pass and
  a places pass); what the machine checks at run time because the verifier
  cannot; and two VM-level fuzz targets, `ir` and `ir_mutate`, that hold both
  to it
- Supersedes: [ADR 0117](0117-verify-refuses-a-program-whose-ids-are-out-of-range.md)'s
  decision that a decoded image which verifies is not run

## Context

[ADR 0117](0117-verify-refuses-a-program-whose-ids-are-out-of-range.md) made
`verify` total: it answers `Err` instead of panicking on any program
`serial::decode` returns. It stopped there on purpose. `verify` checked that a
program was well formed *the way the lowering makes them*, and nobody claimed
the machine was safe on every program it accepted, so a decoded program that
verified was still not run.

That left the machine's safety resting on the lowering never writing certain
programs, with nothing checking it. Every fuzz target before this one entered
through source. They can only reach programs the lowering writes, so a gap
between "verifies" and "runs safely" was invisible to all of them. Reading the
verifier against the machine found the gap at once, and it was wide:

- **a field or element access trusted its own layout.** A `Repr::Ref` slot
  carries no layout, so a `load.field` whose layout put a reference where the
  object held an `Int` read a number as an address. The collector then traced
  it, and a header nobody wrote was read;
- **an address was one unchecked word.** `addr.of-slot` could take the address
  of an `Int` slot and a `store` could put a reference through it. Or
  `addr.of-part` could add a million and write past every frame. An address
  could also be returned, captured or stored into the heap, outliving its
  frame;
- **a layout's `words` and its `shape` could disagree.** A field placed past
  its struct's words, a `Word(Int)` whose word was a `Ref`, layout 0 not the
  free layout. Everything downstream reads whichever of the two it needs;
- **a dynamic view is three frame words, two of them `Int`s.** A program could
  forge one: a `String`'s layout over a `Vector`, an offset past the object.
  The native tier's reflection fast paths then read straight through it;
- **a closure call's callee is a word of an object.** Its arguments were copied
  at the *callee's* parameter widths without being compared with the call's,
  so an `Int` could land in a `Ref` or an `Addr` parameter;
- and the smaller ones the VM-level targets found once they ran: a `Shared`
  lock or unlock on an object that is not a cell, a null object followed
  without a check, an object kept only by a frame address collected under it,
  `dyn.on-path` following an entry out of the stack, a task awaited at another
  width than it answers, and `verify` itself running for minutes over a place
  offset round a loop.

None of these is reachable from source today, because the lowering writes
none of them. All of them were one edit away, for an image from another build,
a lowering bug, or a future feature that hands the machine a program the
lowering did not write. A machine that is safe only on programs a particular
compiler writes is not safe in the way a VM's contract usually means.

## Decision

### 1. The contract

For every `Program`, one of two things happens. The machine refuses it before
its first instruction: `cove_ir::verify`, the encoder or the bytecode verifier
says no, and nothing of it is encoded, converted or compiled
(`Prepared::of`). Or a run of it — any entry, any arguments, under a deadline,
a call-depth limit and a heap capacity — ends in `Ok(value)` or
`Err(RuntimeError)`. "Ends" excludes a panic, an abort, a Rust stack overflow,
a hang past the deadline, and undefined behaviour.

The two halves divide the work by what can be known. What one function's code
settles statically, `verify` settles. What depends on which object a slot
holds, how deep a walk is, or which closure is called, the machine checks where
it happens. Each run-time check is written once and shared by both tiers.

### 2. What `verify` adds

- **`verify/layouts.rs`.** Every layout's words are its shape's: a `Word(r)` is
  `[r]`, an object family is one `Ref`, a struct's fields and an enum's parts
  sit inside its words as their own layouts' words, an enum's words are a `Tag`
  and its payload. `LayoutId(0)` is the free layout. No inline layout contains
  itself. No heap payload holds an `Addr`. The program-wide layouts are the
  shapes they are named for.
- **`verify/places.rs`.** An address goes down the stack and nowhere else: it
  is copied within its frame and passed to a call, and is never answered,
  captured, boxed, stored through another address, handed to a host or a
  task, or allocated into an object. Every place formed has a *need*: per word
  offset, the `Repr` used there and whether it is written. The needs are found
  by reaching definitions within a function and to a fixed point across calls.
  A frame slot's address is checked against the frame here. A field's address
  is handed to the machine with its need.
- **A closure call may take an address** only when every definition of its
  closure slot that reaches the call allocates a closure whose layout names its
  function. That is the shape `lock` lowers to: it hands its closure the
  address of the cell's value. The call is then treated like a direct call to
  each of those callees.
- **An address that may have been loaded out of memory** is the one shape
  outside the need analysis. The lowering writes it for a tracked walk's path
  list, which is linked through the frames of the walks it is inside. A load or
  store through such an address is left to the machine. Moving an address
  *into* a frame through another one is still refused. So every address a
  frame holds points at that frame or a deeper one, and a loaded address names
  a frame that is live.

- **`serial::decode` refuses a span whose file the image does not hold**,
  as `encode` already did. Once a decoded program runs, a failed run reports
  its call chain through the source map, and a span past the map was an index
  out of bounds there.

### 3. What the machine checks

- a field load or store is held to the object's header, word for word
  (`agree.rs`, `cove_ir::field_agrees`), and an element access to the run's
  unit. The native tier's emitted fast path is built from the same rule, and
  its slow path is the same helper;
- an `addr.of-field` is held to the need `verify` worked out for it, against
  the object's header, where the address is formed;
- a load or store through a loaded address is held to the frame it lands in:
  inside a live frame of this task, with the layout's `Repr`s in that frame's
  slots (`Machine::frame_place`). The native tier compiles no function with
  one, so such a function runs encoded;
- a view is held, before any observation follows it, to what `dyn.open` and
  `dyn.child` answer (`View::held`). A heap value is its owner's whole object,
  with the owner's own header and offset nought. An inline value is words of
  the owner that `Machine::field` admits as its layout. The native tier answers
  only the observations that read no payload (`dyn.kind`, `dyn.same-type`,
  `dyn.same-object`, the identity set). `dyn.read`, `dyn.case`, `dyn.count`
  and `dyn.child` go to the runtime's helper, which checks the view;
- a closure call's arguments are word-for-word its callee's parameters, one by
  one, and its answer layout's words are the callee's return layout's. A
  closure call that hands an address calls one of the functions `verify`
  checked the place for (`Machine::closure_callee`, shared by both tiers);
- an address slot no formation reached holds 0, and 0 is refused wherever an
  address is followed. So 0 must name no frame word: the first word of every
  stack segment is a guard that no frame begins at (`mem::STACK_GUARD`).
  Before it, the outermost frame's slot 0 was at linear address 0, so a program
  passing it as a `var` argument was refused for a null it never wrote;
- a task handle is one word with no layout, so a `spawn` is held to its
  body's return layout and an `await` to the layout the task answers. A task
  awaited at another width would read its answer box past its end, and a body
  answering wider than its `spawn` said would write past it;
- the native tier polls on every backedge it emits, a backward `switch` edge
  included, so that a loop an edited program closes through a jump table
  stops at its deadline on both tiers;
- a `Shared` lock and unlock name a cell; every object a frame names the field
  address of is kept by the collector; `dyn.on-path` reads only entries inside
  this task's stack in use; and every word followed as an object is checked
  for null first.

### 4. The VM-level fuzz targets

`crates/cove-fuzz` gains two targets beside `image`, which now runs a decoded
program that verifies:

- **`ir`**: a program `cove_fuzz::irgen` builds from the input, by `Repr` and
  layout, so that a useful share verifies and the rest probe the verifier;
- **`ir_mutate`**: a lowered program read from an image body in the input,
  then edited by `cove_fuzz::mutate` — an operand, a layout, an id, a jump.

Each runs what verifies in every mode: `vm`, `vm-again`, `vm-gc`, `native` and
`native-gc` where the host has the tier, and `image`. They hold the runs to
[ADR 0113](0113-differential-fuzzing-compares-six-modes-and-a-stopped-run-by-its-prefix.md)'s
agreement. A program `verify` refuses must be refused by the machine too, and
not run. `docs/FUZZING.md` records how to run them and what they have found.

## Consequences

- An image from any build, a lowering bug or a hand-written program can no
  longer turn a verified program into memory unsafety in the VM. The claim is
  the contract above, fuzzed rather than proved.
- `cove_ir::verify` does more work: two passes over the layout table, and
  reaching definitions over every function that holds an address, to a fixed
  point across calls. It runs once per program.
- The machine pays for the checks that are not static. A field access goes
  through a table lookup and a mask, a closure call compares two interned
  numbers, and a load or store pays one length test unless its function has a
  loaded use. The cost on covefmt is recorded in the pull request that
  introduced this ADR.
- Reflection observations that read a payload, and functions with a load
  through a loaded address, now run through the runtime even on the native
  tier. The walks that use them are the standard library's rendering and
  equality over erased values. They pay a helper call per observation where an
  inline fast path used to answer.
- A new instruction that reads a word out of an object, forms or follows an
  address, or calls through a value has to be placed in one of the two halves:
  checked in `verify`, or checked by the machine where it runs. The fuzz
  targets are what notice when it is in neither.
