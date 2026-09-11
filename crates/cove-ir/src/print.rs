//! A disassembly, for reading a lowering and for a test to assert on.
//!
//! The format is one instruction per line, `pc  opcode operands`, with slot
//! numbers written `s0`, `s1`.
//!
//! An operand is one of two things and the line says which. A **word** is
//! annotated with what that one word holds — `s3:int`, `s5:tag` — and a word
//! is what arithmetic, a branch's condition and a field offset take. A
//! **value location** is annotated with its *layout* — `s3:Point` — because a
//! value is a run of words and the layout is what says how many. Where the
//! run is wider than one word the whole of it is named: `s5..s7:Result` is
//! the three words `s5`, `s6` and `s7`.
//!
//! Naming the run is what makes the calling convention visible. A
//! `call-host s5..s7:Result console.println (s4:String)` writes three words,
//! and until issue #299 the same line read
//! `call-host s5:tag console.println (s4:String) Result`: a destination that
//! looked like one word, the width parked at the end of the line, and `s6`
//! and `s7` reading as registers with nothing to do with it. The width was
//! there; which slots it covered was not.
//!
//! So the layout is written once, on the location it describes, and a `copy`,
//! a `clear`, a `return` and a call no longer repeat it after their operands.
//! That includes [`Inst::CallClosure`], whose *callee* is a function id read
//! out of an object at run time but whose *answer* is not: the checker
//! settles a call through a value against the callee's function type, so the
//! instruction carries the layout the destination has to be, and the line
//! reads like every other call's.
//!
//! Which operands are locations is not a second opinion. It is where
//! [`mod@crate::verify`] asks whether a run of words *fits* — the same layout, on
//! the same slot — so a listing and the check disagreeing is a bug in one of
//! them rather than a matter of taste. A layout the table does not hold
//! prints as its id and no range, `s5:layout7`, because nothing then says how
//! wide the location is.
//!
//! Before the code come the frame's names, one to a line —
//! `local count -> s3:Int [4, 11)` — because a slot number is not an answer
//! to what the source called something and a slot is reused by several
//! variables in turn. The pair is the half-open range of program counters the
//! name denotes that slot over; see [`crate::Local`]. The `frame` line above
//! them is the one place that is per *word* throughout: it is the ground
//! truth every range indexes into.
//!
//! A test that pins a lowering pins this text, so it is written to be
//! diffed: one fact per line, and no alignment that changes when an
//! unrelated line grows.

use std::fmt::Write as _;

use crate::inst::{ArithOp, CmpOp, Compare, Convert, Inst, Len, Num, Slot};
use crate::layout::{LayoutId, Shape};
use crate::program::{Function, FunctionId, Program};

/// Renders every function of `program`.
pub fn program(program: &Program) -> String {
    let mut out = String::new();
    for index in 0..program.functions.len() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&function(program, FunctionId(index as u32)));
    }
    out
}

/// Renders one function: its boundary, its frame, the names bound in it,
/// then its code.
pub fn function(program: &Program, id: FunctionId) -> String {
    let f = program.function(id);
    let mut out = String::new();
    let params: Vec<String> = f
        .params
        .iter()
        .map(|layout| name_of(program, *layout))
        .collect();
    let _ = writeln!(
        out,
        "fn @{}({}) -> {}{}",
        f.qualified(),
        params.join(" "),
        name_of(program, f.returns),
        if f.is_async { " async" } else { "" }
    );
    let taken = f.param_words(&program.layouts);
    let _ = write!(out, "  frame {}:", f.frame_size());
    for (slot, repr) in f.reprs.iter().enumerate() {
        let role = if (slot as u32) < taken { "!" } else { "" };
        let _ = write!(out, " s{slot}{role}:{repr}");
    }
    out.push('\n');
    for capture in &f.captures {
        let _ = writeln!(
            out,
            "  capture {} -> {}",
            capture.name,
            location(program, capture.slot, capture.layout)
        );
    }
    for local in &f.locals {
        let _ = writeln!(
            out,
            "  local {} -> {} [{}, {})",
            local.name,
            location(program, local.slot, local.layout),
            local.from,
            local.to
        );
    }
    for (pc, inst) in f.code.iter().enumerate() {
        let _ = writeln!(out, "  {pc:>4}  {}", one(program, f, inst));
    }
    out
}

/// Renders one instruction.
pub fn one(program: &Program, f: &Function, inst: &Inst) -> String {
    let s = |slot: Slot| match f.repr(slot) {
        Some(repr) => format!("s{slot}:{repr}"),
        None => format!("s{slot}:?"),
    };
    let l = |layout: LayoutId| name_of(program, layout);
    // A whole value location, where `s` above renders one word. Which of the
    // two an operand is, is what this module exists to say on a line.
    let v = |slot: Slot, layout: LayoutId| location(program, slot, layout);
    match inst {
        Inst::Unit { dst } => format!("unit {}", s(*dst)),
        Inst::Bool { dst, value } => format!("bool {} {value}", s(*dst)),
        Inst::Int { dst, value } => format!("int {} {value}", s(*dst)),
        // Named, not numbered, for `Inst::FuncRef`'s reason below: a case
        // added before this one changes its index and would otherwise
        // change every listing that never mentions it.
        Inst::Tag { dst, layout, case } => {
            format!("tag {} {}", s(*dst), case_name(program, *layout, *case))
        }
        // Named, not numbered — the whole point of this instruction over an
        // `Inst::Int` carrying the same word. `FunctionId` is dense and
        // renumbers whenever an unrelated declaration is added or moved, so
        // printing it would make this line, and the golden test that pins
        // it, churn on changes that have nothing to do with this closure.
        Inst::FuncRef { dst, callee } => {
            format!(
                "func-ref {} @{}",
                s(*dst),
                program.function(*callee).qualified()
            )
        }
        Inst::Float { dst, bits } => format!("float {} {}", s(*dst), f64::from_bits(*bits)),
        Inst::Str { dst, text } => format!("str {} {:?}", s(*dst), program.string(*text)),
        Inst::Copy { dst, src, layout } => {
            format!("copy {} {}", v(*dst, *layout), v(*src, *layout))
        }
        Inst::Clear { slot, layout } => format!("clear {}", v(*slot, *layout)),
        Inst::Neg { num, dst, a } => format!("neg.{} {} {}", num_name(*num), s(*dst), s(*a)),
        Inst::Arith { num, op, dst, a, b } => format!(
            "{}.{} {} {} {}",
            arith_name(*op),
            num_name(*num),
            s(*dst),
            s(*a),
            s(*b)
        ),
        Inst::Cmp { on, op, dst, a, b } => format!(
            "{}.{} {} {} {}",
            cmp_name(*op),
            compare_name(*on),
            s(*dst),
            s(*a),
            s(*b)
        ),
        // An immediate is written bare, and the `.imm` on the opcode is what
        // says the last operand is one. Nothing else is needed: a slot in
        // this format is always `sN:repr`, so a number standing where an
        // operand goes is already not a slot — it is how `jump 2` writes a
        // program counter, how `int s5:int 7` writes a value, and how
        // `load-field s2:Int s1:ref +0` writes an offset. Marking it
        // `#7` would spell a distinction the format already draws.
        Inst::ArithImm { op, dst, a, value } => {
            format!("{}.int.imm {} {} {value}", arith_name(*op), s(*dst), s(*a))
        }
        Inst::CmpImm { op, dst, a, value } => {
            format!("{}.int.imm {} {} {value}", cmp_name(*op), s(*dst), s(*a))
        }
        Inst::Not { dst, a } => format!("not {} {}", s(*dst), s(*a)),
        Inst::Convert { to, dst, a } => format!(
            "{} {} {}",
            match to {
                Convert::IntToFloat => "int-to-float",
                Convert::FloatToInt => "float-to-int",
            },
            s(*dst),
            s(*a)
        ),
        Inst::Jump { to } => format!("jump {to}"),
        Inst::BranchFalse { cond, to } => format!("branch-false {} {to}", s(*cond)),
        Inst::Switch { on, table } => {
            let table = program.table(*table);
            let targets: Vec<String> = table.targets.iter().map(|to| to.to_string()).collect();
            format!(
                "switch {} [{}] else {}",
                s(*on),
                targets.join(" "),
                table.default
            )
        }
        Inst::Return { src } => format!("return {}", v(*src, f.returns)),
        Inst::Call { dst, callee, args } => {
            let target = program.function(*callee);
            format!(
                "call {} {} ({})",
                v(*dst, target.returns),
                target.qualified(),
                args_of(program, *args)
            )
        }
        // The callee is a word in a slot, read out of the closure object
        // when the instruction runs, and it is written as one. The answer is
        // a location like every other call's: `Inst::CallClosure` carries
        // that layout itself, because there is no declared callee to read it
        // from. See the module docs.
        Inst::CallClosure {
            dst,
            closure,
            args,
            result,
        } => format!(
            "call-closure {} {} ({})",
            v(*dst, *result),
            s(*closure),
            args_of(program, *args)
        ),
        Inst::CallHost { dst, op, args } => {
            let op = program.host_op(*op);
            format!(
                "call-host {} {} ({})",
                v(*dst, op.result),
                op.qualified(),
                args_of(program, *args)
            )
        }
        Inst::CallResource {
            dst,
            receiver,
            op,
            args,
        } => {
            let op = program.host_op(*op);
            format!(
                "call-resource {} {} {} ({})",
                v(*dst, op.result),
                s(*receiver),
                op.qualified(),
                args_of(program, *args)
            )
        }
        Inst::CallBuiltin { dst, builtin, args } => {
            let builtin = program.builtin(*builtin);
            format!(
                "call-builtin {} {}.{} ({})",
                v(*dst, builtin.result),
                builtin.receiver,
                builtin.operation,
                args_of(program, *args)
            )
        }
        Inst::Alloc { dst, layout, len } => {
            let shape = &program.layout(*layout).shape;
            let len = match len {
                Len::Fixed => String::new(),
                Len::Count(n) => format!(" x{n}"),
                Len::Slot(slot) => format!(" x{}", s(*slot)),
            };
            format!(
                "alloc {} {}<{}>{len}",
                s(*dst),
                l(*layout),
                shape_name(shape)
            )
        }
        Inst::LoadField {
            dst,
            obj,
            at,
            layout,
        } => format!("load-field {} {} +{at}", v(*dst, *layout), s(*obj)),
        Inst::StoreField {
            obj,
            at,
            src,
            layout,
        } => format!("store-field {} +{at} {}", s(*obj), v(*src, *layout)),
        Inst::LoadElem {
            dst,
            obj,
            index,
            layout,
        } => format!("load-elem {} {} {}", v(*dst, *layout), s(*obj), s(*index)),
        Inst::StoreElem {
            obj,
            index,
            src,
            layout,
        } => format!("store-elem {} {} {}", s(*obj), s(*index), v(*src, *layout)),
        Inst::ByteAt { dst, obj, at } => {
            format!("byte-at {} {} {}", s(*dst), s(*obj), s(*at))
        }
        Inst::Len { dst, obj } => format!("len {} {}", s(*dst), s(*obj)),
        Inst::LayoutOf { dst, obj } => format!("layout-of {} {}", s(*dst), s(*obj)),
        Inst::AddrOfSlot { dst, slot } => format!("addr-of-slot {} {}", s(*dst), s(*slot)),
        Inst::AddrOfField { dst, obj, at } => {
            format!("addr-of-field {} {} +{at}", s(*dst), s(*obj))
        }
        Inst::AddrOfElem {
            dst,
            obj,
            index,
            layout,
        } => format!(
            "addr-of-elem {} {} {} {}",
            s(*dst),
            s(*obj),
            s(*index),
            l(*layout)
        ),
        Inst::AddrOfPart { dst, addr, at } => {
            format!("addr-of-part {} {} +{at}", s(*dst), s(*addr))
        }
        Inst::Load { dst, addr, layout } => {
            format!("load {} {}", v(*dst, *layout), s(*addr))
        }
        Inst::Store { addr, src, layout } => {
            format!("store {} {}", s(*addr), v(*src, *layout))
        }
        Inst::Box { dst, src, layout } => format!("box {} {}", s(*dst), v(*src, *layout)),
        Inst::Unbox { dst, src, layout } => {
            format!("unbox {} {}", v(*dst, *layout), s(*src))
        }
        Inst::ScopeEnter { dst, name } => {
            format!("scope.enter {} {:?}", s(*dst), program.string(*name))
        }
        Inst::ScopeLeave {
            scope,
            failed,
            error,
            layout,
        } => format!(
            "scope.leave {} {} {}",
            s(*scope),
            s(*failed),
            v(*error, *layout)
        ),
        Inst::ScopeCancel { scope } => format!("scope.cancel {}", s(*scope)),
        Inst::Spawn {
            dst,
            scope,
            closure,
            answer,
        } => format!(
            "spawn {} {} {} {}",
            s(*dst),
            s(*scope),
            s(*closure),
            l(*answer)
        ),
        Inst::Await { dst, task, answer } => {
            format!("await {} {}", v(*dst, *answer), s(*task))
        }
        Inst::Cancel { task } => format!("cancel {}", s(*task)),
        Inst::Settled { dst, src, answer } => {
            format!("settled {} {}", s(*dst), v(*src, *answer))
        }
        Inst::SharedLock { cell } => format!("shared.lock {}", s(*cell)),
        Inst::SharedUnlock { cell } => format!("shared.unlock {}", s(*cell)),
        Inst::Trap { message } => format!("trap {:?}", program.string(*message)),
        Inst::AssertFailed { message } => format!("assert.failed {}", s(*message)),
    }
}

/// What a case is called in a listing: `Shape.Circle`, not `1`.
///
/// The number is the fact the instruction carries and the name is what a
/// reader wants, and printing the number is what made a listing change when
/// an unrelated case was declared before this one. Where the layout is not an
/// enum, or the index is past its cases, the id is printed instead — a
/// listing is read while a lowering is being debugged, and a lowering that
/// produced either of those is the thing being debugged.
fn case_name(program: &Program, layout: LayoutId, case: crate::CaseId) -> String {
    match program.layouts.get(layout.index()) {
        Some(held) => match &held.shape {
            crate::layout::Shape::Enum { cases, .. } => match cases.get(case.index()) {
                Some(found) => format!("{}.{}", held.name, found.name),
                None => format!("{}.{case}", held.name),
            },
            _ => format!("{}.{case}", held.name),
        },
        None => format!("{layout}.{case}"),
    }
}

/// What a layout is called in a listing, or its id where the table is too
/// short to say — a listing is also read while a lowering is being debugged.
fn name_of(program: &Program, layout: LayoutId) -> String {
    match program.layouts.get(layout.index()) {
        Some(held) => held.name.to_string(),
        None => layout.to_string(),
    }
}

/// An argument is a value location, so it prints as one: the *layout* it
/// names rather than the `Repr` of its first word, and the whole run of slots
/// the callee will read. A listing that showed `s3:int` for a `Point` would
/// show the same thing for its `x`, and which of the two a call passes is the
/// question the argument list exists to answer.
fn args_of(program: &Program, args: crate::ArgsId) -> String {
    match program.args.get(args.index()) {
        Some(list) => list
            .iter()
            .map(|arg| location(program, arg.slot, arg.layout))
            .collect::<Vec<_>>()
            .join(" "),
        None => args.to_string(),
    }
}

/// A whole value location: `s3:Int` for a one-word value, `s5..s7:Result` for
/// one that runs over three.
///
/// The base slot, the last slot of the run, and the layout that decides
/// which. The run is `layout.words`, which is the same thing
/// [`mod@crate::verify`]'s `fits` walks — the printer and the check read one
/// fact rather than two that can drift.
///
/// A one-word value stays compact. `s3..s3:Int` would be noise on the great
/// majority of the lines in a listing, and a reader who wants the width of a
/// scalar has the `frame` line above.
///
/// Two cases are rendered rather than indexed. A layout the table does not
/// hold prints as its id and no range, because nothing says how wide it is;
/// and a zero-word layout — [`crate::Layout::free`], which is not a value —
/// prints as its base alone rather than as a range that runs backwards. A
/// listing is read while a lowering is being debugged, and a lowering that
/// produced either is the thing being debugged.
fn location(program: &Program, slot: Slot, layout: LayoutId) -> String {
    let Some(held) = program.layouts.get(layout.index()) else {
        return format!("s{slot}:{layout}");
    };
    match held.width() {
        0 | 1 => format!("s{slot}:{}", held.name),
        // In `u64`, because an ill-formed program may name a slot near the
        // top of the range and this renders it rather than panicking.
        width => format!("s{slot}..s{}:{}", slot as u64 + width as u64 - 1, held.name),
    }
}

fn num_name(num: Num) -> &'static str {
    match num {
        Num::Int => "int",
        Num::Float => "float",
    }
}

fn compare_name(on: Compare) -> &'static str {
    match on {
        Compare::Int => "int",
        Compare::Float => "float",
        Compare::Bool => "bool",
        Compare::Str => "str",
        Compare::Identity => "identity",
    }
}

fn arith_name(op: ArithOp) -> &'static str {
    match op {
        ArithOp::Add => "add",
        ArithOp::Sub => "sub",
        ArithOp::Mul => "mul",
        ArithOp::Div => "div",
        ArithOp::Rem => "rem",
    }
}

fn cmp_name(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "eq",
        CmpOp::Ne => "ne",
        CmpOp::Lt => "lt",
        CmpOp::Le => "le",
        CmpOp::Gt => "gt",
        CmpOp::Ge => "ge",
    }
}

fn shape_name(shape: &Shape) -> &'static str {
    match shape {
        Shape::Free => "free",
        Shape::Word(_) => "word",
        Shape::Str => "str",
        Shape::Struct { .. } => "struct",
        Shape::Enum { .. } => "enum",
        Shape::Elements {
            growable: false, ..
        } => "array",
        Shape::Elements { growable: true, .. } => "store",
        Shape::Vector { .. } => "vector",
        Shape::Members { .. } => "set",
        Shape::Entries { .. } => "map",
        Shape::Closure { .. } => "closure",
        Shape::Shared { .. } => "shared",
        Shape::Boxed => "boxed",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cove_diag::{FileId, Span};

    use super::*;
    use crate::layout::{Case, Layout};
    use crate::program::{Arg, HostOp, Local};
    use crate::repr::{RefMap, Repr};
    use crate::{ArgsId, HostOpId};

    /// One word.
    const INT: LayoutId = LayoutId(0);
    /// One word, and an address rather than a scalar — the case that is one
    /// word *and* a family of its own, so a listing must not read it as the
    /// `ref` its frame word says.
    const STR: LayoutId = LayoutId(1);
    /// Two words, inline: `struct Point { x: Int, y: Int }`.
    const POINT: LayoutId = LayoutId(2);
    /// Three words, inline: `Result<Unit, Error>` is a tag, a `Unit` and the
    /// error's one reference. This is the layout issue #299 is written about.
    const RESULT: LayoutId = LayoutId(3);
    /// Nothing holds this: the table stops before it.
    const MISSING: LayoutId = LayoutId(9);

    fn layouts() -> Vec<Layout> {
        vec![
            Layout::word("Int", Repr::Int),
            Layout::object("String", Shape::Str),
            Layout::inline(
                "m.Point",
                Shape::Struct {
                    fields: Vec::new(),
                    opaque: false,
                },
                vec![Repr::Int, Repr::Int],
            ),
            Layout::inline(
                "Result",
                Shape::Enum {
                    cases: vec![
                        Case {
                            name: Arc::from("Ok"),
                            parts: Vec::new(),
                        },
                        Case {
                            name: Arc::from("Err"),
                            parts: Vec::new(),
                        },
                    ],
                    payload: vec![Repr::Unit, Repr::Ref],
                },
                vec![Repr::Tag, Repr::Unit, Repr::Ref],
            ),
        ]
    }

    fn span() -> Span {
        Span::new(FileId(0), 0, 0)
    }

    /// A frame wide enough for every case here: `s0..s2` is a `Result`,
    /// `s3..s4` a `Point`, `s5` an `Int` and `s6` a `String`.
    fn function(code: Vec<Inst>) -> Function {
        let reprs = vec![
            Repr::Tag,
            Repr::Unit,
            Repr::Ref,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Ref,
        ];
        Function {
            module: Arc::from("m"),
            name: Arc::from("f"),
            params: Vec::new(),
            spans: vec![span(); code.len()],
            refs: RefMap::of(&reprs),
            reprs,
            returns: RESULT,
            captures: Vec::new(),
            code,
            locals: Vec::new(),
            inlined: Vec::new(),
            span: span(),
            is_async: false,
            stub: false,
        }
    }

    fn program() -> Program {
        Program {
            layouts: layouts(),
            str_layout: STR,
            ..Program::default()
        }
    }

    /// What one instruction reads as, in a program with the layouts above.
    fn line(inst: Inst) -> String {
        let held = program();
        one(&held, &function(vec![inst.clone()]), &inst)
    }

    /// A one-word value is its base slot and its layout, with no range: a
    /// `s5..s5:Int` on nearly every line of a listing would be noise, and the
    /// layout is still what says which family the word is of.
    #[test]
    fn a_one_word_value_is_its_base_slot_and_its_layout() {
        assert_eq!(
            line(Inst::Copy {
                dst: 5,
                src: 3,
                layout: INT,
            }),
            "copy s5:Int s3:Int"
        );
        // One word and an address: the frame says `ref`, and which family of
        // reference it is is exactly what the location's layout adds.
        assert_eq!(
            line(Inst::Clear {
                slot: 6,
                layout: STR
            }),
            "clear s6:String"
        );
    }

    /// An inline struct names both of the slots it covers. Its frame words
    /// are two `Repr::Int`s and say nothing about where the value ends.
    #[test]
    fn an_inline_struct_names_every_slot_it_covers() {
        assert_eq!(
            line(Inst::Copy {
                dst: 3,
                src: 3,
                layout: POINT,
            }),
            "copy s3..s4:m.Point s3..s4:m.Point"
        );
    }

    /// Issue #299's own example. The call writes `s0`, `s1` and `s2`, and the
    /// line says so; it used to read `call-host s0:tag console.println (…)
    /// Result`, which named the discriminant and left the other two words
    /// looking like unrelated registers.
    #[test]
    fn a_call_s_answer_names_the_whole_run_it_writes() {
        let mut held = program();
        held.args.push(vec![Arg {
            slot: 6,
            layout: STR,
        }]);
        held.host_ops.push(HostOp {
            module: Arc::from("console"),
            operation: Arc::from("println"),
            resource: None,
            result: RESULT,
        });
        let inst = Inst::CallHost {
            dst: 0,
            op: HostOpId(0),
            args: ArgsId(0),
        };
        assert_eq!(
            one(&held, &function(vec![inst.clone()]), &inst),
            "call-host s0..s2:Result console.println (s6:String)"
        );
        assert_eq!(
            one(
                &held,
                &function(vec![Inst::Return { src: 0 }]),
                &Inst::Return { src: 0 }
            ),
            "return s0..s2:Result"
        );
    }

    /// A closure call reads like every other call. Its *callee* is a word in
    /// a slot — that is the run-time fact — and its *answer* is a location,
    /// because the instruction carries the layout the checker settled.
    #[test]
    fn a_closure_call_names_its_answer_and_leaves_its_callee_a_word() {
        let mut held = program();
        held.args.push(vec![Arg {
            slot: 5,
            layout: INT,
        }]);
        let inst = Inst::CallClosure {
            dst: 0,
            closure: 6,
            args: ArgsId(0),
            result: RESULT,
        };
        assert_eq!(
            one(&held, &function(vec![inst.clone()]), &inst),
            "call-closure s0..s2:Result s6:ref (s5:Int)"
        );
    }

    /// A word operation still prints one word and its `Repr`. Arithmetic, a
    /// discriminant and a field offset are about the word in front of them,
    /// and a range on them would claim an extent nothing has.
    #[test]
    fn a_word_operation_still_prints_one_word_and_its_repr() {
        assert_eq!(
            line(Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: 5,
                a: 3,
                b: 4,
            }),
            "add.int s5:int s3:int s4:int"
        );
        assert_eq!(
            line(Inst::Tag {
                dst: 0,
                layout: RESULT,
                case: crate::CaseId(1),
            }),
            "tag s0:tag Result.Err"
        );
        // A field is read *into* a location and *out of* one word's offset,
        // so this one line has both spellings on it.
        assert_eq!(
            line(Inst::LoadField {
                dst: 3,
                obj: 6,
                at: 2,
                layout: POINT,
            }),
            "load-field s3..s4:m.Point s6:ref +2"
        );
    }

    /// A layout the table does not hold renders as its id and no range,
    /// because nothing then says how wide the location is. A listing is read
    /// while a lowering is being debugged, and this is the shape of a
    /// lowering that is being debugged.
    #[test]
    fn a_layout_the_table_does_not_hold_renders_rather_than_panicking() {
        assert_eq!(
            line(Inst::Clear {
                slot: 5,
                layout: MISSING,
            }),
            "clear s5:layout9"
        );
    }

    /// The frame line stays per *word*, and the names above the code are
    /// locations like any other operand: `wide` is three slots and says so.
    #[test]
    fn the_frame_is_words_and_the_names_over_it_are_locations() {
        let mut held = program();
        let mut f = function(vec![Inst::Return { src: 0 }]);
        f.locals = vec![
            Local {
                name: Arc::from("wide"),
                slot: 0,
                layout: RESULT,
                from: 0,
                to: 1,
            },
            Local {
                name: Arc::from("n"),
                slot: 5,
                layout: INT,
                from: 0,
                to: 1,
            },
        ];
        held.functions.push(f);
        // `super::function`, because this module has a `function` of its own
        // that builds the one being rendered.
        assert_eq!(
            super::function(&held, FunctionId(0)),
            "\
fn @m.f() -> Result
  frame 7: s0:tag s1:unit s2:ref s3:int s4:int s5:int s6:ref
  local wide -> s0..s2:Result [0, 1)
  local n -> s5:Int [0, 1)
     0  return s0..s2:Result
"
        );
    }
}
