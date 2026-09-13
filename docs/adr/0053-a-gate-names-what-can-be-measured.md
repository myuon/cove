# ADR 0053: A gate names what can be measured, not what was hoped for

- Status: Accepted
- Date: 2026-09-13
- Decides: the implementation gate for
  [ADR 0052](0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md)'s
  byte builder, now that the one it named is known to be unreachable
- Supersedes: [ADR 0052](0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md)'s
  implementation gate, and nothing else it decided. Its storage architecture,
  its append-only API, its packing rules and its stop-bound requirements all
  stand as written

## Context

ADR 0052 named its own gate:

> The implementation gate is `examples/covefmt` rewritten to the builder, with
> identical output and all formatter ratchets passing.

That gate cannot be run, and the reason is not a shortcoming of the
implementation. **`covefmt`'s output is not an append-only sink.** Two things
in `examples/covefmt/print.cove` read it back or unwrite it:

- `column` walks the written pieces backwards to find the last newline, which
  is how every line-break decision is made. It runs 79,980 times.
- `trimTrailing` pops pieces off the end while they are only whitespace, from
  four call sites.

ADR 0052 is deliberate that the builder has neither: "it does not gain
indexing, insertion, removal, sorting or general collection algorithms merely
because Vector has them." So the gate asks for a rewrite the decision it gates
forbids.

### Giving the builder those operations was measured, not assumed

The obvious repair is `byteAt` and `truncate`, and carrying the column
alongside the builder rather than reading it back. The arithmetic refuses it:

| | |
|---|---:|
| `column` calls | 79,980 |
| pieces it scans, total | 170,660 |
| ⇒ pieces per call | **2.1** |
| writes into the same output | **410,949** |

`column` stops at the last newline, so it is close to constant work — newlines
are frequent and it never walks a line's history. Maintaining the column on
every write is 410,949 measurements against 170,660: **2.4 times the work**, to
replace something already cheap. The read-back is not a cost the builder would
remove; it is a cost the builder would *create*, by turning a 2.1-piece scan
into a byte-wise backward walk.

`column` is 2.48% of the run's instructions in total, so even deleting it
outright is a small ceiling.

### What the builder did measure

The construction sites that *are* append-only were converted, and the result
is the reason this ADR exists rather than a second attempt at the gate:

| | allocations | wall clock |
|---|---|---|
| `String.join`/`sliceBytes` copying words natively | unchanged | 4.185 s → 4.05 s |
| ADR 0051's byte-run vocabulary | unchanged | no change |
| `tokenWidth`, 610 K allocations removed | −15.5% | 4.06 s → 4.00 s |
| `spacing` and `flattened` on the builder | **−11%** | **no change** |

Eleven per cent of every allocation in the run, and the wall clock did not
move. Four measurements say the same thing: **this program's time is not string
allocation.** It is instructions — around 590 million of them for 3.9 seconds.

## Decision

ADR 0052's implementation gate is replaced by:

> The gate is the construction sites that can use the builder, converted, with
> wall time, instruction count, allocations and allocated words reported
> against the pre-builder main. A site whose output is read back or unwritten
> is not one of them, and is evidence about the builder's shape rather than a
> failure to finish.

Everything else ADR 0052 decided is untouched. The builder keeps its
append-only API and gains no `byteAt` and no `truncate`; `examples/covefmt`'s
`emit` keeps its `Vector<String>`.

## What this costs

**The ADR no longer proves what it hoped to prove.** ADR 0052 was motivated by
`covefmt`'s largest join, and the gate was how that motivation would be
checked. Restating the gate is not making the check easier — it is admitting
the check cannot be run and recording what was learned instead, which is that
the motivation was mismeasured. The builder is sound, it works through
recursive `var` calls, and it is used; it is not where the time was.

**A gate that can be restated is a weaker gate.** The protection against
restating a gate whenever it is inconvenient is that this one is superseded
with its measurements attached and with the decision it gates left standing.
An implementation that simply had not reached its gate would not be entitled
to this.

## Alternatives considered

### Add `byteAt` and `truncate` and run the gate as written

Measured above and rejected: it makes the formatter slower, and it spends
exactly the API restraint ADR 0052 chose on purpose.

### Restructure the formatter so its output is append-only

`column`'s own doc comment records that carrying the number was considered and
rejected — "a third parameter for a number the output already holds is a third
thing to keep in step" — and the corpus of 246 files reproduces byte for byte
today. Rewriting the layout engine to satisfy a gate is the gate deciding the
program, which is backwards.

### Leave the gate unmet and say nothing

Then ADR 0052 stands with a gate nobody can run and no record of why, and the
next reader repeats the whole measurement to find out.

## Consequences

- ADR 0052's builder is judged by the sites it can serve, and it passes.
- `examples/covefmt`'s `emit` keeps its `Vector<String>` indefinitely, unless
  something other than this ADR gives a reason to change it.
- The next place to look for `covefmt`'s time is the instruction count itself
  — inlining, superinstructions, and what the dispatch loop costs per
  instruction — and not another representation for its strings.
