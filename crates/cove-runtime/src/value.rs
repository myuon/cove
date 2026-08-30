//! Runtime values.
//!
//! Assignment and ordinary argument passing use one rule: field-wise shallow
//! copy. That rule is encoded directly in [`Clone`]: cloning a struct or enum
//! copies its fields, cloning an `Array` shares immutable storage, and cloning
//! a `Vector` copies only the handle so aliases observe the same elements and
//! length. Cove never performs an implicit deep copy.
//!
//! # What an embedding host should write
//!
//! A host builds a value through a constructor — [`Value::unit`],
//! [`Value::bool`], [`Value::int`], [`Value::float`], [`Value::duration`],
//! [`Value::string`], [`Value::range_of`], [`Value::array`], [`Value::set`],
//! [`Value::map`], [`Value::structure`], [`Value::enumeration`],
//! [`Value::from_resource`], [`Value::host_fn`], [`Value::host_module`],
//! [`Value::type_value`], [`Value::ok`], [`Value::err`], [`Value::some`],
//! [`Value::none`], [`Value::error`] — and reads one through a reader:
//! [`Value::field`],
//! [`Value::fields`], [`Value::case`], [`Value::payload`], [`Value::items`],
//! [`Value::elements`], [`Value::entries`], [`Value::declared_type`],
//! [`Value::range`], [`Value::resource`], [`Value::host_op`],
//! [`Value::arity`], and the scalar `as_*` family. Between them they cover
//! every shape that crosses the Host API boundary, and none of them says how
//! the runtime holds one.
//!
//! **Matching on a variant is the thing that breaks.** [`Value`]'s variants
//! are `pub`, so nothing prevents it, and every change to what a value *is*
//! has therefore been a source break for the hosts that did: issue #104 moved
//! a struct from a `Box` to an `Rc`, issue #109 put a bound host operation's
//! two names behind one pointer to take every value in the program from forty
//! bytes to twenty-four, issue #121 replaced a closure's parameter list with
//! an arity, and issue #183 replaced an enum payload's `Vec<Value>` with
//! [`Payload`]. Each was invisible through the constructors and visible
//! through a `match`. The readers are issue #186's answer to that; what they
//! promise, what borrowing them forecloses, and what they do with a wrong
//! shape are stated once, on the `impl Value` block that holds them.

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
    /// A struct value.
    ///
    /// The storage is shared and copied on write. Cloning a struct value is
    /// the field-wise shallow copy the Language Card describes, and sharing
    /// the fields until one of them is written is unobservable: a write goes
    /// through a place, [`crate::interp`]'s `Place::with_mut` is the only way
    /// to reach one, and it takes a private copy first when the storage has
    /// another holder. Nothing else can tell the two apart, because `is` is
    /// defined only for `Vector`.
    ///
    /// It is shared because it was copied on every non-mutating method call:
    /// `self` is passed by value, and a copy was a `Box` and a `Vec` and a
    /// clone of every field. That made a method call twice the cost of the
    /// same code with the fields read directly, which issue #99 measured and
    /// issue #104 set out to remove.
    ///
    /// This was a `Box<StructValue>` until issue #104. Nothing a Cove program
    /// can write sees the difference, but an embedder building a struct value
    /// writes `Rc::new` where it wrote `Box::new`, and one matching on the
    /// variant binds an `&Rc<StructValue>` where it bound an
    /// `&Box<StructValue>` — both deref to `StructValue`, so a body that only
    /// reads fields needs no change. Mutating through one needs
    /// [`Rc::make_mut`], which is what keeps the copy private.
    Struct(Rc<StructValue>),
    /// An enum value, including `Option` and `Result`.
    ///
    /// One allocation, not two: the box is the whole of it, because
    /// [`Payload`] holds the arities an ordinary program builds inside the
    /// [`EnumValue`] rather than in a vector beside it. Issue #183 is why,
    /// and `benches/arrayget` is where it shows.
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
    ///
    /// The two names live behind one pointer, exactly as [`Value::Struct`]
    /// and [`Value::Dyn`] hold their contents, and for the same kind of
    /// reason: a variant is as wide as its widest member, and this one held
    /// two fat pointers where every other variant holds at most one. Thirty-
    /// two bytes for the pair set the width of every `Value` in the program,
    /// including the `Int`s the two backends spend most of their time moving.
    ///
    /// The trade is an allocation for a value that is built when a host
    /// operation is *used* as a value — `console.println` bound to a name or
    /// passed as an argument — rather than called in place, which is rare,
    /// against sixteen bytes off every value everywhere.
    ///
    /// An embedder constructing one writes `Value::HostFn(Rc::new(
    /// HostFnValue { module, op }))` where it wrote `Value::HostFn { module,
    /// op }`, and one matching on the variant binds an `&Rc<HostFnValue>`
    /// whose fields have the names and the types they had.
    HostFn(Rc<HostFnValue>),
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

// A `Value` is twenty-four bytes, and nothing else in this file says so.
//
// It was forty until issue #109's audit, because exactly one variant was wide:
// `HostFn` inlined two fat pointers, so the variant that names
// `console.println` set the width of every `Int` both backends move. Boxing it
// was worth `field` −4.9% and `method` −6.3% on the VM and `arith` −8.2% on
// the interpreter — the first change since the VM landed to make both backends
// faster. Width was then measured directly, by widening `Value` with a padding
// variant nothing constructs and running the suite at 24, 32 and 40: about a
// percent per eight bytes. See `docs/VM_ARCHITECTURE.md`, "The value
// representation, audited".
//
// So this is the one number a new variant can undo silently. A second variant
// holding two pointers takes every `Value` in the program back to 32 and costs
// what the audit bought, and nothing about writing it would say so. This
// refuses to compile instead.
//
// Twenty-four is the floor for the variants that exist — `Range` is two `i128`
// halves reduced to `(i64, i64, bool)` whose `bool` niche holds the
// discriminant — and sixteen is the floor for the *language*, since `Int` is a
// full sixty-four bits with overflow a broken invariant, which is why NaN
// boxing and pointer tagging are rejected rather than deferred. Neither number
// is a target to shrink to: 24 → 16 was measured and not taken.
//
// Guarded on the pointer width because every non-scalar variant is one
// pointer, so this says nothing on a 32-bit target.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(
    std::mem::size_of::<Value>() == 24,
    "a Value is 24 bytes; a variant wider than one pointer takes every value \
     in the program back to 32 — see docs/VM_ARCHITECTURE.md, \"The value \
     representation, audited\""
);

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
    /// Whether the type was declared `export opaque struct`, in which case
    /// the value renders as its name alone (ADR 0014).
    ///
    /// The flag rides on the value because rendering is context-free: a
    /// `Display` has no idea which module is watching, and a value formatted
    /// in the module that declares it can be handed to one that may not name
    /// its fields. So the representation is hidden from every reader,
    /// including the declaring module, which publishes a readable form by
    /// exporting a method that builds one.
    pub opaque: bool,
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
    pub payload: Payload,
}

/// What an enum case carries, held inline for the arities that occur.
///
/// This was a `Vec<Value>` until [issue
/// #183](https://github.com/myuon/cove/issues/183). A `Value::Enum` is a
/// `Box<EnumValue>`, so a `Some(x)` cost *two* allocations: the box for the
/// struct, and a vector for a payload of one. Almost every enum case an
/// ordinary program builds carries zero or one value — `Option` and `Result`
/// are the two the language builds constantly, and `benches/arrayget`'s
/// comment says why: an `Option` is how every indexed read answers. Holding
/// the common arities in the box that already exists makes `Some(x)` one
/// allocation instead of two.
///
/// It is an enum rather than a `Box<[Value]>` because a boxed slice still
/// allocates for one element, and rather than a general small-vector because
/// there are exactly two shapes worth naming: no payload, and one. Two or
/// more is a `Box<[Value]>` rather than a `Vec` because a payload is never
/// grown after the case is built — the length is the case declaration's, and
/// nothing in this workspace pushes onto one.
///
/// **This reads as a slice.** [`std::ops::Deref`] and [`std::ops::DerefMut`]
/// to `[Value]`, and `IntoIterator` on `&Payload` and `&mut Payload`, so
/// `payload.len()`, `payload[0]`, `payload.first()`, `for item in &payload`
/// and matching `&*payload` against `[inner]` all mean what they meant. [`fmt::Debug`] is
/// written by hand to print as the list it reads as, so a `Debug` rendering
/// of an enum value is the one it always was.
///
/// An embedder that built one by naming the field writes `payload:
/// Payload::One(value)` or `payload: values.into()` where it wrote `payload:
/// vec![value]`; one that goes through [`Value::enumeration`],
/// [`Value::some`] or [`Value::ok`] writes nothing new, because those take
/// what they took.
#[derive(Clone, Default)]
pub enum Payload {
    /// A case with no payload, such as `None`.
    #[default]
    Empty,
    /// A case carrying exactly one value, such as `Some(x)` or `Err(e)`.
    One(Value),
    /// A case carrying two or more.
    Many(Box<[Value]>),
}

impl Payload {
    /// The payload as a slice, which is what every reader wants of one.
    pub fn as_slice(&self) -> &[Value] {
        match self {
            Payload::Empty => &[],
            Payload::One(value) => std::slice::from_ref(value),
            Payload::Many(values) => values,
        }
    }

    /// The payload as a mutable slice.
    pub fn as_mut_slice(&mut self) -> &mut [Value] {
        match self {
            Payload::Empty => &mut [],
            Payload::One(value) => std::slice::from_mut(value),
            Payload::Many(values) => values,
        }
    }

    /// The payload as an owned vector, for a caller that needs one.
    ///
    /// This allocates, which is the thing the type exists to avoid, so it is
    /// here for the callers that genuinely take ownership rather than as the
    /// way to read one.
    pub fn into_vec(self) -> Vec<Value> {
        match self {
            Payload::Empty => Vec::new(),
            Payload::One(value) => vec![value],
            Payload::Many(values) => values.into_vec(),
        }
    }
}

impl std::ops::Deref for Payload {
    type Target = [Value];

    fn deref(&self) -> &[Value] {
        self.as_slice()
    }
}

impl std::ops::DerefMut for Payload {
    fn deref_mut(&mut self) -> &mut [Value] {
        self.as_mut_slice()
    }
}

impl<'a> IntoIterator for &'a Payload {
    type Item = &'a Value;
    type IntoIter = std::slice::Iter<'a, Value>;

    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

impl<'a> IntoIterator for &'a mut Payload {
    type Item = &'a mut Value;
    type IntoIter = std::slice::IterMut<'a, Value>;

    fn into_iter(self) -> Self::IntoIter {
        self.as_mut_slice().iter_mut()
    }
}

/// Prints as the list a payload reads as, rather than as the variant that
/// happens to be holding it.
///
/// Deliberate: the arity a payload is stored at is an implementation detail
/// of this type, and a `Debug` rendering that named it would make the same
/// enum value print two ways depending on how many values it carries.
impl fmt::Debug for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

impl FromIterator<Value> for Payload {
    fn from_iter<I: IntoIterator<Item = Value>>(values: I) -> Payload {
        let mut values = values.into_iter();
        let Some(first) = values.next() else {
            return Payload::Empty;
        };
        let Some(second) = values.next() else {
            return Payload::One(first);
        };
        let mut rest = Vec::with_capacity(2 + values.size_hint().0);
        rest.push(first);
        rest.push(second);
        rest.extend(values);
        Payload::Many(rest.into_boxed_slice())
    }
}

impl From<Vec<Value>> for Payload {
    fn from(mut values: Vec<Value>) -> Payload {
        match values.len() {
            0 => Payload::Empty,
            1 => Payload::One(
                values
                    .pop()
                    .expect("a vector of length one has a last element"),
            ),
            _ => Payload::Many(values.into_boxed_slice()),
        }
    }
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

/// The two names a [`Value::HostFn`] is: the host module the operation
/// belongs to, and the operation itself.
///
/// Neither is the operation's implementation. A bound host operation is a
/// name the same way [`Value::HostModule`] is, and what it names is looked up
/// through the registry at the call.
#[derive(Clone, Debug)]
pub struct HostFnValue {
    /// The host module, such as `console`.
    pub module: Rc<str>,
    /// The operation's own name, such as `println`.
    pub op: Rc<str>,
}

/// A closure captures its environment by value at creation time.
#[derive(Debug)]
pub struct Closure {
    pub is_async: bool,
    /// How many parameters this closure declares.
    ///
    /// The whole of its signature that anything outside the backend that
    /// built it asks for, and the reason it is a number rather than the
    /// parameter list it used to be. Every reader wanted a count:
    /// [`crate::builtins::Callable::arity`] answers out of this,
    /// `Result.mapError` reads it to decide whether to hand its callback the
    /// error it is replacing, and `builtins::expect_callback` refuses a
    /// callback of the wrong shape in one place for both backends. None of
    /// them wanted a name, a default, a type or a span, and issue #121 is
    /// where each was asked and answered.
    ///
    /// It counts parameters, not arguments a call must supply: a defaulted
    /// or a variadic parameter is one of these like any other, which is what
    /// `params.len()` answered when this was a `Vec<Param>`.
    pub arity: usize,
    pub body: ClosureBody,
    /// The module a closure body resolves names in.
    pub module: Rc<str>,
    pub captures: Vec<(Rc<str>, Value)>,
}

/// Where a closure's body is, which is the one thing about a closure the two
/// backends do not agree on.
///
/// Everything else a closure is — what it captured, how many parameters it
/// declares, which module it resolves names in, whether it is `async` — is
/// the same fact whichever backend made it, and a host that receives one
/// reads those the same way either way. The body is not: the interpreter
/// walks a tree and the VM runs a lowered function, and neither can run the
/// other's.
///
/// **The declaration is part of the body, not part of the closure.** The
/// parameters an interpreted call binds against and the return type it
/// coerces to are syntax, and syntax is one backend's form of a body: a
/// lowered function has neither, because `cove_ir::lower` spent both when it
/// chose the slots and emitted the conversions. Keeping them beside the
/// `Arc<Block>` they came from is what lets every field of a [`Closure`]
/// outside this enum be a fact both backends state the same way, so that
/// reaching syntax means reaching past the one variant that has any — which
/// is the direction issue #109 asks for, and which issue #121 asked for on
/// the `cove_ir` side of the same field.
///
/// So this is an enum rather than a second `Value` variant. Issue #109 asks
/// that the internal representation become *less* exposed to an embedder,
/// not more, and a `Value::LoweredClosure` beside `Value::Closure` would make
/// every host that already handles a callback handle two — while the
/// difference between them is one field that no host reads. A host calls a
/// closure back through [`crate::host::Reentry`], which hands it to the
/// backend that made it, and that backend is the only party that has to know
/// which of these it is.
#[derive(Clone, Debug)]
pub enum ClosureBody {
    /// The syntax [`crate::interp::Interpreter`] walks, and the declaration
    /// it walks it under.
    Tree {
        /// The parameters as source wrote them.
        ///
        /// `Interpreter::bind_params` reads every field of one: the name to
        /// match a labelled argument against, `variadic` to know which
        /// arguments to gather, `default` to evaluate in the callee's
        /// environment when an argument was left out, and `is_var` to bind
        /// the caller's place rather than a copy — which is also what
        /// `Interpreter::call_shared_method` reads off the first parameter
        /// of a `lock` closure. The VM answers that last question from
        /// `cove_ir::Function::params` instead, and has no use for the other
        /// three.
        params: Vec<Param>,
        /// The block to evaluate.
        block: Arc<cove_syntax::ast::Block>,
        /// The declaration this closure came from, and `None` for a lambda,
        /// which has none of its own.
        ///
        /// Read for the written return type: a `dyn Trait` in it is what
        /// tells the interpreter to wrap what the body produced. The lowered
        /// form needs no equivalent, because `cove_ir::lower` emits a
        /// `cove_ir::Inst::MakeDyn` before every return of such a function,
        /// so the answer that leaves a lowered body is already wrapped.
        decl: Option<Arc<FnDecl>>,
    },
    /// The lowered function [`crate::vm::Vm`] runs, addressed in the
    /// [`cove_ir::Program`] that run was given.
    ///
    /// An id and nothing else, because the captures are beside it in
    /// [`Closure::captures`] and the program is the VM's. A closure built by
    /// one run cannot be called by another, which is true of the tree form
    /// as well: both name something a particular run owns.
    Lowered(cove_ir::FunctionId),
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
    /// way, in declaration order, and whether its type is opaque — a key is
    /// rendered back as a value for `keys()` and for `Display`, and a value
    /// of an opaque type shows only its name wherever it is read from.
    Struct(String, Vec<(String, MapKey)>, bool),
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
        // Through the `dyn Trait` wrapper first. Two values `==` calls equal
        // have to be interchangeable as keys, and equality already looks
        // through it, so a written `dyn Trait` and a lambda's inferred one
        // key as the same thing they compare as: the value they hold.
        match value.erased() {
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
                Ok(MapKey::Struct(s.type_name.to_string(), fields, s.opaque))
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
            MapKey::Struct(type_name, fields, opaque) => Value::Struct(Rc::new(StructValue {
                type_name: type_name.as_str().into(),
                fields: fields
                    .iter()
                    .map(|(name, key)| (name.as_str().into(), key.to_value()))
                    .collect(),
                opaque: *opaque,
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

/// The eight builtin names, made once per thread and handed out by
/// reference count.
///
/// `Option` and `Result` are built constantly — every `Array.get`, every
/// `?`, every fallible builtin — and issue #104 stopped each one allocating
/// two `Rc<str>` by keeping the strings in a thread-local list and scanning
/// it for the one asked for. The strings never changed, but the scan stayed:
/// a `Some(x)` cost two thread-local accesses, two `RefCell` borrows, and
/// two linear walks comparing string contents. This is the same idea with
/// the lookup taken out — the names are fields, so a constructor reaches the
/// thread-local once and clones two `Rc`s out of it. Issue #193 records it
/// as the other half of #183, which took the payload's allocation and left
/// this.
///
/// Per thread rather than global, for #104's reason: `Rc` is not shareable
/// across threads, and ADR 0008 gives each task a thread.
struct BuiltinNames {
    result: Rc<str>,
    ok: Rc<str>,
    err: Rc<str>,
    option: Rc<str>,
    some: Rc<str>,
    none: Rc<str>,
    error: Rc<str>,
    message: Rc<str>,
}

thread_local! {
    /// Built on the first `Ok`, `Err`, `Some`, `None` or `Error` this thread
    /// makes, which is the same eight allocations #104's list made lazily,
    /// paid once rather than one name at a time.
    static BUILTIN_NAMES: BuiltinNames = BuiltinNames {
        result: Rc::from(RESULT.name),
        ok: Rc::from(OK_CASE.name),
        err: Rc::from(ERR_CASE.name),
        option: Rc::from(OPTION.name),
        some: Rc::from(SOME_CASE.name),
        none: Rc::from(NONE_CASE.name),
        error: Rc::from(ERROR.name),
        message: Rc::from(MESSAGE_FIELD.name),
    };
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
        BUILTIN_NAMES.with(|names| {
            Value::Enum(Box::new(EnumValue {
                type_name: names.result.clone(),
                case: names.ok.clone(),
                payload: Payload::One(value),
            }))
        })
    }

    /// `Err(error)`
    pub fn err(error: Value) -> Value {
        BUILTIN_NAMES.with(|names| {
            Value::Enum(Box::new(EnumValue {
                type_name: names.result.clone(),
                case: names.err.clone(),
                payload: Payload::One(error),
            }))
        })
    }

    /// `Some(value)`
    pub fn some(value: Value) -> Value {
        BUILTIN_NAMES.with(|names| {
            Value::Enum(Box::new(EnumValue {
                type_name: names.option.clone(),
                case: names.some.clone(),
                payload: Payload::One(value),
            }))
        })
    }

    /// `None`
    pub fn none() -> Value {
        BUILTIN_NAMES.with(|names| {
            Value::Enum(Box::new(EnumValue {
                type_name: names.option.clone(),
                case: names.none.clone(),
                payload: Payload::Empty,
            }))
        })
    }

    /// The builtin `Error` struct.
    pub fn error(message: impl Into<String>) -> Value {
        let message = Value::Str(message.into().into());
        BUILTIN_NAMES.with(|names| {
            Value::Struct(Rc::new(StructValue {
                type_name: names.error.clone(),
                fields: vec![(names.message.clone(), message)],
                opaque: false,
            }))
        })
    }

    /// A value of the declared struct type `type_name`, carrying `fields` in
    /// declaration order.
    ///
    /// `type_name` is the qualified name the declaring module gives it, such
    /// as `rules.policy.PullRequest`: that is the name every value of a
    /// declared type carries, and the name an invocation and the Host API
    /// boundary both check against.
    ///
    /// This exists so that a host building an argument for
    /// [`Vm::invoke`](crate::vm::Vm::invoke) does not have to name
    /// [`StructValue`]'s layout to do it — the `Rc`, the field vector, and in
    /// particular `opaque`, which records that the *declaration* said `export
    /// opaque struct` (ADR 0014) and is therefore not a thing a caller has an
    /// answer for. Issue #109 asks that the internal representation become
    /// less exposed to embedders; this is one place it was exposed for no
    /// reason.
    pub fn structure<N: Into<Rc<str>>>(
        type_name: impl Into<Rc<str>>,
        fields: impl IntoIterator<Item = (N, Value)>,
    ) -> Value {
        Value::Struct(Rc::new(StructValue {
            type_name: type_name.into(),
            fields: fields
                .into_iter()
                .map(|(name, value)| (name.into(), value))
                .collect(),
            opaque: false,
        }))
    }

    /// A value of the declared enum type `type_name`, in the case `case`,
    /// carrying `payload` in the order the case declares it.
    ///
    /// The companion of [`Value::structure`] for the other declared shape.
    /// [`Value::ok`] and the three beside it build the *builtin* enums, whose
    /// case names come from `cove_schema::builtins` and are not a caller's to
    /// choose; this one takes both names because a package's own enum is a
    /// package's own.
    pub fn enumeration(
        type_name: impl Into<Rc<str>>,
        case: impl Into<Rc<str>>,
        payload: impl IntoIterator<Item = Value>,
    ) -> Value {
        Value::Enum(Box::new(EnumValue {
            type_name: type_name.into(),
            case: case.into(),
            payload: payload.into_iter().collect(),
        }))
    }

    /// An `Array` holding `items`, in order.
    ///
    /// The companion of [`Value::structure`], and there for the same reason: a
    /// host that builds an array should not have to know that the elements are
    /// stored behind a shared pointer to a slice.
    pub fn array(items: impl IntoIterator<Item = Value>) -> Value {
        Value::Array(items.into_iter().collect())
    }

    /// A `Set` holding `items`.
    ///
    /// The elements are [`MapKey`]s and not [`Value`]s, and that is the
    /// [`MapKey`] restriction showing through rather than an inconvenience: a
    /// set of values would be a constructor that could fail, and there is
    /// nothing sensible for it to do when it does. A host building one from
    /// its own data writes the key it means — `MapKey::Str(name)` for a set of
    /// names — and a host holding a [`Value`] it did not build converts with
    /// [`MapKey::from_value`], which reports the part that cannot be a key and
    /// the path to reach it.
    ///
    /// Duplicates collapse, exactly as they do for a `Set` a Cove program
    /// builds, and the order the set iterates in is ascending key order
    /// whatever order they arrived in.
    pub fn set(items: impl IntoIterator<Item = MapKey>) -> Value {
        Value::Set(Rc::new(items.into_iter().collect()))
    }

    /// A `Map` holding `entries`.
    ///
    /// The companion of [`Value::set`], with the same reason for taking a
    /// [`MapKey`]: only the key carries the restriction, so the value half is
    /// an ordinary [`Value`]. A later entry under a key an earlier one used
    /// replaces it.
    pub fn map(entries: impl IntoIterator<Item = (MapKey, Value)>) -> Value {
        Value::Map(Rc::new(entries.into_iter().collect()))
    }

    /// `()`, the value a statement and a function with no result answer.
    pub fn unit() -> Value {
        Value::Unit
    }

    /// The `Bool` `b`.
    pub fn bool(b: bool) -> Value {
        Value::Bool(b)
    }

    /// The `Int` `n`.
    ///
    /// A full sixty-four bits, because an `Int` is one: issue #109 measured
    /// the alternatives that are not, and NaN boxing and pointer tagging are
    /// refused rather than deferred because neither can hold every `Int` and
    /// every `Float` at once.
    pub fn int(n: i64) -> Value {
        Value::Int(n)
    }

    /// The `Float` `x`, including every NaN and both zeroes.
    pub fn float(x: f64) -> Value {
        Value::Float(x)
    }

    /// The `Duration` of `nanos` nanoseconds.
    ///
    /// Nanoseconds rather than a [`std::time::Duration`], for the reason
    /// [`Value::as_duration_nanos`] gives on the way out: a Cove duration is
    /// a *signed* count of them, and `-1s` is an ordinary value that
    /// `std::time::Duration` cannot hold.
    pub fn duration(nanos: i64) -> Value {
        Value::Duration(nanos)
    }

    /// The `String` `text`.
    ///
    /// Named for the Cove type and not for Rust's, which is why it takes
    /// anything a string can be made from rather than a `String`
    /// specifically: `Value::string("hi")` copies the characters once and
    /// says nothing about where they end up.
    pub fn string(text: impl Into<Rc<str>>) -> Value {
        Value::Str(text.into())
    }

    /// The range `start..end`, or `start..<end` when `inclusive_end` is
    /// false.
    ///
    /// Both bounds as source writes them, rather than the normalised
    /// half-open pair [`Value::range`] answers with: `1..3` and `1..<4` cover
    /// the same integers and are still two different values, since `==`
    /// compares the bounds a range was written with.
    ///
    /// The name is `range_of` and not `range` because the reader took
    /// `range`, and the readers are what issue #195 shipped.
    pub fn range_of(start: i64, end: i64, inclusive_end: bool) -> Value {
        Value::Range {
            start,
            end,
            inclusive_end,
        }
    }

    /// A handle to a resource the host owns, such as a database connection.
    ///
    /// The companion of [`Value::resource`], and it takes the whole handle
    /// because ADR 0013 decides that a handle *is* a name and every field of
    /// it is part of that name. What it hides is the shared pointer, which is
    /// there so a handle can cross into a task when its schema allows it —
    /// pass either a [`ResourceHandle`] or the `Arc` that
    /// [`ResourceHandle::new`](crate::host::ResourceHandle::new) answers.
    ///
    /// The name is `from_resource` and not `resource` because the reader took
    /// `resource`.
    pub fn from_resource(handle: impl Into<Arc<ResourceHandle>>) -> Value {
        Value::Resource(handle.into())
    }

    /// A bound host operation, such as `console.println`.
    ///
    /// The companion of [`Value::host_op`]. Two names and not an
    /// implementation: what they name is found in the registry at the call.
    pub fn host_fn(module: impl Into<Rc<str>>, op: impl Into<Rc<str>>) -> Value {
        Value::HostFn(Rc::new(HostFnValue {
            module: module.into(),
            op: op.into(),
        }))
    }

    /// A bound host module, such as `console`.
    pub fn host_module(name: impl Into<Rc<str>>) -> Value {
        Value::HostModule(name.into())
    }

    /// A type used as a value, such as `Vector` in `Vector.of(1, 2)`.
    ///
    /// The name is `type_value` and not `type_name` because
    /// [`Value::type_name`] answers the name of the type a value *is*, which
    /// is a different question asked of every value rather than of this one.
    pub fn type_value(name: impl Into<Rc<str>>) -> Value {
        Value::Type(name.into())
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
    /// Whether `other` is a value of the same type as this one, without
    /// naming either type.
    ///
    /// `==` has to refuse a comparison between two types before it compares
    /// two values, and it asked that question by building both type names and
    /// comparing the strings. Two allocations per comparison is a great deal
    /// to pay for an answer that is a discriminant check and, for the two
    /// declared kinds, one string comparison — and a parser compares
    /// characters constantly, so this was measurable (issue #104). The names
    /// are still built for the diagnostic, which happens once.
    pub fn same_type_as(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Struct(left), Value::Struct(right)) => left.type_name == right.type_name,
            (Value::Enum(left), Value::Enum(right)) => left.type_name == right.type_name,
            (Value::Dyn(left), Value::Dyn(right)) => left.trait_name == right.trait_name,
            (Value::Resource(left), Value::Resource(right)) => {
                left.module == right.module && left.type_name == right.type_name
            }
            (Value::HostModule(left), Value::HostModule(right)) => left == right,
            (Value::HostFn(left), Value::HostFn(right)) => {
                left.module == right.module && left.op == right.op
            }
            (Value::Type(left), Value::Type(right)) => left == right,
            // Everything else is one type per variant, so the discriminants
            // answering the same is the whole of the question.
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }

    /// The name of the declared type this value is of, for a struct or an
    /// enum, and `None` for everything else.
    ///
    /// A method declared in a package can only ever be found on one of those
    /// two, so this is what receiver dispatch asks rather than
    /// [`Value::type_name`]: a builtin receiver answers `None` and no name is
    /// built at all, and a declared one hands back the name it already holds.
    pub fn declared_type_name(&self) -> Option<&Rc<str>> {
        match self {
            Value::Struct(value) => Some(&value.type_name),
            Value::Enum(value) => Some(&value.type_name),
            _ => None,
        }
    }

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
            Value::HostFn(host) => {
                format!("host operation `{}.{}`", host.module, host.op)
            }
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
    /// everything that compares, renders, or keys a value looks through the
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

/// Reading a value without naming its representation.
///
/// The constructors above — [`Value::structure`], [`Value::enumeration`],
/// [`Value::array`], [`Value::set`], [`Value::map`], and the builtin four —
/// let a host *build* every shape that crosses the boundary without writing
/// an `Rc`, a `Box`, a field vector or the `opaque` flag. These are the other
/// half: they let a host *read* the same shapes the same way.
///
/// Only one half existed, and the missing half cost something every time the
/// representation moved. Issue #104 made [`Value::Struct`] an
/// `Rc<StructValue>`; issue #109 put [`Value::HostFn`]'s two names behind one
/// pointer and took every value in the program from forty bytes to
/// twenty-four; issue #121 replaced a closure's parameter list with an arity;
/// issue #183 replaced an enum payload's `Vec<Value>` with [`Payload`]. Every
/// one of those was invisible to a host that only built values and a source
/// break for one that read them, because reading meant matching on a variant
/// and a match on a variant is a match on the representation. Issue #186 is
/// where that was written down, and this is its answer.
///
/// **A reader borrows.** It hands back a reference into the value it was
/// asked about and the caller clones what it means to keep, which is what
/// [`Value::ok_payload`] and [`StructValue::get`] already did and what a host
/// wants: a conversion into the host's own types reads each part once and
/// keeps no `Value` at all. Borrowing is the half of this that constrains
/// what can still move, and it constrains it in one direction — every part a
/// reader answers with has to be *stored* as the thing it answers with. A
/// struct's fields can move behind a different pointer, a shared shape table,
/// or an inline arity the way [`Payload`] already did, and none of that is
/// visible here; they cannot become values that are *computed* — unpacked
/// from a tagged word, decoded lazily, or held under a lock — without these
/// signatures changing. A reader that cloned would forbid none of that, and
/// would charge every read for the possibility. [`Value::Vector`] is where
/// the line already falls, and it falls the same way for building: its
/// elements are behind a `RefCell` because the language lets an alias write
/// them, so there is no borrowing reader for one here and no constructor for
/// one above.
///
/// **A wrong shape answers `None`.** Asking an `Int` for its fields is the
/// host's mistake rather than the program's — no Cove code asked for it and
/// none can handle it — so it is not a
/// [`RuntimeError`](crate::error::RuntimeError); and it is not a panic
/// either, because a host converting a value it did not build wants to report
/// what arrived instead. [`Value::type_name`] is what names that in the
/// report. This is the convention the readers that already existed use:
/// [`Value::ok_payload`], [`Value::error_message`] and [`StructValue::get`]
/// all answer `None` to the question they were not the right value for.
///
/// **A reader looks through `dyn Trait`.** Each of these calls
/// [`Value::erased`] first, for the reason that method gives: the wrapper
/// records where a value was converted, nothing a program can ask should be
/// able to tell a written `dyn Trait` from a lambda's inferred one, and
/// [`fmt::Display`] already looks through it — "the wrapper is a
/// representation, not something the program put there". There is no reader
/// for the wrapper itself, which matches the constructors, none of which can
/// build one.
impl Value {
    /// The `Bool` this is.
    pub fn as_bool(&self) -> Option<bool> {
        match self.erased() {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// The `Int` this is.
    ///
    /// A full sixty-four bits, because an `Int` is one and overflow is a
    /// broken invariant rather than a wrap.
    pub fn as_int(&self) -> Option<i64> {
        match self.erased() {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }

    /// The `Float` this is.
    pub fn as_float(&self) -> Option<f64> {
        match self.erased() {
            Value::Float(x) => Some(*x),
            _ => None,
        }
    }

    /// The `Duration` this is, in nanoseconds.
    ///
    /// Nanoseconds rather than a [`std::time::Duration`], because a Cove
    /// duration is a signed count of them: `-1s` is an ordinary value and
    /// `std::time::Duration` cannot hold it.
    pub fn as_duration_nanos(&self) -> Option<i64> {
        match self.erased() {
            Value::Duration(ns) => Some(*ns),
            _ => None,
        }
    }

    /// The `String` this is.
    pub fn as_str(&self) -> Option<&str> {
        match self.erased() {
            Value::Str(text) => Some(text),
            _ => None,
        }
    }

    /// Whether this is `()`.
    pub fn is_unit(&self) -> bool {
        matches!(self.erased(), Value::Unit)
    }

    /// The declared type this value is of — `rules.policy.Decision`, or
    /// `Option` for a builtin — for a struct or an enum, and `None` for
    /// everything else.
    ///
    /// The qualified name [`Value::structure`] and [`Value::enumeration`]
    /// take, which is what a host checks an answer against before reading it
    /// apart. [`Value::declared_type_name`] answers the same question with
    /// the shared handle itself, because the two backends clone it to
    /// dispatch a method; this is the reader, and it does not say what the
    /// handle is made of.
    pub fn declared_type(&self) -> Option<&str> {
        self.erased().declared_type_name().map(|name| &**name)
    }

    /// The field `name` of a struct value.
    ///
    /// `None` both when this is not a struct and when the struct declares no
    /// such field, because a host has the same thing to say about either and
    /// [`Value::type_name`] is what says it: "`Int` carries no field
    /// `policy`" and "`rules.policy.Decision` carries no field `polciy`" are
    /// the same sentence with the name filled in.
    pub fn field(&self, name: &str) -> Option<&Value> {
        match self.erased() {
            Value::Struct(value) => value.get(name),
            _ => None,
        }
    }

    /// A struct value's fields, in declaration order, and `None` when this is
    /// not a struct — which is also how a host asks whether it is one.
    ///
    /// Declaration order rather than the order anything asked for: it is the
    /// order [`Value::structure`] was handed and the order the declaration
    /// states, so a host reading a struct positionally reads what a host
    /// building one wrote.
    ///
    /// An `export opaque struct` (ADR 0014) answers here like any other. The
    /// flag governs *rendering*, because a `Display` has no idea which module
    /// is watching; a host holding the value has already been handed it, and
    /// hiding the fields from it would hide them from the very code the
    /// module exported the value to.
    pub fn fields(&self) -> Option<impl Iterator<Item = (&str, &Value)> + '_> {
        match self.erased() {
            Value::Struct(value) => Some(value.fields.iter().map(|(name, v)| (&**name, v))),
            _ => None,
        }
    }

    /// The case of an enum value, such as `Some`, `Err`, or `Require`.
    ///
    /// The case alone, unqualified, exactly as [`Value::enumeration`] takes
    /// it; [`Value::declared_type`] is the other half of the name.
    pub fn case(&self) -> Option<&str> {
        match self.erased() {
            Value::Enum(value) => Some(&value.case),
            _ => None,
        }
    }

    /// What an enum value's case carries, in the order the case declares it.
    ///
    /// A slice, and an empty one for a case that carries nothing, for the
    /// reason [`Value::ok_payload`] gives: what a caller does with an empty
    /// payload differs and only the caller knows which. Those four ask a
    /// builtin question — "is this an `Ok`?" — and answer the payload as a
    /// consequence; this one is asked of a value whose case the caller reads
    /// for itself with [`Value::case`], which is what a package's own enum
    /// needs.
    pub fn payload(&self) -> Option<&[Value]> {
        match self.erased() {
            Value::Enum(value) => Some(value.payload.as_slice()),
            _ => None,
        }
    }

    /// An `Array`'s elements, in order.
    ///
    /// The companion of [`Value::array`]. A `Vector` answers `None` and that
    /// is not an oversight: its elements are behind a `RefCell` because an
    /// alias may write them, so nothing can hand out a plain slice of them,
    /// and there is no constructor for one either.
    /// [`Value::vector_elements`] is how a vector is read — a guard rather
    /// than a slice, which is what a part behind a cell can answer with.
    pub fn items(&self) -> Option<&[Value]> {
        match self.erased() {
            Value::Array(items) => Some(items),
            _ => None,
        }
    }

    /// A `Set`'s elements, in ascending key order.
    ///
    /// [`MapKey`]s and not [`Value`]s, for the reason [`Value::set`] gives on
    /// the way in: the restriction is real, and showing it is better than a
    /// reader that pretends a set holds anything. [`MapKey::to_value`]
    /// converts one back.
    ///
    /// Ascending key order whatever order they were inserted in, which is the
    /// order a Cove program iterating the same set sees.
    pub fn elements(&self) -> Option<impl Iterator<Item = &MapKey> + '_> {
        match self.erased() {
            Value::Set(items) => Some(items.iter()),
            _ => None,
        }
    }

    /// A `Map`'s entries, in ascending key order.
    ///
    /// The companion of [`Value::map`], with the same split: only the key
    /// carries the [`MapKey`] restriction, so the value half is an ordinary
    /// [`Value`].
    pub fn entries(&self) -> Option<impl Iterator<Item = (&MapKey, &Value)> + '_> {
        match self.erased() {
            Value::Map(entries) => Some(entries.iter()),
            _ => None,
        }
    }

    /// A `Range`'s bounds, half-open.
    ///
    /// [`RangeBounds`] rather than the three fields the variant holds,
    /// because `..` and `..<` are two ways of writing one range: `1..3` and
    /// `1..<4` cover the same integers, and a host asking what a range covers
    /// should not have to normalise them itself. The bounds are `i128` so
    /// that an inclusive `i64::MAX` end cannot overflow.
    pub fn range(&self) -> Option<RangeBounds> {
        match *self.erased() {
            Value::Range {
                start,
                end,
                inclusive_end,
            } => Some(RangeBounds::of(start, end, inclusive_end)),
            _ => None,
        }
    }

    /// The resource handle this is.
    ///
    /// [`ResourceHandle`] is the answer rather than something this hides,
    /// because ADR 0013 decides that a handle *is* a name: "every field of it
    /// is part of the name", and there is no field for state because the
    /// state is the host's. What this hides is the `Arc`, which is there so
    /// that a handle can cross into a task when its schema allows it.
    pub fn resource(&self) -> Option<&ResourceHandle> {
        match self.erased() {
            Value::Resource(handle) => Some(handle),
            _ => None,
        }
    }

    /// The module and operation a bound host operation names, such as
    /// `("console", "println")`.
    ///
    /// Two names and not an implementation: a bound host operation is a name
    /// the way [`Value::HostModule`] is, and what it names is found in the
    /// registry at the call. This is the reader for the variant issue #109
    /// boxed to buy the sixteen bytes — a host that matched `Value::HostFn {
    /// module, op }` had to be rewritten, and one that had called this would
    /// not have noticed.
    pub fn host_op(&self) -> Option<(&str, &str)> {
        match self.erased() {
            Value::HostFn(host) => Some((&host.module, &host.op)),
            _ => None,
        }
    }

    /// How many parameters a closure value declares.
    ///
    /// Parameters and not arguments a call must supply: a defaulted or a
    /// variadic parameter counts like any other. A host that was handed a
    /// callback asks this to refuse one of the wrong shape before calling it
    /// back through [`Reentry`](crate::host::Reentry), which is the only way
    /// to call one, since the body belongs to the backend that made it. This
    /// is the reader for the field issue #121 replaced with a count, and it
    /// is the whole of a closure a host has any business reading.
    pub fn arity(&self) -> Option<usize> {
        match self.erased() {
            Value::Closure(closure) => Some(closure.arity),
            _ => None,
        }
    }

    /// A `Vector`'s elements, in order, for as long as the guard is held.
    ///
    /// The companion of [`Value::items`], which answers `None` for a
    /// `Vector` because its elements sit behind a cell — an alias may write
    /// them, so nothing can hand out a plain `&[Value]` of them. Issue #196
    /// records that as "the one place the borrow-based reader design cannot
    /// reach"; ADR 0028 decision 7 is where it is reached, and this is the
    /// shape of the answer: a part whose storage will not sit still is handed
    /// out as an opaque guard, and the guard is public API.
    ///
    /// [`Elements`] derefs to `[Value]`, so a host reads a vector the way it
    /// reads an array. Holding one *borrows* the vector: drop it before
    /// calling back into Cove through
    /// [`Reentry`](crate::host::Reentry), because Cove code that writes the
    /// same vector while the guard is alive is a panic rather than a data
    /// race.
    pub fn vector_elements(&self) -> Option<Elements<'_>> {
        match self.erased() {
            Value::Vector(storage) => Some(Elements(storage.elements.borrow())),
            _ => None,
        }
    }

    /// The trait a `dyn Trait` value was used at, such as `render.Display`.
    ///
    /// The one reader that does *not* look through the wrapper, because it is
    /// the reader for the wrapper. Every other reader and
    /// [`Value::view`] call [`Value::erased`] first, for the reason that
    /// method gives — the wrapper is a representation, not something the
    /// program put there — so this is how a host that genuinely wants to name
    /// the trait in a diagnostic asks for it.
    pub fn dyn_trait(&self) -> Option<&str> {
        match self {
            Value::Dyn(d) => Some(&d.trait_name),
            _ => None,
        }
    }

    /// Classify this value: what *kind* of Cove value it is, and its parts.
    ///
    /// O(1), allocates nothing, and borrows from `self`. It looks through
    /// `dyn Trait` exactly as every reader beside it does, which is why
    /// [`ValueView`] has no `Dyn` variant; [`Value::dyn_trait`] is how a host
    /// asks about the wrapper.
    ///
    /// This is the exhaustive match that sealing takes away, given back
    /// deliberately. See [`ValueView`] for what it promises and when it
    /// breaks.
    ///
    /// A `Vector` borrows its elements for as long as the view is held, for
    /// the reason [`Value::vector_elements`] gives.
    pub fn view(&self) -> ValueView<'_> {
        match self.erased() {
            Value::Unit => ValueView::Unit,
            Value::Bool(b) => ValueView::Bool(*b),
            Value::Int(n) => ValueView::Int(*n),
            Value::Float(x) => ValueView::Float(*x),
            Value::Duration(ns) => ValueView::Duration(*ns),
            Value::Str(text) => ValueView::Str(text),
            Value::Array(items) => ValueView::Array(items),
            Value::Vector(storage) => ValueView::Vector(Elements(storage.elements.borrow())),
            Value::Map(entries) => ValueView::Map(Entries(entries)),
            Value::Set(members) => ValueView::Set(Members(members)),
            Value::Struct(value) => ValueView::Struct(StructView(value)),
            Value::Enum(value) => ValueView::Enum(EnumView(value)),
            Value::Closure(closure) => ValueView::Closure(ClosureView(closure)),
            Value::HostModule(name) => ValueView::HostModule(name),
            Value::HostFn(host) => ValueView::HostFn {
                module: &host.module,
                op: &host.op,
            },
            Value::Resource(handle) => ValueView::Resource(handle),
            Value::Type(name) => ValueView::Type(name),
            Value::Range {
                start,
                end,
                inclusive_end,
            } => ValueView::Range(RangeBounds::of(*start, *end, *inclusive_end)),
            Value::Task(task) => ValueView::Task(TaskView(task)),
            Value::TaskScope(scope) => ValueView::TaskScope(TaskScopeView(scope)),
            Value::Shared(_) => ValueView::Shared(SharedView(std::marker::PhantomData)),
            // `erased` looks through every wrapper, including a wrapper
            // holding a wrapper, so control never arrives here.
            Value::Dyn(_) => unreachable!("`Value::erased` answers no `dyn` wrapper"),
        }
    }
}

/// What kind of Cove value this is: the stable public classification, and the
/// exhaustive match a host is allowed to write.
///
/// # Why this exists
///
/// [`Value`]'s variants are sealed (ADR 0028 decision 6), which takes away a
/// real safety property: a host that matched every variant got a compile
/// error when a new one arrived. Issue #196 raises exactly that objection.
/// This is the answer, and it is a better answer than the thing it replaces,
/// because today one enum carries two unrelated kinds of change and a host
/// cannot tell them apart. Moving a struct from a `Box` to an `Rc` (issue
/// #104) and "Cove has a new kind of value" arrive at a host as the same
/// compile error.
///
/// After this they are different events:
///
/// - a **representation** change is invisible — nothing here names an `Rc`, a
///   `Box`, a slot, a heap object or a tag, so how the runtime holds a value
///   may move without a host noticing;
/// - a **language** change is a compile error at every `match` — a new kind
///   of Cove value is a new variant here, and that is the right way round.
///
/// # It is exhaustive on purpose
///
/// This is deliberately **not** `#[non_exhaustive]`, and that is the whole
/// point. A `#[non_exhaustive]` view would give back the syntax of an
/// exhaustive match and none of its value: every host would carry a `_` arm,
/// and the compile error that a new kind of value *should* cause would never
/// happen anywhere.
///
/// The cost is real and is accepted: this is a second place every new kind of
/// Cove value must be added, and adding one is a breaking change for every
/// embedder. Forgetting is a compile error inside this crate, at
/// [`Value::view`], which is the good case.
///
/// # What it promises
///
/// Each payload borrows from the value or copies out of it, and building one
/// allocates nothing — so every part named here must still be *stored* as the
/// thing it answers with. That is a promise about a materialized boundary
/// value and not about how the VM holds one: ADR 0028 separates the two, and
/// the parts that will actually move — slots, heap objects, dynamic values —
/// are not [`Value`] and never reach here.
///
/// A part whose storage sits behind a cell is answered as an opaque guard
/// rather than a borrow: [`Elements`] is the one that exists, and it is what
/// lets `Vector` be viewed at all.
///
/// There is no `Dyn` variant, because [`Value::view`] looks through the
/// wrapper like every reader beside it. [`Value::dyn_trait`] answers the
/// trait name for a host that wants it.
#[derive(Clone, Debug)]
pub enum ValueView<'a> {
    /// `()`
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A duration in nanoseconds, signed, exactly as
    /// [`Value::as_duration_nanos`] answers it.
    Duration(i64),
    Str(&'a str),
    /// A fixed-length immutable sequence.
    Array(&'a [Value]),
    /// A growable sequence, borrowed for as long as the view is held.
    Vector(Elements<'a>),
    Map(Entries<'a>),
    Set(Members<'a>),
    Struct(StructView<'a>),
    /// An enum value, including `Option` and `Result`.
    Enum(EnumView<'a>),
    /// A callback. A host calls one back through
    /// [`Reentry`](crate::host::Reentry) and never directly, since the body
    /// belongs to the backend that made it.
    Closure(ClosureView<'a>),
    /// A bound host module such as `console`.
    HostModule(&'a str),
    /// A bound host operation such as `console.println`.
    HostFn {
        /// The host module, such as `console`.
        module: &'a str,
        /// The operation's own name, such as `println`.
        op: &'a str,
    },
    /// A handle to a resource the host owns. ADR 0013 decides that the handle
    /// is a name and that every field of it is part of that name, which is
    /// why the whole of it is the answer.
    Resource(&'a ResourceHandle),
    /// A type used as a value, such as `Vector` in `Vector.of(1, 2)`.
    Type(&'a str),
    /// An integer range, normalised to half-open bounds.
    Range(RangeBounds),
    /// A handle to a spawned task. Its value is reachable only through
    /// `await` or through the scope settling it.
    Task(TaskView<'a>),
    /// The scope `scope tasks { ... }` binds.
    TaskScope(TaskScopeView<'a>),
    /// Mutable state more than one task may reach. Its contents are reachable
    /// only through `lock`, so there is nothing here to read.
    Shared(SharedView<'a>),
}

/// A `Vector`'s elements, borrowed.
///
/// Reads as `[Value]`, so a host reads a vector the way it reads an array:
/// `elements.len()`, `elements[0]`, `for value in &elements`.
///
/// It is a guard and not a slice because the elements sit behind a cell — the
/// language lets an alias write them — and a guard is ADR 0028's general
/// answer for a part whose storage will not sit still. Holding one borrows
/// the vector, so drop it before letting Cove code write the same vector.
pub struct Elements<'a>(std::cell::Ref<'a, Vec<Value>>);

impl Clone for Elements<'_> {
    /// Another guard onto the same elements, which is a second shared borrow
    /// and never a copy of them.
    fn clone(&self) -> Self {
        Elements(std::cell::Ref::clone(&self.0))
    }
}

impl std::ops::Deref for Elements<'_> {
    type Target = [Value];

    fn deref(&self) -> &[Value] {
        &self.0
    }
}

impl fmt::Debug for Elements<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl<'a, 'b> IntoIterator for &'b Elements<'a> {
    type Item = &'b Value;
    type IntoIter = std::slice::Iter<'b, Value>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// A `Map`'s entries, in ascending key order.
///
/// Opaque so that the key-ordered storage behind it stays the runtime's
/// business; what it promises is the order, which is the order a Cove program
/// iterating the same map sees.
#[derive(Clone, Copy, Debug)]
pub struct Entries<'a>(&'a BTreeMap<MapKey, Value>);

impl<'a> Entries<'a> {
    /// How many entries the map holds.
    pub fn len(self) -> usize {
        self.0.len()
    }

    /// Whether the map holds none.
    pub fn is_empty(self) -> bool {
        self.0.is_empty()
    }

    /// What `key` maps to, if anything.
    pub fn get(self, key: &MapKey) -> Option<&'a Value> {
        self.0.get(key)
    }

    /// The entries, in ascending key order.
    pub fn iter(self) -> impl Iterator<Item = (&'a MapKey, &'a Value)> {
        self.0.iter()
    }
}

impl<'a> IntoIterator for Entries<'a> {
    type Item = (&'a MapKey, &'a Value);
    type IntoIter = std::collections::btree_map::Iter<'a, MapKey, Value>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// A `Set`'s elements, in ascending key order.
///
/// [`MapKey`]s and not [`Value`]s, for the reason [`Value::set`] gives on the
/// way in: the restriction is real, and showing it is better than pretending
/// a set holds anything.
#[derive(Clone, Copy, Debug)]
pub struct Members<'a>(&'a BTreeSet<MapKey>);

impl<'a> Members<'a> {
    /// How many elements the set holds.
    pub fn len(self) -> usize {
        self.0.len()
    }

    /// Whether the set holds none.
    pub fn is_empty(self) -> bool {
        self.0.is_empty()
    }

    /// Whether `member` is one of them.
    pub fn contains(self, member: &MapKey) -> bool {
        self.0.contains(member)
    }

    /// The elements, in ascending key order.
    pub fn iter(self) -> impl Iterator<Item = &'a MapKey> {
        self.0.iter()
    }
}

impl<'a> IntoIterator for Members<'a> {
    type Item = &'a MapKey;
    type IntoIter = std::collections::btree_set::Iter<'a, MapKey>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// A struct value's name and fields.
#[derive(Clone, Copy, Debug)]
pub struct StructView<'a>(&'a StructValue);

impl<'a> StructView<'a> {
    /// The qualified name of the declared type, such as
    /// `rules.policy.PullRequest` — the name [`Value::structure`] takes.
    pub fn type_name(self) -> &'a str {
        &self.0.type_name
    }

    /// The field `name`, or `None` when the struct declares no such field.
    pub fn field(self, name: &str) -> Option<&'a Value> {
        self.0.get(name)
    }

    /// The fields, in declaration order.
    pub fn fields(self) -> impl Iterator<Item = (&'a str, &'a Value)> {
        self.0.fields.iter().map(|(name, value)| (&**name, value))
    }

    /// How many fields the struct declares.
    pub fn len(self) -> usize {
        self.0.fields.len()
    }

    /// Whether the struct declares no fields at all.
    pub fn is_empty(self) -> bool {
        self.0.fields.is_empty()
    }

    /// Whether the declaration said `export opaque struct` (ADR 0014), which
    /// governs how the value *renders* and nothing else.
    ///
    /// The fields are readable here whatever this answers, for the reason
    /// [`Value::fields`] gives: a host holding the value has already been
    /// handed it.
    pub fn is_opaque(self) -> bool {
        self.0.opaque
    }
}

/// An enum value's name, case, and payload.
#[derive(Clone, Copy, Debug)]
pub struct EnumView<'a>(&'a EnumValue);

impl<'a> EnumView<'a> {
    /// The qualified name of the declared type, or `Option` / `Result` for
    /// the builtins.
    pub fn type_name(self) -> &'a str {
        &self.0.type_name
    }

    /// The case, unqualified: `Some`, `Err`, `Require`.
    pub fn case(self) -> &'a str {
        &self.0.case
    }

    /// What the case carries, in the order the case declares it, and an empty
    /// slice for a case that carries nothing.
    pub fn payload(self) -> &'a [Value] {
        self.0.payload.as_slice()
    }
}

/// What a host may read of a callback.
///
/// The body is not here and cannot be: the interpreter walks a tree and the
/// VM runs a lowered function, and calling one is
/// [`Reentry`](crate::host::Reentry)'s job because only the backend that made
/// a closure can run it.
#[derive(Clone, Copy, Debug)]
pub struct ClosureView<'a>(&'a Closure);

impl ClosureView<'_> {
    /// How many parameters the closure declares — parameters, not arguments a
    /// call must supply, so a defaulted or a variadic one counts like any
    /// other.
    pub fn arity(self) -> usize {
        self.0.arity
    }

    /// Whether it was declared `async`.
    pub fn is_async(self) -> bool {
        self.0.is_async
    }
}

/// What a host may read of a task handle.
#[derive(Clone, Copy, Debug)]
pub struct TaskView<'a>(&'a Task);

impl<'a> TaskView<'a> {
    /// Trace identity, unique across the run.
    pub fn id(self) -> u64 {
        self.0.id
    }

    /// The name of the scope that owns the task.
    pub fn scope(self) -> &'a str {
        &self.0.scope
    }

    /// Position in spawn order within that scope, counting from one.
    pub fn position(self) -> usize {
        self.0.position
    }
}

/// What a host may read of a task scope.
#[derive(Clone, Copy, Debug)]
pub struct TaskScopeView<'a>(&'a TaskScope);

impl<'a> TaskScopeView<'a> {
    /// The name the scope is bound to.
    pub fn name(self) -> &'a str {
        &self.0.name
    }
}

/// A `Shared` cell, which has nothing readable on it.
///
/// Its contents are reachable only through `lock`, and showing them here
/// would be a read outside one — the single thing the type exists to prevent.
/// The variant is in [`ValueView`] so that a host can *tell* a `Shared` from
/// everything else, which is all a host can do with one.
///
/// It carries the borrow and no accessor, so the cell it was made from is not
/// reachable through it — deliberately, since reaching it is what `lock` is
/// for.
#[derive(Clone, Copy, Debug)]
pub struct SharedView<'a>(std::marker::PhantomData<&'a SharedCell>);

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
                // An opaque type renders as its name and nothing else. Its
                // fields are the module's own business, and a rendering is
                // read by whoever the string reaches, so showing them here
                // would publish through `println` what the checker refuses
                // to publish through a field access.
                if s.opaque {
                    return f.write_str(short);
                }
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
            Value::HostFn(host) => write!(f, "<host fn {}.{}>", host.module, host.op),
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

    /// Inlining the common arities cost an `EnumValue` nothing at all.
    ///
    /// A `Payload` is three variants, one of them a whole [`Value`], and it
    /// is still twenty-four bytes — the width of the `Vec` it replaced —
    /// because `Value`'s discriminant lives in a `bool` niche with room to
    /// spare and `Payload`'s fits beside it. So `Some(x)` lost an allocation
    /// and gained no width, and `Marker::visit`'s
    /// `size_of::<EnumValue>()` charge means what it meant.
    ///
    /// Asserted rather than remembered because it is not obvious and because
    /// a future variant of either enum could take it away silently: a
    /// `Payload` wider than a `Vec` makes every enum value's box bigger, and
    /// this is the only place that would say so.
    #[test]
    fn a_payload_is_no_wider_than_the_vector_it_replaced() {
        assert_eq!(size_of::<Payload>(), size_of::<Vec<Value>>());
        assert_eq!(size_of::<Payload>(), 24);
        assert_eq!(size_of::<EnumValue>(), 56);
    }

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
            payload: Payload::Empty,
        }))
    }

    fn point(x: i64, y: i64) -> Value {
        Value::Struct(Rc::new(StructValue {
            type_name: "test.Point".into(),
            fields: vec![("x".into(), Value::Int(x)), ("y".into(), Value::Int(y))],
            opaque: false,
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
                ],
                false
            )
        );
    }

    #[test]
    fn a_struct_nested_inside_a_struct_is_a_valid_key_when_every_field_is() {
        let line = Value::Struct(Rc::new(StructValue {
            type_name: "test.Line".into(),
            fields: vec![("from".into(), point(0, 0)), ("to".into(), point(1, 1))],
            opaque: false,
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
                            ],
                            false
                        )
                    ),
                    (
                        "to".to_string(),
                        MapKey::Struct(
                            "test.Point".to_string(),
                            vec![
                                ("x".to_string(), MapKey::Int(1)),
                                ("y".to_string(), MapKey::Int(1)),
                            ],
                            false
                        )
                    ),
                ],
                false
            )
        );
    }

    #[test]
    fn an_enum_case_with_an_admissible_payload_is_a_valid_key() {
        let value = Value::Enum(Box::new(EnumValue {
            type_name: "test.Colour".into(),
            case: "Named".into(),
            payload: Payload::One(Value::Str("teal".into())),
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
        let value = Value::Struct(Rc::new(StructValue {
            type_name: "test.Point".into(),
            fields: vec![("tags".into(), Value::Vector(VectorStorage::new(Vec::new())))],
            opaque: false,
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
                false,
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

    /// Every shape a constructor builds, read back through a reader, with no
    /// variant named on either side.
    ///
    /// This is the round trip issue #186 asked for: the constructors were the
    /// only half that existed, so a host could build a boundary value without
    /// naming `Rc<StructValue>` and could not read one back the same way.
    #[test]
    fn every_shape_a_constructor_builds_reads_back_through_a_reader() {
        let structure = Value::structure(
            "rules.policy.Decision",
            [("policy", Value::Int(1)), ("findings", Value::array([]))],
        );
        assert_eq!(structure.declared_type(), Some("rules.policy.Decision"));
        assert_eq!(structure.field("policy").and_then(Value::as_int), Some(1));
        assert!(structure.field("absent").is_none());
        assert_eq!(
            structure
                .fields()
                .expect("a struct has fields")
                .map(|(name, _)| name)
                .collect::<Vec<_>>(),
            ["policy", "findings"],
            "declaration order, which is the order the constructor was handed"
        );

        let enumeration = Value::enumeration(
            "rules.policy.ReviewPolicy",
            "Require",
            [Value::Int(2), Value::Str("large change".into())],
        );
        assert_eq!(
            enumeration.declared_type(),
            Some("rules.policy.ReviewPolicy")
        );
        assert_eq!(enumeration.case(), Some("Require"));
        assert_eq!(enumeration.payload().map(<[Value]>::len), Some(2));

        let array = Value::array([Value::Int(1), Value::Int(2)]);
        assert_eq!(
            array.items().map(|items| items.len()),
            Some(2),
            "an `Array` reads as the slice it is"
        );

        let set = Value::set([MapKey::Int(2), MapKey::Int(1), MapKey::Int(2)]);
        assert_eq!(
            set.elements()
                .expect("a set has elements")
                .collect::<Vec<_>>(),
            [&MapKey::Int(1), &MapKey::Int(2)],
            "ascending key order, and a duplicate collapsed"
        );

        let map = Value::map([
            (MapKey::Int(2), Value::Str("b".into())),
            (MapKey::Int(1), Value::Str("a".into())),
        ]);
        assert_eq!(
            map.entries()
                .expect("a map has entries")
                .map(|(key, value)| (key.clone(), value.to_string()))
                .collect::<Vec<_>>(),
            [
                (MapKey::Int(1), "a".to_string()),
                (MapKey::Int(2), "b".to_string()),
            ],
            "ascending key order, which is what a Cove program iterating sees"
        );

        assert_eq!(
            Value::ok(Value::Int(1)).payload().map(<[Value]>::len),
            Some(1),
            "a builtin enum reads through the general reader as well as the four"
        );
        assert_eq!(
            Value::none().payload().map(<[Value]>::len),
            Some(0),
            "a case that carries nothing reads as an empty slice"
        );
        assert_eq!(
            Value::error("broken")
                .error_message()
                .and_then(Value::as_str),
            Some("broken")
        );
    }

    /// Every scalar reads back as the Rust value it was built from.
    #[test]
    fn a_scalar_reads_back_as_itself() {
        assert_eq!(Value::Bool(true).as_bool(), Some(true));
        assert_eq!(Value::Int(-7).as_int(), Some(-7));
        assert_eq!(Value::Float(1.5).as_float(), Some(1.5));
        assert_eq!(Value::Duration(-1_000).as_duration_nanos(), Some(-1_000));
        assert_eq!(Value::Str("hello".into()).as_str(), Some("hello"));
        assert!(Value::Unit.is_unit());
        assert_eq!(
            Value::Range {
                start: 1,
                end: 3,
                inclusive_end: true,
            }
            .range()
            .map(|bounds| (bounds.start, bounds.end)),
            Some((1, 4)),
            "`..` and `..<` normalise to one half-open pair"
        );
    }

    /// A wrong shape answers `None` rather than panicking, which is the
    /// convention `ok_payload` and `StructValue::get` already set.
    #[test]
    fn a_reader_asked_of_the_wrong_shape_answers_none() {
        let value = Value::Int(1);
        assert_eq!(value.as_str(), None);
        assert_eq!(value.as_bool(), None);
        assert_eq!(value.declared_type(), None);
        assert!(value.field("policy").is_none());
        assert!(value.fields().is_none());
        assert_eq!(value.case(), None);
        assert!(value.payload().is_none());
        assert!(value.items().is_none());
        assert!(value.elements().is_none());
        assert!(value.entries().is_none());
        assert!(value.range().is_none());
        assert!(value.resource().is_none());
        assert_eq!(value.host_op(), None);
        assert_eq!(value.arity(), None);
        assert!(!value.is_unit());

        // A `Vector` is not an `Array` and says so, because its elements are
        // behind a `RefCell` and nothing can hand out a slice of them.
        let vector = Value::Vector(VectorStorage::new(vec![Value::Int(1)]));
        assert!(vector.items().is_none());
    }

    /// A reader looks through a `dyn Trait` wrapper, exactly as `Display` and
    /// `eq_value` do: nothing a program can ask tells a written conversion
    /// from a lambda's inferred one.
    #[test]
    fn a_reader_looks_through_a_trait_object() {
        let wrapped = Value::Dyn(Rc::new(DynValue {
            trait_name: "render.Display".into(),
            value: Value::structure("app.Point", [("x", Value::Int(1))]),
        }));
        assert_eq!(wrapped.declared_type(), Some("app.Point"));
        assert_eq!(wrapped.field("x").and_then(Value::as_int), Some(1));

        let wrapped_scalar = Value::Dyn(Rc::new(DynValue {
            trait_name: "render.Display".into(),
            value: Value::Str("shown".into()),
        }));
        assert_eq!(wrapped_scalar.as_str(), Some("shown"));
    }

    /// The three representation changes that were embedder source breaks, read
    /// through the API that would have hidden each of them.
    ///
    /// Not a test of behaviour so much as of what the signatures admit: every
    /// assertion below names a shape the *language* has and no type the
    /// runtime chose to hold it in. Issue #104 moved a struct from a `Box` to
    /// an `Rc`, issue #109 moved a bound host operation's two names behind one
    /// pointer, and issue #183 replaced an enum payload's `Vec<Value>` with
    /// [`Payload`] — a host written against these lines would have compiled
    /// unchanged across all three, and the point of writing them down is that
    /// the next such change has to keep them compiling.
    #[test]
    fn a_reader_hides_each_representation_change_that_was_a_source_break() {
        // #104: the struct behind an `Rc`, where a host once wrote
        // `let Value::Struct(s) = value` and bound a `&Box<StructValue>`.
        let decision = Value::structure("app.Decision", [("reviewers", Value::Int(2))]);
        assert_eq!(decision.field("reviewers").and_then(Value::as_int), Some(2));

        // #183: the payload at each of the three arities `Payload` holds, read
        // as one slice whichever it is.
        for (payload, len) in [
            (Vec::new(), 0),
            (vec![Value::Int(1)], 1),
            (vec![Value::Int(1), Value::Int(2)], 2),
        ] {
            let value = Value::enumeration("app.Verdict", "Case", payload);
            assert_eq!(value.case(), Some("Case"));
            assert_eq!(value.payload().map(<[Value]>::len), Some(len));
        }

        // #109: the two names of a bound host operation, which were inline
        // fields of the variant until they cost every value in the program
        // sixteen bytes.
        let bound = Value::HostFn(Rc::new(HostFnValue {
            module: "console".into(),
            op: "println".into(),
        }));
        assert_eq!(bound.host_op(), Some(("console", "println")));

        // #121, the fourth of the same kind: a closure's parameter list became
        // a count, and a host only ever wanted the count.
        let closure = Value::Closure(Rc::new(Closure {
            is_async: false,
            arity: 2,
            body: ClosureBody::Lowered(cove_ir::FunctionId(0)),
            module: "app".into(),
            captures: Vec::new(),
        }));
        assert_eq!(closure.arity(), Some(2));
    }

    /// One value of every kind the language has, classified.
    ///
    /// The list is exhaustive on purpose: [`ValueView`] is not
    /// `#[non_exhaustive]`, so a new kind of Cove value is a compile error at
    /// `Value::view` first and here second, and this is where the count is
    /// checked. A `_` arm below would defeat both.
    #[test]
    fn a_view_names_every_kind_of_value() {
        let cell = SharedCell::new(crate::Transfer::Int(1));
        let scope = TaskScope::new("work".into());
        let task = Task::settled(Value::unit());
        let kinds = [
            Value::unit(),
            Value::bool(true),
            Value::int(1),
            Value::float(1.0),
            Value::duration(1),
            Value::string("hi"),
            Value::array([Value::int(1)]),
            Value::Vector(VectorStorage::new(vec![Value::int(1)])),
            Value::map([(MapKey::Int(1), Value::int(2))]),
            Value::set([MapKey::Int(1)]),
            Value::structure("app.Point", [("x", Value::int(1))]),
            Value::some(Value::int(1)),
            Value::Closure(Rc::new(Closure {
                is_async: true,
                arity: 2,
                body: ClosureBody::Lowered(cove_ir::FunctionId(0)),
                module: "app".into(),
                captures: Vec::new(),
            })),
            Value::host_module("console"),
            Value::host_fn("console", "println"),
            Value::from_resource(ResourceHandle {
                module: "database".to_string(),
                type_name: "Connection".to_string(),
                id: 1,
                task_safe: true,
            }),
            Value::type_value("Vector"),
            Value::range_of(1, 3, false),
            Value::Task(task),
            Value::TaskScope(scope),
            Value::Shared(cell),
        ];
        let mut seen = 0;
        for value in &kinds {
            seen += 1;
            match value.view() {
                ValueView::Unit => assert_eq!(seen, 1),
                ValueView::Bool(b) => assert!(b),
                ValueView::Int(n) => assert_eq!(n, 1),
                ValueView::Float(x) => assert_eq!(x, 1.0),
                ValueView::Duration(ns) => assert_eq!(ns, 1),
                ValueView::Str(text) => assert_eq!(text, "hi"),
                ValueView::Array(items) => assert_eq!(items.len(), 1),
                // The one part that answers a guard rather than a borrow,
                // and it reads as the slice it guards.
                ValueView::Vector(elements) => assert_eq!(elements[0].as_int(), Some(1)),
                ValueView::Map(entries) => {
                    assert_eq!(
                        entries.get(&MapKey::Int(1)).and_then(Value::as_int),
                        Some(2)
                    )
                }
                ValueView::Set(members) => assert!(members.contains(&MapKey::Int(1))),
                ValueView::Struct(value) => {
                    assert_eq!(value.type_name(), "app.Point");
                    assert!(!value.is_opaque());
                    assert_eq!(value.field("x").and_then(Value::as_int), Some(1));
                }
                ValueView::Enum(value) => {
                    assert_eq!((value.type_name(), value.case()), ("Option", "Some"));
                    assert_eq!(value.payload().len(), 1);
                }
                ValueView::Closure(closure) => {
                    assert!(closure.is_async());
                    assert_eq!(closure.arity(), 2);
                }
                ValueView::HostModule(name) => assert_eq!(name, "console"),
                ValueView::HostFn { module, op } => {
                    assert_eq!((module, op), ("console", "println"))
                }
                ValueView::Resource(handle) => assert_eq!(handle.id, 1),
                ValueView::Type(name) => assert_eq!(name, "Vector"),
                ValueView::Range(bounds) => assert_eq!((bounds.start, bounds.end), (1, 3)),
                ValueView::Task(task) => assert_eq!(task.scope(), "this call"),
                ValueView::TaskScope(scope) => assert_eq!(scope.name(), "work"),
                ValueView::Shared(_) => assert_eq!(seen, 21),
            }
        }
        assert_eq!(seen, kinds.len());
    }

    /// The view looks through a `dyn Trait` wrapper, exactly as every reader
    /// beside it does — which is why there is no `Dyn` variant to match, and
    /// why the trait name is a reader instead.
    #[test]
    fn a_view_looks_through_a_trait_object() {
        let wrapped = Value::Dyn(Rc::new(DynValue {
            trait_name: "render.Display".into(),
            value: Value::structure("app.Point", [("x", Value::int(1))]),
        }));
        let ValueView::Struct(value) = wrapped.view() else {
            panic!("a wrapped struct views as a struct, not as a wrapper");
        };
        assert_eq!(value.type_name(), "app.Point");
        assert_eq!(wrapped.dyn_trait(), Some("render.Display"));
        assert_eq!(Value::int(1).dyn_trait(), None);

        // Twice over: `erased` looks through a wrapper holding a wrapper, so
        // `view` never has one to answer.
        let twice = Value::Dyn(Rc::new(DynValue {
            trait_name: "render.Display".into(),
            value: wrapped,
        }));
        assert!(matches!(twice.view(), ValueView::Struct(_)));
    }

    /// Viewing a value shares nothing new.
    ///
    /// The collector infers a root by comparing the references it can see
    /// against `Rc::strong_count`, so a view that cloned an `Rc` would add a
    /// reference no walk can see and turn a dead object into a live one — a
    /// leak — while one that a walk *could* see twice would conceal the
    /// shortfall that makes a Rust local a root, which is a use-after-free.
    /// A view borrows and copies scalars, and this is what says so.
    #[test]
    fn a_view_changes_no_reference_count() {
        let storage = VectorStorage::new(vec![Value::int(1)]);
        let vector = Value::Vector(storage.clone());
        let fields = Rc::new(StructValue {
            type_name: "app.Point".into(),
            fields: vec![("x".into(), Value::int(1))],
            opaque: false,
        });
        let structure = Value::Struct(fields.clone());

        let before = (Rc::strong_count(&storage), Rc::strong_count(&fields));
        let views = (vector.view(), structure.view(), vector.vector_elements());
        assert_eq!(
            before,
            (Rc::strong_count(&storage), Rc::strong_count(&fields))
        );
        drop(views);
        assert_eq!(
            before,
            (Rc::strong_count(&storage), Rc::strong_count(&fields))
        );
    }

    /// Every scalar a host can build, built without naming a variant and read
    /// back through the reader it mirrors.
    ///
    /// There was no way to build an `Int` at all until this: a host wrote
    /// `Value::Int(3)` because there was nothing else to write, which is why
    /// sealing the variants had to wait for the constructors. Each line pairs
    /// a constructor with the reader ADR 0028 calls its mirror, so a
    /// constructor that stopped agreeing with its reader fails here.
    #[test]
    fn every_scalar_has_a_constructor_that_mirrors_its_reader() {
        assert!(Value::unit().is_unit());
        assert_eq!(Value::bool(true).as_bool(), Some(true));
        assert_eq!(Value::int(i64::MIN).as_int(), Some(i64::MIN));
        assert_eq!(Value::float(-0.5).as_float(), Some(-0.5));
        assert_eq!(
            Value::duration(-1_000_000_000).as_duration_nanos(),
            Some(-1_000_000_000)
        );
        assert_eq!(Value::string("hi").as_str(), Some("hi"));
        assert_eq!(Value::string(String::from("hi")).as_str(), Some("hi"));

        // `1..3` and `1..<4` cover the same integers and are still two
        // different values, so the constructor takes the bounds as written
        // and the reader answers the normalised pair.
        let bounds = Value::range_of(1, 3, true).range().expect("a range");
        assert_eq!((bounds.start, bounds.end), (1, 4));

        assert_eq!(
            Value::host_fn("console", "println").host_op(),
            Some(("console", "println"))
        );
        assert_eq!(
            Value::host_module("console").type_name(),
            "host module `console`"
        );
        assert_eq!(Value::type_value("Vector").type_name(), "type `Vector`");

        let handle = ResourceHandle {
            module: "database".to_string(),
            type_name: "Connection".to_string(),
            id: 7,
            task_safe: true,
        };
        let value = Value::from_resource(handle.clone());
        assert!(value.resource().expect("a resource").names_same(&handle));
        // The `Arc` a host already holds is accepted as it stands, so nothing
        // has to clone a handle to build a value out of one.
        assert!(Value::from_resource(Arc::new(handle)).resource().is_some());
    }
}
