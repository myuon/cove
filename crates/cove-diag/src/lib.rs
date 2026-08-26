//! Source positions and compiler diagnostics shared by every Cove component.
//!
//! ADR 0001 treats diagnostics as part of the learning interface: an error
//! states the Cove rule it enforces, points at the source, and shows a textual
//! correction when one is unambiguous.

use std::fmt;
use std::path::{Path, PathBuf};

/// Identifies one source file inside a [`SourceMap`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FileId(pub u32);

/// A half-open byte range `[start, end)` inside a single file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub file: FileId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn new(file: FileId, start: u32, end: u32) -> Self {
        Span { file, start, end }
    }

    /// The smallest span covering both `self` and `other`.
    pub fn to(self, other: Span) -> Span {
        debug_assert_eq!(self.file, other.file);
        Span {
            file: self.file,
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// A value paired with the source range it came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Spanned<T> {
    pub node: T,
    pub span: Span,
}

impl<T> Spanned<T> {
    pub fn new(node: T, span: Span) -> Self {
        Spanned { node, span }
    }
}

/// One loaded source file.
pub struct SourceFile {
    pub id: FileId,
    pub path: PathBuf,
    pub text: String,
    /// Byte offset of the start of each line.
    line_starts: Vec<u32>,
}

impl SourceFile {
    /// 1-based line and column (in characters) for a byte offset.
    pub fn line_col(&self, offset: u32) -> (usize, usize) {
        let line = match self.line_starts.binary_search(&offset) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let line_start = self.line_starts[line] as usize;
        let col = self.text[line_start..offset as usize].chars().count() + 1;
        (line + 1, col)
    }

    /// Text of the 1-based `line`, without its terminator.
    pub fn line_text(&self, line: usize) -> &str {
        let start = self.line_starts[line - 1] as usize;
        let end = self
            .line_starts
            .get(line)
            .map(|&e| e as usize)
            .unwrap_or(self.text.len());
        self.text[start..end].trim_end_matches(['\n', '\r'])
    }
}

/// Owns every source file the compiler has read.
#[derive(Default)]
pub struct SourceMap {
    files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn new() -> Self {
        SourceMap::default()
    }

    pub fn add(&mut self, path: impl Into<PathBuf>, text: impl Into<String>) -> FileId {
        let id = FileId(self.files.len() as u32);
        let text = text.into();
        let mut line_starts = vec![0u32];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        self.files.push(SourceFile {
            id,
            path: path.into(),
            text,
            line_starts,
        });
        id
    }

    pub fn get(&self, id: FileId) -> &SourceFile {
        &self.files[id.0 as usize]
    }

    pub fn path(&self, id: FileId) -> &Path {
        &self.files[id.0 as usize].path
    }

    pub fn files(&self) -> impl Iterator<Item = &SourceFile> {
        self.files.iter()
    }
}

/// How seriously a diagnostic should be taken.
///
/// The three are ordered by what they ask of a reader. An error is a program
/// the toolchain refuses. A warning is a program it accepts and doubts, so
/// `cove check --deny-warnings` refuses it instead. A note is a program it
/// accepts and does not doubt: the compiler is saying what it deliberately
/// did not prove, which is a fact about the language rather than a suspicion
/// about this program, so no strictness setting turns one into a failure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Error => f.write_str("error"),
            Severity::Warning => f.write_str("warning"),
            Severity::Note => f.write_str("note"),
        }
    }
}

/// A secondary source location that explains the primary one.
#[derive(Clone, Debug)]
pub struct Label {
    pub span: Span,
    pub message: String,
}

/// A compiler or runtime message about a specific place in source.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub severity: Severity,
    /// Stable identifier such as `cove::parse::unexpected_token`.
    pub code: String,
    pub message: String,
    pub primary: Option<Span>,
    pub labels: Vec<Label>,
    /// The Cove rule this diagnostic enforces, in one sentence.
    pub rule: Option<String>,
    /// An unambiguous textual correction, when one exists.
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Error,
            code: code.into(),
            message: message.into(),
            primary: None,
            labels: Vec::new(),
            rule: None,
            help: None,
        }
    }

    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            ..Diagnostic::error(code, message)
        }
    }

    /// Something the toolchain deliberately did not check, said out loud.
    ///
    /// A note is not a complaint. It reports a place where a rule of the
    /// language, or a promise a schema made, leaves the compiler nothing to
    /// prove, so that a run which succeeds still says which of its silences
    /// were chosen ones.
    pub fn note(code: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic {
            severity: Severity::Note,
            ..Diagnostic::error(code, message)
        }
    }

    pub fn at(mut self, span: Span) -> Self {
        self.primary = Some(span);
        self
    }

    pub fn label(mut self, span: Span, message: impl Into<String>) -> Self {
        self.labels.push(Label {
            span,
            message: message.into(),
        });
        self
    }

    pub fn rule(mut self, rule: impl Into<String>) -> Self {
        self.rule = Some(rule.into());
        self
    }

    pub fn help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}

/// Collects diagnostics produced by one compilation.
#[derive(Default)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Diagnostics::default()
    }

    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.items.push(diagnostic);
    }

    pub fn extend(&mut self, other: impl IntoIterator<Item = Diagnostic>) {
        self.items.extend(other);
    }

    pub fn has_errors(&self) -> bool {
        self.items.iter().any(|d| d.severity == Severity::Error)
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.items.iter()
    }

    pub fn into_vec(self) -> Vec<Diagnostic> {
        self.items
    }
}

/// Renders a diagnostic as plain text with a source excerpt.
pub fn render(sources: &SourceMap, diagnostic: &Diagnostic) -> String {
    let mut out = format!(
        "{}[{}]: {}\n",
        diagnostic.severity, diagnostic.code, diagnostic.message
    );

    if let Some(span) = diagnostic.primary {
        out.push_str(&render_span(sources, span, None));
    }
    for label in &diagnostic.labels {
        out.push_str(&render_span(sources, label.span, Some(&label.message)));
    }
    if let Some(rule) = &diagnostic.rule {
        out.push_str(&format!("  rule: {rule}\n"));
    }
    if let Some(help) = &diagnostic.help {
        for (i, line) in help.lines().enumerate() {
            if i == 0 {
                out.push_str(&format!("  help: {line}\n"));
            } else {
                out.push_str(&format!("        {line}\n"));
            }
        }
    }
    out
}

fn render_span(sources: &SourceMap, span: Span, message: Option<&str>) -> String {
    let file = sources.get(span.file);
    let (line, col) = file.line_col(span.start);
    let text = file.line_text(line);
    let gutter = line.to_string();
    let pad = " ".repeat(gutter.len());
    let width = {
        let (end_line, end_col) = file.line_col(span.end);
        if end_line == line {
            (end_col - col).max(1)
        } else {
            text.chars().count().saturating_sub(col - 1).max(1)
        }
    };

    let mut out = format!("{pad}--> {}:{line}:{col}\n", file.path.display());
    out.push_str(&format!("{pad} |\n"));
    out.push_str(&format!("{gutter} | {text}\n"));
    out.push_str(&format!(
        "{pad} | {}{}{}\n",
        " ".repeat(col - 1),
        "^".repeat(width),
        match message {
            Some(m) => format!(" {m}"),
            None => String::new(),
        }
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_counts_from_one() {
        let mut map = SourceMap::new();
        let id = map.add("a.cove", "let x = 1\nlet y = 2\n");
        let file = map.get(id);
        assert_eq!(file.line_col(0), (1, 1));
        assert_eq!(file.line_col(10), (2, 1));
        assert_eq!(file.line_text(2), "let y = 2");
    }

    #[test]
    fn render_points_at_the_span() {
        let mut map = SourceMap::new();
        let id = map.add("a.cove", "let x = 1\n");
        let d = Diagnostic::error("cove::test::demo", "example")
            .at(Span::new(id, 4, 5))
            .rule("`let` creates a read-only place.");
        let text = render(&map, &d);
        assert!(text.contains("error[cove::test::demo]: example"));
        assert!(text.contains("a.cove:1:5"));
        assert!(text.contains("^"));
    }
}
