//! The `cove` command-line tool.
//!
//! The CLI does not invent semantics: the compiler derives facts, the runtime
//! enforces and records them, and the CLI explains them.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cove_diag::{render, Diagnostic, SourceMap, Span};
use cove_runtime::embed::{register_hosts, HostSetup};
use cove_runtime::host::HostRegistry;
use cove_runtime::interp::Interpreter;
use cove_runtime::{
    create_trace_file, Budget, Cancellation, HeapStats, JsonlSink, Limits, NullSink,
    RecordingBackend, Runtime, TraceEvent, TraceHeader, TraceSink, ValueCapture, Vm,
};
use cove_sema::capability::open_reasons;
use cove_sema::config::RunConfig;
use cove_sema::package::Package;
use cove_sema::resolve::{
    AliasEntry, EnumEntry, FnEntry, Program, ResolvedModule, StructEntry, TraitEntry,
};
use cove_sema::HostSchemas;
use cove_syntax::ast::ItemKind;

mod api;
mod build;
mod debug;
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
  cove debug <name> [flags] [args]     run <name> under a stopping debugger, from a prompt
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
and no source tree. It is not a code generator: the binary runs the same
program `cove run` does, on the same backend `cove build` chose for it. Its entry, its granted capabilities, and its
limits are the ones `[run.<name>]` recorded when it was built, and a
`cove.toml` placed beside it grants it nothing. Building one needs `cargo`
and this toolchain's own source, because an executable has to link the
runtime; `cove build --help` says so in full, and ADR 0009 says why.

The linear-memory backend of ADR 0034 is what runs a program.
`cove run --backend ast` runs the entry on the tree-walking interpreter
instead, which is the semantic oracle and what a disagreement between the two
is decided by. What is lowered is what the entry can reach, so a construct
elsewhere in the package cannot stop an entry that does not reach it. The
backend answers no admission predicate: a construct its lowering has not been
taught is a gap in the lowering rather than a program it declines, and what
stops the run is a compile error naming the gap, before anything happens.
`--stats` reports how long lowering took apart from how long the run took,
and how many instructions the run executed — the figure a change to the
lowering is judged by, because wall time moves for many reasons and that
moves for one.

`cove run --encoded` runs the entry from the program's fixed-width encoded
instructions rather than from the readable IR. It is a development flag for
issue #245's phased bytecode work, not a second backend: the same machine
runs the same program over the same memory and answers the same thing, and
the encoded path today implements the opcodes the `arith` benchmark reaches
and refuses every other one by name before the run starts.

`cove test` runs every `test fn` in the package, reports each one, and exits
non-zero when any failed. `--filter` runs only the tests whose qualified name
contains the given substring, and `--backend <ast|vm>` chooses which
backend runs them, defaulting to `vm` as `cove run` does. Each test is
lowered on its own, so a gap in the lowering fails that test and not the
suite. Each test is granted exactly the
capabilities its call graph requires, with each host's fake implementation,
so a suite is deterministic; `cove.toml`'s `[test] allow_real = [...]` names
the capabilities to grant for real instead.

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
`--backend <ast|vm>` chooses the backend a generator runs on, defaulting
to `vm`; it is the only `cove run` flag `cove generate` takes, because every
other budget is `[run.<name>]`'s.

`cove trace` reads a JSONL trace written by `cove run --trace` and prints a
summary and a timeline. It reports which of the distinctions ADR 0001 asks a
trace to make the events actually carry, and which they do not.

`cove replay <file> <name>` runs `[run.<name>]`'s entry again with every host
replaced by one that answers from `<file>`, in the recorded order. The
program's own computation runs for real; only the Host API boundary is canned,
so no host is called and nothing outside the process changes. A replay exits
non-zero when it diverges: the program asked for a call the trace does not
have, asked for a different one, or stopped before using them all.
`--backend <ast|vm>` chooses which backend replays it, defaulting since
ADR 0026 to the one the trace says recorded it rather than to `cove run`'s
default, so an ordinary replay is a same-backend replay and is one by reading
the file. Naming the flag replays across backends deliberately, which stays
supported; the summary and every divergence report then say so, because a
divergence found across backends could be the two backends' rather than the
program's.

`cove debug <name>` runs `[run.<name>]`'s entry on the linear-memory backend
with the run stopped before its first instruction, and reads gdb-shaped
commands from stdin: `break`, `continue`, `step`, `next`, `stepi`, `finish`,
`backtrace`, `frame`, `list`, `print`, `locals`, `words`, `disassemble`,
`object`, `quit`. `help` at the prompt lists them and `help limits` says what
`step` and `break` get wrong, which is worth reading once: spans are
per-instruction and expression-level, so `one source line` is a rule with
edges rather than a fact the program records. It takes `--fuel`, `--deadline`,
`--max-host-calls`, `--max-tasks`, `--files-root` and `--allow-exec`, and no
`--backend`: a debugger is a feature of the linear-memory machine and that is
the only backend it runs on. A `--deadline` keeps elapsing while you stand at
the prompt, which is what a deadline means.

`cove run` flags (may appear in any position after <name>; everything after a
literal `--` is a program argument, even if it looks like a flag):
  --fuel <n>            stop the run after <n> fuel is spent
  --deadline <duration>  stop the run after <duration> has elapsed, e.g. `500ms`, `5s`, `1h`
  --max-host-calls <n>  stop the run after <n> host calls
  --trace <path>        write a JSONL trace to <path>, or `-` for stderr
  --trace-values <mode> `full` (the default) records each host call's arguments and result, which is what `cove replay` needs; `redacted` records only their types
  --max-tasks <n>       stop the run when it would hold more than <n> tasks at once
  --backend <ast|vm>    which backend runs the entry: `vm`, the linear-memory backend of ADR 0034 and the default, or `ast`, the tree-walking interpreter and the semantic oracle
  --stats               print the backend's lowering and execution times and the instructions it executed, then fuel spent, host calls, irreversible writes, elapsed time, host-call wait, and the heap, to stderr
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
        "debug" => debug::cmd_debug(&args[1..]),
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

/// Renders a function or method's doc, signature, definition location,
/// required capabilities, and whether that list is a floor rather than the
/// whole of it.
///
/// A capability-open declaration says so on a line of its own rather than
/// leaving a reader to assume `requires` is exhaustive: ADR 0015 makes the
/// derived set a lower bound, and a report that hid the difference would be
/// the one thing that decision rules out.
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
    if entry.is_capability_open() {
        out.push_str(&format!(
            "{:indent$}capability-open: {}\n",
            "",
            open_reasons(&entry.open_calls),
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
        Err(ExecuteError::NotLowered(items)) => Err(CliError::Diagnostics { items, sources }),
        Err(ExecuteError::Runtime(error)) => Err(CliError::Diagnostics {
            items: vec![runtime_failure(&program, module, entry, &error)],
            sources,
        }),
    }
}

/// The diagnostic a run's failure is reported as, with what a
/// capability-open entry owes a refusal at the Host boundary.
///
/// The runtime decides what a call may do and its answer stands. What it
/// cannot say is why the static report did not warn about this call first,
/// and for an entry whose call graph contains an indirect call the answer is
/// that it could not: ADR 0015 makes the derived set a floor, so a refusal
/// here is exactly the case that floor was honest about.
pub(crate) fn runtime_failure(
    program: &Program,
    module: &str,
    entry: &str,
    error: &cove_runtime::RuntimeError,
) -> Diagnostic {
    let mut diagnostic = error.to_diagnostic();
    let open = program
        .lookup_fn(module, entry)
        .is_some_and(FnEntry::is_capability_open);
    if !open || error.denied_capability.is_none() {
        return diagnostic;
    }
    let note = format!(
        "`{module}.{entry}` is capability-open, so `cove outline` reports the capabilities it needs as a floor rather than the whole list; a call reached through a function value or a `dyn` receiver is not in it"
    );
    diagnostic.help = Some(match diagnostic.help {
        Some(help) => format!("{help}; {note}"),
        None => note,
    });
    diagnostic
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
/// These are kept apart because only the last two carry a [`SourceMap`]
/// span: a setup failure is the CLI's own message, while a runtime failure
/// and a refused lowering both point into the program.
pub(crate) enum ExecuteError {
    Setup(String),
    Runtime(cove_runtime::RuntimeError),
    /// The backend met something its lowering could not emit code for.
    ///
    /// Diagnostics rather than a refusal, and that is what ADR 0034 decides:
    /// `cove-ir` has no admission predicate to answer, so what stops it is
    /// either a gap in the lowering or a type the checker never settled, and
    /// both are already `cove_diag::Diagnostic`s pointing at source. The CLI
    /// renders them exactly as it renders a compile error, because that is
    /// what they are — including the ones whose text says the fault is the
    /// backend's.
    ///
    /// A `Vec`, because one lowering finds every gap in what the entry
    /// reaches rather than stopping at the first.
    NotLowered(Vec<Diagnostic>),
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
    // A run either finishes on the backend it named or fails before any side
    // effect, so the lowering happens here: before a host is registered,
    // before a trace file is created, before anything the program could be
    // observed by. A gap in the lowering stops the command with the gap named
    // and pointed at, and never quietly finishes on the interpreter.
    //
    // What is lowered is what this entry reaches, which is the program this
    // command was asked to run. A package holds as many programs as it has
    // `[run.<name>]` tables, and a closure in one of the others is not a
    // reason this one cannot run: `cove_ir::lower_entry` slices by the
    // checker's own call graph and closes the slice against what the lowering
    // names. The coverage harness lowers a case the same way, so the two
    // agree about what an entry's program is.
    let lowered = match flags.backend {
        Backend::Ast => None,
        Backend::Vm => {
            let started = Instant::now();
            // The shipped schemas and no others, which is the set
            // `cove_sema::Compiler::new()` checked this package against —
            // a `cove` command registers the hosts it ships and nothing
            // else, and the lowering has to read what the checker read.
            let ir = cove_ir::lower_entry(program, sources, &HostSchemas::new(), module, entry)
                .map_err(ExecuteError::NotLowered)?;
            Some(Lowered {
                program: Arc::new(ir),
                lower: started.elapsed(),
            })
        }
    };

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
    // The backend goes in the header because this is the one place that
    // knows both that a trace is being written and which evaluator is about
    // to write it; ADR 0026 is why a file says so rather than a reader
    // guessing.
    let header = TraceHeader {
        backend: flags.backend.recording(),
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

    // One entry, one backend of three, and the same three arguments whichever
    // it is: `run_entry` is the seam ADR 0019 puts them behind, so what
    // differs between one `--backend` and another is which evaluator is
    // built and nothing else about how it is called.
    let started = Instant::now();
    let (outcome, memory, instructions) = match lowered.as_ref().map(|l| &l.program) {
        Some(ir) => {
            // `--encoded` refuses before the run rather than during it, so a
            // program this path cannot execute stops here having done
            // nothing — no host call, no file, no output.
            let mut vm = if flags.encoded {
                Vm::encoded(&runtime, runtime.hosts(), ir).map_err(ExecuteError::Runtime)?
            } else {
                Vm::new(&runtime, runtime.hosts(), ir)
            };
            let outcome = vm.run_entry(module, entry, program_args);
            (
                outcome,
                Memory::Words {
                    held: vm.heap_words(),
                    handed_out: vm.allocated_words(),
                },
                Some(vm.instructions()),
            )
        }
        None => {
            let mut interpreter = Interpreter::new(&runtime);
            let outcome = interpreter.run_entry(module, entry, program_args);
            (outcome, Memory::Objects(interpreter.heap_stats()), None)
        }
    };
    let execution = started.elapsed();

    if flags.stats {
        print_backend_stats(flags.backend, lowered.as_ref(), execution, instructions);
        print_stats(runtime.hosts(), &wait_total, &memory);
    }

    outcome.map_err(ExecuteError::Runtime)
}

/// What the lowering produced before the run, and what producing it cost.
///
/// Issue #111 asks for a compile/lower breakdown apart from steady-state
/// execution, and the two costs are separable only because they happen at
/// separate times: lowering runs once per program, execution runs for as long
/// as the program does.
///
/// The program is what the entry reaches, so the figure measures what was
/// lowered rather than what the package happened to hold beside it.
///
/// There is no verification time beside it, and that is not an omission:
/// `cove_ir::lower` verifies what it emitted before it answers, so there is
/// no second phase to time.
struct Lowered {
    program: Arc<cove_ir::Program>,
    lower: Duration,
}

/// What a run's memory did, in the figures its own backend counts.
///
/// The two evaluators do not count the same things, and neither one's figures
/// can be derived from the other's: the interpreter's heap is a set of
/// objects and reports how many were allocated, freed and live, while the
/// linear memory is a run of words and reports how many it holds and how
/// many it handed out. Reporting one in the other's shape would mean either
/// inventing object counts the machine never kept or printing zeros that
/// read as measurements.
enum Memory {
    Objects(HeapStats),
    Words {
        /// Words the heap region occupies, free blocks included.
        held: u64,
        /// Words handed out over the whole run, reuse counted each time.
        handed_out: u64,
    },
}

/// Prints which backend ran the entry, what each phase cost, and how many
/// instructions the run executed, for `--stats`.
///
/// The interpreter reports no lowering time because it does none: it walks
/// the checked program directly, and a zero there would read as a measurement
/// rather than as the absence of a phase. It reports no instruction count for
/// the same reason — it has no instructions, and `instructions=0` would read
/// as a run that did nothing.
///
/// The count is here, beside the timings, because it is what a change to the
/// lowering is judged by: wall time moves for many reasons, and how many
/// instructions a program needed moves for exactly one.
fn print_backend_stats(
    backend: Backend,
    lowered: Option<&Lowered>,
    execution: Duration,
    instructions: Option<u64>,
) {
    let counted = match instructions {
        Some(instructions) => instructions.to_string(),
        None => "none".to_string(),
    };
    match lowered {
        Some(lowered) => eprintln!(
            "backend: {backend} lower={:?} validate=in-lower execute={:?} instructions={counted}",
            lowered.lower, execution
        ),
        None => {
            eprintln!("backend: {backend} lower=none execute={execution:?} instructions={counted}")
        }
    }
}

/// A `--backend`, `--fuel`, `--deadline`, `--max-host-calls`, `--trace`,
/// `--stats`, `--files-root`, or `--allow-exec` flag to `cove run`, parsed
/// from anywhere after the run name.
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
#[derive(Clone)]
pub(crate) struct RunFlags {
    /// Which backend runs the entry. `vm` is the default: issue #111's gate
    /// was passed and ADR 0022 records the decision for the backend of the
    /// day, and ADR 0034 kept the arrangement when it replaced that backend
    /// with this one. `ast` remains what a disagreement is decided by, which
    /// is a different job from running a program.
    backend: Backend,
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
    /// Run the entry from the program's encoded instructions rather than
    /// from the readable `Inst` IR.
    ///
    /// [Issue #245](https://github.com/myuon/cove/issues/245)'s Phase 3, and
    /// a **development flag** rather than a way to run a program: the
    /// encoded path executes what the `arith` benchmark reaches and refuses
    /// every other opcode by name, before the run starts. It is not a
    /// `--backend` value because it is not a backend — the same machine runs
    /// the same program over the same memory, and what differs is the
    /// representation the loop reads. A trace written from it says `vm`,
    /// because that is what wrote it.
    ///
    /// It defaults to false everywhere, including [`RunFlags::none`], so no
    /// existing command reaches it.
    encoded: bool,
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
    ///
    /// "No overrides" includes the backend, so a generator runs on whichever
    /// backend `cove run` runs on. ADR 0010 makes a generator an ordinary
    /// capability-controlled Cove entry and `execute_entry` is the one seam
    /// both reach it through; a generator pinned to the other backend would
    /// be a second kind of run.
    pub(crate) fn none() -> RunFlags {
        RunFlags {
            backend: Backend::default_for_a_run(),
            fuel: None,
            deadline: None,
            max_host_calls: None,
            max_tasks: None,
            trace: None,
            trace_values: ValueCapture::Full,
            stats: false,
            encoded: false,
            files_root: None,
            allow_exec: Vec::new(),
            program_args: Vec::new(),
        }
    }

    /// Selects a backend, for a command that parses `--backend` itself
    /// rather than through [`parse_run_flags`].
    pub(crate) fn set_backend(&mut self, backend: Backend) {
        self.backend = backend;
    }
}

/// Which backend runs the entry.
///
/// The linear-memory backend of ADR 0034 is the default and is what a program
/// runs on. The interpreter stays selectable, and stays the oracle: a backend
/// checked against it is presumed wrong when the two disagree, whichever of
/// them a run reached by default.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Backend {
    /// The tree-walking interpreter.
    Ast,
    /// The linear-memory backend of ADR 0034, over `cove-ir`.
    ///
    /// It ran under the transitional spelling `lvm` between ADR 0034's
    /// cutover and the rename that followed, and nothing was built to keep
    /// that spelling alive — no alias, no deprecation path, no second
    /// spelling. `--backend lvm` is refused the way any other unknown value
    /// is.
    Vm,
}

impl Backend {
    /// The backend a command that runs a program uses when nobody named
    /// one.
    ///
    /// One function rather than a literal at each command, because "the
    /// default backend" is one decision and five commands make it: `cove
    /// run`, `cove generate`, `cove test`, `cove build`, and — since ADR
    /// 0023 — `cove replay`. Written out five times it could be changed in
    /// four places, and a toolchain whose commands disagreed about which
    /// backend runs a program would be a mixture nobody asked for.
    pub(crate) fn default_for_a_run() -> Backend {
        Backend::Vm
    }

    pub(crate) fn parse(value: &str) -> Option<Backend> {
        match value {
            "ast" => Some(Backend::Ast),
            "vm" => Some(Backend::Vm),
            _ => None,
        }
    }

    /// What `--backend` accepts, as a message names them.
    ///
    /// One string rather than a literal at each command, for the reason
    /// [`Backend::default_for_a_run`] is one function: the set of names is
    /// one fact, five commands refuse an unknown one, and a list written out
    /// five times is a list that can be extended in four places. It is also
    /// what made renaming the backend a single edit.
    pub(crate) const NAMES: &'static str = "`ast` or `vm`";

    /// This backend as a trace header names it.
    ///
    /// Two enums for two backends, because the crates draw the line
    /// elsewhere: `cove_runtime` records without knowing what a command-line
    /// flag is, and this one is a flag's parsed value. They share the two
    /// spellings rather than the type, and this is the one function that
    /// joins them.
    pub(crate) fn recording(self) -> RecordingBackend {
        match self {
            Backend::Ast => RecordingBackend::Ast,
            Backend::Vm => RecordingBackend::Vm,
        }
    }

    /// The backend that wrote a recording, as the flag that could have named
    /// it.
    ///
    /// The inverse of [`Backend::recording`], and what makes `cove replay`'s
    /// default a reading of the file rather than an inference about it.
    pub(crate) fn of_recording(backend: RecordingBackend) -> Backend {
        match backend {
            RecordingBackend::Ast => Backend::Ast,
            RecordingBackend::Vm => Backend::Vm,
        }
    }
}

/// Splits `--backend <ast|vm>` out of a command's arguments, leaving the
/// rest in the order they were written.
///
/// It may appear anywhere, exactly as it may on `cove run`: one flag spelled
/// two ways depending on which command it is passed to would be a flag with
/// two meanings. `cove generate` and `cove replay` both reach it through here
/// rather than each parsing it, so the value it accepts and the sentence an
/// unknown one is refused with are one thing rather than several that could
/// drift.
pub(crate) fn split_backend(args: &[String]) -> Result<(Backend, Vec<String>), CliError> {
    let (backend, rest) = split_backend_if_named(args)?;
    Ok((backend.unwrap_or_else(Backend::default_for_a_run), rest))
}

/// The same split, answering `None` for a command that was given no
/// `--backend`.
///
/// `cove replay` is the one command with somewhere better than
/// [`Backend::default_for_a_run`] to look when nobody named a backend: since
/// ADR 0026 the trace says which backend wrote it, and a replay of it should
/// be that backend. Whether the flag was written is therefore a question
/// that command has to be able to ask, and it asks it here rather than by
/// scanning the arguments a second time — the value `--backend` accepts and
/// the sentence an unknown one is refused with stay in one place, which is
/// the whole point of this function.
pub(crate) fn split_backend_if_named(
    args: &[String],
) -> Result<(Option<Backend>, Vec<String>), CliError> {
    let mut backend = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--backend" {
            let value = args.get(i + 1).ok_or_else(|| {
                CliError::Message(format!("`--backend` needs a value: {}", Backend::NAMES))
            })?;
            backend = Some(Backend::parse(value).ok_or_else(|| {
                CliError::Message(format!(
                    "`--backend` must be {}, found `{value}`",
                    Backend::NAMES
                ))
            })?);
            i += 2;
            continue;
        }
        rest.push(args[i].clone());
        i += 1;
    }
    Ok((backend, rest))
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Backend::Ast => "ast",
            Backend::Vm => "vm",
        })
    }
}

/// Where `--trace` (or the config's `trace` key) sends trace lines.
#[derive(Clone)]
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
        backend: Backend::default_for_a_run(),
        fuel: None,
        deadline: None,
        max_host_calls: None,
        max_tasks: None,
        trace: None,
        trace_values: ValueCapture::Full,
        stats: false,
        encoded: false,
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
            "--backend" => {
                let value = flag_value(args, &mut i, "--backend")?;
                flags.backend = Backend::parse(&value).ok_or_else(|| {
                    CliError::Message(format!(
                        "`--backend` must be {}, found `{value}`",
                        Backend::NAMES
                    ))
                })?;
            }
            "--stats" => flags.stats = true,
            "--encoded" => flags.encoded = true,
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
fn print_stats(hosts: &HostRegistry, wait_total: &WaitTotal, memory: &Memory) {
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
    match memory {
        Memory::Objects(heap) => eprintln!(
            "heap: allocated={} allocated_bytes={} collections={} freed={} live_bytes={} peak_bytes={} pause={:?}",
            heap.allocated_objects,
            heap.allocated_bytes,
            heap.collections,
            heap.freed_objects,
            heap.live_bytes,
            heap.peak_bytes,
            heap.pause,
        ),
        Memory::Words { held, handed_out } => eprintln!(
            "memory: heap_words={held} heap_bytes={} allocated_words={handed_out}",
            held * 8
        ),
    }
}

/// An entry returning `Err(...)` fails the run and prints the error.
pub(crate) fn report_exit(value: cove_runtime::Value) -> Result<(), CliError> {
    if let Some(payload) = value.err_payload() {
        let message = payload.first().map(ToString::to_string).unwrap_or_default();
        return Err(CliError::Message(message));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{examples_root, load_fixture, write, TempDir};
    use cove_runtime::RunOutcome;

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
        // The next module's header is what pins the end of this one: without
        // it a `config` that had grown a declaration would still contain the
        // text above. Modules render in sorted order, so the terminator is
        // whichever module now sorts next, and that is a name a new example
        // can take: it was `cq`, and `covecheck` sorts before it.
        assert!(
            out.contains(&format!("{expected_config}module covecheck\n")),
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

    /// `requires` is a floor whenever a declaration reaches a call the
    /// compiler cannot follow, so the outline says which it is rather than
    /// letting a reader read the line above as the whole list.
    #[test]
    fn outline_marks_a_capability_open_declaration() {
        let dir = TempDir::new("outline-capability-open");
        write(dir.path(), "cove.toml", "");
        write(
            dir.path(),
            "app/main.cove",
            "\
use console.println

/// Runs whatever it was handed, which may be anything at all.
export fn run(work: fn() -> Unit) {
  work()
}

/// Hands `run` a closure that prints.
export fn main() {
  run(fn() {
    console.println(\"hi\")
  })
}
",
        );
        let (sources, package, program) = load_fixture(dir.path());
        let out = render_outline(&sources, &package, &program);

        let expected = "\
module app
  /// Runs whatever it was handed, which may be anything at all.
  export fn run(work: fn() -> Unit)
    at app/main.cove:4:11
    capability-open: calls a function value

  /// Hands `run` a closure that prints.
  export fn main()
    at app/main.cove:9:11
    requires console
    capability-open: calls a capability-open declaration
";
        assert_eq!(out, expected);
    }

    /// The note belongs to a refusal about a *capability* and to nothing
    /// else. `RunOutcome::HostBoundary` also covers an unknown host module,
    /// an operation that does not exist, an argument the schema does not
    /// admit, and an exhausted host-call budget; telling a reader that the
    /// derived set was a floor when their argument had the wrong type
    /// misattributes the failure and buries the help that would have fixed
    /// it.
    #[test]
    fn a_boundary_failure_that_is_not_a_denied_capability_gets_no_capability_open_note() {
        let dir = TempDir::new("runtime-failure-note");
        write(dir.path(), "cove.toml", "");
        write(
            dir.path(),
            "app/main.cove",
            "\
/// Runs whatever it was handed, which may be anything at all.
export fn run(work: fn() -> Unit) {
  work()
}

/// Hands `run` a closure.
export fn main() {
  run(fn() {
  })
}
",
        );
        let (_, _, program) = load_fixture(dir.path());
        assert!(
            program
                .lookup_fn("app", "main")
                .is_some_and(FnEntry::is_capability_open),
            "the entry has to be capability-open for the note to be in question"
        );

        let schema_failure = cove_runtime::RuntimeError::new("`console.println` takes 1 argument")
            .with_help("the Host API schema declares `console.println(text: String)`")
            .with_outcome(RunOutcome::HostBoundary);
        let diagnostic = runtime_failure(&program, "app", "main", &schema_failure);
        assert_eq!(
            diagnostic.help.as_deref(),
            Some("the Host API schema declares `console.println(text: String)`"),
            "a schema failure keeps its own help and gains nothing"
        );

        let denied = cove_runtime::RuntimeError::new(
            "`console.println` requires the `console` capability, which this run was not granted",
        )
        .with_help("add `console` to `allow` in the run's `cove.toml` table")
        .with_outcome(RunOutcome::HostBoundary)
        .with_denied_capability("console");
        let diagnostic = runtime_failure(&program, "app", "main", &denied);
        let help = diagnostic.help.expect("a refusal explains itself");
        assert!(
            help.starts_with("add `console` to `allow`"),
            "the runtime's own help is kept: {help}"
        );
        assert!(
            help.contains("capability-open"),
            "and the note is appended to it: {help}"
        );
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

    /// The linear-memory backend is the default, and `cove generate` reaches
    /// it through the same `RunFlags::none()` `cove run` reaches it through,
    /// so the two are asserted together rather than separately: they are one
    /// decision, and a change that moved only one of them would be the drift
    /// ADR 0010's one-seam rule exists to prevent.
    #[test]
    fn a_run_that_names_no_backend_gets_the_default() {
        assert_eq!(flags(&["input.txt"]).backend, Backend::Vm);
        assert_eq!(RunFlags::none().backend, Backend::Vm);
    }

    #[test]
    fn the_backend_is_read_from_anywhere_after_the_name() {
        let chosen = flags(&["first", "--backend", "vm", "second"]);
        assert_eq!(chosen.backend, Backend::Vm);
        assert_eq!(chosen.program_args, ["first", "second"]);
        assert_eq!(flags(&["--backend", "ast"]).backend, Backend::Ast);
    }

    /// A backend nobody has written is a typo, and a typo that fell through
    /// to the default would run the program on the other backend while
    /// looking like it had done what was asked.
    #[test]
    fn an_unknown_backend_is_refused_rather_than_defaulted() {
        let error = parse_run_flags(&["--backend".to_string(), "jit".to_string()])
            .err()
            .expect("an unknown backend should be refused");
        match error {
            CliError::Message(message) => {
                assert_eq!(message, "`--backend` must be `ast` or `vm`, found `jit`")
            }
            _ => panic!("expected a message"),
        }
    }

    /// The commands that parse `--backend` themselves rather than through
    /// `RunFlags` share one function, so the value they accept and the
    /// sentence they refuse an unknown one with cannot drift apart. It takes
    /// the flag from anywhere, exactly as `cove run` does, and hands back
    /// everything else in the order it was written -- which is what lets
    /// `cove replay` go on calling every remaining `--` argument a flag it
    /// does not have.
    #[test]
    fn the_shared_backend_flag_is_taken_from_anywhere_and_leaves_the_rest_alone() {
        let args =
            |args: &[&str]| -> Vec<String> { args.iter().map(|arg| (*arg).to_string()).collect() };
        let Ok((backend, rest)) =
            split_backend(&args(&["t.jsonl", "--backend", "ast", "restricted"]))
        else {
            panic!("the flag parses");
        };
        assert_eq!(backend, Backend::Ast);
        assert_eq!(rest, ["t.jsonl", "restricted"]);

        let Ok((backend, rest)) = split_backend(&args(&["t.jsonl", "restricted", "--jit"])) else {
            panic!("an unrelated flag is left where it was");
        };
        assert_eq!(backend, Backend::default_for_a_run());
        assert_eq!(rest, ["t.jsonl", "restricted", "--jit"]);

        for (given, expected) in [
            (
                vec!["--backend", "jit"],
                "`--backend` must be `ast` or `vm`, found `jit`",
            ),
            (
                vec!["--backend"],
                "`--backend` needs a value: `ast` or `vm`",
            ),
        ] {
            let Err(CliError::Message(message)) = split_backend(&args(&given)) else {
                panic!("an unknown backend should be refused with a message");
            };
            assert_eq!(message, expected);
        }
    }

    /// The same split, asked whether the flag was written at all.
    ///
    /// `cove replay` needs the difference between "this backend, because
    /// nobody said" and "this backend, because somebody said": since ADR 0026
    /// the first is answered by the trace and the second overrides it. The two
    /// answers must come from one parser, or an unknown value would be
    /// refused with two sentences.
    #[test]
    fn the_shared_backend_flag_says_whether_it_was_written() {
        let args =
            |args: &[&str]| -> Vec<String> { args.iter().map(|arg| (*arg).to_string()).collect() };
        let Ok((backend, rest)) = split_backend_if_named(&args(&["t.jsonl", "restricted"])) else {
            panic!("no flag is not an error");
        };
        assert_eq!(backend, None);
        assert_eq!(rest, ["t.jsonl", "restricted"]);

        let Ok((backend, _)) =
            split_backend_if_named(&args(&["--backend", "vm", "t.jsonl", "restricted"]))
        else {
            panic!("the flag parses");
        };
        assert_eq!(backend, Some(Backend::Vm));

        let Err(CliError::Message(message)) = split_backend_if_named(&args(&["--backend", "jit"]))
        else {
            panic!("an unknown backend should be refused with a message");
        };
        assert_eq!(message, "`--backend` must be `ast` or `vm`, found `jit`");
    }

    /// A backend and the name a trace header writes it under are the same two
    /// things named twice, and the two crates that name them agree.
    #[test]
    fn a_backend_survives_the_trip_through_a_trace_header() {
        for backend in [Backend::Ast, Backend::Vm] {
            assert_eq!(Backend::of_recording(backend.recording()), backend);
            assert_eq!(backend.recording().as_str(), backend.to_string());
        }
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
