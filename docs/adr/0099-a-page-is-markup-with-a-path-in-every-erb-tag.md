# ADR 0099: A page is markup with a path in every `<%= %>`

- Status: Proposed; superseded in part by
  [ADR 0101](0101-a-page-is-markup-with-an-expression-in-every-hole.md):
  the hole notation `<%= path %>`, `fill`, and the fallible `render`
- Date: 2026-10-14
- Refers to:
  [minicloud](https://github.com/myuon/minicloud)'s `apps/admin`, the program
  this is measured against;
  [ADR 0097](0097-std-json.md), whose `Json` is the other thing a loader might
  answer;
  the `impl` rule in [LANGUAGE_REFERENCE.md](../LANGUAGE_REFERENCE.md)
  "Items" and "The one implicit conversion", which between them closed the
  alternative this ADR rejects first
- Decides: that `std.html` exports an opaque `Html` and one function,
  `fill(template, data)`, whose holes are written `<%= path %>` — ERB's
  delimiters, naming a path into `data`; that **the type of what a path
  reaches decides whether it is escaped**, so an `Html` goes in whole and
  everything else is escaped; that **`render` is fallible and `fill` is
  not**, so a hole whose path names no field is recorded on the `Html`,
  propagated through composition, and refused once per page rather than
  written onto it; and that markup stays markup while loops and conditions
  stay in Cove

## Context

A server-rendered page is written today by joining strings. The program that
does the most of it is minicloud's admin app, and it is worth measuring
rather than describing:

- `apps/admin/pages.cove` is **2,018 lines**, almost all of it string
  building.
- It calls its escaping helper **179 times**, and **175 of those are inside an
  interpolation** — `{h(app.name)}`. One missed call is an injection, in the
  one app that may change the host's settings, and nothing in the type system
  says which of its interpolations need it. One 480-character expression has
  six: four escaped, two deliberately raw markup. Telling them apart means
  reading each callee.

  **This bullet said 259, and 259 was a miscount — twice over.** It is written
  out rather than quietly adjusted, because a measurement a decision rests on
  should not move under the decision without saying so. First, the figure came
  from `grep -c 'h('`, which counts *lines* rather than calls and which
  matches `push(`, `length(` and `match(` as well as `h(`; counting the token
  instead, with `grep -oE '(^|[^A-Za-z0-9_])h\('`, gives 180 occurrences, one
  of which is `fn h(`'s own definition — so **179** calls, of which 175 sit
  inside a `{…}`. Second, 259 is not even that grep against the file this
  bullet measures: on the 2,018-line version, minicloud `b80c6276`,
  `grep -c 'h('` answers **239**; **259** is the same grep against
  `87a4a17e`, a 2,241-line version of the file committed later the same day.
  The bullet reported two trees at once.

  **This bullet also gave a count of the file's interpolations, and that
  figure is withdrawn rather than corrected.** It could not be reproduced:
  two scanners of the same 2,018-line tree disagree with it and with each
  other. A third number nobody has checked would be worse than none, and the
  argument does not rest on it — what it rests on is that every one of those
  interpolations has to be decided about by hand.

  None of this changes the decision. 179 places a reader has to decide about
  is the same argument 259 was, and the argument was never about the
  magnitude: it is that *no* number of them is checked by anything.
- The compactness is an illusion. The median line is **28** characters, but
  **49** are over 200, **22** over 300, and the longest is **1,109**. That
  line cannot be reviewed or diffed.

What is wanted is the separation a React Router loader has — **a function
that answers data, and markup that is filled from it** — without a JavaScript
runtime, without a second process, and without a build step, because the
embedder distributes an app as a directory of source that it checks before it
switches to it.

### What the language had already decided

Each of these was found by compiling it, not by reading about it.

1. **A trait may be implemented only for a struct or enum the module
   declares** (`cove::resolve::unknown_impl_type`) — a builtin is neither, so
   `impl Children for String` is refused before the orphan rule is reached.
   The language's one implicit conversion, a concrete value standing for a
   `dyn Trait`, therefore cannot let a bare `String` be children. This closed
   the obvious alternative, a tree of element calls, by making its bracket
   noise permanent.
2. **There is no spread**, so a variadic `children: Node...` could not take a
   computed array, and pages build children in loops.
3. **A positional argument may not follow a labeled one**
   (`cove::parse::positional_after_label`).
4. **Every one-character sigil collides with something a page holds, and
   `{` is the language's.** `?` appears 19 times in the admin app's markup and
   **every one is a query string** — `?key=`, `?over=`, `?n=`. `$` appears
   **0** times across four page-rendering files, which looked like enough
   until a `$` had to be told from a hole: a price is `$5.00` and a total is
   `100$`, and no rule reading the text around a bare `$` separates those from
   a hole standing alone. And `{{ }}`, which nearly every template language
   uses, is unavailable outright, because interpolation owns `{` and a literal
   brace in a Cove string is `\{`. What is left is a *pair* of delimiters that
   needs no escaping and cannot occur in markup.
5. **Reflection is a standard library privilege.** `core.dynamicOpen`,
   `dynamicChildCount`, `dynamicChild`, `dynamicFieldName`, `dynamicTypeName`,
   `dynamicString` and the rest are host builtins an application cannot
   reach — an embedder describes no schema for `core`, so a module that names
   it is refused as unchecked. Inside `std` they are ordinary:
   `std/dynamic.cove` calls `core.dynamicOpen` with no import at all, and
   takes `Any`.

(5) is why this belongs in the standard library rather than in a package. It
is not a matter of convenience; the thing cannot be written anywhere else.

## Decision

A loader answers an ordinary struct. Markup is an ordinary string whose holes
are paths into it. `fill` puts them together.

```cove
export opaque struct Html { … }

export fn fill(template: String, data: Any) -> Html
export fn markup(said: String) -> Html     // the one way past escaping
export fn empty() -> Html
export fn render(page: Html) -> Result<String, Error>
```

**A hole is `<%=`, a path, and `%>`** — ERB's delimiters, and the path is
jq's dot notation.

| | |
| --- | --- |
| `<%= . %>` | the data itself, as `.` is jq's input |
| `<%= name %>` | a field |
| `<%= limits.maxQueued %>` | a field of a field |
| `<%= items[2] %>` | an element |

A segment's name is letters, digits and `_`; an index is `[` digits `]`. **The
space inside the delimiters is trimmed**, so `<%=name%>` and `<%= name %>` are
one hole written two ways. **Everything outside a pair of them is text**, with
nothing to escape: a template may hold `$5.00`, `?over=hour`, `[[x]]`, `$(y)`,
`100%` and `{` freely, and that is the thing no sigil could promise.

**The type of what the path reaches decides how it goes in:**

| what the path reaches | what `fill` does |
| --- | --- |
| an `Html` | puts it in whole — it is already markup |
| an `Array<Html>` | puts them in, one after another |
| anything else | renders it and **escapes** it |

So the escaping decision is written where the data is declared, not in the
template:

```cove
struct Row {
  at: String
  said: String
  who: Html      // this one is markup, and the declaration says so
}

fill("<tr><td class=at><%= at %></td><%= who %><td><%= said %></td></tr>", row)
```

`Html` is opaque, so the only values that skip escaping are ones `fill` made
or ones `markup` was told about — **`markup` is the single way past escaping,
and one name a review can grep for.**

```cove
// the loader knows nothing about markup, so the page and the `.json`
// endpoint are the same call
fn lines() -> Array<Line> { … }

// the view binds by path; the `for` and the `if` are Cove's
fn row(line: Line) -> Html {
  fill(
    "<tr><td class=at><%= at %></td><td class=said><%= said %></td></tr>",
    line,
  )
}

// `.` is the data itself, so wrapping needs no struct
fill("<div class=scroll><table class=log><%= . %></table></div>", rows)
```

**`<%= %>` because it brings Rails' meaning with it.** `{{ }}` is the
near-universal convention — Mustache, Handlebars, Jinja, Liquid, Angular, Vue,
Go — and Cove cannot have it, for Context (4)'s reason. `<%= %>` is the next
most recognised pair (ERB, EJS, JSP, ASP), it needs no escaping in a Cove
string, and `<%` is not valid HTML, so it collides with no markup, no URL and
no prose.

The deciding reason is the *semantics* it arrives with and not the shape.
**Rails' `<%= %>` already decides escaping by type**: an
`ActiveSupport::SafeBuffer` goes in whole and a plain `String` is escaped, with
`raw` and `html_safe` as the single opt-out — and the standard warning in that
community is that `html_safe` scattered through a view is the XSS
anti-pattern. That is this decision exactly, down to there being one name a
review greps for, so a reader who knows Rails already knows what `<%= %>`
means here *and* what to be suspicious of. No other delimiter brings that:
`{{ }}` in Mustache and Jinja escapes by default but has no type to decide by,
so its opt-out is per hole — `{{{ x }}}`, `{{ x|safe }}` — which is the
escaping decision back in the template's punctuation, where this ADR already
refused to put it.

Two ways this is stricter than Rails. `Html` is `opaque`, so unlike a
`SafeBuffer` it cannot be conjured out of any `String` by a method call;
`fill` and `markup` are the only two things that make one. And `markup` has to
be imported by name, so the opt-out shows in the import list as well as at its
call site.

**A hole that goes wrong makes `render` refuse, and `fill` stays infallible.**
A path that names no field is a mistake in the template; `fill` *records* it
on the `Html` it answers and `render` hands back `Err(Error(…))` naming the
path and the template it was in.

This paragraph said something else and was wrong, so what it said is kept
here and the reason it failed is recorded with it. **It said the hole was
loud**: a path that reached nothing rendered `[no such path: what.was.asked]`,
a `<%=` that was never closed rendered `[unclosed <%=: …]`, both escaped and
so inert wherever they landed; a silent empty string would have hidden the
one mistake this design can still make, and a `Result` would have poisoned
every call site of a function used once per element.

**What that got wrong is where "loud" has to be.** A marker is loud in the
output and silent in every gate a program has. It was measured in the program
this ADR is measured against: one hole in `apps/admin` was changed from
`<%= name %>` to `<%= nmae %>`, in a chart legend that renders on every view
of that page, and then `minicloud check` reported 0 warnings and 0 refusals
and `minicloud test` reported 58 passed. Nothing failed. The page would have
shipped reading `[no such path: nmae]`, having passed everything it had to
pass. Before that page used `std.html` the compiler checked the field
reference, because the page named the field in Cove; the template took the
check away and the marker did not put it back. Loud where a person might
happen to look and silent where a machine does look is the wrong way round.

**`render` is fallible and `fill` is not**, and the objection above survives
exactly half of itself. It is about `fill`, which runs once per row and once
per cell and whose value is that it reads like the markup it makes; a `?`
there is a `?` at every element. `render` is the single boundary where markup
becomes a response — once per page, at a site that is already fallible for
half a dozen other reasons — so the check costs one `?` and buys back what
the compiler used to do.

`Html` is opaque, so it has somewhere to keep the problem, and composition
propagates it: an `Html` carrying a problem, put into another template through
a hole or arriving in a list of markup, hands the problem to the result. The
**first** one is kept — a page with two bad holes is one bug report and the
first is the one to go and look at. A hole that goes wrong puts in nothing;
there is no marker any more, because once `render` refuses, the text of a
broken `Html` cannot be observed at all.

**A path that does not reach a value is an error.** One rule, with no second
kind. A segment that names no field of what it is applied to — `<%= nmae %>`,
or `<%= who.said %>` into an opaque value — is a mistake in the template,
because a struct's fields are settled when it is declared.

**An index past the end is not an exception**, and an earlier draft of this
paragraph said it was: `<%= items[5] %>` against three items was to put in
nothing, on the grounds that the length depends on the data and a short list
is something a page legitimately renders. That does not survive contact with
the notation. A fixed index is the template *asserting* there is a sixth
item — a claim about the shape of the data — and a false claim is a mistake
like a misspelt field. A page whose length really varies loops in Cove and
hands over an `Array<Html>`, which is what a list hole and `<%= . %>` are
for. Letting the index through would also have left one hole still doing the
thing this whole decision stopped doing: rendering something and failing
nothing, where a hole that quietly contributes nothing is a hole nobody
notices. (An empty list put in whole still puts in nothing — no rows is no
markup. It is indexing that makes a claim.)

Because a hole has an end, the text between the two is a path or it is
nothing: a trailing `.`, a bracket with no number, a space in the middle of a
name are each refused like a path that names no field. A sigil has no end to
stop at, so a scanner could only read as far as it understood and leave the
rest standing in the markup: `$.limits.maxQued` failed loudly and
`$.limits.max Qued` not at all. And an unclosed hole was not an error anybody
could report.

**Paths, not positions.** An earlier draft used `$1`, `$2`. A path does not
move when the markup is rearranged — reordering columns, adding a cell —
which is the edit people actually make; positional holes swap their values
silently under exactly that edit. A path also reaches `limits.maxQueued`,
which a flat name could not.

**A struct, not a map or a `Json`.** Both would work. A struct is better for
three reasons that are not style:

- its construction is checked — a field cannot be forgotten, misspelled or
  given the wrong type, which `Map.of(MapEntry(key: "aat", …))` cannot say;
- `fill` can enumerate the field names, so **`<%= nmae %>` is detectable**.
  Against
  a map, a name that is not a key is indistinguishable from a key whose value
  is empty;
- a field that is not a `String` needs no stringifying by the caller, because
  `dynamicKind` can tell an `Int` from a `Bool`.

## Consequences

**Escaping stops being a decision.** Every path escapes unless what it reaches
is an `Html`, including inside an attribute value, where `"` matters. The 179
places a reader must reason about become none.

**One loader, two renderings.** The function that answers the page's data
answers the `.json` endpoint, because it never knew about markup. That is the
property this was taken from React Router for, and it arrives with no
JavaScript anywhere.

**Markup stays markup.** `<td class=at>` is written as `<td class=at>`, not
translated into a call. There is no control flow in a template: the `for` and
the `if` are the language's, which is also why nothing has to be learned
beyond a path.

**One hole notation and one argument.** An earlier draft had a second sigil,
`$$n`, for a part built already, and a `parts` array to hold them. Both are
gone: a part is a field whose type is `Html`, and the reader no longer has to
know what a doubled sigil means. There is no second delimiter either: ERB's
`<% %>` without the `=`, its statement form, has no meaning here, because the
`for` and the `if` are the language's.

**The `=` is kept even though nothing needs distinguishing**, which is worth
saying because one form makes it look like ceremony. In ERB the opener says
which of four kinds of tag this is — `<%=` outputs, `<%` runs without
output, `<%#` is a comment, `<%%` is a literal `<%` — and the closer only says
where it ends, which is why it does not repeat the kind and why `-%>` can
carry something else instead. A symmetric closer earns its place where things
nest, as `</div>` does, and these do not. Borrowing the delimiter and then
spelling output as plain `<% %>` would invert the one form a reader of ERB is
surest of: there it means *do not output this*. Faithful is cheaper than
clever, and it leaves `<%#` and `<%%` sitting there already understood if this
ever wants a comment or an escape.

**Nothing is built.** It is Cove source; it travels in the app's directory and
the embedder checks it like any other module.

**`fill` renders a value itself, and that is structural rather than a
duplication.** Escaping what a path reaches needs the language's own text for
an arbitrary value, and `std.dynamic`'s renderers take a `RenderPath`, which
the lowering makes and nothing else does — `cove_schema::builtins` asserts that
no `core.*` intrinsic answers one. A standard library module that is not the
lowering's callee therefore cannot call them. `fill`'s own renderer agrees
with `"{value}"` for everything a page carries; where `std.dynamic` uses the
path to notice a vector holding itself, this bounds depth at sixteen and
writes `…` past it.

**The binding is a run-time check, and the run-time check is `render`'s.**
This paragraph said the trade was safe because "a path that reaches nothing is
a programming error a test catches", and `fill` could say so loudly because it
knows the field names. **The second half was true and the first half was
false.** No test catches it, because a marker is text: the program this ADR is
measured against was given one misspelt hole on a page that renders on every
view of it, and `check` reported nothing and all 58 tests passed. A page would
have shipped with the marker in it.

So the check is still at run time — compile-time binding would need either a
macro system or syntax, and ADR authors before this one have declined both for
less — but it is a *failing* check and not a visible one. `fill` records what
it knows and `render` answers `Err`, which is the one shape a test and a
request both already understand.

### What it does to the program it was measured on

Of the admin app's 104 page functions, **43 already take a `host.*` struct**
that the embedder hands over with its fields named, so they need no loader at
all: `{h(app.name)}` becomes `<%= name %>`.

It is **not a line-count win and should not be sold as one** — roughly flat.
What moves is that 179 escaping decisions become none, and that markup
repeated by hand collapses: `<p class=muted>` appears 24 times in that one
file, and the four figures of its "Now" strip are four copies of one template.
What is added is a small view-model struct wherever a value is computed rather
than carried — perhaps twenty of them.

## Alternatives rejected

**A tree of element calls** — `td([cls("at")], [t(x)])`. It translates HTML
into calls, which is the notation this was meant to avoid, and its brackets
cannot be removed: §1 above closed the only door. Measured against the same
page it was *longer*, not shorter.

**A template language.** Control flow in markup, a second syntax to parse,
check, format and colour, to express what `if` and `for` already express.

**A fallible `fill`.** This entry is narrower than the one it replaces. The
first draft of this ADR rejected `Result` outright, on the grounds that it
would poison every call site of a function used once per element — and that
is right *about `fill`*, which is called once per row and once per cell in
code whose whole value is that it reads like the markup it makes. It is not
an argument about `render`, which is called once per page at the boundary
where markup becomes a response, and where one `?` sits beside the half dozen
already there. Generalising the objection from the one to the other is what
left a programming error with no gate to fail, so what is rejected here is a
fallible `fill`, and `render` is fallible.

**A second sigil for parts** (`$$n`, with a `parts: Array<Html>` argument).
It put the escaping decision in the template's punctuation, where a reader
cannot see what it means, instead of in the type of the thing being put in.

**`Json` as the data interface.** It works and stays possible, but it is an
enum rather than a struct with named fields, so it needs a branch of its own
in `fill`, and it gives up all three of the advantages listed above.

**`{{ }}` and `${ }`.** The conventions everybody knows, and Cove owns `{`:
interpolation makes a literal brace `\{`, so the one place a template has to be
readable would be written `\{\{ name \}\}`. Unavailable rather than rejected.

**A bare `$`, and then `$.`** — what this ADR proposed first, and the patch to
it. A bare `$` cannot coexist with a literal dollar: `100$</p>` and
`$</table>` are the same three characters, and no rule reading left to right
tells them apart. That finding stands whatever the delimiter is, and it is why
every sigil-only form loses. `$.` fixed that one problem and kept the rest of
them: no end to the hole, so a malformed path becomes markup and an unclosed
one is no error at all; and `$.` is nobody's convention, so there is nothing a
reader already knows about it.

**Bare `?` holes.** 19 collisions with query strings in one app; escaping them
would make nearly every URL in it read `??key=`.

**`[[ ]]` and `$( )`.** Both measured at 0 occurrences in the pages, so
neither collides — and neither is anybody's convention. `$(` is jQuery's,
which is a different language's meaning for the same two characters, and a
delimiter's whole value is that it is recognised.

**Positional bare holes** (`^`, `~` — both measured at 0 occurrences). No
counting, but they swap silently when markup is rearranged.

## Open

- **An `Html` is recognised by its shape, not its identity.**
  `core.dynamicTypeName` answers a declared name with no module on it, so
  `fill` asks instead for an opaque struct named `Html` holding two `String`s,
  the markup and the problem.
  A program that declares the same shape is indistinguishable — which costs
  nothing it did not already have, since such a program could call `markup`
  and be believed. A qualified type name would close it, and that is a
  language change, not this ADR's.
- **Whether `[n]` earns its place.** An `Array<Html>` goes in whole, so
  wanting one element of an array is rare, and an index is the one part of the
  path notation that invites logic into a template. It is included because it
  costs nothing — and, now that a hole has an end, dropping it would not even
  simplify the scanner, only the path grammar inside it. It is still the first
  thing to drop if it is used for branching.
- **`Json` needs one branch**, since its fields are a `Map` inside an enum
  case rather than a struct's.
- **A problem propagates only where an `Html` is put in as markup.** An
  `Html` reached through the rendering path instead — inside a struct put in
  whole, or in a list that is markup only in part — prints as the opaque type
  name and its problem is dropped. Propagating from data that was never put
  in would be worse, since it would refuse a page for a fragment it did not
  use, so this stands; it is recorded because it is the one seam in
  "composition propagates it".
- ~~This is unverified end to end.~~ **It is implemented**, in
  `crates/cove-sema/std/html.cove`, with 28 cases in `tests/e2e/std_html`
  passing on both backends: escaping between tags and inside an attribute's
  quotes, a nested path, an index, an `Html` field going in whole, an
  `Array<Html>`, `<%= . %>` as the data, the space inside the delimiters
  trimmed, a template holding `$5.00`, `?over=hour`, `[[x]]`, `$(y)`, `100%`
  and `{` untouched, a non-`String` scalar, and — since the markers were
  replaced by a refusal — a bad field name making `render` answer `Err`, the
  `Err` naming the path and the template, a bad hole inside a nested fragment
  still reaching the outer `render`, an index past the end refused too, and
  two bad holes answering the first. The hole
  notation is the one part of the Decision that changed under implementation,
  and it changed twice: a bare `$` could not coexist with a literal dollar,
  and `$.`, which fixed only that, gave way to `<%= %>`, which fixes the rest
  and is somebody's convention. Both are recorded under "Alternatives
  rejected" rather than removed.
- ~~What `fill` does with a path that reaches nothing.~~ **Decided**, and the
  decision is in the Decision: a path that names no field makes `render`
  refuse, and so does an index past the end, and the marker is gone. This
  item is listed as closed rather than deleted because the first answer —
  recorded there — was the wrong one, and the way it was wrong is the useful
  part of the record.
