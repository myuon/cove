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
word 0                                  STACK_WORDS            ...
|--------------- stack region ---------------|---- heap region ----|
| seg 0 | seg 1 | seg 2 | …                   |
~~~

The stack region divides into one segment per task; see "Each task has a
stack segment of its own" below.

- `STACK_WORDS` is a compile-time constant of the runtime, and is
  `SEGMENT_WORDS × SEGMENTS`. The stack region is reserved, not committed:
  a segment's backing store grows on demand and no heap object is ever
  placed below `STACK_WORDS`.
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
slots in one numbering, which is `Function::reprs`.

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

**"At a temporary's last use" is the intent, and a diverging sub-expression
is where it is hard.** In `f(a, if c { b } else { break })` the last use of
`a` on the taken path is not where the release was written, because control
left the expression. A temporary belongs to no scope, so clearing scope
bindings does not reach it. The lowering therefore keeps a list of the
temporaries currently holding a reference, and an early exit clears the ones
above the mark the loop took — everything above it is this turn's and nothing
above it is read after the jump lands.

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
| a Host resource | one word: a name in the run's resource table |
| a `Task`, a `TaskScope` | one word: a name in the run's scheduler table |

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

### Recursion is rejected, not boxed

`struct Node { value: Int, next: Option<Node> }` has no finite inline width,
and [ADR 0035](adr/0035-a-value-type-may-not-contain-itself.md) decides what
happens to it: **the checker rejects it.** A recursive cycle must pass through
a type whose values are a reference — `String`, `Array`, `Map`, `Set`,
`Vector`, `Shared`, a closure or a `dyn` trait object — and every recursive
declaration in the corpus already does.

The lowering therefore has no implicit boxing to do. That was the first
implementation's answer and it is the one the ADR rejects: it would make
whether a write through a copy is visible depend on whether a type happens to
mention itself, which is a representation deciding the language's semantics.

What that leaves is worth naming, because it is the reason the rule is worth
having: **`Shape::Boxed` has exactly one meaning — a value whose type was
*intentionally* erased.** Erasure and recursion no longer share a mechanism.

### A layout describes a family, and both regions use it

A layout describes a *family*, not an instantiation: `Array<String>` and
`Array<Point>` are one layout, because a reference is a reference. The
lowering interns them, so one shape is one `LayoutId` however many times the
source writes it.

A *declared* generic type is the other side of that rule and not an exception
to it. `Cell<Int>` is one word and `Cell<Point>` is two, so they are not one
family and cannot be one layout — see "Generics are monomorphised" below.
What decides which of the two a declaration falls under is the same question
as everywhere else here: whether the argument changes the words.

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
| `struct T` | `Struct`, fields inline | declared struct, per instantiation |
| `enum E` | `Enum`, discriminant then payload | declared enum, per instantiation |
| `Option<T>` | `Enum`, cases `None`, `Some` | payload layout |
| `Result<T, E>` | `Enum`, cases `Ok`, `Err` | payload layout pair |
| `String` | `Str` | the program |
| `Array<T>` | `Elements` | element layout |
| `Vector<T>` | `Vector`, `[len, store]` over an `Elements` store | element layout |
| `Set<T>` | `Members`, sorted and distinct | element layout |
| `Map<K, V>` | `Entries`, sorted by key | key/value layout pair |
| `MapEntry<K, V>` | `Struct { key, value }`, inline | key/value layout pair |
| `Range` | `Struct { start: Int, end: Int, inclusive: Bool }` | the program |
| `Shared<T>` | `Shared`, a lock word then the value inline | wrapped-value layout |
| a function value | one `Ref` word, `Word(Ref)` | the program |
| a closure environment | `Closure`, captures inline | lowered lambda |
| `dyn`, `Any` | `Boxed` | the program |

A function value and its environment are **two** layouts, and the distinction
is load-bearing: the location holding a function value is one `Ref` word under
one layout for every signature, because a reference is a reference and which
environment a word names is the object header's business. The environment is
one layout per lowered lambda, with the callee's id in payload word 0 and the
captures inline after it.

A *declared* function used as a value is the same environment with no
captures. One shape rather than two means a call through a value has one thing
to read, and `xs.map(double)` and `xs.map(fn(x) { ... })` are one lowering.

A `Set` and a `Map` are sorted runs rather than hash tables, because the
language says they iterate in ascending order and render that way: the order
is part of the value, not an implementation's leftovers.

**One entry of an `Entries` run is a `MapEntry`** — the key's words then the
value's, in that order and at that width. That correspondence is load-bearing
rather than incidental: it is what lets a `for` over a `Map` bind an entry
with one element load, and what lets `Map.of` read the entries it is given
without unpacking them.

`Members` and `Entries` objects are element-addressable at their own width,
exactly as `Elements` is. `LoadElem` and `Len` are about a run of equal-width
things and not about arrays, so a `for` over a `Set` needs nothing an array
does not.

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

Two things a call site has to answer that the frame layout does not:

- **The caller builds a variadic's array.** The parameter is one ordinary
  location holding one ordinary `Array<T>`, so the callee's frame has nothing
  special in it and no instruction knows a variadic exists.
- **The caller evaluates a default, in the callee's scope.** A default may
  name an earlier parameter, so it is lowered at the call site with the
  parameters before it bound to the locations the call already wrote — which
  is where the tree-walking oracle evaluates it. That costs no extra frame,
  no extra call and no change to the convention; the alternatives were a
  hidden "was it supplied" parameter, a function per arity, or a thunk per
  default.

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

### The boundary tags a box with the family that *describes* the value

A `Value` crossing in at an erased position has to be tagged with some
layout, and more than one can accept it: `Result<Int, Error>` and
`Result<Any, Error>` both admit `Ok(7)`, because a boxed position admits
everything.

The tag exists for `Unbox`, which is asked for the layout a **static type**
named — and no static type names `Result<Any, Error>`'s `Ok`. So the search
prefers a family that describes the value and falls back to an erasing one
only when nothing does. Taking the first match instead makes a program's
answer depend on the order the layout table happened to intern in, which is
the kind of bug that passes until an unrelated return type changes.

**The search is the fallback, not the rule.** A value that describes one
family can describe two: `Err(Error("no"))` is exactly a `Result<Int, Error>`
and exactly a `Result<String, Error>`, and no property of the value will ever
tell them apart. What does is where the value came from — a box built from a
**callback's answer** is tagged with the layout that callback returns, which
is a static fact the machine already holds. The search runs only where
nothing knows.

That is the same shape as everything else here: a word is untagged and its
meaning comes from the static metadata it came from, never from its bits.

## A Host resource is a name, not an object

`Repr::Host` is one word, and it is neither inline data nor an address into
this memory. It indexes a run-owned table of `ResourceHandle`s.

[ADR 0031](adr/0031-a-host-handle-is-not-a-vm-handle.md) is why it cannot be
an object: a heap object is storage this runtime allocated and manages, and
making a resource one would put a collection in charge of a lifetime
[ADR 0013](adr/0013-host-resource-handles.md) gives to the host. The run
would be sweeping something whose `close` the program never wrote.

It is not a second value store either — ADR 0034 names "Host-owned resource
registries" among the things that are not one — because the table holds
*names*, and nothing that wanted to dodge a heap representation can be put in
it.

Three rules the lowering has to know:

- **The word is one past the index, so zero is no resource.** Frames are
  zeroed on entry, so a `Host` slot nothing has written reads zero; indexing
  straight by the word would make an unwritten slot name whichever resource
  happened to be first. Zero earns the same refusal a null reference does.
- **A resource is interned.** ADR 0013 makes two handles equal when they name
  the same resource, so one resource is one word — otherwise comparing the
  words would not be comparing the resources.
- **Nothing is ever removed.** ADR 0013 keeps a closed resource's handle
  alive as a name and never reuses an identity, so the refusal a stale handle
  earns is the *host's* and is reached by handing the host the name. The cost
  is one name per distinct resource, which is the size of the table the host
  keeps anyway.

The collector cannot reach one, and that is a property of the same predicate
everything else rests on: tracing enqueues on `Repr::is_ref`, and
`Repr::Host` is not a reference. The table holds no addresses at all, so a
mark would have nothing to follow.

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
`CallClosure`; `filter` and `fold` are the same shape with a different body, and
`sorted` is the same idea over two runs. `Shared.lock` is the same shape
again: acquire, call, release, with the release an obligation on every exit
path exactly as `Clear` is. Three things follow, and all three are the reason:

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

Each task has a stack segment of its own, and it **must** — two tasks whose
stacks were each addressed from zero would form the same addresses for
different words. The reserved stack region divides into `SEGMENTS` segments
of `SEGMENT_WORDS` each, task *k* owns segment *k*, and a frame that would
leave its segment is a stack overflow.

The region decoder does not change: `addr < STACK_WORDS` is still the whole
of it, and *which* segment an address is in is a question only the task that
owns one ever asks. The ranges are disjoint by construction, so an address
formed in one task is not an address in another — arithmetic rather than a
convention anyone has to keep.

Every segment and the object heap belong to the run's one logical linear
address space.

### What synchronises a word

**Nothing, except one word of a `Shared` cell.** Every ordinary load and
store in the memory is relaxed, which on every target this runs on is a
plain load and store.

That is sound because of what the language already forbids. A value that is
not task-safe cannot cross a task boundary, so two tasks can only reach one
value through a `Shared`, and a `Shared`'s lock word is a release/acquire
pair. Acquiring it publishes everything the previous holder wrote — the
cell's own words *and* every object it allocated and stored into them.

Paying for ordering on every word would be paying in every program for
something the task-safety rule has already ruled out.

The heap's allocator synchronises handing out words and stopping the world.
It does not synchronise a value, and those two are kept apart deliberately.

**A task that blocks must still count as being at a safepoint.** A waiter on
a cell, and a task inside a Host call, publish their roots and park; without
that a collector waits for a task that waits for a task that waits for the
collector.

Two things are named by a word but are not Cove-owned objects, and ADR 0034
carves out both: `Task` and `TaskScope` name **scheduler control state**, and
`Resource` names **Host-owned state**. Neither is a value store, and neither is
a place a Cove value may be put to avoid giving it a heap representation.

## Erasure: `dyn`, `Any`, and what the checker settled

A value whose type is *intentionally* erased — `dyn Trait`, a Host result a
schema declared `Any` — is one `Ref` word naming a `Boxed` object. That is
the only thing a `Boxed` object is: a recursive layout is a compile error
under [ADR 0035](adr/0035-a-value-type-may-not-contain-itself.md) rather than
something quietly boxed, so nothing else shares this mechanism. The object
records the layout of what it holds and holds that value's words inline, so a
boxed `Point` is a two-word payload rather than a reference to somewhere else
again.

Erasure is where a value stops having a static width, and a heap object is
where a value without a static width has to live. An `Int` written as an
`Int` is one inline word; the same `Int` written as a `dyn` allocates. That
is the right place to pay, and paying it there is what keeps every
*un*-erased location's width a static fact.

### A box is opened at the use, at the type the checker settled there

Not at the binding, and not by rebuilding the value that contains it.

`clock.timeout`'s schema answers `Result<Any, Error>`, and a program that
writes `let answer: Result<Result<http.Response, Error>, Error> = …` has said
what is inside. The box is one word *inside* the enum, so the outer `match`
and the `?` read an ordinary enum, and the use that reaches the box word — an
arm's binding, a `?`, a field read — is itself an expression whose type the
checker settled. Opening it there is one `Unbox`.

That is why nesting needs no conversion: each level is opened at its own use,
to any depth, one instruction each. Rebuilding the enum case by case would
have been a branch and several instructions to remake a value where one
instruction reads the word that actually differs — and it would still have
left a box inside for this rule to open.

Nothing re-reads the written annotation. An annotation is a name, and only
the checker knows what the name meant; it has already carried the answer to
every use.

**A use with no static answer stays a gap.** `ask("n").length()` where the
checker settles `Any` has one other move — dispatch on what the box turned
out to hold — and that is the move this backend does not make.

A `Ty::Unknown` is not that. It is the checker declining, and a program the
checker declined about is a compile error. The target is *every valid checked
program lowers*, not *every checker abstention becomes runtime dispatch*.

There is therefore **no `Unsupported`, no admission predicate and no lowering
floor**. A construct the lowering has not been taught is a bug in the lowering.

## Generics are monomorphised, and the value model is what decides it

A generic declaration is lowered to **one function per instantiation**, and a
generic `struct` or `enum` to **one layout per instantiation**. `f<Int>` and
`f<Point>` are two functions with two frames; `Cell<Int>` and `Cell<Point>`
are two layouts, one word and two.

This is not a choice between three reasonable implementations. The model
above rules out the other two, and it is worth writing down which sentence
does it.

**The sentence is "a slot's `Repr` is fixed for the whole function".** That is
what makes one static bitmap correct at every program counter, and the
bitmap is what the collector reads instead of inspecting a word. Everything
else here — that a value is a run of words, that a copy is a word-range copy,
that a field is arithmetic on a slot number — is downstream of a location's
width being a static fact.

A generic value's width is **not** a fact about the generic declaration. It
is a fact about the type argument:

~~~cove
struct Cell<T> { it: T }
~~~

`Cell<Int>` is one word. `Cell<Point>` is two. `Cell<String>` is one word and
that word is a `Ref`, so it is a root; `Cell<Int>`'s is not. The width, the
offsets and the reference map all move with `T`.

So:

- **Dictionary passing** — carrying a layout at run time and reading widths
  out of it — makes a frame's slot numbering depend on a value the callee is
  handed. Then a slot's `Repr` is no longer a function of the function, the
  static bitmap is no longer correct at every program counter, and the
  collector is back to inspecting words to decide what is a pointer. The
  foundation goes with it. There is no version of this that keeps the
  reference map static, because the map's whole content is widths and kinds
  and those are exactly what the dictionary would be carrying.

- **A uniform boxed representation** — every generic value one `Ref` to a
  heap object — keeps the map static and pays for it somewhere worse. It
  allocates on every call that passes a generic value, including
  `f(1)`. And it contradicts the model's own claim that a value's words are
  where the value is: an `Int` that is an inline word in every other position
  becomes an address because the function it was passed to was written with a
  type parameter. The language already has a name for "one copy of the code,
  and the value carries its own description", and it is `dyn Trait` — which
  the erasure section above says is where allocating is the right answer
  because the type was *intentionally* erased. Making a type parameter mean
  the same thing would erase what the program did not ask to erase, and would
  leave `dyn` and `<T>` two spellings of one mechanism.

- **Monomorphisation** is what is left, and it is not a consolation. If two
  instantiations have different widths, different offsets and different
  reference maps, they are genuinely different code, and lowering them to
  different code is describing the situation rather than working around it.

The checker is what makes it cheap. It walks a generic body **once**, with
its type parameters rigid, and records a type for every expression in that
body in terms of them. The lowering does not re-check anything per
instantiation: it carries the substitution and completes each recorded fact
as it reads it, which is `Ty::instantiate` — the same operation the checker's
own `Signature` documentation points a consumer at. One recorded walk serves
every instantiation.

### Which instantiation a call site asks for

Nothing records it, and nothing needs to. What is recorded is the type of
every operand of the call and the type of the call itself, all settled with
the type arguments already applied — a call written `f<Int>(x)` and one
written `f(x)` are indistinguishable by then, which is why an explicit type
argument needs no separate path. The declaration's `Signature` still holds
the `Ty::Param`s, so walking a declared type and a settled one together reads
one off the other. It is a reading rather than an inference: the checker
unified these two before the lowering saw them, which is why the call was
accepted at all.

The one thing it cannot read is a type parameter that appears in neither the
parameters nor the answer, because then the written argument is the only place
it exists and no fact carries it. That is reported rather than guessed at.

### A bound is dispatched statically, and costs nothing

~~~cove
fn headline<T: Summary>(entry: T) -> String { entry.summary() }
~~~

The checker records no target for `entry.summary()`, and that is the right
answer *about the declaration*: which implementation it reaches is decided by
the argument. But this body is lowered for one argument. ADR 0006 makes
conformance explicit, so that argument names exactly one implementation, and
the call is an ordinary `Call` found the way a `Type.method` call finds one.
No dictionary, no vtable, no indirect jump. A bound is free at run time and
costs one function per type it is used at.

### The instantiation depth is bounded

~~~cove
fn f<T>(x: T) { f(Cell(x)) }
~~~

This checks. [ADR 0035](adr/0035-a-value-type-may-not-contain-itself.md) is
about a *declaration*'s layout containing itself, and `Cell<Cell<Int>>` is
finite, so nothing rejects it. It asks for `f<Int>`, `f<Cell<Int>>`,
`f<Cell<Cell<Int>>>` and so on without end, and every one of them is a
different width, so there is no finite set of functions to lower it to.

A recursive generic at *one* type is not this: the id is recorded before the
body is lowered, so `fn f<T>(x: T) { f(x) }` finds the number it already
took. What has no fixed point is a chain that grows the type at every step.

So the lowering caps the depth of the chain and reports a diagnostic naming
it step by step. It is a **refusal**, not one of the lowering's gaps: no later
task removes it, because monomorphisation is what the model admits and this
program has no monomorphisation. A reader who meets it has something to do —
take the argument as a `dyn Trait`, which is one function for every type that
conforms. And a compiler that does not terminate is worse than one that
refuses.

Where the cap goes is a judgement, and the number is not part of the
language, the IR or any public API, for the same reason `STACK_WORDS` is not.

## What is not decided here

Everything ADR 0034 lists as undecided: when the two backing allocations
become one block, how the block grows, the final allocator, a moving
collector, and whether dispatch is later threaded or compiled. Enum layout is
decided here only as far as the payload-agreement rule requires; how tightly a
payload region is packed within that rule is not.

## The stack limit is not a language fact

The stack region is reserved and divided into segments, so "too deep" is a
frame that would leave *its own* segment — one task is not stopped because a
sibling is deep. The constant is an implementation
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
