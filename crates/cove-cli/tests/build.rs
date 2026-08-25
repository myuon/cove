//! `cove build`, end to end.
//!
//! Everything ADR 0009 promises about the artifact is a claim about a real
//! executable running somewhere else, so this suite builds them with the real
//! `cove` binary and runs them: in a directory holding no `cove.toml`, no
//! source, and nothing else of the package they came from. `examples/hello`
//! answers whether a built binary runs at all, and `tests/e2e`'s two task
//! cases answer whether one still runs what it spawns.
//!
//! The build is a `cargo build --release` of a generated crate, so it takes
//! Rust's build time rather than a test's. It is not `#[ignore]`d: nothing
//! else here can tell whether `cove build` produces something that runs.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// A package in this repository, resolved so that the child process reports
/// the same absolute paths this suite passes it.
fn package(relative: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative);
    path.canonicalize()
        .unwrap_or_else(|e| panic!("cannot resolve `{}`: {e}", path.display()))
}

/// Builds `[run.<name>]` of the package at `relative` with the real `cove`
/// binary, and answers with the executable it wrote and what the build
/// reported.
fn build(relative: &str, name: &str) -> (PathBuf, String) {
    let root = package(relative);
    let out = root.join(format!("target/{name}-e2e"));
    let output = Command::new(env!("CARGO_BIN_EXE_cove"))
        .current_dir(&root)
        .args(["build", name, "--out"])
        .arg(&out)
        .output()
        .expect("`cove build` starts");
    assert!(
        output.status.success(),
        "`cove build {name}` failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(out.is_file(), "`{}` was not written", out.display());
    (out, String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Builds `examples/hello` once for the whole suite, since two tests read
/// the one executable and building it is Rust's build time, not a test's.
fn built_hello() -> &'static Path {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        let (out, summary) = build("examples", "hello");
        assert!(
            summary.contains("entry:  hello.main") && summary.contains("grants: console"),
            "the build must report the boundary it baked in:\n{summary}"
        );
        out
    })
}

#[test]
fn a_built_binary_runs_where_nothing_of_its_package_is() {
    let dir = TempDir::new("runs-alone");
    let program = dir.install(built_hello());

    // Nothing of the package: no `cove.toml`, no `.cove` file, no `cove`.
    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("the directory exists")
            .count(),
        1,
        "the binary must be the only thing in the directory it runs from"
    );

    let outcome = run(&program, dir.path(), &[]);
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "Hello, world!\n");
    assert_eq!(outcome.stderr, "");

    // The arguments it accepts are its program's own, and they reach the
    // entry function exactly as `cove run hello -- Ada` delivers them.
    let outcome = run(&program, dir.path(), &["Ada"]);
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "Hello, Ada!\n");
}

#[test]
fn a_cove_toml_beside_a_built_binary_changes_nothing() {
    let dir = TempDir::new("hostile-config");
    let program = dir.install(built_hello());

    // Every line of this would change the run if it were read: a different
    // entry, a capability `[run.hello]` never granted, and a fuel limit no
    // program can finish under. `cove run` would honour all three.
    std::fs::write(
        dir.path().join("cove.toml"),
        "[run.hello]\nentry = \"hello.greeting\"\nallow = [\"console\", \"files\", \"process\"]\nfuel = 1\n",
    )
    .expect("the config is written");

    let outcome = run(&program, dir.path(), &[]);
    assert!(
        outcome.status.success(),
        "a `cove.toml` beside the binary must not stop it:\n{}",
        outcome.stderr
    );
    assert_eq!(
        outcome.stdout, "Hello, world!\n",
        "the entry and the limits are the ones the build baked in"
    );
    assert_eq!(outcome.stderr, "");

    // The binary also honours no flag of its own, so there is no way to ask
    // it for the authority the file beside it offers: `--files-root` is a
    // program argument, and this program greets it.
    let outcome = run(&program, dir.path(), &["--files-root", "/"]);
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert_eq!(outcome.stdout, "Hello, --files-root!\n");
}

/// ADR 0008 runs each spawned task on a thread of its own, and a built
/// binary is the same runtime, so it has to spawn them too. This is the
/// claim a compile error cannot make for us: `Embedded::main` builds its own
/// `Runtime`, and a run that never left the entry's thread would still link.
#[test]
fn a_built_binary_runs_the_tasks_its_program_spawns() {
    // `tasks_shared` counts from two tasks into one `Shared`, so its answer
    // is only right if both tasks ran and neither lost a count to the other.
    let (program, _) = build("tests/e2e", "tasks_shared");
    let dir = TempDir::new("tasks-shared");
    let program = dir.install(&program);
    let outcome = run(&program, dir.path(), &[]);
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        "requests=100 failures=50
"
    );

    // `tasks_scope` spawns two 300ms waits and reports whether the scope
    // finished in less than their sum, so it distinguishes tasks that ran on
    // threads from tasks that were run one after another.
    let (program, _) = build("tests/e2e", "tasks_scope");
    let dir = TempDir::new("tasks-scope");
    let program = dir.install(&program);
    let outcome = run(&program, dir.path(), &[]);
    assert!(outcome.status.success(), "{}", outcome.stderr);
    assert_eq!(
        outcome.stdout,
        "1 2
both waits overlapped true
"
    );
}

/// What running a built binary produced.
struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

fn run(program: &Path, dir: &Path, args: &[&str]) -> Run {
    let output = Command::new(program)
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("cannot run `{}`: {e}", program.display()));
    Run {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// A temporary directory that removes itself when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "cove-build-e2e-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the clock is after the epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("the temporary directory is created");
        TempDir(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// Copies `program` into this directory, keeping it executable.
    fn install(&self, program: &Path) -> PathBuf {
        let installed = self
            .0
            .join(program.file_name().expect("the build named it"));
        std::fs::copy(program, &installed).expect("the executable is copied");
        installed
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
