//! A disassembly, for reading a lowering and for a test to assert on.
//!
//! The format is one instruction per line, `pc  opcode operands`, with slot
//! numbers written `s0`, `s1` and annotated with what they hold. A test that
//! pins a lowering pins this text, so it is written to be diffed: one fact
//! per line, and no alignment that changes when an unrelated line grows.

use std::fmt::Write as _;

use crate::inst::{ArithOp, CmpOp, Compare, Convert, Inst, Len, Num, Slot};
use crate::layout::Shape;
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

/// Renders one function: its frame, then its code.
pub fn function(program: &Program, id: FunctionId) -> String {
    let f = program.function(id);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{id} {}({}) -> {}{}",
        f.qualified(),
        f.arity,
        f.returns,
        if f.is_async { " async" } else { "" }
    );
    let _ = write!(out, "  frame {}:", f.frame_size());
    for (slot, repr) in f.reprs.iter().enumerate() {
        let role = if (slot as u32) < f.arity { "!" } else { "" };
        let _ = write!(out, " s{slot}{role}:{repr}");
    }
    out.push('\n');
    for capture in &f.captures {
        let _ = writeln!(
            out,
            "  capture {} -> s{}:{}",
            capture.name, capture.slot, capture.repr
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
    match inst {
        Inst::Unit { dst } => format!("unit {}", s(*dst)),
        Inst::Bool { dst, value } => format!("bool {} {value}", s(*dst)),
        Inst::Int { dst, value } => format!("int {} {value}", s(*dst)),
        Inst::Float { dst, bits } => format!("float {} {}", s(*dst), f64::from_bits(*bits)),
        Inst::Str { dst, text } => format!("str {} {:?}", s(*dst), program.string(*text)),
        Inst::Move { dst, src } => format!("move {} {}", s(*dst), s(*src)),
        Inst::Clear { slot } => format!("clear {}", s(*slot)),
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
            args_of(program, f, *args)
        ),
        Inst::CallClosure { dst, closure, args } => format!(
            "call-closure {} {} ({})",
            s(*dst),
            s(*closure),
            args_of(program, f, *args)
        ),
        Inst::CallHost { dst, op, args } => {
            let op = program.host_op(*op);
            format!(
                "call-host {} {}.{} ({})",
                s(*dst),
                op.module,
                op.operation,
                args_of(program, f, *args)
            )
        }
        Inst::CallBuiltin { dst, builtin, args } => {
            let builtin = program.builtin(*builtin);
            format!(
                "call-builtin {} {}.{} ({})",
                s(*dst),
                builtin.receiver,
                builtin.operation,
                args_of(program, f, *args)
            )
        }
        Inst::Alloc { dst, layout, len } => {
            let shape = &program.layout(*layout).shape;
            let name = &program.layout(*layout).name;
            let len = match len {
                Len::Fixed => String::new(),
                Len::Count(n) => format!(" x{n}"),
                Len::Slot(slot) => format!(" x{}", s(*slot)),
            };
            format!("alloc {} {name}<{}>{len}", s(*dst), shape_name(shape))
        }
        Inst::GetWord { dst, obj, at } => format!("get-word {} {} +{at}", s(*dst), s(*obj)),
        Inst::SetWord { obj, at, src } => format!("set-word {} +{at} {}", s(*obj), s(*src)),
        Inst::GetElem { dst, obj, index } => {
            format!("get-elem {} {} {}", s(*dst), s(*obj), s(*index))
        }
        Inst::SetElem { obj, index, src } => {
            format!("set-elem {} {} {}", s(*obj), s(*index), s(*src))
        }
        Inst::Len { dst, obj } => format!("len {} {}", s(*dst), s(*obj)),
        Inst::AddrOfSlot { dst, slot } => format!("addr-of-slot {} {}", s(*dst), s(*slot)),
        Inst::AddrOfWord { dst, obj, at } => format!("addr-of-word {} {} +{at}", s(*dst), s(*obj)),
        Inst::AddrOfElem { dst, obj, index } => {
            format!("addr-of-elem {} {} {}", s(*dst), s(*obj), s(*index))
        }
        Inst::Load { dst, addr } => format!("load {} {}", s(*dst), s(*addr)),
        Inst::Store { addr, src } => format!("store {} {}", s(*addr), s(*src)),
        Inst::Box { dst, src, repr } => format!("box {} {} {repr}", s(*dst), s(*src)),
        Inst::Unbox { dst, src, repr } => format!("unbox {} {} {repr}", s(*dst), s(*src)),
        Inst::Trap { message } => format!("trap {:?}", program.string(*message)),
    }
}

fn args_of(program: &Program, f: &Function, args: crate::ArgsId) -> String {
    program
        .arg_list(args)
        .iter()
        .map(|slot| match f.repr(*slot) {
            Some(repr) => format!("s{slot}:{repr}"),
            None => format!("s{slot}:?"),
        })
        .collect::<Vec<_>>()
        .join(" ")
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
