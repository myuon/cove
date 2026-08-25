//! Runtime observability.
//!
//! ADR 0001 asks the runtime to trace host calls, capability use, and task
//! lifecycle "without language-specific application hooks" — a program does
//! not opt in to being traced, and it cannot opt out of being traceable. This
//! module defines the event shape and where events go; [`crate::host`] and
//! (in a later pass) the interpreter are the only places that produce them.
//!
//! A trace is also the input to `cove replay`, which reproduces a run's Host
//! API interactions without calling a host. That is why [`TraceEvent::HostCall`]
//! carries the call's arguments and its result and not only its shape: a
//! trace that says a call happened is enough to inspect a run, and not enough
//! to reproduce one.
//!
//! # JSON schema
//!
//! [`JsonlSink`] writes one JSON object per line, and every `Duration` field
//! is rendered as an integer count of nanoseconds under a key ending in
//! `_ns`. The first line is a header declaring [`TRACE_FORMAT_VERSION`], so a
//! reader that does not know the version can reject the trace rather than
//! misread it. These keys are a stable, documented interface — a trace format
//! that changes silently breaks whatever reads it:
//!
//! ```text
//! {"event":"trace_header","version":<u32>,"values":"full"|"redacted","entry":<string>,"args":[<string>...]}
//! {"event":"task_spawned","id":<u64>,"parent":<u64|null>,"scope":<string>}
//! {"event":"task_completed","id":<u64>,"cpu_ns":<u64>}
//! {"event":"task_cancelled","id":<u64>}
//! {"event":"host_call","module":<string>,"op":<string>,"capability":<string>,"wait_ns":<u64>,"granted":<bool>,"args":[<value>...],"outcome":<outcome>|null}
//! {"event":"entry_enter","module":<string>,"function":<string>}
//! {"event":"entry_exit","module":<string>,"function":<string>,"cpu_ns":<u64>,"wait_ns":<u64>}
//! {"event":"heap_collected","task":<u64|null>,"allocated":<u64>,"freed":<u64>,"live_objects":<u64>,"live_bytes":<u64>,"pause_ns":<u64>}
//! {"event":"heap_summary","allocated":<u64>,"allocated_bytes":<u64>,"collections":<u64>,"live_bytes":<u64>,"peak_bytes":<u64>,"pause_ns":<u64>}
//! ```
//!
//! An `<outcome>` is `null` for a call that never reached the host, and
//! otherwise one of:
//!
//! ```text
//! {"kind":"value","value":<value>}
//! {"kind":"error","message":<string>}
//! {"kind":"not_recordable"}
//! ```
//!
//! A `<value>` is a tagged encoding of one [`Value`], covering the shapes
//! that cross the Host API boundary:
//!
//! ```text
//! {"type":"unit"}
//! {"type":"bool","value":<bool>}
//! {"type":"int","value":<i64>}
//! {"type":"float","value":<number>}
//! {"type":"duration","ns":<i64>}
//! {"type":"string","value":<string>}
//! {"type":"array","items":[<value>...]}
//! {"type":"enum","name":<string>,"case":<string>,"payload":[<value>...]}
//! {"type":"struct","name":<string>,"fields":[{"name":<string>,"value":<value>}...]}
//! {"type":"redacted","of":<string>}
//! {"type":"opaque","of":<string>,"shown":<string>}
//! ```
//!
//! `redacted` is what [`ValueCapture::Redacted`] writes in place of every
//! recorded value; `opaque` is what a value the encoding cannot represent —
//! a vector, a closure, a task handle — leaves behind. Both are readable and
//! neither can be replayed, which is exactly the distinction `cove replay`
//! reports.
//!
//! # What an event may carry
//!
//! An event is produced by whichever task made the call and written by the
//! one sink the run shares, so every event crosses a thread boundary. What
//! may cross one is what the Language Card's task-safety rule allows, which
//! `cove_runtime::task::Transfer` both decides and carries — so that is the
//! form a [`RecordedValue`] keeps a value in. A value that may not cross
//! keeps instead what a trace could have said about it anyway: what it was
//! and what it printed as, which is the `opaque` the format already writes
//! for a vector. The two features agree by construction: a value a task
//! could not have carried is a value a replay could not have reproduced.

use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::task::Transfer;
use crate::value::Value;

/// The version of the JSONL trace format this build writes, and the only one
/// it reads.
pub const TRACE_FORMAT_VERSION: u32 = 1;

/// How much of a host call's arguments and results a trace records.
///
/// A trace is a file a human may read and may share, and `env.get`,
/// `files.read`, and `documents.read` all answer with whatever the host holds
/// — which may be a secret. [`ValueCapture::Full`] is the default because a
/// trace that does not carry values cannot be replayed, and replay is the
/// reason the values are recorded at all; [`ValueCapture::Redacted`] is the
/// form to share, and it is honest about what it dropped rather than
/// pretending the call never happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueCapture {
    /// Record every argument and result in full.
    Full,
    /// Record each value's type and nothing else.
    Redacted,
}

impl ValueCapture {
    /// The name this mode is written under in a trace header, and the name
    /// `cove run --trace-values` accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            ValueCapture::Full => "full",
            ValueCapture::Redacted => "redacted",
        }
    }

    /// Parses the name [`ValueCapture::as_str`] produces.
    pub fn parse(text: &str) -> Option<ValueCapture> {
        match text {
            "full" => Some(ValueCapture::Full),
            "redacted" => Some(ValueCapture::Redacted),
            _ => None,
        }
    }
}

/// What a trace declares about itself before its first event.
///
/// The entry and its arguments are here because a replay needs them and no
/// event carries them: `cove replay` starts the same entry with the same
/// arguments, and refuses a trace recorded from a different one.
#[derive(Clone, Debug)]
pub struct TraceHeader {
    /// How much of each host call's values the trace carries.
    pub values: ValueCapture,
    /// The qualified entry function the run started, such as
    /// `restricted.main`.
    pub entry: String,
    /// The process arguments the entry was given.
    pub args: Vec<String>,
}

/// One value as a trace records it.
///
/// A trace event crosses from the task that produced it to the sink that
/// writes it, so the values it carries are exactly the values that may cross
/// a task boundary: a [`Transfer`], which is what the task-safety rule
/// produces. Everything else is kept as the `opaque` marker the format
/// already writes for it.
#[derive(Clone, Debug)]
pub enum RecordedValue {
    /// A value the trace carries whole.
    Carried(Transfer),
    /// A value no task boundary may carry — a vector, a task, a task scope —
    /// kept as the type it had and the text it printed as.
    Opaque {
        /// The type name, which is also all a redacted trace records.
        of: String,
        /// What the value printed as.
        shown: String,
    },
}

impl RecordedValue {
    /// Records `value`: whole when it may cross a boundary, and as what it
    /// was when it may not.
    pub fn of(value: &Value) -> RecordedValue {
        match Transfer::of(value) {
            Ok(transfer) => RecordedValue::Carried(transfer),
            Err(_) => RecordedValue::Opaque {
                of: value.type_name(),
                shown: value.to_string(),
            },
        }
    }
}

/// What a host call produced, when the trace records it.
#[derive(Clone, Debug)]
pub enum HostOutcome {
    /// The host answered with a value. A Cove `Err(...)` is a value like any
    /// other and arrives here, not in [`HostOutcome::Error`].
    Value(RecordedValue),
    /// The host refused the call with a runtime error, which is not an
    /// ordinary Cove failure.
    Error(String),
    /// The operation's schema declares it not recordable, so the call
    /// dispatched but its result was deliberately not written down.
    ///
    /// `process.exit` is the shipped example: handing its result back on a
    /// replay would keep running a program that had ended.
    NotRecordable,
}

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
    ///
    /// `args` are the arguments the program passed, and `outcome` is what the
    /// host answered — `None` for a call that never reached a host, so there
    /// was nothing to answer. Together they are what makes the call
    /// reproducible.
    HostCall {
        module: String,
        op: String,
        capability: String,
        wait: Duration,
        granted: bool,
        args: Vec<RecordedValue>,
        outcome: Option<HostOutcome>,
    },
    /// One task's heap was collected.
    ///
    /// ADR 0001 asks a trace to make allocation and memory pressure visible,
    /// and ADR 0011 makes this the event that does it. Allocation is reported
    /// as the count since the previous collection rather than as one event per
    /// object: an event per allocation would be most of the trace, and would
    /// tell a reader less about pressure than the pair of numbers that bracket
    /// it — what was allocated, and what survived.
    ///
    /// A heap belongs to one task, so this event says whose it was, and two
    /// tasks collecting at the same time produce two independent events.
    HeapCollected {
        /// The task whose heap this was, or `None` for the entry's own.
        task: Option<u64>,
        /// Objects allocated since the previous collection.
        allocated: u64,
        /// Objects this collection reclaimed.
        freed: u64,
        /// Objects live after it.
        live_objects: u64,
        /// Bytes live after it.
        live_bytes: u64,
        /// How long the task was stopped.
        pause: Duration,
    },
    /// What every heap in the run did, recorded once as the run ends.
    HeapSummary {
        /// Collectable objects allocated over the whole run, by every task.
        allocated: u64,
        /// Bytes those allocations asked for.
        allocated_bytes: u64,
        /// How many collections ran, over every task.
        collections: u64,
        /// Bytes live when the run ended. Every heap is swept once more as it
        /// is retired, so this is what the entry was still holding after its
        /// own last sweep — usually nothing.
        live_bytes: u64,
        /// The largest live set any one collection measured.
        peak_bytes: u64,
        /// Total time tasks were stopped for collection, summed over threads,
        /// so a run with four tasks collecting at once can report more pause
        /// than it took wall-clock time.
        pause: Duration,
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
///
/// A sink records through a shared reference and is `Send + Sync` because
/// every task thread traces into the same one: ADR 0008 runs each spawned
/// task on its own thread, and a trace that each thread wrote to separately
/// would not be one trace. A sink that needs mutable state of its own
/// synchronizes it, which is also what keeps two threads from interleaving
/// halves of a line.
pub trait TraceSink: Send + Sync {
    /// Records one event. Must not panic: a broken trace sink should degrade
    /// the trace, not the program being traced.
    fn record(&self, event: TraceEvent);

    /// Whether anything will read what is recorded.
    ///
    /// Describing a host call's values costs a copy of each of them, and a
    /// value no boundary may carry costs printing it. A run that is not being
    /// traced should not pay for a trace nobody keeps, so a sink that
    /// discards everything says so and [`crate::host::HostRegistry::call`]
    /// skips the description. The default is `true`: a sink that does
    /// something with an event needs the event to be complete.
    fn is_recording(&self) -> bool {
        true
    }
}

/// Creates the file a trace is written to, readable only by its owner where
/// the platform can say so.
///
/// A full-capture trace holds whatever the host answered with, so it is not a
/// file to leave world-readable by default. The mode applies to a file this
/// call creates; one that already exists keeps the permissions it has.
pub fn create_trace_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Discards every event. The default when a run is not being traced.
pub struct NullSink;

impl TraceSink for NullSink {
    fn record(&self, _event: TraceEvent) {}

    fn is_recording(&self) -> bool {
        false
    }
}

/// Writes one JSON object per line to `W`, flushing after every event so a
/// trace is visible as it happens rather than only at exit.
///
/// The writer is behind a lock so that a line written from a task thread is
/// written whole: concurrent tasks produce events at the same time, and half
/// a JSON object is not a trace line.
pub struct JsonlSink<W: Write + Send> {
    writer: Mutex<W>,
    values: ValueCapture,
}

impl<W: Write + Send> JsonlSink<W> {
    /// Writes trace lines to `writer`, starting with the header line that
    /// declares the format version, the value capture mode, and the entry the
    /// run started.
    pub fn new(mut writer: W, header: TraceHeader) -> Self {
        let args = header
            .args
            .iter()
            .map(|arg| json_string(arg))
            .collect::<Vec<_>>()
            .join(",");
        // A trace sink degrades silently: losing a trace line must never
        // fail the run it is observing.
        let _ = writeln!(
            writer,
            "{{\"event\":\"trace_header\",\"version\":{TRACE_FORMAT_VERSION},\"values\":{},\"entry\":{},\"args\":[{args}]}}",
            json_string(header.values.as_str()),
            json_string(&header.entry),
        );
        let _ = writer.flush();
        JsonlSink {
            writer: Mutex::new(writer),
            values: header.values,
        }
    }
}

impl<W: Write + Send> TraceSink for JsonlSink<W> {
    fn record(&self, event: TraceEvent) {
        let line = to_json_line(&event, self.values);
        // A trace sink degrades silently: losing a trace line must never fail
        // the run it is observing, and that includes a lock another thread
        // poisoned by panicking.
        let Ok(mut writer) = self.writer.lock() else {
            return;
        };
        let _ = writeln!(writer, "{line}");
        let _ = writer.flush();
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

/// Renders one [`Value`] in the trace's value encoding, honouring `capture`.
///
/// [`ValueCapture::Redacted`] replaces the whole value, not only its leaves:
/// a redaction that kept the shape of a struct would still describe the
/// secret it was hiding.
pub fn value_to_json(value: &Value, capture: ValueCapture) -> String {
    match capture {
        ValueCapture::Full => encode_value(value),
        ValueCapture::Redacted => format!(
            "{{\"type\":\"redacted\",\"of\":{}}}",
            json_string(&value.type_name())
        ),
    }
}

/// Renders one [`Value`] in full.
fn encode_value(value: &Value) -> String {
    let opaque = |value: &Value| {
        format!(
            "{{\"type\":\"opaque\",\"of\":{},\"shown\":{}}}",
            json_string(&value.type_name()),
            json_string(&value.to_string())
        )
    };
    match value {
        Value::Unit => "{\"type\":\"unit\"}".to_string(),
        Value::Bool(b) => format!("{{\"type\":\"bool\",\"value\":{b}}}"),
        Value::Int(i) => format!("{{\"type\":\"int\",\"value\":{i}}}"),
        // JSON has no way to write an infinity or a NaN, so a float that is
        // not finite is recorded as what it printed rather than as a number
        // no reader could parse back.
        Value::Float(x) if x.is_finite() => format!("{{\"type\":\"float\",\"value\":{x:?}}}"),
        Value::Duration(ns) => format!("{{\"type\":\"duration\",\"ns\":{ns}}}"),
        Value::Str(s) => format!("{{\"type\":\"string\",\"value\":{}}}", json_string(s)),
        Value::Array(items) => {
            let items = items.iter().map(encode_value).collect::<Vec<_>>().join(",");
            format!("{{\"type\":\"array\",\"items\":[{items}]}}")
        }
        Value::Enum(value) => {
            let payload = value
                .payload
                .iter()
                .map(encode_value)
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{{\"type\":\"enum\",\"name\":{},\"case\":{},\"payload\":[{payload}]}}",
                json_string(&value.type_name),
                json_string(&value.case)
            )
        }
        Value::Struct(value) => {
            let fields = value
                .fields
                .iter()
                .map(|(name, field)| {
                    format!(
                        "{{\"name\":{},\"value\":{}}}",
                        json_string(name),
                        encode_value(field)
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{{\"type\":\"struct\",\"name\":{},\"fields\":[{fields}]}}",
                json_string(&value.type_name)
            )
        }
        other => opaque(other),
    }
}

/// Renders one [`RecordedValue`], honouring `capture`.
///
/// A carried value is rebuilt before it is written: that is the far side of
/// the boundary the event crossed, and rebuilding it is the same copy the
/// rule already demands. What it produces is one [`Value`] again, so the
/// encoding below is the one encoding, whichever side of a boundary a value
/// reached it from.
fn recorded_to_json(recorded: &RecordedValue, capture: ValueCapture) -> String {
    match recorded {
        RecordedValue::Carried(transfer) => value_to_json(&transfer.clone().into_value(), capture),
        RecordedValue::Opaque { of, shown } => match capture {
            ValueCapture::Full => format!(
                "{{\"type\":\"opaque\",\"of\":{},\"shown\":{}}}",
                json_string(of),
                json_string(shown)
            ),
            ValueCapture::Redacted => {
                format!("{{\"type\":\"redacted\",\"of\":{}}}", json_string(of))
            }
        },
    }
}

/// Renders one [`HostOutcome`], or `null` when a call never reached a host.
fn encode_outcome(outcome: Option<&HostOutcome>, capture: ValueCapture) -> String {
    match outcome {
        None => "null".to_string(),
        Some(HostOutcome::Value(value)) => format!(
            "{{\"kind\":\"value\",\"value\":{}}}",
            recorded_to_json(value, capture)
        ),
        // A runtime error is the host refusing, not data the host holds, so
        // it stays readable even in a redacted trace.
        Some(HostOutcome::Error(message)) => format!(
            "{{\"kind\":\"error\",\"message\":{}}}",
            json_string(message)
        ),
        Some(HostOutcome::NotRecordable) => "{\"kind\":\"not_recordable\"}".to_string(),
    }
}

/// Renders one [`TraceEvent`] as the single JSON line documented on this
/// module.
fn to_json_line(event: &TraceEvent, capture: ValueCapture) -> String {
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
            args,
            outcome,
        } => {
            let args = args
                .iter()
                .map(|arg| recorded_to_json(arg, capture))
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{{\"event\":\"host_call\",\"module\":{},\"op\":{},\"capability\":{},\"wait_ns\":{},\"granted\":{granted},\"args\":[{args}],\"outcome\":{}}}",
                json_string(module),
                json_string(op),
                json_string(capability),
                json_ns(*wait),
                encode_outcome(outcome.as_ref(), capture),
            )
        }
        TraceEvent::HeapCollected {
            task,
            allocated,
            freed,
            live_objects,
            live_bytes,
            pause,
        } => {
            let task = match task {
                Some(id) => id.to_string(),
                None => "null".to_string(),
            };
            format!(
                "{{\"event\":\"heap_collected\",\"task\":{task},\"allocated\":{allocated},\"freed\":{freed},\"live_objects\":{live_objects},\"live_bytes\":{live_bytes},\"pause_ns\":{}}}",
                json_ns(*pause)
            )
        }
        TraceEvent::HeapSummary {
            allocated,
            allocated_bytes,
            collections,
            live_bytes,
            peak_bytes,
            pause,
        } => format!(
            "{{\"event\":\"heap_summary\",\"allocated\":{allocated},\"allocated_bytes\":{allocated_bytes},\"collections\":{collections},\"live_bytes\":{live_bytes},\"peak_bytes\":{peak_bytes},\"pause_ns\":{}}}",
            json_ns(*pause)
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
/// "CPU" here means "not waiting": neither on a host call nor for a task to
/// finish. Each task thread keeps its own timing, so with ADR 0008's thread
/// per task the separation is a concurrency measurement — one task's CPU work
/// and another's wait are recorded against different contexts and can overlap
/// in wall-clock time, which is what ADR 0001 asks a trace to make
/// attributable. It is also why the two no longer sum to the run's elapsed
/// time: several tasks can be waiting at once.
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

    fn header(values: ValueCapture) -> TraceHeader {
        TraceHeader {
            values,
            entry: "hello.main".to_string(),
            args: Vec::new(),
        }
    }

    /// Records `event` and returns the lines after the header.
    fn record_with(values: ValueCapture, event: TraceEvent) -> String {
        let sink = JsonlSink::new(Buffer(Vec::new()), header(values));
        sink.record(event);
        let text = String::from_utf8(sink.writer.into_inner().unwrap().0).unwrap();
        assert!(text.ends_with('\n'), "line should end with a newline");
        let mut lines = text.trim_end_matches('\n').split('\n');
        lines.next().expect("the header line");
        lines.collect::<Vec<_>>().join("\n")
    }

    fn record_one(event: TraceEvent) -> String {
        record_with(ValueCapture::Full, event)
    }

    /// A recorded value, written the way a host call's argument arrives:
    /// as the value itself.
    fn recorded(value: Value) -> RecordedValue {
        RecordedValue::of(&value)
    }

    /// The recorded form of a host's answer.
    fn answered(value: Value) -> Option<HostOutcome> {
        Some(HostOutcome::Value(recorded(value)))
    }

    fn host_call(args: Vec<Value>, outcome: Option<HostOutcome>) -> TraceEvent {
        TraceEvent::HostCall {
            module: "documents".to_string(),
            op: "read".to_string(),
            capability: "documents".to_string(),
            wait: Duration::from_nanos(900),
            granted: true,
            args: args.into_iter().map(recorded).collect(),
            outcome,
        }
    }

    #[test]
    fn the_first_line_declares_the_version_the_mode_the_entry_and_the_arguments() {
        let sink = JsonlSink::new(
            Buffer(Vec::new()),
            TraceHeader {
                values: ValueCapture::Full,
                entry: "restricted.main".to_string(),
                args: vec!["one".to_string(), "two".to_string()],
            },
        );
        assert_eq!(
            String::from_utf8(sink.writer.into_inner().unwrap().0).unwrap(),
            "{\"event\":\"trace_header\",\"version\":1,\"values\":\"full\",\"entry\":\"restricted.main\",\"args\":[\"one\",\"two\"]}\n"
        );
    }

    #[test]
    fn the_header_names_the_redacted_mode_when_that_is_what_was_asked_for() {
        let sink = JsonlSink::new(Buffer(Vec::new()), header(ValueCapture::Redacted));
        let text = String::from_utf8(sink.writer.into_inner().unwrap().0).unwrap();
        assert!(text.contains("\"values\":\"redacted\""), "{text}");
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
    fn a_granted_call_records_its_arguments_and_its_result() {
        assert_eq!(
            record_one(host_call(
                vec![Value::Str("input".into())],
                answered(Value::ok(Value::Str("text".into()))),
            )),
            r#"{"event":"host_call","module":"documents","op":"read","capability":"documents","wait_ns":900,"granted":true,"args":[{"type":"string","value":"input"}],"outcome":{"kind":"value","value":{"type":"enum","name":"Result","case":"Ok","payload":[{"type":"string","value":"text"}]}}}"#
        );
    }

    #[test]
    fn a_call_that_never_reached_a_host_records_no_outcome() {
        assert_eq!(
            record_one(TraceEvent::HostCall {
                module: "network".to_string(),
                op: "fetch".to_string(),
                capability: "network".to_string(),
                wait: Duration::ZERO,
                granted: false,
                args: vec![recorded(Value::Str("https://example.test".into()))],
                outcome: None,
            }),
            r#"{"event":"host_call","module":"network","op":"fetch","capability":"network","wait_ns":0,"granted":false,"args":[{"type":"string","value":"https://example.test"}],"outcome":null}"#
        );
    }

    /// The `recordable` flag decides, and `process.exit` is the operation it
    /// decides against: a replay that handed its result back would keep
    /// running a program that had ended.
    #[test]
    fn an_operation_that_is_not_recordable_records_that_instead_of_a_result() {
        assert_eq!(
            record_one(host_call(Vec::new(), Some(HostOutcome::NotRecordable))),
            r#"{"event":"host_call","module":"documents","op":"read","capability":"documents","wait_ns":900,"granted":true,"args":[],"outcome":{"kind":"not_recordable"}}"#
        );
    }

    #[test]
    fn a_host_that_refused_records_the_runtime_error_it_refused_with() {
        assert_eq!(
            record_one(host_call(
                Vec::new(),
                Some(HostOutcome::Error("no such host".to_string())),
            )),
            r#"{"event":"host_call","module":"documents","op":"read","capability":"documents","wait_ns":900,"granted":true,"args":[],"outcome":{"kind":"error","message":"no such host"}}"#
        );
    }

    /// A redacted trace is the one to share: it says a call happened, with
    /// what kinds of values, and nothing about their contents.
    #[test]
    fn redacted_capture_replaces_every_argument_and_result_with_its_type() {
        assert_eq!(
            record_with(
                ValueCapture::Redacted,
                host_call(
                    vec![Value::Str("PASSWORD".into())],
                    answered(Value::some(Value::Str("hunter2".into()))),
                )
            ),
            r#"{"event":"host_call","module":"documents","op":"read","capability":"documents","wait_ns":900,"granted":true,"args":[{"type":"redacted","of":"String"}],"outcome":{"kind":"value","value":{"type":"redacted","of":"Option"}}}"#
        );
    }

    /// Redaction replaces the whole value rather than its leaves: a redacted
    /// struct that kept its shape would still describe the secret.
    #[test]
    fn redaction_does_not_leave_the_shape_of_what_it_hid() {
        let text = record_with(
            ValueCapture::Redacted,
            host_call(
                vec![Value::Struct(Box::new(crate::value::StructValue {
                    type_name: "Credentials".into(),
                    fields: vec![("token".into(), Value::Str("hunter2".into()))],
                }))],
                None,
            ),
        );
        assert!(
            text.contains(r#""args":[{"type":"redacted","of":"Credentials"}]"#),
            "{text}"
        );
        assert!(!text.contains("token"), "{text}");
        assert!(!text.contains("hunter2"), "{text}");
    }

    #[test]
    fn every_value_shape_that_crosses_the_boundary_has_an_encoding() {
        let encoded = |value: Value| encode_value(&value);
        assert_eq!(encoded(Value::Unit), r#"{"type":"unit"}"#);
        assert_eq!(
            encoded(Value::Bool(true)),
            r#"{"type":"bool","value":true}"#
        );
        assert_eq!(encoded(Value::Int(-7)), r#"{"type":"int","value":-7}"#);
        assert_eq!(
            encoded(Value::Float(1.5)),
            r#"{"type":"float","value":1.5}"#
        );
        assert_eq!(
            encoded(Value::Duration(1_000)),
            r#"{"type":"duration","ns":1000}"#
        );
        assert_eq!(
            encoded(Value::Str("hi".into())),
            r#"{"type":"string","value":"hi"}"#
        );
        assert_eq!(
            encoded(Value::Array(vec![Value::Int(1), Value::Int(2)].into())),
            r#"{"type":"array","items":[{"type":"int","value":1},{"type":"int","value":2}]}"#
        );
        assert_eq!(
            encoded(Value::none()),
            r#"{"type":"enum","name":"Option","case":"None","payload":[]}"#
        );
        assert_eq!(
            encoded(Value::error("broken")),
            r#"{"type":"struct","name":"Error","fields":[{"name":"message","value":{"type":"string","value":"broken"}}]}"#
        );
    }

    /// A value that may not cross a task boundary is recorded as what it was
    /// and what it printed as, which is the same `opaque` the encoding
    /// already writes for it — so a trace says the same thing whether the
    /// call was made by the entry or by a task.
    #[test]
    fn a_value_that_may_not_cross_a_boundary_is_recorded_as_opaque() {
        let vector = Value::Vector(crate::value::VectorStorage::new(vec![Value::Int(1)]));
        let recorded = RecordedValue::of(&vector);
        assert!(
            matches!(&recorded, RecordedValue::Opaque { of, shown } if of == "Vector" && shown == "[1]")
        );
        assert_eq!(
            recorded_to_json(&recorded, ValueCapture::Full),
            r#"{"type":"opaque","of":"Vector","shown":"[1]"}"#
        );
        assert_eq!(
            recorded_to_json(&recorded, ValueCapture::Redacted),
            r#"{"type":"redacted","of":"Vector"}"#
        );
    }

    /// A value that may cross is carried whole, and rebuilding it on the
    /// writing side produces the encoding it would have had all along.
    #[test]
    fn a_value_that_may_cross_a_boundary_is_carried_whole() {
        let recorded = RecordedValue::of(&Value::ok(Value::Str("text".into())));
        assert!(matches!(recorded, RecordedValue::Carried(_)));
        assert_eq!(
            recorded_to_json(&recorded, ValueCapture::Full),
            r#"{"type":"enum","name":"Result","case":"Ok","payload":[{"type":"string","value":"text"}]}"#
        );
    }

    /// A value the encoding cannot represent leaves a readable marker behind
    /// rather than a number no reader could parse or a silently dropped
    /// argument.
    #[test]
    fn a_value_the_encoding_cannot_represent_is_recorded_as_opaque() {
        assert_eq!(
            encode_value(&Value::Vector(crate::value::VectorStorage::new(vec![
                Value::Int(1)
            ]))),
            r#"{"type":"opaque","of":"Vector","shown":"[1]"}"#
        );
        assert_eq!(
            encode_value(&Value::Float(f64::INFINITY)),
            r#"{"type":"opaque","of":"Float","shown":"inf"}"#
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
    fn heap_collected() {
        assert_eq!(
            record_one(TraceEvent::HeapCollected {
                task: Some(2),
                allocated: 64,
                freed: 60,
                live_objects: 4,
                live_bytes: 512,
                pause: Duration::from_micros(9),
            }),
            r#"{"event":"heap_collected","task":2,"allocated":64,"freed":60,"live_objects":4,"live_bytes":512,"pause_ns":9000}"#
        );
    }

    #[test]
    fn heap_collected_for_the_entry_names_no_task() {
        assert_eq!(
            record_one(TraceEvent::HeapCollected {
                task: None,
                allocated: 1,
                freed: 0,
                live_objects: 1,
                live_bytes: 8,
                pause: Duration::ZERO,
            }),
            r#"{"event":"heap_collected","task":null,"allocated":1,"freed":0,"live_objects":1,"live_bytes":8,"pause_ns":0}"#
        );
    }

    #[test]
    fn heap_summary() {
        assert_eq!(
            record_one(TraceEvent::HeapSummary {
                allocated: 128,
                allocated_bytes: 4096,
                collections: 2,
                live_bytes: 96,
                peak_bytes: 1024,
                pause: Duration::from_micros(31),
            }),
            r#"{"event":"heap_summary","allocated":128,"allocated_bytes":4096,"collections":2,"live_bytes":96,"peak_bytes":1024,"pause_ns":31000}"#
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
    fn value_capture_names_round_trip() {
        for mode in [ValueCapture::Full, ValueCapture::Redacted] {
            assert_eq!(ValueCapture::parse(mode.as_str()), Some(mode));
        }
        assert_eq!(ValueCapture::parse("some"), None);
    }

    #[test]
    fn null_sink_records_nothing_observable() {
        let sink = NullSink;
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
