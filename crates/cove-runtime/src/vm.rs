//! The VM that runs the executable IR.
//!
//! [ADR 0019](../../../docs/adr/0019-executable-ir-and-vm.md) decides why this
//! exists. A tree-walking interpreter re-derives, on every evaluation, facts
//! that were settled before the program ran — where a binding lives, what a
//! call targets, how big a frame is — and a tree has nowhere to put an answer
//! so that using it costs nothing. [`cove_ir`] put those answers into the
//! instructions; this runs them.
//!
//! # The interpreter is the oracle, so nothing here decides anything
//!
//! ADR 0012 ranks the specification above the oracle above a backend, and ADR
//! 0019 keeps that arrangement: [`crate::interp`] is what a Cove program
//! means, and this is a second way of arriving at the same answer. So every
//! value operation here calls the interpreter's own code rather than
//! restating it — `binary`, `unary`, [`crate::builtins::call_method`],
//! [`crate::builtins::call_constructor`], and the one [`HostRegistry`] both
//! backends dispatch through. What is written here is the execution model and
//! nothing else: a stack, a frame, and a dispatch loop.
//!
//! Where an answer is not the interpreter's to give, it is the checker's. A
//! condition that is not a `Bool` reports what `cove-sema` reports, because
//! the IR does not say whether the jump it lowered to came from an `if`, a
//! `while`, or a `&&`, and the checker refuses such a program before any of
//! them is lowered.
//!
//! # One stack, and a frame is a region of it
//!
//! Every frame's slots and every frame's operands live in one contiguous
//! `Vec<Value>`. A call does not allocate: the arguments are already on the
//! stack, in order, and they *become* the callee's first slots. That is the
//! whole point of the exercise — issue #104 measured a minimal call at
//! 650–790 ns, most of it building an environment and tearing it down.
//!
//! # What is not here
//!
//! Closures, tasks, `var` places, and everything else `cove_ir::lower`
//! reports as [`cove_ir::Unsupported`]. ADR 0019's no-silent-fallback rule is
//! what makes that the right shape: a program the lowering refuses never
//! reaches this, so there is no construct this can be wrong about.

use std::rc::Rc;
use std::time::{Duration, Instant};

use cove_diag::{SourceMap, Span};
use cove_ir::{
    BinaryOp as IrBinary, Const, ConstId, Function, FunctionId, Inst, Program, UnaryOp as IrUnary,
};
use cove_schema::builtins::{free_builtin, FreeBuiltinKind, NONE_CASE, OPTION, RESULT};
use cove_syntax::ast::{BinaryOp, UnaryOp};

use crate::budget::{Cancellation, Stopped};
use crate::builtins::{self, Callable};
use crate::error::RuntimeError;
use crate::heap::{Heap, HeapStats};
use crate::host::{HostRegistry, Reentry};
use crate::interp::{
    binary, no_field, not_a_struct, returned_error_message, source_text, unary, work_stopped,
    MAX_CALL_DEPTH,
};
use crate::runtime::{Runtime, ENTRY_TASK};
use crate::trace::{RunOutcome, Timing, TraceEvent};
use crate::value::{StructValue, Value};

/// Fuel charged for executing one instruction.
///
/// ADR 0019 requires fuel to be charged for VM work, and accepts that the two
/// backends will not report the same `fuel_spent` for the same program:
/// instructions are not AST nodes and there is no honest mapping between
/// them. What must hold on both is the property fuel exists for — a run that
/// exceeds its budget stops, deterministically, at a point the program can be
/// told about — and an instruction is the unit of work this backend can
/// promise that about.
const INSTRUCTION_FUEL: u64 = 1;

/// How much fuel may accumulate between safepoints before one is forced.
///
/// Fuel is charged per instruction and spent against the shared
/// [`crate::budget::Budget`] at a safepoint, so a long stretch of
/// straight-line instructions would otherwise hold its charge until the next
/// call or back edge. A cap keeps "a run that exceeds its budget stops" a
/// statement about the run rather than about its loops.
const SAFEPOINT_INTERVAL: u64 = 1024;

/// One call in progress.
///
/// The three numbers are what a return needs and nothing more, which is why a
/// call costs a push here rather than an allocation.
#[derive(Clone, Copy)]
struct Frame {
    /// The function whose instructions are running.
    function: FunctionId,
    /// Where the caller resumes: the instruction after its `Call`.
    return_pc: usize,
    /// Where this frame's slots begin in the value stack. Its slots are
    /// `stack[base .. base + frame_size]`, and its operands sit above them.
    ///
    /// This is also the caller's operand top, because the arguments were
    /// pushed onto the caller's stack and then became this frame's first
    /// slots without moving. A return truncates to `base`, which is that one
    /// fact read from the other side.
    base: usize,
}

/// What a `MakeStruct` builds, worked out once for the whole run.
///
/// Splitting the field list and asking the checker whether the type is opaque
/// are facts about a declaration rather than about an execution, so they are
/// settled where every other such fact is: before the first instruction runs.
struct StructShape {
    type_name: Rc<str>,
    fields: Vec<Rc<str>>,
    /// Whether the declaration was `export opaque struct`, which is what
    /// makes a value of it render as its name alone (ADR 0014). The IR
    /// carries the type's qualified name and the checker knows the rest, so
    /// this is read from the checker exactly as `Interpreter::init_struct`
    /// reads it.
    opaque: bool,
}

/// Runs a lowered program.
///
/// One VM runs one entry on one thread. Everything shared with the rest of
/// the run — the checked program, the source map, the host boundary, the
/// trace — is reached through the [`Runtime`] it borrows, which is the
/// arrangement [`crate::interp::Interpreter`] already has.
pub struct Vm<'a> {
    runtime: &'a Runtime,
    hosts: &'a HostRegistry,
    program: &'a Program,
    /// The run's sources, for the one diagnostic that quotes source text: a
    /// failing assertion names its condition in the words the test was
    /// written in. Read off the [`Runtime`] exactly as
    /// [`crate::interp::Interpreter`] reads it.
    sources: &'a SourceMap,
    /// One contiguous value stack shared by every frame: slots below,
    /// operands above.
    stack: Vec<Value>,
    frames: Vec<Frame>,
    /// One entry per constant, filled for the constants a `MakeStruct` names
    /// its type with and empty everywhere else.
    shapes: Vec<Option<StructShape>>,
    /// This run's heap.
    ///
    /// Nothing the lowering covers allocates growable storage — `Vector.of`
    /// is an associated function of a builtin type and `push` writes through
    /// its receiver, and both are refused — so this exists to answer
    /// [`Callable::allocate_vector`] honestly rather than to be swept. There
    /// is no collection here for the same reason there is nothing to collect:
    /// a cycle needs a `Vector` that reaches itself, and nothing in the
    /// subset can build one.
    heap: Heap,
    /// Fuel charged since the last safepoint, spent at the next one.
    fuel: u64,
    /// Flags raised by a host call that bounds the work it was given, one for
    /// each such call this thread is inside, checked at every safepoint
    /// exactly as [`crate::interp::Interpreter`] checks them.
    stops: Vec<Cancellation>,
    /// Active timing contexts, one for the body this VM is running. A host
    /// call's wait is charged against every one, which is what lets
    /// `EntryExit` separate the work an entry did from the time it spent
    /// waiting for a host to answer.
    timings: Vec<Timing>,
    /// What the last finished run spent waiting on hosts, kept after its
    /// timing context ended so a caller can read it back.
    wait: Duration,
    /// Where the most recent assertion failed, and the message it produced.
    ///
    /// A failed assertion is an ordinary `Err`, which carries a message and
    /// no source position; this is the position, recorded by the one party
    /// that saw the assertion, so a test runner can point at it.
    assertion_failure: Option<(Span, String)>,
}

impl<'a> Vm<'a> {
    /// A VM for `program`, running against `runtime` and calling through
    /// `hosts`.
    pub fn new(runtime: &'a Runtime, hosts: &'a HostRegistry, program: &'a Program) -> Self {
        Vm {
            runtime,
            hosts,
            program,
            sources: runtime.sources(),
            stack: Vec::new(),
            frames: Vec::new(),
            shapes: struct_shapes(runtime, program),
            heap: Heap::new(),
            fuel: 0,
            stops: Vec::new(),
            timings: Vec::new(),
            wait: Duration::ZERO,
            assertion_failure: None,
        }
    }

    /// Runs `function` with `args`, answering what the entry answered.
    ///
    /// The arguments arrive on the operand stack, pushed left to right, and
    /// become the first slots of the frame — which is what a `Call` does,
    /// done by hand here because there is no caller to have done it.
    pub fn run(&mut self, function: FunctionId, args: Vec<Value>) -> Result<Value, RuntimeError> {
        let entry = self.program.function(function);
        if args.len() as u32 != entry.arity {
            return Err(RuntimeError::new(format!(
                "`{}.{}` takes {} argument(s), but {} were given",
                entry.module,
                entry.name,
                entry.arity,
                args.len()
            ))
            .at(entry.span));
        }
        self.stack.clear();
        self.frames.clear();
        self.fuel = 0;
        self.stack.extend(args);
        self.stack.resize(entry.frame_size as usize, Value::Unit);
        self.frames.push(Frame {
            function,
            return_pc: 0,
            base: 0,
        });

        // The two events an entry is bracketed by are source-level, which is
        // the only kind ADR 0019 keeps on both backends: `cove trace` reads a
        // VM run and an interpreted run the same way, and an instruction-level
        // trace would be a different artifact.
        self.runtime.trace(TraceEvent::EntryEnter {
            module: entry.module.to_string(),
            function: entry.name.to_string(),
        });
        self.timings.push(Timing::start());
        let outcome = self.execute();
        let timing = self
            .timings
            .pop()
            .expect("a run pushes exactly the one timing it pops");
        self.wait = timing.wait();
        self.runtime.trace(TraceEvent::EntryExit {
            module: entry.module.to_string(),
            function: entry.name.to_string(),
            cpu: timing.cpu(),
            wait: timing.wait(),
        });
        outcome
    }

    /// Runs the entry `module.name` on the VM, handing it the process
    /// arguments, and reports how the run ended.
    ///
    /// This is the seam a backend is chosen at.
    /// [`crate::interp::Interpreter::run_entry`] takes the same three things
    /// and answers the same way, so selecting a backend selects which of the
    /// two to build and decides nothing else. Issue #111 gates the VM's
    /// adoption on the two agreeing, and two answers reached through
    /// differently shaped calls would be comparing the calls as well.
    ///
    /// A program the lowering refused never reaches here — ADR 0019's rule is
    /// that a VM run fails before any side effect — so the only entry this
    /// cannot find is one the caller named and the lowering did not emit.
    pub fn run_entry(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        let outcome = self.invoke_entry(module, name, args);
        let (classification, message) = match &outcome {
            // Cove's entry returns `Result<Unit, Error>`, so an `Err` is the
            // program saying what it was written to say, exactly as it is on
            // the interpreter: a failure of the program's work rather than of
            // the run.
            Ok(value) if value.is_err() => (RunOutcome::Error, returned_error_message(value)),
            Ok(_) => (RunOutcome::Success, None),
            Err(error) => (error.outcome, Some(error.message.clone())),
        };
        self.runtime.trace(TraceEvent::RunEnded {
            outcome: classification,
            message,
        });
        outcome
    }

    /// The entry itself: finding it, and turning the process arguments into
    /// the one value it may take.
    ///
    /// What an entry may declare is the language's rule and not a backend's,
    /// so the shape checked here and the words it is refused in are
    /// [`crate::interp::Interpreter`]'s.
    fn invoke_entry(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        let id = self.program.function_named(module, name).ok_or_else(|| {
            RuntimeError::new(format!("this package does not declare `{module}.{name}`"))
        })?;
        let entry = self.program.function(id);
        let arguments = match entry.arity {
            0 => Vec::new(),
            1 => vec![Value::Array(args.into_iter().map(Value::Str).collect())],
            other => {
                return Err(RuntimeError::new(format!(
                    "entry `{module}.{name}` declares {other} parameters"
                ))
                .at(entry.span)
                .with_rule(
                    "An entry function takes either no parameters or one `Array<String>` of process arguments.",
                )
                .with_help(format!(
                    "write `fn {name}()` or `fn {name}(args: Array<String>)`"
                )));
            }
        };
        self.run(id, arguments)
    }

    /// What this run allocated, and what the rest of the run allocated
    /// beside it.
    ///
    /// Read exactly as [`crate::interp::Interpreter::heap_stats`] reads it:
    /// the totals are the runtime's, because every retired task folded its
    /// own into them, and the live figures are this VM's, because its heap
    /// has not been retired yet.
    pub fn heap_stats(&self) -> HeapStats {
        let mut stats = self.runtime.heap_stats();
        let mine = self.heap.stats();
        stats.live_bytes = mine.live_bytes;
        stats.live_objects = mine.live_objects;
        stats
    }

    /// Where the most recent failed assertion was written, together with the
    /// message it produced, or `None` when no assertion has failed.
    pub fn assertion_failure(&self) -> Option<(Span, &str)> {
        self.assertion_failure
            .as_ref()
            .map(|(span, message)| (*span, message.as_str()))
    }

    /// How long the last finished run spent waiting on host calls.
    ///
    /// A host call's wait is not work the program did, so it is measured
    /// where it is spent and reported apart from the rest, exactly as
    /// [`Timing`] separates the two for the interpreter.
    pub fn wait(&self) -> Duration {
        self.wait
    }

    // -------------------------------------------------------- the dispatch

    /// The loop, from the frame [`Vm::run`] pushed to the value it answers.
    ///
    /// The running function, its instructions, and its frame are held in
    /// locals rather than read back out of the frame stack on every
    /// instruction: they change only at a call and at a return, and reading
    /// them anywhere else is the re-derivation this backend exists to stop
    /// doing.
    fn execute(&mut self) -> Result<Value, RuntimeError> {
        let program = self.program;
        let mut frame = *self
            .frames
            .last()
            .expect("`run` pushes the frame this executes");
        let mut running = program.function(frame.function);
        let mut code: &[Inst] = &running.code;
        let mut pc = 0usize;

        // Entering a call is a safepoint and the entry is a call, so a run
        // that was cancelled before it began stops before its first
        // instruction — which is what `Interpreter::invoke` does for the
        // entry as well.
        self.safepoint(running.span)?;

        loop {
            let inst = code[pc];
            self.fuel += INSTRUCTION_FUEL;
            if self.fuel >= SAFEPOINT_INTERVAL {
                self.safepoint(running.span_at(pc))?;
            }
            match inst {
                Inst::Const(id) => self.stack.push(constant(program.constant(id))),
                Inst::LoadLocal(slot) => {
                    let value = self.stack[frame.base + slot as usize].clone();
                    self.stack.push(value);
                }
                Inst::StoreLocal(slot) => {
                    let value = self.pop();
                    self.stack[frame.base + slot as usize] = value;
                }
                Inst::LoadCapture(index) => {
                    // Nothing lowers a closure yet, so a lowered function
                    // carries no captures and `cove_ir::lower::validate`
                    // refuses every index into the empty list. Reporting is
                    // what is left: there is no capture storage to read, and
                    // inventing one would be inventing a representation the
                    // IR has not decided on.
                    return Err(RuntimeError::new(format!(
                        "capture {index} was asked for, but this call was given none"
                    ))
                    .at(running.span_at(pc))
                    .with_rule(
                        "A closure's captures are an explicit list, decided when it is lowered.",
                    ));
                }
                Inst::Pop => {
                    self.pop();
                }
                Inst::Dup => {
                    let value = self
                        .stack
                        .last()
                        .expect("`dup` has a value to copy")
                        .clone();
                    self.stack.push(value);
                }
                Inst::Unary(op) => {
                    let value = self.pop();
                    let answer = unary(unary_op(op), value, running.span_at(pc))?;
                    self.stack.push(answer);
                }
                Inst::Binary(op) => {
                    let rhs = self.pop();
                    let lhs = self.pop();
                    // Comparing two strings and comparing two integers are
                    // one instruction and not one cost, so the operand's own
                    // size is charged beside the instruction.
                    self.fuel += size_of_value(&lhs);
                    let answer = binary(binary_op(op), lhs, rhs, running.span_at(pc))?;
                    self.stack.push(answer);
                }
                Inst::Jump(to) => {
                    if to as usize <= pc {
                        self.safepoint(running.span_at(pc))?;
                    }
                    pc = to as usize;
                    continue;
                }
                Inst::JumpIfFalse(to) | Inst::JumpIfTrue(to) => {
                    let taken_on = matches!(inst, Inst::JumpIfTrue(_));
                    let test = self.pop();
                    let Value::Bool(test) = test else {
                        return Err(not_a_condition(&test, running.span_at(pc)));
                    };
                    if test == taken_on {
                        if to as usize <= pc {
                            self.safepoint(running.span_at(pc))?;
                        }
                        pc = to as usize;
                        continue;
                    }
                }
                Inst::Call {
                    function: target,
                    argc,
                } => {
                    let span = running.span_at(pc);
                    let callee = program.function(target);
                    self.enter(callee, span)?;
                    let base = self.stack.len() - argc as usize;
                    self.stack
                        .resize(base + callee.frame_size as usize, Value::Unit);
                    frame = Frame {
                        function: target,
                        return_pc: pc + 1,
                        base,
                    };
                    self.frames.push(frame);
                    running = callee;
                    code = &callee.code;
                    pc = 0;
                    continue;
                }
                Inst::CallHost { module, op, argc } => {
                    let span = running.span_at(pc);
                    let module = name(program, module);
                    let op = name(program, op);
                    let values = self.take(argc as usize);
                    let value = self.call_host(module, op, values, span)?;
                    self.stack.push(value);
                }
                Inst::CallBuiltin { name: method, argc } => {
                    let span = running.span_at(pc);
                    let method = name(program, method);
                    let values = self.take(argc as usize);
                    let receiver = self.pop();
                    let value = builtins::call_method(self, &receiver, method, values, span)?;
                    // A builtin's cost follows what it produced: `chars()`
                    // builds one element per character, and charging for the
                    // instruction alone would price it like `length()`.
                    self.fuel += size_of_value(&value);
                    self.stack.push(value);
                }
                Inst::MakeArray(len) => {
                    let at = self.stack.len() - len as usize;
                    let items: Rc<[Value]> = self.stack.drain(at..).collect();
                    self.fuel += u64::from(len);
                    self.stack.push(Value::Array(items));
                }
                Inst::Concat(parts) => {
                    let at = self.stack.len() - parts as usize;
                    let mut text = String::new();
                    for value in &self.stack[at..] {
                        // The same rendering interpolation uses, because it is
                        // the same operation: `Display` is what `"{x}"` means,
                        // and a literal part is a `Str` that renders as itself.
                        text.push_str(&value.to_string());
                    }
                    self.stack.truncate(at);
                    self.fuel += u64::from(parts) + text.len() as u64;
                    self.stack.push(Value::Str(text.into()));
                }
                Inst::MakeStruct { ty, .. } => {
                    let shape = self.shapes[ty.0 as usize]
                        .as_ref()
                        .expect("every `make-struct` names a type this VM shaped");
                    let width = shape.fields.len();
                    let at = self.stack.len() - width;
                    let fields: Vec<(Rc<str>, Value)> = shape
                        .fields
                        .iter()
                        .cloned()
                        .zip(self.stack.drain(at..))
                        .collect();
                    let value = Value::Struct(Rc::new(StructValue {
                        type_name: shape.type_name.clone(),
                        fields,
                        opaque: shape.opaque,
                    }));
                    self.fuel += width as u64;
                    self.stack.push(value);
                }
                Inst::GetField(field) => {
                    let span = running.span_at(pc);
                    let field = name(program, field);
                    let base_value = self.pop();
                    let Value::Struct(held) = &base_value else {
                        return Err(RuntimeError::new(format!(
                            "`{}` has no field `{field}`",
                            base_value.type_name()
                        ))
                        .at(span));
                    };
                    let Some(found) = held.get(field) else {
                        return Err(no_field(&held.type_name, field, span));
                    };
                    let found = found.clone();
                    self.stack.push(found);
                }
                Inst::SetField(field) => {
                    let span = running.span_at(pc);
                    let field = name(program, field);
                    let value = self.pop();
                    let target = self.pop();
                    let Value::Struct(mut held) = target else {
                        return Err(not_a_struct(&target, field, span));
                    };
                    let type_name = held.type_name.clone();
                    // `make_mut` copies when another holder exists and does
                    // nothing when none does, which is what makes sharing a
                    // copied struct unobservable. It is the call
                    // `Place::with_mut` makes, for the same reason.
                    let Some(slot) = Rc::make_mut(&mut held).get_mut(field) else {
                        return Err(no_field(&type_name, field, span));
                    };
                    *slot = value;
                    self.fuel += held.fields.len() as u64;
                    self.stack.push(Value::Struct(held));
                }
                Inst::MakeBuiltin { name: which, argc } => {
                    let span = running.span_at(pc);
                    let which = name(program, which);
                    let values = self.take(argc as usize);
                    let value = self.make_builtin(which, values, running.arg_spans_at(pc), span)?;
                    self.stack.push(value);
                }
                Inst::Try => {
                    let span = running.span_at(pc);
                    let value = self.pop();
                    match opened(value, span)? {
                        Ok(payload) => self.stack.push(payload),
                        Err(failure) => {
                            self.safepoint(span)?;
                            match self.leave(failure) {
                                Answer::Done(value) => return Ok(value),
                                Answer::Caller(caller, resumed) => {
                                    frame = caller;
                                    running = program.function(frame.function);
                                    code = &running.code;
                                    pc = resumed;
                                    continue;
                                }
                            }
                        }
                    }
                }
                Inst::Return => {
                    self.safepoint(running.span_at(pc))?;
                    let value = self.pop();
                    match self.leave(value) {
                        Answer::Done(value) => return Ok(value),
                        Answer::Caller(caller, resumed) => {
                            frame = caller;
                            running = program.function(frame.function);
                            code = &running.code;
                            pc = resumed;
                            continue;
                        }
                    }
                }
            }
            pc += 1;
        }
    }

    // ------------------------------------------------------- stack and frames

    /// The top of the operand stack.
    ///
    /// `cove_ir::lower::validate` simulated the depth of every instruction
    /// control can reach before the VM was handed the program, so an empty
    /// stack here is a broken invariant rather than a program that could be
    /// told about it.
    fn pop(&mut self) -> Value {
        self.stack
            .pop()
            .expect("a validated instruction takes only values that are there")
    }

    /// The top `count` values, in the order they were pushed.
    fn take(&mut self, count: usize) -> Vec<Value> {
        let at = self.stack.len() - count;
        self.stack.drain(at..).collect()
    }

    /// Checks what a call is allowed to do before it does it.
    ///
    /// The three checks, in the order `Interpreter::invoke` makes them: the
    /// unconditional depth limit, the host's own `max_call_depth`, and the
    /// safepoint, because every call is one.
    fn enter(&mut self, callee: &Function, span: Span) -> Result<(), RuntimeError> {
        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(RuntimeError::new(format!(
                "call depth limit of {MAX_CALL_DEPTH} reached while calling `{}`",
                callee.name
            ))
            .at(span)
            .with_rule("Recursion depth is a runtime control, not a proof obligation."));
        }
        let depth = self.frames.len() + 1;
        if let Some(Some(error)) =
            self.hosts
                .with_budget(|budget| match budget.limits().max_call_depth {
                    Some(limit) if depth > limit => {
                        Some(budget.to_runtime_error(Stopped::CallDepth))
                    }
                    _ => None,
                })
        {
            return Err(error.at(span));
        }
        self.safepoint(span)
    }

    /// Pops the running frame and hands `value` back to whoever called it.
    ///
    /// The stack is truncated to the frame's base, which is where the
    /// caller's operands ended before it pushed the arguments, so the value
    /// lands exactly where the caller expects it.
    fn leave(&mut self, value: Value) -> Answer {
        let done = self.frames.pop().expect("a return leaves a frame");
        self.stack.truncate(done.base);
        match self.frames.last().copied() {
            Some(caller) => {
                self.stack.push(value);
                Answer::Caller(caller, done.return_pc)
            }
            None => Answer::Done(value),
        }
    }

    // ------------------------------------------------------------- budget

    /// Spends the fuel charged since the last safepoint, and checks the
    /// deadline, the run's cancellation, and every bounded call this thread
    /// is inside.
    ///
    /// This is `Interpreter::charge_safepoint` with the same budget and the
    /// same order of questions; only what is charged differs, because what
    /// this backend does between two safepoints is instructions rather than
    /// AST nodes. A stop surfaces as the ordinary [`RuntimeError`]
    /// [`crate::budget::Budget`] already produces, pointing at the
    /// instruction that reached the limit.
    ///
    /// # Where the safepoints are
    ///
    /// Every backward jump and every call, which are the interpreter's loop
    /// back edges and calls; every return, which is where the fuel of a body
    /// that made neither is finally spent; entering the entry, so a run
    /// cancelled before it began stops before its first instruction; and
    /// [`SAFEPOINT_INTERVAL`] instructions of anything else, so a long
    /// straight line is bounded too.
    fn safepoint(&mut self, span: Span) -> Result<(), RuntimeError> {
        // A bounded call's flag stops only the body it bounds. The host that
        // raised it turns the stop into the answer it promised, so this need
        // only say that the body is not to continue.
        if self.stops.iter().any(Cancellation::is_cancelled) {
            return Err(work_stopped(span));
        }
        let fuel = std::mem::take(&mut self.fuel);
        if let Some(Err(error)) = self.hosts.with_budget(|budget| {
            budget
                .safepoint(fuel)
                .map_err(|stopped| budget.to_runtime_error(stopped))
        }) {
            return Err(error.at(span));
        }
        Ok(())
    }

    /// Records `wait` against every active [`Timing`] context, so a trace can
    /// separate the work a body did from the time it spent waiting for a host
    /// to answer.
    fn charge_wait(&mut self, wait: Duration) {
        for timing in &mut self.timings {
            timing.add_wait(wait);
        }
    }

    // -------------------------------------------------------------- calls

    /// Dispatches a host call through the boundary both backends share, and
    /// records its wait.
    ///
    /// The grant check, the schema check on both sides, the budget charge,
    /// and the trace all live in [`HostRegistry`], so a VM run is held to
    /// exactly what an interpreted run is held to — this adds the timing and
    /// the span and nothing else.
    fn call_host(
        &mut self,
        module: &str,
        op: &str,
        values: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let hosts = self.hosts;
        let started = Instant::now();
        let result = hosts.call_with(module, op, values, &mut Callback { vm: self, span });
        self.charge_wait(started.elapsed());
        result.map_err(|error| error.at(span))
    }

    /// `Ok`, `Err`, `Some`, `Error`, `None`, `assert`, and `assertEqual`.
    ///
    /// # How an assertion quotes its condition
    ///
    /// `assert` and `assertEqual` are builtins rather than library functions
    /// because their failure quotes the *source text* of the condition, in
    /// the words the test was written in. An instruction's own span covers
    /// the whole call, so the argument's span comes from
    /// [`cove_ir::Function::arg_spans`], which the lowering records for
    /// exactly these two; the text is then read out of the [`SourceMap`]
    /// through the interpreter's own [`source_text`], so a failure here is
    /// worded byte for byte as an interpreted one is.
    fn make_builtin(
        &mut self,
        which: &str,
        values: Vec<Value>,
        arg_spans: &[Span],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        if which == NONE_CASE.name {
            // `None` is the one builtin case written as a bare name rather
            // than as a call, so it is a value and not a constructor.
            return Ok(Value::none());
        }
        let assertion =
            free_builtin(which).is_some_and(|schema| schema.kind == FreeBuiltinKind::Assertion);
        if !assertion {
            return builtins::call_constructor(which, values, span);
        }
        let sources: Vec<&str> = arg_spans
            .iter()
            .map(|span| source_text(self.sources, *span))
            .collect();
        let outcome = builtins::call_assertion(which, values, &sources, span)?;
        if let Some(payload) = outcome.err_payload() {
            self.assertion_failure = Some((span, payload[0].to_string()));
        }
        Ok(outcome)
    }
}

/// What a return did: ended the run, or handed control back to a caller at
/// the instruction after its `Call`.
enum Answer {
    Done(Value),
    Caller(Frame, usize),
}

/// `expr?`: the payload, or the failure to return from this call.
///
/// The rule is the interpreter's, for both of the types it is defined over: a
/// `Result` answers its `Ok` payload or returns the whole `Err`, and an
/// `Option` answers its `Some` payload or returns `None`. An empty payload
/// answers `()`, because the schema says one is carried and a host that broke
/// its word is not a reason to lose the shape of the answer.
fn opened(value: Value, span: Span) -> Result<Result<Value, Value>, RuntimeError> {
    match &value {
        Value::Enum(result) if &*result.type_name == RESULT.name => Ok(match value.ok_payload() {
            Some(payload) => Ok(payload.first().cloned().unwrap_or(Value::Unit)),
            None => Err(value),
        }),
        Value::Enum(option) if &*option.type_name == OPTION.name => {
            Ok(match value.some_payload() {
                Some(payload) => Ok(payload.first().cloned().unwrap_or(Value::Unit)),
                None => Err(Value::none()),
            })
        }
        other => Err(RuntimeError::new(format!(
            "`?` needs a `Result` or an `Option`, but found `{}`",
            other.type_name()
        ))
        .at(span)
        .with_rule("`expr?` returns the error from the current function.")),
    }
}

/// A conditional jump was handed something that is not a `Bool`.
///
/// The sentence is `cove-sema`'s rather than the interpreter's, which has one
/// for an `if`, one for a `while`, and one for `&&`. A jump does not say
/// which of the three it was lowered from, and the checker refuses a
/// non-`Bool` condition before any of them is lowered, so this is what a
/// program that reached here would have been told first.
fn not_a_condition(value: &Value, span: Span) -> RuntimeError {
    RuntimeError::new(format!(
        "a condition must be a `Bool`, but found `{}`",
        value.type_name()
    ))
    .at(span)
    .with_rule("There are no implicit boolean conversions.")
}

/// One constant as the value it stands for.
fn constant(held: &Const) -> Value {
    match held {
        Const::Unit => Value::Unit,
        Const::Bool(value) => Value::Bool(*value),
        Const::Int(value) => Value::Int(*value),
        Const::Float(value) => Value::Float(*value),
        Const::Duration(value) => Value::Duration(*value),
        Const::Str(text) => Value::Str(text.clone()),
        // A name is carried by an instruction that already knows what to do
        // with it, and nothing loads one as a value. It is still a string, so
        // there is nothing to invent if something ever does.
        Const::Name(text) => Value::Str(text.clone()),
    }
}

/// The text an instruction's constant names.
fn name(program: &Program, id: ConstId) -> &str {
    match program.constant(id) {
        Const::Name(text) | Const::Str(text) => text,
        other => unreachable!("an instruction named {other:?} rather than a name"),
    }
}

/// The IR's unary operator as the interpreter's.
fn unary_op(op: IrUnary) -> UnaryOp {
    match op {
        IrUnary::Not => UnaryOp::Not,
        IrUnary::Neg => UnaryOp::Neg,
    }
}

/// The IR's binary operator as the interpreter's.
///
/// `&&` and `||` have no IR operator: they short-circuit, so they lower to a
/// jump, and there is nothing here for them to be.
fn binary_op(op: IrBinary) -> BinaryOp {
    match op {
        IrBinary::Add => BinaryOp::Add,
        IrBinary::Sub => BinaryOp::Sub,
        IrBinary::Mul => BinaryOp::Mul,
        IrBinary::Div => BinaryOp::Div,
        IrBinary::Rem => BinaryOp::Rem,
        IrBinary::Eq => BinaryOp::Eq,
        IrBinary::Ne => BinaryOp::Ne,
        IrBinary::Lt => BinaryOp::Lt,
        IrBinary::Le => BinaryOp::Le,
        IrBinary::Gt => BinaryOp::Gt,
        IrBinary::Ge => BinaryOp::Ge,
        IrBinary::Is => BinaryOp::Is,
    }
}

/// How big a value is, in the units the work over it is proportional to.
///
/// ADR 0019 asks that an operation whose cost is not constant — copying a
/// collection, comparing two strings, building a value proportional to its
/// input — is charged proportionally. This is that size, read in constant
/// time from what a value already knows about itself, so asking costs nothing
/// on the paths that ask.
fn size_of_value(value: &Value) -> u64 {
    match value {
        Value::Str(text) => text.len() as u64,
        Value::Array(items) => items.len() as u64,
        Value::Struct(held) => held.fields.len() as u64,
        Value::Enum(case) => case.payload.len() as u64,
        _ => 0,
    }
}

/// The shape of every struct the program builds, worked out once.
///
/// A `MakeStruct` names its type with one constant and its fields with
/// another, and the same type always carries the same pair, so one entry per
/// type constant is a complete table. Whether the type is opaque is the
/// checker's answer, read here rather than at every construction.
fn struct_shapes(runtime: &Runtime, program: &Program) -> Vec<Option<StructShape>> {
    let mut shapes: Vec<Option<StructShape>> = (0..program.constants.len()).map(|_| None).collect();
    for function in &program.functions {
        for inst in &function.code {
            let Inst::MakeStruct { ty, fields } = *inst else {
                continue;
            };
            if shapes[ty.0 as usize].is_some() {
                continue;
            }
            let type_name: Rc<str> = name(program, ty).into();
            let written = name(program, fields);
            let fields = if written.is_empty() {
                Vec::new()
            } else {
                written.split(',').map(Rc::<str>::from).collect()
            };
            let opaque = is_opaque(runtime, &type_name);
            shapes[ty.0 as usize] = Some(StructShape {
                type_name,
                fields,
                opaque,
            });
        }
    }
    shapes
}

/// Whether the module that declares `qualified` declared it `export opaque
/// struct`.
///
/// The checker is what knows, so it is asked, exactly as
/// `Interpreter::is_opaque` asks it.
fn is_opaque(runtime: &Runtime, qualified: &str) -> bool {
    let Some((module, name)) = qualified.rsplit_once('.') else {
        return false;
    };
    runtime
        .program()
        .modules
        .get(module)
        .and_then(|resolved| resolved.structs.get(name))
        .is_some_and(|entry| entry.opaque)
}

/// The way back into a VM run that a host call was handed.
///
/// Nothing the lowering covers can build a closure, so no host call this
/// backend makes can be given one to run. What is still owed is the rest of
/// the contract: a host that waits is standing where a safepoint would be, so
/// it is told what a safepoint would find.
struct Callback<'v, 'a> {
    vm: &'v mut Vm<'a>,
    /// Where the host call that is running this was written, so a failure
    /// inside it points at the call rather than at nothing.
    span: Span,
}

impl Reentry for Callback<'_, '_> {
    fn call(&mut self, callee: &Value, _args: Vec<Value>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::new(format!(
            "this host call cannot run {}, because the VM has no closures yet",
            callee.type_name()
        ))
        .at(self.span)
        .with_rule("A construct the IR does not cover is named rather than approximated."))
    }

    fn call_until(
        &mut self,
        callee: &Value,
        args: Vec<Value>,
        stop: &Cancellation,
    ) -> Result<Value, RuntimeError> {
        self.vm.stops.push(stop.clone());
        let result = self.call(callee, args);
        self.vm.stops.pop();
        result
    }

    /// Everything [`Vm::safepoint`] would stop on, asked from outside the
    /// loop: every bounded call this thread is inside, and the run's own
    /// cancellation.
    fn is_cancelled(&self) -> bool {
        if self.vm.stops.iter().any(Cancellation::is_cancelled) {
            return true;
        }
        self.vm
            .hosts
            .with_budget(|budget| budget.cancellation().is_cancelled())
            .unwrap_or(false)
    }

    /// What the run's deadline leaves, read from the one budget that knows
    /// when the run started.
    fn time_left(&self) -> Option<Duration> {
        self.vm.hosts.with_budget(|budget| {
            let deadline = budget.limits().deadline?;
            Some(deadline.saturating_sub(budget.elapsed()))
        })?
    }

    /// The task the boundary records the call against. A VM run is the
    /// entry's, because nothing it can run spawns a task.
    fn task(&self) -> u64 {
        ENTRY_TASK
    }
}

impl Callable for Vm<'_> {
    fn allocate_vector(&mut self, elements: Vec<Value>) -> Value {
        Value::Vector(self.heap.allocate(elements))
    }

    fn call_value(
        &mut self,
        callee: &Value,
        _args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::new(format!(
            "this builtin cannot run {}, because the VM has no closures yet",
            callee.type_name()
        ))
        .at(span)
        .with_rule("A construct the IR does not cover is named rather than approximated."))
    }

    fn arity(&self, callee: &Value) -> Option<usize> {
        match callee {
            Value::Closure(closure) => Some(closure.params.len()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use cove_diag::SourceMap;
    use cove_sema::config::Config;
    use cove_sema::package::{Module, Package, Unit};
    use cove_sema::resolve::Program as Checked;

    use crate::budget::{Budget, Limits};
    use crate::clock::{Clock, VirtualTime};
    use crate::host::{Console, Env as EnvHost, Grants};
    use crate::interp::Interpreter;

    /// Every capability a differential run is granted.
    ///
    /// The same set every time, because granting one is not what these tests
    /// are about: a program that calls no host is unaffected by holding the
    /// capability to, and a program that calls one is compared against an
    /// interpreted run holding exactly the same grants.
    const GRANTS: &[&str] = &["console", "clock", "env"];

    /// A `console` sink a test can read back.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("no test panics while printing")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Buffer {
        fn text(&self) -> String {
            String::from_utf8(
                self.0
                    .lock()
                    .expect("no test panics while printing")
                    .clone(),
            )
            .expect("console output is UTF-8")
        }
    }

    /// What one backend made of one program: the value or error it answered,
    /// and everything it wrote to `console`.
    ///
    /// The value is rendered rather than carried, because a [`Value`] is
    /// `Rc`-based and a run happens on a thread of its own.
    #[derive(Debug)]
    struct Outcome {
        answer: Result<String, RuntimeError>,
        output: String,
    }

    impl Outcome {
        fn value(&self) -> &str {
            match &self.answer {
                Ok(rendered) => rendered,
                Err(error) => panic!("the program ran without a runtime error: {error:?}"),
            }
        }

        fn error(&self) -> &RuntimeError {
            match &self.answer {
                Ok(rendered) => {
                    panic!("expected a runtime error, but the program answered {rendered}")
                }
                Err(error) => error,
            }
        }
    }

    /// One run's answer, rendered so it can leave the thread it happened on.
    fn described(answer: Result<Value, RuntimeError>) -> Result<String, RuntimeError> {
        answer.map(|value| format!("{value:?}"))
    }

    /// The hosts a differential run calls through: a `console` the test reads
    /// back, a clock whose virtual time never advances on its own, and an
    /// `env` with nothing in it.
    fn hosts(buffer: &Buffer, budget: Option<Budget>) -> Arc<HostRegistry> {
        let mut hosts = HostRegistry::new(Grants::new(GRANTS.to_vec()));
        hosts.register(Box::new(Console::new(buffer.clone())));
        hosts.register(Box::new(EnvHost::new(BTreeMap::new())));
        hosts.register(Box::new(Clock::virtual_clock(VirtualTime::new())));
        if let Some(budget) = budget {
            hosts.set_budget(budget);
        }
        Arc::new(hosts)
    }

    /// Runs `module.main` on the oracle.
    fn interpreted(
        checked: &Arc<Checked>,
        sources: &Arc<SourceMap>,
        module: &str,
        budget: Option<Budget>,
    ) -> Outcome {
        let buffer = Buffer::default();
        let runtime = Runtime::new(checked.clone(), sources.clone(), hosts(&buffer, budget));
        let answer = Interpreter::new(&runtime).run_entry(module, "main", Vec::new());
        Outcome {
            answer: described(answer),
            output: buffer.text(),
        }
    }

    /// Lowers the program and runs `module.main` on the VM.
    ///
    /// The lowering and the validation happen here rather than beside the
    /// interpreted run because a `cove_ir::Program` holds `Rc`s and cannot
    /// cross the thread boundary [`crate::on_cove_stack`] draws.
    fn lowered(
        checked: &Arc<Checked>,
        sources: &Arc<SourceMap>,
        module: &str,
        budget: Option<Budget>,
    ) -> Outcome {
        let program = match cove_ir::lower::lower(checked) {
            Ok(program) => program,
            Err(why) => panic!("the program lowers, but stopped at {why}"),
        };
        cove_ir::lower::validate(&program)
            .unwrap_or_else(|why| panic!("the lowering holds the VM's invariants: {why}"));
        let entry = program
            .function_named(module, "main")
            .unwrap_or_else(|| panic!("`{module}.main` was lowered"));
        let buffer = Buffer::default();
        let hosts = hosts(&buffer, budget);
        let runtime = Runtime::new(checked.clone(), sources.clone(), hosts.clone());
        let answer = Vm::new(&runtime, &hosts, &program).run(entry, Vec::new());
        Outcome {
            answer: described(answer),
            output: buffer.text(),
        }
    }

    /// Runs one program on both backends, on a stack the runtime sized.
    ///
    /// Both runs happen inside one [`crate::on_cove_stack`] because the
    /// interpreter is a recursive tree walker and a test thread's stack is
    /// not one it chose; only the two rendered outcomes come back out.
    fn on_both(
        checked: &Arc<Checked>,
        sources: &Arc<SourceMap>,
        module: &str,
        limits: Option<Limits>,
    ) -> (Outcome, Outcome) {
        crate::on_cove_stack(|| {
            let budget = || limits.clone().map(Budget::new);
            (
                interpreted(checked, sources, module, budget()),
                lowered(checked, sources, module, budget()),
            )
        })
        .expect("a thread to run Cove on")
    }

    /// Parses and checks `source` as the single unit of module `m`.
    fn checked_module(source: &str) -> (Arc<SourceMap>, Arc<Checked>) {
        let mut sources = SourceMap::new();
        let path = PathBuf::from("m/main.cove");
        let file = sources.add(path.clone(), source);
        let ast = match cove_syntax::parse_file(&sources, file) {
            Ok(ast) => ast,
            Err(items) => panic!("the source parses:\n{}", rendered(&sources, &items)),
        };
        let package = Package {
            root: PathBuf::new(),
            config: Config::default(),
            modules: BTreeMap::from([(
                "m".to_string(),
                Module {
                    name: "m".to_string(),
                    dir: PathBuf::from("m"),
                    units: vec![Unit { file, path, ast }],
                },
            )]),
        };
        match cove_sema::resolve::resolve(&package) {
            Ok(program) => (Arc::new(sources), Arc::new(program)),
            Err(items) => panic!("the source checks:\n{}", rendered(&sources, &items)),
        }
    }

    fn rendered(sources: &SourceMap, items: &[cove_diag::Diagnostic]) -> String {
        items
            .iter()
            .map(|item| cove_diag::render(sources, item))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// **The differential test.** Runs `source` on both backends and asserts
    /// they agree about everything a program can be observed by: the value or
    /// the error it answered, and what it wrote to `console`.
    ///
    /// Every test here goes through this. ADR 0012 ranks the oracle above a
    /// backend, so a test that asserted only what the VM did would be a test
    /// of what somebody expected rather than of what Cove means.
    fn agree(source: &str) -> Outcome {
        let (sources, checked) = checked_module(source);
        let (interpreted, lowered) = on_both(&checked, &sources, "m", None);
        assert_eq!(
            format!("{:?}", interpreted.answer),
            format!("{:?}", lowered.answer),
            "the two backends answered differently for:\n{source}"
        );
        assert_eq!(
            interpreted.output, lowered.output,
            "the two backends printed differently for:\n{source}"
        );
        lowered
    }

    /// `agree`, for a `main` written around `body` and returning `ty`.
    fn agree_main(ty: &str, body: &str) -> Outcome {
        agree(&format!(
            "use console.println\n\nexport fn main() -> {ty} {{\n{body}\n}}\n"
        ))
    }

    /// What both backends made of one expression, rendered as a `Value`.
    fn expression(ty: &str, expr: &str) -> String {
        agree_main(ty, &format!("  {expr}")).value().to_string()
    }

    /// The message both backends refused one expression with.
    fn refused(ty: &str, expr: &str) -> String {
        agree_main(ty, &format!("  {expr}")).error().message.clone()
    }

    // -------------------------------------------------------- operators

    /// Every operator the IR carries, on every type the language defines it
    /// for.
    ///
    /// One test rather than one per operator, because what is being checked
    /// is the mapping from the IR's operator to the interpreter's: a
    /// `Sub` that reached `binary` as `Add` still answers a number, so only
    /// running every one of them against the oracle catches it.
    #[test]
    fn every_operator_answers_what_the_interpreter_answers() {
        let cases: &[(&str, &str, &str)] = &[
            ("Int", "7 + 5", "Int(12)"),
            ("Int", "7 - 5", "Int(2)"),
            ("Int", "7 * 5", "Int(35)"),
            ("Int", "7 / 5", "Int(1)"),
            ("Int", "7 % 5", "Int(2)"),
            ("Int", "-7", "Int(-7)"),
            ("Float", "7.5 + 0.25", "Float(7.75)"),
            ("Float", "7.5 - 0.25", "Float(7.25)"),
            ("Float", "7.5 * 2.0", "Float(15.0)"),
            ("Float", "7.5 / 2.0", "Float(3.75)"),
            ("Float", "7.5 % 2.0", "Float(1.5)"),
            ("Float", "-7.5", "Float(-7.5)"),
            ("Duration", "1ms + 500us", "Duration(1500000)"),
            ("Duration", "1ms - 500us", "Duration(500000)"),
            ("Duration", "-1ms", "Duration(-1000000)"),
            ("Bool", "7 == 7", "Bool(true)"),
            ("Bool", "7 != 7", "Bool(false)"),
            ("Bool", "7 < 5", "Bool(false)"),
            ("Bool", "7 <= 7", "Bool(true)"),
            ("Bool", "7 > 5", "Bool(true)"),
            ("Bool", "7 >= 8", "Bool(false)"),
            ("Bool", "\"a\" < \"b\"", "Bool(true)"),
            ("Bool", "\"a\" == \"a\"", "Bool(true)"),
            ("Bool", "1ms > 999us", "Bool(true)"),
            ("Bool", "0.5 <= 0.5", "Bool(true)"),
            ("Bool", "!true", "Bool(false)"),
            ("Bool", "true && false", "Bool(false)"),
            ("Bool", "true || false", "Bool(true)"),
            ("Bool", "[1, 2] == [1, 2]", "Bool(true)"),
        ];
        for (ty, expr, expected) in cases {
            assert_eq!(&expression(ty, expr), expected, "for `{expr}`");
        }
    }

    /// The failures arithmetic can have, in the words the interpreter reports
    /// them in.
    ///
    /// A mixed-type comparison is not here because no checked program has
    /// one: `cove-sema` refuses `1 == "a"` before either backend sees it, and
    /// so refuses every other operator applied across two types. What is left
    /// that a checked program can still do is overflow and divide by zero,
    /// and both are reported by the one `binary` both backends call.
    #[test]
    fn arithmetic_fails_the_way_the_interpreter_fails() {
        let most_negative = "  let least = -9223372036854775807 - 1\n";
        let cases: &[(&str, &str)] = &[
            (
                "  let big = 9223372036854775807\n  big + 1",
                "`Int` addition overflowed",
            ),
            (
                "  let least = -9223372036854775807 - 1\n  least - 1",
                "`Int` subtraction overflowed",
            ),
            (
                "  let big = 9223372036854775807\n  big * 2",
                "`Int` multiplication overflowed",
            ),
            ("  let zero = 0\n  1 / zero", "`Int` division by zero"),
            ("  let zero = 0\n  1 % zero", "`Int` remainder by zero"),
        ];
        for (body, message) in cases {
            assert_eq!(
                &agree_main("Int", body).error().message,
                message,
                "for:\n{body}"
            );
        }
        assert_eq!(
            agree_main("Int", &format!("{most_negative}  -least"))
                .error()
                .message,
            "`Int` negation overflowed"
        );
        assert_eq!(
            agree_main("Int", &format!("{most_negative}  least / -1"))
                .error()
                .message,
            "`Int` division overflowed"
        );
    }

    /// A failure carries the span of the instruction that raised it, so a
    /// diagnostic points at the same source it points at today.
    #[test]
    fn a_failure_points_at_the_operator_that_raised_it() {
        let source = "export fn main() -> Int {\n  let zero = 0\n  1 / zero\n}\n";
        let error = agree(source).error().clone();
        let span = error.span.expect("a runtime error points at source");
        assert_eq!(&source[span.start as usize..span.end as usize], "1 / zero");
    }

    // ----------------------------------------------------- control flow

    #[test]
    fn an_if_answers_the_branch_that_ran() {
        assert_eq!(expression("Int", "if 1 < 2 { 10 } else { 20 }"), "Int(10)");
        assert_eq!(expression("Int", "if 1 > 2 { 10 } else { 20 }"), "Int(20)");
        assert_eq!(
            expression("Unit", "if 1 < 2 {\n    let seen = 1\n  }"),
            "Unit"
        );
    }

    #[test]
    fn a_while_loop_counts_and_leaves() {
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  var i = 0\n  while i < 5 {\n    total += i\n    i += 1\n  }\n  total"
            )
            .value(),
            "Int(10)"
        );
    }

    #[test]
    fn a_for_walks_a_range_and_a_sequence() {
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  for i in 0..<5 {\n    total += i\n  }\n  total"
            )
            .value(),
            "Int(10)"
        );
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  for i in 0..5 {\n    total += i\n  }\n  total"
            )
            .value(),
            "Int(15)"
        );
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  for n in [3, 4, 5] {\n    total += n\n  }\n  total"
            )
            .value(),
            "Int(12)"
        );
    }

    #[test]
    fn break_and_continue_leave_and_skip() {
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  for i in 0..<10 {\n    if i == 5 {\n      break\n    }\n    total += i\n  }\n  total"
            )
            .value(),
            "Int(10)"
        );
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  for i in 0..<10 {\n    if i % 2 == 0 {\n      continue\n    }\n    total += i\n  }\n  total"
            )
            .value(),
            "Int(25)"
        );
        assert_eq!(
            agree_main(
                "Int",
                "  var total = 0\n  var i = 0\n  while true {\n    i += 1\n    if i > 3 {\n      break\n    }\n    total += i\n  }\n  total"
            )
            .value(),
            "Int(6)"
        );
    }

    #[test]
    fn an_early_return_ends_the_function_where_it_is_written() {
        let source = "fn first(items: Array<Int>) -> Int {\n  for n in items {\n    if n > 2 {\n      return n\n    }\n  }\n  0\n}\n\nexport fn main() -> Int {\n  first([1, 2, 3, 4])\n}\n";
        assert_eq!(agree(source).value(), "Int(3)");
    }

    // ---------------------------------------------------------- calls

    /// Recursion, deep enough that the frame stack has to be a stack.
    ///
    /// `fib(20)` is about 22,000 nested and sibling calls, which is the same
    /// workload `benches/pure` measures — enough that a frame layout that
    /// only worked one level deep could not answer it.
    #[test]
    fn recursion_answers_what_the_interpreter_answers() {
        let source = "fn fib(n: Int) -> Int {\n  if n < 2 {\n    n\n  } else {\n    fib(n - 1) + fib(n - 2)\n  }\n}\n\nexport fn main() -> Int {\n  fib(20)\n}\n";
        assert_eq!(agree(source).value(), "Int(6765)");
    }

    /// Recursion past the depth limit reports the limit rather than
    /// exhausting anything.
    #[test]
    fn unbounded_recursion_reports_the_depth_limit() {
        let source = "fn down(n: Int) -> Int {\n  down(n + 1)\n}\n\nexport fn main() -> Int {\n  down(0)\n}\n";
        assert_eq!(
            agree(source).error().message,
            format!("call depth limit of {MAX_CALL_DEPTH} reached while calling `down`")
        );
    }

    // -------------------------------------------------------- structs

    const CURSOR: &str = "struct Cursor {\n  at: Int\n  step: Int\n}\n\n";

    #[test]
    fn a_struct_is_built_read_and_written() {
        assert_eq!(
            agree(&format!(
                "{CURSOR}export fn main() -> Int {{\n  let cursor = Cursor(at: 3, step: 2)\n  cursor.at\n}}\n"
            ))
            .value(),
            "Int(3)"
        );
        assert_eq!(
            agree(&format!(
                "{CURSOR}export fn main() -> Int {{\n  var cursor = Cursor(at: 3, step: 2)\n  cursor.at = 9\n  cursor.at\n}}\n"
            ))
            .value(),
            "Int(9)"
        );
        assert_eq!(
            agree(&format!(
                "{CURSOR}export fn main() -> Int {{\n  var cursor = Cursor(at: 3, step: 2)\n  cursor.at += cursor.step\n  cursor.at\n}}\n"
            ))
            .value(),
            "Int(5)"
        );
    }

    /// A struct is a value: writing a copy's field leaves the original alone.
    #[test]
    fn writing_a_copys_field_leaves_the_original_alone() {
        assert_eq!(
            agree(&format!(
                "{CURSOR}export fn main() -> Int {{\n  let first = Cursor(at: 1, step: 1)\n  var second = first\n  second.at = 99\n  first.at\n}}\n"
            ))
            .value(),
            "Int(1)"
        );
    }

    #[test]
    fn a_method_takes_its_receiver_and_answers() {
        let source = format!(
            "{CURSOR}impl Cursor {{\n  fn position(self) -> Int {{\n    self.at\n  }}\n\n  fn ahead(self, by: Int) -> Int {{\n    self.at + by * self.step\n  }}\n}}\n\nexport fn main() -> Int {{\n  let cursor = Cursor(at: 4, step: 3)\n  cursor.position() + cursor.ahead(by: 2)\n}}\n"
        );
        assert_eq!(agree(&source).value(), "Int(14)");
    }

    /// An opaque struct renders as its name alone, on both backends.
    ///
    /// The IR carries the type's name and not whether it is opaque, so the VM
    /// asks the checker the same question `Interpreter::init_struct` asks.
    #[test]
    fn an_opaque_struct_renders_as_its_name() {
        let source = "export opaque struct Token {\n  secret: Int\n}\n\nexport fn main() -> String {\n  let token = Token(secret: 7)\n  \"{token}\"\n}\n";
        assert_eq!(agree(source).value(), "Str(\"Token\")");
    }

    // ------------------------------------------------------- builtins

    #[test]
    fn builtin_methods_answer_what_the_interpreter_answers() {
        assert_eq!(expression("Int", "[1, 2, 3].length()"), "Int(3)");
        assert_eq!(expression("Int", "[1, 2, 3].get(1).unwrapOr(0)"), "Int(2)");
        assert_eq!(expression("Int", "[1, 2, 3].get(9).unwrapOr(0)"), "Int(0)");
        assert_eq!(expression("Int", "\"hello\".chars().length()"), "Int(5)");
        assert_eq!(
            expression("String", "\"hello\".chars().get(1).unwrapOr(\"\")"),
            "Str(\"e\")"
        );
        assert_eq!(expression("Int", "\"hello\".length()"), "Int(5)");
    }

    /// A builtin's own failure is the interpreter's, because it is the same
    /// call.
    #[test]
    fn a_builtin_fails_the_way_the_interpreter_fails() {
        assert_eq!(
            agree_main(
                "Int",
                "  let least = -9223372036854775807 - 1\n  least.abs()"
            )
            .error()
            .message,
            "`Int` abs overflowed"
        );
        assert_eq!(
            refused("Int", "[1, 2, 3].get(1, 2).unwrapOr(0)"),
            "`Array.get` takes 1 argument(s), but 2 were given"
        );
    }

    // ----------------------------------------------- results and options

    #[test]
    fn the_builtin_constructors_build_what_the_interpreter_builds() {
        let source = concat!(
            "fn nothing() -> Option<Int> {\n  None\n}\n\n",
            "export fn main() -> String {\n",
            "  let good: Result<Int, Error> = Ok(1)\n",
            "  let bad: Result<Int, Error> = Err(Error(message: \"no\"))\n",
            "  let there = Some(2)\n",
            "  let boom = Error(message: \"boom\")\n",
            "  \"{good} {bad} {there} {nothing()} {boom}\"\n",
            "}\n"
        );
        assert_eq!(
            agree(source).value(),
            "Str(\"Ok(1) Err(no) Some(2) None boom\")"
        );
    }

    /// `?` on both of the types it is defined over, taking both paths.
    #[test]
    fn a_question_mark_opens_or_returns() {
        let ok = "fn answer() -> Result<Int, Error> {\n  Ok(7)\n}\n\nexport fn main() -> Result<Int, Error> {\n  let n = answer()?\n  Ok(n + 1)\n}\n";
        assert_eq!(
            agree(ok).value(),
            "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Int(8)] })"
        );

        let err = "fn answer() -> Result<Int, Error> {\n  Err(Error(message: \"no\"))\n}\n\nexport fn main() -> Result<Int, Error> {\n  let n = answer()?\n  Ok(n + 1)\n}\n";
        assert_eq!(
            agree(err).value(),
            "Enum(EnumValue { type_name: \"Result\", case: \"Err\", payload: [Struct(StructValue { type_name: \"Error\", fields: [(\"message\", Str(\"no\"))], opaque: false })] })"
        );

        let some = "fn answer() -> Option<Int> {\n  Some(7)\n}\n\nexport fn main() -> Option<Int> {\n  let n = answer()?\n  Some(n + 1)\n}\n";
        assert_eq!(
            agree(some).value(),
            "Enum(EnumValue { type_name: \"Option\", case: \"Some\", payload: [Int(8)] })"
        );

        let none = "fn answer() -> Option<Int> {\n  None\n}\n\nexport fn main() -> Option<Int> {\n  let n = answer()?\n  Some(n + 1)\n}\n";
        assert_eq!(
            agree(none).value(),
            "Enum(EnumValue { type_name: \"Option\", case: \"None\", payload: [] })"
        );
    }

    // ------------------------------------------------------- rendering

    #[test]
    fn interpolation_renders_every_part_left_to_right() {
        assert_eq!(
            expression("String", "\"a{1 + 2}b{true}c\""),
            "Str(\"a3btruec\")"
        );
        assert_eq!(
            agree(&format!(
                "{CURSOR}export fn main() -> String {{\n  \"{{Cursor(at: 1, step: 2)}}\"\n}}\n"
            ))
            .value(),
            "Str(\"Cursor(at: 1, step: 2)\")"
        );
    }

    #[test]
    fn an_array_is_built_left_to_right() {
        assert_eq!(
            expression("String", "\"{[1 + 1, 2 + 2, 3 + 3]}\""),
            "Str(\"[2, 4, 6]\")"
        );
        assert_eq!(expression("Int", "[].length()"), "Int(0)");
    }

    // ------------------------------------------------------ assertions

    #[test]
    fn a_holding_assertion_answers_ok_on_both() {
        assert_eq!(
            agree_main(
                "Result<Unit, Error>",
                "  assert(1 < 2)?\n  assertEqual(1 + 1, 2)?\n  Ok(())"
            )
            .value(),
            "Enum(EnumValue { type_name: \"Result\", case: \"Ok\", payload: [Unit] })"
        );
    }

    // -------------------------------------------------------- the host

    /// The same calls, in the same order, with the same values.
    #[test]
    fn host_calls_reach_the_host_in_the_same_order() {
        let outcome = agree_main(
            "Result<Unit, Error>",
            "  println(\"one\")?\n  for i in 0..<3 {\n    println(\"tick {i}\")?\n  }\n  println(\"done\")?\n  Ok(())",
        );
        assert_eq!(outcome.output, "one\ntick 0\ntick 1\ntick 2\ndone\n");
    }

    /// A capability the run was not granted is refused at the boundary both
    /// backends call through.
    #[test]
    fn an_ungranted_capability_is_refused_at_the_boundary() {
        let (sources, checked) = checked_module(
            "use console.println\n\nexport fn main() -> Result<Unit, Error> {\n  println(\"hello\")?\n  Ok(())\n}\n",
        );
        let (interpreted, lowered) = crate::on_cove_stack(|| {
            let ungranted = || {
                let mut hosts = HostRegistry::new(Grants::new(Vec::<&str>::new()));
                hosts.register(Box::new(Console::new(Buffer::default())));
                Arc::new(hosts)
            };
            let interpreted = {
                let runtime = Runtime::new(checked.clone(), sources.clone(), ungranted());
                described(Interpreter::new(&runtime).run_entry("m", "main", Vec::new()))
            };
            let lowered = {
                let program = cove_ir::lower::lower(&checked).expect("it lowers");
                let entry = program.function_named("m", "main").expect("`main` lowered");
                let hosts = ungranted();
                let runtime = Runtime::new(checked.clone(), sources.clone(), hosts.clone());
                described(Vm::new(&runtime, &hosts, &program).run(entry, Vec::new()))
            };
            (interpreted, lowered)
        })
        .expect("a thread to run Cove on");
        assert_eq!(format!("{interpreted:?}"), format!("{lowered:?}"));
        assert_eq!(
            lowered.expect_err("the capability was not granted").message,
            "`console.println` requires the `console` capability, which this run was not granted"
        );
    }

    // -------------------------------------------------------- budgets

    /// Fuel exhaustion stops the VM.
    ///
    /// The two backends do not stop at the same point and are not asked to:
    /// ADR 0019 makes `fuel_spent` backend-specific, because an instruction
    /// is not an AST node. What both must do is stop, with the message the
    /// shared budget writes.
    #[test]
    fn fuel_exhaustion_stops_both_backends() {
        let (sources, checked) = checked_module(
            "export fn main() -> Int {\n  var total = 0\n  var i = 0\n  while i < 1000000 {\n    total += i\n    i += 1\n  }\n  total\n}\n",
        );
        let limits = Limits {
            fuel: Some(1_000),
            ..Limits::default()
        };
        let (interpreted, lowered) = on_both(&checked, &sources, "m", Some(limits));
        assert_eq!(
            interpreted.error().message,
            "execution stopped: fuel budget of 1000 exhausted"
        );
        assert_eq!(
            lowered.error().message,
            "execution stopped: fuel budget of 1000 exhausted"
        );
    }

    /// Cancellation stops the VM at its next safepoint.
    #[test]
    fn cancellation_stops_both_backends() {
        let (sources, checked) = checked_module(
            "export fn main() -> Int {\n  var total = 0\n  var i = 0\n  while i < 1000000 {\n    total += i\n    i += 1\n  }\n  total\n}\n",
        );
        // Cancelled before the first instruction, so both backends stop at
        // the first safepoint they reach rather than at a moment a test would
        // have to race them for.
        let cancelled = || {
            let cancellation = Cancellation::new();
            let budget = Budget::with_cancellation(Limits::default(), cancellation.clone());
            cancellation.cancel();
            budget
        };
        let stopped = crate::on_cove_stack(|| {
            (
                interpreted(&checked, &sources, "m", Some(cancelled())),
                lowered(&checked, &sources, "m", Some(cancelled())),
            )
        })
        .expect("a thread to run Cove on");
        assert_eq!(
            stopped.0.error().message,
            "execution stopped: the run was cancelled"
        );
        assert_eq!(
            stopped.1.error().message,
            "execution stopped: the run was cancelled"
        );
    }

    // ------------------------ where a program is refused before it runs

    /// What the lowering said when it refused `source`.
    ///
    /// A program the lowering refuses never reaches the VM, which is ADR
    /// 0019's no-silent-fallback rule from the other side: the run stops
    /// before any side effect and says what stopped it.
    fn not_lowered(checked: &Arc<Checked>) -> String {
        match cove_ir::lower::lower(checked) {
            Ok(_) => panic!("the program lowered, and was expected not to"),
            Err(why) => why.what,
        }
    }

    /// One interpreted run, on a stack the runtime sized.
    fn only_interpreted(checked: &Arc<Checked>, sources: &Arc<SourceMap>) -> Outcome {
        crate::on_cove_stack(|| interpreted(checked, sources, "m", None))
            .expect("a thread to run Cove on")
    }

    /// A failing assertion quotes its condition identically on both backends.
    ///
    /// `assert` and `assertEqual` are builtins because their failure names
    /// the condition in the words the test was written in. The interpreter
    /// reads the argument's span out of the `SourceMap`; the VM reads the
    /// same span out of `cove_ir::Function::arg_spans` and the same text out
    /// of the same map, so the two messages are compared byte for byte here
    /// rather than merely both being failures.
    #[test]
    fn a_failing_assertion_quotes_its_condition_on_both_backends() {
        let (sources, checked) = checked_module(
            "export fn main() -> Result<Unit, Error> {\n  assert(1 > 2)?\n  Ok(())\n}\n",
        );
        let (interpreted, lowered) = on_both(&checked, &sources, "m", None);
        assert!(
            interpreted.value().contains("assertion failed: `1 > 2`"),
            "{}",
            interpreted.value()
        );
        assert_eq!(interpreted.value(), lowered.value());

        let (equal_sources, equal_checked) = checked_module(
            "export fn main() -> Result<Unit, Error> {\n  assertEqual(1 + 1, 3)?\n  Ok(())\n}\n",
        );
        let (interpreted, lowered) = on_both(&equal_checked, &equal_sources, "m", None);
        assert!(
            interpreted
                .value()
                .contains("assertion failed: `1 + 1` is `2`, expected `3`"),
            "{}",
            interpreted.value()
        );
        assert_eq!(interpreted.value(), lowered.value());
    }

    /// Assigning to a `let` binding is refused by both backends: by the
    /// interpreter when the write happens, and by the lowering before the VM
    /// is handed anything.
    ///
    /// The interpreter's message is asserted here rather than only in
    /// `interp`, because what the two say is the point: a backend that
    /// performed this write would be more permissive than the oracle, and
    /// the two refusals have to stay comparable for a reader to see that
    /// they are the same rule.
    #[test]
    fn writing_a_let_binding_is_refused_by_both_backends() {
        let (sources, checked) =
            checked_module("export fn main() -> Int {\n  let x = 1\n  x = 2\n  x\n}\n");
        assert_eq!(
            only_interpreted(&checked, &sources).error().message,
            "cannot assign to `x`, which is a read-only place"
        );
        assert_eq!(
            not_lowered(&checked),
            "assignment to `x`, which is a read-only place"
        );
    }

    /// A field of a `let` binding is a read-only place too, and the write is
    /// refused on both backends for the same reason.
    #[test]
    fn writing_a_field_of_a_let_binding_is_refused_by_both_backends() {
        let (sources, checked) = checked_module(
            "struct P {\n  x: Int\n}\n\nexport fn main() -> Int {\n  let p = P(x: 1)\n  p.x = 2\n  p.x\n}\n",
        );
        assert_eq!(
            only_interpreted(&checked, &sources).error().message,
            "cannot assign to `p.x`, which is a read-only place"
        );
        assert_eq!(
            not_lowered(&checked),
            "assignment to `p.x`, which is a read-only place"
        );
    }

    /// A `var` binding is still written, on both backends.
    ///
    /// The refusal above is about a read-only place and not about assignment,
    /// and this is what says so.
    #[test]
    fn writing_a_var_binding_is_performed_by_both_backends() {
        assert_eq!(
            agree("export fn main() -> Int {\n  var x = 1\n  x = 2\n  x\n}\n").value(),
            "Int(2)"
        );
    }

    /// A method name a builtin type and a declared type both answer to is
    /// refused rather than resolved to one of them.
    ///
    /// The interpreter tries a declared method of the receiver's *runtime*
    /// type first and falls back to the builtin table, so which applies is a
    /// fact about the receiver. The lowering has no type table, so a name
    /// both could answer to has two possible targets and no way to choose:
    /// `[1, 2, 3].length()` is the builtin's `3` here, and a `Call` to the
    /// declared `Box.length` would be a different program. Refusing to lower
    /// is the answer; guessing is not.
    #[test]
    fn a_method_name_a_builtin_and_a_declared_type_share_is_refused() {
        let (sources, checked) = checked_module(
            "struct Box {\n  n: Int\n}\n\nimpl Box {\n  fn length(self) -> Int {\n    self.n\n  }\n}\n\nexport fn main() -> Int {\n  [1, 2, 3].length()\n}\n",
        );
        assert_eq!(only_interpreted(&checked, &sources).value(), "Int(3)");
        assert_eq!(
            not_lowered(&checked),
            "a call to `length`, which a builtin type and a declared type both have"
        );
    }

    /// A declared method whose name no builtin has still lowers to a `Call`.
    ///
    /// The refusal above is about the collision and not about declared
    /// methods, and `benches/method` depends on this staying true.
    #[test]
    fn a_declared_method_no_builtin_shares_still_lowers() {
        assert_eq!(
            agree(
                "struct Box {\n  n: Int\n}\n\nimpl Box {\n  fn held(self) -> Int {\n    self.n\n  }\n}\n\nexport fn main() -> Int {\n  Box(n: 3).held()\n}\n"
            )
            .value(),
            "Int(3)"
        );
    }

    // -------------------------------------------------------- benchmarks
    //
    // The `benches/` entries used to be checked here, one agreement test
    // each. `crates/cove-cli/tests/differential.rs` now runs the whole corpus
    // — every `tests/e2e` case and every `examples/` and `benches/` entry —
    // through both backends and compares the value, the console, the outcome,
    // and the filesystem the run left, so these were the same ground covered
    // less thoroughly and twice, at half a minute of every `cargo test`.
    //
    // What stays here is what is about the VM itself rather than about the two
    // backends agreeing: one instruction, one construct, one refusal at a
    // time.
}
