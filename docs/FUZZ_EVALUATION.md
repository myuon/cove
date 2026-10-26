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

## 4. Artificial defects

Four defects, written into the runtime or the code generator on purpose, and
whether the harness noticed — by three routes reported apart. **What a mutant
measures is whether that defect is detected, not how often an unknown one
would be**: four hand-picked defects are not a sample of the bugs nobody has
seen.

### How

Each mutant is a patch, `fuzz/mutants/<name>.patch`, marked `COVE-MUTANT
<name>`. `scripts/fuzz-mutant.sh apply <name>` makes a detached worktree of the
current commit under `fuzz/measure/mutants/` (ignored by git) and applies it
there; `revert` deletes the worktree; `check` — a step of CI's main job —
fails if a mutant is applied in the tree being tested or no longer applies.
A patch rather than a cfg or a feature, because a cfg is code that ships in
every build, one flag from being on; a patch has to be applied by hand, to a
copy, to exist at all.

`scripts/fuzz-mutant-experiment.sh <name> --replay T=DIR... --explore T=DIR...`
is the whole experiment, and `fuzz/measure/mutation/<name>/report.txt` its
record:

1. **dedicated tests**, in the mutant's worktree: `cove-fuzz`'s `gc_roots`
   and `regressions`, `cove-native`'s `template` (111 cases), `cove-runtime`'s
   `native_tier` (75), with `--features template`;
2. **corpus replay**: `cove-fuzz stats` built with the mutant over corpora
   that existed before the experiment — the committed seeds and regressions
   (`known/` left out), the `differential_source` starting corpus as it was
   before section 3's seeds, and the baseline campaign's grown corpora
   (section 1);
3. **new exploration**: `scripts/fuzz-measure.sh <target> 300 --runs 3
   --fork 4 --until-finding`, seeds 1–3, without ASan, from the baseline's
   starting corpus (8 inputs for `differential`, 413 for
   `differential_source`, **without** section 3's seeds) **with every input
   the replays found the mutant with taken out** — a known reproducer is never
   counted as found again. Each finding is replayed on the clean build: one
   that fails there too is the toolchain's, not the mutant's.

The clean tree passes all of it: the gate's `cargo test` and the baseline
campaign's 15 sessions found nothing (section 1).

Measured 2026-10-26, on the mutants applied to `ce8c4dd`–`d8e6261` (this
branch; the code they mutate is the same in each), on the machine of the
setup.

| mutant | what it breaks | dedicated tests | corpus replay | new exploration, 3 × 300 s, `-fork=4` |
|---|---|---|---|---|
| `001-native-int-lt-is-le` | native `Int` `<` answers `<=`, in every form `Emit::compare` emits (stored, immediate, fused with a branch) | **detected**: `gc_roots` 1, `template` 3 of 111, `native_tier` 33 of 75 failed | **detected**: every committed `differential` and `differential_source` seed; 1,715–1,953 of ~2,000 of each `differential` corpus, 319–465 of ~2,200 `differential_source`, 323 of 1,010 `ir_mutate`, 346 of 1,075 `image`; 0 of 4,288 `ir` | `differential`: every starting input found it, so it started empty, and the first program the generator made found it (0–1 s, all three seeds); `differential_source`: 25 s, 242 s, 48 s |
| `002-native-int-mul-unchecked` | native `Int` `*` wraps instead of trapping on overflow | **detected** by `template` only (3 of 111: `every_arithmetic_failure_is_the_vms`, `an_immediate_operand_fails_the_same_way`, `a_duration_destination_renames_only_three_overflows`); `gc_roots`, `regressions`, `native_tier` pass | **not** by any committed seed or regression; 4–7 of ~2,000 of each `differential` corpus; 1 of 1,075 `image`; **0** of every `differential_source`, `ir`, `ir_mutate` corpus | `differential`: 32 s, 277 s, and 300 s (the last second of its budget); `differential_source`: **not detected** in any of the three |
| `003-gc-innermost-frame-under-compiled-code` | a collection under compiled code does not walk the innermost frame's references | **detected**: `gc_roots` (5 of its 6 programs; see below), `native_tier` 2 of 75; `template` passes | **detected**: 5 of section 3's 6 seeds; before them, 0 of the 3 `differential_source` seeds and regressions, 1 of 8 `differential` (`random-192`); 21 of 413 of the old starting corpus; 26–62 of ~2,000 `differential`, 42–119 of ~2,200 `differential_source`, 92 of 1,010 `ir_mutate`, 43 of 1,075 `image`, 5 of 4,288 `ir` | `differential`: 21 s, 6 s, 12 s; `differential_source`: 73 s in one session, nothing in 300 s in another, and the third was **stopped at 76 s by an unrelated finding** (below) |
| `004-gc-interior-addresses` | a collection does not walk frames' addresses into heap objects (ADR 0120's interior roots), on either tier | **not detected** | **not detected** in any corpus | **not detected**: `differential` 0 of 3; `differential_source` 0 of 2, the third **stopped at 124 s by an unrelated finding** |

Two of the exploration sessions stopped on something the mutant did not do:
replayed on the clean build, both inputs fail there too. They are the
toolchain's own bugs, found by this measurement and kept under
`fuzz/regressions/differential_source/known/`: a module-level `fn f(self)`
that the checker accepts and the interpreter refuses to call while the VM runs
it ([#720](https://github.com/myuon/cove/issues/720)), and `!` on the `Any`
that `clock.timeout` answers, which the checker accepts and the lowering then
panics on ([#721](https://github.com/myuon/cove/issues/721)). Neither is
counted as a detection of a mutant, and both sessions are reported as not
having run their budget out. They also say what the baseline's "0
mismatches" was worth: the baseline's three `differential_source` sessions,
from almost the same starting corpus, found neither.

### Where each route came from, and why the misses missed

**Measured**, with section 2's coverage replay extended to the comparisons
fused with a branch and with an immediate (`cove-fuzz coverage`, over the
seed-1 corpora):

- **001 was reached everywhere.** A counted loop's test is a `CmpImmBranch` or
  `CmpBranch` `<` whose operands are equal on its last iteration: in compiled
  code in 1,795 and 1,692 of 2,026 `differential` programs and 392 and 352 of
  604 `differential_source` programs. The detection counts match that
  (1,863 of 2,026; 354 of the corpus's 2,196 inputs, of which 604 run). The
  stored-answer `Cmp.Int.Lt` with equal operands alone — which this mutant was
  first written to break, by mistake — runs in compiled code in 40 of 2,026.
  On `ir` nothing runs in compiled code (section 1), and 0 of 4,288 found it.
- **002 is the state the generator rarely makes in compiled code.** An
  overflowing `Arith.Int.Mul` or `ArithImm.Mul` executed in compiled code in
  7, 6 and 4 programs of the three `differential` corpora; the replay found
  the mutant with 7, 5 and 4 of them. In every `differential_source` corpus
  that state occurred in compiled code in **0** programs, and the replay and
  all three explorations missed — **the defect's trigger state was never
  made**, not made and missed. In `differential` the overflowing products are
  mostly in `main` (25 programs against 3 in compiled functions in seed 1's
  corpus), which runs on the encoded tier.
- **003 is reached by a collection while the frame that allocated holds the
  last reference to what it reads next.** Section 3's six programs measure it:
  the five whose holder allocates in its own frame found it; the sixth,
  `gc-root-string-callee`, whose holder calls a compiled callee to allocate —
  so the holder is not the innermost frame — passed, as the mutant's
  definition says it must. That is section 3's oracle working as designed,
  and also the first measurement that one of the six shapes is blind to a
  defect the others catch.
- **004 cannot misbehave on what the lowering writes.** The source targets'
  interior heap addresses come from `Shared.lock` (`lower/cells.rs`), which
  holds the cell itself in a reference slot for as long as the address lives
  ("an address into an object is live for exactly as long as the lock that
  made it safe to hold"); dropping the interior root leaves the cell rooted.
  `AddrOfField` executed in 2 of 604 `differential_source` programs and 2 of
  498 `image` programs, in compiled code in none. On `ir`, which can write an
  interior address with no other root, `AddrOfField` executed in 58 programs
  — not in compiled code, and whether any of them collected while that
  address was the only root is **not measured**. So: in the source targets,
  *the defect's state cannot arise* (inferred from the lowering, consistent
  with no detection); in `ir`, *not established*.

### Before and after section 3

The only change between "before" and "after" is section 3's six seeds; the
revision, settings, corpora and budget are otherwise the same.

| | before (the starting corpus without them) | after (the committed seeds with them) |
|---|---|---|
| `003` found by a committed `differential_source` seed or regression | 0 of 3 | 5 of 9 |
| `003` found by replaying `differential_source`'s starting corpus | 21 of 413 (all repository programs: benchmarks and examples that allocate in compiled frames) | — |
| `003` found by `cargo t`'s dedicated test | none existed | `gc_roots` fails |
| `001`, `002`, `004` found by the six | — | 001: all six (via the loops); 002, 004: none |

**Measured:** the seeds turn mutant 003 from "found by replaying the
repository corpus or by a fresh session within about a minute" into "found by
`cargo t` and by the first replay of the committed seeds". **Inferred:** the
same holds for a real defect of the same shape, and not for one of
`gc-root-string-callee`'s shape, which none of the six would find.

## 5. What to do next

Evidence first, then the proposal. Ranked by what the measurements say is
missing, not by what is imaginable.

1. **Make the native modes of `ir` run compiled code** (*generation target
   expansion* and *observation*). Measured: `ir` verifies 75,000–80,000
   programs a session and the native tier ran compiled code in 47–431 of them;
   in its corpus, not one `Call` and 4–30 `Alloc`s ran in compiled code;
   mutants 001 and 002 were found by 0 of 4,288 `ir` inputs. The `ir`
   target's `vm|native` and `vm-gc|native-gc` pairs compare the encoded VM
   with itself. The cheapest change is in the generator: helpers that the
   entry calls in a loop, so the work is in a frame the tier compiles; and
   `ir::plausible_arg` should build an argument the entry accepts — 30% of
   the programs that verify never start.
2. **A run-to-completion generator profile for `differential`** (issue 706).
   Measured: 33–51% of what runs succeeds in every mode; two traps are 33–49%
   of `vm|native`'s full comparisons; mutant 002's trigger — an overflowing
   product in compiled code — occurs in 0.2–0.35% of the corpus, mostly
   because overflows are drawn in `main`. A profile that keeps boundary values
   out of the path to the end, and puts the deliberate overflow, out-of-range
   slice or allocation *after* the state it is meant to break and in a helper,
   would raise both. Keep the current profile beside it: its traps are what
   the trap paths are compared on.
3. **Separate the small-heap pair's purpose from the rest** (*small heap / GC
   conditions*). Measured: 3.7–5.9% of `vm-gc` runs run out of memory before
   their first instruction, and the small-heap pairs are most of the empty
   prefix comparisons. The fix is not to drop the out-of-memory comparison —
   it is how a leak is caught between `vm-gc` and `native-gc` — but to size
   the small heap from what the lowered program's literals need (so it starts)
   and keep collections forced by a pacing chunk rather than by exhaustion.
   Section 3's programs collect seven or eight times on 2^11 words because
   they allocate *after* starting; the generator's programs do not.
4. **More shapes of root, and an interior-root program** (*observation*).
   Measured: one of the six shapes is blind to mutant 003, and mutant 004 is
   unobservable from the source targets. A shape whose holder calls an
   allocating callee *and* whose callee holds the last reference, and an `ir`
   program holding only an interior address across a collection, would close
   both; each is a few dozen lines in `gc_roots.rs`.
5. **Not now: a larger plain budget, or stdlib re-checking speed.** Measured:
   for every mutant found by exploration, the median time was under five
   minutes on four workers; the two misses are a trigger state the generator
   does not make (002 on sources) and one that cannot arise (004) — neither of
   which more time fixes. `differential_source` spends 98% of its executions on
   inputs the front end refuses, so speeding up the standard library's check
   would raise its rate, but what it runs is what limits it.

## 7. The small heap is sized from the program's literals (issue 724)

Section 5's third item, done:
[ADR 0124](adr/0124-the-small-heap-is-sized-from-the-program-s-literals.md)
gives `vm-gc` and `native-gc` the words the program's literals took on `vm`
and 2^9 words above them, where ADR 0113 gave them 2^11 words in all. The
ADR has the replay that chose 2^9.

### Measured: the same budget, before and after

`scripts/fuzz-measure.sh <target> 180 --runs 3 --fork 4 --replay`, seeds 1–3,
the same starting corpora (`scripts/fuzz-seed-corpus.sh` at `c5be4fc`: 8
`differential` inputs, 419 `differential_source`), on the setup's machine.
**Before** is `c5be4fc`, **after** `e29452e`. In-session counts, the three
seeds:

| `differential` | before | after |
|---|---|---|
| executions counted | 7,413 / 9,086 / 7,393 | 8,494 / 7,874 / 6,771 |
| small-heap runs that did not start where `vm` did | **258 / 197 / 539** | **0 / 0 / 0** |
| `vm-gc` collected (programs) | 1,436 / 953 / 2,048 | 1,462 / 1,801 / 1,953 |
| a collection under a compiled frame (programs) | 1,318 / 759 / 1,831 | 1,298 / 1,605 / 1,852 |
| `vm-gc` ran out of memory | 587 / 490 / 1,559 | 410 / 374 / 486 |
| `vm-gc` vs `native-gc`, compared in full | 7,412 / 9,059 / 7,390 | 8,482 / 7,865 / 6,771 |
| of those, both success | 2,879 / 4,741 / 2,109 | 3,920 / 3,599 / 1,806 |
| prefix-only programs, of which the common prefix was empty | 593, 517 / 519, 439 / 1,571, 1,213 | 430, 310 / 395, 274 / 494, 373 |
| mismatches | 0 | 0 |

| `differential_source` | before | after |
|---|---|---|
| executions counted | 144,889 / 106,460 / 77,705 | 194,886 / 73,960 / 149,377 |
| ran | 2,419 / 2,305 / 1,998 | 2,799 / 1,861 / 2,722 |
| small-heap runs that did not start where `vm` did | 1 / 72 / 172 | 0 / 0 / 0 |
| `vm-gc` collected (programs) | 787 / 800 / 804 | 998 / 586 / 843 |
| a collection under a compiled frame (programs) | 629 / 810 / 934 | 772 / 599 / 810 |
| `vm-gc` ran out of memory | 255 / 435 / 399 | 421 / 145 / 365 |
| `vm-gc` vs `native-gc` both success, in full | 746 / 816 / 800 | 932 / 529 / 972 |
| mismatches | 0 | 0 |

And replaying each session's grown `differential` corpus once an input (the
before corpora with the before build, the after with the after):

| `differential`, corpus | before | after |
|---|---|---|
| inputs | 1,576 / 1,697 / 1,299 | 1,613 / 1,520 / 1,266 |
| small-heap runs that did not start | 80 / 66 / 118 | 0 / 0 / 0 |
| `vm-gc` collected | 367 / 261 / 383 | 373 / 398 / 374 |
| a collection under a compiled frame | 336 / 208 / 347 | 328 / 340 / 349 |
| `vm-gc` ran out of memory | 169 / 137 / 308 | 85 / 83 / 88 |
| prefix-only, of which empty | 169, 149 / 142, 120 / 309, 250 | 89, 64 / 89, 57 / 88, 63 |

**Measured:** no small-heap run fails before its first instruction now on
either source target, and the programs that collect, and that collect under
a compiled frame, are as many or more; the out-of-memory stops and the
programs compared only by an empty prefix fell by a third to two thirds on
`differential`. The `differential_source` sessions vary more between seeds
than between the two builds.

### Measured: mutant 003, before and after

`scripts/fuzz-mutant-experiment.sh 003-gc-innermost-frame-under-compiled-code`
on each build, replaying the committed seeds and regressions, exploring
3 × 300 s (`-fork=4`) from the starting corpora above with every input that
found the mutant removed:

| | before (`c5be4fc`) | after (`e29452e`) |
|---|---|---|
| dedicated tests | `gc_roots`, `native_tier` | the same |
| inputs of the `differential_source` starting corpus that found it | 26 of 419 | 34 of 419 |
| new exploration, `differential` | 14 s, 25 s, 12 s | 20 s, 13 s, 19 s |
| new exploration, `differential_source` | 75 s, 55 s, 119 s | 136 s, 111 s, 48 s |

**Inferred:** the change does not move the time to find 003 by more than
the spread between seeds; it does make eight more of the repository's
programs find it on replay, which is consistent with more of them collecting
under a compiled frame once nothing runs out of memory before starting.

## What is not known from these

- How the four mutants' detection would move with a longer budget or more
  seeds: three seeds of five minutes each is a spread, not a distribution.
- Whether `ir` programs ever hold the only reference to an object in an
  interior address across a collection (mutant 004 on `ir`).
- Anything about the defects nobody has written: the detection of four
  chosen mutants is not a detection rate.
- Whether any of this would notice a defect: a count of what was compared is
  not a count of what a comparison would catch.
- How a session's counts move with its length: every session here is three
  or five minutes on a shared machine.
- The coverage replay's native columns are an inference from the encoded
  VM's run; compiled code is not observed.
