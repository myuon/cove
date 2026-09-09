# ADR 0049: A closure call answers a layout the program knows

- Status: Accepted
- Date: 2026-09-09
- Decides: that `Inst::CallClosure` carries the layout of what it answers, and
  what that costs — for
  [issue #299](https://github.com/myuon/cove/issues/299)
- Supersedes: [ADR 0041](0041-a-slot-number-fits-in-sixteen-bits.md)'s
  encoding of `CallClosure`, which is one row of that ADR's table and nothing
  else about the format it decided

## Context

A value location in this IR is a base slot and a layout, and the layout is the
only thing that says how many words the location is. Every instruction that
moves a value therefore has a layout somewhere: on the instruction, as a
`copy`'s and a `clear`'s is, or on a declaration the instruction names, as a
call's answer is.

Four of the five calls take the second route. `Call` reads `Function::returns`
off the `FunctionId` it names, `CallHost` and `CallResource` read `HostOp
::result` off the `HostOpId`, `CallBuiltin` reads `Builtin::result`. Each of
those is immutable program metadata and each is reachable from the
instruction, so nothing has to be carried.

`CallClosure` names no declaration. Its callee is the function id in the first
payload word of a `Shape::Closure` object, read when the instruction runs, and
that is the whole point of the instruction: a callee in a slot is the pair to
`Call`'s callee in the program.

**The width of the answer was thrown away with the callee's identity.** It did
not have to be. `lower::closures::call_value` reads the callee's *function
type* off what the checker settled — it must, because that is where the
parameter layouts come from — and `signature` computes the answer's layout in
the same breath. It then dropped it on the floor, and the layout was
unavailable to everything downstream:

- `crate::verify` could ask only `repr(dst)`: that `dst` was a slot inside the
  frame at all. Every other call gets `fits`, which asks that the location's
  words *are* the layout's words in order. A two-word answer written into the
  last slot of a frame was checked by nothing, and the machine wrote the frame
  above it — the exact failure mode `fits` exists for and which
  [ADR 0034](0034-one-physical-word-stack.md) records five of.
- `bytecode::verify` was in the same position through `Operand::Word(ANY)`.
- `slot_facts` treated the destination as one word wide, so a wider answer
  did not poison the slots it actually clobbers.
- The listing printed the head word's `Repr` where every other call now prints
  the run. `call-closure s10:int s3:ref (s9:Int)` was a two-word `m.Point`
  answer in `s10..s11`, and the line said `int`.

Issue #299 is about making a value location visible in the listing. It named
this instruction as the one exception and said a guessed layout would be worse
than the gap. The review of that change is what settled that there was no
guess to make: **the callee's identity is dynamic and the callee's type is
not.**

## Decision

`Inst::CallClosure` carries `result: LayoutId`, the layout of the value the
call writes at `dst`.

It is the layout the checker settled for the callee's function type, which is
the same fact the argument layouts already come from. The lowering has it at
every site: `call_value` from `signature`, and `lower::walks` and
`lower::cells` from the temporary they made for the answer.

The encoding is the one half ADR 0041 left free. `call.closure` had `lo =
ArgsId` and an unused `hi`; the layout goes in `hi`, so the row becomes
`lo = ArgsId, hi = LayoutId` and the instruction is the same eight bytes it
was. Nothing about the format changes — not the width, not the three operand
fields, not the payload's two halves. The opcode's field table declares its
`a` field `Operand::Value` rather than `Operand::Word(ANY)`, and the
`fits` check the encoded verifier already runs for every `Operand::Value` then
covers this destination without a line of its own.

## Consequences

The verifier gains the check it could not make. A closure call whose
destination is not a location of the answer's layout is a fault, in both
verifiers, from the same `fits` every other call is under.

The listing reads like every other call: `call-closure s10..s11:m.Point
s3:ref (s9:Int)`. The callee stays a word — `s3:ref` — because *that* is the
part which is genuinely a run-time fact.

**The machine is untouched.** `exec::encoded`'s `CALL_CLOSURE` arm reads
`held.lo()` for the argument list and nothing else; the width the answer is
copied at is still the callee's `Function::returns`, read when the frame
returns, because the frame being written is the callee's. No golden listing's
`frame N:` line moved and no instruction count changed.

Nothing static ties the object's function id to `result`. It cannot: which
body a closure holds is the question the instruction exists to ask at run time.
What `result` records is the *call site's* obligation, settled by the checker,
and `check_closure_callee` remains the only place the two are compared, in the
one case where a `FuncRef` stored into an environment is visible to the
verifier.

`lower::frees` still bounds the destination by the widest answer any function
in the program returns rather than by `result`. Narrowing it would remove that
pass's may-write/definitely-write split entirely, which is a change to what the
lowering *emits* and wants its own measurement:
[issue #301](https://github.com/myuon/cove/issues/301).

## Alternatives

**Leave the gap and document it.** What #299 originally decided, and what the
listing said for one commit: the destination stays a word, and the module
documentation explains that nothing in the program says what a closure call
answers. It is a true sentence about the *callee* and a false one about the
*answer*, and it made the IR the one place the checker's fact was unavailable.

**Look the layout up from the closure's layout.** A `Shape::Closure` names its
`function`, so where the object's layout is a static fact the answer is
reachable. It is not always: the layout of the slot holding a closure is
`Repr::Ref`, and which closure family it is, is exactly what a call through a
function value does not know. This would work for some call sites and not
others, which is worse than either alternative — a check that runs sometimes
reads as a check.

**A `Program::signatures` table, indexed like `args`.** One more piece of
program metadata and one more indirection to reach a single `LayoutId` that
fits in a payload half that was already free. Issue #299 asked for no second
metadata table and there is no reason for one here.
