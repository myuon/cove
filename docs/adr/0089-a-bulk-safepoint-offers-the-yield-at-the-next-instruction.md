# ADR 0089: A bulk safepoint offers the yield at the next instruction

- Status: Accepted
- Date: 2026-10-06
- Refers to:
  [ADR 0084](0084-a-run-may-yield-at-a-safepoint.md), whose "the request is
  read only where a safepoint is already due" (§2) this narrows for the
  dispatch loop;
  [ADR 0086](0086-a-yield-request-makes-compiled-code-poll.md), whose §4 left
  the dispatch loop unchanged and the case to
  [issue #606](https://github.com/myuon/cove/issues/606);
  [ADR 0052](0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md),
  the chunked charge that takes a safepoint inside an instruction
- Supersedes: nothing. ADR 0084 and ADR 0086 are Proposed; this records what
  changes in them and why
- Decides: that a safepoint taken inside a bulk instruction, while a yield is
  wanted and the run could honour it, moves the dispatch loop's next question
  to the next instruction and lowers the slow path's stride to nought there,
  so the yield is offered between the two instructions; and that a yield taken
  there puts the schedule back where the uninterrupted run has it

## Context

ADR 0084 §2 has the dispatch loop read a yield request only inside its one
branch, where a stride of work has gathered since the last charge. A loop
whose every turn makes a bulk charge — `Vector.snapshot`, `Array.toVector`,
a large `RunCopy`, a long `String` search — takes its safepoints *inside*
the instruction (`encoded::in_chunks`, `encoded::find`), and each of those
resets `charged_work`. The loop's own stride test then never finds a stride
gathered, so the request is never read and the run never yields. Under
`native_yielding.rs`'s storm of requests on the encoded tier, `snapshotting`
and `regrowing` yielded nought times, through 67,152 and 101,907 of the
monitor's requests.

ADR 0086 §4 tried polling early in the dispatch loop — an `else` arm on the
stride test, inline and out of line — and measured 1.5–6.9% on rows that
never ask. The fast path is what every instruction pays, so the loop was left
alone.

## Decision

### 1. The bulk path lowers the threshold, the loop is unchanged

After a safepoint inside an instruction, the bulk path calls
`Machine::after_bulk_safepoint` in place of recomputing `next_check`. It
recomputes it as before and, **only when a yield is wanted** (requested, not
declined this stride, not just resumed) **and the run is yieldable**
(ADR 0084 §3's conditions), sets `next_check` to the next instruction and the
machine's `stride` to nought. That is out of line and runs once a stride, in a
path that has just taken a safepoint.

The loop's fast path — one increment, one comparison against `next_check` —
is untouched. Its slow path compares the work since the last charge against
`machine.stride` where it compared against the constant `SAFEPOINT_STRIDE`.
`stride` is `SAFEPOINT_STRIDE` except for that one instruction, so with no
request the loop asks the same questions at the same counts.

At the next instruction the slow path finds `0 >= 0`, reads the request, and
`offer_yield` yields, standing before that instruction with the count taken
back as ADR 0084 §4 has it.

### 2. A yield taken there takes no safepoint the uninterrupted run did not

ADR 0084 §4's resumed run takes the safepoint it yielded at. Here that
safepoint has already been taken, by the bulk operation. So `offer_yield`,
finding `stride` nought, restores it and puts `next_check` back a stride past
that charge before it yields: the resumed run goes on exactly as the
uninterrupted run, with the same safepoints, charges, fuel stop and
collections.

Gating on yieldability is what keeps that true. A lowered slow path whose
offer declined would go on to take a safepoint the uninterrupted run does
not, and a fuel limit could stop it at a different instruction. Between the
bulk operation's safepoint and the next instruction's check nothing can
change the answer: the instruction finishes, and the request is lowered only
by a yield.

`Machine::safepoint` restores `stride`, so nothing leaves it nought past the
next safepoint, whichever tier takes it.

### 3. What ADR 0084 §2 now says for the dispatch loop

The request is read where a safepoint is due, **and at the instruction after
a bulk operation took the due safepoint inside itself while a yield was
wanted**. The bulk operations still do not yield inside an instruction.

## Consequences

- A VM loop of bulk operations is sliced as a loop of ordinary instructions
  is: `yielding.rs`'s `snapshotting` and `regrowing` yield once for every one
  of twenty requests (nought before), in the same count, for the same fuel,
  and a fuel limit stops them where it stops the uninterrupted run. Under the
  storm, `snapshotting` and `regrowing` yield thousands of times, going on
  through at most a few hundred requests between yields — the same as every
  other shape.
- `native_yielding.rs`'s encoded storm now asserts that every shape yields and
  none runs long without yielding.
- A loop whose every turn makes a **host call** resets `charged_work` the same
  way, at `charge_at_host_boundary`, and is not covered here; the storm's host
  shapes (`waiting`, `timing`) yield because their turns cross strides between
  calls.

## Measured

On the same x86-64 macOS machine, 2026-10-06, base `2ca1c94` against this
change, release builds, three interleaved rounds (base, head, base, …),
medians of the per-round medians. Another agent's builds shared the machine:
load average 14 at the first round's start, 2.1–2.9 for the rest.

| row | base | head | |
|---|---:|---:|---:|
| `arith`, VM | 46.50 ms | 46.78 ms | +0.6% |
| `call`, VM | 52.15 ms | 52.66 ms | +1.0% |
| `field`, VM | 47.14 ms | 47.28 ms | +0.3% |
| `method`, VM | 58.85 ms | 58.19 ms | −1.1% |
| `arrayget`, VM | 124.4 ms | 125.5 ms | +0.9% |
| `chars`, VM | 428.8 ms | 429.7 ms | +0.2% |
| `callback`, VM | 173.7 ms | 172.3 ms | −0.8% |
| `crunch n=20000`, `Vm` (`cove-edge-compare`) | 4,099 µs | 4,076 µs | −0.6% |
| `crunch n=150000`, `Vm` | 64.40 ms | 64.24 ms | −0.2% |
| `crunch n=20000`, edge path | 4,079 µs | 4,055 µs | −0.6% |
| covefmtBench VM, `whole` | 993 ms | 973 ms | −2.0% |

Every difference is inside the base's own spread across its three rounds
(1.3–6.1% per row). Instruction counts are identical on every deterministic
row.

Yield latency under `native_yielding.rs`'s encoded storm (a request raised
with no pause, the most requests the monitor raised between two yields):

| shape | base | head |
|---|---:|---:|
| `snapshotting` | 0 yields, 67,152 unheeded | 3,705 yields, 224 |
| `regrowing` | 0 yields, 101,907 unheeded | 2,361 yields, 225 |
| `copying`, `building`, `main` (for scale) | 246 / 246 / 145 | 263 / 243 / 158 |

## Alternatives considered

- **Poll early in the dispatch loop** (ADR 0086 §4). Measured to cost rows
  that never ask.
- **Defer the bulk operation's last safepoint to the loop.** Covers a copy of
  one chunk, not one of many: its due safepoints fall on chunk boundaries
  inside the copy, and deferring those would let a cancelled run copy the
  rest — ADR 0040's `S + T` plus the copy's length.
- **Yield inside the bulk operation, at the end of its copy.** The
  instruction's result is not yet written; each bulk arm would need its own
  resume point.
- **Pre-charge and leave `charged_work` behind.** Makes the stride test pass
  without a new field, but every reader of `charged_work` — the host boundary,
  the end of a run, the native poll budget — would need the correction.
