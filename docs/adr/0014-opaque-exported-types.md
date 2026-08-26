# ADR 0014: Opaque exported types

- Status: Accepted
- Date: 2026-08-26
- Supersedes: [ADR 0005](0005-module-to-module-imports.md)'s account of what
  exporting a struct publishes. There, a declaration was public or private and
  exporting a struct published all of it — name, fields, and the constructor
  they synthesize. Here an export answers two questions for a struct rather
  than one: exported, and if so, how much of it.
- Implemented by: the PR that closes issue #75
- Implementation status: complete

## Context

Cove has declaration-level `export`. An `export` declaration is public; other
declarations are module-private, and that has been the whole of the rule since
ADR 0005 made it enforceable. For a struct, though, "public" has always meant
its representation, not just its name:

```cove
export struct User {
  name: String
  visits: Int
}
```

publishes the fields and, with them, the synthesized labeled constructor
`User(name: ..., visits: ...)`. Another module can write

```cove
User(name: "", visits: -1)
```

directly, which means the declaring module can neither require validation on
construction nor change which fields exist without breaking every caller. A
module can export a *name*, but it cannot export a *type* while keeping the
right to decide what is inside it. That is a real gap: `cove outline` and
`cove api diff` exist to describe a module's boundary, and a boundary that
cannot hide a representation is not much of one.

## Decision

**`export opaque struct` exports the type's name and its exported methods and
associated functions. It does not export the fields or the synthesized
labeled constructor.**

```cove
export opaque struct User {
  name: String
  visits: Int
}

impl User {
  /// The only way another module gets one.
  export fn create(name: String) -> Result<User, Error> {
    if name == "" {
      return Err(Error("a user needs a name"))
    }

    Ok(User(name: name, visits: 0))
  }

  /// What another module may ask of one.
  export fn label(self) -> String {
    "{self.name} ({self.visits})"
  }
}
```

A plain `export struct` is unchanged: the name, the fields, and the
constructor are all public, exactly as before. Adding `opaque` narrows that —
it does not add a new capability, it withdraws two.

### The declaring module is unaffected

Inside the module that declares `User`, it is an ordinary struct.
`User(name: ..., visits: 0)` and `user.visits` both work, because `opaque`
describes a boundary between modules and the declaring module is not on the
far side of its own boundary. A `test fn` belongs to the module it is written
in, so a test alongside `User` sees its representation even though `opaque`
is written on it — the test is not "another module" any more than the rest of
the module is.

### `opaque` only ever accompanies `export`

Writing `opaque` on a declaration that is not exported is
`cove::parse::opaque_not_exported`: "`opaque` describes an export, and this
declaration is not exported", because a module-private declaration has no
boundary for `opaque` to draw — it is already invisible to every other module,
representation included. The fix the diagnostic offers is `export opaque
struct`, or removing `opaque`.

### `opaque` applies to structs only

Writing `opaque` on anything else is `cove::parse::opaque_not_a_struct`:
"`opaque` marks a struct, not this declaration". Exporting an enum always
exports its cases, and there is no opaque enum. That is a deliberate
limitation and not an oversight: an enum exists to be matched on, and a
`match` must cover every case, so an enum a caller cannot match on is a struct
with extra syntax — none of the value an enum offers survives hiding its
cases. A module that wants to hide a variant's representation wraps it in an
`export opaque struct` and exports the operations it wants, which is the same
answer Cove already gives for hiding any other representation.

### Two alternatives, rejected

**Field-level export.** Fields and the synthesized constructor could default
to module-private, with `export` written on the members that should be
public. It was rejected because it scatters "what is public" across a
declaration instead of stating it in one place: the ordinary, fully-public
struct — still the common case — would carry `export` noise on every field,
and `cove outline` and `cove api diff` would need to represent a many-valued
state (which fields, exactly) instead of a two-valued one (public
representation, or none). Its one real advantage — exporting some fields and
not others — is an intermediate state Cove deliberately does not want: a
struct's representation is one thing, and a caller who can see part of it can
usually reconstruct the rest of the invariant anyway.

**An opaque enum.** Rejected for the reason given above — a `match` a caller
cannot write is not a smaller enum, it is a worse struct. The struct-wrapping
answer costs nothing an opaque enum would have saved.

### Enforcement is the type checker's

Naming a field or calling the constructor of an opaque type from outside its
declaring module is a compile-time error, not a runtime one:

```text
error[cove::type::opaque_field]: `User` is opaque here, so its field `visits` cannot be read
  rule: An `export opaque struct` exports its name and its exported methods;
        its fields belong to the module that declares it.
```

```text
error[cove::type::opaque_construction]: `User` is opaque here, so it cannot be built field by field
  rule: An `export opaque struct` does not export the labeled constructor its
        fields synthesize; only the module that declares it may write one.
```

Both diagnostics point the caller at the type's exported associated functions
or methods — `User.create` and `label` in the example above — because that is
the boundary the declaring module chose to publish instead.

A refusal is the whole diagnosis. Checking stops at the refusal rather than
going on to match the call against the fields, because a caller who does not
know the representation is exactly the caller who guesses at the initializer,
and answering that guess would publish through the diagnostic channel what
`opaque` withholds: `known labels: name, visits`, one `missing_argument` per
hidden field, and a source snippet quoting the declaring module's file.

### An opaque value renders as its name

`"{user}"` is `User`, with no fields shown, and so is every other rendering
of the value — `println`, an assertion's failure message, a trace.

The rule is unconditional: it holds inside the module that declares the type
as much as outside it. A rendering could in principle be made to depend on
who is watching, but nothing watching a string knows who produced it — a
value formatted in its declaring module is an ordinary `String` that can be
returned, stored, and printed anywhere. A boundary the checker enforces per
module cannot be enforced per module at the point a string is *read*, so it
is drawn once, at the point the string is made.

The cost is that the declaring module loses the default rendering it would
have had for its own type. The answer is the same one `opaque` gives
everywhere: export a method.

```cove
impl User {
  /// The form this module is willing to publish.
  export fn label(self) -> String {
    "{self.name} ({self.visits})"
  }
}
```

Without this, `println("{user}")` would print the field names, their order,
and their values to a module that may not name a single one of them — and
renaming a private field, which this ADR calls a compatible change, would
break every caller that had formatted the value.

### There is no general visibility hierarchy

No `public`/`private`/`protected`, and no per-field visibility. An exported
type's representation is either public or opaque; there is no third state and
no partial one. This is the same two-valued shape the rejected field-level
alternative would have broken.

### Outline and API diff read the same bit

`cove outline` prints `export opaque struct User` and lists its exported
methods, with no field listed at all — the representation is not part of the
outline, the same way a module-private declaration is not part of it.

`cove api snapshot` records the header `export opaque struct User` and no
`field ...` lines for it, so the interface hash an opaque type contributes
does not move when its fields change; `cove api diff` reports nothing for a
representation change that stayed opaque. Gaining `opaque` is classified
Breaking, because the fields and the constructor were withdrawn from callers
who may have used them; losing `opaque` is classified Compatible, because the
representation merely became public and nothing that worked before stops
working.

`cove fmt` round-trips the modifier and normalises the order to `export
opaque struct` regardless of which order it was written in source.

## Consequences

A module can now export a nominal type while keeping its representation free
to change, which is the property issue #75 asked for. The cost is the
limitations named above, stated plainly rather than left to be discovered:
there is no per-field visibility, so hiding one field means hiding all of
them; there is no opaque enum, so a variant an author wants to hide has to be
wrapped in a struct first; and a representation change to an opaque type is
invisible to `cove api diff` *by construction*, not because the tool analysed
the change and found it harmless. `cove api diff` cannot tell a compatible
representation change (renaming a private field) from a breaking one (an
invariant the caller depended on through a method that now behaves
differently) — it can only tell that the representation was never part of the
interface it hashes. That is the same bargain `opaque` makes everywhere else:
the boundary is coarse on purpose, and the fineness that field-level export
would have bought was rejected above.

Four further consequences, none of them accidents:

**An opaque type has no default readable rendering, and neither does the
module that declares it.** `"{user}"` is `User` everywhere, as decided above,
so a module that wants a value of its own opaque type to print as anything
more informative has to export a method that says what. The gain is that this
is the one rule that makes `cove api diff`'s silence honest: with the fields
shown by every `println`, renaming a private field would break every caller
that had formatted the value, and the tool that reports "no interface change"
would be reporting it about a change that broke people.

**`opaque` is now a keyword, and that is a source-compatibility break.** The
lexer has no soft-keyword mechanism, so the name is reserved everywhere an
identifier can appear: `let opaque = 1`, a parameter or field named `opaque`,
and `fn opaque()` all stop parsing on code that compiled before. Only the
position after a `.` is unaffected, matching how `test` already behaves.
Nothing in this repository used the name, and reserving `test` was the same
trade, taken for the same reason: reading `opaque` as a modifier only when a
declaration follows would make the grammar's answer depend on how far ahead
it has looked. The break is recorded here so that it is a decision rather
than a surprise.

**The boundary is per module, not per package.** A module and its own
sibling submodule are strangers under this rule: `auth.internal` may not name
a field of a type `auth` declares, exactly as an unrelated package could not.
There is no friend, package-private, or `internal` escape hatch. This follows
from ADR 0005 — a module is the unit of visibility, and `opaque` refines what
an export carries rather than introducing a second unit — but it means a
module that grows too large to hide a representation inside must publish the
operations its submodules need, like any other caller.

**Equality stays structural.** `==` on two values of an opaque type still
compares them field by field, so a hidden field is observable in the sense
that two values differing only in one compare unequal. What is not observable
is which field, what it holds, or that it exists at all. A module that wants
equality to mean something narrower defines the operation it wants and
exports it.

## How this was decided

Issue #75 laid out both decisions above as options — field-level export
against an `opaque` modifier, and whether to allow an opaque enum — and
recommended the design this ADR records as the default: `opaque` modifier,
struct-only. An alignment question asking whether that default was correct
went unanswered past its timeout, so implementation proceeded on the
recommended default rather than waiting further. If that premise is wrong,
the PR that closes issue #75 is the place to say so; the scope was kept
narrow enough that the alternative above remains available to switch to.
