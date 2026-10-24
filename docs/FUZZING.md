# Fuzzing Cove

What is fuzzed, how to run it, what counts as a bug, and what is out of reach.
[ADR 0112](adr/0112-fuzzing-is-a-library-a-thin-cargo-fuzz-crate-and-a-stable-replay.md)
records the structure; [issue #681](https://github.com/myuon/cove/issues/681)
is the plan this is priority 1 of.

## The pieces

| Path | What it is |
|---|---|
| `crates/cove-fuzz/` | Every harness, as a stable workspace library. `src/generate.rs` is the program generator. `src/bin/cove-fuzz.rs` replays inputs without cargo-fuzz. |
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
| `image` | the body of a lowered-IR image | `cove_ir::serial::decode`, with a valid header supplied |

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
  `sorted`.

`ByteBuffer` itself cannot be named by a program — `core.bytes*` is the
standard library's alone (ADR 0058) — so it is reached through
`StringBuilder`, string interpolation and the string functions that build
their answers in one.

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
- an AddressSanitizer report;
- an input that runs past libFuzzer's `-timeout=10` (a "timeout" artifact);
- an input that pushes the process past `-rss_limit_mb` or makes one
  allocation over `-malloc_limit_mb` (an "oom" artifact): the run's heap is
  capped, so this is memory the runtime allocated outside it, sized by the
  guest;
- in `structured` only, a generated program the front end refuses. That
  panics with the program's text, and is a generator bug or a checker bug.

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
| heap | 2^18 words (2 MiB) | a huge allocation meets the heap's refusal well inside the fuzzer's memory limit |
| grants | `console` only, into a sink | no file, network, process, environment, database or clock host is registered, and the grant source is sealed |

The fuzzer, per session (`scripts/fuzz.sh`):

| Flag | Value | Why |
|---|---|---|
| `-max_total_time` | the second argument, default 60 s | session length |
| `-timeout` | 10 s | an order of magnitude over two runs' deadlines and a front end under ASan |
| `-rss_limit_mb` | 4096 | ASan's shadow and quarantine are most of a session's 500–800 MB |
| `-malloc_limit_mb` | 1024 | one huge host-side allocation is reported where it is made |
| `-max_len` | 64 KiB source, 4 KiB generator choices, 256 KiB image | the largest program in the tree fits |
| `-dict` | `fuzz/dictionaries/cove.dict` | source targets only |
| `-len_control=0` | `structured` only | its choices are read from the front; a short input is a short program |

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
trust). The `template` feature is on in `fuzz/Cargo.toml`, so `run` and
`structured` reach compiled code on x86-64 Unix; elsewhere the native run
answers ADR 0055's capability diagnostic and only the VM runs.

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
2. **Minimise.** `cargo +nightly fuzz tmin <target> <input>` shrinks one
   input while keeping the crash. For a `structured` input, also save the
   generated program: it is what a person reads, and it is a `run` input in
   its own right. `cargo +nightly fuzz cmin <target>` shrinks a whole corpus
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
- **leaks.** LeakSanitizer is not supported on macOS; on Linux CI it runs at
  exit, but a leak in a long-lived process is not a memory-safety bug here and
  is not what this is for.

What would close those gaps, and whether it is worth it:

1. **The VM-against-native differential** (issue #681, priority 2). The
   encoded VM is the semantic reference; a compiled function that loads the
   wrong word answers differently from it far more often than it faults.
   This is the measure that catches the most of what ASan cannot see, and it
   is the next piece of #681. Recommended.
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

- **Comparing tiers.** `run` and `structured` run the VM and the native tier
  and require only that neither crashes. Whether they *agree* is priority 2,
  and comparing them needs rules this does not have yet — which outcomes are
  comparable, how NaN compares, which refusals are tier-specific.
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
- **Verifying or running decoded images.** The `image` target decodes, and
  neither verifies nor runs what it decoded: `serial::decode` trusts an image whose header
  matches, because the only images are the ones this build wrote, and running
  arbitrary IR would test a promise nobody has made. ADR 0112 §5.
- **Multi-module packages, `cove test`, `cove fmt`.** The source targets
  compile one file as one module. covefmt is a Cove program the `run` corpus
  already includes.

**In-process isolation is not OS isolation.** Everything here tests that one
Cove run, inside one process, cannot corrupt that process. That is a different
guarantee from the one a process, container or microVM boundary gives a host
running many tenants, and nothing here establishes the second. The language
card says the same of memory limits: strict isolation is a process,
container or microVM boundary's job.
