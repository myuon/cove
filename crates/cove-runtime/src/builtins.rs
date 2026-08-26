//! Builtin methods, associated functions, and constructors.
//!
//! Everything here is dispatched dynamically on a receiver value, a type name,
//! or a constructor name. The MVP has no method table derived from types yet,
//! so an arity or type mismatch is an ordinary [`RuntimeError`] that names the
//! method it came from.
//!
//! What each builtin *is* — its parameters, its result, and whether its
//! receiver is `var self` — is [`cove_schema::builtins`], two tables below
//! both this crate and the compiler: one for what is called on a receiver and
//! one for the constructors and assertions, which are called on nothing. This
//! module is the other half: the bodies, which have to be here because a body
//! reaches into a [`Value`] and `cove-schema` has no values. Every question a
//! name alone can answer is asked of the schema rather than answered twice —
//! which names are namespaces, which methods mutate, which names construct,
//! which assert, how many arguments each takes, and which receivers report a
//! `length` — and `tests/builtin_schema.rs` drives every entry in both tables
//! through a real interpreter, so a signature declared with no body behind it
//! fails a test rather than a program.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use cove_diag::Span;
use cove_schema::builtins::{
    FreeBuiltinKind, FreeBuiltinSchema, MAP_ENTRY, OK_CASE, OPTION, RESULT, SOME_CASE,
};

use crate::error::RuntimeError;
use crate::shared::SharedCell;
use crate::value::{InvalidKey, MapKey, RangeBounds, Value, VectorStorage};

/// Type names a program may write as a namespace, such as `Vector.of`.
///
/// This is [`cove_schema::builtins::is_builtin_type`], re-exported so that
/// the interpreter still asks one module about builtins.
pub use cove_schema::builtins::is_builtin_type;

/// The methods that take a `var self` receiver and therefore need a mutable
/// place at the call site.
///
/// This is [`cove_schema::builtins::is_mutating_method`]: `push` and `freeze`
/// declare `mutating` in the shared table, and nothing here restates them.
pub use cove_schema::builtins::is_mutating_method;

/// How the builtins call back into the evaluator.
///
/// Higher-order builtins such as `Result.mapError` invoke a Cove callback, so
/// they need the interpreter that owns the call stack.
pub trait Callable {
    /// Allocates growable vector storage in the running task's heap.
    ///
    /// Every `Vector` a program can reach is created through this, so the
    /// collector's table of objects is the complete set of values that can
    /// form a cycle. A builtin that makes one asks its caller rather than
    /// calling [`VectorStorage::new`] directly.
    fn allocate_vector(&mut self, elements: Vec<Value>) -> Value;

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

/// The builtins that are called on nothing: the constructors `Ok`, `Err`,
/// `Some`, `Error`, and `Shared`, and the assertions `assert` and
/// `assertEqual`.
///
/// This is [`cove_schema::builtins::free_builtin`], re-exported so that the
/// interpreter still asks one module about builtins. Which of the two kinds
/// an entry is decides which path a call is dispatched through, and how many
/// arguments it declares is what that call is held to.
pub use cove_schema::builtins::free_builtin;

/// `assert(condition: Bool) -> Result<Unit, Error>` and
/// `assertEqual(actual: T, expected: T) -> Result<Unit, Error>`.
///
/// `sources` holds the source text of each argument expression, in order.
/// That text is the whole reason these are builtins rather than a library:
/// a failure message says which condition failed in the words the test was
/// written in, and only the compiler has them.
///
/// A failing assertion is an expected failure, so it is an `Err` rather than
/// a panic — panics stay reserved for broken invariants. `assertEqual`
/// reports both values, since knowing only that they differ rarely explains
/// why.
///
/// How many arguments each takes and what each one is called are the shared
/// table's, so the arity this enforces is the arity `cove check` reported on.
pub fn call_assertion(
    name: &str,
    args: Vec<Value>,
    sources: &[&str],
    span: Span,
) -> Result<Value, RuntimeError> {
    let Some(schema) =
        free_builtin(name).filter(|schema| schema.kind == FreeBuiltinKind::Assertion)
    else {
        return Err(RuntimeError::new(format!("unknown assertion `{name}`")).at(span));
    };
    let args = expect_args(name, args, schema.arity(), span)?;
    match name {
        "assert" => {
            let Value::Bool(holds) = &args[0] else {
                return Err(declared_type_error(schema, 0, &args[0], span));
            };
            if *holds {
                return Ok(Value::ok(Value::Unit));
            }
            Ok(assertion_failure(format!(
                "assertion failed: `{}`",
                source_of(sources, 0)
            )))
        }
        "assertEqual" => {
            // `assertEqual` compares the way `==` does, so it refuses the
            // same comparison `==` refuses. The shared table says as much by
            // naming one type parameter twice.
            if args[0].type_name() != args[1].type_name() {
                return Err(RuntimeError::new(format!(
                    "`assertEqual` cannot compare `{}` with `{}`",
                    args[0].type_name(),
                    args[1].type_name()
                ))
                .at(span)
                .with_rule("`==` means value equality between values of the same type."));
            }
            if args[0].eq_value(&args[1]) {
                return Ok(Value::ok(Value::Unit));
            }
            Ok(assertion_failure(format!(
                "assertion failed: `{}` is `{}`, expected `{}`",
                source_of(sources, 0),
                args[0],
                args[1]
            )))
        }
        // The table admitted the name, so this is a table entry with no body
        // behind it, which `tests/builtin_schema.rs` is what catches.
        _ => Err(RuntimeError::new(format!("unknown assertion `{name}`")).at(span)),
    }
}

/// A free builtin was given an argument its declared parameter does not
/// admit.
///
/// The parameter's name and type are read out of the shared table, so
/// `Error("boom")` and `assert(1)` are refused in the words the table
/// declares them in.
fn declared_type_error(
    schema: &FreeBuiltinSchema,
    index: usize,
    found: &Value,
    span: Span,
) -> RuntimeError {
    let param = &schema.params[index];
    type_error(schema.name, param.name, &param.ty.to_string(), found, span)
}

/// The `Err` a failed assertion produces.
fn assertion_failure(message: String) -> Value {
    Value::err(Value::error(message))
}

/// The source text of argument `index`, or a placeholder when the caller
/// could not supply it.
fn source_of<'a>(sources: &[&'a str], index: usize) -> &'a str {
    sources.get(index).copied().unwrap_or("?")
}

/// `Ok(v)`, `Err(e)`, `Some(v)`, `Error("message")`, `Shared(value)`.
///
/// Which names these are and how many arguments each carries come from the
/// shared table; what each one builds is here, because building one needs a
/// [`Value`].
pub fn call_constructor(name: &str, args: Vec<Value>, span: Span) -> Result<Value, RuntimeError> {
    let Some(schema) =
        free_builtin(name).filter(|schema| schema.kind == FreeBuiltinKind::Constructor)
    else {
        return Err(RuntimeError::new(format!("unknown constructor `{name}`")).at(span));
    };
    let mut args = expect_args(name, args, schema.arity(), span)?;
    let value = args.remove(0);
    Ok(match name {
        "Ok" => Value::ok(value),
        "Err" => Value::err(value),
        "Some" => Value::some(value),
        // `Shared` is the one constructor that can refuse its payload: what
        // it wraps must be task-safe, since a `Shared` is reachable from
        // every task it was given to.
        "Shared" => Value::Shared(SharedCell::wrap(&value, span)?),
        "Error" => match value {
            Value::Str(message) => Value::error(message.to_string()),
            other => {
                return Err(declared_type_error(schema, 0, &other, span));
            }
        },
        // As in `call_assertion`: a name the table declares and nothing here
        // builds is what `tests/builtin_schema.rs` refuses to let happen.
        _ => return Err(RuntimeError::new(format!("unknown constructor `{name}`")).at(span)),
    })
}

/// `Vector.of(...)` and `Int.parse(...)`.
pub fn call_associated(
    host: &mut dyn Callable,
    type_name: &str,
    name: &str,
    args: Vec<Value>,
    span: Span,
) -> Result<Value, RuntimeError> {
    match (type_name, name) {
        ("Vector", "of") => Ok(host.allocate_vector(args)),
        // `Map.of` takes the `MapEntry` values `MapEntry(key:, value:)`
        // builds. A literal with two identical keys is a mistake, not an
        // intent, so a duplicate key is rejected rather than resolved by
        // silently keeping the first or last entry.
        ("Map", "of") => {
            let mut map: BTreeMap<MapKey, Value> = BTreeMap::new();
            for arg in args {
                let Value::Struct(entry) = &arg else {
                    return Err(expects_map_entry(&arg, span));
                };
                if &*entry.type_name != MAP_ENTRY.name {
                    return Err(expects_map_entry(&arg, span));
                }
                let key_value = entry.get("key").expect("MapEntry always has a `key` field");
                let key = to_map_key("Map.of", "map key", key_value, span)?;
                if map.contains_key(&key) {
                    return Err(duplicate_key_error("Map.of", "key", &key, span));
                }
                let value = entry
                    .get("value")
                    .expect("MapEntry always has a `value` field")
                    .clone();
                map.insert(key, value);
            }
            Ok(Value::Map(Rc::new(map)))
        }
        // `Set.of` rejects a duplicate element for the same reason `Map.of`
        // rejects a duplicate key.
        ("Set", "of") => {
            let mut set: BTreeSet<MapKey> = BTreeSet::new();
            for item in args {
                let key = to_map_key("Set.of", "set element", &item, span)?;
                if !set.insert(key.clone()) {
                    return Err(duplicate_key_error("Set.of", "element", &key, span));
                }
            }
            Ok(Value::Set(Rc::new(set)))
        }
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
        // Mirrors `Int.parse` exactly in shape. Rust's `f64::from_str`
        // accepts `inf`, `-inf`, and `NaN`, which is why this does too, and
        // it rejects the `_` digit separators a `Float` literal may be
        // written with — the same thing `Int.parse` above already does,
        // not a new choice made here.
        ("Float", "parse") => {
            let args = expect_args("Float.parse", args, 1, span)?;
            let Value::Str(text) = &args[0] else {
                return Err(type_error("Float.parse", "text", "String", &args[0], span));
            };
            Ok(match text.parse::<f64>() {
                Ok(value) => Value::ok(Value::Float(value)),
                Err(_) => Value::err(Value::error(format!("`{text}` is not a Float"))),
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
    // A receiver that answers `length()` is one a program might have written
    // `count()` on, so the shared table's own methods are what decide who is
    // taught the spelling. The name is compared first, so an ordinary call
    // never asks.
    if name == "count" {
        let type_name = receiver.type_name();
        if cove_schema::builtins::declares_length(&type_name) {
            return Err(count_is_spelled_length(&type_name, span));
        }
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
        Value::Map(entries) => match name {
            "get" => {
                let args = expect_args("Map.get", args, 1, span)?;
                let key = to_map_key("Map.get", "map key", &args[0], span)?;
                Ok(entries
                    .get(&key)
                    .cloned()
                    .map(Value::some)
                    .unwrap_or_else(Value::none))
            }
            "contains" => {
                let args = expect_args("Map.contains", args, 1, span)?;
                let key = to_map_key("Map.contains", "map key", &args[0], span)?;
                Ok(Value::Bool(entries.contains_key(&key)))
            }
            "length" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Int(entries.len() as i64))
            }
            "isEmpty" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Bool(entries.is_empty()))
            }
            // Ascending key order, matching the `BTreeMap` storage and the
            // order `for` iterates.
            "keys" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Array(entries.keys().map(MapKey::to_value).collect()))
            }
            "values" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Array(entries.values().cloned().collect()))
            }
            // `Map` is immutable, so `inserted`/`removed` return a new map
            // rather than write through `entries`; the past-participle names
            // say so, unlike `Vector`'s mutating `push`.
            "inserted" => {
                let mut args = expect_args("Map.inserted", args, 2, span)?;
                let value = args.remove(1);
                let key = to_map_key("Map.inserted", "map key", &args[0], span)?;
                let mut next = (**entries).clone();
                next.insert(key, value);
                Ok(Value::Map(Rc::new(next)))
            }
            "removed" => {
                let args = expect_args("Map.removed", args, 1, span)?;
                let key = to_map_key("Map.removed", "map key", &args[0], span)?;
                let mut next = (**entries).clone();
                next.remove(&key);
                Ok(Value::Map(Rc::new(next)))
            }
            _ => Err(no_method("Map", name, span)),
        },
        Value::Set(items) => match name {
            "contains" => {
                let args = expect_args("Set.contains", args, 1, span)?;
                let key = to_map_key("Set.contains", "set element", &args[0], span)?;
                Ok(Value::Bool(items.contains(&key)))
            }
            "length" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Int(items.len() as i64))
            }
            "isEmpty" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Bool(items.is_empty()))
            }
            "toArray" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Array(items.iter().map(MapKey::to_value).collect()))
            }
            "inserted" => {
                let args = expect_args("Set.inserted", args, 1, span)?;
                let key = to_map_key("Set.inserted", "set element", &args[0], span)?;
                let mut next = (**items).clone();
                next.insert(key);
                Ok(Value::Set(Rc::new(next)))
            }
            "removed" => {
                let args = expect_args("Set.removed", args, 1, span)?;
                let key = to_map_key("Set.removed", "set element", &args[0], span)?;
                let mut next = (**items).clone();
                next.remove(&key);
                Ok(Value::Set(Rc::new(next)))
            }
            _ => Err(no_method("Set", name, span)),
        },
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
            "chars" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Array(
                    text.chars()
                        .map(|c| Value::Str(c.to_string().into()))
                        .collect(),
                ))
            }
            "split" => {
                let args = expect_args("String.split", args, 1, span)?;
                let separator = expect_str("String.split", "separator", &args[0], span)?;
                if separator.is_empty() {
                    return Err(empty_needle_error(
                        "String.split",
                        "separator",
                        "use `chars()` to take a string apart character by character",
                        span,
                    ));
                }
                Ok(Value::Array(
                    text.split(separator)
                        .map(|part| Value::Str(part.into()))
                        .collect(),
                ))
            }
            "join" => {
                let args = expect_args("String.join", args, 1, span)?;
                let Value::Array(parts) = &args[0] else {
                    return Err(type_error(
                        "String.join",
                        "parts",
                        "Array<String>",
                        &args[0],
                        span,
                    ));
                };
                let mut joined = String::new();
                for (index, part) in parts.iter().enumerate() {
                    if index > 0 {
                        joined.push_str(text);
                    }
                    joined.push_str(expect_str("String.join", "parts", part, span)?);
                }
                Ok(Value::Str(joined.into()))
            }
            "slice" => {
                let args = expect_args("String.slice", args, 2, span)?;
                let Value::Int(from) = &args[0] else {
                    return Err(type_error("String.slice", "from", "Int", &args[0], span));
                };
                let Value::Int(to) = &args[1] else {
                    return Err(type_error("String.slice", "to", "Int", &args[1], span));
                };
                let chars: Vec<char> = text.chars().collect();
                let len = chars.len() as i64;
                let from = (*from).clamp(0, len) as usize;
                let to = (*to).clamp(0, len) as usize;
                Ok(Value::Str(if to <= from {
                    "".into()
                } else {
                    chars[from..to].iter().collect::<String>().into()
                }))
            }
            "trim" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Str(text.trim().into()))
            }
            "contains" => {
                let args = expect_args("String.contains", args, 1, span)?;
                let needle = expect_str("String.contains", "text", &args[0], span)?;
                Ok(Value::Bool(text.contains(needle)))
            }
            "startsWith" => {
                let args = expect_args("String.startsWith", args, 1, span)?;
                let prefix = expect_str("String.startsWith", "prefix", &args[0], span)?;
                Ok(Value::Bool(text.starts_with(prefix)))
            }
            "endsWith" => {
                let args = expect_args("String.endsWith", args, 1, span)?;
                let suffix = expect_str("String.endsWith", "suffix", &args[0], span)?;
                Ok(Value::Bool(text.ends_with(suffix)))
            }
            "indexOf" => {
                let args = expect_args("String.indexOf", args, 1, span)?;
                let needle = expect_str("String.indexOf", "text", &args[0], span)?;
                Ok(match text.find(needle) {
                    // `find` answers a byte offset; the characters before it
                    // are counted to convert that into the character index
                    // `length()` already counts in.
                    Some(byte_index) => {
                        Value::some(Value::Int(text[..byte_index].chars().count() as i64))
                    }
                    None => Value::none(),
                })
            }
            "replace" => {
                let args = expect_args("String.replace", args, 2, span)?;
                let old = expect_str("String.replace", "old", &args[0], span)?;
                if old.is_empty() {
                    return Err(empty_needle_error(
                        "String.replace",
                        "old",
                        "`old` is the text to look for, and an empty `old` names none",
                        span,
                    ));
                }
                let new = expect_str("String.replace", "new", &args[1], span)?;
                Ok(Value::Str(text.replace(old, new).into()))
            }
            "toUpper" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Str(text.to_uppercase().into()))
            }
            "toLower" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Str(text.to_lowercase().into()))
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
        Value::Enum(value) if &*value.type_name == OPTION.name => {
            let some = &*value.case == SOME_CASE.name;
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
        Value::Enum(value) if &*value.type_name == RESULT.name => {
            let ok = &*value.case == OK_CASE.name;
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
        Value::Int(n) => match name {
            "toFloat" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Float(*n as f64))
            }
            "abs" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Int(
                    n.checked_abs()
                        .ok_or_else(|| crate::interp::overflow("abs", span))?,
                ))
            }
            "min" => {
                let args = expect_args("Int.min", args, 1, span)?;
                let Value::Int(other) = &args[0] else {
                    return Err(type_error("Int.min", "other", "Int", &args[0], span));
                };
                Ok(Value::Int((*n).min(*other)))
            }
            "max" => {
                let args = expect_args("Int.max", args, 1, span)?;
                let Value::Int(other) = &args[0] else {
                    return Err(type_error("Int.max", "other", "Int", &args[0], span));
                };
                Ok(Value::Int((*n).max(*other)))
            }
            _ => Err(no_method("Int", name, span)),
        },
        Value::Float(x) => match name {
            "toInt" => {
                expect_args(name, args, 0, span)?;
                Ok(float_to_int(*x))
            }
            "round" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Float(x.round()))
            }
            "abs" => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Float(x.abs()))
            }
            "min" => {
                let args = expect_args("Float.min", args, 1, span)?;
                let Value::Float(other) = &args[0] else {
                    return Err(type_error("Float.min", "other", "Float", &args[0], span));
                };
                Ok(Value::Float(x.min(*other)))
            }
            "max" => {
                let args = expect_args("Float.max", args, 1, span)?;
                let Value::Float(other) = &args[0] else {
                    return Err(type_error("Float.max", "other", "Float", &args[0], span));
                };
                Ok(Value::Float(x.max(*other)))
            }
            "format" => {
                let args = expect_args("Float.format", args, 1, span)?;
                let Value::Int(digits) = &args[0] else {
                    return Err(type_error("Float.format", "digits", "Int", &args[0], span));
                };
                if !(0..=17).contains(digits) {
                    return Err(format_digits_error(*digits, span));
                }
                Ok(Value::Str(format!("{:.*}", *digits as usize, x).into()))
            }
            _ => Err(no_method("Float", name, span)),
        },
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

/// `Float.toInt`: truncates toward zero and names which of the three expected
/// failures stopped it. `NaN` is not a number, an infinity has no
/// truncation, and a magnitude at or past 2^63 does not fit in an `Int`.
fn float_to_int(x: f64) -> Value {
    if x.is_nan() {
        return Value::err(Value::error(
            "`Float.toInt` cannot convert `NaN`, which is not a number",
        ));
    }
    if x.is_infinite() {
        return Value::err(Value::error(format!(
            "`Float.toInt` cannot convert `{x}`, which has no truncation"
        )));
    }
    let truncated = x.trunc();
    if truncated < i64::MIN as f64 || truncated >= i64::MAX as f64 {
        return Value::err(Value::error(format!(
            "`Float.toInt` cannot convert `{x}`, which is outside Int's range"
        )));
    }
    Value::ok(Value::Int(truncated as i64))
}

/// `Float.format` refused a `digits` outside `0..=17`.
///
/// A `Float` carries at most 17 significant decimal digits, so a `digits`
/// past that asks for padding rather than precision, and a negative `digits`
/// names nothing.
fn format_digits_error(digits: i64, span: Span) -> RuntimeError {
    RuntimeError::new(format!("`Float.format` cannot use `{digits}` digits"))
        .at(span)
        .with_rule(
            "A Float carries at most 17 significant decimal digits, so `digits` must be between 0 and 17.",
        )
}

/// Reads `value` as a `String`, or reports the type `method` declares for
/// `parameter` instead.
fn expect_str<'a>(
    method: &str,
    parameter: &str,
    value: &'a Value,
    span: Span,
) -> Result<&'a str, RuntimeError> {
    match value {
        Value::Str(text) => Ok(text),
        other => Err(type_error(method, parameter, "String", other, span)),
    }
}

/// `split` and `replace` both refuse an empty needle: matching against one
/// would match between every character rather than answer either method's
/// question.
///
/// The two are told different things afterwards, because the operation they
/// were reaching for is different. Splitting on nothing is a request for the
/// characters, which `chars()` answers; replacing nothing is not a request for
/// anything, so `replace` is told what it is missing rather than offered a
/// substitute.
fn empty_needle_error(method: &str, parameter: &str, help: &str, span: Span) -> RuntimeError {
    RuntimeError::new(format!("`{method}` cannot use an empty `{parameter}`"))
        .at(span)
        .with_rule(
            "An empty separator or search string would match between every character, rather than answer the question the method asks.",
        )
        .with_help(help)
}

/// Converts `value` to a [`MapKey`], or reports why it cannot be a map key or
/// set element.
fn to_map_key(method: &str, role: &str, value: &Value, span: Span) -> Result<MapKey, RuntimeError> {
    MapKey::from_value(value).map_err(|invalid| invalid_key_error(method, role, &invalid, span))
}

/// Names the specific offending part when the invalid value is nested, such
/// as `` a `Vector` inside `Point.tags` ``, rather than blaming the whole
/// struct: the Language Card promises errors that teach the rule they name.
fn invalid_key_error(method: &str, role: &str, invalid: &InvalidKey, span: Span) -> RuntimeError {
    let message = if invalid.path.is_empty() {
        format!(
            "`{method}` cannot use a `{}` as a {role}",
            invalid.type_name
        )
    } else {
        format!(
            "`{method}` cannot use a `{}` inside `{}` as a {role}",
            invalid.type_name, invalid.path
        )
    };
    RuntimeError::new(message)
        .at(span)
        .with_rule(invalid.rule())
        .with_help(invalid.help())
}

/// `Map.of` and `Set.of` reject a duplicate key or element rather than
/// silently keeping one entry, because a literal with two identical keys is a
/// mistake, not an intent.
fn duplicate_key_error(method: &str, role: &str, key: &MapKey, span: Span) -> RuntimeError {
    RuntimeError::new(format!("`{method}` was given the {role} `{key}` more than once"))
        .at(span)
        .with_rule(
            "A literal with two identical keys is a mistake, not an intent; duplicate keys are rejected rather than silently resolved by keeping the last one.",
        )
        .with_help(format!("remove the duplicate, or give it a different {role}"))
}

/// `Map.of` takes `MapEntry` values, built with `MapEntry(key:, value:)`.
fn expects_map_entry(found: &Value, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "`Map.of` expects `MapEntry` values, but found `{}`",
        found.type_name()
    ))
    .at(span)
    .with_rule(
        "`Map.of(entries: MapEntry<K, V>...)` takes values built with `MapEntry(key:, value:)`.",
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
