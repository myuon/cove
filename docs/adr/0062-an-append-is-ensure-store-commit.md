# ADR 0062: An append is ensure, store, commit

- Status: Accepted
- Date: 2026-09-17
- Decides: how shared semantic IR spells a growable append; what the verifier
  requires of it; where the orchestration of `Vector.push` and the string
  builder's appends lives; and how the encoded VM and both native code
  generators recover one fast path from the split without the shared IR
  becoming nominal again. Recorded for
  [issue #409](https://github.com/myuon/cove/issues/409)
- Supersedes:
  [ADR 0052](0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md)'s
  **"The IR exposes bulk owner operations"**, which made `append-byte` and
  `append-bytes` instructions of the IR so that a bulk append would be one
  dispatch. They leave shared IR; one dispatch per append becomes a property of
  the encoded VM's legalization, not of the IR. `alloc-builder` and
  `finish-builder` keep their instructions, for the reasons given below; and
  [ADR 0041](0041-a-slot-number-fits-in-sixteen-bits.md)'s **"One instruction
  in, one instruction out"**, together with the canonical-encoding property its
  Verification section derives from it, **for the head of a fused window only**.
  A fused head's encoding depends on the instructions after it, so
  `encode(decode(b)) == b` no longer holds of that row and a window's interior
  pcs are not branch targets. Bytecode pc is still IR pc, every `Inst` still has
  exactly one row, and `decode` of every row, fused head included, is still the
  `Inst` at that pc
- Elaborates:
  [ADR 0058](0058-collection-apis-lower-through-typed-run-intrinsics.md)'s
  `growable-ensure`, `run-store` and `growable-commit` families and its sentence
  "Verification requires every path which commits units to initialize them
  first. A backend may fuse ensure, copy and commit after that fact is proved";
  and [ADR 0055](0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md)'s
  division of fusion into common optimization and VM-only superinstructions
- Preserves: [ADR 0040](0040-a-bound-outlives-its-backend.md)'s bounds, in
  their own units; ADR 0052's growth policy, unique finish and zeroed spare
  capacity
- Changes no source-language API
- Implementation status: **stages 0 through 9 are merged**, as pull requests
  [#413](https://github.com/myuon/cove/pull/413) through
  [#422](https://github.com/myuon/cove/pull/422). Every number below is
  measured against `720df04`, the commit before the first of them. The
  Adoption section names which stage each pull request is, what the series
  measured end to end, where the implementation decided differently from the
  Decision above, and what is left open

## Context

ADR 0058 named the protocol a growable append is made of:

```text
ensure capacity
initialize the suffix with store or run-copy
commit initialized units
```

and moved the public collection methods into Cove. What it did not yet move
is the append itself. `Vector.push` is a Cove function whose body is one
library-only call, `core.vectorPush`, and that call lowers to one composite
instruction, `Inst::GrowablePush{Words}`. The builder's `append`,
`appendSlice` and `appendByte` are the same shape over
`GrowablePush/GrowableExtend{PackedBytes}`, and interpolation emits those two
instructions directly. The public method is Cove; its orchestration is hidden
in one IR instruction.

`Inst::GrowableExtend`'s own documentation says why, and the reason is
ADR 0041's:

> The encoding is 1:1 with this IR, so a split cannot be undone at the encoder,
> and nothing yet combines two ensures or fuses a slice into an append — a split
> form would pay three dispatches for what one does.

That was tried. Splitting an append into ensure, copy and commit directly in
the executable stream added about six VM dispatches per append and measured
**+0.42% on covefmt**, bought no optimization, and was correctly dropped. What
the experiment showed is not that the protocol must stay opaque. It is that
there was no layer between verified IR and its encoding in which a split could
be put back together — and ADR 0041's bijection is what made there be none.

The composite form has costs of its own, and they have accumulated:

- **Both native code generators rebuild the same fast path by instruction.**
  `vector_push` and `byte_push` exist in `crates/cove-native/src/template.rs`
  and `crates/cove-native/src/compile.rs` alike, each emitting a null check, a
  layout check, a capacity compare, a stride write and a length increment for
  one nominal instruction, and each with a cold path that redoes the whole push
  in `crates/cove-runtime/src/vm/exec/native.rs`, which reads `code[pc]`
  expecting exactly `GrowablePush`.
- **String policy lives inside a run instruction.** `GrowableExtend` checks
  that `from` and `to` are character boundaries, which is `appendSlice`'s range
  policy — #378's Q6, recorded and unresolved.
- **The commit rule is written down and checked by nobody.** `GrowableExtend`'s
  documentation states when a future `growable-commit` would be sound. No
  verifier implements it, because no instruction needs it.
- **Without a commit, length is published by a field store.**
  `core_extend_keyed` in `crates/cove-ir/src/lower/core.rs` copies a keyed run
  into a vector and raises its length with a plain `StoreField VECTOR_LEN`,
  which is exactly the unrestricted publication issue #409 forbids — and it is
  hot on cq.

### What runs today

Build `checked` with `--features template`; `cove run covefmtBench --files-root
.. --backend vm|native --boundary --stats` from `examples/`, and cq's
`revenue-summary` over a fixed 20,000-record `cqSample`.

| | covefmt VM | cq 20k VM |
| --- | ---: | ---: |
| dispatches | 733.4M | 264.8M |
| `growable-push.words` | 5,395,970 (~95 sites; 504,371 grew) | 196,677 (`map.inserted`) |
| `growable-extend.bytes` | 513,517 (295,496 `append`, 215,730 `appendSlice`, 2,291 interpolation) | 600,004 (all interpolation) |
| `growable-alloc.bytes` / `run-finish.bytes` | 162,871 | 300,007 |
| `run-finish.words` | 24,131 | 196,677 |
| `growable-truncate.words` | 14,008 | 0 |
| `run-copy.words` + `store-field LEN` (keyed extend) | — | 393,354 |

Native growable helper calls: **1,352,737** on covefmt and **1,200,026** on cq.

Two facts in that table shape the decision. A push is the most executed
growable operation by an order of magnitude, so whatever the VM pays per push
is paid five million times on covefmt; a split that costs dispatches there is
the +0.42% again, larger. And an inlined builder `append` already dispatches
twelve instructions — an address, a string, a load, a constant, a length, the
extend, a unit and five clears — of which the append is one: the composite
instruction was never what made an append cheap to dispatch.

### What the two superseded decisions assumed

ADR 0052 put `append-byte` and `append-bytes` in the IR for a stated reason:
"A bulk append is one dispatch, not a loop of interpreted stores." That reason
still holds and this ADR keeps it. What no longer holds is that the IR is the
only place it can be kept. Under ADR 0041 an IR instruction *was* a dispatch,
so a property of dispatch had to be a property of the IR; ADR 0055 has since
divided fusion into what removes semantic work, which belongs to the shared
optimizer, and what removes only dispatch, which belongs behind the VM. A
composite push removes only dispatch.

ADR 0041's bijection was chosen for what it saves — spans, local ranges and
switch targets indexed by pc with no mapping, and a debugger whose mapping is
the identity — and it was stated as a consequence of the operand invariant
rather than as a goal. Every one of those savings survives a fused head whose
tail rows are kept in place. The one thing that does not survive is that a row
encodes alone, and that is exactly the layer the dropped experiment was
missing.

## Decision

### Shared IR spells an append as ensure, a typed write, and commit

Three instructions join `cove_ir::Inst`:

```rust
GrowableEnsure { owner: Slot, additional: Slot, storage: Storage },
GrowableCommit { owner: Slot, count: Slot, storage: Storage },
RunStore { run: Slot, index: Slot, src: Slot, storage: Storage },
```

- **`GrowableEnsure`** makes room for `additional` more units. If they fit it
  does nothing; otherwise it grows the store under ADR 0052's policy, copying
  the live prefix and replacing the owner's store reference. It may allocate
  and collect, and it writes no slot. Arithmetic is checked before mutation.
- **`GrowableCommit`** raises the logical length by `count` over units the
  block has written. It **keeps a runtime bound check**,
  `0 <= count && length + count <= capacity`, because the loader-side bytecode
  verifier has no dataflow and must stay safe against arbitrary bytes; the
  compare is the whole cost of that guarantee.
- **`RunStore`** writes one unit of a packed run and exists for
  `Storage::PackedBytes` only, as `RunLoad` does; the byte is checked to be
  `0..=255`. A word write is `StoreElem`, which `core.vectorStore` already
  lowers to. A bulk write is `RunCopy` into the store, unchanged: memmove
  semantics, proportional charge, bounded chunks.

Length and store are read with the `LoadField{VECTOR_LEN | BUFFER_LEN}` and
`LoadField{STORE}` that already exist. The encoding gains
`GrowableEnsure{Bytes,Words}`, `GrowableCommit{Bytes,Words}` and
`RunStoreBytes`, split by storage as every run opcode is.

`Inst::GrowablePush` and `Inst::GrowableExtend` **leave shared IR**, together
with their core lowering arms, verifier cases, runtime entry points and native
`GrowableOp` variants. A later change that renames them rather than deleting
them does not satisfy this ADR.

### The orchestration is standard-library Cove

The standard library gains library-only core calls, none importable by a
user program:

```text
core.vectorEnsure<T>(items: Vector<T>, additional: Int)
core.vectorCommit<T>(items: Vector<T>, count: Int)
core.bytesEnsure(buffer: ByteBuffer, additional: Int)
core.bytesStore(buffer: ByteBuffer, at: Int, byte: Int)
core.bytesCopy(buffer: ByteBuffer, at: Int, text: String, from: Int, count: Int)
core.bytesCommit(buffer: ByteBuffer, count: Int)
core.vectorCopyFromSet / core.vectorCopyFromMap(out, at, run, from, count)
core.refuseByteRange(text: String, from: Int, to: Int)
```

The keyed copies take the destination offset `at` rather than reading the
owner's length themselves. A window's head row is the length read, and a copy
that read the length would read it *after* the ensure, where the fact the
verifier needs — `at ≡ len(o)` before any growth — is no longer available.

and the algorithms are written over them:

```cove
export fn push<T>(items: Vector<T>, value: T) {
  let at = core.vectorLength(items)
  core.vectorEnsure(items, 1)
  core.vectorStore(items, at, value)
  core.vectorCommit(items, 1)
}

fn appendText(buffer: ByteBuffer, text: String) {
  let count = core.byteLength(text)
  let at = core.bytesLength(buffer)
  core.bytesEnsure(buffer, count)
  core.bytesCopy(buffer, at, text, 0, count)
  core.bytesCommit(buffer, count)
}
```

`appendByteInto` is the same with an ensure of one, a `bytesStore` and a commit
of one. `appendText` and `appendByteInto` take the buffer by value, not
`var self`, so interpolation can call them as it calls `std.int.renderInto`,
and so they stay on the mandatory-expansion path rather than the ordered one.
Keyed extend is `at`, `vectorEnsure(out, n)`, `vectorCopyFromMap(out, at,
entries, from, n)`, `vectorCommit(out, n)`, and its `StoreField VECTOR_LEN`
goes.

`std.map` and `std.set` write the protocol out where they place an element,
rather than calling `Vector.push`: `placeAt`'s run is a parameter and not a
writable place, so a call would have to hand the owner over and get it back.
The two sites are the protocol, not a second spelling of it — the same four
core calls in the same order, and `legalize` recognises them as the same
window.

**`appendSlice`'s range policy moves into Cove.** One `if` per question, in
`sliceBytes`'s shape, and each refusal calls `core.refuseByteRange`, an
`IntrinsicCall` that always raises — an intrinsic rather than a Cove call, so
the body remains an inlinable leaf, and blamed at the caller's site under
ADR 0058. This resolves #378's Q6. It is measured when it lands, and if its VM
cost is above noise it falls back to a checked copy that keeps the test below
the library.

The five questions and the window live in a private `appendRange(buffer, text,
from, to)`, which `appendSlice` calls, rather than in `appendSlice`'s own
body. The checks and the window are one block, and keeping them one function
is what lets `legalize` see the window whether or not the public method was
expanded. `core.refuseByteRange` lowers to `Intrinsic::StringRefuseByteRange`,
whose receiver is `String` although no `String` method is named
`refuseByteRange`, as `ValueRenderInto` and `ValueRefuseDuplicate` already
are. The fallback was not needed: the range policy in Cove measured −0.35% on
covefmt against the build that had it in the run instruction.

The division is ADR 0058's table, now true of the append: ranges, branches,
arithmetic and ordering belong to Cove; allocation, collection, the grow slow
path, bounded bulk copy, relabelling and UTF-8 validation stay trusted.

A protocol call whose answer nobody reads writes no `Unit`. Today
`unit_answer` always does, and five million pushes on covefmt each dispatch
one.

### The verifier checks a block-local reservation

`verify.rs` gains `check_reservations`, over facts it can establish without a
control-flow graph beyond block boundaries. The leaders computation and a
shared `Inst::writes()` move into `cove-ir`, because `inline::written`,
`frees` and `slot_facts` already enumerate writes three times.

1. **Facts.** `LoadField{dst a, obj o, at LEN}` gives `a ≡ len(o)`;
   `LoadField{dst s, obj o, at STORE}` read *after* an ensure gives
   `s ≡ store(o)`; `Int{dst, value}` gives a constant. A fact dies when its slot
   is written, and at the end of the block.
2. **Open.** `GrowableEnsure{o, n}` opens a reservation `{o, n}`. A second open
   reservation is a fault.
3. **Inside the window** only these are admitted: `Int`, `Bool`, `Unit`, `Tag`,
   `Float`, `Str`, `Copy`, `Arith(Imm)`, `Cmp(Imm)`, `Neg`, `Not`, `Convert`,
   `LoadField`, `LoadElem`, `RunLoad`, `Len`, `LayoutOf`, `Load`, `Clear`, and
   the reservation's own write — and none of them may write `o`, `a`, `s` or
   `n`. Every call, `IntrinsicCall` included, every heap or address store, every
   other growable instruction on any owner, every allocation, and every branch
   or terminator is refused. A branch target inside the window closes it.
4. **The write** is `StoreElem{obj s, index a}` or `RunStore{run s, index a}`
   with `n` the constant one, or `RunCopy{[s, a, _, _, c]}` with `c ≡ n`.
5. **Commit.** `GrowableCommit{o, m}` requires an open reservation on `o`, its
   write done, and `m ≡ n`; it closes the reservation.
6. **End of block.** A written reservation must be committed before the block
   ends. An unwritten ensure may lapse: room is not a value.

This is stricter than the provisional rule in `GrowableExtend`'s documentation,
which admitted any non-calling, non-collecting instruction between ensure and
commit. Nothing any sketch above lowers to needs more, and
`docs/PHILOSOPHY.md`'s "earn complexity through use" applies to a verifier as
much as to syntax. A
flow-sensitive rule is for the first producer that cannot be written in one
block.

Issue #409's safety contract, clause by clause:

- **Committed units are initialized on every incoming path.** A reservation is
  block-local, a branch target closes it, the write must precede the commit,
  and `m ≡ n ≡` the write's count.
- **A failed ensure or validation publishes nothing.** Ensure raises before the
  write; the commit is the only instruction that moves length and it is last.
  A `RunCopy` stopped part way leaves its bytes above the length, where they
  are spare room. `appendSlice`'s range checks run before the `let at`.
- **The destination is not visible before initialization.** Length changes
  only at commit. Keyed extend's field-store publication is removed by the
  stages below, and a lowering test pins that the only `StoreField` at a
  growable owner's length offset anywhere in the standard library is a
  constant nought. `core.vectorWithCapacity` keeps that one: it allocates a
  store and initializes the owner's header, and its length store publishes
  nothing because it publishes emptiness. Admitting `GrowableAlloc{Words}` so
  that even that store goes is stage 8's, and it is not done — see Adoption.
- **Word truncation clears every vacated reference.** `GrowableTruncate` is
  unchanged.
- **Allocation and growth keep the owner and source rooted.** Ensure is the
  only instruction in the window that may collect, its operands are frame
  slots and therefore roots, and the store is read *after* it, so no stale
  store survives a growth. Nothing else in the window allocates.
- **Overlapping copies keep memmove semantics.** `RunCopy` is unchanged.
- **Proportional work and cancellation stay bounded.** `RunCopy` keeps its
  chunks; a window is a constant number of instructions; growth is charged as
  the allocation and copy it is.
- **Finish enforces uniqueness and consumes the owner**, and **byte finish
  validates UTF-8** unless validity is proved: `RunFinish` and
  `cove-sema`'s uniqueness facts are unchanged.

`GrowableCommit`'s runtime bound is what is left for a stream that bypassed
this verifier: it cannot publish past capacity, and ADR 0052's zeroed spare
capacity means what it could publish is still a traceable value.

### One pattern definition, used by every consumer

A new module, `cove_ir::legalize`, is the **one documented definition** of the
windows a backend may treat as a unit. The encoded VM, the template code
generator, the Cranelift code generator and the inliner's `THIN` count all ask
it; none of them recognises `Vector.push` or `StringBuilder.appendSlice` by
name, and none keeps a private copy of the shape.

The canonical windows, after inlining, tails, frees and branch fusion, are:

```text
Push(S):   LoadField{at <- o.LEN}  Int{n <- 1}  GrowableEnsure{o, n, S}
           LoadField{st <- o.STORE}
           StoreElem{st[at] <- src} | RunStore{st[at] <- src, Bytes}
           [Clear{st}]  Int{m <- 1}  GrowableCommit{o, m, S}

Append(S): LoadField{at <- o.LEN}  GrowableEnsure{o, n, S}  [Int{z <- 0}]
           LoadField{st <- o.STORE}
           RunCopy{[st, at, src, z | from, n], S}
           [Clear{st}]  GrowableCommit{o, n, S}
```

The exact rows are whatever the standard-library source lowers to, and a test
pins that **every** protocol site in `std/` is recognised, so that fusion
cannot silently stop firing when a body changes.

A window that does not match is not an error. It is correct primitive IR, and
every backend runs it as such.

`is_thin_library` counts a recognised window as **one step**. The new bodies
are seven to ten instructions, over `THIN`'s four, and without this `push`
and the builder's appends would stop being mandatory expansions — the one
regression this design would otherwise make certain.

### The encoded VM fuses a window's head and keeps its tail

`encode_function` runs after every IR pass and after verification. Where
`legalize` recognises a window it changes **only the head row's opcode**, to
`FusedPushWords`, `FusedPushByte`, `FusedAppendBytes` or `FusedAppendWords`;
the head keeps its own `a`, `b`, `c` and payload. The tail rows stay encoded
as the primitives they are — shadow rows the fused arm reads operands from at
`code[pc + 1 .. pc + k]`. The arm executes the window, writes every frame slot
the primitives would have written (`at`, `n`, `m`, `st`), and advances
`pc += k`.

What this keeps:

- **pc is IR pc.** Every `Inst` still has one row. `Function::spans`,
  `Local`'s pc ranges and `Table::targets` keep their meaning with no mapping.
- **`decode` is still lossless per row.** `decode` of a fused head is the head
  primitive; the round trip in `crates/cove-cli/tests/bytecode_corpus.rs`
  becomes window-aware rather than being dropped.
- **An error has the span it had.** A failing constituent reports at its own
  pc, and the machine synchronises to the ensure's pc before a growth can
  collect.

What it gives up, and why that is the part of ADR 0041 superseded: a head's
encoding is no longer a function of its own `Inst`, so `encode(decode(b)) == b`
fails for that row, and "any pc in `[0, code.len())` is an instruction
boundary" is no longer true inside a window. `bytecode::verify` therefore
**re-matches every fused window** against the same definition, and refuses a
mismatched tail or a branch or table target in a window's interior.

**Fusion is decided at run time, per window, and is skipped whenever a question
falls inside it.** The arm fuses only when
`instructions + WINDOW_TAIL < next_check`, where `WINDOW_TAIL` is the *longest*
window's tail — `legalize::MAX_ROWS - 1`, eight — and not this window's own
`k - 1`. The bound has to be decidable before the rows are decoded, and this
window's `k` is not known until its commit has been found; a constant that
bounds every window is. Otherwise the arm executes the head as its primitive
and the tail dispatches normally, which it can because the tail was never
removed. An installed debugger or profiler makes `next_check` the next
instruction, so a breakpoint, a step and `--profile` see semantic instructions
and never a fused one. The price of the conservative bound is measured and
small: between 0.4% and 0.8% of windows decline on the two programs, and they
run as rows, which is correct.

Each pattern gets its own fast path, out of line and reached first:
`fused_push` for both push patterns, `fused_append_bytes` and
`fused_append_words` for the two appends. Each writes every frame word the
rows would have written, in their order and with their values, so a frame, a
heap and a debugger reading after the window cannot tell it was fused; each
declines — returning to the general `fused_window` — when the store has no
room, so a growth finishes through `fused_tail` and the grow policy stays in
one place. They are separate calls rather than arms inlined into `dispatch`
because inlining them there enlarged the dispatch frame: a push measured 118 ms
inlined against 94 ms out of line on two million `Int` pushes, and the
alternating native-tier chain's stack margin is about fifty kilobytes.

**Fuel counts semantic instructions.** A fused window charges `k`, not one.
Fuel must not depend on whether fusion fired, because whether it fired depends
on how close the window was to a safepoint; and native execution already
charges static IR counts per block. This is a one-time, documented rise in
`fuel_spent` against today's composite instructions — on covefmt, roughly five
extra instructions per push over five million pushes, estimated here as on the
order of 4%. Measured over the whole series it is **+4.92%**, 746,631,783 to
783,352,704, and on cq **+0.15%**. Dispatches were expected not to move; they
*fell*, by 0.54% on covefmt and 2.47% on cq, because a fused window is one
dispatch where the composite instruction was one dispatch inside a Cove call
the inliner now has less to expand.

`cove run --boundary` reports semantic instructions, dispatches, and fusions
fired per pattern, separately.

### Native admits each primitive and emits the window's fast path

`subset::supported` admits `GrowableEnsure`, `GrowableCommit` and
`RunStore{Bytes}` individually, so migration adds **no native refusal** and no
native-to-VM crossing:

- an ensure emits `additional <= capacity - length` as an unsigned compare, so
  a negative count goes cold, and a cold `GrowableOp::Ensure{Bytes,Words}` that
  runs `growable_ensure` and rejoins;
- a commit emits the add and a cold refusal;
- a byte store emits the write and a cold refusal.

At block emission both generators ask `cove_ir::legalize`. A `Push` window
emits today's fast path — the checks, the stride write, the length increment —
plus the slot writes the window makes, with a cold path that ensures and
rejoins rather than redoing the push. An `Append` window emits the ensure fast
path, the `RunCopy` helper and an emitted commit. `WordPush` and `BytePush` in
`subset.rs` become window decoders over the same definition, and the cold
helpers in `native.rs` key on the ensure instruction rather than on
`code[pc]` being a `GrowablePush`.

A runtime helper is entered only for growth, proportional bulk work,
validation or refusal. ADR 0055's rule holds in both directions: native does
not manufacture a fused opcode to split again, and the fused VM opcodes are
VM-private.

### The oracle stages an uncommitted suffix

The tree-walking evaluator in `crates/cove-runtime/src/builtins.rs` has no
capacity, so it models the protocol rather than memory. A byte buffer and a
vector gain a `staged` suffix: an ensure checks liveness only; a store or copy
at `at == length + staged.len()` stages units; a commit moves exactly `n`
staged units into the committed contents. The oracle therefore disagrees,
loudly, with a lowering that commits what it did not write.

### Truncate, alloc and finish stay composite

- **`GrowableTruncate`** is commit's inverse and runs 14,008 times on covefmt
  and never on cq. Splitting it needs a typed clear and a second verifier
  relation, and nothing would read the pieces. Only its message becomes free of
  method names.
- **`GrowableAlloc`** holds a temporary root across the allocation of the owner
  and its store; the window between the two cannot be written as primitives
  without exposing an owner with no store to a collection.
- **`RunFinish`** validates, relabels and consumes as one transition, and each
  piece alone is unsound: a relabel without the consume leaves a second holder,
  and a consume without the validation publishes a String that is not text.

None of the three is renamed. A rename is churn a reader has to map, for no
change in meaning.

## Adoption

Each stage lands green on its own, with the full gate and `cove test` over
`examples/`.

0. **This ADR, and tooling** — [#412](https://github.com/myuon/cove/pull/412)
   and [#413](https://github.com/myuon/cove/pull/413). `--boundary` reports
   helper counts per `GrowableOp` and `RunOp`; a `--profile-rows` flag replaces
   editing `PROFILE_ROWS` in source.
1. **Primitives, no producers** —
   [#414](https://github.com/myuon/cove/pull/414). The three `Inst` variants,
   their opcodes and VM arms, `check_reservations`, native admission and
   primitive lowering, and `cove_ir::flow::{leaders, targets, writes}`. Gate:
   every count byte-identical; verifier accept and reject tests for each
   numbered rule; VM and native agree, including a collection during growth.
2. **Legalization and fused heads** —
   [#415](https://github.com/myuon/cove/pull/415). `cove_ir::legalize`, the
   four fused opcodes, the bytecode verifier's re-match, boundary reporting.
   Gate: fused and unfused agree in memory, errors, spans and fuel; an
   installed debugger disables fusion; an interior target is refused.
3. **Native window emission** in both code generators, with the
   `cranelift,template` agreement test —
   [#416](https://github.com/myuon/cove/pull/416).
4. **`Vector.push`** for vectors, maps and sets through the protocol;
   `core.vectorEnsure/Commit`; oracle staging; `THIN` counting windows;
   `core.vectorPush` deleted — [#417](https://github.com/myuon/cove/pull/417).
   Gate: fused `Push(Words)` fires 5,395,970 times on covefmt; dispatches
   unchanged; native helper calls and wall time no worse. Measured: 5,360,296
   of 5,395,970 fused, dispatches −1,007,264, machine code +21,802 bytes,
   covefmt VM −1.35%.
5. **Byte push**: `appendByte`, `int.renderInto`, interpolation's one-byte
   literals — [#418](https://github.com/myuon/cove/pull/418), with stage 6.
6. **Whole-string append**: the builder's `append` and interpolation —
   [#418](https://github.com/myuon/cove/pull/418).
7. **`appendSlice`'s range policy in Cove**, with `core.refuseByteRange`, or
   the checked-copy fallback if its VM cost is above noise —
   [#419](https://github.com/myuon/cove/pull/419). The fallback was not needed.
8. **Keyed extend** through ensure, copy and commit —
   [#420](https://github.com/myuon/cove/pull/420), and then a word-append fast
   path the plan had not asked for,
   [#421](https://github.com/myuon/cove/pull/421), because stage 8 gave
   `Pattern::AppendWords` its first producer and fusing it through the generic
   arms cost 0.66% until it had one. The second half of stage 8 —
   `GrowableAlloc{Words}` admitted for `core.vectorWithCapacity` — is **not
   done**; see "What is left open" below.
9. **Deletion** of `Inst::GrowablePush`, `Inst::GrowableExtend` and everything
   that exists for them, with the lowering test that no length is published by
   a field store — [#422](https://github.com/myuon/cove/pull/422). The opcode
   table falls from 179 to 176 and `GrowableOp` from twelve variants to nine.
10. **Re-profile**, reported against the table above — issue #409's step 10,
    and the tables below.

Every migrating stage is held to issue #409's gates: identical results and
diagnostics on the AST, VM and native backends; GC, aliasing, consumed-owner
and invalid-UTF-8 tests; `responsiveness.rs` and fuel bounds green; no new
native refusal or crossing; covefmt and cq measured with interleaved builds on
a fixed `--files-root`, with the VM against an ablation rather than against a
number, because covefmt's rebuild noise floor is about one per cent; native
fast-path wall time not worse; emitted IR showing the protocol; and a report
of dispatches, fusions, helper calls, allocations, allocated words and
machine-code size.

### What the whole series measured

Two builds, `720df04` and the tip after #422, each in its own target directory
with `--features cove-cli/template,cove-bench/template`, run interleaved on one
fixed `--files-root` copy of the tree. Counts are deterministic and taken once;
wall times are medians of interleaved rounds.

| covefmt, VM | `720df04` | after #422 | |
| --- | ---: | ---: | ---: |
| semantic instructions | 738,943,455 | 775,664,376 | +4.97% |
| dispatches | 738,943,455 | 734,969,979 | −0.54% |
| `fuel_spent` | 746,631,783 | 783,352,704 | +4.92% |
| allocations | 3,890,981 | 3,890,981 | ±0 |
| allocated words | 48,365,193 | 48,365,193 | ±0 |
| emitted IR | 11,378 / 211 fns | 12,586 / 214 fns | |
| std `Call` sites unexpanded | 45 | 45 | ±0 |

| cq revenue-summary, 20k, VM | `720df04` | after #422 | |
| --- | ---: | ---: | ---: |
| semantic instructions | 264,820,020 | 265,212,294 | +0.15% |
| dispatches | 264,820,020 | 258,285,837 | −2.47% |
| `fuel_spent` | 269,371,851 | 269,764,125 | +0.15% |
| allocations | 1,433,567 | 1,433,567 | ±0 |
| allocated words | 9,614,096 | 9,614,096 | ±0 |
| std `Call` sites unexpanded | 40 | 40 | ±0 |

Windows fused, per pattern, after #422: covefmt `push.words` 5,402,548,
`push.byte` 1,191, `append.bytes` 514,045, `append.words` 0; cq `push.words`
195,401, `push.byte` 33, `append.bytes` 600,004, `append.words` 391,679. The
dispatch fall is those windows: the protocol adds rows and the fusion removes
more of them than the composite instruction ever was.

Native, on the template code generator:

| | covefmt `720df04` | covefmt after | cq `720df04` | cq after |
| --- | ---: | ---: | ---: | ---: |
| compiled / refused | 205 / 6 | 208 / 6 | 85 / 24 | 89 / 24 |
| machine code | 861,737 B | 922,417 B | 269,626 B | 376,834 B |
| VM → native | 153,122 | 153,122 | 953,363 | 953,363 |
| native → VM | 148,587 | 148,587 | 260,000 | 260,000 |
| `growable` helpers | 1,363,759 | 848,179 | 1,200,026 | 600,026 |
| `run_copy` helpers | 1,248,538 | 1,764,118 | 753,354 | 1,353,354 |
| the two together | 2,612,297 | 2,612,297 | 1,953,380 | 1,953,380 |

The last row is the honest reading of the two above it. No bulk work was
removed: what a composite `GrowableExtend` did inside the growable helper a
window now does through `RunCopy`, and the same number of helper calls is made
either way. What changed is that the capacity test and the length increment are
emitted code on both sides of it. Machine code is +7.0% on covefmt and +39.8%
on cq, the latter because cq's four newly compiled functions are the standard
library's appends and keyed copies, which were a helper call before.

Refusals are the same set of functions with the same reasons on both programs;
only the pcs move, because the bodies are longer. No crossing count moved.

The per-op breakdown of the native helper rows exists only after #413, which
added it, so the `720df04` column is a total. After #422 it is: covefmt
`Alloc` 163,198, `Finish` 163,198, `TruncateWords` 14,083, `EnsureWords`
507,700; `CopyBytes` 515,580, `SliceWords` 270,856, `SliceBytes` 977,682. cq
`Alloc` 300,006, `Finish` 300,006, `EnsureWords` 14; `CopyBytes` 600,000,
`CopyWords` 393,354, `SliceBytes` 360,000. Every remaining `growable` call is
an allocation, a finish, a truncate or a growth — never an append.

Wall time, `execute=` medians, with `benches/arith` as the layout control
because it executes no growable instruction at all. Two independent
interleaved sets were taken, of thirteen and of fifteen rounds, and the second
also paired each round's two runs against each other so that drift over the
set cancels:

| | `720df04` | after #422 | Δ (13) | Δ (15) | paired (15) | spread |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| covefmt VM | 4,843.5 ms | 4,905.7 ms | +1.23% | +1.28% | +1.20% | 1.2% / 1.5% |
| cq 20k VM | 2,213.1 ms | 2,196.7 ms | +0.23% | −0.74% | −0.62% | 4.2% / 3.8% |
| covefmt native | 1,990.9 ms | 1,996.6 ms | −0.19% | +0.28% | +0.17% | 5.7% / 2.8% |
| cq 20k native | 1,450.7 ms | 1,449.8 ms | +0.43% | −0.06% | +0.22% | 3.7% / 3.0% |
| `arith` VM (control) | 47.1 ms | 48.8 ms | **+3.18%** | **+3.50%** | **+3.22%** | 5.7% / 3.8% |

**The control moved further than anything it controls, and it reproduced.**
`arith` runs no append and executes not one of the new instructions; its
+3.2% to +3.5% is the dispatch loop's own layout, which this series changed by
adding four opcodes and deleting three. Every VM workload moved *less* than
that, in both sets. So the right statement about the append is "not worse",
not "faster": the series bought the protocol and the dispatch counts, and paid
nothing above what a program with no appends in it paid for the same rebuild.
Native is flat on both programs — ±0.3% across both sets, against spreads of
3% — which is the gate that mattered for the fast path. cq's VM figure is the
one that will not hold still: +0.23% in one set and −0.74% in the other, on
counts that are identical to the digit.

### Where the implementation decided differently

- **The three primitive VM arms are one out-of-line call, and each pattern's
  fast path is another.** Written inline into `dispatch` they enlarged its
  frame enough to overflow `native_tier`'s three-hundred-frame alternating
  chain, and a push measured 118 ms inlined against 94 ms out of line. The
  dispatch loop's frame is a budget, and it is spent on the loop.
- **The fusion bound is the longest window's tail, not each window's own**, as
  the Decision now says; it declines 0.4% to 0.8% of windows.
- **Native's rejoin re-asks the room question** rather than trusting
  `growable_ensure`'s postcondition, so that a disagreement between the emitted
  test and the helper is one more helper call and not a write past capacity.
- **`appendSlice` stopped being expanded at cold sites.** Its body is about
  thirty rows, over `inline::LIMIT`'s sixteen and under `HOT_LIMIT`'s
  forty-eight, so hot sites expand and cold ones call. The unexpanded-call
  count did not move.
- **A refusal from a byte store is worded as the instruction, not the method.**
  `run-store`'s value is out of range where `appendByte`'s argument used to be,
  because the check is now the instruction's.
- **`cove debug`'s breakpoint rule changed**: a line's own instruction wins
  over the start of an expansion at that line. Protocol rows made the older
  rule pick the wrong one.

### What is left open

- **A window near a safepoint declines and runs as rows.** `WINDOW_TAIL` is the
  longest window's tail, so 0.66% of covefmt's pushes, 0.70% of its builder
  appends, 0.78% of its `appendSlice`s and 0.43% of cq's keyed extends decline.
  Using each window's own `k` needs the commit found before the bound is
  decided.
- **A growth finishes through `fused_tail`.** All three fast paths decline when
  the store has no room; whether finishing a grown window on the fast path
  buys anything has not been measured. On covefmt 504,049 of 5.4 M pushes grow.
- **`GrowableAlloc{Words}` for `core.vectorWithCapacity` is not done.** It is
  the second half of stage 8, it touches `admit_storage`, the encoder, the
  decoder, the VM's alloc arm, both code generators and the oracle, and it
  would move the allocated-word count of cq's 180,000 constructions. It belongs
  in a change that measures it.
- **Native pays for a window's frame writes where they are dead.** In
  `std.int.renderInto`'s loop nothing reads `at`, `n`, `m` or `st` after the
  window, and both code generators write them anyway: the Int-piece rows of
  `benches/builtincall` are about 0.9 ns a digit dearer on native. A
  liveness-directed elision in `cove-native` would remove it.
- **Nothing counts how often a fast path declines in a real run.**
  `Machine::fused_fast` is `#[cfg(test)]` and `--boundary`'s `fusions` cannot
  tell a fast path from `fused_tail`. A row that could would have priced #420's
  37 ns a window without building a third binary.
- **cq's VM wall time is layout-sensitive beyond this change's reach.** Three
  interleaved sets of the same pair of builds have put it at +0.23%, at −0.74%
  and — in #422's own set — at +3.76%, against `arith` controls of +3.18%,
  +3.50% and +1.76%. The counts are identical to the digit in every one of
  them; what moves is the dispatch loop's code layout, and nothing in this
  series can hold it still.

## Alternatives considered

**Keep the composite instructions, renamed.** `BufferPush` is `GrowablePush`
with a new name: the fast path still lives in two code generators, the policy
still lives in a run instruction, and the optimizer still sees nothing. Issue
#409 names this as not done, and it is not.

**Split in the executable stream and accept the dispatches.** Measured: about
six more dispatches an append, +0.42% on covefmt, for nothing that used the
split. On push the multiplier is five million. That is a slowdown with no
purchase, which ADR 0055 gives no licence for.

**Fuse in shared IR, as ADR 0054 fused a comparison into its branch.** A
`CmpBranch` removes a slot write every backend would otherwise make. A fused
push removes only dispatch, so as a shared instruction it is the nominal
`GrowablePush` under another name, which native must split again and which
ADR 0055 places behind the VM.

**Fuse into one row and remap pcs.** Compresses the instruction stream, and
breaks `Function::spans`, `Local` ranges, switch tables and the debugger's
identity mapping, all of which would need a table ADR 0041 was written to
avoid. Shadow tails keep every one of them for the cost of rows that are
present but not dispatched.

**Charge one fuel per fused window.** Then `fuel_spent` would depend on whether
a window happened to sit near a safepoint or a debugger was attached — the same
program, a different number, for a reason no reader could see.

**Give the bytecode verifier dataflow instead of a runtime commit check.** It
moves a one-compare runtime check into a loader that ADR 0041 kept linear, and
the IR verifier already proves the invariant for everything the compiler
emits.

**Let commit be a field store with a verifier rule.** Issue #409 forbids it,
and the reason is the rule's reach: a `StoreField` is the instruction every
struct write uses, so the verifier would have to decide which of them is a
commit by the offset it writes — a nominal exception inside a general
instruction — and the field store has no place for commit's runtime bound.

## Consequences

- Shared IR shows the buffer protocol. `GrowablePush` and `GrowableExtend` are
  gone from it, and the collection method set no longer defines an instruction.
- `Vector.push`, the builder's appends, interpolation's appends and keyed
  extend are standard-library Cove; the character-boundary check leaves the
  run instruction.
- Common optimization can see ensure, write and commit — which is what
  ADR 0058's pipeline needs before it can combine two ensures or fuse a slice
  into an append. Neither is done here.
- The verifier checks a reservation rule where there was a comment.
- The encoded VM keeps one dispatch per recognised append and per push, and
  pays extra dispatches only where a window does not match or a debugger,
  profiler or safepoint is inside it.
- Bytecode pc is still IR pc. A fused head is the one row whose encoding
  depends on its neighbours, and the bytecode verifier knows it.
- `fuel_spent` on the encoded VM rises once, by the protocol's instructions,
  and says so in the stage that causes it.
- Both native code generators and the inliner consult one pattern definition,
  and native gains no refusal. Measured: the same refused functions, for the
  same reasons, on both programs.
- Native code grows, because a window is emitted where a helper call was:
  +7.0% on covefmt and +39.8% on cq, the latter over a small base.
- Frames gain a few temporary words per inlined site, for `at`, `n`, `m` and
  `st`. Measured: the widest frame in `bytecode_corpus` goes from 184 words to
  194, and its margin under the limit from 350× to 330×.

## What is not decided here

- **Combining adjacent ensures, or folding `sliceBytes` into an append.** This
  ADR makes both expressible and does neither.
- **A flow-sensitive reservation rule.** Block-local until a producer needs
  more.
- **Whether the fused VM opcodes outlive native adoption.** They are
  VM-private and may be deleted without touching shared IR.
- **Map and Set beyond keyed extend.** `inserted` and `removed` already reach
  the protocol through `Vector.push`; their own table policy is ADR 0059's.
