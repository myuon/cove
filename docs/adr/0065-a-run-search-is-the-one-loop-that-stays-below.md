# ADR 0065: A run search is the one loop that stays below

- Status: Proposed
- Date: 2026-09-21
- Decides: that the IR gains **one** instruction, a bounded search over a run
  of packed bytes; its operands and every edge of its meaning; that it is
  chunked, charged and polled by the rule
  [ADR 0058](0058-collection-apis-lower-through-typed-run-intrinsics.md)'s run
  family already has; that both code generators lower it in the pull request
  that adds it; and — the part worth arguing — **why an operation belongs
  below the boundary, stated as the rule the run family already runs on rather
  than as a claim about asymptotic complexity.** Phase 1 of
  [issue #432](https://github.com/myuon/cove/issues/432)
- Departs from, without superseding:
  [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md)'s **"It is not
  added on this ADR's authority. It is added when Phase 1's whole-program
  measurement asks for it, and refused if it does not."** Phase 1's
  whole-program measurement does **not** ask for it, and by that sentence
  alone this primitive would be refused. It is added anyway, for a reason ADR
  0064 could not have had: the measurement it set that rule against was
  measuring something else. The rule is not withdrawn — Decision 6 restates it
  in terms that cover `RunCopy` and `RunSlice` too, which the original did not
- Refers to, without superseding: ADR 0064's Decision 2, whose acceptable
  vocabulary already names "a bounded proportional run operation — **search**,
  compare, slice, copy — whose work is charged and which is cancellable under
  ADR 0040", so nothing here widens what a primitive may be; ADR 0064's
  Decision 1, untouched — `Intrinsic` still only shrinks and this is not one;
  ADR 0058's run substrate, of which this is the sixth member;
  [ADR 0040](0040-a-bound-outlives-its-backend.md)'s `S + T`, which is what
  Decision 4 exists to satisfy;
  [ADR 0052](0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md)'s
  "charged proportionally to the bytes or words examined"
- Changes no source-language API

## Context

ADR 0064 anticipated this instruction and refused to authorise it:

> **A byte-run search is the first primitive this ADR is likely to owe.** […]
> It is not added on this ADR's authority. It is added when Phase 1's
> whole-program measurement asks for it, and refused if it does not.

Three of Phase 1's predicates have since migrated —
[#439](https://github.com/myuon/cove/pull/439) `endsWith`,
[#443](https://github.com/myuon/cove/pull/443) `startsWith` — as Cove byte
loops with nothing beneath them, and both *faster* than the intrinsic at the
sizes real programs use. On that evidence `contains` should be next and should
need nothing. Two things established since ADR 0064 say otherwise.

### The baseline ADR 0064 measured `contains` against was inflated

[Issue #442](https://github.com/myuon/cove/issues/442) found that every text
intrinsic materialised its operands before it looked at them: `operand::text`
built an owned Rust `String` per operand per call, through
`Machine::string_bytes`, which is not a memcpy but a nested loop doing one
shift and one `push` per byte, and then validated the result again. None of it
appeared in any counter — the allocations are Rust-side, so `--boundary`'s
`allocs` and `words` columns read zero for every one of these variants.

On ADR 0064's own `contains` inputs that cost **146.5 ns, 59.5% of the
intrinsic's 246.4 ns**. [PR #446](https://github.com/myuon/cove/pull/446)
removed it: 246.4 → 99.9 ns, every executed count byte-identical.

So ADR 0064's two `contains` figures were not arithmetic errors; they were
measurements of a different thing. Against a copy-free intrinsic:

| | ADR 0064, as measured | against a copy-free intrinsic |
| --- | --- | --- |
| `contains` VM | 185.2 ns intrinsic, 743.5 Cove → **4.02x slower** | **≈17x slower** |
| `contains` native | 181.2 ns intrinsic, 158.3 Cove → **1.14x faster** | **≈4.1x slower** |

**The native sign flips.** The single number that made a `contains` migration
look free on the tier these operations are going to was the operand copy. That
is the correction ADR 0064 could not have made, because the copy was invisible
to every counter it had.

### What is actually different about `contains`, stated correctly

An earlier draft of this ADR said the difference was asymptotic — O(n·m) in
Cove against O(n+m) below. **That is wrong and is corrected here.** Cove has
loops, arrays and arithmetic; a two-way search or a KMP table is perfectly
expressible in it, and a Cove KMP would be O(n+m) like any other. Neither
expressibility nor complexity class distinguishes the two sides.

What distinguishes them is that **a Cove loop pays a VM dispatch per unit of
proportional work and an instruction pays one for the whole of it.** That is a
statement about dispatch counts, which are exact: the migrated predicates
execute **seven instructions per byte examined** — measured to the instruction
on `endsWith`, whose whole-program delta reconciled as `5×85 + 6×1074 +
7×1403 + 6×806 + 2×268 − 1159 = 20,903` against an observed 20,903 — so a Cove
scan of an 88-byte haystack dispatches at least 616 instructions however good
its algorithm is. `RunFind` dispatches one.

The time that buys is measured directly rather than inferred from a
coefficient. On the `noMatch` row below — an 88-byte haystack, a 6-byte
needle, no match, so both sides examine everything — the intrinsic costs
**151 ns for the whole operation** and the Cove scan **4,729 ns**. The
intrinsic's entire cost on a full 88-byte scan is less than what a Cove loop
spends examining six bytes, at its measured **≈30 ns a byte**.

And it does not depend on which algorithm either side runs. A Cove KMP over
that haystack still turns its loop 88 times: ≈88 × 30 ≈ **2,600 ns against the
same measured 151** — **≈17x with the asymptotics equal**, and all of it
dispatch.

**This is not a new argument.** It is the argument `RunCopy` and `RunSlice`
already stand on. Nobody proposes writing a byte copy as a Cove loop, and the
reason is not that Cove cannot express one — it is that a copy's work is
proportional to a run and Cove would pay a dispatch per unit of it.
`Inst::RunCopy`'s own doc comment says so: "one dispatch and one unit of work
per payload word moved". A search is the same shape — work proportional to a
run, each unit a load and a compare — and belongs in the same family for the
same reason.

What the naive-Cove measurement below still shows is not the class but the
constant, on the body anyone would actually write: no standard library writes
KMP for a six-byte needle.

### `startsWith` and `endsWith` are a different shape, and that is the rule

They compare at a known offset. Their work is bounded by the **needle**, which
is the caller's own argument — covefmt's `endsWith` reads 2,209 bytes over
1,159 calls, because three quarters of them end after one byte. The work is
small because the caller asked for something small, and a dispatch per unit of
it costs little.

`contains` must touch the haystack, which the caller did not size. That is the
distinction Decision 6 turns into a rule.

## Decision

### 1. One instruction, over one storage

`Inst::RunFind { args: ArgsId, storage: Storage }`, a sixth member of ADR
0058's run family beside `RunLoad`, `RunStore`, `RunSlice`, `RunCopy` and
`RunFinish`, with its operands behind an `ArgsId` as `RunSlice`'s and
`RunCopy`'s are.

**`Storage::PackedBytes` only.** Not `Storage::Elements`. ADR 0058 moved
`Array.contains` and `Array.indexOf` to Cove loops over `==` and they stay
there. This is stated as a restriction rather than left implicit because the
temptation runs the other way: "a bounded run search over a named storage"
sounds general, and defining it over one storage while calling it general
would be naming a `String` primitive after a capability it does not have. A
sequence search that wants this instruction gets its own decision, its own
measurement, and `Array`'s Cove loops as the thing to beat.

### 2. The operands, and every edge of the meaning

The row holds exactly four `Arg`s, in the order **`dst`, `haystack`,
`needle`, `from`**. `dst` is *written*, as `RunSlice`'s is, and the pass
asking what an instruction writes reads it out of the row. Both runs' lengths
come from their headers, as `RunSlice`'s `src` length does; neither an offset
nor a count is passed for either run.

- **The answer is an absolute unit offset into the haystack run**, not an
  offset relative to `from`. Relative would be a footgun at every call site,
  and `indexOf`, `split` and `replace` all want a position in the receiver.
- **Not found is `-1`.** Not an `Option`: an instruction answers a word, and
  the standard library builds what the public API needs — `std.string.contains`
  a `Bool`, `std.string.indexOf` an `Option<Int>` in *character* positions
  after a walk of its own. That split is ADR 0058's and does not change.
- **An empty needle answers `from`.** `str::find("")` is `Some(0)`, and with a
  start it is that start; the empty needle occurs at every position including
  the end.
- **A needle longer than `haystack_len - from` answers `-1`**, and is not an
  error. So does any needle that does not occur.
- **`from` must be in `0 ..= haystack_len`.** `from == haystack_len` is legal
  and answers `-1`, or `from` for an empty needle. A `from` outside that range
  **stops the run**, as a range outside the source does in `RunSlice`: it is a
  broken invariant of the lowering, never a program's mistake, and the
  standard-library body above it is what holds a program's index to the range
  — `std.string.contains` passes zero, and a future `split` computes `from`
  from a previous answer and a needle length, both already in range.
- **A null run stops the run**, as it does in `RunSlice`.
- **The two runs may alias.** `s.contains(s)`, a needle that is the haystack,
  and two overlapping runs of one object are all defined: the instruction only
  reads, so there is no order in which a write could be seen. Stated because
  `RunCopy`'s aliasing rules are not this one's and a reader will ask.
- **It searches bytes and knows nothing else.** The comparison is bitwise over
  units of `Storage::PackedBytes`. The instruction has no notion of a
  character, an encoding, a boundary or a `String`; it neither validates nor
  interprets what it reads, and it would answer the same for a run of
  arbitrary bytes that never came from text. **Whether a byte match is also a
  character match is `std.string.contains`' question, argued in
  `std.string`** — by UTF-8's self-synchronisation, the way
  `std.string.endsWith` already argues it for its own offset. Putting that
  argument here would be writing `String` policy into an instruction, which is
  the thing ADR 0064's Decision 2 refuses.
- **Both operands are fixed runs.** A run under construction is not admitted,
  for the reason `RunSlice` gives. If a byte buffer ever wants searching before
  it is finished, that is a decision with its own measurement.

### 3. The algorithm lives beneath the instruction

The runtime's implementation may not be a quadratic scan. If the work below
the boundary is what a Cove loop would have done anyway, the Context's argument
supports nothing and the instruction should not exist.

Decision 4 constrains *how* it may be written, and the two together are the
requirement: a resumable matcher, linear in the haystack and the needle
together, that can stop between any two units and continue.

### 4. It is chunked, charged and polled, by the rule the family already has

`Inst::RunCopy`'s doc comment states the convention and the reason:

> The copy is made in bounded chunks with a safepoint between them, and that
> is not a refinement of the charge but the thing that makes it sound. A charge
> taken only *after* an arbitrarily large copy would let one instruction run
> arbitrarily far past a fuel or cancellation bound before anything looked,
> which ADR 0040's `S + T` forbids.

`RunFind` is held to it. Concretely:

**Two answers are reached before any of this, and charge no bulk work.** They
are stated first because everything below is undefined for them — there is no
needle to prepare when it is empty, and nothing to search when it does not
fit:

- **an empty needle answers `from` immediately**, before any preparation of
  the needle begins;
- **a needle longer than `haystack_len - from` answers `-1` immediately**,
  having examined nothing. `from == haystack_len` with a non-empty needle is
  this case.

Neither charges any bulk work, because neither examines a unit. The
instruction's own single unit of fuel — the one every dispatched instruction
costs — is unchanged, so a fast path is one fuel and nothing else. That is the
same accounting `endsWith`'s length refusal already has, where 85 of its 1,159
covefmt calls read zero bytes and were charged zero work.

Otherwise the rule is stated over **work done**, not over a window size, and
it binds **both** phases of the search. An earlier draft made the window
`max(SAFEPOINT_STRIDE, m)`, which defeats the whole convention exactly where
it is most needed: a needle of ten million bytes made one window ten million
units of uninterruptible work, and the preparation of that needle was
uninterruptible before the first window even began. Both are corrected here.

- **The needle is prepared in bounded steps.** Whatever preparation the
  algorithm needs is `O(m)` and is *not* done in one go: it processes at most
  `SAFEPOINT_STRIDE` units, charges them, polls, and resumes where it stopped.
  Its progress is therefore state the instruction can hold across a safepoint.
- **The search consumes the haystack in bounded steps**, by the same rule:
  at most `SAFEPOINT_STRIDE` units consumed, charged, polled, resumed. The
  matcher's position in the haystack and its own internal state are held
  across the poll.
- **So the uninterruptible span is `SAFEPOINT_STRIDE` units, always**, whatever
  `n` and `m` are and whichever phase the instruction is in. That is the
  property ADR 0040's `S + T` asks for, and the one the window rule lost.
- **Work is charged as it is consumed**, in the same coordinate `RunCopy` is
  charged in — a word is the unit of work whatever the unit of the run is —
  and it is charged for *both* phases: preparing an `m`-byte needle is `m`
  units of work and is paid for, not free because it happens before the
  search.

**The instruction is therefore a resumable matcher, and that is a real
constraint on the implementation.** It rules out calling `str::find` once,
which reports neither where it stopped nor anything to resume from; and it
rules out re-searching overlapping windows, which the earlier draft needed
only because a window re-entered the algorithm from scratch. A matcher that
can stop between any two units and continue needs no overlap at all, so the
double charge the earlier draft levied on it is gone with it.

**`O(n + m)` is preserved and is a requirement, not a hope.** Each unit of the
needle is prepared once and each unit of the haystack is consumed a bounded
number of times. The two-way algorithm — which is what `str::find`, and
therefore the intrinsic this replaces, already uses — has all three properties
this decision needs: linear time, **`O(1)` auxiliary space**, and a state
small enough to carry across a poll. An implementation that instead wants a
table proportional to the needle may have one, but then the allocation is
stated in the pull request and charged, because Decision 8 gates on
allocations and a search that quietly allocates `m` words is not what the
intrinsic it replaces did.

`GrowableAlloc` and `RunFinish` are the family's exceptions — not chunked,
because their bulk work is inside an allocator's zeroing and inside one
`from_utf8`, "neither of which this could interrupt, and charging an operation
that cannot be interrupted only makes its overshoot visible rather than
bounded". **A search is not in that position.** The loop is ours, so it is
interruptible, so it is interrupted.

### 5. Both code generators lower it, in the pull request that adds it

A native **refusal is not an acceptable outcome**, and this is sharper than a
gate item because of how a refusal propagates: the native tier refuses a
*function* that contains an instruction it cannot lower, not just the
instruction. `std.string.contains` is expanded at its call sites, so a refused
`RunFind` would take **every caller** back to the VM — strictly worse than the
mediated intrinsic call it replaces, which crosses once and leaves its caller
compiled.

So: `template` and `cranelift` both lower it, in the same pull request, as
`RunCopy` and `RunSlice` are lowered — a direct call to a runtime helper,
which is a native-to-runtime crossing and **not** a native-to-VM one.

The gate is stated in counts, not in intent: **no new function refusal on
covefmt or cq, and `native→VM` unchanged on both.**

### 6. What belongs below the boundary — the rule, restated for the family

ADR 0064's trigger is narrowed rather than repealed, and stated so that it
covers `RunCopy` and `RunSlice` as well as this:

> **An operation belongs below the boundary when its work is proportional to
> the length of a run the caller has not bounded, and each unit of that work is
> a load and a comparison.** What the boundary buys is a dispatch per unit
> against one for the whole — seven instructions a byte in Cove, measured, and
> one instruction for a search of any length — and that is the same purchase
> `RunCopy` makes for a copy and `RunSlice` for a slice.
>
> **An operation does not belong below it when its work is bounded by an
> argument the caller already holds.** `startsWith` compares at most its
> prefix and `endsWith` at most its suffix. Both are 1.23x and 5.75x slower in
> Cove and both correctly stayed there.
>
> **And being proportional is necessary, not sufficient.** `String.length` is
> proportional to its run and is in Cove, at 9.33x on a 43-character string,
> because its runs are measured at **3.54 characters** with 40% of its calls on
> a single one. A migration claiming this rule still has to show the lengths
> the operation is actually called at.

A constant factor is not a licence, and neither is an unmeasured
proportionality. `contains` is the first operation in Phase 1 to meet all
three: proportional, unbounded by the caller, and measured at lengths where
the dispatch dominates.

## Measurement

Baseline `fb4d40a`, `--profile checked`, x86-64 macOS,
`--features cove-cli/template`. `benches/contains` — nine rows varying receiver
length, needle length and match position independently, with a bare-loop
control subtracted and three arms over the same strings in one process —
medians of interleaved sets, ns per call. The intrinsic column is the one
shipped in PR #446, after the operand copy was removed.

| row | receiver | needle | match at | intrinsic | Cove `byteAt` scan | ratio |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `empty` | 16 | 0 | — | 79 | 9 | **0.11x** |
| `needleLong` | 16 | 50 | refusal | 87 | 16 | **0.18x** |
| `start` | 88 | 6 | 0 | 112 | 228 | 2.0x |
| `middle` | 88 | 6 | 50 | 114 | 3,187 | 28x |
| `end` | 88 | 6 | 82 | 113 | 5,078 | 45x |
| `noMatch` | 88 | 6 | — | 151 | 4,729 | 31x |
| `shortHit` | 16 | 6 | 0 | 90 | 229 | 2.5x |
| `shortMiss` | 16 | 6 | — | 119 | 650 | 5.5x |
| `longNeedle` | 88 | 50 | 0 | 255 | 1,563 | 6.1x |

The two rows the Cove loop wins are the two where it compares **no bytes at
all** — an empty needle and a needle longer than the receiver, both answered
from two header loads where the intrinsic pays a call.

**Read this table for the constant, not for the growth.** The `middle` and
`end` rows grow because the body measured is the naive scan any standard
library would actually write; a Cove KMP would flatten them to roughly
`88 × 30 ns ≈ 2,600 ns` against the `noMatch` row's measured **151**, which is
the ≈17x Decision 6 rests on.

Fitted, the Cove scan is **≈48 ns + 57 ns per start position + 30 ns per byte
compared** on the VM, and ≈4 + 10 + 7.25 on native.

The copy-free intrinsic fits **≈28 ns + 0.07 ns per receiver byte + 3.0 ns per
needle byte** on the VM, 28 + 0.08 + 2.89 on native. **Those are regression
coefficients over the nine rows and not the cost of examining one byte** — a
two-way search skips most of a haystack, so what the receiver term measures is
how the whole operation varies with receiver length, not what a byte costs.
They are reported because the receiver term read **1.13 ns/byte before
PR #446** and 0.07 after: that difference *was* the operand copy, and it is the
one thing the coefficient is evidence for. No per-unit cost below the boundary
is claimed anywhere in this ADR; the argument is dispatch counts and the
directly measured `noMatch` row.

The 57 ns per start position is also why a bounded run **equality** primitive
would not do: on the `noMatch` row's 83 start positions, 83 × 57 = **4,731 ns**
against the row's measured **4,729**. Essentially the whole cost is the turn,
not the comparison inside it, so collapsing the comparison removes nothing. It
is the loop that has to be below the boundary.

Two further figures bound what this instruction may cost, and are the adoption
gate's rather than this section's claims:

- an `Inst::IntrinsicCall`'s own fixed cost, measured as the copy-free
  intrinsic's intercept, is **46–49 ns and the same on both tiers** — ADR
  0064's central claim survives PR #446 intact. An instruction the VM
  dispatches directly should be well under that; if it is not, the case for
  this migration is only that the native tier can compile the code around it;
- covefmt reaches `contains` **891** times and cq **80**, on short strings. No
  whole-program wall-time movement is expected, claimable, or measurable: this
  repository's rebuild noise floor is **at least ±2.9%**, measured on cq, whose
  every count was byte-identical across two builds and which still lost 15 of
  15 paired runs.

## Consequences

**The IR gains an instruction, and instructions do not leave.** `Intrinsic`
shrinks by one when `String.contains` migrates and by one more when
`String.indexOf` does, and ADR 0064's ratchet is untouched — but the run family
grows by a member every backend, the verifier, the printer and the encoder
carry from here on. Paid once for an operation `indexOf`, `split` and `replace`
all need.

**Decision 4 costs the implementation its simplest form.** A resumable matcher
is more code than one `str::find`, it carries state across a poll, and it will
be marginally slower than an uninterruptible call — and the needle's
preparation has to be resumable too, which is the part an implementer will not
think of unless it is written down. That is the price of a bound that holds at
every needle length rather than at convenient ones, and ADR 0040 does not offer
the alternative.

**Decision 5 costs the pull request both code generators.** The instruction
cannot land in one and be finished in the next, because a refused instruction
refuses its caller.

**`String.indexOf` rides on this decision and is not separately justified.** It
answers a *character* position, so its Cove body is this instruction plus a
walk counting characters before the match — and it runs zero times on both
benchmark programs, so no whole-program measurement can gate it either. It
inherits Decision 6's argument and nothing more.

Against those: the one operation in Phase 1 whose Cove form would pay a
dispatch per byte of a haystack nobody sized no longer has to, `Intrinsic` can
keep shrinking, and the rule that stopped three earlier migrations from
reaching for a primitive is stated more sharply, and in terms that explain the
run family it was always implicitly about.

## Adoption

One pull request adds the instruction, lowers it in both code generators, and
migrates `String.contains` onto it — a primitive with no caller cannot be
measured. `String.indexOf` follows in its own.

Gated on ADR 0064's Decision 8 as every Phase 1 migration is — public API
unchanged, AST and both native generators agreeing, allocations and allocated
words not regressing, and neither covefmt nor cq regressing past a control —
plus three of this ADR's own:

1. **No new function refusal on either program, and `native→VM` unchanged.**
   Decision 5, in counts.
2. **The bound and the fast paths are tested, not asserted.** In particular,
   with a needle **much longer than `SAFEPOINT_STRIDE`** and a small fuel
   bound:

   - the run stops **during the needle's preparation**, within
     `SAFEPOINT_STRIDE` units of where the bound fell — the case the window
     rule got wrong, and the reason it was rewritten;
   - with fuel enough to finish preparing but not to search, it stops
     **during the first stretch of the search**, within the same bound;
   - the work charged for a search that runs to the end of the haystack is
     `m + (n - from)` units, so preparation is charged and no unit is charged
     twice.

   And, independently of needle size: a match found across the point at which a
   poll happened to fall is still found; a cancellation and a deadline stop the
   run the way a fuel bound does; and **an empty needle and an over-long needle
   answer while charging no bulk work**, on a haystack large enough that
   examining it would show in the charge.
3. **The instruction's dispatch cost is measured against the 46–49 ns
   `IntrinsicCall` floor.** If it lands at the floor, the pull request says so,
   and the migration's case is the native tier's alone.
