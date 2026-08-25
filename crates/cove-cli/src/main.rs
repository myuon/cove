//! The `cove` command-line tool.
//!
//! The CLI does not invent semantics: the compiler derives facts, the runtime
//! enforces and records them, and the CLI explains them.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use cove_diag::{render, Diagnostic, SourceMap, Span};
use cove_runtime::clock::Clock;
use cove_runtime::host::{Console, Documents, Env, Grants, HostRegistry};
use cove_runtime::interp::Interpreter;
use cove_runtime::{Budget, Cancellation, JsonlSink, Limits, NullSink, TraceEvent, TraceSink};
use cove_sema::package::Package;
use cove_sema::resolve::{
    AliasEntry, EnumEntry, FnEntry, Program, ResolvedModule, StructEntry, TraitEntry,
};
use cove_syntax::ast::ItemKind;

const USAGE: &str = "\
cove — the Cove toolchain

usage:
  cove fmt [path] [--check]            format every `.cove` file in the package
  cove check [path] [--deny-warnings]  parse, resolve, and type-check the package
  cove run <name> [flags] [args]       run the entry selected by `[run.<name>]` in cove.toml
  cove outline [path]                  show modules and their exported declarations
  cove help                            show this message

`cove fmt` rewrites files in place and prints how many changed. `--check`
writes nothing, prints the path of every file that would change, and exits
non-zero when there is one, which is the form to run in CI. A file that does
not parse is reported and never rewritten.

`--deny-warnings` fails `cove check` when the package has any warnings, as
does setting `deny_warnings = true` in `cove.toml`'s `[check]` table; either
one is enough to deny.

`cove run` flags (may appear in any position after <name>; everything after a
literal `--` is a program argument, even if it looks like a flag):
  --fuel <n>            stop the run after <n> fuel is spent
  --deadline <duration>  stop the run after <duration> has elapsed, e.g. `500ms`, `5s`, `1h`
  --max-host-calls <n>  stop the run after <n> host calls
  --trace <path>        write a JSONL trace to <path>, or `-` for stderr
  --stats               print fuel spent, host calls, elapsed time, and host-call wait to stderr
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("help");

    let result = match command {
        "fmt" => cmd_fmt(&args[1..]),
        "check" => cmd_check(&args[1..]),
        "run" => cmd_run(&args[1..]),
        "outline" => cmd_outline(args.get(1).map(Path::new)),
        "help" | "-h" | "--help" => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        other => Err(CliError::Message(format!(
            "unknown command `{other}`\n\n{USAGE}"
        ))),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Message(message)) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
        Err(CliError::Diagnostics { sources, items }) => {
            for item in &items {
                eprint!("{}", render(&sources, item));
            }
            let errors = items
                .iter()
                .filter(|d| d.severity == cove_diag::Severity::Error)
                .count();
            if errors > 0 {
                eprintln!("{errors} error(s)");
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(CliError::WarningsDenied) | Err(CliError::Unformatted) => ExitCode::FAILURE,
    }
}

enum CliError {
    Message(String),
    Diagnostics {
        sources: SourceMap,
        items: Vec<Diagnostic>,
    },
    /// `cove check --deny-warnings` found warnings. The warnings and summary
    /// were already printed, so there is nothing left to say.
    WarningsDenied,
    /// `cove fmt --check` found files that are not formatted. Their paths
    /// were already printed, so there is nothing left to say.
    Unformatted,
}

impl From<String> for CliError {
    fn from(message: String) -> Self {
        CliError::Message(message)
    }
}

/// Loads and resolves the package containing `start`.
fn load(start: Option<&Path>) -> Result<(SourceMap, Package, Program), CliError> {
    let start = match start {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()
            .map_err(|e| CliError::Message(format!("cannot read the current directory: {e}")))?,
    };
    let root = find_root(&start).ok_or_else(|| {
        CliError::Message(format!(
            "no `cove.toml` found in `{}` or any parent directory",
            start.display()
        ))
    })?;

    let mut sources = SourceMap::new();
    let package = match cove_sema::package::load(&root, &mut sources) {
        Ok(package) => package,
        Err(items) => return Err(CliError::Diagnostics { sources, items }),
    };
    let mut program = match cove_sema::resolve::resolve(&package) {
        Ok(program) => program,
        Err(items) => return Err(CliError::Diagnostics { sources, items }),
    };

    // `cove check` type-checks, and `cove run` refuses to execute a package
    // that does not check. Type warnings join the resolver's, so `cove check`
    // reports and counts them the same way.
    let (errors, warnings): (Vec<Diagnostic>, Vec<Diagnostic>) =
        cove_sema::typeck::check(&package, &program)
            .into_iter()
            .partition(|d| d.severity == cove_diag::Severity::Error);
    if !errors.is_empty() {
        let mut items = errors;
        items.extend(warnings);
        return Err(CliError::Diagnostics { sources, items });
    }
    program.warnings.extend(warnings);

    Ok((sources, package, program))
}

/// Walks up from `start` to the nearest directory holding a `cove.toml`.
fn find_root(start: &Path) -> Option<PathBuf> {
    let mut dir: Option<&Path> = if start.is_dir() {
        Some(start)
    } else {
        start.parent()
    };
    while let Some(current) = dir {
        if current.join("cove.toml").is_file() {
            return Some(current.to_path_buf());
        }
        dir = current.parent();
    }
    None
}

/// Formats every `.cove` file the user asked for, or reports the ones that
/// are not formatted when `--check` is given.
fn cmd_fmt(args: &[String]) -> Result<(), CliError> {
    let mut check = false;
    let mut path: Option<&Path> = None;
    for arg in args {
        if arg == "--check" {
            check = true;
        } else {
            path = Some(Path::new(arg.as_str()));
        }
    }

    let mut sources = SourceMap::new();
    let mut diagnostics = Vec::new();
    let mut changed: Vec<PathBuf> = Vec::new();

    for target in fmt_targets(path)? {
        let text = std::fs::read_to_string(&target)
            .map_err(|e| CliError::Message(format!("cannot read `{}`: {e}", target.display())))?;
        let file = sources.add(&target, text.clone());
        // A file that does not parse is reported and never rewritten.
        let unit = match cove_syntax::parse_file(&sources, file) {
            Ok(unit) => unit,
            Err(items) => {
                diagnostics.extend(items);
                continue;
            }
        };
        let formatted = cove_syntax::format::format_source(&text, &unit);
        if formatted == text {
            continue;
        }
        if !check {
            std::fs::write(&target, &formatted).map_err(|e| {
                CliError::Message(format!("cannot write `{}`: {e}", target.display()))
            })?;
        }
        changed.push(target);
    }

    if !diagnostics.is_empty() {
        return Err(CliError::Diagnostics {
            sources,
            items: diagnostics,
        });
    }

    if !check {
        println!("{}", fmt_summary(changed.len()));
        return Ok(());
    }
    for path in &changed {
        println!("{}", path.display());
    }
    if changed.is_empty() {
        Ok(())
    } else {
        Err(CliError::Unformatted)
    }
}

/// The one-line summary `cove fmt` prints to stdout.
fn fmt_summary(changed: usize) -> String {
    format!("formatted {changed} file(s)")
}

/// The files `cove fmt` should consider: one named file, or every `.cove`
/// file in the package `path` sits in.
fn fmt_targets(path: Option<&Path>) -> Result<Vec<PathBuf>, CliError> {
    if let Some(path) = path {
        if path.is_file() {
            return Ok(vec![path.to_path_buf()]);
        }
    }
    let start = match path {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()
            .map_err(|e| CliError::Message(format!("cannot read the current directory: {e}")))?,
    };
    let root = find_root(&start).ok_or_else(|| {
        CliError::Message(format!(
            "no `cove.toml` found in `{}` or any parent directory",
            start.display()
        ))
    })?;
    let mut files = Vec::new();
    collect_cove_files(&root, &mut files);
    Ok(files)
}

/// Every `.cove` file of the package rooted at `dir`, in sorted order.
///
/// A subdirectory holding its own `cove.toml` is a nested package, so its
/// files belong to `cove fmt` run there, not here.
fn collect_cove_files(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| Some(entry.ok()?.path()))
        .collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        if path.is_dir() {
            if !path.join("cove.toml").is_file() {
                collect_cove_files(&path, found);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("cove") {
            found.push(path);
        }
    }
}

fn cmd_check(args: &[String]) -> Result<(), CliError> {
    let mut deny_warnings_flag = false;
    let mut path: Option<&Path> = None;
    for arg in args {
        if arg == "--deny-warnings" {
            deny_warnings_flag = true;
        } else {
            path = Some(Path::new(arg.as_str()));
        }
    }

    let (sources, package, program) = load(path)?;
    let modules = program.modules.len();
    let files: usize = package.modules.values().map(|m| m.units.len()).sum();
    for warning in &program.warnings {
        eprint!("{}", render(&sources, warning));
    }
    println!("{}", check_summary(modules, files, program.warnings.len()));

    // `--deny-warnings` and `cove.toml`'s `[check]` table only ever add
    // strictness, never relax it, so a run that asks for either denies
    // warnings: a CI invocation requesting stricter behavior always wins.
    let deny_warnings = deny_warnings_flag || package.config.check.deny_warnings;
    if deny_warnings && !program.warnings.is_empty() {
        return Err(CliError::WarningsDenied);
    }
    Ok(())
}

/// The one-line summary `cove check` prints to stdout.
fn check_summary(modules: usize, files: usize, warnings: usize) -> String {
    if warnings > 0 {
        format!("checked {modules} module(s), {files} file(s), {warnings} warning(s)")
    } else {
        format!("checked {modules} module(s), {files} file(s)")
    }
}

fn cmd_outline(path: Option<&Path>) -> Result<(), CliError> {
    let (sources, package, program) = load(path)?;
    print!("{}", render_outline(&sources, &package, &program));
    Ok(())
}

/// Renders every module's exported declarations, in the form the Language
/// Card promises `cove outline` derives from source: the typed public
/// interface, definition locations, and required capabilities.
fn render_outline(sources: &SourceMap, package: &Package, program: &Program) -> String {
    let mut out = String::new();
    for (name, resolved) in &program.modules {
        out.push_str(&format!("module {name}\n"));
        let blocks = module_blocks(sources, &package.root, package, resolved);
        for (i, block) in blocks.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(block);
        }
    }
    out
}

/// One rendered block per exported top-level declaration in `resolved`, in
/// the order they were declared in source.
fn module_blocks(
    sources: &SourceMap,
    root: &Path,
    package: &Package,
    resolved: &ResolvedModule,
) -> Vec<String> {
    const INDENT: usize = 2;
    let mut blocks = Vec::new();
    let Some(pkg_module) = package.modules.get(&resolved.name) else {
        return blocks;
    };
    for unit in &pkg_module.units {
        for item in &unit.ast.items {
            match &item.kind {
                ItemKind::Fn(decl) => {
                    if let Some(entry) = resolved.functions.get(&decl.name.node) {
                        if entry.exported {
                            blocks.push(render_fn_block(sources, root, entry, INDENT));
                        }
                    }
                }
                ItemKind::Struct(decl) => {
                    if let Some(entry) = resolved.structs.get(&decl.name.node) {
                        if entry.exported {
                            blocks.push(render_struct_block(
                                sources,
                                root,
                                &decl.name.node,
                                entry,
                                resolved,
                                INDENT,
                            ));
                        }
                    }
                }
                ItemKind::Enum(decl) => {
                    if let Some(entry) = resolved.enums.get(&decl.name.node) {
                        if entry.exported {
                            blocks.push(render_enum_block(
                                sources,
                                root,
                                &decl.name.node,
                                entry,
                                resolved,
                                INDENT,
                            ));
                        }
                    }
                }
                ItemKind::TypeAlias(decl) => {
                    if let Some(entry) = resolved.aliases.get(&decl.name.node) {
                        if entry.exported {
                            blocks.push(render_alias_block(sources, root, entry, INDENT));
                        }
                    }
                }
                ItemKind::Trait(decl) => {
                    if let Some(entry) = resolved.traits.get(&decl.name.node) {
                        if entry.exported {
                            blocks.push(render_trait_block(sources, root, entry, INDENT));
                        }
                    }
                }
                ItemKind::Impl(_) => {
                    // Exported methods and the conformances an `impl Trait
                    // for Type` block declares are rendered under their
                    // struct or enum's own block, wherever it appears.
                }
            }
        }
    }
    blocks
}

/// `path/to/file.cove`, relative to the package root, with `/` separators so
/// output is machine-independent.
fn rel_path(sources: &SourceMap, root: &Path, span: Span) -> String {
    let path = sources.path(span.file);
    let rel = path.strip_prefix(root).unwrap_or(path);
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// `  at path:line:col\n`, indented by `indent` spaces.
fn location_line(sources: &SourceMap, root: &Path, span: Span, indent: usize) -> String {
    let (line, col) = sources.get(span.file).line_col(span.start);
    format!(
        "{:indent$}at {}:{line}:{col}\n",
        "",
        rel_path(sources, root, span)
    )
}

/// `/// ` doc lines, indented by `indent` spaces, one per line of `doc`.
/// Prints nothing when `doc` is `None`.
fn doc_lines(doc: &Option<String>, indent: usize, out: &mut String) {
    let Some(doc) = doc else { return };
    for line in doc.lines() {
        out.push_str(&format!("{:indent$}/// {line}\n", ""));
    }
}

/// Renders a function or method's signature in the source form it would be
/// written in: `export [async] fn name[<T, U>](params) -> ReturnType`, with a
/// method's receiver as its first parameter.
fn fn_signature(entry: &FnEntry) -> String {
    let decl = &entry.decl;
    let mut sig = String::from("export ");
    if decl.is_async {
        sig.push_str("async ");
    }
    sig.push_str("fn ");
    sig.push_str(&decl.name.node);
    sig.push_str(&generics_suffix(&decl.generics));
    sig.push('(');
    let mut params: Vec<String> = Vec::new();
    if let Some(receiver) = &decl.receiver {
        params.push(if receiver.is_var { "var self" } else { "self" }.to_string());
    }
    params.extend(decl.params.iter().map(|p| p.to_string()));
    sig.push_str(&params.join(", "));
    sig.push(')');
    if let Some(return_type) = &decl.return_type {
        sig.push_str(" -> ");
        sig.push_str(&return_type.to_string());
    }
    sig
}

/// Renders a function or method's doc, signature, definition location, and
/// required capabilities.
fn render_fn_block(sources: &SourceMap, root: &Path, entry: &FnEntry, indent: usize) -> String {
    let mut out = String::new();
    doc_lines(&entry.doc, indent, &mut out);
    out.push_str(&format!("{:indent$}{}\n", "", fn_signature(entry)));
    out.push_str(&location_line(
        sources,
        root,
        entry.decl.name.span,
        indent + 2,
    ));
    if !entry.required_capabilities.is_empty() {
        let caps: Vec<String> = entry
            .required_capabilities
            .iter()
            .map(|c| c.to_string())
            .collect();
        out.push_str(&format!(
            "{:indent$}requires {}\n",
            "",
            caps.join(", "),
            indent = indent + 2
        ));
    }
    out
}

/// Renders a struct's doc, header, definition location, fields, and any
/// exported methods declared for it in an `impl` block.
fn render_struct_block(
    sources: &SourceMap,
    root: &Path,
    name: &str,
    entry: &StructEntry,
    resolved: &ResolvedModule,
    indent: usize,
) -> String {
    let mut out = String::new();
    doc_lines(&entry.doc, indent, &mut out);
    out.push_str(&format!(
        "{:indent$}export struct {name}{}\n",
        "",
        generics_suffix(&entry.decl.generics)
    ));
    out.push_str(&location_line(
        sources,
        root,
        entry.decl.name.span,
        indent + 2,
    ));
    for field in &entry.decl.fields {
        out.push_str(&format!(
            "{:indent$}{}: {}\n",
            "",
            field.name.node,
            field.ty,
            indent = indent + 2
        ));
    }
    out.push_str(&render_conformances(name, resolved, indent + 2));
    out.push_str(&render_methods(sources, root, name, resolved, indent + 2));
    out
}

/// Renders an enum's doc, header, definition location, cases with their
/// payload types, and any exported methods declared for it.
fn render_enum_block(
    sources: &SourceMap,
    root: &Path,
    name: &str,
    entry: &EnumEntry,
    resolved: &ResolvedModule,
    indent: usize,
) -> String {
    let mut out = String::new();
    doc_lines(&entry.doc, indent, &mut out);
    out.push_str(&format!(
        "{:indent$}export enum {name}{}\n",
        "",
        generics_suffix(&entry.decl.generics)
    ));
    out.push_str(&location_line(
        sources,
        root,
        entry.decl.name.span,
        indent + 2,
    ));
    for case in &entry.decl.cases {
        if case.payload.is_empty() {
            out.push_str(&format!(
                "{:indent$}{}\n",
                "",
                case.name.node,
                indent = indent + 2
            ));
        } else {
            let payload: Vec<String> = case.payload.iter().map(|ty| ty.to_string()).collect();
            out.push_str(&format!(
                "{:indent$}{}({})\n",
                "",
                case.name.node,
                payload.join(", "),
                indent = indent + 2
            ));
        }
    }
    out.push_str(&render_methods(sources, root, name, resolved, indent + 2));
    out
}

/// Renders a trait's doc, header, definition location, and the signature of
/// each method it declares.
///
/// A trait's implementors are part of the derived interface too, so each
/// type's own block names the traits it conforms to.
fn render_trait_block(
    sources: &SourceMap,
    root: &Path,
    entry: &TraitEntry,
    indent: usize,
) -> String {
    let mut out = String::new();
    doc_lines(&entry.doc, indent, &mut out);
    out.push_str(&format!(
        "{:indent$}export trait {}\n",
        "", entry.decl.name.node
    ));
    out.push_str(&location_line(
        sources,
        root,
        entry.decl.name.span,
        indent + 2,
    ));
    for method in &entry.decl.methods {
        let mut params: Vec<String> = Vec::new();
        if let Some(receiver) = method.receiver {
            params.push(if receiver.is_var { "var self" } else { "self" }.to_string());
        }
        params.extend(method.params.iter().map(ToString::to_string));
        let ret = match &method.return_type {
            Some(ty) => format!(" -> {ty}"),
            None => String::new(),
        };
        let default = if method.default.is_some() {
            " (default)"
        } else {
            ""
        };
        out.push_str(&format!(
            "{:indent$}{}fn {}({}){ret}{default}\n",
            "",
            if method.is_async { "async " } else { "" },
            method.name.node,
            params.join(", "),
            indent = indent + 2
        ));
    }
    out
}

/// Every trait the type named `type_name` conforms to, in trait-name order.
fn render_conformances(type_name: &str, resolved: &ResolvedModule, indent: usize) -> String {
    let mut out = String::new();
    for (trait_name, owner) in resolved.conformances.keys() {
        if owner == type_name {
            out.push_str(&format!(
                "{:indent$}conforms to {trait_name}\n",
                "",
                indent = indent
            ));
        }
    }
    out
}

/// Renders a type alias's doc, header, and definition location.
fn render_alias_block(
    sources: &SourceMap,
    root: &Path,
    entry: &AliasEntry,
    indent: usize,
) -> String {
    let mut out = String::new();
    doc_lines(&entry.doc, indent, &mut out);
    out.push_str(&format!(
        "{:indent$}export type {}{} = {}\n",
        "",
        entry.decl.name.node,
        generics_suffix(&entry.decl.generics),
        entry.decl.ty
    ));
    out.push_str(&location_line(
        sources,
        root,
        entry.decl.name.span,
        indent + 2,
    ));
    out
}

/// Every exported method declared for the type named `type_name`, in the
/// module's method order.
fn render_methods(
    sources: &SourceMap,
    root: &Path,
    type_name: &str,
    resolved: &ResolvedModule,
    indent: usize,
) -> String {
    let mut out = String::new();
    for ((owner, _), method) in &resolved.methods {
        if owner == type_name && method.exported {
            out.push_str(&render_fn_block(sources, root, method, indent));
        }
    }
    out
}

/// `<T, U: Display>`, or an empty string when there are no generic
/// parameters.
fn generics_suffix(generics: &[cove_syntax::ast::GenericParam]) -> String {
    if generics.is_empty() {
        return String::new();
    }
    let names: Vec<String> = generics
        .iter()
        .map(cove_syntax::ast::GenericParam::to_string)
        .collect();
    format!("<{}>", names.join(", "))
}

fn cmd_run(args: &[String]) -> Result<(), CliError> {
    let Some(name) = args.first() else {
        return Err(CliError::Message(
            "`cove run` needs the name of a `[run.<name>]` table in cove.toml".into(),
        ));
    };

    let (sources, package, program) = load(None)?;
    let Some(run) = package.config.runs.get(name.as_str()) else {
        let known: Vec<&str> = package.config.runs.keys().map(String::as_str).collect();
        return Err(CliError::Message(format!(
            "cove.toml has no `[run.{name}]` table\n  known runs: {}",
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            }
        )));
    };
    let Some((module, entry)) = run.entry_parts() else {
        return Err(CliError::Message(format!(
            "`[run.{name}] entry` must be a qualified function such as `hello.main`, found `{}`",
            run.entry
        )));
    };
    if program.lookup_fn(module, entry).is_none() {
        return Err(CliError::Message(format!(
            "`[run.{name}] entry` refers to `{}`, which this package does not declare",
            run.entry
        )));
    }

    let flags = parse_run_flags(&args[1..])?;

    let mut hosts = HostRegistry::new(Grants::new(run.allow.clone()));
    hosts.register(Box::new(Console::new(std::io::stdout())));
    hosts.register(Box::new(Env::from_process()));
    hosts.register(Box::new(Documents::rooted(package.root.join("documents"))));
    // Registering a module does not grant it: `HostRegistry::call` rejects
    // every call whose capability is missing from `[run.<name>] allow`.
    hosts.register(Box::new(Clock::real()));

    let limits = Limits {
        fuel: flags.fuel.or(run.fuel),
        deadline: flags.deadline.or(run.deadline),
        max_host_calls: flags.max_host_calls.or(run.max_host_calls),
        max_call_depth: None,
    };
    // The interpreter checks this budget at its own safepoints (loop back
    // edges, calls, `await`); see `RunFlags` below for why the handle is not
    // yet wired to SIGINT.
    let cancellation = Cancellation::new();
    let budget = Budget::with_cancellation(limits, cancellation);

    let trace_target = flags
        .trace
        .or_else(|| run.trace.as_deref().map(TraceTarget::from_flag));
    let wait_total = WaitTotal::default();
    let primary_sink: Box<dyn TraceSink> = match &trace_target {
        Some(TraceTarget::Stderr) => Box::new(JsonlSink::new(std::io::stderr())),
        Some(TraceTarget::File(path)) => {
            let file = std::fs::File::create(path).map_err(|e| {
                CliError::Message(format!(
                    "cannot create trace file `{}`: {e}",
                    path.display()
                ))
            })?;
            Box::new(JsonlSink::new(file))
        }
        None => Box::new(NullSink),
    };
    // `HostRegistry::call` and the interpreter's own task and entry events
    // both need to reach the one trace destination `--trace` selected. A
    // `SharedSink` lets each hold a handle to that one destination, rather
    // than each opening or wrapping it separately — two independent sinks
    // writing the same file would race for it.
    let sink = SharedSink::new(Box::new(CompositeSink {
        primary: primary_sink,
        wait_total: wait_total.clone(),
    }));
    hosts.set_trace(Box::new(sink.clone()));
    hosts.set_budget(budget);

    let program_args: Vec<Rc<str>> = flags
        .program_args
        .iter()
        .map(|a| a.as_str().into())
        .collect();

    let mut interpreter = Interpreter::new(&program, &sources, &mut hosts);
    interpreter.set_trace(Box::new(sink));
    let outcome = interpreter.run_entry(module, entry, program_args);

    if flags.stats {
        print_stats(&hosts, &wait_total);
    }

    match outcome {
        Ok(value) => report_exit(value),
        Err(error) => Err(CliError::Diagnostics {
            sources,
            items: vec![error.to_diagnostic()],
        }),
    }
}

/// A `--fuel`, `--deadline`, `--max-host-calls`, `--trace`, or `--stats` flag
/// to `cove run`, parsed from anywhere after the run name.
///
/// This is deliberately not hooked up: Rust's standard library has no signal
/// handling API, so installing a SIGINT handler would need a crate (such as
/// `signal-hook` or `ctrlc`) or unsafe, platform-specific `extern "C"` FFI
/// duplicating one. Neither fits "if it is not straightforward with std
/// alone, skip it," so `cove run` cannot yet be interrupted through
/// `Cancellation` from outside; only the limits below can stop a run.
struct RunFlags {
    fuel: Option<u64>,
    deadline: Option<Duration>,
    max_host_calls: Option<u64>,
    trace: Option<TraceTarget>,
    stats: bool,
    program_args: Vec<String>,
}

/// Where `--trace` (or the config's `trace` key) sends trace lines.
enum TraceTarget {
    Stderr,
    File(PathBuf),
}

impl TraceTarget {
    fn from_flag(value: &str) -> TraceTarget {
        if value == "-" {
            TraceTarget::Stderr
        } else {
            TraceTarget::File(PathBuf::from(value))
        }
    }
}

/// Parses the flags and program arguments following `cove run <name>`.
///
/// Flags may appear in any position; everything after a literal `--` is a
/// program argument even if it looks like a flag, and anything not
/// recognized as a flag is a program argument too, so `cove run <name> <arg>`
/// keeps working exactly as it always has.
fn parse_run_flags(args: &[String]) -> Result<RunFlags, CliError> {
    let mut flags = RunFlags {
        fuel: None,
        deadline: None,
        max_host_calls: None,
        trace: None,
        stats: false,
        program_args: Vec::new(),
    };
    let mut passthrough = false;
    let mut i = 0;
    while i < args.len() {
        if passthrough {
            flags.program_args.push(args[i].clone());
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--" => passthrough = true,
            "--fuel" => {
                let value = flag_value(args, &mut i, "--fuel")?;
                flags.fuel = Some(value.parse().map_err(|_| {
                    CliError::Message(format!(
                        "`--fuel` must be a non-negative integer, found `{value}`"
                    ))
                })?);
            }
            "--deadline" => {
                let value = flag_value(args, &mut i, "--deadline")?;
                flags.deadline = Some(
                    parse_duration_flag(&value)
                        .map_err(|e| CliError::Message(format!("`--deadline`: {e}")))?,
                );
            }
            "--max-host-calls" => {
                let value = flag_value(args, &mut i, "--max-host-calls")?;
                flags.max_host_calls = Some(value.parse().map_err(|_| {
                    CliError::Message(format!(
                        "`--max-host-calls` must be a non-negative integer, found `{value}`"
                    ))
                })?);
            }
            "--trace" => {
                let value = flag_value(args, &mut i, "--trace")?;
                flags.trace = Some(TraceTarget::from_flag(&value));
            }
            "--stats" => flags.stats = true,
            other => flags.program_args.push(other.to_string()),
        }
        i += 1;
    }
    Ok(flags)
}

/// Consumes and returns the value following the flag at `args[*i]`,
/// advancing `*i` to point at it so the caller's loop increment lands on the
/// next unconsumed argument.
fn flag_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, CliError> {
    let value = args
        .get(*i + 1)
        .ok_or_else(|| CliError::Message(format!("`{flag}` needs a value")))?;
    *i += 1;
    Ok(value.clone())
}

/// Parses a `--deadline` value such as `"500ms"`, using the same unit
/// meanings as `cove.toml`'s `deadline` key and the lexer's duration
/// literals: `ns`, `us`, `ms`, `s`, `m`, and `h`.
fn parse_duration_flag(text: &str) -> Result<Duration, String> {
    let accepted = "the accepted units are `ns`, `us`, `ms`, `s`, `m`, and `h`";
    let invalid = || format!("`{text}` is not a valid duration; {accepted}");

    let split_at = text
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(invalid)?;
    let (digits, unit) = text.split_at(split_at);
    if digits.is_empty() {
        return Err(invalid());
    }
    let value: u64 = digits.parse().map_err(|_| invalid())?;

    let nanos_per_unit: u64 = match unit {
        "ns" => 1,
        "us" => 1_000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60_000_000_000,
        "h" => 3_600_000_000_000,
        _ => return Err(invalid()),
    };
    let nanos = value
        .checked_mul(nanos_per_unit)
        .ok_or_else(|| format!("`{text}` overflows a 64-bit nanosecond count"))?;
    Ok(Duration::from_nanos(nanos))
}

/// Total host-call wait time, shared with the trace sink that measures it,
/// so `--stats` can report it even when no trace file was requested.
#[derive(Clone, Default)]
struct WaitTotal(Rc<RefCell<Duration>>);

impl WaitTotal {
    fn get(&self) -> Duration {
        *self.0.borrow()
    }
}

impl TraceSink for WaitTotal {
    fn record(&mut self, event: TraceEvent) {
        if let TraceEvent::HostCall { wait, .. } = event {
            *self.0.borrow_mut() += wait;
        }
    }
}

/// Lets two independent owners — the `HostRegistry` and the `Interpreter` —
/// each hold a handle to the one real trace destination.
///
/// `HostRegistry::call` traces `HostCall`, and the interpreter traces task
/// and entry events; both need to land in the same JSONL stream, in the
/// order the single-threaded run produced them. Cloning shares the same
/// underlying sink rather than opening or wrapping the destination twice.
#[derive(Clone)]
struct SharedSink(Rc<RefCell<Box<dyn TraceSink>>>);

impl SharedSink {
    fn new(sink: Box<dyn TraceSink>) -> Self {
        SharedSink(Rc::new(RefCell::new(sink)))
    }
}

impl TraceSink for SharedSink {
    fn record(&mut self, event: TraceEvent) {
        self.0.borrow_mut().record(event);
    }
}

/// Forwards every event to both the sink `--trace` selected and the
/// `WaitTotal` accumulator `--stats` reads from, so tracing and stats
/// reporting compose instead of competing for the one sink slot.
struct CompositeSink {
    primary: Box<dyn TraceSink>,
    wait_total: WaitTotal,
}

impl TraceSink for CompositeSink {
    fn record(&mut self, event: TraceEvent) {
        self.wait_total.record(event.clone());
        self.primary.record(event);
    }
}

/// Prints fuel spent, host calls, elapsed time, and host-call wait to
/// stderr, for `--stats`.
fn print_stats(hosts: &HostRegistry, wait_total: &WaitTotal) {
    if let Some(budget) = hosts.budget() {
        eprintln!(
            "stats: fuel_spent={} host_calls={} elapsed={:?} wait={:?}",
            budget.fuel_spent(),
            budget.host_calls(),
            budget.elapsed(),
            wait_total.get(),
        );
    }
}

/// An entry returning `Err(...)` fails the run and prints the error.
fn report_exit(value: cove_runtime::Value) -> Result<(), CliError> {
    use cove_runtime::value::Value;
    if let Value::Enum(result) = &value {
        if &*result.type_name == "Result" && &*result.case == "Err" {
            let payload = result
                .payload
                .first()
                .map(ToString::to_string)
                .unwrap_or_default();
            return Err(CliError::Message(payload));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cove_sema::package::load;
    use cove_sema::resolve::resolve;

    /// A package written to a real temporary directory, so relative paths
    /// and `SourceMap::path` behave exactly as they do for a package loaded
    /// from disk by the CLI.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "cove-cli-test-{name}-{}-{}",
                std::process::id(),
                nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// Loads and resolves the package rooted at `root`.
    fn load_fixture(root: &Path) -> (SourceMap, Package, Program) {
        let mut sources = SourceMap::new();
        let package = load(root, &mut sources).expect("fixture package loads");
        let program = resolve(&package).expect("fixture package resolves");
        (sources, package, program)
    }

    #[test]
    fn fmt_check_reports_an_unformatted_file_and_formatting_makes_it_clean() {
        let dir = TempDir::new("fmt");
        write(dir.path(), "cove.toml", "");
        let unformatted = "\
use console.println
/// Runs.
export fn main() -> Result<Unit,Error> {
        Ok(())
}
";
        let formatted = "\
use console.println

/// Runs.
export fn main() -> Result<Unit, Error> {
  Ok(())
}
";
        write(dir.path(), "app/main.cove", unformatted);
        let source = dir.path().join("app/main.cove");
        let path = dir.path().display().to_string();

        let Err(error) = cmd_fmt(&["--check".into(), path.clone()]) else {
            panic!("an unformatted file must fail `--check`");
        };
        assert!(matches!(error, CliError::Unformatted));
        assert_eq!(
            std::fs::read_to_string(&source).unwrap(),
            unformatted,
            "`--check` must not rewrite anything"
        );

        assert!(
            cmd_fmt(std::slice::from_ref(&path)).is_ok(),
            "formatting succeeds"
        );
        assert_eq!(std::fs::read_to_string(&source).unwrap(), formatted);

        assert!(
            cmd_fmt(&["--check".into(), path]).is_ok(),
            "a formatted package passes `--check`"
        );
    }

    #[test]
    fn fmt_reports_a_file_that_does_not_parse_and_leaves_it_alone() {
        let dir = TempDir::new("fmt-broken");
        write(dir.path(), "cove.toml", "");
        let broken = "fn main() {\n  let x = ;\n}\n";
        write(dir.path(), "app/main.cove", broken);
        let source = dir.path().join("app/main.cove");

        let Err(error) = cmd_fmt(&[dir.path().display().to_string()]) else {
            panic!("a file that does not parse must be an error");
        };
        assert!(matches!(error, CliError::Diagnostics { .. }));
        assert_eq!(std::fs::read_to_string(&source).unwrap(), broken);
    }

    #[test]
    fn fmt_summary_counts_the_files_it_rewrote() {
        assert_eq!(fmt_summary(0), "formatted 0 file(s)");
        assert_eq!(fmt_summary(3), "formatted 3 file(s)");
    }

    #[test]
    fn outline_matches_hello_and_config_in_the_real_examples_package() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let (sources, package, program) = load_fixture(&root);
        let out = render_outline(&sources, &package, &program);

        let expected_hello = "\
module hello
  /// Returns a greeting for `name`.
  export fn greeting(name: String) -> String
    at hello/main.cove:4:11

  /// Runs the command-line program.
  export fn main(args: Array<String>) -> Result<Unit, Error>
    at hello/main.cove:9:11
    requires console
";
        assert!(
            out.contains(&format!("{expected_hello}module restricted\n")),
            "unexpected outline:\n{out}"
        );

        let expected_config = "\
module config
  /// Validated application configuration.
  export struct Config
    at config/load.cove:4:15
    port: Int
    logLevel: LogLevel

  /// Supported logging levels.
  export enum LogLevel
    at config/load.cove:10:13
    Debug
    Info
    Warn
    Error

  /// Configuration validation failures.
  export enum ConfigError
    at config/load.cove:18:13
    InvalidPort(String)
    InvalidLogLevel(String)

  /// Loads configuration from the host environment.
  export fn loadConfig() -> Result<Config, ConfigError>
    at config/load.cove:24:11
    requires env
";
        assert!(
            out.contains(&format!("{expected_config}module hello\n")),
            "unexpected outline:\n{out}"
        );
    }

    /// A fixture module exercising shapes the real `examples/` package does
    /// not: an exported `impl` method, a generic function, an `async fn`, a
    /// type alias, a function with no return type, and a function requiring
    /// two capabilities. Paired with a second module that exports nothing.
    fn write_kitchen_fixture(root: &Path) {
        write(root, "cove.toml", "");
        write(
            root,
            "kitchen/main.cove",
            "\
use console.println
use clock.now

/// A widget with an exported accessor.
export struct Widget {
  id: String
}

impl Widget {
  /// Returns the widget's id.
  export fn describe(self) -> String {
    self.id
  }
}

/// Returns its argument unchanged.
export fn identity<T>(value: T) -> T {
  value
}

/// Greets asynchronously.
export async fn greetAsync(name: String) -> String {
  name
}

/// A callback invoked with an Int and returning nothing.
export type Callback = async fn(value: Int) -> Unit

/// Logs a message; has no return type.
export fn log(message: String) {
  console.println(message)
}

/// Logs a message with the current time, needing two capabilities.
export fn logWithTime(message: String) {
  console.println(\"{clock.now()}: {message}\")
}
",
        );
        write(
            root,
            "private/main.cove",
            "\
/// Internal helper, never exported.
fn helper() -> Int {
  1
}
",
        );
    }

    #[test]
    fn outline_renders_every_derived_shape_and_hides_private_modules() {
        let dir = TempDir::new("kitchen");
        write_kitchen_fixture(dir.path());
        let (sources, package, program) = load_fixture(dir.path());
        let out = render_outline(&sources, &package, &program);

        let expected = "\
module kitchen
  /// A widget with an exported accessor.
  export struct Widget
    at kitchen/main.cove:5:15
    id: String
    /// Returns the widget's id.
    export fn describe(self) -> String
      at kitchen/main.cove:11:13

  /// Returns its argument unchanged.
  export fn identity<T>(value: T) -> T
    at kitchen/main.cove:17:11

  /// Greets asynchronously.
  export async fn greetAsync(name: String) -> String
    at kitchen/main.cove:22:17

  /// A callback invoked with an Int and returning nothing.
  export type Callback = async fn(value: Int) -> Unit
    at kitchen/main.cove:27:13

  /// Logs a message; has no return type.
  export fn log(message: String)
    at kitchen/main.cove:30:11
    requires console

  /// Logs a message with the current time, needing two capabilities.
  export fn logWithTime(message: String)
    at kitchen/main.cove:35:11
    requires clock, console
module private
";
        assert_eq!(out, expected);
    }

    #[test]
    fn check_summary_omits_warning_count_when_there_are_none() {
        assert_eq!(check_summary(7, 7, 0), "checked 7 module(s), 7 file(s)");
    }

    #[test]
    fn check_summary_mentions_warning_count_when_present() {
        assert_eq!(
            check_summary(7, 7, 3),
            "checked 7 module(s), 7 file(s), 3 warning(s)"
        );
    }

    #[test]
    fn program_warnings_feed_the_check_summary() {
        let dir = TempDir::new("undocumented");
        write(dir.path(), "cove.toml", "");
        write(
            dir.path(),
            "bare/main.cove",
            "\
export fn a() -> Int {
  1
}

export fn b() -> Int {
  2
}

export fn main() -> Result<Unit, Error> {
  Ok(())
}
",
        );
        let (_, _, program) = load_fixture(dir.path());
        assert_eq!(program.warnings.len(), 3);
        assert_eq!(
            check_summary(1, 1, program.warnings.len()),
            "checked 1 module(s), 1 file(s), 3 warning(s)"
        );
    }

    #[test]
    fn check_deny_warnings_flag_denies_even_with_no_config_key() {
        let dir = TempDir::new("deny-flag");
        write(dir.path(), "cove.toml", "");
        write(
            dir.path(),
            "bare/main.cove",
            "export fn a() -> Int {\n  1\n}\n",
        );

        let path = dir.path().display().to_string();
        let result = cmd_check(&[path, "--deny-warnings".to_string()]);
        assert!(matches!(result, Err(CliError::WarningsDenied)));
    }

    #[test]
    fn check_config_deny_warnings_denies_without_the_flag() {
        let dir = TempDir::new("deny-config");
        write(dir.path(), "cove.toml", "[check]\ndeny_warnings = true\n");
        write(
            dir.path(),
            "bare/main.cove",
            "export fn a() -> Int {\n  1\n}\n",
        );

        let path = dir.path().display().to_string();
        let result = cmd_check(&[path]);
        assert!(matches!(result, Err(CliError::WarningsDenied)));
    }

    #[test]
    fn check_without_flag_or_config_key_does_not_deny() {
        let dir = TempDir::new("deny-neither");
        write(dir.path(), "cove.toml", "");
        write(
            dir.path(),
            "bare/main.cove",
            "export fn a() -> Int {\n  1\n}\n",
        );

        let path = dir.path().display().to_string();
        let result = cmd_check(&[path]);
        assert!(result.is_ok());
    }
}
