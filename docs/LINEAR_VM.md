# The linear-memory VM

This is the design [ADR 0034](adr/0034-one-physical-word-stack.md) decides,
written out at the level an implementer needs. It describes `cove-lir` — the
executable IR — and `cove_runtime::lvm` — the virtual machine that runs it.

It is a clean-room design. Nothing here is derived from the predecessor
`cove-ir`, `Vm` or `FrameVm`, and no instruction, storage region or naming
convention is carried over from them for compatibility. Where this document
and the predecessor agree, they agree by arriving at the same place, and
where they differ this document is the one that is being built.

The design was reviewed in [issue #240](https://github.com/myuon/cove/issues/240);
its answers are folded in here rather than left in the thread.

## What is kept and what is replaced

Kept, unchanged: the lexer, parser and AST (`cove-syntax`); name resolution
and type checking (`cove-sema`); the tree-walking interpreter
(`cove_runtime::interp`) as the semantic oracle; the public `Host` API and the
materialised `Value` boundary; the conformance corpus and its span, error and
trace expectations.

Replaced: everything between the checker and the answer.

## One linear memory

The machine owns one logical linear memory addressed in **words** of eight
bytes. A *linear address* is a word index into it. Two regions divide it:

~~~text
word 0                    STACK_WORDS                        ...
|-------- stack region --------|--------- heap region ---------|
~~~

- `STACK_WORDS` is a compile-time constant of the runtime. The stack region is
  reserved, not committed: the backing store grows on demand, but no heap
  object is ever placed below `STACK_WORDS`.
- A linear address below `STACK_WORDS` names a stack word. One at or above it
  names a heap word.

That comparison is the whole of the decoder. It is the only thing that knows
the two regions currently live in two Rust allocations, and it keeps working
unchanged when they are placed in one block, because no address changes value
when that happens. This is what ADR 0034 asks for when it says the IR and the
public API must not depend on which allocation holds a region.

Addresses are **indices, not pointers**. A region's backing store may be
reallocated as it grows and every live address stays correct.

### The stack region

A task has one stack region. A call frame is a contiguous run of words in it,
and a frame is named by its `frame_base`, a linear address. A slot is:

~~~text
memory[frame_base + slot]
~~~

There is no second stack. Parameters, locals, temporaries and captures are all
slots in one numbering, which is `Function::slots`.

### The heap region

The heap region holds every value that is not one word of scalar bits. An
object is a header word followed by its payload words:

~~~text
+0  header:  [ kind: u16 | layout: u16 | len: u32 ]
+1  payload word 0
+2  payload word 1
...
~~~

An object's linear address is the address of its header. A `Ref` word holds
that address, or `0` for "no object" — address `0` is a stack word, so it can
never be a real object and is free to mean null.

The allocator bump-allocates and reclaims with a non-moving mark and sweep.
Because it never moves an object, an address that points *into* an object —
which is what a `var` argument naming a field is — stays valid across a
collection.

## A word is untagged; metadata says what it means

A word is 64 bits with no tag. What it means comes from static metadata: the
instruction that reads it, and the function's per-slot table.

`Repr` is that table's entry. Every Cove value occupies exactly one word.

| `Repr`     | the word holds                                        | GC root |
|------------|-------------------------------------------------------|---------|
| `Unit`     | zero                                                   | no  |
| `Bool`     | 0 or 1                                                 | no  |
| `Int`      | a two's-complement `i64`                               | no  |
| `Float`    | an IEEE-754 double, bit-cast                           | no  |
| `Duration` | nanoseconds as `i64`                                   | no  |
| `Ref`      | a heap address, or 0                                   | **yes** |
| `Addr`     | a linear address of one mutable word                   | no  |
| `Host`     | an index into the run's host resource table            | no  |

The collector derives a frame's roots from `Function::refs`, a static bitmap
over the slot numbering: the slots whose `Repr` is `Ref`. It never inspects a
word to decide whether it is a pointer.

A slot's `Repr` is fixed for the whole function, and that is what makes one
static bitmap correct at every program counter. A slot **may** be reused by a
later value of the same `Repr` — otherwise a long body's frame grows with
every temporary it mentions — but never by one of a different `Repr`.

Frames are zeroed on entry, so a `Ref` slot that has not been written yet
reads as null rather than as whatever the previous frame left there.

### A static map must not become a leak

The map says which slots the collector *reads*. It cannot say when the value
in one stopped being needed, because that is a fact about a program point and
the map is a fact about a function. If it were left there, every object a
frame ever touched would be retained until the frame returned — which for a
server loop or a long `for` is the whole run.

The lowering answers liveness in the data instead. `Clear { slot }` writes
zero, and the lowering emits one at the end of the scope a binding belonged
to and at a temporary's last use, for every `Ref` and `Addr` slot that would
otherwise retain something. A dead reference slot holds null, the collector
traces nothing from it, and the object is unreachable at the next collection
rather than at the next return.

## The IR is a register machine

Every instruction names its operands by slot number and, where it produces a
value, its destination slot. There is no operand stack, no push and no pop.

This falls directly out of ADR 0034's "parameters, locals, temporaries and
captures share the one slot numbering": if a temporary is a slot, then an
instruction that consumes a temporary names a slot, and the thing an operand
stack exists to provide is already there.

Two consequences are worth naming, because they are the reason to prefer it:

- **The reference map is static.** A stack machine's set of live references
  changes as operands are pushed and popped, so its map is a function of the
  program counter. Here it is a function of the function.
- **The calling convention is direct.** A callee's frame begins where the
  caller's ends, so the caller writes argument *i* to the callee's slot *i*
  before transferring control. Nothing is pushed, permuted or copied
  afterwards. This is ADR 0034's "the caller writes each result to the
  callee's declared destination slot", implemented literally.

## The calling convention

- The callee's frame base is the caller's frame base plus the caller's frame
  size. Frames are contiguous and there is no per-call bookkeeping in the
  stack region beyond the frame itself.
- Slots `0..arity` are the parameters, in declaration order. A mixed list is
  not sorted into type groups; there are no type groups.
- The caller evaluates each argument in source order into its own temporary
  slot, then `Call` copies the listed argument slots into the callee's frame.
  The list is static: an `ArgsId` into a program-wide pool of slot lists.
- A `Call` names the destination slot in the *caller's* frame that the
  return value is written to. `Return` names the slot in the callee's frame
  that holds it.
- Captures follow the parameters: a closure's slots `arity..arity+captures`
  are filled from the closure object before the body runs.

## A place is a one-word address

An assignable expression lowers to a `Repr::Addr` word. There is no place
object, no place stack and no table of places.

- `AddrOfSlot { dst, slot }` — the address of a slot of the current frame.
- `AddrOfField { dst, base, field }` — the address of a field of the heap
  object in `base`.
- `AddrOfElem { dst, base, index }` — the address of an element.
- `Load { dst, addr }` and `Store { addr, src }` read and write through one.

A `var` parameter is an ordinary slot whose `Repr` is `Addr`. `bump(var total)`
passes the address of the caller's slot, and the callee's `Store` writes the
caller's word — which is the aliasing the language specifies, with no copy
back.

Keeping an interior address alive is the lowering's job, not the collector's:
the object an interior address points into is held in a `Ref` slot for exactly
the address's live range, and that slot is cleared when the address dies. The
base is retained for as long as the address can be used and no longer.

Stack addresses do not escape their frame: the checker's rules on `var` are
what make that true, and the lowering does not create an address it cannot
show is frame-local.

## The public `Value` is a boundary, not a store

`Value` is materialised only when crossing into or out of the host: a `Host`
API call's arguments and result, an entry's answer, a trace capture. Cove
calling Cove never builds one. There is no `Vec<Value>` operand area,
argument buffer, spill area or fallback path anywhere in the machine.

## The layout of each value family

A layout describes a *family*, not an instantiation. `Array<String>` and
`Array<Point>` are one layout, because a reference is a reference and what an
element actually is is a question its own object answers. `Array<Int>` and
`Array<Duration>` are two, because their `Repr`s differ and the boundary has
to know which. The lowering interns layouts, so the same shape is the same
`LayoutId` however many times the source writes it.

| value | shape | one layout per |
|---|---|---|
| `String` | `Str` | the program (`Program::str_layout`) |
| `struct T` | `Struct` | declared struct, fields in declaration order |
| `enum E` | `Enum` | declared enum, cases in declaration order |
| `Option<T>` | `Enum`, cases `None`, `Some` | payload `Repr` |
| `Result<T, E>` | `Enum`, cases `Ok`, `Err` | payload `Repr` pair |
| `Array<T>` | `Elements { growable: false }` | element `Repr` |
| `Vector<T>` | `Vector`, payload `[len, store]`, over an `Elements { growable: true }` store | element `Repr` |
| `Range` | `Struct { start: Int, end: Int, inclusive: Bool }` | the program |
| `MapEntry<K, V>` | `Struct { key, value }` | field `Repr` pair |
| a lambda | `Closure` | lowered lambda |
| `dyn`, `Any` | `Boxed` | the program |

A `Vector` is the one family with an indirection, and it earns it: its
identity is observable — `is` is defined for it, and mutation through one copy
is visible through every other — so growing must not move the object a program
is holding. The header stays put and the store beneath it is replaced. An
`Array` cannot grow, so it needs none of that and pays none of it: its elements
are in the object, one indirection nearer.

An enum object is one word of case index followed by the payload of the case
it is in, sized for the widest case. Which of those words are references
therefore depends on the case, and the collector reads word 0 to find out.
That is a fact about an object, answered by the object; it is not a static
kind per case, and no case is added because a program was refused.

## A builtin never calls back into Cove

`xs.map { it * 2 }` runs Cove code once per element. There are two places that
could live, and only one of them works here.

A builtin that invoked the closure itself would have to re-enter the dispatch
loop from inside a Rust function — which puts a Rust frame under every Cove
frame the closure creates, and gives back the property the loop was built to
have: that how deep a Cove program may nest is decided by the reserved stack
region and not by how large a Rust frame the interpreter compiled to. A `map`
over a `map` over a `map` would then be three Rust frames deep before the
program did anything.

So a closure-taking sequence method **lowers to a loop in the IR**. `map`
allocates the result and calls the closure per element with an ordinary
`CallClosure`; `filter`, `fold`, `each` and `sorted` are the same shape with a
different body. Three things follow, and all three are the reason:

- `builtins` stays a library over words with no reentry and no knowledge of
  frames. Nothing in it can call anything.
- The closure's calls are frames like any other, so depth, fuel, cancellation
  and the collector's roots all work without a second story for them.
- The element binding gets the same `Clear` discipline a `for` gets, because
  it *is* the same lowering.

## Ownership: what is in the heap and what is not

There is **one object heap per run**, shared by the run's task threads, not one
per task. `Shared` is an ordinary object in it, which is what lets two tasks
reach the same cell without a second value store. Allocation is synchronised
where correctness requires it; the single-task path is not optimised ahead of
a measurement that says it needs to be.

Each task may have a stack segment of its own. Every segment and the object
heap belong to the run's one logical linear address space.

Two things are named by a word but are not Cove-owned objects, and ADR 0034
carves out both: `Task` and `TaskScope` name **scheduler control state**, and
`Resource` names **Host-owned state**. Neither is a value store, and neither is
a place a Cove value may be put to avoid giving it a heap representation.

## Erasure: `dyn`, `Any`, and what the checker settled

A value whose type is *intentionally* erased — `dyn Trait`, a Host result a
schema declared `Any` — is a `Ref` to a `Boxed` object holding a `Repr` tag and
the word. An ordinary `Int` is an unboxed word; an `Int` explicitly erased to
`dyn` allocates. That is the right place to pay, and it buys one word per value
everywhere else.

A `Ty::Unknown` is not that. It is the checker declining, and a program the
checker declined about is a compile error. The target is *every valid checked
program lowers*, not *every checker abstention becomes runtime dispatch*.

There is therefore **no `Unsupported`, no admission predicate and no lowering
floor**. A construct the lowering has not been taught is a bug in the lowering.

## What is not decided here

Everything ADR 0034 lists as undecided: when the two backing allocations
become one block, how the block grows, the final allocator, a moving
collector, enum layout, and whether dispatch is later threaded or compiled.

## The stack limit is not a language fact

The stack region is reserved, so "too deep" is
`frame_base + frame_size >= STACK_WORDS`. The constant is an implementation
choice and is deliberately not part of the language, the IR or any public API:
the tree-walking oracle and this machine represent a frame differently and will
reach a limit at different depths, and requiring them to agree on a number
would be requiring one of them to represent a frame the other's way.

What both must do is fail the same *way*: a stack-overflow runtime error, with
a useful span, deterministically, within the run's configured memory budget.

## Naming, while both backends exist

`cove-lir` and `cove_runtime::lvm` are transitional names. The predecessor
`cove-ir`, `Vm` and `FrameVm` are frozen — fixed only where a fix is needed to
keep the oracle and the differential gate usable — and at the cutover commit
they are deleted and `cove-lir`/`lvm` take the names `cove-ir`/`vm`.

"Linear" describes the memory model, not the IR, which is a register IR. It is
not a name worth keeping once there is nothing to distinguish it from.
