# ADR 0001: MVP language design

- Status: Accepted
- Date: 2026-08-23, accepted 2026-08-25
- Amended by: [ADR 0008](0008-concurrent-task-execution.md), which makes
  `Shared<T>` the MVP's only synchronized handle;
  [ADR 0011](0011-garbage-collection.md), which narrows memory management to
  the interpreter until a native backend exists;
  [ADR 0012](0012-performance-gate-and-native-backend.md), which turns the
  performance criterion into a measured gate; and
  [ADR 0013](0013-host-resource-handles.md), which gives the Host API boundary
  resource handles and a way back into Cove, and whose two amendments make the
  schema this ADR asks for one description both ends enforce; and
  [ADR 0004](0004-static-type-checking.md)'s third amendment, which settles
  the builtin `Error` as a struct carrying a `message` a program may read,
  rather than an opaque type; and
  [ADR 0011](0011-garbage-collection.md)'s "Amendment (2026-08-25): the memory
  budget is removed", which retracts the memory limit this ADR lists under
  "Runtime resource control" below, leaving the collector's measurements as
  observability rather than an enforced bound
- Implemented by: [ADR 0002](0002-implementation-language-and-backend.md)
  through [ADR 0013](0013-host-resource-handles.md), each of which decides and
  builds a part of this one
- Implementation status: partial — see "Status and implementation" below

## Context

Cove explores whether a new language can combine three properties that are
usually separated:

1. a productive general-purpose language for CLIs and servers;
2. a small, safely embeddable language whose external authority is controlled
   by its host;
3. a text-first program structure that humans and coding agents can understand
   from architecture down to implementation.

Embedding and capability control alone are not novel. Languages such as Lua,
Luau, Wren, Rhai, and Mog already demonstrate much of that design space. Cove
must therefore justify itself through the integration of ordinary application
development, host-controlled execution, structural readability, predictable
performance, and observability.

## Decision

Build an MVP of a statically typed, native-first, host-controlled language with
familiar syntax and deliberately small semantics.

The MVP is a hypothesis test, not a commitment to a stable language. Features
that do not validate the core experience should be removed rather than
preserved for compatibility.

## Status and implementation

This ADR was recorded as `Proposed` and is now `Accepted`, which is a claim
about the decision rather than about the implementation. The decision above was
taken, acted on, and never revisited: twelve later ADRs, every one of them
accepted, amend and elaborate this one rather than offer an alternative to it,
and four of them narrow it in ways the header records. Leaving it `Proposed`
would say the direction was still open while all the work that assumed it went
on regardless, which is the opposite of what happened. What a `Proposed` status
is for — a decision a reader may still argue with — stopped being true of this
document somewhere around ADR 0004.

What is *built* is the separate question, and the answer is most of this
document. The language and compiler exist, from the lexer through resolution,
a type checker, module-to-module imports, traits and both dispatch forms, and
derived outlines, capability requirements, and API snapshots. The runtime
exists, with Host API dispatch under grants, a thread per task, `Shared<T>`, a
per-task collector whose allocation and heap size are observable rather than
enforced, budgets for fuel, deadlines, host calls, and the tasks a run holds
at once — the last decided by
[ADR 0003](0003-task-execution-and-runtime-control.md)'s amendment — and
traces that feed `cove trace` and `cove replay`. Every command in the
Language Card's tooling contract exists and does what the card says. The
eight representative programs all execute.

What is not built is worth naming in one place, because each item is otherwise
a sentence somewhere above that a reader would have to check against the code:

- **Native code generation.** ADR 0002 deferred it, ADR 0009's `cove build`
  packages the interpreter rather than compiling past it, and ADR 0012 records
  what would have to be true before compiling is worth starting. "Native" as an
  execution profile means a binary that runs anywhere, which exists; it does
  not mean machine code generated from Cove, which does not.
- **Wasm.** Deferred, as the classification below states.
- **`Mutex<T>`, `RwLock<T>`, `Atomic<T>`, `Channel<T>`, and `Once<T>`.** ADR
  0008 chose `Shared<T>` alone, and no representative program has yet shown the
  friction that would earn the rest.
- **Two of the six trace distinctions.** Allocation and memory pressure arrived
  with ADR 0011, and a host call's task with ADR 0003's "Amendment
  (2026-08-25): a run ends with an event, and a call names its task", which
  also gave a run the terminal event its errors had never had. Cache hits and
  misses have no event because there is no cache; task suspension and
  resumption have none either, so only spawn, completion, and cancellation are
  recorded. `cove trace` ends its own summary with what is left rather than
  leaving a reader to discover it.
- **Rejecting structural mutation during iteration.** A loop reads a snapshot
  of the elements, so a mutation through another alias is not observed and not
  refused.
- **`Array.build` and `Map.build`.** The scoped builders this ADR names do not
  exist; `Vector.of`, `Map.of`, and `Set.of` do.
- **Batteries.** `database` ships a fake and a denied implementation and no
  real one, `http` speaks no TLS, JSON exists only as `http.json`'s encoding of
  a response body, and argument parsing is whatever a program does with
  `args: Array<String>`.
- **Doc-comment warnings on modules.** An exported declaration without a doc
  comment warns. A module has nowhere to attach one, its name being derived
  from its path, so the module half of that rule has no implementation to have.

## Product boundary

Cove should support the same source language in several execution profiles:

- **Native:** standalone CLI and server binaries;
- **Embedded:** loaded in-process by a host application;
- **Sandboxed:** executed with resource limits and restricted host APIs;
- **Wasm:** a portable sandbox target where the runtime trade-offs are
  acceptable.

Browser replacement is not a goal. Wasm is an optional deployment target, not
the semantic center of the language.

### Execution profile classification

The four profiles above are not one commitment of equal weight. The list
describes the shape a mature Cove should eventually have; it does not by
itself say which parts an MVP may still be missing, and a reader comparing it
against the implementation cannot tell a completion criterion from a
direction of travel. This section removes that ambiguity.

- **Native — MVP required.** `cove run` and `cove build` are the primary way
  every other claim in this ADR is exercised, in the CLI itself and in
  `examples/`. A Cove that could not run natively would not be testing this
  ADR's hypothesis at all.
- **Sandboxed — MVP required.** `crates/cove-runtime/src/host.rs` rejects a
  Host API call the run was not granted, and `crates/cove-runtime/src/budget.rs`
  stops a run that exceeds its fuel, deadline, or host-call budget (memory was
  a fourth budget here until [ADR 0011](0011-garbage-collection.md)'s
  "Amendment (2026-08-25): the memory budget is removed" retracted it; the
  collector's allocation and heap numbers stay observable, not enforced).
  Every profile below runs under this same boundary; "sandboxed" is not a
  fourth build mode to add later, it is host-controlled authority and runtime
  limits, which already exist and are covered by both modules' own tests.
- **Embedded — MVP required.** `crates/cove-runtime/src/embed.rs` lets a Rust
  host carry a Cove package inside its own binary and fix its entry, grants,
  and limits at build time; `HostApi` and `HostRegistry` let that host
  register capability implementations `cove-runtime` never ships, not only
  the shipped `Console`, `Documents`, `Clock`, `Files`, `Process`, and
  `Database`. The smallest acceptance test this requires is
  `crates/cove-runtime/tests/embedding.rs`: a host registers its own
  `documents` implementation, grants it and stays under its own host-call
  limit (a successful run, with the host's implementation observed to have
  run), then withholds the grant (a denial), then grants it but sets its own
  limit to zero (a second denial, from the host's resource control rather
  than its capability control). `cargo test --workspace` runs this test, so a
  regression here fails CI rather than going unnoticed.
- **Wasm — deferred.** No crate in this workspace targets Wasm; nothing here
  builds, links, or runs on it. Issue #1's roadmap explicitly defers a
  production Wasm backend. For this MVP, Wasm is only a semantic-portability
  constraint on the language and backend design — "the language avoids
  backend-dependent semantics so native and Wasm programs behave
  consistently" — and not a working target the MVP must produce. A reader
  should not infer a Wasm build from the list above; it names Wasm as a
  possible future profile, not a present or MVP-required one.

The MVP's execution-profile claim is complete only when every line below
holds, each backed by a passing test rather than a description:

- [x] **Native:** `cove run` and `cove build` execute a representative
      program end to end (`examples/hello`, and `cove-cli`'s own tests).
- [x] **Sandboxed:** an ungranted Host API call is refused, and a run is
      stopped by at least one of its fuel, deadline, host-call, and memory
      limits (`crates/cove-runtime/src/host.rs` and `.../budget.rs` tests).
      The memory clause no longer holds: see
      [ADR 0011](0011-garbage-collection.md)'s "Amendment (2026-08-25): the
      memory budget is removed". Fuel, deadline, and host-call limits still
      do.
- [x] **Embedded:** a host outside `cove-runtime` can supply its own
      capability implementation and its own limits, and observe both a
      successful run and a denial (`crates/cove-runtime/tests/embedding.rs`).
- [ ] **Wasm:** intentionally not attempted. There is no MVP checklist item
      for a working Wasm build; its only MVP obligation is the
      semantic-portability constraint stated above.

A profile claimed above without a passing test to back it is not
"MVP required" and complete — it is a direction of travel, and should be
described as one rather than as delivered.

## Familiar core, explicit delta

Cove should reuse syntax and semantics already familiar from TypeScript, Go,
Swift, and Rust where doing so does not compromise the execution model.

The language-specific delta must fit in a one-page **Language Card**. This is a
design budget: adding a rule should require demonstrating that the language is
still predictable from existing knowledge plus that card.

Cove prefers explicit behavior over hidden behavior, but does not require
facts the compiler can derive to be declared twice. Common choices receive one
safe, predictable default; configuration is reserved for meaningful
differences.

Initial semantic preferences:

- left-to-right evaluation;
- explicit and consistent overflow, floating-point, equality, and panic rules;
- `Option` and `Result` instead of hidden null/error behavior;
- defined collection iteration order;
- minimal implicit conversion;
- no side effects caused merely by importing a module;
- structured lifetimes for concurrent tasks.

The MVP type system includes nominal structs and enums, exhaustive matching,
generics, traits, `Option`, `Result`, and local type inference. Function and
public API boundaries remain explicitly typed. Higher-kinded types, implicit
instance search, dependent types, and user-written effect polymorphism are out
of scope.

Trait conformance is explicit. Static and dynamic dispatch are distinct in the
semantic model and generated code. The exact surface syntax remains open; one
candidate is `T: Trait` for static dispatch and `dyn Trait` for dynamic
dispatch.

## Calls, initializers, and variadic arguments

Function calls and value initialization use the same familiar call syntax.
Argument labels are static parameter names and part of a public API contract.

```cove
let user = User(
  name: "Alice",
  age: 20
)

let response = request(
  url: endpoint,
  timeout: 5s
)
```

Positional arguments may precede labeled arguments; after the first label, all
remaining arguments are labeled. Structs receive a synthesized initializer
whose labels match their fields. User-defined initializers use the same syntax.
Cove does not use a separate `Type { fields }` expression form.

A homogeneous variadic parameter is written `items: T...`. Inside the
function it is an immutable `Array<T>`; the compiler may eliminate its
allocation when that is not observable. Spread uses `...array`.

```cove
fn of(items: T...) -> Vector<T>
let values = Vector.of(1, 2, 3)
```

This makes Vector construction an ordinary user-definable associated function,
not a language-specific literal. Default arguments are evaluated by the callee.
Dynamic keyword dictionaries and arbitrary keyword forwarding are not part of
the MVP.

Compiler diagnostics are part of the learning interface. Errors should explain
the relevant Cove rule and show a corrected textual example.

## Host-controlled authority

Cove code has no ambient authority in embedded or sandboxed execution. External
operations are typed Host APIs supplied by the host.

The compiler derives required capabilities per function from resolved Host API
calls and their call graph. This analysis is useful for inspection and
diagnostics but is not exposed as a user-written effect system.

The host chooses the entry function and grants coarse capabilities at the
execution boundary:

```toml
[run.booking_server]
entry = "booking.main"
allow = ["database", "network", "clock"]
```

The host decides which implementations are available and may replace them with
filtered, virtual, remote, test, or denied implementations. The runtime rejects
ungranted Host API calls. Stronger isolation may additionally use process,
syscall, Wasm, or microVM boundaries.

A machine-readable Host API schema is shared by the compiler, runtime, and CLI.
Each operation describes its argument, result, and error types; capability;
serialization and resource ownership; cancellation and recordability; and
whether it is a read, reversible write, or irreversible write.

"Shared" is literal: the description is one table, in a crate the compiler and
the runtime both depend on, rather than a description in one of them and a
copy in the other. Both ends read it and both ends enforce it — `cove check`
checks a call's arguments where they are written, and the boundary checks them
again for the host modules a compiler cannot see, which is every module an
embedding registers. [ADR 0013](0013-host-resource-handles.md)'s two
amendments decide the whole of that.

## Runtime resource control

Termination, CPU usage, and memory usage are runtime concerns in the MVP rather
than properties proved by the type system.

The runtime should be able to impose:

- CPU or instruction/fuel budgets;
- memory and allocation limits;
- wall-clock deadlines;
- cancellation;
- concurrency limits;
- host-call limits and timeouts.

Memory's place in this list was retracted by
[ADR 0011](0011-garbage-collection.md)'s "Amendment (2026-08-25): the memory
budget is removed"; the collector still reports allocation and heap size, but
the runtime no longer imposes a memory limit, so that entry now records what
was decided in 2026, not what holds today.

Totality, determinism, and absence of loops are explicitly not MVP guarantees.

## Memory management

The MVP uses a precise, non-moving, stop-the-world mark-and-sweep garbage
collector. The compiler emits stack maps so the collector does not conservatively
treat arbitrary integers as pointers. Non-moving objects simplify embedding,
FFI, stable trace identity, and the initial runtime implementation.

The MVP has no finalizers, compacting collector, generational collector, or
concurrent collector. Heap fragmentation, pause time, allocation, live heap
size, and GC work must be visible in traces. The allocator, object layout,
root enumeration, stack maps, mark queue, sweep, and heap budget remain
separate runtime components so the collector can evolve later.

## Values, collections, and mutation

Assignment and ordinary argument passing use one rule: field-wise shallow copy.
Cove does not change expression semantics according to whether the destination
is declared with `let` or `var`.

Primitive values, strings, enums, and user-defined structs have value semantics.
Copying a struct copies each field according to that field's semantics. A
one-field wrapper therefore naturally behaves like the value it wraps.

The MVP exposes two sequence types:

- `Array<T>` is a fixed-length immutable sequence. Its length may be decided
  at runtime. Array literals such as `[1, 2]` produce arrays.
- `Vector<T>` is a growable mutable sequence backed by stable GC-managed
  storage. Copying a vector handle is O(1), and aliases observe the same
  elements and length.

`Map<K, V>` and `Set<T>` are immutable collections in the MVP. They are
created with literals, transformations, or scoped builders. A generally
available mutable fixed-length array or mutable map is deferred until
representative programs require it; `Array.build` and `Map.build` keep
temporary mutation inside construction.

```cove
let fixed = [1, 2]

var first = Vector.of(1, 2)
var second = first
second.push(3)
// first and second both observe [1, 2, 3]
```

A vector's length, capacity, and element buffer belong to its shared storage, so
growth remains visible through every alias even after reallocation. `let`
creates a read-only place and `var` a mutable place. A `let Vector<T>` is a
valid read-only view and may observe changes made through another mutable alias.

A mutating receiver declares `var self`. An ordinary parameter receives a
shallow copy. A `var` parameter is the explicit exception: it is an inout
alias to the caller's mutable place and the call site also writes `var`.

```cove
fn length(self) -> Int
fn push(var self, value: T)
fn fill(var output: Vector<Int>)

fill(var output)
```

This is local mutation syntax, not a whole-language borrow system. A `var`
parameter cannot be stored or captured beyond the call.

`Vector.freeze()` consumes a vector with uniquely owned storage and returns an
`Array<T>` in O(1). The compiler only performs conservative, local uniqueness
checking for this explicit transition. If uniqueness cannot be proved,
`toArray()` creates an independent O(n) immutable array. Ordinary code does
not otherwise use move semantics.

```cove
var output = Vector<Int>.of()
output.push(1)
let result = output.freeze()
// output is no longer usable
```

Cove never performs an implicit deep copy. A type may implement the
`Snapshot<T>` contract when it can create an independent mutable graph.
Snapshots preserve cycles and internal sharing. Immutable values return
themselves; closures, synchronized values, and Host resources do not implement
`Snapshot` by default.

Value equality uses `==`. Identity-capable mutable handles use `is` for
shared-storage identity. Mutable handles and structs containing them are not
valid map keys. Structural mutation through any vector alias during iteration
is detected and rejected.

## Tasks and shared mutation

Cove tasks are lightweight runtime tasks with structured lifetimes, explicit
asynchronous functions, cancellation, and deadlines. I/O wait suspends a task;
CPU work runs on runtime workers.

Task transfer is determined from types rather than whole-program alias analysis.
Immutable values whose fields are task-safe may cross task boundaries.
`Vector<T>` cannot cross a task boundary, even through a `let` place; finish
it as an `Array<T>`, create an independent value, or use an explicit
synchronized handle such as `Shared<T>`, `Mutex<T>`, `RwLock<T>`,
`Atomic<T>`, or `Channel<T>`.

Closures may cross a task boundary only when every capture is task-safe. Host
resources declare task-safety in their Host API schema. The compiler rejects
unsafe captures before execution.

## Progressive disclosure

Source code should be understandable from the outside in:

```text
repository -> component -> module -> declaration -> implementation
```

Each module is a directory. Its name is derived from its path and cannot be
overridden in source. Sibling `.cove` files are implementation units of the
same module.

Illustrative public declarations:

```cove
/// A confirmed booking.
export struct Booking(
  id: BookingId,
  status: BookingStatus
)

/// Creates a booking after validation.
export fn createBooking(
  request: BookingRequest
) -> Result<Booking, BookingError> {
  // ...
}
```

Exported declarations are the single source of truth. Other declarations are
module-private. The compiler derives a typed outline, definition locations,
required capabilities, and an interface hash directly from source. API
snapshots and diffs provide stability checks without hand-written duplicate
contracts.

Ordinary purpose and intent are written as `///` doc comments attached to
declarations. The compiler preserves them for outlines, generated
documentation, and inspection, but does not pretend to verify their prose.
Public modules and declarations without doc comments produce a warning by
default; CI may promote warnings to errors.

Imports must not perform hidden initialization. Compile-time constants are
allowed, but external, fallible, or asynchronous initialization is an explicit
ordinary function called from an entry or host. Module-level mutable variables
are not part of the MVP. A one-time shared value, if needed, uses an explicit
primitive such as `Once<T>` rather than an implicit module initializer.

A package is rooted at the nearest `cove.toml`; module paths are relative to
that root. The normal build never executes arbitrary project code. Build
scripts are excluded from the MVP. Code generation is an explicit
`cove generate` workflow whose generator runs as an ordinary capability-
controlled Cove entry and whose output is inspectable source.

## Documentation and annotations

Ordinary purpose and intent are written as `///` doc comments attached to
declarations. The compiler preserves them for outlines, generated
documentation, and inspection, but does not pretend to verify their prose.
Public modules and declarations without doc comments produce a warning by
default; CI may promote warnings to errors.

Decorator syntax is reserved for enforceable compiler or runtime behavior.
The MVP defines no decorators. New decorators are accepted only after their
checking, runtime, tracing, and compatibility semantics are specified.

## Observability

Complete, low-friction tracing is part of the runtime contract rather than an
optional framework convention.

Traces should distinguish at least:

- CPU execution;
- I/O wait;
- allocation and memory pressure;
- host calls and capability use;
- task spawn, suspension, cancellation, and completion;
- cache hits and misses.

Trace identities should correspond to the same modules and functions visible
in source, plus host-selected entry calls. The host must be able to inspect and control a
running program from outside without language-specific application hooks.

## Performance and implementation direction

Both compilation speed and execution speed are product requirements. They
affect iteration time, operational cost, and predictability.

The initial target is approximately Go-class compilation speed and execution
performance: predictable, sufficiently fast native programs without maximizing
peak optimization at the expense of iteration speed.

The implementation should favor a simple compiler pipeline, local inference,
parallel package compilation, cached semantic graphs, and a small runtime over
a sophisticated JIT. Native AOT execution is primary. Development builds use
limited optimization; release builds may spend more time on inlining, escape
analysis, bounds-check elimination, and profile-guided work. 
The exact backend remains an MVP implementation decision; QBE, Cranelift, LLVM,
and C generation should be compared by implementation cost, compile latency,
runtime performance, debug information, and portability. The language avoids
backend-dependent semantics so native and Wasm programs behave consistently.

## Ordinary application DX

Cove will not be adopted merely because it embeds safely. Writing a CLI or
server must be at least as pleasant as in Go or TypeScript.

The MVP should demonstrate:

- a single tool for format, check, run, test, build, and trace;
- fast startup and incremental iteration;
- straightforward HTTP, JSON, filesystem, process, and database APIs;
- typed configuration and argument parsing;
- explicit error handling without excessive ceremony;
- reproducible, inspectable builds;
- compiler explanations that make performance and generated behavior visible.

AI integration remains text-first. Cove will not introduce a language-level
query protocol for models. Its AI experience comes from predictable syntax,
small semantics, explicit architecture, strong diagnostics, and the ability to
navigate between abstraction levels using ordinary source text.

## Change experience and responsibility boundaries

If implementation becomes cheap, producing code is no longer the main
bottleneck. Understanding a change, validating it, observing it in production,
and reversing it safely become the expensive work. Cove should optimize this
**change experience**, not add model-specific syntax.

Each layer has a distinct responsibility:

- **Language and compiler:** define types, modules, exports, errors, structured
  concurrency, and typed Host API calls. Build the semantic graph from which
  public interfaces, capability requirements, and affected dependents can be
  derived.
- **Host API definitions:** describe each external operation's capability,
  types, serialization, resource ownership, cancellation, recordability, and
  whether it is a read, reversible write, or irreversible write.
- **Runtime:** enforce grants and resource budgets; dispatch replaceable Host
  API implementations; record tasks, CPU work, I/O wait, allocations, and Host
  calls; support replay at the Host API boundary.
- **Cove CLI:** present compiler and runtime facts through `outline`, API and
  operational diffs, change-impact reports, traces, replay, and implementation
  comparisons. It also owns ordinary workflows such as format, check, run,
  test, and build.
- **Project configuration:** select entries, granted capabilities, resource
  budgets, Host API implementations, tracing policy, build target, and profile.
- **Hosting systems:** own deployment, routing, canaries, traffic shadowing,
  and rollback. These may use Cove metadata but are not language semantics.

The CLI must not invent semantics known only to the CLI. The compiler derives
facts, the runtime enforces and records them, and the CLI explains them and
composes workflows around them.

This boundary should enable a change review to answer, before deployment:

- which exported types and behaviors changed;
- which modules and entries may be affected;
- which capabilities, resources, or irreversible operations were added;
- whether recorded traffic can reproduce the relevant behavior;
- how two implementations differ in result, trace, and performance.

Replay is deliberately limited to replaceable Host API interactions. Cove does
not require whole-language determinism to make failures reproducible enough for
testing and comparison.

## MVP scope

The first usable slice should be built in this order:

1. **Language and compiler:** lexer, parser, formatter, diagnostics, core types,
   directory modules, exports, semantic graph, native backend, and derived Host
   API capability requirements.
2. **Runtime:** Host API dispatch, grant enforcement, cancellation, deadlines,
   CPU and memory budgets, and minimal trace events separating CPU from I/O
   wait.
3. **CLI:** `fmt`, `check`, `run`, `test`, `build`, `outline`, API snapshots and
   diffs, traces, and change-impact reports.
4. **Validation:** one CLI, one HTTP server, and one embedded sandbox program,
   documented by the one-page Language Card.

Host-boundary replay and side-by-side implementation comparison follow once
Host API dispatch and trace identity are stable enough to support them.

The MVP does not include a JIT, package registry, browser UI framework, effect
system, totality checker, distributed actor runtime, durable workflows, or
microVM orchestrator. Those are possible consequences of a successful runtime,
not prerequisites for validating the language.

## Success criteria

The experiment is promising if:

- a new developer can write a useful program using prior language knowledge
  plus the Language Card;
- a coding agent can understand the public structure of an unfamiliar project
  from file headers before opening implementations;
- the same nontrivial module runs standalone and under restricted embedding;
- CPU time and I/O wait are accurately attributable in traces;
- API and impact reports explain the source and operational consequences of a
  change without requiring a reviewer to reconstruct them from a code diff;
- compile and execution performance are competitive enough that developers do
  not avoid Cove for ordinary tools and services.

## Consequences

This decision intentionally makes the runtime and Host API as important as the
compiler. It also creates tension between a tiny embeddable core and the
batteries expected from a general-purpose language.

The project should resist solving that tension through hidden runtime behavior
or a large magical standard library. Host APIs, execution profiles, and
generated behavior must remain inspectable.

## Open questions

Four of the seven questions this ADR opened have been answered, and the answers
live in the ADRs that answered them rather than being restated here:

- Which implementation language and code-generation backend minimize time to a
  credible MVP? **Answered by [ADR 0002](0002-implementation-language-and-backend.md):**
  Rust, and a tree-walking interpreter as the only MVP backend, with native
  code generation deferred.
  [ADR 0012](0012-performance-gate-and-native-backend.md) adds what would have
  to be true before the deferred half is worth taking up.
- Which object representation, stack-map format, and allocator best support the
  initial non-moving collector? **Answered for the interpreter by
  [ADR 0011](0011-garbage-collection.md):** a per-task precise, non-moving
  mark-and-sweep heap over the one value shape that can close a cycle, with no
  stack maps, because a tree walker's roots are its own structures rather than
  a machine stack. The stack-map half of the question stays open, and becomes
  answerable when a native backend exists.
- How are dependency cycles represented and diagnosed? **Answered by
  [ADR 0005](0005-module-to-module-imports.md):** forbidden, and reported with
  the path that forms them.
- Which license should the project use? **Answered:** the workspace manifest
  declares MIT. The repository carries no `LICENSE` file yet, which is a
  packaging gap rather than a decision still to make.

Three remain open:

- What compatibility guarantees should generated API snapshots cover beyond
  source types, such as capability requirements and host bindings? `cove api
  snapshot` records each declaration's required capabilities and treats a newly
  required one as a breaking change; host bindings are not covered, and nothing
  yet decides whether they should be.
- Which annotations belong in the Language Card? The MVP still defines none,
  which is a scope decision rather than an answer.
- What Host API boundary remains stable across native, embedded, and Wasm
  execution? [ADR 0013](0013-host-resource-handles.md) settled the boundary's
  shape for native and embedded execution — a resource handle is a name, and a
  Host call may run a Cove closure on the calling task — and considered no
  third profile, because there is no third profile to consider yet.
