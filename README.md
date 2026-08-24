# Cove

Cove is an experimental, host-controlled general-purpose programming language.

It aims to be:

- familiar and unsurprising to humans and coding agents;
- useful for ordinary CLI and server applications;
- safely embeddable in host applications;
- explicit about dependencies, authority, intent, and performance;
- fast to compile, run, inspect, and iterate on.

The project is currently in the design and MVP exploration stage. The initial
design is recorded in [ADR 0001](docs/adr/0001-mvp-language-design.md).

- [Philosophy](docs/PHILOSOPHY.md)
- [Language Card](docs/LANGUAGE_CARD.md)
- [Representative programs](examples/README.md)

## Status

An MVP compiler front end and interpreter exist. `cove check` and `cove outline`
cover every representative program; `cove run` executes the ones that do not yet
need asynchronous execution.

```console
$ cd examples
$ cove check
checked 7 module(s), 7 file(s)
$ cove run hello
Hello, world!
```

Implemented: lexer, parser, directory modules, `export` visibility, derived
outlines and capability requirements, a tree-walking interpreter, Host API
dispatch with grant enforcement, task scopes, and runtime budgets for fuel,
deadlines, and host calls with tracing. Not yet implemented: static type
checking, concurrent task execution, the `http`, `database`, and `clock`
hosts, `cove fmt`, `cove build`, `cove test`, and the garbage collector.

The implementation direction is recorded in
[ADR 0002](docs/adr/0002-implementation-language-and-backend.md), and how
tasks execute in
[ADR 0003](docs/adr/0003-task-execution-and-runtime-control.md).

Syntax is still provisional and may change.

## Name

A cove is a small, sheltered inlet. The name reflects code that can run inside
a host-provided boundary without making the language feel limited to sandboxed
scripting.
