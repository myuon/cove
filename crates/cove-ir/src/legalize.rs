//! Which runs of primitive instructions a backend may treat as one step:
//! [ADR 0062]'s push and append windows, defined once.
//!
//! Shared IR spells an append as a length read, `GrowableEnsure`, a store read,
//! one typed write and `GrowableCommit`, and `crate::verify`'s reservation rule
//! is what makes that sound. What the split costs is dispatch: a push the
//! encoded VM runs row by row is seven or eight turns of its loop where the
//! composite `growable-push` was one. The ADR's answer is legalization behind
//! the backends rather than a composite instruction in front of them, and this
//! module is the **one pattern definition** every consumer asks — the encoded
//! VM's fused heads, both native code generators and the inliner's `THIN`
//! count. None of them recognises `Vector.push` by name, and none keeps a
//! private copy of the shape.
//!
//! # The windows
//!
//! ```text
//! Push(S):   load-field at <- o +0      the head
//!            int n <- 1
//!            growable-ensure o, n
//!            load-field st <- o +1
//!            store-elem st[at] <- src   S = Words(E), of E
//!          | run-store st[at] <- src    S = PackedBytes
//!            [clear st]
//!            [int m <- 1]
//!            growable-commit o, m | n
//!
//! Append(S): load-field at <- o +0      the head
//!            [int n <- k]
//!            growable-ensure o, n
//!            [int z <- v]
//!            load-field st <- o +1
//!            [int z <- v]               at most one `int z` in all
//!            run-copy [st, at, src, from, n]
//!            [clear st]
//!            [int m <- k]               only after an `int n <- k`
//!            growable-commit o, m | n
//! ```
//!
//! The bracketed rows are the optional instructions the reservation rule admits
//! and a lowering is likely to leave there — the clear `frees` puts after the
//! store's last read, and the constants a `0` offset and a second `1` are. The
//! set is deliberately this small: a row outside it is not a window, and a
//! window that does not match is **correct primitive IR** that every backend
//! runs as such. Nothing is lost but a dispatch.
//!
//! The head is always the length read, so a window is at most
//! [`MAX_ROWS`] rows and no row inside one can be the head of another: the
//! only other `load-field` in a window reads word 1. Every program counter is
//! therefore the head of at most one window, and [`windows`]' scan from the top
//! finds the same set as asking [`recognize`] at every pc.
//!
//! # What a match promises
//!
//! **Exactly one the verifier accepts.** A window alone in its block passes
//! `crate::verify`'s reservation rule; the tests below hold that over every
//! shape and over mutations of each. So the slot relations the rule needs are
//! part of the match rather than left to it: the length and the store are
//! read into slots that are not the owner's and not the count's, no optional
//! row writes a slot the rule forbids, and no row after the head is a branch or
//! table target — that is [`crate::flow::leaders`], and a target inside would
//! be a second way in.
//!
//! It also promises what a fused VM arm needs in order to check once and then
//! write: a push's unit does not overlap the count or the store slot, so the
//! unit read before the window is the unit the write reads.
//!
//! **Not that the window is reached with nothing open.** Recognition is local
//! to the rows, and an unwritten reservation opened earlier in the block would
//! make this window's ensure a second one. The verifier refuses that program
//! before anything asks this module about it.
//!
//! [ADR 0062]: ../../../docs/adr/0062-an-append-is-ensure-store-commit.md

use crate::inst::{Inst, Slot, Storage};
use crate::layout::LayoutId;
use crate::program::{Function, Program};

/// Payload word 0 of a growable owner, its logical length.
pub const LENGTH: u32 = 0;

/// Payload word 1 of a growable owner, its store.
pub const STORE: u32 = 1;

/// The most rows a window has: a push with both optional rows, or an append
/// with all three.
pub const MAX_ROWS: usize = 9;

/// Which window, and over which storage: one member per fused VM opcode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Pattern {
    /// One element onto a `Vector`: the write is a `store-elem`.
    PushWords,
    /// One byte onto a byte buffer: the write is a `run-store`.
    PushByte,
    /// A run of bytes onto a byte buffer: the write is a byte `run-copy`.
    AppendBytes,
    /// A run of elements onto a `Vector`: the write is a word `run-copy`.
    AppendWords,
}

impl Pattern {
    /// Every pattern, in [`Pattern::index`] order.
    pub const ALL: [Pattern; 4] = [
        Pattern::PushWords,
        Pattern::PushByte,
        Pattern::AppendBytes,
        Pattern::AppendWords,
    ];

    /// Where this pattern is in [`Pattern::ALL`], for a table of counts.
    pub fn index(self) -> usize {
        match self {
            Pattern::PushWords => 0,
            Pattern::PushByte => 1,
            Pattern::AppendBytes => 2,
            Pattern::AppendWords => 3,
        }
    }

    /// What a report calls it.
    pub fn name(self) -> &'static str {
        match self {
            Pattern::PushWords => "push.words",
            Pattern::PushByte => "push.byte",
            Pattern::AppendBytes => "append.bytes",
            Pattern::AppendWords => "append.words",
        }
    }
}

/// What a row of a window writes into the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Written {
    /// The owner's length, by the head.
    Length,
    /// A constant, by an `int`.
    Constant(i64),
    /// The owner's store, by the second `load-field`.
    Store,
    /// Nothing, by a `clear` of the store slot.
    Cleared,
}

/// One frame write a window makes: every one is a single word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameWrite {
    pub pc: usize,
    pub slot: Slot,
    pub written: Written,
}

/// A recognised window, decoded: its operands, the program counter of each
/// member, and the frame writes a backend that runs it as one step must still
/// make, so that the frame is the one the rows would have left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub pattern: Pattern,
    pub storage: Storage,
    /// The first row, the length read.
    pub head: usize,
    /// How many rows, head included.
    pub rows: usize,
    /// The growable owner.
    pub owner: Slot,
    /// The slot the head reads the length into, which the write is at.
    pub at: Slot,
    /// The slot the ensure asks for room by.
    pub count: Slot,
    /// The constant written into `count` just before the ensure, if one was.
    /// A push's is always `1`.
    pub constant: Option<i64>,
    /// The slot the store is read into after the ensure.
    pub store: Slot,
    /// A push's unit, `stride` words long, or the run an append copies from.
    pub src: Slot,
    /// The slot holding the offset an append copies from; `None` for a push.
    pub from: Option<Slot>,
    /// Words per unit: the element's width for [`Storage::Words`], and `1`
    /// for a byte, which is what a push's `src` is.
    pub stride: u32,
    /// Where each member is.
    pub ensure: usize,
    pub load_store: usize,
    pub write: usize,
    pub clear: Option<usize>,
    pub commit: usize,
    /// The slot the commit names: `count`, or a second slot holding its
    /// constant.
    pub committed: Slot,
    writes: [Option<FrameWrite>; 6],
}

impl Window {
    /// The frame writes the window's rows make, in program-counter order.
    pub fn frame_writes(&self) -> impl Iterator<Item = FrameWrite> + '_ {
        self.writes.iter().flatten().copied()
    }

    /// The program counters after the head.
    pub fn tail(&self) -> std::ops::Range<usize> {
        self.head + 1..self.head + self.rows
    }
}

/// The window whose head is `code[head]`, if there is one.
///
/// `leaders` is [`crate::flow::leaders`] of the same code: a row after the head
/// that begins a block is where some branch lands, and a window may not be
/// entered anywhere but its head.
pub fn recognize(
    program: &Program,
    code: &[Inst],
    leaders: &[bool],
    head: usize,
) -> Option<Window> {
    let word = |layout: LayoutId| {
        program
            .layouts
            .get(layout.index())
            .is_some_and(|held| held.width() == 1)
    };
    let mut writes = Writes::default();
    let &Inst::LoadField {
        dst: at,
        obj: owner,
        at: LENGTH,
        layout,
    } = code.get(head)?
    else {
        return None;
    };
    if at == owner || !word(layout) {
        return None;
    }
    writes.push(head, at, Written::Length);
    let mut rows = Rows {
        code,
        leaders,
        pc: head + 1,
    };

    // The count: a constant written just before the ensure, or a slot it names.
    let mut constant = None;
    if let Some(&Inst::Int { dst, value }) = rows.peek() {
        if dst == owner || dst == at {
            return None;
        }
        writes.push(rows.pc, dst, Written::Constant(value));
        constant = Some((dst, value));
        rows.pc += 1;
    }
    let ensure = rows.pc;
    let &Inst::GrowableEnsure {
        owner: ensured,
        additional: count,
        storage,
    } = rows.take()?
    else {
        return None;
    };
    let counted = match constant {
        Some((slot, _)) => slot == count,
        None => count != owner && count != at,
    };
    if ensured != owner || !counted {
        return None;
    }

    // An append's offset constant, before or after the store is read, and the
    // store read itself.
    // `None` for a row that is a constant into a slot the rule reads.
    fn take_offset(
        rows: &mut Rows<'_>,
        writes: &mut Writes,
        offset: &mut Option<Slot>,
        read: &[Slot],
    ) -> Option<()> {
        if offset.is_some() {
            return Some(());
        }
        if let Some(&Inst::Int { dst, value }) = rows.peek() {
            if read.contains(&dst) {
                return None;
            }
            writes.push(rows.pc, dst, Written::Constant(value));
            *offset = Some(dst);
            rows.pc += 1;
        }
        Some(())
    }
    let mut offset: Option<Slot> = None;
    take_offset(&mut rows, &mut writes, &mut offset, &[owner, at, count])?;
    let load_store = rows.pc;
    let &Inst::LoadField {
        dst: store,
        obj,
        at: STORE,
        layout,
    } = rows.take()?
    else {
        return None;
    };
    if obj != owner || !word(layout) || [owner, at, count].contains(&store) || offset == Some(store)
    {
        return None;
    }
    writes.push(load_store, store, Written::Store);
    take_offset(
        &mut rows,
        &mut writes,
        &mut offset,
        &[owner, at, count, store],
    )?;

    // The write, which decides the pattern.
    let write = rows.pc;
    let (pattern, src, from, stride) = match *rows.take()? {
        Inst::StoreElem {
            obj,
            index,
            src,
            layout,
        } if obj == store && index == at && storage == Storage::Words(layout) => {
            let stride = program.layouts.get(layout.index())?.width();
            (Pattern::PushWords, src, None, stride)
        }
        Inst::RunStore {
            run,
            index,
            src,
            storage: Storage::PackedBytes,
        } if run == store && index == at && storage == Storage::PackedBytes => {
            (Pattern::PushByte, src, None, 1)
        }
        Inst::RunCopy {
            args,
            storage: copied,
        } if copied == storage => {
            let [dst, dst_at, src, from, n] = program.args.get(args.index())?.as_slice() else {
                return None;
            };
            if dst.slot != store || dst_at.slot != at || n.slot != count {
                return None;
            }
            let (pattern, stride) = match storage {
                Storage::PackedBytes => (Pattern::AppendBytes, 1),
                Storage::Words(elem) => (
                    Pattern::AppendWords,
                    program.layouts.get(elem.index())?.width(),
                ),
            };
            (pattern, src.slot, Some(from.slot), stride)
        }
        _ => return None,
    };
    if matches!(pattern, Pattern::PushWords | Pattern::PushByte) {
        let unit = src..src.saturating_add(stride);
        if constant.map(|(_, value)| value) != Some(1)
            || offset.is_some()
            || unit.contains(&count)
            || unit.contains(&store)
        {
            return None;
        }
    }

    // What may follow the write before the commit: only rows that cannot fail.
    let mut clear = None;
    if let Some(&Inst::Clear { slot, layout }) = rows.peek() {
        if slot != store || !word(layout) {
            return None;
        }
        writes.push(rows.pc, slot, Written::Cleared);
        clear = Some(rows.pc);
        rows.pc += 1;
    }
    let mut committed = count;
    if let Some(&Inst::Int { dst, value }) = rows.peek() {
        if constant.map(|(_, held)| held) != Some(value) || dst == owner || dst == count {
            return None;
        }
        writes.push(rows.pc, dst, Written::Constant(value));
        committed = dst;
        rows.pc += 1;
    }
    let commit = rows.pc;
    let &Inst::GrowableCommit {
        owner: onto,
        count: published,
        storage: into,
    } = rows.take()?
    else {
        return None;
    };
    if onto != owner || published != committed || into != storage {
        return None;
    }
    Some(Window {
        pattern,
        storage,
        head,
        rows: rows.pc - head,
        owner,
        at,
        count,
        constant: constant.map(|(_, value)| value),
        store,
        src,
        from,
        stride,
        ensure,
        load_store,
        write,
        clear,
        commit,
        committed,
        writes: writes.held,
    })
}

/// How many rows the window headed at `pc` is, if one is.
///
/// The question the inliner asks when it counts a callee's steps: a window is
/// one step to a backend, so it is one step to the size a thin body is held to.
pub fn window_len_at(
    program: &Program,
    code: &[Inst],
    leaders: &[bool],
    pc: usize,
) -> Option<usize> {
    recognize(program, code, leaders, pc).map(|window| window.rows)
}

/// Every window in `function`, in program-counter order.
pub fn windows(program: &Program, function: &Function) -> Vec<Window> {
    let leaders = crate::flow::leaders(program, function);
    let mut found = Vec::new();
    let mut pc = 0;
    while pc < function.code.len() {
        match recognize(program, &function.code, &leaders, pc) {
            Some(window) => {
                pc += window.rows;
                found.push(window);
            }
            None => pc += 1,
        }
    }
    found
}

/// The rows after a head, read in order, refusing one where a branch lands.
struct Rows<'a> {
    code: &'a [Inst],
    leaders: &'a [bool],
    pc: usize,
}

impl<'a> Rows<'a> {
    fn peek(&self) -> Option<&'a Inst> {
        if self.leaders.get(self.pc).copied().unwrap_or(true) {
            return None;
        }
        self.code.get(self.pc)
    }

    fn take(&mut self) -> Option<&'a Inst> {
        let inst = self.peek()?;
        self.pc += 1;
        Some(inst)
    }
}

/// A window's frame writes, gathered as its rows are read.
#[derive(Default)]
struct Writes {
    held: [Option<FrameWrite>; 6],
    len: usize,
}

impl Writes {
    fn push(&mut self, pc: usize, slot: Slot, written: Written) {
        self.held[self.len] = Some(FrameWrite { pc, slot, written });
        self.len += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cove_diag::{FileId, Span};

    use super::*;
    use crate::layout::{Layout, Shape};
    use crate::program::Arg;
    use crate::repr::{RefMap, Repr};
    use crate::ArgsId;

    const INT: LayoutId = LayoutId(0);
    const STR: LayoutId = LayoutId(1);
    const POINT: LayoutId = LayoutId(2);

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
        ]
    }

    /// `s0` the owner, `s1` its length, `s2` the count, `s3` the store,
    /// `s4..s6` a unit (two words for a `Point`), `s6` a second count, `s7` the
    /// run an append copies from, `s8` its offset, `s9` a `Bool`.
    fn reprs() -> Vec<Repr> {
        vec![
            Repr::Ref,
            Repr::Int,
            Repr::Int,
            Repr::Ref,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Ref,
            Repr::Int,
            Repr::Bool,
        ]
    }

    fn program(code: Vec<Inst>) -> Program {
        let span = Span::new(FileId(0), 0, 0);
        let reprs = reprs();
        let arg = |slot, layout| Arg { slot, layout };
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
                inlined: Vec::new(),
                span,
                is_async: false,
                stub: false,
            }],
            layouts: layouts(),
            str_layout: STR,
            args: vec![vec![
                arg(3, STR),
                arg(1, INT),
                arg(7, STR),
                arg(8, INT),
                arg(2, INT),
            ]],
            ..Program::default()
        }
    }

    fn length() -> Inst {
        Inst::LoadField {
            dst: 1,
            obj: 0,
            at: LENGTH,
            layout: INT,
        }
    }

    fn store() -> Inst {
        Inst::LoadField {
            dst: 3,
            obj: 0,
            at: STORE,
            layout: STR,
        }
    }

    fn one(dst: Slot) -> Inst {
        Inst::Int { dst, value: 1 }
    }

    fn clear() -> Inst {
        Inst::Clear {
            slot: 3,
            layout: STR,
        }
    }

    fn ensure(storage: Storage) -> Inst {
        Inst::GrowableEnsure {
            owner: 0,
            additional: 2,
            storage,
        }
    }

    fn commit(count: Slot, storage: Storage) -> Inst {
        Inst::GrowableCommit {
            owner: 0,
            count,
            storage,
        }
    }

    fn push_words(layout: LayoutId) -> Vec<Inst> {
        let words = Storage::Words(layout);
        vec![
            length(),
            one(2),
            ensure(words),
            store(),
            Inst::StoreElem {
                obj: 3,
                index: 1,
                src: 4,
                layout,
            },
            clear(),
            one(6),
            commit(6, words),
        ]
    }

    fn push_byte() -> Vec<Inst> {
        let bytes = Storage::PackedBytes;
        vec![
            length(),
            one(2),
            ensure(bytes),
            store(),
            Inst::RunStore {
                run: 3,
                index: 1,
                src: 4,
                storage: bytes,
            },
            commit(2, bytes),
        ]
    }

    fn append(storage: Storage) -> Vec<Inst> {
        vec![
            length(),
            ensure(storage),
            store(),
            Inst::Int { dst: 8, value: 0 },
            Inst::RunCopy {
                args: ArgsId(0),
                storage,
            },
            clear(),
            commit(2, storage),
        ]
    }

    /// Every shape the module doc draws, with and without its optional rows.
    fn shapes() -> Vec<(Pattern, Vec<Inst>)> {
        let bytes = Storage::PackedBytes;
        let mut all = vec![
            (Pattern::PushWords, push_words(INT)),
            (Pattern::PushWords, push_words(POINT)),
            (Pattern::PushByte, push_byte()),
            (Pattern::AppendBytes, append(bytes)),
            (Pattern::AppendWords, append(Storage::Words(INT))),
        ];
        // A push without its clear and without its second constant.
        let mut bare = push_words(INT);
        bare.remove(6);
        bare.remove(5);
        bare[5] = commit(2, Storage::Words(INT));
        all.push((Pattern::PushWords, bare));
        // A byte push with both.
        let mut full = push_byte();
        full.insert(5, clear());
        full.insert(6, one(6));
        full[7] = commit(6, bytes);
        all.push((Pattern::PushByte, full));
        // An append whose offset is written before the store is read, and one
        // with no offset constant and no clear.
        let mut early = append(bytes);
        early.swap(2, 3);
        all.push((Pattern::AppendBytes, early));
        let mut plain = append(bytes);
        plain.remove(5);
        plain.remove(3);
        all.push((Pattern::AppendBytes, plain));
        // An append of a constant count, committed through a second constant.
        let mut constant = append(bytes);
        constant.insert(1, Inst::Int { dst: 2, value: 5 });
        constant.insert(7, Inst::Int { dst: 6, value: 5 });
        let last = constant.len() - 1;
        constant[last] = commit(6, bytes);
        all.push((Pattern::AppendBytes, constant));
        all
    }

    fn found(code: &[Inst]) -> Vec<Window> {
        let held = program(code.to_vec());
        windows(&held, &held.functions[0])
    }

    /// Whether `code`, followed by a return, is a program the verifier accepts.
    fn verifies(code: &[Inst]) -> Result<(), Vec<String>> {
        let mut code = code.to_vec();
        code.push(Inst::Return { src: 1 });
        crate::verify(&program(code))
            .map_err(|faults| faults.into_iter().map(|fault| fault.what).collect())
    }

    /// **Every drawn shape is recognised, whole, and verifies.**
    #[test]
    fn every_window_the_doc_draws_is_recognised_and_verifies() {
        for (pattern, code) in shapes() {
            assert_eq!(verifies(&code), Ok(()), "{code:#?}");
            let held = found(&code);
            assert_eq!(held.len(), 1, "{code:#?}");
            let window = held[0];
            assert_eq!(window.pattern, pattern);
            assert_eq!((window.head, window.rows), (0, code.len()));
            assert_eq!((window.owner, window.at, window.count), (0, 1, 2));
            assert_eq!(window.store, 3);
            assert!(matches!(code[window.ensure], Inst::GrowableEnsure { .. }));
            assert!(matches!(
                code[window.load_store],
                Inst::LoadField { at: STORE, .. }
            ));
            assert!(matches!(code[window.commit], Inst::GrowableCommit { .. }));
            let program = program(code.clone());
            let leaders = crate::flow::leaders(&program, &program.functions[0]);
            assert_eq!(
                window_len_at(&program, &code, &leaders, 0),
                Some(code.len())
            );
            // Every row that writes the frame is in the list, in order, and
            // nothing else is.
            let listed: Vec<(usize, Slot)> = window
                .frame_writes()
                .map(|write| (write.pc, write.slot))
                .collect();
            let mut wrote = Vec::new();
            for (pc, inst) in code.iter().enumerate() {
                inst.writes(&program, &mut |slot, width| {
                    assert_eq!(width, 1, "{inst:?}");
                    wrote.push((pc, slot));
                });
            }
            assert_eq!(listed, wrote, "{pattern:?}");
        }
    }

    /// The decoded operands of a two-word push and of an append.
    #[test]
    fn a_window_names_its_operands() {
        let push = found(&push_words(POINT))[0];
        assert_eq!(push.storage, Storage::Words(POINT));
        assert_eq!((push.src, push.stride, push.from), (4, 2, None));
        assert_eq!((push.constant, push.committed), (Some(1), 6));
        assert_eq!((push.write, push.clear, push.commit), (4, Some(5), 7));
        assert_eq!(push.tail(), 1..8);

        let copy = found(&append(Storage::PackedBytes))[0];
        assert_eq!((copy.src, copy.from, copy.stride), (7, Some(8), 1));
        assert_eq!((copy.constant, copy.committed), (None, 2));
        assert_eq!(
            copy.frame_writes()
                .map(|write| write.written)
                .collect::<Vec<_>>(),
            vec![
                Written::Length,
                Written::Store,
                Written::Constant(0),
                Written::Cleared
            ]
        );
    }

    /// **Recognised implies verified**, over every way of damaging each shape
    /// by one step: a row deleted, two rows swapped, and a row inserted from a
    /// menu of the instructions that sit around windows. Whatever the scan
    /// still finds in the damaged code, alone in its block, is a reservation the
    /// verifier accepts. And the damage that must break the match does.
    #[test]
    fn a_damaged_window_is_recognised_only_where_it_still_verifies() {
        let menu = [
            Inst::Int { dst: 8, value: 0 },
            Inst::Int { dst: 2, value: 1 },
            Inst::Int { dst: 0, value: 1 },
            Inst::Int { dst: 1, value: 1 },
            clear(),
            store(),
            length(),
            Inst::Copy {
                dst: 8,
                src: 1,
                layout: INT,
            },
            Inst::Unit { dst: 6 },
            Inst::Len { dst: 2, obj: 7 },
        ];
        let mut checked = 0;
        for (_, shape) in shapes() {
            let mut damaged: Vec<Vec<Inst>> = Vec::new();
            for at in 0..shape.len() {
                let mut code = shape.clone();
                code.remove(at);
                damaged.push(code);
                for with in at + 1..shape.len() {
                    let mut code = shape.clone();
                    code.swap(at, with);
                    damaged.push(code);
                }
            }
            for at in 0..=shape.len() {
                for inst in &menu {
                    let mut code = shape.clone();
                    code.insert(at, inst.clone());
                    damaged.push(code);
                }
            }
            for code in damaged {
                for window in found(&code) {
                    let rows = &code[window.head..window.head + window.rows];
                    assert_eq!(verifies(rows), Ok(()), "{rows:#?}");
                    checked += 1;
                }
            }
        }
        assert!(
            checked > 100,
            "only {checked} damaged windows were recognised"
        );
    }

    /// The four kinds of damage issue #409 names, each of which a backend must
    /// not run as a window.
    #[test]
    fn a_window_that_is_not_the_shape_is_not_recognised() {
        let words = Storage::Words(INT);
        // Reordered: the store read before the ensure, which a growth may
        // replace.
        let mut stale = push_words(INT);
        stale.swap(2, 3);
        assert!(found(&stale).is_empty());
        // Reordered: the commit's constant written before the write.
        let mut early = push_words(INT);
        early.swap(4, 6);
        assert!(found(&early).is_empty());
        // An extra instruction between two members, even one the reservation
        // rule would admit.
        for at in [2, 3, 4, 5, 7] {
            let mut extra = push_words(INT);
            extra.insert(
                at,
                Inst::Copy {
                    dst: 8,
                    src: 1,
                    layout: INT,
                },
            );
            assert!(found(&extra).is_empty(), "an extra row at +{at}");
        }
        // A missing clear is a window; a missing ensure, write or commit is not.
        let mut unclear = push_words(INT);
        unclear.remove(5);
        assert_eq!(found(&unclear).len(), 1);
        for at in [1, 2, 3, 4, 7] {
            let mut missing = push_words(INT);
            missing.remove(at);
            assert!(found(&missing).is_empty(), "without +{at}");
        }
        // A branch or table target at every interior row, and at the head,
        // which is the one place a window may be entered.
        for target in 1..push_words(INT).len() {
            let mut code = vec![Inst::BranchFalse {
                cond: 9,
                to: target as u32 + 1,
            }];
            code.extend(push_words(INT));
            let held = found(&code);
            assert!(held.is_empty(), "a target at +{target}: {held:?}");
        }
        let mut entered = vec![Inst::BranchFalse { cond: 9, to: 1 }];
        entered.extend(push_words(INT));
        assert_eq!(found(&entered).len(), 1);
        // Slots the rule reads written by an optional row: the offset constant
        // into the count or the length, the store into the count, and a commit
        // constant into the count.
        let mut into_count = append(Storage::PackedBytes);
        into_count[3] = Inst::Int { dst: 2, value: 0 };
        assert!(found(&into_count).is_empty());
        let mut into_length = append(Storage::PackedBytes);
        into_length[3] = Inst::Int { dst: 1, value: 0 };
        assert!(found(&into_length).is_empty());
        let mut store_over_count = push_words(INT);
        store_over_count[3] = Inst::LoadField {
            dst: 2,
            obj: 0,
            at: STORE,
            layout: STR,
        };
        assert!(found(&store_over_count).is_empty());
        let mut commit_over_count = push_words(INT);
        commit_over_count[6] = one(2);
        commit_over_count[7] = commit(2, words);
        assert!(found(&commit_over_count).is_empty());
        // A push of anything but one, and a push whose unit is the count.
        let mut two = push_words(INT);
        two[1] = Inst::Int { dst: 2, value: 2 };
        assert!(found(&two).is_empty());
        let mut unit_is_count = push_byte();
        unit_is_count[4] = Inst::RunStore {
            run: 3,
            index: 1,
            src: 2,
            storage: Storage::PackedBytes,
        };
        assert!(found(&unit_is_count).is_empty());
        // A write of another storage than the one ensured.
        let mut other = push_words(INT);
        other[4] = Inst::StoreElem {
            obj: 3,
            index: 1,
            src: 4,
            layout: POINT,
        };
        assert!(found(&other).is_empty());
    }
}
