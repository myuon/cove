# ADR 0070: A call whose continuation is doomed may be expanded

- Status: Accepted
- Date: 2026-09-29
- Decides: that the inliner may expand a body holding a call, when every path
  on from that call ends in a trap without returning and without going round a
  loop; how that property is computed; that the size limits weigh such a body
  by every instruction it holds; what keeps the pass terminating; and that
  `std.stringbuilder`'s byte-range refusal is therefore a Cove call and not an
  intrinsic. Recorded for
  [issue #432](https://github.com/myuon/cove/issues/432)
- Supersedes: [ADR 0062](0062-an-append-is-ensure-store-commit.md)'s
  **"each refusal calls `core.refuseByteRange`, an `IntrinsicCall` that always
  raises — an intrinsic rather than a Cove call, so the body remains an
  inlinable leaf"**, and nothing else it decided. The range policy stays in
  Cove, in `appendRange`, in `sliceBytes`' order and shape, and the copy beneath
  it stays a write already proved legal; what changes is how a refusal is
  spelled and why the body is still expanded
- Refers to, without superseding:
  [ADR 0067](0067-a-trap-carries-the-sentence-it-was-handed.md), which gave a
  Cove body `core.refuse` and left ADR 0062's decision standing — "the
  intrinsic stays until a migration measures its replacement". This is that
  migration and that measurement.
  [ADR 0064](0064-an-intrinsic-names-a-machine-not-a-method.md)'s Decision 2,
  which `String.refuseByteRange` failed — its only purpose was a sentence that
  quotes a range — and Decision 8, whose gates the measurement below is held
  to

## Context

ADR 0062 moved `StringBuilder.appendSlice`'s range policy out of the copy and
into `appendRange`: five questions in Cove, then an append window a backend
fuses. The window is worth something only where the method is expanded, and
the inliner expanded only **leaves** — bodies that call nothing — because a
leaf cannot reach its own caller, which is what makes the pass terminate with
no call graph. So each refusal had to be something other than a call:
`core.refuseByteRange`, an intrinsic whose five sentences were written out in
Rust twice, beside the Cove copy `std.string`'s `refuseRange` already had.

ADR 0067 gave Cove a way to stop a run with a sentence it built, so the
sentences could move. Moving them makes each refusal
`core.refuse(byteRangeRefusalMessage(text, from, to), "", "")` — and an
interpolation is calls, so `appendRange` stops being a leaf, `appendSlice`
stops being expanded, and `examples/covefmt`, which appends a range 483,660
times a run, pays a frame for each. Measured as the direct migration on its own
(the second of this change's commits): **+1,539,942 executed instructions
(+0.09%) and +968,304 calls on the VM, +1,936,608 native helper calls**, and
the fused window moved out of the printer into a library frame.

Nothing about those half-million calls runs the refusal. The helper is reached
only on a path that is about to stop the run, and a body is refused expansion
for a call that never happens on any call that succeeds.

## Decision

### 1. The property is the continuation's, not the callee's

A call may stand in a body the inliner expands when its **continuation is
doomed**: every path on from the instruction after it ends in an
`Inst::Trap`, without a `Return` and without a backward edge. The callee is
not asked anything. `byteRangeRefusalMessage` returns — it builds a `String`
and answers it — and it is what the caller does next that stops the run. A
rule phrased as "the callee always traps" would be about the wrong function,
and would not admit this one.

`doomed(pc)` is one pass from the last instruction to the first, over forward
edges only:

- a `Trap` is doomed;
- a `Return` is not, and neither is an instruction with a backward edge;
- any other instruction is doomed when every instruction it may go to next is,
  provided it reaches nothing (the leaf rule's own test) or is itself an
  `Inst::Call`.

A call at `pc` has a doomed continuation when `doomed(pc + 1)` holds, it does
not call the function it stands in, and none of its arguments is an address.
`return refuseRange(...)`, `std.string.sliceBytes`' shape, is not one: the
callee's answer is the caller's.

### 2. Every instruction is weighed

`LIMIT`, `HOT_LIMIT` and `LOOP_LIMIT` count the instructions on doomed paths
like any others. They do not run, but an expansion copies them into every site,
and code size is what the limits are for. The mandatory expansion of a thin
standard-library wrapper still asks for a strict leaf: a body expanded past
every limit is one whose whole body is the operation it wraps.

This bound, and not the rule, is what `appendRange` had to be fitted to, and
the limits were not raised to fit it. The inliner weighs a body before the
passes that tidy it, and there the five refusals were 20 instructions more
than their source: `core.refuse` released its three temporaries with a `clear`
each *after* the trap, where nothing runs, and spelled `""` twice. A lowering
change that lands before this rule, and is measured on its own below, makes
`core.refuse` emit nothing after its trap and name one slot for a `rule` and a
`help` that are the same literal. It is a general improvement to every refusal
the standard library words, not a part of this rule.

As measured on 2026-09-29, that left `appendRange` at 45 instructions where the
hot site in `appendSlice` admits 48, and the expanded method at 47 where
`covefmt`'s `spacing` admits 48. That headroom is small, and it is written
down as a dated fact rather than held by a test: what the tests hold is that
both bodies are expanded at both of covefmt's shapes of site, and that the path
a legal range takes calls nothing. The next refusal added to `appendRange` is
likely to cost the expansion, and those tests are where that will show.

### 3. Expansion still terminates without a call graph

A body with such a call can reach its caller at run time, in frames of its
own; that is an ordinary call. What has to terminate is *expansion*, and two
rules keep it so: a call of the body itself is never one of these, and **a
call that stands inside an earlier expansion is never expanded.** A body is
copied into a caller once per site, and the call it brought stays a call. So
the pass's `Inlined` records still never nest one expansion it made inside
another, and a diagnostic raised through the helper's frame is blamed exactly
as before: the record puts back the library's lines and the caller's site.

### 4. `hot_functions` is unchanged

A call with a doomed continuation still carries hotness to its callee. Not
carrying it across the edges this change creates — the calls an expansion
brings with it — changes no generated code in any program in the repository.
Not carrying it across every such edge, including the ones that already
existed (`std.dynamic`'s `unordered`, `std.float`'s and `std.int`'s refusal
helpers), shrinks 43 of 263 programs by 2,043 IR instructions in all. That is
a question about code the refusal paths already had, and it is **a follow-up,
not part of this decision**: `hot_functions` is left exactly as it was.

### 5. The sentence is one Cove function

`std.stringbuilder.byteRangeRefusalMessage(text, from, to) -> String` is the
one copy of the five sentences. `appendRange` raises it and
`std.string.refuseRange` answers it as an `Err`, so a builder's append and a
string's slice cannot word one range differently; the oracle's and the
machine's Rust copies are deleted with the intrinsic. Which question fails is
still decided by each caller's own order — below the start, backwards, past the
end, then each boundary — and which sentence a range gets by the helper's —
`from` outside, `to` outside, backwards, then each boundary — exactly as
before.

## Measurement

`before` is `c08653d`; the direct migration, the `core.refuse` lowering change
and this rule are measured as separate builds, each `--profile checked
--features template` in its own target directory, over one pristine tree from
`examples/` with `--files-root ..`.

**The lowering change on its own** moves counts and nothing that runs on a
path that succeeds. covefmt: IR 11,591 → 11,586 (the second `str ""` gone at
each of `appendRange`'s five refusals), machine code 823,698 → 823,593 bytes,
executed instructions identical (1,685,076,243 on the VM). cq: IR 8,347 →
8,349, functions 89 → 88, unexpanded standard-library calls 70 → 69, machine
code 748,943 → 748,654 bytes, executed instructions identical (257,229,581),
because `std.string.replace` is now small enough to be expanded into
`cq.csv.formatField`. Across the corpus it changes 28 of 263 programs and
removes 76 IR instructions.

| covefmt | before | direct migration | with the lowering change and this rule |
| --- | ---: | ---: | ---: |
| emitted IR / functions | 11,594 / 109 | 11,591 / 112 | 11,617 / 110 |
| `IntrinsicCall` sites | 10 | 0 | 0 |
| executed instructions (VM) | 1,683,536,301 | 1,685,076,243 | **1,683,536,301** |
| unexpanded std `Call` sites | 57 | 66 | 68 |
| Cove calls (native) | 21,416,510 | 22,384,814 | 21,416,510 |
| runtime helper calls (native) | 52,650,694 | 54,587,302 | 52,650,694 |
| compiled / refused (native) | 107 / 2 | 110 / 2 | 108 / 2 |
| machine code | 824,593 B | 823,698 B | 827,668 B |
| allocations / words | 8,150,928 / 104,053,486 | same | same |
| frame words, `flattened` / `spacing` | 69 / 46 | 60 / 37 | 70 / 47 |

Fifteen interleaved rounds after a discarded cold one, paired deltas against
`before`, medians:

| row | direct migration | with this rule |
| --- | ---: | ---: |
| covefmt native `whole` | +1.08% (3 wins of 15) | −0.43% (10 of 15) |
| covefmt native `execute=` | +1.40% (1 of 15) | +0.04% (7 of 15) |
| covefmt VM `whole` | +2.55% (1 of 15) | +2.08% (1 of 15) |
| covefmt VM `lex` | +3.55% | +2.84% |
| cq VM `execute=` | −1.14% | −0.49% |
| cq native `execute=` | +0.72% | +0.56% |

**The covefmt VM rows are over its ±1% floor, and that residual is accepted
with its cause not established.** It is consistent with the sensitivity to
global binary layout that ADR 0064's Decision 8 warns of — ADR 0063 measured
−2.13% between two builds whose every count was identical — and the evidence
for that reading is this: with this rule covefmt's VM executes the same
1,683,536,301 instructions in the same dispatches, fused windows and declines
as `before`, and the `lex` phase, which never calls `appendSlice`, moved
further (+2.84%) than the whole run did (+2.08%). Consistent with is not
shown to be: no layout experiment was run, and none is planned for this
change. The native tier is within noise.

cq is unchanged in executed instructions on both tiers (IR +11, machine code
+994 B against `before`, most of it the lowering change's expansion of
`std.string.replace` into `cq.csv.formatField`).
`benches/stringlib` executes 6 more instructions (7,159 → 7,165) and 3 more
calls: its three `sliceBytes` refusals now call the shared helper, which is
Decision 5's cost and paid only on the path that answers `Err`. `join`,
`split_rows`, `slice`, `trimwords` and `bytescan` have identical IR, machine
code and counts on both tiers.

## Consequences

- `Intrinsic::StringRefuseByteRange` is deleted with its schema, lowering,
  oracle arm, machine arm and its module; two variants are left.
- Blame is unchanged in everything but the quoted standard-library line and
  its caret, on the interpreter, the VM and the native tier alike; five
  end-to-end cases and the native tier's edge rows pin it.
- Any function whose only calls have doomed continuations is now expandable.
  In the repository's corpus that newly expands `appendRange` and
  `appendSlice` (`covefmt`'s `flattened` and `spacing`), `std.map.of` into
  `benches/keyed`'s `ofFive`, and `lower::synth`'s `describes` walks into
  `std.set.of` and `std.set.contains` over some key layouts. A thin wrapper a
  refusal path calls — `std.dynamic.notAKey`, and `std.string.replace`'s
  `replaceRefused`, `emptyNeedleRule` and `replaceHelp` in `cq` — can now be
  left a call inside one of those expansions, on the path that stops the run.
- Follow-ups: whether `hot_functions` should skip a doomed call (Decision 4);
  the dead instructions other forms still leave after a trap — an `if` arm's
  `jump` to its join, a `let` scope's `clear`, a `Unit` function's `return` —
  which the lowering change did not touch; and `std.string.refuseRange`'s one
  copy of the helper's answer into its `Error`, which is now a copy after a
  producer and so one more of the forwardable copies `copies.rs` counts in
  every program.
