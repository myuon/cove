# ADR 0112: Fuzzing is a library, a thin cargo-fuzz crate, and a stable replay

- Status: Accepted. Superseded in part by [ADR 0117](0117-verify-refuses-a-program-whose-ids-are-out-of-range.md): the decision that a decoded image is not verified
- Date: 2026-10-24
- Decides: that Cove's fuzz harnesses live in a workspace library,
  `crates/cove-fuzz`, and that `fuzz/` — the cargo-fuzz crate — is one line a
  target over it and is not a workspace member; that every committed input,
  seed or fixed crash, is replayed by an ordinary `cargo t` test on stable
  Rust; that a harness discards every outcome the toolchain is allowed to give
  — a diagnostic, a guest trap, a limit's stop, an out-of-memory refusal, an
  `Unreadable` image — and that a finding is what is left: a panic, an abort,
  a stack overflow, a sanitizer report, or an input that outlives the fuzzer's
  timeout; that a generated program the front end refuses is a finding too;
  and that a decoded image is neither verified nor run. Recorded for
  [issue #681](https://github.com/myuon/cove/issues/681), priority 1
- Supersedes: nothing
- Refers to, without superseding:
  [ADR 0077](0077-cove-fmt-is-covefmt.md), whose image format the `image`
  target reads, and [ADR 0091](0091-a-run-is-stopped-by-its-host-not-a-fuel-allowance.md),
  whose deadline is what makes an infinite loop an outcome rather than a hang

## Context

Issue #681 asks for continuous checks for memory corruption, JIT
miscompilation and isolation bugs, and starts with fuzzing: arbitrary bytes
through the parser and the compiler, generated well-typed programs through the
runtime, AddressSanitizer where it is available, reproducible seeds and crash
inputs, and limits on the fuzzer itself so that a guest's infinite loop is not
mistaken for a bug.

There was no fuzz target in the repository. There are a great many tests that
run Cove programs — the end-to-end corpus, `vm_coverage`, the native tier's
fixtures, covefmt's oracle — and every one of them runs programs a person
wrote to be correct. That is the gap: none of them is an adversary.

`cargo fuzz` is the ordinary tool, and its ordinary layout puts the harness
code in `fuzz/fuzz_targets/*.rs`. That layout has a cost here. `fuzz/` needs
nightly and `-Zsanitizer=address`, so it cannot be a workspace member without
making every `cargo t` need nightly; and a harness that lives only in `fuzz/`
can be run only through `cargo fuzz`, so a crash input it found can be
replayed only by someone who has installed both. A fix for it would then have
a regression test nobody's gate runs.

## Decision

### 1. The harnesses are a library; `fuzz/` is a wrapper

`crates/cove-fuzz` is a workspace member, `publish = false`, holding every
harness as a `pub fn(&[u8])`: `parse`, `compile`, `run`, `structured` and
`image`. Each `fuzz/fuzz_targets/<name>.rs` is one `fuzz_target!` calling the
function of that name. `fuzz/` is listed in the root manifest's `exclude` and
has a `[workspace]` of its own.

### 2. Every committed input is replayed on stable

`crates/cove-fuzz/tests/regressions.rs` runs every file under
`fuzz/seeds/<target>/` and `fuzz/regressions/<target>/` through the harness
of that name, under `cargo t`. A fixed finding becomes a regression test by
copying its minimised input into `fuzz/regressions/<target>/`. An input that
still crashes goes in `fuzz/regressions/<target>/known/`, which the test
skips — a crash there may be an abort, which would take the test binary down —
and which the corpus script does not copy, because libFuzzer stops on a crash
in its starting corpus.

The corpus itself is not committed. It is rebuilt from the repository's
programs by `scripts/fuzz-seed-corpus.sh`.

### 3. What a finding is

A harness hands its input to the toolchain the way `cove run` would — one file
as the package `playground`, the standard library attached, the shipped host
schemas — and discards every answer the toolchain is allowed to give:
diagnostics from any stage; a run's `Err(RuntimeError)`, which covers guest
traps, the deadline, the call-depth and host-call limits, the heap's
out-of-memory refusal and a refused host call; an image's `Unreadable`.

What is left is a finding: a Rust panic, including a debug assertion and an
overflow check (the fuzz build keeps both on); an abort; a stack overflow; a
sanitizer report; and an input that runs past libFuzzer's `-timeout`, which is
set an order of magnitude above the run's deadline so that only a loop the
deadline cannot see reaches it.

The `structured` target inverts one of those. Its programs are generated to be
well typed, so a generated program the front end refuses panics with its text:
either the generator is wrong, or the checker is.

### 4. A run is bounded, sealed and quiet

Each run has a 150 ms deadline, a call-depth limit of 512, 4096 host calls, no
tasks, and a heap of 2^18 words. It is granted `console` alone, writing into a
sink, with the grant source sealed; no file, network, process, environment,
database or clock host is registered. The run is made on the VM and then on
the native tier where the build and host have one. The two are not compared:
that is issue #681's priority 2.

### 5. A decoded image is decoded, and nothing more

`cove_ir::serial::decode` promises to refuse wrong bytes without panicking and
says plainly that it does not re-verify what it accepts: an image is trusted
once its header matches, because the only images are the ones this build's
`cove-cli/build.rs` wrote. The `image` target supplies a valid header, so the
fuzzer is not spent on a 64-bit fingerprint, and decodes the body. That is the
whole target.

It was written to run `cove_ir::verify` over what decodes as well, and the
first session found `verify` indexing a layout table with an unchecked id.
`verify` is written against programs the lowering built, whose tables are
consistent by construction; holding it to arbitrary tables is the same trust
`decode` declines, one step later. Running decoded IR would go further still
and test that the VM and the code generator are safe on IR no lowering
produced. Nobody claims either, and a target that tested them would be a
source of findings about inputs that cannot exist. The `verify` input is kept
under `fuzz/regressions/image/known/` as the first thing to fix if images are
ever accepted from anywhere but this build.

## Consequences

- `cargo t` gains one test binary and the generator's own checks, a few
  seconds. Nothing in it needs nightly.
- A fuzz input is replayable three ways: `cargo fuzz run <target> <file>`
  under ASan, `cargo run -p cove-fuzz -- <target> <file>` on stable, and the
  regression test once it is committed.
- The `compile`, `run` and `structured` targets are slow — single-digit to low
  tens of inputs a second under ASan — because each input type-checks the whole
  standard library again. That is the front end's cost and not the harness's,
  and making it cheaper is a change to `cove-sema`, not to fuzzing.
- Out of scope, and recorded as such in `docs/FUZZING.md`: comparing the VM
  with the native tier (priority 2), failure-and-cleanup sequences and
  isolates (priority 3), capability boundaries, real hosts, the browser
  playground's C ABI, and running decoded images.
- ASan instruments the Rust and C the harness links, and not the machine code
  the native tier emits into its own mappings; `docs/FUZZING.md` states what
  that leaves unseen.
