# ADR 0085: Compiled frames resume where they yielded

- Status: Proposed
- Date: 2026-10-04
- Refers to:
  [ADR 0084](0084-a-run-may-yield-at-a-safepoint.md), whose §6 left
  yielding inside compiled code undecided and whose request, decline rule,
  `Step::Yielded` and "stands before the safepoint" this extends to the
  native tier; [ADR 0080](0080-a-host-call-may-answer-pending.md), whose
  `OwnedVm` had no native constructor and whose quiescence rule this keeps;
  [ADR 0082](0082-a-parked-run-keeps-its-deadline.md), whose resume-asks-first
  rule a resumed compiled run keeps; [ADR 0055](0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md),
  whose "the VM stack is the first root map" and `cove_native::abi`'s
  "Values stay in the frame" are what this is built on;
  [ADR 0057](0057-a-native-call-returns-into-the-destination-its-caller-named.md)
  and [ADR 0079](0079-a-direct-call-opens-its-frame-in-emitted-code.md), the
  calling convention and the frame records compiled code keeps;
  [ADR 0060](0060-a-backedge-tests-the-stride-before-it-calls.md) and
  [ADR 0078](0078-a-native-call-tests-the-stride-before-it-takes-a-safepoint.md),
  the native polls that are the yield points; PR #589's comparison against
  Go, which found the need
- Supersedes: nothing. ADR 0084 §6 says compiled frames *block* a yield and
  that how they might not is "not decided here"; this decides it. Nothing
  accepted is contradicted
- Decides: that a run yields inside compiled code by leaving every compiled
  frame standing as the VM frame it already is and unwinding the native
  stack, and resumes by re-entering each frame's own code at a resume point;
  which safepoints offer the yield and which decline; that `OwnedVm` gets
  its native tier from a `PreparedProgram` compiled once; that a resumed run
  starts with its request lowered

## Context

PR #589 put `examples/edge` beside the same service in Go. On `crunch`, the
CPU-heavy tenant, the server was **4.2× Go** on the encoded VM and the native
tier was **1.3×** in-process — the main gap, and one the tier already closes.
But the server's isolates are `OwnedVm`s, which had no native constructor,
and ADR 0084 §6 made compiled frames block a yield. A native tier that
cannot be interrupted brings back the head-of-line blocking #587 measured and
#588 removed: with four workers, `hello` behind `crunch` had a p99 of 77 ms
unsliced and 15.6 ms sliced at VM safepoints.

So the question is what a run that is inside compiled code is made of at a
safepoint, and which of it a yield has to carry to another thread. The code
answers it, and the answer is short.

### What a run with compiled frames live is made of, at a safepoint

The chain is: the outermost `drive` → `encoded::dispatch` → its `CALL` arm →
`native::from_encoded` (a `Bridge` on the Rust stack) → `native::enter` (a
`NativeCtx` on the Rust stack) → compiled frames, joined by direct calls in
emitted code or by the `call` helper and another `enter` → a helper
(`safepoint`, `alloc`, `open`/`call`).

| state | where it lives | is it the thread's? |
|---|---|---|
| every Cove value of every frame, compiled or encoded | the word stack (`Memory`'s words); compiled code loads and stores the slot on every IR read and write and keeps nothing across an instruction boundary (`cove_native::abi`, "Values stay in the frame") | no |
| every frame's record — base, function, pc, destination slot | `Machine::frames`, the `FrameStack` emitted code pushes and pops (ADR 0079). A compiled frame waiting on a call holds the pc after it (`suspended_at_the_call`, and the store `open_inline` emits); the innermost is synced by the helper it is in | no |
| the heap | the run's own chunks (`Arc<Space>`), rooted by the frames' static reference maps | no |
| work since the frame's last poll | `r13` (`WORK`), handed to the helper as its argument | handed over: the helper adds it to `bulk_work` |
| work done and not yet charged to fuel | `work() − charged_work` on the machine | no |
| the context: words and chunk pointers, literal and layout tables, frame and word stacks, `poll_at`, `pending_work`, `direct_calls`, the raise fields | a `NativeCtx` on `enter`'s Rust frame, shared by direct callees | every field is derived from the machine or charged on the way out |
| the bridge: machine, budget, tier, the error leaving, the chain's floor | a `Bridge` on `from_encoded`'s Rust frame | derived |
| each compiled frame's return address and the seven registers its prologue pushed | the OS stack | **yes** |
| `rbx` (context), `rbx`-relative base and destination offsets | callee-saved registers, derived by the prologue from `Entry`'s four arguments | derivable from the frame record and the caller's |
| the frame pointer (`FRAME`) and a direct call's callee index (`r15`) | registers; `FRAME` is dead at every block start and after every call, and `r15` is dead once the call's close has run | not live at a block start or after a call |
| the Rust frames between `drive` and the helper | the OS stack | **yes**; nothing in them outlives the chain's stop path, which already unwinds them for an error |
| the machine code and the tier table | `NativeProgram`: finalized read-execute pages, immutable, `Send + Sync` | no |
| the counters (`Tiers`, refusals, chunk table) | the machine's boxed `Tiering` | no |
| thread-local storage | nothing on this path reads any; a `Shared` cell's lock word is the task's since ADR 0080 §6 | no |

**Everything a compiled frame *is* lives in VM memory already.** What is the
thread's is the OS stack — return addresses, saved registers, the Rust frames
— and all of it is either derivable from the frame records or dead at two
kinds of point in the code: the start of a block, and the instant after a
call. That is not an accident of this change; it is ADR 0055's choice that
the frame is canonical "at every instruction boundary and not merely at a
safepoint", which `cove_native::abi` records as "the thing a later slice will
want to change".

### The options, against that

**(a) Explicit continuations** — compile every frame so it returns to a
scheduler and is called back. The frames are explicit already; what this adds
over (d) is returning to a trampoline on *every* call, which gives back ADR
0079's direct call (−6.1% wall on covefmtBench native, −21.1% on cq) to pay
for a yield taken a few hundred times a second.

**(b) A native stack per run, switched to on entry and moved with it.** It
would leave compiled code untouched. But the Rust frames between `drive` and
the helper would live on that stack too, and Rust code is entitled to assume
it does not change threads under them — the standard library's thread-locals,
the allocator's per-thread caches, `std::thread::scope` in `drive` itself.
Moving such a stack is unsound in general, and the place to prevent it would
be every Rust frame in the chain. It also needs a context switch this
workspace has no code for (`cove-native` depends on `libc` alone), a guard
page and a stack's worth of memory per suspended run — 10,000 parked
`aggregate` runs at 64 KiB each is 640 MB — and the collector would gain
nothing, because the roots are the frame records either way.

**(c) Deoptimise to the encoded VM at the yield, and resume there.** It
needs almost nothing: the frames are VM frames, and the dispatch loop runs
them correctly — this was observed while building (d), when an off-by-one
floor let the loop take the outermost compiled frame and the answers stayed
right. But a frame resumed there stays on the VM until it returns, and
`crunch`'s loop never returns: a yielded native run would run at the VM's
speed from its first slice on. It also changes the run's accounting, since
the two tiers count work differently, so a yielded run would not be charged
as an uninterrupted one on the same tier. Getting back into compiled code at
a later backedge needs a test in the dispatch loop's backedge (a cost every
program pays) and entries into the middle of compiled functions — which is
(d)'s mechanism with a slower path in front of it.

**(d) Unwind the native stack, keep the frames, re-enter each one where it
stands.** The yield takes the existing stop path out — the same unwinding a
fuel stop takes — and leaves the frames standing as an error does. Resuming
re-enters each compiled frame through a second prologue that jumps to a
resume point, innermost first, and does from Rust what the caller's call
sequence would have done when the callee answered. It costs a resume
prologue and three small tables per compiled function, and nothing on any path
that does not yield.

(d) is chosen. It is the smallest change that fits the structure, because
the structure is already (d)'s precondition: the frame is canonical, the
calling convention passes indices rather than pointers (ADR 0057), and the
records compiled code pushes are the VM's.

## Decision

### 1. A compiled function has resume points, and a prologue that jumps to one

`cove_native` lays down, after each compiled function's body, a **resume
prologue**: the entry prologue exactly — the same seven pushes, the same
registers derived from the same four arguments, the work accumulator zeroed
— and then `jmp r8`. Its type is `ResumeEntry`, `Entry` with a fifth
argument: the address to resume at. A resumed frame therefore leaves by the
same epilogue an entered one does, to whoever called the prologue.

Beside it, `ResumePoints` holds three per-instruction tables of offsets into
the function's mapping:

| kind | where | used for |
|---|---|---|
| a block's start | `block_at`, before the block's static charge | the innermost frame, yielded at a backedge's safepoint, standing before the loop head the jump was going to |
| an instruction's start | before the template of an `Inst::Alloc` or an `Inst::Call`, after any block charge | the innermost frame, yielded at the safepoint that allocation or that call's poll takes before it does anything |
| after a call | the end of an `Inst::Call`'s template | every other compiled frame: it is waiting on the call above it |

At each of these points nothing but the frame, the context and the
prologue's registers is live — the code generator already re-derives its
frame pointer there — and the work accumulator is nought in an uninterrupted
run (a backedge's safepoint clears it before the jump; a call and an
allocation publish and clear it before the hand-over). So entering at one,
from the prologue, is the uninterrupted run's state exactly.

`Jit::resume_points` hands the table out; `NativeProgram` keeps one per
compiled function, and `Tiered::resume` answers it (`None` by default, so a
table that offers none — a test double, a `Session`'s — is a table whose
frames decline).

### 2. Where compiled code offers the yield

ADR 0084's request, read in the native helpers where a safepoint is already
due, and only there:

| safepoint | offers the yield | the innermost frame stands |
|---|---|---|
| a backedge's poll that found the stride reached (`safepoint`) | yes | before the loop head (`AtBlock`) |
| an `Inst::Alloc` (`alloc`), which takes a safepoint every time | yes — a loop that allocates every turn never reaches a due backedge, so this is where it polls | before the allocation (`AtInstruction`) |
| a call's poll that found the stride reached (`open`, `call`) | yes — a recursion with no loop polls nowhere else | before the call (`AtInstruction`) |
| the bulk operations, buffer windows, `RunSlice`, field and dynamic helpers | no: they stand inside an instruction, as ADR 0084 §2's bulk safepoints do | — |

The helper yields when the request is raised and the run could take a VM
yield (ADR 0084 §3: no callback, no running task, no debugger, **no
`drive_from` below** — `nested == 0`, so every frame from the chain's floor up
is compiled and was entered with no Rust caller between them that is not the
chain's), and every frame from the floor up has the resume point it needs.
Otherwise it **declines**, counted in `yields_declined`, the request stays
raised, and the run yields at the first due safepoint where it can — which
for a request raised below an encoded callee of compiled code is in compiled
code once the callee has returned.

The yield stands before the safepoint, as ADR 0084 §4's does: the work since
the last poll is on `bulk_work` (it was done), the safepoint's charge,
cancellation check and collector poll have not happened, and the helper
answers stop with a marker the chain carries out exactly as it carries a fuel
stop. No instruction count is taken back — compiled code dispatches none.
The machine records the chain's floor (the index of its outermost compiled
frame) and how the innermost stands.

### 3. Resuming re-enters the frames, innermost first

`YieldedVm::resume` runs `native::resume` before the dispatch loop:

1. At a block, the safepoint the run yielded at is taken first. At an
   instruction, it is not: the instruction runs again from its first byte
   and takes it itself.
2. The innermost frame is entered through its resume prologue at its point,
   under a fresh context built as `enter` builds one, with the poll threshold
   the machine has left of its stride.
3. When it answers — into the destination its caller named, ADR 0057 — what
   the caller's call sequence would have done is done here: its unpaid work
   charged, its record and its words taken off. The next frame down is then
   entered after its call.
4. When the frame at the floor has answered, the encoded caller below it goes
   on in the dispatch loop at the instruction after its `call`, as
   `from_encoded`'s caller would have.

A frame that raises, stops or yields again leaves the frames standing as it
would have uninterrupted, and the error is the one that run would have
answered — a `Raise` is named by `raised` at the frame that raised it, and the
encoded call's span is added only where the error has none, as the `CALL`
arm adds it. Re-entering is not a call and counts no tier transition.

### 4. The contract

On the same tier, a run that yields and is resumed — on any thread, any number
of times — answers what the uninterrupted run answers, **in the same
instruction count, for the same fuel, with the same allocation and the same
collections**. The cross-tier contract is unchanged: ADR 0040's bound, not
equal counts between the encoded VM and the native tier.

- **One compilation per program.** `PreparedProgram::with_native` compiles
  the program the preparation holds, once, into an `Arc<NativeProgram>`;
  every `OwnedVm` built from a clone shares the pages and has its own frames,
  counters and budget. Pairing code with another program is not expressible.
- **Budgets are not reset.** Fuel, the host-call count, the deadline and
  cancellation are the run's `Meter`, which the machine keeps across a yield.
  `YieldedVm::resume` asks `Meter::interrupted` first (ADR 0082 §3), and a
  yielded run's `cancel` ends it at the safepoint it stands at.
- **Nothing depends on the thread.** After the yield the OS stack holds
  nothing of the run, and nothing on the path reads thread-local state.
  `YieldedVm` stays `Send`.
- **The collector** walks the frame records, which are what they were; a
  collection that the resumed safepoint or the re-run allocation triggers
  walks the standing compiled frames as it walks encoded ones.
- **Park and yield coexist.** A host call is made only by encoded code, so a
  run parks only where it did (ADR 0080 §2); it yields in either tier.
- **Unsupported boundaries decline.** Below a host's callback, beside a
  running task, under a debugger, below an encoded callee of compiled code,
  or over a table without resume points, the request waits. A spawned task
  has no tier and no flag. A `Session` never parks.

### 5. A resumed run starts with its request lowered

ADR 0084 lowers the request when a run begins and when it leaves its thread.
A yielded run is now also lowered **when it resumes**: a request raised while
it waited in a queue is not one for the slice that is starting. Left raised
it would be honoured at the first thing a resumed run reaches — the very
safepoint it stands before — and a run resumed with its flag up would yield
again having done nothing, every time. (This was found by a benchmark that
raised the request before each resume, and it livelocked on both tiers.)

### 6. `OwnedVm` and `examples/edge`

`OwnedVm::new` installs the tier when its preparation has one; `OwnedVm`
gains `collections` and `tiers`, and `YieldedVm` gains `compiled_frames` —
how many compiled frames a yield left standing, nought for one the dispatch
loop took. `examples/edge` takes `--backend vm|native` (default `vm`: the
example's default build has no code generator, which is ADR 0055's rule for
an embedder, and its README's numbers are the VM's); `native` compiles each
tenant once at deploy and slices runs exactly as before.

## Consequences

- A native isolate can be sliced: an embedder bounds how long one holds a
  worker to its slice plus one stride, on either tier.
- Nothing that does not yield pays for this. The added work is one load of
  the request inside branches already taken once a stride, the resume
  prologue's bytes after each function (39 bytes, measured), and three table
  entries per IR instruction in the `NativeProgram`.
- **The frame stays canonical, now by contract.** ADR 0055 permits register
  promotion between safepoints, and `cove_native::abi` expects a later slice
  to want it. That slice now also has to keep both resume points honest: a
  value held in a register across a block start or a call would have to be
  spilled there, or the point dropped from the table — in which case the
  frame declines rather than resumes wrongly.
- A frame resumed on a different thread arrives with a cold cache, as any
  yielded run does; the measured cost of a yield is below.

## Measured

On one x86-64 macOS machine (i7-10700K, 16 hardware threads) shared with
other agents' builds: **load average 3.6 to 10** while these ran, recorded
per row in the raw files under `examples/edge/compare/results/native/`.

**Hot path, nothing asking to yield.** Base `fcf64a9` against this change,
both `--profile checked`, three interleaved rounds (`hotpath.sh`); medians,
because the load moved single rounds by up to 50% on both sides
(`hotpath.txt` has every round):

| row | base | head | | counts |
|---|---:|---:|---:|---|
| covefmtBench, native, `execute` | 4,275 ms (3,276–5,119) | 3,288 ms (3,270–3,308) | best round −0.2% | 214,250 encoded, identical |
| covefmtBench, VM, `execute` | 12,765 ms | 12,380 ms | −3.0% | 2,231,988,228, identical |
| `crunch n=150000`, native `Vm` | 20.49 ms | 20.56 ms | +0.4% | |
| `crunch n=20000`, native `Vm` | 1,336.7 µs | 1,338.6 µs | +0.1% | |
| `crunch n=2000`, native `Vm` | 67.6 µs | 67.7 µs | +0.0% | |
| `hello`, native `Vm` | 4.10 µs | 4.13 µs | +0.7% | |
| `crunch n=20000`, edge path, VM | 4,396 µs | 4,370 µs | −0.6% | |

Nothing moved beyond the noise, which is what the change predicts: the added
code is a load inside branches taken once a stride and a prologue no
unyielded run enters. covefmt's machine code grew by 4,992 bytes over 128
compiled functions (1,314,151 → 1,319,143, +0.38%): 39 bytes a function, the
resume prologue.

**A yield and a resume** cost about **0.6 µs** on the native tier, on one
thread: `crunch n=150000` on the edge path with a monitor raising the request
every 20 µs and each yield resumed at once, against the same path
unrequested, paired by round — 60, 76 and 88 µs more over 126.5–131.8 yields
a call (0.47–0.69 µs each), with two compiled frames re-entered each time.

**`examples/edge` on `--backend native`**
(`examples/edge/compare/README.md`, "The native backend", measured
2026-10-05 at load average 2.2–6.7): `crunch` is **1.34× Go** at the server
(2,783 against 3,720 req/s at 16 in flight; 4.1× on the VM), 1.33–1.36×
in-process from `n` = 20,000 up, and the `cpu-io` mix 1.05–1.31× Go. Under the
mix, unsliced native `hello` has a p99 of **46.7 ms** at 990 req/s, and
**13.5 ms** with the 2 ms slice — the level #588 measured on the VM (15.6 ms,
here 14.9) at three times its rate; at #588's own 330 req/s the native pool
is idle enough that `hello` is 3 ms either way.

**Correctness.** `crates/cove-runtime/tests/native_yielding.rs`: twenty
yields inside a compiled loop, each resumed on a new thread, with the
uninterrupted native run's answer, instruction count, fuel, allocation and
collections; a yield with two compiled frames standing; a monitor slicing a
300,000-turn compiled loop; a loop allocating every turn, yielding at its
allocations across 122 collections with an array kept live; a loopless
recursion yielding at its calls; a request below an encoded callee declined
and honoured after it; fuel, cancellation and a deadline after yields; drop
of a yielded run; park and yield alternating six times each; eight isolates
over one compiled program at once, half under a fuel limit, each with its
own monitor. `examples/edge`'s `tests/server.rs` slices a native `crunch` on
one worker while `hello` waits.

## What this does not decide

- **Yielding below an encoded callee of compiled code**, and so **parking**
  at a host call made by an encoded function that compiled code called. Both
  need the `drive_from` segment — a dispatch loop with a floor, whose answer
  `call` writes into the compiled caller's destination — to be resumable as a
  segment. The frames are VM frames there too; what is missing is resuming a
  floor, not a new representation.
- **Compiled code in a spawned task.**
- **Register promotion**, which would have to bring resume-point spills with
  it (Consequences).
- A scheduler, a slice, or a monitor, as in ADR 0084.

## Alternatives considered

The three in Context — explicit continuations, a stack per run, and
deoptimisation to the encoded VM — and:

- **Yield only at a backedge.** Smaller by two kinds of point. But a loop that
  allocates every turn takes its safepoint at the allocation and never finds
  the backedge due, and a recursion polls only at its calls; both would hold
  their worker as an uninterrupted native run does.
- **Yield only when the chain is one frame deep.** It needs no resume after a
  call. But `crunch`'s hot loop is a call (`primesUpTo` → `isOddPrime`), as
  is almost every real loop, so the yields it allowed would land rarely.
- **A resume table in machine code** (a compare chain over the points). The
  runtime knows the point it wants; a jump to its address from a prologue
  shared by every point is smaller and has nothing to search.
- **Take the safepoint at the yield and resume after it.** Simpler to state,
  but the resumed run would be charged and collected a stride away from where
  the uninterrupted one is, as ADR 0084 found for the VM.
