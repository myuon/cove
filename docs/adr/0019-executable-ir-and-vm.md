# ADR 0019: An executable IR and a dedicated VM

- Status: Accepted
- Superseded in part by
  [ADR 0024](0024-a-stop-is-a-bound-not-a-point.md), which replaces
  "Fuel is charged for VM work"'s consequence that anything comparing runs
  across backends must compare outcomes rather than fuel: under a fuel limit
  the outcomes differ too
- Date: 2026-08-26
- Supersedes: [ADR 0012](0012-performance-gate-and-native-backend.md)'s
  decision that "the interpreter remains the only backend ... nothing here
  proposes writing a second one". Everything else in ADR 0012 stands, and this
  ADR leans on it: the ranking of the specification above the oracle above a
  backend is what makes a second backend safe to have, and the five gates are
  untouched
- Implemented by: [PR #114](https://github.com/myuon/cove/pull/114)
- Implementation status: the architecture is built and the VM is not yet the
  default. `crates/cove-ir` lowers and validates; `cove_runtime::vm` executes;
  `cove run --backend vm` selects it; and
  `crates/cove-cli/tests/differential.rs` runs the whole corpus through both
  backends. Of 119 cases, 43 lower and agree on both, 51 are refused by name,
  and 25 do not check, so there is nothing to run. The refusals are the
  roadmap, and the harness prints them grouped by construct.
  [Issue #111](https://github.com/myuon/cove/issues/111) is the gate that would
  make the VM the default, and it has not been passed

## Context

[ADR 0012](0012-performance-gate-and-native-backend.md) set five gates that
would make building a compiled backend a decision worth making, and refused to
build one until one was crossed. Gate 1 is throughput, and it is written as a
comparison this repository could not make: a representative Cove program
against "a reference native implementation of the same program", which did not
exist.

[Issue #104](https://github.com/myuon/cove/issues/104)'s bounded sprint did
something else, and it turns out to be enough to decide with. It took
`examples/cq` — a real streaming transformation over 100,000 records, not a
benchmark — profiled it, attributed the cost, removed what could be removed
locally, and measured what was left.

What it found:

- **Local optimization has a ceiling of about 2×.** Allocation was half the
  run, and removing all of it leaves the tree walk, which is the other half.
  Two rounds of work took `cq` from 111.8 s to 83.5 s: 1.34×, against a target
  of 10×.
- **The cost is a call, and it is structural.** A minimal call costs 650–790 ns.
  A call builds an environment, allocates a vector for its arguments, declares
  each parameter, and tears it all down. The run evaluates about 700 million
  AST nodes at roughly 130 ns each.
- **Three plausible explanations were wrong, and measurement found each one.**
  The receiver copy ([#99](https://github.com/myuon/cove/issues/99)) was real
  and was not why methods were slow. Name lookup, which this ADR's predecessor
  discussion took as the structural cost, is 3.8–5.5% of a run. Resolving names
  to frame indices ahead of time made most mechanism benchmarks *slower*,
  because an index that must be looked up beside an instruction and confirmed
  against a name is not cheaper than a short scan.

That third finding is the one that decides this ADR. Each of those attempts
failed for the same reason: a tree-walking interpreter re-derives, on every
evaluation, facts that were settled before the program ran — where a binding
lives, what a call targets, how big a frame is — and there is nowhere in a tree
walk to put an answer so that it costs nothing to use. An index is only free
when it is *part of the instruction*. A frame is only cheap when it is a
region of a stack that already exists rather than a structure built per call.

So the remaining cost is not a list of things to shave. It is the shape of the
execution model.

## Decision

Introduce a linear executable IR and a dedicated VM that runs it, and keep the
tree-walking interpreter as the reference oracle.

```text
source
  → parsed AST
  → resolved and checked program        (cove-sema, unchanged)
  → executable IR                       (new: lowering)
  → VM                                  (new: execution)
```

Three things this is not, stated because each has been mistaken for this
before:

**It is not a JIT, and it does not cross ADR 0012's gate 1.** No native code
is generated, nothing is compiled to machine instructions, and the comparison
gate 1 asks for still has not been made. Adaptive compilation and native
code generation remain that ADR's open question, to be reconsidered only after
this one has numbers. What changed is not that gate 1 was crossed; it is that a
different question was asked and answered — how far local optimization of the
tree walk can go — and the answer bounds it well below what the project wants.

**It does not remove the interpreter as the semantic oracle.** Said that way
rather than "it does not replace the interpreter", because as an *execution
path* it is meant to: once #111 passes, the VM is what runs a program, and the
tree walk is not. What it does not take over is the other role. ADR 0012 ranks
the specification above the oracle above a backend, and says a compiled backend
"sits below the interpreter and is checked differentially against it". That is
exactly the arrangement here, and it is what makes a second backend safe to
have rather than a second thing to be wrong.

So the interpreter stays after the VM is the default, is not optimized further,
and stays readable, because being readable is most of what makes it useful as
an oracle — and an oracle nobody executes is a document. What that costs, and
for how long, is a question #111 answers rather than this one.

**It is not a stable format.** The IR and the instruction encoding are internal
and versioned by nothing, because nothing outside this repository consumes
them. No serialization, no compatibility promise, no `.covec` file.

### The IR is a lowering, not a second source of truth

`cove-sema` already answers what every reference denotes and what every
expression's type is. The IR does not re-derive any of that; it records it in a
shape the VM can act on without asking again. Where the two could disagree, the
checker is right by construction, because the lowering reads its answers rather
than recomputing them.

That is also why the lowering may be slow and allocate freely. It runs once per
program. The thing being optimized is execution, and nothing else.

### Slots, not names

A function's frame is a contiguous region of slots whose size is known when the
function is lowered. Parameters, locals, and temporaries occupy it by index.
Captures are an explicit list with an explicit layout, decided at lowering
rather than discovered when a closure is created.

This is the finding above turned into a structure: the index is in the
instruction, so using it costs nothing, and there is nothing to confirm because
the layout is the thing the index refers to.

### Fuel is charged for VM work

Fuel today counts evaluated AST nodes. Instructions are not nodes and there is
no honest mapping between them, so the two backends will not report the same
`fuel_spent` for the same program. That is accepted and must be documented per
backend rather than papered over.

What must hold on both is the property fuel exists for: a run that exceeds its
budget stops, deterministically, at a point the program can be told about. An
operation whose cost is not constant — copying a collection, comparing two
strings, building a value proportional to its input — is charged
proportionally on both.

### The oracle is enforced, not assumed

An execution on the VM either completes on the VM or fails before any side
effect with a diagnostic that says the backend cannot run this construct. It
never quietly finishes on the interpreter.

Without that rule the two backends drift into one: a VM that falls back is a VM
whose measurements are about a mixture, and whose conformance is about whatever
it happened to cover.
[Issue #111](https://github.com/myuon/cove/issues/111) is the gate that decides
when the VM becomes the default, and this rule is what keeps its evidence
honest.

## What is built and what is not

This ADR is accepted with the architecture decided and the work in phases:

- [Issue #107](https://github.com/myuon/cove/issues/107) — lower checked
  programs to the IR.
- [Issue #108](https://github.com/myuon/cove/issues/108) — the VM.
- [Issue #111](https://github.com/myuon/cove/issues/111) — differential
  conformance, and the decision to make the VM the default.

Until #111 passes, the interpreter is the default backend and the VM is
selected explicitly. A construct the IR does not yet cover is named as
unsupported at lowering time, which is what lets the VM grow one construct at a
time without ever being wrong about what it ran.

### What it bought, on what it can run

Mean wall time over the `benches/` package, both backends, same machine:

| benchmark | AST | VM | |
| --- | ---: | ---: | ---: |
| `pure` — nothing but calls | 16.43 ms | 4.11 ms | **4.0×** |
| `call` — a call per iteration | 1,598 ms | 590 ms | **2.71×** |
| `method` — a call around a field | 2,871 ms | 1,238 ms | **2.32×** |
| `chars` — a per-character scan | 1,869 ms | 1,020 ms | 1.83× |
| `arrayget` — an indexed read | 1,448 ms | 980 ms | 1.48× |
| `arith` — the loop alone | 451 ms | 380 ms | 1.19× |
| `hostheavy` — Host dispatch | 4.86 ms | 4.08 ms | 1.19× |
| `field` — a struct field | 864 ms | 800 ms | 1.08× |

The order is the argument. What improves most is what is most about calling —
`pure` is almost nothing but calls, and it is four times faster — and what
improves least is what a call barely touches. That is the prediction this ADR
made from #104's measurements, and it is the shape that came back.

Lowering the whole package is 50 µs, four orders of magnitude below any
execution figure, which is why lowering is allowed to be slow and is not.

What it cannot run is the other half of the number. Of a 119-case corpus, 43
lower and agree; 51 are refused by name, most often for an associated function
of a builtin type, a `match`, a closure, or a task scope. That list is the
roadmap, and it is printed by the differential harness rather than kept
anywhere it could go stale.

One thing the refusals are not is per-entry.
[Issue #115](https://github.com/myuon/cove/issues/115) records it: the unit
being lowered is the package and the unit being run is an entry, so one closure
anywhere refuses every program in that package. Refusing is right and its scope
is wrong.

## Alternatives considered

**Keep optimizing the tree walk.** This is what #104 did, and its own
measurements are why this ADR exists: 1.34× across two rounds, a measured
ceiling near 2×, and three separate hypotheses about where the time went that
were each wrong. The next increment is not obviously positive — the resolved
indices measured *negative* on most mechanism benchmarks — which is the signal
that the model, rather than its constant factors, is what is left.

**A typed IR aimed at native compilation, per ADR 0012's incremental path.**
That ADR names "a typed IR first" as step one toward AOT or adaptive
compilation. This IR is not that, and calling it that would overstate it: it is
shaped for an interpreter loop, and whether it is a good input to a code
generator is a question for whoever crosses gate 1. Building for a compiler
this project has not decided to write would be designing against a requirement
nobody has.

**Closures over the AST, or a threaded-code interpreter.** Both keep the tree
and remove some dispatch overhead. Neither fixes the call, which is where the
measured cost is: an environment still gets built, and a name still has
nowhere free to live. They are optimizations of the model this ADR is
replacing.

**Replacing the interpreter outright.** Faster to reach one backend, and it
discards the only thing that makes a new backend checkable. ADR 0012 already
argued this position; nothing here disturbs it.

## Consequences

- There are two executable answers to what a Cove program means, and they must
  be kept in agreement by tests rather than by hope. ADR 0012's ranking says
  which is presumed right when they differ, and also says that agreement
  between them is consistency and not correctness — the Language Card is still
  what both are answerable to.
- Every language feature now has to be implemented twice before it can be used
  on the default backend. That is a real ongoing cost and it is the price of
  the oracle.
- `fuel_spent` becomes backend-specific. Anything comparing runs across
  backends must compare outcomes, not fuel.
- `cove trace` and `cove replay` must keep working on both, which means trace
  events stay source-level: an instruction-level trace would be a different
  artifact, and is not proposed here.
- ADR 0012's benchmark harness gains a second thing to measure. Every number it
  reports must say which backend produced it.
