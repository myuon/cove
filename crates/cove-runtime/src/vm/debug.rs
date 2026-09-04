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
//!
//! [`Local::words`] widens *which* words a [`Word`] view can show and not
//! what one is: the words a name covers, in the same projection as the words
//! no name covers, so that a named reference can be followed the way an
//! unnamed one already could. The argument above is unchanged by it, which
//! is the test of whether it belonged here.
//!
//! One number that is not a representation crosses too. [`Stop::task`] is
//! the id `crate::trace` writes on its events, and it is here because
//! everything else a [`Stop`] answers — the count, the depth, the frames —
//! belongs to one task and nothing said which. It names a task; it is not a
//! way to reach one.

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
    /// instruction about to run was counted. It is *this task's*, and
    /// [`Stop::task`] is what says whose.
    pub fn instructions(&self) -> u64 {
        self.machine.instructions()
    }

    /// Which task this stop is in.
    ///
    /// Everything else a `Stop` answers is one task's — the count, the
    /// depth, the frames — because a spawned task runs on a machine of its
    /// own, and until this was here a debugger could not tell two of them
    /// apart. A policy stated in frame depth is then a policy that a second
    /// task can satisfy by accident: a `step` asked in the entry, finished
    /// by an instruction of a task the entry spawned.
    ///
    /// The number is [`crate::ENTRY_TASK`] for the entry and a spawned
    /// task's own id otherwise, which is to say it is the number
    /// `crate::trace`'s events carry under `task`. That is deliberate and it
    /// is the whole of the choice here: a debugger and a trace of the same
    /// run must name the same task the same way, or a person holding both
    /// has to work out the correspondence themselves.
    ///
    /// It is opaque. Two stops with the same id are the same task and two
    /// with different ids are not; nothing else about the number is
    /// promised, and no ordering of it means anything.
    pub fn task(&self) -> u64 {
        self.machine.task()
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
    /// This renders every local of every frame — and every word of every
    /// local — so a debugger that stops at every instruction and takes a
    /// whole backtrace at each one is doing real work per instruction.
    /// [`Stop::frame`] is the same view of one call, for a session that only
    /// shows what it was asked for.
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

    /// The instructions around frame `at`'s pc, `reach` either side of it,
    /// or nothing past the outermost frame.
    ///
    /// The disassembly a session shows beside a stop.
    /// [`cove_ir::print::one`] renders each, which is the same rendering
    /// `cove ir` prints, so a debugger and a dump do not disagree about what
    /// an instruction is called.
    ///
    /// `at` names a frame the way [`Stop::frame`] names one, and for the
    /// same reason: a session that lets a person select a frame has to be
    /// able to show that frame's code, and one that could only ever
    /// disassemble the innermost would answer `frame 2` with frame 0's
    /// instructions. It is a parameter here rather than a method on [`Call`]
    /// because a `Call` is an owned snapshot: giving it a disassembly would
    /// mean rendering every instruction of every live function at every
    /// stop, and a backtrace is already the expensive view.
    ///
    /// The pc it reads is the frame's own, so the line marked
    /// [`Line::current`] is the instruction about to run for the innermost
    /// frame and the one to return to for every other — which is what
    /// [`Call::pc`] answers, and the same distinction.
    pub fn code(&self, at: usize, reach: usize) -> Vec<Line> {
        let Some((id, _, frame_pc)) = self.machine.calls().get(at).copied() else {
            return Vec::new();
        };
        let frame_pc = frame_pc as usize;
        let program = self.machine.program();
        let function = program.function(id);
        let from = frame_pc.saturating_sub(reach);
        let to = (frame_pc + reach + 1).min(function.code.len());
        (from..to)
            .map(|pc| Line {
                pc: pc as u32,
                text: print::one(program, function, &function.code[pc]),
                span: function.span_at(pc),
                current: pc == frame_pc,
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
                words: words
                    .iter()
                    .enumerate()
                    .map(|(offset, raw)| self.word(function, local.slot + offset as u32, *raw))
                    .collect(),
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
                let raw = self.machine.frame_run(base, at, 1)[0];
                self.word(function, at, raw)
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

    /// One word of a frame, read as the frame itself says it should be.
    ///
    /// The same projection whether a name covers the word or nothing does,
    /// which is what lets [`Local::words`] and [`Call::words`] be one type:
    /// a position, what the frame says is in it, the word, and that word
    /// rendered. A frame that says nothing about a word — one past the
    /// declared frame, which the lowering does not produce — is read as an
    /// `Int`, because a raw word shown as a number is the least a reader can
    /// be told and it is still true.
    fn word(&self, function: &cove_ir::Function, at: u32, raw: u64) -> Word {
        let repr = function.repr(at).unwrap_or(cove_ir::Repr::Int);
        Word {
            at,
            holds: repr.name(),
            raw,
            value: render::raw(self.machine, repr, raw),
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

    /// The names the source bound that are in scope at this pc, in
    /// declaration order.
    ///
    /// One name may appear twice. Shadowing is *recorded* rather than
    /// resolved — see [`cove_ir::Local`] — so `let x = 1; let x = x + 41` is
    /// two live bindings of two words, and both are here. [`Call::local`] is
    /// what chooses between them.
    pub fn locals(&self) -> &[Local] {
        &self.locals
    }

    /// The frame's own words that no name in scope covers.
    pub fn words(&self) -> &[Word] {
        &self.words
    }

    /// The local called `name` that the source means at this pc, if one is
    /// in scope here.
    ///
    /// **The last match wins**, which is
    /// [`cove_ir::Function::local_at`]'s rule and is the rule because the
    /// lowering resolves a name by searching its scope backwards. Two
    /// bindings of one name are live at once and the later one is what the
    /// source at this pc means; taking the first match would answer with the
    /// *shadowed* binding, which is a debugger that is wrong about the value
    /// of a name exactly where a person is most likely to ask.
    ///
    /// The earlier binding is not hidden — it is still in [`Call::locals`],
    /// where a reader can see both — because it is still in the frame, and a
    /// view of the machine that quietly dropped a word would be a worse
    /// lie than a view that shows two.
    pub fn local(&self, name: &str) -> Option<&Local> {
        self.locals.iter().rev().find(|local| local.name == name)
    }
}

/// One name the source bound, and what it holds.
#[derive(Clone, Debug)]
pub struct Local {
    name: String,
    value: String,
    at: u32,
    width: u32,
    words: Vec<Word>,
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
    /// covers `width` of them from [`Local::at`], and [`Local::words`] is
    /// that run.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// The words the name covers, read as the frame says they should be.
    ///
    /// [`Local::value`] is what the name holds *rendered*, and that is where
    /// a reader stops. This is where a debugger does not: a name bound to a
    /// vector renders as its elements and holds a reference, and until this
    /// was here there was no way to get from the name to the reference —
    /// [`Stop::object`] follows a word, and [`Call::words`] by construction
    /// excludes every word a name covers. So `print xs` could show a vector
    /// and nothing could then look at the object, which is a hole in a
    /// debugger rather than a missing convenience.
    ///
    /// It is the same [`Word`] view [`Call::words`] hands out, and
    /// deliberately: a word of a frame is a word of a frame whether a name
    /// covers it or not, and a second shape for the same thing would be a
    /// second thing to keep true. `words().len()` is [`Local::width`], and
    /// the first of them is at [`Local::at`].
    pub fn words(&self) -> &[Word] {
        &self.words
    }
}

/// One word of a frame.
///
/// [`Call::words`] hands out the ones no name in scope covers, which is what
/// the view exists for; [`Local::words`] hands out the ones a name does. The
/// projection is the same either way — a position, what the frame says is in
/// it, the word, and that word rendered — because whether a name happens to
/// cover a word does not change what the word is.
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

    /// One name bound twice in one frame, so that two bindings of it are
    /// live at the same time.
    const SHADOWED: &str = "
export fn main() -> Int {
  let total = 0
  let total = total + 20
  total
}
";

    /// A name bound to something on the heap, for following a reference the
    /// frame holds by the name that holds it.
    const ARRAY: &str = "
export fn main() -> Int {
  let items = [10, 20, 30]
  items.length()
}
";

    /// Two tasks, so that two machines ask the same debugger.
    const SPAWNED: &str = "
export fn main() -> Int {
  var total = 0
  scope tasks {
    let first = tasks.spawn { 42 }
    total = await first
  }
  total
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
                    *held = Some((stop.function(), stop.pc(), stop.code(0, 2)));
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

    /// **A name bound twice in one frame is answered by the later binding,
    /// and the earlier one is still there to be seen.**
    ///
    /// `cove_ir::Function::local_at` is the rule and the reason: shadowing
    /// is recorded rather than resolved, so `let total = 0; let total =
    /// total + 20` is two live bindings of two words, and the lowering
    /// resolves a name by searching its scope *backwards*. A view that took
    /// the first match would answer `total` with the value the source
    /// stopped meaning one line earlier — wrong, and wrong silently, since
    /// both answers are numbers.
    ///
    /// The method is pinned here and not only in `cove debug`'s own suite
    /// because every reader of a stop calls it: the CLI worked around the
    /// first-match rule by reading `Call::locals` backwards itself, and the
    /// next reader would have had to know to do the same.
    #[test]
    fn a_shadowed_name_is_answered_by_the_binding_the_source_means() {
        /// Keeps the last frame in which `total` was bound twice at once.
        #[derive(Default)]
        struct Shadow(Mutex<Option<Call>>);

        impl Debugger for Shadow {
            fn at(&self, stop: &Stop<'_>) -> Resume {
                if let Some(call) = stop.frame(0) {
                    let bound = call
                        .locals()
                        .iter()
                        .filter(|local| local.name() == "total")
                        .count();
                    if bound == 2 {
                        *self.0.lock().expect("a lock") = Some(call);
                    }
                }
                Resume::Go
            }
        }

        let world = World::new(SHADOWED);
        let shadow = Shadow::default();
        let mut vm = world.watched(&shadow);
        let answer = vm.invoke("m", "main", Vec::new()).expect("the run answers");
        assert_eq!(format!("{answer}"), "20");

        let call = shadow
            .0
            .lock()
            .expect("a lock")
            .clone()
            .expect("both bindings of `total` were live at once");
        let both: Vec<&Local> = call
            .locals()
            .iter()
            .filter(|local| local.name() == "total")
            .collect();
        assert_eq!(both.len(), 2, "shadowing is recorded, not resolved");
        assert_ne!(both[0].at(), both[1].at(), "two bindings are two words");
        assert_eq!(both[0].value(), "0", "the shadowed binding is still there");

        let meant = call.local("total").expect("`total` is in scope");
        assert_eq!(
            meant.at(),
            both[1].at(),
            "the later declaration is the one the source means"
        );
        assert_eq!(meant.value(), "20");
    }

    /// **A stop says which task it is in, and the id is the one a trace
    /// writes.**
    ///
    /// Everything else a stop answers is one task's — the instruction count,
    /// the depth, the frames — because a spawned task runs on a machine of
    /// its own. Without this, a policy stated in frame depth can be
    /// satisfied by a task nobody was stepping in, and a count can be read
    /// as the run's when it is one thread's.
    ///
    /// The count is checked per task rather than the id alone, because the
    /// id would be worth nothing if the things it names were not really that
    /// task's: each machine counts from one, so the first stop of every task
    /// is its own first instruction.
    #[test]
    fn a_stop_names_the_task_it_is_in_and_a_spawned_task_is_not_the_entry() {
        /// The lowest instruction count seen in each task.
        #[derive(Default)]
        struct Whose(Mutex<BTreeMap<u64, u64>>);

        impl Debugger for Whose {
            fn at(&self, stop: &Stop<'_>) -> Resume {
                let mut seen = self.0.lock().expect("a lock");
                let first = seen.entry(stop.task()).or_insert(u64::MAX);
                *first = (*first).min(stop.instructions());
                Resume::Go
            }
        }

        let world = World::new(SPAWNED);
        let whose = Whose::default();
        let mut vm = world.watched(&whose);
        let answer = vm.invoke("m", "main", Vec::new()).expect("the run answers");
        assert_eq!(format!("{answer}"), "42");

        let seen = whose.0.lock().expect("a lock").clone();
        assert_eq!(
            seen.len(),
            2,
            "the entry and the task it spawned are two tasks, not one"
        );
        assert!(
            seen.contains_key(&crate::runtime::ENTRY_TASK),
            "the entry names itself the way a trace names it"
        );
        for (task, first) in seen {
            assert_eq!(first, 1, "task {task} counts its own instructions from one");
        }
    }

    /// **A disassembly is of the frame it was asked for, and the outermost
    /// is the last one there is.**
    ///
    /// A session that lets a person select a frame has to be able to show
    /// that frame's code. Reading the stopping function's would answer
    /// `frame 2` with frame 0's instructions — a listing that looks right,
    /// is wrong, and says nothing about which frame it is of.
    #[test]
    fn a_disassembly_is_of_the_frame_it_was_asked_for() {
        /// The first stop inside `m.inner`: the innermost frame's code, its
        /// caller's, that caller's own pc, and a frame that is not there.
        #[derive(Default)]
        #[allow(clippy::type_complexity)]
        struct Frames(Mutex<Option<(Vec<Line>, Vec<Line>, Pc, Vec<Line>)>>);

        impl Debugger for Frames {
            fn at(&self, stop: &Stop<'_>) -> Resume {
                let mut held = self.0.lock().expect("a lock");
                if held.is_none() && stop.function() == "m.inner" {
                    let caller = stop.frame(1).expect("`m.inner` was called from `m.outer`");
                    *held = Some((
                        stop.code(0, 2),
                        stop.code(1, 2),
                        caller.pc(),
                        stop.code(9, 2),
                    ));
                }
                Resume::Go
            }
        }

        let world = World::new(NESTED);
        let frames = Frames::default();
        let mut vm = world.watched(&frames);
        vm.invoke("m", "main", Vec::new()).expect("the run answers");

        let held = frames.0.lock().expect("a lock").clone();
        let (innermost, outer, caller_pc, past) = held.expect("the run entered `m.inner`");
        let marked = |lines: &[Line]| {
            let found: Vec<Line> = lines
                .iter()
                .filter(|line| line.current())
                .cloned()
                .collect();
            assert_eq!(found.len(), 1, "exactly one line is the one stopped at");
            found[0].clone()
        };

        assert_eq!(marked(&innermost).pc(), 0, "a call stops at the callee's 0");
        assert_eq!(
            marked(&outer).pc(),
            caller_pc,
            "the caller is shown at the pc it will return to"
        );
        assert_ne!(
            marked(&outer).text(),
            marked(&innermost).text(),
            "two frames of two functions are two instructions"
        );
        assert!(past.is_empty(), "there is no ninth frame to disassemble");
    }

    /// **A name hands over the words it covers, so a reference a name holds
    /// can be followed into the heap.**
    ///
    /// `Stop::object` follows a word, and `Call::words` by construction
    /// shows only the words no name covers. Between the two there was a hole
    /// exactly where a debugger is most used: a name bound to a vector
    /// rendered as its elements, and nothing could then ask what the object
    /// was. The word is the same `Word` view an unnamed word gets, because a
    /// word of a frame does not change shape by being named.
    #[test]
    fn a_local_hands_over_its_word_so_a_named_reference_can_be_followed() {
        /// The first stop at which `items` holds something.
        #[derive(Default)]
        #[allow(clippy::type_complexity)]
        struct Follow(Mutex<Option<(u32, u32, u32, String, Option<Object>)>>);

        impl Debugger for Follow {
            fn at(&self, stop: &Stop<'_>) -> Resume {
                let mut held = self.0.lock().expect("a lock");
                if held.is_some() {
                    return Resume::Go;
                }
                let Some(call) = stop.frame(0) else {
                    return Resume::Go;
                };
                let Some(items) = call.local("items") else {
                    return Resume::Go;
                };
                let [word] = items.words() else {
                    return Resume::Go;
                };
                if word.raw() == 0 {
                    return Resume::Go;
                }
                *held = Some((
                    items.at(),
                    items.width(),
                    word.at(),
                    word.holds().to_string(),
                    stop.object(word.raw()),
                ));
                Resume::Go
            }
        }

        let world = World::new(ARRAY);
        let follow = Follow::default();
        let mut vm = world.watched(&follow);
        let answer = vm.invoke("m", "main", Vec::new()).expect("the run answers");
        assert_eq!(format!("{answer}"), "3");

        let held = follow.0.lock().expect("a lock").clone();
        let (at, width, word_at, holds, object) = held.expect("`items` held a reference");
        assert_eq!(width, 1, "a reference is one word");
        assert_eq!(word_at, at, "the name's first word is the name's position");
        assert_eq!(holds, "ref", "the frame says the word is a reference");

        let object = object.expect("the word names an object of this run's heap");
        let values: Vec<&str> = object.fields().iter().map(Field::value).collect();
        assert_eq!(
            values,
            vec!["10", "20", "30"],
            "the object `items` names is the array the source wrote"
        );
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
