# ADR 0095: A native return is a poll

- Status: Accepted
- Date: 2026-10-13
- Supersedes:
  [ADR 0078](0078-a-native-call-tests-the-stride-before-it-takes-a-safepoint.md)'s
  **"The return path. `close` charges the callee's last unpaid block and is
  not a safepoint … It was not a poll before and is not one now"**, and its
  header's "return path, which was and remains free of safepoints" as far as
  that says a return does not poll. Nothing else in ADR 0078 moves: a call is
  still a poll, allocation still takes a safepoint every time, and `close`
  itself still takes none
- Preserves:
  [ADR 0040](0040-a-bound-outlives-its-backend.md)'s table of bounds, which
  this makes true of a compiled recursion's unwinding as well as its descent;
  [ADR 0057](0057-a-native-call-returns-into-the-destination-its-caller-named.md)'s
  "published only on the return path, where no safepoint intervenes", because
  the poll is taken after the answer is written and the callee's frame is off,
  not between the two
- Refers to: [issue #624](https://github.com/myuon/cove/issues/624), which found
  the gap and where the maintainer chose this over documenting it;
  [ADR 0085](0085-compiled-frames-resume-where-they-yielded.md), whose
  after-the-call resume point this poll stands at;
  [ADR 0091](0091-a-run-is-stopped-by-its-host-not-a-fuel-allowance.md), whose
  stop tests (`vm::stops`) found it
- Decides: that every compiled call's return polls the stride in its caller,
  once the callee's frame is off; where that poll is emitted, what it stands
  at if it yields, and the cost accepted for it

## Context

ADR 0060 made a compiled backedge a poll and ADR 0078 made a compiled call
one. ADR 0078 said in as many words that a *return* was not, and that "the
*unwinding* of a deep loop-free recursion still polls nowhere until its caller
next calls or loops". It called that gap older than itself, and left it.

ADR 0091 replaced fuel with stops a host raises, and the tests written for it
(`crates/cove-runtime/src/vm/stops.rs`) raise a cancellation, a passed deadline
or a yield request at a dozen points across each run and hold the run to
ADR 0040's `S + T` from each. On the native tier the loop-free recursion
`counts(30000)` failed that bound everywhere in its ascent: a stop raised while
it unwound was noticed some **23,000** units of work later, at the outermost
frame. The tests were therefore held to the first 45% of that run — the
descent — and issue #624 asked whether a return should poll, or whether the
bound should say that unwinding is not covered.

The dispatch loop has no such gap: a `RETURN` is an instruction like any other
and the loop tests the stride before every one.

## Decision

### Where the poll is

**In the caller, after the call, once the callee's frame is off.** Not at the
callee's `Return`:

- the direct call's inline close ([ADR 0079]'s `close_inline`) already
  lowers the threshold by the callee's last unpaid work and branched only to
  saturate it at nought. That branch now tells the two cases apart — above
  nought, nothing is due, and the call is done; at or below, the threshold is
  stored as nought and the poll's helper is called. The frame's two pops move
  ahead of it, so the poll stands in the caller. **On the common path this adds
  no instruction**: `jae` becomes `ja`, and the jump that used to go to the
  outcome test goes past it instead, which the outcome test did not need (the
  inline close is reached only on `Returned`);
- every other way a call comes back — through `close`, the mediated `call`,
  a closure call, or a callee the runtime finished itself — reaches the same
  poll as a `cmp` of the accumulator, which is nought there, against the
  threshold, and a branch: [ADR 0060]'s two instructions.

A host call is not a return from Cove code and does not take it; its park
already resumes after it ([ADR 0087]).

### What it calls

`NativeHelpers::returned`, of `SafepointFn`'s shape, with the instruction after
the call as its `pc` and no work (the close charged it). It is the backedge's
helper but for where it stands: it syncs the caller's frame to that `pc`,
offers a yield if one is asked for, and otherwise takes ADR 0040's safepoint
if it is due.

### Where a yield there resumes

At the call's `returns` entry ([ADR 0085]'s "the instant after a call"), which
is now the poll's first byte rather than the code after it. That one change
covers both frames that need it:

- the innermost, yielded at this poll (`Stands::AfterReturn`): it is entered
  at the poll and takes it again, and so takes the safepoint the uninterrupted
  run took there. Nothing is taken before entering, which is
  `Stands::AtInstruction`'s rule and not `Stands::AtBlock`'s;
- every frame below it, waiting on a call: `resume` closes each callee as the
  call would have and enters its caller at the same point — so a chain
  resumed half-way through an unwinding polls the rest of the way up, as the
  uninterrupted run does. With the entry left after the poll, a resumed
  ascent took no safepoint at all, and the yield test's position-by-position
  schedule comparison found it.

### Why not at the callee's `Return`

Measured too, in three forms (below): the poll in the `Return` template with
the instruction as a resume point, the same with the frame pointer kept live
on the not-due path, and the same skipped in functions that make no call. None
was cheaper than the caller's, and the first two add two instructions to every
return, where the caller's adds none to an inline return. A recursive
function is never a leaf, so skipping leaves buys nothing on the programs this
is for.

### The bound, stated

The work between two polls in compiled code is at most `S + T` in a descent
(ADR 0078), around a loop (ADR 0060), and now in an ascent: one level of the
return and whatever straight-line code follows it in the caller before its
next poll. `vm::stops` asserts it over **whole** runs, on both tiers: stops and
yield requests are raised across all of `counts(30000)`, at least five of each
set in its ascent, and each is held to `S + T`. The 45% window is gone.

## Consequences

Measured on x86-64 macOS with one binary and a temporary switch between the
two code generators, so no rebuild enters the comparison; "execution" from
`cove run --backend native`; interleaved, arm order rotated each round, each
sample started only once the one-minute load was under 4.5. Median [min..max]
in ms; "paired" is the median of each round's ratio to the arm without the
poll, and how many rounds were slower.

| row | no return poll | return poll | delta | paired |
| --- | ---: | ---: | ---: | ---: |
| covefmtBench, n=11 | 3,063.7 [3,053..3,093] | 3,049.5 [3,032..3,076] | −0.46% | −0.30%, 2/11 slower |
| covefmtBench `whole`, n=11 | 281 [276..291] | 280 [277..286] | −0.36% | −0.71%, 4/11 |
| cq `revenue-summary` 100k, n=11 | 3,009.9 [2,986..3,075] | 3,065.1 [3,052..3,165] | +1.83% | +1.93%, 10/11 |
| cq again, n=15 | 3,016.1 [3,000..3,214] | 3,077.7 [3,047..3,121] | +2.04% | +2.16%, 13/15 |
| 20M closure calls through `filter`, n=11 | 1,505.3 [1,495..1,568] | 1,503.3 [1,496..1,527] | −0.13% | +0.13%, 6/11 |
| `fib(32)`, 7M calls, n=31 | 58.6 [57.8..68.2] | 59.1 [57.7..81.0] | +0.85% | +1.21%, 22/31 |
| `fib(32)` again, n=15 | 57.9 [57.7..60.1] | 59.3 [57.9..61.9] | +2.42% | +1.56%, 12/15 |
| 400 × `counts(20000)`, n=31 | 116.7 [115.4..125.5] | 118.2 [116.3..127.0] | +1.29% | +1.07%, 25/31 |
| 400 × `counts(20000)` again, n=15 | 116.2 [115.2..130.9] | 118.0 [116.2..125.2] | +1.55% | +1.45%, 11/15 |

The callee-side forms, on the same rows: covefmt +0.73%, cq +2.27%,
closures +0.98%, `fib` +1.0 to +2.6%, `counts` +1.5 to +2.9%.

Machine code grows by about 2% — covefmt 1,508,119 → 1,538,746 bytes, cq
1,051,055 → 1,071,295 — because there is a poll at every call site. Executed
counts do not move: the encoded tier's instructions and dispatches, the
native-to-native and mediated call counts, and the work are the same numbers.
What moves is which poll takes a due safepoint: under `--boundary`, covefmt's
safepoint-helper calls go from 1,011,029 to 1,032,549, `fib`'s from 0 to
15,927 — safepoints a callee's `open` used to take, now taken by the return
before it.

**So this costs about 2% on call-dense native code** — cq and pure recursion —
reproducibly, in two sessions, and nothing on covefmt or on closure calls.
That is inside cq's ±2.9% rebuild floor, but these arms are one binary, so
that floor does not explain it; with no instruction added to the inline path,
the likeliest cause is the 2% of code. It was taken knowingly on issue #624, as
[PHILOSOPHY](../PHILOSOPHY.md)'s "a small measured slowdown may buy …
correctness" allows: the price of a stop bound that holds while a recursion
unwinds rather than only while it descends. It is not a change of performance
class.

- A cancellation, a deadline or a yield request raised during an unwinding is
  now noticed within `S + T`, where it was noticed at the outermost frame.
- A collection another task asks for waits at most `S + T` of this task's work
  there too.
- A run that yields during an unwinding resumes with the same safepoint
  schedule as the run that did not yield, which `vm::stops` asserts.

## Alternatives considered

**Document the gap instead.** A sentence saying that the stride bounds descent,
loops and calls, and that a loop-free recursion's unwinding is bounded only by
its depth. It costs nothing, and it leaves the one stop condition a host cannot
work around — a deep recursion it did not write — unbounded in exactly the
direction a program spends half its time in. Offered on #624, and declined.

**Poll at the callee's `Return`.** Measured and set aside above: no cheaper,
and two instructions on every return.

**Poll only returns from recursive functions.** A function is recursive through
a cycle of any length and through closures, and a static answer the code
generator could trust would be a call-graph analysis it does not have. The
caller-side poll costs no instruction on the path the analysis would have
spared, so there is nothing for it to buy.

[ADR 0060]: 0060-a-backedge-tests-the-stride-before-it-calls.md
[ADR 0079]: 0079-a-direct-call-opens-its-frame-in-emitted-code.md
[ADR 0085]: 0085-compiled-frames-resume-where-they-yielded.md
[ADR 0087]: 0087-compiled-code-calls-the-host.md
