//! `Float.exp`, `ln`, `log10`, `sin`, `cos` and `pow` against a 300-bit
//! reference, in units in the last place.
//!
//! The six are Cove — `std.float`'s FreeBSD-derived bodies over `Float` and
//! `Int` arithmetic (ADR 0098) — and their contract is **within one unit in
//! the last place of the exact value**, which no golden of printed answers can
//! hold them to: a golden says what the code answered on the day, and an
//! answer two units out would have been written into it as faithfully as one
//! half a unit out. So the oracle here is one the subject does not ship.
//! `float_math_reference.txt` was written by `float_math_reference.py` with
//! `mpmath` at 300 bits, each exact value carried as two doubles — the value
//! correctly rounded, and the rest in units in its last place — so that the
//! error of an answer can be measured here in `f64` arithmetic alone, with no
//! bignum of the test's own, subnormal answers included.
//!
//! # The inputs
//!
//! About four thousand rows: seeded random draws from every regime the six
//! have, and their edges — see the generator's docstring for the list. The
//! rows that matter most are the ones a careless reimplementation gets wrong
//! by more than a unit and still prints a plausible number for: `sin` and
//! `cos` far past `2^20 · π/2`, where the reduction has to be exact,
//! including `6381956970095103 · 2^797`, the double closest to a multiple of
//! `π/2`; `pow` within `2^-20` of one with an exponent past `2^31`, where
//! fdlibm's own leading product was one bit short; and the subnormal answers
//! of `exp` and `pow`, which have to be rounded once.
//!
//! # What is asserted
//!
//! - every answer is within **one** unit in the last place of the exact value,
//!   and a `NaN` exactly where the exact answer is not a real number;
//! - an exact value that is a double — `ln(1)`, every normal power of ten's
//!   `log10`, every power of two integers below `2^53` — is answered exactly;
//! - the worst error seen is printed, so that a change which moves it is
//!   visible in the test's output before it is visible in a failure.
//!
//! # The tiers
//!
//! Every row runs on the encoded VM. Under the `template` feature on a host
//! the native tier runs on, the same rows run through compiled code too, and
//! the two are held to each other **bit for bit** as well as to the
//! reference: the functions are Cove built from correctly rounded operations,
//! so a tier that answered a different bit would be a code generator that
//! lowered one of them wrongly.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_runtime::{Grants, HostRegistry, Runtime, Value, Vm};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::{Compiler, Config};

const MODULE: &str = "m";

/// The subject, in a function the native tier compiles, and a caller it
/// refuses.
///
/// `counts` is recursive, so `computes` is not a leaf and
/// `cove_ir::lower::inline` cannot expand it into `callsComputes`: an
/// expanded call would run on the caller's tier, which is the VM.
const SOURCE: &str = "\
/// Recursive, so no caller of this can be inlined away. `counts(0)` is zero.
export fn counts(n: Int) -> Int {
  if n <= 0 {
    0
  } else {
    counts(n - 1) + 1
  }
}

/// One of the six, by number, in a body the native tier compiles.
export fn computes(which: Int, x: Float, y: Float, n: Int) -> Float {
  let guard = counts(n)
  if which == 0 {
    return x.exp()
  }
  if which == 1 {
    return x.ln()
  }
  if which == 2 {
    return x.log10()
  }
  if which == 3 {
    return x.sin()
  }
  if which == 4 {
    return x.cos()
  }
  x.pow(y)
}

/// A caller the native tier refuses, so that the call is reached across the
/// boundary: the outermost frame is always encoded.
export fn callsComputes(which: Int, x: Float, y: Float, n: Int) -> Float {
  let nothing = Shared(0).lock(fn(v) { v })
  computes(which, x, y, n)
}
";

/// The six, in [`SOURCE`]'s numbering.
const FUNCTIONS: [&str; 6] = ["exp", "ln", "log10", "sin", "cos", "pow"];

/// One reference row.
#[derive(Clone, Debug)]
struct Row {
    /// An index into [`FUNCTIONS`].
    which: usize,
    x: f64,
    /// `pow`'s exponent, and `0.0` for the other five.
    y: f64,
    /// The exact value as `hi` correctly rounded and what is left of it in
    /// units in the last place of `hi`; `None` where the exact answer is not
    /// a real number.
    exact: Option<(f64, f64)>,
    /// Whether the exact value is `hi` itself, so that the answer must be.
    representable: bool,
}

impl Row {
    fn shown(&self) -> String {
        if self.which == 5 {
            format!("{:e}.pow({:e})", self.x, self.y)
        } else {
            format!("{:e}.{}()", self.x, FUNCTIONS[self.which])
        }
    }
}

/// `float_math_reference.txt`, read.
fn rows() -> Vec<Row> {
    let text = include_str!("float_math_reference.txt");
    let bits = |word: &str| f64::from_bits(u64::from_str_radix(word, 16).expect("hex bits"));
    text.lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(|line| {
            let words: Vec<&str> = line.split(' ').collect();
            let which = FUNCTIONS
                .iter()
                .position(|name| *name == words[0])
                .expect("a function this file knows");
            let arity = if which == 5 { 2 } else { 1 };
            let x = bits(words[1]);
            let y = if arity == 2 { bits(words[2]) } else { 0.0 };
            let (exact, representable) = match words[1 + arity] {
                "nan" => (None, false),
                hi => (
                    Some((bits(hi), bits(words[2 + arity]))),
                    words[3 + arity] == "=",
                ),
            };
            Row {
                which,
                x,
                y,
                exact,
                representable,
            }
        })
        .collect()
}

/// The error of `got` against `hi` plus `rest` units in its last place, in
/// those units; infinite for an answer that is not even the right kind of
/// number.
fn error(got: f64, exact: Option<(f64, f64)>) -> f64 {
    let Some((hi, rest)) = exact else {
        return if got.is_nan() { 0.0 } else { f64::INFINITY };
    };
    if got.is_nan() {
        return f64::INFINITY;
    }
    if hi.is_infinite() {
        return if got == hi { 0.0 } else { f64::INFINITY };
    }
    if got.is_infinite() {
        return f64::INFINITY;
    }
    // A unit in the last place at `hi`: the gap to the next double away from
    // nought, which is the least subnormal at and below the least normal.
    let unit = if hi == 0.0 {
        f64::from_bits(1)
    } else {
        let magnitude = hi.abs();
        f64::from_bits(magnitude.to_bits() + 1) - magnitude
    };
    // `got - hi` is exact for the neighbours this can be asked about, and so
    // is the quotient by a power of two; a far answer is far whatever the
    // rounding.
    ((got - hi) / unit - rest).abs()
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

/// One program, lowered once, that every row runs through.
struct Subject {
    program: Arc<cove_sema::resolve::Program>,
    sources: Arc<SourceMap>,
    lowered: Arc<cove_ir::Program>,
}

/// The arguments [`SOURCE`]'s two entry points take for `row`.
fn arguments(row: &Row) -> Vec<Value> {
    vec![
        Value::int(row.which as i64),
        Value::float(row.x),
        Value::float(row.y),
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

    /// Every row's answer on the encoded VM.
    fn on_the_vm(&self, rows: &[Row]) -> Vec<f64> {
        let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
        let runtime = Runtime::new(
            Arc::clone(&self.program),
            Arc::clone(&self.sources),
            Arc::clone(&hosts),
        );
        rows.iter()
            .map(|row| {
                Vm::new(&runtime, &hosts, &self.lowered)
                    .invoke(MODULE, "computes", arguments(row))
                    .unwrap_or_else(|error| {
                        panic!("{} stopped the VM: {}", row.shown(), error.message)
                    })
                    .as_float()
                    .expect("a `Float`")
            })
            .collect()
    }

    /// Every row's answer from the native tier's compiled `computes`.
    #[cfg(all(feature = "template", target_arch = "x86_64", unix))]
    fn on_the_native_tier(&self, rows: &[Row]) -> Vec<f64> {
        let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
        let runtime = Runtime::new(
            Arc::clone(&self.program),
            Arc::clone(&self.sources),
            Arc::clone(&hosts),
        );
        let native = cove_runtime::compile_native(&self.lowered).expect("this host compiles");
        let refusals = native.refusals();
        let refused: Vec<&str> = refusals.iter().map(|row| row.name.as_str()).collect();
        assert!(
            !refused.contains(&"m.computes"),
            "`computes` is meant to be compiled, and the tier refused it"
        );
        assert!(
            refused.contains(&"m.callsComputes"),
            "`callsComputes` is meant to be refused, and the tier took it"
        );
        // A helper that fell back to the VM would make every row through it
        // the VM's answer again, and the comparison below would be the VM
        // held to itself.
        for name in [
            "std.float.exp",
            "std.float.ln",
            "std.float.log10",
            "std.float.sin",
            "std.float.cos",
            "std.float.pow",
            "std.float.reduced",
            "std.float.reducedFar",
            "std.float.twoOverPi",
            "std.float.scaled",
            "std.float.twoTo",
            "std.float.exponentOf",
        ] {
            assert!(
                !refused.contains(&name),
                "`{name}` is meant to be compiled, and the tier refused it"
            );
        }
        rows.iter()
            .map(|row| {
                let mut vm = Vm::with_native(&runtime, &hosts, &self.lowered, &native);
                let value = vm
                    .invoke(MODULE, "callsComputes", arguments(row))
                    .unwrap_or_else(|error| {
                        panic!("{} stopped the native tier: {}", row.shown(), error.message)
                    });
                assert!(
                    vm.tiers().vm_to_native >= 1,
                    "{}: the crossing into compiled code was taken: {:?}",
                    row.shown(),
                    vm.tiers()
                );
                value.as_float().expect("a `Float`")
            })
            .collect()
    }
}

/// Holds `answers` to the reference, naming every row past one unit and
/// every exact value answered inexactly, and printing the worst error per
/// function.
fn within_one_unit(tier: &str, rows: &[Row], answers: &[f64]) {
    let mut wrong = Vec::new();
    let mut worst = [0.0_f64; 6];
    for (row, &got) in rows.iter().zip(answers) {
        let e = error(got, row.exact);
        worst[row.which] = worst[row.which].max(e);
        let bound_broken = e >= 1.0;
        let exact_missed = row.representable && e != 0.0;
        if bound_broken || exact_missed {
            wrong.push(format!(
                "  {}: {tier} {got:e}, exact {:?} ({e:.3} ulp)",
                row.shown(),
                row.exact
            ));
        }
    }
    for (name, e) in FUNCTIONS.iter().zip(worst) {
        println!("{tier}: {name} worst {e:.4} ulp");
    }
    assert!(
        wrong.is_empty(),
        "{} of {} row(s) are past one unit in the last place, or inexact where the \
         exact value is a double, on the {tier}:\n{}",
        wrong.len(),
        rows.len(),
        wrong.join("\n")
    );
}

#[test]
fn every_row_is_within_one_unit_on_the_vm() {
    let rows = rows();
    assert!(rows.len() > 3000, "the reference has its rows");
    let answers = Subject::new().on_the_vm(&rows);
    within_one_unit("VM", &rows, &answers);
}

#[cfg(all(feature = "template", target_arch = "x86_64", unix))]
#[test]
fn every_row_is_within_one_unit_on_the_native_tier_and_matches_the_vm_bit_for_bit() {
    let rows = rows();
    let subject = Subject::new();
    let native = subject.on_the_native_tier(&rows);
    within_one_unit("native tier", &rows, &native);
    let vm = subject.on_the_vm(&rows);
    let differ: Vec<String> = rows
        .iter()
        .zip(native.iter().zip(&vm))
        .filter(|(_, (n, v))| n.to_bits() != v.to_bits() && !(n.is_nan() && v.is_nan()))
        .map(|(row, (n, v))| format!("  {}: native {n:e}, VM {v:e}", row.shown()))
        .collect();
    assert!(
        differ.is_empty(),
        "{} row(s) answer different bits on the two tiers:\n{}",
        differ.len(),
        differ.join("\n")
    );
}
