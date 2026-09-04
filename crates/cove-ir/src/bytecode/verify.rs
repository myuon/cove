//! The check that makes encoded bytes safe to execute without checking again.
//!
//! Issue #245's boundary: **encode, verify once, then trust**. After this
//! answers `Ok`, a dispatch loop may read `a`, `b` and `c` as frame offsets
//! and the payload as a table index without asking whether either is in
//! range, because this asked. What stays a run-time question stays one —
//! division by zero, an object's layout against the layout the instruction
//! names, element bounds, fuel, deadlines, cancellation, host failure.
//!
//! # This is not [`mod@crate::verify`], and the difference is the point
//!
//! [`mod@crate::verify`] checks a **lowering**: it reads `Function::code` as
//! [`Inst`]s and asks whether `crate::lower` produced a well
//! formed program. A fault there is a bug in this compiler, it is reported by
//! a panic, and it is about instructions that are Rust values and therefore
//! cannot be malformed — an `Inst::Copy` always has a `dst`, a `src` and a
//! `layout`, whatever they name.
//!
//! This checks **bytes**. Sixteen bytes can say things no `Inst` can: an
//! opcode that names nothing, a `flags` byte that is not zero, an operand in a
//! field the opcode does not use. So this runs first over the structure — that
//! is [`decode`], which refuses anything that is not the canonical encoding of
//! some instruction — and then over the same program-relative facts the other
//! one checks, driven by [`Op::fields`] rather than by a match on an enum.
//!
//! The two therefore overlap on purpose and neither replaces the other. One is
//! a compiler's self-check over its own output; the other is a loader's check
//! over an input, and it must be **safe against arbitrary bytes** even while
//! the format is internal, because a verifier that is only safe against its
//! own encoder is not a verifier. Nothing in this module indexes with a value
//! it has not bounded, and nothing panics on any sixteen bytes at all.

use crate::inst::Inst;
use crate::layout::LayoutId;
use crate::program::{Function, FunctionId, Program};
use crate::repr::Repr;
use crate::Slot;

use super::decode::decode;
use super::encode::Encoded;
use super::op::{Half, Op, Operand, Payload};
use super::EncodedInst;

/// A way in which encoded bytes are not something that may be run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fault {
    /// The function the fault is in, as `module.name`.
    pub function: String,
    /// The instruction it is at, or `None` when the fault is the run's own —
    /// a program and an encoding of different lengths, say.
    pub pc: Option<usize>,
    pub what: String,
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.pc {
            Some(pc) => write!(f, "{}+{pc}: {}", self.function, self.what),
            None => write!(f, "{}: {}", self.function, self.what),
        }
    }
}

/// Checks a whole encoded program against the program it was encoded from.
///
/// Every fault rather than the first, for [`mod@crate::verify`]'s reason: one
/// cause usually shows up in several places, and seeing all of them is what
/// says which one it is.
pub fn verify(program: &Program, encoded: &Encoded) -> Result<(), Vec<Fault>> {
    let mut faults = Vec::new();
    if encoded.functions.len() != program.functions.len() {
        faults.push(Fault {
            function: "<program>".to_string(),
            pc: None,
            what: format!(
                "has {} encoded functions and the program has {}",
                encoded.functions.len(),
                program.functions.len()
            ),
        });
        return Err(faults);
    }
    for (index, code) in encoded.functions.iter().enumerate() {
        let id = FunctionId(index as u32);
        // The 1:1 encoding is what keeps `Function::spans`, `Local`'s pc
        // ranges and `Table::targets` meaning what they meant, so a run of a
        // different length is a fault about the whole function rather than
        // about one of its instructions.
        let function = program.function(id);
        if code.len() != function.code.len() {
            faults.push(Fault {
                function: function.qualified(),
                pc: None,
                what: format!(
                    "is {} encoded instructions and the function has {}, so a pc means two \
                     things",
                    code.len(),
                    function.code.len()
                ),
            });
        }
        faults.extend(verify_function(program, id, code));
    }
    if faults.is_empty() {
        Ok(())
    } else {
        Err(faults)
    }
}

/// Checks one run of encoded instructions against the frame it runs in.
///
/// The entry point that takes bytes nothing produced: `code` is read as the
/// authority on what will execute, and `program.function(id)` supplies the
/// frame, the parameters and the answer it has to agree with.
pub fn verify_function(program: &Program, id: FunctionId, code: &[EncodedInst]) -> Vec<Fault> {
    let mut check = Check {
        program,
        function: program.function(id),
        code,
        faults: Vec::new(),
    };
    if code.is_empty() {
        check.fault(None, "has no instructions, so there is nowhere to begin");
    }
    for pc in 0..code.len() {
        check.inst(pc);
    }
    check.faults
}

struct Check<'a> {
    program: &'a Program,
    function: &'a Function,
    code: &'a [EncodedInst],
    faults: Vec<Fault>,
}

impl Check<'_> {
    fn fault(&mut self, pc: Option<usize>, what: impl Into<String>) {
        self.faults.push(Fault {
            function: self.function.qualified(),
            pc,
            what: what.into(),
        });
    }

    fn inst(&mut self, pc: usize) {
        let bytes = self.code[pc];
        let at = Some(pc);
        // The structure first: a defined opcode, a zero `flags`, a zero in
        // every field the opcode does not use, an in-range constant, a
        // displacement that lands on some program counter. `decode` is that
        // check, and it is the only thing here that reads a byte it has not
        // been told the shape of.
        let inst = match decode(bytes, pc as u32) {
            Ok(inst) => inst,
            Err(why) => {
                self.fault(at, why.to_string());
                return;
            }
        };
        let op = match Op::from_number(bytes.opcode()) {
            Some(op) => op,
            // Unreachable: `decode` answered `Ok`, so the opcode is defined.
            None => return,
        };
        self.payload(at, op, bytes);
        self.slots(at, op, bytes);
        self.meaning(at, &inst);
    }

    /// Every id in the payload indexes its table, and every half the opcode
    /// leaves for a number is a number the field can hold.
    ///
    /// A `Count` and an `Offset` are 32 bits in a 32-bit half, so their range
    /// is the field's own and there is nothing to check; they are named here
    /// so that the table is read exhaustively rather than by omission.
    fn payload(&mut self, at: Option<usize>, op: Op, bytes: EncodedInst) {
        let Payload::Halves(lo, hi) = op.fields().payload else {
            return;
        };
        for (half, value) in [(lo, bytes.lo()), (hi, bytes.hi())] {
            let len = match half {
                Half::Unused | Half::Count | Half::Offset => continue,
                Half::Function => self.program.functions.len(),
                Half::Str => self.program.strings.len(),
                Half::Layout => self.program.layouts.len(),
                Half::Table => self.program.tables.len(),
                Half::Args => self.program.args.len(),
                Half::Builtin => self.program.builtins.len(),
                Half::HostOp => self.program.host_ops.len(),
            };
            if value as usize >= len {
                let what = half.name();
                self.fault(at, format!("names {what} {value}, and there are {len}"));
            }
        }
    }

    /// The central check, and the one a sixteen-bit slot operand buys.
    ///
    /// *Every slot is inside the function frame* is not forty-nine rules but
    /// one rule over three fields, driven by which of the three the opcode
    /// declares live. A field it does not use was already required to be zero
    /// by [`decode`].
    fn slots(&mut self, at: Option<usize>, op: Op, bytes: EncodedInst) {
        let fields = op.fields();
        let held = [bytes.a(), bytes.b(), bytes.c()];
        let names = ["a", "b", "c"];
        for ((operand, value), name) in fields.operands().into_iter().zip(held).zip(names) {
            let slot = Slot::from(value);
            match operand {
                Operand::Unused => {}
                Operand::Word(want) => {
                    let Some(found) = self.function.repr(slot) else {
                        self.outside(at, name, slot);
                        continue;
                    };
                    if !want.is_empty() && !want.contains(&found) {
                        let names: Vec<&str> = want.iter().map(|repr| repr.name()).collect();
                        self.fault(
                            at,
                            format!(
                                "slot {slot} holds {found}, and this opcode wants {}",
                                names.join(" or ")
                            ),
                        );
                    }
                }
                // The head of a run whose width the payload's layout gives.
                // The layout half was checked above, so a missing one here is
                // a fault already reported and this declines to guess.
                Operand::Value => match self.named_layout(op, bytes) {
                    Some(layout) => {
                        self.fits(at, slot, layout, &format!("the value at {name}"));
                    }
                    None => {
                        if self.function.repr(slot).is_none() {
                            self.outside(at, name, slot);
                        }
                    }
                },
            }
        }
    }

    /// The facts that are about what the instruction *says* rather than about
    /// where its operands are: a branch inside this function, a switch table
    /// whose targets are, a call whose arguments are the callee's parameters,
    /// and a destination as wide as the answer written into it.
    ///
    /// These read the decoded instruction because they are the checks whose
    /// shape differs per opcode; everything uniform is above.
    fn meaning(&mut self, at: Option<usize>, inst: &Inst) {
        match *inst {
            Inst::Jump { to } | Inst::BranchFalse { to, .. } => self.target(at, to),
            Inst::Switch { table, .. } => {
                // The table stays immutable program metadata with absolute
                // targets, and this is where absolute breaks loudly if a
                // table is ever shared between two sites.
                if let Some(table) = self.program.tables.get(table.index()) {
                    let targets = table.targets.clone();
                    let default = table.default;
                    for to in targets.into_iter().chain(std::iter::once(default)) {
                        self.target(at, to);
                    }
                }
            }
            Inst::Return { src } => {
                let returns = self.function.returns;
                self.fits(at, src, returns, "what is returned");
            }
            Inst::Call { dst, callee, args } => {
                let Some(target) = self.program.functions.get(callee.index()) else {
                    return;
                };
                let returns = target.returns;
                let params = target.params.clone();
                let name = target.qualified();
                self.fits(at, dst, returns, "the destination of a call");
                self.args_match(at, args, &params, &name);
            }
            Inst::CallClosure { args, .. } => self.args_fit(at, args),
            Inst::CallHost { dst, op, args } | Inst::CallResource { dst, op, args, .. } => {
                if let Some(op) = self.program.host_ops.get(op.index()) {
                    let result = op.result;
                    self.fits(at, dst, result, "the answer of a host call");
                }
                self.args_fit(at, args);
            }
            Inst::CallBuiltin { dst, builtin, args } => {
                if let Some(builtin) = self.program.builtins.get(builtin.index()) {
                    let result = builtin.result;
                    self.fits(at, dst, result, "the answer of a builtin");
                }
                self.args_fit(at, args);
            }
            _ => {}
        }
    }

    /// Which layout the payload names, where the opcode says it names one.
    fn named_layout(&self, op: Op, bytes: EncodedInst) -> Option<LayoutId> {
        let Payload::Halves(lo, hi) = op.fields().payload else {
            return None;
        };
        let id = match (lo, hi) {
            (Half::Layout, _) => LayoutId(bytes.lo()),
            (_, Half::Layout) => LayoutId(bytes.hi()),
            _ => return None,
        };
        self.program.layouts.get(id.index()).map(|_| id)
    }

    fn outside(&mut self, at: Option<usize>, field: &str, slot: Slot) {
        let size = self.function.frame_size();
        self.fault(
            at,
            format!("{field} names slot {slot}, outside a frame of {size}"),
        );
    }

    /// A branch or a switch target lands on an instruction of this function.
    ///
    /// Under a 1:1 encoding that is any pc in `[0, code.len())`, which is why
    /// "every target is an instruction boundary" needs no arithmetic: a
    /// boundary is what a pc is.
    fn target(&mut self, at: Option<usize>, to: u32) {
        if to as usize >= self.code.len() {
            let len = self.code.len();
            self.fault(at, format!("jumps to {to}, past the {len} instructions"));
        }
    }

    /// The location at `slot` is a value of `layout`: it is inside the frame,
    /// and its words are the layout's words in order.
    ///
    /// The same rule [`mod@crate::verify`] turns on, made about a decoded operand
    /// rather than about an enum field. The extent is what keeps a multiword
    /// copy near the top of a frame from reaching the frame above it, and the
    /// words are what keep a collection from tracing a `Float` or missing a
    /// `Ref`.
    fn fits(&mut self, at: Option<usize>, slot: Slot, layout: LayoutId, what: &str) {
        let Some(described) = self.program.layouts.get(layout.index()) else {
            let size = self.program.layouts.len();
            self.fault(
                at,
                format!("{what} is layout {layout}, and there are {size}"),
            );
            return;
        };
        let words: Vec<Repr> = described.words.clone();
        let name = described.name.clone();
        let size = self.function.frame_size();
        if u64::from(slot) + words.len() as u64 > u64::from(size) {
            self.fault(
                at,
                format!(
                    "{what} is `{name}`, {} words at slot {slot}, and the frame has {size}",
                    words.len()
                ),
            );
            return;
        }
        for (offset, want) in words.iter().enumerate() {
            let found = self.function.reprs[slot as usize + offset];
            if found != *want {
                self.fault(
                    at,
                    format!(
                        "{what} is `{name}`, whose word {offset} is {want}, but slot {} holds \
                         {found}",
                        slot as usize + offset
                    ),
                );
                return;
            }
        }
    }

    /// Every argument is a value location of the layout it names, inside this
    /// frame.
    fn args_fit(&mut self, at: Option<usize>, args: crate::ArgsId) {
        let Some(list) = self.program.args.get(args.index()) else {
            return;
        };
        for (index, arg) in list.clone().into_iter().enumerate() {
            self.fits(at, arg.slot, arg.layout, &format!("argument {index}"));
        }
    }

    /// The same, where the callee declares what it takes: the arity is the
    /// callee's, each argument's layout is the parameter's, and each location
    /// is a value of it.
    fn args_match(
        &mut self,
        at: Option<usize>,
        args: crate::ArgsId,
        want: &[LayoutId],
        name: &str,
    ) {
        let Some(list) = self.program.args.get(args.index()) else {
            return;
        };
        let passed = list.clone();
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
        for (index, (arg, layout)) in passed.into_iter().zip(want).enumerate() {
            if arg.layout != *layout {
                let passed = self.name_of(arg.layout);
                let declared = self.name_of(*layout);
                self.fault(
                    at,
                    format!(
                        "argument {index} of `{name}` is passed as a `{passed}`, and the \
                         parameter is a `{declared}`"
                    ),
                );
                continue;
            }
            self.fits(
                at,
                arg.slot,
                *layout,
                &format!("argument {index} of `{name}`"),
            );
        }
    }

    /// What a layout is called, or its id where the table is too short.
    fn name_of(&self, layout: LayoutId) -> String {
        match self.program.layouts.get(layout.index()) {
            Some(held) => held.name.to_string(),
            None => layout.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cove_diag::{FileId, Span};

    use super::*;
    use crate::bytecode::encode::{encode, encode_function, encode_program};
    use crate::bytecode::{instructions, EncodedInst};
    use crate::inst::{ArithOp, Num};
    use crate::layout::{Layout, Shape};
    use crate::program::{Arg, Table};
    use crate::repr::RefMap;
    use crate::{ArgsId, LayoutId, StrId, TableId};

    const INT: LayoutId = LayoutId(0);
    const STR: LayoutId = LayoutId(1);
    /// Two `Int` words, so that a value location has an extent to run off the
    /// end of.
    const POINT: LayoutId = LayoutId(2);
    const BOXED: LayoutId = LayoutId(3);

    fn layouts() -> Vec<Layout> {
        vec![
            Layout::word("Int", Repr::Int),
            Layout::object("String", Shape::Str),
            Layout::inline(
                "Point",
                Shape::Struct {
                    fields: Vec::new(),
                    opaque: false,
                },
                vec![Repr::Int, Repr::Int],
            ),
            Layout::object("Any", Shape::Boxed),
        ]
    }

    fn span() -> Span {
        Span::new(FileId(0), 0, 0)
    }

    /// A frame of `[int, int, ref, bool]`, answering an `Int`.
    fn function(code: Vec<Inst>) -> Function {
        let reprs = vec![Repr::Int, Repr::Int, Repr::Ref, Repr::Bool];
        Function {
            module: Arc::from("m"),
            name: Arc::from("f"),
            params: Vec::new(),
            spans: vec![span(); code.len()],
            refs: RefMap::of(&reprs),
            reprs,
            returns: INT,
            captures: Vec::new(),
            code,
            locals: Vec::new(),
            span: span(),
            is_async: false,
            stub: false,
        }
    }

    fn program(code: Vec<Inst>) -> Program {
        Program {
            functions: vec![function(code)],
            layouts: layouts(),
            str_layout: STR,
            boxed_layout: BOXED,
            ..Program::default()
        }
    }

    /// What the verifier says about a run of bytes, in its own words.
    fn faults(program: &Program, code: &[EncodedInst]) -> Vec<String> {
        verify_function(program, FunctionId(0), code)
            .into_iter()
            .map(|fault| fault.what)
            .collect()
    }

    /// The bytes of one instruction, encoded at pc 0.
    fn at(inst: Inst) -> EncodedInst {
        encode(&inst, 0).expect("the instruction encodes")
    }

    /// Sets one byte of an instruction, which is how these tests write bytes
    /// no encoder produced.
    fn with(code: EncodedInst, offset: usize, byte: u8) -> EncodedInst {
        let mut bytes = *code.bytes();
        bytes[offset] = byte;
        EncodedInst::from_bytes(bytes)
    }

    /// The whole point of the boundary: bytes the encoder produced from a
    /// well formed lowering pass, and after that the dispatch loop may index
    /// without asking.
    #[test]
    fn a_well_formed_encoding_has_nothing_to_say_about_it() {
        let held = program(vec![
            Inst::Int { dst: 0, value: 7 },
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: 0,
                a: 0,
                b: 1,
            },
            Inst::Return { src: 0 },
        ]);
        let code = encode_function(&held.functions[0]).expect("it encodes");
        assert_eq!(faults(&held, &code), Vec::<String>::new());
        assert_eq!(
            verify(&held, &encode_program(&held).expect("encodes")),
            Ok(())
        );
    }

    /// A byte that names no operation stops the instruction being read at
    /// all, rather than reaching a table with an index nothing bounded.
    #[test]
    fn an_opcode_no_encoder_produced_is_refused() {
        let held = program(vec![Inst::Return { src: 0 }]);
        let code = [with(at(Inst::Return { src: 0 }), 0, 200)];
        assert_eq!(faults(&held, &code), ["opcode 200 names no operation"]);
    }

    /// `flags` is reserved and must be zero, which is ADR 0041's decision
    /// about a byte it deliberately found no use for.
    #[test]
    fn a_nonzero_flags_byte_is_refused() {
        let held = program(vec![Inst::Return { src: 0 }]);
        let code = [with(at(Inst::Return { src: 0 }), 1, 4)];
        assert_eq!(
            faults(&held, &code),
            ["flags is 4, and it is reserved and must be zero"]
        );
    }

    /// The check the whole format rests on. A slot operand is sixteen bits,
    /// so any of 65,536 values can appear in it, and only the ones inside
    /// this frame may be read as a frame offset.
    #[test]
    fn a_slot_past_the_frame_is_refused() {
        let held = program(vec![Inst::Return { src: 0 }]);
        let code = [at(Inst::Unit { dst: 9 })];
        assert_eq!(
            faults(&held, &code),
            ["a names slot 9, outside a frame of 4"]
        );
        // The same rule over `b` and `c`, because it is one rule over three
        // fields rather than one per instruction.
        let code = [at(Inst::Arith {
            num: Num::Int,
            op: ArithOp::Add,
            dst: 0,
            a: 4,
            b: 5,
        })];
        assert_eq!(
            faults(&held, &code),
            [
                "b names slot 4, outside a frame of 4",
                "c names slot 5, outside a frame of 4"
            ]
        );
    }

    /// A slot inside the frame is not enough when the operand heads a *run*
    /// of words: two words at the last slot reach the frame above.
    #[test]
    fn a_value_location_that_runs_off_the_top_of_the_frame_is_refused() {
        let held = program(vec![Inst::Return { src: 0 }]);
        let code = [at(Inst::Copy {
            dst: 3,
            src: 0,
            layout: POINT,
        })];
        assert_eq!(
            faults(&held, &code),
            ["the value at a is `Point`, 2 words at slot 3, and the frame has 4"]
        );
        // The words are checked as well as the extent, because a location
        // whose second word is a reference is what a collection would trace.
        let code = [at(Inst::Copy {
            dst: 1,
            src: 0,
            layout: POINT,
        })];
        assert_eq!(
            faults(&held, &code),
            ["the value at a is `Point`, whose word 1 is int, but slot 2 holds ref"]
        );
    }

    /// The opcode says which `Repr`s its operands may hold, so a `float`
    /// addition over `int` words is a refusal here and not a wrong answer
    /// later. This is `crate::verify`'s `expect` check, driven by an opcode
    /// instead of by a match on an enum.
    #[test]
    fn a_slot_whose_repr_the_opcode_does_not_admit_is_refused() {
        let held = program(vec![Inst::Return { src: 0 }]);
        let code = [at(Inst::Arith {
            num: Num::Float,
            op: ArithOp::Add,
            dst: 0,
            a: 1,
            b: 1,
        })];
        assert_eq!(
            faults(&held, &code),
            [
                "slot 0 holds int, and this opcode wants float",
                "slot 1 holds int, and this opcode wants float",
                "slot 1 holds int, and this opcode wants float"
            ]
        );
    }

    /// A branch is relative and its target has to land on an instruction of
    /// this function — which, under a 1:1 encoding, is any pc it has.
    #[test]
    fn a_branch_past_the_last_instruction_is_refused() {
        let held = program(vec![Inst::Return { src: 0 }]);
        let code = [
            encode(&Inst::Jump { to: 7 }, 0).expect("encodes"),
            at(Inst::Return { src: 0 }),
        ];
        assert_eq!(
            faults(&held, &code),
            ["jumps to 7, past the 2 instructions"]
        );
        // One past the last is off the end; the last itself is not.
        let code = [
            encode(&Inst::Jump { to: 2 }, 0).expect("encodes"),
            at(Inst::Return { src: 0 }),
        ];
        assert_eq!(
            faults(&held, &code),
            ["jumps to 2, past the 2 instructions"]
        );
        let code = [
            encode(&Inst::Jump { to: 1 }, 0).expect("encodes"),
            at(Inst::Return { src: 0 }),
        ];
        assert_eq!(faults(&held, &code), Vec::<String>::new());
    }

    /// A switch table stays immutable program metadata with absolute targets,
    /// and this is where absolute breaks loudly.
    #[test]
    fn a_switch_target_past_the_last_instruction_is_refused() {
        let mut held = program(vec![Inst::Return { src: 0 }]);
        held.tables.push(Table {
            targets: vec![0, 5],
            default: 9,
        });
        let code = [
            at(Inst::Switch {
                on: 0,
                table: TableId(0),
            }),
            at(Inst::Return { src: 0 }),
        ];
        assert_eq!(
            faults(&held, &code),
            [
                "jumps to 5, past the 2 instructions",
                "jumps to 9, past the 2 instructions"
            ]
        );
    }

    /// Every id in the payload indexes its own table, and a program that
    /// names one it does not have is refused before anything indexes with it.
    #[test]
    fn an_id_past_the_end_of_its_table_is_refused() {
        let held = program(vec![Inst::Return { src: 0 }]);
        let code = [at(Inst::Str {
            dst: 2,
            text: StrId(0),
        })];
        assert_eq!(faults(&held, &code), ["names string 0, and there are 0"]);

        let code = [at(Inst::Copy {
            dst: 0,
            src: 1,
            layout: LayoutId(40),
        })];
        assert_eq!(faults(&held, &code), ["names layout 40, and there are 4"]);
    }

    /// A call's arity is the callee's, not the call site's.
    #[test]
    fn a_call_that_passes_the_wrong_number_of_arguments_is_refused() {
        let mut held = program(vec![Inst::Return { src: 0 }]);
        let mut callee = function(vec![Inst::Return { src: 0 }]);
        callee.name = Arc::from("g");
        callee.params = vec![INT, INT];
        held.functions.push(callee);
        held.args.push(vec![Arg {
            slot: 0,
            layout: INT,
        }]);
        let code = [at(Inst::Call {
            dst: 0,
            callee: FunctionId(1),
            args: ArgsId(0),
        })];
        assert_eq!(
            faults(&held, &code),
            ["passes 1 arguments to `m.g`, which declares 2"]
        );
    }

    /// A function with no instructions has nowhere to begin, and a dispatch
    /// loop that trusted its bounds would read whatever followed it.
    #[test]
    fn a_run_with_no_instructions_is_refused() {
        let held = program(vec![Inst::Return { src: 0 }]);
        assert_eq!(
            faults(&held, &[]),
            ["has no instructions, so there is nowhere to begin"]
        );
    }

    /// A byte stream that stops mid-instruction never becomes instructions,
    /// so the verifier is never handed a partial one. Sixteen bytes is the
    /// only length an instruction has.
    #[test]
    fn a_truncated_stream_never_reaches_the_verifier() {
        let code = at(Inst::Return { src: 0 });
        let whole = code.bytes().to_vec();
        assert!(instructions(&whole).is_ok());
        assert!(instructions(&whole[..15]).is_err());
        assert!(instructions(&[whole.clone(), whole[..4].to_vec()].concat()).is_err());
    }

    /// An encoding of a different length is a fault about the whole function
    /// rather than about one instruction: a pc would mean two things, and
    /// spans, local ranges and switch targets are all indexed by one.
    #[test]
    fn an_encoding_of_a_different_length_than_the_function_is_refused() {
        let held = program(vec![Inst::Return { src: 0 }]);
        let encoded = Encoded {
            functions: vec![vec![
                at(Inst::Return { src: 0 }),
                at(Inst::Return { src: 0 }),
            ]],
        };
        let said: Vec<String> = verify(&held, &encoded)
            .expect_err("two encoded instructions against one")
            .into_iter()
            .map(|fault| fault.what)
            .collect();
        assert_eq!(
            said,
            ["is 2 encoded instructions and the function has 1, so a pc means two things"]
        );
    }

    /// The verifier is a reader of input, and the format being internal does
    /// not make its bytes trusted. Nothing here panics, indexes out of range,
    /// or loops, whatever the bytes are.
    #[test]
    fn arbitrary_bytes_answer_rather_than_panic() {
        let mut held = program(vec![Inst::Return { src: 0 }]);
        held.strings.push(Arc::from("one"));
        held.args.push(Vec::new());
        held.tables.push(Table {
            targets: vec![0],
            default: 0,
        });
        let mut bytes = [0u8; EncodedInst::BYTES];
        for seed in 0u32..20_000 {
            for (offset, byte) in bytes.iter_mut().enumerate() {
                *byte = (seed
                    .wrapping_mul(2_654_435_761)
                    .rotate_left(offset as u32 * 5)
                    ^ offset as u32) as u8;
            }
            let code = [EncodedInst::from_bytes(bytes)];
            let _ = verify_function(&held, FunctionId(0), &code);
        }
    }
}
