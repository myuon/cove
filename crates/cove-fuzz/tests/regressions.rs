//! Replays every committed fuzz input through the harness that found it.
//!
//! `fuzz/regressions/<target>/` holds minimised inputs that once crashed a
//! harness and no longer do; `fuzz/seeds/<target>/` holds the small starting
//! inputs each target's corpus is built from. Both are replayed here, on
//! stable Rust, under `cargo t` — which is what makes a fixed fuzz finding a
//! regression test rather than a file nobody runs. `docs/FUZZING.md` says how
//! an input gets here.
//!
//! `fuzz/regressions/<target>/known/` holds inputs that **still** crash: a
//! finding recorded and not yet fixed. They are not replayed, because a
//! crash there may be an abort or a stack overflow, which would take this
//! test binary down rather than fail one test. Fixing one means moving it up
//! a directory, where this file starts replaying it.

use std::path::{Path, PathBuf};

fn fuzz_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz")
}

/// The committed inputs for `target`: its seeds and its fixed regressions,
/// in a stable order. A `README.md` beside them is prose, not an input.
fn inputs(target: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in ["seeds", "regressions"] {
        let Ok(entries) = std::fs::read_dir(fuzz_dir().join(dir).join(target)) else {
            continue;
        };
        for entry in entries {
            let path = entry.expect("a readable directory entry").path();
            let readme = path.extension().is_some_and(|e| e == "md");
            if path.is_file() && !readme {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn replay(target: &str) {
    let harness = cove_fuzz::harness(target).expect("a target cove-fuzz names");
    let inputs = inputs(target);
    for input in &inputs {
        let bytes = std::fs::read(input).expect("a readable input");
        eprintln!("replaying {}", input.display());
        harness(&bytes);
    }
}

#[test]
fn parse_replays() {
    replay("parse");
}

#[test]
fn compile_replays() {
    replay("compile");
}

#[test]
fn run_replays() {
    replay("run");
}

#[test]
fn structured_replays() {
    replay("structured");
}

#[test]
fn image_replays() {
    replay("image");
}

#[test]
fn ir_replays() {
    replay("ir");
}

#[test]
fn ir_mutate_replays() {
    replay("ir_mutate");
}

/// Every target has seeds, so a target whose directory was renamed is
/// noticed here rather than by a corpus that quietly started empty.
#[test]
fn every_target_has_committed_seeds() {
    for target in cove_fuzz::TARGETS {
        assert!(
            inputs(target)
                .iter()
                .any(|p| p.starts_with(fuzz_dir().join("seeds"))),
            "fuzz/seeds/{target}/ has no inputs"
        );
    }
}

/// Bytes for the generator, from a fixed seed: an xorshift, so the inputs
/// are the same on every run and every machine.
fn choices(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// The generator's promise, held over fixed inputs: every program it writes
/// checks and lowers. `structured` panics on a refusal too, but a fuzzer
/// finds that by chance; this finds it on every `cargo t`.
#[test]
fn generated_programs_check() {
    for seed in 0..48 {
        let program = cove_fuzz::generate::from_bytes(&choices(seed, 64 + 48 * seed as usize));
        if let Err(why) = cove_fuzz::diagnose(&program) {
            panic!("seed {seed} generated a program that was refused:\n{why}\n{program}");
        }
    }
}

/// A generator whose every program stops on its first statement would pass
/// the test above and reach nothing. So a share of fixed programs has to run
/// to the end on the VM — not most of them, since stopping on an overflow or
/// a refused range is half of what the generator is for.
#[test]
fn generated_programs_reach_the_end() {
    let seeds = 0..32u64;
    let mut finished = 0;
    for seed in seeds.clone() {
        let program = cove_fuzz::generate::from_bytes(&choices(seed, 256 + 64 * seed as usize));
        let outcomes = cove_fuzz::explain(&program);
        assert!(
            !outcomes.iter().any(|o| o.starts_with("refused")),
            "seed {seed}: {outcomes:?}\n{program}"
        );
        if outcomes.iter().any(|o| o == "vm: ok") {
            finished += 1;
        }
        // And every one of them runs without a panic on both tiers.
        cove_fuzz::structured(&choices(seed, 256 + 64 * seed as usize));
    }
    let total = seeds.count();
    assert!(
        finished * 4 >= total,
        "only {finished} of {total} generated programs ran to the end on the VM"
    );
}

/// Programs that are not UTF-8 are not inputs at all, and an empty one is a
/// program with no `main`. Neither may panic.
#[test]
fn degenerate_inputs() {
    for target in cove_fuzz::TARGETS {
        let harness = cove_fuzz::harness(target).expect("a named target");
        harness(b"");
        harness(&[0xff, 0xfe, 0x00]);
        harness(&[b'('; 4096]);
    }
}

#[test]
fn differential_replays() {
    replay("differential");
}

#[test]
fn differential_source_replays() {
    replay("differential_source");
}

/// The differential generator's promise, held as [`generated_programs_check`]
/// holds the other's: every program it writes checks and lowers.
#[test]
fn differential_programs_check() {
    for seed in 0..48 {
        let program =
            cove_fuzz::generate::differential_from_bytes(&choices(seed, 64 + 48 * seed as usize));
        if let Err(why) = cove_fuzz::diagnose(&program) {
            panic!("seed {seed} generated a program that was refused:\n{why}\n{program}");
        }
    }
}

/// Every mode agrees over fixed inputs, and the comparison is not vacuous: a
/// share of the programs reach the end on the interpreter, so that more than
/// a stop is compared, and the small-heap modes collect at least sometimes.
#[test]
fn the_modes_agree_over_fixed_inputs() {
    use cove_fuzz::differential::Mode;

    let seeds = 0..32u64;
    let (mut finished, mut collected) = (0, 0);
    for seed in seeds.clone() {
        let program =
            cove_fuzz::generate::differential_from_bytes(&choices(seed, 256 + 64 * seed as usize));
        let report = cove_fuzz::differential::compare(&program)
            .unwrap_or_else(|why| panic!("seed {seed} was refused:\n{why}\n{program}"));
        assert!(
            report.mismatch.is_none(),
            "seed {seed}:\n{program}\n{}",
            report.describe()
        );
        let ast = report.observed.iter().find(|o| o.mode == Mode::Ast);
        if ast.is_some_and(|o| o.stopped.is_none() && o.outcome.as_str() != "invariant") {
            finished += 1;
        }
        if report
            .observed
            .iter()
            .any(|o| o.mode == Mode::VmGc && o.collections.unwrap_or(0) > 0)
        {
            collected += 1;
        }
    }
    let total = seeds.count();
    assert!(
        finished * 4 >= total,
        "only {finished} of {total} generated programs reached the end on the interpreter"
    );
    assert!(
        collected > 0,
        "no generated program collected on the small heap"
    );
}
