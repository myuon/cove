//! The dispatch loop.
//!
//! One `Machine` runs one task's frames over one [`Memory`]. It is a
//! register machine: every instruction names its operands and its
//! destination by slot, and a slot is `memory[frame_base + slot]`.
//!
//! # There is no Rust recursion here
//!
//! A Cove call pushes a frame onto [`Machine::frames`] and continues the same
//! loop. Nothing about a call grows the native stack, so how deep a Cove
//! program may recurse is decided by [`STACK_WORDS`] alone rather than by how
//! large a Rust stack frame the dispatch loop happens to compile to — which
//! is a number that changes when an unrelated instruction is added.
//!
//! # There is no `Value` here
//!
//! Ordinary Cove-to-Cove execution moves words and heap objects. The public
//! `Value` is built at the boundary — a Host call, an entry's answer, a trace
//! capture — and nowhere else. There is no operand `Vec<Value>`, no argument
//! buffer, no spill area and no fallback path, which is what ADR 0034 asks
//! for and what the predecessor could not say.

use cove_diag::Span;
use cove_lir::{
    ArithOp, CmpOp, Compare, Convert, FunctionId, Inst, LayoutId, Len, Num, Program, Repr, Shape,
    Slot, StrId,
};

use crate::budget::Budget;
use crate::error::RuntimeError;
use crate::lvm::mem::{Collected, Memory, Overflow, Roots};

/// How many instructions run between two budget checks.
///
/// A budget check reads an atomic and a clock, and doing that per instruction
/// would cost more than most instructions do. Doing it per *call* would let a
/// tight arithmetic loop run unbounded. A fixed stride is the arrangement that
/// bounds both: the run notices a cancellation within a known number of
/// instructions, whatever it is doing.
pub(crate) const SAFEPOINT_STRIDE: u64 = 1024;

/// One live call.
///
/// The top of [`Machine::frames`] is the frame currently executing, not the
/// caller of it. That costs a write of `pc` before anything that can collect
/// or fail, and it buys a collector and an error reporter that need no
/// special case for "and also the one in the local variables".
struct Frame {
    function: FunctionId,
    /// The linear address of slot 0.
    base: u64,
    /// Where this frame resumes: the instruction after the call it is
    /// suspended at, or the one about to run.
    pc: u32,
    /// The slot of the *caller's* frame this call's answer is written to.
    dst: Slot,
}

/// The frames of one task, as the collector sees them.
///
/// A snapshot of addresses rather than a live view, because a collection
/// takes `&mut Memory` and reading a slot takes `&Memory`. Gathering first
/// costs one pass over the reference slots — which the collector was going to
/// make anyway — and keeps [`Memory`] free of any idea of what a frame is.
struct Held<'a>(&'a [u64]);

impl Roots for Held<'_> {
    fn each_root(&self, f: &mut dyn FnMut(u64)) {
        for &addr in self.0 {
            f(addr);
        }
    }
}

/// One task's execution over one linear memory.
pub(crate) struct Machine<'a> {
    program: &'a Program,
    mem: Memory,
    frames: Vec<Frame>,
    /// The string object for each [`StrId`], allocated on first use.
    ///
    /// A literal in a loop allocates once for the run rather than once per
    /// turn. The table is a root for as long as the machine lives, which is
    /// the price: a string mentioned once and never reached again is retained.
    /// That is the right trade for a *literal*, which the program named
    /// statically and can name again.
    interned: Vec<u64>,
    /// Reused across collections so a collection does not allocate.
    roots: Vec<u64>,
    instructions: u64,
    collected: Collected,
}

impl<'a> Machine<'a> {
    pub(crate) fn new(program: &'a Program, heap_words: usize) -> Machine<'a> {
        Machine {
            program,
            mem: Memory::new(heap_words),
            frames: Vec::new(),
            interned: vec![0; program.strings.len()],
            roots: Vec::new(),
            instructions: 0,
            collected: Collected::default(),
        }
    }

    /// How many instructions this machine has run.
    pub(crate) fn instructions(&self) -> u64 {
        self.instructions
    }

    /// What every collection so far has done.
    pub(crate) fn collected(&self) -> Collected {
        self.collected
    }

    /// Runs `entry` with `args` already in word form, answering its word.
    ///
    /// The caller converts: this is below the boundary, and nothing here
    /// knows what a public `Value` is.
    pub(crate) fn run(
        &mut self,
        entry: FunctionId,
        args: &[u64],
        budget: &Budget,
    ) -> Result<u64, RuntimeError> {
        let program = self.program;
        let function = program.function(entry);
        debug_assert_eq!(args.len(), function.arity as usize);

        let base = self
            .mem
            .push_frame(function.frame_size())
            .map_err(|Overflow| self.too_deep(function.span))?;
        for (slot, word) in args.iter().enumerate() {
            self.mem.set_slot(base, slot as u32, *word);
        }
        self.frames.push(Frame {
            function: entry,
            base,
            pc: 0,
            dst: 0,
        });
        self.dispatch(budget)
    }

    /// The loop.
    ///
    /// `function`, `base` and `pc` are kept in locals rather than read out of
    /// the top frame on every instruction, and written back at the two points
    /// where something else looks: a collection, and a failure.
    fn dispatch(&mut self, budget: &Budget) -> Result<u64, RuntimeError> {
        let program = self.program;
        let top = self.frames.last().expect("run pushed a frame");
        let mut id = top.function;
        let mut base = top.base;
        let mut pc = top.pc as usize;
        let mut code = &program.function(id).code[..];

        loop {
            self.instructions += 1;
            if self.instructions.is_multiple_of(SAFEPOINT_STRIDE) {
                if let Err(stopped) = budget.safepoint(SAFEPOINT_STRIDE) {
                    self.sync(pc);
                    return Err(budget.to_runtime_error(stopped).at(self.span(id, pc)));
                }
            }

            let inst = &code[pc];
            pc += 1;

            macro_rules! fail {
                ($error:expr) => {{
                    self.sync(pc - 1);
                    return Err($error.at(self.span(id, pc - 1)));
                }};
            }

            match *inst {
                // ---- constants and moves -------------------------------
                Inst::Unit { dst } => self.mem.set_slot(base, dst, 0),
                Inst::Bool { dst, value } => self.mem.set_slot(base, dst, value as u64),
                Inst::Int { dst, value } => self.mem.set_slot(base, dst, value as u64),
                Inst::Float { dst, bits } => self.mem.set_slot(base, dst, bits),
                Inst::Str { dst, text } => {
                    self.sync(pc - 1);
                    match self.intern(text) {
                        Ok(addr) => self.mem.set_slot(base, dst, addr),
                        Err(error) => fail!(error),
                    }
                }
                Inst::Move { dst, src } => {
                    let word = self.mem.slot(base, src);
                    self.mem.set_slot(base, dst, word);
                }
                // The one instruction whose whole purpose is what it stops
                // happening: a reference the frame no longer needs is not a
                // root, so the object it named is unreachable now rather
                // than when this frame returns.
                Inst::Clear { slot } => self.mem.set_slot(base, slot, 0),

                // ---- scalar operations ----------------------------------
                Inst::Neg { num, dst, a } => {
                    let a = self.mem.slot(base, a);
                    let word = match num {
                        Num::Int => match (a as i64).checked_neg() {
                            Some(value) => value as u64,
                            None => fail!(overflowed("negation")),
                        },
                        Num::Float => (-f64::from_bits(a)).to_bits(),
                    };
                    self.mem.set_slot(base, dst, word);
                }
                Inst::Arith { num, op, dst, a, b } => {
                    let (x, y) = (self.mem.slot(base, a), self.mem.slot(base, b));
                    let word = match num {
                        Num::Int => {
                            // Which of the two the operands are decides only
                            // what the message calls the operation. The
                            // arithmetic is the same, because a `Duration` is
                            // nanoseconds and nanoseconds add like integers.
                            let duration = self.repr(id, dst) == Some(Repr::Duration);
                            match int_arith(op, x as i64, y as i64, duration) {
                                Ok(value) => value as u64,
                                Err(error) => fail!(error),
                            }
                        }
                        Num::Float => {
                            let (x, y) = (f64::from_bits(x), f64::from_bits(y));
                            float_arith(op, x, y).to_bits()
                        }
                    };
                    self.mem.set_slot(base, dst, word);
                }
                Inst::Cmp { on, op, dst, a, b } => {
                    let (x, y) = (self.mem.slot(base, a), self.mem.slot(base, b));
                    let answer = match on {
                        Compare::Int => compare(op, (x as i64).cmp(&(y as i64))),
                        Compare::Bool | Compare::Identity => match op {
                            CmpOp::Eq => x == y,
                            CmpOp::Ne => x != y,
                            // The verifier admits only `Eq` and `Ne` here;
                            // ordering a `Bool` or an identity is not a
                            // question the language asks.
                            _ => fail!(RuntimeError::new(
                                "this comparison is not defined for these operands"
                            )),
                        },
                        Compare::Float => {
                            let (x, y) = (f64::from_bits(x), f64::from_bits(y));
                            match op {
                                CmpOp::Eq => x == y,
                                CmpOp::Ne => x != y,
                                CmpOp::Lt => x < y,
                                CmpOp::Le => x <= y,
                                CmpOp::Gt => x > y,
                                CmpOp::Ge => x >= y,
                            }
                        }
                        Compare::Str => {
                            let ordering = self.compare_strings(x, y);
                            compare(op, ordering)
                        }
                    };
                    self.mem.set_slot(base, dst, answer as u64);
                }
                Inst::Not { dst, a } => {
                    let a = self.mem.slot(base, a);
                    self.mem.set_slot(base, dst, (a == 0) as u64);
                }
                Inst::Convert { to, dst, a } => {
                    let a = self.mem.slot(base, a);
                    let word = match to {
                        Convert::IntToFloat => (a as i64 as f64).to_bits(),
                        Convert::FloatToInt => f64::from_bits(a) as i64 as u64,
                    };
                    self.mem.set_slot(base, dst, word);
                }

                // ---- control flow ----------------------------------------
                Inst::Jump { to } => pc = to as usize,
                Inst::BranchFalse { cond, to } => {
                    if self.mem.slot(base, cond) == 0 {
                        pc = to as usize;
                    }
                }
                Inst::Switch { on, table } => {
                    let index = self.mem.slot(base, on) as usize;
                    let table = program.table(table);
                    pc = *table.targets.get(index).unwrap_or(&table.default) as usize;
                }
                Inst::Return { src } => {
                    let answer = self.mem.slot(base, src);
                    self.mem.pop_frame(base);
                    // The frame being left is what says where its answer
                    // goes. Keeping the destination with the callee rather
                    // than re-reading the caller's `Call` means a return
                    // touches one instruction, not two.
                    let done = self.frames.pop().expect("a frame is executing");
                    match self.frames.last() {
                        None => return Ok(answer),
                        Some(caller) => {
                            id = caller.function;
                            base = caller.base;
                            pc = caller.pc as usize;
                            code = &program.function(id).code[..];
                            self.mem.set_slot(base, done.dst, answer);
                        }
                    }
                }

                // ---- calls -------------------------------------------------
                Inst::Call { dst, callee, args } => {
                    let target = program.function(callee);
                    let list = program.arg_list(args);
                    let callee_base = match self.mem.push_frame(target.frame_size()) {
                        Ok(base) => base,
                        Err(Overflow) => fail!(self.too_deep_error()),
                    };
                    for (slot, src) in list.iter().enumerate() {
                        let word = self.mem.slot(base, *src);
                        self.mem.set_slot(callee_base, slot as u32, word);
                    }
                    self.sync(pc);
                    self.frames.push(Frame {
                        function: callee,
                        base: callee_base,
                        pc: 0,
                        dst,
                    });
                    id = callee;
                    base = callee_base;
                    pc = 0;
                    code = &program.function(id).code[..];
                }
                Inst::CallClosure { .. } | Inst::CallHost { .. } | Inst::CallBuiltin { .. } => {
                    fail!(RuntimeError::new(
                        "this call is not lowered by the linear-memory backend yet"
                    ))
                }

                // ---- the heap ----------------------------------------------
                Inst::Alloc { dst, layout, len } => {
                    let len = match len {
                        Len::Fixed => 0,
                        Len::Count(n) => n,
                        Len::Slot(slot) => self.mem.slot(base, slot) as u32,
                    };
                    self.sync(pc - 1);
                    match self.allocate(layout, len) {
                        Ok(addr) => self.mem.set_slot(base, dst, addr),
                        Err(error) => fail!(error),
                    }
                }
                Inst::GetWord { dst, obj, at } => {
                    let addr = self.mem.slot(base, obj);
                    match self.checked(addr, at) {
                        Ok(()) => {
                            let word = self.mem.payload(addr, at);
                            self.mem.set_slot(base, dst, word);
                        }
                        Err(error) => fail!(error),
                    }
                }
                Inst::SetWord { obj, at, src } => {
                    let addr = self.mem.slot(base, obj);
                    let word = self.mem.slot(base, src);
                    match self.checked(addr, at) {
                        Ok(()) => self.mem.set_payload(addr, at, word),
                        Err(error) => fail!(error),
                    }
                }
                Inst::GetElem { dst, obj, index } => {
                    let addr = self.mem.slot(base, obj);
                    let at = self.mem.slot(base, index) as i64;
                    match self.element(addr, at) {
                        Ok(at) => {
                            let word = self.mem.payload(addr, at);
                            self.mem.set_slot(base, dst, word);
                        }
                        Err(error) => fail!(error),
                    }
                }
                Inst::SetElem { obj, index, src } => {
                    let addr = self.mem.slot(base, obj);
                    let at = self.mem.slot(base, index) as i64;
                    let word = self.mem.slot(base, src);
                    match self.element(addr, at) {
                        Ok(at) => self.mem.set_payload(addr, at, word),
                        Err(error) => fail!(error),
                    }
                }
                Inst::Len { dst, obj } => {
                    let addr = self.mem.slot(base, obj);
                    if addr == 0 {
                        fail!(null_object());
                    }
                    let len = self.mem.object_len(addr) as i64;
                    self.mem.set_slot(base, dst, len as u64);
                }

                // ---- places --------------------------------------------------
                Inst::AddrOfSlot { dst, slot } => self.mem.set_slot(base, dst, base + slot as u64),
                Inst::AddrOfWord { dst, obj, at } => {
                    let addr = self.mem.slot(base, obj);
                    match self.checked(addr, at) {
                        Ok(()) => {
                            let word = self.mem.payload_addr(addr, at);
                            self.mem.set_slot(base, dst, word);
                        }
                        Err(error) => fail!(error),
                    }
                }
                Inst::AddrOfElem { dst, obj, index } => {
                    let addr = self.mem.slot(base, obj);
                    let at = self.mem.slot(base, index) as i64;
                    match self.element(addr, at) {
                        Ok(at) => {
                            let word = self.mem.payload_addr(addr, at);
                            self.mem.set_slot(base, dst, word);
                        }
                        Err(error) => fail!(error),
                    }
                }
                Inst::Load { dst, addr } => {
                    let addr = self.mem.slot(base, addr);
                    let word = self.mem.read(addr);
                    self.mem.set_slot(base, dst, word);
                }
                Inst::Store { addr, src } => {
                    let addr = self.mem.slot(base, addr);
                    let word = self.mem.slot(base, src);
                    self.mem.write(addr, word);
                }

                // ---- erasure ---------------------------------------------------
                Inst::Box { dst, src, repr } => {
                    let word = self.mem.slot(base, src);
                    self.sync(pc - 1);
                    let boxed = match self.allocate(self.boxed_layout(), 0) {
                        Ok(addr) => addr,
                        Err(error) => fail!(error),
                    };
                    self.mem.set_payload(boxed, 0, repr.tag());
                    self.mem.set_payload(boxed, 1, word);
                    self.mem.set_slot(base, dst, boxed);
                }
                Inst::Unbox { dst, src, repr } => {
                    let addr = self.mem.slot(base, src);
                    if addr == 0 {
                        fail!(null_object());
                    }
                    let tag = self.mem.payload(addr, 0);
                    if Repr::from_tag(tag) != Some(repr) {
                        fail!(RuntimeError::new(
                            "this value is not of the type it is being read as"
                        ));
                    }
                    let word = self.mem.payload(addr, 1);
                    self.mem.set_slot(base, dst, word);
                }

                // ---- failure -----------------------------------------------------
                Inst::Trap { message } => {
                    let message = program.string(message).to_string();
                    fail!(RuntimeError::new(message))
                }
            }
        }
    }

    /// Writes the local program counter back into the top frame.
    ///
    /// Called before anything that reads the frames: a collection, which
    /// walks them for roots, and a failure, which reads a span out of one.
    fn sync(&mut self, pc: usize) {
        if let Some(frame) = self.frames.last_mut() {
            frame.pc = pc as u32;
        }
    }

    fn span(&self, id: FunctionId, pc: usize) -> Span {
        self.program.function(id).span_at(pc)
    }

    fn repr(&self, id: FunctionId, slot: Slot) -> Option<Repr> {
        self.program.function(id).repr(slot)
    }

    fn too_deep(&self, span: Span) -> RuntimeError {
        self.too_deep_error().at(span)
    }

    fn too_deep_error(&self) -> RuntimeError {
        RuntimeError::new("this call nests too deeply")
            .with_rule("A recursion that does not terminate is stopped rather than left to run.")
    }

    /// Allocates, collecting once if the first attempt does not fit.
    fn allocate(&mut self, layout: LayoutId, len: u32) -> Result<u64, RuntimeError> {
        let words = self.program.layout(layout).payload_words(len);
        if let Some(addr) = self.mem.alloc(layout, len, words) {
            return Ok(addr);
        }
        self.collect();
        self.mem
            .alloc(layout, len, words)
            .ok_or_else(|| RuntimeError::new("this run has no memory left"))
    }

    /// Marks from every live frame and every interned string, then sweeps.
    fn collect(&mut self) {
        let program = self.program;
        let mut roots = std::mem::take(&mut self.roots);
        roots.clear();
        for frame in &self.frames {
            let function = program.function(frame.function);
            for slot in function.refs.iter() {
                let word = self.mem.slot(frame.base, slot);
                if word != 0 {
                    roots.push(word);
                }
            }
        }
        roots.extend(self.interned.iter().copied().filter(|addr| *addr != 0));
        self.collected = self.mem.collect(&program.layouts, &Held(&roots));
        self.roots = roots;
    }

    /// The string object for `text`, allocated the first time it is asked for.
    fn intern(&mut self, text: StrId) -> Result<u64, RuntimeError> {
        if self.interned[text.index()] != 0 {
            return Ok(self.interned[text.index()]);
        }
        let bytes = self.program.string(text).clone();
        let addr = self.allocate(self.program.str_layout, bytes.len() as u32)?;
        for (at, chunk) in bytes.as_bytes().chunks(8).enumerate() {
            let mut word = 0u64;
            for (i, byte) in chunk.iter().enumerate() {
                word |= (*byte as u64) << (i * 8);
            }
            self.mem.set_payload(addr, at as u32, word);
        }
        self.interned[text.index()] = addr;
        Ok(addr)
    }

    /// The layout a [`Inst::Box`] allocates.
    fn boxed_layout(&self) -> LayoutId {
        self.program
            .layouts
            .iter()
            .position(|layout| matches!(layout.shape, Shape::Boxed))
            .map(|at| LayoutId(at as u32))
            .unwrap_or(LayoutId::FREE)
    }

    /// Checks that `addr` is an object with a payload word `at`.
    ///
    /// A reference slot carries no layout, so the object is the only thing
    /// that can say how wide it is. The lowering computed `at` from the type
    /// the checker settled, so this should never refuse — and it is here
    /// because "should never" is not "cannot", and reading past an object
    /// into whatever follows it would be a silent wrong answer rather than a
    /// loud one.
    fn checked(&self, addr: u64, at: u32) -> Result<(), RuntimeError> {
        if addr == 0 {
            return Err(null_object());
        }
        let layout = self.program.layout(self.mem.object_layout(addr));
        let words = layout.payload_words(self.mem.object_len(addr));
        if at >= words {
            return Err(RuntimeError::new(format!(
                "this reads word {at} of a `{}`, which has {words}",
                layout.name
            )));
        }
        Ok(())
    }

    /// Turns a language-level index into a payload offset.
    fn element(&self, addr: u64, at: i64) -> Result<u32, RuntimeError> {
        if addr == 0 {
            return Err(null_object());
        }
        let len = self.mem.object_len(addr) as i64;
        if at < 0 || at >= len {
            return Err(
                RuntimeError::new(format!("index {at} is outside a collection of {len}"))
                    .with_rule("An index outside a collection is a broken invariant."),
            );
        }
        Ok(at as u32)
    }

    /// Orders two string objects by their bytes.
    fn compare_strings(&self, a: u64, b: u64) -> std::cmp::Ordering {
        let bytes = |addr: u64| -> Vec<u8> {
            if addr == 0 {
                return Vec::new();
            }
            let len = self.mem.object_len(addr) as usize;
            let mut out = Vec::with_capacity(len);
            for at in 0..len.div_ceil(8) {
                let word = self.mem.payload(addr, at as u32);
                for i in 0..8 {
                    if out.len() == len {
                        break;
                    }
                    out.push((word >> (i * 8)) as u8);
                }
            }
            out
        };
        bytes(a).cmp(&bytes(b))
    }
}

fn compare(op: CmpOp, ordering: std::cmp::Ordering) -> bool {
    use std::cmp::Ordering::*;
    match op {
        CmpOp::Eq => ordering == Equal,
        CmpOp::Ne => ordering != Equal,
        CmpOp::Lt => ordering == Less,
        CmpOp::Le => ordering != Greater,
        CmpOp::Gt => ordering == Greater,
        CmpOp::Ge => ordering != Less,
    }
}

/// `Int` arithmetic, with the language's messages.
///
/// The messages are the interpreter's, word for word, because overflow and
/// division by zero are rules of the language rather than of a backend. The
/// differential corpus compares them.
fn int_arith(op: ArithOp, a: i64, b: i64, duration: bool) -> Result<i64, RuntimeError> {
    let named = |what: &'static str| -> &'static str {
        if duration {
            "duration arithmetic"
        } else {
            what
        }
    };
    match op {
        ArithOp::Add => a
            .checked_add(b)
            .ok_or_else(|| overflowed(named("addition"))),
        ArithOp::Sub => a
            .checked_sub(b)
            .ok_or_else(|| overflowed(named("subtraction"))),
        ArithOp::Mul => a
            .checked_mul(b)
            .ok_or_else(|| overflowed(named("multiplication"))),
        ArithOp::Div => {
            if b == 0 {
                Err(divided_by_zero("division"))
            } else {
                a.checked_div(b).ok_or_else(|| overflowed("division"))
            }
        }
        ArithOp::Rem => {
            if b == 0 {
                Err(divided_by_zero("remainder"))
            } else {
                a.checked_rem(b).ok_or_else(|| overflowed("remainder"))
            }
        }
    }
}

fn float_arith(op: ArithOp, a: f64, b: f64) -> f64 {
    match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div => a / b,
        ArithOp::Rem => a % b,
    }
}

fn overflowed(operation: &str) -> RuntimeError {
    RuntimeError::new(format!("`Int` {operation} overflowed"))
        .with_rule("Integer overflow is a broken invariant, not a wrapped result.")
}

fn divided_by_zero(operation: &str) -> RuntimeError {
    RuntimeError::new(format!("`Int` {operation} by zero"))
        .with_rule("Division and remainder by zero are broken invariants.")
}

/// A reference slot held null where an object was needed.
///
/// This is not a language-level `nil`: Cove has none. It is a lowering bug
/// reaching the machine, reported rather than read through.
fn null_object() -> RuntimeError {
    RuntimeError::new("this value was read before it was given one")
}

#[cfg(test)]
mod tests {
    use super::*;
    use cove_lir::{ArgsId, Capture, Function, Layout, RefMap, Table, TableId};
    use std::sync::Arc;

    /// Builds a program by hand.
    ///
    /// The lowering is a separate piece and a separate test suite. What is
    /// under test here is the machine, so its programs are written in the IR
    /// directly: a failure is then unambiguously the loop's, and a change to
    /// the lowering cannot quietly stop exercising an instruction.
    #[derive(Default)]
    struct Build {
        program: Program,
    }

    impl Build {
        fn strings(mut self, texts: &[&str]) -> Build {
            self.program.strings = texts.iter().map(|text| Arc::from(*text)).collect();
            self
        }

        fn layout(&mut self, name: &str, shape: Shape) -> LayoutId {
            if self.program.layouts.is_empty() {
                self.program.layouts.push(Layout::free());
            }
            self.program.layouts.push(Layout {
                name: Arc::from(name),
                shape,
            });
            LayoutId(self.program.layouts.len() as u32 - 1)
        }

        fn args(&mut self, slots: &[Slot]) -> ArgsId {
            self.program.args.push(slots.to_vec());
            ArgsId(self.program.args.len() as u32 - 1)
        }

        fn table(&mut self, targets: &[u32], default: u32) -> TableId {
            self.program.tables.push(Table {
                targets: targets.to_vec(),
                default,
            });
            TableId(self.program.tables.len() as u32 - 1)
        }

        fn function(
            &mut self,
            name: &str,
            arity: u32,
            reprs: &[Repr],
            returns: Repr,
            code: Vec<Inst>,
        ) -> FunctionId {
            let nowhere = Span::new(cove_diag::FileId(0), 0, 0);
            let spans = vec![nowhere; code.len()];
            self.program.functions.push(Function {
                module: Arc::from("t"),
                name: Arc::from(name),
                arity,
                reprs: reprs.to_vec(),
                refs: RefMap::of(reprs),
                returns,
                captures: Vec::<Capture>::new(),
                code,
                spans,
                span: nowhere,
                is_async: false,
            });
            let id = FunctionId(self.program.functions.len() as u32 - 1);
            self.program
                .by_name
                .insert((Arc::from("t"), Arc::from(name)), id);
            id
        }

        /// Checks the program the way the lowering must, so a malformed test
        /// fixture fails as a fixture rather than as a machine bug.
        fn done(self) -> Program {
            cove_lir::verify(&self.program).expect("a hand-written test program is well formed");
            self.program
        }
    }

    fn budget() -> Budget {
        Budget::new(crate::budget::Limits::default())
    }

    fn run(program: &Program, entry: FunctionId, args: &[u64]) -> Result<u64, RuntimeError> {
        Machine::new(program, 1 << 16).run(entry, args, &budget())
    }

    #[test]
    fn a_constant_comes_back() {
        let mut build = Build::default();
        let f = build.function(
            "answer",
            0,
            &[Repr::Int],
            Repr::Int,
            vec![Inst::Int { dst: 0, value: 42 }, Inst::Return { src: 0 }],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[]).unwrap() as i64, 42);
    }

    #[test]
    fn arithmetic_reads_and_writes_slots() {
        let mut build = Build::default();
        let f = build.function(
            "add",
            2,
            &[Repr::Int, Repr::Int, Repr::Int],
            Repr::Int,
            vec![
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[3, 4]).unwrap() as i64, 7);
    }

    /// The messages are the language's, not the backend's; the differential
    /// corpus compares them against the tree-walking oracle word for word.
    #[test]
    fn arithmetic_faults_say_what_the_oracle_says() {
        let cases: Vec<(ArithOp, i64, i64, &str)> = vec![
            (ArithOp::Div, 1, 0, "`Int` division by zero"),
            (ArithOp::Rem, 1, 0, "`Int` remainder by zero"),
            (ArithOp::Add, i64::MAX, 1, "`Int` addition overflowed"),
            (ArithOp::Mul, i64::MAX, 2, "`Int` multiplication overflowed"),
        ];
        for (op, a, b, message) in cases {
            let mut build = Build::default();
            let f = build.function(
                "fault",
                2,
                &[Repr::Int, Repr::Int, Repr::Int],
                Repr::Int,
                vec![
                    Inst::Arith {
                        num: Num::Int,
                        op,
                        dst: 2,
                        a: 0,
                        b: 1,
                    },
                    Inst::Return { src: 2 },
                ],
            );
            let program = build.done();
            let error = run(&program, f, &[a as u64, b as u64]).unwrap_err();
            assert_eq!(error.message, message);
        }
    }

    /// A `Duration` is nanoseconds and its arithmetic is an integer's, so the
    /// only thing that changes is what an overflow is called.
    #[test]
    fn a_duration_overflow_is_named_a_duration_overflow() {
        let mut build = Build::default();
        let f = build.function(
            "late",
            2,
            &[Repr::Duration, Repr::Duration, Repr::Duration],
            Repr::Duration,
            vec![
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        let error = run(&program, f, &[i64::MAX as u64, 1]).unwrap_err();
        assert_eq!(error.message, "`Int` duration arithmetic overflowed");
    }

    #[test]
    fn a_branch_takes_one_side() {
        let mut build = Build::default();
        // fn abs(n) { if n < 0 { -n } else { n } }
        let f = build.function(
            "abs",
            1,
            &[Repr::Int, Repr::Int, Repr::Bool],
            Repr::Int,
            vec![
                Inst::Int { dst: 1, value: 0 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::BranchFalse { cond: 2, to: 4 },
                Inst::Neg {
                    num: Num::Int,
                    dst: 0,
                    a: 0,
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[(-5i64) as u64]).unwrap() as i64, 5);
        assert_eq!(run(&program, f, &[5]).unwrap() as i64, 5);
    }

    #[test]
    fn a_loop_runs_to_its_bound() {
        let mut build = Build::default();
        // fn sum(n) { var t = 0; var i = 0; while i < n { t = t + i; i = i + 1 }; t }
        let f = build.function(
            "sum",
            1,
            &[Repr::Int, Repr::Int, Repr::Int, Repr::Bool, Repr::Int],
            Repr::Int,
            vec![
                Inst::Int { dst: 1, value: 0 },
                Inst::Int { dst: 2, value: 0 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 3,
                    a: 2,
                    b: 0,
                },
                Inst::BranchFalse { cond: 3, to: 8 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 1,
                    a: 1,
                    b: 2,
                },
                Inst::Int { dst: 4, value: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 2,
                    a: 2,
                    b: 4,
                },
                Inst::Jump { to: 2 },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[10]).unwrap() as i64, 45);
    }

    /// A call writes its arguments straight into the callee's slots and its
    /// answer straight into the caller's destination. There is no buffer
    /// between the two frames, and this is what says so.
    #[test]
    fn recursion_nests_frames_and_unwinds_them() {
        let mut build = Build::default();
        let args = build.args(&[3]);
        // fn fact(n) { if n <= 1 { 1 } else { n * fact(n - 1) } }
        let f = build.function(
            "fact",
            1,
            &[Repr::Int, Repr::Int, Repr::Bool, Repr::Int, Repr::Int],
            Repr::Int,
            vec![
                Inst::Int { dst: 1, value: 1 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Le,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::BranchFalse { cond: 2, to: 4 },
                Inst::Return { src: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Sub,
                    dst: 3,
                    a: 0,
                    b: 1,
                },
                Inst::Call {
                    dst: 4,
                    callee: FunctionId(0),
                    args,
                },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Mul,
                    dst: 4,
                    a: 0,
                    b: 4,
                },
                Inst::Return { src: 4 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[10]).unwrap() as i64, 3_628_800);
    }

    /// Depth is bounded by the reserved stack region, not by the Rust stack:
    /// a call does not recurse in the dispatch loop, so this returns an error
    /// rather than ending the process.
    #[test]
    fn an_unbounded_recursion_is_stopped() {
        let mut build = Build::default();
        let args = build.args(&[0]);
        let f = build.function(
            "forever",
            1,
            &[Repr::Int, Repr::Int],
            Repr::Int,
            vec![
                Inst::Call {
                    dst: 1,
                    callee: FunctionId(0),
                    args,
                },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let error = run(&program, f, &[0]).unwrap_err();
        assert_eq!(error.message, "this call nests too deeply");
    }

    /// `bump(var total)` adds to the caller's own binding rather than to a
    /// copy of it. A place is one word holding the address of that binding,
    /// and this is the whole of the mechanism.
    #[test]
    fn a_place_writes_the_callers_own_slot() {
        let mut build = Build::default();
        let args = build.args(&[1]);
        let bump = build.function(
            "bump",
            1,
            &[Repr::Addr, Repr::Int, Repr::Int, Repr::Unit],
            Repr::Unit,
            vec![
                Inst::Load { dst: 1, addr: 0 },
                Inst::Int { dst: 2, value: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 1,
                    a: 1,
                    b: 2,
                },
                Inst::Store { addr: 0, src: 1 },
                Inst::Unit { dst: 3 },
                Inst::Return { src: 3 },
            ],
        );
        let caller = build.function(
            "main",
            0,
            &[Repr::Int, Repr::Addr, Repr::Unit],
            Repr::Int,
            vec![
                Inst::Int { dst: 0, value: 10 },
                Inst::AddrOfSlot { dst: 1, slot: 0 },
                Inst::Call {
                    dst: 2,
                    callee: bump,
                    args,
                },
                Inst::Return { src: 0 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, caller, &[]).unwrap() as i64, 11);
    }

    #[test]
    fn an_object_round_trips_through_its_fields() {
        let mut build = Build::default();
        let point = build.layout(
            "Point",
            Shape::Struct {
                fields: vec![
                    cove_lir::Field {
                        name: Arc::from("x"),
                        repr: Repr::Int,
                    },
                    cove_lir::Field {
                        name: Arc::from("y"),
                        repr: Repr::Int,
                    },
                ],
            },
        );
        let f = build.function(
            "make",
            0,
            &[Repr::Ref, Repr::Int, Repr::Int],
            Repr::Int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: point,
                    len: Len::Fixed,
                },
                Inst::Int { dst: 1, value: 3 },
                Inst::SetWord {
                    obj: 0,
                    at: 0,
                    src: 1,
                },
                Inst::Int { dst: 1, value: 4 },
                Inst::SetWord {
                    obj: 0,
                    at: 1,
                    src: 1,
                },
                Inst::GetWord {
                    dst: 1,
                    obj: 0,
                    at: 0,
                },
                Inst::GetWord {
                    dst: 2,
                    obj: 0,
                    at: 1,
                },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Mul,
                    dst: 1,
                    a: 1,
                    b: 2,
                },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[]).unwrap() as i64, 12);
    }

    /// Reading past an object is a lowering bug, and the machine reports it
    /// rather than reading whatever follows the object in the heap.
    #[test]
    fn a_field_past_the_object_is_refused() {
        let mut build = Build::default();
        let one = build.layout(
            "One",
            Shape::Struct {
                fields: vec![cove_lir::Field {
                    name: Arc::from("x"),
                    repr: Repr::Int,
                }],
            },
        );
        let f = build.function(
            "past",
            0,
            &[Repr::Ref, Repr::Int],
            Repr::Int,
            vec![
                Inst::Alloc {
                    dst: 0,
                    layout: one,
                    len: Len::Fixed,
                },
                Inst::GetWord {
                    dst: 1,
                    obj: 0,
                    at: 3,
                },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        let error = run(&program, f, &[]).unwrap_err();
        assert!(
            error.message.contains("word 3 of a `One`"),
            "{}",
            error.message
        );
    }

    /// The loop allocates in a loop, clearing the slot each turn. Without
    /// `Clear` the frame would hold every string it ever made; with it the
    /// heap stays flat, and this is the test that says so.
    #[test]
    fn clearing_a_slot_lets_the_collector_reclaim() {
        let mut build = Build::default();
        let cell = build.layout(
            "Cell",
            Shape::Elements {
                elem: Repr::Int,
                growable: false,
            },
        );
        let f = build.function(
            "churn",
            1,
            &[Repr::Int, Repr::Int, Repr::Bool, Repr::Ref, Repr::Int],
            Repr::Int,
            vec![
                Inst::Int { dst: 1, value: 0 },
                Inst::Cmp {
                    on: Compare::Int,
                    op: CmpOp::Lt,
                    dst: 2,
                    a: 1,
                    b: 0,
                },
                Inst::BranchFalse { cond: 2, to: 8 },
                Inst::Alloc {
                    dst: 3,
                    layout: cell,
                    len: Len::Count(64),
                },
                Inst::Clear { slot: 3 },
                Inst::Int { dst: 4, value: 1 },
                Inst::Arith {
                    num: Num::Int,
                    op: ArithOp::Add,
                    dst: 1,
                    a: 1,
                    b: 4,
                },
                Inst::Jump { to: 1 },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        // A heap far smaller than 4000 objects of 65 words: the run only
        // finishes because each turn's object is unreachable by the next.
        let mut machine = Machine::new(&program, 4096);
        let answer = machine.run(f, &[4000], &budget()).unwrap();
        assert_eq!(answer as i64, 4000);
        assert!(
            machine.collected().collections > 0,
            "the run should have had to collect"
        );
    }

    #[test]
    fn a_string_literal_is_allocated_once() {
        let mut build = Build::default().strings(&["hello"]);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let f = build.function(
            "twice",
            0,
            &[Repr::Ref, Repr::Ref, Repr::Bool],
            Repr::Bool,
            vec![
                Inst::Str {
                    dst: 0,
                    text: StrId(0),
                },
                Inst::Str {
                    dst: 1,
                    text: StrId(0),
                },
                Inst::Cmp {
                    on: Compare::Identity,
                    op: CmpOp::Eq,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[]).unwrap(), 1);
    }

    #[test]
    fn strings_compare_by_their_bytes() {
        let mut build = Build::default().strings(&["apple", "banana"]);
        let str_layout = build.layout("String", Shape::Str);
        build.program.str_layout = str_layout;
        let f = build.function(
            "order",
            0,
            &[Repr::Ref, Repr::Ref, Repr::Bool],
            Repr::Bool,
            vec![
                Inst::Str {
                    dst: 0,
                    text: StrId(0),
                },
                Inst::Str {
                    dst: 1,
                    text: StrId(1),
                },
                Inst::Cmp {
                    on: Compare::Str,
                    op: CmpOp::Lt,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[]).unwrap(), 1);
    }

    #[test]
    fn a_box_answers_its_tag_and_refuses_another() {
        let mut build = Build::default();
        let boxed = build.layout("Any", Shape::Boxed);
        let _ = boxed;
        let good = build.function(
            "round-trip",
            1,
            &[Repr::Int, Repr::Ref, Repr::Int],
            Repr::Int,
            vec![
                Inst::Box {
                    dst: 1,
                    src: 0,
                    repr: Repr::Int,
                },
                Inst::Unbox {
                    dst: 2,
                    src: 1,
                    repr: Repr::Int,
                },
                Inst::Return { src: 2 },
            ],
        );
        let wrong = build.function(
            "wrong-type",
            1,
            &[Repr::Int, Repr::Ref, Repr::Bool],
            Repr::Bool,
            vec![
                Inst::Box {
                    dst: 1,
                    src: 0,
                    repr: Repr::Int,
                },
                Inst::Unbox {
                    dst: 2,
                    src: 1,
                    repr: Repr::Bool,
                },
                Inst::Return { src: 2 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, good, &[7]).unwrap() as i64, 7);
        assert!(run(&program, wrong, &[7]).is_err());
    }

    #[test]
    fn a_switch_picks_a_case_and_falls_to_its_default() {
        let mut build = Build::default();
        let table = build.table(&[3, 5], 7);
        let f = build.function(
            "pick",
            1,
            &[Repr::Int, Repr::Int],
            Repr::Int,
            vec![
                Inst::Switch { on: 0, table },
                Inst::Int { dst: 1, value: 0 },
                Inst::Return { src: 1 },
                Inst::Int { dst: 1, value: 10 },
                Inst::Return { src: 1 },
                Inst::Int { dst: 1, value: 20 },
                Inst::Return { src: 1 },
                Inst::Int { dst: 1, value: 30 },
                Inst::Return { src: 1 },
            ],
        );
        let program = build.done();
        assert_eq!(run(&program, f, &[0]).unwrap() as i64, 10);
        assert_eq!(run(&program, f, &[1]).unwrap() as i64, 20);
        assert_eq!(run(&program, f, &[9]).unwrap() as i64, 30);
    }

    /// A run that will not stop on its own is stopped by its budget, and the
    /// stride is what bounds how long that takes.
    #[test]
    fn a_cancelled_run_stops_at_a_safepoint() {
        let mut build = Build::default();
        let f = build.function(
            "spin",
            0,
            &[Repr::Int],
            Repr::Int,
            vec![Inst::Int { dst: 0, value: 0 }, Inst::Jump { to: 0 }],
        );
        let program = build.done();
        let cancellation = crate::budget::Cancellation::new();
        let budget =
            Budget::with_cancellation(crate::budget::Limits::default(), cancellation.clone());
        cancellation.cancel();
        let mut machine = Machine::new(&program, 1 << 12);
        assert!(machine.run(f, &[], &budget).is_err());
        assert!(machine.instructions() <= SAFEPOINT_STRIDE + 1);
    }
}
