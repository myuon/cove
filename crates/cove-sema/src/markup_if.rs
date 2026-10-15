//! The two `if`s that choose markup and have a better spelling,
//! [ADR 0105](../../../docs/adr/0105-an-if-that-chooses-markup-says-what-it-chooses.md):
//! an `if`/`else` one of whose branches is `html""`, which is `html.only`,
//! and an `if`/`else` whose branches are `html` literals that differ only in
//! text between the same markup, which is one literal with the `if` in a
//! hole.
//!
//! What is here is the comparison and the rewrite, over a literal already
//! reduced to [`Atom`]s: the checker decides which `if`s qualify — exactly
//! an `if` and an `else`, branch types — and what each hole's source and type
//! are. Nothing here reads the markup. A literal is compared as text, a
//! character at a time, and a differing part is looked at for five
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
/// is the other branch as written.
pub(crate) fn only_help(condition: &str, markup: &str) -> String {
    format!(
        "write `html.only({condition}, {markup})` — or, if that markup is only safe to build \
         when the condition holds, an `Option` hole (ADR 0104)"
    )
}

/// The help for an `if` whose branches are the two `html` literals `a` (the
/// `if` branch) and `b` (the `else`), or `None` when the `if` does not have
/// the shape: what differs holds one of the five characters a hole escapes,
/// or a hole that puts markup in.
pub(crate) fn same_markup_help(condition: &str, a: &[Atom], b: &[Atom]) -> Option<String> {
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
        return Some(format!(
            "both branches are the same markup, so the `if` can go and either branch stand in \
             its place{shown}"
        ));
    }
    if !middle_a.iter().chain(middle_b).all(Atom::movable) {
        return None;
    }
    let before = elided_end(&a[..prefix]);
    let after = elided_start(&a[a.len() - suffix..]);
    Some(format!(
        "put the `if` where they differ: `html\"{before}{{if {condition} {{ \"{}\" }} else {{ \"{}\" }}}}{after}\"`",
        written(middle_a),
        written(middle_b),
    ))
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
        same_markup_help("up", &atoms(a), &atoms(b))
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
    fn one_side_may_be_empty() {
        assert_eq!(
            help("<b>x</b>", "<b></b>").unwrap(),
            "put the `if` where they differ: `html\"<b>{if up { \"x\" } else { \"\" }}</b>\"`"
        );
    }

    #[test]
    fn a_difference_holding_an_escaped_character_is_left_alone() {
        for (a, b) in [
            ("<b>x</b>", "<i>x</i>"),
            ("<p>a</p>", "<p>a<br></p>"),
            ("<p>a &amp; b</p>", "<p>a</p>"),
            ("<p>say \"hi\"</p>", "<p>hi</p>"),
            ("<p>it's</p>", "<p>its</p>"),
        ] {
            assert_eq!(help(a, b), None, "{a} against {b}");
        }
    }

    #[test]
    fn a_difference_holding_a_markup_hole_is_left_alone() {
        assert_eq!(help("<p>{!icon} up</p>", "<p>down</p>"), None);
        // A markup hole both sides share is common, and does not move.
        assert!(help("<p>{!icon} up</p>", "<p>{!icon} down</p>").is_some());
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
            shown.contains("{if up { \"n\" } else { \"ff\" }}"),
            "{shown}"
        );
        assert!(shown.contains("`html\"…"), "{shown}");
        assert!(shown.contains("…\"`"), "{shown}");
        assert!(!shown.contains(&"x".repeat(70)), "{shown}");
    }

    #[test]
    fn only_names_the_condition_and_the_markup() {
        assert_eq!(
            only_help("app.isAdmin", "html\"<b>x</b>\""),
            "write `html.only(app.isAdmin, html\"<b>x</b>\")` — or, if that markup is only \
             safe to build when the condition holds, an `Option` hole (ADR 0104)"
        );
    }

    /// What `cove check` warns about `body`, the body of a function in a
    /// module that imports `std.html`, with `up: Bool`, `n: Int`,
    /// `name: String`, `icon: Html`, `icons: Array<Html>` and
    /// `note: Option<Html>` in scope — as `(code, message, help)`.
    fn warnings(body: &str) -> Vec<(String, String, String)> {
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
            .filter(|d| d.severity == Severity::Warning)
            .map(|d| (d.code, d.message, d.help.unwrap_or_default()))
            .collect()
    }

    #[track_caller]
    fn warns(body: &str, code: &str) -> String {
        let found = warnings(body);
        assert_eq!(found.len(), 1, "{body}: {found:?}");
        assert_eq!(found[0].0, code, "{body}: {found:?}");
        found[0].2.clone()
    }

    #[track_caller]
    fn quiet(body: &str) {
        let found = warnings(body);
        assert!(found.is_empty(), "{body}: {found:?}");
    }

    #[test]
    fn an_empty_else_is_html_only() {
        let help = warns(
            r#"if up { html"<b>{n}</b>" } else { html"" }"#,
            EMPTY_BRANCH,
        );
        assert_eq!(help, only_help("up", r#"html"<b>{n}</b>""#));
    }

    #[test]
    fn an_empty_if_branch_is_html_only_of_the_negated_condition() {
        let help = warns(r#"if up { html"" } else { icon }"#, EMPTY_BRANCH);
        assert_eq!(help, only_help("!up", "icon"));
        let help = warns(r#"if n > 1 { html"" } else { icon }"#, EMPTY_BRANCH);
        assert_eq!(help, only_help("!(n > 1)", "icon"));
        let help = warns(
            r#"if name.isEmpty() { html"" } else { icon }"#,
            EMPTY_BRANCH,
        );
        assert_eq!(help, only_help("!name.isEmpty()", "icon"));
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
        let help = warns(
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
        let help = warns(
            r#"if up { html"""<a class="a">x</a>""" } else { html"""<a class="b">x</a>""" }"#,
            SAME_MARKUP,
        );
        assert!(help.contains(r#"{if up { "a" } else { "b" }}"#), "{help}");
    }

    #[test]
    fn a_plain_hole_in_what_differs_moves_and_a_markup_one_does_not() {
        let help = warns(
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
        // A markup hole both share is common and stays where it is.
        warns(
            r#"if up { html"<p>{icon} up</p>" } else { html"<p>{icon} down</p>" }"#,
            SAME_MARKUP,
        );
    }

    #[test]
    fn a_difference_in_markup_or_an_escaped_character_is_left_alone() {
        quiet(r#"if up { html"<a>x</a>" } else { html"<span>x</span>" }"#);
        quiet(r#"if up { html"<p>a &amp; b</p>" } else { html"<p>a</p>" }"#);
        quiet(r#"if up { html"<p>it's</p>" } else { html"<p>its</p>" }"#);
        // A branch that is not a literal is not this shape.
        quiet(r#"if up { icon } else { html"<p>its</p>" }"#);
    }

    #[test]
    fn identical_branches_say_the_if_can_go() {
        let help = warns(
            r#"if up { html"<b>x</b>" } else { html"<b>x</b>" }"#,
            SAME_MARKUP,
        );
        assert!(
            help.starts_with("both branches are the same markup"),
            "{help}"
        );
    }
}
