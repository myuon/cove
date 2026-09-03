//! The Cove runtime: values, Host API dispatch, and the MVP interpreter.

pub mod budget;
pub mod builtins;
pub mod clock;
pub mod database;
pub mod embed;
pub mod error;
pub mod files;
pub mod heap;
pub mod host;
pub mod http;
pub mod interp;
// Private: what it holds is one check both backends make, and the public
// surface of it is `Interpreter::invoke` and `Vm::invoke`.
mod invoke;
// Private, with one type re-exported below: the execution backend of ADR
// 0034. The module stays private so that what a caller can name is decided
// here rather than by which items inside it happen to be `pub` — the words,
// the layouts, the memory and the dispatch loop are the representation this
// boundary exists to keep in.
pub mod process;
pub mod runtime;
pub mod schema;
pub mod shared;
pub mod task;
pub mod trace;
pub mod value;
mod vm;

pub use budget::{Budget, Cancellation, Limits, Meter, Stopped};
pub use clock::{Clock, VirtualTime};
pub use database::Database;
pub use error::RuntimeError;
pub use files::Files;
pub use heap::{Collection, HeapStats};
pub use host::{
    shipped_schema, Console, Documents, Env, GrantSource, Grants, HostApi, HostRegistry, NoReentry,
    Reentry, ResourceHandle,
};
pub use http::{Http, ScriptedRequest, Served};
pub use interp::{on_cove_stack, STACK_SIZE};
pub use process::{Process, ProcessLog};
pub use runtime::{Runtime, ENTRY_TASK};
pub use schema::{
    Admits, Effect, FieldSchema, HostType, Mismatch, ModuleSchema, OperationSchema, Part,
    ResourceSchema, TypeSchema,
};
pub use shared::SharedCell;
pub use task::Transfer;
pub use trace::{
    create_trace_file, value_to_json, HostOutcome, JsonlSink, NullSink, RecordedValue,
    RecordingBackend, RunOutcome, TraceEvent, TraceHeader, TraceSink, ValueCapture,
    TRACE_FORMAT_VERSION,
};
pub use value::{Value, ValueView};
pub use vm::exec::SAFEPOINT_STRIDE;
pub use vm::Vm;
