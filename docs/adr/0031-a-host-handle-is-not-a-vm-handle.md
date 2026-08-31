# ADR 0031: A host handle is not a VM handle, and a trait method is a public signature

- Status: Accepted
- Date: 2026-08-31
- Supersedes:
  [ADR 0028](0028-five-representations-and-one-is-public.md)'s **visibility
  rule** — the one sentence under decision 0 that begins "No public signature
  in this workspace mentions", and the claim attached to it in the same
  paragraph and repeated in ADR 0028's Consequences that the rule "is
  checkable — it is a `grep` over `pub fn`". Nothing else in ADR 0028 is
  replaced. Its five representations, all eight of its decisions including the
  seal and `ValueView`, decision 8's three multiplicities, its resolution of
  the tension [issue #195](https://github.com/myuon/cove/issues/195) left, and
  its Costs all stand exactly as written; "What ADR 0028 keeps", below, goes
  through them one at a time, because a reader of a superseding ADR needs to
  know what was *not* replaced as much as what was
- Records: [issue #203](https://github.com/myuon/cove/issues/203)'s second
  finding. Its first and third findings are deliberately **not** decided here;
  "What is not superseded, and is not a decision" says why

## Context

### The rule is load-bearing, and two of its words are wrong

ADR 0028 exists to make one sentence of
[issue #197](https://github.com/myuon/cove/issues/197) true by construction
rather than by care: *changing the VM's internal representation must not
require exposing that representation to embedders.* Four of its five
representations are private to `cove-runtime` and the fifth, `Value`, is what
a host is handed. The thing that turns that table from an intention into a
fact is the rule under it:

> **No public signature in this workspace mentions a `Slot`, a `HeapObject`,
> a `Dynamic`, a layout id, a witness, a handle, a frame base, or a tag.**
>
> That is checkable — it is a `grep` over `pub fn` — and it is the difference
> between "the representations are allowed to differ" and "the representations
> cannot fail to differ".

It is a good rule and this ADR does not weaken it. It is wrong in two places,
and both of them are load-bearing rather than cosmetic: one of them makes the
rule forbid something ADR 0028 itself writes, and the other makes the check
that enforces it blind to a whole category of public signature.

### "Handle" is two different things, and the ADR bans one while writing the other

Decision 7's `ValueView` sketch — ADR 0028's own text — contains

```rust
Resource(&'a ResourceHandle),
```

A handle. In a public signature. Written by the ADR that forbids handles in
public signatures.

Both halves are wanted, which is what makes this a wording problem rather than
a contradiction to be resolved by dropping one of them.
[ADR 0013](0013-host-resource-handles.md) decided that a resource handle **is
a name** — the name of something the *host* owns, where "every field of it is
part of the name" — and that `Value::Resource(Arc<ResourceHandle>)` is the
whole of the runtime's representation of one. A host is *supposed* to be
handed one; that is the entire mechanism by which a host tells its own
resources apart. The rule's "handle", meanwhile, is decision 2's handle: the
one-word reference into storage the VM allocated and owns, which today is
`slot::Handle`, a `pub(crate) struct Handle(u32)`, and which must never be
public because it is an index whose meaning is the VM's current arrangement of
its own heap.

The two senses share a noun and nothing else. One is a name the host minted
and the VM merely carries; the other is a coordinate in VM-owned storage. The
rule, read literally, bans both.

### A trait method is a public signature that never says `pub`

The second error is in the enforcement claim. "A `grep` over `pub fn`" cannot
see a trait method, because a trait method carries no visibility of its own —
it is exactly as public as the trait, and the trait says `pub` on a different
line.

This was not theoretical. `Callable::call_value` was added and then reshaped
by [PR #201](https://github.com/myuon/cove/pull/201) to take `&mut Vec<Value>`
while the check ran on every commit and said nothing, because a `pub fn` scan
had no reason to look inside `pub trait Callable`.
[PR #202](https://github.com/myuon/cove/pull/202)'s commit `80649d8` extended
the scan to read `pub trait` blocks as well, and the first thing it turned up
was `HostApi::call_resource` — a signature taking `&ResourceHandle` that had
been public since ADR 0013 and that the `pub fn` scan had never once seen. The
trait half of a method whose inherent half the check already knew about.

### What that costs today

`crates/cove-runtime/tests/representation_is_private.rs` is the rule, run by
`cargo test`. It works, and it works with a seven-entry allowlist of exact
signature strings: six of them `ResourceHandle`, one of them
`std::thread::JoinHandle` on `Task::running`, each with a sentence saying why
it is not a representation.

Every one of those seven entries exists because of the two words above. None
of them records a decision; they record the same decision seven times, in a
form that has to be re-typed whenever a parameter moves — which is exactly
what will happen the next time `call_resource` changes, and did happen when it
gained its `back: &mut dyn Reentry` argument. A list of exceptions that long
is a rule that was stated slightly wrong, and the maintenance is the interest
payment.

## Decision

### 1. The rule, restated

> **No public signature in `cove-runtime` names a slot, a heap object, a
> dynamic value, a witness, a layout id, a frame base, a tag, or a handle into
> storage the VM owns.**
>
> A **VM heap handle** is a reference — an index, an offset, a one-word token
> — into storage `cove-runtime` allocated and manages. It is part of one of
> the four private representations and is never public.
>
> A **handle the VM does not own** is not one of these and the rule does not
> reach it. There are two kinds: a **host resource handle**
> ([ADR 0013](0013-host-resource-handles.md)), which is a name the host gave
> to something the host owns and which decision 7 hands to an embedder in
> `ValueView::Resource`; and a handle belonging to the **standard library**,
> such as the `std::thread::JoinHandle` by which
> [ADR 0008](0008-concurrent-task-execution.md)'s spawned task is joined.
>
> A **public signature** is any signature an embedder can name. That is every
> `pub fn`, *and* every method declared by a `pub trait`, which carries no
> `pub` of its own. It is not `pub(crate) fn`, and it is not anything compiled
> only under `cfg(test)`.

The scope narrows from "this workspace" to `cove-runtime`, which is not a
change of substance but a correction of the same kind: ADR 0028's own
visibility column places representations 2, 3 and 4 inside `cove-runtime` and
`Value` at its boundary, so `cove-runtime`'s public surface is the only place
the rule can bite. `cove-ir` names slots publicly and always has, deliberately
— [ADR 0019](0019-executable-ir-and-vm.md)'s "Slots, not names" and
[ADR 0027](0027-a-place-and-a-capture-name-a-slot.md)'s places are the
*lowering's* vocabulary, decided before ADR 0028 and untouched by it, and a
lowered program is not something an embedder is handed a piece of. The check
has read only `cove-runtime` since it was written; this states why rather than
changing what it does.

### 2. The check names the category, and the allowlist goes to zero

The restated rule is enforceable without listing a single exception, because
the distinction it draws is one the source already carries.

- The seven-entry list is deleted. All seven.
- The forbidden vocabulary keeps `slot`, `heapobject`, `dynamic`, `witness`,
  `layout`, `frame_base` and `tag` as blunt case-insensitive matches, exactly
  as before. A name close enough to be caught is a name close enough to want a
  sentence about why it is allowed.
- `handle` stays in the vocabulary but is resolved rather than matched. A
  handle-named identifier in a public signature is permitted when it is a type
  `cove-runtime` declares `pub`, or a type the file imports from `std`; a
  lowercase binding such as `handle:` is permitted when a permitted handle
  type appears in the same signature, because that is what it is bound to.
  Everything else fails — `pub fn handle_of(&self) -> u32` above all, the
  untyped leak that no type checker would catch and that is the reason the
  word is in the vocabulary at all.
- One invariant closes the loop: **the only handle-named type `cove-runtime`
  declares `pub` is `ResourceHandle`.** The test asserts that set exactly.
  `Handle`, `HandleRoots`, `HandleCollection` and `HandleHeap` are all
  `pub(crate)`, which is decision 0's visibility column already holding, and
  the day one of them is published the check says so at the declaration rather
  than waiting for a signature to carry it out.

The "exact in both directions" property the old list had — it failed both when
an unlisted signature appeared *and* when a listed one disappeared, so that an
exception which quietly stops applying is not an exception nobody re-reads —
is kept, at the level of the category rather than of the signature. The test
asserts that each of the two permitted categories is still exercised by at
least one public signature. If the last `ResourceHandle` or the last
`JoinHandle` leaves the public surface, the category has stopped being needed
and somebody is made to re-read it.

### 3. The `JoinHandle` exception is not irreducible, and neither is the rest

Issue #203 leaves open whether the `std::thread::JoinHandle` entry has to
survive as an exception. It does not. It is irreducible as an *exception* and
entirely reducible as a *category*: nothing the standard library declares is
one of ADR 0028's five representations, because all five are types this
workspace defines. "A handle the standard library owns" is a clause in the
rule, and it costs no list. This ADR prefers that shape wherever it applies —
name the category, do not enumerate the members — and the seven entries
disappearing is the whole of the evidence that these two categories are the
right ones.

## What ADR 0028 keeps

Everything except the sentence named above. Stated one at a time, because this
ADR is easy to mistake for a re-litigation and it is not one:

- **The five representations and decision 0's table.** Cove value semantics,
  `Slot`, `HeapObject`, `Dynamic`, `Value`, and the visibility column that
  makes exactly one of them public. This ADR restates the rule that *enforces*
  that column; the column itself is unchanged, and clause 1 above is the same
  claim said precisely.
- **Decisions 1 through 4.** Eight-byte untagged slots whose kind comes from
  metadata, a heap object as a handle plus a header, a sixteen-byte
  `(witness, payload)` dynamic value, reflection that reads metadata and never
  bits.
- **Decision 5.** `Value` is materialized at the boundary, the boundary list
  is closed, the VM must not materialize a `Value` to execute an instruction,
  and the interpreter is the deliberate and permanent exception.
- **Decision 6, the seal.** All twenty-two variants, including the scalars,
  and the argument for why a partial seal is worse than none — which is itself
  an argument *from* the visibility rule, and survives the rule being restated
  because the restatement does not narrow what it forbids.
- **Decision 7, `ValueView`.** Exhaustive on purpose, not
  `#[non_exhaustive]`, changing when the language gains a kind of value and
  not when the runtime changes how one is stored — including
  `Resource(&'a ResourceHandle)`, which this ADR makes legal by saying what
  ADR 0028 meant rather than by carving out what it wrote.
- **Decision 8, what the collector is owed**, and all three of its
  multiplicities.
- **The resolution of the tension #195 left.** The readers keep their names
  and the borrow-shaped ones keep their shapes.
- **The Alternatives, the Costs, and "What is not decided here"**, unchanged,
  including every measurement the prototype still owes.

## What is not superseded, and is not a decision

Issue #203 records two further findings. Neither is superseded here, and the
reason is the same for both: **an ADR supersedes a decision, and neither of
these is one.** Recording them as decisions would make this ADR a grab-bag and
would spend the supersession mechanism on facts that are not rules anybody
checks.

**Three constructor names.** ADR 0028's decision 6 lists `Value::resource`,
`Value::range` and `Value::type_name` among the constructors sealing requires.
All three names were already taken by readers #195 had added and ADR 0028's own
resolution of the borrow-reader tension explicitly kept, so Rust forbids the
collision and they shipped as `from_resource`, `range_of` and `type_value`.
That is a naming slip and not a declined decision: what decision 6 *decided*
is that scalar and opaque constructors must exist — without which the seal
makes `Value` unbuildable — and that was followed exactly. The names were
illustrative, and they are wrong in the way a sketch is wrong rather than in
the way a decision is wrong. Issue #203 is the record, and it is linked from
here so that a reader who greps ADR 0028 for `Value::range`, finds nothing,
and comes looking has somewhere to arrive.

**The migration estimate.** ADR 0028's Costs section says "about sixty-six
mentions across fifteen variants" outside `cove-runtime`. The recount during
[#196](https://github.com/myuon/cove/issues/196)'s implementation was 135
across 11, plus 58 sites in `crates/cove-runtime/tests/` that the estimate did
not consider, and `cove-ir` — one of the three crates the estimate names — had
**zero** code sites, every mention there being prose. So the number was about
2× low and wrong about where the work was. It stays as written, and this is
the clearest case in the whole issue for why: a Costs section is a record of
what the project believed a thing would cost *at the moment it committed to
it*, and correcting the belief afterwards destroys the only evidence that the
estimate was made by grep and should not have been trusted. A future reader
weighing a similar seal is better served by an estimate visibly off by 2× and
an issue saying so than by a number quietly made right.

The substantive question ADR 0028's design rested on — whether any site would
turn out to be *inexpressible* through the sealed API — was answered in the
ADR's favour. None was, and nothing was widened to make anything compile.

## Costs

**A second ADR to read before the rule is understood.** ADR 0028's decision 0
still contains the sentence this replaces, and it will keep containing it,
because an accepted ADR is immutable. A reader arriving at the wrong sentence
gets one header line pointing here. That is the mechanism working as intended,
and it is still a cost.

**The check does more than grep.** It resolves handle-named identifiers
against the crate's own `pub` declarations and its `std` imports, which is
more machinery than a word list and is machinery that can itself be wrong. It
is bounded — it reads source text and resolves nothing the compiler would
resolve — and the invariant in clause 2 is what keeps the resolution from
being load-bearing in the dangerous direction.

**The lowercase-binding permission is approximate.** A signature naming two
handles, one permitted and one not, passes the binding clause. The unpermitted
one would have to be a private type in a public signature, which the compiler
already refuses, so the gap is closed elsewhere rather than left open — but it
is closed elsewhere, and this ADR would rather say so than claim the clause is
exact.

## Consequences

- The sentence #197 calls its thesis stays checkable, and the check now covers
  trait methods, which is where much of this crate's public surface actually
  is: `Callable`, `HostApi`, `Reentry`.
- `ValueView::Resource(&'a ResourceHandle)` is legal by the rule rather than
  by exception, which is what ADR 0028 meant when it wrote it.
- The check's failure message stops offering "add it to `ALLOWED` with a
  sentence that says why" as a way out. There is no allowlist to add to. A
  public signature that names a representation is a signature to change, or a
  rule to supersede again.
- Nothing in `cove-runtime` changes. This ADR describes a rule and the test
  that runs it; no runtime type, signature or behaviour moves.
