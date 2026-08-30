# ADR 0027: A place and a capture name a slot, not a stack

- Status: Accepted
- Date: 2026-08-30
- Supersedes: nothing. It completes
  [ADR 0019](0019-executable-ir-and-vm.md)'s "Slots, not names" rather than
  replacing any part of it, and refers to it from Context one way
- Implemented by: this change, against
  [issue #162](https://github.com/myuon/cove/issues/162)
- Implementation status: two of the three things #162 asks for are built and
  measured, and the third is named under "What is not decided here". The
  slot identity below is the whole of what a place and a capture now name; a
  *single physical* frame — one base, one region, one index space — is not
  built and this ADR does not decide it

## Context

The VM runs over three parallel stacks: a `Vec<Value>` for general values, a
`Vec<i64>` for the slots the checker proved are `Int` or `Bool`, and a
`Vec<Place>` for the aliases a `var` parameter receives. ADR 0019 already
said what a frame is — "a contiguous region of slots whose size is known when
the function is lowered", with parameters, locals and temporaries occupying
it by index, and captures "an explicit list with an explicit layout, decided
at lowering rather than discovered when a closure is created". Nothing below
disturbs that. What it disturbs is an assumption that grew up beside it and
was never decided: that *some* slots are not numbered by the same rule.

Two were not, and issue #116's assessment measured both on
`benches/convention`, whose rows are one loop written nine ways so that the
difference between two of them is one thing.

**A place could only name the value stack.** So `cove_ir::lower` walked every
body once before it emitted anything, collected the *names* used as the root
of a `var` argument or of a `freeze` receiver, and kept every binding of one
of those names on the value stack — even where the checker had settled it as
`Int`. That is not a cost paid where the place is built. It is paid on every
read and every write of the binding, for the whole of the body, whether or
not the place is ever built at all: `conv_var` is `conv_local` with one
`root(var v)` written *outside* the loop and an identical loop body, and it
cost **1.30×**, two extra instructions a turn, with `drop_in_place<Value>`
and `Value::clone` appearing at 3.2% where `conv_local` had neither.

**A capture was a value slot by construction.** The call copied the closure's
captures into the frame's value window, behind the value parameters, so a
captured `Int` had no scalar representation available to it and every read of
one crossed. `conv_capture` ran five boundary instructions a turn against
`conv_closure`'s two — the parameter, the capture and the answer all crossed
— and cost **1.16×**, with `drop_in_place<Value>` and `Value::clone` at 10.5%
of the run.

Both were the same question, and the assessment said so: what would fix
either is a slot numbering scheme that does not decide the representation for
the slot's *role*.

Two results argue the other way and are part of the decision rather than
against it. A boundary crossing is cheap and allocates nothing: two of them
are about 16 ns a turn, and seven of the matrix's nine rows allocate not once
in two million turns, because a boundary instruction converts between an
`i64` and a `Value::Int`, which owns nothing. So what was worth fixing was
never the crossing's direct cost. It was that in these two cases the crossing
happened **per read** rather than per call.

## Decision

**A place names a slot, and a slot is a stack and a number in it. A capture
takes the slot its own kind names.** Nothing about a slot's *role* —
parameter, local, temporary, capture, or the root of an alias — decides which
stack it lives in. Only the checker's answer about its type does.

### A place carries which stack it is rooted at

`cove_runtime::vm::Place` is a root and a path, where the root is a value
slot or a scalar slot. `Inst::PlaceScalar(slot, Scalar)` is `Inst::PlaceLocal`
for the other stack, and it carries the `Scalar` because the scalar stack
keeps no tag and a read through the place has to put one back — the same
argument `Inst::ScalarToValue` carries one for.

A scalar root has no path and can never acquire one. Neither `Int` nor `Bool`
has a field, and `crate::lower` emits a field step only where the checker
settled the struct type the step is taken in, so the two instructions that
walk a path can never be handed one.

The pre-pass goes with it. `lower::scan::var_argument_roots` and `Body::rooted`
are deleted, and with them the over-approximation across shadowing that
`bump(var total)` written anywhere in a body imposed on every `total` the body
declared.

### A capture is a frame slot like any other

`Function::captures` pairs each capture's name with the stack its slot is in.
The value captures are dense from `capture_base` and the scalar captures dense
from scalar slot 0 — which they can be because `validate` refuses a function a
closure is made of that takes any argument on the scalar stack, and that
refusal is what makes `0` a static number.

A body reads a scalar capture with an ordinary `load-scalar`. So
`Inst::LoadCapture` is gone: a capture is reached by the path every other
binding is reached by, which is what ADR 0019's "slots, not names" says a
frame slot is.

### What travels is still a `Value`

A closure holds `(name, Value)` pairs whichever backend built it. That is not
a compromise; it is what a closure *is* to everything outside the backend
that made it, and a host reads those pairs.

It is also what makes the decision above sound. `crate::lower` numbers one
lambda per syntactic site, however many specialisations of the body around it
are lowered — and a declaration reached both directly and through a value is
lowered twice, once with an `Int` parameter as a scalar slot and once with
every argument on the value stack. So the two `make-closure` sites for one
lambda can disagree about the representation a capture had *where it stood*,
while the lambda has one capture layout. What the callee states is where the
value lands, and the call converts it there. The checker's type is the same on
every road to the same lambda, so the conversion cannot fail: a disagreement
costs a conversion and cannot cost an answer.

The conversion therefore happens **once per call in place of once per read**,
which is the whole of the prize.

### The collector is unchanged, and must stay unchanged

A place is not a root and must not be walked. A place rooted at a value slot
reaches only what that slot holds, which is inside the value window
`StackRoots::walk` already yields; a place rooted at a scalar slot reaches an
`i64` and so reaches nothing. Walking a place as well would charge one value
twice, which is the failure mode PR #192 kept `Vm::arg_vectors` out of the
root set for.

A scalar capture leaves the value window, which can only shrink the root set,
and what it leaves behind holds an `i64`. The closure still holds the `Value`
and is still walked as one object, so no heap figure moves — and none does:
the differential corpus and `vm::tests::heap`'s `same_heap` comparisons hold
exactly, including two new ones that put a scalar-rooted place and a
mixed-capture closure across a collection.

## What this bought, and how it was measured

Wall-clock numbers here are stated as ratios **within one binary**, and the
instruction counts as absolutes, for a reason this change measured rather than
assumed. See "The layout band is much wider than it was thought to be" in
`docs/VM_ARCHITECTURE.md`: a control build of the *base* commit, whose only
change is one `Inst` variant that is never emitted and never executed, moved
`benches/arith` on the VM by **+23.5%** and `conv_local` by **+6.4%** on
identical instruction counts. A cross-build absolute comparison on this
workspace cannot separate a design from its code layout, and this ADR does
not claim one.

| | base | control | after |
| --- | ---: | ---: | ---: |
| `conv_var` ÷ `conv_local` | 1.30× | 1.37× | **1.00×** |
| `conv_capture` ÷ `conv_closure` | 1.23× | 1.20× | **1.11×** |

Instruction counts, which no layout moves:

| row | base | after |
| --- | ---: | ---: |
| `conv_var` | 39,142,890 | **35,142,890** |
| `conv_capture` | 53,142,883 | **51,142,883** |

`conv_var` now runs `conv_local`'s instructions exactly — 17.6 a turn against
17.6 — because the two programs' loops were always the same loop and the
difference was only ever the representation one of them was forced into.

What is left between `conv_capture` and `conv_closure` is four instructions a
turn, two of which are the addition and the read that `+ zero` *is*, and two
of which are the general convention's: a closure's parameter arrives on the
value stack and its answer leaves on it, because nothing at an
`Inst::CallValue` knows which function it will enter. That is not a capture
cost and this decision does not address it.

## What is not decided here

**A single physical frame.** Issue #162's title asks to unify the VM's
logical stack and frame layout, and this ADR unifies the *identity* of a slot
without unifying its storage: there are still three stacks, three bases per
frame, and three counts on a call. Design B of that issue — one compact
word-wide slot stack with a GC bitmap — is not built, not measured, and not
refused here. What this change removes is the reason it looked mandatory: the
two costs #116 handed to #162 were both consequences of a slot's *role*
deciding its representation, and neither needed one physical stack to fix.

**A typed read through a place.** `place-read` answers a `Value` whatever the
place is rooted at, so a `var` parameter the checker settled as `Int` still
crosses on entry to the body that reads it. That is once per call rather than
once per read, so it is not a cliff; a `place-read-scalar` would remove it.

**A capture whose kind follows its use.** A capture's kind is the enclosing
binding's, which is a proxy for the checker's type and not for what the
lambda's body does with it. A closure that captures a scalar only to answer
with it — `benches/convention`'s `conv_fresh` — now runs one *more*
instruction a turn than it did, because the capture is a word and the answer
must be a value. The rows say the trade is worth taking as a default and do
not say it is worth taking always.

**Which stack a closure's parameters arrive on.** They are all value slots,
because an `Inst::CallValue` places its arguments before the callee is known.
That is the general convention and it is the case the convention exists for.

## Consequences

- A binding the checker settled as `Int` or `Bool` keeps its representation
  whatever a `var` argument elsewhere in the body does with it. There is no
  longer any construct that can demote a scalar local.
- One walk over a body's source is gone from the lowering, and with it the
  only place where a *name* rather than a binding decided a layout fact.
- VM fuel changes for programs that root a place at a scalar binding or read
  a scalar capture, because VM fuel is its instruction count. ADR 0019 makes
  fuel backend-specific and says why; ADR 0024's four bounds are untouched,
  because nothing here changes what may be gathered between two checks.
- `Function` carries one capture list rather than two read in lockstep, and
  is 240 bytes as it was. `Inst` is 16 bytes as it was: the instruction stream
  did not grow.
- Every claim above is a within-build ratio or an instruction count. The
  absolute wall times of both builds are recorded in
  `docs/VM_ARCHITECTURE.md` beside the control that says what they are worth.
