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
- [API documentation](https://myuon.github.io/cove/) — rustdoc for the
  implementation crates, published from `main`

## Status

An MVP compiler front end and interpreter exist. `cove check` and `cove outline`
cover every representative program; `cove run` executes the ones whose hosts
exist.

```console
$ cd examples
$ cove check
checked 9 module(s), 9 file(s), 18 warning(s)
$ cove run hello
Hello, world!
$ cove test
ok    text.countsWordsSeparatedBySpaces
ok    text.reportsOnTheConsole
ran 2 test(s), 2 passed
$ cove fmt --check
$ cove generate --check
$ cove build hello
built `hello` from 9 file(s) into `target/hello`
  entry:  hello.main
  grants: console
  limits: (none)
$ cp target/hello /tmp && cd /tmp && ./hello
Hello, world!
```

Implemented: lexer, parser, directory modules, `export` visibility, derived
outlines and capability requirements, a deterministic formatter, a
tree-walking interpreter, Host API dispatch with grant enforcement, task
scopes with a thread per task, a per-task mark-and-sweep collector, and
runtime budgets for fuel, deadlines, memory, and host calls with tracing.
`cove trace` reads a recorded trace back and summarises it, and `cove replay`
runs an entry again with every host answering from the trace, reporting a
divergence when the program asks for something the trace does not have.
The `console`, `env`, `documents`, `clock`, `files`, and `process` hosts each
ship a real and a fake implementation; `database` ships a fake and a denied
one, because connecting to a real database needs more than the standard
library. `cove test` runs every `test fn` in a package, granting each test
the fake implementation of every capability its call graph requires unless
`cove.toml`'s `[test] allow_real` names one. `cove build` packages a run as a
single native executable that runs with no toolchain, no `cove` on the path,
and no source tree. `cove generate <name>` runs a `[run.<name>]` entry that
returns `Result<String, Error>` under its granted capabilities, writes and
formats what it returns to the package-relative `generates` path, and checks
the package; `cove generate --check` regenerates every such run into memory
and fails on the first file that differs from what is on disk, which is what
CI runs. Tasks spawned in a scope run on threads, so a trace attributes each
one's wait to the task that waited, and `Shared` holds mutable state across
them. Not yet implemented: the `http` host, host resource handles such as a
database connection, and the garbage collector.

`cove build` is not a code generator. The executable it writes embeds the
program's sources and the interpreter, so it delivers a program without
compiling one: startup and throughput are the interpreter's, and only the
packaging changed. Its entry, its granted capabilities, and its limits are
the ones `[run.<name>]` recorded when it was built, and it reads no
`cove.toml` afterwards — a file placed beside it grants it nothing, which
makes a built binary a stricter boundary than `cove run`. Building one needs
`cargo` and a checkout of this repository, because an executable has to link
the runtime; running one needs neither. This is recorded in
[ADR 0009](docs/adr/0009-cove-build.md), and native code generation remains
[ADR 0002](docs/adr/0002-implementation-language-and-backend.md)'s open
decision.

The implementation direction is recorded in
[ADR 0002](docs/adr/0002-implementation-language-and-backend.md), and how
tasks execute in
[ADR 0003](docs/adr/0003-task-execution-and-runtime-control.md).

Syntax is still provisional and may change.

## MVP execution profiles

[ADR 0001](docs/adr/0001-mvp-language-design.md) names four execution
profiles: native, embedded, sandboxed, and Wasm. They are not equally weighted
MVP obligations, so completeness is judged against this checklist rather than
against the profile list on its own:

- [x] **Native — MVP required.** `cove run` and `cove build` execute a
  representative program end to end; see `examples/` and `cove-cli`'s own
  tests.
- [x] **Sandboxed — MVP required.** An ungranted Host API call is refused and
  a run is stopped by its fuel, deadline, host-call, or memory limits; see the
  tests in `crates/cove-runtime/src/host.rs` and
  `crates/cove-runtime/src/budget.rs`.
- [x] **Embedded — MVP required.** A host outside `cove-runtime` can supply
  its own capability implementation and its own limits, and see both a
  successful run and a denial; see
  `crates/cove-runtime/tests/embedding.rs`, which `cargo test --workspace`
  (and CI) runs.
- [ ] **Wasm — deferred.** No crate in this workspace builds for or runs on
  Wasm. For the MVP, Wasm is only a semantic-portability constraint on the
  language and backend design, not a working target; a production Wasm
  backend is explicitly deferred in the roadmap.

A profile checked above is backed by a passing test, not only a description;
an unchecked profile is not implemented, whatever the Product boundary section
of ADR 0001 might suggest on its own.

## Name

A cove is a small, sheltered inlet. The name reflects code that can run inside
a host-provided boundary without making the language feel limited to sandboxed
scripting.
