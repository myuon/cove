# ADR 0102: A short literal may be three-quoted on one line, and markup joins

- Status: Accepted
- Date: 2026-10-15
- Supersedes:
  [ADR 0100](0100-a-literal-may-be-written-over-lines-and-the-closer-sets-its-margin.md)'s
  rule that `"""` must be followed by a newline; and
  [ADR 0101](0101-a-page-is-markup-with-an-expression-in-every-hole.md)'s
  `std.html` surface — `render(page)` as a free function, and `empty()`
- Refers to:
  [minicloud](https://github.com/myuon/minicloud) #126, the conversion of the
  admin app to `html"…"`, which is where every figure below was counted
- Decides: that `"""…"""` may open and close on **one line**, with `"` as
  content and the last three quotes of a closing run as the closer; that
  `std.html` adds `join(parts, between)`; that rendering is a **method**,
  `page.render()`; and that `html""` is the one spelling of no markup, so
  `empty()` goes

## Context

ADRs 0100 and 0101 were implemented (cove #659) and the program they were
measured against was converted with them (minicloud #126): 187 escaping calls
to none, 289 escaped quotes to 9, 49 lines over 200 characters to none, and
183 pages rendered byte for byte as before. The conversion also reported what
was awkward, counted on the converted `apps/admin`:

| friction | where it shows | count |
| --- | --- | ---: |
| a fragment of one line that holds a `"` costs three lines as `"""` or an escape per quote as `"…"` | `html"""` blocks with one line of content | 37 |
| | fragments beginning with a space, kept as `"…"` with `\"` because a leading space in `"""` is indentation | 2 (4 escapes) |
| no way to put markup between the items of a list | a hand-written `joined(items, between)`, called | 9 |
| no markup is spelt `html.empty()` | `pages.cove` and `admin.cove` | 28 |
| a test that looks at markup wraps it | `html.render(…)` in `admin_test.cove` | 89 |

Two more were reported and turned out not to be: **`Html` can already be
compared** — `==` is structural for every type and `html"" == html.empty()`
is `true`, verified by running it — so the `Option<Html>` the conversion
reached for, to tell "nothing" from something, was not needed; and **`html""`
already is no markup**, so `empty()` was a second spelling all along.

## Decision

**A `"""` literal may open and close on one line.** Where the opener is not
followed by a newline, the literal is the text up to the closer, exactly as
written: no margin, `"` is content, escapes and `{expr}` holes as in `"…"`. It
may not run onto a second line; one that does is refused, naming the newline.

```cove
html""" class="is-{item.level}""""
html""" <span class=added title="set here, not in app.toml">(changed)</span>"""
html"""<a href="{at}/kv">{name}</a>"""
```

**The closer is the last three quotes of a run.** A run of more than three
`"` where a literal can close leaves the ones before the last three as
content, so `""" class="x""""` is ` class="x"`. This is Kotlin's rule for its
raw strings, and it is the one that lets a fragment end on an attribute's
closing quote — the case the first example above is.

The multi-line form is unchanged: `"""` followed by a newline still takes its
margin from a closer on a line of its own.

**`std.html` gains `join`:**

```cove
export fn join(parts: Array<Html>, between: Html) -> Html
```

which puts `between` between each two of `parts` and nothing around them — the
`" · ".join(…)` of markup. The nine calls of the admin's own `joined` become
`html.join`.

**Rendering is a method.** `std.html` declares `Html`, so it may give it an
`impl` (`cove::resolve::foreign_inherent_impl` refuses only other modules):

```cove
impl Html {
  fn render(self) -> String
}
```

`html.render(page)` becomes `page.render()`, and a test reads left to right:
`metricsSteps(at, "day").render().contains("<b>the last day</b>")`. The free
function goes rather than staying beside the method, because two spellings of
one thing is what this ADR is removing elsewhere.

**`empty()` goes.** `html""` is no markup, says so where it is used, and is
already the value `empty()` returns.

## Consequences

The admin's 37 three-line fragments become one line each, the last four `\"`
in its markup go, its `joined` helper goes, and `html.empty()` becomes
`html""` at 28 sites. What is left of `\"` in that file is SQL — a quote
doubled inside a statement — which is not markup and reads worse in `"""`.

`page.render()` is the first method in the standard library on a type a
library module declares rather than a builtin; nothing about that is new to
the language, which has allowed an `impl` in the declaring module since the
first ADR, but it is a pattern a later library will copy.

## Alternatives rejected

**A `\s` escape for a space**, which Java added with its text blocks for
exactly the leading- and trailing-space case. Every fragment that wanted it in
the admin app is one line, which the one-line form covers; nothing measured
needs it in the multi-line form. Not earned yet.

**An `Option<Html>` hole** that puts in nothing for `None`. Reached for
because `Html` looked incomparable; it is not, and `page == html""` says what
the `Option` was standing in for.

**`isEmpty()` on `Html`.** `== html""` says it, and is the same comparison any
other value gets.

**Keeping `render(page)` beside `page.render()`, and `empty()` beside
`html""`.** Two spellings each, and the reason this ADR exists is that
friction was counted rather than imagined; a spelling nobody needs is the
opposite.

**Requiring the one-line closer to be exactly three quotes**, as Swift's and
Python's do. A fragment ending in an attribute's closing quote — one of the
two that kept `\"` — could then not be written at all.
