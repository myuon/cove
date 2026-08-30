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
/// This is [`cove_schema::builtins::is_mutating_method`]: `push`, `set`,
/// `pop`, `remove` and `freeze` declare `mutating` in the shared table, and
/// nothing here restates them.
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

    /// The independent copy `Snapshot` makes of one value.
    ///
    /// A hook rather than a function because a struct and an enum answer it
    /// through their own `impl Snapshot for Type`, which is a declaration
    /// only a backend can reach: the interpreter invokes the conformance,
    /// and the VM has no way to call one from inside an instruction and
    /// reports it instead. [`snapshot`] is the half that is the same for
    /// both, and it recurses through here so that a `Vector` of structs
    /// reaches whichever answer its backend has.
    fn snapshot(&mut self, value: &Value, span: Span) -> Result<Value, RuntimeError>;
}

/// The independent copy `Snapshot` makes of a value that no declared
/// conformance answers for.
///
/// The Language Reference makes an independent copy an explicit `impl
/// Snapshot for Type`, and this is everything that decision leaves over: a
/// value with nothing mutable inside it returns itself, because a copy of it
/// is not observable, and a `Vector` — the one thing a copy is observable of
/// — allocates storage of its own and snapshots what it held.
///
/// An `Array`, a `Map` and a `Set` are cloned rather than walked, which is
/// `Interpreter::snapshot`'s own answer and not a shortcut taken here: each
/// is immutable, so an element that shares storage with something else went
/// on sharing it before this was called and there is nothing for a copy to
/// separate.
///
/// A struct, an enum and a `dyn` are not here. They are what the caller
/// answers, through [`Callable::snapshot`].
pub fn snapshot(
    callable: &mut dyn Callable,
    value: &Value,
    span: Span,
) -> Result<Value, RuntimeError> {
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
            check_live(storage, "snapshot", span)?;
            let elements = storage.elements.borrow().clone();
            let mut snapshotted = Vec::with_capacity(elements.len());
            for item in &elements {
                snapshotted.push(callable.snapshot(item, span)?);
            }
            Ok(callable.allocate_vector(snapshotted))
        }
        other => Err(no_snapshot_conformance(other, span)),
    }
}

/// What a `...` argument that is neither an `Array` nor a `Vector` is
/// refused with.
///
/// A spread passes an existing sequence where a variadic parameter's
/// elements would go, so the two sequences are what it reads; `bind_params`
/// reports anything else, and the VM reports it from the instruction that
/// does the appending. One wording, because it is one failure.
pub fn spread_needs_a_sequence(span: Span) -> RuntimeError {
    RuntimeError::new("`...` spreads an `Array` or a `Vector`").at(span)
}

/// What a value that implements no `Snapshot` conformance is refused with.
///
/// Both backends reach it: the interpreter for a struct or an enum whose
/// type wrote none, and the VM for one it reached inside a `Vector` — where
/// it cannot call the conformance even if there is one, because an
/// instruction has no way to run a whole function in the middle of itself.
pub fn no_snapshot_conformance(value: &Value, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "`{}` does not implement `Snapshot`",
        value.type_name()
    ))
    .at(span)
    .with_rule(
        "Closures, synchronized values, and Host resources do not implement `Snapshot` by default; a struct or enum conforms explicitly with `impl Snapshot for Type`.",
    )
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
    args: &mut Vec<Value>,
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
pub fn call_constructor(
    name: &str,
    args: &mut Vec<Value>,
    span: Span,
) -> Result<Value, RuntimeError> {
    let Some(schema) =
        free_builtin(name).filter(|schema| schema.kind == FreeBuiltinKind::Constructor)
    else {
        return Err(RuntimeError::new(format!("unknown constructor `{name}`")).at(span));
    };
    let args = expect_args(name, args, schema.arity(), span)?;
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
    args: &mut Vec<Value>,
    span: Span,
) -> Result<Value, RuntimeError> {
    match (type_name, name) {
        ("Vector", "of") => Ok(host.allocate_vector(std::mem::take(args))),
        // `Map.of` takes the `MapEntry` values `MapEntry(key:, value:)`
        // builds. A literal with two identical keys is a mistake, not an
        // intent, so a duplicate key is rejected rather than resolved by
        // silently keeping the first or last entry.
        ("Map", "of") => {
            let mut map: BTreeMap<MapKey, Value> = BTreeMap::new();
            for arg in args.drain(..) {
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
            for item in args.drain(..) {
                let key = to_map_key("Set.of", "set element", &item, span)?;
                if !set.insert(key.clone()) {
                    return Err(duplicate_key_error("Set.of", "element", &key, span));
                }
            }
            Ok(Value::Set(Rc::new(set)))
        }
        // `Duration.millis(n)` and its five neighbours: the six duration
        // literal suffixes as functions, so a count a program computed
        // becomes the `Duration` a host call takes. `Duration.seconds(1)`
        // and `1s` are one value.
        //
        // A negative count is a negative duration, because a `Duration` is
        // signed nanoseconds and `-1s` is already writable. A count whose
        // nanoseconds do not fit stops the run in the words `Duration`
        // arithmetic already stops it in — `checked_mul` is the same
        // question `checked_add` asks for `1h + 1h`, so an overflow is one
        // kind of event however the duration was reached.
        ("Duration", unit) if duration_unit(unit).is_some() => {
            let factor = duration_unit(unit).expect("the guard just asked");
            let what = format!("Duration.{unit}");
            let args = expect_args(&what, args, 1, span)?;
            let Value::Int(count) = &args[0] else {
                return Err(type_error(&what, "count", "Int", &args[0], span));
            };
            let nanos = count
                .checked_mul(factor)
                .ok_or_else(|| crate::interp::overflow("duration arithmetic", span))?;
            Ok(Value::Duration(nanos))
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
        // `Int.parse` in a base other than ten. A `radix` outside `2..=36`
        // names no notation, so it stops the run the way an empty
        // `String.split` separator does; text that is not a number in a
        // radix that does exist is the data's failure and answers `Err`,
        // which is the same line `Int.parse` draws. Rust's
        // `i64::from_str_radix` reads a leading `+` or `-` and no digit
        // separators, exactly as `parse::<i64>` above does.
        ("Int", "parseRadix") => {
            let args = expect_args("Int.parseRadix", args, 2, span)?;
            let Value::Str(text) = &args[0] else {
                return Err(type_error(
                    "Int.parseRadix",
                    "text",
                    "String",
                    &args[0],
                    span,
                ));
            };
            let Value::Int(radix) = &args[1] else {
                return Err(type_error("Int.parseRadix", "radix", "Int", &args[1], span));
            };
            let Some(radix) = (2..=36).contains(radix).then_some(*radix as u32) else {
                return Err(radix_error(*radix, span));
            };
            Ok(match i64::from_str_radix(text, radix) {
                Ok(value) => Value::ok(Value::Int(value)),
                Err(_) => Value::err(Value::error(format!(
                    "`{text}` is not an Int in radix {radix}"
                ))),
            })
        }
        // The one-character `String` a Unicode code point names. A character
        // in Cove is a `String` of length 1 — `chars()` answers an array of
        // them — so this is that decomposition run backwards, and there is
        // no `Character` type for it to answer instead.
        ("String", "fromCodePoint") => {
            let args = expect_args("String.fromCodePoint", args, 1, span)?;
            let Value::Int(code_point) = &args[0] else {
                return Err(type_error(
                    "String.fromCodePoint",
                    "codePoint",
                    "Int",
                    &args[0],
                    span,
                ));
            };
            Ok(from_code_point(*code_point))
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
    args: &mut Vec<Value>,
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
            "get" => Ok(index_of("Array.get", args, span)?
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
            "contains" => contains("Array.contains", items, args, span),
            "indexOf" => index_of_element("Array.indexOf", items, args, span),
            "slice" => Ok(Value::Array(slice("Array.slice", items, args, span)?)),
            // `Vector.toArray` run backwards: a growable copy of these
            // elements that nothing else holds a handle to, so a `freeze()`
            // on it is the O(1) one. The elements are cloned as they are
            // rather than snapshotted, which is `toArray`'s own rule — this
            // separates the sequence and nothing inside it. The storage
            // comes from the running task's heap like every other `Vector`,
            // so the collector sees it.
            "toVector" => {
                expect_args("toVector", args, 0, span)?;
                Ok(host.allocate_vector(items.to_vec()))
            }
            "map" | "filter" | "fold" | "sorted" => {
                walk_with(host, "Array", items.to_vec(), name, args, span)
            }
            _ => Err(no_method("Array", name, span)),
        },
        Value::Vector(storage) => {
            check_live(storage, name, span)?;
            match name {
                "push" => {
                    let args = expect_args("push", args, 1, span)?;
                    storage.elements.borrow_mut().push(args.remove(0));
                    Ok(Value::Unit)
                }
                // Replaces the element at `index` and answers what was
                // there, or answers `None` and writes nothing when `index`
                // is not already in the vector — which is `get`'s answer to
                // the same bad index, so a program has one rule about
                // indices rather than two. The write goes through the
                // storage handle, exactly as `push`'s does, so an alias
                // observes it and there is nothing to write back to the
                // receiver's own slot.
                "set" => {
                    let args = expect_args("Vector.set", args, 2, span)?;
                    let value = args.remove(1);
                    let Some(index) = index_of("Vector.set", args, span)? else {
                        return Ok(Value::none());
                    };
                    let mut elements = storage.elements.borrow_mut();
                    let Some(slot) = elements.get_mut(index) else {
                        return Ok(Value::none());
                    };
                    Ok(Value::some(std::mem::replace(slot, value)))
                }
                // Takes the last element out and answers it, or answers
                // `None` and writes nothing when there is no last element.
                //
                // The empty case is `remove(length() - 1)` on an empty
                // vector, where that index is `-1` — which `get`, `set` and
                // `remove` all answer `None` for. One rule about indices,
                // rather than a rule about indices and a rule about
                // emptiness.
                "pop" => {
                    expect_args("Vector.pop", args, 0, span)?;
                    Ok(storage
                        .elements
                        .borrow_mut()
                        .pop()
                        .map(Value::some)
                        .unwrap_or_else(Value::none))
                }
                // Takes the element at `index` out, moves everything after
                // it down one, and answers what was there — or answers
                // `None` and removes nothing for an index that is not
                // already in the vector, which is `get`'s answer and
                // `set`'s. The write goes through the storage handle, as
                // `push`'s and `set`'s do, so an alias observes the shrink.
                "remove" => {
                    let Some(index) = index_of("Vector.remove", args, span)? else {
                        return Ok(Value::none());
                    };
                    let mut elements = storage.elements.borrow_mut();
                    if index >= elements.len() {
                        return Ok(Value::none());
                    }
                    Ok(Value::some(elements.remove(index)))
                }
                "get" => Ok(index_of("Vector.get", args, span)?
                    .and_then(|i| storage.elements.borrow().get(i).cloned())
                    .map(Value::some)
                    .unwrap_or_else(Value::none)),
                "contains" => contains("Vector.contains", &storage.elements.borrow(), args, span),
                "indexOf" => {
                    index_of_element("Vector.indexOf", &storage.elements.borrow(), args, span)
                }
                "slice" => {
                    let sliced = slice("Vector.slice", &storage.elements.borrow(), args, span)?;
                    Ok(Value::Array(sliced))
                }
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
                "map" | "filter" | "fold" | "sorted" => {
                    // The elements come out here, before the first callback,
                    // and the borrow ends with this statement. Both matter:
                    // a callback can reach this very vector and push onto it
                    // or `freeze` it, and it must find neither a live borrow
                    // nor a walk that changes under it.
                    let elements = storage.elements.borrow().clone();
                    walk_with(host, "Vector", elements, name, args, span)
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
                let args = expect_args("Map.inserted", args, 2, span)?;
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
                    text.chars().map(|c| Value::Str(one_character(c))).collect(),
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
                    let args = expect_args("unwrapOr", args, 1, span)?;
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
                // `Option.unwrapOr` above, with `Ok` where it has `Some`.
                // The error an `Err` carries is dropped rather than passed
                // to anything, which is the whole difference between this
                // and `mapError`: a caller that wants to see the error has
                // that one, and a caller that has a default has this one.
                "unwrapOr" => {
                    let args = expect_args("unwrapOr", args, 1, span)?;
                    Ok(match value.payload.first() {
                        Some(inner) if ok => inner.clone(),
                        _ => args.remove(0),
                    })
                }
                "mapError" => {
                    let args = expect_args("mapError", args, 1, span)?;
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
        // The six builders read backwards: `d.millis()` is the whole number
        // of milliseconds in `d`, **truncated toward zero**, which is what
        // `Int` division already does — so `1500ms.seconds()` is 1 and
        // `(-1500ms).seconds()` is -1, and `d.seconds()` is
        // `d.nanos() / 1_000_000_000` whichever way a program asks. None can
        // fail: dividing a count that fits leaves a count that fits.
        Value::Duration(ns) => match duration_unit(name) {
            Some(factor) => {
                expect_args(name, args, 0, span)?;
                Ok(Value::Int(ns / factor))
            }
            None => Err(no_method("Duration", name, span)),
        },
        other => Err(no_method(&other.type_name(), name, span)),
    }
}

/// The nanoseconds in one of the six units a `Duration` is written in.
///
/// One table for both directions and for both halves of the toolchain's
/// question: `Duration.millis(n)` multiplies by what `d.millis()` divides
/// by, so a duration built in a unit and read back in it is the same number.
/// The names are the schema's, and the factors are the ones the lexer gives
/// the matching literal suffix — `ns`, `us`, `ms`, `s`, `m`, `h` — so `1s`
/// and `Duration.seconds(1)` cannot come apart.
fn duration_unit(name: &str) -> Option<i64> {
    Some(match name {
        "nanos" => 1,
        "micros" => 1_000,
        "millis" => 1_000_000,
        "seconds" => 1_000_000_000,
        "minutes" => 60 * 1_000_000_000,
        "hours" => 60 * 60 * 1_000_000_000,
        _ => return None,
    })
}

/// `map`, `filter`, `fold`, and `sorted`, which are the same four operations
/// on an `Array` and on a `Vector`.
///
/// `elements` is already the caller's own copy — the `Array`'s elements, or
/// the `Vector`'s taken out from under its `RefCell` before this was
/// called — which is what makes the walk a walk over a snapshot. A callback
/// that reaches the vector it was handed an element of may push onto it,
/// `freeze` it, or drop the last other handle to it, and none of the three
/// changes what is being walked or what comes back. `cove_ir::Inst::IterItems`
/// makes a `for` ask once and walk what it was given; this is the same
/// decision, in the place where a closure rather than a loop body is what
/// could do the mutating.
///
/// Everything a callback costs is accounted where any other call is:
/// [`Callable::call_value`] is the evaluator re-entered, so fuel, the depth
/// limit, the host's `max_call_depth`, cancellation, and the trace are the
/// running task's exactly as they are outside a builtin. There is nothing
/// here that steps around a safepoint, because there is nothing here that
/// runs Cove code by any other route.
///
/// A callback that fails takes the whole call with it. The answer is built
/// to the side and returned only on success, so no half-built array and no
/// half-sorted sequence is ever reachable, and no receiver is written
/// through on any path.
fn walk_with(
    host: &mut dyn Callable,
    type_name: &str,
    elements: Vec<Value>,
    name: &str,
    args: &mut Vec<Value>,
    span: Span,
) -> Result<Value, RuntimeError> {
    let method = format!("{type_name}.{name}");
    match name {
        "map" => {
            let args = expect_args(&method, args, 1, span)?;
            let transform = args.remove(0);
            expect_callback(
                host,
                &method,
                "transform",
                "fn(T) -> R",
                1,
                &transform,
                span,
            )?;
            let mut mapped = Vec::with_capacity(elements.len());
            for item in elements {
                mapped.push(host.call_value(&transform, vec![item], span)?);
            }
            Ok(Value::Array(mapped.into()))
        }
        "filter" => {
            let args = expect_args(&method, args, 1, span)?;
            let keep = args.remove(0);
            expect_callback(host, &method, "keep", "fn(T) -> Bool", 1, &keep, span)?;
            let mut kept = Vec::new();
            for item in elements {
                let verdict = host.call_value(&keep, vec![item.clone()], span)?;
                if callback_bool(&method, "keep", &verdict, span)? {
                    kept.push(item);
                }
            }
            Ok(Value::Array(kept.into()))
        }
        "fold" => {
            let args = expect_args(&method, args, 2, span)?;
            let step = args.remove(1);
            let mut total = args.remove(0);
            expect_callback(host, &method, "step", "fn(R, T) -> R", 2, &step, span)?;
            for item in elements {
                total = host.call_value(&step, vec![total, item], span)?;
            }
            Ok(total)
        }
        "sorted" => {
            let args = expect_args(&method, args, 1, span)?;
            let by = args.remove(0);
            expect_callback(host, &method, "by", "fn(T, T) -> Bool", 2, &by, span)?;
            Ok(Value::Array(
                merge_sort(host, &method, elements, &by, span)?.into(),
            ))
        }
        // Only the four names above are routed here, and the shared table is
        // what says which four. Answering the way an unknown method is
        // answered keeps that a fact rather than a `panic!` nobody can reach.
        _ => Err(no_method(type_name, name, span)),
    }
}

/// A stable merge sort under a Cove callback.
///
/// Written out rather than handed to `slice::sort_by`, for two reasons
/// either of which would be enough on its own.
///
/// `by` can fail — it is a Cove closure, and a closure can raise or be
/// cancelled — and a `FnMut(&T, &T) -> Ordering` has nowhere to put a
/// failure. Smuggling one out through a cell and re-raising it afterwards
/// would mean the sort kept comparing after the run should have stopped.
///
/// And `by` can contradict itself. `slice::sort_by` panics when its
/// comparison function does not order the elements, and a panic in this
/// runtime means a broken invariant of the runtime; a program that wrote an
/// inconsistent comparison has broken nothing but its own ordering. A merge
/// answers some permutation instead, which is exactly what "no promise about
/// which" means, and it is the schema's stated behaviour rather than a
/// consequence of the algorithm that was to hand.
///
/// Bottom up: runs of one merged into runs of two, then four. The right
/// run's element is taken only when `by` says it comes *strictly* before the
/// left run's, which is what makes the sort stable — equal elements meet
/// with the earlier one on the left and the earlier one is kept.
fn merge_sort(
    host: &mut dyn Callable,
    method: &str,
    elements: Vec<Value>,
    by: &Value,
    span: Span,
) -> Result<Vec<Value>, RuntimeError> {
    let len = elements.len();
    let mut source = elements;
    let mut merged: Vec<Value> = Vec::with_capacity(len);
    let mut width = 1usize;
    while width < len {
        merged.clear();
        let mut start = 0usize;
        while start < len {
            let middle = (start + width).min(len);
            let end = (start + width * 2).min(len);
            let (mut left, mut right) = (start, middle);
            while left < middle && right < end {
                let verdict =
                    host.call_value(by, vec![source[right].clone(), source[left].clone()], span)?;
                if callback_bool(method, "by", &verdict, span)? {
                    merged.push(source[right].clone());
                    right += 1;
                } else {
                    merged.push(source[left].clone());
                    left += 1;
                }
            }
            merged.extend_from_slice(&source[left..middle]);
            merged.extend_from_slice(&source[right..end]);
            start = end;
        }
        std::mem::swap(&mut source, &mut merged);
        width *= 2;
    }
    Ok(source)
}

/// Holds a higher-order builtin's callback to the shape its signature
/// declares, before it is called rather than while it is being called.
///
/// The checker settles this for every program it accepts, so nothing a
/// checked program does reaches either failure. It is still asked here, and
/// asked once for the whole walk, because the two backends enter a closure
/// through different code — `Interpreter::call_value_slots` and
/// `Vm::call_from_host` — and a callback of the wrong arity would otherwise
/// be refused by whichever of those the run happened to be on, in that one's
/// words. One question asked in the one implementation both backends share
/// is one answer.
fn expect_callback(
    host: &dyn Callable,
    method: &str,
    parameter: &str,
    expected: &str,
    parameters: usize,
    value: &Value,
    span: Span,
) -> Result<(), RuntimeError> {
    match host.arity(value) {
        Some(found) if found == parameters => Ok(()),
        Some(found) => Err(RuntimeError::new(format!(
            "`{method}` expects `{expected}` for `{parameter}`, but found a function of {found} parameter(s)"
        ))
        .at(span)),
        None => Err(type_error(method, parameter, expected, value, span)),
    }
}

/// Reads a callback's answer as the `Bool` its signature declares.
///
/// Unreachable from a checked program for the same reason [`expect_callback`]
/// is, and stated for the same reason: the alternative is a `Bool` taken on
/// trust in the middle of a sort.
fn callback_bool(
    method: &str,
    parameter: &str,
    value: &Value,
    span: Span,
) -> Result<bool, RuntimeError> {
    match value {
        Value::Bool(answer) => Ok(*answer),
        other => Err(RuntimeError::new(format!(
            "`{method}` expects `{parameter}` to answer a `Bool`, but found `{}`",
            other.type_name()
        ))
        .at(span)),
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

/// `contains(element)` on a sequence: whether any element is `==` to it.
///
/// Equality is [`Value::eq_value`], the same one `==` is and the same one
/// `Map` and `Set` are keyed by, so a sequence answers membership exactly as
/// a comparison of the two values would. An empty receiver answers `false`,
/// and no argument can be refused: every value has an equality.
fn contains(
    method: &str,
    items: &[Value],
    args: &mut Vec<Value>,
    span: Span,
) -> Result<Value, RuntimeError> {
    let args = expect_args(method, args, 1, span)?;
    Ok(Value::Bool(
        items.iter().any(|item| item.eq_value(&args[0])),
    ))
}

/// `indexOf(element)` on a sequence: the first position holding a value `==`
/// to it, or `None`.
///
/// The same equality [`contains`] uses, so the two cannot disagree about
/// whether an element is there. An empty receiver and an element that is not
/// in the sequence both answer `None`, which is what `String.indexOf` and
/// `Array.get` answer a question with no position to name.
fn index_of_element(
    method: &str,
    items: &[Value],
    args: &mut Vec<Value>,
    span: Span,
) -> Result<Value, RuntimeError> {
    let args = expect_args(method, args, 1, span)?;
    Ok(items
        .iter()
        .position(|item| item.eq_value(&args[0]))
        .map(|at| Value::some(Value::Int(at as i64)))
        .unwrap_or_else(Value::none))
}

/// `slice(from, to)` on a sequence: the elements at `from..<to`.
///
/// Both bounds are clamped into `0..len` and a `to` at or below `from`
/// answers nothing, which is `String.slice`'s rule applied where the same
/// question arises rather than a second answer to it. So no argument can
/// stop the run: this refuses only a bound that is not an `Int` at all,
/// which is the receiver being called wrongly rather than an index being out
/// of range.
fn slice(
    method: &str,
    items: &[Value],
    args: &[Value],
    span: Span,
) -> Result<Rc<[Value]>, RuntimeError> {
    if args.len() != 2 {
        return Err(arity_error(method, 2, args.len(), span));
    }
    let bound = |at: usize, parameter: &str| match &args[at] {
        Value::Int(index) => Ok((*index).clamp(0, items.len() as i64) as usize),
        other => Err(type_error(method, parameter, "Int", other, span)),
    };
    let from = bound(0, "from")?;
    let to = bound(1, "to")?;
    if to <= from {
        return Ok(Rc::from([]));
    }
    Ok(Rc::from(&items[from..to]))
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

fn expect_args<'a>(
    method: &str,
    args: &'a mut Vec<Value>,
    count: usize,
    span: Span,
) -> Result<&'a mut Vec<Value>, RuntimeError> {
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

/// `String.fromCodePoint`: the one-character `String` a code point names, and
/// otherwise which of the two ways the number names no character.
///
/// The surrogates get a sentence of their own because they are the failure a
/// caller is most likely to be able to do something about. A format that
/// writes a code point in sixteen bits — JSON's `\u`, and UTF-16 generally —
/// writes anything past `0xFFFF` as a pair of them, so a program that reached
/// here with a `0xD800` has half of a character rather than a bad one, and
/// what it needs to hear is that the other half is still to come. Combining
/// the pair is arithmetic the program does before it calls this: there is no
/// half-formed value to hand back, because a Cove `String` is UTF-8 and holds
/// no such thing.
fn from_code_point(code_point: i64) -> Value {
    if (0xD800..=0xDFFF).contains(&code_point) {
        return Value::err(Value::error(format!(
            "`{code_point}` is a surrogate half, which is not a character on its own"
        )));
    }
    match u32::try_from(code_point).ok().and_then(char::from_u32) {
        Some(character) => Value::ok(Value::Str(one_character(character))),
        None => Value::err(Value::error(format!(
            "`{code_point}` is not a Unicode code point"
        ))),
    }
}

/// `Int.parseRadix` refused a `radix` outside `2..=36`.
///
/// A radix of 1 has no place value and a radix of 0 has no digits, and past
/// 36 there are no more letters to spell one with. None of those is text the
/// data got wrong, so none of them is an `Err`: it is the call that is wrong,
/// and the run stops the way it stops for an empty `String.split` separator.
fn radix_error(radix: i64, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "`Int.parseRadix` cannot read a number in radix `{radix}`"
    ))
    .at(span)
    .with_rule(
        "A radix is 2 through 36, which is as many digits as the ten numerals and the twenty-six letters afford.",
    )
    .with_help("pass a `radix` between 2 and 36, such as 16 for hexadecimal")
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

/// The one-character string `character` spells.
///
/// An ASCII character answers a string this thread already made, because
/// `chars()` is how a program takes text apart and a scanner asks for every
/// character of every line it reads. Allocating one string per character made
/// that the largest single source of allocation in `examples/cq`, and a
/// character's string is immutable and interchangeable, so there is no way for
/// a program to tell a shared one from a fresh one (issue #104).
///
/// The table is per thread rather than global because `Rc` is not shareable
/// across threads, which is the same reason `Value` uses `Rc` at all.
fn one_character(character: char) -> Rc<str> {
    thread_local! {
        static ASCII: [Rc<str>; 128] =
            std::array::from_fn(|byte| Rc::from((byte as u8 as char).to_string().as_str()));
    }
    if character.is_ascii() {
        return ASCII.with(|table| table[character as usize].clone());
    }
    character.to_string().into()
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
