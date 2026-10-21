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

/// The names of `std.html`'s functions that have a body in `program`.
fn html_bodies(program: &Program) -> Vec<String> {
    program
        .functions
        .iter()
        .filter(|f| &*f.module == "std.html" && !f.is_stub())
        .map(|f| f.name.to_string())
        .collect()
}

/// `std.html` is named by the lowering, but only to assemble an `html"…"`
/// literal (ADR 0101), so it stays a module a program has only by importing
/// it: a package that writes none lowers none of it, even when it imports it.
#[test]
fn a_package_with_no_html_literal_lowers_none_of_std_html() {
    let program = package("fn f() -> Int { 1 }");
    assert_eq!(html_bodies(&program), Vec::<String>::new());
    let program = package(
        "use std.html\n\
         fn f() -> String { \"{1}\" }",
    );
    assert_eq!(html_bodies(&program), Vec::<String>::new());
}

/// A package that writes one has what its assembly calls, and only that: a
/// literal whose holes are a `String` and an `Int` escapes and wraps, and
/// appends no markup.
#[test]
fn an_html_literal_lowers_what_its_assembly_calls() {
    let program = package(
        "use std.html\n\
         use std.html.Html\n\
         fn f(name: String, count: Int) -> Html { html\"<b>{name}</b>{count}\" }",
    );
    let bodies = html_bodies(&program);
    for name in ["appendEscaped", "assembled"] {
        assert!(
            bodies.iter().any(|body| body == name),
            "`std.html.{name}` has no body: {bodies:?}"
        );
    }
    for name in ["appendMarkup", "appendEach", "render"] {
        assert!(
            !bodies.iter().any(|body| body == name),
            "`std.html.{name}` lowered though nothing reaches it: {bodies:?}"
        );
    }
}

/// `std.url` is on the same list (ADR 0107): a package that does not import
/// it has none of its bodies and none of its layouts, and one that calls
/// `parseForm` has what `parseForm` reaches and not `percentDecode`.
#[test]
fn std_url_is_lowered_only_as_far_as_a_package_reaches_it() {
    let url_bodies = |program: &Program| -> Vec<String> {
        program
            .functions
            .iter()
            .filter(|f| &*f.module == "std.url" && !f.is_stub())
            .map(|f| f.name.to_string())
            .collect()
    };
    let program = package("fn f() -> Int { 1 }");
    assert_eq!(url_bodies(&program), Vec::<String>::new());
    assert!(
        !program
            .layouts
            .iter()
            .any(|layout| layout.name.starts_with("std.url")),
        "a `std.url` layout in a package that never imports it"
    );

    let program = package(
        "use std.url\n\
         fn f(body: String) -> Array<String> { url.parseForm(body).all(\"tag\") }",
    );
    let bodies = url_bodies(&program);
    for name in ["parseForm", "component", "unescaped"] {
        assert!(
            bodies.iter().any(|body| body == name),
            "`std.url.{name}` has no body: {bodies:?}"
        );
    }
    assert!(
        !bodies.iter().any(|body| body == "percentDecode"),
        "`percentDecode` lowered though nothing reaches it: {bodies:?}"
    );
}

/// `std.datetime` is on the list too (ADR 0108): a package that does not
/// import it has none of its bodies and no `DateTime` layout, and one that
/// only formats has the writers and not `parseIso`.
#[test]
fn std_datetime_is_lowered_only_as_far_as_a_package_reaches_it() {
    let datetime_bodies = |program: &Program| -> Vec<String> {
        program
            .functions
            .iter()
            .filter(|f| &*f.module == "std.datetime" && !f.is_stub())
            .map(|f| f.name.to_string())
            .collect()
    };
    let program = package("fn f() -> Int { 1 }");
    assert_eq!(datetime_bodies(&program), Vec::<String>::new());
    assert!(
        !program
            .layouts
            .iter()
            .any(|layout| layout.name.starts_with("std.datetime")),
        "a `std.datetime` layout in a package that never imports it"
    );

    let program = package(
        "use std.datetime\n\
         fn f(millis: Int) -> String { datetime.fromUnixMillis(millis, 0).toIso() }",
    );
    let bodies = datetime_bodies(&program);
    for name in ["fromUnixMillis", "writeDate", "appendDigits"] {
        assert!(
            bodies.iter().any(|body| body == name),
            "`std.datetime.{name}` has no body: {bodies:?}"
        );
    }
    assert!(
        !bodies.iter().any(|body| body == "parseIso"),
        "`parseIso` lowered though nothing reaches it: {bodies:?}"
    );
}
