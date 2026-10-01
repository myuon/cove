//! `Int`'s seven bit operations against an independent 64-bit reference, on
//! every tier.
//!
//! [ADR 0074](../../../docs/adr/0074-an-int-also-carries-a-fixed-width-bit-pattern.md)
//! asks for deterministic randomized results against an independent 64-bit
//! reference, with the interpreter, the encoded VM and the native tier all
//! agreeing, and for boundary tables over nought, all ones, both ends of
//! `Int`, the alternating masks and every valid shift count. This is both.
//!
//! The reference is [`reference`], and it is independent of every tier on
//! purpose: the shifts are the ADR's definitions computed in 128-bit
//! arithmetic — `u(x) * 2^n mod 2^64`, `floor(x / 2^n)`,
//! `floor(u(x) / 2^n)` — rather than a second copy of `<<` and `>>`, and a
//! count outside `0..=63` is refused by a comparison written here.
//!
//! # The tiers
//!
//! The subject is `computed`, one Cove function over an operation code, two
//! operands and a guard. It runs on the tree-walking interpreter and on the
//! encoded VM directly, and under the `template` feature on the native tier
//! through `callsComputed`, a refused caller of the compiled `computed` — the
//! shape `native_tier.rs` and `float_parse.rs` use — and the native case
//! asserts that the crossing into compiled code was taken, so that it cannot
//! compare the VM with itself.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_runtime::interp::Interpreter;
use cove_runtime::{Grants, HostRegistry, Runtime, Value, Vm};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::{Compiler, Config};

const MODULE: &str = "m";

/// The subject, in a function the native tier compiles, and a caller it
/// refuses.
///
/// `counts` is recursive, so `computed` is not a leaf and
/// `cove_ir::lower::inline` cannot expand it into `callsComputed`: an
/// expanded operation would run on the caller's tier, which is the VM.
const SOURCE: &str = "\
/// Recursive, so no caller of this can be inlined away. `counts(0)` is zero.
export fn counts(n: Int) -> Int {
  if n <= 0 {
    0
  } else {
    counts(n - 1) + 1
  }
}

/// Operation `op` over `x` and `y`: 0 to 2 are `bitAnd`, `bitOr` and
/// `bitXor`, 3 is `bitNot` of `x`, and 4 to 6 are the three shifts of `x` by
/// `y`.
export fn computed(op: Int, x: Int, y: Int, n: Int) -> Int {
  let guard = counts(n)
  if op == 0 {
    x.bitAnd(y)
  } else if op == 1 {
    x.bitOr(y)
  } else if op == 2 {
    x.bitXor(y)
  } else if op == 3 {
    x.bitNot()
  } else if op == 4 {
    x.shiftLeft(y)
  } else if op == 5 {
    x.shiftRight(y)
  } else {
    x.shiftRightLogical(y)
  }
}

/// A caller the native tier refuses, so that `computed` is reached across the
/// boundary: the outermost frame is always encoded.
export fn callsComputed(op: Int, x: Int, y: Int, n: Int) -> Int {
  let nothing = Shared(0).lock(fn(v) { v })
  computed(op, x, y, n)
}
";

/// The seven operations, by the code `computed` reads.
const OPERATIONS: [&str; 7] = [
    "bitAnd",
    "bitOr",
    "bitXor",
    "bitNot",
    "shiftLeft",
    "shiftRight",
    "shiftRightLogical",
];

/// What an operation answered: an `Int`, or the sentence it stopped with.
type Answer = Result<i64, String>;

/// ADR 0074's contract, written out independently of every tier.
fn reference(op: usize, x: i64, y: i64) -> Answer {
    let (ux, uy) = (x as u64, y as u64);
    if op <= 3 {
        return Ok(match op {
            0 => ux & uy,
            1 => ux | uy,
            2 => ux ^ uy,
            _ => u64::MAX - ux,
        } as i64);
    }
    if !(0..=63).contains(&y) {
        return Err(format!("a shift count must be between 0 and 63, got {y}"));
    }
    let power = 1u128 << y;
    Ok(match op {
        4 => (u128::from(ux) * power) as u64 as i64,
        5 => i128::from(x).div_euclid(power as i128) as i64,
        _ => (u128::from(ux) / power) as u64 as i64,
    })
}

/// A value `computed` answered, in the terms the comparison is made in.
fn answered(result: Result<Value, cove_runtime::RuntimeError>) -> Answer {
    match result {
        Ok(value) => Ok(value.as_int().expect("`computed` answers an `Int`")),
        Err(error) => Err(error.message),
    }
}

/// [`SOURCE`] checked, with the standard library attached.
fn checked() -> (Arc<SourceMap>, Arc<cove_sema::resolve::Program>) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("m/main.cove");
    let file = sources.add(path.clone(), SOURCE);
    let ast = cove_syntax::parse_file(&sources, file).expect("the fixture parses");
    let mut modules = BTreeMap::from([(
        MODULE.to_string(),
        Module {
            name: MODULE.to_string(),
            dir: PathBuf::from(MODULE),
            units: vec![Unit { file, path, ast }],
        },
    )]);
    for (name, module) in cove_sema::stdlib::attach(&mut sources).expect("stdlib parses") {
        modules.insert(name, module);
    }
    let package = Package {
        root: PathBuf::new(),
        config: Config::default(),
        modules,
    };
    match Compiler::new().compile(&package) {
        Ok(program) => (Arc::new(sources), Arc::new(program)),
        Err(items) => panic!(
            "the fixture checks:\n{}",
            items
                .iter()
                .map(|item| cove_diag::render(&sources, item))
                .collect::<Vec<_>>()
                .join("")
        ),
    }
}

/// One program, lowered once, that every row of a case runs through.
struct Subject {
    program: Arc<cove_sema::resolve::Program>,
    sources: Arc<SourceMap>,
    lowered: Arc<cove_ir::Program>,
}

/// One row: an operation code and its two operands.
type Row = (usize, i64, i64);

fn arguments(&(op, x, y): &Row) -> Vec<Value> {
    vec![
        Value::int(op as i64),
        Value::int(x),
        Value::int(y),
        Value::int(0),
    ]
}

impl Subject {
    fn new() -> Subject {
        let (sources, program) = checked();
        let lowered = Arc::new(
            cove_ir::lower(&program, &sources, &cove_sema::HostSchemas::new())
                .expect("the fixture lowers"),
        );
        Subject {
            program,
            sources,
            lowered,
        }
    }

    fn runtime(&self) -> (Arc<HostRegistry>, Runtime) {
        let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
        let runtime = Runtime::new(
            Arc::clone(&self.program),
            Arc::clone(&self.sources),
            Arc::clone(&hosts),
        );
        (hosts, runtime)
    }

    /// Every row's answer on the tree-walking interpreter.
    fn on_the_interpreter(&self, rows: &[Row]) -> Vec<Answer> {
        let (_hosts, runtime) = self.runtime();
        rows.iter()
            .map(|row| {
                answered(Interpreter::new(&runtime).invoke(MODULE, "computed", arguments(row)))
            })
            .collect()
    }

    /// Every row's answer on the encoded VM, each on a machine of its own.
    fn on_the_vm(&self, rows: &[Row]) -> Vec<Answer> {
        let (hosts, runtime) = self.runtime();
        rows.iter()
            .map(|row| {
                answered(Vm::new(&runtime, &hosts, &self.lowered).invoke(
                    MODULE,
                    "computed",
                    arguments(row),
                ))
            })
            .collect()
    }

    /// Every row's answer from the native tier's compiled `computed`.
    #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
    fn on_the_native_tier(&self, rows: &[Row]) -> Vec<Answer> {
        let (hosts, runtime) = self.runtime();
        let native = cove_runtime::compile_native(&self.lowered).expect("this host compiles");
        let refused: Vec<&str> = native
            .refusals()
            .iter()
            .map(|row| row.name.as_str())
            .collect();
        assert!(
            !refused.contains(&"m.computed"),
            "`computed` is meant to be compiled, and the tier refused it"
        );
        assert!(
            refused.contains(&"m.callsComputed"),
            "`callsComputed` is meant to be refused, and the tier took it"
        );
        rows.iter()
            .map(|row| {
                let mut vm = Vm::with_native(&runtime, &hosts, &self.lowered, &native);
                let answer = answered(vm.invoke(MODULE, "callsComputed", arguments(row)));
                assert!(
                    vm.tiers().vm_to_native >= 1,
                    "{row:?}: the crossing into the compiled `computed` was taken: {:?}",
                    vm.tiers()
                );
                assert_eq!(
                    vm.tiers().native_to_vm,
                    0,
                    "{row:?}: and nothing went back the other way: {:?}",
                    vm.tiers()
                );
                answer
            })
            .collect()
    }
}

/// Holds `answers` to [`reference`], naming every row that disagreed.
fn agree(tier: &str, rows: &[Row], answers: &[Answer]) {
    let wrong: Vec<String> = rows
        .iter()
        .zip(answers)
        .filter_map(|(&(op, x, y), answer)| {
            let expected = reference(op, x, y);
            (*answer != expected).then(|| {
                format!(
                    "  {x}.{}({y}): {tier} {answer:?}, reference {expected:?}",
                    OPERATIONS[op]
                )
            })
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "{} of {} row(s) disagree with the reference on the {tier}:\n{}",
        wrong.len(),
        rows.len(),
        wrong.join("\n")
    );
}

/// `0x5555_5555_5555_5555` and `0xAAAA_AAAA_AAAA_AAAA`.
const FIVES: i64 = 0x5555_5555_5555_5555;
const ACES: i64 = 0xAAAA_AAAA_AAAA_AAAA_u64 as i64;

/// ADR 0074's boundary operands.
const BOUNDARY: &[i64] = &[0, -1, i64::MIN, i64::MAX, FIVES, ACES];

/// The counts a shift refuses, the two ends of `Int` among them.
const INVALID_COUNTS: &[i64] = &[-1, 64, 65, i64::MIN, i64::MAX];

/// The boundary table: every boundary operand under every two-operand bit
/// operation with every other and under `bitNot`; shifted by every count
/// from 0 to 63 on all three shifts; and shifted by every invalid count. The
/// ADR's own example table is in it too, row for row.
fn boundary() -> Vec<Row> {
    let mut rows = vec![
        (0, 5, 3),
        (1, 5, 3),
        (2, 5, 3),
        (3, 0, 0),
        (3, -1, 0),
        (4, 1, 63),
        (4, -1, 1),
        (5, -3, 1),
        (6, -1, 1),
        (6, -1, 63),
        // Floor, not truncation toward zero.
        (5, -1, 63),
        (5, -5, 2),
        (5, -7, 1),
    ];
    for &x in BOUNDARY {
        for &y in BOUNDARY {
            for op in 0..3 {
                rows.push((op, x, y));
            }
        }
        rows.push((3, x, 0));
        for op in 4..7 {
            for count in 0..64 {
                rows.push((op, x, count));
            }
            for &count in INVALID_COUNTS {
                rows.push((op, x, count));
            }
        }
    }
    rows
}

/// A small, fixed-seed generator, so that every run asks the same rows.
struct Seeded(u64);

impl Seeded {
    /// SplitMix64.
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// An operand: usually arbitrary bits, sometimes a boundary operand or a
    /// small number, so that the edges come up among the random rows too.
    fn operand(&mut self) -> i64 {
        match self.next() % 8 {
            0 => BOUNDARY[(self.next() % BOUNDARY.len() as u64) as usize],
            1 => (self.next() % 256) as i64 - 128,
            _ => self.next() as i64,
        }
    }

    /// A shift count: usually a valid one, sometimes one either side of the
    /// word, and sometimes arbitrary bits, which are almost never valid.
    fn count(&mut self) -> i64 {
        match self.next() % 16 {
            0 => INVALID_COUNTS[(self.next() % INVALID_COUNTS.len() as u64) as usize],
            1 => self.next() as i64,
            _ => (self.next() % 64) as i64,
        }
    }
}

/// `count` seeded random rows, spread over the seven operations.
fn random(seed: u64, count: usize) -> Vec<Row> {
    let mut rng = Seeded(seed);
    (0..count)
        .map(|_| {
            let op = (rng.next() % 7) as usize;
            let x = rng.operand();
            let y = if op >= 4 { rng.count() } else { rng.operand() };
            (op, x, y)
        })
        .collect()
}

const SEED: u64 = 0x0b17_5eed_0074;
const RANDOM_ROWS: usize = 3000;

// ------------------------------------------------------------------ cases

/// The reference is the ADR's table before it is anything else.
#[test]
fn the_reference_answers_the_adr_s_table() {
    for (row, want) in [
        ((0, 5, 3), 1),
        ((1, 5, 3), 7),
        ((2, 5, 3), 6),
        ((3, 0, 0), -1),
        ((3, -1, 0), 0),
        ((4, 1, 63), i64::MIN),
        ((4, -1, 1), -2),
        ((5, -3, 1), -2),
        ((6, -1, 1), i64::MAX),
        ((6, -1, 63), 1),
        ((5, -1, 63), -1),
    ] {
        assert_eq!(reference(row.0, row.1, row.2), Ok(want), "{row:?}");
    }
}

/// The boundary table, on the interpreter.
#[test]
fn the_boundary_table_agrees_on_the_interpreter() {
    let rows = boundary();
    let answers = Subject::new().on_the_interpreter(&rows);
    agree("interpreter", &rows, &answers);
}

/// The boundary table, on the VM.
#[test]
fn the_boundary_table_agrees_on_the_vm() {
    let rows = boundary();
    let answers = Subject::new().on_the_vm(&rows);
    agree("VM", &rows, &answers);
}

/// The boundary table, on the native tier.
#[cfg(all(feature = "template", target_arch = "x86_64", unix))]
#[test]
fn the_boundary_table_agrees_on_the_native_tier() {
    let rows = boundary();
    let answers = Subject::new().on_the_native_tier(&rows);
    agree("native tier", &rows, &answers);
}

/// A few thousand seeded random rows, on the interpreter.
#[test]
fn seeded_random_rows_agree_on_the_interpreter() {
    let rows = random(SEED, RANDOM_ROWS);
    let answers = Subject::new().on_the_interpreter(&rows);
    agree("interpreter", &rows, &answers);
}

/// The same rows, on the VM.
#[test]
fn seeded_random_rows_agree_on_the_vm() {
    let rows = random(SEED, RANDOM_ROWS);
    let answers = Subject::new().on_the_vm(&rows);
    agree("VM", &rows, &answers);
}

/// The same rows, on the native tier.
#[cfg(all(feature = "template", target_arch = "x86_64", unix))]
#[test]
fn seeded_random_rows_agree_on_the_native_tier() {
    let rows = random(SEED, RANDOM_ROWS);
    let answers = Subject::new().on_the_native_tier(&rows);
    agree("native tier", &rows, &answers);
}
