# ADR 0123: The native tier compiles what is hot

- Status: Accepted
- Date: 2026-10-25
- Decides: that a run the default put on the native tier compiles a function
  at the call that makes it hot — its sixteenth call that found no machine
  code, or its first if its body has a loop — and not before the run; that the
  count is taken where a call already asks the tier table for an entry, never
  at a dispatch; that `--backend native`, named, still compiles every function
  before the first instruction; that a function nobody called often enough is
  reported as *not hot*, apart from the refusals; and that the native tier no
  longer verifies a program, or takes its loaded addresses, a second time.
  Recorded for [issue #713](https://github.com/myuon/cove/issues/713)
- Supersedes: [ADR 0055](0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md)'s
  "The first implementation does not have a hotness counter" and "When native
  execution is selected, it may eagerly compile every supported reachable
  function or compile a function on its first call". The rest of ADR 0055
  stands, and in particular "the cached unit is one function and code is
  immutable for the lifetime of its Program", and "It must not add a branch to
  every VM dispatch"
- Supersedes: [ADR 0056](0056-the-first-code-generator-is-the-one-that-was-cheaper-everywhere.md)'s
  "Compile latency is 5.4 µs a function, which makes ADR 0055's 'compile every
  supported reachable function eagerly' cheap enough not to need a hotness
  counter". The choice of the template compiler stands
- Supersedes: [ADR 0122](0122-with-no-backend-a-command-runs-on-the-fastest-tier-this-host-has.md)'s
  Decision 3 sentence "every case the interpreter is compared on is also run
  with `--backend vm`". The end-to-end suite now runs those cases with
  `--backend native` instead; everything else in ADR 0122 stands

## Context

ADR 0122 made the native tier the default on x86-64 Unix and wrote down what
that gave up: "Short runs pay a compile." `cove test` measured it — on the
default tier, `examples/` took 1.80 s against the VM's 1.49 s and
`tools/covefmt/` 4.50 s against 3.34 s. The tier compiled every reachable
function of every test's lowering before the test's first instruction.

Issue #713 measured where that time went (x86-64 macOS, i7-10700K,
`--profile checked`), before changing anything.

**Seventy per cent of it was not compiling.** The compile time of a `cove test`
run was 308 ms in `examples/` and 1,170 ms in `tools/covefmt/`, and
`cove_ir::verify` was 210 ms and 849 ms of it. `PreparedProgram::new` verifies
a program for the encoded VM (ADR 0120), and the native tier then verified the
same program again. Measured by difference, the second verification was the
whole gap between a native run that compiled nothing and the VM, to within the
noise. The subset check then ran `cove_ir::verify::places::loaded_uses` on
every function — 167 ms in `tools/covefmt/` — and the VM's preparation had
already taken that too.

**What was compiling was mostly wasted.**

| calls per (test, function) | 0 | 1 | 2–9 | 10–99 | 100–999 | 1000+ |
|---|---:|---:|---:|---:|---:|---:|
| `examples/` (3,124) | 1,453 | 537 | 647 | 302 | 170 | 15 |
| `tools/covefmt/` (5,698) | 2,608 | 748 | 1,670 | 643 | 29 | 0 |

46% of what `cove test` compiled was never called, and the same functions were
compiled again in every test that reached them — 37 times each, on average, in
`tools/covefmt/`. Emitting cost 15–18 µs a function, not ADR 0056's 5.4 µs —
covefmt's functions are larger than ADR 0056's two — and a mapping per
function 6–7 µs more.

**And one thing was not compilation at all.** `benches/`' `conv_fnvalue`,
`conv_closure`, `conv_capture` and `conv_fresh` run 6–8% slower native than on
the VM. Each is a loop in the outermost frame — which is always the encoded
VM's — calling a small function two million times, and the cost is the
crossing from the VM into compiled code, about 10 ns a call. That callee is
hot by any count, and no compilation policy changes it.

## Decision

### 1. Nothing is done twice

`PreparedProgram::with_native` hands the native tier the answer
`cove_ir::verify` already gave and the loaded addresses the VM's preparation
already took (`cove_native::supported_given`). `cove run`, `cove fmt` and
`cove test` all build their tables over a `PreparedProgram`. `compile_native`
over a bare `Program` still verifies it, because it has nobody to ask.

### 2. A function is compiled when it is hot

`cove_runtime::TierUp` is the policy, and `NativeProgram` keeps a call count
per function:

- **`TierUp::AtCall(n)`**: nothing is compiled when the table is built. A call
  to a function with no machine code counts one; the call that makes the count
  `n` compiles the function, and that call enters the code. A function whose
  body has a loop — an instruction that may transfer control to itself or
  before — is compiled at its first call.
- **`TierUp::Eager`**: every supported function before the first instruction,
  as before.

The default is `TierUp::HOT`, `n = HOT_CALLS = 16`.

The count is taken where the runtime already asks the table for an entry — the
encoded `CALL` arm (`Tiering::crossing`) and the call helpers
(`Bridge::entry_of`), which were already counting the calls a refusal kept in
the VM — and only when the answer was "no entry". A dispatch pays nothing, a
call into compiled code pays nothing, and a call that stays in the VM pays one
load and one compare, or a load, compare and store while it counts. Emitted
code reads the table directly, as before; a cell that was empty is filled once,
and a call that finds it empty goes to the `open` helper, which asks again.

**The loop rule is there because there is no on-stack replacement.** A call
that is running finishes on the tier it began on, so a function called once
with a long loop in it is compiled at that call or never. Measured without the
rule, `benches/keyed` — thirty calls, one of them a long loop — took 2.5 s at
any count above one against 0.87 s, which is the VM's time; with it, 0.87 s at
every count measured.

**Code stays immutable per function.** Every function already had a mapping of
its own; compiling one while others run writes only that mapping, and flips
only it from writable to executable before its entry is published. No page is
writable while a Cove frame stands on it, which is what ADR 0055's one-finalize
rule was for.

### 3. `--backend native` named compiles eagerly

A tier that was asked for by name is being measured or tested — CI's
covefmtBench comparison, the end-to-end corpus, a person reading the full
report — and is given every function before the run, as it was. A run the
default put on the tier is there to be the fastest, and gets `TierUp::HOT`.
`cove run`, `cove fmt`, `cove test` and `cove replay` all follow this rule.
Every test in the repository that tests machine code builds its table eagerly
(`compile_native`, `PreparedProgram::with_native`, `TestBackend::Native(Eager)`),
so none of them is weakened by the policy.

The end-to-end suite, which used to hold the default and `--backend vm` to the
golden files, now holds the default and `--backend native` to them: under this
policy a short case's default run is mostly the VM, and the named run is what
keeps the code generator held to every case. The VM alone is held to the same
files by CI's `--no-default-features` run.

### 4. "Not hot" is not a refusal

`NativeProgram::not_hot` counts the reachable functions that were never asked
about. `cove run`'s one-line summary and full report and `cove test`'s suite
report print it apart from the refusals, which are still only the functions
the code generator turned down. Compile time is still reported apart from
execution: a run that compiles as it goes subtracts it from the execution
figure.

## Measurements

Medians of five interleaved runs, ms, on a machine with other load on it
(other processes held two to three cores busy); the spread is the range.

| workload | before (origin/main) | after, default | VM | after, `--backend native` |
|---|---:|---:|---:|---:|
| `cove test`, `examples/` | 1,722 (1,702–2,305) | 1,465 (1,429–1,925) | 1,415 (1,397–1,429) | 1,480 (1,469–1,516) |
| `cove test`, `tools/covefmt/` | 4,485 (4,477–4,515) | 3,397 (3,354–3,405) | 3,316 (3,231–3,398) | 3,447 (3,407–3,479) |
| covefmtBench, `whole` | 358 | 363 | 1,419 | 355 |
| `cove run cqSample` | 1,151 | 1,150 | 1,657 | 1,120 |
| `cove run covecheck` | 136.9 (133–142) | 124.3 (123–129) | 124.5 (123–130) | 130.3 |
| `cove run life` | 107.9 (103–113) | 101.6 (100–105) | 100.4 (97–102) | 103.0 |
| `cove run reviewPolicy` | 91.7 | 91.9 | 90.7 | 91.2 |
| `cove fmt --check` | 546 | 527 | 1,575 | 525 |

`benches/`' runs with no Cove calls, and those whose calls are all hot, are
unchanged within ±2% before and after. `cove-bench` names its backends
(`ast`, `vm`) and does not reach the native tier.

The count was chosen from this, on the code with Decision 1 and the loop rule
in place (wall seconds, three runs each):

| count | 1 | 2 | 4 | 8 | 16 | 64 | VM |
|---|---:|---:|---:|---:|---:|---:|---:|
| `cove test`, `examples/` | 1.43–1.45 | 1.43–1.55 | 1.43–1.47 | 1.42–1.43 | 1.42–1.43 | 1.42–1.43 | 1.38–1.39 |
| `cove test`, `tools/covefmt/` | 3.33–3.36 | — | 3.34–3.35 | — | 3.33–3.34 | — | 3.22–3.26 |
| `cove run life`, ms | 102 | 100 | 103 | 100 | 98 | 101 | 98 |
| `cove run covecheck`, ms | 127 | 123 | 122 | 124 | 122 | 123 | 124 |

From two up, the long-running programs — covefmtBench, cqSample, keyed,
seqsearch, chars, callback — did not move at any count; the short ones prefer a
higher count and stop improving around eight. Sixteen is the middle of that
plateau.

## What this gives up

- **`cove test` is still not as fast as the VM.** 3.5% in `examples/` and
  2.4% in `tools/covefmt/`, both larger than the run-to-run spread of the
  native runs and inside the spread of the VM's. What is left is the loop rule:
  about 2,300 functions with loops are compiled at their first call over the
  `tools/covefmt/` suite, 85 ms, and most of them are the same few dozen
  functions compiled again in each of 82 tests. Sharing compiled code across
  tests is not possible as the code is emitted: a function's machine code
  holds its slice's function, layout, string and table ids as immediates, and
  every test is a slice of its own (ADR 0022).
- **A long call that is already running stays on the VM**, unless its function
  has a loop. A loop-free function's call is bounded by its length and its
  callees, which are counted on their own.
- **The VM-to-native crossing is not addressed.** `conv_fnvalue` and its
  neighbours are 6–8% slower native than on the VM, before this change and
  after it. Two things would address it, and both are their own decision:
  entering the outermost frame compiled (it is always the VM's today, and in
  these benchmarks it is also refused, for an `AssertFailed`), or a crossing
  cheaper than the one a `CALL` arm makes into `enter` today.
- **A table now changes after it is built.** A cell is filled once and never
  changes after, and every reader reads it atomically; but a reader of the
  figures — `compiled`, `refusals`, `compile_time` — reads them at a moment.

## Alternatives considered

**First-call compilation (`AtCall(1)`).** ADR 0055's other option. It removes
the never-called half and keeps the loop functions compiled at once, and it was
measurably slower than a higher count on every short workload above.

**A back-edge counter.** Without on-stack replacement it would compile a hot
loop for its *next* call, which is no help to the single long call the loop
rule exists for, and it is a counter in the dispatch loop's backedge. The
compiled tier's resume points (ADR 0085) are a plausible route to entering a
running frame compiled; that is on-stack replacement and is not decided here.

**Packing functions into shared pages.** Measured: one `mmap` for many
functions instead of one each saved 2.6 ms of 85 in `tools/covefmt/`, because
the cost is touching fresh pages rather than the call that maps them, and
sharing a page between functions compiled at different times would break W^X.

## Consequences

- `cove_runtime::TierUp`, `HOT_CALLS`, `compile_native_tiered` and
  `PreparedProgram::with_native_tiered` exist; `TestBackend::Native` names a
  policy.
- `Tiered::table` is a slice of `NativeEntryCell`, and `Tiered::missed` is
  asked when a call finds no entry.
- `NativeProgram::refusals` answers a copy, and `NativeProgram::not_hot` and
  `policy` exist.
- `cove_native::supported_given` and `Jit::compile_given` decide the subset
  with loaded addresses the caller already has.
- The end-to-end suite compares the default with `--backend native`.
