//! Runtime values.
//!
//! Assignment and ordinary argument passing use one rule: field-wise shallow
//! copy. That rule is encoded directly in [`Clone`]: cloning a struct or enum
//! copies its fields, cloning an `Array` shares immutable storage, and cloning
//! a `Vector` copies only the handle so aliases observe the same elements and
//! length. Cove never performs an implicit deep copy.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

use cove_syntax::ast::{FnDecl, Param};

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
    /// Immutable in the MVP.
    Map(Rc<BTreeMap<MapKey, Value>>),
    /// Immutable in the MVP.
    Set(Rc<Vec<Value>>),
    /// A struct value. Cloning copies each field by that field's own rule.
    Struct(Box<StructValue>),
    /// An enum value, including `Option` and `Result`.
    Enum(Box<EnumValue>),
    /// A callback is an ordinary handle value.
    Closure(Rc<Closure>),
    /// A bound host module such as `console`.
    HostModule(Rc<str>),
    /// A bound host operation such as `console.println`.
    HostFn {
        module: Rc<str>,
        op: Rc<str>,
    },
    /// A type used as a value, such as `Vector` in `Vector.of(1, 2)`.
    Type(Rc<str>),
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

/// A closure captures its environment by value at creation time.
#[derive(Debug)]
pub struct Closure {
    pub is_async: bool,
    pub params: Vec<Param>,
    /// `None` for lambdas, which have no declaration of their own.
    pub decl: Option<Rc<FnDecl>>,
    pub body: Rc<cove_syntax::ast::Block>,
    /// The module a closure body resolves names in.
    pub module: Rc<str>,
    pub captures: Vec<(Rc<str>, Value)>,
}

/// A value usable as a `Map` key. Mutable handles are not valid map keys.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MapKey {
    Bool(bool),
    Int(i64),
    Str(String),
    /// A payload-free enum case, keyed by `(type, case)`.
    EnumCase(String, String),
}

impl Value {
    /// `Ok(value)`
    pub fn ok(value: Value) -> Value {
        Value::Enum(Box::new(EnumValue {
            type_name: "Result".into(),
            case: "Ok".into(),
            payload: vec![value],
        }))
    }

    /// `Err(error)`
    pub fn err(error: Value) -> Value {
        Value::Enum(Box::new(EnumValue {
            type_name: "Result".into(),
            case: "Err".into(),
            payload: vec![error],
        }))
    }

    /// `Some(value)`
    pub fn some(value: Value) -> Value {
        Value::Enum(Box::new(EnumValue {
            type_name: "Option".into(),
            case: "Some".into(),
            payload: vec![value],
        }))
    }

    /// `None`
    pub fn none() -> Value {
        Value::Enum(Box::new(EnumValue {
            type_name: "Option".into(),
            case: "None".into(),
            payload: Vec::new(),
        }))
    }

    /// The builtin `Error` struct.
    pub fn error(message: impl Into<String>) -> Value {
        Value::Struct(Box::new(StructValue {
            type_name: "Error".into(),
            fields: vec![("message".into(), Value::Str(message.into().into()))],
        }))
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
            Value::HostModule(m) => format!("host module `{m}`"),
            Value::HostFn { module, op } => format!("host operation `{module}.{op}`"),
            Value::Type(t) => format!("type `{t}`"),
        }
    }

    /// Value equality. Identity, when available, is explicit and separate.
    pub fn eq_value(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Unit, Value::Unit) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Duration(a), Value::Duration(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Array(a), Value::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.eq_value(y))
            }
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
            Value::Float(x) => write!(f, "{x}"),
            Value::Duration(ns) => write!(f, "{ns}ns"),
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
                    match k {
                        MapKey::Bool(b) => write!(f, "{b}")?,
                        MapKey::Int(n) => write!(f, "{n}")?,
                        MapKey::Str(s) => write!(f, "{s}")?,
                        MapKey::EnumCase(t, c) => write!(f, "{t}.{c}")?,
                    }
                    write!(f, ": {v}")?;
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
                if &*s.type_name == "Error" {
                    return match s.get("message") {
                        Some(Value::Str(m)) => f.write_str(m),
                        _ => f.write_str("Error"),
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
            Value::Closure(_) => f.write_str("<fn>"),
            Value::HostModule(m) => write!(f, "<host module {m}>"),
            Value::HostFn { module, op } => write!(f, "<host fn {module}.{op}>"),
            Value::Type(t) => write!(f, "<type {t}>"),
        }
    }
}
