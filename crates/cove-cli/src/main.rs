//! The `cove` command-line tool.
//!
//! The CLI does not invent semantics: the compiler derives facts, the runtime
//! enforces and records them, and the CLI explains them.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cove_diag::{render, Diagnostic, SourceMap, Span};
use cove_runtime::host::{Console, Documents, Env, Grants, HostRegistry};
use cove_runtime::interp::Interpreter;
use cove_sema::package::Package;
use cove_sema::resolve::{AliasEntry, EnumEntry, FnEntry, Program, ResolvedModule, StructEntry};
use cove_syntax::ast::ItemKind;

const USAGE: &str = "\
cove — the Cove toolchain

usage:
  cove check [path] [--deny-warnings]  parse and resolve every module in the package
  cove run <name> [args]               run the entry selected by `[run.<name>]` in cove.toml
  cove outline [path]                  show modules and their exported declarations
  cove help                            show this message

`--deny-warnings` fails `cove check` when the package has any warnings.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("help");

    let result = match command {
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
        Err(CliError::WarningsDenied) => ExitCode::FAILURE,
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
    let program = match cove_sema::resolve::resolve(&package) {
        Ok(program) => program,
        Err(items) => return Err(CliError::Diagnostics { sources, items }),
    };
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

fn cmd_check(args: &[String]) -> Result<(), CliError> {
    let mut deny_warnings = false;
    let mut path: Option<&Path> = None;
    for arg in args {
        if arg == "--deny-warnings" {
            deny_warnings = true;
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
                ItemKind::Impl(_) => {
                    // Exported methods are rendered under their struct or
                    // enum's own block, wherever it appears.
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
    if !decl.generics.is_empty() {
        let generics: Vec<&str> = decl.generics.iter().map(|g| g.node.as_str()).collect();
        sig.push('<');
        sig.push_str(&generics.join(", "));
        sig.push('>');
    }
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

/// `<T, U>`, or an empty string when there are no generic parameters.
fn generics_suffix(generics: &[cove_syntax::ast::Ident]) -> String {
    if generics.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = generics.iter().map(|g| g.node.as_str()).collect();
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

    let mut hosts = HostRegistry::new(Grants::new(run.allow.clone()));
    hosts.register(Box::new(Console::new(std::io::stdout())));
    hosts.register(Box::new(Env::from_process()));
    hosts.register(Box::new(Documents::rooted(package.root.join("documents"))));

    let program_args: Vec<std::rc::Rc<str>> = args[1..].iter().map(|a| a.as_str().into()).collect();

    let mut interpreter = Interpreter::new(&program, &sources, &mut hosts);
    match interpreter.run_entry(module, entry, program_args) {
        Ok(value) => report_exit(value),
        Err(error) => Err(CliError::Diagnostics {
            sources,
            items: vec![error.to_diagnostic()],
        }),
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
}
