//! What one construct lowers to, asserted as the whole listing.
//!
//! Every listing test asserts the whole of a function rather than a line of
//! it, so a change that moves an instruction is a test that fails rather
//! than a test that still passes for a reason nobody meant. The helpers here
//! are what the cases in the sibling modules are written in terms of:
//! [`listing`] renders one lowered function with the program validated
//! first, [`specialisation`] tells apart the functions one name is lowered
//! to, and [`refused`] is what stopped a lowering, in the words it
//! reported.

mod benches;
mod blocks;
mod constructs;
mod dynamic;
mod entry;
mod patterns;
mod places;
mod slots;
mod unsupported;

use super::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cove_diag::SourceMap;
use cove_sema::config::Config;
use cove_sema::package::{Module, Package, Unit};

use cove_diag::Span;
use cove_sema::resolve::Program as Checked;
use cove_syntax::ast::{Arg, Expr, ExprKind};

use crate::{Const, ConstId, FunctionId, Inst, Program, SlotKind};

use super::{arguments_in_order, stack_shape, Args};

/// Checks one module of source the way `cove run` checks a package:
/// parse, resolve, and type-check.
///
/// Both halves, because the second is what settles a type and the
/// lowering reads those: a program that was only resolved carries no
/// facts, so every listing taken from one would show the untyped
/// instruction and would say nothing about the rule that picks between
/// them.
///
/// The module is called `m`, so a listing reads `fn m.something` and a
/// test asserts on the whole of it.
fn checked(source: &str) -> Checked {
    let mut sources = SourceMap::new();
    let file = sources.add("m/main.cove", source.to_string());
    let ast = match cove_syntax::parse_file(&sources, file) {
        Ok(ast) => ast,
        Err(items) => panic!("the source parses:\n{}", rendered(&sources, &items)),
    };
    let package = Package {
        root: PathBuf::from("."),
        config: Config::default(),
        modules: BTreeMap::from([(
            "m".to_string(),
            Module {
                name: "m".to_string(),
                dir: PathBuf::from("m"),
                units: vec![Unit {
                    file,
                    path: PathBuf::from("m/main.cove"),
                    ast,
                }],
            },
        )]),
    };
    match cove_sema::Compiler::new().compile(&package) {
        Ok(program) => program,
        Err(items) => panic!("the source checks:\n{}", rendered(&sources, &items)),
    }
}

fn rendered(sources: &SourceMap, items: &[cove_diag::Diagnostic]) -> String {
    items
        .iter()
        .map(|item| cove_diag::render(sources, item))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The rendered instructions of one lowered function, with the whole
/// program validated first.
///
/// Every listing test asserts the whole listing rather than a line of
/// it, so a change that moves an instruction is a test that fails rather
/// than a test that still passes for a reason nobody meant.
fn listing(source: &str, name: &str) -> String {
    let program = lower(&checked(source)).expect("the program lowers");
    validate(&program).expect("the lowering holds the VM's invariants");
    let id = program
        .function_named("m", name)
        .unwrap_or_else(|| panic!("`{name}` was lowered"));
    crate::render(&program, id)
}

/// The listing of the specialisation of `name` that takes `arity`
/// arguments.
///
/// A call that leaves a parameter to its default reaches a function of
/// its own, so a name is no longer one function and [`listing`] — which
/// asks for the first one of that name — would always answer the whole
/// one. The arity is what tells the specialisations apart, because each
/// takes exactly what its call site supplies.
fn specialisation(source: &str, name: &str, arity: u32) -> String {
    let program = lower(&checked(source)).expect("the program lowers");
    validate(&program).expect("the lowering holds the VM's invariants");
    let id = program
        .functions
        .iter()
        .position(|function| &*function.name == name && function.arity == arity)
        .map(|index| FunctionId(index as u32))
        .unwrap_or_else(|| panic!("`{name}` was lowered taking {arity} argument(s)"));
    crate::render(&program, id)
}

/// What stopped the lowering, in the words it reported.
fn refused(source: &str) -> String {
    match lower(&checked(source)) {
        Ok(_) => panic!("the program lowered, and was expected not to"),
        Err(why) => why.what,
    }
}

/// The whole `examples/` package, checked.
///
/// The evidence that a package is not a program: it holds eleven
/// `[run.<name>]` entries, and `callbacks/` holds a closure the lowering
/// refuses. Nothing about `hello` changes because of that.
fn examples() -> Checked {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut sources = SourceMap::new();
    let package = match cove_sema::package::load(&root, &mut sources) {
        Ok(package) => package,
        Err(items) => panic!(
            "the examples package loads:\n{}",
            rendered(&sources, &items)
        ),
    };
    match cove_sema::Compiler::new().compile(&package) {
        Ok(program) => program,
        Err(items) => panic!(
            "the examples package checks:\n{}",
            rendered(&sources, &items)
        ),
    }
}

/// The `benches/` package with only the module `name` kept.
///
/// [`lower`] is all-or-nothing over a package, so keeping one module at a
/// time is what lets a test say which entry lowers rather than only that
/// some entry did not.
fn bench(name: &str) -> Checked {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benches");
    let mut sources = SourceMap::new();
    let mut package = match cove_sema::package::load(&root, &mut sources) {
        Ok(package) => package,
        Err(items) => panic!("the benches package loads:\n{}", rendered(&sources, &items)),
    };
    let module = package
        .modules
        .remove(name)
        .unwrap_or_else(|| panic!("`benches/{name}` is a module of the package"));
    package.modules = BTreeMap::from([(name.to_string(), module)]);
    match cove_sema::Compiler::new().compile(&package) {
        Ok(program) => program,
        Err(items) => panic!(
            "the benches package checks:\n{}",
            rendered(&sources, &items)
        ),
    }
}
