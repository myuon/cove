# ADR 0030: A Host call asks the fuel limit

- Status: Accepted
- Date: 2026-08-31
- Supersedes: [ADR 0024](0024-a-stop-is-a-bound-not-a-point.md)'s decision
  "A Host call is a stop point for every flag and for no budget", in the one
  clause that exempts fuel — "**Fuel is not**, and deliberately" — and in the
  bound that clause states, "the bound on Host effects under an exhausted fuel
  budget is one straight line's worth of them". Everything else in ADR 0024
  stands and this ADR leans on all of it: a stop stated as a bound rather than
  as a point, the two backends held to the same bound in their own units and
  not to the same schedule, what a stop may leave behind, and pending fuel
  never lost
- Implements: [issue #160](https://github.com/myuon/cove/issues/160)

## Context

ADR 0024 wrote down where each runtime control is measured and what may still
happen after it becomes true. For a Host call it decided that all three stop
*flags* are read before the call — so no Host effect follows a raised flag,
which is one of the three faults [issue #120](https://github.com/myuon/cove/issues/120)
found — and that of the three *budgets*, the deadline and `max_host_calls` are
measured at every call and fuel is not.

Its reason for the exemption was cost, in one sentence:

> measuring fuel means spending what a backend has charged and not yet handed
> over, and not doing that per instruction is the whole of why block charging
> exists.

That was true of the runtime ADR 0024 was written for. Spending meant reaching
the run's `Budget` through `HostRegistry::with_budget`, which locked a mutex
every task of the run shares — the mutex
[issue #182](https://github.com/myuon/cove/issues/182) then measured at 36% of
`benches/call`, because every call and every return is a safepoint.

Issue #160 was opened to measure the exemption rather than keep arguing it,
and the measuring is what changed the answer.

**The semantic half.** A straight line of 300 `clock.now()` calls in one basic
block makes all 300 after the fuel limit is logically exhausted, at *every*
fuel limit tried — 600, 2,700, 9,900, 48,900 — because what bounds it is the
extent of the block the run happened to be in and not the limit. In a loop the
same measurement prevents one to six calls, because `BACK_EDGE_FUEL` already
bounds that case. `fuel_spent` is identical either way: the fuel is charged at
the block head regardless and spent at the end regardless, and the only thing
that moves is where the run stops. So the quantity ADR 0024 called "one
straight line's worth" is real, is not small, and is fixed by the shape of the
program rather than by any constant of the runtime — while ADR 0024's own rule
for a schedule is that "what it gathers between two checks must be bounded by
a constant fixed before the run".

**The cost half.** Issue #160 built the variant, benchmarked it bracketed
against two unmodified runs, and could not resolve it: under 50 ns against a
Host call costing 640 to 890, with the two baselines disagreeing by more than
the effect, inside the ±6% layout band `docs/VM_ARCHITECTURE.md` now documents.
It left a design note — fold the flush into `Budget::charge_host_call` so it
reuses the lock the boundary already takes, and the cost becomes free by
construction rather than smaller than the band.

**That note was written against a tree that no longer exists.** PR #191 closed
#182 after the measurement was taken, and it removed the run's mutex from the
safepoint: a run's accounting is an `Arc<Accounting>` of atomics, `Meter` is
the `&self` view a safepoint charges through, and both backends take one where
a run begins. The flush therefore takes no lock at all now — a `mem::take` and
a relaxed `fetch_add` — so there is nothing left to fold, and folding it into
`charge_host_call` would in fact move it *behind* `HostRegistry::with_budget`'s
lock, which that path still takes. The variant #160 measured was strictly more
expensive than the one this ADR decides on: it took a second mutex acquisition
per Host call and was still under 50 ns.

Both halves of ADR 0024's trade therefore moved. The bound is larger than the
words "one straight line's worth" suggest, and the cost the exemption bought
is gone.

## Decision

### No Host call begins once the fuel a run has been charged has reached its limit

One sentence, and it holds on both backends, because it is about the bound and
not about the count. ADR 0024 established that a fuel limit is not portable
between backends and this ADR does not disturb that; what the two now share is
the property, not the number that satisfies it.

**On the VM** it holds because `Vm::charge_at_host_boundary` runs at
`Inst::CallHost` and `Inst::CallResource`, before the call is dispatched: it
hands the fuel charged since the last safepoint to the run's `Meter` and asks
whether the run may continue. The bound on Host effects after logical
exhaustion goes from `SAFEPOINT_INTERVAL` of standing fuel plus one block
extent, divided by what a Host call charges, to **zero**.

**On the AST interpreter** it holds already, and there is nothing to add.
`Interpreter::charge_safepoint` hands `SAFEPOINT_FUEL` to the shared budget in
the same call that charges it, so that backend has no pending fuel and its
charged total cannot move while a straight line runs. A safepoint that reaches
the limit stops the run at that safepoint; nothing after it is dispatched.

### What the two backends still do not share is what a limit admits

A straight line of Host calls charges the VM about two fuel each and charges
the tree walk nothing at all, so the same limit lets very different numbers of
effects happen *before* exhaustion. That is ADR 0024's accepted cost — "a fuel
limit is not portable between backends" — and it is untouched. A statement
about how many effects a fuel limit permits is still a statement about one
backend and one program.

### Fuel still does not bound effects, and `max_host_calls` still does

The three controls keep their three meanings: fuel bounds work, the deadline
bounds time, `max_host_calls` bounds effects. This ADR narrows one gap and
claims nothing beyond it. How many Host calls a fuel limit admits still
depends on what the program does between them, so an embedder that wants a
number sets `max_host_calls`, and the documentation still says so where an
embedder reads it.

### A Host call is still not a collection point

`Vm::charge_at_host_boundary` is `Vm::safepoint` without `collect_if_due` and
without the two thread-owned flags, which the caller has already read. The
arguments have been drained into a `Vec<Value>` by then and the receiver of a
resource call popped, so a collection there would be sound — those values are
rooted by their own references, as `Vm::collect` documents for the callback
case — but it would put an unpredictable sweep in front of every Host call for
a reason the budget never asked for.

### The four constants do not move

`BACK_EDGE_FUEL`, `SAFEPOINT_INTERVAL`, `SAFEPOINT_FUEL` and
`DEADLINE_CHECK_INTERVAL` are unchanged. This adds a place where the budget is
asked; it does not change what any schedule gathers.

## Consequences

- A VM run under a fuel limit may now stop *inside* a straight line, at a Host
  call, where before it was stopped only at the head of a block or at its end.
  It can only stop earlier than it did, never later, and `fuel_spent` for a
  given program and limit does not change: the fuel was charged at the block
  head either way.
- The `fuel` row of `docs/VM_ARCHITECTURE.md`'s table of bounds changes, and
  its paragraph "A Host call is not a safepoint, and it is not meant to become
  one" is now wrong about fuel and right about everything else. Both are
  rewritten there rather than here.
- `crates/cove-runtime/tests/responsiveness.rs` carried the old decision as an
  assertion — forty Host calls in one straight line all happening under a fuel
  limit of one — and now carries this one. Host reentry is covered by a second
  test: a callback the host re-enters Cove with runs on the same backend state
  with the same pending fuel, so a Host call inside it meets the same
  boundary, and the test measures the exact limit at which the first effect of
  a reentry is refused.
- A third backend owes this property and is free to satisfy it either way: by
  flushing at the boundary, as the VM does, or by holding no pending fuel at
  all, as the tree walk does.
- The cost is argued from the code rather than measured again. The flush takes
  no lock, on a boundary that already takes one for `Budget::charge_host_call`,
  reads the clock inside it for the deadline, and reads it twice more to time
  the wait; and #160's measurement bounded a strictly more expensive version of
  it at under 50 ns. `docs/VM_ARCHITECTURE.md` says a row's error bar is about
  0.8% and that the lowering rows are not evidence under ~10%, so a benchmark
  that could resolve this does not exist in this tree. That is stated rather
  than papered over with a number.
