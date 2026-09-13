# ADR 0053: Native execution compiles optimized IR one function at a time

- Status: Accepted
- Date: 2026-09-13
- Amends: [ADR 0012](0012-performance-gate-and-native-backend.md), whose throughput gate is now crossed by a representative Cove program and its native reference
- Extends: [ADR 0019](0019-executable-ir-and-vm.md), whose executable IR remains the input to execution, and [ADR 0022](0022-the-vm-is-the-default-backend.md), whose VM remains the portable execution path while native execution is introduced
- Preserves: [ADR 0040](0040-a-bound-outlives-its-backend.md)'s bounded stop, fuel, cancellation and Host-effect contracts
- Does not supersede: the tree-walking interpreter as the semantic oracle, or the VM as the default backend before this ADR's adoption gate passes

## Context

Cove has now spent two execution models finding the boundary between a local optimization and a different machine.

ADR 0019 replaced the tree walk with an executable IR and a dedicated VM. That was the right change: settled names became slots, calls became frames, and the VM became several times faster than the oracle. ADR 0022 made it the default only after the corpus agreed on both.

The same evidence now says the next boundary has been reached.

`examples/covefmt` is a representative Cove program over a real corpus, with a Rust implementation of the same formatter as its native reference. The recorded stage timing is 859 ms for Cove against about 60 ms for Rust, roughly 14x. The repeated end-to-end performance run used while optimizing its printer is about 3.9 s. The two measurements have different harness shapes and must not be divided into one invented ratio, but both say the same thing ADR 0012's throughput gate asks: execution is more than 10x from the native reference and the profiled CPU time is dominated by the execution mechanism rather than Host waits.

The VM dispatches roughly 590 million instructions in that repeated run. Dispatch alone has measured between 44% and 54% of CPU time. Removing intermediate strings and 11% of allocations moved wall time by approximately zero; making a builtin substantially faster bought only a few percent; carrying formatter state to avoid a scan performed more work and lost. These were useful experiments because they separate real local costs from the structural one. They do not add up to the order-of-magnitude change the target requires.

The performance target is approximately twice the native reference. Reaching 120 ms from a 3.9 s run would require about 32x from the current execution path. No plausible sequence of dispatch-loop constant-factor improvements promises that. Native execution is no longer a speculative alternative to an unmeasured VM. It is the next execution tier justified by the VM's own measurements.

Two questions must be answered together:

1. At what point in the pipeline is native code generated?
2. What happens to the inlining and instruction fusion already built for the VM?

The answer is not to discard the optimizer. Some transformations remove semantic work and help every backend. Others exist only to pay fewer interpreter dispatches and should remain private to that interpreter.

## Decision

Cove adds a baseline native execution tier which compiles **optimized executable IR one function at a time**.

```text
checked program
  -> typed executable IR
  -> target-aware common optimization
       |- VM legalization -> VM-only fusion -> encoded VM
       `- native legalization -> baseline native code
```

A function is the unit of compilation, caching and selection. A basic block, not a function, is the unit of bulk fuel accounting and safepoint placement.

The first code generator is Cranelift. The semantic boundary is Cove's IR and runtime ABI, not Cranelift's API: replacing the code generator must not change a Cove program. Cranelift is chosen for the first implementation because it supports fast baseline compilation and can later emit object code from the same lowering. This ADR does not make AOT the default and does not promise a serialized IR format.

### Native execution is a tier of the VM

The tree-walking interpreter remains the semantic oracle and is never a per-function fallback.

A native run may contain both native and encoded functions. Both consume the same verified Cove IR, use the same runtime objects and slot-frame ABI, and call the same Host, allocation and collection machinery. An uncompiled function is executed by the encoded VM; it is not reinterpreted from source or AST.

This is deliberately different from the silent fallback ADR 0019 forbids. That rule rejected a backend claiming conformance or performance for work an unrelated evaluator happened to finish. Here the encoded and native entries are two execution tiers below one lowering and one runtime. The run reports how many calls and functions used each tier, so a benchmark cannot present a mixed run as fully native.

Initially native execution is explicit. The VM remains the default until the adoption gate below passes. Platforms that cannot or do not permit executable memory continue to use the VM without losing language coverage.

### Compile functions, not traces

The first implementation does not have a hotness counter, on-stack replacement or deoptimization.

When native execution is selected, it may eagerly compile every supported reachable function or compile a function on its first call. That is an implementation and compile-latency choice; either way the cached unit is one function and code is immutable for the lifetime of its Program.

A later adaptive policy may choose functions from measured call or loop activity. It must not add a branch to every VM dispatch without a benchmark showing that branch pays for itself. It also must not change the function ABI or the meaning of a mixed run.

A function containing an operation the native lowering does not yet support runs entirely on the encoded VM. The initial implementation does not split one function into native and interpreted regions.

### One slot ABI joins both tiers

The first native ABI preserves the VM's frame as the canonical home of Cove values. Parameters, results and values live at their settled slot offsets. Native code may keep non-reference temporaries in registers between safepoints, but at a safepoint every live reference is discoverable through the existing frame/root machinery.

Calls first go through a function-entry table:

```text
Program + FunctionId -> encoded entry | native entry
```

That table permits VM-to-native, native-to-VM and native-to-native calls without changing Cove calling semantics. Direct native-to-native calls and more aggressive register allocation are later optimizations, not requirements of the first tier.

Runtime operations whose correctness already lives in Rust — allocation, collection, Host calls, strings, buffers, task operations and runtime errors — remain runtime helpers initially. Native code replaces dispatch and scalar execution before it duplicates mature runtime machinery.

### Optimization is shared by meaning, not by opcode shape

The optimizer is divided by why a transformation is valid.

Common, target-aware optimization runs before either execution representation:

- inlining;
- constant propagation and folding;
- dead code and dead value elimination;
- removal of redundant allocations, bounds checks and copies;
- construction followed by immediate projection;
- fusion such as a sliced range appended directly from its source;
- simplification exposed by settled types, effects, layouts and uniqueness.

These transformations remove calls, memory traffic, checks or semantic work. They remain valuable in native code. Cranelift is a baseline compiler and does not replace Cove transformations that depend on Cove's types, ownership, effects or library protocols.

The implementation of these passes is shared, but their policy may be selected by target. In particular, inlining has separate VM and native budgets. The VM values removal of dispatch and frame setup more aggressively; native code also prices code size and instruction-cache pressure. Tiny wrappers and calls that expose constant propagation remain good candidates for both.

VM-only lowering follows the common optimizer:

- superinstructions whose only purpose is to reduce dispatch;
- encoding-specific operand packing;
- peepholes justified only by the shape of the dispatch loop.

Native lowering does not manufacture a fused opcode merely to split it again. It emits the equivalent Cranelift operations directly. Conversely, a fused operation which removes a real allocation, traversal, check or copy is not “VM-only” merely because it is represented by one instruction today.

The current executable IR already contains slot and VM-shaped decisions. Introducing the native tier does not require an IR rewrite first. The initial lowering may consume today's `Inst` and explicitly lower its fused forms. When a fusion proves dispatch-only, it moves behind the VM branch. This is a migration discovered by implementation, not permission to build a second source of truth.

### Safepoints and work survive compilation

Native speed does not weaken runtime control.

The native tier preserves ADR 0040's order at every safepoint:

1. cancellation and task-local stops;
2. fuel and deadline accounting;
3. the collector rendezvous.

Native code does not update an atomic or call the Meter for every IR instruction. Lowering computes a work charge for a bounded run of IR and pays it at safepoints. Safepoints occur at least:

- on loop backedges;
- before Host effects;
- around allocation or runtime calls which may collect;
- at bounded intervals inside long straight-line code;
- between chunks of proportionally charged bulk work.

No compiled interval may exceed the backend's stated `T`. A long basic block is split or given internal polls so cancellation and fuel cannot be postponed by its source shape. Bulk runtime helpers keep their own bounded chunking.

Fuel remains backend-specific as ADR 0024 and ADR 0040 require. A native run does not promise the same `fuel_spent` as an encoded or AST run. It does promise the same stop outcome, no forbidden Host effect after a stop becomes visible, pending work charged on every exit, and a measured maximum overspend. The native row is added to `responsiveness.rs` before native execution is adopted.

### Collection uses the VM stack as the first root map

The first native tier does not require a machine-stack map or a moving collector.

Before a native safepoint, every live reference is materialized in its Cove slot frame and the frame's program counter is synchronized. The existing non-moving heap can then walk the same logical roots it walks for encoded execution. Native registers may contain duplicate stale copies across the call, but code reloads any reference whose validity depends on the safepoint.

This leaves optimization on the table and makes the first collector boundary auditable. Precise register stack maps may replace the spill discipline only under a later decision with differential GC tests.

### Executable memory is optional, not assumed

JIT code pages are never simultaneously writable and executable. The implementation writes through a writable mapping, finalizes it, and executes only a non-writable mapping according to the facilities of the code-generator runtime and operating system. Verified IR, checked bounds and the runtime ABI remain the security boundary; native compilation does not make unverified bytecode executable.

Where executable memory is prohibited or unavailable — including restricted mobile, browser or edge environments — Cove uses the encoded VM. Selecting native execution on an unsupported target produces a capability diagnostic, not an attempted fallback to the AST evaluator.

The same IR-to-native lowering may later support AOT object emission for such deployments. Whether `cove build` embeds JIT code, links an object, or keeps the VM is not decided here. No native-code cache is persisted across processes by this ADR.

### Debugging, tracing and profiling

Source-level Host traces and run outcomes remain available under native execution because native code uses the same runtime boundary.

Instruction stepping remains an encoded-VM facility initially. Asking for the interactive step debugger selects the VM explicitly and says so; it does not claim the run was native.

The existing opcode profiler measures dispatched opcodes. It must not silently label statically counted IR in native blocks as dispatched instructions. Until a native profiler exists, requesting opcode profiling with native execution is refused or explicitly runs the VM. Native runs do report wall time, compilation time, native/VM function and call counts, fuel, allocation and Host statistics.

Aggregated static IR-work counts may later be added for native blocks, but they are a different metric with a different name.

## Adoption gate

The first implementation is deliberately narrower than the decision.

It begins with scalar and control-flow functions sufficient for `benches/arith`:

- integer and Boolean constants and arithmetic;
- comparisons;
- slot reads, writes and copies;
- branches and jumps;
- calls and returns;
- runtime-error exits;
- block work accounting and safepoints.

It then adds the operations needed by `examples/covefmt`: object fields, enum tags and branches, allocation, Array access and calls to existing String and Buffer helpers.

Native execution may become the default only after all of the following hold:

1. Every native-capable corpus entry agrees with the VM and AST oracle on answer or error, console, Host effects, source trace and terminal outcome.
2. Tests exercise VM-to-native, native-to-VM and native-to-native calls, recursion, errors, cancellation, fuel exhaustion, GC at a compiled safepoint and task-local heaps.
3. `responsiveness.rs` states and measures the native tier's stop bounds.
4. A benchmark reports compile time separately from execution time and keeps a trivial warm run within ADR 0012's approximately 50 ms startup gate.
5. `arith` demonstrates that removing dispatch produces a structural rather than percent-scale gain.
6. `covefmt` runs the same corpus and reports wall time against both the encoded VM and the Rust reference. The long-term goal is approximately 2x the native reference; the first implementation must report the result honestly rather than make 2x a condition for landing the architecture.
7. A build without the native feature has no executable-memory dependency and retains the full VM corpus.

Making the native tier the default, changing `cove build` to AOT, or persisting native code each require the evidence above and a follow-up ADR.

## Alternatives considered

### Continue optimizing the dispatch loop

Rejected as the route to the performance target. Dispatch remains roughly half of profiled covefmt time after several measured improvements, and the target requires an order-of-magnitude change. Dispatch-loop work remains worthwhile when it is small and measured, because the VM is permanent portability and debug infrastructure; it is no longer the plan for reaching native-class throughput.

### Discard the current optimizer and let Cranelift optimize

Rejected. Cranelift does not know that a Cove operation is pure, that a builder is unique, that an enum construction is immediately projected, or that a slice may be copied directly from its source. It also cannot recover work hidden behind a runtime helper after lowering. Native code removes dispatch; it does not remove the need to present good work to the code generator.

### Send VM superinstructions to native lowering unchanged forever

Rejected as the architecture. A dispatch-only fusion is a compression format for an interpreter, not a language operation. Keeping it as the permanent common IR couples the native compiler to yesterday's dispatch loop and can hide optimization opportunities. Consuming today's forms during migration is allowed.

### Compile the AST or checked semantic graph directly

Rejected. It would duplicate the lowering that already settled frame layout, representation, calls and control flow, and it would give the native tier a different input from the VM it must agree with. The executable IR is the conformance seam.

### Compile a whole package as one native unit

Rejected for the first tier. Function granularity permits incremental coverage, bounded compile latency, recursion through a stable entry table and a clear fallback to encoded execution. Whole-program optimization may later run before function code generation without changing the compilation unit.

### Build an optimizing JIT first

Rejected. Hotness counters, OSR, speculative guards and deoptimization solve problems not yet measured. A baseline compiler removes dispatch and tests the runtime boundary with much less machinery. Adaptive selection is considered only after eager or first-call function compilation has numbers.

### Make JIT mandatory

Rejected. Executable-memory policy is an embedding constraint, not something a guest program should determine. The encoded VM remains a complete execution path, and future AOT can use the native lowering where JIT allocation is forbidden.

### Generate C first

Deferred rather than forbidden. C is portable as an AOT experiment but is a poor fit for function-at-a-time execution, in-process compilation and direct control of safepoints. The first experiment should test the architecture the decision names.

## Consequences

- ADR 0012's throughput gate is crossed; native execution is now implementation work rather than an untriggered roadmap option.
- The executable IR becomes the shared input to encoded and native execution.
- Inlining and semantic optimization are retained and gain target-specific budgets.
- Dispatch-only fusion becomes explicitly VM-private.
- A run may mix native and encoded functions, but never AST evaluation, and reports the mixture.
- The VM remains necessary for restricted runtimes, debugging, conformance and functions not yet covered by native lowering.
- Runtime helpers, slot frames and the non-moving heap allow native execution to arrive before a new GC or ABI.
- Fuel figures differ by execution tier; bounded stopping and Host-effect guarantees do not.
- JIT does not imply persisted code, AOT artifacts, an IR file format, speculative optimization or a new language surface.
