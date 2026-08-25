//! Static type checking, between resolution and execution.
//!
//! ADR 0004 decides what this checks and how: annotations are mandatory at
//! boundaries and inferred inside, types are nominal with no subtyping and no
//! coercion, generics are parametric and unbounded, and checking is
//! per-module. A module sees its own declarations plus the builtins.
//!
//! # The type representation
//!
//! [`Ty`] is a closed enum of the builtin types, the structs and enums the
//! module declares, function types, and rigid type parameters. Two types are
//! equal when they name the same declaration and their arguments are equal:
//! there is no subtyping, no coercion, and no variance, so `Array<Int>` is
//! not an `Array<Any>` — there is no `Any`.
//!
//! Two variants are not types a program can write:
//!
//! - [`Ty::Unknown`] is *the checker does not know*. It compares equal to
//!   every type, and every operation on it produces `Unknown` again, so one
//!   unknown never becomes a cascade of wrong errors.
//! - [`Ty::Never`] is the type of an expression that does not produce a
//!   value, such as `return`. It also compares equal to every type, because
//!   an arm that never produces a value never disagrees with one that does.
//!
//! # Where the checker abstains
//!
//! `Unknown` is produced deliberately, never as a shrug at an expression the
//! checker simply failed to walk. It comes from exactly these places, each of
//! which is a gap in the *language*, not in this pass:
//!
//! - **Host APIs.** `console.println(...)`, `http.Request`, and every other
//!   operation or type reached through a host module. ADR 0001 promises a
//!   typed Host API schema and there is none yet, so the checker has nothing
//!   to check a host call against. It warns ([`HOST_TYPE`]) at a host *type*
//!   so the gap is visible in `cove check`.
//! - **Names no module declares.** With no module-to-module imports, a
//!   capitalized name a module does not declare cannot be resolved by any
//!   means the language offers; it is assumed to come from the host and warns
//!   ([`UNRESOLVED_NAME`], [`UNKNOWN_TYPE`]). A lowercase name has no such
//!   excuse — locals, parameters, module functions and `use`d host items are
//!   all in scope — so an unresolved one is an error ([`UNKNOWN_NAME`]).
//! - **A type used as a value.** `Vector` in `Vector.of(1, 2)` is understood
//!   as part of the call; a bare `Vector`, `console`, or `Counter` used as a
//!   value is not a form with a type in this system.
//! - **The value a `scope` binds.** `scope tasks { ... }` binds a task scope,
//!   whose only operation, `spawn`, is typed here; the scope itself is
//!   [`Ty::Scope`], a type the language gives no name to.
//! - **A lambda's `return`.** A lambda with no expected type takes its result
//!   from its body's value; an early `return` inside it is checked against
//!   nothing, because there is no written signature to check it against.
//!
//! Everything else is checked. In particular, `Unknown` is never the result
//! of a struct field, a declared parameter, or a call to a function this
//! module declares, so an ordinary program's errors cannot hide behind it.
//!
//! # What the runtime keeps
//!
//! The interpreter's own checks stay, as ADR 0004 says. Two rules are left to
//! it entirely, because they are not about types:
//!
//! - assignment to a `let` place, and the mutability of a `var` argument or
//!   `var self` receiver;
//! - the order of labeled arguments. Labels are matched to parameters by
//!   name here, and the rule that they must appear in declaration order stays
//!   where it was.
//!
//! One rule runs the other way. An `if` with no `else` has type `()` here and
//! its branch's value is discarded, because there is no second branch to give
//! the missing case a value; the interpreter, which only ever evaluates the
//! branch it took, hands that branch's value back. ADR 0004 does not settle
//! this, and the card says only that control-flow forms are expressions.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::rc::Rc;

use cove_diag::{Diagnostic, Span};
use cove_syntax::ast::{
    Arg, BinaryOp, Block, EnumDecl, Expr, ExprKind, FnDecl, Ident, ItemKind, MatchArm, Param,
    Pattern, PatternKind, Stmt, StmtKind, StrPart, StructDecl, Type, TypeKind, UnaryOp,
};

use crate::package::Package;
use crate::resolve::{Program, ResolvedModule};

/// An argument's type does not match the parameter, field, or payload it is
/// given to.
pub const MISMATCH: &str = "cove::type::mismatch";
/// A call passes more arguments than the callee declares parameters.
pub const ARITY: &str = "cove::type::arity";
/// A call omits a parameter that has no default.
pub const MISSING_ARGUMENT: &str = "cove::type::missing_argument";
/// An argument label names no parameter of the callee.
pub const UNKNOWN_LABEL: &str = "cove::type::unknown_label";
/// A lowercase name is not in scope.
pub const UNKNOWN_NAME: &str = "cove::type::unknown_name";
/// A capitalized name no module declares (warning).
pub const UNRESOLVED_NAME: &str = "cove::type::unresolved_name";
/// A type name no module declares (warning).
pub const UNKNOWN_TYPE: &str = "cove::type::unknown_type";
/// A type reached through a host module (warning).
pub const HOST_TYPE: &str = "cove::type::host_type";
/// A generic type is given the wrong number of type arguments.
pub const TYPE_ARGUMENTS: &str = "cove::type::type_arguments";
/// A type alias expands to itself.
pub const ALIAS_CYCLE: &str = "cove::type::alias_cycle";
/// A field access names no field of the receiver's type.
pub const UNKNOWN_FIELD: &str = "cove::type::unknown_field";
/// A method call names no method of the receiver's type.
pub const UNKNOWN_METHOD: &str = "cove::type::unknown_method";
/// An associated call names no associated function of the type.
pub const UNKNOWN_ASSOCIATED: &str = "cove::type::unknown_associated_function";
/// A qualified case names no case of the enum.
pub const UNKNOWN_CASE: &str = "cove::type::unknown_case";
/// An enum case is constructed with the wrong number of payload values.
pub const PAYLOAD_ARITY: &str = "cove::type::payload_arity";
/// An operator is not defined for these operand types.
pub const OPERATOR: &str = "cove::type::operator";
/// A condition is not a `Bool`.
pub const CONDITION: &str = "cove::type::condition";
/// The branches of an `if` or the arms of a `match` produce different types.
pub const BRANCHES: &str = "cove::type::branches";
/// `?` was applied to something that is not a `Result` or an `Option`.
pub const TRY_OPERAND: &str = "cove::type::try_operand";
/// `?` propagates a failure the enclosing function cannot return.
pub const TRY_RETURN: &str = "cove::type::try_return";
/// `await` was applied to something that is not a `Task`.
pub const AWAIT_OPERAND: &str = "cove::type::await_operand";
/// `for` was given something it cannot iterate.
pub const ITERABLE: &str = "cove::type::iterable";
/// A call was made to something that is not a function.
pub const NOT_CALLABLE: &str = "cove::type::not_callable";
/// A pattern matches a different type than the scrutinee.
pub const PATTERN: &str = "cove::type::pattern";
/// A method was called without a receiver, or an associated function with one.
pub const RECEIVER: &str = "cove::type::receiver";
/// An assignment target is not a place.
pub const NOT_A_PLACE: &str = "cove::type::not_a_place";
/// An entry function's shape does not fit the host boundary.
pub const ENTRY: &str = "cove::type::entry";

/// Type-checks a resolved program.
///
/// Every module is checked against its own declarations plus the builtins,
/// and every `[run.<name>]` entry against the shape the host boundary calls.
/// The result holds both errors and warnings; an empty result means the
/// program checks.
pub fn check(package: &Package, program: &Program) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut checked: BTreeMap<&str, Checker> = BTreeMap::new();
    for (name, module) in &program.modules {
        let mut checker = Checker::new(module);
        checker.check_module();
        diagnostics.append(&mut checker.diagnostics);
        checked.insert(name.as_str(), checker);
    }
    check_entries(package, &checked, &mut diagnostics);
    diagnostics
}

// ------------------------------------------------------------------- types

/// A Cove type.
#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    /// The checker could not determine this type; see the module docs.
    Unknown,
    /// The type of an expression that never produces a value, such as
    /// `return`.
    Never,
    Unit,
    Bool,
    Int,
    Float,
    Str,
    Duration,
    Error,
    Range,
    Array(Box<Ty>),
    Vector(Box<Ty>),
    Set(Box<Ty>),
    Map(Box<Ty>, Box<Ty>),
    /// One `key`/`value` pair: what `Map.of` collects and what `for` binds
    /// over a `Map`.
    MapEntry(Box<Ty>, Box<Ty>),
    Option(Box<Ty>),
    Result(Box<Ty>, Box<Ty>),
    Task(Box<Ty>),
    /// The value `scope name { ... }` binds.
    Scope,
    /// A struct this module declares, with its type arguments.
    Struct(Rc<str>, Vec<Ty>),
    /// An enum this module declares, with its type arguments.
    Enum(Rc<str>, Vec<Ty>),
    Fn(Rc<FnTy>),
    /// A type parameter, rigid inside the body that declares it.
    Param(Rc<str>),
}

/// A function type: `fn(Int) -> Int`, `async fn() -> Result<Unit, Error>`.
#[derive(Clone, Debug, PartialEq)]
pub struct FnTy {
    pub is_async: bool,
    pub params: Vec<Ty>,
    pub ret: Ty,
}

impl Ty {
    fn func(is_async: bool, params: Vec<Ty>, ret: Ty) -> Ty {
        Ty::Fn(Rc::new(FnTy {
            is_async,
            params,
            ret,
        }))
    }

    /// Whether this type carries no information, so a diagnostic about it
    /// would be a guess.
    fn is_wild(&self) -> bool {
        matches!(self, Ty::Unknown | Ty::Never)
    }

    /// Nominal equality, with `Unknown` and `Never` equal to everything.
    ///
    /// Generic arguments are invariant: `Array<Int>` and `Array<Float>` are
    /// unrelated types, in either direction.
    fn matches(&self, other: &Ty) -> bool {
        if self.is_wild() || other.is_wild() {
            return true;
        }
        match (self, other) {
            (Ty::Array(a), Ty::Array(b))
            | (Ty::Vector(a), Ty::Vector(b))
            | (Ty::Set(a), Ty::Set(b))
            | (Ty::Option(a), Ty::Option(b))
            | (Ty::Task(a), Ty::Task(b)) => a.matches(b),
            (Ty::Map(ak, av), Ty::Map(bk, bv))
            | (Ty::MapEntry(ak, av), Ty::MapEntry(bk, bv))
            | (Ty::Result(ak, av), Ty::Result(bk, bv)) => ak.matches(bk) && av.matches(bv),
            (Ty::Struct(a, aargs), Ty::Struct(b, bargs))
            | (Ty::Enum(a, aargs), Ty::Enum(b, bargs)) => {
                a == b
                    && aargs.len() == bargs.len()
                    && aargs.iter().zip(bargs).all(|(a, b)| a.matches(b))
            }
            (Ty::Fn(a), Ty::Fn(b)) => {
                a.is_async == b.is_async
                    && a.params.len() == b.params.len()
                    && a.params.iter().zip(&b.params).all(|(a, b)| a.matches(b))
                    && a.ret.matches(&b.ret)
            }
            (Ty::Param(a), Ty::Param(b)) => a == b,
            (a, b) => std::mem::discriminant(a) == std::mem::discriminant(b),
        }
    }

    /// The more informative of two types that already [`Ty::matches`], used
    /// where two branches must agree: a known type wins over `Unknown`, and
    /// any type wins over `Never`.
    fn join(&self, other: &Ty) -> Ty {
        match (self, other) {
            (Ty::Never, other) | (other, Ty::Never) => other.clone(),
            (Ty::Unknown, other) | (other, Ty::Unknown) => other.clone(),
            _ => self.clone(),
        }
    }

    /// Replaces every type parameter bound in `subst`, leaving the rest.
    fn substitute(&self, subst: &BTreeMap<Rc<str>, Ty>) -> Ty {
        if subst.is_empty() {
            return self.clone();
        }
        match self {
            Ty::Param(name) => subst.get(name).cloned().unwrap_or_else(|| self.clone()),
            Ty::Array(inner) => Ty::Array(Box::new(inner.substitute(subst))),
            Ty::Vector(inner) => Ty::Vector(Box::new(inner.substitute(subst))),
            Ty::Set(inner) => Ty::Set(Box::new(inner.substitute(subst))),
            Ty::Option(inner) => Ty::Option(Box::new(inner.substitute(subst))),
            Ty::Task(inner) => Ty::Task(Box::new(inner.substitute(subst))),
            Ty::Map(k, v) => Ty::Map(Box::new(k.substitute(subst)), Box::new(v.substitute(subst))),
            Ty::MapEntry(k, v) => {
                Ty::MapEntry(Box::new(k.substitute(subst)), Box::new(v.substitute(subst)))
            }
            Ty::Result(t, e) => {
                Ty::Result(Box::new(t.substitute(subst)), Box::new(e.substitute(subst)))
            }
            Ty::Struct(name, args) => Ty::Struct(
                name.clone(),
                args.iter().map(|a| a.substitute(subst)).collect(),
            ),
            Ty::Enum(name, args) => Ty::Enum(
                name.clone(),
                args.iter().map(|a| a.substitute(subst)).collect(),
            ),
            Ty::Fn(f) => Ty::func(
                f.is_async,
                f.params.iter().map(|p| p.substitute(subst)).collect(),
                f.ret.substitute(subst),
            ),
            other => other.clone(),
        }
    }
}

/// Renders a type in the source form it would be written in, so a diagnostic
/// shows the type the reader wrote.
impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // An unknown type reads as `_` because that is how an
            // unconstrained position is written in Cove source.
            Ty::Unknown => f.write_str("_"),
            Ty::Never => f.write_str("!"),
            Ty::Unit => f.write_str("()"),
            Ty::Bool => f.write_str("Bool"),
            Ty::Int => f.write_str("Int"),
            Ty::Float => f.write_str("Float"),
            Ty::Str => f.write_str("String"),
            Ty::Duration => f.write_str("Duration"),
            Ty::Error => f.write_str("Error"),
            Ty::Range => f.write_str("Range"),
            Ty::Scope => f.write_str("Scope"),
            Ty::Array(inner) => write!(f, "Array<{inner}>"),
            Ty::Vector(inner) => write!(f, "Vector<{inner}>"),
            Ty::Set(inner) => write!(f, "Set<{inner}>"),
            Ty::Option(inner) => write!(f, "Option<{inner}>"),
            Ty::Task(inner) => write!(f, "Task<{inner}>"),
            Ty::Map(k, v) => write!(f, "Map<{k}, {v}>"),
            Ty::MapEntry(k, v) => write!(f, "MapEntry<{k}, {v}>"),
            Ty::Result(t, e) => write!(f, "Result<{t}, {e}>"),
            Ty::Param(name) => f.write_str(name),
            Ty::Struct(name, args) | Ty::Enum(name, args) => {
                f.write_str(name)?;
                if !args.is_empty() {
                    let args: Vec<String> = args.iter().map(Ty::to_string).collect();
                    write!(f, "<{}>", args.join(", "))?;
                }
                Ok(())
            }
            Ty::Fn(func) => {
                if func.is_async {
                    f.write_str("async ")?;
                }
                let params: Vec<String> = func.params.iter().map(Ty::to_string).collect();
                write!(f, "fn({})", params.join(", "))?;
                if func.ret != Ty::Unit {
                    write!(f, " -> {}", func.ret)?;
                }
                Ok(())
            }
        }
    }
}

// -------------------------------------------------------------- signatures

/// One parameter of a declared function, method, or synthesized struct
/// initializer.
#[derive(Clone, Debug)]
struct ParamSig {
    name: String,
    ty: Ty,
    variadic: bool,
    has_default: bool,
    span: Span,
}

/// A declared function's or method's type, as written at its boundary.
#[derive(Clone, Debug)]
struct FnSig {
    /// Type parameters this signature binds, rigid inside its own body.
    generics: Vec<Rc<str>>,
    params: Vec<ParamSig>,
    ret: Ty,
    ret_span: Span,
    is_async: bool,
    /// The type of `self`, for a method.
    receiver: Option<Ty>,
}

impl FnSig {
    /// The type of this function used as a value, which is what a bare
    /// reference to it evaluates to.
    fn as_value(&self) -> Ty {
        Ty::func(
            self.is_async,
            self.params.iter().map(|p| p.ty.clone()).collect(),
            self.ret.clone(),
        )
    }
}

/// A struct's fields, in declaration order.
#[derive(Clone, Debug)]
struct StructSig {
    generics: Vec<Rc<str>>,
    fields: Vec<ParamSig>,
}

/// An enum's cases, in declaration order.
#[derive(Clone, Debug)]
struct EnumSig {
    generics: Vec<Rc<str>>,
    cases: Vec<CaseSig>,
}

#[derive(Clone, Debug)]
struct CaseSig {
    name: String,
    payload: Vec<Ty>,
    span: Span,
}

/// What a binding in a body means.
#[derive(Clone, Debug)]
struct Binding {
    ty: Ty,
}

/// Where an expectation came from, so a mismatch can point at the
/// declaration that imposed it as well as the expression that broke it.
#[derive(Clone, Debug)]
struct Origin {
    span: Span,
    label: String,
}

/// A type an expression is checked against, and the declaration that asks
/// for it.
#[derive(Clone, Debug)]
struct Expected {
    ty: Ty,
    origin: Option<Origin>,
}

impl Expected {
    fn new(ty: Ty, span: Span, label: impl Into<String>) -> Expected {
        Expected {
            ty,
            origin: Some(Origin {
                span,
                label: label.into(),
            }),
        }
    }
}

// ---------------------------------------------------------------- checking

/// Checks one module against its own declarations plus the builtins.
struct Checker<'a> {
    module: &'a ResolvedModule,
    diagnostics: Vec<Diagnostic>,
    functions: BTreeMap<String, FnSig>,
    methods: BTreeMap<(String, String), FnSig>,
    structs: BTreeMap<String, StructSig>,
    enums: BTreeMap<String, EnumSig>,
    /// Expanded type aliases, resolved once so the names inside an alias are
    /// reported once however many times it is used.
    aliases: BTreeMap<String, (Vec<Rc<str>>, Ty)>,
    /// Aliases currently being expanded, to catch `type A = A`.
    expanding: Vec<String>,
    /// Type parameters in scope, innermost last.
    type_params: Vec<Rc<str>>,
    scopes: Vec<BTreeMap<String, Binding>>,
    /// The declared return type of the function whose body is being checked,
    /// and where it was written.
    ret: Ty,
    ret_span: Span,
}

impl<'a> Checker<'a> {
    fn new(module: &'a ResolvedModule) -> Checker<'a> {
        Checker {
            module,
            diagnostics: Vec::new(),
            functions: BTreeMap::new(),
            methods: BTreeMap::new(),
            structs: BTreeMap::new(),
            enums: BTreeMap::new(),
            aliases: BTreeMap::new(),
            expanding: Vec::new(),
            type_params: Vec::new(),
            scopes: Vec::new(),
            ret: Ty::Unknown,
            ret_span: Span::new(cove_diag::FileId(0), 0, 0),
        }
    }

    /// Resolves every declaration's written types, then checks every body.
    fn check_module(&mut self) {
        self.prepare();
        let fn_names: Vec<String> = self.module.functions.keys().cloned().collect();
        for name in fn_names {
            let decl = self.module.functions[&name].decl.clone();
            let sig = self.functions[&name].clone();
            self.check_body(&decl, &sig);
        }
        let method_keys: Vec<(String, String)> = self.module.methods.keys().cloned().collect();
        for key in method_keys {
            let decl = self.module.methods[&key].decl.clone();
            let sig = self.methods[&key].clone();
            self.check_body(&decl, &sig);
        }
    }

    /// Resolves every written type of every declaration, once.
    ///
    /// A type written once is reported once, however many bodies mention it,
    /// because a body reads the resolved signature rather than the syntax.
    fn prepare(&mut self) {
        let alias_names: Vec<String> = self.module.aliases.keys().cloned().collect();
        for name in alias_names {
            self.alias(&name);
        }

        let struct_names: Vec<String> = self.module.structs.keys().cloned().collect();
        for name in struct_names {
            let decl = self.module.structs[&name].decl.clone();
            let sig = self.struct_sig(&decl);
            self.structs.insert(name, sig);
        }

        let enum_names: Vec<String> = self.module.enums.keys().cloned().collect();
        for name in enum_names {
            let decl = self.module.enums[&name].decl.clone();
            let sig = self.enum_sig(&decl);
            self.enums.insert(name, sig);
        }

        let fn_names: Vec<String> = self.module.functions.keys().cloned().collect();
        for name in fn_names {
            let decl = self.module.functions[&name].decl.clone();
            let sig = self.fn_sig(&decl, None);
            self.functions.insert(name, sig);
        }

        let method_keys: Vec<(String, String)> = self.module.methods.keys().cloned().collect();
        for key in method_keys {
            let decl = self.module.methods[&key].decl.clone();
            let sig = self.fn_sig(&decl, Some(&key.0));
            self.methods.insert(key, sig);
        }
    }

    /// Checks one function's or method's body against its declared return
    /// type.
    ///
    /// A function with no `->` returns `Unit`, so its body's value must be
    /// `Unit` too.
    fn check_body(&mut self, decl: &FnDecl, sig: &FnSig) {
        self.type_params = sig.generics.clone();
        self.ret = sig.ret.clone();
        self.ret_span = sig.ret_span;
        self.scopes.push(BTreeMap::new());
        if let Some(receiver) = &sig.receiver {
            self.declare("self", receiver.clone());
        }
        for param in &sig.params {
            let ty = if param.variadic {
                Ty::Array(Box::new(param.ty.clone()))
            } else {
                param.ty.clone()
            };
            self.declare(&param.name, ty);
        }
        for (param, declared) in decl.params.iter().zip(&sig.params) {
            if let Some(default) = &param.default {
                let expected = Expected::new(
                    declared.ty.clone(),
                    param.name.span,
                    format!("this parameter is `{}`", declared.ty),
                );
                self.expr(default, Some(&expected));
            }
        }
        let expected = Expected::new(
            sig.ret.clone(),
            sig.ret_span,
            if decl.return_type.is_some() {
                format!("the declared return type is `{}`", sig.ret)
            } else {
                "this function declares no return type, so it returns `()`".to_string()
            },
        );
        self.block(&decl.body, Some(&expected));
        self.scopes.pop();
        self.type_params.clear();
    }

    // ------------------------------------------------------ declarations

    fn struct_sig(&mut self, decl: &StructDecl) -> StructSig {
        let outer = std::mem::take(&mut self.type_params);
        let generics = self.enter_generics(&decl.generics);
        let fields = decl
            .fields
            .iter()
            .map(|field| ParamSig {
                name: field.name.node.clone(),
                ty: self.resolve(&field.ty),
                variadic: false,
                has_default: false,
                span: field.name.span,
            })
            .collect();
        self.type_params = outer;
        StructSig { generics, fields }
    }

    fn enum_sig(&mut self, decl: &EnumDecl) -> EnumSig {
        let outer = std::mem::take(&mut self.type_params);
        let generics = self.enter_generics(&decl.generics);
        let cases = decl
            .cases
            .iter()
            .map(|case| CaseSig {
                name: case.name.node.clone(),
                payload: case.payload.iter().map(|ty| self.resolve(ty)).collect(),
                span: case.name.span,
            })
            .collect();
        self.type_params = outer;
        EnumSig { generics, cases }
    }

    /// The signature of a free function, or of a method of `receiver_type`.
    ///
    /// A method's type parameters are the ones its type declares. Resolution
    /// does not record an `impl` block's own parameter list, so an `impl`
    /// that renames its type's parameters is not supported.
    fn fn_sig(&mut self, decl: &FnDecl, receiver_type: Option<&str>) -> FnSig {
        let mut type_generics: Vec<Ident> = Vec::new();
        if let Some(type_name) = receiver_type {
            if let Some(entry) = self.module.structs.get(type_name) {
                type_generics.extend(entry.decl.generics.iter().cloned());
            } else if let Some(entry) = self.module.enums.get(type_name) {
                type_generics.extend(entry.decl.generics.iter().cloned());
            }
        }
        let mut names = type_generics.clone();
        names.extend(decl.generics.iter().cloned());
        let outer = self.type_params.clone();
        let generics = self.enter_generics(&names);
        let owner_arity = type_generics.len();

        let params = decl
            .params
            .iter()
            .map(|param| self.param_sig(param))
            .collect::<Vec<_>>();
        let ret = match &decl.return_type {
            Some(ty) => self.resolve(ty),
            None => Ty::Unit,
        };
        let ret_span = match &decl.return_type {
            Some(ty) => ty.span,
            None => decl.name.span,
        };
        let receiver = receiver_type
            .filter(|_| decl.receiver.is_some())
            .map(|name| {
                let args: Vec<Ty> = generics
                    .iter()
                    .take(owner_arity)
                    .map(|p| Ty::Param(p.clone()))
                    .collect();
                self.nominal(name, args)
            });
        self.type_params = outer;
        FnSig {
            generics,
            params,
            ret,
            ret_span,
            is_async: decl.is_async,
            receiver,
        }
    }

    /// A parameter's declared type. Its default value is checked with the
    /// body, in [`Checker::check_body`], where every signature in the module
    /// is known and the other parameters are in scope.
    fn param_sig(&mut self, param: &Param) -> ParamSig {
        let ty = match &param.ty {
            Some(ty) => self.resolve(ty),
            None => Ty::Unknown,
        };
        ParamSig {
            name: param.name.node.clone(),
            ty,
            variadic: param.variadic,
            has_default: param.default.is_some(),
            span: param.span,
        }
    }

    /// Brings `names` into scope as type parameters, on top of whatever is
    /// already in scope, and returns just the ones it added. Every caller
    /// restores the previous list when the declaration ends.
    fn enter_generics(&mut self, names: &[Ident]) -> Vec<Rc<str>> {
        let generics: Vec<Rc<str>> = names.iter().map(|n| n.node.as_str().into()).collect();
        self.type_params.extend(generics.iter().cloned());
        generics
    }

    /// A struct or enum this module declares, or `Unknown` when it declares
    /// neither.
    fn nominal(&self, name: &str, args: Vec<Ty>) -> Ty {
        if self.module.structs.contains_key(name) {
            Ty::Struct(name.into(), args)
        } else if self.module.enums.contains_key(name) {
            Ty::Enum(name.into(), args)
        } else {
            Ty::Unknown
        }
    }

    // ---------------------------------------------------- written types

    /// Resolves a written type against the builtins, this module's
    /// declarations, and the type parameters in scope.
    fn resolve(&mut self, ty: &Type) -> Ty {
        match &ty.kind {
            TypeKind::Unit => Ty::Unit,
            TypeKind::Fn {
                is_async,
                params,
                return_type,
            } => {
                let params = params
                    .iter()
                    .map(|param| match &param.ty {
                        Some(ty) => self.resolve(ty),
                        None => Ty::Unknown,
                    })
                    .collect();
                let ret = match return_type {
                    Some(ty) => self.resolve(ty),
                    None => Ty::Unit,
                };
                Ty::func(*is_async, params, ret)
            }
            TypeKind::Named { path, args } => self.resolve_named(path, args, ty.span),
        }
    }

    fn resolve_named(&mut self, path: &[Ident], args: &[Type], span: Span) -> Ty {
        let arguments: Vec<Ty> = args.iter().map(|arg| self.resolve(arg)).collect();
        if path.len() > 1 {
            let head = &path[0].node;
            if self.module.host_uses.contains(head.as_str()) {
                self.diagnostics.push(
                    Diagnostic::warning(
                        HOST_TYPE,
                        format!(
                            "`{}` is a host type, so values of it are unchecked",
                            join_path(path)
                        ),
                    )
                    .at(span)
                    .rule("A Host API's types come from its schema, and there is no schema yet.")
                    .help(
                        "the checker treats this type as unknown; every operation on it is left to the runtime",
                    ),
                );
            } else {
                self.diagnostics.push(
                    Diagnostic::warning(
                        UNKNOWN_TYPE,
                        format!("`{}` names no type this module can see", join_path(path)),
                    )
                    .at(span)
                    .rule("A module sees its own declarations plus the builtins; `use` names a host module.")
                    .help(format!(
                        "add `use {}` if `{}` is a host module, or declare the type in this module",
                        path[0].node,
                        path[0].node
                    )),
                );
            }
            return Ty::Unknown;
        }

        let name = path[0].node.as_str();
        if let Some(param) = self.type_params.iter().find(|p| &***p == name).cloned() {
            self.check_type_arity(name, 0, arguments.len(), span);
            return Ty::Param(param);
        }
        if let Some(ty) = self.builtin_type(name, &arguments, span) {
            return ty;
        }
        if let Some(entry) = self.module.structs.get(name) {
            let declared = entry.decl.generics.len();
            self.check_type_arity(name, declared, arguments.len(), span);
            return Ty::Struct(name.into(), fit(arguments, declared));
        }
        if let Some(entry) = self.module.enums.get(name) {
            let declared = entry.decl.generics.len();
            self.check_type_arity(name, declared, arguments.len(), span);
            return Ty::Enum(name.into(), fit(arguments, declared));
        }
        if self.module.aliases.contains_key(name) {
            let (generics, ty) = self.alias(name);
            self.check_type_arity(name, generics.len(), arguments.len(), span);
            let subst = generics
                .into_iter()
                .zip(
                    fit(arguments, 0)
                        .into_iter()
                        .chain(std::iter::repeat(Ty::Unknown)),
                )
                .collect();
            return ty.substitute(&subst);
        }
        self.diagnostics.push(
            Diagnostic::warning(
                UNKNOWN_TYPE,
                format!("`{name}` names no type this module declares"),
            )
            .at(span)
            .rule("A module sees its own declarations plus the builtins; there are no module-to-module imports yet.")
            .help(format!(
                "declare `struct {name}`, `enum {name}`, or `type {name} = ...` in this module; until then values of `{name}` are unchecked"
            )),
        );
        Ty::Unknown
    }

    /// The builtin named `name`, with its arity checked.
    fn builtin_type(&mut self, name: &str, args: &[Ty], span: Span) -> Option<Ty> {
        let arity = match name {
            "Unit" | "Bool" | "Int" | "Float" | "String" | "Duration" | "Error" | "Range" => 0,
            "Array" | "Vector" | "Set" | "Option" | "Task" => 1,
            "Map" | "MapEntry" | "Result" => 2,
            _ => return None,
        };
        self.check_type_arity(name, arity, args.len(), span);
        let first = args.first().cloned().unwrap_or(Ty::Unknown);
        let second = args.get(1).cloned().unwrap_or(Ty::Unknown);
        Some(match name {
            "Unit" => Ty::Unit,
            "Bool" => Ty::Bool,
            "Int" => Ty::Int,
            "Float" => Ty::Float,
            "String" => Ty::Str,
            "Duration" => Ty::Duration,
            "Error" => Ty::Error,
            "Range" => Ty::Range,
            "Array" => Ty::Array(Box::new(first)),
            "Vector" => Ty::Vector(Box::new(first)),
            "Set" => Ty::Set(Box::new(first)),
            "Option" => Ty::Option(Box::new(first)),
            "Task" => Ty::Task(Box::new(first)),
            "Map" => Ty::Map(Box::new(first), Box::new(second)),
            "MapEntry" => Ty::MapEntry(Box::new(first), Box::new(second)),
            _ => Ty::Result(Box::new(first), Box::new(second)),
        })
    }

    fn check_type_arity(&mut self, name: &str, expected: usize, found: usize, span: Span) {
        if expected == found {
            return;
        }
        self.diagnostics.push(
            Diagnostic::error(
                TYPE_ARGUMENTS,
                format!("`{name}` takes {expected} type argument(s), but {found} were written"),
            )
            .at(span)
            .rule("A generic type is written with exactly the arguments its declaration binds.")
            .help(if expected == 0 {
                format!("write `{name}`")
            } else {
                format!(
                    "write `{name}<{}>`",
                    (0..expected).map(|_| "_").collect::<Vec<_>>().join(", ")
                )
            }),
        );
    }

    /// Expands a type alias, once per module.
    fn alias(&mut self, name: &str) -> (Vec<Rc<str>>, Ty) {
        if let Some(cached) = self.aliases.get(name) {
            return cached.clone();
        }
        let Some(entry) = self.module.aliases.get(name) else {
            return (Vec::new(), Ty::Unknown);
        };
        let decl = entry.decl.clone();
        if self.expanding.iter().any(|n| n == name) {
            self.diagnostics.push(
                Diagnostic::error(ALIAS_CYCLE, format!("`{name}` expands to itself"))
                    .at(decl.name.span)
                    .rule("A type alias names an existing type; it cannot be defined in terms of itself.")
                    .help(format!(
                        "declare `struct {name}` or `enum {name}` instead, which may refer to itself through a field"
                    )),
            );
            return (Vec::new(), Ty::Unknown);
        }
        self.expanding.push(name.to_string());
        let outer = std::mem::take(&mut self.type_params);
        let generics = self.enter_generics(&decl.generics);
        let ty = self.resolve(&decl.ty);
        self.type_params = outer;
        self.expanding.pop();
        let resolved = (generics, ty);
        self.aliases.insert(name.to_string(), resolved.clone());
        resolved
    }

    // ------------------------------------------------------------ scopes

    fn declare(&mut self, name: &str, ty: Ty) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), Binding { ty });
        }
    }

    fn lookup(&self, name: &str) -> Option<&Binding> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    // ------------------------------------------------------- expressions

    /// Checks a block and returns the type of its value, which is its tail
    /// expression's type or `Unit` when it has none.
    fn block(&mut self, block: &Block, expected: Option<&Expected>) -> Ty {
        self.scopes.push(BTreeMap::new());
        for stmt in &block.statements {
            self.stmt(stmt);
        }
        let ty = match &block.tail {
            Some(tail) => self.expr(tail, expected),
            None => {
                let ty = Ty::Unit;
                if let Some(expected) = expected {
                    self.expect(&ty, expected, block.span);
                }
                ty
            }
        };
        self.scopes.pop();
        ty
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match &stmt.kind {
            StmtKind::Let {
                name, ty, value, ..
            } => {
                let bound = match ty {
                    Some(written) => {
                        let declared = self.resolve(written);
                        let expected = Expected::new(
                            declared.clone(),
                            written.span,
                            format!("the declared type is `{declared}`"),
                        );
                        self.expr(value, Some(&expected));
                        declared
                    }
                    None => {
                        let inferred = self.expr(value, None);
                        // A binding whose initializer never produces a value
                        // has no type to infer; stop rather than guess.
                        if inferred == Ty::Never {
                            Ty::Unknown
                        } else {
                            inferred
                        }
                    }
                };
                self.declare(&name.node, bound);
            }
            StmtKind::Expr(expr) => {
                self.expr(expr, None);
            }
            StmtKind::Item(item) => {
                // A local `fn` is an ordinary closure the body can call.
                if let ItemKind::Fn(decl) = &item.kind {
                    let outer_params = self.type_params.clone();
                    let sig = self.fn_sig(decl, None);
                    self.declare(&decl.name.node, sig.as_value());
                    let outer_ret = std::mem::replace(&mut self.ret, sig.ret.clone());
                    let outer_span = std::mem::replace(&mut self.ret_span, sig.ret_span);
                    self.type_params.extend(sig.generics.iter().cloned());
                    self.scopes.push(BTreeMap::new());
                    for param in &sig.params {
                        self.declare(&param.name, param.ty.clone());
                    }
                    let expected = Expected::new(
                        sig.ret.clone(),
                        sig.ret_span,
                        format!("the declared return type is `{}`", sig.ret),
                    );
                    self.block(&decl.body, Some(&expected));
                    self.scopes.pop();
                    self.type_params = outer_params;
                    self.ret_span = outer_span;
                    self.ret = outer_ret;
                }
            }
        }
    }

    /// Checks an expression, against `expected` when the surrounding form
    /// imposes one, and returns its type.
    fn expr(&mut self, expr: &Expr, expected: Option<&Expected>) -> Ty {
        let span = expr.span;
        let ty = match &expr.kind {
            ExprKind::Int(_) => Ty::Int,
            ExprKind::Float(_) => Ty::Float,
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Duration(_) => Ty::Duration,
            ExprKind::Unit => Ty::Unit,
            ExprKind::Str(parts) => {
                for part in parts {
                    if let StrPart::Interpolation(inner) = part {
                        // Interpolation renders any value, so an interpolated
                        // expression is checked but not constrained.
                        self.expr(inner, None);
                    }
                }
                Ty::Str
            }
            ExprKind::Ident(name) => self.ident(name, span, expected),
            ExprKind::ArrayLit(items) => self.array_literal(items, span, expected),
            ExprKind::Field { base, name } => self.field(base, name, span),
            ExprKind::Call {
                callee,
                generics,
                args,
                trailing,
            } => self.call(callee, generics, args, trailing.as_deref(), span, expected),
            ExprKind::Unary { op, operand } => self.unary(*op, operand, span),
            ExprKind::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs, span),
            ExprKind::Assign { op, target, value } => self.assign(*op, target, value, span),
            ExprKind::Try(inner) => self.try_expr(inner, span),
            ExprKind::Await(inner) => self.await_expr(inner, span),
            ExprKind::Block(block) => return self.block(block, expected),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                return self.if_expr(
                    condition,
                    then_branch,
                    else_branch.as_deref(),
                    span,
                    expected,
                )
            }
            ExprKind::Match { scrutinee, arms } => {
                return self.match_expr(scrutinee, arms, span, expected)
            }
            ExprKind::For {
                binding,
                iterable,
                body,
            } => self.for_expr(binding, iterable, body),
            ExprKind::While { condition, body } => {
                self.condition(condition);
                self.block(body, None);
                Ty::Unit
            }
            ExprKind::Return(value) => {
                let expected = Expected::new(
                    self.ret.clone(),
                    self.ret_span,
                    format!("the declared return type is `{}`", self.ret),
                );
                match value {
                    Some(value) => {
                        self.expr(value, Some(&expected));
                    }
                    None => self.expect(&Ty::Unit, &expected, span),
                }
                Ty::Never
            }
            // A loop's value comes from `break`, so a `break` operand is
            // checked against the loop's expected type rather than the
            // function's. Neither form produces a value of its own.
            ExprKind::Break(value) => {
                if let Some(value) = value {
                    self.expr(value, None);
                }
                Ty::Never
            }
            ExprKind::Continue => Ty::Never,
            ExprKind::Lambda {
                is_async,
                params,
                body,
            } => return self.lambda(*is_async, params, body, span, expected),
            ExprKind::Scope { name, body } => {
                self.scopes.push(BTreeMap::new());
                self.declare(&name.node, Ty::Scope);
                let ty = self.block(body, expected);
                self.scopes.pop();
                return ty;
            }
            ExprKind::Range {
                start,
                end,
                inclusive_end: _,
            } => {
                let bound = Expected::new(Ty::Int, span, "a range runs between two `Int`s");
                self.expr(start, Some(&bound));
                self.expr(end, Some(&bound));
                Ty::Range
            }
        };
        if let Some(expected) = expected {
            self.expect(&ty, expected, span);
        }
        ty
    }

    /// Reports a type that does not match what the surrounding form asked
    /// for, pointing at the expression and labelling what imposed it.
    fn expect(&mut self, found: &Ty, expected: &Expected, span: Span) {
        if found.matches(&expected.ty) {
            return;
        }
        let mut diagnostic = Diagnostic::error(
            MISMATCH,
            format!("expected `{}`, found `{found}`", expected.ty),
        )
        .at(span)
        .rule("Types are nominal and there are no implicit conversions: a value must already have the type its place asks for.");
        if let Some(origin) = &expected.origin {
            diagnostic = diagnostic.label(origin.span, origin.label.clone());
        }
        if let Some(help) = conversion_help(&expected.ty, found) {
            diagnostic = diagnostic.help(help);
        }
        self.diagnostics.push(diagnostic);
    }

    /// A bare name: a local, a module function, a constructor, a host item,
    /// or a name only the host can explain.
    fn ident(&mut self, name: &str, span: Span, expected: Option<&Expected>) -> Ty {
        if let Some(binding) = self.lookup(name) {
            return binding.ty.clone();
        }
        if name == "None" {
            return match expected.map(|e| &e.ty) {
                Some(Ty::Option(inner)) => Ty::Option(inner.clone()),
                _ => Ty::Option(Box::new(Ty::Unknown)),
            };
        }
        if let Some(sig) = self.functions.get(name) {
            return sig.as_value();
        }
        // A type or a host module used as a value has no type in this system;
        // the forms that give it meaning (`Vector.of`, `MapEntry(key:,
        // value:)`, `console.println`) are understood at the call itself.
        if self.module.structs.contains_key(name)
            || self.module.enums.contains_key(name)
            || is_builtin_type(name)
            || name == "MapEntry"
            || self.module.host_uses.contains(name)
            || self.module.host_items.contains_key(name)
        {
            return Ty::Unknown;
        }
        self.unresolved_name(name, span)
    }

    /// The type of a name nothing in scope explains.
    ///
    /// A capitalized name is assumed to come from the host: with no
    /// module-to-module imports, there is no other way for one to reach this
    /// module, so the checker says so and abstains. A lowercase name has no
    /// such excuse.
    fn unresolved_name(&mut self, name: &str, span: Span) -> Ty {
        if starts_uppercase(name) {
            self.diagnostics.push(
                Diagnostic::warning(
                    UNRESOLVED_NAME,
                    format!("`{name}` is not declared in this module, so it is unchecked"),
                )
                .at(span)
                .rule("A module sees its own declarations plus the builtins; anything else must come from a host module.")
                .help(format!(
                    "declare `{name}` in this module, or leave it to the host; until the Host API schema exists, values of `{name}` are unchecked"
                )),
            );
        } else {
            self.diagnostics.push(
                Diagnostic::error(UNKNOWN_NAME, format!("cannot find `{name}` in this scope"))
                    .at(span)
                    .rule("A name must be a local binding, a parameter, a declaration of this module, or a `use`d host item.")
                    .help(format!(
                        "declare `let {name} = ...` before this expression, or `use <host>.{name}`"
                    )),
            );
        }
        Ty::Unknown
    }

    fn array_literal(&mut self, items: &[Expr], span: Span, expected: Option<&Expected>) -> Ty {
        let element_hint = match expected.map(|e| &e.ty) {
            Some(Ty::Array(inner)) => Some((**inner).clone()),
            _ => None,
        };
        let mut element = element_hint.clone().unwrap_or(Ty::Unknown);
        for item in items {
            let hint = element_hint
                .clone()
                .map(|ty| {
                    let label = format!("the array's element type is `{ty}`");
                    Expected::new(ty, span, label)
                })
                .or_else(|| {
                    (!element.is_wild()).then(|| {
                        Expected::new(
                            element.clone(),
                            span,
                            format!("the first element is `{element}`"),
                        )
                    })
                });
            let ty = self.expr(item, hint.as_ref());
            element = element.join(&ty);
        }
        Ty::Array(Box::new(element))
    }

    /// `base.name`: an enum case, a host operation, or a struct field.
    fn field(&mut self, base: &Expr, name: &Ident, span: Span) -> Ty {
        if let ExprKind::Ident(head) = &base.kind {
            if self.lookup(head).is_none() {
                if self.module.enums.contains_key(head.as_str()) {
                    return self.enum_case(head, name, &[], span);
                }
                if self.module.host_uses.contains(head.as_str()) {
                    return Ty::Unknown;
                }
            }
        }
        let base_ty = self.expr(base, None);
        self.field_of(&base_ty, name, span)
    }

    fn field_of(&mut self, base_ty: &Ty, name: &Ident, span: Span) -> Ty {
        match base_ty {
            Ty::Unknown => Ty::Unknown,
            Ty::Struct(struct_name, args) => {
                let Some(sig) = self.structs.get(struct_name.as_ref()) else {
                    return Ty::Unknown;
                };
                let sig = sig.clone();
                let subst = substitution(&sig.generics, args);
                match sig.fields.iter().find(|f| f.name == name.node) {
                    Some(field) => field.ty.substitute(&subst),
                    None => {
                        let known: Vec<String> =
                            sig.fields.iter().map(|f| f.name.clone()).collect();
                        self.diagnostics.push(
                            Diagnostic::error(
                                UNKNOWN_FIELD,
                                format!("`{struct_name}` has no field `{}`", name.node),
                            )
                            .at(span)
                            .rule("A struct's fields are exactly the ones its declaration lists.")
                            .help(format!("`{struct_name}` declares {}", list(&known))),
                        );
                        Ty::Unknown
                    }
                }
            }
            Ty::MapEntry(key, value) => match name.node.as_str() {
                "key" => (**key).clone(),
                "value" => (**value).clone(),
                other => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            UNKNOWN_FIELD,
                            format!("`MapEntry` has no field `{other}`"),
                        )
                        .at(span)
                        .rule("A `MapEntry` carries exactly a `key` and a `value`.")
                        .help("write `.key` or `.value`"),
                    );
                    Ty::Unknown
                }
            },
            other => {
                self.diagnostics.push(
                    Diagnostic::error(
                        UNKNOWN_FIELD,
                        format!("`{other}` has no field `{}`", name.node),
                    )
                    .at(span)
                    .rule("Only a struct has fields.")
                    .help(format!(
                        "`{other}` is not a struct; call a method such as `{}()` instead, if one exists",
                        name.node
                    )),
                );
                Ty::Unknown
            }
        }
    }

    /// `Enum.Case` or `Enum.Case(payload...)`.
    fn enum_case(&mut self, enum_name: &str, case: &Ident, args: &[Arg], span: Span) -> Ty {
        let Some(sig) = self.enums.get(enum_name).cloned() else {
            return Ty::Unknown;
        };
        let ty = Ty::Enum(
            enum_name.into(),
            sig.generics.iter().cloned().map(Ty::Param).collect(),
        );
        let Some(found) = sig.cases.iter().find(|c| c.name == case.node) else {
            // An associated function is only reached through a call, which is
            // handled before this; a bare `Enum.name` that is not a case is a
            // case name that does not exist.
            let known: Vec<String> = sig.cases.iter().map(|c| c.name.clone()).collect();
            self.diagnostics.push(
                Diagnostic::error(
                    UNKNOWN_CASE,
                    format!("`{enum_name}` has no case `{}`", case.node),
                )
                .at(span)
                .rule("An enum's cases are exactly the ones its declaration lists.")
                .help(format!("`{enum_name}` declares {}", list(&known))),
            );
            return ty;
        };
        if found.payload.len() != args.len() {
            self.diagnostics.push(
                Diagnostic::error(
                    PAYLOAD_ARITY,
                    format!(
                        "`{enum_name}.{}` carries {} value(s), but {} were given",
                        case.node,
                        found.payload.len(),
                        args.len()
                    ),
                )
                .at(span)
                .label(found.span, "declared here")
                .rule("An enum case carries exactly the payload its declaration writes.")
                .help(if found.payload.is_empty() {
                    format!("write `{enum_name}.{}`", case.node)
                } else {
                    format!(
                        "write `{enum_name}.{}({})`",
                        case.node,
                        found
                            .payload
                            .iter()
                            .map(Ty::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }),
            );
        }
        // A generic enum's arguments are decided by the payload it is given,
        // exactly as a generic function's are decided by its arguments.
        let generic_set: BTreeSet<Rc<str>> = sig.generics.iter().cloned().collect();
        let mut subst: BTreeMap<Rc<str>, Ty> = BTreeMap::new();
        for (arg, payload) in args.iter().zip(&found.payload) {
            let hint = self.open(payload, &sig.generics, &subst);
            let expected = Expected::new(
                hint.clone(),
                found.span,
                format!("this case carries a `{hint}`"),
            );
            let found_ty = self.expr(&arg.value, Some(&expected));
            unify(payload, &found_ty, &generic_set, &mut subst);
        }
        for arg in args.iter().skip(found.payload.len()) {
            self.expr(&arg.value, None);
        }
        self.open(&ty, &sig.generics, &subst)
    }

    fn unary(&mut self, op: UnaryOp, operand: &Expr, span: Span) -> Ty {
        let ty = self.expr(operand, None);
        if ty.is_wild() {
            return ty;
        }
        match (op, &ty) {
            (UnaryOp::Not, Ty::Bool) => Ty::Bool,
            (UnaryOp::Neg, Ty::Int) => Ty::Int,
            (UnaryOp::Neg, Ty::Float) => Ty::Float,
            (UnaryOp::Neg, Ty::Duration) => Ty::Duration,
            _ => {
                let symbol = match op {
                    UnaryOp::Not => "!",
                    UnaryOp::Neg => "-",
                };
                self.diagnostics.push(
                    Diagnostic::error(OPERATOR, format!("`{symbol}` is not defined for `{ty}`"))
                        .at(span)
                        .rule("There are no implicit numeric, string, or boolean conversions.")
                        .help(match op {
                            UnaryOp::Not => {
                                "`!` negates a `Bool`; compare instead, as in `x == 0`".to_string()
                            }
                            UnaryOp::Neg => {
                                "`-` negates an `Int`, a `Float`, or a `Duration`".to_string()
                            }
                        }),
                );
                Ty::Unknown
            }
        }
    }

    fn binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr, span: Span) -> Ty {
        let left = self.expr(lhs, None);
        let right = self.expr(rhs, None);
        self.binary_result(op, &left, &right, span)
    }

    /// The type of `left op right`, mirroring the operators the runtime
    /// defines. Mixed operands are rejected: the card allows no implicit
    /// numeric, string, or boolean conversions, and this is that rule made
    /// static.
    fn binary_result(&mut self, op: BinaryOp, left: &Ty, right: &Ty, span: Span) -> Ty {
        match op {
            BinaryOp::And | BinaryOp::Or => {
                let mut ok = true;
                for ty in [left, right] {
                    if !ty.is_wild() && *ty != Ty::Bool {
                        ok = false;
                    }
                }
                if !ok {
                    self.operator_error(op, left, right, span, "`&&` and `||` combine two `Bool`s");
                }
                Ty::Bool
            }
            BinaryOp::Eq | BinaryOp::Ne => {
                if !left.matches(right) {
                    self.diagnostics.push(
                        Diagnostic::error(
                            OPERATOR,
                            format!("cannot compare `{left}` with `{right}`"),
                        )
                        .at(span)
                        .rule("`==` means value equality between values of the same type.")
                        .help(format!(
                            "convert one side explicitly so both are `{left}`, or compare values that already share a type"
                        )),
                    );
                }
                Ty::Bool
            }
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => {
                if left.is_wild() || right.is_wild() {
                    return left.join(right);
                }
                if left != right {
                    self.operator_error(
                        op,
                        left,
                        right,
                        span,
                        "arithmetic combines two values of the same type",
                    );
                    return Ty::Unknown;
                }
                match left {
                    Ty::Int | Ty::Float => left.clone(),
                    Ty::Duration if matches!(op, BinaryOp::Add | BinaryOp::Sub) => Ty::Duration,
                    Ty::Str if op == BinaryOp::Add => {
                        self.diagnostics.push(
                            Diagnostic::error(OPERATOR, "`+` is not defined for `String`")
                                .at(span)
                                .rule("There are no implicit string conversions.")
                                .help("use string interpolation, such as \"{left}{right}\""),
                        );
                        Ty::Unknown
                    }
                    _ => {
                        self.operator_error(
                            op,
                            left,
                            right,
                            span,
                            "arithmetic is defined for `Int`, `Float`, and (for `+` and `-`) `Duration`",
                        );
                        Ty::Unknown
                    }
                }
            }
            BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
                if left.is_wild() || right.is_wild() {
                    return Ty::Bool;
                }
                if left != right {
                    self.operator_error(
                        op,
                        left,
                        right,
                        span,
                        "an ordering compares two values of the same type",
                    );
                } else if !matches!(left, Ty::Int | Ty::Float | Ty::Duration | Ty::Str) {
                    self.operator_error(
                        op,
                        left,
                        right,
                        span,
                        "`<`, `<=`, `>`, and `>=` are defined for `Int`, `Float`, `Duration`, and `String`",
                    );
                }
                Ty::Bool
            }
        }
    }

    fn operator_error(&mut self, op: BinaryOp, left: &Ty, right: &Ty, span: Span, help: &str) {
        let symbol = operator_symbol(op);
        self.diagnostics.push(
            Diagnostic::error(
                OPERATOR,
                format!("`{symbol}` is not defined for `{left}` and `{right}`"),
            )
            .at(span)
            .rule("There are no implicit numeric, string, or boolean conversions.")
            .help(help.to_string()),
        );
    }

    fn assign(&mut self, op: Option<BinaryOp>, target: &Expr, value: &Expr, span: Span) -> Ty {
        if !matches!(target.kind, ExprKind::Ident(_) | ExprKind::Field { .. }) {
            self.diagnostics.push(
                Diagnostic::error(
                    NOT_A_PLACE,
                    "this expression is not a place, so it cannot be assigned",
                )
                .at(target.span)
                .rule("Only a binding or a field of one is a place.")
                .help("assign to a `var` binding, or to a field of one"),
            );
            self.expr(value, None);
            return Ty::Unit;
        }
        let target_ty = self.expr(target, None);
        match op {
            None => {
                let expected = Expected::new(
                    target_ty.clone(),
                    target.span,
                    format!("the assigned place is `{target_ty}`"),
                );
                self.expr(value, Some(&expected));
            }
            Some(op) => {
                let value_ty = self.expr(value, None);
                let result = self.binary_result(op, &target_ty, &value_ty, span);
                let expected = Expected::new(
                    target_ty.clone(),
                    target.span,
                    format!("the assigned place is `{target_ty}`"),
                );
                self.expect(&result, &expected, span);
            }
        }
        Ty::Unit
    }

    /// `expr?`, which returns the failure from the current function.
    fn try_expr(&mut self, inner: &Expr, span: Span) -> Ty {
        let ty = self.expr(inner, None);
        match &ty {
            Ty::Unknown | Ty::Never => Ty::Unknown,
            Ty::Result(ok, error) => {
                let (ok, error) = ((**ok).clone(), (**error).clone());
                match self.ret.clone() {
                    Ty::Unknown => {}
                    Ty::Result(_, ret_error) if error.matches(&ret_error) => {}
                    Ty::Result(_, ret_error) => self.diagnostics.push(
                        Diagnostic::error(
                            TRY_RETURN,
                            format!(
                                "`?` propagates `{error}`, but this function returns `{ret_error}` as its failure"
                            ),
                        )
                        .at(span)
                        .label(self.ret_span, format!("the declared failure type is `{ret_error}`"))
                        .rule("`expr?` returns the error from the current function, so the two failure types must be the same.")
                        .help(format!(
                            "map the failure first, as in `expr.mapError {{ ... }}?`, or declare this function `-> Result<_, {error}>`"
                        )),
                    ),
                    other => self.diagnostics.push(
                        Diagnostic::error(
                            TRY_RETURN,
                            format!("`?` needs a function that returns a `Result`, but this one returns `{other}`"),
                        )
                        .at(span)
                        .label(self.ret_span, format!("the declared return type is `{other}`"))
                        .rule("`expr?` returns the error from the current function.")
                        .help(format!("declare this function `-> Result<{other}, {error}>`")),
                    ),
                }
                ok
            }
            Ty::Option(inner_ty) => {
                let inner_ty = (**inner_ty).clone();
                match self.ret.clone() {
                    Ty::Unknown | Ty::Option(_) => {}
                    other => self.diagnostics.push(
                        Diagnostic::error(
                            TRY_RETURN,
                            format!("`?` on an `Option` needs a function that returns an `Option`, but this one returns `{other}`"),
                        )
                        .at(span)
                        .label(self.ret_span, format!("the declared return type is `{other}`"))
                        .rule("`expr?` returns the missing value from the current function.")
                        .help(format!("declare this function `-> Option<{other}>`, or handle the `None` with `unwrapOr`")),
                    ),
                }
                inner_ty
            }
            Ty::Task(inner_ty) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        TRY_OPERAND,
                        format!(
                            "`?` needs a `Result` or an `Option`, but found `Task<{inner_ty}>`"
                        ),
                    )
                    .at(span)
                    .rule("`expr?` returns the error from the current function.")
                    .help("settle the task first, as in `task.await()?`"),
                );
                Ty::Unknown
            }
            other => {
                self.diagnostics.push(
                    Diagnostic::error(
                        TRY_OPERAND,
                        format!("`?` needs a `Result` or an `Option`, but found `{other}`"),
                    )
                    .at(span)
                    .rule("`expr?` returns the error from the current function.")
                    .help(format!("`{other}` cannot fail, so drop the `?`")),
                );
                Ty::Unknown
            }
        }
    }

    fn await_expr(&mut self, inner: &Expr, span: Span) -> Ty {
        let ty = self.expr(inner, None);
        match &ty {
            Ty::Unknown | Ty::Never => Ty::Unknown,
            Ty::Task(inner_ty) => (**inner_ty).clone(),
            other => {
                self.diagnostics.push(
                    Diagnostic::error(
                        AWAIT_OPERAND,
                        format!("`await` needs a task, but found `{other}`"),
                    )
                    .at(span)
                    .rule("`await` settles a task. Only a task spawned into a scope, or one returned by an `async fn`, has a value to settle.")
                    .help("call an `async fn`, or spawn the work into a task scope, and await that handle"),
                );
                Ty::Unknown
            }
        }
    }

    fn condition(&mut self, condition: &Expr) -> Ty {
        let ty = self.expr(condition, None);
        if !ty.matches(&Ty::Bool) {
            self.diagnostics.push(
                Diagnostic::error(
                    CONDITION,
                    format!("a condition must be a `Bool`, but found `{ty}`"),
                )
                .at(condition.span)
                .rule("There are no implicit boolean conversions.")
                .help(condition_help(&ty)),
            );
        }
        Ty::Bool
    }

    /// An `if` with an `else` is an expression whose branches must agree.
    ///
    /// An `if` with no `else` is a statement: its type is `()` and the value
    /// of its branch is discarded, because there is no second branch to give
    /// the missing case a value.
    fn if_expr(
        &mut self,
        condition: &Expr,
        then_branch: &Block,
        else_branch: Option<&Expr>,
        span: Span,
        expected: Option<&Expected>,
    ) -> Ty {
        self.condition(condition);
        let Some(else_branch) = else_branch else {
            self.block(then_branch, None);
            if let Some(expected) = expected {
                self.expect(&Ty::Unit, expected, span);
            }
            return Ty::Unit;
        };
        let then_ty = self.block(then_branch, expected);
        let else_ty = self.expr(else_branch, expected);
        // With an expectation, both branches were already checked against it
        // and a disagreement was reported there; without one, the branches
        // are only answerable to each other.
        if expected.is_none() && !then_ty.matches(&else_ty) {
            self.branches_disagree(then_branch.span, else_branch.span, &then_ty, &else_ty);
        }
        then_ty.join(&else_ty)
    }

    fn branches_disagree(&mut self, first: Span, second: Span, first_ty: &Ty, second_ty: &Ty) {
        self.diagnostics.push(
            Diagnostic::error(
                BRANCHES,
                format!("this branch produces `{second_ty}`, but the other produces `{first_ty}`"),
            )
            .at(second)
            .label(first, format!("this branch produces `{first_ty}`"))
            .rule(
                "Every branch of an `if` or `match` used as an expression produces the same type.",
            )
            .help(format!(
                "make both branches produce `{first_ty}`, or bind them separately"
            )),
        );
    }

    fn match_expr(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
        expected: Option<&Expected>,
    ) -> Ty {
        let scrutinee_ty = self.expr(scrutinee, None);
        let mut result: Option<(Ty, Span)> = None;
        for arm in arms {
            self.scopes.push(BTreeMap::new());
            self.pattern(&arm.pattern, &scrutinee_ty);
            let ty = self.expr(&arm.body, expected);
            self.scopes.pop();
            result = Some(match result {
                None => (ty, arm.body.span),
                Some((previous, previous_span)) => {
                    if expected.is_none() && !previous.matches(&ty) {
                        self.branches_disagree(previous_span, arm.body.span, &previous, &ty);
                    }
                    (previous.join(&ty), previous_span)
                }
            });
        }
        let _ = span;
        match result {
            Some((ty, _)) => ty,
            // A `match` with no arms produces nothing; resolution already
            // reports it as non-exhaustive.
            None => Ty::Never,
        }
    }

    /// Checks a pattern against the scrutinee's type and binds its names.
    ///
    /// Case names and exhaustiveness belong to resolution, which reports them
    /// from the arms alone; this adds what only a type can say — that the
    /// pattern's enum is the scrutinee's enum, and that a payload has the
    /// arity and types the case declares.
    fn pattern(&mut self, pattern: &Pattern, scrutinee: &Ty) {
        match &pattern.kind {
            PatternKind::Wildcard => {}
            PatternKind::Binding(name) => self.declare(name, scrutinee.clone()),
            PatternKind::Literal(expr) => {
                let ty = self.expr(expr, None);
                if !ty.matches(scrutinee) {
                    self.diagnostics.push(
                        Diagnostic::error(
                            PATTERN,
                            format!(
                                "this pattern matches `{ty}`, but the scrutinee is `{scrutinee}`"
                            ),
                        )
                        .at(pattern.span)
                        .rule("A pattern matches values of the scrutinee's type.")
                        .help(format!(
                            "write a `{scrutinee}` literal, or a binding such as `other`"
                        )),
                    );
                }
            }
            PatternKind::Variant { path, payload } => {
                self.variant_pattern(pattern.span, path, payload, scrutinee)
            }
        }
    }

    fn variant_pattern(&mut self, span: Span, path: &[Ident], payload: &[Pattern], scrutinee: &Ty) {
        let case = path.last().expect("a variant path is never empty");
        let payload_types: Option<Vec<Ty>> = match scrutinee {
            Ty::Unknown | Ty::Never => None,
            Ty::Option(inner) => match case.node.as_str() {
                "Some" => Some(vec![(**inner).clone()]),
                "None" => Some(Vec::new()),
                _ => None,
            },
            Ty::Result(ok, error) => match case.node.as_str() {
                "Ok" => Some(vec![(**ok).clone()]),
                "Err" => Some(vec![(**error).clone()]),
                _ => None,
            },
            Ty::Enum(name, args) => {
                if let [qualifier, _] = path {
                    if qualifier.node != **name {
                        self.diagnostics.push(
                            Diagnostic::error(
                                PATTERN,
                                format!(
                                    "this pattern matches `{}`, but the scrutinee is `{name}`",
                                    qualifier.node
                                ),
                            )
                            .at(span)
                            .rule("A pattern matches values of the scrutinee's type.")
                            .help(format!(
                                "write a `{name}` case, such as `{name}.{}`",
                                first_case_of(self.enums.get(name.as_ref()))
                            )),
                        );
                        None
                    } else {
                        self.case_payload(name, &case.node, args)
                    }
                } else {
                    self.case_payload(name, &case.node, args)
                }
            }
            other => {
                self.diagnostics.push(
                    Diagnostic::error(
                        PATTERN,
                        format!(
                            "`{other}` has no cases, so it cannot be matched by `{}`",
                            case.node
                        ),
                    )
                    .at(span)
                    .rule("A variant pattern matches an enum case.")
                    .help(format!(
                        "match a literal `{other}`, or bind the value with a name"
                    )),
                );
                None
            }
        };

        let Some(types) = payload_types else {
            for sub in payload {
                self.pattern(sub, &Ty::Unknown);
            }
            return;
        };
        if types.len() != payload.len() {
            self.diagnostics.push(
                Diagnostic::error(
                    PAYLOAD_ARITY,
                    format!(
                        "`{}` carries {} value(s), but this pattern binds {}",
                        case.node,
                        types.len(),
                        payload.len()
                    ),
                )
                .at(span)
                .rule("A pattern binds exactly the payload its case declares.")
                .help(if types.is_empty() {
                    format!("write `{}`", case.node)
                } else {
                    format!(
                        "write `{}({})`",
                        case.node,
                        types.iter().map(|_| "value").collect::<Vec<_>>().join(", ")
                    )
                }),
            );
        }
        for (sub, ty) in payload.iter().zip(types.iter()) {
            self.pattern(sub, ty);
        }
        for sub in payload.iter().skip(types.len()) {
            self.pattern(sub, &Ty::Unknown);
        }
    }

    /// The payload types of `case` on the enum `name`, substituted with the
    /// scrutinee's type arguments, or `None` when the enum has no such case —
    /// resolution reports that.
    fn case_payload(&mut self, name: &str, case: &str, args: &[Ty]) -> Option<Vec<Ty>> {
        let sig = self.enums.get(name)?;
        let subst = substitution(&sig.generics, args);
        let found = sig.cases.iter().find(|c| c.name == case)?;
        Some(
            found
                .payload
                .iter()
                .map(|ty| ty.substitute(&subst))
                .collect(),
        )
    }

    fn for_expr(&mut self, binding: &Ident, iterable: &Expr, body: &Block) -> Ty {
        let ty = self.expr(iterable, None);
        let element = match &ty {
            Ty::Unknown | Ty::Never => Ty::Unknown,
            Ty::Array(inner) | Ty::Vector(inner) | Ty::Set(inner) => (**inner).clone(),
            Ty::Range => Ty::Int,
            // A `Map` iterates in ascending key order, binding each pair as
            // the same `MapEntry` shape `Map.of` accepts.
            Ty::Map(key, value) => Ty::MapEntry(key.clone(), value.clone()),
            other => {
                self.diagnostics.push(
                    Diagnostic::error(
                        ITERABLE,
                        format!(
                            "`for` iterates an `Array`, a `Vector`, a `Range`, a `Set`, or a `Map`, but found `{other}`"
                        ),
                    )
                    .at(iterable.span)
                    .rule("`for` iterates a sequence; iteration order is defined by each collection type.")
                    .help(iterable_help(other)),
                );
                Ty::Unknown
            }
        };
        self.scopes.push(BTreeMap::new());
        self.declare(&binding.node, element);
        self.block(body, None);
        self.scopes.pop();
        Ty::Unit
    }

    /// A lambda takes its parameter types from the expected type at the call
    /// site, as ADR 0004 decides; a parameter it writes for itself is used as
    /// written.
    fn lambda(
        &mut self,
        is_async: bool,
        params: &[Param],
        body: &Block,
        span: Span,
        expected: Option<&Expected>,
    ) -> Ty {
        let hint = match expected.map(|e| &e.ty) {
            Some(Ty::Fn(func)) => Some(func.clone()),
            _ => None,
        };
        if let Some(func) = &hint {
            if func.params.len() != params.len() {
                self.diagnostics.push(
                    Diagnostic::error(
                        ARITY,
                        format!(
                            "this function takes {} parameter(s), but {} were expected here",
                            params.len(),
                            func.params.len()
                        ),
                    )
                    .at(span)
                    .rule("A function value has exactly the parameters the place that holds it declares.")
                    .help(format!("write `fn({}) {{ ... }}`", (0..func.params.len()).map(|i| format!("p{i}")).collect::<Vec<_>>().join(", "))),
                );
            }
        }

        let mut param_types = Vec::with_capacity(params.len());
        self.scopes.push(BTreeMap::new());
        for (index, param) in params.iter().enumerate() {
            let ty = match &param.ty {
                Some(written) => self.resolve(written),
                None => hint
                    .as_ref()
                    .and_then(|f| f.params.get(index))
                    .cloned()
                    .unwrap_or(Ty::Unknown),
            };
            param_types.push(ty.clone());
            self.declare(&param.name.node, ty);
        }

        // The expected type decides the result only when it has one to give;
        // otherwise the body does.
        let declared_ret = hint
            .as_ref()
            .map(|f| f.ret.clone())
            .filter(|ty| !ty.is_wild());
        let outer_ret =
            std::mem::replace(&mut self.ret, declared_ret.clone().unwrap_or(Ty::Unknown));
        let outer_span = std::mem::replace(&mut self.ret_span, span);
        let expected_body = declared_ret.clone().map(|ty| {
            let label = format!("this function value produces `{ty}`");
            Expected::new(ty, span, label)
        });
        let body_ty = self.block(body, expected_body.as_ref());
        self.ret = outer_ret;
        self.ret_span = outer_span;
        self.scopes.pop();

        Ty::func(is_async, param_types, declared_ret.unwrap_or(body_ty))
    }

    // -------------------------------------------------------------- calls

    /// A call, resolved the way the interpreter resolves one: a local
    /// binding, then a declaration of this module, then a host item, then a
    /// builtin.
    fn call(
        &mut self,
        callee: &Expr,
        generics: &[Type],
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
        expected: Option<&Expected>,
    ) -> Ty {
        match &callee.kind {
            ExprKind::Ident(name) if self.lookup(name).is_none() => {
                self.call_named(name, generics, args, trailing, span, callee.span, expected)
            }
            ExprKind::Field { base, name } => {
                if let ExprKind::Ident(head) = &base.kind {
                    if self.lookup(head).is_none() {
                        if let Some(ty) = self.call_qualified(head, name, args, trailing, span) {
                            return ty;
                        }
                    }
                }
                let receiver = self.expr(base, None);
                self.method_call(&receiver, name, args, trailing, span)
            }
            _ => {
                let callee_ty = self.expr(callee, None);
                self.call_value(&callee_ty, args, trailing, span, callee.span)
            }
        }
    }

    /// `name(...)` where `name` is not a local binding.
    #[allow(clippy::too_many_arguments)]
    fn call_named(
        &mut self,
        name: &str,
        generics: &[Type],
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
        callee_span: Span,
        expected: Option<&Expected>,
    ) -> Ty {
        if let Some(sig) = self.functions.get(name).cloned() {
            let explicit = generics.iter().map(|ty| self.resolve(ty)).collect();
            return self.call_signature(&sig, &format!("`{name}`"), explicit, args, trailing, span);
        }
        if let Some(sig) = self.structs.get(name).cloned() {
            return self.struct_init(name, &sig, args, trailing, span);
        }
        if self.module.enums.contains_key(name) {
            let cases = first_case_of(self.enums.get(name));
            self.diagnostics.push(
                Diagnostic::error(NOT_CALLABLE, format!("`{name}` is an enum, not a function"))
                    .at(callee_span)
                    .rule("An enum value is one of its cases; the enum itself is not callable.")
                    .help(format!("name a case, such as `{name}.{cases}`")),
            );
            self.check_args_freely(args, trailing);
            return Ty::Unknown;
        }
        if self.module.host_items.contains_key(name) {
            self.check_args_freely(args, trailing);
            return Ty::Unknown;
        }
        if name == "MapEntry" {
            return self.map_entry(args, trailing, span);
        }
        if let Some(ty) = self.constructor(name, args, trailing, span, expected) {
            return ty;
        }
        if name == "None" {
            self.diagnostics.push(
                Diagnostic::error(NOT_CALLABLE, "`None` is a value, not a call")
                    .at(callee_span)
                    .rule("`None` is the empty case of `Option`, which carries nothing.")
                    .help("write `None`"),
            );
            self.check_args_freely(args, trailing);
            return Ty::Option(Box::new(Ty::Unknown));
        }
        self.check_args_freely(args, trailing);
        self.unresolved_name(name, callee_span)
    }

    /// `Ok(v)`, `Err(e)`, `Some(v)`, and `Error("message")`.
    fn constructor(
        &mut self,
        name: &str,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
        expected: Option<&Expected>,
    ) -> Option<Ty> {
        let hint = expected.map(|e| &e.ty);
        let (params, ret): (Vec<Ty>, Ty) = match name {
            "Ok" => {
                let (ok, error) = match hint {
                    Some(Ty::Result(ok, error)) => ((**ok).clone(), (**error).clone()),
                    _ => (Ty::Unknown, Ty::Unknown),
                };
                (vec![ok.clone()], Ty::Result(Box::new(ok), Box::new(error)))
            }
            "Err" => {
                let (ok, error) = match hint {
                    Some(Ty::Result(ok, error)) => ((**ok).clone(), (**error).clone()),
                    _ => (Ty::Unknown, Ty::Unknown),
                };
                (
                    vec![error.clone()],
                    Ty::Result(Box::new(ok), Box::new(error)),
                )
            }
            "Some" => {
                let inner = match hint {
                    Some(Ty::Option(inner)) => (**inner).clone(),
                    _ => Ty::Unknown,
                };
                (vec![inner.clone()], Ty::Option(Box::new(inner)))
            }
            "Error" => (vec![Ty::Str], Ty::Error),
            _ => return None,
        };
        let mut supplied: Vec<&Expr> = args.iter().map(|arg| &arg.value).collect();
        if let Some(trailing) = trailing {
            supplied.push(trailing);
        }
        if supplied.len() != 1 {
            self.diagnostics.push(
                Diagnostic::error(
                    ARITY,
                    format!(
                        "`{name}` takes 1 argument, but {} were given",
                        supplied.len()
                    ),
                )
                .at(span)
                .rule("A constructor carries exactly one value.")
                .help(format!("write `{name}(value)`")),
            );
        }
        let mut ret = ret;
        for (index, value) in supplied.iter().enumerate() {
            match params.get(index) {
                Some(param) if !param.is_wild() => {
                    let expected =
                        Expected::new(param.clone(), span, format!("`{name}` carries a `{param}`"));
                    self.expr(value, Some(&expected));
                }
                _ => {
                    // Nothing constrains the payload, so it decides the
                    // constructed type instead.
                    let ty = self.expr(value, None);
                    if index == 0 {
                        ret = match (name, ret) {
                            ("Ok", Ty::Result(_, error)) => Ty::Result(Box::new(ty), error),
                            ("Err", Ty::Result(ok, _)) => Ty::Result(ok, Box::new(ty)),
                            ("Some", Ty::Option(_)) => Ty::Option(Box::new(ty)),
                            (_, ret) => ret,
                        };
                    }
                }
            }
        }
        Some(ret)
    }

    /// `head.name(...)` where `head` is not a local binding: a host
    /// operation, an enum case, an associated function, or a method reached
    /// through its type's name.
    fn call_qualified(
        &mut self,
        head: &str,
        name: &Ident,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Option<Ty> {
        if self.module.host_uses.contains(head) {
            self.check_args_freely(args, trailing);
            return Some(Ty::Unknown);
        }
        if self.module.enums.contains_key(head) {
            let is_case = self
                .enums
                .get(head)
                .is_some_and(|sig| sig.cases.iter().any(|c| c.name == name.node));
            if !is_case {
                if let Some(sig) = self
                    .methods
                    .get(&(head.to_string(), name.node.clone()))
                    .cloned()
                {
                    self.check_receiver(&sig, head, &name.node, span, false);
                    return Some(self.call_signature(
                        &sig,
                        &format!("`{head}.{}`", name.node),
                        Vec::new(),
                        args,
                        trailing,
                        span,
                    ));
                }
            }
            return Some(self.enum_case(head, name, args, span));
        }
        if self.module.structs.contains_key(head) {
            if let Some(sig) = self
                .methods
                .get(&(head.to_string(), name.node.clone()))
                .cloned()
            {
                self.check_receiver(&sig, head, &name.node, span, false);
                return Some(self.call_signature(
                    &sig,
                    &format!("`{head}.{}`", name.node),
                    Vec::new(),
                    args,
                    trailing,
                    span,
                ));
            }
            let known = self.known_members(head);
            self.diagnostics.push(
                Diagnostic::error(
                    UNKNOWN_ASSOCIATED,
                    format!("`{head}` has no associated function `{}`", name.node),
                )
                .at(span)
                .rule("An associated function is declared in the type's `impl` block.")
                .help(format!("`{head}` declares {known}")),
            );
            self.check_args_freely(args, trailing);
            return Some(Ty::Unknown);
        }
        if is_builtin_type(head) {
            return Some(self.builtin_associated(head, name, args, trailing, span));
        }
        None
    }

    /// `Vector.of(...)`, `Map.of(...)`, `Set.of(...)`, `Int.parse(...)`.
    ///
    /// This table is the static half of
    /// `cove_runtime::builtins::call_associated`: every associated function
    /// the runtime dispatches appears here with a type, and nothing else
    /// does. A spread argument is left to the runtime, which rejects one in
    /// any of these calls.
    fn builtin_associated(
        &mut self,
        type_name: &str,
        name: &Ident,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        let element = |name: &'static str, param: Ty, ret: Ty, generics: Vec<Rc<str>>| BuiltinSig {
            generics,
            params: vec![(name, param)],
            variadic: true,
            ret,
        };
        let sig = match (type_name, name.node.as_str()) {
            ("Vector", "of") => element(
                "items",
                Ty::Param("T".into()),
                Ty::Vector(Box::new(Ty::Param("T".into()))),
                vec!["T".into()],
            ),
            // `Map.of` collects the pairs `MapEntry(key:, value:)` builds.
            ("Map", "of") => element(
                "entries",
                Ty::MapEntry(
                    Box::new(Ty::Param("K".into())),
                    Box::new(Ty::Param("V".into())),
                ),
                Ty::Map(
                    Box::new(Ty::Param("K".into())),
                    Box::new(Ty::Param("V".into())),
                ),
                vec!["K".into(), "V".into()],
            ),
            ("Set", "of") => element(
                "items",
                Ty::Param("T".into()),
                Ty::Set(Box::new(Ty::Param("T".into()))),
                vec!["T".into()],
            ),
            ("Int", "parse") => BuiltinSig {
                generics: Vec::new(),
                params: vec![("text", Ty::Str)],
                variadic: false,
                ret: Ty::Result(Box::new(Ty::Int), Box::new(Ty::Error)),
            },
            _ => {
                self.diagnostics.push(
                    Diagnostic::error(
                        UNKNOWN_ASSOCIATED,
                        format!("`{type_name}` has no associated function `{}`", name.node),
                    )
                    .at(span)
                    .rule(
                        "A builtin type's associated functions are `Vector.of`, `Map.of`, `Set.of`, and `Int.parse`.",
                    )
                    .help(format!(
                        "`{type_name}` has no `{}`; construct the value another way",
                        name.node
                    )),
                );
                self.check_args_freely(args, trailing);
                return Ty::Unknown;
            }
        };
        let what = format!("`{type_name}.{}`", name.node);
        self.call_builtin(&sig, &what, args, trailing, span)
    }

    /// `MapEntry(key: ..., value: ...)`, the one builtin struct: a
    /// synthesized labeled call, exactly like a declared struct's
    /// initializer, that exists so `Map.of` has a call-shaped way to write
    /// the pairs it collects.
    fn map_entry(&mut self, args: &[Arg], trailing: Option<&Expr>, span: Span) -> Ty {
        let sig = BuiltinSig {
            generics: vec!["K".into(), "V".into()],
            params: vec![
                ("key", Ty::Param("K".into())),
                ("value", Ty::Param("V".into())),
            ],
            variadic: false,
            ret: Ty::MapEntry(
                Box::new(Ty::Param("K".into())),
                Box::new(Ty::Param("V".into())),
            ),
        };
        self.call_builtin(&sig, "`MapEntry`", args, trailing, span)
    }

    /// `Type(field: value, ...)`, the synthesized labeled call the card
    /// describes.
    fn struct_init(
        &mut self,
        name: &str,
        sig: &StructSig,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        let generics: Vec<Rc<str>> = sig.generics.clone();
        let subst = self.match_arguments(
            &sig.fields,
            &generics,
            BTreeMap::new(),
            args,
            trailing,
            span,
            &format!("`{name}`"),
            "the field",
        );
        Ty::Struct(
            name.into(),
            generics
                .iter()
                .map(|g| subst.get(g).cloned().unwrap_or(Ty::Unknown))
                .collect(),
        )
    }

    /// Checks a call against a declared signature, unifying the callee's type
    /// parameters at the call site and substituting them into the result.
    fn call_signature(
        &mut self,
        sig: &FnSig,
        what: &str,
        explicit: Vec<Ty>,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        let mut subst: BTreeMap<Rc<str>, Ty> = BTreeMap::new();
        for (param, ty) in sig.generics.iter().zip(explicit) {
            subst.insert(param.clone(), ty);
        }
        let subst = self.match_arguments(
            &sig.params,
            &sig.generics,
            subst,
            args,
            trailing,
            span,
            what,
            "the parameter",
        );
        let ret = self.open(&sig.ret, &sig.generics, &subst);
        if sig.is_async {
            // An `async fn` is called like any other function and produces a
            // task; its value is reachable only through `await`.
            Ty::Task(Box::new(ret))
        } else {
            ret
        }
    }

    /// Calls a value of function type.
    fn call_value(
        &mut self,
        callee: &Ty,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
        callee_span: Span,
    ) -> Ty {
        match callee {
            Ty::Unknown | Ty::Never => {
                self.check_args_freely(args, trailing);
                Ty::Unknown
            }
            Ty::Fn(func) => {
                let params: Vec<ParamSig> = func
                    .params
                    .iter()
                    .enumerate()
                    .map(|(index, ty)| ParamSig {
                        name: format!("#{index}"),
                        ty: ty.clone(),
                        variadic: false,
                        has_default: false,
                        span: callee_span,
                    })
                    .collect();
                self.match_arguments(
                    &params,
                    &[],
                    BTreeMap::new(),
                    args,
                    trailing,
                    span,
                    "this function value",
                    "the parameter",
                );
                if func.is_async {
                    Ty::Task(Box::new(func.ret.clone()))
                } else {
                    func.ret.clone()
                }
            }
            other => {
                self.diagnostics.push(
                    Diagnostic::error(NOT_CALLABLE, format!("`{other}` is not a function"))
                        .at(callee_span)
                        .rule("Only a function value can be called.")
                        .help(format!(
                            "`{other}` is a value, not a function; remove the argument list"
                        )),
                );
                self.check_args_freely(args, trailing);
                Ty::Unknown
            }
        }
    }

    /// Matches call-site arguments to parameters by position and by label,
    /// checking each against the parameter's type and binding the callee's
    /// type parameters as it goes.
    ///
    /// Labels are parameter names, so a labeled argument goes to the
    /// parameter it names. Their *order* is the runtime's rule, not this
    /// one's.
    #[allow(clippy::too_many_arguments)]
    fn match_arguments(
        &mut self,
        params: &[ParamSig],
        generics: &[Rc<str>],
        mut subst: BTreeMap<Rc<str>, Ty>,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
        what: &str,
        role: &str,
    ) -> BTreeMap<Rc<str>, Ty> {
        let variadic_last = params.last().is_some_and(|p| p.variadic);
        let mut slots: Vec<Option<&Arg>> = vec![None; params.len()];
        let mut rest: Vec<&Arg> = Vec::new();
        let mut next = 0usize;
        let mut labeled = false;
        // One mistake, one diagnostic: a label that names no parameter has
        // already been reported, so the parameter it failed to fill is not
        // reported as missing too.
        let mut mislabeled = false;
        let generic_set: BTreeSet<Rc<str>> = generics.iter().cloned().collect();

        for arg in args {
            match &arg.label {
                Some(label) => {
                    labeled = true;
                    match params.iter().position(|p| p.name == label.node) {
                        Some(index) => {
                            slots[index] = Some(arg);
                            next = index + 1;
                        }
                        None => {
                            let known: Vec<String> =
                                params.iter().map(|p| p.name.clone()).collect();
                            self.diagnostics.push(
                                Diagnostic::error(
                                    UNKNOWN_LABEL,
                                    format!("{what} has no parameter labeled `{}`", label.node),
                                )
                                .at(arg.span)
                                .rule("Argument labels are parameter names and part of the API contract.")
                                .help(format!("known labels: {}", list(&known))),
                            );
                            self.expr(&arg.value, None);
                            mislabeled = true;
                        }
                    }
                }
                // The runtime rejects a positional argument after a labeled
                // one; there is no parameter to check this one against.
                None if labeled => {
                    self.expr(&arg.value, None);
                }
                None if variadic_last && next + 1 >= params.len() => rest.push(arg),
                None if next < params.len() => {
                    slots[next] = Some(arg);
                    next += 1;
                }
                None => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            ARITY,
                            format!(
                                "{what} takes {} argument(s), but more were given",
                                params.len()
                            ),
                        )
                        .at(arg.span)
                        .rule("A call passes exactly the arguments the declaration binds.")
                        .help(format!(
                            "{what} declares {}",
                            list(&params.iter().map(|p| p.name.clone()).collect::<Vec<_>>())
                        )),
                    );
                    self.expr(&arg.value, None);
                }
            }
        }

        // A trailing closure fills the first parameter still empty, which is
        // the variadic one when the signature ends in `...`.
        let trailing_slot = trailing.and_then(|_| {
            if variadic_last {
                None
            } else {
                slots.iter().position(Option::is_none)
            }
        });
        if let Some(trailing) = trailing {
            match trailing_slot.and_then(|index| params.get(index).map(|p| (index, p))) {
                Some((index, param)) => {
                    let param = param.clone();
                    let expected = param.ty.substitute(&subst);
                    let hint = self.open(&expected, generics, &subst);
                    let found = self.trailing_type(trailing, Some(&hint));
                    self.check_argument(
                        &found,
                        &hint,
                        &expected,
                        trailing.span,
                        &param,
                        &generic_set,
                        &mut subst,
                        role,
                    );
                    slots[index] = None;
                }
                None => {
                    let ty = self.trailing_type(trailing, None);
                    if variadic_last {
                        if let Some(param) = params.last() {
                            let param = param.clone();
                            let element = param.ty.substitute(&subst);
                            let hint = self.open(&element, generics, &subst);
                            self.check_argument(
                                &ty,
                                &hint,
                                &element,
                                trailing.span,
                                &param,
                                &generic_set,
                                &mut subst,
                                role,
                            );
                        }
                    } else {
                        self.diagnostics.push(
                            Diagnostic::error(
                                ARITY,
                                format!(
                                    "{what} takes {} argument(s), but a trailing closure was given too",
                                    params.len()
                                ),
                            )
                            .at(trailing.span)
                            .rule("A trailing closure is the call's last argument.")
                            .help("remove the trailing closure, or pass it in place of an argument"),
                        );
                    }
                }
            }
        }

        for (index, param) in params.iter().enumerate() {
            if param.variadic {
                let element = param.ty.substitute(&subst);
                let mut supplied: Vec<&Arg> = rest.clone();
                if let Some(arg) = slots[index] {
                    supplied.insert(0, arg);
                }
                for arg in supplied {
                    self.variadic_argument(
                        arg,
                        &element,
                        param,
                        generics,
                        &generic_set,
                        &mut subst,
                        role,
                    );
                }
                continue;
            }
            let Some(arg) = slots[index] else {
                if !param.has_default && trailing_slot != Some(index) && !mislabeled {
                    self.diagnostics.push(
                        Diagnostic::error(
                            MISSING_ARGUMENT,
                            format!("{what} needs {role} `{}`", param.name),
                        )
                        .at(span)
                        .label(param.span, format!("`{}` is `{}`", param.name, param.ty))
                        .rule("A call passes every parameter that has no default.")
                        .help(format!("pass `{}: <{}>`", param.name, param.ty)),
                    );
                }
                continue;
            };
            let expected = param.ty.substitute(&subst);
            let hint = self.open(&expected, generics, &subst);
            let hint_expected = Expected::new(
                hint.clone(),
                param.span,
                format!("{role} `{}` is `{}`", param.name, param.ty),
            );
            let found = self.expr(&arg.value, Some(&hint_expected));
            self.check_argument(
                &found,
                &hint,
                &expected,
                arg.span,
                param,
                &generic_set,
                &mut subst,
                role,
            );
        }
        subst
    }

    /// Binds the callee's type parameters from one argument, and reports the
    /// mismatch the expectation could not: an expectation is checked against
    /// `hint`, in which every unbound type parameter is `Unknown`, so only
    /// unification can tell that two uses of the same parameter disagree.
    #[allow(clippy::too_many_arguments)]
    fn check_argument(
        &mut self,
        found: &Ty,
        hint: &Ty,
        expected: &Ty,
        span: Span,
        param: &ParamSig,
        generics: &BTreeSet<Rc<str>>,
        subst: &mut BTreeMap<Rc<str>, Ty>,
        role: &str,
    ) {
        let unified = unify(expected, found, generics, subst);
        if !unified && found.matches(hint) {
            let expected = expected.substitute(subst);
            self.report_argument(found, &expected, span, param, role);
        }
    }

    /// A signature type with every type parameter still unbound replaced by
    /// `Unknown`, so it can be used as an expectation without pretending the
    /// call site has decided what the parameter is.
    fn open(&self, ty: &Ty, generics: &[Rc<str>], subst: &BTreeMap<Rc<str>, Ty>) -> Ty {
        if generics.is_empty() {
            return ty.clone();
        }
        let map: BTreeMap<Rc<str>, Ty> = generics
            .iter()
            .map(|g| (g.clone(), subst.get(g).cloned().unwrap_or(Ty::Unknown)))
            .collect();
        ty.substitute(&map)
    }

    /// One argument passed to a variadic parameter, which is an `Array<T>`
    /// inside the callee: each ordinary argument is a `T`, and a spread
    /// argument is a sequence of them.
    #[allow(clippy::too_many_arguments)]
    fn variadic_argument(
        &mut self,
        arg: &Arg,
        element: &Ty,
        param: &ParamSig,
        generics: &[Rc<str>],
        generic_set: &BTreeSet<Rc<str>>,
        subst: &mut BTreeMap<Rc<str>, Ty>,
        role: &str,
    ) {
        if arg.spread {
            let ty = self.expr(&arg.value, None);
            let spread_element = match &ty {
                Ty::Array(inner) | Ty::Vector(inner) => (**inner).clone(),
                Ty::Unknown | Ty::Never => Ty::Unknown,
                other => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            MISMATCH,
                            format!("`...` spreads an `Array` or a `Vector`, but found `{other}`"),
                        )
                        .at(arg.span)
                        .label(param.span, format!("`{}` is variadic", param.name))
                        .rule("A variadic parameter is an `Array<T>`, so a spread argument must be a sequence of `T`.")
                        .help(format!("pass the value directly, as in `f(<{other}>)`")),
                    );
                    return;
                }
            };
            if !unify(element, &spread_element, generic_set, subst) {
                self.diagnostics.push(
                    Diagnostic::error(
                        MISMATCH,
                        format!("expected `{element}`, found `{spread_element}`"),
                    )
                    .at(arg.span)
                    .label(param.span, format!("`{}` is `{element}...`", param.name))
                    .rule("A variadic parameter is an `Array<T>`; every spread element is a `T`.")
                    .help(format!("spread a sequence of `{element}`")),
                );
            }
            return;
        }
        let hint = self.open(element, generics, subst);
        let hint_expected = Expected::new(
            hint.clone(),
            param.span,
            format!("{role} `{}` is `{}`", param.name, param.ty),
        );
        let found = self.expr(&arg.value, Some(&hint_expected));
        self.check_argument(
            &found,
            &hint,
            element,
            arg.span,
            param,
            generic_set,
            subst,
            role,
        );
    }

    fn report_argument(
        &mut self,
        found: &Ty,
        expected: &Ty,
        span: Span,
        param: &ParamSig,
        role: &str,
    ) {
        if found.matches(expected) {
            return;
        }
        let mut diagnostic = Diagnostic::error(
            MISMATCH,
            format!("expected `{expected}`, found `{found}`"),
        )
        .at(span)
        .label(param.span, format!("{role} `{}` is `{}`", param.name, param.ty))
        .rule("Types are nominal and there are no implicit conversions: an argument must already have the parameter's type.");
        if let Some(help) = conversion_help(expected, found) {
            diagnostic = diagnostic.help(help);
        }
        self.diagnostics.push(diagnostic);
    }

    /// A trailing block is a closure argument: `mapError { ... }` is a
    /// function of no parameters.
    fn trailing_type(&mut self, trailing: &Expr, expected: Option<&Ty>) -> Ty {
        match &trailing.kind {
            ExprKind::Block(block) => {
                let ret = match expected {
                    Some(Ty::Fn(func)) => Some(func.ret.clone()),
                    _ => None,
                };
                let hint = ret.clone().filter(|ty| !ty.is_wild()).map(|ty| {
                    let label = format!("the trailing closure produces `{ty}`");
                    Expected::new(ty, trailing.span, label)
                });
                let ty = self.block(block, hint.as_ref());
                Ty::func(
                    false,
                    Vec::new(),
                    ret.filter(|ty| !ty.is_wild()).unwrap_or(ty),
                )
            }
            _ => {
                let hint = expected.cloned().map(|ty| {
                    let label = format!("the trailing argument is `{ty}`");
                    Expected::new(ty, trailing.span, label)
                });
                self.expr(trailing, hint.as_ref())
            }
        }
    }

    /// Checks every argument of a call whose callee has no signature, so an
    /// error inside one is still reported.
    fn check_args_freely(&mut self, args: &[Arg], trailing: Option<&Expr>) {
        for arg in args {
            self.expr(&arg.value, None);
        }
        if let Some(trailing) = trailing {
            self.trailing_type(trailing, None);
        }
    }

    // ------------------------------------------------------------ methods

    fn method_call(
        &mut self,
        receiver: &Ty,
        name: &Ident,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        match receiver {
            Ty::Unknown | Ty::Never => {
                self.check_args_freely(args, trailing);
                return Ty::Unknown;
            }
            Ty::Struct(type_name, type_args) | Ty::Enum(type_name, type_args) => {
                let key = (type_name.to_string(), name.node.clone());
                if let Some(sig) = self.methods.get(&key).cloned() {
                    self.check_receiver(&sig, type_name, &name.node, span, true);
                    let generics = self.declared_generics(type_name);
                    let subst = substitution(&generics, type_args);
                    let sig = FnSig {
                        generics: sig
                            .generics
                            .iter()
                            .filter(|g| !generics.contains(g))
                            .cloned()
                            .collect(),
                        params: sig
                            .params
                            .iter()
                            .map(|p| ParamSig {
                                ty: p.ty.substitute(&subst),
                                ..p.clone()
                            })
                            .collect(),
                        ret: sig.ret.substitute(&subst),
                        ..sig
                    };
                    return self.call_signature(
                        &sig,
                        &format!("`{type_name}.{}`", name.node),
                        Vec::new(),
                        args,
                        trailing,
                        span,
                    );
                }
                let known = self.known_members(type_name);
                self.diagnostics.push(
                    Diagnostic::error(
                        UNKNOWN_METHOD,
                        format!("`{type_name}` has no method `{}`", name.node),
                    )
                    .at(span)
                    .rule("A method is declared in its type's `impl` block.")
                    .help(format!("`{type_name}` declares {known}")),
                );
                self.check_args_freely(args, trailing);
                return Ty::Unknown;
            }
            _ => {}
        }

        if let (Ty::Result(ok, error), "mapError") = (receiver, name.node.as_str()) {
            return self.map_error(ok, error, args, trailing, span);
        }

        match builtin_method(receiver, &name.node) {
            Some(sig) => {
                let what = format!("`{}.{}`", builtin_name(receiver), name.node);
                self.call_builtin(&sig, &what, args, trailing, span)
            }
            None => {
                self.diagnostics
                    .push(unknown_builtin_method(receiver, &name.node, span));
                self.check_args_freely(args, trailing);
                Ty::Unknown
            }
        }
    }

    /// `result.mapError { ... }`, which replaces a `Result`'s failure with
    /// whatever its callback produces.
    ///
    /// The Language Card writes the callback as a trailing closure that may
    /// ignore the error it replaces, so a callback of no parameters and one
    /// of a single `E` parameter are both accepted — exactly the two forms
    /// the runtime dispatches.
    fn map_error(
        &mut self,
        ok: &Ty,
        error: &Ty,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        let callback: Option<&Expr> = match (args.first(), trailing) {
            (Some(arg), None) => Some(&arg.value),
            (None, Some(trailing)) => Some(trailing),
            _ => None,
        };
        let count = args.len() + usize::from(trailing.is_some());
        if count != 1 {
            self.diagnostics.push(
                Diagnostic::error(
                    ARITY,
                    format!("`Result.mapError` takes 1 argument, but {count} were given"),
                )
                .at(span)
                .rule("`mapError` replaces a failure with the value its one callback produces.")
                .help("write `result.mapError { ... }`"),
            );
        }
        let Some(callback) = callback else {
            self.check_args_freely(args, trailing);
            return Ty::Result(Box::new(ok.clone()), Box::new(Ty::Unknown));
        };
        let takes_error =
            matches!(&callback.kind, ExprKind::Lambda { params, .. } if params.len() == 1);
        let expected = Ty::func(
            false,
            if takes_error {
                vec![error.clone()]
            } else {
                Vec::new()
            },
            Ty::Unknown,
        );
        let found = self.trailing_type(callback, Some(&expected));
        let replacement = match &found {
            Ty::Fn(func) => func.ret.clone(),
            _ => Ty::Unknown,
        };
        Ty::Result(Box::new(ok.clone()), Box::new(replacement))
    }

    /// Checks a call against a builtin signature, which binds no generics of
    /// its own beyond those already substituted into it.
    fn call_builtin(
        &mut self,
        sig: &BuiltinSig,
        what: &str,
        args: &[Arg],
        trailing: Option<&Expr>,
        span: Span,
    ) -> Ty {
        let last = sig.params.len().saturating_sub(1);
        let params: Vec<ParamSig> = sig
            .params
            .iter()
            .enumerate()
            .map(|(index, (name, ty))| ParamSig {
                name: (*name).to_string(),
                ty: ty.clone(),
                variadic: sig.variadic && index == last,
                has_default: false,
                span,
            })
            .collect();
        let subst = self.match_arguments(
            &params,
            &sig.generics,
            BTreeMap::new(),
            args,
            trailing,
            span,
            what,
            "the parameter",
        );
        self.open(&sig.ret, &sig.generics, &subst)
    }

    /// Reports a method called as an associated function, or an associated
    /// function called on a value.
    ///
    /// A method's first parameter is its receiver, written `self` or `var
    /// self`; an associated function has none. Which one a declaration is,
    /// is part of its type.
    fn check_receiver(
        &mut self,
        sig: &FnSig,
        type_name: &str,
        name: &str,
        span: Span,
        given: bool,
    ) {
        match (sig.receiver.is_some(), given) {
            (true, false) => self.diagnostics.push(
                Diagnostic::error(
                    RECEIVER,
                    format!("`{type_name}.{name}` is a method and needs a receiver"),
                )
                .at(span)
                .rule("A method is called on a value; only an associated function is called on its type.")
                .help(format!(
                    "call it on a value, as in `value.{name}(...)`, or declare `fn {name}()` without `self`"
                )),
            ),
            (false, true) => self.diagnostics.push(
                Diagnostic::error(
                    RECEIVER,
                    format!("`{type_name}.{name}` takes no receiver"),
                )
                .at(span)
                .rule("An associated function is called on its type; only a method is called on a value.")
                .help(format!("write `{type_name}.{name}(...)`")),
            ),
            _ => {}
        }
    }

    /// The type parameters a struct or enum declares.
    fn declared_generics(&self, name: &str) -> Vec<Rc<str>> {
        if let Some(sig) = self.structs.get(name) {
            return sig.generics.clone();
        }
        if let Some(sig) = self.enums.get(name) {
            return sig.generics.clone();
        }
        Vec::new()
    }

    /// The methods and cases a diagnostic can suggest for `type_name`.
    fn known_members(&self, type_name: &str) -> String {
        let mut names: Vec<String> = self
            .methods
            .keys()
            .filter(|(owner, _)| owner == type_name)
            .map(|(_, name)| name.clone())
            .collect();
        if let Some(sig) = self.enums.get(type_name) {
            names.extend(sig.cases.iter().map(|c| c.name.clone()));
        }
        if names.is_empty() {
            format!("no methods; declare one in `impl {type_name}`")
        } else {
            list(&names)
        }
    }
}

// ------------------------------------------------------------- entry shape

/// Checks every `[run.<name>]` entry against the shape the host boundary
/// calls: no parameters or one `Array<String>` of process arguments, and a
/// value the host can report.
fn check_entries(
    package: &Package,
    checked: &BTreeMap<&str, Checker<'_>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for run in package.config.runs.values() {
        let Some((module_name, entry)) = run.entry_parts() else {
            continue;
        };
        let Some(checker) = checked.get(module_name) else {
            continue;
        };
        let Some(sig) = checker.functions.get(entry) else {
            continue;
        };
        let Some(function) = checker.module.functions.get(entry) else {
            continue;
        };
        let name_span = function.decl.name.span;

        if sig.params.len() > 1 {
            diagnostics.push(
                Diagnostic::error(
                    ENTRY,
                    format!(
                        "entry `{}` declares {} parameters",
                        run.entry,
                        sig.params.len()
                    ),
                )
                .at(name_span)
                .rule("An entry function takes either no parameters or one `Array<String>` of process arguments.")
                .help(format!(
                    "write `fn {entry}()` or `fn {entry}(args: Array<String>)`"
                )),
            );
        } else if let Some(param) = sig.params.first() {
            let expected = Ty::Array(Box::new(Ty::Str));
            if !param.ty.matches(&expected) {
                diagnostics.push(
                    Diagnostic::error(
                        ENTRY,
                        format!(
                            "entry `{}` takes `{}`, but the host passes `Array<String>`",
                            run.entry, param.ty
                        ),
                    )
                    .at(param.span)
                    .rule("An entry function's one parameter is the process arguments, an `Array<String>`.")
                    .help(format!("write `fn {entry}(args: Array<String>)`")),
                );
            }
        }

        if !matches!(sig.ret, Ty::Unit | Ty::Result(_, _) | Ty::Unknown) {
            diagnostics.push(
                Diagnostic::error(
                    ENTRY,
                    format!(
                        "entry `{}` returns `{}`, which the host cannot report",
                        run.entry, sig.ret
                    ),
                )
                .at(sig.ret_span)
                .rule("The host reports an entry's failure through its `Err`, so an entry returns `()` or a `Result`.")
                .help(format!(
                    "write `fn {entry}(...) -> Result<{}, Error>`",
                    sig.ret
                )),
            );
        }
    }
}

// -------------------------------------------------------------- unification

/// Unifies a parameter type with an argument type, binding the callee's own
/// type parameters in `subst`.
///
/// This is the whole of ADR 0004's "unify at the call site, substitute into
/// the signature": a type parameter binds to the first type it meets and
/// must match every later one, and nothing else is inferred.
fn unify(
    param: &Ty,
    arg: &Ty,
    generics: &BTreeSet<Rc<str>>,
    subst: &mut BTreeMap<Rc<str>, Ty>,
) -> bool {
    if let Ty::Param(name) = param {
        if generics.contains(name) {
            return match subst.get(name) {
                Some(bound) => bound.matches(arg),
                None => {
                    if !arg.is_wild() {
                        subst.insert(name.clone(), arg.clone());
                    }
                    true
                }
            };
        }
    }
    if param.is_wild() || arg.is_wild() {
        return true;
    }
    match (param, arg) {
        (Ty::Array(a), Ty::Array(b))
        | (Ty::Vector(a), Ty::Vector(b))
        | (Ty::Set(a), Ty::Set(b))
        | (Ty::Option(a), Ty::Option(b))
        | (Ty::Task(a), Ty::Task(b)) => unify(a, b, generics, subst),
        (Ty::Map(ak, av), Ty::Map(bk, bv))
        | (Ty::MapEntry(ak, av), Ty::MapEntry(bk, bv))
        | (Ty::Result(ak, av), Ty::Result(bk, bv)) => {
            unify(ak, bk, generics, subst) && unify(av, bv, generics, subst)
        }
        (Ty::Struct(a, aargs), Ty::Struct(b, bargs)) | (Ty::Enum(a, aargs), Ty::Enum(b, bargs)) => {
            a == b
                && aargs.len() == bargs.len()
                && aargs
                    .iter()
                    .zip(bargs)
                    .all(|(a, b)| unify(a, b, generics, subst))
        }
        (Ty::Fn(a), Ty::Fn(b)) => {
            a.is_async == b.is_async
                && a.params.len() == b.params.len()
                && a.params
                    .iter()
                    .zip(&b.params)
                    .all(|(a, b)| unify(a, b, generics, subst))
                && unify(&a.ret, &b.ret, generics, subst)
        }
        (param, arg) => param.matches(arg),
    }
}

fn substitution(generics: &[Rc<str>], args: &[Ty]) -> BTreeMap<Rc<str>, Ty> {
    generics
        .iter()
        .cloned()
        .zip(args.iter().cloned().chain(std::iter::repeat(Ty::Unknown)))
        .collect()
}

/// Truncates or pads `args` to `arity`, so a type written with the wrong
/// number of arguments still has a shape the rest of the pass can use.
fn fit(mut args: Vec<Ty>, arity: usize) -> Vec<Ty> {
    args.truncate(arity);
    while args.len() < arity {
        args.push(Ty::Unknown);
    }
    args
}

// ----------------------------------------------------------------- builtins

/// A builtin method's or associated function's signature.
struct BuiltinSig {
    /// Type parameters this signature binds, unified at the call site just
    /// like a declared function's.
    generics: Vec<Rc<str>>,
    params: Vec<(&'static str, Ty)>,
    /// Whether the last parameter takes the rest of the arguments, as
    /// `Vector.of(items: T...)` does.
    variadic: bool,
    ret: Ty,
}

/// Whether `name` is a builtin type usable as a namespace.
///
/// This is `cove_runtime::builtins::is_builtin_type`, which cannot be called
/// from here: the compiler does not depend on the runtime.
fn is_builtin_type(name: &str) -> bool {
    matches!(
        name,
        "Array"
            | "Vector"
            | "String"
            | "Int"
            | "Float"
            | "Bool"
            | "Map"
            | "Set"
            | "Option"
            | "Result"
            | "Error"
    )
}

/// The signature of a builtin method, or `None` when the receiver has no such
/// method.
///
/// This table is the static half of `cove_runtime::builtins`: every method the
/// runtime dispatches appears here with a type, and nothing else does.
fn builtin_method(receiver: &Ty, name: &str) -> Option<BuiltinSig> {
    let sig = |params: Vec<(&'static str, Ty)>, ret: Ty| {
        Some(BuiltinSig {
            generics: Vec::new(),
            params,
            variadic: false,
            ret,
        })
    };
    let generic = |name: &'static str, params: Vec<(&'static str, Ty)>, ret: Ty| {
        Some(BuiltinSig {
            generics: vec![name.into()],
            params,
            variadic: false,
            ret,
        })
    };
    match receiver {
        Ty::Array(element) => match name {
            "get" => sig(
                vec![("index", Ty::Int)],
                Ty::Option(Box::new((**element).clone())),
            ),
            "length" => sig(Vec::new(), Ty::Int),
            "isEmpty" => sig(Vec::new(), Ty::Bool),
            _ => None,
        },
        Ty::Vector(element) => match name {
            "push" => sig(vec![("value", (**element).clone())], Ty::Unit),
            "get" => sig(
                vec![("index", Ty::Int)],
                Ty::Option(Box::new((**element).clone())),
            ),
            "length" => sig(Vec::new(), Ty::Int),
            "isEmpty" => sig(Vec::new(), Ty::Bool),
            "freeze" | "toArray" => sig(Vec::new(), Ty::Array(element.clone())),
            _ => None,
        },
        // `Map` and `Set` are immutable, so `inserted` and `removed` return a
        // new collection rather than change this one; their past-participle
        // names say so. Which values may be keys or elements is a runtime
        // rule — a key's equality must not be able to change — and stating it
        // here would need bounds, which the MVP does not have.
        Ty::Map(key, value) => match name {
            "get" => sig(vec![("key", (**key).clone())], Ty::Option(value.clone())),
            "contains" => sig(vec![("key", (**key).clone())], Ty::Bool),
            "length" => sig(Vec::new(), Ty::Int),
            "isEmpty" => sig(Vec::new(), Ty::Bool),
            "keys" => sig(Vec::new(), Ty::Array(key.clone())),
            "values" => sig(Vec::new(), Ty::Array(value.clone())),
            "inserted" => sig(
                vec![("key", (**key).clone()), ("value", (**value).clone())],
                Ty::Map(key.clone(), value.clone()),
            ),
            "removed" => sig(
                vec![("key", (**key).clone())],
                Ty::Map(key.clone(), value.clone()),
            ),
            _ => None,
        },
        Ty::Set(element) => match name {
            "contains" => sig(vec![("element", (**element).clone())], Ty::Bool),
            "length" => sig(Vec::new(), Ty::Int),
            "isEmpty" => sig(Vec::new(), Ty::Bool),
            "toArray" => sig(Vec::new(), Ty::Array(element.clone())),
            "inserted" | "removed" => sig(
                vec![("element", (**element).clone())],
                Ty::Set(element.clone()),
            ),
            _ => None,
        },
        Ty::Str => match name {
            "length" => sig(Vec::new(), Ty::Int),
            "isEmpty" => sig(Vec::new(), Ty::Bool),
            "words" => sig(Vec::new(), Ty::Array(Box::new(Ty::Str))),
            _ => None,
        },
        Ty::Range => match name {
            "length" => sig(Vec::new(), Ty::Int),
            "isEmpty" => sig(Vec::new(), Ty::Bool),
            "contains" => sig(vec![("value", Ty::Int)], Ty::Bool),
            _ => None,
        },
        Ty::Option(inner) => match name {
            "isSome" | "isNone" => sig(Vec::new(), Ty::Bool),
            "unwrapOr" => sig(vec![("fallback", (**inner).clone())], (**inner).clone()),
            _ => None,
        },
        Ty::Result(_ok, _error) => match name {
            "isOk" | "isError" => sig(Vec::new(), Ty::Bool),
            // `mapError` is checked by `Checker::map_error`: the Language Card
            // writes it with a trailing closure that may ignore the error it
            // replaces, so its callback takes either the error or nothing.
            _ => None,
        },
        Ty::Task(inner) => match name {
            "await" => sig(Vec::new(), (**inner).clone()),
            "cancel" => sig(Vec::new(), Ty::Unit),
            _ => None,
        },
        Ty::Scope => match name {
            // `scope.spawn { ... }` takes the trailing closure as its body and
            // hands back a handle to the value that body produces.
            "spawn" => generic(
                "T",
                vec![("body", Ty::func(false, Vec::new(), Ty::Param("T".into())))],
                Ty::Task(Box::new(Ty::Param("T".into()))),
            ),
            _ => None,
        },
        _ => None,
    }
}

/// The name a builtin type answers to in a diagnostic.
fn builtin_name(ty: &Ty) -> String {
    match ty {
        Ty::Array(_) => "Array".to_string(),
        Ty::Vector(_) => "Vector".to_string(),
        Ty::Option(_) => "Option".to_string(),
        Ty::Result(_, _) => "Result".to_string(),
        Ty::Task(_) => "Task".to_string(),
        Ty::Map(_, _) => "Map".to_string(),
        Ty::Set(_) => "Set".to_string(),
        Ty::MapEntry(_, _) => "MapEntry".to_string(),
        other => other.to_string(),
    }
}

fn unknown_builtin_method(receiver: &Ty, name: &str, span: Span) -> Diagnostic {
    let type_name = builtin_name(receiver);
    if name == "count" && matches!(receiver, Ty::Array(_) | Ty::Vector(_) | Ty::Str | Ty::Range) {
        return Diagnostic::error(
            UNKNOWN_METHOD,
            format!("`{type_name}` has no method `count`; Cove spells the number of elements `length()`"),
        )
        .at(span)
        .rule("Every sequence reports its element count as `length()`; there is no `count()`.")
        .help("write `length()` instead of `count()`");
    }
    let known = builtin_methods_of(receiver);
    Diagnostic::error(
        UNKNOWN_METHOD,
        format!("`{type_name}` has no method `{name}`"),
    )
    .at(span)
    .rule("A builtin type's methods are exactly the ones the language defines.")
    .help(if known.is_empty() {
        format!("`{type_name}` has no methods")
    } else {
        format!("`{type_name}` has {}", list(&known))
    })
}

/// Every method name a builtin receiver answers to, for a diagnostic's help.
fn builtin_methods_of(receiver: &Ty) -> Vec<String> {
    const CANDIDATES: [&str; 19] = [
        "get", "length", "isEmpty", "push", "freeze", "toArray", "words", "contains", "keys",
        "values", "inserted", "removed", "isSome", "isNone", "unwrapOr", "isOk", "isError",
        "mapError", "await",
    ];
    CANDIDATES
        .iter()
        .filter(|name| builtin_method(receiver, name).is_some())
        .map(|name| (*name).to_string())
        .collect()
}

// ------------------------------------------------------------------- prose

fn operator_symbol(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Rem => "%",
        BinaryOp::Eq => "==",
        BinaryOp::Ne => "!=",
        BinaryOp::Lt => "<",
        BinaryOp::Le => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::Ge => ">=",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
    }
}

fn starts_uppercase(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
}

fn join_path(path: &[Ident]) -> String {
    path.iter()
        .map(|segment| segment.node.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        return "nothing".to_string();
    }
    items
        .iter()
        .map(|item| format!("`{item}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn first_case_of(sig: Option<&EnumSig>) -> String {
    sig.and_then(|sig| sig.cases.first())
        .map(|case| case.name.clone())
        .unwrap_or_else(|| "Case".to_string())
}

/// The correction for a mismatch, when the language offers exactly one.
fn conversion_help(expected: &Ty, found: &Ty) -> Option<String> {
    Some(match (expected, found) {
        (Ty::Str, _) => {
            format!("interpolate the `{found}`, as in \"{{value}}\", to make a `String`")
        }
        (Ty::Int, Ty::Str) => {
            "parse the `String` with `Int.parse(text)`, which returns a `Result<Int, Error>`"
                .to_string()
        }
        (Ty::Option(inner), other) if inner.matches(other) => {
            format!("wrap it, as in `Some(value)`, to make an `Option<{inner}>`")
        }
        (Ty::Result(ok, _), other) if ok.matches(other) => {
            format!("wrap it, as in `Ok(value)`, to make a `Result<{ok}, _>`")
        }
        (other, Ty::Option(inner)) if other.matches(inner) => {
            format!(
                "unwrap it, as in `value.unwrapOr(<{other}>)`, which always produces a `{other}`"
            )
        }
        (Ty::Array(element), Ty::Vector(other)) if element.matches(other) => {
            "finish the vector, as in `vector.freeze()` or `vector.toArray()`".to_string()
        }
        (Ty::Float, Ty::Int) | (Ty::Int, Ty::Float) => {
            format!("write the literal as a `{expected}`; Cove converts nothing implicitly")
        }
        _ => return None,
    })
}

fn condition_help(ty: &Ty) -> String {
    match ty {
        Ty::Option(_) => "compare it, as in `value.isSome()`".to_string(),
        Ty::Int | Ty::Float => format!("compare it, as in `value != 0`; a `{ty}` is not a `Bool`"),
        _ => format!("compare it, so the condition is a `Bool` rather than a `{ty}`"),
    }
}

fn iterable_help(ty: &Ty) -> String {
    match ty {
        Ty::Option(inner) => {
            format!("match the `Option`, or write `for x in [value.unwrapOr(<{inner}>)]`")
        }
        Ty::Int => "write a range, as in `0..<n`".to_string(),
        Ty::MapEntry(_, _) => {
            "iterate the `Map` itself; `for` already binds each pair as a `MapEntry`".to_string()
        }
        _ => format!("build an `Array`, a `Vector`, or a `Range` from the `{ty}` first"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::package::{Module, Unit};
    use crate::resolve::resolve;
    use cove_diag::{Severity, SourceMap};
    use std::path::{Path, PathBuf};

    /// Type-checks one module written inline, without touching the
    /// filesystem, and returns everything it reported.
    fn diagnostics_of(source: &str) -> Vec<Diagnostic> {
        diagnostics_with(source, Config::default())
    }

    fn diagnostics_with(source: &str, config: Config) -> Vec<Diagnostic> {
        let mut sources = SourceMap::new();
        let path = PathBuf::from("main.cove");
        let file = sources.add(path.clone(), source);
        let ast = cove_syntax::parse_file(&sources, file).expect("test source parses");
        let mut modules = BTreeMap::new();
        modules.insert(
            "main".to_string(),
            Module {
                name: "main".to_string(),
                dir: PathBuf::from("main"),
                units: vec![Unit { file, path, ast }],
            },
        );
        let package = Package {
            root: PathBuf::new(),
            config,
            modules,
        };
        let program = resolve(&package).expect("test source resolves");
        check(&package, &program)
    }

    fn errors_of(source: &str) -> Vec<Diagnostic> {
        diagnostics_of(source)
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .collect()
    }

    fn warnings_of(source: &str) -> Vec<Diagnostic> {
        diagnostics_of(source)
            .into_iter()
            .filter(|d| d.severity == Severity::Warning)
            .collect()
    }

    /// Asserts that `source` checks, showing what was reported when it does
    /// not.
    #[track_caller]
    fn accepts(source: &str) {
        let errors = errors_of(source);
        assert!(
            errors.is_empty(),
            "expected no errors, found: {}",
            errors
                .iter()
                .map(|d| format!("{}: {}", d.code, d.message))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }

    /// Asserts that `source` produces exactly one error, and returns it.
    #[track_caller]
    fn rejects(source: &str) -> Diagnostic {
        let mut errors = errors_of(source);
        assert_eq!(
            errors.len(),
            1,
            "expected exactly one error, found: {}",
            errors
                .iter()
                .map(|d| format!("{}: {}", d.code, d.message))
                .collect::<Vec<_>>()
                .join("; ")
        );
        errors.remove(0)
    }

    /// Wraps `body` in an entry function, the shape most of these tests need.
    fn in_main(body: &str) -> String {
        format!(
            "use console.println\n\nexport fn main() -> Result<Unit, Error> {{\n{body}\n  Ok(())\n}}\n"
        )
    }

    #[track_caller]
    fn accepts_body(body: &str) {
        accepts(&in_main(body));
    }

    #[track_caller]
    fn rejects_body(body: &str) -> Diagnostic {
        rejects(&in_main(body))
    }

    // ------------------------------------------------------- accepted

    #[test]
    fn accepts_the_card_s_greeting_program() {
        accepts(
            "\
use console.println

export fn greet(name: String) -> String {
  \"Hello, {name}!\"
}

export fn main(args: Array<String>) -> Result<Unit, Error> {
  let name = args.get(0).unwrapOr(\"world\")
  console.println(greet(name))?
  Ok(())
}
",
        );
    }

    #[test]
    fn infers_a_let_from_its_initializer() {
        accepts_body("  let n = 1\n  let doubled = n * 2\n  println(\"{doubled}\")?");
    }

    #[test]
    fn checks_a_written_let_annotation() {
        accepts_body("  let n: Int = 1\n  println(\"{n}\")?");
        let error = rejects_body("  let n: Int = \"one\"");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
        assert_eq!(error.rule.unwrap(), "Types are nominal and there are no implicit conversions: a value must already have the type its place asks for.");
        assert_eq!(
            error.help.unwrap(),
            "parse the `String` with `Int.parse(text)`, which returns a `Result<Int, Error>`"
        );
    }

    #[test]
    fn an_array_literal_takes_its_element_type_from_its_elements() {
        accepts_body("  let items = [1, 2]\n  let first: Option<Int> = items.get(0)");
        let error = rejects_body("  let items = [1, \"two\"]");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn an_empty_array_literal_has_no_element_type_to_infer() {
        // Nothing says what the elements are, so operations that do not
        // depend on the element type still work and the rest is unchecked.
        accepts_body("  let empty = []\n  println(\"{empty.length()} {empty.isEmpty()}\")?");
    }

    #[test]
    fn a_vector_grows_and_freezes_into_an_array() {
        accepts(
            "\
fn build(upTo: Int) -> Array<Int> {
  var building = Vector.of(1)
  for n in 1..upTo {
    building.push(n)
  }
  building.freeze()
}
",
        );
        let error = rejects(
            "\
fn build() -> Array<String> {
  var building = Vector.of(1)
  building.freeze()
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(
            error.message,
            "expected `Array<String>`, found `Array<Int>`"
        );
    }

    #[test]
    fn a_vector_push_takes_the_element_type() {
        let error = rejects(
            "\
fn build() -> Array<Int> {
  var building = Vector.of(1)
  building.push(\"two\")
  building.freeze()
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn checks_every_map_operation() {
        accepts_body(
            "  let ages = Map.of(MapEntry(key: \"Alice\", value: 30))\n\
             \x20 let found: Option<Int> = ages.get(\"Alice\")\n\
             \x20 let has: Bool = ages.contains(\"Bob\")\n\
             \x20 let n: Int = ages.length()\n\
             \x20 let empty: Bool = ages.isEmpty()\n\
             \x20 let names: Array<String> = ages.keys()\n\
             \x20 let numbers: Array<Int> = ages.values()\n\
             \x20 let more: Map<String, Int> = ages.inserted(\"Carol\", 41)\n\
             \x20 let fewer: Map<String, Int> = ages.removed(\"Alice\")",
        );
        let error =
            rejects_body("  let ages = Map.of(MapEntry(key: \"Alice\", value: 30))\n  ages.get(1)");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
    }

    #[test]
    fn checks_every_set_operation() {
        accepts_body(
            "  let tags = Set.of(\"a\", \"b\")\n\
             \x20 let has: Bool = tags.contains(\"a\")\n\
             \x20 let n: Int = tags.length()\n\
             \x20 let empty: Bool = tags.isEmpty()\n\
             \x20 let items: Array<String> = tags.toArray()\n\
             \x20 let bigger: Set<String> = tags.inserted(\"c\")\n\
             \x20 let smaller: Set<String> = tags.removed(\"a\")",
        );
        let error = rejects_body("  let tags = Set.of(\"a\")\n  tags.contains(1)");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
    }

    #[test]
    fn map_of_collects_map_entries() {
        let error = rejects_body("  let ages = Map.of(1)");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `MapEntry<_, _>`, found `Int`");

        let error = rejects_body(
            "  let ages = Map.of(MapEntry(key: \"a\", value: 1), MapEntry(key: 2, value: 3))",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(
            error.message,
            "expected `MapEntry<String, Int>`, found `MapEntry<Int, Int>`"
        );
    }

    #[test]
    fn a_map_entry_carries_a_key_and_a_value() {
        accepts_body(
            "  let entry = MapEntry(key: \"a\", value: 1)\n\
             \x20 let key: String = entry.key\n\
             \x20 let value: Int = entry.value",
        );
        let error = rejects_body("  let entry = MapEntry(key: \"a\", value: 1)\n  entry.other");
        assert_eq!(error.code, UNKNOWN_FIELD);
        assert_eq!(error.message, "`MapEntry` has no field `other`");
        assert_eq!(
            error.rule.unwrap(),
            "A `MapEntry` carries exactly a `key` and a `value`."
        );
        assert_eq!(error.help.unwrap(), "write `.key` or `.value`");
    }

    #[test]
    fn a_map_iterates_map_entries_and_a_set_its_elements() {
        accepts_body(
            "  let ages = Map.of(MapEntry(key: \"a\", value: 1))\n\
             \x20 for entry in ages {\n\
             \x20   let key: String = entry.key\n\
             \x20   let value: Int = entry.value\n\
             \x20 }\n\
             \x20 for tag in Set.of(\"a\") {\n\
             \x20   let element: String = tag\n\
             \x20 }",
        );
        let error = rejects_body(
            "  let ages = Map.of(MapEntry(key: \"a\", value: 1))\n\
             \x20 for entry in ages {\n\
             \x20   let key: Int = entry.key\n\
             \x20 }",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn checks_every_range_operation() {
        accepts_body(
            "  let range = 0..<3\n\
             \x20 let n: Int = range.length()\n\
             \x20 let empty: Bool = range.isEmpty()\n\
             \x20 let has: Bool = range.contains(1)",
        );
        let error = rejects_body("  let range = 0..<3\n  range.contains(\"one\")");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn checks_struct_initialization_and_field_access() {
        accepts(
            "\
struct Point { x: Int, y: Int }

fn sum(point: Point) -> Int {
  point.x + point.y
}

fn origin() -> Point {
  Point(x: 0, y: 0)
}
",
        );
    }

    #[test]
    fn rejects_a_struct_field_of_the_wrong_type() {
        let error = rejects(
            "\
struct Point { x: Int, y: Int }

fn origin() -> Point {
  Point(x: 0, y: \"zero\")
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
        assert_eq!(error.labels[0].message, "the field `y` is `Int`");
    }

    #[test]
    fn rejects_a_missing_struct_field() {
        let error = rejects(
            "\
struct Point { x: Int, y: Int }

fn origin() -> Point {
  Point(x: 0)
}
",
        );
        assert_eq!(error.code, MISSING_ARGUMENT);
        assert_eq!(error.message, "`Point` needs the field `y`");
        assert_eq!(
            error.rule.unwrap(),
            "A call passes every parameter that has no default."
        );
        assert_eq!(error.help.unwrap(), "pass `y: <Int>`");
    }

    #[test]
    fn rejects_a_field_the_struct_does_not_declare() {
        let error = rejects(
            "\
struct Point { x: Int, y: Int }

fn z(point: Point) -> Int {
  point.z
}
",
        );
        assert_eq!(error.code, UNKNOWN_FIELD);
        assert_eq!(error.message, "`Point` has no field `z`");
        assert_eq!(
            error.rule.unwrap(),
            "A struct's fields are exactly the ones its declaration lists."
        );
        assert_eq!(error.help.unwrap(), "`Point` declares `x`, `y`");
    }

    #[test]
    fn checks_enum_construction_and_match_payloads() {
        accepts(
            "\
enum Status {
  Pending
  Active(Int)
}

fn describe(status: Status) -> String {
  match status {
    Status.Pending => \"pending\"
    Status.Active(since) => \"active since {since}\"
  }
}

fn active() -> Status {
  Status.Active(7)
}
",
        );
    }

    #[test]
    fn rejects_an_enum_payload_of_the_wrong_type() {
        let error = rejects(
            "\
enum Status {
  Active(Int)
}

fn active() -> Status {
  Status.Active(\"now\")
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn rejects_an_enum_payload_of_the_wrong_arity() {
        let error = rejects(
            "\
enum Status {
  Active(Int)
}

fn active() -> Status {
  Status.Active(1, 2)
}
",
        );
        assert_eq!(error.code, PAYLOAD_ARITY);
        assert_eq!(
            error.message,
            "`Status.Active` carries 1 value(s), but 2 were given"
        );
        assert_eq!(
            error.rule.unwrap(),
            "An enum case carries exactly the payload its declaration writes."
        );
        assert_eq!(error.help.unwrap(), "write `Status.Active(Int)`");
    }

    #[test]
    fn rejects_a_case_the_enum_does_not_declare() {
        let error = rejects(
            "\
enum Status {
  Pending
}

fn active() -> Status {
  Status.Active
}
",
        );
        assert_eq!(error.code, UNKNOWN_CASE);
        assert_eq!(error.message, "`Status` has no case `Active`");
        assert_eq!(
            error.rule.unwrap(),
            "An enum's cases are exactly the ones its declaration lists."
        );
        assert_eq!(error.help.unwrap(), "`Status` declares `Pending`");
    }

    #[test]
    fn rejects_a_pattern_from_another_enum() {
        let error = rejects(
            "\
enum Suit {
  Hearts
}

enum Card {
  Blank
}

fn name(card: Card) -> String {
  match card {
    Suit.Hearts => \"hearts\"
    _ => \"other\"
  }
}
",
        );
        assert_eq!(error.code, PATTERN);
        assert_eq!(
            error.message,
            "this pattern matches `Suit`, but the scrutinee is `Card`"
        );
        assert_eq!(
            error.rule.unwrap(),
            "A pattern matches values of the scrutinee's type."
        );
        assert_eq!(
            error.help.unwrap(),
            "write a `Card` case, such as `Card.Blank`"
        );
    }

    #[test]
    fn rejects_a_literal_pattern_of_another_type() {
        let error = rejects(
            "\
fn name(n: Int) -> String {
  match n {
    \"one\" => \"one\"
    _ => \"other\"
  }
}
",
        );
        assert_eq!(error.code, PATTERN);
        assert_eq!(
            error.message,
            "this pattern matches `String`, but the scrutinee is `Int`"
        );
        assert_eq!(
            error.help.unwrap(),
            "write a `Int` literal, or a binding such as `other`"
        );
    }

    #[test]
    fn checks_methods_and_associated_functions() {
        accepts(
            "\
struct Counter { hits: Int }

impl Counter {
  fn start() -> Counter {
    Counter(hits: 0)
  }

  fn hit(var self) {
    self.hits += 1
  }

  fn describe(self) -> String {
    \"{self.hits}\"
  }
}

fn run() -> String {
  var counter = Counter.start()
  counter.hit()
  counter.describe()
}
",
        );
    }

    #[test]
    fn a_method_needs_a_receiver_and_an_associated_function_takes_none() {
        let source = "\
struct Counter { hits: Int }

impl Counter {
  fn start() -> Counter {
    Counter(hits: 0)
  }

  fn describe(self) -> String {
    \"{self.hits}\"
  }
}
";
        let error = rejects(&format!(
            "{source}\nfn run() -> String {{\n  Counter.describe()\n}}\n"
        ));
        assert_eq!(error.code, RECEIVER);
        assert_eq!(
            error.message,
            "`Counter.describe` is a method and needs a receiver"
        );
        assert_eq!(
            error.rule.unwrap(),
            "A method is called on a value; only an associated function is called on its type."
        );
        assert_eq!(
            error.help.unwrap(),
            "call it on a value, as in `value.describe(...)`, or declare `fn describe()` without `self`"
        );

        let error = rejects(&format!(
            "{source}\nfn run(counter: Counter) -> Counter {{\n  counter.start()\n}}\n"
        ));
        assert_eq!(error.code, RECEIVER);
        assert_eq!(error.message, "`Counter.start` takes no receiver");
        assert_eq!(error.help.unwrap(), "write `Counter.start(...)`");
    }

    #[test]
    fn rejects_a_method_the_type_does_not_declare() {
        let error = rejects(
            "\
struct Counter { hits: Int }

impl Counter {
  fn describe(self) -> String {
    \"{self.hits}\"
  }
}

fn run(counter: Counter) -> String {
  counter.report()
}
",
        );
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(error.message, "`Counter` has no method `report`");
        assert_eq!(
            error.rule.unwrap(),
            "A method is declared in its type's `impl` block."
        );
        assert_eq!(error.help.unwrap(), "`Counter` declares `describe`");
    }

    #[test]
    fn rejects_an_associated_function_the_type_does_not_declare() {
        let error = rejects(
            "\
struct Counter { hits: Int }

fn run() -> Counter {
  Counter.start()
}
",
        );
        assert_eq!(error.code, UNKNOWN_ASSOCIATED);
        assert_eq!(
            error.message,
            "`Counter` has no associated function `start`"
        );
        assert_eq!(
            error.help.unwrap(),
            "`Counter` declares no methods; declare one in `impl Counter`"
        );
    }

    #[test]
    fn count_is_spelled_length() {
        let error = rejects_body("  let items = [1]\n  println(\"{items.count()}\")?");
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(
            error.message,
            "`Array` has no method `count`; Cove spells the number of elements `length()`"
        );
        assert_eq!(
            error.rule.unwrap(),
            "Every sequence reports its element count as `length()`; there is no `count()`."
        );
        assert_eq!(error.help.unwrap(), "write `length()` instead of `count()`");
    }

    #[test]
    fn rejects_a_builtin_method_that_does_not_exist() {
        let error = rejects_body("  println(\"{\"text\".trim()}\")?");
        assert_eq!(error.code, UNKNOWN_METHOD);
        assert_eq!(error.message, "`String` has no method `trim`");
        assert_eq!(
            error.help.unwrap(),
            "`String` has `length`, `isEmpty`, `words`"
        );
    }

    // ------------------------------------------------------ calls

    #[test]
    fn checks_an_argument_against_its_parameter() {
        let error = rejects(
            "\
fn greet(name: String) -> String {
  name
}

fn run() -> String {
  greet(42)
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
        assert_eq!(error.labels[0].message, "the parameter `name` is `String`");
        assert_eq!(
            error.help.unwrap(),
            "interpolate the `Int`, as in \"{value}\", to make a `String`"
        );
    }

    #[test]
    fn rejects_too_many_arguments() {
        let error = rejects(
            "\
fn greet(name: String) -> String {
  name
}

fn run() -> String {
  greet(\"a\", \"b\")
}
",
        );
        assert_eq!(error.code, ARITY);
        assert_eq!(
            error.message,
            "`greet` takes 1 argument(s), but more were given"
        );
        assert_eq!(
            error.rule.unwrap(),
            "A call passes exactly the arguments the declaration binds."
        );
        assert_eq!(error.help.unwrap(), "`greet` declares `name`");
    }

    #[test]
    fn rejects_a_label_that_names_no_parameter() {
        let error = rejects(
            "\
fn between(low: Int, high: Int) -> Int {
  high - low
}

fn run() -> Int {
  between(low: 1, top: 2)
}
",
        );
        assert_eq!(error.code, UNKNOWN_LABEL);
        assert_eq!(error.message, "`between` has no parameter labeled `top`");
        assert_eq!(
            error.rule.unwrap(),
            "Argument labels are parameter names and part of the API contract."
        );
        assert_eq!(error.help.unwrap(), "known labels: `low`, `high`");
    }

    #[test]
    fn labels_bind_arguments_to_the_parameters_they_name() {
        // Argument *order* is the runtime's rule, so a label out of
        // declaration order still binds by name here.
        accepts(
            "\
fn between(low: Int, high: Int) -> Int {
  high - low
}

fn run() -> Int {
  between(high: 2, low: 1)
}
",
        );
    }

    #[test]
    fn a_parameter_with_a_default_may_be_omitted() {
        accepts(
            "\
fn measure(value: Int, unit: String = \"m\") -> String {
  \"{value}{unit}\"
}

fn run() -> String {
  measure(3)
}
",
        );
        let error = rejects(
            "\
fn measure(value: Int, unit: String = 1) -> String {
  \"{value}{unit}\"
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
    }

    #[test]
    fn checks_a_variadic_parameter_and_its_spread() {
        accepts(
            "\
fn joinAll(separator: String, items: String...) -> Int {
  items.length()
}

fn run() -> Int {
  let ready = [\"x\"]
  joinAll(\"-\", \"a\", ...ready)
}
",
        );
        let error = rejects(
            "\
fn joinAll(items: String...) -> Int {
  items.length()
}

fn run() -> Int {
  joinAll(\"a\", 2)
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
    }

    #[test]
    fn rejects_a_spread_of_the_wrong_element_type() {
        let error = rejects(
            "\
fn joinAll(items: String...) -> Int {
  items.length()
}

fn run() -> Int {
  joinAll(...[1, 2])
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
        assert_eq!(
            error.rule.unwrap(),
            "A variadic parameter is an `Array<T>`; every spread element is a `T`."
        );
        assert_eq!(error.help.unwrap(), "spread a sequence of `String`");
    }

    #[test]
    fn rejects_calling_something_that_is_not_a_function() {
        let error = rejects_body("  let n = 1\n  println(\"{n(2)}\")?");
        assert_eq!(error.code, NOT_CALLABLE);
        assert_eq!(error.message, "`Int` is not a function");
        assert_eq!(error.rule.unwrap(), "Only a function value can be called.");
    }

    #[test]
    fn rejects_calling_an_enum_rather_than_a_case() {
        let error = rejects(
            "\
enum Status {
  Pending
}

fn run() -> Status {
  Status(1)
}
",
        );
        assert_eq!(error.code, NOT_CALLABLE);
        assert_eq!(error.message, "`Status` is an enum, not a function");
        assert_eq!(error.help.unwrap(), "name a case, such as `Status.Pending`");
    }

    // ------------------------------------------------------ generics

    #[test]
    fn unifies_a_type_parameter_at_the_call_site() {
        accepts(
            "\
fn first<T>(items: Array<T>, fallback: T) -> T {
  items.get(0).unwrapOr(fallback)
}

fn run() -> Int {
  first([1, 2], 0)
}
",
        );
    }

    #[test]
    fn rejects_a_type_parameter_used_at_two_types() {
        let error = rejects(
            "\
fn pair<T>(left: T, right: T) -> T {
  left
}

fn run() -> Int {
  pair(1, \"two\")
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn substitutes_a_type_parameter_into_the_result() {
        let error = rejects(
            "\
fn identity<T>(value: T) -> T {
  value
}

fn run() -> String {
  identity(1)
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
    }

    #[test]
    fn checks_a_generic_struct_s_fields_through_its_arguments() {
        accepts(
            "\
struct Box<T> { value: T }

fn unwrap(box: Box<Int>) -> Int {
  box.value
}
",
        );
        let error = rejects(
            "\
struct Box<T> { value: T }

fn unwrap(box: Box<String>) -> Int {
  box.value
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn a_generic_enum_takes_its_arguments_from_its_payload() {
        accepts(
            "\
enum Slot<T> {
  Full(T)
  Empty
}

fn run() -> Slot<Int> {
  Slot.Full(1)
}
",
        );
        let error = rejects(
            "\
enum Slot<T> {
  Full(T)
  Empty
}

fn run() -> Slot<Int> {
  Slot.Full(\"one\")
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Slot<Int>`, found `Slot<String>`");
    }

    #[test]
    fn a_generic_type_s_method_sees_its_arguments() {
        accepts(
            "\
struct Slot<T> { value: T }

impl Slot {
  fn get(self) -> T {
    self.value
  }
}

fn run(slot: Slot<Int>) -> Int {
  slot.get()
}
",
        );
        let error = rejects(
            "\
struct Slot<T> { value: T }

impl Slot {
  fn get(self) -> T {
    self.value
  }
}

fn run(slot: Slot<String>) -> Int {
  slot.get()
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn rejects_the_wrong_number_of_type_arguments() {
        let error = rejects("fn run(items: Array<Int, String>) -> Int {\n  1\n}\n");
        assert_eq!(error.code, TYPE_ARGUMENTS);
        assert_eq!(
            error.message,
            "`Array` takes 1 type argument(s), but 2 were written"
        );
        assert_eq!(
            error.rule.unwrap(),
            "A generic type is written with exactly the arguments its declaration binds."
        );
        assert_eq!(error.help.unwrap(), "write `Array<_>`");
    }

    // ------------------------------------------------------ lambdas

    #[test]
    fn a_lambda_takes_its_parameter_types_from_the_expected_type() {
        accepts(
            "\
fn apply(value: Int, transform: fn(Int) -> Int) -> Int {
  transform(value)
}

fn run() -> Int {
  apply(5, fn(n) { n + 1 })
}
",
        );
    }

    #[test]
    fn rejects_a_lambda_whose_result_does_not_fit() {
        let error = rejects(
            "\
fn apply(value: Int, transform: fn(Int) -> Int) -> Int {
  transform(value)
}

fn run() -> Int {
  apply(5, fn(n) { \"{n}\" })
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn rejects_a_lambda_with_the_wrong_number_of_parameters() {
        let error = rejects(
            "\
fn apply(transform: fn(Int) -> Int) -> Int {
  transform(1)
}

fn run() -> Int {
  apply(fn(a, b) { a })
}
",
        );
        assert_eq!(error.code, ARITY);
        assert_eq!(
            error.message,
            "this function takes 2 parameter(s), but 1 were expected here"
        );
        assert_eq!(error.help.unwrap(), "write `fn(p0) { ... }`");
    }

    #[test]
    fn a_lambda_with_no_expected_type_infers_nothing_about_its_parameters() {
        // Nothing says what `n` is, so the checker abstains rather than
        // guessing, and the body is still walked.
        accepts_body("  let double = fn(n) { n * 2 }\n  println(\"{double(4)}\")?");
    }

    #[test]
    fn checks_a_function_value_s_arguments() {
        let error = rejects(
            "\
fn apply(transform: fn(Int) -> Int) -> Int {
  transform(\"one\")
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    // ------------------------------------------------- operators

    #[test]
    fn rejects_mixed_arithmetic() {
        let error = rejects_body("  println(\"{1 + 1.0}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`+` is not defined for `Int` and `Float`");
        assert_eq!(
            error.rule.unwrap(),
            "There are no implicit numeric, string, or boolean conversions."
        );
        assert_eq!(
            error.help.unwrap(),
            "arithmetic combines two values of the same type"
        );
    }

    #[test]
    fn rejects_mixed_equality() {
        let error = rejects_body("  println(\"{1 == \"1\"}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "cannot compare `Int` with `String`");
        assert_eq!(
            error.rule.unwrap(),
            "`==` means value equality between values of the same type."
        );
        assert_eq!(
            error.help.unwrap(),
            "convert one side explicitly so both are `Int`, or compare values that already share a type"
        );
    }

    #[test]
    fn rejects_adding_two_strings() {
        let error = rejects_body("  println(\"{\"a\" + \"b\"}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`+` is not defined for `String`");
        assert_eq!(
            error.rule.unwrap(),
            "There are no implicit string conversions."
        );
        assert_eq!(
            error.help.unwrap(),
            "use string interpolation, such as \"{left}{right}\""
        );
    }

    #[test]
    fn accepts_duration_arithmetic_and_comparison() {
        accepts_body("  println(\"{1s + 500ms} {1s > 999ms}\")?");
        let error = rejects_body("  println(\"{1s * 2s}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(
            error.message,
            "`*` is not defined for `Duration` and `Duration`"
        );
    }

    #[test]
    fn rejects_negating_a_string() {
        let error = rejects_body("  println(\"{-\"a\"}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`-` is not defined for `String`");
        assert_eq!(
            error.help.unwrap(),
            "`-` negates an `Int`, a `Float`, or a `Duration`"
        );
    }

    #[test]
    fn rejects_a_non_bool_operand_of_and() {
        let error = rejects_body("  println(\"{1 && true}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`&&` is not defined for `Int` and `Bool`");
        assert_eq!(error.help.unwrap(), "`&&` and `||` combine two `Bool`s");
    }

    #[test]
    fn rejects_ordering_two_bools() {
        let error = rejects_body("  println(\"{true < false}\")?");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`<` is not defined for `Bool` and `Bool`");
    }

    #[test]
    fn rejects_a_non_bool_condition() {
        let error = rejects_body("  if 1 {\n    println(\"never\")?\n  }");
        assert_eq!(error.code, CONDITION);
        assert_eq!(
            error.message,
            "a condition must be a `Bool`, but found `Int`"
        );
        assert_eq!(
            error.rule.unwrap(),
            "There are no implicit boolean conversions."
        );
        assert_eq!(
            error.help.unwrap(),
            "compare it, as in `value != 0`; a `Int` is not a `Bool`"
        );
    }

    // ------------------------------------------------- control flow

    #[test]
    fn an_if_with_no_else_is_a_statement() {
        accepts_body("  var seen = 0\n  if true {\n    seen = 1\n  }\n  println(\"{seen}\")?");
        // Its value is `()` whatever the branch produces, so binding it and
        // using it as an `Int` is an error.
        let error = rejects_body("  let n: Int = if true { 1 }");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `()`");
    }

    #[test]
    fn if_branches_must_agree() {
        accepts_body("  let n = if true { 1 } else { 2 }\n  println(\"{n}\")?");
        let error = rejects_body("  let n = if true { 1 } else { \"two\" }");
        assert_eq!(error.code, BRANCHES);
        assert_eq!(
            error.message,
            "this branch produces `String`, but the other produces `Int`"
        );
        assert_eq!(
            error.rule.unwrap(),
            "Every branch of an `if` or `match` used as an expression produces the same type."
        );
        assert_eq!(
            error.help.unwrap(),
            "make both branches produce `Int`, or bind them separately"
        );
    }

    #[test]
    fn match_arms_must_agree() {
        let error = rejects(
            "\
fn name(n: Int) -> String {
  let value = match n {
    0 => \"zero\"
    _ => 1
  }
  \"{value}\"
}
",
        );
        assert_eq!(error.code, BRANCHES);
        assert_eq!(
            error.message,
            "this branch produces `Int`, but the other produces `String`"
        );
    }

    #[test]
    fn a_return_never_disagrees_with_a_branch() {
        accepts(
            "\
fn label(n: Int) -> String {
  match n {
    0 => return \"zero\"
    _ => \"other\"
  }
}
",
        );
    }

    #[test]
    fn checks_return_against_the_declared_return_type() {
        let error = rejects(
            "\
fn label(n: Int) -> String {
  if n == 0 {
    return 0
  }
  \"other\"
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `String`, found `Int`");
        assert_eq!(
            error.labels[0].message,
            "the declared return type is `String`"
        );
    }

    #[test]
    fn a_block_s_value_is_its_tail() {
        accepts_body("  let n = {\n    let base = 1\n    base + 1\n  }\n  println(\"{n}\")?");
        accepts_body("  let nothing = { }\n  println(\"{nothing}\")?");
    }

    #[test]
    fn a_function_with_no_return_type_returns_unit() {
        accepts(
            "\
fn record(var log: Vector<String>, entry: String) {
  log.push(entry)
}
",
        );
        let error = rejects("fn total() {\n  1\n}\n");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `()`, found `Int`");
        assert_eq!(
            error.labels[0].message,
            "this function declares no return type, so it returns `()`"
        );
    }

    #[test]
    fn checks_a_for_loop_s_iterable_and_binding() {
        accepts(
            "\
fn total(items: Array<Int>) -> Int {
  var sum = 0
  for item in items {
    sum += item
  }
  sum
}
",
        );
        let error = rejects(
            "\
fn total(items: Array<String>) -> Int {
  var sum = 0
  for item in items {
    sum += item
  }
  sum
}
",
        );
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`+` is not defined for `Int` and `String`");
    }

    #[test]
    fn rejects_iterating_something_that_is_not_a_sequence() {
        let error = rejects_body("  for n in 1 {\n    println(\"{n}\")?\n  }");
        assert_eq!(error.code, ITERABLE);
        assert_eq!(
            error.message,
            "`for` iterates an `Array`, a `Vector`, a `Range`, a `Set`, or a `Map`, but found `Int`"
        );
        assert_eq!(
            error.rule.unwrap(),
            "`for` iterates a sequence; iteration order is defined by each collection type."
        );
        assert_eq!(error.help.unwrap(), "write a range, as in `0..<n`");
    }

    #[test]
    fn checks_an_assignment_against_the_place_s_type() {
        let error = rejects_body("  var n = 1\n  n = \"one\"");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn checks_a_compound_assignment_with_the_operator_s_rule() {
        accepts_body("  var n = 1\n  n += 2\n  println(\"{n}\")?");
        let error = rejects_body("  var n = 1\n  n += 1.0");
        assert_eq!(error.code, OPERATOR);
        assert_eq!(error.message, "`+` is not defined for `Int` and `Float`");
    }

    #[test]
    fn a_range_takes_two_ints() {
        accepts_body("  let range = 0..<3\n  println(\"{range.length()}\")?");
        let error = rejects_body("  let range = 0..<\"three\"");
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    // ------------------------------------------------- ? and await

    #[test]
    fn checks_the_question_mark_against_the_enclosing_return_type() {
        accepts(
            "\
fn double(text: String) -> Result<Int, Error> {
  let value = Int.parse(text)?
  Ok(value * 2)
}
",
        );
    }

    #[test]
    fn rejects_the_question_mark_on_a_value_that_cannot_fail() {
        let error = rejects(
            "\
fn length(text: String) -> Result<Int, Error> {
  let n = text.length()?
  Ok(n)
}
",
        );
        assert_eq!(error.code, TRY_OPERAND);
        assert_eq!(
            error.message,
            "`?` needs a `Result` or an `Option`, but found `Int`"
        );
        assert_eq!(
            error.rule.unwrap(),
            "`expr?` returns the error from the current function."
        );
        assert_eq!(error.help.unwrap(), "`Int` cannot fail, so drop the `?`");
    }

    #[test]
    fn rejects_the_question_mark_when_the_failure_types_differ() {
        let error = rejects(
            "\
enum ParseError {
  NotANumber
}

fn double(text: String) -> Result<Int, ParseError> {
  let value = Int.parse(text)?
  Ok(value * 2)
}
",
        );
        assert_eq!(error.code, TRY_RETURN);
        assert_eq!(
            error.message,
            "`?` propagates `Error`, but this function returns `ParseError` as its failure"
        );
        assert_eq!(
            error.rule.unwrap(),
            "`expr?` returns the error from the current function, so the two failure types must be the same."
        );
        assert_eq!(
            error.help.unwrap(),
            "map the failure first, as in `expr.mapError { ... }?`, or declare this function `-> Result<_, Error>`"
        );
    }

    #[test]
    fn rejects_the_question_mark_in_a_function_that_cannot_fail() {
        let error = rejects(
            "\
fn double(text: String) -> Int {
  Int.parse(text)? * 2
}
",
        );
        assert_eq!(error.code, TRY_RETURN);
        assert_eq!(
            error.message,
            "`?` needs a function that returns a `Result`, but this one returns `Int`"
        );
        assert_eq!(
            error.help.unwrap(),
            "declare this function `-> Result<Int, Error>`"
        );
    }

    #[test]
    fn the_question_mark_unwraps_an_option_inside_an_option() {
        accepts(
            "\
fn shout(text: String) -> Option<String> {
  let word = text.words().get(0)?
  Some(\"{word}!\")
}
",
        );
        let error = rejects(
            "\
fn shout(text: String) -> String {
  let word = text.words().get(0)?
  \"{word}!\"
}
",
        );
        assert_eq!(error.code, TRY_RETURN);
        assert_eq!(
            error.message,
            "`?` on an `Option` needs a function that returns an `Option`, but this one returns `String`"
        );
    }

    #[test]
    fn map_error_replaces_the_failure_type() {
        accepts(
            "\
enum ParseError {
  NotANumber(String)
}

fn parseOrFail(text: String) -> Result<Int, ParseError> {
  Int.parse(text).mapError { ParseError.NotANumber(text) }
}
",
        );
        let error = rejects(
            "\
enum ParseError {
  NotANumber(String)
}

fn parseOrFail(text: String) -> Result<Int, ParseError> {
  Int.parse(text).mapError { text }
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(
            error.message,
            "expected `Result<Int, ParseError>`, found `Result<Int, String>`"
        );
    }

    #[test]
    fn map_error_also_takes_a_callback_of_the_error() {
        accepts(
            "\
fn keep(text: String) -> Result<Int, Error> {
  Int.parse(text).mapError(fn(error) { error })
}
",
        );
    }

    #[test]
    fn calling_an_async_function_produces_a_task_that_await_settles() {
        accepts(
            "\
async fn load() -> Int {
  1
}

async fn run() -> Int {
  await load()
}
",
        );
        let error = rejects(
            "\
async fn load() -> Int {
  1
}

async fn run() -> Int {
  load()
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `Task<Int>`");
    }

    #[test]
    fn rejects_awaiting_something_that_is_not_a_task() {
        let error = rejects_body("  let n = await 1");
        assert_eq!(error.code, AWAIT_OPERAND);
        assert_eq!(error.message, "`await` needs a task, but found `Int`");
        assert_eq!(
            error.help.unwrap(),
            "call an `async fn`, or spawn the work into a task scope, and await that handle"
        );
    }

    #[test]
    fn rejects_the_question_mark_on_a_task() {
        let error = rejects(
            "\
async fn load() -> Result<Int, Error> {
  Ok(1)
}

async fn run() -> Result<Int, Error> {
  let value = load()?
  Ok(value)
}
",
        );
        assert_eq!(error.code, TRY_OPERAND);
        assert_eq!(
            error.message,
            "`?` needs a `Result` or an `Option`, but found `Task<Result<Int, Error>>`"
        );
        assert_eq!(
            error.help.unwrap(),
            "settle the task first, as in `task.await()?`"
        );
    }

    #[test]
    fn a_scope_spawns_tasks_that_carry_the_block_s_value() {
        accepts(
            "\
async fn run() -> Result<Int, Error> {
  scope tasks {
    let first = tasks.spawn { 1 }
    let value = await first
    Ok(value)
  }
}
",
        );
    }

    // ------------------------------------------------- aliases

    #[test]
    fn expands_a_type_alias() {
        accepts(
            "\
type Transform = fn(Int) -> Int

fn apply(value: Int, transform: Transform) -> Int {
  transform(value)
}

fn run() -> Int {
  apply(1, fn(n) { n + 1 })
}
",
        );
        let error = rejects(
            "\
type Transform = fn(Int) -> Int

fn apply(value: Int, transform: Transform) -> Int {
  transform(value)
}

fn run() -> Int {
  apply(1, fn(n) { \"{n}\" })
}
",
        );
        assert_eq!(error.code, MISMATCH);
        assert_eq!(error.message, "expected `Int`, found `String`");
    }

    #[test]
    fn rejects_a_type_alias_that_expands_to_itself() {
        let error = rejects("type Loop = Loop\n\nfn run(value: Loop) {\n}\n");
        assert_eq!(error.code, ALIAS_CYCLE);
        assert_eq!(error.message, "`Loop` expands to itself");
        assert_eq!(
            error.rule.unwrap(),
            "A type alias names an existing type; it cannot be defined in terms of itself."
        );
    }

    // ------------------------------------------------- abstention

    #[test]
    fn a_host_call_is_unknown_and_suppresses_what_follows() {
        // `console.println` has no schema, so nothing it returns can be
        // checked — and nothing derived from it is wrongly reported either.
        accepts(
            "\
use console.println
use env.get

export fn main() -> Result<Unit, Error> {
  let value = env.get(\"PORT\").unwrapOr(\"8080\")
  println(\"{value + 1}\")?
  Ok(())
}
",
        );
    }

    #[test]
    fn a_host_type_warns_rather_than_failing() {
        let warnings = warnings_of(
            "\
use http

fn handle(request: http.Request) -> Int {
  1
}
",
        );
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, HOST_TYPE);
        assert_eq!(
            warnings[0].message,
            "`http.Request` is a host type, so values of it are unchecked"
        );
        assert_eq!(
            warnings[0].rule.as_deref().unwrap(),
            "A Host API's types come from its schema, and there is no schema yet."
        );
    }

    #[test]
    fn a_capitalized_name_no_module_declares_warns_rather_than_failing() {
        let warnings = warnings_of("fn run() -> Int {\n  Shared(1)\n  1\n}\n");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, UNRESOLVED_NAME);
        assert_eq!(
            warnings[0].message,
            "`Shared` is not declared in this module, so it is unchecked"
        );
    }

    #[test]
    fn a_lowercase_name_nothing_declares_is_an_error() {
        let error = rejects("fn run() -> Int {\n  total\n}\n");
        assert_eq!(error.code, UNKNOWN_NAME);
        assert_eq!(error.message, "cannot find `total` in this scope");
        assert_eq!(
            error.rule.unwrap(),
            "A name must be a local binding, a parameter, a declaration of this module, or a `use`d host item."
        );
        assert_eq!(
            error.help.unwrap(),
            "declare `let total = ...` before this expression, or `use <host>.total`"
        );
    }

    #[test]
    fn an_unknown_type_name_warns_rather_than_failing() {
        let warnings = warnings_of("fn run(value: Missing) -> Int {\n  1\n}\n");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code, UNKNOWN_TYPE);
        assert_eq!(
            warnings[0].message,
            "`Missing` names no type this module declares"
        );
    }

    #[test]
    fn an_error_inside_an_unchecked_call_is_still_reported() {
        // Abstaining about the callee never means abstaining about the
        // arguments.
        let error = rejects(
            "\
use console.println

export fn main() -> Result<Unit, Error> {
  println(1 + 1.0)?
  Ok(())
}
",
        );
        assert_eq!(error.code, OPERATOR);
    }

    // ------------------------------------------------- entry shape

    fn config_with_entry(entry: &str) -> Config {
        let mut runs = BTreeMap::new();
        runs.insert(
            "run".to_string(),
            crate::config::RunConfig {
                entry: entry.to_string(),
                allow: Vec::new(),
                fuel: None,
                deadline: None,
                max_host_calls: None,
                trace: None,
            },
        );
        Config {
            runs,
            ..Config::default()
        }
    }

    #[track_caller]
    fn entry_errors(source: &str) -> Vec<Diagnostic> {
        diagnostics_with(source, config_with_entry("main.main"))
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .collect()
    }

    #[test]
    fn accepts_both_entry_shapes() {
        assert!(
            entry_errors("export fn main() -> Result<Unit, Error> {\n  Ok(())\n}\n").is_empty()
        );
        assert!(entry_errors(
            "export fn main(args: Array<String>) -> Result<Unit, Error> {\n  Ok(())\n}\n"
        )
        .is_empty());
        assert!(entry_errors("export fn main() {\n}\n").is_empty());
    }

    #[test]
    fn rejects_an_entry_with_two_parameters() {
        let errors = entry_errors(
            "export fn main(args: Array<String>, extra: Int) -> Result<Unit, Error> {\n  Ok(())\n}\n",
        );
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, ENTRY);
        assert_eq!(errors[0].message, "entry `main.main` declares 2 parameters");
        assert_eq!(
            errors[0].rule.as_deref().unwrap(),
            "An entry function takes either no parameters or one `Array<String>` of process arguments."
        );
        assert_eq!(
            errors[0].help.as_deref().unwrap(),
            "write `fn main()` or `fn main(args: Array<String>)`"
        );
    }

    #[test]
    fn rejects_an_entry_whose_parameter_is_not_the_process_arguments() {
        let errors =
            entry_errors("export fn main(count: Int) -> Result<Unit, Error> {\n  Ok(())\n}\n");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, ENTRY);
        assert_eq!(
            errors[0].message,
            "entry `main.main` takes `Int`, but the host passes `Array<String>`"
        );
        assert_eq!(
            errors[0].help.as_deref().unwrap(),
            "write `fn main(args: Array<String>)`"
        );
    }

    #[test]
    fn rejects_an_entry_whose_result_the_host_cannot_report() {
        let errors = entry_errors("export fn main() -> Int {\n  1\n}\n");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].code, ENTRY);
        assert_eq!(
            errors[0].message,
            "entry `main.main` returns `Int`, which the host cannot report"
        );
        assert_eq!(
            errors[0].rule.as_deref().unwrap(),
            "The host reports an entry's failure through its `Err`, so an entry returns `()` or a `Result`."
        );
    }

    // --------------------------------------------- the whole repository

    /// Every program in the repository, checked the way the CLI checks one.
    ///
    /// A package whose directory name begins with `fail_` exists to pin a
    /// failure, so it must fail; every other package must check with no
    /// errors at all. That is the acceptance bar for this pass, and it keeps
    /// itself honest: a new example or end-to-end case joins it by existing.
    #[test]
    fn every_program_in_the_repository_checks() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut packages = vec![root.join("examples"), root.join("tests/e2e")];
        let mut nested: Vec<PathBuf> = std::fs::read_dir(root.join("tests/e2e"))
            .expect("the end-to-end suite exists")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.join("cove.toml").is_file())
            .collect();
        nested.sort();
        packages.append(&mut nested);
        assert!(
            packages.len() > 8,
            "expected to find every package, found {packages:?}"
        );

        let mut failures = Vec::new();
        for directory in &packages {
            let name = directory
                .file_name()
                .expect("a package directory has a name")
                .to_string_lossy()
                .into_owned();
            let must_fail = name.starts_with("fail_");

            let mut sources = SourceMap::new();
            let package = match crate::package::load(directory, &mut sources) {
                Ok(package) => package,
                Err(diagnostics) => {
                    if !must_fail {
                        failures.push(format!(
                            "{name}: does not load: {}",
                            render_all(&sources, &diagnostics)
                        ));
                    }
                    continue;
                }
            };
            let program = match resolve(&package) {
                Ok(program) => program,
                Err(diagnostics) => {
                    if !must_fail {
                        failures.push(format!(
                            "{name}: does not resolve: {}",
                            render_all(&sources, &diagnostics)
                        ));
                    }
                    continue;
                }
            };
            let errors: Vec<Diagnostic> = check(&package, &program)
                .into_iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            match (must_fail, errors.is_empty()) {
                (false, false) => failures.push(format!(
                    "{name}: does not type-check: {}",
                    render_all(&sources, &errors)
                )),
                (true, true) => {
                    failures.push(format!("{name}: was expected to fail, but it checks"))
                }
                _ => {}
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    fn render_all(sources: &SourceMap, diagnostics: &[Diagnostic]) -> String {
        diagnostics
            .iter()
            .map(|d| cove_diag::render(sources, d))
            .collect::<Vec<_>>()
            .join("")
    }
}
