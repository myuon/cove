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

use std::rc::Rc;

use cove_diag::Span;
use cove_ir::{DynamicKind, MinMax};
use cove_schema::builtins::{FreeBuiltinKind, FreeBuiltinSchema, MAP_ENTRY};

use crate::error::RuntimeError;
use crate::shared::SharedCell;
use crate::value::{
    ByteBufferStorage, InvalidKey, MapKey, RangeBounds, Repr, StructValue, Value, VectorStorage,
};

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
    ///
    /// **The arguments are taken out of `args`, which is left empty.** The
    /// vector belongs to the caller and comes back to it, capacity and all,
    /// which is what lets a per-element callback be invoked without
    /// allocating a vector per element: `walk_with` hands the same one
    /// down for the whole walk. Issue #193 is the cost that made that worth
    /// arranging — `map`, `filter`, `fold` and `sorted` built and dropped a
    /// `Vec<Value>` for every element they visited, which is the same shape
    /// the predecessor's own argument vectors had before #184 and on the
    /// one path that scheme could not reach.
    ///
    /// A caller that fails partway is still handed back a vector it may
    /// reuse: an implementation drains what it was given before it runs
    /// anything, so `args` is empty whether the call answered or raised.
    fn call_value(
        &mut self,
        callee: &Value,
        args: &mut Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError>;

    /// The number of parameters `callee` declares, when it is a closure.
    fn arity(&self, callee: &Value) -> Option<usize>;

    /// The independent copy `Snapshot` makes of one value.
    ///
    /// A hook on this trait, whose one implementor is `Interpreter`, because
    /// a struct and an enum answer their own `impl Snapshot for Type`
    /// through a declaration that only the interpreter reaches this way. The
    /// linear-memory backend puts the same recursion in the lowering
    /// instead, exactly because a builtin never calls back into Cove —
    /// `docs/LINEAR_VM.md` says why. [`snapshot`] recurses through here so
    /// that a `Vector` of structs reaches the interpreter's own answer for
    /// each one.
    fn snapshot(&mut self, value: &Value, span: Span) -> Result<Value, RuntimeError>;

    /// Where `case` stands among the cases the enum `type_name` declares, in
    /// declaration order.
    ///
    /// [ADR 0068](../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
    /// `core.dynamicCase` answers a case as the discriminant the machine
    /// stores, and a value here carries its case by name — so the position is
    /// a question about the declaration, which only the interpreter reaches.
    /// `Option` and `Result` are answered by [`call_core`] itself and never
    /// asked here.
    ///
    /// `None` for a type this caller cannot find; the default is that, so a
    /// caller with no declarations — a test's — reflects on the builtin enums
    /// alone.
    fn case_index(&self, type_name: &str, case: &str) -> Option<usize> {
        let _ = (type_name, case);
        None
    }
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
        Value(Repr::Unit)
        | Value(Repr::Bool(_))
        | Value(Repr::Int(_))
        | Value(Repr::Float(_))
        | Value(Repr::Duration(_))
        | Value(Repr::Str(_))
        | Value(Repr::Array(_))
        | Value(Repr::Map(_))
        | Value(Repr::Set(_))
        | Value(Repr::Range { .. }) => Ok(value.clone()),
        Value(Repr::Vector(storage)) => {
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
/// reports anything else, and the linear-memory backend reports it from the
/// instruction that does the appending. One wording, because it is one
/// failure.
pub fn spread_needs_a_sequence(span: Span) -> RuntimeError {
    RuntimeError::new("`...` spreads an `Array` or a `Vector`").at(span)
}

/// What a value that implements no `Snapshot` conformance is refused with.
///
/// Only the interpreter reaches it: a struct or an enum whose type wrote
/// none, met while walking a `Vector` whose element type is not known until
/// the value is. The linear-memory backend has no runtime version of this
/// question — a `Vector`'s element type is part of its layout, so whether it
/// snapshots itself or calls a conformance is decided when the walk is
/// lowered, not when it runs.
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
            let Value(Repr::Bool(holds)) = &args[0] else {
                return Err(declared_type_error(schema, 0, &args[0], span));
            };
            if *holds {
                return Ok(Value::ok(Value(Repr::Unit)));
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
                return Ok(Value::ok(Value(Repr::Unit)));
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
        "Shared" => Value(Repr::Shared(SharedCell::wrap(&value, span)?)),
        "Error" => match value {
            Value(Repr::Str(message)) => Value::error(message.to_string()),
            other => {
                return Err(declared_type_error(schema, 0, &other, span));
            }
        },
        // As in `call_assertion`: a name the table declares and nothing here
        // builds is what `tests/builtin_schema.rs` refuses to let happen.
        _ => return Err(RuntimeError::new(format!("unknown constructor `{name}`")).at(span)),
    })
}

/// `core.byteLength(text)`: a core intrinsic, which only a standard-library
/// module may call.
///
/// [ADR 0058](../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md)
/// keeps a public method's algorithm in Cove and only its smallest
/// representation-dependent operation here, so this is the oracle's whole
/// share of a method that moved: one small function per entry of
/// `cove_schema::builtins::CORE_INTRINSICS`, over values, and the standard
/// library's Cove body — which this interpreter runs as it runs any other —
/// around it. The interpreter asks only from a module
/// `cove_sema::stdlib::is_library_module` answers for, which is the question
/// the checker asked before it admitted the call.
pub fn call_core(
    host: &mut dyn Callable,
    name: &str,
    args: &mut Vec<Value>,
    span: Span,
) -> Result<Value, RuntimeError> {
    let Some(schema) = cove_schema::builtins::core_intrinsic(name) else {
        return Err(RuntimeError::new(format!("unknown core intrinsic `core.{name}`")).at(span));
    };
    let shown = format!("core.{name}");
    let args = expect_args(&shown, args, schema.arity(), span)?;
    match name {
        "byteLength" => {
            let Value(Repr::Str(text)) = &args[0] else {
                return Err(type_error(&shown, "text", "String", &args[0], span));
            };
            Ok(Value(Repr::Int(text.len() as i64)))
        }
        // ADR 0062's append, beneath `std.vector.push`: an ensure, a store at the
        // length, and a commit. The oracle has no capacity, so an ensure asks
        // only what the machine's `growableEnsure` refuses before it would grow
        // — a consumed vector, a negative room — and starts a reservation with
        // nothing staged: an earlier window's uncommitted elements are spare
        // room again, as they are above the machine's length. The store stages
        // (`vectorStore` below), and the commit publishes exactly what was
        // staged, so a lowering that commits an element it did not write
        // disagrees with this loudly rather than answering a zero.
        "vectorEnsure" => {
            let Value(Repr::Vector(storage)) = &args[0] else {
                return Err(type_error(&shown, "items", "Vector", &args[0], span));
            };
            check_consumed(storage, span)?;
            let Value(Repr::Int(additional)) = &args[1] else {
                return Err(type_error(&shown, "additional", "Int", &args[1], span));
            };
            if *additional < 0 {
                return Err(RuntimeError::new(format!(
                    "`growableEnsure` was asked for room for {additional} unit(s), and room is \
                     never negative"
                ))
                .at(span));
            }
            storage.staged.borrow_mut().clear();
            Ok(Value(Repr::Unit))
        }
        "vectorCommit" => {
            let Value(Repr::Vector(storage)) = &args[0] else {
                return Err(type_error(&shown, "items", "Vector", &args[0], span));
            };
            check_consumed(storage, span)?;
            let Value(Repr::Int(count)) = &args[1] else {
                return Err(type_error(&shown, "count", "Int", &args[1], span));
            };
            let mut staged = storage.staged.borrow_mut();
            let mut elements = storage.elements.borrow_mut();
            match usize::try_from(*count) {
                Ok(count) if count <= staged.len() => {
                    elements.extend(staged.drain(..count));
                    staged.clear();
                    Ok(Value(Repr::Unit))
                }
                _ => Err(RuntimeError::new(format!(
                    "`growableCommit` would publish {count} unit(s) onto a length of {} with {} \
                     written above it, and a commit publishes only units its window wrote",
                    elements.len(),
                    staged.len()
                ))
                .at(span)),
            }
        }
        // The element read and write beneath `std.vector.set`. The body holds the
        // index below `items.length()` before either is asked, so the refusal
        // here is the machine's `LoadElem`/`StoreElem` bound and not a sentence a
        // checked program reaches.
        "vectorLoad" => {
            let Value(Repr::Vector(storage)) = &args[0] else {
                return Err(type_error(&shown, "items", "Vector", &args[0], span));
            };
            check_consumed(storage, span)?;
            let elements = storage.elements.borrow();
            let at = core_index(&shown, &args[1], elements.len(), span)?;
            Ok(elements[at].clone())
        }
        //
        // A store at or above the length is a push's write, into the room an
        // ensure made: at `length + staged` it stages one more element, and
        // below that it replaces one already staged. Only an index past the
        // staged suffix is refused, which on the machine is a write past the
        // room — `vectorEnsure` above says why the oracle cannot be exact about
        // where the machine's capacity ends.
        "vectorStore" => {
            let value = args.remove(2);
            let Value(Repr::Vector(storage)) = &args[0] else {
                return Err(type_error(&shown, "items", "Vector", &args[0], span));
            };
            check_consumed(storage, span)?;
            let mut elements = storage.elements.borrow_mut();
            let mut staged = storage.staged.borrow_mut();
            let len = elements.len();
            let at = core_index(&shown, &args[1], len + staged.len() + 1, span)?;
            if at < len {
                elements[at] = value;
            } else if at - len < staged.len() {
                staged[at - len] = value;
            } else {
                staged.push(value);
            }
            Ok(Value(Repr::Unit))
        }
        // `std.vector.freeze`'s whole body: the elements taken out of the
        // storage as the array, and the storage marked consumed.
        //
        // No handle is counted. The tree-walking oracle once refused a freeze
        // whose `Rc` another alias shared, which answered ADR 0001's question at
        // run time and in this evaluator only; `cove_sema::unique` answers it
        // before either evaluator runs (#240), and it is authoritative (#378,
        // Q9). A standard-library body is handed the vector by value — a second
        // handle by construction — so counting here would refuse every call.
        "vectorFinish" => {
            let Value(Repr::Vector(storage)) = &args[0] else {
                return Err(type_error(&shown, "items", "Vector", &args[0], span));
            };
            check_consumed(storage, span)?;
            let elements = storage.elements.take();
            *storage.frozen.borrow_mut() = true;
            Ok(Value(Repr::Array(elements.into())))
        }
        // Beneath `std.vector.pop` and `std.vector.remove`: the length lowered,
        // the elements above it dropped. The body computed `len` from the
        // length it read, so the refusal is the machine's truncate invariant.
        "vectorTruncate" => {
            let Value(Repr::Vector(storage)) = &args[0] else {
                return Err(type_error(&shown, "items", "Vector", &args[0], span));
            };
            check_consumed(storage, span)?;
            let Value(Repr::Int(len)) = &args[1] else {
                return Err(type_error(&shown, "len", "Int", &args[1], span));
            };
            let mut elements = storage.elements.borrow_mut();
            let had = elements.len();
            match usize::try_from(*len) {
                Ok(len) if len <= had => {
                    elements.truncate(len);
                    Ok(Value(Repr::Unit))
                }
                _ => Err(RuntimeError::new(format!(
                    "`growableTruncate` would take a length of {had} to {len}, and a truncate \
                     only lowers a length"
                ))
                .at(span)),
            }
        }
        // `std.vector.remove`'s shift of the tail: memmove over the elements,
        // both ranges inside the length the body read.
        "vectorMove" => {
            let Value(Repr::Vector(storage)) = &args[0] else {
                return Err(type_error(&shown, "items", "Vector", &args[0], span));
            };
            check_consumed(storage, span)?;
            let mut elements = storage.elements.borrow_mut();
            let len = elements.len();
            let from = core_range(
                &shown,
                "runCopy",
                "element(s)",
                &args[2],
                &args[3],
                len,
                span,
            )?;
            let to = core_range(
                &shown,
                "runCopy",
                "element(s)",
                &args[1],
                &args[3],
                len,
                span,
            )?;
            let moved: Vec<Value> = elements[from].to_vec();
            elements[to].clone_from_slice(&moved);
            Ok(Value(Repr::Unit))
        }
        // The copies beneath `std.array.slice`, `std.vector.slice` and
        // `std.vector.toArray`. Each body has clamped its range into the
        // sequence first, so the refusal here is the machine's `run-slice`
        // bound and not a sentence a checked program reaches.
        "arraySlice" => {
            let Value(Repr::Array(items)) = &args[0] else {
                return Err(type_error(&shown, "items", "Array", &args[0], span));
            };
            let range = core_range(
                &shown,
                "runSlice",
                "element(s)",
                &args[1],
                &args[2],
                items.len(),
                span,
            )?;
            Ok(Value(Repr::Array(Rc::from(&items[range]))))
        }
        "vectorSlice" => {
            let Value(Repr::Vector(storage)) = &args[0] else {
                return Err(type_error(&shown, "items", "Vector", &args[0], span));
            };
            check_consumed(storage, span)?;
            let elements = storage.elements.borrow();
            let range = core_range(
                &shown,
                "runSlice",
                "element(s)",
                &args[1],
                &args[2],
                elements.len(),
                span,
            )?;
            Ok(Value(Repr::Array(Rc::from(&elements[range]))))
        }
        // `std.array.toVector`'s whole body: a growable copy of the elements
        // as they are, from the running task's heap like every other `Vector`.
        "arrayToVector" => {
            let Value(Repr::Array(items)) = &args[0] else {
                return Err(type_error(&shown, "items", "Array", &args[0], span));
            };
            Ok(host.allocate_vector(items.to_vec()))
        }
        // The copy beneath `std.string.sliceBytes`, which has held the range
        // inside the string and both ends at character boundaries before it
        // asks. So the refusals here are the machine's `run-slice` bound, and a
        // cut inside a character — which `str` would refuse by panicking — is a
        // broken invariant said in words rather than a sentence a checked program
        // reaches.
        "stringSlice" => {
            let Value(Repr::Str(text)) = &args[0] else {
                return Err(type_error(&shown, "text", "String", &args[0], span));
            };
            let range = core_range(
                &shown,
                "runSlice",
                "byte(s)",
                &args[1],
                &args[2],
                text.len(),
                span,
            )?;
            match text.get(range.clone()) {
                Some(cut) => Ok(Value(Repr::Str(cut.into()))),
                None => Err(RuntimeError::new(format!(
                    "`runSlice` cuts `{}..{}` of a string inside a character",
                    range.start, range.end
                ))
                .at(span)),
            }
        }
        // ADR 0065's run search, beneath `std.string.contains`: the first byte
        // offset at or after `from` where the needle's bytes occur, or -1.
        //
        // Over `as_bytes` rather than over `str::find`, and the difference is
        // not a nicety: `&text[from..]` is a panic when `from` is not a
        // character boundary, and the instruction admits every `from` in
        // `0 ..= byteLength` because it has no notion of a character at all.
        // `crate::find` is the matcher the linear-memory backend runs, called
        // here without its safepoints, so the two tiers cannot come to
        // different answers about what a run search means.
        //
        // A `from` outside the text stops the run, as an out-of-range slice
        // does: the body above this has held it inside, and `contains` passes
        // zero.
        "stringFind" => {
            let Value(Repr::Str(text)) = &args[0] else {
                return Err(type_error(&shown, "text", "String", &args[0], span));
            };
            let Value(Repr::Str(needle)) = &args[1] else {
                return Err(type_error(&shown, "needle", "String", &args[1], span));
            };
            let Value(Repr::Int(from)) = &args[2] else {
                return Err(type_error(&shown, "from", "Int", &args[2], span));
            };
            let len = text.len();
            if *from < 0 || *from > len as i64 {
                return Err(RuntimeError::new(format!(
                    "`runFind` starts at {from} of a haystack of {len} byte(s), and a search \
                     starts at 0 to that length"
                ))
                .at(span));
            }
            let from = *from as usize;
            Ok(Value(Repr::Int(crate::find::find_bytes(
                text.as_bytes(),
                needle.as_bytes(),
                from,
            ))))
        }
        // `std.stringbuilder`'s `withCapacity`: ADR 0052's owner, empty, with
        // room for `capacity` bytes.
        //
        // A `Vec<u8>` *is* the ADR's owner-over-a-replaceable-store: a stable
        // handle whose run it may reallocate on growth, which is why this side
        // has one object where the linear-memory backend has two. So the
        // capacity is passed to `Vec::with_capacity` and is a hint in exactly
        // the ADR's sense — nothing a program can ask reads it back, and
        // exceeding it grows.
        //
        // A negative capacity is refused rather than clamped, for the reason
        // `Machine::alloc_buffer` refuses one: it is nonsense rather than a
        // small number, and answering an empty buffer would make the one
        // arithmetic a caller could not have meant a silent success. The
        // sentence is the one this evaluator said before the buffer became the
        // standard library's own.
        "bytesAllocate" => {
            let Value(Repr::Int(capacity)) = &args[0] else {
                return Err(type_error(&shown, "capacity", "Int", &args[0], span));
            };
            let Ok(capacity) = usize::try_from(*capacity) else {
                return Err(RuntimeError::new(format!(
                    "`ByteBuffer.allocate`'s capacity is `{capacity}`, and a capacity is 0 or more"
                ))
                .at(span)
                .with_rule(
                    "A capacity is a hint for the first allocation, so it is a count of bytes.",
                )
                .with_help("pass 0 for a buffer whose size is not worth estimating"));
            };
            Ok(Value(Repr::ByteBuffer(ByteBufferStorage::new(capacity))))
        }
        // ADR 0062's append over a byte run, beneath `std.stringbuilder`'s
        // `appendByteInto` and `appendText`: an ensure, a store or a copy at the
        // length, and a commit — `vectorEnsure` above, for bytes, and staged the
        // same way, so a commit publishes exactly what its window wrote. The
        // refusals are the machine's, in its words: a consumed buffer is the
        // ensure's, a byte that is not one is the store's, and a range outside
        // the text is the copy's.
        "bytesEnsure" => {
            let Value(Repr::ByteBuffer(storage)) = &args[0] else {
                return Err(type_error(&shown, "buffer", "ByteBuffer", &args[0], span));
            };
            check_buffer_live(storage, "growableEnsure", span)?;
            let Value(Repr::Int(additional)) = &args[1] else {
                return Err(type_error(&shown, "additional", "Int", &args[1], span));
            };
            if *additional < 0 {
                return Err(RuntimeError::new(format!(
                    "`growableEnsure` was asked for room for {additional} unit(s), and room is \
                     never negative"
                ))
                .at(span));
            }
            storage.staged.borrow_mut().clear();
            Ok(Value(Repr::Unit))
        }
        "bytesStore" => {
            let Value(Repr::ByteBuffer(storage)) = &args[0] else {
                return Err(type_error(&shown, "buffer", "ByteBuffer", &args[0], span));
            };
            check_buffer_live(storage, "runStore", span)?;
            let Value(Repr::Int(value)) = &args[2] else {
                return Err(type_error(&shown, "byte", "Int", &args[2], span));
            };
            let len = storage.len();
            let mut staged = storage.staged.borrow_mut();
            let at = core_index(&shown, &args[1], len + staged.len() + 1, span)?;
            let Ok(byte) = u8::try_from(*value) else {
                return Err(RuntimeError::new(format!(
                    "`runStore`'s value is `{value}`, and a byte is 0 to 255"
                ))
                .at(span));
            };
            if at < len {
                storage.bytes.borrow_mut()[at] = byte;
            } else if at - len < staged.len() {
                staged[at - len] = byte;
            } else {
                staged.push(byte);
            }
            Ok(Value(Repr::Unit))
        }
        "bytesCopy" => {
            let Value(Repr::ByteBuffer(storage)) = &args[0] else {
                return Err(type_error(&shown, "buffer", "ByteBuffer", &args[0], span));
            };
            check_buffer_live(storage, "runCopy", span)?;
            let Value(Repr::Str(text)) = &args[2] else {
                return Err(type_error(&shown, "text", "String", &args[2], span));
            };
            let Value(Repr::Int(from)) = &args[3] else {
                return Err(type_error(&shown, "from", "Int", &args[3], span));
            };
            let Value(Repr::Int(count)) = &args[4] else {
                return Err(type_error(&shown, "count", "Int", &args[4], span));
            };
            if *count < 0 {
                return Err(RuntimeError::new(format!(
                    "`runCopy`'s count is `{count}`, and a copy cannot have a negative length"
                ))
                .at(span));
            }
            let bytes = text.as_bytes();
            let src_len = bytes.len() as i64;
            if *from < 0 || from.checked_add(*count).is_none_or(|end| end > src_len) {
                return Err(RuntimeError::new(format!(
                    "`runCopy` reads {count} byte(s) from {from} of a source of {src_len}"
                ))
                .at(span));
            }
            let len = storage.len();
            let mut staged = storage.staged.borrow_mut();
            // The oracle has no capacity, so the one destination it can vouch
            // for is the end of what is already staged: a copy there stages the
            // bytes, and one anywhere else is a write past the room.
            let Value(Repr::Int(at)) = &args[1] else {
                return Err(type_error(&shown, "at", "Int", &args[1], span));
            };
            let end = len + staged.len();
            if usize::try_from(*at) != Ok(end) {
                return Err(RuntimeError::new(format!(
                    "`runCopy` writes {count} byte(s) to {at} of a buffer whose room begins at {end}"
                ))
                .at(span));
            }
            staged.extend_from_slice(&bytes[*from as usize..(*from + *count) as usize]);
            Ok(Value(Repr::Unit))
        }
        "bytesCommit" => {
            let Value(Repr::ByteBuffer(storage)) = &args[0] else {
                return Err(type_error(&shown, "buffer", "ByteBuffer", &args[0], span));
            };
            check_buffer_live(storage, "growableCommit", span)?;
            let Value(Repr::Int(count)) = &args[1] else {
                return Err(type_error(&shown, "count", "Int", &args[1], span));
            };
            let mut staged = storage.staged.borrow_mut();
            let mut bytes = storage.bytes.borrow_mut();
            match usize::try_from(*count) {
                Ok(count) if count <= staged.len() => {
                    bytes.extend(staged.drain(..count));
                    staged.clear();
                    Ok(Value(Repr::Unit))
                }
                _ => Err(RuntimeError::new(format!(
                    "`growableCommit` would publish {count} unit(s) onto a length of {} with {} \
                     written above it, and a commit publishes only units its window wrote",
                    bytes.len(),
                    staged.len()
                ))
                .at(span)),
            }
        }
        // Beneath `StringBuilder.appendSlice`, on the path where its range is
        // not one: what is wrong with it, as the run stops.
        //
        // The five questions are Cove — `std.stringbuilder`'s `appendRange`
        // asks them, in `String.sliceBytes`' shape — so this decides nothing
        // about whether the range is legal. It says which of them failed, in
        // `sliceBytes`' words, which ADR 0052 requires in as many words and
        // which `std.string.sliceBytes`' `refuseRange` writes out again in
        // Cove. What differs is what a refusal *is*: `sliceBytes` answers a
        // `Result` because a caller asked for a value, and this stops the run
        // because a builder's append has no `Result` to answer.
        "refuseByteRange" => {
            let Value(Repr::Str(text)) = &args[0] else {
                return Err(type_error("appendSlice", "text", "String", &args[0], span));
            };
            let Value(Repr::Int(from)) = &args[1] else {
                return Err(type_error("appendSlice", "from", "Int", &args[1], span));
            };
            let Value(Repr::Int(to)) = &args[2] else {
                return Err(type_error("appendSlice", "to", "Int", &args[2], span));
            };
            Err(RuntimeError::new(wrong_byte_range(text, *from, *to)).at(span))
        }
        // Any standard-library body's own refusal, worded in Cove out of
        // values it computed rather than by this interpreter. The compiled
        // tier's `TRAP` arm reads the same three strings out of the same
        // three slots `cove_ir::Inst::Trap` carries, and must answer exactly
        // this: ADR 0034 makes this evaluator the definition of what a Cove
        // program means, so an empty `rule` or `help` is absent here the
        // same way it is there, not a blank line.
        "refuse" => {
            let Value(Repr::Str(message)) = &args[0] else {
                return Err(type_error(&shown, "message", "String", &args[0], span));
            };
            let Value(Repr::Str(rule)) = &args[1] else {
                return Err(type_error(&shown, "rule", "String", &args[1], span));
            };
            let Value(Repr::Str(help)) = &args[2] else {
                return Err(type_error(&shown, "help", "String", &args[2], span));
            };
            let mut error = RuntimeError::new(message.to_string());
            if !rule.is_empty() {
                error = error.with_rule(rule.to_string());
            }
            if !help.is_empty() {
                error = error.with_help(help.to_string());
            }
            Err(error.at(span))
        }
        // `StringBuilder.finish`'s whole body, which consumes: the bytes are
        // validated once and become the `String`, and the owner is emptied so a
        // read after it is refused rather than answered as an empty buffer.
        //
        // **There is no uniqueness check here, and that is deliberate.** The
        // standard library's body is handed the buffer by value, which is
        // already a second handle, so counting here would refuse every call —
        // and `vectorFinish` does not count either. Uniqueness is
        // `cove_sema::unique`'s proof for both backends, which records this
        // call as the transition; what both evaluators keep is the liveness
        // check. The refusal is the machine's sentence alone:
        // `growable_finish` raises it with no rule and no help.
        "bytesFinish" => {
            let Value(Repr::ByteBuffer(storage)) = &args[0] else {
                return Err(type_error(&shown, "buffer", "ByteBuffer", &args[0], span));
            };
            check_buffer_live(storage, "finish", span)?;
            let bytes = storage.bytes.take();
            *storage.finished.borrow_mut() = true;
            match String::from_utf8(bytes) {
                Ok(text) => Ok(Value(Repr::Str(text.into()))),
                Err(_) => {
                    Err(RuntimeError::new("this string's bytes are not valid UTF-8").at(span))
                }
            }
        }
        // `StringBuilder.length`: the *logical* length, which is the only length
        // a program can ask about. `Vec`'s capacity is unobservable here for the
        // reason the store's header length is unobservable there — ADR 0052's
        // "capacity is not an Array length".
        "bytesLength" => {
            let Value(Repr::ByteBuffer(storage)) = &args[0] else {
                return Err(type_error(&shown, "buffer", "ByteBuffer", &args[0], span));
            };
            check_buffer_live(storage, "length", span)?;
            Ok(Value(Repr::Int(storage.len() as i64)))
        }
        // `std.array.length` and `std.vector.length`'s whole bodies. The
        // vector's is refused once a finish consumed it, in the words every
        // other vector core intrinsic here refuses it in.
        "arrayLength" => {
            let Value(Repr::Array(items)) = &args[0] else {
                return Err(type_error(&shown, "items", "Array", &args[0], span));
            };
            Ok(Value(Repr::Int(items.len() as i64)))
        }
        "vectorLength" => {
            let Value(Repr::Vector(storage)) = &args[0] else {
                return Err(type_error(&shown, "items", "Vector", &args[0], span));
            };
            check_consumed(storage, span)?;
            Ok(Value(Repr::Int(storage.len() as i64)))
        }
        // ADR 0059's value order, beneath a keyed search in the standard
        // library: `MapKey`'s own `Ord`, which is the order the oracle keeps a
        // `Map`'s keys and a `Set`'s members in. The body admitted the key
        // first, so the conversion refusing is a key no checked program hands
        // this.
        "order" => {
            let a = MapKey::from_value(&args[0])
                .map_err(|invalid| invalid_key_error(&shown, "key", &invalid, span))?;
            let b = MapKey::from_value(&args[1])
                .map_err(|invalid| invalid_key_error(&shown, "key", &invalid, span))?;
            Ok(Value(Repr::Int(match a.cmp(&b) {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            })))
        }
        // The admission a keyed method asks of its argument before anything
        // is compared, in the method's words and naming the key by its role —
        // the refusal every `Map` and `Set` arm made before the search moved.
        "admitKey" => match MapKey::from_value(&args[0]) {
            Ok(_) => Ok(Value(Repr::Unit)),
            Err(invalid) => {
                let (method, role) = core_names(&shown, &args[1], &args[2], span)?;
                Err(invalid_key_error(&method, &role, &invalid, span))
            }
        },
        // The element reads of a sorted run: the member or the entry at a
        // position the body has already held below the length. A slice indexes
        // in one step, which is why the oracle keeps one.
        "memberAt" => {
            let Value(Repr::Set(items)) = &args[0] else {
                return Err(type_error(&shown, "members", "Set", &args[0], span));
            };
            let at = core_index(&shown, &args[1], items.len(), span)?;
            Ok(items[at].to_value())
        }
        "entryAt" => {
            let Value(Repr::Map(entries)) = &args[0] else {
                return Err(type_error(&shown, "entries", "Map", &args[0], span));
            };
            let at = core_index(&shown, &args[1], entries.len(), span)?;
            let (key, value) = &entries[at];
            Ok(map_entry(key, value))
        }
        // The copy beneath `std.set.toArray`: the members, in the order the set
        // keeps them, as the values they are.
        "setSlice" => {
            let Value(Repr::Set(items)) = &args[0] else {
                return Err(type_error(&shown, "items", "Set", &args[0], span));
            };
            let range = core_range(
                &shown,
                "runSlice",
                "element(s)",
                &args[1],
                &args[2],
                items.len(),
                span,
            )?;
            Ok(Value(Repr::Array(
                items[range].iter().map(MapKey::to_value).collect(),
            )))
        }
        // The growable vector a keyed update is built in, with room for the run
        // it will hold. A `Vec`'s capacity is a hint here as it is for a byte
        // buffer: nothing a program asks reads it back. A negative capacity is
        // the machine's allocation refusal there and a broken invariant here.
        "vectorWithCapacity" => {
            let Value(Repr::Int(capacity)) = &args[0] else {
                return Err(type_error(&shown, "capacity", "Int", &args[0], span));
            };
            let Ok(capacity) = usize::try_from(*capacity) else {
                return Err(RuntimeError::new(format!(
                    "`{shown}`'s capacity is `{capacity}`, and a capacity is 0 or more"
                ))
                .at(span));
            };
            Ok(host.allocate_vector(Vec::with_capacity(capacity)))
        }
        // A range of a sorted run written into the room a `vectorEnsure` made:
        // a member as the value it is, an entry as the `MapEntry`
        // `core.entryAt` answers. The body held the range inside the run, so
        // the refusal is the machine's `run-copy` bound.
        //
        // It stages what it copies rather than publishing it, as `bytesCopy`
        // does and for ADR 0062's reason: the length moves at `vectorCommit`
        // and nowhere else. The oracle has no capacity, so the one destination
        // it can vouch for is the end of what is already staged.
        "vectorCopyFromSet" | "vectorCopyFromMap" => {
            let Value(Repr::Vector(storage)) = &args[0] else {
                return Err(type_error(&shown, "out", "Vector", &args[0], span));
            };
            check_consumed(storage, span)?;
            let copied: Vec<Value> = match &args[2] {
                Value(Repr::Set(items)) if name == "vectorCopyFromSet" => {
                    let range = core_range(
                        &shown,
                        "runCopy",
                        "element(s)",
                        &args[3],
                        &args[4],
                        items.len(),
                        span,
                    )?;
                    items[range].iter().map(MapKey::to_value).collect()
                }
                Value(Repr::Map(entries)) if name == "vectorCopyFromMap" => {
                    let range = core_range(
                        &shown,
                        "runCopy",
                        "element(s)",
                        &args[3],
                        &args[4],
                        entries.len(),
                        span,
                    )?;
                    entries[range]
                        .iter()
                        .map(|(key, value)| map_entry(key, value))
                        .collect()
                }
                other => {
                    let (role, family) = match name {
                        "vectorCopyFromSet" => ("items", "Set"),
                        _ => ("entries", "Map"),
                    };
                    return Err(type_error(&shown, role, family, other, span));
                }
            };
            let len = storage.elements.borrow().len();
            let mut staged = storage.staged.borrow_mut();
            let Value(Repr::Int(at)) = &args[1] else {
                return Err(type_error(&shown, "at", "Int", &args[1], span));
            };
            let end = len + staged.len();
            if usize::try_from(*at) != Ok(end) {
                return Err(RuntimeError::new(format!(
                    "`runCopy` writes {} element(s) to {at} of a store whose room begins at {end}",
                    copied.len()
                ))
                .at(span));
            }
            staged.extend(copied);
            Ok(Value(Repr::Unit))
        }
        // The keyed finish: the vector's elements taken out as the sorted run of
        // a `Set` or a `Map`, and the vector consumed, as `vectorFinish` takes
        // them as an `Array`. The body built the run ascending and distinct;
        // under `debug_assertions` that is asserted here — the machine asserts it
        // only in its own tests, where the cost does not reach a measurement
        // (#378, Q4.10).
        "setFinish" | "mapFinish" => {
            let Value(Repr::Vector(storage)) = &args[0] else {
                return Err(type_error(&shown, "run", "Vector", &args[0], span));
            };
            check_consumed(storage, span)?;
            let elements = storage.elements.take();
            *storage.frozen.borrow_mut() = true;
            let key_of = |value: &Value| {
                MapKey::from_value(value)
                    .map_err(|invalid| invalid_key_error(&shown, "key", &invalid, span))
            };
            if name == "setFinish" {
                let members = elements
                    .iter()
                    .map(key_of)
                    .collect::<Result<Vec<MapKey>, _>>()?;
                debug_assert!(
                    members.windows(2).all(|pair| pair[0] < pair[1]),
                    "`core.setFinish` was handed a run that is not ascending and distinct"
                );
                return Ok(Value(Repr::Set(members.into())));
            }
            let mut pairs = Vec::with_capacity(elements.len());
            for element in &elements {
                let Value(Repr::Struct(entry)) = element else {
                    return Err(expects_map_entry(element, span));
                };
                let key = entry.get("key").expect("MapEntry always has a `key` field");
                let value = entry
                    .get("value")
                    .expect("MapEntry always has a `value` field");
                pairs.push((key_of(key)?, value.clone()));
            }
            debug_assert!(
                pairs.windows(2).all(|pair| pair[0].0 < pair[1].0),
                "`core.mapFinish` was handed a run that is not ascending and distinct"
            );
            Ok(Value(Repr::Map(pairs.into())))
        }
        // ADR 0068's structural observations. A view here is simply the
        // erased value it denotes: `Value::erased` is what "a view never
        // denotes a box" means in a tree of values, and a child is the value
        // it is — the oracle's allocation is not what the ADR's gate is about.
        "dynamicOpen" => Ok(args[0].erased().clone()),
        "dynamicKind" => Ok(Value(Repr::Int(dynamic_kind(&args[0]).code()))),
        "dynamicSameType" => Ok(Value(Repr::Bool(dynamic_same_type(&args[0], &args[1])))),
        "dynamicBool" | "dynamicInt" | "dynamicFloat" | "dynamicDuration" | "dynamicString" => {
            dynamic_read(name, &args[0]).map_err(|error| error.at(span))
        }
        "dynamicCase" => dynamic_case(host, &args[0]).map_err(|error| error.at(span)),
        "dynamicChildCount" => Ok(Value(Repr::Int(dynamic_count(&args[0]) as i64))),
        "dynamicChild" => {
            let count = dynamic_count(&args[0]);
            let Value(Repr::Int(index)) = &args[1] else {
                return Err(type_error(&shown, "index", "Int", &args[1], span));
            };
            match usize::try_from(*index) {
                Ok(at) if at < count => Ok(dynamic_child(&args[0], at)),
                _ => Err(dynamic_internal(format!(
                    "child {index} of a dynamic view with {count} children was asked"
                ))
                .at(span)),
            }
        }
        // A name the table declares and nothing here executes. No program can
        // reach one of these from its own modules, so the check that every
        // entry has a body here is `vm::differential`'s, which calls each
        // from a standard-library module on both evaluators.
        _ => Err(RuntimeError::new(format!("unknown core intrinsic `{shown}`")).at(span)),
    }
}

/// The [`DynamicKind`] of the value a view denotes: `cove_ir`'s one table,
/// read off a `Value`'s variant the way the machine reads it off a layout's
/// shape.
///
/// `Dyn` is looked through first, so a view never denotes a box here either.
/// A struct is a struct whatever its name — `Error` and `MapEntry` included —
/// and whether or not it is `opaque`: its fields are the declaring module's,
/// and the only reader of a view is the standard library's own walks, which
/// compare them as `eq_value` does. A host operation used as a value is a
/// function, as the closure the machine builds for one is; and every handle,
/// module, type and cell is opaque (ADR 0068, Decision 7).
pub(crate) fn dynamic_kind(value: &Value) -> DynamicKind {
    match value.erased() {
        Value(Repr::Unit) => DynamicKind::Unit,
        Value(Repr::Bool(_)) => DynamicKind::Bool,
        Value(Repr::Int(_)) => DynamicKind::Int,
        Value(Repr::Float(_)) => DynamicKind::Float,
        Value(Repr::Duration(_)) => DynamicKind::Duration,
        Value(Repr::Str(_)) => DynamicKind::String,
        Value(Repr::Struct(_)) => DynamicKind::Struct,
        Value(Repr::Enum(_)) => DynamicKind::Enum,
        Value(Repr::Array(_)) => DynamicKind::Array,
        Value(Repr::Vector(_)) => DynamicKind::Vector,
        Value(Repr::Set(_)) => DynamicKind::Set,
        Value(Repr::Map(_)) => DynamicKind::Map,
        Value(Repr::Range { .. }) => DynamicKind::Range,
        Value(Repr::Closure(_) | Repr::HostFn(_)) => DynamicKind::Function,
        Value(
            Repr::ByteBuffer(_)
            | Repr::Dyn(_)
            | Repr::HostModule(_)
            | Repr::Resource(_)
            | Repr::Type(_)
            | Repr::TaskScope(_)
            | Repr::Task(_)
            | Repr::Shared(_),
        ) => DynamicKind::Opaque,
    }
}

/// `core.dynamicSameType`: equal kinds and, for the nominal three, equal
/// declared names.
///
/// **The name is the one the value carries, and it is qualified** —
/// `m.geometry.Point`, `Option`, `Error` — which is the machine's layout name
/// too once an instantiation is left off it. A value here carries no type
/// arguments at all, so an `Option<Int>` and an `Option<String>` are one type
/// without anything being erased.
pub(crate) fn dynamic_same_type(a: &Value, b: &Value) -> bool {
    let kind = dynamic_kind(a);
    kind == dynamic_kind(b) && (!kind.is_nominal() || dynamic_type_name(a) == dynamic_type_name(b))
}

/// The declared name a nominal view's type has, with any instantiation left
/// off.
fn dynamic_type_name(value: &Value) -> Option<&str> {
    match value.erased() {
        Value(Repr::Struct(s)) => Some(cove_ir::dynamic::declared_name(&s.type_name)),
        Value(Repr::Enum(e)) => Some(cove_ir::dynamic::declared_name(&e.type_name)),
        Value(Repr::Range { .. }) => Some("Range"),
        _ => None,
    }
}

/// `core.dynamicBool` and the four beside it: the scalar a view holds, held to
/// the kind `name` reads.
fn dynamic_read(name: &str, view: &Value) -> Result<Value, RuntimeError> {
    let value = view.erased();
    let wanted = match name {
        "dynamicBool" => DynamicKind::Bool,
        "dynamicInt" => DynamicKind::Int,
        "dynamicFloat" => DynamicKind::Float,
        "dynamicDuration" => DynamicKind::Duration,
        _ => DynamicKind::String,
    };
    let found = dynamic_kind(value);
    if found == wanted {
        return Ok(value.clone());
    }
    Err(dynamic_internal(format!(
        "a dynamic view of a {} was read as a `{}`",
        found.name(),
        wanted.name()
    )))
}

/// `core.dynamicCase`: the position of an enum view's case in its declaration.
///
/// `Option` and `Result` are the lowering's own order — `None` then `Some`,
/// `Ok` then `Err`, as `cove_ir`'s `Shapes::of` lays them out — and every
/// other enum's is its declaration's, which the caller finds.
fn dynamic_case(host: &dyn Callable, view: &Value) -> Result<Value, RuntimeError> {
    let Value(Repr::Enum(e)) = view.erased() else {
        return Err(dynamic_internal(format!(
            "the case of a dynamic view of a {} was asked",
            dynamic_kind(view).name()
        )));
    };
    let index = match (&*e.type_name, &*e.case) {
        ("Option", "None") | ("Result", "Ok") => Some(0),
        ("Option", "Some") | ("Result", "Err") => Some(1),
        (type_name, case) => host.case_index(type_name, case),
    };
    match index {
        Some(index) => Ok(Value(Repr::Int(index as i64))),
        None => Err(dynamic_internal(format!(
            "the case `{}` of `{}` has no position this evaluator can find",
            e.case, e.type_name
        ))),
    }
}

/// `core.dynamicChildCount`: how many children a view has, in
/// [`dynamic_child`]'s order.
pub(crate) fn dynamic_count(view: &Value) -> usize {
    match view.erased() {
        Value(Repr::Struct(s)) => s.fields.len(),
        Value(Repr::Enum(e)) => e.payload.len(),
        Value(Repr::Array(items)) => items.len(),
        Value(Repr::Vector(storage)) => storage.elements.borrow().len(),
        Value(Repr::Set(items)) => items.len(),
        Value(Repr::Map(entries)) => 2 * entries.len(),
        Value(Repr::Range { .. }) => 3,
        _ => 0,
    }
}

/// `core.dynamicChild`: child `at` of a view, which [`call_core`] has already
/// held below [`dynamic_count`].
///
/// The machine's canonical order: a struct's fields in declaration order, a
/// case's payload, a sequence's or a set's elements, a map's entries as key
/// then value, and a range's `start`, `end` and `inclusive` — the three fields
/// the program's `Range` struct declares. A set member and a map key are
/// [`MapKey`]s here and are read back as the values they were.
pub(crate) fn dynamic_child(view: &Value, at: usize) -> Value {
    match view.erased() {
        Value(Repr::Struct(s)) => s.fields[at].1.erased().clone(),
        Value(Repr::Enum(e)) => e.payload[at].erased().clone(),
        Value(Repr::Array(items)) => items[at].erased().clone(),
        Value(Repr::Vector(storage)) => storage.elements.borrow()[at].erased().clone(),
        Value(Repr::Set(items)) => items[at].to_value(),
        Value(Repr::Map(entries)) => {
            let (key, value) = &entries[at / 2];
            if at.is_multiple_of(2) {
                key.to_value()
            } else {
                value.erased().clone()
            }
        }
        Value(Repr::Range {
            start,
            end,
            inclusive_end,
        }) => match at {
            0 => Value(Repr::Int(*start)),
            1 => Value(Repr::Int(*end)),
            _ => Value(Repr::Bool(*inclusive_end)),
        },
        other => unreachable!("a {} has no children", dynamic_kind(other).name()),
    }
}

/// The oracle's twin of the machine's internal reflection error: a view asked
/// a question its kind has no answer to.
fn dynamic_internal(message: String) -> RuntimeError {
    RuntimeError::new(format!("internal error: {message}")).with_help(
        "a dynamic view is read by the standard library's own walks, so this is a bug in the \
         standard library rather than in the program",
    )
}

/// The `MapEntry(key:, value:)` one entry of a map's sorted run is, as a value.
fn map_entry(key: &MapKey, value: &Value) -> Value {
    Value(Repr::Struct(Rc::new(StructValue {
        type_name: MAP_ENTRY.name.into(),
        fields: vec![
            (MAP_ENTRY.fields[0].name.into(), key.to_value()),
            (MAP_ENTRY.fields[1].name.into(), value.clone()),
        ],
        opaque: false,
    })))
}

/// The method and the role a keyed refusal is written in, as the standard
/// library passed them to `core.admitKey` or `core.refuseDuplicate`.
fn core_names(
    shown: &str,
    method: &Value,
    role: &Value,
    span: Span,
) -> Result<(String, String), RuntimeError> {
    let Value(Repr::Str(method)) = method else {
        return Err(type_error(shown, "method", "String", method, span));
    };
    let Value(Repr::Str(role)) = role else {
        return Err(type_error(shown, "role", "String", role, span));
    };
    Ok((method.to_string(), role.to_string()))
}

/// What a core intrinsic answers for a vector a finish already consumed.
///
/// An internal invariant and not a program's mistake — `cove_sema::unique`
/// refuses a read after a `freeze()` and a `freeze()` of a vector another place
/// still holds — so it is one sentence that names no method, and the machine's
/// run instructions refuse in the same words (`vm::exec::consumed_vector`).
pub const CONSUMED_VECTOR: &str =
    "a vector was used after it was consumed, which the uniqueness check rules out";

/// Refuses a vector a finish consumed, in [`CONSUMED_VECTOR`]'s words.
fn check_consumed(storage: &Rc<VectorStorage>, span: Span) -> Result<(), RuntimeError> {
    if *storage.frozen.borrow() {
        return Err(RuntimeError::new(CONSUMED_VECTOR).at(span));
    }
    Ok(())
}

/// A core intrinsic's range, `from` for `count` units, inside a run of `len`.
///
/// The machine's `run-slice` or `run-copy` refusal — `instruction` names which,
/// and `unit` what it counts — in its words: a range outside the source is a
/// broken invariant of the standard-library body that asked, which decided the
/// range first.
fn core_range(
    shown: &str,
    instruction: &str,
    unit: &str,
    from: &Value,
    count: &Value,
    len: usize,
    span: Span,
) -> Result<std::ops::Range<usize>, RuntimeError> {
    let Value(Repr::Int(from)) = from else {
        return Err(type_error(shown, "from", "Int", from, span));
    };
    let Value(Repr::Int(count)) = count else {
        return Err(type_error(shown, "count", "Int", count, span));
    };
    let (from, count) = (*from, *count);
    if count < 0 {
        return Err(RuntimeError::new(format!(
            "`{instruction}`'s count is `{count}`, and a copy cannot have a negative length"
        ))
        .at(span));
    }
    match from.checked_add(count) {
        Some(end) if from >= 0 && end <= len as i64 => Ok(from as usize..end as usize),
        _ => Err(RuntimeError::new(format!(
            "`{instruction}` reads {count} {unit} from {from} of a source of {len}"
        ))
        .at(span)),
    }
}

/// A core intrinsic's element index, inside a run of `len`.
///
/// `Machine::element`'s refusal, in its words: an index outside the run a core
/// intrinsic reads is a broken invariant of the standard-library body that
/// called it, not a program's mistake.
fn core_index(shown: &str, index: &Value, len: usize, span: Span) -> Result<usize, RuntimeError> {
    let Value(Repr::Int(at)) = index else {
        return Err(type_error(shown, "index", "Int", index, span));
    };
    match usize::try_from(*at) {
        Ok(at) if at < len => Ok(at),
        _ => Err(
            RuntimeError::new(format!("index {at} is outside a collection of {len}"))
                .at(span)
                .with_rule("An index outside a collection is a broken invariant."),
        ),
    }
}

/// `Vector.of(...)` and `Int.parseRadix(...)`.
pub fn call_associated(
    host: &mut dyn Callable,
    type_name: &str,
    name: &str,
    args: &mut Vec<Value>,
    span: Span,
) -> Result<Value, RuntimeError> {
    match (type_name, name) {
        ("Vector", "of") => Ok(host.allocate_vector(std::mem::take(args))),
        // `Map.of` and `Set.of` are not here: each is `std.map.of` or
        // `std.set.of` (#378, P4-8), which `Interpreter` reaches through
        // `cove_schema::builtins::standard_associated_binding` before this
        // function is asked. `String.fromCodePoint` left the same way and is
        // `std.string.fromCodePoint`: the range of Unicode and the surrogate
        // hole are a policy over a representation (ADR 0064's Decision 2), and
        // the encode under them is `std.stringbuilder` over ADR 0062's
        // ensure/store/commit. It was the only `String` arm this function had.
        // `Duration.nanos(count)`: the one primitive builder left.
        // `micros` through `hours` are `std.duration.ofMicros` and its four
        // neighbours now — see `cove_schema::builtins::standard_associated_binding`
        // — and this arm no longer names them, so there is no factor to
        // multiply by and nothing here can overflow: a `Duration` is signed
        // nanoseconds and every `Int` is already a valid count of them.
        ("Duration", "nanos") => {
            let args = expect_args("Duration.nanos", args, 1, span)?;
            let Value(Repr::Int(count)) = &args[0] else {
                return Err(type_error("Duration.nanos", "count", "Int", &args[0], span));
            };
            Ok(Value(Repr::Duration(*count)))
        }
        // `Int.parse` and `Int.parseRadix` are not here: both are `std.int`
        // bodies, reached through
        // `cove_schema::builtins::standard_associated_binding` before this
        // function is asked. `parseRadix` was the last `Int` arm, because it
        // raised on a radix outside `2..=36` and a Cove body had nothing to
        // raise with until ADR 0067's `core.refuse`.
        // Mirrors `Int.parse` exactly in shape. Rust's `f64::from_str`
        // accepts `inf`, `-inf`, and `NaN`, which is why this does too, and
        // it rejects the `_` digit separators a `Float` literal may be
        // written with — the same thing `Int.parse` above already does,
        // not a new choice made here.
        ("Float", "parse") => {
            let args = expect_args("Float.parse", args, 1, span)?;
            let Value(Repr::Str(text)) = &args[0] else {
                return Err(type_error("Float.parse", "text", "String", &args[0], span));
            };
            Ok(match text.parse::<f64>() {
                Ok(value) => Value::ok(Value(Repr::Float(value))),
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
        Value(Repr::Array(items)) => match name {
            "get" => Ok(index_of("Array.get", args, span)?
                .and_then(|i| items.get(i).cloned())
                .map(Value::some)
                .unwrap_or_else(Value::none)),
            // `length` is not here: it is `std.array.length`, over
            // `call_core`'s `arrayLength`.
            // `isEmpty` used to answer here too, `length() == 0`. It does
            // not reach this arm any more: `Interpreter::eval_method_call`
            // resolves it to a call into `std.array.isEmpty` before this
            // function is ever asked about it — see
            // `cove_schema::builtins::standard_binding`.
            // `contains` and `indexOf` are not here: they are `std.array`'s
            // loops over `==`, which this interpreter runs as Cove.
            // `slice` and `toVector` are not here: they are `std.array.slice`,
            // a clamp in Cove over `call_core`'s `arraySlice`, and
            // `std.array.toVector` over its `arrayToVector`.
            // `filter` and `fold` used to answer here too, through
            // `walk_with` below. Neither reaches this arm any more:
            // `Interpreter::eval_method_call` resolves both to a call into
            // `std.array.filter` and `std.array.fold` before this function is
            // ever asked about them — see
            // `cove_schema::builtins::standard_binding`.
            "map" | "sorted" => walk_with(host, "Array", items.to_vec(), name, args, span),
            _ => Err(no_method("Array", name, span)),
        },
        Value(Repr::Vector(storage)) => {
            check_live(storage, name, span)?;
            match name {
                // `push` is not here: it is `std.vector.push`, over
                // `call_core`'s `vectorEnsure`, `vectorStore` and `vectorCommit`.
                // `set` is not here either: it is `std.vector.set`, whose
                // range decision and `Option` are Cove over `call_core`'s
                // `vectorLoad` and `vectorStore`.
                // `pop` and `remove` are not here either: they are
                // `std.vector.pop` and `std.vector.remove`, whose index
                // decisions and `Option`s are Cove over `call_core`'s
                // `vectorLoad`, `vectorMove` and `vectorTruncate`.
                "get" => Ok(index_of("Vector.get", args, span)?
                    .and_then(|i| storage.elements.borrow().get(i).cloned())
                    .map(Value::some)
                    .unwrap_or_else(Value::none)),
                // `length` is not here either: it is `std.vector.length`, over
                // `call_core`'s `vectorLength`.
                // `contains` and `indexOf` are not here: they are
                // `std.vector`'s loops over `==` and `call_core`'s
                // `vectorLoad`.
                // `slice` is not here: it is `std.vector.slice`, over
                // `call_core`'s `vectorSlice`.
                // `isEmpty` used to answer here too, `storage.is_empty()`.
                // It does not reach this arm any more:
                // `Interpreter::eval_method_call` resolves it to a call into
                // `std.vector.isEmpty` before this function is ever asked
                // about it — see `cove_schema::builtins::standard_binding`.
                // `freeze` is `std.vector.freeze` the same way, over
                // `call_core`'s `vectorFinish`, and `toArray` is
                // `std.vector.toArray`, over its `vectorSlice`.
                // `filter` and `fold` used to answer here too, through
                // `walk_with` below, taking the same copy first. Neither
                // reaches this arm any more: `Interpreter::eval_method_call`
                // resolves both to a call into `std.vector.filter` and
                // `std.vector.fold` before this function is ever asked about
                // them — see `cove_schema::builtins::standard_binding`.
                "map" | "sorted" => {
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
        Value(Repr::Map(entries)) => match name {
            // `get` and `contains` do not reach this arm: both are `std.map`
            // binary searches over `core.order` and `core.entryAt` (ADR 0059),
            // which `call_core` executes over this sorted run.
            "length" => {
                expect_args(name, args, 0, span)?;
                Ok(Value(Repr::Int(entries.len() as i64)))
            }
            // `isEmpty` used to answer here too, `entries.is_empty()`. It
            // does not reach this arm any more: `Interpreter::eval_method_call`
            // resolves it to a call into `std.map.isEmpty` before this
            // function is ever asked about it — see
            // `cove_schema::builtins::standard_binding`.
            // `keys` and `values` are `std.map` loops over the entries (P4-7).
            // `inserted` and `removed` do not reach this arm either: each is
            // `std.map`'s seek and a growable run finished into the new map
            // (#378, P4-6), which `call_core` executes over this sorted run.
            _ => Err(no_method("Map", name, span)),
        },
        Value(Repr::Set(items)) => match name {
            // `contains` does not reach this arm: it is `std.set`'s binary
            // search over `core.order` and `core.memberAt` (ADR 0059).
            "length" => {
                expect_args(name, args, 0, span)?;
                Ok(Value(Repr::Int(items.len() as i64)))
            }
            // `isEmpty` used to answer here too, `items.is_empty()`. It does
            // not reach this arm any more: `Interpreter::eval_method_call`
            // resolves it to a call into `std.set.isEmpty` before this
            // function is ever asked about it — see
            // `cove_schema::builtins::standard_binding`.
            // `toArray`, `inserted` and `removed` are `std.set`'s (P4-6, P4-7).
            _ => Err(no_method("Set", name, span)),
        },
        Value(Repr::Str(text)) => match name {
            // `length` used to answer here, `text.chars().count()`. It does
            // not reach this arm any more: `Interpreter::eval_method_call`
            // resolves it to a call into `std.string.length` before this
            // function is ever asked about it — a Cove loop that reads each
            // lead byte and advances by its width (ADR 0064), which is where
            // the character-counting policy belongs.
            // `isEmpty` used to answer here too, `text.is_empty()`. It does
            // not reach this arm any more: `Interpreter::eval_method_call`
            // resolves it to a call into `std.string.isEmpty` before this
            // function is ever asked about it — see
            // `cove_schema::builtins::standard_binding`.
            // `words` used to answer here, out of `split_ascii_whitespace`.
            // It does not reach this arm any more: `std.string.words` is a
            // byte scan over the five separator bytes, and
            // `Interpreter::eval_method_call` resolves the method to it before
            // anything looks here. Issue #454's Step 5.
            // `chars` used to answer here, by collecting a one-character Rust
            // `String` per `char` of the receiver. It does not reach this arm
            // any more: `Interpreter::eval_method_call` resolves it to
            // `std.string.chars` first, which sizes a `Vector` with the walk
            // under `std.string.length` and then fills it with one
            // `core.stringSlice` per character. ADR 0064's Decision 2 — where
            // a character begins is a fact about a *representation*, and this
            // arm read it out of Rust's decoder — and issue #454's Step 3,
            // which is where the measurement of what the move cost lives.
            // `split` used to answer here, out of `str::split`. It does not
            // reach this arm any more: `std.string.split` is a
            // `core.stringFind` a separator and a `core.stringSlice` a part,
            // and raises on an empty separator through ADR 0067's
            // `core.refuse`. Issue #454's Step 3, finished.
            // `join` used to answer here, by pushing each part onto a Rust
            // `String` with the receiver between them and handing the whole
            // thing back. It does not reach this arm any more:
            // `Interpreter::eval_method_call` resolves it to `std.string.join`
            // first, which sums the parts' byte lengths and the separator's
            // times one fewer than the parts, sizes a `StringBuilder` by that
            // sum and appends into it. ADR 0064's Decision 2 — the count of
            // separators is an arithmetic policy over a representation, and
            // `join` is a method's name — and issue #454's Step 3, which is
            // where the measurement of what the move cost lives.
            // `slice` used to answer here, by collecting the whole receiver
            // into a `Vec<char>`, clamping each bound into it and collecting
            // the middle back again. It does not reach this arm any more:
            // `Interpreter::eval_method_call` resolves it to `std.string.slice`
            // first, which clamps the two positions the same way and then walks
            // the lead bytes as far as `to` to find the two byte offsets its
            // `core.stringSlice` copies between. ADR 0064's Decision 2 — a
            // clamp is a range policy and `slice` is a method's name — and ADR
            // 0058's table, which gives ranges to Cove and keeps the bounded
            // copy below.
            // `trim` used to answer here, out of `str::trim`, which is
            // `char::is_whitespace` and so whatever Unicode table this
            // *toolchain* was built against. `std.string.trim` is the set
            // written out in Cove with its version stated, and this backend
            // runs that body like the other one does. ADR 0064's Decision 5,
            // and issue #454's Step 5.
            // `contains`, `indexOf`, `startsWith` and `endsWith` used to
            // answer here — one `text.contains(needle)`, one `text.find(needle)`
            // with `chars().count()` over the prefix, one
            // `text.starts_with(prefix)` and one `text.ends_with(suffix)`. None
            // of the four reaches this arm any more:
            // `Interpreter::eval_method_call` resolves each to a call into
            // `std.string` before this function is ever asked about it. The two
            // comparisons are Cove loops over bytes (ADR 0064) — the suffix one
            // needs UTF-8's self-synchronization to justify the offset it starts
            // at, the prefix one starts at 0 and needs nothing. The two searches
            // are Cove bodies over `core.stringFind` (ADR 0065), which is
            // `crate::find` above: a bounded run search rather than a loop,
            // because their work is proportional to a haystack the caller did
            // not size. `indexOf` then walks the prefix's lead bytes to turn
            // the byte offset that search answers into a character position —
            // the half of the old arm that was never a search at all.
            // `replace` used to answer here, out of `str::replace`. It does
            // not reach this arm any more: `std.string.replace` counts the
            // matches with `core.stringFind`, sizes its answer from that and
            // writes it in append windows, and raises on an empty `old` the
            // way `split` does. Issue #454's Step 3, finished.
            // `toUpper` and `toLower` used to answer here, out of
            // `str::to_uppercase` and `str::to_lowercase`, which are whatever
            // Unicode case-mapping tables this *toolchain* was built against.
            // Neither reaches this arm any more: `Interpreter::eval_method_call`
            // resolves each to a call into `std.string` first, over the
            // generated `upperRuns`/`lowerRuns`/`upperExpansions`/
            // `lowerExpansions`/`casedRanges`/`ignorableRanges` tables
            // `crates/cove-sema/tests/unicase.rs` regenerates. ADR 0064's
            // Decision 5, and issue #454's Step 5.
            // The byte-counted operations. Their diagnostics are written out
            // again in `crates/cove-runtime/src/vm/intrinsics/text.rs` rather
            // than shared, as every other builtin's are; what holds the two
            // readings together is `tests/e2e/values_string`, which runs on
            // both backends against one `expected.out`. `byteLength` is not
            // here: it is `std.string` over `call_core`'s `byteLength`, and
            // `codePointAtByte` is `std.string`'s decode over `byteAt` below.
            "byteAt" => {
                let args = expect_args("String.byteAt", args, 1, span)?;
                let Value(Repr::Int(offset)) = &args[0] else {
                    return Err(type_error("String.byteAt", "offset", "Int", &args[0], span));
                };
                // Refused rather than answered, which is `sliceBytes`'s rule
                // and not `codePointAtByte`'s: a byte offset out of range is
                // one this type never handed out, and `byteLength()` is how a
                // caller knows the range. The VM's `Inst::RunLoad` refuses in
                // the same words.
                match usize::try_from(*offset)
                    .ok()
                    .and_then(|at| text.as_bytes().get(at))
                {
                    Some(byte) => Ok(Value(Repr::Int(*byte as i64))),
                    None => Err(RuntimeError::new(format!(
                        "`byteAt` is `{offset}`, and a byte offset into this string is 0 to {}",
                        text.len() as i64 - 1
                    ))
                    .at(span)),
                }
            }
            _ => Err(no_method("String", name, span)),
        },
        Value(Repr::Range {
            start,
            end,
            inclusive_end,
        }) => {
            let bounds = RangeBounds::of(*start, *end, *inclusive_end);
            match name {
                "length" => {
                    expect_args(name, args, 0, span)?;
                    Ok(Value(Repr::Int(bounds.len())))
                }
                "isEmpty" => {
                    expect_args(name, args, 0, span)?;
                    Ok(Value(Repr::Bool(bounds.is_empty())))
                }
                "contains" => {
                    let args = expect_args("contains", args, 1, span)?;
                    let Value(Repr::Int(value)) = &args[0] else {
                        return Err(type_error("Range.contains", "value", "Int", &args[0], span));
                    };
                    Ok(Value(Repr::Bool(bounds.contains(*value))))
                }
                _ => Err(no_method("Range", name, span)),
            }
        }
        // `Option` and `Result` do not answer here at all any more. Every
        // one of their methods — `isSome`, `isNone`, `unwrapOr` on the one,
        // `isOk`, `isError`, `unwrapOr`, `mapError` on the other — is
        // resolved by `Interpreter::eval_method_call` to a call into
        // `std.option` or `std.result` before this function is ever asked;
        // see `cove_schema::builtins::standard_binding`.
        //
        // `mapError` was the last to go and it needed a language change
        // rather than a migration: while a callback of no parameters could
        // stand in for one that takes the error, no Cove body could call it.
        // ADR 0044 removed that exception.
        Value(Repr::Int(n)) => match name {
            "toFloat" => {
                expect_args(name, args, 0, span)?;
                Ok(Value(Repr::Float(*n as f64)))
            }
            // `min`, `max`, and `abs` used to answer here too, `(*n).min(*other)`,
            // `(*n).max(*other)`, and `n.checked_abs()`. None reaches this arm
            // any more: `Interpreter::eval_method_call` resolves them to a
            // call into `std.int.min`/`std.int.max`/`std.int.abs` before this
            // function is ever asked about them — see
            // `cove_schema::builtins::standard_binding`.
            _ => Err(no_method("Int", name, span)),
        },
        Value(Repr::Float(x)) => match name {
            "toInt" => {
                expect_args(name, args, 0, span)?;
                Ok(float_to_int(*x))
            }
            "round" => {
                expect_args(name, args, 0, span)?;
                Ok(Value(Repr::Float(x.round())))
            }
            "abs" => {
                expect_args(name, args, 0, span)?;
                Ok(Value(Repr::Float(x.abs())))
            }
            "sqrt" => {
                expect_args(name, args, 0, span)?;
                Ok(Value(Repr::Float(x.sqrt())))
            }
            // `crate::float::extremum` rather than `f64::min`, and the bit
            // casts on both sides are the point of it: the answer is one of
            // the two operands handed back *whole*, so a winning signalling
            // NaN stays signalling here exactly as it does in the encoded VM
            // and in the native tier. `f64::min`'s own documentation
            // declines to decide the tie — "either input may be returned
            // non-deterministically" — and this operation's tie is decided, so
            // the tier that is the **semantic oracle** for the other two is
            // the last one that should be inheriting the answer from whichever
            // `rustc` built the binary. See that function for the whole of it.
            "min" => {
                let args = expect_args("Float.min", args, 1, span)?;
                let Value(Repr::Float(other)) = &args[0] else {
                    return Err(type_error("Float.min", "other", "Float", &args[0], span));
                };
                Ok(Value(Repr::Float(f64::from_bits(crate::float::extremum(
                    x.to_bits(),
                    other.to_bits(),
                    MinMax::Min,
                )))))
            }
            "max" => {
                let args = expect_args("Float.max", args, 1, span)?;
                let Value(Repr::Float(other)) = &args[0] else {
                    return Err(type_error("Float.max", "other", "Float", &args[0], span));
                };
                Ok(Value(Repr::Float(f64::from_bits(crate::float::extremum(
                    x.to_bits(),
                    other.to_bits(),
                    MinMax::Max,
                )))))
            }
            // `format` used to answer here, out of `format!("{:.*}")`. It is
            // `std.float.format` now, resolved before this function is asked.
            _ => Err(no_method("Float", name, span)),
        },
        // `d.nanos()`: the one primitive reader left. `micros` through
        // `hours` are `std.duration.micros` and its four neighbours now,
        // resolved by `Interpreter::eval_method_call` before this function
        // is ever asked — see
        // `cove_schema::builtins::standard_binding`.
        Value(Repr::Duration(ns)) => match name {
            "nanos" => {
                expect_args(name, args, 0, span)?;
                Ok(Value(Repr::Int(*ns)))
            }
            _ => Err(no_method("Duration", name, span)),
        },
        other => Err(no_method(&other.type_name(), name, span)),
    }
}

/// `map` and `sorted`, the two operations on an `Array` and on a `Vector`
/// that still take their callback here, as the interpreter runs them: the
/// linear-memory backend lowers each of the two to its own loop instead —
/// see `crates/cove-ir/src/lower/walks.rs`, which calls this file's version
/// the oracle it has to agree with.
///
/// `filter` and `fold` used to be two more. They are ordinary calls into
/// `std.array` and `std.vector` now, resolved before either evaluator ever
/// reaches this function — see `cove_schema::builtins::standard_binding`.
///
/// `elements` is already the caller's own copy — the `Array`'s elements, or
/// the `Vector`'s taken out from under its `RefCell` before this was
/// called — which is what makes the walk a walk over a snapshot. A callback
/// that reaches the vector it was handed an element of may push onto it,
/// `freeze` it, or drop the last other handle to it, and neither changes
/// what is being walked or what comes back. The lowering makes the same
/// decision by reading a sequence's length once, with `Inst::Len`, before it
/// walks; this is that decision in the place where a closure rather than a
/// loop body is what could do the mutating.
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
///
/// # The argument list is `args`, once, for the whole walk
///
/// Each of the two takes its callback out of `args` first, which leaves that
/// vector empty with its capacity intact — so it is what every invocation of
/// the callback is handed, filled and drained again per element rather than
/// allocated per element. That is issue #193: `map` built a `vec![item]` for
/// each element it visited, `filter` a `vec![item.clone()]`, `fold` a
/// `vec![total, item]`, and `sorted` one per comparison, which for
/// `examples/life`'s `population()` is an allocation per creature per tick —
/// true of all four at the time #193 was fixed, even though two of them have
/// since moved out of this function entirely.
///
/// It costs nothing to arrange because `args` is already a vector the
/// caller lends. The predecessor pooled its own argument vectors the same
/// way starting at #184; #193 is that scheme reaching a path it could not
/// reach before, by being handed one level further down. A slice would not
/// do here for the same reason it would not do there — `map` moves its
/// element into the call, and the callback re-enters the evaluator and may
/// push onto the very stack a slice would point into.
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
                args.push(item);
                mapped.push(host.call_value(&transform, args, span)?);
            }
            Ok(Value(Repr::Array(mapped.into())))
        }
        "sorted" => {
            let args = expect_args(&method, args, 1, span)?;
            let by = args.remove(0);
            expect_callback(host, &method, "by", "fn(T, T) -> Bool", 2, &by, span)?;
            Ok(Value(Repr::Array(
                merge_sort(host, &method, elements, &by, args, span)?.into(),
            )))
        }
        // Only the two names above are routed here now; `filter` and `fold`
        // are resolved to a standard-library call before either evaluator
        // reaches this function. Answering the way an unknown method is
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
    args: &mut Vec<Value>,
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
                args.push(source[right].clone());
                args.push(source[left].clone());
                let verdict = host.call_value(by, args, span)?;
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
/// checked program does reaches either failure. It is still asked here,
/// once for the whole walk, rather than left for
/// `Interpreter::call_value_slots` to discover on the first call: a walk of
/// zero elements never makes that call at all, so leaving the check there
/// would mean an empty `Array` misses a callback of the wrong arity that a
/// full one catches. Asking here also gives the failure the builtin's own
/// words — the declared shape, `fn(T) -> R`, and which parameter — rather
/// than a plain arity count. `map` and `sorted` are the interpreter's own
/// implementation of the two walks that remain here — `filter` and `fold`
/// moved to the standard library and never reach this function — and the
/// linear-memory backend lowers `map` and `sorted` on its own and never
/// reaches this function either.
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
        Value(Repr::Bool(answer)) => Ok(*answer),
        other => Err(RuntimeError::new(format!(
            "`{method}` expects `{parameter}` to answer a `Bool`, but found `{}`",
            other.type_name()
        ))
        .at(span)),
    }
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

/// A buffer consumed by `finish()` is no longer usable.
///
/// [`check_live`]'s sentence for a buffer, and the same refusal
/// `Machine::buffer` makes when it finds a null store: a consumed buffer read
/// as an empty one would turn a uniqueness proof that let something through
/// into a silently wrong answer.
fn check_buffer_live(
    storage: &Rc<ByteBufferStorage>,
    method: &str,
    span: Span,
) -> Result<(), RuntimeError> {
    if *storage.finished.borrow() {
        return Err(RuntimeError::new(format!(
            "`{method}` was called on a byte buffer that `finish()` already consumed"
        ))
        .at(span)
        .with_rule("`finish()` consumes its buffer; the source buffer is no longer usable.")
        .with_help("use the `String` that `finish()` returned, or build a new buffer"));
    }
    Ok(())
}

fn index_of(method: &str, args: &[Value], span: Span) -> Result<Option<usize>, RuntimeError> {
    if args.len() != 1 {
        return Err(arity_error(method, 1, args.len(), span));
    }
    match &args[0] {
        Value(Repr::Int(i)) if *i >= 0 => Ok(Some(*i as usize)),
        Value(Repr::Int(_)) => Ok(None),
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
    Value::ok(Value(Repr::Int(truncated as i64)))
}

/// What is wrong with the byte range `appendSlice(text, from, to)` names, which
/// `std.stringbuilder`'s `appendRange` has already found to be wrong.
///
/// It is `String.sliceBytes`' rule, and was that method's own until ADR 0058
/// moved it into `std.string`, whose `refuseRange` says the same sentences in
/// Cove — and whose arms this walks in the same order. Five things can be
/// wrong, asked in the order a reader would ask them: is each end a byte offset
/// into this string at all, do they run forwards, and does each begin a
/// character. The last is the one `slice` has no equivalent of, and it is why
/// `appendSlice` refuses where `slice` clamps — an offset inside a character
/// was never handed out by `codePointAtByte`, so moving it to the nearest legal
/// one would answer a question nobody asked.
///
/// The final arm is the `to` boundary rather than a case of its own, exactly as
/// `refuseRange`'s `else` is: this is reached only after a check in Cove has
/// failed, and the two copies agree about which sentence a range gets by having
/// the same shape rather than by each deciding.
///
/// The linear-memory backend's copy is `vm::intrinsics::text::refuse_byte_range`
/// — written twice for the reason that module's rendering is, and kept honest
/// by the differential corpus.
fn wrong_byte_range(text: &str, from: i64, to: i64) -> String {
    let len = text.len() as i64;
    let boundary = |at: i64| at < len && !text.is_char_boundary(at as usize);
    if from < 0 || from > len {
        format!("`from` is `{from}`, and a byte offset into this string is 0 to {len}")
    } else if to < 0 || to > len {
        format!("`to` is `{to}`, and a byte offset into this string is 0 to {len}")
    } else if from > to {
        format!("`from` is `{from}` and `to` is `{to}`, so this range runs backwards")
    } else if boundary(from) {
        format!("`from` is `{from}`, which is inside a character rather than at the start of one")
    } else {
        format!("`to` is `{to}`, which is inside a character rather than at the start of one")
    }
}

// `one_character` stood here: a per-thread table of the 128 ASCII
// one-character `Rc<str>`s, so that `chars()` on this backend handed out a
// shared string per ASCII character rather than allocating one. Issue #104 put
// it here because `chars()` was how a program took text apart and that made it
// the largest single source of allocation in `examples/cq`; issue #454's Step 3
// took its one caller away, and `std.string.chars` allocates a fresh string per
// character on this backend as it does on the other one.
//
// **Nothing observable changed and something unobservable did.** Issue #104's
// own argument is why the deletion is safe — "a character's string is immutable
// and interchangeable, so there is no way for a program to tell a shared one
// from a fresh one" — and it is also why the interning could never move into
// Cove: `core.stringSlice` answers a run of the receiver's bytes, and a Cove
// body has nothing to consult a table with. So the tree-walking interpreter
// allocates more per `chars()` call than it did. That is a cost on the
// *oracle*, which ADR 0034 keeps as the definition of what a Cove program
// means rather than as something anything is timed on, and the linear-memory
// backend never had the table.

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

/// ADR 0068's seven observations as the oracle answers them: `call_core`'s
/// arms, over the same values the machine's `vm::exec::dynamic` tests box, held
/// to the same descriptions — so a kind code, a child order or a case index
/// that differed between the two evaluators would fail one side or the other
/// against one shared string.
#[cfg(test)]
mod dynamic_tests {
    use std::rc::Rc;

    use cove_diag::{FileId, Span};
    use cove_ir::DynamicKind;

    use super::{call_core, Callable};
    use crate::error::RuntimeError;
    use crate::value::{DynValue, MapKey, Repr, StructValue, Value, VectorStorage};

    /// A caller with no program behind it but the one enum the fixtures
    /// declare: `m.Mark`, whose cases are `Plain`, `Count` and `Named` in that
    /// order.
    struct Probe;

    impl Callable for Probe {
        fn allocate_vector(&mut self, elements: Vec<Value>) -> Value {
            Value(Repr::Vector(VectorStorage::new(elements)))
        }

        fn call_value(
            &mut self,
            _: &Value,
            _: &mut Vec<Value>,
            _: Span,
        ) -> Result<Value, RuntimeError> {
            unreachable!("a reflection calls nothing")
        }

        fn arity(&self, _: &Value) -> Option<usize> {
            None
        }

        fn snapshot(&mut self, _: &Value, _: Span) -> Result<Value, RuntimeError> {
            unreachable!("a reflection copies nothing")
        }

        fn case_index(&self, type_name: &str, case: &str) -> Option<usize> {
            match type_name {
                "m.Mark" => ["Plain", "Count", "Named"]
                    .iter()
                    .position(|held| *held == case),
                _ => None,
            }
        }
    }

    fn span() -> Span {
        Span::new(FileId(0), 0, 0)
    }

    /// `core.name(args)`, answered or refused.
    fn core(name: &str, args: Vec<Value>) -> Result<Value, RuntimeError> {
        call_core(&mut Probe, name, &mut args.clone(), span())
    }

    fn ask(name: &str, args: Vec<Value>) -> Value {
        core(name, args).unwrap_or_else(|error| panic!("`core.{name}`: {}", error.message))
    }

    fn int(value: &Value) -> i64 {
        value.as_int().expect("an `Int`")
    }

    /// The oracle's description of a view, in the words the machine's tests
    /// describe its own in.
    fn describe(view: &Value) -> String {
        let code = int(&ask("dynamicKind", vec![view.clone()]));
        let kind = DynamicKind::from_code(code).unwrap_or_else(|| panic!("kind {code}"));
        match kind {
            DynamicKind::Unit => "()".to_string(),
            DynamicKind::Bool => ask("dynamicBool", vec![view.clone()])
                .as_bool()
                .unwrap()
                .to_string(),
            DynamicKind::Int => int(&ask("dynamicInt", vec![view.clone()])).to_string(),
            DynamicKind::Float => format!(
                "{:?}",
                ask("dynamicFloat", vec![view.clone()]).as_float().unwrap()
            ),
            DynamicKind::Duration => format!(
                "{}ns",
                ask("dynamicDuration", vec![view.clone()])
                    .as_duration_nanos()
                    .unwrap()
            ),
            DynamicKind::String => format!(
                "{:?}",
                ask("dynamicString", vec![view.clone()]).as_str().unwrap()
            ),
            DynamicKind::Function | DynamicKind::Opaque => {
                assert_eq!(int(&ask("dynamicChildCount", vec![view.clone()])), 0);
                kind.name().to_string()
            }
            _ => {
                let mut out = kind.name().to_string();
                if kind == DynamicKind::Enum {
                    let case = int(&ask("dynamicCase", vec![view.clone()]));
                    out.push_str(&format!("#{case}"));
                }
                let count = int(&ask("dynamicChildCount", vec![view.clone()]));
                let children: Vec<String> = (0..count)
                    .map(|at| describe(&ask("dynamicChild", vec![view.clone(), Value::int(at)])))
                    .collect();
                out.push_str(&format!("[{}]", children.join(", ")));
                out
            }
        }
    }

    /// A `dyn` wrapper around `value`, which is what the oracle's box is.
    fn erased(value: Value) -> Value {
        Value(Repr::Dyn(Rc::new(DynValue {
            trait_name: "m.Probed".into(),
            value,
        })))
    }

    fn point(x: i64, y: i64) -> Value {
        Value::structure("m.Point", [("x", Value::int(x)), ("y", Value::int(y))])
    }

    fn inner(label: &str, x: i64, y: i64) -> Value {
        Value::structure(
            "m.Inner",
            [("label", Value::string(label)), ("at", point(x, y))],
        )
    }

    /// The machine's `every_kind_is_described_through_the_instructions`, row
    /// for row.
    #[test]
    fn every_kind_is_described_through_the_seven_arms() {
        let secret = Value(Repr::Struct(Rc::new(StructValue {
            type_name: "m.Secret".into(),
            fields: vec![("code".into(), Value::int(9))],
            opaque: true,
        })));
        let cases: Vec<(&str, Value)> = vec![
            (
                "struct[\"outer\", struct[\"inner\", struct[3, 4]]]",
                Value::structure(
                    "m.Outer",
                    [
                        ("name", Value::string("outer")),
                        ("inner", inner("inner", 3, 4)),
                    ],
                ),
            ),
            ("enum#0[]", Value::enumeration("m.Mark", "Plain", [])),
            (
                "enum#1[7]",
                Value::enumeration("m.Mark", "Count", [Value::int(7)]),
            ),
            (
                "enum#2[\"named\"]",
                Value::enumeration("m.Mark", "Named", [Value::string("named")]),
            ),
            ("enum#1[5]", Value::some(Value::int(5))),
            ("enum#0[]", Value::none()),
            ("enum#1[\"only\"]", Value::some(Value::string("only"))),
            ("enum#0[1]", Value::ok(Value::int(1))),
            ("enum#1[\"no\"]", Value::err(Value::string("no"))),
            (
                "Array[1, 2, 3]",
                Value::array([Value::int(1), Value::int(2), Value::int(3)]),
            ),
            (
                "Vector[struct[1, 2], struct[3, 4]]",
                Value(Repr::Vector(VectorStorage::new(vec![
                    point(1, 2),
                    point(3, 4),
                ]))),
            ),
            ("Set[1, 5]", Value::set([MapKey::Int(1), MapKey::Int(5)])),
            (
                "Map[\"a\", 1, \"b\", 2]",
                Value::map([
                    (MapKey::Str("a".to_string()), Value::int(1)),
                    (MapKey::Str("b".to_string()), Value::int(2)),
                ]),
            ),
            ("Range[1, 4, false]", Value::range_of(1, 4, false)),
            ("function", Value::host_fn("console", "println")),
            ("struct[1, 2]", erased(point(1, 2))),
            (
                "struct[struct[\"n\", struct[5, 6]]]",
                Value::structure("m.Holder", [("it", erased(inner("n", 5, 6)))]),
            ),
            ("struct[9]", secret),
            ("()", Value::unit()),
            ("true", Value::bool(true)),
            ("-7", Value::int(-7)),
            ("1.5", Value::float(1.5)),
            ("250ns", Value::duration(250)),
            ("\"s\"", Value::string("s")),
        ];
        for (want, value) in cases {
            let view = ask("dynamicOpen", vec![erased(value)]);
            assert_eq!(describe(&view), want);
        }
    }

    /// The machine's `same_type_is_kind_and_declared_name`, row for row: the
    /// name a value carries is qualified, and carries no instantiation.
    #[test]
    fn same_type_is_kind_and_declared_name() {
        let rows: Vec<(Value, Value, bool)> = vec![
            (Value::some(Value::int(5)), Value::none(), true),
            (
                Value::some(Value::int(5)),
                Value::some(Value::string("x")),
                true,
            ),
            (Value::some(Value::int(5)), Value::ok(Value::int(1)), false),
            (point(1, 2), point(1, 2), true),
            (point(1, 2), inner("l", 1, 2), false),
            (
                Value::range_of(1, 2, false),
                Value::range_of(5, 9, true),
                true,
            ),
            (Value::range_of(1, 2, false), point(1, 2), false),
            (Value::int(3), Value::int(4), true),
            (Value::int(3), Value::duration(3), false),
        ];
        for (a, b, want) in rows {
            let shown = format!("{a:?} and {b:?}");
            let same = ask("dynamicSameType", vec![erased(a), erased(b)]);
            assert_eq!(same.as_bool(), Some(want), "{shown}");
        }
    }

    /// The machine's `a_question_the_kind_has_no_answer_to_is_refused`, in the
    /// same sentences.
    #[test]
    fn a_question_the_kind_has_no_answer_to_is_refused() {
        let view = ask("dynamicOpen", vec![erased(Value::string("t"))]);
        for (name, args, message) in [
            (
                "dynamicInt",
                vec![view.clone()],
                "internal error: a dynamic view of a String was read as a `Int`",
            ),
            (
                "dynamicCase",
                vec![view.clone()],
                "internal error: the case of a dynamic view of a String was asked",
            ),
            (
                "dynamicChild",
                vec![view.clone(), Value::int(0)],
                "internal error: child 0 of a dynamic view with 0 children was asked",
            ),
        ] {
            let error = core(name, args).expect_err("refused");
            assert_eq!(error.message, message, "`core.{name}`");
        }
    }
}
