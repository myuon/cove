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

Run the ignored ones by hand: `cargo test --workspace --lib --tests -- --ignored`.
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

Before pushing, the full gate is what CI runs: `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`,
and `cargo doc --workspace --no-deps`.

### What the gate costs, measured

Run it in the **background** and keep working. It is the single thing most
likely to leave somebody watching a blank terminal, and none of it needs
watching.

The steady state is about a minute — 22s for `cargo t` on an unchanged tree,
13s for clippy, 26s for `cargo doc`. Everything above that is the first build
after a change, and that is where a wait of several minutes comes from. So
run the gate when there is something to gate, not after every edit.

Two things were guessed wrong before they were measured, and both guesses are
worth not repeating.

**Clippy and `cargo t` do not invalidate each other.** `cargo t` immediately
after a clippy run takes 12s, not a rebuild. The order they run in does not
matter and neither needs a target directory of its own.

**`--release` is not the faster gate.** What costs time here is compilation,
not running: `cargo t` is 2,300 tests that finish in seconds, and an
optimised build takes rustc *longer*. The one place release would pay is the
`#[ignore]`d coverage ratchet, which really is compute-bound — it runs the
whole corpus — and even there the rebuild has to be earned back.

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
