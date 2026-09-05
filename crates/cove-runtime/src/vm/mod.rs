//! The execution backend [ADR 0034](../../../../docs/adr/0034-one-physical-word-stack.md)
//! decided on, and since the cutover, the only one: this is the production
//! path an embedder runs a Cove program on.
//!
//! [`docs/LINEAR_VM.md`](../../../../docs/LINEAR_VM.md) is the design. It was
//! written as a clean-room replacement rather than a renovation: nothing here
//! was derived from the backend it replaced, and at the cutover that backend
//! was deleted. `lvm` and `cove-lir` were transitional spellings, worn while
//! the predecessor still held these names; it is gone, and this module and
//! `cove-ir` have taken them.
//!
//! Two things leave this module: [`Vm`], the type an embedder holds, and
//! [`exec::SAFEPOINT_STRIDE`], a number a test asserts a bound against.
//! Nothing else does, because what a caller can name is the whole of what
//! this boundary decides. A word, a layout, a [`mem::Memory`] and a
//! [`exec::Machine`] are the representation, and a representation that leaves
//! the crate is one that cannot be changed without changing somebody else's
//! code.
//!
//! # What a run owns, and what each of its tasks owns
//!
//! Issue #240's Q1 answers this and [`docs/LINEAR_VM.md`](../../../../docs/LINEAR_VM.md)'s
//! "Ownership" section writes it out. Five things, and which of them is a
//! value store is the whole of what ADR 0034 cares about:
//!
//! | | belongs to | where it is | a value store? |
//! |---|---|---|---|
//! | the object heap | the **run** | [`mem::Space`], one per run, behind an `Arc` | **yes**, and the only one |
//! | a stack segment | a **task** | `[k * SEGMENT_WORDS, (k+1) * SEGMENT_WORDS)` of the same address space | yes, and it is part of the same one |
//! | a `Shared` cell | the **run**'s heap | an ordinary object; its lock is one of its words ([`cell`]) | no — it *is* an object in the heap |
//! | a host resource | the **host** | a table of names, shared by every task | no — see ADR 0031 |
//! | a `Task`, a `TaskScope` | the **scheduler** | a table of control state, one per task | no |
//!
//! The last row is the one that has to be argued rather than asserted, because
//! a `Task` is a value a Cove program writes down. Its word is **one past an
//! index into a scheduler table**, the way a `Repr::Host` word is, and the
//! entry it names holds a task id, a scope name and position, a
//! `Cancellation`, and the *address* of the heap object holding its answer.
//! Every one of those but the last is scheduler bookkeeping that no Cove value
//! could be hidden in. The last is an address, which makes the table a **root
//! provider** and not a second store: the answer's words are in the run's
//! heap, in an object the spawning task allocated before the thread existed,
//! and the table names one the way `Machine::interned` names a string
//! literal. Nothing that wanted to dodge a heap representation could be put
//! there, which is the test ADR 0034 actually applies.
//!
//! # The scheduler's table is a *task's*, and the host's is the *run's*
//!
//! The two look alike — a word one past an index into a table of names — and
//! they are owned differently, for a reason each states about itself rather
//! than by analogy.
//!
//! A `Task` and a `TaskScope` may not cross a task boundary; the task-safety
//! rule says so, and `cove_sema`'s `task_safe_offender` is where it is
//! enforced. So a word formed in one task is only ever read in that task, the
//! tables are disjoint by construction, and there is nothing to share. That is
//! the same arithmetic that keeps two stack segments apart, and it is why
//! [`exec::Machine`] holds its own.
//!
//! A **host resource** does cross: ADR 0013 gives the host the record of what
//! is open, a resource declares its own task-safety in its schema, and a
//! task-safe one is copied into a spawned closure like any other value — as
//! its word. A table of one task's own would make that word an index into a
//! list the receiving task does not have. So the resource table is the run's,
//! behind a lock, and one resource is one word for the length of the run,
//! which is what ADR 0013's *"two handles are equal when they name the same
//! resource"* costs once there is more than one thread.
//!
//! The dead-code allowance stays, and what it now covers is narrower than
//! what it covered before: not "nothing reaches this module" but "several
//! items below it are reached only from their own `#[cfg(test)]` code" —
//! `Machine::new`, `cell`'s whole surface until the lowering reaches
//! `Shared`, the collection counters a test asserts over, and
//! [`Vm::collected`] and [`Vm::host_wait`], the two figures this type can
//! report that nothing outside asks for yet. It is one line in one place
//! rather than an attribute per item, so that removing it is a single edit
//! whose failure lists exactly what is still unused.
#![allow(dead_code)]

use std::rc::Rc;
use std::time::Duration;

use cove_diag::Span;
use cove_ir::{Function, FunctionId, Program};

use crate::budget::{Budget, Limits, Meter};
use crate::error::RuntimeError;
use crate::host::HostRegistry;
use crate::runtime::Runtime;
use crate::trace::{RunOutcome, Timing, TraceEvent};
use crate::vm::debug::Debugger;
use crate::vm::exec::Machine;
use crate::vm::mem::Collected;
// The public `Value` reaches this file for the one reason ADR 0034 allows it
// to reach any of them: this is a boundary. An entry's arguments and its
// answer are what a host hands in and reads back, and they are `Value`s on
// both sides of that line. Nothing here stores one — every value named below
// is on its way into [`boundary::from_value`] or out of
// [`boundary::to_value`].
use crate::value::Value;

pub(crate) mod boundary;
pub(crate) mod builtins;
pub(crate) mod cell;
pub(crate) mod debug;
#[cfg(test)]
mod differential;
#[cfg(test)]
mod erasure;
pub(crate) mod exec;
pub(crate) mod mem;
pub(crate) mod render;

/// The words a run's heap region may grow to unless an embedder says
/// otherwise.
///
/// Four mebiwords, thirty-two mebibytes. Reserved is not committed: the
/// backing store grows on demand, so a program that allocates nothing pays
/// nothing, and what the number buys is a run that fails with "this run has no
/// memory left" rather than taking the machine down with it. Like
/// [`mem::STACK_WORDS`] it is an implementation choice and not a language
/// fact.
const DEFAULT_HEAP_WORDS: usize = 1 << 22;

/// One run of a lowered program.
///
/// This is the type above the machine: it holds the program, the memory the
/// run executes over, and the two things that make a run a run rather than a
/// dispatch loop — the boundary a `Value` crosses, and the accounting a
/// safepoint charges. The dispatch loop underneath knows nothing about any
/// of them.
///
/// The two ways in are the two the language has, and they are the same two
/// [`crate::interp::Interpreter`] offers. [`Vm::run_entry`] is how a
/// *command* speaks to a program: the arguments are process arguments, which
/// are strings. [`Vm::invoke`] is how an *application* does: the arguments
/// are values the host built, held to the types the checker resolved before
/// the first instruction runs. Everything below the two is one path.
pub struct Vm<'a> {
    runtime: &'a Runtime,
    hosts: &'a HostRegistry,
    program: &'a Program,
    machine: Machine<'a>,
    /// The run's accounting, in the handle a safepoint charges through.
    ///
    /// Taken once, where the run begins, for the reason [`Meter`] gives. A
    /// registry with no budget installed answers `None`, which has always
    /// meant no limit; a meter over default [`Limits`] is that, written down.
    budget: Meter,
}

impl<'a> Vm<'a> {
    /// A run of `program`, over `runtime`'s checked program and `hosts`.
    ///
    /// The heap budget is this module's `DEFAULT_HEAP_WORDS` and is not a
    /// parameter yet:
    /// no caller has had a reason to name one, and a knob nobody turns is a
    /// knob whose meaning nobody has had to decide.
    pub fn new(runtime: &'a Runtime, hosts: &'a HostRegistry, program: &'a Program) -> Vm<'a> {
        Vm {
            runtime,
            hosts,
            program,
            machine: Machine::for_run(program, DEFAULT_HEAP_WORDS, Some(hosts), Some(runtime)),
            budget: meter_of(hosts),
        }
    }

    /// The same run, watched by `debugger`.
    ///
    /// A second constructor rather than a parameter on [`Vm::new`], for the
    /// reason the heap budget is not one either: no existing caller has a
    /// debugger to name, and a parameter every caller passes `None` to is a
    /// question every caller is asked and none of them answers.
    ///
    /// What it costs the run is stated where it is paid, in
    /// the debugger's own module: the machine asks before **every** instruction
    /// for as long as the debugger is installed, so a debugged run is slower
    /// by whatever the debugger does per instruction. A run built with
    /// [`Vm::new`] is unchanged — the loop's comparison is the same one it
    /// was, against the next safepoint.
    pub fn debugged(
        runtime: &'a Runtime,
        hosts: &'a HostRegistry,
        program: &'a Program,
        debugger: &'a dyn Debugger,
    ) -> Vm<'a> {
        let mut vm = Vm::new(runtime, hosts, program);
        vm.machine.watch(Some(debugger));
        vm
    }

    /// The same run, executing the program's **encoded** instructions.
    ///
    /// [Issue #245](https://github.com/myuon/cove/issues/245)'s Phase 4, and
    /// [ADR 0041](../../../../docs/adr/0041-a-slot-number-fits-in-sixteen-bits.md)
    /// is the format. The program is encoded, verified once, and checked
    /// against what the encoded dispatch loop implements *here*, before the run
    /// exists — so this answers `Err` for a program the encoded path cannot
    /// execute, naming the opcode and pointing at its source, and no
    /// half-run happens.
    ///
    /// Every opcode is implemented, and the whole differential corpus runs
    /// through here and agrees with the tree-walking oracle
    /// (`crates/cove-cli/tests/differential.rs`), so the refusal is a
    /// scaffold that nothing reaches rather than a limit. It is kept because
    /// "no silent fallback to enum execution" is a property that has to be
    /// checkable, and a path that could quietly hand a program back is one
    /// where it is not.
    ///
    /// A third constructor rather than a parameter on [`Vm::new`], which is
    /// the same shape [`Vm::debugged`] has and for the same reason: a
    /// question every existing caller would answer the same way is a
    /// question not worth asking them. **Nothing reaches this by default.**
    /// `cove run --encoded` is the one way in, and it is a development flag
    /// for the phase rather than a way to run a program.
    ///
    /// What it costs a run built with [`Vm::new`] is nothing, and that is
    /// measured rather than asserted: the choice is made once, in
    /// `Machine::drive`, and neither dispatch loop contains a test for it.
    pub fn encoded(
        runtime: &'a Runtime,
        hosts: &'a HostRegistry,
        program: &'a Program,
    ) -> Result<Vm<'a>, RuntimeError> {
        let code = exec::encoded::prepare(program)?;
        let mut vm = Vm::new(runtime, hosts, program);
        vm.machine.execute_encoded(code);
        Ok(vm)
    }

    /// Runs `module.name` with the process arguments `args`.
    ///
    /// An entry takes either no parameters or one `Array<String>`, and that
    /// rule is the language's rather than a backend's — the oracle refuses
    /// the third shape in these words, at this span.
    pub fn run_entry(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        let outcome = self.enter(module, name, args);
        self.ended(outcome)
    }

    /// Calls `module.name` with values the host built.
    ///
    /// The arguments are held to what the checker resolved about the
    /// declaration — the shape it has to be callable at all, the count, and
    /// each value's type followed as deeply as the type goes — before
    /// anything runs. That check is the crate's own `invoke`, shared with the
    /// oracle so that a host that gets it wrong reads one answer and not one
    /// per backend.
    ///
    /// One refusal belongs to this backend rather than to the language, and
    /// it is about the *lowering* rather than about the program.
    /// [`cove_ir::lower_entry`] lowers what one entry can reach and nothing
    /// else, so a run built for one entry cannot invoke a function no path
    /// from that entry leads to. Saying the package does not declare it would
    /// be false and would send an embedder to the wrong file, so this says
    /// which of the two is missing and what to lower instead.
    pub fn invoke(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let outcome = self.invoke_checked(module, name, args);
        self.ended(outcome)
    }

    /// [`Vm::run_entry`], bounded by `budget` and by nothing else.
    ///
    /// The command-shaped way in, bounded the way [`Vm::invoke_within`]
    /// bounds the application-shaped one. Issue #152 is why both exist: an
    /// application that runs somebody else's Cove wants the *request*
    /// bounded, not the session, and a session is built once and invoked
    /// many times.
    pub fn run_entry_within(
        &mut self,
        budget: Budget,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        self.hosts.begin_run(budget);
        self.bind_budget();
        let outcome = self.enter(module, name, args);
        self.ended(outcome)
    }

    /// [`Vm::invoke`], bounded by `budget` and by nothing else.
    ///
    /// The check runs before the budget is installed, so a call refused for a
    /// wrong argument spends none of the budget it was handed and leaves
    /// whatever bounded this backend where it was.
    pub fn invoke_within(
        &mut self,
        budget: Budget,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let outcome = self.checked_within(budget, module, name, args);
        self.ended(outcome)
    }

    /// The check, the budget, and then the call.
    fn checked_within(
        &mut self,
        budget: Budget,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        crate::invoke::check(self.runtime.program(), module, name, &args)?;
        self.hosts.begin_run(budget);
        self.bind_budget();
        let id = self.lowered(module, name)?;
        self.enter_with(module, name, id, args)
    }

    /// Re-reads the meter after the registry was given a new budget.
    ///
    /// The handle is taken once where a run begins, so installing a budget
    /// for one invocation has to be followed by taking the handle again;
    /// otherwise the safepoints would go on charging the budget the session
    /// was built over.
    fn bind_budget(&mut self) {
        self.budget = meter_of(self.hosts);
    }

    /// How many instructions this run has executed.
    pub fn instructions(&self) -> u64 {
        self.machine.instructions()
    }

    /// What every collection of this run has done.
    pub(crate) fn collected(&self) -> Collected {
        self.machine.collected()
    }

    /// Words the heap region occupies, free blocks included.
    pub fn heap_words(&self) -> u64 {
        self.machine.heap_words()
    }

    /// Words handed out over the whole run, reuse counted each time.
    pub fn allocated_words(&self) -> u64 {
        self.machine.allocated_words()
    }

    /// How long this run has spent inside host calls.
    pub(crate) fn host_wait(&self) -> Duration {
        self.machine.host_wait()
    }

    /// Where the most recent failed assertion was written, together with the
    /// message it produced, or `None` when no assertion has failed.
    ///
    /// The same answer [`crate::interp::Interpreter::assertion_failure`]
    /// gives, and it is here for the same caller: a test runner points at
    /// the assertion the way every other error points at source. An
    /// assertion that failed and was then handled inside the program is
    /// still recorded, which is why the message is part of the answer — a
    /// caller reports at this span only when the failure it is holding is
    /// this one.
    pub fn assertion_failure(&self) -> Option<(Span, &str)> {
        self.machine.assertion_failure()
    }

    /// The process arguments as the one value an entry may take them as.
    fn enter(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        let id = self.lookup(module, name)?;
        let function = self.program.function(id);
        let arguments = match function.arity() {
            0 => Vec::new(),
            1 => vec![Value::array(args.into_iter().map(Value::string))],
            other => {
                return Err(RuntimeError::new(format!(
                    "entry `{module}.{name}` declares {other} parameters"
                ))
                .at(function.span)
                .with_rule(
                    "An entry function takes either no parameters or one `Array<String>` of process arguments.",
                )
                .with_help(format!(
                    "write `fn {name}()` or `fn {name}(args: Array<String>)`"
                )));
            }
        };
        self.enter_with(module, name, id, arguments)
    }

    /// The check, and then the call.
    fn invoke_checked(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        crate::invoke::check(self.runtime.program(), module, name, &args)?;
        let id = self.lowered(module, name)?;
        self.enter_with(module, name, id, args)
    }

    /// The call itself, from the arguments in to the answer out.
    ///
    /// The one seam. [`Vm::run_entry`] reaches it having turned the process
    /// arguments into the array an entry declares, and [`Vm::invoke`]
    /// reaches it having held a host's own values to what the checker
    /// resolved; nothing below this line knows which of the two happened.
    fn enter_with(
        &mut self,
        module: &str,
        name: &str,
        id: FunctionId,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let function = self.program.function(id);
        let span = function.span;
        let returns = function.returns;

        self.runtime.trace(TraceEvent::EntryEnter {
            module: module.to_string(),
            function: name.to_string(),
        });
        // Started here and not in `run_entry`, because what this measures is
        // the entry: the argument conversion is the entry's own boundary
        // crossing and the run is what follows it.
        let timing = Timing::start();
        let waited = self.machine.host_wait();

        let outcome = self
            .words_of(function, &args)
            .map_err(|e| e.at(span))
            .and_then(|words| self.machine.run(id, &words, &self.budget))
            .and_then(|answer| {
                boundary::to_value(&self.machine, returns, &answer).map_err(|e| e.at(span))
            });

        // Both events on every path, the way the oracle writes them: an entry
        // that failed still entered and still left, and a run that failed
        // still allocated. A trace that recorded the exit only for a run that
        // answered would be a trace whose shape depended on the answer.
        self.runtime.trace(TraceEvent::EntryExit {
            module: module.to_string(),
            function: name.to_string(),
            cpu: timing
                .elapsed()
                .saturating_sub(self.machine.host_wait().saturating_sub(waited)),
            wait: self.machine.host_wait().saturating_sub(waited),
        });
        self.summarize_heap();
        outcome
    }

    /// What this run's memory did, recorded once as the run ends.
    ///
    /// The word half of the event and none of the object half. Issue #240
    /// decided that `heap_summary` does not choose between the two — an
    /// inline struct is words here and no object at all on the oracle, so
    /// neither family's figures can be derived from the other's — and the
    /// rule that follows is that a machine leaves `None` in what it does not
    /// count rather than a zero that reads as a measurement.
    ///
    /// `live_words` is one of those. It is what the last collection found
    /// alive, so a run that never collected has nothing that measured it, and
    /// the figure is absent rather than nought. `capacity_words` is not: the
    /// heap region occupies what it occupies whether anything has been swept
    /// or not.
    ///
    /// There is no pause here, and that is the same rule again. This
    /// collector does not time itself yet, and a zero would say it stopped
    /// the world for no time at all.
    fn summarize_heap(&self) {
        let collected = self.machine.collected();
        self.runtime.trace(TraceEvent::HeapSummary {
            collections: collected.collections,
            object_count: None,
            allocated_bytes: None,
            live_bytes: None,
            peak_bytes: None,
            pause: None,
            allocated_words: Some(self.machine.allocated_words()),
            capacity_words: Some(self.machine.heap_words()),
            live_words: (collected.collections > 0).then_some(collected.live_words),
        });
    }

    /// The arguments in word form.
    ///
    /// Each conversion allocates and an allocation can collect, so an
    /// argument already converted is held as a temporary root until the frame
    /// that will own it exists. The roots are released here rather than after
    /// the run because nothing between this line and the write of the entry's
    /// frame allocates: [`Machine::run`] reserves stack words and copies the
    /// arguments into them, and the frame is a root from that moment on.
    /// Holding them for the length of the run instead would retain the
    /// entry's arguments past the point the lowering cleared their slots,
    /// which is exactly the retention the static reference map was careful
    /// not to be.
    fn words_of(&mut self, function: &Function, args: &[Value]) -> Result<Vec<u64>, RuntimeError> {
        let params = function.params.clone();
        let mark = self.machine.temps();
        let mut words = Vec::with_capacity(args.len());
        let mut failed = None;
        for (layout, value) in params.iter().zip(args) {
            // Each argument's own words, in declaration order, because that
            // is what the callee's frame is: parameters occupy it from slot 0
            // at their own widths, and a `(Int, Point, Int)` list is four
            // words rather than three slots.
            //
            // `from_value` releases its own temporary roots when it returns,
            // so every reference among the words is re-taken here and held
            // until the frame that will own it exists.
            match boundary::from_value(&mut self.machine, *layout, value) {
                Ok(written) => {
                    let reprs = self.program.layout(*layout).words.clone();
                    for (repr, word) in reprs.iter().zip(&written) {
                        if repr.is_ref() && *word != 0 {
                            self.machine.push_temp(*word);
                        }
                    }
                    words.extend_from_slice(&written);
                }
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
        self.machine.release_temps(mark);
        match failed {
            Some(error) => Err(error),
            None => Ok(words),
        }
    }

    fn lookup(&self, module: &str, name: &str) -> Result<FunctionId, RuntimeError> {
        self.program.function_named(module, name).ok_or_else(|| {
            RuntimeError::new(format!("this package does not declare `{module}.{name}`"))
        })
    }

    /// The same lookup, for a caller that has already established the package
    /// declares the function.
    ///
    /// [`crate::invoke::check`] has passed by the time this runs, so the
    /// package *does* declare it and the reader should not be told it does
    /// not. What is missing is the lowering, and the remedy is the caller's.
    fn lowered(&self, module: &str, name: &str) -> Result<FunctionId, RuntimeError> {
        self.program.function_named(module, name).ok_or_else(|| {
            RuntimeError::new(format!(
                "this run's lowering does not include `{module}.{name}`"
            ))
            .with_rule(
                "A run executes the functions one entry can reach, because that is what `lower_entry` lowers.",
            )
            .with_help(format!(
                "lower it too, by naming `{module}.{name}` as a root, and build the run on that program"
            ))
        })
    }

    /// Writes the run's terminal event, whichever way in produced it.
    ///
    /// Every path into a program passes through here, which is what makes
    /// "every run has one" true rather than a claim about the paths somebody
    /// remembered. An entry that answers `Err` is the program saying what it
    /// was written to say: a failure of the program's work and not of the
    /// run, which is why it is its own outcome rather than one more kind of
    /// stop.
    fn ended(&self, outcome: Result<Value, RuntimeError>) -> Result<Value, RuntimeError> {
        let (classification, message) = match &outcome {
            Ok(value) if value.is_err() => (
                RunOutcome::Error,
                crate::interp::returned_error_message(value),
            ),
            Ok(_) => (RunOutcome::Success, None),
            Err(error) => (error.outcome, Some(error.message.clone())),
        };
        self.runtime.trace(TraceEvent::RunEnded {
            outcome: classification,
            message,
        });
        outcome
    }
}

/// The meter a run charges through, over `hosts`'s budget or over none.
///
/// A registry with no budget installed answers `None`, which has always meant
/// no limit; a meter over default [`Limits`] is that, written down.
fn meter_of(hosts: &HostRegistry) -> Meter {
    hosts
        .budget_meter()
        .unwrap_or_else(|| Budget::new(Limits::default()).meter())
}
