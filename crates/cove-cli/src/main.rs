//! The `cove` command-line tool.
//!
//! The CLI does not invent semantics: the compiler derives facts, the runtime
//! enforces and records them, and the CLI explains them.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cove_diag::{render, Diagnostic, SourceMap};
use cove_runtime::host::{Console, Documents, Env, Grants, HostRegistry};
use cove_runtime::interp::Interpreter;
use cove_sema::package::Package;
use cove_sema::resolve::Program;

const USAGE: &str = "\
cove — the Cove toolchain

usage:
  cove check [path]        parse and resolve every module in the package
  cove run <name> [args]   run the entry selected by `[run.<name>]` in cove.toml
  cove outline [path]      show modules and their exported declarations
  cove help                show this message
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = args.first().map(String::as_str).unwrap_or("help");

    let result = match command {
        "check" => cmd_check(args.get(1).map(Path::new)),
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
    }
}

enum CliError {
    Message(String),
    Diagnostics {
        sources: SourceMap,
        items: Vec<Diagnostic>,
    },
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

fn cmd_check(path: Option<&Path>) -> Result<(), CliError> {
    let (sources, package, program) = load(path)?;
    let modules = program.modules.len();
    let files: usize = package.modules.values().map(|m| m.units.len()).sum();
    let _ = sources;
    println!("checked {modules} module(s), {files} file(s)");
    Ok(())
}

fn cmd_outline(path: Option<&Path>) -> Result<(), CliError> {
    let (_, _, program) = load(path)?;
    for module in program.modules.values() {
        println!("module {}", module.name);
        for (name, entry) in &module.functions {
            if !entry.exported {
                continue;
            }
            println!("  export fn {name}");
        }
        for (name, entry) in &module.structs {
            if entry.exported {
                println!("  export struct {name}");
            }
        }
        for (name, entry) in &module.enums {
            if entry.exported {
                println!("  export enum {name}");
            }
        }
    }
    Ok(())
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
