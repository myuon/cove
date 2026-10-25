# Evaluating the fuzzing harness

What the fuzzing harness of [FUZZING.md](FUZZING.md) reaches, what kinds of
error it can detect, and how that was measured — as opposed to how many bugs
it found. [Issue #681](https://github.com/myuon/cove/issues/681) is the plan;
this is the record of the measurements made for it, with the commands that
reproduce them.

Every section separates **measured** — a count a command printed, which the
command reproduces — from **inferred** — a reading of those counts that a
measurement did not establish on its own. A number here is a number for the
revision, settings and machine it names; none of it is a detection rate for
bugs nobody has seen.

## The setup

All of it on one machine: an x86-64 Mac (i7-10700K, 8 cores, 16 threads),
macOS, nightly `rustc 1.95.0-nightly (47611e160 2026-02-12)`, cargo-fuzz
0.11.2. The machine was shared: its load average was 4–8 throughout, so a
rate here is a floor and the ratios between runs of one campaign are worth
more than any one figure.

A **session** is `scripts/fuzz-measure.sh <target> <seconds> --runs 3 --fork 4
--replay` ([FUZZING.md, "Measuring a target"](FUZZING.md#measuring-a-target)):
three sessions per target, libFuzzer seeds 1, 2 and 3, each with `-fork=4`
(four worker processes: the core budget) for a fixed time, each from its own
copy of the same starting corpus — what `scripts/fuzz-seed-corpus.sh` makes —
built without AddressSanitizer (`-s none`; a count and a disagreement are not
memory-safety questions), counted in session (`COVE_FUZZ_STATS_DIR`) and then
by replaying the grown corpus once each (`cove-fuzz stats`).
`scripts/fuzz-measure-table.py <dir> [session|corpus]` turns a campaign's
output into the tables below.

## 1. What each pair of modes compared

Measured 2026-10-25 at `f561583` (the first version of the per-pair counts),
the **baseline** campaign: `differential` and `differential_source` 3 × 300 s,
`ir`, `ir_mutate` and `image` 3 × 180 s, `-fork=4` each. Starting corpora: 8
inputs (`differential`, its committed seeds and regressions), 413
(`differential_source`, the repository's programs and seeds), 6 (`ir`), 251
(`ir_mutate`), 256 (`image`). Each cell is *min–max over the three seeds*
(the median beside it where it matters); the full tables are what
`scripts/fuzz-measure-table.py` prints.

### `differential` (generated programs)

| | per session (min–max, median) |
|---|---|
| executions counted | 11,504–12,938 (12,813) |
| compared in full | 8,961–11,603 |
| **of those, every mode ran to success** | **3,845–6,481** (33.4–50.6% of what ran) |
| prefix-only | 1,210–2,543, of which an empty prefix 67.2–82.8% |
| `vm` vs `native`, full: success / error / trap / limit | 4,027–6,706 / 1,034–2,323 / 4,199–5,627 / 374–794 |
| `vm` vs `native`, full, both success, native ran | 4,027–6,706 |
| `vm-gc` vs `native-gc`, full, both success, native ran | 3,865–6,490 |
| `vm` vs `vm-gc`, prefix: nonempty / empty | 201–473 / 964–2,007 |
| small heap ran out of memory before its first instruction (`vm-gc` ran − started) | 473–683 runs (3.7–5.9%) |
| `ast` skipped (ran out of memory on the 2 MiB heap, which the interpreter has no equivalent of) | 374–794 |
| mismatches | 0 |

**Measured:** a full comparison is a run to success in fewer than half of the
programs; the rest of the full comparisons compare two runs that ended alike
in an `Err` (8–18%), a trap (33–49%) or a limit. The `vm` vs `native` pair
compared two complete successful runs with compiled code in 4,027–6,706
programs a session. The prefix-only comparisons are almost all the small heap
against the 2 MiB one (`vm` vs `vm-gc` and the like), and two thirds or more
of them compared no output; 473–683 runs a session of the small heap ran out
of memory before executing anything.

**Inferred:** the 33–49% of full comparisons that are two traps mostly stop
early — the generator draws boundary values on purpose (FUZZING.md, "What the
generator aims at") — so the operations after the first trap in such a
program are compared by nobody. How early they stop is not measured here.

### `differential_source` (mutated sources)

| | per session (min–max) |
|---|---|
| executions counted | 129,939–211,314 |
| ran (the source parsed, checked and lowered) | 3,054–3,773 (1.8–2.4% of executions) |
| compared in full, every mode success | 1,280–1,870 (41.9–49.6% of what ran) |
| of those, native ran | 1,248–1,808 |
| prefix-only, of which empty | 750–1,093, 26.1–77.2% |
| `ast` stopped by the deadline | 736–1,018 of 3,054–3,773 runs |
| no `main` (`this package does not declare`) | 196–285 |

**Measured:** 98% of a `differential_source` session's executions never reach
a run — 38% are not UTF-8, the parser refuses 93% of the rest, the checker
about half of what parses — and of what runs, the interpreter meets its 150 ms deadline in about a quarter,
which makes every `ast` pair of those programs a prefix comparison.

### The IR targets

| | `ir` | `ir_mutate` | `image` |
|---|---|---|---|
| executions counted | 102,487–113,488 | 254,432–320,819 | 254,397–320,943 |
| **built** (generated; decoded, and edited) | every input | 6,860–7,440 (2.3–2.9%) | 7,254–7,419 (2.3–2.9%) |
| verified | 74,906–80,584 | 3,940–4,217 | 4,361–4,457 |
| **started** (`vm` ran an instruction) | 54,201–58,363 | 3,929–4,199 | 4,250–4,439 |
| verified and did not start | 16,543–26,383, every one the entry refusing its argument (`this value is not the …`) | 11–18 | 18–120, some of them the bytecode verifier refusing what `cove_ir::verify` accepted |
| full, every mode success | 3,210–6,203 (4.2–7.9% of verified) | 358–555 | 739–921 |
| **of those, native ran** | **12–20** | 354–551 | 732–915 |
| native ran (programs) | 47–431 | 3,757–4,093 | 3,935–4,281 |
| a collection under a compiled frame (programs) | 17–207 | 2,912–3,118 | 2,783–3,114 |
| `vm` stopped by the 25 ms deadline | 2,955 in seed 1 | 2,832–3,448 | 2,643–3,167 |
| mismatches | 0 | 0 | 0 |

**Measured:**

- `ir`: 30% of the programs `verify` accepts never start, because the entry
  refuses the argument `ir::plausible_arg` built for it; the full
  comparisons are 95% two traps, and **the native
  tier ran compiled code in 0.06–0.5% of them** — so `vm` vs `native` on `ir`
  almost always compares the encoded VM with itself.
- `ir_mutate` and `image`: 97% of a session's inputs do not decode, so 2–3% of
  executions reach a program; of the programs that verify, about three
  quarters run into the 25 ms deadline on `vm` (the corpus is the
  repository's programs, benchmarks included), so most of their pairs are
  prefix comparisons. Where they do finish, native code ran in nearly all.

**Inferred:** the native tier's share on `ir` is low because a generated
program's work is in its entry, and the entry is the outermost frame, which
always runs encoded; that the generator calls few helpers was not measured
directly — the coverage replay in section 3 says which instructions ran in a
compiled function.

### What the in-session counts were missing

Before this change a session wrote its counts every 256 inputs or every ten
seconds. **Measured** on one 20-second `differential` session with
`-fork=2`, before and after: 12 inputs counted against libFuzzer's 240
executions at ten seconds, 410 counted against its 250 at one second (the
count exceeds libFuzzer's because each `-fork` child re-runs its slice of the
corpus). The 2026-10-25 figures in FUZZING.md were made with the ten-second
writes; which children's counts they lost is not known, so their shares are
not comparable with these.

## 2. Held is not executed

Measured 2026-10-26 with `cove-fuzz coverage <target> <corpus>`
([FUZZING.md](FUZZING.md#the-metrics), "What a program holds is not what it
executes") over each baseline session's grown corpus, once an input. A row is
*programs*: how many held a kind of instruction, executed it, held it in a
function the native tier compiles, and executed it there (not in the entry,
whose frame always runs encoded). The three columns per cell are the three
seeds' corpora.

| key | `differential`: present / executed / ran in compiled code | `differential_source` | `ir` | `ir_mutate` | `image` |
|---|---|---|---|---|---|
| programs measured | 2,026 / 2,134 / 1,847 | 604 / 656 / 648 | 3,530 / 3,365 / 3,119 | 417 / 488 / 396 | 498 / 566 / 555 |
| `Arith.Int.Mul` | 2,026 / 1,847 / 1,840 (seed 1) | 504 / 339 / 322 | 170 / 11 / **0** | 395 / 309 / 307 | 461 / 315 / 309 |
| `Cmp.Int.Lt` | 1,694 / 401 / 81 | 196 / 101 / 99 | 19 / 2 / **0** | 153 / 94 / 93 | 204 / 95 / 93 |
| `Call` | 2,026 / 1,995 / 1,755 | 530 / 505 / 383 | 1,016 / 94 / **0** | 411 / 393 / 305 | 486 / 442 / 366 |
| `Alloc` | 1,862 / 1,704 / 828 | 328 / 316 / 227 | 2,931 / 1,289 / **9** | 360 / 335 / 208 | 313 / 265 / 151 |
| `AddrOfSlot` | 1,520 / 422 / 224 | 235 / 181 / 158 | 1,266 / 62 / **0** | 263 / 218 / 196 | 209 / 157 / 137 |

And the **state that tells a right implementation from a wrong one**, in
programs (executed anywhere / in a compiled function other than the entry),
per seed's corpus:

| state | `differential` | `differential_source` | `ir` | `ir_mutate` | `image` |
|---|---|---|---|---|---|
| `Cmp.Int.Lt` with equal operands | 40/40, 81/80, 70/68 | 7/6, 11/10, 41/40 | 2/0, 5/0, 2/0 | 2/2, 7/7, 2/2 | 8/7, 23/22, 48/47 |
| `Arith.Int.Mul` overflowing | 25/3, 24/3, 17/4 | 0, 0, 0 | 2/0, 0, 0 | 0, 3/3, 0 | 0, 1/1, 0 |
| `ArithImm.Mul` overflowing | 35/4, 16/3, 23/0 | 0, 0, 0 | 1/0, 1/0, 7/0 | 0, 1/1, 0 | 2/1, 0, 1/1 |

Present and never executed in any of a target's three corpora: in
`differential`, `Store` and `Cmp.Bool.Eq`; in the source and image targets,
the task and resource instructions (`Cancel`, `ScopeLeave`, `ScopeCancel`,
`CallResource`, `HandleText`, `DynHandleText` — no task or resource is
granted); in `ir`, 8–21 comparison kinds in each corpus.

**Measured:**

- `ir` executes almost nothing in compiled code: of 3,119–3,530 programs a
  corpus, a `Call` ran in compiled code in none and an `Alloc` in 4–30. The
  native modes of `ir` compare the encoded VM with itself.
- `differential` holds `Arith.Int.Mul` in every program and executes it in
  compiled code in 91%, but executes an **overflowing** multiplication in
  compiled code in 3–4 programs of about 2,000; `differential_source`'s
  corpus in none. An overflowing product is drawn mostly in `main`, which runs
  encoded.
- `Cmp.Int.Lt` with equal operands in compiled code: 40–80 programs of about
  2,000 in `differential`; 6–40 of about 600 in `differential_source`.

**Inferred:** a defect in compiled `Int` multiplication's overflow check would
be reached by about 0.2% of the `differential` corpus and by none of the
source corpus; section 4 tests that reading against a mutant that has exactly
that defect.

## 3. A reference held across a collection by a compiled frame

`crates/cove-fuzz/tests/gc_roots.rs` ([FUZZING.md, "Roots of compiled
frames"](FUZZING.md#roots-of-compiled-frames)): six programs, one shape of
root each, run on the native tier over the differential targets' 2^11-word
heap. Measured 2026-10-26 (`cargo test --profile checked -p cove-fuzz
--features template --test gc_roots -- --nocapture`):

| seed | collections | under a compiled frame | the same, loop emptied | encoded instructions on `native` / on `vm` | calls into compiled code |
|---|---|---|---|---|---|
| `gc-root-string` | 7 | 7 | 0 | 20 / 408,666 | 12 |
| `gc-root-string-callee` | 7 | 7 | 0 | 20 / 408,672 | 14 |
| `gc-root-vector` | 8 | 8 | 0 | 20 / 409,751 | 6 |
| `gc-root-struct` | 7 | 7 | 0 | 20 / 410,346 | 24 |
| `gc-root-closure` | 7 | 7 | 0 | 20 / 408,575 | 18 |
| `gc-root-frames` | 7 | 7 | 0 | 20 / 410,202 | 20 |

**Measured**, per program: each holder is a function of its own after
lowering and the native tier compiled it; the encoded VM executed 20
instructions of the native run against about 409,000 on `vm`, so everything
but `main`'s calls ran as machine code; every collection happened under a
compiled frame, and none without the allocating loop — so all of them fell
between the value being built and being read; and the printed lines, which
carry the held values' contents, equal the text in the test and the
interpreter's.

**Not measured here:** that a root the collector *missed* would change those
lines. That is section 4's question.

## What is not known from these

- Whether any of this would notice a defect: a count of what was compared is
  not a count of what a comparison would catch.
- How a session's counts move with its length: every session here is three
  or five minutes on a shared machine.
- The coverage replay's native columns are an inference from the encoded
  VM's run; compiled code is not observed.
