//! Task scopes, task handles, and the task-safety rule at their boundary.
//!
//! The Language Card states the whole contract this module implements:
//!
//! > Concurrent work belongs to a task scope. Leaving the scope waits for or
//! > cancels its child tasks. Immutable task-safe values such as arrays may
//! > cross task boundaries. A vector cannot cross, even through `let`; finish
//! > it as an array or wrap mutable state in `Shared` or another synchronized
//! > type. Closures are task-safe only when every capture is.
//!
//! ADR 0008 runs a spawned task on a thread of its own, so a handle here owns
//! that thread: the body starts when the task is created and the value it
//! produces is reachable only by joining it, which is what `await` and
//! leaving a scope both do. The state machine still holds no scheduling
//! policy — [`Tasking`] is what decides when a task is joined, and both
//! evaluators reach it — because a value is observable through `await` or
//! scope exit and through nothing else.
//!
//! A task's handle belongs to the thread that spawned it: [`Task`] is `Rc`
//! and its state is a [`RefCell`], because only the spawning task ever
//! touches it. What crosses the boundary is the body on the way in and the
//! value on the way out, both as a [`Transfer`], plus a [`Cancellation`] the
//! child observes at its own safepoints.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cove_diag::Span;
use cove_syntax::ast::{FnDecl, Param};

use crate::budget::Cancellation;
use crate::error::RuntimeError;
use crate::host::{HostRegistry, ResourceHandle};
use crate::runtime::Runtime;
use crate::shared::SharedCell;
use crate::trace::TraceEvent;
use crate::value::{
    Closure, ClosureBody, DynValue, EnumValue, HostFnValue, MapKey, StructValue, Value,
};

/// What a task thread hands back to the task that spawned it: the value the
/// body produced, in the form that may cross the boundary, or why it stopped.
pub type TaskOutcome = Result<Transfer, RuntimeError>;

/// What a spawned task has done so far.
#[derive(Debug)]
pub enum TaskState {
    /// The body is running on its own thread, which has not been joined yet.
    Running,
    /// The body produced a value. Awaiting again returns the same value.
    Settled(Value),
    /// The body raised a [`RuntimeError`]. Awaiting again raises the same one.
    Failed(RuntimeError),
    /// The task was cancelled: its own flag was raised, and it stopped at the
    /// next safepoint rather than finishing. Awaiting a cancelled task is an
    /// error.
    Cancelled,
}

/// A spawned unit of work and the value it will produce.
///
/// The value is reachable only through [`TaskState::Settled`], so no caller
/// can observe it without going through `await` or scope exit.
pub struct Task {
    /// Trace identity, unique across the run. Zero for a task that was
    /// already settled when it was created, which never appears in a trace
    /// because it never ran as a task.
    pub id: u64,
    /// The name of the scope that owns this task, for diagnostics.
    pub scope: Rc<str>,
    /// Position in spawn order within that scope, counting from one.
    pub position: usize,
    pub state: RefCell<TaskState>,
    /// The thread running the body, until something joins it.
    thread: RefCell<Option<JoinHandle<TaskOutcome>>>,
    /// This task's own cancellation flag, raised by `cancel()` or by leaving
    /// its scope early.
    ///
    /// It is separate from the run's flag on purpose: cancelling one task
    /// stops that task, while cancelling the run stops everything. The child
    /// observes both, since its safepoints charge the run's budget and check
    /// this flag.
    cancellation: Cancellation,
}

impl Task {
    /// A task whose body is already running on `thread`.
    pub fn running(
        id: u64,
        scope: Rc<str>,
        position: usize,
        cancellation: Cancellation,
        thread: JoinHandle<TaskOutcome>,
    ) -> Rc<Task> {
        Rc::new(Task {
            id,
            scope,
            position,
            state: RefCell::new(TaskState::Running),
            thread: RefCell::new(Some(thread)),
            cancellation,
        })
    }

    /// A task whose value is already known.
    ///
    /// An `async fn` is called like any other function and runs its body at
    /// the call site, so the handle it returns is settled on creation. ADR
    /// 0008 gives a thread to `spawn`, which is where the language says
    /// concurrency begins; nothing may depend on when an `async fn` body ran,
    /// only on the value `await` produces.
    pub fn settled(value: Value) -> Rc<Task> {
        Rc::new(Task {
            id: 0,
            scope: "this call".into(),
            position: 0,
            state: RefCell::new(TaskState::Settled(value)),
            thread: RefCell::new(None),
            cancellation: Cancellation::new(),
        })
    }

    /// Whether the body is still running on its own thread.
    pub fn is_running(&self) -> bool {
        matches!(&*self.state.borrow(), TaskState::Running)
    }

    /// Whether this task's own cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// Asks this task to stop at its next safepoint.
    ///
    /// A task that has already finished is unaffected: cancellation stops
    /// work that has not happened, it does not undo work that has. Nothing
    /// here waits — [`Task::join`] is what waits — because a scope cancels
    /// all of its children before waiting for any of them.
    pub fn cancel(&self) {
        if self.is_running() {
            self.cancellation.cancel();
        }
    }

    /// Waits for the body's thread and records what it produced, unless the
    /// task has already been joined.
    ///
    /// A task's body runs at most once and is joined at most once, so
    /// awaiting the same handle twice returns the same value and repeats no
    /// effect.
    pub fn join(&self) {
        let Some(thread) = self.thread.borrow_mut().take() else {
            return;
        };
        let outcome = match thread.join() {
            Ok(outcome) => outcome,
            // A panic is a broken invariant in the task's own thread. The
            // panic message has already reached stderr; what the spawning
            // task needs is an error rather than a value that never arrived.
            Err(_) => Err(RuntimeError::new(format!(
                "{} ended in a broken invariant",
                self.describe()
            ))),
        };
        *self.state.borrow_mut() = match outcome {
            Ok(value) => TaskState::Settled(value.into_value()),
            // A task that stopped after its own cancellation was requested is
            // cancelled, not failed: that is the stop the scope asked for.
            Err(_) if self.is_cancelled() => TaskState::Cancelled,
            Err(error) => TaskState::Failed(error),
        };
    }

    /// How this task is named in diagnostics.
    pub fn describe(&self) -> String {
        if self.position == 0 {
            "this task".to_string()
        } else {
            format!("task {} of scope `{}`", self.position, self.scope)
        }
    }
}

/// A task shows as what it is, never as the value it will produce: that value
/// is observable through `await` or scope exit and through nothing else.
impl std::fmt::Debug for Task {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

/// The task scope `scope name { ... }` binds.
///
/// The scope owns every task spawned into it, which is what lets leaving the
/// scope wait for or cancel its children.
#[derive(Debug)]
pub struct TaskScope {
    /// The name the scope is bound to, for diagnostics.
    pub name: Rc<str>,
    /// Child tasks in spawn order.
    pub tasks: RefCell<Vec<Rc<Task>>>,
    /// Set once the scope has been left; a handle that outlives its scope can
    /// no longer spawn into it.
    closed: Cell<bool>,
}

impl TaskScope {
    pub fn new(name: Rc<str>) -> Rc<TaskScope> {
        Rc::new(TaskScope {
            name,
            tasks: RefCell::new(Vec::new()),
            closed: Cell::new(false),
        })
    }

    /// Adopts a task that is already running, and returns its handle.
    pub fn adopt(&self, task: Rc<Task>) -> Rc<Task> {
        self.tasks.borrow_mut().push(task.clone());
        task
    }

    /// The position the next task spawned into this scope will have.
    pub fn next_position(&self) -> usize {
        self.tasks.borrow().len() + 1
    }

    /// The task at `index` in spawn order, if the scope has one.
    pub fn task_at(&self, index: usize) -> Option<Rc<Task>> {
        self.tasks.borrow().get(index).cloned()
    }

    /// Asks every child that is still running to stop.
    pub fn cancel_running(&self) {
        for task in self.tasks.borrow().iter() {
            task.cancel();
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.get()
    }

    pub fn close(&self) {
        self.closed.set(true);
    }
}

// ------------------------------------------------------- both backends

/// What an evaluator has to answer for a task to be spawned into a scope,
/// waited for, and charged for.
///
/// Everything below this trait is the same on both backends by construction
/// rather than by agreement. A task-safety rule, a budget charge, or a trace
/// event stated twice is one that will drift, and a concurrency bug that only
/// one backend has is the kind that does not reproduce — so `spawn`, `await`,
/// and leaving a scope are written once here, and the two things that really
/// differ are the two this trait asks about: which evaluator runs a body, and
/// which timing contexts a wait is charged against.
///
/// [`crate::interp::Interpreter`] and [`crate::vm::Vm`] are the two
/// implementations, and ADR 0008's "each task gets an evaluator of its own" is
/// what makes them symmetrical: each holds a run's [`Runtime`], a stack of
/// timings, and the id of the task it is running.
pub(crate) trait Tasking {
    /// What every thread of this run shares, which is what a `spawn` hands
    /// the thread it starts.
    fn runtime(&self) -> &Runtime;

    /// The host boundary, which owns the run's budget.
    fn hosts(&self) -> &HostRegistry;

    /// Records `wait` against every timing context this body is inside.
    ///
    /// A body blocked on `await` is doing nothing, exactly as a body blocked
    /// on a host call is, so the two are charged the same way and a trace can
    /// tell a scope that waited for two tasks from one that computed for as
    /// long as they ran.
    fn charge_wait(&mut self, wait: Duration);

    /// The task whose body this evaluator is running, or `None` for the
    /// entry, so that a nested `spawn` can name its immediate parent.
    fn running_task(&self) -> Option<u64>;
}

/// `scope.spawn { ... }`: starts a thread for `body` and hands back the
/// handle the scope now owns.
///
/// Converting the closure for the new thread *is* the task-safety check:
/// what may cross a task boundary is exactly what a thread can own, so a
/// capture that may not cross is reported at the `spawn` that would have
/// carried it, before any thread exists.
///
/// This returns once the thread exists and orders nothing else: whether the
/// child has run an instruction by the time the parent's next statement runs
/// is the operating system's answer, not this runtime's. A rendezvous here
/// would be a scheduling policy, which ADR 0008's amendment refuses for the
/// same reason the concurrency limit below refuses to wait.
///
/// `run` is the whole of what a backend contributes: it receives a
/// [`Runtime`] of its own, the id, the flag the body observes, and the body
/// in the form that crossed, and it evaluates it on the new thread. The
/// evaluator it builds there is the receiving task's; nothing of this one
/// crosses, because nothing of this one could.
pub(crate) fn spawn_into<H: Tasking>(
    host: &mut H,
    scope: &Rc<TaskScope>,
    body: Value,
    span: Span,
    run: impl FnOnce(Runtime, u64, Cancellation, Transfer, Span) -> TaskOutcome + Send + 'static,
) -> Result<Value, RuntimeError> {
    if scope.is_closed() {
        return Err(RuntimeError::new(format!(
            "scope `{}` has already been left, so it can take no more tasks",
            scope.name
        ))
        .at(span)
        .with_rule("Leaving a task scope waits for or cancels its child tasks."));
    }
    if !matches!(body, Value::Closure(_)) {
        return Err(RuntimeError::new(format!(
            "`spawn` takes the work to run as a trailing closure, but found `{}`",
            body.type_name()
        ))
        .at(span)
        .with_help(format!("write `{}.spawn {{ ... }}`", scope.name)));
    }
    let body = Transfer::of(&body).map_err(|found| {
        RuntimeError::new(format!(
            "`spawn` cannot capture `{}`, which is a `{}`",
            found.path, found.type_name
        ))
        .at(span)
        .with_rule(TASK_SAFETY_RULE)
        .with_help(found.help("spawning"))
    })?;

    // Charged before this task is given an id, an event, or a thread: a
    // thread that has started is a resource already taken, which no later
    // safepoint could refuse. A run past its concurrency limit is stopped
    // here the way an exhausted fuel budget stops one, rather than made to
    // wait for a sibling to end, because waiting would be a scheduling
    // policy and ADR 0008 has none.
    if let Some(Err(error)) = host.hosts().with_budget(|budget| {
        budget
            .charge_task()
            .map_err(|stopped| budget.to_runtime_error(stopped))
    }) {
        return Err(error.at(span));
    }

    let runtime = host.runtime().clone();
    let id = runtime.next_task_id();
    // Traced before the thread starts, so a task is never seen completing
    // before it was seen spawning.
    runtime.trace(TraceEvent::TaskSpawned {
        id,
        parent: host.running_task(),
        scope: scope.name.to_string(),
    });

    let cancellation = Cancellation::new();
    let flag = cancellation.clone();
    let thread = std::thread::Builder::new()
        .name(format!("cove task {id}"))
        // A task evaluates Cove, so it gets the stack the depth limit is
        // calibrated against rather than whatever the platform hands a thread
        // by default. Without this a task overflows its stack long before
        // `MAX_CALL_DEPTH` stops it, which ends the process and takes every
        // sibling task with it.
        .stack_size(crate::interp::STACK_SIZE)
        .spawn(move || run(runtime, id, flag, body, span))
        .map_err(|e| {
            // A task the machine refused is not a task the run holds, so the
            // place charged for it above goes back.
            host.hosts().with_budget(|budget| budget.release_task());
            RuntimeError::new(format!("this task could not be given a thread: {e}")).at(span)
        })?;

    let task = Task::running(
        id,
        scope.name.clone(),
        scope.next_position(),
        cancellation,
        thread,
    );
    Ok(Value::Task(scope.adopt(task)))
}

/// Waits for a task's thread, charging the time against this body's timings
/// as wait rather than as work.
///
/// This is also the one place that learns whether a cancellation actually
/// stopped a task, so it is where `TaskCancelled` is traced. A task is waited
/// for once, so the event is recorded once; a task that had already finished
/// is unaffected by cancellation, and tracing it as cancelled would say work
/// was stopped that in fact happened.
pub(crate) fn join<H: Tasking>(host: &mut H, task: &Rc<Task>) {
    if !task.is_running() {
        return;
    }
    let started = Instant::now();
    task.join();
    // A task ends by finishing, by failing, by being cancelled, or by
    // breaking an invariant in its own thread, and a join is where all four
    // are observed — so this is where the place it held under the
    // concurrency limit goes back. Releasing it on the task's own thread
    // instead would make what a `spawn` is refused for depend on how quickly
    // a sibling happened to finish.
    host.hosts().with_budget(|budget| budget.release_task());
    host.charge_wait(started.elapsed());
    if matches!(&*task.state.borrow(), TaskState::Cancelled) {
        host.runtime()
            .trace(TraceEvent::TaskCancelled { id: task.id });
    }
}

/// `await`: waits for a task's thread and answers the value its body produced.
///
/// A task's body runs at most once and is waited for at most once, so
/// awaiting the same handle twice returns the same value and repeats no
/// effect.
pub(crate) fn settle<H: Tasking>(
    host: &mut H,
    task: &Rc<Task>,
    span: Span,
) -> Result<Value, RuntimeError> {
    join(host, task);
    match &*task.state.borrow() {
        TaskState::Settled(value) => Ok(value.clone()),
        TaskState::Failed(error) => Err(error.clone()),
        TaskState::Cancelled => Err(awaiting_a_cancelled_task(task, span)),
        TaskState::Running => {
            unreachable!("joining a task leaves it settled, failed, or cancelled")
        }
    }
}

/// Cancels every running child of `scope` and waits for it to stop.
///
/// Every child is asked first and waited for afterwards, so they stop at the
/// same time rather than one after another. Leaving a scope waits for or
/// cancels its children, so this does both: a scope never outlives a thread
/// it started.
pub(crate) fn cancel_children<H: Tasking>(host: &mut H, scope: &Rc<TaskScope>) {
    scope.cancel_running();
    let mut index = 0;
    while let Some(task) = scope.task_at(index) {
        index += 1;
        join(host, &task);
    }
}

/// How a child ended, where that is something the scope has to pass on.
pub(crate) enum ChildFailure {
    /// The task's value was `Err(...)`, already wrapped as the value the
    /// enclosing call is to answer.
    ///
    /// A task whose value is a failed `Result` returns that failure from the
    /// function the scope was written in, exactly as `?` would, which is what
    /// makes `scope s { s.spawn { f()? } }` mean what a reader expects: the
    /// failure reaches the caller rather than sitting unread in a handle
    /// nobody awaited.
    Returned(Value),
    /// The task's body raised. The error propagates as itself.
    Raised(RuntimeError),
}

/// Waits for every task the body did not await, in spawn order, and reports
/// the first child that did not simply finish.
///
/// A task that fails is not swallowed — a [`RuntimeError`] propagates as
/// itself, and a task whose value is `Err(error)` returns that error from the
/// enclosing function, exactly as `?` would. A task the program itself
/// cancelled is neither: the program asked for that stop, so leaving the
/// scope is not the place to complain about it. Either way the tasks still
/// running are cancelled and waited for, which is what the caller does with
/// what this answers.
///
/// Waiting happens in spawn order, which is an order of *observation* only:
/// the tasks ran at the same time on threads of their own, so only the set of
/// effects a scope produces is defined, never their sequence.
pub(crate) fn wait_for_children<H: Tasking>(
    host: &mut H,
    scope: &Rc<TaskScope>,
) -> Option<ChildFailure> {
    // Waiting reads the scope's children by index rather than from a
    // snapshot, so a scope that grew while it was being left is still waited
    // for to the end.
    let mut index = 0;
    while let Some(task) = scope.task_at(index) {
        index += 1;
        if !task.is_running() {
            continue;
        }
        join(host, &task);
        // The state is read and released before anything else runs, so
        // cancelling the rest of the scope can borrow these same tasks.
        let outcome = match &*task.state.borrow() {
            TaskState::Settled(value) => {
                failure_of(value).map(|error| ChildFailure::Returned(Value::err(error)))
            }
            TaskState::Failed(error) => Some(ChildFailure::Raised(error.clone())),
            TaskState::Cancelled | TaskState::Running => None,
        };
        if outcome.is_some() {
            return outcome;
        }
    }
    None
}

/// The trace event a finished task's thread writes, and the form its value
/// crosses back in.
///
/// A task stopped by its own cancellation did not run to completion, so it is
/// traced as cancelled — by whoever waits for it, which is the only place
/// that knows it stopped rather than finished — and not here.
pub(crate) fn finished(
    runtime: &Runtime,
    id: u64,
    cancellation: &Cancellation,
    span: Span,
    result: Result<Value, RuntimeError>,
    cpu: Duration,
) -> TaskOutcome {
    if !(result.is_err() && cancellation.is_cancelled()) {
        runtime.trace(TraceEvent::TaskCompleted { id, cpu });
    }
    let value = result?;
    Transfer::of(&value).map_err(|found| {
        RuntimeError::new(format!(
            "this task produced {}, which cannot leave a task",
            found.subject()
        ))
        .at(span)
        .with_rule(TASK_SAFETY_RULE)
        .with_help(found.help("returning it from a task"))
    })
}

/// The error a `Result` carries, when the value is one and it failed.
fn failure_of(value: &Value) -> Option<Value> {
    value
        .err_payload()
        .map(|payload| payload.first().cloned().unwrap_or(Value::Unit))
}

fn awaiting_a_cancelled_task(task: &Task, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "{} was cancelled, so it has no value to await",
        task.describe()
    ))
    .at(span)
    .with_rule("Leaving a task scope waits for or cancels its child tasks, and a cancelled task never runs.")
    .with_help("await the task before cancelling it, and before leaving its scope early")
}

// ------------------------------------------------------------ task safety

/// The first value in a capture that may not cross a task boundary.
#[derive(Clone, Debug)]
pub struct NotTaskSafe {
    /// How the offending value is reached from the closure, such as
    /// `app.metrics` or `handler -> builder`.
    pub path: String,
    /// The type that is not task-safe.
    pub type_name: String,
}

/// A task-safe value in the form the receiving task can own.
///
/// The interpreter's [`Value`] is reference-counted with `Rc`, so it belongs
/// to the thread that built it, while a value crossing a task boundary has to
/// be owned by the thread that receives it. ADR 0008 observes that those are
/// one condition: the Language Card lets a value cross exactly when copying
/// it is the whole of transferring it, which is what a thread requires too.
///
/// So this is not a second task-safety rule standing beside the first — it
/// *is* the rule, and the walk that once only answered it was replaced by
/// this one. [`Transfer::of`] answers both questions in one walk: whether
/// the value may cross, and what the receiving task owns once it has.
#[derive(Clone, Debug)]
pub enum Transfer {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Duration(i64),
    Str(String),
    Array(Vec<Transfer>),
    Map(BTreeMap<MapKey, Transfer>),
    Set(BTreeSet<MapKey>),
    Struct {
        type_name: String,
        fields: Vec<(String, Transfer)>,
        /// Whether the type is opaque, so that a value rebuilt in the
        /// receiving task keeps rendering as its name alone.
        opaque: bool,
    },
    Enum {
        type_name: String,
        case: String,
        payload: Vec<Transfer>,
    },
    Dyn {
        trait_name: String,
        value: Box<Transfer>,
    },
    Closure(Box<TransferClosure>),
    HostModule(String),
    HostFn {
        module: String,
        op: String,
    },
    Type(String),
    Range {
        start: i64,
        end: i64,
        inclusive_end: bool,
    },
    /// The one value that crosses by sharing rather than by copying: both
    /// sides address the same [`SharedCell`], which is what makes `Shared`
    /// the sanctioned way to hold mutable state across tasks.
    Shared(Arc<SharedCell>),
    /// A resource handle, when its schema says it may cross.
    ///
    /// A handle is a name and nothing else, so it crosses the way a string
    /// does. What decides is not the handle but the resource: a host that
    /// keeps a connection behind a lock says so in its
    /// [`crate::schema::ResourceSchema`], and the answer travels on the
    /// handle so this walk can read it.
    Resource(Arc<ResourceHandle>),
}

/// The parts of a [`Closure`] a receiving task can own.
///
/// The body and the declaration are shared rather than copied: an
/// [`Arc<Block>`] is immutable syntax, so two threads reading the same one
/// observe nothing about each other. Only the captures are converted, which
/// is where the task-safety rule has anything to decide.
#[derive(Clone, Debug)]
pub struct TransferClosure {
    pub is_async: bool,
    pub params: Vec<Param>,
    pub decl: Option<Arc<FnDecl>>,
    /// The body, in whichever of the two forms the backend that made this
    /// closure builds.
    ///
    /// [`ClosureBody::Tree`] is syntax and crosses the way the declaration
    /// beside it does: an `Arc<Block>` is immutable, so two threads reading
    /// one observe nothing about each other. [`ClosureBody::Lowered`] is an
    /// id into *one run's* `cove_ir::Program`, so what it means depends on
    /// which program the receiving task holds — and the answer is that it
    /// holds the same one. A `cove_ir::Program` is immutable once lowered and
    /// holds `Arc<str>` throughout, so a run has one of them and every thread
    /// of that run reads it; `crate::vm::Vm` therefore takes a share of the
    /// handle rather than a bare reference, and a `FunctionId` names the same
    /// function on the far side because it is the same `functions` it indexes
    /// into. Lowering the program a second time on the receiving thread would
    /// not have given that, which is why nothing does.
    pub body: ClosureBody,
    pub module: String,
    pub captures: Vec<(String, Transfer)>,
}

impl Transfer {
    /// Converts `value` into the form a receiving task owns, or reports the
    /// first part of it that may not cross a task boundary.
    pub fn of(value: &Value) -> Result<Transfer, NotTaskSafe> {
        Transfer::convert("", value)
    }

    /// Whether `target` is reachable from this transfer without passing
    /// through a second [`SharedCell`].
    ///
    /// [`SharedCell::lock`] calls this on the value it is about to store,
    /// with `target` the cell being locked, to reject the one shape of cycle
    /// ADR 0011 makes cheap to catch: a cell ending up holding a handle to
    /// itself. `Transfer::Shared` holds an `Arc` handle, not the other cell's
    /// contents — those sit behind a `Mutex` this walk does not take — so
    /// the search stops at every `Shared` it meets and never risks the
    /// deadlock or unbounded work that chasing into another cell could
    /// cause. A cycle through two or more cells is invisible to this check;
    /// that is the wider, deferred problem the ADR's amendment names.
    ///
    /// A [`Transfer::Resource`] is a leaf here for the same reason it is a
    /// leaf everywhere else: a handle is a name the host resolves, not a
    /// container, so nothing is reachable through one.
    pub(crate) fn reaches(&self, target: *const SharedCell) -> bool {
        match self {
            Transfer::Shared(cell) => std::ptr::eq(Arc::as_ptr(cell), target),
            Transfer::Array(items) => items.iter().any(|item| item.reaches(target)),
            Transfer::Map(entries) => entries.values().any(|item| item.reaches(target)),
            Transfer::Struct { fields, .. } => fields.iter().any(|(_, item)| item.reaches(target)),
            Transfer::Enum { payload, .. } => payload.iter().any(|item| item.reaches(target)),
            Transfer::Dyn { value, .. } => value.reaches(target),
            Transfer::Closure(closure) => closure
                .captures
                .iter()
                .any(|(_, item)| item.reaches(target)),
            Transfer::Unit
            | Transfer::Bool(_)
            | Transfer::Int(_)
            | Transfer::Float(_)
            | Transfer::Duration(_)
            | Transfer::Str(_)
            | Transfer::Set(_)
            | Transfer::HostModule(_)
            | Transfer::HostFn { .. }
            | Transfer::Type(_)
            | Transfer::Range { .. }
            | Transfer::Resource(_) => false,
        }
    }

    /// `path` names how the value was reached, and is extended as the walk
    /// descends, so a diagnostic can point at the capture rather than at the
    /// closure as a whole.
    fn convert(path: &str, value: &Value) -> Result<Transfer, NotTaskSafe> {
        match value {
            // Primitives, strings, and ranges are values; copying one is the
            // whole of transferring it.
            Value::Unit => Ok(Transfer::Unit),
            Value::Bool(b) => Ok(Transfer::Bool(*b)),
            Value::Int(n) => Ok(Transfer::Int(*n)),
            Value::Float(x) => Ok(Transfer::Float(*x)),
            Value::Duration(ns) => Ok(Transfer::Duration(*ns)),
            Value::Str(s) => Ok(Transfer::Str(s.to_string())),
            Value::Range {
                start,
                end,
                inclusive_end,
            } => Ok(Transfer::Range {
                start: *start,
                end: *end,
                inclusive_end: *inclusive_end,
            }),
            // A vector is growable shared mutable storage, so it cannot cross
            // even through `let`: the `let` restricts this alias, not the
            // storage.
            Value::Vector(_) => Err(NotTaskSafe {
                path: path.to_string(),
                type_name: value.type_name(),
            }),
            // `Array` and `Map` are immutable, so they cross exactly when
            // everything they contain does.
            Value::Array(items) => {
                let mut converted = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    converted.push(Transfer::convert(&format!("{path}[{i}]"), item)?);
                }
                Ok(Transfer::Array(converted))
            }
            // A `Set` element is a `MapKey`: always `Bool`, `Int`, `Str`, or a
            // payload-free enum case, all of which are unconditionally
            // task-safe.
            Value::Set(items) => Ok(Transfer::Set((**items).clone())),
            Value::Map(entries) => {
                let mut converted = BTreeMap::new();
                for (key, item) in entries.iter() {
                    converted.insert(
                        key.clone(),
                        Transfer::convert(&format!("{path}[{key}]"), item)?,
                    );
                }
                Ok(Transfer::Map(converted))
            }
            Value::Struct(structure) => {
                let mut fields = Vec::with_capacity(structure.fields.len());
                for (name, field) in &structure.fields {
                    fields.push((
                        name.to_string(),
                        Transfer::convert(&format!("{path}.{name}"), field)?,
                    ));
                }
                Ok(Transfer::Struct {
                    type_name: structure.type_name.to_string(),
                    fields,
                    opaque: structure.opaque,
                })
            }
            Value::Enum(enumeration) => {
                let mut payload = Vec::with_capacity(enumeration.payload.len());
                for (i, item) in enumeration.payload.iter().enumerate() {
                    payload.push(Transfer::convert(
                        &format!("{path}.{}({i})", enumeration.case),
                        item,
                    )?);
                }
                Ok(Transfer::Enum {
                    type_name: enumeration.type_name.to_string(),
                    case: enumeration.case.to_string(),
                    payload,
                })
            }
            // A trait object is task-safe exactly when the value it holds is:
            // the wrapper adds a trait name, which is not state.
            Value::Dyn(d) => Ok(Transfer::Dyn {
                trait_name: d.trait_name.to_string(),
                value: Box::new(Transfer::convert(path, &d.value)?),
            }),
            // Closures are task-safe only when every capture is.
            Value::Closure(closure) => {
                let mut captures = Vec::with_capacity(closure.captures.len());
                for (name, captured) in &closure.captures {
                    let capture_path = if path.is_empty() {
                        name.to_string()
                    } else {
                        format!("{path} -> {name}")
                    };
                    captures.push((
                        name.to_string(),
                        Transfer::convert(&capture_path, captured)?,
                    ));
                }
                Ok(Transfer::Closure(Box::new(TransferClosure {
                    is_async: closure.is_async,
                    params: closure.params.clone(),
                    decl: closure.decl.clone(),
                    body: closure.body.clone(),
                    module: closure.module.to_string(),
                    captures,
                })))
            }
            // A host module or operation is a name, not state. What a host
            // call *produces* declares its own task-safety in the Host API
            // schema, which [`crate::host::HostRegistry::result_is_task_safe`]
            // reads; addressing the module is not itself a transfer of state,
            // and the grant check still happens at the call.
            Value::HostModule(module) => Ok(Transfer::HostModule(module.to_string())),
            Value::HostFn(host) => Ok(Transfer::HostFn {
                module: host.module.to_string(),
                op: host.op.to_string(),
            }),
            Value::Type(name) => Ok(Transfer::Type(name.to_string())),
            // "Host resources declare task-safety in their Host API schema."
            // The handle carries that declaration, so a resource the host
            // keeps to one task is refused here exactly like a vector.
            Value::Resource(handle) if handle.task_safe => Ok(Transfer::Resource(handle.clone())),
            Value::Resource(_) => Err(NotTaskSafe {
                path: path.to_string(),
                type_name: value.type_name(),
            }),
            // A `Shared` is the one exception to the copy rule: it crosses by
            // sharing the cell, which is the reason the type exists.
            Value::Shared(cell) => Ok(Transfer::Shared(cell.clone())),
            // A scope and a handle belong to the task that holds them: a child
            // may not spawn into its parent's scope or await its siblings.
            // That keeps the scope's children a set its own body decides.
            Value::TaskScope(_) | Value::Task(_) => Err(NotTaskSafe {
                path: path.to_string(),
                type_name: value.type_name(),
            }),
        }
    }

    /// Rebuilds the value in the receiving task, where it is an ordinary
    /// [`Value`] again with no trace of having crossed.
    pub fn into_value(self) -> Value {
        match self {
            Transfer::Unit => Value::Unit,
            Transfer::Bool(b) => Value::Bool(b),
            Transfer::Int(n) => Value::Int(n),
            Transfer::Float(x) => Value::Float(x),
            Transfer::Duration(ns) => Value::Duration(ns),
            Transfer::Str(s) => Value::Str(s.into()),
            Transfer::Array(items) => {
                Value::Array(items.into_iter().map(Transfer::into_value).collect())
            }
            Transfer::Map(entries) => Value::Map(Rc::new(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, value.into_value()))
                    .collect(),
            )),
            Transfer::Set(items) => Value::Set(Rc::new(items)),
            Transfer::Struct {
                type_name,
                fields,
                opaque,
            } => Value::Struct(Rc::new(StructValue {
                type_name: type_name.into(),
                fields: fields
                    .into_iter()
                    .map(|(name, value)| (name.into(), value.into_value()))
                    .collect(),
                opaque,
            })),
            Transfer::Enum {
                type_name,
                case,
                payload,
            } => Value::Enum(Box::new(EnumValue {
                type_name: type_name.into(),
                case: case.into(),
                payload: payload.into_iter().map(Transfer::into_value).collect(),
            })),
            Transfer::Dyn { trait_name, value } => Value::Dyn(Rc::new(DynValue {
                trait_name: trait_name.into(),
                value: value.into_value(),
            })),
            Transfer::Closure(closure) => {
                let closure = *closure;
                Value::Closure(Rc::new(Closure {
                    is_async: closure.is_async,
                    params: closure.params,
                    decl: closure.decl,
                    body: closure.body,
                    module: closure.module.into(),
                    captures: closure
                        .captures
                        .into_iter()
                        .map(|(name, value)| (name.into(), value.into_value()))
                        .collect(),
                }))
            }
            Transfer::HostModule(module) => Value::HostModule(module.into()),
            Transfer::HostFn { module, op } => Value::HostFn(Rc::new(HostFnValue {
                module: module.into(),
                op: op.into(),
            })),
            Transfer::Type(name) => Value::Type(name.into()),
            Transfer::Range {
                start,
                end,
                inclusive_end,
            } => Value::Range {
                start,
                end,
                inclusive_end,
            },
            Transfer::Shared(cell) => Value::Shared(cell),
            Transfer::Resource(handle) => Value::Resource(handle),
        }
    }
}

impl NotTaskSafe {
    /// How the offending value is named in a diagnostic: as the type itself
    /// when the value under test *is* the problem, and with the path to it
    /// when the problem is nested inside a larger value.
    ///
    /// A path is written as it is reached from the value under test, so a
    /// leading `.` is dropped: the root has no name to hang it on.
    pub fn subject(&self) -> String {
        match self.path.trim_start_matches('.') {
            "" => format!("a `{}`", self.type_name),
            path => format!("a `{}` in `{path}`", self.type_name),
        }
    }

    /// The correction the Language Card promises for this violation, at the
    /// boundary that rejected the value: `spawn` for a task, `Shared` for a
    /// synchronized handle.
    pub fn help(&self, boundary: &str) -> String {
        if self.type_name == "Vector" {
            format!(
                "finish it as an array with `freeze()`, or copy it with `toArray()`, before {boundary}"
            )
        } else if self.type_name.contains('.') {
            format!(
                "`{}` is a host resource whose Host API schema declares it not task-safe; open one in the task that uses it rather than {boundary}",
                self.type_name
            )
        } else {
            "wrap mutable state in `Shared` or another synchronized type, or pass an immutable value"
                .to_string()
        }
    }
}

/// The Language Card sentence every task-safety diagnostic quotes.
pub const TASK_SAFETY_RULE: &str = "Immutable task-safe values such as arrays may cross task boundaries. A vector cannot cross, even through `let`; finish it as an array or wrap mutable state in `Shared` or another synchronized type. Closures are task-safe only when every capture is.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ResourceSchema;
    use crate::value::VectorStorage;
    use cove_diag::{FileId, Span};
    use cove_syntax::ast::Block;

    /// The point of the type: a value that may cross a task boundary is a
    /// value a thread can own. If this ever stopped holding, `spawn` could
    /// not hand a body to a thread.
    #[test]
    fn a_transfer_can_be_owned_by_another_thread() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Transfer>();
    }

    #[test]
    fn converting_a_task_safe_value_and_back_preserves_it() {
        let value = Value::Array(
            vec![
                Value::Int(1),
                Value::Str("two".into()),
                Value::Struct(Rc::new(StructValue {
                    type_name: "test.Point".into(),
                    fields: vec![("x".into(), Value::Int(3))],
                    opaque: false,
                })),
            ]
            .into(),
        );
        let crossed = Transfer::of(&value)
            .expect("an array of task-safe values may cross")
            .into_value();
        assert!(crossed.eq_value(&value), "{crossed} != {value}");
    }

    #[test]
    fn a_vector_reached_through_a_struct_is_named_by_its_path() {
        let value = Value::Struct(Rc::new(StructValue {
            type_name: "test.Draft".into(),
            fields: vec![(
                "guests".into(),
                Value::Vector(VectorStorage::new(vec![Value::Int(1)])),
            )],
            opaque: false,
        }));
        let found = Transfer::of(&value).expect_err("a vector may not cross");
        assert_eq!(found.path, ".guests");
        assert_eq!(found.type_name, "Vector");
    }

    /// The one exception to the copy rule: both sides address one cell.
    #[test]
    fn a_shared_crosses_by_sharing_rather_than_by_copying() {
        let cell = SharedCell::new(Transfer::Int(1));
        let crossed = Transfer::of(&Value::Shared(cell.clone()))
            .expect("a `Shared` is task-safe")
            .into_value();
        match crossed {
            Value::Shared(other) => assert!(Arc::ptr_eq(&cell, &other)),
            other => panic!("expected a `Shared`, found {other}"),
        }
    }

    // ------------------------------------------- shapes a value crosses in
    //
    // The tests above cover a struct field directly. Everything below walks
    // the rest of the shapes `Transfer::convert` descends into — arrays,
    // enums, maps, trait objects, closures, and Host resource handles — and
    // pins both directions: a task-safe value of that shape crosses and
    // round-trips, and a `Vector` (or a non-task-safe resource handle)
    // nested in that shape is refused with a `path` that names how it was
    // reached.

    /// A resource handle for a fictitious host, naming `module.Connection`,
    /// with its schema's `task_safe` set as the test needs.
    fn resource_handle(module: &str, task_safe: bool, id: u64) -> Arc<ResourceHandle> {
        let schema = ResourceSchema {
            name: "Connection",
            task_safe,
            operations: &[],
        };
        ResourceHandle::new(module, &schema, id)
    }

    /// A closure with no body worth running: only its captures matter to
    /// `Transfer::convert`, so the body is the emptiest one the AST allows.
    fn closure_value(module: &str, captures: Vec<(&str, Value)>) -> Value {
        let span = Span::new(FileId(0), 0, 0);
        Value::Closure(Rc::new(Closure {
            is_async: false,
            params: Vec::new(),
            decl: None,
            body: ClosureBody::Tree(Arc::new(Block {
                statements: Vec::new(),
                tail: None,
                span,
            })),
            module: module.into(),
            captures: captures
                .into_iter()
                .map(|(name, value)| (name.into(), value))
                .collect(),
        }))
    }

    #[test]
    fn an_array_of_structs_crosses_and_round_trips() {
        let value = Value::Array(
            vec![
                Value::Struct(Rc::new(StructValue {
                    type_name: "test.Point".into(),
                    fields: vec![("x".into(), Value::Int(1)), ("y".into(), Value::Int(2))],
                    opaque: false,
                })),
                Value::Struct(Rc::new(StructValue {
                    type_name: "test.Point".into(),
                    fields: vec![("x".into(), Value::Int(3)), ("y".into(), Value::Int(4))],
                    opaque: false,
                })),
            ]
            .into(),
        );
        let crossed = Transfer::of(&value)
            .expect("an array of structs built only from Ints is task-safe")
            .into_value();
        assert!(crossed.eq_value(&value), "{crossed} != {value}");
    }

    /// "Immutable task-safe values such as arrays may cross task boundaries"
    /// — but an array is only as task-safe as what it holds. A vector
    /// nested two levels down, inside a struct inside the array, is still
    /// refused, and `Transfer::convert` builds the path by extending it once
    /// per level: `"{path}[{i}]"` for the array, then `"{path}.{name}"` for
    /// the struct field.
    #[test]
    fn an_array_of_structs_is_refused_for_the_one_vector_it_holds() {
        let value = Value::Array(
            vec![Value::Struct(Rc::new(StructValue {
                type_name: "test.Draft".into(),
                fields: vec![(
                    "guests".into(),
                    Value::Vector(VectorStorage::new(vec![Value::Int(1)])),
                )],
                opaque: false,
            }))]
            .into(),
        );
        let found = Transfer::of(&value).expect_err("a vector nested in an array may not cross");
        assert_eq!(found.path, "[0].guests");
        assert_eq!(found.type_name, "Vector");
    }

    #[test]
    fn an_enum_payload_that_is_task_safe_crosses_and_round_trips() {
        let value = Value::Enum(Box::new(EnumValue {
            type_name: "test.Shape".into(),
            case: "Circle".into(),
            payload: vec![Value::Int(4)],
        }));
        let crossed = Transfer::of(&value)
            .expect("an enum payload of Ints is task-safe")
            .into_value();
        assert!(crossed.eq_value(&value), "{crossed} != {value}");
    }

    /// An enum case's payload is walked exactly like a struct's fields, just
    /// with no field names to hang a path on: `Transfer::convert` names the
    /// case and the payload's position instead, `"{path}.{case}({i})"`.
    #[test]
    fn an_enum_payload_holding_a_vector_is_refused_by_its_case_and_index() {
        let value = Value::Enum(Box::new(EnumValue {
            type_name: "test.Shape".into(),
            case: "Wrap".into(),
            payload: vec![Value::Vector(VectorStorage::new(Vec::new()))],
        }));
        let found = Transfer::of(&value).expect_err("a vector in an enum payload may not cross");
        assert_eq!(found.path, ".Wrap(0)");
        assert_eq!(found.type_name, "Vector");
    }

    #[test]
    fn a_map_of_task_safe_values_crosses_and_round_trips() {
        let value = Value::Map(Rc::new(BTreeMap::from([
            (MapKey::Str("a".to_string()), Value::Int(1)),
            (MapKey::Str("b".to_string()), Value::Int(2)),
        ])));
        let crossed = Transfer::of(&value)
            .expect("a map of Ints is task-safe")
            .into_value();
        assert!(crossed.eq_value(&value), "{crossed} != {value}");
    }

    #[test]
    fn a_map_value_holding_a_vector_is_refused_naming_the_key() {
        let value = Value::Map(Rc::new(BTreeMap::from([(
            MapKey::Str("widgets".to_string()),
            Value::Vector(VectorStorage::new(Vec::new())),
        )])));
        let found = Transfer::of(&value).expect_err("a vector held by a map entry may not cross");
        assert_eq!(found.path, "[widgets]");
        assert_eq!(found.type_name, "Vector");
    }

    #[test]
    fn a_dyn_value_that_is_task_safe_crosses_keeping_its_trait_name() {
        let value = Value::Dyn(Rc::new(DynValue {
            trait_name: "render.Display".into(),
            value: Value::Str("hi".into()),
        }));
        let crossed = Transfer::of(&value)
            .expect("a `dyn Trait` wrapping a task-safe value is itself task-safe")
            .into_value();
        match crossed {
            Value::Dyn(d) => {
                assert_eq!(&*d.trait_name, "render.Display");
                assert!(d.value.eq_value(&Value::Str("hi".into())));
            }
            other => panic!("expected a `Dyn`, found {other}"),
        }
    }

    /// "A trait object is task-safe exactly when the value it holds is: the
    /// wrapper adds a trait name, which is not state" — and `Transfer::convert`
    /// takes that literally about the path too, passing `path` through to the
    /// wrapped value *unchanged*. So a struct's `Vector` field is refused with
    /// exactly the path it would have outside the `Dyn`, with no `dyn` marker
    /// anywhere in it.
    #[test]
    fn a_dyn_wrapping_a_struct_with_a_vector_is_refused_at_the_fields_path() {
        let value = Value::Dyn(Rc::new(DynValue {
            trait_name: "render.Display".into(),
            value: Value::Struct(Rc::new(StructValue {
                type_name: "test.Draft".into(),
                fields: vec![(
                    "guests".into(),
                    Value::Vector(VectorStorage::new(Vec::new())),
                )],
                opaque: false,
            })),
        }));
        let found = Transfer::of(&value).expect_err("a vector inside a `Dyn` may not cross");
        assert_eq!(found.path, ".guests");
        assert_eq!(found.type_name, "Vector");
    }

    #[test]
    fn a_closure_with_a_task_safe_capture_crosses_keeping_its_captures() {
        let value = closure_value(
            "test.mod",
            vec![("count", Value::Int(1)), ("label", Value::Str("a".into()))],
        );
        let crossed = Transfer::of(&value)
            .expect("a closure whose captures are an Int and a String is task-safe")
            .into_value();
        match crossed {
            Value::Closure(closure) => {
                assert_eq!(closure.captures.len(), 2);
                assert!(closure.captures[0].1.eq_value(&Value::Int(1)));
                assert!(closure.captures[1].1.eq_value(&Value::Str("a".into())));
            }
            other => panic!("expected a `Closure`, found {other}"),
        }
    }

    /// "Closures are task-safe only when every capture is" — including a
    /// capture that is itself a closure, whose own captures are walked in
    /// turn. `Transfer::convert` writes `" -> "` between a capture's name and
    /// the name of whatever it captures next, so a vector reached two
    /// captures deep still names the whole chain that reaches it, not just
    /// the last step.
    #[test]
    fn a_closure_capturing_a_closure_whose_capture_holds_a_vector_is_refused_two_levels_deep() {
        let inner = closure_value(
            "test.mod",
            vec![(
                "state",
                Value::Struct(Rc::new(StructValue {
                    type_name: "test.Draft".into(),
                    fields: vec![(
                        "guests".into(),
                        Value::Vector(VectorStorage::new(Vec::new())),
                    )],
                    opaque: false,
                })),
            )],
        );
        let outer = closure_value("test.mod", vec![("handler", inner)]);
        let found =
            Transfer::of(&outer).expect_err("a vector captured two closures deep may not cross");
        assert_eq!(found.path, "handler -> state.guests");
        assert_eq!(found.type_name, "Vector");
    }

    // ------------------------------------------------------ host resources

    /// "Host resources declare task-safety in their Host API schema." A
    /// resource whose state the host keeps behind a lock says
    /// `task_safe: true`, and ADR 0013 says a handle is a name and nothing
    /// else, so it then crosses the way a string does: the same `Arc` both
    /// sides address, naming one resource.
    #[test]
    fn a_resource_handle_whose_schema_says_task_safe_crosses_naming_the_same_resource() {
        let handle = resource_handle("test", true, 7);
        let crossed = Transfer::of(&Value::Resource(handle.clone()))
            .expect("a resource whose schema says task-safe may cross")
            .into_value();
        match crossed {
            Value::Resource(other) => assert!(
                Arc::ptr_eq(&handle, &other),
                "a handle crosses by sharing its `Arc`, so both sides should name one resource: {handle} and {other}"
            ),
            other => panic!("expected a `Resource`, found {other}"),
        }
    }

    #[test]
    fn a_resource_handle_whose_schema_says_not_task_safe_is_refused() {
        let handle = resource_handle("test", false, 7);
        let found = Transfer::of(&Value::Resource(handle))
            .expect_err("a resource whose schema says not task-safe may not cross");
        assert_eq!(found.type_name, "test.Connection");
    }

    /// The same rule against a shipped schema rather than a fictitious one.
    /// ADR 0018 gives `files.Reader` `task_safe: false` because a reader is a
    /// position in a file, and two tasks taking turns at one position each
    /// receive some of the lines and neither receives the file. The refusal
    /// is what makes that a mistake the run reports rather than an
    /// interleaving no test can pin.
    #[test]
    fn a_files_reader_is_refused_at_a_task_boundary() {
        let handle = ResourceHandle::new("files", &cove_schema::hosts::FILES.resources[0], 1);
        let found = Transfer::of(&Value::Resource(handle))
            .expect_err("a `files.Reader` may not cross a task boundary");
        assert_eq!(found.type_name, "files.Reader");
        assert_eq!(
            found.help("spawning"),
            "`files.Reader` is a host resource whose Host API schema declares it not task-safe; open one in the task that uses it rather than spawning"
        );
    }

    /// The correction for a host resource is not "wrap it in `Shared`" — the
    /// host already decided this resource's state stays with the task that
    /// opened it — so `NotTaskSafe::help` reads a `.` in the type name, which
    /// is exactly how `Value::type_name` renders a resource's
    /// `module.Type`, and gives a different sentence than the generic one a
    /// struct or closure capture gets.
    #[test]
    fn not_task_safe_help_for_a_host_resource_names_its_schema() {
        let found = NotTaskSafe {
            path: String::new(),
            type_name: "database.Connection".to_string(),
        };
        assert_eq!(
            found.help("spawning"),
            "`database.Connection` is a host resource whose Host API schema declares it not task-safe; open one in the task that uses it rather than spawning"
        );
    }

    // ------------------------------------------------------ shared cells

    /// `Value::Shared(cell) => Ok(Transfer::Shared(cell.clone()))` asks
    /// nothing about what `cell` holds — unlike every other shape above,
    /// this arm does not walk. That is sound in the running language only
    /// because nothing reaches a `Shared` with an unchecked payload:
    /// `SharedCell::wrap` (`crates/cove-runtime/src/shared.rs`), called from
    /// the `Shared(value)` constructor in
    /// `crates/cove-runtime/src/builtins.rs`, runs `Transfer::of` on the
    /// payload before a cell is ever built, so a `Shared` this walk sees has
    /// already been vetted once, by a check that lives outside this file.
    /// This test reaches around that guard with `SharedCell::new` — a
    /// constructor ordinary Cove code cannot call — to pin what the walk
    /// itself actually does: it does not repeat the check.
    #[test]
    fn a_shared_crosses_without_rechecking_what_it_already_holds() {
        let handle = resource_handle("test", false, 1);
        Transfer::of(&Value::Resource(handle.clone()))
            .expect_err("a task-unsafe resource handle is refused on its own");
        let cell = SharedCell::new(Transfer::Resource(handle));
        let crossed = Transfer::of(&Value::Shared(cell.clone()))
            .expect("`Shared` crosses without walking its payload")
            .into_value();
        match crossed {
            Value::Shared(other) => assert!(Arc::ptr_eq(&cell, &other)),
            other => panic!("expected a `Shared`, found {other}"),
        }
    }

    // ------------------------------------------------------- subject()

    #[test]
    fn not_task_safe_subject_names_the_type_alone_at_the_root() {
        let found = NotTaskSafe {
            path: String::new(),
            type_name: "Vector".to_string(),
        };
        assert_eq!(found.subject(), "a `Vector`");
    }

    #[test]
    fn not_task_safe_subject_names_the_path_when_nested() {
        let found = NotTaskSafe {
            path: ".guests".to_string(),
            type_name: "Vector".to_string(),
        };
        assert_eq!(found.subject(), "a `Vector` in `guests`");
    }
}
