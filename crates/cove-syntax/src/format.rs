//! Deterministic formatting of Cove source.
//!
//! The formatter prints an [`ast::SourceUnit`] back to source text. It is
//! deterministic and idempotent: formatting twice produces exactly what
//! formatting once produced, and re-parsing the result produces the same tree.
//!
//! # Layout
//!
//! Two spaces of indentation, no tabs, no trailing whitespace, one trailing
//! newline. Lines are kept within [`MAX_WIDTH`] columns where a legal break
//! exists.
//!
//! # The newline rule
//!
//! Cove statements end at the end of a line, so a formatter cannot break a
//! line wherever it likes: a break in the wrong place silently splits one
//! statement into two. Every break this module introduces is one the parser
//! reads as a continuation — inside a `(`, `[`, or `<` group, immediately
//! after a binary operator, or before a leading `.` — which is why long
//! argument lists break one argument per line, a binary expression breaks
//! *after* its operator, and a method chain breaks *before* its dots.
//!
//! # Comments
//!
//! The tree carries `///` doc comments but not `//` or `/* */` comments, and
//! not the blank lines an author wrote between statements. [`format_source`]
//! reads those back out of the source text and re-attaches them by position:
//! a comment on its own line attaches to the item or statement that follows
//! it, and a comment at the end of a line stays at the end of that line. No
//! comment is ever dropped; one the formatter cannot place exactly is moved
//! to the nearest following boundary rather than lost.

use cove_diag::Span;

use crate::ast::{
    Arg, BinaryOp, Block, EnumCase, EnumDecl, Expr, ExprKind, Field, FnDecl, GenericParam,
    ImplBlock, Item, ItemKind, MatchArm, Param, Pattern, PatternKind, Receiver, SourceUnit, Stmt,
    StmtKind, StrPart, StructDecl, TraitDecl, TraitMethod, Type, TypeAlias, TypeKind, UnaryOp, Use,
};

/// The column the formatter keeps lines within when a legal break exists.
pub const MAX_WIDTH: usize = 80;

/// One indentation step, in spaces.
const INDENT: usize = 2;

/// Formats one parsed source unit deterministically.
///
/// The tree does not carry `//` and `/* */` comments or the blank lines
/// between statements, so this function cannot reproduce them. Use
/// [`format_source`] to format a unit together with the text it was parsed
/// from, which is what `cove fmt` does.
pub fn format_unit(unit: &SourceUnit) -> String {
    format_source("", unit)
}

/// Renders one expression on a single line, from the tree alone.
///
/// A literal's spelling is not in the tree, so `0xff` renders as `255`. That
/// is enough for the places this is used: showing a parameter's default in a
/// signature, where what matters is which value it is.
pub fn format_expr(expr: &Expr) -> String {
    Formatter::new("").flat(expr, 0)
}

/// Formats `unit`, which must be the tree parsed from `source`.
///
/// Reading the source alongside the tree is what lets the formatter keep
/// comments, blank lines, and the exact spelling of numeric and string
/// literals.
pub fn format_source(source: &str, unit: &SourceUnit) -> String {
    let mut formatter = Formatter::new(source);
    formatter.source_unit(unit);
    formatter.finish()
}

/// The display width of `text`, in characters.
fn width(text: &str) -> usize {
    text.chars().count()
}

// ---------------------------------------------------------------------------
// Comments
// ---------------------------------------------------------------------------

/// One `//` or `/* */` comment found in the source text.
///
/// `///` doc comments are not collected: the parser already attached them to
/// the declaration they document, so the tree prints them itself.
#[derive(Clone, Debug)]
struct Comment {
    start: usize,
    end: usize,
    /// True when only whitespace precedes the comment on its line, which is
    /// what makes it belong to the construct that follows rather than to the
    /// line it sits on.
    own_line: bool,
    /// The comment text, with trailing whitespace removed from every line.
    text: String,
    /// The column the comment starts at, used to re-indent the continuation
    /// lines of a multi-line block comment.
    column: usize,
}

/// Every `//` and `/* */` comment in `source`, in source order.
fn scan_comments(source: &str) -> Vec<Comment> {
    let bytes = source.as_bytes();
    let mut comments = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => i = skip_string(bytes, i),
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                let is_doc = bytes.get(i + 2) == Some(&b'/') && bytes.get(i + 3) != Some(&b'/');
                let end = line_end(bytes, i);
                if !is_doc {
                    comments.push(comment_at(source, i, end));
                }
                i = end;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let end = skip_block_comment(bytes, i);
                comments.push(comment_at(source, i, end));
                i = end;
            }
            _ => i += 1,
        }
    }
    comments
}

fn comment_at(source: &str, start: usize, end: usize) -> Comment {
    let before = &source[..start];
    let line = before.rsplit('\n').next().unwrap_or("");
    let text = source[start..end]
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    Comment {
        start,
        end,
        own_line: line.trim().is_empty(),
        text,
        column: width(line),
    }
}

fn line_end(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() && bytes[i] != b'\n' {
        i += 1;
    }
    i
}

/// Skips a string literal, including the interpolations that may nest further
/// strings inside it, exactly as the lexer does.
fn skip_string(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            b'{' => i = skip_interpolation(bytes, i + 1),
            _ => i += 1,
        }
    }
    i
}

fn skip_interpolation(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len() {
        match bytes[i] {
            b'}' => return i + 1,
            b'{' => i = skip_interpolation(bytes, i + 1),
            b'"' => i = skip_string(bytes, i),
            _ => i += 1,
        }
    }
    i
}

/// Skips a `/* ... */` comment. Block comments nest.
fn skip_block_comment(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 2;
    let mut depth = 1u32;
    while i < bytes.len() {
        if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
            i += 2;
            depth -= 1;
            if depth == 0 {
                return i;
            }
        } else if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            depth += 1;
        } else {
            i += 1;
        }
    }
    i
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// One output line: its code, and the comment that trails it, if any.
#[derive(Default)]
struct Line {
    text: String,
    comment: Option<String>,
}

/// The line buffer the formatter writes into.
///
/// Trailing comments are held beside their line rather than appended to it,
/// so that a run of consecutive lines that all end in a comment can be
/// aligned once the whole run is known.
#[derive(Default)]
struct Out {
    lines: Vec<Line>,
    current: String,
    comment: Option<String>,
    open: bool,
    pending_blank: bool,
}

impl Out {
    /// Ends the current line and begins a new one indented by `indent`.
    ///
    /// Calling this twice without writing anything in between re-indents the
    /// line instead of emitting an empty one, so callers may start a line
    /// without knowing whether their caller already did.
    fn start_line(&mut self, indent: usize) {
        let empty = self.current.trim().is_empty() && self.comment.is_none();
        if self.open && !empty {
            self.lines.push(Line {
                text: std::mem::take(&mut self.current),
                comment: self.comment.take(),
            });
        }
        if self.pending_blank && !self.lines.is_empty() {
            self.lines.push(Line::default());
        }
        self.pending_blank = false;
        self.current = " ".repeat(indent);
        self.open = true;
    }

    fn write(&mut self, text: &str) {
        self.current.push_str(text);
    }

    /// The column the next character would be written at.
    fn col(&self) -> usize {
        width(&self.current)
    }

    /// Attaches `text` to the end of the current line.
    ///
    /// A comment with nothing before it on the line is written as ordinary
    /// text instead, so that it never turns into a line of padding.
    fn set_comment(&mut self, text: &str) {
        if self.current.trim().is_empty() && self.comment.is_none() {
            self.write(text);
            return;
        }
        match &mut self.comment {
            Some(existing) => {
                existing.push(' ');
                existing.push_str(text);
            }
            None => self.comment = Some(text.to_string()),
        }
    }

    /// Renders the buffered lines, aligning each run of consecutive trailing
    /// comments and ending the file with exactly one newline.
    fn finish(mut self) -> String {
        if self.open {
            self.lines.push(Line {
                text: self.current,
                comment: self.comment,
            });
        }
        while self
            .lines
            .last()
            .is_some_and(|line| line.text.trim().is_empty() && line.comment.is_none())
        {
            self.lines.pop();
        }

        let mut out = String::new();
        let mut i = 0;
        while i < self.lines.len() {
            if self.lines[i].comment.is_none() {
                out.push_str(self.lines[i].text.trim_end());
                out.push('\n');
                i += 1;
                continue;
            }
            let mut end = i;
            let mut column = 0;
            while end < self.lines.len() && self.lines[end].comment.is_some() {
                column = column.max(width(self.lines[end].text.trim_end()));
                end += 1;
            }
            for line in &self.lines[i..end] {
                let text = line.text.trim_end();
                out.push_str(text);
                out.push_str(&" ".repeat(column - width(text) + 1));
                out.push_str(line.comment.as_deref().unwrap_or(""));
                out.push('\n');
            }
            i = end;
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Precedence
// ---------------------------------------------------------------------------

/// Precedence levels, lowest first, matching the parser's descent.
mod prec {
    pub const RETURN: u8 = 0;
    pub const ASSIGN: u8 = 1;
    pub const OR: u8 = 2;
    pub const AND: u8 = 3;
    pub const COMPARISON: u8 = 4;
    pub const RANGE: u8 = 5;
    pub const ADDITIVE: u8 = 6;
    pub const MULTIPLICATIVE: u8 = 7;
    pub const UNARY: u8 = 8;
    pub const POSTFIX: u8 = 9;
    pub const PRIMARY: u8 = 10;
}

fn binary_prec(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Or => prec::OR,
        BinaryOp::And => prec::AND,
        BinaryOp::Eq
        | BinaryOp::Ne
        | BinaryOp::Lt
        | BinaryOp::Le
        | BinaryOp::Gt
        | BinaryOp::Ge
        | BinaryOp::Is => prec::COMPARISON,
        BinaryOp::Add | BinaryOp::Sub => prec::ADDITIVE,
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => prec::MULTIPLICATIVE,
    }
}

fn expr_prec(expr: &Expr) -> u8 {
    match &expr.kind {
        ExprKind::Return(_) | ExprKind::Break(_) => prec::RETURN,
        ExprKind::Assign { .. } => prec::ASSIGN,
        ExprKind::Binary { op, .. } => binary_prec(*op),
        ExprKind::Range { .. } => prec::RANGE,
        ExprKind::Unary { .. } | ExprKind::Await(_) => prec::UNARY,
        ExprKind::Field { .. } | ExprKind::Call { .. } | ExprKind::Try(_) => prec::POSTFIX,
        _ => prec::PRIMARY,
    }
}

fn binary_symbol(op: BinaryOp) -> &'static str {
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
        BinaryOp::Is => "is",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
    }
}

fn unary_symbol(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Not => "!",
        UnaryOp::Neg => "-",
    }
}

/// Whether the rendering of `expr` ends in a `}`.
///
/// The header of `if`, `while`, `for`, and `match` is parsed with trailing
/// closures disabled, so a header that would end in a brace must be
/// parenthesised or the body's `{` would be read as part of the header.
fn ends_with_brace(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Block(_)
        | ExprKind::If { .. }
        | ExprKind::Match { .. }
        | ExprKind::For { .. }
        | ExprKind::While { .. }
        | ExprKind::Scope { .. }
        | ExprKind::Lambda { .. } => true,
        ExprKind::Call { trailing, .. } => trailing.is_some(),
        ExprKind::Binary { rhs, .. } => ends_with_brace(rhs),
        ExprKind::Assign { value, .. } => ends_with_brace(value),
        ExprKind::Range { end, .. } => ends_with_brace(end),
        ExprKind::Unary { operand, .. } => ends_with_brace(operand),
        ExprKind::Await(operand) => ends_with_brace(operand),
        ExprKind::Return(Some(value)) | ExprKind::Break(Some(value)) => ends_with_brace(value),
        _ => false,
    }
}

/// The offset of the `}`, `]`, or `)` that a construct ending at `end`
/// closes with.
fn close_brace(end: u32) -> usize {
    (end as usize).saturating_sub(1)
}

fn block_is_empty(block: &Block) -> bool {
    block.statements.is_empty() && block.tail.is_none()
}

/// The body of a trailing closure, which the parser stores as a parameterless
/// lambda.
fn trailing_body(expr: &Expr) -> Option<&Block> {
    match &expr.kind {
        ExprKind::Lambda { body, .. } => Some(body),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The formatter
// ---------------------------------------------------------------------------

struct Formatter<'a> {
    source: &'a str,
    comments: Vec<Comment>,
    /// The first comment that has not been emitted yet.
    next: usize,
    /// How far into the source everything emitted so far reaches, used to
    /// find the blank lines and comments that come next.
    pos: usize,
    out: Out,
}

impl<'a> Formatter<'a> {
    fn new(source: &'a str) -> Self {
        Formatter {
            source,
            comments: scan_comments(source),
            next: 0,
            pos: 0,
            out: Out::default(),
        }
    }

    fn finish(self) -> String {
        self.out.finish()
    }

    // -- source helpers ----------------------------------------------------

    fn text(&self, span: Span) -> Option<&'a str> {
        self.source.get(span.start as usize..span.end as usize)
    }

    /// The source spelling of a numeric literal, so that `0xff`, `1_000`, and
    /// `60s` survive formatting.
    fn number_text(&self, span: Span) -> Option<&'a str> {
        let text = self.text(span)?;
        let first = text.chars().next()?;
        if !first.is_ascii_digit() || text.chars().any(char::is_whitespace) {
            return None;
        }
        Some(text)
    }

    /// The source spelling of a string literal, escapes and interpolations
    /// included. A string is reproduced rather than rebuilt because its
    /// interpolations cannot be broken across lines.
    fn string_text(&self, span: Span) -> Option<&'a str> {
        let text = self.text(span)?;
        if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
            Some(text)
        } else {
            None
        }
    }

    /// Whether a blank line separates `from` from `to` in the source.
    fn blank_line_between(&self, from: usize, to: usize) -> bool {
        if from >= to {
            return false;
        }
        let Some(text) = self.source.get(from..to) else {
            return false;
        };
        let lines: Vec<&str> = text.split('\n').collect();
        lines.len() >= 3
            && lines[1..lines.len() - 1]
                .iter()
                .any(|l| l.trim().is_empty())
    }

    /// Whether an unemitted comment falls between `from` and `to`.
    fn comment_between(&self, from: u32, to: u32) -> bool {
        self.comments[self.next..]
            .iter()
            .any(|c| c.start >= from as usize && c.start < to as usize)
    }

    /// Whether an unemitted comment falls inside `span`.
    ///
    /// Such a comment can only be kept where the author wrote it if the
    /// construct is laid out across lines, so this forces the construct to
    /// break.
    fn holds_comment(&self, span: Span) -> bool {
        self.comment_between(span.start, span.end)
    }

    fn advance(&mut self, to: u32) {
        self.pos = self.pos.max(to as usize);
    }

    // -- comment placement -------------------------------------------------

    /// Emits every comment that precedes `start`, plus the blank lines around
    /// them, at `indent`.
    ///
    /// `allow_blank` is false at the start of a file or a block, where a
    /// leading blank line is never kept.
    fn lead(&mut self, start: usize, indent: usize, allow_blank: bool) {
        let mut allow = allow_blank;
        while self.next < self.comments.len() && self.comments[self.next].start < start {
            let comment = self.comments[self.next].clone();
            if allow && self.blank_line_between(self.pos, comment.start) {
                self.out.pending_blank = true;
            }
            allow = true;
            self.out.start_line(indent);
            self.write_comment(&comment, indent);
            self.pos = self.pos.max(comment.end);
            self.next += 1;
        }
        if allow && self.blank_line_between(self.pos, start) {
            self.out.pending_blank = true;
        }
    }

    /// Emits the comments that precede a closing `}` at `start`, without the
    /// blank line that would otherwise separate them from it.
    fn lead_close(&mut self, start: usize, indent: usize) {
        self.lead(start, indent, true);
        self.out.pending_blank = false;
    }

    /// Attaches the comment that follows `end` on the same source line to the
    /// line just written.
    fn trail(&mut self, end: u32) {
        let end = end as usize;
        while self.next < self.comments.len() {
            let comment = &self.comments[self.next];
            if comment.own_line || comment.start < end || comment.text.contains('\n') {
                return;
            }
            match self.source.get(end..comment.start) {
                Some(between) if !between.contains('\n') => {}
                _ => return,
            }
            let text = comment.text.clone();
            let comment_end = comment.end;
            self.out.set_comment(&text);
            self.pos = self.pos.max(comment_end);
            self.next += 1;
        }
    }

    /// Writes a comment, re-indenting the continuation lines of a multi-line
    /// block comment by the same amount as its first line moved.
    fn write_comment(&mut self, comment: &Comment, indent: usize) {
        let mut lines = comment.text.split('\n');
        if let Some(first) = lines.next() {
            self.out.write(first);
        }
        let prefix = " ".repeat(comment.column);
        for line in lines {
            self.out.start_line(indent);
            let rest = line
                .strip_prefix(prefix.as_str())
                .unwrap_or(line.trim_start());
            self.out.write(rest);
        }
    }

    // -- top level ---------------------------------------------------------

    fn source_unit(&mut self, unit: &SourceUnit) {
        let first_item = unit
            .items
            .first()
            .map(|item| item.span.start as usize)
            .unwrap_or(usize::MAX);

        for use_decl in &unit.uses {
            let limit = (use_decl.span.start as usize).min(first_item);
            self.lead(limit, 0, false);
            self.out.start_line(0);
            self.use_decl(use_decl);
            self.advance(use_decl.span.end);
            self.trail(use_decl.span.end);
        }

        for (i, item) in unit.items.iter().enumerate() {
            if i > 0 || !unit.uses.is_empty() {
                self.out.pending_blank = true;
            }
            self.lead(item.span.start as usize, 0, true);
            self.item(item, 0);
            self.advance(item.span.end);
            self.trail(item.span.end);
        }

        self.lead(self.source.len(), 0, true);
    }

    fn use_decl(&mut self, use_decl: &Use) {
        self.out.write("use ");
        let path: Vec<&str> = use_decl.path.iter().map(|s| s.node.as_str()).collect();
        self.out.write(&path.join("."));
    }

    fn item(&mut self, item: &Item, indent: usize) {
        if let Some(doc) = &item.doc {
            self.doc_comment(doc, indent);
        }
        self.out.start_line(indent);
        if item.exported {
            self.out.write("export ");
        }
        match &item.kind {
            ItemKind::Fn(decl) => self.fn_decl(decl, indent),
            ItemKind::Struct(decl) => self.struct_decl(decl, indent),
            ItemKind::Enum(decl) => self.enum_decl(decl, indent),
            ItemKind::Trait(decl) => self.trait_decl(decl, indent),
            ItemKind::Impl(block) => self.impl_block(block, indent),
            ItemKind::TypeAlias(alias) => self.type_alias(alias, indent),
        }
    }

    /// Writes a `///` comment directly above its declaration, with no blank
    /// line in between.
    fn doc_comment(&mut self, doc: &str, indent: usize) {
        for line in doc.split('\n') {
            self.out.start_line(indent);
            if line.is_empty() {
                self.out.write("///");
            } else {
                self.out.write("/// ");
                self.out.write(line);
            }
        }
        self.out.pending_blank = false;
    }

    /// `<T, U: Display + Ordered>`, or nothing.
    fn generics(&mut self, generics: &[GenericParam]) {
        if generics.is_empty() {
            return;
        }
        let names: Vec<String> = generics.iter().map(GenericParam::to_string).collect();
        self.out.write("<");
        self.out.write(&names.join(", "));
        self.out.write(">");
    }

    // -- declarations ------------------------------------------------------

    fn fn_decl(&mut self, decl: &FnDecl, indent: usize) {
        if decl.is_async {
            self.out.write("async ");
        }
        self.out.write("fn ");
        self.out.write(&decl.name.node);
        self.generics(&decl.generics);
        self.param_list(
            decl.receiver,
            &decl.params,
            indent,
            decl.return_type.as_ref(),
        );
        if let Some(return_type) = &decl.return_type {
            self.out.write(" -> ");
            self.type_ref(return_type, indent);
        }
        self.out.write(" ");
        self.block(&decl.body, indent);
    }

    /// Writes `(...)`, breaking one parameter per line when the whole
    /// signature does not fit.
    fn param_list(
        &mut self,
        receiver: Option<Receiver>,
        params: &[Param],
        indent: usize,
        return_type: Option<&Type>,
    ) {
        let mut entries: Vec<String> = Vec::new();
        if let Some(receiver) = receiver {
            entries.push(if receiver.is_var {
                "var self".into()
            } else {
                "self".into()
            });
        }
        entries.extend(params.iter().map(|p| self.param_flat(p)));

        let flat = format!("({})", entries.join(", "));
        let tail = match return_type {
            Some(ty) => width(&ty.to_string()) + 4,
            None => 0,
        };
        // Two more columns for the ` {` that opens the body.
        if self.out.col() + width(&flat) + tail + 2 <= MAX_WIDTH {
            self.out.write(&flat);
            return;
        }
        self.out.write("(");
        for entry in &entries {
            self.out.start_line(indent + INDENT);
            self.out.write(entry);
            self.out.write(",");
        }
        self.out.start_line(indent);
        self.out.write(")");
    }

    /// `[var ]name[: Type][...][ = default]`, the form a parameter is
    /// declared in.
    fn param_flat(&self, param: &Param) -> String {
        let mut out = String::new();
        if param.is_var {
            out.push_str("var ");
        }
        if !param.name.node.is_empty() {
            out.push_str(&param.name.node);
            if param.ty.is_some() {
                out.push_str(": ");
            }
        }
        if let Some(ty) = &param.ty {
            out.push_str(&ty.to_string());
        }
        if param.variadic {
            out.push_str("...");
        }
        if let Some(default) = &param.default {
            out.push_str(" = ");
            out.push_str(&self.flat(default, prec::RETURN));
        }
        out
    }

    fn struct_decl(&mut self, decl: &StructDecl, indent: usize) {
        self.out.write("struct ");
        self.out.write(&decl.name.node);
        self.generics(&decl.generics);
        self.out.write(" ");
        if decl.fields.is_empty() && !self.holds_comment(decl.span) {
            self.out.write("{ }");
            return;
        }
        self.out.write("{");
        let inner = indent + INDENT;
        for (i, field) in decl.fields.iter().enumerate() {
            self.lead(field.span.start as usize, inner, i > 0);
            self.field(field, inner);
            self.advance(field.span.end);
            self.trail(field.span.end);
        }
        self.lead_close(close_brace(decl.span.end), inner);
        self.out.start_line(indent);
        self.out.write("}");
    }

    fn field(&mut self, field: &Field, indent: usize) {
        if let Some(doc) = &field.doc {
            self.doc_comment(doc, indent);
        }
        self.out.start_line(indent);
        self.out.write(&field.name.node);
        self.out.write(": ");
        self.type_ref(&field.ty, indent);
    }

    fn enum_decl(&mut self, decl: &EnumDecl, indent: usize) {
        self.out.write("enum ");
        self.out.write(&decl.name.node);
        self.generics(&decl.generics);
        self.out.write(" ");
        if decl.cases.is_empty() && !self.holds_comment(decl.span) {
            self.out.write("{ }");
            return;
        }
        self.out.write("{");
        let inner = indent + INDENT;
        for (i, case) in decl.cases.iter().enumerate() {
            self.lead(case.span.start as usize, inner, i > 0);
            self.enum_case(case, inner);
            self.advance(case.span.end);
            self.trail(case.span.end);
        }
        self.lead_close(close_brace(decl.span.end), inner);
        self.out.start_line(indent);
        self.out.write("}");
    }

    fn enum_case(&mut self, case: &EnumCase, indent: usize) {
        if let Some(doc) = &case.doc {
            self.doc_comment(doc, indent);
        }
        self.out.start_line(indent);
        self.out.write(&case.name.node);
        if !case.payload.is_empty() {
            let types: Vec<String> = case.payload.iter().map(Type::to_string).collect();
            self.out.write("(");
            self.out.write(&types.join(", "));
            self.out.write(")");
        }
    }

    /// `trait Name { ... }`, one method per line.
    fn trait_decl(&mut self, decl: &TraitDecl, indent: usize) {
        self.out.write("trait ");
        self.out.write(&decl.name.node);
        self.out.write(" ");
        if decl.methods.is_empty() && !self.holds_comment(decl.span) {
            self.out.write("{ }");
            return;
        }
        self.out.write("{");
        let inner = indent + INDENT;
        for (i, method) in decl.methods.iter().enumerate() {
            if i > 0 {
                self.out.pending_blank = true;
            }
            self.lead(method.span.start as usize, inner, i > 0);
            self.trait_method(method, inner);
            self.advance(method.span.end);
            self.trail(method.span.end);
        }
        self.lead_close(close_brace(decl.span.end), inner);
        self.out.start_line(indent);
        self.out.write("}");
    }

    /// One trait method: a signature, plus a default body when it has one.
    fn trait_method(&mut self, method: &TraitMethod, indent: usize) {
        if let Some(doc) = &method.doc {
            self.doc_comment(doc, indent);
        }
        self.out.start_line(indent);
        if method.is_async {
            self.out.write("async ");
        }
        self.out.write("fn ");
        self.out.write(&method.name.node);
        self.param_list(
            method.receiver,
            &method.params,
            indent,
            method.return_type.as_ref(),
        );
        if let Some(return_type) = &method.return_type {
            self.out.write(" -> ");
            self.type_ref(return_type, indent);
        }
        if let Some(default) = &method.default {
            self.out.write(" ");
            self.block(default, indent);
        }
    }

    fn impl_block(&mut self, block: &ImplBlock, indent: usize) {
        self.out.write("impl ");
        if let Some(trait_name) = &block.trait_name {
            self.out.write(&trait_name.node);
            self.out.write(" for ");
        }
        self.out.write(&block.type_name.node);
        self.generics(&block.generics);
        self.out.write(" ");
        if block.items.is_empty() && !self.holds_comment(block.span) {
            self.out.write("{ }");
            return;
        }
        self.out.write("{");
        let inner = indent + INDENT;
        for (i, item) in block.items.iter().enumerate() {
            if i > 0 {
                self.out.pending_blank = true;
            }
            self.lead(item.span.start as usize, inner, i > 0);
            self.item(item, inner);
            self.advance(item.span.end);
            self.trail(item.span.end);
        }
        self.lead_close(close_brace(block.span.end), inner);
        self.out.start_line(indent);
        self.out.write("}");
    }

    fn type_alias(&mut self, alias: &TypeAlias, indent: usize) {
        self.out.write("type ");
        self.out.write(&alias.name.node);
        self.generics(&alias.generics);
        self.out.write(" = ");
        self.type_ref(&alias.ty, indent);
    }

    // -- types -------------------------------------------------------------

    /// Writes a type, breaking a function type's parameters one per line when
    /// the whole type does not fit.
    fn type_ref(&mut self, ty: &Type, indent: usize) {
        let flat = ty.to_string();
        if self.out.col() + width(&flat) <= MAX_WIDTH {
            self.out.write(&flat);
            return;
        }
        let TypeKind::Fn {
            is_async,
            params,
            return_type,
        } = &ty.kind
        else {
            self.out.write(&flat);
            return;
        };
        if *is_async {
            self.out.write("async ");
        }
        self.out.write("fn(");
        for param in params {
            self.out.start_line(indent + INDENT);
            self.out.write(&param.to_string());
            self.out.write(",");
        }
        self.out.start_line(indent);
        self.out.write(")");
        if let Some(return_type) = return_type {
            self.out.write(" -> ");
            self.type_ref(return_type, indent);
        }
    }

    // -- blocks and statements ---------------------------------------------

    /// Writes `{ ... }` starting at the current column.
    fn block(&mut self, block: &Block, indent: usize) {
        if block_is_empty(block) && !self.holds_comment(block.span) {
            self.out.write("{ }");
            self.advance(block.span.end);
            return;
        }
        self.out.write("{");
        self.advance(block.span.start + 1);
        let inner = indent + INDENT;
        let mut first = true;
        for stmt in &block.statements {
            self.lead(stmt.span.start as usize, inner, !first);
            self.stmt(stmt, inner);
            self.advance(stmt.span.end);
            self.trail(stmt.span.end);
            first = false;
        }
        if let Some(tail) = &block.tail {
            self.lead(tail.span.start as usize, inner, !first);
            self.out.start_line(inner);
            self.expr(tail, prec::RETURN, inner);
            self.advance(tail.span.end);
            self.trail(tail.span.end);
        }
        self.lead_close(close_brace(block.span.end), inner);
        self.out.start_line(indent);
        self.out.write("}");
        self.advance(block.span.end);
    }

    fn stmt(&mut self, stmt: &Stmt, indent: usize) {
        match &stmt.kind {
            StmtKind::Item(item) => self.item(item, indent),
            StmtKind::Let {
                is_var,
                name,
                ty,
                value,
            } => {
                self.out.start_line(indent);
                self.out.write(if *is_var { "var " } else { "let " });
                self.out.write(&name.node);
                if let Some(ty) = ty {
                    self.out.write(": ");
                    self.type_ref(ty, indent);
                }
                self.out.write(" = ");
                self.expr(value, prec::RETURN, indent);
            }
            StmtKind::Expr(value) => {
                self.out.start_line(indent);
                self.expr(value, prec::RETURN, indent);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Postfix chains
// ---------------------------------------------------------------------------

/// One postfix operator applied to the base of a chain.
enum Post<'e> {
    Field(&'e str),
    Call {
        generics: &'e [Type],
        args: &'e [Arg],
        trailing: Option<&'e Expr>,
    },
    Try,
}

/// Splits `a.b(x).c(y)?` into its base and the postfix operators applied to
/// it, so that a chain too long for one line can break before its dots.
fn flatten_postfix(expr: &Expr) -> (&Expr, Vec<Post<'_>>) {
    match &expr.kind {
        ExprKind::Field { base, name } => {
            let (base, mut ops) = flatten_postfix(base);
            ops.push(Post::Field(name.node.as_str()));
            (base, ops)
        }
        ExprKind::Call {
            callee,
            generics,
            args,
            trailing,
        } => {
            let (base, mut ops) = flatten_postfix(callee);
            ops.push(Post::Call {
                generics,
                args,
                trailing: trailing.as_deref(),
            });
            (base, ops)
        }
        ExprKind::Try(inner) => {
            let (base, mut ops) = flatten_postfix(inner);
            ops.push(Post::Try);
            (base, ops)
        }
        _ => (expr, Vec::new()),
    }
}

/// The indices of the dots a method chain may break before: every field
/// access that follows a call.
fn chain_break_points(ops: &[Post<'_>]) -> Vec<usize> {
    let mut points = Vec::new();
    let mut seen_call = false;
    for (i, op) in ops.iter().enumerate() {
        match op {
            Post::Call { .. } => seen_call = true,
            Post::Field(_) if seen_call => points.push(i),
            _ => {}
        }
    }
    points
}

/// Whether hugging the last argument of a call is worth doing.
///
/// A closure always is: the call site reads as one call with a body. Anything
/// else only is when it is the whole argument list, so that a call such as
/// `push(Route(...))` expands its initializer in place while a call with
/// several arguments still breaks one argument per line.
fn hug_applies(closure: bool, last: &Arg, earlier: &[Arg]) -> bool {
    closure || (earlier.is_empty() && last.label.is_none() && !last.is_var && !last.spread)
}

fn call_count(ops: &[Post<'_>]) -> usize {
    ops.iter()
        .filter(|op| matches!(op, Post::Call { .. }))
        .count()
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

impl Formatter<'_> {
    /// Whether `expr` must be laid out across several lines.
    ///
    /// This is a layout policy, not a limit: a declaration body, a control
    /// flow body, and a lambda body always read better broken, and an
    /// expression holding a comment can only keep it where it was written if
    /// it breaks.
    fn breaks(&self, expr: &Expr) -> bool {
        if self.holds_comment(expr.span) {
            return true;
        }
        match &expr.kind {
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Bool(_)
            | ExprKind::Duration(_)
            | ExprKind::Str(_)
            | ExprKind::Unit
            | ExprKind::Ident(_) => false,
            ExprKind::ArrayLit(elements) => elements.iter().any(|e| self.breaks(e)),
            ExprKind::Field { base, .. } => self.breaks(base),
            ExprKind::Call {
                callee,
                args,
                trailing,
                ..
            } => {
                self.breaks(callee)
                    || args.iter().any(|arg| self.breaks(&arg.value))
                    || trailing.as_deref().is_some_and(|t| self.trailing_breaks(t))
            }
            ExprKind::Unary { operand, .. } => self.breaks(operand),
            ExprKind::Binary { lhs, rhs, .. } => self.breaks(lhs) || self.breaks(rhs),
            ExprKind::Assign { target, value, .. } => self.breaks(target) || self.breaks(value),
            ExprKind::Try(inner) | ExprKind::Await(inner) => self.breaks(inner),
            ExprKind::Block(block) => !block_is_empty(block),
            ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::For { .. }
            | ExprKind::While { .. }
            | ExprKind::Scope { .. } => true,
            ExprKind::Return(value) | ExprKind::Break(value) => {
                value.as_deref().is_some_and(|v| self.breaks(v))
            }
            ExprKind::Continue => false,
            ExprKind::Lambda { body, .. } => !block_is_empty(body),
            ExprKind::Range { start, end, .. } => self.breaks(start) || self.breaks(end),
        }
    }

    /// A trailing closure stays on one line when its body is a single
    /// expression that fits, so `tasks.spawn { fetch() }` reads as the one
    /// call it is.
    fn trailing_breaks(&self, closure: &Expr) -> bool {
        let Some(body) = trailing_body(closure) else {
            return self.breaks(closure);
        };
        if self.holds_comment(body.span) || !body.statements.is_empty() {
            return true;
        }
        body.tail.as_deref().is_some_and(|tail| self.breaks(tail))
    }

    /// Writes `expr`, parenthesised when its precedence is below `min`, and
    /// broken across lines when it must be or does not fit.
    fn expr(&mut self, expr: &Expr, min: u8, indent: usize) {
        if expr_prec(expr) < min {
            self.out.write("(");
            self.expr(expr, prec::RETURN, indent);
            self.out.write(")");
            return;
        }
        if !self.breaks(expr) {
            let flat = self.flat_inner(expr);
            if self.out.col() + width(&flat) <= MAX_WIDTH {
                self.out.write(&flat);
                return;
            }
        }
        self.expr_broken(expr, indent);
    }

    /// Writes a header expression — the condition of `if` or `while`, the
    /// iterable of `for`, the scrutinee of `match` — parenthesising it when
    /// it would otherwise end in a `{` the parser would read as the body.
    fn header(&mut self, expr: &Expr, indent: usize) {
        if ends_with_brace(expr) {
            self.out.write("(");
            self.expr(expr, prec::RETURN, indent);
            self.out.write(")");
        } else {
            self.expr(expr, prec::RETURN, indent);
        }
    }

    fn expr_broken(&mut self, expr: &Expr, indent: usize) {
        match &expr.kind {
            ExprKind::Call { .. } => self.call(expr, indent),
            ExprKind::ArrayLit(elements) => self.array_literal(elements, expr.span, indent),
            ExprKind::Field { base, name } => {
                self.expr(base, prec::POSTFIX, indent);
                self.out.write(".");
                self.out.write(&name.node);
            }
            ExprKind::Unary { op, operand } => {
                self.out.write(unary_symbol(*op));
                self.expr(operand, prec::UNARY, indent);
            }
            ExprKind::Binary { op, lhs, rhs } => self.binary(*op, lhs, rhs, indent),
            ExprKind::Assign { op, target, value } => {
                self.expr(target, prec::POSTFIX, indent);
                match op {
                    Some(op) => {
                        self.out.write(" ");
                        self.out.write(binary_symbol(*op));
                        self.out.write("= ");
                    }
                    None => self.out.write(" = "),
                }
                self.expr(value, prec::ASSIGN, indent);
            }
            // `await x?` is how the parser builds `Try(Await(x))`, so it is
            // also how the formatter writes it back.
            ExprKind::Try(inner) => match &inner.kind {
                ExprKind::Await(awaited) => {
                    self.out.write("await ");
                    self.expr(awaited, prec::POSTFIX, indent);
                    self.out.write("?");
                }
                _ => {
                    self.expr(inner, prec::POSTFIX, indent);
                    self.out.write("?");
                }
            },
            ExprKind::Await(inner) => {
                self.out.write("await ");
                self.expr(inner, prec::POSTFIX, indent);
            }
            ExprKind::Block(block) => self.block(block, indent),
            ExprKind::If { .. } => self.if_expr(expr, indent),
            ExprKind::Match { scrutinee, arms } => {
                self.match_expr(scrutinee, arms, expr.span, indent)
            }
            ExprKind::For {
                binding,
                iterable,
                body,
            } => {
                self.out.write("for ");
                self.out.write(&binding.node);
                self.out.write(" in ");
                self.header(iterable, indent);
                self.out.write(" ");
                self.block(body, indent);
            }
            ExprKind::While { condition, body } => {
                self.out.write("while ");
                self.header(condition, indent);
                self.out.write(" ");
                self.block(body, indent);
            }
            ExprKind::Scope { name, body } => {
                self.out.write("scope ");
                self.out.write(&name.node);
                self.out.write(" ");
                self.block(body, indent);
            }
            ExprKind::Return(value) => {
                self.out.write("return");
                if let Some(value) = value {
                    self.out.write(" ");
                    self.expr(value, prec::RETURN, indent);
                }
            }
            ExprKind::Break(value) => {
                self.out.write("break");
                if let Some(value) = value {
                    self.out.write(" ");
                    self.expr(value, prec::RETURN, indent);
                }
            }
            ExprKind::Continue => self.out.write("continue"),
            ExprKind::Lambda {
                is_async,
                params,
                body,
            } => self.lambda(*is_async, params, body, indent),
            ExprKind::Range {
                start,
                end,
                inclusive_end,
            } => {
                self.expr(start, prec::ADDITIVE, indent);
                self.out.write(if *inclusive_end { ".." } else { "..<" });
                self.expr(end, prec::ADDITIVE, indent);
            }
            _ => {
                let flat = self.flat_inner(expr);
                self.out.write(&flat);
            }
        }
    }

    /// Writes a binary expression, breaking *after* the operator when the
    /// right-hand side does not fit: an operator that ends a line continues
    /// the expression onto the next one, while an operator that starts a line
    /// is an error.
    fn binary(&mut self, op: BinaryOp, lhs: &Expr, rhs: &Expr, indent: usize) {
        let level = binary_prec(op);
        self.expr(lhs, level, indent);
        self.out.write(" ");
        self.out.write(binary_symbol(op));
        if self.breaks(rhs) {
            self.out.write(" ");
            self.expr(rhs, level + 1, indent);
            return;
        }
        let flat = self.flat(rhs, level + 1);
        if self.out.col() + 1 + width(&flat) <= MAX_WIDTH {
            self.out.write(" ");
            self.out.write(&flat);
        } else {
            self.out.start_line(indent + INDENT);
            self.expr(rhs, level + 1, indent + INDENT);
        }
    }

    fn if_expr(&mut self, expr: &Expr, indent: usize) {
        let ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } = &expr.kind
        else {
            return;
        };
        self.out.write("if ");
        self.header(condition, indent);
        self.out.write(" ");
        self.block(then_branch, indent);
        let Some(else_branch) = else_branch else {
            return;
        };
        self.out.write(" else ");
        match &else_branch.kind {
            ExprKind::Block(block) => self.block(block, indent),
            ExprKind::If { .. } => self.if_expr(else_branch, indent),
            _ => self.expr(else_branch, prec::RETURN, indent),
        }
    }

    fn match_expr(&mut self, scrutinee: &Expr, arms: &[MatchArm], span: Span, indent: usize) {
        self.out.write("match ");
        self.header(scrutinee, indent);
        self.out.write(" ");
        if arms.is_empty() && !self.holds_comment(span) {
            self.out.write("{ }");
            self.advance(span.end);
            return;
        }
        self.out.write("{");
        let inner = indent + INDENT;
        for (i, arm) in arms.iter().enumerate() {
            self.lead(arm.span.start as usize, inner, i > 0);
            self.out.start_line(inner);
            self.out.write(&self.pattern_flat(&arm.pattern));
            self.out.write(" => ");
            self.expr(&arm.body, prec::RETURN, inner);
            self.advance(arm.span.end);
            self.trail(arm.span.end);
        }
        self.lead_close(close_brace(span.end), inner);
        self.out.start_line(indent);
        self.out.write("}");
        self.advance(span.end);
    }

    /// Writes `fn(x) { ... }`. A lambda always shows its parameter list, even
    /// when it is empty, so that `fn() { ... }` is never mistaken for the
    /// braces of a trailing closure.
    fn lambda(&mut self, is_async: bool, params: &[Param], body: &Block, indent: usize) {
        if is_async {
            self.out.write("async ");
        }
        self.out.write("fn");
        self.param_list(None, params, indent, None);
        self.out.write(" ");
        self.block(body, indent);
    }

    /// The first line a lambda would occupy, used to decide whether a call
    /// can keep its arguments on one line and let the closure expand below.
    fn lambda_header(&self, is_async: bool, params: &[Param]) -> String {
        let mut head = String::new();
        if is_async {
            head.push_str("async ");
        }
        head.push_str("fn(");
        let entries: Vec<String> = params.iter().map(|p| self.param_flat(p)).collect();
        head.push_str(&entries.join(", "));
        head.push_str(") {");
        head
    }

    fn call(&mut self, expr: &Expr, indent: usize) {
        let ExprKind::Call {
            callee,
            generics,
            args,
            trailing,
        } = &expr.kind
        else {
            return;
        };

        // A chain of calls that is merely too long breaks before its dots.
        if !self.breaks(expr) {
            let (base, ops) = flatten_postfix(expr);
            let points = chain_break_points(&ops);
            if call_count(&ops) >= 2 && !points.is_empty() {
                self.chain(base, &ops, &points, expr.span, indent);
                return;
            }
        }

        self.expr(callee, prec::POSTFIX, indent);
        self.generic_args(generics);
        self.call_tail(args, trailing.as_deref(), generics, expr.span, indent);
    }

    /// Writes the argument list, and the trailing closure when there is one.
    fn call_tail(
        &mut self,
        args: &[Arg],
        trailing: Option<&Expr>,
        generics: &[Type],
        span: Span,
        indent: usize,
    ) {
        // `tasks.spawn { ... }` writes no parentheses at all; a generic call
        // always does, because the parser only reads `<T>` as a type list
        // when a `(` follows it.
        let parens = !(args.is_empty() && trailing.is_some() && generics.is_empty());
        if parens {
            self.arg_list(args, trailing.is_some(), span, indent);
        }
        if let Some(trailing) = trailing {
            self.out.write(" ");
            match trailing_body(trailing) {
                Some(body) => self.block(body, indent),
                None => self.expr(trailing, prec::RETURN, indent),
            }
        }
    }

    fn arg_list(&mut self, args: &[Arg], has_trailing: bool, span: Span, indent: usize) {
        // A comment written between the arguments only stays where it was if
        // the list breaks, so it counts as a reason to break.
        let region_end = if has_trailing {
            args.last().map(|arg| arg.span.end)
        } else {
            Some(span.end)
        };
        let commented = region_end.is_some_and(|end| self.comment_between(span.start, end));

        let flat = format!("({})", self.args_flat(args));
        // Two more columns for the ` {` of a trailing closure.
        let reserved = if has_trailing { 2 } else { 0 };
        if !commented
            && !args.iter().any(|arg| self.breaks(&arg.value))
            && self.out.col() + width(&flat) + reserved <= MAX_WIDTH
        {
            self.out.write(&flat);
            return;
        }

        // Breaking a lone argument that cannot itself break, and would not
        // fit on a line of its own either, only makes the call longer.
        if let [only] = args {
            let value = self.flat(&only.value, prec::RETURN);
            if !commented
                && !self.breaks(&only.value)
                && only.label.is_none()
                && !only.is_var
                && !only.spread
                && indent + INDENT + width(&value) + 1 > MAX_WIDTH
            {
                self.out.write(&flat);
                return;
            }
        }

        // A final closure, call, or array argument keeps the call's head on
        // one line and expands below it, which is how a callback and a
        // struct initializer read at the call site.
        if !has_trailing && self.hug_last(args, span, indent) {
            return;
        }

        self.out.write("(");
        for (i, arg) in args.iter().enumerate() {
            self.lead(arg.span.start as usize, indent + INDENT, i > 0);
            self.out.start_line(indent + INDENT);
            self.arg_prefix(arg);
            self.expr(&arg.value, prec::RETURN, indent + INDENT);
            self.out.write(",");
            self.advance(arg.span.end);
            self.trail(arg.span.end);
        }
        if !has_trailing {
            self.lead_close(close_brace(span.end), indent + INDENT);
        }
        self.out.start_line(indent);
        self.out.write(")");
    }

    /// The first line the last argument occupies when a call hugs it: the
    /// header of a closure, the `[` of an array, or the callee and `(` of a
    /// nested call, together with whether that head bottoms out in a closure.
    ///
    /// `None` when the argument has no such head and so cannot be hugged.
    fn hug_head(&self, expr: &Expr) -> Option<(String, bool)> {
        match &expr.kind {
            ExprKind::Lambda {
                is_async, params, ..
            } => Some((self.lambda_header(*is_async, params), true)),
            ExprKind::ArrayLit(elements) if !elements.is_empty() => Some(("[".to_string(), false)),
            ExprKind::Call {
                callee,
                generics,
                args,
                trailing,
            } if trailing.is_none() && !args.is_empty() => {
                let mut head = self.flat(callee, prec::POSTFIX);
                if !generics.is_empty() {
                    let names: Vec<String> = generics.iter().map(Type::to_string).collect();
                    head.push('<');
                    head.push_str(&names.join(", "));
                    head.push('>');
                }
                head.push('(');
                let mut closure = false;
                if let Some((last, earlier)) = args.split_last() {
                    if let Some((inner, inner_closure)) = self.hug_head(&last.value) {
                        if hug_applies(inner_closure, last, earlier)
                            && !earlier.iter().any(|arg| self.breaks(&arg.value))
                        {
                            for arg in earlier {
                                head.push_str(&self.arg_flat(arg));
                                head.push_str(", ");
                            }
                            head.push_str(&self.arg_prefix_text(last));
                            head.push_str(&inner);
                            closure = inner_closure;
                        }
                    }
                }
                Some((head, closure))
            }
            _ => None,
        }
    }

    /// Writes `f(a, b, fn(x) { ... })` with the last argument expanded in
    /// place, or reports that the shape does not apply here.
    fn hug_last(&mut self, args: &[Arg], span: Span, indent: usize) -> bool {
        let Some((last, earlier)) = args.split_last() else {
            return false;
        };
        if earlier.iter().any(|arg| self.breaks(&arg.value))
            || self.comment_between(span.start, last.span.start)
        {
            return false;
        }
        let Some((inner_head, closure)) = self.hug_head(&last.value) else {
            return false;
        };
        if !hug_applies(closure, last, earlier) {
            return false;
        }

        let mut prefix = String::from("(");
        for arg in earlier {
            prefix.push_str(&self.arg_flat(arg));
            prefix.push_str(", ");
        }
        prefix.push_str(&self.arg_prefix_text(last));
        let column = self.out.col() + width(&prefix);
        if column + width(&inner_head) > MAX_WIDTH {
            return false;
        }
        // Hugging only helps when the argument really does expand below the
        // head; one that still fits on this line would leave the call as long
        // as it already was.
        if !self.breaks(&last.value)
            && column + width(&self.flat(&last.value, prec::RETURN)) <= MAX_WIDTH
        {
            return false;
        }

        self.out.write(&prefix);
        self.expr(&last.value, prec::RETURN, indent);
        self.advance(last.span.end);
        self.out.write(")");
        true
    }

    fn array_literal(&mut self, elements: &[Expr], span: Span, indent: usize) {
        self.out.write("[");
        for element in elements {
            self.lead(element.span.start as usize, indent + INDENT, false);
            self.out.start_line(indent + INDENT);
            self.expr(element, prec::RETURN, indent + INDENT);
            self.out.write(",");
            self.advance(element.span.end);
            self.trail(element.span.end);
        }
        self.lead_close(close_brace(span.end), indent + INDENT);
        self.out.start_line(indent);
        self.out.write("]");
        self.advance(span.end);
    }

    /// Writes a method chain broken before each dot that follows a call.
    fn chain(
        &mut self,
        base: &Expr,
        ops: &[Post<'_>],
        points: &[usize],
        span: Span,
        indent: usize,
    ) {
        self.expr(base, prec::POSTFIX, indent);
        let mut level = indent;
        for (i, op) in ops.iter().enumerate() {
            match op {
                Post::Field(name) => {
                    if points.contains(&i) {
                        level = indent + INDENT;
                        self.out.start_line(level);
                    }
                    self.out.write(".");
                    self.out.write(name);
                }
                Post::Call {
                    generics,
                    args,
                    trailing,
                } => {
                    self.generic_args(generics);
                    self.call_tail(args, *trailing, generics, span, level);
                }
                Post::Try => self.out.write("?"),
            }
        }
    }

    fn generic_args(&mut self, generics: &[Type]) {
        if generics.is_empty() {
            return;
        }
        let names: Vec<String> = generics.iter().map(Type::to_string).collect();
        self.out.write("<");
        self.out.write(&names.join(", "));
        self.out.write(">");
    }

    fn arg_prefix(&mut self, arg: &Arg) {
        let prefix = self.arg_prefix_text(arg);
        self.out.write(&prefix);
    }

    fn arg_prefix_text(&self, arg: &Arg) -> String {
        let mut out = String::new();
        if let Some(label) = &arg.label {
            out.push_str(&label.node);
            out.push_str(": ");
        }
        if arg.is_var {
            out.push_str("var ");
        }
        if arg.spread {
            out.push_str("...");
        }
        out
    }
}

// ---------------------------------------------------------------------------
// One-line rendering
// ---------------------------------------------------------------------------

/// The largest duration unit that divides `ns` exactly.
///
/// Used only when the source spelling is unavailable, since a duration
/// literal is stored as a nanosecond count.
fn duration_text(ns: i64) -> String {
    if ns == 0 {
        return "0ns".to_string();
    }
    for (factor, unit) in [
        (3_600_000_000_000i64, "h"),
        (60_000_000_000, "m"),
        (1_000_000_000, "s"),
        (1_000_000, "ms"),
        (1_000, "us"),
        (1, "ns"),
    ] {
        if ns % factor == 0 {
            return format!("{}{unit}", ns / factor);
        }
    }
    format!("{ns}ns")
}

impl Formatter<'_> {
    /// Renders `expr` on one line, parenthesised when its precedence is below
    /// `min`.
    fn flat(&self, expr: &Expr, min: u8) -> String {
        let inner = self.flat_inner(expr);
        if expr_prec(expr) < min {
            format!("({inner})")
        } else {
            inner
        }
    }

    fn flat_header(&self, expr: &Expr) -> String {
        if ends_with_brace(expr) {
            format!("({})", self.flat_inner(expr))
        } else {
            self.flat_inner(expr)
        }
    }

    fn flat_inner(&self, expr: &Expr) -> String {
        match &expr.kind {
            ExprKind::Int(value) => self
                .number_text(expr.span)
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string()),
            ExprKind::Float(value) => self
                .number_text(expr.span)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{value:?}")),
            ExprKind::Duration(ns) => self
                .number_text(expr.span)
                .map(str::to_string)
                .unwrap_or_else(|| duration_text(*ns)),
            ExprKind::Bool(value) => value.to_string(),
            ExprKind::Str(parts) => self
                .string_text(expr.span)
                .map(str::to_string)
                .unwrap_or_else(|| self.string_from_parts(parts)),
            ExprKind::Unit => "()".to_string(),
            ExprKind::Ident(name) => name.clone(),
            ExprKind::ArrayLit(elements) => {
                let items: Vec<String> = elements
                    .iter()
                    .map(|e| self.flat(e, prec::RETURN))
                    .collect();
                format!("[{}]", items.join(", "))
            }
            ExprKind::Field { base, name } => {
                format!("{}.{}", self.flat(base, prec::POSTFIX), name.node)
            }
            ExprKind::Call {
                callee,
                generics,
                args,
                trailing,
            } => {
                let mut out = self.flat(callee, prec::POSTFIX);
                if !generics.is_empty() {
                    let names: Vec<String> = generics.iter().map(Type::to_string).collect();
                    out.push('<');
                    out.push_str(&names.join(", "));
                    out.push('>');
                }
                if !(args.is_empty() && trailing.is_some() && generics.is_empty()) {
                    out.push('(');
                    out.push_str(&self.args_flat(args));
                    out.push(')');
                }
                if let Some(trailing) = trailing {
                    out.push(' ');
                    match trailing_body(trailing) {
                        Some(body) => out.push_str(&self.flat_block(body)),
                        None => out.push_str(&self.flat(trailing, prec::RETURN)),
                    }
                }
                out
            }
            ExprKind::Unary { op, operand } => {
                format!("{}{}", unary_symbol(*op), self.flat(operand, prec::UNARY))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let level = binary_prec(*op);
                format!(
                    "{} {} {}",
                    self.flat(lhs, level),
                    binary_symbol(*op),
                    self.flat(rhs, level + 1)
                )
            }
            ExprKind::Assign { op, target, value } => {
                let operator = match op {
                    Some(op) => format!("{}=", binary_symbol(*op)),
                    None => "=".to_string(),
                };
                format!(
                    "{} {operator} {}",
                    self.flat(target, prec::POSTFIX),
                    self.flat(value, prec::ASSIGN)
                )
            }
            ExprKind::Try(inner) => match &inner.kind {
                ExprKind::Await(awaited) => {
                    format!("await {}?", self.flat(awaited, prec::POSTFIX))
                }
                _ => format!("{}?", self.flat(inner, prec::POSTFIX)),
            },
            ExprKind::Await(inner) => format!("await {}", self.flat(inner, prec::POSTFIX)),
            ExprKind::Block(block) => self.flat_block(block),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let mut out = format!(
                    "if {} {}",
                    self.flat_header(condition),
                    self.flat_block(then_branch)
                );
                if let Some(else_branch) = else_branch {
                    out.push_str(" else ");
                    out.push_str(&self.flat(else_branch, prec::RETURN));
                }
                out
            }
            ExprKind::Match { scrutinee, arms } => {
                // Arms are comma-separated here: on one line a bare newline
                // cannot end an arm, and the parser accepts the comma.
                let arms: Vec<String> = arms
                    .iter()
                    .map(|arm| {
                        format!(
                            "{} => {}",
                            self.pattern_flat(&arm.pattern),
                            self.flat(&arm.body, prec::RETURN)
                        )
                    })
                    .collect();
                if arms.is_empty() {
                    format!("match {} {{ }}", self.flat_header(scrutinee))
                } else {
                    format!(
                        "match {} {{ {} }}",
                        self.flat_header(scrutinee),
                        arms.join(", ")
                    )
                }
            }
            ExprKind::For {
                binding,
                iterable,
                body,
            } => format!(
                "for {} in {} {}",
                binding.node,
                self.flat_header(iterable),
                self.flat_block(body)
            ),
            ExprKind::While { condition, body } => format!(
                "while {} {}",
                self.flat_header(condition),
                self.flat_block(body)
            ),
            ExprKind::Scope { name, body } => {
                format!("scope {} {}", name.node, self.flat_block(body))
            }
            ExprKind::Return(value) => match value {
                Some(value) => format!("return {}", self.flat(value, prec::RETURN)),
                None => "return".to_string(),
            },
            ExprKind::Break(value) => match value {
                Some(value) => format!("break {}", self.flat(value, prec::RETURN)),
                None => "break".to_string(),
            },
            ExprKind::Continue => "continue".to_string(),
            ExprKind::Lambda {
                is_async,
                params,
                body,
            } => {
                let mut out = String::new();
                if *is_async {
                    out.push_str("async ");
                }
                out.push_str("fn(");
                let entries: Vec<String> = params.iter().map(|p| self.param_flat(p)).collect();
                out.push_str(&entries.join(", "));
                out.push_str(") ");
                out.push_str(&self.flat_block(body));
                out
            }
            ExprKind::Range {
                start,
                end,
                inclusive_end,
            } => format!(
                "{}{}{}",
                self.flat(start, prec::ADDITIVE),
                if *inclusive_end { ".." } else { "..<" },
                self.flat(end, prec::ADDITIVE)
            ),
        }
    }

    fn args_flat(&self, args: &[Arg]) -> String {
        let entries: Vec<String> = args.iter().map(|arg| self.arg_flat(arg)).collect();
        entries.join(", ")
    }

    fn arg_flat(&self, arg: &Arg) -> String {
        format!(
            "{}{}",
            self.arg_prefix_text(arg),
            self.flat(&arg.value, prec::RETURN)
        )
    }

    fn flat_block(&self, block: &Block) -> String {
        if block_is_empty(block) {
            return "{ }".to_string();
        }
        let mut parts: Vec<String> = block
            .statements
            .iter()
            .map(|stmt| self.stmt_flat(stmt))
            .collect();
        if let Some(tail) = &block.tail {
            parts.push(self.flat(tail, prec::RETURN));
        }
        format!("{{ {} }}", parts.join(" "))
    }

    fn stmt_flat(&self, stmt: &Stmt) -> String {
        match &stmt.kind {
            StmtKind::Expr(value) => self.flat(value, prec::RETURN),
            StmtKind::Let {
                is_var,
                name,
                ty,
                value,
            } => {
                let mut out = String::from(if *is_var { "var " } else { "let " });
                out.push_str(&name.node);
                if let Some(ty) = ty {
                    out.push_str(": ");
                    out.push_str(&ty.to_string());
                }
                out.push_str(" = ");
                out.push_str(&self.flat(value, prec::RETURN));
                out
            }
            StmtKind::Item(item) => {
                let mut sub = Formatter::new("");
                sub.item(item, 0);
                sub.finish()
                    .lines()
                    .map(str::trim)
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        }
    }

    /// Rebuilds a string literal from its parsed parts, for the rare case
    /// where the source spelling is unavailable.
    fn string_from_parts(&self, parts: &[StrPart]) -> String {
        let mut out = String::from("\"");
        for part in parts {
            match part {
                StrPart::Text(text) => {
                    for c in text.chars() {
                        match c {
                            '\\' => out.push_str("\\\\"),
                            '"' => out.push_str("\\\""),
                            '\n' => out.push_str("\\n"),
                            '\t' => out.push_str("\\t"),
                            '\r' => out.push_str("\\r"),
                            '\0' => out.push_str("\\0"),
                            '{' => out.push_str("\\{"),
                            '}' => out.push_str("\\}"),
                            c => out.push(c),
                        }
                    }
                }
                StrPart::Interpolation(value) => {
                    out.push('{');
                    out.push_str(&self.flat(value, prec::RETURN));
                    out.push('}');
                }
            }
        }
        out.push('"');
        out
    }

    fn pattern_flat(&self, pattern: &Pattern) -> String {
        match &pattern.kind {
            PatternKind::Wildcard => "_".to_string(),
            PatternKind::Binding(name) => name.clone(),
            PatternKind::Literal(value) => self.flat(value, prec::RETURN),
            PatternKind::Variant { path, payload } => {
                let path: Vec<&str> = path.iter().map(|p| p.node.as_str()).collect();
                let mut out = path.join(".");
                if !payload.is_empty() {
                    let items: Vec<String> = payload.iter().map(|p| self.pattern_flat(p)).collect();
                    out.push('(');
                    out.push_str(&items.join(", "));
                    out.push(')');
                }
                out
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cove_diag::SourceMap;
    use std::path::{Path, PathBuf};

    /// Test sources are written as raw strings that begin with a newline, so
    /// that the first line lines up with the rest in this file.
    fn src(text: &str) -> String {
        text.strip_prefix('\n').unwrap_or(text).to_string()
    }

    fn parse(source: &str) -> SourceUnit {
        let mut sources = SourceMap::new();
        let file = sources.add("test.cove", source.to_string());
        match crate::parse_file(&sources, file) {
            Ok(unit) => unit,
            Err(diagnostics) => {
                let rendered: Vec<String> = diagnostics
                    .iter()
                    .map(|d| cove_diag::render(&sources, d))
                    .collect();
                panic!("source does not parse:\n{}", rendered.join(""));
            }
        }
    }

    fn format(source: &str) -> String {
        format_source(source, &parse(source))
    }

    /// Asserts that `source` is already formatted, which is how this module
    /// records the intended shape of a construct.
    fn formatted(source: &str) {
        let source = src(source);
        assert_eq!(format(&source), source, "\n--- source was:\n{source}");
    }

    /// Asserts that `source` formats to `expected`, and that `expected` is a
    /// fixed point.
    fn reformats(source: &str, expected: &str) {
        let source = src(source);
        let expected = src(expected);
        assert_eq!(format(&source), expected, "\n--- source was:\n{source}");
        assert_eq!(format(&expected), expected, "formatting is not idempotent");
    }

    /// The tree, with every span erased, so that two trees can be compared
    /// for the structure that formatting must preserve.
    fn without_spans(unit: &SourceUnit) -> String {
        let text = format!("{unit:?}");
        let mut out = String::new();
        let mut rest = text.as_str();
        while let Some(start) = rest.find("Span {") {
            out.push_str(&rest[..start]);
            out.push_str("Span");
            let close = rest[start..]
                .find('}')
                .expect("a Span renders as one set of braces");
            rest = &rest[start + close + 1..];
        }
        out.push_str(rest);
        out
    }

    // -- items -------------------------------------------------------------

    #[test]
    fn keeps_uses_in_source_order_above_one_blank_line() {
        reformats(
            "
use console

use console.println
use http


/// A function.
fn a() { }
",
            "
use console
use console.println
use http

/// A function.
fn a() { }
",
        );
    }

    #[test]
    fn separates_top_level_items_with_one_blank_line() {
        reformats(
            "
fn a() { }
fn b() { }



fn c() { }
",
            "
fn a() { }

fn b() { }

fn c() { }
",
        );
    }

    #[test]
    fn formats_function_declarations() {
        formatted(
            "
/// Documented.
export async fn run<T>(self, var count: Int, items: String...) -> Result<T, U> {
  count
}
",
        );
    }

    #[test]
    fn formats_a_mutating_receiver_and_a_default_parameter() {
        formatted(
            "
impl Counter {
  /// Bumps.
  fn bump(var self, by: Int = 2 * 21) -> Int {
    self.hits
  }
}
",
        );
    }

    #[test]
    fn formats_a_trait_with_docs_defaults_and_an_associated_function() {
        formatted(
            "
/// A value that can render itself for a human.
export trait Display {
  /// Returns the human-readable form.
  fn describe(self) -> String

  /// Returns a short label.
  fn label(self) -> String {
    self.describe()
  }

  /// Builds one from nothing.
  fn empty() -> Int
}
",
        );
    }

    #[test]
    fn writes_an_empty_trait_on_one_line() {
        reformats(
            "
trait Marker {
}
",
            "
trait Marker { }
",
        );
    }

    #[test]
    fn formats_a_conformance_and_keeps_it_apart_from_an_inherent_impl() {
        formatted(
            "
impl Display for Booking {
  fn describe(self) -> String {
    \"booking\"
  }
}

impl Booking {
  /// The identifier.
  fn id(self) -> Int {
    1
  }
}
",
        );
    }

    #[test]
    fn formats_bounds_on_type_parameters() {
        formatted(
            "
fn render<T: Display, U, V: Display + Ordered>(value: T, other: V) -> String {
  value.describe()
}
",
        );
    }

    #[test]
    fn formats_dyn_types() {
        formatted(
            "
fn renderAll(values: Array<dyn Display>, one: dyn Display) -> dyn Display {
  one
}
",
        );
    }

    #[test]
    fn keeps_comments_inside_a_trait() {
        formatted(
            "
trait Display {
  // Required.
  fn describe(self) -> String

  /// Defaulted.
  fn label(self) -> String {
    // Falls back.
    self.describe()
  }
}
",
        );
    }

    #[test]
    fn writes_struct_fields_one_per_line() {
        reformats(
            "
export struct Point { x: Int, y: Int }

struct Tag(name: String, weight: Int)

struct Empty { }
",
            "
export struct Point {
  x: Int
  y: Int
}

struct Tag {
  name: String
  weight: Int
}

struct Empty { }
",
        );
    }

    #[test]
    fn writes_enum_cases_one_per_line_with_their_docs() {
        reformats(
            "
export enum Status { Pending, Active(Int), Failed(String, Int) }
",
            "
export enum Status {
  Pending
  Active(Int)
  Failed(String, Int)
}
",
        );
    }

    #[test]
    fn formats_documented_fields_and_cases() {
        formatted(
            "
struct Config {
  /// The port.
  port: Int
}

enum Level {
  /// The quiet one.
  Debug
}
",
        );
    }

    #[test]
    fn formats_impl_blocks_and_type_aliases() {
        formatted(
            "
export type Handler = async fn(request: http.Request) -> Result<Unit, Error>

impl Metrics<T> {
  /// One.
  fn one(self) -> Int {
    1
  }

  /// Two.
  export fn two() -> Int {
    2
  }
}

impl Empty { }
",
        );
    }

    #[test]
    fn keeps_a_multi_line_doc_comment_attached() {
        formatted(
            "
/// One line.
///
/// Another line.
fn documented() { }
",
        );
    }

    #[test]
    fn removes_a_blank_line_between_a_doc_comment_and_its_declaration() {
        reformats(
            "
/// Documented.

fn documented() { }
",
            "
/// Documented.
fn documented() { }
",
        );
    }

    // -- types -------------------------------------------------------------

    #[test]
    fn formats_every_type_form() {
        formatted(
            "
fn types(
  a: Int,
  b: Array<String>,
  c: Map<String, Array<Int>>,
  d: http.Request,
  e: (),
  f: fn(String) -> Int,
  g: async fn(name: String, var items: Int...),
) { }
",
        );
    }

    // -- statements --------------------------------------------------------

    #[test]
    fn formats_statements() {
        formatted(
            "
fn statements() {
  let plain = 1
  let typed: Array<Int> = [1, 2]
  var mutable = 3
  mutable = 4
  mutable += 1
  mutable -= 1
  mutable *= 1
  mutable /= 1
  mutable %= 1
  mutable
}
",
        );
    }

    #[test]
    fn keeps_one_blank_line_between_statements_and_none_at_a_block_edge() {
        reformats(
            "
fn spaced() {

  let a = 1



  let b = 2

}
",
            "
fn spaced() {
  let a = 1

  let b = 2
}
",
        );
    }

    #[test]
    fn formats_a_nested_declaration_inside_a_block() {
        formatted(
            "
fn outer() {
  /// Inner.
  fn inner() -> Int {
    1
  }
  inner()
}
",
        );
    }

    // -- expressions -------------------------------------------------------

    #[test]
    fn formats_literals_and_keeps_their_spelling() {
        formatted(
            "
fn literals() {
  let ints = [1_000_000, 0xff, 0b1010]
  let floats = [1.5, 1_000.5, 1.5e3, 2e-2]
  let durations = [1ns, 500ms, 60s, 1h]
  let bools = [true, false]
  let unit = ()
  let text = \"escapes \\\\ \\\" \\n and {ints} interpolation\"
  text
}
",
        );
    }

    #[test]
    fn formats_is_at_the_same_precedence_as_comparison() {
        formatted(
            "
fn identity(a: Vector<Int>, b: Vector<Int>) {
  a is b && a == b
}
",
        );
    }

    #[test]
    fn formats_calls_labels_spreads_generics_and_trailing_closures() {
        formatted(
            "
fn calls(var output: Vector<Int>) {
  plain(1, 2)
  labeled(low: 1, high: 2)
  fill(var output)
  joinAll(\"-\", ...ready)
  api.fetch<Array<Booking>>(\"/bookings\")
  tasks.spawn { fetch() }
  clock.timeout(500ms) { fetch() }
  Point(x: 1, y: 2)
}
",
        );
    }

    #[test]
    fn formats_lambdas_with_and_without_parameters() {
        formatted(
            "
fn lambdas() {
  let one = fn(n) {
    n * 2
  }
  let none = async fn() {
    one
  }
  none
}
",
        );
    }

    #[test]
    fn parenthesises_only_where_precedence_requires_it() {
        reformats(
            "
fn precedence() {
  let a = (1 + 2) * 3
  let b = 1 + (2 * 3)
  let c = -(1 + 2)
  let d = !(a && b)
  let e = (a + b)?
  let f = (a + b).field
  let g = a || b && c
  let h = (a || b) && c
  let i = (0..<3).isEmpty()
  let j = 1 - (2 - 3)
  j
}
",
            "
fn precedence() {
  let a = (1 + 2) * 3
  let b = 1 + 2 * 3
  let c = -(1 + 2)
  let d = !(a && b)
  let e = (a + b)?
  let f = (a + b).field
  let g = a || b && c
  let h = (a || b) && c
  let i = (0..<3).isEmpty()
  let j = 1 - (2 - 3)
  j
}
",
        );
    }

    #[test]
    fn writes_await_before_the_question_mark_it_propagates() {
        formatted(
            "
async fn awaiting() {
  await task()?
  await task()?.field
  await task()
}
",
        );
    }

    #[test]
    fn formats_control_flow_across_lines() {
        formatted(
            "
fn control(items: Array<Int>) -> Int {
  if items.isEmpty() {
    0
  } else if items.length() == 1 {
    1
  } else {
    2
  }

  for item in items {
    item
  }

  for index in 0..<3 {
    index
  }

  while items.isEmpty() {
    items
  }

  scope tasks {
    tasks
  }

  {
    let inner = 1
    inner
  }

  return 1
}
",
        );
    }

    #[test]
    fn writes_match_arms_one_per_line() {
        reformats(
            "
fn matching(value: Card) -> String {
  match value {
    -1 => \"minus one\",
    \"yes\" => \"literal\",
    true => \"flag\",
    Card.Blank => \"blank\",
    Card.Numbered(count) => { let doubled = count * 2
      \"{doubled}\" }
    other => other,
    _ => \"wildcard\",
  }
}
",
            "
fn matching(value: Card) -> String {
  match value {
    -1 => \"minus one\"
    \"yes\" => \"literal\"
    true => \"flag\"
    Card.Blank => \"blank\"
    Card.Numbered(count) => {
      let doubled = count * 2
      \"{doubled}\"
    }
    other => other
    _ => \"wildcard\"
  }
}
",
        );
    }

    #[test]
    fn parenthesises_a_header_that_would_otherwise_end_in_a_brace() {
        let source = src("
fn headers() {
  if (tasks.spawn { ready() }) {
    1
  }
}
");
        assert_eq!(format(&source), source);
        assert_eq!(
            without_spans(&parse(&source)),
            without_spans(&parse(&format(&source)))
        );
    }

    #[test]
    fn writes_an_empty_block_on_one_line() {
        formatted(
            "
fn empty() {
  let nothing = { }
  nothing
}
",
        );
    }

    // -- line breaking -----------------------------------------------------

    #[test]
    fn breaks_an_argument_list_one_per_line_with_a_trailing_comma() {
        reformats(
            "
fn wide() {
  configure(alphaValueHere, betaValueHere, gammaValueHere, deltaValueHere, epsilon)
}
",
            "
fn wide() {
  configure(
    alphaValueHere,
    betaValueHere,
    gammaValueHere,
    deltaValueHere,
    epsilon,
  )
}
",
        );
    }

    #[test]
    fn breaks_a_parameter_list_one_per_line_when_the_signature_is_too_wide() {
        reformats(
            "
fn wideSignature(alphaValue: String, betaValue: String, gammaValue: String) -> Int {
  1
}
",
            "
fn wideSignature(
  alphaValue: String,
  betaValue: String,
  gammaValue: String,
) -> Int {
  1
}
",
        );
    }

    #[test]
    fn breaks_an_array_literal_one_element_per_line() {
        reformats(
            "
fn wideArray() {
  let items = [alphaValueHere, betaValueHere, gammaValueHere, deltaValueHere, eps]
  items
}
",
            "
fn wideArray() {
  let items = [
    alphaValueHere,
    betaValueHere,
    gammaValueHere,
    deltaValueHere,
    eps,
  ]
  items
}
",
        );
    }

    #[test]
    fn breaks_a_binary_expression_after_its_operator() {
        reformats(
            "
fn wideSum() {
  let total = alphaValueHere + betaValueHere + gammaValueHere + deltaValueHereOne
  total
}
",
            "
fn wideSum() {
  let total = alphaValueHere + betaValueHere + gammaValueHere +
    deltaValueHereOne
  total
}
",
        );
    }

    #[test]
    fn breaks_a_method_chain_before_each_dot() {
        reformats(
            "
fn wideChain(reading: Reading) {
  reading.normalise().withPrecision(4).formattedAsText().withoutTrailingZeroesAt()
}
",
            "
fn wideChain(reading: Reading) {
  reading.normalise()
    .withPrecision(4)
    .formattedAsText()
    .withoutTrailingZeroesAt()
}
",
        );
    }

    #[test]
    fn breaks_an_argument_list_above_a_trailing_closure() {
        reformats(
            "
fn wideTrailing() {
  clock.timeout(alphaValueHere, betaValueHere, gammaValueHere, deltaValueHereOk) { work() }
  1
}
",
            "
fn wideTrailing() {
  clock.timeout(
    alphaValueHere,
    betaValueHere,
    gammaValueHere,
    deltaValueHereOk,
  ) {
    work()
  }
  1
}
",
        );
    }

    #[test]
    fn keeps_a_call_head_on_one_line_and_expands_a_closure_below_it() {
        formatted(
            "
fn callbacks(app: App) {
  builder.get(\"/health\", withObservability(app, async fn(request) {
    Ok(request)
  }))
}
",
        );
    }

    #[test]
    fn expands_a_sole_initializer_argument_in_place() {
        formatted(
            "
fn initializers(builder: RouterBuilder, path: String, handler: Handler) {
  builder.routes.push(Route(
    method: http.Method.Get,
    path: path,
    handler: handler,
  ))
}
",
        );
    }

    #[test]
    fn leaves_a_lone_unbreakable_argument_where_it_is() {
        // Breaking it would produce a line that is still too long.
        formatted(
            "
fn unbreakable() {
  println(\"a string literal so long that no line break anywhere can ever help\")?
}
",
        );
    }

    #[test]
    fn breaks_a_function_type_that_does_not_fit() {
        reformats(
            "
export type Handler = async fn(request: http.Request, retries: Int) -> Result<http.Response, Error>
",
            "
export type Handler = async fn(
  request: http.Request,
  retries: Int,
) -> Result<http.Response, Error>
",
        );
    }

    #[test]
    fn every_break_it_introduces_still_parses_the_same_way() {
        let sources = [
            "fn a() {\n  configure(alphaValueHere, betaValueHere, gammaValueHere, deltaValueHere, epsilon)\n}\n",
            "fn a() {\n  let total = alphaValueHere + betaValueHere + gammaValueHere + deltaValueHereOne\n}\n",
            "fn a(reading: R) {\n  reading.normalise().withPrecision(4).formattedAsText().withoutTrailingZeroesAt()\n}\n",
            "fn a() {\n  let items = [alphaValueHere, betaValueHere, gammaValueHere, deltaValueHere, eps]\n}\n",
            "fn wideSignature(alphaValue: String, betaValue: String, gammaValue: String) -> Int {\n  1\n}\n",
            "fn a() {\n  builder.get(\"/health\", withObservability(app, async fn(request) { Ok(1) }))\n}\n",
            "fn a() {\n  clock.timeout(alphaValueHere, betaValueHere, gammaValueHere, deltaValue) { w() }\n  1\n}\n",
        ];
        for source in sources {
            let formatted = format(source);
            assert_ne!(formatted, source, "this case is meant to be reformatted");
            assert_eq!(
                without_spans(&parse(source)),
                without_spans(&parse(&formatted)),
                "reformatting changed the tree of:\n{source}\ninto:\n{formatted}"
            );
        }
    }

    // -- comments ----------------------------------------------------------

    #[test]
    fn keeps_a_comment_on_its_own_line_above_what_follows_it() {
        formatted(
            "
// Above the use.
use console.println

// Above the item.
fn commented() {
  // Above the statement.
  let a = 1

  // After a blank line.
  a
}
",
        );
    }

    #[test]
    fn keeps_a_comment_at_the_end_of_the_line_it_was_written_on() {
        formatted(
            "
fn trailing() {
  let a = 1 // one
  a
}
",
        );
    }

    #[test]
    fn aligns_a_run_of_trailing_comments() {
        reformats(
            "
fn aligned() {
  let a = 1 // one
  let longerName = 2 // two
  let c = 3 // three

  let d = 4 // alone
  d
}
",
            "
fn aligned() {
  let a = 1          // one
  let longerName = 2 // two
  let c = 3          // three

  let d = 4 // alone
  d
}
",
        );
    }

    #[test]
    fn keeps_a_comment_before_a_closing_brace() {
        formatted(
            "
fn closing() {
  let a = 1
  // The last word.
}
",
        );
    }

    #[test]
    fn keeps_a_comment_at_the_end_of_the_file() {
        formatted(
            "
fn done() { }

// A closing remark
// over two lines.
",
        );
    }

    #[test]
    fn keeps_a_block_comment_and_re_indents_its_continuation_lines() {
        reformats(
            "
fn blocks() {
        /* one
           two */
  let a = 1 /* trailing */
  a
}
",
            "
fn blocks() {
  /* one
     two */
  let a = 1 /* trailing */
  a
}
",
        );
    }

    #[test]
    fn keeps_a_comment_inside_an_argument_list_by_breaking_the_call() {
        formatted(
            "
fn inside() {
  configure(
    // Why alpha.
    alpha,
    beta, // and beta
  )
}
",
        );
    }

    #[test]
    fn keeps_a_comment_between_enum_cases_and_struct_fields() {
        formatted(
            "
struct Point {
  // The horizontal one.
  x: Int
  y: Int // the vertical one
}

enum Level {
  // The quiet one.
  Debug
  Info
}
",
        );
    }

    #[test]
    fn never_drops_a_comment() {
        let source = src("
// one
use console.println // two

// three
fn a(/* four */ x: Int) { // five
  // six
  let y = x // seven
  /* eight */
  y
  // nine
}
// ten
");
        let output = format(&source);
        for word in [
            "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
        ] {
            assert!(
                output.contains(word),
                "comment `{word}` was dropped:\n{output}"
            );
        }
        assert_eq!(format(&output), output, "formatting is not idempotent");
    }

    // -- whole-repository properties ---------------------------------------

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .canonicalize()
            .expect("the workspace root exists")
    }

    fn cove_files() -> Vec<PathBuf> {
        fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            let mut paths: Vec<PathBuf> =
                entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
            paths.sort();
            for path in paths {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if name.starts_with('.') || name == "target" {
                    continue;
                }
                if path.is_dir() {
                    walk(&path, found);
                } else if path.extension().and_then(|e| e.to_str()) == Some("cove") {
                    found.push(path);
                }
            }
        }
        let mut found = Vec::new();
        walk(&repo_root(), &mut found);
        assert!(!found.is_empty(), "the repository has `.cove` files");

        // The end-to-end suite deliberately contains sources that do not
        // parse, because it pins the diagnostics they produce. `cove fmt`
        // never rewrites a file it cannot parse, so neither do these tests.
        let mut formattable = Vec::new();
        let mut unparsable = Vec::new();
        for path in found {
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut sources = SourceMap::new();
            let file = sources.add(path.clone(), source);
            if crate::parse_file(&sources, file).is_ok() {
                formattable.push(path);
            } else {
                unparsable.push(path);
            }
        }
        assert!(
            formattable.len() > unparsable.len() * 4,
            "most of the repository should parse; {} of {} did not",
            unparsable.len(),
            formattable.len() + unparsable.len()
        );
        formattable
    }

    #[test]
    fn formatting_every_repository_file_twice_changes_nothing() {
        for path in cove_files() {
            let source = std::fs::read_to_string(&path).expect("the file is readable");
            let once = format(&source);
            let twice = format(&once);
            assert_eq!(once, twice, "`{}` is not a fixed point", path.display());
        }
    }

    #[test]
    fn formatting_every_repository_file_preserves_its_tree() {
        for path in cove_files() {
            let source = std::fs::read_to_string(&path).expect("the file is readable");
            let formatted = format(&source);
            assert_eq!(
                without_spans(&parse(&source)),
                without_spans(&parse(&formatted)),
                "formatting `{}` changed its tree",
                path.display()
            );
        }
    }

    #[test]
    fn every_repository_file_formats_to_clean_layout() {
        for path in cove_files() {
            let source = std::fs::read_to_string(&path).expect("the file is readable");
            let formatted = format(&source);
            assert!(
                formatted.ends_with('\n') && !formatted.ends_with("\n\n"),
                "`{}` does not end in exactly one newline",
                path.display()
            );
            for (i, line) in formatted.lines().enumerate() {
                assert!(
                    !line.contains('\t'),
                    "`{}` line {} contains a tab",
                    path.display(),
                    i + 1
                );
                assert_eq!(
                    line.trim_end(),
                    line,
                    "`{}` line {} has trailing whitespace",
                    path.display(),
                    i + 1
                );
            }
        }
    }

    #[test]
    fn formatting_every_repository_file_keeps_every_comment() {
        for path in cove_files() {
            let source = std::fs::read_to_string(&path).expect("the file is readable");
            let formatted = format(&source);
            let before = scan_comments(&source);
            let after = scan_comments(&formatted);
            assert_eq!(
                before.len(),
                after.len(),
                "formatting `{}` changed how many comments it has",
                path.display()
            );
        }
    }

    #[test]
    fn format_unit_formats_a_tree_without_its_source() {
        let unit = parse("fn a() {\n  let x = 0xff\n  x\n}\n");
        assert_eq!(format_unit(&unit), "fn a() {\n  let x = 255\n  x\n}\n");
    }
}
