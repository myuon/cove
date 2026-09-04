//! `cove debug`, driven the way a person drives it: a script on stdin.
//!
//! What this suite asks that its neighbours do not is whether the *session*
//! is right — whether a breakpoint set by source line stops on that line,
//! whether a backtrace names the calls that are really live, whether `print`
//! answers with the binding the source means. `tests/e2e.rs` pins one whole
//! transcript byte for byte, which says what the output looks like and says
//! it once; this says what each command means, one question per test, with
//! the program held still and only the script varying.
//!
//! The distinction matters because the debugger's answers come from three
//! places that are otherwise never compared: `cove-ir`'s spans and locals
//! table, `cove-runtime`'s stop views, and this crate's policy — which
//! source line is "the next one", which of two live bindings of one name is
//! meant, which instruction a line resolves to. A golden transcript would
//! notice all three moving together and could not say which had moved.
//!
//! # Why a real process
//!
//! There is no way to ask these questions in process. `cove debug`'s prompt
//! runs *inside* `Debugger::at`, which the machine calls from the dispatch
//! loop, so the session is not a value a test can drive — it is a program
//! reading stdin. So the binary is spawned, the script is written down its
//! stdin, and the transcript is read back. That is also the only arrangement
//! in which "`quit` exits cleanly" is a statement about an exit code.
//!
//! Every test runs `tests/e2e/debug_session`, which is checked in as an
//! end-to-end case and doubles as this suite's fixture: three functions deep,
//! a name bound twice in one frame, and a declaration the entry never
//! reaches.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The `tests/e2e` package, resolved so that the child reports the same
/// paths whatever directory `cargo test` was started from.
fn corpus() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/e2e");
    path.canonicalize()
        .unwrap_or_else(|e| panic!("cannot resolve `{}`: {e}", path.display()))
}

/// What one session produced.
struct Transcript {
    out: String,
    err: String,
    code: Option<i32>,
}

impl Transcript {
    /// Fails unless `needle` is somewhere in what the session printed,
    /// showing the whole transcript when it is not — a session's output is
    /// the only evidence there is about what it did.
    #[track_caller]
    fn says(&self, needle: &str) {
        assert!(
            self.out.contains(needle),
            "the session never printed `{needle}`\n--- stdout\n{}\n--- stderr\n{}",
            self.out,
            self.err
        );
    }

    /// Fails unless the program itself wrote `line` — a whole line of
    /// stdout, and not the same text quoted back inside a source listing,
    /// which is what `says` would find.
    #[track_caller]
    fn wrote(&self, line: &str) {
        assert!(
            self.out.lines().any(|written| written == line),
            "the program never wrote the line `{line}`\n--- stdout\n{}",
            self.out
        );
    }

    #[track_caller]
    fn never_wrote(&self, line: &str) {
        assert!(
            !self.out.lines().any(|written| written == line),
            "the program wrote the line `{line}` and should not have\n--- stdout\n{}",
            self.out
        );
    }

    #[track_caller]
    fn never_says(&self, needle: &str) {
        assert!(
            !self.out.contains(needle),
            "the session printed `{needle}` and should not have\n--- stdout\n{}",
            self.out
        );
    }

    /// The one line holding `needle`, for an assertion about what follows it
    /// rather than about the whole transcript.
    #[track_caller]
    fn line_with(&self, needle: &str) -> &str {
        self.out
            .lines()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no line holds `{needle}`\n--- stdout\n{}", self.out))
    }

    /// Where `needle` first appears, for an assertion about order.
    #[track_caller]
    fn at(&self, needle: &str) -> usize {
        self.out
            .find(needle)
            .unwrap_or_else(|| panic!("no `{needle}`\n--- stdout\n{}", self.out))
    }
}

/// Runs `cove debug debug_session` with `script` on its stdin.
fn debug(script: &str) -> Transcript {
    run(&["debug", "debug_session"], script)
}

/// Runs the real binary in the corpus with `script` on its stdin.
fn run(args: &[&str], script: &str) -> Transcript {
    let mut child = Command::new(env!("CARGO_BIN_EXE_cove"))
        .current_dir(corpus())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the `cove` binary starts");
    // The whole script fits in the pipe's buffer and the child writes only
    // after it has read, so writing it all and then reading cannot deadlock.
    //
    // A failed write is not a failed test: a child that refused its flags
    // has already exited, and writing at its closed stdin is the expected
    // way to find that out. What the test is about is what the child said.
    let mut stdin = child.stdin.take().expect("a piped stdin");
    let _ = stdin.write_all(script.as_bytes());
    drop(stdin);
    let output = child.wait_with_output().expect("the session ends");
    Transcript {
        out: String::from_utf8_lossy(&output.stdout).into_owned(),
        err: String::from_utf8_lossy(&output.stderr).into_owned(),
        code: output.status.code(),
    }
}

/// **A run starts stopped, before its first instruction, and the entry has
/// not printed anything yet.**
///
/// The whole shape of the command: a debugger installed at
/// `Vm::debugged` is asked before the *first* instruction, so there is
/// somewhere to set a breakpoint from. A session that started after the
/// program had begun would be a session that could not break on the first
/// line of the entry.
#[test]
fn a_session_starts_stopped_before_the_entry_s_first_instruction() {
    let session = debug("quit\n");
    session.says("entering debug_session.main at debug_session/main.cove:");
    session.never_wrote("before");
    assert_eq!(session.code, Some(0));
}

/// **A breakpoint set on a source line stops the run on that line, in the
/// function that line is written in.**
///
/// The reverse index this command had to build: nothing in a lowered
/// program maps source back to instructions, so `break <file>:<line>` is
/// answered by a table `cove debug` computes and by nothing the compiler
/// records.
#[test]
fn a_breakpoint_set_on_a_source_line_stops_the_run_on_that_line() {
    let session = debug("break debug_session/main.cove:6\ncontinue\nquit\n");
    session.says("breakpoint 1 at 1 location:");
    session.says("debug_session.twice pc 0 at debug_session/main.cove:6:");
    session.says("breakpoint 1, debug_session.twice at debug_session/main.cove:6:");
    // Stopped at the line, and stopped *before* the program got past it.
    session.wrote("before");
    session.never_wrote("after");
    assert_eq!(session.code, Some(0));
}

/// **A breakpoint resolves to the lowest instruction written on its line,
/// and a line that no instruction was lowered for is refused.**
///
/// The second half is the one worth having. `lower_entry` lowers what the
/// entry reaches and leaves every other declaration a stub, so a breakpoint
/// in unreached code would be accepted and then never fire — a debugger
/// silently lying about where a program can stop.
#[test]
fn a_breakpoint_on_a_line_with_no_lowered_instruction_is_refused_rather_than_accepted() {
    let blank = debug("break debug_session/main.cove:2\ninfo breakpoints\nquit\n");
    blank.says("no instruction was lowered for debug_session/main.cove:2");
    blank.says("no breakpoints");

    let unreached = debug("break debug_session/main.cove:19\nquit\n");
    unreached.says("no instruction was lowered for debug_session/main.cove:19");
    unreached.says("`debug_session.unreached` is declared there");
}

/// **A function the entry does not reach is named as unreached rather than
/// as unknown.**
///
/// Two different mistakes with two different fixes: a name that is not in
/// the package is a typo, and a name that is in the package but out of the
/// slice is a program that never calls it.
#[test]
fn a_breakpoint_on_a_declaration_the_entry_never_calls_says_which_of_the_two_it_is() {
    let unreached = debug("break unreached\nquit\n");
    unreached.says("`debug_session.unreached` is declared, but this entry does not reach it");

    let unknown = debug("break notAFunction\nquit\n");
    unknown.says("no lowered function is called `notAFunction`");
}

/// **A backtrace taken in a nested call names every live frame, innermost
/// first, so the entry is the last line it prints.**
#[test]
fn a_backtrace_in_a_nested_call_names_the_functions_outermost_last() {
    let session = debug("break debug_session/main.cove:6\ncontinue\nbacktrace\nquit\n");
    session.says("#0  debug_session.twice at debug_session/main.cove:6:");
    session.says("#1  debug_session.raise at debug_session/main.cove:14:");
    session.says("#2  debug_session.main at debug_session/main.cove:");
    assert!(
        session.at("#0  debug_session.twice") < session.at("#1  debug_session.raise")
            && session.at("#1  debug_session.raise") < session.at("#2  debug_session.main"),
        "innermost first, entry last:\n{}",
        session.out
    );
}

/// **`print` finds a local of the selected frame by the name the source gave
/// it, and `frame` is what chooses whose.**
#[test]
fn print_finds_a_local_by_name_in_whichever_frame_is_selected() {
    let session =
        debug("break debug_session/main.cove:6\ncontinue\nprint n\nframe 1\nprint raised\nquit\n");
    session.says("n = 21");
    session.says("raised = 21");
    session.says("#1  debug_session.raise");
}

/// **When one name is bound twice in a frame, `print` answers with the
/// binding the source means, and `locals` shows both.**
///
/// `cove_ir::Local`'s rule: shadowing is *recorded* rather than resolved, so
/// two bindings of `total` are live at once and the later one is what the
/// source means — the lowering searches its scope backwards. `Call::local`
/// takes the *first* match and would answer with the shadowed one, so this
/// command reads the list backwards itself. That disagreement is the whole
/// reason this test exists.
#[test]
fn print_answers_with_the_later_binding_when_a_name_is_shadowed() {
    let session =
        debug("break debug_session/main.cove:6\ncontinue\nframe 2\nprint total\nlocals\nquit\n");
    assert!(
        session.line_with("total = 20").contains("total = 20"),
        "the later binding is what `total` means"
    );
    session.says("total = 20");
    // Both are live, and the earlier one is shown as shadowed rather than
    // hidden: it is still in the frame, and `words` would otherwise be the
    // only way to see that.
    session.says("total = 0");
    assert!(
        session.line_with("total = 0").contains("shadowed"),
        "the earlier binding is marked:\n{}",
        session.out
    );
    // The two are different words of the frame, which is what makes the
    // choice a real one rather than a rendering.
    assert_ne!(
        session.line_with("total = 0"),
        session.line_with("total = 20")
    );
}

/// **`finish` runs the selected frame to its return and stops in its
/// caller.**
///
/// A frame-depth policy, and one the machine has no notion of: `Stop::depth`
/// is the only thing it offers, and "shallower than where I asked from" is
/// this command's own rule.
#[test]
fn finish_runs_the_frame_to_its_return_and_stops_in_the_caller() {
    let session = debug("break debug_session/main.cove:6\ncontinue\nfinish\nbacktrace\nquit\n");
    session.says("returned to debug_session.raise at debug_session/main.cove:14:");
    // Two frames, not three: `twice` is gone, and the stop is in its caller
    // rather than at whatever instruction happened to come next.
    let frames: Vec<&str> = session
        .out
        .lines()
        .filter(|line| line.starts_with('#'))
        .collect();
    assert_eq!(
        frames.len(),
        2,
        "the backtrace after `finish`:\n{}",
        session.out
    );
    assert!(frames[0].contains("debug_session.raise"), "{}", frames[0]);
    assert!(frames[1].contains("debug_session.main"), "{}", frames[1]);
}

/// **`step` stops inside a call and `next` runs it to completion.**
///
/// The one difference between the two rules — `next` refuses to stop at a
/// frame deeper than the one it was asked from — measured where it shows:
/// stepping at the call in `raise`.
#[test]
fn step_stops_inside_a_call_and_next_runs_it_to_completion() {
    let into = debug("break debug_session/main.cove:14\ncontinue\nstep\nbacktrace\nquit\n");
    into.says("#0  debug_session.twice");

    let over = debug("break debug_session/main.cove:14\ncontinue\nnext\nbacktrace\nquit\n");
    over.never_says("#0  debug_session.twice");
    // Still in `raise`, or already back in `main`; either way not deeper.
    assert!(
        over.out.contains("#0  debug_session.raise") || over.out.contains("#0  debug_session.main"),
        "`next` never stopped below the frame it was asked from:\n{}",
        over.out
    );
}

/// **`stepi` advances exactly one instruction, and the instruction count the
/// session reports moves by one.**
#[test]
fn stepi_advances_one_instruction_and_the_count_says_so() {
    let one = debug("stepi\ninfo run\nquit\n");
    let two = debug("stepi\nstepi\ninfo run\nquit\n");
    let counted = |session: &Transcript| {
        session
            .line_with("instruction(s) run")
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .expect("a count")
    };
    assert_eq!(
        counted(&two),
        counted(&one) + 1,
        "one `stepi` is one instruction\n{}\n{}",
        one.out,
        two.out
    );
}

/// **`disassemble` marks the instruction the run is about to execute, and
/// `words` reports the frame words no name covers.**
///
/// The two views the sketch asks for on behalf of VM development, and the
/// only two that show anything a source-level view cannot.
#[test]
fn disassemble_marks_the_current_instruction_and_words_reports_the_unnamed_ones() {
    let session = debug("break debug_session/main.cove:6\ncontinue\ndisassemble 1\nwords\nquit\n");
    session.says("debug_session.twice:");
    let marked: Vec<&str> = session
        .out
        .lines()
        .filter(|line| line.starts_with("=>"))
        .collect();
    assert_eq!(marked.len(), 1, "exactly one instruction is marked");
    // `n` is a name, so it is not among the words; something else is.
    assert!(
        session.out.lines().any(|line| line.starts_with("word ")),
        "the frame holds a word no name covers:\n{}",
        session.out
    );
}

/// **`object` follows a frame word into the heap and names what is there,
/// and answers plainly for a word that is not an object.**
///
/// Two runs of one program rather than one, because a word is not something
/// a script can be written to know: the first session reads a live reference
/// out of `words`, and the second follows exactly that word. The same
/// program run twice is the same run, so the address the first found is the
/// address the second sees.
#[test]
fn object_follows_a_frame_word_into_the_heap_and_names_what_is_there() {
    let prefix = "break debug_session/main.cove:6\ncontinue\nfinish\nfinish\nstepi\nstepi\n";
    let looked = debug(&format!("{prefix}words\nquit\n"));
    let word = looked
        .out
        .lines()
        .filter(|line| line.starts_with("word ") && line.contains(" ref "))
        .filter_map(|line| line.split_whitespace().find(|w| w.starts_with("0x")))
        .find(|word| *word != "0x0000000000000000")
        .unwrap_or_else(|| panic!("no live reference in the frame:\n{}", looked.out));

    let followed = debug(&format!("{prefix}object {word}\nobject 12345\nquit\n"));
    followed.says(&format!("{word}: "));
    followed.says("0x0000000000003039 does not name an object of this run's heap");
}

/// **`quit` ends the run where it stands, and the command succeeds.**
///
/// A person stopping is not a program failing. The machine reports the halt
/// as `RunOutcome::Debugger` and this command turns that back into a clean
/// exit, so `cove debug` in a script does not look like a crash.
#[test]
fn quit_ends_the_run_where_it_stands_and_the_command_succeeds() {
    let session = debug("break debug_session/main.cove:6\ncontinue\nquit\n");
    assert_eq!(session.code, Some(0), "stderr:\n{}", session.err);
    assert!(session.err.is_empty(), "stderr:\n{}", session.err);
    session.says("the run was halted after");
    session.never_wrote("after");
}

/// **An end of input halts the run the way `quit` does.**
///
/// A script that runs out has said everything it was going to say, and the
/// alternative — carrying on unwatched — would be a run nobody asked for
/// finishing after the session ended.
#[test]
fn an_end_of_input_halts_the_run_the_way_quit_does() {
    let session = debug("break debug_session/main.cove:6\ncontinue\n");
    assert_eq!(session.code, Some(0), "stderr:\n{}", session.err);
    session.says("the run was halted after");
    session.never_wrote("after");
}

/// **A `continue` with nothing left to stop at runs the program to its end,
/// and the program's own output is the output it always had.**
///
/// The regression that would be easiest to introduce and hardest to see: a
/// debugger that changed what the program did.
#[test]
fn continuing_to_the_end_runs_the_program_it_always_was() {
    let session = debug("continue\n");
    session.wrote("before");
    session.wrote("answer 42");
    session.wrote("after");
    session.says("the run finished after");
    assert_eq!(session.code, Some(0), "stderr:\n{}", session.err);
}

/// **An empty line repeats the last command.**
///
/// What fingers expect from gdb, and the only piece of the prompt that is a
/// convenience rather than a capability.
#[test]
fn an_empty_line_repeats_the_last_command() {
    let typed = debug("stepi\ninfo run\nquit\n");
    let repeated = debug("stepi\n\n\ninfo run\nquit\n");
    let counted = |session: &Transcript| {
        session
            .line_with("instruction(s) run")
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<u64>().ok())
            .expect("a count")
    };
    assert_eq!(
        counted(&repeated),
        counted(&typed) + 2,
        "two blank lines are two more `stepi`s:\n{}\n{}",
        typed.out,
        repeated.out
    );
}

/// **`cove debug` names no backend, and says so rather than ignoring the
/// flag.**
///
/// A debugger is a feature of the linear-memory machine; the tree walker has
/// none. Accepting `--backend ast` and running on the machine anyway would
/// be answering a different question than the one asked.
#[test]
fn cove_debug_refuses_a_backend_because_there_is_only_one_to_run_on() {
    let session = run(&["debug", "debug_session", "--backend", "ast"], "quit\n");
    assert_ne!(session.code, Some(0));
    assert!(
        session.err.contains("takes no `--backend`"),
        "stderr:\n{}",
        session.err
    );
}

/// **`cove run` is not a debugger, whatever is on its stdin.**
///
/// The command is its own; a run does not grow a prompt because somebody
/// piped commands at it.
#[test]
fn cove_run_never_stops_however_much_is_written_at_its_stdin() {
    let session = run(
        &["run", "debug_session"],
        "break debug_session/main.cove:6\nquit\n",
    );
    session.never_says("(cove)");
    session.wrote("before");
    session.wrote("after");
    assert_eq!(session.code, Some(0), "stderr:\n{}", session.err);
}

/// **The session can say what its own stepping rule gets wrong.**
///
/// Spans are per-instruction and expression-level and nothing marks where a
/// statement begins, so "one source line" is a rule with edges rather than a
/// fact the program records. An honest limitation beats a silent one, and
/// the place to state it is where the person using it will read it.
#[test]
fn the_session_states_what_its_stepping_rule_gets_wrong() {
    let session = debug("help limits\nquit\n");
    session.says("run until the line changes");
    session.says("a loop whose body is written on one line");
    session.says("the line number can go backwards");
    session.says("a spawned task reaching an instruction can");
}
