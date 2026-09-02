//! A disassembly, for reading a lowering and for a test to assert on.
//!
//! The format is one instruction per line, `pc  opcode operands`, with slot
//! numbers written `s0`, `s1` and annotated with what that one word holds.
//! An instruction that moves a *value* names the layout after its operands,
//! because a value is a run of words and the layout is what says how many —
//! `copy s2:int s0:int Point` moves two.
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

/// Renders one function: its boundary, its frame, then its code.
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
        "{id} {}({}) -> {}{}",
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
            "  capture {} -> s{}:{}",
            capture.name,
            capture.slot,
            name_of(program, capture.layout)
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
    match inst {
        Inst::Unit { dst } => format!("unit {}", s(*dst)),
        Inst::Bool { dst, value } => format!("bool {} {value}", s(*dst)),
        Inst::Int { dst, value } => format!("int {} {value}", s(*dst)),
        Inst::Float { dst, bits } => format!("float {} {}", s(*dst), f64::from_bits(*bits)),
        Inst::Str { dst, text } => format!("str {} {:?}", s(*dst), program.string(*text)),
        Inst::Copy { dst, src, layout } => {
            format!("copy {} {} {}", s(*dst), s(*src), l(*layout))
        }
        Inst::Clear { slot, layout } => format!("clear {} {}", s(*slot), l(*layout)),
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
        Inst::Return { src } => format!("return {}", s(*src)),
        Inst::Call { dst, callee, args } => format!(
            "call {} {} ({})",
            s(*dst),
            program.function(*callee).qualified(),
            args_of(program, *args)
        ),
        Inst::CallClosure { dst, closure, args } => format!(
            "call-closure {} {} ({})",
            s(*dst),
            s(*closure),
            args_of(program, *args)
        ),
        Inst::CallHost { dst, op, args } => {
            let op = program.host_op(*op);
            format!(
                "call-host {} {} ({})",
                s(*dst),
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
                s(*dst),
                s(*receiver),
                op.qualified(),
                args_of(program, *args)
            )
        }
        Inst::CallBuiltin { dst, builtin, args } => {
            let builtin = program.builtin(*builtin);
            format!(
                "call-builtin {} {}.{} ({})",
                s(*dst),
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
        } => format!("load-field {} {} +{at} {}", s(*dst), s(*obj), l(*layout)),
        Inst::StoreField {
            obj,
            at,
            src,
            layout,
        } => format!("store-field {} +{at} {} {}", s(*obj), s(*src), l(*layout)),
        Inst::LoadElem {
            dst,
            obj,
            index,
            layout,
        } => format!(
            "load-elem {} {} {} {}",
            s(*dst),
            s(*obj),
            s(*index),
            l(*layout)
        ),
        Inst::StoreElem {
            obj,
            index,
            src,
            layout,
        } => format!(
            "store-elem {} {} {} {}",
            s(*obj),
            s(*index),
            s(*src),
            l(*layout)
        ),
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
            format!("load {} {} {}", s(*dst), s(*addr), l(*layout))
        }
        Inst::Store { addr, src, layout } => {
            format!("store {} {} {}", s(*addr), s(*src), l(*layout))
        }
        Inst::Box { dst, src, layout } => format!("box {} {} {}", s(*dst), s(*src), l(*layout)),
        Inst::Unbox { dst, src, layout } => {
            format!("unbox {} {} {}", s(*dst), s(*src), l(*layout))
        }
        Inst::Trap { message } => format!("trap {:?}", program.string(*message)),
        Inst::AssertFailed { message } => format!("assert.failed {}", s(*message)),
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

/// An argument prints as its slot and the *layout* it names, not the `Repr` of
/// its first word: a listing that showed `s3:int` for a `Point` would show the
/// same thing for its `x`, and which of the two a call passes is the question
/// the argument list exists to answer.
fn args_of(program: &Program, args: crate::ArgsId) -> String {
    match program.args.get(args.index()) {
        Some(list) => list
            .iter()
            .map(|arg| format!("s{}:{}", arg.slot, name_of(program, arg.layout)))
            .collect::<Vec<_>>()
            .join(" "),
        None => args.to_string(),
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
        Shape::Boxed => "boxed",
    }
}
