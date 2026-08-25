//! The Cove lexer.
//!
//! Converts one source file into a flat token stream. Lexing never panics on
//! malformed input: every lexical error is collected as a [`Diagnostic`] and
//! the lexer recovers by skipping the offending text, so a single call to
//! [`lex`] can report every problem in a file at once.

use cove_diag::{Diagnostic, FileId, SourceMap, Span};

use crate::token::{Keyword, StringPart, Token, TokenKind};

/// Lexes `file` out of `sources` into a token stream.
///
/// The returned stream always ends with exactly one [`TokenKind::Eof`] whose
/// span is the empty range at end of file. On any lexical error, every error
/// found in the file is collected and returned; no tokens are produced in
/// that case.
pub fn lex(sources: &SourceMap, file: FileId) -> Result<Vec<Token>, Vec<Diagnostic>> {
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

    if lexer.diagnostics.is_empty() {
        Ok(lexer.tokens)
    } else {
        Err(lexer.diagnostics)
    }
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
                self.bump();
                if let Some(kind) = self.lex_string(start) {
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
                self.push_token(kind, start);
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

    /// Lexes a string literal, having already consumed its opening `"` at
    /// `quote_start`. Returns `None` (after recording a diagnostic) if the
    /// string or one of its interpolations is never closed.
    fn lex_string(&mut self, quote_start: usize) -> Option<TokenKind> {
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
                    return Some(TokenKind::Str(parts));
                }
                Some('\\') => {
                    let esc_start = self.pos;
                    self.bump();
                    match self.peek_char() {
                        None => {
                            self.unterminated_string(quote_start);
                            return None;
                        }
                        Some(escaped) => {
                            self.bump();
                            match escaped {
                                '\\' => current.push('\\'),
                                '"' => current.push('"'),
                                'n' => current.push('\n'),
                                't' => current.push('\t'),
                                'r' => current.push('\r'),
                                '0' => current.push('\0'),
                                '{' => current.push('{'),
                                '}' => current.push('}'),
                                _ => {
                                    let span =
                                        Span::new(self.file, esc_start as u32, self.pos as u32);
                                    let message = format!("unknown escape sequence `\\{escaped}`");
                                    self.diagnostics.push(
                                        Diagnostic::error("cove::lex::unknown_escape", message)
                                            .at(span)
                                            .help(
                                                "Use one of the supported escapes: \\\\, \\\", \\n, \\t, \\r, \\0, \\{, \\}.",
                                            ),
                                    );
                                }
                            }
                        }
                    }
                }
                Some('{') => {
                    let brace_start = self.pos;
                    self.bump();
                    if !current.is_empty() {
                        parts.push(StringPart::Text(std::mem::take(&mut current)));
                    }
                    let interp_start = self.pos;
                    match self.skip_interpolation_body() {
                        Ok(()) => {
                            let interp_end = self.pos - 1;
                            let source = self.text[interp_start..interp_end].to_string();
                            let span = Span::new(self.file, interp_start as u32, interp_end as u32);
                            parts.push(StringPart::Interpolation { source, span });
                        }
                        Err(()) => {
                            let span = Span::new(self.file, brace_start as u32, self.pos as u32);
                            self.diagnostics.push(
                                Diagnostic::error(
                                    "cove::lex::unterminated_interpolation",
                                    "string interpolation is never closed",
                                )
                                .at(span)
                                .help("Add a matching `}`."),
                            );
                            return None;
                        }
                    }
                }
                Some(c) => {
                    self.bump();
                    current.push(c);
                }
            }
        }
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
    /// that was just consumed by the caller, recursing into nested `{ }`
    /// blocks and skipping over any nested string literals (so that a `}`
    /// inside a nested string does not end the interpolation early).
    fn skip_interpolation_body(&mut self) -> Result<(), ()> {
        loop {
            match self.peek_char() {
                None => return Err(()),
                Some('}') => {
                    self.bump();
                    return Ok(());
                }
                Some('{') => {
                    self.bump();
                    self.skip_interpolation_body()?;
                }
                Some('"') => {
                    self.bump();
                    self.skip_string_body()?;
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
    }

    /// Consumes source text up to and including the `"` matching the `"`
    /// that was just consumed by the caller, recursing into any nested
    /// interpolations so their braces and strings are not mistaken for this
    /// string's terminator.
    fn skip_string_body(&mut self) -> Result<(), ()> {
        loop {
            match self.peek_char() {
                None => return Err(()),
                Some('\\') => {
                    self.bump();
                    if self.peek_char().is_none() {
                        return Err(());
                    }
                    self.bump();
                }
                Some('"') => {
                    self.bump();
                    return Ok(());
                }
                Some('{') => {
                    self.bump();
                    self.skip_interpolation_body()?;
                }
                Some(_) => {
                    self.bump();
                }
            }
        }
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

    #[test]
    fn duration_example_program_lexes() {
        let tokens =
            kinds("clock.timeout(500ms) { retry(3, attempts) }\nfor attempt in 0..<attempts {}");
        assert!(tokens.contains(&TokenKind::Duration(500_000_000)));
        assert!(tokens.contains(&TokenKind::DotDotLt));
        assert!(tokens.contains(&TokenKind::Keyword(Keyword::For)));
    }
}
