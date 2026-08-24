//! Tokens produced by the Cove lexer.

use cove_diag::Span;

/// A lexed token.
#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    /// True when a line break separates this token from the previous one.
    ///
    /// Newlines are not tokens; the parser reads this flag only where a
    /// statement could end, so most parsing routines never see line breaks.
    /// Line breaks hidden inside a `//` or `/* */` comment count as well, so
    /// commenting out the tail of a line cannot join two statements.
    pub preceded_by_newline: bool,
}

/// Keywords recognised by the MVP grammar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keyword {
    Async,
    Await,
    Else,
    Enum,
    Export,
    Fn,
    For,
    If,
    Impl,
    In,
    Let,
    Match,
    Return,
    Scope,
    SelfValue,
    Struct,
    Trait,
    Type,
    Use,
    Var,
    While,
}

impl Keyword {
    pub fn from_text(text: &str) -> Option<Keyword> {
        Some(match text {
            "async" => Keyword::Async,
            "await" => Keyword::Await,
            "else" => Keyword::Else,
            "enum" => Keyword::Enum,
            "export" => Keyword::Export,
            "fn" => Keyword::Fn,
            "for" => Keyword::For,
            "if" => Keyword::If,
            "impl" => Keyword::Impl,
            "in" => Keyword::In,
            "let" => Keyword::Let,
            "match" => Keyword::Match,
            "return" => Keyword::Return,
            "scope" => Keyword::Scope,
            "self" => Keyword::SelfValue,
            "struct" => Keyword::Struct,
            "trait" => Keyword::Trait,
            "type" => Keyword::Type,
            "use" => Keyword::Use,
            "var" => Keyword::Var,
            "while" => Keyword::While,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Keyword::Async => "async",
            Keyword::Await => "await",
            Keyword::Else => "else",
            Keyword::Enum => "enum",
            Keyword::Export => "export",
            Keyword::Fn => "fn",
            Keyword::For => "for",
            Keyword::If => "if",
            Keyword::Impl => "impl",
            Keyword::In => "in",
            Keyword::Let => "let",
            Keyword::Match => "match",
            Keyword::Return => "return",
            Keyword::Scope => "scope",
            Keyword::SelfValue => "self",
            Keyword::Struct => "struct",
            Keyword::Trait => "trait",
            Keyword::Type => "type",
            Keyword::Use => "use",
            Keyword::Var => "var",
            Keyword::While => "while",
        }
    }
}

/// One piece of a string literal.
///
/// `"Hello, {name}!"` lexes to `[Text("Hello, "), Interpolation("name"), Text("!")]`.
#[derive(Clone, Debug, PartialEq)]
pub enum StringPart {
    /// Literal text with escape sequences already resolved.
    Text(String),
    /// Source text between `{` and `}`, re-parsed as an expression by the parser.
    Interpolation { source: String, span: Span },
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    Ident(String),
    Keyword(Keyword),
    /// `true` / `false`.
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A duration literal such as `500ms` or `5s`, normalised to nanoseconds.
    Duration(i64),
    Str(Vec<StringPart>),
    /// `/// text` attached to the following declaration.
    DocComment(String),

    // Delimiters
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,

    // Punctuation
    Comma,
    Colon,
    Dot,
    DotDot,
    DotDotLt,
    Ellipsis,
    Arrow,
    FatArrow,
    Question,
    Underscore,

    // Operators
    Eq,
    EqEq,
    Bang,
    BangEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    AmpAmp,
    PipePipe,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,

    Eof,
}

impl TokenKind {
    /// A short human-readable name used in diagnostics.
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Ident(name) => format!("identifier `{name}`"),
            TokenKind::Keyword(k) => format!("keyword `{}`", k.as_str()),
            TokenKind::Bool(b) => format!("`{b}`"),
            TokenKind::Int(_) => "integer literal".into(),
            TokenKind::Float(_) => "float literal".into(),
            TokenKind::Duration(_) => "duration literal".into(),
            TokenKind::Str(_) => "string literal".into(),
            TokenKind::DocComment(_) => "doc comment".into(),
            TokenKind::Eof => "end of file".into(),
            other => format!("`{}`", other.symbol().unwrap_or("?")),
        }
    }

    /// The literal spelling of a punctuation or operator token.
    pub fn symbol(&self) -> Option<&'static str> {
        Some(match self {
            TokenKind::LParen => "(",
            TokenKind::RParen => ")",
            TokenKind::LBrace => "{",
            TokenKind::RBrace => "}",
            TokenKind::LBracket => "[",
            TokenKind::RBracket => "]",
            TokenKind::Comma => ",",
            TokenKind::Colon => ":",
            TokenKind::Dot => ".",
            TokenKind::DotDot => "..",
            TokenKind::DotDotLt => "..<",
            TokenKind::Ellipsis => "...",
            TokenKind::Arrow => "->",
            TokenKind::FatArrow => "=>",
            TokenKind::Question => "?",
            TokenKind::Underscore => "_",
            TokenKind::Eq => "=",
            TokenKind::EqEq => "==",
            TokenKind::Bang => "!",
            TokenKind::BangEq => "!=",
            TokenKind::Lt => "<",
            TokenKind::LtEq => "<=",
            TokenKind::Gt => ">",
            TokenKind::GtEq => ">=",
            TokenKind::Plus => "+",
            TokenKind::Minus => "-",
            TokenKind::Star => "*",
            TokenKind::Slash => "/",
            TokenKind::Percent => "%",
            TokenKind::AmpAmp => "&&",
            TokenKind::PipePipe => "||",
            TokenKind::PlusEq => "+=",
            TokenKind::MinusEq => "-=",
            TokenKind::StarEq => "*=",
            TokenKind::SlashEq => "/=",
            TokenKind::PercentEq => "%=",
            _ => return None,
        })
    }
}
