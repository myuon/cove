//! Talking to a stopped machine: `cove debug`.
//!
//! Issue #241's second half. The machine side landed first, in
//! `cove_runtime::vm::debug`, and it is deliberately policy-free: a
//! [`Debugger`] is asked before every instruction and answers [`Resume::Go`]
//! or [`Resume::Halt`], and the machine has no notion of a breakpoint, of a
//! step, or of a frame depth to come back to. Everything below is that
//! missing notion.
//!
//! # The prompt runs inside `Debugger::at`
//!
//! There is no driver loop here, and there cannot be one. A reader who comes
//! looking for `while let Some(command) = read()` around a suspended machine
//! will not find it, because the machine's own module explains why no handle
//! to a suspended machine can exist: the dispatch loop runs inside a
//! `std::thread::scope` and holds the borrow that scope hands out, so a value
//! carrying it could not outlive the call that made it.
//!
//! So the call is inverted, and this module is inverted with it.
//! [`Session::at`] *is* the loop body: when the policy says this instruction
//! is one to stop at, `Session::converse` prints where the run is, blocks on
//! stdin, and does not return until the person at the terminal has said what
//! should happen next. The machine is standing still for exactly as long as
//! that call takes. `continue`, `step` and the rest are not messages sent to
//! anything; they are a mode written into [`State`] just before `at` returns
//! `Go`, and the next question is answered against it.
//!
//! Two consequences follow and are worth stating rather than discovering.
//! The whole of [`State`] is behind one lock, because a spawned task's
//! machine asks the same debugger from that task's own thread; the lock is
//! held for the length of a stop, so a second task that reaches an
//! instruction while somebody is at the prompt waits there. And a per-
//! instruction question is answered under that lock, which is a mutex
//! acquisition per instruction — a debugged run is slower than a run, and
//! this is where it is slower. What it is not is *allocating*: the hot path
//! reads [`Stop::pc`], [`Stop::span`] and [`Stop::depth`], all of which are
//! copies of what the machine already had, and never [`Stop::function`],
//! which builds a `String`.
//!
//! The all-stop that follows is deliberate and it is not free: a program
//! whose tasks wait on each other can be stopped in a state where the task
//! at the prompt is the one another task is waiting for, and `continue` is
//! then the only thing that unsticks it. That is the same bargain every
//! all-stop debugger makes, and the alternative — a prompt per task —
//! is a second session, not a smaller change.
//!
//! # A breakpoint is a pc and a span, not a name
//!
//! Nothing in the lowered program maps source back to instructions, so
//! [`Sites`] builds that index once, before the run: one pass over every
//! instruction of every lowered function, keeping for each function and each
//! source line the *lowest* pc written there. `break demo/main.cove:12`
//! resolves against it and produces a set of [`Site`]s, and a site is a
//! `(pc, span)` pair rather than a function and a pc. That is what makes the
//! hot check a comparison of two `Copy` values the machine handed over for
//! free: a function id would have to be recovered from [`Stop::function`],
//! and that allocates.
//!
//! The pair identifies a location because two instructions of two different
//! functions at the same pc were written in two different places — except
//! when they were not, which is the case of one generic function lowered
//! twice. Then a breakpoint set on the source line stops in both
//! instantiations, which is the answer a reader of the source would expect.
//!
//! # What one `step` is
//!
//! Spans are per-instruction and expression-level, several adjacent
//! instructions share one, and nothing marks where a statement begins. The
//! rule this module settled on, written out in [`Mode::Line`]:
//!
//! > Remember the task, the file, the line, and the frame depth the step was
//! > asked from. Stop at the first instruction *of that task* that is either
//! > *shallower* than that depth — the frame returned — or written on a
//! > different line. `next` adds one clause: an instruction *deeper* than
//! > that depth is skipped, so a call runs to completion without stopping
//! > inside it.
//!
//! The task is part of the rule rather than an afterthought to it. A spawned
//! task runs on a machine of its own with a stack of its own, so a depth
//! compared across tasks compares two measurements as one, and a step asked
//! in the entry would be satisfied by whichever thread happened to reach an
//! instruction first. `Stop::task` is what the comparison is against.
//!
//! [`Session::misses`] is the honest list of what that rule gets wrong, and
//! it is printed by `help limits` so that the person using the debugger reads
//! it rather than this comment.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cove_diag::{FileId, SourceMap, Span};
use cove_ir::Pc;
use cove_runtime::embed::{register_hosts, HostSetup};
use cove_runtime::{
    Budget, Cancellation, Debugger, Limits, Local, Resume, RunOutcome, Runtime, Stop, Vm, Word,
};
use cove_sema::package::Package;
use cove_sema::HostSchemas;

use crate::{
    flag_value, load, lookup_entry, lookup_run, parse_duration_flag, report_exit, runtime_failure,
    CliError,
};

/// `cove debug <name> [flags] [args]`.
///
/// The same setup `cove run` does — the same package, the same
/// `[run.<name>]` table, the same capabilities, the same lowering — and then
/// [`Vm::debugged`] instead of `Vm::new`. There is no `--backend`: a debugger
/// is a thing the linear-memory machine has and the tree walker does not, so
/// naming a backend here would be naming the only one there is.
pub(crate) fn cmd_debug(args: &[String]) -> Result<(), CliError> {
    let Some(name) = args.first() else {
        return Err(CliError::Message(
            "`cove debug` needs the name of a `[run.<name>]` table in cove.toml".into(),
        ));
    };
    let flags = parse_debug_flags(&args[1..])?;

    let (sources, package, checked) = load(None)?;
    let run = lookup_run(&package, name)?;
    let (module, entry) = lookup_entry(&checked, name, run)?;
    let sources = Arc::new(sources);
    let checked = Arc::new(checked);

    // Lowered before a host is registered, for the reason `execute_entry`
    // lowers there: a gap in the lowering stops the command with the gap
    // pointed at in source, before the program can be observed by anything.
    let ir = cove_ir::lower_entry(&checked, &sources, &HostSchemas::new(), module, entry).map_err(
        |items| CliError::Diagnostics {
            items,
            sources: Arc::clone(&sources),
        },
    )?;
    let ir = Arc::new(ir);

    let mut hosts = register_hosts(HostSetup {
        grants: run.allow.clone(),
        documents_root: package.root.join("documents"),
        files_root: flags
            .files_root
            .clone()
            .unwrap_or_else(|| package.root.join("files")),
        program_args: flags.program_args.clone(),
        allow_exec: flags.allow_exec.clone(),
    });
    let limits = Limits {
        fuel: flags.fuel.or(run.fuel),
        deadline: flags.deadline.or(run.deadline),
        max_host_calls: flags.max_host_calls.or(run.max_host_calls),
        max_call_depth: None,
        max_tasks: flags.max_tasks.or(run.max_tasks),
    };
    hosts.set_budget(Budget::with_cancellation(limits, Cancellation::new()));

    let program_args: Vec<Rc<str>> = flags
        .program_args
        .iter()
        .map(|a| a.as_str().into())
        .collect();
    let runtime = Runtime::new(Arc::clone(&checked), Arc::clone(&sources), Arc::new(hosts));

    let session = Session::new(&package, &ir, &sources);
    println!(
        "cove debug — {}.{entry} on the linear-memory backend. `help` lists the commands.",
        module
    );
    let mut vm = Vm::debugged(&runtime, runtime.hosts(), &ir, &session);
    let outcome = vm.run_entry(module, entry, program_args);
    let quit = session.quit();

    match outcome {
        Ok(value) => {
            println!(
                "the run finished after {} instruction(s).",
                vm.instructions()
            );
            report_exit(value)
        }
        // A `quit` is a person stopping, not a program failing, so it is not
        // rendered as a diagnostic and does not fail the command.
        Err(error) if error.outcome == RunOutcome::Debugger && quit => {
            println!(
                "the run was halted after {} instruction(s).",
                vm.instructions()
            );
            Ok(())
        }
        Err(error) => Err(CliError::Diagnostics {
            items: vec![runtime_failure(&checked, module, entry, &error)],
            sources,
        }),
    }
}

/// The `cove run` flags that mean something to a debugged run.
///
/// Not [`crate::RunFlags`], and the difference is the point: `--backend` has
/// no answer here, and `--trace`, `--trace-values` and `--stats` describe a
/// run nobody is interrupting. What is left is the two budgets and the two
/// pieces of authority a `cove.toml` cannot express.
struct Flags {
    fuel: Option<u64>,
    deadline: Option<Duration>,
    max_host_calls: Option<u64>,
    max_tasks: Option<u64>,
    files_root: Option<PathBuf>,
    allow_exec: Vec<PathBuf>,
    program_args: Vec<String>,
}

/// Parses what follows `cove debug <name>`, the way `cove run` parses what
/// follows its own name: flags anywhere, everything after `--` a program
/// argument, anything unrecognised a program argument too.
fn parse_debug_flags(args: &[String]) -> Result<Flags, CliError> {
    let mut flags = Flags {
        fuel: None,
        deadline: None,
        max_host_calls: None,
        max_tasks: None,
        files_root: None,
        allow_exec: Vec::new(),
        program_args: Vec::new(),
    };
    let mut passthrough = false;
    let mut i = 0;
    while i < args.len() {
        if passthrough {
            flags.program_args.push(args[i].clone());
            i += 1;
            continue;
        }
        match args[i].as_str() {
            "--" => passthrough = true,
            "--fuel" => {
                let value = flag_value(args, &mut i, "--fuel")?;
                flags.fuel = Some(value.parse().map_err(|_| {
                    CliError::Message(format!(
                        "`--fuel` must be a non-negative integer, found `{value}`"
                    ))
                })?);
            }
            "--deadline" => {
                let value = flag_value(args, &mut i, "--deadline")?;
                flags.deadline = Some(
                    parse_duration_flag(&value)
                        .map_err(|e| CliError::Message(format!("`--deadline`: {e}")))?,
                );
            }
            "--max-host-calls" => {
                let value = flag_value(args, &mut i, "--max-host-calls")?;
                flags.max_host_calls = Some(value.parse().map_err(|_| {
                    CliError::Message(format!(
                        "`--max-host-calls` must be a non-negative integer, found `{value}`"
                    ))
                })?);
            }
            "--max-tasks" => {
                let value = flag_value(args, &mut i, "--max-tasks")?;
                flags.max_tasks = Some(value.parse().map_err(|_| {
                    CliError::Message(format!(
                        "`--max-tasks` must be a non-negative integer, found `{value}`"
                    ))
                })?);
            }
            "--files-root" => {
                let value = flag_value(args, &mut i, "--files-root")?;
                flags.files_root = Some(PathBuf::from(value));
            }
            "--allow-exec" => {
                let value = flag_value(args, &mut i, "--allow-exec")?;
                let path = PathBuf::from(&value);
                if !path.is_absolute() {
                    return Err(CliError::Message(format!(
                        "`--allow-exec` takes an absolute path, found `{value}`"
                    )));
                }
                flags.allow_exec.push(path);
            }
            // Refused rather than ignored. A debugger is something the
            // linear-memory machine has and the tree walker does not, so
            // accepting `--backend ast` would be accepting a request that
            // cannot be honoured.
            "--backend" => {
                return Err(CliError::Message(
                    "`cove debug` takes no `--backend`: a debugger is a feature of the \
                     linear-memory backend, and that is the only backend it runs on"
                        .into(),
                ))
            }
            other => flags.program_args.push(other.to_string()),
        }
        i += 1;
    }
    Ok(flags)
}

/// One place a breakpoint may fire, named the way the hot path can compare
/// it: the instruction's own pc and the span it was lowered from.
///
/// Not a function id, because recovering one at a stop means
/// [`Stop::function`], and that allocates a `String` for every instruction of
/// a `continue`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Site {
    pc: Pc,
    span: Span,
}

/// Source line to instruction, built once before the run.
///
/// The index nothing in `cove-ir` provides: `Function::spans` maps an
/// instruction to where it was written, and this is that map inverted. For
/// each lowered function and each line, the *lowest* pc written on that line,
/// which is the closest thing to "where the line begins" a lowering with no
/// statement markers can offer.
///
/// It costs one pass over every instruction of the lowered program and holds
/// one entry per distinct function-and-line pair.
///
/// # A stub is not code
///
/// `cove_ir::lower_entry` lowers what the entry reaches and leaves every
/// other declaration of the package as a *stub* — a function of one `Return`
/// carrying the declaration's own span. A stub is in `Program::functions`
/// like any other function, so an index built without noticing them would
/// accept `break some.unreached` and `break main.cove:<its declaration>` and
/// then never fire, which is the silent failure this command is supposed not
/// to have.
///
/// `cove_ir::Function::is_stub` is how they are told apart, and it is the
/// lowering's own record of what it did. This command used to recognise one
/// by its shape — one instruction, written at the declaration's own span, no
/// parameters, no names — which worked and was still wrong: the shape is an
/// accident of how a stub is built, it is `cove-ir`'s to change, and nothing
/// there said so. Asking the lowering is asking the only thing that knows.
struct Sites {
    by_line: BTreeMap<(FileId, usize), Vec<(Site, String)>>,
    /// Each lowered function's first instruction, for `break <function>`.
    entries: Vec<(Site, String)>,
    /// The declarations this entry does not reach, and where each is
    /// written, so that a breakpoint on one is refused by name.
    stubs: Vec<(Span, String)>,
}

impl Sites {
    fn build(ir: &cove_ir::Program, sources: &SourceMap) -> Sites {
        let mut by_line: BTreeMap<(FileId, usize), Vec<(Site, String)>> = BTreeMap::new();
        let mut entries = Vec::new();
        let mut stubs = Vec::new();
        for function in &ir.functions {
            let qualified = function.qualified();
            if function.is_stub() {
                stubs.push((function.span, qualified));
                continue;
            }
            if !function.code.is_empty() {
                entries.push((
                    Site {
                        pc: 0,
                        span: function.span_at(0),
                    },
                    qualified.clone(),
                ));
            }
            // Lowest pc wins, and pcs are visited in order, so the first
            // instruction seen on a line is the one kept. A line reached
            // twice in one function — two statements written on it, or an
            // `if` and its `else` — keeps only the earlier, which is the
            // limitation `help limits` names.
            let mut seen: BTreeMap<(FileId, usize), Site> = BTreeMap::new();
            for pc in 0..function.code.len() {
                let span = function.span_at(pc);
                let line = sources.get(span.file).line_col(span.start).0;
                seen.entry((span.file, line))
                    .or_insert(Site { pc: pc as Pc, span });
            }
            for (key, site) in seen {
                by_line
                    .entry(key)
                    .or_default()
                    .push((site, qualified.clone()));
            }
        }
        Sites {
            by_line,
            entries,
            stubs,
        }
    }

    /// The unreached declaration written on `line` of `file`, if one is.
    fn stub_on(&self, file: FileId, line: usize, sources: &SourceMap) -> Option<&str> {
        self.stubs.iter().find_map(|(span, name)| {
            let at = sources.get(span.file).line_col(span.start).0;
            (span.file == file && at == line).then_some(name.as_str())
        })
    }

    /// Every instruction that begins `line` of `file`, one per function.
    fn at(&self, file: FileId, line: usize) -> &[(Site, String)] {
        self.by_line
            .get(&(file, line))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// One breakpoint the person set, and where it resolved to.
struct Breakpoint {
    number: usize,
    /// What was typed, for `info breakpoints` to read back.
    spec: String,
    /// Every instruction it fires at, with the function each is in.
    where_: Vec<(Site, String)>,
    hits: u64,
}

/// What makes the next question a stop.
enum Mode {
    /// Only a breakpoint does. `continue`.
    Free,
    /// The very next instruction does, whatever it is and whichever task
    /// runs it. `stepi`, and the state the session starts in so that the run
    /// stops before its first instruction.
    ///
    /// The one stepping mode that is *not* scoped to a task, deliberately:
    /// "one instruction" is a question about the machine rather than about a
    /// call, and in a program with a spawned task the next instruction the
    /// machine runs may be that task's. `help limits` says so.
    Instruction,
    /// A different source line does, or a shallower frame, in the task the
    /// step was asked in. `step`, and with `over` set, `next`.
    ///
    /// The whole of the source-stepping rule. The step was asked in `task`,
    /// from a stack `depth` deep, at an instruction written somewhere in
    /// `from..to` of `file`; an instruction of that task stops the run when
    /// it is shallower than `depth`, or when it was written outside that
    /// range. `over` skips anything deeper, which is the only difference
    /// between `next` and `step`.
    ///
    /// `task` is what keeps the depth meaning something. A spawned task runs
    /// on a machine of its own with a stack of its own, so a depth compared
    /// across tasks is two different measurements compared as one: a `step`
    /// asked in the entry could be satisfied by an instruction of a task the
    /// entry spawned, which is a stop the person did not ask for and cannot
    /// explain. `Stop::task` is what makes the comparison answerable.
    ///
    /// The line is held as the byte range it occupies rather than as its
    /// number, because the number is what a `SourceMap` has to search for
    /// and this comparison is made before every instruction. The search
    /// happens once, when the step is asked for.
    Line {
        file: FileId,
        from: u32,
        to: u32,
        depth: usize,
        over: bool,
        task: u64,
    },
    /// A frame of `task` shallower than `depth` does. `finish`.
    ///
    /// Scoped for the reason [`Mode::Line`] is: another task standing one
    /// frame deep is not this frame returning.
    Out { depth: usize, task: u64 },
}

/// Why the run stopped, for the line printed above the prompt.
enum Why {
    Entry,
    Breakpoint(usize),
    Stepped,
    Returned,
}

/// Everything one debugging session remembers, behind one lock.
struct State {
    mode: Mode,
    breakpoints: Vec<Breakpoint>,
    /// Every enabled breakpoint's sites, flattened, with the number to
    /// report. Rebuilt whenever a breakpoint is added or deleted, so that
    /// the per-instruction check is a scan of a short `Vec` of `Copy` pairs
    /// and nothing else.
    armed: Vec<(Site, usize)>,
    next_number: usize,
    /// Which frame `print`, `locals`, `words` and `list` read. Reset to the
    /// innermost at every stop, as gdb resets it.
    frame: usize,
    /// What an empty line repeats.
    last: String,
    /// Whether the run has been stopped at once already, which is what
    /// makes the first stop announce itself as an entry rather than as a
    /// step nobody asked for.
    started: bool,
    /// Whether a `quit` (or an end of input) asked for the halt, which is
    /// what tells the command a `RunOutcome::Debugger` was wanted.
    quit: bool,
}

/// A `cove debug` session: the policy the machine has none of.
struct Session {
    state: Mutex<State>,
    sources: Arc<SourceMap>,
    sites: Sites,
    /// The package root, so that a path is printed the way a person would
    /// type it rather than as wherever the package happens to live.
    root: PathBuf,
    /// Whether to echo each command after its prompt. A terminal echoed it
    /// already; a script piped in did not, and a transcript that does not
    /// say what was asked is unreadable.
    echo: bool,
}

impl Session {
    fn new(package: &Package, ir: &cove_ir::Program, sources: &Arc<SourceMap>) -> Session {
        Session {
            state: Mutex::new(State {
                mode: Mode::Instruction,
                breakpoints: Vec::new(),
                armed: Vec::new(),
                next_number: 1,
                frame: 0,
                last: String::new(),
                started: false,
                quit: false,
            }),
            sources: Arc::clone(sources),
            sites: Sites::build(ir, sources),
            root: package.root.clone(),
            echo: !std::io::stdin().is_terminal(),
        }
    }

    fn quit(&self) -> bool {
        self.state.lock().expect("a lock").quit
    }
}

impl Debugger for Session {
    fn at(&self, stop: &Stop<'_>) -> Resume {
        let mut state = self.state.lock().expect("a lock");
        // A quit halts every task, not only the one that asked for it: the
        // person said stop, and a second thread still running would go on
        // producing output after the session ended.
        if state.quit {
            return Resume::Halt;
        }
        match state.wanted(stop) {
            None => Resume::Go,
            Some(why) => self.converse(&mut state, stop, why),
        }
    }
}

impl State {
    /// The per-instruction question, and the only code here that runs at
    /// every instruction.
    ///
    /// It reads four things off the stop, all of them copies the machine
    /// already had: the pc, the span, the depth and the task, and compares
    /// them against integers. It allocates nothing and reads no source.
    fn wanted(&mut self, stop: &Stop<'_>) -> Option<Why> {
        if !self.started {
            self.started = true;
            return Some(Why::Entry);
        }
        let pc = stop.pc();
        let span = stop.span();
        // Breakpoints first, so that a `next` which runs into one reports
        // the breakpoint rather than the step.
        for (site, number) in &self.armed {
            if site.pc == pc && site.span == span {
                let number = *number;
                if let Some(b) = self.breakpoints.iter_mut().find(|b| b.number == number) {
                    b.hits += 1;
                }
                return Some(Why::Breakpoint(number));
            }
        }
        match self.mode {
            Mode::Free => None,
            Mode::Instruction => Some(Why::Stepped),
            Mode::Out { depth, task } => {
                (stop.task() == task && stop.depth() < depth).then_some(Why::Returned)
            }
            Mode::Line {
                file,
                from,
                to,
                depth,
                over,
                task,
            } => {
                // Another task's instruction is not this step's, however
                // deep it stands or wherever it was written: a depth is that
                // task's own measurement of its own stack.
                if stop.task() != task {
                    return None;
                }
                let here = stop.depth();
                if here < depth {
                    return Some(Why::Returned);
                }
                if over && here > depth {
                    return None;
                }
                let same = span.file == file && from <= span.start && span.start < to;
                (!same).then_some(Why::Stepped)
            }
        }
    }
}

/// What one command did: went back to the prompt, or let the machine go.
enum Act {
    Stay,
    Go(Resume),
}

impl Session {
    /// The prompt. Runs inside `Debugger::at`, with the machine standing
    /// still for exactly as long as it takes.
    fn converse(&self, state: &mut State, stop: &Stop<'_>, why: Why) -> Resume {
        state.frame = 0;
        self.announce(stop, why);
        loop {
            let Some(line) = self.ask() else {
                // End of input is a quit. A session whose script ran out has
                // said everything it was going to say.
                state.quit = true;
                return Resume::Halt;
            };
            let line = match line.trim() {
                "" => state.last.clone(),
                text => {
                    state.last = text.to_string();
                    text.to_string()
                }
            };
            if line.is_empty() {
                continue;
            }
            match self.act(state, stop, &line) {
                Act::Stay => continue,
                Act::Go(resume) => return resume,
            }
        }
    }

    /// Prints the prompt and reads one command, echoing it when the input is
    /// not a terminal so that a piped script produces a readable transcript.
    fn ask(&self) -> Option<String> {
        print!("(cove) ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => {
                println!();
                None
            }
            Ok(_) => {
                if self.echo {
                    println!("{}", line.trim_end_matches(['\n', '\r']));
                }
                Some(line)
            }
        }
    }

    /// Where the run is, in one line, with the source line under it.
    fn announce(&self, stop: &Stop<'_>, why: Why) {
        let at = self.place(stop.span());
        match why {
            Why::Entry => println!("entering {} at {at}", stop.function()),
            Why::Breakpoint(number) => {
                println!("breakpoint {number}, {} at {at}", stop.function())
            }
            Why::Stepped => println!("{} at {at}", stop.function()),
            Why::Returned => println!("returned to {} at {at}", stop.function()),
        }
        self.show_source(stop.span(), 0);
    }

    /// One command.
    fn act(&self, state: &mut State, stop: &Stop<'_>, line: &str) -> Act {
        let mut words = line.split_whitespace();
        let Some(head) = words.next() else {
            return Act::Stay;
        };
        let rest: Vec<&str> = words.collect();
        match head {
            "continue" | "cont" | "c" => {
                state.mode = Mode::Free;
                Act::Go(Resume::Go)
            }
            "stepi" | "si" => {
                state.mode = Mode::Instruction;
                Act::Go(Resume::Go)
            }
            "step" | "s" => {
                state.mode = self.line_mode(stop, false);
                Act::Go(Resume::Go)
            }
            "next" | "n" => {
                state.mode = self.line_mode(stop, true);
                Act::Go(Resume::Go)
            }
            "finish" | "fin" => self.finish(state, stop),
            "quit" | "q" => {
                state.quit = true;
                Act::Go(Resume::Halt)
            }
            "break" | "b" => {
                self.set_break(state, &rest);
                Act::Stay
            }
            "delete" | "d" => {
                self.delete(state, &rest);
                Act::Stay
            }
            "info" | "i" => {
                self.info(state, stop, &rest);
                Act::Stay
            }
            "backtrace" | "bt" | "where" => {
                self.backtrace(stop);
                Act::Stay
            }
            "frame" | "f" => {
                self.select(state, stop, &rest);
                Act::Stay
            }
            "list" | "l" => {
                self.list(state, stop, &rest);
                Act::Stay
            }
            "print" | "p" => {
                self.print_local(state, stop, &rest);
                Act::Stay
            }
            "locals" => {
                self.locals(state, stop);
                Act::Stay
            }
            "words" => {
                self.words(state, stop);
                Act::Stay
            }
            "disassemble" | "disas" => {
                self.disassemble(state, stop, &rest);
                Act::Stay
            }
            "object" | "o" => {
                self.object(state, stop, &rest);
                Act::Stay
            }
            "help" | "h" | "?" => {
                match rest.first() {
                    Some(&"limits") => print!("{}", Session::misses()),
                    _ => print!("{HELP}"),
                }
                Act::Stay
            }
            other => {
                println!("unknown command `{other}`; `help` lists them");
                Act::Stay
            }
        }
    }

    /// The mode a `step` or a `next` leaves behind: the byte range of the
    /// line the step was asked from, and the depth it was asked at.
    ///
    /// The range is found by scanning outwards from the instruction's own
    /// offset, which reads exactly one line of text, once. `SourceMap`
    /// exposes a line's *number* and its *text* but not where it begins, and
    /// asking for the number per instruction is the search this avoids.
    fn line_mode(&self, stop: &Stop<'_>, over: bool) -> Mode {
        let span = stop.span();
        let text = &self.sources.get(span.file).text;
        let at = (span.start as usize).min(text.len());
        let from = text[..at].rfind('\n').map_or(0, |end| end + 1);
        let to = text[at..].find('\n').map_or(text.len(), |end| at + end + 1);
        Mode::Line {
            file: span.file,
            from: from as u32,
            to: to as u32,
            depth: stop.depth(),
            over,
            task: stop.task(),
        }
    }

    /// `finish`: run until the *selected* frame returns, which is what makes
    /// `frame 2` and then `finish` mean "get me out of that one".
    ///
    /// The frame is one of this task's, so the mode remembers which task
    /// that was: another task standing one frame deep is not this frame
    /// returning, and before `Stop::task` existed it could not be told from
    /// one.
    fn finish(&self, state: &mut State, stop: &Stop<'_>) -> Act {
        let depth = stop.depth().saturating_sub(state.frame);
        if depth <= 1 {
            println!(
                "frame #{} is the outermost; the run will finish",
                state.frame
            );
        }
        state.mode = Mode::Out {
            depth,
            task: stop.task(),
        };
        Act::Go(Resume::Go)
    }

    // -- breakpoints ------------------------------------------------------

    fn set_break(&self, state: &mut State, rest: &[&str]) {
        let Some(spec) = rest.first() else {
            println!("`break` takes `<file>:<line>` or a function name");
            return;
        };
        let where_ = match spec.rsplit_once(':') {
            Some((file, line)) if line.chars().all(|c| c.is_ascii_digit()) && !line.is_empty() => {
                match self.sites_on_line(file, line.parse().unwrap_or(0)) {
                    Ok(sites) => sites,
                    Err(message) => {
                        println!("{message}");
                        return;
                    }
                }
            }
            _ => match self.sites_in_function(spec) {
                Ok(sites) => sites,
                Err(message) => {
                    println!("{message}");
                    return;
                }
            },
        };

        let number = state.next_number;
        state.next_number += 1;
        println!(
            "breakpoint {number} at {} location{}:",
            where_.len(),
            if where_.len() == 1 { "" } else { "s" }
        );
        for (site, function) in &where_ {
            println!("  {function} pc {} at {}", site.pc, self.place(site.span));
        }
        state.breakpoints.push(Breakpoint {
            number,
            spec: (*spec).to_string(),
            where_,
            hits: 0,
        });
        state.rearm();
    }

    /// Resolves `<file>:<line>`, distinguishing "there is no such file" from
    /// "the entry reaches no code written there".
    ///
    /// The second is the case `lower_entry` creates and it is worth saying
    /// out loud: what was lowered is what the entry reaches, so a line in a
    /// function no path leads to has no instructions at all, and accepting
    /// the breakpoint silently would be promising a stop that can never
    /// happen.
    fn sites_on_line(&self, file: &str, line: usize) -> Result<Vec<(Site, String)>, String> {
        let mut matched: Vec<FileId> = Vec::new();
        for source in self.sources.files() {
            let shown = self.relative(&source.path);
            if shown == file || shown.ends_with(&format!("/{file}")) {
                matched.push(source.id);
            }
        }
        let id = match matched.as_slice() {
            [] => {
                let known: Vec<String> = self
                    .sources
                    .files()
                    .map(|source| self.relative(&source.path))
                    .collect();
                return Err(format!(
                    "no source file of this package is called `{file}`\n  it holds: {}",
                    known.join(", ")
                ));
            }
            [one] => *one,
            many => {
                let known: Vec<String> = many
                    .iter()
                    .map(|id| self.relative(self.sources.path(*id)))
                    .collect();
                return Err(format!(
                    "`{file}` names {} files; write more of the path: {}",
                    many.len(),
                    known.join(", ")
                ));
            }
        };
        let sites = self.sites.at(id, line);
        if sites.is_empty() {
            let shown = self.relative(self.sources.path(id));
            return Err(match self.sites.stub_on(id, line, &self.sources) {
                Some(name) => format!(
                    "no instruction was lowered for {shown}:{line}\n  \
                     `{name}` is declared there, but this entry does not reach it"
                ),
                None => format!(
                    "no instruction was lowered for {shown}:{line}\n  \
                     `cove debug` lowers what the entry reaches, so a line in code no path \
                     leads to has no instructions to stop at, and neither has a line that is \
                     blank, a comment, or a declaration"
                ),
            });
        }
        Ok(sites.to_vec())
    }

    /// Resolves a function name to its first instruction.
    fn sites_in_function(&self, spec: &str) -> Result<Vec<(Site, String)>, String> {
        let suffix = format!(".{spec}");
        let found: Vec<(Site, String)> = self
            .sites
            .entries
            .iter()
            .filter(|(_, function)| function == spec || function.ends_with(&suffix))
            .cloned()
            .collect();
        if !found.is_empty() {
            return Ok(found);
        }
        // Declared, but not reached. The distinction the lowering creates,
        // said in the words the lowering makes true.
        if let Some((_, name)) = self
            .sites
            .stubs
            .iter()
            .find(|(_, name)| name == spec || name.ends_with(&suffix))
        {
            return Err(format!(
                "`{name}` is declared, but this entry does not reach it: `cove debug` lowers \
                 only what the entry can call, so it has no instructions to break on"
            ));
        }
        Err(format!(
            "no lowered function is called `{spec}`\n  \
             name it as `name` or `module.name`, or set the breakpoint by `<file>:<line>`"
        ))
    }

    fn delete(&self, state: &mut State, rest: &[&str]) {
        if rest.is_empty() {
            let gone = state.breakpoints.len();
            state.breakpoints.clear();
            state.rearm();
            println!("deleted {gone} breakpoint(s)");
            return;
        }
        for spec in rest {
            match spec.parse::<usize>() {
                Ok(number) if state.breakpoints.iter().any(|b| b.number == number) => {
                    state.breakpoints.retain(|b| b.number != number);
                    println!("deleted breakpoint {number}");
                }
                Ok(number) => println!("there is no breakpoint {number}"),
                Err(_) => println!("`delete` takes breakpoint numbers, found `{spec}`"),
            }
        }
        state.rearm();
    }

    fn info(&self, state: &State, stop: &Stop<'_>, rest: &[&str]) {
        match rest.first().copied().unwrap_or("breakpoints") {
            "breakpoints" | "break" | "b" => {
                if state.breakpoints.is_empty() {
                    println!("no breakpoints");
                    return;
                }
                for b in &state.breakpoints {
                    println!(
                        "breakpoint {} at `{}`, hit {} time(s)",
                        b.number, b.spec, b.hits
                    );
                    for (site, function) in &b.where_ {
                        println!("  {function} pc {} at {}", site.pc, self.place(site.span));
                    }
                }
            }
            // The count and the depth are both the *stopping task's* — a
            // spawned task runs on a machine of its own and counts its own
            // instructions — so the task is named beside them. Without the
            // name the two numbers are unreadable in a program that spawns
            // anything: a count that jumps and a depth that changes are a
            // second task's, and nothing said so.
            "run" | "r" => {
                println!(
                    "{} instruction(s) run in task {}, {} frame(s) live, frame #{} selected",
                    stop.instructions(),
                    stop.task(),
                    stop.depth(),
                    state.frame
                );
            }
            other => println!("`info` takes `breakpoints` or `run`, found `{other}`"),
        }
    }

    // -- looking ----------------------------------------------------------

    fn backtrace(&self, stop: &Stop<'_>) {
        for (at, call) in stop.backtrace().iter().enumerate() {
            println!("#{at}  {} at {}", call.function(), self.place(call.span()));
        }
    }

    fn select(&self, state: &mut State, stop: &Stop<'_>, rest: &[&str]) {
        if let Some(spec) = rest.first() {
            match spec.parse::<usize>() {
                Ok(at) if at < stop.depth() => state.frame = at,
                Ok(at) => {
                    println!(
                        "there is no frame #{at}; the stack is {} frame(s) deep",
                        stop.depth()
                    );
                    return;
                }
                Err(_) => {
                    println!("`frame` takes a frame number, found `{spec}`");
                    return;
                }
            }
        }
        let Some(call) = stop.frame(state.frame) else {
            println!("there is no frame #{}", state.frame);
            return;
        };
        println!(
            "#{}  {} at {}",
            state.frame,
            call.function(),
            self.place(call.span())
        );
        self.show_source(call.span(), 0);
    }

    fn list(&self, state: &State, stop: &Stop<'_>, rest: &[&str]) {
        let reach = match rest.first() {
            Some(spec) => match spec.parse::<usize>() {
                Ok(reach) => reach,
                Err(_) => {
                    println!("`list` takes how many lines either side, found `{spec}`");
                    return;
                }
            },
            None => 3,
        };
        let Some(call) = stop.frame(state.frame) else {
            println!("there is no frame #{}", state.frame);
            return;
        };
        self.show_source(call.span(), reach);
    }

    fn print_local(&self, state: &State, stop: &Stop<'_>, rest: &[&str]) {
        let Some(name) = rest.first() else {
            println!("`print` takes the name of a local");
            return;
        };
        let Some(call) = stop.frame(state.frame) else {
            println!("there is no frame #{}", state.frame);
            return;
        };
        match call.local(name) {
            // The raw words come out beside the rendering, because a
            // rendering is where a reader stops and a debugger is not: a
            // name bound to a vector reads as its elements and *holds* a
            // reference, and the word printed here is the word `object`
            // takes. `object <name>` follows it without being told, and this
            // is what makes that word something a person can also see, copy,
            // and compare with what `words` says about the frame.
            Some(local) => {
                let raw: Vec<String> = local
                    .words()
                    .iter()
                    .map(|word| format!("{:#018x}", word.raw()))
                    .collect();
                println!(
                    "{} = {}  (word {})  {}",
                    local.name(),
                    local.value(),
                    local.at(),
                    raw.join(" ")
                );
            }
            None => {
                let names: Vec<&str> = call.locals().iter().map(Local::name).collect();
                println!(
                    "no local called `{name}` is in scope in frame #{} ({})\n  in scope: {}",
                    state.frame,
                    call.function(),
                    if names.is_empty() {
                        "(none)".to_string()
                    } else {
                        names.join(", ")
                    }
                );
            }
        }
    }

    fn locals(&self, state: &State, stop: &Stop<'_>) {
        let Some(call) = stop.frame(state.frame) else {
            println!("there is no frame #{}", state.frame);
            return;
        };
        if call.locals().is_empty() {
            println!("no names are in scope in frame #{}", state.frame);
            return;
        }
        for (at, local) in call.locals().iter().enumerate() {
            // Two bindings of one name may be live at once, because
            // shadowing is recorded rather than resolved. The later one is
            // what the source means here; the earlier is still in the frame
            // and is shown, marked, rather than hidden.
            let shadowed = call.locals()[at + 1..]
                .iter()
                .any(|later| later.name() == local.name());
            println!(
                "{} = {}  (word {}, {} word(s){})",
                local.name(),
                local.value(),
                local.at(),
                local.width(),
                if shadowed { ", shadowed" } else { "" }
            );
        }
    }

    fn words(&self, state: &State, stop: &Stop<'_>) {
        let Some(call) = stop.frame(state.frame) else {
            println!("there is no frame #{}", state.frame);
            return;
        };
        if call.words().is_empty() {
            println!("every word of frame #{} has a name", state.frame);
            return;
        }
        for word in call.words() {
            println!(
                "word {:<3} {:<9} {:#018x}  {}",
                word.at(),
                word.holds(),
                word.raw(),
                word.value()
            );
        }
    }

    fn disassemble(&self, state: &State, stop: &Stop<'_>, rest: &[&str]) {
        let reach = match rest.first() {
            Some(spec) => match spec.parse::<usize>() {
                Ok(reach) => reach,
                Err(_) => {
                    println!(
                        "`disassemble` takes how many instructions either side, found `{spec}`"
                    );
                    return;
                }
            },
            None => 4,
        };
        // The selected frame's, like everything else `frame` selects: a
        // person who has selected a frame and asks what it is executing is
        // asking about that frame. The marked line is the instruction about
        // to run in the innermost frame and the one to return to in every
        // other, which is what `backtrace` already says of a frame's pc.
        let Some(call) = stop.frame(state.frame) else {
            println!("there is no frame #{}", state.frame);
            return;
        };
        println!("{}:", call.function());
        for line in stop.code(state.frame, reach) {
            println!(
                "{} {:>4} | {}",
                if line.current() { "=>" } else { "  " },
                line.pc(),
                line.text()
            );
        }
    }

    /// `object <word>`, and `object <name>` for a reference a name holds.
    ///
    /// The second is what makes a named reference followable. `words` prints
    /// the frame words *no* name covers, so before `Local::words` existed
    /// the only reference a person could follow was one nothing was called —
    /// `print xs` would render a vector and there was no way to then ask
    /// what the object was. Resolving the name here rather than making the
    /// person copy the word is the difference between a hole and a step.
    fn object(&self, state: &State, stop: &Stop<'_>, rest: &[&str]) {
        let Some(spec) = rest.first() else {
            println!("`object` takes a word, as `words` and `print` print one, or the name of a local of the selected frame");
            return;
        };
        let word = match spec.strip_prefix("0x").or_else(|| spec.strip_prefix("0X")) {
            Some(hex) => match u64::from_str_radix(hex, 16) {
                Ok(word) => Some(word),
                Err(_) => {
                    println!("`object` takes a word in decimal or as `0x…`, found `{spec}`");
                    None
                }
            },
            // Not a number, so it is a name. No Cove name is a number, so
            // the two cannot be confused for one another.
            None => match spec.parse::<u64>() {
                Ok(word) => Some(word),
                Err(_) => self.reference_of(state, stop, spec),
            },
        };
        let Some(word) = word else {
            return;
        };
        match stop.object(word) {
            Some(object) => {
                println!("{:#018x}: {}", word, object.name());
                for field in object.fields() {
                    println!("  {} = {}", field.name(), field.value());
                }
            }
            None => println!("{word:#018x} does not name an object of this run's heap"),
        }
    }

    /// The word a named local holds, when exactly one of its words is a
    /// reference. Says why not, and answers `None`, when that is not so.
    ///
    /// A name covers a run of words and only some of them are references —
    /// a struct held by value is several — so "the word of a name" is a
    /// question with more than one answer in general. Where it has exactly
    /// one, that is the answer; where it has none or several, the words are
    /// printed and the person picks, which is the same conversation `words`
    /// and `object` already have about the rest of the frame.
    fn reference_of(&self, state: &State, stop: &Stop<'_>, name: &str) -> Option<u64> {
        let call = stop.frame(state.frame).or_else(|| {
            println!("there is no frame #{}", state.frame);
            None
        })?;
        let Some(local) = call.local(name) else {
            println!(
                "no local called `{name}` is in scope in frame #{} ({}), and `{name}` is not a word",
                state.frame,
                call.function()
            );
            return None;
        };
        let refs: Vec<&Word> = local
            .words()
            .iter()
            .filter(|word| word.holds() == "ref")
            .collect();
        match refs.as_slice() {
            [word] => Some(word.raw()),
            _ => {
                match refs.len() {
                    0 => println!("`{name}` holds no reference to follow; its words are:"),
                    many => {
                        println!("`{name}` holds {many} references, so name the word to follow:")
                    }
                }
                for word in local.words() {
                    println!(
                        "  word {:<3} {:<9} {:#018x}  {}",
                        word.at(),
                        word.holds(),
                        word.raw(),
                        word.value()
                    );
                }
                None
            }
        }
    }

    // -- rendering --------------------------------------------------------

    /// `path:line:col`, with the path written the way a person would type it.
    fn place(&self, span: Span) -> String {
        let file = self.sources.get(span.file);
        let (line, col) = file.line_col(span.start);
        format!("{}:{line}:{col}", self.relative(&file.path))
    }

    /// The path relative to the package root, so that a golden file and a
    /// terminal agree about what a source file is called.
    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .display()
            .to_string()
            .replace('\\', "/")
    }

    /// The source around a span, `reach` lines either side, with the span's
    /// own line marked.
    fn show_source(&self, span: Span, reach: usize) {
        let file = self.sources.get(span.file);
        let (line, _) = file.line_col(span.start);
        let last = file.text.lines().count().max(1);
        let from = line.saturating_sub(reach).max(1);
        let to = (line + reach).min(last);
        for at in from..=to {
            println!(
                "{} {:>4} | {}",
                if at == line { ">" } else { " " },
                at,
                file.line_text(at)
            );
        }
    }

    /// What the stepping rule gets wrong, said out loud.
    ///
    /// Most of these are a consequence of what a lowered program records: an
    /// expression-level span per instruction, adjacent instructions sharing
    /// one, and nothing that says where a statement begins. A rule built on
    /// "the line changed" cannot avoid them. The rest are what an all-stop
    /// debugger of a concurrent program is: one prompt, and every task
    /// standing still while somebody is at it.
    ///
    /// A limitation a person can read beats one they have to discover — and
    /// a limitation that has been fixed does not belong here at all, because
    /// a list that names what a person can now do is a list they stop
    /// reading.
    fn misses() -> &'static str {
        "\
one `step` is: run until the line changes in the task it was asked in, or
until the frame it was asked from returns. `next` also skips anything
deeper than that frame.

what that rule gets wrong:
  - a callee whose body is written on the line that calls it is stepped
    over, not into, because the line did not change;
  - a loop whose body is written on one line is one `step` for the whole
    loop, not one per turn: the line never changes, so nothing stops it;
  - a statement written across several lines stops several times, in
    evaluation order, so the line number can go backwards;
  - a stop is at the first instruction carrying a new line, which is
    somewhere inside the expression rather than at the statement's start,
    so a name assigned on that line still holds its old value;
  - `step`, `next` and `finish` are satisfied only in the task they were
    asked in, but `stepi` is not: it stops at the very next instruction the
    machine runs, which in a program with a spawned task may be that task's;
  - a breakpoint fires in whichever task reaches it, and stopping one task
    stops them all, so a program whose tasks wait on each other can be
    stopped in a state only `continue` unsticks;
  - a breakpoint set on a line resolves to the lowest instruction written
    there in each function, so a line holding two statements, or an `if`
    and its `else`, fires only on the earlier of them.
"
    }
}

impl State {
    /// Rebuilds the flattened site list the per-instruction check scans.
    fn rearm(&mut self) {
        self.armed = self
            .breakpoints
            .iter()
            .flat_map(|b| b.where_.iter().map(move |(site, _)| (*site, b.number)))
            .collect();
    }
}

const HELP: &str = "\
running:
  continue, c          run until a breakpoint
  step, s              one source line, into calls
  next, n              one source line, over calls
  stepi, si            one instruction
  finish               run until the selected frame returns
  quit, q              stop the run and leave

breakpoints:
  break <file>:<line>  stop at the lowest instruction written there
  break <function>     stop at a function's first instruction
  delete [n…]          delete those breakpoints, or all of them
  info breakpoints     what is set, and how often each has fired

looking:
  backtrace, bt        every live call, innermost first
  frame <n>, f <n>     select the frame print, locals, words and list read
  list [n], l [n]      source around the selected frame, n lines either side
  print <name>, p      one local of the selected frame, and its words
  locals               every local in scope there
  words                every frame word no name covers, for VM development
  disassemble [n]      instructions around the selected frame's pc, marked
  object <word|name>   what a word, or a reference a local holds, points at
  info run             which task, instructions run in it, frames, selection

  help limits          what `step` and `break` get wrong, and why

an empty line repeats the last command.
";

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Flags {
        let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        match parse_debug_flags(&args) {
            Ok(flags) => flags,
            Err(CliError::Message(message)) => panic!("these flags parse: {message}"),
            Err(_) => panic!("these flags parse"),
        }
    }

    /// **The budgets and the two pieces of authority are read from anywhere
    /// after the run name, and everything else is the program's.**
    #[test]
    fn a_debug_session_takes_the_run_flags_that_mean_something_to_a_stopped_run() {
        let flags = parsed(&[
            "--fuel",
            "500",
            "report",
            "--deadline",
            "5s",
            "--max-host-calls",
            "3",
            "--max-tasks",
            "2",
            "--files-root",
            "data",
            "--",
            "--fuel",
        ]);
        assert_eq!(flags.fuel, Some(500));
        assert_eq!(flags.deadline, Some(Duration::from_secs(5)));
        assert_eq!(flags.max_host_calls, Some(3));
        assert_eq!(flags.max_tasks, Some(2));
        assert_eq!(flags.files_root, Some(PathBuf::from("data")));
        assert_eq!(
            flags.program_args,
            vec!["report".to_string(), "--fuel".to_string()],
            "after `--`, a flag is an argument"
        );
    }

    /// **`--backend` is refused rather than ignored.**
    ///
    /// A debugger is a feature of the linear-memory machine. Accepting the
    /// flag and running on that machine anyway would answer a question
    /// nobody asked; treating it as a program argument would be worse, since
    /// the program would silently receive it.
    #[test]
    fn a_debug_session_refuses_a_backend_instead_of_passing_it_to_the_program() {
        let args = vec!["--backend".to_string(), "ast".to_string()];
        let Err(CliError::Message(message)) = parse_debug_flags(&args) else {
            panic!("`--backend` is refused");
        };
        assert!(message.contains("takes no `--backend`"), "{message}");
    }

    /// **`--allow-exec` still takes an absolute path.**
    ///
    /// The same rule `cove run` holds: an allow-list resolved against
    /// whatever directory the command happened to start in is not an
    /// allow-list anybody can be sure of.
    #[test]
    fn allow_exec_refuses_a_relative_path_here_as_it_does_for_a_run() {
        let args = vec!["--allow-exec".to_string(), "bin/tool".to_string()];
        let Err(CliError::Message(message)) = parse_debug_flags(&args) else {
            panic!("a relative `--allow-exec` is refused");
        };
        assert!(message.contains("absolute path"), "{message}");
    }
}
