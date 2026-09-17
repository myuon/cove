//! The **real** native tier, over the real runtime: compiled, finalized, and
//! entered from the encoded `CALL` arm.
//!
//! `native_return.rs` beside this file tests the boundary with a hand-written
//! third tier — an interpreter over the same IR through the same
//! [`Entry`](cove_runtime::NativeEntry) — which is what lets those cases run in
//! the ordinary `cargo t` with no code generator and no executable page. What it
//! cannot say is anything about *machine code*: a template that stored one word
//! too many, a prologue that clobbered a callee-saved register, a page that was
//! never made executable.
//!
//! This file is that half. It needs a code generator, so it is behind the
//! `template` feature and is compiled by nothing a default build does — which is
//! ADR 0055's adoption gate ("a build without the native feature has no
//! executable-memory dependency") and the same place `cove-native`'s own suites
//! live. `.github/workflows/ci.yml` runs it.
//!
//! # Every case is differential and nothing here asserts a number
//!
//! The encoded VM is the semantic reference, so every case runs the same entry
//! twice — once with [`Vm::new`] and once with [`Vm::with_native`] — and asserts
//! the two answers are equal. What it does assert about the tier is only that the
//! tier was *used*: a case whose `vm_to_native` counter did not move has compared
//! the VM against itself and would pass whatever the code generator emitted.

#![cfg(feature = "template")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_runtime::{Grants, HostRegistry, Runtime, Value, Vm};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::{Compiler, Config};

const MODULE: &str = "m";

/// Functions the template compiler takes, called from functions it does not.
///
/// The shape every case needs is a **refused caller and a compiled callee**,
/// because that is the hop issue #369 added. `held` is what keeps the inliner off
/// the callees — `cove_ir::lower::inline` expands a call to a leaf of under
/// sixteen instructions, and a fixture whose call was expanded would be testing a
/// crossing that no longer happens — and `counts` is recursive, so nothing that
/// calls it can be expanded either.
const SOURCE: &str = "\
use std.stringbuilder
use std.stringbuilder.StringBuilder

/// Recursive, so no caller of this can be inlined away. `counts(0)` is zero.
export fn counts(n: Int) -> Int {
  if n <= 0 {
    0
  } else {
    counts(n - 1) + 1
  }
}

/// The identity, and a non-leaf.
export fn held(x: Int) -> Int {
  counts(0) + x
}

export fn adds(a: Int, b: Int) -> Int {
  held(a) + b
}

/// A caller that is refused, so that its `call` is the VM-to-native hop.
///
/// `let nothing = Shared(0).lock(fn(v) { v })` is what refuses it, and it is
/// the marker every refused caller in this file uses: `Shared<T>.lock` emits
/// `Inst::AddrOfField` unconditionally — see
/// `cove_ir::lower::cells::Lower::shared_lock` — and nothing lowers that
/// instruction, so this function runs on the encoded tier however the table
/// is built — which is the point of it.
///
/// **It has already been three other things, and that is the hazard rather
/// than an accident.** It was a string literal until `Inst::Str` was lowered,
/// `let nothing = ()` until `Inst::Unit` was, and `Vector.of(0)` until
/// `Inst::StoreField` was; each time, the instruction the marker relied on
/// joined the subset and a dozen cases in this file came within one commit of
/// comparing the native tier with itself. So the marker is chosen for how
/// *unlikely* it is to be lowered next rather than for how small it is:
/// `Inst::AddrOfField` refuses through `Machine::checked`, whose message names
/// a layout by name and its payload word count, and `cove-native` cannot build
/// that sentence — `subset.rs`'s own reason for leaving it out, unlike
/// `Inst::LoadField` and `Inst::StoreField` beside it, which is what made this
/// marker's two predecessors compile out from under it.
///
/// That is still a judgement and not a guarantee. What actually stands between
/// this file and a self-comparison is not the marker: it is that **every case
/// asserts the tiers it needs** — `on_each_tier`, `compiled < reachable`, and a
/// tier counter that moved. Those fail loudly on the day this marker compiles.
export fn callsAdds(a: Int, b: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  adds(a, b)
}

/// Negation, so that `Inst::Neg` runs as machine code and can raise there.
///
/// `counts(0)` is zero and is here for the reason it is in every fixture above:
/// it keeps `cove_ir::lower::inline` from expanding this body into its caller's,
/// which would put the negation on the caller's tier instead of on this one's.
export fn negates(a: Int) -> Int {
  -a + counts(0)
}

/// A refused caller, so the negation is reached across the boundary.
export fn callsNegates(a: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  negates(a)
}

/// `Int.abs`, which is the standard library's and a small leaf, so the inliner
/// expands it here: a fault in it is raised by *this* function's compiled code,
/// at a span inside `std/int.cove`.
export fn absolutes(a: Int) -> Int {
  a.abs() + counts(0)
}

/// A refused caller, so the expanded `abs` is reached across the boundary.
export fn callsAbsolutes(a: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  absolutes(a)
}

/// Division, so that a raise crosses the boundary.
export fn divides(a: Int, b: Int) -> Int {
  held(a) / b
}

export fn callsDivides(a: Int, b: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  divides(a, b)
}

/// A deep recursion whose outer destinations are pending while the stack's `Vec`
/// reallocates.
export fn callsCounts(n: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  counts(n)
}

/// Two words inline, so that a place can name the second of them.
export struct Point {
  x: Int
  y: Int
}

/// A `Point` a closure captures, read back inside the closure's own body.
///
/// A closure's captures live in the closure's own heap object — `Shape::Closure`,
/// one of `Layout::fixed_payload_words`' `Some` shapes — and are copied in by
/// `Inst::StoreField` where the closure is built and read back by
/// `Inst::LoadField` where a captured name is used, both at the closure's own
/// payload offset rather than the caller's frame. So `p.x` and `p.y` here are not
/// `movesY`'s `Inst::AddrOfPart` — that is a place *inline* in a frame — this is
/// the whole two-word value read out of an object on the heap, which is what
/// makes it a case `Inst::LoadField`'s table lookup answers rather than one the
/// `var`-address family already covered.
///
/// `counts(0)` is here for the reason it is everywhere above: without it the
/// closure's own body is a leaf under sixteen instructions and
/// `cove_ir::lower::inline` would expand it into whichever frame calls it.
export fn capturesAPoint(x: Int, y: Int) -> Int {
  let p = Point(x: x, y: y)
  let f = fn() { p.x * 1000 + p.y + counts(0) }
  f()
}

/// Writes through a `var` parameter, which is one word holding a linear address.
///
/// `Inst::AddrOfSlot` in whoever calls it, `Inst::Load` and `Inst::Store` here.
///
/// Every fixture below answers an `Int` rather than nothing, and that is not a
/// style choice: `Inst::Unit` is outside the subset on purpose — see
/// `cove-native`'s `anything_outside_the_slice_refuses_the_whole_function`, where
/// the line is drawn at the adoption gate's list and not at what happens to be
/// easy — so a `Unit`-returning function is refused for one store and could not be on
/// the tier these cases need it on.
export fn bumps(var total: Int, by: Int) -> Int {
  total = total + by + counts(0)
  total
}

/// A refused caller, so the address crosses from an encoded frame into a compiled
/// one and the word written through it is read back by the frame that owns it.
///
/// Both halves are in the answer: `total` says the store landed in *this* frame's
/// slot, and `seen` says the callee also saw it, so an arm that wrote the right
/// number to the wrong place cannot pass on the second alone.
export fn callsBumps(a: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  var total = a
  let seen = bumps(var total, 5)
  total * 1000 + seen
}

/// Writes *one word* of a two-word value location, which is `Inst::AddrOfPart`.
export fn movesY(var p: Point, to: Int) -> Int {
  p.y = to + counts(0)
  p.y
}

/// A refused caller lending two words of its own frame, one of which is written.
export fn callsMovesY(a: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  var p = Point(x: a, y: 0)
  let seen = movesY(var p, 9)
  p.x * 1000 + p.y * 10 + seen
}

/// Refused — the marker is why — and it writes through the `var` it was lent.
export fn shows(var total: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  total = total + 1
  total
}

/// Compiled, and it lends a slot of *its own* frame to a callee that is not.
///
/// The address crosses the other way: formed in machine code, followed by the
/// encoded tier, and the word it names read back by the compiled frame.
export fn lendsToTheVm(a: Int) -> Int {
  var total = a + counts(0)
  let seen = shows(var total)
  total * 1000 + seen
}

/// The outermost frame, refused, because `Vm::invoke` enters the encoded tier for
/// the frame it opens itself: without a caller above it `lendsToTheVm` would run
/// on the VM and the crossing under test would not happen.
export fn callsLendsToTheVm(a: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  lendsToTheVm(a)
}

/// A `var` threaded down a recursion that changes tier at every step.
export fn descends(n: Int, var total: Int) -> Int {
  if n > 0 {
    total = total + 1
    lowers(n - 1, var total)
  } else {
    total
  }
}

/// Refused, and it calls back into the compiled one — so one address is carried
/// through an alternating chain of encoded and compiled frames while the stack's
/// `Vec` reallocates under all of them.
export fn lowers(n: Int, var total: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  descends(n, var total)
}

/// The frame that owns the word every step of the chain wrote through.
export fn threads(n: Int) -> Int {
  var total = 0
  let seen = descends(n, var total)
  total * 1000 + seen
}

/// The allocation a collection is forced with, on the encoded tier in every case.
///
/// It needed no marker while `String.slice` was outside anything the template
/// compiler lowers. Compiled code calls intrinsics now (#378, P5-6), so it carries
/// the marker every refused fixture in this file carries.
///
/// It was `sliceBytes` until ADR 0058 moved that into the standard library over
/// a byte run slice, which both code generators lower. `slice` counts characters
/// rather than bytes, and every character of the text it is handed is ASCII, so
/// the answer is the same number.
export fn allocates(s: String, n: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  s.slice(held(0), n).byteLength()
}

/// A `Shared` keeps its value in a **heap object**, and `lock` hands the closure
/// the address of the field the value sits in — which is the one route a Cove
/// program has to a `Repr::Addr` word naming the *heap* rather than a frame slot.
///
/// The closure is the compiled one: its `word = ...` is an `Inst::Store` through a
/// heap address and its answer an `Inst::Load` through the same, so the `is_stack`
/// decision generated code takes is taken on *both* of its arms by this fixture
/// and the ones above it together. `counts(0)` is there for the reason it is
/// everywhere else.
///
/// The second `lock` takes no `var`, so the VM loads the word itself and the
/// closure is handed a copy: that is the encoded tier reading back what compiled
/// code wrote into the heap.
export fn heapsThrough(a: Int) -> Int {
  let cell = Shared(a)
  let seen = cell.lock(fn(var word) {
    word = word + 5 + counts(0)
    word
  })
  let after = cell.lock(fn(word) {
    word + counts(0)
  })
  seen * 1000 + after
}

/// A reference given up and a reference kept, across an allocation that collects.
///
/// `Inst::Clear` is emitted at `a`'s last use, so the compiled frame stops being
/// a root for the object `a` names — and `b` is read *after* the allocation, so a
/// clear that zeroed one word too many, or the wrong slot, would let the collector
/// sweep an object this frame still needs.
export fn keepsWhatItStillNeeds(a: String, b: String, n: Int) -> Int {
  let first = a.byteAt(held(0))
  let grew = allocates(b, n)
  first + b.byteAt(held(1)) + grew
}

/// `String.byteLength()` in a frame that is **compiled**.
///
/// `std.string.byteLength` is `core.byteLength(text)`, which lowers to
/// `Inst::Len` — a null refusal and one header read — and is expanded into this
/// frame, so there is no call and no builtin left in it. `counts(0)` is here
/// for the reason it is everywhere else, and it earns one thing more: it makes
/// this function's own call a **native-to-native direct** one, which is what says
/// the measurement happened on this tier rather than on the VM. A refused
/// `measures` would have made that same call a VM-to-native one instead, and the
/// two counters are how a case tells them apart.
export fn measures(s: String) -> Int {
  s.byteLength() * 10 + counts(0)
}

/// A string **literal** in a frame that is compiled.
///
/// `Inst::Str` is a load of `NativeCtx::literals[text]`, and the table is
/// published by the runtime rather than compiled in — see `cove_native::abi`'s
/// note on why a literal's address is a run-time load — so what a case has to say is that
/// the word this frame stores is the address `Machine::place_literals` handed out
/// and not a number that happens to look like one.
///
/// Two literals and a branch, so that reading the wrong table entry answers the
/// *other* string rather than nothing at all, and `counts(0)` for the reason it is
/// in every fixture above.
export fn picksALiteral(n: Int) -> String {
  if n > counts(0) { \"alpha\" } else { \"beta gamma\" }
}

/// A refused caller that **follows** what compiled code handed it.
///
/// A literal that answered a plausible word rather than a reference passes a
/// comparison and fails here, because a byte length is a read through the address.
export fn callsPicksALiteral(n: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  picksALiteral(n).byteLength()
}

/// The same literal, carried back across the boundary as the `String` itself.
///
/// This is the half a length cannot be: the object compiled code stored is what
/// the *caller* renders, on the other tier, so a reference that was right in the
/// callee's frame and wrong in the caller's destination is a string that vanished
/// at a tier boundary.
export fn saysALiteral(n: Int) -> String {
  let nothing = Shared(0).lock(fn(v) { v })
  picksALiteral(n)
}

/// A refused caller, so the measurement is reached across the boundary.
export fn callsMeasures(s: String) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  measures(s)
}

/// A compiled frame that **allocates**, with a reference live across it.
///
/// The array literal is an `Inst::Alloc` and four `Inst::StoreElem`s, so this is
/// the only shape a Cove program has for reaching the allocation helper from
/// compiled code. `s` is read on both sides of it — `first` before and
/// `byteLength` after — so a collection during the allocation has to have found
/// the reference in the slot the frame's static map names, and has to have left it
/// there.
export fn allocatesAndKeeps(s: String, n: Int) -> Int {
  let first = s.byteAt(held(0))
  let made = [n, n + 1, n + 2, n + 3]
  first + made.length() + s.byteLength()
}

/// A refused caller that holds a reference of its own across the callee's
/// allocation.
///
/// `also` is an `Array` this frame builds and reads back *after* the call, so the
/// collection the callee's allocation may cause happens with a live reference in
/// an **encoded** frame below a compiled one — which is the pair no single-tier
/// case can be.
export fn callsAllocatesAndKeeps(s: String, n: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  let also = [n, n, n]
  allocatesAndKeeps(s, n) + also.length()
}

/// `Vector.push` in a compiled frame: the fast path and the growth one.
///
/// The vector is a **parameter** rather than a local: it is the *caller* that
/// carries this file's `Shared(0).lock(...)` marker and is refused for it, so a
/// vector this function made itself would still be made in a compiled frame —
/// `Vector.of` lowers fully now, `Inst::StoreField` included — and the shape below
/// is kept anyway, because a callee that only receives what its caller already
/// built is the stronger place to read a growth back from.
///
/// It answers the *counter* and not `v.length()`, and that is no longer forced —
/// `length()` is an `Inst::LoadField` and this arm lowers one now — but the
/// counter is kept: the caller reading the length back out of the same header
/// this callee wrote is `callsPushesOnto`'s own assertion, and answering it here
/// too would make that read redundant rather than load-bearing.
///
/// `counts(0)` is here for the reason it is in every fixture above, and this is
/// where it was learnt: without it the whole body is a leaf of under sixteen
/// instructions, `cove_ir::lower::inline` expands it into its caller, and the
/// pushes run on the caller's tier — which is the VM. The case then compares the
/// VM with itself and passes whatever either code generator emitted.
export fn pushesOnto(given: Vector<Int>, n: Int) -> Int {
  var v = given
  var at = 1
  while at < n {
    v.push(at)
    at = at + 1
  }
  at + counts(0)
}

/// A refused caller that reads back everything the compiled pushes wrote.
///
/// The sum is what says the **live prefix survived growth**: a `push` that has to
/// grow allocates a larger store, copies the elements it already had and replaces
/// the header's store word, and a caller that could only see the *length* would
/// pass whatever the copy did. `v.length()` here is read out of the same header
/// the callee bumped, so the replaced store word is read by this frame and not by
/// the one that replaced it.
export fn callsPushesOnto(n: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  var v = Vector.of(7)
  let answered = pushesOnto(v, n)
  var total = 0
  var at = 0
  while at < v.length() {
    match v.get(at) {
      Some(x) => total = total + x
      None => total = total - 1
    }
    at = at + 1
  }
  answered * 1000000 + v.length() * 1000 + total
}

/// `Vector.set` in a compiled frame: the fast path, in range and out of it.
///
/// `held`'s reason not to inline is `pushesOnto`'s, unchanged: without
/// `counts(0)` this callee is a leaf under sixteen instructions and the
/// caller keeps the crossing for itself.
export fn setsAt(given: Vector<Int>, index: Int, value: Int) -> Int {
  var v = given
  let was = v.set(index, value)
  let answer = match was {
    Some(x) => x
    None => -1
  }
  answer + counts(0)
}

/// A refused caller that builds a vector of `size` elements — `0, 10, ..,
/// (size - 1) * 10` — sets one, and reads back both the answer and the whole
/// vector, so a wrong offset or a write that smeared past its own element is
/// caught by the sum and not only by the answered element.
export fn callsSetsAt(size: Int, index: Int, value: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  var v = Vector.of(0)
  var i = 1
  while i < size {
    v.push(i * 10)
    i = i + 1
  }
  let answered = setsAt(v, index, value)
  var total = 0
  var at = 0
  while at < v.length() {
    match v.get(at) {
      Some(x) => total = total + x
      None => total = total - 1
    }
    at = at + 1
  }
  answered * 1000000 + v.length() * 1000 + total
}

/// The same, on a vector `pop()` has emptied — the shape a `set` on an empty
/// vector needs: `Vector.of()` with no elements does not say what it is a
/// vector of, and `Vector.of(0)` is already one.
export fn callsSetsOnEmpty(index: Int, value: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  var v = Vector.of(0)
  let popped = v.pop()
  let answered = setsAt(v, index, value)
  answered * 1000 + v.length()
}

/// `Vector.set` of a two-word element in a compiled frame — the stride case,
/// where an offset that forgot the multiply lands on the wrong element
/// instead of merely a wrong one.
export fn setsPointAt(given: Vector<Point>, index: Int, x: Int, y: Int) -> Int {
  var v = given
  let was = v.set(index, Point(x: x, y: y))
  let answer = match was {
    Some(p) => p.x * 1000 + p.y
    None => -1
  }
  answer + counts(0)
}

/// A refused caller that builds a `Vector<Point>` of `size` elements, sets
/// one, and reads every element back.
export fn callsSetsPointAt(size: Int, index: Int, x: Int, y: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  var v = Vector.of(Point(x: 0, y: 1))
  var i = 1
  while i < size {
    v.push(Point(x: i * 10, y: i * 10 + 1))
    i = i + 1
  }
  let answered = setsPointAt(v, index, x, y)
  var total = 0
  var at = 0
  while at < v.length() {
    match v.get(at) {
      Some(p) => total = total + p.x + p.y
      None => total = total - 1
    }
    at = at + 1
  }
  answered * 1000000 + v.length() * 1000 + total
}

/// `Vector.freeze() -> Array<T>`: `Memory::relabel` turning the store into the
/// array in place, read back through `Array.get` and `Array.length` rather than
/// through the `Vector` it no longer is.
///
/// The vector is built **here** rather than handed in as a parameter: `freeze()`
/// is a consuming transition, and the checker's uniqueness proof for one only
/// reaches back across a `var self` receiver of a method in a plain `impl`
/// block — `cove::unique::not_unique`'s own message — which a plain function
/// parameter cannot carry. `size` sweeps past the vector's own growth, so the
/// store `freeze()` relabels sometimes has spare capacity and sometimes none —
/// `Vector.push`'s doubling means a `size` that is itself a power of two lands
/// exactly full.
export fn freezesInto(size: Int) -> Int {
  var v = Vector.of(0)
  var i = 1
  while i < size {
    v.push(i)
    i = i + 1
  }
  let array = v.freeze()
  var total = 0
  var at = 0
  while at < array.length() {
    match array.get(at) {
      Some(x) => total = total + x
      None => total = total - 1
    }
    at = at + 1
  }
  total + counts(0)
}

/// A refused caller, so `freezesInto`'s build-push-freeze-read is reached
/// across the boundary rather than run in the outermost, always-encoded frame.
export fn callsFreezesInto(size: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  freezesInto(size)
}

/// `String`-keyed searches, whose binary-search steps are `Cmp(Str, Order)`:
/// `std.set.contains<String>` and `std.map.get<String, Int>` compile since the
/// string order became a leaf helper (#378, Q4.14). The keys hold the empty
/// string, a prefix chain, a difference in the ninth byte and a two-byte
/// character; the probes add misses on either side of each. Each probe shifts
/// one bit into the answer, so a single wrong order changes it.
export fn ordersStrings(which: Int) -> Int {
  let keys = Set.of(\"\", \"a\", \"ab\", \"abc\", \"abcdefghi\", \"abcdefghj\", \"h\u{e9}llo\", \"Z\")
  let ranks = Map.of(
    MapEntry(key: \"abcdefghi\", value: 3),
    MapEntry(key: \"h\u{e9}llo\", value: 5),
    MapEntry(key: \"\", value: 7),
    MapEntry(key: \"ab\", value: 11),
  )
  let probes = [\"\", \"a\", \"aa\", \"ab\", \"abcd\", \"abcdefgh\", \"abcdefghi\", \"abcdefghk\", \"hello\", \"h\u{e9}llo\", \"Z\", \"zz\"]
  var found = which
  var at = 0
  while at < probes.length() {
    match probes.get(at) {
      Some(probe) => {
        found = found * 2
        if keys.contains(probe) {
          found = found + 1
        }
        match ranks.get(probe) {
          Some(rank) => found = found + rank * 10000
          None => found = found
        }
      }
      None => found = found - 1
    }
    at = at + 1
  }
  found + counts(0)
}

/// A refused caller, so `ordersStrings`' searches run behind a compiled frame.
export fn callsOrdersStrings(which: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  ordersStrings(which)
}

/// Allocations from a compiled frame that are **kept**, so the heap runs out.
///
/// Every array goes into the vector, so nothing a collection could reclaim is
/// unreachable and the allocation helper eventually meets the refusal
/// `Machine::allocate` raises when neither the bump nor the collection can satisfy
/// it. The sentence is the runtime\'s — this crate names errors and never builds
/// one — so what the differential says is that the *same* sentence arrives.
export fn fillsTheHeap(given: Vector<Array<Int>>, n: Int) -> Int {
  var v = given
  var at = 0
  while at < n {
    let made = [at, at, at, at, at, at, at, at]
    v.push(made)
    at = at + 1
  }
  at + counts(0)
}

/// A refused caller, so the allocations that exhaust the heap are a compiled
/// frame\'s.
export fn callsFillsTheHeap(n: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  var v: Vector<Array<Int>> = Vector.of([0])
  fillsTheHeap(v, n)
}

/// **A builder allocated, appended to and finished in one compiled frame.**
///
/// [ADR 0052]'s four, as a Cove program reaches them: `withCapacity` is an
/// `growable-alloc`, each `append` an `growable-extend`, and `finish` a
/// `run-finish`. Every one of them is a call into the runtime — see
/// `cove_native::abi`'s `GrowableFn` for why all four are and none is half emitted
/// — so what a differential case says is that the *frame around them* is compiled
/// and the answer is still the VM's, byte for byte.
///
/// The capacity is deliberately small, so a caller that hands it more than four
/// bytes makes the store grow: growth allocates a larger run, copies the live
/// prefix and replaces the owner's store word, and a caller that could only see
/// the length would pass whatever the copy did.
export fn builds(a: String, b: String) -> String {
  var out = StringBuilder.withCapacity(4 + counts(0))
  out.append(a)
  out.append(b)
  out.finish()
}

/// A refused caller, so the whole build is reached across the boundary.
export fn callsBuilds(a: String, b: String) -> String {
  let nothing = Shared(0).lock(fn(v) { v })
  builds(a, b)
}

/// One byte appended and the run finished, in a compiled frame.
///
/// `appendByte` can put anything a byte can hold into the run, so `finish` is
/// where a run that is not valid UTF-8 is caught — and a value that is not a byte
/// at all is caught before that. Both sentences are the runtime's, and
/// `cove-native` names errors rather than building them, so what this says is
/// that the *same* sentence arrives from a compiled frame.
export fn buildsAByte(n: Int) -> String {
  var out = StringBuilder.withCapacity(4 + counts(0))
  out.appendByte(n)
  out.finish()
}

/// A refused caller, so the refusal crosses the boundary.
export fn callsBuildsAByte(n: Int) -> String {
  let nothing = Shared(0).lock(fn(v) { v })
  buildsAByte(n)
}

/// `buildsAByte` with the byte appended in a standard-library *frame*.
///
/// `appendByte` is expanded wherever it is called, so a fault in it is raised in
/// its caller's frame. `appendByteBelow` — [`PROBE`], installed in
/// `std.stringbuilder` — calls itself, so no expansion reaches it, and the call
/// here is one a compiled frame waits on.
export fn buildsAByteBelow(n: Int) -> String {
  var out = StringBuilder.withCapacity(4 + counts(0))
  stringbuilder.appendByteBelow(var out, n, 0)
  out.finish()
}

/// A refused caller, so the refusal crosses the boundary.
export fn callsBuildsAByteBelow(n: Int) -> String {
  let nothing = Shared(0).lock(fn(v) { v })
  buildsAByteBelow(n)
}

/// Refused, and it appends to a builder it was lent.
export fn addsTo(var out: StringBuilder, text: String) {
  let nothing = Shared(0).lock(fn(v) { v })
  out.append(text)
}

/// A builder whose `growable-alloc` and `run-finish` are compiled and whose
/// middle append is the VM's.
///
/// ADR 0052's whole point as a tier question: the owner is stable, so the growth
/// that happens in an *encoded* frame is visible to the compiled one that made it
/// — there is nothing to be visible of, they are naming one owner. A builder that
/// reallocated itself, or a tier that copied the handle rather than the address,
/// loses the bytes the other tier appended.
export fn buildsAcross(a: String, b: String) -> String {
  var out = StringBuilder.withCapacity(4 + counts(0))
  out.append(a)
  addsTo(var out, b)
  out.finish()
}

/// A refused caller, so the outermost frame is not the one under test.
export fn callsBuildsAcross(a: String, b: String) -> String {
  let nothing = Shared(0).lock(fn(v) { v })
  buildsAcross(a, b)
}

/// A half-built run held live across an allocation that collects.
///
/// The bytes appended so far are reachable from the store, the store from the
/// owner, and the owner from one `Repr::Ref` slot of **this** frame — so the walk
/// `cove_native::abi` describes, where a live reference is in its slot at every
/// instruction boundary, is what stands between the array being allocated and the
/// half-built string being swept. The
/// answer is the finished byte length, so a run that lost its prefix is a wrong
/// number rather than a crash.
export fn buildsWhileCollecting(s: String, n: Int) -> Int {
  var out = StringBuilder.withCapacity(2 + counts(0))
  out.append(s)
  let made = [n, n + 1, n + 2, n + 3]
  out.append(s)
  out.finish().byteLength() + made.length()
}

/// A refused caller, so the frame the builder lives in is a compiled one.
export fn callsBuildsWhileCollecting(s: String, n: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  buildsWhileCollecting(s, n)
}

/// `n` bytes appended one at a time to a builder with room for two, with an
/// array allocated after every eighth.
///
/// A byte push into spare capacity is emitted code and one that grows is the
/// runtime's, so a run of `n` pushes alternates between the two: every push that
/// finds the store full replaces it, and the pushes after it blend into the new
/// store. The arrays are what make a collection land between two pushes when the
/// heap is small. The answer folds every byte of the finished text in order,
/// with the length and the arrays' elements beside it, so a byte blended at the
/// wrong offset, a length bumped twice and a prefix lost to a growth or a
/// collection are each a wrong number.
export fn pushesBytes(n: Int) -> Int {
  var out = StringBuilder.withCapacity(2 + counts(0))
  var made = 0
  var i = 0
  while i < n {
    out.appendByte(97 + i % 26)
    if i % 8 == 7 {
      let held = [i, i + 1, i + 2, i + 3]
      made = made + held.length()
    }
    i = i + 1
  }
  let text = out.finish()
  var folded = 0
  var at = 0
  while at < text.byteLength() {
    folded = (folded * 31 + text.byteAt(at)) % 1000003
    at = at + 1
  }
  folded * 1000000 + text.byteLength() * 1000 + made
}

/// A refused caller, so the frame the pushes are made in is a compiled one.
export fn callsPushesBytes(n: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  pushesBytes(n)
}

/// A refused caller, so the frame that clears a slot is a **compiled** one.
///
/// `Vm::invoke` enters the encoded tier for the frame it opens itself, so
/// `keepsWhatItStillNeeds` invoked directly runs the *VM's* `clear` arm and says
/// nothing about the emitted one. `cove_runtime::NativeSession` is the other way
/// to reach it — that is what the collection case below uses, because it needs a
/// small heap at the same time — and this is the way the differential rows above
/// already work.
export fn callsKeepsWhatItStillNeeds(a: String, b: String, n: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  keepsWhatItStillNeeds(a, b, n)
}

/// A compiled loop over one operation that never reaches the runtime and one that
/// sometimes does: `String.byteLength()` is an expanded `Inst::Len`, and
/// `Vector.push` is an expanded word `growable-push`, an emitted fast path whose
/// growth is the `growable` helper.
/// `counts(0)` for `pushesOnto`'s reason.
export fn measuresAndPushes(s: String, given: Vector<Int>, n: Int) -> Int {
  var v = given
  var at = 0
  while at < n {
    v.push(s.byteLength())
    at = at + 1
  }
  at + counts(0)
}

/// `Vector.toArray` in a compiled frame: an allocation and one word `run-copy` of
/// the whole live prefix, which `cove_native::RunCopyFn` hands to the runtime.
///
/// The push after the snapshot is what makes it a snapshot rather than a view: an
/// array that shared the store would read the pushed element or a store the push
/// replaced. `counts(0)` for `pushesOnto`'s reason.
export fn snapshots(given: Vector<Point>, n: Int) -> Int {
  var v = given
  var at = 1
  while at < n {
    v.push(Point(x: at, y: at * 10))
    at = at + 1
  }
  let held = v.toArray()
  v.push(Point(x: 1000000, y: 1000000))
  var total = 0
  var i = 0
  while i < held.length() {
    match held.get(i) {
      Some(p) => total = total + p.x + p.y
      None => total = total - 1
    }
    i = i + 1
  }
  total * 1000 + held.length() + counts(0)
}

/// A refused caller, so the snapshot is taken across the boundary.
export fn callsSnapshots(n: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  var v = Vector.of(Point(x: 0, y: 1))
  snapshots(v, n)
}

/// A snapshot of **references** that is the only thing holding them, across
/// allocations that collect, in a compiled frame.
///
/// The vector is built here and then replaced, so once `held` is taken the arrays
/// it names are reachable from `held` and from nothing else — and `held` is one
/// `Repr::Ref` slot of this compiled frame. The garbage loop allocates far more
/// than the pushes did, so a collection lands in it far more often than not, and
/// the sum over every element's second word is what a swept array would change.
export fn snapshotsWhileCollecting(n: Int) -> Int {
  var v = Vector.of([counts(0), 1])
  var at = 1
  while at < n {
    v.push([at, at + 1])
    at = at + 1
  }
  let held = v.toArray()
  v = Vector.of([0, 0])
  var garbage = 0
  while garbage < 64 {
    let made = [garbage, garbage, garbage, garbage, garbage, garbage, garbage, garbage]
    garbage = garbage + made.length() - 7
  }
  var total = 0
  var i = 0
  while i < held.length() {
    match held.get(i) {
      Some(pair) => match pair.get(1) {
        Some(x) => total = total + x
        None => total = total - 1
      }
      None => total = total - 1
    }
    i = i + 1
  }
  total + v.length()
}

/// A refused caller, so the frame holding the snapshot is a compiled one.
export fn callsSnapshotsWhileCollecting(n: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  snapshotsWhileCollecting(n)
}

/// A refused caller making `n` calls to `String.indexOf`, which nothing
/// lowers, before handing the same `n` to the compiled loop above.
export fn countsTheBoundary(s: String, n: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  var cut = 0
  var at = 0
  while at < n {
    match s.indexOf(\"h\") {
      Some(_) => cut = cut + 1
      None => cut = cut - 1
    }
    at = at + 1
  }
  var v = Vector.of(7)
  measuresAndPushes(s, v, n) * 1000 + cut
}

/// `String.contains` in a compiled loop: an intrinsic whose declared effects
/// neither allocate nor raise, so the call is a plain one — no work published,
/// no program counter, no outcome tested, and the frame pointer kept across it.
/// `counts(0)` for the reason it is in every fixture above.
export fn containsIn(s: String, n: Int) -> Int {
  var found = counts(0)
  var at = 0
  while at < n {
    if s.contains(\"needle\") {
      found = found + 1
    }
    at = at + 1
  }
  found * 1000 + s.byteLength()
}

/// A refused caller, so the loop is reached across the boundary.
export fn callsContainsIn(s: String, n: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  containsIn(s, n)
}

/// `String.split` over a separator the caller chose: an intrinsic that may raise,
/// because an empty separator is the language's own refusal.
export fn splitsOn(s: String, on: String) -> Int {
  s.split(on).length() * 1000 + counts(0) + s.byteLength()
}

/// A refused caller, so the refusal is raised under compiled code.
export fn callsSplitsOn(s: String, on: String) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  splitsOn(s, on)
}

/// `String.trim` in a compiled loop, with an `Array` and the receiver live in the
/// compiled frame across every call: an intrinsic that allocates, so the call is
/// a safepoint and may collect.
export fn trimsAndKeeps(s: String, n: Int) -> Int {
  let kept = [n, n + 1, n + 2]
  var total = counts(0)
  var at = 0
  while at < n {
    total = total + s.trim().byteLength()
    at = at + 1
  }
  total * 1000 + kept.length() + s.byteLength()
}

/// A refused caller holding a reference of its own across the calls.
export fn callsTrimsAndKeeps(s: String, n: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  let also = [n, n]
  trimsAndKeeps(s, n) + also.length()
}
";

/// A standard-library body that can fault and that no expansion reaches.
///
/// Installed in `std.stringbuilder` by [`checked`], because the library has no
/// such body of its own once the builder's `var self` methods are expanded
/// wherever they are called: a fault in the library under a compiled frame
/// that waits on a library call needs a library call to wait on.
const PROBE: &str = "\
/// `appendByte`, `depth` frames down a recursion no expansion can reach.
export fn appendByteBelow(var out: StringBuilder, value: Int, depth: Int) {
  if depth > 0 {
    appendByteBelow(var out, value, depth - 1)
  } else {
    out.appendByte(value)
  }
}
";

fn checked() -> (Arc<SourceMap>, Arc<cove_sema::resolve::Program>) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("m/main.cove");
    let file = sources.add(path.clone(), SOURCE);
    let ast = cove_syntax::parse_file(&sources, file).expect("the fixture parses");
    let mut modules = BTreeMap::from([(
        MODULE.to_string(),
        Module {
            name: MODULE.to_string(),
            dir: PathBuf::from(MODULE),
            units: vec![Unit { file, path, ast }],
        },
    )]);
    for (name, module) in cove_sema::stdlib::attach(&mut sources).expect("stdlib parses") {
        modules.insert(name, module);
    }
    let path = PathBuf::from("std/stringbuilder_probe.cove");
    let file = sources.add_library(path.clone(), PROBE);
    let ast = cove_syntax::parse_file(&sources, file).expect("the probe parses");
    modules
        .get_mut("std.stringbuilder")
        .expect("the standard library has `std.stringbuilder`")
        .units
        .push(Unit { file, path, ast });
    let package = Package {
        root: PathBuf::new(),
        config: Config::default(),
        modules,
    };
    match Compiler::new().compile(&package) {
        Ok(program) => (Arc::new(sources), Arc::new(program)),
        Err(items) => panic!(
            "the fixture checks:\n{}",
            items
                .iter()
                .map(|item| cove_diag::render(&sources, item))
                .collect::<Vec<_>>()
                .join("")
        ),
    }
}

/// What one entry answered on each tier, and how the native run's calls divided.
///
/// The answers are **rendered** rather than held, because `Value` is not
/// `PartialEq` — the public boundary type deliberately answers questions rather
/// than comparing — and what a differential case needs is that the two runs are
/// indistinguishable to a reader. `Display` is that: a wrong word, a wrong width
/// or a wrong case reads differently.
struct Both {
    vm: Result<String, String>,
    native: Result<String, String>,
    tiers: cove_runtime::Tiers,
    compiled: usize,
    reachable: usize,
    /// The stack origin the native run's task had, which is `0` on the first
    /// segment.
    ///
    /// Part of the answer rather than an assumption, for
    /// [`Segment::Later`]'s reason: a case that asked for a later segment and
    /// silently got the first one is the blind case again.
    origin: u64,
}

/// Which stack segment the runs are on.
///
/// **A `Repr::Addr` word is a linear index and a frame slot's index is
/// segment-relative, and on segment 0 those are the same number.** The entry task
/// of every run is segment 0, so a case that drives an address through the real
/// runtime cannot tell an arm that added `NativeCtx::stack_origin` from one that
/// forgot to: `origin` is nought and both arms answer alike. Mutation testing
/// found exactly that — the origin dropped in the template compiler's
/// `frame_addr`, and dropped in its `word_ptr`, passes every other case in this
/// file and every one of the runtime's own suites — and the only file that catches
/// either is `cove-native`'s, which sets a `NativeCtx` origin by hand and so says
/// nothing about the runtime the words come from.
///
/// [`Segment::Later`] closes that: the run is moved onto a further segment before
/// it has executed anything, so the origin is a segment's worth of words and an
/// index is *not* an address. Everything else about the run is what it was — the
/// same `Space`, the same heap, the same literals at the same addresses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Segment {
    /// The entry task's, where `stack_origin` is nought.
    First,
    /// A further one, where it is not.
    Later,
}

/// Runs `module.name` once on the encoded VM and once on the native tier.
///
/// The table is built **once, before either run**, and the `NativeProgram` outlives
/// the `Vm` it is given to, because it owns the pages the entries point into. It is
/// also never rebuilt between the two runs: finalizing a mapping twice is the thing
/// ADR 0055's W^X rule forbids, and one table for a whole process is what the
/// design is.
fn both(name: &str, args: Vec<Value>) -> Both {
    both_on(Segment::First, name, args)
}

/// [`both`], with the segment both runs execute on named.
///
/// The encoded run is moved too, and not only the native one. It is the reference
/// either way — the case that uses this compares a later segment's answers against
/// the *first* segment's encoded answer as well — and moving it says the thing a
/// differential case on one tier could not: that the encoded arms resolve an
/// address the same way wherever the task's words begin.
fn both_on(segment: Segment, name: &str, args: Vec<Value>) -> Both {
    let (sources, program) = checked();
    let lowered = Arc::new(
        cove_ir::lower(&program, &sources, &cove_sema::HostSchemas::new())
            .expect("the fixture lowers"),
    );
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(
        Arc::clone(&program),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    let native = cove_runtime::compile_native(&lowered).expect("this host compiles");
    let said = |answer: Result<Value, cove_runtime::RuntimeError>| {
        answer
            .map(|value| value.to_string())
            .map_err(|error| error.message)
    };
    // The segment is chosen before either run executes anything, which is what
    // the seam requires: a frame that is already standing holds a base into the
    // segment it was pushed in.
    let moved = |vm: &mut Vm<'_>| match segment {
        Segment::First => 0,
        Segment::Later => {
            let origin = vm.on_a_later_stack_segment();
            assert!(origin > 0, "a later segment does not begin at word nought");
            origin
        }
    };
    let vm = {
        let mut vm = Vm::new(&runtime, &hosts, &lowered);
        moved(&mut vm);
        said(vm.invoke(MODULE, name, args.clone()))
    };
    let mut with = Vm::with_native(&runtime, &hosts, &lowered, &native);
    let origin = moved(&mut with);
    let answered = said(with.invoke(MODULE, name, args));
    Both {
        vm,
        native: answered,
        tiers: with.tiers(),
        compiled: native.compiled(),
        reachable: native.reachable(),
        origin,
    }
}

/// The table compiles something, and refuses something, and says which.
#[test]
fn the_table_is_built_once_and_reports_its_mixture() {
    let both = both("callsAdds", vec![Value::int(20), Value::int(22)]);
    assert_eq!(both.vm, Ok("42".to_string()), "the fixture answers `a + b`");
    assert_eq!(both.native, both.vm, "and the native tier answers the same");
    assert!(
        both.compiled >= 3,
        "the arithmetic callees compiled: {} of {}",
        both.compiled,
        both.reachable
    );
    assert!(
        both.compiled < both.reachable,
        "and the caller did not, which is what makes the crossing a crossing"
    );
}

/// **A VM-to-native call, in machine code.**
///
/// The encoded `CALL` arm consulted the table, entered a compiled `adds`, and the
/// answer arrived in the destination the lowering settled — through a real
/// prologue, a real argument copy and a real `ret`.
#[test]
fn the_encoded_call_arm_enters_compiled_machine_code() {
    let both = both("callsAdds", vec![Value::int(20), Value::int(22)]);
    assert_eq!(both.native, both.vm);
    assert!(
        both.tiers.vm_to_native >= 1,
        "the crossing was taken: {:?}",
        both.tiers
    );
    assert!(
        both.tiers.host_to_vm >= 1,
        "and the caller it crossed from was the VM's, so the run really was mixed: {:?}",
        both.tiers
    );
    assert!(
        both.compiled < both.reachable,
        "which is only true because something was refused"
    );
}

/// A native-to-native direct call, and a native-to-VM one, in the same run.
///
/// `counts` calls itself, which is PR #368's direct protocol between two compiled
/// functions; `callsCounts` reaches it from the VM. The deep recursion is also the
/// reallocation case: three hundred frames of pending destinations while
/// `push_frame` resizes the stack's `Vec` under them, which is the reason the ABI
/// carries indices and not pointers.
#[test]
fn a_deep_native_recursion_returns_through_a_reallocation() {
    const DEEP: i64 = 300;
    let both = both("callsCounts", vec![Value::int(DEEP)]);
    assert_eq!(both.vm, Ok(DEEP.to_string()));
    assert_eq!(
        both.native, both.vm,
        "every frame returned into the one below"
    );
    assert!(
        both.tiers.vm_to_native >= 1,
        "the VM entered the recursion: {:?}",
        both.tiers
    );
    assert!(
        both.tiers.native_to_native_direct >= DEEP as u64,
        "and every frame of it called the next directly: {:?}",
        both.tiers
    );
}

/// A raise in machine code crosses into the VM as the sentence the VM would have
/// written.
///
/// `cove-native` names the operation and `cove-runtime` writes the sentence, so a
/// `/` by zero in compiled code says word for word what a dispatched one says.
#[test]
fn a_raise_in_machine_code_is_the_vm_s_sentence() {
    let both = both("callsDivides", vec![Value::int(1), Value::int(0)]);
    let vm = both.vm.expect_err("the vm refuses a zero divisor");
    let native = both.native.expect_err("and so does compiled code");
    assert_eq!(native, vm, "the same sentence across the boundary");
    assert!(vm.contains("zero"), "and it says what happened: {vm}");
}

/// **Negation in machine code, and the one input it refuses.**
///
/// `Inst::Neg` over `Num::Int` is `checked_neg`, and the two arms of this crate
/// reach its `None` differently — the template arm on the flag its `neg` set, the
/// Cranelift arm on a comparison against `i64::MIN` — so what is asserted here is
/// the *sentence*: `cove-native` names the operation and `cove-runtime` writes the
/// error, and a negation that overflowed in compiled code has to say word for word
/// what a dispatched one says.
#[test]
fn negation_and_its_overflow_are_the_vm_s() {
    on_each_tier(&["negates"], &["callsNegates"]);

    let ordinary = both("callsNegates", vec![Value::int(42)]);
    assert_eq!(
        ordinary.vm,
        Ok("-42".to_string()),
        "the fixture answers `-a`"
    );
    assert_eq!(
        ordinary.native, ordinary.vm,
        "and compiled code answers the same"
    );
    assert!(
        ordinary.tiers.vm_to_native >= 1,
        "the crossing into the compiled negation was taken: {:?}",
        ordinary.tiers
    );

    let least = both("callsNegates", vec![Value::int(i64::MIN)]);
    let vm = least.vm.expect_err("the vm refuses to negate `i64::MIN`");
    let native = least.native.expect_err("and so does compiled code");
    assert_eq!(native, vm, "the same sentence across the boundary");
    assert!(
        vm.contains("negation"),
        "and it names the operation rather than renaming it: {vm}"
    );
}

/// **A fault in a standard-library body, raised under compiled code, is blamed on
/// its caller exactly as the VM blames it — whether the body was expanded or
/// called.**
///
/// ADR 0058's "Fallibility preserves the source call site's blame".
/// `RuntimeError::with_chain` moves the blame, from the span and the call sites it
/// is handed, so what has to be the VM's is every one of those: the primary span,
/// the library context and the chain.
///
/// Three fixtures, for the two ways a call site reaches the rule. `absolutes`
/// expands `abs` and `buildsAByte` expands `appendByte`, a `var self` method, so
/// their call sites come out of `Function::inlined` at the faulting
/// instruction. `buildsAByteBelow` *calls* the probe `appendByteBelow`, which
/// calls itself and so is never expanded, and its call site is read from a
/// compiled frame waiting on that call, whose program counter compiled code
/// syncs to the call itself rather than to the instruction after it.
#[test]
fn a_fault_in_a_library_body_under_compiled_code_is_blamed_on_its_caller() {
    on_each_tier(
        &["absolutes", "buildsAByte", "buildsAByteBelow"],
        &[
            "callsAbsolutes",
            "callsBuildsAByte",
            "callsBuildsAByteBelow",
        ],
    );

    let (sources, program) = checked();
    let lowered = Arc::new(
        cove_ir::lower(&program, &sources, &cove_sema::HostSchemas::new())
            .expect("the fixture lowers"),
    );
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(
        Arc::clone(&program),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    let calls = |caller: &str, callee: &str| {
        lowered
            .functions
            .iter()
            .find(|f| &*f.module == MODULE && &*f.name == caller)
            .expect("the fixture is lowered")
            .code
            .iter()
            .any(|inst| {
                matches!(inst, cove_ir::Inst::Call { callee: id, .. }
                    if lowered.function(*id).name.ends_with(callee))
            })
    };
    assert!(
        !calls("buildsAByte", "appendByte"),
        "`appendByte` is expanded into `buildsAByte`"
    );
    assert!(
        calls("buildsAByteBelow", "appendByteBelow"),
        "`appendByteBelow` is a call, which is what makes it the framed case"
    );
    let native = cove_runtime::compile_native(&lowered).expect("this host compiles");
    let text = |span: cove_diag::Span| {
        sources.get(span.file).text[span.start as usize..span.end as usize].to_string()
    };
    let blame = |answer: Result<Value, cove_runtime::RuntimeError>| {
        let error = answer.expect_err("the fixture refuses its argument");
        (
            error.message.clone(),
            error.span,
            error.library_sites().to_vec(),
            error.chain().to_vec(),
        )
    };
    for (entry, arg, called, caller) in [
        ("callsAbsolutes", i64::MIN, "a.abs()", "absolutes(a)"),
        (
            "callsBuildsAByte",
            300,
            "out.appendByte(n)",
            "buildsAByte(n)",
        ),
        (
            "callsBuildsAByteBelow",
            300,
            "stringbuilder.appendByteBelow(var out, n, 0)",
            "buildsAByteBelow(n)",
        ),
    ] {
        let args = vec![Value::int(arg)];
        let vm = blame(Vm::new(&runtime, &hosts, &lowered).invoke(MODULE, entry, args.clone()));
        let mut with = Vm::with_native(&runtime, &hosts, &lowered, &native);
        let compiled = blame(with.invoke(MODULE, entry, args));
        assert!(
            with.tiers().vm_to_native >= 1,
            "`{entry}`: the fault was raised under compiled code: {:?}",
            with.tiers()
        );
        assert_eq!(
            compiled, vm,
            "`{entry}`: compiled code blames what the VM blames"
        );

        let (message, span, library, chain) = vm;
        let span = span.expect("a fault carries a span");
        assert!(
            !sources.is_library(span.file),
            "`{entry}` ({message}): the primary span is the caller's"
        );
        assert_eq!(text(span), called, "`{entry}`: and it is the call");
        assert!(
            !library.is_empty() && library.iter().all(|site| sources.is_library(site.file)),
            "`{entry}`: the library's own line is kept as context: {library:?}"
        );
        assert_eq!(
            chain.iter().map(|site| text(*site)).collect::<Vec<_>>(),
            [caller],
            "`{entry}`: and the refused caller is still named"
        );
    }
}

/// Which reachable functions the template compiler took, by name.
///
/// Every case below asserts *which side of the boundary each fixture is on*, and
/// not merely that the two tiers agreed. A fixture that quietly moved to the
/// other tier — a lowering change that refused `bumps`, or one that compiled
/// `shows` — would make its case compare the VM against itself and pass whatever
/// the code generator emitted.
fn compiled_names() -> Vec<String> {
    let (sources, program) = checked();
    let lowered = Arc::new(
        cove_ir::lower(&program, &sources, &cove_sema::HostSchemas::new())
            .expect("the fixture lowers"),
    );
    let native = cove_runtime::compile_native(&lowered).expect("this host compiles");
    let refused: Vec<&str> = native
        .refusals()
        .iter()
        .map(|row| row.name.as_str())
        .collect();
    lowered
        .functions
        .iter()
        .filter(|f| !f.stub)
        .map(|f| format!("{}.{}", f.module, f.name))
        .filter(|name| !refused.contains(&name.as_str()))
        .collect()
}

/// Asserts the tiers each fixture is meant to be on.
fn on_each_tier(compiled: &[&str], refused: &[&str]) {
    let names = compiled_names();
    for name in compiled {
        let full = format!("{MODULE}.{name}");
        assert!(
            names.contains(&full),
            "`{name}` is meant to be compiled, and the tier took {names:?}"
        );
    }
    for name in refused {
        let full = format!("{MODULE}.{name}");
        assert!(
            !names.contains(&full),
            "`{name}` is meant to be refused, and the tier took it"
        );
    }
}

/// **A `var` parameter written through by a compiled callee, read by its encoded
/// caller.**
///
/// The word the callee is handed is a *linear address* of a slot of the caller's
/// frame, and generated code has to resolve it the way `Memory::write` does — the
/// `is_stack` branch, and then `words[addr - stack_origin]`. An arm that resolved
/// it as a segment-relative index instead would write a million words away, which
/// on the first segment is the same word and everywhere else is not.
#[test]
fn a_var_parameter_is_written_through_by_compiled_code() {
    on_each_tier(&["bumps"], &["callsBumps"]);
    let both = both("callsBumps", vec![Value::int(20)]);
    // `total` is `a + 5` and so is what the callee answered: `25 * 1000 + 25`.
    assert_eq!(both.vm, Ok("25025".to_string()));
    assert_eq!(
        both.native, both.vm,
        "the store through the address landed in the caller's slot"
    );
    assert!(
        both.tiers.vm_to_native >= 1,
        "and it crossed the boundary to get there: {:?}",
        both.tiers
    );
}

/// One word of a two-word value location, through `Inst::AddrOfPart`.
///
/// A place is the address of the *first* word of a value location, so a field of
/// one is at a static offset from it — and writing `p.y` must leave `p.x` alone,
/// which is the whole reason the instruction exists rather than a load of both
/// words, a change to one and a store of both.
#[test]
fn a_place_names_one_word_of_an_inline_value() {
    on_each_tier(&["movesY"], &["callsMovesY"]);
    let both = both("callsMovesY", vec![Value::int(20)]);
    // `x` is still `a`, `y` is `9`, and the callee answered `y`: `20 * 1000 + 9 * 10 + 9`.
    assert_eq!(
        both.vm,
        Ok("20099".to_string()),
        "`x` untouched and `y` set"
    );
    assert_eq!(both.native, both.vm);
    assert!(both.tiers.vm_to_native >= 1, "{:?}", both.tiers);
}

/// **The address going the other way**: formed in machine code, followed by the
/// encoded tier.
///
/// `lendsToTheVm` is compiled and `shows` is not, so the `addr-of-slot` is emitted
/// code and the `store` through it is `encoded.rs`'s own arm. The two have to agree
/// about what the word means, and this is the case that says they do — the reverse
/// of the one above, and it fails differently: a wrong address here is a wrong
/// word written by the *VM*, into whatever the number happened to name.
#[test]
fn an_address_formed_in_machine_code_is_followed_by_the_vm() {
    on_each_tier(&["lendsToTheVm"], &["shows", "callsLendsToTheVm"]);
    let both = both("callsLendsToTheVm", vec![Value::int(20)]);
    // `a + 1` in this frame's slot, and `a + 1` answered: `21 * 1000 + 21`.
    assert_eq!(both.vm, Ok("21021".to_string()));
    assert_eq!(both.native, both.vm);
    assert!(
        both.tiers.native_to_vm >= 1,
        "a compiled frame called an encoded one: {:?}",
        both.tiers
    );
}

/// One address carried down an alternating chain of compiled and encoded frames,
/// deep enough that the stack's `Vec` reallocates under all of them.
///
/// This is the case the ABI's "an address is an index precisely so it survives
/// that" is about, one step further out than ADR 0057's destination: a *linear*
/// address is relative to a segment origin that does not move, so it stays correct
/// across every `push_frame` the chain makes — and the slot it names is in the
/// bottom frame, which is three hundred frames below where the last write happens.
#[test]
fn a_var_survives_a_reallocation_under_an_alternating_chain() {
    const DEEP: i64 = 300;
    on_each_tier(&["descends"], &["lowers"]);
    let both = both("threads", vec![Value::int(DEEP)]);
    // `300` in the bottom frame's slot, and `300` answered from the top of the
    // chain: `300 * 1000 + 300`.
    assert_eq!(
        both.vm,
        Ok(format!("{}", DEEP * 1000 + DEEP)),
        "every step of the chain added one to the same word"
    );
    assert_eq!(both.native, both.vm);
    assert!(
        both.tiers.vm_to_native >= DEEP as u64,
        "and every other step of it crossed into machine code: {:?}",
        both.tiers
    );
}

/// **A `Clear` in a compiled frame, and a collection after it.**
///
/// `Inst::Clear` exists for what it stops happening: a reference slot the frame no
/// longer needs holds null, so the collector reads null and the object is
/// unreachable. Generated code writes those zeroes itself, so a clear that missed
/// a word would keep an object alive forever and a clear that wrote one too many —
/// or the wrong slot — would *drop* one the frame still needs. This case catches
/// the second, which is the dangerous direction: `b` is read after an allocation
/// that collects, and if its slot had been zeroed the object would have been swept
/// and the byte read out of reclaimed words.
///
/// It is a [`cove_runtime::NativeSession`] rather than [`both`] because it needs
/// two things at once that no constructor offers together: a **small heap**, so
/// that a collection happens at all, and a **native tier**. A session takes the
/// entry table per call, which is exactly the pair.
///
/// The loop runs until a collection has actually happened. A case that depends on
/// a heap size staying small enough proves nothing the day it stops being.
#[test]
fn a_cleared_slot_is_not_a_root_and_a_live_one_still_is() {
    // One heap chunk, which is the smallest a heap is: the slices `allocates`
    // builds do not all fit in it.
    const SMALL_HEAP_WORDS: usize = 1 << 13;
    const AT: i64 = 64;
    let text = "a string long enough that slicing it fills a heap chunk, and long enough that a \
                byte can be read out of the middle of it without asking whether it is there.";
    on_each_tier(&["keepsWhatItStillNeeds"], &["allocates"]);

    let (sources, program) = checked();
    let lowered = Arc::new(
        cove_ir::lower(&program, &sources, &cove_sema::HostSchemas::new())
            .expect("the fixture lowers"),
    );
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(
        Arc::clone(&program),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    let native = cove_runtime::compile_native(&lowered).expect("this host compiles");

    let mut vm = Vm::with_heap_words(&runtime, &hosts, &lowered, SMALL_HEAP_WORDS);
    let calls = {
        let mut session = vm
            .native_session(
                MODULE,
                "keepsWhatItStillNeeds",
                vec![Value::string(text), Value::string(text), Value::int(AT)],
            )
            .expect("the session opens");
        let words = session.arguments().to_vec();
        let expected = session
            .call(&cove_runtime::NothingCompiled, &words)
            .expect("the vm answers");
        let bytes = text.as_bytes();
        assert_eq!(
            expected,
            vec![(u64::from(bytes[0]) + u64::from(bytes[1]) + AT as u64)],
            "the fixture answers two bytes of the strings and the slice's length"
        );

        let before = session.collections();
        let mut calls = 0;
        while session.collections() == before && calls < 20_000 {
            let answered = session
                .call(&native, &words)
                .expect("the native tier answers");
            assert_eq!(answered, expected, "call {calls} answered wrongly");
            calls += 1;
        }
        assert!(
            session.collections() > before,
            "no collection ran in {calls} call(s), so this case proved nothing"
        );
        calls
    };
    // And the other direction, as far as this can say it: the run handed out far
    // more words than the heap ever held, so the objects whose last reference was
    // a slot the lowering cleared *were* reclaimed. What says a cleared slot holds
    // null is `cove-native`'s own suite, which reads the words; this says the
    // collector then did something with the null.
    assert!(
        vm.allocated_words() > SMALL_HEAP_WORDS as u64,
        "{} word(s) handed out over {calls} call(s) of a {SMALL_HEAP_WORDS}-word heap, \
         so nothing was reused",
        vm.allocated_words()
    );
}

/// **Every address family, on a stack segment that does not begin at word
/// nought.**
///
/// This is the case the ones above it could not be. All of them drive real
/// addresses through real generated code, and every one of them runs on segment
/// 0 — where [`Segment`]'s note applies: the origin is nought, so an
/// `addr-of-slot` that added it and one that forgot it form the same word, and a
/// `load` that resolved a stack address as a segment-relative index reads the
/// right one. Two injected mutants — the origin dropped in the template
/// compiler's `frame_addr`, and dropped in its `word_ptr` — survive this
/// runtime's whole suite for that reason, and a *spawned* task, which is where a
/// later segment occurs in a real program, is exactly where they would bite.
///
/// So the run is moved to a later segment first and every family is driven over
/// it again:
///
/// - `callsBumps` — `addr-of-slot` in an encoded frame, `load` and `store`
///   through it in a compiled one;
/// - `callsMovesY` — `addr-of-part`, one word of two;
/// - `callsLendsToTheVm` — the address formed *in machine code* and followed by
///   the VM, which is the pair that has to agree about what the word means;
/// - `threads` — one address carried down an alternating chain deep enough to
///   reallocate the segment's `Vec` underneath it;
/// - `heapsThrough` — a `load` and a `store` through an address into the **heap**,
///   so the `is_stack` decision generated code takes is taken on both arms here;
/// - `callsKeepsWhatItStillNeeds` — a `clear` in a compiled frame. It is the one
///   member of the family that resolves a slot without forming an address at all
///   — `store_slot` from the frame base, and no `stack_origin` anywhere near it —
///   and it is here because the adoption gate lists it beside the others: a frame
///   whose zeroes landed elsewhere on a later segment would be as wrong as an
///   address that did.
///
/// Each row is asserted against the encoded VM on the same segment *and* against
/// the encoded VM on the first one, so neither a wrong answer nor a pair of runs
/// that are wrong together can pass, and against `vm_to_native`, so a row that
/// stopped crossing is a failure rather than a comparison of the VM with itself.
#[test]
fn every_address_family_resolves_on_a_later_stack_segment() {
    on_each_tier(
        &[
            "bumps",
            "movesY",
            "lendsToTheVm",
            "descends",
            "keepsWhatItStillNeeds",
            // The `lock` closure, which is the one function of this fixture that
            // is handed an address into the heap. `heapsThrough#1` is handed a
            // copy of the word instead — the VM loads it, because that closure
            // took no `var` — and is not what this row is for.
            "heapsThrough#0",
        ],
        &[
            "callsBumps",
            "callsMovesY",
            "callsLendsToTheVm",
            "lowers",
            "allocates",
            "heapsThrough",
            "callsKeepsWhatItStillNeeds",
        ],
    );
    const DEEP: i64 = 300;
    const AT: i64 = 64;
    let text = "a string long enough that a byte can be read out of the middle of it without \
                asking whether it is there.";
    let rows: Vec<(&str, Vec<Value>, &str)> = vec![
        (
            "callsBumps",
            vec![Value::int(20)],
            "addr-of-slot, and a store through it into the caller's frame",
        ),
        (
            "callsMovesY",
            vec![Value::int(20)],
            "addr-of-part: one word of a two-word value location",
        ),
        (
            "callsLendsToTheVm",
            vec![Value::int(20)],
            "an address formed in machine code and followed by the VM",
        ),
        (
            "threads",
            vec![Value::int(DEEP)],
            "one address down an alternating chain, across a reallocation",
        ),
        (
            "heapsThrough",
            vec![Value::int(20)],
            "a load and a store through an address into the heap",
        ),
        (
            "callsKeepsWhatItStillNeeds",
            vec![Value::string(text), Value::string(text), Value::int(AT)],
            "a clear in a compiled frame",
        ),
    ];
    for (name, args, what) in rows {
        let here = both(name, args.clone());
        assert_eq!(here.origin, 0, "the entry task is the first segment");
        let there = both_on(Segment::Later, name, args);
        assert!(
            there.origin > 0,
            "`{name}` was meant to run on a later segment and ran on the first"
        );
        assert_eq!(
            here.vm, there.vm,
            "`{name}` ({what}): the encoded tier answers the same on either segment"
        );
        assert_eq!(
            there.native, here.vm,
            "`{name}` ({what}): compiled code on a segment at {} answers what the encoded tier \
             answers",
            there.origin
        );
        assert!(
            there.tiers.vm_to_native >= 1,
            "`{name}` crossed into machine code on the later segment: {:?}",
            there.tiers
        );
    }
}

/// **A refusal says which builtin, or which allocation, stopped it.**
///
/// `Refused::instruction` names an opcode, and for two opcodes that is not a task
/// a reader can act on: `IntrinsicCall` is every builtin the language has and the
/// `Alloc` opcodes are every layout a program declares. `Refused::blocked` is the
/// second key, and what is asserted here is the join — that it is present for
/// exactly those two opcodes, absent for every other, and names the thing the
/// source actually wrote.
///
/// # Both tables are expected to be *empty*, and that is the assertion
///
/// `Inst::Alloc` is lowered, and so is `Inst::IntrinsicCall` for every intrinsic
/// (#378, P5-6), so no function can be refused at either
/// and both halves of the census have no rows. What follows is the allocation
/// half's argument, and the intrinsic half's is the same. Asserting that rather than deleting
/// the arm is deliberate in both directions: the arm has to stay, because
/// `Refused::blocked` is a fact about a *program* and an operand bound could still
/// refuse an allocation; and a row appearing again is news, because it would mean
/// an allocation shape had become a blocker and the ranked table needs reading
/// again. What tests the row's own formatting now is `native.rs`'s own
/// `an_allocation_census_row_names_the_family_and_the_shape`, which builds the
/// refusal by hand and so does not depend on what the subset happens to lower.
#[test]
fn a_refusal_says_which_builtin_or_which_allocation_blocked_it() {
    use cove_runtime::Blocked;
    let (sources, program) = checked();
    let lowered = Arc::new(
        cove_ir::lower(&program, &sources, &cove_sema::HostSchemas::new())
            .expect("the fixture lowers"),
    );
    let native = cove_runtime::compile_native(&lowered).expect("this host compiles");

    let mut builtins = 0;
    let mut allocations = 0;
    for row in native.refusals() {
        match (row.instruction.as_deref(), &row.blocked) {
            (Some("IntrinsicCall"), Some(Blocked::Intrinsic(named))) => {
                assert!(
                    named.contains('.'),
                    "a builtin is a receiver and an operation: `{named}` in `{}`",
                    row.name
                );
                builtins += 1;
            }
            (Some("AllocImm" | "AllocFixed" | "AllocSlot"), Some(Blocked::Allocation { .. })) => {
                allocations += 1
            }
            // Every other opcode names one operation already, so a subject for it
            // would be a column repeating the opcode. See `cove_runtime::Blocked`.
            (_, None) => {}
            (opcode, blocked) => panic!(
                "`{}` was refused at {opcode:?} and the census says {blocked:?}, \
                 which is neither of the two aggregates nor nothing",
                row.name
            ),
        }
    }
    assert_eq!(
        builtins, 0,
        "`Inst::IntrinsicCall` is lowered for every intrinsic, so nothing is refused at \
         one — see this case's own note before changing this number"
    );
    assert_eq!(
        allocations, 0,
        "`Inst::Alloc` is lowered, so nothing is refused at one — see this case's \
         own note before changing this number"
    );

    // And the one the fixture's own source writes, by name: `allocates` calls
    // `s.slice(..)`, which compiled code calls now, so it is refused for its marker.
    let named = |of: &str| {
        let full = format!("{MODULE}.{of}");
        native
            .refusals()
            .iter()
            .find(|row| row.name == full)
            .unwrap_or_else(|| panic!("`{of}` is refused"))
            .blocked
            .clone()
    };
    assert_eq!(named("allocates"), None);
    // `heapsThrough` constructs a `Shared(a)`, whose allocation now lowers — so it
    // is refused for the `store-field` that fills the object in, and an opcode that
    // names one operation already carries no second key.
    assert_eq!(named("heapsThrough"), None);
}

/// **An intrinsic that neither allocates nor raises is a plain call from compiled
/// code, and answers what the VM answers.**
///
/// `String.contains` carries neither `MAY_ALLOCATE`/`MAY_COLLECT` nor
/// `MAY_RAISE`, so both code generators emit the call with nothing around it —
/// see `cove_native::IntrinsicProtocol`. Under `debug_assertions`, which this
/// suite runs with, the runtime's helper also asserts the promise that makes that
/// sound: the stack did not move and the heap did not collect.
#[test]
fn a_plain_intrinsic_call_from_compiled_code_agrees_with_the_vm() {
    on_each_tier(&["containsIn"], &["callsContainsIn"]);
    for text in ["hay needle hay", "haystack", ""] {
        let both = both("callsContainsIn", vec![Value::string(text), Value::int(7)]);
        assert!(both.vm.is_ok(), "`{text}`: {:?}", both.vm);
        assert_eq!(both.native, both.vm, "`{text}`: compiled code agrees");
        assert!(both.tiers.vm_to_native >= 1, "`{text}`: {:?}", both.tiers);
    }
}

/// **An intrinsic that raises from compiled code reports the VM's message, span
/// and outcome.**
///
/// `String.split` refuses an empty separator. The call carries `MAY_RAISE`, so
/// the helper synchronises the program counter and compiled code tests the
/// outcome and leaves with it, publishing the unpaid work on the way out. What is
/// compared is the whole error as a caller sees it — the sentence, the primary
/// span, the library sites and the chain — and a separator that is not empty
/// answers alike too.
#[test]
fn a_raising_intrinsic_call_from_compiled_code_is_the_vm_s_refusal() {
    on_each_tier(&["splitsOn"], &["callsSplitsOn"]);
    let (sources, program) = checked();
    let lowered = Arc::new(
        cove_ir::lower(&program, &sources, &cove_sema::HostSchemas::new())
            .expect("the fixture lowers"),
    );
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(
        Arc::clone(&program),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    let native = cove_runtime::compile_native(&lowered).expect("this host compiles");
    let said = |answer: Result<Value, cove_runtime::RuntimeError>| match answer {
        Ok(value) => Ok(value.to_string()),
        Err(error) => Err((
            error.message.clone(),
            error.span,
            error.library_sites().to_vec(),
            error.chain().to_vec(),
        )),
    };
    for on in ["", ","] {
        let args = vec![Value::string("a,b,,c"), Value::string(on)];
        let vm =
            said(Vm::new(&runtime, &hosts, &lowered).invoke(MODULE, "callsSplitsOn", args.clone()));
        let mut with = Vm::with_native(&runtime, &hosts, &lowered, &native);
        let compiled = said(with.invoke(MODULE, "callsSplitsOn", args));
        assert!(
            with.tiers().vm_to_native >= 1,
            "separator `{on}`: the call was made under compiled code: {:?}",
            with.tiers()
        );
        assert_eq!(
            compiled, vm,
            "separator `{on}`: compiled code answers what the VM does"
        );
        if on.is_empty() {
            let (message, span, _, _) = vm.expect_err("an empty separator is refused");
            let span = span.expect("the refusal carries a span");
            let text = &sources.get(span.file).text[span.start as usize..span.end as usize];
            assert_eq!(text, "s.split(on)", "`{message}` names the call");
        }
    }
}

/// **An intrinsic that allocates from compiled code collects, and keeps what the
/// compiled frame still holds.**
///
/// `String.trim` allocates the string it answers, so the call is a safepoint: the
/// work is published before it, the helper takes ADR 0040's three steps and the
/// allocation may collect with `trimsAndKeeps`' `kept` array and receiver live in
/// the compiled frame — and `callsTrimsAndKeeps`' own array live in the encoded
/// one below it. Both are read back after the loop, so a walk that missed either
/// sweeps an object that is then handed out again, and the answer is a wrong
/// number rather than a crash.
///
/// A session over a **small heap**, for
/// `an_allocation_from_compiled_code_collects_and_keeps_what_is_live`' reason: a
/// collection has to actually happen, and the loop runs until one has.
#[test]
fn a_collecting_intrinsic_call_from_compiled_code_keeps_what_is_live() {
    const SMALL_HEAP_WORDS: usize = 1 << 13;
    const N: i64 = 9;
    let text = "   a string with room around it, long enough to be several words   ";
    on_each_tier(&["trimsAndKeeps"], &["callsTrimsAndKeeps"]);

    let (sources, program) = checked();
    let lowered = Arc::new(
        cove_ir::lower(&program, &sources, &cove_sema::HostSchemas::new())
            .expect("the fixture lowers"),
    );
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(
        Arc::clone(&program),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    let native = cove_runtime::compile_native(&lowered).expect("this host compiles");

    let mut vm = Vm::with_heap_words(&runtime, &hosts, &lowered, SMALL_HEAP_WORDS);
    let (calls, crossings) = {
        let mut session = vm
            .native_session(
                MODULE,
                "callsTrimsAndKeeps",
                vec![Value::string(text), Value::int(N)],
            )
            .expect("the session opens");
        let words = session.arguments().to_vec();
        let expected = session
            .call(&cove_runtime::NothingCompiled, &words)
            .expect("the vm answers");
        let trimmed = text.trim().len() as u64;
        assert_eq!(
            expected,
            vec![trimmed * N as u64 * 1000 + 3 + text.len() as u64 + 2],
            "the fixture answers the trimmed lengths, two array lengths and a byte length"
        );

        let before = session.collections();
        let mut calls = 0;
        while session.collections() == before && calls < 20_000 {
            let answered = session
                .call(&native, &words)
                .expect("the native tier answers");
            assert_eq!(answered, expected, "call {calls} answered wrongly");
            calls += 1;
        }
        assert!(
            session.collections() > before,
            "no collection ran in {calls} call(s), so this case proved nothing"
        );
        (calls, session.tiers().vm_to_native)
    };
    assert!(
        crossings >= calls,
        "every call crossed into machine code: {crossings} of {calls}"
    );
}

/// **`String.byteLength()` in machine code, over a byte count and not a
/// character count.**
///
/// `std.string.byteLength` is `core.byteLength`, which is `Inst::Len` — so both
/// arms lower it with the emitter `Inst::Len` uses and this is the differential
/// that says the two agree. The
/// multi-byte string is the half of it a character count would pass: `"héllo"` is
/// five characters and six bytes, so an arm that answered `String.length`'s
/// question would be off by exactly one here and by nothing on an ASCII string.
///
/// The empty string is the other end — a header whose low half is nought, which is
/// also what a null reference's *word* is — and the two together are why the null
/// refusal is tested where it is: a `String` slot that holds zero is not reachable
/// from a checked Cove program, because every binding of one is initialised, so
/// the refusal is driven directly in `cove-native`'s own suite over a frame built
/// by hand. What this file can say is that every receiver a program *can* produce
/// answers what the VM answers.
#[test]
fn a_byte_length_is_a_header_read_in_compiled_code() {
    on_each_tier(&["measures"], &["callsMeasures"]);
    let rows: [(&str, i64); 4] = [
        // Five characters, six bytes: `é` is two.
        ("héllo", 6),
        ("hello", 5),
        ("", 0),
        // Four bytes in one character, so a length in code points would say one.
        ("😀", 4),
    ];
    for (text, bytes) in rows {
        let both = both("callsMeasures", vec![Value::string(text)]);
        assert_eq!(
            both.vm,
            Ok((bytes * 10).to_string()),
            "`{text}` is {bytes} byte(s) to the encoded tier"
        );
        assert_eq!(
            both.native, both.vm,
            "and compiled code reads the same header for `{text}`"
        );
        assert!(
            both.tiers.native_to_native_direct >= 1,
            "`measures` ran as machine code, so its own call was a direct one: {:?}",
            both.tiers
        );
    }
}

/// **A literal's address, loaded in machine code and followed on the other tier.**
///
/// The whole of `Inst::Str` is `ctx.literals[text]`, and `cove-native`'s own suite
/// already holds each arm to a table it wrote itself. What only this file can say
/// is that the table the *runtime* publishes is the one
/// `Machine::place_literals` built — a compiled `Inst::Str` reading a pointer that
/// was never published, or published from the wrong `Arc`, passes every test whose
/// context is built by hand.
///
/// Three claims, and they are three because none of them implies the next:
///
/// - the byte length is the encoded tier's, which says the word is an address of
///   the object the run placed rather than a number that looks like one;
/// - the `String` itself is what the caller renders, which says the reference
///   survived the crossing into the destination the caller named;
/// - a *different* argument answers the *other* literal, which says the
///   displacement picks an entry rather than always the table's first.
#[test]
fn a_literal_is_the_object_the_run_placed() {
    on_each_tier(&["picksALiteral"], &["callsPicksALiteral", "saysALiteral"]);
    for (n, text, bytes) in [(1i64, "alpha", 5i64), (0, "beta gamma", 10)] {
        let measured = both("callsPicksALiteral", vec![Value::int(n)]);
        assert_eq!(
            measured.vm,
            Ok(bytes.to_string()),
            "`{text}` is {bytes} byte(s) to the encoded tier"
        );
        assert_eq!(
            measured.native, measured.vm,
            "and compiled code loaded the address of the same object"
        );
        assert!(
            measured.tiers.vm_to_native >= 1,
            "the literal was loaded in machine code: {:?}",
            measured.tiers
        );

        let said = both("saysALiteral", vec![Value::int(n)]);
        assert_eq!(said.vm, Ok(text.to_string()));
        assert_eq!(
            said.native, said.vm,
            "and the same object came back across the boundary"
        );
    }
}

/// The same literal, on a **later stack segment**.
///
/// A literal's address is a heap address and the table is the run's rather than
/// the task's, so nothing about either depends on where this task's words begin —
/// and that is the claim rather than an assumption. `NativeCtx` holds
/// `stack_origin` two fields from `literals`, and an arm that had confused the two
/// would read a word out of the stack; the runtime's first task, whose origin is
/// nought, hides exactly that.
#[test]
fn a_literal_resolves_on_a_later_stack_segment() {
    let here = both("callsPicksALiteral", vec![Value::int(1)]);
    let there = both_on(Segment::Later, "callsPicksALiteral", vec![Value::int(1)]);
    assert!(there.origin > 0, "the run really is on a later segment");
    assert_eq!(there.vm, here.vm, "the encoded tier answers the same there");
    assert_eq!(
        there.native, there.vm,
        "and so does the native tier on a segment that does not begin at nought"
    );

    let said = both_on(Segment::Later, "saysALiteral", vec![Value::int(0)]);
    assert_eq!(said.vm, Ok("beta gamma".to_string()));
    assert_eq!(said.native, said.vm);
}

/// The same header read, on a **later stack segment**.
///
/// The receiver is a `Repr::Ref` word naming the heap, so nothing about it depends
/// on the segment — and that is the claim, not an assumption: `object_len` goes
/// through `heap_ptr`, which subtracts `HEAP_ORIGIN_WORDS` rather than
/// `stack_origin`, and an arm that had confused the two would read a stack word.
/// The runtime's first task hides that, which is what `Segment::Later` is for.
#[test]
fn a_byte_length_reads_the_heap_on_a_later_segment() {
    let here = both("callsMeasures", vec![Value::string("héllo")]);
    let there = both_on(
        Segment::Later,
        "callsMeasures",
        vec![Value::string("héllo")],
    );
    assert!(there.origin > 0, "the run was moved off the first segment");
    assert_eq!(here.vm, Ok("60".to_string()));
    assert_eq!(there.vm, here.vm, "the encoded tier is the same on either");
    assert_eq!(
        there.native, here.vm,
        "and so is compiled code on a segment at {}",
        there.origin
    );
    assert!(
        there.tiers.native_to_native_direct >= 1,
        "`measures` was the compiled frame that read it: {:?}",
        there.tiers
    );
}

/// **A builder allocated, appended to and finished in compiled code.**
///
/// [ADR 0052]'s four reached from a compiled frame, against the VM. What the
/// native tier does with each of them is hand it to the runtime — see
/// `cove_native::abi`'s `GrowableFn` — so the claim is not that the append got
/// faster; it is that the frame around it compiles and the string it builds is
/// still character for character the VM's.
///
/// The rows are chosen so that the store is **not** grown, grown once, and grown
/// repeatedly: the builder asks for four bytes, and growth allocates a larger
/// run, copies the live prefix and replaces the owner's store word. A tier that
/// lost the prefix answers a short string; one that kept a stale store word
/// answers the prefix twice.
#[test]
fn a_builder_is_built_and_finished_in_compiled_code() {
    on_each_tier(&["builds"], &["callsBuilds"]);
    let rows: [(&str, &str); 5] = [
        // Inside the initial capacity, so nothing grows.
        ("ab", "cd"),
        ("", ""),
        // Over it, so the store is replaced once.
        ("hello", " world"),
        // Far over it, so it is replaced several times.
        ("the quick brown fox jumps over the lazy dog, ", "and again"),
        // Multi-byte, so a growth that split a character would be visible.
        ("héllo 😀", " wörld"),
    ];
    for (a, b) in rows {
        let both = both("callsBuilds", vec![Value::string(a), Value::string(b)]);
        assert_eq!(
            both.vm,
            Ok(format!("{a}{b}")),
            "the encoded tier joins `{a}` and `{b}`"
        );
        assert_eq!(
            both.native, both.vm,
            "and so does a compiled frame, for `{a}` and `{b}`"
        );
        assert!(
            both.tiers.vm_to_native >= 1,
            "the build happened in machine code: {:?}",
            both.tiers
        );
    }
}

/// A finish of a run that is not valid UTF-8 raises the VM's own sentence.
///
/// `cove-native` names errors and never builds one, so this is the
/// `GrowableFn`-shaped version of the claim every raise in this file makes: the
/// message, the rule and the span are the runtime's whichever tier the frame was
/// on. Two failures and one success, because the two failures are raised by
/// *different* instructions — `growable-push` refuses a value that is not a byte
/// and `run-finish` refuses bytes that are not text — and a tier that reported
/// one instruction's span for the other's error would still print a sentence.
#[test]
fn a_finish_of_invalid_utf8_is_the_vm_s_sentence() {
    on_each_tier(&["buildsAByte"], &["callsBuildsAByte"]);
    for n in [65i64, 0x7f, 0xff, 0x80, 256, -1] {
        let both = both("callsBuildsAByte", vec![Value::int(n)]);
        assert_eq!(
            both.native, both.vm,
            "the two tiers answer the same thing for byte {n}"
        );
    }
    // Named rather than merely compared, so that a change of wording is a change
    // this file has to agree to.
    for (n, said) in [
        (
            0xffi64,
            Err("this string's bytes are not valid UTF-8".to_string()),
        ),
        (
            256,
            Err("`appendByte`'s value is `256`, and a byte is 0 to 255".to_string()),
        ),
        (65, Ok("A".to_string())),
    ] {
        let named = both("callsBuildsAByte", vec![Value::int(n)]);
        assert_eq!(named.vm, said, "the encoded tier's own words for {n}");
        assert_eq!(named.native, named.vm);
    }
}

/// **A builder that crosses a tier boundary between its `alloc` and its
/// `finish`.**
///
/// ADR 0052's stable owner, as a tier question. The `growable-alloc` and the
/// `run-finish` are a compiled frame's and the append in between is an encoded
/// one's, reached through a `var` — so the growth that happens on the VM has to be
/// visible to the compiled frame that made the builder. It is, because there is
/// nothing to be visible *of*: both frames name one owner, and only the store
/// under it was replaced.
///
/// The second string is long enough to force that growth. Without it the case
/// would pass for a tier that copied the handle, which is the mistake the ADR's
/// "a builder that reallocated itself would leave every `var` address behind it
/// stale" is about.
#[test]
fn a_builder_crosses_a_tier_boundary_between_alloc_and_finish() {
    on_each_tier(&["buildsAcross"], &["callsBuildsAcross", "addsTo"]);
    for (a, b) in [
        ("ab", "cd"),
        (
            "",
            "a string long enough to replace the store beneath the owner",
        ),
        ("héllo", " wörld 😀"),
    ] {
        let both = both(
            "callsBuildsAcross",
            vec![Value::string(a), Value::string(b)],
        );
        assert_eq!(both.vm, Ok(format!("{a}{b}")));
        assert_eq!(
            both.native, both.vm,
            "the encoded append is visible to the compiled finish, for `{a}` + `{b}`"
        );
        assert!(
            both.tiers.native_to_vm >= 1,
            "the builder really did cross into the VM and back: {:?}",
            both.tiers
        );
    }
}

/// The same four, on a **later stack segment**.
///
/// The helper is handed `base` as a word index and resolves its operands through
/// the frame the runtime is already holding, so nothing about it should depend on
/// where the task's words begin — and that is the claim rather than an
/// assumption. The first task's origin is nought, which is what hides a helper
/// that read a slot as though it were.
#[test]
fn a_builder_resolves_on_a_later_stack_segment() {
    let args = || vec![Value::string("héllo"), Value::string(" wörld 😀")];
    let here = both("callsBuilds", args());
    let there = both_on(Segment::Later, "callsBuilds", args());
    assert!(
        there.origin > 0,
        "a later segment does not begin at word nought"
    );
    assert_eq!(there.vm, here.vm, "the encoded tier answers the same there");
    assert_eq!(
        there.native, there.vm,
        "and so does the native tier on a segment that does not begin at nought"
    );

    let across = both_on(Segment::Later, "callsBuildsAcross", args());
    assert_eq!(across.vm, Ok("héllo wörld 😀".to_string()));
    assert_eq!(across.native, across.vm);

    let bad = both_on(Segment::Later, "callsBuildsAByte", vec![Value::int(0xff)]);
    assert_eq!(
        bad.vm,
        Err("this string's bytes are not valid UTF-8".to_string())
    );
    assert_eq!(bad.native, bad.vm);
}

/// **An allocation made from compiled code, with a collection forced inside it.**
///
/// The case the allocation helper exists for, and the one genuinely new thing
/// about it: before it, compiled code could not *cause* a collection. It reached
/// the collector only at a safepoint, where nothing was half-built, or through a
/// call, where the callee's frame was what was at risk. An allocation made from a
/// compiled frame collects with **that frame's own references live in it**, so
/// `cove_native::abi`'s "every live reference is already in the slot the frame's
/// static map names" is what stands between the array being allocated and the
/// string being swept.
///
/// Both frames hold one. `allocatesAndKeeps` reads `s` on either side of its
/// allocation, and `callsAllocatesAndKeeps` — which is *encoded* — holds an
/// `Array` of its own across the whole call and reads it back afterwards. So a
/// walk that missed either tier's roots fails here, and it fails as a wrong answer
/// rather than as a crash, because swept words are handed out again.
///
/// It is a [`cove_runtime::NativeSession`] over a **small heap** for
/// `a_cleared_slot_is_not_a_root_and_a_live_one_still_is`' reason: a collection has
/// to actually happen, and the loop runs until one has.
#[test]
fn an_allocation_from_compiled_code_collects_and_keeps_what_is_live() {
    // One heap chunk, which is the smallest a heap is.
    const SMALL_HEAP_WORDS: usize = 1 << 13;
    const N: i64 = 11;
    let text = "a string long enough that a byte can be read out of the middle of it.";
    on_each_tier(&["allocatesAndKeeps"], &["callsAllocatesAndKeeps"]);

    let (sources, program) = checked();
    let lowered = Arc::new(
        cove_ir::lower(&program, &sources, &cove_sema::HostSchemas::new())
            .expect("the fixture lowers"),
    );
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(
        Arc::clone(&program),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    let native = cove_runtime::compile_native(&lowered).expect("this host compiles");

    let mut vm = Vm::with_heap_words(&runtime, &hosts, &lowered, SMALL_HEAP_WORDS);
    let (calls, crossings) = {
        let mut session = vm
            .native_session(
                MODULE,
                "callsAllocatesAndKeeps",
                vec![Value::string(text), Value::int(N)],
            )
            .expect("the session opens");
        let words = session.arguments().to_vec();
        let expected = session
            .call(&cove_runtime::NothingCompiled, &words)
            .expect("the vm answers");
        // `s.byteAt(0)`, the array's four and the string's byte length, then the
        // caller's own array of three.
        let bytes = text.as_bytes();
        assert_eq!(
            expected,
            vec![u64::from(bytes[0]) + 4 + text.len() as u64 + 3],
            "the fixture answers a byte, two lengths and a byte length"
        );

        let before = session.collections();
        let mut calls = 0;
        while session.collections() == before && calls < 20_000 {
            let answered = session
                .call(&native, &words)
                .expect("the native tier answers");
            assert_eq!(answered, expected, "call {calls} answered wrongly");
            calls += 1;
        }
        assert!(
            session.collections() > before,
            "no collection ran in {calls} call(s), so this case proved nothing"
        );
        (calls, session.tiers().vm_to_native)
    };
    assert!(
        crossings >= calls,
        "every call crossed into machine code: {crossings} of {calls}"
    );
    // The arrays were reclaimed rather than merely allocated: far more words were
    // handed out than the heap ever held.
    assert!(
        vm.allocated_words() > SMALL_HEAP_WORDS as u64,
        "{} word(s) handed out over {calls} call(s) of a {SMALL_HEAP_WORDS}-word heap",
        vm.allocated_words()
    );
}

/// **A collection forced with a half-built run live in a compiled frame.**
///
/// The case the buffer helper's rooting argument is about, and the one thing
/// about it that is genuinely new. `growable-alloc` allocates *twice* and the store
/// is unreachable from any frame between the two — the runtime holds it with
/// `Machine::push_temp`, which is why the whole instruction is one helper and not
/// two [`cove_native::AllocFn`] calls with emitted code in between. After it, the
/// owner is in one `Repr::Ref` slot of a **compiled** frame and the bytes already
/// appended hang off it, and then an array literal in the same body allocates
/// again.
///
/// So a walk that missed the compiled frame's slot sweeps a run that has bytes in
/// it, and the finish answers a shorter string or a different one. The answer is
/// a length, so that is a wrong number rather than a crash — and swept words are
/// handed out again, so it is a wrong number that stays wrong.
///
/// It is a [`cove_runtime::NativeSession`] over a **small heap** for
/// `an_allocation_from_compiled_code_collects_and_keeps_what_is_live`' reason: a
/// collection has to actually happen, and the loop runs until one has.
#[test]
fn a_half_built_run_survives_a_collection_from_compiled_code() {
    // One heap chunk, which is the smallest a heap is.
    const SMALL_HEAP_WORDS: usize = 1 << 13;
    const N: i64 = 11;
    // Longer than the builder's capacity, so the store is replaced before the
    // array is allocated and the *replaced* run is what has to survive.
    let text = "a string long enough that appending it twice replaces the store.";
    on_each_tier(&["buildsWhileCollecting"], &["callsBuildsWhileCollecting"]);

    let (sources, program) = checked();
    let lowered = Arc::new(
        cove_ir::lower(&program, &sources, &cove_sema::HostSchemas::new())
            .expect("the fixture lowers"),
    );
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(
        Arc::clone(&program),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    let native = cove_runtime::compile_native(&lowered).expect("this host compiles");

    let mut vm = Vm::with_heap_words(&runtime, &hosts, &lowered, SMALL_HEAP_WORDS);
    let (calls, crossings) = {
        let mut session = vm
            .native_session(
                MODULE,
                "callsBuildsWhileCollecting",
                vec![Value::string(text), Value::int(N)],
            )
            .expect("the session opens");
        let words = session.arguments().to_vec();
        let expected = session
            .call(&cove_runtime::NothingCompiled, &words)
            .expect("the vm answers");
        // The text twice, and the array's four.
        assert_eq!(
            expected,
            vec![2 * text.len() as u64 + 4],
            "the fixture answers the finished length and the array's"
        );

        let before = session.collections();
        let mut calls = 0;
        while session.collections() == before && calls < 20_000 {
            let answered = session
                .call(&native, &words)
                .expect("the native tier answers");
            assert_eq!(answered, expected, "call {calls} answered wrongly");
            calls += 1;
        }
        assert!(
            session.collections() > before,
            "no collection ran in {calls} call(s), so this case proved nothing"
        );
        (calls, session.tiers().vm_to_native)
    };
    assert!(
        crossings >= calls,
        "every call crossed into machine code: {crossings} of {calls}"
    );
    // The runs were reclaimed rather than merely allocated: far more words were
    // handed out than the heap ever held.
    assert!(
        vm.allocated_words() > SMALL_HEAP_WORDS as u64,
        "{} word(s) handed out over {calls} call(s) of a {SMALL_HEAP_WORDS}-word heap",
        vm.allocated_words()
    );
}

/// **A byte push from compiled code: the emitted store, the growth path, and the
/// text that comes out.**
///
/// `pushesBytes` is compiled and appends `n` bytes to a builder with room for
/// two, so a run is mostly the emitted push into spare capacity, punctuated by
/// pushes that find the store full and are handed to the runtime, which grows it.
/// The rows are no push, the two that fit, the one that first grows, the push at
/// each end of a word, and enough to grow several times.
#[test]
fn a_byte_push_from_compiled_code_is_the_vm_s_text() {
    on_each_tier(&["pushesBytes"], &["callsPushesBytes"]);
    for n in [0i64, 1, 2, 3, 8, 9, 16, 17, 100] {
        let both = both("callsPushesBytes", vec![Value::int(n)]);
        let text: Vec<u8> = (0..n).map(|i| b'a' + (i % 26) as u8).collect();
        let folded = text.iter().fold(0i64, |held, byte| {
            (held * 31 + i64::from(*byte)) % 1_000_003
        });
        assert_eq!(
            both.vm,
            Ok(format!("{}", folded * 1_000_000 + n * 1000 + n / 8 * 4)),
            "n = {n}: the fold of the text, its length and the arrays"
        );
        assert_eq!(both.native, both.vm, "n = {n}: the compiled pushes' text");
        assert!(
            both.tiers.vm_to_native >= 1,
            "n = {n}: the pushes were machine code: {:?}",
            both.tiers
        );
    }
}

/// **Byte pushes with collections landing between them, in a compiled frame.**
///
/// [`a_half_built_run_survives_a_collection_from_compiled_code`]'s case with the
/// run built a byte at a time: the emitted push reads the store out of the owner
/// on every push rather than keeping it, so a collection that moved nothing and a
/// growth that replaced the store are both seen by the next one. The loop runs
/// until a collection has happened.
#[test]
fn byte_pushes_survive_collections_from_compiled_code() {
    const SMALL_HEAP_WORDS: usize = 1 << 13;
    const N: i64 = 200;
    on_each_tier(&["pushesBytes"], &["callsPushesBytes"]);

    let (sources, program) = checked();
    let lowered = Arc::new(
        cove_ir::lower(&program, &sources, &cove_sema::HostSchemas::new())
            .expect("the fixture lowers"),
    );
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(
        Arc::clone(&program),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    let native = cove_runtime::compile_native(&lowered).expect("this host compiles");

    let mut vm = Vm::with_heap_words(&runtime, &hosts, &lowered, SMALL_HEAP_WORDS);
    let (calls, crossings) = {
        let mut session = vm
            .native_session(MODULE, "callsPushesBytes", vec![Value::int(N)])
            .expect("the session opens");
        let words = session.arguments().to_vec();
        let expected = session
            .call(&cove_runtime::NothingCompiled, &words)
            .expect("the vm answers");

        let before = session.collections();
        let mut calls = 0;
        while session.collections() == before && calls < 20_000 {
            let answered = session
                .call(&native, &words)
                .expect("the native tier answers");
            assert_eq!(answered, expected, "call {calls} answered wrongly");
            calls += 1;
        }
        assert!(
            session.collections() > before,
            "no collection ran in {calls} call(s), so this case proved nothing"
        );
        (calls, session.tiers().vm_to_native)
    };
    assert!(
        crossings >= calls,
        "every call crossed into machine code: {crossings} of {calls}"
    );
}

/// **`Vector.push` from compiled code: the fast path, the growth path, and what
/// the owner sees afterwards.**
///
/// `pushesOnto` is compiled and pushes `n - 1` times onto a vector its caller made
/// with one element, so a run is mostly the emitted fast path and is punctuated by
/// the growth path — which is the cold one, where the store is replaced. The caller
/// then reads **every element back**, which is the assertion that matters: a growth
/// that copied the live prefix wrongly, or that left the header pointing at the old
/// store, answers a wrong sum while the length is still right.
///
/// The counter the compiled loop ended on goes out with the sum, so a push that
/// silently did nothing cannot pass on the elements alone either.
#[test]
fn a_push_from_compiled_code_grows_and_the_owner_sees_it() {
    on_each_tier(&["pushesOnto"], &["callsPushesOnto"]);
    // One push, a handful, and enough to double the store several times.
    for n in [1i64, 2, 5, 40] {
        let both = both("callsPushesOnto", vec![Value::int(n)]);
        // `Vector.of(7)` and then `1..n`, so the length is `max(n, 1)` and the sum
        // is `7 + (n - 1) * n / 2`.
        let length = n.max(1);
        let total = 7 + (n - 1) * n / 2;
        assert_eq!(
            both.vm,
            Ok(format!("{}", n * 1_000_000 + length * 1000 + total)),
            "n = {n}: the counter, the length and the sum of the elements"
        );
        assert_eq!(
            both.native, both.vm,
            "n = {n}: every element the compiled pushes wrote is where the VM put it"
        );
        assert!(
            both.tiers.vm_to_native >= 1,
            "n = {n}: the pushes were machine code: {:?}",
            both.tiers
        );
    }
}

/// The same pushes, on a **later stack segment**.
///
/// The vector and its store are heap objects, so nothing about a push depends on
/// the segment — and that is the claim rather than an assumption. Every heap
/// address the emitted push forms goes through `heap_ptr`, which subtracts
/// `HEAP_ORIGIN_WORDS`; an arm that had reached for `stack_origin` anywhere in it
/// would write into a frame, and on the first segment those two numbers are the
/// same. See [`Segment`].
#[test]
fn a_push_from_compiled_code_writes_the_heap_on_a_later_segment() {
    let here = both("callsPushesOnto", vec![Value::int(40)]);
    let there = both_on(Segment::Later, "callsPushesOnto", vec![Value::int(40)]);
    assert!(there.origin > 0, "the run was moved off the first segment");
    assert_eq!(here.vm, Ok("40040787".to_string()));
    assert_eq!(there.vm, here.vm, "the encoded tier is the same on either");
    assert_eq!(
        there.native, here.vm,
        "and so is compiled code on a segment at {}",
        there.origin
    );
    assert!(there.tiers.vm_to_native >= 1, "{:?}", there.tiers);
}

/// `Vector.set` from compiled code: in range, at the two ends, and outside the
/// vector on both sides.
///
/// The vector is `0, 10, .., (size - 1) * 10`, so the displaced element at
/// `index` is `index * 10` whenever `index` is in range, and the caller's own
/// read-back sum is what says the write landed at `index * stride` and not
/// merely somewhere: a lowering that forgot the multiply would agree with the
/// VM at `index == 1`, where the two coincide, and disagree everywhere else —
/// which is why `2` and `size - 1` are in the table and not only `0` and `1`.
#[test]
fn a_set_from_compiled_code_writes_in_range_and_answers_none_outside_it() {
    on_each_tier(&["setsAt"], &["callsSetsAt"]);
    const SIZE: i64 = 5;
    let before: i64 = (0..SIZE).map(|i| i * 10).sum();
    for index in [0i64, 2, SIZE - 1, SIZE, -1] {
        let both = both(
            "callsSetsAt",
            vec![Value::int(SIZE), Value::int(index), Value::int(999)],
        );
        let (answered, total) = if (0..SIZE).contains(&index) {
            (index * 10, before - index * 10 + 999)
        } else {
            (-1, before)
        };
        assert_eq!(
            both.vm,
            Ok(format!("{}", answered * 1_000_000 + SIZE * 1000 + total)),
            "index {index}: the answer, the length and the sum of the elements"
        );
        assert_eq!(
            both.native, both.vm,
            "index {index}: compiled `set` agrees with the VM"
        );
        assert!(
            both.tiers.vm_to_native >= 1,
            "index {index}: the set was machine code: {:?}",
            both.tiers
        );
    }
}

/// `Vector.set` on an empty vector answers `None`, the same as an
/// out-of-range index on a non-empty one — one rule about indices, checked
/// where a vector has none at all rather than merely too few.
#[test]
fn a_set_on_an_empty_vector_answers_none() {
    on_each_tier(&["setsAt"], &["callsSetsOnEmpty"]);
    for index in [0i64, -1] {
        let both = both("callsSetsOnEmpty", vec![Value::int(index), Value::int(999)]);
        assert_eq!(
            both.vm,
            Ok("-1000".to_string()),
            "index {index}: `None`, and the vector is still empty"
        );
        assert_eq!(both.native, both.vm, "index {index}");
        assert!(
            both.tiers.vm_to_native >= 1,
            "index {index}: {:?}",
            both.tiers
        );
    }
}

// **`Vector.set`'s two cold paths are not reachable from here.**
//
// A frozen store is exercised, and the two code generators are checked
// against each other on it, in `cove-native`'s own suite —
// `every_cold_path_of_a_set_goes_to_the_runtime` in
// `crates/cove-native/tests/suite/mod.rs`, run over hand-built IR. It cannot
// be exercised from checked Cove source in this file: `crates/cove-sema`'s
// `unique` pass proves every `freeze()` it accepts is the *only* handle to
// its storage and refuses every later read of it (`used_after_freeze`), and
// refuses the `freeze()` itself wherever that cannot be proved
// (`not_unique`) — see that pass's own "This is not a borrow checker": a
// program it accepts is never a program that reaches a store word of nought
// through a second handle, because there is no second handle a checked
// program can hold. `Vector.push`'s identical cold path is untested here for
// the same reason.
//
// The other cold path — a receiver whose object is not the layout the call
// site declared — is unreachable from checked Cove source the same way: it
// would need an unsound cast the type checker does not offer, and it is
// covered by the same `cove-native` suite instead.

/// `Vector.set` of a two-word element — `Point` — from compiled code: the
/// stride case, where the run this method copies is two words and not one.
#[test]
fn a_set_of_a_two_word_element_writes_the_whole_element() {
    on_each_tier(&["setsPointAt"], &["callsSetsPointAt"]);
    const SIZE: i64 = 4;
    let before: i64 = (0..SIZE).map(|i| i * 10 + (i * 10 + 1)).sum();
    for index in [1i64, SIZE - 1, SIZE] {
        let both = both(
            "callsSetsPointAt",
            vec![
                Value::int(SIZE),
                Value::int(index),
                Value::int(777),
                Value::int(888),
            ],
        );
        let (answered, total) = if (0..SIZE).contains(&index) {
            let old = index * 10 * 1000 + (index * 10 + 1);
            (old, before - (index * 20 + 1) + (777 + 888))
        } else {
            (-1, before)
        };
        assert_eq!(
            both.vm,
            Ok(format!("{}", answered * 1_000_000 + SIZE * 1000 + total)),
            "index {index}: the displaced `Point`, the length and the sum of the elements"
        );
        assert_eq!(
            both.native, both.vm,
            "index {index}: compiled `set` agrees with the VM on a two-word element"
        );
        assert!(
            both.tiers.vm_to_native >= 1,
            "index {index}: {:?}",
            both.tiers
        );
    }
}

/// **A struct captured by a closure, read back inside the closure's own
/// compiled body.**
///
/// `capturesAPoint` itself is refused — it contains a `FuncRef`, which nothing
/// in this crate lowers, the same as every closure-forming function in this
/// file — but the closure it builds is not, and `capturesAPoint` needs no
/// marker of its own for that reason: it is already the outermost frame's
/// refusal, `allocates`'s shape. What crosses at `f()` is a `Shape::Closure`
/// object's own captured payload, read by an `Inst::LoadField` `cove-native`'s
/// own suite never drives through a real, checked program.
#[test]
fn a_field_read_by_a_closures_own_body_agrees_with_the_vm() {
    for (x, y) in [(3i64, 4i64), (0, 0), (-5, 12)] {
        let both = both("capturesAPoint", vec![Value::int(x), Value::int(y)]);
        assert_eq!(both.vm, Ok((x * 1000 + y).to_string()), "x={x} y={y}");
        assert_eq!(
            both.native, both.vm,
            "x={x} y={y}: the closure's own compiled body reads its capture the \
             same way the VM does"
        );
        assert!(
            both.tiers.vm_to_native >= 1,
            "x={x} y={y}: the closure body ran natively: {:?}",
            both.tiers
        );
    }
}

/// **`Vector.freeze()` from compiled code: the store relabelled in place, and
/// read back through the array it became.**
///
/// `size` sweeps past `Vector.push`'s own doubling — the element floor, then
/// one past it, then a power of two — so the freeze this drives sometimes finds
/// spare capacity in the store and sometimes finds none, and `array.get`/
/// `array.length` read every element back through the layout `relabel` wrote
/// rather than through the `Vector` header that named it before.
#[test]
fn a_freeze_from_compiled_code_answers_the_same_elements() {
    on_each_tier(&["freezesInto"], &["callsFreezesInto"]);
    for size in [1i64, 2, 3, 4, 5, 8, 9] {
        let expected: i64 = (1..size).sum();
        let both = both("callsFreezesInto", vec![Value::int(size)]);
        assert_eq!(both.vm, Ok(expected.to_string()), "size {size}");
        assert_eq!(
            both.native, both.vm,
            "size {size}: compiled `freeze` agrees with the VM"
        );
        assert!(
            both.tiers.vm_to_native >= 1,
            "size {size}: {:?}",
            both.tiers
        );
    }
}

/// **A `String` key's order from compiled code agrees with the VM's.**
///
/// `std.set.contains<String>` and `std.map.get<String, Int>` search by
/// `Cmp(Str, Order)`, which both code generators hand to the leaf
/// `OrderStrFn` since #378's Q4.14 was answered. What is compared is the answer
/// every probe's membership and rank folded into, on the encoded tier and on the
/// native one, and that the searches did run natively.
#[test]
fn a_string_order_from_compiled_code_agrees_with_the_vm() {
    on_each_tier(&["ordersStrings"], &["callsOrdersStrings"]);
    let names = compiled_names();
    for search in [
        "std.set.seekSet<String>",
        "std.map.seekMap<String, Int>",
        "std.set.seekPlaced<String>",
        "std.map.seekPlaced<String, Int>",
    ] {
        assert!(
            names.iter().any(|name| name == search),
            "`{search}` steps by `Cmp(Str, Order)` and is compiled: {names:?}"
        );
    }
    for which in [0i64, 1, 5] {
        let both = both("callsOrdersStrings", vec![Value::int(which)]);
        assert!(both.vm.is_ok(), "which {which}: {:?}", both.vm);
        assert_eq!(
            both.native, both.vm,
            "which {which}: compiled string orders agree with the VM"
        );
        assert!(
            both.tiers.vm_to_native >= 1,
            "which {which}: {:?}",
            both.tiers
        );
    }
}

/// **An allocation from compiled code that exhausts the heap raises the VM's own
/// sentence.**
///
/// `Machine::allocate`'s one refusal — the bump did not fit, a collection freed
/// nothing, and the second attempt did not fit either. It reaches compiled code as
/// a zero from the helper and leaves as [`Raise::Called`](cove_runtime::NativeRaise),
/// which carries no message *because the runtime is already holding the whole
/// error*: this crate names errors and never builds one. So what is compared is the
/// sentence, and it has to be the same sentence.
///
/// Nothing the loop allocates is unreachable — every array goes into the vector —
/// so the collection cannot help and the refusal is reached rather than deferred.
///
/// A session, because this needs a **small heap** and a **native tier** at once and
/// no constructor offers both: `Vm::with_heap_words` installs no table and
/// `Vm::with_native` takes the default heap. A session takes the table per call,
/// which is exactly the pair.
///
/// # What this case cannot catch, and where that is caught instead
///
/// Emitted code that **did not test the helper's answer at all** passes here, and
/// the reason is worth knowing: the helper stashed the error before it answered
/// nought, and `native.rs`'s `raised` hands back a stashed error whatever
/// [`Raise`](cove_runtime::NativeRaise) compiled code went on to name. So a zero
/// stored into the destination, and the null refusal the next instruction makes of
/// it, produce *this very sentence*. The missing test is caught by
/// `cove-native`'s `an_allocation_the_runtime_refuses_leaves_as_called`, which
/// reads the outcome and the destination directly — and it matters, because the
/// next instruction is only guaranteed to refuse a null while every instruction
/// that follows an allocation happens to do so.
#[test]
fn an_allocation_that_exhausts_the_heap_raises_the_vm_s_sentence() {
    const SMALL_HEAP_WORDS: usize = 1 << 13;
    // Enough nine-word arrays to overrun a one-chunk heap several times over.
    const N: i64 = 4000;
    on_each_tier(&["fillsTheHeap"], &["callsFillsTheHeap"]);

    let (sources, program) = checked();
    let lowered = Arc::new(
        cove_ir::lower(&program, &sources, &cove_sema::HostSchemas::new())
            .expect("the fixture lowers"),
    );
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(
        Arc::clone(&program),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    let native = cove_runtime::compile_native(&lowered).expect("this host compiles");

    let mut vm = Vm::with_heap_words(&runtime, &hosts, &lowered, SMALL_HEAP_WORDS);
    let mut session = vm
        .native_session(MODULE, "callsFillsTheHeap", vec![Value::int(N)])
        .expect("the session opens");
    let words = session.arguments().to_vec();
    // The **span** goes out with the message, and it is the half that says *which*
    // allocation failed. Both tiers attach `Function::span_at(pc)`, so a native run
    // that had failed at the `push` and a VM run that failed at the literal would
    // agree about the sentence and disagree here.
    let said = |answer: Result<Vec<u64>, cove_runtime::RuntimeError>| {
        answer
            .map(|words| format!("{words:?}"))
            .map_err(|error| (error.message, error.span))
    };
    let expected = said(session.call(&cove_runtime::NothingCompiled, &words));
    let (message, span) = expected.clone().expect_err("the VM runs out of memory");
    assert_eq!(
        message, "this run has no memory left",
        "which is what makes this case a comparison"
    );
    // The array literal, and not the `push` beside it: it is the one allocation of
    // this loop that compiled code makes itself, and the fixture puts it on a line
    // of its own so that the two are told apart by more than a column.
    let at = span.expect("a runtime error carries a span");
    assert!(
        SOURCE[at.start as usize..].starts_with('['),
        "the allocation that failed is the array literal, and the source at the \
         span is `{}`",
        &SOURCE[at.start as usize..(at.end as usize).min(SOURCE.len())]
    );

    let answered = said(session.call(&native, &words));
    assert_eq!(
        answered, expected,
        "compiled code left with the sentence the runtime built, at the instruction \
         it was on, and not with one of its own"
    );
    assert!(
        session.tiers().vm_to_native >= 1,
        "and it was compiled code that asked: {:?}",
        session.tiers()
    );
}

/// **`Vector.toArray` from compiled code answers the elements the vector had.**
///
/// ADR 0058's word `run-copy`, as a Cove program reaches it. The sizes sweep
/// across the vector's growth so the store copied from sometimes has spare
/// capacity and sometimes none, and the element after the snapshot is pushed onto
/// the vector and must not appear in the array.
#[test]
fn a_snapshot_from_compiled_code_answers_the_same_elements() {
    on_each_tier(&["snapshots"], &["callsSnapshots"]);
    for n in [1i64, 2, 4, 5, 40] {
        let expected: i64 = (1..n).map(|at| at * 11).sum::<i64>() + 1;
        let both = both("callsSnapshots", vec![Value::int(n)]);
        assert_eq!(both.vm, Ok((expected * 1000 + n).to_string()), "n = {n}");
        assert_eq!(both.native, both.vm, "n = {n}: compiled `toArray` agrees");
        assert!(both.tiers.vm_to_native >= 1, "n = {n}: {:?}", both.tiers);
    }
}

/// **A snapshot of references held only by a compiled frame survives the
/// collections that frame's own allocations make.**
///
/// A [`cove_runtime::NativeSession`] over a small heap, for
/// `a_half_built_run_survives_a_collection_from_compiled_code`' reason, and it
/// keeps calling until several collections have run: the first one may land among
/// the pushes, before there is a snapshot to lose.
#[test]
fn a_snapshot_of_references_survives_a_collection_from_compiled_code() {
    const SMALL_HEAP_WORDS: usize = 1 << 13;
    const N: i64 = 11;
    on_each_tier(
        &["snapshotsWhileCollecting"],
        &["callsSnapshotsWhileCollecting"],
    );

    let (sources, program) = checked();
    let lowered = Arc::new(
        cove_ir::lower(&program, &sources, &cove_sema::HostSchemas::new())
            .expect("the fixture lowers"),
    );
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(
        Arc::clone(&program),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    let native = cove_runtime::compile_native(&lowered).expect("this host compiles");

    let mut vm = Vm::with_heap_words(&runtime, &hosts, &lowered, SMALL_HEAP_WORDS);
    let (calls, crossings) = {
        let mut session = vm
            .native_session(MODULE, "callsSnapshotsWhileCollecting", vec![Value::int(N)])
            .expect("the session opens");
        let words = session.arguments().to_vec();
        let expected = session
            .call(&cove_runtime::NothingCompiled, &words)
            .expect("the vm answers");
        // `1 + 2 + .. + N`, and the replacement vector's one element.
        assert_eq!(
            expected,
            vec![(N * (N + 1) / 2 + 1) as u64],
            "the fixture answers the second words and a length"
        );

        let before = session.collections();
        let mut calls = 0;
        while session.collections() < before + 8 && calls < 20_000 {
            let answered = session
                .call(&native, &words)
                .expect("the native tier answers");
            assert_eq!(answered, expected, "call {calls} answered wrongly");
            calls += 1;
        }
        assert!(
            session.collections() >= before + 8,
            "only {} collection(s) ran in {calls} call(s), so this case proved little",
            session.collections() - before
        );
        (calls, session.tiers().vm_to_native)
    };
    assert!(
        crossings >= calls,
        "every call crossed into machine code: {crossings} of {calls}"
    );
}

/// One run of `countsTheBoundary`, counted when `count` says so, answering the
/// report it took.
fn counted_run(
    lowered: &cove_ir::Program,
    runtime: &Runtime,
    hosts: &HostRegistry,
    native: Option<&cove_runtime::NativeProgram>,
    count: bool,
    n: i64,
) -> Option<cove_runtime::BoundaryReport> {
    let mut vm = match native {
        Some(native) => Vm::with_native(runtime, hosts, lowered, native),
        None => Vm::new(runtime, hosts, lowered),
    };
    if count {
        vm.count_boundary();
    }
    let answered = vm
        .invoke(
            MODULE,
            "countsTheBoundary",
            vec![Value::string("hello"), Value::int(n)],
        )
        .map(|value| value.to_string())
        .map_err(|error| error.message);
    assert_eq!(
        answered,
        Ok(format!("{}", n * 1000 + n)),
        "the loop's counter, then the `indexOf` that answered"
    );
    vm.boundary()
}

/// **The boundary report counts each quantity apart, and exactly.**
///
/// ADR 0058's Phase 1 asks for "emitted IR, mediated intrinsics, encoded VM
/// instructions, native-to-VM crossings and native-to-runtime calls" to be
/// reported separately. `countsTheBoundary` is refused and calls
/// `indexOf` `n` times on the encoded tier; `measuresAndPushes` is compiled and calls
/// `byteLength` and `push` `n` times each in machine code. So each lands in a
/// different place, and a report that lumped any two of them together would fail
/// one of the rows below:
///
/// - `String.indexOf`: `n` from the encoded tier, none from native code;
/// - `String.byteLength`: not an intrinsic at all. It is `std.string` over
///   ADR 0058's `core.byteLength`, a thin wrapper the lowering expands into
///   `measuresAndPushes` as an `Inst::Len` — so no site, no mediated call, and
///   no library call left for either tier to make;
/// - `Vector.push`: not an intrinsic either. It is `std.vector` over
///   `core.vectorPush`, expanded into `measuresAndPushes` as a word
///   `growable-push`, so it has no site and no mediated call on either tier —
///   and from native code only its growths reach the runtime, through the
///   `growable` helper. The vector starts as `Vector.of(7)`, one element in a
///   store of exactly one, and a growth doubles from a minimum of four, so forty
///   pushes grow the store at lengths 1, 4, 8, 16 and 32 — five helper calls.
///
/// The program is lowered from the one entry, as `cove run` lowers it, so the
/// static counts are this slice's own.
#[test]
fn the_boundary_report_counts_each_quantity_apart() {
    use cove_ir::Intrinsic;
    use cove_runtime::BoundaryReport;
    const N: i64 = 40;
    let (sources, program) = checked();
    let lowered = cove_ir::lower_entry(
        &program,
        &sources,
        &cove_sema::HostSchemas::new(),
        MODULE,
        "countsTheBoundary",
    )
    .expect("the fixture lowers");
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(
        Arc::clone(&program),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    on_each_tier(&["measuresAndPushes"], &["countsTheBoundary"]);
    let counting = cove_runtime::compile_native_counting(&lowered).expect("this host compiles");
    let production = cove_runtime::compile_native(&lowered).expect("this host compiles");

    // Off unless asked: a run that did not ask has no report, whichever table it
    // was given — the counting one included.
    for native in [None, Some(&counting), Some(&production)] {
        assert_eq!(
            counted_run(&lowered, &runtime, &hosts, native, false, N),
            None,
            "nothing was asked for, so nothing is reported"
        );
    }

    let on_vm = counted_run(&lowered, &runtime, &hosts, None, true, N).expect("asked for");
    let on_native =
        counted_run(&lowered, &runtime, &hosts, Some(&counting), true, N).expect("asked for");
    let uncounted =
        counted_run(&lowered, &runtime, &hosts, Some(&production), true, N).expect("asked for");

    // Emitted IR is a fact about the program, so every run says the same.
    assert_eq!(on_vm.emitted, on_native.emitted);
    assert_eq!(on_vm.emitted, uncounted.emitted);
    assert!(
        on_vm.emitted.instructions > on_vm.emitted.intrinsic_sites,
        "{:?}",
        on_vm.emitted
    );
    let row = |report: &BoundaryReport, intrinsic: Intrinsic| {
        report
            .intrinsic(intrinsic)
            .unwrap_or_else(|| panic!("the program names {intrinsic}"))
    };
    assert_eq!(row(&on_vm, Intrinsic::StringIndexOf).sites, 1);
    assert_eq!(
        on_vm.emitted.intrinsic_sites,
        on_vm.intrinsics.iter().map(|row| row.sites).sum::<u64>(),
        "every site is some intrinsic's"
    );

    // Mediated intrinsics, by tier.
    let n = N as u64;
    let calls = |report: &BoundaryReport, intrinsic: Intrinsic| {
        let held = row(report, intrinsic);
        (held.encoded, held.native)
    };
    assert_eq!(calls(&on_vm, Intrinsic::StringIndexOf), (n, 0));
    for report in [&on_native, &uncounted] {
        assert_eq!(calls(report, Intrinsic::StringIndexOf), (n, 0));
        // Sorted by dynamic calls, most first.
        assert!(report
            .intrinsics
            .windows(2)
            .all(|pair| pair[0].calls() >= pair[1].calls()));
    }

    // The library: `byteLength` was expanded, so nothing is left to call.
    assert_eq!(on_vm.emitted.library_call_sites, 0, "{:?}", on_vm.emitted);
    assert_eq!(on_vm.library_calls.encoded, 0);
    assert_eq!(
        on_vm.library_calls.native, None,
        "no tier, so no native count"
    );
    assert_eq!(on_native.library_calls.native, Some(0));
    assert_eq!(
        uncounted.library_calls.native, None,
        "production helpers count nothing"
    );

    // Encoded instructions: the native run dispatched fewer, because the loop of
    // `measuresAndPushes` was machine code.
    assert!(
        on_native.encoded_instructions < on_vm.encoded_instructions,
        "{} against {}",
        on_native.encoded_instructions,
        on_vm.encoded_instructions
    );
    assert_eq!(
        on_native.encoded_instructions,
        uncounted.encoded_instructions
    );

    // Crossings: none without a tier, and the tier's own counts with one.
    assert_eq!(on_vm.tiers, None);
    assert_eq!(on_vm.helpers, None);
    let tiers = on_native.tiers.expect("a native tier was installed");
    // Two: the marker's `fn(v) { v }`, which compiles, and `measuresAndPushes`.
    assert_eq!(tiers.vm_to_native, 2, "{tiers:?}");
    assert_eq!(uncounted.tiers, Some(tiers));

    // Native-to-runtime calls: counted with the counting table, and said to be
    // uncounted with the production one rather than printed as zeroes.
    assert_eq!(uncounted.helpers, None);
    let helpers = on_native.helpers.expect("the counting helpers were bound");
    assert_eq!(
        helpers.growable, 5,
        "one `growable` helper call per growth, and none for a push with room: {helpers:?}"
    );
    assert_eq!(
        helpers.intrinsic, 0,
        "no intrinsic was mediated: {helpers:?}"
    );
    // Every call compiled code made went out through `open` or `call` — an `open`
    // whose callee has no machine code runs the mediated call itself, and is
    // still one `open` — and only a direct one comes back through `close`.
    assert_eq!(
        helpers.open + helpers.call,
        tiers.native_to_native_direct + tiers.native_to_native_mediated + tiers.native_to_vm,
        "{helpers:?}"
    );
    assert_eq!(helpers.close, tiers.native_to_native_direct, "{helpers:?}");

    let printed = on_native.to_string();
    assert!(printed.contains("boundary: native -> runtime helper calls"));
    assert!(uncounted.to_string().contains("were not counted"));
}
