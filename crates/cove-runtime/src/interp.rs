//! The MVP tree-walking interpreter.
//!
//! The interpreter is an ordinary evaluator over [`cove_syntax::ast`] plus the
//! four rules that make Cove Cove:
//!
//! - assignment and ordinary argument passing clone a [`Value`], and `Clone`
//!   already encodes field-wise shallow copy, so there is no deep-copy path;
//! - `let` binds a read-only place and `var` a mutable one, so mutation always
//!   resolves an lvalue down to a slot the caller owns;
//! - `var self` and `var` parameters bind the caller's place instead of a copy;
//! - Host API calls go through [`HostRegistry::call`], which enforces grants.
//!
//! Static checking (types, exhaustiveness, uniqueness) is future work; the
//! interpreter enforces the same rules dynamically and says which rule it
//! enforced.

use std::cell::RefCell;
use std::rc::Rc;

use cove_diag::{SourceMap, Span};
use cove_sema::resolve::{Program, ResolvedModule};
use cove_syntax::ast::{
    Arg, BinaryOp, Block, EnumDecl, Expr, ExprKind, FnDecl, ItemKind, Param, Pattern, PatternKind,
    Receiver, StmtKind, StrPart, StructDecl, UnaryOp,
};

use crate::builtins::{self, Callable};
use crate::error::RuntimeError;
use crate::host::HostRegistry;
use crate::value::{Closure, EnumValue, StructValue, Value};

/// How deep Cove calls may nest before the runtime reports a limit instead of
/// exhausting the host stack.
const MAX_CALL_DEPTH: usize = 256;

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

    /// Every binding a closure body could see, read by value at creation time.
    fn captures(&self, span: Span) -> Result<Vec<(Rc<str>, Value)>, RuntimeError> {
        let mut captured: Vec<(Rc<str>, Value)> = Vec::new();
        for scope in &self.scopes {
            for (name, place) in scope {
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
pub struct Interpreter<'a> {
    pub program: &'a Program,
    pub sources: &'a SourceMap,
    pub hosts: &'a mut HostRegistry,
    depth: usize,
}

impl<'a> Interpreter<'a> {
    pub fn new(program: &'a Program, sources: &'a SourceMap, hosts: &'a mut HostRegistry) -> Self {
        Interpreter {
            program,
            sources,
            hosts,
            depth: 0,
        }
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

        self.invoke(
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

    // ---------------------------------------------------------------- calls

    fn invoke(
        &mut self,
        target: &Target<'_>,
        receiver: Option<ArgSlot>,
        args: Vec<EvaluatedArg>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if target.is_async {
            return Err(unsupported("calling an `async fn`", span));
        }
        if self.depth >= MAX_CALL_DEPTH {
            return Err(RuntimeError::new(format!(
                "call depth limit of {MAX_CALL_DEPTH} reached while calling `{}`",
                target.name
            ))
            .at(span)
            .with_rule("Recursion depth is a runtime control, not a proof obligation."));
        }
        self.depth += 1;
        let result = self.invoke_body(target, receiver, args, span);
        self.depth -= 1;
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
                self.hosts
                    .call(&module, &op, values)
                    .map_err(|e| e.at(span))
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
                    other => Err(RuntimeError::new(format!(
                        "`?` needs a `Result` or an `Option`, but found `{}`",
                        other.type_name()
                    ))
                    .at(span)
                    .with_rule("`expr?` returns the error from the current function.")
                    .into()),
                }
            }
            ExprKind::Await(_) => Err(unsupported("`await`", span).into()),
            ExprKind::Scope { .. } => Err(unsupported("a `scope` block", span).into()),
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
            ExprKind::Range { .. } => Err(RuntimeError::new(
                "a range is only usable as the iterable of a `for` loop in the MVP",
            )
            .at(span)
            .into()),
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
        let captures = env.captures(span)?;
        Ok(Value::Closure(Rc::new(Closure {
            is_async,
            params,
            decl: None,
            body: Rc::new(body),
            module: env.module.clone(),
            captures,
        })))
    }

    fn iterable_items(&mut self, env: &mut Env, expr: &Expr) -> Result<Vec<Value>, Control> {
        if let ExprKind::Range {
            start,
            end,
            inclusive_end,
        } = &expr.kind
        {
            let start = expect_int(self.eval(env, start)?, "a range bound", expr.span)?;
            let end = expect_int(self.eval(env, end)?, "a range bound", expr.span)?;
            let end = if *inclusive_end { end + 1 } else { end };
            return Ok((start..end).map(Value::Int).collect());
        }
        // Iteration reads a snapshot of the elements; rejecting structural
        // mutation during iteration is future work.
        match self.eval(env, expr)? {
            Value::Array(items) => Ok(items.iter().cloned().collect()),
            Value::Vector(storage) => Ok(storage.elements.borrow().clone()),
            other => Err(RuntimeError::new(format!(
                "`for` iterates an `Array`, a `Vector`, or a range, but found `{}`",
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
                "enum `{}` has no case `{case}`",
                decl.name.node
            ))
            .at(span)
            .with_help(format!(
                "known cases: {}",
                decl.cases
                    .iter()
                    .map(|c| c.name.node.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
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
                    return Ok(self
                        .hosts
                        .call(&host, name, values)
                        .map_err(|e| e.at(span))?);
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
                            return Ok(self
                                .hosts
                                .call(head, &name.node, values)
                                .map_err(|e| e.at(span))?);
                        }
                        if let Some(decl) = self.find_enum(&module, head) {
                            let args = self.eval_args(env, args, trailing)?;
                            let values = plain_values(args, &format!("{head}.{}", name.node))?;
                            return Ok(self.enum_case(&module, &decl, &name.node, values, span)?);
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

        // `tasks.spawn { ... }` writes the await as a postfix call.
        if name == "await" {
            return Err(unsupported("`await`", span).into());
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
                slots[index] = Some(arg);
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

// ------------------------------------------------------------ diagnostics

fn unsupported(what: &str, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "{what} is not implemented yet in the MVP interpreter"
    ))
    .at(span)
    .with_rule("The MVP interpreter runs the synchronous subset of Cove.")
    .with_help("asynchronous functions, `await`, `scope`, and `spawn` are not available yet")
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
        let body = "  let items = [10, 20]\n  console.println(\"{items.get(0).unwrapOr(0)} {items.get(5).isNone()} {items.count()} {items.isEmpty()}\")?\n  console.println(\"{\"a bc  d\".words().length()} {\"abc\".length()} {\"\".isEmpty()}\")?";
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

    #[test]
    fn unsupported_constructs_say_so_plainly() {
        let source = r#"
export async fn main() -> Result<Unit, Error> {
  Ok(())
}
"#;
        let error = run_entry_of(source, "main", &[]).error();
        assert_eq!(
            error.message,
            "calling an `async fn` is not implemented yet in the MVP interpreter"
        );

        let awaiting = r#"
fn ready() -> Int {
  1
}

export fn main() -> Result<Unit, Error> {
  let value = await ready()
  Ok(())
}
"#;
        let error = run_entry_of(awaiting, "main", &[]).error();
        assert!(
            error.message.contains("not implemented yet"),
            "{}",
            error.message
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
