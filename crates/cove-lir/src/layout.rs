//! What a value is made of.
//!
//! A [`Layout`] answers three questions about one family of values: how many
//! words a value of it occupies, what each of those words holds, and where
//! its parts are. That is all a frame slot, a heap object payload and a
//! garbage collection need, and it is deliberately one vocabulary for all
//! three — the stack region and the heap region are regions of one linear
//! memory, and a struct inside a closure environment is laid out the way a
//! struct in a frame is.
//!
//! # A value is a run of words
//!
//! [`docs/LINEAR_VM.md`](../../../docs/LINEAR_VM.md) states the rule:
//!
//! > One slot is one eight-byte word. One value may occupy one or more
//! > consecutive slots.
//!
//! So a `Point { x: Int, y: Int }` is two words *where the value is*, not one
//! word naming two words somewhere else. That is what makes ADR 0001's
//! field-wise shallow copy a copy: two words in, two words out. The earlier
//! design put every struct behind one address, which made an ordinary copy an
//! alias and then needed a sharing bit and copy-on-write to conceal it —
//! machinery that existed only to undo the representation choice.
//!
//! # What is inline and what is an address
//!
//! A value has a static width or it lives in the heap. Scalars, structs and
//! enums have one; strings, collections, closures, boxed values and
//! deliberately boxed recursive layouts do not, and a value of one of those
//! families is a single [`Repr::Ref`] word.
//!
//! A heap object's payload is described by a layout in exactly the same way,
//! so a struct stored in an array element or a closure environment is inline
//! in that payload, and the collector walks it with the same map.
//!
//! # This table describes families, not instantiations
//!
//! `Array<String>` and `Array<Point>` are one layout, because a reference is
//! a reference. `Array<Int>` and `Array<Duration>` are two, because their
//! words differ and a boundary has to know which. Nothing here grows a case
//! because a program was refused, and nothing here is a runtime type
//! universe: what an individual object *is* is a question its own header
//! answers.

use std::sync::Arc;

use crate::repr::Repr;
use crate::FunctionId;

/// Names a [`Layout`] in [`crate::Program::layouts`].
///
/// `LayoutId(0)` is reserved for [`Layout::free`]: the sweeper writes it into
/// the header of a reclaimed run of words so the heap stays a walkable
/// sequence of objects. No Cove value ever has it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayoutId(pub u32);

impl LayoutId {
    /// The layout of a reclaimed run of words.
    pub const FREE: LayoutId = LayoutId(0);

    /// The index this id names.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl std::fmt::Display for LayoutId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "layout{}", self.0)
    }
}

/// One field of a struct, and where it starts.
///
/// `at` is a word offset within the struct, so `l.from.x` is a slot number
/// the lowering computes and not an instruction the machine runs. A field of
/// an *inline* value costs nothing to reach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: Arc<str>,
    pub layout: LayoutId,
    pub at: u32,
}

/// One part of an enum case's payload, and where it sits in the payload
/// region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub layout: LayoutId,
    /// A word offset within the payload region, which begins *after* the
    /// discriminant word.
    pub at: u32,
}

/// One case of an enum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Case {
    pub name: Arc<str>,
    /// The parts of this case's payload, in declaration order.
    pub parts: Vec<Part>,
}

/// How a family's words are arranged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    /// A run of free words. Not a value; see [`LayoutId::FREE`].
    Free,
    /// One word of the given interpretation.
    ///
    /// The width-one case of the whole model, and the one every scalar is.
    Word(Repr),
    /// Consecutive fields, inline.
    Struct {
        fields: Vec<Field>,
        /// Whether the declaration was `export opaque struct`.
        ///
        /// A fact about a *declaration* on a table that otherwise describes
        /// families, and it is here because nothing downstream can derive it:
        /// by the time a value is a word, the declaration is gone. What reads
        /// it is a rendering, which shows an opaque value's name and nothing
        /// else — its fields are the declaring module's business, and a
        /// rendering is read by whoever the string reaches.
        opaque: bool,
    },
    /// Word 0 is the case index; the words after it are the payload region.
    ///
    /// The region is wide enough for every case, and its per-word [`Repr`]s
    /// are in [`Shape::Enum::payload`]. **Every case that uses a payload word
    /// agrees on that word's `Repr`** — the lowering assigns offsets under
    /// that constraint — because one static reference map has to be right
    /// whatever case a value holds. A word cannot be a reference in one case
    /// and an integer in another.
    ///
    /// Two things follow. Constructing a case zeroes the payload words it
    /// does not fill, so a reference word belonging to another case reads
    /// null. And a collection never reads the discriminant: the region's map
    /// is static, which is one fewer thing that can be wrong.
    ///
    /// The cost is a region that can be wider than the widest case. That is
    /// the price of a static map, paid in words rather than in a run-time
    /// question.
    Enum {
        cases: Vec<Case>,
        /// The payload region's words, after the discriminant.
        payload: Vec<Repr>,
    },
    /// UTF-8 bytes, eight to a word, little end first. The header's `len` is
    /// the byte count, so the payload is `len.div_ceil(8)` words and the
    /// trailing bytes of the last word are zero.
    Str,
    /// The header's `len` elements, each `elem`'s words, contiguous.
    ///
    /// One shape covers `Array<T>` for every `T`, and is also what a
    /// [`Shape::Vector`] stores its elements in — `growable` says which of
    /// the two an object is.
    Elements { elem: LayoutId, growable: bool },
    /// Payload word 0 is the element count; word 1 is a reference to the
    /// [`Shape::Elements`] object holding them.
    ///
    /// The indirection is what a growable value needs and an immutable one
    /// does not. A `Vector`'s identity is observable — `is` is defined for it
    /// and mutation through one copy is visible through every other — so
    /// growing must not move the object a program is holding. The header
    /// stays where it is and the store beneath it is replaced by a larger
    /// one. An `Array` needs none of that and pays none of it.
    Vector { elem: LayoutId },
    /// The header's `len` members, ascending and distinct.
    Members { elem: LayoutId },
    /// The header's `len` entries — key then value — ascending by key.
    Entries { key: LayoutId, value: LayoutId },
    /// Payload word 0 is the callee's [`FunctionId`]; the words after it are
    /// the captures, each inline under its own layout.
    Closure {
        function: FunctionId,
        captures: Vec<LayoutId>,
    },
    /// Payload word 0 is a [`LayoutId`]; the words after it are a value of
    /// that layout, inline.
    ///
    /// This is what an intentionally erased value occupies: `dyn Trait`, a
    /// Host result a schema declared `Any`, and a recursive layout that had
    /// to be broken. Erasure is where a value stops having a static width,
    /// and a heap object is where a value without a static width lives.
    Boxed,
}

/// The description of one family of values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// What a boundary calls a value of this family.
    ///
    /// Qualified for a declared type — `m.geometry.Point` — because a layout
    /// is an identity and two modules may each declare a `Point`. A rendering
    /// shortens it, which is what the public `Display` does with the same
    /// string.
    pub name: Arc<str>,
    pub shape: Shape,
    /// The words a value of this family occupies in a frame, or inline in a
    /// heap object's payload.
    ///
    /// Cached rather than computed, because computing it means walking the
    /// layout table and every reader of it is on a path where that would be
    /// the expensive part: a frame's reference map, a copy's width, a
    /// collection's walk.
    ///
    /// One [`Repr::Ref`] for every family that lives in the heap, which is
    /// what "a value has a static width or it lives in the heap" means when
    /// written down.
    pub words: Vec<Repr>,
}

impl Layout {
    /// The layout the sweeper writes into a reclaimed run of words.
    pub fn free() -> Layout {
        Layout {
            name: Arc::from("<free>"),
            shape: Shape::Free,
            words: Vec::new(),
        }
    }

    /// A one-word family.
    pub fn word(name: impl Into<Arc<str>>, repr: Repr) -> Layout {
        Layout {
            name: name.into(),
            shape: Shape::Word(repr),
            words: vec![repr],
        }
    }

    /// A family that lives in the heap, so a value of it is one reference.
    pub fn object(name: impl Into<Arc<str>>, shape: Shape) -> Layout {
        Layout {
            name: name.into(),
            shape,
            words: vec![Repr::Ref],
        }
    }

    /// An inline family, whose words the caller has already flattened.
    pub fn inline(name: impl Into<Arc<str>>, shape: Shape, words: Vec<Repr>) -> Layout {
        Layout {
            name: name.into(),
            shape,
            words,
        }
    }

    /// How many words a value of this family occupies.
    pub fn width(&self) -> u32 {
        self.words.len() as u32
    }

    /// Whether a value of this family is one reference rather than inline
    /// words.
    pub fn is_ref(&self) -> bool {
        self.words.len() == 1 && self.words[0].is_ref()
    }

    /// How many payload words an object of this layout with header length
    /// `len` occupies.
    ///
    /// `len` means different things to different shapes — a byte count for a
    /// string, an element count for an array, and nothing at all for a
    /// struct — and this is the one place that difference is written down.
    ///
    /// A `Struct` or an `Enum` answers its own inline words, because a boxed
    /// value's payload *is* the value.
    pub fn payload_words(&self, len: u32, layouts: &[Layout]) -> u32 {
        match &self.shape {
            Shape::Free => len,
            Shape::Str => len.div_ceil(8),
            Shape::Word(_) | Shape::Struct { .. } | Shape::Enum { .. } => self.width(),
            Shape::Elements { elem, .. } => len * layouts[elem.index()].width(),
            Shape::Vector { .. } => 2,
            Shape::Members { elem } => len * layouts[elem.index()].width(),
            Shape::Entries { key, value } => {
                len * (layouts[key.index()].width() + layouts[value.index()].width())
            }
            Shape::Closure { captures, .. } => {
                1 + captures
                    .iter()
                    .map(|id| layouts[id.index()].width())
                    .sum::<u32>()
            }
            // One word of `LayoutId` and then whatever it named, whose width
            // this layout cannot know: the header's `len` carries it.
            Shape::Boxed => 1 + len,
        }
    }

    /// Whether an object of this layout can hold a reference at all.
    ///
    /// The collector uses it to skip an object without looking at any of its
    /// words: a string, an `Array<Int>` and a boxed scalar are all leaves.
    pub fn may_hold_refs(&self, layouts: &[Layout]) -> bool {
        match &self.shape {
            Shape::Free | Shape::Str => false,
            Shape::Word(repr) => repr.is_ref(),
            Shape::Struct { .. } | Shape::Enum { .. } => {
                self.words.iter().any(|repr| repr.is_ref())
            }
            Shape::Elements { elem, .. } | Shape::Members { elem } => {
                layouts[elem.index()].words.iter().any(|r| r.is_ref())
            }
            Shape::Entries { key, value } => {
                layouts[key.index()].words.iter().any(|r| r.is_ref())
                    || layouts[value.index()].words.iter().any(|r| r.is_ref())
            }
            // Word 1 is always a reference to the store.
            Shape::Vector { .. } => true,
            Shape::Closure { captures, .. } => captures
                .iter()
                .any(|id| layouts[id.index()].words.iter().any(|r| r.is_ref())),
            // What a box holds is named by its own first payload word, so the
            // collector has to look.
            Shape::Boxed => true,
        }
    }

    /// The field `name`, if this is a struct-shaped layout.
    pub fn field(&self, name: &str) -> Option<&Field> {
        match &self.shape {
            Shape::Struct { fields, .. } => fields.iter().find(|field| &*field.name == name),
            _ => None,
        }
    }

    /// The case index `name` is at, if this is an enum-shaped layout.
    pub fn case(&self, name: &str) -> Option<u32> {
        match &self.shape {
            Shape::Enum { cases, .. } => cases
                .iter()
                .position(|case| &*case.name == name)
                .map(|at| at as u32),
            _ => None,
        }
    }

    /// Whether this is an `export opaque struct`.
    pub fn is_opaque(&self) -> bool {
        matches!(self.shape, Shape::Struct { opaque: true, .. })
    }
}

/// Lays out a struct's fields, answering the fields and the flattened words.
///
/// Fields are placed in declaration order with no padding: a word is a word
/// and there is nothing to align.
pub fn struct_layout(
    fields: &[(Arc<str>, LayoutId)],
    layouts: &[Layout],
) -> (Vec<Field>, Vec<Repr>) {
    let mut placed = Vec::with_capacity(fields.len());
    let mut words = Vec::new();
    for (name, layout) in fields {
        placed.push(Field {
            name: name.clone(),
            layout: *layout,
            at: words.len() as u32,
        });
        words.extend_from_slice(&layouts[layout.index()].words);
    }
    (placed, words)
}

/// Lays out an enum's payload region, answering the cases and the region's
/// words.
///
/// The one constraint is that **every case that uses a payload word agrees on
/// that word's `Repr`**, because one static reference map has to be right
/// whatever case a value holds. Each case's parts are placed greedily into
/// the lowest run of payload words that is free for this case and either
/// unassigned or already assigned the same `Repr`s.
///
/// A region can therefore be wider than the widest case — `A(Int, String)`
/// and `B(Float)` need four words between them, not three. That is the price
/// of a map a collection can read without asking which case a value is in.
pub fn enum_layout(
    cases: &[(Arc<str>, Vec<LayoutId>)],
    layouts: &[Layout],
) -> (Vec<Case>, Vec<Repr>) {
    let mut region: Vec<Repr> = Vec::new();
    let mut placed = Vec::with_capacity(cases.len());
    for (name, parts) in cases {
        let mut taken: Vec<bool> = vec![false; region.len()];
        let mut placed_parts = Vec::with_capacity(parts.len());
        for id in parts {
            let want = &layouts[id.index()].words;
            let at = fit(&mut region, &mut taken, want);
            placed_parts.push(Part {
                layout: *id,
                at: at as u32,
            });
        }
        placed.push(Case {
            name: name.clone(),
            parts: placed_parts,
        });
    }
    (placed, region)
}

/// The lowest offset in `region` where `want` fits: free for this case, and
/// either unassigned or already the same words. Extends the region if it has
/// to.
fn fit(region: &mut Vec<Repr>, taken: &mut Vec<bool>, want: &[Repr]) -> usize {
    let mut at = 0;
    'search: loop {
        for (i, repr) in want.iter().enumerate() {
            let word = at + i;
            if word < region.len() && (taken[word] || region[word] != *repr) {
                at += 1;
                continue 'search;
            }
        }
        break;
    }
    for (i, repr) in want.iter().enumerate() {
        let word = at + i;
        if word == region.len() {
            region.push(*repr);
            taken.push(false);
        }
        taken[word] = true;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Vec<Layout> {
        vec![
            Layout::free(),
            Layout::word("Int", Repr::Int),
            Layout::word("Float", Repr::Float),
            Layout::object("String", Shape::Str),
        ]
    }

    const INT: LayoutId = LayoutId(1);
    const FLOAT: LayoutId = LayoutId(2);
    const STR: LayoutId = LayoutId(3);

    #[test]
    fn a_struct_is_the_words_of_its_fields() {
        let layouts = table();
        let (fields, words) =
            struct_layout(&[(Arc::from("x"), INT), (Arc::from("y"), INT)], &layouts);
        assert_eq!(words, vec![Repr::Int, Repr::Int]);
        assert_eq!(fields[1].at, 1);
    }

    #[test]
    fn nesting_is_inline_and_recursive() {
        let mut layouts = table();
        let (fields, words) =
            struct_layout(&[(Arc::from("x"), INT), (Arc::from("y"), INT)], &layouts);
        layouts.push(Layout::inline(
            "Point",
            Shape::Struct {
                fields,
                opaque: false,
            },
            words,
        ));
        let point = LayoutId(layouts.len() as u32 - 1);

        let (fields, words) = struct_layout(
            &[(Arc::from("from"), point), (Arc::from("to"), point)],
            &layouts,
        );
        // Four words and no indirection: `l.to.x` is a slot offset.
        assert_eq!(words, vec![Repr::Int; 4]);
        assert_eq!(fields[1].at, 2);
    }

    /// ADR 0001's rule, as a layout: the `Point` words are inline and the
    /// `Vector` is one address, so one copy makes the first independent and
    /// leaves the second shared.
    #[test]
    fn a_struct_holding_a_vector_is_words_then_an_address() {
        let mut layouts = table();
        layouts.push(Layout::object("Vector", Shape::Vector { elem: INT }));
        let vector = LayoutId(layouts.len() as u32 - 1);
        let (_, words) = struct_layout(
            &[
                (Arc::from("a"), INT),
                (Arc::from("b"), FLOAT),
                (Arc::from("v"), vector),
            ],
            &layouts,
        );
        assert_eq!(words, vec![Repr::Int, Repr::Float, Repr::Ref]);
    }

    #[test]
    fn an_enums_payload_words_agree_across_its_cases() {
        let layouts = table();
        // `enum E { A(Int, String), B(Float) }`. `B` can use neither of `A`'s
        // words, so its `Float` takes a third.
        let (cases, payload) = enum_layout(
            &[
                (Arc::from("A"), vec![INT, STR]),
                (Arc::from("B"), vec![FLOAT]),
            ],
            &layouts,
        );
        assert_eq!(payload, vec![Repr::Int, Repr::Ref, Repr::Float]);
        assert_eq!(cases[0].parts[0].at, 0);
        assert_eq!(cases[0].parts[1].at, 1);
        assert_eq!(cases[1].parts[0].at, 2);
    }

    #[test]
    fn two_cases_of_one_shape_share_their_words() {
        let layouts = table();
        let (cases, payload) = enum_layout(
            &[(Arc::from("Ok"), vec![INT]), (Arc::from("Err"), vec![INT])],
            &layouts,
        );
        assert_eq!(payload, vec![Repr::Int]);
        assert_eq!(cases[1].parts[0].at, 0);
    }

    #[test]
    fn a_case_with_no_payload_costs_nothing() {
        let layouts = table();
        let (cases, payload) = enum_layout(
            &[(Arc::from("None"), vec![]), (Arc::from("Some"), vec![STR])],
            &layouts,
        );
        assert_eq!(payload, vec![Repr::Ref]);
        assert!(cases[0].parts.is_empty());
    }

    #[test]
    fn a_family_that_lives_in_the_heap_is_one_reference() {
        let layouts = table();
        assert_eq!(layouts[STR.index()].words, vec![Repr::Ref]);
        assert!(layouts[STR.index()].is_ref());
        assert!(!layouts[INT.index()].is_ref());
    }

    #[test]
    fn a_string_pays_one_word_per_eight_bytes() {
        let layouts = table();
        let str_layout = &layouts[STR.index()];
        assert_eq!(str_layout.payload_words(0, &layouts), 0);
        assert_eq!(str_layout.payload_words(9, &layouts), 2);
        assert!(!str_layout.may_hold_refs(&layouts));
    }

    #[test]
    fn an_array_of_multiword_elements_is_len_times_the_width() {
        let mut layouts = table();
        let (fields, words) =
            struct_layout(&[(Arc::from("x"), INT), (Arc::from("y"), INT)], &layouts);
        layouts.push(Layout::inline(
            "Point",
            Shape::Struct {
                fields,
                opaque: false,
            },
            words,
        ));
        let point = LayoutId(layouts.len() as u32 - 1);
        layouts.push(Layout::object(
            "Array",
            Shape::Elements {
                elem: point,
                growable: false,
            },
        ));
        let array = &layouts[layouts.len() - 1];
        assert_eq!(array.payload_words(5, &layouts), 10);
        assert!(!array.may_hold_refs(&layouts));
    }
}
