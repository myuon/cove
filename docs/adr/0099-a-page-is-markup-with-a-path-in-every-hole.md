# ADR 0099: A page is markup with a path in every hole

- Status: Proposed
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
  `fill(template, data)`, whose holes are JSONPath into `data`; that **the
  type of what a path reaches decides whether it is escaped**, so an `Html`
  goes in whole and everything else is escaped; and that markup stays markup
  while loops and conditions stay in Cove

## Context

A server-rendered page is written today by joining strings. The program that
does the most of it is minicloud's admin app, and it is worth measuring
rather than describing:

- `apps/admin/pages.cove` is **2,018 lines**, almost all of it string
  building.
- It calls its escaping helper **259 times**. One missed call is an injection,
  in the one app that may change the host's settings, and nothing in the type
  system says which of its 581 interpolations need it. One 480-character
  expression has six: four escaped, two deliberately raw markup. Telling them
  apart means reading each callee.
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
4. **`?` is not available as a hole.** It appears 19 times in the admin app's
   markup and **every one is a query string** — `?key=`, `?over=`, `?n=`.
   `$` appears **0** times across four page-rendering files, and needs no
   escaping in a Cove string, which `{` does.
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
export fn render(page: Html) -> String
```

**A hole is a path, written as JSONPath:**

| | |
| --- | --- |
| `$` | the data itself |
| `$.name` | a field |
| `$.limits.maxQueued` | a field of a field |
| `$.items[2]` | an element |

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

fill("<tr><td class=at>$.at</td>$.who<td class=said>$.said</td></tr>", row)
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
  fill("<tr><td class=at>$.at</td><td class=said>$.said</td></tr>", line)
}

// `$` is the data itself, so wrapping needs no struct
fill("<div class=scroll><table class=log>$</table></div>", rows)
```

**Paths, not positions.** An earlier draft used `$1`, `$2`. A path does not
move when the markup is rearranged — reordering columns, adding a cell —
which is the edit people actually make; positional holes swap their values
silently under exactly that edit. A path also reaches `$.limits.maxQueued`,
which a flat name could not.

**A struct, not a map or a `Json`.** Both would work. A struct is better for
three reasons that are not style:

- its construction is checked — a field cannot be forgotten, misspelled or
  given the wrong type, which `Map.of(MapEntry(key: "aat", …))` cannot say;
- `fill` can enumerate the field names, so **`$.nmae` is detectable**. Against
  a map, a name that is not a key is indistinguishable from a key whose value
  is empty;
- a field that is not a `String` needs no stringifying by the caller, because
  `dynamicKind` can tell an `Int` from a `Bool`.

## Consequences

**Escaping stops being a decision.** Every path escapes unless what it reaches
is an `Html`, including inside an attribute value, where `"` matters. The 259
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
know what a doubled sigil means.

**Nothing is built.** It is Cove source; it travels in the app's directory and
the embedder checks it like any other module.

**The binding is a run-time check, and that is a deliberate trade.** A path
that reaches nothing is a programming error a test catches, and `fill` can say
so loudly because it knows the field names. Compile-time binding would need
either a macro system or syntax, and ADR authors before this one have declined
both for less.

### What it does to the program it was measured on

Of the admin app's 104 page functions, **43 already take a `host.*` struct**
that the embedder hands over with its fields named, so they need no loader at
all: `{h(app.name)}` becomes `$.name`.

It is **not a line-count win and should not be sold as one** — roughly flat.
What moves is that 259 escaping decisions become none, and that markup
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

**A second sigil for parts** (`$$n`, with a `parts: Array<Html>` argument).
It put the escaping decision in the template's punctuation, where a reader
cannot see what it means, instead of in the type of the thing being put in.

**`Json` as the data interface.** It works and stays possible, but it is an
enum rather than a struct with named fields, so it needs a branch of its own
in `fill`, and it gives up all three of the advantages listed above.

**Bare `?` holes.** 19 collisions with query strings in one app; escaping them
would make nearly every URL in it read `??key=`.

**Positional bare holes** (`^`, `~` — both measured at 0 occurrences). No
counting, but they swap silently when markup is rearranged.

## Open

- **What `fill` does with a path that reaches nothing.** It can refuse, or
  render something conspicuous. It should be loud; which kind of loud is for
  the implementation.
- **Whether `[n]` earns its place.** An `Array<Html>` goes in whole, so
  wanting one element of an array is rare, and an index is the one part of the
  path notation that invites logic into a template. It is included because it
  costs nothing; it is the first thing to drop if it is used for branching.
- **`Json` needs one branch**, since its fields are a `Map` inside an enum
  case rather than a struct's.
- **This is unverified end to end.** The pieces were each checked by
  compiling — the `impl` rule, `positional_after_label`, the `?` collisions,
  `$` needing no escape, and `core.dynamic*` being a std privilege — and a
  positional prototype of `fill` was built and run against hostile input in an
  embedder. The path version cannot be prototyped from an application, because
  reflection is exactly what an application may not reach. Implementing it in
  `std` with an `e2e` test is the next step, and this ADR should not be
  accepted before that runs.
