# covefmt

The lexer of a Cove formatter, written in Cove.

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

## What it costs

Over this repository — 238 files, 468,759 bytes, 90,502 tokens:

| | |
| --- | ---: |
| lex, in Cove | **283 ms** |
| per byte | 0.60 µs |
| the same scaled to all 695 KB of Cove here | ~420 ms |
| `cove fmt --check` over that 695 KB, in Rust: lex, parse, format *and* compare | **40–70 ms** |

So the lexer alone is six to ten times the whole Rust job, and the parser and
the printer are not written. The target is 5×; reaching it is a performance
project rather than a consequence, and this program is the workload it should
be argued from.

Two floors, measured on this tree:

- **0.15 µs per byte inspection** — `codePointAtByte` and a `match` and a
  comparison and a loop step. At the VM's 6.6 ns per instruction, that is about
  23 IR instructions per byte looked at.
- **a lexer looks at each byte two to three times**, which is where 0.60 µs
  comes from. Almost none of it is the program.

## What writing it found

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

The parser and the printer. This is the front end and the measurement it
establishes; nothing here formats anything.

## Tests

`cove test` runs eleven of them and they need no capability at all: the lexer
takes a `String` and answers an `Array<Token>`.
