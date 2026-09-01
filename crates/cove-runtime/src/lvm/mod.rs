//! The execution backend [ADR 0034](../../../../docs/adr/0034-one-physical-word-stack.md)
//! decides, being built alongside the one it replaces.
//!
//! [`docs/LINEAR_VM.md`](../../../../docs/LINEAR_VM.md) is the design. It is a
//! clean-room replacement rather than a renovation: nothing here is derived
//! from [`crate::vm`], [`crate::frame`] or `cove_ir`, and this module imports
//! from none of them. Those are frozen — fixed only where a fix keeps the
//! oracle and the differential gate usable — and deleted at the cutover, when
//! `cove-lir` and `lvm` take the names `cove-ir` and `vm`.
//!
//! The memory, the dispatch loop, the boundary and [`Lvm`] — the type an
//! embedder holds — all exist, and since `cove run --backend lvm` the last
//! of those is reached from outside the crate: [`Lvm`] is re-exported from
//! the crate root and nothing else here is, because what a caller can name
//! is the whole of what this boundary decides. A word, a layout, a
//! [`mem::Memory`] and a [`exec::Machine`] are the representation, and a
//! representation that leaves the crate is one that cannot be changed
//! without changing somebody else's code.
//!
//! The dead-code allowance stays, and what it now covers is narrower than
//! what it covered before: not "nothing reaches this module" but "several
//! items below it are reached only from their own `#[cfg(test)]` code" —
//! `Machine::new`, the collection counters a test asserts over, and
//! [`Lvm::collected`] and [`Lvm::host_wait`], the two figures this type can
//! report that nothing outside asks for yet. It is one line in one place
//! rather than an attribute per item, so that removing it is a single edit
//! whose failure lists exactly what is still unused.
#![allow(dead_code)]

use std::rc::Rc;
use std::time::Duration;

use cove_lir::{Function, FunctionId, Program};

use crate::budget::{Budget, Limits, Meter};
use crate::error::RuntimeError;
use crate::host::HostRegistry;
use crate::lvm::exec::Machine;
use crate::lvm::mem::Collected;
use crate::runtime::Runtime;
use crate::trace::{RunOutcome, TraceEvent};
// The public `Value` reaches this file for the one reason ADR 0034 allows it
// to reach any of them: this is a boundary. An entry's arguments and its
// answer are what a host hands in and reads back, and they are `Value`s on
// both sides of that line. Nothing here stores one — every value named below
// is on its way into [`boundary::from_value`] or out of
// [`boundary::to_value`].
use crate::value::Value;

pub(crate) mod boundary;
pub(crate) mod builtins;
#[cfg(test)]
mod differential;
pub(crate) mod exec;
pub(crate) mod mem;

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
/// [`crate::interp::Interpreter`] offers. [`Lvm::run_entry`] is how a
/// *command* speaks to a program: the arguments are process arguments, which
/// are strings. [`Lvm::invoke`] is how an *application* does: the arguments
/// are values the host built, held to the types the checker resolved before
/// the first instruction runs. Everything below the two is one path.
pub struct Lvm<'a> {
    runtime: &'a Runtime,
    program: &'a Program,
    machine: Machine<'a>,
    /// The run's accounting, in the handle a safepoint charges through.
    ///
    /// Taken once, where the run begins, for the reason [`Meter`] gives. A
    /// registry with no budget installed answers `None`, which has always
    /// meant no limit; a meter over default [`Limits`] is that, written down.
    budget: Meter,
}

impl<'a> Lvm<'a> {
    /// A run of `program`, over `runtime`'s checked program and `hosts`.
    ///
    /// The heap budget is this module's `DEFAULT_HEAP_WORDS` and is not a
    /// parameter yet:
    /// no caller has had a reason to name one, and a knob nobody turns is a
    /// knob whose meaning nobody has had to decide.
    pub fn new(runtime: &'a Runtime, hosts: &'a HostRegistry, program: &'a Program) -> Lvm<'a> {
        Lvm {
            runtime,
            program,
            machine: Machine::with_hosts(program, DEFAULT_HEAP_WORDS, Some(hosts)),
            budget: hosts
                .budget_meter()
                .unwrap_or_else(|| Budget::new(Limits::default()).meter()),
        }
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
    pub fn invoke(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        let outcome = self.invoke_checked(module, name, args);
        self.ended(outcome)
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
        let id = self.lookup(module, name)?;
        self.enter_with(module, name, id, args)
    }

    /// The call itself, from the arguments in to the answer out.
    ///
    /// The one seam. [`Lvm::run_entry`] reaches it having turned the process
    /// arguments into the array an entry declares, and [`Lvm::invoke`]
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

        let words = self.words_of(function, &args).map_err(|e| e.at(span))?;
        let answer = self.machine.run(id, &words, &self.budget)?;
        boundary::to_value(&self.machine, returns, &answer).map_err(|e| e.at(span))
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
