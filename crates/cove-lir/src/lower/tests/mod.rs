//! What a construct lowers to, asserted as the whole listing.
//!
//! Every case here pins the whole of a function rather than a line of it, so
//! a change that moves an instruction fails a test rather than leaving one
//! passing for a reason nobody meant. The listing is
//! [`crate::print::function`], which exists for exactly this: one fact per
//! line, and no alignment that shifts when an unrelated line grows.
//!
//! Every listing goes through [`crate::verify`] on the way, because a
//! lowering that emitted a well-read but ill-formed program would otherwise
//! be caught only when something ran it.

mod calls;
mod control;
mod gaps;
mod slots;
mod values;

use std::collections::BTreeMap;
use std::path::PathBuf;

use cove_diag::SourceMap;
use cove_sema::config::Config;
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program as Checked;

use super::lower;

/// Checks one module of source the way `cove run` checks a package: parse,
/// resolve, and type-check.
///
/// Both halves, because the lowering reads what the second settled: a
/// program that was only resolved carries no facts, and every listing taken
/// from one would say nothing about the rule that chose the instruction.
///
/// The module is called `m`, so a listing reads `fn m.something`.
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

/// The disassembly of one lowered function.
fn listing(source: &str, name: &str) -> String {
    let program = lower(&checked(source)).expect("the program lowers");
    let id = program
        .function_named("m", name)
        .unwrap_or_else(|| panic!("`{name}` was lowered"));
    crate::print::function(&program, id)
}

/// What stopped a lowering, in the words it reported.
fn refused(source: &str) -> Vec<String> {
    match lower(&checked(source)) {
        Ok(_) => panic!("the program lowered, and this case is about what stops one"),
        Err(items) => items.into_iter().map(|item| item.message).collect(),
    }
}
