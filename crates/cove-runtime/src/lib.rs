//! The Cove runtime: values, Host API dispatch, and the MVP interpreter.

pub mod budget;
pub mod builtins;
pub mod clock;
pub mod database;
pub mod embed;
pub mod error;
pub mod files;
// Private: the matcher beneath `Inst::RunFind`, shared by the two execution
// tiers so that neither can come to a different answer about what a run
// search means. It is bytes and counters and nothing else — no `Machine`, no
// `Value` — which is what lets the tree-walking interpreter run it over a
// slice and the linear-memory backend over the heap, a bounded step at a
// time.
mod find;
// Private: the float operations Cove decides for itself rather than inheriting
// from `f64`, shared by the two evaluators for `find`'s reason and one more —
// the native tier's code generator spells the same contract out below one of
// them, so this is one specification with three implementations.
mod float;
pub mod heap;
pub mod host;
pub mod http;
pub mod interp;
// Private: what it holds is one check both backends make, and the public
// surface of it is `Interpreter::invoke` and `Vm::invoke`.
mod invoke;
/// The native tier, owned for one lowered program. See [`mod@native`].
pub mod native;
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
    RUNTIME_VERSION, TRACE_FORMAT_VERSION,
};
pub use value::{Value, ValueView};
// The machine side of issue #241's debugger. The module is private like the
// rest of `vm`, and what leaves it is a trait to implement, an answer to give,
// and the owned snapshots a stop hands out — no word, no frame, no piece of
// the representation `vm` exists to keep in.
pub use vm::debug::{Call, Debugger, Field, Line, Local, Object, Resume, Stop, Word};
// The one ABI type the native boundary's public surface names.
//
// `Tiered::entry` answers one, so a crate that implements `Tiered` has to be able
// to name it — and an arm's features are `cove-native`'s, not that crate's. So it
// is re-exported here rather than left to every caller to depend on `cove-native`
// for: the whole point of `abi` being compiled without a code generator is that
// the boundary can be written against it, and this is a caller of the boundary
// doing exactly that.
pub use cove_native::{Entry as NativeEntry, IntrinsicCode, WindowCode};
pub use native::{
    compile as compile_native, compile_counting as compile_native_counting, Blocked, Blocker,
    NativeProgram, Refused,
};
pub use vm::exec::native::{
    ablate as native_ablate, census_reset, census_taken, helpers as native_helpers,
    helpers_ablated as native_helpers_ablated, helpers_counting as native_helpers_counting, Census,
    NothingCompiled, Session as NativeSession, Tiered, Tiers,
};
pub use vm::exec::SAFEPOINT_STRIDE;
pub use vm::profile::{Cost, Profiler};
pub use vm::report::{
    BoundaryReport, Decline, Emitted, HelperCalls, IntrinsicCalls, LibraryCalls, Outcome, Windows,
};
pub use vm::Vm;
