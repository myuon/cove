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
/// `let nothing = ()` is what refuses it, and it is the marker every refused
/// caller in this file uses: `Inst::Unit` is outside the adoption gate's list and
/// is deliberately kept outside it — see `cove-native`'s
/// `anything_outside_the_slice_refuses_the_whole_function`, whose first row is
/// this very instruction — so this function runs on the encoded tier however the
/// table is built, which is the point of it.
///
/// **It used to be a string literal, and it stopped being one when `Inst::Str`
/// was lowered.** That is the hazard a marker like this has: the moment the
/// instruction it relies on joins the subset, every case in this file compares the
/// native tier with itself and passes whatever was emitted. What stands between
/// this file and that is not the marker, it is the two assertions every case makes
/// — `compiled < reachable`, and a tier counter that moved.
export fn callsAdds(a: Int, b: Int) -> Int {
  let nothing = ()
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
  let nothing = ()
  negates(a)
}

/// Division, so that a raise crosses the boundary.
export fn divides(a: Int, b: Int) -> Int {
  held(a) / b
}

export fn callsDivides(a: Int, b: Int) -> Int {
  let nothing = ()
  divides(a, b)
}

/// A deep recursion whose outer destinations are pending while the stack's `Vec`
/// reallocates.
export fn callsCounts(n: Int) -> Int {
  let nothing = ()
  counts(n)
}

/// Two words inline, so that a place can name the second of them.
export struct Point {
  x: Int
  y: Int
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
  let nothing = ()
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
  let nothing = ()
  var p = Point(x: a, y: 0)
  let seen = movesY(var p, 9)
  p.x * 1000 + p.y * 10 + seen
}

/// Refused — the unit marker is why — and it writes through the `var` it was lent.
export fn shows(var total: Int) -> Int {
  let nothing = ()
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
  let nothing = ()
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
  let nothing = ()
  descends(n, var total)
}

/// The frame that owns the word every step of the chain wrote through.
export fn threads(n: Int) -> Int {
  var total = 0
  let seen = descends(n, var total)
  total * 1000 + seen
}

/// The allocation a collection is forced with. `sliceBytes` is outside anything
/// the template compiler lowers, so this runs on the encoded tier in every case.
/// It needs no marker of its own for that reason.
export fn allocates(s: String, n: Int) -> Int {
  match s.sliceBytes(held(0), n) {
    Ok(cut) => cut.byteLength()
    Err(_) => 0
  }
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
/// `vm::builtins::text::byte_length` is a null refusal and one header read, which
/// is what `Inst::Len` already was, so this is the same emitter reached through a
/// `call-builtin` — see `cove_native`'s `Method::ByteLength`. `counts(0)` is here
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
  let nothing = ()
  picksALiteral(n).byteLength()
}

/// The same literal, carried back across the boundary as the `String` itself.
///
/// This is the half a length cannot be: the object compiled code stored is what
/// the *caller* renders, on the other tier, so a reference that was right in the
/// callee's frame and wrong in the caller's destination is a string that vanished
/// at a tier boundary.
export fn saysALiteral(n: Int) -> String {
  let nothing = ()
  picksALiteral(n)
}

/// A refused caller, so the measurement is reached across the boundary.
export fn callsMeasures(s: String) -> Int {
  let nothing = ()
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
  let nothing = ()
  let also = [n, n, n]
  allocatesAndKeeps(s, n) + also.length()
}

/// `Vector.push` in a compiled frame: the fast path and the growth one.
///
/// The vector is a **parameter** rather than a local, and that is not a style
/// choice: `Vector.of` lowers to an `Inst::StoreField`, which nothing lowers, so a
/// function that made its own vector would be refused and the pushes would run on
/// the VM. Made by the caller, it is the caller that is refused — which is the
/// shape every case in this file needs anyway.
///
/// It answers the *counter* and not `v.length()` for the same reason: a `Vector`'s
/// length is payload word nought of its header, so `length()` is an
/// `Inst::LoadField` rather than an `Inst::Len` and nothing lowers one yet. The
/// caller reads the length instead, which is the stronger place to read it from.
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
  let nothing = ()
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
  let nothing = ()
  var v: Vector<Array<Int>> = Vector.of([0])
  fillsTheHeap(v, n)
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
  let nothing = ()
  keepsWhatItStillNeeds(a, b, n)
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
/// a reader can act on: `CallBuiltin` is every builtin the language has and the
/// `Alloc` opcodes are every layout a program declares. `Refused::blocked` is the
/// second key, and what is asserted here is the join — that it is present for
/// exactly those two opcodes, absent for every other, and names the thing the
/// source actually wrote.
///
/// # The allocation table is expected to be *empty*, and that is the assertion
///
/// `Inst::Alloc` is lowered, so no function can be refused at one and the
/// allocation half of the census has no rows. Asserting that rather than deleting
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
            (Some("CallBuiltin"), Some(Blocked::Builtin(named))) => {
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
    assert!(builtins > 0, "the builtin table has rows");
    assert_eq!(
        allocations, 0,
        "`Inst::Alloc` is lowered, so nothing is refused at one — see this case's \
         own note before changing this number"
    );

    // And the one the fixture's own source writes, by name: `allocates` calls
    // `s.sliceBytes(..)`, which nothing lowers.
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
    assert_eq!(
        named("allocates"),
        Some(Blocked::Builtin("String.sliceBytes".to_string()))
    );
    // `heapsThrough` constructs a `Shared(a)`, whose allocation now lowers — so it
    // is refused for the `store-field` that fills the object in, and an opcode that
    // names one operation already carries no second key.
    assert_eq!(named("heapsThrough"), None);
}

/// **`String.byteLength()` in machine code, over a byte count and not a
/// character count.**
///
/// `vm::builtins::text::byte_length` is `receiver_addr` and `machine.object_len`,
/// which is what `Inst::Len` already is — so both arms lower it with the emitter
/// `Inst::Len` uses and this is the differential that says the two agree. The
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
        // `s.byteAt(0)` and the array's four and the string's byte length, then the
        // caller's own array of three, its `"x"`, and the `- 1`.
        let bytes = text.as_bytes();
        assert_eq!(
            expected,
            vec![u64::from(bytes[0]) + 4 + text.len() as u64 + 3 + 1 - 1],
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
