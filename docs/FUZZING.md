# Fuzzing Cove

What is fuzzed, how to run it, what counts as a bug, and what is out of reach.
[ADR 0112](adr/0112-fuzzing-is-a-library-a-thin-cargo-fuzz-crate-and-a-stable-replay.md)
records the structure; [issue #681](https://github.com/myuon/cove/issues/681)
is the plan this is priorities 1 and 2 of.
[ADR 0113](adr/0113-differential-fuzzing-compares-six-modes-and-a-stopped-run-by-its-prefix.md)
records the differential targets' rules ([below](#the-differential-targets)).

## The pieces

| Path | What it is |
|---|---|
| `crates/cove-fuzz/` | Every harness, as a stable workspace library. `src/generate.rs` is the program generator; `src/differential.rs` runs one program in every mode and compares them. `src/stats.rs` counts what the differential targets compared. `src/bin/cove-fuzz.rs` replays inputs without cargo-fuzz, and `cove-fuzz stats` counts a corpus. |
| `crates/cove-fuzz/tests/regressions.rs` | Replays every committed input under `cargo t`, and holds the generator to its promise. |
| `fuzz/` | The cargo-fuzz crate: one `fuzz_target!` per harness. Not a workspace member (nightly only). |
| `fuzz/seeds/<target>/` | A few small starting inputs per target, committed. |
| `fuzz/regressions/<target>/` | Minimised inputs that once crashed and no longer do, committed and replayed. |
| `fuzz/regressions/<target>/known/` | Minimised inputs that **still** crash: findings not yet fixed. Committed, not replayed. |
| `fuzz/dictionaries/cove.dict` | Keywords, operators and standard-library names for the source targets. |
| `fuzz/corpus/`, `fuzz/artifacts/`, `fuzz/logs/` | What a session makes. Ignored by git. |
| `scripts/fuzz-seed-corpus.sh` | Builds `fuzz/corpus/<target>/` from the repository's programs and the seeds. |
| `scripts/fuzz.sh` | Runs one target with the limits below. |
| `scripts/fuzz-measure.sh`, `scripts/fuzz-measure-table.py` | Measures a target: N sessions of a fixed length and core budget from one starting corpus, counted, and tabulated ([Measuring a target](#measuring-a-target)). |
| `crates/cove-fuzz/src/coverage.rs` | `cove-fuzz coverage`: which instructions a corpus held and which it executed. |
| [`FUZZ_EVALUATION.md`](FUZZ_EVALUATION.md) | What the harness was measured to reach and detect, and how. |
| `.github/workflows/fuzz.yml` | The pull-request smoke and the nightly run. |

## The targets

| Target | Input | What it drives |
|---|---|---|
| `parse` | UTF-8 source | the lexer and parser |
| `compile` | UTF-8 source | parse, standard-library attach, name resolution, type and uniqueness checking, lowering to IR, IR verification |
| `run` | UTF-8 source | `compile`, then a run on the encoded VM and a run on the native tier |
| `structured` | choices for the generator | a generated well-typed program through `compile` and both runs |
| `image` | the body of a lowered-IR image | `cove_ir::serial::decode`, with a valid header supplied, then `cove_ir::verify` over what decodes, and every mode's run of what verifies ([below](#the-vm-level-targets)) |
| `differential` | choices for the generator | a generated well-typed program run in six modes, every pair compared |
| `differential_source` | UTF-8 source | `compile`, then the same six runs and the same comparison |
| `ir` | choices for the IR generator | a `cove_ir::Program` built by `src/irgen.rs`, `verify`, and every mode's run of it if it verifies ([below](#the-vm-level-targets)) |
| `ir_mutate` | an image body and edits | a lowered program decoded from the body, edited by `src/mutate.rs`, then as `ir` |

Input that is not UTF-8 returns at once from the three source targets. Every
way a source reaches the front end is already a `String`, so such bytes are
not an input the front end can be given, and a lossy conversion would map
every malformed sequence to the same U+FFFD.

The source targets compile a file as the playground does: one module,
`playground`, whose `main` is the entry, with the standard library attached
and the shipped host schemas. A corpus program written as a package of several
modules is refused for an unresolved name, which is a normal outcome.

### What the generator aims at

`structured` exists because a flipped byte in a source is far more often a
syntax error than a new program, so `run` spends most of its time in the
parser. The generator turns the fuzzer's bytes into choices — which
statement, which operand, which boundary value — and every choice yields a
program that type-checks. The module documentation of
`crates/cove-fuzz/src/generate.rs` lists what it covers; in short:

- `Int` leaves drawn mostly from `EDGES` — 0, ±1, the powers of two around a
  byte, a 32-bit word and 2^53, `i64::MIN`, `i64::MAX` — and from a
  collection's `length()` or `byteLength()` plus or minus one;
- `Array` and `Vector` indexing, slicing, `push`, `pop`, `set`, `remove`,
  aliasing, `Vector.filled` with huge or negative counts, `freeze`;
- strings mixing one- to four-byte UTF-8 and a combining mark, through
  `slice`, `sliceBytes`, `codePointAtByte`, `chars`, `split`, `toUtf8`,
  `fromUtf8`, `fromUtf8Lossy`, `fromCodePoint` at the surrogates and past
  U+10FFFF;
- `StringBuilder` — the program-visible face of the standard library's
  `ByteBuffer` — with a zero, negative or huge capacity, `appendSlice` ranges
  that are backwards, past the end or inside a character, `appendByte` outside
  0..=255, and `finish` over bytes that may not be UTF-8;
- arithmetic that overflows or divides by zero, shifts by counts outside
  0..=63, radix and width arguments out of range;
- bounded loops with `break`, helpers that call earlier helpers, recursion to
  a depth the program computes, closures to `map`, `filter`, `fold` and
  `sorted`;
- now and then an operand that leaves — a `return` or a `break` as the
  operand of `+`, of `-`, or of a method's argument — after which the rest of
  the form is unreachable ([issue 698](https://github.com/myuon/cove/issues/698)).

`ByteBuffer` itself cannot be named by a program — `core.bytes*` is the
standard library's alone (ADR 0058) — so it is reached through
`StringBuilder`, string interpolation and the string functions that build
their answers in one.

For `differential` the generator has a second profile
([issue 706](https://github.com/myuon/cove/issues/706)), chosen by the
input's first byte (`generate::is_complete`: 0, 2, 3 or 6 modulo 8, half of
what a fuzzer makes from nothing; no input committed before it begins with
one). The list above is the **hostile** profile, which stops most runs on
purpose — in 33–51% of a session's programs did every mode reach success —
so what a program does after its first stop is compared by nobody. The
**complete** profile keeps the operations and draws operands that let the
run end: small integers, odd divisors, shift counts masked to 0..=63, whole
ranges, capacities, radixes, widths and digit counts a call accepts, ASCII
bytes, non-empty `split` and `replace` needles, no `*=` round a loop, a
recursion depth masked to 0..=15, and an `Err` in `main` read with
`unwrapOr` rather than propagated. Over 300 fixed inputs each (with ADR
0124's small heap), every mode ran to success in 296 complete programs (the
other 4 ran out of memory on the small heap only, or met the interpreter's
deadline) and in 129 hostile ones. A fuzzer seldom changes an input's first
byte, so the complete profile reaches a session through its seeds,
`fuzz/seeds/differential/complete-*`. `ir`'s compiled profile (below) is
the IR target's equivalent: forward jumps, counted loops, initialised
values.

## The differential targets

`differential` and `differential_source` run one program every way this
build can run it and compare the answers — issue #681's priority 2.
`crates/cove-fuzz/src/differential.rs` is the implementation and its module
documentation the long form of what follows.

### The modes

| Mode | What runs | Over |
|---|---|---|
| `ast` | the tree-walking interpreter, ADR 0034's semantic oracle | the checked program |
| `vm` | the encoded VM (`OwnedVm` over a `PreparedProgram`), 2 MiB heap | `cove_ir::lower_entry`'s program |
| `native` | the same with `PreparedProgram::with_native`: the template tier where it compiled a function, the encoded VM where it refused one | the same |
| `vm-gc` | the encoded VM over a small heap — the words the program's literals took on `vm`, and 2^9 (4 KiB) more ([ADR 0124](adr/0124-the-small-heap-is-sized-from-the-program-s-literals.md)) — so a program that allocates collects, and none runs out of memory before its first instruction | the same |
| `native-gc` | the native tier over the same small heap: compiled frames' roots at a collection | the same |
| `image` | the encoded VM over the program `cove_ir::serial` writes and reads back — the path `cove fmt` runs | `decode(encode(..))` |

`native` and `native-gc` are skipped on a host or build without the tier
(anything but x86-64 Unix, or a build without `template`).

Considered and **not** made modes, and why:

- **Whole-package lowering** (`cove_ir::lower`) beside the entry-sliced one.
  It is what a listing and the corpus survey use, not what `cove run` runs;
  it lowers every standard-library function the package declares, which costs
  more than all six runs of a generated program together, and the sliced
  lowering is the one a bug would reach users through.
- **Inliner or other optimisation settings.** There are none to vary: the
  lowering has no options, and the inliner's thresholds are constants. The
  native tier's per-function fallback is the one "setting", and `native`
  exercises both sides of it in every run that has a refused function.
- **Parked and yielded runs** (`run_entry_parkable`, ADR 0084/0085): a run
  parks only at an asynchronous host call or a cooperative yield, and neither
  the console nor the virtual clock is one. Priority 3's lifecycle harness is
  the place for them.
- **The debugger** (`Vm::debugged`): it observes rather than executes.
- **`Vm::new` against `Vm::with_prepared`/`OwnedVm`**: the same machine over
  the same `exec::Prepared`; `vm` and `image` already differ in where the
  preparation came from.

### What is compared

Every pair of modes, on: how the run ended (`RunOutcome` — success, the entry
returning `Err`, a trap, a refused host call, a limit); what it answered (the
entry's value, or the error's message, rule, help, refused capability and the
span it is blamed on); and both console streams, **byte for byte and
separately**. The span is compared because `cove-cli/tests/differential.rs`
finds the evaluators agree on it over the whole corpus. Not compared: the call
chain and the library call sites a fault was moved past (the inliner decides
which frames exist on the lowered side), work counts, collection counts,
timings, and the trace — none of them is a property of the program.

A run that ends in a way that belongs to the **mode** rather than to the
program is *stopped*, and a stopped run is compared only by its prefix: what
it printed on each stream must be a prefix of what the other run printed. The
stops are the deadline (wall-clock; the interpreter is several times slower
than the VM), the heap's out-of-memory refusal (each mode's heap is its own;
the interpreter has none), the call-depth limit, the VM's stack region and the
interpreter's fixed 256 frames (the inliner decides how many frames a call is),
and cancellation. The host-call limit is not a stop: both evaluators count the
same calls, and a run it stops is compared in full. Nor is running out of
memory between two modes over the same capacity (`vm`, `native`, `image`; or
`vm-gc`, `native-gc`): one of a pair running out where the other did not is a
leak or a lost collection.

The interpreter has no heap capacity, so a program that runs out of memory on
the VM's 2 MiB heap is not run on it at all: it would make the same allocation
from the process's own allocator, at whatever size the guest asked.

### Determinism

A run is granted `console` and `clock`. The console writes into two buffers;
the clock is the virtual clock, which moves only when the program sleeps, and
each mode gets a fresh one at the same origin. Nothing else is registered: no
randomness, no real time, no file system, no network. The deadline is the one
wall-clock dependence, and it can only make a run stopped. The generator
leaves out the one construct whose answer the language does not fix — a
`sorted` comparator that contradicts itself.

### Floating point

A `Float` is compared by what the program does with it, which is print it or
branch on it. Printed, it is compared byte for byte, so every finite value is
compared exactly and **the sign of a zero is compared** (`-0.0` prints as
`-0.0`). Every NaN prints as `NaN`, so **NaNs are equal whatever their sign
and payload** — the language's own rule: there is no `toBits`, `format` writes
`NaN` "whichever sign bit it has", and `==` against a NaN is `false`. A NaN's
payload is not observable, and so not compared.

### What the generator covers for it

Beyond the boundary values above (`crates/cove-fuzz/src/generate.rs` lists
both): `Float` edges (both zeros, subnormal and normal minima, the maximum,
2^53 and its neighbour, ±2^63, NaN, both infinities, arbitrary bit patterns)
through arithmetic, `abs`, `sqrt`, `round`, `min`/`max` with NaN on either
side, `exp`, `ln`, `log10`, `sin`, `cos`, `pow`, every comparison,
`Int.toFloat`, `Float.toInt` out of range, `Float.parse` over special
spellings, rounding boundaries and its own `format(17)`, and `format` with a
digit count in and out of range; `Option<Int>` and `Result<Int, Error>` built,
matched, unwrapped and propagated with `?` from helpers and from `main`; a
struct copied and then mutated through the copy; an enum matched and
compared; `fn(Int) -> Int` values captured, passed to and returned from
helpers and handed to `map`; `eprintln` beside `println`; the virtual clock's
`now` and `sleep`; and a loop that churns strings through a vector so the
small-heap modes collect with values live. Every operation is put in a helper
as well as in `main`, because the outermost frame always runs on the encoded
tier.

Not generated, and why, is listed in the same module documentation: `Map`,
`Set`, JSON and the library modules (reached through `differential_source`'s
corpus instead); traits, generics, `dyn` and `var` parameters; tasks (no task
is granted); every other host; identity (`is`); unbounded loops.

### What agreement does not show

Every mode shares the front end, the standard library's Cove source, and —
all but `ast` — the lowering. A bug in any of those is the same bug in every
mode and gives the same wrong answer everywhere. **Agreement shows the modes
are consistent with each other. It does not show that any of them is right,
and it is not a safety guarantee.** A miscompiled access that happens to read
the right value, or a corruption no later operation reads, is invisible to it
as it is to ASan.

## The VM-level targets

`ir`, `ir_mutate` and, since
[ADR 0120](adr/0120-the-vm-runs-only-what-verifies.md), `image` hand the
machine a `cove_ir::Program` with no front end in front of it. The source
targets can only reach programs the lowering writes. These reach the rest,
which is what the machine's contract is about: **for every program, either it
is refused before its first instruction, or a run of it ends in `Ok` or
`Err(RuntimeError)`** — no panic, no abort, no hang past the deadline, no
undefined behaviour.

- `ir` builds a program from the input with `crates/cove-fuzz/src/irgen.rs`.
  It chooses by `Repr` and layout, so that about half of what it writes
  verifies and the rest probes the verifier: 473 of 1,000 fixed inputs
  verified on 2026-10-25, and 580 on 2026-10-26 with the compiled profile
  below; every one of the 87 instruction kinds the generator knows appeared
  in a program that verified, both times.
  It has two profiles, chosen by the input's first byte
  (`irgen::is_compiled`: 2, 3 or 5 modulo 8, three eighths of what a
  fuzzer makes from nothing; no input committed before the profile existed
  begins with one, so each still builds the program it built). A **free**
  program draws every kind, jumps backwards as readily as forwards and does
  its work in its entry — what the verifier and the deadline are tested on.
  A **compiled** program ([issue 723](https://github.com/myuon/cove/issues/723))
  is shaped for the native tier: helpers that draw only the kinds the tier
  compiles, hold no host, task or scope slot, jump only forwards, call only
  later helpers and may close a counted loop of their own; loads and stores
  through addresses of known locations, at those locations' layouts; and an
  entry that calls the helpers from a counted loop — so the calls cross from
  the encoded VM into machine code — and folds what each answers into the
  `Int` it returns. A refused entry argument is drawn only by a wild program
  now: it used to be one argument in eight, and every argument once the
  input ran out, which kept a quarter of the programs that verified from
  starting.
- `ir_mutate` reads an image body from the front of the input and applies the
  edits in the bytes after it (`crates/cove-fuzz/src/mutate.rs`): an operand,
  a layout, an id or a jump target changed. Its seeds are every program in
  the repository, so what it edits is what the lowering writes.
- `image` decodes a body, verifies it, and runs what verifies the way `ir`
  does.

A program that verifies is run in every mode — `vm`, `vm-again`, `vm-gc`,
`native` and `native-gc` where the host has the tier, and `image` — with
plausible arguments for the entry's parameters, under a 25 ms deadline
(`ir::IR_DEADLINE`): a generated body jumps backwards as readily as forwards,
and many of them loop until they are stopped. The modes are held to
[ADR 0113](adr/0113-differential-fuzzing-compares-six-modes-and-a-stopped-run-by-its-prefix.md)'s
agreement. A program `verify` refuses must be refused by the machine as
well, with the verifier's sentence, and not run.

Replaying and inspecting, without cargo-fuzz:

```console
$ cargo build --profile checked -p cove-fuzz
$ ./target/checked/cove-fuzz ir-show <input>          # the listing, the entry and every mode's run
$ ./target/checked/cove-fuzz ir-mutate-show <input>
$ ./target/checked/cove-fuzz image-show <input>
$ ./target/checked/cove-fuzz ir-stats 1000             # how the generator does over fixed inputs
$ ./target/checked/cove-fuzz ir-seed 511 out           # the input `ir-stats` made for one seed
```

`ir-stats` is the measure of the generator. It reports the share that
verifies, how the verified runs ended, the most common refusals, which
instruction kinds never appeared in a verified program, and every finding
with its seed. It also stops on a program that runs past 20 seconds, which is
how the `verify` hang below was found.

### What they found

Reading the verifier against the machine, and then running these targets,
found the gaps ADR 0120 records. Each has a regression input under
`fuzz/regressions/image/` or `fuzz/regressions/ir/`, and the `image` ones are
built by `crates/cove-fuzz/tests/vm_findings.rs`:

- a field read as a family its object does not hold (a `String`'s text read
  as an object address);
- `lock` on an object that is not a cell;
- a closure called with arguments that are not its function's parameters;
- a dynamic view forged from two `Int`s and a reference;
- a layout whose words disagree with its fields;
- the address of an `Int` slot loaded through as a reference;
- `verify` itself running for minutes on a place offset round a loop
  (`ir-stats` seed 91);
- `vm` and `vm-again` disagreeing over a forged view followed into
  heap-dependent words (`ir-stats` seed 511).

The ASan sessions that found the last four, on 2026-10-25, on an x86-64 Mac
running under Rosetta, with `-fork` over 16 cores shared between the targets
(so read the rates as a floor):

| Target | session | inputs | crashes, timeouts | what it found |
|---|---|---|---|---|
| `ir` | 124 inputs, then stopped | 124 | 1 crash | `await` at another width than the task answers |
| `ir` | 1,218 s, `-fork=6` | 295,704 | none | |
| `image` | 602 inputs, then stopped | 602 | 1 crash | a span naming a file the image does not hold |
| `image` | 1,241 s, `-fork=4` | 298,871 | 1 timeout | the backward switch edge |
| `image` | 1,197 s, `-fork=6` | 303,496 | none | |
| `ir_mutate` | 446 s, then stopped | 335,604 | 1 crash | the same span |
| `ir_mutate` | 1,207 s, `-fork=5` | 442,262 | 9 timeouts | a backward switch edge compiled with no poll |
| `ir_mutate` | 1,209 s, `-fork=7` | 448,583 | none | |

A session with `-fork` stops at its first crash unless it is passed
`-ignore_crashes=1`, which is what the later sessions were given; each crash
and timeout is replayed and fixed before the next. A timeout is replayed alone
before it is believed: on a machine running three sessions and a build, an
input that takes a second can take ten, and only one that reproduces alone
is a finding. One of the nine was replayed alone under ASan and took twelve
seconds; after the fix all nine end at the deadline in every mode.

## What counts as a bug

A harness discards every answer the toolchain is allowed to give:

- **a refusal** — diagnostics from the parser, the checker or the lowering;
- **a stopped run** — any `Err(RuntimeError)`: a guest trap (overflow,
  division by zero, a range `sliceBytes` or `appendSlice` refuses, invalid
  UTF-8 at `finish`), the deadline, the call-depth or host-call limit, the
  heap's "this run has no memory left", a refused host call, a refused
  `spawn`;
- **an unreadable image** — `Err(Unreadable)`.

A **finding** is anything else:

- a Rust panic, including `debug_assert!` and integer overflow in the
  toolchain itself — the fuzz build is optimised with both checks on, as
  `[profile.checked]` is;
- an abort, a segmentation fault, a stack overflow;
- an AddressSanitizer report, a LeakSanitizer one included (Linux only): a
  run that leaves memory behind leaves a reference behind, which
  [ADR 0114](adr/0114-what-a-failed-run-leaves-behind.md) says a run may not
  do, and [ADR 0115](adr/0115-the-interpreter-reclaims-a-cell-cycle-when-its-run-ends.md)
  is the one this has found so far;
- an input that runs past libFuzzer's `-timeout=10` (a "timeout" artifact);
- an input that pushes the process past `-rss_limit_mb` or makes one
  allocation over `-malloc_limit_mb` (an "oom" artifact): the run's heap is
  capped, so this is memory the runtime allocated outside it, sized by the
  guest;
- in `structured` and `differential`, a generated program the front end
  refuses. That panics with the program's text, and is a generator bug or a
  checker bug;
- in `differential` and `differential_source`, two modes that disagree under
  [the rules above](#what-is-compared). That panics with the program and every
  mode's observation;
- in `ir`, `ir_mutate` and `image`, a run of a verified program that ends
  any other way than `Ok` or `Err`, two modes that disagree, or a program
  `verify` refuses that the machine runs anyway.

**An infinite loop is not a bug.** A guest may loop forever; the deadline
stops it after 150 ms and the run answers `Err`. libFuzzer's timeout is ten
seconds, so it is reached only by a loop the runtime's deadline cannot see —
one that never reaches a safepoint — which is a bug in the runtime.

## The limits

The harness, per run (`crates/cove-fuzz/src/lib.rs`):

| Limit | Value | Why |
|---|---|---|
| deadline | 150 ms | an infinite loop becomes a stopped run |
| call depth | 512 | deep recursion stops on a limit the host chose |
| host calls | 4096 | a printing loop stops before it spends the deadline |
| tasks | 0 | a task is a thread; `spawn` is refused |
| heap | 2^18 words (2 MiB); 2^11 words in `vm-gc` and `native-gc` | a huge allocation meets the heap's refusal well inside the fuzzer's memory limit |
| grants | `console` only, into a sink; in the differential targets into two buffers, and the virtual clock | no file, network, process, environment, database or real-clock host is registered, and the grant source is sealed |

The fuzzer, per session (`scripts/fuzz.sh`):

| Flag | Value | Why |
|---|---|---|
| `-max_total_time` | the second argument, default 60 s | session length |
| `-timeout` | 10 s | an order of magnitude over six runs' deadlines and a front end under ASan |
| `-rss_limit_mb` | 4096 | ASan's shadow and quarantine are most of a session's 500–800 MB |
| `-malloc_limit_mb` | 1024 | one huge host-side allocation is reported where it is made |
| `-max_len` | 64 KiB source, 4 KiB generator choices (`ir` included), 256 KiB image (`ir_mutate` included) | the largest program in the tree fits |
| `-dict` | `fuzz/dictionaries/cove.dict` | source targets only |
| `-len_control=0` | `structured`, `differential` and `ir` | their choices are read from the front; a short input is a short program |
| leaks | on | LeakSanitizer's per-input check names the input that leaked, and its check at exit catches the rest; there is no suppression file, because nothing the process keeps on purpose is reported |

Nothing in a run depends on the wall clock except the deadline, and that can
only turn a long run into a stopped one. A run does not touch the network or
the file system. Two inputs are not independent in one sense: libFuzzer runs
every input in one process, so the runtime's thread-local and global caches
(the standard library's parse, for one) persist between them, as they do
between the files `cove fmt` formats.

## Running it locally

Once:

```console
$ rustup toolchain install nightly
$ cargo install cargo-fuzz --locked --version 0.11.2
```

Then:

```console
$ scripts/fuzz-seed-corpus.sh             # every target; or name some
$ scripts/fuzz.sh run 300                 # five minutes of `run` under ASan
$ scripts/fuzz.sh structured 600 -- -fork=8
```

`scripts/fuzz.sh` seeds the target's corpus itself if it does not exist, logs
to `fuzz/logs/<target>.log`, and leaves any crash input in
`fuzz/artifacts/<target>/`. Everything after `--` is passed to libFuzzer.

`cargo fuzz build` builds with optimisations, debug assertions, overflow
checks and AddressSanitizer by default (pass `-O` to drop the two checks,
`-s none` to drop ASan; do neither for a session whose findings you want to
trust). The `template` feature is on in `fuzz/Cargo.toml`, so `run`,
`structured` and the differential targets reach compiled code on x86-64
Unix; elsewhere the native run answers ADR 0055's capability diagnostic and
only the VM runs.

A disagreement is not a memory-safety question, so a differential session
does not need ASan to find one, and is several times faster without it:
`cargo +nightly fuzz run -s none differential …` from `fuzz/` (or
`scripts/fuzz.sh` for the ASan build). Run the ASan build too, for a while:
the six runs reach code the other targets do not.

### Speed

Under ASan on an x86-64 Mac, one process, measured 2026-10-24 over the seeded
corpus (the session `fuzz/regressions/README.md` records):

| Target | inputs a second | edges (`cov:`) after the session |
|---|---|---|
| `parse` | 308 | 4,503 |
| `compile` | 27 | 44,602 |
| `run` | 35 | 58,733 |
| `structured` | 6 | 38,621 |
| `image` | 510 | 2,184 |

The differential targets, measured the same day with `-fork=4` on a machine
loaded by a second build (load average 10–18), so read the ratios rather than
the figures:

| Target | build | session | inputs | inputs a second, all forks | edges (`cov:`) |
|---|---|---|---|---|---|
| `differential` | ASan | 664 s | 4,416 | 6.7 | 48,382 |
| `differential` | `-s none` | 569 s + 617 s | about 15,300 + 14,158 | 27, 23 | 31,336 (not comparable with ASan's count) |
| `differential_source` | ASan | 240 s | 11,584 | 50 | 66,212 |

`compile`, `run` and `structured` are slow because every input type-checks the
whole standard library again: about 15 ms an input without ASan, several times
that with it. That is the front end's cost and not the harness's. `-fork=N`
or `-jobs=N` is the way to spend more cores on it.

## What a session compared

A differential session that found nothing says as much as what it compared,
and libFuzzer's own numbers — executions, coverage edges — do not say that. A
program every mode stopped on before printing anything agrees trivially.
So the differential targets and the IR targets (`differential`,
`differential_source`, `ir`, `ir_mutate`, `image`) classify every input,
before they panic on it, and count ([issue #704](https://github.com/myuon/cove/issues/704);
`crates/cove-fuzz/src/stats.rs` is the implementation and its module
documentation the long form). The counts are kept apart — they overlap — and
nothing here is a score.

Four readings the numbers do **not** allow:

- **`prefix` is not "invalid".** A program counts as `prefix` when *at least
  one pair* of its modes was compared by prefix; every other pair of the same
  program may have been compared in full. The `pairs` key says which.
- **`full` is not "ran to completion".** A full comparison compares how two
  runs ended, whatever that was: two runs that trapped at the same first
  statement are compared in full. `pairs.<a|b>.full.<kind>` and
  `comparisons.full_success` say how the runs ended.
- **Native execution and a collection under a compiled frame are not a
  tested root.** They say compiled code ran and the collector asked a compiled
  frame for its roots; not that a reference was live across the collection or
  read after it. [Roots of compiled frames](#roots-of-compiled-frames) is the
  test for that.
- **Executions are not coverage of the input space.** A session counts every
  execution, the corpus's re-runs and repeated inputs included.

### The metrics

| Key | Counts |
|---|---|
| `inputs`, `targets.<name>` | inputs, per target |
| `stages.utf8` … `stages.ran` | how far through the front end inputs got, each stage counting the inputs that passed it: `utf8` (every `differential` input is), `parsed`, `checked`, `lowered`, `verified` (`cove_ir::verify` accepts the lowered program), `entry` (it has `playground.main` — the source targets' form of "the arguments were accepted", since `main` takes none), `ran` (every mode ran and was compared). A program that does not verify is still run, and every lowered mode refuses it alike, so `ran` may exceed `verified` |
| `mismatches` | findings: two modes disagreed, the image did not read back, or (for `differential`) the front end refused a generated program. Nought in a passing session |
| `crashes` | inputs `cove-fuzz stats` caught panicking anywhere else (a session reports those as crashes and does not get to count them) |
| `comparisons.full` | programs whose every pair of modes was compared in full |
| `comparisons.prefix` | programs with at least one pair compared **by prefix only** — one of the pair stopped in a way that belongs to its mode (the deadline, a heap of another size running out, a call-depth limit) |
| `comparisons.no_entry` | programs with no `main`: every mode refuses them alike before their first instruction, so they are neither. `full + prefix + no_entry = ran` |
| `comparisons.empty_prefix` | of `prefix`, programs whose **every** prefix-compared pair had an **empty common prefix**: a pair compared by prefix checks that the shorter of each console stream is a prefix of the longer, so what it compares is the shorter stdout and the shorter stderr, and its common prefix is empty when both are empty — it compared no output at all |
| `comparisons.pairs_full`, `pairs_prefix`, `pairs_empty_prefix` | the same, counted in pairs of modes rather than programs |
| `comparisons.{full,prefix}_with_native`, `…_with_gc_while_compiled` | the `full` or `prefix` programs that also count under the `programs` key of that name |
| `comparisons.full_success`, `full_success_with_native` | of `full`, the programs every one of whose runs ended in **success** — `RunOutcome::Success`, the entry returning a value that is not an `Err` — and of those, the ones whose `native` mode entered compiled code: the only full comparisons in which every mode ran the program to its end |
| `pairs.<a\|b>` | per pair of modes, named in the order the modes are declared (`ast`, `vm`, `native`, `vm-gc`, `native-gc`, `image`, `vm-again`), whichever ran first: `full.<kind>` — compared in full, by how both runs ended: `success`, `error` (the entry returned `Err`), `trap` (`RunOutcome::Invariant` other than the heap running out), `limit` (out of memory on a shared heap, the host-call, concurrency or call-depth limit, the deadline), `other` (a refused host call, cancellation), or `mixed` when the two ended in different kinds, which is always also a disagreement; `full_success_with_native`, compared in full, both succeeded, and one of the two entered compiled code; `prefix.nonempty` and `prefix.empty`; `skipped.<reason>`, one of the two did not run; `no_entry`; `mismatch`, the two disagreed. Each count is read from the comparison the harness made — `differential::compare_pair` decides both the verdict and the basis, and the report keeps every pair's — so the statistics cannot count a different comparison from the one that was made |
| `stages.started`, `not_started.<reason>` | the `vm` run executed an instruction (`OwnedVm::instructions() > 0`): the entry and its arguments were accepted and the run began; otherwise the error's first sentence, numbers and names left out |
| `stages.built`, `stages.verified`, `stages.entry`, `stages.ran`, `refusals.<rule>` | **the IR targets' stages**, in place of the front end's: `built`, the input made a program (generated by `ir`; decoded by `image`; decoded and edited by `ir_mutate` — an input that does not decode counts under none); `verified`, `cove_ir::verify` accepted it, and `refusals.<rule>` the rule it refused by otherwise (the machine is held to refusing it too in `ir` and `ir_mutate`; `image` does not run it); `entry`, the program has the function it is entered by; `ran`, every mode ran and was compared |
| `programs.native_ran` | the `native` mode entered compiled code (`native_calls > 0`); without that, `native` compared the encoded VM with itself |
| `programs.vm_gc_collected`, `native_gc_collected` | that mode collected at least once |
| `programs.gc_while_compiled` | some mode collected **while a compiled frame was on the stack** — `OwnedVm::collections_while_compiled`, a runtime counter — which is what tests a compiled frame's roots. A run that collected and ran compiled code at different moments did not. `collections_while_compiled` is the number of such collections, summed |
| `modes.<mode>` | per mode: `ran`; `ended.<outcome>` for runs that did not stop, by `RunOutcome` name (`success`; `error`, the entry returned `Err`; `invariant`, a trap; …); `stopped.<reason>`; `end.<kind>`, the five kinds of `pairs`; `started`; `skipped.<reason>`; `entered_compiled_code`, `collected`, `collected_while_compiled` |

The file is JSON: objects, with non-negative integers at the leaves; two files
merge by adding leaf to leaf. Every key of the first version (#717) means what
it meant; the rest are new keys in the same tree, so an old file merges with a
new one and the new keys read nought in it — which is "not counted", not
"none".

**What a program holds is not what it executes.** `stages.verified` and
`ir-stats`' kinds say which instructions a program *contains*; one after a
trap, behind a loop that never ends or in a function nobody calls never runs.
`cove-fuzz coverage <target> PATH...` measures the second thing, on a replay
of its own because it watches every instruction (`crates/cove-fuzz/src/coverage.rs`):
for each program that verifies, every instruction it holds is counted under
its kind — with the operation for arithmetic and comparisons, `Arith.Int.Mul`,
`Cmp.Int.Lt` — as `present`, and again as `present_compiled` if it is in a
function the native tier compiles; then the program runs on the encoded VM
with a debugger that records every instruction it executes, halted at the
count the unwatched run reached, and those are `executed`, and
`executed_compiled` when the function is compiled and is not the entry (the
outermost frame always runs encoded). For the instructions whose answer a
plausible wrong implementation would get wrong only in one state, it also reads
the operands (up to 256 times a site) and counts `<key>:<state>` as
`triggered` and `triggered_compiled`: `Cmp.Int.<op>:equal`, two equal
operands, where `<` and `<=` differ — and the same for the forms the code
generator emits through the same template, `CmpImm.<op>`,
`CmpBranch.Int.<op>` and `CmpImmBranch.<op>`, which a counted loop's test
usually is; `Arith.Int.<op>:overflow` and
`ArithImm.<op>:overflow`, an addition, subtraction or multiplication whose
exact answer is not an `Int`, where checking and wrapping differ. The counts
are of programs. What it
cannot see is compiled code itself: `executed_compiled` is the inference that
the native modes ran, as machine code, what the VM ran in a function the tier
compiled.

The runtime counter costs one add and one subtract each time Rust enters
compiled code (`run_frame`, the one body every such entry goes through) and
one comparison per collection; a call compiled code makes to compiled code
directly pays nothing, and the dispatch loop is untouched.

### Producing them

**In a session.** Set `COVE_FUZZ_STATS_DIR`, and every process the target runs
in writes `$COVE_FUZZ_STATS_DIR/<target>-<pid>.json` after its first input,
every 256 inputs, at least every second, and before an input that is
about to panic — by a temporary file and a rename, so a reader never sees half
a file. It is written as it goes because libFuzzer does not promise to run
anything at exit and kills a `-fork` child. Unset, nothing is counted and no
file is touched.

```console
$ COVE_FUZZ_STATS_DIR=$PWD/fuzz/stats/session scripts/fuzz.sh differential 600 -- -fork=4
$ cargo run --profile checked -p cove-fuzz --features template -- stats merge fuzz/stats/session > fuzz/stats/session.json
```

A session counts **executions**: libFuzzer runs its corpus first, and each
`-fork` child runs part of it again, so an input is counted every time it runs
— which is why a session's count exceeds the executions libFuzzer's own
progress line reports. A `-fork` child killed between two writes loses at most
a second of counts; at ten seconds, as #717 first wrote them, a 20-second
`-fork=2` session counted 12 of libFuzzer's 240 executions.

**Over a corpus.** `cove-fuzz stats <target> PATH...`, for any of the five
targets, replays every file under the paths once — directories recursively, `*.md` and
dot-files left out — and counts it, a finding included, without stopping. It
prints the JSON on stdout and the summary on stderr, lists each finding, and
exits non-zero if there was one.

```console
$ cargo run --profile checked -p cove-fuzz --features template -- stats differential fuzz/corpus/differential > corpus.json
```

The summary leads with one sentence:

```text
N programs compared in full, M of them with native execution, K with a collection while compiled code was live; P prefix-only, Q of those with an empty common prefix (Q/P); … mismatches and … crashes over … inputs.
```

### Measuring a target

`scripts/fuzz-measure.sh <target> <seconds> --runs N --fork F` is the
measurement the numbers below are made with, so that two of them can be
compared: N sessions of the same length and core budget (`-fork=F`), each from
a fresh copy of the same starting corpus — the one `scripts/fuzz-seed-corpus.sh`
makes, or `--corpus DIR` — with libFuzzer seeds 1 to N, counted in session and
merged. It builds without AddressSanitizer unless given `--asan` (a count and
a disagreement are not memory-safety questions, and the build without it is
several times faster), and writes everything under `fuzz/measure/`, which is
not committed: `meta.txt` with the revision, the tree's state, the
toolchains, the host, the load average and each session's exact command line;
per session `session.json` and its summary, `result.txt` (wall time,
executions, findings), libFuzzer's log and any finding. `--replay` counts each
session's grown corpus as well, once an input; `--until-finding` stops a
session at its first finding and records when, which is the mutation
experiments' time-to-detection ([FUZZ_EVALUATION.md](FUZZ_EVALUATION.md)).

```console
$ scripts/fuzz-measure.sh differential 300 --runs 3 --fork 4 --replay
```

### The native tier on the command line

`--features template` matters on the command line above: `-p cove-fuzz` alone
is not a workspace build, so without it nothing turns the native tier on, and
`native` and `native-gc` are skipped.

### What they say

Measured 2026-10-25 on an x86-64 Mac, when the counts landed. Report these
columns for a session from now on, beside the speed table's.

| Run | inputs | full | of which native ran | of which a collection under a compiled frame | prefix-only | **of which empty prefix** | no entry | mismatches |
|---|---|---|---|---|---|---|---|---|
| `differential` session, `-s none -fork=4 -ignore_crashes=1`, 600 s | 15,178 executions | 12,781 | 12,595 | 2,108 | 2,397 | **2,009 (83.8%)** | 0 | 0 |
| `differential`, 500 random inputs (seed 704, 64 B–4 KiB), `cove-fuzz stats` | 500 | 452 | 444 | 41 | 48 | **43 (89.6%)** | 0 | 0 |
| `differential`, the 8 committed seeds and regressions | 8 | 8 | 8 | 4 | 0 | 0 | 0 | 0 |
| `differential_source`, the seeded corpus (413 repository programs and seeds) | 413 (297 ran) | 193 | 155 | 27 | 55 | **30 (54.5%)** | 49 | 0 |
| `differential_source`, the 3 committed seeds and regressions | 3 | 3 | 3 | 1 | 0 | 0 | 0 | 0 |

(The repository corpus's 413 inputs: 4 do not parse, 105 do not check — a
package's module read as one file meets an unresolved name — and 7 do not
lower; 49 of the 297 that run have no `main`.)

What the numbers say:

- **A prefix-only comparison is almost always an empty one.** In the session,
  13.2% of the programs (2,009 of 15,178) were compared only by prefix and the
  prefix held nothing — 84.8% of the prefix-compared pairs (14,902 of 17,572)
  compared no byte of output. Nearly all of them are `vm-gc` and `native-gc`
  running out of their 2^11-word heap (2,370 of each), most before the first
  line was printed: the standard library's literals take about 1,200 of those
  words before the first instruction. Those programs were compared in full
  between `vm-gc` and `native-gc`, which share the heap, and in full among the
  other four; it is the small-heap pair against the rest that compared
  nothing.
- **Compiled frames' roots are tested, and mostly by `native-gc`.** 17.3% of
  the session's programs (2,619) collected while a compiled frame was on the
  stack, 25,479 such collections, against 19.9% (3,023) that collected at all
  on the small heap — so most collections the small heap forces happen under
  compiled code.
- **The native tier ran in 97% of programs** (14,742), so `native` is not
  comparing the encoded VM with itself.
- In-session counts lag libFuzzer's: it reported about 16,100 executions, and
  a `-fork` child killed between two writes loses what it counted since the
  last one.
- The first session, without `-ignore_crashes`, stopped 131 s in on a
  generator bug (`fuzz/regressions/differential/generator-byte-range-nests-too-deep`),
  which it had counted before the panic: one mismatch, one input refused by
  the parser.

## Roots of compiled frames

`programs.gc_while_compiled` says a collection happened while a compiled
frame was on the stack: the collector *asked* a compiled frame for its roots.
It does not say the frame held a reference that was still needed, nor that
anything read that reference afterwards — and a root the collector missed is
only wrong if both are true. `crates/cove-fuzz/tests/gc_roots.rs` holds twelve
programs to that stronger standard, one shape of root each:

| Seed (`fuzz/seeds/differential_source/`) | What a compiled frame holds across the collections |
|---|---|
| `gc-root-string` | a string it built, while the same frame allocates |
| `gc-root-string-callee` | a string it built, while a compiled callee allocates |
| `gc-root-vector` | a `Vector<String>` of strings it built |
| `gc-root-struct` | a struct with a `String` and an `Array<Int>` field |
| `gc-root-closure` | a closure whose environment holds a string |
| `gc-root-frames` | a string in each of three nested frames; the innermost allocates |
| `gc-root-map` | a `Map<String, String>` |
| `gc-root-enum` | an enum whose cases carry a `String` |
| `gc-root-option` | an `Option<String>` and a `Result<String, Error>` |
| `gc-root-closure-closure` | a closure that captures a closure that captures a string |
| `gc-root-recursion` | a string in every frame of a recursion twelve deep; the deepest allocates |
| `gc-root-callee-holds` | a string the innermost frame builds and holds while it allocates, then returns, and one its caller holds across the call |

Each program builds the value, holds it in a function the native tier
compiles, allocates about 600 short strings in a loop (on the small heap of
`vm-gc` and `native-gc` — the program's literals and 2^9 words more — several
collections), and then reads the value
into the line it prints. The test does not assume any of that happened; it
measures each step, on the native tier over the small heap:

- **the holder ran as machine code**: it is a function of its own after
  lowering (not written into its caller), the native tier compiled it, and
  the encoded VM executed less than a tenth of the instructions on the native
  run that it executed on `vm` — the loop's thousands of instructions ran
  elsewhere;
- **the collections happened while the value was live**: the run collected
  under a compiled frame, and the same program with the loop emptied
  (`while i < 0`) collects fewer times under a compiled frame — none, as
  measured — so the counted collections are the loop's — after the value was built, before it was read;
- **the read happened and was checked**: the printed line is compared byte for
  byte with the text in the test, which the tree-walking interpreter — no
  heap capacity, no collector — prints too, and every line carries the held
  value's contents.

The seeds are also in the `differential_source` corpus, so the fuzzer starts
from these shapes, and `tests/regressions.rs` replays them in every mode.

The first six are issue 681's, the next six issue 725's. One shape no
source program can make has a test of its own: an object kept alive by
nothing but an address into it. The lowering keeps the object in a reference
slot for as long as an address into it lives, so dropping the interior root
(ADR 0120's `Space::containers`) leaves the object rooted anyway from source.
`crates/cove-fuzz/tests/interior_roots.rs` writes the program as IR — take
`&obj.v`, clear the slot holding `obj`, allocate three thousand objects of
the same size, load through the address — and requires every mode to answer
what was stored, `vm-gc` after collecting and `vm` without. Its image is
`fuzz/seeds/image/interior-root`, so the `image` target starts from it. The
holder is the entry, on the encoded tier, because the native tier does not
compile `AddrOfField`: no compiled frame holds an interior address.

What this does not show is that a dropped root would be *noticed*: a
reference wrongly freed is visible only if its memory is reused and
overwritten before the read. The loops allocate strings of the held values'
size for that reason, and the mutation experiment in
[FUZZ_EVALUATION.md](FUZZ_EVALUATION.md) measures whether it is enough.

## What it detects: artificial defects

`fuzz/mutants/*.patch` are defects written into the runtime and the code
generator on purpose, to measure whether the harness notices them — by the
dedicated tests, by replaying an existing corpus, and by a new session —
and `scripts/fuzz-mutant-experiment.sh <name>` is the experiment.
[FUZZ_EVALUATION.md](FUZZ_EVALUATION.md), section 4, has the method and the
results. A mutant is never applied to this tree: `scripts/fuzz-mutant.sh
apply` makes a scratch worktree under `fuzz/measure/` and applies it there,
and `scripts/fuzz-mutant.sh check`, a step of CI's main job, fails if a
mutant's marker is anywhere in the tree under test or a patch no longer
applies.

```console
$ scripts/fuzz-mutant.sh list
$ scripts/fuzz-mutant.sh apply 002-native-int-mul-unchecked
$ scripts/fuzz-mutant.sh run 002-native-int-mul-unchecked -- cargo test --profile checked -p cove-native --features template --test template
$ scripts/fuzz-mutant.sh revert 002-native-int-mul-unchecked
```

An exploration's finding is replayed on the clean build before it is
counted: one that fails there too is the toolchain's own, which is how
[#720](https://github.com/myuon/cove/issues/720) and
[#721](https://github.com/myuon/cove/issues/721) were found.

## Triage, minimise, keep

1. **Reproduce.** `cargo +nightly fuzz run <target> fuzz/artifacts/<target>/crash-…`
   from `fuzz/` replays one input under ASan. On stable, without cargo-fuzz:

   ```console
   $ cargo run --profile checked -p cove-fuzz -- <target> path/to/input
   ```

   For a source input, `cove-fuzz explain FILE` prints how it ends on each
   tier and `cove-fuzz check FILE` its diagnostics; for a `structured` input,
   `cove-fuzz generate FILE` prints the program it generates and
   `cove-fuzz explain-generated FILE` how that ends.

   For the differential targets, `cove-fuzz diff-generated FILE` (a
   `differential` input) and `cove-fuzz diff FILE` (a source) print the
   program, each mode's outcome, answer and both streams, which modes were
   skipped and why, what the native tier compiled, and `agree` or the first
   `MISMATCH`. They exit non-zero on a mismatch, so a shell loop or a
   hand-rolled reducer can test against them. `cove-fuzz generate-differential
   FILE` prints only the program.
2. **Minimise.** `cargo +nightly fuzz tmin <target> <input>` shrinks one
   input while keeping the crash. A disagreement is a panic, so `tmin` works
   on the differential targets unchanged — though what it keeps is *a*
   disagreement, not necessarily the same one; check the minimised input's
   `diff-generated` report says the same thing. For a `structured` or
   `differential` input, also save the generated program: it is what a person
   reads, and it is a `run` or `differential_source` input in its own right,
   which a person can shrink by hand far further than `tmin` shrinks the
   choices. `cargo +nightly fuzz cmin <target>` shrinks a whole corpus
   to the inputs that keep its coverage, which is worth doing before a long
   corpus is cached or shared.
3. **Keep.**
   - Fixed: copy the minimised input to `fuzz/regressions/<target>/` with a
     name that says what it was (no `.cove` extension — the repository's
     `.cove` files are all formatted, surveyed and counted, and a fuzz input
     is none of those). `cargo t` now replays it.
   - Not fixed: copy it to `fuzz/regressions/<target>/known/` and add a line
     to that directory's `README.md` with the message and the issue.
     When the fix lands, move it up a directory in the same commit.
   - A disagreement, once fixed, deserves an end-to-end case as well as the
     fuzz input, because the end-to-end corpus is what every other harness
     reads: `crates/cove-cli/tests/differential.rs` runs it on the interpreter
     and the VM, `tests/e2e.rs` runs it on the default — the native tier where
     the host has one (ADR 0122) — and again on `--backend vm` and on the
     interpreter against that, and `expected.out` pins the right
     answer, which no differential can. Write the hand-shrunk program as
     `tests/e2e/<name>/main.cove` with its `expected.out`, and add its
     `[run.<name>]` row to `tests/e2e/cove.toml` — forgetting the row fails
     somewhere that does not name the file. A new program moves
     `crates/cove-cli/tests/copies.rs`'s total (CLAUDE.md, "Adding a program
     to the repository trips a ratchet"): decompose the rise rather than
     accepting it.

## In CI

`.github/workflows/fuzz.yml`, kept out of `ci.yml` so the ordinary gate is not
slowed by it:

- **On a pull request** that touches `crates/`, `fuzz/`, the fuzz scripts,
  the manifests or the lockfile: each target, as its own job, for 60 seconds
  on `ubuntu-latest` under ASan. The point is a harness that still builds and
  an input that does not crash at once; findings come from the long run.
- **Daily at 18:00 UTC, and by hand** (`workflow_dispatch`, with a length in
  seconds): each target for 25 minutes with `-fork=$(nproc)`. The corpus each
  target grew is saved in the Actions cache and restored the next night, so
  the nightly run accumulates.
- **For the differential targets, on every run**, pass or fail: the session
  runs with `COVE_FUZZ_STATS_DIR` set, and afterwards a stable, non-ASan
  `cove-fuzz` merges the session's files and replays the job's corpus
  ([What a session compared](#what-a-session-compared)). Both summaries go
  into the job's step summary, and the JSON is uploaded as
  `fuzz-stats-<target>-<run id>`, kept thirty days. It holds counts and
  reasons only, never an input. The step does not fail the job; the fuzz
  step does that.
- **On a crash**, the input and the log are uploaded as an artifact with a
  seven-day retention. The workflow reads no secret and has `contents: read`
  only, so neither holds anything private. This repository is public, and so
  are its Actions artifacts to anyone signed in to GitHub: an input that
  crashes the runtime may be the shape of a vulnerability, so triage it before
  opening a public issue that quotes it, and use a private security advisory
  for one that reaches memory unsafety.

## AddressSanitizer: what it sees and what it does not

`cargo fuzz` builds every crate in the graph with `-Zsanitizer=address`. That
covers:

- every Rust crate — the front end, the IR, the VM, the runtime's helpers and
  the native tier's *code generator* — including their `unsafe` blocks
  (`vm/exec/native.rs`, `vm/parked.rs`, `cove-native`'s `abi.rs` and
  `template.rs`): an out-of-bounds read or write of a Rust allocation, a
  use-after-free, a double free, a stack-buffer overflow, made by any of that
  code, is reported;
- `memcpy`, `memmove`, `memset` and `malloc`/`free` calls from anywhere,
  through ASan's interceptors.

It does **not** cover:

- **the machine code the native tier emits.** `cove-native` writes x86-64
  into an anonymous `mmap`, flips it to `PROT_READ | PROT_EXEC`, and jumps
  to it. That code was not compiled by rustc, so it carries no ASan checks: a
  compiled load or store through a wrong address is invisible unless it
  happens to fault. A helper the compiled code calls back into *is*
  instrumented.
- **an access that is wrong but inside an allocation.** The VM's heap is a
  spine of 64 KiB chunks and its stack is a `Vec`; a guest object is a range
  of words inside one of those. An index that runs past an object into the
  next object in the same chunk is a valid access to ASan, from compiled code
  and from the interpreter loop alike. ASan's unit is the Rust allocation,
  not the Cove object.
- **leaks, on macOS.** LeakSanitizer is Linux only. On Linux it is on, per
  input and at exit, and the CI smoke runs there; it found
  [issue #688](https://github.com/myuon/cove/issues/688), a ring of `Shared`
  cells the interpreter never freed, on its first run with leak checking
  left on — it had been switched off, on the reasoning that a leak is not a
  memory-safety bug. On a Mac, a leak shows
  only as a test that counts what is still alive, like
  `crates/cove-runtime/tests/shared_cycles.rs`, or by running the target in
  a Linux container.

What would close those gaps, and whether it is worth it:

1. **The VM-against-native differential** (issue #681, priority 2). The
   encoded VM is the semantic reference; a compiled function that loads the
   wrong word answers differently from it far more often than it faults.
   This is the measure that catches the most of what ASan cannot see.
   Done: the [differential targets](#the-differential-targets), whose
   `native` and `native-gc` modes are this comparison.
2. **A bounds-checked code-generation mode.** A `cove-native` debug flag that
   emits, before every compiled heap access, a check of the address against
   the object's header length and a trap on failure — the same check the VM's
   `element` makes. It would make an out-of-object access by compiled code a
   deterministic crash for the fuzzer. It doubles the generator's surface for
   one instruction family at a time, so it is worth building only if the
   differential finds miscompiled accesses it cannot localise.
   Recommended after (1), not before.
3. **Guard pages.** Allocating each heap chunk with an unmapped page after
   it would turn a run off the end of a *chunk* into a fault. It does nothing
   for the in-chunk case, which is the common one, and it changes the
   allocator every embedder runs. Not recommended.
4. **Miri** on the small `unsafe` pieces that do not execute generated code
   (`vm/parked.rs`'s self-referential `OwnedVm`, the ABI structs). Miri cannot
   run JIT code at all. Worth a separate, small job; not part of this.

## Out of scope, and why

- **Comparing tiers in `run` and `structured`.** Those two run the VM and the
  native tier and require only that neither crashes. Whether the tiers
  *agree* is the differential targets' question, under the rules
  [above](#what-is-compared).
- **Failure, cleanup and the next run; isolates.** Priority 3. A harness
  here builds a fresh registry, runtime and VM for every run, which is
  exactly what a lifecycle test must not do.
- **Capability boundaries.** Every run is granted `console` alone and a
  refused call is a normal outcome, so a grant check that wrongly *allowed* a
  call would go unnoticed. Testing that needs an oracle — a host that records
  every call that reaches it — and is its own item in #681.
- **Real hosts.** No file, network, process, environment, database or clock
  host is registered, so nothing a guest does leaves the process. The
  in-memory and recording hosts the playground uses are not fuzzed either.
- **The playground's C ABI** (`crates/cove-wasm/src/abi.rs`). Its input is a
  pointer and a length from JavaScript; fuzzing it means fuzzing a wasm
  module, not this process.
- **A proof.** Since ADR 0120 a decoded image that verifies is run, and the
  machine's contract covers every program `verify` accepts. That claim is
  fuzzed by `ir`, `ir_mutate` and `image`, not proved: a gap the generator
  and the mutator cannot reach would still be a gap.
- **Multi-module packages, `cove test`, `cove fmt`.** The source targets
  compile one file as one module. covefmt is a Cove program the `run` corpus
  already includes.

**In-process isolation is not OS isolation.** Everything here tests that one
Cove run, inside one process, cannot corrupt that process. That is a different
guarantee from the one a process, container or microVM boundary gives a host
running many tenants, and nothing here establishes the second. The language
card says the same of memory limits: strict isolation is a process,
container or microVM boundary's job.

[ISOLATION_TESTING.md](ISOLATION_TESTING.md) is priority 3 and the permission boundary: failure, clean-up and the next run.
