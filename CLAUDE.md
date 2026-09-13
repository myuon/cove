# Cove

## Philosophy

[docs/PHILOSOPHY.md](docs/PHILOSOPHY.md) is what Cove is trying to be. Read it
when designing a new feature, weighing a trade-off, or deciding whether a cost
is acceptable — not only when writing an ADR. The sections most often reached
for are "Earn complexity through use" (a feature is added when representative
programs show recurring friction, not when it is imaginable), "Syntax must earn
its place", and "Preserve the performance class" (a small measured slowdown may
buy simplicity, correctness, or maintainability; a change of performance class
may not).

## Tests

`cargo t` is the local test command — an alias in `.cargo/config.toml` for
`cargo test --workspace --lib --bins --tests`. It leaves out the doc examples
and the `#[ignore]`d cases, which together are more than half of a warm
`cargo test --workspace` and which CI runs in steps of their own.

**Never run a bare `cargo test`.** Both aliases carry `--profile checked`,
and a direct `cargo test` — including `cargo test -p … --test …` for a single
file — silently drops it and runs unoptimised, which on this suite costs 4x
to 6x. Cargo cannot default a profile for `test` and ignores an alias that
shadows a built-in, so nothing enforces this: pass `--profile checked`
yourself when you invoke `cargo test` directly. This has already been got
wrong twice, once for 241 seconds.

Run the ignored ones with `cargo ratchet`, an alias for the same thing under
`--profile checked`. Use the alias rather than writing the command out: the
profile is not a nicety there, it is 39s against 241s, because this is the
one suite that is compute-bound rather than spawn-bound.
There is one, and it is the roadmap: `crates/cove-cli/tests/vm_coverage.rs`
runs every program in the repository on the linear-memory backend and sorts
the answers into agrees, *disagrees*, and does not lower. It is ignored
because it runs what it lowers, and the benchmark rows are two million turns
each.

Its two ratchets are both load-bearing and they are not the same. The count
may rise and never fall. The known-disagreement set is compared as a *set*,
because a count cannot tell a new disagreement from an old one — a change
that teaches one family and breaks another raises the count while introducing
a program that lowers and lies. The set has caught that twice.

Before pushing, the full gate is what CI runs, and CI runs all of it under
`--profile checked`: `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets --profile checked -- -D warnings`,
`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --profile checked`,
`cargo test --workspace --profile checked`, and `cargo ratchet`.

The profile is on every one of them for the reason it is on the aliases —
this suite runs Cove programs, so unoptimised it is several times slower —
and CI carries it too, so a run there and a run here are the same run. Pass
it by hand; nothing can default it for you.

**`RUSTDOCFLAGS="-D warnings"` is not optional on the `cargo doc` line**, and
it is the one place a local run and a CI run were not the same. Without it a
broken intra-doc link is a *warning* and `cargo doc` exits zero, so the gate
reads green locally and both CI jobs stop on it — `.github/workflows/ci.yml`
sets the variable and so does `pages.yml`. This has already been got wrong
once, on a link to a private item from another module, and the failure mode is
the worst kind: a gate that passes and a pull request that is red.

### `cove-native` is behind a feature, so the five commands do not reach it

`crates/cove-native` — the native execution tier of ADR 0055 — has its whole
Cranelift dependency edge behind a `native` feature that is **off by
default**, because the ADR's adoption gate asks that "a build without the
native feature has no executable-memory dependency" and that is a fact about
the dependency graph rather than about which functions compile. So
`--workspace` builds it as a crate of ABI declarations, `cargo t` runs two of
its eighteen tests, and the lowering itself is compiled by none of the five
commands.

Three more, then, after a change under `crates/cove-native/`:

```console
$ cargo clippy -p cove-native --all-targets --features native --profile checked -- -D warnings
$ RUSTDOCFLAGS="-D warnings" cargo doc -p cove-native --no-deps --features native --profile checked
$ cargo test -p cove-native --features native --profile checked
```

CI runs exactly those three, in one step, for the same reason it runs the
other five: a run there and a run here are the same run. Cranelift is about
twenty seconds of compilation the first time and nothing after, so this is
cheap to run and cheap to forget — and forgetting it is a green gate over an
untested code generator.

### The five commands are not the whole job

CI's `test, lint, and dogfood` job runs the five above and then **runs the
toolchain against this repository's own Cove**, which is the half that catches
what a Rust test cannot:

```console
$ cargo build --profile checked -p cove-cli -p cove-bench
$ ./target/checked/cove fmt --check
$ ./target/checked/cove reference --check
$ cd examples
$ ../target/checked/cove check
$ ../target/checked/cove generate --check
$ ../target/checked/cove test
$ ../target/checked/cove generate --check --backend ast
$ ../target/checked/cove test --backend ast
$ cd .. && ./target/checked/cove-bench --iterations 1
```

`cove test (examples)` is the one that matters most and the one most easily
forgotten: it runs 165 `test fn`s in `examples/`, on the linear-memory backend
and then on the interpreter, and it is the first place a lowering change is
felt by a *real* program rather than by a fixture. A change to the IR has been
merged-shaped and green on the five commands while crashing there — the
symptom was a VM panic writing past a frame, and nothing above `cove test`
went anywhere near it.

### What the gate costs, measured

Run it in the **background** and keep working. It is the single thing most
likely to leave somebody watching a blank terminal, and none of it needs
watching.

The steady state is about a minute — 22s for `cargo t` on an unchanged tree,
13s for clippy, 26s for `cargo doc`. Changing one file deep in the workspace
and rebuilding everything that depends on it is **59s**. So run the gate when
there is something to gate, not after every edit — but a gate that takes many
minutes is not what this workspace costs, it is something else going on.

Usually that something else is **two builds at once**. A rebuild measured at
481s while two agents were building measured 59s alone: the same work, eight
times the wall clock, because they contend for the CPU and serialise on
cargo's lock. Worse, this repository has tests that assert *timing maxima*
(`crates/cove-runtime/tests/responsiveness.rs`), and those fail under
contention for no reason at all — a red suite that says nothing about the
code. One heavy command at a time.

**`[profile.dev] debug = "line-tables-only"` buys nothing here, measured.**
59s against 59s for the same one-file rebuild. It is the obvious thing to
reach for and it is worth not reaching for twice: what costs time is the
codegen and the linking of fifteen test binaries, not the debug info in them.

Two things were guessed wrong before they were measured, and both guesses are
worth not repeating.

**Clippy and `cargo t` do not invalidate each other.** `cargo t` immediately
after a clippy run takes 12s, not a rebuild. The order they run in does not
matter and neither needs a target directory of its own.

**`cargo t` runs optimised, and that is the single biggest thing about its
cost.** `[profile.checked]` inherits `release` and turns `debug-assertions`
and `overflow-checks` back on, so every check the unoptimised build has is
still there. The whole suite finishes in **20s**; from scratch, build and all,
it is 85s.

The reason is that this suite *runs Cove programs* rather than merely
compiling a harness: the end-to-end suite spawns the real binary 248 times
and measured 28s unoptimised against 7s optimised, and `trace_replay` and
`embedding` are the same shape. This file previously said the opposite —
that release could not help because the time was compilation and the tests
finished in seconds — and that was generalised from a `cargo t` measurement
that had stopped early at a failing target without running the slow suites at
all. **Measure the suites individually before believing a total**, which is
what eventually settled it.

`target/` grows to tens of gigabytes, most of it `debug/deps`. A rename or a
deletion leaves the old crate's artifacts behind forever: after the backend
cutover there were 4.9 GB under `cove_lir`, a crate that no longer existed.
Nothing collects those, so sweeping by the dead name is worth doing after a
rename, and it is safe — no current target can reference an artifact named
after one that is gone.

## Architecture Decision Records

ADRs live in `docs/adr/`, numbered sequentially.

**An accepted ADR is immutable.** Once an ADR's status is `Accepted`, its
decision does not change. If the decision needs to change, write a new ADR
that supersedes it. Do not amend, reword, or extend the decision in place —
not to correct it, not to narrow it, not to record what a later change made
true.

The reason is that an ADR is a record of what was decided and why it was
decided *at the time*. Editing it destroys exactly the thing it exists to
preserve: a reader can no longer tell what the project believed when it
committed to a course, or what it learned that made it change course. A
superseding ADR keeps both, and the pair reads as a history.

This overrides the "Amendment (date): ..." sections found in older ADRs. That
convention is retired. Leave the existing ones where they are — removing them
would be the same mistake in the other direction — but do not add more.

### Superseding

A new ADR that replaces an older one:

- states `Supersedes: [ADR NNNN](NNNN-slug.md)` in its header;
- explains what changed and why the earlier decision no longer holds, not just
  what the new decision is.

The superseded ADR gets exactly one edit, to its header, and nothing else:

- its status becomes `Superseded by [ADR NNNN](NNNN-slug.md)`.

That pointer is the only permitted change to an accepted ADR, because without
it the new decision is unfindable from the old one. The body stays as written,
including the parts the new ADR contradicts.

Most supersession is partial: a broad ADR such as
[0001](docs/adr/0001-mvp-language-design.md) decides many things at once, and a
later ADR usually replaces one of them. Then the new ADR says
`Supersedes: [ADR NNNN](NNNN-slug.md)'s <named decision>`, and the older ADR's
header gains `Superseded in part by [ADR NNNN](NNNN-slug.md)`, naming which
decision. Its prose still stays untouched — including the sentence that is now
wrong. The pointer is what tells a reader to go find out how.

A new ADR that does not contradict an earlier one supersedes nothing. It
refers to the earlier ADR from its own Context, one way, and does not edit it.

### Numbering

Take the next free number. When two branches in flight both claim one, the
second to merge renumbers — which means moving the file *and* updating every
link to it, including back-links from other ADRs and from `README.md`.
