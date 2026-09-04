//! Reading encoded instructions back as text.
//!
//! # There is one printer, and it is [`crate::print`]
//!
//! ADR 0041 decides this rather than leaving it open, and the reason is the
//! 1:1 encoding. `crate::print` is already a disassembler for
//! [`Inst`](crate::Inst) — one instruction per line, one fact per line, no
//! alignment that shifts when an unrelated line grows, written so that a test
//! which pins a lowering can diff it — and [`decode`] is lossless. So a
//! disassembly is `decode` and then that printer, and it **cannot drift from
//! the IR's own rendering**, because it is the IR's own rendering.
//!
//! A second renderer would be a second thing to keep in step, for no reader's
//! benefit: the two would print the same instruction, and the day they
//! disagreed the disagreement would be the bug rather than the report of one.
//! It would also cost the property that makes this worth having — that
//! *lowered IR* and *executable bytecode* are two views of one index, so a
//! difference between the two panes of a debugger is a difference in the
//! encoding and never in the prose.
//!
//! # What this module is, then
//!
//! The part `print` cannot do: the bytes. [`listing`] adds the program
//! counter, the byte offset `pc << 4`, and the raw sixteen bytes in front of
//! the text — issue #245's debugger row — and [`one`] is the text alone.
//!
//! Neither can panic on any sixteen bytes at all. A run that does not decode
//! prints as its bytes and says so, because a disassembler is what somebody
//! reaches for when the bytes are *wrong*.

use std::fmt::Write as _;

use crate::inst::Pc;
use crate::program::{FunctionId, Program};

use super::decode::decode;
use super::EncodedInst;

/// One encoded instruction as [`crate::print::one`] renders it.
///
/// Bytes that do not decode print as `<the reason>`, with the reason
/// [`Malformed`](super::Malformed) gives — which is the one thing the readable
/// printer has no way to say, because no `Inst` is malformed.
pub fn one(program: &Program, id: FunctionId, code: EncodedInst, pc: Pc) -> String {
    let function = program.function(id);
    match decode(code, pc) {
        Ok(inst) => crate::print::one(program, function, &inst),
        Err(why) => format!("<{why}>"),
    }
}

/// One encoded instruction's sixteen bytes, in hex, low byte first.
///
/// The order the bytes are stored in rather than the order a number reads in,
/// because what a reader of this is checking is a byte offset.
pub fn bytes(code: EncodedInst) -> String {
    code.bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// A whole run of encoded instructions, one to a line.
///
/// `pc`, the byte offset, the raw sixteen bytes, then the text. The first
/// three are what a readable listing has no reason to carry and a bytecode
/// view exists for; the fourth is [`one`], so the right-hand column of this
/// and the body of [`crate::print::function`] are the same characters.
pub fn listing(program: &Program, id: FunctionId, code: &[EncodedInst]) -> String {
    let mut out = String::new();
    for (pc, held) in code.iter().enumerate() {
        let _ = writeln!(
            out,
            "{pc:>4}  +{:<6} {}  {}",
            EncodedInst::offset_of(pc as Pc),
            bytes(*held),
            one(program, id, *held, pc as Pc)
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cove_diag::{FileId, Span};

    use super::*;
    use crate::bytecode::encode::encode_function;
    use crate::inst::{ArithOp, Inst, Num};
    use crate::layout::{Layout, LayoutId};
    use crate::program::Function;
    use crate::repr::{RefMap, Repr};

    const INT: LayoutId = LayoutId(0);

    fn held() -> Program {
        let reprs = vec![Repr::Int, Repr::Int];
        let code = vec![
            Inst::Int { dst: 0, value: 7 },
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: 0,
                a: 0,
                b: 1,
            },
            Inst::Jump { to: 3 },
            Inst::Return { src: 0 },
        ];
        let span = Span::new(FileId(0), 0, 0);
        Program {
            functions: vec![Function {
                module: Arc::from("m"),
                name: Arc::from("f"),
                params: Vec::new(),
                spans: vec![span; code.len()],
                refs: RefMap::of(&reprs),
                reprs,
                returns: INT,
                captures: Vec::new(),
                code,
                locals: Vec::new(),
                span,
                is_async: false,
                stub: false,
            }],
            layouts: vec![Layout::word("Int", Repr::Int)],
            str_layout: INT,
            boxed_layout: INT,
            ..Program::default()
        }
    }

    /// The disassembly of an encoding *is* the readable listing of the
    /// instruction it encodes, character for character. That is what one
    /// printer buys, and it is the property a second renderer would cost.
    #[test]
    fn the_disassembly_of_an_encoding_is_the_readable_listing_of_it() {
        let program = held();
        let id = FunctionId(0);
        let code = encode_function(program.function(id)).expect("it encodes");
        let read: Vec<String> = code
            .iter()
            .enumerate()
            .map(|(pc, held)| one(&program, id, *held, pc as Pc))
            .collect();
        let written: Vec<String> = program
            .function(id)
            .code
            .iter()
            .map(|inst| crate::print::one(&program, program.function(id), inst))
            .collect();
        assert_eq!(read, written);
        assert_eq!(read[0], "int s0:int 7");
        // A relative displacement is decoded back to the absolute target the
        // readable IR names, so the two panes agree about where a jump goes.
        assert_eq!(read[2], "jump 3");
    }

    /// The bytecode row: the pc, the byte offset `pc << 4`, the raw sixteen
    /// bytes, and the text. Issue #245's debugger view, and the only thing
    /// here the readable listing has no reason to carry.
    #[test]
    fn a_listing_shows_the_pc_the_byte_offset_and_the_raw_bytes() {
        let program = held();
        let id = FunctionId(0);
        let code = encode_function(program.function(id)).expect("it encodes");
        let text = listing(&program, id, &code);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("   0  +0     "), "{:?}", lines[0]);
        assert!(lines[0].ends_with("int s0:int 7"), "{:?}", lines[0]);
        assert!(lines[1].contains("+16"), "{:?}", lines[1]);
        assert!(lines[3].contains("+48"), "{:?}", lines[3]);
        assert_eq!(bytes(code[0]).len(), 32);
        assert!(bytes(code[0]).starts_with("02"), "{}", bytes(code[0]));
    }

    /// A disassembler is what somebody reaches for when the bytes are wrong,
    /// so bytes that decode to nothing print as the reason rather than
    /// stopping the listing.
    #[test]
    fn bytes_that_decode_to_nothing_say_so_rather_than_panicking() {
        let program = held();
        let id = FunctionId(0);
        let bad = EncodedInst::from_bytes([255u8; EncodedInst::BYTES]);
        assert_eq!(
            one(&program, id, bad, 0),
            "<flags is 255, and it is reserved and must be zero>"
        );
        let mut held = [0u8; EncodedInst::BYTES];
        held[0] = 255;
        let unknown = EncodedInst::from_bytes(held);
        assert_eq!(
            one(&program, id, unknown, 0),
            "<opcode 255 names no operation>"
        );
        assert!(listing(&program, id, &[bad, unknown]).contains("names no operation"));
    }
}
