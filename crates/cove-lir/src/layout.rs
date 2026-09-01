//! What a heap object is made of.
//!
//! A heap object is a header word and a run of payload words:
//!
//! ~~~text
//! +0  header:  [ layout: u32 | len: u32 ]
//! +1  payload word 0
//! +2  payload word 1
//! ...
//! ~~~
//!
//! The header names a [`LayoutId`], and the [`Layout`] is what says how many
//! payload words there are, which of them are references, and what the
//! object is called when a boundary has to render it.
//!
//! This is the one table [ADR 0034](../../../docs/adr/0034-one-physical-word-stack.md)
//! permits: *"Static type/layout tables ... describe or manage values but do
//! not store general Cove values."* It is not a runtime type universe and it
//! does not grow a case per corpus refusal. A family of values is described
//! generally — every `Array<T>` is one [`Shape::Elements`] whatever `T` is —
//! and what an individual object *is* is answered by the object, at run time,
//! by reading its own header.

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

/// One field of a struct-shaped object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: Arc<str>,
    pub repr: Repr,
}

/// One case of an enum-shaped object.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Case {
    pub name: Arc<str>,
    /// The payload words of this case, in order. Empty for a case with none.
    pub payload: Vec<Repr>,
}

/// How an object's payload words are arranged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    /// A run of free words. Not a value; see [`LayoutId::FREE`].
    Free,
    /// UTF-8 bytes, eight to a word, little end first. The header's `len` is
    /// the byte count, so the payload is `len.div_ceil(8)` words and the
    /// trailing bytes of the last word are zero.
    ///
    /// A string holds no references, which is why a program that only moves
    /// strings around still scans its frames in the time a scalar program
    /// does.
    Str,
    /// A fixed run of named fields.
    ///
    /// This is what a declared `struct` is. It is also what the machine uses
    /// for the few compound values that have a fixed shape and a name of
    /// their own, such as a range.
    Struct {
        fields: Vec<Field>,
        /// Whether the declaration was `export opaque struct`.
        ///
        /// The one thing outside this crate that reads it is a rendering: an
        /// opaque value shows its name and nothing else, because its fields
        /// are the declaring module's own business and a rendering is read by
        /// whoever the string reaches. Printing them would publish through
        /// `println` what the checker refuses to let a caller name, which is
        /// ADR 0014's whole point.
        ///
        /// It is a fact about a *declaration* on a table that otherwise
        /// describes families, and it is here rather than derived because
        /// nothing downstream can derive it: by the time a value is a word,
        /// the declaration is gone.
        opaque: bool,
    },
    /// Payload word 0 is the case index; words `1..` are that case's
    /// payload.
    ///
    /// The object is sized for the widest case, so every case fits and an
    /// assignment never has to reallocate. Which payload words are
    /// references therefore depends on which case the object is *in*, and
    /// the collector reads word 0 to find out. That is a fact about an
    /// object, answered by the object; it is not a static kind per case.
    Enum { cases: Vec<Case> },
    /// The header's `len` elements, each one word of `elem`, contiguous.
    ///
    /// One shape covers `Array<T>` for every `T`, and is also what a
    /// [`Shape::Vector`] stores its elements in — `growable` says which of
    /// the two an object is. That keeps the layout table a description of
    /// families rather than a list of every instantiation the corpus happens
    /// to contain.
    Elements { elem: Repr, growable: bool },
    /// Payload word 0 is the element count; word 1 is a reference to the
    /// [`Shape::Elements`] object holding them.
    ///
    /// The indirection is what a growable value needs and an immutable one
    /// does not. A `Vector`'s identity is observable — `is` is defined for it
    /// and mutation through one copy is visible through every other — so
    /// growing must not move the object a program is holding a reference to.
    /// The header stays where it is and the store beneath it is replaced by a
    /// larger one.
    ///
    /// An `Array` needs none of that, and pays none of it: its elements are
    /// in the object, one indirection nearer.
    Vector { elem: Repr },
    /// Payload word 0 is the callee's [`FunctionId`]; words `1..` are the
    /// captures, in the order [`crate::Function::captures`] lists them.
    ///
    /// The function is in the object rather than only in the layout because
    /// a closure value is called through, and the call needs the id without
    /// a table lookup. It is in the layout as well because the capture
    /// reprs come from the callee, and one layout per lowered lambda is one
    /// per *source* lambda, not one per closure created.
    Closure {
        function: FunctionId,
        captures: Vec<Repr>,
    },
    /// The header's `len` members, one word of `elem` each, in ascending
    /// order with no duplicates.
    ///
    /// A `Set` is a sorted run rather than a hash table because the language
    /// says it iterates in ascending order and renders that way, so the order
    /// is part of the value and not an implementation's leftovers. Membership
    /// is a binary search, which is what a sorted run is for.
    ///
    /// It is a shape of its own rather than an [`Shape::Elements`] with a
    /// name, because "these words are sorted and distinct" is an invariant a
    /// builtin may rely on and an array's words are neither.
    Members { elem: Repr },
    /// The header's `len` entries, two words each — key then value — in
    /// ascending key order with no duplicate keys.
    ///
    /// The same reasoning as [`Shape::Members`], with a second word per
    /// entry. A `Map` is ordered by its keys in the language, so a lookup is
    /// a binary search over the key words and an iteration walks them in
    /// place.
    Entries { key: Repr, value: Repr },
    /// Payload word 0 is a [`Repr`] discriminant; word 1 is the value.
    ///
    /// This is what a value whose static type the checker did not settle
    /// occupies: `dyn Display`, a Host result a schema declared `Any`, an
    /// expression under `Ty::Unknown`. Boxing costs an allocation on a path
    /// that was never going to be fast, and it buys one word per value
    /// everywhere else and a reference map that is one bit per slot.
    Boxed,
}

/// The description of one family of heap objects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// What a boundary calls an object of this layout.
    pub name: Arc<str>,
    pub shape: Shape,
}

impl Layout {
    /// The layout the sweeper writes into a reclaimed run of words.
    pub fn free() -> Layout {
        Layout {
            name: Arc::from("<free>"),
            shape: Shape::Free,
        }
    }

    /// How many payload words an object of this layout with header length
    /// `len` occupies.
    ///
    /// `len` means different things to different shapes — a byte count for a
    /// string, an element count for an array, and nothing at all for a
    /// struct — and this is the one place that difference is written down.
    pub fn payload_words(&self, len: u32) -> u32 {
        match &self.shape {
            Shape::Free => len,
            Shape::Str => len.div_ceil(8),
            Shape::Struct { fields, .. } => fields.len() as u32,
            Shape::Enum { cases } => 1 + Self::widest_case(cases),
            Shape::Elements { .. } => len,
            Shape::Vector { .. } => 2,
            Shape::Members { .. } => len,
            Shape::Entries { .. } => len * 2,
            Shape::Closure { captures, .. } => 1 + captures.len() as u32,
            Shape::Boxed => 2,
        }
    }

    /// The payload words of the widest case of an enum.
    fn widest_case(cases: &[Case]) -> u32 {
        cases
            .iter()
            .map(|case| case.payload.len() as u32)
            .max()
            .unwrap_or(0)
    }

    /// Whether an object of this layout can hold a reference at all.
    ///
    /// The collector uses it to skip an object without looking at any of its
    /// words: a string, a `Array<Int>` and a boxed scalar are all leaves.
    pub fn may_hold_refs(&self) -> bool {
        match &self.shape {
            Shape::Free | Shape::Str => false,
            Shape::Struct { fields, .. } => fields.iter().any(|field| field.repr.is_ref()),
            Shape::Enum { cases } => cases
                .iter()
                .any(|case| case.payload.iter().any(|repr| repr.is_ref())),
            Shape::Elements { elem, .. } => elem.is_ref(),
            // Word 1 is always a reference to the store, whatever the
            // elements are.
            Shape::Vector { .. } => true,
            Shape::Members { elem } => elem.is_ref(),
            Shape::Entries { key, value } => key.is_ref() || value.is_ref(),
            Shape::Closure { captures, .. } => captures.iter().any(|repr| repr.is_ref()),
            // A boxed word is a reference exactly when its tag says so, and
            // the tag is in the object. The collector has to look.
            Shape::Boxed => true,
        }
    }

    /// The field index `name` is at, if this is a struct-shaped layout.
    pub fn field(&self, name: &str) -> Option<u32> {
        match &self.shape {
            Shape::Struct { fields, .. } => fields
                .iter()
                .position(|field| &*field.name == name)
                .map(|at| at as u32),
            _ => None,
        }
    }

    /// The case index `name` is at, if this is an enum-shaped layout.
    pub fn case(&self, name: &str) -> Option<u32> {
        match &self.shape {
            Shape::Enum { cases } => cases
                .iter()
                .position(|case| &*case.name == name)
                .map(|at| at as u32),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, repr: Repr) -> Field {
        Field {
            name: Arc::from(name),
            repr,
        }
    }

    #[test]
    fn a_string_pays_one_word_per_eight_bytes() {
        let layout = Layout {
            name: Arc::from("String"),
            shape: Shape::Str,
        };
        assert_eq!(layout.payload_words(0), 0);
        assert_eq!(layout.payload_words(1), 1);
        assert_eq!(layout.payload_words(8), 1);
        assert_eq!(layout.payload_words(9), 2);
        assert!(!layout.may_hold_refs());
    }

    #[test]
    fn an_enum_is_sized_for_its_widest_case() {
        // `Result<Str, Str>`: one payload word either way. `Option<T>`: one
        // for `Some`, none for `None`, and the object is sized for `Some`.
        let layout = Layout {
            name: Arc::from("Option"),
            shape: Shape::Enum {
                cases: vec![
                    Case {
                        name: Arc::from("None"),
                        payload: vec![],
                    },
                    Case {
                        name: Arc::from("Some"),
                        payload: vec![Repr::Ref],
                    },
                ],
            },
        };
        // One word for the case index, one for the widest payload.
        assert_eq!(layout.payload_words(0), 2);
        assert!(layout.may_hold_refs());
        assert_eq!(layout.case("Some"), Some(1));
        assert_eq!(layout.case("Nothing"), None);
    }

    #[test]
    fn a_scalar_struct_is_a_leaf() {
        let layout = Layout {
            name: Arc::from("Point"),
            shape: Shape::Struct {
                fields: vec![field("x", Repr::Int), field("y", Repr::Int)],
                opaque: false,
            },
        };
        assert_eq!(layout.payload_words(0), 2);
        assert!(!layout.may_hold_refs());
        assert_eq!(layout.field("y"), Some(1));
    }

    #[test]
    fn an_array_of_scalars_is_a_leaf_and_an_array_of_refs_is_not() {
        let ints = Layout {
            name: Arc::from("Array"),
            shape: Shape::Elements {
                elem: Repr::Int,
                growable: false,
            },
        };
        let refs = Layout {
            name: Arc::from("Array"),
            shape: Shape::Elements {
                elem: Repr::Ref,
                growable: false,
            },
        };
        assert_eq!(ints.payload_words(5), 5);
        assert!(!ints.may_hold_refs());
        assert!(refs.may_hold_refs());
    }
}
