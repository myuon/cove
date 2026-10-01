# ADR 0077: `cove fmt` is covefmt, shipped as lowered IR

- Status: Accepted
- Date: 2026-10-02
- Decides: that `cove fmt` runs `tools/covefmt`, the formatter written in
  Cove, instead of `cove_syntax::format`; that the `cove` binary carries
  covefmt as **lowered IR in an internal binary format**, produced by
  `cove-cli`'s build script and readable only by the build that wrote it, so
  that a start pays no front end and no lowering; that the Rust formatter stays
  as a test-only oracle and as what `cove generate` and the AST's `Display`
  use; and that the whole-repository `--check` being about 3.5× slower is
  accepted for now. Recorded for
  [issue #556](https://github.com/myuon/cove/issues/556)
- Supersedes nothing. Refers to
  [ADR 0055](0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md),
  which declined to promise a serialized IR format — this one makes none
  either; the format is a build artifact, not an interface — and
  [ADR 0076](0076-the-native-tier-is-built-by-default.md), which put the
  native tier in the default build so that `cove fmt` can rely on it

## Context

`examples/covefmt` began as a representative program — a formatter for Cove,
written in Cove, judged against the Rust formatter on every file in the
repository. Issue #556 asked whether it could *be* the formatter, and listed
what that needed:

1. **A correctness guarantee of its own**, because its only judge was the Rust
   formatter the switch would remove. #557 gave it one: every output it changes
   is checked to have the same significant tokens and the same statement tree
   as its input, and is refused otherwise. Sabotaging the printer is refused
   on 1,580 and 1,555 outputs; the corpus and its four mutations, on none.
2. **No per-invocation compile.** A covefmt start was 189.7 ms, measured by
   `cove-compile-phases` (#560): 5.3 ms of process, 82 ms of front end, 70 ms
   of lowering, the rest dropping it all again. The Rust formatter does a file
   in 12 ms.
3. **The native tier where `cove fmt` runs**, which ADR 0076 provided on
   x86-64 unix.

Moving covefmt to a package of its own, `tools/covefmt`, cut a start to about
127 ms by checking 7k lines instead of 16k. The rest is the front end and the
lowering of covefmt itself, and they produce the same answer every time for a
given build of `cove`. Other toolchains that write their tools in their own
language ship those tools already compiled — Go's `gofmt`, Rust's `rustfmt` —
or ship a snapshot tied to the SDK that wrote it, as Dart does for
`dart format`. The last is the shape that fits a language whose programs run
on a VM: the work before the first instruction is done once, when the
toolchain is built.

## Decision

### 1. `cove fmt` runs covefmt

`cove fmt [path] [--check]` keeps its contract — which files it considers,
that it rewrites in place or with `--check` writes nothing and lists what
would change, its exit codes, and that **a file that does not parse is
reported and never rewritten**. Whether a file parses is still decided by
`cove_syntax`'s parser, which the toolchain has anyway; only a file that
parses is handed to covefmt, so the diagnostics a broken file gets are
unchanged.

Walking the targets, reading, writing and reporting stay in Rust. For each
file that parses, `cove fmt` calls covefmt's pure entry `formatSource(source:
String) -> Formatted` (`tools/covefmt/covefmt/print.cove`), which answers the
formatted text or the input with the reason it refused. covefmt is therefore
run **with no capability at all**: it cannot read, write or reach anything,
and needs no `[run]` grant. The program is set up once per `cove fmt` and
invoked once per file.

If covefmt's meaning check refuses its own output for a file, `cove fmt`
leaves the file alone, says so naming the file and the check's reason, and
exits non-zero. That is a formatter bug, and it is reported as one rather than
written to disk.

### 2. It runs on the native tier where there is one

`cove fmt` runs covefmt on the native tier on a host that has it and on the
encoded VM otherwise. This is not the silent substitution ADR 0055 forbids:
the two tiers run one lowering under one runtime, and CI asserts that they
print the same bytes for the whole corpus. `cove fmt --backend vm` and
`--backend native` select a tier explicitly, the second with ADR 0055's
capability diagnostic where it is unavailable; `--backend ast` is refused,
because the interpreter runs a checked program and the binary carries none.

### 3. covefmt is carried as lowered IR, built by `cove-cli`'s build script

`cove-cli`'s `build.rs` checks `tools/covefmt` against the standard library
and lowers `covefmt.formatSource`, with the same front end and lowering `cove
run` uses, and the result is embedded in the binary. At start `cove fmt`
decodes it, sets up the VM (and the native tier), and runs.

- The build script depends on `cove-syntax`, `cove-sema` and `cove-ir` as
  build-dependencies, runs them optimised (`[profile.*.build-override]`), and
  declares `rerun-if-changed` on `tools/covefmt` and the standard library.
- A covefmt that does not check or lower **fails the build**, with the
  diagnostic. A `cove` binary never carries a formatter it could not have run.
- Nothing generated is checked into the repository.

### 4. The format is binary, internal and version-locked

The serialized IR is a compact binary encoding written and read by a
hand-written encoder in `cove-ir`, with no new dependency. It carries what a
run needs — the functions reached from the entry, the program's literals and
layouts, the entry's signature for argument checking, and enough of covefmt's
source to name a file and line in a runtime error — and nothing a run does
not.

It begins with a header naming the format and a fingerprint of the build that
wrote it. A reader refuses any other, so the format can change in any commit
without a migration, a version scheme or a promise. It is an artifact of one
build, like a CPython `.pyc` or a Dart SDK snapshot, and is not an interface
anyone outside the binary reads.

### 5. The Rust formatter stays, as an oracle

`cove_syntax::format` is not deleted:

- it is the oracle for a test that asserts covefmt's output equals it on every
  file in the repository and on a damaged copy of each — re-indented, its
  blank lines taken out, the space inside its braces squeezed, its ends left
  loose — so that what was until now covefmt's only judge is kept as one judge
  of two. `covefmtBench`'s four mutations stay judged against the files on
  disk, which the undamaged half of this test makes the Rust formatter's fixed
  points;
- `cove generate` keeps using it to format generated source in-process, and
  the AST's `Display` keeps using it for expressions. Both are inside the
  compiler, where starting a Cove program would be the wrong cost, and the
  agreement test above is what keeps them and `cove fmt` from drifting.

### 6. The cost is accepted, and named

Measured (see the Measurement section for how), `cove fmt --check`, medians of
15 interleaved runs:

| | Rust formatter (`main`) | covefmt, native | covefmt, VM |
| --- | ---: | ---: | ---: |
| a small file (13 lines) | 5.5 ms | 14.6 ms (2.7×) | 10.8 ms (2.0×) |
| a large file (`print.cove`, 3,732 lines) | 14.3 ms | 61.9 ms (4.3×) | 136.7 ms (9.6×) |
| whole repository | 148.9 ms | 684.1 ms (4.6×) | 1,686.9 ms (11.3×) |

When this ADR was written the expectation was a single file at about 13–16 ms
native and the whole repository at about 500 ms (~3.5×) native and 1.5 s VM.
A single file — the shape an editor's format-on-save has — stays in
milliseconds, within a factor of three of the Rust formatter on a small file.
The whole-repository check does not stay in its class, because covefmt's
formatting itself is several times the Rust formatter's. PHILOSOPHY's
"Preserve the performance class" would refuse that as a matter of course;
myuon accepted it explicitly on 2026-10-02 as a temporary cost — then expected
at ~3.5×, about a third of a second, and measured here at 4.6×, about half a
second more than the Rust formatter — for a formatter written in the language
it formats. Shown the measured 4.6×, myuon accepted it the same day on the
grounds that the whole-repository check stays under a second. That is the
bound this ADR records: **a whole-repository `cove fmt --check` under one
second** on the native tier. Making covefmt's formatting faster is the next
thing to work on, not a settled price.

## What this does not decide

- **The front-end and lowering speed-ups** #556's profile ranked — flat
  dataflow passes, one `emit` instead of three, skipping the drop at exit, the
  allocator, shared ASTs. They speed every `cove run` and are separate work.
- **A cache for user programs.** This embeds one program the toolchain ships;
  it is not a mechanism for `cove run` to reuse a compile across runs.
- **Shipped machine code.** Persisting native code is ADR 0055's question. It
  would save the 2–3 ms native compilation on top of this, and needs the IR
  anyway on every host the tier does not serve.
- **Other tools in Cove.** The mechanism would serve them; whether any should
  follow is a decision for each.

## Consequences

- `cove-cli` takes longer to build: the build script runs the front end and
  the lowering once. Measured in the Measurement section.
- `tools/covefmt` is now part of the toolchain. A change to it changes
  `cove fmt`, and its tests, its benchmark and its meaning check are gates on
  the formatter rather than on an example.
- covefmt's limitations become `cove fmt`'s: where it breaks a line differently
  from the Rust formatter, the agreement test fails, and the fix goes in
  covefmt.
- `cove fmt --backend` stops being a flag that "selects nothing here".

## Alternatives considered

**Compile covefmt from source on every run.** Rejected: 127 ms before the
first instruction, ten times a Rust single-file format.

**Check the serialized IR into the repository** and gate it with a staleness
check, as `examples/cove-api.txt` is. Rejected by myuon: a build output does
not belong in the repository.

**Ship machine code instead of IR.** Rejected for now, above: it saves 2–3 ms,
needs relocation of addresses the template compiler bakes in, still needs the
IR, and makes stored machine code executable, which ADR 0055's "verified IR is
the security boundary" avoids.

**Keep the Rust formatter as `cove fmt` and covefmt as an example.** Rejected:
it leaves the language's own formatter a benchmark, and the work #556 did to
give covefmt a guarantee of its own would guard nothing anyone runs.

## Measurement

Measured 2026-10-02 on the implementation's branch, i7-10700K (8 cores, macOS,
x86-64, so the native tier is present), `--profile checked`, nothing else
building. The baseline is `origin/main` at `ca92e5e` built in a worktree of its
own. Timings are medians with `[min..max]`; arms were interleaved within each
round, and a first round was discarded as warm-up.

**`cove fmt --check`**, 15 rounds. Every arm reported the same files — none, on
the formatted tree, exit 0 — and a copy of `examples/hello/main.cove` with its
indentation doubled was reported by all three, exit 1.

| | Rust formatter (`main`) | covefmt, native | covefmt, VM |
| --- | ---: | ---: | ---: |
| `examples/hello/main.cove`, 13 lines | 5.5 ms [5.1..8.3] | 14.6 ms [12.9..16.6] | 10.8 ms [10.4..12.9] |
| `tools/covefmt/covefmt/print.cove`, 3,732 lines | 14.3 ms [14.1..18.8] | 61.9 ms [60.9..64.7] | 136.7 ms [132.6..139.9] |
| the repository root, 384 files | 148.9 ms [146.5..154.6] | 684.1 ms [677.7..694.9] | 1,686.9 ms [1,672.2..1,715.0] |

On a small file the VM is faster than the native tier: compiling covefmt's
reached functions is a few milliseconds that one small file does not repay.

**The start.** `cove-compile-phases tools/covefmt covefmt.formatSource 21`
(extended for this ADR to time the image), 21 iterations:

| | median [min..max] |
| --- | ---: |
| front end and lowering of covefmt, which the image replaces | 94.93 ms [91.98..100.51] |
| `cove_ir::serial::decode` of the image | 1.67 ms [1.65..1.72] |
| decode, then `Runtime` and `Vm::new` over it | 3.75 ms [3.72..4.85] |
| `compile_native` and `Vm::with_native` | 5.21 ms [4.48..6.53] |
| `cove_ir::serial::encode` (the build script's write) | 1.38 ms [1.29..1.49] |

**Sizes.** The embedded image is **352,942 bytes**: 541 functions of which 120
are lowered and 421 are stubs kept so that no function id is renumbered,
13,143 instructions, and **192,330 bytes of covefmt's own source text**, carried
so that a runtime error inside covefmt renders with its excerpt; the standard
library's files are named by path and hash rather than carried, because the
binary holds them already. The `cove` binary went from **9,504,024 to
9,941,192 bytes** (+437,168, +4.6%).

**The build.** 15 interleaved rounds of `cargo build --profile checked -p
cove-cli` after touching a file, and 3 from an empty target directory:

| | `main` | this ADR |
| --- | ---: | ---: |
| after touching `crates/cove-cli/src/main.rs` | 6.25 s [6.14..6.52] | 6.35 s [6.19..6.48] |
| after touching `tools/covefmt/covefmt/print.cove` | 0.06 s (nothing to do) | 6.80 s [6.63..7.01] |
| from an empty target directory | 36.80 s [36.67..36.93] | 38.71 s [38.56..38.95] |

The build script itself, over 15 runs: checking covefmt **52.6 ms**
[50.9..55.6], lowering it **52.7 ms** [50.4..55.5], writing the image 1.7 ms
[1.5..2.3]. A change to covefmt therefore costs a `cove-cli` rebuild plus about
half a second, and the build-dependencies are shared with the binary's own
dependencies under `checked` rather than compiled twice:
`[profile.release.build-override]` sets release's own `opt-level = 3` and sixteen codegen units,
and `[profile.checked]` names its sixteen codegen units explicitly so that the
two profiles compare equal. Under `dev` the build-dependencies are compiled a
second time, optimised.

**What the image does not depend on.** Nothing in `cove-diag`, `cove-schema`,
`cove-syntax`, `cove-sema` or `cove-ir` is conditional on the target, and
`serial`'s tests assert that, so an image the build script makes on the host is
the image the target would have made — `cargo check -p cove-cli` for
`aarch64-apple-darwin` and `x86_64-pc-windows-gnu` both build it.

**What the switch found.** The agreement test of decision 5 passed on the
repository from the first run, and the existing `cmd_fmt` tests did not: on
input that still needed formatting, covefmt wrote no blank line between two
declarations, kept blank lines at the end of a file and space at its top,
wrote `{1}` as `{ 1\n }`, kept tokens the source ran together inside a group it
left on one line, kept a call broken around one argument that could not fit
either way, and kept a doc comment's missing space and a comment's trailing
space. None of the corpus's files or `covefmtBench`'s mutations asks for any of
those, because every file here already obeys them. covefmt now answers what the
Rust formatter answers on each, which is why the agreement test runs on a
damaged copy of every file as well. The rules cost `covefmtBench`'s native
`whole` 489.5 ms [486..494] on `main` against 548.0 ms [545..567] here (8
interleaved runs; the corpus is 0.7% larger here). One difference is known and
left: doubling every space, string literals included, pushes some lines past
the width, and on two of them — a call around a call around a long literal —
covefmt breaks the outer call where the Rust formatter hugs it.
