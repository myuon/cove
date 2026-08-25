# ADR 0002: Implementation language and first backend

- Status: Accepted
- Date: 2026-08-25
- Amended by: [ADR 0011](0011-garbage-collection.md), which built the collector
  this ADR deferred, and
  [ADR 0012](0012-performance-gate-and-native-backend.md), which measured the
  execution speed this ADR left unmeasured
- Implemented by: PR #3
- Implementation status: complete — Rust, the tree-walking interpreter, and the
  crate layout below are what the workspace is. Native code generation stays
  deferred, which is this ADR's decision rather than a gap in it.

## Context

ADR 0001 left three coupled questions open: which implementation language and
code-generation backend minimize time to a credible MVP, and which object
representation best supports the initial collector. Every later roadmap phase
depends on the answer, and the first milestone is small and concrete: make
`examples/hello/` run.

## Decision

Implement Cove in **Rust**, and make a **tree-walking interpreter the only MVP
backend**. Defer native ahead-of-time code generation.

### Rust

Rust gives a single distributable binary, no runtime of its own to fight with
the collector Cove will need, and a C ABI suitable for the embedding story in
ADR 0001. Go and TypeScript were rejected because their own garbage collectors
and runtimes conflict with hosting a guest language in-process.

### Interpreter first

ADR 0001 asks the implementation to favour a simple pipeline over a
sophisticated JIT. The properties that make Cove distinctive — Host API
dispatch, grant enforcement, fuel and deadline budgets, cancellation, and
source-level tracing — must be hand-instrumented under *every* backend. None of
them is easier under QBE, Cranelift, or LLVM, so a compiled backend would not
buy correctness for the parts of the design that are actually unproven. It
would only buy execution speed, which is not what the MVP is testing.

The interpreter therefore fixes the semantics first, and the native backend is
validated later against it. ADR 0001's performance target stands; it is a
requirement of the language, not of its first implementation.

### Memory

The MVP represents values with Rust ownership and `Rc` handles rather than a
collector. This matches the Language Card's rule directly: cloning a value
performs a field-wise shallow copy, `Array` shares immutable storage
unobservably, and `Vector` copies only a handle so aliases observe the same
elements and length.

The precise, non-moving mark-and-sweep collector described in ADR 0001 remains
the target once cyclic structures and tasks exist. Keeping allocation behind a
value abstraction now is what makes that replacement possible later.

Both arrived, and so did the collector:
[ADR 0011](0011-garbage-collection.md) added a per-task heap that reclaims the
cycles `Rc` cannot. `Rc` did not go away — it still reclaims every acyclic
value, which is nearly all of them — so this section is now a description of
the larger half of how memory is managed rather than of all of it.

## Crate layout

```text
crates/
  cove-diag/     source positions and diagnostics
  cove-syntax/   lexer, AST, parser
  cove-sema/     package and module loading, name resolution, derived facts
  cove-runtime/  values, Host API dispatch and grants, interpreter
  cove-cli/      the `cove` binary
```

The boundary from ADR 0001 is preserved: `cove-sema` derives facts,
`cove-runtime` enforces and records them, and `cove-cli` explains them.

## Consequences

Execution speed is not measured by the MVP, so the ADR 0001 success criterion
about competitive performance cannot be evaluated until a native backend
exists. Accepting that gap is the point of the decision: the MVP tests whether
the language and its host boundary are worth compiling.

That last sentence stood for three ADRs and no longer does.
[ADR 0012](0012-performance-gate-and-native-backend.md) built a harness that
measures the interpreter and wrote down five gates saying what would have to be
true before a compiled backend is worth building. The criterion is still not
*met* — nothing has been compared against a native reference — but "not
measured" has stopped being why.

Because the interpreter is the reference for semantics, its behaviour must stay
traceable to the Language Card. Where the Language Card and ADR 0001 disagree,
the Language Card wins.
