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
| `crates/cove-fuzz/` | Every harness, as a stable workspace library. `src/generate.rs` is the program generator; `src/differential.rs` runs one program in every mode and compares them. `src/bin/cove-fuzz.rs` replays inputs without cargo-fuzz. |
| `crates/cove-fuzz/tests/regressions.rs` | Replays every committed input under `cargo t`, and holds the generator to its promise. |
| `fuzz/` | The cargo-fuzz crate: one `fuzz_target!` per harness. Not a workspace member (nightly only). |
| `fuzz/seeds/<target>/` | A few small starting inputs per target, committed. |
| `fuzz/regressions/<target>/` | Minimised inputs that once crashed and no longer do, committed and replayed. |
| `fuzz/regressions/<target>/known/` | Minimised inputs that **still** crash: findings not yet fixed. Committed, not replayed. |
| `fuzz/dictionaries/cove.dict` | Keywords, operators and standard-library names for the source targets. |
| `fuzz/corpus/`, `fuzz/artifacts/`, `fuzz/logs/` | What a session makes. Ignored by git. |
| `scripts/fuzz-seed-corpus.sh` | Builds `fuzz/corpus/<target>/` from the repository's programs and the seeds. |
| `scripts/fuzz.sh` | Runs one target with the limits below. |
| `.github/workflows/fuzz.yml` | The pull-request smoke and the nightly run. |

## The targets

| Target | Input | What it drives |
|---|---|---|
| `parse` | UTF-8 source | the lexer and parser |
| `compile` | UTF-8 source | parse, standard-library attach, name resolution, type and uniqueness checking, lowering to IR, IR verification |
| `run` | UTF-8 source | `compile`, then a run on the encoded VM and a run on the native tier |
| `structured` | choices for the generator | a generated well-typed program through `compile` and both runs |
| `image` | the body of a lowered-IR image | `cove_ir::serial::decode`, with a valid header supplied, then `cove_ir::verify` over what decodes (not run) |
| `differential` | choices for the generator | a generated well-typed program run in six modes, every pair compared |
| `differential_source` | UTF-8 source | `compile`, then the same six runs and the same comparison |

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
| `vm-gc` | the encoded VM over a 2^11-word (16 KiB) heap, so a program that allocates collects | the same |
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
  mode's observation.

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
| `-max_len` | 64 KiB source, 4 KiB generator choices, 256 KiB image | the largest program in the tree fits |
| `-dict` | `fuzz/dictionaries/cove.dict` | source targets only |
| `-len_control=0` | `structured` and `differential` | their choices are read from the front; a short input is a short program |
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
     and the VM, an empty `native` file beside it makes `tests/e2e.rs` run it
     on `--backend native` against the VM, and `expected.out` pins the right
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
- **Running decoded images.** The `image` target decodes and verifies what
  it decoded and does not run it. `verify` refuses a program whose ids are out
  of range (ADR 0117), but it checks well-formedness as the lowering
  establishes it, not that the VM and the code generator are safe on every
  program it accepts, and nobody claims that. ADR 0112 §5, in part superseded
  by ADR 0117.
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
