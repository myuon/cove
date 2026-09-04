//! Colouring source the way the compiler reads it, and a disassembly the
//! way its printer writes it.
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
//! # Six categories for source, and the two that are not token kinds
//!
//! [`Kind`] is deliberately short. A playground wants a reader to see the
//! shape of a program, and twenty colours is a wall rather than a shape. Six
//! of its seven are what source is cut into; the seventh, `slot`, belongs to
//! the disassembly below and is argued there.
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
//!
//! # The other text on the page: a disassembly
//!
//! [`disassembly`] colours what [`cove_ir::print`] emits, and it exists for
//! the reason above rather than in spite of it. The page shows that text
//! already; a wall of one colour is what it was. The obvious way to fix that
//! would have been a regular expression in the page, and it would have been
//! the same mistake: an informal second reader of a format, drifting from the
//! format with nothing watching.
//!
//! What is honest to say is that this *is* a second reader. There is no lexer
//! for a disassembly to borrow, and making the printer emit spans would mean
//! rewriting two hundred lines of `format!` in `cove-ir` for a colour on a
//! playground pane. So the reader is here, in Rust, next to the crate that
//! prints the text and inside the module the page already takes tilings from,
//! and the drift is caught by a test rather than by discipline:
//! `web/check.mjs` colours the **real disassembly of all nine shipped
//! samples** and fails the build if a single line of any of them is one this
//! reader does not recognise. That is what `ok` is for here — not "the text
//! is well formed", which it always is, but "every line of it was a line
//! shape [`cove_ir::print`] documents".
//!
//! It reads the six line shapes that module writes and nothing about any
//! particular instruction: a header, a frame, a capture, a local, a blank
//! line, and `pc  opcode operands`. Inside an instruction it goes by the
//! shape of each token — `s3:int` is a slot, `"…"` is a literal, digits are a
//! number, a name before ` (` is a callee and any other name is a layout.
//! Adding an instruction to the language therefore needs nothing here;
//! changing how *operands* are written does, and that is what the nine
//! samples are asserted for.

use cove_diag::SourceMap;
use cove_syntax::lexer::lex_recovered;
use cove_syntax::token::TokenKind;

use crate::PATH;

/// What one piece of a text is coloured as.
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
    /// An integer, a float, or a duration such as `500ms`. In a
    /// [`disassembly`] it is also a program counter, a jump target and a
    /// function's id: an index into the program is the same kind of fact as a
    /// number written in it, and a reader scanning for "where" wants them one
    /// colour rather than two.
    Number,
    /// A slot and what it is annotated with, `s3:int`, in a [`disassembly`].
    ///
    /// The one category source has no use for, and the one the disassembly
    /// could not do without: a slot number is the thing a reader follows from
    /// line to line, and it is written in more places than any other token.
    /// It is one piece and not three because the annotation is what makes the
    /// number mean something — `s3` on its own says nothing about whether the
    /// instruction moved a word or a `Point`.
    Slot,
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
            Kind::Slot => "slot",
            Kind::Comment => "comment",
            Kind::Plain => "plain",
        }
    }
}

/// One run of text that is all one colour.
///
/// `at` and `len` are in UTF-16 code units, which is what `String.prototype
/// .slice` counts in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub at: usize,
    pub len: usize,
    pub kind: Kind,
}

/// A tiling of one text, and whether the thing that read it had a complaint.
#[derive(Debug)]
pub struct Painting {
    pub pieces: Vec<Piece>,
    /// False when the reader had something to say: for [`paint`], that the
    /// lexer did; for [`disassembly`], that a line was not one of the shapes
    /// [`cove_ir::print`] writes. The pieces are still a tiling of the whole
    /// text either way; this says how much to trust their colours.
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

/// Colours a disassembly and answers a colour for every part of it.
///
/// The pieces tile, as [`paint`]'s do. `ok` is whether every line was one of
/// the shapes [`cove_ir::print`] writes; a line that was not is left entirely
/// plain and turns `ok` false, which is the signal a check reads to find out
/// that the printer has grown a line this reader does not know. See the
/// module documentation for why that check is where the agreement lives.
pub fn disassembly(text: &str) -> Painting {
    let mut tiling = Tiling {
        source: text,
        pieces: Vec::new(),
        utf16: 0,
    };
    let mut ok = true;
    let mut at = 0;
    while at < text.len() {
        let end = text[at..].find('\n').map_or(text.len(), |n| at + n + 1);
        let body = if text[..end].ends_with('\n') {
            end - 1
        } else {
            end
        };
        ok &= line(&mut tiling, text, at, body);
        // The line terminator belongs to no line's colouring.
        tiling.take(body, end, Kind::Plain);
        at = end;
    }
    Painting {
        pieces: tiling.pieces,
        ok,
    }
}

/// The words [`cove_ir::print`] writes as words rather than as an operand.
///
/// Four, and every one of them is in that module's source as a string
/// literal: `else` separates a `switch`'s table from its default, `async`
/// ends the header of an async function, and `true` and `false` are how a
/// `bool` immediate is spelled. Without this list `else` would be read as a
/// layout name, because in that position everything else is one.
const WORDS: [&str; 4] = ["else", "async", "true", "false"];

/// Colours one line of a disassembly, without its terminator.
///
/// Answers whether it was recognised. The four shapes with an indent are told
/// apart by their first character — a digit begins an instruction, a letter
/// begins one of the three headings — and a line with no indent is the
/// function header itself.
fn line(tiling: &mut Tiling, text: &str, start: usize, end: usize) -> bool {
    let held = &text[start..end];
    if held.trim().is_empty() {
        tiling.take(start, end, Kind::Plain);
        return true;
    }
    if !held.starts_with(' ') {
        return header(tiling, text, start, end);
    }
    let body = held.trim_start();
    let at = end - body.len();
    for word in ["frame", "capture", "local"] {
        if let Some(rest) = body.strip_prefix(word) {
            if rest.starts_with(' ') {
                return heading(tiling, text, start, at, end, word);
            }
        }
    }
    if body.starts_with(|c: char| c.is_ascii_digit()) {
        return instruction(tiling, text, start, at, end);
    }
    tiling.take(start, end, Kind::Plain);
    false
}

/// `fn0 playground.main(Int) -> Int`, optionally ` async`.
///
/// The name is left plain as a whole, generic arguments and all: it is one
/// name however many angle brackets are in it, and cutting it up would say
/// that `playground.headline<playground.Booking>` is two things.
fn header(tiling: &mut Tiling, text: &str, start: usize, end: usize) -> bool {
    let held = &text[start..end];
    let id = held
        .strip_prefix("fn")
        .map(|rest| 2 + rest.bytes().take_while(u8::is_ascii_digit).count())
        .filter(|len| *len > 2 && held[*len..].starts_with(' '));
    let (Some(id), Some(close)) = (id, held.rfind(") -> ")) else {
        tiling.take(start, end, Kind::Plain);
        return false;
    };
    let Some(open) = held[..close].find('(') else {
        tiling.take(start, end, Kind::Plain);
        return false;
    };
    tiling.take(start, start + id, Kind::Number);
    tiling.take(start + id, start + open + 1, Kind::Plain);
    operands(tiling, text, start + open + 1, start + close);
    tiling.take(start + close, start + close + 5, Kind::Plain);
    match held.strip_suffix(" async") {
        Some(front) => {
            operands(tiling, text, start + close + 5, start + front.len());
            tiling.take(start + front.len(), end - 5, Kind::Plain);
            tiling.take(end - 5, end, Kind::Keyword);
        }
        None => operands(tiling, text, start + close + 5, end),
    }
    true
}

/// `  frame 4: s0!:int …`, `  capture text -> s0:String`, or
/// `  local n -> s1:Int [1, 4)`.
///
/// The two with an arrow name a *source* name, which is neither a layout nor
/// a callee and is left plain; everything after the arrow is operands.
fn heading(
    tiling: &mut Tiling,
    text: &str,
    start: usize,
    at: usize,
    end: usize,
    word: &str,
) -> bool {
    tiling.take(start, at, Kind::Plain);
    tiling.take(at, at + word.len(), Kind::Keyword);
    let rest = at + word.len();
    if word == "frame" {
        operands(tiling, text, rest, end);
        return true;
    }
    let Some(arrow) = text[rest..end].find(" -> ").map(|n| rest + n + 4) else {
        tiling.take(rest, end, Kind::Plain);
        return false;
    };
    tiling.take(rest, arrow, Kind::Plain);
    operands(tiling, text, arrow, end);
    true
}

/// `     0  int s1:int 21`: a program counter, then an opcode, then operands.
fn instruction(tiling: &mut Tiling, text: &str, start: usize, at: usize, end: usize) -> bool {
    let bytes = text.as_bytes();
    let mut pc = at;
    while pc < end && bytes[pc].is_ascii_digit() {
        pc += 1;
    }
    let mut gap = pc;
    while gap < end && bytes[gap] == b' ' {
        gap += 1;
    }
    let mut op = gap;
    while op < end && (bytes[op].is_ascii_lowercase() || matches!(bytes[op], b'.' | b'-')) {
        op += 1;
    }
    if gap == pc || op == gap {
        tiling.take(start, end, Kind::Plain);
        return false;
    }
    tiling.take(start, at, Kind::Plain);
    tiling.take(at, pc, Kind::Number);
    tiling.take(pc, gap, Kind::Plain);
    tiling.take(gap, op, Kind::Keyword);
    operands(tiling, text, op, end);
    true
}

/// Colours `text[from..to]` by the shape of each token in it.
///
/// This is the part that knows nothing about which instruction it is in, and
/// deliberately: an instruction added to `cove-ir` writes its operands in the
/// same six spellings every other one does, so it arrives here already
/// coloured. What a new *spelling* would do is arrive as a layout name, which
/// is the fallback, and that is the drift the nine samples are asserted
/// against.
fn operands(tiling: &mut Tiling, text: &str, from: usize, to: usize) {
    let bytes = text.as_bytes();
    let mut at = from;
    // Where the last name ended, so that the `<` of `Array<array>` — a shape,
    // written against its layout — is told from the `<` of `<addr>`, which is
    // a layout name that is spelled in brackets.
    let mut name = usize::MAX;
    while at < to {
        let c = bytes[at];
        if c == b' ' {
            let end = (at..to).find(|i| bytes[*i] != b' ').unwrap_or(to);
            tiling.take(at, end, Kind::Plain);
            at = end;
        } else if c == b'"' {
            let end = literal(text, at, to);
            tiling.take(at, end, Kind::Str);
            at = end;
        } else if let Some(end) = slot(bytes, at, to) {
            tiling.take(at, end, Kind::Slot);
            at = end;
        } else if c == b'x' && at + 1 < to && counted(bytes, at + 1, to) {
            // The `x` of `alloc s10:ref Array<array> x3`, which is a mark on
            // the count rather than a name of its own.
            tiling.take(at, at + 1, Kind::Plain);
            at += 1;
        } else if c.is_ascii_digit()
            || (matches!(c, b'+' | b'-') && at + 1 < to && bytes[at + 1].is_ascii_digit())
        {
            let mut end = at + 1;
            while end < to
                && (bytes[end].is_ascii_digit()
                    || (bytes[end] == b'.' && end + 1 < to && bytes[end + 1].is_ascii_digit()))
            {
                end += 1;
            }
            tiling.take(at, end, Kind::Number);
            at = end;
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let mut end = at;
            while end < to
                && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'_' | b'.' | b'#'))
            {
                end += 1;
            }
            let word = &text[at..end];
            tiling.take(
                at,
                end,
                if WORDS.contains(&word) {
                    Kind::Keyword
                } else if matches!(word, "inf" | "NaN") {
                    Kind::Number
                } else if text[end..to].starts_with(" (") {
                    // A callee: the printer writes every call's argument list
                    // as ` (…)`, and nothing else is followed by one.
                    Kind::Plain
                } else {
                    Kind::Type
                },
            );
            name = end;
            at = end;
        } else if c == b'<' {
            let end = text[at..to].find('>').map_or(to, |n| at + n + 1);
            let kind = if name == at { Kind::Plain } else { Kind::Type };
            tiling.take(at, end, kind);
            at = end;
        } else {
            // A UTF-8 character and not a byte: a string literal is the only
            // place a non-ASCII one can be, but slicing one in half panics.
            let step = text[at..].chars().next().map_or(1, char::len_utf8);
            tiling.take(at, (at + step).min(to), Kind::Plain);
            at += step;
        }
    }
}

/// The end of the string literal starting at `at`, or `to` if it is unclosed.
fn literal(text: &str, at: usize, to: usize) -> usize {
    let mut escaped = false;
    for (off, c) in text[at + 1..to].char_indices() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            return at + 1 + off + 1;
        }
    }
    to
}

/// The end of the slot starting at `at`, if one starts there.
///
/// `s3:int`, `s0!:ref` in a frame line where `!` marks a parameter, `s10:?`
/// where the frame is too short to say what the word holds, `s10:<addr>`, and
/// `s3:playground.Point` in an argument list where the annotation is the
/// layout rather than the `Repr`.
fn slot(bytes: &[u8], at: usize, to: usize) -> Option<usize> {
    if bytes[at] != b's' {
        return None;
    }
    let mut end = at + 1;
    while end < to && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == at + 1 {
        return None;
    }
    if end < to && bytes[end] == b'!' {
        end += 1;
    }
    if end >= to || bytes[end] != b':' {
        return None;
    }
    end += 1;
    if end < to && bytes[end] == b'<' {
        while end < to && bytes[end] != b'>' {
            end += 1;
        }
        return Some((end + 1).min(to));
    }
    if end < to && bytes[end] == b'?' {
        return Some(end + 1);
    }
    let annotation = end;
    while end < to
        && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'_' | b'.' | b'#'))
    {
        end += 1;
    }
    (end > annotation).then_some(end)
}

/// Whether `at` begins the count of an `alloc`, which is a number or a slot.
fn counted(bytes: &[u8], at: usize, to: usize) -> bool {
    bytes[at].is_ascii_digit() || slot(bytes, at, to).is_some()
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

    // ---- the disassembly ------------------------------------------------
    //
    // `web/check.mjs` is where this is held against the real output of
    // `cove_ir::print`, on all nine shipped samples, because only the real
    // thing can catch the printer growing a line this file does not know.
    // What is here is the line shapes it is meant to read, written out so
    // that a change to one of them fails at `cargo t` rather than at the end
    // of a wasm build.

    /// `[(text, kind)]` for a disassembly, with the plain runs dropped: it is
    /// the coloured pieces that are the claim, and the punctuation between
    /// them is noise in an assertion.
    fn lit(text: &str) -> Vec<(String, Kind)> {
        let painting = disassembly(text);
        assert!(painting.ok, "every line is one this reader knows: {text}");
        tiles(text, &painting);
        let units: Vec<u16> = text.encode_utf16().collect();
        painting
            .pieces
            .iter()
            .filter(|piece| piece.kind != Kind::Plain)
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
    fn a_header_names_its_layouts_and_leaves_the_function_plain() {
        assert_eq!(
            lit("fn2 playground.Point.shift(<addr> Int Int) -> Unit\n"),
            vec![
                ("fn2".into(), Kind::Number),
                ("<addr>".into(), Kind::Type),
                ("Int".into(), Kind::Type),
                ("Int".into(), Kind::Type),
                ("Unit".into(), Kind::Type),
            ]
        );
    }

    /// A generic instantiation is written into the name, and it is still one
    /// name: the brackets do not turn it into a layout half-way through.
    #[test]
    fn a_generic_header_is_one_name() {
        assert_eq!(
            lit("fn7 playground.headline<playground.Booking>(playground.Booking) -> String\n"),
            vec![
                ("fn7".into(), Kind::Number),
                ("playground.Booking".into(), Kind::Type),
                ("String".into(), Kind::Type),
            ]
        );
    }

    #[test]
    fn an_async_header_says_so_in_the_word_the_printer_wrote() {
        let held = lit("fn0 playground.main() -> Int async\n");
        assert_eq!(held.last(), Some(&("async".to_string(), Kind::Keyword)));
    }

    #[test]
    fn a_frame_is_its_slots_and_a_parameter_keeps_its_mark() {
        assert_eq!(
            lit("  frame 3: s0!:int s1:ref s2:?\n"),
            vec![
                ("frame".into(), Kind::Keyword),
                ("3".into(), Kind::Number),
                ("s0!:int".into(), Kind::Slot),
                ("s1:ref".into(), Kind::Slot),
                ("s2:?".into(), Kind::Slot),
            ]
        );
    }

    /// The name a `local` binds is the source's own, and is neither a layout
    /// nor a callee. The pc range it holds the slot over is a pair of numbers
    /// like any other.
    #[test]
    fn a_local_names_a_slot_over_a_range_of_program_counters() {
        assert_eq!(
            lit("  local count -> s3:Int [4, 11)\n"),
            vec![
                ("local".into(), Kind::Keyword),
                ("s3:Int".into(), Kind::Slot),
                ("4".into(), Kind::Number),
                ("11".into(), Kind::Number),
            ]
        );
    }

    #[test]
    fn a_capture_is_a_name_and_a_slot() {
        assert_eq!(
            lit("  capture text -> s0:String\n"),
            vec![
                ("capture".into(), Kind::Keyword),
                ("s0:String".into(), Kind::Slot),
            ]
        );
    }

    #[test]
    fn an_instruction_is_a_program_counter_an_opcode_and_operands() {
        assert_eq!(
            lit("     2  mul.int s3:int s1:int s2:int\n"),
            vec![
                ("2".into(), Kind::Number),
                ("mul.int".into(), Kind::Keyword),
                ("s3:int".into(), Kind::Slot),
                ("s1:int".into(), Kind::Slot),
                ("s2:int".into(), Kind::Slot),
            ]
        );
    }

    /// A layout named after its slots is a type; a callee is a name, and the
    /// two are told apart by the argument list the printer writes after one
    /// of them and never after the other.
    ///
    /// Both are in this one line: `playground.greeting` is the callee and the
    /// `String` after the arguments is the layout of what the call answers,
    /// so a reader that told them apart by "a name in an instruction is a
    /// layout" would colour one of them wrong.
    #[test]
    fn a_callee_is_a_name_and_everything_else_that_is_named_is_a_layout() {
        assert_eq!(
            lit("     1  call s4:ref playground.greeting (s3:String) String\n"),
            vec![
                ("1".into(), Kind::Number),
                ("call".into(), Kind::Keyword),
                ("s4:ref".into(), Kind::Slot),
                ("s3:String".into(), Kind::Slot),
                ("String".into(), Kind::Type),
            ]
        );
        assert_eq!(
            lit("     5  copy s1:ref s4:ref String\n").last(),
            Some(&("String".to_string(), Kind::Type))
        );
    }

    #[test]
    fn a_string_literal_is_one_piece_spaces_escapes_and_all() {
        assert_eq!(
            lit("     0  str s2:ref \"Hello, \\\"you\\\"!\"\n").last(),
            Some(&("\"Hello, \\\"you\\\"!\"".to_string(), Kind::Str))
        );
    }

    /// `alloc` writes the shape against its layout and then a count that is
    /// either a number or a slot. The `x` is a mark on the count and not a
    /// name, which is the one place a bare letter appears in an operand.
    #[test]
    fn an_alloc_carries_a_shape_and_a_count() {
        assert_eq!(
            lit("    12  alloc s14:ref Array<array> x3\n"),
            vec![
                ("12".into(), Kind::Number),
                ("alloc".into(), Kind::Keyword),
                ("s14:ref".into(), Kind::Slot),
                ("Array".into(), Kind::Type),
                ("3".into(), Kind::Number),
            ]
        );
        assert_eq!(
            lit("   170  alloc s10:ref Array<array> xs4:int\n").last(),
            Some(&("s4:int".to_string(), Kind::Slot))
        );
    }

    /// A layout the table names in brackets, which is what a bare `ref` or
    /// `addr` is called. It is a layout name and not a shape, and the two are
    /// told apart by whether a name is written against the bracket.
    #[test]
    fn a_bracketed_layout_is_a_layout_and_a_bracketed_shape_is_not() {
        assert_eq!(
            lit("    18  clear s13:ref <ref>\n").last(),
            Some(&("<ref>".to_string(), Kind::Type))
        );
        // A layout name with a space in it — `closure playground.reading#0`
        // is one name — is coloured as the layout it is, both halves of it,
        // and the shape written against it is not.
        assert_eq!(
            lit("     1  alloc s12:ref closure playground.reading#0<closure>\n"),
            vec![
                ("1".into(), Kind::Number),
                ("alloc".into(), Kind::Keyword),
                ("s12:ref".into(), Kind::Slot),
                ("closure".into(), Kind::Type),
                ("playground.reading#0".into(), Kind::Type),
            ]
        );
    }

    #[test]
    fn a_switch_keeps_its_table_apart_from_its_default() {
        assert_eq!(
            lit("     0  switch s0:int [1 7 12] else 15\n"),
            vec![
                ("0".into(), Kind::Number),
                ("switch".into(), Kind::Keyword),
                ("s0:int".into(), Kind::Slot),
                ("1".into(), Kind::Number),
                ("7".into(), Kind::Number),
                ("12".into(), Kind::Number),
                ("else".into(), Kind::Keyword),
                ("15".into(), Kind::Number),
            ]
        );
    }

    #[test]
    fn an_immediate_is_a_number_whatever_it_is_spelled_like() {
        assert_eq!(
            lit("     3  int s1:int -21\n").last(),
            Some(&("-21".to_string(), Kind::Number))
        );
        assert_eq!(
            lit("     4  float s2:float 1.5\n").last(),
            Some(&("1.5".to_string(), Kind::Number))
        );
        assert_eq!(
            lit("     5  bool s3:bool true\n").last(),
            Some(&("true".to_string(), Kind::Keyword))
        );
        assert_eq!(
            lit("     6  store-field s1:ref +2 s0:int Int\n")
                .iter()
                .map(|(text, _)| text.as_str())
                .collect::<Vec<_>>(),
            vec!["6", "store-field", "s1:ref", "+2", "s0:int", "Int"]
        );
    }

    /// The signal a check reads. A line the printer never wrote is coloured
    /// as nothing rather than guessed at, and the whole answer says so.
    #[test]
    fn a_line_this_reader_does_not_know_is_left_plain_and_reported() {
        let text = "fn0 playground.main() -> Int\n  something new\n     0  unit s0:unit\n";
        let painting = disassembly(text);
        assert!(!painting.ok);
        tiles(text, &painting);
        assert!(
            painting
                .pieces
                .iter()
                .any(|piece| piece.kind == Kind::Keyword),
            "the lines it did know are still coloured"
        );
    }

    #[test]
    fn an_empty_disassembly_is_an_empty_tiling() {
        let painting = disassembly("");
        assert!(painting.ok);
        assert!(painting.pieces.is_empty());
    }

    /// The whole of a small program, which is what a reader actually sees:
    /// the pieces tile, the blank line between two functions is a line like
    /// any other, and nothing in it went unrecognised.
    #[test]
    fn a_whole_disassembly_tiles_and_is_understood() {
        let text = "fn0 playground.twice(Int) -> Int\n\
                    \x20 frame 3: s0!:int s1:int s2:int\n\
                    \x20 local n -> s0:Int [0, 3)\n\
                    \x20    0  add.int s2:int s0:int s0:int\n\
                    \x20    1  copy s1:int s2:int Int\n\
                    \x20    2  return s1:int Int\n\
                    \n\
                    fn1 playground.main() -> Int\n\
                    \x20 frame 2: s0:int s1:int\n\
                    \x20    0  int s1:int 21\n\
                    \x20    1  call s0:int playground.twice (s1:Int) Int\n\
                    \x20    2  return s0:int Int\n";
        let painting = disassembly(text);
        assert!(painting.ok, "{:?}", painting.pieces);
        tiles(text, &painting);
        for pair in painting.pieces.windows(2) {
            assert_ne!(pair[0].kind, pair[1].kind, "{:?}", painting.pieces);
        }
    }
}
