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
use std::sync::{Mutex, OnceLock};

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
    build_on(relative, name, &[], name)
}

/// [`build`], with `extra` flags and a suffix of its own for the executable,
/// so that two builds of the same run do not overwrite each other.
fn build_on(relative: &str, name: &str, extra: &[&str], suffix: &str) -> (PathBuf, String) {
    // One build at a time. The generated crate lives at
    // `target/cove-build/<name>/`, keyed by the run rather than by anything
    // this suite chooses, so two builds of one run — the same program on the
    // two backends — write the same directory and `emit` clears it. Cargo's
    // own lock does not help: the race is in the crate, before Cargo sees
    // it.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _building = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let root = package(relative);
    let out = root.join(format!("target/{suffix}-e2e"));
    let output = Command::new(env!("CARGO_BIN_EXE_cove"))
        .current_dir(&root)
        .args(["build", name])
        .args(extra)
        .arg("--out")
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
            summary.contains("entry:   hello.main")
                && summary.contains("backend: lvm")
                && summary.contains("grants:  console"),
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

/// A built binary embeds an evaluator, and this is what choosing the other
/// one must not change: the same program, built both ways, answering the same
/// thing.
///
/// It is the only test in this file that names a backend. Everything else
/// here builds the default one, which is the point — a suite that named the
/// backend everywhere would stop noticing the day the default moved again.
#[test]
fn the_same_program_built_on_either_backend_answers_the_same() {
    let (on_the_oracle, summary) =
        build_on("examples", "hello", &["--backend", "ast"], "hello-ast");
    assert!(
        summary.contains("backend: ast"),
        "a build says which backend it baked in:\n{summary}"
    );

    let dir = TempDir::new("both-backends");
    let interpreted = dir.install(&on_the_oracle);
    let interpreted = run(&interpreted, dir.path(), &["Ada"]);

    let dir = TempDir::new("both-backends-lowered");
    let lowered = dir.install(built_hello());
    let lowered = run(&lowered, dir.path(), &["Ada"]);

    assert_eq!(interpreted.stdout, lowered.stdout);
    assert_eq!(interpreted.stderr, lowered.stderr);
    assert_eq!(
        interpreted.status.success(),
        lowered.status.success(),
        "{}",
        lowered.stderr
    );
}

/// ADR 0009 says a built binary "must not defer an error to whoever runs
/// it", and a program the lowering will not accept is such an error: the
/// person who can act on it is holding the source, and they are not the one
/// holding the binary. So it stops the build, and nothing is written.
///
/// The program it stops on is `crates/cove-cli/tests/fixtures/`'s, and which
/// program that is took some care. ADR 0034 leaves the backend no admission
/// predicate at all, so almost nothing a checked program can contain will
/// stop the lowering — a construct it has not been taught is a bug in the
/// lowering and not a program it declines. What is left is the one refusal
/// `docs/LINEAR_VM.md` argues is permanent: a generic chain that asks for a
/// wider type at every step has no finite set of functions to lower it to,
/// and no later task removes that. A fixture built on a gap would stop being
/// a fixture the day the gap was filled.
///
/// It lives under `tests/fixtures/` rather than in `tests/e2e/` because the
/// coverage harnesses walk `tests/e2e/`, `examples/` and `benches/` and count
/// a program that does not lower — and this one is there precisely because it
/// does not.
///
/// No `cargo build` happens here, because the failure comes before one is
/// started, which is most of why failing at build time is worth doing.
#[test]
fn a_program_that_cannot_be_lowered_is_refused_before_a_binary_is_written() {
    let root = package("crates/cove-cli/tests/fixtures/instantiation_depth");
    let out = root.join("target/instantiation-depth-e2e");
    let _ = std::fs::remove_file(&out);
    let output = Command::new(env!("CARGO_BIN_EXE_cove"))
        .current_dir(&root)
        .args(["build", "app", "--out"])
        .arg(&out)
        .output()
        .expect("`cove build` starts");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("cove::lower::instantiation_depth"),
        "{stderr}"
    );
    assert!(
        stderr.contains("no finite set of functions to lower it to"),
        "{stderr}"
    );
    assert!(
        !out.exists(),
        "a binary that could not start must never be written"
    );
}

/// ADR 0009 says a built binary's grants cannot be widened after the fact:
/// "An embedded grant set is the point. A built binary carries the authority
/// its `[run.<name>]` table granted and cannot be handed more by editing a
/// file beside it." `a_cove_toml_beside_a_built_binary_changes_nothing`
/// proves that a neighbouring `cove.toml` cannot change the entry or the
/// limits, but every capability that test's `cove.toml` names is one `hello`
/// already holds, so a binary that quietly read the neighbouring `allow`
/// list would still pass it. This test asks for a capability the binary was
/// never granted, `files`, from a program that would otherwise have been
/// able to read the file it names — the failure below can only be the
/// missing grant, never a missing file.
#[test]
fn a_cove_toml_beside_a_built_binary_grants_it_no_capability() {
    let (program, _) = build("tests/e2e", "fail_files_no_capability");
    let dir = TempDir::new("sealed-grants");
    let program = dir.install(&program);

    // A file the `files` capability would have let the program read, so a
    // refusal below is about the missing grant, not about a missing file.
    std::fs::create_dir(dir.path().join("files")).expect("the files directory is created");
    std::fs::write(dir.path().join("files").join("notes.txt"), "the notes")
        .expect("the note is written");

    // Widens `allow` to include `files`, which the binary was never built
    // with. This is the config source `cove run` would honour; the binary
    // was sealed before this file existed.
    std::fs::write(
        dir.path().join("cove.toml"),
        "[run.fail_files_no_capability]\nentry = \"fail_files_no_capability.main\"\nallow = [\"console\", \"files\"]\n",
    )
    .expect("the config is written");

    let outcome = run(&program, dir.path(), &[]);
    assert!(
        !outcome.status.success(),
        "a `cove.toml` beside the binary must not grant it `files`:\n{}",
        outcome.stdout
    );
    // The capability the binary was sealed with, `console`, still works: the
    // refusal below is about `files` alone.
    assert_eq!(outcome.stdout, "console was granted\n");
    assert!(
        outcome.stderr.contains(
            "`files.read` requires the `files` capability, which this run was not granted"
        ),
        "{}",
        outcome.stderr
    );
    // The sealed help line, not the config one `cove run` would print for the
    // same refusal: this is what shows the sealed code path ran.
    assert!(
        outcome.stderr.contains(
            "  help: this binary carries the capabilities it was built with; add `files` to `allow` in the run's `cove.toml` table and build it again\n"
        ),
        "{}",
        outcome.stderr
    );
    assert!(
        !outcome
            .stderr
            .contains("  help: add `files` to `allow` in the run's `cove.toml` table\n"),
        "{}",
        outcome.stderr
    );
    // A package-relative path, not the build machine's absolute one: this
    // binary can run somewhere that path never existed.
    assert!(
        outcome
            .stderr
            .contains(" --> fail_files_no_capability/main.cove:7:27"),
        "{}",
        outcome.stderr
    );
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

/// Runs `program`, retrying while Linux reports the executable as busy.
///
/// These tests run in threads and each spawns processes. A child forked by
/// one test inherits the write descriptor another test still holds on the
/// executable it is copying, and Linux refuses to exec a file any process has
/// open for writing. The descriptor closes on its own; nothing here can wait
/// for a fork it did not make, so this waits for the symptom.
fn run(program: &Path, dir: &Path, args: &[&str]) -> Run {
    let mut attempt = 0;
    let output = loop {
        match Command::new(program).current_dir(dir).args(args).output() {
            Ok(output) => break output,
            Err(e) if e.raw_os_error() == Some(26) && attempt < 50 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("cannot run `{}`: {e}", program.display()),
        }
    };
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
