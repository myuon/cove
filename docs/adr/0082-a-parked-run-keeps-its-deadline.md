# ADR 0082: A parked run keeps its deadline

- Status: Accepted
- Date: 2026-10-04
- Refers to:
  [ADR 0080](0080-a-host-call-may-answer-pending.md), whose "What this does
  not decide" leaves cancelling a parked run open and notes that a deadline
  "keeps running while the run is parked" without anything acting on it;
  [ADR 0001](0001-mvp-language-design.md)'s position that time limits are
  runtime controls; [ADR 0024](0024-a-stop-is-a-bound-not-a-point.md), whose
  bounds this keeps true across a park;
  issue [#577](https://github.com/myuon/cove/issues/577), which made the
  budget the run's, so that a parked run carries its own
- Supersedes: nothing. ADR 0080 is Proposed and decides nothing about a parked
  run's deadline; this decides what it left open
- Decides: what a parked run says about its deadline, how an embedder ends a
  parked run, and what resuming one past its deadline does

## Context

ADR 0080 lets a host answer a call pending and hands the run back as a
`ParkedVm`, which the embedder resumes wherever it likes. Two things were left
open, and `examples/edge` found both (its README, "What was awkward" item 8):

- **A parked run's deadline never fires.** `Limits::deadline` is wall-clock
  and starts with the run, and `invoke_within_parkable` documents that it
  bounds the run "parked time included" — but the deadline is read at a
  safepoint, and a parked run reaches none. An upstream that never answers
  holds the run, and in a server the socket it owes a response to, until the
  embedder gives up by some means of its own.
- **There is no way to end a parked run.** Dropping a `ParkedVm` abandons it:
  its memory is freed and its trace stops at `entry_enter`, which reads as a
  run that never finished rather than one that was stopped.

There is a third thing ADR 0080 did not notice. A run resumed after its
deadline passed is not stopped at once: the answer is written, and the run goes
on until a safepoint reads the clock — which with a fuel limit is one in 64 —
or until its next host call, whose charge always reads it. A run with neither
left before it answers answers, past a deadline that was meant to bound it.

## Decision

### 1. The runtime does not wake a parked run; it says when its deadline is

`ParkedVm::time_left() -> Option<Duration>`: what the run's deadline leaves,
zero once it has passed, `None` for a run with no deadline (`Meter::time_left`
underneath). The clock is the run's own, started when the run began and not
paused by the park.

No timer, thread or callback is added to the runtime. *When* to look at a
parked run is scheduling, and ADR 0080 left every scheduling decision to the
embedder; a deadline that woke a run would be the runtime owning a timer per
parked run, which is a policy (a thread? a heap? what resolution?) the
embedder already has an answer to — it is holding the run in a structure of
its own, waiting for an answer that is due at some time. PHILOSOPHY's
"separate policy from mechanism": the mechanism is "how long is left" and
"end it"; the embedder sets the timer.

A `Duration` rather than an `Instant` because the runtime's clock is not
`std::time::Instant` everywhere (`wallclock.rs`: on `wasm32` it is an imported
host function), and a scheduler converts once, at the park.

### 2. `ParkedVm::cancel` ends a parked run as a safepoint ends a running one

`ParkedVm::cancel(self) -> (OwnedVm, RuntimeError)`. The run is ended at the
call it is parked at, with the stop a safepoint would report, asked in the
order a safepoint asks it (`Meter::interrupted`):

- `Stopped::Cancelled` if the budget's `Cancellation` is raised;
- `Stopped::Deadline` if the deadline has passed;
- otherwise `Stopped::Cancelled`, because the embedder cancelling it is what
  this call is.

The error is the budget's own (`Meter::to_runtime_error`) — "execution stopped:
wall-clock deadline of 300ms exceeded", with ADR 0001's rule and the
`RunOutcome` of the stop — at the parked call's span, with the call chain under
it. It is produced by the path a resume takes, with the stop as the host's
answer: the call's `host_call` event is written with that error as its
outcome and the parked time as its wait, then `entry_exit`, `heap_summary`
and `run_ended`, classified `deadline` or `cancelled`. So a trace says the run
was stopped, and where. The machine comes back for its next run, as an
answered `Step` would return it.

There is nothing else to clean up, and that is ADR 0080's quiescence doing
the work: a run parks only with no task running, no `Shared` cell held and no
callback below it, and its pending fuel was spent before the call. The host's
request, if the embedder did not take it, is dropped with the run's suspended
state.

The classification is by the budget rather than by a second method
(`expire` beside `cancel`) because the embedder's timer and the run's budget
can disagree only by the time between them, and the budget is the record ADR
0024's bounds are stated against: a scheduler that cancels a run because its
timer fired gets `Deadline` exactly when the deadline has in fact passed.

### 3. Resuming a run that has been stopped fails at once

`ParkedVm::resume` asks `Meter::interrupted` first. If the run's flag is raised
or its deadline has passed, the answer is discarded unread and the run ends as
`cancel` ends it. This closes the gap in the Context: a deadline is a bound on
the run, the run was parked past it, and the answer arriving does not make the
run younger. It also makes the race between an embedder's timer and an answer
that lands at the same moment harmless: whichever the scheduler acts on, the
run past its deadline ends with `Deadline`.

The check reads an atomic and the clock once per resume. A resume already
costs about 9 µs (ADR 0080's measurement) and opens a thread scope; one clock
read is not visible beside that.

## Consequences

- An embedder can bound a parked run by the deadline it gave it: read
  `time_left` at the park, put the run in its timer structure at the earlier
  of the answer's due time and the deadline, and `cancel` it if the deadline
  comes first. `examples/edge` does this and answers 504; its `hang` upstream
  answers after an hour, and a tenant's deadline ends the request instead.
- A trace of a cancelled parked run ends with `run_ended`, which a dropped one
  does not. Its last `host_call` carries the stop as an error outcome, so a
  replay of that tape fails at the same call with the same message (as a host
  failure; the replay does not reproduce the `deadline` classification).
- `resume` is now not only "the answer": a stale answer to a run past its
  deadline is refused, and an embedder that wanted to deliver it anyway raises
  the deadline when it builds the budget, not after.
- Dropping a `ParkedVm` still abandons it, and that stays allowed: it is the
  embedder saying the run does not matter, and the runtime has nothing to
  hold it to.

## What this does not decide

- **Waking a parked run from inside the runtime**, a timer wheel, or a
  `Future`-shaped resume. Nothing has shown the need; an embedder that wants
  one builds it over `time_left`.
- **Cancelling the host's pending work.** The request is the host's (ADR
  0080 §4); whether an outbound fetch is aborted when its run is cancelled is
  between the host and the embedder, which holds both.
- **A run blocked inside a host call** (not parked) is unchanged: its
  deadline is read when the call returns, as before.

## Alternatives considered

- **`ParkedVm::deadline() -> Option<Instant>`.** The natural shape for a timer
  heap, but the runtime's clock is not `std::time::Instant` on every target,
  and the scheduler converts a `Duration` once.
- **A runtime timer that resumes a run with the stop.** The runtime would own
  a thread or an event loop per process, which is the scheduler ADR 0080 did
  not build.
- **Leaving `resume` alone, so a late answer runs until the next safepoint.**
  It would make the deadline a bound only for runs that happen to reach a
  safepoint, which is the claim ADR 0024 does not let a stop make.
- **Raising the budget's `Cancellation` in `cancel`.** It changes nothing the
  run can observe — it has ended — and would make the flag, which the
  embedder may share with other things, say something the embedder did not.
