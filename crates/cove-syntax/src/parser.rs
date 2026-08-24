//! The Cove parser.
//!
//! Turns a token stream into an [`ast::SourceUnit`]. Cove has no statement
//! terminators: `;` is not part of the language and newlines are not
//! significant, so item and statement boundaries follow from the grammar
//! alone and the last expression of a block is that block's value.
//!
//! Parsing never stops at the first error. Every diagnostic is collected and
//! the parser resynchronises at the next plausible declaration or statement,
//! so a single run reports as many independent problems as it can find.

use cove_diag::{Diagnostic, FileId, SourceMap, Span, Spanned};

use crate::ast::*;
use crate::lexer;
use crate::token::{Keyword, StringPart, Token, TokenKind};

/// Parses `tokens`, which must be the token stream lexed from `file`.
///
/// Returns every diagnostic found in the file rather than only the first.
pub fn parse(
    sources: &SourceMap,
    file: FileId,
    tokens: Vec<Token>,
) -> Result<SourceUnit, Vec<Diagnostic>> {
    let mut parser = Parser::new(sources, file, tokens);
    let unit = parser.parse_source_unit();
    if parser.diagnostics.is_empty() {
        Ok(unit)
    } else {
        Err(parser.diagnostics)
    }
}

/// Signals that a diagnostic was recorded and the current construct was
/// abandoned. Recovery happens at the nearest declaration or statement.
struct Bail;

type PResult<T> = Result<T, Bail>;

struct Parser<'a> {
    sources: &'a SourceMap,
    file: FileId,
    tokens: Vec<Token>,
    pos: usize,
    diagnostics: Vec<Diagnostic>,
    /// Set while parsing the header expression of `if`, `while`, `for`,
    /// `match`, or `scope`, where a following `{` opens the body instead of a
    /// trailing closure.
    no_trailing_closure: bool,
}

fn expr(kind: ExprKind, span: Span) -> Expr {
    Expr { kind, span }
}

/// A place expression names storage: a variable or a field of a place.
fn is_place_expr(target: &Expr) -> bool {
    match &target.kind {
        ExprKind::Ident(_) => true,
        ExprKind::Field { base, .. } => is_place_expr(base),
        _ => false,
    }
}

/// Only a callee-shaped expression can take a braced trailing closure, so
/// `tasks.spawn { ... }` is a call while `[1, 2] { ... }` is not.
fn can_take_trailing_closure(callee: &Expr) -> bool {
    matches!(
        callee.kind,
        ExprKind::Ident(_) | ExprKind::Field { .. } | ExprKind::Call { trailing: None, .. }
    )
}

fn rebase_span(span: Span, file: FileId, offset: u32) -> Span {
    Span::new(file, offset + span.start, offset + span.end)
}

/// Moves a token lexed out of an interpolation's source text back onto the
/// file that contains the string literal.
fn rebase_token(mut token: Token, file: FileId, offset: u32) -> Token {
    token.span = rebase_span(token.span, file, offset);
    if let TokenKind::Str(parts) = &mut token.kind {
        for part in parts {
            if let StringPart::Interpolation { span, .. } = part {
                *span = rebase_span(*span, file, offset);
            }
        }
    }
    token
}

fn rebase_diagnostic(mut diagnostic: Diagnostic, file: FileId, offset: u32) -> Diagnostic {
    if let Some(span) = diagnostic.primary {
        diagnostic.primary = Some(rebase_span(span, file, offset));
    }
    for label in &mut diagnostic.labels {
        label.span = rebase_span(label.span, file, offset);
    }
    diagnostic
}

impl<'a> Parser<'a> {
    fn new(sources: &'a SourceMap, file: FileId, tokens: Vec<Token>) -> Self {
        let tokens = if tokens.is_empty() {
            vec![Token {
                kind: TokenKind::Eof,
                span: Span::new(file, 0, 0),
            }]
        } else {
            tokens
        };
        Parser {
            sources,
            file,
            tokens,
            pos: 0,
            diagnostics: Vec::new(),
            no_trailing_closure: false,
        }
    }

    fn peek(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    fn peek_at(&self, offset: usize) -> &TokenKind {
        let index = (self.pos + offset).min(self.tokens.len() - 1);
        &self.tokens[index].kind
    }

    fn span(&self) -> Span {
        self.tokens[self.pos].span
    }

    /// The span of the token most recently consumed, used to close the span of
    /// a node that ends at the current position.
    fn prev_span(&self) -> Span {
        self.tokens[self.pos.saturating_sub(1)].span
    }

    fn is_eof(&self) -> bool {
        matches!(self.peek(), TokenKind::Eof)
    }

    fn bump(&mut self) -> Token {
        let token = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        token
    }

    fn at(&self, kind: &TokenKind) -> bool {
        self.peek() == kind
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.at(kind) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn at_keyword(&self, keyword: Keyword) -> bool {
        matches!(self.peek(), TokenKind::Keyword(found) if *found == keyword)
    }

    fn eat_keyword(&mut self, keyword: Keyword) -> bool {
        if self.at_keyword(keyword) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn error(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    fn unexpected(&mut self, expected: &str) -> Bail {
        let found = self.peek().describe();
        let span = self.span();
        self.error(
            Diagnostic::error(
                "cove::parse::unexpected_token",
                format!("expected {expected}, found {found}"),
            )
            .at(span),
        );
        Bail
    }

    fn expect(&mut self, kind: &TokenKind, expected: &str) -> PResult<Token> {
        if self.at(kind) {
            Ok(self.bump())
        } else {
            Err(self.unexpected(expected))
        }
    }

    fn expect_keyword(&mut self, keyword: Keyword, expected: &str) -> PResult<Span> {
        if self.at_keyword(keyword) {
            Ok(self.bump().span)
        } else {
            Err(self.unexpected(expected))
        }
    }

    fn expect_ident(&mut self) -> PResult<Ident> {
        let span = self.span();
        match self.peek() {
            TokenKind::Ident(name) => {
                let name = name.clone();
                self.bump();
                Ok(Spanned::new(name, span))
            }
            _ => Err(self.unexpected("identifier")),
        }
    }

    /// After `.`, a keyword names an ordinary member: `task.await()` reads the
    /// member `await`, not the `await` operator.
    fn expect_member_name(&mut self) -> PResult<Ident> {
        let span = self.span();
        let name = match self.peek() {
            TokenKind::Ident(name) => name.clone(),
            TokenKind::Keyword(keyword) => keyword.as_str().to_string(),
            _ => return Err(self.unexpected("a field or method name")),
        };
        self.bump();
        Ok(Spanned::new(name, span))
    }

    /// Runs `parse` with the trailing-closure rule temporarily set. Header
    /// expressions forbid trailing closures; everything nested inside
    /// parentheses, brackets, or braces allows them again.
    fn scoped<T>(&mut self, no_trailing_closure: bool, parse: impl FnOnce(&mut Self) -> T) -> T {
        let saved = self.no_trailing_closure;
        self.no_trailing_closure = no_trailing_closure;
        let result = parse(self);
        self.no_trailing_closure = saved;
        result
    }

    fn dangling_doc(&mut self, span: Span) {
        self.error(
            Diagnostic::error(
                "cove::parse::dangling_doc_comment",
                "doc comment is not attached to a declaration",
            )
            .at(span)
            .rule("A `///` doc comment documents the declaration that follows it.")
            .help("Move the comment above a declaration, or write it as an ordinary `//` comment."),
        );
    }

    /// Joins the run of `///` comments at the cursor into one doc string.
    fn collect_doc(&mut self) -> Option<(String, Span)> {
        let mut lines: Vec<String> = Vec::new();
        let mut span: Option<Span> = None;
        loop {
            let TokenKind::DocComment(text) = self.peek() else {
                break;
            };
            let text = text.clone();
            let line_span = self.span();
            span = Some(match span {
                Some(previous) => previous.to(line_span),
                None => line_span,
            });
            lines.push(text);
            self.bump();
        }
        span.map(|span| (lines.join("\n"), span))
    }

    /// Whether the cursor begins a declaration. `fn` and `async fn` only start
    /// a declaration when a name follows; otherwise they open a lambda.
    fn at_item_start(&self) -> bool {
        match self.peek() {
            TokenKind::Keyword(
                Keyword::Export | Keyword::Struct | Keyword::Enum | Keyword::Impl | Keyword::Type,
            ) => true,
            TokenKind::Keyword(Keyword::Fn) => matches!(self.peek_at(1), TokenKind::Ident(_)),
            TokenKind::Keyword(Keyword::Async) => {
                matches!(self.peek_at(1), TokenKind::Keyword(Keyword::Fn))
                    && matches!(self.peek_at(2), TokenKind::Ident(_))
            }
            _ => false,
        }
    }

    fn at_stmt_start(&self) -> bool {
        self.at_item_start()
            || matches!(
                self.peek(),
                TokenKind::Keyword(Keyword::Let | Keyword::Var | Keyword::Return)
                    | TokenKind::DocComment(_)
            )
    }

    /// Skips tokens until the next declaration at brace depth zero. When
    /// `in_braces`, the `}` closing the enclosing group also stops recovery and
    /// is left for the caller. At least one token is always consumed unless
    /// recovery stops immediately at such a `}`.
    fn recover_to_item(&mut self, in_braces: bool) {
        let mut depth = 0i32;
        let mut consumed = false;
        while !self.is_eof() {
            if depth == 0 && self.at(&TokenKind::RBrace) && (in_braces || consumed) {
                if in_braces {
                    return;
                }
                self.bump();
                return;
            }
            if depth == 0
                && consumed
                && (self.at_item_start()
                    || self.at_keyword(Keyword::Use)
                    || matches!(self.peek(), TokenKind::DocComment(_)))
            {
                return;
            }
            match self.peek() {
                TokenKind::LBrace => depth += 1,
                TokenKind::RBrace => depth -= 1,
                _ => {}
            }
            self.bump();
            consumed = true;
        }
    }

    /// Skips tokens until the next statement inside a block, or the `}` that
    /// closes it. The closing brace is left for [`Parser::parse_block`].
    fn recover_in_block(&mut self) {
        let mut depth = 0i32;
        let mut consumed = false;
        while !self.is_eof() {
            if depth == 0 && self.at(&TokenKind::RBrace) {
                return;
            }
            if depth == 0 && consumed && self.at_stmt_start() {
                return;
            }
            match self.peek() {
                TokenKind::LBrace => depth += 1,
                TokenKind::RBrace => depth -= 1,
                _ => {}
            }
            self.bump();
            consumed = true;
        }
    }
}

/// Declarations.
impl Parser<'_> {
    fn parse_source_unit(&mut self) -> SourceUnit {
        let start = self.span();
        let mut uses = Vec::new();
        let mut items = Vec::new();

        while !self.is_eof() {
            let doc = self.collect_doc();

            if self.at_keyword(Keyword::Use) {
                if let Some((_, span)) = doc {
                    self.dangling_doc(span);
                }
                match self.parse_use() {
                    Ok(use_decl) => uses.push(use_decl),
                    Err(Bail) => self.recover_to_item(false),
                }
                continue;
            }

            if !self.at_item_start() {
                match doc {
                    Some((_, span)) => self.dangling_doc(span),
                    None => {
                        self.unexpected("a declaration");
                    }
                }
                self.recover_to_item(false);
                continue;
            }

            match self.parse_item(doc.map(|(text, _)| text)) {
                Ok(item) => items.push(item),
                Err(Bail) => self.recover_to_item(false),
            }
        }

        SourceUnit {
            uses,
            items,
            span: start.to(self.span()),
        }
    }

    /// `use http` and `use console.println`.
    fn parse_use(&mut self) -> PResult<Use> {
        let start = self.expect_keyword(Keyword::Use, "`use`")?;
        let mut path = vec![self.expect_ident()?];
        while self.eat(&TokenKind::Dot) {
            path.push(self.expect_ident()?);
        }
        Ok(Use {
            path,
            span: start.to(self.prev_span()),
        })
    }

    fn parse_item(&mut self, doc: Option<String>) -> PResult<Item> {
        let start = self.span();
        let exported = self.eat_keyword(Keyword::Export);
        let keyword = match self.peek() {
            TokenKind::Keyword(keyword) => Some(*keyword),
            _ => None,
        };
        let kind = match keyword {
            Some(Keyword::Fn | Keyword::Async) => ItemKind::Fn(self.parse_fn_decl()?),
            Some(Keyword::Struct) => ItemKind::Struct(self.parse_struct_decl()?),
            Some(Keyword::Enum) => ItemKind::Enum(self.parse_enum_decl()?),
            Some(Keyword::Impl) => ItemKind::Impl(self.parse_impl_block()?),
            Some(Keyword::Type) => ItemKind::TypeAlias(self.parse_type_alias()?),
            _ => return Err(self.unexpected("a declaration")),
        };
        Ok(Item {
            doc,
            exported,
            kind,
            span: start.to(self.prev_span()),
        })
    }

    fn parse_fn_decl(&mut self) -> PResult<FnDecl> {
        let start = self.span();
        let is_async = self.eat_keyword(Keyword::Async);
        self.expect_keyword(Keyword::Fn, "`fn`")?;
        let name = self.expect_ident()?;
        let generics = self.parse_generic_params()?;
        self.expect(&TokenKind::LParen, "`(`")?;
        let (receiver, params) = self.parse_param_list()?;
        let return_type = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let body = self.parse_block()?;
        Ok(FnDecl {
            name,
            is_async,
            generics,
            receiver,
            params,
            return_type,
            body,
            span: start.to(self.prev_span()),
        })
    }

    /// `<T, U>`, or nothing.
    fn parse_generic_params(&mut self) -> PResult<Vec<Ident>> {
        if !self.eat(&TokenKind::Lt) {
            return Ok(Vec::new());
        }
        let mut generics = Vec::new();
        while !self.at(&TokenKind::Gt) && !self.is_eof() {
            generics.push(self.expect_ident()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::Gt, "`>`")?;
        Ok(generics)
    }

    /// Parses a parameter list up to and including its `)`. A leading `self`
    /// or `var self` is the method receiver rather than a parameter.
    fn parse_param_list(&mut self) -> PResult<(Option<Receiver>, Vec<Param>)> {
        let mut receiver = None;
        let mut params = Vec::new();
        let mut first = true;

        while !self.at(&TokenKind::RParen) && !self.is_eof() {
            if first {
                first = false;
                if let Some(parsed) = self.try_receiver() {
                    receiver = Some(parsed);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                    continue;
                }
            }
            params.push(self.parse_param()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }

        self.expect(&TokenKind::RParen, "`)`")?;
        Ok((receiver, params))
    }

    fn try_receiver(&mut self) -> Option<Receiver> {
        let start = self.span();
        if self.at_keyword(Keyword::SelfValue) {
            self.bump();
            return Some(Receiver {
                is_var: false,
                span: start,
            });
        }
        if self.at_keyword(Keyword::Var)
            && matches!(self.peek_at(1), TokenKind::Keyword(Keyword::SelfValue))
        {
            self.bump();
            self.bump();
            return Some(Receiver {
                is_var: true,
                span: start.to(self.prev_span()),
            });
        }
        None
    }

    /// `[var] name [: Type] [...] [= default]`.
    fn parse_param(&mut self) -> PResult<Param> {
        let start = self.span();
        let is_var = self.eat_keyword(Keyword::Var);
        let name = self.expect_ident()?;
        let ty = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        let variadic = self.eat(&TokenKind::Ellipsis);
        let default = if self.eat(&TokenKind::Eq) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        Ok(Param {
            is_var,
            name,
            ty,
            variadic,
            default,
            span: start.to(self.prev_span()),
        })
    }

    /// `struct Name { field: Type ... }`, and the parenthesised
    /// `struct Name(field: Type, ...)` form. Fields are separated by newlines,
    /// commas, or both.
    fn parse_struct_decl(&mut self) -> PResult<StructDecl> {
        let start = self.expect_keyword(Keyword::Struct, "`struct`")?;
        let name = self.expect_ident()?;
        let generics = self.parse_generic_params()?;
        let close = if self.eat(&TokenKind::LBrace) {
            TokenKind::RBrace
        } else if self.eat(&TokenKind::LParen) {
            TokenKind::RParen
        } else {
            return Err(self.unexpected("`{` or `(`"));
        };

        let mut fields = Vec::new();
        while !self.at(&close) && !self.is_eof() {
            let doc = self.collect_doc();
            if self.at(&close) {
                if let Some((_, span)) = doc {
                    self.dangling_doc(span);
                }
                break;
            }
            let field_start = self.span();
            let field_name = self.expect_ident()?;
            self.expect(&TokenKind::Colon, "`:`")?;
            let ty = self.parse_type()?;
            fields.push(Field {
                doc: doc.map(|(text, _)| text),
                name: field_name,
                ty,
                span: field_start.to(self.prev_span()),
            });
            self.eat(&TokenKind::Comma);
        }
        self.expect(&close, "`}`")?;

        Ok(StructDecl {
            name,
            generics,
            fields,
            span: start.to(self.prev_span()),
        })
    }

    /// `enum Name { Case  Case(Type, Type) ... }`, with cases separated by
    /// newlines, commas, or both.
    fn parse_enum_decl(&mut self) -> PResult<EnumDecl> {
        let start = self.expect_keyword(Keyword::Enum, "`enum`")?;
        let name = self.expect_ident()?;
        let generics = self.parse_generic_params()?;
        self.expect(&TokenKind::LBrace, "`{`")?;

        let mut cases = Vec::new();
        while !self.at(&TokenKind::RBrace) && !self.is_eof() {
            let doc = self.collect_doc();
            if self.at(&TokenKind::RBrace) {
                if let Some((_, span)) = doc {
                    self.dangling_doc(span);
                }
                break;
            }
            let case_start = self.span();
            let case_name = self.expect_ident()?;
            let mut payload = Vec::new();
            if self.eat(&TokenKind::LParen) {
                while !self.at(&TokenKind::RParen) && !self.is_eof() {
                    payload.push(self.parse_type()?);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
                self.expect(&TokenKind::RParen, "`)`")?;
            }
            cases.push(EnumCase {
                doc: doc.map(|(text, _)| text),
                name: case_name,
                payload,
                span: case_start.to(self.prev_span()),
            });
            self.eat(&TokenKind::Comma);
        }
        self.expect(&TokenKind::RBrace, "`}`")?;

        Ok(EnumDecl {
            name,
            generics,
            cases,
            span: start.to(self.prev_span()),
        })
    }

    fn parse_impl_block(&mut self) -> PResult<ImplBlock> {
        let start = self.expect_keyword(Keyword::Impl, "`impl`")?;
        let type_name = self.expect_ident()?;
        let generics = self.parse_generic_params()?;
        self.expect(&TokenKind::LBrace, "`{`")?;

        let mut items = Vec::new();
        while !self.at(&TokenKind::RBrace) && !self.is_eof() {
            let doc = self.collect_doc();
            if !self.at_item_start() {
                match doc {
                    Some((_, span)) => self.dangling_doc(span),
                    None => {
                        self.unexpected("a declaration");
                    }
                }
                self.recover_to_item(true);
                continue;
            }
            match self.parse_item(doc.map(|(text, _)| text)) {
                Ok(item) => items.push(item),
                Err(Bail) => self.recover_to_item(true),
            }
        }
        self.expect(&TokenKind::RBrace, "`}`")?;

        Ok(ImplBlock {
            type_name,
            generics,
            items,
            span: start.to(self.prev_span()),
        })
    }

    fn parse_type_alias(&mut self) -> PResult<TypeAlias> {
        let start = self.expect_keyword(Keyword::Type, "`type`")?;
        let name = self.expect_ident()?;
        let generics = self.parse_generic_params()?;
        self.expect(&TokenKind::Eq, "`=`")?;
        let ty = self.parse_type()?;
        Ok(TypeAlias {
            name,
            generics,
            ty,
            span: start.to(self.prev_span()),
        })
    }
}

/// Types.
impl Parser<'_> {
    fn parse_type(&mut self) -> PResult<Type> {
        let start = self.span();
        let kind = match self.peek() {
            TokenKind::LParen => {
                self.bump();
                self.expect(&TokenKind::RParen, "`)`")?;
                TypeKind::Unit
            }
            TokenKind::Keyword(Keyword::Async | Keyword::Fn) => self.parse_fn_type()?,
            TokenKind::Ident(_) => {
                let mut path = vec![self.expect_ident()?];
                while self.at(&TokenKind::Dot) && matches!(self.peek_at(1), TokenKind::Ident(_)) {
                    self.bump();
                    path.push(self.expect_ident()?);
                }
                let args = if self.at(&TokenKind::Lt) {
                    self.parse_type_args()?
                } else {
                    Vec::new()
                };
                TypeKind::Named { path, args }
            }
            _ => return Err(self.unexpected("a type")),
        };
        Ok(Type {
            kind,
            span: start.to(self.prev_span()),
        })
    }

    fn parse_fn_type(&mut self) -> PResult<TypeKind> {
        let is_async = self.eat_keyword(Keyword::Async);
        self.expect_keyword(Keyword::Fn, "`fn`")?;
        self.expect(&TokenKind::LParen, "`(`")?;
        let mut params = Vec::new();
        while !self.at(&TokenKind::RParen) && !self.is_eof() {
            params.push(self.parse_fn_type_param()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RParen, "`)`")?;
        let return_type = if self.eat(&TokenKind::Arrow) {
            Some(Box::new(self.parse_type()?))
        } else {
            None
        };
        Ok(TypeKind::Fn {
            is_async,
            params,
            return_type,
        })
    }

    /// A function-type parameter is either named (`request: http.Request`) or
    /// bare (`String`); a bare parameter has an empty name.
    fn parse_fn_type_param(&mut self) -> PResult<Param> {
        let start = self.span();
        let is_var = self.eat_keyword(Keyword::Var);
        if matches!(self.peek(), TokenKind::Ident(_)) && self.peek_at(1) == &TokenKind::Colon {
            let name = self.expect_ident()?;
            self.bump();
            let ty = self.parse_type()?;
            let variadic = self.eat(&TokenKind::Ellipsis);
            return Ok(Param {
                is_var,
                name,
                ty: Some(ty),
                variadic,
                default: None,
                span: start.to(self.prev_span()),
            });
        }
        let ty = self.parse_type()?;
        let variadic = self.eat(&TokenKind::Ellipsis);
        let span = start.to(self.prev_span());
        Ok(Param {
            is_var,
            name: Spanned::new(String::new(), span),
            ty: Some(ty),
            variadic,
            default: None,
            span,
        })
    }

    /// `<T, Result<U, E>>`. The lexer never joins `>>`, so nested generic
    /// arguments close naturally.
    fn parse_type_args(&mut self) -> PResult<Vec<Type>> {
        self.expect(&TokenKind::Lt, "`<`")?;
        let mut args = Vec::new();
        while !self.at(&TokenKind::Gt) && !self.is_eof() {
            args.push(self.parse_type()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::Gt, "`>`")?;
        Ok(args)
    }
}

/// Blocks and statements.
impl Parser<'_> {
    /// Parses `{ ... }`. The last statement, when it is an expression,
    /// becomes the block's value.
    fn parse_block(&mut self) -> PResult<Block> {
        let start = self.span();
        self.expect(&TokenKind::LBrace, "`{`")?;

        let mut statements = Vec::new();
        self.scoped(false, |parser| {
            while !parser.at(&TokenKind::RBrace) && !parser.is_eof() {
                match parser.parse_stmt() {
                    Ok(stmt) => statements.push(stmt),
                    Err(Bail) => parser.recover_in_block(),
                }
            }
        });

        let end = self.span();
        self.expect(&TokenKind::RBrace, "`}`")?;

        let mut tail = None;
        if matches!(
            statements.last(),
            Some(Stmt {
                kind: StmtKind::Expr(_),
                ..
            })
        ) {
            if let Some(Stmt {
                kind: StmtKind::Expr(value),
                ..
            }) = statements.pop()
            {
                tail = Some(Box::new(value));
            }
        }

        Ok(Block {
            statements,
            tail,
            span: start.to(end),
        })
    }

    fn parse_stmt(&mut self) -> PResult<Stmt> {
        let doc = self.collect_doc();

        if self.at_item_start() {
            let item = self.parse_item(doc.map(|(text, _)| text))?;
            let span = item.span;
            return Ok(Stmt {
                kind: StmtKind::Item(Box::new(item)),
                span,
            });
        }
        if let Some((_, span)) = doc {
            self.dangling_doc(span);
        }

        if self.at_keyword(Keyword::Let) || self.at_keyword(Keyword::Var) {
            return self.parse_let_stmt();
        }

        let value = self.parse_expr()?;
        Ok(Stmt {
            span: value.span,
            kind: StmtKind::Expr(value),
        })
    }

    /// `let name: T = value` and `var name = value`.
    fn parse_let_stmt(&mut self) -> PResult<Stmt> {
        let start = self.span();
        let is_var = self.at_keyword(Keyword::Var);
        self.bump();
        let name = self.expect_ident()?;
        let ty = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(&TokenKind::Eq, "`=`")?;
        let value = self.parse_expr()?;
        Ok(Stmt {
            kind: StmtKind::Let {
                is_var,
                name,
                ty,
                value,
            },
            span: start.to(self.prev_span()),
        })
    }
}

/// Expressions.
impl Parser<'_> {
    fn parse_expr(&mut self) -> PResult<Expr> {
        self.parse_assign()
    }

    fn parse_assign(&mut self) -> PResult<Expr> {
        let target = self.parse_or()?;
        let op = match self.peek() {
            TokenKind::Eq => None,
            TokenKind::PlusEq => Some(BinaryOp::Add),
            TokenKind::MinusEq => Some(BinaryOp::Sub),
            TokenKind::StarEq => Some(BinaryOp::Mul),
            TokenKind::SlashEq => Some(BinaryOp::Div),
            TokenKind::PercentEq => Some(BinaryOp::Rem),
            _ => return Ok(target),
        };
        self.bump();
        let value = self.parse_assign()?;

        if !is_place_expr(&target) {
            self.error(
                Diagnostic::error(
                    "cove::parse::invalid_assignment_target",
                    "this expression cannot be assigned to",
                )
                .at(target.span)
                .rule("Assignment writes to a place: a name, or a field of a place.")
                .help("Assign to a variable or a field, such as `self.count = 1`."),
            );
        }

        let span = target.span.to(value.span);
        Ok(expr(
            ExprKind::Assign {
                op,
                target: Box::new(target),
                value: Box::new(value),
            },
            span,
        ))
    }

    fn parse_or(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_and()?;
        while self.at(&TokenKind::PipePipe) {
            self.bump();
            let rhs = self.parse_and()?;
            lhs = binary(BinaryOp::Or, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_comparison()?;
        while self.at(&TokenKind::AmpAmp) {
            self.bump();
            let rhs = self.parse_comparison()?;
            lhs = binary(BinaryOp::And, lhs, rhs);
        }
        Ok(lhs)
    }

    fn parse_comparison(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_range()?;
        loop {
            let op = match self.peek() {
                TokenKind::EqEq => BinaryOp::Eq,
                TokenKind::BangEq => BinaryOp::Ne,
                TokenKind::Lt => BinaryOp::Lt,
                TokenKind::LtEq => BinaryOp::Le,
                TokenKind::Gt => BinaryOp::Gt,
                TokenKind::GtEq => BinaryOp::Ge,
                _ => return Ok(lhs),
            };
            self.bump();
            let rhs = self.parse_range()?;
            lhs = binary(op, lhs, rhs);
        }
    }

    /// `0..<attempts` excludes its end; `0..n` includes it.
    fn parse_range(&mut self) -> PResult<Expr> {
        let start = self.parse_additive()?;
        let inclusive_end = match self.peek() {
            TokenKind::DotDot => true,
            TokenKind::DotDotLt => false,
            _ => return Ok(start),
        };
        self.bump();
        let end = self.parse_additive()?;
        let span = start.span.to(end.span);
        Ok(expr(
            ExprKind::Range {
                start: Box::new(start),
                end: Box::new(end),
                inclusive_end,
            },
            span,
        ))
    }

    fn parse_additive(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                TokenKind::Plus => BinaryOp::Add,
                TokenKind::Minus => BinaryOp::Sub,
                _ => return Ok(lhs),
            };
            self.bump();
            let rhs = self.parse_multiplicative()?;
            lhs = binary(op, lhs, rhs);
        }
    }

    fn parse_multiplicative(&mut self) -> PResult<Expr> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                TokenKind::Star => BinaryOp::Mul,
                TokenKind::Slash => BinaryOp::Div,
                TokenKind::Percent => BinaryOp::Rem,
                _ => return Ok(lhs),
            };
            self.bump();
            let rhs = self.parse_unary()?;
            lhs = binary(op, lhs, rhs);
        }
    }

    /// `await` binds tighter than any binary operator and looser than the
    /// postfix operators, so `await handler(event)?` awaits the whole
    /// fallible call.
    fn parse_unary(&mut self) -> PResult<Expr> {
        let start = self.span();
        let op = match self.peek() {
            TokenKind::Bang => Some(UnaryOp::Not),
            TokenKind::Minus => Some(UnaryOp::Neg),
            TokenKind::Keyword(Keyword::Await) => None,
            _ => return self.parse_postfix(),
        };
        match op {
            Some(op) => {
                self.bump();
                let operand = self.parse_unary()?;
                let span = start.to(operand.span);
                Ok(expr(
                    ExprKind::Unary {
                        op,
                        operand: Box::new(operand),
                    },
                    span,
                ))
            }
            None => {
                self.bump();
                let operand = self.parse_unary()?;
                let span = start.to(operand.span);
                Ok(expr(ExprKind::Await(Box::new(operand)), span))
            }
        }
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        let mut value = self.parse_primary()?;
        loop {
            match self.peek() {
                TokenKind::Dot => {
                    self.bump();
                    let name = self.expect_member_name()?;
                    let span = value.span.to(name.span);
                    value = expr(
                        ExprKind::Field {
                            base: Box::new(value),
                            name,
                        },
                        span,
                    );
                }
                TokenKind::LParen => {
                    self.bump();
                    let args = self.parse_args()?;
                    value = self.finish_call(value, Vec::new(), args)?;
                }
                TokenKind::Question => {
                    let span = value.span.to(self.span());
                    self.bump();
                    value = expr(ExprKind::Try(Box::new(value)), span);
                }
                TokenKind::Lt => match self.try_generic_call(value)? {
                    Ok(call) => value = call,
                    Err(unchanged) => return Ok(unchanged),
                },
                TokenKind::LBrace
                    if !self.no_trailing_closure && can_take_trailing_closure(&value) =>
                {
                    let closure = self.parse_trailing_closure()?;
                    let span = value.span.to(closure.span);
                    value = expr(
                        ExprKind::Call {
                            callee: Box::new(value),
                            generics: Vec::new(),
                            args: Vec::new(),
                            trailing: Some(Box::new(closure)),
                        },
                        span,
                    );
                }
                _ => return Ok(value),
            }
        }
    }

    /// Builds a call, attaching `f(x) { ... }`-style trailing closures.
    fn finish_call(&mut self, callee: Expr, generics: Vec<Type>, args: Vec<Arg>) -> PResult<Expr> {
        let mut span = callee.span.to(self.prev_span());
        let trailing = if !self.no_trailing_closure && self.at(&TokenKind::LBrace) {
            let closure = self.parse_trailing_closure()?;
            span = span.to(closure.span);
            Some(Box::new(closure))
        } else {
            None
        };
        Ok(expr(
            ExprKind::Call {
                callee: Box::new(callee),
                generics,
                args,
                trailing,
            },
            span,
        ))
    }

    /// A trailing closure is a parameterless lambda written as a block.
    fn parse_trailing_closure(&mut self) -> PResult<Expr> {
        let body = self.parse_block()?;
        let span = body.span;
        Ok(expr(
            ExprKind::Lambda {
                is_async: false,
                params: Vec::new(),
                body,
            },
            span,
        ))
    }

    /// Resolves the `<` ambiguity by speculation: `api.fetch<Array<Booking>>(...)`
    /// is a generic call only when a type list closed by `>` is immediately
    /// followed by `(`. Otherwise the cursor rewinds and `<` stays a
    /// comparison operator.
    #[allow(clippy::type_complexity)]
    fn try_generic_call(&mut self, callee: Expr) -> PResult<Result<Expr, Expr>> {
        let saved_pos = self.pos;
        let saved_diagnostics = self.diagnostics.len();

        let generics = match self.parse_type_args() {
            Ok(generics) if self.at(&TokenKind::LParen) => generics,
            _ => {
                self.pos = saved_pos;
                self.diagnostics.truncate(saved_diagnostics);
                return Ok(Err(callee));
            }
        };

        self.bump();
        let args = self.parse_args()?;
        Ok(Ok(self.finish_call(callee, generics, args)?))
    }

    /// Parses arguments up to and including `)`.
    fn parse_args(&mut self) -> PResult<Vec<Arg>> {
        self.scoped(false, |parser| {
            let mut args: Vec<Arg> = Vec::new();
            let mut first_label: Option<Span> = None;

            while !parser.at(&TokenKind::RParen) && !parser.is_eof() {
                let start = parser.span();
                let label = if matches!(parser.peek(), TokenKind::Ident(_))
                    && parser.peek_at(1) == &TokenKind::Colon
                {
                    let label = parser.expect_ident()?;
                    parser.bump();
                    Some(label)
                } else {
                    None
                };

                match (&label, first_label) {
                    (Some(label), None) => first_label = Some(label.span),
                    (None, Some(previous)) => parser.error(
                        Diagnostic::error(
                            "cove::parse::positional_after_label",
                            "a positional argument cannot follow a labeled argument",
                        )
                        .at(start)
                        .label(previous, "the first labeled argument is here")
                        .rule(
                            "Positional arguments may precede labeled arguments; after the first \
                             label, every remaining argument is labeled.",
                        )
                        .help("Give this argument its parameter label, such as `name: value`."),
                    ),
                    _ => {}
                }

                let is_var = parser.eat_keyword(Keyword::Var);
                let spread = parser.eat(&TokenKind::Ellipsis);
                let value = parser.parse_expr()?;
                args.push(Arg {
                    label,
                    is_var,
                    spread,
                    value,
                    span: start.to(parser.prev_span()),
                });

                if !parser.eat(&TokenKind::Comma) {
                    break;
                }
            }

            parser.expect(&TokenKind::RParen, "`)`")?;
            Ok(args)
        })
    }

    fn parse_primary(&mut self) -> PResult<Expr> {
        let start = self.span();
        let kind = self.peek().clone();
        match kind {
            TokenKind::Int(value) => {
                self.bump();
                Ok(expr(ExprKind::Int(value), start))
            }
            TokenKind::Float(value) => {
                self.bump();
                Ok(expr(ExprKind::Float(value), start))
            }
            TokenKind::Bool(value) => {
                self.bump();
                Ok(expr(ExprKind::Bool(value), start))
            }
            TokenKind::Duration(value) => {
                self.bump();
                Ok(expr(ExprKind::Duration(value), start))
            }
            TokenKind::Str(parts) => {
                self.bump();
                let parts = self.parse_str_parts(&parts);
                Ok(expr(ExprKind::Str(parts), start))
            }
            TokenKind::Ident(name) => {
                self.bump();
                Ok(expr(ExprKind::Ident(name), start))
            }
            TokenKind::Keyword(Keyword::SelfValue) => {
                self.bump();
                Ok(expr(ExprKind::Ident("self".into()), start))
            }
            TokenKind::LParen => {
                self.bump();
                if self.at(&TokenKind::RParen) {
                    self.bump();
                    return Ok(expr(ExprKind::Unit, start.to(self.prev_span())));
                }
                let inner = self.scoped(false, |parser| parser.parse_expr())?;
                self.expect(&TokenKind::RParen, "`)`")?;
                Ok(expr(inner.kind, start.to(self.prev_span())))
            }
            TokenKind::LBracket => {
                self.bump();
                let elements = self.scoped(false, |parser| {
                    let mut elements = Vec::new();
                    while !parser.at(&TokenKind::RBracket) && !parser.is_eof() {
                        elements.push(parser.parse_expr()?);
                        if !parser.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                    parser.expect(&TokenKind::RBracket, "`]`")?;
                    Ok(elements)
                })?;
                Ok(expr(
                    ExprKind::ArrayLit(elements),
                    start.to(self.prev_span()),
                ))
            }
            TokenKind::LBrace => {
                let block = self.parse_block()?;
                let span = block.span;
                Ok(expr(ExprKind::Block(block), span))
            }
            TokenKind::Keyword(Keyword::If) => self.parse_if(),
            TokenKind::Keyword(Keyword::Match) => self.parse_match(),
            TokenKind::Keyword(Keyword::For) => self.parse_for(),
            TokenKind::Keyword(Keyword::While) => self.parse_while(),
            TokenKind::Keyword(Keyword::Scope) => self.parse_scope(),
            TokenKind::Keyword(Keyword::Return) => {
                self.bump();
                let value = if self.can_start_expr() {
                    Some(Box::new(self.parse_expr()?))
                } else {
                    None
                };
                Ok(expr(ExprKind::Return(value), start.to(self.prev_span())))
            }
            TokenKind::Keyword(Keyword::Fn | Keyword::Async) => self.parse_lambda(),
            _ => Err(self.unexpected("an expression")),
        }
    }

    fn can_start_expr(&self) -> bool {
        matches!(
            self.peek(),
            TokenKind::Int(_)
                | TokenKind::Float(_)
                | TokenKind::Bool(_)
                | TokenKind::Duration(_)
                | TokenKind::Str(_)
                | TokenKind::Ident(_)
                | TokenKind::LParen
                | TokenKind::LBracket
                | TokenKind::LBrace
                | TokenKind::Bang
                | TokenKind::Minus
                | TokenKind::Keyword(
                    Keyword::If
                        | Keyword::Match
                        | Keyword::For
                        | Keyword::While
                        | Keyword::Scope
                        | Keyword::Fn
                        | Keyword::Async
                        | Keyword::Await
                        | Keyword::Return
                        | Keyword::SelfValue
                )
        )
    }

    fn parse_if(&mut self) -> PResult<Expr> {
        let start = self.expect_keyword(Keyword::If, "`if`")?;
        let condition = self.scoped(true, |parser| parser.parse_expr())?;
        let then_branch = self.parse_block()?;
        let else_branch = if self.eat_keyword(Keyword::Else) {
            if self.at_keyword(Keyword::If) {
                Some(Box::new(self.parse_if()?))
            } else {
                let block = self.parse_block()?;
                let span = block.span;
                Some(Box::new(expr(ExprKind::Block(block), span)))
            }
        } else {
            None
        };
        Ok(expr(
            ExprKind::If {
                condition: Box::new(condition),
                then_branch,
                else_branch,
            },
            start.to(self.prev_span()),
        ))
    }

    fn parse_match(&mut self) -> PResult<Expr> {
        let start = self.expect_keyword(Keyword::Match, "`match`")?;
        let scrutinee = self.scoped(true, |parser| parser.parse_expr())?;
        self.expect(&TokenKind::LBrace, "`{`")?;
        let arms = self.scoped(false, |parser| {
            let mut arms = Vec::new();
            while !parser.at(&TokenKind::RBrace) && !parser.is_eof() {
                let arm_start = parser.span();
                let pattern = parser.parse_pattern()?;
                parser.expect(&TokenKind::FatArrow, "`=>`")?;
                let body = parser.parse_expr()?;
                arms.push(MatchArm {
                    pattern,
                    body,
                    span: arm_start.to(parser.prev_span()),
                });
                parser.eat(&TokenKind::Comma);
            }
            Ok(arms)
        })?;
        self.expect(&TokenKind::RBrace, "`}`")?;
        Ok(expr(
            ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            start.to(self.prev_span()),
        ))
    }

    fn parse_for(&mut self) -> PResult<Expr> {
        let start = self.expect_keyword(Keyword::For, "`for`")?;
        let binding = self.expect_ident()?;
        self.expect_keyword(Keyword::In, "`in`")?;
        let iterable = self.scoped(true, |parser| parser.parse_expr())?;
        let body = self.parse_block()?;
        Ok(expr(
            ExprKind::For {
                binding,
                iterable: Box::new(iterable),
                body,
            },
            start.to(self.prev_span()),
        ))
    }

    fn parse_while(&mut self) -> PResult<Expr> {
        let start = self.expect_keyword(Keyword::While, "`while`")?;
        let condition = self.scoped(true, |parser| parser.parse_expr())?;
        let body = self.parse_block()?;
        Ok(expr(
            ExprKind::While {
                condition: Box::new(condition),
                body,
            },
            start.to(self.prev_span()),
        ))
    }

    fn parse_scope(&mut self) -> PResult<Expr> {
        let start = self.expect_keyword(Keyword::Scope, "`scope`")?;
        let name = self.expect_ident()?;
        let body = self.parse_block()?;
        Ok(expr(
            ExprKind::Scope { name, body },
            start.to(self.prev_span()),
        ))
    }

    /// `fn(x) { ... }`, `async fn(x) { ... }`, and the parameterless
    /// `async fn { ... }`.
    fn parse_lambda(&mut self) -> PResult<Expr> {
        let start = self.span();
        let is_async = self.eat_keyword(Keyword::Async);
        self.expect_keyword(Keyword::Fn, "`fn`")?;
        let params = if self.eat(&TokenKind::LParen) {
            let (receiver, params) = self.scoped(false, |parser| parser.parse_param_list())?;
            if let Some(receiver) = receiver {
                self.error(
                    Diagnostic::error(
                        "cove::parse::self_outside_method",
                        "`self` is only a parameter of a method",
                    )
                    .at(receiver.span)
                    .rule("A `self` receiver belongs to a function declared inside `impl`.")
                    .help("Remove `self`, or move this function into an `impl` block."),
                );
            }
            params
        } else {
            Vec::new()
        };
        let body = self.scoped(false, |parser| parser.parse_block())?;
        Ok(expr(
            ExprKind::Lambda {
                is_async,
                params,
                body,
            },
            start.to(self.prev_span()),
        ))
    }
}

fn binary(op: BinaryOp, lhs: Expr, rhs: Expr) -> Expr {
    let span = lhs.span.to(rhs.span);
    expr(
        ExprKind::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        },
        span,
    )
}

/// Patterns.
impl Parser<'_> {
    /// Parses one `match` pattern.
    ///
    /// A name is a variant when it is dotted or begins with an uppercase
    /// letter (`Ok(value)`, `LogLevel.Debug`, `ConfigError.InvalidPort(raw)`).
    /// A lone name that begins with a lowercase letter binds the scrutinee
    /// (`other`), and `_` matches without binding.
    fn parse_pattern(&mut self) -> PResult<Pattern> {
        let start = self.span();
        match self.peek() {
            TokenKind::Underscore => {
                self.bump();
                Ok(Pattern {
                    kind: PatternKind::Wildcard,
                    span: start,
                })
            }
            TokenKind::Int(_)
            | TokenKind::Float(_)
            | TokenKind::Bool(_)
            | TokenKind::Duration(_)
            | TokenKind::Str(_) => {
                let literal = self.parse_primary()?;
                let span = literal.span;
                Ok(Pattern {
                    kind: PatternKind::Literal(literal),
                    span,
                })
            }
            TokenKind::Minus => {
                let literal = self.parse_unary()?;
                let span = literal.span;
                Ok(Pattern {
                    kind: PatternKind::Literal(literal),
                    span,
                })
            }
            TokenKind::Ident(_) => {
                let mut path = vec![self.expect_ident()?];
                while self.at(&TokenKind::Dot) {
                    self.bump();
                    path.push(self.expect_ident()?);
                }

                let mut payload = Vec::new();
                let has_payload = self.at(&TokenKind::LParen);
                if has_payload {
                    self.bump();
                    while !self.at(&TokenKind::RParen) && !self.is_eof() {
                        payload.push(self.parse_pattern()?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(&TokenKind::RParen, "`)`")?;
                }

                let is_variant = path.len() > 1
                    || has_payload
                    || path[0]
                        .node
                        .chars()
                        .next()
                        .is_some_and(|first| first.is_uppercase());

                let span = start.to(self.prev_span());
                let kind = if is_variant {
                    PatternKind::Variant { path, payload }
                } else {
                    PatternKind::Binding(path.remove(0).node)
                };
                Ok(Pattern { kind, span })
            }
            _ => Err(self.unexpected("a pattern")),
        }
    }
}

/// String literals and their interpolations.
impl Parser<'_> {
    fn parse_str_parts(&mut self, parts: &[StringPart]) -> Vec<StrPart> {
        let mut resolved = Vec::new();
        for part in parts {
            match part {
                StringPart::Text(text) => resolved.push(StrPart::Text(text.clone())),
                StringPart::Interpolation { source, span } => {
                    if let Some(value) = self.parse_interpolation(source, *span) {
                        resolved.push(StrPart::Interpolation(value));
                    }
                }
            }
        }
        resolved
    }

    /// Parses the expression inside `"... {expr} ..."`.
    ///
    /// The interpolation is lexed as its own scratch source and every span it
    /// produces is rebased onto the file that contains the string, so a
    /// diagnostic points at the real position of the code.
    fn parse_interpolation(&mut self, source: &str, span: Span) -> Option<Expr> {
        if source.trim().is_empty() {
            self.error(
                Diagnostic::error(
                    "cove::parse::empty_interpolation",
                    "string interpolation contains no expression",
                )
                .at(span)
                .rule("`{ }` inside a string literal interpolates exactly one expression.")
                .help("Write an expression between the braces, or escape them as `\\{` and `\\}`."),
            );
            return None;
        }

        let mut scratch = SourceMap::new();
        let scratch_file = scratch.add("<interpolation>", source.to_string());
        let tokens = match lexer::lex(&scratch, scratch_file) {
            Ok(tokens) => tokens,
            Err(diagnostics) => {
                for diagnostic in diagnostics {
                    let rebased = rebase_diagnostic(diagnostic, self.file, span.start);
                    self.error(rebased);
                }
                return None;
            }
        };
        let tokens = tokens
            .into_iter()
            .map(|token| rebase_token(token, self.file, span.start))
            .collect();

        let mut inner = Parser::new(self.sources, self.file, tokens);
        let value = inner.parse_expr().ok();
        if value.is_some() && !inner.is_eof() {
            let found = inner.peek().describe();
            let rest = inner.span();
            inner.error(
                Diagnostic::error(
                    "cove::parse::unexpected_token",
                    format!("expected end of interpolation, found {found}"),
                )
                .at(rest)
                .rule("`{ }` inside a string literal interpolates exactly one expression."),
            );
        }
        let failed = !inner.diagnostics.is_empty();
        self.diagnostics.append(&mut inner.diagnostics);
        if failed {
            None
        } else {
            value
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn parse_source(source: &str) -> (SourceMap, Result<SourceUnit, Vec<Diagnostic>>) {
        let mut sources = SourceMap::new();
        let file = sources.add("test.cove", source);
        let result = match lexer::lex(&sources, file) {
            Ok(tokens) => parse(&sources, file, tokens),
            Err(diagnostics) => Err(diagnostics),
        };
        (sources, result)
    }

    fn ok(source: &str) -> SourceUnit {
        let (sources, result) = parse_source(source);
        match result {
            Ok(unit) => unit,
            Err(diagnostics) => {
                let rendered: String = diagnostics
                    .iter()
                    .map(|diagnostic| cove_diag::render(&sources, diagnostic))
                    .collect();
                panic!("expected `{source}` to parse:\n{rendered}");
            }
        }
    }

    fn errors(source: &str) -> Vec<Diagnostic> {
        let (_, result) = parse_source(source);
        match result {
            Ok(_) => panic!("expected `{source}` to fail"),
            Err(diagnostics) => diagnostics,
        }
    }

    fn codes(diagnostics: &[Diagnostic]) -> Vec<&str> {
        diagnostics.iter().map(|d| d.code.as_str()).collect()
    }

    fn fn_decl(item: &Item) -> &FnDecl {
        match &item.kind {
            ItemKind::Fn(decl) => decl,
            other => panic!("expected a function, found {other:?}"),
        }
    }

    /// Parses `source` as the body of `fn main`, returning the block's value.
    fn tail_expr(source: &str) -> Expr {
        let unit = ok(&format!("fn main() {{\n{source}\n}}"));
        let decl = fn_decl(&unit.items[0]);
        *decl
            .body
            .tail
            .clone()
            .unwrap_or_else(|| panic!("`{source}` produced no tail expression"))
    }

    #[test]
    fn parses_uses_and_items_in_any_order() {
        let unit = ok("use http\nfn a() {}\nuse console.println\nfn b() {}");
        assert_eq!(unit.uses.len(), 2);
        assert_eq!(unit.uses[0].path.len(), 1);
        assert_eq!(unit.uses[1].path[1].node, "println");
        assert_eq!(unit.items.len(), 2);
    }

    #[test]
    fn parses_function_declarations() {
        let unit = ok("export async fn run<T, U>(name: String, items: T... , retries: Int = 3) -> Result<Unit, Error> { Ok(()) }");
        let item = &unit.items[0];
        assert!(item.exported);
        let decl = fn_decl(item);
        assert!(decl.is_async);
        assert_eq!(decl.name.node, "run");
        assert_eq!(decl.generics.len(), 2);
        assert!(decl.receiver.is_none());
        assert_eq!(decl.params.len(), 3);
        assert!(decl.params[1].variadic);
        assert!(decl.params[2].default.is_some());
        assert!(decl.return_type.is_some());
    }

    #[test]
    fn parses_struct_in_brace_and_paren_form() {
        let braced = ok("export struct App {\n  repository: Repo\n  metrics: Shared<Metrics>\n}");
        let ItemKind::Struct(decl) = &braced.items[0].kind else {
            panic!("expected a struct");
        };
        assert_eq!(decl.fields.len(), 2);

        let parens = ok("export struct Booking(id: BookingId, status: BookingStatus)");
        let ItemKind::Struct(decl) = &parens.items[0].kind else {
            panic!("expected a struct");
        };
        assert_eq!(decl.fields.len(), 2);
        assert_eq!(decl.fields[1].name.node, "status");
    }

    #[test]
    fn parses_enum_cases_with_and_without_commas() {
        let unit =
            ok("enum ConfigError {\n  InvalidPort(String)\n  Missing,\n  Pair(Int, String)\n}");
        let ItemKind::Enum(decl) = &unit.items[0].kind else {
            panic!("expected an enum");
        };
        assert_eq!(decl.cases.len(), 3);
        assert_eq!(decl.cases[0].payload.len(), 1);
        assert!(decl.cases[1].payload.is_empty());
        assert_eq!(decl.cases[2].payload.len(), 2);
    }

    #[test]
    fn parses_impl_blocks_with_receivers() {
        let unit = ok("impl Metrics {\n  /// Records one request.\n  fn record(var self, failed: Bool) { self.requests += 1 }\n  fn read(self) -> Int { self.requests }\n}");
        let ItemKind::Impl(block) = &unit.items[0].kind else {
            panic!("expected an impl");
        };
        assert_eq!(block.type_name.node, "Metrics");
        assert_eq!(block.items.len(), 2);
        let first = fn_decl(&block.items[0]);
        assert_eq!(block.items[0].doc.as_deref(), Some("Records one request."));
        assert!(first.receiver.expect("receiver").is_var);
        assert_eq!(first.params.len(), 1);
        assert!(!fn_decl(&block.items[1]).receiver.expect("receiver").is_var);
    }

    #[test]
    fn parses_type_alias_with_function_type() {
        let unit = ok(
            "export type Handler = async fn(request: http.Request) -> Result<http.Response, Error>",
        );
        let ItemKind::TypeAlias(alias) = &unit.items[0].kind else {
            panic!("expected a type alias");
        };
        let TypeKind::Fn {
            is_async,
            params,
            return_type,
        } = &alias.ty.kind
        else {
            panic!("expected a function type");
        };
        assert!(is_async);
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name.node, "request");
        assert!(return_type.is_some());
    }

    #[test]
    fn parses_unnamed_function_type_parameters_and_unit() {
        let unit = ok("type Predicate = fn(String, Int) -> ()");
        let ItemKind::TypeAlias(alias) = &unit.items[0].kind else {
            panic!("expected a type alias");
        };
        let TypeKind::Fn {
            params,
            return_type,
            ..
        } = &alias.ty.kind
        else {
            panic!("expected a function type");
        };
        assert_eq!(params.len(), 2);
        assert!(params[0].name.node.is_empty());
        assert!(matches!(
            return_type.as_deref().map(|ty| &ty.kind),
            Some(TypeKind::Unit)
        ));
    }

    #[test]
    fn joins_consecutive_doc_comments() {
        let unit = ok("/// First line.\n/// Second line.\nexport fn a() {}");
        assert_eq!(
            unit.items[0].doc.as_deref(),
            Some("First line.\nSecond line.")
        );
    }

    #[test]
    fn attaches_doc_comments_to_fields_and_cases() {
        let unit =
            ok("struct S {\n  /// The port.\n  port: Int\n}\nenum E {\n  /// A case.\n  Case\n}");
        let ItemKind::Struct(decl) = &unit.items[0].kind else {
            panic!("expected a struct");
        };
        assert_eq!(decl.fields[0].doc.as_deref(), Some("The port."));
        let ItemKind::Enum(decl) = &unit.items[1].kind else {
            panic!("expected an enum");
        };
        assert_eq!(decl.cases[0].doc.as_deref(), Some("A case."));
    }

    #[test]
    fn dangling_doc_comment_is_an_error() {
        let diagnostics = errors("/// Nothing follows this.\n");
        assert_eq!(codes(&diagnostics), ["cove::parse::dangling_doc_comment"]);
    }

    #[test]
    fn parses_nested_generic_types() {
        let unit = ok("struct S { field: Map<String, Array<Result<Vector<Int>, Error>>> }");
        let ItemKind::Struct(decl) = &unit.items[0].kind else {
            panic!("expected a struct");
        };
        let TypeKind::Named { path, args } = &decl.fields[0].ty.kind else {
            panic!("expected a named type");
        };
        assert_eq!(path[0].node, "Map");
        assert_eq!(args.len(), 2);
        let TypeKind::Named { args: inner, .. } = &args[1].kind else {
            panic!("expected a named type");
        };
        let TypeKind::Named { args: inner, .. } = &inner[0].kind else {
            panic!("expected a named type");
        };
        assert_eq!(inner.len(), 2);
    }

    #[test]
    fn parses_dotted_type_paths() {
        let unit = ok("fn f(request: http.Request) -> http.Response<Body> { request }");
        let decl = fn_decl(&unit.items[0]);
        let TypeKind::Named { path, .. } = &decl.params[0].ty.as_ref().unwrap().kind else {
            panic!("expected a named type");
        };
        assert_eq!(path.len(), 2);
        assert_eq!(path[1].node, "Request");
    }

    #[test]
    fn parses_labeled_var_and_spread_arguments() {
        let positional = tail_expr("f(a, var d, ...e)");
        let ExprKind::Call { args, .. } = &positional.kind else {
            panic!("expected a call");
        };
        assert_eq!(args.len(), 3);
        assert!(args.iter().all(|arg| arg.label.is_none()));
        assert!(args[1].is_var);
        assert!(args[2].spread);

        let labeled = tail_expr("f(a, b: c, d: var e, rest: ...g)");
        let ExprKind::Call { args, .. } = &labeled.kind else {
            panic!("expected a call");
        };
        assert_eq!(args.len(), 4);
        assert!(args[0].label.is_none());
        assert_eq!(args[1].label.as_ref().unwrap().node, "b");
        assert!(args[2].is_var);
        assert!(args[3].spread);
        assert_eq!(args[3].label.as_ref().unwrap().node, "rest");
    }

    #[test]
    fn positional_argument_after_label_is_an_error() {
        let diagnostics = errors("fn main() { f(a: 1, 2) }");
        assert_eq!(codes(&diagnostics), ["cove::parse::positional_after_label"]);
        assert!(diagnostics[0].rule.is_some());
        assert!(diagnostics[0].help.is_some());
    }

    #[test]
    fn generic_call_arguments_are_disambiguated_from_comparison() {
        let call = tail_expr("api.fetch<Array<Booking>>(\"/bookings\")");
        let ExprKind::Call { generics, args, .. } = &call.kind else {
            panic!("expected a call");
        };
        assert_eq!(generics.len(), 1);
        assert_eq!(args.len(), 1);

        let empty = tail_expr("request.json<CreateBookingRequest>()");
        let ExprKind::Call { generics, args, .. } = &empty.kind else {
            panic!("expected a call");
        };
        assert_eq!(generics.len(), 1);
        assert!(args.is_empty());

        let comparison = tail_expr("a < b");
        assert!(matches!(
            comparison.kind,
            ExprKind::Binary {
                op: BinaryOp::Lt,
                ..
            }
        ));

        let mixed = tail_expr("a < b && c > d");
        assert!(matches!(
            mixed.kind,
            ExprKind::Binary {
                op: BinaryOp::And,
                ..
            }
        ));
    }

    #[test]
    fn parses_the_three_trailing_closure_shapes() {
        let bare = tail_expr("tasks.spawn { 1 }");
        let ExprKind::Call { args, trailing, .. } = &bare.kind else {
            panic!("expected a call");
        };
        assert!(args.is_empty());
        assert!(matches!(
            trailing.as_deref().map(|value| &value.kind),
            Some(ExprKind::Lambda {
                is_async: false,
                ..
            })
        ));

        let after_args = tail_expr("clock.timeout(500ms) { 1 }");
        let ExprKind::Call { args, trailing, .. } = &after_args.kind else {
            panic!("expected a call");
        };
        assert_eq!(args.len(), 1);
        assert!(trailing.is_some());

        let then_try = tail_expr("value.mapError { ConfigError.InvalidPort(raw) }?");
        let ExprKind::Try(inner) = &then_try.kind else {
            panic!("expected `?`");
        };
        assert!(matches!(inner.kind, ExprKind::Call { .. }));
    }

    #[test]
    fn control_flow_headers_do_not_take_trailing_closures() {
        let conditional = tail_expr("if ready { 1 } else if other { 2 } else { 3 }");
        let ExprKind::If {
            condition,
            else_branch,
            ..
        } = &conditional.kind
        else {
            panic!("expected an if");
        };
        assert!(matches!(condition.kind, ExprKind::Ident(_)));
        assert!(matches!(
            else_branch.as_deref().map(|value| &value.kind),
            Some(ExprKind::If { .. })
        ));

        let loop_expr = tail_expr("for item in items.all() { item }");
        let ExprKind::For { iterable, .. } = &loop_expr.kind else {
            panic!("expected a for loop");
        };
        assert!(matches!(
            iterable.kind,
            ExprKind::Call { trailing: None, .. }
        ));

        let while_expr = tail_expr("while running { step() }");
        assert!(matches!(while_expr.kind, ExprKind::While { .. }));

        let scrutinee = tail_expr("match value { _ => 1 }");
        let ExprKind::Match { scrutinee, .. } = &scrutinee.kind else {
            panic!("expected a match");
        };
        assert!(matches!(scrutinee.kind, ExprKind::Ident(_)));
    }

    #[test]
    fn parses_every_pattern_form() {
        let value = tail_expr(
            "match value {\n  _ => 1\n  other => 2\n  \"debug\" => 3\n  1 => 4\n  true => 5\n  Ok(inner) => 6\n  LogLevel.Debug => 7\n  ConfigError.InvalidPort(raw) => 8\n}",
        );
        let ExprKind::Match { arms, .. } = &value.kind else {
            panic!("expected a match");
        };
        assert_eq!(arms.len(), 8);
        assert!(matches!(arms[0].pattern.kind, PatternKind::Wildcard));
        assert!(matches!(&arms[1].pattern.kind, PatternKind::Binding(name) if name == "other"));
        assert!(matches!(arms[2].pattern.kind, PatternKind::Literal(_)));
        assert!(matches!(arms[3].pattern.kind, PatternKind::Literal(_)));
        assert!(matches!(arms[4].pattern.kind, PatternKind::Literal(_)));
        let PatternKind::Variant { path, payload } = &arms[5].pattern.kind else {
            panic!("expected a variant");
        };
        assert_eq!(path[0].node, "Ok");
        assert_eq!(payload.len(), 1);
        let PatternKind::Variant { path, payload } = &arms[6].pattern.kind else {
            panic!("expected a variant");
        };
        assert_eq!(path.len(), 2);
        assert!(payload.is_empty());
        assert!(matches!(arms[7].pattern.kind, PatternKind::Variant { .. }));
    }

    #[test]
    fn match_arms_accept_blocks_returns_and_trailing_commas() {
        let value = tail_expr("match value {\n  Ok(v) => { v }\n  Err(e) => return Err(e),\n}");
        let ExprKind::Match { arms, .. } = &value.kind else {
            panic!("expected a match");
        };
        assert_eq!(arms.len(), 2);
        assert!(matches!(arms[0].body.kind, ExprKind::Block(_)));
        assert!(matches!(arms[1].body.kind, ExprKind::Return(Some(_))));
    }

    #[test]
    fn await_binds_tighter_than_binary_operators() {
        let value = tail_expr("await handler(event)?");
        let ExprKind::Await(inner) = &value.kind else {
            panic!("expected an await");
        };
        let ExprKind::Try(call) = &inner.kind else {
            panic!("expected a `?`");
        };
        assert!(matches!(call.kind, ExprKind::Call { .. }));

        let sum = tail_expr("await a + b");
        let ExprKind::Binary { op, lhs, .. } = &sum.kind else {
            panic!("expected an addition");
        };
        assert_eq!(*op, BinaryOp::Add);
        assert!(matches!(lhs.kind, ExprKind::Await(_)));
    }

    #[test]
    fn await_is_also_an_ordinary_member_name() {
        let value = tail_expr("bookings.await()?");
        let ExprKind::Try(call) = &value.kind else {
            panic!("expected a `?`");
        };
        let ExprKind::Call { callee, .. } = &call.kind else {
            panic!("expected a call");
        };
        let ExprKind::Field { name, .. } = &callee.kind else {
            panic!("expected a field");
        };
        assert_eq!(name.node, "await");
    }

    #[test]
    fn ranges_are_distinct_from_float_literals() {
        let exclusive = tail_expr("0..<attempts");
        let ExprKind::Range { inclusive_end, .. } = &exclusive.kind else {
            panic!("expected a range");
        };
        assert!(!inclusive_end);

        let inclusive = tail_expr("0..count");
        let ExprKind::Range { inclusive_end, .. } = &inclusive.kind else {
            panic!("expected a range");
        };
        assert!(inclusive_end);

        assert!(matches!(tail_expr("0.5").kind, ExprKind::Float(_)));
        assert!(matches!(tail_expr("500ms").kind, ExprKind::Duration(_)));
    }

    #[test]
    fn precedence_runs_from_assignment_to_postfix() {
        let value = tail_expr("total = a + b * c == d && e || f");
        let ExprKind::Assign { op, value, .. } = &value.kind else {
            panic!("expected an assignment");
        };
        assert!(op.is_none());
        assert!(matches!(
            value.kind,
            ExprKind::Binary {
                op: BinaryOp::Or,
                ..
            }
        ));

        let compound = tail_expr("self.requests += 1");
        let ExprKind::Assign { op, target, .. } = &compound.kind else {
            panic!("expected an assignment");
        };
        assert_eq!(*op, Some(BinaryOp::Add));
        assert!(matches!(target.kind, ExprKind::Field { .. }));

        let unary = tail_expr("!ready");
        assert!(matches!(
            unary.kind,
            ExprKind::Unary {
                op: UnaryOp::Not,
                ..
            }
        ));
    }

    #[test]
    fn parses_primary_expression_forms() {
        assert!(matches!(tail_expr("()").kind, ExprKind::Unit));
        assert!(matches!(tail_expr("(1 + 2)").kind, ExprKind::Binary { .. }));
        assert!(matches!(tail_expr("[1, 2]").kind, ExprKind::ArrayLit(_)));
        assert!(matches!(tail_expr("[]").kind, ExprKind::ArrayLit(_)));
        assert!(matches!(tail_expr("self").kind, ExprKind::Ident(_)));
        assert!(matches!(tail_expr("return").kind, ExprKind::Return(None)));
        assert!(matches!(tail_expr("{ 1 }").kind, ExprKind::Block(_)));
        assert!(matches!(
            tail_expr("scope tasks { 1 }").kind,
            ExprKind::Scope { .. }
        ));
    }

    #[test]
    fn parses_lambda_forms() {
        let plain = tail_expr("fn(request) { request }");
        let ExprKind::Lambda {
            is_async, params, ..
        } = &plain.kind
        else {
            panic!("expected a lambda");
        };
        assert!(!is_async);
        assert_eq!(params.len(), 1);
        assert!(params[0].ty.is_none());

        let mutating = tail_expr("fn(var metrics) { metrics }");
        let ExprKind::Lambda { params, .. } = &mutating.kind else {
            panic!("expected a lambda");
        };
        assert!(params[0].is_var);

        let parameterless = tail_expr("async fn { 1 }");
        let ExprKind::Lambda {
            is_async, params, ..
        } = &parameterless.kind
        else {
            panic!("expected a lambda");
        };
        assert!(is_async);
        assert!(params.is_empty());
    }

    #[test]
    fn blocks_take_their_last_expression_as_a_value() {
        let unit = ok("fn f() -> Int {\n  let a = 1\n  a + 1\n}");
        let body = &fn_decl(&unit.items[0]).body;
        assert_eq!(body.statements.len(), 1);
        assert!(matches!(body.statements[0].kind, StmtKind::Let { .. }));
        assert!(matches!(
            body.tail.as_deref().map(|value| &value.kind),
            Some(ExprKind::Binary { .. })
        ));

        let unit = ok("fn f() {\n  a()\n  b()\n}");
        let body = &fn_decl(&unit.items[0]).body;
        assert_eq!(body.statements.len(), 1);
        assert!(body.tail.is_some());

        let unit = ok("fn f() {\n  let a = 1\n}");
        let body = &fn_decl(&unit.items[0]).body;
        assert_eq!(body.statements.len(), 1);
        assert!(body.tail.is_none());
    }

    #[test]
    fn statements_include_bindings_and_nested_items() {
        let unit = ok("fn f() {\n  let a: Int = 1\n  var b = 2\n  fn helper() {}\n  helper()\n}");
        let body = &fn_decl(&unit.items[0]).body;
        assert!(matches!(
            &body.statements[0].kind,
            StmtKind::Let {
                is_var: false,
                ty: Some(_),
                ..
            }
        ));
        assert!(matches!(
            body.statements[1].kind,
            StmtKind::Let { is_var: true, .. }
        ));
        assert!(matches!(body.statements[2].kind, StmtKind::Item(_)));
        assert!(body.tail.is_some());
    }

    #[test]
    fn parses_string_interpolation() {
        let value = tail_expr("\"Hello, {name}! {a + b}\"");
        let ExprKind::Str(parts) = &value.kind else {
            panic!("expected a string");
        };
        assert_eq!(parts.len(), 4);
        assert!(matches!(&parts[0], StrPart::Text(text) if text == "Hello, "));
        assert!(matches!(
            &parts[1],
            StrPart::Interpolation(Expr {
                kind: ExprKind::Ident(_),
                ..
            })
        ));
        assert!(matches!(
            &parts[3],
            StrPart::Interpolation(Expr {
                kind: ExprKind::Binary { .. },
                ..
            })
        ));
    }

    #[test]
    fn interpolation_spans_are_rebased_onto_the_original_file() {
        let source = "fn main() {\n  \"count: {name}\"\n}";
        let unit = ok(source);
        let tail = fn_decl(&unit.items[0]).body.tail.as_deref().unwrap();
        let ExprKind::Str(parts) = &tail.kind else {
            panic!("expected a string");
        };
        let StrPart::Interpolation(value) = &parts[1] else {
            panic!("expected an interpolation");
        };
        let start = source.find("name").unwrap() as u32;
        assert_eq!(value.span.start, start);
        assert_eq!(value.span.end, start + 4);
    }

    #[test]
    fn diagnostics_inside_an_interpolation_point_into_the_original_file() {
        let source = "fn main() {\n  \"count: {name ] rest}\"\n}";
        let diagnostics = errors(source);
        assert_eq!(codes(&diagnostics), ["cove::parse::unexpected_token"]);
        let span = diagnostics[0].primary.expect("a primary span");
        assert_eq!(span.start as usize, source.find(']').unwrap());
        assert_eq!(span.file, FileId(0));
    }

    #[test]
    fn empty_interpolation_is_an_error() {
        let diagnostics = errors("fn main() { \"a {  } b\" }");
        assert_eq!(codes(&diagnostics), ["cove::parse::empty_interpolation"]);
    }

    #[test]
    fn recovers_from_a_broken_statement_and_keeps_parsing() {
        let diagnostics = errors("fn a() { let = 1 }\nfn b() { let = 2 }");
        assert_eq!(
            codes(&diagnostics),
            [
                "cove::parse::unexpected_token",
                "cove::parse::unexpected_token"
            ]
        );
    }

    #[test]
    fn recovers_from_a_broken_item_at_the_next_declaration() {
        let diagnostics = errors("fn a( {}\nstruct S { x: Int }\nenum E { A }\nfn ! {}");
        assert_eq!(diagnostics.len(), 2);
        assert!(diagnostics
            .iter()
            .all(|d| d.code == "cove::parse::unexpected_token"));
    }

    #[test]
    fn reports_a_statement_that_is_not_a_declaration_at_the_top_level() {
        let diagnostics = errors("let x = 1\nfn a() {}");
        assert_eq!(codes(&diagnostics), ["cove::parse::unexpected_token"]);
        assert!(diagnostics[0].message.contains("expected a declaration"));
    }

    #[test]
    fn rejects_assignment_to_a_non_place() {
        let diagnostics = errors("fn a() { f() = 1 }");
        assert_eq!(
            codes(&diagnostics),
            ["cove::parse::invalid_assignment_target"]
        );
    }

    #[test]
    fn reports_several_independent_errors_in_one_run() {
        let diagnostics = errors("fn a() { f(x: 1, 2) }\n/// dangling\n1\nfn b() { let = 1 }");
        assert_eq!(
            codes(&diagnostics),
            [
                "cove::parse::positional_after_label",
                "cove::parse::dangling_doc_comment",
                "cove::parse::unexpected_token"
            ]
        );
    }

    #[test]
    fn unexpected_token_names_what_was_expected_and_found() {
        let diagnostics = errors("fn a(1) {}");
        assert_eq!(
            diagnostics[0].message,
            "expected identifier, found integer literal"
        );
    }

    fn collect_cove_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries = std::fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", dir.display()));
        for entry in entries {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                collect_cove_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "cove") {
                out.push(path);
            }
        }
    }

    #[test]
    fn every_example_program_parses() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let mut files = Vec::new();
        collect_cove_files(&root, &mut files);
        files.sort();
        assert!(
            files.len() >= 7,
            "expected the example programs to be found"
        );

        for path in files {
            let relative = path
                .strip_prefix(&root)
                .expect("a path under examples")
                .to_string_lossy()
                .replace('\\', "/");
            let text = std::fs::read_to_string(&path).expect("readable example");
            let mut sources = SourceMap::new();
            let file = sources.add(path.clone(), text);
            let result = crate::parse_file(&sources, file);
            if let Err(diagnostics) = result {
                let rendered: String = diagnostics
                    .iter()
                    .map(|diagnostic| cove_diag::render(&sources, diagnostic))
                    .collect();
                panic!("{relative} failed to parse:\n{rendered}");
            }
        }
    }
}
