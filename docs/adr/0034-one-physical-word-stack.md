# ADR 0034: Cove runtime values live in one linear memory

- Status: Accepted
- Date: 2026-09-01
- Supersedes:
  [ADR 0027](0027-a-place-and-a-capture-name-a-slot.md),
  [ADR 0028](0028-five-representations-and-one-is-public.md), and
  [ADR 0033](0033-an-identity-is-not-a-vm-heap-object.md). Their surviving
  requirements are restated here; implementations must not reconstruct their
  parallel stacks, runtime Place objects, representation taxonomy or identity
  stores by treating the older texts as concurrently binding
- Decides: issue #162's physical realization and replaces the representation
  decisions previously spread across issues #197 and #218
- Implementation status: not implemented. The current executable IR, Vm and
  FrameVm are predecessor implementations and evidence, not foundations that
  the replacement must preserve. The new backend may initially keep stack and
  heap in separate Rust allocations, but its IR and address model target one
  linear memory from the start

## Context

ADR 0027 gave places and captures one logical slot identity while retaining
three physical stacks. It explicitly did not decide whether those stacks
should become one physical frame.

ADR 0028 then decided the representation contract:

- a slot is eight bytes and untagged;
- its meaning comes from lowered layout metadata;
- a function has one logical slot numbering and one frame base;
- heap-backed values occupy a slot as handles;
- a genuinely erased Dynamic may occupy two adjacent slots.

It deliberately left the physical arrangement to measurement and permitted a
split realization if every physical offset was derived from the one logical
layout.

That uncertainty has now served its purpose. FrameVm has exercised a
contiguous word stack with a reference bitmap across scalar calls, mixed
arguments, places, strings, host calls, structs, enums, payloads and collection
under stress. The work found correctness gaps in lowering and metadata, not a
reason to preserve three physical stores. The declaration-order calling
convention and per-slot layout are now facts in cove_ir.

Keeping the physical choice open has acquired a cost of its own. It allows the
production Vm's value/scalar/place stores and the experimental FrameVm to grow
as parallel execution models. New language features can then be added by
expanding the experiment's admission predicate one refusal at a time, without
ever making the production representation converge. The open question is now
blocking the migration it was intended to protect.

The project's performance rule is also narrower than "choose the fastest
measured layout at any cost": performance is balanced with semantics,
convenience and maintainability. A small local regression, on the order of
five percent, does not justify retaining two execution architectures. An
order-of-magnitude regression, or a repeated multi-fold regression on
representative programs, does require the decision to be revisited. Noisy
cross-build wall-clock differences below that threshold do not keep the
physical representation permanently undecided.

## Decision

### One linear memory owns Cove runtime values

The final production runtime has one VM-owned linear-memory block. Within that
one address space it manages two regions:

1. a stack region of eight-byte words;
2. a heap region containing variable-sized, escaping, shared or otherwise
   indirect objects.

These are allocation disciplines inside one memory, not independent value
stores or handle universes. Scalars live directly in stack or object words.
Strings, closures and other compound values occupy the heap region and words
hold addresses or offsets into that same linear memory.

The first production migration may keep the stack and heap regions in separate
Rust allocations where combining them immediately would add risk. That is a
temporary implementation state, not an architecture choice. Address encoding,
lowered layouts, GC maps and public APIs must not expose which backing
allocation currently contains a region, so the regions can later be placed in
one block without another representation migration.

A new Cove value kind must use one of these regions. Identity, mutability,
variable size, boundary crossing, rooting convenience or an incomplete
migration do not justify an identity store, boundary value store, side heap or
other third value store.

Static type/layout tables, instruction metadata, GC mark bits and allocator
bookkeeping describe or manage values but do not store general Cove values.
Scheduler control structures and Host-owned resource registries are not Cove
value stores and must not become fallback storage for ordinary values.

Strings, closures, structs, enums, arrays, maps, sets, vectors and Shared cells
live in the VM object heap when represented by the production VM. Observable
identity changes whether materialisation may copy an object; it does not
change which heap owns the object.

Adding another runtime value store or handle universe requires a later ADR
that explicitly supersedes this rule and demonstrates why the one linear
memory cannot implement the required semantics.

### One stack region

The production VM stores frames and operands in one contiguous stack region of
eight-byte words.

A running task has one physical word stack. Every call frame has one
frame_base. Parameters, locals, temporaries and captures share the one slot
numbering already carried by cove_ir::Function.

A slot is addressed as:

~~~text
word_stack[frame_base + slot]
~~~

There is no independently based value stack, scalar stack or place stack in
the production representation.

### The calling convention names destination slots

Parameters occupy slots 0..arity in declaration order. The caller evaluates
arguments in the language-defined order and writes each result to the
callee's declared destination slot. A mixed list such as (Int, String, Int)
is not permuted into type regions.

Return placement, receivers, captures and variadic arguments are described by
the same lowered function/call metadata. They must not depend on an implicit
parallel-stack ordering.

### Words are interpreted by metadata, not tags

The word itself remains untagged. The instruction and the function's
per-slot/per-value layout determine whether it contains scalar bits, a VM heap
handle, a stable host handle, a place representation, or one word of a
multi-slot value.

The collector derives frame roots from the lowered reference map. It must
never infer roots by inspecting word bits. Temporary handles held outside the
stack follow ADR 0028's explicit temporary-root discipline.

A place remains ADR 0027's root plus path. Its root names a slot in the one
numbering; it does not name one of several physical stacks. A place is not
itself an additional GC root.

### A place is an address, not a store or object

A place remains a compiler concept for an assignable expression. It is not a
third runtime representation. Lowering turns a place into a one-word logical
address into the stack or VM heap.

The production runtime has no place stack, no allocated root-plus-path object
and no table of places. Field and element operations compute an address from a
base address or heap handle plus lowered offsets. A mutable parameter carries
that address in an ordinary eight-byte slot. Stack-address escape and
invalidation by growable collections are rejected or constrained by the type
and borrowing rules; they are not repaired by allocating a Place object.

The concrete address encoding is private runtime detail. It must be compatible
with one VM linear address space. During the temporary two-allocation phase an
internal decoder may distinguish the regions, but no Cove value, IR layout or
public API may preserve that distinction.

### Heap-backed values and the boundary

A struct, string, array, VM-owned enum or other VM-owned heap-backed value is a
handle in one word, under ADR 0028 and the ownership corrections of ADRs 0031
and 0033.

The materialized public Value remains the host/oracle boundary representation.
A Vec<Value> may exist transiently while entering or leaving that boundary,
but it is not an internal operand stack, call buffer, spill area or fallback
execution path. VM execution converts at explicit boundaries and otherwise
operates on words and VM heap objects.

Mutable Vector or Shared identity crossing a Host boundary must not be
implemented by moving the value to another store. A later boundary decision
may reject that crossing, expose an immutable Array snapshot, or define an
explicit rooted borrow or handle; the ordinary runtime storage remains the VM
heap in every case.

### Replace the execution backend; do not renovate it

The production implementation is a clean replacement of the executable IR,
lowering and VM backend. It is not a continuation of FrameVm's admission
experiment and does not preserve the old IR for compatibility.

The replacement keeps:

- the lexer, parser and source AST;
- name resolution, type checking and the checked semantic facts;
- the tree-walking interpreter as the semantic oracle;
- the public Host API and materialised Value boundary;
- the conformance corpus, error/span expectations and trace semantics;
- the language contracts for capability, fuel and cancellation.

The replacement does not inherit:

- value/scalar/place regions or independent frame sizes and bases;
- ValueToScalar, ScalarToValue, StoreScalar, PlaceLocal, PlaceScalar or other
  instructions whose purpose is crossing predecessor representations;
- runtime Place root/path objects;
- an internal Vec<Value> operand, argument, spill or boundary store;
- FrameVm's admits predicate or partial-backend fallback;
- duplicate dispatch loops, heaps or GC root models;
- one ValueKind case or special side table per corpus refusal.

The new lowering starts from checked source facts and emits an executable IR
that directly describes typed words, layouts, addresses, allocations, calls
and control flow in the memory model decided here. Unsupported language
features are implementation work in the replacement; they are not represented
by a permanent admission predicate.

The old and new execution backends may coexist only during the bounded
replacement period needed for differential testing. New language features and
optimisations are implemented in the replacement rather than added to both
backends.

The replacement is complete only when:

1. the new executable IR contains no predecessor storage regions or conversion
   instructions;
2. the new VM executes frames and operands on one word-stack region and stores
   every Cove-owned indirect value in its one heap region;
3. compiler places lower to one-word addresses and no runtime Place storage
   remains;
4. ordinary Cove-to-Cove execution never materialises or stores a public
   Value;
5. Vector, Shared and every other Cove-owned object use the VM heap rather
   than an identity or side store;
6. the full differential corpus agrees with the tree-walking oracle, including
   values, errors, source spans and trace events;
7. GC stress, recursion, closures, mutable references, collections, tasks,
   cancellation, Host reentry and fuel contracts pass on the replacement;
8. the replacement becomes the production path and the predecessor executable
   IR, Vm, FrameVm, admits mechanism, duplicate heap and migration machinery
   are deleted;
9. representative performance gates show no order-of-magnitude or repeated
   multi-fold regression;
10. temporary separate backing allocations for stack and heap are recorded as
    the remaining step toward the final single linear-memory block, and
    nothing in IR or public APIs depends on that separation.

Coverage and profiling may reveal missing semantic facts. They guide the
replacement implementation but do not justify further refusal-specific
extensions to the predecessor backend.

## Consequences

- The production frame has the direct and inspectable meaning promised by ADR
  0028: one base plus one slot index.
- Mixed scalar/reference/place arguments require no type-region permutation.
- Every live frame word costs eight bytes; metadata and heap headers carry the
  descriptions that no longer travel in each slot.
- GC correctness depends on precise lowered reference maps and explicit
  temporary roots. The tests built for FrameVm become production invariants.
- The old multi-store implementation and FrameVm cannot remain as permanent
  alternatives. Maintaining both would violate this decision even if both are
  correct.
- Small benchmark movement is accepted as the price of one maintainable
  execution architecture. Material semantic or usability compromises are not
  made to recover the last few percent.
- If representative workloads regress by an order of magnitude or repeatedly
  by several times, this ADR must be revisited with the workload, instruction
  counts and profiling evidence recorded. Physical splitting is not
  reintroduced for ordinary benchmark noise.

## What this does not decide

- when the stack and heap regions move from temporary separate backing
  allocations into the final single memory block;
- how the one block grows and whether stack and heap grow toward one another;
- the concrete heap allocator or garbage-collection algorithm;
- the exact Host API for passing or borrowing mutable Vector and Shared values;
- a moving collector;
- the final layout of every enum, Option or Result;
- whether the dispatch loop is later replaced by threaded dispatch, JIT or
  native code;
- whether a multi-slot value other than Dynamic is profitable.

Those choices must preserve the one physical word stack and the lowered layout
contract unless a later ADR explicitly supersedes this one.
