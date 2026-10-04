//! The standard library as a separately checked unit.
//!
//! Every package holds the standard library, and resolving and type-checking
//! its 8,900 lines was the fixed cost of every compile once
//! [`stdlib::attach`](crate::stdlib::attach) stopped parsing it again: about
//! 8 ms of a 9 ms tenant deploy in `examples/edge`. Nothing a package writes
//! changes what those passes conclude about the library's own modules, so
//! [ADR 0083](../../../docs/adr/0083-the-standard-library-is-checked-once-per-process.md)
//! does them once per process and has every later package *link* against the
//! result: [issue 569](https://github.com/myuon/cove/issues/569)'s stage 1.
//!
//! # What is kept, and what still runs per package
//!
//! Resolution and the check are each two kinds of work. One is per module —
//! a module's `use`s, its declarations, the call sites in its bodies, its
//! signatures, and the walk of its bodies — and for a library module none of
//! it reads a module of the package. The other is package-wide — import
//! cycles, method collisions, the call graph, the capability fixed point, the
//! `[run]` entries and `test fn`s, and the uniqueness proof — and it reads
//! every module at once.
//!
//! A [`Unit`] is the first kind for the library's modules, taken from a
//! compile that checked them in place: each module as resolution left it *before*
//! the package-wide passes, its call sites, its import edges, and what its
//! checker settled (the environment it exports, its signatures, and its
//! facts). A later compile puts those in place of the per-module work and
//! runs every package-wide pass as it always did, over the library's modules
//! and the package's together. So the link step is the existing passes, and
//! nothing about the result depends on whether the library came from a unit
//! or was checked in place: the same values reach the same passes.
//!
//! # Why a library module's work does not read the package
//!
//! The per-module work reads three things beyond the module itself, and each
//! is either the library's own or held fixed by a condition [`link`] checks:
//!
//! - **other modules**, through `use`: a library module imports only library
//!   modules. What could change that is a package module whose name is a
//!   prefix of, or extends, a library path — `use std.string` would then be
//!   ambiguous — so a package with a module named `std` or `std.*` that is
//!   not the library's is not linked.
//! - **host schemas**: a library module names no host module, which is
//!   asserted when a unit is kept (an empty `host_uses` and `host_items`), and
//!   every read of a schema in both passes goes through those. What remains
//!   is `module_shadows_host`, which asks whether a module a `use` names is
//!   also a host's name; a compilation whose schemas name `std` or `std.*` is
//!   not linked.
//! - **`OpaqueFields`**, the one package-wide input:
//!   resolution keys "this field holds a `dyn` value" by field *name* across
//!   the whole package, so a package's `struct Box { item: dyn Show }` makes
//!   every `.item` in the library read as opaque. The unit records each field
//!   name the library's walks asked about and the answer they got, and a
//!   package that answers any of them differently is not linked.
//!
//! The rest is held by construction. The check of a library module reads the
//! program only for the modules it imports, and a library function's
//! capability facts are not part of the unit at all: the fixed point is
//! package-wide and recomputes them.
//!
//! # Which library a package holds
//!
//! A unit is reused only for a package holding *the same parse*: the same
//! modules, at the same [`FileId`](cove_diag::FileId)s, sharing the same
//! function bodies. The facts it carries are keyed by those ids, and the
//! shared bodies are what `stdlib::attach` hands every package it answers
//! from one parse — so a package composed with a library of its own, or a
//! library parsed again at other ids, is checked in place. A unit is kept
//! only when the library's modules reported nothing while it was captured;
//! the library is clean, and a diagnostic would have to be merged back in
//! order.
//!
//! # When a unit is captured
//!
//! Not on the first compilation of a library, but on the second. Capturing
//! copies what the library's modules produced, which costs a compilation about
//! a millisecond, and a process that compiles once — every `cove` command —
//! would pay it for a unit nothing reads. So the first compilation of a parse
//! only notes that it was seen, the second captures, and the third and every
//! one after it links.
//!
//! Like the parses `stdlib` keeps, a process keeps a unit for at most
//! [`UNIT_LIMIT`] parses, and a package matching none of them when the table
//! is full is checked in place, as it always was.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use cove_diag::FileId;
use cove_schema::HostSchemas;
use cove_syntax::ast::{FnDecl, ItemKind};

use crate::package::Package;
use crate::{resolve, stdlib, typeck};

/// How a compilation treats the standard library's modules.
pub(crate) enum Link {
    /// Every module is resolved and checked in place.
    Whole,
    /// Every module is resolved and checked in place, and what the library's
    /// modules produced is kept for the compilations after this one.
    Capture(Box<Capture>),
    /// The library's modules are taken from a unit; only the package's own
    /// are resolved and checked.
    Library(Arc<Unit>),
}

impl Link {
    /// The unit this compilation links against, if it links against one.
    pub(crate) fn unit(&self) -> Option<&Unit> {
        match self {
            Link::Library(unit) => Some(unit),
            _ => None,
        }
    }

    /// What this compilation is keeping, if it is capturing.
    pub(crate) fn capture(&mut self) -> Option<&mut Capture> {
        match self {
            Link::Capture(capture) => Some(capture),
            _ => None,
        }
    }
}

/// The standard library, resolved and checked: everything a compilation
/// needs of its modules except the package-wide passes.
pub(crate) struct Unit {
    identity: Identity,
    pub(crate) resolution: resolve::LibraryResolution,
    pub(crate) check: BTreeMap<String, typeck::LibraryCheck>,
}

/// A unit being gathered by a compilation that checks the library in place.
pub(crate) struct Capture {
    /// The table the unit is kept in.
    into: &'static Units,
    identity: Identity,
    pub(crate) resolution: resolve::LibraryResolution,
    pub(crate) check: BTreeMap<String, typeck::LibraryCheck>,
    /// Set when a library module reported something, or read something a
    /// unit cannot carry: what was gathered is then not kept.
    pub(crate) spoiled: bool,
}

/// How many parses a process keeps a unit for.
///
/// The same bound as the parses [`stdlib::attach`] keeps, and for the same
/// reason: every package composed alike starts the library at the same id,
/// so one entry serves a host compiling thousands of them.
pub const UNIT_LIMIT: usize = 8;

/// The units a process keeps, and the parses it has compiled once.
type Units = Mutex<Table>;

#[derive(Default)]
struct Table {
    kept: Vec<Arc<Unit>>,
    /// Parses compiled once and not yet captured: the next compilation of
    /// one of them captures it.
    seen: Vec<Identity>,
}

fn units() -> &'static Units {
    static UNITS: OnceLock<Units> = OnceLock::new();
    UNITS.get_or_init(|| Mutex::new(Table::default()))
}

/// How `package`, compiled against `schemas`, treats the library's modules.
pub(crate) fn link(package: &Package, schemas: &HostSchemas) -> Link {
    link_in(units(), package, schemas)
}

/// [`link`], against the units kept in `table`.
fn link_in(table: &'static Units, package: &Package, schemas: &HostSchemas) -> Link {
    if !linkable(package, schemas) {
        return Link::Whole;
    }
    let Some(identity) = Identity::of(package) else {
        return Link::Whole;
    };
    let mut units = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(unit) = units.kept.iter().find(|unit| unit.identity.same(&identity)) {
        return Link::Library(Arc::clone(unit));
    }
    if units.kept.len() >= UNIT_LIMIT {
        return Link::Whole;
    }
    let Some(seen) = units.seen.iter().position(|seen| seen.same(&identity)) else {
        if units.seen.len() < UNIT_LIMIT {
            units.seen.push(identity);
        }
        return Link::Whole;
    };
    units.seen.swap_remove(seen);
    Link::Capture(Box::new(Capture {
        into: table,
        identity,
        resolution: resolve::LibraryResolution::default(),
        check: BTreeMap::new(),
        spoiled: false,
    }))
}

/// Keeps what a capturing compilation gathered, when it gathered all of it.
pub(crate) fn keep(link: Link) {
    let Link::Capture(capture) = link else {
        return;
    };
    let complete = stdlib::module_names()
        .iter()
        .all(|name| capture.resolution.holds(name) && capture.check.contains_key(*name));
    if capture.spoiled || !complete || !capture.resolution.names_no_host() {
        return;
    }
    let into = capture.into;
    let mut units = into.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if units.kept.len() >= UNIT_LIMIT
        || units
            .kept
            .iter()
            .any(|unit| unit.identity.same(&capture.identity))
    {
        return;
    }
    let Capture {
        identity,
        mut resolution,
        check,
        ..
    } = *capture;
    resolution.settle();
    units.kept.push(Arc::new(Unit {
        identity,
        resolution,
        check,
    }));
}

/// Whether nothing about `package` or `schemas` could change what the
/// library's modules resolve to; see the module documentation.
fn linkable(package: &Package, schemas: &HostSchemas) -> bool {
    let library_path = |name: &str| name == "std" || name.starts_with("std.");
    let modules_alone = package
        .modules
        .keys()
        .all(|name| stdlib::is_library_module(name) || !library_path(name));
    let hosts_alone = schemas.names().all(|name| !library_path(name));
    modules_alone && hosts_alone
}

/// Which parse of the library a package holds: each library module's files,
/// and its function bodies, which a parse shares with every copy of its trees.
struct Identity(Vec<ModuleIdentity>);

struct ModuleIdentity {
    files: Vec<FileId>,
    /// Held rather than compared by address alone, so that no body this was
    /// taken from can be freed and its address reused by another parse.
    bodies: Vec<Arc<FnDecl>>,
}

impl Identity {
    /// The identity of the library `package` holds, or `None` when it does
    /// not hold one module of it, or holds one with no function to recognise
    /// it by.
    fn of(package: &Package) -> Option<Identity> {
        let mut modules = Vec::new();
        for name in stdlib::module_names() {
            let module = package.modules.get(*name)?;
            let mut files = Vec::new();
            let mut bodies = Vec::new();
            for unit in &module.units {
                files.push(unit.file);
                for item in &unit.ast.items {
                    match &item.kind {
                        ItemKind::Fn(decl) => bodies.push(Arc::clone(decl)),
                        ItemKind::Impl(block) => {
                            for inner in &block.items {
                                if let ItemKind::Fn(decl) = &inner.kind {
                                    bodies.push(Arc::clone(decl));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            if bodies.is_empty() {
                return None;
            }
            modules.push(ModuleIdentity { files, bodies });
        }
        Some(Identity(modules))
    }

    fn same(&self, other: &Identity) -> bool {
        self.0.len() == other.0.len()
            && self.0.iter().zip(&other.0).all(|(a, b)| {
                a.files == b.files
                    && a.bodies.len() == b.bodies.len()
                    && a.bodies
                        .iter()
                        .zip(&b.bodies)
                        .all(|(x, y)| Arc::ptr_eq(x, y))
            })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use cove_diag::{render, Diagnostic, SourceMap};
    use cove_schema::{HostSchemas, ModuleSchema};

    use super::*;
    use crate::config::Config;
    use crate::package::{Module, Unit as SourceUnit};
    use crate::resolve::Program;
    use crate::Compiler;

    /// A table of units of a test's own, so that what it asserts does not
    /// depend on how many units the other tests in this process kept.
    fn table() -> &'static Units {
        Box::leak(Box::new(Mutex::new(Table::default())))
    }

    /// Composes packages of the same modules, each holding the same parse of
    /// the library.
    ///
    /// That is what `stdlib::attach` hands out when it has kept a parse, and
    /// it is kept here rather than relied on, because the process-wide table
    /// of parses may already be full of other tests' packages.
    struct Composer<'m> {
        modules: &'m [(&'m str, &'m str)],
        library: Option<Vec<(String, Module)>>,
    }

    impl<'m> Composer<'m> {
        fn new(modules: &'m [(&'m str, &'m str)]) -> Self {
            Composer {
                modules,
                library: None,
            }
        }

        /// A composer whose packages hold `library`, as another composer's of
        /// the same number of files did.
        fn holding(modules: &'m [(&'m str, &'m str)], library: Vec<(String, Module)>) -> Self {
            Composer {
                modules,
                library: Some(library),
            }
        }

        fn compose(&mut self) -> (SourceMap, Package) {
            let mut sources = SourceMap::new();
            let mut loaded = BTreeMap::new();
            for (name, text) in self.modules {
                let path = PathBuf::from(format!("{name}/main.cove"));
                let file = sources.add(path.clone(), *text);
                let ast = cove_syntax::parse_file(&sources, file).expect("the fixture parses");
                loaded.insert(
                    (*name).to_string(),
                    Module {
                        name: (*name).to_string(),
                        dir: PathBuf::from(name),
                        units: vec![SourceUnit { file, path, ast }],
                    },
                );
            }
            let attached = stdlib::attach(&mut sources).expect("the standard library parses");
            let library = self.library.get_or_insert(attached);
            loaded.extend(library.iter().cloned());
            let package = Package {
                root: PathBuf::new(),
                config: Config::default(),
                modules: loaded,
            };
            (sources, package)
        }
    }

    /// Everything a compilation answers, rendered so that two can be
    /// compared: every module, the call graph, every fact, and every
    /// diagnostic as `cove check` would print it.
    fn answer(sources: &SourceMap, result: &Result<Program, Vec<Diagnostic>>) -> String {
        match result {
            Ok(program) => format!(
                "{:?}\n{:?}\n{}\n{}",
                program.modules,
                program.call_graph,
                program.facts.describe(),
                program
                    .notices
                    .iter()
                    .map(|notice| render(sources, notice))
                    .collect::<String>()
            ),
            Err(items) => items.iter().map(|item| render(sources, item)).collect(),
        }
    }

    /// What [`compile_both_ways`] found.
    struct Both {
        /// Whether the package checked.
        checked: bool,
        /// Whether the second compilation linked against a unit.
        linked: bool,
        table: &'static Units,
        /// The parse of the library every package composed here held.
        library: Vec<(String, Module)>,
    }

    /// Compiles `modules` in place, and then as `Compiler::compile` does
    /// once two compilations of the same package have kept a unit, and
    /// asserts the two answer the same.
    fn compile_both_ways(modules: &[(&str, &str)], schemas: &HostSchemas) -> Both {
        let table = table();
        let compiler = Compiler::new().with_schemas(schemas.clone());
        let mut composer = Composer::new(modules);
        let (sources, whole) = composer.compose();
        let in_place = compiler.compile_linked(&whole, Link::Whole);
        for _ in 0..2 {
            let (_, earlier) = composer.compose();
            let _ = compiler.compile_linked(&earlier, link_in(table, &earlier, schemas));
        }
        let (_, again) = composer.compose();
        let decided = link_in(table, &again, schemas);
        let linked = matches!(decided, Link::Library(_));
        let answered = compiler.compile_linked(&again, decided);
        assert_eq!(answer(&sources, &answered), answer(&sources, &in_place));
        Both {
            checked: in_place.is_ok(),
            linked,
            table,
            library: composer.library.expect("a package was composed"),
        }
    }

    const PROGRAM: &str = "\
/// A value that can summarise itself.
export trait Summary {
  /// The one-line summary.
  fn summarize(self) -> String

  /// The line in a report.
  fn line(self) -> String {
    \"- {self.summarize()}\"
  }
}

/// A booking.
export struct Booking {
  id: Int
  guests: Int
}

impl Summary for Booking {
  fn summarize(self) -> String {
    \"booking {self.id} for {self.guests}\"
  }
}

/// Every non-empty word of `text`.
export fn words(text: String) -> Array<String> {
  var found: Vector<String> = Vector.of()
  for word in text.split(\" \") {
    if !word.isEmpty() {
      found.push(word.trim())
    }
  }
  found.freeze()
}

/// A report of entries of any type.
export fn report(entries: Array<dyn Summary>) -> String {
  var text = \"Report\"
  for entry in entries {
    text = \"{text}\\n{entry.line()}\"
  }
  text
}

/// Counts what `words` finds.
export fn count(text: String) -> Int {
  words(text).length()
}
";

    #[test]
    fn a_linked_package_answers_what_checking_it_whole_answers() {
        let both = compile_both_ways(&[("app", PROGRAM)], &HostSchemas::new());
        assert!(both.checked, "the program checks");
        assert!(
            both.linked,
            "a later compilation of the same package links against a unit"
        );
    }

    /// A process that compiles a library once — every `cove` command — pays
    /// nothing for units: the first compilation is in place and keeps
    /// nothing, the second captures, and only the third links.
    #[test]
    fn a_unit_is_captured_by_the_second_compilation_of_a_library() {
        let table = table();
        let modules = [("app", PROGRAM)];
        let mut composer = Composer::new(&modules);
        let compiler = Compiler::new();
        let mut decided = Vec::new();
        for _ in 0..4 {
            let (_, package) = composer.compose();
            let link = link_in(table, &package, &HostSchemas::new());
            decided.push(match &link {
                Link::Whole => "whole",
                Link::Capture(_) => "capture",
                Link::Library(_) => "library",
            });
            compiler
                .compile_linked(&package, link)
                .expect("the program checks");
        }
        assert_eq!(decided, ["whole", "capture", "library", "library"]);
    }

    #[test]
    fn a_package_that_does_not_check_reports_the_same_diagnostics_linked() {
        let broken = "\
/// Adds a string to a number.
export fn wrong(n: Int) -> Int {
  n + \"one\"
}

/// Calls nothing that exists.
export fn missing() -> Int {
  absent(1)
}
";
        let both = compile_both_ways(&[("app", broken)], &HostSchemas::new());
        assert!(!both.checked, "the program does not check");
        assert!(both.linked);
    }

    /// A unit records the field names the library's walks asked about. A
    /// package that declares one of them with a `dyn` type answers it
    /// differently, so its compilation resolves the library in place — and
    /// says the same as checking the whole package.
    #[test]
    fn a_package_that_answers_an_opaque_field_differently_is_checked_in_place() {
        let both = compile_both_ways(&[("app", PROGRAM)], &HostSchemas::new());
        let unit = Arc::clone(&both.table.lock().expect("not poisoned").kept[0]);
        let (name, _, _) = unit
            .resolution
            .opaque()
            .iter()
            .find(|(_, direct, container)| !direct && !container)
            .expect("the library reads some field that holds nothing opaque")
            .clone();
        let shadowing = format!(
            "{PROGRAM}
/// Holds something whose implementation its producer chose.
export struct Holder {{
  {name}: dyn Summary
}}
"
        );
        let modules = [("app", shadowing.as_str())];
        let mut composer = Composer::holding(&modules, both.library);
        let (sources, package) = composer.compose();
        let mut link = link_in(both.table, &package, &HostSchemas::new());
        assert!(
            matches!(link, Link::Library(_)),
            "the package holds the library the unit was kept for"
        );
        let _ = resolve::resolve_linked(&package, &HostSchemas::new(), &mut link);
        assert!(
            matches!(link, Link::Whole),
            "a package answering `{name}` differently resolves the library in place"
        );
        let compiler = Compiler::new();
        let (_, again) = composer.compose();
        let linked =
            compiler.compile_linked(&again, link_in(both.table, &again, &HostSchemas::new()));
        let in_place = compiler.compile_linked(&package, Link::Whole);
        assert_eq!(answer(&sources, &linked), answer(&sources, &in_place));
    }

    #[test]
    fn a_package_with_a_module_on_a_library_path_is_checked_in_place() {
        let helper = "\
/// Answers one.
export fn one() -> Int {
  1
}
";
        for name in ["std", "std.extra"] {
            let modules = [("app", PROGRAM), (name, helper)];
            let both = compile_both_ways(&modules, &HostSchemas::new());
            assert!(!both.linked, "a module named `{name}`");
            assert!(both.table.lock().expect("not poisoned").kept.is_empty());
        }
    }

    #[test]
    fn a_compilation_whose_hosts_name_a_library_path_is_checked_in_place() {
        const SHADOW: ModuleSchema = ModuleSchema {
            name: "std.extra",
            capability: "extra",
            operations: &[],
            types: &[],
            resources: &[],
        };
        let schemas = HostSchemas::new().with(SHADOW);
        let both = compile_both_ways(&[("app", PROGRAM)], &schemas);
        assert!(!both.linked);
        assert!(both.table.lock().expect("not poisoned").kept.is_empty());
    }

    /// A library whose function bodies are not the ones the unit was kept
    /// for — the same text, copied anew — is not the library the unit was
    /// checked from, so it is checked in place.
    #[test]
    fn a_library_from_another_parse_is_not_linked() {
        let modules = [("app", PROGRAM)];
        let both = compile_both_ways(&modules, &HostSchemas::new());
        assert!(both.linked);
        let (_, package) = Composer::holding(&modules, both.library.clone()).compose();
        assert!(matches!(
            link_in(both.table, &package, &HostSchemas::new()),
            Link::Library(_)
        ));
        let (_, mut package) = Composer::holding(&modules, both.library).compose();
        let module = package
            .modules
            .get_mut("std.array")
            .expect("the library is attached");
        for unit in &mut module.units {
            for item in &mut unit.ast.items {
                if let ItemKind::Fn(decl) = &mut item.kind {
                    *decl = Arc::new(FnDecl::clone(decl));
                }
            }
        }
        assert!(!matches!(
            link_in(both.table, &package, &HostSchemas::new()),
            Link::Library(_)
        ));
    }
}
