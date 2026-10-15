# ADR 0105: An `if` that chooses markup says what it chooses

- Status: Accepted
- Date: 2026-10-16
- Supersedes: nothing
- Refers to:
  [ADR 0101](0101-a-page-is-markup-with-an-expression-in-every-hole.md),
  whose holes this leans on;
  [ADR 0103](0103-in-html-a-line-break-is-spacing.md), whose line — the
  compiler does not read the markup — this keeps;
  [ADR 0104](0104-in-html-an-absent-value-is-no-markup.md), which gave
  `html.only` and the `Option` hole;
  minicloud's admin app and four example apps, converted to `html"…"`
- Decides: two **notes** from `cove check`, each with the rewrite it
  suggests in its help — an `if`/`else` one of whose branches is `html""`
  (**write `html.only`**), and an `if`/`else` whose two branches are `html`
  literals that differ only in text between the same markup (**put the `if`
  in the hole**, cut at tags) or by something one of them adds (**write
  `html.only` of it where it goes**). Notes, not warnings or errors: they
  name a better spelling, the owner wants a convention, not a rule, and a
  warning is a rule to an embedder that loads only warning-free programs

## Context

Converting six apps to `html"…"` left a shape behind in each of them: an
`if` whose branches are both markup, where one of them is no markup at all,
or where both are the same markup with one word different.

```cove
let mine = if app.isAdmin { html" <span class=muted>(this app)</span>" } else { html"" }
let state = if up { html"<span class=state>up</span>" } else { html"<span class=state>down</span>" }
```

The first is what `html.only` (ADR 0104) is for. The second says twice what
it means once — the `<span>` — and the reader has to diff the two branches to
find the one word that changes. JSX's answer to both is to put the condition
where the difference is, and the owner wants that to be how pages here are
written: as a convention the tools point at, not one they enforce.

## Decision

`cove check` adds a note on an `if` expression with exactly an `if` and an
`else` branch (no `else if`) in either of two shapes.

They are notes, not warnings. A note is a suggestion: it prints, with its
help, and stops nothing — `--deny-warnings` and `[check] deny_warnings` let
it through. A warning would be refused by an embedder that loads only
programs whose check has no warnings, and minicloud's host is one: it would
turn a convention into a ban on the shape, in exactly the programs this is
written for.

### A — one branch is no markup: `cove::type::html_if_empty_branch`

One branch is exactly the empty literal `html""`, and the other has type
`Html`.

> note: one branch of this `if` is no markup
> help: write `html.only(<condition>, <markup>)` — or, if the markup is only
> safe to build when the condition holds, an `Option` hole (ADR 0104)

The help names the rewrite with the actual condition and branch, negating
the condition when `html""` is the `if` branch. It says the second half
because `html.only` builds its argument either way, and a lint cannot tell a
branch that unwraps from one that does not.

When the other branch is a block with statements — `{ let … }` — pasting it
on one line into a call would not read, so the help names the move instead:

> help: move this branch into a function answering `Html`, then write
> `html.only(<condition>, thatFunction(…))` — or, if the markup is only safe
> to build when the condition holds, an `Option` hole (ADR 0104)

### B — the same markup with different text: `cove::type::html_if_same_markup`

Both branches are `html` literals. Their common beginning and common end
are found first, as long as they go.

**If one literal is the other with something added** — what is left of one
is empty — the shape is "this or nothing", not a choice between two texts,
and the help is `html.only` of what is added, in a hole where it goes,
negating the condition when it is the `else` that adds:

> note: one branch of this `if` is the other with something added
> help: add it with `html.only` where it goes:
> `html"<ul{html.only(open == 0, html" class=tree")}>"`

for `if open == 0 { html"<ul class=tree>" } else { html"<ul>" }`. What is
added stays `html`, so it may be anything — part of a tag, as here, or a
whole tag. The comparison puts an insertion as late as it can go, so for
`<p>a</p>` against `<p>a<br></p>` it finds `br><`; it is slid back over
equal characters until it is text with no `<` or `>`, or begins with `<`
and ends with `>` (`<br>`), and if no position is either, this shape does
not apply.

**Otherwise the cut moves back to the tags.** The common beginning is cut
back to end just after a `>`, or to nothing; the common end to start at a
`<`, or to nothing. A run of text is never split — a hole in it counts as
text and goes with it — so two sentences that share their first words and a
final `.` move whole:

> note: both branches of this `if` are the same markup
> help: put the `if` where they differ:
> `html"<p>{if up { "The service is up." } else { "The service is down." }}</p>"`

and not `html"<p>The service is {if up { "up" } else { "down" }}.</p>"`, whose
sentence the reader has to reassemble across a hole. A cut that falls inside
a tag — between its `<` and `>`, which is not text — stays where it is, so a
quoted attribute value that differs still moves alone. Then what is left of
each — the part that differs — must hold none of `<`, `>`, `&`, `"` and `'`
in its text, and no hole whose type is `Html`, `Array<Html>` or an `Option`
of either:

> note: both branches of this `if` are the same markup
> help: put the `if` where they differ:
> `html"<span class=state>{if up { "up" } else { "down" }}</span>"`

The five characters are the reason the rewrite keeps every byte: the part
that moves becomes a plain `String`, which the hole escapes, so it must hold
nothing the escaping would change — and a `<` or `>` would mean the
difference is markup, which is a real choice between two structures and not
this shape. A markup hole is excluded for the same reason: in a `String` it
would be rendered as text. The common beginning and end are found on the
literal's parts — text compared as text, a hole compared by its source — so
`{x}` in both branches is common.

Neither note reads the markup. A looks at a literal's emptiness and a type;
B compares two literals' text and looks at `<` and `>` to place its cut and
at five characters to accept it, which is string comparison and not parsing
— the line ADR 0103 drew stands.

## Consequences

**A page that follows the convention has no such `if`.** The admin and the
example apps are brought to zero of these notes when this lands; the help is
the diff to apply. A page that does not follow it still checks, still
passes `--deny-warnings`, and still loads in an embedder that refuses
warnings.

**Neither note fires on a real choice.** `if c { html"<a …>" } else {
html"<span …>" }` differs in markup and is left alone; so is an `else if`
chain, and so is any `if` whose branch is not a literal (a call answering
`Html`), except under A, where the other branch only has to have type `Html`.

## Alternatives rejected

**A note on every `if` whose branches are all `Html`.** Drafted first. It
fires on a choice between two structures, for which there is no better
spelling to suggest — a suggestion with no fix is noise.

**An error.** The owner's word: a convention, as with attribute values.

**Warnings.** The first draft of this ADR, and the first implementation,
made both of them warnings. Applying them to minicloud showed the cost: its
host refuses to load an app whose check has a warning, so a warning there
forbids the shape outright — a ban, not a convention, and one the owner had
already declined as an error. A note says the same thing, with the same
help, and leaves the choice with the page. The same pass found three helps
too weak to follow — a sentence-final `.` pulled out of two different
sentences, a "this or nothing" written as a choice between a string and
`""`, and a `{ let … }` branch pasted on one line into `html.only` — which
are the cut at tags, the `html.only` of what is added, and the help that
names the move, above.

**A separate `cove lint` command.** `cove check` already carries warnings
(`missing_doc`, `unreachable_match_arm`) and notes (`unconstrained_result`,
`variadic_as_value`); a second command for two suggestions is more than they
need.
