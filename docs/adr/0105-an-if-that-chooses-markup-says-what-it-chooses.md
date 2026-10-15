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
- Decides: two **warnings** from `cove check`, each with the rewrite it
  suggests in its help — an `if`/`else` one of whose branches is `html""`
  (**write `html.only`**), and an `if`/`else` whose two branches are `html`
  literals that differ only in text between the same markup (**put the `if`
  in the hole**). Warnings, not errors: they name a better spelling, and the
  owner wants a convention, not a rule

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

`cove check` warns on an `if` expression with exactly an `if` and an `else`
branch (no `else if`) in either of two shapes.

### A — one branch is no markup: `cove::type::html_if_empty_branch`

One branch is exactly the empty literal `html""`, and the other has type
`Html`.

> warning: one branch of this `if` is no markup
> help: write `html.only(<condition>, <markup>)` — or, if the markup is only
> safe to build when the condition holds, an `Option` hole (ADR 0104)

The help names the rewrite with the actual condition and branch, negating
the condition when `html""` is the `if` branch. It says the second half
because `html.only` builds its argument either way, and a lint cannot tell a
branch that unwraps from one that does not.

### B — the same markup with different text: `cove::type::html_if_same_markup`

Both branches are `html` literals, and once their common beginning and
common end are taken away, what is left of each — the part that differs —
holds none of `<`, `>`, `&`, `"` and `'` in its text, and no hole whose type
is `Html`, `Array<Html>` or an `Option` of either.

> warning: both branches of this `if` are the same markup
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

Neither warning reads the markup. A looks at a literal's emptiness and a
type; B compares two literals' text and looks for five characters, which is
string comparison and not parsing — the line ADR 0103 drew stands.

## Consequences

**A page that follows the convention has no such `if`.** The admin and the
example apps are brought to zero warnings when this lands; the help is the
diff to apply.

**Neither warning fires on a real choice.** `if c { html"<a …>" } else {
html"<span …>" }` differs in markup and is left alone; so is an `else if`
chain, and so is any `if` whose branch is not a literal (a call answering
`Html`), except under A, where the other branch only has to have type `Html`.

## Alternatives rejected

**Warning on every `if` whose branches are all `Html`.** Drafted first. It
fires on a choice between two structures, for which there is no better
spelling to suggest — a warning with no fix is noise.

**An error.** The owner's word: a convention, as with attribute values.

**A separate `cove lint` command.** `cove check` already carries warnings
(`missing_doc`, `unreachable_match_arm`) and `--deny-warnings` for whoever
wants them fatal; a second command for two rules is more than they need.
