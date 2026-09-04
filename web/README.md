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
— on every push to `main`, stages `index.html`, `worker.mjs`, `cove.mjs` and
the built `.wasm` into one directory, and runs `node web/check.mjs` against
that build before anything is deployed. A run that does not hold — a program
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
browser uses — and then compiles and runs Cove programs through it: a program
that prints and answers, one that does not parse, one that spawns a task, one
that loops past its fuel, one that loops past its deadline, and one that calls
a capability the playground does not grant. It exits non-zero on the first
that does not hold.

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
| `index.html` | the page: a `<textarea>`, three panes, and the worker |
| `worker.mjs` | the thread a Cove program runs on |
| `cove.mjs` | the JavaScript half of the C ABI, and the loader |
| `check.mjs` | the node harness, run by CI |

The ABI itself — four exported functions, one import, and a length-prefixed
answer — is documented in `crates/cove-wasm/src/abi.rs`.
