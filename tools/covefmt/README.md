# covefmt

**This is `cove fmt`.** A formatter for Cove, written in Cove: a lexer, a
parser, a printer, and a check that what it printed means what it read.
[ADR 0077](../../docs/adr/0077-cove-fmt-is-covefmt.md) made it the toolchain's
formatter; `cove_syntax::format`, the Rust formatter it replaced, stays as the
judge of it and as what `cove generate` formats with.

It reaches the `cove` binary as lowered IR. `crates/cove-cli/build.rs` checks
this package, lowers `covefmt.formatSource`, and embeds the result; `cove fmt`
reads it back and calls `formatSource` once per file that parses, with no
capability at all, on the native tier where the host has one. So **a change
here is a change to `cove fmt`**, and these are the gates on it:

- the build itself: a covefmt that does not check or lower fails
  `cargo build -p cove-cli` with its diagnostics, and one that reaches a host
  operation is refused there too;
- `cargo t`'s oracle, `covefmt_formats_every_file_in_the_repository_as_the_rust_formatter_does`
  in `crates/cove-cli/src/covefmt.rs`: on every `.cove` file in the repository,
  and on a damaged copy of each, the built-in formatter answers what the Rust
  formatter answers and refuses nothing;
- `cove fmt --check` over the repository, which is now this formatter checking
  itself and every other file;
- this package's tests and its bench, below — the bench's mutations judged
  against the files on disk, on the VM and on the native tier.

It is a package of its own: `cove.toml` beside this file, and the formatter
as the one module `covefmt` in `covefmt/`. It was a module of `examples` until
[issue 556](https://github.com/myuon/cove/issues/556) moved it here, so that
starting it checks covefmt and the standard library and nothing else. Run its
tests and its bench over the repository from this directory:

```console
$ cargo build --profile checked -p cove-cli
$ cd tools/covefmt
$ ../../target/checked/cove test
$ ../../target/checked/cove run covefmtBench --files-root ../..
```

What the switch found is worth knowing before changing the printer: the corpus
and the bench's mutations had never asked for a blank line between two
declarations, the blank lines at the end of a file or the margin at its top, a
body written against its braces (`{1}`), the space inside a group kept on one
line that the source ran together, a call broken around one argument too long
to fit anyway, a doc comment without its space, or a comment ending on space —
every file here already obeys those rules, and each mutation's answer is the
file. covefmt got every one of them wrong on its first day as `cove fmt`. The oracle's damaged copy asks for them
now, and `what_cove_fmt_writes_whatever_the_source_had` in `parsetests.cove`
pins each. One known difference is left: a call around a call around a string
literal long enough to push the line past the width is broken at the outer
call where the Rust formatter hugs it.

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
| A statement ends at the end of a line; Cove has no `;` — and a `/* */` that spans a line break ends it too | `atLineEnd` |
| An operator at the end of a line carries it on: `a +` then `b` is one expression, `a` then `+ b` is two statements. So does a keyword other than `self`, `break`, `continue` and `return`: `a is` then `b` | `continues`, which asks about the token *before* the newline |
| A line beginning with `.` continues the chain above it, and so does one beginning with `?`, `else` or `=>` | `opensWithAContinuation` |
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

### Read against the real rule

The meaning check below compares two trees this parser built, so the rule was
read against `crates/cove-syntax/src/parser.rs` — `at_statement_break`,
`ends_expression`, `at_operand`, and the places that read a token across a line
break by name — rather than against the reference's prose. Six divergences
were found and fixed, each with a case in `a_statement_ends_where_cove_syntax_ends_it`:
`else` and `=>` on the next line, `?` on the next line, a keyword at the end of
a line (`a is` then `b`), a `/* */` spanning a line break, which the real lexer
counts as one, and a bare `return` or `break` followed by a line beginning with
`.` or `?`.

What is left falls on the safe side of the check, with one exception that is
safe for another reason. Where this parser ends a statement the real one
continues — `let x` then `= 1`, `if a` then `{`, a method header broken before
its `->`, a generic list broken without a trailing comma — a formatter that
moved the newline would be *refused*, not believed: the check is stricter than
the language there. The opposite direction, a statement this parser continues
and the real one ends, is the one that could hide a change of meaning, and the
only case of it found is match arms separated by a comma at the end of a line,
which this parser reads as one statement. Joining or splitting those changes
nothing, because a comma-separated arm list means the same on one line.

## The repository is the oracle

Every `.cove` file here passes `cove fmt --check`, so every one of them is
already what a formatter should produce — and since ADR 0077, when `cove fmt`
became this formatter, `cargo t`'s oracle is what keeps that meaning "what the
Rust formatter produces" rather than "what covefmt produces" — and a correct
formatter reproduces
all 384 byte for byte. That is `print(parse(source)) == source`, over nearly
two megabytes of real source, and it is the **weakest** of the five
checks — the one to distrust.

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

All five are at **384 of 384**. Each has its own ratchet in `bench.cove` that
may rise and never fall — and all four once stood at 248 while the corpus grew
to 384, which is how one file covefmt re-broke differently from `cove fmt` got
in unnoticed: `crates/cove-sema/std/float.cove`, where a sum of two calls was
broken in front of a dot as though it were a chain
([issue #551](https://github.com/myuon/cove/issues/551)).

**`benches/covefmtBench` asserts them**, which it did not at first, and the
gap was the point: `cove test` sees the samples in `parsetests.cove` — a few
hundred bytes — and the corpus is half a megabyte of source nobody wrote to be
parsed. Every mistake this parser has made was found on the corpus and would
have passed on the samples. The round-trip number was *printed* rather than
checked for a while, and in one sitting it fell from the whole corpus to 244
and back three times without anything failing.

## Meaning is checked on every file

Every check above is agreement with `cove fmt`, and a switch to covefmt would
remove the formatter it agrees with. So covefmt also checks, on every file it
formats, the thing that has to hold whatever the layout: **its output means
what its input meant**. Cove has no `;`, so a formatter changes a program's
meaning in exactly two ways, and `formatted` in `print.cove` asks both:

1. **The significant tokens are equal, text for text** — everything but runs
   of space. Comments count: the compiler would never notice one dropped, a
   line run onto the end of a `//` comment is commented away, and a formatter
   is exactly the program that must not be trusted with either. The
   one token left out is a comma against a `)` or `]`, which both formatters
   add when they break a group and drop when they fold it.
2. **covefmt's own parser gives both the same tree**, every node's range
   compared by *significant-token index* rather than by byte offset, so that
   re-indenting moves nothing and a statement split in two, or two run into
   one, does.

When either fails, `formatted` **refuses**: the caller gets its input back
untouched, with the reason beside it, and never the output. A formatter that
leaves a file alone is a nuisance; one that changes what a program does is a
bug in every program it touches. A file that comes back byte for byte is not
checked at all, because identical text means the same thing by definition.

`benches/covefmtBench` formats through it in every pass and prints one line —
`refused by the meaning check: 0 match, 0 re-indent, 0 re-break, 0 re-open,
0 re-space` — and fails on any count above zero. The mutations are compared
*as mutated*: the damage is chosen not to change meaning, so the check must
pass on it. A refused file is handed back unchanged, so it can still score as a
match; the line beside the scores is what says it was refused.

A check that only ever sees correct output proves nothing about itself, so it
is tested on pairs it must reject — a dropped bracket, a changed operator, a
dropped comment, two statements joined, a statement split after an operand, a
newline moved out of a `(`, a reordered token — and on pairs it must accept.
And the printer was broken on purpose twice, once dropping every `?` and once
joining every line that ends on one: the bench refused 316 and 311 files a
pass and failed, naming the token and the node.

What it costs, on `whole` and the encoded VM, medians of five interleaved runs
over the same 384 files, taken 2026-10-01:

| | `whole` | |
| --- | ---: | ---: |
| before the check | 1,343 ms | |
| with it, and the #469 and statement-rule fixes it landed with | 1,357 ms | +1.0% |
| with it, and the identical-text shortcut taken out | 2,292 ms | +71% |

The corpus is already formatted, so every file takes the shortcut and the check
costs one string comparison a file. The third row is what checking a file
that *did* change costs — a lex and a parse of the output, and a walk of both
token lists and both trees — paid on every file at once: about 2.5 ms a file
on the VM, which is most of what formatting it took in the first place.

## Over this repository

384 files, 1,888,025 bytes, 327,591 tokens. **Every file parses, every tree
covers its tokens, and every file round-trips.**

Everything below is published against those, so both harnesses check them and
stop rather than average over two corpora: `scripts/covefmt-tiers.sh` and
`crates/cove-bench/src/bin/fmt_phases.rs` assert the file count exactly and the
byte count to within 2%. The previous figures — 246 files, 683,514 bytes,
126,394 tokens — were two files and 14,017 bytes out of date by the time anyone
compared them, which is what the check is for.

The **file count** is the equality and the **byte count** is the band, and the
asymmetry is the interesting part: the corpus *is* this repository, so editing a
doc comment anywhere in the tree moves the byte count. Correcting the stale
figures in `bench.cove`, `print.cove` and `parsetests.cove` moved it by 950
bytes, in the very commit that recorded the new one. A gate that fired on every
prose change is a gate nobody would keep.

| | | of the pipeline |
| --- | ---: | ---: |
| lex | 155 ms | 11% |
| parse | 337 ms | 24% |
| print | 891 ms | 64% |
| **together** | **1,384 ms** | |

Medians of fifteen interleaved runs on the encoded VM, taken 2026-10-01; the
native tier's are in its own section below. The previous table — 604 ms over
248 files and 698,481 bytes — is not comparable and is not a regression: the
corpus is 2.7 times the bytes it was. Every number in this file and in the two
sections under it comes from these five commands and nothing else:

```console
$ cargo build --profile checked -p cove-bench
$ cargo build --profile checked -p cove-cli
$ scripts/covefmt-tiers.sh 15             # the three arms, interleaved and checked
$ scripts/covefmt-profile.sh 5            # where the machine time goes, per tier
$ ./target/checked/cove-fmt-phases . 51   # the Rust arm's phases, apart
```

The numbers above were taken on a `--features template` build, which this
section used to insist went **last**: cargo replaces `target/checked/cove` with
whichever feature set was last asked for, and an ordinary `cargo t` in between
put the featureless default back and the native arm became unavailable. Since
[ADR 0076](../../docs/adr/0076-the-native-tier-is-built-by-default.md) the
default build of `cove-cli` *is* that build — the same features, so the same
binary — and the order no longer matters. What still replaces it is a
`--no-default-features` build, and a host the code generator cannot serve has
no native arm at all, so both scripts still ask before they time anything and
say which command to run, because the alternative is a measurement that dies
fourteen rounds in.

`covefmt-tiers.sh` reports min and max beside every median, separates the cold
run from the warm ones, diffs the two Cove arms byte for byte on every round,
and stops rather than print a table over a run that formatted something else.

Everything below is that machine and that build, because a wall-clock number is
neither without them: **Intel Core i7-10700K at 3.80 GHz** (8 cores, 16
threads), macOS 26.6.2 (`x86_64-apple-darwin`), rustc 1.98.1,
`--profile checked`, load average under 2, one heavy command at a time, clean
tree. The 2026-10-01 figures were taken at a load average of about 2.5, and
their spreads are as tight as the earlier ones. ADR 0029 is why none of it is gated anywhere.

### The Rust reference, and the process that was five things

`cove fmt --check` at the repository root is the same job in Rust, and the
**143 ms** its process takes is the figure this file used to divide by. It
should not be, and the correction matters more than the number:

| | |
| --- | ---: |
| process startup and argument parsing (`cove` with no work to do) | 4.7 ms |
| `fmt_targets`' walk of the repository | 15.1 ms |
| reading 1,888,443 bytes | 11.5 ms |
| **lex, parse, format and compare** | **103.5 ms** [100.7..114.6] |
| — of which lex | 14.6 ms |
| — of which parse, and the numbering | 44.6 ms |
| — of which format and compare | 44.1 ms |
| the `SourceMap`'s second copy of every file, and three diagnostics rendered | the remainder, ~8 ms |
| **the process** | **143 ms** |

The three phases are differences between two whole passes — the same design
`bench.cove` uses and for the same reason — so each carries both passes' noise
and the split moves by a millisecond or two between sessions where `whole` does
not. Read `whole` as the measurement and the split as its shape.

The Cove bench reads all 384 files before its clock starts and is not a fresh
process, so **1,384 ms against 103.5 ms is the like-for-like comparison and it
is 13.4×** on the encoded VM, and **477 ms against 103.5 ms is 4.6×** on the
native tier. It was 15.1× on the 248-file corpus, when the native tier was the
VM's speed. Against the 143 ms process they read as 9.7× and 3.3×, and that is the Rust arm
being charged for a directory walk, a file read and an execve that the Cove arm
does not pay. The medians are fifty-one iterations;
`crates/cove-bench/src/bin/fmt_phases.rs` is where the four rows come from, and
it exists because `cove fmt --check` reports one number over all of them and the
column could not be filled from it at all.

Both walks skip `target` and any directory whose name begins with a dot, so the
two are over the same bytes — asserted rather than assumed, by the corpus pair
both harnesses check.

One asymmetry runs the other way and is worth naming beside the ratio: this
parser has **no expression grammar** (see "What is not here yet"), so a
statement's own tokens are leaves where `cove_syntax` builds an `ExprKind`
tree. covefmt is doing *less* work per file in its parse phase than the Rust
arm and taking 7.6× as long over it on the VM, 3.3× on the native tier.

Three of the 384 are files the Rust formatter *refuses*: `fail_code_point`,
`fail_export_test` and `fail_reserved_annotation` under `tests/e2e`, written
not to parse. `cove fmt` skips a file it cannot parse and leaves it alone, so
for those three the corpus is not the formatter's output and reproducing them
says only that the tiling holds. It is three files out of 384 and it is
written down because the opposite mistake — reading "it skipped that" as "it
agreed with that" — is the one this corpus makes easy.

The phase shares are worth reading against the old ones. Lexing was 49% and is
11%: `Scan.at` reads bytes rather than code points, and `lower::inline`
expands it where it is called. Printing was 21% and is 64%, which is what
having layout rules costs — and it is where most of the remaining 13.4× is.

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
   5  intrinsic-call s8..s9:Option String.codePointAtByte (s0:String s2:Int)
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

That profile has been retaken properly since, over the whole run rather than a
slice of it, and it is the section below.

## The native tier over the same corpus

[ADR 0055](../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md)'s
experimental native tier runs this program too:

```console
$ cargo build --profile checked -p cove-cli
$ cd tools/covefmt && ../../target/checked/cove run covefmtBench --files-root ../.. --backend native
```

It compiles every supported reachable function of one lowered program eagerly,
finalizes once, and the encoded `CALL` arm consults the same table generated
code does — so a compiled function called from an encoded one is entered. The
default *backend* does not change, and a build without the feature
(`--no-default-features`, or any embedder of `cove-runtime`) has no
executable-memory dependency.

**On this corpus it is 2.9 times the encoded VM's speed**, and it was not
always: on 2026-09-16, over 248 files, it compiled 29.6% of the reachable
functions, ran 5.3% of the instruction stream, and was the VM's speed to
within the noise floor. What changed is mostly the subset: the refusals the
section below ranks were lowered one family at a time, `String ==` (#508)
among the last of them, until nothing the formatter calls is refused.

| | 2026-10-01, 384 files | 2026-09-16, 248 files |
| --- | ---: | ---: |
| reachable functions | 110 | 179 |
| compiled | 108 (**98.2%**) | 53 (29.6%) |
| refused, and run encoded | 2 — `covefmt.main` and `covefmt.walk` | 126 |
| machine code emitted | 827,668 bytes | 56,632 bytes |
| compilation | **2.2 ms**, once | 0.4 ms |
| VM → VM calls | 501 | 6,144,478 |
| VM → native calls | 10,003 | 1,680,390 |
| native → VM calls | 0 | 299,857 |
| native → native, direct | 22,261,768 | 412,242 |
| Cove calls that used native code | **100.0%** | 24.5% |
| instructions the encoded tier dispatched | 196,317 | — |
| `whole`, vm | 1,384 ms | 604 ms |
| `whole`, native | **477 ms** | 607 ms |

The two refused functions are the bench's own driver — the walk of the
directory and `main` — and both are refused for `CallHost`, which is the file
system. Neither is on a path the formatter runs, so the formatter itself is
entirely compiled, and the 196,317 instructions the encoded tier still
dispatches are a rounding error on the VM arm's 1,755,436,658.

The paired reading is the one to trust, and this time it is not a coin toss.
Within each of fifteen interleaved rounds native is faster on every phase in
every round: **−906 ms** on `whole` [−960..−883], −104 ms on `lex`, −189 ms on
`parse` and −614 ms on `print`. Byte-identical output on every round, and the
five oracle checks all at 384 of 384 on both.

The phases do not move together. `lex` and `print` are 3.1× and 3.2× faster
compiled and `parse` is 2.3×, so the native tier's split is lex 10%, parse 31%,
print 58% — the parser is where the compiled run spends a larger share than
the encoded one did.

The memory figures are **identical** between the arms to the last word:
8,477,680 allocations, 107,471,857 words handed out, 27 collections, on both.
Nothing about which tier ran a function changes what it allocates, which is the
expected reading and is worth having as a check rather than an assumption —
`cove run --stats` prints all four, on either tier, without a profiler, because
ADR 0055 refuses `--profile` beside `--backend native`.

One column of issue #369's table is missing and cannot be filled: **copied
words**. Nothing counts them. `Memory::copy_words` is one of the five hottest
functions in both arms, and an increment in it would be instrumentation on the
path being measured — the count would be over a run that was not the run. What
can be said instead is in the profile below, where the copies are 12.5% of the
encoded run and 3.8% of the compiled one, and in ADR 0057's per-call census:
2.99 parameter words copied per call, into frames 14.5 words wide.

### Where a fully compiled run's time goes

`scripts/covefmt-profile.sh`, five sampled runs an arm, three sessions, taken
2026-10-01 after #551. `/usr/bin/sample` at 1 ms, self time, the whole bench
process — the pipeline *and* the four oracle passes, which is why the times are
the 11 s and 4 s processes and not the `whole` column. A share's bracket is the
three sessions; the milliseconds are samples per run, so read them as
approximate. The load average was 2.8 to 4 throughout, above the "under 2"
this file asks for; the sessions agreeing to half a point is the evidence it
did not move the shares.

| component | vm | ≈ ms a run | native | ≈ ms a run |
| --- | ---: | ---: | ---: | ---: |
| encoded dispatch | **66.2%** [66.0..66.2] | 5,990 | 1.9% [1.6..2.1] | 63 |
| **generated native code** | 0.0% | 0 | **52.2%** [52.2..52.4] | 1,732 |
| open/close/call helpers | 2.6% [2.5..2.6] | 232 | **12.9%** [12.9..13.1] | 428 |
| safepoints | 0.5% [0.4..0.5] | 42 | **8.9%** [8.8..9.0] | 297 |
| allocation and collection | 3.1% [3.0..3.2] | 282 | 7.8% [7.7..8.1] | 259 |
| — of which the host allocator | 1.7% [1.7..1.8] | 157 | 4.6% [4.5..4.8] | 152 |
| linear-memory slot access | 10.2% [9.8..10.2] | 921 | 7.1% [6.9..7.5] | 233 |
| slot copies: arguments, returns and `copy` | 12.5% [12.5..12.7] | 1,149 | 3.8% [3.6..3.8] | 126 |
| frame growth and zeroing | 4.9% [4.9..5.0] | 449 | 3.8% [3.8..4.0] | 125 |
| tier lookup and transition | 0.0% | 0 | 0.9% [0.7..0.9] | 30 |
| unattributed | 0.2% | 16 | 0.5% | 18 |
| **the run** | | **≈ 9,070** | | **≈ 3,310** |

Three sessions agree to within half a point on every row, so every difference
below is larger than the noise.

**The generated code is half of the compiled run, and the other half is the
runtime it calls.** On 2026-09-16 generated code was 1.1% of the run and the
question was how to get more of the program compiled. That question is
answered — 108 of 110 functions, and the two that are not are the bench's own
directory walk and `main` — and the encoded dispatch left over is 63 ms, a
third of it `__getattrlist`, the file system those two functions call. What
the compiled run pays beside its own code is:

- **the call path, 14%**: `native::open`, `close` and `republish`, and the
  tier lookup, over 22.3 million calls that are now all native → native. It
  was 2.6% of the encoded run, where `open_frame` is the only part of it the
  sampler can see outside the dispatcher.
- **safepoints, 9%**: `Machine::safepoint` alone is 7.2%. The encoded figure
  beside it is a lower bound, because the dispatcher inlines most of its
  safepoint (see the caveats below), so the two columns do not say the native
  safepoint is seven times dearer — they say it is now the second largest
  thing in the run.
- **allocation, 8%**, which is the same 260 to 280 ms on both arms. It should
  be: the two arms make the same 8,481,918 allocations, and the compiled code
  calls the same allocator.
- **slot access, 7%**: `Memory::read` and the layout lookups that compiled code
  still makes through the runtime for heap objects.

So the lever on this workload has moved. It was the refusals, and they are
gone; it is now the native call path and the safepoint, which together are
roughly a quarter of the compiled run, and both are the runtime's protocol
rather than the quality of the code the template compiler emits.

Three caveats belong with the table rather than under it.

**The sampler costs about 8% to 9%** of wall time — `whole` is 1,506 ms under
it against 1,381 without on the VM, and 509 against 471 on the native tier —
so the shares are comparable and the times are not.

**An inlined callee is charged to its inliner.** `--profile checked` carries no
debug info, so `Machine::safepoint` and `Memory::push_frame` are partly inside
`encoded::dispatch`: on the encoded arm the safepoint and frame-zeroing rows
are **lower bounds**, and the per-call ablation tables of
[ADR 0057](../../docs/adr/0057-a-native-call-returns-into-the-destination-its-caller-named.md)
are the instrument for those. The native arm reaches the same functions from
generated code; how much of them that code does inline is not visible from
here, so read its rows as self time in the named function and no more.

**The unwinder cannot walk out of generated code**, so a sample inside a JIT
page has no caller and the native column is self time only. An inclusive
attribution — which compiled function the 297 ms of safepoints were called
from — is not available from this instrument at all.

### What the refusals cost, when there were refusals

On 2026-09-16 126 of 179 reachable functions were refused, and ranked by the
dynamic calls each kept in the VM, two families were the whole of it: a frame
slot holding a `Repr::Addr` (a `var` parameter, 32 functions, 39.9% of calls)
and `Inst::Clear` (48 functions, 31.7%). Lowering both was read as a ceiling of
96.1% of calls, with the warning that a first-blocker count cannot forecast —
a function refused for one thing may hold a second. Those families, and the
`Str`, `LoadField`, `Neg(Int)`, `CallBuiltin` and `AllocImm` rows below them,
were lowered or left the program since; the measured result is the 100.0% in the table above. The
census itself is in the history of this file.

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

That is the open risk in replacing it, and it is worth naming plainly — but it
is now a risk to the *layout* and not to the program. A narrower instrument that
reaches the same answers as `cove fmt` on 384 files might break a line
differently on the 385th; it cannot hand back something that means something
else without being refused, because the check in "Meaning is checked on every
file" runs on every file it formats. What is *not* missing is the layout itself
— where a line breaks, how far it is indented, how much space goes between two
tokens, when a body of one closes up, when a chain breaks at its dots, when a
call hugs its last argument. Those are all here, and the section below is how
they are checked.

What the check rests on is covefmt's own statement rule, and "Read against the
real rule" says what is left of the difference between it and
`crates/cove-syntax`'s.

This section previously said the opposite — that no layout decision was made
at all — for long enough that a reader of it got the answer backwards. The
prose is the part that rots; the numbers below are run by `cove test`.

## Tests

`cove test` runs seventy-four of them and they need no capability at all: the
lexer takes a `String` and answers an `Array<Token>`, the parser answers a
`Tree`, and the printer answers a `String`.
