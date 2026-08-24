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
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;
use std::time::Instant;

use cove_diag::{SourceMap, Span};
use cove_sema::resolve::{Program, ResolvedModule};
use cove_syntax::ast::{
    Arg, BinaryOp, Block, EnumDecl, Expr, ExprKind, FnDecl, Ident, ItemKind, Param, Pattern,
    PatternKind, Receiver, StmtKind, StrPart, StructDecl, UnaryOp,
};

use crate::builtins::{self, Callable};
use crate::error::RuntimeError;
use crate::host::HostRegistry;
use crate::task::{self, Task, TaskScope, TaskState};
use crate::trace::{NullSink, Timing, TraceEvent, TraceSink};
use crate::value::{Closure, EnumValue, RangeBounds, StructValue, Value};

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
}

impl From<RuntimeError> for Control {
    fn from(error: RuntimeError) -> Self {
        Control::Error(error)
    }
}

type Eval = Result<Value, Control>;

/// Converts a completed call back into an ordinary result.
fn finish(result: Eval) -> Result<Value, RuntimeError> {
    match result {
        Ok(value) => Ok(value),
        Err(Control::Return(value)) => Ok(value),
        Err(Control::Error(error)) => Err(error),
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
}

/// Executes a resolved program.
///
/// # Ownership of the run's [`crate::budget::Budget`]
///
/// The `Budget` is owned by the [`HostRegistry`] this interpreter borrows,
/// not by `Interpreter` itself: a host installs it once with
/// `HostRegistry::set_budget`, and the interpreter reaches it through
/// `self.hosts.budget_mut()` at every safepoint. There is exactly one
/// `Budget` per run either way, so this is a choice of which existing owner
/// keeps it, not a second copy — fuel, call depth, and host-call counters are
/// each charged from exactly one place.
pub struct Interpreter<'a> {
    pub program: &'a Program,
    pub sources: &'a SourceMap,
    pub hosts: &'a mut HostRegistry,
    depth: usize,
    trace: Box<dyn TraceSink>,
    /// The next id [`Interpreter::spawn`] assigns to a spawned task. Task ids
    /// are a trace identity, unrelated to a task's spawn-order `position`.
    next_task_id: u64,
    /// Maps a task's address (stable for the `Rc<Task>`'s lifetime) to the id
    /// assigned when it was spawned, so `settle` and cancellation can trace
    /// the same id `spawn` announced.
    task_ids: HashMap<usize, u64>,
    /// Ids of the tasks whose bodies are currently running, innermost last,
    /// so a nested `spawn` can name its immediate parent.
    task_stack: Vec<u64>,
    /// Active timing contexts, one for the entry and one more for each task
    /// currently running its body. A host call's wait is charged against
    /// every context on this stack, so both the task and the entry that
    /// (directly or transitively) awaits it see the same wait.
    timings: Vec<Timing>,
}

impl<'a> Interpreter<'a> {
    pub fn new(program: &'a Program, sources: &'a SourceMap, hosts: &'a mut HostRegistry) -> Self {
        Interpreter {
            program,
            sources,
            hosts,
            depth: 0,
            trace: Box::new(NullSink),
            next_task_id: 1,
            task_ids: HashMap::new(),
            task_stack: Vec::new(),
            timings: Vec::new(),
        }
    }

    /// Installs where trace events go. Replaces any sink installed earlier;
    /// the default is [`NullSink`], which discards everything.
    pub fn set_trace(&mut self, sink: Box<dyn TraceSink>) {
        self.trace = sink;
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

        self.trace.record(TraceEvent::EntryEnter {
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
        self.trace.record(TraceEvent::EntryExit {
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

    fn find_function(&self, module: &str, name: &str) -> Option<Rc<FnDecl>> {
        Some(self.resolved(module)?.functions.get(name)?.decl.clone())
    }

    fn find_method(&self, module: &str, type_name: &str, name: &str) -> Option<Rc<FnDecl>> {
        Some(
            self.resolved(module)?
                .methods
                .get(&(type_name.to_string(), name.to_string()))?
                .decl
                .clone(),
        )
    }

    fn find_struct(&self, module: &str, name: &str) -> Option<Rc<StructDecl>> {
        Some(self.resolved(module)?.structs.get(name)?.decl.clone())
    }

    fn find_enum(&self, module: &str, name: &str) -> Option<Rc<EnumDecl>> {
        Some(self.resolved(module)?.enums.get(name)?.decl.clone())
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
        if let Some(budget) = self.hosts.budget_mut() {
            if let Err(stopped) = budget.safepoint(SAFEPOINT_FUEL) {
                return Err(budget.to_runtime_error(stopped).at(span));
            }
        }
        Ok(())
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
        let wait = started.elapsed();
        for timing in &mut self.timings {
            timing.add_wait(wait);
        }
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

        // Every call is a safepoint: `enter_call` bounds recursion against a
        // host-configured `max_call_depth` (in addition to the unconditional
        // `MAX_CALL_DEPTH` above), and the fuel charge counts the call itself.
        // Both are undone on every path out of this call, including the error
        // path from the fuel charge, so depth never leaks.
        if let Some(budget) = self.hosts.budget_mut() {
            if let Err(stopped) = budget.enter_call() {
                return Err(budget.to_runtime_error(stopped).at(span));
            }
        }
        if let Err(error) = self.charge_safepoint(span) {
            if let Some(budget) = self.hosts.budget_mut() {
                budget.leave_call();
            }
            return Err(error);
        }

        self.depth += 1;
        let result = self.invoke_body(target, receiver, args, span);
        self.depth -= 1;
        if let Some(budget) = self.hosts.budget_mut() {
            budget.leave_call();
        }
        if target.is_async {
            // An `async fn` is called like any other function and produces a
            // task, so its value is reachable only through `await`.
            //
            // ADR 0003 phase 1 runs the body here, at the call, and returns a
            // handle that is already settled. A scheduler is free to start it
            // anywhere between the call and the `await`, so nothing may depend
            // on when the body ran; a body that is never awaited has still run.
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
        finish(self.eval_block(&mut env, target.body))
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
                        env.declare(name, Place::binding(value, false));
                    }
                },
                None => match &param.default {
                    // Default arguments are evaluated by the callee.
                    Some(default) => {
                        let value = finish(self.eval(env, default))?;
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
                    ty: _,
                    value,
                } => {
                    let value = self.eval(env, value)?;
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
            ExprKind::Scope { name, body } => self.eval_scope(env, name, body, span),
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
                for item in items {
                    // Once per iteration, at the back edge: this is the
                    // safepoint that bounds a `for` over an unbounded
                    // iterable, since Cove does not prove termination.
                    self.charge_safepoint(span)?;
                    env.push();
                    env.declare(binding.node.as_str().into(), Place::binding(item, false));
                    let result = self.eval_block(env, body);
                    env.pop();
                    result?;
                }
                Ok(Value::Unit)
            }
            ExprKind::While { condition, body } => {
                loop {
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
                        break;
                    }
                    // Once per iteration, at the back edge: this is the
                    // safepoint that bounds a non-terminating `while`, which
                    // is otherwise unbounded by anything the type system
                    // proves.
                    self.charge_safepoint(span)?;
                    self.eval_block(env, body)?;
                }
                Ok(Value::Unit)
            }
            ExprKind::Return(value) => {
                let value = match value {
                    Some(expr) => self.eval(env, expr)?,
                    None => Value::Unit,
                };
                Err(Control::Return(value))
            }
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
            body: Rc::new(body),
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
    fn eval_scope(&mut self, env: &mut Env, name: &Ident, body: &Block, span: Span) -> Eval {
        let scope = TaskScope::new(name.node.as_str().into());
        env.push();
        env.declare(
            name.node.as_str().into(),
            Place::binding(Value::TaskScope(scope.clone()), false),
        );
        let result = self.eval_block(env, body);
        env.pop();
        let left = self.leave_scope(&scope, result, span);
        scope.close();
        left
    }

    /// Waits for or cancels the children of a scope that is being left.
    ///
    /// A normal exit settles every task the body did not await, in spawn
    /// order, and discards its value: a scope waits for its children, it does
    /// not collect them. A task that fails is not swallowed — a `RuntimeError`
    /// propagates as itself, and a task whose value is `Err(error)` returns
    /// that error from the enclosing function, exactly as `?` would. Either
    /// way the tasks that have not run are cancelled, as they are when the
    /// body itself leaves early through `return`, `?`, or an error.
    ///
    /// ADR 0003: spawn order is phase 1's choice, not the language's. A
    /// scheduler may settle unawaited children in any order, or have already
    /// settled them before the body finished, so only the set of effects a
    /// scope produces is defined, never their sequence.
    fn leave_scope(&mut self, scope: &Rc<TaskScope>, result: Eval, span: Span) -> Eval {
        let value = match result {
            Ok(value) => value,
            early => {
                self.cancel_scope(scope);
                return early;
            }
        };

        // Settling reads the scope's children by index rather than from a
        // snapshot, so a scope that grows while it is being left is still
        // settled to the end.
        let mut index = 0;
        while let Some(task) = scope.task_at(index) {
            index += 1;
            if !task.is_pending() {
                continue;
            }
            match self.settle(&task, span) {
                Ok(settled) => {
                    if let Some(error) = failure_of(&settled) {
                        self.cancel_scope(scope);
                        return Err(Control::Return(Value::err(error)));
                    }
                }
                Err(error) => {
                    self.cancel_scope(scope);
                    return Err(Control::Error(error));
                }
            }
        }
        Ok(value)
    }

    /// Cancels every pending child of `scope`, the hook point for
    /// `TaskCancelled`.
    ///
    /// `TaskScope::cancel_pending` lives in `task.rs` and has no tracing of
    /// its own, so this walks the same tasks first to trace exactly the ones
    /// that were pending (and so are the ones cancellation actually stops),
    /// then applies the real cancellation the same way `task.rs` already
    /// does.
    fn cancel_scope(&mut self, scope: &Rc<TaskScope>) {
        let mut index = 0;
        while let Some(task) = scope.task_at(index) {
            index += 1;
            self.trace_cancel_if_pending(&task);
        }
        scope.cancel_pending();
    }

    /// Traces `TaskCancelled` for `task` if it is still pending, i.e. if
    /// cancelling it now would actually stop it rather than being a no-op.
    fn trace_cancel_if_pending(&mut self, task: &Rc<Task>) {
        if task.is_pending() {
            if let Some(&id) = self.task_ids.get(&task_key(task)) {
                self.trace.record(TraceEvent::TaskCancelled { id });
            }
        }
    }

    /// Runs a task's body unless it has already settled, and returns its
    /// value.
    ///
    /// A task's body runs at most once, so awaiting the same handle twice
    /// returns the same value and repeats no effect.
    fn settle(&mut self, task: &Rc<Task>, span: Span) -> Result<Value, RuntimeError> {
        let body = match &*task.state.borrow() {
            TaskState::Settled(value) => return Ok(value.clone()),
            TaskState::Failed(error) => return Err(error.clone()),
            TaskState::Cancelled => return Err(awaiting_a_cancelled_task(task, span)),
            TaskState::Running => return Err(awaiting_a_running_task(task, span)),
            TaskState::Pending => task.body.clone(),
        };
        *task.state.borrow_mut() = TaskState::Running;

        // A task reaching this point was spawned through `Interpreter::spawn`,
        // which is the only caller of `TaskScope::spawn`, so it always has an
        // id here.
        let id = self.task_ids.get(&task_key(task)).copied();
        if let Some(id) = id {
            self.task_stack.push(id);
        }
        self.timings.push(Timing::start());

        let result = self.call_value_slots(body, Vec::new(), span);

        let timing = self
            .timings
            .pop()
            .expect("settle pushes exactly the one timing it pops");
        if let Some(id) = id {
            self.task_stack.pop();
            self.trace.record(TraceEvent::TaskCompleted {
                id,
                cpu: timing.cpu(),
            });
        }

        *task.state.borrow_mut() = match &result {
            Ok(value) => TaskState::Settled(value.clone()),
            Err(error) => TaskState::Failed(error.clone()),
        };
        result
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

    /// `scope.spawn { ... }`.
    ///
    /// The trailing closure is checked against the task-safety rule before the
    /// task exists, so a value that may not cross the boundary is reported at
    /// the `spawn` that would have carried it.
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
        if let Err(found) = task::task_safety("", &body) {
            return Err(RuntimeError::new(format!(
                "`spawn` cannot capture `{}`, which is a `{}`",
                found.path, found.type_name
            ))
            .at(span)
            .with_rule(task::TASK_SAFETY_RULE)
            .with_help(found.help()));
        }
        let task = scope.spawn(body);
        let id = self.next_task_id;
        self.next_task_id += 1;
        self.task_ids.insert(task_key(&task), id);
        self.trace.record(TraceEvent::TaskSpawned {
            id,
            parent: self.task_stack.last().copied(),
            scope: scope.name.to_string(),
        });
        Ok(Value::Task(task))
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
                // Cancelling a task that already ran changes nothing:
                // cancellation stops work that has not happened. Trace only
                // the tasks this call actually cancels.
                self.trace_cancel_if_pending(task);
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
            other => Err(RuntimeError::new(format!(
                "`for` iterates an `Array`, a `Vector`, or a `Range`, but found `{}`",
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
        if let Some(decl) = self.find_function(&module, name) {
            return Ok(Value::Closure(Rc::new(Closure {
                is_async: decl.is_async,
                params: decl.params.clone(),
                body: Rc::new(decl.body.clone()),
                decl: Some(decl),
                module,
                captures: Vec::new(),
            })));
        }
        if self.find_struct(&module, name).is_some() || self.find_enum(&module, name).is_some() {
            return Ok(Value::Type(format!("{module}.{name}").into()));
        }
        if builtins::is_builtin_type(name) {
            return Ok(Value::Type(name.into()));
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
                if let Some(decl) = self.find_enum(&module, head) {
                    return Ok(self.enum_case(&module, &decl, name, Vec::new(), span)?);
                }
                if self.is_host_module(&module, head) {
                    return Ok(Value::HostFn {
                        module: head.as_str().into(),
                        op: name.into(),
                    });
                }
            }
        }

        let base_value = self.eval(env, base)?;
        match &base_value {
            Value::Struct(value) => match value.get(name) {
                Some(field) => Ok(field.clone()),
                None => Err(no_field(&value.type_name, name, span).into()),
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
    fn known_members(&self, module: &str, decl: &Rc<EnumDecl>) -> String {
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
        decl: &Rc<EnumDecl>,
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
                if let Some(decl) = self.find_function(&module, name) {
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
                        },
                        None,
                        args,
                        span,
                    )?);
                }
                if let Some(decl) = self.find_struct(&module, name) {
                    let args = self.eval_args(env, args, trailing)?;
                    return Ok(self.init_struct(&module, &decl, args, span)?);
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
                        if let Some(enum_decl) = self.find_enum(&module, head) {
                            // A case wins over an associated function of the
                            // same name, so naming a case never changes
                            // meaning when an `impl` block is added.
                            let is_case = enum_decl
                                .cases
                                .iter()
                                .any(|case| case.name.node == name.node);
                            if !is_case {
                                if let Some(decl) = self.find_method(&module, head, &name.node) {
                                    let args = self.eval_args(env, args, trailing)?;
                                    return Ok(self.invoke(
                                        &Target {
                                            name: &name.node,
                                            params: &decl.params,
                                            body: &decl.body,
                                            module,
                                            receiver: decl.receiver,
                                            is_async: decl.is_async,
                                            captures: &[],
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
                                self.enum_case(&module, &enum_decl, &name.node, values, span)?
                            );
                        }
                        if self.find_struct(&module, head).is_some() {
                            if let Some(decl) = self.find_method(&module, head, &name.node) {
                                let args = self.eval_args(env, args, trailing)?;
                                return Ok(self.invoke(
                                    &Target {
                                        name: &name.node,
                                        params: &decl.params,
                                        body: &decl.body,
                                        module,
                                        receiver: decl.receiver,
                                        is_async: decl.is_async,
                                        captures: &[],
                                    },
                                    None,
                                    args,
                                    span,
                                )?);
                            }
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
        let temporary = match &place {
            Some(_) => None,
            None => Some(self.eval(env, receiver)?),
        };
        let type_name = match (&place, &temporary) {
            (Some(place), _) => place.with_ref(span, Value::type_name)?,
            (_, Some(value)) => value.type_name(),
            _ => unreachable!("a receiver is either a place or a temporary"),
        };

        if let Some((module, short)) = type_name.rsplit_once('.') {
            if let Some(decl) = self.find_method(module, short, name) {
                let module: Rc<str> = module.into();
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
                    },
                    Some(receiver_slot),
                    args,
                    span,
                )?);
            }
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
        decl: &Rc<StructDecl>,
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
            fields.push((
                field.name.node.as_str().into(),
                value_of(&arg, &field.name.node, arg.span)?,
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
                ItemKind::Struct(_) | ItemKind::Enum(_) | ItemKind::TypeAlias(_) => {}
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
        ExprKind::Return(inner) => {
            if let Some(inner) = inner {
                mention_expr(inner, out);
            }
        }
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
fn task_key(task: &Rc<Task>) -> usize {
    Rc::as_ptr(task) as usize
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

fn awaiting_a_running_task(task: &Task, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "{} is already running, so awaiting it here cannot make progress",
        task.describe()
    ))
    .at(span)
    .with_rule("A task's value is observable only once its body has completed.")
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
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
    }
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

    use cove_sema::config::Config;
    use cove_sema::package::{Module, Package, Unit};

    use crate::host::{Console, Documents, Env as EnvHost, Grants, HostRegistry};

    /// A `console` sink the tests can read back.
    #[derive(Clone, Default)]
    struct Buffer(Rc<RefCell<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).expect("console output is UTF-8")
        }
    }

    /// Parses `source` as the single unit of module `test`.
    fn program_of(source: &str) -> (SourceMap, Program) {
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
        (sources, program)
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
        program: &Program,
        sources: &SourceMap,
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
        let value = Interpreter::new(program, sources, &mut hosts).run_entry(
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

    #[test]
    fn a_match_with_no_matching_arm_is_a_runtime_error() {
        let source = r#"
enum Color {
  Red
  Green
}

export fn main() -> Result<Unit, Error> {
  let color = Color.Green
  let name = match color {
    Color.Red => "red"
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
    // ADR 0003 phase 1 settles tasks sequentially, so these tests assert what
    // `await` and scope exit produce, and never the order in which two
    // independent tasks happen to run. Where phase 1 makes a choice a
    // scheduler could make differently, the test says so.

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

    /// ADR 0003: phase 1 runs an `async fn` body at the call site, so a call
    /// that is never awaited has still run by the time the call returns. A
    /// scheduler may start that body at any point up to the `await` instead,
    /// so the assertion is that the effect happened, not when.
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
        let error = run_task_body("  let value = await load(true)?").error();
        assert_eq!(
            error.message,
            "`?` needs a `Result` or an `Option`, but found `Task`"
        );
        assert!(
            error.help.unwrap().contains("task.await()?"),
            "the diagnostic shows the correction"
        );
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
    fn returning_from_a_scope_cancels_a_task_that_never_ran() {
        let source = format!(
            "{TASKS}
export fn main() -> Result<Unit, Error> {{
  scope tasks {{
    let ignored = tasks.spawn {{ println(\"this must not run\")? }}
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
    fn an_error_inside_a_scope_cancels_a_task_that_never_ran() {
        let source = format!(
            "{TASKS}
export fn main() -> Result<Int, Error> {{
  scope tasks {{
    let ignored = tasks.spawn {{ println(\"this must not run\")? }}
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
        let run = run_task_body(
            "  scope tasks {\n    let timer = tasks.spawn { println(\"this must not run\")? }\n    timer.cancel()\n    let value = await timer\n  }",
        );
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

    // ------------------------------------------------- acceptance tests

    fn examples_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
    }

    /// Loads the repository's real `examples/` package.
    fn examples_program() -> (SourceMap, Program) {
        let root = examples_root();
        let mut sources = SourceMap::new();
        let package = cove_sema::package::load(&root, &mut sources).expect("examples load");
        let program = cove_sema::resolve::resolve(&package).expect("examples resolve");
        (sources, program)
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
        let value = Interpreter::new(&program, &sources, &mut hosts)
            .run_entry("restricted", "main", Vec::new())
            .expect("the program ran without a runtime error");

        assert_eq!(buffer.text(), "5 words\n");
        assert_eq!(value.to_string(), "Ok(())");
    }
}
