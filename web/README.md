# The Cove playground

A page that compiles and runs Cove in the browser. There is no server: the
parser, the type checker, the lowering and the linear-memory VM are one
WebAssembly module, built from `crates/cove-wasm`, and nothing typed into it
leaves the tab.

## Build it

```
cargo build -p cove-wasm --target wasm32-unknown-unknown --release
```

That writes `target/wasm32-unknown-unknown/release/cove_wasm.wasm`, 3.08 MB.
There is no bundler, no `npm`, and no other step: `index.html`, `cove.mjs` and
`worker.mjs` are loaded as they are.

Optimizing for size gets it to 2.71 MB, a 12% saving, measured:

```
RUSTFLAGS="-C opt-level=z" cargo build -p cove-wasm --target wasm32-unknown-unknown --release
```

That is a flag and not a `[profile.release]` in the workspace manifest, which
says there why it has none: release is the reference for what a user's build
does, at Cargo's defaults, and it is what CI builds. Twelve percent of one
download is not worth making that sentence untrue. `wasm-opt` would take more
off again and is not run here, because it is a tool this repository does not
otherwise depend on and the build is meant to be one `cargo build`.

If the target is not installed yet:

```
rustup target add wasm32-unknown-unknown
```

## Open it

Serve the **repository root** and open `/web/`:

```
python3 -m http.server 8000
# then http://localhost:8000/web/
```

The root and not `web/`, because the page loads the module from
`../target/wasm32-unknown-unknown/release/cove_wasm.wasm`, which is outside
this directory. Copying `cove_wasm.wasm` into `web/` works too — `cove.mjs`
looks there first — and then `web/` alone can be served.

A static server and not `file://`. ES modules, module workers and `fetch` are
all refused over `file://` by every browser's origin rules; that is the
browser's decision and there is nothing this page can do about it. Any static
server will do.

## The published copy

`.github/workflows/pages.yml` builds this page — with
`RUSTFLAGS="-C opt-level=z"`, for the 12% off `cove_wasm.wasm` described above
— on every push to `main`, stages everything in `web/` except this file and
`check.mjs` into one directory beside the built `.wasm`, and runs
`node web/check.mjs` against that build before anything is deployed. A run that does not hold — a program
that no longer compiles, a `spawn` that stops saying why, a fuel or deadline
bound that stops firing — fails the job, and nothing is published.

It shares the repository's one GitHub Pages site with the API documentation
(`cargo doc`, from the same workflow), rather than a site of its own, because
a repository has exactly one such site when the source is "GitHub Actions"
and a second workflow deploying to it would only race this one. It is reached
at `https://myuon.github.io/cove/playground/`, and linked from the
documentation's own landing page.

Every path the page asks for — the worker, `cove.mjs`, the `.wasm` — is
resolved relative to the file that asks for it, never to the site root, so
nothing here needed to change to be served from that subdirectory rather than
from `/`.

## Check it without a browser

```
node web/check.mjs
```

This is what CI runs. It loads the module through `cove.mjs` — the same module
the page's worker loads through, so the loader under test is the loader the
browser uses — and then compiles and runs Cove programs through it: **every
sample in `samples/`**, and then a program that does not parse, one that spawns
a task, one that loops past its fuel, one that loops past its deadline, and one
that calls a capability the playground does not grant. It exits non-zero on the
first that does not hold.

It also lexes every sample, because a sample a visitor opens and sees in one
colour is a silent failure — nothing about it looks broken. What is asserted
of a colouring is that its spans tile the source and that a known program's
pieces are the categories the lexer called them, kinds and not counts: a
tiling of the right length made of the wrong categories is exactly what a
highlighter that had drifted from the language would produce.

And it colours **every sample's real disassembly**, which is the other pane
the module colours and the one with no lexer to borrow. What holds there is
that every line was a line shape `crates/cove-ir/src/print.rs` documents — the
module answers `ok: false` for a line it does not recognise, and a false
answer for any shipped sample fails the build. That is what keeps a change to
the printer from quietly turning the pane into a wall of one colour. Beside it
the categories are named on a small program, and the six the nine samples
between them use are asserted as a set, for the reason kinds are asserted of
source.

Then it records one. A known program is run under the recording debugger and
the recording is checked moment by moment: the stops it should have, in the
order it ran them, on the lines they were written at, with the locals holding
what they held *at each one*; the disassembly interned once per function; a
local that names a heap object and the object it names; and a recording that
hit its bound, said so, and left the run it was recording to reach its own
end and answer.

That last group is worth running on the wasm build and not only under
`cargo test`, and this is not hypothetical. The first version of the
disassembly capture asked for a range of `u32::MAX` instructions around the
pc. On a 64-bit host that is the whole function. On `wasm32` a `usize` is
32 bits, the addition wrapped, and every function came back with a *different*
empty range at every pc — so the interning made a fresh table entry each time
and the Instructions pane would have shown one instruction. `cargo test`
passed. This did not.

## What the page can and cannot do

**The same as `cove run`:** the parser, the checker, the lowering, the VM, and
the diagnostics — rendered by `cove_diag::render`, so the caret and the
snippet are the CLI's. The host schemas are the shipped set, so a program that
uses `http` type-checks here exactly as it does on the command line.

**No filesystem and no network.** `files` and `documents` are in-memory and
start empty; `process` is recorded; `http` and `database` are denied. A
program that calls one of those is told it was not granted the capability, in
the runtime's own words.

**No tasks.** A `spawn` is refused, with a span, saying this environment has no
threads to give a task. A Cove task is a thread (ADR 0008) and one Web Worker
is one thread. The alternative — running a task's body inline — would make the
browser answer differently from the tree-walking oracle, and the whole corpus
is held together by those two agreeing. `examples/tasks` does not run here.

**A virtual clock.** `clock.now()` starts at zero and `clock.sleep` finishes at
once by moving the clock, which is `Clock::virtual_clock`, the same host the
differential harness uses. The *run's* deadline is a different thing and is
real: it is enforced against `performance.now()`, which the page supplies to
the module as an import.

## Recording a run

**Run & record** does the same run watched by a debugger that writes down what
it saw, and then the page scrubs through the timeline. The source it scrubs
through is the **editor itself**: the slider marks the line the moment was
written at and, inside it, the instruction's own span, and scrolls the editor
when the mark would otherwise be off screen. Three panes go with it —
Instructions, Runtime and Memory — all moved by that one slider. Selecting a
frame in Runtime moves the other two *and the mark in the editor* to that
frame's call site, which is what makes an outer frame readable.

There was a fourth pane, listing the recorded source with the moment's line
marked. It is gone rather than kept: the page was showing the same program
twice, once to type into and once to read, and the marked line belongs in the
box the program was typed into.

## The editor is locked while a recording is open

A recording is of the text as it was when it ran. A marker computed from one
and drawn over text that has since been edited points at source that is not
there, so **Run & record** makes the editor read-only — for the run and then
for the replay, with no window in between where the text could move — and
**End replay** gives it back.

A `readonly` textarea refuses keystrokes and says nothing about it, which is
the failure this has to avoid: someone types, nothing happens, and no reason
was given. So the state is written in words directly above the editor, with
the End replay button beside them, and the box is visibly outlined while it is
locked. It is the same standard the picker's `(your own program)` sets — a
state is shown rather than left to be discovered.

Nothing makes a visitor end a replay before doing something else. Run, Run &
record and Compile only all end it themselves and start what was asked for;
choosing a sample ends it after the usual question about replacing an edited
program, so declining leaves the timeline where it was; and Stop, which can
only fire while a run is in flight, unlocks because a terminated run has no
recording to lock over.

It records rather than steps, and the reason is a browser's. A Web Worker
cannot block waiting for a message from the page — there is no synchronous
receive, and an event loop that is inside a wasm call is not running — so the
shape `cove debug` uses, where the prompt runs *inside* the debugger callback,
has no spelling here. `Atomics.wait` on a `SharedArrayBuffer` would give it
one, and a `SharedArrayBuffer` needs the page to be cross-origin isolated,
which needs `Cross-Origin-Opener-Policy` and `Cross-Origin-Embedder-Policy`
headers GitHub Pages does not send. So the machine is not asked anything: it
runs to its end and hands over the whole recording at once. For reading a
program that is the better trade anyway, because the timeline goes backwards.
It is not a refusal of live stepping forever — only until something serves
those two headers.

A recording is bounded three ways, and each says so rather than truncating
quietly: **moments** (the field beside fuel — the first N stops, after which
the run goes on unrecorded and the timeline says `truncated`), a hard ceiling
of 4 MiB of recording, and sixteen frames and thirty-two heap objects per
moment. The **moments** figure is why the timeline can be trusted not to
exhaust the tab: an unbounded recording of a real program would.

What is captured is `cove debug`'s line-change rule with one clause added —
the first instruction, every call and return, and the first instruction
written on a new source line. Everything that rule gets wrong,
`cove debug`'s `help limits` lists, and it is all still true here. The one
you will notice first: a moment is at the *first* instruction carrying a new
line, so a name assigned on that line still shows its old value.

Measured, under node against the release build: the greeting example records
six moments in 3.2 kB; a thousand-turn loop fills the default 1024 moments at
about 200 B each, 206 kB in all. A recorded run of a fourteen-million-
instruction loop takes 186 ms against the plain run's 119 ms.
`crates/cove-wasm/src/record.rs` is where all of this is argued.

## The samples

The picker above the editor offers nine programs, in ascending order of what a
reader has to already understand: values and functions, structs and methods,
enums and `match`, `Result` and `?`, collections, traits and `dyn`, closures, a
host call, and one written to be stepped rather than read. Writing your own
stays what the page is for — the first entry is *(your own program)*, and it
selects itself the moment the editor stops matching the sample it was filled
from. Switching away from an edited program asks first, because nothing here
is saved anywhere.

They are real `.cove` files in `samples/`, and not strings inside `index.html`,
for three reasons:

- `cove fmt --check` at the repository root walks `web/` like everything else
  — it stops only at dot-directories and at `target` — so a sample is
  formatted by the project's own formatter and a drifting one fails CI's
  dogfooding step. Nothing had to be added to any workflow for that.
- A file that stops parsing is a file: `git diff` shows what changed.
- `check.mjs` compiles and runs **every one of them** through the real wasm
  module and asserts what each prints and answers. A sample that stops working
  fails the build rather than greeting a visitor with an error on the live
  site.

`samples.mjs` is the manifest: a label and a one-line description per sample,
for the picker, and what the sample is expected to print and answer, for the
check. It and the directory may not disagree — `check.mjs` compares them as a
*set*, so a file nothing lists and an entry naming no file both fail, and a
rename fails as both at once rather than passing a count.

`09-stepping.cove` is the one written for **Run & record** rather than for
reading: a three-deep call chain, a name assigned from a call, a shadowed
binding, and a local that names a heap object. Its manifest entry says so in a
`records` field, and `check.mjs` records it and asserts the backtrace is still
that deep and a local still names an object — so a rewrite that flattened it
fails instead of leaving a sample whose own comment promises what it no longer
does.

## Colouring the editor, and the disassembly

The editor is a `<pre>` of coloured text with a transparent `<textarea>` on
top of it: what you type into is the textarea, what you see is the `<pre>`,
and the caret is the textarea's own.

The colours come from `cove_lex`, which is the compiler's own lexer and
nothing after it. A keyword list written in JavaScript would be a second,
informal spelling of the language that nothing compares against the first, and
the day a keyword is added the page would keep colouring the old language
with no test anywhere having an opinion. The module in the tab already holds
the front end, so the lexer is simply *there*.

It answers a **tiling**: spans that between them cover every character of the
source exactly once, in order, each named `keyword`, `type`, `string`,
`number`, `comment` or `plain`. The page paints by walking that list and
slicing its own text. Two of the six are not token kinds — `type` is an
identifier beginning with an uppercase letter, which is the rule the parser
itself reads `Ok(value)` by, and `comment` is recovered from the gaps the
lexer left, because it discards comments rather than tokenizing them.
`crates/cove-wasm/src/highlight.rs` argues all of it.

Source that does not lex is the normal case, not the exception: one open quote
and the file has a lexical error, and that is the state a string literal is in
for as long as it takes to write one. So the lexer's *recovered* tokens are
what is coloured — `cove_syntax::lexer::lex_recovered`, which is `lex` without
the step that throws the tokens away — the answer says `ok: false`, and an
unterminated literal is coloured as the string it is up to the end of the
file. Nothing goes plain and nothing goes stale.

The **Lowered IR** pane is coloured the same way and for the same reason, with
one honest difference: there is no lexer for a disassembly to borrow. So
`crates/cove-wasm/src/highlight.rs` reads the six line shapes
`crates/cove-ir/src/print.rs` documents — a header, a frame, a capture, a
local, a blank line, and `pc  opcode operands` — and inside an instruction it
goes by the shape of each token rather than by which instruction it is. It
answers the same tiling, with one category more: `slot`, for `s3:int`, which
is the thing a reader follows from line to line there and which source has
nothing like. Opcodes are keywords, layout names are types, program counters
and immediates are numbers, string literals are strings, and a callee — the
one name written before a ` (` — is left plain along with the local names and
the punctuation.

That reader *is* a second reader of a format, which is the thing the paragraph
above refuses to write in JavaScript. What makes it honest is where it is and
what watches it: it is in Rust beside the crate that prints the text, it says
in `ok` when it met a line it did not recognise, and `check.mjs` fails the
build if that happens on any of the nine samples' real disassembly. Adding an
instruction needs nothing there; changing how *operands* are written does, and
that is exactly what the nine catch.

These are the only calls to the module made on the page's own thread rather
than on the worker. The worker exists because a Cove program can loop and wasm
cannot be interrupted from outside it; lexing is one pass over the text. Being
on this thread is what makes a keystroke and its colours the same frame — the
glyphs are the `<pre>`'s, so an answer a frame late would be a *character* a
frame late.

Neither box wraps, and that is the alignment: the font, border and padding
come from one CSS rule that names both, and with `white-space: pre` there are
no wrap points for a scrollbar on one box and not the other to move. The
colours are colours only, never bold or italic, because those are not the same
width in every monospace font and one glyph of drift is one too many.

## Stopping a run

Two bounds and one blunt instrument.

The **fuel** and **deadline** fields bound a run from inside it. They are what
to reach for: a run they stop reports *why* it stopped, with the limit named,
in the same vocabulary `cove run --fuel` uses.

**Stop** terminates the worker. It is the only way to interrupt WebAssembly
from outside it, and it is blunt: the run is gone and has nothing to say about
itself. The page builds a new worker on the next Run, which means loading the
module again.

Running in a worker at all is why an infinite loop does not freeze the tab.

## The files

| file | what it is |
|---|---|
| `index.html` | the page: the editor, three panes, the timeline's three, and the worker |
| `worker.mjs` | the thread a Cove program runs on |
| `cove.mjs` | the JavaScript half of the C ABI, and the loader |
| `samples.mjs` | the manifest the picker is built from, and what each sample is expected to do |
| `samples/` | the sample programs, as ordinary `.cove` files |
| `check.mjs` | the node harness, run by CI |

The ABI itself — seven exported functions, one import, and a length-prefixed
answer — is documented in `crates/cove-wasm/src/abi.rs`.
