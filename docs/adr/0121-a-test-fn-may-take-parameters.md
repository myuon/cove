# ADR 0121: A `test fn` may take parameters, generated from their types

- Status: Accepted
- Date: 2026-10-25
- Decides: that a `test fn` with parameters is a property, whose arguments
  `cove test` generates from the parameters' types, runs over many cases, and
  shrinks when a case fails; that generation and shrinking are the runner's,
  written in Rust, and nothing is added to the language or to `std`; that a
  parameter type the runner cannot generate is a check-time error; that a run
  is deterministic, seeded from the test's name, and `cove test --seed`
  overrides the seed; and that `[test] cases` sets the case count. Phase 1 of
  [issue #703](https://github.com/myuon/cove/issues/703): `Int`, `String`,
  `Bool` and `Array` of those
- Supersedes: [ADR 0007](0007-test-declarations.md)'s "no parameterised tests"

## Context

ADR 0007 left parameterised tests out, "a decision that should be forced by a
representative test suite rather than added because other languages have
it". Three suites have forced it since.

- **`crates/cove-runtime/tests/std_laws.rs`** — issues 679 and 702 wrote the
  standard library's laws as property tests, and about 1,540 lines of Rust
  exist to do it: generators, a shrinker, and the code that calls each Cove law
  function from Rust and reads its answer back. The laws themselves are a few
  lines of Cove each. The harness is the bulk, and it is the bulk because the
  laws cannot be written where the code they describe is written.
- **Four hand-written stride sweeps** in `tests/e2e/` —
  `values_std_url`, `values_std_datetime`, `values_int_to_decimal` and
  `values_string_utf8` — each walk an input space with a loop and a step,
  which is a property test with a fixed, uniform, unshrunk generator.
- **Two hand-written linear congruential generators** in `examples/`
  (`examples/life/rng` and `examples/cq/sample.cove`), each reinventing the
  input source a test would want.

"Earn complexity through use" asks for recurring friction in representative
programs; this is the same friction in three places, in Rust and in Cove. And
the form that removes it needs no syntax: a function that takes parameters is
already how Cove says "for any value of these types".

## Decision

1. **A `test fn` may declare parameters, and one that does is a property.**
   Nothing new is written: the parameters are ordinary parameters, the
   return type stays `Result<Unit, Error>`, `async` stays refused, and `?`,
   `assert` and `assertEqual` work as they do in any test.

   ```cove
   test fn percentRoundTrips(text: String) -> Result<Unit, Error> {
     assertEqual(url.percentDecode(url.percentEncode(text))?, text)?
     Ok(())
   }
   ```

2. **The arguments are generated from the parameters' types**, by
   `cove test`, in Rust. A parameter's type must be one the runner can
   generate, and the checker refuses any other at the parameter, naming the
   type and the set the runner supports (`cove::type::test`). Phase 1's set is
   `Int`, `String`, `Bool`, and `Array<T>` of a generatable `T`, nested arrays
   included; `cove_sema::typeck::Ty::is_generatable` is the one place it is
   written. A `var` or variadic parameter, and a type parameter on the test,
   are refused too: the runner supplies values, never places or types. The
   types the issue's phase 2 adds — `Float`, `Option`, `Result`, `Map`, and the
   package's own structs and enums, recursive ones depth-bounded — widen that
   one predicate and the generator beside it; a type with nothing to generate,
   a function, a capability, a `Shared`, stays a check-time error.

3. **Generation is biased toward what breaks code.** An `Int` is an edge —
   0, ±1, the extremes, ±2^53 and its neighbours, the 32-bit and byte
   boundaries — a quarter of the time, a small number a quarter, a number of
   a uniformly chosen bit width a quarter, and uniform 64-bit bits the rest.
   A `String` mixes printable ASCII with the characters `std` treats
   specially (`% & < > " ' \ + =`, space, the controls, braces), multi-byte
   characters at UTF-8's edges, and astral code points. A *size* grows from 1
   to 100 across a run, as QuickCheck's does, and bounds every length, so the
   first cases are the small ones; a length is usually well under the size
   and now and then several times it.

4. **A failing case is shrunk, by the runner, on the generated tree.** The
   shrinker is greedy: it tries each simpler candidate of each argument, keeps
   the first that still fails, and starts again from it, until none does or
   2,000 runs have been spent. An `Int` moves toward 0 (QuickCheck's
   candidates: 0, then `-n` for a negative `n`, then `n` less ever smaller
   halves of itself); a `String` or `Array` loses chunks, halving down to
   single elements, and then has each element simplified, a character toward
   `a` (or `é`, so a failure that needs a non-ASCII character keeps a
   two-byte one). Any failure counts: an `Err`, and equally a trap — an
   overflow, a refused capability — which is shrunk the same way.

5. **A run is deterministic.** The seed is FNV-1a over the test's qualified
   name and the generator is SplitMix64, so a suite runs the same cases on
   every run and every machine. `cove test --seed <n>` runs every property
   under `n` instead; the failure report names the seed, so `--seed` with
   `--filter` reproduces it. `cove.toml`'s `[test] cases = N` sets how many
   cases each property runs, 100 when it is absent — one number per package,
   for the reason `allow_real` is one list per package.

6. **A property runs on the backend `cove test` runs on.** It is lowered once,
   as an entry of its own exactly as any test is, and each case is an
   invocation of the lowered program with the generated values —
   `Vm::invoke` on the linear-memory backend, `Interpreter::invoke` under
   `--backend ast` — with a host registry of its own, since a fake host holds
   state and one case must not see what another left. The arguments go
   through the same door an embedder's values do, so each is held to the
   parameter type the checker resolved before anything runs.
   `cove_runtime::testing` gains `TestRun::prepare` and
   `TestRun::run_prepared` for it; `TestRun::run`, which an embedder's runner
   calls, is unchanged.

7. **A failure is reported the way a failing test is**, through the same
   diagnostic, with the case added to the message — the shrunk arguments as
   Cove literals, the seed, how many cases passed and how many times it was
   shrunk — and the rerun command as help:

   ```text
   error[cove::test::failed]: test `laws.encodingNeverWritesPercentTwentyFive` failed for `text = "%", limit = 0` (seed 6359041024845030997, 46 case(s) passed, shrunk 8 time(s)): assertion failed: `!url.percentEncode(text).contains("%25") || limit > 1000`
   ```

   A property that holds reports `ok`, with its case count.

8. **Narrowing an input is the property body's job.** A property that wants
   minutes of a day writes `offset % 1440`. A `discard`, or a range written on
   the parameter, is not added until a suite shows the body cannot do it.

## Alternatives rejected

- **A `Gen<T>` and a PRNG in `std`**, with properties as ordinary Cove
  calling a library. It is more surface for every program to see — types,
  combinators, a random source — to serve the test runner only. `Int`
  arithmetic traps on overflow, which is the right default for programs and
  the wrong one for a generator, whose arithmetic wraps by design; and
  shrinking written in Cove, over values of every generatable type, is a
  large library for what the runner does on a tree it built. The two LCGs in
  `examples/` are not evidence for it: they are what the programs needed for
  themselves, not for testing.
- **Keeping the harness Rust-only**, growing `std_laws.rs`. The laws describe
  code written in Cove, and belong in the language it is written in, next to
  it, where `cove test` runs them. What stays in Rust is what has to: the
  comparisons that use a Rust implementation as the oracle (`toDecimal`
  against `format!`, `fromUtf8` against `from_utf8`).
- **An attribute or a new declaration form** (`property fn`, `@cases(500)`).
  A parameter list already says "for any value of these types"; syntax would
  have to earn its place by saying something more, and nothing here needs it.
- **Seeding from the clock.** More cases over many runs, and a suite that is
  red once and green on the retry. The fixed seed makes a property a test
  like any other; a wider search is `--seed` or `[test] cases`.

## Consequences

`test fn` keeps its one shape for the runner and gains its parameters; the
Language Card and the reference say so.

A property costs its case count in runs, each with a registry, a `Runtime`
and a machine of its own, but not a lowering or an encoding: the lowered
program is prepared once (`PreparedProgram`) and each case's machine is built
from it. That sharing is load-bearing, measured on 2026-10-25 under
`--profile checked`: a property whose body does nothing costs **0.35 ms a
case** on the VM when each case builds its machine with `Vm::new`, which
re-encodes the program, and **about 7 µs** with the preparation shared — 10,000
cases in 0.11 s against 3.6 s, the interpreter taking 0.08 s. On
`tests/e2e/test_property` the whole `cove test` — two properties of 100 cases
that hold, two that fail and are shrunk — is 61 ms on the VM against 43 ms for
`cove check` of the same package, and 252 ms under `--backend ast`. A failing
property costs up to 2,000 more runs to shrink.

`--backend native` stays refused by `cove test` for the reason it was
(no aggregate report over each case's tier table), so a property runs on the
VM and the interpreter, and the e2e case compares the two.

Phase 2 widens `Ty::is_generatable` and the generator to the rest of the
issue's types; phase 3 moves `std_laws.rs`'s laws into Cove `test fn`s and
replaces the e2e stride sweeps.
