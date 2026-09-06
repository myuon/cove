# ADR 0044: A callback takes what its type declares

- Status: Accepted
- Date: 2026-09-06
- Supersedes: [ADR 0043](0043-a-method-moves-if-it-is-total-and-takes-no-closure.md)'s
  **"It must not take a closure"** condition, which generalised from one case
  and was wrong to
- Decides: that a function type's arity is matched exactly, everywhere, with
  no builtin-specific exception; and what happens to the one call shape that
  relied on the exception

## Context

`Result.mapError` accepts two callback shapes. A program may write

```cove
Int.parse(text).mapError { ParseError.NotANumber(text) }
```

with a trailing closure that names no parameter, and the error is silently
not passed. Three separate pieces of the implementation cooperate to make
that work:

- `cove_sema`'s `Checker::map_error` special-cases the call, **reads how many
  parameters the closure declared, and builds the expected function type to
  match** — so the ordinary arity check never sees a mismatch;
- the tree-walking interpreter asks `host.arity(&callback)` and pushes the
  error only when it is not zero;
- the lowering matches on `func.params.is_empty()` and emits no operand when
  it is.

**This is the only such exception in the language.** `Array.map`, `filter`,
`fold`, `sorted` and the `Vector` equivalents already require an exact match
and say so — `expect_callback` in the interpreter errors on a wrong count,
`callback_matches` in the lowering refuses to lower one — and `Shared.lock`
rejects a zero-parameter closure outright. Every one of the corpus's calls to
those already writes the parameters out. `Scope.spawn` and `clock.timeout`
declare `fn() -> T`, so there is nothing to mismatch.

Two further facts shape what removing it costs.

**A trailing closure cannot declare parameters at all.** `parse_trailing_closure`
produces a lambda with `params: Vec::new()`, unconditionally; there is no
production for a parameter list in that form. So `mapError { ... }` is not a
short spelling of a one-parameter closure — it is *structurally* a
zero-parameter one, and the exception exists to paper over exactly that.

**`_` is not a parameter name.** The lexer gives it its own token and
`expect_ident` refuses it, so a closure that means to ignore its argument
must still name it. Nothing warns about a parameter that goes unread.

## Decision

**A function value has exactly the parameters the place that holds it
declares.** That sentence is already the checker's rule — it is the `rule:`
line on the existing arity diagnostic — and it now has no exceptions.

`Result.mapError`'s callback is `fn(E) -> F`, so a closure passed to it
declares one parameter. The three cooperating special cases are removed: the
checker's `map_error` stops rigging the expected type, the interpreter stops
asking the callback's arity, and the lowering stops branching on whether it
has parameters.

A callback that does not want its argument **names it anyway**. Six call
sites in the corpus do exactly that, and they become

```cove
Int.parse(text).mapError(fn(cause) { ParseError.NotANumber(text) })
```

This ADR does **not** add `_` as a parameter name. That would be new syntax,
and [PHILOSOPHY.md](../PHILOSOPHY.md)'s "Syntax must earn its place" asks for
recurring friction rather than an imaginable one: six unread parameters is
not that yet. If the noise becomes real, it is a small decision of its own,
and it is a better one for being asked separately.

## What this supersedes, and the mistake in it

[ADR 0043](0043-a-method-moves-if-it-is-total-and-takes-no-closure.md) made
"it must not take a closure" a condition on moving a builtin into the
standard library, and said the rule "stands in front of `map`, `filter`,
`fold` and `sorted`". **That was generalised from `mapError` alone and it is
false.** Those four already demand an exact arity, from both evaluators, and
no call site in the repository passes them a closure that declares fewer
parameters than they take. Nothing about them was ever blocked by this.

So the condition dissolves rather than narrows. After this ADR a standard-library
body may call a callback exactly as any Cove code does, and `Result.mapError`
becomes migratable like anything else.

What ADR 0043 said about `map`/`filter`/`fold`/`sorted` being blocked
therefore reverts to what [ADR 0042](0042-a-builtin-is-a-primitive-a-library-or-a-capability.md)
said in the first place: they are blocked on a **measurement**, because they
lower to walk instructions rather than to `CallBuiltin` and moving them
replaces an instruction with a loop.

ADR 0043's other two conditions stand untouched. A method still must be
**total** — [issue #258](https://github.com/myuon/cove/issues/258) — and its
Rust must still be **per-method** rather than shared, which is what struck
`Duration`.

## Consequences

**A silent adaptation becomes a diagnostic**, and it has to be all three
pieces or none. Removing the interpreter's `arity` check and the lowering's
`params.is_empty()` while leaving the checker rigging the expected type would
turn a working program into a crash rather than an error message, because the
checker would go on accepting what nothing downstream could run.

**The error a user sees must say what to write.** "this function takes 0
parameter(s), but 1 were expected here" is true and unhelpful on a trailing
closure, because a trailing closure *cannot* take one — the fix is not to add
a parameter to it but to stop writing it as a trailing closure. The
diagnostic says so.

**Six `.cove` files and about a dozen embedded test snippets change**, and
none of the six reads the error it discards. That is the honest cost of the
rule: the corpus gets slightly noisier at those six lines, and everything
else in the language gets one fewer rule to know.

**`mapError` is no longer special anywhere.** The schema declares
`fn(E) -> F`; the checker enforces it; both evaluators pass one argument. The
comment in `cove-schema` calling it "the one builtin whose callback has two
accepted shapes" goes away, because it does not.
