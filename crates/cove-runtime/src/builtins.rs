//! Builtin methods, associated functions, and constructors.
//!
//! Everything here is dispatched dynamically on a receiver value, a type name,
//! or a constructor name. The MVP has no method table derived from types yet,
//! so an arity or type mismatch is an ordinary [`RuntimeError`] that names the
//! method it came from.

use std::rc::Rc;

use cove_diag::Span;

use crate::error::RuntimeError;
use crate::value::{RangeBounds, Value, VectorStorage};

/// How the builtins call back into the evaluator.
///
/// Higher-order builtins such as `Result.mapError` invoke a Cove callback, so
/// they need the interpreter that owns the call stack.
pub trait Callable {
    /// Calls a closure value with already evaluated arguments.
    fn call_value(
        &mut self,
        callee: &Value,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError>;

    /// The number of parameters `callee` declares, when it is a closure.
    fn arity(&self, callee: &Value) -> Option<usize>;
}

/// Type names that carry builtin associated functions, such as `Vector.of`.
pub fn is_builtin_type(name: &str) -> bool {
    matches!(
        name,
        "Array"
            | "Vector"
            | "String"
            | "Int"
            | "Float"
            | "Bool"
            | "Map"
            | "Set"
            | "Option"
            | "Result"
            | "Error"
    )
}

/// Names usable as bare constructor calls, such as `Ok(value)`.
pub fn is_constructor(name: &str) -> bool {
    matches!(name, "Ok" | "Err" | "Some" | "Error")
}

/// The methods that take a `var self` receiver and therefore need a mutable
/// place at the call site.
pub fn is_mutating_method(name: &str) -> bool {
    matches!(name, "push" | "freeze")
}

/// `Ok(v)`, `Err(e)`, `Some(v)`, `Error("message")`.
pub fn call_constructor(name: &str, args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
    let mut args = expect_args(name, args, 1, span)?;
    let value = args.remove(0);
    Ok(match name {
        "Ok" => Value::ok(value),
        "Err" => Value::err(value),
        "Some" => Value::some(value),
        "Error" => match value {
            Value::Str(message) => Value::error(message.to_string()),
            other => {
                return Err(type_error("Error", "message", "String", &other, span));
            }
        },
        _ => return Err(RuntimeError::new(format!("unknown constructor `{name}`")).at(span)),
    })
}

/// `Vector.of(...)` and `Int.parse(...)`.
pub fn call_associated(
    type_name: &str,
    name: &str,
    args: Vec<Value>,
    span: Span,
) -> Result<Value, RuntimeError> {
    match (type_name, name) {
        ("Vector", "of") => Ok(Value::Vector(VectorStorage::new(args))),
        ("Int", "parse") => {
            let args = expect_args("Int.parse", args, 1, span)?;
            let Value::Str(text) = &args[0] else {
                return Err(type_error("Int.parse", "text", "String", &args[0], span));
            };
            Ok(match text.parse::<i64>() {
                Ok(value) => Value::ok(Value::Int(value)),
                Err(_) => Value::err(Value::error(format!("`{text}` is not an Int"))),
            })
        }
        _ => Err(
            RuntimeError::new(format!("`{type_name}` has no associated function `{name}`"))
                .at(span),
        ),
    }
}

/// Dispatches `receiver.name(args)` to a builtin method.
pub fn call_method(
    host: &mut dyn Callable,
    receiver: &Value,
    name: &str,
    args: Vec<Value>,
    span: Span,
) -> Result<Value, RuntimeError> {
    if name == "count" && is_sequence(receiver) {
        return Err(count_is_spelled_length(&receiver.type_name(), span));
    }
    match receiver {
        Value::Array(items) => match name {
            "get" => Ok(index_of("Array.get", &args, span)?
                .and_then(|i| items.get(i).cloned())
                .map(Value::some)
                .unwrap_or_else(Value::none)),
            "length" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Int(items.len() as i64))
            }
            "isEmpty" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Bool(items.is_empty()))
            }
            _ => Err(no_method("Array", name, span)),
        },
        Value::Vector(storage) => {
            check_live(storage, name, span)?;
            match name {
                "push" => {
                    let mut args = expect_args("push", args, 1, span)?;
                    storage.elements.borrow_mut().push(args.remove(0));
                    Ok(Value::Unit)
                }
                "get" => Ok(index_of("Vector.get", &args, span)?
                    .and_then(|i| storage.elements.borrow().get(i).cloned())
                    .map(Value::some)
                    .unwrap_or_else(Value::none)),
                "length" => {
                    expect_args(name, args, 0, span)?;
                    Ok(Value::Int(storage.len() as i64))
                }
                "isEmpty" => {
                    expect_args(name, args, 0, span)?;
                    Ok(Value::Bool(storage.is_empty()))
                }
                "freeze" => {
                    expect_args("freeze", args, 0, span)?;
                    freeze(storage, span)
                }
                "toArray" => {
                    expect_args("toArray", args, 0, span)?;
                    Ok(Value::Array(
                        storage.elements.borrow().iter().cloned().collect(),
                    ))
                }
                _ => Err(no_method("Vector", name, span)),
            }
        }
        Value::Str(text) => match name {
            "length" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Int(text.chars().count() as i64))
            }
            "isEmpty" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Bool(text.is_empty()))
            }
            "words" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Array(
                    text.split_ascii_whitespace()
                        .map(|w| Value::Str(w.into()))
                        .collect(),
                ))
            }
            _ => Err(no_method("String", name, span)),
        },
        Value::Range {
            start,
            end,
            inclusive_end,
        } => {
            let bounds = RangeBounds::of(*start, *end, *inclusive_end);
            match name {
                "length" => {
                    expect_args(name, args, 0, span)?;
                    Ok(Value::Int(bounds.len()))
                }
                "isEmpty" => {
                    expect_args(name, args, 0, span)?;
                    Ok(Value::Bool(bounds.is_empty()))
                }
                "contains" => {
                    let args = expect_args("contains", args, 1, span)?;
                    let Value::Int(value) = &args[0] else {
                        return Err(type_error("Range.contains", "value", "Int", &args[0], span));
                    };
                    Ok(Value::Bool(bounds.contains(*value)))
                }
                _ => Err(no_method("Range", name, span)),
            }
        }
        Value::Enum(value) if &*value.type_name == "Option" => {
            let some = &*value.case == "Some";
            match name {
                "isSome" => {
                    expect_args(name, args, 0, span)?;
                    Ok(Value::Bool(some))
                }
                "isNone" => {
                    expect_args(name, args, 0, span)?;
                    Ok(Value::Bool(!some))
                }
                "unwrapOr" => {
                    let mut args = expect_args("unwrapOr", args, 1, span)?;
                    Ok(match value.payload.first() {
                        Some(inner) if some => inner.clone(),
                        _ => args.remove(0),
                    })
                }
                _ => Err(no_method("Option", name, span)),
            }
        }
        Value::Enum(value) if &*value.type_name == "Result" => {
            let ok = &*value.case == "Ok";
            match name {
                "isOk" => {
                    expect_args(name, args, 0, span)?;
                    Ok(Value::Bool(ok))
                }
                "isError" => {
                    expect_args(name, args, 0, span)?;
                    Ok(Value::Bool(!ok))
                }
                "mapError" => {
                    let mut args = expect_args("mapError", args, 1, span)?;
                    let callback = args.remove(0);
                    if ok {
                        return Ok(receiver.clone());
                    }
                    let error = value.payload.first().cloned().unwrap_or(Value::Unit);
                    // The Language Card writes `mapError { ... }` with a trailing
                    // closure that may ignore the error it replaces.
                    let arguments = match host.arity(&callback) {
                        Some(0) => Vec::new(),
                        _ => vec![error],
                    };
                    Ok(Value::err(host.call_value(&callback, arguments, span)?))
                }
                _ => Err(no_method("Result", name, span)),
            }
        }
        other => Err(no_method(&other.type_name(), name, span)),
    }
}

/// Consumes uniquely owned vector storage and returns its elements as an
/// `Array` in O(1).
///
/// Uniqueness is the runtime form of the Language Card's local uniqueness
/// check: the caller must hold the only handle to this storage.
pub fn freeze(storage: &Rc<VectorStorage>, span: Span) -> Result<Value, RuntimeError> {
    check_live(storage, "freeze", span)?;
    if Rc::strong_count(storage) != 1 {
        return Err(RuntimeError::new(
            "`freeze()` needs uniquely owned vector storage, but another alias observes this vector",
        )
        .at(span)
        .with_rule(
            "`freeze()` consumes a locally unique vector and returns an immutable array in O(1).",
        )
        .with_help(
            "call `toArray()` instead, which copies the elements in O(n), or drop the other alias before calling `freeze()`",
        ));
    }
    let elements = storage.elements.take();
    *storage.frozen.borrow_mut() = true;
    Ok(Value::Array(elements.into()))
}

/// A vector consumed by `freeze()` is no longer usable.
pub fn check_live(
    storage: &Rc<VectorStorage>,
    method: &str,
    span: Span,
) -> Result<(), RuntimeError> {
    if *storage.frozen.borrow() {
        return Err(RuntimeError::new(format!(
            "`{method}` was called on a vector that `freeze()` already consumed"
        ))
        .at(span)
        .with_rule("`freeze()` consumes its vector; the source vector is no longer usable.")
        .with_help("use the `Array` that `freeze()` returned, or build a new vector"));
    }
    Ok(())
}

fn index_of(method: &str, args: &[Value], span: Span) -> Result<Option<usize>, RuntimeError> {
    if args.len() != 1 {
        return Err(arity_error(method, 1, args.len(), span));
    }
    match &args[0] {
        Value::Int(i) if *i >= 0 => Ok(Some(*i as usize)),
        Value::Int(_) => Ok(None),
        other => Err(type_error(method, "index", "Int", other, span)),
    }
}

fn expect_args(
    method: &str,
    args: Vec<Value>,
    count: usize,
    span: Span,
) -> Result<Vec<Value>, RuntimeError> {
    if args.len() != count {
        return Err(arity_error(method, count, args.len(), span));
    }
    Ok(args)
}

fn arity_error(method: &str, expected: usize, found: usize, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "`{method}` takes {expected} argument(s), but {found} were given"
    ))
    .at(span)
}

fn type_error(
    method: &str,
    parameter: &str,
    expected: &str,
    found: &Value,
    span: Span,
) -> RuntimeError {
    RuntimeError::new(format!(
        "`{method}` expects `{expected}` for `{parameter}`, but found `{}`",
        found.type_name()
    ))
    .at(span)
}

fn no_method(type_name: &str, method: &str, span: Span) -> RuntimeError {
    RuntimeError::new(format!("`{type_name}` has no method `{method}`")).at(span)
}

/// The builtin sequences, which all report their element count the same way.
fn is_sequence(receiver: &Value) -> bool {
    matches!(
        receiver,
        Value::Array(_) | Value::Vector(_) | Value::Str(_) | Value::Range { .. }
    )
}

/// `count()` was removed in favour of a single spelling.
fn count_is_spelled_length(type_name: &str, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "`{type_name}` has no method `count`; Cove spells the number of elements `length()`"
    ))
    .at(span)
    .with_rule("Every sequence reports its element count as `length()`; there is no `count()`.")
    .with_help("write `length()` instead of `count()`")
}
