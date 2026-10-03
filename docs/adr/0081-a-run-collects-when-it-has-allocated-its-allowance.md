# ADR 0081: A run collects when it has allocated its allowance

- Status: Proposed
- Date: 2026-10-03
- Issue: [#572](https://github.com/myuon/cove/issues/572)
- Refers to:
  [ADR 0034](0034-one-physical-word-stack.md), which leaves "the concrete heap
  allocator or garbage-collection algorithm" undecided, and so is where both
  halves of this land;
  [ADR 0055](0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md)'s
  safepoint contract, under which compiled code already collects from its
  allocating helper;
  [ADR 0080](0080-a-host-call-may-answer-pending.md) and #570's
  `PreparedProgram`, the isolate work this is the memory half of
- Supersedes: nothing. No accepted decision says when the linear memory
  collects; `Machine::allocate` collected when an allocation did not fit the
  budget, and that is what changes
- Decides: when a run collects, what a run's heap is bounded by, and how the
  allocator hands back a freed block

## Context

Issue #571 asks for `Vm`s to behave like V8 isolates — thousands resident in
one process, each invoked again and again. #570 and #573 made one cheap to
*build*: 21.8 KB for a run of `rules.floor`, 24.5 KB for `rules.decideSample`.
What decides the footprint of a thousand resident ones is what each costs once
it has been *used*, and that was unbounded.

A run collected in exactly one place: `Machine::allocate`, when the allocator
answered that the object fitted neither a free block nor what was left of the
budget (`DEFAULT_HEAP_WORDS`, four mebiwords, 32 MiB). So a resident run that
allocated a few hundred words of garbage per invocation committed a 64 KiB
chunk every few invocations until it had all 32 MiB, and only then collected —
to find 288 words alive. Measured with the new `cove-rules-isolates resident`
(N prepared VMs kept alive, each invoked K times round-robin; checked profile,
macOS x86-64):

| entry | N | K | retained / VM | RSS / VM | heap words / VM | collections / VM |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `floor` | 1,000 | 1 | 21,816 B | 22,475 B | 4 | 0 |
| `floor` | 1,000 | 1,000 | 87,352 B | 88,044 B | 4,000 | 0 |
| `floor` | 100 | 10,000 | 349,496 B | 340,255 B | 40,000 | 0 |
| `decideSample` | 1,000 | 1 | 24,472 B | 25,088 B | 360 | 0 |
| `decideSample` | 1,000 | 10 | 90,008 B | 88,293 B | 1,458 | 0 |
| `decideSample` | 1,000 | 100 | 155,544 B | 154,644 B | 12,438 | 0 |
| `decideSample` | 1,000 | 1,000 | 1,007,512 B | 1,007,174 B | 122,238 | 0 |
| `decideSample` | 100 | 10,000 | 9,789,336 B | 9,791,980 B | 1,220,238 | 0 |
| `decideSample` | 10 | 50,000 | 34,106,960 B | 34,066,022 B | 4,194,299 | 1 (live: 288 words) |

A thousand decision isolates that have each served a thousand requests held a
gigabyte, of which a thousand times 2.3 KB was alive. That is the whole of the
issue, and it is as the issue described it, with one thing the table adds: it
is linear in the number of invocations and nothing else, so the figure for any
deployment is "how long has it been up".

The same rule decided how throughput programs behave, and that is the half
that makes this a decision rather than a fix. Every `benches/` row that
allocates more than a chunk filled its 32 MiB before collecting, or ended
first: `chars` collected once at 4,194,275 words to find 25 alive,
`builtincall` 21 times with 110 alive, `covefmtBench` 34 times with about
265,000 alive. Collecting earlier changes how often every one of them
collects.

### What a collection costs, and what it gives back

`Space::mark_sweep` marks from the roots — work in the live set — and sweeps
the heap from the static floor to the bump pointer, object by object — work in
the *occupied* heap, which is why a 32 MiB heap's sweep is not free even when
nothing is alive. It rebuilds the free blocks and nothing else: the bump
pointer never retreats and a committed chunk is never released. That is not an
omission to fix in passing. Compiled code holds every chunk's address
(`Tiering::chunks`, `cove_native`'s chunk table), and `Words::bases` relies on
the committed chunks being a prefix that only grows; a chunk that could be
unmapped would be a dangling pointer in a table emitted code reads without a
check.

So what a collection buys a resident run is that *later* allocations reuse the
words it freed, and the heap stops growing. It does not make RSS fall after a
spike, and nothing here claims it does.

## Decision

### 1. A run collects when it has allocated its allowance

The allocator counts the words it has handed out since the last collection.
Once that reaches the run's **allowance**, the next allocation an instruction
asks for answers "collect first", exactly as an exhausted budget always has,
and `Machine::allocate` collects and retries. After every collection the
allowance is

```text
allowance = max(PACE_MIN_WORDS, PACE_GROWTH × live words)
PACE_MIN_WORDS = 8,192 words (one full heap chunk, 64 KiB)
PACE_GROWTH    = 2
```

and before the first, it is the same function of the static region, counted
from the moment the literals are sealed.

The budget still bounds the heap, and an allocation that does not fit it still
collects. Pacing is only ever *earlier*: it decides when a run collects, never
whether a run may have the words its budget allows. Two things follow and are
in the code:

- the retry after a collection is **unpaced**, so another task allocating
  between the collection and the retry cannot turn "collect first" into "out
  of memory";
- the literals' placement is unpaced too, as it has to be: nothing may be
  collected before `seal_static`.

It is one mechanism with one default and no knob. A thousand resident runs and
a formatter over the repository want the same thing from it — a heap
proportional to what is alive, and a collection paid for by the allocation
that made it necessary — and neither has a reason to set a number of its own;
see "Strong defaults". There is no embedder-facing setting, and no
`Vm::collect`; see "Alternatives considered".

**Why one chunk.** The heap is committed a chunk at a time, and after the 4 KiB
first chunk the next commitment is a whole 64 KiB one. A smaller minimum saves
no memory for any run that outgrows the first chunk and costs it collections; a
larger one is memory every resident run holds for nothing. Measured on the
resident table at K = 1,000 (before the free list below changed, which moves
these figures by a few hundred bytes):

| minimum | `floor` retained / VM | `decideSample` retained / VM | `decideSample` collections / VM | `decideSample` invoke |
| ---: | ---: | ---: | ---: | ---: |
| 256 words | 21,944 B | 92,200 B | 444 | 16.6 µs |
| **8,192 words** | 87,304 B | 94,232 B | 14 | 15.2 µs |
| 65,536 words | 87,304 B | 558,040 B | 1 | 15.4 µs |

(At 256, `floor` stays inside its first chunk, but `decideSample`'s 258 live
words plus its allowance do not, so it commits the second chunk anyway and
collects thirty times as often, 9% slower per invocation.)

**Why two.** A run whose allocations all survive — the case the factor is
for — marks each surviving word about `1 + 1/PACE_GROWTH` times over its life
and peaks at `1 + PACE_GROWTH` times what it keeps. Measured on the two
programs written to be that case (see "Measured"), against pacing switched off
in the same build: at one they were 24% and 24% slower, at two 15% and 18%, at
three 17% and 6%. Three buys little over two on one of them and nothing on the
other, and lets a growing heap reach four times what it keeps; two keeps it
within three.

### 2. A free block is passed at most once per collection

The free list was one `Vec<u64>` of blocks in address order, searched first
fit from the front, with a `Vec::remove` when a block was used whole. Each
operation was linear in the number of free blocks. That was harmless while a
run collected at most a few times, from a heap that was mostly garbage — a few
huge blocks — and it is a change of performance class the moment a run
collects with a large live set of small objects: every survivor leaves a hole,
and a request that fits none of them walks all of them, every time. This was
latent before pacing, not introduced by it:

| program | `main` (one collection, at the budget) | this change |
| --- | ---: | ---: |
| 250,000 one-element vectors kept, two discarded between each | **7,120 ms** | 208 ms |

and pacing made it ordinary: with the old list and a paced trigger, the same
kind of program at 200,000 took 13,362 ms where unpaced it took 147.

So the free blocks are now walked **once**:

- A sweep leaves them in one list in address order, threaded through the
  blocks' own first payload words (`0`, never a heap address, ends it).
- Allocation cuts requests from the front of the current block — the
  **cursor** — while they fit, and when one does not, moves on to the next
  block in address order. That is the old first fit's behaviour exactly in the
  case it was good at: a mostly-garbage heap is a few long runs, and objects
  are cut from them one after another, low addresses first.
- A block the walk passes because the request in hand did not fit it — and the
  cursor's remainder when it is left — goes to a list **by size**: one per exact
  size up to 64 words and one per power of two above, each threaded through the
  blocks like the first, with a bit per list that says it is not empty. When
  the walk is over, a request takes the first block of the lowest non-empty
  list *every* block of which fits it: a mask, a trailing-zeros count and a pop.

So every block is passed at most once per collection and every allocation is a
constant number of list operations, however many blocks there are. A one-word
block has no payload word to thread and is on no list; the next sweep
coalesces it.

The size lists give up one thing: a block in the request's own power-of-two
list that happens to be big enough, which a list cannot know without reading
it. That block is reached only when the alternative is refusing: an allocation
the budget cannot hold scans that one list before it answers "out of memory",
so a run may still occupy every word of its budget.

The allocator's fast path is the cursor alone, inlined: a request the cursor
holds is a load, a compare and a header write, as first fit's was, and the
walk, the size lists and the budget's last-resort scan are calls out of line.

Three other shapes were built and measured on the way here, against `main` on
the rows that moved most, and the record is worth having:

| shape | `chars` | `callback` | `conv_fresh` | `contains_slice` | resident `decideSample` |
| --- | ---: | ---: | ---: | ---: | ---: |
| size lists only, smallest fitting block first | +3.4% | +4.4% | +5.3% | +2.7% | 94.9 KB |
| the walk, everything inline | +1.9% | +5.9% | +3.0% | +3.1% | 94.9 KB |
| **the walk, the cursor alone inline** | **+1.1%** | **+3.3%** | **−0.5%** | **+1.5%** | **94.9 KB** |

and, before the lists were threaded through the blocks, one `Vec` of addresses
per class held 4.3 KB more per resident `decideSample` run (98.5 KB against
94.2 KB) — a few per cent of all a resident run holds. Threaded, what a run
pays is a few words, and 91 list heads (728 bytes) once its walk has first
passed a block. Size lists alone, smallest block first, hop about the heap
taking the smallest hole that fits; the walk cuts objects from long runs in
address order, as first fit did, and that is most of the difference.

ADR 0034 leaves the allocator undecided, and this does not decide it either:
it is the allocator the heap has now, chosen because the one before it was
quadratic in exactly the situation pacing creates.

### 3. What stays as it was

- The collector: non-moving, stop-the-world, mark and sweep, the same roots.
  A paced collection is the collection an exhausted budget always ran, asked
  for at the same place (`Machine::allocate`), from the same allocating
  helper on the native tier. No new safepoint, no new root, no new thread
  rendezvous.
- Committed chunks are never released, and so a heap's addresses and the
  native tier's chunk table are as stable as they were. Pacing bounds how far
  a heap *grows*; it does not shrink one.
- The budget, and the failure when it is exhausted.

## Measured

All on one macOS x86-64 machine, checked profile, `main` at `3d67ec9` against
this change, interleaved, minimum of the rounds unless it says otherwise.

### Resident runs (`cove-rules-isolates resident`)

| entry | N | K | before: retained / VM | after: retained / VM | after: RSS / VM | after: heap words | after: collections / VM |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `floor` | 1,000 | 1 | 21,816 B | 21,816 B | 22,159 B | 4 | 0 |
| `floor` | 1,000 | 1,000 | 87,352 B | 87,352 B | 85,860 B | 4,000 | 0 |
| `floor` | 100 | 10,000 | 349,496 B | 89,448 B | 89,866 B | 8,192 | 4 |
| `decideSample` | 1,000 | 1 | 24,472 B | 24,472 B | 24,916 B | 360 | 0 |
| `decideSample` | 1,000 | 10 | 90,008 B | 90,008 B | 89,432 B | 1,458 | 0 |
| `decideSample` | 1,000 | 100 | 155,544 B | 92,136 B | 91,386 B | 8,432 | 1 |
| `decideSample` | 1,000 | 1,000 | 1,007,512 B | 94,880 B | 96,252 B | 8,520 | 14 |
| `decideSample` | 100 | 10,000 | 9,789,336 B | 94,880 B | 81,592 B | 8,520 | 148 |
| `decideSample` | 10 | 50,000 | 34,106,960 B | 94,880 B | — | 8,520 | 744 |

(RSS per VM at N = 10 is below the resolution of `ps`'s figure and is left
out.) A resident decision run is now 95 KB whether it has served a hundred
requests or fifty thousand: the first chunk, one full chunk, and the run
itself. Invocation time did not move: `decideSample` 15.1 µs before and
15.4 µs after at K = 1,000, 14.6 µs before and 14.5 µs after at N = 10,
K = 50,000.

### Throughput

`benches/` rows on `--backend vm`, every row that allocates more than a chunk
plus three controls, through `cove run` (compilation included), three
interleaved rounds. Collections and heap words are from the run's
`heap_summary`.

| row | collections | heap words | `main` | this change | change |
| --- | ---: | ---: | ---: | ---: | ---: |
| `chars` | 1 → 741 | 4,194,275 → 8,511 | 576.0 ms | 582.5 ms | +1.1% |
| `callback` | 0 → 91 | 750,076 → 8,246 | 264.5 ms | 273.2 ms | +3.3% |
| `conv_fresh` | 1 → 732 | 4,194,302 → 8,201 | 394.7 ms | 392.8 ms | -0.5% |
| `conv_host` | 2 → 1,464 | 4,194,304 → 8,230 | 72,354.7 ms | 71,675.3 ms | -0.9% |
| `contains_slice` | 0 → 380 | 3,120,056 → 8,249 | 378.8 ms | 384.4 ms | +1.5% |
| `parse_rows` | 0 → 258 | 2,122,981 → 8,464 | 259.8 ms | 260.5 ms | +0.3% |
| `parse_radix_rows` | 0 → 146 | 1,200,401 → 8,450 | 311.0 ms | 306.7 ms | -1.4% |
| `split_rows` | 6 → 3,105 | 4,194,283 → 9,339 | 1,707.6 ms | 1,691.1 ms | -1.0% |
| `builtincall` | 21 → 11,226 | 4,194,303 → 8,311 | 17,301.9 ms | 17,068.8 ms | -1.3% |
| `keyed` | 3 → 1,960 | 4,194,300 → 17,746 | 2,400.6 ms | 2,293.7 ms | -4.5% |
| `slice` | 0 → 305 | 2,500,215 → 8,332 | 838.5 ms | 843.0 ms | +0.5% |
| `frompoint` | 1 → 585 | 4,194,298 → 8,316 | 477.1 ms | 469.1 ms | -1.7% |
| `join` | 0 → 277 | 2,280,605 → 8,526 | 331.0 ms | 326.0 ms | -1.5% |
| `chars_rows` | 1 → 596 | 4,194,304 → 9,040 | 328.8 ms | 328.5 ms | -0.1% |
| `words_rows` | 0 → 327 | 2,693,933 → 9,042 | 531.4 ms | 540.5 ms | +1.7% |
| `upper_rows` | 0 → 43 | 357,357 → 9,197 | 543.4 ms | 529.7 ms | -2.5% |
| `lower_rows` | 0 → 43 | 359,650 → 9,512 | 762.6 ms | 754.4 ms | -1.1% |
| `equals` | 0 → 55 | 581,476 → 16,090 | 1,212.3 ms | 1,231.4 ms | +1.6% |
| `rendering` | 0 → 356 | 2,924,626 → 17,889 | 339.6 ms | 336.7 ms | -0.9% |
| `casemap` | 0 → 6 | 53,405 → 9,608 | 82.4 ms | 79.0 ms | -4.1% |
| `seqsearch` | 0 → 2 | 17,210 → 13,260 | 611.8 ms | 616.9 ms | +0.8% |
| `hostheavy` | 0 → 1 | 14,008 → 8,202 | 67.0 ms | 61.8 ms | -7.8% |
| `arith` | 0 → 0 | 8 → 8 | 105.0 ms | 105.4 ms | +0.4% |
| `method` | 0 → 0 | 8 → 8 | 121.4 ms | 121.0 ms | -0.3% |

Every row runs in a heap of 8,200 to 18,000 words where most filled 32 MiB,
and every row but one is within 1.7% of `main` or faster. `callback` (+3.3%)
is the cost reuse has where there was none: it allocated 750,076 words in all
and so never collected before, and now it sweeps 91 times and zeroes every
block it reuses. The rows that got faster (`keyed` −4.5%, `casemap`,
`upper_rows`) reuse a few chunks that stay in cache instead of committing and
zeroing fresh ones. (`hostheavy`'s −7.8% is a 67 ms run with one collection in
it, and is the noise on a short row, not a result.)

`covefmtBench` over the repository (384 files, 1.95 MB; five interleaved
rounds; the outputs, timing lines stripped, are byte-for-byte the same on
both builds and both tiers):

| tier | `main` whole | this change | change | collections | heap words | live words at the end |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| VM | 1,159 ms | 1,170 ms | +0.9% | 34 → 238 | 4,194,298 → 2,401,148 | 264,497 → 251,724 |
| native | 292 ms | 296 ms | +1.4% | 34 → 238 | 4,194,298 → 2,401,148 | 264,497 → 251,724 |

`examples/cq` over 100,000 generated records (`cqSample`'s seed; three
interleaved rounds):

| program | tier | `main` | this change | change | collections | heap words |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| `revenue-summary` | VM | 11.32 s | 11.50 s | +1.5% | 10 → 5,590 | 4,194,304 → 10,689 |
| `confirmed-bookings` | VM | 14.84 s | 14.94 s | +0.7% | 17 → 9,097 | 4,194,304 → 10,462 |
| `revenue-summary` | native | 5.42 s | 5.40 s | −0.5% | 10 → 5,590 | 4,194,304 → 10,689 |
| `confirmed-bookings` | native | 7.96 s | 7.90 s | −0.8% | 17 → 9,097 | 4,194,304 → 10,462 |

No representative program moved by more than 1.5%, in either direction, on
either tier.

### The two programs written to be the worst case

Every allocation survives, or survives with garbage of the same size beside it
— the programs a growth factor exists for, and the ones the free list must not
lose on. Not checked in: each is a loop of `Vector.of()` and `push`, and
neither is a program anybody would write except to measure this. Best of five
(`main` best of three); `cove run`, compilation included.

| program | `main` | this change | change | collections | heap words |
| --- | ---: | ---: | ---: | ---: | ---: |
| `frag` — 200,000 kept, two discarded between each | 144.9 ms | 174.4 ms | +20% | 0 → 10 | 4,124,311 → 2,647,299 |
| `grow` — 300,000 kept, nothing discarded | 117.1 ms | 140.1 ms | +20% | 0 → 7 | 3,748,600 → 3,486,419 |
| `frag` at 250,000, which fills the budget | 7,135.6 ms | 200.4 ms | −97% | 1 → 10 | 4,194,302 → 3,547,299 |

The first two are what a growth factor costs: a run that keeps everything
marks it about one and a half times over, and nothing it frees pays for that.
They are written to have nothing else in them, and the cost is a fifth of a
run's wall clock, not a multiple of it. The third is the old free list meeting
one collection at the budget.

## Consequences

- A resident run's heap is bounded by its live set and its allowance, not by
  how long it has been resident: 8,520 words for `rules.decideSample` after
  fifty thousand invocations, against the full four mebiwords.
- Every run that allocates more than a chunk now collects, most of them
  hundreds or thousands of times where they collected once or never. That is
  the point, and it is also the most thorough exercise the collector has had:
  `cove test` in `examples/` (165 tests) and in `tools/covefmt` (75) both pass
  with the minimum forced down to 512 words and no growth, which is a
  collection every 512 words, and `covefmtBench` prints the same bytes on the
  VM and on the native tier that way.
- Programs run in a heap a few chunks wide rather than tens of megabytes, which
  is why most `benches/` rows are no slower: the same words are reused while
  they are still in cache, instead of a fresh chunk being committed and zeroed.
- A program that only ever grows pays for marking what it keeps, about one and
  a half times over: 20% on the two programs written to be that and nothing
  else, at most 1.5% on `covefmtBench` and `cq`, and 3.3% on the one
  `benches/` row that used to finish without collecting.
- The free list is no longer quadratic in the number of survivors, which it
  was — 35 times a run's wall clock after one collection at the budget — for
  any program that filled the budget with a fragmented live set.

## What this does not decide

- **Giving memory back.** A run that once needed many chunks keeps them.
  Releasing trailing chunks would mean retreating the bump pointer and either
  unmapping chunks the native tier holds addresses of, or advising the kernel
  that their pages are free while keeping the mapping; either is a change to an
  invariant `Words::bases` and the chunk table rely on, and it should be
  decided by a workload that spikes and then stays resident, which none of the
  measured ones does.
- **An embedder's own policy.** No soft limit, no per-run allowance, no
  explicit collection. Each is easy to add over this mechanism if an embedder
  shows a need the default cannot meet.
- **A different allocator.** The walk and the size lists are what the heap
  has; ADR 0034's open question stays open.

## Alternatives considered

**Keep collecting only at the budget, and lower the budget for resident
runs.** `Vm::with_heap_words` already exists. It moves the problem rather than
solving it: the embedder must guess a number per program, a guess too low is a
run that fails rather than one that collects, and every resident run still
grows to whatever the guess is — 8,192 words is `decideSample`'s heap *by
pacing*, and would have to be found per program by hand. "Strong defaults":
this is the decision every embedder would have to repeat.

**An explicit `Vm::collect` (and `OwnedVm::collect`) between invocations.** As
a substitute it needs the embedder to know when, and it does nothing the
paced trigger does not already do at the right moment. As an *addition* it
buys nothing measurable: a collection releases no chunk, so collecting an idle
run lowers neither its retained bytes nor its RSS — it only changes which
later allocation pays. Not added; it can be, over the same mechanism, when
there is a measurement it moves.

**Collect at the end of an invocation past a threshold.** That is pacing with
the check moved to a place only the embedder-facing entry points reach. It
does nothing for a long invocation, for a spawned task, or for a run that
never returns to the host, and it collects with an empty stack only by
coincidence of where it is called. The allocation path is where the
information is and where a collection already happens.

**A growth trigger on *committed* chunks** ("collect when the committed size
doubles"). It would bound a resident run too, and it is close to pacing with
the allowance set to the heap's size. What it lacks is the live set: the
committed size is a high-water mark that never falls, so after one large
invocation a run that now keeps two kilobytes is still paced by the megabytes
it once needed, and a heap that only grows is collected at every doubling of
its *size* rather than of what it keeps. It also moves in 64 KiB steps. Words
allocated since the last collection is the quantity a collection's cost is
amortised over, at word granularity, and the live set is what a collection
measures anyway; pacing uses both.

**Growth factor one, or three.** Measured above: one costs 24% on both
programs that keep everything, for no saving a resident run can see (its live
set is far below the minimum either way); three saves little more than two on
one of them and lets a growing heap reach four times its live set.

**Keep the address-ordered first fit.** Measured above: 13.4 s against 0.17 s
with pacing on the fragmenting program, and 7.1 s against 0.20 s at a larger
size where `main` collects once. Not a trade-off — a change of performance
class.
