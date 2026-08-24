//! End-to-end tests: real Cove programs, run through the real `cove` binary.
//!
//! Every directory under `tests/e2e/` that contains a `main.cove` is one case,
//! discovered automatically and run in sorted order by
//! `cove run <case> [arguments]`, with the working directory set to
//! `tests/e2e/`. A case must have a matching `[run.<case>]` table in
//! `tests/e2e/cove.toml`; a case without one fails the suite rather than being
//! skipped.
//!
//! Each case pins its observable behaviour with golden files:
//!
//! ```text
//! tests/e2e/
//!   cove.toml                 one [run.<name>] table per case
//!   <case>/main.cove          the program
//!   <case>/expected.out       exact expected stdout
//!   <case>/expected.err       present only when the case must fail
//!   <case>/args               optional: one program argument per line
//!   <case>/env                optional: KEY=VALUE per line, or a bare KEY
//!                             to remove that variable from the child
//! ```
//!
//! The rules this harness enforces:
//!
//! - the exit status is zero when there is no `expected.err`, and non-zero
//!   when there is one;
//! - stdout equals `expected.out` byte for byte;
//! - stderr equals `expected.err` when it exists, and is empty when it does
//!   not.
//!
//! Diagnostics contain absolute paths, so stderr is normalised before it is
//! compared: the absolute path of the `tests/e2e` directory becomes the
//! literal `<e2e>`. Nothing else is normalised, so line and column numbers
//! stay pinned.
//!
//! # Updating the golden files
//!
//! ```console
//! $ UPDATE_EXPECT=1 cargo test -p cove-cli --test e2e
//! ```
//!
//! That rewrites every golden file from the actual output: `expected.out` is
//! always written, `expected.err` is created when a case newly fails, and
//! deleted when a case newly succeeds. A regression can therefore never hide
//! behind a stale file. Always read the resulting `git diff` before committing
//! it: a golden file is the specification of the current behaviour.
//!
//! The suite is one `#[test]` that runs every case and collects every
//! mismatch, so a single run reports the whole story.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The placeholder that replaces the absolute path of `tests/e2e`.
const E2E: &str = "<e2e>";

/// One discovered case.
struct Case {
    /// The directory name, which is also the `[run.<name>]` table name.
    name: String,
    dir: PathBuf,
    /// Extra arguments passed after `cove run <name>`.
    args: Vec<String>,
    /// Variables to set, or to remove when the value is `None`.
    env: Vec<(String, Option<String>)>,
}

/// What one run of the `cove` binary produced.
struct Actual {
    success: bool,
    stdout: String,
    /// Already normalised.
    stderr: String,
}

#[test]
fn every_case_matches_its_golden_files() {
    let root = e2e_root();
    let update = matches!(std::env::var("UPDATE_EXPECT"), Ok(value) if !value.is_empty());

    let names = discover(&root);
    let declared = declared_runs(&root);
    assert!(
        !names.is_empty(),
        "no case directories found under `{}`",
        root.display()
    );

    let mut failures: Vec<String> = Vec::new();

    for name in &names {
        if !declared.contains(name) {
            failures.push(format!(
                "case `{name}`: `tests/e2e/cove.toml` has no `[run.{name}]` table\n  \
                 add one so the case actually runs"
            ));
            continue;
        }
        let case = Case::load(&root, name);
        let actual = case.run(&root);
        if update {
            case.write_goldens(&actual);
        }
        case.check(&actual, &mut failures);
    }

    for name in &declared {
        if !names.contains(name) {
            failures.push(format!(
                "run `{name}`: `tests/e2e/cove.toml` declares `[run.{name}]`, but \
                 `tests/e2e/{name}/main.cove` does not exist"
            ));
        }
    }

    if !failures.is_empty() {
        panic!(
            "{} of {} end-to-end case(s) did not match their golden files:\n\n{}\n\
             re-run with `UPDATE_EXPECT=1 cargo test -p cove-cli --test e2e` to rewrite them.\n",
            failures.len(),
            names.len(),
            failures.join("\n")
        );
    }
}

impl Case {
    /// Reads the optional `args` and `env` files of one case.
    fn load(root: &Path, name: &str) -> Case {
        let dir = root.join(name.replace('.', std::path::MAIN_SEPARATOR_STR));
        let args = read_optional(&dir.join("args"))
            .map(|text| text.lines().map(str::to_string).collect())
            .unwrap_or_default();
        let env = read_optional(&dir.join("env"))
            .map(|text| {
                text.lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(|line| match line.split_once('=') {
                        Some((key, value)) => (key.to_string(), Some(value.to_string())),
                        None => (line.trim().to_string(), None),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Case {
            name: name.to_string(),
            dir,
            args,
            env,
        }
    }

    /// Runs the real `cove` binary from the `tests/e2e` directory.
    fn run(&self, root: &Path) -> Actual {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cove"));
        command.current_dir(root).arg("run").arg(&self.name);
        command.args(&self.args);
        for (key, value) in &self.env {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        let output = command
            .output()
            .unwrap_or_else(|e| panic!("case `{}`: cannot run the `cove` binary: {e}", self.name));
        Actual {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: normalize(&String::from_utf8_lossy(&output.stderr), root),
        }
    }

    /// Rewrites this case's golden files from `actual`.
    ///
    /// `expected.err` is created when the run failed and removed when it
    /// succeeded, so a case that stops failing cannot leave a stale file
    /// behind.
    fn write_goldens(&self, actual: &Actual) {
        write(&self.dir.join("expected.out"), &actual.stdout);
        let err = self.dir.join("expected.err");
        if actual.success {
            if err.exists() {
                fs::remove_file(&err)
                    .unwrap_or_else(|e| panic!("cannot remove `{}`: {e}", err.display()));
            }
        } else {
            write(&err, &actual.stderr);
        }
    }

    /// Compares `actual` against the golden files, collecting every mismatch.
    fn check(&self, actual: &Actual, failures: &mut Vec<String>) {
        let name = &self.name;
        let expected_err = read_optional(&self.dir.join("expected.err"));

        match (&expected_err, actual.success) {
            (None, false) => failures.push(format!(
                "case `{name}`: the run failed, but there is no `{name}/expected.err`\n\
                 {}",
                indent(&actual.stderr)
            )),
            (Some(_), true) => failures.push(format!(
                "case `{name}`: `{name}/expected.err` exists, so the run must fail, \
                 but it exited successfully"
            )),
            _ => {}
        }

        match read_optional(&self.dir.join("expected.out")) {
            Some(expected) if expected != actual.stdout => failures.push(format!(
                "case `{name}`: stdout does not match `{name}/expected.out`\n{}",
                diff(&expected, &actual.stdout)
            )),
            None => failures.push(format!(
                "case `{name}`: `{name}/expected.out` does not exist\n{}",
                indent(&actual.stdout)
            )),
            Some(_) => {}
        }

        match expected_err {
            Some(expected) if expected != actual.stderr => failures.push(format!(
                "case `{name}`: stderr does not match `{name}/expected.err`\n{}",
                diff(&expected, &actual.stderr)
            )),
            None if !actual.stderr.is_empty() => failures.push(format!(
                "case `{name}`: there is no `{name}/expected.err`, so stderr must be empty\n{}",
                indent(&actual.stderr)
            )),
            _ => {}
        }
    }
}

/// The `tests/e2e` directory, with symbolic links resolved so that the child
/// process reports the same absolute paths the harness normalises.
fn e2e_root() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/e2e");
    path.canonicalize()
        .unwrap_or_else(|e| panic!("cannot resolve `{}`: {e}", path.display()))
}

/// Every directory below `root` that holds a `main.cove`, in sorted order.
fn discover(root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    walk(root, root, &mut names);
    names.sort();
    names
}

fn walk(root: &Path, dir: &Path, names: &mut Vec<String>) {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read `{}`: {e}", dir.display()))
        .filter_map(Result::ok);
    let mut paths: Vec<PathBuf> = entries.map(|entry| entry.path()).collect();
    paths.sort();

    for path in paths {
        if !path.is_dir() {
            continue;
        }
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.starts_with('.') || name == "target" {
            continue;
        }
        if path.join("main.cove").is_file() {
            let rel = path.strip_prefix(root).expect("walk stays below the root");
            names.push(
                rel.components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("."),
            );
        }
        walk(root, &path, names);
    }
}

/// The `[run.<name>]` tables declared in `tests/e2e/cove.toml`.
fn declared_runs(root: &Path) -> BTreeSet<String> {
    let path = root.join("cove.toml");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read `{}`: {e}", path.display()));
    text.lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("[run.")?.strip_suffix(']'))
        .map(str::to_string)
        .collect()
}

/// Replaces the absolute path of the `tests/e2e` directory with `<e2e>`.
///
/// Nothing else is normalised: line and column numbers stay exactly as the
/// toolchain reported them.
fn normalize(text: &str, root: &Path) -> String {
    text.replace(&root.display().to_string(), E2E)
}

fn read_optional(path: &Path) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => panic!("cannot read `{}`: {e}", path.display()),
    }
}

fn write(path: &Path, text: &str) {
    fs::write(path, text).unwrap_or_else(|e| panic!("cannot write `{}`: {e}", path.display()));
}

/// A line-by-line report of how `actual` differs from `expected`.
fn diff(expected: &str, actual: &str) -> String {
    let expected: Vec<&str> = expected.split('\n').collect();
    let actual: Vec<&str> = actual.split('\n').collect();
    let mut out = String::new();
    for index in 0..expected.len().max(actual.len()) {
        let line = index + 1;
        match (expected.get(index), actual.get(index)) {
            (Some(a), Some(b)) if a == b => {
                let _ = writeln!(out, "  {line:>3}   {}", escape(a));
            }
            (a, b) => {
                if let Some(a) = a {
                    let _ = writeln!(out, "  {line:>3} - {}", escape(a));
                }
                if let Some(b) = b {
                    let _ = writeln!(out, "  {line:>3} + {}", escape(b));
                }
            }
        }
    }
    let _ = writeln!(out, "  (`-` is expected, `+` is actual)");
    out
}

fn indent(text: &str) -> String {
    let mut out = String::new();
    for line in text.split('\n') {
        let _ = writeln!(out, "      {}", escape(line));
    }
    out
}

/// Makes control characters visible in a report without changing what is
/// compared.
fn escape(line: &str) -> String {
    let mut out = String::new();
    for c in line.chars() {
        match c {
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{{{:x}}}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}
