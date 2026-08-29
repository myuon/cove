//! The Host API boundary.
//!
//! Cove code has no ambient authority. Files, network, clocks, processes, and
//! databases are explicit capabilities with replaceable real, fake, filtered,
//! or denied implementations. The runtime rejects Host API calls that were not
//! granted.
//!
//! This module holds the boundary itself — [`HostApi`], [`Grants`], and
//! [`HostRegistry`] — together with the three small modules that have nothing
//! else to say: [`Console`], [`Env`], and [`Documents`]. A host with rules of
//! its own gets a module of its own: [`crate::clock`], [`crate::files`],
//! [`crate::http`], [`crate::process`], and [`crate::database`].
//!
//! Two things cross this boundary besides plain values. A [`ResourceHandle`]
//! goes outwards: the host keeps a connection or a listening socket and
//! Cove holds the name of it, so a later call names the resource the way a
//! method names its receiver. A [`Reentry`] goes inwards: a host that was
//! handed a Cove callback — a route's handler, a repeating timer's body, the
//! work a timeout bounds — needs a way to run it, and this is the only one
//! there is. Both are ADR 0013's.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cove_sema::Capability;

use crate::budget::{Budget, Cancellation};
use crate::error::RuntimeError;
use crate::schema::{
    Admits, Effect, Mismatch, ModuleSchema, OperationSchema, Part, ResourceSchema, TypeSchema,
};
use crate::trace::{HostOutcome, NullSink, RecordedValue, RunOutcome, TraceEvent, TraceSink};
use crate::value::Value;

/// One host-provided module, such as `console` or `env`.
///
/// A host is shared by every task of a run, so an operation is invoked
/// through a shared reference and a host is `Send + Sync`. A host that needs
/// mutable state of its own says so with a lock it owns, which is also what
/// decides how much of it two tasks may do at once: `console` serializes its
/// writes so a line is never torn, while `clock.sleep` holds nothing, so two
/// tasks can wait at the same time instead of queueing behind each other.
///
/// # An operation that blocks
///
/// A host call is a hole in the run's safepoint chain. The interpreter checks
/// fuel, the deadline, and cancellation at loop back edges, calls, and
/// `await`, and a program sitting inside a host reaches none of the three;
/// [`Budget::charge_host_call`] checks the deadline and the cancellation flag
/// once more before dispatch, but that bounds when a call *starts*, not how
/// long it runs. Nothing in the runtime can interrupt a host that is waiting
/// in `accept` or `read`. So this is a contract the boundary states and each
/// host keeps, rather than something the boundary can enforce.
///
/// An operation that waits must bound how long it waits. It polls in steps
/// short enough that the run's controls are still responsive, asks the
/// [`Reentry`] it was handed whether the run has been stopped
/// ([`Reentry::is_cancelled`]) and how long it has left
/// ([`Reentry::time_left`]) between steps, and holds no lock while it does —
/// a host waiting under its own mutex blocks every other task that wants it.
/// One total allowance covers a multi-part operation: a per-read timeout that
/// starts again on every successful read bounds nothing, because a peer that
/// makes slow progress can hold the call open forever. `http.Server.handle`
/// is the worked example. It accepts by polling rather than blocking, gives
/// the whole of one request a single deadline clamped by what the run has
/// left, and answers "nothing more to serve" when the run is stopped, so the
/// program's own loop ends and the budget reports the stop it owns.
///
/// An operation that genuinely cannot cooperate — a C library call with no
/// timeout, a syscall that cannot be interrupted — must say so in its own
/// documentation, so an embedder knows the run's deadline does not bound that
/// call and can decide what to do about it. What is not acceptable is a host
/// that blocks indefinitely and says nothing.
///
/// # Migrating from the five-accessor form
///
/// This trait used to ask a module to describe itself five times — `name()`,
/// `capability()`, `schema()`, `types()`, and `resources()`. It asks once
/// now, through [`HostApi::module_schema`], and the five are gone rather than
/// defaulted: a defaulted accessor is an overridable one, and an overridable
/// one is a second description of the module, which the checker and the
/// boundary could then read differently. An implementation written against
/// the old shape fails to compile with `not all trait items implemented`,
/// which is the intended way to find out.
///
/// The migration is to delete all five and write the table they were reading
/// from:
///
/// ```
/// # use cove_runtime::error::RuntimeError;
/// # use cove_runtime::host::HostApi;
/// # use cove_runtime::schema::ModuleSchema;
/// # use cove_runtime::value::Value;
/// # struct Company;
/// const COMPANY: ModuleSchema = ModuleSchema {
///     name: "company",
///     capability: "directory",
///     operations: &[],
///     types: &[],
///     resources: &[],
/// };
///
/// impl HostApi for Company {
///     fn module_schema(&self) -> ModuleSchema {
///         COMPANY
///     }
///
///     fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
///         # let _ = (op, args);
///         // unchanged
///         # Ok(Value::Unit)
///     }
/// }
/// ```
///
/// `name` is the string `name()` returned, `capability` is the string behind
/// the `Capability` `capability()` returned, and the three slices are what
/// `schema()`, `types()`, and `resources()` returned. `call`, `call_with`,
/// and `call_resource` are untouched.
///
/// The one thing that gets harder is a schema assembled at run time. The old
/// accessors handed back borrows of `self`, so a host could keep a `String`
/// and a `Vec<OperationSchema>` and return references into itself;
/// [`ModuleSchema`] is `Copy` with `'static` contents, so a module whose
/// shape comes from configuration or a manifest builds its table once, leaks
/// it, and hands out the same copy.
///
/// Putting a lifetime on [`ModuleSchema`] would not give the old form back.
/// A module registered here is a `Box<dyn HostApi>`, which is
/// `Box<dyn HostApi + 'static>` — a spawned task's thread holds the registry
/// and `std::thread::Builder::spawn` takes a `'static` closure — so a host
/// has nothing outside itself to borrow a schema from, and borrowing from a
/// field beside another one of its own is a self-referential struct.
/// [`ModuleSchema`]'s own documentation weighs that against the alternatives
/// and spells the pattern out; `crates/cove-runtime/tests/embedding.rs` runs
/// it end to end.
pub trait HostApi: Send + Sync {
    /// The whole of what this module declares about itself: the name Cove
    /// source uses, the capability a host must grant for it, the operations
    /// it exposes, the types it declares, and the kinds of resource it can
    /// open.
    ///
    /// The schema is the module's declaration of itself: a host cannot
    /// expose an operation without saying what it takes, what it produces,
    /// what it costs the outside world, and whether its result may cross a
    /// task boundary. The boundary holds every call to it, so an operation
    /// arriving in `call` has already been checked against what is declared
    /// here.
    ///
    /// One table rather than five methods, because this exact value is also
    /// what the *checker* reads: `cove_sema::Compiler::with_host_schema`
    /// takes a [`ModuleSchema`], so the description a run enforces and the
    /// description `cove check` checked a call against are the same bytes
    /// for an embedder's module as they already are for a shipped one. This
    /// is the only way to ask a module what it declares — there is nothing
    /// else on this trait to override instead, so a module cannot describe
    /// itself one way to the boundary and another way to the checker.
    fn module_schema(&self) -> ModuleSchema;

    /// Invokes one operation.
    ///
    /// The default forwards to [`HostApi::call`], which is what a module that
    /// never runs a Cove callback wants. A module that does — `clock.every`,
    /// `http.Server.handle` — overrides this instead and leaves `call`
    /// unreachable.
    ///
    /// `back` is the way into the program that made this call, and it is on
    /// loan for the duration of the call and no longer. [`Reentry`] states
    /// the whole of what may be done with it, and the parts an implementor is
    /// most likely to get wrong are these: it may not be retained past this
    /// return, it may be used as many times as the operation means, it may be
    /// used from this thread only, and no lock this module owns may be held
    /// while it is used, because the Cove code it runs may call this module
    /// again.
    fn call_with(
        &self,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        let _ = back;
        self.call(op, args)
    }

    /// Invokes one operation.
    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError>;

    /// Invokes one operation on a handle this module issued.
    ///
    /// A module that declares no resources can never be reached here, so the
    /// default says so rather than inventing an answer.
    ///
    /// `back` is the same loan [`HostApi::call_with`] describes, under the
    /// same rules, and the lock rule is sharper here than anywhere else: the
    /// table of open resources is exactly the lock a module holds, and the
    /// callback is exactly the code that may ask for a resource in it. Take
    /// what the callback's work needs, release the guard, and reenter after.
    fn call_resource(
        &self,
        handle: &ResourceHandle,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        let _ = (args, back);
        Err(RuntimeError::new(format!(
            "host module `{}` issues no resource handles, so `{}.{op}` cannot be called",
            self.module_schema().name,
            handle.qualified_type()
        )))
    }
}

/// How a host runs a Cove callback it was handed.
///
/// A Host API call is a value in and a value out, which is enough until an
/// operation is given work to do rather than data to act on. A route's
/// handler, a repeating timer's body, and the block a timeout bounds are all
/// Cove closures the host holds and has to run; without this they could be
/// stored and never called.
///
/// The callback runs on the task that made the host call, on that task's own
/// stack, charged to that task's budget. There is no second thread and no
/// scheduler: a host that wants concurrency spawns nothing, because
/// concurrency in Cove belongs to a task scope the program wrote.
///
/// It is also how a host asks about the run it is inside.
/// [`Reentry::is_cancelled`] and [`Reentry::time_left`] answer the two
/// questions an operation that waits has to keep asking, and they are the
/// only way to ask them: a host holds no budget and no interpreter of its
/// own. [`HostApi`] says what a host owes them.
///
/// And it is what the boundary itself asks one question of.
/// [`Reentry::task`] says which task is calling, which nothing else at the
/// boundary knows: a [`HostRegistry`] is shared by every thread of a run,
/// while the way back borrows the interpreter of exactly one task. The answer
/// goes on the call's trace event, so a trace of a run with concurrent tasks
/// can be grouped by whose I/O each call was.
///
/// # For the current call, and no longer
///
/// A `&mut dyn Reentry` arrives with a lifetime of its own, shorter than the
/// `&self` beside it, and `dyn Reentry` is neither `Send` nor `Sync`. So a
/// host cannot put one in a field: a [`HostApi`] is `Send + Sync` and shared
/// across the tasks of a run, and neither the lifetime nor the auto traits
/// will let this through. Both refusals are deliberate rather than an
/// accident of how the signature was written, and neither is worth working
/// around with an `Arc` or a raw pointer. Behind the reference is a borrow of
/// the interpreter running the task that made this call. It is valid while
/// that call is on the stack, and once the call returns the interpreter has
/// moved on: a retained one would name a stack frame the task has left, and
/// would run Cove code on a task that is doing something else.
///
/// A host that wants work done later does not keep the way back. It keeps
/// the callback — a [`Value`] is an ordinary owned value — and asks for
/// another call, which is what `http.Server.handle` is and why the loop
/// around it is written in Cove.
///
/// # As many times as the operation means
///
/// A host may call its callback none, once, or many times. `clock.every`
/// calls it once a period until the timer's task is cancelled;
/// `http.Server.handle` calls it once, and not at all when no request
/// arrived. Nothing here counts invocations and nothing makes the second one
/// cheaper than the first.
///
/// Each one is a call the run pays for in full. Fuel is charged at the
/// callback's own safepoints, its calls count against the run's call-depth
/// limit while it is on the stack, and the run's deadline and cancellation
/// stop it wherever they would stop any other Cove code. A host that loops
/// therefore does not have to police the run: a body that would overrun the
/// budget stops of its own accord and the error comes back out of
/// [`Reentry::call`]. What the host owes is to stop looping when it is told
/// to — [`Reentry::is_cancelled`] between rounds — rather than to keep
/// starting rounds that will all fail.
///
/// # Nested, up to a bound
///
/// A callback is Cove code, so it may call any host the run granted,
/// including this one, and that host may in turn be handed work. The nesting
/// is real: the second host call builds a second way back further down the
/// same native stack, and it reenters the same interpreter, so the inner
/// callback sees the same task, the same heap, and the same budget as the
/// outer one.
///
/// How deep it may go is a runtime control, like recursion depth and for the
/// same reason. Two bound it. The Cove frames a callback makes count against
/// the run's call-depth limit, since they are ordinary calls. And the number
/// of host calls running a callback that may be stacked on one thread is
/// bounded separately and much lower, because between one callback's frame
/// and the next sits however much native stack the host chose to use, which
/// nothing can measure. Past that bound the next reentry is refused with a
/// [`RuntimeError`]; the run stops, rather than the process. It is a bound
/// and not a proof: a host that uses an enormous amount of stack before it
/// reenters can still exhaust it at the first level, and that is the host's
/// responsibility, not the boundary's.
///
/// # One at a time, on the calling thread
///
/// A host may not call back from a thread of its own, and cannot: there is
/// exactly one `&mut dyn Reentry` per host call and it is neither `Send` nor
/// `Sync`, so two threads cannot hold it and it cannot be moved to one. This
/// is the design's answer and not a limitation waiting to be lifted.
/// Concurrency in Cove belongs to a task scope the program wrote; a host that
/// ran a program's code on threads the program never asked for would be
/// deciding how much of that program runs at once, and would be doing it
/// outside every control the run was given.
///
/// # No lock may be held across it
///
/// A host must not hold a resource mutex, or any other non-reentrant lock,
/// while it calls a callback. The callback is Cove code and Cove code may
/// call this same host again; a `std::sync::Mutex` is not reentrant, so the
/// second call would deadlock the task on a lock the first call is holding
/// three frames up its own stack. Nothing detects this, because from the
/// lock's point of view nothing is wrong.
///
/// The shape that works is to take what the callback's work needs while the
/// lock is held, release it, and then reenter. `http`'s `Server.handle` is
/// the worked example: it takes the next request, or a clone of the listening
/// socket, out from under the table of open listeners, drops the guard, and
/// only then runs the route's handler — which is free to call `http.listen`
/// again, or to serve on the same handle.
///
/// # Reentry is not task transfer
///
/// Nothing crosses a task boundary here, so the rules that govern one do not
/// apply. A callback's arguments are handed from a host to the interpreter of
/// the task that called it, and its result comes back the same way, both on
/// one thread, both belonging to one task throughout. [`crate::task::Transfer`]
/// is not consulted and cannot be: an argument that may not cross a task
/// boundary — a `Vector`, a resource handle whose schema says
/// `task_safe: false` — is a perfectly ordinary argument to a callback, and a
/// callback may answer with one.
///
/// Two things nearby do belong to tasks, and it is worth saying which.
/// [`ResourceSchema::task_safe`] still decides whether the handle an
/// operation *returns* may later be captured by a `spawn`; that is a question
/// about the value, asked at the boundary the value eventually crosses, and
/// reentry is not that boundary. And a callback that is an `async fn` answers
/// with a task, which the implementation settles before handing the value
/// back — the host was given a callback rather than a task, so this is what
/// `await` would have done at the call site the host is standing in for. That
/// settle is a join, and a value coming back out of a *spawned* task does
/// cross a boundary and is checked; a settled `async fn` body ran on this
/// thread and crosses nothing.
pub trait Reentry {
    /// Calls `callee` with `args` and answers what it produced.
    ///
    /// The call runs to completion before this returns, on this thread. An
    /// `Err` is what the callback failed with, or what stopped the run while
    /// it was running — exhausted fuel, an expired deadline, a raised
    /// cancellation, a depth limit — and a host that receives one has nothing
    /// useful to add: pass it on, so the reason the run stopped reaches the
    /// caller as the runtime wrote it.
    ///
    /// No lock the host owns may be held across this call. See the trait's
    /// documentation for why, and for what a host may and may not do with
    /// the way back it was handed.
    fn call(&mut self, callee: &Value, args: Vec<Value>) -> Result<Value, RuntimeError>;

    /// Calls `callee` with `args`, stopping it at its next safepoint if
    /// `stop` is raised while it runs.
    ///
    /// This is how a timeout is a timeout rather than a measurement taken
    /// afterwards: the body observes the flag exactly where it observes its
    /// own task's cancellation, and stops there.
    ///
    /// `stop` bounds this call and everything inside it, including a further
    /// host call the body makes and any callback that host runs in turn — a
    /// bound that a nested call escaped would not be a bound. It adds to the
    /// reasons the body may stop and replaces none of them: the run's own
    /// cancellation and deadline still apply, and a body stopped by one of
    /// those is not stopped by this. A caller that needs to tell the two
    /// apart reads `stop` afterwards, which is what `clock.timeout` does to
    /// decide whether to report its bound or the error it was given.
    fn call_until(
        &mut self,
        callee: &Value,
        args: Vec<Value>,
        stop: &Cancellation,
    ) -> Result<Value, RuntimeError>;

    /// Whether the work that made this host call has been asked to stop.
    ///
    /// This is everything a safepoint in Cove code would answer to: the run's
    /// own cancellation, the task's, and the flag of any bounded call this
    /// one is nested inside — a blocking call made from the body of a
    /// `clock.timeout` is inside that bound as much as any Cove statement is.
    ///
    /// A host that loops or waits reads this between rounds and gives up when
    /// it is raised: `clock.every` ends the timer rather than leaving it
    /// running with nobody waiting, and `http.Server.handle` stops waiting
    /// for a connection nobody is going to make. Giving up means answering
    /// whatever the operation's own "nothing happened" is. The stop belongs
    /// to the runtime, which reports it at the next safepoint with the limit
    /// that was configured; a host that raised an error of its own would be
    /// answering a question it was not asked.
    fn is_cancelled(&self) -> bool;

    /// How long the run that made this host call has before its deadline
    /// expires.
    ///
    /// `None` means the run has no deadline and nothing here bounds it.
    /// `Some(Duration::ZERO)` means the deadline has passed, and is as much a
    /// reason to stop as [`Reentry::is_cancelled`] answering true.
    ///
    /// A host that waits reads this for two things. It stops when the answer
    /// reaches zero, the same way it stops when the run is cancelled. And it
    /// clamps its own timeouts by it, so an operation willing to wait thirty
    /// seconds for a peer does not sit there for thirty seconds on behalf of
    /// a run that had two hundred milliseconds left: the shorter of the two
    /// allowances is the one that is honest.
    ///
    /// The answer is a duration rather than an instant because that is what a
    /// host does with it — pass it to a socket timeout, or compare it against
    /// zero — and because a run's deadline is measured from when the run
    /// started, which is the budget's business and not the host's.
    fn time_left(&self) -> Option<Duration>;

    /// Which task made this host call: the innermost spawned task's id, or
    /// [`crate::runtime::ENTRY_TASK`] when the call came from the entry.
    ///
    /// Nothing else can answer it. A [`HostRegistry`] is shared by every
    /// thread of a run and knows nothing about who is calling; the way back is
    /// the one thing at the boundary that belongs to one task, because it
    /// borrows that task's interpreter. So the boundary asks it, and writes
    /// the answer on the call's trace event — which is what lets a trace of a
    /// run with concurrent tasks be grouped by whose I/O each call was.
    ///
    /// A host is not expected to do anything with this. It is asked once per
    /// call, before the operation is dispatched, and a host that reads it is
    /// reading an identity rather than a capability: knowing which task is
    /// calling grants nothing, and two calls from one task are as unrelated
    /// as any other two.
    fn task(&self) -> u64;
}

/// A [`Reentry`] for a caller that has no interpreter to reenter.
///
/// [`HostRegistry::call`] uses it, so a host that never runs a callback can
/// still be driven from a test or a tool with no program behind it. An
/// operation that does need one is told what is missing rather than being
/// handed a closure it cannot run.
pub struct NoReentry;

impl Reentry for NoReentry {
    fn call(&mut self, callee: &Value, _args: Vec<Value>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::new(format!(
            "this host call cannot run {}, because it was not made from a running program",
            callee.type_name()
        )))
    }

    fn call_until(
        &mut self,
        callee: &Value,
        args: Vec<Value>,
        _stop: &Cancellation,
    ) -> Result<Value, RuntimeError> {
        self.call(callee, args)
    }

    fn is_cancelled(&self) -> bool {
        false
    }

    /// A caller with no run behind it has no deadline to run out of, so a
    /// host driven from a test or a tool waits for as long as its own limits
    /// allow and no less.
    fn time_left(&self) -> Option<Duration> {
        None
    }

    /// A caller with no program behind it made the call the way an entry
    /// makes one: outside any spawned task.
    fn task(&self) -> u64 {
        crate::runtime::ENTRY_TASK
    }
}

/// The identity of one resource a host owns.
///
/// ADR 0013: a handle is a name. Every field here is part of the name and
/// none of them is state — `module` and `type_name` say what kind of thing is
/// named, `id` says which one, and `task_safe` is the schema's answer copied
/// onto the handle so the task boundary can read it without a registry.
///
/// That is why a handle is [`Arc`]-shared and immutable: copying one copies a
/// name, two tasks holding it name the same resource, and a trace that
/// records it records something a replay can reproduce exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceHandle {
    /// The host module that issued this handle, such as `database`.
    pub module: String,
    /// The resource kind, such as `Connection`.
    pub type_name: String,
    /// Which resource of that kind, unique among the ones this host issued.
    pub id: u64,
    /// Whether this handle may cross a task boundary, copied from the
    /// resource's [`ResourceSchema`] when the handle was issued.
    pub task_safe: bool,
}

impl ResourceHandle {
    /// Issues a handle for resource `id` of kind `resource` in `module`.
    pub fn new(module: &str, resource: &ResourceSchema, id: u64) -> Arc<ResourceHandle> {
        Arc::new(ResourceHandle {
            module: module.to_string(),
            type_name: resource.name.to_string(),
            id,
            task_safe: resource.task_safe,
        })
    }

    /// The type as Cove source writes it: `database.Connection`.
    pub fn qualified_type(&self) -> String {
        format!("{}.{}", self.module, self.type_name)
    }

    /// Whether two handles name the same resource.
    pub fn names_same(&self, other: &ResourceHandle) -> bool {
        self.module == other.module && self.type_name == other.type_name && self.id == other.id
    }
}

/// A handle shows as the name it is: the type, and which one.
impl std::fmt::Display for ResourceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.qualified_type(), self.id)
    }
}

/// The set of capabilities granted at the execution boundary.
#[derive(Clone, Debug, Default)]
pub struct Grants {
    granted: BTreeSet<Capability>,
}

impl Grants {
    pub fn new(names: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Grants {
            granted: names.into_iter().map(Capability::new).collect(),
        }
    }

    pub fn allows(&self, capability: &Capability) -> bool {
        self.granted.contains(capability)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.granted.iter()
    }
}

/// Where a run's grants came from, and so what a reader has to change to
/// widen them.
///
/// A `cove run` reads `[run.<name>] allow` every time it starts, so editing
/// that table is the answer. A binary `cove build` produced carries the grant
/// set it was built with and reads no configuration at all, so the answer
/// there is to change the table and build again — advice to edit a
/// `cove.toml` would be advice that does nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GrantSource {
    /// The `[run.<name>] allow` table this run read when it started.
    #[default]
    Config,
    /// The grant set baked into a built binary.
    Sealed,
}

/// Holds every host module available to a run, and the grants that gate them.
///
/// `HostRegistry::call` is the single choke point through which Cove code
/// reaches external authority, so it is also the right place to observe that
/// authority being exercised: an optional [`Budget`] charges every call
/// against the run's host-call limit before dispatch, and an optional
/// [`TraceSink`] records every call, granted or denied, with how long it
/// took.
pub struct HostRegistry {
    modules: Vec<Box<dyn HostApi>>,
    grants: Grants,
    grant_source: GrantSource,
    trace: Arc<dyn TraceSink>,
    /// The run's budget, shared by every task: ADR 0008 draws a task's fuel
    /// from the run's budget rather than giving each task one of its own, so
    /// there is still exactly one authoritative count of what the run spent.
    budget: Mutex<Option<Budget>>,
    irreversible_writes: AtomicU64,
}

/// What one dispatch is addressed to: a module's operation, or a handle's.
///
/// The two differ only in how they are named, so this carries the naming and
/// [`HostRegistry::dispatch`] carries the rules.
struct Callee {
    /// The host module, which is what a trace records and what a grant gates.
    module: String,
    /// The operation as the trace records it: `query` for a module's, and
    /// `Connection.query` for a handle's.
    op: String,
    /// The handle the call was made on, for a handle's operation.
    ///
    /// A trace records it as the call's first argument, so a run holding two
    /// connections records which one each query went to — and a replay can
    /// tell them apart.
    receiver: Option<Value>,
    /// What has the operation, as a diagnostic names it.
    owner: String,
    /// Every operation the owner has, for the help when this one is not among
    /// them.
    known: Vec<&'static str>,
}

impl Callee {
    /// The call as Cove source writes it: `database.query`, or
    /// `database.Connection.query`.
    fn shown(&self) -> String {
        format!("{}.{}", self.module, self.op)
    }

    /// The operation's own name, without the resource that answers it.
    fn bare_op(&self) -> &str {
        match self.op.rsplit_once('.') {
            Some((_, op)) => op,
            None => &self.op,
        }
    }

    /// The declared signature, qualified the way this callee is named.
    fn signature(&self, schema: &OperationSchema) -> String {
        match self.op.rsplit_once('.') {
            Some((resource, _)) => format!("{resource}.{}", schema.signature()),
            None => schema.signature(),
        }
    }
}

impl HostRegistry {
    pub fn new(grants: Grants) -> Self {
        HostRegistry {
            modules: Vec::new(),
            grants,
            grant_source: GrantSource::default(),
            trace: Arc::new(NullSink),
            budget: Mutex::new(None),
            irreversible_writes: AtomicU64::new(0),
        }
    }

    pub fn register(&mut self, module: Box<dyn HostApi>) {
        self.modules.push(module);
    }

    pub fn grants(&self) -> &Grants {
        &self.grants
    }

    /// Records where these grants came from, which is what a refused call
    /// tells the reader to change.
    pub fn set_grant_source(&mut self, source: GrantSource) {
        self.grant_source = source;
    }

    pub fn contains(&self, name: &str) -> bool {
        self.modules.iter().any(|m| m.module_schema().name == name)
    }

    /// Installs where trace events go. Replaces any sink installed earlier;
    /// the default is [`NullSink`], which discards everything.
    pub fn set_trace(&mut self, sink: Arc<dyn TraceSink>) {
        self.trace = sink;
    }

    /// Installs the budget every call is charged against. Replaces any
    /// budget installed earlier; the default is no budget, which imposes no
    /// host-call limit here (the interpreter's own safepoints still apply
    /// its other limits).
    pub fn set_budget(&mut self, budget: Budget) {
        *self
            .budget
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(budget);
    }

    /// Runs `f` against the run's budget, if the host installed one.
    ///
    /// This is how the interpreter charges its own safepoints — loop back
    /// edges, calls, and `await` — and how a caller reads the counters after
    /// a run. Every task thread reaches the one budget through here, so the
    /// lock is held for the charge and nothing else.
    pub fn with_budget<R>(&self, f: impl FnOnce(&mut Budget) -> R) -> Option<R> {
        let mut budget = self
            .budget
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        budget.as_mut().map(f)
    }

    /// How many calls this run dispatched whose schema declares them
    /// [`Effect::IrreversibleWrite`].
    ///
    /// This is what reads the `effect` an operation declares. Cove makes
    /// irreversible operations require visible intent, so a run is able to
    /// say how many of the things it did cannot be taken back; `cove run
    /// --stats` prints the count. Whether each call actually reached the
    /// outside world is the host's business rather than the registry's, so a
    /// call the host answered with `Err` is still counted: the registry knows
    /// only that a program asked for something irreversible.
    pub fn irreversible_writes(&self) -> u64 {
        self.irreversible_writes.load(Ordering::Relaxed)
    }

    /// The table every registered module declares itself with.
    ///
    /// This is the pairing the checker needs. An embedding registers its
    /// hosts here and hands these same values to `cove_sema::Compiler`, so
    /// the program is checked against the descriptions this registry is
    /// about to enforce rather than against a second set written out beside
    /// them:
    ///
    /// ```ignore
    /// let program = Compiler::new()
    ///     .with_host_schemas(hosts.module_schemas())
    ///     .compile(&package)?;
    /// ```
    ///
    /// A registry that has two modules registered under one name still
    /// dispatches a call to only one of them: every lookup below that finds
    /// a module by name — `contains`, `schema_for`, `host_type`, `call`,
    /// `call_with`, `call_resource`, `module_for_operation` — takes the
    /// first one registered. So this keeps only the first schema registered
    /// under each name too, rather than handing the checker a second
    /// description of a module the runtime will never reach through: the
    /// list it hands back describes exactly what this registry dispatches
    /// to, not everything that was ever registered.
    pub fn module_schemas(&self) -> Vec<ModuleSchema> {
        let mut seen = BTreeSet::new();
        self.modules
            .iter()
            .map(|module| module.module_schema())
            .filter(|schema| seen.insert(schema.name))
            .collect()
    }

    /// Looks up which host module exposes `op`, for unqualified `use` imports.
    pub fn module_for_operation(&self, op: &str) -> Option<&'static str> {
        self.modules.iter().find_map(|m| {
            let schema = m.module_schema();
            schema
                .operations
                .iter()
                .any(|entry| entry.name == op)
                .then_some(schema.name)
        })
    }

    /// The schema of one operation, if the module and the operation both
    /// exist.
    pub fn schema_for(&self, module: &str, op: &str) -> Option<&'static OperationSchema> {
        self.modules
            .iter()
            .find(|m| m.module_schema().name == module)?
            .module_schema()
            .operations
            .iter()
            .find(|entry| entry.name == op)
    }

    /// The type `module.name` declares, if the module declares one.
    ///
    /// A [`TypeSchema`] is [`Copy`], so this hands back the entry itself
    /// rather than a borrow of the registry: the interpreter that asks is
    /// about to evaluate arguments, which it cannot do while holding one.
    pub fn host_type(&self, module: &str, name: &str) -> Option<TypeSchema> {
        self.modules
            .iter()
            .find(|m| m.module_schema().name == module)?
            .module_schema()
            .types
            .iter()
            .find(|declared| declared.name == name)
            .copied()
    }

    /// Whether the value `module.op` produces may cross a task boundary, or
    /// `None` when no such operation exists.
    ///
    /// The Language Card puts this decision in the schema rather than in the
    /// value: "Host resources declare task-safety in their Host API schema."
    pub fn result_is_task_safe(&self, module: &str, op: &str) -> Option<bool> {
        Some(self.schema_for(module, op)?.result_is_task_safe)
    }

    /// Describes one call for a trace, or hands back nothing when no sink
    /// will read it.
    ///
    /// The handle a resource operation was called on comes first, so a run
    /// holding two connections records which one each query went to. A
    /// [`RecordedValue`] is a copy, and one of a value no boundary may carry
    /// is also a rendering of it, so an untraced run makes neither: an event
    /// nothing keeps has nothing worth describing.
    fn record_call(&self, callee: &Callee, args: &[Value]) -> Vec<RecordedValue> {
        if !self.trace.is_recording() {
            return Vec::new();
        }
        callee
            .receiver
            .iter()
            .chain(args)
            .map(RecordedValue::of)
            .collect()
    }

    /// Dispatches a Host API call after checking the grant, the schema, and
    /// the budget, tracing the outcome either way.
    ///
    /// This is the boundary's one choke point, and it takes no interpreter:
    /// an operation that was handed a Cove callback cannot be reached through
    /// it. [`HostRegistry::call_with`] is the same dispatch with a way back
    /// into the program.
    pub fn call(&self, module: &str, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        self.call_with(module, op, args, &mut NoReentry)
    }

    /// Dispatches a Host API call that may run a Cove callback it was given.
    pub fn call_with(
        &self,
        module: &str,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        let task = back.task();
        let Some(entry) = self
            .modules
            .iter()
            .find(|m| m.module_schema().name == module)
        else {
            return Err(RuntimeError::new(format!("unknown host module `{module}`"))
                .with_outcome(RunOutcome::HostBoundary));
        };
        let schema = entry.module_schema();
        let declared = schema
            .operations
            .iter()
            .find(|entry| entry.name == op)
            .copied();
        // An operation declares the capability it needs; the module's own
        // capability stands in for an operation that does not exist, so a
        // call into an ungranted module is still reported as ungranted rather
        // than as a misspelling.
        let capability = match &declared {
            Some(op_schema) => Capability::new(op_schema.capability),
            None => Capability::new(schema.capability),
        };
        let callee = Callee {
            module: module.to_string(),
            op: op.to_string(),
            receiver: None,
            owner: format!("host module `{module}`"),
            known: schema.operations.iter().map(|e| e.name).collect(),
        };
        self.dispatch(task, &callee, declared, capability, args, |args| {
            entry.call_with(op, args, back)
        })
    }

    /// Dispatches an operation on a resource handle, through the same gate
    /// every other Host API call passes.
    ///
    /// A handle is a name, so nothing here trusts it: the module it names has
    /// to exist, the resource kind has to be one that module declares, and
    /// the operation has to be one that kind answers. A handle that outlived
    /// what it named fails inside the host instead, which is where the only
    /// record of what is still open lives.
    pub fn call_resource(
        &self,
        handle: &ResourceHandle,
        op: &str,
        args: Vec<Value>,
        back: &mut dyn Reentry,
    ) -> Result<Value, RuntimeError> {
        let task = back.task();
        let qualified = handle.qualified_type();
        let Some(entry) = self
            .modules
            .iter()
            .find(|m| m.module_schema().name == handle.module)
        else {
            return Err(
                RuntimeError::new(format!("unknown host module `{}`", handle.module))
                    .with_help(format!(
                        "`{qualified}` names a resource of a host module this run has none of"
                    ))
                    .with_outcome(RunOutcome::HostBoundary),
            );
        };
        let schema = entry.module_schema();
        let Some(resource) = schema
            .resources
            .iter()
            .find(|resource| resource.name == handle.type_name)
        else {
            return Err(RuntimeError::new(format!(
                "host module `{}` issues no `{}` handles",
                handle.module, handle.type_name
            ))
            .with_outcome(RunOutcome::HostBoundary));
        };
        let declared = resource.operation(op).copied();
        let capability = match &declared {
            Some(op_schema) => Capability::new(op_schema.capability),
            None => Capability::new(schema.capability),
        };
        let callee = Callee {
            module: handle.module.clone(),
            // A resource operation is recorded under the name that says which
            // resource answered it, so a trace of a run holding two kinds of
            // handle does not read as one flat list of `query` calls.
            op: format!("{}.{op}", handle.type_name),
            receiver: Some(Value::Resource(Arc::new(handle.clone()))),
            owner: format!("`{qualified}`"),
            known: resource.operations.iter().map(|e| e.name).collect(),
        };
        self.dispatch(task, &callee, declared, capability, args, |args| {
            entry.call_resource(handle, op, args, back)
        })
    }

    /// The grant check, the schema check on the way in, the budget charge,
    /// the trace, the dispatch itself, and the schema check on the way out —
    /// everything a Host API call passes through, whether it was addressed to
    /// a module or to a handle.
    ///
    /// Both schema checks read one declaration from both sides: the
    /// arguments must be what `params` says before the host is reached, and
    /// the result must be what `result` says before it is handed on.
    fn dispatch(
        &self,
        task: u64,
        callee: &Callee,
        declared: Option<OperationSchema>,
        capability: Capability,
        args: Vec<Value>,
        invoke: impl FnOnce(Vec<Value>) -> Result<Value, RuntimeError>,
    ) -> Result<Value, RuntimeError> {
        let shown = callee.shown();
        let refused = |args: Vec<RecordedValue>| TraceEvent::HostCall {
            task,
            module: callee.module.clone(),
            op: callee.op.clone(),
            capability: capability.to_string(),
            wait: std::time::Duration::ZERO,
            granted: false,
            args,
            outcome: None,
        };
        if !self.grants.allows(&capability) {
            self.trace.record(refused(self.record_call(callee, &args)));
            return Err(RuntimeError::new(format!(
                "`{shown}` requires the `{capability}` capability, which this run was not granted"
            ))
            .with_rule("Cove code has no ambient authority; the host grants capabilities at the execution boundary.")
            .with_help(match self.grant_source {
                GrantSource::Config => {
                    format!("add `{capability}` to `allow` in the run's `cove.toml` table")
                }
                // Naming a `cove.toml` here would name a file this binary
                // never reads: its grants were fixed when it was built.
                GrantSource::Sealed => format!(
                    "this binary carries the capabilities it was built with; add `{capability}` to `allow` in the run's `cove.toml` table and build it again"
                ),
            })
            .with_outcome(RunOutcome::HostBoundary)
            .with_denied_capability(capability.to_string()));
        }
        let Some(schema) = declared else {
            return Err(RuntimeError::new(format!(
                "{} has no operation `{}`",
                callee.owner,
                callee.bare_op()
            ))
            .with_help(format!(
                "{} exposes {}",
                callee.owner,
                if callee.known.is_empty() {
                    "no operations".to_string()
                } else {
                    callee
                        .known
                        .iter()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            ))
            .with_outcome(RunOutcome::HostBoundary));
        };
        if !schema.accepts(args.len()) {
            return Err(RuntimeError::new(format!(
                "`{shown}` takes {}, but {} were given",
                schema.expected_arity(),
                args.len()
            ))
            .with_help(format!(
                "the Host API schema declares `{}.{}`",
                callee.module,
                callee.signature(&schema)
            ))
            .with_outcome(RunOutcome::HostBoundary));
        }
        // Arity and types are the same check on the same declaration, so they
        // are made together and in the same place: before the host is
        // reached, before the budget is charged, and with nothing on the
        // trace, because a call refused here never happened.
        //
        // `cove check` makes this check too, at the call site, where the
        // mistake has a span to point at — but it can only make it for the
        // hosts it can see. An embedder's own module is registered at run
        // time and named in no table the compiler reads, so this is the only
        // thing standing between such a host and an argument its schema does
        // not admit.
        if let Some(mismatch) = undeclared_argument(&schema, &args) {
            return Err(RuntimeError::new(
                mismatch
                    .what
                    .describe(&shown, Part::Argument(mismatch.position)),
            )
            .with_rule(A_CALL_KEEPS_THE_SCHEMA)
            .with_help(format!(
                "the Host API schema declares `{}.{}`",
                callee.module,
                callee.signature(&schema)
            ))
            .with_outcome(RunOutcome::HostBoundary));
        }

        if let Some(Err(error)) = self.with_budget(|budget| {
            budget
                .charge_host_call()
                .map_err(|stopped| budget.to_runtime_error(stopped))
        }) {
            self.trace.record(refused(self.record_call(callee, &args)));
            return Err(error);
        }

        if schema.effect == Effect::IrreversibleWrite {
            self.irreversible_writes.fetch_add(1, Ordering::Relaxed);
        }

        // A trace has to carry the arguments to be replayable, and the host
        // takes ownership of them, so they are recorded before dispatch
        // rather than reconstructed afterwards.
        let recorded_args = self.record_call(callee, &args);
        let started = Instant::now();
        let result = invoke(args);
        let wait = started.elapsed();
        // The schema decides whether the result is written down. An operation
        // that is not recordable has its call recorded and its result left
        // out: replaying `process.exit` by handing back a value would keep
        // running a program that had ended.
        if self.trace.is_recording() {
            let outcome = if schema.recordable {
                match &result {
                    Ok(value) => HostOutcome::Value(RecordedValue::of(value)),
                    Err(error) => HostOutcome::Error(error.message.clone()),
                }
            } else {
                HostOutcome::NotRecordable
            };
            self.trace.record(TraceEvent::HostCall {
                task,
                module: callee.module.clone(),
                op: callee.op.clone(),
                capability: capability.to_string(),
                wait,
                granted: true,
                args: recorded_args,
                outcome: Some(outcome),
            });
        }

        // The last thing the boundary asks is the one thing it never used to:
        // that the host answered the type it declared. ADR 0001 makes the
        // schema one description shared by the compiler, runtime, and CLI,
        // and a description nothing enforces is a comment. The trace is
        // written first, so what the host actually did is on the record
        // either way; the value is what stops here.
        //
        // Only a value is checked. A host that answers `Err` has already
        // failed on its own terms, and the `Error` a schema declares is the
        // one inside a Cove `Result`, not this one.
        if let Ok(value) = &result {
            if let Err(mismatch) = schema.result.admits(value) {
                return Err(RuntimeError::new(mismatch.describe(&shown, Part::Result))
                    .with_rule(HOST_KEEPS_ITS_SCHEMA)
                    .with_help(format!(
                        "the Host API schema declares `{}.{}`",
                        callee.module,
                        callee.signature(&schema)
                    ))
                    .with_outcome(RunOutcome::HostBoundary));
            }
        }
        result
    }
}

/// The first argument the operation's declaration does not admit, if there is
/// one.
///
/// A loop of its own rather than one more paragraph inside
/// [`HostRegistry::dispatch`], which carries six checks already. A call that
/// keeps its declaration pays one walk of its own arguments and allocates
/// nothing: the description of a disagreement is built on the way out of a
/// failure.
fn undeclared_argument(schema: &OperationSchema, args: &[Value]) -> Option<UndeclaredArgument> {
    for (index, argument) in args.iter().enumerate() {
        // Arity was checked first, so an index past a fixed operation's
        // parameters cannot happen; a variadic one answers for the rest.
        let declared = schema.param(index)?;
        if let Err(what) = declared.admits(argument) {
            return Some(UndeclaredArgument {
                position: index + 1,
                what,
            });
        }
    }
    None
}

/// One argument that is not what its operation declared, and which one it was.
struct UndeclaredArgument {
    /// Which argument, counted from one as a diagnostic counts.
    position: usize,
    /// Where it stopped agreeing with the declared type.
    what: Mismatch,
}

/// The rule a host breaks by answering something its own declaration does not
/// admit.
///
/// This is a broken invariant on the host's side of the boundary rather than
/// an expected failure, so it stops the run instead of arriving in Cove code
/// as a value that program never asked for and cannot handle.
const HOST_KEEPS_ITS_SCHEMA: &str = "A host operation answers the type its Host API schema declares; the schema is one description shared by the compiler, runtime, and CLI.";

/// The rule a call breaks by passing an argument the operation's own
/// declaration does not admit.
///
/// This is the program's mistake rather than the host's, and `cove check`
/// reports it at the call site before a run starts. It is stated again here
/// because the checker reads only the schema of the modules the toolchain
/// ships, and a host may be anyone's.
const A_CALL_KEEPS_THE_SCHEMA: &str = "A Host API call passes the argument types its operation's schema declares; the schema is one description shared by the compiler, runtime, and CLI.";

/// The schema of every host module the toolchain ships.
///
/// `cove trace` and `cove replay` read a trace without a host to ask, and
/// both need what the schema says: which calls the trace recorded are
/// irreversible, which capability each one needs, and whether a result was
/// recordable. `cove-sema` needs the same table with no runtime to depend on
/// at all. So the table is [`cove_schema::hosts::SHIPPED`] and every module
/// below answers with its entry from it: not a copy that agrees, the same
/// bytes.
pub fn shipped_schema() -> &'static [ModuleSchema] {
    cove_schema::hosts::shipped()
}

/// What `console` declares about itself.
///
/// The table is [`cove_schema::hosts::CONSOLE`], so the description the
/// compiler checks a call against and the one the boundary dispatches through
/// are the same bytes.
const CONSOLE_SCHEMA: ModuleSchema = cove_schema::hosts::CONSOLE;

/// `console`: line-oriented output on two streams.
///
/// The two writers are the whole of the difference between the streams. What
/// a program produces goes to `out` through `println` and `print`; what it
/// says about what it produces goes to `err` through `eprintln` and `eprint`,
/// and a host that wants the two apart is a host that hands over two
/// different writers. `cove run` gives the process's stdout and stderr, which
/// is why a program's records can now be piped somewhere while its complaints
/// stay on the terminal.
///
/// The two streams are one capability. All four operations require
/// `console`, so a run that may print may complain, and this type is where
/// the difference between the streams is: an embedding that wants a program's
/// output captured while its complaints reach the terminal hands over a
/// buffer and `std::io::stderr()`, which is a wiring choice rather than an
/// authority one. Nothing here consults the grants at all — the boundary has
/// already decided by the time an operation arrives.
///
/// # Migrating from the one-writer form
///
/// `Console::new` took one writer and now takes two. The second is where
/// diagnostics go, and there is deliberately no default: a host that captured
/// a program's output before this existed would silently start capturing its
/// diagnostics too, which is exactly the mixing the second stream is for
/// undoing. `Console::new(w)` therefore fails to compile rather than changing
/// meaning. An embedding that genuinely wants one stream says so —
/// `Console::new(w.clone(), w)` for a shared writer, `Console::new(w,
/// std::io::sink())` to drop diagnostics — and one that wants the process's
/// streams writes `Console::new(std::io::stdout(), std::io::stderr())`.
pub struct Console<O: Write + Send, E: Write + Send> {
    /// Held under a lock so that one task's line is written whole: two tasks
    /// printing at once must interleave lines, never halves of a line.
    out: Mutex<O>,
    /// The diagnostic stream, under a lock of its own: a task writing a
    /// warning must not queue behind a task writing a record, since the two
    /// streams are usually two different files.
    err: Mutex<E>,
}

impl<O: Write + Send, E: Write + Send> Console<O, E> {
    /// A console whose output goes to `out` and whose diagnostics go to
    /// `err`.
    pub fn new(out: O, err: E) -> Self {
        Console {
            out: Mutex::new(out),
            err: Mutex::new(err),
        }
    }
}

impl<O: Write + Send, E: Write + Send> HostApi for Console<O, E> {
    fn module_schema(&self) -> ModuleSchema {
        CONSOLE_SCHEMA
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let text = args
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        // Each arm holds one stream's lock for the whole of one write, and
        // neither arm takes the other's: the two streams are independent, so
        // a diagnostic never waits on a record.
        let result = match op {
            "println" => write_line(&self.out, &text, true),
            "print" => write_line(&self.out, &text, false),
            "eprintln" => write_line(&self.err, &text, true),
            "eprint" => write_line(&self.err, &text, false),
            _ => unreachable!("checked by HostRegistry::call"),
        };
        match result {
            Ok(()) => Ok(Value::ok(Value::Unit)),
            Err(e) => Ok(Value::err(Value::error(format!("console: {e}")))),
        }
    }
}

/// Writes `text` to one of a console's streams, with a newline after it when
/// `newline`, and flushes.
///
/// Both streams write the same way, and a stream that is written differently
/// from the other is a stream a program can tell apart by something other
/// than where it goes.
fn write_line<W: Write + Send>(
    stream: &Mutex<W>,
    text: &str,
    newline: bool,
) -> std::io::Result<()> {
    let mut stream = stream
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if newline {
        writeln!(stream, "{text}")?;
    } else {
        write!(stream, "{text}")?;
    }
    stream.flush()
}

/// What `env` declares about itself.
const ENV_SCHEMA: ModuleSchema = cove_schema::hosts::ENV;

/// `env`: read-only access to the environment the host supplies.
///
/// The map is given to the constructor rather than read from the process, so a
/// host decides exactly which variables a run can observe.
pub struct Env {
    vars: BTreeMap<String, String>,
}

impl Env {
    /// Builds an environment from the variables the host chooses to expose.
    pub fn new(vars: BTreeMap<String, String>) -> Self {
        Env { vars }
    }

    /// Snapshots the real process environment. Explicit by design: nothing
    /// else in the runtime reads `std::env`.
    pub fn from_process() -> Self {
        Env {
            vars: std::env::vars().collect(),
        }
    }
}

impl HostApi for Env {
    fn module_schema(&self) -> ModuleSchema {
        ENV_SCHEMA
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "get" => {
                let [Value::Str(name)] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                Ok(match self.vars.get(&**name) {
                    Some(value) => Value::some(Value::Str(value.as_str().into())),
                    None => Value::none(),
                })
            }
            _ => unreachable!("checked by HostRegistry::call"),
        }
    }
}

/// What `documents` declares about itself.
const DOCUMENTS_SCHEMA: ModuleSchema = cove_schema::hosts::DOCUMENTS;

/// `documents`: a filtered, read-only view over a fixed set of named text
/// documents.
///
/// Granting `documents` never grants filesystem access. A host names exactly
/// which documents exist; there is no way to reach a path this module was not
/// built to expose, so a grant of `documents` is narrow authority, never
/// ambient access to a directory.
pub struct Documents {
    source: DocumentsSource,
}

enum DocumentsSource {
    InMemory(BTreeMap<String, String>),
    Rooted(PathBuf),
}

impl Documents {
    /// A fake implementation backed by an in-memory map, for tests.
    pub fn in_memory(documents: BTreeMap<String, String>) -> Self {
        Documents {
            source: DocumentsSource::InMemory(documents),
        }
    }

    /// Reads `<root>/<name>.txt` for a document named `name`.
    ///
    /// `name` must be a single plain path component: empty names, `.`, `..`,
    /// and names containing `/`, `\`, or a NUL byte are all rejected before
    /// the filesystem is touched. This keeps the capability narrow: a grant
    /// of `documents` can only ever reach the fixed set of `.txt` files under
    /// `root`, never an arbitrary path via traversal or an absolute path.
    pub fn rooted(root: PathBuf) -> Self {
        Documents {
            source: DocumentsSource::Rooted(root),
        }
    }

    fn read(&self, name: &str) -> Result<String, String> {
        let missing = || format!("no document named `{name}`");
        match &self.source {
            DocumentsSource::InMemory(documents) => {
                documents.get(name).cloned().ok_or_else(missing)
            }
            DocumentsSource::Rooted(root) => {
                if !is_plain_document_name(name) {
                    return Err(missing());
                }
                std::fs::read_to_string(root.join(format!("{name}.txt"))).map_err(|_| missing())
            }
        }
    }
}

/// Whether `name` is safe to join onto a root: a single component, never a
/// path that could escape it.
fn is_plain_document_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0')
}

impl HostApi for Documents {
    fn module_schema(&self) -> ModuleSchema {
        DOCUMENTS_SCHEMA
    }

    fn call(&self, op: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        match op {
            "read" => {
                let [Value::Str(name)] = args.as_slice() else {
                    unreachable!("checked by HostRegistry::call")
                };
                Ok(match self.read(name) {
                    Ok(text) => Value::ok(Value::Str(text.into())),
                    Err(message) => Value::err(Value::error(message)),
                })
            }
            _ => unreachable!("checked by HostRegistry::call"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::HostType;
    use std::path::Path;

    /// A temporary directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "cove-runtime-test-{name}-{}-{}",
                std::process::id(),
                nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn ok_str(value: Value) -> String {
        match value.ok_payload() {
            Some(payload) => match payload.first() {
                Some(Value::Str(text)) => text.to_string(),
                other => panic!("expected `Ok(String)`, found {other:?}"),
            },
            None => panic!("expected `Ok(String)`, found {value}"),
        }
    }

    fn err_message(value: Value) -> String {
        match value.err_payload() {
            Some(payload) => payload.first().map(ToString::to_string).unwrap_or_default(),
            None => panic!("expected `Err(...)`, found {value}"),
        }
    }

    #[test]
    fn in_memory_read_hits_and_misses() {
        let documents = Documents::in_memory(BTreeMap::from([(
            "input".to_string(),
            "hello world".to_string(),
        )]));

        let hit = documents
            .call("read", vec![Value::Str("input".into())])
            .expect("no runtime error");
        assert_eq!(ok_str(hit), "hello world");

        let miss = documents
            .call("read", vec![Value::Str("missing".into())])
            .expect("no runtime error");
        assert_eq!(err_message(miss), "no document named `missing`");
    }

    #[test]
    fn rooted_reads_a_real_file() {
        let dir = TempDir::new("rooted-read");
        std::fs::write(dir.path().join("input.txt"), "five little words here").unwrap();
        let documents = Documents::rooted(dir.path().to_path_buf());

        let read = documents
            .call("read", vec![Value::Str("input".into())])
            .expect("no runtime error");
        assert_eq!(ok_str(read), "five little words here");
    }

    #[test]
    fn rooted_rejects_a_missing_document() {
        let dir = TempDir::new("rooted-missing");
        let documents = Documents::rooted(dir.path().to_path_buf());

        let read = documents
            .call("read", vec![Value::Str("absent".into())])
            .expect("no runtime error");
        assert_eq!(err_message(read), "no document named `absent`");
    }

    #[test]
    fn rooted_rejects_path_traversal() {
        let dir = TempDir::new("rooted-traversal");
        let documents = Documents::rooted(dir.path().to_path_buf());

        let read = documents
            .call("read", vec![Value::Str("..".into())])
            .expect("no runtime error");
        assert_eq!(err_message(read), "no document named `..`");
    }

    #[test]
    fn rooted_rejects_a_nested_path() {
        let dir = TempDir::new("rooted-nested");
        let documents = Documents::rooted(dir.path().to_path_buf());

        let read = documents
            .call("read", vec![Value::Str("a/b".into())])
            .expect("no runtime error");
        assert_eq!(err_message(read), "no document named `a/b`");
    }

    #[test]
    fn rooted_rejects_an_empty_name() {
        let dir = TempDir::new("rooted-empty");
        let documents = Documents::rooted(dir.path().to_path_buf());

        let read = documents
            .call("read", vec![Value::Str("".into())])
            .expect("no runtime error");
        assert_eq!(err_message(read), "no document named ``");
    }

    #[test]
    fn registry_without_the_documents_grant_rejects_the_call() {
        let mut hosts = HostRegistry::new(Grants::new(Vec::<String>::new()));
        hosts.register(Box::new(Documents::in_memory(BTreeMap::new())));

        let error = hosts
            .call("documents", "read", vec![Value::Str("input".into())])
            .expect_err("the call should be rejected");
        assert!(error.message.contains("documents"), "{}", error.message);
    }

    #[test]
    fn registry_with_the_documents_grant_allows_the_call() {
        let mut hosts = HostRegistry::new(Grants::new(["documents"]));
        hosts.register(Box::new(Documents::in_memory(BTreeMap::from([(
            "input".to_string(),
            "hello world".to_string(),
        )]))));

        let value = hosts
            .call("documents", "read", vec![Value::Str("input".into())])
            .expect("the call should be allowed");
        assert_eq!(ok_str(value), "hello world");
    }

    /// Collects every event recorded into it, for assertions.
    #[derive(Clone, Default)]
    struct RecordingSink(Arc<Mutex<Vec<TraceEvent>>>);

    impl RecordingSink {
        fn events(&self) -> Vec<TraceEvent> {
            self.0
                .lock()
                .expect("no test panics while recording")
                .clone()
        }
    }

    impl TraceSink for RecordingSink {
        fn record(&self, event: TraceEvent) {
            self.0
                .lock()
                .expect("no test panics while recording")
                .push(event);
        }
    }

    /// What a recorded value shows as, which is the value it carried on the
    /// far side of the boundary the event crossed.
    fn shown(recorded: &RecordedValue) -> String {
        recorded_value(recorded).to_string()
    }

    /// The value a recorded value carried.
    fn recorded_value(recorded: &RecordedValue) -> Value {
        match recorded {
            RecordedValue::Carried(transfer) => transfer.clone().into_value(),
            RecordedValue::Opaque { shown, .. } => Value::Str(shown.as_str().into()),
        }
    }

    fn registry_with_documents() -> HostRegistry {
        let mut hosts = HostRegistry::new(Grants::new(["documents"]));
        hosts.register(Box::new(Documents::in_memory(BTreeMap::from([(
            "input".to_string(),
            "hello world".to_string(),
        )]))));
        hosts
    }

    #[test]
    fn budget_stops_a_call_before_it_dispatches() {
        use crate::budget::{Budget, Limits};

        let mut hosts = registry_with_documents();
        hosts.set_budget(Budget::new(Limits {
            max_host_calls: Some(0),
            ..Limits::default()
        }));
        let sink = RecordingSink::default();
        hosts.set_trace(Arc::new(sink.clone()));

        let error = hosts
            .call("documents", "read", vec![Value::Str("input".into())])
            .expect_err("the call should be stopped by the budget");
        assert!(error.rule.is_some(), "{error:?}");

        let events = sink.events();
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            TraceEvent::HostCall { granted, .. } => assert!(!granted),
            other => panic!("expected a HostCall event, found {other:?}"),
        }
        assert_eq!(hosts.with_budget(|budget| budget.host_calls()), Some(1));
    }

    #[test]
    fn a_granted_call_produces_one_event_with_a_plausible_wait() {
        let mut hosts = registry_with_documents();
        let sink = RecordingSink::default();
        hosts.set_trace(Arc::new(sink.clone()));

        hosts
            .call("documents", "read", vec![Value::Str("input".into())])
            .expect("the call should be allowed");

        let events = sink.events();
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            TraceEvent::HostCall {
                task,
                module,
                op,
                capability,
                wait,
                granted,
                args,
                outcome,
            } => {
                // Nothing ran a program here, so the call belongs to the
                // entry's own id, which is what a call made outside a task
                // reports.
                assert_eq!(*task, crate::runtime::ENTRY_TASK);
                assert_eq!(module, "documents");
                assert_eq!(op, "read");
                assert_eq!(capability, "documents");
                assert!(*granted);
                assert!(*wait < std::time::Duration::from_secs(1), "{wait:?}");
                // A trace that says only that a call happened cannot replay
                // it, so the event carries the call's arguments and its
                // result too.
                assert_eq!(args.len(), 1);
                assert_eq!(shown(&args[0]), "input");
                match outcome {
                    Some(HostOutcome::Value(value)) => {
                        assert_eq!(ok_str(recorded_value(value)), "hello world")
                    }
                    other => panic!("expected a recorded value, found {other:?}"),
                }
            }
            other => panic!("expected a HostCall event, found {other:?}"),
        }
    }

    /// The schema's `recordable` flag decides whether a result is written
    /// down, and `process.exit` is the one shipped operation it decides
    /// against: replaying it by handing back a value would keep running a
    /// program that had ended.
    #[test]
    fn a_result_is_recorded_only_when_the_schema_says_it_may_be() {
        let mut hosts = HostRegistry::new(Grants::new(["process"]));
        hosts.register(Box::new(crate::process::Process::recorded(
            vec!["one".to_string()],
            BTreeMap::new(),
            crate::process::ProcessLog::new(),
        )));
        let sink = RecordingSink::default();
        hosts.set_trace(Arc::new(sink.clone()));

        hosts.call("process", "args", Vec::new()).expect("granted");
        hosts
            .call("process", "exit", vec![Value::Int(2)])
            .expect("granted");

        let events = sink.events();
        assert_eq!(events.len(), 2, "{events:?}");
        match &events[0] {
            // `process.args` is recordable, so its result is recorded.
            TraceEvent::HostCall {
                outcome: Some(HostOutcome::Value(value)),
                ..
            } => assert_eq!(shown(value), "[one]"),
            other => panic!("expected a recorded value, found {other:?}"),
        }
        match &events[1] {
            TraceEvent::HostCall {
                op,
                args,
                outcome: Some(HostOutcome::NotRecordable),
                ..
            } => {
                assert_eq!(op, "exit");
                // The call itself is still recorded, arguments and all: what
                // the program asked for is exactly the part worth knowing.
                assert_eq!(args.len(), 1);
                assert_eq!(shown(&args[0]), "2");
            }
            other => panic!("expected `not recordable`, found {other:?}"),
        }
    }

    /// A sink that reads nothing is asked for nothing: describing a call's
    /// values costs a copy of each, and an untraced run — whose sink is
    /// [`NullSink`] — should not pay for a description nobody keeps.
    #[test]
    fn a_sink_that_is_not_recording_is_given_no_events() {
        /// Records every event it is given, while saying it will not read
        /// them, exactly as `NullSink` does.
        #[derive(Clone, Default)]
        struct Deaf(RecordingSink);

        impl TraceSink for Deaf {
            fn record(&self, event: TraceEvent) {
                self.0.record(event);
            }

            fn is_recording(&self) -> bool {
                false
            }
        }

        let mut hosts = registry_with_documents();
        let sink = Deaf::default();
        hosts.set_trace(Arc::new(sink.clone()));

        hosts
            .call("documents", "read", vec![Value::Str("input".into())])
            .expect("the call should be allowed");

        assert!(sink.0.events().is_empty(), "{:?}", sink.0.events());
    }

    /// A call the run was not granted never reaches a host, so there is no
    /// result to record — but there is still a request worth recording.
    #[test]
    fn a_refused_call_records_its_arguments_and_no_result() {
        let mut hosts = HostRegistry::new(Grants::new(Vec::<String>::new()));
        hosts.register(Box::new(Documents::in_memory(BTreeMap::new())));
        let sink = RecordingSink::default();
        hosts.set_trace(Arc::new(sink.clone()));

        hosts
            .call("documents", "read", vec![Value::Str("input".into())])
            .expect_err("the call should be rejected");

        let events = sink.events();
        match &events[0] {
            TraceEvent::HostCall {
                granted,
                args,
                outcome,
                ..
            } => {
                assert!(!granted);
                assert_eq!(args.len(), 1);
                assert!(outcome.is_none(), "{outcome:?}");
            }
            other => panic!("expected a HostCall event, found {other:?}"),
        }
    }

    #[test]
    fn an_unknown_operation_lists_the_operations_that_exist() {
        let hosts = registry_with_documents();

        let error = hosts
            .call("documents", "write", Vec::new())
            .expect_err("`documents` has no `write`");
        assert_eq!(
            error.message,
            "host module `documents` has no operation `write`"
        );
        assert_eq!(
            error.help.as_deref(),
            Some("host module `documents` exposes `read`")
        );
    }

    #[test]
    fn too_many_arguments_are_rejected_before_the_host_sees_them() {
        let hosts = registry_with_documents();

        let error = hosts
            .call(
                "documents",
                "read",
                vec![Value::Str("input".into()), Value::Str("extra".into())],
            )
            .expect_err("`documents.read` takes one argument");
        assert_eq!(
            error.message,
            "`documents.read` takes 1 argument, but 2 were given"
        );
        assert_eq!(
            error.help.as_deref(),
            Some("the Host API schema declares `documents.read(String) -> Result<String, Error>`")
        );
    }

    #[test]
    fn too_few_arguments_are_rejected_too() {
        let hosts = registry_with_documents();

        let error = hosts
            .call("documents", "read", Vec::new())
            .expect_err("`documents.read` takes one argument");
        assert_eq!(
            error.message,
            "`documents.read` takes 1 argument, but 0 were given"
        );
    }

    /// `console.println("a", "b")` is one line of two parts, so the arity
    /// check must not reject it.
    #[test]
    fn a_variadic_operation_accepts_any_number_of_arguments() {
        let mut hosts = HostRegistry::new(Grants::new(["console"]));
        hosts.register(Box::new(Console::new(Vec::new(), Vec::new())));

        for arity in 0..3 {
            hosts
                .call("console", "println", vec![Value::Str("part".into()); arity])
                .unwrap_or_else(|e| panic!("arity {arity} should be accepted: {}", e.message));
        }
    }

    /// A writer two owners can share: one of them is inside a [`Console`] and
    /// the other is the assertion.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Shared {
        fn written(&self) -> String {
            String::from_utf8(
                self.0
                    .lock()
                    .expect("nothing panicked while writing")
                    .clone(),
            )
            .expect("a console writes what it was given, which was text")
        }
    }

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("nothing panicked while writing")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A registry holding one [`Console`] over the two writers given, granted
    /// the one capability both of its streams answer to.
    fn registry_with_console(out: Shared, err: Shared) -> HostRegistry {
        let mut hosts = HostRegistry::new(Grants::new(["console"]));
        hosts.register(Box::new(Console::new(out, err)));
        hosts
    }

    /// The whole of what the second stream is for: what a program says about
    /// its output is not in its output.
    #[test]
    fn what_the_diagnostic_stream_is_given_stays_out_of_the_output_stream() {
        let out = Shared::default();
        let err = Shared::default();
        let hosts = registry_with_console(out.clone(), err.clone());

        for (op, text) in [
            ("println", "id,name"),
            ("eprintln", "line 2 is malformed, skipping"),
            ("println", "1,ada"),
        ] {
            hosts
                .call("console", op, vec![Value::Str(text.into())])
                .unwrap_or_else(|e| panic!("`console.{op}` should be allowed: {}", e.message));
        }

        assert_eq!(out.written(), "id,name\n1,ada\n");
        assert_eq!(err.written(), "line 2 is malformed, skipping\n");
    }

    /// `print` and `eprint` differ from `println` and `eprintln` in the same
    /// way on both streams: the newline, and nothing else.
    #[test]
    fn the_unterminated_form_writes_the_same_way_on_either_stream() {
        let out = Shared::default();
        let err = Shared::default();
        let hosts = registry_with_console(out.clone(), err.clone());

        for op in ["print", "eprint"] {
            hosts
                .call(
                    "console",
                    op,
                    vec![Value::Str("two".into()), Value::Str("parts".into())],
                )
                .unwrap_or_else(|e| panic!("`console.{op}` should be allowed: {}", e.message));
        }

        assert_eq!(out.written(), "two parts");
        assert_eq!(err.written(), "two parts");
    }

    /// One capability covers both streams. A run granted `console` may write
    /// a record and may complain about it, which is why a grant written
    /// before `eprintln` existed did not have to be read again when it
    /// arrived; a run granted nothing may do neither, and is refused under
    /// the one name either way.
    #[test]
    fn one_grant_covers_both_streams() {
        let out = Shared::default();
        let err = Shared::default();
        let granted = registry_with_console(out.clone(), err.clone());

        granted
            .call("console", "println", vec![Value::Str("record".into())])
            .expect("`console` grants the output stream");
        granted
            .call("console", "eprintln", vec![Value::Str("warning".into())])
            .expect("`console` grants the diagnostic stream too");
        assert_eq!(out.written(), "record\n");
        assert_eq!(err.written(), "warning\n");

        let mut ungranted = HostRegistry::new(Grants::new(Vec::<String>::new()));
        ungranted.register(Box::new(Console::new(Shared::default(), Shared::default())));
        for op in ["println", "print", "eprintln", "eprint"] {
            let refused = ungranted
                .call("console", op, vec![Value::Str("anything".into())])
                .expect_err("a run granted nothing reaches neither stream");
            assert_eq!(
                refused.denied_capability.as_deref(),
                Some("console"),
                "`console.{op}`: {}",
                refused.message
            );
        }
    }

    #[test]
    fn task_safety_of_a_host_result_comes_from_the_schema() {
        let hosts = registry_with_documents();

        assert_eq!(hosts.result_is_task_safe("documents", "read"), Some(true));
        assert_eq!(hosts.result_is_task_safe("documents", "write"), None);
        assert_eq!(hosts.result_is_task_safe("network", "read"), None);
    }

    /// A host used only to give two modules the same name, so
    /// [`module_schemas_describes_the_module_dispatch_will_actually_reach`]
    /// can tell which one a lookup actually reached: its answer to `ping` is
    /// baked in rather than computed, so the test reads it off the return
    /// value instead of having to ask the host anything more.
    struct DuplicateNamed {
        schema: ModuleSchema,
        answer: &'static str,
    }

    impl HostApi for DuplicateNamed {
        fn module_schema(&self) -> ModuleSchema {
            self.schema
        }

        fn call(&self, _op: &str, _args: Vec<Value>) -> Result<Value, RuntimeError> {
            Ok(Value::Str(self.answer.into()))
        }
    }

    static DUPLICATE_FIRST_OPERATIONS: &[OperationSchema] = &[OperationSchema {
        name: "ping",
        params: &[],
        variadic: false,
        result: HostType::String,
        capability: "duplicate-first",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }];

    static DUPLICATE_FIRST: ModuleSchema = ModuleSchema {
        name: "duplicate",
        capability: "duplicate-first",
        operations: DUPLICATE_FIRST_OPERATIONS,
        types: &[],
        resources: &[],
    };

    static DUPLICATE_SECOND_OPERATIONS: &[OperationSchema] = &[OperationSchema {
        name: "ping",
        params: &[],
        variadic: false,
        result: HostType::String,
        capability: "duplicate-second",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    }];

    static DUPLICATE_SECOND: ModuleSchema = ModuleSchema {
        name: "duplicate",
        capability: "duplicate-second",
        operations: DUPLICATE_SECOND_OPERATIONS,
        types: &[],
        resources: &[],
    };

    /// [`HostRegistry::module_schemas`] hands the checker one schema per
    /// name, and every dispatch method resolves a duplicate name by taking
    /// the first module registered under it — so the schema this hands out
    /// for a duplicated name had better be the first module's, the same one
    /// [`HostRegistry::call`] reaches, or the checker would be checking a
    /// call against a module the runtime never dispatches to.
    #[test]
    fn module_schemas_describes_the_module_dispatch_will_actually_reach() {
        let mut hosts = HostRegistry::new(Grants::new(["duplicate-first", "duplicate-second"]));
        hosts.register(Box::new(DuplicateNamed {
            schema: DUPLICATE_FIRST,
            answer: "first",
        }));
        hosts.register(Box::new(DuplicateNamed {
            schema: DUPLICATE_SECOND,
            answer: "second",
        }));

        assert_eq!(hosts.module_schemas(), vec![DUPLICATE_FIRST]);

        match hosts
            .call("duplicate", "ping", Vec::new())
            .expect("the call should be allowed")
        {
            Value::Str(text) => assert_eq!(&*text, "first"),
            other => panic!("expected a Str, found {other:?}"),
        }
    }

    /// Every module a run registers declares itself out of
    /// [`cove_schema::hosts::SHIPPED`] rather than out of a table of its own.
    ///
    /// This is what makes "one description shared by the compiler, runtime,
    /// and CLI" a fact rather than an intention. `cove-sema` reads that table
    /// and never sees a host, so a module answering with anything else would
    /// be a second description with nothing holding it against the first —
    /// which is exactly how `http` once came to be missing from the
    /// compiler's list of host modules with no diagnostic anywhere. A test
    /// used to compare the two lists; there is one list now, and this is what
    /// keeps it one.
    ///
    /// A module agreeing with the table in isolation is not the same claim
    /// as a registry holding every shipped module agreeing with it too, so
    /// this also registers all eight and checks the registry's own dispatch
    /// tables — `contains`, `schema_for`, `host_type` — against the same
    /// table, the way [`HostRegistry::call`] and [`HostRegistry::call_with`]
    /// read them.
    #[test]
    fn every_module_a_run_registers_declares_itself_out_of_the_shared_schema() {
        let modules: Vec<Box<dyn HostApi>> = vec![
            Box::new(Console::new(std::io::sink(), std::io::sink())),
            Box::new(Env::new(BTreeMap::new())),
            Box::new(Documents::in_memory(BTreeMap::new())),
            Box::new(crate::clock::Clock::real()),
            Box::new(crate::files::Files::in_memory(BTreeMap::new())),
            Box::new(crate::process::Process::real(Vec::new(), Vec::new())),
            Box::new(crate::database::Database::denied()),
            Box::new(crate::http::Http::real()),
        ];

        assert_eq!(
            modules.len(),
            shipped_schema().len(),
            "a module the schema describes is one a run registers, and the reverse"
        );
        for (module, declared) in modules.iter().zip(shipped_schema()) {
            assert_eq!(&module.module_schema(), declared, "`{}`", declared.name);
        }

        let mut hosts = HostRegistry::new(Grants::new(Vec::<String>::new()));
        for module in modules {
            hosts.register(module);
        }
        for declared in shipped_schema() {
            assert!(hosts.contains(declared.name), "`{}`", declared.name);
            for op in declared.operations {
                assert_eq!(
                    hosts.schema_for(declared.name, op.name),
                    Some(op),
                    "`{}.{}`",
                    declared.name,
                    op.name
                );
            }
            for ty in declared.types {
                assert_eq!(
                    hosts.host_type(declared.name, ty.name),
                    Some(*ty),
                    "`{}.{}`",
                    declared.name,
                    ty.name
                );
            }
        }
    }

    /// The counter behind `cove run --stats`, and the one thing that reads
    /// an operation's declared `effect`.
    #[test]
    fn only_the_calls_the_schema_calls_irreversible_are_counted() {
        let mut hosts = HostRegistry::new(Grants::new(["console", "files"]));
        hosts.register(Box::new(Console::new(Vec::new(), Vec::new())));
        hosts.register(Box::new(crate::files::Files::in_memory(BTreeMap::new())));
        assert_eq!(hosts.irreversible_writes(), 0);

        hosts
            .call("files", "exists", vec![Value::Str("a.txt".into())])
            .expect("the call should be allowed");
        assert_eq!(hosts.irreversible_writes(), 0);

        hosts
            .call(
                "files",
                "write",
                vec![Value::Str("a.txt".into()), Value::Str("x".into())],
            )
            .expect("the call should be allowed");
        hosts
            .call("console", "println", vec![Value::Str("a".into())])
            .expect("the call should be allowed");
        assert_eq!(hosts.irreversible_writes(), 2);
    }

    /// A call the run was not granted never reaches the host, so it never
    /// changed anything to count.
    #[test]
    fn an_ungranted_irreversible_call_is_not_counted() {
        let mut hosts = HostRegistry::new(Grants::new(Vec::<String>::new()));
        hosts.register(Box::new(crate::files::Files::in_memory(BTreeMap::new())));

        hosts
            .call(
                "files",
                "write",
                vec![Value::Str("a.txt".into()), Value::Str("x".into())],
            )
            .expect_err("the call should be rejected");
        assert_eq!(hosts.irreversible_writes(), 0);
    }

    #[test]
    fn a_denied_call_produces_an_event_with_granted_false() {
        let mut hosts = HostRegistry::new(Grants::new(Vec::<String>::new()));
        hosts.register(Box::new(Documents::in_memory(BTreeMap::new())));
        let sink = RecordingSink::default();
        hosts.set_trace(Arc::new(sink.clone()));

        hosts
            .call("documents", "read", vec![Value::Str("input".into())])
            .expect_err("the call should be rejected for the missing grant");

        let events = sink.events();
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            TraceEvent::HostCall {
                capability,
                granted,
                ..
            } => {
                assert_eq!(capability, "documents");
                assert!(!granted);
            }
            other => panic!("expected a HostCall event, found {other:?}"),
        }
    }

    // ----------------------------------------------- a host and its schema
    //
    // ADR 0001 makes the schema shared property: "A machine-readable Host API
    // schema is shared by the compiler, runtime, and CLI. Each operation
    // describes its argument, result, and error types". Every shipped host is
    // written to obey its own, which is what makes them useless for asking
    // what happens when one does not. These tests register a host that can be
    // told to disagree with its declaration, and pin which disagreements the
    // boundary catches.

    /// The operation [`Wayward`] declares.
    static WAYWARD_SCHEMA: &[OperationSchema] = &[
        OperationSchema {
            name: "read",
            params: &[HostType::String],
            variadic: false,
            result: HostType::Result(&HostType::String, &HostType::Error),
            capability: "wayward",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        OperationSchema {
            name: "open",
            params: &[],
            variadic: false,
            result: HostType::Named("wayward.Handle"),
            capability: "wayward",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        // A result with something inside it, so a disagreement can be nested
        // rather than sitting at the top of the value.
        OperationSchema {
            name: "list",
            params: &[],
            variadic: false,
            result: HostType::Result(&HostType::Array(&HostType::String), &HostType::Error),
            capability: "wayward",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        // An operation that declares nothing about what it produces, which is
        // what `clock.timeout` and `clock.every` declare about the work they
        // are given.
        OperationSchema {
            name: "anything",
            params: &[],
            variadic: false,
            result: HostType::Any,
            capability: "wayward",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        // An argument with something inside it, so a disagreement can be
        // nested there too.
        OperationSchema {
            name: "send",
            params: &[HostType::Array(&HostType::String)],
            variadic: false,
            result: HostType::Result(&HostType::String, &HostType::Error),
            capability: "wayward",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        // One declared parameter answering for as many arguments as the call
        // makes, which is what `console.println` declares.
        OperationSchema {
            name: "say",
            params: &[HostType::String],
            variadic: true,
            result: HostType::Result(&HostType::String, &HostType::Error),
            capability: "wayward",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
        // An operation that declares nothing about what it is *given*, which
        // is what `clock.timeout` declares of the work it bounds.
        OperationSchema {
            name: "bound",
            params: &[HostType::Any],
            variadic: false,
            result: HostType::Result(&HostType::String, &HostType::Error),
            capability: "wayward",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        },
    ];

    /// The one kind of resource [`Wayward`] declares it can open.
    static WAYWARD_RESOURCES: &[ResourceSchema] = &[ResourceSchema {
        name: "Handle",
        task_safe: true,
        operations: &[OperationSchema {
            name: "close",
            params: &[],
            variadic: false,
            result: HostType::Result(&HostType::Unit, &HostType::Error),
            capability: "wayward",
            effect: Effect::ReversibleWrite,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        }],
    }];

    /// A kind of resource [`Wayward`] does *not* declare, which it mints a
    /// handle for anyway.
    static UNDECLARED_RESOURCE: ResourceSchema = ResourceSchema {
        name: "Ghost",
        task_safe: true,
        operations: &[],
    };

    /// What [`Wayward`] answers with.
    ///
    /// A host holds data and builds its answer at the call, because a
    /// [`Value`] is reference-counted and belongs to the thread that built it
    /// while a host is shared by every task of a run. So this says which
    /// answer to build rather than holding one.
    #[derive(Clone, Copy)]
    enum Answer {
        /// What each operation declares: `Ok("what was declared")`, an
        /// `Ok` of an array of strings, and a handle of the one resource
        /// kind this module says it can open.
        Declared,
        /// The same operations, each answering something its own declaration
        /// does not admit: an `Int` where a `Result` was declared, an array
        /// with an `Int` among its strings, and a handle naming a resource
        /// kind this module never declared.
        Undeclared,
    }

    /// A host whose behaviour can be made to disagree with its schema.
    ///
    /// It counts how often it was reached, so a test can tell a call the
    /// registry refused from one it dispatched: "before the host sees them"
    /// is a claim about where the check happens, not only about what it says.
    struct Wayward {
        answer: Answer,
        calls: Arc<AtomicU64>,
    }

    impl Wayward {
        fn answering(answer: Answer) -> (Wayward, Arc<AtomicU64>) {
            let calls = Arc::new(AtomicU64::new(0));
            (
                Wayward {
                    answer,
                    calls: Arc::clone(&calls),
                },
                calls,
            )
        }
    }

    /// The module this test host declares itself with, which is the one
    /// the registry holds it to.
    const WAYWARD: ModuleSchema = ModuleSchema {
        name: "wayward",
        capability: "wayward",
        operations: WAYWARD_SCHEMA,
        types: &[],
        resources: WAYWARD_RESOURCES,
    };

    impl HostApi for Wayward {
        fn module_schema(&self) -> ModuleSchema {
            WAYWARD
        }

        fn call(&self, op: &str, _args: Vec<Value>) -> Result<Value, RuntimeError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let declared = matches!(self.answer, Answer::Declared);
            Ok(match op {
                // A handle naming a resource this module's schema does not
                // declare: the value is well formed and the name is a lie.
                "open" => Value::Resource(ResourceHandle::new(
                    "wayward",
                    if declared {
                        &WAYWARD_RESOURCES[0]
                    } else {
                        &UNDECLARED_RESOURCE
                    },
                    1,
                )),
                "list" => Value::ok(Value::Array(if declared {
                    vec![Value::Str("what was declared".into())].into()
                } else {
                    vec![Value::Str("what was declared".into()), Value::Int(3)].into()
                })),
                // Declared as `Any`, so this one cannot disagree with itself.
                "anything" => Value::Int(3),
                _ if declared => Value::ok(Value::Str("what was declared".into())),
                _ => Value::Int(3),
            })
        }

        fn call_resource(
            &self,
            _handle: &ResourceHandle,
            _op: &str,
            _args: Vec<Value>,
            _back: &mut dyn Reentry,
        ) -> Result<Value, RuntimeError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(match self.answer {
                Answer::Declared => Value::ok(Value::Unit),
                Answer::Undeclared => Value::ok(Value::Str("not the declared `Unit`".into())),
            })
        }
    }

    /// A registry holding one [`Wayward`], with its capability granted and
    /// its trace recorded.
    fn registry_with_wayward(answer: Answer) -> (HostRegistry, Arc<AtomicU64>, RecordingSink) {
        let (host, calls) = Wayward::answering(answer);
        let mut hosts = HostRegistry::new(Grants::new(["wayward"]));
        hosts.register(Box::new(host));
        let sink = RecordingSink::default();
        hosts.set_trace(Arc::new(sink.clone()));
        (hosts, calls, sink)
    }

    /// The success half: a host that answers what it declared is dispatched,
    /// its value reaches the caller unchanged, and the trace records it.
    #[test]
    fn a_host_that_answers_what_its_schema_declares_is_dispatched_and_recorded() {
        let (hosts, calls, sink) = registry_with_wayward(Answer::Declared);

        let value = hosts
            .call("wayward", "read", vec![Value::Str("input".into())])
            .expect("a conforming call is dispatched");

        assert_eq!(ok_str(value), "what was declared");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let events = sink.events();
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            TraceEvent::HostCall {
                op,
                granted,
                outcome,
                ..
            } => {
                assert_eq!(op, "read");
                assert!(granted);
                match outcome {
                    Some(HostOutcome::Value(recorded)) => {
                        assert_eq!(shown(recorded), "Ok(what was declared)")
                    }
                    other => panic!("expected a recorded value, found {other:?}"),
                }
            }
            other => panic!("expected a HostCall event, found {other:?}"),
        }
    }

    /// A handle is a name, and the boundary trusts no name it is given: a
    /// `wayward.Ghost` names a resource kind the module's own `resources()`
    /// does not declare, so an operation on it is refused without the host
    /// being asked.
    #[test]
    fn a_handle_naming_a_resource_the_module_never_declared_is_refused() {
        let (hosts, calls, _) = registry_with_wayward(Answer::Declared);
        let ghost = ResourceHandle::new("wayward", &UNDECLARED_RESOURCE, 1);

        let error = hosts
            .call_resource(&ghost, "close", Vec::new(), &mut NoReentry)
            .expect_err("a handle the schema does not declare is refused");

        assert_eq!(
            error.message,
            "host module `wayward` issues no `Ghost` handles"
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "the host was not asked to act on a handle its schema disowns"
        );
    }

    /// The declared kind of handle passes, and the operations it declares can
    /// then be called on it: the check refuses a name, not handles.
    #[test]
    fn a_handle_of_the_kind_the_operation_declared_is_admitted() {
        let (hosts, _, _) = registry_with_wayward(Answer::Declared);

        let opened = hosts
            .call("wayward", "open", Vec::new())
            .expect("a handle of the declared kind is admitted");
        let Value::Resource(handle) = opened else {
            panic!("expected a resource handle, found {opened}");
        };
        assert_eq!(handle.qualified_type(), "wayward.Handle");

        let closed = hosts
            .call_resource(&handle, "close", Vec::new(), &mut NoReentry)
            .expect("the handle answers the operation its kind declares");
        assert!(matches!(closed, Value::Enum(_)), "{closed}");
    }

    /// An operation the schema does not declare is refused at the boundary,
    /// and the diagnostic lists what the module does declare rather than
    /// leaving the caller to guess.
    #[test]
    fn an_operation_the_schema_does_not_declare_is_refused_before_the_host_sees_it() {
        let (hosts, calls, _) = registry_with_wayward(Answer::Declared);

        let error = hosts
            .call("wayward", "write", vec![Value::Str("input".into())])
            .expect_err("an undeclared operation is refused");

        assert_eq!(
            error.message,
            "host module `wayward` has no operation `write`"
        );
        let help = error.help.expect("the diagnostic lists what does exist");
        assert!(help.contains("`read`"), "{help}");
        assert!(help.contains("`open`"), "{help}");
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    /// Arity is the first part of an operation's declared shape the boundary
    /// enforces, and it enforces it before the host is reached.
    #[test]
    fn arguments_the_schema_does_not_accept_are_refused_before_the_host_sees_them() {
        let (hosts, calls, _) = registry_with_wayward(Answer::Declared);

        let error = hosts
            .call("wayward", "read", Vec::new())
            .expect_err("a call with too few arguments is refused");

        assert_eq!(
            error.message,
            "`wayward.read` takes 1 argument, but 0 were given"
        );
        let help = error.help.expect("the diagnostic quotes the schema");
        assert!(
            help.contains("wayward.read(String) -> Result<String, Error>"),
            "{help}"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    /// An argument the operation's own declaration does not admit is refused
    /// before the host sees it, and nothing of the call is recorded: a call
    /// stopped here never happened.
    ///
    /// This is the same table as the result check, read from the other side.
    /// `cove check` reports this mistake at the call site, where it has a
    /// span; the boundary reports it for the hosts the checker cannot see —
    /// which is every host an embedder writes.
    #[test]
    fn an_argument_the_schema_does_not_admit_is_refused_before_the_host_sees_it() {
        let (hosts, calls, sink) = registry_with_wayward(Answer::Declared);

        let error = hosts
            .call("wayward", "read", vec![Value::Int(3)])
            .expect_err("an `Int` where a `String` was declared is refused");

        assert_eq!(
            error.message,
            "`wayward.read` was given `Int` as argument 1, but its schema declares `String` there"
        );
        let help = error.help.expect("the diagnostic quotes the schema");
        assert!(
            help.contains("wayward.read(String) -> Result<String, Error>"),
            "{help}"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(
            sink.events().is_empty(),
            "a call the boundary refused never reached the host to be recorded"
        );
    }

    /// An argument is followed as far down as a result is, so an `Int` among
    /// the strings of a declared `Array<String>` is caught and the diagnostic
    /// says which element.
    #[test]
    fn a_violation_inside_an_argument_says_where_it_is() {
        let (hosts, calls, _) = registry_with_wayward(Answer::Declared);

        let error = hosts
            .call(
                "wayward",
                "send",
                vec![Value::Array(
                    vec![Value::Str("one".into()), Value::Int(2)].into(),
                )],
            )
            .expect_err("an `Int` among the declared strings is refused");

        assert_eq!(
            error.message,
            "`wayward.send` was given `Int` at `[1]` of argument 1, but its schema declares `String` there"
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    /// A variadic operation's one declared parameter answers for every
    /// argument from its own position onwards, so the fourth `say` is checked
    /// against the same `String` the first one was.
    #[test]
    fn a_variadic_operation_checks_every_argument_against_its_declared_type() {
        let (hosts, _, _) = registry_with_wayward(Answer::Declared);

        hosts
            .call(
                "wayward",
                "say",
                vec![Value::Str("a".into()), Value::Str("b".into())],
            )
            .expect("strings all the way along are admitted");

        let error = hosts
            .call(
                "wayward",
                "say",
                vec![Value::Str("a".into()), Value::Int(2)],
            )
            .expect_err("an `Int` among the declared strings is refused");
        assert_eq!(
            error.message,
            "`wayward.say` was given `Int` as argument 2, but its schema declares `String` there"
        );
    }

    /// `Any` admits whatever it is given on the way in for the same reason it
    /// does on the way out: it is the type of an operation whose meaning does
    /// not depend on which value it was handed.
    #[test]
    fn an_argument_declared_any_admits_whatever_it_is_given() {
        let (hosts, calls, _) = registry_with_wayward(Answer::Declared);

        for argument in [Value::Int(3), Value::Unit, Value::Str("text".into())] {
            hosts
                .call("wayward", "bound", vec![argument])
                .expect("`Any` admits everything");
        }
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }

    /// A host whose *result* violates its declared type is refused, and the
    /// diagnostic names the host that broke its word rather than the Cove
    /// code that would have received the value.
    ///
    /// `wayward.read` declares `Result<String, Error>` and answers `3`. ADR
    /// 0001 asks the schema to describe "argument, result, and error types"
    /// and to be "shared by the compiler, runtime, and CLI", and a
    /// description nothing enforces is a comment. The trace still records
    /// what the host did: the check refuses the value, not the fact.
    #[test]
    fn a_result_that_violates_its_declared_type_is_refused() {
        let (hosts, calls, sink) = registry_with_wayward(Answer::Undeclared);

        let error = hosts
            .call("wayward", "read", vec![Value::Str("input".into())])
            .expect_err("a host that breaks its own schema is refused");

        assert_eq!(
            error.message,
            "`wayward.read` answered `Int`, but its schema declares `Result<String, Error>`"
        );
        let help = error.help.expect("the diagnostic quotes the schema");
        assert!(
            help.contains("wayward.read(String) -> Result<String, Error>"),
            "{help}"
        );
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "the host was reached: this is a check on what it answered"
        );
        let events = sink.events();
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            TraceEvent::HostCall {
                granted, outcome, ..
            } => {
                assert!(granted);
                match outcome {
                    Some(HostOutcome::Value(recorded)) => assert_eq!(shown(recorded), "3"),
                    other => panic!("expected the answer on the record, found {other:?}"),
                }
            }
            other => panic!("expected a HostCall event, found {other:?}"),
        }
    }

    /// The check follows the declared type's own recursion, so an array of
    /// the declared shape holding one element that is not is caught, and the
    /// diagnostic says which element.
    #[test]
    fn a_violation_inside_a_declared_result_says_where_it_is() {
        let (hosts, _, _) = registry_with_wayward(Answer::Undeclared);

        let error = hosts
            .call("wayward", "list", Vec::new())
            .expect_err("an `Int` among the declared strings is refused");

        assert_eq!(
            error.message,
            "`wayward.list` answered `Int` at `Ok(_)[1]` of its result, but its schema declares `String` there"
        );
    }

    /// `Any` is not a missing type but the type of an operation whose meaning
    /// does not depend on which value it was given, so it admits whatever the
    /// host answers — including the `Int` every other declaration here
    /// refuses.
    #[test]
    fn a_result_declared_any_admits_whatever_the_host_answers() {
        for answer in [Answer::Declared, Answer::Undeclared] {
            let (hosts, _, _) = registry_with_wayward(answer);

            let value = hosts
                .call("wayward", "anything", Vec::new())
                .expect("`Any` admits everything");

            assert!(matches!(value, Value::Int(3)), "{value}");
        }
    }

    /// A `Named` result is checked by the name the value carries, and a
    /// handle carries the module and kind it was issued for. So the lie the
    /// boundary used to catch only at the *next* call — a handle naming a
    /// resource kind the module never declared — is caught where it is told.
    #[test]
    fn a_handle_naming_a_kind_the_operation_did_not_declare_is_refused() {
        let (hosts, _, _) = registry_with_wayward(Answer::Undeclared);

        let error = hosts
            .call("wayward", "open", Vec::new())
            .expect_err("a handle of an undeclared kind is refused where it is answered");

        assert_eq!(
            error.message,
            "`wayward.open` answered `wayward.Ghost`, but its schema declares `wayward.Handle`"
        );
    }

    /// A resource operation passes through the same choke point as a module
    /// operation, so it is held to its declaration the same way, and the
    /// diagnostic names it the way Cove source does.
    #[test]
    fn a_resource_operation_is_held_to_its_declaration_too() {
        let (hosts, _, _) = registry_with_wayward(Answer::Undeclared);
        let handle = ResourceHandle::new("wayward", &WAYWARD_RESOURCES[0], 1);

        let error = hosts
            .call_resource(&handle, "close", Vec::new(), &mut NoReentry)
            .expect_err("a handle's operation that breaks its own schema is refused");

        assert_eq!(
            error.message,
            "`wayward.Handle.close` answered `String` at `Ok(_)` of its result, but its schema declares `Unit` there"
        );
        let help = error.help.expect("the diagnostic quotes the schema");
        assert!(
            help.contains("wayward.Handle.close() -> Result<Unit, Error>"),
            "{help}"
        );
    }
}
