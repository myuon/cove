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
all 248 byte for byte. That is `print(parse(source)) == source`, over two
thirds of a megabyte of real source, and it is the **weakest** of the five
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

All five are at **248 of 248**, and each has its own ratchet in `bench.cove`
that may rise and never fall.

**`benches/covefmtBench` asserts them**, which it did not at first, and the
gap was the point: `cove test` sees the samples in `parsetests.cove` — a few
hundred bytes — and the corpus is half a megabyte of source nobody wrote to be
parsed. Every mistake this parser has made was found on the corpus and would
have passed on the samples. The round-trip number was *printed* rather than
checked for a while, and in one sitting it fell from the whole corpus to 244
and back three times without anything failing.

## Over this repository

248 files, 698,481 bytes, 127,720 tokens. **Every file parses, every tree
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
| lex | 61 ms | 10% |
| parse | 150 ms | 25% |
| print | 393 ms | 65% |
| **together** | **604 ms** | |

Medians of fifteen interleaved runs. Every number in this file and in the two
sections under it comes from these five commands and nothing else:

```console
$ cargo build --profile checked -p cove-bench
$ cargo build --profile checked -p cove-cli --features template
$ scripts/covefmt-tiers.sh 15             # the three arms, interleaved and checked
$ scripts/covefmt-profile.sh 5            # where the machine time goes, per tier
$ ./target/checked/cove-fmt-phases . 51   # the Rust arm's phases, apart
```

The feature build goes **last**, and that is not a style preference: cargo
replaces `target/checked/cove` with whichever feature set was last asked for, so
an ordinary `cargo t` or `cargo clippy --workspace` in between puts the default
build back and the native arm becomes unavailable. Both scripts ask before they
time anything and say which command to run, because the alternative is a
measurement that dies fourteen rounds in. CI builds it last for the same reason.

`covefmt-tiers.sh` reports min and max beside every median, separates the cold
run from the warm ones, diffs the two Cove arms byte for byte on every round,
and stops rather than print a table over a run that formatted something else.

Everything below is that machine and that build, because a wall-clock number is
neither without them: **Intel Core i7-10700K at 3.80 GHz** (8 cores, 16
threads), macOS 26.6.2 (`x86_64-apple-darwin`), rustc 1.98.1,
`--profile checked`, load average under 2, one heavy command at a time, clean
tree. ADR 0029 is why none of it is gated anywhere.

### The Rust reference, and the 60 ms that was four things

`cove fmt --check` at the repository root is the same job in Rust, and the
**68 ms** its process takes is the figure this file used to divide by. It
should not be, and the correction matters more than the number:

| | |
| --- | ---: |
| process startup and argument parsing (`cove` with no work to do) | 4.6 ms |
| `fmt_targets`' walk of the repository | 7–10 ms |
| reading 698,481 bytes | 4–7 ms |
| **lex, parse, format and compare** | **40.1 ms** [38.8..43.6] |
| — of which lex | 5.6 ms |
| — of which parse, and the numbering | 15.8 ms |
| — of which format and compare | 18.4 ms |
| the `SourceMap`'s second copy of every file, and three diagnostics rendered | the remainder, ~5 ms |
| **the process** | **68 ms** |

The three phases are differences between two whole passes — the same design
`bench.cove` uses and for the same reason — so each carries both passes' noise
and the split moves by a millisecond or two between sessions where `whole` does
not. Read `whole` as the measurement and the split as its shape.

The Cove bench reads all 248 files before its clock starts and is not a fresh
process, so **604 ms against 40.1 ms is the like-for-like comparison and it is
15.1×**. Against the 68 ms process it reads as 8.9×, and that is the Rust arm
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
arm and taking 9.5× as long over it.

Three of the 248 are files the Rust formatter *refuses*: `fail_code_point`,
`fail_export_test` and `fail_reserved_annotation` under `tests/e2e`, written
not to parse. `cove fmt` skips a file it cannot parse and leaves it alone, so
for those three the corpus is not the formatter's output and reproducing them
says only that the tiling holds. It is three files out of 248 and it is
written down because the opposite mistake — reading "it skipped that" as "it
agreed with that" — is the one this corpus makes easy.

The phase shares are worth reading against the old ones. Lexing was 49% and is
10%: `Scan.at` reads bytes rather than code points, and `lower::inline`
expands it where it is called. Printing was 21% and is 65%, which is what
having layout rules costs — and it is where the remaining 15.1× is.

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
$ cargo build --profile checked -p cove-cli --features template
$ cd examples && ../target/checked/cove run covefmtBench --files-root .. --backend native
```

It compiles every supported reachable function of one lowered program eagerly,
finalizes once, and the encoded `CALL` arm consults the same table generated
code does — so a compiled function called from an encoded one is entered. The
default does not change and a build without the feature has no
executable-memory dependency.

**On this corpus it is the encoded VM's speed, to within the noise floor.**

| | |
| --- | ---: |
| reachable functions | 179 |
| compiled | 53 (**29.6%**) |
| refused, and run encoded | 126 |
| machine code emitted | 56,632 bytes |
| compilation | **0.4 ms**, once |
| VM → VM calls | 6,144,478 |
| VM → native calls | 1,680,390 |
| native → VM calls | 299,857 |
| native → native, direct | 412,242 |
| Cove calls that used native code | **24.5%** |
| IR instructions inside compiled code | 33.5 M of 632.3 M, **5.3%** |
| `whole`, native against vm, paired within each of fifteen interleaved rounds | **+3 ms of 604**, 10 rounds of 15 slower |

The last row is the result and the shape of it is why it is trustworthy. The two
arms run seconds apart in the same round, so the difference *within* a round is
a far tighter measurement than the difference between two medians — and it is a
coin toss, ten rounds one way and five the other on a median difference of half
a percent. Byte-identical output on every round, all five oracle checks at 248
of 248 on both.

The one phase that is **not** a coin toss is `lex`: native is 2 ms faster there
and it is slower in one round of fifteen. That is where the compiled
functions are — `Scan.at` and the byte predicates around it, which are exactly
the small scalar leaves the template compiler can take — and 3% off a phase that
is 10% of the pipeline is 0.3% of the pipeline. Nothing else moves.

The memory figures are **identical** between the arms to the last word:
4,001,253 allocations, 43,018,808 words handed out, ten collections, on both.
Nothing about which tier ran a function changes what it allocates, which is the
expected reading and is worth having as a check rather than an assumption —
`cove run --stats` prints all four, on either tier, without a profiler, because
ADR 0055 refuses `--profile` beside `--backend native`.

One column of issue #369's table is missing and cannot be filled: **copied
words**. Nothing counts them. `Memory::copy_words` and `Memory::copy_slots` are
two of the five hottest functions in both arms, and an increment in either would
be instrumentation on the path being measured — the count would be over a run
that was not the run. What can be said instead is in the profile below, where
the copies are 10% of both arms, and in ADR 0057's per-call census: 2.99
parameter words copied per call, into frames 14.5 words wide.

### Why 24.5% of the calls is 5.3% of the work

The template compiler takes the small scalar leaves. A compiled call runs **16
IR instructions** on average where the run's mean is 74, so a quarter of the
calls is a twentieth of the instruction stream — and `scripts/covefmt-profile.sh`
says what that is worth. Five sampled runs an arm, `/usr/bin/sample`, self time,
and the reading is the **difference** between the columns:

| component | vm | native | difference | over three sessions |
| --- | ---: | ---: | ---: | ---: |
| encoded dispatch | 53.5% | 50.4% | **−3.1** | −2.2 to −3.2 |
| linear-memory slot access (`Memory::read`/`write`) | 11.7% | 11.4% | −0.3 | −0.2 to −0.4 |
| runtime builtins | 11.4% | 11.7% | +0.2 | −0.1 to +0.2 |
| slot copies: arguments, returns and `copy` | 10.5% | 10.0% | −0.5 | −0.1 to −0.9 |
| allocation and collection | 5.5% | 6.1% | **+0.7** | +0.4 to +0.7 |
| — of which the host allocator | 4.1% | 4.7% | **+0.6** | +0.6 to +0.8 |
| frame growth and zeroing | 4.8% | 4.8% | −0.1 | −0.3 to +0.2 |
| open/close/call helpers | 2.2% | 2.9% | **+0.7** | +0.3 to +1.0 |
| **generated native code** | 0.0% | **1.1%** | **+1.1** | +1.0 to +1.1 |
| tier lookup and transition | 0.0% | **1.1%** | **+1.1** | +1.0 to +1.1 |
| safepoints | 0.4% | 0.5% | +0.0 | +0.0 to +0.2 |

The last column is three independent five-run sessions and it is the honest
noise floor: between runs a bucket's share moves by about ±0.3 percentage points
and the 50% bucket by ±1.0, several times the square-root-of-the-count floor.
Five components survive it — the dispatcher, allocation, the helpers, generated
code and the tier lookup — and so does the sub-row under allocation. The other
five do not, and are in the table because a component issue #369 names and whose
answer is "nothing measurable" is an answer.

Read as a budget it balances, and that is the finding. The native tier **takes
3.9 points off what the encoded tier was doing and puts 3.9 back**:

| gone | | added | |
| --- | ---: | --- | ---: |
| encoded dispatch | −3.1 | generated native code | +1.1 |
| slot copies | −0.5 | the tier lookup, at 8.5 M calls | +1.1 |
| linear-memory slot access | −0.3 | the open/close/call helpers | +0.7 |
| frame growth and zeroing | −0.1 | allocation and collection | +0.7 |
| | | runtime builtins, the safepoint, the rest | +0.3 |
| **together** | **−3.9** | **together** | **+3.9** |

The sum of the two columns is **+0.01 percentage points**, which is what the
wall clock said from the other direction, and two instruments agreeing on zero
by different routes is most of the reason to believe either.

The added column is worth reading term by term, because three of its four large
entries are costs a code generator cannot remove by generating better code. The
tier lookup is a table read the `CALL` path now performs on **every** call,
8,536,967 of them, whether or not the callee is compiled. The allocation is the
encoded floor's owned vector, which the 299,857 native → VM calls still pay —
[ADR 0057](../../docs/adr/0057-a-native-call-returns-into-the-destination-its-caller-named.md)
priced it at about 60 ns a call, a macOS `malloc`/`free` round trip, and 299,857
of those is 18 ms of a 4.7 s run. Only the 1.1 in generated code is work that
*replaced* something, and it replaced 3.1.

**The generated code is about three times the dispatcher, and that is the number
to carry forward.** 1.1% of 4.73 s is 52 ms for the 33.5 M IR instructions inside
compiled functions, which is 1.6 ns each; the encoded tier's dispatch and slot
access together are 61.8% of the run for 598.8 M, which is 4.9 ns each. The
second figure is approximate — it charges the dispatcher with all of its slot
access and none of its builtins — so read it as "about 3×" and not as 3.1. It
sits where ADR 0056's 2.44× on call-free code and ADR 0057's 1.97× on
call-shaped code said it would, which is the cross-check that makes it worth
quoting at all. **The tier is not slow. It covers 5.3% of the work.**

Three caveats belong with the table rather than under it.

**The sampler costs about 8%** of wall time — 654 ms against 604 for the bench's
own `whole` — and it costs the same on both arms (657 against 654), so the
shares are comparable and the times are not.

**An inlined callee is charged to its inliner.** `--profile checked` carries no
debug info, so `Machine::safepoint` and `Memory::push_frame` are partly inside
`encoded::dispatch`: the safepoint and frame-zeroing rows are **lower bounds**,
and the per-call ablation tables of
[ADR 0057](../../docs/adr/0057-a-native-call-returns-into-the-destination-its-caller-named.md)
are the instrument for those — they put the safepoint at 9.1% of the native
*call path*, which this table cannot see and does not contradict.

**Doubling and sampling order the terms; neither prices them.** The components
are not strictly additive — a second instance of one runs with the first one's
cache lines warm, and removing one changes the branch history of the next — so
every figure here ranks a component against another and none of them is a
subtraction that would be recovered as wall time.

### What the refusals cost, counted

126 functions are refused, and the report ranks them by the dynamic calls each
kept in the VM rather than by count — a hundred refused functions nothing calls
cost a run nothing. Grouped by the first instruction or slot the lowering could
not take:

| first blocker | functions | dynamic calls | of all 8,536,967 Cove calls |
| --- | ---: | ---: | ---: |
| a frame slot holds a `Repr::Addr` — a `var` parameter | 32 | 3,406,192 | **39.9%** |
| `Inst::Clear` | 48 | 2,703,465 | **31.7%** |
| `Inst::LoadField` | 5 | 125,106 | 1.5% |
| `Inst::Str` | 5 | 103,859 | 1.2% |
| `Inst::Neg(Int)` | 12 | 78,059 | 0.9% |
| `Inst::CallBuiltin` | 10 | 25,656 | 0.3% |
| `Inst::AllocImm` | 13 | 1,998 | 0.02% |
| `Inst::AllocBuffer` | 1 | 0 | 0.0% |
| **together** | **126** | **6,444,335** | **75.5%** |

The accounting is exact rather than approximate: 6,444,335 is VM → VM plus
native → VM, and 2,092,632 — the other 24.5% — is VM → native plus
native → native. The two sum to the 8,536,967 calls the run made, which is what
says the ranking is over every call and not over a sample of them.

The five refusals that block the most work are `covefmt.Parser.leaf` (1,021,045
calls, an `Addr` slot), `covefmt.emit` (664,214, an `Addr` slot),
`covefmt.holdsABody` (527,087, `Clear`), `covefmt.Parser.trivia` (405,911, an
`Addr` slot) and `covefmt.wantsASpaceBetween` (394,740, `Clear`).

**Two families are the whole of it.** `Repr::Addr` frame slots and `Inst::Clear`
are 80 of the 126 refusals and 71.6% of every call the run makes; lowering both
would take the native share of calls from 24.5% to **96.1%**. That is an upper
bound and the reason is worth knowing: a row names a function's *first* blocker
only, so a function refused for an `Addr` slot that also holds a `Str` does not
compile when `Addr` slots are lowered. It is a ceiling read off a count, not a
forecast.

It is also the only lever on this workload with a ceiling worth the name, and
the profile above is why. Every other candidate — inlining at the native target,
initialising only the reference words of a frame, a register ABI for arguments
and returns — improves a path that is **1% to 3% of this run**, because the tier
covers 5.3% of the instruction stream. Making that path twice as fast is worth
a percent. The refusals are what set the 5.3%, and they are the only thing that
can move it.

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
narrower instrument that reaches the same answers on 248 files might not reach
them on the 249th. What is *not* missing is the layout itself — where a line
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
