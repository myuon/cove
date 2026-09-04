//! Every program the repository keeps, encoded.
//!
//! `vm_coverage.rs` walks this corpus to say what the machine *runs*, and
//! `differential.rs` to say whether it agrees with the oracle. This walks it
//! to say three things about the fixed-width encoding
//! [ADR 0041](../../../docs/adr/0041-a-slot-number-fits-in-sixteen-bits.md)
//! decides, and it is here rather than beside the encoder because the encoder
//! can only be tested against instructions somebody wrote down by hand:
//!
//! - **the encoder is total.** ADR 0041's audit covers all forty-nine
//!   variants, so no program the compiler lowers holds an instruction it
//!   refuses, and no program's slots are wider than a sixteen-bit field;
//! - **the encoding is lossless.** `decode(encode(inst)) == inst` at every
//!   program counter of every function, so bytecode pc is IR pc and the
//!   debugger's mapping between the two panes is the identity;
//! - **the two verifiers agree.** A lowering `cove_ir::verify` accepts —
//!   which every one of these is, because `lower` panics otherwise — is one
//!   `cove_ir::bytecode::verify` accepts too. They check different things,
//!   one a lowering and one bytes, and a disagreement would mean the byte
//!   verifier refusing programs the compiler is entitled to run.
//!
//! It also records what the corpus is, in the two numbers the frame limit and
//! the width decision were argued from: the widest frame in the repository,
//! and how many opcodes real programs reach.
//!
//! Nothing here runs a program, which is why it is not `#[ignore]`d: the
//! corpus is parsed, checked, lowered and encoded, and the whole survey is a
//! fraction of a second.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use cove_ir::bytecode::{decode, encode_program, verify};
use cove_ir::MAX_FRAME_WORDS;
use cove_sema::HostSchemas;

// The half of the corpus machinery this survey needs is discovery, parsing and
// type-checking. What a *run* is — its arguments, its limits, the entry it
// names — belongs to the two surveys that run something, and is unused here.
#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

use support::{Case, ModuleIndex, Prepared};

/// Every entry point of the repository, in a fixed order: the same set
/// `vm_coverage.rs` walks, for the same reason.
fn discover() -> Vec<Case> {
    let root = support::repo_root();
    let mut roots = vec![root.join("tests/e2e")];
    roots.extend(support::nested_packages(&root.join("tests/e2e")));
    roots.push(root.join("examples"));
    roots.push(root.join("benches"));
    roots
        .iter()
        .flat_map(|package| support::cases_of(&root, package))
        .collect()
}

/// What the survey found.
#[derive(Default)]
struct Survey {
    lowered: usize,
    functions: usize,
    instructions: usize,
    /// The largest `Function::reprs.len()` anywhere in the repository, which
    /// is the number ADR 0041's frame limit is 537 times above.
    widest_frame: usize,
    /// Which opcodes real programs reach, out of the hundred defined.
    reached: BTreeSet<u8>,
    /// Every way a program failed one of the three claims, named.
    faults: Vec<String>,
}

fn survey() -> Survey {
    let mut found = Survey::default();
    let mut indexes: BTreeMap<PathBuf, ModuleIndex> = BTreeMap::new();
    let cases = discover();
    assert!(!cases.is_empty(), "the corpus is empty");
    for case in cases {
        let index = indexes
            .entry(case.root.clone())
            .or_insert_with(|| ModuleIndex::of(&case.root));
        // A package that does not check has no program in it, and a gap in the
        // lowering is `vm_coverage.rs`'s finding rather than this one's:
        // nothing is encoded that was never lowered.
        let Ok(prepared) = Prepared::of(&case, index) else {
            continue;
        };
        let Ok(program) = cove_ir::lower(&prepared.checked, &prepared.sources, &HostSchemas::new())
        else {
            continue;
        };
        found.lowered += 1;
        found.functions += program.functions.len();
        for function in &program.functions {
            found.widest_frame = found.widest_frame.max(function.reprs.len());
            found.instructions += function.code.len();
        }

        let encoded = match encode_program(&program) {
            Ok(encoded) => encoded,
            Err(why) => {
                found
                    .faults
                    .push(format!("{}: does not encode: {why}", case.name));
                continue;
            }
        };
        for (index, code) in encoded.functions.iter().enumerate() {
            found.reached.extend(code.iter().map(|held| held.opcode()));
            for (pc, held) in code.iter().enumerate() {
                let read = decode(*held, pc as u32);
                if read.as_ref() != Ok(&program.functions[index].code[pc]) {
                    let name = program.functions[index].qualified();
                    found.faults.push(format!(
                        "{}: {name}+{pc} does not decode back to itself: {read:?}",
                        case.name
                    ));
                }
            }
        }
        if let Err(rejected) = verify(&program, &encoded) {
            for fault in rejected.iter().take(5) {
                found
                    .faults
                    .push(format!("{}: the bytes are refused: {fault}", case.name));
            }
        }
    }
    found
}

/// One `#[test]` rather than one per claim, because the survey is what costs
/// anything and the three claims are three readings of it.
#[test]
fn every_program_the_repository_keeps_encodes_verifies_and_reads_back() {
    let found = survey();
    println!(
        "the fixed-width encoding over {} corpus program(s):\n  \
           {} functions, {} instructions, {} bytes encoded\n  \
           widest frame {} words, against a limit of {MAX_FRAME_WORDS}\n  \
           {} of the 100 opcodes are reached",
        found.lowered,
        found.functions,
        found.instructions,
        found.instructions * cove_ir::EncodedInst::BYTES,
        found.widest_frame,
        found.reached.len(),
    );
    assert!(
        found.faults.is_empty(),
        "{} program(s) the compiler lowered are not encoded, decoded or verified as they \
         should be:\n  {}",
        found.faults.len(),
        found.faults.join("\n  ")
    );

    // The frame limit's own ratchet. ADR 0041 adopts a cap no program here
    // comes within 500 times of, and the whole argument for adopting it rests
    // on that staying true — so it is asserted rather than remembered.
    assert!(
        found.widest_frame * 500 < MAX_FRAME_WORDS,
        "the widest frame in the repository is {} words, which is no longer far under the \
         {MAX_FRAME_WORDS}-word limit ADR 0041 adopts",
        found.widest_frame
    );

    // Real programs reach most of the instruction set, which is what makes the
    // three claims above worth making rather than a statement about the dozen
    // opcodes a small test happens to use.
    assert!(
        found.reached.len() >= 80,
        "only {} opcodes are reached, and this survey is worth its cost because it reaches \
         most of them",
        found.reached.len()
    );
}
