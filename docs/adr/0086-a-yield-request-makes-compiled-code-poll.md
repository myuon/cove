# ADR 0086: A yield request makes compiled code poll

- Status: Accepted
- Superseded in part by [ADR 0090](0090-a-host-call-s-charge-begins-a-stride-for-a-yield-too.md), for §2: a host
  call's charge also lowers `just_resumed`
- Date: 2026-10-05
- Refers to:
  [ADR 0085](0085-compiled-frames-resume-where-they-yielded.md), whose resume
  points, yield points and resume rule this corrects and completes;
  [ADR 0084](0084-a-run-may-yield-at-a-safepoint.md), whose "the request is
  read only where a safepoint is already due" (§2) this changes for compiled
  code and keeps for the dispatch loop;
  [ADR 0060](0060-a-backedge-tests-the-stride-before-it-calls.md), the
  backedge poll whose threshold carries the request;
  [issue #604](https://github.com/myuon/cove/issues/604), which found both
  problems
- Supersedes: nothing. ADR 0084 and ADR 0085 are Proposed; this records what
  changed in them and why, and ADR 0085's one wrong sentence points here
- Decides: that an instruction-start resume point re-derives the frame
  pointer; that a run resumed from a yield takes the safepoint it stood
  before before it may yield again; that while a yield is asked for compiled
  code is told its next poll is due, and the poll offers the yield without
  taking a safepoint that is not; and that the dispatch loop does not do the
  same, for a measured reason

## Context

cove-tools' algorithm playground found two things in ADR 0085's native yield
(issue #604).

**Wrong answers.** A native run sliced while `Array.toVector` copied an array
could resume with an allocation of the wrong size: "`runCopy` writes 12000
element(s) to 0 of a destination of 0", or "this run has no memory left".
`bench/repro` in cove-tools failed 13–19 of 160 requests with a 2 ms slice,
none unsliced and none on the VM.

The cause is ADR 0085 §1's claim about instruction-start resume points: "At
each of these points nothing but the frame, the context and the prologue's
registers is live — the code generator already re-derives its frame pointer
there." It does at a block's start and after a call, where `Emit::frame_live`
is false. It does not before an `Inst::Alloc` or an `Inst::Call` in the
middle of a block: there `frame_live` is whatever the previous instruction
left, usually true, so the template reads its operands through the frame
pointer register without deriving it. Entered from the resume prologue,
which derives no frame pointer, that register holds whatever the code that
called the prologue left in it. `core.arrayToVector` lowers to `Len`, then
`Alloc { len: Len::Slot(len) }`, then `RunCopy`, so the `Alloc` re-run on
resuming read its count from a wild address — nought, mostly — allocated an
empty store, and the copy into it refused. A call re-run there could read
its arguments from the same wild address.

**A loop that neither yields nor declines.** The same app's loop of
`toVector()` and a length check was asked to yield 22 times and did neither.
Compiled code learns of a request at a poll: a backedge whose accumulated
work has reached the stride, an allocation, or a call whose poll is due. A
loop whose every turn passes a helper that takes a safepoint — a copy's
chunked charge, a buffer's growth, a string's text — has its stride reset each
turn by that helper, so its backedge is never due, and those helpers stand
inside an instruction and cannot yield. Such a loop polls nowhere a yield can
be taken, however long it runs. A storm of requests over a string-building
loop reproduced it: 22 ms and more without a yield, none declined.

A third thing was found while testing the fix: a run resumed from a yield
with its request raised again before it moved — a monitor raising it as fast
as the runtime lowers it — yields again at the very safepoint it stood before,
having done nothing. ADR 0085 §5 lowers the request on resuming; a request
raised in the next microsecond is not lowered.

## Decision

### 1. An instruction-start resume point re-derives the frame pointer

`cove_native`'s code generator marks the frame pointer dead
(`frame_live = false`) before it records an instruction-start resume point
and emits the instruction, as it already is at a block's start and after a
call. The template derives the pointer itself, from the context and the
frame's base, which the resume prologue does set. The cost is one load and
one add before each `Inst::Alloc` and `Inst::Call` that follows another
instruction in its block.

So ADR 0085 §1's invariant now holds as stated at all three kinds of point:
nothing is live but the frame, the context and the prologue's registers.

### 2. A resumed run takes the safepoint it stood before before it yields again

The machine keeps `just_resumed`, set when a yielded run resumes and lowered
by the next safepoint it takes — which is the one it stood before (at a
block, `native::resume` takes it; at an instruction, the re-run instruction
takes it; on the dispatch loop, the loop takes it). While it is set, a
request is not honoured, and not counted as declined. So a run always makes
progress between two yields, and a request raised early waits at most one
stride.

### 3. While a yield is asked for, compiled code's next poll is due

The threshold compiled code is given (`NativeCtx::poll_at`, published at
entry and after every helper) is `Machine::native_poll_at`: the stride's
remainder as before, or **nought while a yield is asked for and this run has
not declined one this stride and is not just resumed**. Nought makes the next
backedge call the safepoint helper and the next inline call take the
`open` path. Those offer the yield whether or not a safepoint is due:

| poll | safepoint due | not due (the request made it early) |
|---|---|---|
| a backedge (`safepoint`) | offer; resume takes the safepoint, enters the block (`AtBlock`) | offer; resume enters the block and takes nothing (`BeforeBlock`, new) |
| a call (`open`, `call`) | offer; the call is re-run and takes it (`AtInstruction`) | offer; the call is re-run and takes nothing |

Nothing that is not due is taken, so a yielded run's safepoints, charges and
collections are still the uninterrupted run's: moving the accumulator's work
into `bulk_work` early leaves `poll_budget` — which counts both — where it
was. A decline sets `declined_this_stride`, which `native_poll_at` reads, so
a run that cannot yield (below a callback, an encoded callee, a debugger)
pays one early poll a stride and not one a turn; the next safepoint lowers
it. A helper sees a request raised by another thread when it next publishes
the threshold, which in such a loop is every turn.

The cost when nothing asks: `native_poll_at` is one `bool` test for a run
that is not parkable and one relaxed load for one that is, at every helper
return — measured as no change (below).

### 4. The dispatch loop keeps ADR 0084 §2

The encoded tier has the same starvation: a VM loop whose every turn makes a
bulk charge — `Vector.snapshot` in one — takes its safepoints inside the
instruction and never finds one due at the loop's check, so it does not
yield. Polling early there was tried, in the two ways the loop allows: an
`else` arm on the stride test, and that arm as an out-of-line call. Both cost
every program, because the dispatch loop's footprint is what every
instruction pays (ADR 0080 and 0084 measured the same thing): `crunch` on the
VM +1.5–2.0% and the edge path +2.3% for the inline arm; `arith` +6.9%,
`call` +4.5% and `crunch` +3.0% for the out-of-line call — on rows that never
ask. That is not a price this fix may charge the VM, so
the dispatch loop is unchanged and the case is left to its own issue
([#606](https://github.com/myuon/cove/issues/606)).

## Consequences

- `examples/edge`, cove-tools and any embedder slicing native runs get right
  answers: `bench/repro` 0 failures in 2,400 requests (15 runs), against
  13–19 of 160 before.
- A request is honoured at the next poll compiled code makes, not the next
  *due* one: in `native_yielding.rs`'s `below` case, four requests are four
  yields, where ADR 0085 made one. No compiled shape in the storm test runs
  25 ms without yielding when asked.
- `Stands` has a third case, `BeforeBlock`, and `ResumePoints` is unchanged.
- The encoded tier still cannot be sliced in a loop of bulk operations (§4).

## Measured

On the same x86-64 macOS machine, 2026-10-05, base `9272282` against this
change, three interleaved rounds, medians, load average 2.8–6.9 (raw:
the PR description).

| row | base | head | |
|---|---:|---:|---:|
| `crunch n=20000`, native `Vm` | 733.8 µs | 728.1 µs | −0.8% |
| `crunch n=150000`, native `Vm` | 10.72 ms | 10.62 ms | −0.9% |
| `crunch n=20000`, edge path native | 740.8 µs | 737.3 µs | −0.5% |
| `hello`, native `Vm` | 4.01 µs | 3.97 µs | −0.8% |
| `crunch n=20000`, VM | 4,306 µs | 4,338 µs | +0.7% |
| `arith`, VM | 48.7 ms | 48.2 ms | −0.9% |
| `call`, VM | 57.6 ms | 56.4 ms | −2.0% |
| covefmtBench, native / VM `execute` | 3,281 / 12,600 ms | 3,282 / 12,605 ms | 0.0% / 0.0% |

Instruction counts are identical on every deterministic row.

## Alternatives considered

- **Drop allocations and calls as yield points.** Removes the wrong answers
  by removing the points, and brings back the starvation ADR 0085 added them
  to cure: a loop that allocates every turn, and a recursion, would not yield.
- **Save the frame pointer in the resume table.** There is nothing to save:
  it is derived from two values the prologue already has.
- **Let helpers that stand inside an instruction yield.** They would need a
  resume point inside a template, or inside a buffer window whose state is
  in registers. Making the next poll early reaches a point that is already
  resumable instead.
- **Clear the request at every helper.** It would drop a request the run
  could have honoured a few instructions later.
