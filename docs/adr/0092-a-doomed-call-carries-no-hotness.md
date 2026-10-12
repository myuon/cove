# ADR 0092: A call whose continuation is doomed carries no hotness

- Status: Accepted
- Date: 2026-10-13
- Supersedes: [ADR 0070](0070-a-call-whose-continuation-is-doomed-may-be-expanded.md)'s
  Decision 4, **"`hot_functions` is unchanged"**, and nothing else it decided.
  The doomed-continuation predicate, the weighing of every instruction, the
  termination argument and the one refusal sentence all stand
- Refers to: [issue #538](https://github.com/myuon/cove/issues/538), the
  follow-up ADR 0070 named; [PHILOSOPHY](../PHILOSOPHY.md)'s "Preserve the
  performance class"
- Decides: that the inliner's `hot_functions` skips every `Inst::Call` whose
  continuation is doomed, by ADR 0070's own predicate, both inside a loop and
  in a function that is already hot

## Context

The inliner weighs a call site against one of three limits, and which one is
decided by `hot_functions`: a call inside a loop, or in a function a hot site
calls, makes its callee hot, and a hot callee's own calls are hot in turn.
ADR 0070 let a body holding a call with a doomed continuation — a call after
which every path runs into a trap — be expanded, and left `hot_functions`
alone. So such a call still carried hotness: `core.refuse(digitsRefused(d),
...)` inside `Float.parse`'s loop made `digitsRefused` hot, and everything it
called to build the sentence — `std.int.renderInto`, `std.dynamic`'s
rendering, a `std.set.of`'s `duplicateElement` — was weighed against the hot
limit and expanded into code that runs once, on the path that ends the run.

ADR 0070 measured two readings of the follow-up and chose neither: the narrow
one (skip only the calls an expansion creates) changed no program; the broad
one (skip every such call) shrank 43 of 263 programs by 2,043 IR instructions
and added 125 unexpanded standard-library calls. That is a change to generated
code, so it was left to be measured on its own. The maintainer chose the broad
reading on issue #538.

## Decision

`hot_functions` asks `doomed_continuations` of every function once and does not
count a call it marks: such a call makes no callee hot, wherever it stands.
The predicate is ADR 0070's, unchanged — every path on from the instruction
after the call reaches a trap without a return or a backward edge, the call is
not of the function itself, and no argument is an address. Nothing else in the
pass asks a new question; `is_expandable` already admitted these calls, and
the limits still weigh every instruction a body holds.

The reason is the one ADR 0070 gave for expanding around such a call: it runs
at most once per call of the function holding it, and only on a path that stops
the run. Hotness is a claim that a site runs often; for these sites it is
false by construction.

## Measurement

One binary with a temporary switch between the two rules, `--profile checked`,
on `a0cd6b7` (after #629). Static figures are from lowering each program's own
entry, as `cove run` does. Executed counts are `--stats`'.

**Over the repository's 271 programs**, 44 change: −2,143 IR instructions in
all and +126 standard-library `Call` sites left unexpanded. 27 shrink; 17 grow
by exactly 2 instructions each, all of them a `Map.of` or `Set.of` whose
duplicate-key refusal now keeps `duplicateKey` and the `renderInto` beneath it
as functions of their own instead of copying both into `of`
(`std.map.of<Int, Float>` goes from 116 instructions to 60, with
`duplicateKey` at 24 and `std.int.renderInto` at 36 beside it). No program
changes its function count.

| program | IR | std calls | machine code (B) | VM executed | native dispatched / Cove calls |
| --- | ---: | ---: | ---: | --- | --- |
| covefmt | 16,795 = | 92 = | 1,508,119 = | 2,235,806,142 = | 67,139 / 15,591,555 = |
| cq | 10,130 → 10,097 | 96 → 97 | 1,054,072 → 1,051,055 | 261,642,846 = | 760,142 / 18,519,676 = |
| `benches/admission` | 7,766 → 7,635 | 123 → 130 | 763,469 → 753,842 | same distribution | 1,155 / 1,400,238 = |
| `benches/ordering` | 6,933 → 6,736 | 83 → 91 | 658,048 → 641,762 | same distribution | 1,018 / 400,109 = |
| `benches/rendering` | 5,091 → 5,025 | 42 → 45 | 432,643 → 427,420 | same distribution | 516 / 366,216 = |
| `benches/parse_radix_rows` | 1,725 → 1,692 | 48 → 49 | 32,099 → 29,082 | same distribution | 80,606 / 340,050 = |

Every other bench row has identical IR and machine code, and identical counts
on the native tier (`conv_host`, which waits on a real clock, was checked
statically only). "Same distribution": some rows' VM counts vary from run to
run by multiples of 19 instructions under *either* rule — these four, and
`contains`, `indexof` and `frompoint`, whose programs are identical — and the
two rules draw from the same values.
covefmt is untouched because none of its hot paths reaches a refusal helper
through a doomed call; cq's −33 instructions are `std.float.digitsRefused`
(57 → 24), reached from `Float.parse`'s refusal.

Wall time, `execute=` from `--stats`: thirteen interleaved rounds, the two
rules back to back for each row with the order alternating by round, each round
started only once the one-minute load average was under 4 (another worktree was
benchmarking on the same machine); the first round is dropped as cold, and the
paired column is the median of the twelve per-round ratios.

| row | tier | before (ms) | after (ms) | paired median | after faster |
| --- | --- | ---: | ---: | ---: | ---: |
| covefmt | VM | 12,387.5 | 12,381.8 | −0.06% | 8/12 |
| covefmt | native | 3,115.0 | 3,117.7 | +0.18% | 5/12 |
| cq | VM | 1,960.2 | 1,962.8 | +0.37% | 5/12 |
| cq | native | 682.5 | 681.7 | +0.24% | 6/12 |
| `benches/admission` | VM | 192.4 | 191.7 | −0.42% | 7/12 |
| `benches/admission` | native | 60.7 | 60.4 | −0.64% | 8/12 |
| `benches/ordering` | VM | 98.3 | 98.0 | −0.28% | 8/12 |
| `benches/ordering` | native | 29.8 | 29.7 | −0.11% | 8/12 |
| `benches/rendering` | VM | 266.5 | 266.0 | −0.09% | 6/12 |
| `benches/rendering` | native | 118.5 | 118.5 | −0.10% | 7/12 |
| `benches/parse_radix_rows` | VM | 246.7 | 246.7 | −0.05% | 6/12 |
| `benches/parse_radix_rows` | native | 68.9 | 67.9 | −1.34% | 9/12 |

Every row is inside its noise floor (±1% for covefmt, ±2.9% for cq). The change
buys code size on cold paths and costs nothing measurable on the paths that
run, which is what the counts said it would: no row executes a different number
of instructions or makes a different number of calls.

## Consequences

- The refusal paths of `std.float`, `std.int`, `std.dynamic`, `std.map` and
  `std.set` stop being expanded as hot code; what a successful call runs is the
  same instructions it ran before.
- `crates/cove-ir/src/lower/tests/inlining.rs` pins the rule: a call followed by
  a trap inside a loop makes no callee hot, and the same call going round the
  loop does.
- `crates/cove-cli/tests/copies.rs`' ratchet does not move: 7,545 forwardable
  copies, as on `a0cd6b7`.
