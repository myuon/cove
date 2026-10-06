# ADR 0084: A run may yield at a safepoint

- Status: Accepted
- Date: 2026-10-04
- Refers to:
  [ADR 0080](0080-a-host-call-may-answer-pending.md), whose quiescence rule,
  `OwnedVm`/`Step` and failure-exit park this extends from a host call to a
  safepoint; [ADR 0082](0082-a-parked-run-keeps-its-deadline.md), whose
  `cancel`/`time_left`/resume-asks-first rules a yielded run keeps;
  [ADR 0060](0060-a-backedge-tests-the-stride-before-it-calls.md) and
  [ADR 0078](0078-a-native-call-tests-the-stride-before-it-takes-a-safepoint.md),
  the stride safepoints at backedges and calls that are the preemption points;
  [ADR 0024](0024-a-stop-is-a-bound-not-a-point.md) and
  [ADR 0040](0040-a-bound-outlives-its-backend.md), whose accounting a yield
  must not move; `examples/edge`'s `--mix cpu-io` measurement (PR #587),
  which found the need
- Supersedes: nothing. ADR 0080 decides where a run may *park*; nothing
  accepted says a run leaves its thread only at a host call. This adds a
  second place, under the same rule
- Decides: that an embedder may ask a parkable run to give its thread up;
  where the run honours the request and where it declines; what crosses
  threads and how it is resumed; that a held `Shared` cell does not block a
  yield; and that compiled frames do

## Context

ADR 0080 made a run movable at a host call: a run waiting on I/O costs its
heap and no thread. A run that computes never makes one, so it keeps its
worker until it answers. `examples/edge`'s `crunch` tenant (PR #587) is that
run — up to 68 ms of arithmetic — and with four workers and a FIFO queue a 19
µs `hello` behind it waited 12.4 ms at the median and 83 ms at worst. That is
head-of-line blocking, and no queue discipline in the embedder fixes it,
because once a long run has a worker nothing takes the worker back.

Go met the same problem and answered it in two halves: the runtime makes a
goroutine stoppable at known points, and a monitor (`sysmon`) asks a
goroutine that has run longer than a slice (10 ms) to stop at the next one.
Cove already has the points. Every `SAFEPOINT_STRIDE` (1,024) instructions
the dispatch loop takes a safepoint — at backedges and calls alike, which Go
before 1.14 did not have for tight loops — and it reads a cancellation flag
there. What it lacks is a way out that is not an error.

## Decision

### 1. The request is a flag on the machine; the embedder raises it

`OwnedVm::yield_request() -> YieldRequest`: an `Arc<AtomicBool>` behind a
`Send + Sync` handle, one per machine and shared by every run it makes.
`YieldRequest::request()` is a relaxed store. The runtime lowers the flag
when a parkable run begins (a request raised too late for the last run is not
one for this one) and whenever the run leaves its thread — answered, parked
or yielded — because a request is about the run that is running now.

*When* to raise it is the embedder's. The runtime keeps no clock, no timer
and no slice length: a slice is a scheduling policy, and ADR 0080 and 0082
left every scheduling decision to the embedder. PHILOSOPHY's "Separate policy
from mechanism": the mechanism is "stop at the next safepoint you can"; the
monitor that decides who has had enough is `examples/edge`'s.

It is not the budget's `Cancellation`, though that is the flag a safepoint
already reads. A cancellation is shared by whatever the embedder chooses —
ADR 0082 notes it may be shared with other things — and means "end"; a yield
request is one run's and means "pause". Folding the two into one word would
make every cancellation reader also decode a yield, and every embedder that
shares a cancellation flag among runs unable to slice one of them.

### 2. The request is read only where a safepoint is already due

The dispatch loop's one comparison (`instructions >= next_check`) is
unchanged. Inside its branch, where the stride is reached, the loop now asks
`yield_requested()` — `parking`, then the flag, one relaxed load — before
`Machine::safepoint`. That is one load per 1,024 instructions, inside a
branch taken once per 1,024. The bulk operations' mid-instruction safepoints
(ADR 0052) and the native tier's safepoints do not ask: neither stands
between two instructions.

The yield itself is `#[cold]` and leaves the loop the way ADR 0080's park
does — an error marker through the failure exit, which `drive` recognises —
for ADR 0080's measured reason: a third outcome in the loop's arm cost 1–2.5%
on rows that never took it.

### 3. Where a run yields, and where it declines

A run yields at a due safepoint when the request is raised, it was started
by one of `OwnedVm`'s parkable entries, and:

| | |
|---|---|
| no host is running a Cove callback below the loop (`reentry_depth == 0`) | as ADR 0080 §2 |
| no compiled frame and no session caller below the loop (`nested == 0`) | as ADR 0080 §2 |
| no spawned task un-joined | as ADR 0080 §2 |
| no debugger installed | it has been asked about the instruction the loop stands at already |

Otherwise it **declines**: the safepoint runs as before, the request stays
raised, and the run yields at the first due safepoint where it can — when the
callback returns, when the task is awaited. `OwnedVm::yields_declined()`
counts the declining safepoints, which costs an increment on a cold path.
Declining rather than refusing for ADR 0080's reason: a program must not
behave differently because of where a scheduler's timer happened to land.

**A held `Shared` cell does not block a yield**, and this is the one place
the condition differs from ADR 0080 §2. Parking was forbidden with a cell
held when lock identity was the thread's; ADR 0080 §6 made it the task's,
and the clause stayed because nothing had asked for it to go. It can go
here because nothing else can be waiting for the cell: a cell is an object
in this run's heap, and no other task of the run is running. Keeping the
clause would make a long computation inside `lock` the one place a run could
not be sliced, which is exactly the head-of-line blocking this exists to
end. On the way out, `drive` returns before it gives cells back; the cell is
still held when the run resumes and is released where the program releases
it.

A spawned task's machine has no yield flag and never yields. Its parent
cannot yield beside it anyway.

### 4. A yielded run stands before the safepoint it yielded at

The loop counts an instruction before it asks whether a safepoint is due, so
at the yield the count includes the instruction at the frame's `pc`, which
has not run. The yield takes that count back and leaves everything else as
it was: the safepoint's cancellation check, fuel charge and collector poll
have not happened, the uncharged work stays pending on the machine, and
`next_check` still says a safepoint is due. Resuming enters the loop at the
frame's `pc`, counts the instruction again, and the first thing it does is
take the safepoint it yielded at.

So a yielded run is charged in the strides an uninterrupted run is charged
in, collects where it collects, stops on a fuel limit at the same charge, and
dispatches the same instruction count. It writes no trace event: the trace of
a yielded run is the trace of the uninterrupted one, and replays the same
way. The time spent yielded is put down as *descheduled* and left out of the
entry's `cpu`, as host wait is; it is not host wait either, so `wait` is
unchanged.

### 5. `Step::Yielded(YieldedVm)`

`Step` gains a third variant. `YieldedVm` is `Send` (asserted at compile time
in the crate and in `tests/yielding.rs`) and offers:

- `resume(self) -> Step` — no answer; asks `Meter::interrupted` first, as
  `ParkedVm::resume` does (ADR 0082 §3), so a run cancelled or past its
  deadline while it waited in a queue ends at once;
- `cancel(self) -> (OwnedVm, RuntimeError)` — ends it at the safepoint it
  stands at, as that safepoint would have: the budget's error at that
  instruction with the call chain, a held cell given back, pending fuel spent,
  `entry_exit`, `heap_summary` and `run_ended` written, classified by the
  budget as in ADR 0082 §2;
- `time_left`, `instructions`, `meter`, `yield_request`.

It is a type of its own rather than a `ParkedVm` with no request, because
`ParkedVm::resume` takes an answer and there is none to give: a type that
needed one sometimes would be a runtime check where a compile-time one is
free.

### 6. Compiled frames block a yield

A live compiled frame is on the native stack, which is the thread's. The
native tier's own safepoints do not offer a yield, and a VM frame entered
from compiled code has `nested > 0`. An `OwnedVm` has no native constructor
yet (ADR 0080), so today no yieldable run has a tier at all; when one does,
it yields only between compiled calls. Yielding *inside* compiled code would
need either a stack per run (a separate native stack that is moved with it)
or deoptimisation of the live compiled frames into VM frames at the
safepoint. Neither is decided here.

## Consequences

- An embedder can bound how long any one run holds a worker, to its slice
  plus at most one stride — at the VM's speed, a few microseconds — without
  the program's cooperation and without a thread per run.
- Every run that is not started parkably is unchanged, and so is every
  parkable run nobody asks to yield: the flag is read once per stride and
  found low.
- A yield costs a resume: the thread scope a resumed run opens and a machine
  arriving cold in another core's cache, about what a park costs (ADR 0080
  measured 2.7 µs a park). A slice of 1 ms pays it a thousand times a second
  per worker.
- `Step` has a third variant, so an embedder that matched it exhaustively
  must say what it does with a yield — `unreachable!` if it never raises a
  request.
- `drive` now returns early for a yield, before cells, tasks and pending
  fuel; the asserts that a body answered leave every cell are not reached for
  it, since it has not answered.

## Measured

**Hot path.** Base `6a6ac86` against this change, both `--profile checked` on
one x86-64 macOS machine (16 cores, otherwise idle), interleaved base/head
for three rounds: `cove-bench --matrix --iterations 5` (the VM rows,
medians) and covefmtBench (`cove run covefmtBench --files-root ../..
--backend vm|native --stats`, `execute=`). Means of the three rounds:

| row | base | head | | instructions |
|---|---|---|---|---|
| `conv_local` | 56.65 ms | 57.03 ms | +0.7% | 12,285,732, identical |
| `conv_static` | 57.25 ms | 56.97 ms | −0.5% | 12,285,731, identical |
| `conv_closure` | 125.24 ms | 124.69 ms | −0.4% | 16,285,734, identical |
| `conv_capture` | 143.88 ms | 143.22 ms | −0.5% | 16,285,736, identical |
| `conv_fresh` | 344.38 ms | 331.45 ms | −3.8% | 26,285,731, identical |
| `conv_host` (2 M host calls, each running a callback) | 4,245.7 ms | 4,174.3 ms | −1.7% | 40,285,731, identical |
| covefmtBench, VM | 12.609 s | 12.504 s | −0.8% | 2,227,553,881, identical |
| covefmtBench, native | 3.276 s | 3.253 s | −0.7% | 211,921 encoded, identical |

(`conv_var`, `conv_fnvalue` and `conv_generic` moved by −0.7% to 0.0%.) The
largest rise is `conv_local`'s +0.7%, 0.4 ms, against a three-round spread
of 0.1 ms on each side; the falls are layout and are read as "not slower",
as ADR 0080 read its own. Nothing on these rows asks for a yield, so what is
measured is exactly the added load-and-branch once a stride and the moved
code, and neither is visible. (covefmt's count moved between the first round
and the second on *both* sides, 2,227,553,822 to …881, because a new file
appeared in the tree it formats while the rounds ran — covefmt's corpus is
every file under `--files-root`; within each round base and head agree.)

**Yields.** `tests/yielding.rs` holds the contract: a run made to yield
twenty times, each resume on a thread of its own, answers the uninterrupted
run's value in its instruction count for its fuel, with its trace; a fuel
limit stops it at the same charge; a monitor thread raising the request every
200 µs slices a 400,000-iteration loop many times with the same answer; a
request inside a callback or beside a running task is declined at every
safepoint until the callback returns or the task is awaited, and then honoured
once; a run yields inside a `lock` and leaves it as the uninterrupted run
does; a request raised before a run begins is lowered; `cancel` and a
deadline end a yielded run with the trace a stopped run writes. What slicing
costs a real server is `examples/edge`'s README, "Slicing long runs at
safepoints".

## What this does not decide

- **A scheduler**, a slice length, or a monitor. `examples/edge` builds one
  (work stealing, a time slice, a monitor thread) as an example, not API.
- **Yielding inside compiled code** (§6): separate stacks or deoptimisation.
- **Yielding a spawned task**, which is a thread of its own (ADR 0008) and
  has no scheduler to yield to.
- **Yielding inside a host's callback.** It would need the host's Rust frame
  to be unwound and re-entered, which ADR 0080 declined for parking.

## Alternatives considered

- **A bit in the budget's cancellation word.** One load instead of two at a
  safepoint, but §1's reasons: the cancellation is shared and means "end".
  The second load is on a path taken once a stride and is not measurable.
- **Fold the request into `next_check`** — have the monitor lower the
  threshold so that the next instruction takes the branch. `next_check` is a
  plain field the loop owns; writing it from another thread would make it an
  atomic in the one comparison every instruction makes.
- **A runtime timer that requests the yield itself** (an instruction or
  wall-clock slice in `Limits`). It would put a scheduling policy — whose
  clock, what length, measured how — in the runtime, which ADR 0082 declined
  for deadlines for the same reason.
- **Charge the pending work at the yield.** Then the resumed run finds less
  than a stride pending, skips the safepoint, and its schedule shifts by up to
  a stride for the rest of the run: the same instruction count, but fuel
  stops and collections at different points than the uninterrupted run's.
  Standing before the safepoint keeps them identical.
- **Keep ADR 0080's held-cell clause.** Simpler to state, at the cost §3
  gives: a `lock` region could not be sliced.
- **Clear the request when declined.** The monitor would have to ask again
  each tick and the run would leave a callback with no request pending,
  holding its worker until the next tick; keeping it raised yields at the
  first safepoint after the callback returns.
