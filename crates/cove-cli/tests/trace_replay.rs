//! End-to-end tests for `cove trace` and `cove replay`, through the real
//! `cove` binary and a real Cove program.
//!
//! The `restricted` example in `examples/` reads a document and prints a
//! report, so one run of it exercises both a read and an irreversible write
//! across two capabilities. Recording it, reading the recording back, and
//! replaying it is the whole loop the Language Card promises; the divergence
//! cases are what the loop is for.
//!
//! Several cases here run the loop across the two backends, which is the half
//! of [issue #111](https://github.com/myuon/cove/issues/111)'s "replay/state
//! result" that a replay can answer at all — the other half, comparing what
//! two backends make of one program, is `tests/differential.rs`'s. Since
//! ADR 0023 both directions exist, and the four combinations of a backend
//! that records and a backend that replays are covered here rather than
//! there: `tests/differential.rs` runs its corpus in process, against
//! `Interpreter` and `Vm` directly, and a replay is a thing the `cove` binary
//! does with a file. What that harness stands in for is the tape's contents
//! over ninety-three programs; what only these cases reach is a run driven by
//! that file.

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

/// The one-program package holding the construct the lowering refuses.
///
/// It is its own package rather than a run in `tests/e2e/` because the case
/// it pins is a command that fails, and the e2e harness runs it as such.
fn backend_unsupported() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/e2e/backend_unsupported")
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

/// Records a trace of `cove run restricted` into `path`, on whatever
/// backend `cove run` runs a program on — the VM, since ADR 0022.
///
/// Deliberately not pinned to a backend. Almost every test below is about
/// what a trace holds and what a replay makes of it, and the recording it
/// reads should be the one a person gets by typing `cove run --trace`.
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

/// Records a trace of `cove run restricted --backend ast` into `path`.
///
/// The same program and the same fakes as [`record`], differing only in which
/// backend ran it, so the two files can be compared line for line. This is
/// the half that has to name a backend now: the default is the VM, so the
/// oracle is what has to be asked for.
fn record_on_the_interpreter(path: &Path) -> String {
    let run = cove(&[
        "run",
        "restricted",
        "--backend",
        "ast",
        "--trace",
        &path.display().to_string(),
    ]);
    assert!(run.status.success(), "the run failed: {}", stderr(&run));
    assert_eq!(stdout(&run), "5 words\n");
    std::fs::read_to_string(path).expect("the trace was written")
}

/// Every combination of a backend that records and a backend that replays.
///
/// This is the cross-backend half of what issue #111 asks for under
/// "replay/state result", and until ADR 0023 only one of its two directions
/// existed: `cove replay` built a `cove_runtime::interp::Interpreter` and
/// took no `--backend`, so a trace could be recorded on the VM and replayed
/// on the interpreter and not the other way about. Both directions are here
/// now, along with the two that stay on one backend, because a replay's
/// value depends on which backend read the tape and there are four answers
/// rather than two.
///
/// Replaying on the oracle is the direction with something to prove: a
/// recording the oracle cannot follow is the VM having asked for something
/// the language does not say it should, and the replay reports it as a
/// divergence with both calls shown. The direction ADR 0023 added catches the
/// converse — a VM that asks for a call the interpreter's recording does not
/// hold. What has stood in for it is `tests/differential.rs`, which compares
/// every host call's module, operation, arguments and outcome, in order, over
/// every case that lowers; that is the same tape a replay reads, checked over
/// ninety-three programs rather than one. What it does not stand in for is a
/// run driven by a file instead of by a host, which is the only thing a
/// replay does that a second run does not — and that is exactly what the two
/// cross-backend cells below do.
///
/// All four succeed today. `restricted` reads a document and prints a report,
/// and the two backends ask for those two host calls in the same order with
/// the same arguments whichever of them is driven by a file.
#[test]
fn a_recording_replays_on_either_backend_from_either_backend() {
    let dir = TempDir::new("cross-backend");
    let on_the_vm = dir.join("vm.jsonl");
    let on_the_interpreter = dir.join("ast.jsonl");
    let recorded = record(&on_the_vm);
    let oracle = record_on_the_interpreter(&on_the_interpreter);

    // The two recordings are one recording once the wall-clock figures are
    // taken out of them, which is the claim `cove replay` is about to rest
    // on: what it reads is a file, and the two backends wrote the same file.
    assert_eq!(
        without_wall_clock(&recorded),
        without_wall_clock(&oracle),
        "the two backends recorded different traces of one program"
    );

    for (recorded_on, trace, replayed_on) in [
        // The ordinary case since ADR 0022: `cove run --trace` then `cove
        // replay`, neither of them naming a backend, both of them the VM's.
        ("vm", &on_the_vm, None),
        // The direction ADR 0023 added, spelled out rather than defaulted.
        ("ast", &on_the_interpreter, Some("vm")),
        // The direction that worked before it, which must keep working — and
        // which now has to be asked for, because the default moved.
        ("vm", &on_the_vm, Some("ast")),
        ("ast", &on_the_interpreter, Some("ast")),
    ] {
        let path = trace.display().to_string();
        let mut args = vec!["replay", path.as_str(), "restricted"];
        if let Some(backend) = replayed_on {
            args.extend(["--backend", backend]);
        }
        let replay = cove(&args);
        assert!(
            replay.status.success(),
            "a recording made on `{recorded_on}` failed to replay on `{}`: {}",
            replayed_on.unwrap_or("vm"),
            stderr(&replay)
        );
        let played = stdout(&replay);
        assert!(
            played.contains("2 of 2 recorded call(s), answered from the trace"),
            "{played}"
        );
        // The replay says which backend read the tape, because the file does
        // not say which backend wrote it and a divergence's meaning turns on
        // both.
        assert!(
            played.contains(&format!("backend     {}", replayed_on.unwrap_or("vm"))),
            "{played}"
        );
        assert!(
            played.contains("a trace does not record which backend recorded it"),
            "{played}"
        );
    }
}

/// ADR 0019's no-silent-fallback rule, read across to a command that calls no
/// host.
///
/// `tests/e2e/backend_unsupported` is the program the lowering refuses, and
/// its own `expected.err` pins what `cove run` says about it. A replay has no
/// side effect for a refusal to come before — it makes no host call at all —
/// so what the rule protects here is the verdict: a replay that quietly
/// finished on the interpreter would report "replayed", or a divergence,
/// about a backend nobody asked for.
///
/// The recording has to be made on the interpreter, since the VM cannot run
/// this program at all. That is the point twice over: the trace exists, it is
/// perfectly replayable, and `--backend vm` still refuses it rather than
/// reading it on the backend that could.
#[test]
fn a_replay_on_the_vm_of_a_program_the_lowering_refuses_is_refused() {
    let dir = TempDir::new("unsupported");
    let path = dir.join("t.jsonl");
    let trace = path.display().to_string();
    let root = backend_unsupported();

    let run = cove_in(
        &root,
        &[
            "run",
            "backend_unsupported",
            "--backend",
            "ast",
            "--trace",
            &trace,
        ],
    );
    assert!(run.status.success(), "the run failed: {}", stderr(&run));

    let refused = cove_in(&root, &["replay", &trace, "backend_unsupported"]);
    assert!(
        !refused.status.success(),
        "a replay on the VM of a program the lowering refuses must refuse"
    );
    let report = stderr(&refused);
    for expected in [
        "error[cove::backend::unsupported]: the VM cannot yet run a function declared inside a function body",
        "it never falls back to the interpreter",
        // The flag this points at is one `cove replay` now has, so the help
        // is advice rather than a dead end.
        "help: run it on the interpreter with `--backend ast`",
    ] {
        assert!(report.contains(expected), "missing `{expected}`:\n{report}");
    }
    // Refused rather than replayed: nothing about the tape was reported.
    assert!(!report.contains("recorded call(s)"), "{report}");
    assert!(
        !stdout(&refused).contains("replayed"),
        "{}",
        stdout(&refused)
    );

    // And the flag the help names does run it, on the very same recording.
    let played = cove_in(
        &root,
        &["replay", &trace, "backend_unsupported", "--backend", "ast"],
    );
    assert!(
        played.status.success(),
        "the flag the refusal points at must work: {}",
        stderr(&played)
    );
    assert!(
        stdout(&played).contains("2 of 2 recorded call(s), answered from the trace"),
        "{}",
        stdout(&played)
    );
}

/// `--backend` is one flag with one spelling, and everything else beginning
/// with `--` is still a flag this command does not have.
///
/// The unknown-flag check is older than the flag and has to keep working:
/// a typo that fell through to a positional would be read as a trace path.
#[test]
fn the_backend_flag_is_spelled_as_it_is_everywhere_else() {
    let dir = TempDir::new("flags");
    let path = dir.join("t.jsonl");
    record(&path);
    let trace = path.display().to_string();

    let unknown = cove(&["replay", &trace, "restricted", "--jit"]);
    assert!(!unknown.status.success(), "an unknown flag must be refused");
    assert!(
        stderr(&unknown).contains("unknown `cove replay` flag `--jit`"),
        "{}",
        stderr(&unknown)
    );

    // The same sentence `cove run`, `cove test`, `cove generate`, and `cove
    // build` refuse an unknown backend with.
    let nonsense = cove(&["replay", &trace, "restricted", "--backend", "jit"]);
    assert!(
        !nonsense.status.success(),
        "an unknown backend must be refused rather than defaulted"
    );
    assert!(
        stderr(&nonsense).contains("`--backend` must be `ast` or `vm`, found `jit`"),
        "{}",
        stderr(&nonsense)
    );

    let bare = cove(&["replay", &trace, "restricted", "--backend"]);
    assert!(!bare.status.success(), "`--backend` needs a value");
    assert!(
        stderr(&bare).contains("`--backend` needs a value: `ast` or `vm`"),
        "{}",
        stderr(&bare)
    );
}

/// A trace with the figure under every `_ns` key replaced, so that two
/// recordings of one program can be compared.
///
/// Every `Duration` a trace carries — `cpu`, `wait`, `pause` — is wall time,
/// and two runs of one program on one backend disagree about all three.
fn without_wall_clock(trace: &str) -> String {
    let mut out = String::with_capacity(trace.len());
    let mut rest = trace;
    while let Some(at) = rest.find("_ns\":") {
        let (head, tail) = rest.split_at(at + "_ns\":".len());
        out.push_str(head);
        out.push_str("<wall clock>");
        let end = tail
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(tail.len());
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

#[test]
fn a_recorded_run_reads_back_and_replays() {
    let dir = TempDir::new("roundtrip");
    let path = dir.join("t.jsonl");
    let recorded = record(&path);
    assert!(
        recorded.starts_with(r#"{"event":"trace_header","version":2,"values":"full","entry":"restricted.main","args":[]}"#),
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
        // Every call this run made, it made itself: the entry is a task with
        // an id like any other, and that is what the calls are attributed to.
        "by the entry",
        "outcome      success",
        "run_ended       success",
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
    std::fs::write(&path, recorded.replace(r#""version":2"#, r#""version":99"#)).unwrap();

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
            stderr(&output).contains("is version 99, and this build of `cove` reads version 2"),
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
    // program's order: `first` before `second`, each under the id of the task
    // that made it.
    let calls: Vec<&str> = recorded
        .lines()
        .filter(|line| line.contains(r#""event":"host_call""#))
        .collect();
    assert_eq!(calls.len(), 2, "{recorded}");
    assert!(
        calls[0].contains(r#""task":1"#)
            && calls[0].contains(r#""args":[{"type":"string","value":"first"}]"#),
        "{recorded}"
    );
    assert!(
        calls[1].contains(r#""task":2"#)
            && calls[1].contains(r#""args":[{"type":"string","value":"second"}]"#),
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

/// The question a concurrent trace could not answer before: which task did
/// this I/O.
///
/// `tests/e2e/tasks_host_order` spawns two tasks that each make exactly one
/// host call, so the recording has two calls from two tasks and nothing else
/// to confuse them. The summary separates them, and `--task` selects one
/// task's calls along with its lifecycle.
#[test]
fn a_concurrent_trace_can_be_grouped_by_the_task_that_made_each_host_call() {
    let dir = TempDir::new("host-by-task");
    let path = dir.join("t.jsonl");
    record_tasks_host_order(&path);
    let trace = path.display().to_string();

    let inspect = cove_in(&e2e(), &["trace", &trace]);
    assert!(inspect.status.success(), "{}", stderr(&inspect));
    let report = stdout(&inspect);
    for expected in [
        "by task",
        "task 1     1 dispatched, 0 refused, 1 irreversible",
        "task 2     1 dispatched, 0 refused, 1 irreversible",
        r#"host_call       console.println("first") [console] dispatched, by task 1"#,
        r#"host_call       console.println("second") [console] dispatched, by task 2"#,
    ] {
        assert!(report.contains(expected), "missing `{expected}`:\n{report}");
    }
    // The summary no longer ends by admitting it cannot say whose call was
    // whose, because it now can.
    assert!(!report.contains("which task made a host call"), "{report}");

    let one = cove_in(&e2e(), &["trace", &trace, "--task", "1"]);
    assert!(one.status.success(), "{}", stderr(&one));
    let timeline = stdout(&one)
        .split("timeline")
        .nth(1)
        .expect("a timeline")
        .to_string();
    assert!(
        timeline.contains(r#"console.println("first")"#),
        "{timeline}"
    );
    assert!(
        !timeline.contains(r#"console.println("second")"#),
        "{timeline}"
    );
    assert!(timeline.contains("task_spawned    1"), "{timeline}");
    assert!(timeline.contains("task_completed  1"), "{timeline}");
}

/// Every run has a terminal outcome, whichever way it ended, and the
/// classification is the name a reader groups runs by.
#[test]
fn every_run_records_how_it_ended() {
    let dir = TempDir::new("outcome");
    // A limit the program cannot get past, an entry that returns `Err`, and a
    // capability the run was not granted: one of each family the
    // classification names.
    let cases: [(&str, &[&str], &str); 4] = [
        ("success", &["run", "hello"], "\"outcome\":\"success\""),
        (
            "deadline",
            &["run", "restricted", "--deadline", "1ns"],
            "\"outcome\":\"deadline\"",
        ),
        (
            "fuel",
            &["run", "restricted", "--fuel", "5"],
            "\"outcome\":\"fuel\"",
        ),
        (
            "host-calls",
            &["run", "restricted", "--max-host-calls", "0"],
            "\"outcome\":\"host_calls\"",
        ),
    ];
    for (name, args, expected) in cases {
        let path = dir.join(&format!("{name}.jsonl"));
        let trace = path.display().to_string();
        let mut argv = args.to_vec();
        argv.extend(["--trace", trace.as_str()]);
        cove(&argv);
        let recorded = std::fs::read_to_string(&path).expect("the trace was written");
        let last = recorded
            .lines()
            .last()
            .expect("a trace has at least a header");
        assert!(
            last.contains("\"event\":\"run_ended\"") && last.contains(expected),
            "`{name}` should end `{expected}`, and it must be the last line:\n{recorded}"
        );
    }
}

/// The other two families, from the `tests/e2e` package: an entry that
/// returned `Err`, and one the Host API boundary refused.
#[test]
fn a_program_s_own_failure_and_the_boundary_s_are_different_outcomes() {
    let dir = TempDir::new("outcome-e2e");
    for (run, expected, said) in [
        (
            "fail_entry_error",
            "\"outcome\":\"error\"",
            "the requested report is not available",
        ),
        (
            "fail_no_capability",
            "\"outcome\":\"host_boundary\"",
            "requires the `console` capability",
        ),
        (
            "fail_divide_by_zero",
            "\"outcome\":\"invariant\"",
            "division by zero",
        ),
    ] {
        let path = dir.join(&format!("{run}.jsonl"));
        let trace = path.display().to_string();
        cove_in(&e2e(), &["run", run, "--trace", &trace]);
        let recorded = std::fs::read_to_string(&path).expect("the trace was written");
        let last = recorded.lines().last().expect("a terminal line");
        assert!(last.contains(expected), "{recorded}");
        assert!(last.contains(said), "{recorded}");
    }
}

/// A replay that ends differently than the recording did is the program
/// saying it would behave differently, which is what a replay is for.
#[test]
fn a_replay_that_ends_differently_than_the_recording_diverges() {
    let dir = TempDir::new("diverge-ending");
    let path = dir.join("t.jsonl");
    let recorded = record(&path);
    std::fs::write(
        &path,
        recorded.replace(
            r#""outcome":"success","message":null"#,
            r#""outcome":"error","message":"the report was not available""#,
        ),
    )
    .unwrap();

    let replay = cove(&["replay", &path.display().to_string(), "restricted"]);
    assert!(
        !replay.status.success(),
        "a different ending must fail the replay"
    );
    let report = stderr(&replay);
    for expected in [
        "divergence: the program ended differently than it did",
        "the trace records  error — the report was not available",
        "the program ended  success",
    ] {
        assert!(report.contains(expected), "missing `{expected}`:\n{report}");
    }
}
