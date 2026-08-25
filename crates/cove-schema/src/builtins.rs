//! What the builtin types declare about themselves.
//!
//! `Array<T>.get`, `Vector.of`, `Shared<T>.lock` and their neighbours are the
//! language's own methods and associated functions rather than a host's, but
//! they have the Host API schema's problem exactly: the compiler needs their
//! signatures to check a call, the runtime needs their names to dispatch one,
//! and the two crates cannot see each other. ADR 0004 wrote the table out
//! twice and said so, "until a crate both can depend on exists". This is that
//! crate, so this is that table, and there is now one of it.
//!
//! # Why this is not [`HostType`](crate::HostType)
//!
//! A Host API operation's signature is deliberately monomorphic:
//! `documents.read(String) -> Result<String, Error>` names concrete types
//! because a host is a boundary, and a boundary that took a type parameter
//! would have nothing to instantiate it with. A builtin is the opposite.
//! `Array<T>.get` answers in the element type of the receiver it was called
//! on, `snapshot` answers in the receiver's own type, `Shared<T>.lock` takes
//! a function and answers in whatever that function produces, and
//! `Vector.of(items: T...)` binds a parameter of its own. None of that fits
//! `HostType`, and widening `HostType` to hold it would put generics into
//! every host signature that has no use for them.
//!
//! So there are two vocabularies here, on purpose:
//! [`HostType`](crate::HostType) for what crosses the Host API boundary, and
//! [`BuiltinType`] for what the language defines about itself. They overlap
//! in the scalars and diverge exactly where the two kinds of signature
//! differ — [`BuiltinType`] has [`BuiltinType::Param`],
//! [`BuiltinType::SelfType`], and [`BuiltinType::Fn`], and `HostType` has
//! [`Any`](crate::HostType::Any), which is a boundary's way of saying it does
//! not look inside a value and means nothing for a method the language itself
//! defines.
//!
//! # Two tables, because a builtin is not always called on something
//!
//! [`BUILTINS`] is keyed by a receiver, and most builtins have one:
//! `items.length()` and `Vector.of(1)` are both reached through a type.
//! `Ok(1)`, `Error("boom")`, and `assert(true)` are not — they are written
//! bare, the way a declared function is — so [`FREE_BUILTINS`] is the second
//! table, holding the five constructors and the two assertions with a name,
//! a kind, and a signature each. They were the last builtins written out in
//! both `cove-sema` and `cove-runtime`; [issue #50](https://github.com/myuon/cove/issues/50)
//! is why they are here.
//!
//! # What is here and what is not
//!
//! The signatures are here; the implementations are not, and cannot be. A
//! builtin's body is Rust that reaches into a `Value`, so it lives in
//! `cove_runtime::builtins` beside the value model it walks. What this table
//! removes is the *second description* of those bodies: `cove-sema` reads
//! every signature from here rather than restating it, and the runtime reads
//! from here every question it can answer from a name alone — which type
//! names are namespaces, which methods take a `var self` receiver, which
//! names are constructors, which are assertions, how many arguments each
//! takes, and which receivers are told that `count()` is spelled `length()`.
//! `crates/cove-runtime/tests/builtin_schema.rs` closes the loop by driving
//! every entry in both tables through a real interpreter, so an entry added
//! here with no implementation behind it fails a test rather than a program.
//!
//! The variants of [`BuiltinType`] cover exactly the types the tables below
//! use, on the same rule the host vocabulary follows: add one when a builtin
//! needs it, because an unused variant is a type nobody can produce.

use std::fmt;

/// A type in a builtin's signature, written in Cove's source vocabulary.
///
/// Like [`HostType`](crate::HostType) this is a small enum rather than
/// `cove_syntax::ast::Type`, because a builtin has no source to point a span
/// at, and like `HostType` its [`fmt::Display`] produces the form the type
/// would be written in Cove.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinType {
    /// `Unit`, what a method that produces nothing answers.
    Unit,
    /// `Bool`.
    Bool,
    /// `Int`, a signed 64-bit integer.
    Int,
    /// `String`.
    String,
    /// `Error`, the builtin error struct.
    Error,
    /// `Array<T>`, the fixed-length immutable sequence.
    Array(&'static BuiltinType),
    /// `Vector<T>`, the growable one.
    Vector(&'static BuiltinType),
    /// `Set<T>`.
    Set(&'static BuiltinType),
    /// `Map<K, V>`.
    Map(&'static BuiltinType, &'static BuiltinType),
    /// `MapEntry<K, V>`, the one `key`/`value` pair `Map.of` collects.
    MapEntry(&'static BuiltinType, &'static BuiltinType),
    /// `Option<T>`.
    Option(&'static BuiltinType),
    /// `Result<T, E>`.
    Result(&'static BuiltinType, &'static BuiltinType),
    /// `Task<T>`, the handle `scope.spawn { ... }` hands back.
    Task(&'static BuiltinType),
    /// `Shared<T>`, the synchronized value `Shared(...)` wraps one in.
    Shared(&'static BuiltinType),
    /// `fn(A, B) -> R`: what a builtin that takes a callback declares, such
    /// as `Shared<T>.lock` or `Scope.spawn`.
    Fn(&'static [BuiltinType], &'static BuiltinType),
    /// A type parameter, by name.
    ///
    /// It is bound either by the receiver — the `T` of the `Array<T>` a
    /// method was called on — or by the signature itself, as
    /// `Vector.of(items: T...)` binds one. Which of the two a name is comes
    /// from where it is declared: [`BuiltinSchema::parameters`] for the
    /// receiver's, [`MethodSchema::generics`] for the signature's.
    Param(&'static str),
    /// The receiver's own type, written `Self`.
    ///
    /// This is what `snapshot` answers, and it is one of the reasons a
    /// builtin's signature cannot be written in the host vocabulary: an
    /// immutable builtin snapshots to itself, so its result is not a type at
    /// all until there is a receiver to read it off.
    SelfType,
}

impl fmt::Display for BuiltinType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuiltinType::Unit => f.write_str("Unit"),
            BuiltinType::Bool => f.write_str("Bool"),
            BuiltinType::Int => f.write_str("Int"),
            BuiltinType::String => f.write_str("String"),
            BuiltinType::Error => f.write_str("Error"),
            BuiltinType::Array(item) => write!(f, "Array<{item}>"),
            BuiltinType::Vector(item) => write!(f, "Vector<{item}>"),
            BuiltinType::Set(item) => write!(f, "Set<{item}>"),
            BuiltinType::Map(key, value) => write!(f, "Map<{key}, {value}>"),
            BuiltinType::MapEntry(key, value) => write!(f, "MapEntry<{key}, {value}>"),
            BuiltinType::Option(some) => write!(f, "Option<{some}>"),
            BuiltinType::Result(ok, error) => write!(f, "Result<{ok}, {error}>"),
            BuiltinType::Task(inner) => write!(f, "Task<{inner}>"),
            BuiltinType::Shared(inner) => write!(f, "Shared<{inner}>"),
            BuiltinType::Fn(params, ret) => {
                let params: Vec<String> = params.iter().map(BuiltinType::to_string).collect();
                write!(f, "fn({}) -> {ret}", params.join(", "))
            }
            BuiltinType::Param(name) => f.write_str(name),
            BuiltinType::SelfType => f.write_str("Self"),
        }
    }
}

/// One parameter of a builtin's signature.
///
/// A host operation's parameters are positions and nothing else; a builtin's
/// are labels a caller may write and a diagnostic does write, which is why
/// this carries a name where
/// [`OperationSchema::params`](crate::OperationSchema::params) carries a bare
/// type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParamSchema {
    /// The label, such as `index` or `fallback`.
    pub name: &'static str,
    /// The parameter's type.
    pub ty: BuiltinType,
}

/// One builtin method or associated function.
///
/// The two are the same shape and differ only in whether there is a receiver,
/// which is what the field they are declared in says: a
/// [`BuiltinSchema::methods`] entry is called on a value and a
/// [`BuiltinSchema::associated`] entry is called on the type. An associated
/// function therefore never names [`BuiltinType::SelfType`], never names the
/// receiver's type parameters, and is never `mutating`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MethodSchema {
    /// The name Cove source calls, such as `isEmpty`.
    pub name: &'static str,
    /// The type parameters this signature binds of its own, unified at the
    /// call site exactly as a declared function's are.
    pub generics: &'static [&'static str],
    /// Parameters in declaration order.
    pub params: &'static [ParamSchema],
    /// Whether the last parameter takes the rest of the arguments, as
    /// `Vector.of(items: T...)` does.
    pub variadic: bool,
    /// The type the call produces.
    pub result: BuiltinType,
    /// Whether the receiver is `var self`, so the call needs the caller's own
    /// mutable place rather than a value.
    pub mutating: bool,
}

impl MethodSchema {
    /// The signature, in the form it would be written in Cove source:
    /// `inserted(key: K, value: V) -> Map<K, V>`.
    pub fn signature(&self) -> String {
        let mut params: Vec<String> = self
            .params
            .iter()
            .map(|param| format!("{}: {}", param.name, param.ty))
            .collect();
        if self.variadic {
            if let Some(last) = params.last_mut() {
                last.push_str("...");
            }
        }
        format!("{}({}) -> {}", self.name, params.join(", "), self.result)
    }
}

/// One builtin type: its name, its type parameters, and what may be called on
/// a value of it or on the name of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinSchema {
    /// The name Cove source writes, such as `Array`.
    pub name: &'static str,
    /// The type parameters the receiver binds, in the order they are written:
    /// `["K", "V"]` for `Map<K, V>`.
    ///
    /// A method's signature names these, and a call site reads them off the
    /// receiver the method was called on.
    pub parameters: &'static [&'static str],
    /// Whether the name may be written as a namespace, as in `Vector.of(...)`
    /// or `Int.parse(...)`.
    ///
    /// This is not the same question as whether the type has associated
    /// functions. `Array.something()` is a call on the builtin type `Array`
    /// whatever `something` turns out to be, and answering "`Array` has no
    /// associated function `something`" is better than treating `Array` as an
    /// undeclared name. The types that say `false` are the ones no program
    /// writes the name of: a `Task` comes from `scope.spawn`, a `Shared` from
    /// the `Shared(...)` constructor, a `Scope` from `scope name { ... }`,
    /// and a `Range`, a `Duration`, or a `Unit` from an expression that makes
    /// one.
    pub namespace: bool,
    /// What may be called on a value of this type.
    ///
    /// The order is the order a diagnostic lists them in when it has to say
    /// what does exist.
    pub methods: &'static [MethodSchema],
    /// What may be called on the type itself.
    pub associated: &'static [MethodSchema],
}

impl BuiltinSchema {
    /// The method `name`, if this type has one.
    pub fn method(&self, name: &str) -> Option<&'static MethodSchema> {
        self.methods.iter().find(|entry| entry.name == name)
    }

    /// The associated function `name`, if this type has one.
    pub fn associated_function(&self, name: &str) -> Option<&'static MethodSchema> {
        self.associated.iter().find(|entry| entry.name == name)
    }
}

/// What a builtin that is called on nothing *is*.
///
/// The two kinds are not variations of one thing — a constructor makes a
/// value and an assertion checks one — and both ends of the toolchain ask
/// which is which before anything else: the interpreter dispatches an
/// assertion through the one path that carries the source text of its
/// arguments, and the checker gives an assertion's arity a different sentence
/// than a constructor's. So the kind is in the table rather than derived from
/// the name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreeBuiltinKind {
    /// `Ok(value)`, `Err(error)`, `Some(value)`, `Error("message")`, and
    /// `Shared(value)`: a name that builds a builtin value out of one
    /// payload.
    Constructor,
    /// `assert(condition)` and `assertEqual(actual, expected)`: a name a test
    /// calls, which reports failure as an ordinary `Err`.
    Assertion,
}

/// One builtin that is called on nothing.
///
/// A [`BuiltinSchema`] is keyed by a receiver, and these have none: `Ok(1)`
/// and `assert(true)` are written bare, like a declared function and unlike
/// `items.length()` or `Vector.of(1)`. Straining the receiver-keyed table to
/// hold them would have meant inventing a receiver they do not have, so they
/// have a table of their own, and it is close to the plainest thing that lets
/// both ends stop restating each other: a name, which kind it is, and the
/// parameters it takes.
///
/// The result is here too, because the checker reads it in both directions.
/// A constructor's result is generic — `Ok(value: T) -> Result<T, E>` — and
/// the type a call site expects is what settles `T` and `E`, so the one
/// declaration that says what `Ok` produces is also the one that says what
/// its payload must be. That is why this carries a signature rather than only
/// an arity: the arity is what the runtime needs, and the signature is what
/// stops the checker from writing the same five names out again to say what
/// each of them makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreeBuiltinSchema {
    /// The name Cove source calls, such as `Ok`.
    pub name: &'static str,
    /// Whether this builds a value or checks one.
    pub kind: FreeBuiltinKind,
    /// The type parameters this signature binds.
    ///
    /// Every type parameter a free builtin names is one it binds itself:
    /// there is no receiver to read one off. A call site settles them from
    /// the type it expects, and from the arguments where it expects nothing
    /// in particular.
    pub generics: &'static [&'static str],
    /// Parameters in declaration order, labelled as a diagnostic names them.
    pub params: &'static [ParamSchema],
    /// The type the call produces.
    pub result: BuiltinType,
}

impl FreeBuiltinSchema {
    /// How many arguments a call must supply.
    pub fn arity(&self) -> usize {
        self.params.len()
    }

    /// The signature, in the form it would be written in Cove source:
    /// `assertEqual(actual: T, expected: T) -> Result<Unit, Error>`.
    pub fn signature(&self) -> String {
        let params: Vec<String> = self
            .params
            .iter()
            .map(|param| format!("{}: {}", param.name, param.ty))
            .collect();
        format!("{}({}) -> {}", self.name, params.join(", "), self.result)
    }
}

/// Every builtin type the language defines.
///
/// The order is the order the associated functions read out in a diagnostic
/// that has to list them, which is why the collections come first.
pub static BUILTINS: &[BuiltinSchema] = &[
    ARRAY, VECTOR, MAP, SET, STRING, RANGE, OPTION, RESULT, INT, FLOAT, BOOL, UNIT, DURATION,
    ERROR, TASK, SHARED, SCOPE,
];

/// Every builtin type the language defines.
pub fn builtins() -> &'static [BuiltinSchema] {
    BUILTINS
}

/// The builtin type `name` describes itself with, if there is one.
pub fn builtin(name: &str) -> Option<&'static BuiltinSchema> {
    BUILTINS.iter().find(|entry| entry.name == name)
}

/// Whether `name` is a builtin type a program may write as a namespace, as in
/// `Vector.of(...)`.
pub fn is_builtin_type(name: &str) -> bool {
    BUILTINS
        .iter()
        .any(|entry| entry.namespace && entry.name == name)
}

/// Whether `name` is a builtin method that takes a `var self` receiver, and
/// so needs a mutable place at the call site rather than a value.
///
/// The question is asked by name alone, because that is what the call site
/// has before it has evaluated a receiver. `push` and `freeze` are the two,
/// and no builtin type spells a mutating method the way another spells an
/// immutable one.
pub fn is_mutating_method(name: &str) -> bool {
    BUILTINS.iter().any(|entry| {
        entry
            .methods
            .iter()
            .any(|method| method.mutating && method.name == name)
    })
}

/// Whether the builtin type `name` reports how many elements it holds.
///
/// This is the audience for the one diagnostic that teaches a spelling: a
/// receiver that answers `length()` is a receiver a program might have
/// written `count()` on, so `Array`, `Vector`, `String`, `Range`, `Map`, and
/// `Set` are told what the spelling is and everything else is told it has no
/// such method. Deriving the set from the table is the point — it used to be
/// written out at both ends, and the two had drifted by two types.
pub fn declares_length(name: &str) -> bool {
    builtin(name).is_some_and(|entry| entry.method("length").is_some())
}

/// Every builtin that is called on nothing: the constructors, then the
/// assertions.
///
/// A name belongs to one kind or the other and never to both, so the order
/// settles nothing at a call site. It is the order a reader meets them in:
/// `Ok` and its neighbours are in every program, and `assert` is in every
/// test.
pub static FREE_BUILTINS: &[FreeBuiltinSchema] =
    &[OK, ERR, SOME, ERROR_OF, SHARED_OF, ASSERT, ASSERT_EQUAL];

/// Every builtin that is called on nothing.
pub fn free_builtins() -> &'static [FreeBuiltinSchema] {
    FREE_BUILTINS
}

/// The free builtin `name` describes itself with, if there is one.
pub fn free_builtin(name: &str) -> Option<&'static FreeBuiltinSchema> {
    FREE_BUILTINS.iter().find(|entry| entry.name == name)
}

// ------------------------------------------- the builtins called on nothing

/// `Ok(value: T) -> Result<T, E>`.
///
/// The error type is the one thing a payload cannot say, so `E` is settled by
/// the type the call site expects and is unknown when it expects nothing.
pub const OK: FreeBuiltinSchema = FreeBuiltinSchema {
    name: "Ok",
    kind: FreeBuiltinKind::Constructor,
    generics: &["T", "E"],
    params: &[ParamSchema {
        name: "value",
        ty: BuiltinType::Param("T"),
    }],
    result: BuiltinType::Result(&BuiltinType::Param("T"), &BuiltinType::Param("E")),
};

/// `Err(error: E) -> Result<T, E>`, the mirror of [`OK`].
pub const ERR: FreeBuiltinSchema = FreeBuiltinSchema {
    name: "Err",
    kind: FreeBuiltinKind::Constructor,
    generics: &["T", "E"],
    params: &[ParamSchema {
        name: "error",
        ty: BuiltinType::Param("E"),
    }],
    result: BuiltinType::Result(&BuiltinType::Param("T"), &BuiltinType::Param("E")),
};

/// `Some(value: T) -> Option<T>`.
///
/// `None` has no entry here because it is not a call: it is the empty case
/// written as a bare name, and writing `None(...)` is a mistake both ends
/// name as one.
pub const SOME: FreeBuiltinSchema = FreeBuiltinSchema {
    name: "Some",
    kind: FreeBuiltinKind::Constructor,
    generics: &["T"],
    params: &[ParamSchema {
        name: "value",
        ty: BuiltinType::Param("T"),
    }],
    result: BuiltinType::Option(&BuiltinType::Param("T")),
};

/// `Error(message: String) -> Error`, the one constructor whose payload has a
/// type of its own rather than one the call site settles.
///
/// The constant is not called `ERROR` because [`BuiltinType::Error`]'s type
/// table already is.
pub const ERROR_OF: FreeBuiltinSchema = FreeBuiltinSchema {
    name: "Error",
    kind: FreeBuiltinKind::Constructor,
    generics: &[],
    params: &[ParamSchema {
        name: "message",
        ty: BuiltinType::String,
    }],
    result: BuiltinType::Error,
};

/// `Shared(value: T) -> Shared<T>`.
///
/// This is the one constructor that can refuse its payload: what a `Shared`
/// wraps is reachable from every task it is given to, so it must be able to
/// cross a task boundary. That rule is not in this table — it is about what a
/// type *is*, not what a call takes — so the checker and the runtime each
/// enforce it with what they have, a type and a value.
pub const SHARED_OF: FreeBuiltinSchema = FreeBuiltinSchema {
    name: "Shared",
    kind: FreeBuiltinKind::Constructor,
    generics: &["T"],
    params: &[ParamSchema {
        name: "value",
        ty: BuiltinType::Param("T"),
    }],
    result: BuiltinType::Shared(&BuiltinType::Param("T")),
};

/// `assert(condition: Bool) -> Result<Unit, Error>`.
///
/// A failing assertion is an expected failure rather than a broken invariant,
/// so it answers `Err` and `?` works on it inside a test.
pub const ASSERT: FreeBuiltinSchema = FreeBuiltinSchema {
    name: "assert",
    kind: FreeBuiltinKind::Assertion,
    generics: &[],
    params: &[ParamSchema {
        name: "condition",
        ty: BuiltinType::Bool,
    }],
    result: BuiltinType::Result(&BuiltinType::Unit, &BuiltinType::Error),
};

/// `assertEqual(actual: T, expected: T) -> Result<Unit, Error>`.
///
/// One type parameter named twice is the whole rule: `assertEqual` compares
/// two values of one type, and comparing values of two types is the mistake
/// it catches. Both ends read that off the repeated `T` — the checker as
/// unification, the runtime as two values whose type names must agree.
pub const ASSERT_EQUAL: FreeBuiltinSchema = FreeBuiltinSchema {
    name: "assertEqual",
    kind: FreeBuiltinKind::Assertion,
    generics: &["T"],
    params: &[
        ParamSchema {
            name: "actual",
            ty: BuiltinType::Param("T"),
        },
        ParamSchema {
            name: "expected",
            ty: BuiltinType::Param("T"),
        },
    ],
    result: BuiltinType::Result(&BuiltinType::Unit, &BuiltinType::Error),
};

// ----------------------------------------------------- the shared signatures

/// `snapshot(self) -> Self`, the builtin `Snapshot` trait's one method.
///
/// Every builtin value type that has it is immutable and returns itself;
/// `Vector`, the one builtin with an independent mutable graph to copy,
/// returns a `Vector` again, and the runtime is what recursively snapshots
/// the elements. A closure, a task, a task scope, and a synchronized value
/// have no such graph this side of a lock and so do not have this method at
/// all. A struct or enum conforms the ordinary way, with an explicit `impl
/// Snapshot for Type`, so it is not a builtin.
const SNAPSHOT: MethodSchema = MethodSchema {
    name: "snapshot",
    generics: &[],
    params: &[],
    variadic: false,
    result: BuiltinType::SelfType,
    mutating: false,
};

/// `length() -> Int`, how every sequence reports its element count. There is
/// no `count()`.
const LENGTH: MethodSchema = MethodSchema {
    name: "length",
    generics: &[],
    params: &[],
    variadic: false,
    result: BuiltinType::Int,
    mutating: false,
};

/// `isEmpty() -> Bool`.
const IS_EMPTY: MethodSchema = MethodSchema {
    name: "isEmpty",
    generics: &[],
    params: &[],
    variadic: false,
    result: BuiltinType::Bool,
    mutating: false,
};

// ------------------------------------------------------------------- Array

/// `Array<T>`: the fixed-length immutable sequence.
///
/// `get` answers an `Option` rather than trapping, so an index outside the
/// array is a value the caller has to open rather than a stopped program.
pub const ARRAY: BuiltinSchema = BuiltinSchema {
    name: "Array",
    parameters: &["T"],
    namespace: true,
    methods: &[
        MethodSchema {
            name: "get",
            generics: &[],
            params: &[ParamSchema {
                name: "index",
                ty: BuiltinType::Int,
            }],
            variadic: false,
            result: BuiltinType::Option(&BuiltinType::Param("T")),
            mutating: false,
        },
        LENGTH,
        IS_EMPTY,
        SNAPSHOT,
    ],
    associated: &[],
};

// ------------------------------------------------------------------ Vector

/// `Vector<T>`: the growable sequence, and the one builtin with a mutable
/// graph of its own.
///
/// `push` and `freeze` are the language's only `var self` methods: one
/// appends, and the other consumes locally unique storage and hands back an
/// `Array` in O(1). `toArray` is the copying alternative, for a caller that
/// cannot give the storage up.
pub const VECTOR: BuiltinSchema = BuiltinSchema {
    name: "Vector",
    parameters: &["T"],
    namespace: true,
    methods: &[
        MethodSchema {
            name: "get",
            generics: &[],
            params: &[ParamSchema {
                name: "index",
                ty: BuiltinType::Int,
            }],
            variadic: false,
            result: BuiltinType::Option(&BuiltinType::Param("T")),
            mutating: false,
        },
        LENGTH,
        IS_EMPTY,
        MethodSchema {
            name: "push",
            generics: &[],
            params: &[ParamSchema {
                name: "value",
                ty: BuiltinType::Param("T"),
            }],
            variadic: false,
            result: BuiltinType::Unit,
            mutating: true,
        },
        MethodSchema {
            name: "freeze",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::Array(&BuiltinType::Param("T")),
            mutating: true,
        },
        MethodSchema {
            name: "toArray",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::Array(&BuiltinType::Param("T")),
            mutating: false,
        },
        SNAPSHOT,
    ],
    associated: &[MethodSchema {
        name: "of",
        generics: &["T"],
        params: &[ParamSchema {
            name: "items",
            ty: BuiltinType::Param("T"),
        }],
        variadic: true,
        result: BuiltinType::Vector(&BuiltinType::Param("T")),
        mutating: false,
    }],
};

// --------------------------------------------------------------------- Map

/// `Map<K, V>`: an immutable mapping, kept in ascending key order.
///
/// `inserted` and `removed` are past participles because the map they answer
/// with is a new one; nothing here writes through the receiver, unlike
/// `Vector`'s `push`. Which values may be keys is a runtime rule — a key's
/// equality must not be able to change — and stating it here would need
/// bounds, which the MVP does not have.
pub const MAP: BuiltinSchema = BuiltinSchema {
    name: "Map",
    parameters: &["K", "V"],
    namespace: true,
    methods: &[
        MethodSchema {
            name: "get",
            generics: &[],
            params: &[ParamSchema {
                name: "key",
                ty: BuiltinType::Param("K"),
            }],
            variadic: false,
            result: BuiltinType::Option(&BuiltinType::Param("V")),
            mutating: false,
        },
        LENGTH,
        IS_EMPTY,
        MethodSchema {
            name: "contains",
            generics: &[],
            params: &[ParamSchema {
                name: "key",
                ty: BuiltinType::Param("K"),
            }],
            variadic: false,
            result: BuiltinType::Bool,
            mutating: false,
        },
        MethodSchema {
            name: "keys",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::Array(&BuiltinType::Param("K")),
            mutating: false,
        },
        MethodSchema {
            name: "values",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::Array(&BuiltinType::Param("V")),
            mutating: false,
        },
        MethodSchema {
            name: "inserted",
            generics: &[],
            params: &[
                ParamSchema {
                    name: "key",
                    ty: BuiltinType::Param("K"),
                },
                ParamSchema {
                    name: "value",
                    ty: BuiltinType::Param("V"),
                },
            ],
            variadic: false,
            result: BuiltinType::Map(&BuiltinType::Param("K"), &BuiltinType::Param("V")),
            mutating: false,
        },
        MethodSchema {
            name: "removed",
            generics: &[],
            params: &[ParamSchema {
                name: "key",
                ty: BuiltinType::Param("K"),
            }],
            variadic: false,
            result: BuiltinType::Map(&BuiltinType::Param("K"), &BuiltinType::Param("V")),
            mutating: false,
        },
        SNAPSHOT,
    ],
    associated: &[MethodSchema {
        name: "of",
        generics: &["K", "V"],
        params: &[ParamSchema {
            name: "entries",
            ty: BuiltinType::MapEntry(&BuiltinType::Param("K"), &BuiltinType::Param("V")),
        }],
        variadic: true,
        result: BuiltinType::Map(&BuiltinType::Param("K"), &BuiltinType::Param("V")),
        mutating: false,
    }],
};

// --------------------------------------------------------------------- Set

/// `Set<T>`: an immutable set, kept in ascending element order.
pub const SET: BuiltinSchema = BuiltinSchema {
    name: "Set",
    parameters: &["T"],
    namespace: true,
    methods: &[
        LENGTH,
        IS_EMPTY,
        MethodSchema {
            name: "toArray",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::Array(&BuiltinType::Param("T")),
            mutating: false,
        },
        MethodSchema {
            name: "contains",
            generics: &[],
            params: &[ParamSchema {
                name: "element",
                ty: BuiltinType::Param("T"),
            }],
            variadic: false,
            result: BuiltinType::Bool,
            mutating: false,
        },
        MethodSchema {
            name: "inserted",
            generics: &[],
            params: &[ParamSchema {
                name: "element",
                ty: BuiltinType::Param("T"),
            }],
            variadic: false,
            result: BuiltinType::Set(&BuiltinType::Param("T")),
            mutating: false,
        },
        MethodSchema {
            name: "removed",
            generics: &[],
            params: &[ParamSchema {
                name: "element",
                ty: BuiltinType::Param("T"),
            }],
            variadic: false,
            result: BuiltinType::Set(&BuiltinType::Param("T")),
            mutating: false,
        },
        SNAPSHOT,
    ],
    associated: &[MethodSchema {
        name: "of",
        generics: &["T"],
        params: &[ParamSchema {
            name: "items",
            ty: BuiltinType::Param("T"),
        }],
        variadic: true,
        result: BuiltinType::Set(&BuiltinType::Param("T")),
        mutating: false,
    }],
};

// ------------------------------------------------------------------ String

/// `String`: an immutable sequence of characters, whose `length` counts
/// characters rather than bytes.
pub const STRING: BuiltinSchema = BuiltinSchema {
    name: "String",
    parameters: &[],
    namespace: true,
    methods: &[
        LENGTH,
        IS_EMPTY,
        MethodSchema {
            name: "words",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::Array(&BuiltinType::String),
            mutating: false,
        },
        SNAPSHOT,
    ],
    associated: &[],
};

// ------------------------------------------------------------------- Range

/// `Range`: what `0..n` and `0..=n` produce.
pub const RANGE: BuiltinSchema = BuiltinSchema {
    name: "Range",
    parameters: &[],
    namespace: false,
    methods: &[
        LENGTH,
        IS_EMPTY,
        MethodSchema {
            name: "contains",
            generics: &[],
            params: &[ParamSchema {
                name: "value",
                ty: BuiltinType::Int,
            }],
            variadic: false,
            result: BuiltinType::Bool,
            mutating: false,
        },
        SNAPSHOT,
    ],
    associated: &[],
};

// ------------------------------------------------------------------ Option

/// `Option<T>`: `Some(value)` or `None`.
///
/// It has no `snapshot`: whether a copy of an `Option` is independent is
/// decided by what it wraps, and the MVP has no bound to say that with.
pub const OPTION: BuiltinSchema = BuiltinSchema {
    name: "Option",
    parameters: &["T"],
    namespace: true,
    methods: &[
        MethodSchema {
            name: "isSome",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::Bool,
            mutating: false,
        },
        MethodSchema {
            name: "isNone",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::Bool,
            mutating: false,
        },
        MethodSchema {
            name: "unwrapOr",
            generics: &[],
            params: &[ParamSchema {
                name: "fallback",
                ty: BuiltinType::Param("T"),
            }],
            variadic: false,
            result: BuiltinType::Param("T"),
            mutating: false,
        },
    ],
    associated: &[],
};

// ------------------------------------------------------------------ Result

/// `Result<T, E>`: `Ok(value)` or `Err(error)`.
pub const RESULT: BuiltinSchema = BuiltinSchema {
    name: "Result",
    parameters: &["T", "E"],
    namespace: true,
    methods: &[
        MethodSchema {
            name: "isOk",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::Bool,
            mutating: false,
        },
        MethodSchema {
            name: "isError",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::Bool,
            mutating: false,
        },
        // The one builtin whose callback has two accepted shapes. The
        // Language Card writes `mapError { ... }` with a trailing closure
        // that may ignore the error it replaces, so a closure of no
        // parameters is accepted where this declares one. Both ends know
        // that — `cove_sema`'s `Checker::map_error` and the arity the runtime
        // asks the callback for — and the shape declared here is the one that
        // carries the error, because the other is this one with a parameter
        // dropped.
        MethodSchema {
            name: "mapError",
            generics: &["F"],
            params: &[ParamSchema {
                name: "body",
                ty: BuiltinType::Fn(&[BuiltinType::Param("E")], &BuiltinType::Param("F")),
            }],
            variadic: false,
            result: BuiltinType::Result(&BuiltinType::Param("T"), &BuiltinType::Param("F")),
            mutating: false,
        },
    ],
    associated: &[],
};

// --------------------------------------------------------------------- Int

/// `Int`: a signed 64-bit integer.
///
/// Parsing fails on text that is not one, which is an expected failure and so
/// a `Result` rather than a trap.
pub const INT: BuiltinSchema = BuiltinSchema {
    name: "Int",
    parameters: &[],
    namespace: true,
    methods: &[SNAPSHOT],
    associated: &[MethodSchema {
        name: "parse",
        generics: &[],
        params: &[ParamSchema {
            name: "text",
            ty: BuiltinType::String,
        }],
        variadic: false,
        result: BuiltinType::Result(&BuiltinType::Int, &BuiltinType::Error),
        mutating: false,
    }],
};

// ------------------------------------------------------------------- Float

/// `Float`: a 64-bit binary floating-point number.
pub const FLOAT: BuiltinSchema = BuiltinSchema {
    name: "Float",
    parameters: &[],
    namespace: true,
    methods: &[SNAPSHOT],
    associated: &[],
};

// -------------------------------------------------------------------- Bool

/// `Bool`.
pub const BOOL: BuiltinSchema = BuiltinSchema {
    name: "Bool",
    parameters: &[],
    namespace: true,
    methods: &[SNAPSHOT],
    associated: &[],
};

// -------------------------------------------------------------------- Unit

/// `Unit`, written `()`: what an expression that produces nothing produces.
pub const UNIT: BuiltinSchema = BuiltinSchema {
    name: "Unit",
    parameters: &[],
    namespace: false,
    methods: &[SNAPSHOT],
    associated: &[],
};

// ---------------------------------------------------------------- Duration

/// `Duration`: a signed count of nanoseconds.
pub const DURATION: BuiltinSchema = BuiltinSchema {
    name: "Duration",
    parameters: &[],
    namespace: false,
    methods: &[SNAPSHOT],
    associated: &[],
};

// ------------------------------------------------------------------- Error

/// `Error`, the builtin error struct.
///
/// It is a namespace because a program writes the name — `Error("message")`
/// builds one — so a mistyped `Error.something()` should be told what `Error`
/// is rather than that the name is undeclared. There is nothing to call on
/// it: the message is read as a field.
pub const ERROR: BuiltinSchema = BuiltinSchema {
    name: "Error",
    parameters: &[],
    namespace: true,
    methods: &[],
    associated: &[],
};

// -------------------------------------------------------------------- Task

/// `Task<T>`: the handle `scope.spawn { ... }` hands back.
///
/// `cancel` only asks. A cancelled task stops at its next safepoint, and
/// whether it stopped or had already finished is known only once something
/// waits for it.
pub const TASK: BuiltinSchema = BuiltinSchema {
    name: "Task",
    parameters: &["T"],
    namespace: false,
    methods: &[
        MethodSchema {
            name: "await",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::Param("T"),
            mutating: false,
        },
        MethodSchema {
            name: "cancel",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::Unit,
            mutating: false,
        },
    ],
    associated: &[],
};

// ------------------------------------------------------------------ Shared

/// `Shared<T>`: mutable state more than one task may reach.
///
/// `lock` is its only operation, and there is no `get` and no `set` by
/// design: every access is scoped, so a read-modify-write is one expression
/// and cannot be split into two that race. The closure receives the wrapped
/// value and `lock` produces whatever the closure does.
pub const SHARED: BuiltinSchema = BuiltinSchema {
    name: "Shared",
    parameters: &["T"],
    namespace: false,
    methods: &[MethodSchema {
        name: "lock",
        generics: &["R"],
        params: &[ParamSchema {
            name: "body",
            ty: BuiltinType::Fn(&[BuiltinType::Param("T")], &BuiltinType::Param("R")),
        }],
        variadic: false,
        result: BuiltinType::Param("R"),
        mutating: false,
    }],
    associated: &[],
};

// ------------------------------------------------------------------- Scope

/// `Scope`: the value `scope name { ... }` binds.
///
/// `spawn` takes its body as a trailing closure and hands back a handle to
/// the value that body produces.
pub const SCOPE: BuiltinSchema = BuiltinSchema {
    name: "Scope",
    parameters: &[],
    namespace: false,
    methods: &[MethodSchema {
        name: "spawn",
        generics: &["T"],
        params: &[ParamSchema {
            name: "body",
            ty: BuiltinType::Fn(&[], &BuiltinType::Param("T")),
        }],
        variadic: false,
        result: BuiltinType::Task(&BuiltinType::Param("T")),
        mutating: false,
    }],
    associated: &[],
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_render_in_cove_source_form() {
        assert_eq!(BuiltinType::Int.to_string(), "Int");
        assert_eq!(
            BuiltinType::Option(&BuiltinType::Param("T")).to_string(),
            "Option<T>"
        );
        assert_eq!(
            BuiltinType::Map(&BuiltinType::Param("K"), &BuiltinType::Param("V")).to_string(),
            "Map<K, V>"
        );
        assert_eq!(
            BuiltinType::MapEntry(&BuiltinType::Param("K"), &BuiltinType::Param("V")).to_string(),
            "MapEntry<K, V>"
        );
        assert_eq!(BuiltinType::SelfType.to_string(), "Self");
        assert_eq!(
            BuiltinType::Fn(&[BuiltinType::Param("T")], &BuiltinType::Param("R")).to_string(),
            "fn(T) -> R"
        );
    }

    #[test]
    fn signatures_read_like_source() {
        assert_eq!(
            MAP.method("inserted").unwrap().signature(),
            "inserted(key: K, value: V) -> Map<K, V>"
        );
        assert_eq!(
            VECTOR.associated_function("of").unwrap().signature(),
            "of(items: T...) -> Vector<T>"
        );
        assert_eq!(
            SHARED.method("lock").unwrap().signature(),
            "lock(body: fn(T) -> R) -> R"
        );
        assert_eq!(
            ARRAY.method("snapshot").unwrap().signature(),
            "snapshot() -> Self"
        );
    }

    /// The names a program may write before a dot with no value in front of
    /// them. Both the compiler and the runtime ask this question, and both
    /// ask it here.
    #[test]
    fn the_namespaces_are_the_builtin_types_a_program_writes_the_name_of() {
        let namespaces: Vec<&str> = BUILTINS
            .iter()
            .filter(|entry| entry.namespace)
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            namespaces,
            [
                "Array", "Vector", "Map", "Set", "String", "Option", "Result", "Int", "Float",
                "Bool", "Error"
            ]
        );
        assert!(is_builtin_type("Vector"));
        assert!(!is_builtin_type("Task"));
    }

    /// `push` and `freeze` are the language's only `var self` methods, and
    /// the call site asks by name because it has no receiver type yet.
    #[test]
    fn push_and_freeze_are_the_mutating_methods() {
        let mut mutating: Vec<&str> = BUILTINS
            .iter()
            .flat_map(|entry| entry.methods)
            .filter(|method| method.mutating)
            .map(|method| method.name)
            .collect();
        mutating.sort_unstable();
        assert_eq!(mutating, ["freeze", "push"]);
        assert!(is_mutating_method("push"));
        assert!(!is_mutating_method("toArray"));
    }

    /// A type parameter a signature names is either one the receiver binds or
    /// one the signature binds itself. A third kind would be a name nothing
    /// at the call site could instantiate.
    #[test]
    fn every_type_parameter_a_signature_names_is_bound() {
        fn named(ty: &BuiltinType, found: &mut Vec<&'static str>) {
            match ty {
                BuiltinType::Param(name) => found.push(name),
                BuiltinType::Array(inner)
                | BuiltinType::Vector(inner)
                | BuiltinType::Set(inner)
                | BuiltinType::Option(inner)
                | BuiltinType::Task(inner)
                | BuiltinType::Shared(inner) => named(inner, found),
                BuiltinType::Map(left, right)
                | BuiltinType::MapEntry(left, right)
                | BuiltinType::Result(left, right) => {
                    named(left, found);
                    named(right, found);
                }
                BuiltinType::Fn(inputs, ret) => {
                    for input in *inputs {
                        named(input, found);
                    }
                    named(ret, found);
                }
                _ => {}
            }
        }

        for entry in BUILTINS {
            let signatures = entry
                .methods
                .iter()
                .map(|method| (method, entry.parameters))
                .chain(entry.associated.iter().map(|method| (method, &[][..])));
            for (method, receiver) in signatures {
                let mut found = Vec::new();
                for param in method.params {
                    named(&param.ty, &mut found);
                }
                named(&method.result, &mut found);
                for name in found {
                    assert!(
                        receiver.contains(&name) || method.generics.contains(&name),
                        "`{}.{}` names `{name}`, which nothing binds",
                        entry.name,
                        method.name
                    );
                }
            }
        }

        // A free builtin has no receiver, so every name it uses is one it
        // binds itself.
        for entry in FREE_BUILTINS {
            let mut found = Vec::new();
            for param in entry.params {
                named(&param.ty, &mut found);
            }
            named(&entry.result, &mut found);
            for name in found {
                assert!(
                    entry.generics.contains(&name),
                    "`{}` names `{name}`, which nothing binds",
                    entry.name
                );
            }
        }
    }

    /// The two kinds of builtin that are called on nothing, in the order
    /// both ends ask about them.
    #[test]
    fn the_free_builtins_are_five_constructors_and_two_assertions() {
        let constructors: Vec<&str> = FREE_BUILTINS
            .iter()
            .filter(|entry| entry.kind == FreeBuiltinKind::Constructor)
            .map(|entry| entry.name)
            .collect();
        assert_eq!(constructors, ["Ok", "Err", "Some", "Error", "Shared"]);
        let assertions: Vec<&str> = FREE_BUILTINS
            .iter()
            .filter(|entry| entry.kind == FreeBuiltinKind::Assertion)
            .map(|entry| entry.name)
            .collect();
        assert_eq!(assertions, ["assert", "assertEqual"]);
        // `None` is the one name that reads like a constructor and is not:
        // it is the empty case written bare, and a call is a mistake both
        // ends name as one.
        assert!(free_builtin("None").is_none());
    }

    /// A constructor carries one value; an assertion takes what it compares.
    #[test]
    fn a_free_builtin_reads_like_source_and_knows_its_arity() {
        assert_eq!(
            free_builtin("Ok").unwrap().signature(),
            "Ok(value: T) -> Result<T, E>"
        );
        assert_eq!(
            free_builtin("Error").unwrap().signature(),
            "Error(message: String) -> Error"
        );
        assert_eq!(
            free_builtin("Shared").unwrap().signature(),
            "Shared(value: T) -> Shared<T>"
        );
        assert_eq!(
            free_builtin("assertEqual").unwrap().signature(),
            "assertEqual(actual: T, expected: T) -> Result<Unit, Error>"
        );
        for entry in FREE_BUILTINS {
            assert_eq!(entry.arity(), entry.params.len(), "`{}`", entry.name);
            if entry.kind == FreeBuiltinKind::Constructor {
                assert_eq!(entry.arity(), 1, "`{}` carries one value", entry.name);
            }
        }
    }

    /// The receivers a `count()` call is taught the spelling on are the ones
    /// that answer `length()`, which is what closed the drift between the
    /// checker's list and the runtime's.
    #[test]
    fn the_sequences_are_the_builtin_types_that_declare_length() {
        let sequences: Vec<&str> = BUILTINS
            .iter()
            .filter(|entry| declares_length(entry.name))
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            sequences,
            ["Array", "Vector", "Map", "Set", "String", "Range"]
        );
        assert!(!declares_length("Option"));
        assert!(!declares_length("Nothing"));
    }

    /// An associated function is called on the type, so there is no receiver
    /// for `Self` to mean and none to mutate.
    #[test]
    fn an_associated_function_has_no_receiver_to_name() {
        for entry in BUILTINS {
            for method in entry.associated {
                assert!(!method.mutating, "`{}.{}`", entry.name, method.name);
                assert_ne!(
                    method.result,
                    BuiltinType::SelfType,
                    "`{}.{}`",
                    entry.name,
                    method.name
                );
            }
        }
    }

    /// A variadic signature's last parameter is the one that repeats, so a
    /// signature with no parameters cannot be variadic.
    #[test]
    fn a_variadic_signature_has_a_parameter_to_repeat() {
        for entry in BUILTINS {
            for method in entry.methods.iter().chain(entry.associated) {
                assert!(
                    !method.variadic || !method.params.is_empty(),
                    "`{}.{}`",
                    entry.name,
                    method.name
                );
            }
        }
    }
}
