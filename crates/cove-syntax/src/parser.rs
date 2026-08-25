//! The Cove parser.
//!
//! Turns a token stream into an [`ast::SourceUnit`](crate::ast::SourceUnit). Cove has no statement
//! terminators: `;` is not part of the language. Instead, as in Go and Swift,
//! a newline ends a statement when the line could have ended there. The last
//! expression of a block is still that block's value.
//!
//! # The newline rule
//!
//! A line break ends the current expression when all of the following hold:
//!
//! 1. the token after the break carries [`Token::preceded_by_newline`];
//! 2. the token before the break can end an expression — an identifier,
//!    `self`, a literal, `)`, `]`, `}`, `?`, or `...` (see
//!    `ends_expression`);
//! 3. the parser is at a point where continuing is optional: a postfix `(`,
//!    `<` generic argument list, or `{` trailing closure, or a binary,
//!    range, or assignment operator;
//! 4. the parser is not inside a `(`, `[`, or `<` group (see
//!    `Parser::grouped`). `{` is not such a group: the statements of a
//!    block do end at newlines.
//!
//! Two exceptions keep familiar code working. A line that starts with `.`
//! continues the previous expression, so a method chain may be split across
//! lines; this falls out of rule 3, because `.` is never an optional
//! continuation. And the continuation keywords `else` and `=>` are read
//! across a line break as well, so `}` followed by a newline and `else` still
//! attaches.
//!
//! Because rule 3 looks at the operator rather than the operand, an operator
//! at the *end* of a line continues onto the next line (`a +` / `b` is one
//! expression) while an operator at the *start* of a line does not (`a` /
//! `+ b` is two statements).
//!
//! # The nesting limit
//!
//! Recursive descent spends native stack per level of nesting, so the parser
//! bounds nesting rather than discovering the bound when the stack runs out.
//! Source that nests deeper than `MAX_NESTING_DEPTH` levels is a
//! `cove::parse::nesting_too_deep` diagnostic like any other and the file is
//! refused, instead of the process ending in a stack overflow that no caller
//! can catch. A level is any construct written inside another, and also each
//! link of a left-associative chain, because the tree such a chain builds is
//! as deep as the chain is long and everything downstream walks that tree by
//! recursing. See the constant for what the number is calibrated against.
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

/// The modifiers written in front of a declaration.
struct ItemModifiers {
    exported: bool,
    is_test: bool,
}

type PResult<T> = Result<T, Bail>;

/// The smallest native stack the parser promises to read a file on.
///
/// Recursive descent spends stack per level of nesting in the file it reads,
/// so a bound on nesting is only worth as much as the stack it is calibrated
/// against. The toolchain gives the parser a generous one — every `cove`
/// command runs its whole dispatch on `cove_runtime::STACK_SIZE`, which is
/// about 106 MiB in a debug build — but that number is unreachable from here
/// and must stay so: `cove-runtime` depends on `cove-syntax`, not the other
/// way round, and [`parse`] is a library entry point that an editor plugin, a
/// language server, or a test may call on any thread it likes.
///
/// So the promise is made against the smallest stack such a caller plausibly
/// has: the platform default for a thread nobody sized, which is 2 MiB on
/// macOS, Linux, and Windows alike, and is also what Rust's test harness
/// gives each test. A process main thread is larger than that everywhere
/// except Windows, where it is 1 MiB — the one stack this figure does not
/// cover, and only in a debug build, since a release build spends a fifth as
/// much per level and fits the limit into 400 KiB. No `cove` command parses on a main thread, because
/// `main` hands the whole dispatch to `cove_runtime::on_cove_stack`, so what
/// is left uncovered is an embedder that both builds without optimizations
/// and parses on the main thread of a Windows process.
const NESTING_STACK: usize = 2 * 1024 * 1024;

/// The native stack one level of nesting costs the parser in a debug build.
///
/// Measured on macOS the way `cove_runtime::STACK_PER_FRAME` was: files of
/// increasing nesting are parsed on threads of two known sizes and the
/// deepest that parses cleanly is binary-searched on each, so the figure is
/// the slope between the two sizes and whatever the parser spends before the
/// nesting starts cancels out. Eighteen shapes were measured, at 4 MiB and 16
/// MiB in a debug build and at 1 MiB and 4 MiB in a release one. Per level of
/// [`Parser::depth`], which is what [`MAX_NESTING_DEPTH`] counts, the worst of
/// them were:
///
/// | nesting                       | debug    | release |
/// |-------------------------------|----------|---------|
/// | `"a{"a{ ... }"}"`             | 28.8 KiB | 5.3 KiB |
/// | `match x { _ => ... }`        | 28.6 KiB | 6.2 KiB |
/// | `[[[ ... ]]]`                 | 26.4 KiB | 5.3 KiB |
/// | `((( ... )))`                 | 25.7 KiB | 5.3 KiB |
/// | `g(g( ... ))`                 | 21.7 KiB | 4.4 KiB |
/// | `if true { ... } else { 0 }`  | 16.4 KiB | 3.7 KiB |
/// | `fn g() { fn g() { ... } }`   | 12.0 KiB | 3.7 KiB |
/// | `Some(Some( ... ))`           |  3.4 KiB | 0.8 KiB |
/// | `Array<Array< ... >>`         |  2.8 KiB | 0.6 KiB |
///
/// The cheap shapes at the bottom are the ones whose level is a single frame:
/// a type argument list or a pattern payload re-enters one function, while a
/// parenthesised expression re-enters the whole precedence chain from
/// `parse_expr` down to `parse_primary`. The braced forms look cheap per
/// source level and are not: a block raises the depth twice per level, once
/// for the block and once for the expression it is the body of, so the figure
/// per level of nesting a reader would count is double what this table shows.
///
/// The figure is the parser's, and the parser is the most expensive stage of
/// the toolchain per level: measured the same way, `cove check` and `cove
/// fmt` over the same files show the same slope to within a fifth of a
/// kibibyte, so what the resolver, the type checker, and the formatter spend
/// walking a tree of a given depth is less than what the parser spent
/// building it. That is why one limit here bounds the whole pipeline and none
/// of them needs a limit of its own. A chain link is cheaper still — it costs
/// the parser nothing, because a chain is parsed by a loop, and the walkers
/// about 2 KiB — and [`MAX_NESTING_DEPTH`] charges it a whole level anyway,
/// which is margin rather than measurement.
///
/// The number here is 32 KiB, the worst measured figure rounded up, and it is
/// deliberately not `#[cfg(debug_assertions)]`-conditional as its runtime
/// counterpart is. [`MAX_NESTING_DEPTH`] is derived from it and is visible to
/// whoever writes the file, so a file that parses in a release build must
/// parse in a debug build; taking the worse profile for both is what makes
/// that true, and it leaves a release build five times the headroom it needs.
const STACK_PER_LEVEL: usize = 32 * 1024;

/// How deeply source may nest before the parser reports a limit instead of
/// exhausting its native stack.
///
/// Sixty-four, and derived rather than chosen: [`NESTING_STACK`] is the stack
/// the parser promises to work on and [`STACK_PER_LEVEL`] is what a level of
/// it costs, so this is how many levels fit.
///
/// A level is anything that puts one construct inside another, counted in one
/// place — [`Parser::depth`] — rather than once per construct, because
/// expressions, blocks, types, and patterns all spend the same stack and a
/// file that alternates between them would pass four separate limits while
/// exhausting the stack anyway. Two things raise it. Every point at which the
/// parser re-enters itself raises it through [`Parser::nested`], which is
/// what bounds the parser's own recursion. And every link of a
/// left-associative chain raises it through [`Parser::link`], which is not
/// recursion at all — `a.b.c` and `1 + 2 + 3` are parsed by a loop — but
/// builds a tree as deep as the chain is long, and the resolver, the type
/// checker, the formatter, and the interpreter all recurse over that tree
/// afterwards. Counting both in one number is what makes the bound hold along
/// a path: a chain hanging off a nested expression is as deep as its links
/// plus the nesting above it, and that is the sum this counter carries.
///
/// So the limit is spent by more than parentheses, and this is the one place
/// where it is a constraint on source anyone would write: a chain of more
/// than sixty-four operands, `1 + 2 + ... + 65`, is refused, as is a method
/// chain of more than thirty-two calls, since a call is a `.` and a `(`. The
/// deepest file in this repository reaches twenty-four levels —
/// `examples/callbacks/main.cove`, whose server loop nests a `scope`, a
/// spawned closure, a callback given to `clock.every`, a `lock` closure, and
/// an interpolated string inside a call, with the field and call links of
/// those chains counted in. Sixty-four is not a lot of room above that, and
/// the alternative was worse: the number is what a 2 MiB stack holds, and
/// raising it means promising a stack no unsized thread has. Raising it later
/// is a compatible change and lowering it is not, which is the direction to
/// err in.
///
/// Past the limit the parser reports `cove::parse::nesting_too_deep` and
/// recovers as it does from any other parse error, so a file that nests a
/// million parentheses produces a diagnostic rather than ending the process.
///
/// This is the parser's half of the promise `cove_runtime::MAX_CALL_DEPTH`
/// makes at run time, and the two are calibrated in opposite directions for
/// the same reason. The runtime owns the thread it evaluates on, so it sizes
/// the stack to fit the limit; the parser is handed a thread by whoever calls
/// it, so it fits the limit to the stack.
const MAX_NESTING_DEPTH: u32 = (NESTING_STACK / STACK_PER_LEVEL) as u32;

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
    /// How many `(`, `[`, or `<` groups enclose the cursor. A newline inside
    /// such a group never ends a statement, so argument lists, array
    /// literals, and generic argument lists may span lines.
    ///
    /// This is not [`Parser::depth`] and the two must not be merged. This one
    /// answers a question about the language — whether a line break here ends
    /// a statement — and so it counts only the three bracket kinds the
    /// newline rule names, and [`Parser::ungrouped`] resets it to zero inside
    /// a `{ }` block because the rule says a block's statements do end at
    /// newlines. The other answers a question about the machine, counts every
    /// kind of nesting there is, and may never be reset. A counter that did
    /// both jobs would have to be wrong about one of them.
    group_depth: u32,
    /// How deep the tree under construction is at the cursor, bounded by
    /// [`MAX_NESTING_DEPTH`].
    ///
    /// One counter serves every construct that can contain another —
    /// expressions, blocks, types, and patterns alike, and the links of a
    /// chain besides — because they all end up on the same native stack, and
    /// a counter for each would let a file that alternates between them pass
    /// every limit while exhausting that stack anyway.
    depth: u32,
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

/// The rule an operator at the start of a line breaks, stated for the reader.
const NEWLINE_OPERATOR_RULE: &str = "A newline ends a statement when the expression before it is \
     complete, so an operator that continues an expression stays on the line it continues.";

/// Whether a token can be the last token of an expression.
///
/// This is the first half of the newline rule: a line break only ends a
/// statement when the line so far reads as a complete expression.
fn ends_expression(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident(_)
            | TokenKind::Keyword(Keyword::SelfValue)
            | TokenKind::Int(_)
            | TokenKind::Float(_)
            | TokenKind::Bool(_)
            | TokenKind::Duration(_)
            | TokenKind::Str(_)
            | TokenKind::RParen
            | TokenKind::RBracket
            | TokenKind::RBrace
            | TokenKind::Question
            | TokenKind::Ellipsis
    )
}

/// Whether a token can only ever continue an expression, never begin one.
///
/// Such a token at the start of a line is always a statement that the newline
/// rule has just cut in two, which [`Parser::expected_expression`] explains.
fn continues_expression_only(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Plus
            | TokenKind::Star
            | TokenKind::Slash
            | TokenKind::Percent
            | TokenKind::EqEq
            | TokenKind::BangEq
            | TokenKind::Lt
            | TokenKind::LtEq
            | TokenKind::Gt
            | TokenKind::GtEq
            | TokenKind::AmpAmp
            | TokenKind::PipePipe
            | TokenKind::Eq
            | TokenKind::PlusEq
            | TokenKind::MinusEq
            | TokenKind::StarEq
            | TokenKind::SlashEq
            | TokenKind::PercentEq
            | TokenKind::DotDot
            | TokenKind::DotDotLt
            | TokenKind::Keyword(Keyword::Is)
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
                preceded_by_newline: false,
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
            group_depth: 0,
            depth: 0,
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
        let mut diagnostic = Diagnostic::error(
            "cove::parse::unexpected_token",
            format!("expected {expected}, found {found}"),
        )
        .at(span);
        diagnostic = self.note_newline_rule(diagnostic);
        self.error(diagnostic);
        Bail
    }

    /// Adds the newline rule to `diagnostic` when the token it reports was cut
    /// off from the previous line, which is otherwise easy to misread as a
    /// problem with the token itself.
    fn note_newline_rule(&self, diagnostic: Diagnostic) -> Diagnostic {
        if !(self.at_statement_break() && continues_expression_only(self.peek())) {
            return diagnostic;
        }
        diagnostic
            .label(self.prev_span(), "a newline ended the statement here")
            .rule(NEWLINE_OPERATOR_RULE)
            .help("Move this operator to the end of the previous line.")
    }

    /// Reports a token that cannot begin an expression.
    ///
    /// When the token could only have continued the previous line, the
    /// diagnostic explains that the newline ended that statement instead of
    /// repeating the generic "expected an expression".
    fn expected_expression(&mut self) -> Bail {
        if !(self.at_statement_break() && continues_expression_only(self.peek())) {
            return self.unexpected("an expression");
        }
        let operator = self.peek().describe();
        let span = self.span();
        let previous = self.prev_span();
        self.error(
            Diagnostic::error(
                "cove::parse::newline_ended_statement",
                format!("{operator} cannot start a statement"),
            )
            .at(span)
            .label(previous, "the previous statement ended here")
            .rule(NEWLINE_OPERATOR_RULE)
            .help("Move this operator to the end of the previous line."),
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

    /// Runs `parse` inside a `(`, `[`, or `<` group, where line breaks never
    /// end a statement.
    fn grouped<T>(&mut self, parse: impl FnOnce(&mut Self) -> T) -> T {
        self.group_depth += 1;
        let result = parse(self);
        self.group_depth -= 1;
        result
    }

    /// Runs `parse` as the body of a `{ ... }` block, where line breaks end
    /// statements again even when the block itself sits inside a group, as in
    /// a lambda passed as an argument.
    fn ungrouped<T>(&mut self, parse: impl FnOnce(&mut Self) -> T) -> T {
        let saved = self.group_depth;
        self.group_depth = 0;
        let result = parse(self);
        self.group_depth = saved;
        result
    }

    /// Runs `parse` one level of nesting deeper, refusing to descend past
    /// [`MAX_NESTING_DEPTH`].
    ///
    /// Every place the parser re-enters itself goes through here, which is
    /// what bounds the native stack a file can spend: [`Parser::depth`] is
    /// raised before `parse` runs and lowered after it, and the two cannot
    /// come apart because a parser that gives up returns `Err(Bail)` as an
    /// ordinary value rather than unwinding, so the failing path leaves
    /// through the same line as the succeeding one.
    fn nested<T>(&mut self, parse: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        if self.depth >= MAX_NESTING_DEPTH {
            return Err(self.nesting_too_deep());
        }
        self.depth += 1;
        let result = parse(self);
        self.depth -= 1;
        result
    }

    /// Raises the depth for one more link of a left-associative chain.
    ///
    /// A chain is built by a loop rather than by recursion, so it costs the
    /// parser nothing, but the tree it builds is as deep as the chain is
    /// long and everything that walks that tree afterwards recurses over it.
    /// A link therefore costs a level exactly as nesting does, and it is held
    /// for as long as the chain is being built: the loop runs inside
    /// [`Parser::chained`], which puts the depth back when the chain is done.
    fn link(&mut self) -> PResult<()> {
        if self.depth >= MAX_NESTING_DEPTH {
            return Err(self.nesting_too_deep());
        }
        self.depth += 1;
        Ok(())
    }

    /// Runs `parse`, which builds a left-associative chain by repeated
    /// [`Parser::link`], and restores the depth those links raised.
    fn chained<T>(&mut self, parse: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        let saved = self.depth;
        let result = parse(self);
        self.depth = saved;
        result
    }

    /// Reports source that nests deeper than the parser will descend.
    fn nesting_too_deep(&mut self) -> Bail {
        let span = self.span();
        self.error(
            Diagnostic::error(
                "cove::parse::nesting_too_deep",
                format!("this nests more than {MAX_NESTING_DEPTH} levels deep"),
            )
            .at(span)
            .rule(format!(
                "Source nests no more than {MAX_NESTING_DEPTH} levels deep, counting each link \
                 of a chain such as `a.b.c` or `1 + 2 + 3` as a level of its own."
            ))
            .help("Give an inner part a name of its own with `let`, or lift it into a function."),
        );
        Bail
    }

    /// Whether a line break at the cursor ends the current statement.
    ///
    /// Callers ask this only where continuing the expression is optional, so
    /// the answer decides between one expression and two statements. See the
    /// module documentation for the full rule.
    fn at_statement_break(&self) -> bool {
        if self.group_depth > 0 || self.pos == 0 {
            return false;
        }
        let next = &self.tokens[self.pos];
        if !next.preceded_by_newline {
            return false;
        }
        // A line starting with `.` continues a method chain.
        if matches!(next.kind, TokenKind::Dot) {
            return false;
        }
        ends_expression(&self.tokens[self.pos - 1].kind)
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
        while let TokenKind::DocComment(text) = self.peek() {
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
                Keyword::Export
                | Keyword::Test
                | Keyword::Struct
                | Keyword::Enum
                | Keyword::Trait
                | Keyword::Impl
                | Keyword::Type,
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
                TokenKind::Keyword(
                    Keyword::Let
                        | Keyword::Var
                        | Keyword::Return
                        | Keyword::Break
                        | Keyword::Continue
                ) | TokenKind::DocComment(_)
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
        let modifiers = self.parse_item_modifiers();
        let keyword = match self.peek() {
            TokenKind::Keyword(keyword) => Some(*keyword),
            _ => None,
        };
        let kind = match keyword {
            Some(Keyword::Fn | Keyword::Async) => ItemKind::Fn(self.parse_fn_decl()?),
            Some(Keyword::Struct) => ItemKind::Struct(self.parse_struct_decl()?),
            Some(Keyword::Enum) => ItemKind::Enum(self.parse_enum_decl()?),
            Some(Keyword::Trait) => ItemKind::Trait(self.parse_trait_decl()?),
            Some(Keyword::Impl) => ItemKind::Impl(self.parse_impl_block()?),
            Some(Keyword::Type) => ItemKind::TypeAlias(self.parse_type_alias()?),
            _ => return Err(self.unexpected("a declaration")),
        };
        let span = start.to(self.prev_span());
        if modifiers.is_test && !matches!(kind, ItemKind::Fn(_)) {
            self.error(
                Diagnostic::error(
                    "cove::parse::test_not_a_function",
                    "`test` marks a function, not this declaration",
                )
                .at(span)
                .rule("`test` marks a `fn` the test runner calls; no other declaration is a test.")
                .help("Remove `test`, or move the behaviour into a `test fn`."),
            );
        }
        Ok(Item {
            doc,
            exported: modifiers.exported,
            is_test: modifiers.is_test,
            kind,
            span,
        })
    }

    /// Reads the `export` or `test` in front of a declaration.
    ///
    /// The two occupy one position and answer one question — who may call
    /// this — so a declaration carries at most one of them, written once.
    /// Every modifier is read before anything is reported, rather than
    /// stopping at the first, so recovery continues at the declaration
    /// itself however the mistake was written.
    fn parse_item_modifiers(&mut self) -> ItemModifiers {
        let mut exported: Option<Span> = None;
        let mut is_test: Option<Span> = None;
        loop {
            let span = self.span();
            let (seen, keyword) = if self.at_keyword(Keyword::Export) {
                (&mut exported, Keyword::Export)
            } else if self.at_keyword(Keyword::Test) {
                (&mut is_test, Keyword::Test)
            } else {
                break;
            };
            self.bump();
            let repeated = seen.is_some();
            seen.get_or_insert(span);
            if repeated {
                self.error(
                    Diagnostic::error(
                        "cove::parse::repeated_modifier",
                        format!("`{}` is written twice", keyword.as_str()),
                    )
                    .at(span)
                    .rule("A declaration carries each modifier at most once.")
                    .help(format!("Remove the second `{}`.", keyword.as_str())),
                );
            }
        }
        if let (Some(exported), Some(is_test)) = (exported, is_test) {
            self.error(
                Diagnostic::error(
                    "cove::parse::exported_test",
                    "a `test fn` may not be exported",
                )
                .at(exported.to(is_test))
                .rule(
                    "A test's whole contract is that the test runner is its only caller, so `test` and `export` cannot both apply to one declaration.",
                )
                .help("Remove `export`, or remove `test` and call it like any other declaration."),
            );
        }
        ItemModifiers {
            // A rejected `export` is dropped rather than kept, so nothing
            // downstream sees a declaration that is both.
            exported: exported.is_some() && is_test.is_none(),
            is_test: is_test.is_some(),
        }
    }

    /// Reports a `test fn` written anywhere but at the top level of a file.
    ///
    /// A test belongs to a module, which is what lets it see the module's
    /// private declarations; a method or a local function is reached through
    /// something else, and the runner cannot call it.
    fn reject_nested_test(&mut self, item: &Item, place: &str) {
        if !item.is_test {
            return;
        }
        self.error(
            Diagnostic::error(
                "cove::parse::nested_test",
                format!("a `test fn` may not be declared {place}"),
            )
            .at(item.span)
            .rule("A test is a top-level declaration of its module, which is what the test runner calls.")
            .help("Move the `test fn` to the top level of the file."),
        );
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

    /// `<T, U: Display + Ordered>`, or nothing.
    ///
    /// A bound names a trait the type argument must conform to, and is
    /// checked at the call site that instantiates the parameter.
    fn parse_generic_params(&mut self) -> PResult<Vec<GenericParam>> {
        if !self.eat(&TokenKind::Lt) {
            return Ok(Vec::new());
        }
        self.grouped(|parser| {
            let mut generics = Vec::new();
            while !parser.at(&TokenKind::Gt) && !parser.is_eof() {
                generics.push(parser.parse_generic_param()?);
                if !parser.eat(&TokenKind::Comma) {
                    break;
                }
            }
            parser.expect(&TokenKind::Gt, "`>`")?;
            Ok(generics)
        })
    }

    /// `T`, or `T: Display`, or `T: Display + Ordered`.
    fn parse_generic_param(&mut self) -> PResult<GenericParam> {
        let start = self.span();
        let name = self.expect_ident()?;
        let mut bounds = Vec::new();
        if self.eat(&TokenKind::Colon) {
            loop {
                bounds.push(self.expect_ident()?);
                if !self.eat(&TokenKind::Plus) {
                    break;
                }
            }
        }
        Ok(GenericParam {
            name,
            bounds,
            span: start.to(self.prev_span()),
        })
    }

    /// Parses a parameter list up to and including its `)`. A leading `self`
    /// or `var self` is the method receiver rather than a parameter.
    fn parse_param_list(&mut self) -> PResult<(Option<Receiver>, Vec<Param>)> {
        self.grouped(Parser::parse_param_list_inner)
    }

    fn parse_param_list_inner(&mut self) -> PResult<(Option<Receiver>, Vec<Param>)> {
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

    /// `trait Name { fn method(self) -> T ... }`.
    ///
    /// A method may end at its signature, which makes it required, or carry a
    /// `{ ... }` default body, which makes it optional for a conformance.
    fn parse_trait_decl(&mut self) -> PResult<TraitDecl> {
        let start = self.expect_keyword(Keyword::Trait, "`trait`")?;
        let name = self.expect_ident()?;
        self.expect(&TokenKind::LBrace, "`{`")?;

        let mut methods = Vec::new();
        while !self.at(&TokenKind::RBrace) && !self.is_eof() {
            let doc = self.collect_doc();
            if self.at(&TokenKind::RBrace) {
                if let Some((_, span)) = doc {
                    self.dangling_doc(span);
                }
                break;
            }
            match self.parse_trait_method(doc.map(|(text, _)| text)) {
                Ok(method) => methods.push(method),
                Err(Bail) => self.recover_to_item(true),
            }
        }
        self.expect(&TokenKind::RBrace, "`}`")?;

        Ok(TraitDecl {
            name,
            methods,
            span: start.to(self.prev_span()),
        })
    }

    /// One method of a trait. A trait declares no generic methods in the MVP,
    /// so a method binds no type parameters of its own.
    fn parse_trait_method(&mut self, doc: Option<String>) -> PResult<TraitMethod> {
        let start = self.span();
        let is_async = self.eat_keyword(Keyword::Async);
        self.expect_keyword(Keyword::Fn, "`fn`")?;
        let name = self.expect_ident()?;
        self.expect(&TokenKind::LParen, "`(`")?;
        let (receiver, params) = self.parse_param_list()?;
        let return_type = if self.eat(&TokenKind::Arrow) {
            Some(self.parse_type()?)
        } else {
            None
        };
        // A `{` on the same logical line opens a default body; a signature
        // that ends at the line break declares the method without one.
        let default = if self.at(&TokenKind::LBrace) && !self.at_statement_break() {
            Some(self.parse_block()?)
        } else {
            None
        };
        Ok(TraitMethod {
            doc,
            name,
            is_async,
            receiver,
            params,
            return_type,
            default,
            span: start.to(self.prev_span()),
        })
    }

    /// `impl Type { ... }`, or `impl Trait for Type { ... }`.
    fn parse_impl_block(&mut self) -> PResult<ImplBlock> {
        let start = self.expect_keyword(Keyword::Impl, "`impl`")?;
        let mut trait_name = None;
        let mut type_name = self.expect_ident()?;
        if self.eat_keyword(Keyword::For) {
            trait_name = Some(type_name);
            type_name = self.expect_ident()?;
        }
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
                Ok(item) => {
                    self.reject_nested_test(&item, "inside an `impl` block");
                    items.push(item);
                }
                Err(Bail) => self.recover_to_item(true),
            }
        }
        self.expect(&TokenKind::RBrace, "`}`")?;

        Ok(ImplBlock {
            trait_name,
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
        self.nested(Parser::parse_type_inner)
    }

    fn parse_type_inner(&mut self) -> PResult<Type> {
        let start = self.span();
        let kind = match self.peek() {
            TokenKind::LParen => {
                self.bump();
                self.expect(&TokenKind::RParen, "`)`")?;
                TypeKind::Unit
            }
            TokenKind::Keyword(Keyword::Async | Keyword::Fn) => self.parse_fn_type()?,
            TokenKind::Keyword(Keyword::Dyn) => {
                self.bump();
                TypeKind::Dyn(self.expect_ident()?)
            }
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
        let params = self.grouped(|parser| {
            let mut params = Vec::new();
            while !parser.at(&TokenKind::RParen) && !parser.is_eof() {
                params.push(parser.parse_fn_type_param()?);
                if !parser.eat(&TokenKind::Comma) {
                    break;
                }
            }
            parser.expect(&TokenKind::RParen, "`)`")?;
            Ok(params)
        })?;
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
        self.grouped(|parser| {
            let mut args = Vec::new();
            while !parser.at(&TokenKind::Gt) && !parser.is_eof() {
                args.push(parser.parse_type()?);
                if !parser.eat(&TokenKind::Comma) {
                    break;
                }
            }
            parser.expect(&TokenKind::Gt, "`>`")?;
            Ok(args)
        })
    }
}

/// Blocks and statements.
impl Parser<'_> {
    /// Parses `{ ... }`. The last statement, when it is an expression,
    /// becomes the block's value.
    fn parse_block(&mut self) -> PResult<Block> {
        self.nested(Parser::parse_block_inner)
    }

    fn parse_block_inner(&mut self) -> PResult<Block> {
        let start = self.span();
        self.expect(&TokenKind::LBrace, "`{`")?;

        let mut statements = Vec::new();
        self.ungrouped(|parser| {
            parser.scoped(false, |parser| {
                while !parser.at(&TokenKind::RBrace) && !parser.is_eof() {
                    match parser.parse_stmt() {
                        Ok(stmt) => {
                            parser.check_detached_trailing_closure(&stmt);
                            statements.push(stmt);
                        }
                        Err(Bail) => parser.recover_in_block(),
                    }
                }
            })
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

    /// Reports the one shape the newline rule silently changes the meaning
    /// of: a statement that is only a name or a field access, followed by a
    /// `{` on the next line. Such a statement computes nothing on its own, so
    /// the block was meant to be its trailing closure and must start on the
    /// same line.
    fn check_detached_trailing_closure(&mut self, stmt: &Stmt) {
        if !matches!(
            &stmt.kind,
            StmtKind::Expr(Expr {
                kind: ExprKind::Ident(_) | ExprKind::Field { .. },
                ..
            })
        ) {
            return;
        }
        if !self.at(&TokenKind::LBrace) || !self.at_statement_break() {
            return;
        }
        let span = self.span();
        self.error(
            Diagnostic::error(
                "cove::parse::newline_before_trailing_closure",
                "a newline ended the statement before this `{`",
            )
            .at(span)
            .label(stmt.span, "this expression is already complete")
            .rule(
                "A newline ends a statement when the expression before it is complete, so a \
                 trailing closure begins on the same line as the call it belongs to.",
            )
            .help("Move `{` up onto the previous line."),
        );
    }

    fn parse_stmt(&mut self) -> PResult<Stmt> {
        let doc = self.collect_doc();

        if self.at_item_start() {
            let item = self.parse_item(doc.map(|(text, _)| text))?;
            self.reject_nested_test(&item, "inside a block");
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
        self.nested(Parser::parse_assign)
    }

    fn parse_assign(&mut self) -> PResult<Expr> {
        let target = self.parse_or()?;
        if self.at_statement_break() {
            return Ok(target);
        }
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
        let value = self.nested(Parser::parse_assign)?;

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
        self.chained(|parser| {
            let mut lhs = parser.parse_and()?;
            while parser.at(&TokenKind::PipePipe) && !parser.at_statement_break() {
                parser.bump();
                parser.link()?;
                let rhs = parser.parse_and()?;
                lhs = binary(BinaryOp::Or, lhs, rhs);
            }
            Ok(lhs)
        })
    }

    fn parse_and(&mut self) -> PResult<Expr> {
        self.chained(|parser| {
            let mut lhs = parser.parse_comparison()?;
            while parser.at(&TokenKind::AmpAmp) && !parser.at_statement_break() {
                parser.bump();
                parser.link()?;
                let rhs = parser.parse_comparison()?;
                lhs = binary(BinaryOp::And, lhs, rhs);
            }
            Ok(lhs)
        })
    }

    fn parse_comparison(&mut self) -> PResult<Expr> {
        self.chained(|parser| {
            let mut lhs = parser.parse_range()?;
            loop {
                if parser.at_statement_break() {
                    return Ok(lhs);
                }
                let op = match parser.peek() {
                    TokenKind::EqEq => BinaryOp::Eq,
                    TokenKind::BangEq => BinaryOp::Ne,
                    TokenKind::Lt => BinaryOp::Lt,
                    TokenKind::LtEq => BinaryOp::Le,
                    TokenKind::Gt => BinaryOp::Gt,
                    TokenKind::GtEq => BinaryOp::Ge,
                    // `is` compares identity at the same precedence as `==`: the
                    // Language Card lists it alongside value equality, and giving
                    // it a different tier would make `a == b is c` guess which
                    // question is asked first.
                    TokenKind::Keyword(Keyword::Is) => BinaryOp::Is,
                    _ => return Ok(lhs),
                };
                parser.bump();
                parser.link()?;
                let rhs = parser.parse_range()?;
                lhs = binary(op, lhs, rhs);
            }
        })
    }

    /// `0..<attempts` excludes its end; `0..n` includes it.
    fn parse_range(&mut self) -> PResult<Expr> {
        let start = self.parse_additive()?;
        if self.at_statement_break() {
            return Ok(start);
        }
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
        self.chained(|parser| {
            let mut lhs = parser.parse_multiplicative()?;
            loop {
                if parser.at_statement_break() {
                    return Ok(lhs);
                }
                let op = match parser.peek() {
                    TokenKind::Plus => BinaryOp::Add,
                    TokenKind::Minus => BinaryOp::Sub,
                    _ => return Ok(lhs),
                };
                parser.bump();
                parser.link()?;
                let rhs = parser.parse_multiplicative()?;
                lhs = binary(op, lhs, rhs);
            }
        })
    }

    fn parse_multiplicative(&mut self) -> PResult<Expr> {
        self.chained(|parser| {
            let mut lhs = parser.parse_unary()?;
            loop {
                if parser.at_statement_break() {
                    return Ok(lhs);
                }
                let op = match parser.peek() {
                    TokenKind::Star => BinaryOp::Mul,
                    TokenKind::Slash => BinaryOp::Div,
                    TokenKind::Percent => BinaryOp::Rem,
                    _ => return Ok(lhs),
                };
                parser.bump();
                parser.link()?;
                let rhs = parser.parse_unary()?;
                lhs = binary(op, lhs, rhs);
            }
        })
    }

    /// `await` binds tighter than any binary operator and tighter than a
    /// trailing `?`, so `await handler(event)?` awaits the call and then
    /// propagates the error from the `Result` the task produced: `Try(Await(Call))`.
    /// A `?` in the middle of the chain, followed by more postfix operators,
    /// stays part of the operand instead: `await f()?.g()` is
    /// `Await(Field(Try(Call), g))`, not `Try(Await(...))`, because only a `?`
    /// that ends the whole chain escapes outside the `Await`.
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
                let operand = self.nested(Parser::parse_unary)?;
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
                let operand = self.parse_postfix()?;
                let operand_span = operand.span;
                Ok(match operand.kind {
                    ExprKind::Try(inner) => {
                        let await_span = start.to(inner.span);
                        let awaited = expr(ExprKind::Await(inner), await_span);
                        expr(ExprKind::Try(Box::new(awaited)), start.to(operand_span))
                    }
                    _ => expr(ExprKind::Await(Box::new(operand)), start.to(operand_span)),
                })
            }
        }
    }

    fn parse_postfix(&mut self) -> PResult<Expr> {
        self.chained(|parser| {
            let mut value = parser.parse_primary()?;
            loop {
                // `(`, `<`, and `{` continue the expression only optionally, so a
                // line break before them ends the statement instead. `.` and `?`
                // can never start one, so they always continue.
                let stop = parser.at_statement_break();
                match parser.peek() {
                    TokenKind::Dot => {
                        parser.link()?;
                        parser.bump();
                        let name = parser.expect_member_name()?;
                        let span = value.span.to(name.span);
                        value = expr(
                            ExprKind::Field {
                                base: Box::new(value),
                                name,
                            },
                            span,
                        );
                    }
                    TokenKind::LParen if !stop => {
                        parser.link()?;
                        parser.bump();
                        let args = parser.parse_args()?;
                        value = parser.finish_call(value, Vec::new(), args)?;
                    }
                    TokenKind::Question => {
                        parser.link()?;
                        let span = value.span.to(parser.span());
                        parser.bump();
                        value = expr(ExprKind::Try(Box::new(value)), span);
                    }
                    TokenKind::Lt if !stop => {
                        parser.link()?;
                        match parser.try_generic_call(value)? {
                            Ok(call) => value = call,
                            Err(unchanged) => return Ok(unchanged),
                        }
                    }
                    TokenKind::LBrace
                        if !stop
                            && !parser.no_trailing_closure
                            && can_take_trailing_closure(&value) =>
                    {
                        parser.link()?;
                        let closure = parser.parse_trailing_closure()?;
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
        })
    }

    /// Builds a call, attaching `f(x) { ... }`-style trailing closures.
    fn finish_call(&mut self, callee: Expr, generics: Vec<Type>, args: Vec<Arg>) -> PResult<Expr> {
        let mut span = callee.span.to(self.prev_span());
        let trailing = if !self.no_trailing_closure
            && self.at(&TokenKind::LBrace)
            && !self.at_statement_break()
        {
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

    /// Parses arguments up to and including `)`. Line breaks inside the
    /// parentheses never end a statement, so an argument list may span lines.
    fn parse_args(&mut self) -> PResult<Vec<Arg>> {
        self.grouped(|parser| parser.scoped(false, Parser::parse_arg_list))
    }

    fn parse_arg_list(&mut self) -> PResult<Vec<Arg>> {
        let mut args: Vec<Arg> = Vec::new();
        let mut first_label: Option<Span> = None;

        while !self.at(&TokenKind::RParen) && !self.is_eof() {
            let start = self.span();
            let label = if matches!(self.peek(), TokenKind::Ident(_))
                && self.peek_at(1) == &TokenKind::Colon
            {
                let label = self.expect_ident()?;
                self.bump();
                Some(label)
            } else {
                None
            };

            match (&label, first_label) {
                (Some(label), None) => first_label = Some(label.span),
                (None, Some(previous)) => self.error(
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

            let is_var = self.eat_keyword(Keyword::Var);
            let spread = self.eat(&TokenKind::Ellipsis);
            let value = self.parse_expr()?;
            args.push(Arg {
                label,
                is_var,
                spread,
                value,
                span: start.to(self.prev_span()),
            });

            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }

        self.expect(&TokenKind::RParen, "`)`")?;
        Ok(args)
    }

    /// Parses the elements of an array literal up to and including `]`.
    fn parse_array_elements(&mut self) -> PResult<Vec<Expr>> {
        let mut elements = Vec::new();
        while !self.at(&TokenKind::RBracket) && !self.is_eof() {
            elements.push(self.parse_expr()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::RBracket, "`]`")?;
        Ok(elements)
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
                let inner = self.grouped(|parser| parser.scoped(false, Parser::parse_expr))?;
                self.expect(&TokenKind::RParen, "`)`")?;
                Ok(expr(inner.kind, start.to(self.prev_span())))
            }
            TokenKind::LBracket => {
                self.bump();
                let elements =
                    self.grouped(|parser| parser.scoped(false, Parser::parse_array_elements))?;
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
            TokenKind::Keyword(Keyword::Break) => {
                self.bump();
                let value = if self.can_start_expr() {
                    Some(Box::new(self.parse_expr()?))
                } else {
                    None
                };
                Ok(expr(ExprKind::Break(value), start.to(self.prev_span())))
            }
            TokenKind::Keyword(Keyword::Continue) => {
                self.bump();
                Ok(expr(ExprKind::Continue, start.to(self.prev_span())))
            }
            TokenKind::Keyword(Keyword::Fn | Keyword::Async) => self.parse_lambda(),
            _ => Err(self.expected_expression()),
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
                        | Keyword::Break
                        | Keyword::Continue
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
        self.nested(Parser::parse_pattern_inner)
    }

    fn parse_pattern_inner(&mut self) -> PResult<Pattern> {
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
                    self.grouped(|parser| {
                        while !parser.at(&TokenKind::RParen) && !parser.is_eof() {
                            payload.push(parser.parse_pattern()?);
                            if !parser.eat(&TokenKind::Comma) {
                                break;
                            }
                        }
                        parser.expect(&TokenKind::RParen, "`)`")
                    })?;
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
        // The interpolation gets a parser of its own, but it is nested inside
        // this file and spends the same stack, so it starts from the depth
        // this parser has reached rather than from zero. Otherwise a string
        // interpolating a string interpolating a string would reset the limit
        // at every level and never reach it.
        inner.depth = self.depth;
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
    fn parses_a_test_declaration() {
        let unit = ok("test fn greetsByName() -> Result<Unit, Error> { Ok(()) }");
        let item = &unit.items[0];
        assert!(item.is_test);
        assert!(!item.exported);
        assert_eq!(fn_decl(item).name.node, "greetsByName");
    }

    #[test]
    fn rejects_an_exported_test_written_either_way_round() {
        for source in [
            "export test fn t() -> Result<Unit, Error> { Ok(()) }",
            "test export fn t() -> Result<Unit, Error> { Ok(()) }",
        ] {
            let diagnostics = errors(source);
            assert_eq!(
                codes(&diagnostics),
                ["cove::parse::exported_test"],
                "{source}"
            );
            assert!(diagnostics[0].message.contains("may not be exported"));
            assert!(diagnostics[0]
                .rule
                .as_ref()
                .expect("the diagnostic states its rule")
                .contains("only caller"));
        }
    }

    #[test]
    fn rejects_a_modifier_written_twice() {
        let diagnostics = errors("export export fn f() {}");
        assert_eq!(codes(&diagnostics), ["cove::parse::repeated_modifier"]);
        assert!(diagnostics[0].message.contains("`export` is written twice"));
    }

    #[test]
    fn rejects_test_on_a_declaration_that_is_not_a_function() {
        let diagnostics = errors("test struct Point { x: Int }");
        assert_eq!(codes(&diagnostics), ["cove::parse::test_not_a_function"]);
    }

    #[test]
    fn rejects_a_test_declared_inside_an_impl_block() {
        let diagnostics = errors(
            "impl Point {
  test fn t() -> Result<Unit, Error> { Ok(()) }
}",
        );
        assert_eq!(codes(&diagnostics), ["cove::parse::nested_test"]);
        assert!(diagnostics[0].message.contains("`impl` block"));
    }

    #[test]
    fn rejects_a_test_declared_inside_a_block() {
        let diagnostics = errors(
            "fn main() {
  test fn t() -> Result<Unit, Error> { Ok(()) }
}",
        );
        assert_eq!(codes(&diagnostics), ["cove::parse::nested_test"]);
    }

    #[test]
    fn test_is_a_keyword_only_in_front_of_a_declaration() {
        // `test` after `.` names an ordinary member, as every keyword does.
        let unit = ok("fn main() {
  suite.test()
}");
        assert_eq!(unit.items.len(), 1);
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
    fn parses_a_trait_with_required_and_defaulted_methods() {
        let unit = ok(
            "/// Renders itself.\nexport trait Display {\n  /// The full form.\n  fn describe(self) -> String\n\n  /// A short form.\n  fn label(self) -> String { self.describe() }\n\n  fn make(width: Int) -> Int\n}",
        );
        let item = &unit.items[0];
        assert!(item.exported);
        assert_eq!(item.doc.as_deref(), Some("Renders itself."));
        let ItemKind::Trait(decl) = &item.kind else {
            panic!("expected a trait");
        };
        assert_eq!(decl.name.node, "Display");
        assert_eq!(decl.methods.len(), 3);
        assert_eq!(decl.methods[0].doc.as_deref(), Some("The full form."));
        assert!(decl.methods[0].receiver.is_some());
        assert!(decl.methods[0].default.is_none());
        assert!(decl.methods[1].default.is_some());
        // A method with no `self` is an associated function.
        assert!(decl.methods[2].receiver.is_none());
        assert_eq!(decl.methods[2].params.len(), 1);
    }

    #[test]
    fn parses_a_conformance_and_an_inherent_impl() {
        let unit = ok("impl Display for Booking {\n  fn describe(self) -> String { \"b\" }\n}\n\nimpl Booking {\n  fn id(self) -> Int { 1 }\n}");
        let ItemKind::Impl(conformance) = &unit.items[0].kind else {
            panic!("expected an impl");
        };
        assert_eq!(
            conformance.trait_name.as_ref().map(|n| n.node.as_str()),
            Some("Display")
        );
        assert_eq!(conformance.type_name.node, "Booking");
        let ItemKind::Impl(inherent) = &unit.items[1].kind else {
            panic!("expected an impl");
        };
        assert!(inherent.trait_name.is_none());
        assert_eq!(inherent.type_name.node, "Booking");
    }

    #[test]
    fn parses_one_bound_and_several_bounds_on_a_type_parameter() {
        let unit = ok("fn render<T: Display, U, V: Display + Ordered>(value: T) { }");
        let decl = fn_decl(&unit.items[0]);
        assert_eq!(decl.generics.len(), 3);
        assert_eq!(decl.generics[0].name.node, "T");
        assert_eq!(decl.generics[0].bounds.len(), 1);
        assert_eq!(decl.generics[0].bounds[0].node, "Display");
        assert!(decl.generics[1].bounds.is_empty());
        assert_eq!(decl.generics[2].bounds.len(), 2);
        assert_eq!(decl.generics[2].bounds[1].node, "Ordered");
    }

    #[test]
    fn parses_dyn_as_a_type() {
        let unit = ok("fn renderAll(values: Array<dyn Display>) -> dyn Display { values }");
        let decl = fn_decl(&unit.items[0]);
        let ty = decl.params[0].ty.as_ref().expect("a written type");
        assert_eq!(ty.to_string(), "Array<dyn Display>");
        let TypeKind::Dyn(name) = &decl.return_type.as_ref().expect("a return type").kind else {
            panic!("expected a `dyn` return type");
        };
        assert_eq!(name.node, "Display");
    }

    #[test]
    fn rejects_dyn_without_a_trait_name() {
        assert_eq!(
            codes(&errors("fn go(value: dyn) { }")),
            ["cove::parse::unexpected_token"]
        );
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
    fn await_binds_tighter_than_a_trailing_question_mark() {
        let value = tail_expr("await handler(event)?");
        let ExprKind::Try(inner) = &value.kind else {
            panic!("expected a `?`");
        };
        let ExprKind::Await(call) = &inner.kind else {
            panic!("expected an await");
        };
        assert!(matches!(call.kind, ExprKind::Call { .. }));
    }

    #[test]
    fn await_alone_has_no_try() {
        let value = tail_expr("await handler(event)");
        let ExprKind::Await(call) = &value.kind else {
            panic!("expected an await");
        };
        assert!(matches!(call.kind, ExprKind::Call { .. }));
    }

    #[test]
    fn await_binds_tighter_than_binary_operators() {
        let sum = tail_expr("await a() + b");
        let ExprKind::Binary { op, lhs, .. } = &sum.kind else {
            panic!("expected an addition");
        };
        assert_eq!(*op, BinaryOp::Add);
        assert!(matches!(lhs.kind, ExprKind::Await(_)));
    }

    /// A `?` that is followed by more of the postfix chain stays part of the
    /// chain instead of escaping the `Await`: `await f()?.g()` awaits
    /// `f()?.g()` as a whole, rather than awaiting `f()` and applying `?` to
    /// the result afterwards.
    #[test]
    fn await_with_a_question_mark_mid_chain_keeps_it_inside_the_chain() {
        let value = tail_expr("await f()?.g()");
        let ExprKind::Await(inner) = &value.kind else {
            panic!("expected an await");
        };
        let ExprKind::Call { callee, .. } = &inner.kind else {
            panic!("expected a call to `g`");
        };
        let ExprKind::Field { base, name } = &callee.kind else {
            panic!("expected a field access");
        };
        assert_eq!(name.node, "g");
        let ExprKind::Try(call) = &base.kind else {
            panic!("expected a `?`");
        };
        assert!(matches!(call.kind, ExprKind::Call { .. }));
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
    fn parses_is_at_the_same_precedence_as_comparison() {
        let value = tail_expr("a is b && c == d");
        let ExprKind::Binary {
            op: BinaryOp::And,
            lhs,
            rhs,
        } = &value.kind
        else {
            panic!("expected `&&` at the top, binding `is` and `==` tighter");
        };
        assert!(matches!(
            lhs.kind,
            ExprKind::Binary {
                op: BinaryOp::Is,
                ..
            }
        ));
        assert!(matches!(
            rhs.kind,
            ExprKind::Binary {
                op: BinaryOp::Eq,
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
        assert!(matches!(tail_expr("break").kind, ExprKind::Break(None)));
        assert!(matches!(
            tail_expr("break 1").kind,
            ExprKind::Break(Some(_))
        ));
        assert!(matches!(tail_expr("continue").kind, ExprKind::Continue));
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

    /// Parses `source` as the body of `fn main`, returning its statements
    /// with the tail expression appended, so a test can count the statements
    /// a body was split into.
    fn body_stmts(source: &str) -> Vec<Stmt> {
        let unit = ok(&format!("fn main() {{\n{source}\n}}"));
        let body = fn_decl(&unit.items[0]).body.clone();
        let mut statements = body.statements;
        if let Some(tail) = body.tail {
            statements.push(Stmt {
                span: tail.span,
                kind: StmtKind::Expr(*tail),
            });
        }
        statements
    }

    fn stmt_expr(stmt: &Stmt) -> &Expr {
        match &stmt.kind {
            StmtKind::Expr(value) => value,
            other => panic!("expected an expression statement, found {other:?}"),
        }
    }

    #[test]
    fn a_newline_ends_a_call_statement() {
        let statements = body_stmts("println(\"x\")\n()");
        assert_eq!(statements.len(), 2);
        assert!(matches!(
            stmt_expr(&statements[0]).kind,
            ExprKind::Call { .. }
        ));
        assert!(matches!(stmt_expr(&statements[1]).kind, ExprKind::Unit));
    }

    #[test]
    fn a_newline_ends_a_statement_before_a_block() {
        let statements = body_stmts("let n = compute()\n{ n }");
        assert_eq!(statements.len(), 2);
        let StmtKind::Let { value, .. } = &statements[0].kind else {
            panic!("expected a binding");
        };
        assert!(matches!(value.kind, ExprKind::Call { trailing: None, .. }));
        assert!(matches!(stmt_expr(&statements[1]).kind, ExprKind::Block(_)));
    }

    #[test]
    fn a_newline_ends_a_match_arm_body() {
        let value = tail_expr(
            "match x {\n  1 => \"one\"\n  -1 => \"minus one\"\n  2 => \"two\"\n  _ => \"other\"\n}",
        );
        let ExprKind::Match { arms, .. } = &value.kind else {
            panic!("expected a match");
        };
        assert_eq!(arms.len(), 4);
        let PatternKind::Literal(literal) = &arms[1].pattern.kind else {
            panic!("expected a literal pattern");
        };
        assert!(matches!(
            literal.kind,
            ExprKind::Unary {
                op: UnaryOp::Neg,
                ..
            }
        ));
        for arm in arms {
            assert!(matches!(arm.body.kind, ExprKind::Str(_)));
        }
    }

    /// A line that starts with `.` continues the expression before it, so a
    /// method chain may be split across lines.
    #[test]
    fn a_method_chain_may_be_split_across_lines() {
        let statements = body_stmts("let result = value\n  .map(f)\n  .unwrapOr(0)");
        assert_eq!(statements.len(), 1);
        let StmtKind::Let { value, .. } = &statements[0].kind else {
            panic!("expected a binding");
        };
        let ExprKind::Call { callee, .. } = &value.kind else {
            panic!("expected a call");
        };
        let ExprKind::Field { name, .. } = &callee.kind else {
            panic!("expected a field");
        };
        assert_eq!(name.node, "unwrapOr");
    }

    #[test]
    fn newlines_inside_parentheses_and_brackets_do_not_end_a_statement() {
        let call = tail_expr("request(\n  url: endpoint,\n  timeout: 5s\n)");
        let ExprKind::Call { args, .. } = &call.kind else {
            panic!("expected a call");
        };
        assert_eq!(args.len(), 2);

        let array = tail_expr("[\n  1,\n  2,\n  3\n]");
        let ExprKind::ArrayLit(elements) = &array.kind else {
            panic!("expected an array literal");
        };
        assert_eq!(elements.len(), 3);

        let parenthesised = tail_expr("(\n  1\n  + 2\n)");
        assert!(matches!(parenthesised.kind, ExprKind::Binary { .. }));

        let generic = tail_expr("api.fetch<\n  Array<Booking>\n>(\n  \"/bookings\"\n)");
        let ExprKind::Call { generics, args, .. } = &generic.kind else {
            panic!("expected a call");
        };
        assert_eq!(generics.len(), 1);
        assert_eq!(args.len(), 1);
    }

    #[test]
    fn newlines_inside_declaration_headers_and_field_lists_do_not_end_a_statement() {
        let unit = ok("fn f<\n  T\n>(\n  a: T,\n  b: Int = 1\n) -> Int {\n  b\n}");
        let decl = fn_decl(&unit.items[0]);
        assert_eq!(decl.generics.len(), 1);
        assert_eq!(decl.params.len(), 2);

        let braced = ok("struct S {\n  a: Int\n  b: Map<\n    String,\n    Int\n  >\n}");
        let ItemKind::Struct(decl) = &braced.items[0].kind else {
            panic!("expected a struct");
        };
        assert_eq!(decl.fields.len(), 2);

        let parens = ok("struct P(\n  a: Int,\n  b: Int\n)");
        let ItemKind::Struct(decl) = &parens.items[0].kind else {
            panic!("expected a struct");
        };
        assert_eq!(decl.fields.len(), 2);
    }

    /// A block inside a group is still a block: its statements end at
    /// newlines even though the enclosing `(` suspended the rule.
    #[test]
    fn a_block_inside_a_group_still_ends_statements_at_newlines() {
        let call = tail_expr("spawn(fn() {\n  a()\n  b()\n})");
        let ExprKind::Call { args, .. } = &call.kind else {
            panic!("expected a call");
        };
        let ExprKind::Lambda { body, .. } = &args[0].value.kind else {
            panic!("expected a lambda");
        };
        assert_eq!(body.statements.len(), 1);
        assert!(body.tail.is_some());
    }

    #[test]
    fn else_attaches_across_a_line_break() {
        let plain = tail_expr("if cond {\n}\nelse {\n}");
        let ExprKind::If { else_branch, .. } = &plain.kind else {
            panic!("expected an if");
        };
        assert!(matches!(
            else_branch.as_deref().map(|value| &value.kind),
            Some(ExprKind::Block(_))
        ));

        let chained = tail_expr("if a {\n}\nelse if b {\n}\nelse {\n}");
        let ExprKind::If { else_branch, .. } = &chained.kind else {
            panic!("expected an if");
        };
        assert!(matches!(
            else_branch.as_deref().map(|value| &value.kind),
            Some(ExprKind::If { .. })
        ));
    }

    #[test]
    fn a_match_arm_may_put_its_pattern_body_and_arrow_on_separate_lines() {
        let value = tail_expr("match x {\n  Ok(v)\n  =>\n    v\n  Err(e) => 0\n}");
        let ExprKind::Match { arms, .. } = &value.kind else {
            panic!("expected a match");
        };
        assert_eq!(arms.len(), 2);
        assert!(matches!(arms[0].body.kind, ExprKind::Ident(_)));
    }

    /// A binary operator continues an expression only from the line it ends:
    /// `a +` / `b` is one expression, while `a` / `+ b` is two statements.
    /// The operand, not the operator, decides where the line may end.
    #[test]
    fn a_binary_operator_continues_only_from_the_end_of_a_line() {
        let continued = tail_expr("a +\n  b");
        assert!(matches!(continued.kind, ExprKind::Binary { .. }));

        let split = errors("fn main() {\n  a\n+ b\n}");
        assert_eq!(codes(&split), ["cove::parse::newline_ended_statement"]);
        assert!(split[0].rule.is_some());
        assert!(split[0].help.is_some());

        // Even where an expression was not what was expected, the reason the
        // operator is stranded is explained.
        let header = errors("fn main() {\n  if a\n  && b {\n  }\n}");
        assert!(header[0].rule.is_some());
        assert!(header[0].help.is_some());

        let assignment = body_stmts("total =\n  1");
        assert_eq!(assignment.len(), 1);
        assert!(matches!(
            stmt_expr(&assignment[0]).kind,
            ExprKind::Assign { .. }
        ));
    }

    /// `-` and `!` can begin an expression, so a line starting with one is a
    /// new statement rather than a continuation.
    #[test]
    fn a_leading_minus_starts_a_new_statement() {
        let statements = body_stmts("a\n- b");
        assert_eq!(statements.len(), 2);
        assert!(matches!(
            stmt_expr(&statements[1]).kind,
            ExprKind::Unary {
                op: UnaryOp::Neg,
                ..
            }
        ));
    }

    #[test]
    fn a_detached_trailing_closure_explains_the_newline_rule() {
        let diagnostics = errors("fn main() {\n  tasks.spawn\n  { work() }\n}");
        assert_eq!(
            codes(&diagnostics),
            ["cove::parse::newline_before_trailing_closure"]
        );
        assert!(diagnostics[0].rule.is_some());
        assert!(diagnostics[0].help.is_some());

        // On one line it is still a trailing closure.
        let attached = tail_expr("tasks.spawn { work() }");
        assert!(matches!(
            attached.kind,
            ExprKind::Call {
                trailing: Some(_),
                ..
            }
        ));
    }
    /// A block and the expression that is its body each raise the depth, so
    /// `fn main() { ... }` costs two levels before the parentheses start.
    const OUTER_LEVELS: usize = 2;

    #[test]
    fn nesting_up_to_the_limit_parses() {
        let depth = MAX_NESTING_DEPTH as usize - OUTER_LEVELS;
        ok(&format!(
            "fn main() {{\n{}1{}\n}}",
            "(".repeat(depth),
            ")".repeat(depth)
        ));
    }

    /// Every shape that nests, one level past the limit, in a debug build on
    /// whatever stack the test harness supplies — which is the case the
    /// limit exists for, since an overflow here would abort the test binary
    /// rather than fail this test.
    #[test]
    fn nesting_past_the_limit_is_a_diagnostic() {
        let past = MAX_NESTING_DEPTH as usize + 1;
        let sources = [
            format!(
                "fn main() {{\n{}1{}\n}}",
                "(".repeat(past),
                ")".repeat(past)
            ),
            format!(
                "fn main() {{\n{}1{}\n}}",
                "{".repeat(past),
                "}".repeat(past)
            ),
            format!(
                "fn main() {{\n{}1{}\n}}",
                "[".repeat(past),
                "]".repeat(past)
            ),
            format!("fn main() {{\n{}1\n}}", "-".repeat(past)),
            format!("fn main() {{\n  a = {}1\n}}", "a = ".repeat(past)),
            format!(
                "fn f(a: {}Int{}) {{}}",
                "Array<".repeat(past),
                ">".repeat(past)
            ),
            format!(
                "fn main() {{\n  match x {{\n    {}y{} => 1\n  }}\n}}",
                "Some(".repeat(past),
                ")".repeat(past)
            ),
            // Chains are parsed by a loop rather than by recursion, and are
            // counted because the tree they build is as deep as they are long.
            format!("fn main() {{\n  1{}\n}}", " + 1".repeat(past)),
            format!("fn main() {{\n  a{}\n}}", ".b".repeat(past)),
            format!("fn main() {{\n  a{}\n}}", "?".repeat(past)),
        ];
        for source in sources {
            let diagnostics = errors(&source);
            let first = &diagnostics[0];
            assert_eq!(first.code, "cove::parse::nesting_too_deep");
            assert!(first.message.contains("64"), "{}", first.message);
            assert!(
                first.primary.is_some(),
                "the limit reports where it was passed"
            );
            assert!(
                first.rule.is_some(),
                "the limit states the rule it enforces"
            );
            assert!(first.help.is_some(), "the limit says what to do instead");
        }
    }

    /// A file whose nesting is three orders of magnitude past the limit is
    /// still one diagnostic and still finishes, because the parser recovers
    /// from this error the way it recovers from any other.
    #[test]
    fn nesting_far_past_the_limit_still_reports_and_returns() {
        let past = 100_000;
        let source = format!(
            "fn main() {{\n{}1{}\n}}",
            "(".repeat(past),
            ")".repeat(past)
        );
        let diagnostics = errors(&source);
        assert_eq!(codes(&diagnostics), vec!["cove::parse::nesting_too_deep"]);
    }

    /// The nesting a string literal may contain is scanned before the parser
    /// runs, so the lexer must not recurse over it either.
    #[test]
    fn a_string_of_nothing_but_open_braces_is_a_lexical_error() {
        let source = format!("fn main() {{\n  \"{}\"\n}}", "{".repeat(100_000));
        let diagnostics = errors(&source);
        assert_eq!(
            codes(&diagnostics),
            vec!["cove::lex::unterminated_interpolation"]
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
