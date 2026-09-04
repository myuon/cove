//! The fixed-width encoded form of the instructions.
//!
//! [ADR 0041](../../../../docs/adr/0041-a-slot-number-fits-in-sixteen-bits.md)
//! decides everything here: sixteen bytes, one opcode byte, one reserved
//! `flags` byte that must be zero, three sixteen-bit fields that are always
//! frame slots, and one sixty-four-bit payload that is everything else.
//!
//! ```text
//! byte:  0        1        2   3     4   5     6   7     8 .. 15
//!        opcode   flags    a         b         c         payload
//!        u8       u8       u16       u16       u16       u64
//! ```
//!
//! # What this is for
//!
//! [`Inst`](crate::Inst) stays the compiler's representation: it is what the
//! lowering builds, what a listing prints, what a test asserts on, and what
//! the debugger shows as *lowered IR*. This is the other half of issue #245's
//! split — a form that is **verified once and then trusted**, so that a
//! dispatch loop can read an operand without asking whether it is in range.
//!
//! Nothing executes one yet. This module is the encoder, the decoder, the
//! verifier and the disassembly; running them is issue #245's Phase 3.
//!
//! # The layout is written and read by hand
//!
//! An [`EncodedInst`] is `[u8; 16]` and every field goes in and comes out
//! through the little-endian accessors below. There is no `#[repr(C)]`
//! struct, no `transmute` and no `bytemuck`: issue #245 asks for the width
//! and the byte order to be the format's own promise rather than something
//! inherited from what the Rust compiler happens to do with a declaration.
//!
//! # It is 1:1 with the readable IR
//!
//! Every `Inst` encodes to exactly one `EncodedInst` and back, so **bytecode
//! pc is IR pc**. That is what keeps [`Function::spans`](crate::Function),
//! [`Local`](crate::Local)'s pc ranges and [`Table::targets`](crate::Table)
//! meaning what they meant with no remapping, and it is why the disassembler
//! is [`decode()`] plus [`crate::print`] rather than a second renderer. See
//! [`disasm`].
//!
//! # Not a compatibility promise
//!
//! ADR 0041 is explicit: this is an internal executable representation. There
//! is no stable on-disk bytecode, no cross-version compatibility, no public
//! ABI, and **no opcode-number stability** — the numbers below are positions
//! in a generated table and move when the table does. [`verify()`] is
//! nevertheless safe against arbitrary bytes, because a verifier that is only
//! safe against its own encoder is not a verifier.

pub mod decode;
pub mod disasm;
pub mod encode;
pub mod op;
pub mod verify;

pub use decode::{decode, Malformed};
pub use disasm::listing;
pub use encode::{encode, encode_function, encode_program, Encoded, TooWide};
pub use op::{Half, Op, Operand, Payload};
pub use verify::{verify, Fault};

use crate::inst::Pc;

/// The most words one function's frame may hold.
///
/// A slot operand is sixteen bits, so slots 0 through 65,535 are nameable and
/// a frame of exactly 65,536 words is exactly encodable. ADR 0041 adopts this
/// as a compiler limit and `crate::lower` is where a frame over it is refused,
/// with a diagnostic at the declaration — never a truncation and never a wrap.
///
/// It is **not** the run's stack budget. `SEGMENT_WORDS` bounds one whole
/// task's stack at `1 << 20` words and is answered at run time by
/// `Memory::push_frame`; this bounds one *function*, at compile time, and is
/// one sixteenth of that.
pub const MAX_FRAME_WORDS: usize = 65_536;

/// One encoded instruction: sixteen bytes, little-endian.
///
/// Built by [`encode()`] and read back by [`decode()`]. The accessors are the
/// whole of the format — an [`EncodedInst`] means nothing except what they
/// say it means.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct EncodedInst([u8; EncodedInst::BYTES]);

impl EncodedInst {
    /// How wide one instruction is. Two words, four to a cache line, and the
    /// byte offset of instruction `pc` is `pc << 4`.
    pub const BYTES: usize = 16;

    /// An instruction from its fields, which is the only way the encoder
    /// builds one.
    pub fn new(opcode: u8, a: u16, b: u16, c: u16, payload: u64) -> EncodedInst {
        let mut bytes = [0u8; EncodedInst::BYTES];
        bytes[0] = opcode;
        // Byte 1 is `flags`, and it stays zero: ADR 0041 reserves it for a
        // fact that does not exist yet and requires the verifier to reject a
        // nonzero one. There is deliberately no way to set it here.
        bytes[2..4].copy_from_slice(&a.to_le_bytes());
        bytes[4..6].copy_from_slice(&b.to_le_bytes());
        bytes[6..8].copy_from_slice(&c.to_le_bytes());
        bytes[8..16].copy_from_slice(&payload.to_le_bytes());
        EncodedInst(bytes)
    }

    /// An instruction from bytes nothing here produced.
    ///
    /// This is how a verifier test, a debugger and a future loader get one,
    /// and it is why [`verify()`] may not assume anything about the contents.
    pub fn from_bytes(bytes: [u8; EncodedInst::BYTES]) -> EncodedInst {
        EncodedInst(bytes)
    }

    /// The sixteen bytes, as they are stored.
    pub fn bytes(&self) -> &[u8; EncodedInst::BYTES] {
        &self.0
    }

    /// Byte 0: which operation this is. See [`Op`].
    pub fn opcode(&self) -> u8 {
        self.0[0]
    }

    /// Byte 1: reserved, and required to be zero.
    pub fn flags(&self) -> u8 {
        self.0[1]
    }

    /// Bytes 2–3: the first slot field.
    pub fn a(&self) -> u16 {
        u16::from_le_bytes([self.0[2], self.0[3]])
    }

    /// Bytes 4–5: the second slot field.
    pub fn b(&self) -> u16 {
        u16::from_le_bytes([self.0[4], self.0[5]])
    }

    /// Bytes 6–7: the third slot field.
    pub fn c(&self) -> u16 {
        u16::from_le_bytes([self.0[6], self.0[7]])
    }

    /// Bytes 8–15: everything that is not a slot.
    pub fn payload(&self) -> u64 {
        u64::from_le_bytes([
            self.0[8], self.0[9], self.0[10], self.0[11], self.0[12], self.0[13], self.0[14],
            self.0[15],
        ])
    }

    /// The payload's low half, which is where a single id goes.
    pub fn lo(&self) -> u32 {
        self.payload() as u32
    }

    /// The payload's high half, which is where a second id goes.
    pub fn hi(&self) -> u32 {
        (self.payload() >> 32) as u32
    }

    /// The byte offset of instruction `pc`, which a fixed width makes a
    /// shift.
    pub fn offset_of(pc: Pc) -> usize {
        (pc as usize) << 4
    }
}

/// Sixteen bytes in hex, so that a failing test says which byte.
impl std::fmt::Debug for EncodedInst {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EncodedInst(")?;
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        f.write_str(")")
    }
}

/// A byte sequence that is not a whole number of instructions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Truncated {
    /// How many bytes there were.
    pub bytes: usize,
    /// How many of them are left over after the last whole instruction.
    pub over: usize,
}

impl std::fmt::Display for Truncated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} bytes is {} instructions and {} bytes over, and an instruction is {}",
            self.bytes,
            self.bytes / EncodedInst::BYTES,
            self.over,
            EncodedInst::BYTES
        )
    }
}

/// Reads a run of bytes as instructions, refusing a partial one.
///
/// The one place truncation is a question a fixed width can be asked: an
/// [`EncodedInst`] is always sixteen bytes, so a short *instruction* cannot
/// exist, but a short *stream* can — a file cut off, a `Uint8Array` handed
/// across the Wasm boundary with the wrong length. This is where that is
/// refused, before anything indexes into it.
pub fn instructions(bytes: &[u8]) -> Result<Vec<EncodedInst>, Truncated> {
    let over = bytes.len() % EncodedInst::BYTES;
    if over != 0 {
        return Err(Truncated {
            bytes: bytes.len(),
            over,
        });
    }
    Ok(bytes
        .chunks_exact(EncodedInst::BYTES)
        .map(|chunk| {
            let mut held = [0u8; EncodedInst::BYTES];
            held.copy_from_slice(chunk);
            EncodedInst::from_bytes(held)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field goes in at the byte offset ADR 0041's diagram gives it,
    /// little-endian, and `flags` is zero because nothing can set it.
    #[test]
    fn the_sixteen_bytes_are_the_layout_the_adr_draws() {
        let inst = EncodedInst::new(0x2a, 0x0102, 0x0304, 0x0506, 0x0807_0605_0403_0201);
        assert_eq!(
            inst.bytes(),
            &[
                0x2a, 0x00, // opcode, flags
                0x02, 0x01, // a
                0x04, 0x03, // b
                0x06, 0x05, // c
                0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // payload
            ]
        );
        assert_eq!(inst.opcode(), 0x2a);
        assert_eq!(inst.flags(), 0);
        assert_eq!(inst.a(), 0x0102);
        assert_eq!(inst.b(), 0x0304);
        assert_eq!(inst.c(), 0x0506);
        assert_eq!(inst.payload(), 0x0807_0605_0403_0201);
        assert_eq!(inst.lo(), 0x0403_0201);
        assert_eq!(inst.hi(), 0x0807_0605);
    }

    /// The byte offset of an instruction is a shift, which is the whole
    /// arithmetic argument for sixteen rather than twenty-four.
    #[test]
    fn the_byte_offset_of_a_pc_is_that_pc_times_sixteen() {
        assert_eq!(EncodedInst::offset_of(0), 0);
        assert_eq!(EncodedInst::offset_of(1), 16);
        assert_eq!(EncodedInst::offset_of(1_000), 16_000);
    }

    /// A stream that is not a whole number of instructions is refused rather
    /// than rounded down.
    #[test]
    fn a_stream_that_stops_mid_instruction_is_refused() {
        assert_eq!(instructions(&[]), Ok(Vec::new()));
        assert_eq!(instructions(&[0u8; 32]).map(|held| held.len()), Ok(2));
        assert_eq!(
            instructions(&[0u8; 17]),
            Err(Truncated { bytes: 17, over: 1 })
        );
        assert_eq!(
            instructions(&[0u8; 15]),
            Err(Truncated {
                bytes: 15,
                over: 15
            })
        );
    }

    /// The limit is exactly what a `u16` slot can name, one past the largest
    /// slot number.
    #[test]
    fn the_frame_limit_is_one_past_the_largest_slot_a_u16_names() {
        assert_eq!(MAX_FRAME_WORDS, u16::MAX as usize + 1);
    }
}
