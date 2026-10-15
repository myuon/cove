# ADR 0100: A literal may be written over lines, and the closer sets its margin

- Status: Accepted; superseded in part by
  [ADR 0102](0102-a-short-literal-may-be-three-quoted-on-one-line-and-markup-joins.md): the rule that `"""` must be followed by a newline
- Date: 2026-10-15
- Refers to:
  [ADR 0045](0045-a-literal-is-there-before-the-program-runs.md), which owns
  what a literal is before the run and therefore what stripping a margin at
  compile time means;
  [ADR 0077](0077-cove-fmt-is-covefmt.md), because both formatters measure a
  literal's width and one of them rebuilds it from parts;
  [ADR 0099](0099-a-page-is-markup-with-a-path-in-every-erb-tag.md) and
  [ADR 0101](0101-a-page-is-markup-with-an-expression-in-every-hole.md), the
  two that want it;
  [minicloud](https://github.com/myuon/minicloud)'s `apps/admin`, the program
  this is measured against
- Decides: that a literal opened with `"""` takes its **margin from its
  closing delimiter**, so the indentation the source needed is not part of
  the string; that inside one a `"` is **content**, needing no escape, while
  escapes and interpolation behave exactly as they do in `"…"`; and that a
  `\` ending a line **eats that line's newline and the next line's
  indentation**, in either literal form, so that text whose bytes matter can
  be laid out — and nested — over source lines without changing a byte of it

## Context

**Cove already has multi-line string literals.** Not by decision — by there
being no arm for a newline. `Lexer::lex_string` loops over four cases,
`None`, `"`, `\` and `{` (`crates/cove-syntax/src/lexer.rs:539-608`), and a
newline falls to the catch-all at `lexer.rs:603-606` that pushes any other
character onto the current run. There is no check and no diagnostic code for
it. This compiles and runs today:

```cove
let s = "a
    b{1 + 1}c"
```

and prints `a`, a newline, four spaces, `b2c`. The newline inside the literal
does not reach `Lexer::push_token`'s `preceded_by_newline`
(`lexer.rs:150-158`), which is set only by `skip_whitespace` and after a block
comment, so the statement-break rule (`Parser::at_statement_break`,
`crates/cove-syntax/src/parser.rs:668-681`) is not disturbed. Both formatters
already tile it: covefmt's `Scan::quoted` exits only on the closer, an escape
pair or EOF (`tools/covefmt/covefmt/scan.cove:271-288`), and the Rust
oracle's `skip_string` is the same byte loop (`crates/cove-syntax/src/format.rs:181-204`).

So the thing missing is not the ability. It is that **a literal written over
lines keeps the indentation the source needed to be readable**, which makes
it useless for any text whose bytes matter — and markup's bytes matter, both
because whitespace between inline elements is significant and because a page
rewritten to be readable in source should render the same page.

### What a page costs today

Measured on minicloud `main` 7e38b4a, `apps/admin/pages.cove`, 2,241 lines.
(ADR 0099's own figures are the 2,018-line `b80c6276`; the file has grown
since, so the two are not comparable line for line and this ADR says which
tree each number is from.)

| | |
| --- | ---: |
| median line | 28 characters |
| lines over 200 | 49 |
| lines over 300 | 22 |
| longest line | **1,101** characters, line 1724 |
| literals holding a `<` | 262, median 74 characters, longest 1,022 |

Every backslash in the file, counted as occurrences rather than lines:

| escape | occurrences |
| --- | ---: |
| `\"` | **289**, in 101 literals, on 99 lines |
| `\n` | 2 |
| `\\` | 1 |
| `\{` | 1 |
| `\}` | 1 |

The decomposition accounts for all 295 backslash characters in the file
(289 + 2 + 2 + 1 + 1, the `\\` being one escape of two characters).

Two costs, and they are separable. One is the 289 — an HTML attribute is
written `class=\"foo\"`, and this is what the file actually pays. The other
is the shape: a row of a table is one line of source because a literal cannot
be laid out, and that line cannot be reviewed or diffed. A middling example,
line 2007, twelve escaped quotes in 326 characters:

```cove
  "{legend}<svg class=chart viewBox=\"0 0 640 120\" role=img aria-label=\"{h(label)}\"><title>up to {h(peak)}, over {a.length()} rows</title><polyline class=\"ln {aClass}\" fill=none points=\"{metricsPoints(times, a, largest)}\"/><polyline class=\"ln {bClass}\" fill=none points=\"{metricsPoints(times, b, largest)}\"/></svg>"
```

## Decision

**`"""` followed by a newline opens a literal whose content begins on the next
line.** The newline after the opener is not content.

**The closing `"""` sits on a line of its own, and its indentation is the
margin**, removed from the start of every line. The newline before the closer
is not content either, so a one-line page is exactly that one line:

```cove
let page = """
  <p class=lead>How fast <code>{h(app.name)}</code> answers.</p>
  """
```

is `<p class=lead>How fast <code>…</code> answers.</p>` and nothing else — no
leading two spaces, no trailing newline.

**A line indented less than the margin is refused**, not guessed at. This is
Swift's rule rather than Java's, which takes the margin to be the smallest
indentation over all non-blank lines and the closer together. Both are
familiar; the one that refuses is the one where a line accidentally outdented
cannot silently change what every other line renders.

**Inside a `"""` literal a `"` is content.** `'`, `{`, `}`, every escape in
the table at `lexer.rs:96-109` and every interpolation behave exactly as they
do in `"…"`: `{expr}` interpolates, `\{` is a literal brace, `\n` is a
newline, `\u{…}` is a scalar. A literal `"""` inside one is written with an
escaped quote. **This is not a raw-string form** — see the rejected
alternatives.

**Trailing horizontal whitespace on a line is not content.** It is invisible
in source and in a diff, and `cove fmt` promises a file with none
(`tools/covefmt/covefmt/print.cove:842-844`), a promise that `roundTrips`
(`print.cove:834-837`) would put in direct conflict with a literal allowed to
hold it.

**A `\` at the end of a line eats that line's newline and all of the next
line's leading whitespace**, in both literal forms. Today `\` followed by a
newline is `cove::lex::unknown_escape` (verified by compiling it), so the
spelling is free. A line's content is read from after the margin, trailing
horizontal whitespace is dropped, and if what remains ends in an unescaped
`\` then the `\`, the newline, and the leading horizontal whitespace of the
line that follows are all dropped — the whole of it, not only the margin, so
markup can be indented as deeply as it nests. A continued line must still sit
at or beyond the margin; the rule refuses what it would otherwise refuse. A
space wanted at such a join is written **before the `\`**: the line then ends
in the `\`, so there is no trailing whitespace for the rule above to drop.

**This paragraph said something else before the ADR was merged, and was
wrong.** It had the `\` eat only the newline, with the next line keeping
whatever indentation it had beyond the margin, and it put a wanted space at
the start of the next line. Under that rule the example below renders
`<tr>  <td …>` — the two spaces of nesting leak into the page — while the
ADR claimed `<tr><td …>`. The example was right about what markup needs and
the rule was not; implementing it is what showed the two disagree. It was
corrected on the branch, before acceptance reached `main`, rather than by a
superseding ADR.

This is the rule that makes the form usable for markup, and without it `"""`
would be half a feature: the margin alone removes the indentation and leaves
the newlines, and a newline between two inline elements is a space the page
did not have before.

```cove
let row = """
  <tr>\
    <td class=title data-label=queue><code>{h(queue.name)}</code></td>\
    <td class=nums data-label=due>{queue.due}</td>\
  </tr>
  """
```

renders `<tr><td class=title …></td><td class=nums …>…</td></tr>` — one line
of output, six lines of source, byte for byte what the one-line literal
rendered. Where two inline elements need the space between them:

```cove
let said = """
  <b>{figure}</b> \
  <span class=muted>{unit}</span>
  """
```

is `<b>…</b> <span class=muted>…</span>`.

**The margin is removed at compile time**, so what ADR 0045 calls the literal
is the stripped text. Nothing is computed at run time, and a `"""` literal
with no holes is one `Inst::Str` like any other.

## Consequences

**The 289 go.** An attribute is written `class="foo"`, which is how it is
written everywhere else in the world.

**A page can be laid out one element to a line and render the same bytes.**
That is the property ADR 0101 needs, because an expression in a hole makes a
line longer, and it is worth having on its own: 49 lines over 200 characters
become reviewable without the page changing.

**Both formatters need work, and they need different work.**

- covefmt measures a literal's width by counting every non-continuation byte
  in its range (`tokenWidth`, `tools/covefmt/covefmt/lex.cove:265-285`), and
  `flatWidth` (`print.cove:1157+`) sums those — and is documented as needing
  to be exact, since *"a width that does not match the string it predicts is
  a width that answers a different question than the one the caller asked"*.
  A three-line literal would be measured as its whole character count and
  every enclosing group judged over `maxWidth()`. `column`
  (`print.cove:1349-1366`) already resynchronises after a newline and needs
  nothing.
- The Rust oracle's `Out::col()` (`format.rs:316-318`) overcounts the same
  way, and its no-source fallback `string_from_parts`
  (`format.rs:2170-2196`) re-escapes a newline as `\n` — so `cove generate`
  and the AST's `Display` would rewrite a `"""` literal into one line. The
  oracle test compares the two formatters on every `.cove` file in the
  repository (`tools/covefmt/README.md:19-22`), so this is a divergence that
  fails a gate rather than one that hides.

**An unclosed `"""` is worse than an unclosed `"`.**
`cove::lex::unterminated_string` fires only at EOF (`lexer.rs:811-821`), so a
missing closer already swallows the rest of the file; a three-character
closer gives one more way to write one.

**Nothing existing changes.** `"…"` keeps every byte it has today, newlines
included, which is why this is a second form rather than a rule added to the
first.

## Alternatives rejected

**A raw form that stops processing escapes.** It would stop interpolation
too, and interpolation is the whole value of a literal in the program this is
measured against — 590 top-level holes in that one file. And there is nothing
to be freed from: outside the 289 quotes, the file's escapes are five
characters in total.

**Java's margin.** The smallest indentation across all non-blank lines and
the closer, with no error for an outdented line. Rejected for the reason
above: it guesses, and it guesses about output.

**`"""` without the continuation `\`.** Half a feature, for markup. The
newlines it leaves in are output.

**The continuation `\` without `"""`.** This is the cheap half, and it is
cheap: one arm in `escaped_char`'s caller, no new delimiter, no margin rule,
no formatter width problem, and `\` + newline is an error today so nothing
breaks. It gets the shape and leaves the 289. It is recorded here as the
fallback if `"""` is judged not to have earned its place, because it is not
obviously the wrong answer.

**Collapsing whitespace when the page is rendered**, so that the source could
be laid out freely and the renderer would tidy up. Rejected before this ADR
was written and recorded here: it breaks `<pre>`, and the space between two
inline elements is a space the page means.

## Open

- **`"""x"""` on one line.** Not allowed, since the opener must be followed
  by a newline — which is also what keeps `""""` from being ambiguous. Whether
  the one-line form is wanted anyway is not decided here.
- **The playground's highlighting** paints a literal as one `Kind::Str` piece
  (`crates/cove-wasm/src/highlight.rs:117-121`); whether its JS consumer
  tolerates a piece spanning newlines has not been traced.
- **Trailing whitespace** is dropped rather than refused. Refusing it would
  be louder and is a smaller rule; dropping it is Java's behaviour and is
  what the formatter's promise implies.
- **The known covefmt/oracle divergence** on a long literal inside a nested
  call, found while implementing `std.html`, is adjacent to the width work
  above and is not fixed by this ADR.
