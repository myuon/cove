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

No compiler or runtime has been implemented yet. Syntax shown in the ADR is
illustrative and may change.

## Name

A cove is a small, sheltered inlet. The name reflects code that can run inside
a host-provided boundary without making the language feel limited to sandboxed
scripting.
