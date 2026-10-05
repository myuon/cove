# ADR 0087: Compiled code calls the host, and may park there

- Status: Proposed
- Date: 2026-10-05
- Refers to:
  [ADR 0085](0085-compiled-frames-resume-where-they-yielded.md), whose
  frames-stand-and-are-re-entered mechanism this reuses for a park, and whose
  §4 says "a host call is made only by encoded code", which stops being true;
  [ADR 0080](0080-a-host-call-may-answer-pending.md), whose parking, quiescence
  and `ParkedVm` this extends to a host call compiled code makes;
  [ADR 0086](0086-a-yield-request-makes-compiled-code-poll.md), the yield
  rules a compiled run keeps around its host calls;
  [issue #605](https://github.com/myuon/cove/issues/605), which found the need
- Supersedes: nothing accepted. ADR 0085 is Proposed; its §4 sentence is a
  statement of fact this changes, and it points here
- Decides: that `Inst::CallHost` is in the compiled subset, made through a
  runtime helper that is `encoded.rs`'s `CALL_HOST` arm whole; and that where
  the run may park the helper parks it with the compiled chain standing,
  resumed after the call

## Context

ADR 0085 lets a run yield inside compiled code only where no encoded
function stands between the compiled frames and the run's own dispatch loop.
cove-tools' algorithm playground (issue #605) found how easily ordinary code
breaks that: its first version timed each algorithm by reading the clock
around the call, in one function. `CallHost` was outside the subset, so that
function stayed encoded, and everything compiled below it — the whole
algorithm — could not be sliced: `hello`'s p99 was 10.2 s beside four heavy
runs, with 72 million declined yields. The workaround was to move the clock
read into a leaf function of its own. The same issue found `String` `!=` and
`<` and `sorted(by:)` doing the same; those are lowered by #605's other
changes, which need no decision.

A host call is the one of the three that touches the scheduling contract,
because a host call is where a run parks (ADR 0080).

## Decision

### 1. A host call is a helper call

`Inst::CallHost` is admitted to the subset with its arguments and its
answer's run bounded, as a call's are. The template hands it to a new helper,
`NativeHelpers::host` — `CallFn`'s shape with the `HostOpId` for the callee —
which is `encoded.rs`'s `CALL_HOST` arm: the frame synced,
`Machine::call_host` (the arguments materialised; the registry's grant,
budget charge, schema and trace; the wait), the answer's words written at the
destination, and a failure raised at the instruction's span. A host may run a
Cove callback, which needs a thread scope for any task it spawns, so the
helper opens one, as `drive_from` does.

So a host call costs compiled code what it costs the VM, plus one helper hop
and an empty thread scope — against a host call, which materialises values
and goes through the registry, neither is measurable.

### 2. A run parks inside compiled code as it yields there

`Machine::call_host` offers the host the parkable call when the run is
parkable and quiescent (ADR 0080 §2). From compiled code the chain's frames
are VM frames with nothing on the Rust stack that matters — the condition
ADR 0085 established for a yield — so the run may park there too, **provided
every frame from the chain's floor up can be re-entered after its call**; if
not, the helper turns parking off for this call and the host is called the
blocking way, as wherever a run cannot park.

A pending answer suspends the run exactly as the dispatch loop's park does;
the helper records the chain's floor (`Machine::parked_native`) and carries
the park's marker out as a yield's is carried. `Machine::resume` writes the
answer at the destination the instruction names, as before, and then
re-enters the chain innermost first — the innermost *after its host call*
(`Stands::AfterCall`, at the same resume point a call's frame resumes at) —
before the dispatch loop goes on. A failed answer leaves the frames standing,
as an error from the blocking call would.

`Inst::CallResource` (a call addressed to a host handle) is not lowered here;
it is the same shape and can follow when a program asks for it.

## Consequences

- A clock read, a log line or any host call no longer keeps a function — and
  everything compiled below it — off the native tier and unpreemptible.
  cove-tools' `algo.timed` workaround is unnecessary.
- A run parks in either tier, and `ParkedVm` is unchanged: the park is resumed
  the same way, and the answer is converted and written by the same code.
- ADR 0085 §4's "a host call is made only by encoded code, so a run parks only
  where it did" no longer holds; what still holds is that a run parks only
  where it is quiescent.

## Measured

See the pull request: `crates/cove-runtime/tests/native_yielding.rs`'s
`waits` parks six times in compiled code, each resumed on another thread, and
answers in the instruction count and fuel of the run that called the host the
blocking way; `timed` — a host call around a long call — yields once when
asked, with nothing declined.

## Alternatives considered

- **Lower `CallHost` blocking only.** Smaller, and enough for the clock read.
  But a compiled function that waits on I/O would then hold its worker for the
  wait, where today the same function, encoded, parks: lowering would be a
  regression for exactly the tenants parking exists for.
- **Make the encoded segment below compiled code resumable instead** (ADR
  0085's named limit). It would also cover the instructions still outside the
  subset, but needs a dispatch loop with a floor to be suspended and resumed,
  and its answer carried into the compiled caller's destination. Lowering the
  three instructions ordinary code reaches was measured to remove the cases
  #605 reports; the limit stays named for what is left.
