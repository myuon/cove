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
profile is not a nicety there, because this is the one suite that is
compute-bound rather than spawn-bound.

**It costs about ten minutes now, and it has doubled twice.** Measured
2026-09-22 by three separate runs that agreed: `cargo ratchet` **9:34 to
9:36**, of which `vm_coverage` is **481.6s** and the formatter's comment probe
**78.6s**, over **196 programs**. This file said 39s, then 5:11 (301s and 9.6s,
160 programs, 2026-09-21), and both were right when they were written.
Nothing regressed either time — the corpus did what it is supposed to do and
grew, and `vm_coverage` runs every program on the tree-walking interpreter as
well as on the linear-memory backend, so its cost is linear in a number this
repository is trying to increase. **The figure here is dated because it goes
stale quietly and every migration adds to it**; if yours disagrees by minutes,
the corpus grew again rather than something being wrong. Budget for it: this
is the one part of the gate worth starting before you need the answer.

There are two, and both do their work once per program in the repository.

The first is the roadmap: `crates/cove-cli/tests/vm_coverage.rs` runs every
program in the repository on the linear-memory backend and sorts the answers
into agrees, *disagrees*, and does not lower. It is ignored because it runs
what it lowers, and the benchmark rows are two million turns each.

Its two ratchets are both load-bearing and they are not the same. The count
may rise and never fall. The known-disagreement set is compared as a *set*,
because a count cannot tell a new disagreement from an old one — a change
that teaches one family and breaks another raises the count while introducing
a program that lowers and lies. The set has caught that twice.

The second is the formatter's comment probe in
`crates/cove-syntax/src/format.rs`, which inserts a comment at every line of
every `.cove` file and checks the formatter still emits it. It is there
because the test beside it — comparing the comments the scanner finds in the
input against the ones it finds in the output — is the scanner judging itself,
and it passed for as long as [issue 402](https://github.com/myuon/cove/issues/402)
existed. A marker the test inserts and counts itself is an oracle the
formatter does not supply. Every variant reparses a whole file, so the work is
quadratic in file size, and it grows with the corpus: **78.6s over the cores**
as of 2026-09-22, against 9s when this paragraph was written.

### Adding a program to the repository trips a ratchet in `cargo t`

`crates/cove-cli/tests/copies.rs` surveys **every program in the repository**
and asserts a total, so a new directory under `tests/e2e/` or `benches/` moves
that number whether or not it has anything to do with your change. It is not
one of the two `#[ignore]`d ratchets above — it runs in a plain `cargo t`, and
it is the one most likely to fail a gate you thought you had not touched.

Two things about it are worth knowing before you reach for the constant.

**Decompose the rise; do not accept it.** The interesting question is how much
came from the *lowering* and how much from the new program merely existing, and
the two are separable: run the survey with the new directories removed, then
add them back. Every migration in ADR 0064's series has done this, and it has
paid — #475's lowering moved the total by **−7**, the first fall in the file's
history, which a bare "it went up by 94" would have hidden entirely; #477's rise
of 98 was 63 the new program and 35 the lowering, and the 35 were one
pre-existing standard-library copy appearing at more expansion sites.

**A new program also needs its row in `tests/e2e/cove.toml`.** Forgetting it
fails somewhere that does not name the file you added.

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

`crates/cove-native` — the native execution tier of ADR 0055 — has its code
generator behind a feature that is **off by default**, because the ADR's
adoption gate asks that "a build without the native feature has no
executable-memory dependency" and that is a fact about the dependency graph
rather than about which functions compile. So `--workspace` builds it as a
crate of ABI declarations, `cargo t` runs two of its tests, and the lowering
is compiled by none of the five commands.

There is one arm. `template` is a hand-written x86-64 template compiler whose
only dependency is `libc`; it is x86-64 only, and refuses every other host
explicitly. ADR 0056 chose it over Cranelift on a measured comparison and
[ADR 0066](docs/adr/0066-a-comparison-ends-when-its-question-is-answered.md)
retired the loser, so a new IR instruction is lowered **once**. If you find
yourself writing a second lowering, or a `cranelift` feature, read ADR 0066
first — it is the decision you would be reversing.

So, after a change under `crates/cove-native/`:

```console
$ cargo clippy -p cove-native --all-targets --features template --profile checked -- -D warnings
$ RUSTDOCFLAGS="-D warnings" cargo doc -p cove-native --no-deps --features template --profile checked
$ cargo test -p cove-native --features template --profile checked
```

CI runs those three in one step, for the same reason it runs the other five:
a run there and a run here are the same run. They are seconds now that the
Cranelift dependency graph is gone, so this is cheap to run and cheap to
forget — and forgetting it is a green gate over an untested code generator.

**Those three are not the whole of the native gate.** `.github/workflows/ci.yml`
has a *second* native step, and it is the one that drives the tier through the
runtime:

```console
$ cargo clippy -p cove-runtime -p cove-cli --all-targets \
    --features template --profile checked -- -D warnings
$ RUSTDOCFLAGS="-D warnings" cargo doc -p cove-runtime -p cove-cli --no-deps \
    --features template --profile checked
$ cargo test -p cove-runtime -p cove-cli --features template --profile checked
```

It catches what `-p cove-native` structurally cannot. That crate's suite
compiles the code generator; the cases that *enter* it from the VM live in
`cove-runtime`, and `crates/cove-runtime/tests/native_tier.rs` is
`#![cfg(feature = "template")]` — so a default build compiles it to a binary
with **no tests in it**. `cargo t`, `cargo test --workspace` and all three
commands above report it green without running a case; only this step runs one.
**Since ADR 0066 it is also the only place a cross-implementation check on
machine code happens at all** — every case in it runs the same entry on
`Vm::new` and on `Vm::with_native` and asserts they agree — so it is worth more
than it was when a second code generator was the other check.

This has already been got wrong once, and the shape is what to remember rather
than the file: a change re-pointed one of that file's fixtures at a new
operation, every command above passed, and the pull request was red on a loop
the tier had refused. The subset the tier compiles is much narrower than Cove —
`crates/cove-native/src/subset.rs` is the whole list, and it admits no float
constant, comparison or arithmetic — so a fixture is compiled only if someone
ran the step that compiles it.

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
forgotten: it runs 223 `test fn`s in `examples/`, on the linear-memory backend
and then on the interpreter, and it is the first place a lowering change is
felt by a *real* program rather than by a fixture. A change to the IR has been
merged-shaped and green on the five commands while crashing there — the
symptom was a VM panic writing past a frame, and nothing above `cove test`
went anywhere near it.

**And there is one more step after those, at `ci.yml`'s line 328**, which is
the same class of gap as the second native step above and is listed here for
the same reason — it was not, and a gap in this file is what makes a green
local run a red pull request:

```console
$ cargo build --profile checked -p cove-cli --features template
$ cd examples
$ ../target/checked/cove run covefmtBench --files-root .. --backend vm
$ ../target/checked/cove run covefmtBench --files-root .. --backend native
```

and the two outputs, with the four timing lines stripped, have to be **equal
byte for byte**. It is a check and not a measurement: a shared runner's wall
clock says nothing, so what is asserted is that the two tiers printed the same
bytes and both exited zero.

What it catches that nothing above it can is a **native tier that compiles and
answers wrongly on a real program**. The three `-p cove-native` commands hold
the code generator to fixtures; the second native step runs the tier from the
runtime, on fixtures again. This is the only place the generator is asked to
compile 103 of covefmt's 109 functions and produce 905 KB of formatted Cove —
and it is the only place a wrong answer has a *right* answer sitting beside it
to be diffed against, because the encoded VM ran the same program in the same
command. A code generator that lowered an instruction to the wrong bits would
pass every fixture that did not happen to cover that bit and fail here on the
first file.

It is cheap: the build is a feature flag on a workspace that is already warm,
and the two runs are about two and five seconds. Run it after touching the
code generator, `subset.rs`, or any instruction it lowers.

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
