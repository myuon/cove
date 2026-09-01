# ADR 0035: A value type may not contain itself

- Status: Accepted
- Date: 2026-09-01
- Supersedes in part: [ADR 0001](0001-mvp-language-design.md)'s type-system
  decision, which admits nominal structs and enums without saying whether one
  may contain itself by value
- Decides: the language rule that
  [ADR 0034](0034-one-physical-word-stack.md)'s value model needs, raised in
  [issue #240](https://github.com/myuon/cove/issues/240)

## Context

ADR 0001 decides two things that meet here. Its type system admits nominal
structs and enums, and says nothing about recursion in a declaration. Its
copy rule says assignment and ordinary argument passing are field-wise
shallow copies, and that structs and enums have value semantics.

ADR 0034 then decides that a Cove value lives in one linear memory, and issue
#240 decides how a value-semantic type is represented in it: **a value is a
run of consecutive words, laid out where the value is.** A `Point` is two
words in the frame, not one word naming two words somewhere else. That is
what makes the shallow copy a copy — two words in, two words out — and it is
why no sharing bit or copy-on-write protocol is needed to keep an assignment
from becoming an alias.

A declaration that contains itself by value has no finite width under that
model:

~~~cove
struct Node {
  value: Int,
  next: Option<Node>,
}
~~~

Something has to give, and the choice is where the language's semantics come
from. Three answers were considered and rejected before this one.

**Insert a box where the cycle is found.** The layout computation notices the
recursion and represents `Node` as one address. It works, and it is what the
first implementation did — but it makes an ordinary assignment share
mutation, because copying the location copies the address. Whether
`b.value = 7` is visible through `a` would then depend on whether the type
happens to mention itself, which is not something ADR 0001 says and not
something a reader of the declaration could see. A representation would be
deciding the language's semantics.

**Deep-copy a boxed value on assignment.** Value semantics hold everywhere
and there is no exception to explain. But `var b = a` on a thousand-node list
allocates a thousand nodes, which turns an ordinary assignment into an
unbounded operation, and a cyclic structure has no terminating copy without a
visited map and alias-preservation machinery. ADR 0034's rule on performance
classes says a change of class must be argued for, and this one would be
hidden inside an assignment.

**Copy-on-write for boxed layouts only.** Smaller than the general version —
one family, one bit — but it is the mechanism issue #240 removed, and
reintroducing it for the case that provokes it is how a rejected design comes
back.

## Decision

**An implicitly recursive value layout is not allowed. A recursive cycle must
pass through a type whose values are a reference.**

A declaration whose layout would contain itself is rejected by the checker,
with a diagnostic that names the recursive edge and says what to do about it.
The rule is the checker's rather than a backend's, so that both execution
backends agree on which programs exist.

The types whose values are a single reference are the ones the language
already has: `String`, `Array`, `Map`, `Set`, `Vector` and `Shared`. A cycle
through any of them terminates, and every recursive declaration in the
conformance corpus already passes through one — `Node { peers: Vector<Node> }`
and `Json { Items(Array<Json>), Fields(Map<String, Json>) }` are both already
legal under this rule. Nothing that works stops working.

What this ADR does *not* decide is whether the language gains a reference
type written for the purpose, so that a linked list can be declared without
reaching for a collection. Issue #240 suggests one and leaves its name open;
this ADR deliberately does not settle it, because the rejection is what the
value model needs and the convenience is a separate question with its own
consequences for the type system, the Host boundary and the collector.

Three properties follow, and they are the point:

- every finite-width value has value semantics and copies by word-range copy;
- an explicit reference is one word and copies the reference, as its type
  says;
- no assignment performs an implicit deep copy, and whether mutation is
  shared never depends on an implementation discovering that a type happens
  to be recursive.

## Consequences

- The checker gains a layout-cycle analysis over struct and enum
  declarations, and one diagnostic. It runs on declarations rather than on
  bodies, so it costs nothing per call site.
- The diagnostic has to be good, because it is the only place a reader learns
  the rule. It names the edge that closes the cycle and the indirection that
  would open it.
- A program that wants a linked list writes the indirection. Until the
  language has a reference type of its own, that means a collection —
  `Vector<Node>` or `Array<Node>` — which is heavier than the declaration
  wants but is what the corpus already does.
- The lowering has no implicit boxing to do, so `Shape::Boxed` is left with
  one meaning: a value whose type was *intentionally* erased. Erasure and
  recursion no longer share a mechanism, which is one fewer place for a
  representation to be doing two jobs.
- A generic type is checked after instantiation, since `struct Cell<T> { it: T }`
  is only recursive for some `T`.

## What this does not decide

- whether Cove gains a reference type written for the purpose, and what it is
  called;
- whether an explicit deep-copy operation is ever offered;
- how the checker orders declarations for the analysis, which is an
  implementation choice.
