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
/// The variants cover exactly the types the shipped hosts use. Add one when a
/// host needs it; an unused variant is a type nobody can produce.
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
            HostType::Option(inner) => write!(f, "Option<{inner}>"),
            HostType::Result(ok, error) => write!(f, "Result<{ok}, {error}>"),
            HostType::Named(name) => f.write_str(name),
            HostType::Any => f.write_str("Any"),
        }
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
/// # A schema assembled at run time has to leak
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
/// there is nowhere to borrow from. Such a host leaks what it assembled:
///
/// ```
/// # use cove_schema::{ModuleSchema, OperationSchema};
/// # let configured_name = String::from("company");
/// # let operations: Vec<OperationSchema> = Vec::new();
/// let schema = ModuleSchema {
///     name: String::leak(configured_name),
///     capability: "company",
///     operations: Vec::leak(operations),
///     types: &[],
///     resources: &[],
/// };
/// ```
///
/// Every `&'static str` inside each operation is another one, so a
/// non-trivial module has several. Build the schema once — a `OnceLock`
/// beside the host, filled the first time it is asked — and hand the same
/// copy out afterwards: a leak per module for the life of a process is what
/// `'static` costs here, and a host that leaks one per call is leaking per
/// call.
///
/// The alternative is a lifetime parameter, which is not free: it reaches
/// every crate that names this type. `Cow` is not the alternative — `Box::new`
/// is not const-constructible, so the shipped tables could not stay `const`.
/// [Issue #86](https://github.com/myuon/cove/issues/86) carries that decision.
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
