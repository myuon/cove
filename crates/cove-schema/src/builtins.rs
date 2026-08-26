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
//! # What a builtin type is made of, and not only what it answers
//!
//! A [`BuiltinSchema`] began as a name and a list of methods, which was
//! enough for a call and not enough for anything else: `Option` is `Some` and
//! `None`, `Result` is `Ok` and `Err`, an `Error` carries a `message`, and a
//! `MapEntry` carries a `key` and a `value`, and none of that is a method. So
//! an entry also declares its [`cases`](BuiltinSchema::cases) if it is an
//! enum and its [`fields`](BuiltinSchema::fields) if it is a struct, and both
//! ends read them: `match` exhaustiveness, the type a pattern's binding gets,
//! the value the interpreter builds, and the field a program reads all come
//! from here. [issue #53](https://github.com/myuon/cove/issues/53) is why,
//! and it is the last of the four.
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
//! takes, which receivers are told that `count()` is spelled `length()`, and
//! what each builtin enum's cases and each builtin struct's fields are
//! called. `crates/cove-runtime/tests/builtin_schema.rs` closes the loop by
//! driving every entry in both tables through a real interpreter, so an entry
//! added here with no implementation behind it fails a test rather than a
//! program.
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

/// One case of a builtin enum.
///
/// A case is what a builtin enum is made of, the way a [`FieldSchema`] is
/// what a builtin struct is made of, and the payload is written in the
/// receiver's own type parameters: `Some` carries a `T` and `Err` carries an
/// `E`. So a pattern reads its binding's type off the scrutinee exactly as a
/// method reads its result off its receiver, and there is one description of
/// what `Ok` carries rather than one on each side of the toolchain.
///
/// This is [`TypeSchema::cases`](crate::TypeSchema::cases) in the builtin
/// vocabulary. A host's enum cases are bare names, because a boundary hands
/// over data it has already made; a builtin's carry a payload the language
/// itself binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaseSchema {
    /// The name Cove source writes in a pattern or a call, such as `Ok`.
    pub name: &'static str,
    /// What the case carries, in order.
    ///
    /// Empty for a case that carries nothing, which is `None` and only
    /// `None`: it is the one builtin case a program writes as a bare name
    /// rather than as a call.
    pub payload: &'static [BuiltinType],
}

impl CaseSchema {
    /// The case, in the form a declaration would write it: `Ok(T)`, or
    /// `None` for a case that carries nothing.
    pub fn signature(&self) -> String {
        if self.payload.is_empty() {
            return self.name.to_string();
        }
        let payload: Vec<String> = self.payload.iter().map(BuiltinType::to_string).collect();
        format!("{}({})", self.name, payload.join(", "))
    }

    /// The case as a pattern that binds nothing: `Ok(_)`, or `None` for a
    /// case that carries nothing.
    ///
    /// This is how a diagnostic points *inside* a value: a host that
    /// declares `Result<String, Error>` and hands back an `Ok(1)` is told
    /// the mismatch is inside `Ok(_)`.
    pub fn wildcard_pattern(&self) -> String {
        if self.payload.is_empty() {
            return self.name.to_string();
        }
        let payload: Vec<&str> = self.payload.iter().map(|_| "_").collect();
        format!("{}({})", self.name, payload.join(", "))
    }
}

/// One field of a builtin struct.
///
/// `Error` and `MapEntry` are structs the language builds rather than a
/// module declares, and a program reads them the ordinary way, by field. The
/// runtime has always built both — a `Value::Struct` with the fields below —
/// so what this adds is the half the checker was missing: what the runtime
/// builds, written down where the checker can read it.
///
/// This is [`FieldSchema`](crate::FieldSchema) in the builtin vocabulary,
/// carrying a [`BuiltinType`] because a builtin struct may be generic:
/// `MapEntry<K, V>`'s two fields are its two type parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldSchema {
    /// The label Cove source writes to read the field, such as `message`.
    pub name: &'static str,
    /// The field's type.
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
    /// The cases, for a builtin enum. Empty for everything else.
    ///
    /// `Option` and `Result` are the two, and this is the one list of what
    /// they are made of: `match` exhaustiveness, the sentence that names a
    /// missing case, the type a pattern's binding gets, and the value the
    /// interpreter builds all read it here.
    pub cases: &'static [CaseSchema],
    /// The fields, for a builtin struct. Empty for everything else.
    ///
    /// `Error` and `MapEntry` are the two. The order is the order an
    /// initializer takes them in and a diagnostic reads them out.
    pub fields: &'static [FieldSchema],
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

    /// The case `name`, if this type declares one.
    pub fn case(&self, name: &str) -> Option<&'static CaseSchema> {
        self.cases.iter().find(|entry| entry.name == name)
    }

    /// The field `name`, if this type declares one.
    pub fn field(&self, name: &str) -> Option<&'static FieldSchema> {
        self.fields.iter().find(|entry| entry.name == name)
    }

    /// Whether this is a builtin enum, which is what having cases means.
    pub fn is_enum(&self) -> bool {
        !self.cases.is_empty()
    }

    /// Whether this is a builtin struct, which is what having fields means.
    pub fn is_struct(&self) -> bool {
        !self.fields.is_empty()
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
    ARRAY, VECTOR, MAP, MAP_ENTRY, SET, STRING, RANGE, OPTION, RESULT, INT, FLOAT, BOOL, UNIT,
    DURATION, ERROR, TASK, SHARED, SCOPE,
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

/// The builtin enum that declares the case `name`, if one does.
///
/// This is what lets a bare `Some(value)` arm say which enum a `match` is
/// over without a list of its own: the two builtin enums are the two entries
/// with cases, and a case name belongs to at most one of them.
pub fn enum_declaring(name: &str) -> Option<&'static BuiltinSchema> {
    BUILTINS.iter().find(|entry| entry.case(name).is_some())
}

// -------------------------------------------- the cases and the one field
//
// The four case names and the two structs' field names are what
// [issue #53](https://github.com/myuon/cove/issues/53) was about: they were
// written out in `cove-sema` for exhaustiveness and pattern types, and again
// in `cove-runtime` for the values it builds. These are the constants both
// ends name.

/// `Some(T)`, the case an `Option` carries a value in.
pub const SOME_CASE: CaseSchema = CaseSchema {
    name: "Some",
    payload: &[BuiltinType::Param("T")],
};

/// `None`, the empty case of `Option`.
///
/// It is the one builtin case with no payload, and therefore the one written
/// as a bare name rather than as a call — which is why both ends ask for this
/// constant by itself: the checker to give the name a type, the interpreter
/// to build the value, and both to say that `None(...)` is a mistake.
pub const NONE_CASE: CaseSchema = CaseSchema {
    name: "None",
    payload: &[],
};

/// `Ok(T)`, the success case of a `Result`.
pub const OK_CASE: CaseSchema = CaseSchema {
    name: "Ok",
    payload: &[BuiltinType::Param("T")],
};

/// `Err(E)`, the failure case of a `Result`.
pub const ERR_CASE: CaseSchema = CaseSchema {
    name: "Err",
    payload: &[BuiltinType::Param("E")],
};

/// `message: String`, the one field of the builtin `Error` struct.
///
/// The runtime has always built an `Error` with this field and served a read
/// of it; the checker used to answer "`Error` has no field `message`" and
/// suggest a method `Error` does not have. Declaring it here is what closed
/// that gap.
pub const MESSAGE_FIELD: FieldSchema = FieldSchema {
    name: "message",
    ty: BuiltinType::String,
};

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
/// Its one parameter *is* [`MESSAGE_FIELD`], the field the value it builds
/// carries, so the label a call writes and the label a read writes cannot
/// come apart.
///
/// The constant is not called `ERROR` because [`BuiltinType::Error`]'s type
/// table already is.
pub const ERROR_OF: FreeBuiltinSchema = FreeBuiltinSchema {
    name: "Error",
    kind: FreeBuiltinKind::Constructor,
    generics: &[],
    params: &[ParamSchema {
        name: MESSAGE_FIELD.name,
        ty: MESSAGE_FIELD.ty,
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
    cases: &[],
    fields: &[],
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
    cases: &[],
    fields: &[],
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
    cases: &[],
    fields: &[],
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

// ---------------------------------------------------------------- MapEntry

/// `MapEntry<K, V>`: the one `key`/`value` pair a `Map` is built from and
/// iterated as.
///
/// It is the second builtin *struct*, and the only builtin whose initializer
/// is labelled: `MapEntry(key: "a", value: 1)` is the synthesized labelled
/// call a declared struct gets, and its labels are the two fields below, read
/// by both ends rather than written out at each. It is not a namespace,
/// because nothing is called on the name — `Map.of` collects the pairs and a
/// `for` over a `Map` binds them.
pub const MAP_ENTRY: BuiltinSchema = BuiltinSchema {
    name: "MapEntry",
    parameters: &["K", "V"],
    namespace: false,
    cases: &[],
    fields: &[
        FieldSchema {
            name: "key",
            ty: BuiltinType::Param("K"),
        },
        FieldSchema {
            name: "value",
            ty: BuiltinType::Param("V"),
        },
    ],
    methods: &[],
    associated: &[],
};

// --------------------------------------------------------------------- Set

/// `Set<T>`: an immutable set, kept in ascending element order.
pub const SET: BuiltinSchema = BuiltinSchema {
    name: "Set",
    parameters: &["T"],
    namespace: true,
    cases: &[],
    fields: &[],
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
/// characters rather than bytes — and every other index this type takes or
/// answers, in `chars`, `slice`, and `indexOf`, counts the same way, so an
/// API that mixed characters and bytes never has the chance to become a trap.
///
/// `join` lives here rather than on `Array<String>`, so that `", ".join(names)`
/// reads receiver-first with the separator: [`BuiltinType`] has no way to
/// constrain a receiver's type parameter, so an `Array<T>.join` would either
/// have to accept an `Array<Int>` too or need a bound the MVP cannot express,
/// and putting the method on `String` instead sidesteps the bound rather than
/// needing it.
///
/// `split` and `replace` both search for a piece of text that may not be
/// empty: an empty needle would match between every character rather than
/// answer either method's question, so both refuse it at run time and point
/// at `chars()`, which is the operation that actually means that.
pub const STRING: BuiltinSchema = BuiltinSchema {
    name: "String",
    parameters: &[],
    namespace: true,
    cases: &[],
    fields: &[],
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
        // One element per character, each a `String` of length 1 — the
        // decomposition `for` cannot do itself, since `for` refuses a
        // `String`.
        MethodSchema {
            name: "chars",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::Array(&BuiltinType::String),
            mutating: false,
        },
        // Every occurrence of `separator` separates, so adjacent separators
        // produce an empty part and text with none produces one part that is
        // the whole text; an empty `separator` is refused, as the type's own
        // doc comment says.
        MethodSchema {
            name: "split",
            generics: &[],
            params: &[ParamSchema {
                name: "separator",
                ty: BuiltinType::String,
            }],
            variadic: false,
            result: BuiltinType::Array(&BuiltinType::String),
            mutating: false,
        },
        // The receiver is the separator; see the type's own doc comment for
        // why this is not `Array<T>.join`.
        MethodSchema {
            name: "join",
            generics: &[],
            params: &[ParamSchema {
                name: "parts",
                ty: BuiltinType::Array(&BuiltinType::String),
            }],
            variadic: false,
            result: BuiltinType::String,
            mutating: false,
        },
        // The characters at indices `from` up to but not including `to`.
        // Both bounds are clamped into `0..length()`, and a `to` at or below
        // `from` answers `""`, so — the same choice `Array.get` makes by
        // answering an `Option`, in the form a substring can take — no
        // argument can stop a program.
        MethodSchema {
            name: "slice",
            generics: &[],
            params: &[
                ParamSchema {
                    name: "from",
                    ty: BuiltinType::Int,
                },
                ParamSchema {
                    name: "to",
                    ty: BuiltinType::Int,
                },
            ],
            variadic: false,
            result: BuiltinType::String,
            mutating: false,
        },
        // Leading and trailing whitespace removed, where whitespace is
        // Unicode whitespace as Rust's own `str::trim` sees it; `words()`
        // still splits on ASCII whitespace only, and this does not change
        // that.
        MethodSchema {
            name: "trim",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::String,
            mutating: false,
        },
        MethodSchema {
            name: "contains",
            generics: &[],
            params: &[ParamSchema {
                name: "text",
                ty: BuiltinType::String,
            }],
            variadic: false,
            result: BuiltinType::Bool,
            mutating: false,
        },
        MethodSchema {
            name: "startsWith",
            generics: &[],
            params: &[ParamSchema {
                name: "prefix",
                ty: BuiltinType::String,
            }],
            variadic: false,
            result: BuiltinType::Bool,
            mutating: false,
        },
        MethodSchema {
            name: "endsWith",
            generics: &[],
            params: &[ParamSchema {
                name: "suffix",
                ty: BuiltinType::String,
            }],
            variadic: false,
            result: BuiltinType::Bool,
            mutating: false,
        },
        // The character index `text` first occurs at, or `None`; an empty
        // `text` occurs at 0.
        MethodSchema {
            name: "indexOf",
            generics: &[],
            params: &[ParamSchema {
                name: "text",
                ty: BuiltinType::String,
            }],
            variadic: false,
            result: BuiltinType::Option(&BuiltinType::Int),
            mutating: false,
        },
        // Every non-overlapping occurrence of `old`, scanning left to right;
        // an empty `old` is refused for the same reason `split`'s empty
        // `separator` is.
        MethodSchema {
            name: "replace",
            generics: &[],
            params: &[
                ParamSchema {
                    name: "old",
                    ty: BuiltinType::String,
                },
                ParamSchema {
                    name: "new",
                    ty: BuiltinType::String,
                },
            ],
            variadic: false,
            result: BuiltinType::String,
            mutating: false,
        },
        // Unicode-aware, by Rust's own `str::to_uppercase`.
        MethodSchema {
            name: "toUpper",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::String,
            mutating: false,
        },
        // Unicode-aware, by Rust's own `str::to_lowercase`.
        MethodSchema {
            name: "toLower",
            generics: &[],
            params: &[],
            variadic: false,
            result: BuiltinType::String,
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
    cases: &[],
    fields: &[],
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
    cases: &[SOME_CASE, NONE_CASE],
    fields: &[],
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
    cases: &[OK_CASE, ERR_CASE],
    fields: &[],
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
    cases: &[],
    fields: &[],
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
    cases: &[],
    fields: &[],
    methods: &[SNAPSHOT],
    associated: &[],
};

// -------------------------------------------------------------------- Bool

/// `Bool`.
pub const BOOL: BuiltinSchema = BuiltinSchema {
    name: "Bool",
    parameters: &[],
    namespace: true,
    cases: &[],
    fields: &[],
    methods: &[SNAPSHOT],
    associated: &[],
};

// -------------------------------------------------------------------- Unit

/// `Unit`, written `()`: what an expression that produces nothing produces.
pub const UNIT: BuiltinSchema = BuiltinSchema {
    name: "Unit",
    parameters: &[],
    namespace: false,
    cases: &[],
    fields: &[],
    methods: &[SNAPSHOT],
    associated: &[],
};

// ---------------------------------------------------------------- Duration

/// `Duration`: a signed count of nanoseconds.
pub const DURATION: BuiltinSchema = BuiltinSchema {
    name: "Duration",
    parameters: &[],
    namespace: false,
    cases: &[],
    fields: &[],
    methods: &[SNAPSHOT],
    associated: &[],
};

// ------------------------------------------------------------------- Error

/// `Error`, the builtin error struct.
///
/// It is a namespace because a program writes the name — `Error("message")`
/// builds one — so a mistyped `Error.something()` should be told what `Error`
/// is rather than that the name is undeclared. There is nothing to call on
/// it: the message is read as a field, and [`MESSAGE_FIELD`] is that field.
pub const ERROR: BuiltinSchema = BuiltinSchema {
    name: "Error",
    parameters: &[],
    namespace: true,
    cases: &[],
    fields: &[MESSAGE_FIELD],
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
    cases: &[],
    fields: &[],
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
    cases: &[],
    fields: &[],
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
    cases: &[],
    fields: &[],
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

        // A case's payload and a field's type name only the receiver's
        // parameters: there is no signature of their own to bind one.
        for entry in BUILTINS {
            let mut found = Vec::new();
            for case in entry.cases {
                for payload in case.payload {
                    named(payload, &mut found);
                }
            }
            for field in entry.fields {
                named(&field.ty, &mut found);
            }
            for name in found {
                assert!(
                    entry.parameters.contains(&name),
                    "`{}` names `{name}`, which nothing binds",
                    entry.name
                );
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

    /// The builtin enums are two, and these are the four names that used to
    /// be written out in `cove-sema` and again in `cove-runtime`.
    #[test]
    fn the_builtin_enums_are_option_and_result() {
        let enums: Vec<(&str, Vec<&str>)> = BUILTINS
            .iter()
            .filter(|entry| entry.is_enum())
            .map(|entry| {
                (
                    entry.name,
                    entry.cases.iter().map(|case| case.name).collect(),
                )
            })
            .collect();
        assert_eq!(
            enums,
            [
                ("Option", vec!["Some", "None"]),
                ("Result", vec!["Ok", "Err"]),
            ]
        );
        assert_eq!(OPTION.case("Some").unwrap().signature(), "Some(T)");
        assert_eq!(OPTION.case("None").unwrap().signature(), "None");
        assert_eq!(RESULT.case("Err").unwrap().signature(), "Err(E)");
        assert!(RESULT.case("Nothing").is_none());
    }

    /// A case name belongs to one builtin enum, which is what lets a bare
    /// `Some(value)` arm say which enum a `match` is over.
    #[test]
    fn a_case_name_names_one_builtin_enum() {
        assert_eq!(
            enum_declaring("Some").map(|entry| entry.name),
            Some("Option")
        );
        assert_eq!(
            enum_declaring("None").map(|entry| entry.name),
            Some("Option")
        );
        assert_eq!(enum_declaring("Ok").map(|entry| entry.name), Some("Result"));
        assert_eq!(
            enum_declaring("Err").map(|entry| entry.name),
            Some("Result")
        );
        assert!(enum_declaring("Confirmed").is_none());
        let mut seen: Vec<&str> = Vec::new();
        for entry in BUILTINS {
            for case in entry.cases {
                assert!(
                    !seen.contains(&case.name),
                    "`{}` is a case of two builtin enums",
                    case.name
                );
                seen.push(case.name);
            }
        }
    }

    /// `None` is the one builtin case that carries nothing, which is why it
    /// is the one written as a bare name rather than as a call.
    #[test]
    fn none_is_the_only_builtin_case_that_carries_nothing() {
        let empty: Vec<&str> = BUILTINS
            .iter()
            .flat_map(|entry| entry.cases)
            .filter(|case| case.payload.is_empty())
            .map(|case| case.name)
            .collect();
        assert_eq!(empty, ["None"]);
        assert!(free_builtin(NONE_CASE.name).is_none());
    }

    /// The builtin structs are two, and their fields are what the runtime
    /// has always built and the checker used to deny.
    #[test]
    fn the_builtin_structs_are_error_and_map_entry() {
        let structs: Vec<(&str, Vec<String>)> = BUILTINS
            .iter()
            .filter(|entry| entry.is_struct())
            .map(|entry| {
                (
                    entry.name,
                    entry
                        .fields
                        .iter()
                        .map(|field| format!("{}: {}", field.name, field.ty))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            structs,
            [
                (
                    "MapEntry",
                    vec!["key: K".to_string(), "value: V".to_string()]
                ),
                ("Error", vec!["message: String".to_string()]),
            ]
        );
        assert!(ERROR.field("code").is_none());
    }

    /// `Error("boom")` takes the field the value it builds carries, so the
    /// label a call writes and the label a read writes are one word.
    #[test]
    fn the_error_constructor_takes_the_field_an_error_carries() {
        let param = ERROR_OF.params.first().expect("`Error` takes a message");
        let field = ERROR.fields.first().expect("an `Error` carries one");
        assert_eq!(param.name, field.name);
        assert_eq!(param.ty, field.ty);
    }

    /// A builtin is a struct or an enum or neither, never both: a value has
    /// cases to match or fields to read, and nothing in the language has
    /// each.
    #[test]
    fn no_builtin_type_has_both_cases_and_fields() {
        for entry in BUILTINS {
            assert!(
                !(entry.is_enum() && entry.is_struct()),
                "`{}` declares both cases and fields",
                entry.name
            );
        }
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
