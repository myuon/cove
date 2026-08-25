//! The MVP tree-walking interpreter.
//!
//! The interpreter is an ordinary evaluator over [`cove_syntax::ast`] plus the
//! five rules that make Cove Cove:
//!
//! - assignment and ordinary argument passing clone a [`Value`], and `Clone`
//!   already encodes field-wise shallow copy, so there is no deep-copy path;
//! - `let` binds a read-only place and `var` a mutable one, so mutation always
//!   resolves an lvalue down to a slot the caller owns;
//! - `var self` and `var` parameters bind the caller's place instead of a copy;
//! - Host API calls go through [`HostRegistry::call`], which enforces grants;
//! - concurrent work belongs to a task scope, and leaving the scope waits for
//!   or cancels the tasks spawned into it.
//!
//! Static checking (types, exhaustiveness, uniqueness) is future work; the
//! interpreter enforces the same rules dynamically and says which rule it
//! enforced.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cove_diag::{SourceMap, Span};
use cove_sema::resolve::{Program, ResolvedModule};
use cove_syntax::ast::{
    Arg, BinaryOp, Block, EnumDecl, Expr, ExprKind, FnDecl, Ident, ItemKind, Param, Pattern,
    PatternKind, Receiver, StmtKind, StrPart, StructDecl, Type, TypeKind, UnaryOp,
};

use crate::budget::{Cancellation, Stopped};
use crate::builtins::{self, Callable};
use crate::error::RuntimeError;
use crate::host::HostRegistry;
use crate::runtime::Runtime;
use crate::task::{Task, TaskOutcome, TaskScope, TaskState, Transfer};
use crate::trace::{Timing, TraceEvent};
use crate::value::{Closure, DynValue, EnumValue, RangeBounds, StructValue, Value, VectorStorage};

/// How deep Cove calls may nest before the runtime reports a limit instead of
/// exhausting the host stack.
///
/// This is an unconditional safety net independent of [`crate::budget::Limits`]:
/// a `Budget`'s `max_call_depth` is optional and `Limits::default()` imposes
/// none, but the interpreter is a recursive Rust tree walker, so unbounded
/// recursion must still be stopped before it exhausts the native stack. A host
/// that configures a stricter `max_call_depth` is stopped by that limit first;
/// this constant is the fallback when it does not.
const MAX_CALL_DEPTH: usize = 256;

/// Fuel charged at every safepoint: a loop back edge, a function call, or an
/// `await`.
///
/// ADR 0001 is explicit that fuel is a coarse runtime control, not a modeled
/// instruction count — real safepoints vary enormously in the CPU work they
/// guard, so no constant here would make fuel mean "instructions executed."
/// A flat per-safepoint cost keeps that honest: fuel measures how many
/// safepoints a run passed through, which is exactly what bounds a
/// non-terminating loop or an unbounded recursion, and nothing more precise
/// than that is claimed.
const SAFEPOINT_FUEL: u64 = 10;

/// Non-local control flow raised while evaluating an expression.
enum Control {
    Error(RuntimeError),
    /// `return` unwinds to the enclosing function call.
    Return(Value),
    /// `break` / `break expr` unwinds to the nearest enclosing loop, which
    /// then evaluates to this value.
    Break(Value),
    /// `continue` unwinds to the nearest enclosing loop's next iteration.
    Continue,
}

impl From<RuntimeError> for Control {
    fn from(error: RuntimeError) -> Self {
        Control::Error(error)
    }
}

impl Control {
    /// Returns `Err(error)` from the enclosing function, which is what a task
    /// whose value is a failed `Result` does to the scope that waited for it.
    fn error_value(error: Value) -> Control {
        Control::Return(Value::err(error))
    }
}

type Eval = Result<Value, Control>;

/// Converts a completed call back into an ordinary result.
///
/// `Break` and `Continue` reaching a function call boundary would mean
/// `break` or `continue` was used outside a loop (or reached past a closure
/// boundary), which resolve-time checking rejects before the interpreter ever
/// runs; see `cove_sema::resolve`'s `break_outside_loop` / `continue_outside_loop`.
fn finish(result: Eval) -> Result<Value, RuntimeError> {
    match result {
        Ok(value) => Ok(value),
        Err(Control::Return(value)) => Ok(value),
        Err(Control::Error(error)) => Err(error),
        Err(Control::Break(_)) => {
            unreachable!("`break` outside a loop is rejected before execution")
        }
        Err(Control::Continue) => {
            unreachable!("`continue` outside a loop is rejected before execution")
        }
    }
}

/// An assignable location: a binding slot plus the struct fields to navigate.
///
/// Every step is taken under a single borrow, so a place never holds a
/// reference across the evaluation of another expression.
#[derive(Clone)]
struct Place {
    slot: Rc<RefCell<Value>>,
    steps: Vec<Rc<str>>,
    /// `var` places are assignable; `let` places are not.
    mutable: bool,
}

impl Place {
    fn binding(value: Value, mutable: bool) -> Place {
        Place {
            slot: Rc::new(RefCell::new(value)),
            steps: Vec::new(),
            mutable,
        }
    }

    fn field(&self, name: Rc<str>) -> Place {
        let mut steps = self.steps.clone();
        steps.push(name);
        Place {
            slot: self.slot.clone(),
            steps,
            mutable: self.mutable,
        }
    }

    fn with_ref<R>(&self, span: Span, f: impl FnOnce(&Value) -> R) -> Result<R, RuntimeError> {
        let root = self.slot.borrow();
        let mut current: &Value = &root;
        for step in &self.steps {
            match current {
                Value::Struct(value) => {
                    current = value
                        .get(step)
                        .ok_or_else(|| no_field(&value.type_name, step, span))?;
                }
                other => return Err(not_a_struct(other, step, span)),
            }
        }
        Ok(f(current))
    }

    fn with_mut<R>(&self, span: Span, f: impl FnOnce(&mut Value) -> R) -> Result<R, RuntimeError> {
        let mut root = self.slot.borrow_mut();
        let mut current: &mut Value = &mut root;
        for step in &self.steps {
            match current {
                Value::Struct(value) => {
                    let type_name = value.type_name.clone();
                    current = value
                        .get_mut(step)
                        .ok_or_else(|| no_field(&type_name, step, span))?;
                }
                other => return Err(not_a_struct(other, step, span)),
            }
        }
        Ok(f(current))
    }

    /// Reading a place clones: that is the value-semantics rule.
    fn read(&self, span: Span) -> Result<Value, RuntimeError> {
        self.with_ref(span, Value::clone)
    }

    fn write(&self, span: Span, value: Value) -> Result<(), RuntimeError> {
        self.with_mut(span, |slot| *slot = value)
    }
}

/// One lexical environment: the module a body resolves names in, and a stack
/// of block scopes holding places.
struct Env {
    module: Rc<str>,
    scopes: Vec<Vec<(Rc<str>, Place)>>,
}

impl Env {
    fn new(module: Rc<str>) -> Env {
        Env {
            module,
            scopes: vec![Vec::new()],
        }
    }

    fn push(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn pop(&mut self) {
        self.scopes.pop();
    }

    fn declare(&mut self, name: Rc<str>, place: Place) {
        self.scopes
            .last_mut()
            .expect("an environment always has one scope")
            .push((name, place));
    }

    fn lookup(&self, name: &str) -> Option<&Place> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.iter().rev().find(|(n, _)| &**n == name))
            .map(|(_, place)| place)
    }

    /// The bindings a closure body can read, by value at creation time.
    ///
    /// Only names the body mentions are captured. What a closure holds is
    /// therefore what actually has to cross a task boundary when the closure
    /// is spawned, rather than every binding that happened to be in scope.
    fn captures(
        &self,
        mentioned: &BTreeSet<String>,
        span: Span,
    ) -> Result<Vec<(Rc<str>, Value)>, RuntimeError> {
        let mut captured: Vec<(Rc<str>, Value)> = Vec::new();
        for scope in &self.scopes {
            for (name, place) in scope {
                if !mentioned.contains(&**name) {
                    continue;
                }
                let value = place.read(span)?;
                match captured.iter_mut().find(|(n, _)| n == name) {
                    Some(slot) => slot.1 = value,
                    None => captured.push((name.clone(), value)),
                }
            }
        }
        Ok(captured)
    }
}

/// An argument that has been evaluated, in call-site order.
struct EvaluatedArg {
    label: Option<Rc<str>>,
    spread: bool,
    slot: ArgSlot,
    span: Span,
}

/// Ordinary arguments pass a value; `var` arguments pass the caller's place.
enum ArgSlot {
    Value(Value),
    Alias(Place),
}

/// The body a call is about to enter.
struct Target<'t> {
    name: &'t str,
    params: &'t [Param],
    body: &'t Block,
    module: Rc<str>,
    receiver: Option<Receiver>,
    is_async: bool,
    captures: &'t [(Rc<str>, Value)],
    /// The written return type, when there is one. A `dyn Trait` in it is
    /// what tells the interpreter to wrap the result; a lambda writes no
    /// return type, so it never converts.
    return_type: Option<&'t Type>,
}

/// Executes a resolved program.
///
/// One interpreter runs one body on one thread: the entry, or the body of a
/// spawned task. Everything shared with the rest of the run is reached
/// through the [`Runtime`] it borrows, which is what a `spawn` hands to the
/// thread it starts.
///
/// # Ownership of the run's [`crate::budget::Budget`]
///
/// The `Budget` is owned by the [`HostRegistry`] this interpreter borrows,
/// not by `Interpreter` itself: a host installs it once with
/// `HostRegistry::set_budget`, and every task thread reaches that one budget
/// through `HostRegistry::with_budget` at its own safepoints. ADR 0008 draws
/// a task's fuel from the run's budget, so there is exactly one authoritative
/// count of what the run spent, whichever thread spent it. Call depth is the
/// exception and is counted here, because a task has a stack of its own.
pub struct Interpreter<'a> {
    pub program: &'a Program,
    pub sources: &'a SourceMap,
    pub hosts: &'a HostRegistry,
    /// What every thread of this run shares, so a `spawn` can hand a task
    /// thread everything it needs to run a body.
    runtime: &'a Runtime,
    depth: usize,
    /// This task's own cancellation flag, when this interpreter is running a
    /// spawned task's body rather than the entry.
    ///
    /// Cancelling the *run* is the budget's flag, which every safepoint
    /// already observes through the shared budget. This is the second flag a
    /// safepoint checks: it stops one task without stopping the run, which is
    /// what leaving a scope early asks for.
    cancellation: Option<Cancellation>,
    /// Ids of the tasks whose bodies this thread is running, innermost last,
    /// so a nested `spawn` can name its immediate parent.
    task_stack: Vec<u64>,
    /// Active timing contexts: one for the body this thread is running, and
    /// one more for each nested context inside it. A host call's wait is
    /// charged against every context on this stack. Each task thread has a
    /// stack of its own, which is what makes one task's CPU work and
    /// another's wait separately attributable.
    timings: Vec<Timing>,
    /// Where the most recent assertion failed, and the message it produced.
    ///
    /// A failed assertion is an ordinary `Err`, which carries a message and
    /// no source position, and that is the right shape for the language: a
    /// test propagates it with `?` like any other expected failure. The test
    /// runner still wants to point at the assertion the way every other
    /// error points at source, so the one party that saw the assertion —
    /// this evaluator — records where it was. The message is kept alongside
    /// so a caller can tell that the `Err` it is holding is that assertion's
    /// and not some later, unrelated failure.
    assertion_failure: Option<(Span, String)>,
}

impl<'a> Interpreter<'a> {
    /// An interpreter for the entry of `runtime`'s run.
    pub fn new(runtime: &'a Runtime) -> Self {
        Interpreter {
            program: runtime.program(),
            sources: runtime.sources(),
            hosts: runtime.hosts(),
            runtime,
            depth: 0,
            cancellation: None,
            task_stack: Vec::new(),
            timings: Vec::new(),
            assertion_failure: None,
        }
    }

    /// Where the most recent failed assertion was written, together with the
    /// message it produced, or `None` when no assertion has failed.
    ///
    /// A caller compares the message against the error it is reporting: an
    /// assertion that failed and was then handled inside the program is not
    /// the reason a later error was returned.
    pub fn assertion_failure(&self) -> Option<(Span, &str)> {
        self.assertion_failure
            .as_ref()
            .map(|(span, message)| (*span, message.as_str()))
    }

    /// The source text `span` covers, for a diagnostic that quotes the code
    /// it is about.
    fn source_text(&self, span: Span) -> &str {
        let file = self.sources.get(span.file);
        file.text
            .get(span.start as usize..span.end as usize)
            .unwrap_or("?")
    }

    /// An interpreter for the body of the spawned task `id`, which stops when
    /// `cancellation` is raised.
    fn for_task(runtime: &'a Runtime, id: u64, cancellation: Cancellation) -> Self {
        let mut interpreter = Interpreter::new(runtime);
        interpreter.cancellation = Some(cancellation);
        interpreter.task_stack.push(id);
        interpreter
    }

    /// Calls the host-selected entry function.
    ///
    /// `args` are the process arguments; they are passed as an
    /// `Array<String>` when the entry declares a parameter for them.
    pub fn run_entry(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        let entry = self.program.lookup_fn(module, name).ok_or_else(|| {
            RuntimeError::new(format!("this package does not declare `{module}.{name}`"))
        })?;
        let decl = entry.decl.clone();
        let span = decl.span;

        let arguments = match decl.params.len() {
            0 => Vec::new(),
            1 => vec![EvaluatedArg {
                label: None,
                spread: false,
                slot: ArgSlot::Value(Value::Array(args.into_iter().map(Value::Str).collect())),
                span,
            }],
            other => {
                return Err(RuntimeError::new(format!(
                    "entry `{module}.{name}` declares {other} parameters"
                ))
                .at(span)
                .with_rule(
                    "An entry function takes either no parameters or one `Array<String>` of process arguments.",
                )
                .with_help(format!(
                    "write `fn {name}()` or `fn {name}(args: Array<String>)`"
                )));
            }
        };

        self.runtime.trace(TraceEvent::EntryEnter {
            module: module.to_string(),
            function: name.to_string(),
        });
        self.timings.push(Timing::start());

        let outcome = self
            .invoke(
                &Target {
                    name,
                    params: &decl.params,
                    body: &decl.body,
                    module: module.into(),
                    receiver: decl.receiver,
                    is_async: decl.is_async,
                    captures: &[],
                    return_type: decl.return_type.as_ref(),
                },
                None,
                arguments,
                span,
            )
            .and_then(|value| match value {
                // The host awaits the entry it chose, so an `async fn` entry
                // hands back its value rather than a handle the host cannot
                // settle.
                Value::Task(task) => self.settle(&task, span),
                value => Ok(value),
            });

        let timing = self
            .timings
            .pop()
            .expect("run_entry pushes exactly the one timing it pops");
        self.runtime.trace(TraceEvent::EntryExit {
            module: module.to_string(),
            function: name.to_string(),
            cpu: timing.cpu(),
            wait: timing.wait(),
        });

        outcome
    }

    fn resolved(&self, module: &str) -> Option<&'a ResolvedModule> {
        self.program.modules.get(module)
    }

    /// Resolves `name` as module `module` sees it, to the module that
    /// declares it and whatever `select` finds there.
    ///
    /// A module's own declaration answers first; failing that, the
    /// declaration a `use` imported under that name does. Which module
    /// answers matters beyond the declaration itself: a body runs in the
    /// module that declares it, so an imported function resolves its own
    /// names where it was written, not where it was called.
    fn find_declared<T>(
        &self,
        module: &str,
        name: &str,
        select: impl Fn(&'a ResolvedModule, &str) -> Option<T>,
    ) -> Option<(Rc<str>, T)> {
        let resolved = self.resolved(module)?;
        if let Some(found) = select(resolved, name) {
            return Some((module.into(), found));
        }
        let owner_name = resolved.imports.get(name)?;
        let owner = self.resolved(owner_name)?;
        select(owner, name).map(|found| (owner_name.as_str().into(), found))
    }

    fn find_function(&self, module: &str, name: &str) -> Option<(Rc<str>, Arc<FnDecl>)> {
        self.find_declared(module, name, |resolved, name| {
            Some(resolved.functions.get(name)?.decl.clone())
        })
    }

    /// The method `type_module.type_name` answers to, and the module whose
    /// body runs it.
    ///
    /// A type's methods usually live with the type. They do not have to: ADR
    /// 0006 allows `impl Trait for Type` in the module that declares the
    /// trait as well as the one that declares the type, so a conformance
    /// written elsewhere puts a method for this type in that other module.
    /// The orphan rule bounds the search — only a module declaring one of
    /// the two parties can have it — and the conformance itself says which
    /// module to look in.
    fn find_method(
        &self,
        type_module: &str,
        type_name: &str,
        name: &str,
    ) -> Option<(Rc<str>, Arc<FnDecl>)> {
        let key = (type_name.to_string(), name.to_string());
        if let Some(entry) = self.resolved(type_module).and_then(|m| m.methods.get(&key)) {
            return Some((type_module.into(), entry.decl.clone()));
        }
        for (module, resolved) in &self.program.modules {
            let conforms = resolved.conformances.values().any(|conformance| {
                conformance.type_module == type_module
                    && conformance.type_name == type_name
                    && conformance.methods.contains(name)
            });
            if !conforms {
                continue;
            }
            if let Some(entry) = resolved.methods.get(&key) {
                return Some((module.as_str().into(), entry.decl.clone()));
            }
        }
        None
    }

    fn find_struct(&self, module: &str, name: &str) -> Option<(Rc<str>, Arc<StructDecl>)> {
        self.find_declared(module, name, |resolved, name| {
            Some(resolved.structs.get(name)?.decl.clone())
        })
    }

    fn find_enum(&self, module: &str, name: &str) -> Option<(Rc<str>, Arc<EnumDecl>)> {
        self.find_declared(module, name, |resolved, name| {
            Some(resolved.enums.get(name)?.decl.clone())
        })
    }

    /// The module that declares the trait `name` as `module` sees it: itself
    /// when it declares the trait, and the module a `use` imported it from
    /// otherwise.
    fn declaring_module(&self, module: &str, name: &str) -> Option<Rc<str>> {
        let resolved = self.resolved(module)?;
        if resolved.traits.contains_key(name) {
            return Some(module.into());
        }
        let owner = resolved.imports.get(name)?;
        self.resolved(owner)?
            .traits
            .contains_key(name)
            .then(|| owner.as_str().into())
    }

    /// The module `head` names in `module`, when `use` imported it whole.
    fn imported_module(&self, module: &str, head: &str) -> Option<Rc<str>> {
        Some(
            self.resolved(module)?
                .module_imports
                .get(head)?
                .as_str()
                .into(),
        )
    }

    /// The exported declaration `owner.name` reaches, when `owner` exports
    /// one.
    ///
    /// A module-private declaration is not reachable qualified, exactly as
    /// it is not importable: `export` is the whole of a module's boundary.
    fn find_exported<T>(
        &self,
        owner: &str,
        name: &str,
        select: impl Fn(&'a ResolvedModule) -> Option<T>,
    ) -> Option<T> {
        let resolved = self.resolved(owner)?;
        if resolved.exported(name) != Some(true) {
            return None;
        }
        select(resolved)
    }

    /// The exported function of `owner` named `name`.
    fn exported_function(&self, owner: &str, name: &str) -> Option<Arc<FnDecl>> {
        self.find_exported(owner, name, |resolved| {
            Some(resolved.functions.get(name)?.decl.clone())
        })
    }

    /// `owner.name` as a value: an exported function is an ordinary handle,
    /// and an exported struct or enum is the type used as a value, exactly
    /// as a bare name for either would be.
    fn module_member(&self, owner: &str, name: &str, span: Span) -> Eval {
        if let Some(decl) = self.exported_function(owner, name) {
            return Ok(Value::Closure(Rc::new(Closure {
                is_async: decl.is_async,
                params: decl.params.clone(),
                body: Arc::new(decl.body.clone()),
                decl: Some(decl),
                module: owner.into(),
                captures: Vec::new(),
            })));
        }
        if self
            .find_exported(owner, name, |resolved| {
                resolved
                    .structs
                    .contains_key(name)
                    .then_some(())
                    .or_else(|| resolved.enums.contains_key(name).then_some(()))
            })
            .is_some()
        {
            return Ok(Value::Type(format!("{owner}.{name}").into()));
        }
        Err(self.no_export(owner, name, span).into())
    }

    /// Reports a qualified name that no export of `owner` answers, naming
    /// the module-private declaration when that is what went wrong.
    fn no_export(&self, owner: &str, name: &str, span: Span) -> RuntimeError {
        let exported = self.resolved(owner).map(|resolved| resolved.exported(name));
        match exported {
            Some(Some(false)) => RuntimeError::new(format!(
                "`{name}` is declared by module `{owner}`, but is not exported"
            ))
            .at(span)
            .with_rule("An `export` declaration is public; other declarations are module-private.")
            .with_help(format!("write `export` on `{name}` in module `{owner}`")),
            _ => RuntimeError::new(format!("module `{owner}` declares no `{name}`"))
                .at(span)
                .with_help(match self.resolved(owner) {
                    Some(resolved) if !resolved.exports().is_empty() => {
                        format!("module `{owner}` exports {}", resolved.exports().join(", "))
                    }
                    _ => format!("module `{owner}` exports nothing"),
                }),
        }
    }

    /// Whether `name` is a host module this module may address by name.
    fn is_host_module(&self, module: &str, name: &str) -> bool {
        self.resolved(module)
            .map(|m| m.host_uses.contains(name))
            .unwrap_or(false)
            || self.hosts.contains(name)
    }

    /// The host module an unqualified `use console.println` import names.
    fn host_item(&self, module: &str, name: &str) -> Option<Rc<str>> {
        self.resolved(module)?
            .host_items
            .get(name)
            .map(|m| m.as_str().into())
    }

    // ------------------------------------------------------------- budget

    /// Charges [`SAFEPOINT_FUEL`] and checks the deadline and cancellation
    /// flag, at a loop back edge, a function call, or an `await`.
    ///
    /// A stop surfaces as the ordinary [`RuntimeError`] `Budget` already
    /// produces, pointing at `span` — the loop, call, or await that hit the
    /// limit. It is not a Cove-level `Result`: like any other `RuntimeError`
    /// it propagates through `Control::Error` and cannot be caught by `?` or
    /// `match` in Cove source, so it terminates the run rather than failing
    /// one function of it.
    fn charge_safepoint(&mut self, span: Span) -> Result<(), RuntimeError> {
        if let Some(cancellation) = &self.cancellation {
            if cancellation.is_cancelled() {
                return Err(task_cancelled(span));
            }
        }
        if let Some(Err(error)) = self.hosts.with_budget(|budget| {
            budget
                .safepoint(SAFEPOINT_FUEL)
                .map_err(|stopped| budget.to_runtime_error(stopped))
        }) {
            return Err(error.at(span));
        }
        Ok(())
    }

    /// Records `wait` against every active [`Timing`] context, so a trace can
    /// separate the work a body did from the time it spent waiting for
    /// something else to finish — a host call, or a task.
    fn charge_wait(&mut self, wait: Duration) {
        for timing in &mut self.timings {
            timing.add_wait(wait);
        }
    }

    /// Dispatches a host call and records its wait against every active
    /// [`Timing`] context, so `EntryExit` and `TaskCompleted` can separate
    /// CPU work from time spent waiting on the host.
    fn call_host(
        &mut self,
        module: &str,
        op: &str,
        values: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let started = Instant::now();
        let result = self.hosts.call(module, op, values);
        self.charge_wait(started.elapsed());
        result.map_err(|e| e.at(span))
    }

    // ---------------------------------------------------------------- calls

    fn invoke(
        &mut self,
        target: &Target<'_>,
        receiver: Option<ArgSlot>,
        args: Vec<EvaluatedArg>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if self.depth >= MAX_CALL_DEPTH {
            return Err(RuntimeError::new(format!(
                "call depth limit of {MAX_CALL_DEPTH} reached while calling `{}`",
                target.name
            ))
            .at(span)
            .with_rule("Recursion depth is a runtime control, not a proof obligation."));
        }

        // A host-configured `max_call_depth` bounds one stack, and ADR 0008
        // gives each task a stack of its own, so it is checked against this
        // interpreter's own depth rather than against a count shared with
        // every other task: a shallow task must not be stopped because a
        // sibling is deep.
        let depth = self.depth + 1;
        let hosts = self.hosts;
        if let Some(Some(error)) =
            hosts.with_budget(|budget| match budget.limits().max_call_depth {
                Some(limit) if depth > limit => Some(budget.to_runtime_error(Stopped::CallDepth)),
                _ => None,
            })
        {
            return Err(error.at(span));
        }
        // Every call is also a safepoint, so the fuel charge counts the call
        // itself.
        self.charge_safepoint(span)?;

        self.depth += 1;
        let result = self.invoke_body(target, receiver, args, span);
        self.depth -= 1;
        if target.is_async {
            // An `async fn` is called like any other function and produces a
            // task, so its value is reachable only through `await`.
            //
            // The body runs here, at the call, and the handle it returns is
            // already settled. ADR 0008 gives a thread to `spawn`, which is
            // where the language says concurrency begins; nothing may depend
            // on when an `async fn` body ran, only on the value `await`
            // produces, so a body that is never awaited has still run.
            return Ok(Value::Task(Task::settled(result?)));
        }
        result
    }

    fn invoke_body(
        &mut self,
        target: &Target<'_>,
        receiver: Option<ArgSlot>,
        args: Vec<EvaluatedArg>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let mut env = Env::new(target.module.clone());
        for (name, value) in target.captures {
            env.declare(name.clone(), Place::binding(value.clone(), false));
        }

        match (target.receiver, receiver) {
            (Some(declared), Some(slot)) => {
                let place = match slot {
                    ArgSlot::Alias(place) => place,
                    ArgSlot::Value(value) => Place::binding(value, declared.is_var),
                };
                env.declare("self".into(), place);
            }
            (Some(_), None) => {
                return Err(RuntimeError::new(format!(
                    "`{}` is a method and needs a receiver",
                    target.name
                ))
                .at(span));
            }
            (None, Some(_)) => {
                return Err(
                    RuntimeError::new(format!("`{}` takes no receiver", target.name)).at(span),
                );
            }
            (None, None) => {}
        }

        self.bind_params(&mut env, target.params, args, target.name, span)?;
        let value = finish(self.eval_block(&mut env, target.body))?;
        Ok(match target.return_type {
            Some(ty) => self.coerce(&target.module, value, ty),
            None => value,
        })
    }

    /// Converts `value` to the written type `ty`, which today means exactly
    /// one thing: wrapping a concrete value as a `dyn Trait` value where a
    /// `dyn Trait` is written.
    ///
    /// This is the only implicit conversion in the language, and it happens
    /// where a type is *written*: a parameter, an annotated `let`, a struct
    /// field, and a declared return type. The checker has already decided the
    /// conversion is legal, so this only builds the representation. It walks
    /// into `Array<dyn Trait>` and `Option<dyn Trait>` because those are the
    /// forms whose elements are written as `dyn` too; every other generic
    /// argument is left alone, since a `Vector` is a shared handle whose
    /// elements cannot be rewritten behind its other aliases.
    fn coerce(&self, module: &str, value: Value, ty: &Type) -> Value {
        match &ty.kind {
            TypeKind::Dyn(trait_name) => {
                if matches!(value, Value::Dyn(_)) {
                    return value;
                }
                // A trait belongs to the module that declares it, which may
                // be one this module imported the trait from: a `dyn` value
                // built here must carry the same name a value built there
                // does, or the two would not compare equal.
                let qualified: Rc<str> = match self.declaring_module(module, &trait_name.node) {
                    Some(owner) => format!("{owner}.{}", trait_name.node).into(),
                    None => trait_name.node.as_str().into(),
                };
                Value::Dyn(Rc::new(DynValue {
                    trait_name: qualified,
                    value,
                }))
            }
            TypeKind::Named { path, args } if args.len() == 1 => {
                let Some(head) = path.last() else {
                    return value;
                };
                match (head.node.as_str(), value) {
                    ("Array", Value::Array(items)) => Value::Array(
                        items
                            .iter()
                            .map(|item| self.coerce(module, item.clone(), &args[0]))
                            .collect(),
                    ),
                    ("Option", Value::Enum(mut option)) if &*option.type_name == "Option" => {
                        for item in &mut option.payload {
                            *item = self.coerce(module, item.clone(), &args[0]);
                        }
                        Value::Enum(option)
                    }
                    (_, value) => value,
                }
            }
            _ => value,
        }
    }

    fn bind_params(
        &mut self,
        env: &mut Env,
        params: &[Param],
        args: Vec<EvaluatedArg>,
        what: &str,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let names: Vec<&str> = params.iter().map(|p| p.name.node.as_str()).collect();
        let variadic = params.last().map(|p| p.variadic).unwrap_or(false);
        let (mut slots, rest) = assign_labels(&names, args, what, variadic)?;

        for (index, param) in params.iter().enumerate() {
            let name: Rc<str> = param.name.node.as_str().into();
            if param.variadic {
                let mut items = Vec::new();
                if let Some(arg) = slots[index].as_ref() {
                    items.push(value_of(arg, &param.name.node, span)?);
                }
                for arg in &rest {
                    match &arg.slot {
                        ArgSlot::Value(Value::Array(values)) if arg.spread => {
                            items.extend(values.iter().cloned());
                        }
                        ArgSlot::Value(Value::Vector(storage)) if arg.spread => {
                            items.extend(storage.elements.borrow().iter().cloned());
                        }
                        ArgSlot::Value(_) if arg.spread => {
                            return Err(RuntimeError::new(
                                "`...` spreads an `Array` or a `Vector`",
                            )
                            .at(arg.span));
                        }
                        _ => items.push(value_of(arg, &param.name.node, arg.span)?),
                    }
                }
                // A variadic parameter is an immutable `Array<T>` inside the body.
                env.declare(name, Place::binding(Value::Array(items.into()), false));
                continue;
            }

            match slots[index].take() {
                Some(arg) => match (param.is_var, arg.slot) {
                    (true, ArgSlot::Alias(place)) => {
                        if !place.mutable {
                            return Err(var_arg_needs_mutable(&param.name.node, arg.span));
                        }
                        env.declare(name, place);
                    }
                    (true, ArgSlot::Value(_)) => {
                        return Err(RuntimeError::new(format!(
                            "parameter `{}` of `{what}` is declared `var`, but the call site passes a value",
                            param.name.node
                        ))
                        .at(arg.span)
                        .with_rule(
                            "A `var` parameter is a non-escaping inout alias, marked at both the declaration and the call site.",
                        )
                        .with_help(format!("write `{what}(var {})`", param.name.node)));
                    }
                    (false, ArgSlot::Alias(_)) => {
                        return Err(RuntimeError::new(format!(
                            "parameter `{}` of `{what}` is not declared `var`, so `var` cannot be written at the call site",
                            param.name.node
                        ))
                        .at(arg.span)
                        .with_rule(
                            "A `var` parameter is a non-escaping inout alias, marked at both the declaration and the call site.",
                        ));
                    }
                    // An ordinary parameter receives a shallow copy and is a
                    // read-only place inside the body.
                    (false, ArgSlot::Value(value)) => {
                        let value = match &param.ty {
                            Some(ty) => self.coerce(&env.module, value, ty),
                            None => value,
                        };
                        env.declare(name, Place::binding(value, false));
                    }
                },
                None => match &param.default {
                    // Default arguments are evaluated by the callee.
                    Some(default) => {
                        let value = finish(self.eval(env, default))?;
                        let value = match &param.ty {
                            Some(ty) => self.coerce(&env.module, value, ty),
                            None => value,
                        };
                        env.declare(name, Place::binding(value, false));
                    }
                    None => {
                        return Err(RuntimeError::new(format!(
                            "`{what}` needs an argument for `{}`",
                            param.name.node
                        ))
                        .at(span));
                    }
                },
            }
        }
        Ok(())
    }

    /// Calls a closure or a bound host operation held in a value.
    fn call_value_slots(
        &mut self,
        callee: Value,
        args: Vec<EvaluatedArg>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match callee {
            Value::Closure(closure) => {
                let module = closure.module.clone();
                self.invoke(
                    &Target {
                        name: "this closure",
                        params: &closure.params,
                        body: &closure.body,
                        module,
                        receiver: None,
                        is_async: closure.is_async,
                        captures: &closure.captures,
                        return_type: closure
                            .decl
                            .as_ref()
                            .and_then(|decl| decl.return_type.as_ref()),
                    },
                    None,
                    args,
                    span,
                )
            }
            Value::HostFn { module, op } => {
                let values = plain_values(args, &format!("{module}.{op}"))?;
                self.call_host(&module, &op, values, span)
            }
            other => {
                Err(RuntimeError::new(format!("`{}` is not callable", other.type_name())).at(span))
            }
        }
    }

    // ---------------------------------------------------------- statements

    fn eval_block(&mut self, env: &mut Env, block: &Block) -> Eval {
        env.push();
        let result = self.eval_block_body(env, block);
        env.pop();
        result
    }

    fn eval_block_body(&mut self, env: &mut Env, block: &Block) -> Eval {
        for stmt in &block.statements {
            match &stmt.kind {
                StmtKind::Let {
                    is_var,
                    name,
                    ty,
                    value,
                } => {
                    let value = self.eval(env, value)?;
                    let value = match ty {
                        Some(ty) => self.coerce(&env.module, value, ty),
                        None => value,
                    };
                    env.declare(name.node.as_str().into(), Place::binding(value, *is_var));
                }
                StmtKind::Expr(expr) => {
                    self.eval(env, expr)?;
                }
                StmtKind::Item(item) => match &item.kind {
                    ItemKind::Fn(decl) => {
                        let closure = self.make_closure(
                            env,
                            decl.is_async,
                            decl.params.clone(),
                            decl.body.clone(),
                            stmt.span,
                        )?;
                        env.declare(
                            decl.name.node.as_str().into(),
                            Place::binding(closure, false),
                        );
                    }
                    _ => {
                        return Err(unsupported(
                            "declaring a type inside a function body",
                            stmt.span,
                        )
                        .into())
                    }
                },
            }
        }
        match &block.tail {
            Some(tail) => self.eval(env, tail),
            None => Ok(Value::Unit),
        }
    }

    // --------------------------------------------------------- expressions

    fn eval(&mut self, env: &mut Env, expr: &Expr) -> Eval {
        let span = expr.span;
        match &expr.kind {
            ExprKind::Int(value) => Ok(Value::Int(*value)),
            ExprKind::Float(value) => Ok(Value::Float(*value)),
            ExprKind::Bool(value) => Ok(Value::Bool(*value)),
            ExprKind::Duration(value) => Ok(Value::Duration(*value)),
            ExprKind::Unit => Ok(Value::Unit),
            ExprKind::Str(parts) => {
                let mut text = String::new();
                for part in parts {
                    match part {
                        StrPart::Text(literal) => text.push_str(literal),
                        StrPart::Interpolation(expr) => {
                            let value = self.eval(env, expr)?;
                            text.push_str(&value.to_string());
                        }
                    }
                }
                Ok(Value::Str(text.into()))
            }
            ExprKind::Ident(name) => self.eval_ident(env, name, span),
            ExprKind::ArrayLit(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    values.push(self.eval(env, item)?);
                }
                Ok(Value::Array(values.into()))
            }
            ExprKind::Field { base, name } => self.eval_field(env, base, &name.node, span),
            ExprKind::Call {
                callee,
                generics: _,
                args,
                trailing,
            } => self.eval_call(env, callee, args, trailing.as_deref(), span),
            ExprKind::Unary { op, operand } => {
                let value = self.eval(env, operand)?;
                Ok(unary(*op, value, span)?)
            }
            ExprKind::Binary { op, lhs, rhs } => match op {
                // `&&` and `||` short-circuit; everything else is left to right.
                BinaryOp::And | BinaryOp::Or => {
                    let left = expect_bool(self.eval(env, lhs)?, *op, span)?;
                    if (*op == BinaryOp::And && !left) || (*op == BinaryOp::Or && left) {
                        return Ok(Value::Bool(left));
                    }
                    let right = expect_bool(self.eval(env, rhs)?, *op, span)?;
                    Ok(Value::Bool(right))
                }
                _ => {
                    let left = self.eval(env, lhs)?;
                    let right = self.eval(env, rhs)?;
                    Ok(binary(*op, left, right, span)?)
                }
            },
            ExprKind::Assign { op, target, value } => {
                let place = self.resolve_place(env, target)?;
                if !place.mutable {
                    return Err(RuntimeError::new(format!(
                        "cannot assign to `{}`, which is a read-only place",
                        describe_place(target)
                    ))
                    .at(span)
                    .with_rule("`let` creates a read-only place; `var` creates a mutable place.")
                    .with_help(format!(
                        "declare it with `var {}` to make it assignable",
                        describe_place(target)
                    ))
                    .into());
                }
                let new_value = match op {
                    None => self.eval(env, value)?,
                    Some(op) => {
                        let current = place.read(span)?;
                        let rhs = self.eval(env, value)?;
                        binary(*op, current, rhs, span)?
                    }
                };
                place.write(span, new_value)?;
                Ok(Value::Unit)
            }
            ExprKind::Try(inner) => {
                let value = self.eval(env, inner)?;
                match &value {
                    Value::Enum(result) if &*result.type_name == "Result" => {
                        if &*result.case == "Ok" {
                            Ok(result.payload.first().cloned().unwrap_or(Value::Unit))
                        } else {
                            Err(Control::Return(value))
                        }
                    }
                    Value::Enum(option) if &*option.type_name == "Option" => {
                        if &*option.case == "Some" {
                            Ok(option.payload.first().cloned().unwrap_or(Value::Unit))
                        } else {
                            Err(Control::Return(Value::none()))
                        }
                    }
                    other => {
                        let error = RuntimeError::new(format!(
                            "`?` needs a `Result` or an `Option`, but found `{}`",
                            other.type_name()
                        ))
                        .at(span)
                        .with_rule("`expr?` returns the error from the current function.");
                        // A task's value is observable only through `await`,
                        // so `?` cannot reach the `Result` inside one.
                        Err(match other {
                            Value::Task(_) => {
                                error.with_help("settle the task first, as in `task.await()?`")
                            }
                            _ => error,
                        }
                        .into())
                    }
                }
            }
            ExprKind::Await(inner) => {
                let value = self.eval(env, inner)?;
                self.charge_safepoint(span)?;
                Ok(self.settle_value(value, span)?)
            }
            ExprKind::Scope { name, body } => self.eval_scope(env, name, body),
            ExprKind::Block(block) => self.eval_block(env, block),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let test = self.eval(env, condition)?;
                let Value::Bool(test) = test else {
                    return Err(RuntimeError::new(format!(
                        "an `if` condition must be a `Bool`, but found `{}`",
                        test.type_name()
                    ))
                    .at(condition.span)
                    .with_rule("There are no implicit boolean conversions.")
                    .into());
                };
                if test {
                    self.eval_block(env, then_branch)
                } else {
                    match else_branch {
                        Some(branch) => self.eval(env, branch),
                        None => Ok(Value::Unit),
                    }
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                let value = self.eval(env, scrutinee)?;
                for arm in arms {
                    env.push();
                    let matched = self.match_pattern(env, &arm.pattern, &value);
                    match matched {
                        Ok(true) => {
                            let result = self.eval(env, &arm.body);
                            env.pop();
                            return result;
                        }
                        Ok(false) => env.pop(),
                        Err(error) => {
                            env.pop();
                            return Err(error);
                        }
                    }
                }
                // Static exhaustiveness checking is future work; until then a
                // `match` that covers no case fails here instead of silently
                // producing a value.
                Err(
                    RuntimeError::new(format!("no `match` arm covers `{value}`"))
                        .at(span)
                        .with_rule("`match` must cover every enum case.")
                        .with_help("add an arm for this case, or a `_` arm")
                        .into(),
                )
            }
            ExprKind::For {
                binding,
                iterable,
                body,
            } => {
                let items = self.iterable_items(env, iterable)?;
                // A loop is an expression: it evaluates to `Unit` unless a
                // `break expr` inside it says otherwise.
                let mut result_value = Value::Unit;
                for item in items {
                    // Once per iteration, at the back edge: this is the
                    // safepoint that bounds a `for` over an unbounded
                    // iterable, since Cove does not prove termination.
                    self.charge_safepoint(span)?;
                    env.push();
                    env.declare(binding.node.as_str().into(), Place::binding(item, false));
                    let result = self.eval_block(env, body);
                    env.pop();
                    match result {
                        Ok(_) => {}
                        Err(Control::Break(value)) => {
                            result_value = value;
                            break;
                        }
                        Err(Control::Continue) => continue,
                        Err(other) => return Err(other),
                    }
                }
                Ok(result_value)
            }
            ExprKind::While { condition, body } => loop {
                let test = self.eval(env, condition)?;
                let Value::Bool(test) = test else {
                    return Err(RuntimeError::new(format!(
                        "a `while` condition must be a `Bool`, but found `{}`",
                        test.type_name()
                    ))
                    .at(condition.span)
                    .into());
                };
                if !test {
                    return Ok(Value::Unit);
                }
                // Once per iteration, at the back edge: this is the
                // safepoint that bounds a non-terminating `while`, which is
                // otherwise unbounded by anything the type system proves.
                self.charge_safepoint(span)?;
                match self.eval_block(env, body) {
                    Ok(_) => {}
                    Err(Control::Break(value)) => return Ok(value),
                    Err(Control::Continue) => continue,
                    Err(other) => return Err(other),
                }
            },
            ExprKind::Return(value) => {
                let value = match value {
                    Some(expr) => self.eval(env, expr)?,
                    None => Value::Unit,
                };
                Err(Control::Return(value))
            }
            ExprKind::Break(value) => {
                let value = match value {
                    Some(expr) => self.eval(env, expr)?,
                    None => Value::Unit,
                };
                Err(Control::Break(value))
            }
            ExprKind::Continue => Err(Control::Continue),
            ExprKind::Lambda {
                is_async,
                params,
                body,
            } => self
                .make_closure(env, *is_async, params.clone(), body.clone(), span)
                .map_err(Control::from),
            // A range is an ordinary value, so it evaluates like any other
            // expression and `for` simply iterates the value it produces.
            ExprKind::Range {
                start,
                end,
                inclusive_end,
            } => {
                let start = expect_int(self.eval(env, start)?, "a range bound", span)?;
                let end = expect_int(self.eval(env, end)?, "a range bound", span)?;
                Ok(Value::Range {
                    start,
                    end,
                    inclusive_end: *inclusive_end,
                })
            }
        }
    }

    fn make_closure(
        &mut self,
        env: &mut Env,
        is_async: bool,
        params: Vec<Param>,
        body: Block,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // Closures capture by value at creation time, like every other copy.
        let mut mentioned = BTreeSet::new();
        mention_block(&body, &mut mentioned);
        let captures = env.captures(&mentioned, span)?;
        Ok(Value::Closure(Rc::new(Closure {
            is_async,
            params,
            decl: None,
            body: Arc::new(body),
            module: env.module.clone(),
            captures,
        })))
    }

    // ---------------------------------------------------------------- tasks

    /// Evaluates `scope name { ... }`.
    ///
    /// The Language Card's rule is the whole of this function: leaving the
    /// scope waits for or cancels its child tasks. The scope's value is the
    /// value of its block, so a scope is an expression like any other block.
    fn eval_scope(&mut self, env: &mut Env, name: &Ident, body: &Block) -> Eval {
        let scope = TaskScope::new(name.node.as_str().into());
        env.push();
        env.declare(
            name.node.as_str().into(),
            Place::binding(Value::TaskScope(scope.clone()), false),
        );
        let result = self.eval_block(env, body);
        env.pop();
        let left = self.leave_scope(&scope, result);
        scope.close();
        left
    }

    /// Waits for or cancels the children of a scope that is being left.
    ///
    /// A normal exit waits for every task the body did not await, in spawn
    /// order, and discards its value: a scope waits for its children, it does
    /// not collect them. A task that fails is not swallowed — a `RuntimeError`
    /// propagates as itself, and a task whose value is `Err(error)` returns
    /// that error from the enclosing function, exactly as `?` would. A task
    /// the program itself cancelled is neither: the program asked for that
    /// stop, so leaving the scope is not the place to complain about it.
    /// Either way the tasks still running are cancelled and waited for, as
    /// they are when the body itself leaves early through `return`, `?`, or
    /// an error.
    ///
    /// Waiting happens in spawn order, which is an order of *observation*
    /// only: the tasks ran at the same time on threads of their own, so only
    /// the set of effects a scope produces is defined, never their sequence.
    fn leave_scope(&mut self, scope: &Rc<TaskScope>, result: Eval) -> Eval {
        let value = match result {
            Ok(value) => value,
            early => {
                self.cancel_scope(scope);
                return early;
            }
        };

        // Waiting reads the scope's children by index rather than from a
        // snapshot, so a scope that grew while it was being left is still
        // waited for to the end.
        let mut index = 0;
        while let Some(task) = scope.task_at(index) {
            index += 1;
            if !task.is_running() {
                continue;
            }
            self.join_task(&task);
            // The state is read and released before anything else runs, so
            // cancelling the rest of the scope can borrow these same tasks.
            let outcome = match &*task.state.borrow() {
                TaskState::Settled(value) => failure_of(value).map(Control::error_value),
                TaskState::Failed(error) => Some(Control::Error(error.clone())),
                TaskState::Cancelled | TaskState::Running => None,
            };
            if let Some(control) = outcome {
                self.cancel_scope(scope);
                return Err(control);
            }
        }
        Ok(value)
    }

    /// Cancels every running child of `scope` and waits for it to stop.
    ///
    /// Every child is asked first and waited for afterwards, so they stop at
    /// the same time rather than one after another. Leaving a scope waits for
    /// or cancels its children, so this does both: a scope never outlives a
    /// thread it started.
    fn cancel_scope(&mut self, scope: &Rc<TaskScope>) {
        scope.cancel_running();
        let mut index = 0;
        while let Some(task) = scope.task_at(index) {
            index += 1;
            self.join_task(&task);
        }
    }

    /// Waits for a task's thread, charging the time against this body's
    /// [`Timing`] as wait rather than as work.
    ///
    /// A body blocked on `await` is doing nothing, exactly as a body blocked
    /// on a host call is. Counting it as CPU would report a scope that waits
    /// for two tasks as having computed for as long as they ran, which is the
    /// attribution ADR 0001 asks a trace to get right.
    ///
    /// This is also the one place that learns whether a cancellation actually
    /// stopped a task, so it is where `TaskCancelled` is traced. A task is
    /// waited for once, so the event is recorded once; a task that had
    /// already finished is unaffected by cancellation, and tracing it as
    /// cancelled would say work was stopped that in fact happened.
    fn join_task(&mut self, task: &Rc<Task>) {
        if !task.is_running() {
            return;
        }
        let started = Instant::now();
        task.join();
        self.charge_wait(started.elapsed());
        if matches!(&*task.state.borrow(), TaskState::Cancelled) {
            self.runtime
                .trace(TraceEvent::TaskCancelled { id: task.id });
        }
    }

    /// Waits for a task's thread and returns the value its body produced.
    ///
    /// A task's body runs at most once and is waited for at most once, so
    /// awaiting the same handle twice returns the same value and repeats no
    /// effect.
    fn settle(&mut self, task: &Rc<Task>, span: Span) -> Result<Value, RuntimeError> {
        self.join_task(task);
        match &*task.state.borrow() {
            TaskState::Settled(value) => Ok(value.clone()),
            TaskState::Failed(error) => Err(error.clone()),
            TaskState::Cancelled => Err(awaiting_a_cancelled_task(task, span)),
            TaskState::Running => {
                unreachable!("joining a task leaves it settled, failed, or cancelled")
            }
        }
    }

    /// `await expr`, and the postfix `expr.await()` that means the same thing.
    fn settle_value(&mut self, value: Value, span: Span) -> Result<Value, RuntimeError> {
        match value {
            Value::Task(task) => self.settle(&task, span),
            other => Err(RuntimeError::new(format!(
                "`await` needs a task, but found `{}`",
                other.type_name()
            ))
            .at(span)
            .with_rule(
                "`await` settles a task. Only a task spawned into a scope, or one returned by an `async fn`, has a value to settle.",
            )
            .with_help("call an `async fn`, or spawn the work into a task scope, and await that handle")),
        }
    }

    /// `scope.spawn { ... }`, which starts a thread for the body.
    ///
    /// Converting the closure for the new thread *is* the task-safety check:
    /// what may cross a task boundary is exactly what a thread can own, so a
    /// capture that may not cross is reported at the `spawn` that would have
    /// carried it, before any thread exists.
    fn spawn(
        &mut self,
        scope: &Rc<TaskScope>,
        body: Value,
        span: Span,
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
            .with_rule(crate::task::TASK_SAFETY_RULE)
            .with_help(found.help("spawning"))
        })?;

        let id = self.runtime.next_task_id();
        // Traced before the thread starts, so a task is never seen completing
        // before it was seen spawning.
        self.runtime.trace(TraceEvent::TaskSpawned {
            id,
            parent: self.task_stack.last().copied(),
            scope: scope.name.to_string(),
        });

        let cancellation = Cancellation::new();
        let runtime = self.runtime.clone();
        let flag = cancellation.clone();
        let thread = std::thread::Builder::new()
            .name(format!("cove task {id}"))
            .spawn(move || run_task(&runtime, id, flag, body, span))
            .map_err(|e| {
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

    /// Dispatches the operations of a task scope and of a task handle.
    fn call_task_method(
        &mut self,
        env: &mut Env,
        receiver: Value,
        name: &str,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Eval {
        let arguments = self.eval_args(env, args, trailing)?;
        let mut values = plain_values(arguments, name)?;
        match (&receiver, name) {
            (Value::TaskScope(scope), "spawn") => {
                if values.len() != 1 {
                    return Err(RuntimeError::new(format!(
                        "`spawn` takes one trailing closure, but {} argument(s) were given",
                        values.len()
                    ))
                    .at(span)
                    .with_help(format!("write `{}.spawn {{ ... }}`", scope.name))
                    .into());
                }
                Ok(self.spawn(scope, values.remove(0), span)?)
            }
            (Value::Task(task), "await") => {
                expect_no_arguments("await", &values, span)?;
                self.charge_safepoint(span)?;
                Ok(self.settle(task, span)?)
            }
            (Value::Task(task), "cancel") => {
                expect_no_arguments("cancel", &values, span)?;
                // Asking is all this does. A cancelled task stops at its next
                // safepoint, and whether it stopped or had already finished is
                // known only once something waits for it — which is what
                // `await` and leaving the scope do, and where `TaskCancelled`
                // is traced.
                task.cancel();
                Ok(Value::Unit)
            }
            (_, "await") => {
                self.charge_safepoint(span)?;
                Ok(self.settle_value(receiver.clone(), span)?)
            }
            (other, _) => Err(RuntimeError::new(format!(
                "`{}` has no method `{name}`",
                other.type_name()
            ))
            .at(span)
            .into()),
        }
    }

    /// Dispatches the one operation of a `Shared`: `lock`.
    ///
    /// There is no `get` and no `set`, by design. Every access is scoped, so
    /// a read-modify-write cannot be written as two operations that race;
    /// see [`crate::shared`].
    fn call_shared_method(
        &mut self,
        env: &mut Env,
        receiver: Value,
        name: &str,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Eval {
        let Value::Shared(cell) = receiver else {
            unreachable!("only a `Shared` receiver reaches this dispatch");
        };
        if name != "lock" {
            return Err(RuntimeError::new(format!("`Shared` has no method `{name}`"))
                .at(span)
                .with_rule(
                    "`lock` is a `Shared`'s only operation: every access to the value it holds is scoped, so there is no `get` and no `set`.",
                )
                .with_help("write `shared.lock(fn(var value) { ... })`")
                .into());
        }
        let arguments = self.eval_args(env, args, trailing)?;
        let mut values = plain_values(arguments, name)?;
        if values.len() != 1 {
            return Err(RuntimeError::new(format!(
                "`lock` takes one closure, but {} argument(s) were given",
                values.len()
            ))
            .at(span)
            .with_help("write `shared.lock(fn(var value) { ... })`")
            .into());
        }
        let body = values.remove(0);
        let Value::Closure(closure) = &body else {
            return Err(RuntimeError::new(format!(
                "`lock` takes the work to run as a closure, but found `{}`",
                body.type_name()
            ))
            .at(span)
            .with_help("write `shared.lock(fn(var value) { ... })`")
            .into());
        };
        let Some(param) = closure.params.first() else {
            return Err(RuntimeError::new(
                "`lock` gives the wrapped value to its closure, but this closure takes no parameter",
            )
            .at(span)
            .with_help("write `shared.lock(fn(var value) { ... })`")
            .into());
        };
        // A closure declaring `var` receives the wrapped value as an alias and
        // mutates it where it lies; one that does not receives a copy, exactly
        // as an ordinary parameter does anywhere else in the language.
        let wants_alias = param.is_var;
        Ok(cell.lock(span, |value| {
            let place = Place::binding(value, true);
            let slot = match wants_alias {
                true => ArgSlot::Alias(place.clone()),
                false => ArgSlot::Value(place.read(span)?),
            };
            let result = self.call_value_slots(
                body.clone(),
                vec![EvaluatedArg {
                    label: None,
                    spread: false,
                    slot,
                    span,
                }],
                span,
            )?;
            let updated = place.read(span)?;
            Ok((result, updated))
        })?)
    }

    fn iterable_items(&mut self, env: &mut Env, expr: &Expr) -> Result<Vec<Value>, Control> {
        // Iteration reads a snapshot of the elements; rejecting structural
        // mutation during iteration is future work.
        match self.eval(env, expr)? {
            Value::Array(items) => Ok(items.iter().cloned().collect()),
            Value::Vector(storage) => Ok(storage.elements.borrow().clone()),
            // An empty or reversed range such as `3..<0` iterates zero times.
            Value::Range {
                start,
                end,
                inclusive_end,
            } => Ok(RangeBounds::of(start, end, inclusive_end).items()),
            // A `Set` is `BTreeSet<MapKey>`-backed, so it iterates its
            // elements in ascending order, the same order `Display` shows.
            Value::Set(items) => Ok(items.iter().map(|key| key.to_value()).collect()),
            // A `Map` iterates in ascending key order, matching its
            // `BTreeMap` storage. Each binding is a `MapEntry` carrying that
            // iteration's `key` and `value`, the same shape `Map.of` accepts.
            Value::Map(entries) => Ok(entries
                .iter()
                .map(|(key, value)| {
                    Value::Struct(Box::new(StructValue {
                        type_name: "MapEntry".into(),
                        fields: vec![("key".into(), key.to_value()), ("value".into(), value.clone())],
                    }))
                })
                .collect()),
            other => Err(RuntimeError::new(format!(
                "`for` iterates an `Array`, a `Vector`, a `Range`, a `Set`, or a `Map`, but found `{}`",
                other.type_name()
            ))
            .at(expr.span)
            .into()),
        }
    }

    fn eval_ident(&mut self, env: &mut Env, name: &str, span: Span) -> Eval {
        if let Some(place) = env.lookup(name) {
            return Ok(place.read(span)?);
        }
        if name == "None" {
            return Ok(Value::none());
        }
        let module = env.module.clone();
        if let Some((owner, decl)) = self.find_function(&module, name) {
            return Ok(Value::Closure(Rc::new(Closure {
                is_async: decl.is_async,
                params: decl.params.clone(),
                body: Arc::new(decl.body.clone()),
                decl: Some(decl),
                module: owner,
                captures: Vec::new(),
            })));
        }
        // A type is named by the module that declares it, wherever it is
        // written: two modules may each declare a `Config`, and a value has
        // to say which one it is.
        if let Some((owner, _)) = self.find_struct(&module, name) {
            return Ok(Value::Type(format!("{owner}.{name}").into()));
        }
        if let Some((owner, _)) = self.find_enum(&module, name) {
            return Ok(Value::Type(format!("{owner}.{name}").into()));
        }
        if builtins::is_builtin_type(name) {
            return Ok(Value::Type(name.into()));
        }
        if let Some(owner) = self.imported_module(&module, name) {
            return Err(RuntimeError::new(format!("`{name}` is a module, not a value"))
                .at(span)
                .with_rule(
                    "A module imported whole is a namespace; its exported declarations are the values.",
                )
                .with_help(format!(
                    "name one of its exports, such as `{name}.<declaration>`, or import the declaration with `use {owner}.<declaration>`"
                ))
                .into());
        }
        if self.is_host_module(&module, name) {
            return Ok(Value::HostModule(name.into()));
        }
        if let Some(host) = self.host_item(&module, name) {
            return Ok(Value::HostFn {
                module: host,
                op: name.into(),
            });
        }
        Err(
            RuntimeError::new(format!("cannot find `{name}` in this scope"))
                .at(span)
                .into(),
        )
    }

    fn eval_field(&mut self, env: &mut Env, base: &Expr, name: &str, span: Span) -> Eval {
        if let ExprKind::Ident(head) = &base.kind {
            if env.lookup(head).is_none() {
                let module = env.module.clone();
                if let Some((owner, decl)) = self.find_enum(&module, head) {
                    return Ok(self.enum_case(&owner, &decl, name, Vec::new(), span)?);
                }
                if self.is_host_module(&module, head) {
                    return Ok(Value::HostFn {
                        module: head.as_str().into(),
                        op: name.into(),
                    });
                }
                // `booking.create` and `booking.Status`: a module imported
                // whole answers with the exported declaration it names.
                if let Some(owner) = self.imported_module(&module, head) {
                    return self.module_member(&owner, name, span);
                }
            }
        }

        let base_value = self.eval(env, base)?;
        match &base_value {
            Value::Struct(value) => match value.get(name) {
                Some(field) => Ok(field.clone()),
                None => Err(no_field(&value.type_name, name, span).into()),
            },
            // `booking.Status.Confirmed`, once `booking.Status` named the
            // type: a case of an enum reached through its module.
            Value::Type(type_name) => match type_name.rsplit_once('.') {
                Some((owner, short)) => match self.find_enum(owner, short) {
                    Some((owner, decl)) => {
                        Ok(self.enum_case(&owner, &decl, name, Vec::new(), span)?)
                    }
                    None => Err(no_field(type_name, name, span).into()),
                },
                None => Err(no_field(type_name, name, span).into()),
            },
            Value::HostModule(module) => Ok(Value::HostFn {
                module: module.clone(),
                op: name.into(),
            }),
            other => Err(RuntimeError::new(format!(
                "`{}` has no field `{name}`",
                other.type_name()
            ))
            .at(span)
            .into()),
        }
    }

    /// The cases and associated functions `Enum.name` could have meant.
    fn known_members(&self, module: &str, decl: &Arc<EnumDecl>) -> String {
        let cases: Vec<&str> = decl
            .cases
            .iter()
            .map(|case| case.name.node.as_str())
            .collect();
        let mut help = format!("known cases: {}", cases.join(", "));
        let functions: Vec<&str> = match self.resolved(module) {
            Some(resolved) => resolved
                .methods
                .keys()
                .filter(|(type_name, _)| *type_name == decl.name.node)
                .map(|(_, name)| name.as_str())
                .collect(),
            None => Vec::new(),
        };
        if !functions.is_empty() {
            help.push_str(&format!("; known functions: {}", functions.join(", ")));
        }
        help
    }

    /// Builds one case of an enum declared in `module`.
    fn enum_case(
        &mut self,
        module: &str,
        decl: &Arc<EnumDecl>,
        case: &str,
        payload: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let Some(found) = decl.cases.iter().find(|c| c.name.node == case) else {
            return Err(RuntimeError::new(format!(
                "enum `{}` has no case or associated function `{case}`",
                decl.name.node
            ))
            .at(span)
            .with_rule(
                "`Enum.name` is a case when the enum declares one, and otherwise an associated function declared in an `impl` block.",
            )
            .with_help(self.known_members(module, decl)));
        };
        if found.payload.len() != payload.len() {
            return Err(RuntimeError::new(format!(
                "case `{}.{case}` carries {} value(s), but {} were given",
                decl.name.node,
                found.payload.len(),
                payload.len()
            ))
            .at(span));
        }
        Ok(Value::Enum(Box::new(EnumValue {
            type_name: format!("{module}.{}", decl.name.node).into(),
            case: case.into(),
            payload,
        })))
    }

    // ---------------------------------------------------------------- calls

    fn eval_call(
        &mut self,
        env: &mut Env,
        callee: &Expr,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Eval {
        match &callee.kind {
            ExprKind::Ident(name) => {
                if let Some(place) = env.lookup(name) {
                    let value = place.read(span)?;
                    let args = self.eval_args(env, args, trailing)?;
                    return Ok(self.call_value_slots(value, args, span)?);
                }
                let module = env.module.clone();
                if let Some((owner, decl)) = self.find_function(&module, name) {
                    let args = self.eval_args(env, args, trailing)?;
                    return Ok(self.invoke(
                        &Target {
                            name,
                            params: &decl.params,
                            body: &decl.body,
                            module: owner,
                            receiver: decl.receiver,
                            is_async: decl.is_async,
                            captures: &[],
                            return_type: decl.return_type.as_ref(),
                        },
                        None,
                        args,
                        span,
                    )?);
                }
                if let Some((owner, decl)) = self.find_struct(&module, name) {
                    let args = self.eval_args(env, args, trailing)?;
                    return Ok(self.init_struct(&owner, &decl, args, span)?);
                }
                if self.find_enum(&module, name).is_some() {
                    return Err(
                        RuntimeError::new(format!("`{name}` is an enum, not a function"))
                            .at(span)
                            .with_help(format!("name a case, such as `{name}.Case(...)`"))
                            .into(),
                    );
                }
                if let Some(host) = self.host_item(&module, name) {
                    let args = self.eval_args(env, args, trailing)?;
                    let values = plain_values(args, name)?;
                    return Ok(self.call_host(&host, name, values, span)?);
                }
                if name == "MapEntry" {
                    let args = self.eval_args(env, args, trailing)?;
                    return Ok(init_map_entry(args, span)?);
                }
                if builtins::is_assertion(name) {
                    return self.assertion(env, name, args, trailing, span);
                }
                if builtins::is_constructor(name) {
                    let args = self.eval_args(env, args, trailing)?;
                    let values = plain_values(args, name)?;
                    return Ok(builtins::call_constructor(name, values, span)?);
                }
                if name == "None" {
                    return Err(RuntimeError::new("`None` is a value, not a call")
                        .at(span)
                        .with_help("write `None`")
                        .into());
                }
                Err(
                    RuntimeError::new(format!("cannot find `{name}` in this scope"))
                        .at(span)
                        .into(),
                )
            }
            ExprKind::Field { base, name } => {
                if let ExprKind::Ident(head) = &base.kind {
                    if env.lookup(head).is_none() {
                        let module = env.module.clone();
                        if self.is_host_module(&module, head) {
                            let args = self.eval_args(env, args, trailing)?;
                            let values = plain_values(args, &format!("{head}.{}", name.node))?;
                            return Ok(self.call_host(head, &name.node, values, span)?);
                        }
                        if let Some((owner, enum_decl)) = self.find_enum(&module, head) {
                            // A case wins over an associated function of the
                            // same name, so naming a case never changes
                            // meaning when an `impl` block is added.
                            let is_case = enum_decl
                                .cases
                                .iter()
                                .any(|case| case.name.node == name.node);
                            if !is_case {
                                if let Some((declaring, decl)) =
                                    self.find_method(&owner, head, &name.node)
                                {
                                    let args = self.eval_args(env, args, trailing)?;
                                    return Ok(self.invoke(
                                        &Target {
                                            name: &name.node,
                                            params: &decl.params,
                                            body: &decl.body,
                                            module: declaring,
                                            receiver: decl.receiver,
                                            is_async: decl.is_async,
                                            captures: &[],
                                            return_type: decl.return_type.as_ref(),
                                        },
                                        None,
                                        args,
                                        span,
                                    )?);
                                }
                            }
                            let args = self.eval_args(env, args, trailing)?;
                            let values = plain_values(args, &format!("{head}.{}", name.node))?;
                            return Ok(
                                self.enum_case(&owner, &enum_decl, &name.node, values, span)?
                            );
                        }
                        if let Some((owner, _)) = self.find_struct(&module, head) {
                            if let Some((declaring, decl)) =
                                self.find_method(&owner, head, &name.node)
                            {
                                let args = self.eval_args(env, args, trailing)?;
                                return Ok(self.invoke(
                                    &Target {
                                        name: &name.node,
                                        params: &decl.params,
                                        body: &decl.body,
                                        module: declaring,
                                        receiver: decl.receiver,
                                        is_async: decl.is_async,
                                        captures: &[],
                                        return_type: decl.return_type.as_ref(),
                                    },
                                    None,
                                    args,
                                    span,
                                )?);
                            }
                        }
                        // `booking.create(...)`: a module imported whole is
                        // called through the declaration it exports.
                        if let Some(owner) = self.imported_module(&module, head) {
                            if let Some(decl) = self.exported_function(&owner, &name.node) {
                                let args = self.eval_args(env, args, trailing)?;
                                return Ok(self.invoke(
                                    &Target {
                                        name: &name.node,
                                        params: &decl.params,
                                        body: &decl.body,
                                        module: owner,
                                        receiver: decl.receiver,
                                        is_async: decl.is_async,
                                        captures: &[],
                                        return_type: decl.return_type.as_ref(),
                                    },
                                    None,
                                    args,
                                    span,
                                )?);
                            }
                            if let Some(decl) = self.find_exported(&owner, &name.node, |resolved| {
                                Some(resolved.structs.get(&name.node)?.decl.clone())
                            }) {
                                let args = self.eval_args(env, args, trailing)?;
                                return Ok(self.init_struct(&owner, &decl, args, span)?);
                            }
                            if self
                                .find_exported(&owner, &name.node, |resolved| {
                                    resolved.enums.get(&name.node)
                                })
                                .is_some()
                            {
                                return Err(RuntimeError::new(format!(
                                    "`{head}.{}` is an enum, not a function",
                                    name.node
                                ))
                                .at(span)
                                .with_help(format!(
                                    "name a case, such as `{head}.{}.Case(...)`",
                                    name.node
                                ))
                                .into());
                            }
                            return Err(self.no_export(&owner, &name.node, span).into());
                        }
                        if builtins::is_builtin_type(head) {
                            let args = self.eval_args(env, args, trailing)?;
                            let values = plain_values(args, &format!("{head}.{}", name.node))?;
                            return Ok(builtins::call_associated(head, &name.node, values, span)?);
                        }
                    }
                }
                self.eval_method_call(env, base, &name.node, args, trailing, span)
            }
            _ => {
                let value = self.eval(env, callee)?;
                let args = self.eval_args(env, args, trailing)?;
                Ok(self.call_value_slots(value, args, span)?)
            }
        }
    }

    /// `assert(condition)` and `assertEqual(actual, expected)`.
    ///
    /// The source text of each argument is read back out of the
    /// [`SourceMap`] with the expression's own span, so a failure message
    /// names the condition in the words the test was written in. That is
    /// what makes these builtins rather than library functions.
    fn assertion(
        &mut self,
        env: &mut Env,
        name: &str,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Eval {
        let spans: Vec<Span> = args
            .iter()
            .map(|arg| arg.value.span)
            .chain(trailing.map(|expr| expr.span))
            .collect();
        let evaluated = self.eval_args(env, args, trailing)?;
        let values = plain_values(evaluated, name)?;
        let sources: Vec<&str> = spans.iter().map(|span| self.source_text(*span)).collect();
        let outcome = builtins::call_assertion(name, values, &sources, span)?;
        if let Value::Enum(result) = &outcome {
            if &*result.case == "Err" {
                self.assertion_failure = Some((span, result.payload[0].to_string()));
            }
        }
        Ok(outcome)
    }

    fn eval_method_call(
        &mut self,
        env: &mut Env,
        receiver: &Expr,
        name: &str,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Eval {
        // The receiver is evaluated before the arguments: evaluation is left
        // to right everywhere.
        let place = self.resolve_place_opt(env, receiver)?;
        let mut temporary = match &place {
            Some(_) => None,
            None => Some(self.eval(env, receiver)?),
        };

        // Dynamic dispatch: a `dyn Trait` receiver is unwrapped to the
        // concrete value it carries, and the implementation is found from
        // *that* value's type. This is what makes the dispatch dynamic — the
        // static type says only which trait the method must come from.
        let mut place = place;
        let dyn_receiver = match (&place, &temporary) {
            (Some(place), _) => place.with_ref(span, |value| match value {
                Value::Dyn(d) => Some(d.value.clone()),
                _ => None,
            })?,
            (_, Some(Value::Dyn(d))) => Some(d.value.clone()),
            _ => None,
        };
        if let Some(concrete) = dyn_receiver {
            place = None;
            temporary = Some(concrete);
        }

        let type_name = match (&place, &temporary) {
            (Some(place), _) => place.with_ref(span, Value::type_name)?,
            (_, Some(value)) => value.type_name(),
            _ => unreachable!("a receiver is either a place or a temporary"),
        };

        if let Some((type_module, short)) = type_name.rsplit_once('.') {
            if let Some((module, decl)) = self.find_method(type_module, short, name) {
                let receiver_slot = match decl.receiver {
                    Some(Receiver { is_var: true, .. }) => {
                        let Some(place) = place else {
                            return Err(var_self_needs_place(name, receiver, span).into());
                        };
                        if !place.mutable {
                            return Err(var_self_needs_mutable(name, receiver, span).into());
                        }
                        ArgSlot::Alias(place)
                    }
                    _ => ArgSlot::Value(match (place, temporary) {
                        (Some(place), _) => place.read(span)?,
                        (_, Some(value)) => value,
                        _ => unreachable!("a receiver is either a place or a temporary"),
                    }),
                };
                let args = self.eval_args(env, args, trailing)?;
                return Ok(self.invoke(
                    &Target {
                        name,
                        params: &decl.params,
                        body: &decl.body,
                        module,
                        receiver: decl.receiver,
                        is_async: decl.is_async,
                        captures: &[],
                        return_type: decl.return_type.as_ref(),
                    },
                    Some(receiver_slot),
                    args,
                    span,
                )?);
            }
        }

        // `snapshot()` is the builtin `Snapshot` trait's one method. A struct
        // or enum conformance was already tried above like any other method;
        // reaching here means either the receiver is a builtin value type,
        // or it is a struct or enum with no conformance, which
        // `Interpreter::snapshot` reports.
        if name == "snapshot" {
            let args = self.eval_args(env, args, trailing)?;
            if !args.is_empty() {
                return Err(RuntimeError::new(format!(
                    "`snapshot` takes 0 argument(s), but {} were given",
                    args.len()
                ))
                .at(span)
                .into());
            }
            let receiver_value = match (place, temporary) {
                (Some(place), _) => place.read(span)?,
                (_, Some(value)) => value,
                _ => unreachable!("a receiver is either a place or a temporary"),
            };
            return Ok(self.snapshot(&receiver_value, span)?);
        }

        // `Shared` is a runtime value rather than a declared type, and `lock`
        // takes the closure itself rather than the closure's value, so it is
        // dispatched here.
        if type_name == "Shared" {
            let receiver_value = match (&place, &temporary) {
                (Some(place), _) => place.read(span)?,
                (_, Some(value)) => value.clone(),
                _ => unreachable!("a receiver is either a place or a temporary"),
            };
            return self.call_shared_method(env, receiver_value, name, args, trailing, span);
        }

        // A task scope and a task handle are runtime values rather than
        // declared types, so their operations are dispatched here.
        // `examples/tasks/load.cove` writes the await as a postfix call, and
        // `bookings.await()` means what `await bookings` means.
        if name == "await" || matches!(type_name.as_str(), "TaskScope" | "Task") {
            let receiver_value = match (&place, &temporary) {
                (Some(place), _) => place.read(span)?,
                (_, Some(value)) => value.clone(),
                _ => unreachable!("a receiver is either a place or a temporary"),
            };
            return self.call_task_method(env, receiver_value, name, args, trailing, span);
        }

        // `push` and `freeze` take a `var self` receiver.
        if builtins::is_mutating_method(name) {
            if let Some(place) = &place {
                if !place.mutable {
                    return Err(var_self_needs_mutable(name, receiver, span).into());
                }
            } else if name == "push" {
                return Err(var_self_needs_place(name, receiver, span).into());
            }
        }

        let args = self.eval_args(env, args, trailing)?;
        let values = plain_values(args, name)?;

        if name == "freeze" {
            // `freeze` needs the storage handle where it lives, so that the
            // uniqueness check counts the caller's own handle only once.
            if let Some(place) = &place {
                return Ok(place.with_mut(span, |slot| match slot {
                    Value::Vector(storage) => builtins::freeze(storage, span),
                    other => Err(RuntimeError::new(format!(
                        "`{}` has no method `freeze`",
                        other.type_name()
                    ))
                    .at(span)),
                })??);
            }
        }

        let receiver_value = match (place, temporary) {
            (Some(place), _) => place.read(span)?,
            (_, Some(value)) => value,
            _ => unreachable!("a receiver is either a place or a temporary"),
        };
        Ok(builtins::call_method(
            self,
            &receiver_value,
            name,
            values,
            span,
        )?)
    }

    /// Struct initialization is a synthesized labeled call.
    fn init_struct(
        &mut self,
        module: &str,
        decl: &Arc<StructDecl>,
        args: Vec<EvaluatedArg>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let names: Vec<&str> = decl.fields.iter().map(|f| f.name.node.as_str()).collect();
        let (mut slots, _) = assign_labels(&names, args, &decl.name.node, false)?;
        let mut fields = Vec::with_capacity(decl.fields.len());
        for (index, field) in decl.fields.iter().enumerate() {
            let Some(arg) = slots[index].take() else {
                return Err(RuntimeError::new(format!(
                    "`{}` needs a value for field `{}`",
                    decl.name.node, field.name.node
                ))
                .at(span)
                .with_rule("Struct initialization is a synthesized labeled call.")
                .with_help(format!(
                    "add `{}: <value>` to the initializer",
                    field.name.node
                )));
            };
            let value = value_of(&arg, &field.name.node, arg.span)?;
            fields.push((
                field.name.node.as_str().into(),
                self.coerce(module, value, &field.ty),
            ));
        }
        Ok(Value::Struct(Box::new(StructValue {
            type_name: format!("{module}.{}", decl.name.node).into(),
            fields,
        })))
    }

    fn eval_args(
        &mut self,
        env: &mut Env,
        args: &[Arg],
        trailing: Option<&Expr>,
    ) -> Result<Vec<EvaluatedArg>, Control> {
        let mut evaluated = Vec::with_capacity(args.len() + usize::from(trailing.is_some()));
        for arg in args {
            let slot = if arg.is_var {
                let place = self.resolve_place(env, &arg.value)?;
                if !place.mutable {
                    return Err(var_arg_needs_mutable(&describe_place(&arg.value), arg.span).into());
                }
                ArgSlot::Alias(place)
            } else {
                ArgSlot::Value(self.eval(env, &arg.value)?)
            };
            evaluated.push(EvaluatedArg {
                label: arg.label.as_ref().map(|l| l.node.as_str().into()),
                spread: arg.spread,
                slot,
                span: arg.span,
            });
        }
        if let Some(trailing) = trailing {
            let value = self.eval_trailing(env, trailing)?;
            evaluated.push(EvaluatedArg {
                label: None,
                spread: false,
                slot: ArgSlot::Value(value),
                span: trailing.span,
            });
        }
        Ok(evaluated)
    }

    /// A trailing block is a closure argument: `mapError { ... }`.
    fn eval_trailing(&mut self, env: &mut Env, expr: &Expr) -> Eval {
        match &expr.kind {
            ExprKind::Block(block) => self
                .make_closure(env, false, Vec::new(), block.clone(), expr.span)
                .map_err(Control::from),
            _ => self.eval(env, expr),
        }
    }

    // --------------------------------------------------------------- places

    /// Resolves an lvalue, or reports why the expression is not a place.
    fn resolve_place(&mut self, env: &mut Env, expr: &Expr) -> Result<Place, Control> {
        match &expr.kind {
            ExprKind::Ident(name) => match env.lookup(name) {
                Some(place) => Ok(place.clone()),
                None => Err(
                    RuntimeError::new(format!("cannot find `{name}` in this scope"))
                        .at(expr.span)
                        .into(),
                ),
            },
            ExprKind::Field { base, name } => {
                let base_place = self.resolve_place(env, base)?;
                base_place.with_ref(expr.span, |value| match value {
                    Value::Struct(value) => match value.get(&name.node) {
                        Some(_) => Ok(()),
                        None => Err(no_field(&value.type_name, &name.node, expr.span)),
                    },
                    other => Err(not_a_struct(other, &name.node, expr.span)),
                })??;
                Ok(base_place.field(name.node.as_str().into()))
            }
            _ => Err(RuntimeError::new(
                "this expression is not a place, so it cannot be assigned or aliased",
            )
            .at(expr.span)
            .with_rule("Only variables and their struct fields are places.")
            .into()),
        }
    }

    /// Resolves an lvalue when the expression denotes one, without failing.
    fn resolve_place_opt(&mut self, env: &mut Env, expr: &Expr) -> Result<Option<Place>, Control> {
        match &expr.kind {
            ExprKind::Ident(name) => Ok(env.lookup(name).cloned()),
            ExprKind::Field { base, name } => {
                let Some(base_place) = self.resolve_place_opt(env, base)? else {
                    return Ok(None);
                };
                let is_field = base_place.with_ref(expr.span, |value| match value {
                    Value::Struct(value) => value.get(&name.node).is_some(),
                    _ => false,
                })?;
                Ok(is_field.then(|| base_place.field(name.node.as_str().into())))
            }
            _ => Ok(None),
        }
    }

    // ------------------------------------------------------------ patterns

    fn match_pattern(
        &mut self,
        env: &mut Env,
        pattern: &Pattern,
        value: &Value,
    ) -> Result<bool, Control> {
        match &pattern.kind {
            PatternKind::Wildcard => Ok(true),
            PatternKind::Binding(name) => {
                // `None` is a case, not a name to bind.
                if name == "None" {
                    if let Value::Enum(option) = value {
                        if &*option.type_name == "Option" {
                            return Ok(&*option.case == "None");
                        }
                    }
                }
                env.declare(name.as_str().into(), Place::binding(value.clone(), false));
                Ok(true)
            }
            PatternKind::Literal(expr) => {
                let literal = self.eval(env, expr)?;
                Ok(value.eq_value(&literal))
            }
            PatternKind::Variant { path, payload } => {
                let Value::Enum(subject) = value else {
                    return Ok(false);
                };
                let Some(case) = path.last() else {
                    return Ok(false);
                };
                if &*subject.case != case.node.as_str() {
                    return Ok(false);
                }
                if path.len() >= 2 {
                    let expected = &path[path.len() - 2].node;
                    let actual = subject
                        .type_name
                        .rsplit('.')
                        .next()
                        .unwrap_or(&subject.type_name);
                    if actual != expected {
                        return Ok(false);
                    }
                }
                if payload.len() != subject.payload.len() {
                    return Err(RuntimeError::new(format!(
                        "case `{}` carries {} value(s), but the pattern binds {}",
                        case.node,
                        subject.payload.len(),
                        payload.len()
                    ))
                    .at(pattern.span)
                    .into());
                }
                for (sub, value) in payload.iter().zip(subject.payload.iter()) {
                    if !self.match_pattern(env, sub, value)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
        }
    }

    /// An independent, mutable copy of `value`'s own graph, per the builtin
    /// `Snapshot` trait.
    ///
    /// Immutable values return themselves, since sharing their storage is
    /// unobservable. `Vector` is the one MVP type with an independent
    /// mutable graph to copy: it allocates fresh storage and snapshots each
    /// element recursively. A `dyn Trait` value snapshots the concrete value
    /// it carries, keeping the same trait. A struct or enum dispatches
    /// through its own `impl Snapshot for Type`, exactly like any other
    /// method call. Closures, tasks, task scopes, and host handles have no
    /// independent graph to copy and do not conform by default.
    ///
    /// The MVP's value model has no way to construct a cycle — every
    /// container (`Struct`, `Enum`, `Array`, `Vector`) owns `Value`s by
    /// `Rc`, and a value is built bottom-up from values that already exist,
    /// so nothing can point back to a container still being built. Snapshots
    /// therefore only need this straightforward structural copy; "preserves
    /// cycles" is not yet a case the MVP can exercise.
    fn snapshot(&mut self, value: &Value, span: Span) -> Result<Value, RuntimeError> {
        match value {
            Value::Unit
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Duration(_)
            | Value::Str(_)
            | Value::Array(_)
            | Value::Map(_)
            | Value::Set(_)
            | Value::Range { .. } => Ok(value.clone()),
            Value::Vector(storage) => {
                builtins::check_live(storage, "snapshot", span)?;
                let elements = storage.elements.borrow().clone();
                let mut snapshotted = Vec::with_capacity(elements.len());
                for item in &elements {
                    snapshotted.push(self.snapshot(item, span)?);
                }
                Ok(Value::Vector(VectorStorage::new(snapshotted)))
            }
            Value::Dyn(wrapped) => Ok(Value::Dyn(Rc::new(DynValue {
                trait_name: wrapped.trait_name.clone(),
                value: self.snapshot(&wrapped.value, span)?,
            }))),
            Value::Struct(s) => self.dispatch_snapshot(&s.type_name, value.clone(), span),
            Value::Enum(e) => self.dispatch_snapshot(&e.type_name, value.clone(), span),
            other => Err(no_snapshot_conformance(other, span)),
        }
    }

    /// Calls `type_name`'s own `snapshot` method, which exists exactly when
    /// some module wrote `impl Snapshot for Type`.
    fn dispatch_snapshot(
        &mut self,
        type_name: &str,
        receiver: Value,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let Some((type_module, short)) = type_name.rsplit_once('.') else {
            return Err(no_snapshot_conformance(&receiver, span));
        };
        let Some((module, decl)) = self.find_method(type_module, short, "snapshot") else {
            return Err(no_snapshot_conformance(&receiver, span));
        };
        self.invoke(
            &Target {
                name: "snapshot",
                params: &decl.params,
                body: &decl.body,
                module,
                receiver: decl.receiver,
                is_async: decl.is_async,
                captures: &[],
                return_type: decl.return_type.as_ref(),
            },
            Some(ArgSlot::Value(receiver)),
            Vec::new(),
            span,
        )
    }
}

impl Callable for Interpreter<'_> {
    fn call_value(
        &mut self,
        callee: &Value,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let args = args
            .into_iter()
            .map(|value| EvaluatedArg {
                label: None,
                spread: false,
                slot: ArgSlot::Value(value),
                span,
            })
            .collect();
        self.call_value_slots(callee.clone(), args, span)
    }

    fn arity(&self, callee: &Value) -> Option<usize> {
        match callee {
            Value::Closure(closure) => Some(closure.params.len()),
            _ => None,
        }
    }
}

// -------------------------------------------------------------- operators

/// Integer arithmetic traps instead of wrapping.
///
/// Overflow of `+`, `-`, `*`, and unary `-`, and division or remainder by
/// zero, are broken invariants: they raise a [`RuntimeError`] naming the
/// operation rather than producing a defined-but-wrong value. There are no
/// implicit numeric, string, or boolean conversions, so mixed operands are
/// rejected too.
fn binary(op: BinaryOp, lhs: Value, rhs: Value, span: Span) -> Result<Value, RuntimeError> {
    match op {
        BinaryOp::Eq | BinaryOp::Ne => {
            if lhs.type_name() != rhs.type_name() {
                return Err(RuntimeError::new(format!(
                    "cannot compare `{}` with `{}`",
                    lhs.type_name(),
                    rhs.type_name()
                ))
                .at(span)
                .with_rule("`==` means value equality between values of the same type."));
            }
            let equal = lhs.eq_value(&rhs);
            Ok(Value::Bool(if op == BinaryOp::Eq { equal } else { !equal }))
        }
        // `is` is narrower than `==`: same shared storage, not same value.
        // `Vector` is the one MVP type with storage of its own; everything
        // else has no identity `is` can answer, which is a distinct error
        // from a type mismatch.
        BinaryOp::Is => {
            if lhs.type_name() != rhs.type_name() {
                return Err(RuntimeError::new(format!(
                    "cannot compare the identity of `{}` with `{}`",
                    lhs.type_name(),
                    rhs.type_name()
                ))
                .at(span)
                .with_rule("`is` compares identity between values of the same type."));
            }
            match (&lhs, &rhs) {
                (Value::Vector(a), Value::Vector(b)) => Ok(Value::Bool(Rc::ptr_eq(a, b))),
                _ => Err(identity_not_available(&lhs, span)),
            }
        }
        BinaryOp::And | BinaryOp::Or => unreachable!("short-circuited in `eval`"),
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => {
            match (&lhs, &rhs) {
                (Value::Int(a), Value::Int(b)) => {
                    let (a, b) = (*a, *b);
                    let value = match op {
                        BinaryOp::Add => a.checked_add(b).ok_or_else(|| overflow("addition", span)),
                        BinaryOp::Sub => a
                            .checked_sub(b)
                            .ok_or_else(|| overflow("subtraction", span)),
                        BinaryOp::Mul => a
                            .checked_mul(b)
                            .ok_or_else(|| overflow("multiplication", span)),
                        BinaryOp::Div => {
                            if b == 0 {
                                Err(divide_by_zero("division", span))
                            } else {
                                a.checked_div(b).ok_or_else(|| overflow("division", span))
                            }
                        }
                        BinaryOp::Rem => {
                            if b == 0 {
                                Err(divide_by_zero("remainder", span))
                            } else {
                                a.checked_rem(b).ok_or_else(|| overflow("remainder", span))
                            }
                        }
                        _ => unreachable!("checked above"),
                    }?;
                    Ok(Value::Int(value))
                }
                (Value::Float(a), Value::Float(b)) => Ok(Value::Float(match op {
                    BinaryOp::Add => a + b,
                    BinaryOp::Sub => a - b,
                    BinaryOp::Mul => a * b,
                    BinaryOp::Div => a / b,
                    BinaryOp::Rem => a % b,
                    _ => unreachable!("checked above"),
                })),
                (Value::Duration(a), Value::Duration(b))
                    if matches!(op, BinaryOp::Add | BinaryOp::Sub) =>
                {
                    let value = match op {
                        BinaryOp::Add => a.checked_add(*b),
                        _ => a.checked_sub(*b),
                    }
                    .ok_or_else(|| overflow("duration arithmetic", span))?;
                    Ok(Value::Duration(value))
                }
                (Value::Str(_), Value::Str(_)) if op == BinaryOp::Add => {
                    Err(RuntimeError::new("`+` is not defined for `String`")
                        .at(span)
                        .with_rule("There are no implicit string conversions.")
                        .with_help("use string interpolation, such as \"{left}{right}\""))
                }
                _ => Err(operator_type_error(op, &lhs, &rhs, span)),
            }
        }
        BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
            let ordering = match (&lhs, &rhs) {
                (Value::Int(a), Value::Int(b)) => a.partial_cmp(b),
                (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
                (Value::Duration(a), Value::Duration(b)) => a.partial_cmp(b),
                (Value::Str(a), Value::Str(b)) => a.partial_cmp(b),
                _ => return Err(operator_type_error(op, &lhs, &rhs, span)),
            };
            let Some(ordering) = ordering else {
                return Ok(Value::Bool(false));
            };
            Ok(Value::Bool(match op {
                BinaryOp::Lt => ordering.is_lt(),
                BinaryOp::Le => ordering.is_le(),
                BinaryOp::Gt => ordering.is_gt(),
                _ => ordering.is_ge(),
            }))
        }
    }
}

fn unary(op: UnaryOp, value: Value, span: Span) -> Result<Value, RuntimeError> {
    match (op, value) {
        (UnaryOp::Not, Value::Bool(value)) => Ok(Value::Bool(!value)),
        (UnaryOp::Neg, Value::Int(value)) => Ok(Value::Int(
            value
                .checked_neg()
                .ok_or_else(|| overflow("negation", span))?,
        )),
        (UnaryOp::Neg, Value::Float(value)) => Ok(Value::Float(-value)),
        (UnaryOp::Neg, Value::Duration(value)) => Ok(Value::Duration(
            value
                .checked_neg()
                .ok_or_else(|| overflow("negation", span))?,
        )),
        (op, value) => Err(RuntimeError::new(format!(
            "`{}` is not defined for `{}`",
            match op {
                UnaryOp::Not => "!",
                UnaryOp::Neg => "-",
            },
            value.type_name()
        ))
        .at(span)
        .with_rule("There are no implicit numeric, string, or boolean conversions.")),
    }
}

// -------------------------------------------------------------- arguments

/// The labels `MapEntry(key:, value:)` accepts, in declaration order.
const MAP_ENTRY_FIELDS: [&str; 2] = ["key", "value"];

/// `MapEntry(key: ..., value: ...)` is a synthesized labeled call for a
/// builtin struct, exactly like a user struct's synthesized initializer. It
/// exists only so `Map.of` has an ordinary call-shaped way to build the pairs
/// it collects; it is not a declared struct because nothing else derives it.
fn init_map_entry(args: Vec<EvaluatedArg>, span: Span) -> Result<Value, RuntimeError> {
    let (mut slots, _) = assign_labels(&MAP_ENTRY_FIELDS, args, "MapEntry", false)?;
    let mut fields = Vec::with_capacity(MAP_ENTRY_FIELDS.len());
    for (index, field_name) in MAP_ENTRY_FIELDS.iter().enumerate() {
        let Some(arg) = slots[index].take() else {
            return Err(RuntimeError::new(format!(
                "`MapEntry` needs a value for field `{field_name}`"
            ))
            .at(span)
            .with_rule("Struct initialization is a synthesized labeled call.")
            .with_help(format!("add `{field_name}: <value>` to the initializer")));
        };
        fields.push(((*field_name).into(), value_of(&arg, field_name, arg.span)?));
    }
    Ok(Value::Struct(Box::new(StructValue {
        type_name: "MapEntry".into(),
        fields,
    })))
}

/// Matches call-site arguments to declared names.
///
/// Positional arguments may precede labels and are matched to names in
/// declaration order; after the first label every argument must be labeled.
#[allow(clippy::type_complexity)]
fn assign_labels(
    names: &[&str],
    args: Vec<EvaluatedArg>,
    what: &str,
    variadic_last: bool,
) -> Result<(Vec<Option<EvaluatedArg>>, Vec<EvaluatedArg>), RuntimeError> {
    let mut slots: Vec<Option<EvaluatedArg>> = (0..names.len()).map(|_| None).collect();
    let mut rest = Vec::new();
    let mut next = 0usize;
    let mut labeled = false;

    for arg in args {
        match &arg.label {
            Some(label) => {
                labeled = true;
                let Some(index) = names.iter().position(|n| *n == &**label) else {
                    return Err(RuntimeError::new(format!(
                        "`{what}` has no parameter labeled `{label}`"
                    ))
                    .at(arg.span)
                    .with_rule("Argument labels are parameter names and part of the API contract.")
                    .with_help(format!("known labels: {}", names.join(", "))));
                };
                if slots[index].is_some() {
                    return Err(RuntimeError::new(format!(
                        "`{what}` was given `{label}` more than once"
                    ))
                    .at(arg.span));
                }
                // Labels are static parameter names, so left-to-right
                // evaluation of the call must match the declaration order.
                if index < next {
                    return Err(RuntimeError::new(format!(
                        "`{what}` was given the label `{label}` out of declaration order"
                    ))
                    .at(arg.span)
                    .with_rule(
                        "Labeled arguments appear in declaration order, so argument order matches parameter order.",
                    )
                    .with_help(format!(
                        "write the arguments in this order: {}",
                        names.join(", ")
                    )));
                }
                slots[index] = Some(arg);
                next = index + 1;
            }
            None => {
                if labeled {
                    return Err(RuntimeError::new(format!(
                        "`{what}` was given a positional argument after a labeled one"
                    ))
                    .at(arg.span)
                    .with_rule(
                        "Positional arguments may precede labels; after the first label every argument must be labeled.",
                    ));
                }
                if variadic_last && next + 1 >= names.len() {
                    rest.push(arg);
                } else if next < names.len() {
                    slots[next] = Some(arg);
                    next += 1;
                } else {
                    return Err(RuntimeError::new(format!(
                        "`{what}` takes {} argument(s), but more were given",
                        names.len()
                    ))
                    .at(arg.span));
                }
            }
        }
    }
    Ok((slots, rest))
}

/// Rejects `var` and `...` where only a plain value is meaningful.
fn plain_values(args: Vec<EvaluatedArg>, what: &str) -> Result<Vec<Value>, RuntimeError> {
    let mut values = Vec::with_capacity(args.len());
    for arg in &args {
        values.push(value_of(arg, what, arg.span)?);
    }
    Ok(values)
}

fn value_of(arg: &EvaluatedArg, what: &str, span: Span) -> Result<Value, RuntimeError> {
    match &arg.slot {
        ArgSlot::Value(value) => Ok(value.clone()),
        ArgSlot::Alias(_) => Err(RuntimeError::new(format!(
            "`{what}` does not take a `var` argument"
        ))
        .at(span)
        .with_rule(
            "A `var` parameter is a non-escaping inout alias, marked at both the declaration and the call site.",
        )),
    }
}

// ------------------------------------------------------------- free names

/// Every name a block can read from the environment around it.
///
/// The set over-approximates: a name the body binds for itself is listed too.
/// Over-approximating is safe, because a closure that captures a name it never
/// reads is only holding one value more than it needs, while missing one would
/// leave the body unable to resolve it.
fn mention_block(block: &Block, out: &mut BTreeSet<String>) {
    for stmt in &block.statements {
        match &stmt.kind {
            StmtKind::Let { value, .. } => mention_expr(value, out),
            StmtKind::Expr(expr) => mention_expr(expr, out),
            StmtKind::Item(item) => match &item.kind {
                ItemKind::Fn(decl) => mention_fn(decl, out),
                ItemKind::Impl(block) => {
                    for item in &block.items {
                        if let ItemKind::Fn(decl) = &item.kind {
                            mention_fn(decl, out);
                        }
                    }
                }
                // A trait's default bodies are reached through the
                // conformances resolution recorded them under, not through
                // this closure's environment.
                ItemKind::Struct(_)
                | ItemKind::Enum(_)
                | ItemKind::Trait(_)
                | ItemKind::TypeAlias(_) => {}
            },
        }
    }
    if let Some(tail) = &block.tail {
        mention_expr(tail, out);
    }
}

fn mention_fn(decl: &FnDecl, out: &mut BTreeSet<String>) {
    mention_params(&decl.params, out);
    mention_block(&decl.body, out);
}

/// A default argument is evaluated by the callee, so the names it reads belong
/// to the body.
fn mention_params(params: &[Param], out: &mut BTreeSet<String>) {
    for param in params {
        if let Some(default) = &param.default {
            mention_expr(default, out);
        }
    }
}

fn mention_expr(expr: &Expr, out: &mut BTreeSet<String>) {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Bool(_)
        | ExprKind::Duration(_)
        | ExprKind::Unit => {}
        ExprKind::Str(parts) => {
            for part in parts {
                if let StrPart::Interpolation(inner) = part {
                    mention_expr(inner, out);
                }
            }
        }
        ExprKind::Ident(name) => {
            out.insert(name.clone());
        }
        ExprKind::ArrayLit(items) => {
            for item in items {
                mention_expr(item, out);
            }
        }
        // A field name is not a binding; only the base can read one.
        ExprKind::Field { base, .. } => mention_expr(base, out),
        ExprKind::Call {
            callee,
            args,
            trailing,
            ..
        } => {
            mention_expr(callee, out);
            for arg in args {
                mention_expr(&arg.value, out);
            }
            if let Some(trailing) = trailing {
                mention_expr(trailing, out);
            }
        }
        ExprKind::Unary { operand, .. } => mention_expr(operand, out),
        ExprKind::Binary { lhs, rhs, .. } => {
            mention_expr(lhs, out);
            mention_expr(rhs, out);
        }
        ExprKind::Assign { target, value, .. } => {
            mention_expr(target, out);
            mention_expr(value, out);
        }
        ExprKind::Try(inner) | ExprKind::Await(inner) => mention_expr(inner, out),
        ExprKind::Block(block) => mention_block(block, out),
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            mention_expr(condition, out);
            mention_block(then_branch, out);
            if let Some(branch) = else_branch {
                mention_expr(branch, out);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            mention_expr(scrutinee, out);
            for arm in arms {
                mention_pattern(&arm.pattern, out);
                mention_expr(&arm.body, out);
            }
        }
        ExprKind::For { iterable, body, .. } => {
            mention_expr(iterable, out);
            mention_block(body, out);
        }
        ExprKind::While { condition, body } => {
            mention_expr(condition, out);
            mention_block(body, out);
        }
        ExprKind::Return(inner) | ExprKind::Break(inner) => {
            if let Some(inner) = inner {
                mention_expr(inner, out);
            }
        }
        ExprKind::Continue => {}
        ExprKind::Lambda { params, body, .. } => {
            mention_params(params, out);
            mention_block(body, out);
        }
        // The scope name is bound by the `scope`, so it shadows anything the
        // surrounding environment holds under that name.
        ExprKind::Scope { body, .. } => mention_block(body, out),
        ExprKind::Range { start, end, .. } => {
            mention_expr(start, out);
            mention_expr(end, out);
        }
    }
}

/// Pattern bindings are binders, so only a literal pattern reads a name.
fn mention_pattern(pattern: &Pattern, out: &mut BTreeSet<String>) {
    match &pattern.kind {
        PatternKind::Wildcard | PatternKind::Binding(_) => {}
        PatternKind::Literal(expr) => mention_expr(expr, out),
        PatternKind::Variant { payload, .. } => {
            for sub in payload {
                mention_pattern(sub, out);
            }
        }
    }
}

// ------------------------------------------------------------------ tasks

/// A stable key for a task's trace id, valid for as long as some `Rc<Task>`
/// keeps the task alive — which every task the interpreter still holds a
/// handle to does.
/// Runs one spawned task's body on its own thread.
///
/// The body arrives as a [`Transfer`] and the value leaves as one: both
/// directions are a task boundary, so both are the copy the task-safety rule
/// demands. A task that produces a value no boundary may carry is reported
/// here rather than handing the value to a thread that cannot own it.
fn run_task(
    runtime: &Runtime,
    id: u64,
    cancellation: Cancellation,
    body: Transfer,
    span: Span,
) -> TaskOutcome {
    let mut interpreter = Interpreter::for_task(runtime, id, cancellation.clone());
    interpreter.timings.push(Timing::start());
    let result = interpreter.call_value_slots(body.into_value(), Vec::new(), span);
    let timing = interpreter
        .timings
        .pop()
        .expect("a task pushes exactly the one timing it pops");
    // A task stopped by its own cancellation did not run to completion, so it
    // is traced as cancelled — by whoever waits for it, which is the only
    // place that knows it stopped rather than finished — and not here.
    if !(result.is_err() && cancellation.is_cancelled()) {
        runtime.trace(TraceEvent::TaskCompleted {
            id,
            cpu: timing.cpu(),
        });
    }
    let value = result?;
    Transfer::of(&value).map_err(|found| {
        RuntimeError::new(format!(
            "this task produced {}, which cannot leave a task",
            found.subject()
        ))
        .at(span)
        .with_rule(crate::task::TASK_SAFETY_RULE)
        .with_help(found.help("returning it from a task"))
    })
}

/// A task that stopped because its own cancellation was requested.
fn task_cancelled(span: Span) -> RuntimeError {
    RuntimeError::new("this task was cancelled")
        .at(span)
        .with_rule("Leaving a task scope waits for or cancels its child tasks.")
}

/// The error a `Result` carries, when the value is one and it failed.
fn failure_of(value: &Value) -> Option<Value> {
    match value {
        Value::Enum(result) if &*result.type_name == "Result" && &*result.case == "Err" => {
            Some(result.payload.first().cloned().unwrap_or(Value::Unit))
        }
        _ => None,
    }
}

fn expect_no_arguments(what: &str, values: &[Value], span: Span) -> Result<(), RuntimeError> {
    if values.is_empty() {
        return Ok(());
    }
    Err(RuntimeError::new(format!(
        "`{what}` takes no arguments, but {} were given",
        values.len()
    ))
    .at(span))
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

// ------------------------------------------------------------ diagnostics

fn unsupported(what: &str, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "{what} is not implemented yet in the MVP interpreter"
    ))
    .at(span)
    .with_rule("The MVP interpreter runs the subset of Cove that the MVP defines.")
}

fn overflow(operation: &str, span: Span) -> RuntimeError {
    RuntimeError::new(format!("`Int` {operation} overflowed"))
        .at(span)
        .with_rule("Integer overflow is a broken invariant, not a wrapped result.")
}

fn divide_by_zero(operation: &str, span: Span) -> RuntimeError {
    RuntimeError::new(format!("`Int` {operation} by zero"))
        .at(span)
        .with_rule("Division and remainder by zero are broken invariants.")
}

fn operator_type_error(op: BinaryOp, lhs: &Value, rhs: &Value, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "`{}` is not defined for `{}` and `{}`",
        operator_text(op),
        lhs.type_name(),
        rhs.type_name()
    ))
    .at(span)
    .with_rule("There are no implicit numeric, string, or boolean conversions.")
}

fn operator_text(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Rem => "%",
        BinaryOp::Eq => "==",
        BinaryOp::Ne => "!=",
        BinaryOp::Lt => "<",
        BinaryOp::Le => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::Ge => ">=",
        BinaryOp::Is => "is",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
    }
}

/// `a is b` where `a` and `b` share a type that has no shared-storage
/// identity to compare.
fn identity_not_available(value: &Value, span: Span) -> RuntimeError {
    RuntimeError::new(format!("identity is not available for `{}`", value.type_name()))
        .at(span)
        .with_rule("`==` means value equality. Identity, when available, is explicit.")
        .with_help(
            "`is` is defined for `Vector`; compare other values with `==`, or call `toArray()` for an independent copy",
        )
}

/// `value.snapshot()` where `value` is a closure, a task, a task scope, a
/// host handle, or a struct or enum with no `impl Snapshot for Type`.
fn no_snapshot_conformance(value: &Value, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "`{}` does not implement `Snapshot`",
        value.type_name()
    ))
    .at(span)
    .with_rule(
        "Closures, synchronized values, and Host resources do not implement `Snapshot` by default; a struct or enum conforms explicitly with `impl Snapshot for Type`.",
    )
}

fn no_field(type_name: &str, field: &str, span: Span) -> RuntimeError {
    RuntimeError::new(format!("`{type_name}` has no field `{field}`")).at(span)
}

fn not_a_struct(value: &Value, field: &str, span: Span) -> RuntimeError {
    RuntimeError::new(format!("`{}` has no field `{field}`", value.type_name()))
        .at(span)
        .with_rule("Only struct fields are places.")
}

fn var_self_needs_place(method: &str, receiver: &Expr, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "`{method}` takes a `var self` receiver, but `{}` is not a place",
        describe_place(receiver)
    ))
    .at(span)
    .with_rule("A mutating receiver declares `var self` and mutates the caller's place.")
    .with_help("bind the value with `var` first, then call the method on that binding")
}

fn var_self_needs_mutable(method: &str, receiver: &Expr, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "`{method}` takes a `var self` receiver, but `{}` is a read-only place",
        describe_place(receiver)
    ))
    .at(span)
    .with_rule("`let` creates a read-only place; `var` creates a mutable place.")
    .with_help(format!(
        "declare it with `var {}`",
        describe_place(receiver)
    ))
}

fn var_arg_needs_mutable(name: &str, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "`{name}` is a read-only place, so it cannot be passed as `var`"
    ))
    .at(span)
    .with_rule("`let` creates a read-only place; `var` creates a mutable place.")
}

fn expect_bool(value: Value, op: BinaryOp, span: Span) -> Result<bool, RuntimeError> {
    match value {
        Value::Bool(value) => Ok(value),
        other => Err(RuntimeError::new(format!(
            "`{}` needs `Bool` operands, but found `{}`",
            operator_text(op),
            other.type_name()
        ))
        .at(span)
        .with_rule("There are no implicit boolean conversions.")),
    }
}

fn expect_int(value: Value, what: &str, span: Span) -> Result<i64, RuntimeError> {
    match value {
        Value::Int(value) => Ok(value),
        other => Err(RuntimeError::new(format!(
            "{what} must be an `Int`, but found `{}`",
            other.type_name()
        ))
        .at(span)),
    }
}

/// How an lvalue is written in source, for diagnostics.
fn describe_place(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Ident(name) => name.clone(),
        ExprKind::Field { base, name } => format!("{}.{}", describe_place(base), name.node),
        _ => "this expression".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use std::sync::Mutex;
    use std::time::Duration;

    use cove_sema::config::Config;
    use cove_sema::package::{Module, Package, Unit};

    use crate::budget::{Budget, Limits};
    use crate::host::{Console, Documents, Env as EnvHost, Grants, HostRegistry};

    /// A `console` sink the tests can read back.
    ///
    /// Synchronized because a host is reachable from every task of a run, and
    /// a test that spawns tasks prints from more than one thread.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Buffer {
        fn written(&self) -> std::sync::MutexGuard<'_, Vec<u8>> {
            self.0.lock().expect("no test panics while printing")
        }

        fn text(&self) -> String {
            String::from_utf8(self.written().clone()).expect("console output is UTF-8")
        }
    }

    /// Parses `source` as the single unit of module `test`.
    fn program_of(source: &str) -> (Arc<SourceMap>, Arc<Program>) {
        let mut sources = SourceMap::new();
        let path = PathBuf::from("test/main.cove");
        let file = sources.add(path.clone(), source);
        let ast = cove_syntax::parse_file(&sources, file).expect("test source parses");
        let mut modules = BTreeMap::new();
        modules.insert(
            "test".to_string(),
            Module {
                name: "test".to_string(),
                dir: PathBuf::from("test"),
                units: vec![Unit { file, path, ast }],
            },
        );
        let package = Package {
            root: PathBuf::new(),
            config: Config::default(),
            modules,
        };
        let program = cove_sema::resolve::resolve(&package).expect("test source resolves");
        (Arc::new(sources), Arc::new(program))
    }

    /// Parses several modules, so one can `use` another.
    fn program_of_modules(modules: &[(&str, &str)]) -> (Arc<SourceMap>, Arc<Program>) {
        let mut sources = SourceMap::new();
        let mut map = BTreeMap::new();
        for (name, source) in modules {
            let path = PathBuf::from(format!("{name}/main.cove"));
            let file = sources.add(path.clone(), *source);
            let ast = cove_syntax::parse_file(&sources, file).expect("test source parses");
            map.insert(
                (*name).to_string(),
                Module {
                    name: (*name).to_string(),
                    dir: PathBuf::from(*name),
                    units: vec![Unit { file, path, ast }],
                },
            );
        }
        let package = Package {
            root: PathBuf::new(),
            config: Config::default(),
            modules: map,
        };
        let program = cove_sema::resolve::resolve(&package).expect("test package resolves");
        (Arc::new(sources), Arc::new(program))
    }

    /// Runs `app.main` of a package written inline, with `console` granted.
    fn run_modules(modules: &[(&str, &str)]) -> Run {
        let (sources, program) = program_of_modules(modules);
        run_in(
            &program,
            &sources,
            "app",
            "main",
            &[],
            &["console"],
            BTreeMap::new(),
        )
    }

    struct Run {
        value: Result<Value, RuntimeError>,
        output: String,
    }

    impl Run {
        fn value(self) -> Value {
            self.value.expect("the program ran without a runtime error")
        }

        fn error(self) -> RuntimeError {
            match self.value {
                Ok(value) => panic!("expected a runtime error, but the program returned {value}"),
                Err(error) => error,
            }
        }
    }

    fn run_in(
        program: &Arc<Program>,
        sources: &Arc<SourceMap>,
        module: &str,
        entry: &str,
        args: &[&str],
        grants: &[&str],
        env: BTreeMap<String, String>,
    ) -> Run {
        let buffer = Buffer::default();
        let mut hosts = HostRegistry::new(Grants::new(grants.to_vec()));
        hosts.register(Box::new(Console::new(buffer.clone())));
        hosts.register(Box::new(EnvHost::new(env)));
        let runtime = Runtime::new(program.clone(), sources.clone(), Arc::new(hosts));
        let value = Interpreter::new(&runtime).run_entry(
            module,
            entry,
            args.iter().map(|a| (*a).into()).collect(),
        );
        Run {
            value,
            output: buffer.text(),
        }
    }

    /// Runs `test.main` with `console` and `env` granted.
    fn run_entry_of(source: &str, entry: &str, args: &[&str]) -> Run {
        let (sources, program) = program_of(source);
        run_in(
            &program,
            &sources,
            "test",
            entry,
            args,
            &["console", "env"],
            BTreeMap::new(),
        )
    }

    /// Runs `body` inside a `main` that returns `Result<Unit, Error>`.
    fn run_body(body: &str) -> Run {
        let source = format!(
            "use console.println\n\nexport fn main() -> Result<Unit, Error> {{\n{body}\n  Ok(())\n}}\n"
        );
        run_entry_of(&source, "main", &[])
    }

    fn output_of(body: &str) -> String {
        run_body(body).output
    }

    fn error_of(body: &str) -> RuntimeError {
        run_body(body).error()
    }

    // ---------------------------------------------------- assertions

    /// Runs `test.check`, a test-shaped function holding `body`, and returns
    /// the `Result` it produced.
    fn run_assertion(body: &str) -> Run {
        let source = format!("test fn check() -> Result<Unit, Error> {{\n{body}\n}}\n");
        let (sources, program) = program_of(&source);
        run_in(
            &program,
            &sources,
            "test",
            "check",
            &[],
            &[],
            BTreeMap::new(),
        )
    }

    /// The message a failed assertion reported, or `None` when it held.
    fn assertion_message(body: &str) -> Option<String> {
        match run_assertion(body).value() {
            Value::Enum(result) if &*result.case == "Err" => Some(result.payload[0].to_string()),
            _ => None,
        }
    }

    #[test]
    fn a_holding_assertion_produces_ok() {
        assert!(matches!(
            run_assertion("  assert(1 + 1 == 2)").value(),
            Value::Enum(result) if &*result.case == "Ok"
        ));
    }

    #[test]
    fn a_failing_assertion_names_the_conditions_source_text() {
        assert_eq!(
            assertion_message("  assert(1 + 1 == 3)").as_deref(),
            Some("assertion failed: `1 + 1 == 3`")
        );
    }

    #[test]
    fn a_failing_assertion_is_an_err_rather_than_a_panic() {
        // `?` propagates it, so the test's own `Err` is the assertion's.
        assert_eq!(
            assertion_message("  assert(false)?\n  Ok(())").as_deref(),
            Some("assertion failed: `false`")
        );
    }

    #[test]
    fn assert_equal_reports_both_values_and_the_actual_expressions_source() {
        assert_eq!(assertion_message("  assertEqual(2 + 2, 4)"), None);
        assert_eq!(
            assertion_message("  assertEqual(2 + 2, 5)").as_deref(),
            Some("assertion failed: `2 + 2` is `4`, expected `5`")
        );
    }

    #[test]
    fn a_failed_assertion_records_where_it_was_written() {
        let source = "test fn check() -> Result<Unit, Error> {\n  assert(1 == 2)\n}\n";
        let (sources, program) = program_of(source);
        let hosts = HostRegistry::new(Grants::default());
        let runtime = Runtime::new(program, sources.clone(), Arc::new(hosts));
        let mut interpreter = Interpreter::new(&runtime);
        interpreter
            .run_entry("test", "check", Vec::new())
            .expect("the assertion fails as an `Err`, not a runtime error");
        let (span, message) = interpreter
            .assertion_failure()
            .expect("the failure was recorded");
        assert_eq!(message, "assertion failed: `1 == 2`");
        assert_eq!(sources.get(span.file).line_col(span.start).0, 2);
    }

    #[test]
    fn a_holding_assertion_records_nothing() {
        let source = "test fn check() -> Result<Unit, Error> {\n  assert(1 == 1)\n}\n";
        let (sources, program) = program_of(source);
        let hosts = HostRegistry::new(Grants::default());
        let runtime = Runtime::new(program, sources, Arc::new(hosts));
        let mut interpreter = Interpreter::new(&runtime);
        interpreter.run_entry("test", "check", Vec::new()).unwrap();
        assert!(interpreter.assertion_failure().is_none());
    }

    #[test]
    fn assert_equal_refuses_the_comparison_that_equality_refuses() {
        let error = run_assertion("  assertEqual(1, \"1\")").error();
        assert!(
            error.message.contains("cannot compare `Int` with `String`"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_module_declaration_wins_over_the_assertion_builtin() {
        let source = "fn assert(value: Int) -> Int {\n  value\n}\n\n                      export fn main() -> Int {\n  assert(7)\n}\n";
        let (sources, program) = program_of(source);
        let run = run_in(
            &program,
            &sources,
            "test",
            "main",
            &[],
            &[],
            BTreeMap::new(),
        );
        assert!(matches!(run.value(), Value::Int(7)));
    }

    // -------------------------------------------------------- traits

    /// A trait with one required and one defaulted method, two conforming
    /// types (one of which overrides the default), and a function for each
    /// dispatch form.
    const TRAITS: &str = r##"
use console.println

trait Display {
  fn describe(self) -> String

  fn label(self) -> String { "<{self.describe()}>" }
}

struct Booking(id: Int)

struct Receipt(total: Int)

impl Display for Booking {
  fn describe(self) -> String { "booking {self.id}" }
  fn label(self) -> String { "#{self.id}" }
}

impl Display for Receipt {
  fn describe(self) -> String { "receipt for {self.total}" }
}

fn render<T: Display>(value: T) -> String {
  "{value.label()} / {value.describe()}"
}

fn renderAll(values: Array<dyn Display>) -> String {
  var out = Vector.of("")
  for value in values {
    out.push(value.label())
  }
  "{out.toArray()}"
}
"##;

    fn run_with_traits(body: &str) -> Run {
        let source =
            format!("{TRAITS}\nexport fn main() -> Result<Unit, Error> {{\n{body}\n  Ok(())\n}}\n");
        run_entry_of(&source, "main", &[])
    }

    #[test]
    fn a_default_body_runs_unless_the_conformance_overrides_it() {
        let output = run_with_traits(
            "  console.println(render(Booking(id: 7)))?\n  console.println(render(Receipt(total: 12)))?",
        )
        .output;
        assert_eq!(
            output,
            "#7 / booking 7\n<receipt for 12> / receipt for 12\n"
        );
    }

    #[test]
    fn dynamic_dispatch_finds_the_implementation_from_the_value() {
        // One call site, two concrete types, two different implementations —
        // including one that runs the trait's default body.
        let output = run_with_traits(
            "  let mixed: Array<dyn Display> = [Booking(id: 1), Receipt(total: 2)]\n  console.println(renderAll(mixed))?",
        )
        .output;
        assert_eq!(output, "[, #1, <receipt for 2>]\n");
    }

    #[test]
    fn a_dyn_value_carries_its_concrete_value_and_its_trait() {
        let (sources, program) = program_of(&format!(
            "{TRAITS}\nexport fn main() -> dyn Display {{\n  Booking(id: 3)\n}}\n"
        ));
        let value = run_in(
            &program,
            &sources,
            "test",
            "main",
            &[],
            &["console"],
            BTreeMap::new(),
        )
        .value();
        let Value::Dyn(trait_object) = &value else {
            panic!("expected a trait object, found {value:?}");
        };
        assert_eq!(&*trait_object.trait_name, "test.Display");
        assert_eq!(trait_object.value.type_name(), "test.Booking");
        assert_eq!(value.type_name(), "dyn test.Display");
        // A trait object shows the value it holds: the wrapper is a
        // representation, not something the program put there.
        assert_eq!(value.to_string(), "Booking(id: 3)");
    }

    #[test]
    fn static_and_dynamic_dispatch_reach_the_same_implementation() {
        let output = run_with_traits(
            "  let one: dyn Display = Booking(id: 5)\n  console.println(render(Booking(id: 5)))?\n  console.println(\"{one.label()} / {one.describe()}\")?",
        )
        .output;
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines[0], lines[1]);
    }

    #[test]
    fn a_trait_object_is_equal_to_one_holding_an_equal_value() {
        let output = run_with_traits(
            "  let a: dyn Display = Booking(id: 1)\n  let b: dyn Display = Booking(id: 1)\n  let c: dyn Display = Receipt(total: 1)\n  console.println(\"{a == b} {a == c}\")?",
        )
        .output;
        assert_eq!(output, "true false\n");
    }

    // ------------------------------------------------------------ imports

    /// The module a body runs in is the module that declares it, so an
    /// imported function resolves its own names where it was written.
    #[test]
    fn an_imported_function_runs_in_the_module_that_declares_it() {
        let run = run_modules(&[
            (
                "greet",
                "use console.println\n\nfn punctuation() -> String {\n  \"!\"\n}\n\n\
                 /// Greets by name.\nexport fn greeting(name: String) -> String {\n  \"Hello, {name}{punctuation()}\"\n}\n",
            ),
            (
                "app",
                "use console.println\nuse greet.greeting\n\n\
                 /// Entry point.\nexport fn main() -> Result<Unit, Error> {\n  console.println(greeting(\"world\"))?\n  Ok(())\n}\n",
            ),
        ]);
        assert_eq!(run.output, "Hello, world!\n");
    }

    #[test]
    fn a_module_imported_whole_is_called_qualified() {
        let run = run_modules(&[
            (
                "greet",
                "/// Greets by name.\nexport fn greeting(name: String) -> String {\n  \"Hello, {name}!\"\n}\n",
            ),
            (
                "app",
                "use console.println\nuse greet\n\n\
                 /// Entry point.\nexport fn main() -> Result<Unit, Error> {\n  console.println(greet.greeting(\"world\"))?\n  Ok(())\n}\n",
            ),
        ]);
        assert_eq!(run.output, "Hello, world!\n");
    }

    #[test]
    fn an_imported_struct_is_constructed_and_its_methods_run() {
        let run = run_modules(&[
            (
                "booking",
                "/// A booking.\nexport struct Booking {\n  id: String\n}\n\n\
                 impl Booking {\n  /// The id, in a sentence.\n  export fn describe(self) -> String {\n    \"booking {self.id}\"\n  }\n}\n",
            ),
            (
                "app",
                "use console.println\nuse booking.Booking\n\n\
                 /// Entry point.\nexport fn main() -> Result<Unit, Error> {\n  \
                 let made = Booking(id: \"7\")\n  console.println(made.describe())?\n  Ok(())\n}\n",
            ),
        ]);
        assert_eq!(run.output, "booking 7\n");
    }

    /// A value carries the module that declares its type, so a method of an
    /// imported type dispatches even when the value crossed a boundary.
    #[test]
    fn an_imported_type_s_value_keeps_its_methods_across_a_boundary() {
        let run = run_modules(&[
            (
                "booking",
                "/// A booking.\nexport struct Booking {\n  id: String\n}\n\n\
                 impl Booking {\n  /// The id, in a sentence.\n  export fn describe(self) -> String {\n    \"booking {self.id}\"\n  }\n}\n\n\
                 /// Makes one.\nexport fn make() -> Booking {\n  Booking(id: \"9\")\n}\n",
            ),
            (
                "app",
                "use console.println\nuse booking.make\n\n\
                 /// Entry point.\nexport fn main() -> Result<Unit, Error> {\n  console.println(make().describe())?\n  Ok(())\n}\n",
            ),
        ]);
        assert_eq!(run.output, "booking 9\n");
    }

    #[test]
    fn an_imported_enum_s_cases_are_built_and_matched() {
        let run = run_modules(&[
            (
                "levels",
                "/// Levels.\nexport enum LogLevel {\n  Debug\n  Info\n}\n",
            ),
            (
                "app",
                "use console.println\nuse levels.LogLevel\n\n\
                 /// Names a level.\nfn name(level: LogLevel) -> String {\n  \
                 match level {\n    LogLevel.Debug => \"debug\"\n    LogLevel.Info => \"info\"\n  }\n}\n\n\
                 /// Entry point.\nexport fn main() -> Result<Unit, Error> {\n  console.println(name(LogLevel.Info))?\n  Ok(())\n}\n",
            ),
        ]);
        assert_eq!(run.output, "info\n");
    }

    /// An enum reached through a module imported whole: `levels.LogLevel`
    /// names the type, and the case follows it.
    #[test]
    fn an_enum_case_is_reached_through_a_module_imported_whole() {
        let run = run_modules(&[
            (
                "levels",
                "/// Levels.\nexport enum LogLevel {\n  Debug\n  Info\n}\n",
            ),
            (
                "app",
                "use console.println\nuse levels\n\n\
                 /// Entry point.\nexport fn main() -> Result<Unit, Error> {\n  console.println(\"{levels.LogLevel.Info}\")?\n  Ok(())\n}\n",
            ),
        ]);
        assert_eq!(run.output, "Info\n");
    }

    /// An imported function is an ordinary handle value, so it can be passed
    /// where any other closure can.
    #[test]
    fn an_imported_function_is_an_ordinary_value() {
        let run = run_modules(&[
            (
                "greet",
                "/// Greets by name.\nexport fn greeting(name: String) -> String {\n  \"Hello, {name}!\"\n}\n",
            ),
            (
                "app",
                "use console.println\nuse greet\n\n\
                 /// Applies `f`.\nfn apply(f: fn(String) -> String) -> String {\n  f(\"world\")\n}\n\n\
                 /// Entry point.\nexport fn main() -> Result<Unit, Error> {\n  console.println(apply(greet.greeting))?\n  Ok(())\n}\n",
            ),
        ]);
        assert_eq!(run.output, "Hello, world!\n");
    }

    /// `export` is the whole of a module's boundary: a qualified name
    /// reaches exactly what a `use` of it would.
    #[test]
    fn a_qualified_name_that_is_not_exported_is_refused() {
        let (sources, program) = program_of_modules(&[
            (
                "greet",
                "fn secret() -> String {\n  \"s\"\n}\n\n/// Greets.\nexport fn greeting() -> String {\n  \"hi\"\n}\n",
            ),
            (
                "app",
                "use greet\n\n/// Entry point.\nexport fn main() -> String {\n  greet.secret()\n}\n",
            ),
        ]);
        let error = run_in(
            &program,
            &sources,
            "app",
            "main",
            &[],
            &["console"],
            BTreeMap::new(),
        )
        .error();
        assert!(error.message.contains("not exported"), "{}", error.message);
    }

    #[test]
    fn a_module_used_as_a_value_is_refused() {
        let (sources, program) = program_of_modules(&[
            (
                "greet",
                "/// Greets.\nexport fn greeting() -> String {\n  \"hi\"\n}\n",
            ),
            (
                "app",
                "use greet\n\n/// Entry point.\nexport fn main() -> String {\n  let m = greet\n  \"x\"\n}\n",
            ),
        ]);
        let error = run_in(
            &program,
            &sources,
            "app",
            "main",
            &[],
            &["console"],
            BTreeMap::new(),
        )
        .error();
        assert!(
            error.message.contains("is a module, not a value"),
            "{}",
            error.message
        );
    }

    /// A host call inside an imported function is charged to the grant the
    /// host gave the entry, not to the module that wrote the call.
    #[test]
    fn a_host_call_inside_an_imported_function_still_needs_the_grant() {
        let (sources, program) = program_of_modules(&[
            (
                "log",
                "use console.println\n\n/// Logs.\nexport fn log(msg: String) -> Result<Unit, Error> {\n  console.println(msg)\n}\n",
            ),
            (
                "app",
                "use log.log\n\n/// Entry point.\nexport fn main() -> Result<Unit, Error> {\n  log(\"hi\")?\n  Ok(())\n}\n",
            ),
        ]);
        let granted = run_in(
            &program,
            &sources,
            "app",
            "main",
            &[],
            &["console"],
            BTreeMap::new(),
        );
        assert_eq!(granted.output, "hi\n");

        let denied = run_in(&program, &sources, "app", "main", &[], &[], BTreeMap::new());
        assert!(denied.value.is_err() || denied.output.is_empty());
    }

    // ------------------------------------------ conformances across modules

    const DISPLAY: &str = "\
/// Renders itself.
export trait Display {
  /// The full form.
  fn describe(self) -> String

  /// A short form, defaulting to the full one.
  fn label(self) -> String { \"<{self.describe()}>\" }
}

/// Renders anything that conforms, through static dispatch.
export fn render<T: Display>(value: T) -> String {
  value.label()
}

/// Renders through dynamic dispatch.
export fn renderDyn(value: dyn Display) -> String {
  value.label()
}
";

    const BOOKING: &str = "\
/// A booking.
export struct Booking {
  id: Int
}
";

    /// ADR 0006 allows the conformance where the type is declared, so the
    /// trait may be imported; both dispatch forms must reach it.
    #[test]
    fn a_conformance_to_an_imported_trait_dispatches_both_ways() {
        let booking = format!(
            "use display.Display\n\n{BOOKING}\nimpl Display for Booking {{\n  \
             /// The full form.\n  fn describe(self) -> String {{\n    \"booking {{self.id}}\"\n  }}\n}}\n"
        );
        let run = run_modules(&[
            ("display", DISPLAY),
            ("booking", &booking),
            (
                "app",
                "use console.println\nuse booking.Booking\nuse display.render\nuse display.renderDyn\n\n\
                 /// Entry point.\nexport fn main() -> Result<Unit, Error> {\n  \
                 let one = Booking(id: 7)\n  \
                 console.println(render(one))?\n  \
                 console.println(renderDyn(one))?\n  \
                 Ok(())\n}\n",
            ),
        ]);
        // The default body comes from the trait's module, the `describe` it
        // calls from the conformance's, and both dispatch forms agree.
        assert_eq!(run.output, "<booking 7>\n<booking 7>\n");
    }

    /// And the reverse: the conformance is declared with the trait, for an
    /// imported type, so the type's methods do not all live with the type.
    #[test]
    fn a_conformance_to_an_imported_type_dispatches_both_ways() {
        let display = format!(
            "use booking.Booking\n\n{DISPLAY}\nimpl Display for Booking {{\n  \
             /// The full form.\n  fn describe(self) -> String {{\n    \"booking {{self.id}}\"\n  }}\n}}\n"
        );
        let run = run_modules(&[
            ("booking", BOOKING),
            ("display", &display),
            (
                "app",
                "use console.println\nuse booking.Booking\nuse display.render\nuse display.Display\n\n\
                 /// Entry point.\nexport fn main() -> Result<Unit, Error> {\n  \
                 let one = Booking(id: 7)\n  \
                 console.println(render(one))?\n  \
                 console.println(one.describe())?\n  \
                 let shown: dyn Display = one\n  \
                 console.println(shown.label())?\n  \
                 Ok(())\n}\n",
            ),
        ]);
        assert_eq!(run.output, "<booking 7>\nbooking 7\n<booking 7>\n");
    }

    /// A `dyn` value names its trait by the module that declares it,
    /// wherever the conversion was written, so two `dyn` values of the same
    /// trait built in different modules are the same kind of value.
    #[test]
    fn a_dyn_value_names_its_trait_by_the_module_that_declares_it() {
        let booking = format!(
            "use display.Display\n\n{BOOKING}\nimpl Display for Booking {{\n  \
             /// The full form.\n  fn describe(self) -> String {{\n    \"b\"\n  }}\n}}\n\n\
             /// Wraps one here, in the module that declares the type.\n\
             export fn shown(value: Booking) -> dyn Display {{\n  value\n}}\n"
        );
        let (sources, program) = program_of_modules(&[
            ("display", DISPLAY),
            ("booking", &booking),
            (
                "app",
                "use booking.Booking\nuse booking.shown\nuse display.Display\n\n\
                 /// Entry point: wraps one here too.\n\
                 export fn main() -> Bool {\n  \
                 let here: dyn Display = Booking(id: 1)\n  \
                 here == shown(Booking(id: 1))\n}\n",
            ),
        ]);
        let run = run_in(
            &program,
            &sources,
            "app",
            "main",
            &[],
            &["console"],
            BTreeMap::new(),
        );
        assert_eq!(run.value().to_string(), "true");
    }

    // ------------------------------------------------------------- rule 1

    #[test]
    fn struct_fields_copy_and_vector_handles_alias() {
        let source = r#"
use console.println

struct Draft {
  count: Int
  guests: Vector<String>
}

export fn main() -> Result<Unit, Error> {
  var original = Draft(count: 1, guests: Vector.of("Alice"))
  var alias = original
  alias.count = 2
  alias.guests.push("Bob")
  console.println("{original.count} {alias.count}")?
  console.println("{original.guests.length()} {alias.guests.length()}")?
  Ok(())
}
"#;
        let run = run_entry_of(source, "main", &[]);
        assert_eq!(run.output, "1 2\n2 2\n");
    }

    #[test]
    fn passing_a_struct_argument_copies_it() {
        let source = r#"
use console.println

struct Point {
  x: Int
}

fn shift(point: Point) -> Int {
  point.x
}

export fn main() -> Result<Unit, Error> {
  var origin = Point(x: 1)
  let seen = shift(origin)
  origin.x = 9
  console.println("{seen} {origin.x}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "1 9\n");
    }

    // ------------------------------------------------------------- rule 2

    #[test]
    fn assigning_to_a_let_binding_is_rejected() {
        let error = error_of("  let total = 1\n  total = 2");
        assert!(
            error.message.contains("read-only place"),
            "{}",
            error.message
        );
        assert!(error
            .rule
            .unwrap()
            .contains("`let` creates a read-only place"));
    }

    #[test]
    fn assigning_to_a_var_field_updates_the_place() {
        let source = r#"
use console.println

struct Counter {
  value: Int
}

export fn main() -> Result<Unit, Error> {
  var counter = Counter(value: 1)
  counter.value += 4
  console.println("{counter.value}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "5\n");
    }

    // ------------------------------------------------------------- rule 3

    const COUNTER: &str = r#"
use console.println

struct Counter {
  value: Int
}

impl Counter {
  fn bump(var self) {
    self.value = self.value + 1
  }

  fn read(self) -> Int {
    self.value
  }
}
"#;

    #[test]
    fn var_self_mutation_is_visible_in_the_caller() {
        let source = format!(
            "{COUNTER}
export fn main() -> Result<Unit, Error> {{
  var counter = Counter(value: 1)
  counter.bump()
  counter.bump()
  console.println(\"{{counter.value}} {{counter.read()}}\")?
  Ok(())
}}
"
        );
        assert_eq!(run_entry_of(&source, "main", &[]).output, "3 3\n");
    }

    #[test]
    fn var_self_through_a_let_binding_is_rejected() {
        let source = format!(
            "{COUNTER}
export fn main() -> Result<Unit, Error> {{
  let counter = Counter(value: 1)
  counter.bump()
  Ok(())
}}
"
        );
        let error = run_entry_of(&source, "main", &[]).error();
        assert!(
            error.message.contains("`var self`") && error.message.contains("read-only place"),
            "{}",
            error.message
        );
    }

    #[test]
    fn var_self_on_a_temporary_is_rejected() {
        let source = format!(
            "{COUNTER}
export fn main() -> Result<Unit, Error> {{
  Counter(value: 1).bump()
  Ok(())
}}
"
        );
        let error = run_entry_of(&source, "main", &[]).error();
        assert!(
            error.message.contains("is not a place"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_var_parameter_aliases_the_caller_place() {
        let source = r#"
use console.println

fn fill(var output: Vector<Int>) {
  output.push(1)
  output.push(2)
}

export fn main() -> Result<Unit, Error> {
  var items = Vector.of()
  fill(var items)
  console.println("{items}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "[1, 2]\n");
    }

    #[test]
    fn a_var_parameter_must_be_marked_at_the_call_site() {
        let source = r#"
fn fill(var output: Vector<Int>) {
  output.push(1)
}

export fn main() -> Result<Unit, Error> {
  var items = Vector.of()
  fill(items)
  Ok(())
}
"#;
        let error = run_entry_of(source, "main", &[]).error();
        assert!(
            error.message.contains("declared `var`"),
            "{}",
            error.message
        );
        assert_eq!(error.help.unwrap(), "write `fill(var output)`");
    }

    #[test]
    fn a_var_argument_needs_a_mutable_place() {
        let source = r#"
fn fill(var output: Vector<Int>) {
  output.push(1)
}

export fn main() -> Result<Unit, Error> {
  let items = Vector.of()
  fill(var items)
  Ok(())
}
"#;
        let error = run_entry_of(source, "main", &[]).error();
        assert!(
            error.message.contains("read-only place"),
            "{}",
            error.message
        );
    }

    // ------------------------------------------------------------- rule 4

    #[test]
    fn array_literals_are_arrays_and_vector_of_builds_a_vector() {
        assert_eq!(
            output_of("  console.println(\"{[1, 2].length()} {Vector.of(1, 2, 3).length()}\")?"),
            "2 3\n"
        );
    }

    #[test]
    fn freeze_consumes_uniquely_owned_storage() {
        let source = r#"
use console.println

export fn main() -> Result<Unit, Error> {
  var items = Vector.of(1)
  items.push(2)
  let frozen = items.freeze()
  console.println("{frozen.length()} {frozen}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "2 [1, 2]\n");
    }

    #[test]
    fn a_frozen_vector_is_no_longer_usable() {
        let source = r#"
export fn main() -> Result<Unit, Error> {
  var items = Vector.of(1)
  let frozen = items.freeze()
  items.push(2)
  Ok(())
}
"#;
        let error = run_entry_of(source, "main", &[]).error();
        assert!(
            error.message.contains("already consumed"),
            "{}",
            error.message
        );
    }

    #[test]
    fn freeze_on_aliased_storage_points_at_to_array() {
        let source = r#"
export fn main() -> Result<Unit, Error> {
  var items = Vector.of(1)
  var alias = items
  let frozen = items.freeze()
  Ok(())
}
"#;
        let error = run_entry_of(source, "main", &[]).error();
        assert!(error.message.contains("freeze()"), "{}", error.message);
        assert!(
            error.help.unwrap().contains("toArray()"),
            "the diagnostic names the O(n) fallback"
        );
    }

    #[test]
    fn to_array_produces_an_independent_array() {
        let source = r#"
use console.println

export fn main() -> Result<Unit, Error> {
  var items = Vector.of(1)
  let snapshot = items.toArray()
  items.push(2)
  console.println("{snapshot.length()} {items.length()}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "1 2\n");
    }

    // ------------------------------------------------------- `is` and `Snapshot`

    #[test]
    fn is_compares_vector_storage_identity() {
        assert_eq!(
            output_of(
                "  var a = Vector.of(1, 2)\n  var b = a\n  var c = Vector.of(1, 2)\n  \
                 println(\"{a is b} {a is c}\")?"
            ),
            "true false\n"
        );
    }

    #[test]
    fn is_rejects_a_type_mismatch_at_runtime() {
        let error = error_of("  println(\"{Vector.of(1) is 1}\")?");
        assert!(
            error.message.contains("cannot compare the identity"),
            "{}",
            error.message
        );
    }

    #[test]
    fn is_rejects_a_value_type_at_runtime() {
        let error = error_of("  println(\"{1 is 1}\")?");
        assert_eq!(error.message, "identity is not available for `Int`");
    }

    #[test]
    fn snapshot_of_a_vector_allocates_independent_storage() {
        let source = r#"
use console.println

export fn main() -> Result<Unit, Error> {
  var original = Vector.of(1, 2)
  var copy = original.snapshot()
  copy.push(3)
  console.println("{original.length()} {copy.length()}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "2 3\n");
    }

    #[test]
    fn snapshot_recurses_into_a_vector_s_own_vector_elements() {
        let source = r#"
use console.println

export fn main() -> Result<Unit, Error> {
  var inner = Vector.of(1)
  var outer = Vector.of(inner)
  var copy = outer.snapshot()
  var innerCopy = copy.get(0).unwrapOr(Vector.of())
  innerCopy.push(2)
  console.println("{inner.length()} {innerCopy.length()}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "1 2\n");
    }

    #[test]
    fn snapshot_dispatches_to_a_struct_s_own_conformance() {
        let source = r#"
use console.println

struct Booking(id: Int)

impl Snapshot for Booking {
  fn snapshot(self) -> Booking { Booking(id: self.id) }
}

export fn main() -> Result<Unit, Error> {
  let booking = Booking(id: 1)
  console.println("{booking.snapshot()}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "Booking(id: 1)\n");
    }

    #[test]
    fn snapshot_is_not_implemented_for_a_closure() {
        let error =
            error_of("  let handler = fn(x: Int) { x }\n  println(\"{handler.snapshot()}\")?");
        assert_eq!(error.message, "`fn` does not implement `Snapshot`");
        assert!(error.rule.unwrap().contains("Closures"));
    }

    #[test]
    fn push_through_a_read_only_place_is_rejected() {
        let source = r#"
export fn main() -> Result<Unit, Error> {
  let items = Vector.of(1)
  items.push(2)
  Ok(())
}
"#;
        let error = run_entry_of(source, "main", &[]).error();
        assert!(error.message.contains("`var self`"), "{}", error.message);
    }

    // ------------------------------------------------------------- rule 5

    const TRY: &str = r#"
use console.println

fn okValue() -> Result<Int, Error> {
  Ok(1)
}

fn errValue() -> Result<Int, Error> {
  Err(Error("boom"))
}

fn someValue() -> Option<Int> {
  Some(2)
}

fn noneValue() -> Option<Int> {
  None
}
"#;

    #[test]
    fn try_unwraps_ok_and_some() {
        let source = format!(
            "{TRY}
export fn main() -> Result<Unit, Error> {{
  let a = okValue()?
  let b = someValue()?
  console.println(\"{{a}} {{b}}\")?
  Ok(())
}}
"
        );
        assert_eq!(run_entry_of(&source, "main", &[]).output, "1 2\n");
    }

    #[test]
    fn try_returns_the_error_from_the_current_function() {
        let source = format!(
            "{TRY}
export fn main() -> Result<Int, Error> {{
  let a = errValue()?
  console.println(\"unreachable\")?
  Ok(a)
}}
"
        );
        let run = run_entry_of(&source, "main", &[]);
        assert_eq!(run.output, "");
        assert_eq!(run.value().to_string(), "Err(boom)");
    }

    #[test]
    fn try_returns_none_from_the_current_function() {
        let source = format!(
            "{TRY}
fn firstDigit() -> Option<Int> {{
  let value = noneValue()?
  Some(value)
}}

export fn main() -> Option<Int> {{
  firstDigit()
}}
"
        );
        assert_eq!(
            run_entry_of(&source, "main", &[]).value().to_string(),
            "None"
        );
    }

    #[test]
    fn try_on_a_plain_value_is_rejected() {
        let error = error_of("  let x = 1?");
        assert!(
            error
                .message
                .contains("`?` needs a `Result` or an `Option`"),
            "{}",
            error.message
        );
    }

    // ------------------------------------------------------------- rule 6

    #[test]
    fn arguments_are_evaluated_left_to_right() {
        let source = r#"
use console.println

fn note(var log: Vector<String>, name: String) -> Int {
  log.push(name)
  0
}

export fn main() -> Result<Unit, Error> {
  var log = Vector.of()
  let total = note(var log, "a") + note(var log, "b")
  console.println("{log}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "[a, b]\n");
    }

    // ------------------------------------------------------------- rule 7

    #[test]
    fn integer_overflow_names_the_operation() {
        let error = error_of("  var big = 9223372036854775807\n  big = big + 1");
        assert_eq!(error.message, "`Int` addition overflowed");
    }

    #[test]
    fn division_by_zero_is_a_runtime_error() {
        assert_eq!(
            error_of("  let x = 1 / 0").message,
            "`Int` division by zero"
        );
        assert_eq!(
            error_of("  let x = 1 % 0").message,
            "`Int` remainder by zero"
        );
    }

    #[test]
    fn mixed_numeric_operands_are_rejected() {
        let error = error_of("  let x = 1 + 1.0");
        assert!(
            error.message.contains("not defined for `Int` and `Float`"),
            "{}",
            error.message
        );
    }

    #[test]
    fn adding_a_string_to_an_int_is_rejected() {
        let error = error_of("  let x = \"a\" + 1");
        assert!(
            error.message.contains("not defined for `String` and `Int`"),
            "{}",
            error.message
        );
    }

    #[test]
    fn adding_two_strings_points_at_interpolation() {
        let error = error_of("  let x = \"a\" + \"b\"");
        assert_eq!(error.message, "`+` is not defined for `String`");
        assert!(error.help.unwrap().contains("interpolation"));
    }

    // ------------------------------------------------------------- rule 8

    /// Static exhaustiveness abstains when the scrutinee's enum cannot be
    /// determined from the patterns, so every match it abstains on still
    /// needs this runtime guard. The fixture is deliberately opaque to that
    /// analysis: two enums declare a case named `Red`, so a bare `Red`
    /// pattern names neither of them unambiguously. Do not make it
    /// analysable -- that would delete the coverage this test exists for.
    #[test]
    fn a_match_with_no_matching_arm_is_a_runtime_error() {
        let source = r#"
enum Color {
  Red
  Green
}

enum Wine {
  Red
  White
}

export fn main() -> Result<Unit, Error> {
  let color = Color.Green
  let name = match color {
    Red => "red"
  }
  Ok(())
}
"#;
        let error = run_entry_of(source, "main", &[]).error();
        assert!(
            error.message.contains("no `match` arm covers"),
            "{}",
            error.message
        );
        assert_eq!(error.rule.unwrap(), "`match` must cover every enum case.");
    }

    #[test]
    fn match_binds_enum_payloads_and_literals() {
        let source = r#"
use console.println

enum Shape {
  Dot
  Line(Int)
}

fn describe(shape: Shape) -> String {
  match shape {
    Shape.Dot => "dot"
    Shape.Line(length) => "line {length}"
  }
}

export fn main() -> Result<Unit, Error> {
  console.println(describe(Shape.Dot))?
  console.println(describe(Shape.Line(3)))?
  let word = match 2 {
    1 => "one"
    other => "many"
  }
  console.println(word)?
  Ok(())
}
"#;
        assert_eq!(
            run_entry_of(source, "main", &[]).output,
            "dot\nline 3\nmany\n"
        );
    }

    // ------------------------------------------------------------- rule 9

    #[test]
    fn equality_is_value_equality() {
        let source = r#"
use console.println

struct Point {
  x: Int
}

export fn main() -> Result<Unit, Error> {
  console.println("{Point(x: 1) == Point(x: 1)} {[1, 2] == [1, 3]}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "true false\n");
    }

    #[test]
    fn comparing_different_types_is_rejected() {
        let error = error_of("  let same = 1 == \"1\"");
        assert!(
            error.message.contains("cannot compare `Int` with `String`"),
            "{}",
            error.message
        );
    }

    // ------------------------------------------------------------ rule 10

    #[test]
    fn blocks_ifs_and_matches_are_expressions() {
        let source = r#"
use console.println

fn classify(value: Int) -> String {
  if value > 0 {
    return "positive"
  }
  "other"
}

export fn main() -> Result<Unit, Error> {
  let doubled = {
    let base = 3
    base * 2
  }
  let label = if doubled > 5 { "big" } else { "small" }
  console.println("{doubled} {label} {classify(1)} {classify(0)}")?
  Ok(())
}
"#;
        assert_eq!(
            run_entry_of(source, "main", &[]).output,
            "6 big positive other\n"
        );
    }

    #[test]
    fn loops_run_to_completion() {
        let source = r#"
use console.println

export fn main() -> Result<Unit, Error> {
  var total = 0
  for value in [1, 2, 3] {
    total += value
  }
  var count = 0
  while count < 2 {
    count += 1
  }
  console.println("{total} {count}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "6 2\n");
    }

    #[test]
    fn a_for_loop_evaluates_to_the_value_of_break() {
        let source = r#"
use console.println

export fn main() -> Result<Unit, Error> {
  let found = for value in [1, 2, 3, 4] {
    if value == 3 {
      break value * 10
    }
  }
  console.println("{found}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "30\n");
    }

    #[test]
    fn a_loop_that_never_breaks_evaluates_to_unit() {
        let source = r#"
use console.println

export fn main() -> Result<Unit, Error> {
  let result = for value in [1, 2] {
    value
  }
  console.println("{result}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "()\n");
    }

    #[test]
    fn continue_skips_the_rest_of_an_iteration() {
        let source = r#"
use console.println

export fn main() -> Result<Unit, Error> {
  var total = 0
  for value in [1, 2, 3, 4] {
    if value % 2 == 0 {
      continue
    }
    total += value
  }
  console.println("{total}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "4\n");
    }

    #[test]
    fn a_while_loop_evaluates_to_the_value_of_break() {
        let source = r#"
use console.println

export fn main() -> Result<Unit, Error> {
  var count = 0
  let found = while true {
    count += 1
    if count == 3 {
      break count
    }
  }
  console.println("{found}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "3\n");
    }

    // ------------------------------------------------------------ rule 11

    #[test]
    fn closures_capture_by_value_at_creation_time() {
        let source = r#"
use console.println

export fn main() -> Result<Unit, Error> {
  var seen = 1
  let read = fn() {
    seen
  }
  seen = 2
  console.println("{read()} {seen}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "1 2\n");
    }

    // ------------------------------------------------------------ rule 12

    #[test]
    fn an_unqualified_use_reaches_the_host_module() {
        let source = r#"
use console.println

export fn main() -> Result<Unit, Error> {
  println("direct")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "direct\n");
    }

    #[test]
    fn an_ungranted_capability_is_rejected_at_the_host_boundary() {
        let source = r#"
use console.println

export fn main() -> Result<Unit, Error> {
  console.println("secret")?
  Ok(())
}
"#;
        let (sources, program) = program_of(source);
        let run = run_in(
            &program,
            &sources,
            "test",
            "main",
            &[],
            &[],
            BTreeMap::new(),
        );
        assert_eq!(run.output, "");
        let error = run.error();
        assert!(
            error.message.contains("requires the `console` capability"),
            "{}",
            error.message
        );
    }

    #[test]
    fn the_env_host_reads_only_what_the_host_supplied() {
        let source = r#"
use env.get
use console.println

export fn main() -> Result<Unit, Error> {
  console.println(env.get("PORT").unwrapOr("none"))?
  console.println(env.get("MISSING").unwrapOr("none"))?
  Ok(())
}
"#;
        let (sources, program) = program_of(source);
        let env = BTreeMap::from([("PORT".to_string(), "9000".to_string())]);
        let run = run_in(
            &program,
            &sources,
            "test",
            "main",
            &[],
            &["console", "env"],
            env,
        );
        assert_eq!(run.output, "9000\nnone\n");
    }

    // --------------------------------------------------------- builtins

    #[test]
    fn array_and_string_builtins() {
        let body = "  let items = [10, 20]\n  console.println(\"{items.get(0).unwrapOr(0)} {items.get(5).isNone()} {items.length()} {items.isEmpty()}\")?\n  console.println(\"{\"a bc  d\".words().length()} {\"abc\".length()} {\"\".isEmpty()}\")?";
        assert_eq!(output_of(body), "10 true 2 false\n3 3 true\n");
    }

    #[test]
    fn int_parse_returns_a_result() {
        let body = "  console.println(\"{Int.parse(\"12\").isOk()} {Int.parse(\"x\").isError()} {Int.parse(\"12\").unwrapOr(0)}\")?";
        let error = error_of(body);
        assert!(
            error.message.contains("has no method `unwrapOr`"),
            "`unwrapOr` belongs to `Option`, not `Result`: {}",
            error.message
        );
        assert_eq!(
            output_of(
                "  console.println(\"{Int.parse(\"12\").isOk()} {Int.parse(\"x\").isError()}\")?"
            ),
            "true true\n"
        );
    }

    #[test]
    fn map_error_accepts_a_trailing_closure() {
        let source = r#"
use console.println

enum ConfigError {
  InvalidPort(String)
}

export fn main() -> Result<Unit, Error> {
  let failed = Int.parse("x").mapError { ConfigError.InvalidPort("x") }
  let kept = Int.parse("7").mapError { ConfigError.InvalidPort("7") }
  console.println("{failed} {kept}")?
  Ok(())
}
"#;
        assert_eq!(
            run_entry_of(source, "main", &[]).output,
            "Err(InvalidPort(x)) Ok(7)\n"
        );
    }

    #[test]
    fn a_method_that_does_not_exist_names_the_receiver_type() {
        let error = error_of("  let x = [1].pop()");
        assert_eq!(error.message, "`Array` has no method `pop`");
    }

    // -------------------------------------------------------- ranges

    #[test]
    fn a_range_is_an_ordinary_value() {
        let output = output_of(
            r#"  let exclusive = 0..<3
  let inclusive = 0..3
  console.println("{exclusive} {inclusive}")?"#,
        );
        assert_eq!(output, "0..<3 0..3\n");
    }

    #[test]
    fn a_range_value_iterates_like_a_range_literal() {
        let output = output_of(
            r#"  let bounds = 0..<3
  var total = 0
  for value in bounds {
    total += value
  }
  for value in 1..3 {
    total += value
  }
  console.println("{total}")?"#,
        );
        assert_eq!(output, "9\n");
    }

    #[test]
    fn a_range_has_the_sequence_methods() {
        let output = output_of(
            r#"  let exclusive = 0..<3
  let inclusive = 0..3
  console.println("{exclusive.length()} {inclusive.length()}")?
  console.println("{exclusive.isEmpty()} {exclusive.contains(2)} {exclusive.contains(3)}")?
  console.println("{inclusive.contains(3)} {inclusive.contains(-1)}")?"#,
        );
        assert_eq!(output, "3 4\nfalse true false\ntrue false\n");
    }

    #[test]
    fn a_reversed_range_is_empty_and_iterates_zero_times() {
        let output = output_of(
            r#"  let reversed = 3..<0
  var rounds = 0
  for _value in reversed {
    rounds += 1
  }
  console.println("{reversed} {reversed.length()} {reversed.isEmpty()} {rounds}")?"#,
        );
        assert_eq!(output, "3..<0 0 true 0\n");
    }

    #[test]
    fn ranges_compare_by_value() {
        let output =
            output_of(r#"  console.println("{0..<3 == 0..<3} {0..<3 == 0..3} {0..<3 == 1..<3}")?"#);
        assert_eq!(output, "true false false\n");
    }

    #[test]
    fn a_range_bound_must_be_an_int() {
        let error = error_of("  let bad = 0..<\"3\"");
        assert!(
            error.message.contains("a range bound must be an `Int`"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_range_has_no_method_it_does_not_declare() {
        let error = error_of("  let bounds = 0..<3\n  let bad = bounds.reverse()");
        assert_eq!(error.message, "`Range` has no method `reverse`");
    }

    // ---------------------------------------------- one spelling: length

    #[test]
    fn count_is_rejected_and_names_the_length_spelling() {
        let bodies = [
            "  let n = [1, 2].count()",
            "  let n = Vector.of(1, 2).count()",
            "  let n = \"a b\".count()",
            "  let n = (0..<3).count()",
            "  let n = Map.of().count()",
            "  let n = Set.of().count()",
        ];
        for body in bodies {
            let error = error_of(body);
            assert!(
                error
                    .message
                    .contains("Cove spells the number of elements `length()`"),
                "{body}: {}",
                error.message
            );
            assert_eq!(
                error.help.as_deref(),
                Some("write `length()` instead of `count()`"),
                "{body}"
            );
        }
    }

    #[test]
    fn length_is_the_one_spelling_every_sequence_answers() {
        let output = output_of(
            r#"  console.println("{[1, 2].length()} {Vector.of(1).length()} {"ab".length()} {(0..<4).length()}")?"#,
        );
        assert_eq!(output, "2 1 2 4\n");
    }

    // ------------------------------------------------------- map and set

    #[test]
    fn map_of_builds_a_map_and_answers_its_methods() {
        let output = output_of(
            r#"  let ages = Map.of(
    MapEntry(key: "Alice", value: 30),
    MapEntry(key: "Bob", value: 25)
  )
  console.println("{ages}")?
  console.println("{ages.length()} {ages.isEmpty()}")?
  console.println("{ages.get("Alice")} {ages.get("Zoe")}")?
  console.println("{ages.contains("Bob")} {ages.contains("Zoe")}")?
  console.println("{ages.keys()} {ages.values()}")?"#,
        );
        assert_eq!(
            output,
            "{Alice: 30, Bob: 25}\n2 false\nSome(30) None\ntrue false\n[Alice, Bob] [30, 25]\n"
        );
    }

    #[test]
    fn an_empty_map_is_empty() {
        let output = output_of(r#"  console.println("{Map.of()} {Map.of().isEmpty()}")?"#);
        assert_eq!(output, "{} true\n");
    }

    #[test]
    fn map_of_rejects_a_duplicate_key() {
        let error = error_of(
            r#"  let bad = Map.of(
    MapEntry(key: "x", value: 1),
    MapEntry(key: "x", value: 2)
  )"#,
        );
        assert_eq!(
            error.message,
            "`Map.of` was given the key `x` more than once"
        );
    }

    #[test]
    fn map_of_rejects_an_argument_that_is_not_a_map_entry() {
        let error = error_of("  let bad = Map.of(1)");
        assert!(
            error.message.contains("`Map.of` expects `MapEntry` values"),
            "{}",
            error.message
        );
    }

    #[test]
    fn map_entry_labels_are_key_then_value_in_declaration_order() {
        let error = error_of(r#"  let bad = MapEntry(value: 1, key: "x")"#);
        assert!(
            error.message.contains("out of declaration order"),
            "{}",
            error.message
        );
    }

    #[test]
    fn map_get_and_contains_reject_an_invalid_key_type() {
        let error = error_of(
            r#"  let m = Map.of()
  let bad = m.get(Vector.of(1))"#,
        );
        assert_eq!(
            error.message,
            "`Map.get` cannot use a `Vector` as a map key"
        );
        assert!(
            error
                .rule
                .as_deref()
                .unwrap_or_default()
                .contains("Mutable handles and structs containing them are not valid map keys"),
            "{:?}",
            error.rule
        );
    }

    #[test]
    fn map_inserted_and_removed_return_a_new_map_and_do_not_mutate_the_original() {
        let output = output_of(
            r#"  let original = Map.of(MapEntry(key: "a", value: 1))
  let inserted = original.inserted("b", 2)
  let removed = inserted.removed("a")
  console.println("{original} {inserted} {removed}")?"#,
        );
        assert_eq!(output, "{a: 1} {a: 1, b: 2} {b: 2}\n");
    }

    #[test]
    fn maps_compare_by_structural_equality() {
        let output = output_of(
            r#"  let a = Map.of(MapEntry(key: "x", value: 1))
  let b = Map.of(MapEntry(key: "x", value: 1))
  let c = Map.of(MapEntry(key: "x", value: 2))
  console.println("{a == b} {a == c}")?"#,
        );
        assert_eq!(output, "true false\n");
    }

    #[test]
    fn map_iterates_map_entries_in_ascending_key_order() {
        let output = output_of(
            r#"  let scores = Map.of(
    MapEntry(key: "b", value: 2),
    MapEntry(key: "a", value: 1)
  )
  for entry in scores {
    console.println("{entry.key} {entry.value}")?
  }"#,
        );
        assert_eq!(output, "a 1\nb 2\n");
    }

    #[test]
    fn set_of_builds_a_set_and_answers_its_methods() {
        let output = output_of(
            r#"  let names = Set.of("b", "a", "c")
  console.println("{names}")?
  console.println("{names.length()} {names.isEmpty()}")?
  console.println("{names.contains("a")} {names.contains("z")}")?
  console.println("{names.toArray()}")?"#,
        );
        assert_eq!(output, "{a, b, c}\n3 false\ntrue false\n[a, b, c]\n");
    }

    #[test]
    fn set_of_rejects_a_duplicate_element() {
        let error = error_of("  let bad = Set.of(1, 1)");
        assert_eq!(
            error.message,
            "`Set.of` was given the element `1` more than once"
        );
    }

    #[test]
    fn set_of_rejects_an_invalid_element_type() {
        let error = error_of("  let bad = Set.of(Vector.of(1))");
        assert_eq!(
            error.message,
            "`Set.of` cannot use a `Vector` as a set element"
        );
    }

    #[test]
    fn set_inserted_and_removed_return_a_new_set_and_do_not_mutate_the_original() {
        let output = output_of(
            r#"  let original = Set.of(1, 2)
  let inserted = original.inserted(3)
  let removed = inserted.removed(1)
  console.println("{original} {inserted} {removed}")?"#,
        );
        assert_eq!(output, "{1, 2} {1, 2, 3} {2, 3}\n");
    }

    #[test]
    fn sets_compare_by_structural_equality() {
        let output = output_of(
            r#"  let a = Set.of(1, 2)
  let b = Set.of(2, 1)
  let c = Set.of(1)
  console.println("{a == b} {a == c}")?"#,
        );
        assert_eq!(output, "true false\n");
    }

    #[test]
    fn set_iterates_in_ascending_order() {
        let output = output_of(
            r#"  var total = 0
  for item in Set.of(3, 1, 2) {
    total = total * 10 + item
  }
  console.println("{total}")?"#,
        );
        assert_eq!(output, "123\n");
    }

    #[test]
    fn a_payload_free_enum_case_is_a_valid_map_key() {
        let run = colour_body(
            r#"  let byColour = Map.of(MapEntry(key: Colour.Red, value: "stop"))
  console.println("{byColour.get(Colour.Red)}")?"#,
        );
        assert_eq!(run.output, "Some(stop)\n");
    }

    #[test]
    fn an_enum_case_with_a_payload_is_a_valid_set_element() {
        let run = colour_body(
            r#"  let colours = Set.of(Colour.Red, Colour.Named("teal"))
  console.println("{colours.contains(Colour.Named("teal"))} {colours.contains(Colour.Named("blue"))}")?"#,
        );
        assert_eq!(run.output, "true false\n");
    }

    #[test]
    fn a_struct_built_only_from_ints_is_a_valid_set_element() {
        let run = point_body(
            r#"  let points = Set.of(Point(x: 1, y: 2), Point(x: 3, y: 4))
  console.println("{points.contains(Point(x: 1, y: 2))} {points.contains(Point(x: 9, y: 9))}")?"#,
        );
        assert_eq!(run.output, "true false\n");
    }

    #[test]
    fn a_struct_nested_inside_a_struct_is_a_valid_set_element() {
        let source = r#"
use console.println

struct Address {
  city: String
}

struct Person {
  name: String
  address: Address
}

export fn main() -> Result<Unit, Error> {
  let people = Set.of(
    Person(name: "Ada", address: Address(city: "London")),
    Person(name: "Grace", address: Address(city: "New York"))
  )
  console.println("{people.contains(Person(name: "Ada", address: Address(city: "London")))}")?
  console.println("{people.contains(Person(name: "Ada", address: Address(city: "Paris")))}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "true\nfalse\n");
    }

    #[test]
    fn an_array_built_only_from_ints_is_a_valid_set_element() {
        let output = output_of(
            r#"  let pairs = Set.of([1, 2], [3, 4])
  console.println("{pairs.contains([1, 2])} {pairs.contains([9])}")?"#,
        );
        assert_eq!(output, "true false\n");
    }

    #[test]
    fn a_struct_containing_a_vector_is_rejected_naming_the_nested_field() {
        let source = r#"
use console.println

struct Point {
  tags: Vector<Int>
}

export fn main() -> Result<Unit, Error> {
  let bad = Set.of(Point(tags: Vector.of(1)))
  Ok(())
}
"#;
        let error = run_entry_of(source, "main", &[]).error();
        assert_eq!(
            error.message,
            "`Set.of` cannot use a `Vector` inside `Point.tags` as a set element"
        );
    }

    #[test]
    fn a_float_is_rejected_as_a_key_for_a_reason_distinct_from_mutability() {
        let error = error_of("  let bad = Set.of(1.5)");
        assert_eq!(
            error.message,
            "`Set.of` cannot use a `Float` as a set element"
        );
        assert!(
            error.rule.as_deref().unwrap_or_default().contains("NaN"),
            "{:?}",
            error.rule
        );
    }

    // --------------------------------- associated functions on an enum

    const COLOUR: &str = r#"
use console.println

enum Colour {
  Red
  Named(String)
}

impl Colour {
  /// Returns the colour used when nothing was chosen.
  fn fallback() -> Colour {
    Colour.Red
  }

  /// Names this colour.
  fn describe(self) -> String {
    match self {
      Colour.Red => "red"
      Colour.Named(name) => name
    }
  }
}
"#;

    fn colour_body(body: &str) -> Run {
        run_entry_of(
            &format!(
                "{COLOUR}\nexport fn main() -> Result<Unit, Error> {{\n{body}\n  Ok(())\n}}\n"
            ),
            "main",
            &[],
        )
    }

    #[test]
    fn an_enum_can_declare_an_associated_function() {
        let run = colour_body("  console.println(\"{Colour.fallback()}\")?");
        assert_eq!(run.output, "Red\n");
    }

    #[test]
    fn an_enum_value_answers_its_methods() {
        let run = colour_body(
            "  console.println(\"{Colour.Red.describe()} {Colour.Named(\"teal\").describe()}\")?",
        );
        assert_eq!(run.output, "red teal\n");
    }

    #[test]
    fn a_case_wins_over_an_associated_function_of_the_same_name() {
        let source = r#"
use console.println

enum Signal {
  Ready
}

impl Signal {
  /// Shadowed by the case of the same name, which keeps naming the case.
  fn Ready() -> String {
    "the function"
  }
}

export fn main() -> Result<Unit, Error> {
  console.println("{Signal.Ready()}")?
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "Ready\n");
    }

    #[test]
    fn an_unknown_enum_member_names_both_possibilities() {
        let error = colour_body("  let missing = Colour.missing()").error();
        assert_eq!(
            error.message,
            "enum `Colour` has no case or associated function `missing`"
        );
        let help = error.help.unwrap();
        assert!(help.contains("known cases: Red, Named"), "{help}");
        assert!(
            help.contains("known functions: describe, fallback"),
            "{help}"
        );
    }

    // --------------------------------------------- struct initialization

    const POINT: &str = r#"
use console.println

struct Point {
  x: Int
  y: Int
}
"#;

    fn point_body(body: &str) -> Run {
        run_entry_of(
            &format!("{POINT}\nexport fn main() -> Result<Unit, Error> {{\n{body}\n  Ok(())\n}}\n"),
            "main",
            &[],
        )
    }

    #[test]
    fn positional_arguments_may_precede_labels() {
        let run = point_body("  console.println(\"{Point(1, y: 2)}\")?");
        assert_eq!(run.output, "Point(x: 1, y: 2)\n");
    }

    #[test]
    fn struct_initialization_reports_missing_unknown_and_duplicate_labels() {
        let missing = point_body("  let p = Point(x: 1)").error();
        assert!(missing.message.contains("field `y`"), "{}", missing.message);

        let unknown = point_body("  let p = Point(x: 1, z: 2)").error();
        assert!(
            unknown.message.contains("no parameter labeled `z`"),
            "{}",
            unknown.message
        );

        let duplicate = point_body("  let p = Point(x: 1, x: 2)").error();
        assert!(
            duplicate.message.contains("`x` more than once"),
            "{}",
            duplicate.message
        );
    }

    #[test]
    fn struct_initializer_labels_must_be_in_declaration_order() {
        let error = point_body("  let p = Point(y: 2, x: 1)").error();
        assert_eq!(
            error.message,
            "`Point` was given the label `x` out of declaration order"
        );
        assert_eq!(
            error.help.as_deref(),
            Some("write the arguments in this order: x, y")
        );
    }

    #[test]
    fn call_labels_must_be_in_declaration_order() {
        let source = r#"
use console.println

fn between(low: Int, high: Int) -> String {
  "[{low}, {high}]"
}

export fn main() -> Result<Unit, Error> {
  console.println(between(high: 6, low: 5))?
  Ok(())
}
"#;
        let error = run_entry_of(source, "main", &[]).error();
        assert_eq!(
            error.message,
            "`between` was given the label `low` out of declaration order"
        );
        assert_eq!(
            error.rule.as_deref(),
            Some(
                "Labeled arguments appear in declaration order, so argument order matches parameter order."
            )
        );
        assert_eq!(
            error.help.as_deref(),
            Some("write the arguments in this order: low, high")
        );
    }

    #[test]
    fn labels_in_declaration_order_are_accepted_after_positional_arguments() {
        let source = r#"
use console.println

fn measure(value: Int, unit: String = "m", prefix: String = "length") -> String {
  "{prefix} {value}{unit}"
}

export fn main() -> Result<Unit, Error> {
  console.println(measure(3, unit: "cm", prefix: "width"))?
  console.println(measure(3, prefix: "width"))?
  console.println(measure(value: 4, unit: "cm"))?
  Ok(())
}
"#;
        assert_eq!(
            run_entry_of(source, "main", &[]).output,
            "width 3cm
width 3m
length 4cm
"
        );
    }

    // --------------------------------------------------------- the entry

    #[test]
    fn an_entry_takes_no_parameters_or_one_array_of_strings() {
        let source = r#"
export fn main(first: String, second: String) -> Result<Unit, Error> {
  Ok(())
}
"#;
        let error = run_entry_of(source, "main", &[]).error();
        assert!(
            error
                .rule
                .unwrap()
                .contains("either no parameters or one `Array<String>`"),
            "{}",
            error.message
        );
    }

    // ------------------------------------------------------------- tasks
    //
    // Tasks run on threads of their own, so these tests assert what `await`
    // and scope exit produce, and never the order in which two independent
    // tasks happen to run. A test that depended on that order would be
    // pinning a race rather than the language.

    const TASKS: &str = r#"
use console.println

async fn answer() -> Int {
  7
}

async fn load(ok: Bool) -> Result<Int, Error> {
  if ok {
    Ok(1)
  } else {
    Err(Error("boom"))
  }
}
"#;

    /// A task body that cannot finish before a cancellation reaches it, and
    /// prints only if it does.
    ///
    /// With ADR 0008 a spawned task starts at once on a thread of its own, so
    /// a test that asserts a cancelled task "never ran" has to give it work
    /// to be stopped in the middle of. The loop stops at its next back-edge
    /// safepoint once the task is cancelled; the bound is there only so that
    /// a runtime which never delivers the cancellation fails the test instead
    /// of hanging.
    const SPINNING_TASK: &str =
        "      var i = 0\n      while i < 1000000000 {\n        i += 1\n      }\n      println(\"this must not run\")?";

    /// Runs `body` inside a `main` that returns `Result<Unit, Error>`, with
    /// the `async fn` helpers of [`TASKS`] in scope.
    fn run_task_body(body: &str) -> Run {
        run_entry_of(
            &format!("{TASKS}\nexport fn main() -> Result<Unit, Error> {{\n{body}\n  Ok(())\n}}\n"),
            "main",
            &[],
        )
    }

    #[test]
    fn an_async_fn_is_called_like_any_other_function_and_awaited() {
        let run = run_task_body("  let value = await answer()\n  println(\"{value}\")?");
        assert_eq!(run.output, "7\n");
    }

    /// An `async fn` runs its body at the call site, so a call that is never
    /// awaited has still run by the time the call returns. ADR 0008 gives a
    /// thread to `spawn` rather than to every `async fn`, so the assertion
    /// here is that the effect happened, not when.
    #[test]
    fn an_async_fn_that_is_never_awaited_still_runs() {
        let source = r#"
use console.println

async fn announce() -> Result<Unit, Error> {
  println("announced")?
  Ok(())
}

export fn main() -> Result<Unit, Error> {
  let ignored = announce()
  Ok(())
}
"#;
        let run = run_entry_of(source, "main", &[]);
        assert!(run.output.contains("announced"), "{:?}", run.output);
    }

    #[test]
    fn awaiting_a_result_propagates_with_a_question_mark() {
        let source = format!(
            "{TASKS}
export fn main() -> Result<Int, Error> {{
  let good = load(true).await()?
  println(\"{{good}}\")?
  let bad = load(false).await()?
  println(\"unreachable\")?
  Ok(bad)
}}
"
        );
        let run = run_entry_of(&source, "main", &[]);
        assert_eq!(run.output, "1\n");
        assert_eq!(run.value().to_string(), "Err(boom)");
    }

    /// `await` binds looser than `?`, so `await load()?` applies `?` to the
    /// handle rather than to the value inside it. The diagnostic names the
    /// spelling that works.
    #[test]
    fn a_question_mark_on_a_task_points_at_await() {
        let error = run_task_body("  let task = load(true)\n  let value = task?").error();
        assert_eq!(
            error.message,
            "`?` needs a `Result` or an `Option`, but found `Task`"
        );
        assert!(
            error.help.unwrap().contains("task.await()?"),
            "the diagnostic shows the correction"
        );
    }

    /// `await` binds tighter than `?`, so `await task()?` awaits and then
    /// propagates. The `?` applies to the `Result` the task produced, never
    /// to the task handle itself.
    #[test]
    fn a_question_mark_after_await_propagates_the_awaited_error() {
        let run = run_task_body("  let value = await load(true)?\n  println(\"{value}\")?");
        assert_eq!(run.output, "1\n");

        let error = run_task_body("  let value = await load(false)?").value;
        match error {
            Ok(Value::Enum(result)) => {
                assert_eq!(&*result.case, "Err");
                assert_eq!(result.payload[0].to_string(), "boom");
            }
            other => panic!("expected the awaited `Err` to propagate, found {other:?}"),
        }
    }

    #[test]
    fn both_await_spellings_settle_the_same_task() {
        let run = run_task_body(
            "  let prefix = await answer()\n  let postfix = answer().await()\n  println(\"{prefix} {postfix}\")?",
        );
        assert_eq!(run.output, "7 7\n");
    }

    #[test]
    fn a_scope_awaits_the_tasks_it_spawned() {
        let run = run_task_body(
            "  scope tasks {\n    let first = tasks.spawn { 1 }\n    let second = tasks.spawn { 2 }\n    let a = await first\n    let b = second.await()\n    println(\"{a} {b}\")?\n  }",
        );
        assert_eq!(run.output, "1 2\n");
    }

    #[test]
    fn leaving_a_scope_settles_a_task_the_body_never_awaited() {
        let run = run_task_body(
            "  scope tasks {\n    let ignored = tasks.spawn { println(\"the task ran\")? }\n  }\n  println(\"after the scope\")?",
        );
        assert_eq!(run.output, "the task ran\nafter the scope\n");
    }

    #[test]
    fn returning_from_a_scope_cancels_a_task_that_is_still_running() {
        let source = format!(
            "{TASKS}
export fn main() -> Result<Unit, Error> {{
  scope tasks {{
    let ignored = tasks.spawn {{
{SPINNING_TASK}
    }}
    return Ok(())
  }}
}}
"
        );
        let run = run_entry_of(&source, "main", &[]);
        assert_eq!(run.output, "");
        assert_eq!(run.value().to_string(), "Ok(())");
    }

    #[test]
    fn an_error_inside_a_scope_cancels_a_task_that_is_still_running() {
        let source = format!(
            "{TASKS}
export fn main() -> Result<Int, Error> {{
  scope tasks {{
    let ignored = tasks.spawn {{
{SPINNING_TASK}
    }}
    let value = load(false).await()?
    Ok(value)
  }}
}}
"
        );
        let run = run_entry_of(&source, "main", &[]);
        assert_eq!(run.output, "");
        assert_eq!(run.value().to_string(), "Err(boom)");
    }

    #[test]
    fn a_task_that_fails_propagates_its_error_out_of_the_scope() {
        let source = format!(
            "{TASKS}
export fn main() -> Result<Unit, Error> {{
  scope tasks {{
    let failing = tasks.spawn {{ Err(Error(\"the task failed\")) }}
    println(\"the body finished\")?
  }}
  println(\"unreachable\")?
  Ok(())
}}
"
        );
        let run = run_entry_of(&source, "main", &[]);
        assert_eq!(run.output, "the body finished\n");
        assert_eq!(run.value().to_string(), "Err(the task failed)");
    }

    #[test]
    fn awaiting_a_cancelled_task_is_rejected() {
        let run = run_task_body(&format!(
            "  scope tasks {{\n    let timer = tasks.spawn {{\n{SPINNING_TASK}\n    }}\n    timer.cancel()\n    let value = await timer\n  }}"
        ));
        assert_eq!(run.output, "");
        let error = run.error();
        assert!(error.message.contains("was cancelled"), "{}", error.message);
        assert!(error.rule.unwrap().contains("waits for or cancels"));
    }

    #[test]
    fn awaiting_the_same_handle_twice_runs_the_body_once() {
        let run = run_task_body(
            "  scope tasks {\n    let once = tasks.spawn {\n      println(\"the body ran\")?\n      7\n    }\n    let first = await once\n    let second = await once\n    println(\"{first} {second}\")?\n  }",
        );
        assert_eq!(run.output, "the body ran\n7 7\n");
    }

    #[test]
    fn awaiting_a_value_that_is_not_a_task_is_rejected() {
        let error = run_task_body("  let value = await 1").error();
        assert_eq!(error.message, "`await` needs a task, but found `Int`");
        assert!(error.rule.unwrap().contains("`await` settles a task"));
    }

    // ------------------------------------------------------- task safety

    #[test]
    fn spawning_a_closure_that_captures_a_vector_is_rejected() {
        let source = r#"
export fn main() -> Result<Unit, Error> {
  var items = Vector.of(1, 2)
  scope tasks {
    let counting = tasks.spawn { items.length() }
  }
  Ok(())
}
"#;
        let error = run_entry_of(source, "main", &[]).error();
        assert_eq!(
            error.message,
            "`spawn` cannot capture `items`, which is a `Vector`"
        );
        assert!(error
            .rule
            .unwrap()
            .contains("A vector cannot cross, even through `let`"));
        let help = error.help.unwrap();
        assert!(
            help.contains("freeze()") && help.contains("toArray()"),
            "{help}"
        );
    }

    #[test]
    fn spawning_a_closure_that_captures_the_frozen_array_is_accepted() {
        let source = r#"
use console.println

export fn main() -> Result<Unit, Error> {
  var items = Vector.of(1, 2)
  let frozen = items.freeze()
  scope tasks {
    let counting = tasks.spawn { frozen.length() }
    let total = await counting
    println("{total}")?
  }
  Ok(())
}
"#;
        assert_eq!(run_entry_of(source, "main", &[]).output, "2\n");
    }

    #[test]
    fn task_safety_names_the_field_that_cannot_cross() {
        let source = r#"
struct Draft {
  guests: Vector<String>
}

export fn main() -> Result<Unit, Error> {
  let draft = Draft(guests: Vector.of("Alice"))
  scope tasks {
    let counting = tasks.spawn { draft.guests.length() }
  }
  Ok(())
}
"#;
        let error = run_entry_of(source, "main", &[]).error();
        assert_eq!(
            error.message,
            "`spawn` cannot capture `draft.guests`, which is a `Vector`"
        );
    }

    #[test]
    fn a_closure_is_task_safe_only_when_every_capture_is() {
        let source = r#"
export fn main() -> Result<Unit, Error> {
  var seen = Vector.of(1)
  let count = fn() {
    seen.length()
  }
  scope tasks {
    let counting = tasks.spawn { count() }
  }
  Ok(())
}
"#;
        let error = run_entry_of(source, "main", &[]).error();
        assert_eq!(
            error.message,
            "`spawn` cannot capture `count -> seen`, which is a `Vector`"
        );
    }

    // --------------------------------------------------- real concurrency

    /// Runs `source`'s `main` with `console` and a real `clock` granted, and
    /// reports how long the whole run took.
    fn run_timed(source: &str) -> (Run, Duration) {
        let (sources, program) = program_of(source);
        let buffer = Buffer::default();
        let mut hosts = HostRegistry::new(Grants::new(["console", "clock"]));
        hosts.register(Box::new(Console::new(buffer.clone())));
        hosts.register(Box::new(crate::clock::Clock::real()));
        let runtime = Runtime::new(program, sources, Arc::new(hosts));
        let started = Instant::now();
        let value = Interpreter::new(&runtime).run_entry("test", "main", Vec::new());
        let elapsed = started.elapsed();
        (
            Run {
                value,
                output: buffer.text(),
            },
            elapsed,
        )
    }

    /// Collects every event a run traced, for assertions.
    #[derive(Clone, Default)]
    struct RecordingSink(Arc<Mutex<Vec<TraceEvent>>>);

    impl RecordingSink {
        fn events(&self) -> Vec<TraceEvent> {
            self.0.lock().expect("no test panics while tracing").clone()
        }
    }

    impl crate::trace::TraceSink for RecordingSink {
        fn record(&self, event: TraceEvent) {
            self.0
                .lock()
                .expect("no test panics while tracing")
                .push(event);
        }
    }

    /// Runs `source`'s `main` with `console` and a real `clock` granted,
    /// reporting what it traced and how long it took.
    fn run_traced(source: &str) -> (Run, Vec<TraceEvent>, Duration) {
        let (sources, program) = program_of(source);
        let buffer = Buffer::default();
        let sink = RecordingSink::default();
        let mut hosts = HostRegistry::new(Grants::new(["console", "clock"]));
        hosts.register(Box::new(Console::new(buffer.clone())));
        hosts.register(Box::new(crate::clock::Clock::real()));
        hosts.set_trace(Arc::new(sink.clone()));
        let runtime =
            Runtime::new(program, sources, Arc::new(hosts)).with_trace(Arc::new(sink.clone()));
        let started = Instant::now();
        let value = Interpreter::new(&runtime).run_entry("test", "main", Vec::new());
        let elapsed = started.elapsed();
        (
            Run {
                value,
                output: buffer.text(),
            },
            sink.events(),
            elapsed,
        )
    }

    #[test]
    fn a_task_can_spawn_tasks_of_its_own() {
        let run = run_task_body(
            "  scope outer {\n    let parent = outer.spawn {\n      scope inner {\n        let a = inner.spawn { 1 }\n        let b = inner.spawn { 2 }\n        await a + await b\n      }\n    }\n    println(\"{await parent}\")?\n  }",
        );
        assert_eq!(run.output, "3\n");
    }

    /// A value leaving a task crosses the same boundary its body crossed to
    /// get there, so it answers to the same rule: a vector cannot come back
    /// out of a task any more than it could go in.
    #[test]
    fn a_task_cannot_produce_a_value_that_may_not_cross() {
        let source = r#"
export fn main() -> Result<Unit, Error> {
  scope tasks {
    let building = tasks.spawn { Vector.of(1, 2) }
    let items = await building
  }
  Ok(())
}
"#;
        let error = run_entry_of(source, "main", &[]).error();
        assert_eq!(
            error.message,
            "this task produced a `Vector`, which cannot leave a task"
        );
    }

    /// The success criterion itself, read off a trace: each task's wait is
    /// attributed to that task, and the waits add up to more than the run
    /// took, which is only possible if they happened at the same time.
    #[test]
    fn a_trace_attributes_each_task_s_wait_to_that_task() {
        let source = r#"
use clock.sleep

export fn main() -> Result<Unit, Error> {
  scope waits {
    let first = waits.spawn { sleep(300ms) }
    let second = waits.spawn { sleep(300ms) }
    await first
    await second
  }
  Ok(())
}
"#;
        let (run, events, elapsed) = run_traced(source);
        run.value();

        // Every one of these events was produced on a task's own thread and
        // written by the sink the run shares, so reading them back is also
        // the evidence that an event, and the values it carries, may cross a
        // task boundary.
        let sleeps: Vec<&TraceEvent> = events
            .iter()
            .filter(|event| matches!(event, TraceEvent::HostCall { op, .. } if op == "sleep"))
            .collect();
        assert_eq!(sleeps.len(), 2);
        for event in &sleeps {
            let TraceEvent::HostCall { args, .. } = event else {
                unreachable!("filtered to host calls")
            };
            assert_eq!(args.len(), 1);
            assert_eq!(
                crate::trace::value_to_json(
                    &match &args[0] {
                        crate::trace::RecordedValue::Carried(transfer) =>
                            transfer.clone().into_value(),
                        other => panic!("expected a carried duration, found {other:?}"),
                    },
                    crate::trace::ValueCapture::Full
                ),
                r#"{"type":"duration","ns":300000000}"#
            );
        }
        let waited: Duration = sleeps
            .iter()
            .filter_map(|event| match event {
                TraceEvent::HostCall { wait, .. } => Some(*wait),
                _ => None,
            })
            .sum();
        assert!(
            waited > elapsed,
            "the two waits total {waited:?}, which is not more than the {elapsed:?} the run took"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, TraceEvent::TaskCompleted { .. }))
                .count(),
            2
        );
    }

    /// Cancellation reaches a task that is already running: it stops at its
    /// next safepoint, and the trace says it was cancelled rather than that
    /// it completed.
    #[test]
    fn cancelling_a_running_task_stops_it_and_traces_it() {
        let source = format!(
            "{TASKS}
export fn main() -> Result<Unit, Error> {{
  scope tasks {{
    let ignored = tasks.spawn {{
{SPINNING_TASK}
    }}
    return Ok(())
  }}
}}
"
        );
        let (run, events, _) = run_traced(&source);
        assert_eq!(run.output, "");
        assert!(events
            .iter()
            .any(|event| matches!(event, TraceEvent::TaskCancelled { id: 1 })));
        assert!(!events
            .iter()
            .any(|event| matches!(event, TraceEvent::TaskCompleted { .. })));
    }

    /// ADR 0001 lists "CPU time and I/O wait are accurately attributable in
    /// traces" as a success criterion, and ADR 0003 records that phase 1
    /// could not validate it, because with one task running at a time nothing
    /// overlapped. This is the smallest observation that it now does: two
    /// tasks that each wait 300ms finish in about 300ms, not 600ms.
    #[test]
    fn two_tasks_wait_at_the_same_time() {
        let source = r#"
use clock.sleep

export fn main() -> Result<Unit, Error> {
  scope waits {
    let first = waits.spawn { sleep(300ms) }
    let second = waits.spawn { sleep(300ms) }
    await first
    await second
  }
  Ok(())
}
"#;
        let (run, elapsed) = run_timed(source);
        run.value();
        assert!(
            elapsed >= Duration::from_millis(250),
            "both tasks really waited, but the run took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(550),
            "the waits overlapped, but the run took {elapsed:?}, which is closer to their sum"
        );
    }

    #[test]
    fn a_scope_with_two_tasks_produces_both_values() {
        let run = run_task_body(
            "  scope tasks {\n    let first = tasks.spawn { 1 }\n    let second = tasks.spawn { 2 }\n    println(\"{await first} {await second}\")?\n  }",
        );
        assert_eq!(run.output, "1 2\n");
    }

    /// A task draws its fuel from the run's budget, so exhausting it inside a
    /// task stops the run exactly as exhausting it in the entry would.
    #[test]
    fn a_budget_exhausted_inside_a_task_stops_the_run() {
        let source = r#"
export fn main() -> Result<Unit, Error> {
  scope tasks {
    let spinning = tasks.spawn {
      var i = 0
      while i < 1000000000 {
        i += 1
      }
      i
    }
    await spinning
  }
  Ok(())
}
"#;
        let (sources, program) = program_of(source);
        let mut hosts = HostRegistry::new(Grants::new(["console"]));
        hosts.register(Box::new(Console::new(Buffer::default())));
        hosts.set_budget(Budget::new(Limits {
            fuel: Some(10_000),
            ..Limits::default()
        }));
        let runtime = Runtime::new(program, sources, Arc::new(hosts));
        let error = Interpreter::new(&runtime)
            .run_entry("test", "main", Vec::new())
            .expect_err("the fuel budget stops the run");
        assert!(error.message.contains("fuel budget"), "{}", error.message);
        assert!(
            runtime
                .hosts()
                .with_budget(|budget| budget.fuel_spent())
                .unwrap_or_default()
                >= 10_000
        );
    }

    // ---------------------------------------------------------- `Shared`

    /// Mutable state of the kind the Language Card says belongs in a
    /// `Shared`.
    const METRICS: &str = r#"
use console.println

struct Metrics {
  requests: Int
  failures: Int
}

impl Metrics {
  /// Records one completed request.
  fn record(var self, failed: Bool) {
    self.requests += 1
    if failed {
      self.failures += 1
    }
  }
}
"#;

    /// Runs `body` inside a `main` with [`METRICS`] in scope.
    fn run_shared_body(body: &str) -> Run {
        run_entry_of(
            &format!(
                "{METRICS}\nexport fn main() -> Result<Unit, Error> {{\n{body}\n  Ok(())\n}}\n"
            ),
            "main",
            &[],
        )
    }

    #[test]
    fn a_lock_gives_a_var_alias_to_the_wrapped_value() {
        let run = run_shared_body(
            "  let metrics = Shared(Metrics(requests: 0, failures: 0))\n  metrics.lock(fn(var value) {\n    value.record(true)\n    value.record(false)\n  })\n  metrics.lock(fn(value) {\n    println(\"{value.requests} {value.failures}\")\n  })?",
        );
        assert_eq!(run.output, "2 1\n");
    }

    #[test]
    fn a_lock_produces_the_value_its_closure_produces() {
        let run = run_shared_body(
            "  let metrics = Shared(Metrics(requests: 4, failures: 1))\n  let doubled = metrics.lock(fn(var value) {\n    value.requests = value.requests * 2\n    value.requests\n  })\n  println(\"{doubled}\")?",
        );
        assert_eq!(run.output, "8\n");
    }

    /// A closure that does not declare `var` receives a copy, exactly as an
    /// ordinary parameter does anywhere else in the language: it can read the
    /// wrapped value, and the `var self` method that would change it is
    /// refused, because a copy is not the place the value lives in.
    #[test]
    fn a_lock_closure_without_var_receives_a_read_only_copy() {
        let run = run_shared_body(
            "  let metrics = Shared(Metrics(requests: 1, failures: 0))\n  metrics.lock(fn(value) {\n    println(\"{value.requests}\")\n  })?",
        );
        assert_eq!(run.output, "1\n");

        let error = run_shared_body(
            "  let metrics = Shared(Metrics(requests: 1, failures: 0))\n  metrics.lock(fn(value) {\n    value.record(true)\n  })",
        )
        .error();
        assert_eq!(
            error.message,
            "`record` takes a `var self` receiver, but `value` is a read-only place"
        );
    }

    /// The whole reason the type exists: a `Shared` crosses a task boundary
    /// by sharing rather than by copying, so every task sees one value, and
    /// `lock` is what keeps their read-modify-writes from racing.
    #[test]
    fn tasks_share_one_value_through_a_shared() {
        let source = format!(
            "{METRICS}
export fn main() -> Result<Unit, Error> {{
  let metrics = Shared(Metrics(requests: 0, failures: 0))
  scope requests {{
    let first = requests.spawn {{
      for i in 0..<100 {{
        metrics.lock(fn(var value) {{ value.record(false) }})
      }}
    }}
    let second = requests.spawn {{
      for i in 0..<100 {{
        metrics.lock(fn(var value) {{ value.record(true) }})
      }}
    }}
    await first
    await second
  }}
  metrics.lock(fn(value) {{
    println(\"{{value.requests}} {{value.failures}}\")
  }})?
  Ok(())
}}
"
        );
        let run = run_entry_of(&source, "main", &[]);
        assert_eq!(run.output, "200 100\n");
    }

    #[test]
    fn a_shared_refuses_a_payload_that_cannot_cross_a_task_boundary() {
        let error = run_shared_body("  let counts = Shared(Vector.of(1, 2))").error();
        assert_eq!(
            error.message,
            "`Shared` cannot wrap a `Vector`, which cannot cross a task boundary"
        );
        assert!(error
            .rule
            .unwrap()
            .contains("A vector cannot cross, even through `let`"));
    }

    #[test]
    fn a_shared_refuses_a_struct_holding_a_vector() {
        let source = r#"
struct Draft {
  guests: Vector<String>
}

export fn main() -> Result<Unit, Error> {
  let draft = Shared(Draft(guests: Vector.of("Alice")))
  Ok(())
}
"#;
        let error = run_entry_of(source, "main", &[]).error();
        assert_eq!(
            error.message,
            "`Shared` cannot wrap a `Vector` in `guests`, which cannot cross a task boundary"
        );
    }

    /// A `lock` inside a `lock` on the same value can never be granted, so
    /// the runtime says so rather than waiting for itself for ever.
    #[test]
    fn a_reentrant_lock_is_reported_rather_than_deadlocking() {
        let error = run_shared_body(
            "  let metrics = Shared(Metrics(requests: 0, failures: 0))\n  metrics.lock(fn(var value) {\n    metrics.lock(fn(var inner) {\n      inner.record(false)\n    })\n  })",
        )
        .error();
        assert_eq!(
            error.message,
            "this task already holds this `Shared`, so `lock` would wait for itself"
        );
        assert!(error.help.unwrap().contains("one `lock`"));
    }

    /// Two different `Shared` values are two different locks, so holding one
    /// while taking the other is ordinary nesting rather than a deadlock.
    #[test]
    fn a_lock_inside_a_lock_on_another_shared_is_allowed() {
        let run = run_shared_body(
            "  let left = Shared(Metrics(requests: 1, failures: 0))\n  let right = Shared(Metrics(requests: 2, failures: 0))\n  let total = left.lock(fn(value) {\n    right.lock(fn(other) {\n      value.requests + other.requests\n    })\n  })\n  println(\"{total}\")?",
        );
        assert_eq!(run.output, "3\n");
    }

    #[test]
    fn a_shared_has_no_operation_but_lock() {
        let error =
            run_shared_body("  let metrics = Shared(Metrics(requests: 0, failures: 0))\n  let value = metrics.get()")
                .error();
        assert_eq!(error.message, "`Shared` has no method `get`");
        assert!(error
            .rule
            .unwrap()
            .contains("there is no `get` and no `set`"));
    }

    // ------------------------------------------------- acceptance tests

    fn examples_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
    }

    /// Loads the repository's real `examples/` package.
    fn examples_program() -> (Arc<SourceMap>, Arc<Program>) {
        let root = examples_root();
        let mut sources = SourceMap::new();
        let package = cove_sema::package::load(&root, &mut sources).expect("examples load");
        let program = cove_sema::resolve::resolve(&package).expect("examples resolve");
        (Arc::new(sources), Arc::new(program))
    }

    #[test]
    fn runs_the_hello_example() {
        let (sources, program) = examples_program();
        let default = run_in(
            &program,
            &sources,
            "hello",
            "main",
            &[],
            &["console"],
            BTreeMap::new(),
        );
        assert_eq!(default.output, "Hello, world!\n");
        assert_eq!(default.value().to_string(), "Ok(())");

        let named = run_in(
            &program,
            &sources,
            "hello",
            "main",
            &["Cove"],
            &["console"],
            BTreeMap::new(),
        );
        assert_eq!(named.output, "Hello, Cove!\n");
    }

    #[test]
    fn runs_the_values_example() {
        let (sources, program) = examples_program();
        let run = run_in(
            &program,
            &sources,
            "values",
            "main",
            &[],
            &["console"],
            BTreeMap::new(),
        );
        assert_eq!(run.output, "Pending\nConfirmed\n2\n2\n2\n1\n");
        assert_eq!(run.value().to_string(), "Ok(())");
    }

    #[test]
    fn runs_the_config_example() {
        let (sources, program) = examples_program();

        let loaded = run_in(
            &program,
            &sources,
            "config",
            "loadConfig",
            &[],
            &["env"],
            BTreeMap::from([
                ("PORT".to_string(), "9000".to_string()),
                ("LOG_LEVEL".to_string(), "debug".to_string()),
            ]),
        );
        assert_eq!(
            loaded.value().to_string(),
            "Ok(Config(port: 9000, logLevel: Debug))"
        );

        let defaulted = run_in(
            &program,
            &sources,
            "config",
            "loadConfig",
            &[],
            &["env"],
            BTreeMap::new(),
        );
        assert_eq!(
            defaulted.value().to_string(),
            "Ok(Config(port: 8080, logLevel: Info))"
        );

        let rejected = run_in(
            &program,
            &sources,
            "config",
            "loadConfig",
            &[],
            &["env"],
            BTreeMap::from([("LOG_LEVEL".to_string(), "verbose".to_string())]),
        );
        assert_eq!(
            rejected.value().to_string(),
            "Err(InvalidLogLevel(verbose))"
        );

        let invalid_port = run_in(
            &program,
            &sources,
            "config",
            "loadConfig",
            &[],
            &["env"],
            BTreeMap::from([("PORT".to_string(), "eighty".to_string())]),
        );
        assert_eq!(invalid_port.value().to_string(), "Err(InvalidPort(eighty))");
    }

    #[test]
    fn runs_the_restricted_example() {
        let (sources, program) = examples_program();

        let buffer = Buffer::default();
        let mut hosts = HostRegistry::new(Grants::new(["documents", "console"]));
        hosts.register(Box::new(Console::new(buffer.clone())));
        hosts.register(Box::new(Documents::rooted(
            examples_root().join("documents"),
        )));
        let runtime = Runtime::new(program, sources, Arc::new(hosts));
        let value = Interpreter::new(&runtime)
            .run_entry("restricted", "main", Vec::new())
            .expect("the program ran without a runtime error");

        assert_eq!(buffer.text(), "5 words\n");
        assert_eq!(value.to_string(), "Ok(())");
    }
}
