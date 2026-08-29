# ADR 0022: The VM is the default backend

- Status: Accepted
- Date: 2026-08-29
- Supersedes: [ADR 0009](0009-cove-build.md)'s decision that the executable
  `cove build` writes embeds the tree-walking interpreter — "a binary that
  embeds an interpreter is a native executable" — and with it the property
  that a built binary's throughput is the interpreter's. Everything else in
  ADR 0009 stands and this ADR leans on it: `cove build` still produces one
  self-contained native executable, it still generates no machine code from
  Cove, its grants and limits are still sealed at build time, and its rule
  that "a built binary must not defer an error to whoever runs it" is what
  decides where the lowering happens below
- Supersedes nothing in [ADR 0019](0019-executable-ir-and-vm.md).
  ADR 0019 decided that the VM exists, that the interpreter stays the
  semantic oracle, and that
  [issue #111](https://github.com/myuon/cove/issues/111) would decide
  adoption. This ADR is #111's answer, not a revision of the question: its
  sentence "until #111 passes, the interpreter is the default backend" was
  satisfied rather than contradicted, and its no-silent-fallback rule is
  enforced here in four more places than it was
- Implementation status: complete for `cove run`, `cove generate`,
  `cove test`, and `cove build`. `cove replay` deliberately stays on the
  interpreter, argued below and tracked by
  [issue #140](https://github.com/myuon/cove/issues/140)

## Context

[ADR 0019](0019-executable-ir-and-vm.md) built a linear executable IR and a
VM that runs it, kept the tree walk as the oracle, and refused to make the
VM the default until #111's evidence existed. It said what the evidence had
to be, and it said what the rule around it was: **a run either finishes on
the VM or fails before any side effect**, never quietly on the interpreter,
because a VM that falls back is a VM whose conformance is about a mixture.

#111's three blockers are closed.

**[#119](https://github.com/myuon/cove/issues/119) — the VM had no
collector**, so a program that ran in bounded memory on the oracle need not
on the VM. Closed by [#127](https://github.com/myuon/cove/pull/127): roots
became a trait so each backend describes its own, and the VM's are its value
stack up to its length and its open task scopes, with every `Vm` field
enumerated and accounted for.

**[#125](https://github.com/myuon/cove/issues/125) — four refusals differed
in *when*, not *whether*.** The oracle refused them at run time, after
producing output; the VM refused them at lowering, before any. Closed by
[#138](https://github.com/myuon/cove/pull/138): six constructs — not five,
the class was larger than the issue's table — became `cove check` errors in
the interpreter's own words, and the matching run-time refusals were deleted
in the same change, in pairs, because deleting from one backend alone would
leave it the more permissive one.
[ADR 0021](0021-places-are-a-static-fact.md) records it.

**The differential harness did not compare a trace.** Closed by
[#139](https://github.com/myuon/cove/pull/139).

### What the evidence says

`crates/cove-cli/tests/differential.rs` runs every program this repository
keeps — every `[run.<name>]` under `tests/e2e/`, `examples/`, and
`benches/` — on both backends against the same deterministic fakes, and
compares the answers. Of 122 cases, 93 lower and agree; 28 do not check, so
there is nothing to run; 1 is refused. **Of the 94 that check and have
something to run, 93 do, and there are zero disagreements**, verified over
thirty consecutive runs.

Compared exactly: the value the entry answered or the error it failed with,
every line written to the console in order, how the run ended and what it
said, the fake filesystem as the run left it, and the JSONL recording —
`entry_enter` and `entry_exit`, every `host_call`'s task, module, operation,
capability, grant, arguments and outcome, every `task_spawned`'s id, parent
and scope, and what `heap_summary` says a run allocated, in objects, in
bytes, and in collections.

Normalized away, each for a stated reason rather than because it differed:
every `Duration`, since all three are wall time; `heap_collected` entirely,
because both what it says and where it stands are the collector's schedule;
and `live_bytes` and `peak_bytes`, because a live set is decided by a root
set and the VM's is a frame's slots where the interpreter's is an
environment chain. `live_bytes` agrees on 92 of the 93 anyway.

Two apparent disagreements were established as races the honest way, by
running each backend against *itself*: `fail_max_tasks` records
`task_completed` for task 1 in 3 of 20 runs on the interpreter alone, and
`examples:callbacks` flips the same way on the VM alone.

And it is faster on everything, by `cove-bench --iterations 15`: `call`
5.14×, `pure` 5.07×, `arith` 4.55×, `method` 2.93×, `chars` 2.15×,
`arrayget` 2.06×, `field` 1.88×, `hostheavy` 1.24×. `hostheavy` is the floor
and should be — both backends reach a host through the same registry.

## Decision

**The VM is what runs a Cove program.** `cove run`, `cove generate`,
`cove test`, and `cove build` all reach it without being asked, and
`--backend ast` is how the interpreter is asked for. The interpreter stays
selectable, stays the oracle, and stays unoptimized, exactly as ADR 0019
said it would.

The default lives in one function, `Backend::default_for_a_run`, rather than
as a literal at each command. Four commands making one decision could be
changed in three places, and a toolchain whose commands disagreed about
which backend runs a program would be ADR 0019's mixture arrived at by a
different road.

Each command is a separate decision, and they are argued separately because
they do not all cost the same thing.

### `cove run` moves, and takes `cove generate` with it

`cove run` already chose; only its default changed. `cove generate` had no
choice at all, and gets one now, but it moves because it cannot sensibly
stay: ADR 0010 makes a generator "an ordinary capability-controlled Cove
entry", and `execute_entry` is the one function both commands run an entry
through. Pinning a generator to the other backend would make it a second
kind of run, which is the thing that single seam exists to prevent.

So `cove generate` takes `--backend`, and it is the only `cove run` flag it
takes: every other budget is `[run.<name>]`'s, per ADR 0010, and this one is
not a budget. Without it a package whose generator reaches a construct the
VM cannot run would have no way to regenerate at all.

### `cove test` moves, and lowers once per test

The argument for moving it is the one that matters most in this ADR: **a
suite that passes on a backend nobody runs is not a gate.** `cove test` is
how a Cove programmer finds out whether their program works, and if it
answered about the interpreter while `cove run` answered about the VM, the
suite would stop being evidence about the thing being shipped.

Lowering happens per run and a suite is many runs, so it is paid per test:
`cove_ir::lower::lower_entry` is called with the test's own entry, and what
it lowers is what that test reaches. That granularity is the point rather
than an accident. A construct the VM cannot run fails **that test**, by
name, with the construct named — and the rest of the suite still runs and
still reports. Lowering the package once instead would have been one call
and would have refused every test in a package that held one unlowerable
declaration anywhere.

The cost is measurable and small. `examples/`, 75 tests over 29 files: 0.21 s
on the VM and 0.21 s on the interpreter. The lowering is inside the noise,
which is the ratio ADR 0019 predicted when it allowed the lowering to be
slow.

### `cove build` moves, and pays for it twice

This is the one that costs something, and it is why ADR 0009 is superseded
in part rather than merely referred to.

A built binary runs on the VM. It has to lower to do that, and ADR 0019
declined to make the IR a serialization format — "no `.covec` file" — so a
binary cannot carry one. It carries its sources, as ADR 0009 decided, and
lowers them when it starts.

It also **type-checks** them when it starts, which an interpreted binary
does not. That is not a second opinion about whether the program is well
formed; ADR 0019 makes the IR a recording of the checker's answers rather
than a second derivation of them, and resolution alone does not produce
those answers. The check is how they get made. Its diagnostics are
unreachable for a binary `cove build` wrote, because that command refuses to
write one for a package that does not check.

Measured: `examples/hello`, whose binary embeds all 29 files of the
`examples/` package, starts in 18.9 ms on the VM against 14.2 ms on the
interpreter — about 4.7 ms, on a program that computes nothing, and it
scales with the package rather than with what the entry reaches, because the
check does. ADR 0012's gate 2 asks that warm process startup stay under
roughly 50 ms for a trivial entry, and both figures are comfortably inside
it. Any program that does real work wins the 4.7 ms back many times over,
which is why the artifact people deploy should not be the slower one.

The second payment is at build time, and it is deliberate. **`cove build`
lowers the same entry and refuses to write a binary it would refuse to
start.** ADR 0009 already says a built binary "must not defer an error to
whoever runs it", and a refused construct is exactly such an error: it names
a place in source, and the person holding the source is not the person
holding the binary — who, per ADR 0009, has no flag to pass and no
`cove.toml` that would be read. The IR built at build time is thrown away;
what it buys is the moment the refusal arrives, not the work. `cove build
--backend ast` is the escape hatch, and it is baked into the binary like
every other decision that command makes.

### `cove replay` does not move

`cove replay` builds an `Interpreter` unconditionally and takes no
`--backend`. It stays that way, and that is a decision rather than an
oversight.

What would have to be decided to move it is not a default. A replay is
driven by a file rather than by a host, so what a VM replay does when the
tape runs out, or holds an answer for a call this backend did not make, is a
question with ADR 0019's no-silent-fallback rule attached to it — and
answering it inside a change that moves four other commands would be
answering it quietly. [Issue #140](https://github.com/myuon/cove/issues/140)
already holds the question.

What this ADR does change is how much it matters. Before it, replaying on
the interpreter was the exotic direction, reached only by someone who typed
`--backend vm` when recording. After it, **an ordinary `cove run --trace`
followed by an ordinary `cove replay` is a cross-backend replay**, because
the recording is the VM's and the replay is the interpreter's. Nothing is
known to diverge — #139 compares the same tape, field for field, over 93
programs — but a divergence this command reports could now be about the two
backends rather than about the program, and the command's own documentation
says so.

## What this gives up

### A Cove program can be written that no longer runs

The lowering refuses what it does not cover, by name, and `--backend ast` is
the only way to run such a program. `crates/cove-ir/src/lower.rs` has 71
`Unsupported` call sites; most of them cannot be reached from a program that
passes `cove check`, either because the checker rejects it first — several
of them since [ADR 0021](0021-places-are-a-static-fact.md) — or because they
guard an invariant of the lowering that a bug, not a program, would break.

What is left is the real cost, and a user is entitled to the list.

Confirmed by running each one through `cove run`:

- **a function declared inside a function body**;
- **a variadic parameter shape nothing decided a meaning for**: `var`,
  written with a default, or standing anywhere but last;
- **a `...` spread argument to anything that collects nothing** — anywhere
  but a declared variadic parameter;
- **a `var` argument to something whose parameter list was not written with
  a marking**: a builtin, a host operation, a struct's synthesized
  initializer, an enum case, a closure, a `dyn` method, or a task operation;
- **a `var` marking that disagrees between the declaration and the call
  site**, in either direction — the one member of ADR 0021's class that ADR
  deliberately left at run time;
- **a call that leaves a `var` parameter to its default**;
- **a closure parameter written `var`, or written with a default**;
- **a declared function used as a value** when one of its parameters is
  `var`, is variadic, or has a default;
- **a host operation used as a value**, such as `let write =
  console.println`, and **another module's declared function used as a
  value**, such as `lib.twice`;
- **a parameter default that names a parameter standing later in the list**;
- **a call through a value expression**, such as `make()()`;
- **a task scope in a function that answers an `Int` or a `Bool`**;
- **a `lock` whose closure is not written at the call**, and **a labelled
  argument to `lock` or `spawn`**, which take none;
- **a labelled argument to a method called through a `dyn`**.

Established by `lower.rs`'s own tests, whose fixtures must pass `cove check`
before they are lowered, rather than re-run here:

- **`snapshot` on a receiver a conformance answers**, such as a `Vector<B>`
  where `B` implements it;
- **a `dyn` written where the type conversion does not reach it** — inside a
  `Map`'s value type, or in a written function type's parameter — as a
  return type, a binding, a field, or a parameter;
- **a `dyn` call that supplies fewer arguments than the trait declares**,
  which a defaulted trait-method parameter allows;
- **a labelled argument naming a variadic parameter that is also passed
  more**.

Two constructs that looked like they belonged on this list do not, and are
recorded because checking them is what makes the rest of it worth reading: a
**positional argument after a labelled one** is a parse error
(`cove::parse::positional_after_label`), and a **type declared inside a
function body** does not resolve. Only the *function* half of that last one
is reachable.

The refusal is scoped by reachability, which softens all of the above by
exactly as much as it should: `lower_entry` lowers what the entry reaches,
so an unlowerable declaration nothing calls refuses nothing.

**The corpus exercises one of them.** `tests/e2e/backend_unsupported` names
a function declared inside a function body, and it is the only refusal in
123 cases. So the honest shape of this break is two numbers that do not
agree: the corpus says the flip costs one construct, and the code says it
costs about fifteen. Neither is wrong. The corpus is what this project
writes, and this project does not write those constructs.

### The collector is new

It was written for this gate, in #127, and it has run for a few days.
Nothing in the corpus disagrees, and the corpus compares what a run
allocated — objects, bytes, and collections — exactly. But two heap
statistics are *excluded* from that comparison, `live_bytes` and
`peak_bytes`, because a live set is decided by a root set and the two
backends have different ones. A program sitting close to a heap limit can
therefore behave differently on the two backends, and the differential
harness would not catch it. `tests/e2e/gc_struct` exists because the
*oracle* was the wrong side once.

### `fuel_spent` changes meaning for everybody

ADR 0019 made `fuel_spent` backend-specific — an instruction is not an AST
node and there is no honest mapping — and the default now reports the VM's
figure. Any number a user recorded from `--stats` before this change and
compares to one after it is comparing two different units. What holds on
both, and is what fuel exists for, is that a run exceeding its budget stops
deterministically at a point the program can be told about.

### The e2e suite changed what it covers, and was given it back

`crates/cove-cli/tests/e2e.rs` runs every case through the real `cove`
binary with real hosts and pins the rendered output byte for byte. Those
cases name no backend, so the flip silently moved all of them to the VM and
would have retired the interpreter's coverage of the real binary.

Every such case now runs a second time with `--backend ast`, and the two
runs must agree on stdout, stderr, and exit status. That is not the
differential harness again: the differential compares in process, against
fakes, on the value and the console and the trace, while this compares what
a person at a terminal sees. All 106 swept cases agree. The suite went from
5.8 s to 10.6 s.

The same was done where the same thing would have happened: `cove test`'s
own unit tests run each fixture suite on both backends and assert the two
report it identically; `cove build`'s end-to-end suite builds `hello` both
ways and compares what the two binaries print; and CI runs `cove generate
--check` and `cove test` over `examples/` on the interpreter as well as on
the VM, because those are this project's own sources and the first real
programs to feel a divergence.

## Alternatives considered

**Move `cove run` alone.** Smallest change, and it makes the toolchain
inconsistent in the direction that matters least to a compiler author and
most to a user: `cove test` would tell you your program works and `cove run`
would run a different implementation of it. ADR 0019's mixture argument is
about measurement, but the same reasoning applies to a suite.

**Leave `cove build` on the interpreter.** Attractive, because a built
binary has no flag to escape with and the startup cost is real. Rejected
because it inverts the point of the artifact: the binary you hand to
somebody else would be the slow one, and ADR 0009's own promise that a built
binary runs "the same program `cove run` does" would quietly stop being
about the same execution. Refusing at build time is what makes the missing
flag survivable — the failure lands on the person who can act on it.

**Serialize the IR into the binary.** Would remove both the type check and
the lowering from startup. ADR 0019 decided the IR is not a format, with
reasons that have not changed: nothing outside this repository consumes it,
and a compatibility promise is a cost paid forever. 4.7 ms is not a reason
to make one. If startup ever approaches ADR 0012's 50 ms gate, that is the
ADR to write.

**Let a refused construct fall back to the interpreter.** This is the thing
ADR 0019 forbids, and making the VM the default is exactly the moment it
would be tempting, because the refusal now reaches users who did not opt in.
It stays forbidden. A backend that falls back has no conformance claim, and
the 93-case agreement above would be a statement about a mixture rather than
about the VM.

## Consequences

- The interpreter is now costing something. ADR 0019 said it "stays after
  the VM is the default, is not optimized further, and stays readable,
  because being readable is most of what makes it useful as an oracle — and
  an oracle nobody executes is a document." This is where that stops being
  free: it is executed by the differential harness on every push, by every
  e2e case a second time, by `cove test`'s own fixtures, and by CI over
  `examples/`. Keeping it executed is a running cost, and it is the price of
  being able to say the VM is right rather than merely consistent.
- Every language feature must now be lowered before it can be used at all,
  not merely before it can be used fast. ADR 0019 named this; it becomes
  concrete here, because the refusal now reaches somebody who did not choose
  a backend.
- `--backend ast` is the one flag that undoes this, and it must keep
  working. `tests/e2e/backend_ast` runs the very program
  `tests/e2e/backend_unsupported` is refused for, on the interpreter, and
  checks that it finishes: a help message pointing at a flag that did not
  work would be worse than no help at all.
- `cove replay` is now a cross-backend tool by default. #140 is worth more
  than it was.
- ADR 0012's five gates are untouched. Gate 1 is still not askable — no
  reference native program exists — and nothing here compiles anything. Gate
  2 is measured on both backends by `cove-bench`'s `startup` benchmark and
  remains met.
