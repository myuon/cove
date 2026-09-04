# ADR 0041: A slot number fits in sixteen bits

- Status: Accepted
- Date: 2026-09-05
- Decides: the width and field layout of the fixed-width instruction
  [issue #245](https://github.com/myuon/cove/issues/245) asks for, an encoding
  for every one of the forty-nine `Inst` variants, and the one compiler limit
  that width costs
- Supersedes nothing. [ADR 0034](0034-one-physical-word-stack.md) decided the
  machine this encodes for — one linear memory, one word stack, one slot
  numbering — and nothing here changes any of it. The readable `Inst` stays
  exactly as it is, including `pub type Slot = u32`
- Implementation status: none. This is a design record written before the
  encoder exists, which is what issue #245 asks for. Everything measured below
  was measured on this tree at commit `88c4387`; nothing decided below has code
  behind it yet

## Context

[ADR 0034](0034-one-physical-word-stack.md) replaced the execution backend
with one linear memory, one word stack, and one slot numbering shared by
parameters, locals, temporaries and captures. What it left behind is a
readable typed register IR — `cove_ir::Inst`, forty-nine variants — which is
both the compiler's representation *and* the thing the dispatch loop matches
on. Issue #245 proposes separating those two jobs: keep `Inst` for lowering,
optimisation, diagnostics, readable tests and the debugger, and execute a
fixed-width encoded form that is verified once and then trusted.

This ADR does not build that. It answers the question every later phase is
downstream of: **how wide is one encoded instruction, and does every variant
fit.**

The issue proposes sixteen bytes:

```rust
struct EncodedInst {
    opcode: u8,
    flags: u8,
    a: u16,
    b: u16,
    c: u16,
    payload: u64,
}
```

and it is blunt about the cost. A frame slot is `pub type Slot = u32`
(`crates/cove-ir/src/inst.rs:48`), so a `u16` operand caps one function's
frame. Its own words: *"If a `u32 Slot` is retained, document why and select
another fixed width, likely 24 or 32 bytes. Do not claim that four `u32`
operands plus an opcode fit in 16 bytes."*

So this is a trade, and it is settled below with numbers.

## What was measured

### The representation today

`size_of::<Inst>() == 24` and `align_of::<Inst>() == 8`. `Slot` is four bytes
and every id — `LayoutId`, `FunctionId`, `StrId`, `ArgsId`, `TableId`,
`HostOpId`, `BuiltinId` — is a `struct X(pub u32)`, also four.

Twenty-four is set by the widest variants: `ScopeLeave` and `CallResource`
carry four `u32`s, and `Int`, `Float`, `ArithImm` and `CmpImm` carry a
64-bit immediate that forces eight-byte alignment. This number matters twice
below.

### Every frame the repository lowers

Enumerating every `[run.*]` case in `tests/e2e/`, `examples/` and `benches/`
the way `crates/cove-cli/tests/vm_coverage.rs` does, checking each, lowering
each, and taking `Function::reprs.len()` over every function of every lowered
program:

- **117 programs lowered. 1,223 functions measured.**
- **The largest frame in the repository is 122 words** —
  `life.world.resolve`, in `examples/life`.
- Median 18. Mean 18.0. p90 37. p99 65.

| frame words | functions | share |
|---|---|---|
| 1–4 | 219 | 17.9% |
| 5–8 | 173 | 14.1% |
| 9–16 | 169 | 13.8% |
| 17–32 | 491 | 40.1% |
| 33–64 | 156 | 12.8% |
| 65–128 | 15 | 1.2% |
| 129–256 | 0 | 0% |
| 257+ | 0 | 0% |

The next nine after 122 are 79, 77, 75, 74, 74, 72, 72, 70, 70 — the `covecheck`
and `cq` examples, which are the largest hand-written programs the repository
keeps.

**122 is 537 times under a 65,536-word cap**, and it is 0.012% of one task's
stack segment; 8,594 frames of that size fit in a segment.

Nothing else an operand would hold is anywhere near a boundary either. Over the
same corpus the largest field word offset in a `LoadField`, `StoreField`,
`AddrOfField` or `AddrOfPart` is **13**; the largest `Len::Count(n)` is **13**;
the largest program has 552 strings, 360 argument lists, 178 functions, 99
layouts, 58 switch tables, 25 builtins and 10 host operations. The largest
single function has 869 instructions.

### What a frame that large would take, built deliberately

A distribution is not a ceiling, so three constructions were built and lowered
to find one.

**Many locals scales linearly and is hopeless as an attack.** A function of
*n* distinct `Int` locals lowers to a frame of exactly *n + 1*: 1,000 locals
is 1,004 lines and a frame of 1,001; 10,000 locals is 10,004 lines, 187 KB of
source, and a frame of 10,001. Reaching 65,536 that way costs about 65,000
lines and 1.4 MB. The lowering reuses a dead run for the next value of the same
shape (`crates/cove-ir/src/lower/frame.rs:207-213`), so only simultaneously
live bindings count at all.

**A deeply nested expression hits an unrelated limit first.** The parser
refuses at 64 levels with `cove::parse::nesting_too_deep`.

**Inline value width is the one that works, and it works easily.** A value
occupies as many frame words as its layout, struct fields are inline, and
[ADR 0035](0035-a-value-type-may-not-contain-itself.md) forbids only a value
type containing *itself* — so widths multiply through nesting. With one generic
declaration and a nested type annotation:

```cove
struct Pair<T> {
  a: T
  b: T
}

export fn main() -> Int {
  let x: Pair<Pair<...>> = build()
  0
}
```

| `Pair` nested | source | frame words | |
|---|---|---|---|
| 14 deep | 21 lines, 383 bytes | 32,768 | fits |
| 15 deep | 21 lines, **395 bytes** | **65,536** | fits — exactly at the cap |
| 16 deep | 21 lines, **407 bytes** | **131,072** | **over the cap** |
| 20 deep | 21 lines, 455 bytes | 2,097,152 | twice a whole task stack |

**Twelve bytes of source are the difference between fitting and not.** The
frame is `2^(d+1)`, so each further nesting doubles it, and the whole program
is twenty-one lines at every depth.

A second construction, fifteen ordinary struct declarations in a doubling chain
with a constructor function each, agrees: `frame = 6 × 2^k`, crossing the cap
between k = 13 (159 lines, 1,540 bytes, 49,152 words) and k = 14 (170 lines,
1,654 bytes, 98,304 words).

Two facts about the last row are the ones that decide this ADR. **Nothing in
`cove-sema` or `cove-ir` caps `Layout::width()` or inline nesting depth** — a
grep finds no such limit, and none of these programs drew a diagnostic, a
panic, or a hang. And a `Pair` nested 20 deep *lowers cleanly today* and then
cannot be called at all: its frame is twice `SEGMENT_WORDS`, so
`Memory::push_frame` refuses the very first call
(`crates/cove-runtime/src/vm/mem.rs:1615`) and the run fails with
`"this call nests too deeply"` — a message about recursion, for a program that
did not recurse.

Generated code was checked and is not an outlier: `examples/httpstatus` and
the `tests/e2e/generate_*` cases are the *smallest* programs in the corpus.

## Decision

### Sixteen bytes, and a slot operand is sixteen bits

```text
byte:  0        1        2   3     4   5     6   7     8 .. 15
       opcode   flags    a         b         c         payload
       u8       u8       u16       u16       u16       u64
```

Stored as `[u8; 16]` with explicit little-endian field accessors, not as a
Rust struct or enum whose layout is the compiler's business. Sixteen bytes is
two words, four to a cache line, and the byte offset of instruction `pc` is
`pc << 4`.

### `a`, `b` and `c` are frame slots; `payload` is everything else

This is the invariant the format rests on, and the audit below is what
establishes it. Reading all forty-nine variants against
`crates/cove-ir/src/verify.rs`, which already computes what each one carries:

- **No variant names more than three frame slots.** The widest are `Arith`,
  `Cmp`, `LoadElem`, `StoreElem`, `AddrOfElem`, `ScopeLeave` and `Spawn`, at
  three each.
- **No variant names more than 64 bits of anything that is not a slot.**
  Either one 64-bit immediate (`Int`, `Float`, `ArithImm`, `CmpImm`, and a
  branch displacement), or one 32-bit id, or two 32-bit ids (`Call`,
  `CallHost`, `CallBuiltin`, `Alloc` with `Len::Count`), or a 32-bit word
  offset beside a `LayoutId` (`LoadField`, `StoreField`), or two slots beside
  two ids (`CallResource`).
- **The two never both peak.** Nothing carries three slots *and* more than 32
  bits of payload, and nothing carries four slots at all.

So one field triple and one 64-bit payload is not a tight fit that happens to
work; it is the shape of the instruction set. `c` is unused by roughly
two-thirds of the opcodes, and that slack is what makes the format uniform.

What follows is the verifier's central check. "Every slot is inside the
function frame" is not forty-nine rules; it is one rule over three fields,
driven by a per-opcode table of which of the three are live. That is what a
*trusted* dispatch loop needs, and it is worth more than the bytes `c` costs.

**Only slot operands are narrowed. Nothing else is.** Every id keeps its full
32 bits, every field offset keeps its 32, and every immediate keeps its 64.

### The per-function frame limit is 65,536 words

A `u16` names slots 0 through 65,535, so a function whose frame is 65,536
words is exactly encodable and one word more is not. The bound is
`Function::reprs.len() <= 65_536` — not 65,535 as issue #245 states it. The
ADR gives the exact number so the boundary test can be exact: 65,536 compiles,
65,537 does not.

**It is a compile-time refusal with a source diagnostic.** `cove_ir::lower`
already answers `Result<Program, Vec<Diagnostic>>`
(`crates/cove-ir/src/lower/mod.rs:163-167`) and a function's `reprs` is final
the moment its body is lowered, so the check has a home and
`Function::span` is what it points at. Nothing truncates and nothing wraps.

**The diagnostic must name the cause, not only the number.** The measurements
above are why. A frame over the limit will almost never be a function with too
many locals — reaching the cap that way takes 65,000 lines — and will almost
always be one binding whose *layout* is enormous. A message that says
`this function's frame is 131,072 words, and the limit is 65,536` and stops
there points at the wrong thing. It must name the widest locations and their
layouts: *`x: Pair<Pair<…>>` occupies 65,536 words*. Without that the
diagnostic is a number the reader cannot act on.

### It is not the run's stack budget

Three limits bound a frame once this lands, and only the first is new:

| limit | bounds | when | where | reported as |
|---|---|---|---|---|
| this ADR's frame limit | one *function*, 65,536 words | compile time | `cove_ir::lower` | a diagnostic at the declaration |
| `SEGMENT_WORDS` | one *task's whole stack*, `1 << 20` words | run time | `Memory::push_frame`, `crates/cove-runtime/src/vm/mem.rs:155` and `:1613-1619` | `"this call nests too deeply"` |
| `Limits::max_call_depth` | one task's *frame count*, embedder-chosen | run time | `Machine::admit_frame`, `crates/cove-runtime/src/vm/exec.rs:1870-1878` | `RunOutcome::CallDepth` |

The arithmetic between the first two is the argument for adopting the limit,
and it is worth stating rather than asserting that 65,536 is a big number:

- `SEGMENT_WORDS` is `1 << 20` = 1,048,576 words, eight mebibytes, and that is
  one task's whole stack.
- A frame at the cap is 65,536 words — **exactly one sixteenth of an entire
  task's stack**, 512 KiB in a single call.
- `push_frame` refuses when `used + size >= SEGMENT_WORDS`, so **fifteen**
  frames at the cap fit in a segment and the sixteenth overflows. A function at
  the cap can nest fifteen deep and no further.

So this is not a new *kind* of restriction. It is a sixteenfold tightening of
one the runtime already imposes, moved from a run-time message that names the
wrong cause to a compile-time diagnostic that names the function.

### What the counter-example costs, stated rather than argued away

A 407-byte program reaches 131,072 words. That is the finding, and it is not
softened here.

What it costs is a band: programs whose widest frame is between 65,537 and
1,048,575 words lower today, have room to run today, and would be refused.
In the `Pair` construction that band is depths 16, 17 and 18 — three nestings,
thirty-six bytes of source apart. At depth 19 the frame is 1,048,576 and the
runtime already refuses the first call.

Sixteen bits is adopted anyway, for four reasons, in the order they carry
weight:

1. **For the very family of programs that reveals the cap, the cap improves
   the diagnostics.** `Pair` nested 20 deep is accepted by the checker,
   accepted by the lowering, and then fails at its first call with
   `"this call nests too deeply"`. That message is wrong about the cause and
   arrives at run time. A frame limit turns the same program into a
   compile-time error naming the function and the layout.
2. **What the counter-example actually reveals is a different missing limit.**
   Cove caps no inline value's width. A 512 KiB struct is copied by value on
   every assignment — `Copy` is field-wise and unconditional
   (`crates/cove-ir/src/inst.rs`, `Inst::Copy`) — and nothing anywhere says so.
   That defect exists at commit `88c4387` and is untouched by whatever width an
   instruction is. See "What this does not decide".
3. **The repository is 537 times under the cap**, across 1,223 functions, with
   nothing above 122 and nothing at all above 128.
4. **A refused program is refused loudly, at compile time, with a span.** No
   truncation, no wrapping, no silently wrong slot number.

What would overturn this decision is stated so that it can be recognised: a
frame within one order of magnitude of the cap — 6,554 words or more — in a
program someone meant to write, whether hand-written, generated, or
monomorphised. The corpus maximum is 122. The diagnostic this ADR requires is
what would report the first such program, which is a better instrument for the
question than this ADR had.

### One opcode per concrete operation, generated from the IR's families

`Inst`'s module doc argues that *"the instruction set describes families, not
cases"* — one `Arith`, not one per numeric type
(`crates/cove-ir/src/inst.rs:27-36`). That argument is about the *language*
growing a concept, and the bytecode grows none by enumerating members that
already exist. So:

- `Arith` becomes ten opcodes, `Num` × `ArithOp`.
- `Cmp` becomes thirty, `Compare` × `CmpOp`.
- `ArithImm` becomes five and `CmpImm` six. `Num` is already absent from both
  — *"there is only `Num::Int` to name"* — and the immediate is already `i64`.
- `Neg` becomes two, `Convert` two.
- `Alloc` becomes three, one per `Len` form: `alloc.fixed`, `alloc.imm`,
  `alloc.slot`.

The cross products are **generated mechanically from the two enums, not
hand-picked**, and that is a decision rather than laziness. `verify.rs`
constrains which `Repr`s a `Compare` admits (`:399-412`) and does not
constrain which `CmpOp` pairs with which `Compare`. A hand-picked table would
be a second, weaker copy of the type rules living in the encoder. An opcode
the lowering never emits costs one number out of 256 and one row of a
generated table; a rule about which pairs are legal costs a place for two
copies to disagree.

That is **100 opcodes** against the 256 an opcode byte can name:

| family | opcodes |
|---|---|
| constants and moves — `const.{unit,bool,int,float}`, `str`, `copy`, `clear` | 7 |
| `neg` | 2 |
| `Arith` | 10 |
| `Cmp` | 30 |
| `ArithImm` | 5 |
| `CmpImm` | 6 |
| `not`, `Convert` | 3 |
| control — `jump`, `branch.false`, `switch`, `return` | 4 |
| calls | 5 |
| `Alloc` | 3 |
| heap — `load.field`, `store.field`, `load.elem`, `store.elem`, `len`, `layout.of` | 6 |
| places — `addr.{slot,field,elem,part}`, `load`, `store` | 6 |
| erasure — `box`, `unbox` | 2 |
| tasks — `scope.{enter,leave,cancel}`, `spawn`, `await`, `cancel`, `settled` | 7 |
| cells — `shared.{lock,unlock}` | 2 |
| failure — `trap`, `assert.failed` | 2 |
| **total** | **100** |

**Opcodes are not merged where merging would lose a check.** `Unit`, `Bool`,
`Int` and `Float` all write one immediate word into a slot and could be one
opcode. They stay four, because the verifier checks a different `Repr` for
each of the four destinations (`crates/cove-ir/src/verify.rs:356-359`) and one
opcode cannot be asked which it meant. Three opcode numbers is the cheaper
side of that trade.

`flags` carries nothing and must be zero; the verifier rejects a nonzero one.
Nothing in the audit needs it, and reserving it for a fact that does not exist
is what `docs/PHILOSOPHY.md` calls earning complexity through use. `Bool`'s
value goes in `payload` beside `Int`'s and `Float`'s, so the four constants
have one shape rather than three and an exception.

### One instruction in, one instruction out

Every row of the audit encodes one `Inst` as exactly one `EncodedInst`. No
variant expands and none is elided — a consequence of the operand invariant
rather than a rule imposed on top of it.

That bijection is worth naming for what it saves. `Function::spans` is already
a parallel array indexed by pc, deliberately outside the instruction
(`crates/cove-ir/src/program.rs:260-265`: *"a span is read when a run fails or
a trace is written, and never in the dispatch loop, so it should not be in the
cache line the loop is reading"*). `Local::from` and `Local::to` are pc ranges.
`Table::targets` are pcs. Under a 1:1 encoding **all three keep their meaning
with no remapping at all**: bytecode pc *is* IR pc.

So issue #245's requirement that spans stay out of the hot body while
remaining indexable by bytecode pc is met by changing nothing, and the
debugger's mapping is the identity. For a selected instruction it can show the
pc, the byte offset `pc << 4`, the raw sixteen bytes, the decoded operands,
`Function::span_at(pc)` (`crates/cove-ir/src/program.rs:318`), and the `Inst`
at `code[pc]` — the last of which makes lowered IR and executable bytecode two
views of one index rather than two programs to correlate.

It also decides what the disassembler is. `crates/cove-ir/src/print.rs` is
already a disassembler for `Inst`, so a lossless decoder plus that printer is a
disassembly that cannot drift from the IR's own rendering. A second renderer
would be a second thing to keep in sync for no reader's benefit.

### Branches are relative; a switch table stays absolute program metadata

`Jump` and `BranchFalse` carry `to - (pc + 1)` in `payload` as a
two's-complement `i64`, and the verifier checks that `pc + 1 + displacement`
lands in `[0, code.len())` — the check `Verifier::target` already makes.
Encoder overflow is not a live hazard: `Pc` is `u32`
(`crates/cove-ir/src/inst.rs:51`) and the displacement is `i64`, so every
representable pc pair has a representable displacement. The encoder asserts it
regardless, because "the encoder rejects overflow" should be a line of code
rather than an argument.

`Switch` keeps `TableId` in `payload`'s low half, and the table stays immutable
program metadata with **absolute** `Pc` targets. The lowering already pushes
one table per switch site without interning
(`crates/cove-ir/src/lower/mod.rs:1137-1138`), so relative would buy nothing
and would make a table's meaning depend on where it is read from. If tables are
ever interned across sites, absolute is the encoding that breaks loudly, in the
verifier.

### Calls keep `ArgsId`

`Call`, `CallClosure`, `CallHost`, `CallResource` and `CallBuiltin` all encode
without a variable-width instruction, and they do it by leaving the argument
list where it is: `Program::args`, a list of `(slot, layout)` pairs held once
per call shape. That is **immutable program metadata**, decided at lowering and
read-only at run time. It is not a runtime value table, and this ADR
introduces none — [ADR 0034](0034-one-physical-word-stack.md) is explicit and
so is the issue.

A contiguous `args_base + argc` convention is not evaluated here and is not
required by the encoding: `CallResource`, the densest call, fits with `c` still
empty.

### Verification

Encode, then verify once, then trust. The verifier must be safe against
arbitrary bytes even while the format is internal, and checks at minimum:

- the opcode byte names a defined opcode;
- `flags` is zero;
- every field the opcode does not use is zero — which makes the encoding
  *canonical*, so `encode` is a function, `decode(encode(i)) == i` and
  `encode(decode(b)) == b` are both testable, and two encodings of one program
  are byte-identical;
- each of `a`, `b`, `c` the opcode declares live is `< Function::reprs.len()`,
  and a slot heading a multiword location satisfies
  `slot + layout.width() <= reprs.len()` — the `fits` check `verify.rs` makes;
- the slot's `Repr` is one the opcode admits — the `expect` check `verify.rs`
  makes, now driven by an opcode instead of by a match on an enum;
- every id in `payload` indexes its table: `FunctionId`, `LayoutId`, `StrId`,
  `ArgsId`, `TableId`, `BuiltinId`, `HostOpId`;
- every branch displacement and every `Table` target lands on an instruction
  boundary inside the function — which, under a 1:1 encoding, is any pc in
  `[0, code.len())`;
- a call's argument list matches the callee's parameter layouts, and its
  destination fits the callee's `returns`;
- a `payload` field narrower than the bits it occupies is in range:
  `Alloc`'s `Len::Count`, and the `at` of `LoadField`, `StoreField`,
  `AddrOfField` and `AddrOfPart`.

After that the dispatch loop may index without checking. What stays a run-time
question stays one: division by zero, an object's layout against the layout the
instruction names, element bounds, fuel, deadlines, cancellation, host failure.

One gap is inherited rather than introduced, and is recorded so it is not
mistaken for a regression. `AddrOfPart`'s `at` is not bounded against the value
the address names, because a frame records no value's extent —
`crates/cove-ir/src/verify.rs:622-630` says so and gives the reason. The
encoding neither changes that nor worsens it; `at` keeps its full 32 bits.

## The encoding audit

Every variant, with `lo` and `hi` the low and high 32-bit halves of `payload`.
An empty cell is a field the opcode does not use, which the verifier requires
to be zero.

| `Inst` variant | opcode(s) | `a` | `b` | `c` | `payload` | outside the instruction |
|---|---|---|---|---|---|---|
| `Unit { dst }` | `const.unit` | dst | — | — | — | — |
| `Bool { dst, value }` | `const.bool` | dst | — | — | `value as u64` (0 or 1) | — |
| `Int { dst, value }` | `const.int` | dst | — | — | `value as u64` — the whole i64 | — |
| `Float { dst, bits }` | `const.float` | dst | — | — | `bits` — the whole u64 | — |
| `Str { dst, text }` | `str` | dst | — | — | lo = `StrId` | `Program::strings`, `Program::str_layout` |
| `Copy { dst, src, layout }` | `copy` | dst | src | — | lo = `LayoutId` | `Program::layouts` |
| `Clear { slot, layout }` | `clear` | slot | — | — | lo = `LayoutId` | `Program::layouts` |
| `Neg { num, dst, a }` | `neg.int`, `neg.float` | dst | a | — | — | — |
| `Arith { num, op, dst, a, b }` | 10: `{add,sub,mul,div,rem}.{int,float}` | dst | a | b | — | — |
| `Cmp { on, op, dst, a, b }` | 30: `{eq,ne,lt,le,gt,ge}.{int,float,bool,str,identity}` | dst | a | b | — | — |
| `ArithImm { op, dst, a, value }` | 5: `{add,sub,mul,div,rem}.int.imm` | dst | a | — | `value as u64` — the whole i64 | — |
| `CmpImm { op, dst, a, value }` | 6: `{eq,ne,lt,le,gt,ge}.int.imm` | dst | a | — | `value as u64` — the whole i64 | — |
| `Not { dst, a }` | `not` | dst | a | — | — | — |
| `Convert { to, dst, a }` | `int.to.float`, `float.to.int` | dst | a | — | — | — |
| `Jump { to }` | `jump` | — | — | — | `to - (pc + 1)` as a two's-complement i64 | — |
| `BranchFalse { cond, to }` | `branch.false` | cond | — | — | `to - (pc + 1)` as a two's-complement i64 | — |
| `Switch { on, table }` | `switch` | on | — | — | lo = `TableId` | `Program::tables`; targets stay absolute `Pc` |
| `Return { src }` | `return` | src | — | — | — | `Function::returns` |
| `Call { dst, callee, args }` | `call` | dst | — | — | lo = `FunctionId`, hi = `ArgsId` | `Program::functions`, `Program::args` |
| `CallClosure { dst, closure, args }` | `call.closure` | dst | closure | — | lo = `ArgsId` | `Program::args`; callee read from the object |
| `CallHost { dst, op, args }` | `call.host` | dst | — | — | lo = `HostOpId`, hi = `ArgsId` | `Program::host_ops`, `Program::args` |
| `CallResource { dst, receiver, op, args }` | `call.resource` | dst | receiver | — | lo = `HostOpId`, hi = `ArgsId` | `Program::host_ops`, `Program::args` |
| `CallBuiltin { dst, builtin, args }` | `call.builtin` | dst | — | — | lo = `BuiltinId`, hi = `ArgsId` | `Program::builtins`, `Program::args` |
| `Alloc { dst, layout, len: Fixed }` | `alloc.fixed` | dst | — | — | lo = `LayoutId` | `Program::layouts` |
| `Alloc { dst, layout, len: Count(n) }` | `alloc.imm` | dst | — | — | lo = `LayoutId`, hi = `n` | `Program::layouts` |
| `Alloc { dst, layout, len: Slot(s) }` | `alloc.slot` | dst | s | — | lo = `LayoutId` | `Program::layouts` |
| `LoadField { dst, obj, at, layout }` | `load.field` | dst | obj | — | lo = `at`, hi = `LayoutId` | `Program::layouts` |
| `StoreField { obj, at, src, layout }` | `store.field` | obj | src | — | lo = `at`, hi = `LayoutId` | `Program::layouts` |
| `LoadElem { dst, obj, index, layout }` | `load.elem` | dst | obj | index | lo = `LayoutId` | `Program::layouts` |
| `StoreElem { obj, index, src, layout }` | `store.elem` | obj | index | src | lo = `LayoutId` | `Program::layouts` |
| `Len { dst, obj }` | `len` | dst | obj | — | — | — |
| `LayoutOf { dst, obj }` | `layout.of` | dst | obj | — | — | — |
| `AddrOfSlot { dst, slot }` | `addr.slot` | dst | slot | — | — | — |
| `AddrOfField { dst, obj, at }` | `addr.field` | dst | obj | — | lo = `at` | — |
| `AddrOfElem { dst, obj, index, layout }` | `addr.elem` | dst | obj | index | lo = `LayoutId` | `Program::layouts` |
| `AddrOfPart { dst, addr, at }` | `addr.part` | dst | addr | — | lo = `at` | — |
| `Load { dst, addr, layout }` | `load` | dst | addr | — | lo = `LayoutId` | `Program::layouts` |
| `Store { addr, src, layout }` | `store` | addr | src | — | lo = `LayoutId` | `Program::layouts` |
| `Box { dst, src, layout }` | `box` | dst | src | — | lo = `LayoutId` | `Program::layouts`, `Program::boxed_layout` |
| `Unbox { dst, src, layout }` | `unbox` | dst | src | — | lo = `LayoutId` | `Program::layouts` |
| `ScopeEnter { dst, name }` | `scope.enter` | dst | — | — | lo = `StrId` | `Program::strings` |
| `ScopeLeave { scope, failed, error, layout }` | `scope.leave` | scope | failed | error | lo = `LayoutId` | `Program::layouts` |
| `ScopeCancel { scope }` | `scope.cancel` | scope | — | — | — | — |
| `Spawn { dst, scope, closure, answer }` | `spawn` | dst | scope | closure | lo = `LayoutId` | `Program::layouts` |
| `Await { dst, task, answer }` | `await` | dst | task | — | lo = `LayoutId` | `Program::layouts` |
| `Cancel { task }` | `cancel` | task | — | — | — | — |
| `Settled { dst, src, answer }` | `settled` | dst | src | — | lo = `LayoutId` | `Program::layouts` |
| `SharedLock { cell }` | `shared.lock` | cell | — | — | — | — |
| `SharedUnlock { cell }` | `shared.unlock` | cell | — | — | — | — |
| `Trap { message }` | `trap` | — | — | — | lo = `StrId` | `Program::strings` |
| `AssertFailed { message }` | `assert.failed` | message | — | — | — | `Function::spans` carries where |

**No variant fails to fit.** The audit's own summary is the invariant above:
three slots at most, 64 bits of payload at most, never both at their maximum.
The three tightest rows are `CallResource`, `LoadField` and `StoreField`, which
use two slots and both halves of the payload; the widest in slots are
`Arith`, `Cmp`, `LoadElem`, `StoreElem`, `AddrOfElem`, `ScopeLeave` and
`Spawn`, which use all three and at most one half.

Issue #245 asks that the hardest cases be covered explicitly, and they are, in
the table:

- `Int` and `Float` take the whole payload and no more. They are what forces
  the payload to be eight bytes and the instruction to be eight-aligned.
- `CallResource` is the only call with a receiver, and it is a slot rather than
  `args[0]` for the reason `Inst`'s own doc gives; that costs `b` and nothing
  else.
- `LoadField` and `StoreField` pack the word offset and the `LayoutId` into the
  two halves of the payload. Both keep 32 bits, though the corpus never uses an
  offset above 13.
- `Alloc` is three opcodes, so `Len`'s three forms are three encodings and no
  discriminant is stored anywhere.
- `AddrOfField`, `AddrOfElem` and `AddrOfPart` are ordinary: two or three slots
  and one 32-bit field.
- `ScopeLeave` is the three-slot case issue #245 predicted would be tight, and
  it is: `scope`, `failed`, `error`, plus a `LayoutId`. It fits with the
  payload half-empty.
- `Spawn`, `Await` and `Settled` carry the answer's `LayoutId` in the payload
  and their slots in `a`, `b`, `c`.
- `SharedLock` and `SharedUnlock` are one slot each. They come in pairs, and
  which paths hold which cell is not a fact about one instruction — the same
  limit `verify.rs` records for them today.
- `Box` and `Unbox` are two slots and a `LayoutId`. `Trap` and `AssertFailed`
  are the two failure operations, one carrying a `StrId` and one a slot, and
  neither needs a span in the instruction because `Function::spans` has it.
- `ArithImm` and `CmpImm`, from #244, are two slots and a 64-bit immediate.
  They are the reason a 16-byte encoding with `u32` operands is impossible:
  `1 + 4 + 4 + 8` is seventeen bytes before alignment.

### What was rejected

**Twenty-four bytes with `u32` operands.** The honest alternative, and it works:
`opcode: u8, flags: u8, pad: u16, a: u32, b: u32, c: u32, payload: u64` is 24
bytes at eight-byte alignment, encodes every variant, and imposes no new limit.
Rejected on three counts, and the first is the weakest.

1. It is 1.5× the static bytes.
2. Twenty-four does not divide sixty-four, so instructions straddle cache lines
   and a pc's byte offset is a multiply where sixteen makes it `pc << 4`.
3. **`size_of::<Inst>()` is already 24 on this tree.** A 24-byte encoded form
   would be exactly the size of the enum it replaces — all of the encoding
   work, none of the density. What would remain is flat dispatch and
   verify-once, which are real, but they are not what a *width* decision is
   for.

Against that it avoids a limit that is sixteen times looser than one the
runtime already enforces, and that no program in this repository comes within
537× of.

**Thirty-two bytes.** Strictly worse than 24; nothing needs the room.

**Sixteen bytes keeping `u32` for `a` and `b`.** An opcode, flags and two
`u32`s leave six bytes of payload, which cannot hold an `i64`. `Int`, `Float`,
`ArithImm` and `CmpImm` would need a constant pool — a new program table and an
indirection on the hottest immediate path. #244's immediates exist precisely so
a loop's literal is not fetched from anywhere, and this would put it back.

**Variable-width encoding.** Issue #245's non-goal, and it would cost the 1:1
mapping that keeps `Function::spans`, `Local`'s pc ranges and `Table::targets`
meaningful without a remap.

**Narrowing `pub type Slot = u32` in the IR.** Rejected. The readable IR is the
compiler's representation and the encoding's width is not its business. Keeping
`Slot = u32` makes the limit one check in the lowering and one assertion in the
encoder, rather than a type change rippling through `Arg`, `Capture`, `Local`,
the lowering and the VM — and it lets an optimisation pass that transiently
exceeds the bound be a diagnostic rather than a type error.

**A runtime side table of any kind.** Ruled out by ADR 0034 and by the issue,
and nothing in the audit asks for one: every operand is a slot, an index into
immutable program metadata, or an immediate.

## Consequences

- One new compiler diagnostic and one boundary test: a frame of exactly 65,536
  words compiles, one of 65,537 does not.
- **Programs that compile today will stop compiling.** Measured: the band is a
  widest frame between 65,537 and 1,048,575 words, and 407 bytes of source
  reaches it. This is the cost, and the previous section is where it is
  weighed rather than hidden.
- The encoder is total. There is no variant it can refuse, no fallback to enum
  execution to design around, and no "encodes on this program" caveat to carry
  into Phase 3.
- The verifier's slot check is one rule over three fields rather than
  forty-nine rules, because `a`, `b` and `c` are always slots.
- The debugger's bytecode view needs no mapping table. Bytecode pc is IR pc,
  byte offset is `pc << 4`, the span is `Function::span_at(pc)`, and the
  lowered instruction is `code[pc]`.
- The disassembler is `decode` plus `crates/cove-ir/src/print.rs`, so
  disassembly cannot drift from the IR's rendering.
- Static code is two thirds of `size_of::<Inst>()` per instruction. Whether
  that shows up in a benchmark is Phase 3's question and this ADR does not
  predict it.
- Nothing about cost accounting changes. `Copy` and `Clear` stay one
  instruction each and still cost one fuel each, on the 1024-instruction stride
  [ADR 0040](0040-a-bound-outlives-its-backend.md) records — exactly as they do
  on the enum today. **A fixed width does not imply a fixed cost**, and this
  ADR records that rather than changing it, because changing it is a change to
  ADR 0040's arithmetic and belongs with a measurement of what it buys.

## What this does not decide

- **A limit on inline value width**, which is what the counter-example above
  actually points at. Cove caps no layout's width; a `Pair` nested 20 deep is a
  legal 8 MiB value that is copied field-wise on every assignment and refused
  only by `push_frame`, at its first call, with a message about recursion. The
  frame limit covers part of that by accident and is not the right instrument
  for it. The number to carry into that decision is the corpus maximum
  *layout* width, not the corpus maximum frame.
- **The calling convention.** `ArgsId` stays. A contiguous `args_base + argc`
  convention is not required by the encoding — `CallResource`, the densest
  call, fits with `c` empty — and issue #245 asks that it be evaluated
  separately.
- **Anything about compatibility.** The first encoded form is an internal
  executable representation. No stable on-disk bytecode, no cross-version
  compatibility, no public ABI, no loading of untrusted serialized bytecode,
  and **no opcode-number stability**. If serialization is added for the
  debugger or for Wasm transport it is versioned and validated, and
  compatibility stays explicitly unsupported until a separate ADR says
  otherwise.
- **What `flags` is for.** Reserved, and required to be zero.
- **Whether unreachable comparison opcodes are worth pruning.** The table is
  the mechanical cross product; if the count ever matters, pruning changes a
  generated table and nothing else.
- **Whether the encoded form is faster.** Issue #245 is explicit that
  measurement is context and not a gate the format must pass before it exists.
  Phase 3 measures `arith` and Phase 5 the rest, on
  [ADR 0029](0029-a-benchmark-number-is-evidence-within-one-build.md)'s terms.
