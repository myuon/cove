# ADR 0001: MVP language design

- Status: Proposed
- Date: 2026-08-23

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

## Product boundary

Cove should support the same source language in several execution profiles:

- **Native:** standalone CLI and server binaries;
- **Embedded:** loaded in-process by a host application;
- **Sandboxed:** executed with resource limits and restricted host APIs;
- **Wasm:** a portable sandbox target where the runtime trade-offs are
  acceptable.

Browser replacement is not a goal. Wasm is an optional deployment target, not
the semantic center of the language.

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

Totality, determinism, and absence of loops are explicitly not MVP guarantees.

## Memory management

The MVP uses a precise, non-moving, stop-the-world mark-and-sweep garbage
collector. The compiler emits stack maps so the collector does not conservatively
treat arbitrary integers as pointers. Non-moving objects simplify embedding,
FFI, stable trace identity, and the initial runtime implementation.

The MVP has no finalizers, compacting collector, generational collector, or
concurrent collector. Heap fragmentation, pause time, allocation, retained
memory, and GC work must be visible in traces. The allocator, object layout,
root enumeration, stack maps, mark queue, sweep, and heap budget remain
separate runtime components so the collector can evolve later.

## Values, places, and receiver mutation

Values do not carry mutability. `let` and `var` describe the place that holds
a value:

- `let x = value` creates a read-only place. Read-only storage-backed values
  may be shallow-shared in O(1).
- `var x = value` creates a mutable place. A storage-backed mutable place may
  be updated in place but may not be implicitly aliased.

A fresh value may initialize either place without changing the expression's
source-level meaning. Copying from a mutable storage-backed place requires an
explicit choice:

```cove
var values = [1, 2, 3]

let snapshot = values.copy() // independent value graph
let alias = values.ref()     // the same mutable place
```

`.copy()` recursively copies ordinary storage-backed value fields. Immutable
storage may remain shared, and explicit identity such as `Ref<T>` is preserved
rather than followed. Cycles therefore stop at explicit references. A type may
implement its own `copy()`; a deliberately one-level copy should use a name
such as `shallowCopy()` so the cheaper semantics remain visible. `.ref()` does
not consume the original variable.
It creates a `Ref<T>` to the same mutable place; an escaping reference may
promote that place to a GC-managed heap cell. Mutations through either name are
then visible through the other. Trivially copyable values such as numbers and
value-only structs do not require either operation.

Cove does not use copy-on-write to define mutation semantics. Read-only values
may share storage, mutable places update stable storage, and transitions that
would introduce mutable aliasing are explicit. Initializing a mutable place
from an existing storage-backed read-only value therefore requires an explicit
copy; a fresh rvalue can be taken directly.

Receiver mutation is part of a method's source-level contract:

```cove
impl List<T> {
  fn length(self) -> Int {
    self.count
  }

  fn push(var self, value: T) {
    // ...
  }
}
```

`self` is read-only and may be called through either a `let` or `var`
place. `var self` may update the receiver and requires a mutable place or an
explicit `Ref<T>`. It introduces no reference syntax, lifetime parameters, or
whole-language borrow checker. Receiver mutability is written explicitly,
appears in outlines, trait contracts, and API snapshots, and changing it is an
API compatibility event.

## Parameters and retention

Passing a value for the duration of a call creates a temporary view and needs no
copy or reference syntax. A normal parameter is read-only; a `var` parameter
may update the caller's mutable place during the call. Neither permission alone
creates a retained alias.

The compiler derives whether a function retains each parameter by storing,
returning, capturing, spawning, or passing it to another retaining operation.
That fact is part of outlines and API snapshots.

- Retaining a read-only argument shallow-shares its immutable storage in O(1).
- Retaining a fresh result transfers its storage without a user-visible move.
- Retaining a mutable place is rejected unless the program chooses an
  independent `.copy()` or passes an explicit `Ref<T>` created by `.ref()`.
- A temporary parameter copied inside the callee may be retained as an
  independent snapshot.
- A `var` parameter may not escape the call without the same explicit choice.

Changing a public parameter from borrowed to retained is an operational
compatibility event because mutable callers may need to choose a retention
mode. Traits, dynamic calls, extern declarations, and Host APIs must expose the
retention contract when it cannot be derived from an implementation.
Higher-order calls may initially be analyzed conservatively.

This makes ordinary reads and immutable constructors concise while ensuring
that copying and mutable identity sharing remain visible where they occur.

## Tasks and shared mutation

Cove tasks are lightweight runtime tasks with structured lifetimes, explicit
asynchronous functions, cancellation, and deadlines. I/O wait suspends a task;
CPU work runs on runtime workers.

Read-only values may cross task boundaries. A mutable place and a task-local
`Ref<T>` may not be captured by another task. Shared mutation requires an
explicit synchronized handle such as `Shared<T>`, `Mutex<T>`, `RwLock<T>`,
`Atomic<T>`, or `Channel<T>`. The compiler rejects mutable captures that do
not use such a type.

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
export struct Booking {
  id: BookingId
  status: BookingStatus
}

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

## Documentation and performance annotations

Decorators provide an extensible but visible place for non-core declarations:

```cove
/// Reserves inventory and then authorizes payment.
@hot
@performance(latency = 20ms)
fn createBooking(request: BookingRequest) -> Result<Booking, BookingError> {
  // ...
}
```

Syntax is reserved for enforceable semantics; prose belongs in doc comments.
The MVP preserves doc comments in its semantic model. An annotation such as
`@hot` must have documented compiler or runtime semantics before it is
accepted; unknown annotations must not silently change behavior.

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
analysis, bounds-check elimination, and profile-guided work. An enforceable
`@hot` annotation may concentrate optimization and tracing budget on selected
functions.

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

- Which implementation language and code-generation backend minimize time to a
  credible MVP?
- Which object representation, stack-map format, and allocator best support the
  initial non-moving collector?
- What compatibility guarantees should generated API snapshots cover beyond
  source types, such as capability requirements and host bindings?
- How are dependency cycles represented and diagnosed?
- Which annotations belong in the Language Card?
- What Host API boundary remains stable across native, embedded, and Wasm
  execution?
- Which license should the project use?
