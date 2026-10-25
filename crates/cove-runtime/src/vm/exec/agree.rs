//! A field or an element is read as what it holds: [ADR 0120]'s check, at
//! the one place a word crosses from an object into a frame or back.
//!
//! A `Repr::Ref` slot carries no layout. `cove_ir::verify` holds a field
//! instruction's *destination* to the instruction's layout, but which object
//! the instruction reaches is a run-time fact, so before this the machine took
//! the layout's word for what was in the object, and only bounded the offset.
//! An instruction whose layout put a reference where the object holds an
//! `Int` read a number as an address — a header nobody wrote, a payload past
//! any object, a collector tracing a word that names nothing — and one that
//! put an `Int` over a reference lost an object the collector was keeping. No
//! lowering emits either, and every other check the machine makes assumes it
//! never meets one.
//!
//! So a field access is held to the object's header: the words it moves have
//! to be references exactly where the object's are, inside the payload, and
//! not a word the machine keeps for itself ([`cove_ir::Access`]). An element
//! access is held to the run's unit: the instruction's layout has to be one
//! unit wide, with references where the unit has them. The rule is
//! `cove_ir::field_agrees` and `cove_ir::unit_agrees`, which the native tier's
//! emitted fast path is built from too; what is here is the same rule as a
//! table, so that the common case is a few word operations and not a walk of
//! a layout.
//!
//! [ADR 0120]: ../../../../../docs/adr/0120-the-vm-runs-only-what-verifies.md

use std::sync::Arc;

use cove_ir::{Access, Layout, LayoutId, Program, Repr, Shape};

use super::{null_object, Machine};
use crate::error::RuntimeError;

/// One layout's answers for the fast path, built once per program.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Agree {
    /// For an object of this layout whose payload is fixed and at most 64
    /// words: how many words, which are references, which a field may read
    /// and which it may write. `fixed` is `0` otherwise, and the slow path
    /// answers.
    fixed: u32,
    refs: u64,
    loads: u64,
    stores: u64,
    /// For this layout as what an instruction moves: its width, and which of
    /// its words are references — `value` is `false` when it is wider than 64
    /// words or holds an address, and the slow path answers.
    value: bool,
    width: u32,
    value_refs: u64,
    /// For an object of this layout that is a run of units at most 64 words
    /// wide with no address in them: the unit's width and its references.
    unit: Option<(u32, u64)>,
}

/// What a word may be read and written as, built once per program: every
/// layout's [`Agree`], and every `AddrOfField`'s need.
#[derive(Debug, Default)]
pub(crate) struct Tables {
    /// In `LayoutId` order.
    pub(crate) layouts: Box<[Agree]>,
    /// By function and pc, for each [`cove_ir::Inst::AddrOfField`]: what the
    /// address it forms is used as — `cove_ir::verify::places::field_needs` —
    /// as `(word offset, repr, written)`.
    places: Box<[Box<[Option<Box<[(u32, Repr, bool)]>>]>]>,
    /// By function, which slots of its frame are addresses: what a collection
    /// reads to keep an object a frame holds the address of a field of.
    pub(crate) addrs: Box<[cove_ir::RefMap]>,
}

/// The [`Tables`] of `program`, which `cove_ir::verify` accepted.
pub(crate) fn tables(program: &Program) -> Arc<Tables> {
    let forms_a_field_address = program.functions.iter().any(|function| {
        function
            .code
            .iter()
            .any(|inst| matches!(inst, cove_ir::Inst::AddrOfField { .. }))
    });
    let places = if forms_a_field_address {
        cove_ir::verify::places::field_needs(program)
            .into_iter()
            .map(|code| {
                code.into_iter()
                    .map(|need| {
                        need.map(|need| {
                            need.into_iter()
                                .map(|(at, (repr, written))| (at, repr, written))
                                .collect()
                        })
                    })
                    .collect()
            })
            .collect()
    } else {
        Box::default()
    };
    // A `RefMap` over the address slots: the reference map's shape, asked of
    // another `Repr`.
    let addrs = program
        .functions
        .iter()
        .map(|function| {
            let marked: Vec<Repr> = function
                .reprs
                .iter()
                .map(|repr| match repr {
                    Repr::Addr => Repr::Ref,
                    _ => Repr::Int,
                })
                .collect();
            cove_ir::RefMap::of(&marked)
        })
        .collect();
    Arc::new(Tables {
        layouts: layout_tables(program),
        places,
        addrs,
    })
}

/// Every layout's [`Agree`], in `LayoutId` order.
fn layout_tables(program: &Program) -> Box<[Agree]> {
    let layouts = &program.layouts;
    layouts
        .iter()
        .map(|layout| {
            let mut agree = Agree::default();
            if let Some(access) = layout.fixed_access(layouts) {
                if !access.is_empty() && access.len() <= 64 {
                    agree.fixed = access.len() as u32;
                    for (at, word) in access.iter().enumerate() {
                        let bit = 1u64 << at;
                        match word {
                            Access::Value(is_ref) => {
                                agree.loads |= bit;
                                agree.stores |= bit;
                                if *is_ref {
                                    agree.refs |= bit;
                                }
                            }
                            Access::ReadOnly(is_ref) => {
                                agree.loads |= bit;
                                if *is_ref {
                                    agree.refs |= bit;
                                }
                            }
                            Access::Sealed => {}
                        }
                    }
                }
            }
            if layout.words.len() <= 64 && !layout.words.contains(&Repr::Addr) {
                agree.value = true;
                agree.width = layout.width();
                agree.value_refs = refs_of(&layout.words);
            }
            if let Some(words) = cove_ir::unit_words(layouts, &layout.shape) {
                if words.len() <= 64 && !words.contains(&Repr::Addr) {
                    agree.unit = Some((words.len() as u32, refs_of(&words)));
                }
            }
            agree
        })
        .collect()
}

fn refs_of(words: &[Repr]) -> u64 {
    words
        .iter()
        .enumerate()
        .filter(|(_, repr)| repr.is_ref())
        .fold(0, |bits, (at, _)| bits | 1u64 << at)
}

fn mask(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

impl Machine<'_> {
    /// That a field instruction naming `layout` at payload word `at` may read
    /// (or, with `store`, write) the object at `addr`: non-null, inside its
    /// payload, and agreeing with it word for word. See the module.
    #[inline]
    pub(crate) fn field(
        &self,
        addr: u64,
        at: u32,
        layout: LayoutId,
        store: bool,
    ) -> Result<(), RuntimeError> {
        if addr == 0 {
            return Err(null_object());
        }
        let object = self.mem.object_layout(addr);
        if let (Some(held), Some(want)) = (
            self.agree.layouts.get(object.index()),
            self.agree.layouts.get(layout.index()),
        ) {
            let end = u64::from(at) + u64::from(want.width);
            if held.fixed != 0 && want.value && end <= u64::from(held.fixed) {
                let words = mask(want.width) << at;
                let allowed = if store { held.stores } else { held.loads };
                if allowed & words == words && held.refs & words == want.value_refs << at {
                    return Ok(());
                }
                return Err(self.disagrees(object, at, layout, store));
            }
        }
        self.field_slow(addr, object, at, layout, store)
    }

    /// [`Machine::field`] for what the table does not answer: a payload the
    /// header decides, or a layout wider than the table's words.
    #[cold]
    fn field_slow(
        &self,
        addr: u64,
        object: LayoutId,
        at: u32,
        layout: LayoutId,
        store: bool,
    ) -> Result<(), RuntimeError> {
        let layouts = &self.program.layouts;
        let held = &layouts[object.index()];
        let want = &layouts[layout.index()].words;
        let len = self.mem.object_len(addr);
        // The access of payload word `k`, and how many words there are.
        let (words, access): (u64, Box<dyn Fn(u64) -> Access + '_>) =
            match (&held.shape, held.fixed_access(layouts)) {
                (Shape::Free, _) => {
                    return Err(RuntimeError::new("this value was read after it was reclaimed"))
                }
                (_, Some(fixed)) => (fixed.len() as u64, Box::new(move |k| fixed[k as usize])),
                (Shape::Str | Shape::Bytes, _) => (
                    u64::from(len).div_ceil(8),
                    Box::new(|_| Access::ReadOnly(false)),
                ),
                (Shape::Boxed, _) => {
                    let inner = LayoutId(self.mem.payload(addr, 0) as u32);
                    let Some(inner) = layouts.get(inner.index()) else {
                        return Err(self.disagrees(object, at, layout, store));
                    };
                    let inner = inner.words.clone();
                    (
                        1 + u64::from(len),
                        Box::new(move |k| match k {
                            0 => Access::ReadOnly(false),
                            k => match inner.get(k as usize - 1) {
                                Some(Repr::Addr) | None => Access::Sealed,
                                Some(repr) => Access::Value(repr.is_ref()),
                            },
                        }),
                    )
                }
                (shape, None) => match cove_ir::unit_words(layouts, shape) {
                    Some(unit) if !unit.is_empty() => {
                        let stride = unit.len() as u64;
                        (
                            u64::from(len) * stride,
                            Box::new(move |k| match unit[(k % stride) as usize] {
                                Repr::Addr => Access::Sealed,
                                repr => Access::Value(repr.is_ref()),
                            }),
                        )
                    }
                    Some(_) => (0, Box::new(|_| Access::Sealed)),
                    // An identity table's words are addresses kept from the
                    // collector, and nothing reads them as a field.
                    None => (u64::from(len), Box::new(|_| Access::Sealed)),
                },
            };
        let end = u64::from(at) + want.len() as u64;
        if end > words {
            return Err(RuntimeError::new(format!(
                "this reads word {at} of a `{}`, which has {words}",
                held.name
            )));
        }
        let agrees = want
            .iter()
            .enumerate()
            .all(|(k, repr)| access(u64::from(at) + k as u64).admits(*repr, store));
        if agrees {
            Ok(())
        } else {
            Err(self.disagrees(object, at, layout, store))
        }
    }

    /// That the `AddrOfField` at `pc` of function `id` may form the address of
    /// payload word `at` of the object at `addr`: non-null, an object whose
    /// payload is fixed — a run's or a box's can be relabelled or freed under
    /// a place — with the word inside it, and every word the address is used
    /// at, read or written, admitting that use ([ADR 0120]'s need, which
    /// `cove_ir::verify` worked out for the slot the address goes into).
    ///
    /// [ADR 0120]: ../../../../../docs/adr/0120-the-vm-runs-only-what-verifies.md
    pub(crate) fn addressable(
        &self,
        addr: u64,
        at: u32,
        id: cove_ir::FunctionId,
        pc: usize,
    ) -> Result<(), RuntimeError> {
        if addr == 0 {
            return Err(null_object());
        }
        let object = self.mem.object_layout(addr);
        let layouts = &self.program.layouts;
        let held = &layouts[object.index()];
        let Some(access) = held.fixed_access(layouts) else {
            return Err(RuntimeError::new(format!(
                "this forms the address of a word of a `{}`, whose payload is not fixed",
                held.name
            ))
            .with_rule("A place is formed only in an object whose words the layout fixes."));
        };
        if at as usize >= access.len() {
            return Err(RuntimeError::new(format!(
                "this reads word {at} of a `{}`, which has {}",
                held.name,
                access.len()
            )));
        }
        let need = self
            .agree
            .places
            .get(id.index())
            .and_then(|code| code.get(pc))
            .and_then(Option::as_deref)
            .unwrap_or(&[]);
        for &(offset, repr, written) in need {
            let word = u64::from(at) + u64::from(offset);
            let admitted = access
                .get(word as usize)
                .is_some_and(|access| access.admits(repr, written));
            if !admitted {
                let verb = if written { "written" } else { "read" };
                return Err(RuntimeError::new(format!(
                    "this forms the address of word {at} of a `{}`, and word {word} is {verb} \
                     through it as {repr}, which is not what the object holds there",
                    held.name
                ))
                .with_rule("A word of an object is read and written as what the object holds there."));
            }
        }
        Ok(())
    }

    /// The refusal of a field access whose words are not the object's.
    #[cold]
    pub(crate) fn disagrees(
        &self,
        object: LayoutId,
        at: u32,
        layout: LayoutId,
        store: bool,
    ) -> RuntimeError {
        let name = |id: LayoutId| {
            self.program
                .layouts
                .get(id.index())
                .map_or_else(|| id.to_string(), |held: &Layout| held.name.to_string())
        };
        let verb = if store { "writes" } else { "reads" };
        RuntimeError::new(format!(
            "this {verb} word {at} of a `{}` as a `{}`, which is not what it holds there",
            name(object),
            name(layout)
        ))
        .with_rule("A word of an object is read and written as what the object holds there.")
    }

    /// The payload offset of element `index` of the run at `addr`, for an
    /// element instruction naming `layout`: non-null, a run whose unit is
    /// `layout`'s words, and an index inside it. See the module.
    #[inline]
    pub(crate) fn unit(
        &self,
        addr: u64,
        index: i64,
        layout: LayoutId,
        store: bool,
    ) -> Result<u32, RuntimeError> {
        if addr == 0 {
            return Err(null_object());
        }
        let object = self.mem.object_layout(addr);
        let fast = match (
            self.agree.layouts.get(object.index()),
            self.agree.layouts.get(layout.index()),
        ) {
            (Some(held), Some(want)) => {
                want.value && held.unit == Some((want.width, want.value_refs))
            }
            _ => false,
        };
        if !fast && !cove_ir::unit_agrees(&self.program.layouts, object, layout) {
            return Err(self.not_its_unit(object, layout, store));
        }
        self.element(addr, index, self.width(layout))
    }

    /// The refusal of an element access whose layout is not the run's unit.
    #[cold]
    pub(crate) fn not_its_unit(&self, object: LayoutId, layout: LayoutId, store: bool) -> RuntimeError {
        let name = |id: LayoutId| {
            self.program
                .layouts
                .get(id.index())
                .map_or_else(|| id.to_string(), |held: &Layout| held.name.to_string())
        };
        let verb = if store { "writes" } else { "reads" };
        RuntimeError::new(format!(
            "this {verb} an element of a `{}` as a `{}`, which is not what it holds",
            name(object),
            name(layout)
        ))
        .with_rule("An element of a run is read and written as the run's own unit.")
    }
}
