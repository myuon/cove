# ADR 0050: Two case indices compare as the words they are

- Status: Accepted
- Date: 2026-09-12
- Decides: that `==` and `!=` on an enum with no payload is one instruction,
  not a walk
- Supersedes: [ADR 0048](0048-a-repr-says-what-a-word-means.md)'s rejection of
  a `Compare::Tag`, and nothing else it decided

## Context

`a == b` on a value the instruction set cannot compare in one step lowers to a
call — `Any.equals`, which walks the two values word by word against their
layout. That is right for a struct, for a string, for an enum that carries a
payload: there is more than one word and what is in them decides.

An enum with **no** payload has one word and that word is the discriminant.
`Kind.Space == Kind.Word` is `1 == 2`. Walked, it is a builtin call, a
dispatch through `builtins::call`, and a recursion into `equal::value` that
reads a layout to find out there is nothing else to read.

Measured, 2,000,000 comparisons of a payload-free enum:

| | |
|---|---:|
| `k == Kind.Space` | 342 ms |
| `match k { Kind.Space => … }` | 210 ms |

The `match` asks the same question and does not call anything: `Inst::Switch`
reads the tag. The 132 ms between them is 66 ns a comparison, which is one
builtin call.

It is not a micro-benchmark's problem. `examples/covefmt` reads a token's kind
and a node's kind in every loop it has, and a native profile of it — `sample`,
because `cove --profile` counts instructions and says so — put two thirds of
the VM's dispatch inside `call_builtin` and 8% of the whole run inside
`equal::equals`.

## Decision

**A `Compare::Tag`, admitting `Repr::Tag` and nothing else.**

`Inst::Cmp { on: Compare::Tag, .. }` compares two words as words, which is
what the dispatch loop already does for a `Bool` and for `is` on a reference —
the same arm answers all three. Ordering is refused, as it is for those two.

**The lowering sends a payload-free enum there instead of to the walk.** The
guard that chooses between an instruction and a call gains one question:
whether the layout is an enum whose payload region is empty. Everything else
about the choice is unchanged, and an enum that carries anything still walks.

## What this costs

**Six opcodes.** `Inst::Cmp` is `Compare × CmpOp`, so a sixth `Compare` is six
more numbers: 103 to 109 of the 256 an opcode byte names.
[ADR 0041](0041-a-slot-number-fits-in-sixteen-bits.md)'s headroom argument is
what those numbers are for, and it is unchanged — more than half the byte is
still unspent.

**Nothing in the machine.** No new arm, no new word class, no change to what a
frame holds or to what the collector traces. A tag was already one
non-reference word and is still one.

## Why ADR 0048 rejected this, and why that does not decide it

ADR 0048 rejected *"a `Compare::Tag`, or an `Inst::TagEq`, **for the sites
that compared a tag to an integer**"*, on the ground that those sites turned
out not to be needed: making the case index its own `Repr` meant a switch read
it, and nothing compared it to a number any more.

That is a different operation. What is decided here is a tag against **another
tag**, which is what `==` between two enum values means and which ADR 0048 did
not weigh — enum equality went through `Any.equals` before it and went through
`Any.equals` after it, unexamined either way.

The artefact has the same name, so this supersedes the rejection rather than
arguing around it. What was rejected stays rejected: a `Compare::Tag` admits a
`Tag` on **both** sides, so the pairing ADR 0048 was protecting against — a
case index read as a number — is still refused by the verifier, in the same
place and for the same reason.

## What it measured

`cove fmt --check`'s work over this repository, done by `examples/covefmt`:

| | |
|---|---:|
| before | 1210 ms |
| after | 900 ms |

The micro-benchmark closes to the `match`: 342 ms to 216, against the `match`'s
203.

A program that does not compare enum cases sees nothing, and that is the
honest shape of it. `cove test` over `examples/` is 1.47 s either way, because
what it spends its time on is starting processes and compiling. The win is
proportional to how often a program asks which case a value holds, which for
anything compiler-shaped is constantly.
