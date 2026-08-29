# ADR 0018: Streaming file reading and writing

- Status: Accepted
- Date: 2026-08-26
- Implemented by: [PR #98](https://github.com/myuon/cove/pull/98)
- Implementation status: complete — `files.open`, `files.create`,
  `files.Reader`, and `files.Writer` all exist in the schema, in the real
  host, and in the in-memory fake, and `tests/e2e/host_files_streaming`
  drives them through the real binary. Standard input is deliberately not part
  of it; "What this does not decide" says why, and
  [issue #94](https://github.com/myuon/cove/issues/94) keeps the question.

## Context

Every `files` operation moves a whole value. `read` answers the entire file as
one `String` and `write` replaces the entire file from one, and there is
nothing between them: no open handle, no line, no chunk, no append.

That was enough for every program this repository had. `examples/codegen`
reads a token file of a few hundred bytes; `examples/restricted` reads a
document. Neither is large and neither is a pipeline.

`examples/cq` — [issue #88](https://github.com/myuon/cove/issues/88) — is the
first program meant to process more data than it wants to hold, and it cannot
be written. Its own success criterion is "at least 100,000 records without
retaining the full input", and `files.read` retains the input by construction:
the whole source text is live for as long as anything parsed out of it is, so
a run's `peak_bytes` reports the file rather than the program.

There is a second thing wrong with `read`, which is that it is the one shipped
host operation with no bound at all. `http`'s real implementation fixes the
request line at 8 KiB, the headers at 32 KiB, and the body at 1 MiB precisely
so that a peer cannot make the host allocate on demand, and
`crates/cove-runtime/src/http.rs` says so. `files.read` calls
`std::fs::read_to_string` and allocates whatever it finds.

[ADR 0013](0013-host-resource-handles.md) already decided the shape this
needs. A host resource handle is a name for something the host owns, every
operation on one goes through the same grant check, schema check, budget
charge, and trace as any other Host API call, and `ResourceSchema` is where a
resource kind declares what it answers. An open file is exactly that, and
nothing in this ADR is a new mechanism — what is new is what the mechanism is
pointed at, and four answers that mechanism has not had to give before.

## Decision

`files` gains two resource kinds and two operations that issue them.

```text
files.open(String)   -> Result<files.Reader, Error>
files.create(String) -> Result<files.Writer, Error>

files.Reader.readLine()      -> Result<Option<String>, Error>
files.Reader.close()         -> Result<Unit, Error>

files.Writer.write(String)     -> Result<Unit, Error>
files.Writer.writeLine(String) -> Result<Unit, Error>
files.Writer.close()           -> Result<Unit, Error>
```

Both are reached through the `files` capability. Both go through the same path
checks `read` and `write` already apply — the lexical refusal of an absolute
path, of `..`, and of a backslash, and the second check that follows symbolic
links — so a handle cannot name a place outside the root the host chose.
Granting `files` still grants exactly one directory, and now grants it in two
more shapes.

### A reader answers lines, not chunks

`readLine` answers the next line with its terminator removed, or `None` when
there is no next line. A final line with no terminator is still a line.

A chunk-oriented reader is the more general operation and the less useful one.
The formats this exists for — JSON Lines, CSV, logs — are made of lines, so a
program given chunks would have to find the line boundaries itself, in Cove,
one character at a time, on every byte of its input. That is the expensive
half of the work and it is the same in every program that does it.

The general operation is also not available. A chunk of a file is bytes, and
Cove has no byte type: the Host API vocabulary is `Unit, Bool, Int, String,
Duration, Error, Array, Option, Result, Named, Any`, and every one of the text
types in it is UTF-8 by construction. A `readChunk` answering a `String` would
have to either split on a character boundary the caller cannot see or fail on
a chunk that ends mid-character. Deciding that is deciding what a byte is in
Cove, which is a larger question than a file API should settle in passing.

So: lines now, because lines are what the data is; bytes when there is a byte
type, and not before.

### Reading is bounded

A line longer than 1 MiB is an error rather than an allocation, with the same
shape and for the same reason as `http`'s bounds: a host reads what it decided
to read, not what the input asked it to.

The bound is the host's and not the program's. A program calling `readLine`
has no way to say how long a line it is willing to receive — the call takes no
argument, and giving it one would make every caller answer a question about a
file it has not seen. The file is not the program's to trust either: it is
whatever was in the directory the host granted.

`files.read` stays unbounded. Bounding it now would break a program that reads
a large file deliberately, and such a program no longer has to: there is a way
to write it that never holds the whole thing. The unbounded operation is the
one that says in its name that it wants everything.

### Neither resource is task-safe

Both declare `task_safe: false`, and they are the first shipped resources that
do.

[ADR 0013](0013-host-resource-handles.md) states the rule: a resource whose
state the host keeps behind a lock says `true`, so two tasks holding the same
handle take turns rather than race. `http.Server` and `database.Connection`
both say `true` and both earn it, because taking turns at a listener or at a
connection is what those things are *for*.

A reader is a position in a file. A lock around a position is sound and it is
not meaningful: two tasks that take turns at one cursor each receive some
lines and neither receives the file, in an interleaving neither of them chose
and no test can pin. A writer is the same fact in the other direction — two
tasks taking turns at one output produce a file whose line order is the
scheduler's.

That is a mistake to refuse, not a race to prevent, so it is refused at the
task boundary with the diagnostic a `Vector` already gets. A program that
wants concurrent readers opens a reader per task, which is a thing it can say
and a thing that means something.

This is worth stating rather than assuming, because `task_safe` has had one
value in every resource shipped so far, and a field with one answer is not yet
a design.

### Effects

`open` reads. `create` is an irreversible write, because it truncates whatever
was there and no host can put it back — the same reason `write` and `delete`
are. `readLine` reads. `write` and `writeLine` are irreversible writes.
`close` on either is a reversible write, matching `http.Server.close` and
`database.Connection.close`.

The consequence is visible and intended: a run that writes 100,000 lines
reports 100,000 irreversible writes under `--stats`. That number is what it
has always meant — how much of what the run did cannot be undone — and a
program that wants it smaller writes fewer, larger pieces, which is a real
choice about the program rather than an accounting adjustment.

`readLine` is cancellable; `write`, `writeLine`, and `create` are not, for the
reason `files.write` already is not: a call in flight may already have reached
the disk.

### The fake gets both

`Files::in_memory` implements both kinds against its own tree, so `cove test`,
`crates/cove-cli/tests/examples.rs`, and `cove-bench` stay deterministic and a
test written against the fake exercises the same rules the real filesystem
enforces. That is the property `Files::in_memory` already had and this keeps
it.

## What this does not decide

**Standard input.** A `files` handle is rooted, and standard input has no path
inside any root, so `files.open` could not name it even if it wanted to. More
than that: granting `files` is granting one directory, and it must not also
grant whatever the process happened to be launched with. A program that wants
to be a filter in a pipeline needs a capability of its own, with its own
grant, and deciding what that is — whether it is a `stdio` module, whether it
is a third resource kind, what it does when there is no terminal — is a
separate decision. [Issue #94](https://github.com/myuon/cove/issues/94)
records it.

**Appending.** `create` truncates. An `files.append` opening a writer at the
end of an existing file is a small addition and nothing yet needs it.

**Seeking.** A reader goes forward. Nothing yet needs otherwise, and a `seek`
would raise the question of what a position means in a file measured in
characters and stored in bytes.

## Alternatives considered

**`files.lines(path) -> Array<String>`.** One operation, no resource, no
close. It also holds every line of the input at once, which is the thing this
ADR exists to stop, so it solves the ergonomics and not the problem.

**A byte-oriented reader.** More general, and it needs a byte type first. See
above.

**Making a reader something `for` iterates.** Cove's `for` iterates five
builtin collections, named one by one in the interpreter. Teaching it a host
resource means deciding what an iterator is in Cove — a protocol, a trait, a
builtin the language knows — and that is a language decision that should be
made for its own reasons, not arrived at as a side effect of opening a file.
A `while` loop over `readLine` is three lines and commits to nothing.

## Consequences

- `examples/cq` becomes writable, which is what [issue #88](https://github.com/myuon/cove/issues/88) was blocked on.
- A run's `peak_bytes` starts measuring the program rather than its input, so
  the heap numbers `--stats` reports become a fact about Cove code.
- `files` becomes the first host with both module operations and resource
  kinds where the resource kinds are the ones a large job uses, so the
  resource machinery from ADR 0013 now has a caller that is not a server or a
  database.
- The host-call count for a streaming job is one per line in each direction. A
  100,000-line transformation makes 200,000 Host API calls, each of them
  grant-checked, schema-checked, budget-charged, and traceable. Whether that
  overhead is acceptable is exactly the kind of question
  [ADR 0012](0012-performance-gate-and-native-backend.md)'s harness exists to
  answer, and `examples/cq` reports the number.
