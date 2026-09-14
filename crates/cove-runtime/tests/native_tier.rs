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
