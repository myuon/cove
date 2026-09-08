# ADR 0047: A code point is written as a character and is an `Int`

- Status: Accepted
- Date: 2026-09-08
- Decides: how a Unicode scalar value is written in Cove source, now that
  programs have to write them
- Supersedes nothing.
  [ADR 0046](0046-a-byte-offset-is-a-value-a-string-hands-out.md) decided that
  a code point **is** an `Int` and named the price of having no way to write
  one; this decides how to write one and does not disturb that. In particular
  it does not add a `Char`, and 0046's reasoning against one stands unedited

## Context

ADR 0046 gave Cove `String.codePointAtByte`, and with it the first programs
that compare code points. It also recorded, under Consequences, what that
cost:

> **There is no readable way to write a code point.** Cove has no top-level
> `let` or `const`, so a scalar constant is a magic number with a comment or a
> zero-argument function, and the function costs 20% in a loop. […] It was not
> designed away, and it is recorded here as the price rather than hidden. If
> it becomes the recurring friction PHILOSOPHY.md's "Earn complexity through
> use" asks for, the answer is a code-point literal that is an `Int` — new
> syntax, which would have to earn its place on that evidence rather than on
> this sentence.

That is the condition this ADR is cashing in, and the evidence is in the tree
rather than imagined. `examples/cq/json`'s escape decoder was left reading

```cove
match character {
  34 => Ok("\"")                                                  // `"`
  92 => Ok("\\")                                                  // `\`
  47 => Ok("/")                                                   // `/`
  110 => Ok("\n")                                                 // `n`
  116 => Ok("\t")                                                 // `t`
  114 => Ok("\r")                                                 // `r`
  117 => Err(scan.fail("`\\u` escapes are not supported"))        // `u`
  98 => Err(scan.fail("`\\b` is a character Cove cannot write"))  // `b`
  102 => Err(scan.fail("`\\f` is a character Cove cannot write")) // `f`
}
```

where the entire content of every arm is in the comment, and the comment is
not checked against anything. `examples/cq/csv` has the same shape, and
`benches/bytescan` and `benches/stringlib` too. **Fifty code points are named
across those four files** — 36 in `json.cove`, 7 in `csv.cove`, 5 in
`stringlib`, 2 in `bytescan` — every one of them a number with a comment
beside it that nothing checks. That is what "recurring" means here.

**The alternative was measured and rejected in 0046.** A zero-argument `fn`
per constant is the only thing the language offers today, and it costs a call
on the path this exists for: `benches/bytescan` against `bytescan_call` is
418 ms against 504 ms, **20%**, for exactly that.

## Decision

**`'a'` is a literal whose type is `Int` and whose value is a Unicode scalar
value.**

- `'a' == 97`, `'é' == 233`, `'😀' == 128512`.
- It holds **exactly one scalar**. `''` is an error, `'ab'` is an error, and
  so is a grapheme cluster written as one character and made of several
  scalars — `'👨‍👩‍👦'` is five, and is refused as five.
- Its escapes are **the string's, and `\'` besides**: `\\`, `\"`, `\'`, `\n`,
  `\t`, `\r`, `\0`, `\{`, `\}`.
- There is no `\u{...}`, in this form or in a string.

### It is a literal, not a type

There is still no `Char`. ADR 0046 rejected one on its cost — a new builtin
aggregate is `MapEntry`-scale, with its own `BuiltinType` variant, its own
`Ty` variant across about twenty-five exhaustive matches in
`crates/cove-sema/src/typeck.rs` alone, an interpreter construction branch,
VM layout handling, four lowering files and the embedding boundary — and
nothing here reopens that.

What this adds is a **spelling**. `'a'` lexes to `TokenKind::Int(97)`, which
is the token `97` lexes to, so the parser, the resolver, the checker, the
lowering and both evaluators are unchanged and do not know the form exists.
That is the whole of what it costs them, and it is why this is a small change
where a `Char` would not have been.

The consequence a reader has to accept is that **`"{'a'}"` prints `97`**. That
is not a wart to be fixed later; it is what "a code point is an `Int`" means,
and a form that printed `a` would be a `Char` with the sign changed.

### Exactly one scalar, and a grapheme is not one

The rule is stated in scalars rather than in "characters" because the two are
not the same thing and the difference is the reason to be strict. A combining
pair and an emoji sequence are each written as one character and are each
several scalars, and there is no code point either of them could mean. A
literal that quietly took the first, or the last, would be a silent wrong
answer in exactly the text that is hardest to test.

### One escape table, and `\'` is in it

The rule is *one* rule: a code-point literal takes the escapes a string takes,
plus `\'`. Making that literally true in the code — one `escaped_char`
function both literal forms call — means `\'` is now legal inside a string as
well, where the apostrophe never needed escaping.

**That widening is deliberate.** The alternative is two tables, which is two
rules to remember and two places for them to drift apart, in exchange for
refusing something no program wanted to write and nothing is harmed by.

### No `\u{...}`

Cove has no `\u{...}` today, in strings or anywhere. Adding it to the
code-point literal alone would create exactly the asymmetry the paragraph
above avoids: `'\u{1F600}'` legal and `"\u{1F600}"` not.

And nothing has asked for it. **Every code point named by a program in this
repository is ASCII** — `{`, `}`, `[`, `]`, `"`, `\`, `/`, `,`, `.`, `+`,
`-`, `0`, `9`, `e`, `E`, and the whitespace escapes. A non-ASCII scalar can be
written as the character itself, which works, and one computed at run time
goes through `String.fromCodePoint`, which already exists.

When something needs it, it goes into `escaped_char` and **both** forms gain
it in the same change. Until then it is syntax that has not earned its place.

## What it cost, and the two things that would have broken quietly

The lexer, and nothing else in the pipeline. `'` was entirely unused in Cove's
grammar — there are no lifetimes, no raw-string prefixes, no character-class
syntax — so a `'` in source was `cove::lex::unexpected_character` and no valid
program contained one.

Two places outside the lexer did have to be taught, and both would have failed
silently rather than loudly.

**`cove fmt` would have deleted the form.** The formatter prints an integer
literal by slicing the original source at the expression's span, guarded by a
predicate that requires the first character to be an ASCII digit. `'a'` fails
that, so the fallback would have rendered the parsed value and the first
format of any file would have rewritten every `'a'` to `97` — the feature
working perfectly and disappearing on save. It needs a predicate of its own
rather than a widening of the numeric one, because that one also refuses a
span containing whitespace and `' '` is a code-point literal whose entire
content is a space.

**A brace inside a code-point literal ended an interpolation.** A `"{ ... }"`
is scanned for its closing brace before its body is parsed, stepping over
nested braces and nested strings. It did not know about apostrophes, so
`"{ head == '\{' }"` counted the brace inside the literal and reported an
unterminated interpolation. A `'...'` is now stepped over whole, and inside
one a brace is a character — which a `"..."` deliberately does *not* do, since
a nested string may itself interpolate.

Both are pinned by tests, in `crates/cove-syntax/src/format.rs` and
`crates/cove-syntax/src/lexer.rs`.

## What it bought

`examples/cq/json`'s escape decoder, after:

```cove
match character {
  '"' => Ok("\"")
  '\\' => Ok("\\")
  '/' => Ok("/")
  'n' => Ok("\n")
  't' => Ok("\t")
  'r' => Ok("\r")
  'u' => Err(scan.fail("`\\u` escapes are not supported"))
  'b' => Err(scan.fail("`\\b` is a character Cove cannot write"))
  'f' => Err(scan.fail("`\\f` is a character Cove cannot write"))
  -1 => Err(scan.fail("a backslash ended the line")) // end of text
}
```

It is now the JSON specification written out, and the comments are gone
because the code says what they said. `isDigit` is
`codePoint >= '0' && codePoint <= '9'`, `isSpace` is
`codePoint == ' ' || codePoint == '\t' || …`, and a digit's value is
`digit - '0'`.

**It costs nothing at run time.** A code-point literal is an `Int` immediate
by the time anything executes, so `fuel_spent` and `allocated_words` for
`benches/chars`, `benches/bytescan`, `benches/stringlib` and `examples/cq`
over the 100,000-record file are identical to ADR 0046's, digit for digit.
This is the rare change where "no measurable cost" is a statement about the
representation rather than about a benchmark's noise floor.

## Consequences

- **`"{'a'}"` prints `97`.** Accepted, and it is what the decision means.
- **`\'` is legal in a string.** A widening, taken so that one escape table
  can be one rule.
- **`-1` is still the end-of-text sentinel** in both parsers' scanners. It is
  not a code point and is deliberately not written as one.
- **`'` is now taken.** Any future syntax wanting the apostrophe — a raw
  string, a label, a lifetime — has to find another character, and a stray
  apostrophe in source now runs to the next one before reporting rather than
  reporting where it stands. That is the same failure mode an unclosed `"`
  has, and it is the cost of taking a delimiter.
