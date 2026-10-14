//! A whole-package lowering and the library modules a program only imports.
//!
//! `crate::lower` lowers every declaration a package holds, and the standard
//! library is attached to every package. `std.json` is the first library
//! module that neither a builtin binds nor the lowering names
//! (`cove_sema::stdlib::is_imported_library`), so it is lowered only as far
//! as the rest of the package reaches into it (ADR 0097). These cases pin
//! both halves: absent when nothing imports it, and complete — including a
//! function reached only as a value — when something does.

use cove_schema::HostSchemas;

use crate::program::Program;

use super::checked;

/// The whole-package lowering of `source` as module `m`.
fn package(source: &str) -> Program {
    let (sources, checked) = checked(source);
    crate::lower(&checked, &sources, &HostSchemas::new()).expect("the package lowers")
}

/// The names of `std.json`'s functions that have a body in `program`.
fn json_bodies(program: &Program) -> Vec<String> {
    program
        .functions
        .iter()
        .filter(|f| &*f.module == "std.json" && !f.is_stub())
        .map(|f| f.name.to_string())
        .collect()
}

/// A package that does not import `std.json` does not have it: no body, and
/// none of its layouts.
#[test]
fn a_package_that_does_not_import_the_module_lowers_none_of_it() {
    let program = package("fn f() -> Int { 1 }");
    assert_eq!(json_bodies(&program), Vec::<String>::new());
    assert!(
        !program
            .layouts
            .iter()
            .any(|layout| layout.name.starts_with("std.json")),
        "a `std.json` layout in a package that never imports it"
    );
}

/// A package that calls `parse` has `parse` and everything `parse` calls, and
/// nothing it does not: `renderPretty` is not reached.
#[test]
fn a_package_that_calls_into_the_module_lowers_what_it_reaches() {
    let program = package(
        "use std.json\n\
         fn f(text: String) -> Bool { json.parse(text).isOk() }",
    );
    let bodies = json_bodies(&program);
    for name in ["parse", "parseValue", "parseObject", "parseText", "escape"] {
        assert!(
            bodies.iter().any(|body| body == name),
            "`std.json.{name}` has no body: {bodies:?}"
        );
    }
    assert!(
        !bodies.iter().any(|body| body == "renderPretty"),
        "`renderPretty` lowered though nothing reaches it: {bodies:?}"
    );
}

/// A function reached only as a value, which a call graph need not show, is
/// still lowered rather than left a stub: a round that names it adds it.
#[test]
fn a_function_reached_only_as_a_value_is_lowered() {
    let program = package(
        "use std.json.Json\n\
         use std.json.render\n\
         fn f(values: Array<Json>) -> Array<String> { values.map(render) }",
    );
    let bodies = json_bodies(&program);
    for name in ["render", "quote", "renderNumber"] {
        assert!(
            bodies.iter().any(|body| body == name),
            "`std.json.{name}` has no body: {bodies:?}"
        );
    }
}
