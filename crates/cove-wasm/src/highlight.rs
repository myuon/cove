//! Colouring source the way the compiler reads it.
//!
//! # Why there is no tokenizer in the page
//!
//! The obvious way to highlight a `<textarea>` is a regular expression and a
//! list of keywords in JavaScript. It is also the way that goes wrong
//! silently: the list is a second, informal specification of the language,
//! nothing compares it against the first, and the day `opaque` or `..<` is
//! added the page keeps colouring the old language and no test anywhere has
//! an opinion. A playground that lied about what Cove is would be worse than
//! one that showed plain text.
//!
//! The module a page already loads contains the whole front end, so the
//! lexer is *there*. [`paint`] calls it. The keywords are
//! [`cove_syntax::token::Keyword`]'s, the numbers are the ones
//! [`cove_syntax::lexer`] accepts including `500ms`, and a token kind the
//! language grows arrives here as a token kind. Agreement is by construction
//! rather than by discipline.
//!
//! # What comes out
//!
//! A *tiling*: a list of pieces that between them cover every UTF-16 code
//! unit of the source exactly once, in order. The page renders it by walking
//! the list and slicing the text — no offsets to reconcile, no gaps to
//! guess, and a check that the pieces tile is a check the whole thing is
//! sound.
//!
//! UTF-16 and not bytes because the consumer is JavaScript, where a string is
//! indexed in UTF-16 code units. Two of the shipped samples contain an em
//! dash, so this is not a hypothetical: byte offsets would have misaligned
//! every colour after the first `—`.
//!
//! # Six categories, and the two that are not token kinds
//!
//! [`Kind`] is deliberately short. A playground wants a reader to see the
//! shape of a program, and twenty colours is a wall rather than a shape.
//!
//! Two of the six do not come from a [`TokenKind`], and both are named here
//! because they are the parts a reader should check rather than trust:
//!
//! - **`type`** is an identifier that begins with an uppercase letter. The
//!   lexer does not know what a type is — it answers
//!   [`TokenKind::Ident`] for `Int` and for `total` alike. Uppercase is not
//!   only a convention in Cove, though: the parser's `parse_pattern` decides
//!   that `Ok(value)` is a variant and `other` is a binding on exactly this
//!   rule, so the page is colouring by something the grammar already reads.
//!   It is still a heuristic about *names*, and a struct someone called
//!   `Total` is coloured as a type because it is one.
//!
//! - **`comment`** for a `//` or `/* */` comment, which is not a token at
//!   all: the lexer discards them. They are recovered from the *gaps* between
//!   tokens, which in a source that lexes hold nothing else. The rule and its
//!   one wrong answer are written out where it is applied.
//!
//! # Source that does not lex
//!
//! Constantly, because the reader is typing. One open quote and the file has
//! a lexical error, and that is the state a string literal is in for as long
//! as it takes to write one.
//!
//! [`cove_syntax::lexer::lex_recovered`] exists for this: the tokens before
//! the error are real tokens and are answered, so the colouring is of the
//! text that is actually in the box rather than a stale picture of the text
//! that was there two keystrokes ago. `ok` reports whether the source lexed
//! cleanly, and it is the *page's* text either way — nothing here ever
//! answers a tiling of something the caller did not send.

use cove_diag::SourceMap;
use cove_syntax::lexer::lex_recovered;
use cove_syntax::token::TokenKind;

use crate::PATH;

/// What one piece of the source is coloured as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A keyword, and `true` / `false` with them: they are spelled like
    /// keywords, a reader reads them as keywords, and a category of their own
    /// would buy a colour nobody needs.
    Keyword,
    /// An identifier beginning with an uppercase letter. See the module
    /// documentation: this is the one category the lexer does not decide.
    Type,
    /// A string literal, interpolations and all. `"Hello, {name}!"` is one
    /// piece and not three — the interpolated expression is not re-lexed,
    /// because a second colour inside a string is a detail a playground can
    /// do without.
    Str,
    /// An integer, a float, or a duration such as `500ms`.
    Number,
    /// A `//`, `/* */` or `///` comment.
    Comment,
    /// Everything else: punctuation, operators, ordinary names, whitespace.
    Plain,
}

impl Kind {
    /// The name the page's CSS class is built from.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Keyword => "keyword",
            Kind::Type => "type",
            Kind::Str => "string",
            Kind::Number => "number",
            Kind::Comment => "comment",
            Kind::Plain => "plain",
        }
    }
}

/// One run of source that is all one colour.
///
/// `at` and `len` are in UTF-16 code units, which is what `String.prototype
/// .slice` counts in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub at: usize,
    pub len: usize,
    pub kind: Kind,
}

/// A tiling of one source, and whether it lexed without complaint.
#[derive(Debug)]
pub struct Painting {
    pub pieces: Vec<Piece>,
    /// False when the lexer had something to say. The pieces are still a
    /// tiling of the whole source; this says how much to trust their colours.
    pub ok: bool,
}

/// Which colour a token gets.
fn category(kind: &TokenKind) -> Kind {
    match kind {
        TokenKind::Keyword(_) | TokenKind::Bool(_) => Kind::Keyword,
        TokenKind::Int(_) | TokenKind::Float(_) | TokenKind::Duration(_) => Kind::Number,
        TokenKind::Str(_) => Kind::Str,
        TokenKind::DocComment(_) => Kind::Comment,
        TokenKind::Ident(name) => {
            if name.chars().next().is_some_and(char::is_uppercase) {
                Kind::Type
            } else {
                Kind::Plain
            }
        }
        _ => Kind::Plain,
    }
}

/// The tiling being built, and where in the source it has got to.
struct Tiling<'a> {
    source: &'a str,
    pieces: Vec<Piece>,
    /// How many UTF-16 code units the pieces so far cover, which is the `at`
    /// of the next one.
    utf16: usize,
}

impl Tiling<'_> {
    /// Adds `source[start..end]` as one piece, merging it into the previous
    /// piece when they are the same colour.
    ///
    /// Merging is not only for the payload's size, though a run of
    /// punctuation and spaces is most of a program and this halves it. It is
    /// so that the page builds one DOM node per *visible* run rather than one
    /// per token, on every keystroke.
    fn take(&mut self, start: usize, end: usize, kind: Kind) {
        if start >= end {
            return;
        }
        let len = self.source[start..end].encode_utf16().count();
        match self.pieces.last_mut() {
            Some(last) if last.kind == kind => last.len += len,
            _ => self.pieces.push(Piece {
                at: self.utf16,
                len,
                kind,
            }),
        }
        self.utf16 += len;
    }

    /// Adds the text between two tokens.
    ///
    /// Whitespace is the whole of a gap in almost every one, and whitespace
    /// has no colour. What is left is what the lexer *skipped*, and in a
    /// source that lexes there is exactly one thing it skips: a comment. So a
    /// gap with anything in it is a comment, and that is how comments are
    /// coloured at all without the lexer producing a token for them.
    ///
    /// In a source that does not lex there is one more thing it skips, and it
    /// is the common one while typing: an unterminated string, from its
    /// opening quote to end of file. A gap opening with `"` is that, and
    /// colouring it as a string is what makes the rest of the file stop
    /// changing colour on every character typed inside one.
    ///
    /// Anything else a broken source skipped — a stray `;`, a `@` — is left
    /// plain, which is the honest answer for text the lexer refused to read.
    /// So is a gap that opens with a comment and then goes wrong: the whole
    /// of it is coloured as the comment it began as, and that is a wrong
    /// colour on text that is already an error.
    fn gap(&mut self, start: usize, end: usize) {
        let text = &self.source[start..end];
        let body = text.trim_start();
        let opens = start + (text.len() - body.len());
        let closes = opens + body.trim_end().len();

        let kind = match body.as_bytes().first() {
            Some(b'/') => Kind::Comment,
            Some(b'"') => Kind::Str,
            _ => Kind::Plain,
        };
        self.take(start, opens, Kind::Plain);
        self.take(opens, closes, kind);
        self.take(closes, end, Kind::Plain);
    }
}

/// Lexes `source` and answers a colour for every part of it.
///
/// The pieces tile: `pieces[0].at` is zero, each one begins where the last
/// ended, and together they cover the source. That holds for a source that
/// does not lex too — see the module documentation for why that case is the
/// normal one rather than the exception.
pub fn paint(source: &str) -> Painting {
    let mut sources = SourceMap::new();
    let file = sources.add(PATH, source.to_string());
    let (tokens, diagnostics) = lex_recovered(&sources, file);

    let mut tiling = Tiling {
        source,
        pieces: Vec::new(),
        utf16: 0,
    };
    let mut covered = 0usize;
    for token in &tokens {
        let start = (token.span.start as usize).min(source.len()).max(covered);
        let end = (token.span.end as usize).min(source.len()).max(start);
        tiling.gap(covered, start);
        tiling.take(start, end, category(&token.kind));
        covered = end;
    }
    tiling.gap(covered, source.len());

    Painting {
        pieces: tiling.pieces,
        ok: diagnostics.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tiling property, checked against the source it is of: every piece
    /// begins where the last ended, and the last ends at the end.
    fn tiles(source: &str, painting: &Painting) {
        let mut at = 0;
        for piece in &painting.pieces {
            assert_eq!(piece.at, at, "a piece begins where the last ended");
            assert!(piece.len > 0, "no empty piece");
            at += piece.len;
        }
        assert_eq!(at, source.encode_utf16().count(), "the pieces cover it all");
    }

    /// `[(text, kind)]`, which is what a colouring actually looks like.
    fn coloured(source: &str) -> Vec<(String, Kind)> {
        let painting = paint(source);
        tiles(source, &painting);
        let units: Vec<u16> = source.encode_utf16().collect();
        painting
            .pieces
            .iter()
            .map(|piece| {
                (
                    String::from_utf16(&units[piece.at..piece.at + piece.len])
                        .expect("a piece is whole code points"),
                    piece.kind,
                )
            })
            .collect()
    }

    #[test]
    fn a_declaration_is_coloured_by_what_the_lexer_called_each_token() {
        assert_eq!(
            coloured("export fn main() -> Int { 42 }"),
            vec![
                ("export".into(), Kind::Keyword),
                (" ".into(), Kind::Plain),
                ("fn".into(), Kind::Keyword),
                (" main() -> ".into(), Kind::Plain),
                ("Int".into(), Kind::Type),
                (" { ".into(), Kind::Plain),
                ("42".into(), Kind::Number),
                (" }".into(), Kind::Plain),
            ]
        );
    }

    #[test]
    fn a_comment_is_the_gap_the_lexer_left() {
        assert_eq!(
            coloured("let n = 1 // why\nlet m = 2"),
            vec![
                ("let".into(), Kind::Keyword),
                (" n = ".into(), Kind::Plain),
                ("1".into(), Kind::Number),
                (" ".into(), Kind::Plain),
                ("// why".into(), Kind::Comment),
                ("\n".into(), Kind::Plain),
                ("let".into(), Kind::Keyword),
                (" m = ".into(), Kind::Plain),
                ("2".into(), Kind::Number),
            ]
        );
    }

    #[test]
    fn a_block_comment_and_a_doc_comment_are_both_comments() {
        let held = coloured("/* out */\n/// in\nfn f() {}");
        assert_eq!(held[0].1, Kind::Comment);
        assert_eq!(held[0].0, "/* out */");
        assert!(
            held.iter()
                .any(|(text, kind)| text.contains("/// in") && *kind == Kind::Comment),
            "{held:?}"
        );
    }

    #[test]
    fn a_string_is_one_piece_interpolation_and_all() {
        assert_eq!(
            coloured("\"Hello, {name}!\""),
            vec![("\"Hello, {name}!\"".into(), Kind::Str)]
        );
    }

    #[test]
    fn a_duration_and_a_float_are_numbers() {
        assert_eq!(
            coloured("500ms 1.5 0xff"),
            vec![
                ("500ms".into(), Kind::Number),
                (" ".into(), Kind::Plain),
                ("1.5".into(), Kind::Number),
                (" ".into(), Kind::Plain),
                ("0xff".into(), Kind::Number),
            ]
        );
    }

    #[test]
    fn true_and_false_are_coloured_as_the_keywords_they_are_spelled_as() {
        assert_eq!(coloured("true"), vec![("true".into(), Kind::Keyword)]);
    }

    /// The state the editor is in for as long as it takes to type a string.
    /// Everything before the quote keeps its colours, and the open literal is
    /// a string to end of file rather than a hole.
    #[test]
    fn an_open_quote_still_colours_the_whole_file() {
        let source = "let n = 1\nlet greeting = \"open";
        let painting = paint(source);
        assert!(!painting.ok, "it does not lex, and says so");
        tiles(source, &painting);
        assert_eq!(
            coloured(source).last(),
            Some(&("\"open".to_string(), Kind::Str))
        );
    }

    #[test]
    fn a_stray_character_is_left_plain_rather_than_guessed_at() {
        let painting = paint("let n = 1;");
        assert!(!painting.ok);
        assert_eq!(
            coloured("let n = 1;").last(),
            Some(&(";".to_string(), Kind::Plain))
        );
    }

    /// Offsets are UTF-16 because JavaScript's are. An em dash is one code
    /// point, two UTF-8 bytes more than an ASCII character, and one UTF-16
    /// code unit; a tiling counted in bytes would put every colour after it
    /// in the wrong place.
    #[test]
    fn offsets_are_counted_the_way_a_javascript_string_is() {
        let source = "// an — dash\nlet n = 1";
        let painting = paint(source);
        tiles(source, &painting);
        assert_eq!(
            painting.pieces[0].len,
            "// an — dash".encode_utf16().count()
        );
        assert_eq!(painting.pieces[0].len, 12);
    }

    #[test]
    fn an_empty_source_is_an_empty_tiling() {
        let painting = paint("");
        assert!(painting.ok);
        assert!(painting.pieces.is_empty());
    }

    #[test]
    fn no_two_neighbours_share_a_colour() {
        let source = "export fn main() -> Int {\n  let n = 1 // one\n  n\n}\n";
        let painting = paint(source);
        tiles(source, &painting);
        for pair in painting.pieces.windows(2) {
            assert_ne!(pair[0].kind, pair[1].kind, "{:?}", painting.pieces);
        }
    }
}
