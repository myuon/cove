# ADR 0060: A backedge tests the stride before it calls

- Status: Accepted
- Date: 2026-09-16
- Supersedes:
  [ADR 0055](0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md)'s
  **"Safepoints occur at least: on loop backedges"**, read as an unconditional
  safepoint at every backedge. A backedge is now a *poll*: compiled code tests
  the machine's stride and enters the safepoint helper only when it has been
  reached. The other four entries in that list are untouched, and so is the
  sentence beneath them — "No compiled interval may exceed the backend's
  stated `T`" — which is what the list exists to buy and which this ADR keeps
  exactly
- Preserves:
  [ADR 0040](0040-a-bound-outlives-its-backend.md)'s table of bounds, in its
  own units and unchanged. The work between two polls in compiled code is at
  most `S + T`: one `SAFEPOINT_STRIDE`, and one turn of the loop that reached
  it. That is the row the encoded tier already has
- Decides: where the stride test lives, and what the compiled tier is told
  about it

## Context

[Issue #398](https://github.com/myuon/cove/issues/398) profiled `covefmt`'s
print phase on the native tier over a fixed 248-file corpus and found that
**22.0% of it — 37.1 ms of 168.5 — was safepoints**, the largest single
addressable cost in the phase and larger than call plumbing (14.8%) or
allocation (8.8%).

Not safepoints *taken*. Safepoints *asked for*:

> The native tier emits a safepoint **on every loop backedge unconditionally** —
> `SAFEPOINT_STRIDE` is tested *inside* the helper. That is the whole 22%.

The count is exact, by difference between two scratch entry points:
`safepoint` was entered **1,845,706 times** in the print phase, at ≈20 ns a
call. `SAFEPOINT_STRIDE` is 1024 units of work, and a turn of a `covefmt` loop
is a handful, so all but roughly one call in two hundred crossed the C ABI,
loaded the machine, added the work, and was told that nothing was due.

The encoded tier has never done this. `encoded::dispatch` tests
`work() - charged_work >= SAFEPOINT_STRIDE` in its own loop and calls
`Machine::safepoint` only when that is true; the compare is one of the two
instructions ADR 0052 says the dispatch loop must keep. The compiled tier was
paying a function call for the same compare, and the reason it was is that the
two tiers were built a year apart and the compiled one started with the
simplest thing that was correct.

## Decision

### A backedge emits a poll, and the poll is a compare

Both code generators emit, in front of the hand-over:

```text
cmp  <work>, [ctx + poll_at]      ; the accumulator against the threshold
jb   through                      ; below it, nothing is due
```

and the call, the stop test and the accumulator's reset stay exactly where
they were on the other side of that branch. The template arm emits those two
instructions literally; the Cranelift arm emits a load, an `icmp` and a `brif`
and its x64 backend folds the load into the compare.

Nothing about what a safepoint *is* changes. The helper is still where
ADR 0040's three steps live, in ADR 0040's order, and the frame is still
canonical at every one of them: the accumulator that decides whether to call is
the same statically accumulated block count the helper was always handed, and a
poll that does not call did not reach a point where anything could have been
stale.

### The threshold is published by the runtime, not compiled in

`NativeCtx` gains one field, `poll_at`, and it is what the compare reads. The
runtime writes it, and what it writes is **what is left of the stride**:

```rust
SAFEPOINT_STRIDE.saturating_sub(machine.work() - machine.charged_work)
```

Two reasons, and the second is the load-bearing one.

`cove-native` does not depend on `cove-runtime` — that inversion is what the
whole ABI exists to buy — so the code generators cannot name
`SAFEPOINT_STRIDE`, which is `cove-runtime`'s contract arithmetic and public
for exactly that reason.

And a compiled call is *entered with work already done*. The encoded tier
charges in arrears at its own stride, so a VM-to-native call can begin with up
to a stride of uncharged instructions behind it. Had compiled code counted
against a constant 1024 of its own, the interval between two polls across that
boundary would have been up to `2S + T` — a bound ADR 0040 does not state, in
a program that merely happened to cross tiers. Publishing the remainder makes
the compiled test *the same test* the dispatch loop makes, in the one
coordinate compiled code keeps, and the poll lands at the same coordinate of
`Machine::work()` either way.

Nought therefore means "poll at every backedge", because the accumulator is
never negative. That is what `NativeCtx::new` leaves and what a harness that
publishes nothing gets: forgetting this field costs a poll that was not needed
rather than skipping one that was.

### Three places publish it, and the third is the interesting one

- **`enter`**, when a `NativeCtx` is built for a call — with the remainder, as
  above, and not with a fresh stride;
- **`republish`**, which every helper that charges what it was handed and takes
  a safepoint already calls, because after one the machine is charged up to
  date and a whole stride is available again. A bulk helper that charged
  proportionally for a megabyte may leave *none*, and then the threshold it
  publishes is nought and the next backedge polls — which is how bulk work
  still forces a poll;
- **`close`**, the far half of a direct native-to-native call, which charges
  the callee's last unpaid block **without** a safepoint. It is the one charge
  point that is deliberately not a poll, so it is the one place where a stale
  threshold would promise a stride that had already been spent.

The same reasoning found one thing that was already wrong and is repaired here:
`enter` charged a returning callee's pending work into `Machine::bulk_work` and
did not recompute `Machine::next_check`, which is in *instruction* coordinates
and therefore does not move when work is charged in bulk. `encoded::in_chunks`
has had that line since ADR 0052 for the same reason. Before this ADR the error
was bounded by a compiled function's straight-line run; after it a callee may
leave a whole stride behind, so the line is no longer optional.

### What the bound is, stated

Between two polls, compiled code does at most `poll_at` units of work plus the
blocks of the turn that crossed it — `S + T`, where `T` is one turn, which is
ADR 0040's row for every stop mode and the same shape as the encoded tier's.
Cancellation, a task's own stop, a bounded call's flag, fuel and the
collector's rendezvous are all observed at the poll that takes the safepoint,
so all of them move from "within a turn of the loop" to "within `S + T`" — up
to the stated maximum, never past it.

This is the one thing that is genuinely *less* prompt than before, and it
should be said plainly rather than buried: compiled code used to be far tighter
than its stated bound, and it now uses the bound it was given. The deadline
beside a fuel limit is the widest row — `64 × (S + T)`, because
`DEADLINE_CHECK_INTERVAL` counts safepoints and compiled code now takes ~200×
fewer of them per unit work — and `64 × (S + T)` is what ADR 0040 promised and
what `responsiveness.rs` asserts.

### What it measured

The corpus is issue #398's, fixed: a checkout of `fa31182`, 248 files,
698,481 bytes, the same `--files-root` for every arm. Template arm, x86-64
macOS, thirteen interleaved rounds with the first dropped, **paired within each
round** so that the difference is not a difference of medians.

| row (native tier) | base | this | delta | slower in |
| --- | ---: | ---: | ---: | ---: |
| covefmt `lex` | 31.0 ms | 18.5 ms | **−12.0** | 0/12 |
| covefmt `parse` | 74.5 ms | 69.0 ms | −6.5 | 0/12 |
| covefmt **`print`** | 168.5 ms | 140.5 ms | **−26.5** | 0/12 |
| covefmt `whole` | 274.0 ms | 227.5 ms | **−46.0** | 0/12 |
| covefmt `execute=` | 2,205.8 ms | 1,811.3 ms | −392.5 | 0/12 |
| `benches/keyed` | 1,719.8 ms | 1,414.3 ms | −307.0 | 0/12 |
| `benches/seqsearch` | 1,152.4 ms | 944.5 ms | −206.7 | 0/12 |
| `cq revenue-summary`, 20,000 records | 1,475.0 ms | 1,416.3 ms | −52.8 | 1/12 |
| `benches/arith` (layout control) | 48.3 ms | 47.9 ms | −0.4 | 0/12 |

The census says where it went, by difference between two scratch entry points
exactly as #398 took it. In the print phase the `safepoint` helper was entered
**1,845,706** times — reproduced to the call — and is now entered **4,389**.
Every other helper count and all four crossing counts are identical between the
arms, which is what says this changed when the runtime is asked and nothing
else: `open` 859,373, `close` 847,255, `growable` 126,361, `alloc` 109,128,
`run_copy` 65,459, `intrinsic` 64,165, and print's four crossings — 0 VM-to-VM,
12,349 VM-to-native, 12,118 native-to-VM, 847,255 direct — each the same on
both. Over the whole run, `safepoint` falls from 24,249,440 to 69,408.

Compiled code grew by **2,574 bytes** over 194 compiled functions — ten bytes a
backedge, of which there are 257 in this program.

**The gain is 26.5 ms of the 37.1 ms the sample profile predicted, and the
difference is attribution rather than residual work.** 1,841,317 calls went
away and 4,389 remain, so what is left of the helper is under 0.1 ms; the two
emitted instructions run 1.85 million times and cost a millisecond or two. What
a sampled, differenced profile attributes to a helper's frame includes costs
that do not leave with the call — the spills around it, the branch it ends in —
and 22.0% was the upper bound on what removing it could buy. 15.7% of the phase
is what it bought.

**One number went the wrong way and it is worth stating plainly: the encoded
tier measured 2.8% slower** on the same corpus (`whole` 551.5 → 568.5 ms,
12/12). The VM executes **none** of this change — with no tier installed there
is no `NativeCtx`, no helper and no compiled code — so a third binary was built
to find out: the runtime and ABI half of this change with *base's* code
generators, which emits the old unconditional backedge and runs the new Rust.
That arm is neutral (`whole` +5.0 ms, `execute` +1.0%), its
`encoded::dispatch` is **the same instruction sequence** as this branch's —
7,621 instructions, compared mnemonic for mnemonic — and `benches/arith`, also
VM-dispatched, is *faster* on this branch. What moved is where the code sits:
the binary's `__text` grew 480 bytes and the dispatch loop landed on a
different alignment. It is the layout sensitivity `docs/VM_ARCHITECTURE.md` and
ADR 0052 both measured before, at a larger amplitude than the 1% previously
recorded, and it is not this decision's to fix.

## Consequences

- `crates/cove-runtime/tests/responsiveness.rs` is unchanged and passes. It
  measures the two evaluators ADR 0040 states bounds for; the native tier is
  behind a default-off feature and has no row there yet, which is ADR 0055's
  own adoption condition ("The native row is added to `responsiveness.rs`
  before native execution is adopted") and is not this change's to satisfy.
  What this change adds instead is where it can be run by the ordinary gate:
  `Machine::poll_budget`'s arithmetic is a unit test beside
  `next_question`'s, and the compiled interval is three suite cases run on
  **both** code generators — a threshold reached, a threshold of nought, and a
  stop taken at the first poll past the threshold — plus a differential case
  that runs seven thresholds through both arms and compares the poll log.
- The `safepoint` row of `cove run --boundary` is now a count of safepoints
  *taken* rather than of backedges executed, and the two were the same number
  before. A reader comparing a run of this tier against an older report should
  know which one they are holding.
- Compiled code is two instructions larger per backedge and one `u64` of the
  context is warmer. The VM is untouched: `encoded::dispatch` makes the same
  test it always made, and the only line that changed on its side is the
  `next_check` repair above, which is on the return path of a native call.
- A later slice that splits long straight-line blocks — ADR 0055's one
  remaining stated gap — has the mechanism it needs already published, and
  should use the same field rather than a second one.

## Alternatives considered

**Leave the test in the helper and make the helper cheaper.** The helper is
already short; what costs is the call, the load of the machine through two
pointers and the compiler's inability to see across the boundary. ≈20 ns is
about what a C-ABI call that touches three cache lines costs, and no amount of
work inside it removes 1.85 million of them.

**Compile `SAFEPOINT_STRIDE` in as an immediate.** One instruction cheaper —
`cmp r13, imm32` against `cmp r13, [rbx+disp8]` — and it fails the bound across
a tier boundary, as the Decision says. It would also put `cove-runtime`'s
contract arithmetic inside `cove-native`, where a change to the constant would
silently not reach the compiled tier until somebody remembered two crates.

**Count turns instead of work.** A backedge could decrement a register and poll
every N turns. It is the same instruction count and it is a bound in the wrong
unit: N turns of a loop whose body is one instruction and N turns of one whose
body is four hundred are not the same amount of work, and ADR 0040's table is
stated in work precisely so that no term of the bound depends on the program's
shape.

**Take a safepoint at every VM-to-native entry so that the accumulator starts
from a true nought.** Then a constant threshold would be exact. It buys a
simpler compare and costs a helper call per crossing — 1.68 million of them on
this corpus, which is the cost this ADR is removing, relocated — and it makes
entering compiled code a collection point, which is a semantic change nobody
asked for.

**Do nothing, and take the 22% elsewhere.** Issue #398's own caution is that
summed percentages are not the gate. This one was measured as a standalone
ablation on a fixed input tree, interleaved, and it is the largest single
number in the profile.

## What is not decided here

- **The value of `SAFEPOINT_STRIDE`.** It is 1024, it is ADR 0040's arithmetic,
  and moving it is still a change to a stated bound and to the test that
  measures it. This ADR changes *where the comparison happens*, not what it
  compares against.
- **Splitting long straight-line blocks.** ADR 0055 asks for a poll "at bounded
  intervals inside long straight-line code" and this tier still does not do it.
  A loop-free function of a hundred thousand instructions still polls once, at
  its return. That gap is stated where the code is and is unchanged by this.
- **Whether the encoded tier should read `poll_at` too.** It has its own
  threshold in its own coordinate, folded into the one comparison the dispatch
  loop already makes, and ADR 0052 measured what a second one costs there.
- **A native row in `responsiveness.rs`.** ADR 0055 requires it before
  adoption; this ADR does not bring adoption forward.
