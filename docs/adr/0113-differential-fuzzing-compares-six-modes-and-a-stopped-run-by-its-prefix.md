# ADR 0113: Differential fuzzing compares six modes, and a stopped run by its prefix

- Status: Accepted
- Superseded in part by [ADR 0124](0124-the-small-heap-is-sized-from-the-program-s-literals.md),
  which sizes `vm-gc`'s and `native-gc`'s heap as the program's literals and
  2^11 words above them, where Decision 1 made it 2^11 words in all
- Date: 2026-10-24
- Decides: that `crates/cove-fuzz` gains two differential targets,
  `differential` (generated programs) and `differential_source` (sources),
  which run one program on the tree-walking interpreter, the encoded VM, the
  native tier, the encoded VM and the native tier over a small heap, and the
  encoded VM over the program read back from its serialised image, and compare
  every pair; what is compared — the run's outcome, its value or error
  (message, rule, help, refused capability, blamed span) and both console
  streams byte for byte — and what is not; that a run stopped by a limit that
  belongs to the mode (the deadline, the heap's out-of-memory refusal, the
  call-depth limits, cancellation) is compared only by the prefix of what it
  printed; that a NaN is compared by its text, so every NaN is equal and the
  sign of a zero is not; and that agreement is not a safety guarantee.
  Recorded for [issue #681](https://github.com/myuon/cove/issues/681),
  priority 2
- Supersedes: nothing
- Refers to, without superseding:
  [ADR 0112](0112-fuzzing-is-a-library-a-thin-cargo-fuzz-crate-and-a-stable-replay.md),
  whose library, wrapper and replay structure the new targets follow;
  [ADR 0034](0034-one-physical-word-stack.md), which makes the interpreter the
  oracle; [ADR 0055](0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md),
  whose native tier is compared; and
  [ADR 0081](0081-a-run-collects-when-it-has-allocated-its-allowance.md),
  whose pacing decides how small the collecting modes' heap has to be

## Context

ADR 0112 built fuzz targets that run a program on the VM and the native tier
and require only that neither crashes. It left the question the native tier
most needs asked — do the two *agree*? — to priority 2, because comparing them
needed rules it did not have: which outcomes are comparable, how a NaN
compares, which refusals belong to a tier rather than to the program. Its own
analysis of AddressSanitizer found the native tier's emitted machine code
outside ASan's reach, and named the VM-against-native differential as the
measure that would catch most of what ASan cannot see.

There are already two differential harnesses, and both are corpus tests.
`cove-runtime`'s `vm/differential.rs` compares the interpreter and the VM over
hand-written cases, on value and message. `cove-cli/tests/differential.rs`
compares them over every program in `tests/e2e` and `examples`, on value,
structured error including its span, both console streams, the file system
and a normalised trace, with the deadline off. Neither compares the native
tier, and neither is an adversary: every program in them was written to be
correct.

## Decision

### 1. Six modes, every pair compared

A program that checks and lowers is run as `ast` (the interpreter), `vm`,
`native`, `vm-gc`, `native-gc` and `image`. The two `-gc` modes run over a
2^11-word heap: the collector is paced and collects only once a run has
allocated a chunk (8,192 words) since the last collection, or when an
allocation does not fit the capacity, so it is the capacity that has to be
small for generated programs — which allocate less than a chunk — to collect
at all. `image` runs `serial::decode(serial::encode(program))`, the path
`cove fmt` takes. `native` modes are skipped where the tier is unavailable.

Every pair is compared, not only each mode against the oracle: when the
oracle is stopped or skipped, the lowered modes are still held to each other.

Not made modes: the whole-package lowering (not what `cove run` runs, and
costlier than all six runs of a generated program); optimisation settings
(the lowering has none to vary); parked and yielded runs (no host the harness
registers parks or yields — priority 3's lifecycle harness is their place);
the debugger.

### 2. What is compared

The `RunOutcome`; the entry's value rendered, or the error's message, rule,
help, refused capability and blamed span; both console streams, separately,
byte for byte. The span is compared because `cove-cli/tests/differential.rs`
finds that the evaluators agree on it over the corpus. The call chain and the
library sites a fault was moved past are not: the inliner decides which frames
exist on the lowered side. Work counts, collection counts, timings and the
trace are not compared; none is a property of the program.

### 3. A stopped run is compared by its prefix

The deadline is wall-clock and the interpreter is several times slower; each
mode's heap is its own and the interpreter has none; the host's call-depth
limit, the VM's stack region and the interpreter's fixed 256-frame limit each
count frames the inliner decides the number of. So a run that ends by one of
these — or by cancellation — is *stopped*, and two runs at least one of which
is stopped are compared only on their streams: the shorter must be a prefix of
the longer, and a finished run may not be the shorter one. The host-call limit
is not a stop: both evaluators count the same calls. Nor is running out of
memory between two modes over the same capacity — `vm`, `native` and
`image`, or the two `-gc` modes: there, one running out where the other did
not is a leak or a lost collection, and is compared in full.

The interpreter is not run on a program that ran out of memory on the VM's
2 MiB heap: it would make the same allocation from the host's allocator, at
the size the guest asked.

### 4. Determinism and floating point

A run is granted `console`, into two buffers, and `clock`, a fresh virtual
clock per mode. Nothing else is registered. The generator does not write a
`sorted` comparator that contradicts itself, whose permutation the language
does not fix.

A `Float` is compared by its printed text. Every NaN prints `NaN`, so NaNs
are equal regardless of sign and payload, which no Cove operation can read;
`-0.0` prints `-0.0`, so the sign of a zero is compared.

### 5. The generator grows, for both generated targets

`Float` edges and operations, `Option` and `Result` with `?`, a struct and an
enum, function values captured, passed and returned, `eprintln`, the virtual
clock (differential only), and a loop that keeps the small heap collecting.
Every operation is reachable from a helper as well as from `main`, because the
outermost frame always runs encoded. The features it does not generate are
listed, with the reason for each, in `generate.rs`'s module documentation.

### 6. A mismatch is a panic, and replayable on stable

A disagreement panics with the program and every mode's observation, so
libFuzzer saves it and `cargo fuzz tmin` minimises it unchanged.
`cove-fuzz diff-generated` and `cove-fuzz diff` print the same report on
stable and exit non-zero on a mismatch; `fuzz/regressions/differential/` and
`fuzz/regressions/differential_source/` are replayed by `cargo t` like every
other target's. A fixed disagreement also becomes an end-to-end case, because
that corpus is what the other differential harnesses read.

### 7. Agreement is not a safety guarantee

Every mode shares the front end and the standard library's Cove source, and
all but one share the lowering. A bug there is the same bug everywhere and
gives the same wrong answer in every mode. Agreement shows consistency, not
correctness, and says nothing about a corruption no later operation reads.

## Consequences

- `cargo t` gains five tests in `cove-fuzz`'s library and four in its
  regression suite, about a second together.
- The differential targets are the slowest: six runs and a front end per
  input, low single digits of inputs a second under ASan on four forks of a
  loaded machine. A disagreement is not a memory-safety question, so the
  differential is worth running without ASan (`-s none`), several times
  faster; `docs/FUZZING.md` says so.
- CI's fuzz matrix runs the two new targets on every pull request and nightly,
  as ADR 0112's five already are.
- A future mode — a bounds-checked code generator, the whole-package
  lowering, a parked run — is one more entry in `run_every_mode` and one more
  row in the table, with no change to the comparison.
