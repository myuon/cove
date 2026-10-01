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

**It costs about eleven minutes now, and it has doubled twice.** Measured
2026-09-28, when ADR 0068 was accepted: `cargo ratchet` **11:00**. Of that,
`vm_coverage` is **557.1s** over **289 corpus programs**, 254 of which run;
the Unicode final-sigma table check is 81.5s; and the formatter's comment
probe is **20.8s**. This file said 39s, then 5:11 (301s and 9.6s, 160
programs, 2026-09-21), then 9:34 to 9:36 (481.6s and 78.6s, 196 programs,
2026-09-22), then 11:08 (550.0s and 80.1s, 283 programs, 2026-09-25), and
each was right when it was written.
Nothing regressed any of those times — the corpus did what it is supposed to do and
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
quadratic in file size, and it grows with the corpus: **20.8s over the cores**
as of 2026-09-28 (80.1s on 2026-09-25 and 78.6s on 2026-09-22),
against 9s when this paragraph was written.

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

### `cove-native` is built by default, through `cove-cli`

`crates/cove-native` — the native execution tier of ADR 0055 — has its code
generator behind the `template` feature. **Since
[ADR 0076](docs/adr/0076-the-native-tier-is-built-by-default.md)
(2026-10-01) `cove-cli` turns it on by default**, and Cargo unifies features
across a workspace build, so all five commands above compile the code
generator and `cargo t` runs its tests: `cove-native`'s 107-case
`tests/template.rs`, `cove-runtime`'s 60-case `tests/native_tier.rs`, the
native halves of `float_parse.rs` and `int_bits.rs`, and the end-to-end cases
with a `native` file run on `--backend native` against the VM.

Those tests are gated on `all(feature = "template", target_arch = "x86_64",
unix)`, not on the feature alone — "the tier can *run* here" rather than "the
tier is built". The feature builds everywhere and the code generator refuses
every host but Unix x86-64, so a test gated on the feature alone would turn an
arm64 or Windows contributor's `cargo t` red. Gate a new native test the same
way.

`cove-runtime` and `cove-native` themselves still default the feature off,
because they are what an embedder and the browser playground link, and ADR
0055's gate item 7 — "a build without the native feature has no
executable-memory dependency and retains the full VM corpus" — is about
them. So a command that selects only one of them, `cargo test -p cove-runtime`
or `cargo test -p cove-native`, builds **no** code generator and runs
`native_tier.rs` as a binary with no tests in it. Add `--features template` to
those, or use the workspace commands.

There is one arm. `template` is a hand-written x86-64 template compiler whose
only dependency is `libc`; it is x86-64 and Unix only, and refuses every other
host explicitly with ADR 0055's capability diagnostic (`libc` is a `cfg(unix)`
dependency, so the feature still builds on Windows and arm64 — checked with
`cargo check -p cove-cli --target …` when ADR 0076 landed). ADR 0056 chose it
over Cranelift on a measured comparison and
[ADR 0066](docs/adr/0066-a-comparison-ends-when-its-question-is-answered.md)
retired the loser, so a new IR instruction is lowered **once**. If you find
yourself writing a second lowering, or a `cranelift` feature, read ADR 0066
first — it is the decision you would be reversing.

CI used to run `cove-native`'s and `cove-runtime`'s suites a second time with
`--features template`, in a job of their own, because the default build
reached none of it; this file spent two sections on the ways that went wrong —
a fixture re-pointed at an operation the tier refused, green on every default
command and red on the pull request. ADR 0076 removed both steps: the test
binaries they built were byte-for-byte the ones `cargo t` now builds (the same
hashes under `target/checked/deps`), so they ran the same tests twice.

What CI checks instead is item 7 from the other side, because a default can
now break it:

```console
$ cargo tree -p cove-wasm --target all -e normal --prefix none -f '{p} {f}' \
    | grep -E '^libc |^cove-native .*[ ,]template(,|$)'   # must print nothing
$ cargo test -p cove-cli --no-default-features --profile checked --test e2e
```

The first is in the playground job, and `--target all` is load-bearing: `libc`
is `cfg(unix)`, so a `--target wasm32-unknown-unknown` graph omits it even with
the feature on, and that check passed on exactly the mistake it exists for. The
second builds a `cove` without the tier and runs the end-to-end corpus through
it. **It replaces `target/checked/cove` with the featureless binary**, so run
it after anything that needs `--backend native`, not before — that is the one
remaining form of the old "the feature build goes last" hazard.

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
$ cd ../tools/covefmt
$ ../../target/checked/cove check
$ ../../target/checked/cove test
$ ../../target/checked/cove test --backend ast
$ cd ../.. && ./target/checked/cove-bench --iterations 1
```

`cove test` is the one that matters most and the one most easily forgotten:
it runs 165 `test fn`s in `examples/` and 74 in `tools/covefmt/` — the Cove
formatter, a package of its own since issue 556 — on the linear-memory backend
and then on the interpreter, and it is the first place a lowering change is
felt by a *real* program rather than by a fixture. A change to the IR has been
merged-shaped and green on the five commands while crashing there — the
symptom was a VM panic writing past a frame, and nothing above `cove test`
went anywhere near it.

**And there is one more step after those, in `ci.yml`'s `native` job**, which
is listed here because a gap in this file is what makes a green local run a
red pull request:

```console
$ cargo build --profile checked -p cove-cli
$ cd tools/covefmt
$ ../../target/checked/cove run covefmtBench --files-root ../.. --backend vm
$ ../../target/checked/cove run covefmtBench --files-root ../.. --backend native
```

and the two outputs, with the four timing lines stripped, have to be **equal
byte for byte**. It is a check and not a measurement: a shared runner's wall
clock says nothing, so what is asserted is that the two tiers printed the same
bytes and both exited zero. Since ADR 0076 the plain build above has the tier;
it used to need `--features template`, and the next ordinary `cargo t` would
silently put a binary without it back.

What it catches that nothing above it can is a **native tier that compiles and
answers wrongly on a real program**. `tests/template.rs` holds the code
generator to fixtures; `native_tier.rs` runs the tier from the runtime, on
fixtures again. This is the only place the generator is asked to
compile 111 of covefmt's 113 functions and produce 1.75 MB of formatted Cove
(as of 2026-09-25; it was 107 compiled until #508 admitted `String ==`) —
and it is the only place a wrong answer has a *right* answer sitting beside it
to be diffed against, because the encoded VM ran the same program in the same
command. A code generator that lowered an instruction to the wrong bits would
pass every fixture that did not happen to cover that bit and fail here on the
first file.

It is cheap: the binary is the one the dogfood build already made, and the two
runs are about two and five seconds. Run it after touching the code generator,
`subset.rs`, or any instruction it lowers.

### What the gate costs, measured

Run it in the **background** and keep working. It is the single thing most
likely to leave somebody watching a blank terminal, and none of it needs
watching.

The steady state is about a minute — 22s for `cargo t` on an unchanged tree,
13s for clippy, 26s for `cargo doc`. Changing one file deep in the workspace
and rebuilding everything that depends on it is **59s**. So run the gate when
there is something to gate, not after every edit — but a gate that takes many
minutes is not what this workspace costs, it is something else going on.

ADR 0076 (2026-10-01) put the native tier's tests into `cargo t`: 196 more
cases, about **+21s** on a warm tree — the end-to-end suite's native
comparisons +9.8s, `native_tier.rs` 8.2s, the rest under 3s together. That was
measured on a machine already loaded by other builds, where the same warm
`cargo t` took 3:37 before and 3:58 after, so read the difference and not the
totals.

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
