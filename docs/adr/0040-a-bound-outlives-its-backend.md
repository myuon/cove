# ADR 0040: A bound outlives its backend

- Status: Accepted
- Date: 2026-09-03
- Supersedes: [ADR 0024](0024-a-stop-is-a-bound-not-a-point.md)'s placement of
  the table of bounds — its first consequence, "`docs/VM_ARCHITECTURE.md`
  carries the table of bounds, and a change to any of the four constants
  changes a number there", and the implementation status that says the same
  document "states each bound in prose". Everything else in ADR 0024 stands,
  and this ADR exists to keep it: a stop stated as a bound rather than as a
  point, the bound a constant known before the run, two backends held to the
  same bound in their own units and not to the same schedule, what a stop may
  leave behind stated rather than minimized, and pending fuel never lost
- Supersedes nothing in
  [ADR 0030](0030-a-host-call-asks-the-fuel-limit.md). Its one sentence — "No
  Host call begins once the fuel a run has been charged has reached its
  limit" — holds on the backend
  [ADR 0034](0034-one-physical-word-stack.md) built, and is met *more tightly*
  than on either backend ADR 0030 was written against. The Decision below says
  by how much and shows the measurement
- Decides: where the bound each way of stopping a run promises is written
  down, and what those bounds are on the backend that runs Cove today
- Implementation status: complete. Nothing here is a change to the runtime.
  `crates/cove-runtime/tests/responsiveness.rs` measures every row of the
  table below on both evaluators and asserts each as a maximum — nineteen
  tests, all passing — and its module doc now points here. Line and figure
  citations are at this ADR's commit

## Context

ADR 0024 answered [issue #120](https://github.com/myuon/cove/issues/120) by
replacing a claim with a contract: for each way a run can be stopped, a
maximum for how much work may still happen and what may still be observed,
in each backend's own fuel, bounded by a constant fixed before the run. It
then had to say where the maximum was written down, and it chose a prose
document beside the backend:

> `docs/VM_ARCHITECTURE.md` carries the table of bounds, and a change to any
> of the four constants changes a number there. That is intended: the
> constants are the contract's arithmetic and they are public for that
> reason.
> — `0024-a-stop-is-a-bound-not-a-point.md:148-150`

Its implementation status says the same thing (`:28-30`), and its Context
leans on that document twice more (`:47`).

[ADR 0034](0034-one-physical-word-stack.md) then replaced the execution
backend outright rather than renovating it, and commit b094d82 — condition 8
— deleted the predecessor: the whole of the executable IR, `vm.rs`,
`frame.rs`, `slot.rs`, the `admits` mechanism and its coverage ratchet. Two
of ADR 0024's four constants went with it. `BACK_EDGE_FUEL` and
`SAFEPOINT_INTERVAL` do not exist in this tree; a grep for either finds
nothing. The mechanism they were the arithmetic of — a whole basic block
charged on arriving at its head, and a back edge checked only once enough
fuel had gathered — went with them too.

The document ADR 0024 made the carrier was kept rather than deleted with its
subject, because six accepted ADRs cite its sections by name and an accepted
ADR cannot be amended to point somewhere else. So from the cutover onward
ADR 0024's obligation went on resolving, and what it resolved to was a table
of a deleted machine's bounds, stated in the units of a charging scheme that
no longer existed, beside a backend that no longer ran anything. A reader who
follows a pointer like that is not stopped; they are answered, wrongly, which
is the hazard [ADR 0039](0039-a-name-in-an-adr-is-read-at-its-date.md) names
for names and which applies here to a number.

Repairing that document is a separate change and does not answer this
question. Whatever is left in it, a prose table beside a backend is the
arrangement that just failed, and putting a corrected one beside the
*replacement* would only schedule the same failure for the next replacement.

### What the prose did not catch, and what did

The deletion commit's own message is the evidence, and it is worth quoting at
length because it is the argument for this ADR:

> **Forty host calls ran under `fuel: Some(1)`.** The replacement charged fuel
> by calling the budget every 1024 instructions and nowhere else, so a run
> shorter than one stride charged **zero**, the instructions after the last
> stride were dropped with the stacks, and a host call was just another
> instruction on the count. ADR 0030 says, in its own words, *"No Host call
> begins once the fuel the run has been charged has reached its limit"* — an
> accepted ADR naming both backends, that the replacement was failing
> outright. `Limits::max_call_depth` was not implemented at all.
>
> So conditions 1–7 were not in fact met when they were reported met. What
> caught it was `responsiveness.rs`, the predecessor's contract suite, which
> would have been deleted with the predecessor had the order been reversed.

Three separate breaches of two accepted ADRs — ADR 0030's one sentence,
ADR 0024's "pending fuel is never lost", and a limit silently unenforced —
survived a corpus of 117 agreeing programs and a passing differential suite.
The table in `docs/VM_ARCHITECTURE.md` did not catch any of them and could
not have: it is prose, it is not run, and by then it described a different
machine. A test file did, and only because the deletion happened to be
ordered so that it outlived its subject.

The repair is in the same commit:

> Fuel is now charged for the instructions actually run — at the stride, at
> every host boundary before the call is dispatched, and once at the end of
> every thread's run. One measured result went the other way from expected:
> at the boundary with `fuel = standing + 1` the replacement performs exactly
> one host call where the interpreter and the predecessor both performed all
> forty, because they handed the same already-charged block over at each
> boundary inside it. **The replacement bounds effects by fuel more tightly
> than either backend ADR 0030 was written against.**

That leaves the contract satisfied and its statement homeless. This ADR is
where it lives.

## Decision

### A bound is stated in the ADR that decides it and measured by the test that measures it

Not in a prose document beside a backend. The table below is the table
ADR 0024 asks for, moved out of `docs/VM_ARCHITECTURE.md` and into the
record, and `crates/cove-runtime/tests/responsiveness.rs` is where every row
of it is measured rather than asserted.

The reason is the one the previous section demonstrates. A backend can be
replaced — ADR 0034 replaced one — and when it is, a document written beside
it describes the predecessor from the moment of the cutover, while the ADR
that promised a bound and the test that measures it both survive the machine
they were written for. `responsiveness.rs` has now outlived one backend
without being rewritten for it, and it caught what the prose could not.

### The unit: one fuel is one instruction, charged in arrears

`crates/cove-runtime/src/vm/exec.rs:428` holds `Machine::charged`, how many
of `Machine::instructions` have been handed to the run's `Meter`. Every place
that pays hands over exactly the difference and sets `charged` to what it paid
up to (`:895-899`, `:752-758`, `:805-812`), so a `Vm` run's `fuel_spent` and
the instructions it dispatched are the same number, and "the run is charged
for every instruction it dispatched" is a subtraction rather than a claim
about the paths somebody remembered.

That is a different arithmetic from the predecessor's, and the difference is
the whole shape of what follows. The predecessor charged a whole basic block's
extent on *arriving* at its head, before running any of it. This machine
charges for instructions already run, at a fixed stride, and never for
instructions it has not run.

Three places pay, and between them they cover every way work can be done:

- the periodic safepoint in `Machine::dispatch`
  (`exec.rs:879`), every `SAFEPOINT_STRIDE` instructions;
- `Machine::charge_at_host_boundary` (`exec.rs:805`), at
  `Inst::CallHost` (`:1903`) and `Inst::CallResource` (`:1987`), before the
  call is dispatched — ADR 0030's;
- `Machine::spend_pending_fuel` (`exec.rs:752`), called from
  `Machine::drive` (`:719`) at the end of a run or of a spawned task's
  thread, because every stop leaves through Rust's `?` rather than through an
  instruction and reaches no further safepoint — ADR 0024's "pending fuel is
  never lost".

### The constants

| constant | value | where |
| -------- | ----- | ----- |
| `SAFEPOINT_STRIDE` | 1024 instructions | `crates/cove-runtime/src/vm/exec.rs:66` |
| `DEADLINE_CHECK_INTERVAL` | 64 safepoints | `crates/cove-runtime/src/budget.rs:38` |
| `SAFEPOINT_FUEL` (the oracle's) | 10 fuel | `crates/cove-runtime/src/interp.rs:248` |

Two of ADR 0024's four are gone with the backend that had them, and no
constant replaced them one for one: `SAFEPOINT_STRIDE` is not
`SAFEPOINT_INTERVAL` renamed. `SAFEPOINT_INTERVAL` bounded the fuel standing
when a straight line was *entered*, so the work between two safepoints was
that plus one whole block extent, which is a quantity the program's shape
decides. `SAFEPOINT_STRIDE` bounds instructions directly, so the work between
two safepoints is 1024 instructions whatever they are, and no term of the
bound depends on how the program is shaped.

### The table of bounds

`S` is `SAFEPOINT_STRIDE`, in instructions, which on this backend is also
fuel. `T` is what one turn of the loop in question charges — a measured
figure, never a written-down one, because the bound has to stay true when the
lowering changes what a turn is made of. The same rows hold of the
tree-walking oracle with `SAFEPOINT_FUEL` where `S` stands and its own
schedule — calls, back edges and `await` — where the stride stands: it
gathers nothing, because it charges at every safepoint of its own and holds
no pending fuel. `responsiveness.rs` runs every case on both.

| stop | measured at, on `Vm` | maximum after it becomes true |
| ---- | -------------------- | ----------------------------- |
| the run's cancellation | `Meter::safepoint` (`budget.rs:268`), every `S` instructions; and every Host call, in `Budget::charge_host_call` (`budget.rs:484`) | `S + T` of Cove work; no Host effect |
| a task's own cancellation | `interp::stopped_here`, every `S` instructions (`exec.rs:888`); and before every Host call (`exec.rs:1902`, `:1983`) | `S + T` of Cove work; no Host effect |
| a bounded call's flag | the same two places, in the same order | `S + T` of Cove work; no Host effect |
| the deadline, with no fuel limit | every safepoint, because nothing else bounds the run | `S` |
| the deadline, beside a fuel limit | every `DEADLINE_CHECK_INTERVAL`th safepoint | `64 × (S + T)` |
| fuel | every safepoint, and `Machine::charge_at_host_boundary` at every Host call before dispatch | `S + T` of overspend; no Host effect; **and no refused prefix** |
| `max_host_calls` | `Budget::charge_host_call`, at every Host call, before it | nothing: the call that would pass it does not happen |
| `max_call_depth` | `Machine::admit_frame` (`exec.rs:1706`), at every frame pushed | nothing: the call that would pass it does not happen |
| the concurrency limit | `Budget::charge_task` (`budget.rs:520`), at every `spawn`, before the thread | nothing: the thread is not taken |

### What measures each row, and what it measured

Every figure below is from `crates/cove-runtime/tests/responsiveness.rs` at
this ADR's commit, on a debug build. The tests assert maxima; these are what
the maxima were reached with.

- **The run's cancellation** —
  `a_cancelled_run_stops_within_one_gathering_of_back_edge_fuel`. A loop of a
  hundred million turns, cancelled from inside a Host call. `T` measures 9 and
  the prefix 18, so the bound is 1,051; the run spends **1,024**, which is
  exactly one stride and the first safepoint it reaches.
- **A task's own cancellation** —
  `a_cancelled_task_stops_within_one_gathering_of_back_edge_fuel`, made
  deterministic with a two-way handshake rather than a sleep.
- **A bounded call's flag** —
  `a_bounded_call_stops_within_one_gathering_of_back_edge_fuel`. The one stop
  mode a program can observe from the inside, because the caller survives it.
- **The deadline with no fuel limit** —
  `an_expired_deadline_with_no_fuel_limit_stops_at_the_first_safepoint`:
  **1,024** against a bound of `S`. The oracle's figure is 10.
- **The deadline beside a fuel limit** —
  `an_expired_deadline_beside_a_fuel_limit_stops_within_the_clock_check_interval`:
  **65,536**, which is 64 strides exactly, against a bound of 66,123. The
  oracle's is 640 against 1,290.
- **Fuel** — `an_exhausted_fuel_budget_is_overspent_by_less_than_one_gathering`,
  at three limits. 1,000 / 5,000 / 20,000 are overspent by **24 / 120 / 480**,
  against a bound of 1,033. Every total is a multiple of the stride, which is
  what "charged in arrears at a fixed stride" looks like from outside.
- **No Host effect after a raised flag** —
  `no_host_effect_follows_a_cancelled_run`,
  `no_host_effect_follows_a_cancelled_task`, and
  `no_host_effect_follows_a_bounded_call_that_was_asked_to_stop`, each
  measuring the count at **0** by subtraction from an armed watermark rather
  than by narration.
- **`max_host_calls`** — `no_host_call_begins_once_the_charged_fuel_has_reached_its_limit`:
  a limit of 7 admits exactly 7 of forty effects, on both evaluators.
- **`max_call_depth` and the outcome of every stop** —
  `every_stop_mode_is_reported_as_itself_on_both_backends`, which runs all
  five and checks that each is reported as itself in the terminal trace event.
- **The concurrency limit** is the one row `responsiveness.rs` does not
  measure. `tests/e2e:fail_max_tasks` is the corpus program for it, run on
  this backend by `crates/cove-cli/tests/vm_coverage.rs`.
- **Pending fuel is never lost** —
  `a_run_never_reports_less_fuel_than_the_instructions_it_charged`, over seven
  ways of ending a run: a plain return, a failed `?`, a raised error, a
  cancelled run, an exhausted budget, a callback the host abandoned, and a
  cancelled task's own thread. The invariant is `fuel_spent >= instructions`,
  and it is the assertion `Machine::spend_pending_fuel` exists to satisfy.

### What the replacement's shape changed, and it is three things

**A straight line is cut off mid-line rather than refused whole.** This is the
sharpest observable difference ADR 0024 named, and it has moved. Four hundred
assignments in a row lower to 1,207 instructions. Under a fuel limit of 603
the machine stops for fuel having spent **1,024** — it ran 1,024 of the 1,207
and was cut off where the stride fell. The predecessor charged the whole
extent — 1,606 instructions in its own lowering of the same program, under a
limit of 803 — at the block's head and executed **none** of them. The oracle
answers `Ok(400)` under the same limit, because a straight line contains no
safepoint of its kind and it charges nothing for one.
`a_straight_line_is_cut_off_mid_line_and_the_tree_walk_finishes_it` measures
both halves. The rule ADR 0024 stated is untouched by the move — a run past
its budget stops, is reported as having stopped for fuel, and what it spends
past its limit is bounded. What went with the deleted backend is "the whole
extent is charged, including the part that never ran", which was always a
description of one mechanism and never a rule about Cove.

**The machine holds pending fuel by construction, and the oracle holds none.**
This is not an implementation detail; it is why the two satisfy ADR 0030 by
different means, which ADR 0030 explicitly allows. `Interpreter::charge_safepoint`
(`interp.rs:1418`) hands `SAFEPOINT_FUEL` to the shared budget in the same
call that charges it, so the oracle's charged total cannot move while a
straight line runs. This machine charges on a fixed instruction stride, so
between two safepoints it has always done work it has not paid for.

**The Host boundary is a charge point and not a safepoint, and that is what
makes ADR 0030's bound zero.** `Machine::charge_at_host_boundary` hands over
the pending fuel and asks the budget, and does nothing else: the two
thread-owned flags are read by the caller one line above, and the collector is
left out on purpose. Its own doc comment argues that at length under "Why this
is not a safepoint" (`exec.rs:784-804`), and the argument is not the
predecessor's — a collection there *would* be sound, because the arguments
were copied out of the heap by `boundary::to_value` and the words they came
from are still in a frame this machine has not left, and `Machine::park` is
about to publish exactly those roots. The reason to leave it out is that this
machine's collection point is a rendezvous poll, and putting one in front of
every Host call would make an unpredictable sweep part of the cost of reaching
the outside world for a reason the budget never asked for.

### ADR 0030 is met, and measurably more tightly than when it was written

Forty `probe.tick()` calls in one straight line, under `fuel: Some(1)`: the
run stops as `RunOutcome::Fuel` having spent **2** fuel, and performs **zero**
effects. The same program with no limit is 407 instructions and forty effects.
The boundary charged the two instructions that preceded the first call, the
budget refused, and the call was never dispatched.

The reentry case is the finding, and it goes the other way from what ADR 0030
expected. `a_host_call_inside_reentry_obeys_the_same_boundary` reads the fuel
standing at the first Host call of a callback off the one `max_host_calls`
limit that refuses exactly there — **5** on this machine, 20 on the oracle —
then runs the program at that limit and at one above it. At the limit: no
effect, and the boundary's own `irreversible_writes` tally agrees at 1, for
the `probe.bounded` call that got in. One fuel above it, this machine performs
exactly **one** effect and the oracle performs all **forty**, because the
oracle's charged total does not move while a straight line runs and this
machine's moves between every two calls. The predecessor sat with the oracle
here: it held a whole block's charge and handed the same already-charged block
over at each boundary inside it.

So the property ADR 0030 decided holds, and the quantity a fuel limit *admits*
past the boundary is smaller on this backend than on either of the two that
ADR 0030 was written against. ADR 0024's "a fuel limit is not portable between
backends" is what makes that a legal difference rather than a contradiction,
and it is untouched.

### ADR 0024's obligation, restated so that it survives a backend

ADR 0024 made a change to a constant a change to a document. That obligation
is replaced by this one:

**A change to a bound is a change to the ADR that states it and to the test
that measures it.** Because an accepted ADR is immutable, changing a bound
means writing the ADR that supersedes this one; and because a bound is only a
bound if it is measured, it also means the row in `responsiveness.rs` that
measures it. Neither alone is enough: an ADR without the test is a claim, and
a test without the ADR is a measurement nobody decided.

That is strictly more expensive than editing a table, and deliberately.
Tightening a constant costs nothing — the tests pass with more margin, which
is the shape ADR 0024 chose when it wrote every assertion as a maximum.
*Loosening* one is a change to what Cove promises a host about a run it cannot
otherwise stop, and it should cost a document.

## Consequences

- The table above is the table. ADR 0024's pointer at
  `docs/VM_ARCHITECTURE.md` is read at ADR 0024's date, under
  [ADR 0039](0039-a-name-in-an-adr-is-read-at-its-date.md)'s rule, and
  nothing here depends on what that document says now.
- `crates/cove-runtime/tests/responsiveness.rs`'s module doc said "`Vm`'s own
  bounds have no prose table yet — this file, and `docs/LINEAR_VM.md`'s
  design, are where they are measured and decided instead". That sentence was
  true when it was written and this ADR is what makes it false; the doc
  comment now points here. It is a source file and not an ADR, so it is
  edited rather than superseded.
- A third backend owes the same thing ADR 0024 said it owed: a stated maximum
  per stop mode in its own units, the same `RunOutcome` for the same stop, and
  the effect rules — plus a row in `responsiveness.rs` measuring each, which
  is what the file is built for. It does not owe this backend's numbers, and
  it does not owe holding pending fuel: ADR 0030 lets it satisfy the Host-call
  bound either way, and the two evaluators in this tree are one of each.
- An embedder's advice does not change. Fuel bounds work, the deadline bounds
  time, and `max_host_calls` is still the only control that bounds effects
  exactly, because how many Host calls a fuel limit admits is a property of
  the program and of the backend. What did change is the number in the worst
  case: it was 300 measured effects past exhaustion when ADR 0030 was written,
  it was zero at block granularity after it, and it is zero here with the
  standing charge measured at 2 fuel.
- Three doc comments in `responsiveness.rs` described the *found* faults of
  b094d82 rather than its repairs — that this backend "has no boundary
  hand-over", "has no `spend_pending_fuel` of its own", and that "nothing in
  `crate::vm` reads `Limits::max_call_depth`" — and each said the test below
  it fails. All three tests pass and all three claims are false of the code at
  `exec.rs:805`, `:752` and `:1707`. They are corrected in the same change as
  the module doc, because an ADR whose cited evidence denies itself is worse
  than no citation.

## What is not decided here

- **The values of the constants are not re-decided.** `SAFEPOINT_STRIDE` is
  1024 because a budget check reads an atomic and a clock and doing that per
  instruction would cost more than most instructions do; that trade is a
  performance question and its measurement is not in this ADR. What is decided
  is that the bound is *stated in terms of* the constant, so that a future
  change to it is a change to a stated bound rather than to a number in a
  file.
- **Whether a fuel limit should be portable between backends** is ADR 0024's,
  decided against, and untouched. The oracle and this machine still stop at
  different places for the same program under the same limit, and every
  figure above is stated per evaluator for exactly that reason.
- **Whether the Host boundary should become a collection point** is ADR 0030's
  and stays decided against, for a partly different reason, which
  `exec.rs:784-804` records where the code is.
- **What becomes of `docs/VM_ARCHITECTURE.md`** — repair, archive or deletion —
  is not decided here. ADR 0039 already says a separate change is fixing its
  pointers, and this ADR only stops depending on it.
- **Whether the oracle's bounds should move into an ADR of their own.** They
  are stated here beside this backend's because the tests measure both and a
  reader comparing two evaluators needs both in one place. If the oracle is
  ever the thing being replaced, that is when the question is worth asking.

## Alternatives considered

**Write a new prose table beside the new backend, in `docs/LINEAR_VM.md`.**
The smallest change: move the table one document over and repoint ADR 0024's
obligation. It is what was done last time, and last time is the evidence
against it. The document that carried the table died with the machine it was
written beside, and for the length of a cutover an accepted ADR's standing
obligation pointed at a table of a deleted mechanism's numbers with nothing
saying so. `docs/LINEAR_VM.md` is a *design* document — it argues for the
linear memory model, the one-heap-per-run ownership rule and the lowering of
closure-taking methods — and a design document is written before the thing and
is worth keeping as written. A contract has to be true after.

**Leave the bounds only in `responsiveness.rs`.** Tempting, because that file
is the thing that actually caught the breaches and it cannot go stale without
going red. It lost on the difference between measuring and deciding. A test
asserts `run.fuel_spent <= prefix + gathering(backend) + turn`; it does not
say that one stride plus one turn is what Cove *promises*, or why the deadline
has two rows, or that `max_host_calls` is the only control that bounds effects
exactly. A host deciding what limits to set on an untrusted program should not
have to read sixteen hundred lines of test to find out what a limit buys, and
a maintainer tightening a constant should not be able to loosen a promise
without writing anything down.

**Amend ADR 0024 to point somewhere else.** Forbidden by `CLAUDE.md`, and
wrong for the reason the convention gives. ADR 0024 was not mistaken: for the
backend it was written about, `docs/VM_ARCHITECTURE.md` really did carry the
table, and the four constants really were that contract's arithmetic. Editing
it would erase the fact that the project once believed a document beside the
backend was the right carrier — which is precisely what this ADR exists to
record having learned.

**Supersede ADR 0030 as well, since its evidence has moved.** Rejected, and
the distinction is the point of writing this down. ADR 0030's decision is one
sentence about a bound and it is *true* on this backend; only the mechanism
under it changed, from a flush of a pending block charge to a flush of a
pending stride, and the bound it promised got smaller rather than larger. A
decision that survives its implementation is not superseded by the
implementation changing — that is ADR 0039's argument for why ADR 0019 and
ADR 0022 survived the replacement of the machine they were about, applied
once more.

**Say nothing and let the next reader work it out from the tests.** The
failure mode is the one b094d82 found: three accepted decisions were being
violated by a backend that passed a 117-program corpus and a differential
suite, and what caught them was a test file that survived by an accident of
ordering. A contract that is only discoverable by running something is a
contract nobody checks before they change it.
