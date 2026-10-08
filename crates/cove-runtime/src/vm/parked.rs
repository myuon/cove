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
//! [`OwnedVm`] is a [`Vm`] that owns the three things a `Vm` borrows, so that
//! it is `'static` and `Send` and can be put in a queue. [`ParkedVm`] is one
//! of those that is parked at a host call. [`YieldedVm`] is one that gave its
//! thread up at a safepoint because its [`YieldRequest`] was raised
//! ([ADR 0084](../../../../docs/adr/0084-a-run-may-yield-at-a-safepoint.md)).
//! [`Step`] is what running one comes to: answered, parked, or yielded.

use std::any::Any;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cove_ir::Program;

use super::{PreparedProgram, Vm, DEFAULT_HEAP_WORDS};
use crate::budget::{Budget, Meter, Stopped};
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
    sends::<YieldedVm>();
    const fn shares<T: Send + Sync>() {}
    shares::<YieldRequest>();
};

impl OwnedVm {
    /// A run of the program `prepared` holds, over `runtime` and `hosts`.
    ///
    /// [`Vm::with_prepared`], owning its arguments: the program-wide work was
    /// done once by [`PreparedProgram::new`], and this pays for the run's own
    /// heap, stack and literals.
    ///
    /// A preparation made [`with_native`](PreparedProgram::with_native) gives
    /// the run its native tier: the shared machine code, entered where the
    /// encoded tier calls a compiled function, as
    /// [`Vm::with_native`] enters it ([ADR 0085]).
    ///
    /// [ADR 0085]: ../../../../docs/adr/0085-compiled-frames-resume-where-they-yielded.md
    pub fn new(
        runtime: Arc<Runtime>,
        hosts: Arc<HostRegistry>,
        prepared: PreparedProgram,
    ) -> OwnedVm {
        OwnedVm::with_heap_words(runtime, hosts, prepared, DEFAULT_HEAP_WORDS)
    }

    /// [`OwnedVm::new`], over a heap that may grow only to `heap_words` words
    /// — the capacity [`OwnedVm::heap_words`] is measured against.
    ///
    /// The heap's capacity, named as what it is rather than as a memory limit
    /// ([ADR 0088]): a run that needs more than this fails its allocation
    /// with the runtime's out-of-memory error, before the heap grows past it,
    /// and what a host holds outside the heap is not counted. When a run
    /// collects is unchanged ([ADR 0081]): it collects when it has allocated
    /// its allowance, and also when an allocation does not fit this capacity.
    ///
    /// [ADR 0081]: ../../../../docs/adr/0081-a-run-collects-when-it-has-allocated-its-allowance.md
    /// [ADR 0088]: ../../../../docs/adr/0088-an-embedder-sizes-the-heap-and-is-told-of-a-cancellation.md
    pub fn with_heap_words(
        runtime: Arc<Runtime>,
        hosts: Arc<HostRegistry>,
        prepared: PreparedProgram,
        heap_words: usize,
    ) -> OwnedVm {
        // Safety: each reference is to the inside of an `Arc` this struct
        // keeps, so it stays valid and unmoved for as long as the struct
        // lives, wherever the struct itself is moved to — an `Arc`'s contents
        // do not move with the handle. The `Vm` is dropped before the `Arc`s
        // (field order), and nothing below hands out the `Vm` or a reference
        // carrying its `'static`, which is what would let one outlive them.
        let mut vm = unsafe {
            let runtime_ref: &'static Runtime = &*Arc::as_ptr(&runtime);
            let hosts_ref: &'static HostRegistry = &*Arc::as_ptr(&hosts);
            let program: &'static Program = &*Arc::as_ptr(&prepared.program);
            Vm::assemble(
                runtime_ref,
                hosts_ref,
                program,
                &prepared.prepared,
                heap_words,
            )
        };
        if let Some(native) = &prepared.native {
            // Safety: the table is inside an `Arc` this struct keeps, for the
            // reason the three references above are sound, and the `Vm` that
            // holds a pointer to it is dropped first.
            unsafe {
                let native: &'static crate::native::NativeProgram = &*Arc::as_ptr(native);
                vm.machine.install_native(native);
            }
        }
        vm.machine
            .install_yield_request(Arc::new(AtomicBool::new(false)));
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

    /// [`Vm::work`].
    pub fn work(&self) -> u64 {
        self.vm.work()
    }

    /// The handle another thread raises to ask this machine's run to give its
    /// thread up at its next safepoint, answering [`Step::Yielded`].
    ///
    /// One per machine, for every run it makes; clones share it. Only a
    /// parkable run honours it, and only where it could park (ADR 0080 §2,
    /// less the `Shared` cell clause): inside a host's callback, beside a
    /// running task, below an encoded function that compiled code called, or
    /// under a debugger the request stays raised and the run yields at the
    /// first safepoint where it can. Inside compiled code it yields at a
    /// backedge, an allocation or a call ([ADR 0085](../../../../docs/adr/0085-compiled-frames-resume-where-they-yielded.md)).
    /// The runtime lowers it when the run yields, parks or answers, when a
    /// parkable run begins and when a yielded one resumes — a request is
    /// about the run that is running now. *When* to raise it is the embedder's: the runtime keeps no clock
    /// for it ([ADR 0084](../../../../docs/adr/0084-a-run-may-yield-at-a-safepoint.md)).
    pub fn yield_request(&self) -> YieldRequest {
        YieldRequest(Arc::clone(
            self.vm
                .machine
                .yield_flag()
                .expect("an `OwnedVm` installs its yield flag when it is built"),
        ))
    }

    /// How many safepoints, over this machine's life, were asked to yield
    /// and could not, because the run was not where it could give its thread
    /// up. Each is one stride the request waited longer.
    pub fn yields_declined(&self) -> u64 {
        self.vm.machine.yields_declined()
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

    /// [`Vm::collections`].
    pub fn collections(&self) -> u64 {
        self.vm.collections()
    }

    /// [`Vm::assertion_failure`]: where the last failed `assert` of the last
    /// run was, and its message, for a test runner reporting at the
    /// assertion rather than at the test.
    pub fn assertion_failure(&self) -> Option<(cove_diag::Span, &str)> {
        self.vm.assertion_failure()
    }

    /// [`Vm::tiers`]: how this machine's calls divided between the encoded
    /// and the native tier, over every run it has made. All nought for a
    /// machine built from a preparation without
    /// [`with_native`](PreparedProgram::with_native).
    pub fn tiers(&self) -> crate::Tiers {
        self.vm.tiers()
    }

    /// The run, answered, parked or yielded — with its yield request
    /// lowered, since either way it has left the thread.
    fn step(self, outcome: Option<Result<Value, RuntimeError>>) -> Step {
        self.vm.machine.clear_yield_request();
        match outcome {
            Some(outcome) => Step::Answered(self, outcome),
            None if self.vm.machine.is_yielded() => Step::Yielded(YieldedVm { vm: self }),
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
    /// The run's [`YieldRequest`] was raised, and it gave its thread up at a
    /// safepoint. It needs no answer: [`YieldedVm::resume`] runs it on.
    Yielded(YieldedVm),
}

/// The flag that asks a run to give its thread up at its next safepoint.
///
/// From [`OwnedVm::yield_request`]; `Send + Sync` and cheap to clone, so a
/// scheduler's monitor thread can hold one per running run and raise it when
/// the run has had its slice. Raising it is a relaxed store and costs the run
/// nothing until its next safepoint, where it is one load — at most
/// [`SAFEPOINT_STRIDE`](super::exec::SAFEPOINT_STRIDE) instructions later.
#[derive(Clone, Debug)]
pub struct YieldRequest(Arc<AtomicBool>);

impl YieldRequest {
    /// Asks the run to yield. Idempotent, and harmless when the run is not
    /// running: the flag is lowered whenever the run leaves its thread.
    pub fn request(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether a request is raised and not yet honoured.
    pub fn is_requested(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// A run that gave its thread up at a safepoint, because its [`YieldRequest`]
/// was raised.
///
/// `Send`, as a [`ParkedVm`] is and for the same reason: it yielded only
/// where it could have parked — no callback below it, no task running, no
/// compiled frame on the native stack — so what it holds is its heap, its
/// stack and its frames. A run that yielded inside compiled code left its
/// compiled frames standing as the VM frames they are, and unwound the
/// native stack ([ADR 0085](../../../../docs/adr/0085-compiled-frames-resume-where-they-yielded.md)).
/// It may hold a `Shared` cell, which is the task's and not the thread's
/// ([ADR 0084](../../../../docs/adr/0084-a-run-may-yield-at-a-safepoint.md)).
///
/// It stands before the instruction it was about to run, and the safepoint
/// it yielded at has not been taken: resuming takes it, so the run is charged,
/// collected and counted exactly as one that was never interrupted, and its
/// trace is that run's — a yield writes no event.
pub struct YieldedVm {
    vm: OwnedVm,
}

impl YieldedVm {
    /// Runs the run on, on this thread, until it answers, parks or yields
    /// again.
    ///
    /// Asked first, as [`ParkedVm::resume`] asks, whether the run has been
    /// stopped while it waited: its [`Cancellation`](crate::Cancellation)
    /// raised or its deadline passed. A deadline is wall-clock and kept
    /// running while the run was off its thread. If it has, the run ends as
    /// [`YieldedVm::cancel`] ends it, with that stop.
    pub fn resume(mut self) -> Step {
        if let Some(stopped) = self.vm.meter().interrupted() {
            return self.stop(stopped);
        }
        let outcome = self.vm.vm.resume_yielded();
        self.vm.step(outcome)
    }

    /// Ends the run at the safepoint it yielded at, the way that safepoint
    /// would have ended it: [`Stopped::Cancelled`] if its flag is raised,
    /// [`Stopped::Deadline`] if its deadline has passed, and
    /// [`Stopped::Cancelled`] otherwise. The error is the budget's own at the
    /// instruction the run stood before, with the call chain under it; the
    /// entry's `entry_exit`, `heap_summary` and `run_ended` are written,
    /// classified as the stop; a cell the run held is given back; and the
    /// machine comes back for its next run.
    pub fn cancel(self) -> (OwnedVm, RuntimeError) {
        let stopped = self.vm.meter().interrupted().unwrap_or(Stopped::Cancelled);
        match self.stop(stopped) {
            Step::Answered(vm, Err(error)) => (vm, error),
            Step::Answered(..) | Step::Parked(_) | Step::Yielded(_) => {
                unreachable!("a yielded run stopped ends with that stop")
            }
        }
    }

    /// What the run's deadline leaves, or `None` for a run with no deadline:
    /// [`ParkedVm::time_left`]'s answer, for a run waiting in a queue rather
    /// than on a host.
    pub fn time_left(&self) -> Option<Duration> {
        self.vm.meter().time_left()
    }

    fn stop(mut self, stopped: Stopped) -> Step {
        let outcome = self.vm.vm.stop_yielded(stopped);
        self.vm.step(outcome)
    }

    /// [`Vm::instructions`], up to the instruction it stands before.
    pub fn instructions(&self) -> u64 {
        self.vm.instructions()
    }

    /// [`Vm::work`], up to the instruction it stands before.
    pub fn work(&self) -> u64 {
        self.vm.work()
    }

    /// [`Vm::meter`]: the run's accounting so far.
    pub fn meter(&self) -> &Meter {
        self.vm.meter()
    }

    /// [`OwnedVm::yield_request`].
    pub fn yield_request(&self) -> YieldRequest {
        self.vm.yield_request()
    }

    /// [`OwnedVm::heap_words`]: the heap as the run left it at the safepoint
    /// it yielded at — where a host enforcing a heap limit of its own looks
    /// between two slices.
    pub fn heap_words(&self) -> u64 {
        self.vm.heap_words()
    }

    /// [`OwnedVm::yields_declined`], up to the safepoint it yielded at.
    pub fn yields_declined(&self) -> u64 {
        self.vm.yields_declined()
    }

    /// How many compiled frames the run left standing when it yielded: nought
    /// for a yield the dispatch loop took, and otherwise the frames that
    /// resuming re-enters, innermost first, before the loop goes on
    /// ([ADR 0085](../../../../docs/adr/0085-compiled-frames-resume-where-they-yielded.md)).
    pub fn compiled_frames(&self) -> usize {
        self.vm.vm.machine.yielded_compiled_frames()
    }
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
/// which is what a run that never finished looks like. [`ParkedVm::cancel`]
/// ends it instead, with a trace that says so, and [`ParkedVm::time_left`] is
/// how long its deadline has left
/// ([ADR 0082](../../../../docs/adr/0082-a-parked-run-keeps-its-deadline.md)).
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
    ///
    /// # A run whose deadline passed while it was parked
    ///
    /// A deadline is wall-clock and kept running while the run was parked, so
    /// the run is asked first whether it has already been stopped: its
    /// [`Cancellation`](crate::Cancellation) raised, or its deadline passed.
    /// If it has, the answer is discarded unread and the run ends exactly as
    /// [`ParkedVm::cancel`] ends it, with that stop. A run resumed after its
    /// deadline would otherwise go on until its next safepoint read the
    /// clock — and a run with no safepoint left before it answered would
    /// answer, past a deadline that was supposed to bound it
    /// ([ADR 0082](../../../../docs/adr/0082-a-parked-run-keeps-its-deadline.md)).
    pub fn resume(self, answer: Result<Transfer, RuntimeError>) -> Step {
        if let Some(stopped) = self.vm.meter().interrupted() {
            return self.stop(stopped);
        }
        self.settle(answer.map(Transfer::into_value))
    }

    /// Ends the run at the call it is parked at, without an answer.
    ///
    /// The run stops the way a running one stops at a safepoint, asked in the
    /// order a safepoint asks: [`Stopped::Cancelled`] if its flag is raised,
    /// [`Stopped::Deadline`] if its deadline has passed, and
    /// [`Stopped::Cancelled`] otherwise — this call being the embedder
    /// cancelling it. The error is the budget's own,
    /// the one a running run stopped by the same control reports, with the
    /// parked call's span and the call chain under it. The call's `host_call`
    /// event is written with that error as its outcome and the time it was
    /// parked as its wait, and the entry's `entry_exit`, `heap_summary` and
    /// `run_ended` follow, classified as the stop — so a trace says how the
    /// run ended, which a dropped `ParkedVm`'s does not.
    ///
    /// A parked run holds nothing else to give back: it is quiescent (ADR
    /// 0080 §2), so it has no task running, no `Shared` cell held and no
    /// callback below it, and its stride was checked before the call. The
    /// host's request, if it was not taken, is dropped.
    ///
    /// The machine comes back with the error, ready for its next run, as an
    /// answered [`Step`] would hand it back.
    pub fn cancel(self) -> (OwnedVm, RuntimeError) {
        let stopped = self.vm.meter().interrupted().unwrap_or(Stopped::Cancelled);
        match self.stop(stopped) {
            Step::Answered(vm, Err(error)) => (vm, error),
            Step::Answered(..) | Step::Parked(_) | Step::Yielded(_) => {
                unreachable!("a run resumed with a stop ends with that stop")
            }
        }
    }

    /// What the run's deadline leaves, or `None` for a run with no deadline:
    /// zero once it has passed.
    ///
    /// The clock started when the run did and kept running while it was
    /// parked, so this is the whole of what a scheduler needs to time a
    /// parked run out: set a timer for this long, and [`ParkedVm::cancel`]
    /// the run if its answer has not come by then. When to look is the
    /// embedder's; the runtime does not wake a parked run by itself.
    pub fn time_left(&self) -> Option<Duration> {
        self.vm.meter().time_left()
    }

    /// Ends the run with `stopped`, as a host call that failed with the
    /// budget's error for it.
    fn stop(self, stopped: Stopped) -> Step {
        let error = self.vm.meter().to_runtime_error(stopped);
        self.settle(Err(error))
    }

    /// The parked call answered with `answer`, and the run driven on from it.
    fn settle(mut self, answer: Result<Value, RuntimeError>) -> Step {
        let outcome = self.vm.vm.resume(answer);
        self.vm.step(outcome)
    }

    /// [`Vm::instructions`], up to the call it is parked at.
    pub fn instructions(&self) -> u64 {
        self.vm.instructions()
    }

    /// [`Vm::work`], up to the call it is parked at.
    pub fn work(&self) -> u64 {
        self.vm.work()
    }

    /// [`Vm::meter`]: the parked run's accounting, up to the call it is
    /// parked at.
    pub fn meter(&self) -> &Meter {
        self.vm.meter()
    }

    /// [`OwnedVm::yield_request`]: the handle a scheduler's monitor raises
    /// once this run is resumed and has had its slice.
    pub fn yield_request(&self) -> YieldRequest {
        self.vm.yield_request()
    }

    /// [`OwnedVm::heap_words`]: the heap as the run left it at the call it is
    /// parked at — where a host enforcing a heap limit of its own looks
    /// before it answers.
    pub fn heap_words(&self) -> u64 {
        self.vm.heap_words()
    }

    /// [`OwnedVm::yields_declined`], up to the call it is parked at.
    pub fn yields_declined(&self) -> u64 {
        self.vm.yields_declined()
    }
}
