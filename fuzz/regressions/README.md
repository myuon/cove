# Fuzz regressions

One directory per target. Every file in `<target>/` is an input that once
crashed that harness and no longer does; `crates/cove-fuzz/tests/regressions.rs`
replays each of them under `cargo t`. Files in `<target>/known/` still crash
and are not replayed; each has a line below. See `docs/FUZZING.md`.

| Input | Found | What it was |
|---|---|---|
| `differential/generator-vector-chain-nests-too-deep` | 2026-10-24, `differential`, session without ASan | A generator bug, not a toolchain one: `fresh_vector` built its array at a fixed depth, and an array can be a fresh vector's `toArray()`, so the pair could chain without the depth growing — here 54 `.toVector()).toArray()` links, which the parser refuses (`cove::parse::nesting_too_deep`, correctly). The generated program was refused and `differential` panicked as it should. `fresh_vector` now takes the depth. |
| `image/span-delta-overflow` | 2026-10-24, `image`, ASan session | `serial::decode` added a span's start delta to the previous span's start, and the end delta to that, unchecked: an `i64` delta near the limits overflowed and panicked (`attempt to add with overflow`, `serial.rs`). Now checked and refused as "a span lands outside a file". |

An `image` input carries the hashes of the standard-library files it names,
so after the standard library changes, `decode` refuses such an input earlier
than the place it once crashed. It still must not panic, which is what the
replay checks, but it no longer reaches that place.

## Known

| Input | Found | What it is |
|---|---|---|
| `image/known/verify-param-layout-out-of-range` | 2026-10-24, `image`, ASan session | A body `decode` accepts whose function names a parameter layout id past the end of the layout table; `cove_ir::verify` then panics in `Function::param_words` (`program.rs`, `index out of bounds: the len is 20 but the index is 75`, from `verify::Check::run`). Not a crash `cove` can reach: images come only from this build, and the `image` target no longer runs `verify` (ADR 0112 §5). Kept as the first thing to fix if images are ever accepted from elsewhere — `decode` checking every id against its table, or `verify` checking them before it indexes. Replay with `cargo run --profile checked -p cove-fuzz -- verify-image <file>`. |
