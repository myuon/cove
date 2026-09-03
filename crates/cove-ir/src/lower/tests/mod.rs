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

mod assertions;
mod calls;
mod cells;
mod closures;
mod collections;
mod control;
mod dispatch;
mod enums;
mod erasure;
mod gaps;
mod generics;
mod hosts;
mod layouts;
mod locals;
mod methods;
mod patterns;
mod ranges;
mod slots;
mod snapshots;
mod strings;
mod structs;
mod tasks;
mod values;
mod walks;

use std::collections::BTreeMap;
use std::path::PathBuf;

use cove_diag::SourceMap;
use cove_schema::HostSchemas;
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
fn checked(source: &str) -> (SourceMap, Checked) {
    checked_with(source, &HostSchemas::new())
}

/// The same, against a named set of host modules rather than the shipped
/// ones alone.
///
/// It is one function with the set as a parameter because the lowering has
/// to read the *same* set the checker did: a case that checked against an
/// embedder's schema and lowered against the shipped tables would be
/// measuring the drift rather than the lowering.
fn checked_with(source: &str, schemas: &HostSchemas) -> (SourceMap, Checked) {
    checked_package(&[("m", source)], schemas)
}

/// The same, for a package of more than one module.
///
/// Two modules is the smallest package that can ask the one question a single
/// module cannot: a name is written bare in the module that declares it and
/// qualified everywhere else, so what a fact recorded *there* means is a
/// question only a reader somewhere else can get wrong.
fn checked_package(modules: &[(&str, &str)], schemas: &HostSchemas) -> (SourceMap, Checked) {
    let mut sources = SourceMap::new();
    let mut held = BTreeMap::new();
    for (name, source) in modules {
        let path = PathBuf::from(format!("{name}/main.cove"));
        let file = sources.add(path.clone(), source.to_string());
        let ast = match cove_syntax::parse_file(&sources, file) {
            Ok(ast) => ast,
            Err(items) => panic!("the source parses:\n{}", rendered(&sources, &items)),
        };
        held.insert(
            (*name).to_string(),
            Module {
                name: (*name).to_string(),
                dir: PathBuf::from(name),
                units: vec![Unit { file, path, ast }],
            },
        );
    }
    let package = Package {
        root: PathBuf::from("."),
        config: Config::default(),
        modules: held,
    };
    match cove_sema::Compiler::new()
        .with_schemas(schemas.clone())
        .compile(&package)
    {
        Ok(program) => (sources, program),
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
///
/// The search is over the functions rather than over
/// [`crate::Program::by_name`], because a lambda is a function of this module
/// too and is not an entry point: it is named after the body that wrote it —
/// `f#0` — and it takes captures, so nothing outside the program that made it
/// could name it on a command line.
fn listing(source: &str, name: &str) -> String {
    let (sources, checked) = checked(source);
    let program = lower(&checked, &sources, &HostSchemas::new()).expect("the program lowers");
    let id = program
        .functions
        .iter()
        .position(|f| &*f.module == "m" && &*f.name == name)
        .map(|at| crate::FunctionId(at as u32))
        .unwrap_or_else(|| panic!("`{name}` was lowered"));
    crate::print::function(&program, id)
}

/// The disassembly of `module.name`, from a package of several modules.
fn listing_in(modules: &[(&str, &str)], module: &str, name: &str) -> String {
    let (sources, checked) = checked_package(modules, &HostSchemas::new());
    let program = lower(&checked, &sources, &HostSchemas::new()).expect("the program lowers");
    let id = program
        .functions
        .iter()
        .position(|f| &*f.module == module && &*f.name == name)
        .map(|at| crate::FunctionId(at as u32))
        .unwrap_or_else(|| panic!("`{module}.{name}` was lowered"));
    crate::print::function(&program, id)
}

/// The same listing, from a package checked and lowered against `schemas`.
fn listing_with(source: &str, schemas: &HostSchemas, name: &str) -> String {
    let (sources, checked) = checked_with(source, schemas);
    let program = lower(&checked, &sources, schemas).expect("the program lowers");
    let id = program
        .functions
        .iter()
        .position(|f| &*f.module == "m" && &*f.name == name)
        .map(|at| crate::FunctionId(at as u32))
        .unwrap_or_else(|| panic!("`{name}` was lowered"));
    crate::print::function(&program, id)
}

/// The disassembly of one function of a lowering sliced to `entry`.
///
/// The same listing [`listing`] takes, from [`crate::lower_entry`] rather
/// than from the whole package: what a slice leaves out is a stub, so a
/// listing is what says whether a declaration was lowered or stood in for.
fn sliced(source: &str, entry: &str, name: &str) -> String {
    sliced_to(source, &[entry], name)
}

/// The same listing, from a lowering sliced to several roots at once.
///
/// [`sliced`] is the one-root case of this, exactly as [`crate::lower_entry`]
/// is the one-root case of [`crate::lower_roots`].
fn sliced_to(source: &str, entries: &[&str], name: &str) -> String {
    let (sources, checked) = checked(source);
    let roots: Vec<(&str, &str)> = entries.iter().map(|entry| ("m", *entry)).collect();
    let program = crate::lower_roots(&checked, &sources, &HostSchemas::new(), &roots)
        .expect("the roots' program lowers");
    let id = program
        .functions
        .iter()
        .position(|f| &*f.module == "m" && &*f.name == name)
        .map(|at| crate::FunctionId(at as u32))
        .unwrap_or_else(|| panic!("`{name}` is in the program"));
    crate::print::function(&program, id)
}

/// What stopped a lowering, in the words it reported.
fn refused(source: &str) -> Vec<String> {
    let (sources, checked) = checked(source);
    match lower(&checked, &sources, &HostSchemas::new()) {
        Ok(_) => panic!("the program lowered, and this case is about what stops one"),
        Err(items) => items.into_iter().map(|item| item.message).collect(),
    }
}
