# ADR 0063: A buffer window is measured before it is optimized

- Status: Accepted
- Date: 2026-09-18
- Decides: what a run counts about [ADR 0062]'s windows; that typed growable
  allocation covers words as well as bytes and that a word allocation is not
  floored; that truncation stays one trusted primitive and why; and — the larger
  half of this record — **which of the optimizations issue #423 asked for are
  not built, and the numbers that decided each one**. Recorded for
  [issue #423](https://github.com/myuon/cove/issues/423)
- Elaborates: [ADR 0062](0062-an-append-is-ensure-store-commit.md), whose "What
  is left open" names six things. Five are answered here — three by building
  them and two by measuring them and declining. The sixth, cq's layout-sensitive
  VM wall time, is answered only in the sense that this series measured it again
  and found the same thing
- Preserves: every decision of ADR 0062. The window is still ensure, a typed
  write and commit; `cove_ir::legalize` is still the one pattern definition; the
  verifier's block-local reservation rule is unchanged; and no composite
  collection instruction returns to shared IR
- Supersedes: nothing. Two of ADR 0062's *measured statements* turn out to be
  wrong, and they are corrected below rather than in it — an accepted ADR is a
  record of what was believed when it was written, and a reader should be able
  to see the belief and the correction as a pair
- Changes no source-language API
- Implementation status: **merged**, as pull requests
  [#424](https://github.com/myuon/cove/pull/424) through
  [#429](https://github.com/myuon/cove/pull/429). Every number below is measured
  against `ce45050`, the commit before the first of them, with both builds in
  their own target directories and run interleaved on fixed inputs

## Context

[ADR 0062] split the append into `GrowableEnsure`, a typed write and
`GrowableCommit`, moved the orchestration into standard-library Cove, and taught
the encoded VM and both native code generators to recover one fast path from the
split. It measured the result and left six things open on purpose, each with a
named beneficiary:

- a window near a safepoint declines and runs as rows;
- a growth finishes through the generic `fused_tail`;
- `GrowableAlloc{Words}` for `core.vectorWithCapacity` is not done;
- native pays for a window's frame writes where they are dead;
- **nothing counts how often a fast path declines in a real run**;
- cq's VM wall time is layout-sensitive beyond that change's reach.

Issue #423 asked for the rest: counters first, then typed word allocation, then
a decision about truncation, then a shared-IR optimization over verified
windows, then backend cleanup gated by the counters.

Its Phase 1 is the one that mattered, and not for the reason it was written. It
says:

> Do not infer growth from capacity arithmetic in a profiler after the fact;
> count the runtime decision.

That instruction, applied, produced numbers that then **decided against three of
the four optimizations the issue went on to ask for**. This ADR is mostly the
record of those refusals, because a refusal with a number behind it is a
decision, and an undocumented one is a thing the next person re-proposes.

## Decision

### A counting run says what became of every window

`--boundary` reports, per `cove_ir::legalize::Pattern`, what each window a fused
head ran actually did: `fast`, `slow`, or `declined` under one of nine named
reasons. It is beside ADR 0062's `fusions` rather than instead of it — `fusions`
is the number that ADR published, and this one is finer.

Three choices in it are load-bearing.

**A growth is counted where the reallocation happens**, in the runtime's `grow`,
and not derived from a window's outcome. The two are different populations: a
window that finishes the ordinary way may not have grown anything, and an ensure
outside every window may have. Deriving one from the other is exactly the
inference Phase 1 forbids, and the covefmt reading below is a case where the two
differ by a factor of three.

**A decline names a reason, and the reasons are families of questions rather
than one per `return`.** A reader asks what was wrong with the window, not which
line said so. The nine are `safepoint`, `charge`, `shape`, `owner`, `store`,
`source`, `range`, `chunk` and `bulk`.

**`safepoint` and `charge` are two reasons and not one**, and this was got wrong
first and fixed in [#429](https://github.com/myuon/cove/pull/429). The entry
test — a safepoint due within `WINDOW_TAIL` of the head — declines the whole
window and costs its entire dispatch saving. The bulk-charge test declines only
the copy; the window still runs fused through `fused_tail` and costs nothing.
Counted together they are uninterpretable: cq's `append.bytes` declines are
*entirely* the free kind and covefmt's `push.words` entirely the costly one.
Counted apart they make an identity exact that was previously hand-waved —

```text
fusions[p] == run(p) − declined[p][Safepoint]
```

— which holds to the digit on all four patterns of both programs, and failed on
three of the four appending rows before the split, by exactly the charge counts.

The census is free when it is off, to `report.rs`'s standing rule: every record
sits inside the `counting.is_some()` test that module already established, on a
path that had already decided to return. Where the head's row is not yet loaded,
the opcode is read *inside* the branch rather than hoisted. Every count ADR 0062
published is unchanged to the digit on both programs.

`Machine::fused_fast` is deleted. It was `#[cfg(test)]` precisely because
nothing counted a fast path in a real run; tests read `windows.fast`, which is
the fact the shipped thing measures.

### Typed growable allocation covers words, and a word allocation is not floored

`Inst::GrowableAlloc` is admitted over `Storage::Words(LayoutId)`, and
`core.vectorWithCapacity` is one of them where it was five instructions — two
`Alloc`s, an `Int` nought and two `StoreField`s.

The reason is the last of those five rather than the count. A plain `StoreField`
at a growable owner's length offset is the instruction ADR 0062 spent a stage
removing from every appending body, and a construction that writes a constant
nought there is harmless *only because the constant is nought* — a rule a reader
has to check rather than one the instruction set states. There is now none on
the growable path anywhere in the standard library.

**An allocation is the only growable operation that reads ADR 0052's ownership
pair backwards.** Every other one is handed an object whose own header names its
layout; this one has no object yet. The element is turned into the
`Shape::Vector` of it and the growable `Shape::Elements` of it through a table
built once per program and shared by `Arc`, the way `Machine::widths` is — not a
scan of the layout table per allocation, and not two more layout fields on the
instruction, which would be a second copy of a derivable fact for an encoder, a
verifier and two code generators to come to disagree about.

**A word allocation does not floor a small capacity, and a byte allocation
does.** This reads like an inconsistency and is the two constants' own contract:
`MIN_GROWABLE_BYTES` is "the smallest byte store a buffer is allocated with, and
the floor a growth doubles up from", because eight bytes are one word and a
store below that buys nothing; `MIN_GROWABLE_ELEMENTS` is "the floor of the
first growth rather than of the first allocation". The symmetric version was
written first and then measured: flooring cost cq's 196,677 constructions
**666,740 allocated words, 6.94% of the run's**, and avoided no growth at all,
because this intrinsic's callers in `std.map` and `std.set` size the vector to
exactly what they are about to push into it. Instructions and dispatches are
byte-identical with the floor and without, which is what says it bought nothing.

### Truncation is one trusted primitive, and the argument is written down

Issue #423 offered two outcomes: express truncation as a verified storage-level
operation that clears the removed range and publishes the smaller length
atomically, or retain one composite primitive and justify it.

The inventory answers the question. `runs::growable_truncate` already zeroes
`[len, run.len)` *at the element stride* and **then** writes the length word, as
one step nothing can observe between. The first outcome's substance is already
true; what it would add is form. So: the second outcome, and the work is the
justification and the test matrix.

- **The interval between the halves is exactly the unsound state.** A length
  published first exposes the vacated words as elements — a collection between
  the halves traces a store whose header says its whole capacity is elements. A
  clear after the publish is a write *above* the logical length, into spare room
  the next ensure may hand to a window that is about to commit. A split form's
  whole contribution is to make that interval nameable: a pc a debugger stops
  on, a safepoint a collection happens at, a place a later pass inserts into.
- **Nothing would read the pieces.** A typed clear *and* a second verifier
  relation — the reservation rule's mirror image, and a rule is the most
  expensive thing this IR can grow, because every backend is held to it —
  against one producer, no emitted fast path in either code generator, and no
  window for an optimizer to prove anything about.
- **It is not hot.** covefmt executes it 14,008 times out of some 775 million
  instructions; cq zero. There is no dispatch to save: the composite is already
  one and the split would be two.
- **It is not a negative `GrowableCommit`**, which the issue forbids. A commit
  publishes storage the window immediately before it has just initialized and
  has nothing to clean; a truncate un-publishes storage initialized arbitrarily
  long ago whose units hold live references until it clears them.

### The block-local capacity-check optimization is not built

Issue #423's Phase 4 asks, in the imperative, for a conservative shared-IR
optimization over verified windows. The analysis was built first and counted.
**It eliminates zero ensure executions on covefmt and zero on cq.** It fires 21
times statically on cq, and all 21 sites are in diagnostic paths that
`revenue-summary` never takes.

The structural reason is the finding, not the count. **Every ensure executed by
either program is already the ensure row of a recognised window** — 189 of 189
static on covefmt, 229 of 229 on cq, and there is not one bare ensure in either
program. A fused window folds the ensure into the one capacity check it makes
before the write, so **an ensure costs no dispatch today**. Proving one redundant
saves one comparison and one unit of fuel, and costs a fifth window shape in
`legalize` that the verifier's reservation rule, the encoder, the VM's fused arms
and both code generators would all have to be held to. A window that stops being
recognised costs six to eight dispatches where it cost one.

Reading the blockers as **full sets rather than first blockers** — "defeat this
one and nothing else, how many ensures become provable" — kills two tempting
directions outright. `call` and `block-boundary` are **never anybody's only
blocker**, so teaching the pass to survive a call, or to run over a dominator
tree instead of a block, buys zero ensures on both programs. The deliberately
crude clearing rules cost nothing, and the issue's own warning against
"speculative cross-block motion before block-local proofs and measurements
exist" is right for a reason it did not know.

Where the measurement does point is **symbolic rather than constant**
reservations: `std.map.inserted` is 49.6% of cq's ensure executions and
allocates exactly `gap + 1 + (length - above)` before asking for those three
parts in the same block. That is not built either, and for the same reason — at
its full measured reach it is about 0.2% of cq's instructions.

`docs/PHILOSOPHY.md`'s "Earn complexity through use" is the rule this follows,
and the issue's own gate anticipated it: "A failed proof leaves the original
correct primitive IR unchanged."

### The native code-size policy is the current codegen

Phase 5 asks to investigate cq's machine-code growth and compare four options,
selecting by fixed-input measurement.

The attribution had to come first, and it is new: the template generator emits a
whole window — hot path, both cold blocks and the join — as one contiguous run
of its code buffer, so a difference of two buffer lengths across the emission is
that window's machine code to the byte. Cranelift lowers to CLIF and the layout
is settled by the backend at the end of the function, so no range of the emitted
buffer corresponds to an IR window; it answers `None` rather than a zero, which
a reader comparing the two arms would take for "windows cost this generator
nothing".

Windows are **16.3%** of covefmt's machine code and **46.5%** of cq's.

Then the measurement that settles the policy: **a window's primitive rows are
larger than the window, for every pattern.** `push.words` 997 against 2,285,
`push.byte` 1,138 against 2,426, `append.bytes` 1,424 against 2,036,
`append.words` 1,426 against 2,036. An append's rows carry *four* guard
sequences — two `load-field`s, the ensure and the commit, each with its own
inline fast path and cold half — against the window's one, and the helper calls
between them are only about seventy bytes each. Declining every window costs
covefmt **+18.9%** and cq **+38.1%**.

So of the four options: "outlined proportional copy with inline capacity check"
**is** the current codegen, since the append arm emits an unconditional
`RunCopyFn` call and not a loop; "fully inlined copy windows" would add a loop to
a window already costing 1,300 bytes; and a size or admission threshold in its
cheap form — declining recognition — makes both programs larger. The current
codegen is retained, and `a_window_is_less_code_than_its_rows` pins the finding
over all five shapes on both arms so it cannot silently regress.

The one lever that does save is demoting a recognised window to the helpers that
are already its cold halves, needing no ABI change. It saves cq **20.0%** of its
machine code and costs **+12.4% native wall time on cq and +39.1% on covefmt**,
and covefmt's ceiling for it is −2.6% regardless, because 70% of its window
bytes are `push.words` whose demoted form is the same size. The issue's first
acceptance criterion is no repeatable wall-clock regression, and it outranks
code size. That ablation is what the code-size criterion's second clause asks
for.

### The two remaining backend items are measured and closed

**Dead native window slots.** Every window frame write is dead after its window —
884 of 884 on covefmt and 1,016 of 1,016 on cq — which is the encouraging half.
Correctness is the other. The cold edges *read* those slots: a naive
skip-the-write build hung covefmt, because the ensure helper read a garbage
`additional`. So most writes can only be *sunk* onto cold edges, which moves
bytes rather than removing them. And `Cleared` is what keeps the static `RefMap`
correct after the window, so eliding it leaves a stale root for the next
safepoint to trace. Only a `push.words` window's `Store` is deletable outright:
112 sites on covefmt, 38 on cq. The whole population is 8,060 bytes on covefmt
(0.87% of its machine code) and 7,463 on cq (1.99%). Emitting one *extra*
complete set of every one of these writes — strictly more than any elision could
remove — moved covefmt native by +0.13% to +0.61% against a 1.4–3.4% spread, and
left `benches/builtincall`'s Int-piece rows unchanged.

**The VM's slow completion.** ADR 0062 asked whether finishing a grown window on
the fast path buys anything. The census re-aims the question: on covefmt,
`push.words` takes the slow completion 1,799,076 times while only 508,022 word
stores are reallocated, and instrumenting the arm directly confirms the split —
**1,294,519 chunk cut against 504,571 growth**. `Words::run` hands back the rest
of the chunk it is asked in, a chunk is 8,192 words, and a store whose payload
reaches past its starting chunk fails the slice test on every push into that
region. So 24.0% of covefmt's pushes leave the in-place write for a reason that
is not growth, and cq has no case at all — 349 of 196,715.

There is **no dispatch benefit whatever**, by construction and measured: a fused
window is one dispatch whether it completes fast or slow, and both arms of the
ablation report the identical 734,970,227. A slow completion costs 12.4 to
14.9 ns, which bounds the whole effect at 16–19 ms of ~4,977 ms (0.32–0.39%) and
the growth subset Phase 5 actually names at 0.13–0.15%. Both are ceilings twice
over, and both sit under covefmt's rebuild noise floor. A specialized completion
path would be a third copy of the push protocol in the hot arm of a dispatch
loop whose frame budget ADR 0062 records as already spent.

## What this corrects in ADR 0062

Two measured statements in that ADR do not survive this series' measurements.
They are recorded here and not edited there.

**Its accounting for cq's +39.8% machine code.** ADR 0062 says the growth was
"cq's four newly compiled functions [which] are the standard library's appends
and keyed copies, which were a helper call before". The mechanism is right and
the scope is too narrow. Those four are identifiable — cq went 85 to 89 compiled
and the four new bodies total **5,484 bytes of the +105,410, 5.2%**. What
dominates is append windows inlined *everywhere*, including into functions that
were already compiled: an append had no emitted fast path at all before, so cq's
77 append windows cost about **93,700 bytes, 89% of the growth**. The
identification cross-checks — covefmt went 205 to 208 with exactly three such
bodies — and covefmt is push-heavy with pushes that already had a fast path,
which is why its growth was 7.0% and not 40%.

**Its figure for dead native frame writes.** ADR 0062 says the Int-piece rows of
`benches/builtincall` are "about 0.9 ns a digit dearer on native". That did not
reproduce: on this tip native's Int-piece slope is 35.0 ns a digit against the
VM's 84.9, so native is some 50 ns a digit *cheaper*. The figure was presumably
native-after-#422 against native-before, which is not what the sentence reads
as.

A third statement is worth recording as never having been true. ADR 0062 says of
`GrowableTruncate` that "Only its message becomes free of method names",
implying it named one. `git log -S` says the message has read `growableTruncate`
since #388, which predates the ADR 0062 series.

## Adoption

Each stage landed green on its own, with the full gate and `cove test` over
`examples/`.

1. **The census** — [#424](https://github.com/myuon/cove/pull/424). `Decline`,
   `Outcome`, `Windows`, `growths`; `fused_fast` deleted.
2. **Typed word allocation** — [#425](https://github.com/myuon/cove/pull/425).
   `GrowableAlloc{Words}`, `Op::GrowableAllocWords`, `GrowableOp::AllocWords`,
   `Machine::word_runs`, `Machine::alloc_vector`.
3. **Truncation settled** — [#426](https://github.com/myuon/cove/pull/426). The
   justification and the test matrix; 532 insertions, 0 deletions.
4. **Window byte attribution** —
   [#427](https://github.com/myuon/cove/pull/427). `WindowCode`, reported under
   the native machine-code line.
5. **The code-size policy** — [#428](https://github.com/myuon/cove/pull/428).
   `a_window_is_less_code_than_its_rows` over all five shapes, and the policy on
   `Emit::window`.
6. **The charge decline** — [#429](https://github.com/myuon/cove/pull/429).

### What the series measured

Two builds, `ce45050` and the tip after #429, each in its own target directory
with `--features cove-cli/template,cove-bench/template`, run interleaved on the
same `examples/` tree and the same fixed 20,000-record input.

**covefmt does not move in any count.** Zero opcode-level deltas across all
9,162 profiled sites: 775,664,863 instructions in 734,970,227 dispatches, fuel
783,353,229, 3,891,020 allocations, 48,365,377 allocated words, 922,417 bytes of
machine code, every helper row identical. It has no `core.vectorWithCapacity`
call site.

| cq revenue-summary, 20k | `ce45050` | after #429 | |
| --- | ---: | ---: | ---: |
| semantic instructions | 265,212,181 | 264,228,796 | −0.371% |
| dispatches | 258,285,724 | 257,322,816 | −0.373% |
| `fuel_spent` | 269,764,011 | 268,780,626 | −983,385 |
| allocations | 1,433,565 | 1,433,565 | ±0 |
| allocated words | 9,614,088 | 9,614,088 | ±0 |
| emitted IR | 6,544 / 113 fns | 6,534 / 113 fns | −10 |
| machine code | 376,834 B | 375,036 B | −1,798 B |
| compiled / refused | 89 / 24 | 89 / 24 | same set |
| `alloc` + `growable` helpers | 1,013,395 | 816,718 | −196,677 |

The instruction fall decomposes exactly. `core.vectorWithCapacity` is reached at
two static sites, executed 196,677 times, and six rows became one:

| opcode | before | after | Δ |
| --- | ---: | ---: | ---: |
| `alloc` | 433,378 | 40,024 | −393,354 |
| `store-field` | 393,372 | 18 | −393,354 |
| `clear` | 28,019,401 | 27,822,724 | −196,677 |
| `int` | 6,798,550 | 6,601,873 | −196,677 |
| `growable-alloc.words` | 0 | 196,677 | +196,677 |
| | | | **−983,385 = 5 × 196,677** |

**The dispatch fall is 962,908 and not 983,385, and the 20,477 difference is a
safepoint phase artifact** — which is worth writing down because it looks like a
lost saving and is not. Removing 983,385 instructions re-phases the safepoint
stride against window heads, so 1,314 more `push.words` windows fused at the
entry test and 5,935 fewer `append.words` did. `1,314 × 7 − 5,935 × 5 =
−20,477`, exactly, and cq's whole folded count reconciles to the row. The heads
themselves are identical between the arms. Nothing changed about what fuses.

### Wall clock, and a control that failed

| | `ce45050` | after | Δ set A | Δ set B | paired A/B |
| --- | ---: | ---: | ---: | ---: | ---: |
| covefmt VM | 4,963.6 ms | 4,857.9 ms | −2.13% | −2.04% | −2.41% / −1.95% |
| cq 20k VM | 2,182.5 ms | 2,127.0 ms | −2.54% | −3.04% | −2.25% / −2.99% |
| covefmt native | 1,985.7 ms | 1,976.7 ms | −0.46% | −0.51% | −0.67% / −0.56% |
| cq 20k native | 1,435.5 ms | 1,428.6 ms | −0.48% | −0.96% | −0.51% / −1.00% |
| `arith` VM (control) | 48.0 ms | 47.9 ms | −0.03% | −0.70% | −0.14% / −0.20% |

**No wall-clock effect is claimed, and covefmt VM is why.** Its counts are
byte-identical between the two builds — it does no work this series touched —
and it still moved −2.13% and −2.04% consistently across both sets with sub-1%
spread. That is the rebuild's code layout, measured on a real workload, and it
is larger than the entire count reduction on cq. Subtracting it from cq VM
leaves roughly the −0.37% the counts predict, which is the right sign and inside
the sets' disagreement with each other.

**The designated `arith` control did not do its job.** It moved −0.03% and
−0.70%, *less* than everything it was supposed to bound, where in ADR 0062 it
moved +3.18% and +3.50%, more. A 48 ms loop over integer instructions touches a
handful of dispatch arms and cannot see a layout change in the growable arms. It
is reported for continuity, but for a growable change **covefmt VM is the honest
control**, because its counts are provably unchanged and it exercises the same
code. That is a lesson about controls rather than about this change: a control
has to be able to see the thing it is controlling for.

### Every remaining growable and run instruction

Dynamic counts from `--profile --profile-rows all`, aggregated from the
by-instruction table — the by-opcode table applies a floor of 1,000 even under
`all`, which hides `run-store.bytes`. Slow-path rates are the census's, from an
unprofiled run.

| instruction | why it exists | std may orchestrate | VM fuses | native | covefmt | cq | slow rate |
| --- | --- | --- | --- | --- | ---: | ---: | --- |
| `growable-alloc.bytes` | two allocations under one temporary root; the owner is unreachable between them | yes | no | helper | 164,353 | 300,007 | — |
| `growable-alloc.words` | the same, over an element the table turns into two layouts | yes | no | helper | 0 | 196,677 | — |
| `growable-ensure.bytes` | the window's reservation | yes | as a window row | inline test, helper cold | 519,111 | 600,037 | see pattern |
| `growable-ensure.words` | the same | yes | as a window row | inline test, helper cold | 5,438,985 | 590,069 | see pattern |
| `growable-commit.bytes` | publishes an initialized suffix; the only thing that raises a length | yes | as a window row | inline, helper cold | 519,111 | 600,037 | see pattern |
| `growable-commit.words` | the same | yes | as a window row | inline, helper cold | 5,438,985 | 590,069 | see pattern |
| `growable-truncate.words` | clears vacated reference-bearing units and publishes the shorter length as one step | yes | no | helper, whole | 14,083 | 0 | — |
| `run-finish.bytes` | validates, relabels and consumes as one transition; each piece alone is unsound | yes | no | helper, whole | 164,353 | 300,007 | — |
| `run-finish.words` | the same, with nothing to validate | yes | no | helper, whole | 24,142 | 196,685 | — |
| `run-copy.bytes` | bounded proportional copy, charged and cancellable in chunks | yes | as a window row | helper, unconditional | 517,893 | 600,004 | — |
| `run-copy.words` | the same, at an element stride | yes | as a window row | helper, unconditional | 0 | 393,354 | — |
| `run-store.bytes` | one byte into a store under construction | yes | as a window row | inline blend, helper cold | 1,218 | 33 | — |
| `run-load.bytes` | one byte out of a run | yes | no | inline | 25,493,778 | 8,456,438 | — |
| `run-slice.bytes` | a bounded slice of a byte run | yes | no | helper | 977,682 | 360,000 | — |
| `run-slice.words` | the same, at an element stride | yes | no | helper | 271,189 | 0 | — |

Per-pattern slow and decline rates, from the census:

| program | pattern | heads | slow | slow rate | declined | declined rate |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| covefmt | push.words | 5,438,985 | 1,799,076 | **33.08%** | 37,886 | 0.70% |
| covefmt | push.byte | 1,211 | 14 | 1.16% | 9 | 0.74% |
| covefmt | append.bytes | 517,893 | 686 | 0.13% | 38,659 | 7.46% |
| cq | push.words | 196,715 | 349 | 0.18% | 45 | 0.02% |
| cq | append.bytes | 600,004 | 52 | 0.009% | 6,002 | 1.00% |
| cq | append.words | 393,354 | 760 | 0.19% | 12,219 | 3.11% |

Every instruction in the first table has a trusted-runtime justification, and
none of them is a public collection operation. The three ADR 0062 named as
composite — alloc, truncate and finish — are still composite, and truncate's
argument is now written out rather than asserted.

## Consequences

- **A counting run prices a fast-path change without a third binary.** That was
  the point, and it paid immediately: three of this issue's four optimizations
  were declined on numbers the census produced.
- **covefmt's `push.words` slow rate of 33.08% is the largest number the census
  made visible**, and it is not growth. It is bounded at 0.32–0.39% of wall
  clock, so it is recorded rather than acted on — but it is the first place to
  look if the chunk geometry ever changes.
- **cq's compiled image is 46.5% window templates.** That is retained on
  measurement, not on preference, and the ablation behind it is in this record.
- **A negative result costs about as much to produce as a positive one.** Four
  of this series' six pull requests are counters, tests and documentation, and
  the two that changed behaviour moved cq by 0.37% and covefmt by nothing. The
  alternative was building a fifth window shape, a symbolic reservation
  analysis, a liveness pass with a sinking transform and a specialized VM
  completion path, for effects between 0.13% and 0.39% on one program.

## What is not decided here

- **Whether symbolic reservations are ever worth it.** They are the one analysis
  the blocker census says would fire — half of cq's ensures — and they are
  declined on the size of the prize, not on the difficulty. A program whose
  ensures are *not* already inside fused windows would change that arithmetic.
- **Whether `Vector.of`'s literal construction should come under the
  no-field-store rule.** It still writes its length by hand, and
  `Vector.of(1, 2)` writes a constant `2` there. That needs a decision about
  what an exactly sized literal is worth.
- **The chunk geometry.** `Words::run` answering to the end of a chunk is what
  costs 24% of covefmt's pushes their in-place write. Nothing here proposes
  changing it.
- **`--profile-rows all` and the by-opcode floor.** The flag's documented
  purpose is "the per-site reading a script aggregates", and the by-opcode table
  still drops rows under 1,000. That is a tooling bug, not a decision.

[ADR 0062]: 0062-an-append-is-ensure-store-commit.md
