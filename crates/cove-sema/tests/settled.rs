//! The invariant: a program that checked carries no unresolved type.
//!
//! `cove check` reporting no error is a claim, and this is the claim stated
//! as something a test can fail. Every expression a checked package records
//! a type for has a type the checker *settled* — not a `Ty::Unknown` and not
//! a type with one inside it. A backend reading [`cove_sema::Facts`]
//! therefore never has to decide what to do about a type the checker did not
//! know, and can never be tempted to answer by dispatching dynamically:
//! there is nothing left for a dynamic dispatch to stand in for.
//!
//! # What is excluded, and why exactly one thing is
//!
//! [ADR 0016](../../../docs/adr/0016-four-kinds-of-unknown.md) names four
//! kinds of unknown. Three of them cannot appear here at all, and saying why
//! is most of the argument that the fourth is the only exclusion:
//!
//! - `Recovery` stands for "an error was already reported". A package with an
//!   error is not a package that checked, so this test never looks at one.
//! - `Placeholder` stands for a position no reachable program observes, and
//!   `Checker::expr` already asserts in debug builds that one never reaches
//!   an expression's type. This test would catch a release-build escape, but
//!   the claim is not new.
//! - `Var` is an inference variable, which
//!   [ADR 0036](../../../docs/adr/0036-an-inference-variable-is-not-a-kind-of-unknown.md)
//!   says never leaves the body that minted it. That property is asserted at
//!   the end of each body; this test is the same claim read off the finished
//!   table.
//!
//! `DynamicBoundary` is excluded, and it is the only exclusion. It stands for
//! a host module *this build* ships no schema for. That is a fact about the
//! build and not about the program: no edit to `sensors.read` fixes it, and
//! the remedy — handing the compiler the module's `ModuleSchema` — is one
//! thing to say however many calls the program makes. ADR 0016 puts the
//! diagnostic at the `use` for exactly that reason, and
//! `cove::resolve::unchecked_host` is where a reader is told. A program that
//! reaches such a host cannot be lowered, and that is honest: what is missing
//! is a description this compilation was never given.
//!
//! `Unconstrained` is what the invariant is about. Every one of those is
//! either something the checker should settle or something it should refuse,
//! and this test is what says so about the whole corpus at once rather than
//! about the cases somebody remembered to write down.
//! [ADR 0038](../../../docs/adr/0038-a-type-nothing-settles-is-not-a-program.md)
//! is where that decision is written down.
//!
//! `Ty::Any` is not excluded, because it is not an unknown. A Host API
//! schema writing `HostType::Any` has said something exact — every value is
//! accepted here, and what comes back is checked at the boundary and by
//! nothing before it — and a settled type is what carries that. It used to
//! be spelled `Unknown::Unconstrained`, which is the single largest reason
//! this invariant did not hold; ADR 0038 says why a promise and an absence
//! had to stop being one value.
//!
//! # Which expressions are asked
//!
//! Every expression [`cove_sema::Facts::types`] holds an entry for, with one
//! subtraction. `Facts` records the type of every expression `Checker::expr`
//! walked, and the checker walks two trees a run never evaluates:
//!
//! - **a probe.** `Checker::probe` walks a lambda body once to learn what it
//!   produces, throwing the diagnostics away, and the real walk records over
//!   the same ids afterwards. A probe of a tree the real walk then does not
//!   reach — because the call it belonged to was rejected — would leave its
//!   own answer behind, but such a package reported an error and is not
//!   looked at here.
//! - **an unreached body.** A module the checker stopped before finishing
//!   leaves partial entries. Again, only a package that reported nothing is
//!   asked.
//!
//! So for a package that checked without error the two coincide, and the
//! subtraction is empty in practice. What is not subtracted, and deliberately
//! so, is a body no entry point reaches: a function nothing calls is
//! executable — some other package's entry, a `test fn`, or an embedder's
//! `invoke` may reach it — and "nothing in this repository calls it" is not
//! a fact about the language. The one thing that would make an expression
//! genuinely unevaluatable is being in a position the grammar never
//! evaluates, and there is no such position: `cove_syntax::number` numbers
//! expressions, and every expression it numbers is one a run can reach.
//!
//! # What is walked
//!
//! Every package in the repository, sliced by module the way
//! `crates/cove-cli/tests/support/mod.rs` slices by entry, and for the same
//! reason: `tests/e2e/` is many unrelated programs sharing one package, a
//! dozen of which pin a check-time diagnostic on purpose, so the package as a
//! whole does not check and never will. A module and the modules its `use`
//! declarations reach is the largest unit that is a program. Slicing by
//! module rather than by `[run.<name>]` entry is a superset: a module no run
//! names is checked here too.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use cove_diag::{FileId, Severity, SourceMap, Span};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::typeck::{Ty, Unknown};
use cove_sema::Compiler;
use cove_syntax::ast::{
    Arg, Block, Expr, ExprId, ExprKind, Item, ItemKind, MatchArm, Param, Pattern, PatternKind,
    SourceUnit, StmtKind, StrPart,
};

// ------------------------------------------------------------ the invariant

/// Whether `ty` holds an unknown the invariant does not allow.
///
/// Every kind but `DynamicBoundary`, anywhere inside the type. A
/// `Vector<Unknown>` is as unsettled as an `Unknown`: the element type is
/// what a backend needs to place a value, and "the checker did not know"
/// is not an answer at any depth.
fn unsettled(ty: &Ty) -> Option<Unknown> {
    let mut found = None;
    each_unknown(ty, &mut |kind| {
        if kind != Unknown::DynamicBoundary && found.is_none() {
            found = Some(kind);
        }
    });
    found
}

/// Calls `f` with every unknown anywhere inside `ty`.
fn each_unknown(ty: &Ty, f: &mut impl FnMut(Unknown)) {
    match ty {
        Ty::Unknown(kind) => f(*kind),
        Ty::Array(inner)
        | Ty::Vector(inner)
        | Ty::Set(inner)
        | Ty::Option(inner)
        | Ty::Task(inner)
        | Ty::Shared(inner) => each_unknown(inner, f),
        Ty::Map(key, value) | Ty::MapEntry(key, value) | Ty::Result(key, value) => {
            each_unknown(key, f);
            each_unknown(value, f);
        }
        Ty::Struct(_, args) | Ty::Enum(_, args) => {
            for arg in args {
                each_unknown(arg, f);
            }
        }
        Ty::Fn(func) => {
            for param in &func.params {
                each_unknown(param, f);
            }
            each_unknown(&func.ret, f);
        }
        _ => {}
    }
}

/// One expression the invariant does not hold for.
struct Escape {
    /// `tests/e2e/type_result/main.cove:7:3`.
    where_: String,
    /// The source line it sits on, trimmed.
    line: String,
    kind: Unknown,
    ty: String,
}

// ------------------------------------------------------------------- corpus

#[test]
fn no_checked_package_leaves_an_unsettled_type() {
    let root = repo_root();
    let mut escapes = Vec::new();
    let mut checked = 0usize;
    let mut skipped = Vec::new();
    for package in packages(&root) {
        let index = ModuleIndex::of(&package);
        for module in index.names() {
            match check_slice(&package, &index, module) {
                Slice::Checked(found) => {
                    checked += 1;
                    escapes.extend(found);
                }
                Slice::DoesNotCheck => {
                    skipped.push(format!("{}:{module}", relative(&root, &package)))
                }
            }
        }
    }
    assert!(
        checked > 50,
        "the corpus walk found only {checked} checking module slices, which means it \
         stopped finding the corpus rather than that the corpus shrank"
    );
    report(&escapes, checked, &skipped);
}

/// Fails with every escape listed, or passes.
fn report(escapes: &[Escape], checked: usize, skipped: &[String]) {
    if escapes.is_empty() {
        return;
    }
    let mut text = format!(
        "{} expressions in {checked} checked module slices hold a type the checker \
         did not settle ({} slices do not check and were not asked):\n",
        escapes.len(),
        skipped.len()
    );
    for escape in escapes {
        let _ = writeln!(
            text,
            "  {}: {:?} in `{}`\n    {}",
            escape.where_, escape.kind, escape.ty, escape.line
        );
    }
    panic!("{text}");
}

/// Every package root in the repository: a directory holding a `cove.toml`.
fn packages(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if root.join("cove.toml").is_file() {
        found.push(root.to_path_buf());
    }
    let mut names: Vec<PathBuf> = std::fs::read_dir(root)
        .unwrap_or_else(|e| panic!("cannot read `{}`: {e}", root.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    names.sort();
    for path in names {
        if path.is_dir() && !skipped_directory(&path) {
            found.extend(packages(&path));
        }
    }
    found
}

/// Build output, dotted directories, and the Rust tree: what the package
/// loader skips, plus `crates/`, which holds no Cove package and is most of
/// the repository.
fn skipped_directory(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name.starts_with('.') || name == "target" || name == "crates"
}

fn repo_root() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    path.canonicalize()
        .unwrap_or_else(|e| panic!("cannot resolve `{}`: {e}", path.display()))
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

// ----------------------------------------------------------- one module slice

enum Slice {
    Checked(Vec<Escape>),
    DoesNotCheck,
}

/// Checks `start` together with the modules it reaches, and asks the
/// invariant of everything the check recorded.
fn check_slice(root: &Path, index: &ModuleIndex, start: &str) -> Slice {
    let Some(wanted) = index.reachable(start) else {
        return Slice::DoesNotCheck;
    };
    let mut sources = SourceMap::new();
    let mut modules = BTreeMap::new();
    let mut spans: BTreeMap<(u32, u32), Span> = BTreeMap::new();
    for name in &wanted {
        let (dir, files) = &index.modules[name];
        let mut units = Vec::new();
        for path in files {
            let Ok(text) = std::fs::read_to_string(path) else {
                return Slice::DoesNotCheck;
            };
            let file = sources.add(path.clone(), &text);
            let Ok(ast) = cove_syntax::parse_file(&sources, file) else {
                return Slice::DoesNotCheck;
            };
            collect_spans(&ast, file, &mut spans);
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
        root: root.to_path_buf(),
        config: Default::default(),
        modules,
    };
    let Ok(program) = Compiler::new().compile(&package) else {
        return Slice::DoesNotCheck;
    };
    Slice::Checked(escapes_of(&program, &sources, &spans))
}

/// Every expression of a checked program whose type is not settled.
fn escapes_of(
    program: &cove_sema::resolve::Program,
    sources: &SourceMap,
    spans: &BTreeMap<(u32, u32), Span>,
) -> Vec<Escape> {
    let mut found = Vec::new();
    for (file, id, ty) in program.facts.types() {
        let Some(kind) = unsettled(ty) else { continue };
        let span = spans.get(&(file.0, id.0)).copied();
        found.push(escape(sources, file, id, span, kind, ty));
    }
    found
}

fn escape(
    sources: &SourceMap,
    file: FileId,
    id: ExprId,
    span: Option<Span>,
    kind: Unknown,
    ty: &Ty,
) -> Escape {
    let source = sources.get(file);
    let (where_, line) = match span {
        Some(span) => {
            let (line, column) = source.line_col(span.start);
            (
                format!("{}:{line}:{column}", source.path.display()),
                source.line_text(line).trim().to_string(),
            )
        }
        // The span table is built by a walk of this test's own, so a missing
        // entry is this file's bug rather than the checker's. Saying which
        // expression it was is still enough to find it.
        None => (
            format!("{} (expression #{})", source.path.display(), id.0),
            String::new(),
        ),
    };
    Escape {
        where_,
        line,
        kind,
        ty: ty.to_string(),
    }
}

// ------------------------------------------------------------ module slicing

/// Every module of one package, and the modules each one's `use`
/// declarations reach.
struct ModuleIndex {
    modules: BTreeMap<String, (PathBuf, Vec<PathBuf>)>,
    uses: BTreeMap<String, BTreeSet<String>>,
}

impl ModuleIndex {
    fn of(root: &Path) -> ModuleIndex {
        let mut modules = BTreeMap::new();
        walk(root, root, &mut modules);
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
                    // one.
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

    fn names(&self) -> Vec<&str> {
        self.modules.keys().map(String::as_str).collect()
    }

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

// -------------------------------------------------------------- id to span

/// The span of every expression in `unit`, keyed by file and id.
///
/// `cove_syntax::number` gives an expression its id and holds no span table,
/// and `Facts` is indexed by id and holds no spans either, so a reader that
/// has a fact and wants a source location has to walk the tree. This is that
/// walk, and it exists only so that a failure names a line rather than a
/// number. It mirrors `cove_syntax::number`'s own walk; where the two
/// disagree the answer is a wrong line in a failure message, not a wrong
/// verdict.
fn collect_spans(unit: &SourceUnit, file: FileId, into: &mut BTreeMap<(u32, u32), Span>) {
    for item in &unit.items {
        item_spans(item, file, into);
    }
}

fn item_spans(item: &Item, file: FileId, into: &mut BTreeMap<(u32, u32), Span>) {
    match &item.kind {
        ItemKind::Fn(decl) => {
            param_spans(&decl.params, file, into);
            block_spans(&decl.body, file, into);
        }
        ItemKind::Struct(_) | ItemKind::Enum(_) | ItemKind::TypeAlias(_) => {}
        ItemKind::Trait(decl) => {
            for method in &decl.methods {
                param_spans(&method.params, file, into);
                if let Some(body) = &method.default {
                    block_spans(body, file, into);
                }
            }
        }
        ItemKind::Impl(block) => {
            for item in &block.items {
                item_spans(item, file, into);
            }
        }
    }
}

fn param_spans(params: &[Param], file: FileId, into: &mut BTreeMap<(u32, u32), Span>) {
    for param in params {
        if let Some(default) = &param.default {
            expr_spans(default, file, into);
        }
    }
}

fn block_spans(block: &Block, file: FileId, into: &mut BTreeMap<(u32, u32), Span>) {
    for stmt in &block.statements {
        match &stmt.kind {
            StmtKind::Let { value, .. } | StmtKind::Expr(value) => expr_spans(value, file, into),
            StmtKind::Item(item) => item_spans(item, file, into),
        }
    }
    if let Some(tail) = &block.tail {
        expr_spans(tail, file, into);
    }
}

fn expr_spans(expr: &Expr, file: FileId, into: &mut BTreeMap<(u32, u32), Span>) {
    into.insert((file.0, expr.id.0), expr.span);
    let one = |inner: &Expr, into: &mut BTreeMap<(u32, u32), Span>| expr_spans(inner, file, into);
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Duration(_)
        | ExprKind::Unit
        | ExprKind::Ident(_)
        | ExprKind::Continue => {}
        ExprKind::Str(parts) => {
            for part in parts {
                if let StrPart::Interpolation(inner) = part {
                    one(inner, into);
                }
            }
        }
        ExprKind::ArrayLit(elements) => {
            for element in elements {
                one(element, into);
            }
        }
        ExprKind::Field { base, .. } => one(base, into),
        ExprKind::Call {
            callee,
            args,
            trailing,
            ..
        } => {
            one(callee, into);
            arg_spans(args, file, into);
            if let Some(trailing) = trailing {
                one(trailing, into);
            }
        }
        ExprKind::Unary { operand, .. } => one(operand, into),
        ExprKind::Binary { lhs, rhs, .. } => {
            one(lhs, into);
            one(rhs, into);
        }
        ExprKind::Assign { target, value, .. } => {
            one(target, into);
            one(value, into);
        }
        ExprKind::Try(inner) | ExprKind::Await(inner) => one(inner, into),
        ExprKind::Block(block) => block_spans(block, file, into),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            one(condition, into);
            block_spans(then_branch, file, into);
            if let Some(else_branch) = else_branch {
                one(else_branch, into);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            one(scrutinee, into);
            for arm in arms {
                arm_spans(arm, file, into);
            }
        }
        ExprKind::For { iterable, body, .. } => {
            one(iterable, into);
            block_spans(body, file, into);
        }
        ExprKind::While { condition, body } => {
            one(condition, into);
            block_spans(body, file, into);
        }
        ExprKind::Return(value) | ExprKind::Break(value) => {
            if let Some(value) = value {
                one(value, into);
            }
        }
        ExprKind::Lambda { params, body, .. } => {
            param_spans(params, file, into);
            block_spans(body, file, into);
        }
        ExprKind::Scope { body, .. } => block_spans(body, file, into),
        ExprKind::Range { start, end, .. } => {
            one(start, into);
            one(end, into);
        }
    }
}

fn arg_spans(args: &[Arg], file: FileId, into: &mut BTreeMap<(u32, u32), Span>) {
    for arg in args {
        expr_spans(&arg.value, file, into);
    }
}

fn arm_spans(arm: &MatchArm, file: FileId, into: &mut BTreeMap<(u32, u32), Span>) {
    pattern_spans(&arm.pattern, file, into);
    expr_spans(&arm.body, file, into);
}

fn pattern_spans(pattern: &Pattern, file: FileId, into: &mut BTreeMap<(u32, u32), Span>) {
    match &pattern.kind {
        PatternKind::Literal(expr) => expr_spans(expr, file, into),
        PatternKind::Variant { payload, .. } => {
            for inner in payload {
                pattern_spans(inner, file, into);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------- fixtures

/// The invariant asked of a program written here rather than found on disk,
/// so that it is stated in one place a reader can see whole.
fn settles(source: &str) -> Result<(), String> {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("app/main.cove");
    let file = sources.add(path.clone(), source);
    let ast = cove_syntax::parse_file(&sources, file).expect("the fixture parses");
    let mut spans = BTreeMap::new();
    collect_spans(&ast, file, &mut spans);
    let package = Package {
        root: PathBuf::new(),
        config: Default::default(),
        modules: [(
            "app".to_string(),
            Module {
                name: "app".to_string(),
                dir: PathBuf::from("app"),
                units: vec![Unit { file, path, ast }],
            },
        )]
        .into_iter()
        .collect(),
    };
    let program = Compiler::new()
        .compile(&package)
        .map_err(|items| render_all(&sources, &items))?;
    let escapes = escapes_of(&program, &sources, &spans);
    if escapes.is_empty() {
        Ok(())
    } else {
        Err(escapes
            .iter()
            .map(|e| format!("{}: {:?} in `{}`\n", e.where_, e.kind, e.ty))
            .collect())
    }
}

fn render_all(sources: &SourceMap, items: &[cove_diag::Diagnostic]) -> String {
    items
        .iter()
        .filter(|item| item.severity == Severity::Error)
        .map(|item| cove_diag::render(sources, item))
        .collect()
}

/// A program written here that the checker refuses, with the code it was
/// refused for.
fn refused(source: &str) -> String {
    match settles(source) {
        Ok(()) => panic!("this was expected to be refused, and it checked:\n{source}"),
        Err(text) => text,
    }
}

/// `main` around `body`, with `items` written above it.
fn program(items: &str, body: &str) -> String {
    format!(
        "use console.println\n\n{items}/// Entry.\nexport fn main() -> Result<Unit, Error> {{\n{body}\n  Ok(())\n}}\n"
    )
}

/// The shape the invariant is about, in one place: a `Vector` whose element
/// type a later use settles carries that type and nothing open.
#[test]
fn a_later_use_settles_what_a_collection_holds() {
    settles(&program(
        "",
        "  var log = Vector.of()\n  log.push(\"one\")\n  println(\"{log}\")?",
    ))
    .expect("a use settles it");
}

/// The case that made this invariant worth writing: what an earlier use
/// settled reaches a *binding* made from it, and not only `Facts` at the end
/// of the body.
///
/// `found.push(item)` says what `found` holds; `sorted(by:)`'s callback then
/// binds `a` and `b` from it. Before, they were bound to a hole and
/// `a.rank > b.rank` was a recovery unknown in a package that checked clean.
#[test]
fn a_callback_parameter_takes_what_an_earlier_use_settled() {
    settles(&program(
        "/// A thing with an order.\nstruct Item {\n  rank: Int\n}\n\n",
        "  var found = Vector.of()\n  found.push(Item(rank: 1))\n  \
         let ordered = found.freeze().sorted(by: fn(a, b) {\n    a.rank > b.rank\n  })\n  \
         println(\"{ordered.length()}\")?",
    ))
    .expect("the callback's parameters take what the push settled");
}

/// A value a schema declares `Any` carries `Any`, which is a type and not an
/// unknown: the schema said there is nothing here that depends on one.
#[test]
fn a_schemas_any_is_a_type_and_not_an_unknown() {
    settles(&format!(
        "use clock.timeout\nuse console.println\n\n{}",
        "/// Entry.\nexport fn main() -> Result<Unit, Error> {\n  \
         let answered = timeout(60s) {\n    41 + 1\n  }?\n  \
         println(\"{answered}\")?\n  Ok(())\n}\n"
    ))
    .expect("a bounded body's result is `Any`, which is settled");
}

/// Each of the five ways a type could be left open, refused.
///
/// One test rather than five, because what is being pinned is that the list
/// is closed: these are every shape the corpus walk above turned up, and a
/// sixth would show as a corpus failure rather than as a missing case here.
#[test]
fn every_way_of_leaving_a_type_open_is_refused() {
    let refusals: &[(&str, &str, &str)] = &[
        (
            "a binding whose initializer opens a type and whose uses settle none",
            "  var log = Vector.of()\n  println(\"{log.length()}\")?",
            "cove::type::unconstrained",
        ),
        (
            "a value no binding holds at all",
            "  println(\"{Vector.of().length()}\")?",
            "cove::type::unconstrained",
        ),
        (
            "an empty array literal",
            "  let empty = []\n  println(\"{empty.length()}\")?",
            "cove::type::unconstrained",
        ),
        (
            "a bare `None`",
            "  let missing = None\n  println(\"{missing.isNone()}\")?",
            "cove::type::unconstrained",
        ),
        (
            "an unannotated lambda parameter nothing expects a type of",
            "  let double = fn(n) { n * 2 }\n  println(\"{double(4)}\")?",
            "cove::type::unconstrained",
        ),
        (
            "a `Result` whose failure type nothing states",
            "  let ok = Ok(1)\n  println(\"{ok.isOk()}\")?",
            "cove::type::unconstrained",
        ),
        (
            "a type that contains itself, which is regular, finitely \
             representable, and unwritable",
            "  var v = Vector.of()\n  v.push(v)",
            "cove::type::recursive_type",
        ),
    ];
    for (what, body, code) in refusals {
        let reported = refused(&program("", body));
        assert!(
            reported.contains(code),
            "{what} is refused with `{code}`, and was reported as:\n{reported}"
        );
    }
}

/// The one exclusion, from the other side: a host module this build ships no
/// schema for leaves `DynamicBoundary` unknowns, the package still checks,
/// and the invariant deliberately does not object.
///
/// The remedy is `Compiler::with_host_schema`, which is a fact about the
/// build; `cove::resolve::unchecked_host` says so at the `use`. Such a
/// program cannot be lowered, and that is honest.
#[test]
fn a_host_no_schema_describes_is_the_one_thing_left_unsettled() {
    let source = "use sensors\n\n/// Entry.\nexport fn main() -> Result<Unit, Error> {\n  \
                  let reading = sensors.read()\n  \
                  console.println(\"{reading.value}\")?\n  Ok(())\n}\n";
    settles(&format!("use console\n{source}")).expect("an unschema'd host still checks");
}
