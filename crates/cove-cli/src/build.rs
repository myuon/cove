//! `cove build`: packaging a run as a self-contained native executable.
//!
//! ADR 0009 decided what the command produces — a native executable that
//! embeds the program and the runtime — and, just as importantly, what it
//! does not: it is not a code generator, and nothing here turns Cove into
//! machine code.
//!
//! The mechanism is deliberately the boring one. `cove build` writes a small
//! Rust crate that `include_str!`s the package's checked sources, records the
//! `[run.<name>]` table as constants, depends on `cove-runtime` by path, and
//! hands both to [`cove_runtime::embed::Embedded::main`]; `cargo build
//! --release` then links the runtime into one executable. That costs a Rust
//! toolchain at build time and buys an artifact anyone can read: the crate is
//! left on disk, and every byte the binary carries came from a file you can
//! open.
//!
//! What the built binary carries is fixed here, at build time. It reads no
//! `cove.toml`, so the grants and limits below are the only ones it will ever
//! have.

use std::path::{Path, PathBuf};
use std::time::Duration;

use cove_diag::{Diagnostic, SourceMap};
use cove_sema::package::Package;
use cove_sema::resolve::Program;

use crate::{flag_value, parse_duration_flag, Backend, CliError};

/// Where a build's generated crates live, under the package's own `target/`
/// so that a second build of the same run is incremental.
const SCRATCH: &str = "target/cove-build";

pub(crate) const BUILD_USAGE: &str = "\
cove build — package a run as a self-contained native executable

usage:
  cove build <name> [--out <path>] [flags]

`cove build` writes a native executable that embeds the package's sources and
the Cove runtime, and that runs `[run.<name>]`'s entry when it is started. It
is not a code generator: the binary runs the same program `cove run` does,
on the same backend, so building changes how a program is delivered rather
than how fast it runs. See docs/adr/0009-cove-build.md and, for which backend
that is, docs/adr/0034-one-physical-word-stack.md.

The binary needs no toolchain, no `cove` on the path, and no source tree. Its
entry, its granted capabilities, and its limits are the ones `[run.<name>]`
recorded when it was built, and it accepts only its program's own arguments:
a `cove.toml` placed beside it grants it nothing.

A run that sets `generates` cannot be built: its entry returns source text
for `cove generate` to write, not a program meant to be started, and a
binary that silently discarded that return value would not be what building
it was asked for. Build the program that consumes the generated file
instead, once `cove generate <name>` has written it.

Building the binary needs more than running one does. It needs `cargo` on the
PATH and the Cove source tree this `cove` was built from, because producing an
executable means compiling and linking the runtime into it; set `COVE_CRATES`
to that tree's `crates/` directory when it has moved. `cove run` needs
neither, which is why it stays the way programs are iterated on.

flags:
  --out <path>          where to write the executable; defaults to
                        `target/<name>` in the package root
  --fuel <n>            bake in a limit of <n> fuel
  --deadline <duration>  bake in a deadline, e.g. `500ms`, `5s`, `1h`
  --max-host-calls <n>  bake in a limit of <n> host calls
  --max-tasks <n>       bake in a limit of <n> tasks alive at once
  --files-root <path>   the one directory the `files` host may reach, baked in
                        as an absolute path; without it the binary uses
                        `files/` in the directory it is run from, as it uses
                        `documents/` there for the `documents` host
  --allow-exec <path>   an absolute path `process.run` may start; repeat to
                        allow more, and omit to allow none
  --backend <ast|vm>    which backend the binary runs on: `vm`, the
                        linear-memory backend of ADR 0034 and the default, or
                        `ast`, the tree-walking interpreter

`--backend` is baked in like everything else, because a built binary honours
no flag of its own. A program the lowering has a gap for is reported here, at
build time, rather than by the binary when somebody starts it: the diagnostic
points into source, and the person holding the source is the one who can do
something about it.

Each flag overrides the `[run.<name>]` table for this build only. There is no
`--stats` and no `--trace`: those are flags of the command that runs a
program, and a built binary honours no flag of its own, since its arguments
are its program's. A `[run.<name>]` table that sets `trace` builds a binary
that writes that trace, because that was recorded before the build; there is
no way to ask a built binary for one it was not built with, and no way to ask
it what it spent. Measure and observe with `cove run`.

The generated crate is left in `target/cove-build/<name>/`, both so you can
read what was built and so a second build is incremental; every run of a
package builds through the one Cargo target directory beside them.
";

/// Builds the executable for `[run.<name>]`.
pub(crate) fn cmd_build(args: &[String]) -> Result<(), CliError> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{BUILD_USAGE}");
        return Ok(());
    }
    let flags = parse_build_flags(args)?;
    // A built binary must not defer an error to whoever runs it, so the
    // package is loaded exactly as `cove run` loads it: a package that does
    // not check is not built.
    let (sources, package, program) = crate::load(None)?;
    let plan = plan(&sources, &package, &program, &flags)?;
    if let Err(items) = lower_what_the_binary_will_lower(&plan, &program, &sources) {
        return Err(CliError::Diagnostics {
            items,
            sources: std::sync::Arc::new(sources),
        });
    }

    emit(&plan)?;
    let artifact = compile(&plan)?;
    install(&plan, &artifact)?;
    println!("{}", build_summary(&plan));
    Ok(())
}

/// What `cove build` was asked for, before a package is loaded.
struct BuildFlags {
    name: String,
    out: Option<PathBuf>,
    fuel: Option<u64>,
    deadline: Option<Duration>,
    max_host_calls: Option<u64>,
    max_tasks: Option<u64>,
    files_root: Option<PathBuf>,
    allow_exec: Vec<PathBuf>,
    backend: Backend,
}

/// Parses `cove build`'s arguments.
///
/// Unlike `cove run`, an unrecognised argument is an error rather than a
/// program argument: a built binary takes its arguments when it is run, so
/// there is nothing here for one to mean.
fn parse_build_flags(args: &[String]) -> Result<BuildFlags, CliError> {
    let mut name: Option<String> = None;
    let mut flags = BuildFlags {
        name: String::new(),
        out: None,
        fuel: None,
        deadline: None,
        max_host_calls: None,
        max_tasks: None,
        files_root: None,
        allow_exec: Vec::new(),
        backend: Backend::default_for_a_run(),
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => flags.out = Some(PathBuf::from(flag_value(args, &mut i, "--out")?)),
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
            "--files-root" => {
                let value = flag_value(args, &mut i, "--files-root")?;
                flags.files_root = Some(absolute(Path::new(&value))?);
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
            other if other.starts_with('-') => {
                return Err(CliError::Message(format!(
                    "unknown `cove build` flag `{other}`\n\n{BUILD_USAGE}"
                )));
            }
            other if name.is_none() => name = Some(other.to_string()),
            other => {
                return Err(CliError::Message(format!(
                    "`cove build` takes one run name, and was given `{other}` as well\n  \
                     a built binary takes its arguments when it is run, not when it is built"
                )));
            }
        }
        i += 1;
    }
    flags.name = name.ok_or_else(|| {
        CliError::Message(
            "`cove build` needs the name of a `[run.<name>]` table in cove.toml".to_string(),
        )
    })?;
    Ok(flags)
}

/// One `.cove` file the built binary will carry.
#[derive(Debug)]
pub(crate) struct EmbeddedFile {
    /// The file's path relative to the package root, which is what names its
    /// module and what a diagnostic from the built binary reports.
    pub(crate) path: String,
    pub(crate) text: String,
}

/// Everything the built binary is made of, derived from the package and the
/// `[run.<name>]` table before anything is written.
#[derive(Debug)]
pub(crate) struct BuildPlan {
    /// The `[run.<name>]` table being built.
    pub(crate) name: String,
    /// The fully qualified entry function, such as `hello.main`.
    pub(crate) entry: String,
    pub(crate) allow: Vec<String>,
    pub(crate) fuel: Option<u64>,
    pub(crate) deadline: Option<Duration>,
    pub(crate) max_host_calls: Option<u64>,
    pub(crate) max_tasks: Option<u64>,
    pub(crate) trace: Option<String>,
    pub(crate) files_root: Option<PathBuf>,
    pub(crate) allow_exec: Vec<PathBuf>,
    /// Where the executable is written.
    pub(crate) out: PathBuf,
    /// Where the generated crate is written.
    pub(crate) scratch: PathBuf,
    /// The Cargo target directory every run of this package builds through,
    /// so that building a second run compiles the runtime again only if
    /// something it depends on changed.
    pub(crate) target_dir: PathBuf,
    /// The generated crate's name, which is also the name of the executable
    /// Cargo writes before it is copied to `out`.
    pub(crate) artifact: String,
    pub(crate) sources: Vec<EmbeddedFile>,
    /// The backend the binary is built to run on.
    pub(crate) backend: Backend,
}

/// Works out what to build, or says why it cannot.
///
/// This resolves the entry the same way `cove run` does, and against the same
/// resolved program, so a run that `cove run` refuses to start is a run
/// `cove build` refuses to build.
fn plan(
    sources: &SourceMap,
    package: &Package,
    program: &Program,
    flags: &BuildFlags,
) -> Result<BuildPlan, CliError> {
    let name = &flags.name;
    let run = crate::lookup_run(package, name)?;
    // A `generates` run's entry produces source text, not a program meant to
    // be started; embedding it as a standalone binary would silently drop
    // the return value a generator's whole point is, which is not something
    // a person asking for a distributable executable is likely to want.
    // `cove generate <name>` is the command for what this run is for.
    if run.generates.is_some() {
        return Err(CliError::Message(format!(
            "`[run.{name}]` sets `generates`, so it is a generator, not a program to build\n  \
             use `cove generate {name}` instead"
        )));
    }
    // The lookup validates the entry; `plan` needs only `run.entry` itself,
    // which the `BuildPlan` below carries as the qualified string.
    crate::lookup_entry(program, name, run)?;

    let mut embedded = Vec::new();
    for module in package.modules.values() {
        for unit in &module.units {
            let relative = unit.path.strip_prefix(&package.root).map_err(|_| {
                CliError::Message(format!(
                    "`{}` is not inside the package root `{}`",
                    unit.path.display(),
                    package.root.display()
                ))
            })?;
            let path = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            embedded.push(EmbeddedFile {
                path,
                text: sources.get(unit.file).text.clone(),
            });
        }
    }

    Ok(BuildPlan {
        name: name.clone(),
        entry: run.entry.clone(),
        allow: run.allow.clone(),
        fuel: flags.fuel.or(run.fuel),
        deadline: flags.deadline.or(run.deadline),
        max_host_calls: flags.max_host_calls.or(run.max_host_calls),
        max_tasks: flags.max_tasks.or(run.max_tasks),
        trace: run.trace.clone(),
        files_root: flags.files_root.clone(),
        allow_exec: flags.allow_exec.clone(),
        out: flags
            .out
            .clone()
            .unwrap_or_else(|| package.root.join("target").join(executable_name(name))),
        scratch: package.root.join(SCRATCH).join(sanitize(name)),
        target_dir: package.root.join(SCRATCH).join("target"),
        artifact: artifact_name(name),
        sources: embedded,
        backend: flags.backend,
    })
}

/// Lowers what the binary will lower, so that a binary that could not start
/// is never written.
///
/// ADR 0009's rule is that "a built binary must not defer an error to
/// whoever runs it", which is why `cove build` checks the package rather
/// than leaving it to the binary. A lowering that stopped is the same kind of
/// error and gets the same treatment: it points into source, and whoever
/// holds the source is who can act on it.
///
/// The IR is thrown away — it is not a serialization format, so the binary
/// lowers again when it starts. What this buys is the moment the diagnostic
/// arrives, not the work.
fn lower_what_the_binary_will_lower(
    plan: &BuildPlan,
    program: &Program,
    sources: &SourceMap,
) -> Result<(), Vec<Diagnostic>> {
    if plan.backend != Backend::Vm {
        return Ok(());
    }
    // `plan` was built from an entry `lookup_entry` already validated, so
    // the split and the lookup below both succeed for anything that got
    // here.
    let Some((module, entry)) = plan.entry.rsplit_once('.') else {
        return Ok(());
    };
    // The shipped schemas and no others, which is the set `crate::load`
    // checked this package against and the set the binary will lower with.
    cove_ir::lower_entry(
        program,
        sources,
        &cove_sema::HostSchemas::new(),
        module,
        entry,
    )
    .map(|_| ())
}

/// The file name an executable for `name` gets, which is `name` itself
/// everywhere the platform does not insist otherwise.
fn executable_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// A run name reduced to something safe to use as one path component.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The generated crate's name.
///
/// Every run of a package builds through one Cargo target directory, so two
/// runs must not both be `cove-built`: they would write over each other's
/// artifact and each rebuild what the other had just built. A Cargo package
/// name is narrower than a run name, so everything else becomes `-`.
fn artifact_name(run: &str) -> String {
    let tail: String = run
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("cove-built-{tail}")
}

/// Resolves `path` against the working directory, since a path baked into a
/// binary is read wherever that binary is later run.
fn absolute(path: &Path) -> Result<PathBuf, CliError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir()
        .map_err(|e| CliError::Message(format!("cannot read the current directory: {e}")))?;
    Ok(cwd.join(path))
}

/// Writes the generated crate: its manifest, its `main.rs`, and a copy of
/// every source file the binary embeds.
fn emit(plan: &BuildPlan) -> Result<(), CliError> {
    let crates = cove_crates_dir(&plan.name)?;
    // A previous build of a package that has since lost a file must not leave
    // that file behind to be embedded again.
    let embedded_dir = plan.scratch.join("src/embedded");
    if embedded_dir.exists() {
        std::fs::remove_dir_all(&embedded_dir).map_err(|e| {
            CliError::Message(format!("cannot clear `{}`: {e}", embedded_dir.display()))
        })?;
    }
    for file in &plan.sources {
        write_file(&embedded_dir.join(&file.path), &file.text)?;
    }
    write_file(&plan.scratch.join("Cargo.toml"), &manifest(plan, &crates))?;
    write_file(&plan.scratch.join("src/main.rs"), &generated_main(plan))?;
    Ok(())
}

fn write_file(path: &Path, text: &str) -> Result<(), CliError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::Message(format!("cannot create `{}`: {e}", parent.display())))?;
    }
    std::fs::write(path, text)
        .map_err(|e| CliError::Message(format!("cannot write `{}`: {e}", path.display())))
}

/// The `crates/` directory of the Cove source tree this `cove` was built
/// from, which the generated crate depends on by path.
///
/// Linking the runtime into an executable means compiling it, so a build
/// needs the runtime's source. `COVE_CRATES` is how to say where it is when
/// the tree has moved since this binary was compiled.
fn cove_crates_dir(name: &str) -> Result<PathBuf, CliError> {
    let from_env = std::env::var_os("COVE_CRATES").map(PathBuf::from);
    let dir = match &from_env {
        Some(dir) => dir.clone(),
        None => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the CLI crate lives in the workspace's `crates/` directory")
            .to_path_buf(),
    };
    if dir.join("cove-runtime/Cargo.toml").is_file() {
        return Ok(dir);
    }
    Err(CliError::Message(format!(
        "`cove build` cannot find the Cove runtime's source at `{}`\n  \
         building an executable compiles and links the runtime into it, so it needs \
         the source tree this `cove` was built from\n  \
         set `COVE_CRATES` to that tree's `crates/` directory, or run `cove run {name}` \
         instead, which needs no source but this package's",
        dir.display(),
    )))
}

/// The generated crate's manifest.
///
/// The empty `[workspace]` table is what keeps the crate from being read as a
/// stray member of whatever workspace the package it was built from happens
/// to sit in.
fn manifest(plan: &BuildPlan, crates: &Path) -> String {
    format!(
        "\
# Generated by `cove build`. Everything here is derived from the package and
# its `[run.<name>]` table; change those and build again rather than this.
[package]
name = \"{artifact}\"
version = \"0.0.0\"
edition = \"2021\"
publish = false

[workspace]

[[bin]]
name = \"{artifact}\"
path = \"src/main.rs\"

[dependencies]
cove-runtime = {{ path = {path} }}
",
        artifact = plan.artifact,
        path = rust_string(&crates.join("cove-runtime").display().to_string())
    )
}

/// The generated crate's `main.rs`.
fn generated_main(plan: &BuildPlan) -> String {
    let mut sources = String::new();
    for file in &plan.sources {
        sources.push_str(&format!(
            "    EmbeddedSource {{\n        path: {},\n        text: include_str!(\"embedded/{}\"),\n    }},\n",
            rust_string(&file.path),
            file.path,
        ));
    }

    format!(
        "\
//! Generated by `cove build {name}`; every line of it is derived, so edit the
//! package it was built from rather than this file.
//!
//! The sources below are the ones that checked at build time, and the run
//! table below is the one they were built for. Nothing here reads a
//! `cove.toml`: this binary's authority was decided when it was made.

use cove_runtime::embed::{{Embedded, EmbeddedBackend, EmbeddedRun, EmbeddedSource}};

/// Every `.cove` file of the package, keyed by its path relative to the
/// package root, which is what names its module.
static SOURCES: &[EmbeddedSource] = &[
{sources}];

fn main() -> std::process::ExitCode {{
    Embedded {{
        sources: SOURCES,
        run: EmbeddedRun {{
            name: {name_literal},
            entry: {entry},
            allow: &[{allow}],
            fuel: {fuel},
            deadline_nanos: {deadline},
            max_host_calls: {max_host_calls},
            max_tasks: {max_tasks},
            trace: {trace},
            files_root: {files_root},
            allow_exec: &[{allow_exec}],
        }},
        backend: EmbeddedBackend::{backend},
    }}
    .main()
}}
",
        name = plan.name,
        name_literal = rust_string(&plan.name),
        entry = rust_string(&plan.entry),
        allow = rust_list(plan.allow.iter().map(String::as_str)),
        fuel = rust_option(plan.fuel.map(|f| f.to_string())),
        deadline = rust_option(plan.deadline.map(|d| format!("{}", d.as_nanos()))),
        max_host_calls = rust_option(plan.max_host_calls.map(|n| n.to_string())),
        max_tasks = rust_option(plan.max_tasks.map(|n| n.to_string())),
        trace = rust_option(plan.trace.as_deref().map(rust_string)),
        files_root = rust_option(
            plan.files_root
                .as_ref()
                .map(|p| rust_string(&p.display().to_string()))
        ),
        allow_exec = rust_list(
            plan.allow_exec
                .iter()
                .map(|p| p.to_str().unwrap_or_default())
        ),
        backend = match plan.backend {
            Backend::Ast => "Ast",
            Backend::Vm => "Vm",
        },
    )
}

/// `text` as a Rust string literal.
fn rust_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The items of a `&[&str]` literal.
fn rust_list<'a>(items: impl Iterator<Item = &'a str>) -> String {
    items.map(rust_string).collect::<Vec<_>>().join(", ")
}

/// An `Option` literal whose `Some` payload is already Rust source.
fn rust_option(value: Option<String>) -> String {
    match value {
        Some(value) => format!("Some({value})"),
        None => "None".to_string(),
    }
}

/// Compiles the generated crate, returning the executable it produced.
fn compile(plan: &BuildPlan) -> Result<PathBuf, CliError> {
    // `CARGO` is the cargo that launched this process, when one did. Using it
    // keeps a build inside a `cargo` invocation on the same toolchain.
    let cargo = std::env::var_os("CARGO")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "cargo".into());
    let status = std::process::Command::new(&cargo)
        .arg("build")
        .arg("--release")
        .arg("--manifest-path")
        .arg(plan.scratch.join("Cargo.toml"))
        // Named rather than inherited: a `CARGO_TARGET_DIR` meant for another
        // build would otherwise send this one into a directory something else
        // holds the lock on.
        .arg("--target-dir")
        .arg(&plan.target_dir)
        .status()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CliError::Message(format!(
                    "`cove build` needs a Rust toolchain, and `{}` is not on the PATH\n  \
                     building an executable compiles and links the runtime into it\n  \
                     install one from https://rustup.rs, or run `cove run {}` instead, \
                     which needs no toolchain",
                    cargo.to_string_lossy(),
                    plan.name,
                ))
            } else {
                CliError::Message(format!("cannot start `{}`: {e}", cargo.to_string_lossy()))
            }
        })?;
    if !status.success() {
        return Err(CliError::Message(format!(
            "compiling the generated crate failed; it is in `{}`",
            plan.scratch.display()
        )));
    }
    Ok(plan
        .target_dir
        .join("release")
        .join(executable_name(&plan.artifact)))
}

/// Copies the compiled executable to the path the build was asked for.
fn install(plan: &BuildPlan, artifact: &Path) -> Result<(), CliError> {
    if let Some(parent) = plan.out.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::Message(format!("cannot create `{}`: {e}", parent.display())))?;
    }
    // Replacing rather than overwriting: a copy into a file another process
    // is running is refused on some platforms, and would rewrite that
    // process's own executable on others.
    match std::fs::remove_file(&plan.out) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(CliError::Message(format!(
                "cannot replace `{}`: {e}",
                plan.out.display()
            )))
        }
    }
    std::fs::copy(artifact, &plan.out)
        .map_err(|e| CliError::Message(format!("cannot write `{}`: {e}", plan.out.display())))?;
    Ok(())
}

/// What `cove build` prints when it has written an executable.
pub(crate) fn build_summary(plan: &BuildPlan) -> String {
    let mut limits = Vec::new();
    if let Some(fuel) = plan.fuel {
        limits.push(format!("fuel {fuel}"));
    }
    if let Some(deadline) = plan.deadline {
        limits.push(format!("deadline {deadline:?}"));
    }
    if let Some(max) = plan.max_host_calls {
        limits.push(format!("max host calls {max}"));
    }
    if let Some(max) = plan.max_tasks {
        limits.push(format!("max tasks {max}"));
    }
    format!(
        "built `{name}` from {files} file(s) into `{out}`\n  \
         entry:   {entry}\n  \
         backend: {backend}\n  \
         grants:  {grants}\n  \
         limits:  {limits}",
        name = plan.name,
        backend = plan.backend,
        files = plan.sources.len(),
        out = plan.out.display(),
        entry = plan.entry,
        grants = if plan.allow.is_empty() {
            "(none)".to_string()
        } else {
            plan.allow.join(", ")
        },
        limits = if limits.is_empty() {
            "(none)".to_string()
        } else {
            limits.join(", ")
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{write, TempDir};

    /// A package with one module, one run, and nothing else.
    fn fixture(dir: &Path, cove_toml: &str) {
        write(dir, "cove.toml", cove_toml);
        write(
            dir,
            "app/main.cove",
            "\
use console.println

/// Runs.
export fn main(args: Array<String>) -> Result<Unit, Error> {
  console.println(\"hi\")?
  Ok(())
}
",
        );
    }

    fn flags(name: &str) -> BuildFlags {
        BuildFlags {
            name: name.to_string(),
            out: None,
            fuel: None,
            deadline: None,
            max_host_calls: None,
            max_tasks: None,
            files_root: None,
            allow_exec: Vec::new(),
            backend: Backend::default_for_a_run(),
        }
    }

    fn plan_for(dir: &Path, flags: &BuildFlags) -> Result<BuildPlan, CliError> {
        let (sources, package, program) = crate::load(Some(dir))?;
        plan(&sources, &package, &program, flags)
    }

    /// `plan_for` for a fixture that is meant to build, reporting whatever
    /// went wrong when it does not.
    fn plan_ok(dir: &Path, flags: &BuildFlags) -> BuildPlan {
        match plan_for(dir, flags) {
            Ok(plan) => plan,
            Err(CliError::Message(message)) => panic!("the fixture must build: {message}"),
            Err(_) => panic!("the fixture must build"),
        }
    }

    #[test]
    fn plans_the_entry_grants_and_limits_the_run_table_recorded() {
        let dir = TempDir::new("build-plan");
        fixture(
            dir.path(),
            "[run.app]\nentry = \"app.main\"\nallow = [\"console\"]\nfuel = 1000\ndeadline = \"5s\"\nmax_host_calls = 3\nmax_tasks = 4\n",
        );

        let plan = plan_ok(dir.path(), &flags("app"));
        assert_eq!(plan.entry, "app.main");
        assert_eq!(plan.allow, ["console"]);
        assert_eq!(plan.fuel, Some(1000));
        assert_eq!(plan.deadline, Some(Duration::from_secs(5)));
        assert_eq!(plan.max_host_calls, Some(3));
        assert_eq!(plan.max_tasks, Some(4));
        assert_eq!(
            plan.sources
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["app/main.cove"],
            "the binary carries the package's sources, keyed by the path that names their module"
        );
        assert!(plan.sources[0].text.contains("console.println"));
    }

    #[test]
    fn a_flag_overrides_the_limit_the_run_table_recorded() {
        let dir = TempDir::new("build-limits");
        fixture(
            dir.path(),
            "[run.app]\nentry = \"app.main\"\nallow = [\"console\"]\nfuel = 1000\n",
        );

        let mut flags = flags("app");
        flags.fuel = Some(7);
        flags.max_host_calls = Some(2);
        flags.max_tasks = Some(6);
        let plan = plan_ok(dir.path(), &flags);
        assert_eq!(plan.fuel, Some(7));
        assert_eq!(plan.max_host_calls, Some(2));
        assert_eq!(plan.max_tasks, Some(6));
    }

    #[test]
    fn output_defaults_to_the_run_name_under_the_package_target_directory() {
        let dir = TempDir::new("build-out");
        fixture(dir.path(), "[run.app]\nentry = \"app.main\"\n");

        let plan = plan_ok(dir.path(), &flags("app"));
        assert_eq!(
            plan.out,
            dir.path().join("target").join(executable_name("app"))
        );
        assert_eq!(plan.scratch, dir.path().join("target/cove-build/app"));

        let mut flags = flags("app");
        flags.out = Some(PathBuf::from("/tmp/somewhere/app"));
        let plan = plan_ok(dir.path(), &flags);
        assert_eq!(plan.out, PathBuf::from("/tmp/somewhere/app"));
    }

    #[test]
    fn a_run_name_that_does_not_exist_lists_the_ones_that_do() {
        let dir = TempDir::new("build-unknown-run");
        fixture(dir.path(), "[run.app]\nentry = \"app.main\"\n");

        let Err(CliError::Message(message)) = plan_for(dir.path(), &flags("nope")) else {
            panic!("an unknown run must fail the build");
        };
        assert_eq!(
            message,
            "cove.toml has no `[run.nope]` table\n  known runs: app"
        );
    }

    #[test]
    fn an_entry_the_package_does_not_declare_fails_the_build() {
        let dir = TempDir::new("build-unknown-entry");
        fixture(dir.path(), "[run.app]\nentry = \"app.missing\"\n");

        let Err(CliError::Message(message)) = plan_for(dir.path(), &flags("app")) else {
            panic!("an entry that does not exist must fail the build");
        };
        assert_eq!(
            message,
            "`[run.app] entry` refers to `app.missing`, which this package does not declare"
        );
    }

    #[test]
    fn a_run_that_sets_generates_is_refused() {
        let dir = TempDir::new("build-refuses-generates");
        fixture(
            dir.path(),
            "[run.app]\nentry = \"app.main\"\ngenerates = \"out/app.cove\"\n",
        );

        let Err(CliError::Message(message)) = plan_for(dir.path(), &flags("app")) else {
            panic!("a run that generates must not be built");
        };
        assert_eq!(
            message,
            "`[run.app]` sets `generates`, so it is a generator, not a program to build\n  use `cove generate app` instead"
        );
    }

    #[test]
    fn a_package_that_does_not_check_is_not_built() {
        let dir = TempDir::new("build-does-not-check");
        write(dir.path(), "cove.toml", "[run.app]\nentry = \"app.main\"\n");
        write(
            dir.path(),
            "app/main.cove",
            "\
/// Runs.
export fn main() -> Result<Unit, Error> {
  let n: Int = \"not an Int\"
  Ok(())
}
",
        );

        let Err(CliError::Diagnostics { items, .. }) = plan_for(dir.path(), &flags("app")) else {
            panic!("a package that does not check must not be built");
        };
        assert!(
            items
                .iter()
                .any(|d| d.severity == cove_diag::Severity::Error),
            "the type error is reported at build time, not left to the built binary"
        );
    }

    #[test]
    fn the_generated_main_bakes_in_the_run_table() {
        let dir = TempDir::new("build-codegen");
        fixture(
            dir.path(),
            "[run.app]\nentry = \"app.main\"\nallow = [\"console\"]\ndeadline = \"5s\"\n",
        );

        let plan = plan_ok(dir.path(), &flags("app"));
        let main = generated_main(&plan);
        assert!(main.contains("entry: \"app.main\""), "{main}");
        assert!(main.contains("allow: &[\"console\"]"), "{main}");
        assert!(main.contains("deadline_nanos: Some(5000000000)"), "{main}");
        assert!(main.contains("fuel: None"), "{main}");
        assert!(
            main.contains("include_str!(\"embedded/app/main.cove\")"),
            "{main}"
        );
    }

    #[test]
    fn a_source_path_and_a_files_root_survive_being_written_as_rust() {
        assert_eq!(rust_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(rust_list(["a", "b"].into_iter()), "\"a\", \"b\"");
        assert_eq!(rust_option(None), "None");
    }

    #[test]
    fn the_summary_reports_the_boundary_that_was_baked_in() {
        let dir = TempDir::new("build-summary");
        fixture(
            dir.path(),
            "[run.app]\nentry = \"app.main\"\nallow = [\"console\"]\nfuel = 10\n",
        );
        let plan = plan_ok(dir.path(), &flags("app"));
        let summary = build_summary(&plan);
        assert!(
            summary.starts_with("built `app` from 1 file(s) into `"),
            "{summary}"
        );
        assert!(summary.contains("entry:   app.main"), "{summary}");
        assert!(summary.contains("backend: vm"), "{summary}");
        assert!(summary.contains("grants:  console"), "{summary}");
        assert!(summary.contains("limits:  fuel 10"), "{summary}");
    }

    #[test]
    fn a_run_with_no_grants_and_no_limits_says_so() {
        let dir = TempDir::new("build-summary-bare");
        fixture(dir.path(), "[run.app]\nentry = \"app.main\"\n");
        let plan = plan_ok(dir.path(), &flags("app"));
        let summary = build_summary(&plan);
        assert!(summary.contains("grants:  (none)"), "{summary}");
        assert!(summary.contains("limits:  (none)"), "{summary}");
    }

    /// The backend the binary was built for is part of the summary, because
    /// it is part of what was baked in: a person handed two binaries built
    /// from the same run table has no other way to tell them apart.
    #[test]
    fn the_summary_names_the_backend_the_binary_was_built_for() {
        let dir = TempDir::new("build-summary-backend");
        fixture(dir.path(), "[run.app]\nentry = \"app.main\"\n");
        let mut chosen = flags("app");
        chosen.backend = Backend::Ast;
        let summary = build_summary(&plan_ok(dir.path(), &chosen));
        assert!(summary.contains("backend: ast"), "{summary}");
    }

    /// A binary that could not start is never written: `cove build` lowers
    /// what the binary will lower, at build time, so that a gap in the
    /// lowering reaches whoever holds the source rather than whoever holds
    /// the binary.
    ///
    /// The program below is the one this case used to pin a *refusal* with —
    /// a function declared inside a function body, which the predecessor
    /// backend had no instruction for. It lowers now, and the case is worth
    /// keeping in that form: what it asserts is that the build-time lowering
    /// runs and answers about the entry, and a construct that used to stop it
    /// is a better witness for that than one that never could.
    #[test]
    fn the_binary_s_lowering_runs_at_build_time() {
        let dir = TempDir::new("build-lowering");
        write(dir.path(), "cove.toml", "[run.app]\nentry = \"app.main\"\n");
        write(
            dir.path(),
            "app/main.cove",
            "\
/// Runs.
export fn main() -> Result<Unit, Error> {
  fn double(n: Int) -> Int {
    n * 2
  }
  assertEqual(double(21), 42)?
  Ok(())
}
",
        );
        let plan = plan_ok(dir.path(), &flags("app"));
        let (sources, _, program) = crate::fixture::check_fixture(dir.path());
        assert!(lower_what_the_binary_will_lower(&plan, &program, &sources).is_ok());

        // And the interpreter, which lowers nothing, is asked for nothing.
        let mut on_the_oracle = flags("app");
        on_the_oracle.backend = Backend::Ast;
        let plan = plan_ok(dir.path(), &on_the_oracle);
        assert!(lower_what_the_binary_will_lower(&plan, &program, &sources).is_ok());
    }

    #[test]
    fn an_argument_that_is_not_a_flag_or_the_run_name_is_refused() {
        let Err(CliError::Message(message)) = parse_build_flags(&["app".into(), "extra".into()])
        else {
            panic!("a second positional argument must fail");
        };
        assert!(message.contains("takes one run name"), "{message}");

        let Err(CliError::Message(message)) = parse_build_flags(&["--nope".into()]) else {
            panic!("an unknown flag must fail");
        };
        assert!(
            message.starts_with("unknown `cove build` flag `--nope`"),
            "{message}"
        );

        let Err(CliError::Message(message)) = parse_build_flags(&[]) else {
            panic!("a build with no run name must fail");
        };
        assert_eq!(
            message,
            "`cove build` needs the name of a `[run.<name>]` table in cove.toml"
        );
    }

    #[test]
    fn the_help_says_it_is_not_a_code_generator_and_what_a_build_needs() {
        assert!(BUILD_USAGE.contains("It\nis not a code generator"));
        assert!(BUILD_USAGE.contains("`cargo` on the"));
        assert!(BUILD_USAGE.contains("a `cove.toml` placed beside it grants it nothing"));
        // Observation is not authority, but it is still a flag, and a built
        // binary has none of its own.
        assert!(BUILD_USAGE.contains("There is no\n`--stats` and no `--trace`"));
    }

    #[test]
    fn two_runs_of_one_package_build_through_one_target_directory_as_two_crates() {
        let dir = TempDir::new("build-two-runs");
        fixture(
            dir.path(),
            "[run.app]\nentry = \"app.main\"\n[run.other-app]\nentry = \"app.main\"\n",
        );

        let one = plan_ok(dir.path(), &flags("app"));
        let two = plan_ok(dir.path(), &flags("other-app"));
        assert_eq!(one.target_dir, two.target_dir);
        assert_ne!(one.scratch, two.scratch);
        // Sharing a target directory means the artifacts in it must not
        // collide, so the crate is named after the run it was built for.
        assert_eq!(one.artifact, "cove-built-app");
        assert_eq!(two.artifact, "cove-built-other-app");
        assert_eq!(artifact_name("a.b c"), "cove-built-a-b-c");
    }
}
