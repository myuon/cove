//! The standard library: Cove source embedded in the compiler binary.
//!
//! `cove_schema::builtins::STANDARD_LIBRARY` names which builtin methods have
//! moved out of Rust and into Cove, and points at the module each one lives
//! in. This module is where that module's source
//! actually is: `crates/cove-sema/std/*.cove`, read into the binary with
//! `include_str!` so a checked program never depends on a file existing on
//! disk at some path relative to the running `cove`.
//!
//! # This is the precompile boundary
//!
//! [`attach`] parses the embedded source into the caller's [`SourceMap`]
//! every time it is called, exactly as parsing any other module does. That
//! is deliberate and temporary: nothing about the standard library changes
//! between runs, so the day it is warm enough to matter, this is the one
//! function that changes — to answer a cached checked
//! [`Program`](crate::resolve::Program) or cached IR instead of parsing from
//! scratch — and every caller stays as it is. Do not build that cache before
//! there is a measurement asking for it; the point of writing it down here is
//! that nothing outside this function has to know when it arrives.

use std::path::PathBuf;

use cove_diag::{Diagnostic, SourceMap};

use crate::package::{Module, Unit};

/// One embedded standard-library source file and the module it belongs to.
struct StdSource {
    /// Dotted module name, such as `"std.array"`.
    module: &'static str,
    /// A path to show in a diagnostic or a stack trace. Never read from
    /// disk: the text beside it is what is actually parsed.
    path: &'static str,
    /// The file's contents, embedded at compile time.
    text: &'static str,
}

/// Every file the standard library is made of.
///
/// One file per receiver, which is why there are several holding one
/// function each: `Array` and `Vector` cannot share a body without a bound
/// the language does not have, so they do not share a file either. A method
/// migrating out of Rust adds a function to an existing file or a new file
/// here, and an entry to `cove_schema::builtins::STANDARD_LIBRARY` pointing
/// at it.
static SOURCES: &[StdSource] = &[
    StdSource {
        module: "std.array",
        path: "std/array.cove",
        text: include_str!("../std/array.cove"),
    },
    StdSource {
        module: "std.vector",
        path: "std/vector.cove",
        text: include_str!("../std/vector.cove"),
    },
    StdSource {
        module: "std.map",
        path: "std/map.cove",
        text: include_str!("../std/map.cove"),
    },
    StdSource {
        module: "std.set",
        path: "std/set.cove",
        text: include_str!("../std/set.cove"),
    },
    StdSource {
        module: "std.string",
        path: "std/string.cove",
        text: include_str!("../std/string.cove"),
    },
    StdSource {
        module: "std.option",
        path: "std/option.cove",
        text: include_str!("../std/option.cove"),
    },
    StdSource {
        module: "std.result",
        path: "std/result.cove",
        text: include_str!("../std/result.cove"),
    },
    StdSource {
        module: "std.int",
        path: "std/int.cove",
        text: include_str!("../std/int.cove"),
    },
    StdSource {
        module: "std.duration",
        path: "std/duration.cove",
        text: include_str!("../std/duration.cove"),
    },
];

/// Every module name the standard library declares.
///
/// This is `cove_sema::package::load`'s and `Compiler::compile`'s way of
/// asking "is the standard library here?" without parsing anything: a
/// package that already has a module by one of these names either loaded it
/// from `attach` or collides with it, and either way the answer does not
/// require a parse.
pub fn module_names() -> &'static [&'static str] {
    // If a later file adds a second module, dedupe here rather than asking
    // every caller to.
    static NAMES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names: Vec<&'static str> = SOURCES.iter().map(|source| source.module).collect();
        names.dedup();
        names
    })
}

/// Adds the standard library's sources to `sources` and answers the modules
/// to put in a package.
///
/// This must add to the *caller's* [`SourceMap`] rather than one of its own:
/// a [`Span`](cove_diag::Span) is an offset into whichever `SourceMap` it was
/// built against, and a diagnostic built from a span into a different map
/// than the one rendering it would point at the wrong file entirely. Calling
/// this is therefore always `attach(&mut sources)` where `sources` is the
/// same map the rest of the package's units are already in.
///
/// Adds the standard library to a package a host is composing.
///
/// This is the one call an embedder makes. `cove_sema::package::load` makes
/// it for a package read off disk; a host that composes its own — because
/// its sources are embedded, or generated, or come from somewhere that is
/// not a directory — makes it itself, and this is the whole of that step:
///
/// ```no_run
/// # use std::collections::BTreeMap;
/// # use cove_diag::SourceMap;
/// # use cove_sema::package::Module;
/// # fn f(sources: &mut SourceMap, modules: &mut BTreeMap<String, Module>) -> Result<(), Vec<cove_diag::Diagnostic>> {
/// cove_sema::stdlib::install(sources, modules)?;
/// # Ok(())
/// # }
/// ```
///
/// It is not done inside [`crate::Compiler::compile`], and that is a
/// decision rather than an omission: what `compile` is given should be a
/// package that is already whole, dependencies and all, so that what checks
/// is what the host assembled. `compile` refuses a package missing a module
/// `cove_schema::builtins::STANDARD_LIBRARY` names — see the diagnostic
/// `cove::compile::missing_stdlib`, which says to call this.
pub fn install(
    sources: &mut SourceMap,
    modules: &mut std::collections::BTreeMap<String, Module>,
) -> Result<(), Vec<Diagnostic>> {
    for (name, module) in attach(sources)? {
        modules.insert(name, module);
    }
    Ok(())
}

/// See the module doc for what this function is allowed to become without
/// its callers changing.
pub fn attach(sources: &mut SourceMap) -> Result<Vec<(String, Module)>, Vec<Diagnostic>> {
    let mut modules = Vec::with_capacity(SOURCES.len());
    let mut diagnostics = Vec::new();
    for source in SOURCES {
        let path = PathBuf::from(source.path);
        let file = sources.add(path.clone(), source.text);
        match cove_syntax::parse_file(sources, file) {
            Ok(ast) => modules.push((
                source.module.to_string(),
                Module {
                    name: source.module.to_string(),
                    dir: path.clone(),
                    units: vec![Unit { file, path, ast }],
                },
            )),
            Err(errs) => diagnostics.extend(errs),
        }
    }
    if diagnostics.is_empty() {
        Ok(modules)
    } else {
        Err(diagnostics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attaches_every_module_it_names() {
        let mut sources = SourceMap::new();
        let modules = attach(&mut sources).expect("the embedded standard library parses");
        let mut names: Vec<&str> = modules.iter().map(|(name, _)| name.as_str()).collect();
        names.sort();
        let mut expected: Vec<&str> = module_names().to_vec();
        expected.sort();
        assert_eq!(names, expected);
    }

    #[test]
    fn declares_every_function_the_schema_binds_to_it() {
        let mut sources = SourceMap::new();
        let modules = attach(&mut sources).expect("the embedded standard library parses");
        for binding in cove_schema::builtins::standard_library() {
            let (_, module) = modules
                .iter()
                .find(|(name, _)| name == binding.module)
                .unwrap_or_else(|| panic!("no embedded module named `{}`", binding.module));
            let declares = module.units.iter().any(|unit| {
                unit.ast.items.iter().any(|item| {
                    matches!(
                        &item.kind,
                        cove_syntax::ast::ItemKind::Fn(decl)
                            if decl.name.node == binding.function
                    )
                })
            });
            assert!(
                declares,
                "`{}.{}` names `{}.{}`, which that module does not declare",
                binding.receiver, binding.method, binding.module, binding.function
            );
        }
    }
}
