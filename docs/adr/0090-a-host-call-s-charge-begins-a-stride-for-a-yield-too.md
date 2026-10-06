# ADR 0090: A host call's charge begins a stride, for a yield too

- Status: Proposed
- Date: 2026-10-06
- Refers to:
  [ADR 0089](0089-a-bulk-safepoint-offers-the-yield-at-the-next-instruction.md),
  whose offer at the next instruction this applies after a host call;
  [ADR 0086](0086-a-yield-request-makes-compiled-code-poll.md), whose
  `just_resumed` (§2) and `declined_this_stride` (§3) a safepoint lowers and
  now a host call's charge lowers too;
  [ADR 0084](0084-a-run-may-yield-at-a-safepoint.md) §2;
  [ADR 0030](0030-a-host-call-asks-the-fuel-limit.md), the charge at a host
  boundary;
  [issue #618](https://github.com/myuon/cove/issues/618)
- Supersedes: [ADR 0086](0086-a-yield-request-makes-compiled-code-poll.md)'s "a run resumed from a yield
  takes the safepoint it stood before before it may yield again" (§2), in
  part: a host call's charge now also lowers `just_resumed`, so a resumed run
  may yield again after a host call without having taken that safepoint
- Decides: that a host call's charge lowers the two flags a safepoint lowers;
  and that on the dispatch loop, after a host call returns, a yield that is
  wanted and can be honoured is offered at the next instruction once a stride
  of work has passed since the last safepoint, by ADR 0089's mechanism

## Context

ADR 0030 has a host call charge the run's unpaid work at its boundary
(`Machine::charge_at_host_boundary`), and that moves `charged_work` as a
safepoint does. So a loop that calls the host every turn, with turns shorter
than a stride, never gathers a stride between two charges:

- **On the dispatch loop** the stride test never finds a safepoint due, so the
  request is never read (ADR 0084 §2) — the bulk-charge shape of #606 with a
  host call in place of the copy. A loop of 3,000 `tick` calls, each raising
  the request, yielded nought times.
- **In compiled code** ADR 0086 §3's early poll finds the request after each
  call and yields at the next backedge, once. Resumed, the run has
  `just_resumed` set until it takes a safepoint (ADR 0086 §2) — and such a
  loop takes none, so it never yields again. The same loop yielded once.

## Decision

### 1. A host call's charge lowers `just_resumed` and `declined_this_stride`

`charge_at_host_boundary` lowers both, as `Machine::safepoint` does. A run
that reached a host call has done something since it was resumed, which is
what `just_resumed` waits for; and the stride `declined_this_stride` covers is
the one this charge ends. Compiled code's threshold is republished after the
call (`native_poll_at`), so in the loop above compiled code yields at the
backedge after each call that finds a request.

A run that cannot yield under a raised request (below a callback, a running
task) now pays one early poll per host call rather than one per stride, and
only while the request is raised.

### 2. The dispatch loop offers the yield after a host call, a stride on

After a host call returns its answer, the encoded `CALL_HOST` and
`CALL_RESOURCE` arms call `Machine::after_host_call`. It does what ADR 0089's
`after_bulk_safepoint` does — sets `next_check` to the next instruction and
the slow path's stride to nought — when a yield is wanted, the run can yield,
**and a stride of work has passed since the last safepoint**
(`Machine::safepoint_work`, set by every safepoint and by a yield taken this
way). That is where the same loop without its host calls would have been
asked, so a run is not made to leave at the first host call after a request
when its stride is not yet over; the nudge-then-compute shapes of
`native_yielding.rs` still yield inside compiled code, where they did.

The offer is made after the call rather than at its boundary because the call
may be what raised the request. `offer_yield` restores the schedule as ADR
0089 §2 has it and also starts the next stride at the yield.

The dispatch loop's fast path and slow path are unchanged by this; the host
arms gain one inlined test, behind `parking`.

## Consequences

- A loop of host calls is sliced on both tiers: under `prod` (a host call that
  raises the request) 3,000 turns yield on compiled code at nearly every turn
  and on the dispatch loop once a stride; under the storm the encoded `ticking`
  shape yields once a stride of work.
- The dispatch loop's latency for such a loop is a stride of *work*, and a
  host call is one unit of work and many instructions' time, so measured in
  wall time or in requests it is longer than for a loop of instructions. The
  native tier's is a backedge.
- `yielding.rs` gains an end-to-end test of the restore: a fuel limit one past
  the charge a run yielded after stops the yielded run where it stops the
  uninterrupted one, which fails if the resumed run charges there again.

## Measured

On the same x86-64 macOS machine, 2026-10-06, base `bfd3028` against this
change, release builds, three interleaved rounds, medians of the per-round
medians, load average 2.0–4.2 (another agent was building on the machine).

| row | base | head | |
|---|---:|---:|---:|
| `hostheavy`, VM | 5.847 ms | 5.745 ms | −1.7% |
| `conv_host` (`--matrix`), VM | 3,841 ms | 3,812 ms | −0.8% |
| `arith`, VM | 46.81 ms | 46.88 ms | +0.2% |
| `call`, VM | 52.83 ms | 52.47 ms | −0.7% |
| `chars`, VM | 424.0 ms | 428.4 ms | +1.0% |
| `callback`, VM | 172.3 ms | 170.1 ms | −1.3% |
| covefmtBench VM / native, `whole` | 988 / 280 ms | 988 / 278 ms | 0.0% / −0.7% |

Instruction counts are identical on every deterministic row. `chars` and
`callback` make no host call; their ±1% is the layout noise of a change to
the dispatch loop's text (ADR 0089 measured `chars` across 427.7–440.3 ms on
the base alone).

Yields, before and after:

| shape | base | head |
|---|---:|---:|
| `prodding` (3,000 host calls, each raising the request), compiled | 1 | ≥ 1,500 asserted |
| `prodding`, encoded | 0 | ≥ one per two strides asserted |
| `ticking` under the encoded storm, no pause | 0 | 350 over 360,009 work |
| `ticking` under the native storm (≤ 20 µs pauses) | 1 | 5,915 |
