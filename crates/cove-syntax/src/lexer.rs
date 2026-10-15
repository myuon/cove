//! The Cove lexer.
//!
//! Converts one source file into a flat token stream. Lexing never panics on
//! malformed input: every lexical error is collected as a [`Diagnostic`] and
//! the lexer recovers by skipping the offending text, so a single call to
//! [`lex`] can report every problem in a file at once.

use cove_diag::{Diagnostic, FileId, SourceMap, Span};

use crate::token::{Keyword, StringPart, Token, TokenKind};

/// One level of the nesting [`Lexer::skip_interpolation_body`] steps over: a
/// `{` that opened an interpolation, a `"` or a `"""` that opened a string
/// literal, or a `'` that opened a code point.
enum Unclosed {
    Brace,
    Quote,
    TripleQuote,
    Apostrophe,
}

/// Where the scan for a `"""` literal's closer stopped. See
/// [`Lexer::find_triple_closer`].
enum TripleEnd {
    /// The byte offset of the closing `"""`.
    Closer(usize),
    /// End of file, with no closer.
    Eof,
    /// An interpolation opened at this offset was never closed.
    OpenHole(usize),
}

/// Whether `c` is horizontal whitespace: what a `"""` literal's margin is
/// made of, and what it drops from the end of a line.
fn is_horizontal_space(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Lexes `file` out of `sources` into a token stream.
///
/// The returned stream always ends with exactly one [`TokenKind::Eof`] whose
/// span is the empty range at end of file. On any lexical error, every error
/// found in the file is collected and returned; no tokens are produced in
/// that case.
pub fn lex(sources: &SourceMap, file: FileId) -> Result<Vec<Token>, Vec<Diagnostic>> {
    let (tokens, diagnostics) = lex_recovered(sources, file);
    if diagnostics.is_empty() {
        Ok(tokens)
    } else {
        Err(diagnostics)
    }
}

/// Lexes `file` and answers both halves of what the lexer found: the tokens it
/// recovered *and* everything it complained about.
///
/// [`lex`] is this function with the tokens thrown away whenever there is a
/// diagnostic, which is the right contract for a compiler — a parser handed a
/// token stream with a hole in it reports a second, invented error at the
/// hole. It is the wrong contract for a reader looking at the text. The
/// recovery this exposes is not new: the lexer has always skipped past a
/// lexical error and carried on so that one call reports every problem in a
/// file, and these are the tokens that pass produced.
///
/// The caller is what makes the difference. `crates/cove-wasm`'s highlighter
/// colours source that is being typed, which is a state that does not lex for
/// most of the time a string literal is being written, and a highlighter that
/// had nothing to say about a file with one open quote in it would have
/// nothing to say most of the time.
///
/// The tokens are the same tokens [`lex`] would have answered had there been
/// no diagnostic; nothing is invented to fill a gap. Text the lexer skipped is
/// simply absent from the stream, so a caller that cares what is between two
/// tokens must look at the source.
pub fn lex_recovered(sources: &SourceMap, file: FileId) -> (Vec<Token>, Vec<Diagnostic>) {
    let text = sources.get(file).text.as_str();
    let mut lexer = Lexer {
        text,
        file,
        pos: 0,
        tokens: Vec::new(),
        diagnostics: Vec::new(),
        pending_newline: false,
    };
    lexer.run();
    (lexer.tokens, lexer.diagnostics)
}

struct Lexer<'a> {
    text: &'a str,
    file: FileId,
    pos: usize,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
    /// Set once a line break is seen in the trivia before the next token, and
    /// cleared when that token is produced.
    pending_newline: bool,
}

/// The character an escape sequence spells, or `None` when it spells none.
///
/// One table for both literal forms, because the rule is one: **a code-point
/// literal takes the escapes a string takes, and `\'` besides.** `\'` is
/// therefore legal in a string too, where the apostrophe needs no escaping —
/// which is a widening, and a deliberate one: a second table would be a
/// second rule to remember and a second place for the two to drift apart.
///
/// `\u{...}` is not in this table because it is not one character long: it
/// is read by [`Lexer::unicode_escape`], which both literal forms call when
/// they meet a `u`, so both gained it at once — which is what
/// [ADR 0047](../../../docs/adr/0047-a-code-point-is-written-as-a-character-and-is-an-int.md)
/// asked of whoever added it.
/// [ADR 0094](../../../docs/adr/0094-a-string-names-a-scalar-by-number-with-u.md)
/// is the record of adding it.
fn escaped_char(escaped: char) -> Option<char> {
    Some(match escaped {
        '\\' => '\\',
        '"' => '"',
        '\'' => '\'',
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        '0' => '\0',
        '{' => '{',
        '}' => '}',
        _ => return None,
    })
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Nanoseconds per unit for a duration suffix, or `None` if `unit` is not one
/// of `ns`, `us`, `ms`, `s`, `m`, `h`.
fn duration_factor(unit: &str) -> Option<i64> {
    Some(match unit {
        "ns" => 1,
        "us" => 1_000,
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "m" => 60_000_000_000,
        "h" => 3_600_000_000_000,
        _ => return None,
    })
}

impl<'a> Lexer<'a> {
    fn peek_char(&self) -> Option<char> {
        self.text[self.pos..].chars().next()
    }

    fn peek_char_at(&self, n: usize) -> Option<char> {
        self.text[self.pos..].chars().nth(n)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    /// Produces one token, transferring any line break seen in the trivia
    /// before it onto [`Token::preceded_by_newline`].
    fn push_token(&mut self, kind: TokenKind, start: usize) {
        let span = Span::new(self.file, start as u32, self.pos as u32);
        let preceded_by_newline = std::mem::take(&mut self.pending_newline);
        self.tokens.push(Token {
            kind,
            span,
            preceded_by_newline,
        });
    }

    fn run(&mut self) {
        loop {
            self.skip_whitespace();
            let start = self.pos;
            let Some(c) = self.peek_char() else { break };

            if c == '/' {
                if let Some(kind) = self.handle_slash() {
                    self.push_token(kind, start);
                }
                continue;
            }

            if c == '"' {
                if let Some(parts) = self.lex_any_string() {
                    self.push_token(TokenKind::Str(parts), start);
                }
                continue;
            }

            if c == '\'' {
                self.bump();
                if let Some(kind) = self.lex_code_point(start) {
                    self.push_token(kind, start);
                }
                continue;
            }

            if c.is_ascii_digit() {
                let kind = self.lex_number(start);
                self.push_token(kind, start);
                continue;
            }

            if is_ident_start(c) {
                let kind = self.lex_ident(start);
                match kind {
                    // A name written against a quote tags the literal, ADR
                    // 0101: adjacency changes the token, as a duration's
                    // suffix does.
                    TokenKind::Ident(tag) if self.peek_char() == Some('"') => {
                        if let Some(parts) = self.lex_any_string() {
                            self.push_token(TokenKind::Tagged { tag, parts }, start);
                        }
                    }
                    kind => self.push_token(kind, start),
                }
                continue;
            }

            if let Some(kind) = self.lex_operator() {
                self.push_token(kind, start);
                continue;
            }

            if c == '@' {
                self.bump();
                self.reserved_annotation(start);
                continue;
            }

            self.bump();
            self.unexpected_character(c, start);
        }

        let eof = self.pos as u32;
        self.push_token(TokenKind::Eof, eof as usize);
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek_char() {
            match c {
                '\n' => self.pending_newline = true,
                ' ' | '\t' | '\r' => {}
                _ => return,
            }
            self.bump();
        }
    }

    fn unexpected_character(&mut self, c: char, start: usize) {
        let span = Span::new(self.file, start as u32, self.pos as u32);
        let mut diag = Diagnostic::error(
            "cove::lex::unexpected_character",
            format!("unexpected character `{c}`"),
        )
        .at(span);
        if c == ';' {
            diag = diag
                .rule("Cove statements are not terminated by `;`.")
                .help("Remove the `;`; the next token or a newline ends the statement.");
        }
        self.diagnostics.push(diag);
    }

    /// `@` is reserved surface, not merely an unknown character: the
    /// Language Card reserves decorator syntax for behavior with specified
    /// compiler or runtime semantics, and the MVP defines none. This is
    /// reported distinctly from [`Lexer::unexpected_character`] so the
    /// message states that rule instead of reading as a stray-character typo.
    fn reserved_annotation(&mut self, start: usize) {
        let span = Span::new(self.file, start as u32, self.pos as u32);
        self.diagnostics.push(
            Diagnostic::error(
                "cove::parse::reserved_annotation",
                "`@` is reserved decorator syntax",
            )
            .at(span)
            .rule(
                "Decorator syntax is reserved for behavior with specified compiler or runtime \
                 semantics; the MVP defines no annotations, so an unknown annotation is an error.",
            )
            .help("remove the `@...`; there is no annotation the MVP recognizes yet"),
        );
    }

    fn find_line_end(&self) -> usize {
        match self.text[self.pos..].find('\n') {
            Some(i) => self.pos + i,
            None => self.text.len(),
        }
    }

    /// Handles everything that can start with `/`: line comments, doc
    /// comments, block comments, `/=`, and plain `/`.
    ///
    /// Returns `Some(kind)` when a token should be produced, or `None` when a
    /// comment was discarded.
    fn handle_slash(&mut self) -> Option<TokenKind> {
        let rest = &self.text[self.pos..];

        if rest.starts_with("///") && !rest.starts_with("////") {
            self.pos += 3;
            let content_start = self.pos;
            let line_end = self.find_line_end();
            let content = &self.text[content_start..line_end];
            let content = content.strip_prefix(' ').unwrap_or(content);
            let text = content.trim_end().to_string();
            self.pos = line_end;
            return Some(TokenKind::DocComment(text));
        }

        if rest.starts_with("//") {
            self.pos = self.find_line_end();
            return None;
        }

        if rest.starts_with("/*") {
            let start = self.pos;
            self.pos += 2;
            self.skip_block_comment(start);
            if self.text[start..self.pos].contains('\n') {
                self.pending_newline = true;
            }
            return None;
        }

        if rest.starts_with("/=") {
            self.pos += 2;
            return Some(TokenKind::SlashEq);
        }

        self.pos += 1;
        Some(TokenKind::Slash)
    }

    /// Skips a `/* ... */` comment, `start` being the offset of its opening
    /// `/`. Block comments nest. `self.pos` must already be past the opening
    /// `/*`.
    fn skip_block_comment(&mut self, start: usize) {
        let mut depth = 1u32;
        loop {
            if self.text[self.pos..].starts_with("*/") {
                self.pos += 2;
                depth -= 1;
                if depth == 0 {
                    return;
                }
                continue;
            }
            if self.text[self.pos..].starts_with("/*") {
                self.pos += 2;
                depth += 1;
                continue;
            }
            if self.bump().is_none() {
                let span = Span::new(self.file, start as u32, self.pos as u32);
                self.diagnostics.push(
                    Diagnostic::error(
                        "cove::lex::unterminated_block_comment",
                        "block comment is never closed",
                    )
                    .at(span)
                    .help("Add a matching `*/`."),
                );
                return;
            }
        }
    }

    fn lex_ident(&mut self, start: usize) -> TokenKind {
        while let Some(c) = self.peek_char() {
            if is_ident_continue(c) {
                self.bump();
            } else {
                break;
            }
        }
        let text = &self.text[start..self.pos];
        match text {
            "_" => TokenKind::Underscore,
            "true" => TokenKind::Bool(true),
            "false" => TokenKind::Bool(false),
            _ => match Keyword::from_text(text) {
                Some(keyword) => TokenKind::Keyword(keyword),
                None => TokenKind::Ident(text.to_string()),
            },
        }
    }

    fn lex_number(&mut self, start: usize) -> TokenKind {
        if self.text[start..].starts_with("0x") {
            let digits_start = start + 2;
            if matches!(self.text[digits_start..].chars().next(), Some(c) if c.is_ascii_hexdigit())
            {
                self.pos = digits_start;
                return self.lex_radix_integer(start, 16, |c| c.is_ascii_hexdigit());
            }
        } else if self.text[start..].starts_with("0b") {
            let digits_start = start + 2;
            if matches!(self.text[digits_start..].chars().next(), Some('0' | '1')) {
                self.pos = digits_start;
                return self.lex_radix_integer(start, 2, |c| c == '0' || c == '1');
            }
        }
        self.lex_decimal_number(start)
    }

    fn lex_radix_integer(
        &mut self,
        start: usize,
        radix: u32,
        pred: impl Fn(char) -> bool,
    ) -> TokenKind {
        let digits_start = self.pos;
        while let Some(c) = self.peek_char() {
            if pred(c) || c == '_' {
                self.bump();
            } else {
                break;
            }
        }
        let raw = self.text[digits_start..self.pos].replace('_', "");
        self.finish_integer_or_duration(start, &raw, radix)
    }

    fn lex_decimal_number(&mut self, start: usize) -> TokenKind {
        while let Some(c) = self.peek_char() {
            if c.is_ascii_digit() || c == '_' {
                self.bump();
            } else {
                break;
            }
        }

        let mut is_float = false;

        if self.peek_char() == Some('.')
            && matches!(self.peek_char_at(1), Some(d) if d.is_ascii_digit())
        {
            is_float = true;
            self.bump();
            while let Some(c) = self.peek_char() {
                if c.is_ascii_digit() || c == '_' {
                    self.bump();
                } else {
                    break;
                }
            }
        }

        if matches!(self.peek_char(), Some('e' | 'E')) {
            let mark = self.pos;
            self.bump();
            if matches!(self.peek_char(), Some('+' | '-')) {
                self.bump();
            }
            if matches!(self.peek_char(), Some(d) if d.is_ascii_digit()) {
                is_float = true;
                while let Some(c) = self.peek_char() {
                    if c.is_ascii_digit() || c == '_' {
                        self.bump();
                    } else {
                        break;
                    }
                }
            } else {
                self.pos = mark;
            }
        }

        if is_float {
            let text = self.text[start..self.pos].replace('_', "");
            let value: f64 = text
                .parse()
                .expect("lexer produced a malformed float literal");
            TokenKind::Float(value)
        } else {
            let raw = self.text[start..self.pos].replace('_', "");
            self.finish_integer_or_duration(start, &raw, 10)
        }
    }

    /// Given the digits of an integer literal already scanned (`raw_digits`,
    /// in `radix`), scans an optional duration-unit suffix and produces
    /// either an `Int` or a `Duration` token.
    fn finish_integer_or_duration(
        &mut self,
        start: usize,
        raw_digits: &str,
        radix: u32,
    ) -> TokenKind {
        let suffix_start = self.pos;
        while let Some(c) = self.peek_char() {
            if is_ident_continue(c) {
                self.bump();
            } else {
                break;
            }
        }
        let suffix = self.text[suffix_start..self.pos].to_string();
        let span = Span::new(self.file, start as u32, self.pos as u32);

        if suffix.is_empty() {
            return match i64::from_str_radix(raw_digits, radix) {
                Ok(value) => TokenKind::Int(value),
                Err(_) => {
                    let message = format!(
                        "integer literal `{}` does not fit in a 64-bit integer",
                        &self.text[start..self.pos]
                    );
                    self.diagnostics.push(
                        Diagnostic::error("cove::lex::integer_out_of_range", message)
                            .at(span)
                            .help("Use a smaller value, or split the computation across multiple steps."),
                    );
                    TokenKind::Int(0)
                }
            };
        }

        if let Some(factor) = duration_factor(&suffix) {
            let ns = i64::from_str_radix(raw_digits, radix)
                .ok()
                .and_then(|value| value.checked_mul(factor));
            match ns {
                Some(ns) => TokenKind::Duration(ns),
                None => {
                    let message = format!(
                        "duration literal `{}` overflows a 64-bit nanosecond count",
                        &self.text[start..self.pos]
                    );
                    self.diagnostics.push(
                        Diagnostic::error("cove::lex::duration_out_of_range", message)
                            .at(span)
                            .help("Use a smaller value or a coarser unit."),
                    );
                    TokenKind::Duration(0)
                }
            }
        } else {
            let message = format!("`{suffix}` is not a valid literal suffix");
            self.diagnostics.push(
                Diagnostic::error("cove::lex::invalid_number_suffix", message)
                    .at(span)
                    .help("Valid duration suffixes are `ns`, `us`, `ms`, `s`, `m`, and `h`."),
            );
            TokenKind::Int(0)
        }
    }

    /// Lexes the string literal whose opening `"` or `"""` is at the cursor,
    /// in whichever of the two forms it is, and answers its parts.
    fn lex_any_string(&mut self) -> Option<Vec<StringPart>> {
        let quote_start = self.pos;
        if self.text[self.pos..].starts_with("\"\"\"") {
            self.pos += 3;
            self.lex_triple_string(quote_start)
        } else {
            self.bump();
            self.lex_string(quote_start)
        }
    }

    /// Lexes a string literal, having already consumed its opening `"` at
    /// `quote_start`. Returns `None` (after recording a diagnostic) if the
    /// string or one of its interpolations is never closed.
    fn lex_string(&mut self, quote_start: usize) -> Option<Vec<StringPart>> {
        let mut parts = Vec::new();
        let mut current = String::new();

        loop {
            match self.peek_char() {
                None => {
                    self.unterminated_string(quote_start);
                    return None;
                }
                Some('"') => {
                    self.bump();
                    if !current.is_empty() {
                        parts.push(StringPart::Text(current));
                    }
                    return Some(parts);
                }
                Some('\\') => {
                    let esc_start = self.pos;
                    self.bump();
                    match self.peek_char() {
                        None => {
                            self.unterminated_string(quote_start);
                            return None;
                        }
                        // A `\` ending a line eats the newline and the next
                        // line's leading spaces and tabs, ADR 0100.
                        Some('\n') => {
                            self.bump();
                            self.skip_horizontal_space();
                        }
                        Some('\r') if self.peek_char_at(1) == Some('\n') => {
                            self.bump();
                            self.bump();
                            self.skip_horizontal_space();
                        }
                        Some(escaped) => {
                            self.bump();
                            self.escape(escaped, esc_start, &mut current);
                        }
                    }
                }
                Some('{') => {
                    if !self.interpolation(&mut parts, &mut current) {
                        return None;
                    }
                }
                Some(c) => {
                    self.bump();
                    current.push(c);
                }
            }
        }
    }

    /// Steps over spaces and tabs: the indentation a continuation `\` eats.
    fn skip_horizontal_space(&mut self) {
        while matches!(self.peek_char(), Some(c) if is_horizontal_space(c)) {
            self.bump();
        }
    }

    /// Resolves the escape `\{escaped}` beginning at `esc_start`, both
    /// characters already consumed, onto `current` — or reports it.
    ///
    /// One arm for both string forms, because ADR 0100 says a `"""` literal's
    /// escapes are exactly a `"…"` literal's.
    fn escape(&mut self, escaped: char, esc_start: usize, current: &mut String) {
        if escaped == 'u' {
            if let Some(character) = self.unicode_escape(esc_start) {
                current.push(character);
            }
        } else {
            match escaped_char(escaped) {
                Some(character) => current.push(character),
                None => self.unknown_escape(escaped, esc_start),
            }
        }
    }

    /// Reads the interpolation whose `{` is the next character, pushing the
    /// text before it and then the hole onto `parts`. Answers `false` (after
    /// recording a diagnostic) if the hole is never closed.
    ///
    /// The hole is kept as the source text between its braces, with that
    /// text's own span, and is re-parsed by the parser from there. So a hole
    /// in a `"""` literal is untouched by the margin: what is stripped is the
    /// literal's text, and a hole is not text.
    fn interpolation(&mut self, parts: &mut Vec<StringPart>, current: &mut String) -> bool {
        let brace_start = self.pos;
        self.bump();
        if !current.is_empty() {
            parts.push(StringPart::Text(std::mem::take(current)));
        }
        let interp_start = self.pos;
        match self.skip_interpolation_body() {
            Ok(()) => {
                let interp_end = self.pos - 1;
                let source = self.text[interp_start..interp_end].to_string();
                let span = Span::new(self.file, interp_start as u32, interp_end as u32);
                parts.push(StringPart::Interpolation { source, span });
                true
            }
            Err(()) => {
                self.unterminated_interpolation(brace_start);
                false
            }
        }
    }

    fn unterminated_interpolation(&mut self, brace_start: usize) {
        let span = Span::new(self.file, brace_start as u32, self.pos as u32);
        self.diagnostics.push(
            Diagnostic::error(
                "cove::lex::unterminated_interpolation",
                "string interpolation is never closed",
            )
            .at(span)
            .help("Add a matching `}`."),
        );
    }

    /// Lexes a `"""` literal, having already consumed its opening `"""` at
    /// `quote_start`. [ADR 0100](../../../docs/adr/0100-a-literal-may-be-written-over-lines-and-the-closer-sets-its-margin.md).
    ///
    /// The opener is followed by a newline, which is not content. The closer
    /// stands on a line of its own, and the whitespace in front of it is the
    /// **margin**: it is removed from the front of every line, compared
    /// character for character, and a line that does not begin with it is an
    /// error rather than a guess — except a line of nothing but whitespace,
    /// which is an empty line. The newline before the closer's line is not
    /// content either. Trailing spaces and tabs on a line are dropped, and
    /// then a line ending in an unescaped `\` loses the `\`, its newline, and
    /// the whole of the next line's indentation — past the margin too, though
    /// that line must still begin with the margin.
    /// Inside, `"` is content and everything else is what it is in `"…"`.
    ///
    /// All of it happens here, so the token's parts are already the stripped
    /// text: what [ADR 0045](../../../docs/adr/0045-a-literal-is-there-before-the-program-runs.md)
    /// calls the literal is that, and nothing is computed at run time.
    ///
    /// It reads the literal twice, because the margin is set by the *end* of
    /// it: once to find the closer, and once to strip and resolve the lines in
    /// front of it. Returns `None` (after recording a diagnostic) when there is
    /// no literal to answer — no newline after the opener, no closer, a closer
    /// with something in front of it on its line, or a hole never closed.
    fn lex_triple_string(&mut self, quote_start: usize) -> Option<Vec<StringPart>> {
        let opener_end = self.pos;

        // The opener: optional spaces and tabs, then the line ends.
        while matches!(self.peek_char(), Some(c) if is_horizontal_space(c)) {
            self.bump();
        }
        let opened = match self.peek_char() {
            Some('\n') => {
                self.bump();
                true
            }
            Some('\r') if self.peek_char_at(1) == Some('\n') => {
                self.bump();
                self.bump();
                true
            }
            _ => false,
        };
        if !opened {
            let span = Span::new(self.file, quote_start as u32, opener_end as u32);
            self.diagnostics.push(
                Diagnostic::error(
                    "cove::lex::triple_quote_opener",
                    "a `\"\"\"` literal must begin a new line after its opening `\"\"\"`",
                )
                .at(span)
                .rule(
                    "`\"\"\"` opens a literal only when nothing but spaces follows it on its \
                     line; the text begins on the next line.",
                )
                .help("Move the text to the next line, or write a one-line literal with `\"`."),
            );
            // Step over the rest as far as a closer, so that one mistake is
            // one diagnostic rather than the rest of the file misread.
            self.pos = opener_end;
            if let TripleEnd::Closer(closer) = self.find_triple_closer() {
                self.pos = closer + 3;
            } else {
                self.pos = self.text.len();
            }
            return None;
        }
        let content_start = self.pos;

        let closer = match self.find_triple_closer() {
            TripleEnd::Closer(closer) => closer,
            TripleEnd::Eof => {
                self.unterminated_triple_string(quote_start);
                return None;
            }
            TripleEnd::OpenHole(brace_start) => {
                self.unterminated_interpolation(brace_start);
                return None;
            }
        };
        // The closer's line begins after the last newline in front of it,
        // which is at worst the opener's own.
        let line_start = self.text[..closer].rfind('\n').map_or(0, |n| n + 1);
        let margin = &self.text[line_start..closer];
        if !margin.chars().all(is_horizontal_space) {
            self.pos = closer + 3;
            let span = Span::new(self.file, closer as u32, self.pos as u32);
            self.diagnostics.push(
                Diagnostic::error(
                    "cove::lex::triple_quote_closer",
                    "a `\"\"\"` literal's closing `\"\"\"` must stand on a line of its own",
                )
                .at(span)
                .rule(
                    "The closing `\"\"\"` has only spaces in front of it on its line, and \
                     they are the margin taken off every line of the literal.",
                )
                .help(
                    "Put the closing `\"\"\"` on the next line, or write a `\"` inside the \
                     literal as `\\\"`.",
                ),
            );
            return None;
        }
        // The newline in front of the closer's line is not content. When the
        // closer is on the line after the opener there is no content at all.
        let content_end = if line_start <= content_start {
            content_start
        } else if self.text[..line_start - 1].ends_with('\r') {
            line_start - 2
        } else {
            line_start - 1
        };

        self.pos = content_start;
        let mut parts = Vec::new();
        let mut current = String::new();
        // Spaces and tabs read but not yet known not to be trailing.
        let mut spaces = String::new();
        let mut at_line_start = true;
        // Whether the line about to start follows a continuation `\`, and so
        // loses all of its indentation rather than only the margin.
        let mut continued = false;
        while self.pos < content_end {
            if at_line_start {
                at_line_start = false;
                let continued = std::mem::take(&mut continued);
                let line_begin = self.pos;
                let rest = &self.text[line_begin..content_end];
                let indent_len = rest.len() - rest.trim_start_matches(is_horizontal_space).len();
                let indent_end = line_begin + indent_len;
                let blank = indent_end == content_end
                    || self.text[indent_end..].starts_with('\n')
                    || self.text[indent_end..].starts_with("\r\n");
                if blank {
                    self.pos = indent_end;
                } else if self.text[line_begin..indent_end].starts_with(margin) {
                    self.pos = if continued {
                        indent_end
                    } else {
                        line_begin + margin.len()
                    };
                } else {
                    self.pos = indent_end;
                    self.literal_margin(line_begin, indent_end, margin);
                }
                continue;
            }
            let Some(c) = self.peek_char() else { break };
            match c {
                '\n' => {
                    self.bump();
                    spaces.clear();
                    current.push('\n');
                    at_line_start = true;
                }
                '\r' if self.peek_char_at(1) == Some('\n') => {
                    self.bump();
                    self.bump();
                    spaces.clear();
                    current.push_str("\r\n");
                    at_line_start = true;
                }
                c if is_horizontal_space(c) => {
                    self.bump();
                    spaces.push(c);
                }
                '\\' => {
                    current.push_str(&spaces);
                    spaces.clear();
                    let esc_start = self.pos;
                    self.bump();
                    // A `\` that ends the line, trailing spaces aside, eats
                    // the newline and the whole of the next line's
                    // indentation, which must still begin with the margin.
                    let rest = &self.text[self.pos..content_end];
                    let after =
                        self.pos + rest.len() - rest.trim_start_matches(is_horizontal_space).len();
                    if after == content_end || self.text[after..].starts_with('\n') {
                        self.pos = (after + 1).min(content_end);
                        at_line_start = true;
                        continued = true;
                    } else if self.text[after..].starts_with("\r\n") {
                        self.pos = (after + 2).min(content_end);
                        at_line_start = true;
                        continued = true;
                    } else if let Some(escaped) = self.bump() {
                        self.escape(escaped, esc_start, &mut current);
                    }
                }
                '{' => {
                    current.push_str(&spaces);
                    spaces.clear();
                    if !self.interpolation(&mut parts, &mut current) {
                        return None;
                    }
                }
                c => {
                    self.bump();
                    current.push_str(&spaces);
                    spaces.clear();
                    current.push(c);
                }
            }
        }
        self.pos = closer + 3;
        if !current.is_empty() {
            parts.push(StringPart::Text(current));
        }
        Some(parts)
    }

    /// Scans forward from the cursor, which is inside a `"""` literal, to the
    /// first unescaped `"""` outside an interpolation, and answers where it
    /// is. The cursor is left wherever the scan stopped.
    fn find_triple_closer(&mut self) -> TripleEnd {
        loop {
            match self.peek_char() {
                None => return TripleEnd::Eof,
                Some('"') if self.text[self.pos..].starts_with("\"\"\"") => {
                    return TripleEnd::Closer(self.pos);
                }
                Some('\\') => {
                    self.bump();
                    // A `\u{...}` escape's brace opens no interpolation.
                    if self.bump() == Some('u') && self.peek_char() == Some('{') {
                        self.bump();
                        while matches!(self.peek_char(), Some(c) if c.is_ascii_alphanumeric()) {
                            self.bump();
                        }
                        if self.peek_char() == Some('}') {
                            self.bump();
                        }
                    }
                }
                Some('{') => {
                    let brace_start = self.pos;
                    self.bump();
                    if self.skip_interpolation_body().is_err() {
                        return TripleEnd::OpenHole(brace_start);
                    }
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    /// Reports a line of a `"""` literal, from `line_begin` to the end of its
    /// indentation at `indent_end`, that does not begin with `margin`.
    fn literal_margin(&mut self, line_begin: usize, indent_end: usize, margin: &str) {
        let span = Span::new(self.file, line_begin as u32, indent_end as u32);
        let shown = margin.replace('\t', "\\t");
        self.diagnostics.push(
            Diagnostic::error(
                "cove::lex::literal_margin",
                "this line of a `\"\"\"` literal does not begin with the literal's margin",
            )
            .at(span)
            .rule(
                "The whitespace in front of the closing `\"\"\"` is the margin, and every \
                 line of the literal that is not blank begins with exactly that whitespace, \
                 character for character.",
            )
            .help(format!(
                "Indent this line at least as far as the closing `\"\"\"`, with the same \
                 characters (`{shown}`), or move the closing `\"\"\"` left."
            )),
        );
    }

    fn unterminated_triple_string(&mut self, quote_start: usize) {
        let span = Span::new(self.file, quote_start as u32, self.pos as u32);
        self.diagnostics.push(
            Diagnostic::error(
                "cove::lex::unterminated_string",
                "string literal is never closed",
            )
            .at(span)
            .help("Add a closing `\"\"\"` on a line of its own."),
        );
    }

    /// A code-point literal: `'a'`, whose value is a Unicode scalar value and
    /// whose type is `Int`.
    ///
    /// There is no `Char` type and this does not add one —
    /// [ADR 0046](../../../docs/adr/0046-a-byte-offset-is-a-value-a-string-hands-out.md)
    /// decided a code point is an `Int`, and this is a literal for one rather
    /// than a type for one. It answers [`TokenKind::Int`], so nothing past
    /// this function knows the form exists, which is the whole of what it
    /// costs the rest of the compiler.
    ///
    /// A literal holding no scalar or more than one is reported and then
    /// answers something anyway, so that one bad literal does not cascade
    /// into every expression that reads it.
    fn lex_code_point(&mut self, quote_start: usize) -> Option<TokenKind> {
        let mut scalars = String::new();
        loop {
            match self.peek_char() {
                None => {
                    let span = Span::new(self.file, quote_start as u32, self.pos as u32);
                    self.diagnostics.push(
                        Diagnostic::error(
                            "cove::lex::unterminated_code_point",
                            "code-point literal is never closed",
                        )
                        .at(span)
                        .help("Add a closing `'`."),
                    );
                    return None;
                }
                Some('\'') => {
                    self.bump();
                    let span = Span::new(self.file, quote_start as u32, self.pos as u32);
                    let mut characters = scalars.chars();
                    let Some(first) = characters.next() else {
                        self.diagnostics.push(
                            Diagnostic::error(
                                "cove::lex::empty_code_point",
                                "code-point literal names no character",
                            )
                            .at(span)
                            .rule("A code-point literal holds exactly one Unicode scalar value.")
                            .help("Write the character between the quotes, as `'a'`. The empty string is `\"\"`."),
                        );
                        return Some(TokenKind::Int(0));
                    };
                    if characters.next().is_some() {
                        let count = scalars.chars().count();
                        self.diagnostics.push(
                            Diagnostic::error(
                                "cove::lex::code_point_is_not_one_scalar",
                                format!("code-point literal holds {count} scalar values, not one"),
                            )
                            .at(span)
                            .rule(
                                "A code-point literal holds exactly one Unicode scalar value, so a \
                                 combining pair or an emoji sequence is more than one and is not a \
                                 code point.",
                            )
                            .help("Write one scalar, or use a `\"...\"` string for text of any length."),
                        );
                    }
                    return Some(TokenKind::Int(first as i64));
                }
                Some('\\') => {
                    let esc_start = self.pos;
                    self.bump();
                    match self.peek_char() {
                        None => {
                            let span = Span::new(self.file, quote_start as u32, self.pos as u32);
                            self.diagnostics.push(
                                Diagnostic::error(
                                    "cove::lex::unterminated_code_point",
                                    "code-point literal is never closed",
                                )
                                .at(span)
                                .help("Add a closing `'`."),
                            );
                            return None;
                        }
                        Some(escaped) => {
                            self.bump();
                            if escaped == 'u' {
                                // A refused escape still stands for one
                                // scalar, so `'\u{D800}'` is one complaint and
                                // not a second one that the literal is empty.
                                let character = self.unicode_escape(esc_start);
                                scalars.push(character.unwrap_or(char::REPLACEMENT_CHARACTER));
                            } else {
                                match escaped_char(escaped) {
                                    Some(character) => scalars.push(character),
                                    None => self.unknown_escape(escaped, esc_start),
                                }
                            }
                        }
                    }
                }
                Some(c) => {
                    self.bump();
                    scalars.push(c);
                }
            }
        }
    }

    /// Reads the rest of a `\u{X…}` escape, the `\u` at `esc_start` having
    /// been consumed, and answers the scalar it names — or reports why it
    /// names none and answers `None`.
    ///
    /// One to six hex digits, either case, naming a Unicode scalar value, so
    /// a surrogate (`D800` to `DFFF`) and anything above `10FFFF` are refused,
    /// as are no digits, a seventh digit, a character that is not a hex digit,
    /// and a missing brace. It is Rust's and Swift's spelling. ADR 0094.
    ///
    /// Letters and digits after the `{` are consumed up to the `}`, so
    /// `\u{12G4}` is one refusal that names the `G` and not a refusal followed
    /// by `G4}` read as text. Inside a string a `{` opens an interpolation;
    /// this consumes the escape's own `{` before the string's loop can see it,
    /// so `"\u{41}"` is the text `A` and never an interpolation of `41`.
    fn unicode_escape(&mut self, esc_start: usize) -> Option<char> {
        if self.peek_char() != Some('{') {
            self.invalid_unicode_escape(esc_start, "`\\u` must be followed by `{`".to_string());
            return None;
        }
        self.bump();
        let digits_start = self.pos;
        while matches!(self.peek_char(), Some(c) if c.is_ascii_alphanumeric()) {
            self.bump();
        }
        let digits = &self.text[digits_start..self.pos];
        if self.peek_char() != Some('}') {
            self.invalid_unicode_escape(esc_start, "`\\u{` is never closed with `}`".to_string());
            return None;
        }
        self.bump();
        if let Some(bad) = digits.chars().find(|c| !c.is_ascii_hexdigit()) {
            self.invalid_unicode_escape(esc_start, format!("`{bad}` is not a hex digit"));
            return None;
        }
        if digits.is_empty() {
            self.invalid_unicode_escape(esc_start, "`\\u{}` names no code point".to_string());
            return None;
        }
        if digits.len() > 6 {
            let count = digits.len();
            self.invalid_unicode_escape(
                esc_start,
                format!("`\\u{{{digits}}}` has {count} hex digits, more than six"),
            );
            return None;
        }
        // Six hex digits at most, so the parse cannot fail.
        let value = u32::from_str_radix(digits, 16).unwrap_or(u32::MAX);
        if (0xD800..=0xDFFF).contains(&value) {
            self.invalid_unicode_escape(
                esc_start,
                format!("`\\u{{{digits}}}` is a surrogate, not a Unicode scalar value"),
            );
            return None;
        }
        let character = char::from_u32(value);
        if character.is_none() {
            self.invalid_unicode_escape(
                esc_start,
                format!("`\\u{{{digits}}}` is above `10FFFF`, the last code point"),
            );
        }
        character
    }

    /// Reports a `\u` escape that names no scalar, over as much of the escape
    /// as was read.
    fn invalid_unicode_escape(&mut self, esc_start: usize, message: String) {
        let span = Span::new(self.file, esc_start as u32, self.pos as u32);
        self.diagnostics.push(
            Diagnostic::error(
                "cove::lex::invalid_unicode_escape",
                format!("invalid unicode escape: {message}"),
            )
            .at(span)
            .rule(
                "`\\u{...}` holds one to six hex digits naming a Unicode scalar value: `0` to \
                 `10FFFF`, the surrogates `D800` to `DFFF` excluded.",
            )
            .help("Write the code point in hex between braces, as `\\u{e9}` or `\\u{1F600}`."),
        );
    }

    /// Reports an escape neither literal form spells anything with.
    fn unknown_escape(&mut self, escaped: char, esc_start: usize) {
        let span = Span::new(self.file, esc_start as u32, self.pos as u32);
        let message = format!("unknown escape sequence `\\{escaped}`");
        self.diagnostics.push(
            Diagnostic::error("cove::lex::unknown_escape", message)
                .at(span)
                .help(
                    "Use one of the supported escapes: \\\\, \\\", \\', \\n, \\t, \\r, \\0, \\{, \\}, \\u{...}, \
                     or a \\ that ends its line.",
                ),
        );
    }

    fn unterminated_string(&mut self, quote_start: usize) {
        let span = Span::new(self.file, quote_start as u32, self.pos as u32);
        self.diagnostics.push(
            Diagnostic::error(
                "cove::lex::unterminated_string",
                "string literal is never closed",
            )
            .at(span)
            .help("Add a closing `\"`."),
        );
    }

    /// Consumes source text up to and including the `}` matching the `{`
    /// that was just consumed by the caller, stepping over any nested `{ }`
    /// blocks and any nested string literals, so that a `}` inside a nested
    /// string does not end the interpolation early.
    ///
    /// A string may hold an interpolation, an interpolation may hold a
    /// string, and either may hold more of itself, so what is stepped over is
    /// a nesting. It is held in a `Vec` rather than in the call stack on
    /// purpose. This runs before the parser, and therefore before
    /// [`crate::parser`]'s nesting limit can refuse anything, so a recursive
    /// scan here would let a string literal of nothing but `{` end the
    /// process — which is the failure that limit exists to prevent. What this
    /// spends instead is one byte of heap per unclosed level, bounded by the
    /// length of the file.
    fn skip_interpolation_body(&mut self) -> Result<(), ()> {
        let mut unclosed = vec![Unclosed::Brace];
        while let Some(innermost) = unclosed.last() {
            // A `'...'` is stepped over whole, the way a `"..."` is, and for
            // one reason more: inside it a brace is a *character*. Without
            // that, `"{ head == '\{' }"` ends the interpolation at a brace
            // the program meant as text. A `{` inside a `"..."` is not the
            // same case — a nested string may itself interpolate — which is
            // why only the apostrophe suppresses it.
            let inside_scalar = matches!(innermost, Unclosed::Apostrophe);
            let escaping = matches!(
                innermost,
                Unclosed::Quote | Unclosed::TripleQuote | Unclosed::Apostrophe
            );
            let in_text = matches!(innermost, Unclosed::Quote | Unclosed::TripleQuote);
            match self.peek_char() {
                None => return Err(()),
                Some('\\') if escaping => {
                    self.bump();
                    if self.peek_char().is_none() {
                        return Err(());
                    }
                    let escaped = self.bump();
                    // A `\u{...}` escape's brace is its own and opens no
                    // interpolation, read as far as `unicode_escape` reads it.
                    if escaped == Some('u') && self.peek_char() == Some('{') {
                        self.bump();
                        while matches!(self.peek_char(), Some(c) if c.is_ascii_alphanumeric()) {
                            self.bump();
                        }
                        if self.peek_char() == Some('}') {
                            self.bump();
                        }
                    }
                }
                // A `"""` in code opens a literal in which a lone `"` is
                // text, and only another `"""` closes it, ADR 0100.
                Some('"')
                    if matches!(innermost, Unclosed::Brace)
                        && self.text[self.pos..].starts_with("\"\"\"") =>
                {
                    self.pos += 3;
                    unclosed.push(Unclosed::TripleQuote);
                }
                Some('"') if matches!(innermost, Unclosed::TripleQuote) => {
                    if self.text[self.pos..].starts_with("\"\"\"") {
                        self.pos += 3;
                        unclosed.pop();
                    } else {
                        self.bump();
                    }
                }
                Some('"') if !inside_scalar => {
                    self.bump();
                    if matches!(innermost, Unclosed::Quote) {
                        unclosed.pop();
                    } else {
                        unclosed.push(Unclosed::Quote);
                    }
                }
                // An apostrophe inside a string is ordinary text, so only a
                // brace's body opens one and only its own closes it.
                Some('\'') if !in_text => {
                    self.bump();
                    if inside_scalar {
                        unclosed.pop();
                    } else {
                        unclosed.push(Unclosed::Apostrophe);
                    }
                }
                Some('{') if !inside_scalar => {
                    self.bump();
                    unclosed.push(Unclosed::Brace);
                }
                Some('}') if matches!(innermost, Unclosed::Brace) => {
                    self.bump();
                    unclosed.pop();
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
        Ok(())
    }

    /// Matches punctuation and operators, longest match first. `/` and
    /// everything that can start with it (comments, `/=`) is handled
    /// separately by [`Lexer::handle_slash`].
    fn lex_operator(&mut self) -> Option<TokenKind> {
        let rest = &self.text[self.pos..];

        macro_rules! op {
            ($lit:literal, $kind:expr) => {
                if rest.starts_with($lit) {
                    self.pos += $lit.len();
                    return Some($kind);
                }
            };
        }

        op!("...", TokenKind::Ellipsis);
        op!("..<", TokenKind::DotDotLt);
        op!("..", TokenKind::DotDot);
        op!(".", TokenKind::Dot);
        op!("->", TokenKind::Arrow);
        op!("=>", TokenKind::FatArrow);
        op!("==", TokenKind::EqEq);
        op!("!=", TokenKind::BangEq);
        op!("<=", TokenKind::LtEq);
        op!(">=", TokenKind::GtEq);
        op!("+=", TokenKind::PlusEq);
        op!("-=", TokenKind::MinusEq);
        op!("*=", TokenKind::StarEq);
        op!("%=", TokenKind::PercentEq);
        op!("&&", TokenKind::AmpAmp);
        op!("||", TokenKind::PipePipe);
        op!("=", TokenKind::Eq);
        op!("!", TokenKind::Bang);
        op!("<", TokenKind::Lt);
        op!(">", TokenKind::Gt);
        op!("+", TokenKind::Plus);
        op!("-", TokenKind::Minus);
        op!("*", TokenKind::Star);
        op!("%", TokenKind::Percent);
        op!("?", TokenKind::Question);
        op!(",", TokenKind::Comma);
        op!(":", TokenKind::Colon);
        op!("(", TokenKind::LParen);
        op!(")", TokenKind::RParen);
        op!("{", TokenKind::LBrace);
        op!("}", TokenKind::RBrace);
        op!("[", TokenKind::LBracket);
        op!("]", TokenKind::RBracket);

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex_ok(src: &str) -> Vec<Token> {
        let mut sources = SourceMap::new();
        let file = sources.add("test.cove", src);
        lex(&sources, file).unwrap_or_else(|diags| {
            panic!("expected `{src}` to lex successfully, got errors: {diags:?}")
        })
    }

    fn lex_err(src: &str) -> Vec<Diagnostic> {
        let mut sources = SourceMap::new();
        let file = sources.add("test.cove", src);
        match lex(&sources, file) {
            Ok(tokens) => panic!("expected `{src}` to fail to lex, got tokens: {tokens:?}"),
            Err(diags) => diags,
        }
    }

    fn kinds(src: &str) -> Vec<TokenKind> {
        let tokens = lex_ok(src);
        assert!(
            matches!(tokens.last().unwrap().kind, TokenKind::Eof),
            "token stream must end with Eof, got {tokens:?}"
        );
        tokens[..tokens.len() - 1]
            .iter()
            .map(|t| t.kind.clone())
            .collect()
    }

    #[test]
    fn empty_file_is_just_eof() {
        let tokens = lex_ok("");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, TokenKind::Eof);
        assert_eq!(tokens[0].span.start, 0);
        assert_eq!(tokens[0].span.end, 0);
    }

    #[test]
    fn whitespace_produces_no_tokens() {
        assert_eq!(kinds(" \t\r\n foo"), vec![TokenKind::Ident("foo".into())]);
    }

    /// A line break is recorded on the token that follows it rather than
    /// becoming a token of its own, and comments do not hide it.
    #[test]
    fn tokens_record_a_preceding_line_break() {
        let tokens = lex_ok("a b\nc /* x\ny */ d // e\nf");
        let flags: Vec<bool> = tokens.iter().map(|t| t.preceded_by_newline).collect();
        // a, b, c, d, f, Eof
        assert_eq!(flags, vec![false, false, true, true, true, false]);
    }

    #[test]
    fn a_block_comment_on_one_line_is_not_a_line_break() {
        let tokens = lex_ok("a /* x */ b");
        assert!(!tokens[1].preceded_by_newline);
    }

    #[test]
    fn keywords_vs_identifiers() {
        assert_eq!(
            kinds("fn foo let bar self"),
            vec![
                TokenKind::Keyword(Keyword::Fn),
                TokenKind::Ident("foo".into()),
                TokenKind::Keyword(Keyword::Let),
                TokenKind::Ident("bar".into()),
                TokenKind::Keyword(Keyword::SelfValue),
            ]
        );
        // Maximal munch: `forever` is one identifier, not `for` + `ever`.
        assert_eq!(kinds("forever"), vec![TokenKind::Ident("forever".into())]);
        assert_eq!(
            kinds("true false"),
            vec![TokenKind::Bool(true), TokenKind::Bool(false)]
        );
    }

    #[test]
    fn is_is_a_keyword_and_maximal_munch_still_applies() {
        assert_eq!(
            kinds("a is b"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Keyword(Keyword::Is),
                TokenKind::Ident("b".into()),
            ]
        );
        // `island` is one identifier, not `is` + `land`.
        assert_eq!(kinds("island"), vec![TokenKind::Ident("island".into())]);
    }

    #[test]
    fn underscore_is_its_own_token() {
        assert_eq!(kinds("_"), vec![TokenKind::Underscore]);
        assert_eq!(kinds("_foo"), vec![TokenKind::Ident("_foo".into())]);
        assert_eq!(kinds("foo_"), vec![TokenKind::Ident("foo_".into())]);
    }

    #[test]
    fn doc_comments_strip_one_leading_space_and_trailing_whitespace() {
        assert_eq!(
            kinds("/// hello\n/// world"),
            vec![
                TokenKind::DocComment("hello".into()),
                TokenKind::DocComment("world".into()),
            ]
        );
        assert_eq!(
            kinds("///no-space"),
            vec![TokenKind::DocComment("no-space".into())]
        );
        assert_eq!(
            kinds("///  two spaces"),
            vec![TokenKind::DocComment(" two spaces".into())]
        );
        assert_eq!(
            kinds("///trailing   \nfn"),
            vec![
                TokenKind::DocComment("trailing".into()),
                TokenKind::Keyword(Keyword::Fn)
            ]
        );
    }

    #[test]
    fn four_slashes_is_a_plain_comment_not_a_doc_comment() {
        assert_eq!(kinds("//// not a doc\n42"), vec![TokenKind::Int(42)]);
    }

    #[test]
    fn line_comments_are_discarded() {
        assert_eq!(kinds("// hello\n42"), vec![TokenKind::Int(42)]);
    }

    #[test]
    fn nested_block_comments_are_discarded() {
        assert_eq!(
            kinds("/* outer /* inner */ still outer */ 42"),
            vec![TokenKind::Int(42)]
        );
    }

    #[test]
    fn unterminated_block_comment_is_an_error() {
        let diags = lex_err("/* never closed");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "cove::lex::unterminated_block_comment");
    }

    #[test]
    fn decimal_integers_and_underscores() {
        assert_eq!(kinds("123"), vec![TokenKind::Int(123)]);
        assert_eq!(kinds("1_000"), vec![TokenKind::Int(1000)]);
        assert_eq!(kinds("0"), vec![TokenKind::Int(0)]);
    }

    #[test]
    fn hex_and_binary_integers() {
        assert_eq!(kinds("0xFF"), vec![TokenKind::Int(255)]);
        assert_eq!(kinds("0b1010"), vec![TokenKind::Int(10)]);
    }

    #[test]
    fn floats() {
        assert_eq!(kinds("1.5"), vec![TokenKind::Float(1.5)]);
        assert_eq!(kinds("1.5e10"), vec![TokenKind::Float(1.5e10)]);
        assert_eq!(kinds("1e-3"), vec![TokenKind::Float(1e-3)]);
    }

    #[test]
    fn range_dot_is_only_part_of_a_float_before_a_digit() {
        assert_eq!(
            kinds("0..<10"),
            vec![TokenKind::Int(0), TokenKind::DotDotLt, TokenKind::Int(10)]
        );
        assert_eq!(
            kinds("0..n"),
            vec![
                TokenKind::Int(0),
                TokenKind::DotDot,
                TokenKind::Ident("n".into())
            ]
        );
    }

    #[test]
    fn every_duration_unit() {
        assert_eq!(kinds("1ns"), vec![TokenKind::Duration(1)]);
        assert_eq!(kinds("1us"), vec![TokenKind::Duration(1_000)]);
        assert_eq!(kinds("1ms"), vec![TokenKind::Duration(1_000_000)]);
        assert_eq!(kinds("1s"), vec![TokenKind::Duration(1_000_000_000)]);
        assert_eq!(kinds("1m"), vec![TokenKind::Duration(60_000_000_000)]);
        assert_eq!(kinds("1h"), vec![TokenKind::Duration(3_600_000_000_000)]);
        assert_eq!(kinds("500ms"), vec![TokenKind::Duration(500_000_000)]);
        assert_eq!(kinds("60s"), vec![TokenKind::Duration(60_000_000_000)]);
        assert_eq!(kinds("5s"), vec![TokenKind::Duration(5_000_000_000)]);
    }

    #[test]
    fn integer_out_of_range_is_an_error() {
        let diags = lex_err("99999999999999999999");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "cove::lex::integer_out_of_range");
    }

    #[test]
    fn duration_out_of_range_is_an_error() {
        let diags = lex_err("9999999999h");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "cove::lex::duration_out_of_range");
    }

    #[test]
    fn invalid_number_suffix_is_an_error() {
        let diags = lex_err("5sx");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "cove::lex::invalid_number_suffix");
    }

    #[test]
    fn string_escapes() {
        let tokens = kinds(r#""\\ \" \n \t \r \0 \{ \}""#);
        assert_eq!(
            tokens,
            vec![TokenKind::Str(vec![StringPart::Text(
                "\\ \" \n \t \r \0 { }".into()
            )])]
        );
    }

    /// A code-point literal is an `Int` token and nothing else.
    ///
    /// There is no `Char`, so what the lexer answers here is the same token
    /// `97` answers, and every test below is written against that fact
    /// rather than against a form the rest of the compiler would have to
    /// know about.
    #[test]
    fn code_point_literals_are_int_tokens() {
        assert_eq!(kinds("'a'"), vec![TokenKind::Int(97)]);
        assert_eq!(kinds("'0'"), vec![TokenKind::Int(48)]);
        assert_eq!(kinds("' '"), vec![TokenKind::Int(32)]);
        assert_eq!(kinds("'{'"), vec![TokenKind::Int(123)]);
        assert_eq!(kinds("'\u{e9}'"), vec![TokenKind::Int(233)]);
        assert_eq!(kinds("'\u{3042}'"), vec![TokenKind::Int(12354)]);
        assert_eq!(kinds("'\u{1F600}'"), vec![TokenKind::Int(128512)]);
    }

    /// The escapes are the string's, and `\'` besides.
    #[test]
    fn code_point_literals_take_the_escapes_a_string_takes() {
        assert_eq!(kinds(r"'\n'"), vec![TokenKind::Int(10)]);
        assert_eq!(kinds(r"'\t'"), vec![TokenKind::Int(9)]);
        assert_eq!(kinds(r"'\r'"), vec![TokenKind::Int(13)]);
        assert_eq!(kinds(r"'\0'"), vec![TokenKind::Int(0)]);
        assert_eq!(kinds(r"'\\'"), vec![TokenKind::Int(92)]);
        assert_eq!(kinds(r"'\''"), vec![TokenKind::Int(39)]);
        assert_eq!(kinds(r#"'\"'"#), vec![TokenKind::Int(34)]);
        assert_eq!(kinds(r"'\{'"), vec![TokenKind::Int(123)]);
        assert_eq!(kinds(r"'\}'"), vec![TokenKind::Int(125)]);
    }

    /// `\'` is legal in a string too, because there is one escape table and
    /// the apostrophe is in it. It spells the apostrophe it always did.
    #[test]
    fn a_string_may_escape_an_apostrophe() {
        assert_eq!(
            kinds(r#""\'""#),
            vec![TokenKind::Str(vec![StringPart::Text("'".to_string())])]
        );
    }

    #[test]
    fn an_empty_code_point_literal_is_an_error() {
        let diags = lex_err("''");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "cove::lex::empty_code_point");
    }

    /// Two scalars is an error, and so is a grapheme cluster that is written
    /// as one character and is not one scalar — which is the case the rule
    /// exists for.
    #[test]
    fn a_code_point_literal_holding_more_than_one_scalar_is_an_error() {
        let diags = lex_err("'ab'");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "cove::lex::code_point_is_not_one_scalar");
        assert_eq!(
            diags[0].message,
            "code-point literal holds 2 scalar values, not one"
        );

        let diags = lex_err("'\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F466}'");
        assert_eq!(diags.len(), 1);
        assert_eq!(
            diags[0].message,
            "code-point literal holds 5 scalar values, not one"
        );
    }

    #[test]
    fn an_unterminated_code_point_literal_is_an_error() {
        let diags = lex_err("'a");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "cove::lex::unterminated_code_point");
    }

    /// An interpolation is scanned for its closing brace before it is parsed,
    /// and a brace inside a code-point literal is a character rather than a
    /// nesting. Without that, this string ends at the wrong place.
    #[test]
    fn a_code_point_literal_inside_an_interpolation_may_hold_a_brace() {
        let tokens = kinds(r#""{ head == '{' }""#);
        assert_eq!(tokens.len(), 1);
        let TokenKind::Str(parts) = &tokens[0] else {
            panic!("expected a string token, got {tokens:?}");
        };
        assert_eq!(parts.len(), 1);
        let StringPart::Interpolation { source, .. } = &parts[0] else {
            panic!("expected one interpolation, got {parts:?}");
        };
        assert_eq!(source.trim(), "head == '{'");
    }

    fn text_of(src: &str) -> String {
        match kinds(src).as_slice() {
            [TokenKind::Str(parts)] => match parts.as_slice() {
                [StringPart::Text(text)] => text.clone(),
                [] => String::new(),
                other => panic!("expected one text part in `{src}`, got {other:?}"),
            },
            other => panic!("expected one string token in `{src}`, got {other:?}"),
        }
    }

    /// `\u{X…}` names a scalar by number, in one to six hex digits of either
    /// case, from `0` to `10FFFF`. ADR 0094.
    #[test]
    fn a_unicode_escape_names_a_scalar_by_number() {
        assert_eq!(text_of(r#""\u{41}""#), "A");
        assert_eq!(text_of(r#""\u{e9}""#), "\u{e9}");
        assert_eq!(text_of(r#""\u{E9}""#), "\u{e9}");
        assert_eq!(text_of(r#""\u{1F600}""#), "\u{1F600}");
        assert_eq!(text_of(r#""\u{10FFFF}""#), "\u{10FFFF}");
        assert_eq!(text_of(r#""\u{0}""#), "\0");
        assert_eq!(text_of(r#""\u{000041}""#), "A");
        assert_eq!(text_of(r#""caf\u{e9} \u{3042}!""#), "caf\u{e9} \u{3042}!");
    }

    /// The escape's braces are its own and not an interpolation's, and a
    /// brace right after one still opens an interpolation.
    #[test]
    fn a_unicode_escape_opens_no_interpolation() {
        let tokens = kinds(r#""\u{41}{b}""#);
        let [TokenKind::Str(parts)] = tokens.as_slice() else {
            panic!("expected one string token, got {tokens:?}");
        };
        let [StringPart::Text(text), StringPart::Interpolation { source, .. }] = parts.as_slice()
        else {
            panic!("expected text then an interpolation, got {parts:?}");
        };
        assert_eq!((text.as_str(), source.as_str()), ("A", "b"));
        // Inside an interpolation, a nested string's escape is balanced and
        // the interpolation ends at its own brace.
        let tokens = kinds(r#""{ f("\u{7D}") }""#);
        let [TokenKind::Str(parts)] = tokens.as_slice() else {
            panic!("expected one string token, got {tokens:?}");
        };
        let [StringPart::Interpolation { source, .. }] = parts.as_slice() else {
            panic!("expected one interpolation, got {parts:?}");
        };
        assert_eq!(source.trim(), r#"f("\u{7D}")"#);
        // And one never closed opens nothing either: the interpolation still
        // ends at its own brace, leaving the escape for the lexing of its
        // source to report.
        let tokens = kinds(r#""{ f("\u{41") }""#);
        let [TokenKind::Str(parts)] = tokens.as_slice() else {
            panic!("expected one string token, got {tokens:?}");
        };
        let [StringPart::Interpolation { source, .. }] = parts.as_slice() else {
            panic!("expected one interpolation, got {parts:?}");
        };
        assert_eq!(source.trim(), r#"f("\u{41")"#);
    }

    /// A code-point literal takes the escape a string takes.
    #[test]
    fn a_code_point_literal_takes_a_unicode_escape() {
        assert_eq!(kinds(r"'\u{41}'"), vec![TokenKind::Int(65)]);
        assert_eq!(kinds(r"'\u{e9}'"), vec![TokenKind::Int(233)]);
        assert_eq!(kinds(r"'\u{1F600}'"), vec![TokenKind::Int(128512)]);
        assert_eq!(kinds(r"'\u{10FFFF}'"), vec![TokenKind::Int(0x10FFFF)]);
        assert_eq!(kinds(r"'\u{0}'"), vec![TokenKind::Int(0)]);
    }

    /// Every refusal is one diagnostic, spanning the escape as far as it was
    /// read, whose message says which rule it broke.
    #[test]
    fn a_unicode_escape_that_names_no_scalar_is_refused() {
        let cases: &[(&str, &str, &str)] = &[
            (r#""\u{D800}""#, r"\u{D800}", "is a surrogate"),
            (r#""\u{dfff}""#, r"\u{dfff}", "is a surrogate"),
            (r#""\u{110000}""#, r"\u{110000}", "is above `10FFFF`"),
            (r#""\u{}""#, r"\u{}", "names no code point"),
            (r#""\u{1234567}""#, r"\u{1234567}", "has 7 hex digits"),
            (r#""\u{12G4}""#, r"\u{12G4}", "`G` is not a hex digit"),
            (r#""\u41""#, r"\u", "must be followed by `{`"),
            (r#""\u{41""#, r"\u{41", "is never closed"),
            (r#""\u{41 }""#, r"\u{41", "is never closed"),
            (r"'\u{D800}'", r"\u{D800}", "is a surrogate"),
            (r"'\u{}'", r"\u{}", "names no code point"),
        ];
        for (src, escape, says) in cases {
            let diags = lex_err(src);
            assert_eq!(diags.len(), 1, "`{src}`: {diags:?}");
            let diag = &diags[0];
            assert_eq!(diag.code, "cove::lex::invalid_unicode_escape", "`{src}`");
            assert!(diag.message.contains(says), "`{src}`: {}", diag.message);
            let span = diag.primary.expect("the escape is pointed at");
            assert_eq!(&src[span.start as usize..span.end as usize], *escape);
        }
    }

    #[test]
    fn unknown_escape_is_an_error() {
        let diags = lex_err(r#""\q""#);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "cove::lex::unknown_escape");
    }

    #[test]
    fn unterminated_string_is_an_error() {
        let diags = lex_err("\"never closed");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "cove::lex::unterminated_string");
    }

    // ADR 0100: `"""` literals, and `\` ending a line.

    /// The one error `src` lexes to, by code.
    fn only_error(src: &str) -> String {
        let diags = lex_err(src);
        assert_eq!(
            diags.len(),
            1,
            "expected one error from `{src}`, got {diags:?}"
        );
        diags[0].code.clone()
    }

    #[test]
    fn a_triple_quoted_literal_takes_its_margin_from_the_closer() {
        assert_eq!(
            text_of("\"\"\"\n  <p class=\"lead\">hi</p>\n  \"\"\""),
            "<p class=\"lead\">hi</p>"
        );
        // Deeper lines keep what they have past the margin.
        assert_eq!(
            text_of("\"\"\"\n    a\n      b\n    c\n    \"\"\""),
            "a\n  b\nc"
        );
        // A closer at column zero is no margin at all.
        assert_eq!(text_of("\"\"\"\n  a\nb\n\"\"\""), "  a\nb");
        // Tabs are a margin too, compared as tabs.
        assert_eq!(text_of("\"\"\"\n\t\ta\n\t\t\tb\n\t\t\"\"\""), "a\n\tb");
    }

    #[test]
    fn the_newlines_around_a_triple_quoted_literal_are_not_content() {
        assert_eq!(text_of("\"\"\"\n  one\n  \"\"\""), "one");
        // Spaces after the opener are allowed and are not content either.
        assert_eq!(text_of("\"\"\"  \t\n  one\n  \"\"\""), "one");
        // A closer on the line after the opener is the empty string.
        assert_eq!(text_of("\"\"\"\n  \"\"\""), "");
        assert_eq!(text_of("\"\"\"\n\"\"\""), "");
        // A blank line before the closer is one newline of content.
        assert_eq!(text_of("\"\"\"\n  a\n\n  \"\"\""), "a\n");
    }

    #[test]
    fn a_blank_line_in_a_triple_quoted_literal_is_an_empty_line() {
        // Shorter than the margin, longer than it, or empty: all three are
        // empty lines.
        assert_eq!(
            text_of("\"\"\"\n    a\n\n  \n        \n    b\n    \"\"\""),
            "a\n\n\n\nb"
        );
    }

    #[test]
    fn trailing_spaces_on_a_line_of_a_triple_quoted_literal_are_dropped() {
        assert_eq!(text_of("\"\"\"\n  a  \n  b\t\n  \"\"\""), "a\nb");
        // An escape is not trailing space, which is how to keep some.
        assert_eq!(text_of("\"\"\"\n  a\\t\n  b \\u{20}\n  \"\"\""), "a\t\nb  ");
        // Spaces before a hole are not trailing.
        let tokens = kinds("\"\"\"\n  a {b}  \n  \"\"\"");
        let [TokenKind::Str(parts)] = tokens.as_slice() else {
            panic!("one string, got {tokens:?}");
        };
        assert_eq!(parts[0], StringPart::Text("a ".into()));
        assert_eq!(parts.len(), 2, "{parts:?}");
    }

    #[test]
    fn a_quote_is_content_in_a_triple_quoted_literal() {
        assert_eq!(text_of("\"\"\"\n  \"a\" \"\"b\n  \"\"\""), "\"a\" \"\"b");
        // An escaped quote is still a quote, and is how `"""` is written.
        assert_eq!(text_of("\"\"\"\n  \\\"\"\"\n  \"\"\""), "\"\"\"");
    }

    #[test]
    fn escapes_and_holes_in_a_triple_quoted_literal_are_those_of_a_plain_one() {
        assert_eq!(
            text_of("\"\"\"\n  \\\\ \\n \\t \\r \\0 \\{ \\} \\' \\u{e9}\n  \"\"\""),
            "\\ \n \t \r \0 { } ' \u{e9}"
        );
        let diags = lex_err("\"\"\"\n  \\q\n  \"\"\"");
        assert_eq!(diags[0].code, "cove::lex::unknown_escape");
    }

    #[test]
    fn a_hole_in_a_triple_quoted_literal_keeps_its_source_span() {
        let src = "\"\"\"\n  <b>{name}</b>\n  \"\"\"";
        let tokens = kinds(src);
        let start = src.find("name").unwrap() as u32;
        assert_eq!(
            tokens,
            vec![TokenKind::Str(vec![
                StringPart::Text("<b>".into()),
                StringPart::Interpolation {
                    source: "name".into(),
                    span: Span::new(FileId(0), start, start + 4),
                },
                StringPart::Text("</b>".into()),
            ])]
        );
    }

    /// A hole may hold a string, and a `"""` one too, which has a closer and
    /// a margin of its own.
    #[test]
    fn a_hole_may_hold_a_triple_quoted_literal() {
        let src = "\"\"\"\n  a{f(\"\"\"\n    \"x\"\n    \"\"\")}b\n  \"\"\"";
        let tokens = kinds(src);
        let [TokenKind::Str(parts)] = tokens.as_slice() else {
            panic!("one string, got {tokens:?}");
        };
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts[2], StringPart::Text("b".into()));
    }

    #[test]
    fn a_backslash_ending_a_line_eats_the_newline_and_the_next_indentation() {
        let src = "\"\"\"\n  <tr>\\\n    <td>{a}</td>\\\n  </tr>\n  \"\"\"";
        let tokens = kinds(src);
        let start = src.find("{a}").unwrap() as u32 + 1;
        assert_eq!(
            tokens,
            vec![TokenKind::Str(vec![
                StringPart::Text("<tr><td>".into()),
                StringPart::Interpolation {
                    source: "a".into(),
                    span: Span::new(FileId(0), start, start + 1),
                },
                StringPart::Text("</td></tr>".into()),
            ])]
        );
        // Trailing spaces go first, so a `\` followed by spaces still joins.
        assert_eq!(text_of("\"\"\"\n  a\\  \n  b\n  \"\"\""), "ab");
        // Spaces in front of the `\` are not trailing and are kept.
        assert_eq!(text_of("\"\"\"\n  a \\\n  b\n  \"\"\""), "a b");
        // On the last line it has no newline left to eat.
        assert_eq!(text_of("\"\"\"\n  a\\\n  \"\"\""), "a");
        // All of the next line's indentation goes, past the margin too.
        assert_eq!(text_of("\"\"\"\n  a\\\n        b\n  \"\"\""), "ab");
        assert_eq!(text_of("\"\"\"\n  a\\\n  \t b\n  \"\"\""), "ab");
        // A joined line that is blank keeps its newline.
        assert_eq!(text_of("\"\"\"\n  a\\\n\n  b\n  \"\"\""), "a\nb");
        // A joined line still has to begin with the margin.
        assert_eq!(
            only_error("\"\"\"\n    a\\\n  b\n    \"\"\""),
            "cove::lex::literal_margin"
        );
    }

    #[test]
    fn a_doubled_backslash_ending_a_line_is_a_backslash() {
        assert_eq!(text_of("\"\"\"\n  a\\\\\n  b\n  \"\"\""), "a\\\nb");
        assert_eq!(text_of("\"a\\\\\nb\""), "a\\\nb");
    }

    /// A hole may span the line a `\` joins, and its span is still its own.
    #[test]
    fn a_hole_spanning_a_joined_line_keeps_its_source_span() {
        let src = "\"\"\"\n  x{1 +\n      2}\\\n  y\n  \"\"\"";
        let tokens = kinds(src);
        let start = src.find("1 +").unwrap() as u32;
        let end = src.find("2}").unwrap() as u32 + 1;
        assert_eq!(
            tokens,
            vec![TokenKind::Str(vec![
                StringPart::Text("x".into()),
                StringPart::Interpolation {
                    source: "1 +\n      2".into(),
                    span: Span::new(FileId(0), start, end),
                },
                StringPart::Text("y".into()),
            ])]
        );
    }

    #[test]
    fn a_backslash_ending_a_line_of_a_plain_literal_eats_the_next_indentation() {
        assert_eq!(text_of("\"a\\\nb\""), "ab");
        // The next line's leading spaces and tabs go with the newline.
        assert_eq!(text_of("\"a\\\n  \t b\""), "ab");
        // Trailing spaces are content too, and a `\` before them is an escape.
        assert_eq!(text_of("\"a \\\nb\""), "a b");
        assert_eq!(only_error("\"a\\ \nb\""), "cove::lex::unknown_escape");
        // `\r\n` is a newline as well.
        assert_eq!(text_of("\"a\\\r\n  b\""), "ab");
        // And a plain literal keeps its unescaped newlines.
        assert_eq!(text_of("\"a\n  b\""), "a\n  b");
    }

    #[test]
    fn a_triple_quoted_literal_reads_crlf_lines() {
        assert_eq!(
            text_of("\"\"\"\r\n  a\r\n  b\\\r\n    c\r\n  \"\"\""),
            "a\r\nbc"
        );
    }

    // ADR 0101: a name against a quote tags the literal.

    #[test]
    fn a_name_against_a_quote_is_one_tagged_literal() {
        let src = "html\"<p>{x}</p>\"";
        let tokens = lex_ok(src);
        let start = src.find('x').unwrap() as u32;
        assert_eq!(
            tokens[0].kind,
            TokenKind::Tagged {
                tag: "html".into(),
                parts: vec![
                    StringPart::Text("<p>".into()),
                    StringPart::Interpolation {
                        source: "x".into(),
                        span: Span::new(FileId(0), start, start + 1),
                    },
                    StringPart::Text("</p>".into()),
                ],
            }
        );
        // The token's span is the tag and the literal together.
        assert_eq!(tokens[0].span, Span::new(FileId(0), 0, src.len() as u32));
        assert_eq!(tokens.len(), 2, "{tokens:?}");
    }

    #[test]
    fn a_name_against_a_triple_quote_tags_that_literal_too() {
        assert_eq!(
            kinds("html\"\"\"\n  <tr>\\\n    <td>a</td>\n  \"\"\""),
            vec![TokenKind::Tagged {
                tag: "html".into(),
                parts: vec![StringPart::Text("<tr><td>a</td>".into())],
            }]
        );
        // Any name is lexed as a tag; which ones mean anything is the
        // checker's question.
        assert_eq!(
            kinds("sql\"x\""),
            vec![TokenKind::Tagged {
                tag: "sql".into(),
                parts: vec![StringPart::Text("x".into())],
            }]
        );
    }

    #[test]
    fn a_space_between_a_name_and_a_quote_is_two_tokens() {
        assert_eq!(
            kinds("html \"x\""),
            vec![
                TokenKind::Ident("html".into()),
                TokenKind::Str(vec![StringPart::Text("x".into())]),
            ]
        );
    }

    #[test]
    fn a_keyword_against_a_quote_is_not_a_tag() {
        assert_eq!(
            kinds("return\"x\""),
            vec![
                TokenKind::Keyword(Keyword::Return),
                TokenKind::Str(vec![StringPart::Text("x".into())]),
            ]
        );
        assert_eq!(
            kinds("true\"x\""),
            vec![
                TokenKind::Bool(true),
                TokenKind::Str(vec![StringPart::Text("x".into())]),
            ]
        );
    }

    #[test]
    fn an_unclosed_tagged_literal_is_an_unclosed_literal() {
        assert_eq!(only_error("html\"never"), "cove::lex::unterminated_string");
        assert_eq!(
            only_error("html\"\"\"x\"\"\""),
            "cove::lex::triple_quote_opener"
        );
    }

    #[test]
    fn a_line_short_of_the_margin_is_an_error() {
        assert_eq!(
            only_error("\"\"\"\n    a\n  b\n    \"\"\""),
            "cove::lex::literal_margin"
        );
        // Tabs and spaces are not interchangeable.
        assert_eq!(
            only_error("\"\"\"\n\ta\n  \"\"\""),
            "cove::lex::literal_margin"
        );
        assert_eq!(
            only_error("\"\"\"\n  \ta\n\t\"\"\""),
            "cove::lex::literal_margin"
        );
        // One error per line.
        let diags = lex_err("\"\"\"\n a\n b\n  \"\"\"");
        assert_eq!(diags.len(), 2);
        assert_eq!(diags[0].primary, Some(Span::new(FileId(0), 4, 5)));
    }

    #[test]
    fn a_triple_quote_followed_by_text_is_an_error() {
        assert_eq!(
            only_error("\"\"\"x\"\"\""),
            "cove::lex::triple_quote_opener"
        );
        assert_eq!(
            only_error("\"\"\"x\n  y\n  \"\"\""),
            "cove::lex::triple_quote_opener"
        );
        // The rest of the literal is stepped over: what follows still lexes.
        let mut sources = SourceMap::new();
        let file = sources.add("test.cove", "\"\"\"x\"\"\" + 1");
        let (tokens, diagnostics) = lex_recovered(&sources, file);
        assert_eq!(diagnostics.len(), 1);
        let kinds: Vec<TokenKind> = tokens.iter().map(|t| t.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![TokenKind::Plus, TokenKind::Int(1), TokenKind::Eof]
        );
    }

    #[test]
    fn a_closer_with_text_before_it_is_an_error() {
        assert_eq!(
            only_error("\"\"\"\n  a\"\"\""),
            "cove::lex::triple_quote_closer"
        );
        assert_eq!(
            only_error("\"\"\"\n  a\n  b \"\"\""),
            "cove::lex::triple_quote_closer"
        );
        // Including a hole's closing brace.
        assert_eq!(
            only_error("\"\"\"\n  a{1 +\n  2}\"\"\""),
            "cove::lex::triple_quote_closer"
        );
    }

    #[test]
    fn an_unclosed_triple_quoted_literal_is_an_error() {
        assert_eq!(
            only_error("\"\"\"\n  a\n"),
            "cove::lex::unterminated_string"
        );
        assert_eq!(
            only_error("\"\"\"\n  a\"\""),
            "cove::lex::unterminated_string"
        );
        assert_eq!(
            only_error("\"\"\"\n  {a\n  \"\"\""),
            "cove::lex::unterminated_interpolation"
        );
    }

    /// A `"""` literal is one token, and the line break inside it is not a
    /// line break between tokens: the statement-break rule sees through it.
    #[test]
    fn a_triple_quoted_literal_is_one_token() {
        let tokens = lex_ok("f(\"\"\"\n  a\n  \"\"\", b)\nc");
        let kinds: Vec<&TokenKind> = tokens.iter().map(|t| &t.kind).collect();
        assert_eq!(kinds.len(), 8, "{kinds:?}");
        assert!(matches!(kinds[2], TokenKind::Str(_)));
        assert!(!tokens[3].preceded_by_newline);
        assert!(tokens[6].preceded_by_newline);
    }

    #[test]
    fn simple_interpolation() {
        let tokens = kinds(r#""a{b}c""#);
        assert_eq!(
            tokens,
            vec![TokenKind::Str(vec![
                StringPart::Text("a".into()),
                StringPart::Interpolation {
                    source: "b".into(),
                    span: Span::new(FileId(0), 3, 4),
                },
                StringPart::Text("c".into()),
            ])]
        );
    }

    #[test]
    fn interpolation_with_nested_braces_and_nested_strings() {
        // `"{f("}")}"` must lex `f("}")` as the interpolation source: the
        // brace inside the nested string must not end the interpolation
        // early, and the nested string's own quotes must not be confused
        // with the outer string's.
        let tokens = kinds("\"{f(\"}\")}\"");
        assert_eq!(
            tokens,
            vec![TokenKind::Str(vec![StringPart::Interpolation {
                source: "f(\"}\")".into(),
                span: Span::new(FileId(0), 2, 8),
            }])]
        );
    }

    #[test]
    fn multiple_interpolations_in_one_string() {
        let tokens = kinds(r#""{a}-{b}""#);
        assert_eq!(
            tokens,
            vec![TokenKind::Str(vec![
                StringPart::Interpolation {
                    source: "a".into(),
                    span: Span::new(FileId(0), 2, 3),
                },
                StringPart::Text("-".into()),
                StringPart::Interpolation {
                    source: "b".into(),
                    span: Span::new(FileId(0), 6, 7),
                },
            ])]
        );
    }

    #[test]
    fn unterminated_interpolation_is_an_error() {
        let diags = lex_err(r#""{a"#);
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "cove::lex::unterminated_interpolation");
    }

    #[test]
    fn longest_match_operators() {
        assert_eq!(
            kinds("... ..< .. ."),
            vec![
                TokenKind::Ellipsis,
                TokenKind::DotDotLt,
                TokenKind::DotDot,
                TokenKind::Dot,
            ]
        );
        assert_eq!(
            kinds("-> => == != <= >= += -= *= /= %= && ||"),
            vec![
                TokenKind::Arrow,
                TokenKind::FatArrow,
                TokenKind::EqEq,
                TokenKind::BangEq,
                TokenKind::LtEq,
                TokenKind::GtEq,
                TokenKind::PlusEq,
                TokenKind::MinusEq,
                TokenKind::StarEq,
                TokenKind::SlashEq,
                TokenKind::PercentEq,
                TokenKind::AmpAmp,
                TokenKind::PipePipe,
            ]
        );
        assert_eq!(
            kinds("= ! < > + - * / % ? , : ( ) { } [ ]"),
            vec![
                TokenKind::Eq,
                TokenKind::Bang,
                TokenKind::Lt,
                TokenKind::Gt,
                TokenKind::Plus,
                TokenKind::Minus,
                TokenKind::Star,
                TokenKind::Slash,
                TokenKind::Percent,
                TokenKind::Question,
                TokenKind::Comma,
                TokenKind::Colon,
                TokenKind::LParen,
                TokenKind::RParen,
                TokenKind::LBrace,
                TokenKind::RBrace,
                TokenKind::LBracket,
                TokenKind::RBracket,
            ]
        );
        // No space: greedy longest match still applies.
        assert_eq!(kinds("<=="), vec![TokenKind::LtEq, TokenKind::Eq]);
    }

    #[test]
    fn semicolon_is_an_error_with_the_cove_rule() {
        let diags = lex_err(";");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "cove::lex::unexpected_character");
        assert!(diags[0].rule.as_deref().unwrap().contains(';'));
    }

    #[test]
    fn unexpected_character_is_an_error() {
        let diags = lex_err("`");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "cove::lex::unexpected_character");
    }

    #[test]
    fn at_sign_is_reserved_decorator_syntax() {
        let diags = lex_err("@decorate");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, "cove::parse::reserved_annotation");
        assert!(diags[0].rule.as_deref().unwrap().contains("Decorator"));
    }

    #[test]
    fn all_errors_in_a_file_are_collected() {
        let diags = lex_err("` ~ #");
        assert_eq!(diags.len(), 3);
        for diag in &diags {
            assert_eq!(diag.code, "cove::lex::unexpected_character");
        }
    }

    /// The half of the lexer's work that [`lex`] discards. A highlighter
    /// reads it, and the case it reads it for is this one: a string that is
    /// open because the reader has not typed the closing quote yet.
    #[test]
    fn recovery_answers_the_tokens_before_an_error_as_well_as_the_error() {
        let mut sources = SourceMap::new();
        let file = sources.add("test.cove", "let n = 1\nlet greeting = \"open");
        let (tokens, diagnostics) = lex_recovered(&sources, file);

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "cove::lex::unterminated_string");
        assert!(
            lex(&sources, file).is_err(),
            "`lex` still refuses what it always refused"
        );

        let kinds: Vec<TokenKind> = tokens.iter().map(|t| t.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Keyword(Keyword::Let),
                TokenKind::Ident("n".into()),
                TokenKind::Eq,
                TokenKind::Int(1),
                TokenKind::Keyword(Keyword::Let),
                TokenKind::Ident("greeting".into()),
                TokenKind::Eq,
                TokenKind::Eof,
            ],
            "everything up to the open quote, and nothing invented for it"
        );
    }

    /// What [`lex`] answers and what [`lex_recovered`] answers are the same
    /// tokens whenever there is nothing to complain about, which is what lets
    /// one be written in terms of the other.
    #[test]
    fn recovery_and_lex_agree_when_there_is_no_error() {
        let source = "export fn main() -> Int { 1 + 2 }";
        let mut sources = SourceMap::new();
        let file = sources.add("test.cove", source);
        let (tokens, diagnostics) = lex_recovered(&sources, file);
        assert!(diagnostics.is_empty());
        assert_eq!(tokens, lex(&sources, file).expect("it lexes"));
    }

    #[test]
    fn duration_example_program_lexes() {
        let tokens =
            kinds("clock.timeout(500ms) { retry(3, attempts) }\nfor attempt in 0..<attempts {}");
        assert!(tokens.contains(&TokenKind::Duration(500_000_000)));
        assert!(tokens.contains(&TokenKind::DotDotLt));
        assert!(tokens.contains(&TokenKind::Keyword(Keyword::For)));
    }
}
