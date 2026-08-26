# ADR 0014: Opaque exported types

- Status: Accepted
- Date: 2026-08-26
- Amends: [ADR 0005](0005-module-to-module-imports.md), whose "only exports
  are visible" rule gains a second question for a struct — exported, and if
  so, how much of it
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

## How this was decided

Issue #75 laid out both decisions above as options — field-level export
against an `opaque` modifier, and whether to allow an opaque enum — and
recommended the design this ADR records as the default: `opaque` modifier,
struct-only. An alignment question asking whether that default was correct
went unanswered past its timeout, so implementation proceeded on the
recommended default rather than waiting further. If that premise is wrong,
the PR that closes issue #75 is the place to say so; the scope was kept
narrow enough that the alternative above remains available to switch to.
