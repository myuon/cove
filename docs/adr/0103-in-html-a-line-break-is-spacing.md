# ADR 0103: In `html"""`, a line break is spacing

- Status: Accepted
- Date: 2026-10-15
- Supersedes:
  [ADR 0100](0100-a-literal-may-be-written-over-lines-and-the-closer-sets-its-margin.md)'s
  line breaks as content, **for multi-line `html"""…"""` only**
- Refers to:
  [ADR 0101](0101-a-page-is-markup-with-an-expression-in-every-hole.md),
  whose split this keeps — the compiler knows a hole's type and nothing of
  HTML;
  [ADR 0102](0102-a-short-literal-may-be-three-quoted-on-one-line-and-markup-joins.md);
  minicloud's `apps/admin` as of `0ec48da` (v0.28.0), where every figure
  below was counted
- Decides: that in a multi-line `html"""` a line break, with the indentation
  around it, is **nothing between two tag-or-hole edges and one space
  anywhere else**, so markup is laid out without a `\` on every line — and
  that this is decided from the characters either side of the break, with no
  reading of the markup

## Context

The admin app was converted to `html"…"` and then to ADR 0102's forms
(minicloud #126, #127). ADR 0100 made a `\` ending a line eat the newline and
the next line's indentation, which is what kept the pages byte-identical, and
the cost is one `\` per line of laid-out markup: **458** of them in
`pages.cove`, judged by the owner not good enough. Sorted by what sits on each
side of the join:

| the join | count | renders today |
| --- | ---: | --- |
| a tag or a hole on both sides (`>` or `}` ⏎ `<` or `{`) | 342 | nothing |
| a space written before the `\` (prose) | 115 | one space |
| anything else | 1 | nothing |

## Decision

In a multi-line `html"""…"""`, a line break inside the content — together
with all of the indentation of the line after it (the trailing whitespace of
the line before is already dropped, ADR 0100) — becomes:

- **nothing**, when the line before ends with `>` or with the `}` that closes
  a hole, **and** the line after begins with `<` or with the `{` that opens
  one;
- **one space**, anywhere else.

A run of blank lines is one break. The margin is still checked. A `\` ending a
line still joins with nothing and takes precedence, so two words, or a hole
and a tag with no space between, can still be joined. `\n` is a newline as it
always was, since it is an escape and not a break in the source — which is how
a `<pre>` keeps one; a hole's value is never touched. The one-line form has no
breaks, and a plain `"""` keeps its line breaks as content, because there they
are text.

```cove
html"""
  <tr>
    <td class=title><code>{queue.name}</code></td>
    <td class=nums>{queue.due}</td>
  </tr>
  """
```

is `<tr><td class=title><code>…</code></td><td class=nums>…</td></tr>`, and

```cove
html"""
  What <code>{app.version}</code> is made of: its
  <code>app.toml</code>, its <code>.cove</code> files
  """
```

is `What <code>…</code> is made of: its <code>app.toml</code>, its …`.

This is Vue's default for its templates (`whitespace: 'condense'`): whitespace
between two tags that holds a line break goes, and elsewhere it is one space.
**It needs no reading of the markup.** The lexer, which already knows the
literal's tag, looks at the last character of one line and the first of the
next; it does not know what a tag is, only what `>` and `<` are.

Under it, **457 of the 458 joins render exactly as they do today**. The one
that does not is `…of <code>{action}</code>:` ⏎ `<pre>`, which gains a space
before a block element and renders the same.

## Consequences

**The admin loses its `\`s.** The one byte that changes is the space before
`<pre>`, on the pages that show a refused change.

**A hole followed by a space and then a tag cannot be split across lines
without a `\`**: `{names(app.required)} ` ⏎ `<span class=muted>` is `}` ⏎ `<`,
so the break is nothing, and the space written before it is trailing
whitespace and dropped. Writing the two on one line, or keeping the `\` with
the space before it, says it. It came up twice in the admin.

## Alternatives rejected

**JSX's line-break rule** — a line break next to a tag goes. It breaks prose:
`its` ⏎ `<code>` renders `its<code>`, and JSX code writes `{" "}` to put the
space back. Prose wrapped before an inline tag is common in the admin's pages.

**`name={expr}` for an attribute's value, decided by type.** Drafted in this
ADR, implemented, and withdrawn before merge, and recorded because the reason
is worth keeping. The draft had the lowering write ` name="…"` for a hole that
is an attribute's whole value, escaped once as a whole, with a `Bool` making
the attribute present or absent and an `Option` present with a value or
absent — JSX's `className={…}` and `checked={x}` — and recommended it as the
one way to put a value in an attribute. **To know that a hole is an
attribute's value, the compiler has to read the markup**: the draft tracked
`<`, `>` and quotes to know whether a position was inside a tag. That is HTML
in the compiler, which ADR 0101 had kept out by having the lowering know only
types, and it does not stay small — comments, a `<` in text, `<script>` and
`<style>` each want one more rule, and the rules end as a parser. Every
language that has `attr={…}` (JSX, templ, Svelte) parses its markup whole; the
notation brings that weight with it. Against it, what it bought was small: a
hole inside a quoted value, `class="state {x}"`, is already escaped and safe,
so the gain was a spelling and one hand-written `&amp;`, and the
present-or-absent attributes were needed only because the spelling had made
them so. The owner judged the direction wrong, and the convention is the
quoted value.

## Open

- A present-or-absent attribute (`checked`, `aria-current`) is still written
  as an `Html` fragment that begins with a space, `html" checked"`, put where
  an attribute goes. A small `std.html` helper could name the pattern without
  any reading of the markup; nothing here decides one.
