//! A run that owns what it runs over, and a run parked at a host call.
//!
//! [ADR 0080](../../../../docs/adr/0080-a-host-call-may-answer-pending.md)'s
//! mechanism, and only the mechanism: an embedder that drives thousands of runs
//! from a pool of worker threads parks a run while it waits on a host call and
//! resumes it on whichever worker is free, and everything about *when* and
//! *where* is the embedder's. Nothing here schedules, queues or polls. A run
//! answers, or it hands itself back parked with the host's request, and the
//! embedder decides what happens next — which is the line PHILOSOPHY's
//! "Separate policy from mechanism" draws.
//!
//! Three types. [`OwnedVm`] is a [`Vm`] that owns the three things a `Vm`
//! borrows, so that it is `'static` and `Send` and can be put in a queue.
//! [`ParkedVm`] is one of those that is parked at a host call. [`Step`] is
//! what running one comes to: answered, or parked.

use std::any::Any;
use std::rc::Rc;
use std::sync::Arc;

use cove_ir::Program;

use super::{PreparedProgram, Vm, DEFAULT_HEAP_WORDS};
use crate::budget::{Budget, Meter};
use crate::error::RuntimeError;
use crate::host::HostRegistry;
use crate::runtime::Runtime;
use crate::task::Transfer;
use crate::value::Value;

/// A run that owns its program, its runtime and its host registry, so that it
/// has no lifetime and may be moved to another thread.
///
/// A [`Vm`] borrows all three, which is right for a caller that runs a program
/// on the thread that holds them and wrong for a scheduler, whose runs outlive
/// any one stack frame and travel between threads. This holds the borrowed
/// three by `Arc` beside the `Vm` that borrows them — the same `Arc`s many runs
/// can share, since [`PreparedProgram`] is one program's encoding for any
/// number of runs — and offers the `Vm`'s ways in.
///
/// The ordinary ones, [`OwnedVm::invoke`] and its siblings, are the `Vm`'s own
/// and block at a host call exactly as they do there. The parkable ones,
/// [`OwnedVm::invoke_parkable`] and [`OwnedVm::run_entry_parkable`], take the
/// run by value and answer a [`Step`]: a host that answers
/// [`HostAnswer::Pending`](crate::HostAnswer::Pending) where the run can park
/// hands it back as a [`ParkedVm`].
///
/// # Why it is not a `Deref` to the `Vm`
///
/// The `Vm` inside borrows the `Arc`s beside it, written as `Vm<'static>`
/// because a struct cannot name its own lifetime. That is sound only while the
/// `Vm` cannot leave the struct whose `Arc`s it borrows, and a `&mut Vm` handed
/// out would let a caller swap two of them. So the methods are forwarded, one
/// by one, and none of them hands out the `Vm` or anything carrying its
/// lifetime.
pub struct OwnedVm {
    // Declared first, so dropped first: it borrows from every field below it,
    // and Rust drops a struct's fields in declaration order.
    vm: Vm<'static>,
    _runtime: Arc<Runtime>,
    _hosts: Arc<HostRegistry>,
    _prepared: PreparedProgram,
}

// Checked where it is claimed, so that a field that stopped being `Send` fails
// this crate's build rather than an embedder's.
const _: () = {
    const fn sends<T: Send>() {}
    sends::<OwnedVm>();
    sends::<ParkedVm>();
};

impl OwnedVm {
    /// A run of the program `prepared` holds, over `runtime` and `hosts`.
    ///
    /// [`Vm::with_prepared`], owning its arguments: the program-wide work was
    /// done once by [`PreparedProgram::new`], and this pays for the run's own
    /// heap, stack and literals.
    pub fn new(
        runtime: Arc<Runtime>,
        hosts: Arc<HostRegistry>,
        prepared: PreparedProgram,
    ) -> OwnedVm {
        // Safety: each reference is to the inside of an `Arc` this struct
        // keeps, so it stays valid and unmoved for as long as the struct
        // lives, wherever the struct itself is moved to — an `Arc`'s contents
        // do not move with the handle. The `Vm` is dropped before the `Arc`s
        // (field order), and nothing below hands out the `Vm` or a reference
        // carrying its `'static`, which is what would let one outlive them.
        let vm = unsafe {
            let runtime_ref: &'static Runtime = &*Arc::as_ptr(&runtime);
            let hosts_ref: &'static HostRegistry = &*Arc::as_ptr(&hosts);
            let program: &'static Program = &*Arc::as_ptr(&prepared.program);
            Vm::assemble(
                runtime_ref,
                hosts_ref,
                program,
                &prepared.prepared,
                DEFAULT_HEAP_WORDS,
            )
        };
        OwnedVm {
            vm,
            _runtime: runtime,
            _hosts: hosts,
            _prepared: prepared,
        }
    }

    /// [`Vm::invoke`]: every host call blocks.
    pub fn invoke(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        self.vm.invoke(module, name, args)
    }

    /// [`Vm::invoke_within`]: every host call blocks.
    pub fn invoke_within(
        &mut self,
        budget: Budget,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, RuntimeError> {
        self.vm.invoke_within(budget, module, name, args)
    }

    /// [`Vm::run_entry`]: every host call blocks.
    pub fn run_entry(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        self.vm.run_entry(module, name, args)
    }

    /// [`Vm::invoke`], with its host calls allowed to answer pending.
    ///
    /// The same check, the same conversion and the same trace; the one
    /// difference is that a host call made where the run can park is offered
    /// to [`HostApi::call_parkable`](crate::HostApi::call_parkable) rather
    /// than to `call_with`, and a pending answer hands the run back as
    /// [`Step::Parked`]. Where the run cannot park — inside a callback, with a
    /// task running, inside a `lock` — the call blocks, as it would here
    /// anyway.
    pub fn invoke_parkable(mut self, module: &str, name: &str, args: Vec<Value>) -> Step {
        let outcome = self.vm.parkable(|vm| vm.invoke_checked(module, name, args));
        self.step(outcome)
    }

    /// [`Vm::invoke_within`], with its host calls allowed to answer pending.
    ///
    /// The budget bounds the whole run, parked time included: a deadline is a
    /// wall-clock bound, and a run that waits for its host is still running.
    pub fn invoke_within_parkable(
        mut self,
        budget: Budget,
        module: &str,
        name: &str,
        args: Vec<Value>,
    ) -> Step {
        let outcome = self
            .vm
            .parkable(|vm| vm.checked_within(budget, module, name, args));
        self.step(outcome)
    }

    /// [`Vm::run_entry`], with its host calls allowed to answer pending.
    pub fn run_entry_parkable(mut self, module: &str, name: &str, args: Vec<Rc<str>>) -> Step {
        let outcome = self.vm.parkable(|vm| vm.enter(module, name, args));
        self.step(outcome)
    }

    /// [`Vm::instructions`].
    pub fn instructions(&self) -> u64 {
        self.vm.instructions()
    }

    /// [`Vm::meter`]: this run's accounting, or the last one's.
    ///
    /// The run's own, however many other runs share its registry: a budget
    /// travels with the run that was given it (issue #577).
    pub fn meter(&self) -> &Meter {
        self.vm.meter()
    }

    /// [`Vm::heap_words`].
    pub fn heap_words(&self) -> u64 {
        self.vm.heap_words()
    }

    /// [`Vm::allocated_words`].
    pub fn allocated_words(&self) -> u64 {
        self.vm.allocated_words()
    }

    /// The run, answered or parked.
    fn step(self, outcome: Option<Result<Value, RuntimeError>>) -> Step {
        match outcome {
            Some(outcome) => Step::Answered(self, outcome),
            None => Step::Parked(ParkedVm { vm: self }),
        }
    }
}

/// What running an [`OwnedVm`] parkably came to.
///
/// Not `Send`, and it does not need to be: [`Step::Answered`] carries the
/// answer as a [`Value`], which the thread that ran the run reads, and
/// [`Step::Parked`] is taken apart on that same thread into the [`ParkedVm`]
/// that *is* `Send`.
pub enum Step {
    /// The run answered — a value, or the error it stopped with — and the
    /// machine is handed back for its next run.
    Answered(OwnedVm, Result<Value, RuntimeError>),
    /// A host call answered pending, and the run is waiting for its answer.
    Parked(ParkedVm),
}

/// A run parked at a host call, waiting for the answer.
///
/// `Send`: hand it to any thread and [`ParkedVm::resume`] it there. What it
/// holds is the run — its heap, its stack, its frames, the host call it stands
/// at — and the request the host answered with; none of it is a [`Value`] or
/// anything else a thread owns, which is what [ADR 0080]'s quiescence
/// condition is for.
///
/// Dropping one abandons the run. Its memory is freed and its host call never
/// answers: the trace has the entry's `entry_enter` and nothing after it,
/// which is what a run that never finished looks like.
///
/// [ADR 0080]: ../../../../docs/adr/0080-a-host-call-may-answer-pending.md
pub struct ParkedVm {
    vm: OwnedVm,
}

impl ParkedVm {
    /// What the host answered pending with, unless it has been taken.
    ///
    /// The host's own type, which the host and the embedder agree on:
    /// `downcast_ref` it.
    pub fn request(&self) -> Option<&(dyn Any + Send)> {
        self.vm.vm.machine.request()
    }

    /// Takes the request out, so that it can be moved to whatever answers it
    /// while the run stays here. A second call answers `None`.
    pub fn take_request(&mut self) -> Option<Box<dyn Any + Send>> {
        self.vm.vm.machine.take_request()
    }

    /// Resumes the run with its host call's answer, on this thread, and runs
    /// it until it answers or parks again.
    ///
    /// The answer is a [`Transfer`], which is `Send`, because it is made
    /// wherever the host's work finished and that is rarely this thread. It
    /// is turned into a value here, and from there it is what a host that
    /// answered at once would have returned: traced, held to the result the
    /// operation declares, and written into the run. An `Err` is a host that
    /// failed, as `call_with`'s would be.
    pub fn resume(mut self, answer: Result<Transfer, RuntimeError>) -> Step {
        let outcome = self.vm.vm.resume(answer.map(Transfer::into_value));
        self.vm.step(outcome)
    }

    /// [`Vm::instructions`], up to the call it is parked at.
    pub fn instructions(&self) -> u64 {
        self.vm.instructions()
    }

    /// [`Vm::meter`]: the parked run's accounting, up to the call it is
    /// parked at.
    pub fn meter(&self) -> &Meter {
        self.vm.meter()
    }
}
