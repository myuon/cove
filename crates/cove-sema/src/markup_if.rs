//! The two `if`s that choose markup and have a better spelling,
//! [ADR 0105](../../../docs/adr/0105-an-if-that-chooses-markup-says-what-it-chooses.md):
//! an `if`/`else` one of whose branches is `html""`, which is `html.only`,
//! and an `if`/`else` whose branches are `html` literals that differ only in
//! text between the same markup, which is one literal with the `if` in a
//! hole — or by something one of them adds, which is `html.only` of it in a
//! hole.
//!
//! What is here is the comparison and the rewrite, over a literal already
//! reduced to [`Atom`]s: the checker decides which `if`s qualify — exactly
//! an `if` and an `else`, branch types — and what each hole's source and type
//! are. Nothing here reads the markup. A literal is compared as text, a
//! character at a time, the cut between what is common and what differs
//! is placed by `<` and `>`, and a differing part is looked at for five
//! characters, which is the line
//! [ADR 0103](../../../docs/adr/0103-in-html-a-line-break-is-spacing.md)
//! drew.

/// An `if` one of whose branches is `html""` while the other is markup.
pub(crate) const EMPTY_BRANCH: &str = "cove::type::html_if_empty_branch";
/// An `if` whose two branches are `html` literals that differ only in text.
pub(crate) const SAME_MARKUP: &str = "cove::type::html_if_same_markup";

/// The characters a differing part may not hold for the `if` to move into a
/// hole: a hole escapes exactly these, so a part holding none of them is
/// written the same as text in the literal and as a `String` in a hole — and
/// a `<` or `>` would mean the branches differ in markup.
const ESCAPED: [char; 5] = ['<', '>', '&', '"', '\''];

/// Past this many characters, a common beginning or end is shown with `…`.
const ELIDE_OVER: usize = 60;
/// How much of an elided beginning or end is still shown, next to the part
/// that moves.
const ELIDED_KEEPS: usize = 30;

/// One piece of an `html` literal, as the comparison sees it: a character of
/// its text, or a hole.
#[derive(Clone, Debug)]
pub(crate) enum Atom {
    Char(char),
    Hole {
        /// The hole's expression as written, which is what two holes are
        /// compared by.
        source: String,
        /// Whether the hole's type puts markup in — `Html`, `Array<Html>`,
        /// an `Option` of either — or is not known well enough to say it
        /// does not.
        markup: bool,
    },
}

impl Atom {
    fn same(&self, other: &Atom) -> bool {
        match (self, other) {
            (Atom::Char(a), Atom::Char(b)) => a == b,
            (Atom::Hole { source: a, .. }, Atom::Hole { source: b, .. }) => a == b,
            _ => false,
        }
    }

    /// Whether this may move out of the literal into a `String`.
    fn movable(&self) -> bool {
        match self {
            Atom::Char(c) => !ESCAPED.contains(c),
            Atom::Hole { markup, .. } => !markup,
        }
    }

    /// The atom as it is written inside a string literal's quotes.
    fn written(&self) -> String {
        match self {
            Atom::Char(c) => escaped(*c),
            Atom::Hole { source, .. } => format!("{{{source}}}"),
        }
    }
}

fn escaped(c: char) -> String {
    match c {
        '\\' => "\\\\".to_string(),
        '"' => "\\\"".to_string(),
        '\n' => "\\n".to_string(),
        '\t' => "\\t".to_string(),
        '\r' => "\\r".to_string(),
        '\0' => "\\0".to_string(),
        '{' => "\\{".to_string(),
        '}' => "\\}".to_string(),
        c => c.to_string(),
    }
}

fn written(atoms: &[Atom]) -> String {
    atoms.iter().map(Atom::written).collect()
}

/// How the two literals of an `if` split: the beginning and end they have in
/// common, and the part of each that differs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Split {
    /// How many atoms both begin with.
    pub prefix: usize,
    /// How many atoms both end with, after the common beginning: the two
    /// never overlap.
    pub suffix: usize,
}

/// The longest common beginning of `a` and `b`, then the longest common end
/// of what is left of both.
pub(crate) fn split(a: &[Atom], b: &[Atom]) -> Split {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x.same(y)).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x.same(y))
        .count();
    Split { prefix, suffix }
}

/// The help for an `if` one of whose branches is `html""`: `condition` is
/// already negated when the empty branch was the `if` branch, and `markup`
/// is the other branch as written, or `None` when it is a block with
/// statements — which would not read on one line inside a call.
pub(crate) fn only_help(condition: &str, markup: Option<&str>) -> String {
    let rest = "— or, if that markup is only safe to build when the condition holds, an `Option` \
                hole (ADR 0104)";
    match markup {
        Some(markup) => format!("write `html.only({condition}, {markup})` {rest}"),
        None => format!(
            "move this branch into a function answering `Html`, then write \
             `html.only({condition}, thatFunction(…))` {rest}"
        ),
    }
}

/// The help for an `if` whose branches are the two `html` literals `a` (the
/// `if` branch) and `b` (the `else`), or `None` when the `if` does not have
/// the shape. `condition` is the condition as written and `negated` its
/// negation.
///
/// When one literal is the other with something added, and what is added
/// stands apart from the words beside it, the help is `html.only` of what is
/// added, in a hole where it goes. Otherwise the
/// common beginning is cut back to end at a tag's `>` and the common end to
/// start at a `<` — so the part that moves is never half a run of text —
/// and the help is a choice between two `String`s in a hole, unless what
/// differs holds one of the five characters a hole escapes, or a hole that
/// puts markup in.
pub(crate) fn same_markup_help(
    condition: &str,
    negated: &str,
    a: &[Atom],
    b: &[Atom],
) -> Option<(Shape, String)> {
    let Split { prefix, suffix } = split(a, b);
    let middle_a = &a[prefix..a.len() - suffix];
    let middle_b = &b[prefix..b.len() - suffix];
    if middle_a.is_empty() && middle_b.is_empty() {
        let whole = written(a);
        let shown = if whole.chars().count() > ELIDE_OVER {
            String::new()
        } else {
            format!(": `html\"{whole}\"`")
        };
        return Some((
            Shape::Differs,
            format!(
                "both branches are the same markup, so the `if` can go and either branch stand \
                 in its place{shown}"
            ),
        ));
    }
    if middle_a.is_empty() || middle_b.is_empty() {
        let (longer, condition) = if middle_a.is_empty() {
            (b, negated)
        } else {
            (a, condition)
        };
        if let Some(at) = insertion(longer, prefix, longer.len() - prefix - suffix) {
            let added = &longer[at..at + longer.len() - prefix - suffix];
            let before = elided_end(&longer[..at]);
            let after = elided_start(&longer[at + added.len()..]);
            return Some((
                Shape::Added,
                format!(
                    "add it with `html.only` where it goes: \
                     `html\"{before}{{html.only({condition}, html\"{}\")}}{after}\"`",
                    written(added),
                ),
            ));
        }
    }
    let prefix = at_a_tag_end(&a[..prefix]);
    let suffix = suffix - at_a_tag_start(&a[a.len() - suffix..]);
    let middle_a = &a[prefix..a.len() - suffix];
    let middle_b = &b[prefix..b.len() - suffix];
    if !middle_a.iter().chain(middle_b).all(Atom::movable) {
        return None;
    }
    let before = elided_end(&a[..prefix]);
    let after = elided_start(&a[a.len() - suffix..]);
    Some((
        Shape::Differs,
        format!(
            "put the `if` where they differ: `html\"{before}{{if {condition} {{ \"{}\" }} else {{ \"{}\" }}}}{after}\"`",
            written(middle_a),
            written(middle_b),
        ),
    ))
}

/// Which of B's two shapes an `if` has, which the message names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// The two literals differ in text between the same markup, or not at
    /// all.
    Differs,
    /// One literal is the other with something added.
    Added,
}

fn is_char(atom: &Atom, c: char) -> bool {
    matches!(atom, Atom::Char(x) if *x == c)
}

fn is_bracket(atom: &Atom) -> bool {
    is_char(atom, '<') || is_char(atom, '>')
}

/// Whether `outside` and `inside`, the atoms either side of one edge of
/// something added, meet at a boundary rather than inside a word: one of
/// them is whitespace, a hole, `<` or `>`, or the one outside is the `"`
/// around an attribute value.
fn boundary(outside: Option<&Atom>, inside: &Atom) -> bool {
    let Some(outside) = outside else {
        return true;
    };
    let open = |atom: &Atom| match atom {
        Atom::Char(c) => c.is_whitespace() || *c == '<' || *c == '>',
        Atom::Hole { .. } => true,
    };
    open(outside) || open(inside) || is_char(outside, '"')
}

/// Where the `count` atoms that `longer` holds at `at` and the other literal
/// does not are taken to start, or `None` when no reading of them is whole
/// markup or plain text standing apart from the words beside it. The
/// comparison puts an insertion as late as it can go, so `<p>a</p>` against
/// `<p>a<br></p>` finds `br><`, and `<p>an</p>` against `<p>a an</p>` finds
/// ` a`, which ends inside `an`; sliding it back while the atom before it is
/// the same as its last finds `<br>` and `a `. An insertion inside a word —
/// `'` into `its` — is not one, and is left to the other shape.
fn insertion(longer: &[Atom], mut at: usize, count: usize) -> Option<usize> {
    let whole = |at: usize| {
        let added = &longer[at..at + count];
        let markup = !added.iter().any(is_bracket)
            || (is_char(&added[0], '<') && is_char(&added[count - 1], '>'));
        markup
            && boundary(at.checked_sub(1).map(|i| &longer[i]), &added[0])
            && boundary(longer.get(at + count), &added[count - 1])
    };
    loop {
        if whole(at) {
            return Some(at);
        }
        if at == 0 || !longer[at - 1].same(&longer[at + count - 1]) {
            return None;
        }
        at -= 1;
    }
}

/// How much of a common beginning to keep so that it does not stop in the
/// middle of a run of text: all of it when it stops inside a tag, which is
/// not text; otherwise up to just after its last `>`, or none of it.
fn at_a_tag_end(prefix: &[Atom]) -> usize {
    match prefix.iter().rposition(is_bracket) {
        Some(at) if is_char(&prefix[at], '<') => prefix.len(),
        Some(at) => at + 1,
        None => 0,
    }
}

/// How many atoms to drop from the front of a common end so that it does
/// not start in the middle of a run of text: none when it starts inside a
/// tag; otherwise everything before its first `<`, or all of it.
fn at_a_tag_start(suffix: &[Atom]) -> usize {
    match suffix.iter().position(is_bracket) {
        Some(at) if is_char(&suffix[at], '>') => 0,
        Some(at) => at,
        None => suffix.len(),
    }
}

/// A common beginning as written, with all but its last few characters
/// elided when it is long.
fn elided_end(atoms: &[Atom]) -> String {
    let whole = written(atoms);
    if whole.chars().count() <= ELIDE_OVER {
        return whole;
    }
    let mut kept = Vec::new();
    let mut length = 0;
    for atom in atoms.iter().rev() {
        let text = atom.written();
        length += text.chars().count();
        if length > ELIDED_KEEPS && !kept.is_empty() {
            break;
        }
        kept.push(text);
    }
    kept.reverse();
    format!("…{}", kept.concat())
}

/// A common end as written, with all but its first few characters elided
/// when it is long.
fn elided_start(atoms: &[Atom]) -> String {
    let whole = written(atoms);
    if whole.chars().count() <= ELIDE_OVER {
        return whole;
    }
    let mut kept = String::new();
    let mut length = 0;
    for atom in atoms {
        let text = atom.written();
        length += text.chars().count();
        if length > ELIDED_KEEPS && !kept.is_empty() {
            break;
        }
        kept.push_str(&text);
    }
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A literal's atoms from a spelling where `{name}` is a hole of a plain
    /// type and `{!name}` a hole that puts markup in.
    fn atoms(text: &str) -> Vec<Atom> {
        let mut out = Vec::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '{' {
                let source: String = chars.by_ref().take_while(|&c| c != '}').collect();
                let (markup, source) = match source.strip_prefix('!') {
                    Some(rest) => (true, rest.to_string()),
                    None => (false, source),
                };
                out.push(Atom::Hole { source, markup });
            } else {
                out.push(Atom::Char(c));
            }
        }
        out
    }

    fn help(a: &str, b: &str) -> Option<String> {
        same_markup_help("up", "!up", &atoms(a), &atoms(b)).map(|(_, help)| help)
    }

    #[test]
    fn the_common_beginning_and_end_are_found_on_characters_and_holes() {
        assert_eq!(
            split(
                &atoms("<span class=state>up</span>"),
                &atoms("<span class=state>down</span>")
            ),
            Split {
                prefix: 18,
                suffix: 7
            }
        );
        // A hole is common when it is written the same, wherever it sits.
        assert_eq!(
            split(&atoms("<b>{x}a{y}</b>"), &atoms("<b>{x}b{y}</b>")),
            Split {
                prefix: 4,
                suffix: 5
            }
        );
        // Two holes written differently are not common.
        assert_eq!(
            split(&atoms("{x}"), &atoms("{y}")),
            Split {
                prefix: 0,
                suffix: 0
            }
        );
    }

    #[test]
    fn the_common_beginning_and_end_do_not_overlap() {
        // `aa` against `aaa`: both begin with `aa`, and nothing is left of
        // the first to be a common end.
        assert_eq!(
            split(&atoms("aa"), &atoms("aaa")),
            Split {
                prefix: 2,
                suffix: 0
            }
        );
        assert_eq!(
            split(&atoms("<b>x</b>"), &atoms("<b>xx</b>")),
            Split {
                prefix: 4,
                suffix: 4
            }
        );
    }

    #[test]
    fn a_word_between_the_same_markup_moves_into_the_hole() {
        assert_eq!(
            help(
                "<span class=state>up</span>",
                "<span class=state>down</span>"
            )
            .unwrap(),
            "put the `if` where they differ: \
             `html\"<span class=state>{if up { \"up\" } else { \"down\" }}</span>\"`"
        );
    }

    #[test]
    fn a_difference_inside_a_quoted_attribute_value_moves_without_its_quotes() {
        assert_eq!(
            help("<a class=\"a\">x</a>", "<a class=\"b\">x</a>").unwrap(),
            "put the `if` where they differ: \
             `html\"<a class=\\\"{if up { \"a\" } else { \"b\" }}\\\">x</a>\"`"
        );
    }

    #[test]
    fn a_hole_of_a_plain_type_moves_and_stays_a_hole() {
        assert_eq!(
            help("<p>{count} up</p>", "<p>{name} down</p>").unwrap(),
            "put the `if` where they differ: \
             `html\"<p>{if up { \"{count} up\" } else { \"{name} down\" }}</p>\"`"
        );
    }

    #[test]
    fn what_one_side_adds_is_html_only_where_it_goes() {
        assert_eq!(
            help("<b>x</b>", "<b></b>").unwrap(),
            "add it with `html.only` where it goes: `html\"<b>{html.only(up, html\"x\")}</b>\"`"
        );
        // Added inside a tag, it is still markup, so it stays `html`.
        assert_eq!(
            help("<ul class=tree>", "<ul>").unwrap(),
            "add it with `html.only` where it goes: `html\"<ul{html.only(up, html\" class=tree\")}>\"`"
        );
        // Added by the `else`, under the negated condition.
        assert_eq!(
            help("<ul>", "<ul class=tree>").unwrap(),
            "add it with `html.only` where it goes: `html\"<ul{html.only(!up, html\" class=tree\")}>\"`"
        );
        // What is added may hold any character, since it stays markup.
        assert!(help("<p>a &amp; b</p>", "<p>a</p>")
            .unwrap()
            .contains("{html.only(up, html\" &amp; b\")}"),);
    }

    #[test]
    fn an_added_tag_is_found_whole() {
        // The comparison alone finds `br><` between `<p>a<` and `/p>`.
        assert_eq!(
            help("<p>a</p>", "<p>a<br></p>").unwrap(),
            "add it with `html.only` where it goes: `html\"<p>a{html.only(!up, html\"<br>\")}</p>\"`"
        );
        // An addition that is text and part of a tag, however it is slid,
        // is no one thing, and is left alone.
        assert_eq!(help("<p></p>", "<p>x<b></p>"), None);
    }

    #[test]
    fn an_addition_is_found_at_a_word_boundary() {
        assert_eq!(
            help("<p>up</p>", "<p>up now</p>").unwrap(),
            "add it with `html.only` where it goes: `html\"<p>up{html.only(!up, html\" now\")}</p>\"`"
        );
        // ` a` ends inside `an`; slid back, `a ` stands apart.
        assert_eq!(
            help("<p>an</p>", "<p>a an</p>").unwrap(),
            "add it with `html.only` where it goes: `html\"<p>{html.only(!up, html\"a \")}an</p>\"`"
        );
        // An element appended after a word: an edge at a tag is a boundary.
        assert_eq!(
            help("<p>a<span class=muted>b</span></p>", "<p>a</p>").unwrap(),
            "add it with `html.only` where it goes: \
             `html\"<p>a{html.only(up, html\"<span class=muted>b</span>\")}</p>\"`"
        );
        // A word added inside a quoted attribute value.
        assert_eq!(
            help("<a class=\"a b\">x</a>", "<a class=\"a\">x</a>").unwrap(),
            "add it with `html.only` where it goes: \
             `html\"<a class=\\\"a{html.only(up, html\" b\")}\\\">x</a>\"`"
        );
    }

    #[test]
    fn an_addition_inside_a_word_is_not_html_only() {
        // The whole words are chosen between instead.
        assert_eq!(
            help("<p>cat</p>", "<p>cart</p>").unwrap(),
            "put the `if` where they differ: `html\"<p>{if up { \"cat\" } else { \"cart\" }}</p>\"`"
        );
        // And `it's` holds a `'`, which may not move into a hole.
        assert_eq!(help("<p>it's</p>", "<p>its</p>"), None);
    }

    #[test]
    fn a_difference_holding_an_escaped_character_is_left_alone() {
        for (a, b) in [
            ("<b>x</b>", "<i>x</i>"),
            ("<p>a &amp; b</p>", "<p>a | b</p>"),
            ("<p>say \"hi\"</p>", "<p>hi</p>"),
            ("<p>it's on</p>", "<p>its off</p>"),
        ] {
            assert_eq!(help(a, b), None, "{a} against {b}");
        }
    }

    #[test]
    fn a_difference_holding_a_markup_hole_is_left_alone() {
        assert_eq!(help("<p>{!icon} up</p>", "<p>down</p>"), None);
        // A hole in a run of text is part of the text, so a markup hole the
        // two share moves with the words beside it, and keeps the `if` out.
        assert_eq!(help("<p>{!icon} up</p>", "<p>{!icon} down</p>"), None);
        // Between tags of its own it is common, and stays where it is.
        assert!(help("<p>{!icon}<b>up</b></p>", "<p>{!icon}<b>down</b></p>").is_some());
    }

    #[test]
    fn a_run_of_text_moves_whole() {
        // The two sentences share a beginning and a final `.`; neither is cut.
        assert_eq!(
            help("<p>The service is up.</p>", "<p>The service is down.</p>").unwrap(),
            "put the `if` where they differ: `html\"<p>{if up { \"The service is up.\" } \
             else { \"The service is down.\" }}</p>\"`"
        );
        // Text after a tag inside the same element moves from that tag on.
        assert_eq!(
            help(
                "<p><b>Note:</b> it is up.</p>",
                "<p><b>Note:</b> it is down.</p>"
            )
            .unwrap(),
            "put the `if` where they differ: `html\"<p><b>Note:</b>{if up { \" it is up.\" } \
             else { \" it is down.\" }}</p>\"`"
        );
    }

    #[test]
    fn identical_literals_say_the_if_can_go() {
        assert_eq!(
            help("<b>x</b>", "<b>x</b>").unwrap(),
            "both branches are the same markup, so the `if` can go and either branch stand in \
             its place: `html\"<b>x</b>\"`"
        );
    }

    #[test]
    fn a_long_common_beginning_and_end_are_elided_and_the_moved_part_is_not() {
        let head = format!("<div class=wide>{}<span>", "x".repeat(70));
        let tail = format!("</span>{}</div>", "y".repeat(70));
        let shown = help(&format!("{head}on{tail}"), &format!("{head}off{tail}")).unwrap();
        assert!(
            shown.contains("{if up { \"on\" } else { \"off\" }}"),
            "{shown}"
        );
        assert!(shown.contains("`html\"…"), "{shown}");
        assert!(shown.contains("…\"`"), "{shown}");
        assert!(!shown.contains(&"x".repeat(70)), "{shown}");
    }

    #[test]
    fn only_names_the_condition_and_the_markup() {
        assert_eq!(
            only_help("app.isAdmin", Some("html\"<b>x</b>\"")),
            "write `html.only(app.isAdmin, html\"<b>x</b>\")` — or, if that markup is only \
             safe to build when the condition holds, an `Option` hole (ADR 0104)"
        );
        assert_eq!(
            only_help("app.isAdmin", None),
            "move this branch into a function answering `Html`, then write \
             `html.only(app.isAdmin, thatFunction(…))` — or, if that markup is only safe to \
             build when the condition holds, an `Option` hole (ADR 0104)"
        );
    }

    /// What `cove check` notes about `body`, the body of a function in a
    /// module that imports `std.html`, with `up: Bool`, `n: Int`,
    /// `name: String`, `icon: Html`, `icons: Array<Html>` and
    /// `note: Option<Html>` in scope — as `(code, message, help)`.
    fn notes(body: &str) -> Vec<(String, String, String)> {
        use std::collections::BTreeMap;
        use std::path::PathBuf;

        use cove_diag::{Severity, SourceMap};

        use crate::config::Config;
        use crate::package::{Module, Package, Unit};

        let source = format!(
            "use std.html\n\n\
             fn page(up: Bool, n: Int, name: String, icon: html.Html, icons: Array<html.Html>, \
             note: Option<html.Html>) -> html.Html {{\n  {body}\n}}\n"
        );
        let mut sources = SourceMap::new();
        let path = PathBuf::from("app/main.cove");
        let file = sources.add(path.clone(), source);
        let ast = cove_syntax::parse_file(&sources, file).expect("the fixture parses");
        let mut modules = BTreeMap::from([(
            "app".to_string(),
            Module {
                name: "app".to_string(),
                dir: PathBuf::from("app"),
                units: vec![Unit { file, path, ast }],
            },
        )]);
        modules.extend(crate::stdlib::attach(&mut sources).expect("stdlib parses"));
        let package = Package {
            root: PathBuf::new(),
            config: Config::default(),
            modules,
        };
        let program = crate::resolve::resolve(&package).expect("the fixture resolves");
        let diagnostics = crate::typeck::check(&package, &program);
        let errors: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|d| d.message.clone())
            .collect();
        assert!(errors.is_empty(), "{body}: {errors:?}");
        diagnostics
            .into_iter()
            .filter(|d| d.severity == Severity::Note)
            .map(|d| (d.code, d.message, d.help.unwrap_or_default()))
            .collect()
    }

    #[track_caller]
    fn noted(body: &str, code: &str) -> String {
        let found = notes(body);
        assert_eq!(found.len(), 1, "{body}: {found:?}");
        assert_eq!(found[0].0, code, "{body}: {found:?}");
        found[0].2.clone()
    }

    #[track_caller]
    fn quiet(body: &str) {
        let found = notes(body);
        assert!(found.is_empty(), "{body}: {found:?}");
    }

    #[test]
    fn an_empty_else_is_html_only() {
        let help = noted(
            r#"if up { html"<b>{n}</b>" } else { html"" }"#,
            EMPTY_BRANCH,
        );
        assert_eq!(help, only_help("up", Some(r#"html"<b>{n}</b>""#)));
    }

    #[test]
    fn an_empty_if_branch_is_html_only_of_the_negated_condition() {
        let help = noted(r#"if up { html"" } else { icon }"#, EMPTY_BRANCH);
        assert_eq!(help, only_help("!up", Some("icon")));
        let help = noted(r#"if up && n > 1 { html"" } else { icon }"#, EMPTY_BRANCH);
        assert_eq!(help, only_help("!(up && n > 1)", Some("icon")));
        let help = noted(
            r#"if name.isEmpty() { html"" } else { icon }"#,
            EMPTY_BRANCH,
        );
        assert_eq!(help, only_help("!name.isEmpty()", Some("icon")));
    }

    #[test]
    fn a_negation_is_not_negated_again() {
        let help = noted(r#"if !up { html"" } else { icon }"#, EMPTY_BRANCH);
        assert_eq!(help, only_help("up", Some("icon")));
        let help = noted(
            r#"if !(up && n > 1) { html"" } else { icon }"#,
            EMPTY_BRANCH,
        );
        assert_eq!(help, only_help("(up && n > 1)", Some("icon")));
    }

    #[test]
    fn a_comparison_is_negated_by_its_opposite() {
        for (written, negated) in [
            ("n > 1", "n <= 1"),
            ("n >= 1", "n < 1"),
            ("n < 1", "n >= 1"),
            ("n <= 1", "n > 1"),
            ("n == 1", "n != 1"),
            (r#"name != """#, r#"name == """#),
        ] {
            let help = noted(
                &format!(r#"if {written} {{ html"" }} else {{ icon }}"#),
                EMPTY_BRANCH,
            );
            assert_eq!(help, only_help(negated, Some("icon")), "{written}");
        }
        // A `Float` is not totally ordered: `!(x < 1.0)` holds for a NaN and
        // `x >= 1.0` does not, so the ordering is kept and wrapped.
        let help = noted(
            r#"let x = 0.5
  if x < 1.0 { html"" } else { icon }"#,
            EMPTY_BRANCH,
        );
        assert_eq!(help, only_help("!(x < 1.0)", Some("icon")));
    }

    #[test]
    fn a_branch_with_statements_is_moved_into_a_function_not_pasted() {
        let help = noted(
            r#"if up {
    let label = "x"
    html"<b>{label}</b>"
  } else {
    html""
  }"#,
            EMPTY_BRANCH,
        );
        assert_eq!(help, only_help("up", None));
        assert!(!help.contains("let"), "{help}");
    }

    #[test]
    fn an_empty_branch_beside_something_that_is_not_markup_is_not_this() {
        // A branch that never produces a value has no type to be markup:
        // `return` here is `Never`.
        quiet(
            r#"let x = if up { html"" } else { return icon }
  x"#,
        );
        // And a plain string's `if` is not about markup.
        quiet(
            r#"let s = if up { "" } else { "no" }
  icon"#,
        );
    }

    #[test]
    fn an_else_if_is_left_alone() {
        quiet(r#"if up { html"" } else if n > 1 { icon } else { html"<b>x</b>" }"#);
        quiet(
            r#"if up { html"<b>a</b>" } else if n > 1 { html"<b>b</b>" } else { html"<b>c</b>" }"#,
        );
    }

    #[test]
    fn a_word_between_the_same_markup_is_the_if_in_a_hole() {
        let help = noted(
            r#"if up { html"<span class=state>up</span>" } else { html"<span class=state>down</span>" }"#,
            SAME_MARKUP,
        );
        assert_eq!(
            help,
            r#"put the `if` where they differ: `html"<span class=state>{if up { "up" } else { "down" }}</span>"`"#
        );
    }

    #[test]
    fn a_quoted_attribute_value_that_differs_moves() {
        let help = noted(
            r#"if up { html"""<a class="a">x</a>""" } else { html"""<a class="b">x</a>""" }"#,
            SAME_MARKUP,
        );
        assert!(help.contains(r#"{if up { "a" } else { "b" }}"#), "{help}");
    }

    #[test]
    fn a_plain_hole_in_what_differs_moves_and_a_markup_one_does_not() {
        let help = noted(
            r#"if up { html"<p>{n} up</p>" } else { html"<p>{name} down</p>" }"#,
            SAME_MARKUP,
        );
        assert!(
            help.contains(r#"{if up { "{n} up" } else { "{name} down" }}"#),
            "{help}"
        );
        quiet(r#"if up { html"<p>{icon} up</p>" } else { html"<p>down</p>" }"#);
        quiet(r#"if up { html"<p>{icons} up</p>" } else { html"<p>down</p>" }"#);
        quiet(r#"if up { html"<p>{note} up</p>" } else { html"<p>down</p>" }"#);
        // A markup hole both share is common and stays where it is, when
        // it is not in the run of text that differs.
        noted(
            r#"if up { html"<p>{icon}<b>up</b></p>" } else { html"<p>{icon}<b>down</b></p>" }"#,
            SAME_MARKUP,
        );
        quiet(r#"if up { html"<p>{icon} up</p>" } else { html"<p>{icon} down</p>" }"#);
    }

    #[test]
    fn a_difference_in_markup_or_an_escaped_character_is_left_alone() {
        quiet(r#"if up { html"<a>x</a>" } else { html"<span>x</span>" }"#);
        quiet(r#"if up { html"<p>a &amp; b</p>" } else { html"<p>a | b</p>" }"#);
        quiet(r#"if up { html"<p>it's on</p>" } else { html"<p>its off</p>" }"#);
        // A branch that is not a literal is not this shape.
        quiet(r#"if up { icon } else { html"<p>its</p>" }"#);
    }

    #[test]
    fn an_attribute_one_branch_adds_is_html_only_in_the_tag() {
        let help = noted(
            r#"if n == 0 { html"<ul class=tree>" } else { html"<ul>" }"#,
            SAME_MARKUP,
        );
        assert_eq!(
            help,
            r#"add it with `html.only` where it goes: `html"<ul{html.only(n == 0, html" class=tree")}>"`"#
        );
        let help = noted(
            r#"if n == 0 { html"<ul>" } else { html"<ul class=tree>" }"#,
            SAME_MARKUP,
        );
        assert!(
            help.contains(r#"{html.only(n != 0, html" class=tree")}"#),
            "{help}"
        );
    }

    #[test]
    fn an_addition_inside_a_word_has_no_note_when_the_words_cannot_move() {
        quiet(r#"if up { html"<p>it's</p>" } else { html"<p>its</p>" }"#);
        let help = noted(
            r#"if up { html"<p>cat</p>" } else { html"<p>cart</p>" }"#,
            SAME_MARKUP,
        );
        assert_eq!(
            help,
            r#"put the `if` where they differ: `html"<p>{if up { "cat" } else { "cart" }}</p>"`"#
        );
    }

    #[test]
    fn identical_branches_say_the_if_can_go() {
        let help = noted(
            r#"if up { html"<b>x</b>" } else { html"<b>x</b>" }"#,
            SAME_MARKUP,
        );
        assert!(
            help.starts_with("both branches are the same markup"),
            "{help}"
        );
    }
}
