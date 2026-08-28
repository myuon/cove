//! The whole corpus, run through both backends, compared answer for answer.
//!
//! [ADR 0019](../../../docs/adr/0019-executable-ir-and-vm.md) leaves Cove with
//! two executable answers to what a program means, and says they must be kept
//! in agreement by tests rather than by hope.
//! [Issue #111](https://github.com/myuon/cove/issues/111) is the gate that
//! decides when the VM becomes the default, and this is its evidence: every
//! program the repository already keeps — every `[run.<name>]` under
//! `tests/e2e/`, `examples/`, and `benches/` — lowered, and then run on the
//! interpreter and on the VM against the same deterministic fakes.
//!
//! # A refusal is coverage, a disagreement is a failure
//!
//! `cove_ir::lower` refuses what it does not cover, so most of this corpus
//! does not reach the VM today. That is the measurement rather than the
//! problem: a case the lowering refuses is recorded with the construct it
//! named and counted, and the counts are printed as the roadmap for what to
//! lower next. A case that *does* lower and then answers differently on the
//! two backends is a failure, and the message shows both sides, because ADR
//! 0012 ranks the oracle above a backend and a backend that disagrees with
//! the oracle is wrong.
//!
//! Two assertions make this a ratchet rather than a report: everything that
//! lowers agrees, and the number of cases that lower never falls below
//! [`LOWERED_FLOOR`].
//!
//! # A case is a program, not a package
//!
//! `tests/e2e/` is seventy unrelated programs sharing one package for the
//! convenience of the harness that runs them, so a case is measured as the
//! program it is rather than as the package it sits in — and it is measured
//! that way twice over.
//!
//! Checking is sliced by module: a case is parsed and checked as its entry's
//! module plus the modules that module's `use` declarations reach,
//! transitively. That slicing is not a workaround but the corpus's own
//! shape: `tests/e2e/` keeps a dozen modules that deliberately do not check,
//! each pinning a check-time diagnostic, and a package holding one of those
//! does not check as a whole.
//!
//! Lowering is sliced by reachability: `cove_ir::lower::lower_entry` lowers
//! what the entry can reach and nothing else, so a construct the VM cannot
//! run refuses only the cases whose entry reaches it. This is the same call
//! `cove run --backend vm` makes with the same entry, so what this harness
//! measures and what the CLI runs are one program rather than two that could
//! drift.
//!
//! # What is compared, and what is not
//!
//! The value the entry answered or the structured error it failed with, every
//! line written to the fake console in order, how the run ended, and the fake
//! filesystem as the run left it. Fuel is not compared: ADR 0019 makes
//! `fuel_spent` backend-specific, since an instruction is not an AST node and
//! there is no honest mapping between them.
//!
//! An error's source position is compared exactly. It did not have to be —
//! an instruction's span covers the operation it came from and a tree walk's
//! covers the expression node, so the two could name one failure from a byte
//! apart — but across everything that lowers today they do not, and asserting
//! the weaker property would be recording less than is true.
//!
//! The hosts are the deterministic fakes `examples.rs` and `cove-bench`
//! already run against — a console that is a buffer, a virtual clock that
//! moves only when something moves it, an in-memory filesystem seeded from
//! the package's own `files/`, recorded documents, http, and rows — so
//! nothing here reaches the network or a real clock, and every answer is the
//! same on every machine.
//!
//! Budgets come from `[run.<name>]` except fuel and the deadline, which are
//! left off on purpose: fuel is backend-specific by ADR 0019, and a deadline
//! is wall-clock, so bounding either would make the two backends disagree by
//! construction rather than by fault. No case in the corpus sets one today.
//!
//! # Reading the coverage summary
//!
//! ```console
//! $ cargo test -p cove-cli --test differential -- --nocapture
//! ```
//!
//! The summary is printed on every run and repeated in the message of either
//! assertion that fails, so a failing run carries it without being asked.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use cove_diag::SourceMap;
use cove_runtime::budget::{Budget, Cancellation, Limits};
use cove_runtime::clock::{Clock, VirtualTime};
use cove_runtime::database::Database;
use cove_runtime::error::RuntimeError;
use cove_runtime::files::{Files, Tree};
use cove_runtime::host::{Console, Documents, Env, Grants, HostRegistry};
use cove_runtime::http::Http;
use cove_runtime::interp::Interpreter;
use cove_runtime::process::{Process, ProcessLog};
use cove_runtime::runtime::Runtime;
use cove_runtime::trace::RunOutcome;
use cove_runtime::value::Value;
use cove_runtime::vm::Vm;
use cove_sema::config::RunConfig;
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program as Checked;

/// How many corpus cases the lowering covers today.
///
/// A floor, not a target. Lowering one more construct raises it; nothing may
/// lower it, because a case that stopped lowering would be coverage lost
/// silently, and the whole point of counting is that it cannot be. Raise this
/// number in the same change that raises the coverage.
///
/// 55 to 56: a range used as a value builds one now — `cove_ir::Inst::MakeRange`
/// takes two `Int` bounds off the scalar stack and leaves the `Value::Range`
/// they make on the value stack — where the lowering previously had no
/// instruction that made one and refused every range a `for` header did not
/// consume. `tests/e2e:values_range` is the case that gained a lowering, and
/// it is the only one the corpus held.
///
/// It stayed at 56 when a variadic parameter began to lower, and that is
/// worth recording rather than leaving as a number that did not move.
/// `tests/e2e:fn_variadic` is the only case in the corpus that declares one,
/// and it also spreads — `joinAll("-", ...ready)` — so it now refuses for
/// the `...` standing behind the variadic parameter rather than for the
/// parameter itself. A spread is its own construct: it reads an `Array` or a
/// `Vector` and refuses anything else, which is a runtime question and not
/// one `make-array` answers.
///
/// 56 to 59: a method on a host resource handle is dispatched through the
/// boundary that issued the handle — `cove_ir::Inst::CallResource` stands the
/// handle below the arguments and lets `HostRegistry::call_resource` read the
/// module and the resource kind off it — where the lowering previously looked
/// the name up among the declared types and the builtins, found neither, and
/// refused. `tests/e2e:fail_http_stale_handle`, `tests/e2e:host_http_resource`
/// and `tests/e2e:host_files_streaming` are the cases that gained a lowering.
///
/// Six cases refused for a resource method and only three of them lower, which
/// is worth recording rather than leaving as a gap between two numbers. The
/// other three hold a second construct this backend does not cover and now
/// refuse for that instead: `examples:cq` and `examples:cqSample` for
/// `freeze`, and `examples:server` for `http.Route`, which initializes a type
/// a host declares.
/// 64 to 68: `freeze` takes the place rather than a read of it.
/// `builtins::freeze` consumes uniquely owned storage and refuses when a
/// second alias observes the vector, so a read of the receiver would be that
/// second alias and the check would refuse every vector `freeze` is written
/// for. `tests/e2e:coll_freeze`, `tests/e2e:fail_freeze_aliased`,
/// `examples:values` and `examples:cqSample` gained a lowering — the second
/// of those being the case that pins the refusal, which now happens
/// identically on both backends.
///
/// `examples:cq` refused for `freeze` too and did not gain one. It refuses
/// for `step: foldRevenue`, a function used as a value, which is one
/// construct further down the same program.
///
/// 68 to 70: a call that leaves a parameter to its default reaches a
/// specialisation of the callee. A default is evaluated by the callee — the
/// interpreter's `bind_params` reaches `None => match &param.default` inside
/// the frame it is filling — so a call that omits one is not the same call
/// with fewer arguments; it is a call to a function whose prologue computes
/// the rest. `cove_ir::lower` numbers one function per supplied-set, which
/// keeps the arity a call passes and the arity the callee takes the same
/// number and leaves the calling convention where it was.
/// `tests/e2e:fn_defaults` and `tests/e2e:fn_recursion` are the cases that
/// gained a lowering.
///
/// It stayed at 70 when `http.Route` began to lower, and that is worth
/// recording rather than leaving as a number that did not move.
/// `examples:server` is the only case in the corpus that initializes a type
/// a host declares, and the line that does it also writes `http.Method.Get`
/// — a case of an enum a host declares, which is a construct of its own —
/// so the case now refuses for that instead. The function it passes as
/// `handler:` is a third.
///
/// 70 to 71: `snapshot` splits by the receiver's type rather than by its
/// name. A struct or an enum with an `impl Snapshot for Type` was already a
/// `Call`, because the checker records which declaration that call reaches;
/// what was refused outright is the half of the trait no conformance answers
/// for, and `cove_ir::Inst::Snapshot` is that half — a `Vector`, which
/// allocates storage of its own, and every value with nothing mutable inside
/// it, which returns itself. A `Vector` whose elements would each dispatch is
/// still refused, because an instruction cannot run a whole Cove function in
/// the middle of itself. `tests/e2e:type_snapshot` is the case that gained a
/// lowering, and it is the only one the corpus held.
///
/// 71 to 72: a `...` argument spreads a sequence into a variadic parameter.
/// A variadic parameter receives one `Array` and `cove_ir::Inst::MakeArray`
/// already built it out of the leftover arguments; a spread is the same array
/// built out of a value, so `cove_ir::Inst::SpreadArgument` appends what one
/// holds — an `Array`'s elements or a `Vector`'s, and nothing else, which is
/// the pair `bind_params` reads. A call that mixes the two builds the array
/// in runs. Everywhere a variadic parameter is *not*, the interpreter reads a
/// spread argument's value and ignores its marking, so those are refused
/// rather than reproduced. `tests/e2e:fn_variadic` is the case that gained a
/// lowering, and it is the only one the corpus held.
///
/// 72 to 74: a lambda is lowered to a function of its own and the values the
/// environment around it handed over. `cove_ir::Function::captures` was
/// scaffolding until now — an explicit list with an explicit layout, decided
/// when the lambda is lowered rather than when the closure is created, which
/// is ADR 0019's "slots, not names" asked of a capture — and
/// `cove_ir::Inst::MakeClosure` fills it while `cove_ir::Inst::CallValue`
/// enters one. `tests/e2e:closures` and `tests/e2e:gc_cycles` are the cases
/// that gained a lowering.
///
/// Two of the three cases that refused for a closure are what moved.
/// `tests/e2e/backend_unsupported:backend_unsupported` is the third and did
/// not: it exists to pin ADR 0019's no-silent-fallback rule, so it was
/// rewritten around a task scope, which the lowering still refuses. What the
/// case is about is the rule and not the construct.
///
/// 74 to 76: a trailing closure is the last positional argument.
/// `Interpreter::eval_args` evaluates the written arguments and then pushes
/// the trailing one on the end with no label, no `var` and no spread, and the
/// parser has already built the block as a lambda — so once a lambda lowers
/// there is nothing left for the sugar to do but land where a written
/// argument would. `Args` is that said once, rather than a second parameter
/// every path that reads a call's arguments would have to remember to use.
/// `tests/e2e:type_result` and `examples:config` are the cases that gained a
/// lowering.
const LOWERED_FLOOR: usize = 76;

// ------------------------------------------------------------------ the test

/// Every case in the corpus, on both backends.
///
/// One `#[test]` rather than one per case: the corpus is discovered rather
/// than declared, so there is nothing to hang a test attribute on, and a
/// single run is what makes the coverage summary a summary.
#[test]
fn both_backends_agree_wherever_the_lowering_reaches() {
    // Everything happens on the stack the runtime sizes. The interpreter is a
    // recursive tree walker and a test thread's stack is not one it chose,
    // and a `cove_ir::Program` is `Rc`-based, so the lowering cannot cross
    // this boundary either. Only the report comes back out.
    let report = cove_runtime::on_cove_stack(run_the_corpus).expect("a thread to run Cove on");
    let summary = report.summary();
    print!("{summary}");

    assert!(
        report.disagreements.is_empty(),
        "{} case(s) answered differently on the two backends:\n\n{}\n{summary}",
        report.disagreements.len(),
        report.disagreements.join("\n")
    );
    assert!(
        report.lowered.len() >= LOWERED_FLOOR,
        "the lowering covered {} case(s), which is below the floor of {LOWERED_FLOOR}; \
         coverage may rise but never fall\n\n{summary}",
        report.lowered.len()
    );
}

/// Discovers the corpus, and runs every case of it.
fn run_the_corpus() -> Report {
    let mut report = Report::default();
    let cases = discover();
    assert!(!cases.is_empty(), "the corpus is empty");
    report.cases = cases.len();

    // One index per package rather than one per case: `tests/e2e` holds
    // seventy cases and a hundred modules, and what each module reaches is a
    // fact about the package that does not change between two of them.
    let mut indexes: BTreeMap<PathBuf, ModuleIndex> = BTreeMap::new();

    for case in cases {
        let index = indexes
            .entry(case.root.clone())
            .or_insert_with(|| ModuleIndex::of(&case.root));

        // A package that does not check has no program in it to lower or to
        // run. `tests/e2e` keeps such cases on purpose — each pins a
        // check-time diagnostic — so they are counted apart rather than
        // reported as anything the VM did or did not cover.
        let Some(prepared) = Prepared::of(&case, index) else {
            report.unchecked.push(case.name.clone());
            continue;
        };

        let (module, entry) = prepared.entry();
        // The same call `cove run --backend vm` makes, with the same entry:
        // what is lowered is what this entry reaches, so the harness and the
        // CLI mean one thing by "the program this entry is".
        let ir = match cove_ir::lower::lower_entry(&prepared.checked, module, entry) {
            Ok(lowered) => lowered.program,
            Err(why) => {
                report.refused.push((case.name.clone(), why.what.clone()));
                continue;
            }
        };
        if let Err(why) = cove_ir::lower::validate(&ir) {
            report
                .disagreements
                .push(format!("{}: the lowering is not valid: {why}", case.name));
            continue;
        }
        report.lowered.push(case.name.clone());

        let oracle = run_on_ast(&case, &prepared, module, entry);
        let backend = run_on_vm(&case, &prepared, &ir, module, entry);
        if oracle != backend {
            report
                .disagreements
                .push(disagreement(&case.name, &oracle, &backend));
        }
    }
    report
}

// -------------------------------------------------------------- the corpora

/// One program of the corpus: a `[run.<name>]` table, and the package it
/// belongs to.
struct Case {
    /// `tests/e2e:flow_if`, `examples:hello`, `benches:arith` — the package
    /// the run belongs to and the run's own name, which is unique across the
    /// corpus where the run name alone is not.
    name: String,
    /// The package root the run's entry is resolved against.
    root: PathBuf,
    run: RunConfig,
    /// The process arguments the case is run with, from the `args` file
    /// `tests/e2e` keeps beside a case that takes them.
    args: Vec<String>,
}

/// The repository root, from this crate's own directory.
fn repo_root() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    path.canonicalize()
        .unwrap_or_else(|e| panic!("cannot resolve `{}`: {e}", path.display()))
}

/// Every case of every corpus, in a fixed order.
///
/// The corpora are `tests/e2e/`, `examples/`, and `benches/`, and a case is a
/// `[run.<name>]` table of any `cove.toml` inside them — including the ones
/// an own-package `tests/e2e` case brings, which are packages of their own
/// exactly as `tests/e2e.rs` treats them.
fn discover() -> Vec<Case> {
    let root = repo_root();
    let mut roots = vec![root.join("tests/e2e")];
    roots.extend(nested_packages(&root.join("tests/e2e")));
    roots.push(root.join("examples"));
    roots.push(root.join("benches"));

    let mut cases = Vec::new();
    for package in roots {
        let text = std::fs::read_to_string(package.join("cove.toml"))
            .unwrap_or_else(|e| panic!("cannot read `{}/cove.toml`: {e}", package.display()));
        let config = cove_sema::config::parse(&text)
            .unwrap_or_else(|e| panic!("`{}/cove.toml`: {e}", package.display()));
        for (name, run) in config.runs {
            let args = read_args(&package, &name);
            cases.push(Case {
                name: format!("{}:{name}", relative(&root, &package)),
                root: package.clone(),
                run,
                args,
            });
        }
    }
    cases
}

/// Every directory below `root` that holds a `cove.toml` of its own.
///
/// Such a directory is a package rather than a module of `root`'s, which is
/// what `cove_sema::package::load` already decides and what lets a
/// check-time-failure case fail alone.
fn nested_packages(root: &Path) -> Vec<PathBuf> {
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
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// The process arguments a case is run with, one per line of its `args` file.
///
/// This is `tests/e2e.rs`'s own convention, read here so that a case that
/// takes arguments is compared having been given them.
fn read_args(package: &Path, name: &str) -> Vec<String> {
    std::fs::read_to_string(package.join(name).join("args"))
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

// ------------------------------------------------- one case, checked alone

/// One case's program: the modules it is made of, checked.
struct Prepared {
    sources: Arc<SourceMap>,
    checked: Arc<Checked>,
    /// `module.entry`, split, and owned so the borrow does not follow the
    /// `RunConfig` around.
    entry: (String, String),
}

impl Prepared {
    /// Parses and checks the case's entry module together with the modules
    /// `index` says it reaches, or `None` when that program does not check.
    fn of(case: &Case, index: &ModuleIndex) -> Option<Prepared> {
        let (module, entry) = case.run.entry_parts()?;
        let wanted = index.reachable(module)?;

        let mut sources = SourceMap::new();
        let mut modules = BTreeMap::new();
        for name in &wanted {
            let (dir, files) = index.modules.get(name)?;
            let mut units = Vec::new();
            for path in files {
                let text = std::fs::read_to_string(path).ok()?;
                let file = sources.add(path.clone(), &text);
                let ast = cove_syntax::parse_file(&sources, file).ok()?;
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
        // before it executes anything. The lowering reads the checker's
        // answers rather than recomputing them, so a program that does not
        // check is not a program either backend has an answer for — and
        // `tests/e2e` keeps a dozen such cases on purpose.
        let checked = cove_sema::Compiler::new().compile(&package).ok()?;
        checked.lookup_fn(module, entry)?;
        Some(Prepared {
            sources: Arc::new(sources),
            checked: Arc::new(checked),
            entry: (module.to_string(), entry.to_string()),
        })
    }

    fn entry(&self) -> (&str, &str) {
        (&self.entry.0, &self.entry.1)
    }
}

/// Every module of one package, and what each of them reaches.
///
/// The index is built by parsing the package once with the `use` declarations
/// the only thing read off it, because a module's dependencies are all that
/// decides which files a case's own program is made of.
struct ModuleIndex {
    /// Each module's directory and its `.cove` files, by dotted name.
    modules: BTreeMap<String, (PathBuf, Vec<PathBuf>)>,
    /// The modules of this package each module's `use` declarations name.
    uses: BTreeMap<String, BTreeSet<String>>,
}

impl ModuleIndex {
    fn of(root: &Path) -> ModuleIndex {
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
    fn reachable(&self, start: &str) -> Option<BTreeSet<String>> {
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
/// A directory holding its own `cove.toml` is a package and not a module of
/// this one, so the walk does not enter it — the rule
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

// ------------------------------------------------------------ the two runs

/// What one backend made of one case: everything the run can be observed by.
#[derive(PartialEq, Eq)]
struct Ran {
    /// The value the entry answered, rendered, or the structured error it
    /// failed with. Rendered rather than carried because a [`Value`] is
    /// `Rc`-based and belongs to the run that made it.
    answer: String,
    /// Every line written to the fake console, in the order they were
    /// written.
    console: Vec<String>,
    /// How the run ended, classified exactly as `run_entry` classifies it for
    /// the run's terminal trace event.
    outcome: RunOutcome,
    /// The fake filesystem as the run left it. A program told to write a file
    /// says on the console that it did, and the console line is not the file.
    files: BTreeMap<String, String>,
}

/// Runs the case on the interpreter, which is the oracle.
fn run_on_ast(case: &Case, prepared: &Prepared, module: &str, entry: &str) -> Ran {
    let (fakes, hosts) = Fakes::build(case);
    let runtime = Runtime::new(
        prepared.checked.clone(),
        prepared.sources.clone(),
        Arc::new(hosts),
    );
    let answer = Interpreter::new(&runtime).run_entry(module, entry, arguments(case));
    fakes.observed(answer)
}

/// Runs the same case on the VM, over the IR it was lowered to.
fn run_on_vm(
    case: &Case,
    prepared: &Prepared,
    ir: &cove_ir::Program,
    module: &str,
    entry: &str,
) -> Ran {
    let (fakes, hosts) = Fakes::build(case);
    let hosts = Arc::new(hosts);
    let runtime = Runtime::new(
        prepared.checked.clone(),
        prepared.sources.clone(),
        hosts.clone(),
    );
    let answer = Vm::new(&runtime, &hosts, ir).run_entry(module, entry, arguments(case));
    fakes.observed(answer)
}

/// The process arguments the entry is handed, as both backends take them.
fn arguments(case: &Case) -> Vec<Rc<str>> {
    case.args.iter().map(|arg| arg.as_str().into()).collect()
}

/// What a run can be observed through, kept where the test can read it back
/// once the run is over.
struct Fakes {
    console: Buffer,
    files: Tree,
}

impl Fakes {
    /// The hosts one run is given, and the handles onto the two of them that
    /// record what it did.
    ///
    /// Every host is registered whether or not this case reaches it, exactly
    /// as `cove run` registers them: the grants are what decide, so a
    /// capability a program reaches for without holding is refused with the
    /// reason rather than with a missing module.
    fn build(case: &Case) -> (Fakes, HostRegistry) {
        let console = Buffer::default();
        let files = Files::in_memory(seeded_files(&case.root));
        let tree = files.tree();

        let mut hosts = HostRegistry::new(Grants::new(case.run.allow.clone()));
        hosts.register(Box::new(Console::new(console.clone())));
        hosts.register(Box::new(Env::new(BTreeMap::new())));
        hosts.register(Box::new(Documents::in_memory(seeded_documents(&case.root))));
        hosts.register(Box::new(Clock::virtual_clock(VirtualTime::new())));
        hosts.register(Box::new(Database::recorded(BTreeMap::new())));
        hosts.register(Box::new(Http::recorded(BTreeMap::new(), Vec::new())));
        hosts.register(Box::new(Process::recorded(
            case.args.clone(),
            BTreeMap::new(),
            ProcessLog::new(),
        )));
        hosts.register(Box::new(files));
        hosts.set_budget(Budget::with_cancellation(
            limits(&case.run),
            Cancellation::new(),
        ));

        (
            Fakes {
                console,
                files: tree,
            },
            hosts,
        )
    }

    /// What the run left behind, beside what it answered.
    fn observed(self, answer: Result<Value, RuntimeError>) -> Ran {
        let outcome = match &answer {
            Ok(value) if value.is_err() => RunOutcome::Error,
            Ok(_) => RunOutcome::Success,
            Err(error) => error.outcome,
        };
        Ran {
            answer: describe(&answer),
            console: self.console.lines(),
            outcome,
            files: self.files.files(),
        }
    }
}

/// The budgets a case runs under.
///
/// Everything `[run.<name>]` sets except fuel and the deadline. Fuel is
/// backend-specific by ADR 0019 — an instruction is not an AST node — and a
/// deadline is wall-clock, so either one would make the two backends stop at
/// different points by construction rather than by fault. What is left counts
/// things both backends count the same way.
fn limits(run: &RunConfig) -> Limits {
    Limits {
        fuel: None,
        deadline: None,
        max_host_calls: run.max_host_calls,
        max_call_depth: None,
        max_tasks: run.max_tasks,
    }
}

/// One run's answer, rendered so that two of them can be compared and either
/// of them read.
///
/// A failure is rendered by its structure rather than by its message alone:
/// what it said, how it classified itself, which capability the boundary
/// refused, the rule it cited, and where in the source it points. #111 asks
/// that a runtime error keep useful Cove spans on both backends, and the
/// strongest form of that claim the corpus supports today is that the two
/// backends point at the same bytes.
fn describe(answer: &Result<Value, RuntimeError>) -> String {
    match answer {
        Ok(value) => format!("value {value:?}"),
        Err(error) => format!(
            "failed {:?}: {}\n    rule: {:?}\n    help: {:?}\n    denied: {:?}\n    at: {:?}",
            error.outcome,
            error.message,
            error.rule,
            error.help,
            error.denied_capability,
            error.span,
        ),
    }
}

/// The message a disagreement is reported with: both sides, in full.
///
/// ADR 0012 presumes the oracle right, so the interpreter's answer is named
/// first and named as the oracle. Which side is wrong is still a judgement,
/// and the message is what somebody makes it from.
fn disagreement(name: &str, oracle: &Ran, backend: &Ran) -> String {
    let mut out = format!("{name}: the two backends did not agree\n");
    let mut side = |which: &str, ran: &Ran| {
        let _ = write!(
            out,
            "  {which}:\n    outcome: {:?}\n    {}\n",
            ran.outcome, ran.answer
        );
        let _ = writeln!(out, "    console: {:?}", ran.console);
        if !ran.files.is_empty() {
            let _ = writeln!(out, "    files: {:?}", ran.files);
        }
    };
    side("ast (the oracle)", oracle);
    side("vm", backend);
    out
}

// -------------------------------------------------------------- the fakes

/// A `console` a run writes to and this test reads back.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Buffer {
    fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().expect("no run panics while printing"))
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("no run panics while printing")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The package's own `files/` directory, read into the in-memory filesystem
/// the run is given.
///
/// Reads answer what the case's fixtures actually hold, so a case that reads
/// a file is compared having read it; writes land in memory, so a run cannot
/// change the repository it was read out of.
fn seeded_files(root: &Path) -> BTreeMap<String, String> {
    let mut seeded = BTreeMap::new();
    read_tree(&root.join("files"), String::new(), &mut seeded);
    seeded
}

/// The package's own `documents/`, read the same way and for the same reason.
fn seeded_documents(root: &Path) -> BTreeMap<String, String> {
    let mut seeded = BTreeMap::new();
    read_tree(&root.join("documents"), String::new(), &mut seeded);
    seeded
}

/// Every readable file below `dir`, keyed by its `/`-separated path from it.
fn read_tree(dir: &Path, prefix: String, into: &mut BTreeMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let key = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            read_tree(&path, key, into);
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            into.insert(key, text);
        }
    }
}

// ------------------------------------------------------------- the summary

/// What the whole corpus came to.
#[derive(Default)]
struct Report {
    cases: usize,
    lowered: Vec<String>,
    /// Each refused case, and the construct the lowering named.
    refused: Vec<(String, String)>,
    /// Cases whose package does not check, which have no program to run.
    unchecked: Vec<String>,
    disagreements: Vec<String>,
}

impl Report {
    /// The coverage summary: how much of the corpus the VM covers today, and
    /// what stands between it and the rest.
    ///
    /// The refusals are grouped by construct and ordered by how many cases
    /// each one blocks, because that list is the roadmap for what to lower
    /// next and the order is the argument for which to lower first.
    fn summary(&self) -> String {
        let mut out = format!(
            "\ndifferential coverage over {} corpus case(s):\n  \
             {:>3} lowered, and agree on both backends\n  \
             {:>3} refused by the lowering\n  \
             {:>3} do not check, so there is nothing to run\n",
            self.cases,
            self.lowered.len(),
            self.refused.len(),
            self.unchecked.len(),
        );

        let mut by_construct: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (case, what) in &self.refused {
            by_construct
                .entry(what.as_str())
                .or_default()
                .push(case.as_str());
        }
        let mut ranked: Vec<(&str, Vec<&str>)> = by_construct.into_iter().collect();
        ranked.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));

        if !ranked.is_empty() {
            out.push_str("\nwhat the lowering refuses, most common first:\n");
            for (what, cases) in ranked {
                let _ = writeln!(out, "  {:>3}  {what}", cases.len());
                let _ = writeln!(out, "       first at {}", cases[0]);
            }
        }
        if !self.lowered.is_empty() {
            out.push_str("\nwhat the VM runs today:\n");
            for case in &self.lowered {
                let _ = writeln!(out, "       {case}");
            }
        }
        out
    }
}
