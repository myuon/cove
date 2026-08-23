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

Imports must not perform hidden initialization; initialization is an explicit
function call.

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

The initial implementation should favor a simple compiler pipeline and a small
runtime over a sophisticated JIT. Native AOT execution is the primary target.
The exact backend remains an MVP implementation decision; QBE, Cranelift, LLVM,
and C generation should be compared by implementation cost, compile latency,
runtime performance, debug information, and portability.

The language must avoid backend-dependent semantics so that native and Wasm
programs behave consistently.

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

## MVP scope

The first usable slice should include:

1. lexer, parser, formatter, and diagnostic framework;
2. functions, structs, enums, pattern matching, generics, `Option`, and
   `Result`;
3. path-derived directory modules, declaration-level exports, and generated
   outlines/API snapshots;
4. a native executable backend;
5. a minimal Host API, embedding interface, execution configuration, and
   per-function capability analysis;
6. memory, time, cancellation, and execution-budget controls;
7. structured traces separating CPU work from I/O wait;
8. one CLI example, one HTTP server, and one embedded sandbox example;
9. a one-page Language Card.

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
- GC, reference counting, ownership, arenas, or a hybrid memory model?
- What compatibility guarantees should generated API snapshots cover beyond
  source types, such as capability requirements and host bindings?
- How are dependency cycles represented and diagnosed?
- Which annotations belong in the Language Card?
- What Host API boundary remains stable across native, embedded, and Wasm
  execution?
- Which license should the project use?
