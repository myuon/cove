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
its answers are folded in here rather than left in the thread. One of them
changed the representation after the first implementation was under way — a
value is a run of words rather than a single word — and the reasoning for
that is under "Why the one-word rule had to go", because it is the kind of
decision a later reader will want the argument for rather than the outcome.

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

`Repr` is that table's entry. **A `Repr` describes one word, not one value.**

| `Repr`     | the word holds                                        | GC root |
|------------|-------------------------------------------------------|---------|
| `Unit`     | zero                                                   | no  |
| `Bool`     | 0 or 1                                                 | no  |
| `Int`      | a two's-complement `i64`                               | no  |
| `Float`    | an IEEE-754 double, bit-cast                           | no  |
| `Duration` | nanoseconds as `i64`                                   | no  |
| `Ref`      | a heap address, or 0                                   | **yes** |
| `Addr`     | a linear address                                       | no  |
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

The lowering answers liveness in the data instead. `Clear` writes zero over a
value location, and the lowering emits one at the end of the scope a binding
belonged to and at a temporary's last use, wherever the location holds a
reference. A dead reference slot holds null, the collector traces nothing
from it, and the object is unreachable at the next collection rather than at
the next return.

## One slot is one word; one value may occupy several

This is the rule the whole representation turns on, and it replaces an
earlier one that said every value was a single word.

> **One slot is one eight-byte word. One value may occupy one or more
> consecutive slots.**

A **value location** is a base slot plus a layout, and the layout decides the
width, the offsets of the parts and the `Repr` of each word.

### Why the one-word rule had to go

Putting every struct behind one heap address makes an ordinary field-wise
copy an alias, because copying the word copies the address. The language says
assignment is a field-wise shallow copy and that structs have value
semantics, so something then has to *conceal* the alias — a sharing bit, a
copy-on-write protocol along every write path, and a rule for propagating
sharing to children.

All of that machinery exists only to undo a representation choice. Laying the
fields out where the value is makes the copy a copy: two words in, two words
out, and nothing to conceal. ADR 0001's semantics are represented directly
rather than reconstructed.

### What is inline and what is a reference

| value | representation |
|---|---|
| `Unit`, `Bool`, `Int`, `Float`, `Duration` | one inline word |
| a fixed-size `struct` | the consecutive words of its fields, inline |
| a fixed-size `enum` | a discriminant word and a payload region, inline |
| `String` | one word: a heap address |
| `Array`, `Map`, `Set` | one word: a heap address, immutable storage |
| `Vector`, `Shared` | one word: a heap address, **identity** storage |
| a closure | one word: a heap address of its environment |
| `dyn`, `Any` | one word: a heap address of a boxed value |
| a recursive layout | one word: a heap address, decided statically |

A `Vector` and a `Shared` are the two families whose storage is both shared
and mutable, so ADR 0001 makes a copy of either an alias — and their rule
must not leak into how a value-semantic struct is represented. That
generalisation is exactly the mistake this model was rewritten to avoid.

### A struct is its fields, in place

`Point { x: Int, y: Int }` is two words. `Line { from: Point, to: Point }` is
four: nesting is inline and recursive, so a `Line` has no indirection in it
at all and `l.from.x` is a slot offset known at lowering time.

A field access on an inline struct is therefore **not an instruction**. It is
arithmetic on a slot number, done by the lowering. Only a field of a *heap
object* needs a load.

### An enum is a discriminant and a payload region

Payload word 0 is the case index. The words after it are the payload of
whichever case the value is in, and the region is wide enough for every case.

Which raises the question the collector cannot be allowed to get wrong: a
payload word cannot be a reference in one case and an integer in another,
because one static bitmap has to be right whatever case the value holds.

So the payload offsets are **assigned per case, under one constraint: every
case that uses a given payload word agrees on its `Repr`.** The lowering
walks the cases in order and, for each payload word, takes the lowest payload
slot that is either unassigned or already assigned that same `Repr` and not
yet used by this case. For

~~~cove
enum E { A(Int, String), B(Float) }
~~~

`A` takes word 1 for its `Int` and word 2 for its `String`; `B` cannot use
either, so its `Float` takes word 3. The layout is
`[Int, Int, Ref, Float]`, four words.

Two things follow. Constructing a case **zeroes the payload region** it does
not fill, so a reference word belonging to another case reads null. And the
collector no longer reads the discriminant to decide what to trace: the
region's reference map is static, which is one fewer thing that can be wrong.

The cost is that a payload region can be wider than the widest case. That is
the price of a static map, and it is paid in words rather than in a run-time
question.

### Recursion is where boxing is decided

`struct Node { value: Int, next: Option<Node> }` has no finite inline width.
A layout that would contain itself is boxed: the field becomes one `Ref` word
and the object it names has the struct's own inline layout as its payload.

**Boxing is a static layout decision, not a representation for structs.** A
type is boxed because its layout demands it, and the lowering records which
ones, so nothing about a `Point` changes because a `Node` exists.

### A layout describes a family, and both regions use it

A layout describes a *family*, not an instantiation: `Array<String>` and
`Array<Point>` are one layout, because a reference is a reference. The
lowering interns them, so one shape is one `LayoutId` however many times the
source writes it.

A heap object's payload is a word array described by a layout in the same
way a frame's value location is. A struct inside a closure environment, an
array element or a boxed value is **inline in that payload**, and the
collector walks the payload's reference map exactly as it walks a frame's.

This is not a second value store. The stack region and the heap region are
regions of one linear memory, and both use one vocabulary of words, layouts
and reference maps.

| value | shape | one layout per |
|---|---|---|
| a scalar | `Word(Repr)` | `Repr` |
| `struct T` | `Struct`, fields inline | declared struct |
| `enum E` | `Enum`, discriminant then payload | declared enum |
| `Option<T>` | `Enum`, cases `None`, `Some` | payload layout |
| `Result<T, E>` | `Enum`, cases `Ok`, `Err` | payload layout pair |
| `String` | `Str` | the program |
| `Array<T>` | `Elements` | element layout |
| `Vector<T>` | `Vector`, `[len, store]` over an `Elements` store | element layout |
| `Set<T>` | `Members`, sorted and distinct | element layout |
| `Map<K, V>` | `Entries`, sorted by key | key/value layout pair |
| `Range` | `Struct { start: Int, end: Int, inclusive: Bool }` | the program |
| a closure | `Closure`, captures inline | lowered lambda |
| `dyn`, `Any` | `Boxed` | the program |

A `Set` and a `Map` are sorted runs rather than hash tables, because the
language says they iterate in ascending order and render that way: the order
is part of the value, not an implementation's leftovers.

A `Vector` is the one collection with an indirection inside it, and it earns
it: its identity is observable, so growing must not move the object a program
is holding. The header stays put and the store beneath it is replaced.

## Copying is a word-range copy

ADR 0001: *"Assignment and ordinary argument passing are field-wise shallow
copies."* Here that is one operation — copy the words of the value location —
and the layout says how many.

~~~text
Point   { x: Int, y: Int }         -> [x, y]                    2 words
Wrapper { p: Point, v: Vector }    -> [p.x, p.y, vector_ref]    3 words
~~~

Copying a `Wrapper` copies three words. The `Point` becomes independent
because its words were copied. The `Vector` stays shared because what was
copied is its address, and a `Vector`'s storage is shared by the language's
own rule. Both answers fall out of the same copy; neither needs a policy.

There is no sharing bit, no copy-on-write, no unsharing of a write path and
no propagation of sharing to children. A nested write updates the destination
words in place.

`let` and `var` are lowered the same way. ADR 0001 says they do not change
expression semantics, and Cove has no move semantics.

The IR may distinguish an unobservable transfer from a copy — a fresh
temporary need not be copied twice — but that is an optimisation, and
**correctness never depends on proving uniqueness.** A plain layout copy is
always the fallback, and a lowering that cannot tell emits one.

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
  caller's ends, so the caller copies each argument's words into the callee's
  frame before transferring control. Nothing is pushed, permuted or copied
  afterwards.

## The calling convention

- The callee's frame base is the caller's frame base plus the caller's frame
  size. Frames are contiguous and there is no per-call bookkeeping in the
  stack region beyond the frame itself.
- Parameters occupy the frame from slot 0 in declaration order, each taking
  the words its layout says. A mixed list is not sorted into type groups;
  there are no type groups. `Function::params` is the list of parameter
  layouts, and where each begins follows from the ones before it.
- The caller evaluates each argument in source order into its own value
  location, then `Call` copies each argument's words into the callee's frame.
  The list is static: an `ArgsId` into a program-wide pool of arguments, each
  of which is a base slot **and the layout of the location it names**. A slot
  says where a value begins and never how wide it is — a scalar is described
  by its slot's `Repr` and a reference by its object's header, and an inline
  struct or enum by neither — so a callee that is polymorphic over the values
  it is handed reads the layout rather than guessing. `Call` and `CallClosure`
  still take each parameter's width from the callee's `params`, because the
  frame being written is the callee's.
- A `Call` names the base slot of the destination *location* in the caller's
  frame; `Return` names the base slot of the answer in the callee's, and the
  machine copies the words `Function::returns` describes.
- Captures follow the parameters, each occupying the words its own layout
  says, copied out of the closure environment before the body runs.

## A place is a one-word address

An assignable expression lowers to a `Repr::Addr` word: the linear address of
the **first word** of a value location. Its width is static, so the address
alone is enough. There is no place object, no place stack and no table of
places.

- `AddrOfSlot` — the address of a slot of the current frame.
- `AddrOfField` — the address plus a statically known field-word offset.
- `AddrOfElem` — an element's address, at a statically known stride.
- `AddrOfPart` — a statically known word offset added to an address, which is
  what makes a place composable: the answer is again the address of the first
  word of a value location, so a field of a `var` parameter goes back through
  a load, a store or another of these with no second rule.
- A load and a store through one move the words the layout says.

A `var` parameter is an ordinary slot whose `Repr` is `Addr`, carrying the
address of the caller's own location. `bump(var total)` writes the caller's
words directly, which is the aliasing the language specifies, with no copy
back — and a nested write through one updates the destination words in place,
because there is nothing between the address and the words.

Keeping an interior address alive is the lowering's job: the object an
interior address points into is held in a `Ref` slot for exactly the
address's live range and cleared when the address dies. Stack addresses do
not escape their frame; the checker's rules on `var` are what make that true.

## The public `Value` is a boundary, not a store

`Value` is materialised only when crossing into or out of the host: a `Host`
API call's arguments and result, an entry's answer, a trace capture. Cove
calling Cove never builds one. There is no `Vec<Value>` operand area,
argument buffer, spill area or fallback path anywhere in the machine.

## Six cases, worked

### 1. Copying and mutating a flat struct

~~~cove
struct Point { x: Int, y: Int }
var a = Point(x: 1, y: 2)
var b = a
b.x = 7
~~~

`Point` is `[Int, Int]`, two words. `a` is at slots 0–1 and `b` at 2–3.

~~~text
int   s0 1
int   s1 2
copy  s2 s0 Point      ; two words: b is independent of a
int   s4 7
copy  s2 s4 Int        ; b.x is s2 + 0
~~~

`a.x` is slot 0 and nothing touched it. No bit was set and no protocol ran.

### 2. Copying and mutating a nested struct

~~~cove
struct Line { from: Point, to: Point }
var m = l
m.from.x = 7
~~~

`Line` is `[from.x, from.y, to.x, to.y]`, four words, no indirection.
`copy s4 s0 Line` copies four; `m.from.x` is slot 4 + 0. `l.from.x` is slot 0.

### 3. A struct containing a `Vector`

~~~cove
struct Wrapper { p: Point, v: Vector<Int> }
~~~

`[p.x: Int, p.y: Int, v: Ref]`, three words. A copy copies all three: the
`Point` words become independent and the `Vector` address is duplicated, so
both wrappers name one vector and a `push` through either is seen by both.
That is ADR 0001 verbatim, from one word-range copy.

### 4. A fixed-size enum payload

~~~cove
enum Shape { Dot, Line(Int), Box(Int, Int) }
~~~

`[disc: Int, Int, Int]`, three words. `Dot` writes the discriminant and zeroes
the rest; `Box(3, 4)` writes all three. A copy copies three words.

With a reference — `enum Msg { Ping, Text(String) }` — the layout is
`[disc: Int, Ref]`, and `Ping` leaves the reference word null, so the
collector reads null rather than a stale address.

### 5. Multiword parameters, returns, joins and captures

- A parameter takes the words its layout says, from slot 0 onward in
  declaration order; a `(Int, Point, Int)` list occupies slots 0, 1–2, 3.
- A return copies `Function::returns`'s words into the caller's destination
  location.
- A branch join is two copies into one destination location, one per arm.
- A capture is stored inline in the closure environment and copied into the
  callee's frame with the other captures.

### 6. GC maps for multiword values

A frame's map is `RefMap::of(&Function::reprs)`, and a multiword value
contributes its flattened per-word `Repr`s — a `Wrapper` at slot 5
contributes `Int, Int, Ref`, so slot 7 is a root and 5 and 6 are not.

A heap object's payload map is the same function of the same kind of layout.
An enum inline anywhere is static because of the payload-agreement rule, so
nothing has to read a discriminant during a collection.

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
schema declared `Any` — is one `Ref` word naming a `Boxed` object. The object
records the layout of what it holds and holds that value's words inline, so a
boxed `Point` is a two-word payload rather than a reference to somewhere else
again.

Erasure is where a value stops having a static width, and a heap object is
where a value without a static width has to live. An `Int` written as an
`Int` is one inline word; the same `Int` written as a `dyn` allocates. That
is the right place to pay, and paying it there is what keeps every
*un*-erased location's width a static fact.

A `Ty::Unknown` is not that. It is the checker declining, and a program the
checker declined about is a compile error. The target is *every valid checked
program lowers*, not *every checker abstention becomes runtime dispatch*.

There is therefore **no `Unsupported`, no admission predicate and no lowering
floor**. A construct the lowering has not been taught is a bug in the lowering.

## What is not decided here

Everything ADR 0034 lists as undecided: when the two backing allocations
become one block, how the block grows, the final allocator, a moving
collector, and whether dispatch is later threaded or compiled. Enum layout is
decided here only as far as the payload-agreement rule requires; how tightly a
payload region is packed within that rule is not.

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
