# ADR 0101: A page is markup with an expression in every hole

- Status: Accepted
- Date: 2026-10-15
- Supersedes:
  [ADR 0099](0099-a-page-is-markup-with-a-path-in-every-erb-tag.md)'s hole
  notation `<%= path %>`, the `fill` that reads it, and the fallible `render`
  that exists because `fill` cannot fail. **It keeps all four of 0099's
  load-bearing decisions** — that the type of what goes in a hole decides
  whether it is escaped, that `Html` is `opaque`, that `markup` is the single
  way past escaping, and that a mistake must fail a gate rather than render.
  0099's header carries `Superseded in part by [ADR 0101]`, naming the
  notation, as `CLAUDE.md` "Superseding" asks.
- Refers to:
  [ADR 0094](0094-a-string-names-a-scalar-by-number-with-u.md), the one ADR
  that granted syntax, and the pattern this one follows: a condition recorded
  by an earlier ADR, cashed in when a program met the friction;
  [ADR 0044](0044-a-callback-takes-what-its-type-declares.md),
  [ADR 0046](0046-a-byte-offset-is-a-value-a-string-hands-out.md),
  [ADR 0047](0047-a-code-point-is-written-as-a-character-and-is-an-int.md) and
  [ADR 0074](0074-an-int-also-carries-a-fixed-width-bit-pattern.md), the four
  that declined syntax;
  [ADR 0069](0069-structural-reflection-stays-inside-the-standard-library.md),
  because this takes reflection *off* the page path;
  [ADR 0100](0100-a-literal-may-be-written-over-lines-and-the-closer-sets-its-margin.md),
  without which this reads worse than what it replaces;
  issue #403, which already moved interpolation onto the builder protocol;
  [minicloud](https://github.com/myuon/minicloud) branch
  `refactor/metrics-std-html`, the pilot this is measured on
- Decides: that `html"…"` — a string literal tagged `html`, available where
  `std.html` is imported — is **assembled by the lowering one piece at a
  time**, appending a hole in whole when its **static type** is `Html` and
  escaping it otherwise; that the lowering names `std.html`'s functions
  directly, as it already names `std.stringbuilder`'s, and **does not
  introduce a general tagged-literal mechanism**, while being written so that
  one could replace it; and that, because a hole is then an ordinary Cove
  expression checked by the checker, **`render` becomes infallible** and the
  run-time path check goes

## Context

ADR 0099 was implemented (`crates/cove-sema/std/html.cove`, 881 lines, 28
e2e cases on both backends) and then tried on the program it was measured
against. The pilot is minicloud's `refactor/metrics-std-html`, pinned to cove
`ccacfc8`, which converted two sections of `apps/admin/pages.cove` to
`std.html` and rendered byte-identical pages.

| | `main` 7e38b4a | pilot 50363fc |
| --- | ---: | ---: |
| lines | 2,241 | 2,441 |
| `struct` definitions | 3 | **12** |
| calls to `h(` | 187 | 176 |
| `html.fill(` | — | 17 |
| `\"` | 289 | 289 |

Two sections of 104 page functions cost 200 lines and nine structs, and
removed eleven of 187 escaping calls. 0099 predicted the shape of this and
said so plainly — *"It is not a line-count win and should not be sold as
one"*, with *"perhaps twenty"* view-model structs over the whole file. Nine
for two sections puts that estimate out by some multiple, but **the magnitude
is not this ADR's argument.** 0099's own argument was never about magnitude
either.

### What is wanted

Pages written the way JSX lets a page be written — markup as markup, a value
where it is used, a condition as an expression, a list as a mapped list of
markup — without a JavaScript runtime, without a second process, and without
a build step, because the embedder distributes an app as a directory of
source that it checks before it switches to it. That is 0099's requirement
unchanged. What 0099 could not also give is the compiler's check on what goes
in a hole.

### The condition this ADR cashes in

0099 wrote down the thing it could not fix, in the paragraph where it
retracted its first answer:

> **The second half was true and the first half was false.** No test catches
> it, because a marker is text: the program this ADR is measured against was
> given one misspelt hole on a page that renders on every view of it, and
> `check` reported nothing and all 58 tests passed. A page would have shipped
> with the marker in it.

and then, having made `render` fallible so that something would fail:

> So the check is still at run time — compile-time binding would need either
> a macro system or syntax, and ADR authors before this one have declined
> both for less — but it is a *failing* check and not a visible one.

That is a recorded condition, and ADR 0094 is the precedent for what to do
with one: 0046 and 0047 wrote down what would have to be true before a code
point could be named by number, and 0094 cashed it in when issue #602 showed
a program meeting the friction. This ADR is the same move. **What it proposes
is the "syntax" half of the sentence above.**

One correction to that sentence, since this ADR leans on it: it is accurate
about syntax — 0044, 0046, 0047 and 0074 each declined some — but **no ADR
has ever declined a macro system.** The only mention of a macro anywhere in
`docs/adr` is ADR 0010's passing *"Not a plugin, not a macro, not a build
script"*. There is no precedent there to lean on, in either direction.

### What `{` actually costs

0099 could not use the brace, and gave the reason in its Context (4): *"`{` is
the language's… interpolation owns `{` and a literal brace in a Cove string
is `\{`"*, so `{{ }}` would have had to be written `\{\{ name \}\}`. That is
true. It is worth knowing what it costs, because this ADR proposes to spend
it. On `main` 7e38b4a, the same file:

| escape | occurrences |
| --- | ---: |
| `\"` | 289, in 101 literals on 99 lines |
| `\n` | 2 |
| `\\` | 1 |
| `\{` | **1** |
| `\}` | 1 |

The one `\{` is the secrets page quoting a TOML fragment at a reader:

```
[secrets] key = \{ store = \"name\" \}
```

So the freedom `<%= %>` bought — a template that may hold a brace with no
escape — is worth **one line of one page** in the program it was bought for.
The tax that file actually pays is the 289, and `fill` did not touch it: the
pilot's tree has the same 289. (That one is ADR 0100's subject.)

### Conditions are already inside these literals

0099's Decision says *"markup stays markup while loops and conditions stay in
Cove"*, and its rejection of a template language is *"control flow in markup,
a second syntax to parse, check, format and colour, to express what `if` and
`for` already express"*. On `main`, line 2209:

```cove
    let replace = "<form class=inline method=post action=\"{h(base)}/secrets/set\"><input type=hidden name=name value=\"{h(secret.name)}\"><input type=password name=value autocomplete=new-password required placeholder=\"new value\"><button class=primary>{if secret.set { "Replace" } else { "Set" }}</button></form>"
```

A condition, inside a literal, inside markup — written in Cove, parsed by
Cove's parser, checked by Cove's checker, formatted by covefmt and coloured
by Cove's highlighter, today, with no ADR. What 0099 rejected was a *second*
syntax; what the file does is put *the* language in a hole. A hole holding
`{if secret.set { "Replace" } else { "Set" }}` is the status quo this ADR
proposes to type, not a liberty it proposes to take.

The parser's restrictions on a hole are already exactly the ones wanted: a
hole holds **one expression** (`cove::parse::unexpected_token`, rule *"`{ }`
inside a string literal interpolates exactly one expression"*,
`crates/cove-syntax/src/parser.rs:2492-2501`), an empty hole is an error
(`cove::parse::empty_interpolation`, `parser.rs:2452-2461`), and nesting
shares the parser's depth budget (`parser.rs:2484-2488`). A `for` does not fit
in a hole and will not start fitting.

### What `fill` cannot do, and what the nine structs hold

`fill` evaluates nothing. A hole names a path into a value, so a figure that
is **worked out** rather than carried needs a field to be carried in. That is
the whole source of the nine structs, and it is worth looking at what ended up
in them. Forty-two fields:

| what the field holds | fields |
| --- | ---: |
| markup — an `Html` built elsewhere on the same page | 14 |
| prose or a formatted figure — `"3 attempts"`, `"1 row"`, `"of 40 in the last day"`, an ISO time, `"12.4"` | 18 |
| a CSS class name — `" is-ok"`, `"ln-served"` | 4 |
| a field copied out of a `host.*` struct | 3 |
| an `Int` the host's series already holds | 3 |

**An earlier draft of this section argued from the wrong property.** It
pointed out that none of the 42 is data in a shape a `.json` endpoint would
want, and concluded that 0099's *"One loader, two renderings"* consequence
was not at risk. That answers a question nobody asked: the owner's interest in
the loader shape is **how it is to write**, not a second rendering of the same
data, and this repository's embedder wants no `.json` endpoint for these
pages. The argument is withdrawn and the census is kept, because the census
is still what the question is about — it is just a different question.

The real trade, stated as what it is rather than settled by a number:

- **`fill`'s shape** gives a flat template that is scannable as markup, and a
  named, typed declaration of what the page shows that a test can construct
  directly. A reader of `<%= retry %>` has to go to the struct literal to
  learn it is `queueDuration(queue.retryMs)` — two places, one of them a
  summary.
- **`html"…"`** puts the value where it is used — one place — and pays for
  it in the markup's shape, because an expression in a hole makes a line
  longer. That cost is real and it is **ADR 0100's**: markup laid out over
  source lines absorbs an expression in a hole, and markup crammed onto one
  line does not. This ADR is worth less without 0100, and should not be
  accepted before it.

Nothing stops a page written with `html"…"` from taking a loader-shaped
struct and reading only its fields; the shape stays available as a style. It
stops being the only way a computed value can reach a hole.

36 of the admin app's 104 page functions take a `host.*` parameter directly
(45 if `Array<host.X>` counts), and those need no intermediate struct under
either design, because the embedder's structs are the data.

## Decision

`html"…"` is a string literal whose holes are **escaped unless their static
type says they are markup**, and whose value is an `Html`:

```cove
use std.html
use std.html.Html

fn row(queue: host.Queue) -> Html {
  html"""
    <tr>\
      <td class=title><code>{queue.name}</code></td>\
      <td class=nums>{queue.due}</td>\
      <td class=block>{settings(queue)}</td>\
    </tr>
    """
}

fn table(queues: Array<host.Queue>) -> Html {
  html"<table class=cards>{queues.map(fn(queue) { row(queue) })}</table>"
}
```

- **Literal text** between holes is appended as-is. It is the template's own
  characters and there is nothing in it to escape.
- **A hole whose static type is `Html`** is appended in whole. It is already
  markup — here, `settings(queue)`.
- **A hole whose static type is `Array<Html>`** is appended element by
  element, in order — here, the mapped rows.
- **Any other hole** is rendered by the machinery that renders it in a plain
  literal today, and the text is **escaped** before it is appended — here,
  `queue.name` and `queue.due`.

The escaping decision is therefore the same decision ADR 0099 made — *the type
of what goes in decides* — moved from a run-time inspection of a value to a
static test on an expression's type. `Html` stays `opaque` and `markup` stays
the single way past escaping, one name a review greps for.

**The tag is the module's name, and only `std.html`'s.** `html"…"` is accepted
where the name `html` resolves to the module `std.html`, so the existing rule
holds — *a program that does not `use` it does not have it* — and the tag
reads as what it is, the module that decides the escaping. Any other name in
front of a quote is an error naming the one tag there is. `html` is also the
spelling lit-html gave the same idea in JavaScript (`` html`…` ``), which is
the nearest thing this has to a convention.

**There is nothing left for `render` to refuse**, so `render(page: Html) ->
String` is infallible and `Html` no longer carries a `wrong`. A hole naming a
field that does not exist is `cove::type::unknown_name` at the hole's span,
which is where it was before `std.html` existed.

**`fill` goes, with its notation and its 28 e2e cases.** A template is never
loaded at run time in this embedder — an app is distributed as source and
checked before it is switched to — so `fill`'s run-time template string has no
use that `html"…"` does not cover, and keeping it would keep the one mechanism
whose holes nothing checks.

### How it lowers

In the same assembly a plain literal uses. Issue #403 did the expensive half
already (#404, #405): an interpolation is not one variadic intrinsic but a
byte buffer and one append per piece, each append a call into
standard-library Cove, each piece appended right after it is evaluated. The
place a piece's type picks its append is `Body::append_piece`,
`crates/cove-ir/src/lower/interpolate.rs:177-193`, whose doc comment already
anticipates an arm of this kind:

> *"The one place a piece's type decides how it becomes text. A type that
> shows itself through a trait of its own would be one more arm here."*

For `html"a{x}b"` the lowering emits, in outline:

```
buf = GrowableAlloc PackedBytes            -- as for any literal
appendText(buf, "a")                       -- literal piece, raw
<evaluate x>
-- the arm, chosen statically on x's checked type:
--   Html        appendText(buf, <x's markup>)
--   Array<Html> the same, once per element
--   String      std.html.appendEscaped(buf, x)
--   otherwise   render x as a plain literal would, into a scratch run,
--               then std.html.appendEscaped(buf, <that text>)
appendText(buf, "b")
s = RunFinish(buf, Utf8)                   -- as for any literal
Html(<s>)                                  -- the literal's value
```

The IR is monomorphised, so every arm is chosen when the literal is lowered:
no run-time inspection of a value's shape, no `core.dynamic*` call. No new
instruction is needed — `GrowableAlloc`, `RunFinish` and calls into std are
what a plain literal already emits. **The escaping itself is Cove**, a
function in `std.html` that writes into the buffer, where `fill` built a
`String` with `renderValue` and then another with `escape`.

The tree-walking oracle gets the same arm in its interpolation
(`crates/cove-runtime/src/interp.rs:2028-2040`), so both backends answer the
same bytes and the e2e suite can keep running on both.

### Why the lowering may name `std.html`

The lowering already names standard-library modules to build literals:
*"an interpolation's appends are `std.stringbuilder`'s, `==` on two erased
values is `std.dynamic`'s"* (`crates/cove-sema/src/stdlib.rs:224-227`).
PHILOSOPHY's *"Separate policy from mechanism"* asks that *"user-visible
abstractions and algorithms belong in Cove source and its standard library"*
and that the IR describe *"resolved storage, control flow, calls, and runtime
boundaries"*. This keeps to it: the lowering's part is a static choice among
calls, which is mechanism, and what escaping means — which bytes, as what
entities — stays in `std.html`'s Cove source, which is policy.

What it does change is `std.html`'s place on the `IMPORTED` list
(`stdlib.rs:222-237`), whose doc comment defines a module on it as one the
lowering does *not* name. Either the lowering names `std.html` only for a
package whose source holds an `html"…"` — so that a program which never
writes one still never has it lowered — or `std.html` leaves the list. The
first keeps every property the list exists for and is the one intended; which
of the two the implementation can honestly keep is left Open.

### How it would generalize, and why it does not yet

Only HTML wants this today, so only HTML gets it. The generalization is
written down so that the HTML implementation does not close it off:

- **What would be general**: a tag naming a module, and the module supplying
  the three things the lowering above calls by name — append text that must
  be escaped, get at a markup value's text, and wrap the finished text as the
  module's type. A `sql"…"` would supply quoting where `std.html` supplies
  entities.
- **How the HTML implementation keeps that open**: the three callees are one
  table the arm consults, not three names scattered through the lowering, and
  the check that the tag is `html` sits in one place. Generalizing is
  replacing that check with a resolution and that table with a lookup — a
  change to a rule, not a rewrite.
- **What would decide it**: a second module that needs type-directed quoting,
  in a real program. Until one exists, PHILOSOPHY's *"Earn complexity through
  use"* says the protocol is not designed, and this ADR does not design it —
  in particular it does not choose between a trait the tagged type conforms
  to and a set of functions a module exports.

## The case against, and the answers

Each objection is stated in the strongest form the author could find, and the
answer is given even where it is not conclusive.

**1. It is new syntax, and syntax must earn its place.** PHILOSOPHY's rule,
in full: *"Syntax is for behavior the compiler or runtime can enforce. Human
intent and background belong in doc comments, not special keywords disguised
as prose."* Escaping decided by type is behaviour a compiler can enforce —
and 0099 put exactly that rule in a library, where the compiler could not. The
four ADRs that declined syntax declined it for things the compiler was not
being asked to enforce: a `_` parameter name (0044), a code-point literal
before any program needed one (0046), bit-pattern operators that methods
already expressed (0074).

**2. "Earn complexity through use" asks for recurring friction in
representative programs.** The friction recurs — 187 escaping calls, nine
structs for two sections, and one misspelt hole that passed every gate — but
it recurs in **one** program, minicloud's `apps/admin`, with 0099's count of
four page-rendering files in that repository as the only sign of the rest.
This is the weakest point of the proposal. It is weaker for a general
mechanism than for this one, which is why this one is HTML-only: what is
added is one tag for the one need that has been measured. A second program
rendering pages should be measured before acceptance. (ADR 0069's reopening
bar, which asks for friction *"recurring across more than one program"*,
governs *public reflection* and does not bind this — `html"…"` takes
reflection off the page path rather than exposing it — but its standard is the
right one to hold this to, and this ADR does not yet meet it.)

**3. It puts expressions in markup, which 0099 refused.** 0099 refused a
second syntax; a hole holds Cove, and the file already has `{if secret.set {
"Replace" } else { "Set" }}` in it. The style rule 0099 wrote is still worth
keeping and this ADR keeps it: a loop stays in Cove — at most a `map` in a
hole, which is an expression — a hole takes one expression because the parser
already says so, and a page that reads like a template language is a page to
review, not a feature.

**4. The loader shape is nicer to write.** Not settled, and not something a
measurement settles. The census above says what the structs actually held:
markup, prose, CSS class names and copies. What is lost is a named, typed
statement of what a page shows that a test can construct. What is not lost is
the option to write that way: a page may still take a loader-shaped struct
and read its fields, and the difference is that it no longer has to.

**5. The language grows a module-specific rule.** It does: the lowering knows
the name `std.html`. That is the same kind of knowledge it has of
`std.stringbuilder` and `std.dynamic`, the policy stays in Cove, and the cost
— one more module the lowering names — is the cost "Earn complexity through
use" prefers to a general protocol nobody else uses yet.

**6. Performance.** The same buffer and the same appends, with escaping moved
from `escape(renderValue(…))` — which builds a `String` and then another — to
a function writing into the buffer. No change of class is expected and none is
claimed; PHILOSOPHY's *"Preserve the performance class"* asks for a
measurement, and this ADR does not have one.

**7. What is gained, against all of it.** The compiler checks the holes again,
which is the whole of 0099's unfixed problem. `Html` is recognised by its
type, not its shape: `isHtml` is structural today and says so — *"A program
that declares its own `export opaque struct Html { said: String, wrong:
String }` would be taken for this one; nothing can tell them apart from
here"* (`std/html.cove:606-636`). That comment goes on to price the hole it
leaves at nearly nothing, *"a thing that program could have done with
[`markup`] anyway"*, and it is right about that; what the static test buys is
not a closed hole but a check that costs no run-time inspection at all.
`render` loses its `Result`. The nine structs and 42 fields go. And an
`Array<Html>` stops needing a separator pushed in as an item, because a hole
holds an expression that can build one.

## Alternatives rejected

**A general tagged-literal mechanism now.** Designed above as the direction,
and not built: one need, one module, and no second implementor to design the
protocol against. An earlier draft of this ADR chose it, on the argument that
a lowering naming `std.html` would break "Separate policy from mechanism".
That argument was wrong — the rule is about where the *algorithm* lives, the
algorithm stays in Cove, and the lowering already names two standard-library
modules for the same reason — and it is recorded here because it was the
whole case for building the general form first.

**No tag; the expected type decides.** `fn row(…) -> Html { "<td>{x}</td>" }`,
Swift's `ExpressibleByStringLiteral`. It adds **no syntax whatsoever**, which
makes it the cheapest thing on this list by the bar that matters most.
Rejected because it makes the escaping decision invisible: the same literal
escapes or does not depending on an annotation that may be a long way away,
and `let s = "<p>{name}</p>"` followed by `markup(s)` is an injection that
reads like the safe spelling. 0099's principle is that the one dangerous thing
is visible and greppable; a tag is, and a return type is not. It is also not
cheaper to implement: a hole is checked today with no expectation and a `Str`
literal is always `Ty::Str` (`crates/cove-sema/src/typeck.rs:4836-4845`), so
the change moves from the lexer into the checker rather than disappearing.

**Keeping `fill` alongside.** Two mechanisms for one job, and the one being
kept would be the one whose holes nothing checks.

**A macro system.** 0099 names it as the other way to get compile-time
binding. Not proposed: it is a much larger language than this needs, and the
one thing wanted here is a static choice among appends in the lowering.

**Leaving it at `fill` plus ADR 0100.** The live alternative, and the one to
prefer if objection 2 is judged unanswered. Multi-line literals and the
continuation `\` make a `fill` template readable, the 289 escaped quotes go,
and 0099 stands as written — at the price of the run-time path check and the
view-model structs, both measured above. Nothing in this ADR is urgent; 0099
ships a working mechanism.

## Consequences

**The admin app converts nearly mechanically.** `"…{h(x)}…"` becomes
`html"…{x}…"`; a hole that was deliberately raw markup — a `String` built by
another page function — becomes an `Html` returned by it, which is the
conversion that turns each implicit decision into a type. Line counts should
fall, not rise, and the conversion should render byte-identical pages, which
is how the pilot was checked and how this should be.

**The lexer fuses a tag.** `html"x"` today is two tokens and no error until
the checker: the main lexer loop dispatches on the first character only and
`lex_ident` never peeks at a following quote (`crates/cove-syntax/src/lexer.rs:160-218`,
`:351-368`), Cove has no statement separator, and a `Str` token can start an
expression — so `let page = html"<p>x</p>"` parses today as `let page = html`
followed by a separate expression statement. Adjacency is recoverable but not
recorded: `Token` carries `kind`, `span` and `preceded_by_newline` and no
"preceded by space" flag (`crates/cove-syntax/src/token.rs:7-17`). The
existing precedent for adjacency changing a token is the duration suffix,
`500ms` (`lexer.rs:467-530`).

**Both formatters and the highlighter see a new token kind**, on top of the
width work ADR 0100 already lists.

## Open

- **Escaping is not contextual.** One rule — five characters to entities —
  applies wherever a hole sits, as it did under 0099. That is right between
  tags and inside a quoted attribute value, and it is not enough inside an
  unquoted attribute value (a space ends it), inside `<script>` or `<style>`,
  or for a URL whose scheme is the danger (`javascript:`). Go's
  `html/template` escapes by context for exactly this reason. In the admin
  app on `main`, no hole sits in an unquoted attribute value — the ten that
  look like it are query parameters inside a quoted `href` — so the gap is
  inherited, not exercised. Whether the lowering should refuse a hole in those
  positions, which it could see in the literal text, is not decided here.
- **A rendered value into a scratch run.** Today the walk renderers append
  straight into the literal's buffer. The escaping arm wants a value's text
  before it is appended, so either the walk is pointed at a scratch run or the
  escaping is folded into the walk. Which one costs less has not been looked
  at.
- **Constructing an opaque `Html` from lowered code.** The literal's value is
  an `Html` the lowering builds, so the lowering needs the right to build a
  value of a type whose module declared it `opaque` — or `std.html` exports a
  function the lowering calls to wrap it, which is the third callee in the
  table above.
- **`IMPORTED`**: whether `std.html` can stay a module a program has only by
  importing it, as discussed under "Why the lowering may name `std.html`".
- **The measurement** objection 6 asks for, and the second program objection 2
  asks for.
