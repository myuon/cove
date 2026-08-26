//! Runtime values.
//!
//! Assignment and ordinary argument passing use one rule: field-wise shallow
//! copy. That rule is encoded directly in [`Clone`]: cloning a struct or enum
//! copies its fields, cloning an `Array` shares immutable storage, and cloning
//! a `Vector` copies only the handle so aliases observe the same elements and
//! length. Cove never performs an implicit deep copy.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use cove_schema::builtins::{
    BuiltinSchema, CaseSchema, ERROR, ERR_CASE, MESSAGE_FIELD, NONE_CASE, OK_CASE, OPTION, RESULT,
    SOME_CASE,
};
use cove_syntax::ast::{FnDecl, Param};

use crate::host::ResourceHandle;
use crate::shared::SharedCell;
use crate::task::{Task, TaskScope};

/// A Cove value.
#[derive(Clone, Debug)]
pub enum Value {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A duration in nanoseconds.
    Duration(i64),
    Str(Rc<str>),
    /// Fixed-length immutable sequence; sharing its storage is unobservable.
    Array(Rc<[Value]>),
    /// Growable mutable sequence backed by stable shared storage. Copying the
    /// handle is O(1) and aliases observe the same elements and length.
    Vector(Rc<VectorStorage>),
    /// Immutable in the MVP. Iterates in ascending key order, since that is
    /// the natural order of its `BTreeMap` storage.
    Map(Rc<BTreeMap<MapKey, Value>>),
    /// Immutable in the MVP. Backed by the same key-ordered storage as `Map`,
    /// so membership is O(log n) and iteration order is defined the same
    /// way: ascending order. An element must satisfy the [`MapKey`]
    /// restriction, exactly like a map key.
    Set(Rc<BTreeSet<MapKey>>),
    /// A struct value. Cloning copies each field by that field's own rule.
    Struct(Box<StructValue>),
    /// An enum value, including `Option` and `Result`.
    Enum(Box<EnumValue>),
    /// A callback is an ordinary handle value.
    Closure(Rc<Closure>),
    /// A `dyn Trait` value: a concrete value together with the trait it was
    /// used at.
    ///
    /// This is the one place where a Cove value's runtime representation
    /// depends on its static type. A concrete value is wrapped here at the
    /// point it is used where a `dyn Trait` is expected, and the wrapper
    /// carries what a concrete value does not: the trait, so a diagnostic can
    /// name it, and the value itself, whose own type is what dispatch finds
    /// the implementation from.
    Dyn(Rc<DynValue>),
    /// A bound host module such as `console`.
    HostModule(Rc<str>),
    /// A handle to a resource the host owns, such as a database connection.
    ///
    /// The handle is a name, never the thing itself: what a
    /// `database.Connection` really is stays on the host's side of the
    /// boundary, and this value carries only the identity that addresses it.
    /// That is what lets a handle be copied like any other value, crossed
    /// into a task when its schema allows it, written into a trace, and
    /// handed back by a replay — see ADR 0013 and
    /// [`crate::host::ResourceHandle`].
    Resource(Arc<ResourceHandle>),
    /// A bound host operation such as `console.println`.
    HostFn {
        module: Rc<str>,
        op: Rc<str>,
    },
    /// A type used as a value, such as `Vector` in `Vector.of(1, 2)`.
    Type(Rc<str>),
    /// An integer range. `..` includes `end` and `..<` excludes it.
    ///
    /// A range is an ordinary value: it can be bound, passed, compared, and
    /// iterated. An empty or reversed range such as `3..<0` yields nothing.
    Range {
        start: i64,
        end: i64,
        inclusive_end: bool,
    },
    /// The task scope `scope tasks { ... }` binds. Concurrent work belongs to
    /// a task scope, and the scope owns the tasks spawned into it.
    TaskScope(Rc<TaskScope>),
    /// A handle to a spawned task. The task's value is reachable only through
    /// `await` or through the scope settling it on exit.
    Task(Rc<Task>),
    /// `Shared(value)`: mutable state more than one task may reach.
    ///
    /// This is the one value whose storage is an [`Arc`] rather than an
    /// [`Rc`]: a `Shared` crosses a task boundary by sharing its cell, so two
    /// task threads address the same one. Its contents are reachable only
    /// through `lock`; see [`crate::shared`].
    Shared(Arc<SharedCell>),
}

/// The half-open bounds of a [`Value::Range`], widened to `i128` so that an
/// inclusive `i64::MAX` end cannot overflow.
#[derive(Clone, Copy, Debug)]
pub struct RangeBounds {
    /// The first value the range can yield.
    pub start: i128,
    /// The first value past the end.
    pub end: i128,
}

impl RangeBounds {
    /// Normalises the AST form, where `inclusive_end` selects `..` over `..<`.
    pub fn of(start: i64, end: i64, inclusive_end: bool) -> RangeBounds {
        RangeBounds {
            start: i128::from(start),
            end: i128::from(end) + i128::from(inclusive_end),
        }
    }

    /// The number of values the range yields. A reversed range yields none.
    pub fn len(self) -> i64 {
        (self.end - self.start).max(0) as i64
    }

    /// Whether the range yields no values at all.
    pub fn is_empty(self) -> bool {
        self.end <= self.start
    }

    /// Whether `value` is one of the values the range yields.
    pub fn contains(self, value: i64) -> bool {
        let value = i128::from(value);
        self.start <= value && value < self.end
    }

    /// The values the range yields, in order.
    pub fn items(self) -> Vec<Value> {
        (self.start..self.end)
            .map(|n| Value::Int(n as i64))
            .collect()
    }
}

/// Growable vector storage. Length, capacity, and elements all belong to the
/// shared storage, so growth stays visible through every alias.
#[derive(Debug, Default)]
pub struct VectorStorage {
    pub elements: RefCell<Vec<Value>>,
    /// Set by `freeze()`, which consumes uniquely owned storage.
    pub frozen: RefCell<bool>,
}

impl VectorStorage {
    pub fn new(elements: Vec<Value>) -> Rc<VectorStorage> {
        Rc::new(VectorStorage {
            elements: RefCell::new(elements),
            frozen: RefCell::new(false),
        })
    }

    pub fn len(&self) -> usize {
        self.elements.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Debug)]
pub struct StructValue {
    /// Fully qualified type name, such as `values.BookingDraft`.
    pub type_name: Rc<str>,
    /// Fields in declaration order.
    pub fields: Vec<(Rc<str>, Value)>,
}

impl StructValue {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.fields
            .iter()
            .find(|(n, _)| &**n == name)
            .map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut Value> {
        self.fields
            .iter_mut()
            .find(|(n, _)| &**n == name)
            .map(|(_, v)| v)
    }
}

#[derive(Clone, Debug)]
pub struct EnumValue {
    /// Fully qualified type name, or `Option` / `Result` for the builtins.
    pub type_name: Rc<str>,
    pub case: Rc<str>,
    pub payload: Vec<Value>,
}

/// The contents of a [`Value::Dyn`].
#[derive(Clone, Debug)]
pub struct DynValue {
    /// Fully qualified trait name, such as `render.Display`.
    pub trait_name: Rc<str>,
    /// The concrete value. Its own type is what dynamic dispatch resolves a
    /// method against, which is exactly what makes this dispatch dynamic.
    pub value: Value,
}

/// A closure captures its environment by value at creation time.
#[derive(Debug)]
pub struct Closure {
    pub is_async: bool,
    pub params: Vec<Param>,
    /// `None` for lambdas, which have no declaration of their own.
    pub decl: Option<Arc<FnDecl>>,
    pub body: Arc<cove_syntax::ast::Block>,
    /// The module a closure body resolves names in.
    pub module: Rc<str>,
    pub captures: Vec<(Rc<str>, Value)>,
}

/// A value usable as a `Map` key or `Set` element.
///
/// ADR 0001 draws the line at mutability, not at primitives: "mutable
/// handles and structs containing them are not valid map keys." A key's
/// equality must not change while a collection holds it, so this is
/// recursive rather than a flat list of primitive shapes — a `Struct`, an
/// `Array`, or an enum case with a payload qualifies exactly when everything
/// nested inside it does. `Map` and `Set` qualify too: both are immutable
/// handles, so nesting one as a key changes nothing about the rule, only how
/// deep the check goes. A `Range` qualifies for the same reason: it is an
/// immutable value with a stable `eq_value`, ordered consistently by its
/// `(start, end, inclusive_end)` fields. `Float` is rejected for an unrelated
/// reason: `NaN` is not equal to itself, which breaks the total order every
/// key needs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MapKey {
    Unit,
    Bool(bool),
    Int(i64),
    Duration(i64),
    Str(String),
    /// An enum case, keyed by `(type, case)`, with every payload value
    /// converted the same way.
    EnumCase(String, String, Vec<MapKey>),
    /// A struct, keyed by type name, with every field converted the same
    /// way, in declaration order.
    Struct(String, Vec<(String, MapKey)>),
    /// An array, with every element converted the same way. An array is
    /// fixed-length and immutable, so its equality cannot change.
    Array(Vec<MapKey>),
    /// A `Set`. Its elements are already `MapKey`s by construction, so
    /// nesting one never fails.
    Set(BTreeSet<MapKey>),
    /// A `Map`. Its keys are already `MapKey`s by construction; only its
    /// values need converting, and the first one that cannot be is why
    /// nesting a `Map` as a key can still fail.
    Map(BTreeMap<MapKey, MapKey>),
    /// A range. Immutable with a stable `eq_value`, so it qualifies under the
    /// same rule as every other key: its equality cannot change while a
    /// collection holds it. Ordered by `(start, end, inclusive_end)`, which is
    /// a total order because every field is.
    Range {
        start: i64,
        end: i64,
        inclusive_end: bool,
    },
}

/// Why a value cannot be a `Map` key or `Set` element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidKey {
    /// How the offending part is reached from the value that was tested,
    /// such as `Point.tags` or `Point.tags[0]`. Empty when the value itself,
    /// not something nested inside it, is the problem.
    pub path: String,
    /// The type that cannot be a key.
    pub type_name: String,
}

impl InvalidKey {
    /// The rule this violation breaks.
    ///
    /// `Float` is excluded for a reason distinct from every other rejection:
    /// `NaN != NaN` breaks the total order a key needs, which has nothing to
    /// do with mutability. Stating that separately keeps anyone from later
    /// "fixing" `Float` as if it were just another mutable-handle case.
    pub fn rule(&self) -> &'static str {
        if self.type_name == "Float" {
            "A `Float` cannot be a map key or set element: `NaN` is not equal to itself, which breaks the total order every key needs."
        } else {
            "Mutable handles and structs containing them are not valid map keys: a key's equality must not change while a collection holds it."
        }
    }

    /// A corrected textual example, tailored to the same distinction.
    pub fn help(&self) -> String {
        if self.type_name == "Float" {
            "convert it to a stable key first, such as rounding to an `Int` or formatting it as a `String`".to_string()
        } else {
            "use a value built only from `Bool`, `Int`, `Str`, `Duration`, `Unit`, a range, arrays, structs, enum cases, `Map`, or `Set` — all free of mutable handles".to_string()
        }
    }
}

impl MapKey {
    /// Converts `value` to a map key or set element, or reports the specific
    /// part that cannot be one, with the path to reach it.
    pub fn from_value(value: &Value) -> Result<MapKey, InvalidKey> {
        Self::convert(None, value)
    }

    /// `anchor` is the path to `value` from the root value under test, so a
    /// rejection nested several levels down can still be reported precisely.
    /// `None` at the root, since a bare value being tested has no name to
    /// anchor a nested path to; a `Struct` or `Enum` invents one from its own
    /// type name the first time a path is needed.
    fn convert(anchor: Option<&str>, value: &Value) -> Result<MapKey, InvalidKey> {
        match value {
            Value::Unit => Ok(MapKey::Unit),
            Value::Bool(b) => Ok(MapKey::Bool(*b)),
            Value::Int(n) => Ok(MapKey::Int(*n)),
            Value::Duration(ns) => Ok(MapKey::Duration(*ns)),
            Value::Str(s) => Ok(MapKey::Str(s.to_string())),
            Value::Enum(e) => {
                let base = anchor
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{}.{}", short_name(&e.type_name), e.case));
                let mut payload = Vec::with_capacity(e.payload.len());
                for (i, item) in e.payload.iter().enumerate() {
                    payload.push(Self::convert(Some(&format!("{base}({i})")), item)?);
                }
                Ok(MapKey::EnumCase(
                    e.type_name.to_string(),
                    e.case.to_string(),
                    payload,
                ))
            }
            Value::Struct(s) => {
                let base = anchor
                    .map(str::to_string)
                    .unwrap_or_else(|| short_name(&s.type_name).to_string());
                let mut fields = Vec::with_capacity(s.fields.len());
                for (name, field) in &s.fields {
                    let child = Self::convert(Some(&format!("{base}.{name}")), field)?;
                    fields.push((name.to_string(), child));
                }
                Ok(MapKey::Struct(s.type_name.to_string(), fields))
            }
            Value::Array(items) => {
                let base = anchor.unwrap_or_default();
                let mut converted = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    converted.push(Self::convert(Some(&format!("{base}[{i}]")), item)?);
                }
                Ok(MapKey::Array(converted))
            }
            // A `Set`'s elements are already `MapKey`s by construction, so
            // this never fails.
            Value::Set(items) => Ok(MapKey::Set((**items).clone())),
            Value::Map(entries) => {
                let base = anchor.unwrap_or_default();
                let mut converted = BTreeMap::new();
                for (key, item) in entries.iter() {
                    let child = Self::convert(Some(&format!("{base}[{key}]")), item)?;
                    converted.insert(key.clone(), child);
                }
                Ok(MapKey::Map(converted))
            }
            Value::Range {
                start,
                end,
                inclusive_end,
            } => Ok(MapKey::Range {
                start: *start,
                end: *end,
                inclusive_end: *inclusive_end,
            }),
            Value::Float(_) => Err(InvalidKey {
                path: anchor.map(str::to_string).unwrap_or_default(),
                type_name: "Float".to_string(),
            }),
            other => Err(InvalidKey {
                path: anchor.map(str::to_string).unwrap_or_default(),
                type_name: other.type_name(),
            }),
        }
    }

    /// Renders this key back as an ordinary value, for `keys()`, `Set`
    /// iteration, and `toArray()`.
    pub fn to_value(&self) -> Value {
        match self {
            MapKey::Unit => Value::Unit,
            MapKey::Bool(b) => Value::Bool(*b),
            MapKey::Int(n) => Value::Int(*n),
            MapKey::Duration(ns) => Value::Duration(*ns),
            MapKey::Str(s) => Value::Str(s.as_str().into()),
            MapKey::EnumCase(type_name, case, payload) => Value::Enum(Box::new(EnumValue {
                type_name: type_name.as_str().into(),
                case: case.as_str().into(),
                payload: payload.iter().map(MapKey::to_value).collect(),
            })),
            MapKey::Struct(type_name, fields) => Value::Struct(Box::new(StructValue {
                type_name: type_name.as_str().into(),
                fields: fields
                    .iter()
                    .map(|(name, key)| (name.as_str().into(), key.to_value()))
                    .collect(),
            })),
            MapKey::Array(items) => Value::Array(items.iter().map(MapKey::to_value).collect()),
            MapKey::Set(items) => Value::Set(Rc::new(items.clone())),
            MapKey::Map(entries) => Value::Map(Rc::new(
                entries
                    .iter()
                    .map(|(key, value)| (key.clone(), value.to_value()))
                    .collect(),
            )),
            MapKey::Range {
                start,
                end,
                inclusive_end,
            } => Value::Range {
                start: *start,
                end: *end,
                inclusive_end: *inclusive_end,
            },
        }
    }
}

/// The unqualified name shown in a key path, matching how `Value`'s
/// `Display` shortens a struct's fully qualified type name.
fn short_name(qualified: &str) -> &str {
    qualified.rsplit('.').next().unwrap_or(qualified)
}

/// A key displays exactly as the value it represents would, so a `Map`'s
/// entries read the same way here as they would anywhere else in the
/// language.
impl fmt::Display for MapKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_value())
    }
}

/// The builtin `Option`, `Result`, and `Error` values, built and read through
/// the one description of what they are made of.
///
/// `Ok`, `Err`, `Some`, `None`, and an `Error`'s `message` are declared in
/// [`cove_schema::builtins`], which is also where `cove-sema` reads them to
/// check a `match` and to type a pattern's binding. Everything in this
/// workspace that builds one of these values or asks which case a value is
/// goes through the constructors and readers below, so the four case names
/// are stated once and the question "is this an `Ok`?" has one answer.
impl Value {
    /// `Ok(value)`
    pub fn ok(value: Value) -> Value {
        Value::Enum(Box::new(EnumValue {
            type_name: RESULT.name.into(),
            case: OK_CASE.name.into(),
            payload: vec![value],
        }))
    }

    /// `Err(error)`
    pub fn err(error: Value) -> Value {
        Value::Enum(Box::new(EnumValue {
            type_name: RESULT.name.into(),
            case: ERR_CASE.name.into(),
            payload: vec![error],
        }))
    }

    /// `Some(value)`
    pub fn some(value: Value) -> Value {
        Value::Enum(Box::new(EnumValue {
            type_name: OPTION.name.into(),
            case: SOME_CASE.name.into(),
            payload: vec![value],
        }))
    }

    /// `None`
    pub fn none() -> Value {
        Value::Enum(Box::new(EnumValue {
            type_name: OPTION.name.into(),
            case: NONE_CASE.name.into(),
            payload: Vec::new(),
        }))
    }

    /// The builtin `Error` struct.
    pub fn error(message: impl Into<String>) -> Value {
        Value::Struct(Box::new(StructValue {
            type_name: ERROR.name.into(),
            fields: vec![(MESSAGE_FIELD.name.into(), Value::Str(message.into().into()))],
        }))
    }

    /// Whether this is an `Ok`, the success case of a `Result`.
    pub fn is_ok(&self) -> bool {
        self.builtin_case(&RESULT, &OK_CASE).is_some()
    }

    /// Whether this is an `Err`.
    pub fn is_err(&self) -> bool {
        self.builtin_case(&RESULT, &ERR_CASE).is_some()
    }

    /// Whether this is a `Some`.
    pub fn is_some(&self) -> bool {
        self.builtin_case(&OPTION, &SOME_CASE).is_some()
    }

    /// What an `Ok` carries, when this is one.
    ///
    /// The payload is a slice rather than a value because what a caller does
    /// with an empty one differs: the `?` operator answers `()` and a
    /// diagnostic answers nothing at all. The schema says an `Ok` carries
    /// exactly one value, so an empty one is a host that broke its word.
    pub fn ok_payload(&self) -> Option<&[Value]> {
        self.builtin_case(&RESULT, &OK_CASE)
            .map(|case| case.payload.as_slice())
    }

    /// What an `Err` carries, when this is one.
    pub fn err_payload(&self) -> Option<&[Value]> {
        self.builtin_case(&RESULT, &ERR_CASE)
            .map(|case| case.payload.as_slice())
    }

    /// What a `Some` carries, when this is one.
    pub fn some_payload(&self) -> Option<&[Value]> {
        self.builtin_case(&OPTION, &SOME_CASE)
            .map(|case| case.payload.as_slice())
    }

    /// The `message` a builtin `Error` carries, when this is one.
    pub fn error_message(&self) -> Option<&Value> {
        match self {
            Value::Struct(value) if &*value.type_name == ERROR.name => {
                value.get(MESSAGE_FIELD.name)
            }
            _ => None,
        }
    }

    /// This value as `case` of the builtin enum `schema`, when it is one.
    ///
    /// Both halves of the question are asked here: a user enum may declare a
    /// case called `Ok`, and it is not this one.
    fn builtin_case(&self, schema: &BuiltinSchema, case: &CaseSchema) -> Option<&EnumValue> {
        match self {
            Value::Enum(value) if &*value.type_name == schema.name && &*value.case == case.name => {
                Some(value)
            }
            _ => None,
        }
    }

    /// The name shown in diagnostics.
    pub fn type_name(&self) -> String {
        match self {
            Value::Unit => "Unit".into(),
            Value::Bool(_) => "Bool".into(),
            Value::Int(_) => "Int".into(),
            Value::Float(_) => "Float".into(),
            Value::Duration(_) => "Duration".into(),
            Value::Str(_) => "String".into(),
            Value::Array(_) => "Array".into(),
            Value::Vector(_) => "Vector".into(),
            Value::Map(_) => "Map".into(),
            Value::Set(_) => "Set".into(),
            Value::Struct(s) => s.type_name.to_string(),
            Value::Enum(e) => e.type_name.to_string(),
            Value::Closure(_) => "fn".into(),
            Value::Dyn(d) => format!("dyn {}", d.trait_name),
            Value::HostModule(m) => format!("host module `{m}`"),
            Value::Resource(handle) => handle.qualified_type(),
            Value::HostFn { module, op } => format!("host operation `{module}.{op}`"),
            Value::Type(t) => format!("type `{t}`"),
            Value::Range { .. } => "Range".into(),
            Value::TaskScope(_) => "TaskScope".into(),
            Value::Task(_) => "Task".into(),
            Value::Shared(_) => "Shared".into(),
        }
    }

    /// The value a trait object holds, or this value when it is not one.
    ///
    /// A `dyn Trait` wrapper records where a value was converted, and the
    /// checker decides where that is: a written type converts and a lambda's
    /// inferred result does not, though both have type `dyn Trait`. Nothing
    /// a program can ask should be able to tell those two apart, so
    /// everything that compares or renders a value looks through the
    /// wrapper first.
    pub fn erased(&self) -> &Value {
        match self {
            Value::Dyn(d) => d.value.erased(),
            other => other,
        }
    }

    /// Value equality. Identity, when available, is explicit and separate.
    pub fn eq_value(&self, other: &Value) -> bool {
        match (self.erased(), other.erased()) {
            (Value::Unit, Value::Unit) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Duration(a), Value::Duration(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Array(a), Value::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.eq_value(y))
            }
            // Both sides are `BTreeMap`s keyed the same way, so two maps with
            // the same keys line up entry-for-entry once both are in their
            // one true ascending order.
            (Value::Map(a), Value::Map(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|((ka, va), (kb, vb))| ka == kb && va.eq_value(vb))
            }
            // `BTreeSet<MapKey>` already compares as a set of keys.
            (Value::Set(a), Value::Set(b)) => a == b,
            (Value::Struct(a), Value::Struct(b)) => {
                a.type_name == b.type_name
                    && a.fields.len() == b.fields.len()
                    && a.fields
                        .iter()
                        .zip(b.fields.iter())
                        .all(|((_, x), (_, y))| x.eq_value(y))
            }
            (Value::Enum(a), Value::Enum(b)) => {
                a.type_name == b.type_name
                    && a.case == b.case
                    && a.payload.len() == b.payload.len()
                    && a.payload
                        .iter()
                        .zip(b.payload.iter())
                        .all(|(x, y)| x.eq_value(y))
            }
            // Ranges compare by the bounds they were written with, so `0..<3`
            // and `0..2` are distinct values even though they yield the same
            // integers.
            (
                Value::Range {
                    start: a,
                    end: b,
                    inclusive_end: a_inclusive,
                },
                Value::Range {
                    start: c,
                    end: d,
                    inclusive_end: b_inclusive,
                },
            ) => a == c && b == d && a_inclusive == b_inclusive,
            // Two handles are equal when they name the same resource. A
            // handle has no contents to compare, so naming the same thing is
            // the whole of being the same value.
            (Value::Resource(a), Value::Resource(b)) => a.names_same(b),
            // `==` means value equality regardless of mutability, so `Vector`
            // compares its current elements structurally, exactly like
            // `Array`. Storage identity — whether two handles are the same
            // growable buffer — is the separate question `is` answers.
            (Value::Vector(a), Value::Vector(b)) => {
                let a = a.elements.borrow();
                let b = b.elements.borrow();
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.eq_value(y))
            }
            _ => false,
        }
    }
}

/// How a value appears inside string interpolation and `console.println`.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Unit => f.write_str("()"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write_float(f, *x),
            Value::Duration(ns) => write_duration(f, *ns),
            Value::Str(s) => f.write_str(s),
            Value::Array(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Value::Vector(storage) => {
                f.write_str("[")?;
                for (i, item) in storage.elements.borrow().iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Value::Map(entries) => {
                f.write_str("{")?;
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{k}: {v}")?;
                }
                f.write_str("}")
            }
            Value::Set(items) => {
                f.write_str("{")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("}")
            }
            Value::Struct(s) => {
                if &*s.type_name == ERROR.name {
                    return match s.get(MESSAGE_FIELD.name) {
                        Some(Value::Str(m)) => f.write_str(m),
                        _ => f.write_str(ERROR.name),
                    };
                }
                let short = s.type_name.rsplit('.').next().unwrap_or(&s.type_name);
                write!(f, "{short}(")?;
                for (i, (name, value)) in s.fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{name}: {value}")?;
                }
                f.write_str(")")
            }
            Value::Enum(e) => {
                f.write_str(&e.case)?;
                if !e.payload.is_empty() {
                    f.write_str("(")?;
                    for (i, value) in e.payload.iter().enumerate() {
                        if i > 0 {
                            f.write_str(", ")?;
                        }
                        write!(f, "{value}")?;
                    }
                    f.write_str(")")?;
                }
                Ok(())
            }
            // A trait object shows the value it holds: the wrapper is a
            // representation, not something the program put there.
            Value::Dyn(d) => write!(f, "{}", d.value),
            Value::Closure(_) => f.write_str("<fn>"),
            Value::HostModule(m) => write!(f, "<host module {m}>"),
            // A handle prints as what it names, identity included: two
            // connections are told apart by the number the host issued and
            // by nothing else.
            Value::Resource(handle) => write!(f, "<{}>", handle),
            Value::HostFn { module, op } => write!(f, "<host fn {module}.{op}>"),
            Value::Type(t) => write!(f, "<type {t}>"),
            Value::Range {
                start,
                end,
                inclusive_end,
            } => {
                let operator = if *inclusive_end { ".." } else { "..<" };
                write!(f, "{start}{operator}{end}")
            }
            Value::TaskScope(scope) => write!(f, "<task scope {}>", scope.name),
            // A task prints as a handle, never as the value it will produce:
            // that value is observable only through `await` or scope exit.
            Value::Task(_) => f.write_str("<task>"),
            // A `Shared` prints as the handle it is. Showing what it holds
            // would be a read outside a `lock`, which is the one thing the
            // type exists to prevent.
            Value::Shared(_) => f.write_str("<shared>"),
        }
    }
}

/// Renders a `Float` so that it is never mistaken for an `Int`.
///
/// Cove performs no implicit numeric conversions, so a float with no
/// fractional part still shows its point: `4.0`, not `4`. Negative zero keeps
/// its sign, and the non-finite values print as `NaN`, `inf`, and `-inf`.
fn write_float(f: &mut fmt::Formatter<'_>, x: f64) -> fmt::Result {
    if x.is_nan() {
        return f.write_str("NaN");
    }
    if x.is_infinite() {
        return f.write_str(if x.is_sign_negative() { "-inf" } else { "inf" });
    }
    if x.fract() == 0.0 {
        write!(f, "{x:.1}")
    } else {
        write!(f, "{x}")
    }
}

/// Nanoseconds per duration unit, largest first, using the suffixes the lexer
/// accepts.
const DURATION_UNITS: [(i64, &str); 6] = [
    (3_600_000_000_000, "h"),
    (60_000_000_000, "m"),
    (1_000_000_000, "s"),
    (1_000_000, "ms"),
    (1_000, "us"),
    (1, "ns"),
];

/// Renders a `Duration` in the largest unit that divides it exactly.
///
/// A duration no larger unit divides exactly stays in nanoseconds, and a
/// negative duration keeps its sign. Zero has no largest unit, so it prints as
/// `0ns`.
fn write_duration(f: &mut fmt::Formatter<'_>, ns: i64) -> fmt::Result {
    if ns == 0 {
        return f.write_str("0ns");
    }
    for (factor, suffix) in DURATION_UNITS {
        if ns % factor == 0 {
            return write!(f, "{}{suffix}", ns / factor);
        }
    }
    unreachable!("every duration is divisible by one nanosecond")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(value: Value) -> String {
        value.to_string()
    }

    #[test]
    fn a_float_is_never_shown_as_an_int() {
        assert_eq!(shown(Value::Float(4.0)), "4.0");
        assert_eq!(shown(Value::Float(-4.0)), "-4.0");
        assert_eq!(shown(Value::Float(1500.0)), "1500.0");
        assert_eq!(shown(Value::Float(1.5)), "1.5");
        assert_eq!(shown(Value::Float(0.25)), "0.25");
        assert_eq!(shown(Value::Float(-0.75)), "-0.75");
        assert_eq!(shown(Value::Float(0.02)), "0.02");
    }

    #[test]
    fn float_edge_cases_are_explicit() {
        assert_eq!(shown(Value::Float(0.0)), "0.0");
        assert_eq!(shown(Value::Float(-0.0)), "-0.0");
        assert_eq!(shown(Value::Float(f64::INFINITY)), "inf");
        assert_eq!(shown(Value::Float(f64::NEG_INFINITY)), "-inf");
        assert_eq!(shown(Value::Float(f64::NAN)), "NaN");
    }

    #[test]
    fn a_duration_uses_the_largest_unit_that_divides_it() {
        assert_eq!(shown(Value::Duration(0)), "0ns");
        assert_eq!(shown(Value::Duration(1)), "1ns");
        assert_eq!(shown(Value::Duration(1_000)), "1us");
        assert_eq!(shown(Value::Duration(1_000_000)), "1ms");
        assert_eq!(shown(Value::Duration(1_000_000_000)), "1s");
        assert_eq!(shown(Value::Duration(60_000_000_000)), "1m");
        assert_eq!(shown(Value::Duration(3_600_000_000_000)), "1h");
        assert_eq!(shown(Value::Duration(500_000_000)), "500ms");
        assert_eq!(shown(Value::Duration(1_500_000_000)), "1500ms");
        assert_eq!(shown(Value::Duration(90_000_000_000)), "90s");
    }

    #[test]
    fn a_duration_no_larger_unit_divides_stays_in_nanoseconds() {
        assert_eq!(shown(Value::Duration(1_001)), "1001ns");
        assert_eq!(shown(Value::Duration(i64::MAX)), format!("{}ns", i64::MAX));
    }

    #[test]
    fn a_negative_duration_keeps_its_sign() {
        assert_eq!(shown(Value::Duration(-3_600_000_000_000)), "-1h");
        assert_eq!(shown(Value::Duration(-500_000_000)), "-500ms");
        assert_eq!(shown(Value::Duration(-1)), "-1ns");
    }

    fn range(start: i64, end: i64, inclusive_end: bool) -> Value {
        Value::Range {
            start,
            end,
            inclusive_end,
        }
    }

    #[test]
    fn a_range_shows_the_operator_it_was_written_with() {
        assert_eq!(shown(range(0, 3, false)), "0..<3");
        assert_eq!(shown(range(0, 3, true)), "0..3");
        assert_eq!(shown(range(-2, -1, false)), "-2..<-1");
    }

    #[test]
    fn ranges_compare_by_value() {
        assert!(range(0, 3, false).eq_value(&range(0, 3, false)));
        assert!(!range(0, 3, false).eq_value(&range(0, 3, true)));
        assert!(!range(0, 3, false).eq_value(&range(1, 3, false)));
        assert!(!range(0, 3, false).eq_value(&Value::Int(0)));
    }

    #[test]
    fn range_bounds_measure_and_test_membership() {
        let exclusive = RangeBounds::of(0, 3, false);
        assert_eq!(exclusive.len(), 3);
        assert!(!exclusive.is_empty());
        assert!(exclusive.contains(0));
        assert!(exclusive.contains(2));
        assert!(!exclusive.contains(3));
        assert!(!exclusive.contains(-1));

        let inclusive = RangeBounds::of(0, 3, true);
        assert_eq!(inclusive.len(), 4);
        assert!(inclusive.contains(3));
    }

    #[test]
    fn a_reversed_or_empty_range_is_empty() {
        for bounds in [
            RangeBounds::of(3, 0, false),
            RangeBounds::of(3, 0, true),
            RangeBounds::of(0, 0, false),
        ] {
            assert_eq!(bounds.len(), 0);
            assert!(bounds.is_empty());
            assert!(bounds.items().is_empty());
            assert!(!bounds.contains(0));
        }
    }

    #[test]
    fn an_inclusive_range_that_ends_at_the_largest_int_does_not_overflow() {
        let bounds = RangeBounds::of(i64::MAX, i64::MAX, true);
        assert_eq!(bounds.len(), 1);
        assert!(bounds.contains(i64::MAX));
    }

    fn payload_free_case(type_name: &str, case: &str) -> Value {
        Value::Enum(Box::new(EnumValue {
            type_name: type_name.into(),
            case: case.into(),
            payload: Vec::new(),
        }))
    }

    fn point(x: i64, y: i64) -> Value {
        Value::Struct(Box::new(StructValue {
            type_name: "test.Point".into(),
            fields: vec![("x".into(), Value::Int(x)), ("y".into(), Value::Int(y))],
        }))
    }

    #[test]
    fn map_keys_accept_the_primitive_shapes() {
        assert_eq!(MapKey::from_value(&Value::Unit), Ok(MapKey::Unit));
        assert_eq!(
            MapKey::from_value(&Value::Bool(true)),
            Ok(MapKey::Bool(true))
        );
        assert_eq!(MapKey::from_value(&Value::Int(7)), Ok(MapKey::Int(7)));
        assert_eq!(
            MapKey::from_value(&Value::Duration(500)),
            Ok(MapKey::Duration(500))
        );
        assert_eq!(
            MapKey::from_value(&Value::Str("a".into())),
            Ok(MapKey::Str("a".to_string()))
        );
        assert_eq!(
            MapKey::from_value(&payload_free_case("Color", "Red")),
            Ok(MapKey::EnumCase(
                "Color".to_string(),
                "Red".to_string(),
                Vec::new()
            ))
        );
    }

    /// A `Range` is immutable with a stable `eq_value`, so it qualifies as a
    /// map key or set element under the same rule as every other value here.
    #[test]
    fn a_range_is_a_valid_map_key() {
        assert_eq!(
            MapKey::from_value(&Value::Range {
                start: 0,
                end: 3,
                inclusive_end: false,
            }),
            Ok(MapKey::Range {
                start: 0,
                end: 3,
                inclusive_end: false,
            })
        );
        // `0..<3` and `0..2` are distinct keys, exactly as they are distinct
        // values: `eq_value` compares the bounds a range was written with.
        assert_ne!(
            MapKey::from_value(&Value::Range {
                start: 0,
                end: 3,
                inclusive_end: false,
            }),
            MapKey::from_value(&Value::Range {
                start: 0,
                end: 2,
                inclusive_end: true,
            })
        );
    }

    #[test]
    fn a_struct_built_only_from_admissible_fields_is_a_valid_key() {
        let key = MapKey::from_value(&point(1, 2)).expect("a struct of Ints is a valid key");
        assert_eq!(
            key,
            MapKey::Struct(
                "test.Point".to_string(),
                vec![
                    ("x".to_string(), MapKey::Int(1)),
                    ("y".to_string(), MapKey::Int(2)),
                ]
            )
        );
    }

    #[test]
    fn a_struct_nested_inside_a_struct_is_a_valid_key_when_every_field_is() {
        let line = Value::Struct(Box::new(StructValue {
            type_name: "test.Line".into(),
            fields: vec![("from".into(), point(0, 0)), ("to".into(), point(1, 1))],
        }));
        let key = MapKey::from_value(&line).expect("nested structs of Ints are a valid key");
        assert_eq!(
            key,
            MapKey::Struct(
                "test.Line".to_string(),
                vec![
                    (
                        "from".to_string(),
                        MapKey::Struct(
                            "test.Point".to_string(),
                            vec![
                                ("x".to_string(), MapKey::Int(0)),
                                ("y".to_string(), MapKey::Int(0)),
                            ]
                        )
                    ),
                    (
                        "to".to_string(),
                        MapKey::Struct(
                            "test.Point".to_string(),
                            vec![
                                ("x".to_string(), MapKey::Int(1)),
                                ("y".to_string(), MapKey::Int(1)),
                            ]
                        )
                    ),
                ]
            )
        );
    }

    #[test]
    fn an_enum_case_with_an_admissible_payload_is_a_valid_key() {
        let value = Value::Enum(Box::new(EnumValue {
            type_name: "test.Colour".into(),
            case: "Named".into(),
            payload: vec![Value::Str("teal".into())],
        }));
        assert_eq!(
            MapKey::from_value(&value),
            Ok(MapKey::EnumCase(
                "test.Colour".to_string(),
                "Named".to_string(),
                vec![MapKey::Str("teal".to_string())]
            ))
        );
    }

    #[test]
    fn an_array_built_only_from_admissible_elements_is_a_valid_key() {
        let value = Value::Array(vec![Value::Int(1), Value::Int(2)].into());
        assert_eq!(
            MapKey::from_value(&value),
            Ok(MapKey::Array(vec![MapKey::Int(1), MapKey::Int(2)]))
        );
    }

    #[test]
    fn map_keys_reject_a_float_for_a_reason_distinct_from_mutability() {
        let invalid = MapKey::from_value(&Value::Float(1.0)).unwrap_err();
        assert_eq!(invalid.type_name, "Float");
        assert!(invalid.path.is_empty());
        assert!(
            invalid.rule().contains("NaN"),
            "a Float's rejection must cite the broken order, not mutability: {}",
            invalid.rule()
        );
    }

    #[test]
    fn map_keys_reject_a_vector_naming_it_directly_at_the_root() {
        let invalid =
            MapKey::from_value(&Value::Vector(VectorStorage::new(Vec::new()))).unwrap_err();
        assert_eq!(invalid.type_name, "Vector");
        assert!(invalid.path.is_empty());
        assert!(
            invalid.rule().contains("Mutable handles"),
            "{}",
            invalid.rule()
        );
    }

    #[test]
    fn a_struct_containing_a_vector_is_rejected_naming_the_nested_field() {
        let value = Value::Struct(Box::new(StructValue {
            type_name: "test.Point".into(),
            fields: vec![("tags".into(), Value::Vector(VectorStorage::new(Vec::new())))],
        }));
        let invalid = MapKey::from_value(&value).unwrap_err();
        assert_eq!(invalid.type_name, "Vector");
        assert_eq!(invalid.path, "Point.tags");
    }

    #[test]
    fn a_map_key_round_trips_through_to_value() {
        for key in [
            MapKey::Unit,
            MapKey::Bool(false),
            MapKey::Int(42),
            MapKey::Duration(500),
            MapKey::Str("hi".to_string()),
            MapKey::EnumCase("Color".to_string(), "Red".to_string(), Vec::new()),
            MapKey::Array(vec![MapKey::Int(1), MapKey::Int(2)]),
            MapKey::Range {
                start: 0,
                end: 3,
                inclusive_end: false,
            },
            MapKey::Struct(
                "test.Point".to_string(),
                vec![
                    ("x".to_string(), MapKey::Int(1)),
                    ("y".to_string(), MapKey::Int(2)),
                ],
            ),
        ] {
            let value = key.to_value();
            assert_eq!(MapKey::from_value(&value), Ok(key));
        }
    }

    #[test]
    fn a_set_is_a_valid_key_because_its_elements_are_already_map_keys() {
        let inner = Value::Set(Rc::new(BTreeSet::from([MapKey::Int(1), MapKey::Int(2)])));
        assert_eq!(
            MapKey::from_value(&inner),
            Ok(MapKey::Set(BTreeSet::from([
                MapKey::Int(1),
                MapKey::Int(2)
            ])))
        );
    }

    #[test]
    fn a_map_is_a_valid_key_when_every_value_is_admissible() {
        let inner = Value::Map(Rc::new(BTreeMap::from([(
            MapKey::Str("a".to_string()),
            Value::Int(1),
        )])));
        assert_eq!(
            MapKey::from_value(&inner),
            Ok(MapKey::Map(BTreeMap::from([(
                MapKey::Str("a".to_string()),
                MapKey::Int(1)
            )])))
        );
    }

    #[test]
    fn a_map_containing_an_inadmissible_value_is_rejected_naming_the_entry() {
        let inner = Value::Map(Rc::new(BTreeMap::from([(
            MapKey::Str("a".to_string()),
            Value::Vector(VectorStorage::new(Vec::new())),
        )])));
        let invalid = MapKey::from_value(&inner).unwrap_err();
        assert_eq!(invalid.type_name, "Vector");
        assert_eq!(invalid.path, "[a]");
    }

    fn map_of(pairs: Vec<(MapKey, Value)>) -> Value {
        Value::Map(Rc::new(pairs.into_iter().collect()))
    }

    fn set_of(keys: Vec<MapKey>) -> Value {
        Value::Set(Rc::new(keys.into_iter().collect()))
    }

    #[test]
    fn maps_compare_structurally() {
        let a = map_of(vec![(MapKey::Str("x".to_string()), Value::Int(1))]);
        let b = map_of(vec![(MapKey::Str("x".to_string()), Value::Int(1))]);
        let c = map_of(vec![(MapKey::Str("x".to_string()), Value::Int(2))]);
        assert!(a.eq_value(&b));
        assert!(!a.eq_value(&c));
    }

    #[test]
    fn sets_compare_structurally() {
        let a = set_of(vec![MapKey::Int(1), MapKey::Int(2)]);
        let b = set_of(vec![MapKey::Int(2), MapKey::Int(1)]);
        let c = set_of(vec![MapKey::Int(1)]);
        assert!(a.eq_value(&b));
        assert!(!a.eq_value(&c));
    }

    /// `==` means value equality regardless of mutability, so two separately
    /// built `Vector`s with the same elements are equal; a vector with
    /// different elements, or a different length, is not. Storage identity
    /// is the separate question `is` answers, not `eq_value`.
    #[test]
    fn vectors_compare_structurally() {
        let a = Value::Vector(VectorStorage::new(vec![Value::Int(1), Value::Int(2)]));
        let b = Value::Vector(VectorStorage::new(vec![Value::Int(1), Value::Int(2)]));
        let c = Value::Vector(VectorStorage::new(vec![Value::Int(1), Value::Int(3)]));
        let d = Value::Vector(VectorStorage::new(vec![Value::Int(1)]));
        assert!(a.eq_value(&b));
        assert!(!a.eq_value(&c));
        assert!(!a.eq_value(&d));
    }

    /// A vector equals itself under `==` too, even though it is a mutable
    /// handle: `==` never asks the identity question.
    #[test]
    fn a_vector_equals_itself_structurally() {
        let a = Value::Vector(VectorStorage::new(vec![Value::Int(1)]));
        assert!(a.eq_value(&a.clone()));
    }

    #[test]
    fn a_map_shows_entries_in_ascending_key_order() {
        let value = map_of(vec![
            (MapKey::Int(2), Value::Str("b".into())),
            (MapKey::Int(1), Value::Str("a".into())),
        ]);
        assert_eq!(shown(value), "{1: a, 2: b}");
    }

    #[test]
    fn a_set_shows_elements_in_ascending_order() {
        let value = set_of(vec![MapKey::Int(3), MapKey::Int(1), MapKey::Int(2)]);
        assert_eq!(shown(value), "{1, 2, 3}");
    }
}
