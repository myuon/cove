# ADR 0056: The first code generator is the one that was cheaper everywhere

- Status: Accepted
- Date: 2026-09-13
- Decides: which code generator the native tier uses first, now that both
  candidates have been measured on the same IR
- Supersedes: [ADR 0055](0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md)'s
  sentence "The first code generator is Cranelift", and nothing else it
  decided. The tier, the slot ABI, the function as the unit of compilation,
  the split of the optimizer by meaning, the safepoint contract and the
  adoption gate all stand as written

## Context

ADR 0055 named Cranelift without a comparison, on the reasonable grounds that
it is a fast baseline compiler that can later emit object code. A bounded
spike then built both candidates against the same lowered IR, the same ABI and
one shared subset predicate, and raced them twice: on `benches/arith`'s scalar
loop, and on `covefmt.byteOfPunct` and `covefmt.wantsASpaceBetween` with real
inputs — 107,285 bytes of `print.cove`, lexed into 19,566 tokens, 19,564 index
pairs an iteration, every answer checked against the VM's on every call.

| scenario | vm | cranelift | template | native ÷ vm |
|---|---:|---:|---:|---:|
| `arith`'s loop, no calls, no heap | 47.63 ms | 18.86 ms | **17.70 ms** | 2.5× |
| `byteOfPunct`, heap and tags, no calls | 4.57 ms | **1.87 ms** | 1.93 ms | 2.44× |
| `wantsASpaceBetween`, 6.4 nested calls each | 16.99 ms | **15.70 ms** | 16.02 ms | **1.08×** |

| | cranelift | template |
|---|---:|---:|
| compile, per function | 373 µs | **5.4 µs** |
| JIT init | 2.3 µs | **0.04 µs** |
| generated code, the covefmt slice | **15,356 B** | 19,415 B |
| stripped binary delta | +4,761,352 B | **+33,992 B** |
| crates added | +38 | **+2** |
| lines to extend to the covefmt slice, less comments | +239 | **+202** |

## Decision

**The native tier's first code generator is the hand-written x86-64 template
compiler.** Cranelift is not the first code generator and its arm may be
removed; the measurements and the benchmark harness stay.

The tier remains what ADR 0055 decided in every other respect. The template
compiler is a *baseline* generator in that ADR's sense: one function at a
time, no hotness counter, no OSR, no deoptimization, the VM's slot frame as
the canonical home of values, and the encoded VM for every function it
refuses. On an architecture other than x86-64 it refuses rather than
approximating, and the VM is the complete execution path there — which is the
same answer ADR 0055 already gives for a platform that forbids executable
memory.

## Why this and not the prediction

The case for Cranelift was never the measurements; it was a forecast that a
hand-written generator would complicate sharply once the IR grew past
arithmetic, and would need a register allocator to go further. The spike was
built to measure that forecast rather than argue it, and **the diff refuses
it**: extending to references, an `Array` element read, an enum tag, a switch,
`String.byteAt` and calls cost the template arm 202 non-comment lines against
Cranelift's 239. It needed no register allocator — six encoder forms and fixed
scratch registers per template. What was hard differed rather than deepened:
Cranelift's jump table wants the DFG's value-list pool and a `br_table` that
selects on `i32`, so a full-width index needs an explicit range check or a
word above `u32::MAX` is truncated into the table; the template arm's heap
addressing is eleven instructions wanting three registers at once.

On execution the two are within a few per cent, in both directions — the
template arm 6.2% ahead on scalars, Cranelift 3.0% and 2.0% ahead on the two
covefmt functions. The direction the forecast named is real and its magnitude
is not, and it is dwarfed by 69× on compile latency and 140× on the bundle.

[PHILOSOPHY](../PHILOSOPHY.md)'s "earn complexity through use" decides the
rest. Thirty-eight crates and 4.7 MB are a cost paid now for a capability no
measurement has yet asked for. When one does — a workload where an allocator
earns it — Cranelift is a decision away, and the harness that would prove it
is kept for exactly that.

## What this costs

**One architecture.** The template arm is x86-64 only. AArch64 is a second
encoder against the same structure, and until it exists those platforms run
the VM. Cranelift would have given three targets for free, and that is the
real thing being given up.

**A ceiling that is ours to raise.** Cranelift's optimiser would have grown
without us. Every future improvement to generated code — values living in
registers across a safepoint, folded immediates, strength reduction — is now
work this repository does, and the point at which that stops being worth it is
the point to reconsider this ADR.

**An unmeasured case.** Neither arm register-promotes, so no measurement here
covers a function needing many live values at once, which is where an
allocator should win. That is the strongest argument against this decision and
it is recorded rather than answered.

## Alternatives considered

### Keep Cranelift, as ADR 0055 said

Rejected on the numbers, and worth saying why it was recommended first: the
initial reading of the spike presented the forecast about future complexity as
if the benchmark had supported it. Review named that, the second measurement
tested the forecast directly, and it did not survive.

### Defer the choice until more of covefmt compiles

The covefmt slice *is* the deferral, taken: two functions carrying 10.8% of
that program's run, with references, tags, switches and calls. A larger slice
would answer the register-allocation question, and the way to get that is a
workload that needs many live values — not more of the same shape.

### Ship both and select at run time

Two generators to keep correct against one differential corpus, for a choice
no measurement supports making twice.

## Consequences

- The native tier compiles with the template compiler; the Cranelift arm may
  be deleted, and the comparison and its harness are kept.
- Native execution is x86-64 only for now; elsewhere the VM is complete, as
  ADR 0055 already provides for.
- Compile latency is 5.4 µs a function, which makes ADR 0055's "compile every
  supported reachable function eagerly" cheap enough not to need a hotness
  counter — a thousand functions is five milliseconds.
- No default build gains a code generator or an executable-memory dependency.
- **The measurement that matters most is not about either arm.** Native is
  2.44× the VM on code holding no call and 1.08× on code that does, because
  both tiers pay the same frame machinery per call and covefmt is short
  functions calling short functions. The next question is therefore the native
  call path — decomposed and measured, not assumed — and what it says decides
  between inlining, a lighter frame and a native calling convention. ADR
  0055's adoption gate is unchanged; this is what the work before it looks
  like.
