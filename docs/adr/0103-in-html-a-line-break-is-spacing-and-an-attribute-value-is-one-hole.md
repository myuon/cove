# ADR 0103: In `html"…"`, a line break is spacing and an attribute's value is one hole

- Status: Proposed
- Date: 2026-10-15
- Supersedes:
  [ADR 0100](0100-a-literal-may-be-written-over-lines-and-the-closer-sets-its-margin.md)'s
  line breaks as content, **for `html"""…"""` only**; and
  [ADR 0101](0101-a-page-is-markup-with-an-expression-in-every-hole.md)'s
  holes anywhere in markup — a hole inside a tag is now `name={…}` or refused
- Refers to:
  [ADR 0102](0102-a-short-literal-may-be-three-quoted-on-one-line-and-markup-joins.md);
  minicloud's `apps/admin` as of `0ec48da` (v0.28.0), where every figure
  below was counted
- Decides: that in a multi-line `html"""` a line break, with the indentation
  around it, is **nothing between two tag-or-hole edges and one space
  anywhere else**, so markup is laid out without a `\` on every line; and that
  inside a tag a value goes in **only as `name={expr}`, the whole value** —
  quoted and escaped by the lowering, or, for a `Bool` or an `Option`, the
  attribute present or absent — with any other hole inside a tag refused

## Context

The admin app was converted to `html"…"` and then to ADR 0102's forms
(minicloud #126, #127). Two things about the result were judged not good
enough by the owner, and both are counted here.

**Every line of laid-out markup ends in `\`.** ADR 0100 made a `\` ending a
line eat the newline and the next line's indentation, which is what kept the
pages byte-identical, and the cost is one `\` per line: **458** of them in
`pages.cove`. Sorted by what sits on each side of the join:

| the join | count | renders today |
| --- | ---: | --- |
| a tag or a hole on both sides (`>` or `}` ⏎ `<` or `{`) | 342 | nothing |
| a space written before the `\` (prose) | 115 | one space |
| anything else | 1 | nothing |

Under the rule decided below, **457 of the 458 render exactly as they do
today**. The one that does not is `…of <code>{action}</code>:` ⏎ `<pre>`,
which gains a space before a block element and renders the same.

**A value inside a tag is written three ways.** 68 attribute values hold a
hole inside quotes (`href="{base}/kv?prefix={p}"`: `href` 32, `class` 11,
`action` 9, `value` 6, …); one hand-writes `&amp;` because the literal's text
is not escaped and the hole's is; and the attributes that are there or not
are `Html` fragments that begin with a space — `current(here)` answering
`html" aria-current=page"`, `mark` answering `html" checked"`, and
`<li{mark}>`. The owner wants one way, JSX's: `class={…}`, and wants the
others refused rather than discouraged.

## Decision

### A line break in `html"""` is spacing

In a multi-line `html"""…"""`, a line break inside the content — together
with all of the indentation of the line after it (the trailing whitespace of
the line before is already dropped, ADR 0100) — becomes:

- **nothing**, when the line before ends with `>` or with the `}` that closes
  a hole, **and** the line after begins with `<` or with the `{` that opens
  one;
- **one space**, anywhere else.

A run of blank lines is one break. The margin is still checked. A `\` ending a
line still joins with nothing and takes precedence, so two words can be
joined without a space. `\n` is a newline as it always was, since it is an
escape and not a break in the source — which is how a `<pre>` keeps one; a
hole's value is never touched. The one-line form has no breaks, and a plain
`"""` keeps its line breaks as content, because there they are text.

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
JSX's rule — a line break next to a tag goes — was the other candidate and
is rejected below.

### Inside a tag, a value is `name={expr}`

The lowering reads an `html` literal's text far enough to know whether a
position is **inside a tag**: `<` followed by a letter, `/` or `!` opens one,
`>` outside quotes closes it, and inside it `"` and `'` open and close a
quoted value. Inside a tag:

- **`name={expr}`** — `=` directly before the hole, and whitespace, `>` or `/`
  directly after it — is the value of attribute `name`, decided by the type
  of `expr`:

  | `expr`'s type | what is written |
  | --- | --- |
  | `Bool` | ` name` when `true`; nothing — not the name, not the space before it — when `false` |
  | `Option<T>` | ` name="…"` for `Some(v)`, `v` as below; nothing for `None` |
  | `Html`, `Array<Html>` | refused: an attribute's value is text, and markup in it is a mistake |
  | anything else | ` name="…"`: rendered as in a plain literal and escaped, inside double quotes the lowering writes |

  ```cove
  html"""<a href={"{at}/kv?prefix={text.urlEncode(p)}&after={text.urlEncode(last)}"}>more</a>"""
  html"""<input type=checkbox name={"cap.{capability}"} checked={granted}>"""
  html"""<a href={"{base}/logs"} aria-current={if here { Some("page") } else { None }}>logs</a>"""
  html"""<li class={if item.level == "" { None } else { Some("is-{item.level}") }}>"""
  ```

  The value is built as an ordinary `String` — interpolation, `if`, a call —
  and escaped **once**, as a whole, so `&after=` is written as `&after=` and
  comes out `&amp;after=`.

- **Any other hole inside a tag is refused**, with a diagnostic that shows the
  `name={…}` spelling: a hole inside a quoted value (`class="state {x}"`), as
  part of an unquoted one (`class=v{x}`), where an attribute's name goes
  (`<li{mark}>`), or in a tag's name. **It is an error, not a warning**: there
  is one way to put a value in an attribute, and the compiler is the linter.

A static attribute is untouched — `class=title`, `class="state removed"`,
`type=checkbox checked` are text, as before. A hole between tags is untouched
— ADR 0101's escaping by type.

This is JSX's notation (`className={…}`, `checked={x}`, an `undefined` value
leaving the attribute out) and the familiar one for the effect the owner asked
for. It also settles part of ADR 0101's Open item on context: **a value can no
longer reach an unquoted attribute**, because the lowering writes the quotes,
and a hole can no longer sit where an attribute's name goes.

## Consequences

**The admin loses its 458 `\`.** What renders differently, expected and
checked by the snapshot rather than assumed: the one space before `<pre>`;
the three `aria-current=page` gain quotes, `aria-current="page"`; nothing
else.

**The 68 quoted holes become 68 `name={…}`**, `current()` and the `mark`
fragments go, and the hand-written `&amp;` goes.

**The tag tracking is HTML knowledge in the compiler**, which ADR 0101 avoided
by having the lowering know only types. It is kept as small as the decision
needs — the five transitions above, no element names, no nesting, no
validation of the markup — and every rendering choice still calls `std.html`.
Cove has no compile-time evaluation of Cove, so a check that must refuse at
compile time has nowhere else to live.

## Alternatives rejected

**JSX's line-break rule** — a line break next to a tag goes. It breaks prose:
`its` ⏎ `<code>` renders `its<code>`, and JSX code writes `{" "}` to put the
space back. Prose wrapped before an inline tag is common in the admin's pages.

**A warning instead of an error** for a hole inside a quoted value. It is
safe — the hole is escaped — so a warning would be defensible. Rejected
because the point is one spelling, and a warning is a second spelling the
compiler tolerates.

**`class` lists** (Vue's `:class="[a, {b: cond}]"`, Svelte's
`class:name={cond}`). The admin builds class values with an `if` in a
`String`, and nothing measured needs more.

**Refusing holes inside `<script>` and `<style>`.** It would need element
names, which the tracking above deliberately does not know, and no page holds
either. Left Open.

## Open

- Holes inside `<script>` / `<style>`, and `javascript:` in a URL value: still
  ADR 0101's Open item.
- Whether `\` at the end of a line in `html"""`, now nearly always redundant,
  should itself become an error.
