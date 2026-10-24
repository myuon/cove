#![no_main]

//! `cove_fuzz::run`; see `crates/cove-fuzz` and `docs/FUZZING.md`.

libfuzzer_sys::fuzz_target!(|data: &[u8]| cove_fuzz::run(data));
