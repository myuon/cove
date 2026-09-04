//! Stopping the machine, and looking at it.
//!
//! Issue #241's first half: the machine side of a debugger, with no session,
//! no commands and no policy in it. What is here is a place to stand — the
//! machine asks a [`Debugger`] before every instruction while one is
//! installed, and honours [`Resume::Go`] or [`Resume::Halt`] — and a way to
//! look, which is a set of owned snapshots this module builds and hands out.
//!
//! # The machine calls the debugger, and never the other way round
//!
//! There is no "suspended machine" here to hold, and there could not be. The
//! dispatch loop runs inside [`Machine::drive`]'s `std::thread::scope`, and
//! it holds a `&'s Scope<'s, 'a>` — the borrow a `spawn` starts its children
//! in — which by construction cannot outlive that call. A handle to a paused
//! machine would be a value carrying that borrow out of the scope that
//! created it, which is exactly what the scope exists to refuse.
//!
//! So the call is inverted. The machine reaches a stop, builds a [`Stop`]
//! that borrows it for the length of one call, and asks. Everything a
//! debugger wants to keep, it copies out of that call — which is why every
//! view below is owned, and why none of them borrows the machine.
//!
//! # A stop costs the loop nothing when nobody is stopping
//!
//! The loop's one per-instruction comparison already existed, as
//! `instructions % SAFEPOINT_STRIDE == 0`. `Machine::next_question` folds
//! the debugger's question into that same comparison rather than adding a
//! second: with no debugger installed the next check is the next multiple of
//! [`SAFEPOINT_STRIDE`](crate::vm::exec::SAFEPOINT_STRIDE) and the loop does
//! what it always did; with one installed it is the very next instruction.
//! `docs/VM_ARCHITECTURE.md` measured what a second per-instruction branch
//! costs — 2.4% on `arith` for a `bool` guarding the counter — and that is
//! the price this arrangement does not pay.
//!
//! It was measured rather than argued, by `scripts/vm-time.sh`: fifteen runs
//! of one benchmark, medians of `execute=`, three interleaved rounds, every
//! build from the same working tree so that nothing but the named line
//! differs. On `arith`, against a base of that tree with this change removed:
//!
//! | build | median |
//! | --- | ---: |
//! | base | 80.7 ms |
//! | the fold, with the question written out in the loop | 85.0 ms |
//! | the fold, with the question in `Machine::ask` | 79.4 ms |
//! | base plus the two fields, loop untouched (a control) | 79.6 ms |
//!
//! Two things came out of it, and only the second was expected. **The
//! comparison is free**: the same tree with the loop's condition put back to
//! `instructions % SAFEPOINT_STRIDE == 0` measured 82.9 ms against the fold's
//! 82.7 ms, indistinguishable. **Where the question is written is not**:
//! building the [`Stop`] and making the indirect call inside the dispatch
//! body cost 4.3%, which is more than the branch this whole arrangement was
//! shaped to avoid, and moving it behind `#[inline(never)]` recovered all of
//! it. The control is the reason that is reported as a code-layout effect
//! rather than as work: adding the same two fields and reading neither of
//! them moved `arith` by 1.1% on its own. The shipped shape measures 1.5%
//! *below* the base on `arith` and 1.6% below it on `field`, which is to say
//! inside that band and not outside it.
//!
//! The safepoint's own schedule does not move by one instruction either.
//! [ADR 0040](../../../../docs/adr/0040-a-bound-outlives-its-backend.md)
//! states every stop mode's bound in multiples of the stride, and
//! `crates/cove-runtime/tests/responsiveness.rs` measures each of them.
//! `the_safepoint_fires_at_the_same_counts_as_it_did_before`, below, is that
//! rule pinned at the instruction rather than left to the bounds.
//!
//! # What may be handed out
//!
//! `crates/cove-runtime/tests/representation_is_private.rs` is the arbiter,
//! and it decided the shape of everything below: no slot, no layout id, no
//! frame base, no word of VM storage leaves in a public signature. A frame
//! word is named by its *position* ([`Local::at`]), a family by its *name*
//! ([`Object::name`]), and a value by what it *renders as* — a `String` this
//! crate produced, not a piece of the representation that produced it.
//!
//! One raw word does cross, in one direction only: [`Stop::object`] takes a
//! word a [`Word`] view already showed and answers what it names, for the VM
//! development the sketch asks for. It is not a handle in the sense
//! ADR 0031 forbids — nothing roots it, nothing stores it, it is not valid
//! after the run, and a word that names no object answers `None` rather than
//! misbehaving.

use cove_diag::Span;
use cove_ir::{print, FunctionId, Pc};

use crate::error::RuntimeError;
use crate::trace::RunOutcome;
use crate::vm::exec::Machine;
use crate::vm::render;

/// What a debugger says when the machine asks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resume {
    /// Run the instruction the stop is at, and go on.
    Go,
    /// End the run here, as a stop and not as a failure of the program.
    Halt,
}

/// Something that watches a run, one instruction at a time.
///
/// The machine's whole contract with it is [`Debugger::at`]: while a debugger
/// is installed the machine asks before every instruction, and honours the
/// answer. **Every policy is the implementor's** — a breakpoint is an `at`
/// that answers `Go` until the pc is one it is looking for, a step is one
/// that counts, a `finish` is one that watches the depth — because a policy
/// in the loop is a policy that cannot be changed without touching the loop.
///
/// `Send + Sync` because a spawned task's machine is handed the same
/// reference and asks it from that task's own thread. A debugger that keeps
/// state therefore keeps it behind a lock or an atomic, as any two threads
/// sharing anything must.
pub trait Debugger: Send + Sync {
    /// Called before each instruction while this debugger is installed.
    fn at(&self, stop: &Stop<'_>) -> Resume;
}

/// One instruction's worth of standing still.
///
/// It borrows the machine for the length of the call and nothing longer,
/// which is the whole of why the call is inverted. Every method answers with
/// an owned snapshot, so a debugger keeps what it asks for and holds nothing
/// of the machine.
///
/// The frame's `pc` is truthful here: the dispatch loop syncs it before
/// asking, which it does not do between instructions, and a view built
/// anywhere else would name the instruction after the one being stopped at.
pub struct Stop<'m> {
    machine: &'m Machine<'m>,
    function: FunctionId,
    pc: usize,
}

impl<'m> Stop<'m> {
    /// The stop the dispatch loop is at, with `pc` already synced.
    pub(crate) fn new(machine: &'m Machine<'m>, function: FunctionId, pc: usize) -> Stop<'m> {
        Stop {
            machine,
            function,
            pc,
        }
    }

    /// How many instructions this task has run, this one included.
    ///
    /// The count [`crate::Vm::instructions`] reports, read at the moment the
    /// instruction about to run was counted.
    pub fn instructions(&self) -> u64 {
        self.machine.instructions()
    }

    /// `module.name` of the function this stop is in.
    pub fn function(&self) -> String {
        self.machine.program().function(self.function).qualified()
    }

    /// Which instruction of that function is about to run.
    pub fn pc(&self) -> u32 {
        self.pc as u32
    }

    /// Where that instruction was written.
    pub fn span(&self) -> Span {
        self.machine
            .program()
            .function(self.function)
            .span_at(self.pc)
    }

    /// How many calls are live, this one included.
    pub fn depth(&self) -> usize {
        self.machine.calls().len()
    }

    /// Every live call, innermost first.
    ///
    /// This renders every local of every frame, so a debugger that stops at
    /// every instruction and takes a whole backtrace at each one is doing
    /// real work per instruction. [`Stop::frame`] is the same view of one
    /// call, for a session that only shows what it was asked for.
    pub fn backtrace(&self) -> Vec<Call> {
        (0..self.depth()).filter_map(|at| self.frame(at)).collect()
    }

    /// The call `at` levels out from this one, or `None` past the outermost.
    pub fn frame(&self, at: usize) -> Option<Call> {
        let (id, base, pc) = *self.machine.calls().get(at)?;
        Some(self.call(id, base, pc))
    }

    /// What the word `at` names, if it names an object of this run's heap.
    ///
    /// The one place a raw word crosses, and it crosses inward: what comes
    /// back is a rendered snapshot. It is for the view VM development wants —
    /// a [`Word`] showed an address, and this says what is there — and it
    /// promises nothing about what a word means. A word that is not an
    /// object this memory holds answers `None`.
    pub fn object(&self, at: u64) -> Option<Object> {
        let (name, fields) = render::parts(self.machine, at)?;
        Some(Object {
            name,
            fields: fields
                .into_iter()
                .map(|(name, value)| Field { name, value })
                .collect(),
        })
    }

    /// The instructions around this one, `reach` either side of it.
    ///
    /// The disassembly a session shows beside a stop.
    /// [`cove_ir::print::one`] renders each, which is the same rendering
    /// `cove ir` prints, so a debugger and a dump do not disagree about what
    /// an instruction is called.
    pub fn code(&self, reach: usize) -> Vec<Line> {
        let program = self.machine.program();
        let function = program.function(self.function);
        let from = self.pc.saturating_sub(reach);
        let to = (self.pc + reach + 1).min(function.code.len());
        (from..to)
            .map(|pc| Line {
                pc: pc as u32,
                text: print::one(program, function, &function.code[pc]),
                span: function.span_at(pc),
                current: pc == self.pc,
            })
            .collect()
    }

    /// One frame, projected.
    fn call(&self, id: FunctionId, base: u64, pc: u32) -> Call {
        let program = self.machine.program();
        let function = program.function(id);
        let mut named = vec![false; function.frame_size() as usize];
        let mut locals = Vec::new();
        for local in &function.locals {
            if !(local.from <= pc && pc < local.to) {
                continue;
            }
            let width = program.layout(local.layout).width();
            let words = self.machine.frame_run(base, local.slot, width);
            for word in local.slot..(local.slot + width).min(function.frame_size()) {
                named[word as usize] = true;
            }
            locals.push(Local {
                name: local.name.to_string(),
                value: render::lossy(self.machine, local.layout, &words),
                at: local.slot,
                width,
            });
        }
        // What no name covers, read as the frame itself describes it. This is
        // the VM development view: a compiler temporary, a slot whose live
        // range has ended, a word the lowering wrote and no source name ever
        // held.
        let words = named
            .iter()
            .enumerate()
            .filter(|(_, named)| !**named)
            .map(|(at, _)| {
                let at = at as u32;
                let repr = function.repr(at).unwrap_or(cove_ir::Repr::Int);
                let raw = self.machine.frame_run(base, at, 1)[0];
                Word {
                    at,
                    holds: repr.name(),
                    raw,
                    value: render::raw(self.machine, repr, raw),
                }
            })
            .collect();
        Call {
            function: function.qualified(),
            pc,
            span: function.span_at(pc as usize),
            locals,
            words,
        }
    }
}

/// One live call, as a debugger sees it.
#[derive(Clone, Debug)]
pub struct Call {
    function: String,
    pc: Pc,
    span: Span,
    locals: Vec<Local>,
    words: Vec<Word>,
}

impl Call {
    /// `module.name` of the function running here.
    pub fn function(&self) -> &str {
        &self.function
    }

    /// Where in it this call is: the instruction about to run for the
    /// innermost call, and the one to return to for every other.
    pub fn pc(&self) -> Pc {
        self.pc
    }

    /// Where that instruction was written.
    pub fn span(&self) -> Span {
        self.span
    }

    /// The names the source bound that are in scope at this pc.
    pub fn locals(&self) -> &[Local] {
        &self.locals
    }

    /// The frame's own words that no name in scope covers.
    pub fn words(&self) -> &[Word] {
        &self.words
    }

    /// The local called `name`, if one is in scope here.
    pub fn local(&self, name: &str) -> Option<&Local> {
        self.locals.iter().find(|local| local.name == name)
    }
}

/// One name the source bound, and what it holds.
#[derive(Clone, Debug)]
pub struct Local {
    name: String,
    value: String,
    at: u32,
    width: u32,
}

impl Local {
    /// What the source called it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What it holds, rendered.
    ///
    /// Always an answer, which is the whole difference from a boundary
    /// crossing. A value that could not be
    /// read carries a marker saying which way it could not — `<reclaimed>`,
    /// `<cycle>`, `<case 7 of 3>` — rather than being absent.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Which word of the frame it begins at.
    ///
    /// A position, not a name of anything this crate owns: it is what lets a
    /// session say *the same word as that one* without being handed the type
    /// the machine indexes frames by.
    pub fn at(&self) -> u32 {
        self.at
    }

    /// How many words it occupies. A value is a run of words, so a name
    /// covers `width` of them from [`Local::at`].
    pub fn width(&self) -> u32 {
        self.width
    }
}

/// One word of a frame that no name in scope covers.
#[derive(Clone, Debug)]
pub struct Word {
    at: u32,
    holds: &'static str,
    raw: u64,
    value: String,
}

impl Word {
    /// Which word of the frame it is.
    pub fn at(&self) -> u32 {
        self.at
    }

    /// What the frame says is in it — `Int`, `Ref`, `Duration`.
    pub fn holds(&self) -> &'static str {
        self.holds
    }

    /// The word itself.
    pub fn raw(&self) -> u64 {
        self.raw
    }

    /// The word read as what the frame says it is. A reference renders as
    /// the address it is and is not followed; [`Stop::object`] is what
    /// follows one.
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// One object of the run's heap, rendered.
#[derive(Clone, Debug)]
pub struct Object {
    name: String,
    fields: Vec<Field>,
}

impl Object {
    /// What the family it belongs to is called.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Its parts, named the way its family names them: a struct's fields by
    /// their source names, a run of elements by its indices, an enum by
    /// `case` and then its payload's positions.
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }
}

/// One named part of an object.
#[derive(Clone, Debug)]
pub struct Field {
    name: String,
    value: String,
}

impl Field {
    /// What the part is called.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What it holds, rendered.
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// One line of disassembly.
#[derive(Clone, Debug)]
pub struct Line {
    pc: Pc,
    text: String,
    span: Span,
    current: bool,
}

impl Line {
    /// Which instruction of the function it is.
    pub fn pc(&self) -> Pc {
        self.pc
    }

    /// The instruction, as `cove ir` prints it.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Where it was written.
    pub fn span(&self) -> Span {
        self.span
    }

    /// Whether it is the one the stop is at.
    pub fn current(&self) -> bool {
        self.current
    }
}

/// The error a run ends with when a debugger answers [`Resume::Halt`].
///
/// A stop and not a failure of the program's work, which is why it is
/// classified as [`RunOutcome::Debugger`] and not as an invariant a program
/// broke. `every_stop_mode_is_reported_as_itself_on_both_backends` in
/// `crates/cove-runtime/tests/responsiveness.rs` is the shape being kept:
/// each way a run can stop reports itself and not something else.
pub(crate) fn halted(span: Span) -> RuntimeError {
    RuntimeError::new("a debugger stopped this run")
        .at(span)
        .with_rule("A debugger may halt a run at any instruction, and the run ends there.")
        .with_outcome(RunOutcome::Debugger)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use cove_diag::SourceMap;
    use cove_sema::config::Config;
    use cove_sema::package::{Module, Package, Unit};
    use cove_sema::resolve::Program as Checked;

    use super::*;
    use crate::budget::{Budget, Limits};
    use crate::host::{Grants, HostRegistry};
    use crate::runtime::Runtime;
    use crate::vm::exec::SAFEPOINT_STRIDE;
    use crate::vm::Vm;

    /// A program that runs for longer than two safepoint strides.
    const LOOP: &str = "
export fn main() -> Int {
  var total = 0
  var i = 0
  while i < 2000 {
    total = total + i
    i = i + 1
  }
  total
}
";

    /// Three frames, and a name bound in each of them.
    const NESTED: &str = "
fn inner(a: Int) -> Int {
  let doubled = a * 2
  doubled
}

fn outer(b: Int) -> Int {
  let raised = b + 1
  inner(raised)
}

export fn main() -> Int {
  outer(20)
}
";

    /// Everything one run needs, held together so that the borrows it takes
    /// of each other outlive the [`Vm`] built on them.
    struct World {
        hosts: Arc<HostRegistry>,
        runtime: Runtime,
        program: cove_ir::Program,
    }

    impl World {
        fn new(source: &str) -> World {
            let (sources, checked) = checked(source);
            let program = lowered(&sources, &checked);
            let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
            let runtime = Runtime::new(checked, sources, hosts.clone());
            World {
                hosts,
                runtime,
                program,
            }
        }

        /// A run nothing is watching.
        fn plain(&self) -> Vm<'_> {
            Vm::new(&self.runtime, &self.hosts, &self.program)
        }

        /// A run `debugger` is watching.
        fn watched<'w>(&'w self, debugger: &'w dyn Debugger) -> Vm<'w> {
            Vm::debugged(&self.runtime, &self.hosts, &self.program, debugger)
        }
    }

    /// Parses, resolves and checks one module called `m`.
    fn checked(source: &str) -> (Arc<SourceMap>, Arc<Checked>) {
        let mut sources = SourceMap::new();
        let file = sources.add("m/main.cove", source.to_string());
        let ast = cove_syntax::parse_file(&sources, file).expect("the fixture parses");
        let package = Package {
            root: PathBuf::from("."),
            config: Config::default(),
            modules: BTreeMap::from([(
                "m".to_string(),
                Module {
                    name: "m".to_string(),
                    dir: PathBuf::from("m"),
                    units: vec![Unit {
                        file,
                        path: PathBuf::from("m/main.cove"),
                        ast,
                    }],
                },
            )]),
        };
        let program = cove_sema::Compiler::new()
            .compile(&package)
            .expect("the fixture checks");
        (Arc::new(sources), Arc::new(program))
    }

    fn lowered(sources: &SourceMap, checked: &Checked) -> cove_ir::Program {
        cove_ir::lower(checked, sources, &cove_schema::HostSchemas::new())
            .expect("the fixture lowers")
    }

    /// A debugger that writes down what it was shown and always says go.
    #[derive(Default)]
    struct Seen(Mutex<Vec<u64>>);

    impl Debugger for Seen {
        fn at(&self, stop: &Stop<'_>) -> Resume {
            self.0.lock().expect("a lock").push(stop.instructions());
            Resume::Go
        }
    }

    /// A debugger that halts once the run has got somewhere.
    struct HaltAt(u64);

    impl Debugger for HaltAt {
        fn at(&self, stop: &Stop<'_>) -> Resume {
            match stop.instructions() >= self.0 {
                true => Resume::Halt,
                false => Resume::Go,
            }
        }
    }

    /// A debugger that takes one backtrace, the first time it is deep enough.
    #[derive(Default)]
    struct Deepest {
        depth: AtomicU64,
        taken: Mutex<Option<Vec<Call>>>,
    }

    impl Debugger for Deepest {
        fn at(&self, stop: &Stop<'_>) -> Resume {
            let depth = stop.depth() as u64;
            if depth > self.depth.load(Ordering::SeqCst) {
                self.depth.store(depth, Ordering::SeqCst);
                *self.taken.lock().expect("a lock") = Some(stop.backtrace());
            }
            Resume::Go
        }
    }

    /// **A debugger is asked before every instruction, in order, and the
    /// count it is shown is the run's own.**
    ///
    /// The whole of what the machine promises: not "often enough", not "at
    /// every call" — every instruction, once, counted the way
    /// [`Vm::instructions`] counts.
    #[test]
    fn a_debugger_is_asked_once_before_every_instruction_in_order() {
        let world = World::new(NESTED);
        let seen = Seen::default();
        let mut vm = world.watched(&seen);
        let answer = vm.invoke("m", "main", Vec::new()).expect("the run answers");

        assert_eq!(format!("{answer}"), "42");
        let counts = seen.0.lock().expect("a lock").clone();
        assert_eq!(
            counts.len() as u64,
            vm.instructions(),
            "one question per instruction the run dispatched"
        );
        let expected: Vec<u64> = (1..=vm.instructions()).collect();
        assert_eq!(counts, expected, "in order, and each one once");
    }

    /// **A debugger that halts ends the run there, and the run reports
    /// itself as the debugger's stop.**
    ///
    /// Its own outcome, the way every other stop mode has one: a run
    /// somebody was stepping through did not break an invariant and did not
    /// run out of anything.
    #[test]
    fn halting_ends_the_run_and_is_reported_as_the_debugger_s_own_stop() {
        let world = World::new(LOOP);
        let halt = HaltAt(500);
        let mut vm = world.watched(&halt);
        let error = vm
            .invoke("m", "main", Vec::new())
            .expect_err("a halted run does not answer");

        assert_eq!(error.outcome, crate::trace::RunOutcome::Debugger);
        assert_eq!(
            vm.instructions(),
            500,
            "the instruction it halted at was counted and not run"
        );
    }

    /// **A backtrace names the functions it is standing in, innermost
    /// first, and a local of an outer frame is found by the name the source
    /// gave it.**
    ///
    /// This is the projection the whole of part 3 exists for, and none of
    /// what it answers is a piece of the machine: a qualified name, a pc, and
    /// a rendered value.
    #[test]
    fn a_backtrace_names_its_calls_and_finds_a_local_of_an_outer_frame() {
        let world = World::new(NESTED);
        let deepest = Deepest::default();
        let mut vm = world.watched(&deepest);
        vm.invoke("m", "main", Vec::new()).expect("the run answers");

        let frames = deepest
            .taken
            .lock()
            .expect("a lock")
            .clone()
            .expect("the run stopped somewhere");
        let names: Vec<&str> = frames.iter().map(Call::function).collect();
        assert_eq!(names, vec!["m.inner", "m.outer", "m.main"]);

        let outer = &frames[1];
        let b = outer.local("b").expect("`outer`'s parameter is in scope");
        assert_eq!(b.value(), "20");
        assert_eq!(b.width(), 1, "an `Int` is one word");
        let raised = outer.local("raised").expect("its `let` is in scope too");
        assert_eq!(raised.value(), "21");
        assert_ne!(
            b.at(),
            raised.at(),
            "two names in scope at once are two positions"
        );
    }

    /// **A stop can say where it is, and read the instruction it is at.**
    ///
    /// The other half of what a session shows beside a frame: the pc, and
    /// the disassembly around it, rendered the way `cove ir` renders it.
    #[test]
    fn a_stop_reads_the_instruction_it_is_about_to_run() {
        /// Keeps the first stop inside `m.inner`.
        #[derive(Default)]
        struct First(Mutex<Option<(String, u32, Vec<Line>)>>);

        impl Debugger for First {
            fn at(&self, stop: &Stop<'_>) -> Resume {
                let mut held = self.0.lock().expect("a lock");
                if held.is_none() && stop.function() == "m.inner" {
                    *held = Some((stop.function(), stop.pc(), stop.code(2)));
                }
                Resume::Go
            }
        }

        let world = World::new(NESTED);
        let first = First::default();
        let mut vm = world.watched(&first);
        vm.invoke("m", "main", Vec::new()).expect("the run answers");

        let held = first.0.lock().expect("a lock").clone();
        let (function, pc, code) = held.expect("the run entered `m.inner`");
        assert_eq!(function, "m.inner");
        assert_eq!(pc, 0, "a call stops first at the callee's first pc");
        let current: Vec<&Line> = code.iter().filter(|line| line.current()).collect();
        assert_eq!(current.len(), 1, "exactly one line is the one stopped at");
        assert_eq!(current[0].pc(), pc);
        assert!(
            !current[0].text().is_empty(),
            "an instruction renders as something"
        );
    }

    /// **A debugger changes what a run does not at all.**
    ///
    /// Same answer, same instruction count, watched or not. A machine whose
    /// work depended on whether anybody was looking would be a debugger that
    /// could not be trusted about the run it was shown.
    #[test]
    fn watching_a_run_changes_neither_its_answer_nor_its_work() {
        let world = World::new(LOOP);
        let mut plain = world.plain();
        let alone = plain.invoke("m", "main", Vec::new()).expect("it answers");
        let instructions = plain.instructions();

        let seen = Seen::default();
        let mut watched = world.watched(&seen);
        let watched_answer = watched.invoke("m", "main", Vec::new()).expect("it answers");

        assert_eq!(format!("{alone}"), format!("{watched_answer}"));
        assert_eq!(instructions, watched.instructions());
    }

    /// **The safepoint fires at exactly the instruction counts it fired at
    /// before, whether a debugger is installed or not.**
    ///
    /// `SAFEPOINT_STRIDE` is contract arithmetic:
    /// `docs/adr/0040-a-bound-outlives-its-backend.md` states every stop
    /// mode's bound in multiples of it and `tests/responsiveness.rs`
    /// measures each one, so folding the debugger's question into the
    /// loop's one comparison may not move the schedule by a single
    /// instruction.
    ///
    /// The instrument is the fuel limit, because a safepoint is the only
    /// place fuel is charged: a run whose budget cannot survive its first
    /// charge stops at exactly the instruction the first safepoint is at,
    /// and one that can survive that but not the second stops at the second.
    #[test]
    fn the_safepoint_fires_at_the_same_counts_as_it_did_before() {
        let world = World::new(LOOP);
        for (fuel, expected) in [
            (1, SAFEPOINT_STRIDE),
            (SAFEPOINT_STRIDE + 1, 2 * SAFEPOINT_STRIDE),
        ] {
            let limits = Limits {
                fuel: Some(fuel),
                ..Limits::default()
            };
            let mut vm = world.plain();
            let error = vm
                .run_entry_within(Budget::new(limits.clone()), "m", "main", Vec::new())
                .expect_err("a run out of fuel does not answer");
            assert_eq!(error.outcome, crate::trace::RunOutcome::Fuel);
            assert_eq!(
                vm.instructions(),
                expected,
                "unwatched, under a fuel limit of {fuel}"
            );

            let seen = Seen::default();
            let mut vm = world.watched(&seen);
            let error = vm
                .run_entry_within(Budget::new(limits), "m", "main", Vec::new())
                .expect_err("a run out of fuel does not answer");
            assert_eq!(error.outcome, crate::trace::RunOutcome::Fuel);
            assert_eq!(
                vm.instructions(),
                expected,
                "watched, under a fuel limit of {fuel}"
            );
        }
    }

    /// **A stop reads the frame the machine is really in, not the one it was
    /// in an instruction ago.**
    ///
    /// `frame.pc` is a local of the dispatch loop between safepoints and is
    /// only made truthful by `Machine::sync`. A debugger asked before the
    /// sync would report the pc of the previous instruction, which is the
    /// one bug in this whole arrangement that would not show up as a crash.
    #[test]
    fn the_pc_a_stop_reports_is_the_one_the_top_frame_holds() {
        /// Records the pc twice: as the stop says it, and as the innermost
        /// frame of the backtrace says it.
        #[derive(Default)]
        struct Both(Mutex<Vec<(u32, u32)>>);

        impl Debugger for Both {
            fn at(&self, stop: &Stop<'_>) -> Resume {
                let innermost = stop.frame(0).expect("a stop is inside a call");
                self.0
                    .lock()
                    .expect("a lock")
                    .push((stop.pc(), innermost.pc()));
                Resume::Go
            }
        }

        let world = World::new(NESTED);
        let both = Both::default();
        let mut vm = world.watched(&both);
        vm.invoke("m", "main", Vec::new()).expect("the run answers");

        let seen = both.0.lock().expect("a lock").clone();
        assert!(!seen.is_empty(), "the run stopped at least once");
        for (stop, frame) in seen {
            assert_eq!(stop, frame, "the frame's pc was synced before the question");
        }
    }

    /// **A run of a program with no debugger is the run it always was.**
    ///
    /// The regression this whole part is about: `Vm::new`'s signature is
    /// unchanged, and so is what it produces.
    #[test]
    fn an_unwatched_run_answers_what_it_always_did() {
        let world = World::new(NESTED);
        let mut vm = world.plain();
        let answer = vm.invoke("m", "main", Vec::new()).expect("it answers");
        assert_eq!(format!("{answer}"), "42");
        assert!(vm.instructions() > 0);
    }
}
