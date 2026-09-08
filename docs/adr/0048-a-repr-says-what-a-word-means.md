# ADR 0048: A `Repr` says what a word means, not what class of word it is

- Status: Accepted
- Date: 2026-09-08
- Decides: that an enum's case index is its own `Repr`, and what that costs —
  for [issue #297](https://github.com/myuon/cove/issues/297)
- Supersedes nothing. [ADR 0034](0034-one-physical-word-stack.md) decided the
  word is untagged and its meaning lives in static metadata; this decides what
  that metadata is allowed to say, and changes nothing about the word

## Context

`Repr` is the per-slot metadata ADR 0034 put the word's meaning in. An enum's
discriminant was `Repr::Int`, and the reason was physical: the bits are an
integer's bits in a non-reference word.

That made two facts one. **Occupying the same kind of word and having integer
semantics are different things**, and the second is the one a static check has
to read. While they shared a name, three things were true and none of them was
noticed by anything:

- `add.int` accepted an enum's case index, as did ordering and integer
  equality, because a tag *was* an `Int` to the verifier;
- nothing bounded the number written into a discriminant against the enum it
  was supposed to name — `Inst::Int { dst, value }` writes any `i64` anywhere;
- the listing said `int s4:int 0`, so which case that was could only be found
  by counting an enum's declaration.

None of it is caught at run time — the machine adds two words either way — so
the static side was the only place any of it could be caught.

The distinction was not made on principle. **The verifier found two places the
IR was already treating a case index as a number**, and it found them by
refusing to compile a lowering that had been correct for as long as a
discriminant was an `Int`.

`?` asked which case its value held as integer equality: materialise the index
into a slot, compare the discriminant against it, branch on the answer. Three
instructions and two temporaries spelling one dispatch. So did the test a
nested case pattern makes — `pattern::test_case`, whose doc comment said
plainly what it was doing: *"the one test an enum case needs: word 0 against
the index."*

Both are now one `Inst::Switch` and no temporary. Neither was rewritten to
satisfy a rule; each was written that way *because* the type allowed it, and
the type no longer does.

## Decision

**An enum's case index is `Repr::Tag`**, written by an instruction of its own
and named by an id of its own.

```rust
Repr::Tag                                  // one non-reference word, semantically not an integer
Inst::Tag { dst: Slot, layout: LayoutId, case: CaseId }
CaseId(pub u32)                            // a case's position in its enum
```

`Inst::Tag` is the only way a discriminant is written. `Inst::Switch` is the
only way one is read. Copying and clearing take a layout rather than a `Repr`
and are unchanged, so an enum is copied and zeroed exactly as it was.

Everything else refuses it by not naming it: `Inst::Arith`, `Inst::Cmp` and
their immediate forms expect `Int`, `Float` or `Duration`, and a `Tag` is
none of those.

### What the physical side does not gain

**No new word class.** A `Tag` is one non-reference word, encoded as it was.
`Repr::is_ref` answers `false` for it as it does for `Int`, so the
collector's question and its answer are untouched and a tag is never a root.

**No runtime value and no dispatch path.** `Inst::Tag` encodes to an opcode
of its own — so the distinction survives into the bytecode, where the
byte-level verifier can check it — and that opcode is answered by the *same
arm* of the dispatch loop that answers `Inst::FuncRef`:

```rust
FUNC_REF | CONST_TAG => machine.mem.set_slot(base, a!(), held.lo() as u64),
```

Both write one metadata number out of the payload's low half. The loop has no
way to tell a case index from a callee id and needs none.

**No wider frame.** A slot is still reused only by a run of exactly the same
`Repr`s, which is what keeps one static reference bitmap correct at every
program counter. Splitting `Int` could have cost frame width — and does not,
because **a discriminant is always word 0 of an enum-shaped run and never a
standalone integer temporary**, so the two were never candidates for the same
slot. That is a measurement rather than an argument: every golden lowering listing in this
repository changed `s1:int` to `s1:tag`, and **not one changed its
`frame N:`**.

## What was rejected

**A separate `IrType` layer beside `Repr`.** The issue left the shape open and
this was the other candidate: keep `Repr` physical and add a semantic type
above it. It costs two per-slot tables that have to agree, and the frame
allocator keys reuse on the `Repr` run, so it would have had to key on both to
stay correct — which is the same split with an extra table. A variant is
smaller and the exhaustive matches on `Repr` made the compiler produce the
list of places to think about.

**A `Compare::Tag`, or an `Inst::TagEq`, for the sites that compared a tag to
an integer.** Both add an opcode family for an operation that turned out not
to be needed at all; see below.

## What it exposed, which was not its own

Reordering the layout table found a latent bug that had nothing to do with
tags, and it is recorded here because the *shape* of it is the same one this
ADR is about — a fact that was inferred where it was already known.

`vm::builtins::make` built a builtin's `Result` or `Option` answer by
**searching** the layout table for an enum of that name whose carrying case
holds the right payload. That cannot tell `Result<String, Error>` from
`Result<String, cq.diag.Detail>`: both are named `Result` and both carry a
`String` in `Ok`. What differs is their *width* — two words against four —
so answering the wrong one is not a wrong discriminant but a word run written
into a destination sized for the other, off the end of a frame if the
destination is near the top of one.

It had been latent for as long as both existed. Adding one layout to the table
changed which of the two the search reached first, and `examples/cq`'s own test
suite crashed the VM.

**`Inst::CallBuiltin` has carried the answer's layout all along.** The machine
now records it on entry to every builtin and `make` reads it instead of
searching, which is exact and cannot be ambiguous. The search is still there
for the callers that declare no enum.

## Consequences

**`Inst::Switch` accepts a `Tag` or an `Int`.** A `dyn` dispatch switches on a
layout id, which is the other metadata-like integer this IR carries and has
not been given a `Repr` of its own. That is a second consumer for a separate
change, not an exception to this one, and the same is true of a function id.

**There is a `<tag>` word layout, and it makes every `dyn` switch table one
entry wider.** A nested enum's discriminant is a payload word of the enum
around it — `Option<Option<Int>>` — so zeroing a payload region has to be able
to name the layout of a tag word. A `dyn` switch has one target per layout, so
one more layout is one more target. It is four bytes per `dyn` dispatch site
and it is the price of the tag being a real `Repr` rather than a comment.

**A listing names the case.** `tag s4:tag Result.Ok`, not `int s4:int 0`, for
the reason `Inst::FuncRef` prints its callee's name: a case declared before
this one changes its index, and a listing that printed the number changed with
it.

## What it measured

One build — this change against its parent, `--release`, one machine —
following [ADR 0029](0029-a-benchmark-number-is-evidence-within-one-build.md).

| | instructions before | after |
| --- | ---: | ---: |
| `cq revenue-summary`, 100,000 records | 1,169,365,142 | **1,159,565,118** |
| `crates/cove-runtime/tests/encoded.rs`'s `arith` | 14,285,738 | 14,285,736 |
| `benches/chars` | 58,240,025 | 58,240,023 |

**Nine point eight million fewer instructions on `cq`**, which is the `?` and
the case tests it runs per record, and exactly two fewer on each of the others,
which is the single `?` in a `main` that runs once.

**Wall time did not move.** `cq` measured 18.18 s and 18.10 s against 17.90 s,
18.00 s and 18.08 s before — the same, within a spread that was already 1%.
`allocated_words` is identical to the digit, which it should be: no allocation
changed.

That is worth stating plainly rather than rounding into a win. A `switch` is a
table read and an unpredictable branch where a compare-and-branch is two
predictable ones, so removing two instructions per `?` bought fewer
instructions and no time. **The reason to make the change is the check, not
the count** — and the count is here so that a later reading of this ADR does
not have to guess whether it was a speedup.
