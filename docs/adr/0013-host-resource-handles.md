# ADR 0013: Host resource handles

- Status: Accepted
- Date: 2026-08-25
- Amends: [ADR 0001](0001-mvp-language-design.md), whose Host API schema gains
  the "serialization and resource ownership" it asks for, and whose Host API
  boundary gains a second direction: a call may now run a Cove closure
- Amended by: this ADR's own "Amendment (2026-08-25): a host answers the type
  it declared" below, which makes `OperationSchema::result` a promise the
  boundary holds a host to rather than a signature it renders; and
  "Amendment (2026-08-25): a call passes the arguments it declared, and both
  ends say so", which does the same for `OperationSchema::params` and moves
  the schema into a crate the compiler can read
- Implemented by: PR #35; the first amendment by PR #46 and the second by
  PR #48
- Implementation status: complete — the boundary is built in both directions
  and both ends read the schema. What sits behind it is a separate question
  this ADR states rather than answers: `database` still ships only a fake and
  a denied host, and `http` speaks no TLS. A declared type's fields are still
  taken on trust at the boundary, and a host resource's task-safety is still
  the runtime's alone; the second amendment's "What is still not checked" says
  why.

## Context

A Host API call is a value in and a value out. `HostApi::call` takes a name and
a vector of `Value` and answers with one. That has been enough for every host
the runtime ships, and it is the reason three of the eight representative
programs did not execute.

`examples/server` asks a host to listen on a port and then to answer requests
on the thing that is listening. `examples/callbacks` asks for a database
connection and then makes queries on it. Both are the same shape: an operation
hands something back that the host still owns, and later calls are addressed
to *that* rather than to the module. `crates/cove-runtime/src/database.rs`
described the gap in its own module documentation and did not close it,
because `Value` had no variant that could mean "a live resource this host
still owns" and the interpreter had no branch that would send a method call on
a host-returned value back to the registry.

`examples/tasks` and `examples/callbacks` need something else that looks
unrelated and is not. `clock.timeout(500ms) { ... }` bounds a block, and
`clock.every(60s, handler)` repeats one; a route's handler is a Cove closure
the server has to run. The clock's schema said as much: `timeout` was absent
because "it takes the work to bound as a trailing closure, and `HostApi::call`
receives values but has no way to call back into the interpreter to run one."

So the boundary is missing two things, one in each direction. Something the
host keeps has to be nameable from Cove, and something Cove wrote has to be
runnable from the host. Neither is worth a framework and both are worth
deciding once, because the shipped hosts will keep needing them and the wrong
answer to either is very hard to take back.

ADR 0001 anticipated the first of them. It asks the Host API schema to
describe each operation's "serialization and resource ownership", and the
Language Card says "host resources declare task-safety in their Host API
schema". Neither sentence had an implementation.

## Decision

**A resource handle is a name.** The host keeps the resource; Cove holds an
identity that addresses it, and holds nothing else.

```rust
pub struct ResourceHandle {
    pub module: String,
    pub type_name: String,
    pub id: u64,
    pub task_safe: bool,
}
```

`Value::Resource(Arc<ResourceHandle>)` is the whole of the representation, and
every field of it is part of the name. A `database.Connection` is `module`
plus `type_name`; *which* connection is `id`, unique among the ones that host
issued; `task_safe` is the schema's answer copied onto the handle so a task
boundary can read it without consulting a registry. There is no field for
state, because the state is not here.

Nearly everything below follows from that one sentence, which is why it is the
decision and the rest is consequence.

### Identity, copying, and equality

A handle is copied like any other value: field-wise and shallow, which for a
name is the whole of copying it. Two handles are equal when they name the same
resource, since a name has no contents to compare. A handle shows as
`http.Server#1`, so two connections are told apart in a diagnostic by the
number the host issued and by nothing else.

Cove already has a value that is a name and not state — `Value::HostModule`,
which addresses `console` without being it. This is the same idea one level
down.

### Ownership and closing

The host owns the resource and holds the only record of which ones are open,
in a table keyed by the identity it issued. `close` is an ordinary operation
declared in the schema: it removes the entry, and the handle that named it
survives as a name for something that is gone. A later call finds nothing and
is reported:

```text
error[cove::runtime]: `http.Server#1` is closed, so `close` has nothing to act on
  rule: A host resource handle names a resource the host owns. Closing the
        resource ends the handle; the name outlives it and addresses nothing.
```

That is a `RuntimeError` and not a Cove `Err`. A query against a closed
connection is not an expected failure a program should be handling — it is the
program having kept a name past the thing it named, which is the same kind of
mistake as awaiting a cancelled task.

Identities are never reused. A host counts upward, so a stale handle can only
ever find an empty slot, never a different resource that has since taken the
number. Without that rule a use-after-close would silently act on somebody
else's connection, which is exactly the failure this design exists to make
impossible.

### Task safety

`ResourceSchema` carries `task_safe`, which is the Language Card's sentence
applied to a host's own types. A resource whose state the host keeps behind a
lock says `true`, and `Transfer::of` then carries the handle across a task
boundary exactly as it carries a string. One whose state belongs to the task
that opened it says `false`, and the handle is refused at the boundary with
the same diagnostic a `Vector` gets.

Both shipped resources say `true`, and both earn it: `http.Server` keeps its
listener behind a mutex and `database.Connection` keeps its table behind one,
so two tasks holding the same handle take turns rather than race.
`examples/callbacks` depends on it — the repository handle is captured by an
`App` that a spawned task receives.

This is the reason `task_safe` is copied onto the handle rather than looked up.
`Transfer::of` is a pure walk over a value with no registry in reach, and
giving it one so that it could ask a question the schema had already answered
would be handing the task-safety rule a dependency on the host set.

### Scope and task exit

Leaving a scope neither closes a resource nor invalidates a handle.

A resource is owned by the run, not by the task or the scope that opened it,
and that is forced by the previous decision rather than chosen beside it: a
task-safe handle may be copied into a sibling task and into a `Shared`, so
"the task that opened it" is not a thing the runtime can identify at scope
exit, and closing on the guess would close a resource somebody else is still
holding. So the rule is the simple one — a resource lives until it is closed
or until its host is dropped with the run.

The consequence is that a program can leak a connection by never closing it,
and Cove does not diagnose that. This is the same bargain Cove makes
everywhere else: no finalizers, no destructors, no `defer`. A run is bounded
by its budget and its host outlives nothing, so the leak is bounded by the
run, and a leak the run reclaims is not worth a language feature to prevent.
If it becomes one, the answer is a scope-bound resource — `task_safe: false`
plus a close at scope exit — and this design already has the field that would
select it.

### Schema typing

`ResourceSchema` declares a resource kind: its name, its task-safety, and the
operations a handle answers. Each operation is an ordinary `OperationSchema`,
so a resource's operation says what it takes, what it produces, which
capability it needs, whether it reads or writes, whether it is cancellable,
and whether it is recordable — the same facts, in the same shape, as a
module's.

Two smaller additions come with it. `HostType::Named("http.Response")` lets a
signature name a host's own type, so `connect(String) -> Result<database.Connection, Error>`
renders in a diagnostic the way it would be written in Cove. And
`HostType::Any` is the type of a parameter whose meaning does not depend on
which value it is — `http.json` renders whatever it is handed, and a callback
a host stores and calls later is a value the host never looks inside. `Any` is
not a hole in the schema; it is a claim, and the claim is that the operation
does not care.

A host may also declare plain-data types, in `TypeSchema`: `http.Method` is an
enum whose cases carry nothing and `http.Route` is a struct initialized with
labels. These are *not* resources and get no handle. They are built as ordinary
`Value::Enum` and `Value::Struct` whose type name is qualified by the module,
so `http.Route(method: http.Method.Get, path: "/health", handler: health)`
reads and behaves like a Cove struct initializer, and the host's data needs no
representation of its own. The line between the two is whether the host keeps
anything: a route is data the program owns, and a listener is not.

The type checker does not read any of this. `cove-runtime` depends on
`cove-sema` and not the other way round, so a host type in a signature is
still reported as unchecked. That is a real limitation and it is unchanged by
this ADR; what changed is that the schema now exists to be read when something
inverts that dependency. The second amendment below is that something: the
description moved *below* both crates rather than inverting anything, and the
checker reads it.

### Capability requirements

A resource operation is gated exactly as a module operation is, because it is
dispatched through the same code. `HostRegistry::dispatch` carries the grant
check, the schema check, the arity check, the budget charge, and the trace;
`call_with` and `call_resource` differ only in how the callee is named. A run
without the grant is refused before the host is reached:

```text
`database.Connection.query` requires the `database` capability, which this run
was not granted
```

Holding a handle is therefore not authority. A handle that crossed into a task
whose run lost the grant — which cannot happen today, since grants are fixed
for a run, but which is exactly what a filtered or remote host would want to
change — would be refused at the call and not at the crossing. There is one
choke point, and it is the same one it has always been.

### Trace identities and replay

Because a handle is a name, recording one records the name:

```json
{"type":"resource","name":"http.Server","id":1}
```

An operation on a handle is recorded under the name that says which resource
answered it — `Server.close` rather than `close` — with the handle itself as
the call's first argument. That second part is what makes a trace of a run
holding two connections say which one each query went to.

A replay then needs no special case at all. `http.listen` is answered with the
identity the trace recorded; the calls made on that identity are matched
against the recorded ones, handle included; and no socket is opened, because
nothing on this path was ever a socket on Cove's side. `cove replay` reproduces
creation, use, and closure by reproducing three names, which is the whole
payoff of the decision at the top.

An operation that opens a resource is therefore `recordable`. The schema's
`recordable` documentation used to say that an operation returning a live
resource handle is not recordable; that was true of a design where the handle
carried the resource, and it is not true of this one.

Handles read out of a trace are taken to be task-safe, and they are: a handle
that reached a trace crossed the boundary that records values, and only a
task-safe one can — a resource its schema keeps to one task is recorded as
opaque instead.

### Real, fake, filtered, remote, and denied

Nothing about a handle constrains what is on the other side of it, which is
the point of it being a name.

`http` ships all three of the ones that make sense for it. The real host binds
a socket; the recorded fake fabricates a listener that replays a scripted queue
of requests and remembers what was served; the denied one refuses to listen at
all. A Cove program cannot tell which it is holding, which is what makes
`examples/server` testable without a client.

`database` ships a fake and a denied one, and still no real one, for the reason
that module has always given: connecting to a database means speaking a wire
protocol to a server, and this runtime depends on nothing but the standard
library. What changed is only that `connect` now exists and the denied
implementation refuses it by name.

A filtered host is a host that narrows what a handle can address, and the real
`http` is already one in the direction that matters: `listen` binds loopback
only, because granting a run the network should not publish a port on every
interface the machine has. A remote host is a host whose table lives in another
process; the handle is unchanged, since an identity is exactly what an RPC can
carry, and that is the case this design was shaped to leave open rather than
one it implements.

### Inside a sealed `cove build` artifact

Nothing changes, and nothing needs to. `register_hosts` is the one place the
host implementations a run gets are chosen, and `cove run` and a built binary
both call it, so a built binary holds the same real hosts and issues the same
handles. Its grants were fixed when it was built, so a refused resource
operation tells the reader to rebuild rather than to edit a `cove.toml` the
binary will never read — which is the `GrantSource::Sealed` help, reached
through the same dispatch as everything else.

A handle is not a bearer token. It is not signed, it grants nothing, and it
cannot travel: there is no syntax that writes one down and no operation that
takes one from outside, so the only way to hold one is to have been handed it
by a host in this run. An identity is meaningful only against the table that
issued it, and a fresh host starts with an empty table and its own numbering.
That is worth stating out loud, because a design where a handle *did* travel
would be a design where a file could grant authority, and a sealed artifact
exists precisely to prevent that.

## The other direction: `Reentry`

A host that was handed a Cove closure needs a way to run it. `Reentry` is that
way and the only one:

```rust
pub trait Reentry {
    fn call(&mut self, callee: &Value, args: Vec<Value>) -> Result<Value, RuntimeError>;
    fn call_until(&mut self, callee: &Value, args: Vec<Value>, stop: &Cancellation)
        -> Result<Value, RuntimeError>;
    fn is_cancelled(&self) -> bool;
}
```

The callback runs on the task that made the host call, on that task's stack,
charged to that run's budget. There is no second thread and no scheduler,
because concurrency in Cove belongs to a task scope the program wrote, and a
host that spawned work would be putting Cove code outside every control the
run was given. `HostApi::call_with` defaults to `HostApi::call`, so a module
that never runs a callback is unchanged and cannot accidentally acquire the
ability.

`call_until` is what makes a timeout a timeout. `clock.timeout` starts a
watchdog that raises a `Cancellation` when the bound is reached, and the
bounded body observes that flag at the same safepoints where it observes its
own task's cancellation — so the work stops, and the caller is told it did,
rather than being told afterwards how long it took. A virtual clock has no
thread to raise anything, because it has no time of its own; it judges after
the fact, from how far the body pushed the clock. `is_cancelled` is what a
repeating timer reads between rounds, so cancelling the task holding a
`clock.every` ends the timer.

"Its next safepoint" carries the same caveat cancellation has carried since
ADR 0003: a body already inside a host call, or already waiting on a task's
thread, reaches its next safepoint when that returns. So a timeout bounds the
work rather than the wall clock, and a body that overran because a `fetch`
took longer than the bound still reports the bound — it just reports it once
the `fetch` has come back. Making the bound cut a wait short means teaching
the wait itself to be interruptible, which is a change to how tasks and hosts
block and not a change to this boundary.

### The loop belongs to the program

`http` exposes `Server.handle(routes)`, which serves one request and answers
whether one arrived, rather than a `serve(port, routes)` that runs until told
to stop. The loop is written in Cove:

```cove
while server.handle(table)? {
  served += 1
}
```

A `serve` that never returned would be a single host call, and a host call is
not a safepoint: the run's fuel, deadline, and cancellation would all stop at
its edge and a server would be the one program none of them could reach. With
the loop in Cove they reach it exactly as they reach anything else, and a fake
listener ends the loop by running out of requests, which is what lets a server
be a test.

## Consequences

The three programs that only parsed, resolved, and type-checked now run, and
they run against fakes in CI: `crates/cove-cli/tests/examples.rs` drives all
three through a listener with a scripted queue, a `fetch` with recorded
answers, a clock that moves only when something moves it, and a database of
canned rows.

`Value` gains a variant, which every walk over values has to answer for.
`Transfer` answers with the schema's `task_safe`; the trace encoding answers
with a name and a number; `eq_value` answers with identity; the collector
answers with nothing, since a handle owns no Cove object and cannot be part of
a cycle.

`HostApi` gains four defaulted methods — `types`, `resources`, `call_with`,
and `call_resource` — so every existing host is unchanged and every new one
opts in to exactly the parts it needs. `HostRegistry::call` is now
`call_with(..., &mut NoReentry)`, and `NoReentry` refuses to run a callback
with a sentence saying it was not called from a running program, which keeps a
host usable from a test or a tool with no interpreter behind it.

The help on an unknown operation changed shape, from "`documents` exposes
`read`" to "host module `documents` exposes `read`", because one dispatch now
serves two kinds of callee and the owner has to be named. That is a small
regression in brevity bought for a real gain in uniformity.

## What this deliberately leaves out

No resource is scope-bound; no handle is closed for you; there is no `defer`,
no `using`, and no `Drop`. A generic resource framework would have had all
three and would have been designed against two resources, which is not enough
use to earn it. ADR 0001's remaining word in that sentence, "serialization",
is still unmodelled for the same reason: every value that crosses the boundary
is an ordinary `Value`, and a field nothing consults is a claim nothing checks.

The type checker still does not read the schema, so a host type in a signature
is still unchecked at compile time and every operation on it is still left to
the runtime. Closing that needs a dependency inversion between `cove-sema` and
the host set — most likely the schema moving to a crate both can see — and it
is a decision worth making on its own rather than inside this one.

## Amendment (2026-08-25): a host answers the type it declared

`OperationSchema::result` has existed since this ADR's first line, and nothing
read it except to render a signature in a diagnostic. Issue #38 asks for the
promise ADR 0001 makes about it to be kept — "Each operation describes its
argument, result, and error types", shared by the compiler, runtime, and CLI —
because a description nothing enforces is a comment. It matters most for a host
that is not the toolchain's: the "real, fake, filtered, remote, and denied"
implementations above all have to agree about what an operation answers, and a
fake that has drifted from the real one is exactly the bug a shared schema is
supposed to make impossible. Until now a host could declare
`Result<String, Error>`, answer `3`, and be dispatched; the `Int` failed
somewhere else, and the diagnostic named the Cove code that received it rather
than the host that broke its word.

### Where the check runs

In `HostRegistry::dispatch`, after `invoke`. That is the one choke point every
Host API call already passes through — the grant, the operation, the arity, the
budget, the irreversible-write counter, the trace — so a module's operation and
a handle's operation are both covered by one rule written once, and a host
cannot be reached by a path that skips it.

It runs after the trace is written, not before. What the host actually did
belongs on the record either way: a trace of a run that stopped here shows the
answer that stopped it, which is the first thing whoever wrote that host will
want to see. The value is what is refused, not the fact.

Only a value is checked. A host that answers `Err` has already failed on its
own terms, and the `Error` a schema declares is the one inside a Cove `Result`,
not a `RuntimeError` beside it.

### How deep it goes

Structurally, following `HostType`'s own recursion: `Array`, `Option`, and
`Result` are checked through to what they contain, so an `Array<Int>` is not
admitted where an `Array<String>` was declared. A shallow check would have
admitted it, and the schema says more than "an array" — writing the element
type and then not reading it would be the same comment this amendment is
removing.

`HostType::Any` admits everything, which is not a hole in the check for the
same reason it is not a hole in the schema: "Schema typing" above already calls
it a claim rather than a gap, and the claim is that the operation does not care.
`clock.timeout` and `clock.every` declare it of the work they are given.

The cost is a match on a value the boundary already holds, plus one walk of an
array a host had to build first. Nothing is allocated unless a value fails: the
path to the part that disagrees — `Ok(_)[1]` — is assembled on the way out of
the recursion, so a run in which every host keeps its word pays for no strings
at all.

### What `Named` means

The name, and only the name. Every value carries the qualified type it was
built with, and a `ResourceHandle` carries the module and kind it was issued
for, so `database.Connection` is checked by asking the value what it is — no
registry lookup, no second table walk on a path every host call takes. That is
a real check, and it is the same reasoning `call_resource` already used when it
refused a handle naming a kind the module never declared; the difference is
that the lie is now caught where it is told rather than at the next call made
on it.

What it does not check is a `TypeSchema`'s fields. A value calling itself an
`http.Response` is taken at its word about what is inside it. The shipped hosts
do not justify more: `http.Response` is the only declared type any operation
produces, its two fields are built by one function three lines long, and the
check would need the registry the paragraph above avoids. If a host is ever
written whose declared types are minted in more than one place, that is the
moment to reconsider, and this paragraph is the record that it was a decision.

### What a violation is

A `RuntimeError` that stops the run, carrying the operation, what it answered,
where the disagreement is, and the signature its schema declares. A host that
breaks its own schema has broken an invariant on the host's side of the
boundary — no Cove program asked for the value and none can handle it — so it
is not expected failure, which in Cove is a `Result` the program was given the
chance to match on.

Issue #38 raises warning-first, on the grounds that this makes a previously
working embedding fail. Every host the toolchain ships passes its own schema
check, verified by running each of them against the check rather than by
reading them, so nothing that works today starts failing. An embedding that
would newly fail is an embedding whose host is already lying about itself, and
a warning would ask its author to keep reading past the first line of a
diagnostic to find that out.

### What is still not checked

A call's arguments. The same machinery would point at `schema.params`, and the
sixteen hand-written `let [Value::Str(name)] = args.as_slice() else { ... }`
arms across the shipped hosts would become the `unreachable!` the operation
arms beside them already are. It is left out on purpose: a wrong argument is
usually the *program's* mistake, made at a call site with a span, and it is the
same mistake `cove check` should catch before the run starts — so what the
boundary says about it cannot be settled without settling what the checker says
about it, and the checker still reads no schema at all. Both halves are
[issue #44](https://github.com/myuon/cove/issues/44), and both are settled by
the amendment below.

## Amendment (2026-08-25): a call passes the arguments it declared, and both ends say so

The amendment above left one half of ADR 0001's sentence unkept. An operation
describes "argument, result, and error types"; the result became a promise the
boundary holds a host to, and the arguments stayed what they had always been —
counted by the boundary, never looked at, and restated by hand in sixteen
`let [Value::Str(name)] = args.as_slice() else { ... }` arms across eight
modules. The checker, meanwhile, said of itself that it had "nothing to check
a host call against", so `cove check` warned at `http.Request` and its
neighbours rather than checking them and a wrong argument reached the run.

Issue #44 asks four questions. This amendment answers them.

### Where the description lives

In `cove-schema`, a crate below both `cove-sema` and `cove-runtime`.

This is the answer the other three depend on. `OperationSchema` lived in
`cove-runtime`, which depends on `cove-sema` and not the other way round —
"Schema typing" above records that as the reason the checker read none of it —
so the compiler could not see the description it was supposed to share. The
dependency must not be inverted: a compiler that depends on a runtime is a
compiler that cannot be used without one. So the description moved below both
of them. `HostType`, `Effect`, `OperationSchema`, `FieldSchema`, `TypeSchema`,
`ResourceSchema`, and `ModuleSchema` are `cove-schema`'s, and so are the
shipped hosts' own tables; `cove_runtime::schema` re-exports every one of them,
so a host written against the runtime still names one crate.

The crate has since gained two more tables on the same argument.
[ADR 0004](0004-static-type-checking.md)'s "Amendment (2026-08-25): one builtin
table" moves the builtin methods and associated functions there as well, and
says why they needed a vocabulary of their own rather than `HostType`: a host
signature is monomorphic and a builtin's is not. Its second amendment, "the
builtins that are called on nothing", moves the constructors and the
assertions, which have no receiver to be keyed by and so are a table of their
own rather than an entry in that one.

What did not move is `HostType::admits`, which is about values and needs one.
It stays beside `Value` as the `Admits` trait. The schema describes types; the
runtime owns values; a crate that held both would be a runtime again.

The shipped hosts' tables moved with the types, and that is the part worth
arguing. Each `HostApi` now answers `schema()` with its entry from
`cove_schema::hosts::SHIPPED` rather than a static of its own, so the
description a run enforces, the one `cove check` checks a call against, and
the one `cove trace` reads out of a recorded file are the same bytes rather
than copies that agree. The alternative — a second table in `cove-sema` and a
cross-crate test holding it against the first — is what the compiler's list of
host module *names* already was, and that list is how `http` came to be
missing from it: a package module named `http` shadowed the host module and
nothing said a word until a test was written to compare the two. One list
cannot drift. The cost is that a host's schema and its implementation are now
in different crates, which is a real cost and the reason this paragraph exists.

### Both ends check the arguments, and neither is redundant

They are not alternatives, and issue #44's first question — whether the
boundary checks arguments at all, "or whether an argument is the checker's
business once the checker reads the schema" — has to be answered *no* on both
sides.

The checker cannot see an embedder's host. A `HostApi` implemented outside
this workspace and registered at run time is named in no table any compiler
reads, and ADR 0001's whole point is that such a host is ordinary: "real,
fake, filtered, remote, or denied implementations". For a program written
against one, the boundary is the only thing standing between it and an
argument the host's own schema does not admit.

The boundary cannot point at a call site. It has the operation, the values,
and no source: a diagnostic from it names the call that failed and the run
stops. `cove check` has the span, catches the mistake before anything runs,
and catches it in code that never runs at all — which is where a wrong
argument most often hides.

So both, and each says the same thing about the same table.

### What the boundary does

`HostRegistry::dispatch` checks each argument against `OperationSchema::params`
where it already checked the arity: before the host is reached, before the
budget is charged, and with nothing written to the trace, because a call
refused there never happened. Arity and types are one check on one
declaration, and they are made together.

The check is the same walk the result check makes, run on the way in instead
of the way out: structural through `Array`, `Option`, and `Result`;
`HostType::Any` admits everything, which is what `clock.timeout` declares of
the work it bounds; and a `Named` type is checked by the name the value
carries. Nothing is allocated by a call that keeps its declaration.

A violation is a `RuntimeError` that stops the run, for the reason the result
check gives: the program asked for something the operation does not offer, and
there is no value the host could produce that would make it right. It names
the operation, which argument, what was found, and where — `` `documents.read`
was given `Int` as argument 1, but its schema declares `String` there `` — and
quotes the same signature the boundary's other diagnostics quote.

The sixteen hand-written arms are gone. Each is now the
`unreachable!("checked by HostRegistry::call")` that the operation arms beside
them have always been, including the one an embedder writes: an embedding host
declares its schema and the boundary holds every call to it, so restating the
declaration in Rust is writing the same thing twice.

### What the checker does

`cove-sema` reads the same entry. A call into a shipped host module is checked
at its call site — its arity, each argument against the type declared for it,
and the result becomes the type the schema declares rather than `Unknown`.

A host type is a type. `Ty::Host("http.Request")` is nominal, compared by the
name the schema wrote, so `fn health(request: http.Request) -> http.Response`
is checked like any other signature; its fields come from the schema, its
cases come from the schema, and an operation on a resource handle —
`server.handle(routes)`, `repository.query(sql)` — is checked against that
kind's own `ResourceSchema`. `HostType::Any` becomes `Ty::Unknown`, which is
not a loss: a type that carries no constraint and *the checker does not know*
mean the same thing at a call site.

The `cove::type::host_type` warning survives, narrowed to what it can honestly
say. It no longer greets every host type; it greets a type from a host module
this build ships no schema for, which is the one case where the checker really
does abstain and the boundary really is alone. `cove check` on `examples/`
reported eight of those warnings and reports none: every host type those
programs name is one a shipped module declares.

### What is still not checked

Three things, and they are worth stating exactly.

A declared type's **fields are not checked by the boundary**. The first
amendment's reasoning is unchanged: a value calling itself an `http.Response`
is taken at its word about what is inside it. The checker does read those
fields — `request.path` is a `String` because the schema says a `Request` has
one — so what is checked there is that the *program* built the value the
schema describes, not that the host did.

A host resource's **task-safety is the runtime's alone**. `ResourceSchema`
declares it and the boundary enforces it; `Ty::Host` says nothing about
crossing a task boundary, so a resource declaring `task_safe: false` is
refused where it crosses and not before. No shipped resource declares it, so
there is nothing to check today and nothing to test against; the day one does
is the day to move it.

A **host module the toolchain does not ship** is unchecked by the compiler, by
construction. That is not a gap to be closed — it is the reason the boundary
checks arguments at all.
