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
  file in the repository and on the four damaged versions of each — the test
  that until now was covefmt's only judge, kept as one judge of two;
- `cove generate` keeps using it to format generated source in-process, and
  the AST's `Display` keeps using it for expressions. Both are inside the
  compiler, where starting a Cove program would be the wrong cost, and the
  agreement test above is what keeps them and `cove fmt` from drifting.

### 6. The cost is accepted, and named

Expected, to be replaced with measurement in this ADR's Measurement section
before it is merged:

| | Rust formatter | covefmt, native | covefmt, VM |
| --- | ---: | ---: | ---: |
| one file | 12 ms | ~13–16 ms | ~10–13 ms + formatting |
| whole repository, `--check` | 144 ms | ~500 ms (~3.5×) | ~1.5 s (~10×) |

A single file — the shape an editor's format-on-save has — stays in the Rust
formatter's class. The whole-repository check does not, because covefmt's
formatting itself is 4.6× the Rust formatter's on the native tier. PHILOSOPHY's
"Preserve the performance class" would refuse that as a matter of course;
myuon accepted it explicitly on 2026-10-02 as a temporary cost, in absolute
terms about a third of a second, for a formatter written in the language it
formats. It is the next thing to work on, not a settled price.

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

To be filled from the implementation: build-time cost of the build script;
size of the embedded IR and of the binary; `cove fmt --check` on one file and
on the repository, on both tiers, against the Rust formatter, medians of
interleaved runs; and the time to decode the IR.
