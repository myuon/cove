//! Where a run spends its instructions.
//!
//! A profiler is a **debugger that never stops**. That is the whole design,
//! and everything good about it follows from reusing a seam this crate
//! already has rather than opening a second one.
//!
//! [`Debugger::at`] is called before every instruction while one is
//! installed, and a [`Stop`] answers which function and which program counter
//! that is. A profiler records the pair and answers [`Resume::Go`]. Nothing
//! is added to `Machine`, nothing is added to the dispatch loop, and a run
//! that asks for no profile is **byte for byte the run it was**.
//!
//! # Why not sample at the safepoint
//!
//! It was written that way first: a `Profile` on the machine, read at the
//! safepoint the loop already stops at, one sample per
//! [`SAFEPOINT_STRIDE`](crate::vm::exec::SAFEPOINT_STRIDE) instructions. It
//! measured **2.9% on `arith` with no profile installed** — an `Option` field
//! nothing read, paid for by every run through `Machine`'s size, and boxing
//! it to one word did not move the number.
//!
//! That is a cost `docs/PHILOSOPHY.md` would let a profiler buy: it is not a
//! multiple and not a change of class, and *"a small measured slowdown can buy
//! those qualities"*. This is the better shape anyway, on two counts that have
//! nothing to do with the 2.9%. It costs **nothing** when off rather than a
//! little, and it **counts** rather than samples — every instruction, not one
//! in a thousand — so a function that runs once is in the report and a hot
//! instruction's share is exact.
//!
//! # What it costs while it is on
//!
//! What a debugger costs: the machine asks before every instruction rather
//! than every thousandth, so a profiled run is several times slower than the
//! same run unprofiled.
//!
//! # What the numbers mean, and what they do not
//!
//! **Instructions executed** is the count, and it is the number to trust
//! absolutely: every instruction is counted, not one in a thousand, so a
//! function that ran once is in the report and a share is exact.
//!
//! It is also, alone, **not enough**, and that is why the three figures
//! beside it are here. A `call-builtin` that allocates a string, a `call`
//! that pushes a frame and an `add.int` are one instruction each. Replacing
//! a byte loop in `examples/covefmt` with one `String.contains` cut the run
//! from 751.1 M instructions to 722.1 M and made it **slower**, and nothing
//! a count-only profile said would have shown that.
//!
//! So a row also carries:
//!
//! - **nanoseconds**, measured as the interval between the stop before an
//!   instruction and the stop after it;
//! - **words** and **allocations**, measured as what the heap handed out
//!   across that same interval.
//!
//! The heap figures are exact: a difference of two counters is what happened
//! in between, whatever it took to happen.
//!
//! The time is **not** exact, and reading it as though it were is the mistake
//! this paragraph exists to stop. The interval holds the instruction, the
//! dispatch that reached it, and two `Instant::now()` calls — around twenty
//! nanoseconds of floor that every instruction pays equally, where the
//! instruction itself may be one nanosecond or five hundred. So:
//!
//! - **a ratio of two rows is worth reading**, because the floor is in both;
//! - **nanoseconds per instruction is the number that finds an expensive
//!   opcode**, because the floor is a constant added to it and a builtin call
//!   stands far above the constant;
//! - **an absolute figure is worth nothing**, and the total will not agree
//!   with the run's own wall clock — the run being measured is several times
//!   slower than the run anybody cares about.
//!
//! A native profiler — `samply`, `sample(1)`, `perf` — has none of that floor
//! and none of this attribution: it says which *machine* code the time went
//! to, and this says which *Cove* code. Neither replaces the other.
//!
//! # One task
//!
//! A debugger is installed on a `Vm`, and a spawned task gets a machine of
//! its own. So a run that spawns profiles the task the profiler was installed
//! on. [`crate::Vm::instructions`] is counted the same way and for the same
//! reason.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use cove_ir::program::FunctionId;

use crate::vm::debug::{Debugger, Resume, Stop};

/// What one instruction of a run cost, summed over every time it ran.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    /// How often it ran.
    pub ran: u64,
    /// Nanoseconds spent running it, the profiler's own overhead included.
    ///
    /// See the module documentation: the overhead is a constant per
    /// instruction, so this number is worth reading *against the count* and
    /// not on its own.
    pub nanos: u64,
    /// Words its heap handed out while it ran.
    pub words: u64,
    /// Objects its heap handed out while it ran.
    pub allocations: u64,
}

impl Cost {
    /// Everything in `other` added to this.
    pub fn add(&mut self, other: &Cost) {
        self.ran += other.ran;
        self.nanos += other.nanos;
        self.words += other.words;
        self.allocations += other.allocations;
    }
}

/// What the last stop saw, so that the next one can say what happened in
/// between.
#[derive(Debug, Clone, Copy)]
struct Previous {
    at: (FunctionId, u32),
    when: Instant,
    words: u64,
    allocations: u64,
}

/// Counts what a run executes, by the instruction that executed.
#[derive(Debug, Default)]
pub struct Profiler {
    /// What each instruction cost, keyed by the function and the program
    /// counter — the pair [`cove_ir::print::one`] renders and the debugger
    /// disassembles.
    ///
    /// A `Mutex` because [`Debugger`] is `Sync` and takes `&self`: a run that
    /// spawns has one machine per task, and two of them may hold the same
    /// profiler. It is taken once per instruction, which is what makes a
    /// profiled run slow and is the same order of cost as the stop that
    /// reached it.
    at: Mutex<HashMap<(FunctionId, u32), Cost>>,
    /// The stop before this one, whose instruction is the one that ran.
    ///
    /// A stop happens *before* an instruction, so nothing at the stop knows
    /// what that instruction is about to cost. The stop after it does: the
    /// clock and the heap have both moved by exactly what it did. So every
    /// measurement here is recorded one stop late, and the last instruction
    /// of a run — the `return` out of the entry — is the one instruction no
    /// later stop closes.
    last: Mutex<Option<Previous>>,
}

impl Profiler {
    /// A profiler with nothing recorded yet.
    pub fn new() -> Profiler {
        Profiler::default()
    }

    /// How many instructions were counted.
    pub fn counted(&self) -> u64 {
        self.at
            .lock()
            .expect("a lock")
            .values()
            .map(|c| c.ran)
            .sum()
    }

    /// What the whole run cost, by the four figures a row carries.
    pub fn total(&self) -> Cost {
        let mut all = Cost::default();
        for cost in self.at.lock().expect("a lock").values() {
            all.add(cost);
        }
        all
    }

    /// Every instruction that ran, and how often, hottest first.
    ///
    /// Ties are broken by the instruction's own identity, so two runs of one
    /// program report the same order: a profile that reordered its own ties
    /// would make an unchanged program look changed.
    pub fn hottest(&self) -> Vec<((FunctionId, u32), Cost)> {
        let held = self.at.lock().expect("a lock");
        let mut rows: Vec<((FunctionId, u32), Cost)> =
            held.iter().map(|(at, n)| (*at, *n)).collect();
        rows.sort_by(|a, b| b.1.ran.cmp(&a.1.ran).then(a.0.cmp(&b.0)));
        rows
    }

    /// Every function that ran, and what the instructions of it cost, hottest
    /// first.
    pub fn by_function(&self) -> Vec<(FunctionId, Cost)> {
        let held = self.at.lock().expect("a lock");
        let mut per: HashMap<FunctionId, Cost> = HashMap::new();
        for ((function, _), cost) in held.iter() {
            per.entry(*function).or_default().add(cost);
        }
        let mut rows: Vec<(FunctionId, Cost)> = per.into_iter().collect();
        rows.sort_by(|a, b| b.1.ran.cmp(&a.1.ran).then(a.0.cmp(&b.0)));
        rows
    }

    /// Every instruction that ran and what it cost, in no particular order.
    ///
    /// For a reader that wants to group by something only the program knows —
    /// the opcode an instruction is, the callee a `call` names — which this
    /// crate deliberately does not: a profiler that read the program would
    /// have to be given one, and the two things that want these groupings
    /// already hold it.
    pub fn rows(&self) -> Vec<((FunctionId, u32), Cost)> {
        self.at
            .lock()
            .expect("a lock")
            .iter()
            .map(|(at, cost)| (*at, *cost))
            .collect()
    }
}

impl Debugger for Profiler {
    /// Closes the instruction that just ran and opens the one about to.
    ///
    /// The count belongs to the instruction this stop is *before*; the time
    /// and the heap belong to the one the previous stop was before, because
    /// those are what moved in between.
    ///
    /// The clock is read twice, first thing and last thing, and the bookkeeping
    /// sits between the two reads. So the interval a number is measured over
    /// holds the instruction, the dispatch that reached it and two clock
    /// reads — and **not** this hook's mutex and map, which is the expensive
    /// part and which would otherwise swamp what it is trying to measure. An
    /// `add.int` is a nanosecond or two and the map is a hundred.
    fn at(&self, stop: &Stop<'_>) -> Resume {
        let now = Instant::now();
        let here = (stop.function_id(), stop.pc());
        let words = stop.allocated_words();
        let allocations = stop.allocations();
        let mut held = self.at.lock().expect("a lock");
        held.entry(here).or_default().ran += 1;
        let mut last = self.last.lock().expect("a lock");
        if let Some(before) = *last {
            let cost = held.entry(before.at).or_default();
            cost.nanos += now.saturating_duration_since(before.when).as_nanos() as u64;
            cost.words += words.saturating_sub(before.words);
            cost.allocations += allocations.saturating_sub(before.allocations);
        }
        *last = Some(Previous {
            at: here,
            words,
            allocations,
            when: Instant::now(),
        });
        Resume::Go
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::debug::tests::World;

    /// A loop, so the instruction the loop is made of is counted many times
    /// and the one before it once.
    const COUNTED: &str = "export fn main() -> Int {\n  \
                           var total = 0\n  \
                           var i = 0\n  \
                           while i < 100 {\n    \
                           total = total + i\n    \
                           i = i + 1\n  \
                           }\n  \
                           total\n\
                           }\n";

    /// **The count is the run's own count.**
    ///
    /// A profile that disagreed with `Vm::instructions` would be measuring
    /// something other than the run, and which of the two to believe would be
    /// an open question at every reading. They are the same number because
    /// the profiler is called once per instruction by the same loop that
    /// increments the counter.
    #[test]
    fn every_instruction_the_run_executed_is_counted_once() {
        let world = World::new(COUNTED);
        let profiler = Profiler::new();
        let mut vm = world.watched(&profiler);
        vm.run_entry("m", "main", Vec::new()).expect("it answers");
        assert_eq!(profiler.counted(), vm.instructions());
        assert!(profiler.counted() > 100, "{}", profiler.counted());
    }

    /// The hottest instruction is one the loop runs, and the report is sorted
    /// by how often rather than by where.
    #[test]
    fn the_hottest_instruction_is_one_the_loop_runs() {
        let world = World::new(COUNTED);
        let profiler = Profiler::new();
        let mut vm = world.watched(&profiler);
        vm.run_entry("m", "main", Vec::new()).expect("it answers");

        let hottest = profiler.hottest();
        assert!(hottest.len() > 1, "more than one instruction ran");
        for pair in hottest.windows(2) {
            assert!(pair[0].1.ran >= pair[1].1.ran, "sorted by how often");
        }
        assert!(
            hottest[0].1.ran >= 100,
            "the top instruction is one of the hundred turns, not the prologue"
        );
    }

    /// **Every allocation the run made is attributed to an instruction.**
    ///
    /// The heap figures are a difference of two counters read at two stops,
    /// so what they cannot do is lose one: whatever the instruction between
    /// them did, the counters moved by it. That is the property worth pinning,
    /// because the alternative — a seam inside `Memory::alloc` that knew which
    /// instruction it was serving — is the one this design avoids having.
    #[test]
    fn what_the_heap_handed_out_is_attributed_to_the_instructions_that_ran() {
        let world = World::new(ALLOCATES);
        let profiler = Profiler::new();
        let mut vm = world.watched(&profiler);
        vm.run_entry("m", "main", Vec::new()).expect("it answers");

        let total = profiler.total();
        assert!(
            total.allocations > 0,
            "a program that builds strings allocates: {total:?}"
        );
        assert!(
            total.words >= total.allocations,
            "an object is at least its header: {total:?}"
        );
        // The rows are the whole of it, so their sum is the total.
        let mut summed = Cost::default();
        for (_, cost) in profiler.rows() {
            summed.add(&cost);
        }
        assert_eq!(summed.allocations, total.allocations);
        assert_eq!(summed.words, total.words);
    }

    /// **A `call-builtin` that allocates is dearer than an `add.int`, and the
    /// profile says so.**
    ///
    /// This is the whole reason the timing is here. A count cannot separate
    /// the two — they are one instruction each — and separating them is what
    /// a reader is trying to do when they ask where a run went.
    #[test]
    fn an_instruction_that_allocates_costs_more_than_one_that_adds() {
        let world = World::new(ALLOCATES);
        let profiler = Profiler::new();
        let mut vm = world.watched(&profiler);
        vm.run_entry("m", "main", Vec::new()).expect("it answers");

        let mut dearest = 0.0_f64;
        let mut cheapest = f64::MAX;
        for ((_, _), cost) in profiler.rows() {
            if cost.ran < 10 {
                continue;
            }
            let each = cost.nanos as f64 / cost.ran as f64;
            if cost.allocations > 0 {
                dearest = dearest.max(each);
            } else {
                cheapest = cheapest.min(each);
            }
        }
        assert!(
            dearest > cheapest,
            "an allocating instruction averaged {dearest} ns and the cheapest \
             non-allocating one {cheapest}"
        );
    }

    /// A loop that builds a string a turn, so that some instructions allocate
    /// and the ones around them do not.
    const ALLOCATES: &str = "export fn main() -> Int {\n  \
                             var total = 0\n  \
                             var i = 0\n  \
                             while i < 200 {\n    \
                             let text = \"n={i}\"\n    \
                             total = total + text.length()\n    \
                             i = i + 1\n  \
                             }\n  \
                             total\n\
                             }\n";

    /// One function here, and all of the instructions are its.
    #[test]
    fn a_function_holds_the_instructions_of_its_own_program_counters() {
        let world = World::new(COUNTED);
        let profiler = Profiler::new();
        let mut vm = world.watched(&profiler);
        vm.run_entry("m", "main", Vec::new()).expect("it answers");

        let per = profiler.by_function();
        assert_eq!(per.len(), 1, "one function ran");
        assert_eq!(per[0].1.ran, profiler.counted());
    }

    /// A profiler that watched nothing counts nothing, which is what a report
    /// over a run that never started has to say.
    #[test]
    fn a_profiler_that_watched_nothing_counts_nothing() {
        let profiler = Profiler::new();
        assert_eq!(profiler.counted(), 0);
        assert!(profiler.hottest().is_empty());
        assert!(profiler.by_function().is_empty());
    }
}
