# ADR 0034: The production VM has one physical word stack

- Status: Accepted
- Date: 2026-09-01
- Supersedes in part:
  [ADR 0028](0028-five-representations-and-one-is-public.md), only where
  decision 1 leaves the physical arrangement open and permits physically split
  regions. All other decisions in ADR 0028 remain in force
- Decides:
  [ADR 0027](0027-a-place-and-a-capture-name-a-slot.md)'s open item
  "A single physical frame" and issue #162's physical realization
- Implementation status: the experimental FrameVm demonstrates the chosen
  representation, but the production Vm has not yet migrated. This ADR is
  the authority to perform that migration and remove the transitional
  backends and stacks

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

### One physical stack

The production VM stores frames and operands in one contiguous stack of
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

### Heap-backed values and the boundary

A struct, string, array, VM-owned enum or other VM-owned heap-backed value is a
handle in one word, under ADR 0028 and the ownership corrections of ADRs 0031
and 0033.

The materialized public Value remains the host/oracle boundary representation.
A Vec<Value> may exist while entering or leaving that boundary, but it is not
an internal operand stack, call buffer, spill area or fallback execution path.
VM execution converts at explicit boundaries and otherwise operates on words
and VM heap objects.

### Migration completes rather than adding a backend

The existing FrameVm is a migration vehicle, not a third permanent backend.
Implementation proceeds by moving its word-stack machinery into the
production Vm, not by completing two dispatch loops independently.

The migration is complete only when:

1. production execution uses the one word stack for frames and operands;
2. the old independent value/scalar/place stacks and their bases are removed;
3. internal boundary Vec<Value> storage is limited to explicit host/oracle
   materialization and is absent from ordinary Cove-to-Cove execution;
4. the experimental FrameVm, duplicate dispatch loop and admission fallback
   are removed or folded into the production implementation;
5. the differential corpus, GC stress tests, recursion, closures, mutable
   places, tasks, cancellation, host reentry, tracing and runtime errors pass
   on the production path;
6. the existing representative performance gates show no order-of-magnitude
   or repeated multi-fold regression.

Coverage measurements may identify missing lowering or layout facts. They do
not justify an indefinite sequence of refusal-specific experimental backends:
facts shared by a family of values are represented generally in IR and layout
metadata.

## Consequences

- The production frame has the direct and inspectable meaning promised by ADR
  0028: one base plus one slot index.
- Mixed scalar/reference/place arguments require no type-region permutation.
- Every live frame word costs eight bytes; metadata and heap headers carry the
  descriptions that no longer travel in each slot.
- GC correctness depends on precise lowered reference maps and explicit
  temporary roots. The tests built for FrameVm become production invariants.
- The old three-store implementation and FrameVm cannot remain as permanent
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

- the concrete heap allocator or garbage-collection algorithm;
- a moving collector;
- the final layout of every enum, Option or Result;
- whether the dispatch loop is later replaced by threaded dispatch, JIT or
  native code;
- whether a multi-slot value other than Dynamic is profitable.

Those choices must preserve the one physical word stack and the lowered layout
contract unless a later ADR explicitly supersedes this one.
