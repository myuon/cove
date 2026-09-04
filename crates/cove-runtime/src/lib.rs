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
// Private: one type, and on every platform but `wasm32-unknown-unknown` it is
// `std::time::Instant` re-exported. See the module for why the exception
// exists and what a deadline does under it.
mod wallclock;

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
// The machine side of issue #241's debugger. The module is private like the
// rest of `vm`, and what leaves it is a trait to implement, an answer to give,
// and the owned snapshots a stop hands out — no word, no frame, no piece of
// the representation `vm` exists to keep in.
pub use vm::debug::{Call, Debugger, Field, Line, Local, Object, Resume, Stop, Word};
pub use vm::exec::SAFEPOINT_STRIDE;
pub use vm::Vm;
