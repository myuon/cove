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
//! The state machine here is deliberately free of any scheduling policy. A
//! task is pending until something settles it, and only [`crate::interp`]
//! decides when that happens. ADR 0003 phase 1 settles tasks sequentially;
//! phase 2 replaces that policy with a thread per task and keeps this state
//! machine, because a value is still observable only through `await` or scope
//! exit either way.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::error::RuntimeError;
use crate::value::Value;

/// What a spawned task has done so far.
#[derive(Clone, Debug)]
pub enum TaskState {
    /// Spawned, with a body that has not run.
    Pending,
    /// The body is running. A task in this state cannot be settled again,
    /// which is how `await` on a task from inside its own body is caught.
    Running,
    /// The body produced a value. Awaiting again returns the same value.
    Settled(Value),
    /// The body raised a [`RuntimeError`]. Awaiting again raises the same one.
    Failed(RuntimeError),
    /// The task was cancelled before it ran, either by `cancel()` or by
    /// leaving its scope early. Awaiting a cancelled task is an error.
    Cancelled,
}

/// A spawned unit of work and the value it will produce.
///
/// The value is reachable only through [`TaskState::Settled`], so no caller can
/// observe it without going through `await` or scope exit.
#[derive(Debug)]
pub struct Task {
    /// The name of the scope that owns this task, for diagnostics.
    pub scope: Rc<str>,
    /// Position in spawn order within that scope, counting from one.
    pub position: usize,
    /// The closure `spawn` was given, or [`Value::Unit`] for a task that was
    /// already settled when it was created.
    pub body: Value,
    pub state: RefCell<TaskState>,
}

impl Task {
    /// A task that has not run yet.
    pub fn pending(scope: Rc<str>, position: usize, body: Value) -> Rc<Task> {
        Rc::new(Task {
            scope,
            position,
            body,
            state: RefCell::new(TaskState::Pending),
        })
    }

    /// A task whose value is already known.
    ///
    /// ADR 0003 phase 1 calls an `async fn` by running its body at the call
    /// site, so the handle it returns is settled on creation. A scheduler is
    /// free to start that body later instead; nothing may depend on when it
    /// ran, only on the value `await` produces.
    pub fn settled(value: Value) -> Rc<Task> {
        Rc::new(Task {
            scope: "this call".into(),
            position: 0,
            body: Value::Unit,
            state: RefCell::new(TaskState::Settled(value)),
        })
    }

    /// Whether the body still has to run.
    pub fn is_pending(&self) -> bool {
        matches!(&*self.state.borrow(), TaskState::Pending)
    }

    /// Marks an unrun task cancelled. A task that already ran is unaffected:
    /// cancellation stops work that has not happened, it does not undo work
    /// that has.
    pub fn cancel(&self) {
        if self.is_pending() {
            *self.state.borrow_mut() = TaskState::Cancelled;
        }
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

    /// Registers a task and returns its handle.
    pub fn spawn(&self, body: Value) -> Rc<Task> {
        let position = self.tasks.borrow().len() + 1;
        let task = Task::pending(self.name.clone(), position, body);
        self.tasks.borrow_mut().push(task.clone());
        task
    }

    /// The task at `index` in spawn order, if the scope has one.
    pub fn task_at(&self, index: usize) -> Option<Rc<Task>> {
        self.tasks.borrow().get(index).cloned()
    }

    /// Cancels every child that has not run.
    pub fn cancel_pending(&self) {
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

/// Whether `value` may cross a task boundary.
///
/// This is the language rule, not an implementation detail of phase 1. ADR
/// 0003 makes the same predicate the phase 2 conversion between the
/// interpreter's `Rc` values and a `Send` transfer value, so a program that
/// spawns successfully today spawns successfully once tasks really run in
/// parallel.
pub fn is_task_safe(value: &Value) -> bool {
    violation("", value).is_none()
}

/// The first reason `value` may not cross a task boundary, if there is one.
///
/// `path` names how the value was reached, and is extended as the walk
/// descends, so the diagnostic can point at the capture rather than at the
/// closure as a whole.
pub fn task_safety(path: &str, value: &Value) -> Result<(), NotTaskSafe> {
    match violation(path, value) {
        Some(found) => Err(found),
        None => Ok(()),
    }
}

fn violation(path: &str, value: &Value) -> Option<NotTaskSafe> {
    match value {
        // Primitives, strings, and ranges are values; copying one is the whole
        // of transferring it.
        Value::Unit
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Duration(_)
        | Value::Str(_)
        | Value::Range { .. } => None,
        // A vector is growable shared mutable storage, so it cannot cross even
        // through `let`: the `let` restricts this alias, not the storage.
        Value::Vector(_) => Some(NotTaskSafe {
            path: path.to_string(),
            type_name: value.type_name(),
        }),
        // `Array` and `Map` are immutable, so they cross exactly when
        // everything they contain does.
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, item)| violation(&format!("{path}[{i}]"), item)),
        // A `Set` element is a `MapKey`: always `Bool`, `Int`, `Str`, or a
        // payload-free enum case, all of which are unconditionally task-safe.
        Value::Set(_) => None,
        Value::Map(entries) => entries
            .iter()
            .find_map(|(key, item)| violation(&format!("{path}[{key}]"), item)),
        Value::Struct(structure) => structure
            .fields
            .iter()
            .find_map(|(name, field)| violation(&format!("{path}.{name}"), field)),
        Value::Enum(enumeration) => enumeration
            .payload
            .iter()
            .enumerate()
            .find_map(|(i, item)| violation(&format!("{path}.{}({i})", enumeration.case), item)),
        // A trait object is task-safe exactly when the value it holds is:
        // the wrapper adds a trait name, which is not state.
        Value::Dyn(d) => violation(path, &d.value),
        // Closures are task-safe only when every capture is.
        Value::Closure(closure) => closure.captures.iter().find_map(|(name, captured)| {
            let path = if path.is_empty() {
                name.to_string()
            } else {
                format!("{path} -> {name}")
            };
            violation(&path, captured)
        }),
        // A host module or operation is a name, not state. Which host
        // resources are task-safe is declared in the Host API schema, which
        // the MVP does not have yet; until it does, addressing a host module
        // from a task is allowed and the grant check still happens at the call.
        Value::HostModule(_) | Value::HostFn { .. } | Value::Type(_) => None,
        // A scope and a handle belong to the task that holds them: a child may
        // not spawn into its parent's scope or await its siblings. That keeps
        // the scope's children a set its own body decides.
        Value::TaskScope(_) | Value::Task(_) => Some(NotTaskSafe {
            path: path.to_string(),
            type_name: value.type_name(),
        }),
    }
}

impl NotTaskSafe {
    /// The correction the Language Card promises for this violation.
    pub fn help(&self) -> String {
        if self.type_name == "Vector" {
            "finish it as an array with `freeze()`, or copy it with `toArray()`, before spawning"
                .to_string()
        } else {
            "wrap mutable state in `Shared` or another synchronized type, or pass an immutable value"
                .to_string()
        }
    }
}

/// The Language Card sentence every task-safety diagnostic quotes.
pub const TASK_SAFETY_RULE: &str = "Immutable task-safe values such as arrays may cross task boundaries. A vector cannot cross, even through `let`; finish it as an array or wrap mutable state in `Shared` or another synchronized type. Closures are task-safe only when every capture is.";
