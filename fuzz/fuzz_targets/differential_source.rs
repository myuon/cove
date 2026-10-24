#![no_main]

//! `cove_fuzz::differential_source`; see `crates/cove-fuzz` and `docs/FUZZING.md`.

libfuzzer_sys::fuzz_target!(|data: &[u8]| cove_fuzz::differential_source(data));
