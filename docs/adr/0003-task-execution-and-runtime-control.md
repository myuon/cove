# ADR 0003: Task execution and runtime control

- Status: Accepted, superseded in part by
  [ADR 0008](0008-concurrent-task-execution.md)
- Date: 2026-08-25
- Superseded by: [ADR 0008](0008-concurrent-task-execution.md), which took over
  phase 2 of the Decision below and, with it, the whole of "What Phase 1 does
  not validate"
- Amended by: this ADR's own "Amendment (2026-08-25): a concurrency limit"
  below, which adds the one control [ADR 0001](0001-mvp-language-design.md)
  asked for and phase 1 did not build; its "Amendment (2026-08-25): a
  blocking Host call must cooperate", which closes the hole a host call leaves
  in the chain of safepoints those controls are checked at; its
  "Amendment (2026-08-25): a run ends with an event, and a call names its
  task", which makes what these controls do to a run visible in the trace;
  and its "Amendment (2026-08-25): the runtime sizes the stack it recurses
  on", which gives the call-depth control the one thing it measures against
  and whose closing section was answered by the parser's own nesting limit
- Implemented by: PR #6 (phase 1); PR #23 replaced phase 1's execution model;
  the concurrency limit by PR #45; the blocking-Host-call contract by the
  change that closed issue #57; the terminal event and the task on a Host call
  by the change that closed issue #61; the stack the depth limit is measured
  against by the change that closed issue #67, and the parser-side limit its
  last section asked for by the change that closed issue #70
- Implementation status: complete — phase 1 shipped whole, phase 2 became an
  ADR of its own rather than a later commit against this one, the concurrency
  limit the first amendment adds is imposed, the blocking operations the
  toolchain's own hosts perform keep the second amendment's contract, every
  run the toolchain starts records how it ended, and every thread the runtime
  evaluates Cove on is one it sized

## Context

ADR 0001 makes the runtime as important as the compiler: grants, cancellation,
deadlines, CPU and memory budgets, and trace events that separate CPU work from
I/O wait are listed as the second deliverable of the MVP. The Language Card
adds the language-visible half of that contract:

> Concurrent work belongs to a task scope. Leaving the scope waits for or
> cancels its child tasks. Immutable task-safe values such as arrays may cross
> task boundaries. A vector cannot cross, even through `let`.

None of it exists. `async fn`, `await`, `scope`, and `spawn` parse and resolve,
and the interpreter stops on them with "not implemented yet". Three of the seven
representative programs are blocked behind that message.

The interpreter is a recursive tree walker holding `Rc` values. Nothing about it
can suspend a computation, which is what real interleaving requires.

## Considered options

**A single-threaded cooperative scheduler** would give real interleaving,
cancellation, deadlines, and I/O attribution while keeping `Rc`. It requires
rewriting the interpreter in continuation-passing style or as an explicit stack
machine, because a recursive Rust tree walker cannot pause mid-expression.

**A thread per task** would also give real interleaving, and needs *less*
interpreter surgery: the tree walker keeps working, one instance per thread.
The cost lands on the value representation instead. Rust demands `Send` for
anything crossing a thread, and `Rc` is not `Send` — but Cove already forbids
sharing mutable structure across tasks, so the only values that may legally
cross are exactly the ones that can be copied at the boundary. The language
rule and the implementation constraint are the same rule.

**Deterministic sequential execution** runs a spawned task to completion where
it is spawned. It gives no interleaving at all, so timeouts, cancellation, and
CPU-versus-wait attribution are shapes without substance.

## Decision

Do the third option first, deliberately and temporarily, and build the runtime
controls around it.

**Phase 1 — semantics and plumbing.** Implement `async fn`, `await`, `scope`,
and `spawn` with sequential execution: `spawn` defers a task, `await` runs it,
and leaving a scope runs or cancels whatever the body did not await. Implement
the runtime controls for real, not as placeholders: a fuel budget, a wall-clock
deadline, a cancellation flag, a call-depth limit, and host-call accounting,
all checked at defined safepoints — loop back edges, calls, and `await`. Emit
trace events for task spawn, completion, and cancellation, and for every host
call.

**Phase 2 — real concurrency.** Replace sequential execution with a thread per
task, and make the task boundary a typed conversion between a `Send` transfer
value and the interpreter's `Rc` values. That conversion is where the Language
Card's task-safety rule stops being prose: an `Array` of task-safe values
converts, a `Vector` does not, and a closure converts only when every capture
does. `Shared<T>` becomes the synchronized handle the card already names.

Phase 1 is chosen for sequencing, not because sequential execution is
defensible as an end state. The budgets, safepoints, trace events, and host
accounting are the parts that every later model needs, and they can be built
and tested without a scheduler. Doing them first means Phase 2 replaces one
component instead of growing three.

## What Phase 1 does not validate

Stating this plainly is part of the decision.

- There is no interleaving, so a deadline can only interrupt a task at a
  safepoint inside its own execution, never while another task runs.
- `clock.timeout` can fail a task that overruns, but cannot overlap the work it
  is timing with anything else.
- Trace events distinguish CPU work from host-call wait, but with one task
  running at a time the distinction is not yet a concurrency measurement.
- ADR 0001's success criterion that "CPU time and I/O wait are accurately
  attributable in traces" is therefore not met until Phase 2.

The MVP is a hypothesis test. Recording an unmet criterion is more useful than
producing a number that looks like it was met.

## Consequences

`examples/tasks/` runs at the end of Phase 1. `examples/server/` and
`examples/callbacks/` additionally need `http`, `database`, and `Shared<T>`,
and follow separately.

Sequential execution must not leak into the language's meaning. `spawn` returns
a handle whose value is only observable through `await`, and a scope's exit
behaviour is defined by the Language Card rather than by when the interpreter
happens to run the body. Any test that would pass only because tasks run
sequentially is a test of the implementation, not of Cove, and should be
written so Phase 2 does not have to change it.

## Superseded in part (2026-08-25): phase 2 became ADR 0008

Phase 1 shipped as written, and almost all of it is still what the runtime
does. Fuel, a wall-clock deadline, a cancellation flag, a call-depth limit,
and host-call accounting are checked at exactly the safepoints named above —
loop back edges, calls, and `await` — and a task spawn, completion, or
cancellation, and every host call, is still an event. Those parts were built
first precisely so that they would survive the change of execution model, and
they did.

What did not survive is sequential execution itself.
[ADR 0008](0008-concurrent-task-execution.md) took over phase 2 rather than
leaving it a later commit against this ADR, so `spawn` starts a thread and
`Shared<T>` is defined there rather than here. The consequence for a reader is
that the Decision's phase 1 paragraph and the whole of "What Phase 1 does not
validate" describe what was true between PR #6 and PR #23, and describe
nothing about the runtime today: tasks interleave, a deadline stops a task
while another runs, and `clock.timeout` overlaps the work it bounds. ADR
0001's success criterion about attributing CPU time and I/O wait is the one
line of that section still worth checking rather than assuming — a trace now
carries each task's own CPU and each host call's wait, but not which task made
the call, which `cove trace` says of itself under "not carried by these
events".

## Amendment (2026-08-25): a concurrency limit

[ADR 0001](0001-mvp-language-design.md)'s "Runtime resource control" lists six
things the runtime should be able to impose. Phase 1 above built four of them —
fuel, a deadline, cancellation, and host-call accounting, along with a
call-depth limit that list does not name — and
[ADR 0011](0011-garbage-collection.md)'s collector later made the fifth,
memory, a quantity something could be held to. Concurrency limits were the one
left, and phase 1 had nothing to limit: a spawned task ran at its `spawn`, so a
scope never held two at once. ADR 0008 changed that and did not add the limit,
so between PR #23 and this
amendment a program could write `while true { tasks.spawn { ... } }` and be
bounded only by fuel, which is charged at the loop back edge *after* the thread
already exists. Threads were the one resource a program could take without
asking.

**What is counted: the tasks a run holds alive at once.** Not the tasks one
scope holds, and not the tasks a run spawns over its life. "Concurrency" means
what is happening at the same time, and it is the run's total for the same
reason memory is: a task's fuel is drawn from the run's budget rather than one
of its own, so a program cannot stay under a limit by spreading the same work
over more scopes. A task counts from its `spawn` until the task that spawned it
observes its end.

**What happens at the limit: the run is stopped.** A `spawn` past the limit
raises a `RuntimeError` naming the limit and what the run was holding, exactly
as an exhausted fuel budget, a passed deadline, `max_host_calls`, and
`max_memory` do (`max_memory` itself was later retracted by
[ADR 0011](0011-garbage-collection.md)'s "Amendment (2026-08-25): the memory
budget is removed"; the analogy still holds for fuel, deadlines, and host
calls). The alternative — a `spawn` that blocks until a sibling
finishes — was rejected: waiting is a scheduling policy, and ADR 0008
deliberately has none. A limit that blocked would also turn a bound on
concurrency into a bound on nothing at all, since the program would still get
every task it asked for, only later.

**Where it is checked: before the thread exists.** This is the one control that
refuses work rather than stopping work already done, and it has to be, because
a thread that has started is a resource already held; no later safepoint could
give it back. `Interpreter::spawn` charges the budget before it takes a task
id, traces the spawn, or calls `std::thread::Builder::spawn`, so a refused task
is never given a thread and never appears in a trace.

**Where a task's place goes back: at the join.** A task ends by finishing, by
producing an `Err`, by being cancelled, or by breaking an invariant in its own
thread, and the join that `await` and scope exit both perform is the one place
all four are observed. Releasing there rather than on the task's own thread
also keeps the limit deterministic: what a `spawn` is refused for depends on
what the program has awaited, never on how quickly a sibling thread happened to
start or finish.

**Where it is configured: the three places every other limit lives.**
`Limits::max_tasks` for a host embedding Cove, `--max-tasks <n>` for `cove
run`, and `max_tasks` in a `[run.<name>]` table; `cove build` seals the
configured value into the executable it writes, as it seals the rest.

## Amendment (2026-08-25): a blocking Host call must cooperate

The Decision above names where the runtime controls are checked: "loop back
edges, calls, and `await`". Everything between two safepoints is unreachable
by them, and a Host API call is the one thing that can sit between two
safepoints for an unbounded length of time. `Budget::charge_host_call` checks
cancellation and the deadline once more before it dispatches, which was
written as a fix for exactly this and is quoted in its own comment: "a
deadline checked only in Cove code would not bound a program that spends its
time inside calls." But a check before dispatch bounds when a call *starts*.
It says nothing about how long the call then runs, and there is no second
check to reach it, because the interpreter is not running.

`http.Server.handle` made this concrete. It reached `TcpListener::accept()`
and stayed there until a client connected. A run cancelled while it waited was
not stopped; a run whose deadline passed while it waited was not stopped; and
the comment on the operation claimed the opposite — that a program serving
against a real listener "runs until the run itself is stopped" — which was
true of the intent and false of the code. Request reading had the same shape
one level down: the socket had a thirty-second read timeout, but it was armed
again before every read, so a peer sending a byte every twenty-nine seconds
held the call open indefinitely while never once timing out.

**The contract.** An operation that blocks must either cooperate with the
run's controls or say in its own documentation that it cannot.

Cooperating means four things. Bound the wait: never block on a call that has
no timeout of its own when a polled equivalent exists. Poll in steps short
enough that the granularity is a rounding error against the controls being
observed — the `clock` watchdog's two milliseconds is the precedent, and the
HTTP listener uses the same figure. Between steps, ask the `Reentry` the host
was handed whether the run has been stopped and how long it has left, and give
up when either says to. Hold no lock while waiting, since a host waiting under
its own mutex blocks every task that wants it, which is a worse failure than
the one being fixed.

A multi-part operation gets one total allowance rather than one per part. This
is the half that is easy to get wrong while looking correct: a per-read
timeout bounds each read and the operation not at all.

Giving up means answering whatever the operation's own "nothing happened" is,
not raising an error. The reason the run stopped belongs to the budget, which
holds the limit and names the configured value; a host that invented a failure
would put a second, worse account of the same event in front of the reader,
and would hand the program a Cove `Err` it could catch and carry on from.

**What the host boundary had to gain.** Cancellation was already askable, but
`Reentry::is_cancelled` read only the calling task's own flag: a host blocked
inside a `clock.timeout` body, or on the entry task, which has no flag of its
own, was told nothing was wrong. It now answers everything a safepoint would
stop on — the task's flag, every bounded call this thread is inside, and the
run's own cancellation. The deadline was not askable at all, so `Reentry`
gained `time_left() -> Option<Duration>`: `None` for a run with no deadline,
and a saturating remainder otherwise, so zero is the only value that can mean
"no time left". A duration rather than an instant, because passing it to a
socket timeout and comparing it against zero are the only two things a host
does with it.

**What the HTTP host does now.** It accepts by polling a nonblocking listener,
looking at cancellation and the remaining time between polls, holding no lock,
and answering `false` — "nothing more to serve" — when either says to stop,
which ends the loop the program wrote and lets the run stop at its next
safepoint with the budget's own diagnostic. One request gets one deadline
covering its line, its headers, and its body together, no longer than what the
run has left; a request that outlives it is answered `408` and the loop goes
round again, because a request that arrived and could not be read is still a
request that arrived. `fetch` clamps its read timeout the same way. Its
connect is the one step still unbounded, and its documentation says so, which
is the contract's other branch being used rather than avoided.

**What a host that cannot cooperate must do.** Say so, where the person
embedding Cove will read it: in the operation's own documentation. An embedder
who knows that one call is outside the deadline can put it behind a process,
a thread it is willing to abandon, or a capability it does not grant. An
embedder who does not know has a deadline that quietly means nothing, which is
worse than having no deadline at all. The rule this amendment adds is
therefore not "a host must never block" — some things genuinely cannot be
interrupted — but "a host must never block silently".

## Amendment (2026-08-25): a run ends with an event, and a call names its task

Everything above is about stopping a run: what the controls are, where they
are checked, and what a host owes them. None of it says how a reader learns
which one fired. [ADR 0001](0001-mvp-language-design.md) asks a trace to
record "host calls, capability use, and errors", and until this amendment the
trace could report the first two and not the third: a host's own failure was a
`host_call` outcome, but a program that divided by zero, broke a task-safety
rule, or ran out of fuel left no event at all — the run simply stopped
producing them. A trace that ends because the file ends cannot be told from
one that ends because the process was killed.

The same gap had a second half. `host_call` said what was called and what it
answered but not who called it, so a trace of a run with concurrent tasks
could not say whose I/O any of it was. ADR 0008 gives each task a thread and
this ADR's controls charge them all to one budget, which is exactly the
arrangement that makes the question worth asking:
[issue #61](https://github.com/myuon/cove/issues/61) asked for both halves
together, and they are one change because both are answers to "which".

**Every run ends with one `run_ended` event.** It is written where every path
into a Cove program already funnels — `Interpreter::run_entry`, which `cove
run`, `cove test`, `cove generate`, `cove replay`, a sealed `cove build`
binary, and a host embedding the runtime all call — and it is written outside
the entry rather than inside it, so a run that never found its entry to run
still ends with an event saying so. Writing it at one place is what makes
"every run has one" a property of the code rather than a claim about the
callers somebody remembered.

**The classification.** Ten names, and they divide the way the runtime already
divides these cases rather than the way a reporting tool might prefer. `success`
and `error` are the program answering: Cove's entry returns `Result<Unit,
Error>`, so a returned `Err` is a program saying what it was written to say,
and collapsing it into "the run failed" would lose the distinction the return
type exists to draw. `invariant` is Cove execution breaking one — a failed
assertion, a division by zero, a violated task-safety rule. `host_boundary` is
the Host API boundary refusing: an ungranted capability, an operation that does
not exist, or an argument or result the operation's schema does not admit,
which are the checks [ADR 0013](0013-host-resource-handles.md)'s two schema
amendments added. And `fuel`, `deadline`, `cancelled`, `call_depth`,
`host_calls`, and `concurrency` are this ADR's own controls, one name each,
because a run out of fuel and a run past its deadline are not the same report
however alike the two look from inside the budget.

Those names are a compatibility surface. A trace consumer groups runs by them,
so they are as much of the format as its keys are, and they are written down
here for the same reason the keys are written down in `trace.rs`.

**What makes it derivable.** A `RuntimeError` now carries which of the three
it is, rather than the classification being recovered by reading a message
back. `error.rs` already said a `RuntimeError` is "a broken invariant, an
ungranted capability, or a limit the host imposed"; the field is that sentence
made into data. The default is the broken invariant, which is what most of them
are and the honest answer from code that knows nothing about limits or
boundaries; the two parties that do know set it — the budget when it names the
limit it stopped for, and the Host API boundary when it is the boundary that
refused. A classification set once is kept, because the innermost party to a
failure is the one that knows what it was.

One distinction the runtime still cannot make, and the event says so rather
than pretending otherwise: a host that failed on its own terms is reported as
`invariant`. An error raised inside a host and an error raised by a Cove
callback that host was running come back out of the same call, and nothing at
the boundary can tell them apart — the callback's error is Cove's, and
relabelling it on the way out would be worse than leaving both where the
default puts them.

**The task on a Host call.** A `HostRegistry` is shared by every thread of a
run and knows nothing about who is calling. The one thing at the boundary that
does belong to one task is the way back the host was handed, because it borrows
that task's interpreter — so `Reentry` gained `task()`, beside the
`is_cancelled` and `time_left` the amendment above added, and the boundary asks
it once per call and writes the answer on the event. The entry is task 0: it is
not a spawned task, so it takes the one id the run's counter never hands out,
which is the convention `heap_collected` now shares rather than writing a null
of its own. `cove trace --task <id>` selects one task's lifecycle, its
collections, and its calls, and `--task 0` is how a reader asks what the entry
did itself.

**What is deliberately not done.** A task does not get a terminal event of its
own. It already has three — spawned, completed, cancelled — and a task's
failure does not decide the run's: it reaches whoever joined it, and either
stops the run, which `run_ended` then reports with that task's own message, or
is handled, in which case no terminal classification would have been true of
it. A fourth per-task event would duplicate the run's without being able to say
anything the run's could not.

## Amendment (2026-08-25): the runtime sizes the stack it recurses on

`MAX_CALL_DEPTH` is one of the controls above, and it is the only one whose
subject is not something the runtime owns. Fuel is counted, a deadline is
read from a clock, a host call is charged at a boundary the runtime holds;
the depth limit's subject is the native stack, and until this amendment
nothing in the runtime decided how much of that there was.
[Issue #67](https://github.com/myuon/cove/issues/67) is what that costs. The
constant is 256 and its comment promises that Cove calls nest that deep
"before the runtime reports a limit instead of exhausting the host stack".
The deepest a plain `fn nest(n: Int) -> Int` that calls itself and adds one
actually reached, by binary search on one machine:

| build   | entry, on the process main thread | a spawned task |
|---------|-----------------------------------|----------------|
| debug   | 65                                | 15             |
| release | 254, and 255 stopped at the limit | 212            |

The promise held in one cell of that table. Everywhere else an ordinary Cove
program with no capability granted at all ended the process with `fatal
runtime error: stack overflow` — a failure that stops every task of the run
at once, tells the embedder nothing, and cannot be caught, which is the one
failure a sandbox may not have. Two different stacks are behind those two
columns and the runtime chose neither: `Interpreter::spawn` built its thread
with no `stack_size`, so a task took the platform default of 2 MiB, and the
entry took whatever the process main thread happened to have, which is 8 MiB
on macOS and Linux and 1 MiB on Windows.

**What is decided: the runtime does not evaluate Cove on a stack it did not
choose the size of.** A public `STACK_SIZE` says what that size is, a task
thread is built with it, and every path the toolchain has into a Cove program
— `cove run`, `cove test`, `cove generate`, `cove replay`, a sealed `cove
build` binary, and `cove-bench` — does its whole run on a thread the runtime
created with it. `cove_runtime::on_cove_stack` is that thread: it is scoped,
so the run is built inside it and nothing Cove-shaped has to cross the
boundary, which matters because a `Value` is `Rc`-based and could not.

**The size is derived from the limits, not chosen beside them.** The issue
offered three ways out — give task threads a stack big enough for the limit,
lower the limit to what the smallest stack holds, or measure the per-frame
cost and relate the two — and this is the third, run in the direction that
keeps the limit a language-visible constant: `STACK_SIZE` is `MAX_CALL_DEPTH`
frames plus `MAX_REENTRY_DEPTH` reentry levels, times a margin. Raising the
depth limit raises the stack, in the source, where the next reader will look.
Lowering the limit instead was refused because it would cost every program
the difference — 256 frames would have become 15 in a debug build — to buy
nothing but agreement with a number nobody chose.

**The per-frame cost was measured, four shapes of it.** Calibrating on the
cheapest recursion Cove can write would produce a bound that is wrong for the
programs people write, so the deepest clean run was binary-searched with the
depth limit lifted, on task threads of two known sizes, and the figure taken
as the slope between them so that whatever the interpreter spends before the
recursion starts cancels out. Per Cove frame, which is what `MAX_CALL_DEPTH`
counts:

| recursion through          | debug   | release |
|----------------------------|---------|---------|
| a free function            | 123 KiB | 9.6 KiB |
| a method on a struct       | 135 KiB | 9.6 KiB |
| a `dyn` trait conformance  | 95 KiB  | 7.3 KiB |
| a `match` with live locals | 101 KiB | 8.2 KiB |

A reentry level of the shipped hosts, measured the same way with
`clock.timeout` nested into itself, costs 163.8 KiB in a debug build and 16.1
KiB in a release one, and the eight of them `MAX_REENTRY_DEPTH` allows are
budgeted here too. That figure also confirms the one
[ADR 0013](0013-host-resource-handles.md) calibrated that bound against: a
thirteenth level exhausted a 2 MiB task thread, which is 161 KiB a level.

**Two numbers, because a debug frame costs fourteen times a release one.**
`#[cfg(debug_assertions)]` picks between them. One number for both would be
absurd in release or useless in debug, and the profile is the only variable
in the table above that changes the answer by more than a factor of two. With
the worst measured shape and a margin of three, that is about 106 MiB in a
debug build and about 8 MiB in a release one.

**The margin is three because the table is measured shapes and not a worst
case.** The interpreter recurses once more for each level of expression
nesting, so a program that writes its recursive call inside a long chain of
nested expressions spends more per frame than anything measured, and no
constant can be the worst case for a program the compiler has not seen. The
margin stands in for that, for another platform's calling convention, and for
a host that spends stack of its own inside a callback.

**A hundred megabytes per thread is affordable because a stack is address
space.** That was checked rather than assumed, because the whole point of
this amendment is that a number with no measurement behind it is what caused
the defect. A debug run holding a hundred tasks alive at once reached a
maximum resident set of 14.9 to 15.1 MB under the new size and 15.07 MB under
the old platform default — a difference smaller than the variation between
runs — against over 10 GiB of reserved address space: a thread stack commits
a page at a time as it is touched. What the size costs is therefore address
space per live task, which a 64-bit host does not notice and `max_tasks`
already bounds, and not memory per live task, which was the trade the issue
was worried about.

**The one thread the runtime cannot size is an embedder's.** A host that
calls `Interpreter::run_entry` on a thread of its own gets whatever that
thread has, and no counter inside the interpreter can read it. That is said
where an embedder reads — on `run_entry`, with the one line that fixes it —
and `tests/embedding.rs`, which is the acceptance test for a host outside the
crate supplying its own limits, now supplies its own stack in the same way
and demonstrates that a run stopped by the depth limit reports it. It is the
same admission the Host API's blocking-call contract makes above: an
operation that genuinely cannot cooperate must say so, and a stack the
runtime did not create is one of those.

**What is deliberately not added: a stack-headroom check.** Recording the
address of a local when a run starts and comparing it against one in `invoke`
would bound the *bytes* rather than the frames, and would hold whatever a
frame costs. It was considered and left out for three reasons. The only place
the interpreter could cheaply check it is `invoke`, which is where
`MAX_CALL_DEPTH` already stops recursion, so it would add coverage for one
case: a call whose per-frame cost is inflated by deep expression nesting.
That case is reached through `eval_expr`, and checking there would put an
address comparison on the interpreter's hottest path to catch what the margin
already covers. And the budget it compared against would have to be a
fraction of `STACK_SIZE`, which is a fact about a thread the runtime sized
and a fiction about one it did not — so on an embedder's small thread, the
one place the check would earn its keep, it would be silently inert. A
mechanism that cannot be tested without a program the parser cannot parse is
not a mechanism this runtime should carry.

**A boundary this does not close.** `cove_syntax`'s parser recurses over
expression nesting with no limit of its own, so a source file with a few
thousand nested parentheses still ends the process — in the parser, before
any run begins. Sizing the thread every `cove` command runs on moved that
from a few hundred levels to a few thousand, which is a side effect and not a
fix: the fix is a nesting limit in the parser, and it is a separate defect
from this one.

That defect is now closed.
[Issue #70](https://github.com/myuon/cove/issues/70) took it, and the fix is
the nesting limit the paragraph above asks for: `MAX_NESTING_DEPTH` in
`cove_syntax::parser`, reported as an ordinary parse diagnostic with a span, a
rule, and a help, so a file that nests too deeply is refused the way any other
malformed file is. The two limits are calibrated in opposite directions, and
the difference is worth stating, because it is the same question answered from
two positions. The runtime owns the thread it evaluates on, so it fixes the
limit and sizes the stack to fit it. The parser does not: `parse` is a library
entry point and whoever calls it chooses the thread, so it fixes the stack it
is willing to promise — the platform default of 2 MiB, which is what an unsized
thread has — and derives the limit from what a level of nesting costs on it.
Sixty-four levels, measured, against 256 frames here.

The parser's limit also bounds what runs after it. The resolver, the type
checker, and the formatter each recurse over the tree the parser built, and
measurement says they spend less per level of it than the parser did building
it, so none of them needs a limit of its own. The one shape that does not
follow from this is a left-associative chain — `a.b.c`, `1 + 2 + 3` — which
the parser builds with a loop while everything downstream still walks it by
recursing; the parser therefore counts a chain link as a level too, which is
why the number bounds the tree and not only the recursion that produced it.
