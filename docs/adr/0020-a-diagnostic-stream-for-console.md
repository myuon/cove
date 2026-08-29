# ADR 0020: A diagnostic stream for `console`

- Status: Accepted
- Date: 2026-08-29
- Implemented by: [PR #132](https://github.com/myuon/cove/pull/132)
- Implementation status: complete — `console.eprintln` and `console.eprint`
  exist in the schema, in the host, and in every fake console the workspace
  builds; `cove run` gives them the process's standard error, and
  `tests/e2e/host_console_streams` drives both streams through the real
  binary. Standard input is still not part of it, for the reason
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

## Decision

`console` gains two operations, under the capability it already had.

```text
console.println(String...)  -> Result<Unit, Error>   requires console
console.print(String...)    -> Result<Unit, Error>   requires console
console.eprintln(String...) -> Result<Unit, Error>   requires console
console.eprint(String...)   -> Result<Unit, Error>   requires console
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

### One capability, because a stream is not an authority

All four operations require `console`. A program that may write to the
terminal may write to the terminal, and which of the process's two file
descriptors a line reaches is a fact about that program's output rather than
about what it was permitted to do.

Splitting authority at the stream would be a finer grain than the thing being
authorised deserves. The unit a grant is written in has to be a unit somebody
can reason about when they write it, and "may print, may not complain" is not
one: the danger a `console` grant is weighed against is that a run can put
bytes in front of a person or into a pipe, and both streams do exactly that,
in the same amount, with the same consequences. A second capability would buy
no protection and would make every `allow = ["console"]` in existence a
question about which half of the console was meant — a list that used to say
one thing would now be read as having taken a position on something its author
never considered.

So a grant written before `eprintln` existed covers it, which is the property
this decision is chosen for: nothing already written has to be read again.
`crates/cove-schema/src/hosts.rs`'s
`every_operation_declares_its_module_capability` stays true, and `console`
stays a module with one answer to "what does granting this allow".

### The two writers are the wiring, and that is where the choice lives

A host that means to capture what a run produces while letting its complaints
reach the terminal is a real configuration, and it is expressed by handing
`Console::new` a buffer and `std::io::stderr()`. That is where it belongs. It
is a decision about where output goes, made by whoever is assembling the run's
plumbing, and it needs no vocabulary in the grant list and no new name in
anybody's `cove.toml`.

`Console::new` takes both writers rather than defaulting the second, and this
is deliberate: a host that captured a program's output before this existed
would otherwise silently begin capturing its diagnostics too, which is the
mixing the second stream exists to undo. `Console::new(w)` fails to compile
instead, and the type's documentation says what to write.

### Effects, and what the streams do not differ in

All four operations are irreversible writes: bytes handed to a terminal cannot
be taken back, whichever stream carried them. All four are variadic over
`String`, join their arguments with a space, are not cancellable, are
recordable, and answer a task-safe `Result<Unit, Error>`. `eprintln` differs
from `println` in exactly one thing — where it writes — because anything else
would be a second way to print rather than a second place to print to.

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

**How a host separates the two streams, beyond handing over two writers.**
Capturing output while passing diagnostics through is a wiring choice now, and
there is no way to express it as a grant. If a host ever genuinely needs the
*authority* split — an embedder that must be able to prove a run cannot write
to one of them — this ADR is what it will have to argue against, and
"Alternatives considered" is where the argument already is.

**Whether a stream is a terminal.** Nothing here says whether either stream is
attached to one, and so nothing here supports a program that colours its
output when a human is reading and does not when a pipe is. That is a third
operation on `console`, and it wants its own reason.

**Moving `examples/cq`'s reports onto the new stream.** It is exactly what the
stream exists for and it is a change to a program, not to the language, so it
belongs to [issue #88](https://github.com/myuon/cove/issues/88) rather than
here. `crates/cove-cli/tests/examples.rs` pins today's behaviour — complaints
in the CSV and an empty diagnostic stream — so that the move shows up as the
improvement it is instead of as two goldens changing at once.

## Alternatives considered

**A `console.error` capability for the diagnostic stream.** This was written
first and then rejected, so it is worth recording as the argument it is rather
than as a note that someone said no.

The case for it: the issue asks for a host that captures a program's output
and lets its diagnostics through, and observes that "it needs the two to be
separately grantable to express it". A capability already belongs to an
operation rather than to a module — ADR 0001 says each operation describes its
own, and ADR 0013's second amendment made both `cove_sema` and the boundary
read it — so `console.error` needed no mechanism, only a string. It would also
have left `console` meaning precisely the set of operations it meant on the
day any given `allow` list was written, which is a genuine virtue: a
capability that quietly grows is the thing a capability may not do.

The case against, which is the one that decided it: authority split at the
stream is a finer grain than the thing being authorised. The two streams carry
the same risk to the same places, so the split protects nothing, and what it
costs is paid by every list already written — each one becomes a question
about which half of the console its author meant, when its author meant the
console. The configuration the issue wants is still available and is better
placed: it is two writers rather than two grants, and it belongs to whoever
wires the run rather than to whoever audits it.

The narrower reading of "a grant must not grow" survives that: `console` did
not gain authority over anything it could not already do, because writing to
the terminal is what it always meant. What it gained is a second place to put
the bytes, which is what the host chooses.

**A second host module.** Fewer questions about capabilities and two unrelated
names for one idea. See above.

**A `console.Stream` resource, with `out` and `err` as values.** The most
general shape, and it buys nothing: a program would open something that is
never closed and hold a handle to one of exactly two things, paying ADR 0013's
machinery for it.

**`console.log`, `console.warn`, `console.error` as levels.** A level is a
policy — which messages matter — and a stream is a destination. Deciding the
first in the schema would fix every program's idea of severity in the Host
API, where a program that wants levels can write them itself on the stream
this adds.

## Consequences

- A Cove program can be a filter: records on stdout, complaints on stderr,
  `cove run prog > out.csv` leaving the complaints on the terminal.
- Every `allow` list, every built binary's carried grants, and every
  embedder's `Grants::new(["console"])` keeps working and keeps meaning what
  its author meant, without anybody reading it again.
- A run's `--stats` irreversible-write count now includes diagnostics, which
  is what that number has always meant: writes that cannot be undone,
  wherever they went.
- `cove test` fakes both streams together, because `[test] allow_real` names
  capabilities and there is one to name. A test that reaches the console
  reaches the whole of it, real or sunk.
- `tests/e2e`'s harness had to stop treating stderr as evidence of failure. A
  case that writes diagnostics and succeeds is neither a failing case nor a
  case with empty stderr, so a case may now state the exit status it must exit
  with in an `expected.status` file, and `host_console_streams` is the first
  that does.
