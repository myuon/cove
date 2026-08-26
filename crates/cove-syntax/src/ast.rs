//! The Cove abstract syntax tree.
//!
//! The tree mirrors the surface language described by `docs/LANGUAGE_CARD.md`.
//! Where the Language Card and ADR 0001 disagree, the Language Card wins.

use cove_diag::{Span, Spanned};

pub type Ident = Spanned<String>;

/// One `.cove` file. Every file in a directory is an implementation unit of the
/// same module.
#[derive(Clone, Debug)]
pub struct SourceUnit {
    pub uses: Vec<Use>,
    pub items: Vec<Item>,
    pub span: Span,
}

/// `use console.println` or `use http`.
#[derive(Clone, Debug)]
pub struct Use {
    pub path: Vec<Ident>,
    pub span: Span,
}

/// A top-level declaration.
#[derive(Clone, Debug)]
pub struct Item {
    pub doc: Option<String>,
    pub exported: bool,
    /// `test fn name() -> Result<Unit, Error>`: a declaration the test
    /// runner calls and nothing else does.
    ///
    /// `test` occupies the position `export` occupies and says the
    /// comparable thing — who may call this — so a declaration carries at
    /// most one of the two, and only a `fn` carries `test` at all.
    pub is_test: bool,
    /// `export opaque struct User { ... }`: the export publishes the type's
    /// name and its exported methods, and nothing about how it is built.
    ///
    /// `opaque` narrows an `export` rather than standing in for one, so it
    /// only ever appears together with one, and only on a struct: exporting
    /// an enum exports its cases, because a `match` over them is what the
    /// enum is for. See ADR 0014.
    pub is_opaque: bool,
    pub kind: ItemKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ItemKind {
    Fn(FnDecl),
    Struct(StructDecl),
    Enum(EnumDecl),
    /// `trait Display { fn describe(self) -> String }`
    Trait(TraitDecl),
    Impl(ImplBlock),
    /// `export type Handler = async fn(...) -> ...`
    TypeAlias(TypeAlias),
}

/// One type parameter of a declaration, with the traits it is bounded by.
///
/// A bound is checked at the call site that instantiates the parameter, and
/// it is what makes a method call on a value of that parameter resolvable.
#[derive(Clone, Debug)]
pub struct GenericParam {
    pub name: Ident,
    /// `T: Display + Ordered` binds two traits; an unbounded `T` binds none.
    pub bounds: Vec<Ident>,
    pub span: Span,
}

impl GenericParam {
    /// An unbounded parameter, which is what every generic was before traits.
    pub fn unbounded(name: Ident) -> GenericParam {
        let span = name.span;
        GenericParam {
            name,
            bounds: Vec::new(),
            span,
        }
    }
}

/// Renders a type parameter back to declaration syntax: `T` or `T: A + B`.
impl std::fmt::Display for GenericParam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name.node)?;
        for (i, bound) in self.bounds.iter().enumerate() {
            f.write_str(if i == 0 { ": " } else { " + " })?;
            f.write_str(&bound.node)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct FnDecl {
    pub name: Ident,
    pub is_async: bool,
    pub generics: Vec<GenericParam>,
    /// `self` / `var self`, present on methods only.
    pub receiver: Option<Receiver>,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
    pub body: Block,
    pub span: Span,
}

/// The `self` parameter of a method.
#[derive(Clone, Copy, Debug)]
pub struct Receiver {
    /// `var self` declares a mutating receiver.
    pub is_var: bool,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Param {
    /// A `var` parameter is a non-escaping inout alias, marked at both the
    /// declaration and the call site.
    pub is_var: bool,
    pub name: Ident,
    pub ty: Option<Type>,
    /// `items: T...` is an immutable `Array<T>` inside the function.
    pub variadic: bool,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct StructDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub fields: Vec<Field>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Field {
    pub doc: Option<String>,
    pub name: Ident,
    pub ty: Type,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct EnumDecl {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub cases: Vec<EnumCase>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct EnumCase {
    pub doc: Option<String>,
    pub name: Ident,
    /// `InvalidPort(String)` carries positional payload types.
    pub payload: Vec<Type>,
    pub span: Span,
}

/// A trait: a set of method signatures a type conforms to explicitly.
///
/// Conformance is only ever declared by an `impl Trait for Type` block; there
/// is no structural conformance and no blanket implementation.
#[derive(Clone, Debug)]
pub struct TraitDecl {
    pub name: Ident,
    pub methods: Vec<TraitMethod>,
    pub span: Span,
}

/// One method signature a trait declares, with an optional default body.
///
/// A method with a default body is supplied by every conformance that does
/// not override it; one without must be supplied by every conformance.
#[derive(Clone, Debug)]
pub struct TraitMethod {
    pub doc: Option<String>,
    pub name: Ident,
    pub is_async: bool,
    /// `self` / `var self`. A method without one is an associated function,
    /// which has no receiver and so cannot be called through `dyn Trait`.
    pub receiver: Option<Receiver>,
    pub params: Vec<Param>,
    pub return_type: Option<Type>,
    pub default: Option<Block>,
    pub span: Span,
}

/// `impl Type { ... }`, or `impl Trait for Type { ... }` when `trait_name` is
/// present.
#[derive(Clone, Debug)]
pub struct ImplBlock {
    /// The trait this block declares a conformance to, for `impl Trait for
    /// Type`.
    pub trait_name: Option<Ident>,
    pub type_name: Ident,
    pub generics: Vec<GenericParam>,
    pub items: Vec<Item>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TypeAlias {
    pub name: Ident,
    pub generics: Vec<GenericParam>,
    pub ty: Type,
    pub span: Span,
}

/// A type expression.
#[derive(Clone, Debug)]
pub struct Type {
    pub kind: TypeKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum TypeKind {
    /// `Int`, `Array<T>`, `http.Request`, `Result<T, E>`.
    Named { path: Vec<Ident>, args: Vec<Type> },
    /// `async fn(request: http.Request) -> Result<http.Response, Error>`
    Fn {
        is_async: bool,
        params: Vec<Param>,
        return_type: Option<Box<Type>>,
    },
    /// `dyn Display`: a value of any type that conforms to the trait,
    /// carrying its implementation with it.
    Dyn(Ident),
    /// `()`
    Unit,
}

/// Renders a type back to the Cove source form it would be written in, so
/// tooling such as `cove outline` can show a typed interface without the
/// user writing it twice.
impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            TypeKind::Unit => write!(f, "()"),
            TypeKind::Dyn(name) => write!(f, "dyn {}", name.node),
            TypeKind::Named { path, args } => {
                let path = path
                    .iter()
                    .map(|segment| segment.node.as_str())
                    .collect::<Vec<_>>()
                    .join(".");
                write!(f, "{path}")?;
                if !args.is_empty() {
                    let args = args
                        .iter()
                        .map(|arg| arg.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    write!(f, "<{args}>")?;
                }
                Ok(())
            }
            TypeKind::Fn {
                is_async,
                params,
                return_type,
            } => {
                if *is_async {
                    write!(f, "async ")?;
                }
                let params = params
                    .iter()
                    .map(|param| param.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "fn({params})")?;
                if let Some(return_type) = return_type {
                    write!(f, " -> {return_type}")?;
                }
                Ok(())
            }
        }
    }
}

/// Renders a parameter back to declaration syntax: `[var ]name: Type[...]`.
/// A parameter with no type (a lambda parameter) prints just its name, and a
/// function-type parameter with no name (the parser's convention for a bare
/// type in a `fn(...)` type) prints just its type.
impl std::fmt::Display for Param {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Some(ty) = &self.ty else {
            return write!(f, "{}", self.name.node);
        };
        if self.is_var {
            write!(f, "var ")?;
        }
        if !self.name.node.is_empty() {
            write!(f, "{}: ", self.name.node)?;
        }
        write!(f, "{ty}")?;
        if self.variadic {
            write!(f, "...")?;
        }
        // A default is part of the signature: adding one is a compatible
        // change, and removing one is not, so a rendering that dropped it
        // could not tell the two apart.
        if let Some(default) = &self.default {
            write!(f, " = {}", crate::format::format_expr(default))?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Block {
    pub statements: Vec<Stmt>,
    /// The last expression in a block is its value.
    pub tail: Option<Box<Expr>>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    /// `let name: T = expr` / `var name = expr`
    Let {
        is_var: bool,
        name: Ident,
        ty: Option<Type>,
        value: Expr,
    },
    Expr(Expr),
    /// A nested declaration, such as a local `fn`.
    Item(Box<Item>),
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

/// One argument at a call site.
#[derive(Clone, Debug)]
pub struct Arg {
    /// Static argument labels are parameter names and part of the API contract.
    pub label: Option<Ident>,
    /// `fill(var output)` marks an inout alias at the call site.
    pub is_var: bool,
    /// `...array` spreads into a variadic parameter.
    pub spread: bool,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Bool(bool),
    Duration(i64),
    /// A string literal with its interpolated expressions already parsed.
    Str(Vec<StrPart>),
    /// `()`
    Unit,
    /// A bare name.
    Ident(String),
    /// `[1, 2]` produces an immutable `Array`.
    ArrayLit(Vec<Expr>),
    /// `console.println`, `LogLevel.Debug`, `self.status`
    Field {
        base: Box<Expr>,
        name: Ident,
    },
    /// `f(a, b: c)`; also struct initialization via synthesized labeled calls.
    Call {
        callee: Box<Expr>,
        generics: Vec<Type>,
        args: Vec<Arg>,
        /// `clock.timeout(500ms) { ... }` and `tasks.spawn { ... }`.
        trailing: Option<Box<Expr>>,
    },
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// `place = value`, `place += value`
    Assign {
        op: Option<BinaryOp>,
        target: Box<Expr>,
        value: Box<Expr>,
    },
    /// `expr?` returns the error from the current function.
    Try(Box<Expr>),
    Await(Box<Expr>),
    Block(Block),
    If {
        condition: Box<Expr>,
        then_branch: Block,
        else_branch: Option<Box<Expr>>,
    },
    /// `match` must cover every enum case.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    /// A loop is an expression: it evaluates to `Unit` unless a `break expr`
    /// inside it says otherwise.
    For {
        binding: Ident,
        iterable: Box<Expr>,
        body: Block,
    },
    While {
        condition: Box<Expr>,
        body: Block,
    },
    Return(Option<Box<Expr>>),
    /// `break` / `break expr`. Exits the nearest enclosing loop, which then
    /// evaluates to `Unit` or to `expr`. Resolve rejects this outside a loop.
    Break(Option<Box<Expr>>),
    /// `continue`. Skips to the next iteration of the nearest enclosing loop.
    /// Resolve rejects this outside a loop.
    Continue,
    /// `fn(x) { ... }` / `async fn(x) { ... }`
    Lambda {
        is_async: bool,
        params: Vec<Param>,
        body: Block,
    },
    /// `scope tasks { ... }`
    Scope {
        name: Ident,
        body: Block,
    },
    /// `0..<attempts` and `0..n`
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
        inclusive_end: bool,
    },
}

/// A resolved piece of a string literal.
#[derive(Clone, Debug)]
pub enum StrPart {
    Text(String),
    Interpolation(Expr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// `a is b`: shared-storage identity, for the handful of types that have
    /// one. Same precedence as `==`; see the Language Card.
    Is,
    And,
    Or,
}

#[derive(Clone, Debug)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub body: Expr,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum PatternKind {
    /// `_`
    Wildcard,
    /// `other` — binds the scrutinee.
    Binding(String),
    /// `"debug"`, `1`, `true`
    Literal(Expr),
    /// `Ok(value)`, `LogLevel.Debug`, `ConfigError.InvalidPort(raw)`
    Variant {
        path: Vec<Ident>,
        payload: Vec<Pattern>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span() -> Span {
        Span::new(cove_diag::FileId(0), 0, 0)
    }

    fn ident(name: &str) -> Ident {
        Spanned::new(name.to_string(), span())
    }

    fn named(path: &[&str], args: Vec<Type>) -> Type {
        Type {
            kind: TypeKind::Named {
                path: path.iter().map(|s| ident(s)).collect(),
                args,
            },
            span: span(),
        }
    }

    fn unit() -> Type {
        Type {
            kind: TypeKind::Unit,
            span: span(),
        }
    }

    fn param(name: &str, ty: Option<Type>) -> Param {
        Param {
            is_var: false,
            name: ident(name),
            ty,
            variadic: false,
            default: None,
            span: span(),
        }
    }

    #[test]
    fn displays_a_plain_named_type() {
        assert_eq!(named(&["Int"], vec![]).to_string(), "Int");
    }

    #[test]
    fn displays_a_named_type_with_one_argument() {
        assert_eq!(
            named(&["Array"], vec![named(&["String"], vec![])]).to_string(),
            "Array<String>"
        );
    }

    #[test]
    fn displays_a_dotted_path() {
        assert_eq!(
            named(&["http", "Request"], vec![]).to_string(),
            "http.Request"
        );
    }

    #[test]
    fn displays_a_named_type_with_two_arguments() {
        assert_eq!(
            named(
                &["Result"],
                vec![named(&["Unit"], vec![]), named(&["Error"], vec![])]
            )
            .to_string(),
            "Result<Unit, Error>"
        );
    }

    #[test]
    fn displays_nested_generics() {
        let ty = named(
            &["Map"],
            vec![
                named(&["String"], vec![]),
                named(&["Array"], vec![named(&["EventHandler"], vec![])]),
            ],
        );
        assert_eq!(ty.to_string(), "Map<String, Array<EventHandler>>");
    }

    #[test]
    fn displays_unit() {
        assert_eq!(unit().to_string(), "()");
    }

    #[test]
    fn displays_a_fn_type_with_named_params_and_return_type() {
        let ty = Type {
            kind: TypeKind::Fn {
                is_async: false,
                params: vec![
                    param("name", Some(named(&["Type"], vec![]))),
                    param("other", Some(named(&["Type"], vec![]))),
                ],
                return_type: Some(Box::new(named(&["Ret"], vec![]))),
            },
            span: span(),
        };
        assert_eq!(ty.to_string(), "fn(name: Type, other: Type) -> Ret");
    }

    #[test]
    fn displays_an_async_fn_type() {
        let ty = Type {
            kind: TypeKind::Fn {
                is_async: true,
                params: vec![],
                return_type: None,
            },
            span: span(),
        };
        assert_eq!(ty.to_string(), "async fn()");
    }

    #[test]
    fn displays_a_fn_type_with_no_return_type() {
        let ty = Type {
            kind: TypeKind::Fn {
                is_async: false,
                params: vec![param("x", Some(named(&["Int"], vec![])))],
                return_type: None,
            },
            span: span(),
        };
        assert_eq!(ty.to_string(), "fn(x: Int)");
    }

    #[test]
    fn displays_an_unnamed_fn_type_param() {
        let ty = Type {
            kind: TypeKind::Fn {
                is_async: false,
                params: vec![param("", Some(named(&["String"], vec![])))],
                return_type: None,
            },
            span: span(),
        };
        assert_eq!(ty.to_string(), "fn(String)");
    }

    #[test]
    fn displays_a_var_and_variadic_fn_type_param() {
        let mut p = param("items", Some(named(&["Int"], vec![])));
        p.is_var = true;
        p.variadic = true;
        let ty = Type {
            kind: TypeKind::Fn {
                is_async: false,
                params: vec![p],
                return_type: None,
            },
            span: span(),
        };
        assert_eq!(ty.to_string(), "fn(var items: Int...)");
    }

    #[test]
    fn displays_a_lambda_param_with_no_type() {
        assert_eq!(param("x", None).to_string(), "x");
    }
}

#[cfg(test)]
mod param_tests {
    use super::*;
    use cove_diag::SourceMap;

    fn parse_one(source: &str) -> SourceUnit {
        let mut sources = SourceMap::new();
        let file = sources.add("test.cove", source.to_string());
        crate::parse_file(&sources, file).expect("source parses")
    }

    fn signature(source: &str) -> String {
        let unit = parse_one(source);
        let ItemKind::Fn(decl) = &unit.items[0].kind else {
            panic!("expected a function");
        };
        decl.params
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }

    #[test]
    fn a_parameter_renders_its_default() {
        assert_eq!(
            signature("export fn f(name: String = \"world\") {\n}\n"),
            "name: String = \"world\""
        );
        assert_eq!(
            signature("export fn f(count: Int = 1) {\n}\n"),
            "count: Int = 1"
        );
    }

    #[test]
    fn a_parameter_without_a_default_renders_without_one() {
        assert_eq!(
            signature("export fn f(name: String) {\n}\n"),
            "name: String"
        );
    }

    #[test]
    fn var_and_variadic_survive_alongside_a_default() {
        assert_eq!(
            signature("export fn f(var out: Int) {\n}\n"),
            "var out: Int"
        );
        assert_eq!(
            signature("export fn f(items: Int...) {\n}\n"),
            "items: Int..."
        );
    }
}
