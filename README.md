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
- [Architecture decisions](docs/adr/) — each one carries its status, the pull
  request that implemented it, how much of it is built, and what has since
  amended or superseded it
- [Representative programs](examples/README.md)
- [API documentation](https://myuon.github.io/cove/) — rustdoc for the
  implementation crates, published from `main`

## Status

An MVP compiler front end and interpreter exist. `cove check`, `cove outline`,
and `cove run` all cover every representative program; what a run does depends
on which host implementations it was given, and one of them still has no real
implementation to give.

```console
$ cd examples
$ cove check
checked 11 module(s), 11 file(s)
$ cove run hello
Hello, world!
$ cove test
ok    text.countsWordsSeparatedBySpaces
ok    text.reportsOnTheConsole
ran 2 test(s), 2 passed
$ cove fmt --check
$ cove generate --check
$ cove build hello
built `hello` from 11 file(s) into `target/hello`
  entry:  hello.main
  grants: console
  limits: (none)
$ cp target/hello /tmp && cd /tmp && ./hello
Hello, world!
```

Implemented: lexer, parser, directory modules, `export` visibility, derived
outlines and capability requirements, a deterministic formatter, a tree-walking
interpreter, Host API dispatch with grant enforcement, task scopes with a
thread per task, a per-task mark-and-sweep collector whose allocation and heap
size stay observable through `--stats` and traces rather than an enforced
limit, and runtime budgets for fuel, deadlines, host calls, and concurrency
with tracing. A Host API call is checked against the schema its operation
declares at both ends: `cove check` checks its arguments where they are
written, and the boundary checks them again, along with what the host answered.
A host resource handle is a name for something the host owns -- a module, a
resource kind, an identity number, and the task-safety its schema declares --
never the resource itself, and every operation called on one goes through the
same dispatch as any other Host API call, so it gets the same grant check, the
same schema check, the same budget charge, and the same trace; a handle whose
resource has been closed reports a diagnostic rather than acting on whatever
now occupies the slot. A host module may also declare plain-data types in a
`TypeSchema`, which Cove source names and initializes with labels exactly like
its own structs and enums. An embedding's own modules are checked the same
way: `HostApi::module_schema` and `cove_sema::Compiler::with_host_schema`
take one `ModuleSchema`, so registering a module and checking a program
written against it read the same table rather than two descriptions that can
drift. `Reentry` lets a host that was handed a Cove closure run it on the
task that made the call, on that task's stack, against that run's budget,
with no second thread and no scheduler. `cove trace` reads a recorded
trace back and summarises it, and `cove replay` runs an entry again with every
host answering from the trace -- reproducing a resource handle by handing back
the recorded name -- and reports a divergence when the program asks for
something the trace does not have. The `console`, `env`, `documents`, `clock`,
`files`, and `process` hosts each ship a real and a fake implementation; `http`
ships a real implementation speaking a deliberately small HTTP/1.1 over TCP --
one request per connection, loopback only, and fixed bounds on the request
line, on the headers, and on the body it will read, so that a peer cannot
choose how much of the process it occupies -- a recorded fake, and a denied
one; `database` ships a fake and a denied one, because connecting to a real
database needs more than the standard library. `clock.timeout` bounds a block
against a watchdog on a real clock, and against how far the block pushed a
virtual one that has no time of its own; `clock.every` repeats a callback until
its task is cancelled or the callback fails, firing exactly once on a virtual
clock, because one round is all a clock that moves only when the host moves it
can honestly give. `cove test` runs every `test fn` in a package, granting each
test the fake implementation of every capability its call graph requires unless
`cove.toml`'s `[test] allow_real` names one. `cove build` packages a run as a
single native executable that runs with no toolchain, no `cove` on the path,
and no source tree. `cove generate <name>` runs a `[run.<name>]` entry that
returns `Result<String, Error>` under its granted capabilities, writes and
formats what it returns to the package-relative `generates` path, and checks
the package; `cove generate --check` regenerates every such run into memory and
fails on the first file that differs from what is on disk, which is what CI
runs. Tasks spawned in a scope run on threads, so waits genuinely overlap and a
trace records each task's own CPU time, each host call's wait and the task that
made it, and how the run itself ended; `Shared` holds mutable state across them.

Still missing, and each of these is a documented gap rather than an oversight:
a real `database` implementation, because connecting to one means speaking a
wire protocol the standard library cannot; TLS in the `http` host, where an
`https` URL is refused rather than downgraded; a trace event for task
suspension or for a cache, which is why `cove trace` still ends its summary
with what it cannot tell you; and native code generation, still ADR 0002's open
decision, which [ADR 0012](docs/adr/0012-performance-gate-and-native-backend.md)
has since attached five gates to.

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
[ADR 0002](docs/adr/0002-implementation-language-and-backend.md), how a run is
controlled — budgets, safepoints, cancellation, and traces — in
[ADR 0003](docs/adr/0003-task-execution-and-runtime-control.md), how tasks
actually execute in
[ADR 0008](docs/adr/0008-concurrent-task-execution.md), which replaced ADR
0003's sequential phase, and how a host hands out a resource handle and
reenters a Cove closure in
[ADR 0013](docs/adr/0013-host-resource-handles.md), and how an embedding's own
host modules become ones `cove check` can see in
[ADR 0017](docs/adr/0017-embedder-host-api-schemas.md), which supersedes ADR
0001's account of what a compiler cannot see.

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
  a run is stopped by its fuel, deadline, host-call, or concurrency limits;
  see the tests in `crates/cove-runtime/src/host.rs` and
  `crates/cove-runtime/src/budget.rs`.
- [x] **Embedded — MVP required.** A host outside `cove-runtime` can supply
  its own capability implementation and its own limits, see both a successful
  run and a denial, and have `cove check` check a program against a module of
  its own before running it; see `crates/cove-runtime/tests/embedding.rs`,
  which `cargo test --workspace` (and CI) runs.
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
