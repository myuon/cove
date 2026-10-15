# ADR 0104: In `html"…"`, an absent value is no markup

- Status: Accepted
- Date: 2026-10-15
- Supersedes: nothing; it adds a row to
  [ADR 0101](0101-a-page-is-markup-with-an-expression-in-every-hole.md)'s
  table of how a hole goes in
- Refers to:
  [ADR 0102](0102-a-short-literal-may-be-three-quoted-on-one-line-and-markup-joins.md),
  which rejected an `Option<Html>` hole for a reason this ADR does not
  dispute and is not answering;
  [ADR 0103](0103-in-html-a-line-break-is-spacing.md), whose withdrawn
  attribute design is the thing this is careful not to become
- Decides: that a hole whose static type is `Option<T>` puts in **nothing for
  `None`** and, for `Some(v)`, **whatever a hole of type `T` would put in for
  `v`** — markup whole, anything else escaped — so that a value that may be
  absent is written as one, with no `html""` standing in for it

## Context

ADR 0101 decides how a hole goes in by its static type: an `Html` whole, an
`Array<Html>` element by element, anything else rendered as a plain literal
renders it and escaped. An `Option` falls to the last row, so
`html"<p>{note}</p>"` with `note: Option<String>` writes `<p>Some(…)</p>` or
`<p>None</p>` — never what a page means.

So a page says "maybe nothing" with an empty value instead: `html""` is the
absence, and a test for it is `== html""`. In minicloud's admin (`f89be57`)
that is `else { html"" }` four times, `None => html""` once, and one field,
`Look.seen`, typed `Html` and compared with `html""` where its meaning is
"there may be no such thing". The conversion to `html"…"` first reached for
`Option<Html>` there, which is the natural type; ADR 0102 rejected the
`Option<Html>` hole because `==` already worked, which is true and is not the
question. The question is whether a page can hold "absent" as absent.

## Decision

A hole whose static type is `Option<T>`:

| the value | what goes in |
| --- | --- |
| `None` | nothing |
| `Some(v)` | what a hole of type `T` puts in for `v` |

`T` is any type a hole takes, so `Option<Html>` is markup or nothing,
`Option<String>` is escaped text or nothing, and `Option<Option<T>>` applies
the rule twice — `Some(None)` is nothing. Where the hole sits does not matter,
as for every hole: between tags, or inside a quoted attribute value, where
`class="v{suffix}"` adds nothing for `None`.

```cove
html"<li><span class=what>{item.what}</span>{item.seen}</li>"   // seen: Option<Html>
html"<p class=lead>{app.note}</p>"                              // note: Option<String>
```

**It is decided by the type alone.** The lowering already chooses each
piece's append from its checked type (ADR 0101); this is one more row, and it
switches on the option's case in the IR. Nothing reads the markup — the line
ADR 0103 drew when it withdrew `name={…}`. As in ADR 0101, a hole whose type
is a type parameter is decided by the type as written: a `T` that turns out
to be an `Option` is rendered and escaped.

This is JSX's behaviour for `{null}` and `{undefined}`, which render nothing.

## Consequences

**`html"…"` and `"…"` now disagree about an `Option`.** `"{opt}"` still
renders `Some(…)` and `None`, which is what a log line or an assertion wants;
`html"{opt}"` renders the value or nothing, which is what a page wants. The
html literal already differs from the plain one in every row of ADR 0101's
table — that is what it is for — and this is one more row of it.

**A present-or-absent attribute is still a fragment.** `<input{mark}>` with
`mark: Option<Html>` reads better than `mark: Html` with `html""`, but it is
the same fragment with a space in front; knowing an attribute from markup
needs the markup read, which ADR 0103 declined.

## Alternatives rejected

**Only `Option<Html>`.** Narrower, and it leaves `Option<String>` rendering
`Some(…)` onto a page, which is never wanted.

**`None` as an error.** A hole is where a page shows what it has; "nothing"
is a value a page shows by showing nothing.
