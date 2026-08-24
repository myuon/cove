//! Runtime observability.
//!
//! ADR 0001 asks the runtime to trace host calls, capability use, and task
//! lifecycle "without language-specific application hooks" — a program does
//! not opt in to being traced, and it cannot opt out of being traceable. This
//! module defines the event shape and where events go; [`crate::host`] and
//! (in a later pass) the interpreter are the only places that produce them.
//!
//! # JSON schema
//!
//! [`JsonlSink`] writes one JSON object per line. The event's `"event"` key
//! names the variant in `snake_case`, and every `Duration` field is rendered
//! as an integer count of nanoseconds under a key ending in `_ns`. These keys
//! are a stable, documented interface — a trace format that changes silently
//! breaks whatever reads it:
//!
//! ```text
//! {"event":"task_spawned","id":<u64>,"parent":<u64|null>,"scope":<string>}
//! {"event":"task_completed","id":<u64>,"cpu_ns":<u64>}
//! {"event":"task_cancelled","id":<u64>}
//! {"event":"host_call","module":<string>,"op":<string>,"capability":<string>,"wait_ns":<u64>,"granted":<bool>}
//! {"event":"entry_enter","module":<string>,"function":<string>}
//! {"event":"entry_exit","module":<string>,"function":<string>,"cpu_ns":<u64>,"wait_ns":<u64>}
//! ```

use std::io::Write;
use std::time::{Duration, Instant};

/// One recorded runtime event.
#[derive(Clone, Debug)]
pub enum TraceEvent {
    /// A task was created in `scope`, as a child of `parent` (or none, for a
    /// root task).
    TaskSpawned {
        id: u64,
        parent: Option<u64>,
        scope: String,
    },
    /// A task ran to completion, having spent `cpu` executing (not waiting
    /// on a host call).
    TaskCompleted { id: u64, cpu: Duration },
    /// A task was cancelled before it completed.
    TaskCancelled { id: u64 },
    /// A Host API call was dispatched (`granted: true`) or rejected
    /// (`granted: false`), after waiting `wait` for the host to respond. For
    /// a rejected call, `wait` is the time spent deciding to reject it, which
    /// is ordinarily negligible.
    HostCall {
        module: String,
        op: String,
        capability: String,
        wait: Duration,
        granted: bool,
    },
    /// A host-selected entry function began running.
    EntryEnter { module: String, function: String },
    /// A host-selected entry function finished, having spent `cpu` executing
    /// and `wait` waiting on host calls.
    EntryExit {
        module: String,
        function: String,
        cpu: Duration,
        wait: Duration,
    },
}

/// Where trace events go.
pub trait TraceSink {
    /// Records one event. Must not panic: a broken trace sink should degrade
    /// the trace, not the program being traced.
    fn record(&mut self, event: TraceEvent);
}

/// Discards every event. The default when a run is not being traced.
pub struct NullSink;

impl TraceSink for NullSink {
    fn record(&mut self, _event: TraceEvent) {}
}

/// Writes one JSON object per line to `W`, flushing after every event so a
/// trace is visible as it happens rather than only at exit.
pub struct JsonlSink<W: Write> {
    writer: W,
}

impl<W: Write> JsonlSink<W> {
    /// Writes trace lines to `writer`.
    pub fn new(writer: W) -> Self {
        JsonlSink { writer }
    }
}

impl<W: Write> TraceSink for JsonlSink<W> {
    fn record(&mut self, event: TraceEvent) {
        let line = to_json_line(&event);
        // A trace sink degrades silently: losing a trace line must never
        // fail the run it is observing.
        let _ = writeln!(self.writer, "{line}");
        let _ = self.writer.flush();
    }
}

/// Escapes `s` as a JSON string literal, including the surrounding quotes.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Renders a `Duration` as the integer nanosecond count a `_ns`-suffixed key
/// expects.
fn json_ns(d: Duration) -> u128 {
    d.as_nanos()
}

/// Renders one [`TraceEvent`] as the single JSON line documented on this
/// module.
fn to_json_line(event: &TraceEvent) -> String {
    match event {
        TraceEvent::TaskSpawned { id, parent, scope } => {
            let parent = match parent {
                Some(id) => id.to_string(),
                None => "null".to_string(),
            };
            format!(
                "{{\"event\":\"task_spawned\",\"id\":{id},\"parent\":{parent},\"scope\":{}}}",
                json_string(scope)
            )
        }
        TraceEvent::TaskCompleted { id, cpu } => format!(
            "{{\"event\":\"task_completed\",\"id\":{id},\"cpu_ns\":{}}}",
            json_ns(*cpu)
        ),
        TraceEvent::TaskCancelled { id } => {
            format!("{{\"event\":\"task_cancelled\",\"id\":{id}}}")
        }
        TraceEvent::HostCall {
            module,
            op,
            capability,
            wait,
            granted,
        } => format!(
            "{{\"event\":\"host_call\",\"module\":{},\"op\":{},\"capability\":{},\"wait_ns\":{},\"granted\":{granted}}}",
            json_string(module),
            json_string(op),
            json_string(capability),
            json_ns(*wait),
        ),
        TraceEvent::EntryEnter { module, function } => format!(
            "{{\"event\":\"entry_enter\",\"module\":{},\"function\":{}}}",
            json_string(module),
            json_string(function)
        ),
        TraceEvent::EntryExit {
            module,
            function,
            cpu,
            wait,
        } => format!(
            "{{\"event\":\"entry_exit\",\"module\":{},\"function\":{},\"cpu_ns\":{},\"wait_ns\":{}}}",
            json_string(module),
            json_string(function),
            json_ns(*cpu),
            json_ns(*wait),
        ),
    }
}

/// Accumulates wait time separately from total elapsed time, so a caller can
/// report CPU as `elapsed - wait`.
///
/// "CPU" here means "not waiting on a host call". Under ADR 0003 Phase 1,
/// with sequential task execution and one task running at a time, this is
/// not yet a concurrency measurement: it separates a single execution's own
/// compute time from the time it spent blocked on a host call, but it cannot
/// show CPU work on one task overlapping wait on another, because nothing
/// overlaps yet. That distinction becomes a concurrency measurement only
/// once Phase 2 replaces sequential execution with real interleaving.
pub struct Timing {
    started_at: Instant,
    wait: Duration,
}

impl Timing {
    /// Starts timing now.
    pub fn start() -> Self {
        Timing {
            started_at: Instant::now(),
            wait: Duration::ZERO,
        }
    }

    /// Records `wait` as time spent waiting on a host call.
    pub fn add_wait(&mut self, wait: Duration) {
        self.wait += wait;
    }

    /// Total wait time recorded so far.
    pub fn wait(&self) -> Duration {
        self.wait
    }

    /// Total time elapsed since [`Timing::start`].
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// `elapsed() - wait()`: time spent doing anything other than waiting on
    /// a host call.
    pub fn cpu(&self) -> Duration {
        self.elapsed().saturating_sub(self.wait)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Buffer(Vec<u8>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn record_one(event: TraceEvent) -> String {
        let mut sink = JsonlSink::new(Buffer(Vec::new()));
        sink.record(event);
        let text = String::from_utf8(sink.writer.0).unwrap();
        assert!(text.ends_with('\n'), "line should end with a newline");
        text.trim_end_matches('\n').to_string()
    }

    #[test]
    fn task_spawned_with_a_parent() {
        assert_eq!(
            record_one(TraceEvent::TaskSpawned {
                id: 2,
                parent: Some(1),
                scope: "worker".to_string(),
            }),
            r#"{"event":"task_spawned","id":2,"parent":1,"scope":"worker"}"#
        );
    }

    #[test]
    fn task_spawned_without_a_parent() {
        assert_eq!(
            record_one(TraceEvent::TaskSpawned {
                id: 1,
                parent: None,
                scope: "main".to_string(),
            }),
            r#"{"event":"task_spawned","id":1,"parent":null,"scope":"main"}"#
        );
    }

    #[test]
    fn task_completed() {
        assert_eq!(
            record_one(TraceEvent::TaskCompleted {
                id: 1,
                cpu: Duration::from_micros(2),
            }),
            r#"{"event":"task_completed","id":1,"cpu_ns":2000}"#
        );
    }

    #[test]
    fn task_cancelled() {
        assert_eq!(
            record_one(TraceEvent::TaskCancelled { id: 3 }),
            r#"{"event":"task_cancelled","id":3}"#
        );
    }

    #[test]
    fn host_call_granted() {
        assert_eq!(
            record_one(TraceEvent::HostCall {
                module: "console".to_string(),
                op: "println".to_string(),
                capability: "console".to_string(),
                wait: Duration::from_nanos(900),
                granted: true,
            }),
            r#"{"event":"host_call","module":"console","op":"println","capability":"console","wait_ns":900,"granted":true}"#
        );
    }

    #[test]
    fn host_call_denied() {
        assert_eq!(
            record_one(TraceEvent::HostCall {
                module: "network".to_string(),
                op: "fetch".to_string(),
                capability: "network".to_string(),
                wait: Duration::ZERO,
                granted: false,
            }),
            r#"{"event":"host_call","module":"network","op":"fetch","capability":"network","wait_ns":0,"granted":false}"#
        );
    }

    #[test]
    fn entry_enter() {
        assert_eq!(
            record_one(TraceEvent::EntryEnter {
                module: "hello".to_string(),
                function: "main".to_string(),
            }),
            r#"{"event":"entry_enter","module":"hello","function":"main"}"#
        );
    }

    #[test]
    fn entry_exit() {
        assert_eq!(
            record_one(TraceEvent::EntryExit {
                module: "hello".to_string(),
                function: "main".to_string(),
                cpu: Duration::from_nanos(1200),
                wait: Duration::from_nanos(300),
            }),
            r#"{"event":"entry_exit","module":"hello","function":"main","cpu_ns":1200,"wait_ns":300}"#
        );
    }

    #[test]
    fn strings_needing_escaping_are_escaped() {
        assert_eq!(
            record_one(TraceEvent::EntryEnter {
                module: "weird\"name\\with\nnewline\tand\rcontrol".to_string(),
                function: "f".to_string(),
            }),
            r#"{"event":"entry_enter","module":"weird\"name\\with\nnewline\tand\rcontrol","function":"f"}"#
        );
    }

    #[test]
    fn control_character_outside_the_named_escapes_uses_a_unicode_escape() {
        assert_eq!(
            record_one(TraceEvent::EntryEnter {
                module: "bell\u{7}".to_string(),
                function: "f".to_string(),
            }),
            "{\"event\":\"entry_enter\",\"module\":\"bell\\u0007\",\"function\":\"f\"}"
        );
    }

    #[test]
    fn null_sink_records_nothing_observable() {
        let mut sink = NullSink;
        sink.record(TraceEvent::TaskCancelled { id: 1 });
        // No assertion beyond "does not panic": NullSink has no observable
        // state.
    }

    #[test]
    fn timing_reports_cpu_as_elapsed_minus_wait() {
        let mut timing = Timing::start();
        std::thread::sleep(Duration::from_millis(5));
        timing.add_wait(Duration::from_millis(2));
        assert_eq!(timing.wait(), Duration::from_millis(2));
        assert!(timing.elapsed() >= Duration::from_millis(5));
        // `elapsed()` (and so `cpu()`) advances every time it is called, so
        // assert the relationship each captures rather than comparing two
        // separate calls for equality.
        assert!(timing.cpu() >= Duration::from_millis(3));
        assert!(timing.cpu() <= timing.elapsed());
    }
}
