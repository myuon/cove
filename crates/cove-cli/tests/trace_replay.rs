//! End-to-end tests for `cove trace` and `cove replay`, through the real
//! `cove` binary and a real Cove program.
//!
//! The `restricted` example in `examples/` reads a document and prints a
//! report, so one run of it exercises both a read and an irreversible write
//! across two capabilities. Recording it, reading the recording back, and
//! replaying it is the whole loop the Language Card promises; the divergence
//! cases are what the loop is for.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The `examples/` package at the repository root.
fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// A temporary directory that removes itself when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "cove-trace-replay-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The `tests/e2e` package at the repository root.
fn e2e() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/e2e")
}

/// Runs `cove` with the working directory set to `examples/`.
fn cove(args: &[&str]) -> Output {
    cove_in(&examples(), args)
}

/// Runs `cove` with the working directory set to `dir`.
fn cove_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cove"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("the `cove` binary runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Records a trace of `cove run restricted` into `path`.
fn record(path: &Path) -> String {
    let run = cove(&["run", "restricted", "--trace", &path.display().to_string()]);
    assert!(run.status.success(), "the run failed: {}", stderr(&run));
    assert_eq!(stdout(&run), "5 words\n");
    // The run says once, out loud, that the file it wrote holds host values.
    assert!(
        stderr(&run).contains("may include secrets"),
        "{}",
        stderr(&run)
    );
    std::fs::read_to_string(path).expect("the trace was written")
}

/// Records a trace of `cove run tasks_host_order` into `path`, from the
/// `tests/e2e` shared package.
///
/// `tasks_host_order` spawns two tasks, each making exactly one
/// `console.println` host call, and awaits the first before spawning the
/// second — so the order the two calls reach the host is fixed by the
/// program rather than by the scheduler, and this recording can never flake.
fn record_tasks_host_order(path: &Path) -> String {
    let run = cove_in(
        &e2e(),
        &[
            "run",
            "tasks_host_order",
            "--trace",
            &path.display().to_string(),
        ],
    );
    assert!(run.status.success(), "the run failed: {}", stderr(&run));
    assert_eq!(stdout(&run), "first\nsecond\n");
    std::fs::read_to_string(path).expect("the trace was written")
}

#[test]
fn a_recorded_run_reads_back_and_replays() {
    let dir = TempDir::new("roundtrip");
    let path = dir.join("t.jsonl");
    let recorded = record(&path);
    assert!(
        recorded.starts_with(r#"{"event":"trace_header","version":1,"values":"full","entry":"restricted.main","args":[]}"#),
        "{recorded}"
    );

    let inspect = cove(&["trace", &path.display().to_string()]);
    assert!(
        inspect.status.success(),
        "`cove trace` failed: {}",
        stderr(&inspect)
    );
    let report = stdout(&inspect);
    for expected in [
        "host calls   2 dispatched, 0 refused",
        "irreversible 1 of the 2 dispatched calls cannot be taken back",
        r#"host_call       documents.read("input") [documents] dispatched"#,
        r#"result Ok("Cove grants only narrow authority.\n")"#,
        r#"host_call       console.println("5 words") [console] dispatched"#,
        "not carried by these events",
    ] {
        assert!(report.contains(expected), "missing `{expected}`:\n{report}");
    }

    let replay = cove(&["replay", &path.display().to_string(), "restricted"]);
    assert!(
        replay.status.success(),
        "`cove replay` failed: {}",
        stderr(&replay)
    );
    let played = stdout(&replay);
    assert!(
        played.contains("2 of 2 recorded call(s), answered from the trace"),
        "{played}"
    );
    // Only the boundary is canned, so the console host answered from the
    // trace instead of printing: the program's output is not repeated.
    assert!(!played.contains("5 words"), "{played}");
}

/// The program asks for a call the trace does not have. This is the direction
/// a changed program produces, and the reason to run a replay at all.
#[test]
fn a_program_that_asks_for_a_different_call_diverges() {
    let dir = TempDir::new("diverge-asked");
    let path = dir.join("t.jsonl");
    let recorded = record(&path);
    std::fs::write(&path, recorded.replace("5 words", "4 words")).unwrap();

    let replay = cove(&["replay", &path.display().to_string(), "restricted"]);
    assert!(
        !replay.status.success(),
        "a divergence must fail the replay"
    );
    let report = stderr(&replay);
    for expected in [
        "divergence: the program asked for a different host call",
        "at recorded call   2",
        r#"the trace records  console.println("4 words")"#,
        r#"the program asked  console.println("5 words")"#,
    ] {
        assert!(report.contains(expected), "missing `{expected}`:\n{report}");
    }
}

/// The other direction: the trace records a call the program never makes.
#[test]
fn a_program_that_stops_before_the_trace_does_diverges() {
    let dir = TempDir::new("diverge-unused");
    let path = dir.join("t.jsonl");
    let recorded = record(&path);
    let read = recorded
        .lines()
        .find(|line| line.contains(r#""op":"read""#))
        .expect("the trace records the document read")
        .to_string();
    let mut lines: Vec<&str> = recorded.lines().collect();
    lines.insert(lines.len() - 1, &read);
    std::fs::write(&path, lines.join("\n")).unwrap();

    let replay = cove(&["replay", &path.display().to_string(), "restricted"]);
    assert!(
        !replay.status.success(),
        "a divergence must fail the replay"
    );
    let report = stderr(&replay);
    for expected in [
        "divergence: the program stopped before the trace did",
        "the trace records  3 call(s)",
        "the program made   2",
        r#"the next recorded  documents.read("input")"#,
    ] {
        assert!(report.contains(expected), "missing `{expected}`:\n{report}");
    }
}

/// A reader that does not know a version rejects the trace rather than
/// reading it as one it does know.
#[test]
fn a_trace_from_a_future_version_is_rejected_by_both_commands() {
    let dir = TempDir::new("version");
    let path = dir.join("t.jsonl");
    let recorded = record(&path);
    std::fs::write(&path, recorded.replace(r#""version":1"#, r#""version":99"#)).unwrap();

    let path = path.display().to_string();
    for args in [
        vec!["trace", path.as_str()],
        vec!["replay", path.as_str(), "restricted"],
    ] {
        let output = cove(&args);
        assert!(
            !output.status.success(),
            "an unknown version must be rejected"
        );
        assert!(
            stderr(&output).contains("is version 99, and this build of `cove` reads version 1"),
            "{}",
            stderr(&output)
        );
    }
}

/// A redacted trace is the one to share, and it says plainly that it traded
/// away the ability to replay.
#[test]
fn a_redacted_trace_carries_no_values_and_cannot_be_replayed() {
    let dir = TempDir::new("redacted");
    let path = dir.join("t.jsonl");
    let run = cove(&[
        "run",
        "restricted",
        "--trace",
        &path.display().to_string(),
        "--trace-values",
        "redacted",
    ]);
    assert!(run.status.success(), "the run failed: {}", stderr(&run));
    let recorded = std::fs::read_to_string(&path).unwrap();
    assert!(recorded.contains(r#""values":"redacted""#), "{recorded}");
    assert!(
        !recorded.contains("Cove grants only narrow authority"),
        "{recorded}"
    );
    assert!(!recorded.contains("5 words"), "{recorded}");

    let inspect = cove(&["trace", &path.display().to_string()]);
    assert!(inspect.status.success(), "{}", stderr(&inspect));
    assert!(
        stdout(&inspect).contains("documents.read(<redacted String>)"),
        "{}",
        stdout(&inspect)
    );

    let replay = cove(&["replay", &path.display().to_string(), "restricted"]);
    assert!(
        !replay.status.success(),
        "a redacted trace cannot be replayed"
    );
    assert!(
        stderr(&replay).contains("carries no values to replay"),
        "{}",
        stderr(&replay)
    );
}

/// A trace recorded from one entry cannot stand in for another.
#[test]
fn replaying_a_trace_against_a_different_entry_is_refused() {
    let dir = TempDir::new("wrong-entry");
    let path = dir.join("t.jsonl");
    record(&path);

    let replay = cove(&["replay", &path.display().to_string(), "hello"]);
    assert!(!replay.status.success(), "the entries do not match");
    assert!(
        stderr(&replay).contains(
            "was recorded from `restricted.main`, but `[run.hello] entry` is `hello.main`"
        ),
        "{}",
        stderr(&replay)
    );
}

/// `--capability` narrows the timeline to one capability's calls.
#[test]
fn the_timeline_can_be_filtered_by_capability() {
    let dir = TempDir::new("filter");
    let path = dir.join("t.jsonl");
    record(&path);

    let inspect = cove(&[
        "trace",
        &path.display().to_string(),
        "--capability",
        "console",
    ]);
    assert!(inspect.status.success(), "{}", stderr(&inspect));
    let report = stdout(&inspect);
    let timeline = report.split("timeline").nth(1).expect("a timeline");
    assert!(timeline.contains("console.println"), "{report}");
    assert!(!timeline.contains("documents.read"), "{report}");
}

/// A resource handle is recorded and replayed like any other value, because
/// that is all it is: a name.
///
/// `tests/e2e/host_http_resource` opens a listener, asks it for its port, and
/// closes it — creation, use, and closure, which is the whole life of a
/// handle. The replay opens no socket at all: `http.listen` is answered with
/// the identity the trace recorded, and the two calls made on that identity
/// are matched against the recorded ones, handle included.
#[test]
fn a_resource_handle_is_recorded_and_replayed_by_the_name_it_is() {
    let dir = TempDir::new("resource");
    let path = dir.join("t.jsonl");
    let trace = path.display().to_string();

    let run = cove_in(&e2e(), &["run", "host_http_resource", "--trace", &trace]);
    assert!(run.status.success(), "the run failed: {}", stderr(&run));
    let recorded = std::fs::read_to_string(&path).expect("the trace was written");

    // Creation records the identity the host issued, and the calls made on it
    // record that identity as their first argument.
    assert!(
        recorded.contains(r#""op":"listen""#)
            && recorded.contains(r#"{"type":"resource","name":"http.Server","id":1}"#),
        "{recorded}"
    );
    assert!(recorded.contains(r#""op":"Server.port""#), "{recorded}");
    assert!(recorded.contains(r#""op":"Server.close""#), "{recorded}");

    let inspect = cove_in(&e2e(), &["trace", &trace]);
    assert!(inspect.status.success(), "{}", stderr(&inspect));
    let report = stdout(&inspect);
    assert!(report.contains("http.Server.close"), "{report}");

    let replay = cove_in(&e2e(), &["replay", &trace, "host_http_resource"]);
    assert!(
        replay.status.success(),
        "the replay diverged: {}",
        stderr(&replay)
    );
    assert!(
        stdout(&replay).contains("host calls  5 of 5 recorded call(s), answered from the trace"),
        "{}",
        stdout(&replay)
    );
}

/// Boundary 6: concurrent Host-call ordering.
///
/// `crates/cove-cli/src/replay.rs`'s module doc names the reason this
/// boundary exists at all: ADR 0008 runs each spawned task on a thread of its
/// own, so "the order in which two concurrent tasks reach the host is the
/// scheduler's, and a trace records the order one run happened to take" —
/// and it is why "a scope's contract is the set of effects it produces and
/// never their sequence." `tests/e2e/tasks_host_order` deliberately does not
/// exercise that freedom: it awaits its first task before spawning its
/// second, so there is exactly one order its two `console.println` calls can
/// reach the host in, and recording it can never flake.
#[test]
fn a_recorded_task_scope_replays_the_host_calls_it_made() {
    let dir = TempDir::new("host-order");
    let path = dir.join("t.jsonl");
    let recorded = record_tasks_host_order(&path);

    // The trace really did record the two `console.println` calls, in the
    // program's order: `first` before `second`.
    let calls: Vec<&str> = recorded
        .lines()
        .filter(|line| line.contains(r#""event":"host_call""#))
        .collect();
    assert_eq!(calls.len(), 2, "{recorded}");
    assert!(
        calls[0].contains(r#""args":[{"type":"string","value":"first"}]"#),
        "{recorded}"
    );
    assert!(
        calls[1].contains(r#""args":[{"type":"string","value":"second"}]"#),
        "{recorded}"
    );

    let replay = cove_in(
        &e2e(),
        &["replay", &path.display().to_string(), "tasks_host_order"],
    );
    assert!(
        replay.status.success(),
        "`cove replay` failed: {}",
        stderr(&replay)
    );
    let played = stdout(&replay);
    assert!(
        played.contains("host calls  2 of 2 recorded call(s), answered from the trace"),
        "{played}"
    );
    // Only the boundary is canned, so `console` answered from the trace
    // instead of printing: the program's own output is not repeated.
    assert!(!played.contains("first"), "{played}");
    assert!(!played.contains("second"), "{played}");
}

/// The other half of boundary 6: a trace whose two concurrent calls are
/// reordered is exactly the trace a different interleaving of the same two
/// tasks would have produced — same events, different order — which is a
/// deterministic stand-in for a scheduler race that a real thread
/// interleaving is not. `crates/cove-cli/src/replay.rs`'s module doc calls
/// this "the truth about the program rather than a defect in the replay."
///
/// The swap touches only which of the two `host_call` lines comes first, and
/// this test proves that: the swapped trace has the same lines, sorted, as
/// the original, so nothing about either call's content changed.
#[test]
fn a_trace_whose_concurrent_calls_are_reordered_diverges_and_says_where() {
    let dir = TempDir::new("host-order-reordered");
    let path = dir.join("t.jsonl");
    let recorded = record_tasks_host_order(&path);

    let mut lines: Vec<&str> = recorded.lines().collect();
    let host_call_indices: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(r#""event":"host_call""#))
        .map(|(i, _)| i)
        .collect();
    let [first, second] = host_call_indices.as_slice() else {
        panic!("expected exactly two host_call lines:\n{recorded}");
    };
    lines.swap(*first, *second);
    let swapped = lines.join("\n") + "\n";

    // The technique: a reordering, not an edit. Sorted, the two traces have
    // exactly the same lines.
    let mut original_sorted: Vec<&str> = recorded.lines().collect();
    let mut swapped_sorted: Vec<&str> = swapped.lines().collect();
    original_sorted.sort();
    swapped_sorted.sort();
    assert_eq!(original_sorted, swapped_sorted, "only the order may differ");
    assert_ne!(recorded, swapped, "the order must actually differ");

    std::fs::write(&path, &swapped).unwrap();

    let replay = cove_in(
        &e2e(),
        &["replay", &path.display().to_string(), "tasks_host_order"],
    );
    assert!(!replay.status.success(), "a reordered trace must diverge");
    let report = stderr(&replay);
    for expected in [
        "divergence: the program asked for a different host call",
        "at recorded call   1",
        r#"the trace records  console.println("second")"#,
        r#"the program asked  console.println("first")"#,
    ] {
        assert!(report.contains(expected), "missing `{expected}`:\n{report}");
    }
    // The trailing `rule` block, which every divergence carries.
    for expected in [
        "rule               a replay answers every Host API call from the trace, in the",
        "recorded order; the program's own computation runs for real,",
        "so a divergence means it took a different path than it did",
    ] {
        assert!(report.contains(expected), "missing `{expected}`:\n{report}");
    }
}
