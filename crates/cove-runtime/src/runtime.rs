//! What every thread of one run shares.
//!
//! ADR 0008 runs each spawned task on its own thread, so everything a task
//! body needs has to outlive the stack frame that spawned it and be reachable
//! from another thread: the resolved program the body resolves names in, the
//! source map a diagnostic points into, the host boundary the body calls
//! through, and where trace events go.
//!
//! Every one of them is either immutable or synchronized by its owner, so a
//! `Runtime` is shared rather than copied: cloning one hands a task thread a
//! handle to the same program, the same hosts, and the same trace.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cove_diag::SourceMap;
use cove_sema::resolve::Program;

use crate::bindings::Bindings;
use crate::heap::HeapStats;
use crate::host::HostRegistry;
use crate::trace::{NullSink, TraceEvent, TraceSink};

/// The task id the entry runs under.
///
/// The entry is not a spawned task and has no id of its own, so it takes the
/// one id [`Runtime::next_task_id`] never hands out. Every event that names a
/// task names it this way, so a trace has one convention for "which task"
/// rather than one per event.
pub const ENTRY_TASK: u64 = 0;

/// The shared context of one run.
#[derive(Clone)]
pub struct Runtime {
    program: Arc<Program>,
    sources: Arc<SourceMap>,
    hosts: Arc<HostRegistry>,
    trace: Arc<dyn TraceSink>,
    /// The next id [`Runtime::next_task_id`] hands out. Task ids are a trace
    /// identity, so they are drawn from one counter for the whole run: two
    /// tasks spawned at the same time on different threads still get
    /// different ids.
    next_task_id: Arc<AtomicU64>,
    /// What every heap of this run has done, folded in as each one is retired.
    ///
    /// A heap belongs to one thread and is never shared, so nothing is
    /// contended here while a task runs: a thread accumulates locally and
    /// takes this lock once, when its heap ends.
    heap: Arc<Mutex<HeapStats>>,
    /// Which frame index each name reference of `program` denotes, where that
    /// could be worked out before the run.
    ///
    /// Resolved once, here, because it is a fact about the program and every
    /// task of the run reads the same program. A task thread is handed this
    /// with the rest of the run's shared state rather than resolving a body
    /// again when it runs one.
    bindings: Arc<Bindings>,
    /// How many local reads this run resolved and how many it searched for.
    ///
    /// One set of counters for the whole run, for the same reason the heap
    /// totals are: a task is a thread, and what the run did is the sum of
    /// what its threads did.
    #[cfg(debug_assertions)]
    resolution: Arc<crate::bindings::ResolutionStats>,
}

impl Runtime {
    /// A run over `program`, reporting against `sources` and calling through
    /// `hosts`, with tracing switched off.
    pub fn new(program: Arc<Program>, sources: Arc<SourceMap>, hosts: Arc<HostRegistry>) -> Self {
        let bindings = Arc::new(Bindings::of(&program));
        Runtime {
            program,
            sources,
            hosts,
            trace: Arc::new(NullSink),
            next_task_id: Arc::new(AtomicU64::new(1)),
            heap: Arc::new(Mutex::new(HeapStats::default())),
            bindings,
            #[cfg(debug_assertions)]
            resolution: Arc::new(crate::bindings::ResolutionStats::default()),
        }
    }

    /// Sends this run's trace events to `sink`. Replaces any sink installed
    /// earlier; the default is [`NullSink`], which discards everything.
    pub fn with_trace(mut self, sink: Arc<dyn TraceSink>) -> Self {
        self.trace = sink;
        self
    }

    pub fn program(&self) -> &Program {
        &self.program
    }

    pub fn sources(&self) -> &SourceMap {
        &self.sources
    }

    /// What this run resolved its name references to before it started.
    pub fn bindings(&self) -> &Bindings {
        &self.bindings
    }

    /// The counters every task of this run adds to as it reads locals.
    #[cfg(debug_assertions)]
    pub fn resolution(&self) -> &crate::bindings::ResolutionStats {
        &self.resolution
    }

    /// How many local reads this run has resolved by index, and how many it
    /// searched for by name.
    #[cfg(debug_assertions)]
    pub fn resolution_counts(&self) -> crate::bindings::ResolutionCounts {
        self.resolution.counts()
    }

    /// The host boundary, so a caller can read a run's counters after it
    /// finishes.
    pub fn hosts(&self) -> &HostRegistry {
        &self.hosts
    }

    /// Records one trace event, from whichever thread produced it.
    pub fn trace(&self, event: TraceEvent) {
        self.trace.record(event);
    }

    /// The next task id, unique across every thread of this run, and never
    /// [`ENTRY_TASK`].
    pub fn next_task_id(&self) -> u64 {
        self.next_task_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Folds a finished heap's totals into the run's.
    ///
    /// Only the counters are folded. What a retired heap last measured as live
    /// went with the thread that owned it, so summing those would report
    /// memory that no longer exists; see [`Interpreter::heap_stats`] for where
    /// the live figure comes from instead.
    ///
    /// [`Interpreter::heap_stats`]: crate::interp::Interpreter::heap_stats
    pub fn retire_heap(&self, stats: &HeapStats) {
        self.locked_heap().merge(stats);
    }

    /// What every heap retired so far has done.
    pub fn heap_stats(&self) -> HeapStats {
        *self.locked_heap()
    }

    /// A poisoned lock means a thread panicked while folding its totals in.
    /// Statistics are not a state anything recovers from, so the numbers are
    /// taken back rather than turned into a second, unrelated failure.
    fn locked_heap(&self) -> std::sync::MutexGuard<'_, HeapStats> {
        self.heap
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
