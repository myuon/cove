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

/// The package holding the one program a backend's lowering can never
/// accept, at any depth.
///
/// `crates/cove-cli/tests/build.rs`'s
/// `a_program_that_cannot_be_lowered_is_refused_before_a_binary_is_written`
/// pins the same fixture's diagnostic for `cove build`; this is the same
/// program read across to `cove replay`. It lives under `tests/fixtures/`
/// rather than in `tests/e2e/` for the reason that test's comment gives: it
/// is a program that never finishes, and the e2e harness runs every case it
/// holds to completion.
fn instantiation_depth() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/instantiation_depth")
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
/// took no `--backend`, so a trace could be recorded on the lowered backend
/// and replayed on the interpreter and not the other way about. Both
/// directions are here now, along with the two that stay on one evaluator,
/// because a replay's value depends on which of them read the tape and there
/// are four answers rather than two.
///
/// Replaying on the oracle is the direction with something to prove: a
/// recording the oracle cannot follow is the backend having asked for
/// something the language does not say it should, and the replay reports it
/// as a divergence with both calls shown. The direction ADR 0023 added
/// catches the converse — a backend that asks for a call the interpreter's
/// recording does not hold. What has stood in for it is
/// `tests/differential.rs`, which compares every host call's module,
/// operation, arguments and outcome, in order, over the corpus; that is the
/// same tape a replay reads, checked over a hundred programs rather than one.
/// What it does not stand in for is a run driven by a file instead of by a
/// host, which is the only thing a replay does that a second run does not —
/// and that is exactly what the two cross-backend cells below do.
///
/// All four succeed today. `restricted` reads a document and prints a report,
/// and the two evaluators ask for those two host calls in the same order with
/// the same arguments whichever of them is driven by a file.
#[test]
fn a_recording_replays_on_either_backend_from_either_backend() {
    let dir = TempDir::new("cross-backend");
    let on_the_backend = dir.join("lvm.jsonl");
    let on_the_interpreter = dir.join("ast.jsonl");
    let recorded = record(&on_the_backend);
    let oracle = record_on_the_interpreter(&on_the_interpreter);

    // The two recordings are one recording once the wall-clock figures are
    // taken out of them, which is the claim `cove replay` is about to rest
    // on: what it reads is a file, and the two backends wrote the same file.
    assert_eq!(
        without_wall_clock(&recorded),
        without_wall_clock(&oracle),
        "the two backends recorded different traces of one program"
    );

    for (recorded_on, trace, replayed_on, ran_on) in [
        // The ordinary case: `cove run --trace` then `cove replay`, neither
        // of them naming a backend. The recording is the default backend's;
        // since ADR 0026 the replay is that one's *because the file says so*
        // rather than because both defaults happen to agree.
        ("lvm", &on_the_backend, None, "lvm"),
        // The case ADR 0026 is for, and the one no flag could express before
        // it: an interpreter recording, replayed with no flag, runs on the
        // interpreter. Under ADR 0023 this same command line crossed
        // backends silently.
        ("ast", &on_the_interpreter, None, "ast"),
        // The direction ADR 0023 added, spelled out rather than defaulted —
        // and now a crossing the command can see and name.
        ("ast", &on_the_interpreter, Some("lvm"), "lvm"),
        // The direction that worked before ADR 0023, which must keep working.
        ("lvm", &on_the_backend, Some("ast"), "ast"),
        ("ast", &on_the_interpreter, Some("ast"), "ast"),
        ("lvm", &on_the_backend, Some("lvm"), "lvm"),
    ] {
        let path = trace.display().to_string();
        let mut args = vec!["replay", path.as_str(), "restricted"];
        if let Some(backend) = replayed_on {
            args.extend(["--backend", backend]);
        }
        let replay = cove(&args);
        assert!(
            replay.status.success(),
            "a recording made on `{recorded_on}` failed to replay on `{ran_on}`: {}",
            stderr(&replay)
        );
        let played = stdout(&replay);
        assert!(
            played.contains("2 of 2 recorded call(s), answered from the trace"),
            "{played}"
        );
        // The replay says which backend read the tape and whether that is the
        // one that wrote it, because a divergence's meaning turns on both.
        if recorded_on == ran_on {
            assert!(
                played.contains(&format!(
                    "backend     {ran_on}, which is the backend that recorded this trace"
                )),
                "{played}"
            );
        } else {
            assert!(
                played.contains(&format!(
                    "backend     {ran_on}, and this trace was recorded on {recorded_on}; this is a"
                )),
                "{played}"
            );
            assert!(played.contains("cross-backend replay"), "{played}");
        }
    }
}

/// ADR 0019's no-silent-fallback rule, read across to a command that calls no
/// host.
///
/// `crates/cove-cli/tests/fixtures/instantiation_depth` holds `grow`, a
/// function that instantiates a generic one layer deeper at every call, so
/// there is no finite set of functions to lower it to — a permanent refusal
/// rather than a gap a later lowering pass fills in.
/// `crates/cove-cli/tests/build.rs`'s
/// `a_program_that_cannot_be_lowered_is_refused_before_a_binary_is_written`
/// pins the diagnostic for `cove build`; this is the same refusal read across
/// to `cove replay`. A replay has no side effect for a refusal to come before
/// — it makes no host call at all — so what the rule protects here is the
/// verdict: a replay that quietly finished on the interpreter would report
/// "replayed", or a divergence, about a backend nobody asked for.
///
/// The recording has to be made on the interpreter, since the linear-memory
/// backend cannot lower this program at all, let alone run it. `grow` never
/// returns, so the interpreter's own recording ends the way `cove run` itself
/// would: stopped by the call-depth limit rather than by success, and the
/// bare replay — naming no backend, so reading the one the file says
/// recorded it — reaches the same limit for the same reason. Both are beside
/// the point being pinned here, which is what happens on the backend that
/// cannot even start: a replay of this recording with `--backend lvm` never
/// reaches the tape, because the lowering refuses first.
#[test]
fn a_replay_of_a_program_the_lowering_can_never_accept_is_refused_before_the_tape() {
    let dir = TempDir::new("unsupported");
    let path = dir.join("t.jsonl");
    let trace = path.display().to_string();
    let root = instantiation_depth();

    let run = cove_in(
        &root,
        &["run", "app", "--backend", "ast", "--trace", &trace],
    );
    assert!(
        !run.status.success(),
        "`grow` never returns, so this run must stop at the call-depth limit"
    );
    assert!(
        stderr(&run).contains("call depth limit"),
        "{}",
        stderr(&run)
    );

    // The bare command reads the file, finds `ast`, and replays there: the
    // interpreter runs the program for real and reaches the same limit the
    // recording did, because only the Host API boundary is canned and this
    // program never reaches one.
    let bare = cove_in(&root, &["replay", &trace, "app"]);
    assert!(
        !bare.status.success(),
        "the interpreted replay must reach the same limit the recording did"
    );
    assert!(
        stderr(&bare).contains("call depth limit"),
        "{}",
        stderr(&bare)
    );

    let refused = cove_in(&root, &["replay", &trace, "app", "--backend", "lvm"]);
    assert!(
        !refused.status.success(),
        "a replay on a backend that cannot lower this program must refuse"
    );
    let report = stderr(&refused);
    for expected in [
        "error[cove::lower::instantiation_depth]",
        "no finite set of functions to lower it to",
        "help: break the chain, or take the argument as a `dyn Trait`",
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
        stderr(&nonsense).contains("`--backend` must be `ast` or `lvm`, found `jit`"),
        "{}",
        stderr(&nonsense)
    );

    let bare = cove(&["replay", &trace, "restricted", "--backend"]);
    assert!(!bare.status.success(), "`--backend` needs a value");
    assert!(
        stderr(&bare).contains("`--backend` needs a value: `ast` or `lvm`"),
        "{}",
        stderr(&bare)
    );
}

/// A trace with the figure under every `_ns` key replaced, and the header's
/// recording backend with it, so that two recordings of one program can be
/// compared.
///
/// Every `Duration` a trace carries — `cpu`, `wait`, `pause` — is wall time,
/// and two runs of one program on one backend disagree about all three.
///
/// The header's `backend` goes for the opposite reason. ADR 0026 put it there
/// precisely so that a recording says which of the two made it, so the two
/// files being compared here differ in it by definition and would differ in
/// it for a program that did nothing at all. It is the one field in the
/// format that is about the backend rather than about the run; everything
/// else in the header, the version and the capture mode and the entry and its
/// arguments, is still compared exactly.
fn without_wall_clock(trace: &str) -> String {
    let mut out = String::with_capacity(trace.len());
    let mut rest = trace;
    while let Some(at) = rest.find("_ns\":") {
        let (head, tail) = rest.split_at(at + "_ns\":".len());
        out.push_str(head);
        out.push_str("<wall clock>");
        // A duration a machine did not measure is `null` rather than a
        // number, and it is blanked with the numbers: what this function is
        // for is that no reader of it compares a clock, and an absent clock
        // is not something to compare either.
        let end = if tail.starts_with("null") {
            "null".len()
        } else {
            tail.find(|c: char| !c.is_ascii_digit())
                .unwrap_or(tail.len())
        };
        rest = &tail[end..];
    }
    out.push_str(rest);
    without_heap(
        &out.replace(r#""backend":"ast","#, r#""backend":"<either>","#)
            .replace(r#""backend":"lvm","#, r#""backend":"<either>","#),
    )
}

/// Replaces every `heap_summary` line with the fact that there was one.
///
/// The two evaluators do not have the same kind of heap, and since
/// [issue #240](https://github.com/myuon/cove/issues/240) the event says so:
/// the interpreter counts objects and the bytes they asked for, the
/// linear-memory backend counts words, and each leaves `null` in the family
/// it does not count. Neither set of figures can be derived from the other —
/// an inline struct is words in one and no object at all in the other — so
/// comparing them would be comparing two answers to two different questions.
///
/// What is still compared is that the event is there, once, in the same place
/// in the sequence. That is a property of the run rather than of the heap: a
/// backend that stopped writing a summary, or wrote one somewhere else, still
/// fails this.
fn without_heap(trace: &str) -> String {
    trace
        .lines()
        .map(|line| {
            if line.starts_with(r#"{"event":"heap_summary","#) {
                r#"{"event":"heap_summary","figures":"<its own heap's>"}"#
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[test]
fn a_recorded_run_reads_back_and_replays() {
    let dir = TempDir::new("roundtrip");
    let path = dir.join("t.jsonl");
    let recorded = record(&path);
    assert!(
        recorded.starts_with(r#"{"event":"trace_header","version":4,"backend":"lvm","values":"full","entry":"restricted.main","args":[]}"#),
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
        // No flag was given, so the replay ran on the backend the file names,
        // and the report says the strong thing ADR 0023 could not say: this
        // divergence is the program's.
        "this replay ran on `lvm`, which is the backend that",
        "recorded the trace, so a divergence is the program's rather",
    ] {
        assert!(report.contains(expected), "missing `{expected}`:\n{report}");
    }
}

/// The same divergence, replayed across backends on purpose, gets the
/// opposite note.
///
/// This is the pair that makes the header field worth its format version.
/// ADR 0023 had to append the caveat to every divergence report, because no
/// file could say whether the crossing had happened; ADR 0026 lets the report
/// say which of the two situations this is, and the two sentences say
/// opposite things about what the divergence can be blamed on. A test that
/// only saw one of them would not be testing the distinction.
#[test]
fn a_cross_backend_divergence_says_the_two_backends_could_be_the_difference() {
    let dir = TempDir::new("diverge-crossed");
    let path = dir.join("t.jsonl");
    let recorded = record(&path);
    std::fs::write(&path, recorded.replace("5 words", "4 words")).unwrap();

    let replay = cove(&[
        "replay",
        &path.display().to_string(),
        "restricted",
        "--backend",
        "ast",
    ]);
    assert!(
        !replay.status.success(),
        "a divergence must fail the replay"
    );
    let report = stderr(&replay);
    for expected in [
        "divergence: the program asked for a different host call",
        "this replay ran on `ast` and the trace was recorded on",
        "`lvm`, so a divergence here could be the two backends'",
    ] {
        assert!(report.contains(expected), "missing `{expected}`:\n{report}");
    }
}

/// `cove trace` names the backend that recorded the file, so a trace can be
/// asked about its provenance without being replayed.
#[test]
fn a_summary_names_the_backend_that_recorded_the_trace() {
    let dir = TempDir::new("summary-backend");
    for (name, record_it, expected) in [
        ("lvm.jsonl", record as fn(&Path) -> String, "lvm"),
        (
            "ast.jsonl",
            record_on_the_interpreter as fn(&Path) -> String,
            "ast",
        ),
    ] {
        let path = dir.join(name);
        record_it(&path);
        let inspect = cove(&["trace", &path.display().to_string()]);
        assert!(inspect.status.success(), "{}", stderr(&inspect));
        let report = stdout(&inspect);
        assert!(
            report.contains(&format!(
                "backend    {expected} — the backend that ran the program this trace recorded"
            )),
            "{report}"
        );
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
///
/// Both directions, because ADR 0026 moved the format forward and the
/// backward one is the compatibility decision it had to make. Version 2 is
/// the version every recording made before ADR 0026 carries, and it is
/// refused by exactly the sentence a version from the future gets: this
/// reader has always read one version, and the alternative — reading a
/// version 2 file and calling its backend unknown — would have been a second
/// compatibility policy invented to serve a replay outcome nothing this build
/// can write is able to produce.
#[test]
fn a_trace_from_a_version_this_build_does_not_read_is_rejected_by_both_commands() {
    let dir = TempDir::new("version");
    let path = dir.join("t.jsonl");
    let recorded = record(&path);

    for version in ["99", "2"] {
        std::fs::write(
            &path,
            recorded.replace(r#""version":4"#, &format!(r#""version":{version}"#)),
        )
        .unwrap();

        let path = path.display().to_string();
        for args in [
            vec!["trace", path.as_str()],
            vec!["replay", path.as_str(), "restricted"],
        ] {
            let output = cove(&args);
            assert!(
                !output.status.success(),
                "version {version} must be rejected"
            );
            assert!(
                stderr(&output).contains(&format!(
                    "is version {version}, and this build of `cove` reads version 4"
                )),
                "{}",
                stderr(&output)
            );
        }
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
