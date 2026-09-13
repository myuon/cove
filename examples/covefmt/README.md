# covefmt

The front end of a Cove formatter, written in Cove: a lexer and a parser.

The intent is replacement rather than a second implementation: if this reaches
`cove fmt`'s output at an acceptable cost, the Rust formatter goes and the
toolchain formats Cove with Cove. That is what makes it worth writing — a
formatter that had to be kept in step with another formatter would be two
things that can drift, and this repository does not keep two of those.

## The contract is that the tokens tile the source

Every byte of a file belongs to exactly one token, trivia included, so joining
the runs reproduces the file exactly. `tiles` is that property written down and
`the_tokens_tile_the_source` asserts it.

It is not a nicety. `cove_syntax::ast` carries `///` doc comments and drops
`//`, `/* */` and the blank lines an author wrote, so `format_source` reads
them back out of the source by position and re-attaches them — and its own
documentation says a comment it cannot place exactly is moved to the nearest
following boundary rather than kept where it was. A lossless token stream
removes that whole class of difficulty instead of solving it a second time.

Every rule answers a range and none of them fails. An unterminated string, an
unclosed block comment, a byte the language does not spell: each is a token,
because the tiling has to hold for a file that does not lex, and while somebody
is typing that is every file.

## The tree tiles too

The parser is recursive descent. `use`, `fn`, `struct`, `enum`, `impl`, `trait`
and `type` are taken apart into the header a formatter has to lay out — the
words in front of them, the name, the generics, the parameters, the answer —
and a `Body` is the statements between its braces.

One level and not all of them. A statement's own tokens are still leaves: what
an expression is made of is the next slice's, and until then what the tree adds
is exactly the boundary a formatter needs first — where one statement ends and
the next begins. A `{ ... }` written inside a statement is a `Body` too, so an
`if`'s block holds statements rather than a run of leaves.

## Where a statement ends

`docs/LANGUAGE_REFERENCE.md` gives the rule in four parts, and all four are in
`Parser::statement` because they interact — the continuations are what stop the
line rule from cutting an expression in half.

| | |
|---|---|
| A statement ends at the end of a line; Cove has no `;` | `atLineEnd` |
| An operator at the end of a line carries it on: `a +` then `b` is one expression, `a` then `+ b` is two statements | `continues`, which asks about the token *before* the newline |
| A line beginning with `.` continues the chain above it | `opensWithDot` |
| A break inside `(` or `[` ends nothing — but a `{ }` block is not such a group, and its statements *do* end at line ends | depth counts the two brackets; a brace opens a body |
| `break`, `continue` and `return` end at their line whatever encloses them | `escapes` |

Two things this got wrong first, both caught by a test and worth not repeating.
A statement must end *in front of* the brace that closes the body it is in, or
`fn a() -> Int { 1 }` becomes a statement that ate the brace. And a statement
must be asked whether it is over *after* taking a nested block, because a block
is where a statement most often ends: `if a { b }` is done at the brace unless
an `else` or a method call follows on the same line, and reading on without
asking made the line under an `if` part of the `if`.

The invariant is the lexer's, one level up: **a node's children cover its range
exactly, in order**, and a node with no children is one token. `covers` is that
written down, and every file in this repository satisfies it.

A file that does not parse is not a failure. Tokens no rule claims become an
`Error` node and the tree still covers them, which is what a file being typed
looks like.

## The repository is the oracle

Every `.cove` file here passes `cove fmt --check`, so every one of them is
already what a formatter should produce, and a correct formatter reproduces
all 246 byte for byte. That is `print(parse(source)) == source`, over half a
megabyte of real source, and it is the **weakest** of the five checks —
the one to distrust.

A formatter's own output is a fixed point of anything that leaves it alone. A
rule that decides nothing and a rule that decides correctly both reproduce an
already-correct file, so reproducing the corpus says covefmt *kept* a layout
and cannot say it would have *reached* one. Measured against source that still
needed formatting, a corpus scoring 246/246 on this check scored **42/246**.

So the corpus is damaged four ways that cannot change what a program means,
and the original is the answer. No second implementation runs:

| | what is done to the source | what it asks |
| --- | --- | --- |
| `deindented` | every line's leading whitespace stripped | does it re-indent? |
| `joinedUp` | newlines inside `(` and `[` removed | does it re-break? |
| `closedUp` | a body of exactly one statement joined onto its line | does it re-open? |
| `spacedOut` | every run of space inside a line doubled | does it re-space? |

Each is chosen so that the damage cannot be right: `deindented` leaves a line
that begins inside a string literal alone, because `cove fmt` does not lay
those out either; `joinedUp` takes `(` and `[` and not `{`, because a brace
would merge two statements; `spacedOut` doubles rather than squeezes, because
doubling can never merge two tokens.

All five are at **246 of 246**, and each has its own ratchet in `bench.cove`
that may rise and never fall.

**`benches/covefmtBench` asserts them**, which it did not at first, and the
gap was the point: `cove test` sees the samples in `parsetests.cove` — a few
hundred bytes — and the corpus is half a megabyte of source nobody wrote to be
parsed. Every mistake this parser has made was found on the corpus and would
have passed on the samples. The round-trip number was *printed* rather than
checked for a while, and in one sitting it fell from the whole corpus to 244
and back three times without anything failing.

## Over this repository

246 files, 683,514 bytes, 126,394 tokens. **Every file parses, every tree
covers its tokens, and every file round-trips.**

| | | of the pipeline |
| --- | ---: | ---: |
| lex | 99 ms | 12% |
| parse | 184 ms | 21% |
| print | 576 ms | 67% |
| **together** | **859 ms** | |
| `cove fmt --check` over the same 246 files, in Rust: lex, parse, format *and* compare | **60 ms** | |

So the whole pipeline is **14×** the Rust job, layout decisions included. Both
walks skip `target` and any directory whose name begins with a dot, so the two
numbers are over the same bytes — that was checked rather than assumed.

Three of the 246 are files the Rust formatter *refuses*: `fail_code_point`,
`fail_export_test` and `fail_reserved_annotation` under `tests/e2e`, written
not to parse. `cove fmt` skips a file it cannot parse and leaves it alone, so
for those three the corpus is not the formatter's output and reproducing them
says only that the tiling holds. It is three files out of 246 and it is
written down because the opposite mistake — reading "it skipped that" as "it
agreed with that" — is the one this corpus makes easy.

The phase shares are worth reading against the old ones. Lexing was 49% and is
12%: `Scan.at` reads bytes rather than code points, and `lower::inline`
expands it where it is called. Printing was 21% and is 67%, which is what
having layout rules costs — and it is where the remaining 14× is.

### These numbers replace worse ones, and the correction is the point

The first version of this measurement reported 1565 ms — lex 246, parse 639,
print 680 — and concluded that the tree walk dominated. It did not. The
benchmark called `tokens` and `parse` again inside the print loop, so "print"
was lex *and* parse *and* print, and "parse" was lex and parse. Two phases were
counted three times.

What found it was a profile rather than a re-reading: `print`'s walk is 3.7% of
everything the pipeline executes, which cannot be true of a phase that is 43%
of its wall clock. `parseTokens` exists because of it — a caller that holds the
tokens should not have to lex again — and the phases are now timed over what
the phase before them produced.

## Where the time actually goes

Counting every instruction the pipeline executes, by the function that ran it:

| | of all 125.9 M instructions |
| --- | ---: |
| `Scan.at` | **22.0%** |
| `tokens` | 9.7% |
| `Scan.line` | 8.8% |
| `startsWord` | 7.0% |
| `isSpace` | 6.0% |
| `utf8Width` | 4.2% |
| `isOperatorByte` | 4.0% |
| `emit` — the tree walk | 3.7% |

**Tiny leaf functions are 43% of everything executed.** `Scan.at` is eight
instructions and runs 3.46 million times; `utf8Width` is four and runs 1.32
million times. Each of those was a `call`, a frame pushed and zeroed, and a
`return`, and this profile is what asked the lowering for
`crates/cove-ir/src/lower/inline.rs` — which expands a small leaf where it is
called, and expands a larger one where a loop reaches the call. The numbers
below are from before it existed.

Two of `Scan.at`'s eight instructions are copies:

```text
   0  ge.int s4:bool s2:int s1:int
   1  branch-false s4:bool 5
   5  call-builtin s8..s9:Option String.codePointAtByte (s0:String s2:Int)
   6  switch s8:tag [10 7] else 13
   7  copy s6:Int s9:Int      <- `Some(c)` binds the payload
   8  copy s3:Int s6:Int      <- the arm's body `c` into the answer
   9  jump 14
  14  return s3:Int
```

which is the shape issue #302's stage 3 is about — a pattern binding that
aliases the subject's run rather than copying out of it — and it is 5.5% of
the whole pipeline on its own.

The native profile of the same run agrees about the shape: 34% in the dispatch
loop, 22% in `Memory::read`, `Memory::write` and `Memory::copy_words`, 18% in
`malloc`/`free`, and 5% in `open_frame`. That is what 3.5 million calls to an
eight-instruction function look like from below.

## What writing the parser found

Three bugs, all of them found by the corpus rather than by a test, and all of
them invisible to the tiling property — which is why a real corpus is worth
more than a sample.

**A character wider than one byte ended the run it was inside.**
`codePointAtByte` answers nothing at an offset that is not a character
boundary, and `Scan::at` reported that as the end of the file, so a comment
containing a `—` ended at the dash. The tiling *held* either way, because a
token that stops early is followed by another that starts there — so the
property is necessary and not sufficient. `utf8Width` computes the step from
the scalar already read rather than probing for the next boundary, which is
arithmetic instead of a second call per character.

**A `"` inside an interpolation ended the string.**
`"\"\{field.replace("\"", "\"\"")\}\""` is one literal, and a rule that stopped
at the first unescaped quote stopped in the middle of it. Braces nest, and a
literal inside one is a literal.

**A line break inside a bracket ended a declaration.** That is the language's
own rule and this did not have it, so
`export type Handler = async fn(\n  request: http.Request,\n) -> ...` became a
`type` and an error.

`async` was simply missing from the words a declaration may be preceded by.

**Reading a byte once beats asking six questions.** A body is most of a file
and every token of one is asked whether it opens or closes a bracket. Asked as
six `isPunct` calls — each an `Option<Token>` and a loop over a word — the
parse took **874 ms** against **635 ms** — both of them measured before the
double counting above was found, so read them as a ratio and not as a time.
Materialising every token as a leaf, which was the suspect, turned out to be
12%: the tree has 100,949 nodes for 95,399 tokens, and dropping the leaves to
27,517 nodes bought that much and no more.

## What writing the lexer found

**There is no module-level constant, so a table cannot be hoisted.** The first
draft matched operators against an `Array<String>` returned by a function, and
that array was rebuilt on every punctuation byte, thirty-six strings at a time,
each compared against a `sliceBytes` cut out of the source. It lexed this
repository in **1275 ms**. The same lexer with the operators written as
comparisons does it in **283 ms**. Cove has neither `const` nor a module-level
`let`, so there is nowhere to put a table that is built once, and a
zero-argument function is a call rather than a constant.

**`Option::unwrapOr` is a Cove function call.** It is `std.option.unwrapOr`, so
reading `codePointAtByte`'s answer through it pushes a frame per byte. Measured
against a `match` on the same loop it is **23% slower**, which is why `Scan::at`
answers `-1` rather than an `Option`.

**The performance figures in `cq/README.md` are obsolete.** That example
measured 1.35 µs to reach a character through a local, 1.87 µs through a
struct's field and 2.70 µs through a struct's method, and called the last of
those "the single most important thing this example learned". On this tree the
three are **0.20 µs, 0.20 µs and 0.30 µs**: the struct-field penalty is gone
entirely and the method penalty is 1.5× rather than 2×.

**A `{` in a string always opens an interpolation**, so Cove source written
inside Cove source escapes its braces as `\{` and `\}`. The tiling test is a
list of lines rather than one literal for that reason.

## What is not here yet

**An expression grammar.** The tree stops at `Stmt`, `Group` and `Member`: a
statement's own tokens are leaves. Every layout rule therefore asks its
question of a run of tokens and of the boundaries the item level gave it,
rather than of a parsed expression — `loosestIn` finds the operator a binary
breaks at by scanning for it, `breaksAtItsDots` decides a chain from where the
dots fall. `cove_syntax::format` dispatches on `ExprKind` and this does not.

That is the open risk in replacing it, and it is worth naming plainly: a
narrower instrument that reaches the same answers on 246 files might not reach
them on the 247th. What is *not* missing is the layout itself — where a line
breaks, how far it is indented, how much space goes between two tokens, when a
body of one closes up, when a chain breaks at its dots, when a call hugs its
last argument. Those are all here, and the section below is how they are
checked.

This section previously said the opposite — that no layout decision was made
at all — for long enough that a reader of it got the answer backwards. The
prose is the part that rots; the numbers below are run by `cove test`.

## Tests

`cove test` runs fifty-four of them and they need no capability at all: the
lexer takes a `String` and answers an `Array<Token>`, the parser answers a
`Tree`, and the printer answers a `String`.
