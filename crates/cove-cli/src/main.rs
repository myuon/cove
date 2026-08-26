//! The `cove` command-line tool.
//!
//! The CLI does not invent semantics: the compiler derives facts, the runtime
//! enforces and records them, and the CLI explains them.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cove_diag::{render, Diagnostic, SourceMap, Span};
use cove_runtime::embed::{register_hosts, HostSetup};
use cove_runtime::host::HostRegistry;
use cove_runtime::interp::Interpreter;
use cove_runtime::{
    create_trace_file, Budget, Cancellation, HeapStats, JsonlSink, Limits, NullSink, Runtime,
    TraceEvent, TraceHeader, TraceSink, ValueCapture,
};
use cove_sema::config::RunConfig;
use cove_sema::package::Package;
use cove_sema::resolve::{
    AliasEntry, EnumEntry, FnEntry, Program, ResolvedModule, StructEntry, TraitEntry,
};
use cove_syntax::ast::ItemKind;

mod api;
mod build;
#[cfg(test)]
mod fixture;
mod generate;
mod impact;
mod json;
mod replay;
mod test;
mod trace;

const USAGE: &str = "\
cove — the Cove toolchain

usage:
  cove fmt [path] [--check]            format every `.cove` file in the package
  cove check [path] [--deny-warnings]  parse, resolve, and type-check the package
  cove run <name> [flags] [args]       run the entry selected by `[run.<name>]` in cove.toml
  cove build <name> [--out <path>]     package that run as a native executable
  cove generate <name>                 run <name>'s entry and write its source to `generates`
  cove generate --check                fail if any `generates` file is stale
  cove test [path] [--filter <sub>]    run every `test fn` the package declares
  cove outline [path]                  show modules and their exported declarations
  cove api snapshot [path]             record the package's derived public interface
  cove api diff [path]                 compare the source against a recorded interface
  cove impact [path] <name>            explain what a change to <name> can affect
  cove trace <file> [--capability <c>] [--task <id>]
                                       summarise and list a recorded trace
  cove replay <file> <name>            re-run <name>, answering every host call from <file>
  cove help                            show this message

`cove fmt` rewrites files in place and prints how many changed. `--check`
writes nothing, prints the path of every file that would change, and exits
non-zero when there is one, which is the form to run in CI. A file that does
not parse is reported and never rewritten.

`--deny-warnings` fails `cove check` when the package has any warnings, as
does setting `deny_warnings = true` in `cove.toml`'s `[check]` table; either
one is enough to deny.

`cove build` writes a native executable that embeds the program and the
runtime, so what it produces runs with no toolchain, no `cove` on the path,
and no source tree. It is not a code generator: the binary interprets the
same program `cove run` does. Its entry, its granted capabilities, and its
limits are the ones `[run.<name>]` recorded when it was built, and a
`cove.toml` placed beside it grants it nothing. Building one needs `cargo`
and this toolchain's own source, because an executable has to link the
runtime; `cove build --help` says so in full, and ADR 0009 says why.

`cove test` runs every `test fn` in the package, reports each one, and exits
non-zero when any failed. `--filter` runs only the tests whose qualified name
contains the given substring. Each test is granted exactly the capabilities
its call graph requires, with each host's fake implementation, so a suite is
deterministic; `cove.toml`'s `[test] allow_real = [...]` names the
capabilities to grant for real instead.

`cove api snapshot` derives the package's public interface from source and
writes it to `cove-api.txt` at the package root, or to `--out <file>`. Check
that file in: `cove api diff` compares it — or the file `--against <file>`
names — against the interface the current source derives, and classifies
every difference as breaking or compatible, exiting non-zero on a breaking
one so CI can use it. The recorded interface hash covers everything but the
doc comments.

`cove impact <name>` reports what a change to a function or method can
affect: what calls it transitively, which modules those live in, which
`[run.<name>]` entries reach it, and whether it needs authority an entry
does not grant. Name it as `name`, `module.name`, or `module.Type.method`.
A caller marked `(approximate)` is reached only through a call whose
receiver type the compiler cannot narrow yet.

`cove generate <name>` runs `[run.<name>]`'s entry under the capabilities and
budgets that table grants, exactly as `cove run` does, except the entry must
return `Result<String, Error>`. The source it returns is written to the
package-relative path `generates` names, formatted, and the package is then
checked; a generator whose output does not parse fails pointing at that file.
The written file carries a header marking it generated and naming the run
that made it. `cove build`, `cove run`, `cove check`, and `cove test` never
generate: `cove generate` is the only command that runs project code besides
an explicit `run`. `cove generate --check` regenerates every run that sets
`generates` into memory, compares it against what is on disk, and exits
non-zero on the first file that differs, which is the form to run in CI.

`cove trace` reads a JSONL trace written by `cove run --trace` and prints a
summary and a timeline. It reports which of the distinctions ADR 0001 asks a
trace to make the events actually carry, and which they do not.

`cove replay <file> <name>` runs `[run.<name>]`'s entry again with every host
replaced by one that answers from `<file>`, in the recorded order. The
program's own computation runs for real; only the Host API boundary is canned,
so no host is called and nothing outside the process changes. A replay exits
non-zero when it diverges: the program asked for a call the trace does not
have, asked for a different one, or stopped before using them all.

`cove run` flags (may appear in any position after <name>; everything after a
literal `--` is a program argument, even if it looks like a flag):
  --fuel <n>            stop the run after <n> fuel is spent
  --deadline <duration>  stop the run after <duration> has elapsed, e.g. `500ms`, `5s`, `1h`
  --max-host-calls <n>  stop the run after <n> host calls
  --trace <path>        write a JSONL trace to <path>, or `-` for stderr
  --trace-values <mode> `full` (the default) records each host call's arguments and result, which is what `cove replay` needs; `redacted` records only their types
  --max-tasks <n>       stop the run when it would hold more than <n> tasks at once
  --stats               print fuel spent, host calls, irreversible writes, elapsed time, host-call wait, and the heap to stderr
  --files-root <path>   the one directory the `files` host may reach; defaults to `files/` in the package
  --allow-exec <path>   an absolute path `process.run` may start; repeat to allow more, and omit to allow none
";

fn main() -> ExitCode {
    // Every command runs on a thread the runtime sized, because the process
    // main thread's stack is the platform's business — 8 MiB here, 1 MiB on
    // Windows — and a tree walker that recurses on it holds its depth limit
    // only by luck. This is the whole of the CLI rather than only the
    // commands that run a program: the parser, the resolver and the type
    // checker all recurse over the same nesting a program may contain, and
    // one place that is true is easier to keep true than five.
    match cove_runtime::on_cove_stack(dispatch) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: cove could not start the thread it runs on: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Reads the command out of the process arguments, runs it, and turns what it
/// answered into an exit code.
fn dispatch() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("help");

    let result = match command {
        "fmt" => cmd_fmt(&args[1..]),
        "check" => cmd_check(&args[1..]),
        "run" => cmd_run(&args[1..]),
        "build" => build::cmd_build(&args[1..]),
        "generate" => generate::cmd_generate(&args[1..]),
        "test" => test::cmd_test(&args[1..]),
        "outline" => cmd_outline(args.get(1).map(Path::new)),
        "api" => api::cmd_api(&args[1..]),
        "impact" => impact::cmd_impact(&args[1..]),
        "trace" => trace::cmd_trace(&args[1..]),
        "replay" => replay::cmd_replay(&args[1..]),
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
        Err(CliError::WarningsDenied)
        | Err(CliError::Unformatted)
        | Err(CliError::BreakingChange)
        | Err(CliError::TestsFailed)
        | Err(CliError::Diverged)
        | Err(CliError::GenerateStale) => ExitCode::FAILURE,
    }
}

pub(crate) enum CliError {
    Message(String),
    Diagnostics {
        /// Shared because a run holds the same source map for as long as it
        /// lasts, and a diagnostic points into it afterwards.
        sources: Arc<SourceMap>,
        items: Vec<Diagnostic>,
    },
    /// `cove check --deny-warnings` found warnings. The warnings and summary
    /// were already printed, so there is nothing left to say.
    WarningsDenied,
    /// `cove fmt --check` found files that are not formatted. Their paths
    /// were already printed, so there is nothing left to say.
    Unformatted,
    /// `cove api diff` found a breaking change. The classified changes were
    /// already printed, so there is nothing left to say.
    BreakingChange,
    /// `cove test` had a test fail. Each failure and the summary were
    /// already printed, so there is nothing left to say.
    TestsFailed,
    /// `cove replay` could not reproduce the recorded run. The divergence
    /// report was already printed, so there is nothing left to say.
    Diverged,
    /// `cove generate --check` found a `generates` file that would change if
    /// regenerated. Which files and how to fix them were already printed, so
    /// there is nothing left to say.
    GenerateStale,
}

impl From<String> for CliError {
    fn from(message: String) -> Self {
        CliError::Message(message)
    }
}

/// Loads and resolves the package containing `start`.
pub(crate) fn load(start: Option<&Path>) -> Result<(SourceMap, Package, Program), CliError> {
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
        Err(items) => {
            return Err(CliError::Diagnostics {
                sources: sources.into(),
                items,
            })
        }
    };
    // `cove check` type-checks, and `cove run` refuses to execute a package
    // that does not check. Type warnings and notes join the resolver's
    // warnings in `Program::notices`, so `cove check` reports them the same
    // way; it counts them apart, because only a warning is a doubt
    // `--deny-warnings` acts on.
    //
    // Only `cove check` prints them. A command that runs a program reports
    // what stops it and nothing else — that was already true of the
    // resolver's warnings, and notes join them on the same footing. Reading
    // out what the checker chose not to prove is what `cove check` is for.
    //
    // A `cove` command reads the shipped Host API schemas and no others: a
    // package on disk names no embedder, and `Compiler` is where a schema
    // would be added if a package ever gained a way to declare one.
    let program = match cove_sema::Compiler::new().compile(&package) {
        Ok(program) => program,
        Err(items) => {
            return Err(CliError::Diagnostics {
                sources: sources.into(),
                items,
            })
        }
    };

    Ok((sources, package, program))
}

/// Walks up from `start` to the nearest directory holding a `cove.toml`.
pub(crate) fn find_root(start: &Path) -> Option<PathBuf> {
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

    // A file that does not parse cannot be formatted, and saying whether
    // source is valid is `cove check`'s job, not this one. Report it and
    // carry on: the exit status reflects formatting, so a broken file cannot
    // hide an unformatted one, and `cove check` still refuses the package.
    for diagnostic in &diagnostics {
        eprint!("{}", render(&sources, diagnostic));
    }
    if !diagnostics.is_empty() {
        eprintln!("{}", skipped_summary(diagnostics.len()));
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

/// The one-line note `cove fmt` prints when it could not parse a file.
fn skipped_summary(skipped: usize) -> String {
    format!("skipped {skipped} file(s) that do not parse; run `cove check`")
}

/// The files `cove fmt` should consider: one named file, every `.cove` file
/// in the package `path` sits in, or -- when there is no package -- every
/// `.cove` file below `path`.
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
    // Formatting is syntactic, so it does not need a package. When there is
    // one, format it, and leave a nested package to a `cove fmt` run there.
    // When there is not -- a directory of packages, such as a repository root
    // -- format everything below, nested packages included, because that is
    // what naming the directory asked for.
    let mut files = Vec::new();
    match find_root(&start) {
        Some(root) => collect_cove_files(&root, true, &mut files),
        None => collect_cove_files(&start, false, &mut files),
    }
    Ok(files)
}

/// Every `.cove` file below `dir`, in sorted order.
///
/// With `stop_at_nested_package`, a subdirectory holding its own `cove.toml`
/// is a package of its own and its files belong to a `cove fmt` run there.
fn collect_cove_files(dir: &Path, stop_at_nested_package: bool, found: &mut Vec<PathBuf>) {
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
            if !(stop_at_nested_package && path.join("cove.toml").is_file()) {
                collect_cove_files(&path, stop_at_nested_package, found);
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
    for diagnostic in &program.notices {
        eprint!("{}", render(&sources, diagnostic));
    }
    // Each count filters on the exact severity. Subtracting one from the
    // length would give the same answer only for as long as `notices` holds
    // nothing else, and what a third severity would then produce is a wrong
    // number rather than a compiler error.
    let count = |severity| {
        program
            .notices
            .iter()
            .filter(|d| d.severity == severity)
            .count()
    };
    let warnings = count(cove_diag::Severity::Warning);
    let notes = count(cove_diag::Severity::Note);
    println!("{}", check_summary(modules, files, warnings, notes));

    // `--deny-warnings` and `cove.toml`'s `[check]` table only ever add
    // strictness, never relax it, so a run that asks for either denies
    // warnings: a CI invocation requesting stricter behavior always wins.
    // Notes are not denied by either, because a note is not a doubt about
    // the program: it is the checker naming something it deliberately did
    // not prove, and no strictness setting can make it prove one.
    let deny_warnings = deny_warnings_flag || package.config.check.deny_warnings;
    if deny_warnings && warnings > 0 {
        return Err(CliError::WarningsDenied);
    }
    Ok(())
}

/// The one-line summary `cove check` prints to stdout.
///
/// Warnings and notes are counted apart because they ask for different
/// things: a warning is something to fix, and a note is a place the checker
/// says it proved nothing — an unconstrained Host API result, most of all —
/// which is a fact about the language rather than a fault in the program.
fn check_summary(modules: usize, files: usize, warnings: usize, notes: usize) -> String {
    let mut out = format!("checked {modules} module(s), {files} file(s)");
    if warnings > 0 {
        out.push_str(&format!(", {warnings} warning(s)"));
    }
    if notes > 0 {
        out.push_str(&format!(", {notes} note(s)"));
    }
    out
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
        let blocks = module_blocks(sources, &package.root, package, program, resolved);
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
    program: &Program,
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
                                program,
                                &resolved.name,
                                &decl.name.node,
                                entry,
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
                                program,
                                &resolved.name,
                                &decl.name.node,
                                entry,
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
pub(crate) fn location_line(sources: &SourceMap, root: &Path, span: Span, indent: usize) -> String {
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
pub(crate) fn fn_signature(entry: &FnEntry) -> String {
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
///
/// An `export opaque struct` renders its header and its exported methods
/// and no fields: the outline is the interface a caller may write against,
/// and the representation of an opaque type is not part of it.
#[allow(clippy::too_many_arguments)]
fn render_struct_block(
    sources: &SourceMap,
    root: &Path,
    program: &Program,
    module: &str,
    name: &str,
    entry: &StructEntry,
    indent: usize,
) -> String {
    let mut out = String::new();
    doc_lines(&entry.doc, indent, &mut out);
    out.push_str(&format!(
        "{:indent$}export {}struct {name}{}\n",
        "",
        if entry.opaque { "opaque " } else { "" },
        generics_suffix(&entry.decl.generics)
    ));
    out.push_str(&location_line(
        sources,
        root,
        entry.decl.name.span,
        indent + 2,
    ));
    if !entry.opaque {
        for field in &entry.decl.fields {
            out.push_str(&format!(
                "{:indent$}{}: {}\n",
                "",
                field.name.node,
                field.ty,
                indent = indent + 2
            ));
        }
    }
    out.push_str(&render_conformances(program, module, name, indent + 2));
    out.push_str(&render_methods(
        sources,
        root,
        program,
        module,
        name,
        indent + 2,
    ));
    out
}

/// Renders an enum's doc, header, definition location, cases with their
/// payload types, and any exported methods declared for it.
#[allow(clippy::too_many_arguments)]
fn render_enum_block(
    sources: &SourceMap,
    root: &Path,
    program: &Program,
    module: &str,
    name: &str,
    entry: &EnumEntry,
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
    out.push_str(&render_conformances(program, module, name, indent + 2));
    out.push_str(&render_methods(
        sources,
        root,
        program,
        module,
        name,
        indent + 2,
    ));
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

/// Every trait the type `module.type_name` conforms to, qualified by the
/// module that declares the trait.
///
/// The conformances come from [`Program::conformances_of`] rather than from
/// the type's own module: ADR 0006 lets an `impl Trait for Type` block be
/// written where the *trait* is declared, and such a conformance is still
/// part of this type's interface.
fn render_conformances(program: &Program, module: &str, type_name: &str, indent: usize) -> String {
    let mut out = String::new();
    for (_, conformance) in program.conformances_of(module, type_name) {
        out.push_str(&format!(
            "{:indent$}conforms to {}.{}\n",
            "", conformance.trait_module, conformance.trait_name
        ));
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

/// Every exported method of the type `module.type_name`, in method-name
/// order, wherever it is declared.
fn render_methods(
    sources: &SourceMap,
    root: &Path,
    program: &Program,
    module: &str,
    type_name: &str,
    indent: usize,
) -> String {
    let mut out = String::new();
    for declared in program.methods_of(module, type_name) {
        if declared.entry.exported {
            out.push_str(&render_fn_block(sources, root, declared.entry, indent));
        }
    }
    out
}

/// `<T, U: Display>`, or an empty string when there are no generic
/// parameters.
pub(crate) fn generics_suffix(generics: &[cove_syntax::ast::GenericParam]) -> String {
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
    let run = lookup_run(&package, name)?;
    let (module, entry) = lookup_entry(&program, name, run)?;

    let flags = parse_run_flags(&args[1..])?;

    let program = Arc::new(program);
    let sources = Arc::new(sources);
    match execute_entry(&package, &program, &sources, run, module, entry, flags) {
        Ok(value) => report_exit(value),
        Err(ExecuteError::Setup(message)) => Err(CliError::Message(message)),
        Err(ExecuteError::Runtime(error)) => Err(CliError::Diagnostics {
            sources,
            items: vec![error.to_diagnostic()],
        }),
    }
}

/// Looks up `[run.<name>]`, reporting every known run name when there is no
/// such table.
pub(crate) fn lookup_run<'a>(package: &'a Package, name: &str) -> Result<&'a RunConfig, CliError> {
    package.config.runs.get(name).ok_or_else(|| {
        let known: Vec<&str> = package.config.runs.keys().map(String::as_str).collect();
        CliError::Message(format!(
            "cove.toml has no `[run.{name}]` table\n  known runs: {}",
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            }
        ))
    })
}

/// Splits `run.entry` into its module and function name and checks the
/// package declares it, which `cove run` and `cove generate` both need
/// before they can invoke it.
pub(crate) fn lookup_entry<'a>(
    program: &Program,
    name: &str,
    run: &'a RunConfig,
) -> Result<(&'a str, &'a str), CliError> {
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
    Ok((module, entry))
}

/// How [`execute_entry`] can fail: setting up its execution -- such as
/// creating the file `--trace` names -- or the entry itself failing.
///
/// These are kept apart because only the second carries a [`SourceMap`]
/// span: a setup failure is the CLI's own message, while a runtime failure
/// is a diagnostic pointing into the program that ran.
pub(crate) enum ExecuteError {
    Setup(String),
    Runtime(cove_runtime::RuntimeError),
}

/// Runs `run`'s entry under the capabilities `[run.<name>] allow` grants and
/// the budgets `flags` and `run` set.
///
/// Shared by `cove run` and `cove generate`: ADR 0010 makes a generator "an
/// ordinary capability-controlled Cove entry", so nothing about how it is
/// invoked may differ from any other run. Host registration itself goes
/// through `register_hosts`, the same call `cove build`'s embedded binary
/// uses, so a run, a generator, and the binary built from a run all face one
/// boundary rather than several that could drift apart.
pub(crate) fn execute_entry(
    package: &Package,
    program: &Arc<Program>,
    sources: &Arc<SourceMap>,
    run: &RunConfig,
    module: &str,
    entry: &str,
    flags: RunFlags,
) -> Result<cove_runtime::Value, ExecuteError> {
    // `files/` in the package is the narrow default, next to the
    // `documents/` the reader host already uses; `--files-root` is how a run
    // that means something else says so.
    let mut hosts = register_hosts(HostSetup {
        grants: run.allow.clone(),
        documents_root: package.root.join("documents"),
        files_root: flags
            .files_root
            .clone()
            .unwrap_or_else(|| package.root.join("files")),
        program_args: flags.program_args.clone(),
        allow_exec: flags.allow_exec.clone(),
    });

    let limits = Limits {
        fuel: flags.fuel.or(run.fuel),
        deadline: flags.deadline.or(run.deadline),
        max_host_calls: flags.max_host_calls.or(run.max_host_calls),
        max_call_depth: None,
        max_tasks: flags.max_tasks.or(run.max_tasks),
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
    let header = TraceHeader {
        values: flags.trace_values,
        entry: run.entry.clone(),
        args: flags.program_args.clone(),
    };
    let primary_sink: Box<dyn TraceSink> = match &trace_target {
        Some(TraceTarget::Stderr) => Box::new(JsonlSink::new(std::io::stderr(), header)),
        Some(TraceTarget::File(path)) => {
            let file = create_trace_file(path).map_err(|e| {
                ExecuteError::Setup(format!(
                    "cannot create trace file `{}`: {e}",
                    path.display()
                ))
            })?;
            // A trace a program can be asked to share should not surprise the
            // person sharing it. The file says so in its header, and the run
            // says so once, here, where the choice was made.
            if flags.trace_values == ValueCapture::Full {
                eprintln!(
                    "note: `{}` will record the arguments and result of every host call, which may include secrets; `--trace-values redacted` records only their types",
                    path.display()
                );
            }
            Box::new(JsonlSink::new(file, header))
        }
        None => Box::new(NullSink),
    };
    // `HostRegistry::call` and the task and entry events the interpreter
    // traces both reach the one destination `--trace` selected, from whichever
    // thread produced them: two independent sinks writing the same file would
    // race for it.
    //
    // A run that asked for neither a trace nor statistics installs no sink at
    // all rather than a composite over `NullSink`. Recording an event
    // describes every value the call carried, and `NullSink` is what tells
    // the registry that nothing will read the description.
    let sink: Arc<dyn TraceSink> = if trace_target.is_some() || flags.stats {
        Arc::new(CompositeSink {
            primary: primary_sink,
            wait_total: wait_total.clone(),
        })
    } else {
        Arc::new(NullSink)
    };
    hosts.set_trace(sink.clone());
    hosts.set_budget(budget);

    let program_args: Vec<Rc<str>> = flags
        .program_args
        .iter()
        .map(|a| a.as_str().into())
        .collect();

    let runtime =
        Runtime::new(Arc::clone(program), Arc::clone(sources), Arc::new(hosts)).with_trace(sink);
    let mut interpreter = Interpreter::new(&runtime);
    let outcome = interpreter.run_entry(module, entry, program_args);
    let heap = interpreter.heap_stats();

    if flags.stats {
        print_stats(runtime.hosts(), &wait_total, &heap);
    }

    outcome.map_err(ExecuteError::Runtime)
}

/// A `--fuel`, `--deadline`, `--max-host-calls`, `--trace`, `--stats`,
/// `--files-root`, or `--allow-exec` flag to `cove run`, parsed from anywhere
/// after the run name.
///
/// The last two are the authority a `cove.toml` cannot yet express: a
/// `[run.<name>]` table grants coarse capabilities by name, and neither the
/// root the `files` host is confined to nor the executables `process.run` may
/// start is a capability name. They are flags rather than config keys because
/// the flag lives here, where the CLI already decides what to register, and
/// `cove.toml`'s parser belongs to the compiler.
///
/// `Cancellation` is deliberately not hooked up: Rust's standard library has no signal
/// handling API, so installing a SIGINT handler would need a crate (such as
/// `signal-hook` or `ctrlc`) or unsafe, platform-specific `extern "C"` FFI
/// duplicating one. Neither fits "if it is not straightforward with std
/// alone, skip it," so `cove run` cannot yet be interrupted through
/// `Cancellation` from outside; only the limits below can stop a run.
pub(crate) struct RunFlags {
    fuel: Option<u64>,
    deadline: Option<Duration>,
    max_host_calls: Option<u64>,
    /// The tasks the run may hold alive at once, across the whole run,
    /// before it is stopped.
    max_tasks: Option<u64>,
    trace: Option<TraceTarget>,
    /// How much of each host call the trace records.
    trace_values: ValueCapture,
    stats: bool,
    /// The one directory the `files` host may reach.
    files_root: Option<PathBuf>,
    /// The executables `process.run` may start. Empty allows none.
    allow_exec: Vec<PathBuf>,
    program_args: Vec<String>,
}

impl RunFlags {
    /// No CLI overrides at all: capabilities and budgets come entirely from
    /// `[run.<name>]`, which is what `cove generate` uses so a generator
    /// runs under exactly the authority its config table grants, nothing
    /// more and nothing a flag could add.
    pub(crate) fn none() -> RunFlags {
        RunFlags {
            fuel: None,
            deadline: None,
            max_host_calls: None,
            max_tasks: None,
            trace: None,
            trace_values: ValueCapture::Full,
            stats: false,
            files_root: None,
            allow_exec: Vec::new(),
            program_args: Vec::new(),
        }
    }
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
        max_tasks: None,
        trace: None,
        trace_values: ValueCapture::Full,
        stats: false,
        files_root: None,
        allow_exec: Vec::new(),
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
            "--max-tasks" => {
                let value = flag_value(args, &mut i, "--max-tasks")?;
                flags.max_tasks = Some(value.parse().map_err(|_| {
                    CliError::Message(format!(
                        "`--max-tasks` must be a non-negative integer, found `{value}`"
                    ))
                })?);
            }
            "--trace" => {
                let value = flag_value(args, &mut i, "--trace")?;
                flags.trace = Some(TraceTarget::from_flag(&value));
            }
            // The default records values: a trace that does not carry them
            // cannot be replayed, and replay is why they are recorded at all.
            // `redacted` is the form to share.
            "--trace-values" => {
                let value = flag_value(args, &mut i, "--trace-values")?;
                flags.trace_values = ValueCapture::parse(&value).ok_or_else(|| {
                    CliError::Message(format!(
                        "`--trace-values` must be `full` or `redacted`, found `{value}`"
                    ))
                })?;
            }
            "--stats" => flags.stats = true,
            "--files-root" => {
                let value = flag_value(args, &mut i, "--files-root")?;
                flags.files_root = Some(PathBuf::from(value));
            }
            // Repeating the flag adds one executable rather than replacing
            // the list: an allow-list a later flag could silently empty would
            // be a filter that is hard to be sure of.
            "--allow-exec" => {
                let value = flag_value(args, &mut i, "--allow-exec")?;
                let path = PathBuf::from(&value);
                if !path.is_absolute() {
                    return Err(CliError::Message(format!(
                        "`--allow-exec` takes an absolute path, found `{value}`"
                    )));
                }
                flags.allow_exec.push(path);
            }
            other => flags.program_args.push(other.to_string()),
        }
        i += 1;
    }
    Ok(flags)
}

/// Consumes and returns the value following the flag at `args[*i]`,
/// advancing `*i` to point at it so the caller's loop increment lands on the
/// next unconsumed argument.
pub(crate) fn flag_value(args: &[String], i: &mut usize, flag: &str) -> Result<String, CliError> {
    let value = args
        .get(*i + 1)
        .ok_or_else(|| CliError::Message(format!("`{flag}` needs a value")))?;
    *i += 1;
    Ok(value.clone())
}

/// Parses a `--deadline` value such as `"500ms"`, using the same unit
/// meanings as `cove.toml`'s `deadline` key and the lexer's duration
/// literals: `ns`, `us`, `ms`, `s`, `m`, and `h`.
pub(crate) fn parse_duration_flag(text: &str) -> Result<Duration, String> {
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
struct WaitTotal(Arc<Mutex<Duration>>);

impl WaitTotal {
    fn get(&self) -> Duration {
        *self
            .0
            .lock()
            .expect("the wait total is never held across a panic")
    }
}

impl TraceSink for WaitTotal {
    fn record(&self, event: TraceEvent) {
        if let TraceEvent::HostCall { wait, .. } = event {
            if let Ok(mut total) = self.0.lock() {
                *total += wait;
            }
        }
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
    fn record(&self, event: TraceEvent) {
        self.wait_total.record(event.clone());
        self.primary.record(event);
    }
}

/// Prints fuel spent, host calls, elapsed time, host-call wait, and the heap
/// to stderr, for `--stats`.
///
/// `irreversible_writes` is the count of calls whose Host API schema declares
/// them irreversible: how much of what this run did cannot be taken back. The
/// heap figures are what ADR 0011 asks a run to be able to report: how much
/// every task allocated, how often their heaps were collected, how much is
/// live, and how long tasks were stopped for. `pause` is summed over threads,
/// so a run whose tasks collected at the same time can report more pause than
/// it took wall-clock time.
fn print_stats(hosts: &HostRegistry, wait_total: &WaitTotal, heap: &HeapStats) {
    let counters =
        hosts.with_budget(|budget| (budget.fuel_spent(), budget.host_calls(), budget.elapsed()));
    if let Some((fuel_spent, host_calls, elapsed)) = counters {
        eprintln!(
            "stats: fuel_spent={} host_calls={} irreversible_writes={} elapsed={:?} wait={:?}",
            fuel_spent,
            host_calls,
            hosts.irreversible_writes(),
            elapsed,
            wait_total.get(),
        );
    }
    eprintln!(
        "heap: allocated={} allocated_bytes={} collections={} freed={} live_bytes={} peak_bytes={} pause={:?}",
        heap.allocated_objects,
        heap.allocated_bytes,
        heap.collections,
        heap.freed_objects,
        heap.live_bytes,
        heap.peak_bytes,
        heap.pause,
    );
}

/// An entry returning `Err(...)` fails the run and prints the error.
pub(crate) fn report_exit(value: cove_runtime::Value) -> Result<(), CliError> {
    use cove_runtime::value::Value;
    if let Value::Enum(result) = &value {
        if value.is_err() {
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
    use crate::fixture::{examples_root, load_fixture, write, TempDir};

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

        // Saying whether source is valid is `cove check`'s job. `cove fmt`
        // reports what it could not parse and succeeds, so that a broken file
        // cannot mask an unformatted one in `--check`.
        assert!(
            cmd_fmt(&[dir.path().display().to_string()]).is_ok(),
            "fmt does not fail on a parse error"
        );
        assert_eq!(std::fs::read_to_string(&source).unwrap(), broken);
    }

    #[test]
    fn fmt_check_fails_for_an_unformatted_file_even_beside_a_broken_one() {
        let dir = TempDir::new("fmt-broken-and-unformatted");
        write(dir.path(), "cove.toml", "");
        write(dir.path(), "app/main.cove", "fn main() {\n  let x = ;\n}\n");
        write(dir.path(), "app/other.cove", "export fn f() -> Int {1}\n");

        let Err(error) = cmd_fmt(&["--check".into(), dir.path().display().to_string()]) else {
            panic!("an unformatted file must fail `--check`");
        };
        assert!(matches!(error, CliError::Unformatted));
    }

    #[test]
    fn fmt_formats_a_directory_of_packages_that_is_not_one_itself() {
        let dir = TempDir::new("fmt-no-package");
        write(dir.path(), "one/cove.toml", "");
        write(
            dir.path(),
            "one/app/main.cove",
            "export fn f() -> Int {1}\n",
        );
        write(dir.path(), "two/cove.toml", "");
        write(
            dir.path(),
            "two/app/main.cove",
            "export fn g() -> Int {2}\n",
        );

        assert!(
            cmd_fmt(&[dir.path().display().to_string()]).is_ok(),
            "formatting succeeds"
        );

        for path in ["one/app/main.cove", "two/app/main.cove"] {
            let text = std::fs::read_to_string(dir.path().join(path)).unwrap();
            assert!(
                text.contains("  1\n") || text.contains("  2\n"),
                "`{path}` was not formatted: {text}"
            );
        }
    }

    #[test]
    fn skipped_summary_counts_files_it_could_not_parse() {
        assert_eq!(
            skipped_summary(1),
            "skipped 1 file(s) that do not parse; run `cove check`"
        );
    }

    #[test]
    fn fmt_summary_counts_the_files_it_rewrote() {
        assert_eq!(fmt_summary(0), "formatted 0 file(s)");
        assert_eq!(fmt_summary(3), "formatted 3 file(s)");
    }

    #[test]
    fn outline_matches_hello_and_config_in_the_real_examples_package() {
        let (sources, package, program) = load_fixture(&examples_root());
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
            out.contains(&format!("{expected_hello}module httpstatus\n")),
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

    /// The outline is the interface another module may write against, so an
    /// opaque type shows its name and its exported methods and not the
    /// fields that back them.
    #[test]
    fn outline_hides_an_opaque_type_s_representation() {
        let dir = TempDir::new("opaque-outline");
        write(dir.path(), "cove.toml", "");
        write(
            dir.path(),
            "auth/token.cove",
            "\
/// A token.
export opaque struct Token {
  raw: String
}

impl Token {
  /// The token as text.
  export fn text(self) -> String {
    self.raw
  }
}
",
        );
        let (sources, package, program) = load_fixture(dir.path());
        let out = render_outline(&sources, &package, &program);

        let expected = "\
module auth
  /// A token.
  export opaque struct Token
    at auth/token.cove:2:22
    /// The token as text.
    export fn text(self) -> String
      at auth/token.cove:8:13
";
        assert_eq!(out, expected);
    }

    fn flags(args: &[&str]) -> RunFlags {
        parse_run_flags(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
            .unwrap_or_else(|_| panic!("`{args:?}` should parse"))
    }

    #[test]
    fn a_run_with_no_host_flags_leaves_the_defaults_alone() {
        let flags = flags(&["input.txt"]);
        assert_eq!(flags.files_root, None);
        assert!(flags.allow_exec.is_empty());
        assert_eq!(flags.program_args, ["input.txt"]);
    }

    #[test]
    fn files_root_and_allow_exec_are_read_from_anywhere_after_the_name() {
        let flags = flags(&[
            "first",
            "--files-root",
            "/tmp/scratch",
            "--allow-exec",
            "/bin/echo",
            "--allow-exec",
            "/bin/cat",
            "second",
        ]);
        assert_eq!(flags.files_root, Some(PathBuf::from("/tmp/scratch")));
        assert_eq!(
            flags.allow_exec,
            [PathBuf::from("/bin/echo"), PathBuf::from("/bin/cat")]
        );
        assert_eq!(flags.program_args, ["first", "second"]);
    }

    /// A relative name would be resolved against `PATH` or the working
    /// directory, so the executable that ran would be chosen by the
    /// environment rather than by the host.
    #[test]
    fn allow_exec_refuses_a_path_that_is_not_absolute() {
        let error = parse_run_flags(&["--allow-exec".to_string(), "echo".to_string()])
            .err()
            .expect("a relative path should be refused");
        match error {
            CliError::Message(message) => assert_eq!(
                message,
                "`--allow-exec` takes an absolute path, found `echo`"
            ),
            _ => panic!("expected a message"),
        }
    }

    /// A conformance may be declared in the module that declares the
    /// *trait*, for a type declared elsewhere: ADR 0006's orphan rule asks
    /// only that the module declaring the block declares one of the two.
    /// The type's own outline block must still name it, because the
    /// conformance and the methods it supplies are part of that type's
    /// public interface wherever they were written.
    #[test]
    fn outline_shows_a_conformance_declared_in_the_trait_s_module() {
        let dir = TempDir::new("cross-module-conformance");
        write(dir.path(), "cove.toml", "");
        write(
            dir.path(),
            "shapes/widget.cove",
            "\
/// A widget.
export struct Widget {
  id: Int
}
",
        );
        write(
            dir.path(),
            "report/summary.cove",
            "\
use shapes.Widget

/// A value that can summarise itself.
export trait Summary {
  /// Returns the one-line summary.
  fn summarize(self) -> String
}

impl Summary for Widget {
  fn summarize(self) -> String {
    \"widget {self.id}\"
  }
}
",
        );
        let (sources, package, program) = load_fixture(dir.path());
        let out = render_outline(&sources, &package, &program);

        let expected = "\
module shapes
  /// A widget.
  export struct Widget
    at shapes/widget.cove:2:15
    id: Int
    conforms to report.Summary
    /// Returns the one-line summary.
    export fn summarize(self) -> String
      at report/summary.cove:10:6
";
        assert!(
            out.contains(expected),
            "the type's block must name the conformance and its method:\n{out}"
        );
    }

    #[test]
    fn check_summary_omits_warning_count_when_there_are_none() {
        assert_eq!(check_summary(7, 7, 0, 0), "checked 7 module(s), 7 file(s)");
    }

    #[test]
    fn check_summary_mentions_warning_count_when_present() {
        assert_eq!(
            check_summary(7, 7, 3, 0),
            "checked 7 module(s), 7 file(s), 3 warning(s)"
        );
    }

    /// A note is counted apart, because `--deny-warnings` does not act on
    /// one: it names something the checker deliberately did not prove.
    #[test]
    fn check_summary_counts_notes_apart_from_warnings() {
        assert_eq!(
            check_summary(7, 7, 0, 1),
            "checked 7 module(s), 7 file(s), 1 note(s)"
        );
        assert_eq!(
            check_summary(7, 7, 2, 1),
            "checked 7 module(s), 7 file(s), 2 warning(s), 1 note(s)"
        );
    }

    #[test]
    fn program_notices_feed_the_check_summary() {
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
        assert_eq!(program.notices.len(), 3);
        assert_eq!(
            check_summary(1, 1, program.notices.len(), 0),
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
