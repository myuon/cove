//! A compiled frame's references survive a collection, checked by programs
//! built to make each step of that happen and to show that it did
//! ([issue 681]; docs/FUZZING.md, "Roots of compiled frames").
//!
//! "A collection happened while a compiled frame was on the stack" — the
//! differential targets' `gc_while_compiled` — says a compiled frame's roots
//! were *asked for*. It does not say a reference the frame held was live at
//! that moment, nor that anything read it afterwards. Each program here, one
//! shape of root apiece, does all four things in order:
//!
//! 1. builds a value that holds references (a string, a vector of strings, a
//!    struct with a string and an array, a closure whose environment holds a
//!    string, a string in each of three frames);
//! 2. holds it in a frame of a function the native tier compiles;
//! 3. allocates enough garbage in that frame (or in a compiled callee) for the
//!    small heap to collect several times;
//! 4. reads the value afterwards, into the line the program prints.
//!
//! and the test shows each step was taken rather than assuming it:
//!
//! - **the function was compiled and its loop ran as machine code**: the
//!   holder is a function of its own after lowering (not written into its
//!   caller) and the native tier compiled it; and the encoded VM ran less than
//!   a tenth of the instructions on `native` that it ran on `vm`, so the
//!   allocating loop — thousands of instructions — did not run there;
//! - **the collections happened while the value was live**: the run collected
//!   under a compiled frame, and the same program with its allocating loop
//!   emptied collects under one *fewer* times — so the collections the test
//!   counts are the loop's, made after the value was built and before it was
//!   read;
//! - **the read was made and checked**: what the program prints after the
//!   collections is compared byte for byte with the text written below, which
//!   the tree-walking interpreter — which has no heap capacity and no
//!   collector — answers too. Each line holds the value's contents, which only
//!   a read after the loop can produce.
//!
//! The programs are `fuzz/seeds/differential_source/gc-root-*`, so the
//! fuzzer starts from them and `tests/regressions.rs` replays them in every
//! mode as well.
//!
//! What this cannot show: that a *dropped* root would be noticed. A reference
//! a collection wrongly freed is visible only if its memory is reused and
//! overwritten before the read; the loops allocate strings of the held
//! values' size for that reason, and docs/FUZZING.md records the mutation
//! experiment that measured it.
//!
//! [issue 681]: https://github.com/myuon/cove/issues/681

use cove_fuzz::differential::{probe, Mode, GC_HEAP_WORDS};
use cove_runtime::trace::RunOutcome;

/// The heap the compiled runs are made over: the differential targets'
/// small one, on which these programs collect several times.
const SMALL: usize = GC_HEAP_WORDS;

/// The heap the encoded run is made over for its instruction count: the
/// harness's ordinary one.
const LARGE: usize = cove_fuzz::HEAP_WORDS;

struct Case {
    /// The seed's file name.
    name: &'static str,
    /// The functions that hold the value, which must be compiled.
    holders: &'static [&'static str],
    /// What the program prints.
    expected: &'static str,
}

const CASES: [Case; 6] = [
    Case {
        name: "gc-root-string",
        holders: &["holdString"],
        expected: "held:7:217:6839:tail 20 13211\nheld:11:341:10747:tail 22 13811\n",
    },
    Case {
        name: "gc-root-string-callee",
        holders: &["holdAcrossCall", "churn"],
        expected: "held:7:217:6839:tail 20 13211\nheld:11:341:10747:tail 22 13811\n",
    },
    Case {
        name: "gc-root-vector",
        holders: &["holdVector"],
        expected: "item:0:0,item:1:7,item:2:14,item:3:21,item:4:28,item:5:35, 6 13211\n\
                   item:0:0,item:1:11,item:2:22,item:3:33,item:4:44,item:5:55, 6 13811\n",
    },
    Case {
        name: "gc-root-struct",
        holders: &["holdStruct"],
        expected:
            "name:7:217 [7, 14, 21, 28] 70 7 13211\nname:11:341 [11, 22, 33, 44] 110 11 13811\n",
    },
    Case {
        name: "gc-root-closure",
        holders: &["holdClosure"],
        expected: "hello:7:217/1 hello:7:217/2 13211\nhello:11:341/1 hello:11:341/2 13811\n",
    },
    Case {
        name: "gc-root-frames",
        holders: &["outer", "middle", "inner"],
        expected: "outer:7:49 [middle:8:248 [inner:9:8793 13211]]\n\
                   outer:11:77 [middle:12:372 [inner:13:12701 13811]]\n",
    },
];

/// The allocating loop's bound in every program, and what the control
/// replaces it with.
const LOOP: &str = "while i < 600 {";
const NO_LOOP: &str = "while i < 0 {";

fn source(name: &str) -> String {
    let path = format!(
        "{}/../../fuzz/seeds/differential_source/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|why| panic!("{path}: {why}"))
}

/// The interpreter answers the text the test expects: the oracle with no
/// collector agrees with what is written here.
#[test]
fn the_interpreter_prints_what_each_program_should() {
    for case in &CASES {
        let ran = probe(&source(case.name), Mode::Ast, SMALL, &[]).expect("it checks");
        assert_eq!(ran.observed.outcome, RunOutcome::Success, "{}", case.name);
        assert_eq!(
            String::from_utf8_lossy(&ran.observed.stdout),
            case.expected,
            "{}",
            case.name
        );
    }
}

/// The whole chain, on the native tier over the small heap.
#[cfg(all(feature = "template", target_arch = "x86_64", unix))]
#[test]
fn a_compiled_frame_keeps_what_it_holds_across_collections() {
    for case in &CASES {
        let text = source(case.name);
        assert!(
            text.contains(LOOP),
            "{}: the control needs the loop",
            case.name
        );

        let native = probe(&text, Mode::Native, SMALL, case.holders).expect("it checks");
        let o = &native.observed;
        // 2. The holders are functions of their own, and compiled.
        for (name, kept, compiled) in &native.functions {
            assert!(*kept, "{}: `{name}` was written into its caller", case.name);
            assert!(*compiled, "{}: the native tier refused `{name}`", case.name);
        }
        // 4. The read happened, and answered what it should.
        assert_eq!(
            o.outcome,
            RunOutcome::Success,
            "{}: {}",
            case.name,
            o.answer
        );
        assert_eq!(
            String::from_utf8_lossy(&o.stdout),
            case.expected,
            "{}",
            case.name
        );
        // 3. It collected under a compiled frame ...
        let under = o.collections_while_compiled.expect("a lowered mode counts");
        assert!(
            under > 0,
            "{}: no collection under a compiled frame",
            case.name
        );
        // ... and those collections were the loop's: without it there are
        // fewer, so the ones counted were made after the value was built and
        // before it was read.
        let control = probe(
            &text.replace(LOOP, NO_LOOP),
            Mode::Native,
            SMALL,
            case.holders,
        )
        .expect("it checks");
        let without = control
            .observed
            .collections_while_compiled
            .expect("a lowered mode counts");
        assert!(
            under > without,
            "{}: {under} collections under a compiled frame with the loop, {without} without",
            case.name
        );
        // 2. The loop ran as machine code: the encoded VM ran a small part of
        // what it runs when it runs everything.
        let vm = probe(&text, Mode::Vm, LARGE, &[]).expect("it checks");
        let (encoded_native, encoded_vm) = (
            o.instructions.expect("counted"),
            vm.observed.instructions.expect("counted"),
        );
        assert!(
            encoded_native * 10 < encoded_vm,
            "{}: the encoded VM ran {encoded_native} instructions on `native` and \
             {encoded_vm} on `vm`",
            case.name
        );
        assert_eq!(vm.observed.stdout, o.stdout, "{}", case.name);
        // What was measured, for `--nocapture` and docs/FUZZ_EVALUATION.md.
        eprintln!(
            "{}: {} collections, {under} under a compiled frame ({without} without the loop); \
             encoded instructions {encoded_native} on native, {encoded_vm} on vm; {} calls \
             entered compiled code",
            case.name,
            o.collections.unwrap_or(0),
            o.native_calls.unwrap_or(0),
        );
    }
}
