# ADR 0036: An inference variable is not a kind of unknown

- Status: Accepted
- Superseded in part by [ADR 0038](0038-a-type-nothing-settles-is-not-a-program.md),
  in property 2 only: a variable nothing settles is still reported as
  `Unconstrained` is reported, and `Unconstrained` is an error rather than a
  warning now. The rest stands, and the claim that an inference variable
  never escapes the body that minted it is what ADR 0038 generalises
- Date: 2026-09-02
- Supersedes: nothing. [ADR 0016](0016-four-kinds-of-unknown.md)'s four kinds
  stand exactly as written; this says why `Unknown` has a fifth variant
  anyway, and what that variant is not allowed to be
- Decides: the boundary that
  [issue #240](https://github.com/myuon/cove/issues/240)'s Q9 needed, raised
  when local constraint inference was implemented

## Context

ADR 0016 decides that an unknown carries which kind of not-knowing it is, and
names four: `Recovery`, `DynamicBoundary`, `Unconstrained`, `Placeholder`. The
classification is about what a reader of `cove check` is told, and every kind
compares equal to every type, so *"knowing the kind decides what a reader is
told and never what type-checks."*

Issue #240 then decided that a local binding's type may be settled by its
later uses:

~~~cove
var log = Vector.of()
log.push(text)          // log: Vector<String>
~~~

Implementing that needs a type the checker can come back to. `Ty` has no
variable and no substitution, so an unconstrained unknown gained an identity —
`Unknown::Var(u32)` — and `Unknown` now has five variants against an ADR
titled "four kinds of unknown".

A reader who opens the enum will count five and reach for the ADR. This says
what they should conclude.

## Decision

**An inference variable is a transient state inside one body's check, not a
kind of not-knowing.** ADR 0016's four are unchanged and remain the complete
answer to "what does a `Ty::Unknown` a pass can observe stand for".

Three properties define it, and the third is the one that makes the first two
true:

1. **It is minted in exactly one situation** — a call produces a value whose
   type parameters neither its arguments nor its call site settled — and
   taken by exactly one thing: a `let` or `var` that wrote no type. A
   parameter, a return type and everything else a declaration publishes keep
   their explicit-type requirement, because nothing else can take ownership
   of one.
2. **It resolves before the body ends.** Either a later use settles it, or it
   is reported as `Unconstrained` is reported — a warning naming the binding
   — at the point the body finishes.
3. **It never escapes the body that minted it.** The facts recorded while a
   variable was open are rewritten when it resolves, so nothing downstream of
   the checker ever holds one. This is asserted rather than intended.

That last property is what keeps ADR 0016 true rather than merely
uncontradicted. A consumer of `Facts::ty` — a backend, a diagnostic, a
tooling pass — sees one of the four kinds or a settled type, and never a
fifth thing. The variant exists inside the checker for the length of one
body and is invisible from outside it.

**A future kind of not-knowing that a pass *can* observe is an ADR 0016
question and needs one.** This is not a precedent for widening the enum; it
is the statement of why one widening did not.

## Consequences

- `Unknown` has five variants and four kinds. That is a discrepancy a reader
  will notice, which is why it is written down here rather than left to a
  doc comment.
- The escape property has to be checked, not assumed. It is a `debug_assert`
  at the end of every body and a test that a fact recorded during inference
  holds the settled type.
- A conflict between two uses is an error of its own —
  `cove::type::inference_conflict` — rather than a kind of unknown. Two
  constraints that disagree are a fact the checker *knows*, which is the
  opposite of not knowing.
- Variable-to-variable unification is not implemented, so two open types
  meeting settle nothing. That keeps the resolution order irrelevant, which
  is what makes "resolves before the body ends" a property of the body rather
  than of the order its expressions were walked.

## What this does not decide

- whether variable-to-variable unification is ever added;
- whether the four eager `Unconstrained` sites — an empty array literal, a
  bare `None`, an unannotated lambda parameter, a struct parameter no field
  mentions — should defer to a variable instead of reporting where they are
  written;
- anything about inference outside a body.
