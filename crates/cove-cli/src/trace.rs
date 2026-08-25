//! Reading a recorded trace back: `cove trace`.
//!
//! [`cove_runtime::trace`] defines the JSONL format and writes it; this
//! module is the only place that reads it. The two halves are deliberately
//! separate crates' worth of concern — the runtime must be able to record
//! without knowing anything about a reader — so the format's documentation
//! lives with the writer and this module's job is to refuse anything it does
//! not recognise rather than guess.
//!
//! What a summary reports is limited by what the events carry. ADR 0001 asks
//! traces to distinguish CPU execution, I/O wait, allocation and memory
//! pressure, host calls and capability use, task lifecycle, and cache hits
//! and misses. Five of those have events today and one does not, and
//! [`render_summary`] says so rather than printing a zero that reads like a
//! measurement. Allocation and memory pressure joined the four when ADR 0011
//! added a collector for them to be measured by, and a host call's wait became
//! attributable to the task that waited when the call gained that task's id.
//!
//! Two questions a summary can now answer that it could not: whose each host
//! call was, which is what the `by task` block and `--task` report, and how
//! the run ended, which is the one thing no event carried at all.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use cove_runtime::schema::{Effect, OperationSchema};
use cove_runtime::{
    value_to_json, RunOutcome, Value, ValueCapture, ENTRY_TASK, TRACE_FORMAT_VERSION,
};

use crate::json::{self, Json};
use crate::CliError;

/// A trace read back from a file.
pub(crate) struct Trace {
    /// What the trace's first line declares about itself.
    pub(crate) header: Header,
    /// Every event after the header, in the order it was recorded.
    pub(crate) events: Vec<Event>,
}

/// What a trace's first line declares about itself.
pub(crate) struct Header {
    /// The format version. A reader that does not know it rejects the trace.
    pub(crate) version: u64,
    /// How much of each host call's values the trace carries.
    pub(crate) values: ValueCapture,
    /// The qualified entry the run started.
    pub(crate) entry: String,
    /// The process arguments the entry was given.
    pub(crate) args: Vec<String>,
}

/// One event read back from a trace.
pub(crate) enum Event {
    TaskSpawned {
        id: u64,
        parent: Option<u64>,
        scope: String,
    },
    TaskCompleted {
        id: u64,
        cpu: Duration,
    },
    TaskCancelled {
        id: u64,
    },
    HostCall(HostCall),
    EntryEnter {
        module: String,
        function: String,
    },
    EntryExit {
        module: String,
        function: String,
        cpu: Duration,
        wait: Duration,
    },
    HeapCollected {
        task: u64,
        allocated: u64,
        freed: u64,
        live_objects: u64,
        live_bytes: u64,
        pause: Duration,
    },
    HeapSummary {
        allocated: u64,
        allocated_bytes: u64,
        collections: u64,
        live_bytes: u64,
        peak_bytes: u64,
        pause: Duration,
    },
    /// How the run ended, which is the last line of every trace.
    RunEnded {
        outcome: RunOutcome,
        message: Option<String>,
    },
}

/// One recorded Host API call.
pub(crate) struct HostCall {
    /// The task that made the call: a spawned task's id, or [`ENTRY_TASK`]
    /// for a call the entry made itself.
    pub(crate) task: u64,
    pub(crate) module: String,
    pub(crate) op: String,
    pub(crate) capability: String,
    pub(crate) wait: Duration,
    /// Whether the call reached a host, or was refused before it could.
    pub(crate) granted: bool,
    pub(crate) args: Vec<Recorded>,
    /// What the host answered, or `None` for a call that never reached one.
    pub(crate) outcome: Option<Outcome>,
}

impl HostCall {
    /// `module.op`, the way source names the call.
    pub(crate) fn qualified(&self) -> String {
        format!("{}.{}", self.module, self.op)
    }

    /// The call as it would be written in source, with its arguments.
    pub(crate) fn shown(&self) -> String {
        format!("{}({})", self.qualified(), joined(&shown_parts(&self.args)))
    }
}

/// What a host answered a recorded call with.
pub(crate) enum Outcome {
    /// A value, which is what a replay hands back.
    Value(Recorded),
    /// A runtime error, which a replay reproduces as the same error.
    Error(String),
    /// Nothing, because the operation's schema declares it not recordable.
    NotRecordable,
}

/// One value a trace recorded.
pub(crate) struct Recorded {
    /// The value itself, or why the trace does not carry it.
    pub(crate) value: Result<Value, Missing>,
    /// How the value reads in a report. A value that cannot be reconstructed
    /// still has something to show: which part of it is missing, and why.
    pub(crate) shown: String,
}

impl Recorded {
    /// The value's encoding, for comparing a replayed call against a recorded
    /// one. `None` when the trace does not carry the value.
    pub(crate) fn canonical(&self) -> Option<String> {
        self.value
            .as_ref()
            .ok()
            .map(|value| value_to_json(value, ValueCapture::Full))
    }
}

/// Why a trace does not carry a value it recorded a place for.
#[derive(Clone, Debug)]
pub(crate) enum Missing {
    /// `cove run --trace-values redacted` recorded the type and nothing else.
    Redacted(String),
    /// The trace's value encoding cannot represent this type, so the trace
    /// carries only how it printed.
    Opaque(String),
}

impl Missing {
    /// Why the value is not there, phrased for a diagnostic.
    pub(crate) fn reason(&self) -> String {
        match self {
            Missing::Redacted(of) => format!(
                "the trace was recorded with `--trace-values redacted`, so it carries the type `{of}` and no value"
            ),
            Missing::Opaque(of) => format!(
                "a `{of}` is not a value the trace format can record, so the trace carries only how it printed"
            ),
        }
    }
}

impl Trace {
    /// Reads and validates the trace at `path`.
    pub(crate) fn read(path: &Path) -> Result<Trace, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
        Trace::read_str(&text).map_err(|e| format!("`{}`{e}", path.display()))
    }

    /// Reads a trace from the text of a JSONL file.
    ///
    /// Every error names the line it came from, so a trace that is almost
    /// right says which line is not.
    pub(crate) fn read_str(text: &str) -> Result<Trace, String> {
        let mut lines = text
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty());

        let Some((at, first)) = lines.next() else {
            return Err(" is empty; a trace begins with a `trace_header` line".to_string());
        };
        let header = parse_header(first).map_err(|e| format!(":{}: {e}", at + 1))?;
        if header.version != u64::from(TRACE_FORMAT_VERSION) {
            return Err(format!(
                ":{}: is version {}, and this build of `cove` reads version {TRACE_FORMAT_VERSION}",
                at + 1,
                header.version
            ));
        }

        let mut events = Vec::new();
        for (at, line) in lines {
            events.push(parse_event(line).map_err(|e| format!(":{}: {e}", at + 1))?);
        }
        Ok(Trace { header, events })
    }

    /// How the run this trace recorded ended, or `None` for a trace that
    /// carries no `run_ended` line.
    ///
    /// Every trace this build writes carries one, so `None` means a file
    /// somebody assembled by hand or truncated — which is worth reporting as
    /// such rather than reading as a run that succeeded.
    pub(crate) fn run_outcome(&self) -> Option<(RunOutcome, Option<&str>)> {
        self.events.iter().rev().find_map(|event| match event {
            Event::RunEnded { outcome, message } => Some((*outcome, message.as_deref())),
            _ => None,
        })
    }

    /// Every recorded call that reached a host, in order. These are the calls
    /// a replay answers from.
    pub(crate) fn dispatched_calls(&self) -> Vec<&HostCall> {
        self.events
            .iter()
            .filter_map(|event| match event {
                Event::HostCall(call) if call.granted => Some(call),
                _ => None,
            })
            .collect()
    }
}

/// Reads the `trace_header` line.
fn parse_header(line: &str) -> Result<Header, String> {
    let json = json::parse(line)?;
    match json.get("event").and_then(Json::as_str) {
        Some("trace_header") => {}
        Some(other) => {
            return Err(format!(
                "begins with a `{other}` event; a trace begins with a `trace_header` line"
            ))
        }
        None => return Err("begins with a line that is not a `trace_header`".to_string()),
    }
    let version = field(&json, "version")?
        .as_u64()
        .ok_or_else(|| "`version` must be a non-negative integer".to_string())?;
    // The version is read before anything else is required, so a future trace
    // is rejected for its version rather than for a field this build has
    // never heard of.
    if version != u64::from(TRACE_FORMAT_VERSION) {
        return Ok(Header {
            version,
            values: ValueCapture::Full,
            entry: String::new(),
            args: Vec::new(),
        });
    }
    let values = string_field(&json, "values")?;
    let values = ValueCapture::parse(&values)
        .ok_or_else(|| format!("`values` must be `full` or `redacted`, found `{values}`"))?;
    let entry = string_field(&json, "entry")?;
    let args = field(&json, "args")?
        .as_array()
        .ok_or_else(|| "`args` must be an array".to_string())?
        .iter()
        .map(|arg| {
            arg.as_str()
                .map(str::to_string)
                .ok_or_else(|| "`args` must be an array of strings".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Header {
        version,
        values,
        entry,
        args,
    })
}

/// Reads one event line.
fn parse_event(line: &str) -> Result<Event, String> {
    let json = json::parse(line)?;
    let event = json
        .get("event")
        .and_then(Json::as_str)
        .ok_or_else(|| "every trace line needs a string `event`".to_string())?;
    match event {
        "trace_header" => Err("a `trace_header` may only be the first line".to_string()),
        "task_spawned" => Ok(Event::TaskSpawned {
            id: u64_field(&json, "id")?,
            parent: match field(&json, "parent")? {
                Json::Null => None,
                other => Some(
                    other
                        .as_u64()
                        .ok_or_else(|| "`parent` must be an integer or null".to_string())?,
                ),
            },
            scope: string_field(&json, "scope")?,
        }),
        "task_completed" => Ok(Event::TaskCompleted {
            id: u64_field(&json, "id")?,
            cpu: nanos_field(&json, "cpu_ns")?,
        }),
        "task_cancelled" => Ok(Event::TaskCancelled {
            id: u64_field(&json, "id")?,
        }),
        "host_call" => Ok(Event::HostCall(parse_host_call(&json)?)),
        "entry_enter" => Ok(Event::EntryEnter {
            module: string_field(&json, "module")?,
            function: string_field(&json, "function")?,
        }),
        "entry_exit" => Ok(Event::EntryExit {
            module: string_field(&json, "module")?,
            function: string_field(&json, "function")?,
            cpu: nanos_field(&json, "cpu_ns")?,
            wait: nanos_field(&json, "wait_ns")?,
        }),
        "heap_collected" => Ok(Event::HeapCollected {
            task: u64_field(&json, "task")?,
            allocated: u64_field(&json, "allocated")?,
            freed: u64_field(&json, "freed")?,
            live_objects: u64_field(&json, "live_objects")?,
            live_bytes: u64_field(&json, "live_bytes")?,
            pause: nanos_field(&json, "pause_ns")?,
        }),
        "heap_summary" => Ok(Event::HeapSummary {
            allocated: u64_field(&json, "allocated")?,
            allocated_bytes: u64_field(&json, "allocated_bytes")?,
            collections: u64_field(&json, "collections")?,
            live_bytes: u64_field(&json, "live_bytes")?,
            peak_bytes: u64_field(&json, "peak_bytes")?,
            pause: nanos_field(&json, "pause_ns")?,
        }),
        "run_ended" => {
            let name = string_field(&json, "outcome")?;
            let outcome = RunOutcome::parse(&name)
                .ok_or_else(|| format!("unknown run outcome `{name}`"))?;
            Ok(Event::RunEnded {
                outcome,
                message: match field(&json, "message")? {
                    Json::Null => None,
                    other => Some(
                        other
                            .as_str()
                            .map(str::to_string)
                            .ok_or_else(|| "`message` must be a string or null".to_string())?,
                    ),
                },
            })
        }
        other => Err(format!(
            "unknown event `{other}`; this build of `cove` reads trace format version {TRACE_FORMAT_VERSION}"
        )),
    }
}

/// Reads a `host_call` line's own fields.
fn parse_host_call(json: &Json) -> Result<HostCall, String> {
    let args = field(json, "args")?
        .as_array()
        .ok_or_else(|| "`args` must be an array".to_string())?
        .iter()
        .map(decode_value)
        .collect::<Result<Vec<_>, _>>()?;
    let outcome = match field(json, "outcome")? {
        Json::Null => None,
        outcome => Some(match outcome.get("kind").and_then(Json::as_str) {
            Some("value") => Outcome::Value(decode_value(field(outcome, "value")?)?),
            Some("error") => Outcome::Error(string_field(outcome, "message")?),
            Some("not_recordable") => Outcome::NotRecordable,
            Some(other) => return Err(format!("unknown outcome kind `{other}`")),
            None => return Err("an `outcome` needs a string `kind`".to_string()),
        }),
    };
    Ok(HostCall {
        task: u64_field(json, "task")?,
        module: string_field(json, "module")?,
        op: string_field(json, "op")?,
        capability: string_field(json, "capability")?,
        wait: nanos_field(json, "wait_ns")?,
        granted: field(json, "granted")?
            .as_bool()
            .ok_or_else(|| "`granted` must be a boolean".to_string())?,
        args,
        outcome,
    })
}

/// Reads one encoded value, keeping both what it is and how it reads.
///
/// A container whose parts are all present becomes a value; one with a
/// redacted or unrepresentable part carries that part's reason upward, so a
/// replay can say which value it does not have and a report can still show
/// the shape around it.
fn decode_value(json: &Json) -> Result<Recorded, String> {
    let tag = json
        .get("type")
        .and_then(Json::as_str)
        .ok_or_else(|| "a recorded value needs a string `type`".to_string())?;
    let recorded = |value: Value, shown: String| {
        Ok(Recorded {
            value: Ok(value),
            shown,
        })
    };
    match tag {
        "unit" => recorded(Value::Unit, "()".to_string()),
        "bool" => {
            let b = field(json, "value")?
                .as_bool()
                .ok_or_else(|| "a `bool` needs a boolean `value`".to_string())?;
            recorded(Value::Bool(b), b.to_string())
        }
        "int" => {
            let i = field(json, "value")?
                .as_i64()
                .ok_or_else(|| "an `int` needs a `value` that fits in 64 bits".to_string())?;
            recorded(Value::Int(i), i.to_string())
        }
        "float" => {
            let x = field(json, "value")?
                .as_f64()
                .ok_or_else(|| "a `float` needs a numeric `value`".to_string())?;
            recorded(Value::Float(x), format!("{x:?}"))
        }
        "duration" => {
            let ns = field(json, "ns")?
                .as_i64()
                .ok_or_else(|| "a `duration` needs an integer `ns`".to_string())?;
            recorded(Value::Duration(ns), format!("{ns}ns"))
        }
        "string" => {
            let text = string_field(json, "value")?;
            let shown = quote(&text);
            recorded(Value::Str(text.into()), shown)
        }
        "array" => {
            let items = field(json, "items")?
                .as_array()
                .ok_or_else(|| "an `array` needs `items`".to_string())?
                .iter()
                .map(decode_value)
                .collect::<Result<Vec<_>, _>>()?;
            let shown = format!("[{}]", joined(&shown_parts(&items)));
            match collect(&items) {
                Ok(values) => recorded(Value::Array(values.into()), shown),
                Err(missing) => Ok(Recorded {
                    value: Err(missing),
                    shown,
                }),
            }
        }
        "enum" => {
            let name = string_field(json, "name")?;
            let case = string_field(json, "case")?;
            let payload = field(json, "payload")?
                .as_array()
                .ok_or_else(|| "an `enum` needs a `payload` array".to_string())?
                .iter()
                .map(decode_value)
                .collect::<Result<Vec<_>, _>>()?;
            let shown = show_case(&name, &case, &shown_parts(&payload));
            match collect(&payload) {
                Ok(values) => recorded(
                    Value::Enum(Box::new(cove_runtime::value::EnumValue {
                        type_name: name.into(),
                        case: case.into(),
                        payload: values,
                    })),
                    shown,
                ),
                Err(missing) => Ok(Recorded {
                    value: Err(missing),
                    shown,
                }),
            }
        }
        "struct" => {
            let name = string_field(json, "name")?;
            let mut names = Vec::new();
            let mut fields = Vec::new();
            for member in field(json, "fields")?
                .as_array()
                .ok_or_else(|| "a `struct` needs a `fields` array".to_string())?
            {
                names.push(string_field(member, "name")?);
                fields.push(decode_value(field(member, "value")?)?);
            }
            let shown = format!(
                "{name}({})",
                names
                    .iter()
                    .zip(&fields)
                    .map(|(name, value)| format!("{name}: {}", value.shown))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            match collect(&fields) {
                Ok(values) => recorded(
                    Value::Struct(Box::new(cove_runtime::value::StructValue {
                        type_name: name.into(),
                        fields: names
                            .into_iter()
                            .map(Into::into)
                            .zip(values)
                            .collect(),
                    })),
                    shown,
                ),
                Err(missing) => Ok(Recorded {
                    value: Err(missing),
                    shown,
                }),
            }
        }
        "resource" => {
            let name = string_field(json, "name")?;
            let id = field(json, "id")?
                .as_i64()
                .ok_or_else(|| "a `resource` needs an integer `id`".to_string())?;
            let (module, type_name) = name
                .rsplit_once('.')
                .ok_or_else(|| format!("a `resource` name must be qualified, found `{name}`"))?;
            let handle = cove_runtime::ResourceHandle {
                module: module.to_string(),
                type_name: type_name.to_string(),
                id: id as u64,
                // A handle that reached a trace crossed the boundary that
                // records values, and only a task-safe one can: a resource
                // its schema keeps to one task is recorded as opaque instead.
                task_safe: true,
            };
            let shown = format!("<{handle}>");
            recorded(Value::Resource(std::sync::Arc::new(handle)), shown)
        }
        "redacted" => {
            let of = string_field(json, "of")?;
            Ok(Recorded {
                shown: format!("<redacted {of}>"),
                value: Err(Missing::Redacted(of)),
            })
        }
        "opaque" => {
            let of = string_field(json, "of")?;
            let shown = string_field(json, "shown")?;
            Ok(Recorded {
                shown: format!("<{of} {shown}>"),
                value: Err(Missing::Opaque(of)),
            })
        }
        other => Err(format!(
            "unknown recorded value type `{other}`; this build of `cove` reads trace format version {TRACE_FORMAT_VERSION}"
        )),
    }
}

/// How each of `parts` reads.
fn shown_parts(parts: &[Recorded]) -> Vec<String> {
    parts.iter().map(|part| part.shown.clone()).collect()
}

/// The values of `parts`, or the first reason one of them is not there.
fn collect(parts: &[Recorded]) -> Result<Vec<Value>, Missing> {
    let mut values = Vec::with_capacity(parts.len());
    for part in parts {
        match &part.value {
            Ok(value) => values.push(value.clone()),
            Err(missing) => return Err(missing.clone()),
        }
    }
    Ok(values)
}

/// `a, b`, for showing a sequence of already-rendered parts.
fn joined(parts: &[String]) -> String {
    parts.join(", ")
}

/// `Ok(x)`, `None`, or `booking.State.Held(1)`: the builtin enums are written
/// bare, exactly as source writes them.
fn show_case(name: &str, case: &str, payload: &[String]) -> String {
    let head = match name {
        "Option" | "Result" => case.to_string(),
        other => format!("{other}.{case}"),
    };
    if payload.is_empty() {
        head
    } else {
        format!("{head}({})", joined(payload))
    }
}

/// Renders one live [`Value`] the way [`decode_value`] renders a recorded
/// one, so a divergence report can put what the program asked for beside
/// what the trace recorded and have them read the same way.
pub(crate) fn show_value(value: &Value) -> String {
    match value {
        Value::Unit => "()".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(x) => format!("{x:?}"),
        Value::Duration(ns) => format!("{ns}ns"),
        Value::Str(s) => quote(s),
        Value::Array(items) => format!("[{}]", joined(&shown_all(items))),
        Value::Enum(value) => show_case(&value.type_name, &value.case, &shown_all(&value.payload)),
        Value::Struct(value) => format!(
            "{}({})",
            value.type_name,
            value
                .fields
                .iter()
                .map(|(name, field)| format!("{name}: {}", show_value(field)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => format!("<{} {other}>", other.type_name()),
    }
}

fn shown_all(values: &[Value]) -> Vec<String> {
    values.iter().map(show_value).collect()
}

/// `s` as a quoted string literal, so a report never confuses a string's
/// content with the report's own punctuation.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn field<'a>(json: &'a Json, name: &str) -> Result<&'a Json, String> {
    json.get(name)
        .ok_or_else(|| format!("no `{name}` field, in {}", json.kind()))
}

fn string_field(json: &Json, name: &str) -> Result<String, String> {
    field(json, name)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("`{name}` must be a string"))
}

fn u64_field(json: &Json, name: &str) -> Result<u64, String> {
    field(json, name)?
        .as_u64()
        .ok_or_else(|| format!("`{name}` must be a non-negative integer"))
}

fn nanos_field(json: &Json, name: &str) -> Result<Duration, String> {
    Ok(Duration::from_nanos(u64_field(json, name)?))
}

/// What the Host API schema says about the operations a trace names.
///
/// A trace records a call's module and operation, not its effect, so the
/// counts a summary reports come from the schema this build ships. An
/// operation the schema does not know is reported as unknown rather than
/// counted as harmless.
struct Schema(BTreeMap<(String, String), OperationSchema>);

impl Schema {
    fn shipped() -> Schema {
        let mut table = BTreeMap::new();
        for module in cove_runtime::shipped_schema() {
            for operation in module.operations {
                table.insert(
                    (module.name.to_string(), operation.name.to_string()),
                    *operation,
                );
            }
        }
        Schema(table)
    }

    fn get(&self, call: &HostCall) -> Option<&OperationSchema> {
        self.0.get(&(call.module.clone(), call.op.clone()))
    }
}

/// What a run's `heap_summary` event carried.
struct HeapTotals {
    allocated: u64,
    allocated_bytes: u64,
    live_bytes: u64,
    peak_bytes: u64,
    pause: Duration,
}

/// Counts of the calls one capability accounts for.
#[derive(Default)]
struct CapabilityCounts {
    dispatched: usize,
    refused: usize,
    irreversible: usize,
    wait: Duration,
}

/// Renders the summary: what the run did, and which of the distinctions ADR
/// 0001 asks for these events actually carry.
pub(crate) fn render_summary(path: &Path, trace: &Trace) -> String {
    let schema = Schema::shipped();
    let mut out = String::new();
    let _ = writeln!(out, "trace {}", path.display());
    let _ = writeln!(out, "  format     version {}", trace.header.version);
    let _ = writeln!(
        out,
        "  values     {}",
        match trace.header.values {
            ValueCapture::Full =>
                "full — arguments and results are recorded, and may include secrets",
            ValueCapture::Redacted =>
                "redacted — only each value's type is recorded, so this trace cannot be replayed",
        }
    );
    let _ = writeln!(out, "  entry      {}", trace.header.entry);
    let _ = writeln!(
        out,
        "  arguments  {}",
        if trace.header.args.is_empty() {
            "none".to_string()
        } else {
            trace
                .header
                .args
                .iter()
                .map(|arg| quote(arg))
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    let _ = writeln!(out, "  events     {}", trace.events.len());

    let mut entries = Vec::new();
    let (mut spawned, mut completed, mut cancelled) = (0usize, 0usize, 0usize);
    let mut task_cpu = Duration::ZERO;
    let mut by_capability: BTreeMap<&str, CapabilityCounts> = BTreeMap::new();
    let mut unknown = Vec::new();
    let (mut dispatched, mut refused, mut irreversible) = (0usize, 0usize, 0usize);
    let mut wait_total = Duration::ZERO;
    let (mut collections, mut collected_allocated, mut freed_objects) = (0usize, 0u64, 0u64);
    let mut collected_by: BTreeMap<u64, usize> = BTreeMap::new();
    let mut by_task: BTreeMap<u64, CapabilityCounts> = BTreeMap::new();
    let mut heap: Option<HeapTotals> = None;
    for event in &trace.events {
        match event {
            Event::TaskSpawned { .. } => spawned += 1,
            Event::TaskCompleted { cpu, .. } => {
                completed += 1;
                task_cpu += *cpu;
            }
            Event::TaskCancelled { .. } => cancelled += 1,
            Event::HeapCollected {
                task,
                allocated,
                freed,
                ..
            } => {
                collections += 1;
                collected_allocated += allocated;
                freed_objects += freed;
                *collected_by.entry(*task).or_default() += 1;
            }
            Event::HeapSummary {
                allocated,
                allocated_bytes,
                live_bytes,
                peak_bytes,
                pause,
                ..
            } => {
                heap = Some(HeapTotals {
                    allocated: *allocated,
                    allocated_bytes: *allocated_bytes,
                    live_bytes: *live_bytes,
                    peak_bytes: *peak_bytes,
                    pause: *pause,
                });
            }
            Event::EntryEnter { .. } | Event::RunEnded { .. } => {}
            Event::EntryExit {
                module,
                function,
                cpu,
                wait,
            } => entries.push((format!("{module}.{function}"), *cpu, *wait)),
            Event::HostCall(call) => {
                let counts = by_capability.entry(&call.capability).or_default();
                let whose = by_task.entry(call.task).or_default();
                counts.wait += call.wait;
                whose.wait += call.wait;
                wait_total += call.wait;
                if call.granted {
                    counts.dispatched += 1;
                    whose.dispatched += 1;
                    dispatched += 1;
                } else {
                    counts.refused += 1;
                    whose.refused += 1;
                    refused += 1;
                }
                match schema.get(call) {
                    Some(operation) => {
                        if call.granted && operation.effect == Effect::IrreversibleWrite {
                            counts.irreversible += 1;
                            whose.irreversible += 1;
                            irreversible += 1;
                        }
                    }
                    None => {
                        let name = call.qualified();
                        if !unknown.contains(&name) {
                            unknown.push(name);
                        }
                    }
                }
            }
        }
    }

    let _ = writeln!(out, "\nsummary");
    if entries.is_empty() {
        let _ = writeln!(out, "  entries      none recorded");
    } else {
        for (name, cpu, wait) in &entries {
            let _ = writeln!(
                out,
                "  entry        {name} cpu {} wait {}",
                pretty(*cpu),
                pretty(*wait)
            );
        }
    }
    let _ = writeln!(
        out,
        "  tasks        {spawned} spawned, {completed} completed, {cancelled} cancelled, cpu {}",
        pretty(task_cpu)
    );
    let _ = writeln!(
        out,
        "  host calls   {dispatched} dispatched, {refused} refused, wait {}",
        pretty(wait_total)
    );
    let _ = writeln!(
        out,
        "  irreversible {irreversible} of the {dispatched} dispatched calls cannot be taken back"
    );
    match &heap {
        Some(totals) => {
            let _ = writeln!(
                out,
                "  heap         {} object(s) allocated in {} bytes, {} live at the end, peak {}",
                totals.allocated, totals.allocated_bytes, totals.live_bytes, totals.peak_bytes
            );
            let _ = writeln!(
                out,
                "  collections  {collections} over {} heap(s), {freed_objects} object(s) reclaimed of {collected_allocated} allocated between them, pause {}",
                collected_by.len(),
                pretty(totals.pause)
            );
        }
        None => {
            let _ = writeln!(
                out,
                "  heap         no `heap_summary` event, so this trace says nothing about memory"
            );
        }
    }
    if !unknown.is_empty() {
        let _ = writeln!(
            out,
            "  unknown      {} — not in this build's Host API schema, so not classified",
            unknown.join(", ")
        );
    }
    let _ = writeln!(
        out,
        "  outcome      {}",
        match trace.run_outcome() {
            Some((outcome, Some(message))) => format!("{} — {message}", outcome.as_str()),
            Some((outcome, None)) => outcome.as_str().to_string(),
            None =>
                "no `run_ended` event, so this trace does not say how the run ended".to_string(),
        }
    );

    if !by_capability.is_empty() {
        let width = by_capability
            .keys()
            .map(|name| name.len())
            .max()
            .unwrap_or(0);
        let _ = writeln!(out, "\nby capability");
        for (capability, counts) in &by_capability {
            let _ = writeln!(
                out,
                "  {capability:width$}  {} dispatched, {} refused, {} irreversible, wait {}",
                counts.dispatched,
                counts.refused,
                counts.irreversible,
                pretty(counts.wait)
            );
        }
    }

    // One task's calls are the whole of what a single-task run made, so a
    // table of one row would repeat the totals above under a heading. It is
    // worth printing when there is something to compare.
    if by_task.len() > 1 {
        let _ = writeln!(out, "\nby task");
        for (task, counts) in &by_task {
            let _ = writeln!(
                out,
                "  {:9}  {} dispatched, {} refused, {} irreversible, wait {}",
                whose_task(*task),
                counts.dispatched,
                counts.refused,
                counts.irreversible,
                pretty(counts.wait)
            );
        }
    }

    out.push_str(
        "\nnot carried by these events\n\
         \x20 task suspension and resumption   only spawn, completion, and cancellation are recorded\n\
         \x20 cache hits and misses            no event records either\n",
    );
    out
}

/// Which task an event belongs to, in the words a report uses: the entry runs
/// under an id like any other task, and naming it is clearer than printing
/// the number and leaving a reader to know that 0 is not a spawned task.
fn whose_task(task: u64) -> String {
    if task == ENTRY_TASK {
        "the entry".to_string()
    } else {
        format!("task {task}")
    }
}

/// Renders the timeline, honouring a `--capability` or `--task` filter.
pub(crate) fn render_timeline(trace: &Trace, filter: &Filter) -> String {
    let mut out = String::new();
    match filter {
        Filter::All => out.push_str("\ntimeline\n"),
        Filter::Capability(name) => {
            let _ = writeln!(out, "\ntimeline (host calls using `{name}`)");
        }
        Filter::Task(id) => {
            let _ = writeln!(
                out,
                "\ntimeline ({}: its lifecycle, its collections, and the host calls it made)",
                whose_task(*id)
            );
        }
    }
    let mut shown = 0;
    for (at, event) in trace.events.iter().enumerate() {
        if !filter.keeps(event) {
            continue;
        }
        shown += 1;
        let at = at + 1;
        match event {
            Event::TaskSpawned { id, parent, scope } => {
                let parent = match parent {
                    Some(parent) => format!("child of {parent}"),
                    None => "root".to_string(),
                };
                let _ = writeln!(out, "{at:>4}  task_spawned    {id} in `{scope}`, {parent}");
            }
            Event::TaskCompleted { id, cpu } => {
                let _ = writeln!(out, "{at:>4}  task_completed  {id}, cpu {}", pretty(*cpu));
            }
            Event::HeapCollected {
                task,
                allocated,
                freed,
                live_objects,
                live_bytes,
                pause,
            } => {
                let whose = whose_task(*task);
                let _ = writeln!(
                    out,
                    "{at:>4}  heap_collected  {whose}: {allocated} allocated, {freed} freed, {live_objects} live in {live_bytes} bytes, pause {}",
                    pretty(*pause)
                );
            }
            Event::HeapSummary {
                allocated,
                allocated_bytes,
                collections,
                live_bytes,
                peak_bytes,
                pause,
            } => {
                let _ = writeln!(
                    out,
                    "{at:>4}  heap_summary    {allocated} allocated in {allocated_bytes} bytes, {collections} collection(s), {live_bytes} live, peak {peak_bytes}, pause {}",
                    pretty(*pause)
                );
            }
            Event::TaskCancelled { id } => {
                let _ = writeln!(out, "{at:>4}  task_cancelled  {id}");
            }
            Event::EntryEnter { module, function } => {
                let _ = writeln!(out, "{at:>4}  entry_enter     {module}.{function}");
            }
            Event::EntryExit {
                module,
                function,
                cpu,
                wait,
            } => {
                let _ = writeln!(
                    out,
                    "{at:>4}  entry_exit      {module}.{function}, cpu {}, wait {}",
                    pretty(*cpu),
                    pretty(*wait)
                );
            }
            Event::RunEnded { outcome, message } => {
                let _ = writeln!(out, "{at:>4}  run_ended       {}", outcome.as_str());
                if let Some(message) = message {
                    let _ = writeln!(out, "        {message}");
                }
            }
            Event::HostCall(call) => {
                let _ = writeln!(
                    out,
                    "{at:>4}  host_call       {} [{}] {}, by {}, wait {}",
                    call.shown(),
                    call.capability,
                    if call.granted {
                        "dispatched"
                    } else {
                        "refused"
                    },
                    whose_task(call.task),
                    pretty(call.wait)
                );
                match &call.outcome {
                    None => {
                        let _ = writeln!(out, "        no result: the call never reached a host");
                    }
                    Some(Outcome::Value(value)) => {
                        let _ = writeln!(out, "        result {}", value.shown);
                    }
                    Some(Outcome::Error(message)) => {
                        let _ = writeln!(out, "        runtime error: {message}");
                    }
                    Some(Outcome::NotRecordable) => {
                        let _ = writeln!(
                            out,
                            "        no result: the Host API schema declares `{}` not recordable",
                            call.qualified()
                        );
                    }
                }
            }
        }
    }
    if shown == 0 {
        out.push_str("  (no event matches)\n");
    }
    out
}

/// Which events `cove trace` shows in the timeline.
pub(crate) enum Filter {
    All,
    /// Only host calls using this capability.
    Capability(String),
    /// Only the lifecycle events of this task.
    Task(u64),
}

impl Filter {
    fn keeps(&self, event: &Event) -> bool {
        match self {
            Filter::All => true,
            Filter::Capability(name) => match event {
                Event::HostCall(call) => &call.capability == name,
                _ => false,
            },
            // Everything a task did that an event records: it starting and
            // stopping, the collections of the heap that was its own, and the
            // host calls it made. The entry is a task here like any other, so
            // `--task 0` is how a reader asks what the entry did itself.
            Filter::Task(wanted) => match event {
                Event::TaskSpawned { id, .. }
                | Event::TaskCompleted { id, .. }
                | Event::TaskCancelled { id } => id == wanted,
                Event::HeapCollected { task, .. } => task == wanted,
                Event::HostCall(call) => call.task == *wanted,
                _ => false,
            },
        }
    }
}

/// A duration in the form the rest of the CLI prints one.
fn pretty(d: Duration) -> String {
    format!("{d:?}")
}

/// `cove trace <file> [--capability <name>] [--task <id>]`.
pub(crate) fn cmd_trace(args: &[String]) -> Result<(), CliError> {
    let mut path: Option<&String> = None;
    let mut filter = Filter::All;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--capability" => {
                let value = crate::flag_value(args, &mut i, "--capability")?;
                filter = Filter::Capability(value);
            }
            "--task" => {
                let value = crate::flag_value(args, &mut i, "--task")?;
                filter = Filter::Task(value.parse().map_err(|_| {
                    CliError::Message(format!(
                        "`--task` must be a non-negative integer, found `{value}`"
                    ))
                })?);
            }
            other if other.starts_with("--") => {
                return Err(CliError::Message(format!(
                    "unknown `cove trace` flag `{other}`"
                )))
            }
            _ => {
                if path.is_some() {
                    return Err(CliError::Message(
                        "`cove trace` reads one trace file".to_string(),
                    ));
                }
                path = Some(&args[i]);
            }
        }
        i += 1;
    }
    let Some(path) = path else {
        return Err(CliError::Message(
            "`cove trace` needs the path of a trace written by `cove run --trace`".into(),
        ));
    };
    let path = Path::new(path);
    let trace = Trace::read(path).map_err(CliError::Message)?;
    print!("{}", render_summary(path, &trace));
    print!("{}", render_timeline(&trace, &filter));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = r#"{"event":"trace_header","version":2,"values":"full","entry":"restricted.main","args":[]}"#;

    fn read(lines: &[&str]) -> Trace {
        Trace::read_str(&lines.join("\n")).expect("the trace reads")
    }

    fn error(lines: &[&str]) -> String {
        match Trace::read_str(&lines.join("\n")) {
            Ok(_) => panic!("the trace should be rejected"),
            Err(message) => message,
        }
    }

    #[test]
    fn a_trace_reads_back_its_header_and_its_events() {
        let trace = read(&[
            HEADER,
            r#"{"event":"entry_enter","module":"restricted","function":"main"}"#,
            r#"{"event":"host_call","task":0,"module":"documents","op":"read","capability":"documents","wait_ns":900,"granted":true,"args":[{"type":"string","value":"input"}],"outcome":{"kind":"value","value":{"type":"enum","name":"Result","case":"Ok","payload":[{"type":"string","value":"text"}]}}}"#,
        ]);
        assert_eq!(trace.header.version, 2);
        assert_eq!(trace.header.values, ValueCapture::Full);
        assert_eq!(trace.header.entry, "restricted.main");
        assert_eq!(trace.events.len(), 2);
        let calls = trace.dispatched_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].shown(), r#"documents.read("input")"#);
        let Some(Outcome::Value(result)) = &calls[0].outcome else {
            panic!("the call records a value");
        };
        assert_eq!(result.shown, r#"Ok("text")"#);
        assert_eq!(
            result.canonical().unwrap(),
            r#"{"type":"enum","name":"Result","case":"Ok","payload":[{"type":"string","value":"text"}]}"#
        );
    }

    /// A reader that does not know a version must reject the trace rather
    /// than read it as though it were one it does know.
    /// Version 1 is the version this reader used to be, and a trace of it can
    /// answer none of the three questions this one now asks: it carries no
    /// task on a host call, no terminal event, and a null where the entry's
    /// own heap events now name a task. It is refused for its version, which
    /// says exactly that, rather than half-read.
    #[test]
    fn a_version_this_build_does_not_know_is_rejected() {
        for version in [1, 3] {
            let header = format!(
                r#"{{"event":"trace_header","version":{version},"values":"full","entry":"a.b","args":[]}}"#
            );
            let message = error(&[&header]);
            assert!(
                message.contains(&format!(
                    "is version {version}, and this build of `cove` reads version 2"
                )),
                "{message}"
            );
        }
    }

    /// A future version may add fields this build has never heard of, so the
    /// version is checked before anything else is required.
    #[test]
    fn a_future_version_is_rejected_for_its_version_not_for_its_fields() {
        let message = error(&[r#"{"event":"trace_header","version":9}"#]);
        assert!(message.contains("is version 9"), "{message}");
    }

    #[test]
    fn a_file_without_a_header_is_not_a_trace() {
        let message = error(&[r#"{"event":"task_cancelled","id":1}"#]);
        assert!(
            message.contains("a trace begins with a `trace_header`"),
            "{message}"
        );
        assert!(Trace::read_str("").is_err());
    }

    #[test]
    fn an_unknown_event_is_rejected_rather_than_skipped() {
        let message = error(&[HEADER, r#"{"event":"gc_pause","ns":10}"#]);
        assert!(message.contains("unknown event `gc_pause`"), "{message}");
        assert!(message.starts_with(":2:"), "{message}");
    }

    #[test]
    fn an_unknown_recorded_value_type_is_rejected() {
        let message = error(&[
            HEADER,
            r#"{"event":"host_call","task":0,"module":"a","op":"b","capability":"c","wait_ns":0,"granted":true,"args":[{"type":"vector"}],"outcome":null}"#,
        ]);
        assert!(
            message.contains("unknown recorded value type `vector`"),
            "{message}"
        );
    }

    #[test]
    fn a_malformed_line_names_the_line_it_is_on() {
        let message = error(&[HEADER, "{not json}"]);
        assert!(message.starts_with(":2:"), "{message}");
    }

    #[test]
    fn a_redacted_value_reads_back_as_missing_rather_than_as_a_value() {
        let trace = read(&[
            r#"{"event":"trace_header","version":2,"values":"redacted","entry":"a.b","args":[]}"#,
            r#"{"event":"host_call","task":0,"module":"env","op":"get","capability":"env","wait_ns":0,"granted":true,"args":[{"type":"redacted","of":"String"}],"outcome":{"kind":"value","value":{"type":"redacted","of":"Option"}}}"#,
        ]);
        let call = trace.dispatched_calls()[0];
        assert_eq!(call.shown(), "env.get(<redacted String>)");
        assert!(call.args[0].canonical().is_none());
        let Err(missing) = &call.args[0].value else {
            panic!("a redacted value carries no value");
        };
        assert!(missing.reason().contains("--trace-values redacted"));
    }

    /// A container is only as reconstructible as its parts, and a report
    /// should still show the shape around the part that is missing.
    #[test]
    fn a_container_holding_a_missing_part_carries_the_reason_upward() {
        let trace = read(&[
            HEADER,
            r#"{"event":"host_call","task":0,"module":"a","op":"b","capability":"c","wait_ns":0,"granted":true,"args":[{"type":"array","items":[{"type":"int","value":1},{"type":"opaque","of":"Vector","shown":"[2]"}]}],"outcome":null}"#,
        ]);
        let call = trace.dispatched_calls()[0];
        assert_eq!(call.args[0].shown, "[1, <Vector [2]>]");
        assert!(matches!(call.args[0].value, Err(Missing::Opaque(_))));
    }

    #[test]
    fn a_not_recordable_result_reads_back_as_that_and_not_as_a_value() {
        let trace = read(&[
            HEADER,
            r#"{"event":"host_call","task":0,"module":"process","op":"exit","capability":"process","wait_ns":0,"granted":true,"args":[{"type":"int","value":0}],"outcome":{"kind":"not_recordable"}}"#,
        ]);
        assert!(matches!(
            trace.dispatched_calls()[0].outcome,
            Some(Outcome::NotRecordable)
        ));
    }

    fn example_trace() -> Trace {
        read(&[
            HEADER,
            r#"{"event":"entry_enter","module":"restricted","function":"main"}"#,
            r#"{"event":"host_call","task":0,"module":"documents","op":"read","capability":"documents","wait_ns":1000,"granted":true,"args":[{"type":"string","value":"input"}],"outcome":{"kind":"value","value":{"type":"enum","name":"Result","case":"Ok","payload":[{"type":"string","value":"text"}]}}}"#,
            r#"{"event":"host_call","task":0,"module":"console","op":"println","capability":"console","wait_ns":2000,"granted":true,"args":[{"type":"string","value":"text"}],"outcome":{"kind":"value","value":{"type":"enum","name":"Result","case":"Ok","payload":[{"type":"unit"}]}}}"#,
            r#"{"event":"host_call","task":0,"module":"files","op":"read","capability":"files","wait_ns":0,"granted":false,"args":[{"type":"string","value":"a.txt"}],"outcome":null}"#,
            r#"{"event":"entry_exit","module":"restricted","function":"main","cpu_ns":5000,"wait_ns":3000}"#,
        ])
    }

    #[test]
    fn the_summary_counts_calls_capabilities_and_irreversible_writes() {
        let text = render_summary(Path::new("t.jsonl"), &example_trace());
        assert!(
            text.contains("entry        restricted.main cpu 5µs wait 3µs"),
            "{text}"
        );
        assert!(
            text.contains("host calls   2 dispatched, 1 refused"),
            "{text}"
        );
        // `console.println` is the irreversible one; `documents.read` reads.
        assert!(
            text.contains("irreversible 1 of the 2 dispatched calls cannot be taken back"),
            "{text}"
        );
        assert!(
            text.contains("console    1 dispatched, 0 refused, 1 irreversible"),
            "{text}"
        );
        assert!(
            text.contains("files      0 dispatched, 1 refused"),
            "{text}"
        );
    }

    /// The summary must not print a zero that reads like a measurement for a
    /// distinction no event carries.
    #[test]
    fn the_summary_says_which_distinctions_the_events_do_not_carry() {
        let text = render_summary(Path::new("t.jsonl"), &example_trace());
        for missing in ["task suspension and resumption", "cache hits and misses"] {
            assert!(text.contains(missing), "{missing} is not reported: {text}");
        }
        // ADR 0011 added the events for this one, so it left the list, and a
        // `host_call` now carries the id of the task that made it, so that
        // left too.
        assert!(!text.contains("allocation and memory pressure"), "{text}");
        assert!(!text.contains("which task made a host call"), "{text}");
    }

    /// A run's ending is the one thing a summary could not report at all, and
    /// the classification is what a reader groups runs by.
    #[test]
    fn the_summary_says_how_the_run_ended() {
        let succeeded = read(&[
            HEADER,
            r#"{"event":"run_ended","outcome":"success","message":null}"#,
        ]);
        let text = render_summary(Path::new("t.jsonl"), &succeeded);
        assert!(text.contains("outcome      success"), "{text}");

        let stopped = read(&[
            HEADER,
            r#"{"event":"run_ended","outcome":"deadline","message":"execution stopped: wall-clock deadline of 1ms exceeded"}"#,
        ]);
        let text = render_summary(Path::new("t.jsonl"), &stopped);
        assert!(
            text.contains(
                "outcome      deadline — execution stopped: wall-clock deadline of 1ms exceeded"
            ),
            "{text}"
        );
        assert!(
            render_timeline(&stopped, &Filter::All).contains("run_ended       deadline"),
            "{text}"
        );
    }

    /// A trace with no terminal event is one somebody assembled or truncated,
    /// and reading it as a run that succeeded would be inventing an answer.
    #[test]
    fn a_trace_without_a_terminal_event_says_so_rather_than_guessing() {
        let text = render_summary(Path::new("t.jsonl"), &example_trace());
        assert!(text.contains("no `run_ended` event"), "{text}");
    }

    #[test]
    fn an_unknown_run_outcome_is_rejected_rather_than_read_as_a_known_one() {
        let message = error(&[
            HEADER,
            r#"{"event":"run_ended","outcome":"exploded","message":null}"#,
        ]);
        assert!(
            message.contains("unknown run outcome `exploded`"),
            "{message}"
        );
    }

    /// A trace with no `heap_summary` says so, rather than printing zeroes
    /// that would read as "this run allocated nothing".
    #[test]
    fn a_trace_without_a_heap_summary_says_it_carries_no_memory_figures() {
        let text = render_summary(Path::new("t.jsonl"), &example_trace());
        assert!(text.contains("no `heap_summary` event"), "{text}");
    }

    #[test]
    fn the_summary_reports_allocation_collections_and_the_live_heap() {
        let trace = read(&[
            HEADER,
            r#"{"event":"heap_collected","task":1,"allocated":40,"freed":36,"live_objects":4,"live_bytes":512,"pause_ns":9000}"#,
            r#"{"event":"heap_collected","task":2,"allocated":24,"freed":24,"live_objects":0,"live_bytes":0,"pause_ns":3000}"#,
            r#"{"event":"heap_summary","allocated":64,"allocated_bytes":4096,"collections":2,"live_bytes":512,"peak_bytes":900,"pause_ns":12000}"#,
        ]);
        let text = render_summary(Path::new("t.jsonl"), &trace);
        assert!(
            text.contains(
                "heap         64 object(s) allocated in 4096 bytes, 512 live at the end, peak 900"
            ),
            "{text}"
        );
        // Two heaps were collected, which is what a run with tasks looks like.
        assert!(
            text.contains(
                "collections  2 over 2 heap(s), 60 object(s) reclaimed of 64 allocated between them"
            ),
            "{text}"
        );
    }

    #[test]
    fn the_timeline_shows_a_collection_and_the_run_s_heap_summary() {
        let trace = read(&[
            HEADER,
            r#"{"event":"heap_collected","task":2,"allocated":64,"freed":60,"live_objects":4,"live_bytes":512,"pause_ns":9000}"#,
            r#"{"event":"heap_summary","allocated":64,"allocated_bytes":4096,"collections":1,"live_bytes":512,"peak_bytes":900,"pause_ns":9000}"#,
        ]);
        let text = render_timeline(&trace, &Filter::All);
        assert!(
            text.contains("heap_collected  task 2: 64 allocated, 60 freed, 4 live in 512 bytes"),
            "{text}"
        );
        assert!(
            text.contains("heap_summary    64 allocated in 4096 bytes, 1 collection(s)"),
            "{text}"
        );
        // A collection belongs to a task, so a task filter keeps it.
        assert!(
            render_timeline(&trace, &Filter::Task(2)).contains("heap_collected"),
            "{text}"
        );
        assert!(
            !render_timeline(&trace, &Filter::Task(1)).contains("heap_collected"),
            "{text}"
        );
    }

    #[test]
    fn an_operation_the_shipped_schema_does_not_know_is_reported_as_unknown() {
        let trace = read(&[
            HEADER,
            r#"{"event":"host_call","task":0,"module":"network","op":"fetch","capability":"network","wait_ns":0,"granted":true,"args":[],"outcome":null}"#,
        ]);
        let text = render_summary(Path::new("t.jsonl"), &trace);
        assert!(text.contains("unknown      network.fetch"), "{text}");
    }

    #[test]
    fn the_timeline_shows_every_event_with_its_arguments_and_result() {
        let text = render_timeline(&example_trace(), &Filter::All);
        assert!(text.contains("entry_enter     restricted.main"), "{text}");
        assert!(
            text.contains(r#"host_call       documents.read("input") [documents] dispatched"#),
            "{text}"
        );
        assert!(text.contains(r#"result Ok("text")"#), "{text}");
        assert!(
            text.contains("no result: the call never reached a host"),
            "{text}"
        );
    }

    #[test]
    fn filtering_by_capability_keeps_only_that_capability_s_calls() {
        let text = render_timeline(&example_trace(), &Filter::Capability("console".into()));
        assert!(text.contains("console.println"), "{text}");
        assert!(!text.contains("documents.read"), "{text}");
        assert!(!text.contains("entry_enter"), "{text}");
    }

    /// A `println` one task made, written the way a trace records it.
    fn task_println(task: u64, text: &str) -> String {
        format!(
            r#"{{"event":"host_call","task":{task},"module":"console","op":"println","capability":"console","wait_ns":{},"granted":true,"args":[{{"type":"string","value":"{text}"}}],"outcome":null}}"#,
            task * 1000
        )
    }

    /// A host call names the task that made it, so filtering by task selects
    /// the calls that task made along with its lifecycle.
    #[test]
    fn filtering_by_task_keeps_the_host_calls_that_task_made() {
        let lines = [
            HEADER.to_string(),
            r#"{"event":"task_spawned","id":1,"parent":null,"scope":"main"}"#.to_string(),
            task_println(1, "one"),
            r#"{"event":"task_completed","id":1,"cpu_ns":10}"#.to_string(),
            r#"{"event":"task_spawned","id":2,"parent":1,"scope":"main"}"#.to_string(),
            task_println(2, "two"),
            task_println(0, "entry"),
        ];
        let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
        let trace = read(&borrowed);

        let text = render_timeline(&trace, &Filter::Task(1));
        assert!(text.contains("timeline (task 1"), "{text}");
        assert!(text.contains("task_spawned    1 in `main`, root"), "{text}");
        assert!(text.contains("task_completed  1"), "{text}");
        assert!(text.contains(r#"console.println("one")"#), "{text}");
        assert!(text.contains("by task 1"), "{text}");
        assert!(!text.contains("task_spawned    2"), "{text}");
        assert!(!text.contains(r#"console.println("two")"#), "{text}");
        assert!(!text.contains(r#"console.println("entry")"#), "{text}");

        // The entry is a task like any other here, and `--task 0` is how a
        // reader asks what it did itself.
        let entry = render_timeline(&trace, &Filter::Task(0));
        assert!(entry.contains("timeline (the entry"), "{entry}");
        assert!(entry.contains(r#"console.println("entry")"#), "{entry}");
        assert!(entry.contains("by the entry"), "{entry}");
        assert!(!entry.contains(r#"console.println("one")"#), "{entry}");
    }

    /// Two tasks that both called a host are separable in the summary, which
    /// is the question a concurrent trace could not answer before.
    #[test]
    fn the_summary_groups_host_calls_by_the_task_that_made_them() {
        let lines = [
            HEADER.to_string(),
            task_println(1, "one"),
            task_println(2, "two"),
            r#"{"event":"host_call","task":2,"module":"files","op":"read","capability":"files","wait_ns":0,"granted":false,"args":[],"outcome":null}"#.to_string(),
        ];
        let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
        let text = render_summary(Path::new("t.jsonl"), &read(&borrowed));
        let row = |name: &str| {
            text.lines()
                .find(|line| line.trim_start().starts_with(name))
                .unwrap_or_else(|| panic!("no `{name}` row in:\n{text}"))
                .to_string()
        };
        assert!(
            row("task 1").contains("1 dispatched, 0 refused, 1 irreversible, wait 1µs"),
            "{text}"
        );
        assert!(
            row("task 2").contains("1 dispatched, 1 refused, 1 irreversible, wait 2µs"),
            "{text}"
        );
    }

    /// One task's calls are the totals over again, so the table that would
    /// say nothing new is not printed.
    #[test]
    fn a_run_whose_calls_all_came_from_one_task_prints_no_table_of_one_row() {
        let text = render_summary(Path::new("t.jsonl"), &example_trace());
        assert!(!text.contains("by task"), "{text}");
    }

    #[test]
    fn a_filter_that_matches_nothing_says_so() {
        let text = render_timeline(&example_trace(), &Filter::Capability("clock".into()));
        assert!(text.contains("(no event matches)"), "{text}");
    }

    #[test]
    fn a_redacted_trace_says_it_cannot_be_replayed() {
        let trace = read(&[
            r#"{"event":"trace_header","version":2,"values":"redacted","entry":"a.b","args":[]}"#,
        ]);
        let text = render_summary(Path::new("t.jsonl"), &trace);
        assert!(text.contains("cannot be replayed"), "{text}");
    }

    #[test]
    fn a_full_trace_says_its_values_may_include_secrets() {
        let text = render_summary(Path::new("t.jsonl"), &example_trace());
        assert!(text.contains("may include secrets"), "{text}");
    }
}
