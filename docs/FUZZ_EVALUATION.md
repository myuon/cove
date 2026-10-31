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

## 現時点で言えること / 言えないこと

（2026-10-26 時点。根拠は各節、特に section 10。）

**言えること（測定）**

- 用意した 16 個の人工バグ（native codegen 6、GC の root / trace 5、VM の bounds、verifier、run 間の状態、capability、lowering 各 1）のうち、**fuzzing（committed 入力の replay か新しい探索）は 13 個、専用テストは 14 個、合わせて 15 個を検出した**。どれも検出しなかったのは verifier の欠陥（013）だけ。
- `differential` と `ir` の探索は、1 session 45 s・4 worker で、見つけた欠陥の大半を数秒〜数十秒で見つける（median 1〜19 s）。`ir` は #723 以前は native code を実質実行しておらず、001/002 を 1 件も検出できなかった。
- 専用テストが見逃して fuzzing だけが見つけた欠陥がある（012、VM の `byteAt` の off-by-one）。逆に fuzzing では原理的に見えない欠陥もある：run 間の状態（014、harness は毎回新しい machine で 1 回だけ走らせる）と capability（015、全 evaluator が同じ host registry を使うので oracle がない）。
- 探索中に本物のバグを 5 件見つけた（#720、#721、#726、#728、#734）。
- （2026-10-31、section 11）committed seed から 1 target 300 s の session 1 回で作った corpus は、Rust の分岐（`if`/`&&` 等の条件の各側）を parser 92%、checker 57%、lowering 59%、verifier 69%、VM 52%、native 生成器 56%、GC 56% 到達する。IR 命令 key は verifier が許す 185 個中 175 個を実行（compiled code 内では 124 個）、型 × 境界値の 107 セル中 104 個を実行（compiled code 内 97 個）。到達は検出ではない。
- （2026-11-01、section 12）4 worker・実時間約 2 分（build を除く）の PR 予算では、重点配分 B（`ir` ×2、`differential`、`image`/`parse`/`compile`）と全 target 配分 A が、人工バグ 002・003・012 をどちらも探索で 3/3 検出した（B の検出までの中央値 4.4 s、A は 9.6 s）。013 はどちらの探索も 0/3 で、#738 の固定テストと seed が検出する。以前の PR smoke は、9 target 中 7 個が 60 s 以内に初期再生を終えていなかった。

**言えないこと**

- **未知のバグの検出率。** 16 個は手で選んだ欠陥で、まだ誰も見ていないバグの標本ではない。スコアはこの 16 個に対する値。
- `differential_source` の探索の力。45 s では corpus の読み込みも終わらず、seed だけから始めた再実行も途中で止めたので、決着していない。
- 変更前（`c5be4fc`）の 16 個についての同じ campaign。予算の都合で走らせていない。前後比較は section 6〜9 の個別の測定だけ。
- 見逃しの診断（到達しない／状態が作られない／観測できない）は一部を除いて推論で、coverage replay による確認はしていない。

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

## 6. `ir` reaches compiled code (issue 723)

Section 5's first item, done: the `ir` generator has a *compiled* profile
beside the free one ([FUZZING.md, "The VM-level targets"](FUZZING.md#the-vm-level-targets)),
and a refused entry argument is drawn only by a wild program.

**Why a quarter of `ir`'s programs never started** (measured over `ir-stats`'
first 1,000 fixed inputs at `c5be4fc`): 119 of the 473 that verified did not
start, every one of them refused at the boundary with `this value is not the
…`, and every one of those had a `Unit` argument. The generator drew one
argument in eight as `Unit` on purpose — and `Unstructured::ratio` answers
`true` once the input is used up, so a program whose input ran out drew
`Unit` for every argument.

### Measured: the same budget, before and after

`scripts/fuzz-measure.sh ir 180 --runs 3 --fork 4 --replay`, seeds 1–3, from
the same 6-input starting corpus (`scripts/fuzz-seed-corpus.sh ir` at
`c5be4fc`, without the compiled seeds this change adds), on the setup's
machine with a load average of 5–7. **Before** is `c5be4fc`, **after**
`70598e3` (this change). Per session, the three seeds:

| | before | after |
|---|---|---|
| executions counted | 80,770 / 113,905 / 96,093 | 77,186 / 181,073 / 119,395 |
| verified | 55,460 / 82,016 / 72,867 | 56,134 / 143,513 / 97,663 |
| started on `vm` (of verified) | 76% / 68% / 75% | 100% / 99.97% / 99.99% |
| full, every mode success | 3,758 / 6,302 / 3,695 | 7,357 / 35,837 / 21,919 |
| **of those, native ran** | **7 / 8 / 0** | **2,138 / 27,077 / 16,175** |
| native ran (programs) | 100 / 452 / 88 (0.1–0.6% of ran) | 4,142 / 69,777 / 34,632 (7.4–48.6%) |
| a collection under a compiled frame (programs) | 27 / 323 / 6 | 153 / 1,198 / 768 |
| mismatches | 0 | 0 |

And what the grown corpora *executed* in a function the tier compiles
(`cove-fuzz coverage ir`, once an input; programs, the three seeds):

| key | before: executed in compiled code | after |
|---|---|---|
| programs measured | 2,877 / 3,676 / 3,170 | 3,331 / 4,426 / 4,108 |
| `Call` | 0 / 0 / 0 | 29 / 319 / 190 |
| `Alloc` | 6 / 5 / 5 | 246 / 1,300 / 914 |
| `Arith.Int.Mul` | 0 / 0 / 0 | 21 / 69 / 73 |
| `Store` | 0 / 0 / 0 | 58 / 100 / 238 |
| state `CmpImmBranch.Lt:equal` (a counted loop's last test) | 0 / 0 / 0 | 43 / 145 / 104 |
| state `Cmp.Int.Lt:equal` | 0 / 0 / 0 | 3 / 6 / 6 |
| state `Arith.Int.Mul` or `ArithImm.Mul` overflowing | 0 / 0 / 0 | 3 / 10 / 21 |

### Measured: mutants 001, 002 and 004 on `ir`

`scripts/fuzz-mutant-experiment.sh` at `70598e3`, replaying the seeds, the
regressions and the three *after* corpora, and exploring 3 × 180 s
(`-fork=4`) from the 6-input starting corpus:

| mutant | dedicated tests | corpus replay (after corpora) | new exploration, 3 × 180 s | before (section 4) |
|---|---|---|---|---|
| `001-native-int-lt-is-le` | detected (`gc_roots`, `template`, `native_tier`, as before) | 1 of 3,999, 8 of 5,141, 2 of 4,704 | 80 s, 58 s, 22 s | 0 of 4,288 `ir` inputs |
| `002-native-int-mul-unchecked` | detected by `template` and now also by `regressions`' `compiled_ir_programs_reach_compiled_code` | 3, 9, 21 | 42 s, 64 s, 16 s | 0 of 4,288 |
| `004-gc-interior-addresses` | not detected | 0, 0, 0 | not detected in two sessions; the third **stopped at 24 s by an unrelated finding** | not detected |

Every exploration finding of 001 and 002 was replayed on the clean build,
which agreed on it. The third 004 session's finding fails on the clean build
too, and on `c5be4fc`: an `ir` entry that returns while it holds a `Shared`
cell's lock, which `verify` accepts and a debug assertion in the run's exit
path panics on ([#728](https://github.com/myuon/cove/issues/728); kept as
`fuzz/regressions/ir/known/a-lock-held-at-return`). It is not counted as a
detection of 004.

**Inferred:** 001 and 002 are found on `ir` now because their trigger states —
a `<` between equal operands, an overflowing product — occur in compiled code,
which they did not before; the replay counts are small (0.02–0.45% of a
corpus) because a compiled helper's answer reaches the entry's result only
through the fold, and most helpers stop early (a full comparison of two
successes is 13–25% of what ran). Mutant 004 is still not reached: the native
tier does not compile `AddrOfField` (it is outside `cove_native`'s subset), so
an interior address is never held by a compiled frame, and whether an `ir`
program ever collects while an interior address is the only root is still
not measured — that is issue 725's.

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

## 8. More shapes of root, and an interior root (issue 725)

Section 5's fourth item, done. `gc_roots.rs` holds twelve programs (six
more: a `Map<String, String>`, an enum carrying a `String`, an `Option` and a
`Result` of one, a closure capturing a closure, a string in each frame of a
recursion twelve deep, and a string the innermost frame holds while it
allocates and then returns), and `tests/interior_roots.rs` holds an IR program
whose object is kept by nothing but an address into it
([FUZZING.md, "Roots of compiled frames"](FUZZING.md#roots-of-compiled-frames)).

### Measured: each new shape did what the test says

`cargo test --profile checked -p cove-fuzz --features template --test
gc_roots --test interior_roots -- --nocapture`, over section 7's small heap
(the literals and 2^9 words):

| seed | heap (words) | collections | under a compiled frame | the same, loop emptied | encoded instructions on `native` / on `vm` | calls into compiled code |
|---|---|---|---|---|---|---|
| `gc-root-map` | 565 | 33 | 33 | 0 | 20 / 410,493 | 28 |
| `gc-root-enum` | 530 | 31 | 31 | 0 | 20 / 410,126 | 20 |
| `gc-root-option` | 528 | 31 | 31 | 0 | 20 / 408,711 | 16 |
| `gc-root-closure-closure` | 518 | 32 | 32 | 0 | 20 / 409,907 | 20 |
| `gc-root-recursion` | 514 | 41 | 41 | 1 | 22 / 411,642 | 82 |
| `gc-root-callee-holds` | 520 | 31 | 31 | 0 | 20 / 410,023 | 16 |

The six of section 3, over the new heap, collect 31–35 times each (7–8 over
2^11 words in all). `interior-root`: `vm-gc` collected 11 times while the
address was the object's only root, `vm` none, and every mode answered 42.

### Measured: mutants 003 and 004, again

`scripts/fuzz-mutant-experiment.sh` at this change, dedicated tests and the
committed seeds and regressions (no exploration):

| mutant | dedicated tests | committed corpora | before (sections 4 and 7) |
|---|---|---|---|
| `003-gc-innermost-frame-under-compiled-code` | `gc_roots` (11 of its 12 programs), `native_tier` | 11 of 14 `differential_source` seeds — every `gc-root-*` but `gc-root-string-callee`; 1 of 6 `differential` | 5 of 9 seeds |
| `004-gc-interior-addresses` | **detected**: `interior_roots` | **detected**: `fuzz/seeds/image/interior-root` (`vm` and `vm-gc`: the answers differ) | **not detected** by any route |

**Measured:** 004 is now found by `cargo t` and by the first replay of the
committed `image` seeds. **Inferred:** the reason it was found by nothing
before is the one section 4 gave — the defect's state, an object whose only
root is an address into it, does not arise from source — and not that the
collector's interior roots were unobservable: given the state, the
comparison of `vm` with `vm-gc` sees it at once. That the fuzzer *makes* the
state on its own is still not shown: the `ir` explorations of section 6 did
not, and the native tier cannot compile a function that holds one.

## 9. A run-to-completion profile (issue 706)

Section 5's second item: the `differential` generator has a **complete**
profile beside the **hostile** one, chosen by the input's first byte
([FUZZING.md, "What the generator aims at"](FUZZING.md#what-the-generator-aims-at)).
`ir`'s compiled profile (section 6) is the IR target's counterpart.

### Measured: over fixed inputs

`cove-fuzz stats differential` over 300 random inputs each, the first byte
forced to one profile (at the change, over ADR 0124's small heap):

| | complete | hostile |
|---|---|---|
| compared in full | 296 | 285 |
| every mode ran to success | **296** | **129** |
| prefix-only | 4 | 15 |
| a collection under a compiled frame | 102 | 56 |
| mismatches | 0 | 0 |

Before the profile was tuned, the first complete programs still stopped in
120 of 300: 81 on an `Err` that `main` propagated, 14 on `replace` and 13 on
`split` given an empty string, and 3 on a builder's bytes that were not
UTF-8. Each is now drawn otherwise in the complete profile only.

### Measured: in a session, and what a session needs to see it

`scripts/fuzz-measure.sh differential 180 --runs 3 --fork 4 --replay`, seeds
1–3, before (`c5be4fc`) and after (`71e6f7b`, the change on the same base).

From the 8-input starting corpus of sections 1–4, the profile did nothing a
session could see: of the after sessions' grown corpora (1,578, 1,578 and
1,283 inputs) 1, 1 and 2 were complete programs, because libFuzzer mutates
an input's later bytes far more often than its first, and every starting
input is hostile. Full comparisons with every mode a success were 38.8 /
52.2 / 28.5% of what ran before and 40.9 / 47.3 / 21.5% after.

From the same 8 inputs **and the three committed complete seeds** (11
inputs, for both builds):

| | before | after |
|---|---|---|
| executions counted | 6,815 / 7,000 / 6,349 | 6,917 / 6,696 / 7,745 |
| full, every mode success | 2,418 / 1,508 / 2,250 | 2,266 / 4,476 / 3,441 |
| as a share of what ran | 35.5 / 21.5 / 35.4% | **32.8 / 66.8 / 44.4%** |
| prefix-only, of which empty | 1,249, 1,039 / 562, 491 / 553, 451 | 1,203, 891 / 673, 531 / 588, 465 |
| complete programs in the grown corpus | — | 39 / 749 / 52 of 1,375 / 1,303 / 1,520 |
| mismatches | 0 | 0 |

**Measured:** the share of runs that end in success in every mode rises
where the complete seeds took hold in the corpus (seed 2: 749 of 1,303
inputs, 66.8%) and hardly moves where they did not (seed 1: 39 inputs).

### Measured: mutant 002, the trigger the profile does not make

Section 5 hoped a complete profile would also raise mutant 002's trigger — an
overflowing product in compiled code — by putting the deliberate overflow
after the state. This change does not do that: the complete profile draws
*no* overflow on purpose, and the hostile one is unchanged. The coverage
replay agrees (`Arith.Int.Mul` or `ArithImm.Mul` overflowing in compiled
code: 0, 0, 3 programs before, 0, 5, 2 after), and so does the mutant:
explored 3 × 300 s from the 11-input corpus, 002 was found at 247 s, not in
300 s, and at 113 s before, and at 34 s, not in 300 s, and at 147 s after —
the same within the spread of three seeds. Placing a hazard after the state
it is meant to break is left to a later change of the hostile profile.

### Also found

`Unstructured::ratio` answers `true` once the input is used up, so a
hostile program whose input ran out draws every hazard from there on — the
generator's documentation said `false`. It is kept for the hostile profile,
whose committed inputs were found under it, and the complete profile answers
`false`; the same behaviour made a quarter of `ir`'s programs refuse their
entry argument (section 6).

## 10. Detection power over sixteen artificial defects (issue 708)

Sixteen mutants (`fuzz/mutants/`): the four of section 4 and twelve more,
across seven categories. Each is one reviewed patch, applied only in a scratch
worktree (`scripts/fuzz-mutant.sh`). **The scores below are over these
sixteen defects. They are not an estimate of how many unknown bugs the
harness would find**: sixteen hand-written defects are not a sample of the
bugs nobody has seen.

| mutant | category | what it breaks |
|---|---|---|
| 001 | native codegen: compare | `Int` `<` answers `<=` |
| 002 / 005 | native codegen: arithmetic | `*` / `+` of two slots does not trap on overflow |
| 006 | native codegen: arithmetic | shift counts 64..127 admitted (masked) |
| 007 | native codegen: arithmetic | 32-bit division path taken below 2^33 |
| 008 | native codegen: memory | `LoadElem` admits index = length |
| 003 / 004 | collector roots | innermost compiled frame not walked / interior addresses not roots |
| 009 / 010 / 011 | collector tracing | map values / a closure's first capture / array elements after the first not traced |
| 012 | VM bounds | encoded `byteAt` admits offset = length |
| 013 | verifier | string id = table length admitted |
| 014 | run-to-run state | resource table not emptied at a run's edge |
| 015 | capability grants | ungranted host call reaches the host |
| 016 | lowering | `!=` of walked values lowered as `==` |

### How

`scripts/fuzz-mutant-campaign.sh --out DIR` at `24fb51f` (origin/main through
#725 plus #706; x86-64 macOS, i7-10700K, 16 threads, load average 4–7), per
mutant: **dedicated tests** (`cove-fuzz`'s `gc_roots`, `interior_roots` and
`regressions` — reported together as `gc_roots` —, `template`,
`native_tier`, and per category `encoded`+`conformance`, `cove-ir` `verify`,
`embedding`+`lifecycle`, `authority`+`embedding`, `cove-ir` `lower`+`cove-cli`
`differential`); **replay** of every target's committed seeds and
regressions; **exploration** of `differential`, `differential_source` and
`ir`, **10 sessions × 45 s, `-fork=4`**, libFuzzer seeds 1–10, from the
starting corpus with every input the replay found removed. A session that
found nothing is censored at 45 s. Every exploration finding was replayed on
the clean build. The whole run took 5 h 50 min (13–28 min a mutant).
`scripts/fuzz-mutant-table.py DIR` makes the tables.

### Measured: per mutant × target

Exploration: sessions that found it of 10, and the time to the first finding
as Q1 / median / Q3 (`>45s` = censored).

| mutant | dedicated | replay (committed inputs that found it) | `differential` | `ir` | diagnosis of the misses (evidence) |
|---|---|---|---|---|---|
| 001 | yes (3 suites) | `differential` 11/11, `differential_source` 16/18, `ir_mutate` 3/6, `image` 1/12 | 10/10, 1 / 1 / 1 s | 10/10, 3 / 8 / 16 s | — |
| 002 | `template`, `regressions` | none | 2/10, >45 s | 10/10, 2 / 3 / 5 s | `differential`: trigger state rare (overflowing product in compiled code: 0–5 programs a corpus, §9) |
| 003 | `gc_roots`, `native_tier` | `differential_source` 11/18, `differential` 1/11, `ir_mutate` 1/6 | 10/10, 6 / 16 / 41 s | 10/10, 5 / 10 / 13 s | — |
| 004 | `interior_roots` | `image` 1/12 (the §8 seed) | 0/10 | 0/10 | state not produced: only the hand-written IR program holds an interior address alone (§8) |
| 005 | `template`, `regressions` | `differential` 2/11 | 10/10, 3 / 3 / 5 s | 10/10, 2 / 3 / 4 s | — |
| 006 | `template`, `native_tier` | none | 4/10, 26 s / >45 s / >45 s | 10/10, 8 / 13 / 30 s | — |
| 007 | 3 suites | `differential_source` 1/18 | 10/10, 4 / 14 / 19 s | 9/10, 1 / 3 / 10 s | — |
| 008 | `template` | none | 0/10 | 10/10, 3 / 5 / 6 s | `differential`: state not produced from source — the lowering bounds-checks before `LoadElem` (inferred from the lowering) |
| 009 | `gc_roots` (`gc-root-map`) | `differential_source` 1/18 | 0/10 | 0/10 | not reached: the generator writes no `Map` (generate.rs); `ir` not measured |
| 010 | `gc_roots` | `differential_source` 1/18 | 5/10, 12 s / >45 s / >45 s | 0/10 | `ir`: no closures in the compiled profile (§6) |
| 011 | `gc_roots`, `native_tier` | `differential_source` 1/18, `ir_mutate` 1/6 | 10/10, 5 / 19 / 23 s | 0/10 | `ir`: not established |
| 012 | **none** (`encoded`, `conformance` pass) | none | 9/10, 5 / 8 / 14 s | 10/10, 1 / 2 / 3 s | — |
| 013 | none (`verify` passes) | none | 0/10 | 0/10 | not reached: `irgen` draws string ids below the table's length; `ir_mutate`, which edits ids, was not explored |
| 014 | `lifecycle` | none | 0/10 | 0/10 | not reached: every mode runs in a fresh machine, once |
| 015 | `authority` | none | 0/10 | 0/10 (1 stopped by #734) | not observable: every evaluator shares the host registry, so there is no oracle |
| 016 | `differential` (cove-cli) | none | 9/10, 3 / 6 / 15 s | 0/10 | `ir`: not reached — the IR target bypasses the lowering |

`differential_source`: **0 of 160 sessions found anything**, and that number
says little. With the 428-input starting corpus a 45 s session ran 20–25
inputs — it did not finish loading its corpus. Re-run (`--no-dedicated`)
from the committed seeds and regressions alone (18 inputs, 550,000–680,000
executions a session), mutants 001–007 were found by 1 of 70 sessions
(003). Section 4's 300 s sessions found 001 and 003 in 25–242 s. So for
`differential_source` 45 s is too short a budget, and for 008–016 the
re-run was stopped before it reached them: **inconclusive**.

### Measured: mutation scores (of these 16 mutants — not an unknown-bug rate)

| | dedicated tests | committed-input replay | exploration, any of 10 sessions | exploration, share of 160 sessions | any route |
|---|---|---|---|---|---|
| `differential` | 14 | 3 | 10 | 79 | 15 |
| `differential_source` | 14 | 6 | 0 (inconclusive, above) | 0 | 14 |
| `ir` | 14 | 0 | 8 | 79 | 15 |
| all targets | 14 | 8 | 11 | | **15 of 16** (fuzzing alone: 13) |

| category | mutants | dedicated tests | fuzzing (replay or exploration) | any route |
|---|---|---|---|---|
| native code generation | 6 | 6 | 6 | 6 |
| collector roots and tracing | 5 | 5 | 5 | 5 |
| VM bounds | 1 | 0 | 1 | 1 |
| verifier | 1 | 0 | 0 | **0** |
| run-to-run state | 1 | 1 | 0 | 1 |
| capability grants | 1 | 1 | 0 | 1 |
| lowering | 1 | 1 | 1 | 1 |

**Measured:** fuzzing detects 13 of 16, all of them through
`differential` or `ir` exploration or a committed input; the dedicated tests
detect 14; together 15. Fuzzing is the only route for 012 (no dedicated test
fails); the dedicated tests are the only route for 014 and 015; nothing
detects 013.

### Also found

The `ir` exploration of mutant 015 stopped on a disagreement the clean build
shares: the native tier's `dyn.same-type` over a view no `dyn.open` made
answers where the encoded VM refuses
([#734](https://github.com/myuon/cove/issues/734);
`fuzz/regressions/ir/known/dyn-same-type-over-a-forged-view`). It is not
counted as a detection.

### Before and after sections 6–9

Not measured over the sixteen: the same campaign at `c5be4fc` was stopped
for budget before it ran. What is measured is per change, in sections 6–9:
001 and 002 on `ir` (0 of 4,288 inputs before; found in 16–80 s after, §6),
004 (found by nothing before; by a dedicated test and a seed after, §8), 003
(unchanged within the seed spread, §7), 002 on `differential` (unchanged, §9).

## 11. What the corpus reaches: branches by component, instruction kinds and boundaries (issue 705)

Three questions about one fixed corpus: how much of each component's Rust
code its replay reaches, which IR instruction kinds its programs execute, and
which combinations of an important type and a boundary value they execute.
Reaching is not detecting: everything here is a count of what ran, not of what
a comparison would have caught (section 10 is the detection measurement).

### How

`scripts/fuzz-coverage-report.sh --out DIR`, 2026-10-31, at `686d300`
(origin/main; the measuring code is this section's change and touches only
`cove-fuzz` and `scripts/`), x86-64 macOS, i7-10700K, 16 threads, nightly
`rustc 1.95.0-nightly (47611e160 2026-02-12)`, cargo-fuzz 0.11.2,
cargo-llvm-cov 0.9.1 (for its llvm-tools; the script calls `llvm-profdata`
and `llvm-cov` directly). Total wall time about 65 minutes.

- **Corpus.** Per target, a starting corpus of the committed `fuzz/seeds/<t>`
  and `fuzz/regressions/<t>` (not `known/`), then **one session of 300 s,
  `-fork=4`, libFuzzer seed 1, no sanitizer** (`scripts/fuzz-measure.sh
  --runs 1`); the corpus is what the session grew. No session found anything.

  | target | start | corpus | executions (last progress line) |
  |---|---:|---:|---:|
  | `parse` | 1 | 3,762 | 6,288,319 |
  | `compile` | 1 | 3,995 | 2,484,338 |
  | `run` | 2 | 3,946 | 2,891,993 |
  | `structured` | 6 | 2,193 | 13,612 |
  | `differential` | 11 | 1,976 | 10,101 |
  | `differential_source` | 18 | 2,927 | 598,705 |
  | `ir` | 9 | 5,503 | 212,752 |
  | `ir_mutate` | 6 | 1,187 | 373,476 |
  | `image` | 12 | 1,128 | 934,999 |

  (`structured` and `differential` compile every input and run it in every
  mode, which is why their execution counts are four orders of magnitude
  lower; both still grew about 2,000 inputs.)
- **Rust branches.** `cove-fuzz` built by nightly with
  `-C instrument-coverage -Z coverage-options=branch` (profile `checked`,
  feature `template`, its own target directory), replaying each target's corpus
  through the target's own harness (`cove-fuzz <target> FILE...`, the function
  libFuzzer calls), 50 inputs a process; no replay stopped on a finding. One
  profile per target, and their union. A **branch** is one side of a condition
  as rustc's branch instrumentation emits it: `if`, `while`, `&&`, `||`,
  `if let`, `let … else`. **A `match` arm is not a branch** in this
  instrumentation — it is a region — so the region columns are given beside
  the branch columns, labelled as regions. A branch in several monomorphised
  copies of one generic function is counted once, taken if any copy took it.
- **Components**, by file (`scripts/fuzz-coverage-components.py`'s
  `COMPONENTS`; test modules are not compiled into the binary and are not
  counted):

  | component | files |
  |---|---|
  | parser | `cove-syntax`: `lexer.rs`, `parser.rs`, `number.rs`, `token.rs`, `ast.rs` (not `format.rs`) |
  | checker | `cove-sema/src/**` (resolve, typeck, unique, facts, markup_if, package, stdlib, …) |
  | lowering | `cove-ir/src/lower/**`, `cove-ir/src/legalize.rs` |
  | verifier | `cove-ir/src/verify.rs`, `cove-ir/src/verify/**`, `cove-ir/src/bytecode/verify.rs` |
  | VM | `cove-runtime/src/vm/**` and `cove-ir/src/bytecode/**` (encoder, decoder, ops), less the rows below |
  | native generator | `cove-native/src/**` (template, subset, abi), and the runtime's native glue `cove-runtime/src/native.rs`, `vm/exec/native.rs` |
  | GC | `cove-runtime/src/heap.rs` (the interpreter's collector), and in `vm/mem.rs`, `vm/exec.rs` and `vm/exec/frames.rs` the functions named for collecting, marking, tracing, sweeping or roots (`GC_FUNCTIONS`) |

  Reported beside them so nothing is dropped: the AST interpreter
  (`interp.rs`), the rest of `cove-runtime` (builtins, values, hosts) and of
  `cove-ir`, the VM's debugger and profiler views (on no replay path), the
  harness itself, and `other` (`cove-diag`, the Rust formatter, …).
- **Instruction kinds and the matrix.** The ordinary build's `cove-fuzz
  coverage <target>` over each corpus of a target that runs programs (all but
  `parse` and `compile`), as in section 2. `cove-fuzz coverage-defined` lists
  what the counts are against.

### Measured: branches by component (union of every target)

| component | branches | reached | % | regions | reached | % |
|---|---:|---:|---:|---:|---:|---:|
| parser | 634 | 581 | **91.6%** | 5,057 | 4,828 | 95.5% |
| checker | 1,814 | 1,032 | **56.9%** | 17,773 | 11,889 | 66.9% |
| lowering | 2,366 | 1,395 | **59.0%** | 28,735 | 18,424 | 64.1% |
| verifier | 572 | 396 | **69.2%** | 5,557 | 4,949 | 89.1% |
| VM | 1,428 | 745 | **52.2%** | 17,348 | 11,342 | 65.4% |
| native generator | 564 | 314 | **55.7%** | 10,911 | 9,245 | 84.7% |
| GC | 150 | 84 | **56.0%** | 1,079 | 783 | 72.6% |
| *interpreter* | 344 | 163 | 47.4% | 5,010 | 2,687 | 53.6% |
| *runtime, other* | 902 | 252 | 27.9% | 10,491 | 2,690 | 25.6% |
| *ir, other* | 138 | 98 | 71.0% | 3,603 | 2,468 | 68.5% |
| *VM debugger/profiler* | 58 | 0 | 0.0% | 1,286 | 0 | 0.0% |

Per target, branches reached as a share of the component's branches:

| target | parser | checker | lowering | verifier | VM | native | GC |
|---|---:|---:|---:|---:|---:|---:|---:|
| `parse` | 89.0% | 0 | 0 | 0 | 0 | 0 | 0 |
| `compile` | 87.1% | 54.4% | 46.7% | 37.6% | 0 | 0 | 0 |
| `run` | 85.8% | 54.6% | 46.6% | 43.4% | 24.7% | 41.0% | 0 |
| `structured` | 46.8% | 44.2% | 55.1% | 45.6% | 34.7% | 46.5% | 24.7% |
| `differential` | 46.8% | 44.4% | 55.2% | 45.5% | 35.4% | 46.3% | 46.7% |
| `differential_source` | 80.9% | 54.5% | 55.7% | 46.3% | 33.8% | 44.9% | 26.0% |
| `ir` | 0 | 0 | 1.9% | 56.5% | 41.1% | 53.9% | 36.0% |
| `ir_mutate` | 0 | 0 | 2.3% | 51.7% | 37.9% | 43.6% | 24.0% |
| `image` | 0 | 0 | 2.4% | 62.2% | 36.4% | 48.9% | 27.3% |
| **union** | 91.6% | 56.9% | 59.0% | 69.2% | 52.2% | 55.7% | 56.0% |

(`compile` reaches the verifier because the lowering verifies what it
lowers, `cove-ir/src/lower/mod.rs:716`. `run` reaches no GC branch;
**inferred:** none of its 81 programs that lower allocates enough to collect.)

### Measured: where the unreached branches are (union)

The functions with the most unreached branches, five a component (the
script's `report.md` lists ten). "Why" is **inferred** from the function and
from what the generators write; no replay was run to confirm it.

| component | function (file:line) | unreached / branches | what it handles; why not reached |
|---|---|---:|---|
| parser | `lex_triple_string` (lexer.rs:755) | 12 / 62 | `"""` strings: indentation and escape errors — mutation rarely keeps a valid triple-quoted frame |
| parser | `fmt` ×3 (ast.rs:298, 249, 94) | 18 / 18 | `Display` of AST nodes — used by diagnostics/tools, not by parsing |
| parser | `parse_trait_decl` (parser.rs:1299) | 3 / 8 | trait declarations with defaults — rare in seeds |
| checker | `resolve_module` (resolve.rs:728) | 30 / 44 | multi-module resolution — every fuzzed program is one module, `playground` |
| checker | `check_literal_arms` (resolve.rs:3283) | 18 / 22 | exhaustiveness of literal `match` arms (`Bool`, `Int`, `String`) — generators do not write literal patterns |
| checker | `check_tests` (typeck.rs:877) | 18 / 18 | `test fn` declarations — the harness compiles no tests |
| checker | `same_markup_help`, `insertion` (markup_if.rs:149, 253) | 30 / 30 | markup-`if` diagnostics — no markup in the corpus |
| checker | `check_entries`, `signature_difference` (typeck.rs:9873, 10081) | 32 / 32 | entry-point and trait-signature mismatch diagnostics — rejected inputs of a shape the mutator does not make |
| lowering | `recognize` (legalize.rs:211) | 30 / 82 | fused-window recognition: the window shapes (appends, pushes) a corpus program rarely forms |
| lowering | `plan` (loads.rs:1234) | 21 / 122 | the cases in which `plan` declines to answer a plan |
| lowering | `assertion` (assertions.rs:71) | 16 / 16 | `assert` — not in the generators' grammar |
| lowering | `shared_lock`, `shared_new` (cells.rs:139, 79) | 26 / 26 | `Shared` cells — tasks are not granted (`max_tasks: 0`) and generators write none |
| lowering | `call_dyn` (dispatch.rs:1567), `admit_by_walk` (core.rs:1576) | 28 / 28 | dynamic dispatch and walked admission — `Dynamic` values only from seeds |
| verifier | `check_inst` (verify.rs:834) | 26 / 68 | per-instruction faults: the faults of kinds `ir`'s refusals rarely hit |
| verifier | `guarded_by_a_walk` (verify.rs:263) | 24 / 24 | the walk-guard shape around a box's wording — no `ir` program makes it |
| verifier | `slots` (bytecode/verify.rs:212) | 13 / 20 | bytecode slot faults — the encoder never writes them; only hand-made bytecode would |
| verifier | `one_admission_boundary`, `words_a_refusal` (verify.rs:164, 216) | 23 / 34 | admission-boundary faults |
| VM | `fused_append_words`, `fused_append_bytes`, `fused_push` (encoded.rs:2312, 2020, 1654) | 114 / 240 | fused append/push windows' generic arms — see `recognize` |
| VM | `unplaced_in` (dynamic.rs:1393) | 36 / 36 | auditing a box's unplaced names — a `Dynamic` failure path |
| VM | `family_of`, `fits` (boundary.rs:875, 1099) | 44 / 44 | host-boundary value families — only `console` is granted, and it takes strings |
| VM | `identity_leave` (dynamic.rs:831) | 15 / 16 | walks' identity sets |
| native | `inst_refused` (subset.rs:886) | 114 / 240 | why an instruction is outside the compiled slice: one arm per refusal; most forms are admitted, so most refusal arms never fire |
| native | `call_body` (vm/exec/native.rs:1954) | 18 / 34 | entering compiled code from the VM: the yield and host-call resumption paths |
| native | `offer_yield` (vm/exec/native.rs:922) | 10 / 10 | a compiled frame yielding to a scheduler — no tasks |
| native | `missed`, `tier_up`, `loops` (native.rs:310, 455, 435) | 17 / 18 | lazy tier-up — every fuzzed mode compiles eagerly |
| native | `growable` (vm/exec/native.rs:1573) | 9 / 14 | growable-run helper's error paths |
| GC | `visit`, `count`, `count_task`, `visit_task` (heap.rs:860, 692, 794, 968) | 34 / 48 | the interpreter's cycle collector over maps, sets, tasks — no tasks, few maps |
| GC | `collect` (vm/mem.rs:1517) | 6 / 10 | a collection with other tasks parked or already collecting — one task a run |
| GC | `each_root` (vm/exec.rs:173) | 4 / 12 | roots of parked/suspended frames |

Every component's branches are concentrated: of the functions with a branch,
the share with **none** reached is parser 4 of 99, checker 53 of 257,
lowering 80 of 336, verifier 1 of 61, VM 67 of 227, native 16 of 81, GC 2 of
19.

### Measured: IR instruction kinds

185 keys are defined: every kind of `irgen::KINDS`, with `Arith`, `ArithImm`,
`Neg`, `Cmp`, `CmpImm`, `CmpBranch` and `CmpImmBranch` split by operation and
operand as `coverage::key` splits them, less the three-way orders the
verifier refuses — what the verifier admits, not what the lowering emits.

| target | programs measured | present | executed | executed in compiled code | executed / defined |
|---|---:|---:|---:|---:|---:|
| `run` | 81 | 65 | 64 | 55 | 34.6% |
| `structured` | 2,193 | 115 | 113 | 109 | 61.1% |
| `differential` | 1,976 | 113 | 111 | 106 | 60.0% |
| `differential_source` | 426 | 95 | 89 | 80 | 48.1% |
| `ir` | 4,837 | 184 | 175 | 119 | 94.6% |
| `ir_mutate` | 587 | 73 | 72 | 64 | 38.9% |
| `image` | 294 | 103 | 69 | 58 | 37.3% |
| **union** | 10,394 | 184 | **175 (94.6%)** | **124 (67.0%)** | |

- **Never executed (10):** `Cmp.Bool.Ge`, `Cmp.Identity.{Eq,Ne,Gt}`,
  `CmpBranch.Identity.{Eq,Ne,Le,Ge}`, `CmpBranch.Tag.Lt` (all present in some
  `ir` program and not reached), and `CmpBranch.Tag.Gt` (never present).
- **Executed, never in compiled code (51):** every `Dyn*` kind and
  `HandleText`, the task and scope kinds (`Spawn`, `Await`, `Cancel`,
  `Settled`, `ScopeEnter/Leave/Cancel`, `SharedLock/Unlock`), `CallResource`,
  `Box`/`Unbox`, `AddrOfField`, `AssertFailed`, `Arith.Float.Rem`, and the
  orderings over `Bool`, `Identity` and `Tag` (`Cmp.Tag.Lt`, …,
  `CmpBranch.Bool.Ge`). **Inferred:** most of these the native tier refuses
  (`subset.rs`), so a function holding one is not compiled; the orderings only
  `ir` makes.
- Without `ir`, the other six targets together execute 119 keys, 113 of them
  in compiled code: the source and image targets reach about two thirds of
  what the verifier admits.

### Measured: the type × boundary matrix

`crates/cove-fuzz/src/matrix.rs`, `MATRIX` — 107 cells, each counted on the
encoded VM (executed anywhere) and in compiled code (executed in a function
the native tier compiles other than the entry): 214 cell × tier. A cell is an
instruction's operands at the moment it runs:

| family | operations | classes | cells |
|---|---|---|---:|
| `int` | add, sub, mul (`Arith.Int`, `ArithImm`) | an operand 0, ±1, `MIN`, `MAX`; exact result not an `Int` | 15 |
| `int` | div, rem | dividend 0, divisor ±1, dividend `MIN`, `MIN / -1`, divisor 0 | 10 |
| `int` | shl, shr, shru (`Shift`) | value negative, count 0, count 63, count outside 0..63 | 12 |
| `int` | cmp (`Cmp.Int`, `CmpImm`, `CmpBranch.Int`, `CmpImmBranch`) | equal, differ by 1, an operand `MIN`/`MAX` | 3 |
| `int` | neg | 0, `MIN` | 2 |
| `float` | arith (`Arith.Float`, `Neg.Float`), cmp, to_int (`FloatTruncate`), math (abs, round, sqrt, min/max) | an operand NaN, +0, −0, ±∞, subnormal | 20 |
| `float` | to_int; from_int (`Convert.IntToFloat`) | finite outside [−2⁶³, 2⁶³); not exact | 2 |
| `string` | byte_at (`RunLoad` of bytes) × content | index 0, = length, > length | 15 |
| `string` | slice (`RunSlice` of bytes) × content | from 0, end = length, end > length | 15 |
| `array` | load, store (`LoadElem`, `StoreElem`) | index −1 (<0), 0, last, = length, > length | 10 |
| `array` | len (`Len` of anything but a `String`) | 0, 1, ≥ 1024 | 3 |

String content is empty, ASCII, or any of multibyte (a 2–3-byte character),
astral (4-byte), combining (a mark in the combining blocks); positions are in
bytes. An array's length is its header length (a `Vector`'s store is its
capacity). Not in the matrix: `Float.parse` and `Float` formatting, which are
Cove code rather than one instruction.

| target | int (VM / compiled) | float | string | array | all |
|---|---:|---:|---:|---:|---:|
| `run` | 16 / 14 of 42 | 0 / 0 of 22 | 7 / 7 of 30 | 6 / 6 of 13 | 29 / 27 |
| `structured` | 40 / 39 | 22 / 22 | 25 / 17 | 7 / 6 | 94 / 84 |
| `differential` | 40 / 36 | 22 / 22 | 24 / 20 | 6 / 6 | 92 / 84 |
| `differential_source` | 19 / 17 | 15 / 13 | 7 / 7 | 6 / 6 | 47 / 43 |
| `ir` | 37 / 36 | 20 / 17 | 20 / 11 | 12 / 12 | 89 / 76 |
| `ir_mutate` | 18 / 16 | 0 / 0 | 9 / 9 | 7 / 6 | 34 / 31 |
| `image` | 19 / 16 | 0 / 0 | 7 / 7 | 6 / 6 | 32 / 29 |
| **union** | 41 / 41 of 42 | 22 / 22 of 22 | 28 / 22 of 30 | 13 / 12 of 13 | **104 / 97 of 107 (201 of 214)** |

- **Never executed (3):** `int.div.overflow` (`MIN / -1` — `rem`'s is
  executed, in 2 programs), `string.slice.empty.end_past`,
  `string.slice.astral.end_past`.
- **Executed, never in compiled code (7):** `string.byte_at.astral.index_len`,
  `string.byte_at.combining.index_len`, `string.byte_at.combining.index_past`,
  `string.slice.{ascii,multibyte,combining}.end_past`, `array.len.large`.
- **Thin cells** (executed in compiled code by ≤ 11 of 10,394 programs):
  `int.sub.overflow` 3, `int.mul.min` 7, `int.rem.min` 9, `int.rem.overflow`
  1, `int.shl.count_63` 11, `int.shr.count_63` 3, `int.shr.count_out_of_range`
  11, `int.neg.min` 3, `string.byte_at.{ascii,multibyte,astral}.index_past`
  4–5, `string.byte_at.empty.index_past` 4, `string.byte_at.ascii.index_len`
  4. `int.mul.overflow` is 86 programs
  anywhere and 20 in compiled code (section 2's 3–4 of ~2,000 was the
  `differential` corpus alone).

**Inferred:** the source-shaped targets (`run`, `differential_source`,
`ir_mutate`, `image`) reach no `float` cell outside `differential_source`
and few boundary cells at all: their corpora grew from a handful of seeds
in one session and keep the values those seeds hold. The cells come almost
wholly from the two generators and `ir`.

**Measured, per target:** `slice.*.end_past`, `array.load.{negative,
index_len, past}` and `array.store.index_len` are executed by `ir` alone
(`array.store.past` also by 12 `ir_mutate` programs); `byte_at.*.index_past`
by `structured` (21 programs) and `differential` (3) as well. **Inferred:**
that split is where the bound is checked — `byteAt` leaves its bound to the
instruction (`lower/methods.rs`, `byte_at`), while an element index is
checked in the lowered code before `LoadElem`, and a slice past the end is
refused in `std.string` before `RunSlice` runs — so the out-of-range cells of
those instructions are tested only against generated IR, never from
source.

### What this does and does not show

- **Coverage is not detection.** A branch reached or a cell executed says the
  code ran in that state, not that a wrong answer there would differ from a
  right one in anything a differential compares (section 10 measures that, for
  sixteen defects).
- **It is this corpus.** One 300 s session per target from the committed
  seeds; a longer session, another libFuzzer seed, or the repository's
  programs as a starting corpus (as sections 1–10 used) would move every
  number. One session is a point, not a spread.
- **Native machine code is not instrumented.** The native generator's rows are
  the Rust that *generates and enters* code; the emitted code runs without
  counters. The matrix's and instruction table's compiled columns are section
  2's inference — the encoded VM's watched run, attributed to compiled code
  when the function is compiled and is not the entry.
- **Instrumentation changes the run.** The instrumented binary is slower, and
  a run is bounded by a 150 ms deadline, so a replay may stop where the
  session's run did not; the counts are of the replay.
- **Only linked code is counted.** A generic function never instantiated in
  `cove-fuzz` has no coverage record and is in no denominator.
- **Branches are conditions, not arms.** A `match` is regions, not branches,
  in rustc's branch coverage; a component written mostly as `match` (the
  checker, the VM's dispatch) has more of its decisions in the region columns.
- **The defined keys are the verifier's.** 185 is what the verifier admits;
  `Cmp.Bool.Lt` is admitted and no source makes it.

## 12. A two-minute budget on four workers (issue 681)

The question is no longer what each target reaches. It is what a pull request
should spend: **about two minutes of wall clock on four workers, not counting
the build**, with the initial replay, exploring and shutting down all inside
that time. Section 12 compares two plans for that budget against how the CI ran
before, and places what the chosen plan leaves out into longer tiers.

**The scope was cut on purpose.** Each configuration got 3 seeds, not 5. Four
mutants were tried, not sixteen. A third configuration (C) was not run,
because A and B gave no measured reason for one. Component branch coverage
for these runs was not measured (the instrumented replay was left out for
budget). The rest of this section says what was and was not evaluated.

### How

A and B at `3fb1056`, the mutant trials at `f832fc2` (this change's runner on
origin/main `ca6c9c0` with #738; the harness is the same in both),
2026-11-01, x86-64 macOS (i7-10700K, 16 threads, load average 3–6, shared
with other work), nightly `rustc 1.95.0-nightly (47611e160 2026-02-12)`,
cargo-fuzz 0.11.2.

- **Runner**: `scripts/fuzz-budget.sh <plan> --no-build --seed N`, with
  `wall 120`, `reserve 15`, every slot `-fork=1` with crashes, timeouts and
  out-of-memory stops ignored, stopped from outside at its share's end
  ([FUZZING.md, "A budget of workers"](FUZZING.md#a-budget-of-workers)).
  libFuzzer seeds `N*100 + worker*10 + slot`, N = 1, 2, 3.
- **Builds**, timed apart from the budget: `cargo fuzz build` of all nine
  targets, 55 s without a sanitizer and 83 s with ASan (a fresh target
  directory). In a mutant's worktree, an incremental rebuild of `cove-fuzz` and
  both sanitizer builds took 122–172 s.
- **Starting corpus**: the PR corpus, `scripts/fuzz-pr-corpus.sh materialise`
  at this change. Its selection (`fuzz/corpus-pr/*.selection.txt`) took 483 s
  and kept every feature its pool reaches. The generator targets' pool is not
  a nightly corpus. It is a 180 s, four-worker run of `fuzz/plans/pool.plan`
  from the committed inputs (seed 99).

| target | starting inputs | of which committed seeds and regressions |
|---|---:|---:|
| `ir` | 99 | 9 |
| `differential` | 39 | 11 |
| `image` | 48 | 12 |
| `parse`, `compile` | 37 | 1 |
| `differential_source` | 54 | 18 |
| `run` | 38 | 2 |
| `ir_mutate` | 43 | 7 |
| `structured` | 34 | 6 |

- **Plans**:

  | | worker 1 | worker 2 | worker 3 | worker 4 |
  |---|---|---|---|---|
  | **A** (`pr-a`, every target) | `differential` 60, `structured` 60 | `ir` 60, `ir_mutate` 60 | `differential_source` 60, `run`† 60 | `image`† 40, `parse`† 40, `compile`† 40 |
  | **B** (`pr-b`, the targets that found most in section 10) | `ir` 120 | `ir` 120 | `differential` 120 | `image`† 60, `parse`† 30, `compile`† 30 |

  Weights are shares of a worker's time. † means ASan; everything else is
  built without a sanitizer. Both plans keep debug assertions and overflow
  checks.

### Measured: how the CI ran before

Taken from the jobs' logs, on GitHub's `ubuntu-latest` (Linux, 4 vCPU). These
are not the macOS numbers below. Run 41209256228 (PR #737) and run 41212672734
(PR #738): nine jobs, each one target for `-max_total_time=60` under ASan, from
a 300-input name-ordered sample of the nightly corpus.

| | run 41209256228 | run 41212672734 |
|---|---|---|
| wall clock, first job start to last job end | 7 min 21 s | 5 min 37 s |
| job time, all nine (setup, build, fuzz, counting) | 2,199 s | 2,223 s |
| targets whose initial replay did not finish inside the 60 s | **7 of 9** | — |
| longest initial replay | `differential_source` 206 s (704 inputs) | — |

In run 41209256228 only `parse` and `ir` explored at all. The other seven spent
the step replaying the sample, 54–206 s, and stopped at `INITED`, with no new
input tried. The nightly run is nine jobs of 25 minutes with `-fork=4`: 17,699
and 17,991 job-seconds, 48 minutes of wall clock (runs 40779944847 and
41136050250).

### Measured: the same budget, A and B

Per run, the three seeds (`summary.json`):

| | A | B |
|---|---|---|
| wall clock, corpus copy to summary | 106.5 / 107.0 / 107.0 s | 106.7 / 107.0 / 107.0 s |
| of which: corpus copy / exploring (replays included) / summary | 2.6–2.7 / 103.7–104.2 / 0.1 s | 1.4 / 105.1–105.4 / 0.1 s |
| worker time, and parallelism | 414–415 s, 3.97–3.99 | 419–420 s, 3.98–3.99 |
| initial replay, summed over slots | 34.7 / 31.1 / 32.1 s (7.5–8.4% of worker time) | 15.4 / 15.5 / 15.5 s (3.7%) |
| findings (clean build) | none | none |

The time from a slot's start to libFuzzer's "starting to fuzz" line,
measured to half a second (0.7 s is the floor: the process starting and the
first poll):

| slot | A: replay of budget | B: replay of budget |
|---|---|---|
| `ir` | 0.7 of 53 s | 0.7 of 105 s (each worker) |
| `differential` | 3.2–3.3 of 52 s | 3.3–3.4 of 105 s |
| `image` (ASan) | 5.2–5.3 of 36 s (15%) | 5.3 of 54 s (10%) |
| `parse` (ASan) | 0.7 of 35 s | 0.7 of 26 s |
| `compile` (ASan) | 4.6–4.7 of 32–34 s (14%) | 4.7 of 25 s (19%) |
| `differential_source` | 5.2–5.8 of 52 s (11%) | — |
| `run` (ASan) | 7.3–8.5 of 51 s (16%) | — |
| `ir_mutate` | 1.3–3.2 of 51 s | — |
| `structured` | 2.7–2.8 of 52 s | — |

What the sessions compared (`COVE_FUZZ_STATS_DIR`, merged per slot; min–max
over the three seeds):

| | A | B |
|---|---|---|
| `ir`: inputs / full comparisons of successes with native code / collections under a compiled frame | 7,265–8,207 / 1,768–2,673 / 537–777 | both workers: 24,017–38,252 / 6,490–9,156 / 2,272–2,355 |
| `differential`: inputs / full successes with native code / under a compiled frame | 509–674 / 210–467 / 52–156 | 883–1,342 / 289–771 / 180–225 |
| `image`: inputs / full successes with native code | 5,417–15,027 / 31–35 | 12,009–23,704 / 44–89 |
| `differential_source`: inputs / full successes with native code | 29,488–59,741 / 62–138 | — |
| `ir_mutate`: inputs / full successes with native code | 30,767–60,494 / 15–22 | — |
| mismatches | 0 | 0 |

### Measured: what was reached at 30, 60 and 120 seconds

`scripts/fuzz-budget-reach.py` over each run's final corpora. The replay is
separate from the run: it took 76–152 s a run, 670 s for the six. Features are
the IR instruction keys executed anywhere and in compiled code, the trigger
states, and the 107 type × boundary cells (section 11). `parse` and `compile`
run no program and have none.

| | at 30 s | at 60 s | at 120 s | union of the three runs |
|---|---|---|---|---|
| A: keys / in compiled code / cells / in compiled code | 165 / 139–140 / 102–103 / 95–97 | 165 / 139–140 / 102–103 / 95–98 | 165 / 139–140 / 102–104 / 95–98 | 165 / 140 / 104 / 98 |
| B | 165–166 / 139–140 / 102–103 / 95–98 | 165–167 / 139–140 / 102–103 / 96–99 | 165–168 / 140 / 102–103 / 96–99 | 170 / 140 / 103 / 99 |

**Measured:** almost everything either plan reaches is there at 30 s, and
nearly all of it was already in the starting corpus. Two minutes of
exploration added at most 3 keys and 3 cells to what a run had at 30 s.

What only one plan's union reached: A alone, `string.byte_at.astral.index_len`
(from `structured`) and an overflowing compiled `Arith.Int.Sub`. B alone,
compiled `int.mul.max`, and `Cmp.Bool.{Ge,Lt}`, `Cmp.Tag.Gt` and
`CmpBranch.Tag.{Le,Lt}` executed (all from `ir`).

**What dropping a target loses**, as the features of a run that no other target
of the same run reached (union over the three runs):

- A: `ir` 59; `differential` 3; `structured` 1; `differential_source`,
  `ir_mutate`, `run`, `image` 0 each.
- B: `ir` 70; `image` 28; `differential` 4.

`image` has 28 here and none in A. That is because A's `differential_source`
reaches the same `Dyn*` kinds, `CallHost` and the large-array cell from the
same programs. Measured this way, A's `differential_source`, `ir_mutate` and
`run` slots added nothing that another of its slots did not.

### Measured: four artificial defects, 2 minutes

`scripts/fuzz-budget-mutants.sh --plans "pr-a pr-b" --seeds "1 2 3"` uses
one scratch worktree, which holds one mutant at a time. Per mutant:

- **corpus replay**: every starting input through the mutant's `cove-fuzz`.
  An input counts if it fails there and not on the clean build.
- **new exploration**: a run from the starting corpus with those inputs
  removed. A finding counts only if the mutant's build fails on it and the
  clean build does not.

The **fixed tests** column is `cargo t`, the PR's gate in `ci.yml`. It is
section 10's measurement at `24fb51f`, which was not repeated here, except
for 013, which was measured with #738.

| mutant | kind | fixed tests (`cargo t`) | corpus replay, A's targets | corpus replay, B's targets | exploration A (found / tried; time to it) | exploration B |
|---|---|---|---|---|---|---|
| 002 | native `*` unchecked | `template`, `regressions` | `ir` 2 of 99 | `ir` 2 of 99 | **3/3**; 11.9, 14.4, 11.1 s (`ir`) | **3/3**; 5.0, 6.4, 15.1 s (`ir`) |
| 003 | GC root, innermost compiled frame | `gc_roots`, `native_tier` | `differential_source` 17/54, `image` 5/48, `ir_mutate` 5/43, `ir` 2/99, `differential` 1/39 | `image` 5, `ir` 2, `differential` 1 | **3/3**; 7.5 s (`differential_source`), 9.6, 4.1 s (`differential`) | **3/3**; 13.4 s (`differential`), 1.2, 1.0 s (`ir`) |
| 012 | VM `byteAt` admits = length | **none** | `differential` 4/39, `ir` 2/99 | the same | **3/3**; 4.7, 15.1, 3.6 s (`ir`) | **3/3**; 1.6, 4.4, 1.5 s (`ir`) |
| 013 | verifier, string id = table length | `verify` unit test, `mutate` test, `regressions` (seed) — **3 tests, #738** | `ir_mutate` 1/43 (#738's seed) | **none** (`ir_mutate` is not in B) | 0/3 | 0/3 |

By category, over the 24 trials:

| | A | B |
|---|---|---|
| detected by the fixed tests | 002, 003, 013 | 002, 003, 013 |
| detected by the initial replay | 002, 003, 012, 013 | 002, 003, 012 |
| detected by new exploration | 002, 003, 012 (9 of 12 trials) | 002, 003, 012 (9 of 12 trials) |
| stopped by an unrelated real defect | 0 (one known-like finding, #726's, in 003/A seed 1, reported beside the detection and not counted) | 0 |
| not detected before the budget ran out | 013, 3 trials | 013, 3 trials |
| a build or the runner failed | 0 | 0 |

Every exploration detection was replayed on the clean build, which did not
fail on it. A non-zero exit alone was never counted. The trials' runs took
106.6–108.9 s each. The 24 trials with the four builds took about 70 minutes.

**Inferred, not measured:** 013 is not found by two minutes of exploration
because its trigger is narrow. The names edit is 4 of 256 edit choices. It
must move a name to exactly the table's length, in a program whose other
edits leave nothing else wrong. #738's fixed test shows the edit reaches
that state within the first 57 fixed choices of a small program, so the
state is reachable. A's single 50 s `ir_mutate` slot did not make it, three
times out of three.

### What was not evaluated

- **The other twelve mutants**, including those only one target finds in
  section 10: 008 (`ir` only), 010 and 016 (`differential` only), 006, 007 and
  009. **Configuration C**. **Five seeds.**
- **Component branch coverage** for these runs: the instrumented replay of
  section 11 was not run over them.
- **These plans on Linux.** Every number in "the same budget" is macOS. The CI
  runs the chosen plan on every pull request, and that run is its Linux
  measurement (FUZZING.md, "In CI").
- **The nightly plan's detection power.** It is issue 681's initial candidate,
  adopted as it is: four workers for 15 minutes, `ir` ×2, `differential`, and
  `image`, `ir_mutate`, `differential_source` five minutes each. Its 60
  worker-minutes against the old nightly's 295 runner-minutes is a cost
  measured from the plan, not a comparison of what each finds.
- **Unknown defects.** Four hand-written defects are not a sample of them.

### What was decided

- **The pull request runs B** (`fuzz/plans/pr.plan`). Measured, A and B
  detected the same mutants by exploration (9 of 12 trials each) and reached
  the same in their union (165 against 170 keys, 104 against 103 cells). B
  found the three detected mutants sooner in 7 of 9 paired trials, with a
  median of 4.4 s against 9.6 s. It spent half the initial replay (15.5 s
  against 32 s of worker time). It has six slots, not nine. With no clear
  difference in detection, the simpler plan is the one taken.
- **What B leaves out goes to a longer tier, not nowhere.** `ir_mutate` and
  `differential_source` move to the nightly run's fourth worker. `run`,
  `structured` and every target's long session move to the weekly tier, which
  is the old nightly's nine 25-minute jobs. A starting input B no longer
  replays is still replayed by `cargo t`, if it is a committed seed or
  regression. That holds for #738's 013 seed and for every GC-root seed. What
  B loses outright is A's initial-replay detection through the PR corpus's
  `differential_source` and `ir_mutate` selections (003: 22 more inputs; 013:
  the seed, which `cargo t` also replays).
- **The fixed tests, the regression inputs, the GC-root, capability and
  lifecycle suites are not replaced by exploration.** They run in `cargo t`
  in CI's main job, as before.

## What is not known from these

- How the four mutants' detection would move with a longer budget or more
  seeds: three seeds of five minutes each is a spread, not a distribution.
- Whether `ir` programs ever hold the only reference to an object in an
  interior address across a collection (mutant 004 on `ir`). Section 8 has a
  program that does, written by hand; whether the generator makes one is
  still not measured.
- Anything about the defects nobody has written: the detection of four
  chosen mutants is not a detection rate.
- Whether any of this would notice a defect: a count of what was compared is
  not a count of what a comparison would catch.
- How a session's counts move with its length: every session here is three
  or five minutes on a shared machine.
- The coverage replay's native columns are an inference from the encoded
  VM's run; compiled code is not observed.
