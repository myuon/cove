//! The machine-readable schemas the compiler and the runtime both read.
//!
//! There are two of them, and they are here for the same reason. The Host API
//! schema, below, is what a host module declares about itself; [`builtins`] is
//! what the language declares about its own types. Neither could live in
//! `cove-runtime` or in `cove-sema`, because each is read by both and the
//! dependency between those two runs one way.
//!
//! The two keep separate vocabularies on purpose. A host operation's
//! signature is monomorphic, so [`HostType`] has no type parameters and needs
//! none; a builtin's is generic, receiver-relative, and sometimes
//! higher-order, so [`builtins::BuiltinType`] has all three and a host
//! signature would have no use for them. [`builtins`] argues that at length.
//!
//! ADR 0001 states what the Host API half has to carry:
//!
//! > A machine-readable Host API schema is shared by the compiler, runtime,
//! > and CLI. Each operation describes its argument, result, and error types;
//! > capability; serialization and resource ownership; cancellation and
//! > recordability; and whether it is a read, reversible write, or
//! > irreversible write.
//!
//! and the Language Card adds the sentence that makes the schema load-bearing
//! for tasks: "Host resources declare task-safety in their Host API schema."
//!
//! "Shared by the compiler, runtime, and CLI" is why this is a crate of its
//! own and not a module of `cove-runtime`. `cove-sema` checks a host call
//! against the same description the boundary dispatches it through, and the
//! dependency between the two runs one way: the compiler must not gain a
//! dependency on the runtime to say what it already knows. So the description
//! moved below both of them, where each can read it, and `cove-runtime`
//! re-exports it so a host written against the runtime still names one crate.
//! Two lists that must agree and cannot see each other drift silently, which
//! this repository has already paid for once; the fix that scales is one
//! list.
//!
//! A schema entry is Rust data, not a parsed declaration, because a host is
//! written in Rust: `HostApi::schema` returns a `'static` table, so a module
//! and its declaration of itself cannot drift apart at run time. The shipped
//! hosts' tables are in [`hosts`], and the module that implements each of
//! them returns the table from there rather than one of its own.
//!
//! Only the parts of ADR 0001's list that something reads are modelled here.
//! Serialization is left out: every value that crosses the boundary is an
//! ordinary runtime value, and a field nothing consults is a claim nothing
//! checks.
//!
//! Resource ownership is no longer among the omissions. A host may declare
//! types of its own — [`TypeSchema`] for the ones that are plain data, and
//! [`ResourceSchema`] for the ones the host keeps on the far side of the
//! boundary — and ADR 0013 makes the second of those the whole of a resource
//! handle's contract: which operations it answers, what capability each of
//! them needs, and whether the handle may cross a task boundary.
//!
//! What a *value* has to be for a declared type to admit it is not here. That
//! question needs values, which this crate has none of; `cove_runtime::schema`
//! answers it, on the side of the boundary where values live.

use std::fmt;

pub mod builtins;
pub mod hosts;

pub use builtins::{builtin, free_builtin, is_builtin_type};
pub use hosts::{module, shipped, HostSchemas};

/// A type in a Host API signature, written in Cove's source vocabulary.
///
/// This is a small enum rather than `cove_syntax::ast::Type` because an
/// `ast::Type` is a *parsed* type: every node carries a span into a source
/// file and its path segments are identifiers with spans of their own, all of
/// which would have to be invented for an operation that has no source to
/// point at. The rendering vocabulary is the same one — [`fmt::Display`]
/// produces the form the type would be written in Cove — so a signature
/// printed from a schema entry reads exactly like a signature written by
/// hand.
///
/// Add a variant when a host needs it; an unused variant is a type nobody can
/// produce. That used to read "the variants cover exactly the types the
/// shipped hosts use", and [`HostType::Set`] and [`HostType::Map`] are why it
/// no longer does: no shipped host has needed either, and an embedder did
/// ([issue #153](https://github.com/myuon/cove/issues/153)). An embedder is
/// not a lesser kind of host — embedding is why `HostApi` is a trait — so the
/// list is what a host may declare rather than what this workspace happens to
/// ship, and `crates/cove-runtime/tests/embedding.rs` is where the two new
/// ones are produced and consumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostType {
    /// `Unit`, the value an operation returns when it returns nothing.
    Unit,
    /// `Bool`.
    Bool,
    /// `Int`, a signed 64-bit integer.
    Int,
    /// `String`.
    String,
    /// `Duration`, a signed count of nanoseconds.
    Duration,
    /// `Error`, the builtin error struct.
    Error,
    /// `Array<T>`, the fixed-length immutable sequence.
    Array(&'static HostType),
    /// `Set<T>`, the key-ordered immutable set.
    ///
    /// `T` must be one [`HostType::may_be_a_key`] allows, because a `Set`
    /// element is a map key: [`ModuleSchema::validate`] is what refuses a
    /// declaration that says otherwise, and it refuses it where the schema is
    /// read rather than where a value is.
    Set(&'static HostType),
    /// `Map<K, V>`, the key-ordered immutable map.
    ///
    /// `K` carries the same restriction a [`HostType::Set`] element does, and
    /// for the same reason; `V` carries none.
    Map(&'static HostType, &'static HostType),
    /// `Option<T>`.
    Option(&'static HostType),
    /// `Result<T, E>`. Expected failure is part of an operation's result
    /// type, exactly as it is in Cove source, rather than a second channel
    /// beside it.
    Result(&'static HostType, &'static HostType),
    /// A type the host declares, written qualified: `http.Response`.
    ///
    /// The name is the one Cove source writes, module included, because that
    /// is what a signature in a diagnostic has to read as. Whether it names a
    /// [`TypeSchema`] or a [`ResourceSchema`] is the host's business; a
    /// signature says only which type it is.
    Named(&'static str),
    /// Any value at all.
    ///
    /// This is not a missing type: it is the type of an operation whose
    /// meaning does not depend on which value it was given. `http.json`
    /// renders whatever it is handed, and a callback a host stores and calls
    /// later is a value the host never looks inside.
    ///
    /// What it promises, and what it costs, are different at the two ends of
    /// a signature, and both are worth stating exactly.
    ///
    /// In a *parameter* it promises that every value is accepted: no
    /// argument of any type is a mistake, the compiler rejects none, and the
    /// boundary rejects none either. Nothing is given up by it, because
    /// there was never a constraint to check.
    ///
    /// In a *result* it says the operation may answer with a value of any
    /// type. That does cost something: from the call onwards the program
    /// holds a value no schema described, so the compiler cannot prove what
    /// a field read off it, a call made on it, or a place it is stored into
    /// will do. Those are checked at run time and by nothing before it.
    /// `cove check` reports each such call rather than letting the silence
    /// pass for a proof — as a note, because a schema declaring `Any` is a
    /// deliberate design decision and not a fault in the program that calls
    /// it.
    Any,
}

impl fmt::Display for HostType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostType::Unit => f.write_str("Unit"),
            HostType::Bool => f.write_str("Bool"),
            HostType::Int => f.write_str("Int"),
            HostType::String => f.write_str("String"),
            HostType::Duration => f.write_str("Duration"),
            HostType::Error => f.write_str("Error"),
            HostType::Array(inner) => write!(f, "Array<{inner}>"),
            HostType::Set(inner) => write!(f, "Set<{inner}>"),
            HostType::Map(key, value) => write!(f, "Map<{key}, {value}>"),
            HostType::Option(inner) => write!(f, "Option<{inner}>"),
            HostType::Result(ok, error) => write!(f, "Result<{ok}, {error}>"),
            HostType::Named(name) => f.write_str(name),
            HostType::Any => f.write_str("Any"),
        }
    }
}

impl HostType {
    /// Whether a value of this type may be a `Map` key or a `Set` element.
    ///
    /// Cove's own rule is `cove_runtime::value::MapKey`'s: "mutable handles
    /// and structs containing them are not valid map keys", because a key's
    /// equality must not change while a collection holds it. That rule is
    /// about a *value*, and this is the most a *name* can say about it.
    ///
    /// Everything made only of the scalar types qualifies, and so does any
    /// composition of qualifying types: an `Array`, an `Option`, a `Result`, a
    /// `Set`, or a `Map` is a key exactly when everything nested inside it is.
    ///
    /// [`HostType::Named`] and [`HostType::Any`] do not, and the reason is the
    /// same in both cases: neither says what its values are made of.
    /// `cove_runtime::schema::Admits` checks a named type by the name the
    /// value carries and deliberately looks no further — ADR 0013's amendment
    /// draws that line — so a schema naming `reviews.PullRequest` has made no
    /// claim about the ten fields behind it, and a `ResourceSchema`'s handle
    /// can never be a key at all. `Any` says less again. A declaration that
    /// promised more than the boundary checks would be a promise nothing
    /// keeps.
    pub fn may_be_a_key(&self) -> bool {
        match self {
            HostType::Unit
            | HostType::Bool
            | HostType::Int
            | HostType::String
            | HostType::Duration
            | HostType::Error => true,
            HostType::Array(item) | HostType::Set(item) | HostType::Option(item) => {
                item.may_be_a_key()
            }
            HostType::Map(key, value) => key.may_be_a_key() && value.may_be_a_key(),
            HostType::Result(ok, error) => ok.may_be_a_key() && error.may_be_a_key(),
            HostType::Named(_) | HostType::Any => false,
        }
    }

    /// The first part of this type that is declared as a key and cannot be
    /// one.
    ///
    /// `Some(t)` names the offending key or element type rather than the
    /// collection around it, because `t` is what a reader has to change.
    fn unkeyable(&self) -> Option<HostType> {
        match self {
            HostType::Set(item) => {
                if item.may_be_a_key() {
                    item.unkeyable()
                } else {
                    Some(**item)
                }
            }
            HostType::Map(key, value) => {
                if key.may_be_a_key() {
                    key.unkeyable().or_else(|| value.unkeyable())
                } else {
                    Some(**key)
                }
            }
            HostType::Array(item) | HostType::Option(item) => item.unkeyable(),
            HostType::Result(ok, error) => ok.unkeyable().or_else(|| error.unkeyable()),
            _ => None,
        }
    }
}

/// A schema that declares something no value can be.
///
/// One kind of fault so far, and it is the one adding `Map` and `Set` to the
/// vocabulary introduced: a key or an element position may only hold a type
/// [`HostType::may_be_a_key`] allows. Everything else a `HostType` can say is
/// satisfiable by construction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaFault {
    /// Where in the module it was found, as a reader would name the place:
    /// `reviews.pull`'s result, or `reviews.PullRequest.labels`.
    pub place: String,
    /// The whole declared type the fault was found in.
    pub declared: HostType,
    /// The part of it that is declared as a key and cannot be one.
    pub key: HostType,
}

impl fmt::Display for SchemaFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is declared `{}`, and `{}` cannot be a `Map` key or a `Set` element",
            self.place, self.declared, self.key
        )
    }
}

/// Whether an operation observes the world or changes it, and whether the
/// change can be taken back.
///
/// ADR 0001 asks each operation to say which of the three it is. The
/// distinction is about the world outside the run, not about the host's own
/// bookkeeping: waiting is a [`Effect::Read`] because nothing outside the run
/// is different afterwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Observes state without changing it.
    Read,
    /// Changes state in a way the same host can put back.
    ReversibleWrite,
    /// Changes state nothing can put back, such as bytes already on a
    /// terminal or a message already sent.
    IrreversibleWrite,
}

impl fmt::Display for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Effect::Read => f.write_str("read"),
            Effect::ReversibleWrite => f.write_str("reversible write"),
            Effect::IrreversibleWrite => f.write_str("irreversible write"),
        }
    }
}

/// One operation of one host module.
///
/// Every field is read by something: `HostRegistry::call` checks `name` and
/// arity before it dispatches, holds a call to `params` before the host sees
/// it and the host to `result` after it answers, `params` and `result` render
/// the signature a diagnostic shows, `capability` is the gate, and
/// `result_is_task_safe` answers the Language Card's rule for values leaving
/// a host call for a task. `cove-sema` reads `params` and `result` too, at
/// the call site, where a mistake still has a span to point at. `effect`,
/// `cancellable`, and `recordable` are the three ADR 0001 facts whose
/// consumers are named but not yet built — `cove replay` for `recordable`,
/// `cove impact` for `effect`, and for `cancellable`, a host that could
/// abandon a call in flight: a cancelled task stops at its next safepoint,
/// which is after the call it is already inside returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationSchema {
    /// The name Cove source calls, such as `println`.
    pub name: &'static str,
    /// Parameter types in declaration order.
    ///
    /// This is a promise both ends hold a call to rather than a label on it:
    /// `cove check` checks each argument at its call site, and the boundary
    /// checks them again for the hosts the checker cannot see.
    pub params: &'static [HostType],
    /// Whether the last parameter is variadic.
    ///
    /// Cove writes a variadic parameter `items: T...` and makes it an
    /// immutable `Array<T>` inside the callee, so it accepts zero or more
    /// arguments: a variadic operation's minimum arity is one less than
    /// `params.len()`.
    pub variadic: bool,
    /// The type the operation produces.
    ///
    /// This is a promise the boundary holds the host to rather than a label
    /// on it: `HostRegistry` checks what the host answered against this
    /// before handing it on, so an operation cannot declare one type and
    /// produce another. `cove_runtime::schema::Admits` says how far the check
    /// goes.
    pub result: HostType,
    /// The capability a host must grant before this operation may be called.
    pub capability: &'static str,
    /// Whether the operation reads, writes reversibly, or writes
    /// irreversibly.
    pub effect: Effect,
    /// Whether abandoning a call that is already in flight is meaningful and
    /// safe. A wait can be abandoned because nothing has happened yet; a
    /// write that has already reached the outside world cannot.
    pub cancellable: bool,
    /// Whether the call's result can be recorded and handed back later
    /// without calling the host again, which is what `cove replay` needs.
    ///
    /// An operation that opens a resource is recordable, because ADR 0013
    /// makes a handle a name rather than a live thing: what the trace records
    /// is the identity the host issued, and a replay hands the same identity
    /// back and answers the calls made on it from the trace too.
    pub recordable: bool,
    /// Whether the value this operation produces may cross a task boundary.
    ///
    /// The Language Card puts this decision here rather than in the value:
    /// "Host resources declare task-safety in their Host API schema."
    pub result_is_task_safe: bool,
}

impl OperationSchema {
    /// The fewest arguments the operation accepts.
    pub fn min_arity(&self) -> usize {
        if self.variadic {
            self.params.len().saturating_sub(1)
        } else {
            self.params.len()
        }
    }

    /// Whether a call with `arity` arguments has the right number of them.
    pub fn accepts(&self, arity: usize) -> bool {
        if self.variadic {
            arity >= self.min_arity()
        } else {
            arity == self.params.len()
        }
    }

    /// The type declared for the argument at `index`.
    ///
    /// A variadic operation's last parameter answers for every argument from
    /// its own position onwards, because that is what `items: T...` means:
    /// one declared `T` and as many arguments of it as the call likes. An
    /// index past a fixed operation's parameters has no declared type, which
    /// is an arity mistake and reported as one.
    pub fn param(&self, index: usize) -> Option<&'static HostType> {
        self.params.get(index).or_else(|| {
            if self.variadic {
                self.params.last()
            } else {
                None
            }
        })
    }

    /// The signature, in the form it would be written in Cove source, without
    /// the module qualifier: `println(String...) -> Result<Unit, Error>`.
    pub fn signature(&self) -> String {
        let mut params = self
            .params
            .iter()
            .map(HostType::to_string)
            .collect::<Vec<_>>();
        if self.variadic {
            if let Some(last) = params.last_mut() {
                last.push_str("...");
            }
        }
        format!("{}({}) -> {}", self.name, params.join(", "), self.result)
    }

    /// How many arguments this operation takes, phrased for a diagnostic.
    pub fn expected_arity(&self) -> String {
        let least = self.min_arity();
        let noun = if least == 1 { "argument" } else { "arguments" };
        if self.variadic {
            format!("at least {least} {noun}")
        } else {
            format!("{least} {noun}")
        }
    }
}

/// One field of a host type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldSchema {
    /// The label Cove source writes in the initializer, such as `method`.
    pub name: &'static str,
    /// The field's type.
    pub ty: HostType,
}

/// The shape of one type a host declares.
///
/// A host type is ordinary data: `http.Method` is an enum whose cases carry
/// nothing, and `http.Route` is a struct initialized with labels, exactly as a
/// Cove struct is. Neither needs a representation of its own — the runtime
/// builds an enum or a struct value whose type name is qualified by the module
/// — so what the schema adds is only the vocabulary: which names exist and
/// what they are made of.
///
/// A type whose values the host keeps rather than hands over is a
/// [`ResourceSchema`] instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypeSchema {
    /// The name Cove source writes after the module, such as `Route`.
    pub name: &'static str,
    /// The cases, for an enum. Empty for a struct.
    pub cases: &'static [&'static str],
    /// The fields, for a struct. Empty for an enum.
    pub fields: &'static [FieldSchema],
}

impl TypeSchema {
    /// Whether this is an enum, which is what having cases means.
    pub fn is_enum(&self) -> bool {
        !self.cases.is_empty()
    }

    /// The initializer, in the form it would be written in Cove source:
    /// `Route(method: http.Method, path: String, handler: Any)`.
    pub fn initializer(&self) -> String {
        let fields = self
            .fields
            .iter()
            .map(|field| format!("{}: {}", field.name, field.ty))
            .collect::<Vec<_>>();
        format!("{}({})", self.name, fields.join(", "))
    }
}

/// One kind of host resource: a value the host owns and Cove only names.
///
/// ADR 0013 states the contract this carries. A handle is an identity, not
/// state: the host keeps whatever a `database.Connection` really is, and Cove
/// holds the name of it. So a resource declares three things and nothing
/// else — what it is called, which operations it answers, and whether the
/// name may cross a task boundary.
///
/// `task_safe` is the Language Card's sentence applied to a host's own types:
/// "Host resources declare task-safety in their Host API schema." A resource
/// whose state the host keeps behind a lock says `true`, and its name then
/// crosses like any other immutable value; one whose state belongs to the
/// task that opened it says `false`, and the name is refused at the boundary
/// with the same diagnostic a vector gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceSchema {
    /// The name Cove source writes after the module, such as `Connection`.
    pub name: &'static str,
    /// Whether a handle to this resource may cross a task boundary.
    pub task_safe: bool,
    /// The operations a handle answers, called as methods on it.
    pub operations: &'static [OperationSchema],
}

impl ResourceSchema {
    /// The operation `name`, if this resource has one.
    pub fn operation(&self, name: &str) -> Option<&'static OperationSchema> {
        self.operations.iter().find(|entry| entry.name == name)
    }
}

/// The name, capability, operations, types, and resources of one host module.
///
/// This is the whole of what a module declares about itself, detached from
/// any module: `cove-sema` reads it with no host to ask and no runtime to
/// depend on, and `cove trace` and `cove replay` read it with nothing running
/// at all. A live module is asked through `HostApi`, whose answers are these
/// same tables.
///
/// # A schema assembled at run time is built once and leaked
///
/// Every field here is `&'static`, and so is every payload a [`HostType`]
/// inside one points at. A schema written as a `const` — every module the
/// toolchain ships, and every one in the tests — costs nothing for that: it
/// was in the binary already, and being [`Copy`] is what lets a caller hold a
/// schema while it goes on reading whatever it asked.
///
/// A module whose shape is only known once the process is running pays,
/// though. A name from configuration, operations from a plugin manifest,
/// resources from a table list discovered at connect time — none of that is
/// `'static`, and `HostApi::module_schema` hands the table back by value, so
/// there is nowhere to borrow from. Such a host assembles its table once, the
/// first time it is asked, leaks it, and hands out the same copy afterwards:
///
/// ```
/// # use cove_schema::{Effect, HostType, ModuleSchema, OperationSchema};
/// # use std::sync::OnceLock;
/// struct Plugin {
///     /// The operations the manifest named, read once at startup.
///     manifest: Vec<String>,
///     /// The table they describe, assembled on the first ask and no other.
///     schema: OnceLock<ModuleSchema>,
/// }
///
/// impl Plugin {
///     fn module_schema(&self) -> ModuleSchema {
///         *self.schema.get_or_init(|| ModuleSchema {
///             name: "plugin",
///             capability: "plugin",
///             operations: Vec::leak(
///                 self.manifest
///                     .iter()
///                     .map(|name| OperationSchema {
///                         name: String::leak(name.clone()),
///                         params: &[HostType::String],
///                         variadic: false,
///                         result: HostType::Result(&HostType::String, &HostType::Error),
///                         capability: "plugin",
///                         effect: Effect::Read,
///                         cancellable: false,
///                         recordable: true,
///                         result_is_task_safe: true,
///                     })
///                     .collect::<Vec<_>>(),
///             ),
///             types: &[],
///             resources: &[],
///         })
///     }
/// }
/// ```
///
/// The [`OnceLock`](std::sync::OnceLock) is the whole of the discipline, and
/// it is what makes the cost a bounded one. A handful of allocations per
/// module for the life of a process is what an in-process embedding
/// registered at startup pays, once; a host that assembles its table inside
/// `module_schema` instead pays it again on every call the registry
/// dispatches, which is not bounded by anything.
/// `crates/cove-runtime/tests/embedding.rs` runs the pattern end to end and
/// asserts the bound.
///
/// # Why the fields are `&'static` and not `&'a`
///
/// [Issue #86](https://github.com/myuon/cove/issues/86) asked for
/// `ModuleSchema<'a>` and called it the principled fix. It is not a fix. It
/// would spread a lifetime through every crate that names this type and
/// leave the leak where it was, because neither of the two things a host
/// could borrow a schema from is available to it.
///
/// It cannot borrow from itself. A host holding `names: Vec<String>` beside
/// `operations: Vec<OperationSchema<'a>>` needs `'a` to be the lifetime of
/// the field next to it, which is a self-referential struct and not
/// something safe Rust builds.
///
/// It cannot borrow from anything longer-lived either, because a registered
/// module is a `Box<dyn HostApi>`, which is `Box<dyn HostApi + 'static>`. It
/// has to be: ADR 0008 gives every spawned task a thread of its own,
/// `std::thread::Builder::spawn` takes a `'static` closure, and that closure
/// holds the `Arc<Runtime>` that holds the registry. A registry that
/// borrowed its modules would be a run that could not spawn a task.
///
/// The representation that *would* remove the leak is the other one: a
/// schema that owns what it describes, so a host keeps one in a field and
/// hands back `&self.schema`. What rules it out is not the reason issue #86
/// gives. `Cow::Borrowed` is const-constructible, so the shipped tables
/// could stay `const` — though the recursive [`HostType`] payloads would
/// need a hand-written `Static | Shared` pair beside it, because
/// `Cow<'static, HostType>` is a layout cycle. What rules it out is the
/// price. Some 260 fields across `hosts.rs` stop being written as literals
/// and start being written as constructor calls, in a table that is
/// hand-written because being read by hand is the point of it. [`Copy`]
/// goes, and with it the shape of every reader that holds a schema while it
/// goes on working: `HostRegistry::host_type` hands the interpreter an entry
/// rather than a borrow precisely because the interpreter is about to
/// evaluate arguments, which it cannot do while borrowing the registry. And
/// a clone of an owned half is a deep copy where a copy of a static one was
/// free. A trait with two implementations pays the same noise for a dynamic
/// call on every read, and gives one description two vocabularies — the
/// drift this crate exists to prevent.
///
/// So the tables stay literals, the readers stay [`Copy`], and the leak
/// stays: bounded, documented here, and exercised by a test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModuleSchema {
    /// The name Cove source uses, such as `console`.
    pub name: &'static str,
    /// The capability a host must grant for this module.
    ///
    /// A capability is a plain name here rather than `cove_sema::Capability`,
    /// because that type belongs to the crate that reads `cove.toml` and this
    /// one sits below it.
    pub capability: &'static str,
    /// Every operation the module exposes.
    pub operations: &'static [OperationSchema],
    /// Every type the module declares.
    pub types: &'static [TypeSchema],
    /// Every kind of resource the module can open.
    pub resources: &'static [ResourceSchema],
}

impl ModuleSchema {
    /// The operation `name`, if this module exposes one.
    pub fn operation(&self, name: &str) -> Option<&'static OperationSchema> {
        self.operations.iter().find(|entry| entry.name == name)
    }

    /// The type `name`, if this module declares one that is plain data.
    pub fn declared_type(&self, name: &str) -> Option<&'static TypeSchema> {
        self.types.iter().find(|entry| entry.name == name)
    }

    /// The kind of resource `name`, if this module can open one.
    pub fn resource(&self, name: &str) -> Option<&'static ResourceSchema> {
        self.resources.iter().find(|entry| entry.name == name)
    }

    /// Whether this module declares `name` as a type of its own, either as
    /// plain data or as a resource it keeps.
    ///
    /// The two are one question wherever a name is being read rather than
    /// used: `http.Response` and `http.Server` are both written the same way
    /// in a signature, and which of them the host keeps is the host's
    /// business.
    pub fn declares_type(&self, name: &str) -> bool {
        self.declared_type(name).is_some() || self.resource(name).is_some()
    }

    /// Whether every type this module declares is one some value could be.
    ///
    /// There is one way to write a type that nothing can satisfy, and
    /// [`HostType::Set`] and [`HostType::Map`] are what introduced it: a `Set`
    /// element and a `Map` key have to satisfy Cove's `MapKey` restriction,
    /// and [`HostType::may_be_a_key`] says which declarations do.
    ///
    /// It is checked here, where a schema is *read*, rather than at the
    /// boundary where a value is. A `Set<reviews.PullRequest>` the boundary
    /// refused would be refused on the first call that carried one, in
    /// production, in whichever operation happened to come first — which is
    /// the failure mode ADR 0017 moved a Host API description out of the
    /// runtime to prevent. Read here it is one sentence naming the field.
    ///
    /// Every table this workspace ships is held to this by
    /// `cove_schema::hosts`'s own tests. An embedder's table is the
    /// embedder's, so an embedder calls this on it — one assertion in the test
    /// that already exists is enough, and
    /// `examples/rules/host/tests/embedding.rs` is where that is written down.
    pub fn validate(&self) -> Result<(), SchemaFault> {
        let operations = self
            .operations
            .iter()
            .map(|entry| (self.name.to_string(), entry))
            .chain(self.resources.iter().flat_map(|resource| {
                resource
                    .operations
                    .iter()
                    .map(move |entry| (format!("{}.{}", self.name, resource.name), entry))
            }));
        for (owner, entry) in operations {
            for (index, param) in entry.params.iter().enumerate() {
                fault(
                    format!("argument {} of `{owner}.{}`", index + 1, entry.name),
                    param,
                )?;
            }
            fault(
                format!("the result of `{owner}.{}`", entry.name),
                &entry.result,
            )?;
        }
        for declared in self.types {
            for field in declared.fields {
                fault(
                    format!("`{}.{}.{}`", self.name, declared.name, field.name),
                    &field.ty,
                )?;
            }
        }
        Ok(())
    }
}

/// The fault `declared` carries at `place`, if it carries one.
fn fault(place: String, declared: &HostType) -> Result<(), SchemaFault> {
    match declared.unkeyable() {
        Some(key) => Err(SchemaFault {
            place,
            declared: *declared,
            key,
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const READ_A_STRING: OperationSchema = OperationSchema {
        name: "read",
        params: &[HostType::String],
        variadic: false,
        result: HostType::Result(&HostType::String, &HostType::Error),
        capability: "documents",
        effect: Effect::Read,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    };

    const PRINT_MANY: OperationSchema = OperationSchema {
        name: "println",
        params: &[HostType::String],
        variadic: true,
        result: HostType::Result(&HostType::Unit, &HostType::Error),
        capability: "console",
        effect: Effect::IrreversibleWrite,
        cancellable: false,
        recordable: true,
        result_is_task_safe: true,
    };

    #[test]
    fn types_render_in_cove_source_form() {
        assert_eq!(HostType::Unit.to_string(), "Unit");
        assert_eq!(HostType::Bool.to_string(), "Bool");
        assert_eq!(HostType::Int.to_string(), "Int");
        assert_eq!(HostType::Duration.to_string(), "Duration");
        assert_eq!(
            HostType::Array(&HostType::String).to_string(),
            "Array<String>"
        );
        assert_eq!(
            HostType::Option(&HostType::String).to_string(),
            "Option<String>"
        );
        assert_eq!(
            HostType::Result(&HostType::Unit, &HostType::Error).to_string(),
            "Result<Unit, Error>"
        );
        assert_eq!(HostType::Set(&HostType::String).to_string(), "Set<String>");
        assert_eq!(
            HostType::Map(&HostType::String, &HostType::Int).to_string(),
            "Map<String, Int>"
        );
    }

    // ------------------------------------------- what may be a key, and why
    //
    // A `Set` element and a `Map` key have to satisfy Cove's `MapKey`
    // restriction. These pin what a *name* can promise about that, which is
    // less than what a value can be held to and is the whole of what a schema
    // gets to say.

    #[test]
    fn a_type_made_of_scalars_may_be_a_key() {
        for scalar in [
            HostType::Unit,
            HostType::Bool,
            HostType::Int,
            HostType::String,
            HostType::Duration,
            HostType::Error,
        ] {
            assert!(scalar.may_be_a_key(), "{scalar}");
        }
        assert!(HostType::Array(&HostType::String).may_be_a_key());
        assert!(HostType::Option(&HostType::Int).may_be_a_key());
        assert!(HostType::Set(&HostType::String).may_be_a_key());
        assert!(HostType::Map(&HostType::String, &HostType::Int).may_be_a_key());
        assert!(HostType::Result(&HostType::Int, &HostType::Error).may_be_a_key());
    }

    /// Neither says what its values are made of, so neither can promise the
    /// one thing a key position needs.
    #[test]
    fn a_named_type_and_any_may_not_be_a_key() {
        assert!(!HostType::Named("reviews.PullRequest").may_be_a_key());
        assert!(!HostType::Any.may_be_a_key());
        assert!(!HostType::Array(&HostType::Any).may_be_a_key());
        assert!(!HostType::Set(&HostType::Named("reviews.PullRequest")).may_be_a_key());
    }

    /// The rule is only about the key half. A map from a name to a pull
    /// request is ordinary, and only a map *keyed* by one is not.
    #[test]
    fn a_module_declaring_a_key_no_value_can_be_is_refused_where_it_is_read() {
        const KEYED_BY_A_STRUCT: ModuleSchema = ModuleSchema {
            name: "reviews",
            capability: "reviews",
            operations: &[],
            types: &[TypeSchema {
                name: "Board",
                cases: &[],
                fields: &[FieldSchema {
                    name: "open",
                    ty: HostType::Set(&HostType::Named("reviews.PullRequest")),
                }],
            }],
            resources: &[],
        };
        let fault = KEYED_BY_A_STRUCT
            .validate()
            .expect_err("a set of a named type is not a set anything can be");
        assert_eq!(
            fault.to_string(),
            "`reviews.Board.open` is declared `Set<reviews.PullRequest>`, and `reviews.PullRequest` cannot be a `Map` key or a `Set` element"
        );

        const VALUED_BY_A_STRUCT: ModuleSchema = ModuleSchema {
            types: &[TypeSchema {
                name: "Board",
                cases: &[],
                fields: &[FieldSchema {
                    name: "open",
                    ty: HostType::Map(&HostType::String, &HostType::Named("reviews.PullRequest")),
                }],
            }],
            ..KEYED_BY_A_STRUCT
        };
        assert!(VALUED_BY_A_STRUCT.validate().is_ok());
    }

    /// An operation's own signature is read the same way a declared type's
    /// fields are, and the place a fault names is the one a reader has to go
    /// and edit.
    #[test]
    fn an_operation_s_signature_is_read_for_the_same_fault() {
        const TAKES_ONE: OperationSchema = OperationSchema {
            name: "post",
            params: &[HostType::Set(&HostType::Any)],
            variadic: false,
            result: HostType::Unit,
            capability: "reviews",
            effect: Effect::Read,
            cancellable: false,
            recordable: true,
            result_is_task_safe: true,
        };
        const MODULE: ModuleSchema = ModuleSchema {
            name: "reviews",
            capability: "reviews",
            operations: &[TAKES_ONE],
            types: &[],
            resources: &[],
        };
        assert_eq!(
            MODULE
                .validate()
                .expect_err("`Any` promises nothing about a key")
                .place,
            "argument 1 of `reviews.post`"
        );

        const ANSWERS_ONE: ModuleSchema = ModuleSchema {
            operations: &[OperationSchema {
                params: &[],
                result: HostType::Result(
                    &HostType::Map(&HostType::Named("reviews.PullRequest"), &HostType::Int),
                    &HostType::Error,
                ),
                ..TAKES_ONE
            }],
            ..MODULE
        };
        assert_eq!(
            ANSWERS_ONE
                .validate()
                .expect_err("a map keyed by a named type is not one either")
                .place,
            "the result of `reviews.post`"
        );
    }

    #[test]
    fn a_fixed_operation_accepts_exactly_its_parameters() {
        assert!(!READ_A_STRING.accepts(0));
        assert!(READ_A_STRING.accepts(1));
        assert!(!READ_A_STRING.accepts(2));
        assert_eq!(READ_A_STRING.min_arity(), 1);
        assert_eq!(READ_A_STRING.expected_arity(), "1 argument");
    }

    #[test]
    fn a_variadic_operation_accepts_zero_or_more() {
        assert!(PRINT_MANY.accepts(0));
        assert!(PRINT_MANY.accepts(1));
        assert!(PRINT_MANY.accepts(7));
        assert_eq!(PRINT_MANY.min_arity(), 0);
        assert_eq!(PRINT_MANY.expected_arity(), "at least 0 arguments");
    }

    /// The declared type of an argument, which is what both ends check one
    /// against. A variadic parameter answers for every argument from its own
    /// position onwards; a fixed one answers for exactly its own.
    #[test]
    fn a_parameter_answers_for_the_argument_at_its_position() {
        assert_eq!(READ_A_STRING.param(0), Some(&HostType::String));
        assert_eq!(READ_A_STRING.param(1), None);

        assert_eq!(PRINT_MANY.param(0), Some(&HostType::String));
        assert_eq!(PRINT_MANY.param(6), Some(&HostType::String));
    }

    #[test]
    fn signatures_read_like_source() {
        assert_eq!(
            READ_A_STRING.signature(),
            "read(String) -> Result<String, Error>"
        );
        assert_eq!(
            PRINT_MANY.signature(),
            "println(String...) -> Result<Unit, Error>"
        );
    }
}
