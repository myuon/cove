# ADR 0054: A comparison that only feeds a branch is the branch

- Status: Accepted
- Date: 2026-09-13
- Decides: that the instruction set gains a fused compare-and-branch, and what
  it may not cost
- Supersedes nothing. It extends
  [ADR 0041](0041-a-slot-number-fits-in-sixteen-bits.md)'s encoding with a
  family that uses the payload word `Inst::Cmp` leaves empty, and changes
  nothing about the sixteen-byte instruction or the sixteen-bit slot

## Context

`examples/covefmt` executes 593,116,164 instructions in 3.9 seconds, and the
mix says where they go:

| opcode | executions | share |
|---|---:|---:|
| `branch-false` | 156,483,109 | **26.4%** |
| `tag` | 46,694,292 | 7.9% |
| `jump` | 41,841,822 | 7.1% |
| `clear` | 39,255,938 | 6.6% |
| `eq.int.imm` | 34,377,450 | 5.8% |
| `copy` | 31,434,136 | 5.3% |

Control flow is a third of the run, and one opcode is a quarter of it. That is
not a surprise once it is written down: a comparison in this instruction set
answers a `Bool` into a slot, and almost every comparison a program writes is
the condition of an `if` or a `while`, so the slot is read once by the
`branch-false` on the very next line and never again.

Counting the pairs rather than assuming them — every instruction of the run,
with its execution count, matched against the one before it:

```text
fusible pairs (static sites)        : 1,040
instructions removed at run time    : 111,485,146
  as a share of all instructions    : 18.8%
adjacent but the branch is a target : 27,036,101 (cannot fuse)
```

The 27 million are the ones worth naming. A `branch-false` can itself be the
target of a `jump` from somewhere else — the second arm of an `else if`, a
loop's continue — and then the branch must remain reachable on its own. A
first count that missed this attributed more executions to `eq.int.imm` than
`eq.int.imm` has, which is how the mistake announced itself.

## Decision

The instruction set gains a fused form of each comparison, which **writes its
`Bool` exactly as the comparison does and then branches on it**.

```text
cmp-branch      on, op, dst, a, b, target
cmp-imm-branch      op, dst, a, value, target
```

The fused instruction is *semantically the two it replaces*, in order, with no
condition attached. That is the whole of its correctness argument and it is
chosen over the alternative deliberately: an instruction that skipped the
write would be sound only where nothing reads `dst` afterwards, which is a
liveness question, and the saving is the dispatch rather than the store. Not
asking the question costs one slot write on the paths where the answer would
have been "nothing reads it" and buys a rule with no side conditions.

### The encoding does not move

`Inst::Cmp` is three slots with an empty payload word, so `target` goes in the
payload and the instruction stays sixteen bytes.

`Inst::CmpImm` is two slots and spends its payload on an `i64`. The fused form
packs the immediate and the target as two halves, so **the immediate is an
`i32` there**. A lowering that meets a wider one emits the unfused pair, which
is the existing instructions doing what they already do. The immediates this
was measured against are character codes and small bounds — 10, 32, 97, 122 —
and a fusion that silently dropped the high bits would be a wrong answer
rather than a slow one, so the narrowing is checked and not assumed.

### It is a peephole over lowered code, not a decision the lowering makes

The pair is recognised after lowering, where the branch targets are already
known: a `Cmp` or `CmpImm` at `pc`, a `BranchFalse` at `pc + 1` reading the
slot the comparison wrote, and `pc + 1` named by no jump, branch or switch in
the function. `super::dropping`'s delete-and-renumber is what removes the
branch and fixes every target that moved, which is machinery two passes
already share.

Recognising it later rather than emitting it earlier keeps every lowering that
produces a comparison — `if`, `while`, `&&`, a `match` guard — unchanged, and
means a form the peephole cannot see is a missed fusion rather than a wrong
one.

## What this costs

**Forty-two opcodes.** `Cmp` is a family of thirty-six, `CmpImm` of six, and
the fused forms mirror both rather than covering the subset one benchmark
happens to execute. That is a 36% growth of the dispatch table, and ADR 0051
named exactly this as a cost with measured precedent — so the gate on this
change is that `examples/covefmt` is *faster*, not merely that it executes
fewer instructions. Four opcodes measured free; forty-two is a different
question and it is asked rather than assumed.

**A second place a comparison can be.** A reader of the IR, the debugger, the
verifier and the profiler all now see two shapes where they saw one. The
profile in particular stops showing `branch-false` at 26% and starts showing
the fusion, which is a truer picture and a less familiar one.

## Alternatives considered

### One fused opcode with the comparison in the payload

Keeps the table at 118 and puts `on` and `op` beside the target. It trades a
dispatch for an inner branch on every execution, which is most of what the
fusion was buying.

### Fuse only the forms `covefmt` executes

Eight opcodes would carry 111 of the 111 million. It is also the instruction
set describing one program: `Compare::Float` and `Compare::Identity` are the
same shape and the same cost, and their absence would be a fact about a
formatter rather than about Cove.

### Drop the `Bool` write when nothing reads it

A liveness question for a saving that is not the dispatch. It can be added
later as a second form if a measurement asks for it; it cannot be removed
later if it turns out to be wrong.

### Leave it and inline more instead

Measured first, and it is the smaller half by an order of magnitude: every
call and return in the run is 13.1 million instructions, so inlining
*everything* — recursion and all, which is impossible — is a 2.1% ceiling
against this 18.8%.

## Consequences

- A comparison that feeds only the branch beside it costs one dispatch.
- The `Bool` is still written, so nothing downstream needs to know whether it
  is read.
- A `branch-false` that is a jump target keeps its own instruction, and there
  are 27 million such executions.
- The immediate form carries an `i32`; a wider immediate is left unfused.
- The change is accepted only on a measured wall-clock improvement to
  `examples/covefmt`, reported with the instruction count beside it, because
  forty-two opcodes are a cost this repository has measured before.
