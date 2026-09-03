//! What compiling once and invoking many times costs, counted.
//!
//! Issue #109's gate asks for a compile-once/invoke-many embedding measured on
//! the compiled backend, and for Host conversion measured beside it. This is
//! that measurement. It is a binary rather than a `#[test]` for one reason: it
//! installs a counting [`std::alloc::GlobalAlloc`], and a count taken while
//! `cargo test` runs other cases on other threads would be a count of the
//! test harness. Nothing here runs under `cargo test`, and the counts the
//! README quotes were taken by running it.
//!
//! ```text
//! cargo run --release -p cove-rules --bin cove-rules-measure -- 2000
//! ```
//!
//! # What is a count and what is a time
//!
//! The allocation counts and the instruction counts are exact and are the same
//! on every machine: they come from a counter incremented on the path, not
//! from a sampler. The wall times are medians over the turns of one process
//! and are worth what any wall time taken on a shared machine is worth, which
//! is the ratios between rows measured in the same run and not the absolute
//! figures. `examples/rules/README.md` says which of its numbers is which.
//!
//! # The rows, and what each isolates
//!
//! Each entry is a control on the one below it.
//!
//! - `rules.floor` does nothing, so it is what an invocation costs before the
//!   program does anything: finding the entry, entering the frame, and
//!   answering.
//! - `rules.decideSample` runs the whole rule catalog over a pull request the
//!   package itself holds, and makes no Host API call at all.
//! - `rules.embedded.evaluate` runs the same catalog over a pull request the
//!   *host* built and handed over as an argument, so what it adds to
//!   `decideSample` is the argument and nothing else.
//! - `rules.embedded.pullOnly` makes one Host API call and converts what comes
//!   back into the package's own struct, and weighs nothing.
//! - `rules.embedded.decideRequest` does both, and reports the decision back
//!   through a second call.
//!
//! The middle three are the point. `evaluate` and `decideRequest` reach the
//! same decision over the same pull request, one with it as an argument and
//! one with it fetched across the Host API boundary, so the difference between
//! them is what the boundary was costing an embedding that had no other way in
//! (issue #150).
//!
//! The Rust side is measured on its own beside them: `PullRequest::to_policy`
//! builds the value an invocation hands over — the same ten fields
//! `to_cove` builds for the boundary — and `Decision::from_cove` reads the one
//! that comes back.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cove_rules::{
    embedding, embedding_without_trace, package_root, Decision, PullRequest, Reviews, RulePackage,
    Session, REVIEWS,
};
use cove_runtime::Limits;

// --------------------------------------------------------------- the counter

/// Allocations made since the process started.
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);
/// Bytes those allocations asked for.
static BYTES: AtomicU64 = AtomicU64::new(0);

/// The system allocator, counting what goes through it.
///
/// A reallocation is counted as one allocation of the new size, because that
/// is what it costs: a growing `Vec` that doubles four times allocated four
/// times. A free is not counted at all, since what this is measuring is
/// pressure rather than residency.
struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// What one measured stretch of work cost.
#[derive(Clone, Copy, Default)]
struct Cost {
    elapsed: Duration,
    allocations: u64,
    bytes: u64,
}

/// Runs `work` once, and says what it cost.
fn cost<T>(work: impl FnOnce() -> T) -> (T, Cost) {
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    let bytes = BYTES.load(Ordering::Relaxed);
    let started = Instant::now();
    let answer = work();
    let elapsed = started.elapsed();
    (
        answer,
        Cost {
            elapsed,
            allocations: ALLOCATIONS.load(Ordering::Relaxed) - allocations,
            bytes: BYTES.load(Ordering::Relaxed) - bytes,
        },
    )
}

/// One row of the report: what a thing cost, divided by how many times it was
/// done.
struct Row {
    what: &'static str,
    turns: u64,
    cost: Cost,
    instructions: Option<u64>,
}

impl Row {
    fn print(&self) {
        let per = self.cost.elapsed.as_nanos() as f64 / self.turns as f64;
        let allocations = self.cost.allocations as f64 / self.turns as f64;
        let bytes = self.cost.bytes as f64 / self.turns as f64;
        let instructions = match self.instructions {
            Some(count) => format!("{:>12.1}", count as f64 / self.turns as f64),
            None => format!("{:>12}", "-"),
        };
        println!(
            "{:<34} {:>12.1} {:>12.2} {:>12.1} {instructions}",
            self.what, per, allocations, bytes
        );
    }
}

/// Which way into the program a row is measuring.
///
/// The two are what issue #150 was about, and measuring them against each
/// other is why both are here. A row that builds its argument builds it inside
/// the loop, because the boundary row pays for the same conversion inside
/// `reviews.pull` and a comparison that charged one and not the other would be
/// measuring the bookkeeping.
#[derive(Clone, Copy)]
enum Way {
    /// `run_entry`, with one process argument — the only way in there used to
    /// be.
    Command(&'static str),
    /// `invoke`, with the pull request the host built.
    Direct,
}

impl Way {
    fn call(self, session: &mut Session<'_>, module: &str, entry: &str, pr: &PullRequest) {
        match self {
            Way::Command(argument) => {
                session
                    .run(module, entry, &[argument])
                    .expect("every invocation succeeds");
            }
            Way::Direct => {
                session
                    .invoke(module, entry, vec![pr.to_policy()])
                    .expect("every invocation succeeds");
            }
        }
    }
}

/// Prints the header the rows line up under.
fn header(title: &str) {
    println!();
    println!("{title}");
    println!(
        "{:<34} {:>12} {:>12} {:>12} {:>12}",
        "", "ns/turn", "allocs/turn", "bytes/turn", "insts/turn"
    );
}

// ----------------------------------------------------------------- the study

fn main() {
    let turns: u64 = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(1000);

    cove_runtime::on_cove_stack(move || study(turns)).expect("a thread to run Cove on");
}

/// The whole measurement, on the stack the runtime sized.
fn study(turns: u64) {
    // ---------------------------------------------------------- paid once
    let (package, load) =
        cost(|| RulePackage::load(&package_root(), REVIEWS).expect("the rule package checks"));
    let detail = package.cost();

    header(&format!(
        "paid once, over {} file(s) in {} module(s)",
        detail.files, detail.modules
    ));
    Row {
        what: "load: read, parse, and check",
        turns: 1,
        cost: load,
        instructions: None,
    }
    .print();
    println!(
        "{:<34} {:>12.1} {:>12} {:>12} {:>12}",
        "  of which: read from disk",
        detail.read.as_nanos() as f64,
        "-",
        "-",
        "-"
    );
    println!(
        "{:<34} {:>12.1} {:>12} {:>12} {:>12}",
        "  of which: parse",
        detail.parse.as_nanos() as f64,
        "-",
        "-",
        "-"
    );
    println!(
        "{:<34} {:>12.1} {:>12} {:>12} {:>12}",
        "  of which: resolve and check",
        detail.check.as_nanos() as f64,
        "-",
        "-",
        "-"
    );

    // Lowering is measured over twenty turns rather than one, because the
    // first lowering a process performs is cold and the cost an embedder
    // pays for a second entry is the warm one. Loading above is measured
    // once, because loading once is what it is for.
    const LOWERINGS: u64 = 20;
    for (module, entry) in [
        ("rules", "floor"),
        ("rules", "decideSample"),
        ("rules.embedded", "evaluate"),
        ("rules.embedded", "pullOnly"),
        ("rules.embedded", "decideRequest"),
    ] {
        let lowering = package
            .lower(module, entry)
            .unwrap_or_else(|why| panic!("{module}.{entry} lowers: {why}"));
        let (_, lowered) = cost(|| {
            for _ in 0..LOWERINGS {
                package.lower(module, entry).expect("the entry lowers");
            }
        });
        println!(
            "{:<34} {:>12.1} {:>12.2} {:>12.1} {:>12}",
            format!("lower {module}.{entry} ({} fns)", lowering.functions),
            lowered.elapsed.as_nanos() as f64 / LOWERINGS as f64,
            lowered.allocations as f64 / LOWERINGS as f64,
            lowered.bytes as f64 / LOWERINGS as f64,
            "-"
        );
    }

    // ------------------------------------------------- paid per invocation
    let subject: PullRequest = cove_rules::samples()
        .remove("req-2")
        .expect("the sample exists");
    header("paid per invocation, one Lvm serving all of them");
    for (what, module, entry, way, grants, trace) in [
        (
            "floor: an entry that does nothing",
            "rules",
            "floor",
            Way::Command("0"),
            &[][..],
            false,
        ),
        (
            "decide, no host call",
            "rules",
            "decideSample",
            Way::Command("1"),
            &[][..],
            false,
        ),
        (
            "evaluate: the request as argument",
            "rules.embedded",
            "evaluate",
            Way::Direct,
            &[][..],
            false,
        ),
        (
            "pull only: one host call",
            "rules.embedded",
            "pullOnly",
            Way::Command("req-2"),
            &["reviews"][..],
            false,
        ),
        (
            "decide, two host calls",
            "rules.embedded",
            "decideRequest",
            Way::Command("req-2"),
            &["reviews"][..],
            false,
        ),
        (
            "evaluate, traced",
            "rules.embedded",
            "evaluate",
            Way::Direct,
            &[][..],
            true,
        ),
        (
            "pull only, traced",
            "rules.embedded",
            "pullOnly",
            Way::Command("req-2"),
            &["reviews"][..],
            true,
        ),
        (
            "decide, two host calls, traced",
            "rules.embedded",
            "decideRequest",
            Way::Command("req-2"),
            &["reviews"][..],
            true,
        ),
    ] {
        let lowering = package.lower(module, entry).expect("the entry lowers");
        let reviews = Reviews::new(cove_rules::samples());
        let embed = if trace {
            embedding(reviews, grants, Limits::default())
        } else {
            embedding_without_trace(reviews, grants, Limits::default())
        };
        let (instructions, measured) = package.serve(
            Arc::clone(&embed.hosts),
            Some(&lowering),
            |session: &mut Session<'_>| {
                // One turn outside the measurement, so that whatever a first
                // invocation warms is warm for all of them.
                way.call(session, module, entry, &subject);
                let before = session.instructions().unwrap_or_default();
                let (_, measured) = cost(|| {
                    for _ in 0..turns {
                        way.call(session, module, entry, &subject);
                    }
                });
                (
                    session.instructions().unwrap_or_default() - before,
                    measured,
                )
            },
        );
        Row {
            what,
            turns,
            cost: measured,
            instructions: Some(instructions),
        }
        .print();
    }

    // ------------------------------------------- what reuse is worth
    header("the same decision, with the session rebuilt each time");
    let lowering = package
        .lower("rules.embedded", "decideRequest")
        .expect("the entry lowers");
    let embed = embedding_without_trace(
        Reviews::new(cove_rules::samples()),
        &["reviews"],
        Limits::default(),
    );
    let (_, rebuilt) = cost(|| {
        for _ in 0..turns {
            package.serve(Arc::clone(&embed.hosts), Some(&lowering), |session| {
                session
                    .run("rules.embedded", "decideRequest", &["req-2"])
                    .expect("every invocation succeeds");
            });
        }
    });
    Row {
        what: "decide, a new Runtime and Lvm each",
        turns,
        cost: rebuilt,
        instructions: None,
    }
    .print();

    let (_, interpreted) = cost(|| {
        package.serve(Arc::clone(&embed.hosts), None, |session| {
            for _ in 0..turns {
                session
                    .run("rules.embedded", "decideRequest", &["req-2"])
                    .expect("every invocation succeeds");
            }
        });
    });
    Row {
        what: "decide, on the interpreter",
        turns,
        cost: interpreted,
        instructions: None,
    }
    .print();

    // ------------------------------------------------ the Rust side alone
    header("the conversion, measured on the Rust side alone");
    let (_, into_cove) = cost(|| {
        for _ in 0..turns {
            std::hint::black_box(subject.to_cove());
        }
    });
    Row {
        what: "PullRequest::to_cove",
        turns,
        cost: into_cove,
        instructions: None,
    }
    .print();

    // The same ten fields under the other name, which is what an invocation
    // hands over. It is here rather than assumed equal to the row above
    // because "the same work" is a claim and a claim is worth a row.
    let (_, into_policy) = cost(|| {
        for _ in 0..turns {
            std::hint::black_box(subject.to_policy());
        }
    });
    Row {
        what: "PullRequest::to_policy",
        turns,
        cost: into_policy,
        instructions: None,
    }
    .print();

    let answer = package.serve(Arc::clone(&embed.hosts), Some(&lowering), |session| {
        session
            .run("rules.embedded", "decideRequest", &["req-2"])
            .expect("the invocation succeeds")
    });
    let (_, out_of_cove) = cost(|| {
        for _ in 0..turns {
            std::hint::black_box(Decision::from_cove(&answer).expect("the answer decodes"));
        }
    });
    Row {
        what: "Decision::from_cove",
        turns,
        cost: out_of_cove,
        instructions: None,
    }
    .print();

    println!();
    println!("{turns} turn(s) a row.");
}
