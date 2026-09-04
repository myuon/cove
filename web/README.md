# The Cove playground

A page that compiles and runs Cove in the browser. There is no server: the
parser, the type checker, the lowering and the linear-memory VM are one
WebAssembly module, built from `crates/cove-wasm`, and nothing typed into it
leaves the tab.

## Build it

```
cargo build -p cove-wasm --target wasm32-unknown-unknown --release
```

That writes `target/wasm32-unknown-unknown/release/cove_wasm.wasm`, 3.01 MB.
There is no bundler, no `npm`, and no other step: `index.html`, `cove.mjs` and
`worker.mjs` are loaded as they are.

Optimizing for size gets it to 2.66 MB, a 12% saving, measured:

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
it saw, and then the page scrubs through the timeline: Source, Instructions,
Runtime and Memory, all four moved by one slider. Selecting a frame in Runtime
moves the other three to that frame.

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
| `index.html` | the page: a `<textarea>`, three panes, the timeline's four, and the worker |
| `worker.mjs` | the thread a Cove program runs on |
| `cove.mjs` | the JavaScript half of the C ABI, and the loader |
| `samples.mjs` | the manifest the picker is built from, and what each sample is expected to do |
| `samples/` | the sample programs, as ordinary `.cove` files |
| `check.mjs` | the node harness, run by CI |

The ABI itself — five exported functions, one import, and a length-prefixed
answer — is documented in `crates/cove-wasm/src/abi.rs`.
