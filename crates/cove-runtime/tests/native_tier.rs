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
/// `String.byteLength` is outside anything the template compiler lowers, so this
/// function runs on the encoded tier however the table is built — which is the
/// point of it.
export fn callsAdds(a: Int, b: Int) -> Int {
  let note = \"x\"
  adds(a, b) + note.byteLength() - 1
}

/// Division, so that a raise crosses the boundary.
export fn divides(a: Int, b: Int) -> Int {
  held(a) / b
}

export fn callsDivides(a: Int, b: Int) -> Int {
  let note = \"x\"
  divides(a, b) + note.byteLength() - 1
}

/// A deep recursion whose outer destinations are pending while the stack's `Vec`
/// reallocates.
export fn callsCounts(n: Int) -> Int {
  let note = \"x\"
  counts(n) + note.byteLength() - 1
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
  let note = \"x\"
  var total = a
  let seen = bumps(var total, 5)
  total * 1000 + seen + note.byteLength() - 1
}

/// Writes *one word* of a two-word value location, which is `Inst::AddrOfPart`.
export fn movesY(var p: Point, to: Int) -> Int {
  p.y = to + counts(0)
  p.y
}

/// A refused caller lending two words of its own frame, one of which is written.
export fn callsMovesY(a: Int) -> Int {
  let note = \"x\"
  var p = Point(x: a, y: 0)
  let seen = movesY(var p, 9)
  p.x * 1000 + p.y * 10 + seen + note.byteLength() - 1
}

/// Refused — the string is why — and it writes through the `var` it was lent.
export fn shows(var total: Int) -> Int {
  let note = \"x\"
  total = total + note.byteLength()
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
  let note = \"x\"
  lendsToTheVm(a) + note.byteLength() - 1
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
  let note = \"x\"
  total = total + note.byteLength() - 1
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
export fn allocates(s: String, n: Int) -> Int {
  match s.sliceBytes(held(0), n) {
    Ok(cut) => cut.byteLength()
    Err(_) => 0
  }
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
}

/// Runs `module.name` once on the encoded VM and once on the native tier.
///
/// The table is built **once, before either run**, and the `NativeProgram` outlives
/// the `Vm` it is given to, because it owns the pages the entries point into. It is
/// also never rebuilt between the two runs: finalizing a mapping twice is the thing
/// ADR 0055's W^X rule forbids, and one table for a whole process is what the
/// design is.
fn both(name: &str, args: Vec<Value>) -> Both {
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
    let vm = {
        let mut vm = Vm::new(&runtime, &hosts, &lowered);
        said(vm.invoke(MODULE, name, args.clone()))
    };
    let mut with = Vm::with_native(&runtime, &hosts, &lowered, &native);
    let answered = said(with.invoke(MODULE, name, args));
    Both {
        vm,
        native: answered,
        tiers: with.tiers(),
        compiled: native.compiled(),
        reachable: native.reachable(),
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
