# ADR 0079: A direct call opens its frame in emitted code

- Status: Accepted
- Date: 2026-10-02
- Supersedes:
  [ADR 0078](0078-a-native-call-tests-the-stride-before-it-takes-a-safepoint.md)'s
  **"No emitted code changes. The compare is in Rust because the helper is
  already running and already holds the two numbers it compares."**, for a
  *direct* native-to-native call. The poll that decision moved into the `open`
  helper now runs in front of it, in emitted code, as the first of four
  compares that decide whether the helper is called at all. The mediated
  `call` helper, and `open` itself when it is called, poll exactly as ADR 0078
  says, and so does the rest of that ADR
- Preserves:
  [ADR 0040](0040-a-bound-outlives-its-backend.md)'s bounds in their own units
  (the work between two polls is still at most `S + T`);
  [ADR 0055](0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md)'s
  root discipline — every live reference is in a frame slot the collector finds
  through the runtime's frame list, at every safepoint — and its safepoint
  contract;
  [ADR 0057](0057-a-native-call-returns-into-the-destination-its-caller-named.md)'s
  return path, its stable indices and its rule that the destination is written
  before the callee's frame is removed;
  [ADR 0034](0034-one-physical-word-stack.md)'s one word stack per task
- Decides: who opens and closes the frame of a direct native-to-native call in
  the common case, what the runtime's frame and word stacks look like so that
  emitted code can, and the exact condition under which it does not

## Context

PR #368 split a compiled call into `open`, the callee's entry, and `close`, so
that the arguments and the entry itself moved into emitted code. What stayed in
Rust was everything that changes how long a runtime `Vec` is: the word stack
(`Memory`'s `Vec<u64>`) and the frame list (`Machine`'s `Vec<Frame>`). ADR 0078
then made the call a poll, and named the next step and declined it in the same
breath: inlining `open` and `close` "is where the rest of the call path's cost
is … but the frame stack and the word stack are Rust `Vec`s that only Rust may
resize … It is not decided here."

This ADR decides it, because the cost is measured and it is the largest single
runtime cost left around compiled code.

### What `open` and `close` cost

Sampled on `main` at 6818726 (`/usr/bin/sample`, the method and buckets of
`scripts/covefmt-profile.sh`; `--profile checked`; x86-64 macOS; load average
2.5 to 3.1 while sampling), share of the Cove thread's samples:

| program, native tier | `open` (self) | `close` (self) | frame zeroing under them | total |
| --- | ---: | ---: | ---: | ---: |
| covefmtBench, 5 runs | 6.69% | 2.25% | ≤2.28% | ≈11% |
| cq `revenue-summary` 100k, 3 runs | 21.62% | 5.98% | 5.11% | ≈33% |

`open`'s mediated arm (a callee with no machine code: `call`, charged to
`open`'s children and not counted above) is a different call and not this
ADR's.

What `open` spends its time on, from its sampled program counters on cq
(2,604 samples; a sample lands on the instruction *after* a stall, so ranges
are attributed to the statement they belong to and are approximate):

| part of `open` | share of `open` | needed on the common path? |
| --- | ---: | --- |
| the C-ABI entry: six pushes and a 472-byte Rust frame | ≈16% | no — an artefact of being a function |
| reaching the bridge, the tier, the entry table | ≈14% | the entry lookup, yes: one load |
| `admit_frame`: meter → `Arc` → `limits().max_call_depth` | ≈17% | a compare, yes; the pointer chase, no |
| `program.function(callee).frame_size()` | ≈10% | no — the code generator knows it |
| the transition counter in the tier's `Box` | ≈14% | a count, yes; that cache line, no |
| `sync`, the caller's `Frame`, its debug assertions | ≈13% | the resume `pc`, yes |
| `Vec::push` of the callee's `Frame` | ≈8% | the record, yes |
| `push_frame` (its `memset` is a further 572 samples) | ≈2% | the zero fill, yes; the parameters' words, no |
| the charge, the poll, `republish` | ≈5% | the charge and the poll, yes; `republish`, no |

`close` is already short — about forty instructions — and its cost is the call
itself, the charge, the threshold it republishes, a `Vec::pop` and a truncation.

So almost everything `open` and `close` do on the common path is either a
number the code generator already knows (the callee's frame size, its
parameters' widths, its id, its destination, the call's `pc`), or one compare
against a number the runtime could publish (the stride, the call depth, the
storage), or a store into a structure whose layout the runtime could publish.
Only three things genuinely need Rust: a safepoint, a growth of either stack,
and a refusal.

## Decision

### The two stacks are published, not hidden behind `Vec`

The length of each stack becomes a field that compiled code may write, in a
`#[repr(C)]` declaration in `cove_native::abi` that is the contract:

- **`FrameStack { records, len, room }`** is the frame list. `Machine::frames`
  is `Frames`, which owns a `Vec<Frame>` whose every element is initialised and
  of which the first `len` are frames, and derefs to `[Frame]` — so every
  reader in the runtime reads it exactly as it read the `Vec`. The runtime's
  `Frame` is `#[repr(C)]` in `FrameRecord`'s field order and the two layouts are
  asserted equal at compile time.
- **`WordStack { len, room }`** is the top of the word stack. `Memory`'s
  storage is a `Vec<u64>` whose length is the room, and `len` is the words in
  use; `push_frame`, `pop_frame` and `holds` read `len` where they read the
  `Vec`'s length.

Both are reached through new `NativeCtx` fields — `frames`, `stack`, the
machine's `bulk_work`, and the runtime's entry table `entries` — published at
every entry into compiled code. None of them moves during an entry.

**Only Rust grows either stack**, and a growth is always a new allocation with
the words (or records) in use copied across, doubling. "Always new" is
deliberate: whether a `Vec` growth moved was the allocator's choice — macOS's
moves, glibc's may extend in place — and the pull requests that found that out
did so on CI only. Now it is a fact the runtime states, and a test constructs
it.

### `room` is the whole of the admission

Each `room` is a number the runtime keeps at most what the storage holds, and
at most what a refusal allows:

- `FrameStack::room = min(records the storage holds, max_call_depth)`, or
  **nought** when the installed table is not a slice covering every function or
  counts its helper calls;
- `WordStack::room = min(words the storage holds, SEGMENT_WORDS - 1)`.

So a frame that fits under both rooms is a frame `open` would have pushed
without growing anything and without refusing.

### The fast path, and the exact condition that leaves it

A direct call whose callee is in the subset is emitted as:

```text
cmp r13, [ctx.poll_at]          ; jae slow   a poll is due          (ADR 0078)
rax = ctx.entries[callee]       ; jz  slow   no machine code: mediated
frames.len < frames.room        ; jae slow   storage full, or max_call_depth
stack.len + size <= stack.room  ; ja  slow   storage full, or the segment
-- committed: nothing below can fail --
stack.len += size
records[len - 1].pc = pc + 1                 the caller resumes after the call
records[len] = { base, callee, pc: 0, dst }  the callee is walkable
frames.len += 1
*ctx.bulk_work += r13; ctx.poll_at -= r13    open's charge and republish
ctx.direct_calls += 1                        open's transition count
zero words [params, size) of the frame       push_frame's fill, sixteen
                                             bytes a store, less the words
                                             the arguments overwrite
... arguments, entry call (unchanged) ...
outcome == Returned:
  *ctx.bulk_work += ctx.pending_work         close's charge
  ctx.poll_at = sat(ctx.poll_at - pending)   close's threshold
  frames.len -= 1; stack.len = callee base   close's pop
outcome != Returned:  call close             (builds the error, as before)
slow:  the PR #368 sequence, calling open
```

**The fast-path condition is the conjunction of the four compares**, and each
one fails exactly where `open` would do something emitted code does not: take a
safepoint, run a mediated call, grow storage or refuse a depth, grow storage or
refuse an overflow. The slow path is `open`, unchanged, and a frame it opened
is closed by the same inline close, because both leave the same record on top
and the same base in `r15`.

`direct_calls` is a counter in the context, folded into the run's
`native_to_native_direct` when the entry returns — where `open` would have
counted the call — and reported apart as `native_to_native_inline` ("of which
opened inline") so that a run can say which path its calls took.

### How each invariant is kept

- **The collector finds every frame and every root.** A collection happens only
  inside a helper — an allocation or a safepoint — and the frame list it walks
  is the one emitted code wrote, with the record written before any instruction
  of the callee runs. The record's `function` names the reference map and its
  `base` the slots; its reference slots are zero until written. Shown by
  `a_collection_under_frames_opened_inline_keeps_what_they_hold` (600 compiled
  frames holding an array each, three collections, every array read back after);
  a record whose function is wrong aborts the suite (checked by mutation).
- **Traps and errors unwind and report the right source.** A raise leaves its
  frames standing and goes to `close`, which builds the error as before; the
  caller's resume `pc` is the one `open` wrote. Shown by
  `a_raise_below_frames_opened_inline_is_the_vm_s_error` (the message, the span
  and the chain equal the VM's forty frames down, and the machine is whole for
  the next call); omitting the resume `pc` fails it and the existing ADR 0058
  blame case (checked by mutation).
- **Cancellation and fuel bounds are unchanged.** The inline poll is the same
  compare `open` makes after its charge, made before it; a call that is due goes
  to `open`. The charge and the threshold are moved by exactly what `open` and
  `republish` would move them by, so the interval between polls is still at most
  `S + T`. Shown by ADR 0078's two cases, which now also assert the descent was
  opened inline; dropping the threshold update fails the fuel case (checked by
  mutation).
- **The VM, the native tier and the frame list agree.** There is one frame list
  and one word stack; the encoded tier pushes onto the same storage through the
  same fields. Shown by `frames_opened_inline_interleave_with_encoded_frames`,
  which crosses the boundary both ways every fourth level for a thousand levels.
- **Stack growth and reallocation.** Emitted code never grows anything; a frame
  that would is sent to `open`, which grows in a new block and republishes the
  words pointer, as before. Shown by
  `frames_opened_inline_and_by_open_return_through_a_growth` (5,000 levels, both
  paths taken, every destination pending across several growths) and
  `the_published_room_is_storage_and_a_growth_moves_it`, which constructs the
  move rather than hoping the allocator makes it.
- **`max_call_depth` and the segment bound.** The rooms are capped by both, so
  the refused frame is refused by `open` with the VM's sentence. Shown by
  `a_call_depth_limit_holds_for_frames_opened_inline`.
- **Helper counting and ablation.** A table compiled to count its helper calls
  is compiled without inline frames, and the runtime's room is nought for such a
  table besides, so every direct call reaches the `open` it counts.
- **Debugging and tracing.** Nothing that walks frames is reached between the
  inline open and the callee's first helper, and what it would walk is the same
  list `open` built.

## Consequences

Measured on x86-64 macOS, this ADR's commit against its parent, both built
`--profile checked`, interleaved, fifteen runs an arm in one session, median
[min..max]:

| row | before | after | delta | load average at start |
| --- | ---: | ---: | ---: | ---: |
| covefmtBench, native, wall | 3,842 [3,822..3,874] ms | 3,607 [3,575..3,717] ms | −6.1% | 2.63 |
| covefmtBench, native, `whole` | 326 [323..330] ms | 298 [295..333] ms | −8.6% | 2.63 |
| covefmtBench, VM, wall | 13,364 [13,259..13,482] ms | 13,097 [13,010..13,174] ms | −2.0% | 2.63 |
| covefmtBench, VM, `whole` | 1,229 [1,223..1,277] ms | 1,204 [1,193..1,237] ms | −2.0% | 2.63 |
| cq `revenue-summary` 100k, native, wall | 5,078 [5,031..5,285] ms | 4,008 [3,947..4,125] ms | −21.1% | 2.95 |
| cq `revenue-summary` 100k, VM, wall | 10,719 [10,647..10,850] ms | 10,465 [10,416..10,674] ms | −2.4% | 2.95 |
| `cove fmt --check` over the repository | 602 [593..627] ms | 574 [569..597] ms | −4.7% | 2.60 |

Every run of both tiers printed the same bytes (covefmtBench) and wrote the
same CSV (cq). Of covefmtBench's 15,714,848 direct calls, 15,604,076 (99.3%)
opened their frame inline; the rest are the polls that were due and the
growths.

The VM moved too, and in the right direction, by about 2% on both programs —
beyond covefmt's ±1% floor and inside cq's ±2.9%. It runs none of the emitted
code, so this is the representation change: `push_frame` zeroes a slice of
storage it already has rather than calling `Vec::resize`, and the frame list
is pushed without a capacity check against a separate field. It is reported,
not claimed.

The sampled profile (same method as above, after the change, load 2.7 to 2.9):

| bucket, native tier | covefmtBench before → after | cq before → after |
| --- | ---: | ---: |
| `open`/`close`/`call` helpers | 8.97% → 0.14% | 30.76% → 4.63% (what is left is the mediated `call`, `drive_from` and `enter` of native-to-VM calls) |
| frame growth and zeroing | 4.38% → 2.55% | 5.86% → 2.35% |
| generated native code | 66.70% → 76.29% | 37.51% → 58.36% |

Machine code for covefmt grew from 1,185,071 to 1,314,151 bytes (+10.9%).

- **A direct call no longer enters Rust on the common path.** The C-ABI hop,
  the Rust prologue, three pointer chases and two `Vec` operations become about
  thirty emitted instructions plus one sixteen-byte store per two zeroed words.
  The fill is what is left, and it is most of it: the same change with
  eight-byte stores measured −1.7% on covefmtBench's native wall, and with
  sixteen-byte ones −6.5% (seven interleaved runs each, same session).
- **The code is larger**, by the 10.9% above: every direct call site carries
  the fast path and the inline close beside the old sequence, which stays as
  the slow path.
- **The VM's frame list and word stack changed representation.** Every reader
  sees the same slice; a push compares against the storage length rather than a
  capacity, and growth is a copy into a new block rather than `realloc`.
- **The frame list and word stack are an ABI now.** `FrameRecord`,
  `FrameStack` and `WordStack` are the contract; the runtime asserts its
  `Frame` matches, and the code generator asserts the record is 24 bytes with
  `pc` after `function`.

## What this does not decide

- **The mediated call.** `call` — a compiled caller and a callee with no
  machine code, or a call site the code generator cannot make direct — is
  unchanged.
- **Allocation as a poll.** ADR 0078 declined it on measurement and this does
  not revisit it.
- **A VM-to-native call.** `from_encoded` still enters through `enter`, which
  is where the rooms are published.
- **Register-resident frames, or eliding the zero fill.** The fill could be
  limited to reference slots, and the record could be written lazily; both are
  further changes to what a frame is, and neither is needed for this one.
- **The return poll.** `close` was not a poll and the inline close is not one
  either: ADR 0078's "What is not changed" stands.

## Alternatives considered

**Keep the `Vec`s and give emitted code a pointer to their length.** A `Vec`'s
length is private to `std`, and writing it from outside is undefined behaviour
whatever the layout happens to be today.

**Reserve the whole segment up front, so the word stack never moves.** It
would remove the words-room compare and every republish of `words`, but it is a
megabyte-scale reservation per task on hosts where memory is not lazily
committed — the browser playground's linear memory among them — for a compare
that costs one instruction.

**Inline only the close.** `close` is a quarter of the cost; `open` is the
rest.

**Leave it.** On cq a third of the native run is the runtime opening and
closing frames whose every property the code generator already knew.
