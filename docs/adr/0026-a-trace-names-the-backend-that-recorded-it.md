# ADR 0026: A trace names the backend that recorded it

- Status: Accepted
- Date: 2026-08-30
- Supersedes: [ADR 0023](0023-a-replay-chooses-its-backend.md)'s decision that
  a replay's backend is an inference rather than a reading — "**A trace does
  not record which backend recorded it.** ... So `cove replay` cannot check
  its flag against the file, and neither can the person running it" — and the
  alternative it rejected under "Record the backend in the trace header, and
  default a replay to it". Everything else in ADR 0023 stands and this ADR
  leans on it: `cove replay` still takes `--backend`, the flag still wins, a
  cross-backend replay is still supported on purpose, and `Divergence` is
  still a property of the tape rather than of an evaluator
- Implemented by: this change, closing
  [issue #159](https://github.com/myuon/cove/issues/159)

## Context

ADR 0023 gave `cove replay` a `--backend` flag and defaulted it to the VM,
and was exact about the one thing that default could not be. The header
carried the value capture, the entry, and the entry's arguments — the three
things a replay needs to start the same entry the same way — and nothing
about a backend. So the default was an inference about how the file was
probably produced: an ordinary `cove run --trace` records on the VM, so an
ordinary `cove replay` runs on the VM, and the two agree because two defaults
agree rather than because anything checked. Both the summary and every
divergence report ended by saying so, in the only sentence the command could
honestly write: *this ran on `vm`; the file does not say what recorded it.*

ADR 0023 also named the price of that. "A per-command default that could
drift from a recording's reality": if `cove run`'s default moved, every
replay of an existing recording would silently cross backends and no file
would say so. And "a recording that only one backend can replay":
`tests/e2e/backend_unsupported` is a program the lowering refuses, so it can
only be recorded on the interpreter and only replayed there — but the default
was the VM, so replaying it took a flag *the file could not suggest*. A user
who was handed that recording had to already know what it was.

Meanwhile the reasons to want the field kept accumulating, and issue #159
lists them: fuel units and stop points are backend-specific ([ADR
0024](0024-a-stop-is-a-bound-not-a-point.md)), GC root sets and heap-limit
behaviour differ, an `--backend ast` recording can be handed to a default
`cove replay` months later, the default backend has already moved once, and a
divergence report cannot separate program drift from backend drift.

ADR 0023 rejected the field "for now" on three grounds and said plainly that
it "remains the better long-run answer, and a later ADR raising the format
version for other reasons should carry it." No later ADR raised the version
for another reason. This one raises it for this reason, and answers the three
grounds directly, because two of them turn out to be smaller than they looked
and the third turns out to be answerable rather than merely deferred.

## Decision

**A trace header names the backend that recorded it, and `cove replay`
defaults to that backend rather than inferring one.**

### The field, and the version it costs

`TraceHeader` gains a `RecordingBackend`, a closed two-value set spelled with
the two names `--backend` accepts, written second in the header line:

```text
{"event":"trace_header","version":3,"backend":"vm","values":"full","entry":"restricted.main","args":[]}
```

`backend` rather than `recording_backend`, which is how issue #159 sketched
it. Every field in a `trace_header` is about the recording — the entry is not
a `recording_entry` and the arguments are not `recording_args` — so the
qualifier would be a stutter that the line's own name already carries.

A closed set rather than free text. The field decides which backend a replay
runs on, so a third name is not provenance a reader can carry along and
ignore: it is a file this build cannot replay by its own rules, and it is
refused as one.

### One build reads one version, and version 2 is refused

This is the compatibility question ADR 0023 identified and did not have to
answer — "every version-2 recording in existence would either be rejected or
be a trace with an unknown backend, and this change would have to decide what
a replay does with the second kind — which is the same question in a new
place."

**The first kind. A version 2 trace is refused for its version**, by the
sentence a version 1 trace has always been refused by, and the sentence names
both versions:

```text
`t.jsonl`:1: is version 2, and this build of `cove` reads version 3
```

Issue #159 proposed the second kind: read old traces, call the backend
unknown, and keep a documented compatibility default. That is rejected, and
the reason is not that it is hard.

It would be a **second compatibility policy** for a format that already has
one. This reader has read exactly one version since it existed, and the
version field's whole purpose — stated where it is defined — is that "a
reader that does not know the version can reject the trace rather than misread
it." Reading version 2 as version 3 with a hole in it converts the version
from a gate into a negotiation, and the next field to be added would have to
decide the same thing again, with two precedents pointing opposite ways.

And it would buy a **replay outcome nothing can produce**. "Unknown-origin
execution" would be a third classification, a third sentence in the summary
and a third in every divergence report, reachable only from a file this build
cannot write. `a construct no corpus case writes is a construct nothing
compares` is the rule this repository runs its two backends by, and a report
branch no run can reach is the same defect in prose: it could only ever be
tested by hand-assembling a header, and it would be exercised for the first
time by whoever it eventually misled.

What is given up is real and is small. A trace is the artifact of a run, and
the run that made it is a command away; re-recording is `cove run --trace`
again. Nothing in this repository stores traces, and the format is one version
old.

### A replay's default is now a reading

`cove replay` with no `--backend` runs on the backend the file names.
`Backend::default_for_a_run` is not consulted by this command at all any
more — there is nothing left for it to infer — and it goes on being the one
decision the four commands that run a program with no file to read from make.

This is the supersession, and it is worth being exact about what it does and
does not change. ADR 0023 wanted the replay to run on "the backend that made
it" and could only approximate that with a constant. The intent is unchanged;
what changed is that the file can now answer, so the approximation is retired
rather than the goal. **An ordinary replay is a same-backend replay because
the file says which backend that is**, not because two defaults happen to
coincide — and if `cove run`'s default ever moves again, every existing
recording keeps replaying on the backend that recorded it. That is the drift
ADR 0023 listed under what it gave up, closed.

### The flag still wins, and a crossing is now visible

Nothing here makes the file authoritative. This was ADR 0023's second ground
for rejecting the field, and it is the one worth keeping: issue #140 calls
cross-backend replay the interesting direction — driven by a file rather than
by a host, does this backend ask for the same calls, in the same order, with
the same arguments? — and a file that could forbid it would take that question
away. `--backend ast|vm` overrides the header, in both directions, with no
warning and no confirmation.

What the header buys instead is that the command can finally *say which
situation this is*. The summary reports one of two things:

```text
  backend     vm, which is the backend that recorded this trace
```

```text
  backend     ast, and this trace was recorded on vm; this is a
              cross-backend replay, so a difference could be the two backends'
```

and a divergence report ends with the matching note. The two notes say
opposite things, and that is the point. ADR 0023 had to append its caveat to
*every* divergence report, because no file could say whether the crossing had
happened; the caveat is now written only for the replays it is true of, and a
same-backend replay gets the strong sentence instead — **a divergence is the
program's**. That sentence is the one ADR 0023 said a replay wants to be able
to say and could not: "'nothing is known to diverge' is a weaker sentence than
'these two ran the same way', and the second one is what a replay wants to be
able to say."

### A recording that only one backend can replay now replays

`tests/e2e/backend_unsupported` is the program the lowering refuses. It can
only be recorded on the interpreter, and under ADR 0023 replaying it took
`--backend ast` — a flag the file could not suggest, for a reason the file did
not record. Now the bare command reads `ast` out of the header and replays it
there.

ADR 0019's no-silent-fallback rule is untouched. `--backend vm` still lowers
before the tape is built and before a host is registered, and still refuses a
construct the lowering does not cover, by name, pointing at `--backend ast`.
What moved is which command line gets the refusal: it belongs to the person
who asked for the VM by name, rather than to the person who asked for nothing.

### `cove trace` names it too

A trace can be asked about its provenance without being replayed:

```text
  backend    ast — the backend that ran the program this trace recorded
```

The field is read by three things — the summary, the replay's default, and the
replay's report — which is the bar this ADR set for itself. Recording a field
nothing reads would not have been worth a format version.

## What this gives up

### Every existing recording

Stated above and repeated here because it is the whole cost: a version 2
trace does not replay on this build. There is no migration and no reader that
accepts both, on purpose.

### One more thing two crates have to agree about

`cove_runtime::RecordingBackend` and the CLI's `Backend` are two enums over
the same two values, joined by `Backend::recording` and
`Backend::of_recording`. The crates draw the line where they have always
drawn it — the runtime records without knowing what a command-line flag is —
so they share the two spellings rather than the type. A unit test walks both
directions of the conversion, because a swap between two same-shaped enums is
exactly the bug a type system does not catch here.

### A field the differential harness has to exclude

`tests/differential.rs` compares the trace two backends wrote of one program,
and this is the first field in the format that differs between them *by
definition*. It is dropped from the comparison, and the exclusion is argued
in `Trace::of` beside the others. This is the exception that proves that
file's rule rather than a hole in it: nothing there is dropped for having
differed, and this is dropped for being, by its own definition, the answer to
"which backend is this", asked of a harness whose entire job is to run one
program on both. Every other header field is still compared exactly.

## Alternatives considered

**Keep ADR 0023's arrangement and document the crossing harder.** This is
where ADR 0023's own reasoning stops working, for the reason ADR 0023 gave
about ADR 0022's position: the sentence in the summary "is only as good as the
reader", and it was printed identically for a replay that crossed backends and
one that did not. A caveat attached to every case tells you nothing about any
case.

**Read version 2 traces with an unknown backend.** Issue #159's proposal,
answered above: a second compatibility policy, bought with a third report
classification that nothing this build writes can produce.

**Make the recorded backend authoritative and refuse a mismatched flag.** It
would make an accidental crossing impossible. It would also delete issue
#140's interesting direction, which is the reason `cove replay` has a
`--backend` at all — and this ADR is not licensed to take back a decision it
is not superseding.

**Record more provenance than the backend — the `cove` version, the platform,
the lowering's shape.** Each has a case, and none of them has this one's:
the backend is the only piece of provenance that changes *what a replay does*,
because it is the only one a replay can be asked to reproduce. A version
string would be a field nothing reads, which is the thing this ADR set out
not to add.

**Put the backend in an event rather than the header.** A replay chooses its
backend before it reads an event, so a backend named in an event would arrive
after the decision it exists to inform.

## Consequences

- `TRACE_FORMAT_VERSION` is 3. A version 2 recording is refused for its
  version by both `cove trace` and `cove replay`, and the refusal is tested
  in both commands alongside the future-version one it has always had.
- `cove replay` with no flag replays on the backend that recorded, which the
  four-way record/replay matrix in `crates/cove-cli/tests/trace_replay.rs` now
  covers as six cells: the two same-backend defaults, the two crossings that
  need a flag, and the two same-backend replays spelled out. All six succeed,
  and no cross-backend divergence was found — the same finding ADR 0023
  reported, now made by a test that knows which case it is in.
- A same-backend divergence report says the divergence is the program's; a
  cross-backend one says it might not be. Both sentences are pinned by tests,
  because a distinction with only one side tested is not a distinction.
- `Backend::default_for_a_run` is now four commands' decision rather than
  five. `cove replay` was the only one of the five with a file to read, and it
  reads it.
- `crates/cove-runtime/src/embed.rs` and `crates/cove-bench` record the
  backend they were built for and the backend they are measuring, so a trace
  from a built binary and a trace from the benchmark harness are as
  self-describing as a trace from `cove run`.
