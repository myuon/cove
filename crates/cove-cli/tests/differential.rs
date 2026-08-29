//! The whole corpus, run through both backends, compared answer for answer.
//!
//! [ADR 0019](../../../docs/adr/0019-executable-ir-and-vm.md) leaves Cove with
//! two executable answers to what a program means, and says they must be kept
//! in agreement by tests rather than by hope.
//! [Issue #111](https://github.com/myuon/cove/issues/111) is the gate that
//! decides when the VM becomes the default, and this is its evidence: every
//! program the repository already keeps — every `[run.<name>]` under
//! `tests/e2e/`, `examples/`, and `benches/` — lowered, and then run on the
//! interpreter and on the VM against the same deterministic fakes.
//!
//! # A refusal is coverage, a disagreement is a failure
//!
//! `cove_ir::lower` refuses what it does not cover, so most of this corpus
//! does not reach the VM today. That is the measurement rather than the
//! problem: a case the lowering refuses is recorded with the construct it
//! named and counted, and the counts are printed as the roadmap for what to
//! lower next. A case that *does* lower and then answers differently on the
//! two backends is a failure, and the message shows both sides, because ADR
//! 0012 ranks the oracle above a backend and a backend that disagrees with
//! the oracle is wrong.
//!
//! Two assertions make this a ratchet rather than a report: everything that
//! lowers agrees, and the number of cases that lower never falls below
//! [`LOWERED_FLOOR`].
//!
//! # A case is a program, not a package
//!
//! `tests/e2e/` is seventy unrelated programs sharing one package for the
//! convenience of the harness that runs them, so a case is measured as the
//! program it is rather than as the package it sits in — and it is measured
//! that way twice over.
//!
//! Checking is sliced by module: a case is parsed and checked as its entry's
//! module plus the modules that module's `use` declarations reach,
//! transitively. That slicing is not a workaround but the corpus's own
//! shape: `tests/e2e/` keeps a dozen modules that deliberately do not check,
//! each pinning a check-time diagnostic, and a package holding one of those
//! does not check as a whole.
//!
//! Lowering is sliced by reachability: `cove_ir::lower::lower_entry` lowers
//! what the entry can reach and nothing else, so a construct the VM cannot
//! run refuses only the cases whose entry reaches it. This is the same call
//! `cove run --backend vm` makes with the same entry, so what this harness
//! measures and what the CLI runs are one program rather than two that could
//! drift.
//!
//! # What is compared, and what is not
//!
//! The value the entry answered or the structured error it failed with, every
//! line written to the fake console in order, how the run ended, the fake
//! filesystem as the run left it, and the trace the run wrote. Fuel is not
//! compared: ADR 0019 makes `fuel_spent` backend-specific, since an
//! instruction is not an AST node and there is no honest mapping between
//! them.
//!
//! An error's source position is compared exactly. It did not have to be —
//! an instruction's span covers the operation it came from and a tree walk's
//! covers the expression node, so the two could name one failure from a byte
//! apart — but across everything that lowers today they do not, and asserting
//! the weaker property would be recording less than is true.
//!
//! The hosts are the deterministic fakes `examples.rs` and `cove-bench`
//! already run against — a console that is a buffer, a virtual clock that
//! moves only when something moves it, an in-memory filesystem seeded from
//! the package's own `files/`, recorded documents, http, and rows — so
//! nothing here reaches the network or a real clock, and every answer is the
//! same on every machine.
//!
//! Budgets come from `[run.<name>]` except fuel and the deadline, which are
//! left off on purpose: fuel is backend-specific by ADR 0019, and a deadline
//! is wall-clock, so bounding either would make the two backends disagree by
//! construction rather than by fault. No case in the corpus sets one today.
//!
//! # The trace, and what a normalization is allowed to drop
//!
//! Issue #111 asks for "source-level trace events after backend-specific
//! normalization", so every case that lowers is run with a
//! `cove_runtime::trace::JsonlSink` on both backends and the two recordings
//! are compared. The recording rather than the events: the JSONL is what
//! `cove trace` reads and what `cove replay` consumes, so comparing the lines
//! compares the artifact somebody else's program is handed, and a field that
//! stops being written is a change to that artifact whether or not the event
//! behind it still exists.
//!
//! A field dropped because it differed is exactly how a real divergence
//! hides, so nothing below is dropped for differing. Each exclusion is a
//! property of a backend rather than of the program, each was established by
//! running the corpus rather than by assuming, and [`Trace::of`] is where
//! each one is made and argued.
//!
//! What survives the normalization is compared exactly and agrees over the
//! whole corpus: `entry_enter` and `entry_exit`'s module and function, every
//! `host_call`'s task, module, operation, capability, grant, arguments and
//! outcome, `task_spawned`'s id, parent and scope, `run_ended`'s outcome and
//! message, and what `heap_summary` says a run allocated. The task ids
//! themselves agree because both backends draw them from the one counter
//! `cove_runtime::runtime::Runtime` holds, so there was no renumbering to
//! normalize.
//!
//! No trace event carries `fuel_spent`. ADR 0019's backend-specific figure
//! reaches `cove run --stats` and never the trace, so there was nothing here
//! to exclude for it.
//!
//! # Reading the coverage summary
//!
//! ```console
//! $ cargo test -p cove-cli --test differential -- --nocapture
//! ```
//!
//! The summary is printed on every run and repeated in the message of either
//! assertion that fails, so a failing run carries it without being asked.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use cove_diag::SourceMap;
use cove_runtime::budget::{Budget, Cancellation, Limits};
use cove_runtime::clock::{Clock, VirtualTime};
use cove_runtime::database::Database;
use cove_runtime::error::RuntimeError;
use cove_runtime::files::{Files, Tree};
use cove_runtime::host::{Console, Documents, Env, Grants, HostRegistry};
use cove_runtime::http::Http;
use cove_runtime::interp::Interpreter;
use cove_runtime::process::{Process, ProcessLog};
use cove_runtime::runtime::{Runtime, ENTRY_TASK};
use cove_runtime::trace::{JsonlSink, RunOutcome, TraceHeader, TraceSink, ValueCapture};
use cove_runtime::value::Value;
use cove_runtime::vm::Vm;
use cove_sema::config::RunConfig;
use cove_sema::package::{Module, Package, Unit};
use cove_sema::resolve::Program as Checked;

/// How many corpus cases the lowering covers today.
///
/// A floor, not a target. Lowering one more construct raises it; nothing may
/// lower it, because a case that stopped lowering would be coverage lost
/// silently, and the whole point of counting is that it cannot be. Raise this
/// number in the same change that raises the coverage.
///
/// 55 to 56: a range used as a value builds one now — `cove_ir::Inst::MakeRange`
/// takes two `Int` bounds off the scalar stack and leaves the `Value::Range`
/// they make on the value stack — where the lowering previously had no
/// instruction that made one and refused every range a `for` header did not
/// consume. `tests/e2e:values_range` is the case that gained a lowering, and
/// it is the only one the corpus held.
///
/// It stayed at 56 when a variadic parameter began to lower, and that is
/// worth recording rather than leaving as a number that did not move.
/// `tests/e2e:fn_variadic` is the only case in the corpus that declares one,
/// and it also spreads — `joinAll("-", ...ready)` — so it now refuses for
/// the `...` standing behind the variadic parameter rather than for the
/// parameter itself. A spread is its own construct: it reads an `Array` or a
/// `Vector` and refuses anything else, which is a runtime question and not
/// one `make-array` answers.
///
/// 56 to 59: a method on a host resource handle is dispatched through the
/// boundary that issued the handle — `cove_ir::Inst::CallResource` stands the
/// handle below the arguments and lets `HostRegistry::call_resource` read the
/// module and the resource kind off it — where the lowering previously looked
/// the name up among the declared types and the builtins, found neither, and
/// refused. `tests/e2e:fail_http_stale_handle`, `tests/e2e:host_http_resource`
/// and `tests/e2e:host_files_streaming` are the cases that gained a lowering.
///
/// Six cases refused for a resource method and only three of them lower, which
/// is worth recording rather than leaving as a gap between two numbers. The
/// other three hold a second construct this backend does not cover and now
/// refuse for that instead: `examples:cq` and `examples:cqSample` for
/// `freeze`, and `examples:server` for `http.Route`, which initializes a type
/// a host declares.
/// 64 to 68: `freeze` takes the place rather than a read of it.
/// `builtins::freeze` consumes uniquely owned storage and refuses when a
/// second alias observes the vector, so a read of the receiver would be that
/// second alias and the check would refuse every vector `freeze` is written
/// for. `tests/e2e:coll_freeze`, `tests/e2e:fail_freeze_aliased`,
/// `examples:values` and `examples:cqSample` gained a lowering — the second
/// of those being the case that pins the refusal, which now happens
/// identically on both backends.
///
/// `examples:cq` refused for `freeze` too and did not gain one. It refuses
/// for `step: foldRevenue`, a function used as a value, which is one
/// construct further down the same program.
///
/// 68 to 70: a call that leaves a parameter to its default reaches a
/// specialisation of the callee. A default is evaluated by the callee — the
/// interpreter's `bind_params` reaches `None => match &param.default` inside
/// the frame it is filling — so a call that omits one is not the same call
/// with fewer arguments; it is a call to a function whose prologue computes
/// the rest. `cove_ir::lower` numbers one function per supplied-set, which
/// keeps the arity a call passes and the arity the callee takes the same
/// number and leaves the calling convention where it was.
/// `tests/e2e:fn_defaults` and `tests/e2e:fn_recursion` are the cases that
/// gained a lowering.
///
/// It stayed at 70 when `http.Route` began to lower, and that is worth
/// recording rather than leaving as a number that did not move.
/// `examples:server` is the only case in the corpus that initializes a type
/// a host declares, and the line that does it also writes `http.Method.Get`
/// — a case of an enum a host declares, which is a construct of its own —
/// so the case now refuses for that instead. The function it passes as
/// `handler:` is a third.
///
/// 70 to 71: `snapshot` splits by the receiver's type rather than by its
/// name. A struct or an enum with an `impl Snapshot for Type` was already a
/// `Call`, because the checker records which declaration that call reaches;
/// what was refused outright is the half of the trait no conformance answers
/// for, and `cove_ir::Inst::Snapshot` is that half — a `Vector`, which
/// allocates storage of its own, and every value with nothing mutable inside
/// it, which returns itself. A `Vector` whose elements would each dispatch is
/// still refused, because an instruction cannot run a whole Cove function in
/// the middle of itself. `tests/e2e:type_snapshot` is the case that gained a
/// lowering, and it is the only one the corpus held.
///
/// 71 to 72: a `...` argument spreads a sequence into a variadic parameter.
/// A variadic parameter receives one `Array` and `cove_ir::Inst::MakeArray`
/// already built it out of the leftover arguments; a spread is the same array
/// built out of a value, so `cove_ir::Inst::SpreadArgument` appends what one
/// holds — an `Array`'s elements or a `Vector`'s, and nothing else, which is
/// the pair `bind_params` reads. A call that mixes the two builds the array
/// in runs. Everywhere a variadic parameter is *not*, the interpreter reads a
/// spread argument's value and ignores its marking, so those are refused
/// rather than reproduced. `tests/e2e:fn_variadic` is the case that gained a
/// lowering, and it is the only one the corpus held.
///
/// 72 to 74: a lambda is lowered to a function of its own and the values the
/// environment around it handed over. `cove_ir::Function::captures` was
/// scaffolding until now — an explicit list with an explicit layout, decided
/// when the lambda is lowered rather than when the closure is created, which
/// is ADR 0019's "slots, not names" asked of a capture — and
/// `cove_ir::Inst::MakeClosure` fills it while `cove_ir::Inst::CallValue`
/// enters one. `tests/e2e:closures` and `tests/e2e:gc_cycles` are the cases
/// that gained a lowering.
///
/// Two of the three cases that refused for a closure are what moved.
/// `tests/e2e/backend_unsupported:backend_unsupported` is the third and did
/// not: it exists to pin ADR 0019's no-silent-fallback rule, so it was
/// rewritten around a task scope, which the lowering still refuses. What the
/// case is about is the rule and not the construct.
///
/// 74 to 76: a trailing closure is the last positional argument.
/// `Interpreter::eval_args` evaluates the written arguments and then pushes
/// the trailing one on the end with no label, no `var` and no spread, and the
/// parser has already built the block as a lambda — so once a lambda lowers
/// there is nothing left for the sugar to do but land where a written
/// argument would. `Args` is that said once, rather than a second parameter
/// every path that reads a call's arguments would have to remember to use.
/// `tests/e2e:type_result` and `examples:config` are the cases that gained a
/// lowering.
///
/// 76 to 77: a declared function used as a value is a closure over nothing.
/// `Interpreter::eval_ident` builds one with `captures: Vec::new()`, because
/// a declaration reads no environment — the whole of what makes a function a
/// value is that it can be called through one. The specialisation a closure
/// is made of is not the one a direct call reaches: `cove_ir::Inst::CallValue`
/// puts every argument on the value stack and reads the answer off it, and a
/// convention is what a slot number means, so the body is lowered a second
/// time under that convention. `examples:cq` — `step: foldRevenue` — is the
/// case that gained a lowering, and it is the only one the corpus held.
///
/// 77 to 78: a `dyn Trait` value is built where one is written, and a method
/// called on one dispatches from the value rather than from the type.
/// `cove_ir::Inst::MakeDyn` is the language's one implicit conversion, made
/// at the four places a type is *written* — a parameter, an annotated `let`,
/// a struct's field, and a declared return type — which is where
/// `Interpreter::coerce` makes it. `cove_ir::Inst::CallDyn` is the other
/// half: a lookup by the concrete type's name over every implementation the
/// package declares, which is the first call in this IR whose target is not
/// a `FunctionId` written into the instruction.
/// `tests/e2e/outline_dyn_field:app` is the case that gained a lowering.
///
/// Three more cases refused for a `dyn` parameter and none of them gained
/// one, which is worth recording rather than leaving as a gap between two
/// numbers. `tests/e2e:module_conformance`, `tests/e2e:type_trait` and
/// `examples:traits` each also call a method on a value whose type is a
/// *bounded type parameter* — `render<T: Display>(value: T)`,
/// `headline<T: Summary>(entry: T)` — and each also reaches a trait's
/// default body, whose `self` is `Self: Trait` and is the same construct
/// again. A call through a trait bound is not a call through a `dyn`, and
/// the lowering has no name for it, so all three now refuse as "a call to
/// `summarize`, which no declared type and no builtin has": the name is
/// all that is left once the receiver's type turns out to be one this pass
/// cannot resolve a method against.
///
/// 78 to 81: a call on a value whose type is a bounded type parameter, or
/// the rigid `Self` of a trait's default body, is the same dispatch a `dyn`
/// gets. `Interpreter::eval_method_call` draws no distinction between the
/// three — it reads the concrete value's own type name and looks the method
/// up from there — so `cove_ir::Inst::CallDyn` serves all three, and the
/// only thing the lowering takes from the static type is which trait the
/// call goes through. Without it a `dyn` dispatch could not reach a
/// method a conformance left to the trait's default, which is half of what
/// a trait is. `tests/e2e:module_conformance`, `tests/e2e:type_trait` and
/// `examples:traits` are the cases that gained a lowering, and they are the
/// three the previous change left behind.
///
/// 81 to 82: `http.Method.Get`, a case of an enum a *host* declares. It has
/// a `cove_schema::TypeSchema` rather than an `EnumDecl`, so
/// `cove_ir::Inst::MakeEnum` — which the VM shapes from a declaration this
/// package holds — could not serve it, and it carries no payload, so a
/// second instruction naming the type and the case is the whole of it.
/// Both backends reach `interp::host_enum_case`, so a case the schema does
/// not name fails in the same words on either. `examples:server` is the
/// case that gained a lowering, and it needed the two changes before this
/// one as well: a type a host declares, and a function used as a value.
/// 82 to 86: a task scope, and a VM of its own for each task spawned into
/// it. ADR 0008 gives every task an evaluator, and `cove_runtime::vm::Vm` is
/// now one of the two things that can be one — over the same `Runtime` and
/// the same `cove_ir::Program`, which a run shares because a lowered
/// closure's `FunctionId` has to mean the same function on both sides of the
/// boundary. Everything a `spawn`, an `await` and a scope exit decide lives
/// in `cove_runtime::task`, which both backends call, so there is no second
/// statement of the task-safety rule for the two to drift apart on.
/// `tests/e2e:fail_max_tasks`, `tests/e2e:gc_tasks`,
/// `tests/e2e:tasks_host_order` and `tests/e2e:tasks_scope` are the cases
/// that gained a lowering.
///
/// A fifth case refused for a task scope and did not gain one.
/// `tests/e2e/backend_unsupported:backend_unsupported` exists to pin ADR
/// 0019's no-silent-fallback rule, so it was rewritten again — around a call
/// whose labelled arguments stand out of declaration order, which is refused
/// deliberately rather than for want of work: the interpreter answers such a
/// call by evaluating the arguments in the order they were *written*, and
/// issue #112 is about moving that decision into the checker. It was a
/// closure, then a task scope; what the case is about is the rule and not the
/// construct. (ADR 0021 moved that decision, so the case has moved again;
/// the note at the end of this comment says where.)
///
/// One scope shape is refused and stays refused, which is worth recording
/// because it is a wall rather than unfinished work. A child whose value is
/// `Err(...)` returns that failure from the function the scope was written
/// in, and `Interpreter::leave_scope` does that whatever the declared return
/// type is — `fn f() -> Int { scope s { ... } }` answers `Err(boom)` on the
/// oracle. A function the checker settled as answering `Int` or `Bool`
/// returns on the scalar stack and every one of its returns is a
/// `return-scalar`, so there is no stack for that failure to travel on. The
/// lowering refuses such a scope rather than approximating it.
/// 86 to 88: `Shared`, which is ADR 0008's other half — the one value that
/// crosses a task boundary by sharing rather than by copying.
/// `Shared(value)` is an ordinary constructor and `lock` is one instruction
/// over `cove_runtime::shared::SharedCell::lock`, so what a cell refuses to
/// wrap, what holding it means, and what a cycle through it costs are all the
/// oracle's. `tests/e2e:fail_shared_cycle` and `tests/e2e:tasks_shared` are
/// the cases that gained a lowering, and they are the two the corpus held.
///
/// The one thing that is not the oracle's is the call. A closure written
/// `fn(var value)` names the cell's contents rather than receiving a copy of
/// them, and every argument of a `call-value` travels on the value stack, so
/// `cove_ir::Inst::Lock` makes the call itself: the contents stand in a value
/// slot of the locking frame and the closure is handed a place rooted there.
/// A `lock` whose closure is a *value* rather than written at the call is
/// therefore refused, which is narrower than the oracle and is what keeps
/// such a closure from ever reaching a `call-value`.
/// 88 to 90: an `async fn`, which is the last of the concurrency cluster.
/// ADR 0008 gives a thread to `spawn` and not to every `async fn`, so one
/// runs its body at the call site and answers a handle that is already
/// settled — `Interpreter::invoke` wraps the result of the whole call, and
/// `cove_ir::Function::answers_a_task` is the same fact read where the VM
/// closes the frame, which is what catches a `?` that failed as well as a
/// `return`. It is the callee's answer and not the call site's, because an
/// `async fn` used as a value is called through a `call-value` and nothing
/// there knows which function it will reach. `examples:callbacks` and
/// `examples:tasks` are the cases that gained a lowering, and they are the
/// two the corpus held.
///
/// What is left refused is what was always going to be. Three cases name a
/// call whose labelled arguments stand out of declaration order and one an
/// assignment to a read-only place; both are programs the oracle rejects at
/// *run* time and this refuses at lowering, which is deliberate and is what
/// issues #112 and #113 are about moving into the checker. Nothing in the
/// corpus is refused for want of an execution model any more. (ADR 0021
/// moved all four; the note at the end of this comment says where they
/// went.)
///
/// 90 to 81, and it is the one time this number has gone *down*. Nine
/// benchmarks left the corpus, not the lowering: `benches/` is no longer one
/// of the corpora, for the reason `discover` gives, and all nine of its
/// cases lowered and agreed on the day they went. Nothing stopped lowering
/// and nothing stopped agreeing. A floor that falls for any other reason is
/// the regression this constant exists to catch, and lowering it should
/// always cost a paragraph saying which.
///
/// 81 to 87: six cases, and none of them a construct. `tests/e2e:gc_capture`,
/// `gc_churn`, `gc_frames`, `gc_graph`, `gc_place` and `gc_reentry` are the
/// programs issue #119 asks for — a graph one member of which stays rooted, a
/// collection with frames standing above frames and a value operand of the
/// outermost still on the stack, a capture that is the only root left, a
/// place written through across a collection, and a collection inside a body
/// the host is running re-entrantly. Every one of them lowered on the day it
/// was written, which is the measurement: what they exercise is collection,
/// and collection is not something a lowering has to reach.
///
/// 87 to 88: `tests/e2e:gc_struct`, the program issue #128 asks for. It is
/// the one case in this corpus whose two answers came apart because of a
/// collector bug rather than a lowering one, and the oracle is the side that
/// was wrong: the interpreter printed an emptied vector where the VM printed
/// the one the program built. It lowered on the day it was written, for the
/// same reason the six before it did.
///
/// 88 to 89: one case and no construct again.
/// `tests/e2e:host_console_streams` writes records to `console.println` and
/// complaints to `console.eprintln`, and it lowered on the day it was
/// written. That is the whole of what issue #102 had to check about the
/// backends: a second stream is a second operation on a module,
/// `cove_ir::Inst::CallHost` already carries the module and the operation as
/// names, and there was no instruction to add. What the case measures is the
/// fake console, which now has a buffer per stream so that a line written to
/// the other one on one backend is a disagreement rather than a coincidence.
///
/// It was briefly two. A second case, `fail_console_error_not_granted`, was
/// written and then removed before the change merged: it pinned a run that
/// granted `console` and not `console.error`, and the repository decided
/// against a second capability, so there is no refusal for it to pin. The
/// floor is 89 rather than 90 because that case is gone and for no other
/// reason — nothing stopped lowering.
///
/// 89 to 93: four cases, and no construct again — but for the first time the
/// four are what this harness is *for*. `Array` and `Vector` gained `map`,
/// `filter`, `fold`, and `sorted(by:)`, which are the language's first
/// higher-order builtins on a collection, and a higher-order builtin is the
/// first thing to drive the two `cove_runtime::builtins::Callable`
/// implementations against each other over whole programs rather than over
/// one `mapError` callback. `Result.mapError` was that one callback and it
/// runs at most once per value; a `sorted` over eight elements re-enters the
/// evaluator seventeen times from inside a single instruction, on the VM
/// through `Vm::call_from_host` — the re-entrant loop that leaves the
/// interrupted instruction's operands standing — and on the interpreter
/// through an ordinary recursive call. `tests/e2e:coll_sorted`,
/// `tests/e2e:coll_transform`, `tests/e2e:fail_sort_callback` and
/// `tests/e2e:fail_map_host_calls` are the cases that gained a lowering, and
/// all four lowered on the day they were written: the lowering already
/// emitted `cove_ir::Inst::CallBuiltin` for any name the shared table
/// declares, and a callback is an ordinary `make-closure`, so there was no
/// instruction to add.
///
/// The last two are the ones worth having. `fail_sort_callback` is a
/// comparison that divides by zero partway through a merge, and both
/// backends stop at the same byte of the same closure with nothing
/// half-sorted printed. `fail_map_host_calls` is a `map` whose transform
/// prints, under a `max_host_calls` this file passes through from
/// `[run.<name>]`: the budget stops the run from *inside* a callback, on the
/// same element on both backends, which is the evidence that a runtime
/// control is accounted the same on either side of a re-entry. A run
/// cancelled inside a callback has no case here, because cancellation is
/// reachable only through a task and a task's cancellation is a race no
/// golden file can pin; the budget stop is the deterministic member of the
/// same family, and it is what this records instead.
///
/// It then did not move at all when three of the four refusals went, and
/// that is worth recording rather than leaving as a number that stood
/// still. ADR 0021
/// made assignment to a read-only place and a labelled argument out of
/// declaration order `cove check` errors, so `tests/e2e:fail_assign_let`,
/// `tests/e2e:fn_labels` and `tests/e2e:type_struct` moved from *refused* to
/// *does not check* — 4 refused and 25 not checking became 1 and 28, over a
/// corpus the four cases above had meanwhile grown to 122 — and
/// each is now a package of its own, so their case names gained a directory.
/// Nothing gained a lowering and nothing lost one. The refusals went because
/// the checker catches them, not because the VM learned anything.
///
/// The one refusal left is `tests/e2e/backend_unsupported:backend_unsupported`,
/// which pins ADR 0019's no-silent-fallback rule and had to be rewritten a
/// fourth time: a program `cove check` refuses never reaches a backend, so a
/// construct this pass refuses *because the program is wrong* can no longer
/// pin what a backend does with one. It names a function declared inside a
/// function body now, which is unsupported in the plain sense — the
/// interpreter runs it and the lowering has no instruction for it.
const LOWERED_FLOOR: usize = 93;

// ------------------------------------------------------------------ the test

/// Every case in the corpus, on both backends.
///
/// One `#[test]` rather than one per case: the corpus is discovered rather
/// than declared, so there is nothing to hang a test attribute on, and a
/// single run is what makes the coverage summary a summary.
#[test]
fn both_backends_agree_wherever_the_lowering_reaches() {
    // Everything happens on the stack the runtime sizes: the interpreter is
    // a recursive tree walker, a test thread's stack is not one it chose, and
    // every `Value` either backend builds belongs to the thread that built
    // it. The lowering could cross — a `cove_ir::Program` is shared by every
    // thread of a run, which is what lets a spawned task run one — but it has
    // no reason to, since what it is for is on the far side. Only the report
    // comes back out.
    let report = cove_runtime::on_cove_stack(run_the_corpus).expect("a thread to run Cove on");
    let summary = report.summary();
    print!("{summary}");

    assert!(
        report.disagreements.is_empty(),
        "{} case(s) answered differently on the two backends:\n\n{}\n{summary}",
        report.disagreements.len(),
        report.disagreements.join("\n")
    );
    assert!(
        report.lowered.len() >= LOWERED_FLOOR,
        "the lowering covered {} case(s), which is below the floor of {LOWERED_FLOOR}; \
         coverage may rise but never fall\n\n{summary}",
        report.lowered.len()
    );
}

/// Discovers the corpus, and runs every case of it.
fn run_the_corpus() -> Report {
    let mut report = Report::default();
    let cases = discover();
    assert!(!cases.is_empty(), "the corpus is empty");
    report.cases = cases.len();

    // One index per package rather than one per case: `tests/e2e` holds
    // seventy cases and a hundred modules, and what each module reaches is a
    // fact about the package that does not change between two of them.
    let mut indexes: BTreeMap<PathBuf, ModuleIndex> = BTreeMap::new();

    for case in cases {
        let index = indexes
            .entry(case.root.clone())
            .or_insert_with(|| ModuleIndex::of(&case.root));

        // A package that does not check has no program in it to lower or to
        // run. `tests/e2e` keeps such cases on purpose — each pins a
        // check-time diagnostic — so they are counted apart rather than
        // reported as anything the VM did or did not cover.
        let Some(prepared) = Prepared::of(&case, index) else {
            report.unchecked.push(case.name.clone());
            continue;
        };

        let (module, entry) = prepared.entry();
        // The same call `cove run --backend vm` makes, with the same entry:
        // what is lowered is what this entry reaches, so the harness and the
        // CLI mean one thing by "the program this entry is".
        let ir = match cove_ir::lower::lower_entry(&prepared.checked, module, entry) {
            Ok(lowered) => lowered.program,
            Err(why) => {
                report.refused.push((case.name.clone(), why.what.clone()));
                continue;
            }
        };
        if let Err(why) = cove_ir::lower::validate(&ir) {
            report
                .disagreements
                .push(format!("{}: the lowering is not valid: {why}", case.name));
            continue;
        }
        report.lowered.push(case.name.clone());

        let oracle = run_on_ast(&case, &prepared, module, entry);
        let backend = run_on_vm(&case, &prepared, &Arc::new(ir), module, entry);
        if !oracle.trace.cancelled.is_empty() || !backend.trace.cancelled.is_empty() {
            report.races.push(case.name.clone());
        }
        if oracle != backend {
            report
                .disagreements
                .push(disagreement(&case.name, &oracle, &backend));
        }
    }
    report
}

// -------------------------------------------------------------- the corpora

/// One program of the corpus: a `[run.<name>]` table, and the package it
/// belongs to.
struct Case {
    /// `tests/e2e:flow_if`, `examples:hello`, `benches:arith` — the package
    /// the run belongs to and the run's own name, which is unique across the
    /// corpus where the run name alone is not.
    name: String,
    /// The package root the run's entry is resolved against.
    root: PathBuf,
    run: RunConfig,
    /// The process arguments the case is run with, from the `args` file
    /// `tests/e2e` keeps beside a case that takes them.
    args: Vec<String>,
}

/// The repository root, from this crate's own directory.
fn repo_root() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    path.canonicalize()
        .unwrap_or_else(|e| panic!("cannot resolve `{}`: {e}", path.display()))
}

/// Every case of every corpus, in a fixed order.
///
/// The corpora are `tests/e2e/` and `examples/`, and a case is a
/// `[run.<name>]` table of any `cove.toml` inside them — including the ones
/// an own-package `tests/e2e` case brings, which are packages of their own
/// exactly as `tests/e2e.rs` treats them.
///
/// # Why `benches/` is not one of them
///
/// It was, and it was 78 of the 340 seconds this test spent running
/// programs. A benchmark is sized to be measurable in an optimized build —
/// `benches/arith` turns a loop two million times — and this test runs
/// unoptimized, twice per case. Nothing about agreement needs two million
/// turns to establish; the first one settles it and the rest are the same
/// instruction again.
///
/// The coverage did not go anywhere. `cove-bench` runs every benchmark on
/// both backends and each of them asserts its own answer, so a backend that
/// disagreed would fail the assertion on the side that was wrong — and it
/// runs them optimized, in fifteen seconds, on every push. What is given up
/// is the console comparison this harness makes and that one does not, and a
/// benchmark writes almost nothing to the console.
fn discover() -> Vec<Case> {
    let root = repo_root();
    let mut roots = vec![root.join("tests/e2e")];
    roots.extend(nested_packages(&root.join("tests/e2e")));
    roots.push(root.join("examples"));

    let mut cases = Vec::new();
    for package in roots {
        let text = std::fs::read_to_string(package.join("cove.toml"))
            .unwrap_or_else(|e| panic!("cannot read `{}/cove.toml`: {e}", package.display()));
        let config = cove_sema::config::parse(&text)
            .unwrap_or_else(|e| panic!("`{}/cove.toml`: {e}", package.display()));
        for (name, run) in config.runs {
            let mut args = read_args(&package, &name);
            if let Some(smaller) = smaller_workload(&name) {
                args = smaller;
            }
            cases.push(Case {
                name: format!("{}:{name}", relative(&root, &package)),
                root: package.clone(),
                run,
                args,
            });
        }
    }
    cases
}

/// The arguments that make a case's workload a test's size rather than its
/// own.
///
/// One case needs this. `examples:cqSample` is `cq.sample`, which writes a
/// file of records for the `cq` benchmark to read, and its own default is a
/// hundred thousand of them — sixteen megabytes, written twice, unoptimized,
/// by a test that is asking whether two backends agree. It was 258 of the
/// 340 seconds this test spent running programs, which is more than the
/// other eighty-nine cases put together by a factor of sixty.
///
/// A hundred records reach every line of it that a hundred thousand do. The
/// entry already reads the count from its arguments, so this changes nothing
/// about what runs and only how many times the loop around it turns, and
/// `cove run cqSample` still writes what the benchmark expects.
///
/// This is a list rather than a rule because it should stay short enough to
/// read. A case that needs to be here is a case whose size was chosen for
/// something other than this test.
fn smaller_workload(name: &str) -> Option<Vec<String>> {
    match name {
        "cqSample" => Some(vec!["100".to_string(), "bookings-sample.jsonl".to_string()]),
        _ => None,
    }
}

/// Every directory below `root` that holds a `cove.toml` of its own.
///
/// Such a directory is a package rather than a module of `root`'s, which is
/// what `cove_sema::package::load` already decides and what lets a
/// check-time-failure case fail alone.
fn nested_packages(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut names: Vec<PathBuf> = std::fs::read_dir(root)
        .unwrap_or_else(|e| panic!("cannot read `{}`: {e}", root.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    names.sort();
    for path in names {
        if !path.is_dir() || skipped_directory(&path) {
            continue;
        }
        if path.join("cove.toml").is_file() {
            found.push(path);
        } else {
            found.extend(nested_packages(&path));
        }
    }
    found
}

/// Whether the walk should not enter `path`: build output and dotted
/// directories, exactly what the package loader skips.
fn skipped_directory(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name.starts_with('.') || name == "target"
}

/// `root`-relative, with forward slashes, for a case name that reads the
/// same on every platform.
fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// The process arguments a case is run with, one per line of its `args` file.
///
/// This is `tests/e2e.rs`'s own convention, read here so that a case that
/// takes arguments is compared having been given them.
fn read_args(package: &Path, name: &str) -> Vec<String> {
    std::fs::read_to_string(package.join(name).join("args"))
        .map(|text| text.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

// ------------------------------------------------- one case, checked alone

/// One case's program: the modules it is made of, checked.
struct Prepared {
    sources: Arc<SourceMap>,
    checked: Arc<Checked>,
    /// `module.entry`, split, and owned so the borrow does not follow the
    /// `RunConfig` around.
    entry: (String, String),
}

impl Prepared {
    /// Parses and checks the case's entry module together with the modules
    /// `index` says it reaches, or `None` when that program does not check.
    fn of(case: &Case, index: &ModuleIndex) -> Option<Prepared> {
        let (module, entry) = case.run.entry_parts()?;
        let wanted = index.reachable(module)?;

        let mut sources = SourceMap::new();
        let mut modules = BTreeMap::new();
        for name in &wanted {
            let (dir, files) = index.modules.get(name)?;
            let mut units = Vec::new();
            for path in files {
                let text = std::fs::read_to_string(path).ok()?;
                let file = sources.add(path.clone(), &text);
                let ast = cove_syntax::parse_file(&sources, file).ok()?;
                units.push(Unit {
                    file,
                    path: path.clone(),
                    ast,
                });
            }
            modules.insert(
                name.clone(),
                Module {
                    name: name.clone(),
                    dir: dir.clone(),
                    units,
                },
            );
        }

        let package = Package {
            root: case.root.clone(),
            config: Default::default(),
            modules,
        };
        // Resolved *and* type-checked, which is what `cove run` requires
        // before it executes anything. The lowering reads the checker's
        // answers rather than recomputing them, so a program that does not
        // check is not a program either backend has an answer for — and
        // `tests/e2e` keeps a dozen such cases on purpose.
        let checked = cove_sema::Compiler::new().compile(&package).ok()?;
        checked.lookup_fn(module, entry)?;
        Some(Prepared {
            sources: Arc::new(sources),
            checked: Arc::new(checked),
            entry: (module.to_string(), entry.to_string()),
        })
    }

    fn entry(&self) -> (&str, &str) {
        (&self.entry.0, &self.entry.1)
    }
}

/// Every module of one package, and what each of them reaches.
///
/// The index is built by parsing the package once with the `use` declarations
/// the only thing read off it, because a module's dependencies are all that
/// decides which files a case's own program is made of.
struct ModuleIndex {
    /// Each module's directory and its `.cove` files, by dotted name.
    modules: BTreeMap<String, (PathBuf, Vec<PathBuf>)>,
    /// The modules of this package each module's `use` declarations name.
    uses: BTreeMap<String, BTreeSet<String>>,
}

impl ModuleIndex {
    fn of(root: &Path) -> ModuleIndex {
        let mut modules = BTreeMap::new();
        walk(root, root, &mut modules);

        // The names are needed in full before any `use` can be read, since a
        // `use` names a module by its longest matching prefix and the module
        // it names may be discovered later in the walk.
        let mut sources = SourceMap::new();
        let known: BTreeSet<String> = modules.keys().cloned().collect();
        let mut uses: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for (name, (_, files)) in &modules {
            let mut reached = BTreeSet::new();
            for path in files {
                let Ok(text) = std::fs::read_to_string(path) else {
                    continue;
                };
                let file = sources.add(path.clone(), &text);
                let Ok(ast) = cove_syntax::parse_file(&sources, file) else {
                    continue;
                };
                for used in &ast.uses {
                    let segments: Vec<&str> =
                        used.path.iter().map(|part| part.node.as_str()).collect();
                    // A `use` names a value, a type, or a whole module, so
                    // the module it reaches is the longest prefix that is
                    // one. `use console.println` names no module of this
                    // package at all, which is a host and not a dependency.
                    for length in (1..=segments.len()).rev() {
                        let candidate = segments[..length].join(".");
                        if known.contains(&candidate) {
                            reached.insert(candidate);
                            break;
                        }
                    }
                }
            }
            uses.insert(name.clone(), reached);
        }
        ModuleIndex { modules, uses }
    }

    /// `start` and everything it reaches, or `None` when this package has no
    /// such module.
    fn reachable(&self, start: &str) -> Option<BTreeSet<String>> {
        self.modules.get(start)?;
        let mut found = BTreeSet::new();
        let mut pending = vec![start.to_string()];
        while let Some(name) = pending.pop() {
            if !found.insert(name.clone()) {
                continue;
            }
            for next in self.uses.get(&name).into_iter().flatten() {
                pending.push(next.clone());
            }
        }
        Some(found)
    }
}

/// Turns every directory of `.cove` files below `dir` into a module named by
/// its dotted path from `root`.
///
/// A directory holding its own `cove.toml` is a package and not a module of
/// this one, so the walk does not enter it — the rule
/// `cove_sema::package::load` follows, followed here for the same reason.
fn walk(root: &Path, dir: &Path, modules: &mut BTreeMap<String, (PathBuf, Vec<PathBuf>)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();

    let mut cove_files = Vec::new();
    let mut subdirs = Vec::new();
    for path in paths {
        if path.is_dir() {
            if !skipped_directory(&path) {
                subdirs.push(path);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("cove") {
            cove_files.push(path);
        }
    }
    if !cove_files.is_empty() && dir != root {
        modules.insert(
            relative(root, dir).replace('/', "."),
            (dir.to_path_buf(), cove_files),
        );
    }
    for subdir in subdirs {
        if !subdir.join("cove.toml").is_file() {
            walk(root, &subdir, modules);
        }
    }
}

// ------------------------------------------------------------ the two runs

/// What one backend made of one case: everything the run can be observed by.
///
/// `Eq` is not derived beside `PartialEq` because [`Trace`] equality is not
/// transitive: whether two traces may be compared over a given task depends
/// on what both of them did with that task, which is a pairwise question.
#[derive(PartialEq)]
struct Ran {
    /// The value the entry answered, rendered, or the structured error it
    /// failed with. Rendered rather than carried because a [`Value`] is
    /// `Rc`-based and belongs to the run that made it.
    answer: String,
    /// Every line written to the fake console's output stream, in the order
    /// they were written.
    console: Vec<String>,
    /// Every line written to the fake console's diagnostic stream, in the
    /// order they were written.
    ///
    /// Kept apart from `console` rather than merged into it, because a
    /// program that wrote a line to the other stream on one backend would
    /// otherwise agree with itself: two streams compared as one are one
    /// stream again the moment it matters.
    diagnostics: Vec<String>,
    /// How the run ended, classified exactly as `run_entry` classifies it for
    /// the run's terminal trace event.
    outcome: RunOutcome,
    /// The fake filesystem as the run left it. A program told to write a file
    /// says on the console that it did, and the console line is not the file.
    files: BTreeMap<String, String>,
    /// The trace the run wrote, normalized. What a program did at the Host
    /// API boundary and what its tasks did is not visible in anything above:
    /// a run that made a call it should not have made still answers the same
    /// value and prints the same line.
    trace: Trace,
}

/// Runs the case on the interpreter, which is the oracle.
fn run_on_ast(case: &Case, prepared: &Prepared, module: &str, entry: &str) -> Ran {
    let (fakes, hosts) = Fakes::build(case, module, entry);
    // The trace reaches the run through two doors and both must be the same
    // sink: `HostRegistry` records the host calls and `Runtime` records
    // everything else, exactly as `cove run --trace` wires them.
    let sink = Arc::clone(&fakes.sink);
    let runtime = Runtime::new(
        prepared.checked.clone(),
        prepared.sources.clone(),
        Arc::new(hosts),
    )
    .with_trace(sink);
    let answer = Interpreter::new(&runtime).run_entry(module, entry, arguments(case));
    fakes.observed(answer)
}

/// Runs the same case on the VM, over the IR it was lowered to.
fn run_on_vm(
    case: &Case,
    prepared: &Prepared,
    ir: &Arc<cove_ir::Program>,
    module: &str,
    entry: &str,
) -> Ran {
    let (fakes, hosts) = Fakes::build(case, module, entry);
    let sink = Arc::clone(&fakes.sink);
    let hosts = Arc::new(hosts);
    let runtime = Runtime::new(
        prepared.checked.clone(),
        prepared.sources.clone(),
        hosts.clone(),
    )
    .with_trace(sink);
    let answer = Vm::new(&runtime, &hosts, ir).run_entry(module, entry, arguments(case));
    fakes.observed(answer)
}

/// The process arguments the entry is handed, as both backends take them.
fn arguments(case: &Case) -> Vec<Rc<str>> {
    case.args.iter().map(|arg| arg.as_str().into()).collect()
}

/// What a run can be observed through, kept where the test can read it back
/// once the run is over.
struct Fakes {
    console: Buffer,
    diagnostics: Buffer,
    files: Tree,
    /// The trace, as the JSONL a `--trace` file would hold.
    trace: Buffer,
    /// The sink writing into `trace`, which the run's `Runtime` needs as well
    /// as its `HostRegistry`.
    sink: Arc<dyn TraceSink>,
}

impl Fakes {
    /// The hosts one run is given, and the handles onto the ones that record
    /// what it did: both of the console's streams and the filesystem.
    ///
    /// Every host is registered whether or not this case reaches it, exactly
    /// as `cove run` registers them: the grants are what decide, so a
    /// capability a program reaches for without holding is refused with the
    /// reason rather than with a missing module.
    fn build(case: &Case, module: &str, entry: &str) -> (Fakes, HostRegistry) {
        let console = Buffer::default();
        let diagnostics = Buffer::default();
        let files = Files::in_memory(seeded_files(&case.root));
        let tree = files.tree();

        let mut hosts = HostRegistry::new(Grants::new(case.run.allow.clone()));
        // Two buffers, because the host has two streams: one buffer would
        // make a line that moved from the one to the other invisible here,
        // which is the only kind of disagreement the second stream adds.
        hosts.register(Box::new(Console::new(console.clone(), diagnostics.clone())));
        hosts.register(Box::new(Env::new(BTreeMap::new())));
        hosts.register(Box::new(Documents::in_memory(seeded_documents(&case.root))));
        hosts.register(Box::new(Clock::virtual_clock(VirtualTime::new())));
        hosts.register(Box::new(Database::recorded(BTreeMap::new())));
        hosts.register(Box::new(Http::recorded(BTreeMap::new(), Vec::new())));
        hosts.register(Box::new(Process::recorded(
            case.args.clone(),
            BTreeMap::new(),
            ProcessLog::new(),
        )));
        hosts.register(Box::new(files));
        // Full capture, because a redacted trace records a value's type and
        // not the value, and the arguments a program passes a host are the
        // half of a host call this test most wants compared. Nothing here
        // reaches a real host, so there is no secret to redact.
        let trace = Buffer::default();
        let sink: Arc<dyn TraceSink> = Arc::new(JsonlSink::new(
            trace.clone(),
            TraceHeader {
                values: ValueCapture::Full,
                entry: format!("{module}.{entry}"),
                args: case.args.clone(),
            },
        ));
        hosts.set_trace(Arc::clone(&sink));
        hosts.set_budget(Budget::with_cancellation(
            limits(&case.run),
            Cancellation::new(),
        ));

        (
            Fakes {
                console,
                diagnostics,
                files: tree,
                trace,
                sink,
            },
            hosts,
        )
    }

    /// What the run left behind, beside what it answered.
    fn observed(self, answer: Result<Value, RuntimeError>) -> Ran {
        let outcome = match &answer {
            Ok(value) if value.is_err() => RunOutcome::Error,
            Ok(_) => RunOutcome::Success,
            Err(error) => error.outcome,
        };
        Ran {
            answer: describe(&answer),
            console: self.console.lines(),
            diagnostics: self.diagnostics.lines(),
            outcome,
            files: self.files.files(),
            trace: Trace::of(&self.trace.lines()),
        }
    }
}

/// The budgets a case runs under.
///
/// Everything `[run.<name>]` sets except fuel and the deadline. Fuel is
/// backend-specific by ADR 0019 — an instruction is not an AST node — and a
/// deadline is wall-clock, so either one would make the two backends stop at
/// different points by construction rather than by fault. What is left counts
/// things both backends count the same way.
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
/// A failure is rendered by its structure rather than by its message alone:
/// what it said, how it classified itself, which capability the boundary
/// refused, the rule it cited, and where in the source it points. #111 asks
/// that a runtime error keep useful Cove spans on both backends, and the
/// strongest form of that claim the corpus supports today is that the two
/// backends point at the same bytes.
fn describe(answer: &Result<Value, RuntimeError>) -> String {
    match answer {
        Ok(value) => format!("value {value:?}"),
        Err(error) => format!(
            "failed {:?}: {}\n    rule: {:?}\n    help: {:?}\n    denied: {:?}\n    at: {:?}",
            error.outcome,
            error.message,
            error.rule,
            error.help,
            error.denied_capability,
            error.span,
        ),
    }
}

/// The message a disagreement is reported with: both sides, in full.
///
/// ADR 0012 presumes the oracle right, so the interpreter's answer is named
/// first and named as the oracle. Which side is wrong is still a judgement,
/// and the message is what somebody makes it from.
fn disagreement(name: &str, oracle: &Ran, backend: &Ran) -> String {
    let mut out = format!("{name}: the two backends did not agree\n");
    let mut side = |which: &str, ran: &Ran| {
        let _ = write!(
            out,
            "  {which}:\n    outcome: {:?}\n    {}\n",
            ran.outcome, ran.answer
        );
        let _ = writeln!(out, "    console: {:?}", ran.console);
        if !ran.diagnostics.is_empty() {
            let _ = writeln!(out, "    diagnostics: {:?}", ran.diagnostics);
        }
        if !ran.files.is_empty() {
            let _ = writeln!(out, "    files: {:?}", ran.files);
        }
        for (task, events) in &ran.trace.tasks {
            let _ = writeln!(out, "    trace of task {task}:");
            for line in events {
                let _ = writeln!(out, "      {line}");
            }
        }
    };
    side("ast (the oracle)", oracle);
    side("vm", backend);
    out
}

// -------------------------------------------------------------- the trace

/// One run's trace, normalized so that two backends' recordings of one
/// program can be compared.
///
/// Held per task rather than as the one interleaved file the run wrote. Every
/// event is produced by whichever task made it and written by the one sink
/// the run shares, so the order two tasks' events reach that sink is the
/// order two threads happened to get there — ADR 0008 gives every spawned
/// task a thread of its own, and nothing in the language fixes which of them
/// writes first. Grouping by task drops exactly that and keeps everything
/// else: within one task the order is the program's, and it is compared.
///
/// This is not a normalization that could be argued either way. Running the
/// interpreter against itself thirty times over, `tests/e2e:gc_tasks`,
/// `tests/e2e:tasks_shared` and `examples:tasks` each wrote a differently
/// interleaved file every time, and every one of them writes the same file
/// once it is read per task. A comparison that failed on the interleaving
/// would be reporting the scheduler, and it would fail against the oracle as
/// readily as against the VM.
struct Trace {
    /// Each task's own events, in the order that task produced them, keyed by
    /// the task's id. The entry is `cove_runtime::runtime::ENTRY_TASK`, which
    /// is the convention every event that names a task already uses.
    tasks: BTreeMap<u64, Vec<String>>,
    /// The tasks this run cancelled. See [`Trace::eq`] for what that costs.
    cancelled: BTreeSet<u64>,
}

impl Trace {
    /// Reads the JSONL a run wrote, and normalizes it.
    ///
    /// # Every `Duration` is blanked
    ///
    /// `cpu`, `wait` and `pause` are wall time. Two runs of one program on
    /// one backend do not agree on any of them either, so comparing them
    /// would report the machine. The keys are kept and only the figures go,
    /// so a `_ns` field that stopped being written is still a difference.
    ///
    /// # `heap_collected` is dropped whole
    ///
    /// The event says when a collection happened and what it found. Both
    /// halves are the collector's schedule. A collection runs at a safepoint
    /// where enough has been allocated since the last one, and the two
    /// backends put safepoints in different places — the interpreter at every
    /// loop turn, the VM at the first back edge with `BACK_EDGE_FUEL`
    /// gathered — so the VM crosses the threshold and keeps allocating until
    /// the next block head. The corpus shows both halves moving:
    /// `tests/e2e:gc_capture` collects after 64 allocations on the
    /// interpreter and after 66 on the VM, and `examples:cqSample` runs its
    /// collection one `files.Writer.writeLine` earlier on the VM than on the
    /// interpreter, so even a `heap_collected` with every figure blanked
    /// stands in a different place in the sequence.
    ///
    /// What the event was for is not lost, because `heap_summary` says the
    /// same things about the whole run and is compared. Dropping the event
    /// and keeping the summary is the only division that is about the
    /// program rather than the schedule: *what* a run allocated and *how
    /// often* it collected are the program's, and *when* each collection fell
    /// is the backend's.
    ///
    /// # `heap_summary` keeps what was allocated and drops what was live
    ///
    /// `allocated`, `allocated_bytes` and `collections` are compared exactly,
    /// and they agree on every case in this corpus. That is worth stating
    /// plainly because `docs/VM_ARCHITECTURE.md` predicted the third of them
    /// would not: a run that allocates identically "can collect five times
    /// here and six times there". Over the ninety-three cases that lower it
    /// never does. The reason the prediction is still right in general and
    /// wrong here is that the threshold is a count of allocations and the two
    /// backends allocate the same objects, so the VM's overshoot moves the
    /// boundary between two collections without changing how many boundaries
    /// there are; it would take an overshoot large enough to swallow a whole
    /// threshold to lose one, and nothing in this corpus allocates fast
    /// enough between two safepoints for that.
    ///
    /// `live_bytes` and `peak_bytes` are dropped. They measure the live set,
    /// and the live set is decided by the root set, which is a frame's slots
    /// on the VM and an environment chain on the interpreter. A `var`
    /// declared in a loop body has left the chain by the time the
    /// interpreter's safepoint is reached and is still the VM frame's slot
    /// until something writes that slot again, because a frame's window is
    /// sized once per function rather than opened and closed per block. Both
    /// backends report truthfully what was reachable from their own roots;
    /// the two are not the same question. `tests/e2e:gc_churn` peaks at 120
    /// bytes on the interpreter and 216 on the VM for this reason.
    ///
    /// `live_bytes` is the near miss, and it is recorded rather than
    /// rounded off: it agrees on ninety-two of the ninety-three cases that
    /// lower. The one that differs is `tests/e2e:fail_freeze_aliased`, which
    /// ends by raising — and a run that raised abandoned its frames where
    /// they stood, so the vector its last sweep still finds in a VM frame
    /// slot is one the interpreter's environment had already left. It is the
    /// same root-set difference as `peak_bytes`, reached by a different
    /// route, so it is excluded for the same stated reason rather than kept
    /// with an exception carved out of it.
    fn of(lines: &[String]) -> Trace {
        let mut tasks: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        let mut cancelled = BTreeSet::new();
        for line in lines {
            if event(line) == "heap_collected" {
                continue;
            }
            let mut normalized = blank_durations(line);
            if event(line) == "heap_summary" {
                normalized = without(&normalized, "live_bytes");
                normalized = without(&normalized, "peak_bytes");
            }
            if event(line) == "task_cancelled" {
                cancelled.insert(number(line, "id").unwrap_or(ENTRY_TASK));
            }
            tasks.entry(whose(line)).or_default().push(normalized);
        }
        Trace { tasks, cancelled }
    }
}

/// Two traces of one program, compared task by task.
///
/// # A task either run cancelled is compared only by its `task_spawned`
///
/// Cancellation is asynchronous: a scope that ends asks its children to stop
/// and lands wherever each thread happened to be, so how far a cancelled task
/// got is the scheduler's answer and not the program's. This is measured
/// rather than assumed. `tests/e2e:fail_max_tasks` records
/// `task_completed` for task 1 in three of twenty runs *on the interpreter
/// alone* and `task_cancelled` in the other seventeen; `examples:callbacks`
/// flips the same way on the VM alone, and on the run where it is cancelled
/// the `clock.every` call that task would have made is missing from the trace
/// with it. Holding two backends to a fact one backend does not hold itself
/// to would be a test that fails at random.
///
/// So what is compared for such a task is that it was spawned, with the same
/// id, by the same parent, into the same scope. What is given up with the
/// rest is real and is worth naming: a VM that always cancelled where the
/// interpreter always completed would not be caught here. What catches it
/// instead is that the entry's own trace is compared exactly, and a task's
/// work reaches the entry — through what it printed, what it left in the
/// filesystem, and what the entry answered, all of which this harness
/// compares whether or not a trace was written. [`Report::races`] names every
/// case this rule applied to, so the loss is printed rather than silent.
impl PartialEq for Trace {
    fn eq(&self, other: &Trace) -> bool {
        let ids: BTreeSet<u64> = self
            .tasks
            .keys()
            .chain(other.tasks.keys())
            .copied()
            .collect();
        let raced: BTreeSet<u64> = self.cancelled.union(&other.cancelled).copied().collect();
        ids.into_iter().all(|id| {
            let mine = self.tasks.get(&id).map(Vec::as_slice).unwrap_or_default();
            let theirs = other.tasks.get(&id).map(Vec::as_slice).unwrap_or_default();
            if raced.contains(&id) {
                spawn_of(mine) == spawn_of(theirs)
            } else {
                mine == theirs
            }
        })
    }
}

/// The `task_spawned` line of one task's events, which is all that is
/// compared for a task a run cancelled.
fn spawn_of(events: &[String]) -> Option<&String> {
    events.iter().find(|line| event(line) == "task_spawned")
}

/// Which task produced an event.
///
/// A `task` field answers directly. A task's own lifecycle events are its
/// own, including the `task_spawned` the parent recorded and the
/// `task_cancelled` the joining scope did: what they say is about the task
/// they name, and keeping them with it is what lets one task's whole life be
/// compared as one sequence. Everything else — the header, the entry's two
/// events, the summary, the ending — belongs to the entry, which is the
/// convention the trace format already uses for the entry's own host calls.
fn whose(line: &str) -> u64 {
    if let Some(task) = number(line, "task") {
        return task;
    }
    match event(line) {
        "task_spawned" | "task_completed" | "task_cancelled" => {
            number(line, "id").unwrap_or(ENTRY_TASK)
        }
        _ => ENTRY_TASK,
    }
}

/// The `event` name of one trace line.
fn event(line: &str) -> &str {
    let Some(at) = line.find("\"event\":\"") else {
        return "";
    };
    let rest = &line[at + "\"event\":\"".len()..];
    &rest[..rest.find('"').unwrap_or(rest.len())]
}

/// The integer under a top-level `key` of one trace line.
fn number(line: &str, key: &str) -> Option<u64> {
    let needle = format!("\"{key}\":");
    let at = line.find(&needle)? + needle.len();
    let rest = &line[at..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

/// The line without `key` and the integer under it.
fn without(line: &str, key: &str) -> String {
    let needle = format!("\"{key}\":");
    let Some(at) = line.find(&needle) else {
        return line.to_string();
    };
    let rest = &line[at + needle.len()..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let (mut head, mut tail) = (&line[..at], &rest[end..]);
    // One of the two commas around the field goes with it, whichever side it
    // is on, so that what is left is still a JSON object.
    if let Some(rest) = tail.strip_prefix(',') {
        tail = rest;
    } else {
        head = head.strip_suffix(',').unwrap_or(head);
    }
    format!("{head}{tail}")
}

/// The line with the figure under every `_ns` key replaced by a placeholder.
fn blank_durations(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
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

// -------------------------------------------------------------- the fakes

/// One of a `console`'s streams, which a run writes to and this test reads
/// back.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Buffer {
    fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.0.lock().expect("no run panics while printing"))
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("no run panics while printing")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The package's own `files/` directory, read into the in-memory filesystem
/// the run is given.
///
/// Reads answer what the case's fixtures actually hold, so a case that reads
/// a file is compared having read it; writes land in memory, so a run cannot
/// change the repository it was read out of.
fn seeded_files(root: &Path) -> BTreeMap<String, String> {
    let mut seeded = BTreeMap::new();
    read_tree(&root.join("files"), String::new(), &mut seeded);
    seeded
}

/// The package's own `documents/`, read the same way and for the same reason.
fn seeded_documents(root: &Path) -> BTreeMap<String, String> {
    let mut seeded = BTreeMap::new();
    read_tree(&root.join("documents"), String::new(), &mut seeded);
    seeded
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

// ------------------------------------------------------------- the summary

/// What the whole corpus came to.
#[derive(Default)]
struct Report {
    cases: usize,
    lowered: Vec<String>,
    /// Each refused case, and the construct the lowering named.
    refused: Vec<(String, String)>,
    /// Cases whose package does not check, which have no program to run.
    unchecked: Vec<String>,
    /// Cases in which either run cancelled a task, so that task's own trace
    /// was compared only by the `task_spawned` that made it. Printed rather
    /// than kept quiet: this is the one place the trace comparison gives
    /// something up, and a reader of the summary should be able to see how
    /// much. [`Trace::eq`] is the argument for why.
    races: Vec<String>,
    disagreements: Vec<String>,
}

impl Report {
    /// The coverage summary: how much of the corpus the VM covers today, and
    /// what stands between it and the rest.
    ///
    /// The refusals are grouped by construct and ordered by how many cases
    /// each one blocks, because that list is the roadmap for what to lower
    /// next and the order is the argument for which to lower first.
    fn summary(&self) -> String {
        let mut out = format!(
            "\ndifferential coverage over {} corpus case(s):\n  \
             {:>3} lowered, and agree on both backends\n  \
             {:>3} refused by the lowering\n  \
             {:>3} do not check, so there is nothing to run\n",
            self.cases,
            self.lowered.len(),
            self.refused.len(),
            self.unchecked.len(),
        );

        let mut by_construct: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (case, what) in &self.refused {
            by_construct
                .entry(what.as_str())
                .or_default()
                .push(case.as_str());
        }
        let mut ranked: Vec<(&str, Vec<&str>)> = by_construct.into_iter().collect();
        ranked.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));

        if !ranked.is_empty() {
            out.push_str("\nwhat the lowering refuses, most common first:\n");
            for (what, cases) in ranked {
                let _ = writeln!(out, "  {:>3}  {what}", cases.len());
                let _ = writeln!(out, "       first at {}", cases[0]);
            }
        }
        if !self.races.is_empty() {
            out.push_str(
                "\nwhere a cancelled task's own trace is a race, so only its spawn is compared:\n",
            );
            for case in &self.races {
                let _ = writeln!(out, "       {case}");
            }
        }
        if !self.lowered.is_empty() {
            out.push_str("\nwhat the VM runs today:\n");
            for case in &self.lowered {
                let _ = writeln!(out, "       {case}");
            }
        }
        out
    }
}
