# ADR 0124: The small heap is sized from the program's literals

- Status: Accepted
- Date: 2026-10-26
- Decides: that the differential and IR fuzz targets' `vm-gc` and
  `native-gc` run over a heap of the words the program's literals take
  before its first instruction (read from the `vm` run) and 2^9 words above
  them, instead of 2^11 words in all; that `vm-gc` and `native-gc` still
  share that capacity, so running out of memory on it is still an outcome
  the two are compared on in full; and that the runtime reports the literals'
  words (`Vm::literal_words`, `OwnedVm::literal_words`). Recorded for
  [issue #724](https://github.com/myuon/cove/issues/724), under
  [issue #681](https://github.com/myuon/cove/issues/681)
- Supersedes: [ADR 0113](0113-differential-fuzzing-compares-six-modes-and-a-stopped-run-by-its-prefix.md)'s
  Decision 1 sentence "The two `-gc` modes run over a 2^11-word heap". The
  rest of ADR 0113 stands, and in particular the reason a small heap is
  wanted at all — the collector is paced, so only a small *capacity* makes a
  generated program collect — and the rule that a heap's running out is a
  mode's stop between two modes of different capacities and an outcome
  between two of the same

## Context

ADR 0113 chose 2^11 words because the collector collects either once a run
has allocated a chunk (8,192 words) since the last collection or when an
allocation does not fit, and generated programs allocate less than a chunk.
It reasoned that 2^11 "still holds what a lowered program carries before its
first instruction — its literals". The measurements of issue #681 said
otherwise (docs/FUZZ_EVALUATION.md, section 1): 3.7–5.9% of a `differential`
session's small-heap runs ran out of memory before their first instruction,
and the small-heap pairs were most of the prefix comparisons that compared no
output at all.

Re-measured for this decision at `c5be4fc`, `differential` 3 × 180 s
(`-fork=4`): 197–539 small-heap runs a session ran out of memory before
their first instruction where `vm` started; `differential_source`, 1–172.
A run that never starts compares nothing, and a capacity of 2^11 *in all*
also makes what a program may allocate before it collects depend on how
large its literals happen to be — a property of the standard library, not of
the program under test.

## Decision

**The capacity is the literals and a fixed allowance.** The `vm` run — on
the large heap — reports the words its literals took (`literal_words`, the
heap below the run's immortal floor, which `Space::seal_static` raises once
every literal is placed and before the first instruction). The literals are
placed the same way whatever the capacity, so `vm-gc` and `native-gc` are
given exactly that and `GC_ALLOWANCE_WORDS` above it
(`cove_fuzz::differential::gc_heap_words`). `ir`, `ir_mutate` and `image`
do the same.

**The allowance is 2^9.** Chosen by replaying one session's grown
`differential` corpus (1,576 inputs) at each candidate:

| capacity | small-heap runs that did not start | programs that collected | collected under a compiled frame | ran out of memory | `vm-gc` vs `native-gc` compared in full, both success | prefix-only, of which empty |
|---|---|---|---|---|---|---|
| 2^11 in all (ADR 0113) | 80 | 367 | 336 | 169 | 522 | 169, 149 |
| literals + 2^11 | 0 | 125 | 111 | 50 | 557 | 55, 40 |
| literals + 2^10 | 0 | 299 | 240 | 56 | 556 | 56, 41 |
| **literals + 2^9** | **0** | **477** | **430** | **57** | **556** | **57, 42** |
| literals + 2^8 | 0 | 702 | 650 | 79 | 546 | 79, 60 |

and one session's grown `differential_source` corpus (1,957 inputs, 500 of
which run): 2^11 in all collected in 198 programs and ran out of memory in
59; literals + 2^9, 212 and 68. 2^11 above the literals would have kept
every program starting at the price of two thirds of the collections — the
purpose of the mode. 2^9 keeps every program starting and collects more than
before; 2^8 collects more still and begins to trade full comparisons for
out-of-memory stops.

**The out-of-memory comparison stays.** `vm-gc` and `native-gc` have the
same capacity, so one of them running out where the other did not is still
a leak or a lost collection and a disagreement (ADR 0113, Decision 3). A
program whose live data exceeds the allowance still runs out, on both.

**`gc_roots.rs` uses the same heap.** Its six programs held to the
small-heap standard of FUZZING.md, "Roots of compiled frames", now run over
`gc_heap_words` of their own literals — about 520 words — and collect 31 to
35 times where they collected 7 or 8.

## Consequences

- The runtime gains one public accessor, `literal_words`, on `Vm` and
  `OwnedVm`, a read of two numbers the allocator already keeps.
- A small-heap run never fails before its first instruction for want of
  memory. What its capacity is depends on the program, so two programs'
  small-heap runs are no longer over the same number of words; the
  allowance above the literals is what is the same.
- Counts made before this ADR (FUZZ_EVALUATION.md sections 1–4) and after it
  are of different small heaps. The collection counts in particular are not
  comparable across it.

## Measurements

Before (`c5be4fc`) and after, `scripts/fuzz-measure.sh <target> 180 --runs 3
--fork 4 --replay`, seeds 1–3, the same starting corpora; docs/FUZZ_EVALUATION.md,
section 7, has the tables and the re-run of mutant 003. In short, per
`differential` session: small-heap runs that did not start where `vm` did,
258 / 197 / 539 before and 0 / 0 / 0 after; programs that collected,
1,436 / 953 / 2,048 and 1,462 / 1,801 / 1,953; out of memory, 587 / 490 /
1,559 and 410 / 374 / 486; no mismatch in either.
