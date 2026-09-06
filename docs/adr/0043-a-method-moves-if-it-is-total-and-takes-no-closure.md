# ADR 0043: A method moves if it is total and takes no closure

- Status: Accepted
- Date: 2026-09-06
- Supersedes: [ADR 0042](0042-a-builtin-is-a-primitive-a-library-or-a-capability.md)'s
  **Library** test, and its `Duration` row
- Decides: two conditions ADR 0042's test did not have, and one way of
  counting that ADR 0042 got wrong
- Implementation status: the conditions are drawn from work that shipped.
  Twelve methods moved in [PR #259](https://github.com/myuon/cove/pull/259);
  three named candidates did not, each for a different one of the reasons
  below

## Context

[ADR 0042](0042-a-builtin-is-a-primitive-a-library-or-a-capability.md) sorted
every builtin into primitive, library or capability, and said a method moves
to the library **if Cove can already say it**. That test was written from a
survey. Then a wave of migrations was actually done, and three separate
things stopped methods that the test admits.

None of the three is an exception to be noted beside the rule. Each is a
condition the rule was missing, and each was found by running code rather
than by reading it — which is the only reason this ADR exists a day after the
one it supersedes.

## Decision

A builtin method moves to the standard library if Cove can say it **and all
three of these hold**. ADR 0042's test is the first clause; the rest are new.

### It must be total

A method that can fail stays primitive until a runtime error can name its
caller.

`Int.abs` traps at the least `Int`. Written in Cove as
`if n < 0 { -n } else { n }` it traps at the same value on both backends —
the *behaviour* is identical, and that is not the problem. The diagnostic
moves:

```
  --> main.cove:12:15          →    --> std/int.cove:9:14
12 |   assertEqual(low.abs(), low)    9 |   if n < 0 { -n } else { n }
   |               ^^^^^^^^^              |              ^^
```

**The caller disappears.** Cove's runtime errors carry no call stack, so a
program with forty `abs` calls is told only that one of them was at the
minimum, in a file it did not write. [Issue #258](https://github.com/myuon/cove/issues/258)
is where that gets answered; until it is, a fallible method is worth less in
the library than it costs at the call site.

Everything that moved in wave 1 is total, which is exactly why this took
until `abs` to notice.

### It must not take a closure

A program writes

```cove
Int.parse(text).mapError { ParseError.NotANumber(text) }
```

with a trailing closure that **names no parameter**, and both evaluators had
a special case for it — `if host.arity(&callback) != Some(0)` in the
interpreter, `match (func.params.is_empty(), ...)` in the lowering — passing
such a closure nothing at all.

Cove source cannot write that. A standard-library body calling `body(error)`
passes one argument always, and a closure that names no parameter refuses it.
**The affordance belongs to the call site and the library has no way to reach
it.**

This is not a fact about `mapError`. It stands in front of `map`, `filter`,
`fold` and `sorted` — which ADR 0042 listed as blocked *on a measurement*.
They are not blocked on a measurement. They are blocked on a language
question, and no benchmark will move them.

### Its Rust must be per-method

A method whose implementation is *shared* by several methods moves nothing
by moving. This is the one that is a counting error rather than a discovery,
and it is why ADR 0042 named the wrong row as its biggest win.

`Duration` has thirteen schema entries and **two** implementations: one
builder and one reader, each parameterised by a table

```rust
fn duration_unit(name: &str) -> Option<i64> {
    Some(match name { "nanos" => 1, "micros" => 1_000, ... })
}
```

so `micros`, `millis`, `seconds`, `minutes` and `hours` are rows in a
constant, not code. Migrating the five readers deletes **no Rust at all**:
the builders keep the table, and `nanos` keeps the branch, because a
`Duration` cannot be constructed or read down to nanoseconds without knowing
its representation.

What it would add is five Cove functions, five table rows, a module, another
renumbering of every lambda in the lowering tests, and a call frame where
there was one instruction. **The trade is negative and the count is what
hid it.**

So: count implementations, not schema entries. The four `isEmpty`s that moved
in wave 1 were four *pairs* of Rust arms. `Duration`'s thirteen entries are
two.

## What this does not change

ADR 0042's three-way split stands, and so does everything else in it. A
primitive is still something that must know the machine's representation,
memory or scheduling; a capability is still something that reaches outside
the process; **performance alone still does not make a primitive**, and the
`Map`/`Set` reasoning — that Cove has no way to hash an arbitrary `K` — is
untouched.

This ADR narrows one clause of one of the three definitions. Every method
ADR 0042 classified as primitive stays primitive for the reason it gave.

## Consequences

**Most of what is left is blocked on two language questions, not on work.**
Fallibility gates `Int.abs`, `Int.parse`, `Float.parse`, `slice`, `get`, the
five `Duration` builders, and every bound-checking method. The closure rule
gates `map`, `filter`, `fold`, `sorted` and `mapError`. Between them that is
most of the interesting surface, and neither is answered by migrating
anything.

That is a better position than it sounds. Before wave 1 the plan was a long
list of migrations of unknown difficulty; it is now a short list of decisions
with a long list behind each.

**`Duration` is struck from the migration plan.** Not deferred — there is
nothing behind the block to collect.

**Two facts about the language were established by needing them**, and both
are worth having on the record because nothing else had asked. `fn f<T, E>`
had never been checked: no `.cove` file in the repository declared a function
with two type parameters, the grammar accepted one, and a parser unit test was
the only witness. It works, and `Result`'s migrated methods are the first
checked programs to prove it. And `unwrapOr` is strict — the inlined form
computed the fallback before it branched — so migrating it changed nothing.
Had it been lazy it could not have moved at all.

**A migration is verified by whether the oracle takes the binding, not by
whether it still answers.** Twice now the tree-walking interpreter has gone
on answering a migrated method in Rust while the lowered backend took the
standard library, and twice the differential corpus said nothing, because
both answers were right. The second time, the hook was gated on a comment's
claim that a builtin receiver answers `None` to `declared_type_name` —
`Option` and `Result` are `Repr::Enum` and answer their own bare names. **The
corpus cannot see a divergence that is not one.** Delete the Rust, then run
it.
