//! A static check that a lowered program is well formed.
//!
//! The machine takes the lowering's word for a great deal: that a slot index
//! is in the frame, that a jump lands on an instruction, that a call passes
//! the number of arguments the callee declares, and — the one that matters
//! most — that a slot's [`Repr`] is what [`Function::refs`] says it is. A
//! collection walks frames using that map, so a lowering that wrote a
//! reference into a slot the map calls an `Int` would produce a dangling
//! reference at the next collection and a wrong answer some time after that.
//!
//! This is where those assumptions are checked, once, before anything runs.
//! It is not a type checker: `cove-sema` already did that, and a failure here
//! is a bug in the lowering rather than a fault in the program. It exists so
//! that such a bug is a loud failure at lowering time instead of a quiet one
//! at collection time.

use crate::inst::{Compare, Inst, Len, Num, Slot};
use crate::program::{Function, FunctionId, Program};
use crate::repr::{RefMap, Repr};

/// A way in which a lowered program is not well formed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invalid {
    /// The function the fault is in, as `module.name`.
    pub function: String,
    /// The instruction it is at, or `None` when the fault is the function's
    /// own — a frame whose reference map disagrees with its reprs, say.
    pub pc: Option<usize>,
    pub what: String,
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.pc {
            Some(pc) => write!(f, "{}+{pc}: {}", self.function, self.what),
            None => write!(f, "{}: {}", self.function, self.what),
        }
    }
}

/// Checks every function of `program`, reporting every fault rather than the
/// first: one lowering bug usually shows up in several places, and seeing
/// all of them is what says which one is the cause.
pub fn verify(program: &Program) -> Result<(), Vec<Invalid>> {
    let mut faults = Vec::new();
    for (index, function) in program.functions.iter().enumerate() {
        Check {
            program,
            function,
            id: FunctionId(index as u32),
            faults: &mut faults,
        }
        .run();
    }
    if faults.is_empty() {
        Ok(())
    } else {
        Err(faults)
    }
}

struct Check<'a> {
    program: &'a Program,
    function: &'a Function,
    id: FunctionId,
    faults: &'a mut Vec<Invalid>,
}

impl Check<'_> {
    fn run(&mut self) {
        self.check_frame();
        for pc in 0..self.function.code.len() {
            self.check_inst(pc);
        }
        self.check_falls_off_the_end();
    }

    fn fault(&mut self, pc: Option<usize>, what: impl Into<String>) {
        self.faults.push(Invalid {
            function: self.function.qualified(),
            pc,
            what: what.into(),
        });
    }

    /// The frame's own invariants: the parameters fit, the spans line up, and
    /// the reference map is the one the reprs imply.
    fn check_frame(&mut self) {
        let size = self.function.frame_size();
        if self.function.arity > size {
            self.fault(
                None,
                format!(
                    "declares {} parameters but a frame of {size} slots",
                    self.function.arity
                ),
            );
        }
        if self.function.spans.len() != self.function.code.len() {
            self.fault(
                None,
                format!(
                    "has {} instructions but {} spans",
                    self.function.code.len(),
                    self.function.spans.len()
                ),
            );
        }
        let expected = RefMap::of(&self.function.reprs);
        if expected != self.function.refs {
            self.fault(
                None,
                "reference map disagrees with the frame's reprs, so a collection would \
                 scan the wrong slots"
                    .to_string(),
            );
        }
        for capture in &self.function.captures {
            match self.function.repr(capture.slot) {
                Some(repr) if repr == capture.repr => {}
                Some(repr) => self.fault(
                    None,
                    format!(
                        "capture `{}` is declared {} but its slot {} holds {repr}",
                        capture.name, capture.repr, capture.slot
                    ),
                ),
                None => self.fault(
                    None,
                    format!(
                        "capture `{}` names slot {}, outside a frame of {size}",
                        capture.name, capture.slot
                    ),
                ),
            }
        }
    }

    /// A function whose last instruction can fall through has nowhere to go.
    fn check_falls_off_the_end(&mut self) {
        let last = self.function.code.len().checked_sub(1);
        let ends = matches!(
            last.map(|pc| &self.function.code[pc]),
            Some(Inst::Return { .. } | Inst::Jump { .. } | Inst::Switch { .. } | Inst::Trap { .. })
        );
        if !ends {
            self.fault(
                last,
                "the last instruction can fall through, and there is nothing after it",
            );
        }
    }

    fn check_inst(&mut self, pc: usize) {
        let inst = self.function.code[pc].clone();
        let at = Some(pc);
        match inst {
            Inst::Unit { dst } => self.expect(at, dst, &[Repr::Unit]),
            Inst::Bool { dst, .. } => self.expect(at, dst, &[Repr::Bool]),
            Inst::Int { dst, .. } => self.expect(at, dst, &[Repr::Int, Repr::Duration]),
            Inst::Float { dst, .. } => self.expect(at, dst, &[Repr::Float]),
            Inst::Str { dst, text } => {
                self.expect(at, dst, &[Repr::Ref]);
                self.in_range(at, text.index(), self.program.strings.len(), "string");
            }
            Inst::Move { dst, src } => {
                if let (Some(d), Some(s)) = (self.repr(at, dst), self.repr(at, src)) {
                    if d != s {
                        self.fault(at, format!("moves {s} into a slot that holds {d}"));
                    }
                }
            }
            Inst::Clear { slot } => {
                // Clearing anything else would be a store of zero into a
                // scalar, which is a lowering bug rather than a cheap no-op.
                self.expect(at, slot, &[Repr::Ref, Repr::Addr]);
            }
            Inst::Neg { num, dst, a } => {
                let want = Self::numeric(num);
                self.expect(at, dst, want);
                self.expect(at, a, want);
            }
            Inst::Arith { num, dst, a, b, .. } => {
                let want = Self::numeric(num);
                self.expect(at, dst, want);
                self.expect(at, a, want);
                self.expect(at, b, want);
            }
            Inst::Cmp { on, dst, a, b, .. } => {
                self.expect(at, dst, &[Repr::Bool]);
                let want: &[Repr] = match on {
                    Compare::Int => &[Repr::Int, Repr::Duration],
                    Compare::Float => &[Repr::Float],
                    Compare::Bool => &[Repr::Bool],
                    Compare::Str => &[Repr::Ref],
                    // `is` compares words, and the only words whose identity
                    // is a language-level question are references.
                    Compare::Identity => &[Repr::Ref],
                };
                self.expect(at, a, want);
                self.expect(at, b, want);
            }
            Inst::Not { dst, a } => {
                self.expect(at, dst, &[Repr::Bool]);
                self.expect(at, a, &[Repr::Bool]);
            }
            Inst::Convert { to, dst, a } => {
                let (from, into) = match to {
                    crate::inst::Convert::IntToFloat => (Repr::Int, Repr::Float),
                    crate::inst::Convert::FloatToInt => (Repr::Float, Repr::Int),
                };
                self.expect(at, a, &[from]);
                self.expect(at, dst, &[into]);
            }
            Inst::Jump { to } => self.target(at, to),
            Inst::BranchFalse { cond, to } => {
                self.expect(at, cond, &[Repr::Bool]);
                self.target(at, to);
            }
            Inst::Switch { on, table } => {
                self.expect(at, on, &[Repr::Int]);
                if self.in_range(at, table.index(), self.program.tables.len(), "table") {
                    let table = self.program.table(table).clone();
                    for to in table.targets.iter().chain(std::iter::once(&table.default)) {
                        self.target(at, *to);
                    }
                }
            }
            Inst::Return { src } => {
                let returns = self.function.returns;
                self.expect(at, src, &[returns]);
            }
            Inst::Call { dst, callee, args } => {
                if !self.in_range(at, callee.index(), self.program.functions.len(), "function") {
                    return;
                }
                let target = self.program.function(callee);
                self.expect(at, dst, &[target.returns]);
                let expected: Vec<Repr> = target.reprs[..target.arity as usize].to_vec();
                let name = target.qualified();
                self.check_args(at, args, &expected, &name);
            }
            Inst::CallClosure { dst, closure, args } => {
                self.expect(at, closure, &[Repr::Ref]);
                self.repr(at, dst);
                self.each_arg(at, args);
            }
            Inst::CallHost { dst, op, args } => {
                if self.in_range(at, op.index(), self.program.host_ops.len(), "host op") {
                    let result = self.program.host_op(op).result;
                    self.expect(at, dst, &[result]);
                }
                self.each_arg(at, args);
            }
            Inst::CallBuiltin { dst, builtin, args } => {
                if self.in_range(at, builtin.index(), self.program.builtins.len(), "builtin") {
                    let result = self.program.builtin(builtin).result;
                    self.expect(at, dst, &[result]);
                }
                self.each_arg(at, args);
            }
            Inst::Alloc { dst, layout, len } => {
                self.expect(at, dst, &[Repr::Ref]);
                self.in_range(at, layout.index(), self.program.layouts.len(), "layout");
                if let Len::Slot(slot) = len {
                    self.expect(at, slot, &[Repr::Int]);
                }
            }
            Inst::GetWord { dst, obj, .. } => {
                self.expect(at, obj, &[Repr::Ref]);
                self.repr(at, dst);
            }
            Inst::SetWord { obj, src, .. } => {
                self.expect(at, obj, &[Repr::Ref]);
                self.repr(at, src);
            }
            Inst::GetElem { dst, obj, index } => {
                self.expect(at, obj, &[Repr::Ref]);
                self.expect(at, index, &[Repr::Int]);
                self.repr(at, dst);
            }
            Inst::SetElem { obj, index, src } => {
                self.expect(at, obj, &[Repr::Ref]);
                self.expect(at, index, &[Repr::Int]);
                self.repr(at, src);
            }
            Inst::Len { dst, obj } => {
                self.expect(at, obj, &[Repr::Ref]);
                self.expect(at, dst, &[Repr::Int]);
            }
            Inst::LayoutOf { dst, obj } => {
                self.expect(at, obj, &[Repr::Ref]);
                self.expect(at, dst, &[Repr::Int]);
            }
            Inst::AddrOfSlot { dst, slot } => {
                self.expect(at, dst, &[Repr::Addr]);
                self.repr(at, slot);
            }
            Inst::AddrOfWord { dst, obj, .. } => {
                self.expect(at, dst, &[Repr::Addr]);
                self.expect(at, obj, &[Repr::Ref]);
            }
            Inst::AddrOfElem { dst, obj, index } => {
                self.expect(at, dst, &[Repr::Addr]);
                self.expect(at, obj, &[Repr::Ref]);
                self.expect(at, index, &[Repr::Int]);
            }
            Inst::Load { dst, addr } => {
                self.expect(at, addr, &[Repr::Addr]);
                self.repr(at, dst);
            }
            Inst::Store { addr, src } => {
                self.expect(at, addr, &[Repr::Addr]);
                self.repr(at, src);
            }
            Inst::Box { dst, src, repr } => {
                self.expect(at, dst, &[Repr::Ref]);
                self.expect(at, src, &[repr]);
            }
            Inst::Unbox { dst, src, repr } => {
                self.expect(at, src, &[Repr::Ref]);
                self.expect(at, dst, &[repr]);
            }
            Inst::Trap { message } => {
                self.in_range(at, message.index(), self.program.strings.len(), "string");
            }
        }
    }

    fn numeric(num: Num) -> &'static [Repr] {
        match num {
            // A `Duration` is nanoseconds, and nanoseconds add like
            // integers. Only the boundary cares what the answer is called.
            Num::Int => &[Repr::Int, Repr::Duration],
            Num::Float => &[Repr::Float],
        }
    }

    /// What slot `slot` holds, reporting a slot outside the frame.
    fn repr(&mut self, at: Option<usize>, slot: Slot) -> Option<Repr> {
        match self.function.repr(slot) {
            Some(repr) => Some(repr),
            None => {
                let size = self.function.frame_size();
                self.fault(at, format!("names slot {slot}, outside a frame of {size}"));
                None
            }
        }
    }

    fn expect(&mut self, at: Option<usize>, slot: Slot, want: &[Repr]) {
        let Some(found) = self.repr(at, slot) else {
            return;
        };
        if !want.contains(&found) {
            let names: Vec<&str> = want.iter().map(|repr| repr.name()).collect();
            self.fault(
                at,
                format!(
                    "slot {slot} holds {found}, but this wants {}",
                    names.join(" or ")
                ),
            );
        }
    }

    fn target(&mut self, at: Option<usize>, to: u32) {
        if to as usize >= self.function.code.len() {
            let len = self.function.code.len();
            self.fault(at, format!("jumps to {to}, past the {len} instructions"));
        }
    }

    fn in_range(&mut self, at: Option<usize>, index: usize, len: usize, what: &str) -> bool {
        if index >= len {
            self.fault(at, format!("names {what} {index}, and there are {len}"));
            false
        } else {
            true
        }
    }

    /// Every argument slot is in the frame, whatever it holds.
    fn each_arg(&mut self, at: Option<usize>, args: crate::ArgsId) {
        if !self.in_range(at, args.index(), self.program.args.len(), "argument list") {
            return;
        }
        for slot in self.program.arg_list(args).to_vec() {
            self.repr(at, slot);
        }
    }

    /// Every argument slot holds what the callee's parameter declares.
    fn check_args(&mut self, at: Option<usize>, args: crate::ArgsId, want: &[Repr], name: &str) {
        if !self.in_range(at, args.index(), self.program.args.len(), "argument list") {
            return;
        }
        let passed = self.program.arg_list(args).to_vec();
        if passed.len() != want.len() {
            self.fault(
                at,
                format!(
                    "passes {} arguments to `{name}`, which declares {}",
                    passed.len(),
                    want.len()
                ),
            );
            return;
        }
        for (slot, repr) in passed.into_iter().zip(want) {
            self.expect(at, slot, &[*repr]);
        }
    }
}

/// The id of the function being checked is carried so that a future fault
/// can name it by id as well as by name; nothing reads it yet.
impl Check<'_> {
    #[allow(dead_code)]
    fn id(&self) -> FunctionId {
        self.id
    }
}
