//! `cove generate`: run an explicit, capability-controlled code generator.
//!
//! ADR 0010 decides the shape: a generator is an ordinary Cove entry, run
//! under the capabilities and budgets `[run.<name>]` grants exactly as
//! `cove run` would -- [`crate::execute_entry`] is the very function both
//! commands call -- except its entry must return `Result<String, Error>`
//! instead of the ordinary `Result<Unit, Error>`. `cove generate <name>`
//! writes what it returns to the package-relative path `[run.<name>]
//! generates` names, formats it, and checks the package again: a generator
//! whose output does not parse, or does not type check, fails pointing at
//! that file, at the moment it was written.
//!
//! `cove build`, `cove run`, `cove check`, and `cove test` never generate --
//! this module is the only place in the toolchain that writes a `generates`
//! file, so a stale one is a real state the toolchain can be in.
//! `cove generate --check` regenerates every run that sets `generates` into
//! memory and refuses to let a stale file on disk go unnoticed, which is
//! what CI runs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_sema::resolve::Program;

use crate::{
    execute_entry, load, lookup_entry, lookup_run, runtime_failure, CliError, ExecuteError,
    RunFlags,
};

/// The return type ADR 0010 requires of a generator's entry, so its output
/// is always the source `cove generate` can write, format, and check.
const REQUIRED_RETURN_TYPE: &str = "Result<String, Error>";

/// Runs `cove generate <name>` or `cove generate --check`.
///
/// `--backend` is the one run flag this command takes, and it takes it for
/// the reason [`ExecuteError::NotLowered`] exists: a generator runs on
/// whichever backend `cove run` runs a program on, and a generator the
/// lowering has a gap for would otherwise have no way to run at all. Every
/// other budget stays `[run.<name>]`'s, per ADR 0010.
pub(crate) fn cmd_generate(args: &[String]) -> Result<(), CliError> {
    let (backend, rest) = crate::split_backend(args)?;
    let mut flags = RunFlags::none();
    flags.set_backend(backend);
    if rest.first().map(String::as_str) == Some("--check") {
        return generate_check(None, &flags);
    }
    let Some(name) = rest.first() else {
        return Err(CliError::Message(
            "`cove generate` needs the name of a `[run.<name>]` table that sets `generates`, or `--check`"
                .into(),
        ));
    };
    generate_one(None, name, &flags)
}

/// Runs `[run.<name>]`'s entry and writes its returned source to the
/// `generates` path, formatted, then checks the package again.
///
/// `path` is `None` for the real CLI, which resolves the package from the
/// current directory exactly as `cove run` does; tests pass a fixture's root
/// directly instead of relying on the process's current directory.
fn generate_one(path: Option<&Path>, name: &str, flags: &RunFlags) -> Result<(), CliError> {
    let (sources, package, program) = load(path)?;
    let run = lookup_run(&package, name)?;
    let Some(output) = run.generates.clone() else {
        return Err(CliError::Message(format!(
            "`[run.{name}]` has no `generates` key, so `cove generate {name}` has nowhere to write its output"
        )));
    };
    let program = Arc::new(program);
    let sources = Arc::new(sources);
    let (module, entry) = lookup_entry(&program, name, run)?;
    check_generator_shape(&program, module, entry, &run.entry)?;

    let body = match execute_entry(
        &package,
        &program,
        &sources,
        run,
        module,
        entry,
        flags.clone(),
    ) {
        Ok(value) => expect_generated_source(&run.entry, value)?,
        Err(ExecuteError::Setup(message)) => return Err(CliError::Message(message)),
        // `cove generate` runs on whichever backend `cove run` runs on, so
        // this arm is reachable: a generator that reaches a gap in the
        // lowering stops before it writes anything, and what is shown is
        // where the lowering stopped.
        Err(ExecuteError::NotLowered(items)) => {
            return Err(CliError::Diagnostics {
                items,
                sources: sources.clone(),
            })
        }
        Err(ExecuteError::Runtime(error)) => {
            return Err(CliError::Diagnostics {
                items: vec![runtime_failure(&program, module, entry, &error)],
                sources,
            })
        }
    };

    let target = package.root.join(&output);
    let text = format_best_effort(&compose(name, &body));
    write_generated_file(&target, &text)?;

    // The file just written is now an ordinary part of the package, so
    // checking it again is what catches a generator whose output does not
    // parse or does not type check -- pointing at the file it just wrote,
    // because that is where this load finds the problem.
    load(path)?;

    println!("wrote `{}` from `[run.{name}]`", output.display());
    Ok(())
}

/// Regenerates every run that sets `generates` into memory and compares it
/// against what is on disk, failing on the first one that differs.
fn generate_check(path: Option<&Path>, flags: &RunFlags) -> Result<(), CliError> {
    let (sources, package, program) = load(path)?;

    let program = Arc::new(program);
    let sources = Arc::new(sources);
    let mut stale: Vec<(String, PathBuf)> = Vec::new();
    for (name, run) in &package.config.runs {
        let Some(output) = &run.generates else {
            continue;
        };
        let (module, entry) = lookup_entry(&program, name, run)?;
        check_generator_shape(&program, module, entry, &run.entry)?;

        let body = match execute_entry(
            &package,
            &program,
            &sources,
            run,
            module,
            entry,
            flags.clone(),
        ) {
            Ok(value) => expect_generated_source(&run.entry, value)?,
            Err(ExecuteError::Setup(message)) => return Err(CliError::Message(message)),
            // Reachable for the reason `generate_one` gives: `--check`
            // lowers every generator the package declares, so one the
            // lowering has a gap for stops the check by name.
            Err(ExecuteError::NotLowered(items)) => {
                return Err(CliError::Diagnostics {
                    items,
                    sources: sources.clone(),
                })
            }
            Err(ExecuteError::Runtime(error)) => {
                return Err(CliError::Diagnostics {
                    items: vec![runtime_failure(&program, module, entry, &error)],
                    sources,
                })
            }
        };

        let target = package.root.join(output);
        let expected = format_best_effort(&compose(name, &body));
        let on_disk = std::fs::read_to_string(&target).unwrap_or_default();
        if expected != on_disk {
            stale.push((name.clone(), output.clone()));
        }
    }

    if stale.is_empty() {
        return Ok(());
    }
    for (name, output) in &stale {
        println!(
            "{} is stale; run `cove generate {name}` to update it",
            output.display()
        );
    }
    Err(CliError::GenerateStale)
}

/// Checks that `module.entry`'s declared return type is
/// `Result<String, Error>`, the shape ADR 0010 requires a generator to have.
fn check_generator_shape(
    program: &Program,
    module: &str,
    entry: &str,
    qualified: &str,
) -> Result<(), CliError> {
    let decl = &program
        .lookup_fn(module, entry)
        .expect("lookup_entry already checked this function exists")
        .decl;
    let found = decl
        .return_type
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "()".to_string());
    if found != REQUIRED_RETURN_TYPE {
        return Err(CliError::Message(format!(
            "`[run.<name>] entry` `{qualified}` must return `{REQUIRED_RETURN_TYPE}` to generate; found `{found}`"
        )));
    }
    Ok(())
}

/// Extracts the `String` a generator entry returned.
///
/// The entry's declared return type was already checked to be
/// `Result<String, Error>` before it ran, so this only has to tell an `Err`
/// apart from an `Ok`; an `Ok` payload that is not a `String` would mean the
/// interpreter and the type checker disagree, which is reported rather than
/// assumed away.
fn expect_generated_source(
    entry_name: &str,
    value: cove_runtime::Value,
) -> Result<String, CliError> {
    use cove_runtime::value::Value;
    if value.case().is_none() {
        return Err(CliError::Message(format!(
            "`{entry_name}` did not return a `Result`"
        )));
    }
    if let Some(payload) = value.err_payload() {
        return Err(CliError::Message(
            payload.first().map(ToString::to_string).unwrap_or_default(),
        ));
    }
    match value
        .ok_payload()
        .and_then(<[Value]>::first)
        .and_then(Value::as_str)
    {
        Some(source) => Ok(source.to_string()),
        _ => Err(CliError::Message(format!(
            "`{entry_name}` returned `Ok(...)` with a value that is not a `String`"
        ))),
    }
}

/// The header `cove generate` prepends to every file it writes, marking it
/// generated and naming the run that produced it.
///
/// An ordinary `//` comment, not a doc comment: a doc comment documents the
/// declaration that follows it, and this marks the whole file instead.
/// `cove fmt` reattaches an ordinary comment to what follows it by position,
/// so this header survives formatting -- and regeneration -- byte for byte.
fn header(name: &str) -> String {
    format!("// Code generated by `cove generate {name}`. DO NOT EDIT.\n")
}

/// The header, a blank line, and `body`: the full text `cove generate`
/// writes, before formatting.
fn compose(name: &str, body: &str) -> String {
    format!("{}\n{body}", header(name))
}

/// Formats `text` when it parses; returns it unchanged when it does not, so
/// the caller can still write it and let the package check that follows
/// report the parse failure, pointing at the file it was written to.
fn format_best_effort(text: &str) -> String {
    let mut sources = SourceMap::new();
    let file = sources.add(Path::new("<generated>"), text.to_string());
    match cove_syntax::parse_file(&sources, file) {
        Ok(unit) => cove_syntax::format::format_source(text, &unit),
        Err(_) => text.to_string(),
    }
}

/// Writes `text` to `target`, creating any parent directories it needs.
fn write_generated_file(target: &Path, text: &str) -> Result<(), CliError> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::Message(format!("cannot create `{}`: {e}", parent.display())))?;
    }
    std::fs::write(target, text)
        .map_err(|e| CliError::Message(format!("cannot write `{}`: {e}", target.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{write, TempDir};

    /// Panics with `context` and the diagnostic or message `result` carried,
    /// since `CliError` does not implement `Debug` and so cannot be
    /// `.expect()`-ed directly.
    #[track_caller]
    fn expect_ok(result: Result<(), CliError>, context: &str) {
        match result {
            Ok(()) => {}
            Err(CliError::Message(message)) => panic!("{context}: {message}"),
            Err(CliError::Diagnostics { items, .. }) => panic!(
                "{context}: {}",
                items
                    .iter()
                    .map(|d| d.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            Err(_) => panic!("{context}"),
        }
    }

    /// The flags every test below generates under: none at all, which is
    /// what the command itself uses and so is the backend a user gets.
    fn flags() -> RunFlags {
        RunFlags::none()
    }

    /// A tiny generator package: `[run.gen]` writes `out/generated.cove`
    /// from `gen/build.cove`'s `build` entry, which needs no capability.
    fn write_generator_fixture(root: &Path, body: &str) {
        write(
            root,
            "cove.toml",
            "[run.gen]\nentry = \"gen.build\"\ngenerates = \"out/generated.cove\"\n",
        );
        write(
            root,
            "gen/build.cove",
            &format!(
                "/// Builds the generated module.\nexport fn build() -> Result<String, Error> {{\n  Ok(\"{body}\")\n}}\n"
            ),
        );
    }

    #[test]
    fn generate_writes_formats_and_headers_the_output() {
        let dir = TempDir::new("generate-writes");
        write_generator_fixture(
            dir.path(),
            "/// A generated constant.\\nexport fn answer() -> Int \\{\\n42\\n\\}\\n",
        );

        expect_ok(
            generate_one(Some(dir.path()), "gen", &flags()),
            "generation succeeds",
        );

        let written =
            std::fs::read_to_string(dir.path().join("out/generated.cove")).expect("file exists");
        assert_eq!(
            written,
            "// Code generated by `cove generate gen`. DO NOT EDIT.\n\
             \n\
             /// A generated constant.\n\
             export fn answer() -> Int {\n  \
             42\n\
             }\n"
        );
    }

    #[test]
    fn generate_rejects_an_entry_that_does_not_return_result_string_error() {
        let dir = TempDir::new("generate-wrong-shape");
        write(
            dir.path(),
            "cove.toml",
            "[run.gen]\nentry = \"gen.build\"\ngenerates = \"out/generated.cove\"\n",
        );
        write(
            dir.path(),
            "gen/build.cove",
            "/// Wrong shape: returns `Result<Int, Error>`, not `Result<String, Error>`.\nexport fn build() -> Result<Int, Error> {\n  Ok(1)\n}\n",
        );

        let error =
            generate_one(Some(dir.path()), "gen", &flags()).expect_err("wrong shape must fail");
        match error {
            CliError::Message(message) => {
                assert!(
                    message.contains("must return `Result<String, Error>`"),
                    "{message}"
                );
                assert!(message.contains("found `Result<Int, Error>`"), "{message}");
            }
            _ => panic!("expected a message"),
        }
        assert!(!dir.path().join("out/generated.cove").exists());
    }

    #[test]
    fn generate_fails_pointing_at_the_file_when_the_output_does_not_parse() {
        let dir = TempDir::new("generate-broken-output");
        write_generator_fixture(dir.path(), "fn (");

        let error = generate_one(Some(dir.path()), "gen", &flags())
            .expect_err("unparseable output must fail");
        assert!(matches!(error, CliError::Diagnostics { .. }));
        // The file is still written: a person can open it to see the broken
        // source a generator produced.
        let written = std::fs::read_to_string(dir.path().join("out/generated.cove"))
            .expect("the broken file is still written");
        assert!(written.contains("fn ("));
    }

    #[test]
    fn generate_check_passes_when_the_written_file_matches() {
        let dir = TempDir::new("generate-check-fresh");
        write_generator_fixture(dir.path(), "export fn answer() -> Int \\{\\n  42\\n\\}\\n");
        expect_ok(
            generate_one(Some(dir.path()), "gen", &flags()),
            "generation succeeds",
        );

        expect_ok(
            generate_check(Some(dir.path()), &flags()),
            "a freshly generated file passes --check",
        );
    }

    #[test]
    fn generate_check_fails_on_a_stale_file() {
        let dir = TempDir::new("generate-check-stale");
        write_generator_fixture(dir.path(), "export fn answer() -> Int \\{\\n  42\\n\\}\\n");
        write(
            dir.path(),
            "out/generated.cove",
            "// Code generated by `cove generate gen`. DO NOT EDIT.\n\nexport fn answer() -> Int {\n  0\n}\n",
        );

        let error =
            generate_check(Some(dir.path()), &flags()).expect_err("a stale file must fail --check");
        assert!(matches!(error, CliError::GenerateStale));
    }

    #[test]
    fn generate_check_passes_when_no_run_sets_generates() {
        let dir = TempDir::new("generate-check-nothing-to-generate");
        write(dir.path(), "cove.toml", "");
        expect_ok(
            generate_check(Some(dir.path()), &flags()),
            "nothing to generate is not a failure",
        );
    }

    #[test]
    fn generate_enforces_capabilities_a_generator_was_not_granted() {
        let dir = TempDir::new("generate-no-capability");
        write(
            dir.path(),
            "cove.toml",
            "[run.gen]\nentry = \"gen.build\"\ngenerates = \"out/generated.cove\"\n",
        );
        write(
            dir.path(),
            "gen/build.cove",
            "use files.read\n\n\
             /// Reads a file without being granted `files`.\n\
             export fn build() -> Result<String, Error> {\n  \
             files.read(\"x\")?\n  \
             Ok(\"unused\")\n\
             }\n",
        );

        let error = generate_one(Some(dir.path()), "gen", &flags())
            .expect_err("an ungranted capability must fail the generator");
        match error {
            CliError::Diagnostics { items, .. } => {
                assert!(
                    items
                        .iter()
                        .any(|d| d.message.contains("requires the `files` capability")),
                    "{items:?}"
                );
            }
            CliError::Message(message) => {
                panic!("expected a capability diagnostic, found: {message}")
            }
            _ => panic!("expected a capability diagnostic"),
        }
        assert!(!dir.path().join("out/generated.cove").exists());
    }

    #[test]
    fn generate_fails_when_the_run_has_no_generates_key() {
        let dir = TempDir::new("generate-no-generates-key");
        write(
            dir.path(),
            "cove.toml",
            "[run.gen]\nentry = \"gen.build\"\n",
        );
        write(
            dir.path(),
            "gen/build.cove",
            "/// Never runs.\nexport fn build() -> Result<String, Error> {\n  Ok(\"\")\n}\n",
        );

        let error = generate_one(Some(dir.path()), "gen", &flags())
            .expect_err("a run with no `generates` key cannot be generated");
        match error {
            CliError::Message(message) => assert!(message.contains("has no `generates` key")),
            _ => panic!("expected a message"),
        }
    }
}
