//! The corpus-walking machinery `differential.rs` and `lvm_coverage.rs` both
//! need: what a `[run.<name>]` case is, which package's `cove.toml` holds it,
//! and — the part that takes the most care — parsing and type-checking
//! exactly the modules that case's entry reaches, and nothing else.
//!
//! This lives apart from either test so that there is one description of
//! "load this corpus case and check it" rather than two that could drift.
//! `lvm_coverage.rs` walks the whole corpus and reports what lowers, runs and
//! agrees; `differential.rs` walks the part of it that does not need a
//! benchmark's two million turns and compares the two runs far more closely,
//! down to the source span of a failure and the trace the run wrote. Neither
//! cares how the other answers its own question, so nothing about running
//! lives here — only discovery, parsing and type-checking, which both need
//! identically.
//!
//! # A case is a program, not a package
//!
//! `tests/e2e/` is many unrelated programs sharing one package for the
//! convenience of the harness that runs them, so a case is loaded as the
//! program it is rather than as the package it sits in — and that is true
//! twice over. Checking is sliced by module: a case is parsed and checked as
//! its entry's module plus the modules that module's `use` declarations
//! reach, transitively, which is what [`ModuleIndex`] answers. That slicing
//! is not a convenience but the corpus's own shape: `tests/e2e/` keeps a
//! dozen modules that deliberately do not check, each pinning a check-time
//! diagnostic, and a package holding one of those does not check as a whole.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_sema::config::RunConfig;
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program as Checked;

// -------------------------------------------------------------- the corpora

/// One program of a corpus: a `[run.<name>]` table, and the package it
/// belongs to.
pub struct Case {
    /// `tests/e2e:flow_if`, `examples:hello`, `benches:arith` — the package
    /// the run belongs to and the run's own name, which is unique across a
    /// corpus where the run name alone is not.
    pub name: String,
    /// The package root the run's entry is resolved against.
    pub root: PathBuf,
    pub run: RunConfig,
    /// The process arguments the case is run with, from the `args` file
    /// `tests/e2e` keeps beside a case that takes them. Unread by a caller
    /// that never executes the program — checking and lowering take no
    /// arguments — and kept here anyway so that a caller that does run it
    /// reads the same case a second loader would not be able to promise it.
    /// The `#[allow(dead_code)]` is there because each test compiles its own
    /// build of this module and neither reads every field of it: a field one
    /// build never touches is not dead code, it is the other build's.
    #[allow(dead_code)]
    pub args: Vec<String>,
}

/// The repository root, from this crate's own directory.
pub fn repo_root() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    path.canonicalize()
        .unwrap_or_else(|e| panic!("cannot resolve `{}`: {e}", path.display()))
}

/// Every `[run.<name>]` case of one package's `cove.toml`, named
/// `<root-relative package>:<run name>`.
pub fn cases_of(root: &Path, package: &Path) -> Vec<Case> {
    let text = std::fs::read_to_string(package.join("cove.toml"))
        .unwrap_or_else(|e| panic!("cannot read `{}/cove.toml`: {e}", package.display()));
    let config = cove_sema::config::parse(&text)
        .unwrap_or_else(|e| panic!("`{}/cove.toml`: {e}", package.display()));

    let mut cases = Vec::new();
    for (name, run) in config.runs {
        let mut args = read_args(package, &name);
        if let Some(smaller) = smaller_workload(&name) {
            args = smaller;
        }
        cases.push(Case {
            name: format!("{}:{name}", relative(root, package)),
            root: package.to_path_buf(),
            run,
            args,
        });
    }
    cases
}

/// The arguments that make a case's workload a test's size rather than its
/// own.
///
/// One case needs this. `examples:cqSample` is `cq.sample`, which writes a
/// file of records for the `cq` benchmark to read, and its own default is a
/// hundred thousand of them — sixteen megabytes, written twice, unoptimized,
/// by a test that is asking whether two backends agree. It was 258 of the
/// 340 seconds `differential.rs` spent running programs, which is more than
/// its other eighty-nine cases put together by a factor of sixty.
///
/// A hundred records reach every line of it that a hundred thousand do. The
/// entry already reads the count from its arguments, so this changes nothing
/// about what runs and only how many times the loop around it turns, and
/// `cove run cqSample` still writes what the benchmark expects.
///
/// This is a list rather than a rule because it should stay short enough to
/// read. A case that needs to be here is a case whose size was chosen for
/// something other than a test that executes it — which nothing here does,
/// but the args are shared with a loader that will.
pub fn smaller_workload(name: &str) -> Option<Vec<String>> {
    match name {
        "cqSample" => Some(vec!["100".to_string(), "bookings-sample.jsonl".to_string()]),
        _ => None,
    }
}

/// The process arguments a case is run with, one per line of its `args` file.
///
/// This is `tests/e2e.rs`'s own convention, read here so that a case that
/// takes arguments is loaded having them.
fn read_args(package: &Path, name: &str) -> Vec<String> {
    std::fs::read_to_string(package.join(name).join("args"))
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Every directory below `root` that holds a `cove.toml` of its own.
///
/// Such a directory is a package rather than a module of `root`'s, which is
/// what `cove_sema::package::load` already decides and what lets a
/// check-time-failure case fail alone.
pub fn nested_packages(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut names: Vec<PathBuf> = std::fs::read_dir(root)
        .unwrap_or_else(|e| panic!("cannot read `{}`: {e}", root.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    names.sort();
    for path in names {
        if !path.is_dir() || skipped_directory(&path) {
            continue;
        }
        if path.join("cove.toml").is_file() {
            found.push(path);
        } else {
            found.extend(nested_packages(&path));
        }
    }
    found
}

/// Whether the walk should not enter `path`: build output and dotted
/// directories, exactly what the package loader skips.
fn skipped_directory(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name.starts_with('.') || name == "target"
}

/// `root`-relative, with forward slashes, for a case name that reads the
/// same on every platform.
pub fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

// ------------------------------------------------------------- one package

/// Every module of one package, and what each of them reaches.
///
/// The index is built by parsing the package once with the `use` declarations
/// the only thing read off it, because a module's dependencies are all that
/// decides which files a case's own program is made of.
pub struct ModuleIndex {
    /// Each module's directory and its `.cove` files, by dotted name.
    modules: BTreeMap<String, (PathBuf, Vec<PathBuf>)>,
    /// The modules of this package each module's `use` declarations name.
    uses: BTreeMap<String, BTreeSet<String>>,
}

impl ModuleIndex {
    pub fn of(root: &Path) -> ModuleIndex {
        let mut modules = BTreeMap::new();
        walk(root, root, &mut modules);

        // The names are needed in full before any `use` can be read, since a
        // `use` names a module by its longest matching prefix and the module
        // it names may be discovered later in the walk.
        let mut sources = SourceMap::new();
        let known: BTreeSet<String> = modules.keys().cloned().collect();
        let mut uses: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (name, (_, files)) in &modules {
            let mut reached = BTreeSet::new();
            for path in files {
                let Ok(text) = std::fs::read_to_string(path) else {
                    continue;
                };
                let file = sources.add(path.clone(), &text);
                let Ok(ast) = cove_syntax::parse_file(&sources, file) else {
                    continue;
                };
                for used in &ast.uses {
                    let segments: Vec<&str> =
                        used.path.iter().map(|part| part.node.as_str()).collect();
                    // A `use` names a value, a type, or a whole module, so
                    // the module it reaches is the longest prefix that is
                    // one. `use console.println` names no module of this
                    // package at all, which is a host and not a dependency.
                    for length in (1..=segments.len()).rev() {
                        let candidate = segments[..length].join(".");
                        if known.contains(&candidate) {
                            reached.insert(candidate);
                            break;
                        }
                    }
                }
            }
            uses.insert(name.clone(), reached);
        }
        ModuleIndex { modules, uses }
    }

    /// `start` and everything it reaches, or `None` when this package has no
    /// such module.
    pub fn reachable(&self, start: &str) -> Option<BTreeSet<String>> {
        self.modules.get(start)?;
        let mut found = BTreeSet::new();
        let mut pending = vec![start.to_string()];
        while let Some(name) = pending.pop() {
            if !found.insert(name.clone()) {
                continue;
            }
            for next in self.uses.get(&name).into_iter().flatten() {
                pending.push(next.clone());
            }
        }
        Some(found)
    }
}

/// Turns every directory of `.cove` files below `dir` into a module named by
/// its dotted path from `root`.
///
/// A directory holding its own `cove.toml` is a package rather than a module
/// of this one, so the walk does not enter it — the rule
/// `cove_sema::package::load` follows, followed here for the same reason.
fn walk(root: &Path, dir: &Path, modules: &mut BTreeMap<String, (PathBuf, Vec<PathBuf>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();

    let mut cove_files = Vec::new();
    let mut subdirs = Vec::new();
    for path in paths {
        if path.is_dir() {
            if !skipped_directory(&path) {
                subdirs.push(path);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("cove") {
            cove_files.push(path);
        }
    }
    if !cove_files.is_empty() && dir != root {
        modules.insert(
            relative(root, dir).replace('/', "."),
            (dir.to_path_buf(), cove_files),
        );
    }
    for subdir in subdirs {
        if !subdir.join("cove.toml").is_file() {
            walk(root, &subdir, modules);
        }
    }
}

// ------------------------------------------------ one case, checked alone

/// Why a case did not become a [`Prepared`] program.
///
/// Two different facts, kept apart because a caller counting refusals must
/// not fold either into "refused": neither one reached a backend at all.
#[derive(Debug)]
pub enum Unprepared {
    /// The package parsed but the checker refused it, or a unit inside the
    /// modules the entry reaches did not even parse. `tests/e2e` keeps a
    /// dozen packages like this on purpose, each pinning a check-time
    /// diagnostic; there is no checked program in one of them for anything
    /// downstream to look at.
    DoesNotCheck,
    /// The run's `entry` does not name a module — or, within a module this
    /// package does declare, a function — this package's [`ModuleIndex`]
    /// or checked program can find. Distinct from [`Unprepared::DoesNotCheck`]
    /// because the package may check perfectly well; it is the entry that
    /// points nowhere.
    EntryNotResolved,
}

/// One case's program: the modules it is made of, checked.
pub struct Prepared {
    /// Read by a caller that lowers the checked program, since the lowering
    /// reports its gaps as diagnostics and a diagnostic points into source.
    #[allow(dead_code)]
    pub sources: Arc<SourceMap>,
    pub checked: Arc<Checked>,
    /// `module.entry`, split, and owned so the borrow does not follow the
    /// `RunConfig` around.
    entry: (String, String),
}

impl Prepared {
    /// Parses and checks the case's entry module together with the modules
    /// `index` says it reaches, or the [`Unprepared`] reason that program
    /// does not exist.
    pub fn of(case: &Case, index: &ModuleIndex) -> Result<Prepared, Unprepared> {
        let (module, entry) = case.run.entry_parts().ok_or(Unprepared::EntryNotResolved)?;
        let wanted = index
            .reachable(module)
            .ok_or(Unprepared::EntryNotResolved)?;

        let mut sources = SourceMap::new();
        let mut modules = BTreeMap::new();
        for name in &wanted {
            let (dir, files) = index
                .modules
                .get(name)
                .ok_or(Unprepared::EntryNotResolved)?;
            let mut units = Vec::new();
            for path in files {
                let text = std::fs::read_to_string(path).map_err(|_| Unprepared::DoesNotCheck)?;
                let file = sources.add(path.clone(), &text);
                let ast = cove_syntax::parse_file(&sources, file)
                    .map_err(|_| Unprepared::DoesNotCheck)?;
                units.push(Unit {
                    file,
                    path: path.clone(),
                    ast,
                });
            }
            modules.insert(
                name.clone(),
                Module {
                    name: name.clone(),
                    dir: dir.clone(),
                    units,
                },
            );
        }

        let package = Package {
            root: case.root.clone(),
            config: Default::default(),
            modules,
        };
        // Resolved *and* type-checked, which is what `cove run` requires
        // before it executes anything. A backend reads the checker's
        // answers rather than recomputing them, so a program that does not
        // check is not a program either backend — or this one — has an
        // answer for.
        let checked = cove_sema::Compiler::new()
            .compile(&package)
            .map_err(|_| Unprepared::DoesNotCheck)?;
        checked
            .lookup_fn(module, entry)
            .ok_or(Unprepared::EntryNotResolved)?;
        Ok(Prepared {
            sources: Arc::new(sources),
            checked: Arc::new(checked),
            entry: (module.to_string(), entry.to_string()),
        })
    }

    pub fn entry(&self) -> (&str, &str) {
        (&self.entry.0, &self.entry.1)
    }
}
