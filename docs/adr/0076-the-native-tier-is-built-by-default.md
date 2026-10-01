# ADR 0076: The native tier is built by default

- Status: Accepted
- Date: 2026-10-01
- Decides: that the `cove` binary is built with the native tier — `cove-cli`'s
  `template` feature is on by default — so that `--backend native` works on a
  supported host without a special build; that the libraries below it
  (`cove-runtime`, `cove-native`) keep the feature off by default, so an
  embedder and the browser playground get no executable-memory dependency
  unless they ask for one; and that a host the code generator cannot serve
  still builds. Recorded for step 3 of
  [issue #556](https://github.com/myuon/cove/issues/556)
- Supersedes: [ADR 0066](0066-a-comparison-ends-when-its-question-is-answered.md)'s
  "`template` stays a feature that is off by default", for the `cove-cli`
  crate only. Everything else in ADR 0066 stands, including the one code
  generator and where its tests went
- Refers to, without superseding:
  [ADR 0055](0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md),
  whose adoption gate is the condition for native execution becoming the
  *default execution*. This ADR does not make it so. Its gate item 7 — "a
  build without the native feature has no executable-memory dependency and
  retains the full VM corpus" — is kept true, and is checked rather than
  assumed; see Decision 3.
  [ADR 0022](0022-the-vm-is-the-default-backend.md), whose VM stays the
  default backend

## Context

Issue #556 asks whether `cove fmt` can move from the Rust formatter to
`examples/covefmt`, the formatter written in Cove, and lists three
preconditions. The first — covefmt checking its own output for a change of
meaning — landed in #557. The third is that the native tier be available
where `cove fmt` runs, because the formatter is a performance-class question:
measured 2026-10-01 over the repository's 384 files, covefmt's pipeline is
1,381 ms on the encoded VM and 471 ms on the native tier, against 104 ms for
the Rust formatter. The tier compiles 108 of covefmt's 110 reachable
functions and carries 100.0% of its calls; it is 2.9 times the VM's speed and
faster in every phase in every one of fifteen paired rounds.

Today none of that is available to anyone who has not built `cove` with
`--features template`. ADR 0066 kept the feature off by default, and gave the
reason in one clause: ADR 0055's gate asks that a build without the feature
have no executable-memory dependency, "and that is a fact about `cargo tree`".

That clause is right about the fact and wider than it needs to be about the
crate. The dependency that matters to an embedder or to the playground is the
one in `cove-runtime`'s graph, because that is the crate they link. The `cove`
binary is not embedded anywhere; it is the tool. Keeping the feature off there
has had two costs:

- **Every consumer has to remember it**, and forgetting is silent until it is
  not. CLAUDE.md spends three sections on the ways a default build does not
  reach the code generator — `native_tier.rs` compiled to a binary with no
  tests in it, a pull request red on a loop the tier had refused, a
  `cargo t` that replaced the feature build under a measurement fourteen
  rounds in.
- **The tier cannot be a dependency of anything shipped.** A `cove fmt` that
  runs covefmt cannot count on a tier that the default build does not contain.

## Decision

### 1. `cove-cli` builds the native tier by default

`crates/cove-cli/Cargo.toml` gets `default = ["template"]`. A plain
`cargo build -p cove-cli`, `cargo install`, and every workspace command build
the code generator. `--no-default-features` builds the binary without it.

The default *backend* does not change. `cove run` is the encoded VM unless
`--backend native` is given, exactly as ADR 0022 and ADR 0055 leave it.
Selecting native execution remains explicit and reported; nothing a program
does changes which tier runs it.

### 2. The libraries keep the feature off

`cove-runtime` and `cove-native` keep `default = []`. `cove-wasm` and any
embedder depending on `cove-runtime` get the VM and no `libc`, `mmap` or
`mprotect` unless they enable `template` themselves. Cargo's feature
unification means a *workspace* build that includes `cove-cli` compiles
`cove-runtime` with the feature; a build that selects only the embedding crate
— which is how the playground is built, `cargo build -p cove-wasm --target
wasm32-unknown-unknown` — does not.

### 3. ADR 0055's item 7 is checked, not inferred

CI asserts that the playground's dependency graph contains no `libc`, and
that `cove-cli --no-default-features` still builds and passes the VM corpus.
ADR 0066's reasoning was that item 7 is a fact about `cargo tree`; a fact that
a default can now break is one a gate has to look at.

### 4. Every host still builds

`template`'s use of `mmap`/`mprotect` is confined to the hosts that have them,
and the code generator answers a host it cannot serve with ADR 0055's
capability diagnostic, as it already does for any architecture other than
x86-64. Turning the feature on by default must not make `cove` fail to build
anywhere it built before. On such a host `--backend native` is refused with
that diagnostic and everything else is unchanged.

## What this does not decide

- **Native execution as the default.** That is ADR 0055's adoption gate and a
  follow-up ADR. Building the tier by default is the precondition for it, not
  a step past it.
- **Another architecture.** The tier remains x86-64 only. On arm64 the
  default build contains the code generator and refuses to run it, and a
  covefmt-based `cove fmt` there would run on the VM.
- **The formatter switch.** Issue #556's step 2 — not compiling covefmt from
  source on every invocation — is open, and the switch itself needs its own
  decision.

## Consequences

- `cargo t` runs `crates/cove-runtime/tests/native_tier.rs` and the native
  half of every test gated on `template`, because the workspace build now
  unifies the feature in. The separate CI steps that existed to reach them
  remain as long as they test anything the workspace run does not.
- CLAUDE.md's sections on the feature being off, and the instructions in
  `scripts/covefmt-tiers.sh`, `scripts/covefmt-profile.sh` and
  `examples/covefmt/README.md` to build with `--features template` *last*,
  describe a hazard that no longer exists for the `cove` binary and are
  rewritten.
- The `cove` binary carries the code generator — a few hundred kilobytes and a
  `libc` dependency — whether or not it is used. That is the price, and it is
  paid by the tool and not by anything that embeds Cove.

## Alternatives considered

**Make native execution the default now.** Rejected for this ADR, not on its
merits: ADR 0055 names seven pieces of evidence for it and this ADR gathers
none of them. Building the tier by default is what lets that evidence be
gathered by every run instead of by whoever remembered a flag.

**Turn the feature on in `cove-runtime`.** Rejected: that is the crate an
embedder links and the one the playground is built from, and ADR 0055's item 7
is about exactly them.

**Keep it off and have `cove fmt` say so.** Rejected: a formatter that is
fast only on a special build is a formatter whose speed is a build flag, and
the hazards CLAUDE.md records would stay.
