# ADR 0080: A host call may answer pending

- Status: Accepted
- Date: 2026-10-03
- Issue: [#571](https://github.com/myuon/cove/issues/571)
- Refers to:
  [ADR 0001](0001-mvp-language-design.md)'s "I/O wait suspends a task", which
  this is the embedder's half of;
  [ADR 0008](0008-concurrent-task-execution.md)'s thread per spawned task,
  which it keeps;
  [ADR 0013](0013-host-resource-handles.md)'s handles, whose operations may
  pend as a module's may;
  [ADR 0024](0024-a-stop-is-a-bound-not-a-point.md) and
  [ADR 0030](0030-a-host-call-asks-the-fuel-limit.md), whose charges happen
  before the host is reached and so before any park;
  [ADR 0034](0034-one-physical-word-stack.md)'s rule that a `Value` exists
  only at the boundary, which is what makes a parked machine `Send` without
  touching the dispatch loop
- Supersedes: nothing. No accepted decision says a host call must answer
  before it returns; the code assumed it, and the assumption is what changes
- Decides: that a host may answer a call *pending*, where in a run that is
  allowed, what crosses threads while it waits and in what form, and the three
  thread affinities that had to end for a run to move at all

## Context

Issue #571 asks for Cove `Vm`s to behave like V8 isolates: thousands in one
process, driven by an embedder's M:N scheduler that parks a run while it waits
on a host call and resumes it on whichever worker thread is free. #570 made the
per-run half cheap — a `PreparedProgram` is one program's encoding for any
number of runs, and a run over one costs 5.7–36 µs and 14–80 KB.

What stood in the way, read from the code at `4d9c81f`:

- **`Vm` was `!Send` for one reason**: three raw pointers in `Tiering`, the
  native tier's per-run table (`vm/exec/native.rs`). None of them pointed at
  anything a thread owns — the read-only native table and the run's own heap
  chunks, which an `Arc<Space>` already sends.
- **A `Value` is not in the machine.** ADR 0034 put it at the boundary only —
  host arguments and answers, an entry's arguments and answer, callbacks — so a
  machine between two instructions holds words, and making a run movable needs
  no `Rc` turned into an `Arc` and no atomics on the hot path.
- **The obstacle was control flow.** A host call is a Rust call nested inside
  the dispatch loop and inside the `std::thread::scope` a run's `spawn`s start
  their children in, and `run_entry` and `invoke` run to completion. But
  `CALL_HOST` already syncs the program counter before it calls, so the frame
  stack and the destination slot very nearly describe the continuation already.
- **Four things genuinely belong to a thread**: the identity a `Shared` cell's
  lock word holds (`vm/cell.rs`'s thread-local tag); a live compiled frame,
  which is on the native stack; a running spawned task, which is a thread inside
  the run's scope; and a host running a Cove callback, which is a Rust frame
  below the loop.

The issue's six decision points were answered with defaults in its comment,
"each can be revisited in review of the ADR". This is that ADR.

## Decision

### 1. A host call may answer pending — when the host opts in

`HostApi` gains two methods, both defaulted:

```rust
fn call_parkable(&self, op: &str, args: Vec<Value>, back: &mut dyn Reentry) -> HostAnswer {
    HostAnswer::Ready(self.call_with(op, args, back))
}
fn call_resource_parkable(&self, handle: &ResourceHandle, op: &str,
                          args: Vec<Value>, back: &mut dyn Reentry) -> HostAnswer {
    HostAnswer::Ready(self.call_resource(handle, op, args, back))
}

pub enum HostAnswer {
    Ready(Result<Value, RuntimeError>),
    Pending(Box<dyn Any + Send>),
}
```

A host that never heard of this behaves exactly as before, and `call_with`,
`call` and `call_resource` are unchanged for every host that exists. The
boundary calls the parkable method **instead of** `call_with` exactly when the
run can park (§2), so a host that pends still answers `call_with` the ordinary
way — by waiting — and that is the path every call takes where parking is not
possible.

This shape was chosen over the alternatives below because it puts the one
decision only the runtime can make — *can this run be moved now?* — in the
runtime, before the host is asked, and leaves the host nothing to get wrong:
it is never handed a choice it cannot honour.

### 2. Pending is offered only where the run is quiescent; elsewhere the call blocks

A run is **quiescent** at a host call when nothing of it but that call is on
the thread:

| | why it pins the run to the thread |
|---|---|
| no host is running a Cove callback below the loop (`reentry_depth == 0`) | the host's Rust frame is on this stack |
| no compiled frame, and no `native::Session` caller, below the loop (`nested == 0`, counted in `drive_from`) | the same, for compiled code — it reaches the encoded tier no other way |
| no `Shared` cell held | the `lock` region ends only by running on |
| no spawned task un-joined | it is a thread inside the run's `thread::scope`, which ends when the loop unwinds |

and the run was started by one of `OwnedVm`'s parkable entries (§5). A run
that is not quiescent, or not parkable, calls `call_with`: **it blocks, as every
host call did before, and the program cannot tell the difference.** Refusing
instead was rejected for the reason the issue gives: a program must never fail
because of where it happened to make a call.

The check costs one `bool` and four comparisons per host call, at the call and
not in the loop. If a host that was offered the choice somehow left the run
non-quiescent before answering pending, the call is refused as a broken
invariant rather than unwinding a run that would cancel a task or abandon a
lock region (`Machine::suspend`).

### 3. Parking is the first half of the instruction; resuming is the second

When a parkable call answers pending, `call_host` (or `call_resource`) stores a
`Suspended` — the registry's pending call, the declared result layout, the
call's span, and when it began — and answers a marker error, so the
`CALL_HOST` arm leaves the dispatch loop through the failure exit it already
has; `drive` sees `suspended` and discards the marker. The pc is already synced
to the call, and the frames are the continuation. **The dispatch loop is not
changed at all**: a first version gave the arm a third outcome, a `return` for
the parked case, and that measured 1–2.5% slower on rows that make no host
call (`conv_local` 58.5–58.8 ms against 57.7–58.3 ms, covefmt 13.04 s against
12.71 s, identical instruction counts) — the loop's layout, not its work. `drive` finishes as it does for any answer (no
cells to give back, nothing running to stop, no pending fuel — ADR 0030 flushed
it before the call).

`Machine::resume` is the rest of the instruction: the parked time is added to
the run's host wait, the answer is settled by the registry, converted at the
declared layout, written at the destination the parked instruction names (read
back from its encoding), the pc advances, and the loop runs
on from there under a fresh thread scope. A refused answer fails the run at the
call, with the call chain the loop would have attached and the run's pending
fuel spent — the same exit as a run that raised.

The registry's `dispatch` is split rather than duplicated: everything before
the host is reached (grant, argument schema, budget charge, irreversible-write
count, argument recording) runs at the call; everything after (the `HostCall`
trace event and the result schema check) is `HostRegistry::settle`, run on the
answer when it arrives. A pending call therefore traces exactly one
`HostCall`, carrying its arguments and the answer it was resumed with, with a
`wait` from dispatch to resume — and since the run was quiescent, no other
event of the run can fall between the two. The trace of a parked run equals
the trace of the same run blocking, and replays the same way
(`tests/parking.rs` holds both).

### 4. What crosses threads, and in what form

**The request is the host's**: `Box<dyn Any + Send>`, moved by the runtime and
never read. The host and the embedder agree on its type; the runtime has
nothing to say about what a pending I/O operation is, which is the line
PHILOSOPHY's "separate policy from mechanism" draws.

**The answer is a `Transfer`**: `ParkedVm::resume(Result<Transfer,
RuntimeError>)`. `Transfer` is the `Send` form of a Cove value that already
exists for exactly this job — crossing from one thread to another — and
`Transfer::into_value` rebuilds the `Value` on the resuming thread, where it is
checked and converted at once. A non-task-safe resource handle a host wants to
answer with is a `Transfer::Resource` it builds directly.

Neither is ever a `Value`: a `Value` is `Rc`-based and belongs to the thread
that made it, and both of these exist to be handed to another.

### 5. The runtime provides an owning, `'static` run

`OwnedVm` holds a `Vm<'static>` beside the `Arc<Runtime>`, `Arc<HostRegistry>`
and `PreparedProgram` it borrows from, the `Vm` declared first so that it is
dropped first. The references are taken from inside the `Arc`s, whose contents
do not move when the handle does. It is not a `Deref` to the `Vm`: a `&mut
Vm<'static>` handed out could be swapped with another `OwnedVm`'s, after which
one would borrow the other's `Arc`s. The ways in are forwarded one by one.

Its blocking entries (`invoke`, `invoke_within`, `run_entry`) are the `Vm`'s.
Its parkable ones (`invoke_parkable`, `invoke_within_parkable`,
`run_entry_parkable`) take the run by value and answer a `Step`:

```rust
pub enum Step { Answered(OwnedVm, Result<Value, RuntimeError>), Parked(ParkedVm) }
```

`ParkedVm` is `Send` and offers `request`, `take_request` and `resume`. Both
`OwnedVm` and `ParkedVm` are asserted `Send` at compile time in the crate and
in the test.

The bundle is the runtime's rather than every embedder's because the unsafe
part — the self-reference — is exactly the kind of thing an embedder should not
have to write correctly, and because the invariants it rests on (which `Vm`
methods hand out a borrow) are the runtime's to keep.

### 6. Lock identity is the task's, not the thread's

A `Shared` cell's state word held a thread-local tag. With runs moving between
threads, two runs parked on one worker would have shared an identity, and one
run resumed elsewhere would have become a stranger to its own cell. Each
`Machine` — one task — now draws a tag from a process-wide counter when it is
built and keeps it; `cell::lock` and `cell::unlock` are handed it. It is not the
trace's task id because that is unique only within one `Runtime`, and a machine
with no runtime counts its own. (Quiescence already forbids parking with a cell
held; the tag is what makes that condition the *only* thing standing between a
moved run and its cells, rather than one of two.)

### 7. The native table is `Send + Sync`, with its invariants stated

`Tiered` gains `Send + Sync` supertraits: a table is read through `&self` and
answers function pointers into pages that are read-execute by then.
`cove_native`'s `Mapping` is `unsafe impl Send + Sync` — it owns its pages, every
write is through `&mut` before `Jit::finalize`, and nothing writes them after.
`Tiering` is `unsafe impl Send` (not `Sync`; it is one machine's): its table
pointers name a `Send + Sync` table its installer keeps alive, and its chunk
pointers name the run's own heap chunks, which an `Arc<Space>` owns and never
unmaps. What *is* the thread's — a live compiled frame — is excluded by §2, and
a running `Vm` cannot be moved at all, since running borrows it.

Letting a *spawned task* run compiled code needs the same bound and is not done
here.

### 8. Spawned tasks stay a thread each

ADR 0008's thread per spawned task is unchanged, and a run with a task that has
not been joined is not quiescent (§2). Green tasks are for when a program shows
the need.

## Consequences

- An embedder can hold a large number of in-flight runs on a small pool of
  threads, each costing its heap, its stack and its frames while it waits, and
  no thread. The measurement below is 1,000 parked runs of a three-call handler
  resumed by four workers.
- Every host that exists is unchanged, and so is every run that is not started
  parkably: a `Vm`, or an `OwnedVm`'s blocking entries, never offers a host the
  choice.
- A parked call is traced, charged and checked exactly as a blocking one, so
  `cove trace` and `cove replay` need nothing new: the tape of a parked run is
  the tape of the blocking run.
- The hot path is the host-call site and nothing else: the instruction loop is
  untouched, a host call tests one `bool` (`parking`, false for every run not
  started parkably, so `quiescent()` is not even reached), and a `drive_from` (a
  native-to-VM call or a session call) increments and decrements a counter.
  Measured below.
- `drive` and `drive_from` now open a thread scope with one empty handle per
  task the machine has already spawned, rather than none. A resumed run needs
  it — its earlier tasks are still in its table — and a machine invoked again
  after a run that spawned needed it too: a task spawned by the second
  invocation was pushed to index 0 of the new handle list while the task table
  gave it index *n*.

## Measured

**Hot path.** Base `4d9c81f` against this change, both built `--profile
checked` on one x86-64 macOS machine, run interleaved base/head for three
rounds: `cove-bench --matrix --iterations 5` (the VM rows, medians) and
covefmtBench on the VM (`cove run covefmtBench --files-root ../.. --backend vm
--stats`, `execute=`). Means of the three rounds:

| row | base | head | | instructions |
|---|---|---|---|---|
| `conv_local` | 57.82 ms | 56.53 ms | −2.2% | 12,285,732, identical |
| `conv_closure` | 127.73 ms | 126.85 ms | −0.7% | 16,285,734, identical |
| `conv_capture` | 151.14 ms | 141.20 ms | −6.6% | 16,285,736, identical |
| `conv_host` (2 M host calls, each running a callback) | 4,203.1 ms | 4,203.4 ms | 0.0% | 40,285,731, identical |
| covefmtBench, VM | 12.704 s | 12.576 s | −1.0% | 2,218,415,757, identical |

The falls are layout, not work — the instruction counts are equal and nothing
on those rows changed — and are read as "not slower". `conv_host` is the row
that exercises the host-call site, and it did not move.

**Parked runs.** `examples/rules/host/src/bin/parked.rs`: a handler making
three host calls with about 1,400 instructions of work after each, every call
answered pending, `n` runs started and parked on one thread and then driven by
an embedder-side queue and four worker threads, each resume on whichever worker
took it:

| | n = 1,000 | n = 10,000 |
|---|---|---|
| bytes one parked run retains (allocator) | 18,070 B | 18,070 B |
| the same by RSS | 18,223 B | 18,107 B |
| start to first park | 3.42 µs mean, 7.67 µs p99 | 3.57 µs, 11.16 µs |
| one resume (answer in, to the next park or the end) | 9.35 µs mean, 8.97 µs p50, 13.16 µs p99 | 9.47 µs, 8.85 µs, 27.20 µs |
| throughput | 343,000 resumes/s | 316,000 resumes/s |
| runs answering other than the blocking run | 0 | 0 |

The same run blocking takes 23.49 µs, so the three parks and resumes add about
8 µs, roughly 2.7 µs a park — the thread scope a resumed run opens again, and
a machine's state arriving cold in another core's cache. Every run answered the
blocking run's value in the blocking run's 4,266 instructions. (Queue wait is
reported by the binary too; it measures the example's burst of `n` runs queued
at once, not the runtime.)

## What this does not decide

- **A scheduler.** The runtime parks and resumes; when, where and in what
  order is the embedder's, and `parked.rs`'s queue is an example, not API.
- **Parking with a compiled frame live**, a debugger installed, or a native
  tier installed on an `OwnedVm`. `OwnedVm` has no native constructor yet; a
  run with a tier still parks only where no compiled frame is live, which the
  counter already says.
- **Cancelling a parked run.** Dropping a `ParkedVm` abandons it: its memory is
  freed, its host call is never answered, and the trace has its `entry_enter`
  and nothing after it. A run's cancellation flag and deadline are read at its
  next safepoint after resuming, as anywhere else; a deadline is wall-clock and
  keeps running while the run is parked.
- **Pending from inside a callback or beside a running task.** Those block
  (§2). Unwinding through a host frame or a thread scope would need a
  different control-flow design, and nothing has shown the need.

## Alternatives considered

- **Refuse a pending answer where the run cannot park.** Rejected in the issue:
  a program would fail because of where a call was made, which nothing in its
  source could show.
- **A `Pending` case in `call_with`'s answer, and the runtime blocks on it.**
  The runtime would then need a way to wait for an answer it cannot produce —
  a channel or a condvar in the runtime, and a cancellation poll around it —
  for the non-quiescent case. Calling `call_with` there instead uses the
  waiting the host already knows how to do.
- **Ask the host (`Reentry::can_park()`), and let it decide.** Every host that
  pends would re-implement the same branch, and one that got it wrong would
  answer pending to a run that cannot park.
- **The request as a `Transfer` of the arguments.** It would make the runtime
  describe the pending work, which is the host's to describe, and would refuse
  calls whose arguments are not task-safe for no reason.
- **A third outcome for the host call inside the loop.** Measured, as §3 says,
  and removed: the park now leaves through the failure exit.
- **Re-dispatch `CALL_HOST` on resume, with the answer preloaded.** Every line
  of the arm would be reused, but the instruction would be counted twice, and
  the fix — decrementing the counter — is the kind of compensation the
  instruction count, an observable, should not depend on.
- **A `Machine` over `Arc`s instead of borrows.** It would remove the
  self-reference, at the cost of a lifetime-free rewrite of every signature in
  the crate and reference-count traffic on every spawn, for one owner type.
