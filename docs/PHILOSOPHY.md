# Cove philosophy

Cove is a familiar general-purpose language that can also run safely inside a
host application.

## Familiar by default

Reuse syntax and semantics programmers already know. Cove-specific rules must
stay small enough to explain on one Language Card.

## Syntax must earn its place

Syntax is for behavior the compiler or runtime can enforce. Human intent and
background belong in doc comments, not special keywords disguised as prose.

## One language, different trust levels

The same source should work as a CLI, a server, or an embedded guest. The host
changes the available authority and resources, not the language's meaning.

## No ambient authority

Embedded code can use only Host APIs it receives. Files, network, clocks,
processes, and databases are explicit capabilities with replaceable real,
fake, filtered, or denied implementations. Code requirements are analyzed per
function; the host grants authority at the execution boundary.

## Architecture should be visible

A module should reveal its public API, dependencies, and owned concepts before
its implementation. Humans and coding agents should be able to understand a
project from the outside in.

## Text is the interface

Cove does not add model-specific query syntax. Predictable source, doc
comments, compiler errors, and structural outlines are the durable interface
between programmers, coding agents, and tools.

## Prefer runtime control to heroic proofs

Cove does not prove that programs terminate. Hosts control CPU, memory,
deadlines, concurrency, cancellation, and external calls at runtime.

## Observability is built in

The runtime should trace modules, functions, tasks, capabilities, allocation,
CPU work, and I/O wait without requiring application instrumentation.

## Performance is developer experience

Fast compilation, startup, and execution matter for iteration speed and
operating cost. Generated behavior should remain predictable and inspectable.

## Earn complexity through use

Cove begins as an experiment. Add features only when representative programs
show recurring friction that simpler language or library designs cannot solve.
