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
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_sema::resolve::Program;

use crate::host::HostRegistry;
use crate::trace::{NullSink, TraceEvent, TraceSink};

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
}

impl Runtime {
    /// A run over `program`, reporting against `sources` and calling through
    /// `hosts`, with tracing switched off.
    pub fn new(program: Arc<Program>, sources: Arc<SourceMap>, hosts: Arc<HostRegistry>) -> Self {
        Runtime {
            program,
            sources,
            hosts,
            trace: Arc::new(NullSink),
            next_task_id: Arc::new(AtomicU64::new(1)),
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

    /// The host boundary, so a caller can read a run's counters after it
    /// finishes.
    pub fn hosts(&self) -> &HostRegistry {
        &self.hosts
    }

    /// Records one trace event, from whichever thread produced it.
    pub fn trace(&self, event: TraceEvent) {
        self.trace.record(event);
    }

    /// The next task id, unique across every thread of this run.
    pub fn next_task_id(&self) -> u64 {
        self.next_task_id.fetch_add(1, Ordering::Relaxed)
    }
}
