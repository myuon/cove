//! Every program the repository keeps, through the linear-memory backend,
//! and what happened to each one.
//!
//! [ADR 0034](../../../docs/adr/0034-one-physical-word-stack.md) makes the
//! replacement's completion conditional on running the corpus and agreeing
//! with the oracle. This is where that condition is measured. It exists for
//! two reasons: it is **the roadmap for what to build next**, and it is a
//! **ratchet**, so that a change which quietly runs fewer programs than the
//! last one did fails instead of passing.
//!
//! The predecessor had a file of its own doing this job, and it could ask a
//! question this one cannot: that backend answered an admission predicate, so
//! "would you run this?" could be asked without running anything.
//! `docs/LINEAR_VM.md` deletes that question — there is no `Unsupported`, no
//! admission predicate and no lowering floor, and the target is that *every
//! valid checked program lowers* — so the only honest way to ask this backend
//! what it covers is to run the program and compare. That is what this does,
//! and it is why the predecessor's file went with the predecessor rather than
//! being repointed.
//!
//! # The three answers
//!
//! Each case falls into exactly one of them, and the middle one is the point:
//!
//! - **lowers and agrees**: the program lowered, ran, and answered what the
//!   tree-walking oracle answered — the value or the failure, everything
//!   written to either console stream, how the run ended, and the fake
//!   filesystem it left behind.
//! - **lowers but disagrees**: the program lowered and ran and said something
//!   else. This is a bug and the list of them is the most valuable thing
//!   printed here, so it is printed in full, both sides, with nothing folded
//!   away. ADR 0012 ranks the oracle above a backend, so the backend is
//!   presumed wrong.
//! - **does not lower**: `cove_ir::lower` answered diagnostics. Every one of
//!   them is a gap in the lowering rather than a fault in the program — the
//!   program passed `cove check` to get here — and they are ranked by how
//!   many programs each blocks, which is the order to build them in.
//!
//! A panic on either side is counted as a disagreement rather than allowed to
//! end the survey, because a survey that stops at the first one measures
//! nothing after it. The panic's message is what the disagreement reports.
//!
//! # What is compared, and what is not
//!
//! The value or the structured failure, both console streams, how the run
//! ended, and the files the run wrote. Not the trace, and not the error's
//! span. `differential.rs` compares both of those, over the same corpus less
//! `benches/`, and comparing them here as well would report the same programs
//! as disagreeing for a second reason and would say nothing new about which
//! family to build next. What this file is for is the count; what that one is
//! for is the depth.
//!
//! Nothing is dropped from the comparison because it differed. A
//! disagreement is the finding, and a comparison weakened until it passes has
//! destroyed the finding rather than fixed it.
//!
//! # Why the benchmarks are in, and why this is `#[ignore]`d
//!
//! `differential.rs` leaves `benches/` out because running a benchmark's two
//! million turns twice, unoptimized, cost it 78 of 340 seconds and told it
//! nothing the first turn had not. That reasoning does not reach here:
//! `benches/` is nine more programs of the corpus, and what this measures is
//! how much of the corpus lowers and runs at all. A benchmark that does not
//! lower costs a lowering and no turns whatsoever, and one that does lower is
//! a program worth having run at that size, because it is where a regression
//! in the dispatch loop shows first.
//!
//! The nine were nearly free while almost none of them reached a run, and
//! they are not now: everything in the corpus lowers, so a benchmark's two
//! million turns happen twice, and the survey costs minutes rather than the
//! half second it began at. That is what decides the other question. This
//! test is `#[ignore]`d and CI runs it with
//! `cargo test --workspace --lib --tests -- --ignored`, which is where the
//! repository puts a case whose cost is measured in minutes and whose value
//! is measured once per change rather than once per `cargo t`.
//!
//! Running them at a size chosen for a test rather than for a benchmark was
//! the alternative, and it would measure something other than what `benches/`
//! is. The next family this backend learns will add to the minutes rather
//! than replace them.
//!
//! # Reading the report
//!
//! ```console
//! $ cargo test -p cove-cli --test vm_coverage -- --nocapture
//! ```
//!
//! It is printed on every run and repeated in the message of a failing
//! assertion, so a failing run carries its own evidence.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use cove_runtime::budget::{Budget, Cancellation, Limits};
use cove_runtime::clock::{Clock, VirtualTime};
use cove_runtime::database::Database;
use cove_runtime::error::RuntimeError;
use cove_runtime::files::{Files, Tree};
use cove_runtime::host::{Console, Documents, Env, Grants, HostRegistry};
use cove_runtime::http::Http;
use cove_runtime::interp::Interpreter;
use cove_runtime::process::{Process, ProcessLog};
use cove_runtime::runtime::Runtime;
use cove_runtime::trace::RunOutcome;
use cove_runtime::value::Value;
use cove_runtime::Vm;
use cove_sema::config::RunConfig;
use cove_sema::HostSchemas;

#[path = "support/mod.rs"]
mod support;
use support::{Case, ModuleIndex, Prepared};

/// How many corpus programs lowered *and* agreed with the oracle the last
/// time this number was raised.
///
/// A floor, not a target: a family this backend learns raises it, and nothing
/// may lower it, because a program that stopped running is coverage lost
/// silently and the whole reason to count is that it cannot be.
///
/// It counts agreement rather than lowering, which is the one place it
/// differs from the floors it was modelled on. Those could count what
/// lowered, because everything that lowered agreed — a disagreement failed
/// the suite outright. Here a disagreement is a recorded finding rather than
/// a stopped build, so a floor over what lowers would rise for a program that
/// lowers and answers the wrong thing, which is the opposite of what a
/// coverage ratchet is for.
///
/// 50 is where it stood when the CLI was first wired to this backend, which
/// is the first time the corpus could be run against it at all: of the 149
/// programs the repository keeps, 118 have a checked program behind them, 53
/// of those lower and run, and 50 of the 53 answer what the oracle answers.
/// See the report this test prints for which they are, for the three that do
/// not, and for what the other 65 are blocked on.
///
/// 62 is where teaching the lowering `assert` and `assertEqual` left it: the
/// eight benchmarks that were blocked by nothing else — `arith`, `arrayget`,
/// `call`, `chars`, `field`, `hostheavy`, `method` and `pure` — all lower,
/// run, and agree.
///
/// That is also the moment this file's own module docs named. Those eight
/// are benchmarks, so what used to be a lowering and no turns is now two
/// million turns twice over, and the survey went from half a second of CPU
/// to eighty. The reasoning for leaving them in has not changed and neither
/// has the reasoning for leaving this test in the ordinary suite, but the
/// cost is no longer negligible and the next family this backend learns will
/// add to it rather than replace it.
///
/// 67 to 70, and the arithmetic is worth writing out because it is four up
/// and one down. `Vector.freeze()` is the four:
/// [issue #240](https://github.com/myuon/cove/issues/240) moved the
/// uniqueness proof out of the run and into `cove_sema::unique`, where ADR
/// 0001 always said it lived, so this machine consumes the store instead of
/// refusing and `tests/e2e:coll_array`, `tests/e2e:coll_freeze`,
/// `examples:values` and `benches:callback` answer what the oracle answers.
/// `tests/e2e:fail_freeze_aliased` is the one down: it used to agree because
/// both sides refused it at run time, and it is now refused by `cove check`,
/// so it has no checked program to lower and has left this survey entirely.
///
/// 70 to 76, and it is three families rather than one.
///
/// **A type a host module declares has a layout.** A resource handle is one
/// `Repr::Host` word — ADR 0013's "the host keeps what it is and Cove holds
/// the name of it" is the whole of the representation — and a type the host
/// hands over is its fields inline or its cases, exactly as a declared one
/// is. That was the largest cluster on the ranked list, because every type
/// built on one failed with it: `Result<files.Reader, Error>` and the `?` on
/// it, `enum Sink { Console, File(files.Writer) }`, `Array<http.Route>`.
/// `tests/e2e:fail_database_connect_denied` and
/// `tests/e2e:fail_http_no_capability` are the two it runs on its own.
///
/// **A trailing lambda is the call's last argument**, which is what
/// `interp::eval_args` makes it, plus `Result.mapError` — the one thing in
/// the corpus a trailing lambda is written on that neither crosses the
/// boundary nor needs a task. `examples:config` is what those two run.
///
/// **A trait method's default body is lowered once per conforming type**,
/// under the substitution `Self := the conforming type`. It needed no
/// machinery of its own: the checker records one `Signature` at the trait
/// method's span with the receiver typed `Ty::Param("Self")`, so it is a
/// generic declaration with one parameter and the monomorphisation path
/// already there completes it. `tests/e2e:type_trait`,
/// `tests/e2e:module_conformance` and `examples:traits` are the three.
///
/// 76 to 81, and it is again three families — the ones that between them
/// blocked the corpus's I/O programs.
///
/// **An operation of a host resource is the boundary, addressed to a
/// handle.** `Inst::CallResource` names the receiver as an operand of its
/// own rather than as an argument, because ADR 0013 makes a handle the thing
/// a call is *addressed to* and `HostRegistry::call_resource` hands the host
/// only what follows it. It was the largest cluster left:
/// `files.Writer.writeLine`, both `close`s, `files.Reader.readLine`,
/// `http.Server.port`. `tests/e2e:fail_http_stale_handle`,
/// `tests/e2e:host_files_streaming` and `tests/e2e:host_http_resource` are
/// what it runs on its own.
///
/// **A capture of a `var` parameter is the value behind the address.** The
/// oracle's `Env::captures` reads every binding through `Place::read`, so the
/// environment holds a copy and the load is the one instruction the
/// difference costs. With the resource operations it is what runs
/// `examples:cq` and `examples:cqSample`.
///
/// **A field's type is read where the declaration wrote it.** The checker
/// records a struct's fields once, in the vocabulary of the module that
/// declares them, and resolving one where the *use* was written asked a
/// module about a name it never imported. Nothing in this survey runs on it
/// alone; it is what took `examples:life` down to a single gap.
///
/// The lowering also now reads the [`HostSchemas`] a compilation was given
/// rather than the shipped tables, so a type an embedder's module declares
/// has a layout. No corpus program is an embedding, so it moves no number
/// here — the cases for it are `cove-ir`'s own.
///
/// 83 to 87 is one family: **a task scope, and the three things a program
/// does with one.** `scope name { ... }` is `cove_ir::Inst::ScopeEnter` and
/// the two ways of leaving it — the one written where the `scope` is, and the
/// one a `return`, a `?`, a `break` or a `continue` owes it. `spawn`, `await`
/// and `cancel` are one instruction each. `cove_runtime::vm::exec` grew the
/// scheduler table those words index, a thread per task over a stack segment
/// of its own and the run's one heap, a per-task `Cancellation` the safepoint
/// reads, and the two places a task that is not running Cove still has to
/// count as being at a safepoint: a host call, and a join.
///
/// `tests/e2e:tasks_scope`, `tests/e2e:gc_tasks`,
/// `tests/e2e:tasks_host_order` and `tests/e2e:fail_max_tasks` are the four
/// it runs. The second is the one worth naming: four tasks allocating at once
/// over one collector, each keeping a vector nothing else can reach, which is
/// where a collection that reached across a task's roots would show.
///
/// 105 to 107 is **`Shared`**, which is ADR 0008's other half: the one value
/// two tasks reach by sharing rather than by copying. It is an ordinary
/// object in the run's one heap whose lock is one of its own words, so
/// `Shared(value)` is an `alloc` and a `store-field` and needs no instruction
/// at all; `lock` is `shared.lock`, an ordinary `call-closure`, and
/// `shared.unlock`, because **a builtin never calls back into Cove** and the
/// release is an obligation on every exit path exactly as `Clear` is.
///
/// `tests/e2e:tasks_shared` is one of the two, and it is the whole point of
/// the type: two tasks counting into one cell, where a read-modify-write that
/// raced would lose a count.
///
/// `tests/e2e:shared_cycle` is the other, and it is new. It replaces
/// `tests/e2e:fail_shared_cycle`, which pinned a refusal
/// [ADR 0037](../../../docs/adr/0037-a-cycle-through-a-cell-is-an-ordinary-cycle.md)
/// removed: a cycle through one or more cells is an ordinary object-graph
/// cycle now, because a cell is an object in the traced heap and the
/// collector ADR 0011's amendment deferred is the collector that is running.
/// The old case could not stay — the oracle still makes that refusal, so a
/// corpus program that closed a cycle *directly* would lower, run, and be
/// told it disagreed. What a
/// corpus case can show without that is a cycle through **two** cells, which
/// the amendment never refused and called an accepted leak; that it is
/// reclaimed rather than leaked is `cove_runtime::vm::cell`'s to show,
/// because only a test that runs the collection itself can say so.
///
/// 115 to 117, and it is the whole board: nothing lowers and disagrees, and
/// nothing fails to lower. The two were the last, and neither was a family
/// this backend had not been taught — both were bugs in something it had.
///
/// **A box is tagged with the family the value left this machine with, where
/// there is one.** `examples:covecheck` trapped at an `Inst::Unbox` before
/// printing anything. `clock.timeout` declares `Result<Any, Error>`, and the
/// search that decides what family a value crossing in at an erased position
/// belongs to reads the value's own description — which does not always name
/// one. `Err(Error("..."))` describes `Result<covecheck.Check, Error>` and
/// `Result<http.Response, Error>` equally exactly, and those are different
/// runs of words, so which one the box recorded was decided by which the
/// lowering happened to intern first. `clock.timeout`'s answer is its
/// *callback's* answer, and a callback's return layout is a static fact, so
/// `cove_runtime::vm::exec::Machine::callback_answer` records it on the way
/// out and `boundary::held_layout` prefers it over any search.
///
/// **Leaving a scope waits for the children the body did not await.**
/// `examples:tasks` spawns two fetches, awaits one inside a `clock.timeout`
/// body, and both fail; the oracle answers the *unawaited* one and this
/// backend answered the awaited one. `crate::task::wait_for_children` skips a
/// child that is no longer running and that first line is a language
/// decision: a child the body awaited has already handed its value to the
/// program, so reporting it again at the scope exit would overwrite whatever
/// the body did with it. `Machine::leave_scope` now skips it too.
///
/// 117 to 116, and it is the one direction this number is not supposed to
/// move, so it is worth saying exactly what left and why it is not coverage
/// lost. ADR 0034's cutover deleted `tests/e2e/backend_unsupported`. That
/// case existed to pin the *predecessor* refusing a construct — a function
/// declared inside a function body — and its own doc comment said it would
/// have to move the day a backend covered the construct. This backend covers
/// it, and it answers no admission predicate at all, so there is no refusal
/// left for the case to be about.
///
/// Its **program** did not stop running. `tests/e2e/backend_ast` holds the
/// same program, byte for byte but for the comment above it, and is one of
/// the 116: the construct is lowered, run and agreed on exactly as it was
/// yesterday. What left the survey is a duplicate that was carrying a
/// different job, and the job is gone. `differential.rs`'s own floor moved
/// 97 to 96 for the same shape of reason and wrote it down the same way.
///
/// The property the deleted case pinned — that a program the backend will
/// not accept stops the command before anything happens, rather than quietly
/// finishing on the interpreter — is not lost either, and is not this
/// survey's to hold. It is pinned by
/// `crates/cove-cli/tests/build.rs`'s `a_program_that_cannot_be_lowered_is_refused_before_a_binary_is_written`
/// and by `crates/cove-cli/tests/trace_replay.rs`, both over
/// `crates/cove-cli/tests/fixtures/instantiation_depth`. That fixture lives
/// outside `tests/e2e/`, `examples/` and `benches/` deliberately: it is a
/// program that cannot be lowered, this survey counts such a program as a gap
/// to be filled, and the monomorphisation depth cap is a **refusal** that no
/// later task removes. Teaching this file to tell a permanent refusal from a
/// gap is the work that would let it come back into the corpus.
const AGREEING_FLOOR: usize = 116;

/// The code `cove_ir` raises a gap under.
///
/// Written out rather than imported: it is `pub(crate)` in `cove-ir`'s own
/// `lower::gap`, deliberately, so that a consumer cannot start matching on a
/// per-construct taxonomy that is scheduled to be deleted. This file wants
/// exactly one bit out of it — whether a program was stopped by a construct
/// nobody has built yet, or by something else — and a string compare is a
/// smaller thing to owe than an exported constant.
const NOT_YET_LOWERED: &str = "cove::lower::not_yet_lowered";

/// Every program that lowers and then answers something other than what the
/// oracle answers, and why each one is still here.
///
/// [`AGREEING_FLOOR`] is a ratchet on a number, and a number cannot tell a
/// new disagreement from an old one: a change that teaches this backend one
/// family and breaks another raises the count of what agrees while
/// introducing a program that lowers and lies. So the set is compared as a
/// whole rather than as a bound, so that a disagreement which quietly stopped
/// being one fails this too: a program that started agreeing is a claim
/// somebody should have written down.
///
/// A line here is a bug that has been *seen*, not a bug that has been
/// allowed. Adding one is meant to be awkward: what reaches this list is a
/// program the checker accepted, the lowering emitted code for, and the
/// machine then ran to a different answer than the language's own definition
/// of what it means. Removing a line happens in the change that fixes it.
const KNOWN_DISAGREEMENTS: &[&str] = &[
    // Empty, and that is the state to keep it in: everything this backend
    // lowers and runs answers what the tree-walking oracle answers. The four
    // that used to be here were one fault — `freeze()` refused because a
    // handle is a word and words are not counted — and
    // [issue #240](https://github.com/myuon/cove/issues/240) settled it by
    // establishing uniqueness in the checker rather than asking the machine
    // to. See [`AGREEING_FLOOR`].
];

#[test]
#[ignore = "runs the whole corpus on two backends, and the benchmark rows are \
            two million turns each; CLAUDE.md's local test command leaves the \
            ignored cases out, and CI runs them with \
            `cargo test --workspace --lib --tests -- --ignored`"]
fn the_corpus_says_what_the_linear_memory_backend_runs() {
    // Everything happens on the stack the runtime sizes, for
    // `differential.rs`'s reason: the oracle is a recursive tree walk, a test
    // thread's stack is not one it chose, and every `Value` either side
    // builds belongs to the thread that built it. Only the report comes back.
    let report = cove_runtime::on_cove_stack(survey).expect("a thread to run Cove on");
    let text = report.render();
    print!("{text}");

    assert!(
        report.agreed.len() >= AGREEING_FLOOR,
        "{} program(s) lowered and agreed with the oracle, which is below the \
         floor of {AGREEING_FLOOR}; coverage may rise but never fall\n\n{text}",
        report.agreed.len()
    );

    let mut disagreed: Vec<&str> = report
        .disagreed
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    disagreed.sort_unstable();
    let mut known = KNOWN_DISAGREEMENTS.to_vec();
    known.sort_unstable();
    assert_eq!(
        disagreed, known,
        "the programs that lower and disagree are not the ones \
         `KNOWN_DISAGREEMENTS` names; a program that started answering \
         something the oracle does not is registered by somebody who looked \
         at it, and one that stopped is a line to delete\n\n{text}"
    );
}

/// Every case of the corpus, lowered, run, and compared.
fn survey() -> Report {
    let mut report = Report::default();
    let cases = discover();
    assert!(!cases.is_empty(), "the corpus is empty");
    report.discovered = cases.len();

    // One index per package rather than one per case, as both harnesses that
    // walk this corpus already keep it: what a module reaches is a fact about
    // the package and does not change between two of its cases.
    let mut indexes: BTreeMap<PathBuf, ModuleIndex> = BTreeMap::new();

    // A panic on either side is a finding this test reports rather than an
    // event that ends it, so the hook that would print each one is taken off
    // for the length of the survey: the payload is captured and rendered
    // beside the program that produced it, and a backtrace per case would
    // bury the report under the thing the report is about. It is put back
    // afterwards, because a panic anywhere else in this file is still a
    // panic.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));

    for case in cases {
        let index = indexes
            .entry(case.root.clone())
            .or_insert_with(|| ModuleIndex::of(&case.root));

        // A package that does not check has no program in it, and neither
        // does a case whose entry names no module or function its package
        // declares. `tests/e2e` keeps a dozen of the first kind on purpose,
        // each pinning a check-time diagnostic, so neither is counted as
        // anything this backend did or did not do.
        let Ok(prepared) = Prepared::of(&case, index) else {
            report.no_program.push(case.name.clone());
            continue;
        };

        let (module, entry) = prepared.entry();
        let program = match lower(&prepared.checked, &prepared.sources) {
            Ok(program) => program,
            Err(gaps) => {
                report.not_lowered.push((case.name.clone(), gaps));
                continue;
            }
        };

        let oracle = on_the_oracle(&case, &prepared, module, entry);
        let machine = on_the_machine(&case, &prepared, &program, module, entry);
        if oracle == machine {
            report.agreed.push(case.name.clone());
        } else {
            report
                .disagreed
                .push((case.name.clone(), disagreement(&oracle, &machine)));
        }
    }

    std::panic::set_hook(hook);
    report
}

/// Lowers the checked program, or answers what stopped it.
///
/// A panic is folded into the same answer as a diagnostic, and it is folded
/// in here rather than reported as a run's disagreement because a lowering
/// that panics has produced no program to run: `cove_ir::lower` verifies
/// what it emitted and panics rather than answering when the verifier rejects
/// it, on the grounds that such a fault is a bug in the lowering and not
/// something to report to the person holding the source. That is the right
/// call for a compiler and the wrong one for a survey, which needs the rest
/// of the corpus measured after it.
fn lower(
    checked: &cove_sema::resolve::Program,
    sources: &cove_diag::SourceMap,
) -> Result<cove_ir::Program, Vec<Gap>> {
    let schemas = HostSchemas::new();
    match std::panic::catch_unwind(AssertUnwindSafe(|| {
        cove_ir::lower(checked, sources, &schemas)
    })) {
        Ok(Ok(program)) => Ok(program),
        Ok(Err(items)) => Err(items
            .iter()
            .map(|item| Gap {
                code: item.code.clone(),
                what: item.message.clone(),
            })
            .collect()),
        Err(payload) => Err(vec![Gap {
            code: "panic".to_string(),
            what: format!("the lowering panicked: {}", panic_message(payload.as_ref())),
        }]),
    }
}

/// One reason a program did not lower.
struct Gap {
    /// `cove::lower::not_yet_lowered`, `cove::lower::unknown_type`, or the
    /// one this file makes for a panic. Kept beside the message because the
    /// two mean different work: a gap is a construct to build, and an
    /// unsettled type is a program the checker declined about.
    code: String,
    what: String,
}

/// Every entry point of the repository, in a fixed order.
///
/// `tests/e2e/`, the packages of its own that some of its cases bring,
/// `examples/` and `benches/` — every program the repository keeps, for the
/// reason this file's module docs give. `differential.rs` walks all of it but
/// `benches/`, and its own docs say why.
fn discover() -> Vec<Case> {
    let root = support::repo_root();
    let mut roots = vec![root.join("tests/e2e")];
    roots.extend(support::nested_packages(&root.join("tests/e2e")));
    roots.push(root.join("examples"));
    roots.push(root.join("benches"));

    roots
        .iter()
        .flat_map(|package| support::cases_of(&root, package))
        .collect()
}

// ------------------------------------------------------------- the two runs

/// What one backend made of one case, in the terms both can be asked in.
#[derive(PartialEq, Eq)]
struct Ran {
    /// The value the entry answered or the failure it stopped with,
    /// rendered — a `Value` is `Rc`-based and belongs to the run that built
    /// it, so what is compared is what each side *said*.
    answer: String,
    console: Vec<String>,
    /// The other console stream, kept apart from the first: two streams
    /// compared as one are one stream again the moment a line moves between
    /// them.
    diagnostics: Vec<String>,
    /// How the run ended, classified exactly as `run_entry` classifies it
    /// for the run's terminal trace event — or `None` for a run that did not
    /// end at all, which is what a panic is. There is no `RunOutcome` for
    /// that and inventing one would put a backend under construction into a
    /// vocabulary the language owns.
    outcome: Option<RunOutcome>,
    /// The fake filesystem as the run left it. A program told to write a file
    /// says on the console that it did, and the console line is not the file.
    files: BTreeMap<String, String>,
}

/// Runs the case on the tree-walking interpreter, which is the oracle.
fn on_the_oracle(case: &Case, prepared: &Prepared, module: &str, entry: &str) -> Ran {
    let (fakes, hosts) = Fakes::build(case);
    let runtime = Runtime::new(
        prepared.checked.clone(),
        prepared.sources.clone(),
        Arc::new(hosts),
    );
    let answer = caught(|| Interpreter::new(&runtime).run_entry(module, entry, arguments(case)));
    fakes.observed(answer)
}

/// Runs the same case on the linear-memory machine.
///
/// Through [`Vm`] rather than through anything below it, because the
/// question is about the language and the language's answer includes the
/// boundary: the same entry-shape check, the same materialisation of the
/// answer, the same terminal event. Comparing the dispatch loop against the
/// whole of the oracle would be comparing two different things.
fn on_the_machine(
    case: &Case,
    prepared: &Prepared,
    program: &cove_ir::Program,
    module: &str,
    entry: &str,
) -> Ran {
    let (fakes, hosts) = Fakes::build(case);
    let hosts = Arc::new(hosts);
    let runtime = Runtime::new(
        prepared.checked.clone(),
        prepared.sources.clone(),
        hosts.clone(),
    );
    let answer =
        caught(|| Vm::new(&runtime, &hosts, program).run_entry(module, entry, arguments(case)));
    fakes.observed(answer)
}

/// One run, with a panic turned into an answer.
///
/// A backend under construction panics where it has not been finished, and a
/// panic that ended the survey would hide every program after it. What it
/// must not do is compare equal to anything: a panicking run and an oracle
/// that answered are a disagreement, which is exactly what it is.
fn caught(
    run: impl FnOnce() -> Result<Value, RuntimeError>,
) -> Result<Result<Value, RuntimeError>, String> {
    std::panic::catch_unwind(AssertUnwindSafe(run))
        .map_err(|payload| panic_message(payload.as_ref()))
}

/// What a panic said, out of the payload `catch_unwind` hands back.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "a panic carrying no message".to_string()
    }
}

/// The process arguments the entry is handed, as both backends take them.
fn arguments(case: &Case) -> Vec<Rc<str>> {
    case.args.iter().map(|arg| arg.as_str().into()).collect()
}

/// The message a disagreement is reported with: both sides, in full.
///
/// ADR 0012 presumes the oracle right, so its answer is named first and named
/// as the oracle. Which side is wrong is still a judgement, and this is what
/// somebody makes it from.
fn disagreement(oracle: &Ran, machine: &Ran) -> String {
    let mut out = String::new();
    let mut side = |which: &str, ran: &Ran| {
        let _ = writeln!(out, "    {which}:");
        let _ = writeln!(out, "      outcome: {:?}", ran.outcome);
        let _ = writeln!(out, "      answer:  {}", ran.answer);
        if !ran.console.is_empty() {
            let _ = writeln!(out, "      console: {:?}", ran.console);
        }
        if !ran.diagnostics.is_empty() {
            let _ = writeln!(out, "      diagnostics: {:?}", ran.diagnostics);
        }
        if !ran.files.is_empty() {
            let _ = writeln!(out, "      files: {:?}", ran.files.keys());
        }
    };
    side("ast (the oracle)", oracle);
    side("vm", machine);
    out
}

// --------------------------------------------------------------- the fakes

/// What a run can be observed through, kept where this test can read it back
/// once the run is over.
///
/// The deterministic fakes `differential.rs` and `examples.rs` already run
/// against — a console that is a buffer, a virtual clock that moves only when
/// something moves it, an in-memory filesystem seeded from the package's own
/// `files/` — so nothing here reaches the network or a real clock and every
/// answer is the same on every machine.
///
/// They are built here rather than shared with `differential.rs` because the
/// two harnesses observe different things: that one records a trace through
/// its fakes and compares it, and this one does not. A shared fixture would
/// have to carry the union and let each read the half it wanted.
/// `tests/support/mod.rs` holds what the two genuinely share, which is how a
/// corpus case is found and checked.
struct Fakes {
    console: Buffer,
    diagnostics: Buffer,
    files: Tree,
}

impl Fakes {
    fn build(case: &Case) -> (Fakes, HostRegistry) {
        let console = Buffer::default();
        let diagnostics = Buffer::default();
        let files = Files::in_memory(seeded(&case.root.join("files")));
        let tree = files.tree();

        // Every host is registered whether or not this case reaches it,
        // exactly as `cove run` registers them: the grants are what decide,
        // so a capability a program reaches for without holding is refused
        // with the reason rather than with a missing module.
        let mut hosts = HostRegistry::new(Grants::new(case.run.allow.clone()));
        hosts.register(Box::new(Console::new(console.clone(), diagnostics.clone())));
        hosts.register(Box::new(Env::new(BTreeMap::new())));
        hosts.register(Box::new(Documents::in_memory(seeded(
            &case.root.join("documents"),
        ))));
        hosts.register(Box::new(Clock::virtual_clock(VirtualTime::new())));
        hosts.register(Box::new(Database::recorded(BTreeMap::new())));
        hosts.register(Box::new(Http::recorded(BTreeMap::new(), Vec::new())));
        hosts.register(Box::new(Process::recorded(
            case.args.clone(),
            BTreeMap::new(),
            ProcessLog::new(),
        )));
        hosts.register(Box::new(files));
        hosts.set_budget(Budget::with_cancellation(
            limits(&case.run),
            Cancellation::new(),
        ));

        (
            Fakes {
                console,
                diagnostics,
                files: tree,
            },
            hosts,
        )
    }

    /// What the run left behind, beside what it answered.
    fn observed(self, answer: Result<Result<Value, RuntimeError>, String>) -> Ran {
        let outcome = match &answer {
            Ok(Ok(value)) if value.is_err() => Some(RunOutcome::Error),
            Ok(Ok(_)) => Some(RunOutcome::Success),
            Ok(Err(error)) => Some(error.outcome),
            Err(_) => None,
        };
        Ran {
            answer: describe(&answer),
            console: self.console.lines(),
            diagnostics: self.diagnostics.lines(),
            outcome,
            files: self.files.files(),
        }
    }
}

/// The budgets a case runs under.
///
/// Everything `[run.<name>]` sets except fuel and the deadline, for
/// `differential.rs`'s reason: fuel is backend-specific — an instruction is
/// not an AST node — and a deadline is wall-clock, so either would make the
/// two sides stop at different points by construction rather than by fault.
fn limits(run: &RunConfig) -> Limits {
    Limits {
        fuel: None,
        deadline: None,
        max_host_calls: run.max_host_calls,
        max_call_depth: None,
        max_tasks: run.max_tasks,
    }
}

/// One run's answer, rendered so that two of them can be compared and either
/// of them read.
///
/// A failure is rendered by its structure rather than by its message alone —
/// what it said, how it classified itself, which capability the boundary
/// refused, and the rule it cited. The span is left out, and that is the one
/// thing this comparison is weaker about than `differential.rs`'s: the
/// lowering is not yet carrying a span through every construct, and comparing
/// them today would report programs as disagreeing about where a failure is
/// while agreeing about everything else, which is not the finding this file
/// is looking for. It joins when the spans do.
fn describe(answer: &Result<Result<Value, RuntimeError>, String>) -> String {
    match answer {
        Ok(Ok(value)) => format!("value {value:?}"),
        Ok(Err(error)) => format!(
            "failed {:?}: {}\n      rule: {:?}\n      denied: {:?}",
            error.outcome, error.message, error.rule, error.denied_capability,
        ),
        Err(message) => format!("panicked: {message}"),
    }
}

/// One of a `console`'s streams, which a run writes to and this test reads
/// back.
///
/// A poisoned lock is taken back rather than treated as a second failure,
/// which is the one thing this differs in from the buffer `differential.rs`
/// keeps. There a run that panicked mid-line was a bug in a finished
/// backend; here it is a measurement, and the lines written before the panic
/// are exactly what the report is for.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Buffer {
    fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|held| held.into_inner()))
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|held| held.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A package's own fixture directory, read into the in-memory host that
/// stands for it.
///
/// Reads answer what the case's fixtures actually hold, so a case that reads
/// a file is compared having read it; writes land in memory, so a run cannot
/// change the repository it was read out of.
fn seeded(dir: &Path) -> BTreeMap<String, String> {
    let mut into = BTreeMap::new();
    read_tree(dir, String::new(), &mut into);
    into
}

/// Every readable file below `dir`, keyed by its `/`-separated path from it.
fn read_tree(dir: &Path, prefix: String, into: &mut BTreeMap<String, String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let key = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if path.is_dir() {
            read_tree(&path, key, into);
        } else if let Ok(text) = std::fs::read_to_string(&path) {
            into.insert(key, text);
        }
    }
}

// -------------------------------------------------------------- the report

/// What the survey found.
#[derive(Default)]
struct Report {
    discovered: usize,
    /// Cases with no checked program behind them: a package that pins a
    /// check-time diagnostic, or an entry naming something its package does
    /// not declare. Neither reached a backend.
    no_program: Vec<String>,
    /// Each program that lowered, ran, and said what the oracle said.
    agreed: Vec<String>,
    /// Each program that lowered, ran, and said something else, with both
    /// sides written out.
    disagreed: Vec<(String, String)>,
    /// Each program that did not lower, and every gap that stopped it.
    not_lowered: Vec<(String, Vec<Gap>)>,
}

impl Report {
    fn render(&self) -> String {
        let ran = self.agreed.len() + self.disagreed.len();
        let mut out = format!(
            "\nlinear-memory backend over {} corpus program(s):\n  \
             {:>3} lower and agree with the oracle\n  \
             {:>3} lower and disagree\n  \
             {:>3} ran in total\n  \
             {:>3} do not lower\n  \
             {:>3} have no checked program to lower\n",
            self.discovered,
            self.agreed.len(),
            self.disagreed.len(),
            ran,
            self.not_lowered.len(),
            self.no_program.len(),
        );

        if !self.agreed.is_empty() {
            out.push_str("\nlower and agree, by name:\n");
            for name in &self.agreed {
                let _ = writeln!(out, "       {name}");
            }
        }

        if !self.disagreed.is_empty() {
            out.push_str(
                "\nlower and disagree — every one of these is a bug, and the \
                 oracle is presumed right:\n",
            );
            for (name, detail) in &self.disagreed {
                let _ = writeln!(out, "  {name}:");
                out.push_str(detail);
            }
        }

        self.render_gaps(&mut out);
        out
    }

    /// The gaps, ranked by how many programs each one blocks.
    ///
    /// A program counts once per distinct gap it raises, however many places
    /// raise it, because what is being ranked is programs blocked and not
    /// lines of source. A program blocked by several gaps counts towards each
    /// of them and is cleared by none of them alone — which is why the
    /// sole-blocker list underneath exists: those are the gaps that would
    /// each, on their own, let another program run.
    fn render_gaps(&self, out: &mut String) {
        if self.not_lowered.is_empty() {
            return;
        }
        let mut blocked: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        for (name, gaps) in &self.not_lowered {
            for gap in gaps {
                blocked.entry(&gap.what).or_default().insert(name);
            }
        }
        let mut ranked: Vec<(&&str, &BTreeSet<&str>)> = blocked.iter().collect();
        ranked.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));

        out.push_str("\nwhat is not lowered, by how many programs it blocks:\n");
        for (what, programs) in &ranked {
            let _ = writeln!(out, "  {:>3}  {what}", programs.len());
            let _ = writeln!(
                out,
                "       first at {}",
                programs.iter().next().expect("a gap blocks a program")
            );
        }

        let sole = self.sole_blockers();
        if !sole.is_empty() {
            out.push_str(
                "\ngaps that alone block a program, so building one runs it, \
                 most first:\n",
            );
            for (what, programs) in &sole {
                let _ = writeln!(out, "  {:>3}  {what}", programs.len());
                let _ = writeln!(out, "       {}", programs.join(", "));
            }
        }

        let declined: Vec<&str> = self
            .not_lowered
            .iter()
            .filter(|(_, gaps)| gaps.iter().any(|gap| gap.code != NOT_YET_LOWERED))
            .map(|(name, _)| name.as_str())
            .collect();
        if !declined.is_empty() {
            out.push_str(
                "\nstopped by something that is not a gap — a type the checker \
                 never settled, or a panic in the lowering:\n",
            );
            for name in declined {
                let _ = writeln!(out, "       {name}");
            }
        }
    }

    /// Every gap that is the only one its program raises, ranked by how many
    /// programs it alone would clear.
    ///
    /// This is the answer to "which one next": a program with two gaps still
    /// does not lower after one of them is built, so only a program whose
    /// whole set is one gap is cleared by building that one.
    fn sole_blockers(&self) -> Vec<(&str, Vec<&str>)> {
        let mut by_gap: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (name, gaps) in &self.not_lowered {
            let distinct: BTreeSet<&str> = gaps.iter().map(|gap| gap.what.as_str()).collect();
            let mut only = distinct.into_iter();
            if let (Some(what), None) = (only.next(), only.next()) {
                by_gap.entry(what).or_default().push(name);
            }
        }
        let mut ranked: Vec<(&str, Vec<&str>)> = by_gap.into_iter().collect();
        ranked.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
        ranked
    }
}
