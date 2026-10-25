#![no_main]

//! `cove_fuzz::ir_mutate`; see `crates/cove-fuzz` and `docs/FUZZING.md`.

libfuzzer_sys::fuzz_target!(|data: &[u8]| cove_fuzz::ir_mutate(data));
