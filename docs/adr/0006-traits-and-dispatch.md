# ADR 0006: Traits and dispatch

- Status: Accepted
- Date: 2026-08-25
- Amends: [ADR 0004](0004-static-type-checking.md), whose "parametric and
  unbounded" generics become parametric with bounds
- Implemented by: PR #13; the `Snapshot` conformance this ADR makes definable,
  by PR #18
- Implementation status: complete — with one thing worth saying plainly: the
  MVP restrictions below are enforced by there being no grammar for what they
  forbid, so writing a supertrait or an associated type is an ordinary parse
  error rather than a diagnostic naming the restriction

## Context

The Language Card states: "Traits are nominal and explicitly implemented;
dynamic dispatch is distinct from generic static dispatch." Nothing of that
exists. `trait` is a reserved word no parser rule consumes.

Three other promises wait on it. Generic type parameters carry no bounds, so a
generic function can only move values it cannot inspect. The card's `Snapshot`
contract, which is how an independent mutable graph copy is requested, is a
trait. And the card's own sentence distinguishes two dispatch strategies that
have no way to be written down.

## Decision

Add nominal traits with explicit conformance, and make the two dispatch
strategies distinct in both syntax and generated behaviour.

### Declaration and conformance

```cove
/// A value that can render itself for a human.
export trait Display {
  /// Returns the human-readable form.
  fn describe(self) -> String
}

impl Display for Booking {
  fn describe(self) -> String {
    "booking {self.id}"
  }
}
```

A trait declares method signatures, with an optional default body. Conformance
is explicit: `impl Trait for Type` is the only way a type conforms. There is no
structural conformance and no blanket implementation.

Explicit conformance is what makes a trait's set of implementors a fact the
compiler can enumerate, which `cove impact` and API snapshots both need. It is
also the card's word, "explicitly implemented", taken literally.

### Static dispatch is the default

```cove
fn render<T: Display>(value: T) -> String {
  value.describe()
}
```

A bound on a type parameter is checked at the call site, and the method is
resolved to one implementation. This is the form that costs nothing, so it is
the form with the shorter spelling.

### Dynamic dispatch is written `dyn`

```cove
fn renderAll(values: Array<dyn Display>) -> String
```

`dyn Trait` is a distinct type whose values carry their implementation. It is
not a type parameter and cannot be used as one. A `dyn Trait` value is produced
by an ordinary conversion at the point a concrete value is used where
`dyn Trait` is expected.

The two are distinct in the semantic model, in the surface syntax, and in what
the runtime does, which is exactly what the card asks for. ADR 0001 named
`T: Trait` and `dyn Trait` as the candidate spellings; this accepts them.

### What a trait may not do in the MVP

No associated types, no associated constants, no generic methods on traits, no
supertraits, no operator overloading, and no trait objects with generic
methods. Each is a decision that should be forced by a representative program
rather than added because other languages have it.

Only trait methods whose first parameter is `self` may be called through
`dyn Trait`. An associated function has no receiver to dispatch on.

### Coherence

An `impl Trait for Type` is allowed only in the module that declares the trait
or the module that declares the type. This is the orphan rule, and it exists
so that a conformance cannot appear from a module neither party knows about.

## Consequences

Generic bounds become expressible, so ADR 0004's "parametric and unbounded"
becomes "parametric with bounds", which is the change that ADR anticipated.

`Snapshot` can be defined, which is what the card's mutation section ends on.

Method resolution gains a step: a method call on a generic type parameter
resolves through its bounds, and a call on `dyn Trait` resolves through the
value. The interpreter needs a representation for a `dyn Trait` value that
carries its type, and the type checker needs to enumerate conformances.

Dynamic dispatch is the first place where a Cove value's runtime
representation depends on its static type, since a `dyn Trait` value carries
what a concrete value does not. That is a real cost and the reason the two
spellings are distinct rather than inferred.
