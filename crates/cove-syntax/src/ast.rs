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
    pub kind: ItemKind,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum ItemKind {
    Fn(FnDecl),
    Struct(StructDecl),
    Enum(EnumDecl),
    Impl(ImplBlock),
    /// `export type Handler = async fn(...) -> ...`
    TypeAlias(TypeAlias),
}

#[derive(Clone, Debug)]
pub struct FnDecl {
    pub name: Ident,
    pub is_async: bool,
    pub generics: Vec<Ident>,
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
    pub generics: Vec<Ident>,
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
    pub generics: Vec<Ident>,
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

#[derive(Clone, Debug)]
pub struct ImplBlock {
    pub type_name: Ident,
    pub generics: Vec<Ident>,
    pub items: Vec<Item>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub struct TypeAlias {
    pub name: Ident,
    pub generics: Vec<Ident>,
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
    /// `()`
    Unit,
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
