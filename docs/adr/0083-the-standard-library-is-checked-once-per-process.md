# ADR 0083: The standard library is checked once per process

- Status: Accepted
- Date: 2026-10-04
- Refers to:
  [issue 569](https://github.com/myuon/cove/issues/569), whose staged plan
  this is stage 1 of, and whose stages 2 and 3 stay parked;
  [ADR 0077](0077-cove-fmt-is-covefmt.md), the precedent for a build-time
  artifact, which this does not follow yet (Alternatives);
  [ADR 0015](0015-capability-analysis-for-higher-order-calls.md), whose name-keyed
  opaque fields are the one way a package reaches into the library's
  resolution; [ADR 0019](0019-executable-ir-and-vm.md), whose facts
  are part of what is kept; #580, which parsed the library once per process
- Supersedes: nothing
- Decides: that the standard library's per-module resolution and type check
  are done once per process and every later package links against them; that
  the link step is the existing package-wide passes, unchanged; which three
  properties of a package put the unit aside; and that a build-time image of
  the checked library is not built now

## Context

Every package holds the standard library — 12 modules, 8,915 lines — and every
compile resolved and type-checked all of it. After #580 parsed it once per
process, that was the fixed cost of a compile: deploying `examples/edge`'s
`hello` tenant 100 times in one process measured **9.0 ms of compile in a 10.3
ms deploy**, and the tenant's own share too small to see. A host that compiles
one package per tenant pays it per tenant, which is the use case issue 569 was
unparked for.

Both passes are written over the whole package at once, but each is two kinds
of work:

- **per module**: a module's `use`s, its declarations, the call sites in its
  bodies and the walk of its bodies in resolution; its signatures, the
  environment it exports and the walk of its bodies in the check;
- **package-wide**: import cycles, method collisions, the call graph, the
  capability fixed point, the `[run]` entries and `test fn`s, and the
  uniqueness proof `freeze()` needs.

What a dependent package needs of the library is its modules' declarations,
signatures, conformances and exported environments to check against, and its
function bodies and their facts for the lowering, which is whole-program and
monomorphises library generics into the package's program. All of it is in the
per-module half.

### What the library's per-module work reads

The question that decides whether the per-module half can be shared is whether
any of it reads the package. It reads three things beyond the module:

1. **Other modules, through `use`.** A library module imports only library
   modules. A package module named `std` or `std.*` could make a library `use`
   ambiguous (`ambiguous_use`) or resolve differently.
2. **Host schemas.** Every read of a schema, in both passes, goes through the
   module's `host_uses` and `host_items`, and a library module has neither;
   the one exception is `module_shadows_host`, which asks whether a module a
   `use` names is also a host's name.
3. **`OpaqueFields`**, the one package-wide input. ADR 0015's resolution keys
   "this field holds a `dyn` value or a generic one" by field *name across the
   whole package*, because it has no types to ask. So a package's `struct
   Holder { byteAt: dyn Summary }` makes every `.byteAt` in the library read as
   opaque — including the callee of `core.byteAt(...)`, which the walk reads as
   a field. This is not hypothetical: the library's walks ask about 31 names,
   and declaring any of **8** of them (`bitAnd`, `byteAt`, `of`, `shiftLeft`,
   `shiftRight`, `shiftRightLogical`, `vectorWithCapacity`, `withCapacity`)
   with a `dyn` type in a package changes the library's own `open_calls`,
   measured on this branch.

The check of a library module reads the program only for the modules it
imports (`qualified_key`, `declaring_module`, `trait_entry`) and for a
diagnostic's help text (`exported_operations`), and reads no capability fact.

## Decision

### 1. The library's per-module half is a unit, captured by the second compile

`cove_sema::library` keeps, per process, a **unit**: for each library module,
what `resolve_module` produced before the package-wide passes (the module, its
call sites, its import edges), and what its checker settled (the environment it
exports, its function signatures, its facts).

It is captured by a compilation that checks the library in place as before —
so that compilation's answer is the old answer — and copies what the library's
modules produced, which costs it about 0.8 ms. Not by the first compilation of
a library, though: a process that compiles once, which is every `cove` command,
would pay the copies for a unit nothing reads. The first compilation of a parse
notes that it was seen, the second captures, and the third and every one after
it links. There is no separate check of the library alone, which would cost a
second check of 8,900 lines.

A process keeps a unit for at most 8 parses (`UNIT_LIMIT`, the bound #580 put on
the parses `stdlib` keeps, for the same reason: a host composing its packages
alike starts the library at the same file id, so one entry serves all of them).

### 2. The link step is the existing package-wide passes

A later compilation puts the unit's modules, call sites, edges, environments,
signatures and facts where the per-module work would have put them, resolves and
checks only the package's own modules, and then runs **every package-wide pass
as it always did**, over the library's modules and the package's together. The
capability facts of library functions are not in the unit; the fixed point
recomputes them.

This is what makes the result identical by construction rather than by
argument: the same values reach the same passes in the same order. The passes
that could have been skipped for the library — its call-graph rows, its share
of the fixed point, and the uniqueness proof's scan of its bodies — are
package-wide for a reason each, and are left in the link step (see "What this
does not decide").

### 3. Three properties of a package put the unit aside

A compilation does not link, and checks the library in place as before, when:

1. a package module is named `std` or `std.*` and is not the library's;
2. its host schemas name `std` or `std.*`;
3. it answers one of the field names the library's walks asked `OpaqueFields`
   about differently from the package the unit was captured from. The unit
   records each name asked and both answers; `OpaqueFields` records what it is
   asked while a library module is resolved. The walk branches on nothing else
   a package supplies, so the same answers make the same walk.

And a unit is reused only for **the same parse**: the same modules at the same
`FileId`s, sharing the same function bodies (`Arc` identity, held by the unit so
an address cannot be reused). The facts are keyed by those ids; a library parsed
anew, or composed by an embedder from text of its own, is checked in place.

### 4. A unit is kept only when the library was silent

If a library module reported anything — an error, a warning, a note — while a
unit was being captured, it is not kept: a diagnostic would have to be merged
back into a package's in its place. The shipped library reports nothing; a
library module that names a host module is also not kept (asserted, not
assumed).

### 5. The library's facts are shared, not copied

`Facts` holds its per-file tables behind an `Arc`, so linking the library's
facts is a reference count per file rather than a copy of every type its bodies
settled (0.33 ms of the 2.7 ms that remained, measured before this step).

## Consequences

- **A host compiling many packages pays the library once.** The edge tenant
  deploy goes from about 10.9 ms to 3.9 ms (compile 9.5 → 2.6 ms), a thousand
  tenants from about eleven seconds to about four. Peak RSS over 100 deploys
  went down, 47.8 MB to 33.9 MB: a linked compile allocates less than the unit
  holds.
- **A `cove` command is unchanged**, in time and in output: it compiles once,
  so it never captures and never links. `cove run hello`, `cove check` in
  `tools/covefmt` and `cove test` in `examples` measure the same before and
  after.
- **In-process test suites that compile many packages gain**: `cove-ir`'s unit
  tests 2.17 s → 1.64 s. `cove-sema`'s do not move: most of them resolve
  without compiling, and their packages start the library at many file ids.
- **Two paths now produce a `Program`**, linked and in place. The in-place path
  is what a fresh process, a package that trips Decision 3, and the public
  `resolve`/`check_facts` take. The identity proof below and the tests in
  `cove_sema::library` hold the two to one answer, and a change to what a
  per-module step reads from the package has to be reflected in Decision 3;
  `library`'s module documentation lists what is read.
- **A unit is a process-lifetime cache**, bounded at 8 parses. Nothing
  invalidates it because nothing can change it: the library is part of the
  binary, and a unit is only ever matched to the parse it was taken from.

## What this does not decide

- **A build-time image of the checked library** (Alternatives). A single `cove`
  command compiles once and gains nothing from this ADR; it is the use the
  image would serve.
- **Linking the library's share of the package-wide passes.** About 1.8 ms of
  the 2.5 ms a linked compile still costs is the library's share of them: the
  uniqueness proof's scan of library bodies (1.1 ms), the library's call-graph
  rows (0.3 ms) and its part of the capability fixed point (0.3 ms). Each could
  be kept, and each needs an argument this ADR does not make: the uniqueness
  proof's tables are keyed by simple name across the package, as
  `OpaqueFields` is, and its scans borrow the trees they walk; the call-graph
  rows are independent of the package only because a package that adds a method
  of the same name to a library type is refused by `check_method_collisions`.
- **Stages 2 and 3 of issue 569** — an on-disk cache of user packages, and
  module-level incremental builds — stay parked.

## Alternatives considered

**A build-time image of the checked library** (issue 569's own suggestion,
after ADR 0077). `cove-cli`'s build script would resolve and check the library
and embed the result, so that even the first compile of a process links. It is
the right shape for a single command, and it is not built now because of its
size: the image has to carry the library's syntax trees (31 AST types, every
one with spans that must be re-based to the `FileId` a package starts the
library at), the resolved modules, the call sites, `Ty` and the checker's
signatures and environments, and the facts — a serializer and reader for the
front end's whole data model, where `cove_ir::serial` for the IR alone is
1,367 lines. The gain is at most the first compile's library front end — about
8 ms of parse and 9 ms of resolve and check of a 73 ms `cove run hello` — less
whatever reading the image back costs. The use case that unparked the issue is
a host compiling many packages, which this ADR serves without it, and the unit
is the in-memory form such an image would be read into.

**Check the library alone once, against no package.** It would make the unit
independent of the package it was captured from, but costs a second check of
the library. Capturing from a compilation that is checking it anyway costs
copies instead, and condition 3 makes the dependence on that package explicit
rather than hidden.

**Capture on the first compile.** Measured, below: it costs every `cove`
command the copies for a unit it never reads.

**Key the unit only by the library and skip condition 3.** Wrong, measured:
8 of the 31 names the library's walks ask about change its open calls when a
package declares them opaque.

**Make `OpaqueFields` per module, so the library's walk reads only the
library's fields.** It would remove condition 3 and change what ADR 0015's
lower bound reports for packages that rely on the over-approximation today —
a semantic change, which this ADR is not.

## Measurement

Measured 2026-10-04 on the implementation's branch, i7-10700K (8 cores, macOS,
x86-64), `--profile checked`, against `main` at `f19dd9c` built from an export
of that commit into a directory of its own. Another agent was building
throughout (load average 9–15), so arms were interleaved and the spreads are
wide; read the differences.

**Edge tenant deploy**: `hello` composed and deployed 100 times in one process
(`examples/edge`'s `load` and `prepare`, without the registry), mean per deploy
over the 100 after the first, three interleaved runs of each:

| | `main` | this branch |
| --- | ---: | ---: |
| parse (the library's kept since #580) | 0.37 ms | 0.35–0.37 ms |
| resolve and check | 9.41–9.75 ms | 2.53–2.63 ms |
| lower | 0.87–0.88 ms | 0.83–0.87 ms |
| prepare | 0.10 ms | 0.10 ms |
| **deploy** | **10.75–11.10 ms** | **3.82–3.95 ms** |

What a linked compile still costs, by temporary instrumentation taken before
the facts were shared: the uniqueness proof 1.5 ms, of which the scan of every
body is 1.1 ms; the call graph 0.33 ms; the capability fixed point 0.33 ms;
linking the facts 0.33 ms, now a reference count per file; copying the
library's resolved modules 0.12 ms; the tenant's own module about 0.25 ms.

**Single commands**, median of 21 with `[min..max]`, interleaved:

| | `main` | this branch |
| --- | ---: | ---: |
| `cove run hello` in `examples` | 73.8 ms [71.0..79.2] | 73.2 ms [70.5..81.6] |
| `cove check` in `tools/covefmt` | 55.5 ms [49.4..60.7] | 55.3 ms [51.0..61.2] |
| `cove test` in `examples`, median of 5 | 1,277.8 ms [1,248.2..1,308.0] | 1,279.9 ms [1,251.8..1,319.6] |

A version that captured on the *first* compile measured, in the same
conditions, `cove run hello` 80.4 → 82.0 ms and `cove check` in
`tools/covefmt` 54.6 → 55.3 ms: the copies, paid by a process that never reads
them. That is why a unit is captured by the second compile.

**Test binaries**, median of 5, `cove-sema`'s without this ADR's own tests:

| | `main` | this branch |
| --- | ---: | ---: |
| `cove-sema` unit tests | 715.3 ms [699.4..738.8] | 707.0 ms [697.9..750.3] |
| `cove-ir` unit tests | 2,170.0 ms [2,123.9..2,178.9] | 1,642.0 ms [1,618.5..1,680.5] |

**Identity.** Every package in the repository — 50 packages; the whole package,
every `[run]` entry and every `test fn`, 566 lowerings — compiled three times in
one process has, in every round, the same IR `Debug` hash and, for every entry,
the same `cove_ir::serial` hash as `main`, the same rendered errors and
notices, and the same `Debug` of every resolved module, of the call graph, and
of every function's capability and open-call sets. That run raised both
8-entry tables to 1,000 so that every package's library could be kept, and
traced each compile: 102 linked, 21 linked and stopped at a resolve error, 9
captured, 9 in place (the first compile of each parse). `cove check`,
`cove outline` and `cove api snapshot` print the same bytes and exit with the
same codes in all 50 packages.
