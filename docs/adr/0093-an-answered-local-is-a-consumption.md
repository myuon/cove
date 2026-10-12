# ADR 0093: An answered local is a consumption

- Status: Accepted
- Date: 2026-10-13
- Refers to: [ADR 0001](0001-mvp-language-design.md)'s "conservative, local
  uniqueness checking"; [ADR 0052](0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md)'s
  "the same conservative local uniqueness proof" for a finish;
  [issue #602](https://github.com/myuon/cove/issues/602) (the cove-tools
  program), [issue #637](https://github.com/myuon/cove/issues/637) (the
  decision); [PHILOSOPHY](../PHILOSOPHY.md)'s "Earn complexity through use"
- Supersedes: nothing. No ADR decided that a named local is never fresh; the
  line was drawn in `crates/cove-sema/src/unique.rs`'s documentation of
  `Freshness`, which this changes
- Decides: a declared function answers fresh storage also when an answering
  expression names a local of its own, provided the body's existing per-place
  proof — the one a `freeze()` of that local in the same position would need —
  goes through

## Context

ADR 0001 says that `Vector.freeze()` is O(1) only where "the compiler ...
performs conservative, local uniqueness checking", and that `toArray()` is the
fallback. ADR 0052 put a `ByteBuffer`'s finish under the same proof. The proof
lives in `crates/cove-sema/src/unique.rs` and asks four things of the place a
consumption names: it was initialised by a creation; nothing copied it (no
`let`, `var` or assignment made a second handle); it did not escape (no
closure captured it, nothing stored it, no call that could keep it was given
it, no `return` carried it out); and nothing reads it afterwards. A site in a
loop or closure body also needs its binding created inside that region.

"A creation" was widened once already, by `Freshness` (#357): a call to a
declared function counts when every expression the function can answer with —
its tail and every `return` operand — is *syntactically* a construction:
`Vector.of(..)`, a call already proved fresh, or a struct literal over those.
Its documentation drew the next line on purpose: "`return x` naming a variable
is not a construction, whatever `x` holds", because admitting a name turns
the question into "did anything else in this body get a handle on what that
name holds", which it called "a whole analysis and not a syntactic test".

The cove-tools algorithm playground hit exactly that line. Its helper

```cove
fn filled(n: Int, v: Int) -> Vector<Int> {
  var out = Vector.of()
  for _i in 0..n {
    out.push(v)
  }
  out
}
```

is the ordinary way to build a sequence, and a caller writing
`var a = filled(n, -1)` … `a.freeze()` was refused, so it paid `toArray()`'s
copy for storage nobody else held. Nothing about the shape is unusual; it is
what a reader writes before they know the proof exists. The analysis on #602
noted that the "whole analysis" the documentation feared already exists: it is
the per-place proof itself, which answers precisely "did anything else in this
body get a handle on this place" for every `freeze()`.

## Decision

**An answering expression that names a place rooted at a local binding of the
body is read as a consumption of that place, and the function's answer counts
as fresh when that consumption proves.** The consumption is the same `Consume`
record a `freeze()` produces, and it is checked by the same `prove` over the
same `Scan` of the body, with:

- the span of the answering expression, so that the answer's own read is the
  consumption and not a second handle — exactly as a `freeze()`'s receiver
  read is;
- the regions (loop and closure bodies) the answering expression sits in;
- `terminal` set, because nothing in the function runs after an answer — the
  same reason `return x.freeze()` has nothing after it to check.

A place rooted at anything else — a parameter, `self`, a module name, a
pattern's binding, a `for` binding — is refused before the proof is asked,
because it has no creation in this body to point at. The diagnostics `prove`
writes are discarded: a refusal is a function that is not in the fresh set,
and the caller's own consumption is where a reader is told why.

The set is still a monotone fixpoint. A local initialised by a call to a
function already in the set is a creation (the existing `establishes`), so a
fresh helper of a fresh helper proves in the round after its callee does.

Not in scope, and still refused: an answer that is an `if` or `match` over
locals, two answers naming the same local (each sees the other as "is
returned"), an answer from inside a loop the local was created outside of, and
the re-initialised `var` in a loop of #603, which needs flow sensitivity.

## Why it is sound

Suppose `f` is admitted and a caller binds `var a = f(..)`. The caller's proof
of `a` relies on one fact: when `f` returns, no live handle other than its
result reaches the storage. Take an answer naming local `x` (or a field path
under it):

1. **The storage was created unaliased.** `x`'s initialiser is a construction
   (`establishes`): a builtin fresh call, a call already proved fresh — by
   induction on the fixpoint, unaliased at its return — or a literal over
   those. At its creation, `x` is the only handle.
2. **`x` still names that storage.** Any assignment to an overlapping place is
   a write and refuses. So at the answer `x` holds what it was created with.
3. **No other handle was made.** Every way the language has to copy a handle
   is a read the scan marks as retained, and any retained read of an
   overlapping place other than the answer itself refuses: a `let`/`var`
   binding, an assignment's value, an array literal element, a `return`
   (including a second answer), a trailing closure, an argument to a call
   whose result can hold an owner, beside a `var` argument, or beside another
   owner-bearing operand, a declared method's receiver when its result can
   hold an owner, and — since this ADR — a `match` scrutinee some arm's pattern
   binds a name of. A read at a closure depth deeper than `x`'s is a capture
   and refuses. A `Vector` and a `ByteBuffer` cannot cross a task boundary or
   be held by a host, so those are all the ways out of a call.
4. **Nothing reads it afterwards.** There is no afterwards: an answer ends the
   function. A `return` inside a local `fn` is collected as an answer too —
   over-collection only ever adds a conjunct — and a lambda cannot `return`.
5. **It does not run twice over one creation.** An answer inside a loop or
   closure body the binding is outside of refuses, as a `freeze()` there
   would.

So at the return, the only handle is the one being answered, which is the
fact the caller needs. This is the argument the existing proof already makes
for a `freeze()`; the answer is a `freeze()` whose result happens to be the
storage itself instead of a relabelled `Array`.

### A gap the argument found, closed here

Point 3 was not true before this ADR, for `freeze()` either. A `match` read its
scrutinee as keeping nothing, and a pattern's binding is a local with no
initialiser, which no proof is ever asked about. So

```cove
var out = Vector.of(1)
match out {
  v => sink.push(v)
}
out.freeze()
```

proved, while `sink` held the same storage. The scrutinee is now a retained
read ("is bound by a match pattern") when some arm's pattern binds a name and
the scrutinee's type can reach an owner (or the checker did not settle it). A
pattern of literals and wildcards still keeps nothing. Nothing in the
repository relied on the gap.

## Consequences

- `filled` and its callers prove; the motivating program is pinned as a test,
  with a helper-of-a-helper chain and the `StringBuilder` version.
- Every negative test written for `Freshness` still refuses, each for a reason
  the per-place proof names rather than because names are refused:
  `handed`/`toVector`-renamed answer a *parameter* (no binding this body
  made); `Holder.toVector` answers `self.items` (rooted at `self`); `wrap`
  answers a struct literal over a parameter (names no place, and is not a
  construction); `leaked` answers `out` after `stash(out)`, whose result can
  hold a buffer, so `out` escapes; the shadowing local `fn make(seed)` is a
  call through a value, and its tail `seed` is a parameter of its own.
- New negative tests pin each way an answered local can be held elsewhere:
  bound or assigned into another variable, stored in a struct or an array
  literal, captured by a closure, passed to a call that could keep it, bound
  by a `match` pattern, answered beside a non-fresh answer, created from or
  reassigned from a parameter, answered from inside a loop it was not created
  in.
- The Reference's list of `cove::unique::not_unique` conditions says what a
  proved-fresh call is, and that a `match` pattern makes a second handle.
- The cost is one `prove` per named answer per fixpoint round, over a scan
  that already exists; no new pass, no new table.
