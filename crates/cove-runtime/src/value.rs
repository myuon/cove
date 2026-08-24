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
            Value::Range { .. } => "Range".into(),
            Value::TaskScope(_) => "TaskScope".into(),
            Value::Task(_) => "Task".into(),
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
}
