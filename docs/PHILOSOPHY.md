# Cove philosophy

Cove is a small, fast, host-controlled general-purpose language designed to be
understood from the outside in.

## One language, multiple degrees of trust

A language should not force a choice between a productive standalone program
and safely embedded code. The same Cove module should be able to become a CLI,
a server component, or a restricted guest. What changes is the authority and
resources supplied by the host, not the meaning of the source language.

## No ambient authority

External operations enter through typed Host APIs. Code cannot reach the
filesystem, network, clock, process, or database merely because the operating
system can. Authority is declared coarsely in source and granted concretely by
the host.

Cove deliberately stops short of a general effect system. The goal is a
practical, inspectable boundary, with stronger isolation enforced by the
runtime and operating environment.

## A normal language before a sandbox DSL

Capability control is not enough to earn adoption. Cove must be pleasant for
ordinary CLIs, servers, and tools: familiar syntax, strong types, fast builds,
fast startup, predictable execution, useful libraries, and excellent errors.

Embedded execution is a profile of the language, not its ceiling.

## Familiar core, explicit delta

Novel syntax and semantics consume a limited comprehension budget. Cove reuses
what programmers and coding agents already know wherever possible. The
language-specific delta should remain explainable by a one-page Language Card.

If a feature makes that card substantially harder to understand, its value
must exceed the permanent cost it adds to every program and every reader.

## Text first

Coding agents evolve faster than language-specific AI protocols. Cove does not
put model queries or prompts in the language core. Its AI experience comes
from ordinary text that is predictable, compact, explicit, and easy to
navigate.

Text remains the durable interface between programmers, agents, compilers, and
version control.

## Understand programs from the outside in

Readers should be able to start at the repository, descend through components
and modules, and open function bodies only when necessary. A significant file
should reveal its purpose, public API, dependencies, ownership, authority, and
entrypoints near the top.

These declarations are compiler-visible contracts, not comments maintained by
convention. The source tree doubles as an architectural index.

## Prose belongs beside implementation

Names and types describe mechanics but do not always preserve why code exists.
Doc comments keep durable purpose and intent beside declarations without
pretending prose has formally verified semantics. The compiler preserves doc
comments for outlines, generated documentation, and inspection tools.

Dedicated syntax is reserved for information the implementation can enforce.
If text does not affect checking, compilation, or execution, making it a
keyword or annotation adds ceremony without adding a guarantee.

## Predictability is a feature

Evaluation order, equality, numeric behavior, errors, collection order,
concurrency lifetime, and backend differences should not be surprises.
Generated behavior must be inspectable, and native and Wasm backends should
not quietly assign different meanings to the same program.

## Runtime control over heroic proofs

Cove does not require totality, determinism, or termination proofs. Hosts need
practical control over CPU, memory, deadlines, concurrency, cancellation, and
external calls. Runtime limits should be cheap, composable, and observable.

Static guarantees are valuable when they remain understandable and improve
ordinary programming. They are not goals in isolation.

## Observability is part of execution

Tracing should not depend on every application adopting the right framework.
The runtime already knows when code computes, waits, allocates, spawns work, or
crosses into a Host API. Cove should expose that knowledge using the same
module, function, task, capability, and entrypoint identities visible in
source.

In particular, traces must distinguish CPU work from I/O wait. Performance is
part of developer experience and operational cost, not a late optimization.

## The compiler teaches the language

Clear diagnostics are a primary interface. An error should explain the rule
that was violated and, when possible, show the smallest correct rewrite. A
small language with excellent errors can be learned while building real
software.

## Earn complexity through use

Cove begins as an experiment. The MVP excludes attractive but unproven ideas
such as a JIT, durable workflows, distributed actors, microVM orchestration,
totality checking, and a package registry.

Features should be added when representative programs demonstrate that they
remove recurring friction. Compatibility with an unvalidated design is less
important than finding a coherent language worth keeping.
