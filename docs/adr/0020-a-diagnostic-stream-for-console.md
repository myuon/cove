# ADR 0020: A diagnostic stream for `console`

- Status: Accepted
- Date: 2026-08-29
- Implemented by: [PR #132](https://github.com/myuon/cove/pull/132)
- Implementation status: complete — `console.eprintln` and `console.eprint`
  exist in the schema, in the host, and in every fake console the workspace
  builds; `cove run` gives them the process's standard error;
  `tests/e2e/host_console_streams` drives both streams through the real
  binary, and `tests/e2e/fail_console_error_not_granted` pins what a run that
  granted one of the two capabilities and not the other is told. Standard
  input is still not part of it, for the reason
  [ADR 0018](0018-streaming-file-io.md) gives, and
  [issue #94](https://github.com/myuon/cove/issues/94) keeps that question.

## Context

`console` had one stream. `println` and `print` both wrote to the one writer a
`Console<W>` held, so a program had one place to put everything it produced
and everything it had to say about what it produced.

That is the whole of [issue #102](https://github.com/myuon/cove/issues/102).
`examples/cq` transforms records and reports the ones it cannot read, and the
reports land in the middle of the CSV:

```text
property,bookings,nights,revenue,averageNightlyRate
bookings-malformed.jsonl:3:1: `nights` must be a number, and is a string
harbour-loft,2,3,552.00,184.00
```

Nothing the program can do fixes that, because there is nowhere else to write.
The workaround `cq` has is `--output <path>`, which moves the *records* to a
file so that the console is free for the complaints — backwards from every
tool of that kind, where the records go to standard output and the complaints
go to standard error.

The runtime has known the difference all along. `cove run` writes its own
diagnostics and its `--stats` lines to stderr and its program's output to
stdout; a built binary does the same. It was only Cove code that could not.

Nothing here is a new mechanism. [ADR 0001](0001-mvp-language-design.md) had
each operation describe "its argument, result, and error types; capability;
..." — a capability *per operation*, not per module — and ADR 0013's second
amendment made both ends of the boundary read it: `cove_sema`'s
`operation_capability` and `HostRegistry::dispatch` each take
`OperationSchema::capability` and fall back to the module's only for an
operation no schema declares. No shipped module had used it. This one does.

## Decision

`console` gains two operations, on a capability of its own.

```text
console.println(String...)  -> Result<Unit, Error>   requires console
console.print(String...)    -> Result<Unit, Error>   requires console
console.eprintln(String...) -> Result<Unit, Error>   requires console.error
console.eprint(String...)   -> Result<Unit, Error>   requires console.error
```

`Console` holds two writers instead of one, under two locks. `cove run` and a
built binary hand it the process's stdout and stderr, so a program's records
can be piped somewhere while its complaints stay on the terminal, which is
what the issue asked for.

### Two operations on `console`, not a module and not a resource

The issue left the shape open: `console.eprintln`, a second host module, or a
`console.Stream` resource with the two streams as values.

Operations on `console`, because the two streams are two ways of writing to
the same thing, and `console` is the name of that thing. A program that prints
a record and a program that complains about one are both writing to the
console; splitting them across two modules would make `console.println` and
`diagnostics.println` two unrelated names for one idea, and a reader of a
`use` line could no longer tell that the second was the console at all.

Not a resource, for the reason ADR 0013 draws the line: a handle is for
something the host *owns and can run out of* — a connection, a listener, an
open file — and something a program opens, holds, and closes. The two streams
are neither opened nor closed and there is exactly one of each. A
`console.Stream` would add a handle, a `close` nothing should call, and a
lifetime nothing has, to say what two names say.

The cost is that `console` is now a module whose operations do not all require
the same capability, and the schema test that used to assert they did is
replaced by one asserting the thing that actually matters: an operation's
capability is the module's own or the module's with a suffix, so a name in
`allow = [...]` always says which module it opens.

### Two capabilities, and which one keeps the old name

`console` keeps meaning exactly what it meant: the output stream, `println`
and `print`, and nothing else. The diagnostic stream is `console.error`.

This is the direction that leaves every existing grant alone. A `cove.toml`
that says `allow = ["console"]`, a binary `cove build` produced carrying that
list, an embedder's `Grants::new(["console"])` — all of them were written when
`console` was the whole module, and under this decision all of them still
reach exactly the operations they reached the day they were written. Widening
`console` to cover both streams would have silently granted every one of them
a stream to the terminal that its author never considered, and a capability
that quietly grows is the one thing a capability may not do.

It is also the configuration the issue asks for. "A host that wants to capture
a program's output but let its diagnostics through is a real configuration,
and it needs the two to be separately grantable to express it" — that host
grants `console.error` and not `console`, and the boundary refuses `println`
with the name of what is missing. `cove test` is now such a host in the small:
`[test] allow_real = ["console"]` gives a test the real stdout and still fakes
its diagnostics into a sink, because that list was written when `console` was
the whole module too.

A dotted name rather than `console_error` or `stderr`, because a capability is
read by whoever decides what a run may do, and `console.error` says which
module it opens where `stderr` says only what a POSIX process calls a file
descriptor. Cove writes qualified names with dots everywhere else.

### Effects, and what the streams do not differ in

All four operations are irreversible writes: bytes handed to a terminal cannot
be taken back, whichever stream carried them. All four are variadic over
`String`, join their arguments with a space, are not cancellable, are
recordable, and answer a task-safe `Result<Unit, Error>`. `eprintln` differs
from `println` in exactly two things — where it writes, and what it requires —
because anything else would be a second way to print rather than a second
place to print to.

The two locks are separate, so a task writing a diagnostic never waits behind
a task writing a record. Ordering *between* the streams is not defined and
cannot be: they are usually two different files, and a program that needs one
line after another puts both on the same stream.

### Neither backend learns anything

The interpreter dispatches a host call by module and operation name, and
`cove_ir::Inst::CallHost { module, op, argc }` carries the same two names for
the VM. A new operation on an existing module is therefore invisible to the
IR: nothing was added to the instruction set, no lowering rule changed, and
`tests/e2e/host_console_streams` lowered on the day it was written. This is
the property ADR 0019 was aiming at and it is worth recording the first time
it pays off — a host that grows does not make the backends grow.

What did have to change is the *fake* console, in both places one is built.
`crates/cove-cli/tests/differential.rs` compares what the two backends
observably did, and a single buffer would have merged the streams back
together: a program that wrote a line to the other stream on one backend would
have compared equal, which is precisely the disagreement the second stream
makes possible. It keeps a buffer per stream and compares both.

## What this does not decide

**Standard input.** [Issue #94](https://github.com/myuon/cove/issues/94) still
holds it, and it is a larger question than this one: a program reading a
pipeline needs a capability that is not "one directory", and what happens when
there is no terminal has to be answered. This ADR is the smaller cousin, and
being able to write to stderr does not settle anything about reading stdin.

**Whether a stream is a terminal.** Nothing here says whether either stream is
attached to one, and so nothing here supports a program that colours its
output when a human is reading and does not when a pipe is. That is a third
operation on `console`, and it wants its own reason.

**Moving `examples/cq`'s reports onto the new stream.** It is exactly what the
stream exists for and it is a change to a program, not to the language, so it
belongs to [issue #88](https://github.com/myuon/cove/issues/88) rather than
here. `crates/cove-cli/tests/examples.rs` pins today's behaviour, complaints
in the CSV and an empty diagnostic stream, so that the move shows up as the
improvement it is instead of as two goldens changing at once.

## Alternatives considered

**One capability covering both streams.** Fewer names, and it makes every
grant already written mean more than it did. See above.

**A `console.Stream` resource, with `out` and `err` as values.** It is the
most general shape and it buys nothing: a program would open something that
was never closed, hold a handle to one of exactly two things, and pay ADR
0013's machinery for it. It would also make the two streams one capability
again, since a resource's operations are reached through the module's, unless
each stream were issued by a differently-grant-checked operation — at which
point the capability split is back and the handle is the only thing added.

**`console.log`, `console.warn`, `console.error` as levels.** A level is a
policy — which messages matter — and a stream is a destination. Deciding the
first in the schema would fix every program's idea of severity in the Host API,
where a program that wants levels can write them itself on the stream this
adds.

## Consequences

- A Cove program can be a filter: records on stdout, complaints on stderr,
  `cove run prog > out.csv` leaving the complaints on the terminal.
- `console` is the first shipped module with two capabilities, so the
  per-operation capability ADR 0001 asked for and ADR 0013 wired up now has a
  caller. A module that wants to split a narrow authority out of a broad one
  has a worked example to copy.
- A run's `--stats` irreversible-write count now includes diagnostics, which
  is what that number has always meant: writes that cannot be undone,
  wherever they went.
- `tests/e2e`'s harness had to stop treating stderr as evidence of failure. A
  case that writes diagnostics and succeeds is neither a failing case nor a
  case with empty stderr, so a case may now state the exit status it must exit
  with in an `expected.status` file, and `host_console_streams` is the first
  that does.
