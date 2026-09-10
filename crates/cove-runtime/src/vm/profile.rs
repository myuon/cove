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
//! same run unprofiled. That distorts nothing the report says, because the
//! report is in instructions and not in seconds.
//!
//! # What the numbers mean, and what they do not
//!
//! **Instructions executed.** A `call-builtin` that allocates a string, a
//! `call` that pushes a frame, and an `add.int` are one instruction each — so
//! this answers *which Cove code is this run made of* and not *which
//! instruction is slow*. A native profiler — `samply`, `sample(1)`, `perf` —
//! answers the second one, about the machine rather than about the program.
//!
//! # One task
//!
//! A debugger is installed on a `Vm`, and a spawned task gets a machine of
//! its own. So a run that spawns profiles the task the profiler was installed
//! on. [`crate::Vm::instructions`] is counted the same way and for the same
//! reason.

use std::collections::HashMap;
use std::sync::Mutex;

use cove_ir::program::FunctionId;

use crate::vm::debug::{Debugger, Resume, Stop};

/// Counts what a run executes, by the instruction that executed.
#[derive(Debug, Default)]
pub struct Profiler {
    /// How often each instruction ran, keyed by the function and the program
    /// counter — the pair [`cove_ir::print::one`] renders and the debugger
    /// disassembles.
    ///
    /// A `Mutex` because [`Debugger`] is `Sync` and takes `&self`: a run that
    /// spawns has one machine per task, and two of them may hold the same
    /// profiler. It is taken once per instruction, which is what makes a
    /// profiled run slow and is the same order of cost as the stop that
    /// reached it.
    at: Mutex<HashMap<(FunctionId, u32), u64>>,
}

impl Profiler {
    /// A profiler with nothing recorded yet.
    pub fn new() -> Profiler {
        Profiler::default()
    }

    /// How many instructions were counted.
    pub fn counted(&self) -> u64 {
        self.at.lock().expect("a lock").values().sum()
    }

    /// Every instruction that ran, and how often, hottest first.
    ///
    /// Ties are broken by the instruction's own identity, so two runs of one
    /// program report the same order: a profile that reordered its own ties
    /// would make an unchanged program look changed.
    pub fn hottest(&self) -> Vec<((FunctionId, u32), u64)> {
        let held = self.at.lock().expect("a lock");
        let mut rows: Vec<((FunctionId, u32), u64)> =
            held.iter().map(|(at, n)| (*at, *n)).collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        rows
    }

    /// Every function that ran, and how many instructions of it did, hottest
    /// first.
    pub fn by_function(&self) -> Vec<(FunctionId, u64)> {
        let held = self.at.lock().expect("a lock");
        let mut per: HashMap<FunctionId, u64> = HashMap::new();
        for ((function, _), n) in held.iter() {
            *per.entry(*function).or_insert(0) += n;
        }
        let mut rows: Vec<(FunctionId, u64)> = per.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        rows
    }
}

impl Debugger for Profiler {
    /// Records the instruction about to run, and lets it run.
    fn at(&self, stop: &Stop<'_>) -> Resume {
        let mut held = self.at.lock().expect("a lock");
        *held.entry((stop.function_id(), stop.pc())).or_insert(0) += 1;
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
            assert!(pair[0].1 >= pair[1].1, "sorted by how often");
        }
        assert!(
            hottest[0].1 >= 100,
            "the top instruction is one of the hundred turns, not the prologue"
        );
    }

    /// One function here, and all of the instructions are its.
    #[test]
    fn a_function_holds_the_instructions_of_its_own_program_counters() {
        let world = World::new(COUNTED);
        let profiler = Profiler::new();
        let mut vm = world.watched(&profiler);
        vm.run_entry("m", "main", Vec::new()).expect("it answers");

        let per = profiler.by_function();
        assert_eq!(per.len(), 1, "one function ran");
        assert_eq!(per[0].1, profiler.counted());
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
