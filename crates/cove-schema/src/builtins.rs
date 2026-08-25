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
//! # What is here and what is not
//!
//! The signatures are here; the implementations are not, and cannot be. A
//! builtin's body is Rust that reaches into a `Value`, so it lives in
//! `cove_runtime::builtins` beside the value model it walks. What this table
//! removes is the *second description* of those bodies: `cove-sema` reads
//! every signature from here rather than restating it, and the runtime reads
//! from here the two questions it can answer from a name alone — which type
//! names are namespaces, and which methods take a `var self` receiver.
//! `crates/cove-runtime/tests/builtin_schema.rs` closes the loop by driving
//! every entry in this table through a real interpreter, so an entry added
//! here with no implementation behind it fails a test rather than a program.
//!
//! The variants of [`BuiltinType`] cover exactly the types the table below
//! uses, on the same rule the host vocabulary follows: add one when a builtin
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
                | BuiltinType::Task(inner) => named(inner, found),
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
