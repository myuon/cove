# ADR 0009: What `cove build` produces

- Status: Accepted
- Date: 2026-08-25
- Amended by: [ADR 0012](0012-performance-gate-and-native-backend.md), which
  measured the performance criterion this ADR could not evaluate and wrote down
  what would have to be true before a real backend is worth building
- Implemented by: PR #24
- Implementation status: complete

## Context

The Language Card's tooling contract says `cove build` produces "a native
executable". It does not exist.

ADR 0002 chose a tree-walking interpreter as the only MVP backend and deferred
native code generation, on the grounds that Host API dispatch, grant
enforcement, budgets, cancellation, and tracing must be hand-instrumented under
every backend, so a compiled backend buys execution speed rather than
correctness for the parts of the design that are unproven. That reasoning still
holds. Every one of those mechanisms now exists and is tested, and none of them
came from a backend.

Meanwhile the card's claim is operational: a user should be able to hand
someone a file that runs.

## Decision

`cove build` produces a single self-contained native executable that embeds the
program and the runtime. It does not generate machine code from Cove.

```
cove build <run-name> [--out <path>]
```

The output is a real native binary: it runs with no toolchain, no `cove` on the
path, and no source tree. Its entry, its granted capabilities, and its resource
limits are the ones `[run.<name>]` recorded, baked in at build time — so the
host boundary is decided when the binary is made rather than by whatever
`cove.toml` happens to sit next to it later.

### Why this, and what it is not

This satisfies the card operationally and honestly. It is not a code generator,
and the README and `cove build --help` must say so rather than let a reader
infer one. A binary that embeds an interpreter is a native executable; it is
not a compiled program, and the difference shows up in startup and throughput,
not in behaviour.

The alternative — QBE, Cranelift, LLVM, or C generation — remains ADR 0002's
open decision. Nothing here forecloses it: `cove build` names an output, not a
strategy, and a later backend replaces the strategy without changing the
command.

### Capabilities are fixed at build time

An embedded grant set is the point. A built binary carries the authority its
`[run.<name>]` table granted and cannot be handed more by editing a file beside
it. `--allow` and the run flags stay available at build time; the built binary
accepts only its program's own arguments.

This makes a built binary a stronger boundary than `cove run`, not a weaker
one, which is the right direction for the artifact people deploy.

### Scope

No cross-compilation, no size optimisation, no stripping, no static linking
choices. Those are packaging decisions, and none of them tests anything about
the language.

## Consequences

The card's `cove build` line becomes true, with a stated limitation. ADR 0001's
success criterion about competitive execution performance still cannot be
evaluated — that one waits on a real backend, and this ADR does not pretend
otherwise. [ADR 0012](0012-performance-gate-and-native-backend.md) later made
the waiting explicit: there is now a recorded baseline for what the embedded
interpreter costs, and a gate saying a backend must not regress the warm
process startup this artifact already achieves.

Build time is Rust's, because building an executable means linking the runtime.
That is acceptable for an artifact you produce to deploy and unacceptable for
an inner loop, which is why `cove run` exists and stays the way programs are
iterated on.
