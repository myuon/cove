//! [ADR 0068](../../../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
//! Phase 1: the `core.dynamic*` intrinsics, each one instruction.
//!
//! They were written before any standard-library body called them —
//! `std.dynamic` arrived with Phase 2 — and each needs a body shaped to the
//! one question it asks, so these cases add a unit of their own to
//! `std.stringbuilder`, a module the
//! privilege of `core.` is decided by the *name* of. A unit is a file of a
//! module, so the module keeps every declaration it ships with and gains the
//! probe beside them.

use std::collections::BTreeMap;
use std::path::PathBuf;

use cove_diag::SourceMap;
use cove_schema::HostSchemas;
use cove_sema::config::Config;
use cove_sema::package::{Module, Package, Unit};

use super::{lower, rendered};
use crate::Program;

/// The module a probe is added to: one the standard library ships, so that
/// `core.` resolves in it, and a leaf, so that nothing it declares is shadowed
/// by the probe's names.
const HOST: &str = "std.stringbuilder";

/// The package `m` and the standard library make, with `probe` added to
/// [`HOST`] as a unit of its own, checked and lowered.
fn lowered(probe: &str, program: &str) -> Result<Program, Vec<String>> {
    let mut sources = SourceMap::new();
    let mut held = BTreeMap::new();
    let path = PathBuf::from("m/main.cove");
    let file = sources.add(path.clone(), program.to_string());
    let ast = cove_syntax::parse_file(&sources, file).expect("the program parses");
    held.insert(
        "m".to_string(),
        Module {
            name: "m".to_string(),
            dir: PathBuf::from("m"),
            units: vec![Unit { file, path, ast }],
        },
    );
    for (name, module) in cove_sema::stdlib::attach(&mut sources).expect("stdlib parses") {
        held.insert(name, module);
    }
    let path = PathBuf::from("std/probe.cove");
    let file = sources.add_library(path.clone(), probe.to_string());
    let ast = cove_syntax::parse_file(&sources, file).expect("the probe parses");
    held.get_mut(HOST)
        .expect("the host module ships")
        .units
        .push(Unit { file, path, ast });
    let package = Package {
        root: PathBuf::from("."),
        config: Config::default(),
        modules: held,
    };
    let checked = match cove_sema::Compiler::new().compile(&package) {
        Ok(checked) => checked,
        Err(items) => panic!("the probe checks:\n{}", rendered(&sources, &items)),
    };
    lower(&checked, &sources, &HostSchemas::new())
        .map_err(|items| items.into_iter().map(|item| item.message).collect())
}

/// The listing of `HOST.name`.
fn listing(program: &Program, name: &str) -> String {
    let id = program
        .functions
        .iter()
        .position(|f| &*f.module == HOST && &*f.name == name)
        .map(|at| crate::FunctionId(at as u32))
        .unwrap_or_else(|| panic!("`{HOST}.{name}` was lowered"));
    crate::print::function(program, id)
}

/// A program that never mentions reflection: it is here so that the probe's
/// module has a caller to be lowered for, and so that `main` exists.
const MAIN: &str = "/// Entry point.\nexport fn main() -> Int {\n  0\n}\n";

/// Each of the eleven intrinsics is its one instruction, written into the
/// location the surrounding form asked for: a view is a whole
/// `DynamicView` location of three words, and a scalar the word its
/// instruction answers. There is no call, no intrinsic call and no copy of a
/// view between them.
#[test]
fn every_dynamic_intrinsic_is_its_one_instruction() {
    let program = lowered(
        "trait Probed {\n  fn probed(self) -> Int\n}\n\n\
         fn probe(value: dyn Probed) -> Int {\n  \
           let view = core.dynamicOpen(value)\n  \
           let child = core.dynamicChild(view, 0)\n  \
           let same = core.dynamicSameType(view, child)\n  \
           let b = core.dynamicBool(child)\n  \
           let f = core.dynamicFloat(child)\n  \
           let d = core.dynamicDuration(child)\n  \
           let s = core.dynamicString(child)\n  \
           core.dynamicKind(view) + core.dynamicCase(view) + core.dynamicChildCount(view) + core.dynamicInt(child)\n\
         }\n",
        MAIN,
    )
    .expect("the probe lowers");
    let listed = listing(&program, "probe");
    let rows: Vec<&str> = listed
        .lines()
        .filter_map(|line| line.trim_start().split_once("  ").map(|(_, inst)| inst))
        .filter(|inst| inst.starts_with("dyn."))
        .collect();
    assert_eq!(
        rows,
        [
            "dyn.open s2..s4:DynamicView s0:ref",
            "dyn.child s6..s8:DynamicView s2..s4:DynamicView s5:int",
            "dyn.same-type s9:bool s2..s4:DynamicView s6..s8:DynamicView",
            "dyn.read s10:bool s6..s8:DynamicView",
            "dyn.read s11:float s6..s8:DynamicView",
            "dyn.read s12:duration s6..s8:DynamicView",
            "dyn.read s13:ref s6..s8:DynamicView",
            "dyn.kind s5:int s2..s4:DynamicView",
            "dyn.case s14:int s2..s4:DynamicView",
            "dyn.count s5:int s2..s4:DynamicView",
            "dyn.read s15:int s6..s8:DynamicView",
        ],
        "{listed}"
    );
    assert!(!listed.contains("intrinsic-call"), "{listed}");
    assert!(!listed.contains(" call "), "{listed}");
}

/// A view is a value of three words and nothing else, so a `Vector` of views
/// is an ordinary vector of an inline struct: ADR 0068's walks are iterative
/// over an explicit work stack (issue #480), and that stack is this.
#[test]
fn a_vector_of_views_is_a_vector_of_three_word_elements() {
    let program = lowered(
        "trait Probed {\n  fn probed(self) -> Int\n}\n\n\
         fn probe(value: dyn Probed) -> Int {\n  \
           var pending: Vector<DynamicView> = Vector.of(core.dynamicOpen(value))\n  \
           var seen = 0\n  \
           while pending.length() > 0 {\n    \
             let next = pending.pop()\n    \
             seen = seen + 1\n  \
           }\n  \
           seen\n\
         }\n",
        MAIN,
    )
    .expect("the probe lowers");
    let view = program.view_layout;
    assert_eq!(&*program.layout(view).name, "DynamicView");
    let vector = program
        .layouts
        .iter()
        .find(|layout| layout.shape == crate::Shape::Vector { elem: view });
    assert!(
        vector.is_some(),
        "a `Vector<DynamicView>` layout was declared"
    );
}

/// ADR 0068's Decision 5: a value whose layout is known is never routed
/// through a view. `core.dynamicOpen` of one is a gap, not an `Inst::Box` and
/// a reflection — the lowering has the synthesized walk for it, and a view of
/// it would be the regression the ADR's gate names.
#[test]
fn a_known_layout_is_not_opened_as_a_view() {
    let refused = lowered(
        "fn probe(value: Int) -> Int {\n  core.dynamicKind(core.dynamicOpen(value))\n}\n",
        MAIN,
    )
    .expect_err("a known layout is refused");
    assert_eq!(
        refused,
        [
            "not yet lowered: `core.dynamicOpen` of a `Int`, whose layout is known — ADR 0068 \
          reflects only on an erased value"
        ]
    );
}

/// The view's layout is seeded: every program has it, at the index the
/// lowering fixes, whether or not anything reflects — which is what lets the
/// verifier and the machine find it from `Program::view_layout` without being
/// told at each instruction.
#[test]
fn every_program_declares_the_view_layout() {
    let program = lowered("", MAIN).expect("the program lowers");
    let view = program.layout(program.view_layout);
    assert_eq!(&*view.name, "DynamicView");
    assert_eq!(view.words, crate::dynamic::VIEW_WORDS);
    assert!(view.is_opaque());
}
