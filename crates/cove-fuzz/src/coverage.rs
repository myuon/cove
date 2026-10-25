//! Which instructions a corpus *executed*, not only which its programs held
//! ([issue 705]).
//!
//! `cove-fuzz ir-stats` and the statistics of [`crate::stats`] say which kinds
//! of instruction appear in a program that verified. An instruction that
//! appears after a trap, behind a loop that never ends, or in a function
//! nobody calls is present and never runs, and a check of what it computes
//! never happens. This module measures the second thing, over a corpus, on a
//! path of its own — a replay, never a fuzzing session — because it watches
//! every instruction and is many times slower than the run it watches.
//!
//! # How
//!
//! For each input, the program the target would run is built the way the
//! target builds it (generated, lowered, decoded or mutated) and, if
//! `cove_ir::verify` accepts it:
//!
//! 1. **present**: every instruction of every function that is not a stub is
//!    counted under its key ([`key`]). **present_compiled** is the same over
//!    the functions the native tier compiles — eagerly, as the differential
//!    modes compile them — so it is what the native modes could have run as
//!    machine code.
//! 2. The program is run once on the encoded VM (`vm`'s heap and limits), to
//!    learn how many instructions the run took — `N`.
//! 3. It is run again on the encoded VM with a [`Debugger`] installed, which
//!    the machine asks before every instruction. The debugger records the
//!    function and pc of each, and halts the run at the `N`th, so that it
//!    watches the instructions the unwatched run executed and not more. Its
//!    deadline is [`WATCH_SLOWDOWN`] times the target's, because watching is
//!    slow. **executed** is every key of an instruction the debugger saw;
//!    **executed_compiled** those of them in a function the native tier
//!    compiled, other than the entry — whose frame is the outermost, and the
//!    outermost frame always runs on the encoded tier.
//!
//! The counts are of **programs**: `coverage.<key>.executed` is how many
//! programs executed an instruction of that key at least once.
//!
//! # What it does not say
//!
//! - **The native tier's own execution is not observed.** The debugger
//!   watches the encoded VM; compiled code calls no hook. `executed_compiled`
//!   is the inference that an instruction the VM executed in a function the
//!   tier compiled is one the native modes executed as machine code, which
//!   holds when the native run follows the same path — what the differential
//!   checks. An entry that calls itself is counted as never compiled.
//! - **A run bounded by the deadline is approximated.** The unwatched run's
//!   `N` is wall-clock dependent, and the watched run may be stopped by its
//!   own deadline before it reaches `N`.
//! - **Executed is not checked.** That an instruction ran says nothing about
//!   whether its answer reached anything the differential compares.
//!
//! [issue 705]: https://github.com/myuon/cove/issues/705

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cove_diag::SourceMap;
use cove_ir::{Inst, Program};
use cove_runtime::{Debugger, Limits, OwnedVm, PreparedProgram, Resume, Runtime, Stop, Value, Vm};

use crate::differential::{self, Mode};
use crate::stats::Stats;
use crate::{on_stack, ENTRY, HEAP_WORDS, MODULE};

/// How much longer than the target's deadline a watched run is given.
pub const WATCH_SLOWDOWN: u32 = 40;

/// The targets [`replay`] can measure.
pub const TARGETS: [&str; 5] = [
    "differential",
    "differential_source",
    "ir",
    "ir_mutate",
    "image",
];

/// The key an instruction is counted under: its kind, as
/// [`crate::irgen::kind_of`] names it, and for the arithmetic and comparison
/// families the operation and what it operates on as well — `Arith.Int.Mul`,
/// `Cmp.Int.Lt` — since a defect in one operation of a family is not reached
/// by executing another.
pub fn key(inst: &Inst) -> String {
    match inst {
        Inst::Arith { num, op, .. } => format!("Arith.{num:?}.{op:?}"),
        Inst::ArithImm { op, .. } => format!("ArithImm.{op:?}"),
        Inst::Cmp { on, op, .. } => format!("Cmp.{on:?}.{op:?}"),
        Inst::Neg { num, .. } => format!("Neg.{num:?}"),
        other => format!("{:?}", crate::irgen::kind_of(other)),
    }
}

/// How a program is entered.
enum Entry {
    /// A source's `playground.main`, as `cove run` enters it.
    Main,
    /// A lowered program's entry with these arguments, as `cove fmt` enters
    /// covefmt.
    Lowered {
        module: Arc<str>,
        name: Arc<str>,
        args: Vec<crate::ir::Arg>,
    },
}

/// A program to measure.
struct Measured {
    program: Arc<Program>,
    sources: Arc<SourceMap>,
    checked: Arc<cove_sema::resolve::Program>,
    entry: Entry,
    limits: Limits,
}

/// The program `target` would run for `data`, or `None` if it would run none.
fn subject(target: &str, data: &[u8]) -> Option<Measured> {
    let lowered = |source: String| {
        crate::front_staged(&source).ok().map(|lowered| Measured {
            program: Arc::new(lowered.program),
            sources: Arc::new(lowered.sources),
            checked: Arc::new(lowered.checked),
            entry: Entry::Main,
            limits: crate::limits(),
        })
    };
    let ir = |subject: crate::ir::Subject| Measured {
        program: Arc::new(subject.program),
        sources: subject.sources,
        checked: Arc::default(),
        entry: Entry::Lowered {
            module: subject.module,
            name: subject.name,
            args: subject.args,
        },
        limits: crate::ir::limits(),
    };
    match target {
        "differential" => lowered(crate::generate::differential_from_bytes(data)),
        "differential_source" => lowered(std::str::from_utf8(data).ok()?.to_string()),
        "ir" => Some(ir(crate::irgen::from_bytes(data))),
        "ir_mutate" => crate::mutate::from_bytes(data).map(ir),
        "image" => crate::ir::image_input(data).map(ir),
        _ => None,
    }
}

/// Records where a watched run was, and halts it at a count.
struct Recorder {
    seen: Mutex<BTreeSet<(u32, u32)>>,
    count: AtomicU64,
    halt_at: u64,
}

impl Debugger for Recorder {
    fn at(&self, stop: &Stop<'_>) -> Resume {
        let n = self.count.fetch_add(1, Ordering::Relaxed);
        if n >= self.halt_at {
            return Resume::Halt;
        }
        if let Ok(mut seen) = self.seen.lock() {
            seen.insert((stop.function_id().0, stop.pc()));
        }
        Resume::Go
    }
}

/// What one program held and executed, by key.
#[derive(Default)]
struct Measure {
    present: BTreeSet<String>,
    present_compiled: BTreeSet<String>,
    executed: BTreeSet<String>,
    executed_compiled: BTreeSet<String>,
    instructions: u64,
    watched: u64,
}

fn measure(m: Measured) -> Option<Measure> {
    let program = Arc::clone(&m.program);
    cove_ir::verify(&program).ok()?;
    // What the native tier compiles, eagerly, as the differential's native
    // modes compile it. A host without the tier compiles nothing.
    let compiled: BTreeSet<u32> = match PreparedProgram::new(Arc::clone(&program)).with_native() {
        Ok(native) => {
            let refused: BTreeSet<u32> = native
                .native()
                .map(|n| n.refusals().iter().map(|r| r.id.0).collect())
                .unwrap_or_default();
            program
                .functions
                .iter()
                .enumerate()
                .filter(|(i, f)| !f.is_stub() && !refused.contains(&(*i as u32)))
                .map(|(i, _)| i as u32)
                .collect()
        }
        Err(_) => BTreeSet::new(),
    };
    let mut out = Measure::default();
    for (i, function) in program.functions.iter().enumerate() {
        if function.is_stub() {
            continue;
        }
        for inst in &function.code {
            out.present.insert(key(inst));
            if compiled.contains(&(i as u32)) {
                out.present_compiled.insert(key(inst));
            }
        }
    }

    // The unwatched run, for its count.
    let (hosts, _, _) = differential::hosts(m.limits.clone());
    let hosts = Arc::new(hosts);
    let runtime = Arc::new(Runtime::new(
        Arc::clone(&m.checked),
        Arc::clone(&m.sources),
        Arc::clone(&hosts),
    ));
    let mut vm = OwnedVm::with_heap_words(
        runtime,
        hosts,
        PreparedProgram::new(Arc::clone(&program)),
        HEAP_WORDS,
    );
    let _ = enter_owned(&mut vm, &m.entry);
    out.instructions = vm.instructions();
    drop(vm);

    // The watched run, halted where the unwatched one ended.
    let watched = Limits {
        deadline: m.limits.deadline.map(|d| d * WATCH_SLOWDOWN),
        ..m.limits
    };
    let (hosts, _, _) = differential::hosts(watched);
    let hosts = Arc::new(hosts);
    let runtime = Runtime::new(
        Arc::clone(&m.checked),
        Arc::clone(&m.sources),
        Arc::clone(&hosts),
    );
    let recorder = Recorder {
        seen: Mutex::new(BTreeSet::new()),
        count: AtomicU64::new(0),
        halt_at: out.instructions.max(1),
    };
    {
        let mut vm = Vm::debugged(&runtime, &hosts, &program, &recorder);
        let _ = enter(&mut vm, &m.entry);
    }
    out.watched = recorder.count.load(Ordering::Relaxed).min(recorder.halt_at);
    // The entry's own frame is the outermost, and the outermost frame always
    // runs on the encoded tier: what it executed is not compiled code's.
    let (module, name) = match &m.entry {
        Entry::Main => (MODULE, ENTRY),
        Entry::Lowered { module, name, .. } => (&**module, &**name),
    };
    let outermost = program.function_named(module, name).map(|id| id.0);
    let seen = recorder.seen.into_inner().unwrap_or_default();
    for (function, pc) in seen {
        let Some(f) = program.functions.get(function as usize) else {
            continue;
        };
        let Some(inst) = f.code.get(pc as usize) else {
            continue;
        };
        let k = key(inst);
        if compiled.contains(&function) && Some(function) != outermost {
            out.executed_compiled.insert(k.clone());
        }
        out.executed.insert(k);
    }
    Some(out)
}

fn enter_owned(vm: &mut OwnedVm, entry: &Entry) -> Result<Value, cove_runtime::RuntimeError> {
    match entry {
        Entry::Main => vm.run_entry(MODULE, ENTRY, Vec::<Rc<str>>::new()),
        Entry::Lowered { module, name, args } => vm.invoke_lowered(
            module,
            name,
            args.iter().map(crate::ir::Arg::value).collect(),
        ),
    }
}

fn enter(vm: &mut Vm<'_>, entry: &Entry) -> Result<Value, cove_runtime::RuntimeError> {
    match entry {
        Entry::Main => vm.run_entry(MODULE, ENTRY, Vec::<Rc<str>>::new()),
        Entry::Lowered { module, name, args } => vm.invoke_lowered(
            module,
            name,
            args.iter().map(crate::ir::Arg::value).collect(),
        ),
    }
}

/// Measures every input under `paths` as `target` would run it, and counts
/// per key how many programs held and executed it. Inputs that do not make a
/// program, or whose program does not verify, are counted and not measured;
/// a panic is counted as a crash.
///
/// # Errors
///
/// If `target` is not one of [`TARGETS`], or a path cannot be read.
pub fn replay(target: &str, paths: &[PathBuf]) -> Result<Stats, String> {
    if !TARGETS.contains(&target) {
        return Err(format!(
            "no coverage for `{target}`: {}",
            TARGETS.join(", ")
        ));
    }
    let files = crate::stats::files_under(paths)?;
    let mut stats = Stats::new();
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    for file in files {
        let bytes = std::fs::read(&file).map_err(|why| format!("{}: {why}", file.display()))?;
        stats.add(&["inputs"], 1);
        let target = target.to_string();
        let measured = std::panic::catch_unwind(move || {
            on_stack(move || subject(&target, &bytes).map(measure))
        });
        match measured {
            Err(_) => stats.add(&["crashes"], 1),
            Ok(None) => stats.add(&["programs", "none"], 1),
            Ok(Some(None)) => stats.add(&["programs", "refused"], 1),
            Ok(Some(Some(m))) => {
                stats.add(&["programs", "measured"], 1);
                if m.watched < m.instructions {
                    stats.add(&["programs", "watched_short"], 1);
                }
                stats.add(&["instructions", "run"], m.instructions);
                stats.add(&["instructions", "watched"], m.watched);
                for (set, name) in [
                    (&m.present, "present"),
                    (&m.present_compiled, "present_compiled"),
                    (&m.executed, "executed"),
                    (&m.executed_compiled, "executed_compiled"),
                ] {
                    for k in set {
                        stats.add(&["coverage", k, name], 1);
                    }
                }
            }
        }
    }
    std::panic::set_hook(hook);
    Ok(stats)
}

/// The counts [`replay`] made, as a table a person reads: one row a key,
/// ordered by how many programs held it, with the keys present and never
/// executed listed last.
pub fn summary(stats: &Stats) -> String {
    use std::fmt::Write as _;
    let g = |path: &[&str]| stats.get(path);
    let mut out = format!(
        "{} inputs: {} measured, {} refused by verify, {} made no program, {} crashed; {} of \
         the measured were watched for fewer instructions than they ran\n",
        g(&["inputs"]),
        g(&["programs", "measured"]),
        g(&["programs", "refused"]),
        g(&["programs", "none"]),
        g(&["crashes"]),
        g(&["programs", "watched_short"]),
    );
    let mut rows: BTreeMap<String, [u64; 4]> = BTreeMap::new();
    for k in stats.groups(&["coverage"]) {
        let c = |name: &str| stats.get(&["coverage", &k, name]);
        rows.insert(
            k.clone(),
            [
                c("present"),
                c("executed"),
                c("present_compiled"),
                c("executed_compiled"),
            ],
        );
    }
    let _ = writeln!(
        out,
        "  {:<28} {:>8} {:>8} {:>10} {:>10}",
        "key", "present", "executed", "in native", "ran there"
    );
    let mut ordered: Vec<_> = rows.iter().collect();
    ordered.sort_by_key(|(k, c)| {
        (
            std::cmp::Reverse(c[1]),
            std::cmp::Reverse(c[0]),
            (*k).clone(),
        )
    });
    for (k, c) in &ordered {
        let _ = writeln!(
            out,
            "  {:<28} {:>8} {:>8} {:>10} {:>10}",
            k, c[0], c[1], c[2], c[3]
        );
    }
    let never: Vec<_> = ordered
        .iter()
        .filter(|(_, c)| c[0] > 0 && c[1] == 0)
        .map(|(k, _)| k.as_str())
        .collect();
    let _ = writeln!(
        out,
        "present in a verified program and never executed ({}): {}",
        never.len(),
        never.join(", ")
    );
    out
}

/// The mode the watched runs stand for.
pub const WATCHED_MODE: Mode = Mode::Vm;

#[cfg(test)]
mod tests {
    use super::*;

    /// An instruction after a trap is present and not executed; one before
    /// it is both.
    #[test]
    fn present_is_not_executed() {
        let source = "use console.println\n\nfn f(a: Int, b: Int) -> Int {\n  a * b\n}\n\nfn g(a: Int, b: Int) -> Int {\n  a.floorDiv(b)\n}\n\n/// m\nexport fn main() -> Result<Unit, Error> {\n  let x = 9223372036854775807 + f(2, 3)\n  println(\"{g(x, 7)}\")?\n  Ok(())\n}\n";
        let m = on_stack(move || subject("differential_source", source.as_bytes()).map(measure))
            .expect("the source lowers")
            .expect("it verifies");
        assert!(m.instructions > 0);
        assert_eq!(
            m.watched, m.instructions,
            "the watched run saw what the run ran"
        );
        assert!(m.executed.contains("Arith.Int.Mul"), "{:?}", m.executed);
        assert!(m.executed.contains("Arith.Int.Add"), "{:?}", m.executed);
        let after: Vec<_> = m.present.difference(&m.executed).collect();
        assert!(
            !after.is_empty(),
            "what follows the trap is present and not run"
        );
        assert!(m.executed.len() < m.present.len());
    }
}
