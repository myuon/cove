# ADR 0094: A string names a scalar by number with `\u{...}`

- Status: Accepted
- Date: 2026-10-13
- Supersedes: [ADR 0047](0047-a-code-point-is-written-as-a-character-and-is-an-int.md)'s
  decision **"There is no `\u{...}`, in this form or in a string"**, and
  nothing else it decided. A code point is still an `Int`, there is still no
  `Char`, and the one escape table both literal forms share stands
- Refers to: [issue #638](https://github.com/myuon/cove/issues/638), the
  decision; [issue #602](https://github.com/myuon/cove/issues/602) item 1, the
  evidence; [PHILOSOPHY](../PHILOSOPHY.md)'s "Earn complexity through use"
  and "Syntax must earn its place"
- Decides: that string and code-point literals accept `\u{X…}`, one to six hex
  digits naming a Unicode scalar value, and refuse every other spelling of it

## Context

ADR 0047 gave Cove the code-point literal and decided, in the same breath,
that neither it nor a string takes `\u{...}`. The reason was evidence, not
taste: **every code point named by a program in the repository was ASCII**,
and a non-ASCII one could be written as the character itself. It named the
condition under which that would change — "When something needs it, it goes
into `escaped_char` and **both** forms gain it in the same change" — and
called the escape, until then, "syntax that has not earned its place".

Something has now needed it. Issue #602 records the friction met writing the
cove-tools webhook lab (myuon/cove-tools#8), a few hundred lines of Cove that
store and render HTTP requests, and its first item is this one: **the app had
to build non-ASCII characters by code point**, through
`String.fromCodePoint`, because writing the character itself is not always
an option. The characters it needed are the ones the paragraph in ADR 0047
did not consider — those that cannot be read when written: a non-breaking
space, a zero-width joiner, a byte-order mark, a combining mark that attaches
to the quote before it, a control character. Written as themselves they are
invisible in the source and in a diff; written through `fromCodePoint` they
are a `Result` to unwrap and an allocation at run time for what is a constant.

ADR 0047's other argument still holds and decides the shape: the escape goes
into both literal forms at once, so `"\u{1F600}"` and `'\u{1F600}'` are both
legal and mean the same scalar.

@myuon decided on #638 (2026-10-13) to adopt it, in Rust's and Swift's
spelling.

## Decision

1. **A string literal and a code-point literal accept `\u{X…}`**: a
   backslash, `u`, `{`, one to six hex digits of either case, `}`. It denotes
   the Unicode scalar value the digits name, so a string holds that scalar's
   UTF-8 encoding and a code-point literal is that scalar as an `Int`.
   `"caf\u{e9}"` equals `"café"`; `'\u{1F600}'` is `128512`.
2. **Everything else that begins `\u` is refused**, with
   `cove::lex::invalid_unicode_escape` pointed at the escape as far as it was
   read: a surrogate (`D800` to `DFFF`), a value above `10FFFF`, no digits, a
   seventh digit (leading zeros included, as in Rust), a character that is not
   a hex digit, a missing `{`, and a missing `}`. There is no `\uXXXX`, no
   `\xNN`, and no surrogate pair.
3. **The escape's braces are its own.** In a string a `{` opens an
   interpolation; the `{` after `\u` does not, and the brace after the
   escape's `}` opens one as it always did — `"\u{41}{name}"` is `A` followed
   by `name`.
4. **It is decoded by the lexer and nowhere else.** Nothing past the lexer
   knows a scalar was written as an escape, exactly as nothing past it knows a
   code point was written as `'a'`. Both formatters — the Rust one in
   `cove_syntax::format`, which is the oracle, and covefmt, which is
   `cove fmt` (ADR 0077) — **write the escape back as it was written**, never
   decoded: `\u{e9}` and `é` are one string to the compiler and two spellings
   to the person who chose one.

## Consequences

- **The lexer is the whole of the language change.** `escaped_char` stays a
  table of one-character escapes; `\u` is the one escape longer than a
  character, so both literal forms hand it to one function,
  `Lexer::unicode_escape`, and the two cannot drift apart.
- **Both formatters write a literal back from its source span**, so neither
  can decode the escape, and both have a test that keeps its spelling. What
  each had to be taught is its *scanner*, which skipped the one byte after a
  backslash: in a well-formed `\u{41}` the `{41}` that followed read as a
  balanced brace pair and ended where the escape ended, so nothing showed;
  but in `"\u{41"`, which does not lex, the `{` opened an interpolation that
  ran past the closing quote. covefmt tiles files that do not lex as well,
  and `covefmtBench` failed on `tests/e2e/fail_unicode_escape` until its
  `quoted` stepped over the escape the way the lexer reads it. The Rust
  formatter's comment scanner and the lexer's own interpolation skipping were
  given the same rule, so all four readers of a literal agree on where it ends.
- **The interpreter, the lowering, the VM and the native tier see a string
  constant**, as they would for the character itself;
  `tests/e2e/values_unicode_escape` holds the three to the same bytes, and
  `tests/e2e/fail_unicode_escape` pins every refusal's diagnostic.
- The cove-tools apps' code-point workarounds can now be written as literals.
  That is their change to make, in their repository.
- `copies.rs`'s ratchet rose by 24, all of it the new program and covefmt's
  new scanner rule and tests, none of it the lowering: with both held out the corpus answers
  the bound it had.
