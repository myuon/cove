# ADR 0023: A replay chooses its backend

- Status: Accepted
- Date: 2026-08-29
- Supersedes: [ADR 0022](0022-the-vm-is-the-default-backend.md)'s decision
  that `cove replay` does not move — "`cove replay` builds an `Interpreter`
  unconditionally and takes no `--backend`. It stays that way, and that is a
  decision rather than an oversight." Everything else in ADR 0022 stands and
  this ADR leans on it: the VM is still what runs a program, the interpreter
  is still the oracle, and `Backend::default_for_a_run` is still the one
  function that says which
- Implemented by: [PR #142](https://github.com/myuon/cove/pull/142), closing
  [issue #140](https://github.com/myuon/cove/issues/140)

## Context

ADR 0022 made the VM the default for `cove run`, `cove generate`,
`cove test`, and `cove build`, and left `cove replay` on the interpreter. Its
reason was procedural rather than substantive: the questions a VM replay
raises — what happens when the tape runs out, and what ADR 0019's
no-silent-fallback rule means for a command that calls no host — are
decisions, and "answering it inside a change that moves four other commands
would be answering it quietly."

That left the toolchain in a state ADR 0022 named and did not like. An
ordinary `cove run --trace` records on the VM. An ordinary `cove replay`
reads that recording on the interpreter. **The ordinary case became the
cross-backend case**, and it became so by default, for a user who named no
backend at either end.

A replay's whole value is that a divergence means the program changed. A
cross-backend replay weakens that claim, because a divergence could be about
the two backends instead. Nothing is known to diverge — `tests/differential.rs`
compares every host call's task, module, operation, capability, grant,
arguments and outcome over the 93 corpus cases that lower, and that is the
same tape a replay reads — but "nothing is known to diverge" is a weaker
sentence than "these two ran the same way", and the second one is what a
replay wants to be able to say.

The other direction did not exist at all. A recording made on the interpreter
could not be replayed on the VM, so the question "does the VM ask for the
same host calls this recording holds, in this order, with these arguments,
when it is driven by a file rather than by a host?" had no way to be asked.

## Decision

`cove replay` takes `--backend <ast|vm>`, spelled and defaulted exactly as
every other command that runs a program spells and defaults it.

### The default is the VM, and it is an inference rather than a reading

`Backend::default_for_a_run` answers for `cove replay` as it answers for the
other four, so "the default backend" stays one decision made in one place.
A replay of a recording made by an ordinary `cove run` therefore runs on the
VM, which is the backend that made it.

That is an inference about how the file was probably produced, and it is
worth being exact about why it can be no better than that. **A trace does not
record which backend recorded it.** `TraceHeader` carries the value capture,
the entry, and the entry's arguments — the three things a replay needs to
start the same entry the same way — and nothing about a backend. So
`cove replay` cannot check its flag against the file, and neither can the
person running it. What a user can know is what they typed, at both ends;
what nobody can know from the file alone is whether a replay crossed
backends.

Two consequences follow, and both are implemented rather than merely noted.
The command's summary names the backend the replay ran on and says the file
does not name the other. A divergence report ends with the same sentence,
because that is where it matters most: a divergence is the program saying it
would behave differently only if the two runs were the same run in every
other way, and whether they were is not something the file can settle.

Recording the backend in the header was considered and rejected below.

### What happens when the tape runs out was already decided, and not by an evaluator

This is the question ADR 0022 deferred, and the answer is that it was never
open. `Divergence` is a property of `crates/cove-cli/src/replay.rs` rather
than of a backend. `Divergence::Unexpected` is "the program made a call after
the trace ran out of them", and `Tape::answer` produces it from two things:
what the trace holds, and what it was asked for. No evaluator appears in
that judgement. Both backends reach the tape through the one `HostApi`
boundary they share, so a VM replay that runs off the end of the tape gets
the answer an interpreter replay already got, and the same is true of a call
that does not match, one the trace has no result for, a trace left unused,
and a run that ended differently.

Moving `cove replay` to the VM was therefore plumbing and a default, not a
new semantics. The deferred question turned out to have been answered by the
design of the tape before it was asked.

### ADR 0019's rule applies, in the only form it can

ADR 0019's rule is that a run either finishes on the VM or fails before any
side effect. A replay makes no host call at all — that is its point — so it
has no side effect for a refusal to come before, and quoting the rule would
be quoting a clause with nothing to bind to.

What a replay has instead is a verdict, and the verdict is its whole output.
A replay that quietly finished on the interpreter would report "replayed", or
a divergence, about a backend nobody asked for. That is the mixture the rule
exists to prevent, arrived at by a different road: a divergence attributed to
the wrong backend is worse than no divergence, because somebody will act on
it.

So `--backend vm` lowers the entry before the tape is built and before a host
is registered, in the same place and in the same order `execute_entry`
lowers, and a construct the lowering does not cover refuses the command by
name. The refusal is the one `cove_ir::Unsupported::to_diagnostic` already
writes, unchanged, and its help — "run it on the interpreter with
`--backend ast`" — becomes true of this command for the first time. Before
this ADR that sentence would have pointed at a flag `cove replay` did not
have.

### The flag may disagree with the recording, and the summary cannot say so

Nothing prevents replaying an interpreter recording on the VM, and that is
the direction issue #140 called the interesting one. It asks a sharper
question than running a program twice does: driven by a file, does this
backend ask for the same host calls, in the same order, with the same
arguments?

The summary does not say the two "differ", because it cannot know that they
do. It says which backend replayed and that the file does not say which
backend recorded. Naming a difference the command cannot see would be worse
than naming none.

## What this gives up

### A recording that only one backend can replay

`tests/e2e/backend_unsupported` is a program the lowering refuses, so it can
be recorded only on the interpreter and replayed only on the interpreter. The
default is the VM, so replaying it takes a flag the recording does not
mention and the file cannot suggest. That is the same cost ADR 0022 already
accepted for `cove run`, reaching one command further.

### A per-command default that could drift from a recording's reality

If `cove run`'s default ever moved back, every `cove replay` of an existing
recording would silently cross backends again, and no file would say so. The
mitigation is the sentence in the summary rather than a mechanism, and the
sentence is only as good as the reader.

## Alternatives considered

**Record the backend in the trace header, and default a replay to it.** This
is the option that would make the default a reading rather than an inference,
and it is the one worth the most words.

It was rejected for now, on three grounds. It changes the trace format, which
is a compatibility surface: `TRACE_FORMAT_VERSION` would go to 3, every
version-2 recording in existence would either be rejected or be a trace with
an unknown backend, and this change would have to decide what a replay does
with the second kind — which is the same question in a new place. It makes a
recording's backend authoritative over the flag, and a user who wants to
replay across backends deliberately, which is what issue #140 says is the
interesting direction, would then be overriding the file. And it buys
correctness only for recordings made after it, while the sentence in the
summary is true of every recording ever made.

It remains the better long-run answer, and a later ADR raising the format
version for other reasons should carry it.

**Leave `cove replay` on the interpreter and document the crossing.** This is
ADR 0022's position, and it is where the honest reading of the situation
stops working: it makes the ordinary case the cross-backend case, and no
amount of documentation gives a user a way to make the two match.

**Default a replay to the interpreter while offering `--backend vm`.** This
keeps the ordinary replay cross-backend for no benefit, and it would make
`cove replay` the one command whose `--backend` defaults differently from the
other four — a flag with two meanings depending on where it is written.

**Refuse a replay unless `--backend` is given.** It would make the crossing
impossible to fall into. It would also break every existing invocation, and
force a choice on a user who has no way to make it correctly, since the file
does not say which backend to name.

## Consequences

- `cove replay` is no longer a cross-backend tool by default. An ordinary
  `cove run --trace` followed by an ordinary `cove replay` now runs both ends
  on the VM.
- The two directions that did not exist do now, and both are tested.
  `crates/cove-cli/tests/trace_replay.rs` covers all four combinations of a
  backend that records and a backend that replays, over `examples/restricted`,
  and they all succeed: driven by a file, the two backends ask for the same
  two host calls in the same order with the same arguments. **No cross-backend
  divergence was found.**
- A program the lowering refuses can be replayed only on the interpreter, and
  the refusal says so with a flag that now exists.
- `Backend::default_for_a_run` is now one decision that five commands make.
  The commands that parse `--backend` outside `RunFlags` — `cove generate` and
  `cove replay` — share one `split_backend`, so the value the flag accepts and
  the sentence an unknown one is refused with cannot drift between them.
- The trace format is unchanged, and still does not record which backend
  wrote a recording. Anything that wants to know must be told.
