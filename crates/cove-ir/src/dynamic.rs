//! [ADR 0068](../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
//! structural view: what a view is made of, and the one table of kinds.
//!
//! A value whose static type was erased — a `dyn Trait`, a Host `Any` — is a
//! box, and until this ADR the only things that could look inside one were
//! four Rust walks, one per standard-library operation. The ADR's diagnosis
//! is that those four are one missing ability: Cove could not inspect a value
//! after its static type was gone. The instructions from
//! [`Inst::DynOpen`](crate::Inst::DynOpen) to
//! [`Inst::DynChild`](crate::Inst::DynChild) are that ability, and this module
//! is the vocabulary they share. [`Inst::DynSameObject`](crate::Inst::DynSameObject)
//! is the one question beside them that is not about structure: whether two
//! views are one vector, which the standard library's walk asks to refuse a
//! value that contains itself (issue #493), and which answers a `Bool` so that
//! no identity reaches Cove as a number. [`Inst::DynNameOrder`](crate::Inst::DynNameOrder)
//! is [`Inst::DynSameType`](crate::Inst::DynSameType) asked three ways — where
//! one [`declared_name`], and then one case name, sorts against another —
//! which `std.dynamic.order` asks so that no name reaches Cove as text.
//!
//! # A view is three words, and none of them is Cove's to read
//!
//! A view is an ordinary inline value of [`crate::Program::view_layout`]:
//!
//! | word | [`Repr`] | holds |
//! |---|---|---|
//! | [`VIEW_LAYOUT`] | `Int` | the [`crate::LayoutId`] of the viewed value |
//! | [`VIEW_OWNER`] | `Ref` | the object the value is in, or is |
//! | [`VIEW_AT`] | `Int` | the payload word the value begins at |
//!
//! Three words rather than a handle into a table because a frame and a
//! `Vector` already know how to hold a run of words, and the collector
//! already knows how to trace one: the owner is a `Ref` word wherever the view
//! is, so the object stays reachable for exactly as long as the view does —
//! the ADR's "a live view keeps its owner reachable across allocation and
//! safepoints" is the reference map doing what it does for every other value.
//! No interior address is ever formed, so none survives a safepoint.
//!
//! The layout is an `opaque` struct, and nothing reads its fields by name:
//! no core intrinsic answers a layout or an offset, so a view's words are a
//! fact about this representation and not something the standard library can
//! depend on (Decision 2). A later representation — a handle into a per-run
//! table — changes this module and the machine, and no Cove.
//!
//! # The kinds are one table
//!
//! [`DynamicKind`] is the code [`Inst::DynKind`](crate::Inst::DynKind)
//! answers and the standard library will branch on. It is written once, here,
//! and both evaluators classify into it: the machine from a layout's shape,
//! the oracle from a `Value`'s variant.

use std::sync::Arc;

use crate::layout::{Field, Layout, LayoutId, Shape};
use crate::repr::Repr;

/// What a view's layout is called, in the table and in a listing.
pub const DYNAMIC_VIEW_NAME: &str = "DynamicView";

/// The view word holding the viewed value's [`LayoutId`], as an `Int`.
pub const VIEW_LAYOUT: u32 = 0;

/// The view word holding the object that roots the viewed value: the object
/// itself for a heap value, and the object it is inline in otherwise.
pub const VIEW_OWNER: u32 = 1;

/// The view word holding the payload word of [`VIEW_OWNER`] the value begins
/// at, as an `Int`. Nought for a heap value, which is its whole object.
pub const VIEW_AT: u32 = 2;

/// The words a view occupies, in order.
pub const VIEW_WORDS: [Repr; 3] = [Repr::Int, Repr::Ref, Repr::Int];

/// The layout of a view, given the layouts of an `Int` word and of a
/// reference word in the table it is going into.
///
/// An `opaque` struct of three fields, so that a view is inline — a slot run
/// in a frame, an element in a `Vector<DynamicView>` — and a listing names it
/// rather than printing its words. Its fields are named for what the words
/// hold and are read by no Cove source: nothing can write a field access on
/// a type whose fields a program cannot name.
pub fn view_layout(int: LayoutId, reference: LayoutId) -> Layout {
    let field = |name: &str, layout: LayoutId, at: u32| Field {
        name: Arc::from(name),
        layout,
        at,
    };
    Layout::inline(
        DYNAMIC_VIEW_NAME,
        Shape::Struct {
            fields: vec![
                field("layout", int, VIEW_LAYOUT),
                field("owner", reference, VIEW_OWNER),
                field("at", int, VIEW_AT),
            ],
            opaque: true,
        },
        VIEW_WORDS.to_vec(),
    )
}

/// The structural kind of a viewed value: what [`Inst::DynKind`](crate::Inst::DynKind)
/// answers, as [`DynamicKind::code`].
///
/// | code | kind | the machine's shape | the oracle's value |
/// |---|---|---|---|
/// | 0 | [`Unit`](DynamicKind::Unit) | `Word(Unit)` | `Unit` |
/// | 1 | [`Bool`](DynamicKind::Bool) | `Word(Bool)` | `Bool` |
/// | 2 | [`Int`](DynamicKind::Int) | `Word(Int)` | `Int` |
/// | 3 | [`Float`](DynamicKind::Float) | `Word(Float)` | `Float` |
/// | 4 | [`Duration`](DynamicKind::Duration) | `Word(Duration)` | `Duration` |
/// | 5 | [`String`](DynamicKind::String) | `Str` | `Str` |
/// | 6 | [`Struct`](DynamicKind::Struct) | a `Struct` that is not the program's `Range`, `Error` and an `opaque` struct included | `Struct` |
/// | 7 | [`Enum`](DynamicKind::Enum) | `Enum`, `Option` and `Result` included | `Enum` |
/// | 8 | [`Array`](DynamicKind::Array) | `Elements` | `Array` |
/// | 9 | [`Vector`](DynamicKind::Vector) | `Vector` | `Vector` |
/// | 10 | [`Set`](DynamicKind::Set) | `Members` | `Set` |
/// | 11 | [`Map`](DynamicKind::Map) | `Entries` | `Map` |
/// | 12 | [`Range`](DynamicKind::Range) | the program's `Range` struct | `Range` |
/// | 13 | [`Function`](DynamicKind::Function) | `Closure` | `Closure`, and a host operation used as a value |
/// | 14 | [`Opaque`](DynamicKind::Opaque) | every other shape: a `Host`, `Task`, `Scope`, `Addr` or `Tag` word, `Bytes`, `ByteBuffer` | every other value |
/// | 15 | [`Shared`](DynamicKind::Shared) | `Shared` | `Shared` |
///
/// The order is the table's and not a ranking: nothing may compare two codes
/// as numbers, and the ordering between kinds a `Map` key needs is
/// `MapKey`'s and is the standard library's to reproduce.
///
/// Decision 7 is the last three rows. A function, a Host handle, a task, a
/// scope and a synchronized cell do not become readable by being boxed, so
/// they have no children here and no scalar to read.
///
/// **A `Shared` cell is row 15 and not row 14**, which it was until ADR 0068's
/// Phase 4b-ii (issue #499's decision 5). It is no more readable than a
/// handle, and every walk treats it as one — equal to nothing, refused as a
/// key — but a rendering shows it as `<shared>`, which is in no layout of a
/// handle, so a walk has to be able to tell it apart. It was appended rather
/// than put beside the other opaque kinds so that no code already in the
/// table moved.
///
/// **An `opaque` struct is row 6 and not row 14.** It was row 14 until ADR
/// 0068's Phase 2, on the reading that its fields belong to the module that
/// declared it. They do, and nothing here changes that: a view is read only by
/// the standard library's own walks, which are trusted code, and what a walk
/// shows of one is the walk's decision — equality compares the fields, as `==`
/// always has on both evaluators, and a rendering shows only the name. Decision
/// 7 is about capabilities, and a struct whose fields are private is not one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DynamicKind {
    Unit,
    Bool,
    Int,
    Float,
    Duration,
    String,
    Struct,
    Enum,
    Array,
    Vector,
    Set,
    Map,
    Range,
    Function,
    Opaque,
    Shared,
}

impl DynamicKind {
    /// Every kind, in code order.
    pub const ALL: [DynamicKind; 16] = [
        DynamicKind::Unit,
        DynamicKind::Bool,
        DynamicKind::Int,
        DynamicKind::Float,
        DynamicKind::Duration,
        DynamicKind::String,
        DynamicKind::Struct,
        DynamicKind::Enum,
        DynamicKind::Array,
        DynamicKind::Vector,
        DynamicKind::Set,
        DynamicKind::Map,
        DynamicKind::Range,
        DynamicKind::Function,
        DynamicKind::Opaque,
        DynamicKind::Shared,
    ];

    /// The `Int` [`Inst::DynKind`](crate::Inst::DynKind) answers.
    pub fn code(self) -> i64 {
        self as i64
    }

    /// The kind `code` names, if it names one.
    pub fn from_code(code: i64) -> Option<DynamicKind> {
        usize::try_from(code)
            .ok()
            .and_then(|at| DynamicKind::ALL.get(at).copied())
    }

    /// Whether two values of this kind have one semantic type only when their
    /// declared names agree too — the three nominal kinds.
    pub fn is_nominal(self) -> bool {
        matches!(
            self,
            DynamicKind::Struct | DynamicKind::Enum | DynamicKind::Range
        )
    }

    /// The [`Repr`] of the word [`Inst::DynRead`](crate::Inst::DynRead) writes
    /// for a view of this kind, or `None` for a kind with nothing to read.
    ///
    /// A `String` is a reference to the string object the view names, and the
    /// four scalars are their own word.
    pub fn read_as(self) -> Option<Repr> {
        match self {
            DynamicKind::Bool => Some(Repr::Bool),
            DynamicKind::Int => Some(Repr::Int),
            DynamicKind::Float => Some(Repr::Float),
            DynamicKind::Duration => Some(Repr::Duration),
            DynamicKind::String => Some(Repr::Ref),
            _ => None,
        }
    }

    /// What a diagnostic calls this kind.
    pub fn name(self) -> &'static str {
        match self {
            DynamicKind::Unit => "Unit",
            DynamicKind::Bool => "Bool",
            DynamicKind::Int => "Int",
            DynamicKind::Float => "Float",
            DynamicKind::Duration => "Duration",
            DynamicKind::String => "String",
            DynamicKind::Struct => "struct",
            DynamicKind::Enum => "enum",
            DynamicKind::Array => "Array",
            DynamicKind::Vector => "Vector",
            DynamicKind::Set => "Set",
            DynamicKind::Map => "Map",
            DynamicKind::Range => "Range",
            DynamicKind::Function => "function",
            DynamicKind::Opaque => "opaque value",
            DynamicKind::Shared => "Shared",
        }
    }
}

/// A declared name with any instantiation left off: `m.Cell<Int>` is
/// `m.Cell`, and `Option` is `Option`.
///
/// What [`Inst::DynSameType`](crate::Inst::DynSameType) compares. A layout of
/// a generic declaration is named with its type arguments, because two
/// instantiations are two layouts; a *type identity* for reflection is the
/// declaration's, because the oracle's values carry no type arguments and the
/// ADR asks that `Option<Int>` and `Option<String>` be one type.
pub fn declared_name(name: &str) -> &str {
    name.split_once('<').map_or(name, |(head, _)| head)
}

/// What `==` says of a value that contains itself (issue #493).
///
/// Equality refuses a cycle rather than answering it, on every path that
/// compares: a walk the lowering composed for a layout that can reach itself
/// through a `Vector`, `std.dynamic.equals` over two boxes, and the oracle's
/// `Value::eq_value`. All three raise these three sentences, which are written
/// once here for the two in Rust; `std/dynamic.cove` spells them out itself,
/// and the `fail_equals_cycle_*` programs hold the three to one another byte
/// for byte on every evaluator.
///
/// One sentence for `==`, `!=` and `assertEqual`: a walk is shared by every
/// site that compares its layout, so it cannot say which operator reached it,
/// and the oracle says what the walk can.
pub const CONTAINS_ITSELF: &str = "this value contains itself, so `==` cannot compare it";

/// The rule [`CONTAINS_ITSELF`] is refused under.
pub const CONTAINS_ITSELF_RULE: &str = "Equality compares finite structure, and a value that \
                                        reaches itself through a `Vector` has no end to compare.";

/// What [`CONTAINS_ITSELF`] suggests instead.
pub const CONTAINS_ITSELF_HELP: &str = "compare the parts that do not lead back to the value, or \
                                        use `is` to ask whether two vectors are the same one";

#[cfg(test)]
mod tests {
    use super::*;

    /// A code is a position in [`DynamicKind::ALL`], and the table in the
    /// type's documentation is the order: pinned here, because the standard
    /// library will mirror these numbers and a reordering would change what
    /// its branches mean without changing anything that fails to compile.
    #[test]
    fn the_codes_are_the_documented_table() {
        let named: Vec<(i64, &str)> = DynamicKind::ALL
            .iter()
            .map(|kind| (kind.code(), kind.name()))
            .collect();
        assert_eq!(
            named,
            [
                (0, "Unit"),
                (1, "Bool"),
                (2, "Int"),
                (3, "Float"),
                (4, "Duration"),
                (5, "String"),
                (6, "struct"),
                (7, "enum"),
                (8, "Array"),
                (9, "Vector"),
                (10, "Set"),
                (11, "Map"),
                (12, "Range"),
                (13, "function"),
                (14, "opaque value"),
                (15, "Shared"),
            ]
        );
        for kind in DynamicKind::ALL {
            assert_eq!(DynamicKind::from_code(kind.code()), Some(kind));
        }
        assert_eq!(DynamicKind::from_code(16), None);
        assert_eq!(DynamicKind::from_code(-1), None);
    }

    #[test]
    fn an_instantiation_is_not_part_of_a_declared_name() {
        assert_eq!(declared_name("m.Cell<Int>"), "m.Cell");
        assert_eq!(declared_name("m.Cell<m.Pair<Int, String>>"), "m.Cell");
        assert_eq!(declared_name("Option"), "Option");
    }

    #[test]
    fn a_view_is_an_int_a_reference_and_an_int() {
        let layout = view_layout(LayoutId(4), LayoutId(7));
        assert_eq!(layout.words, VIEW_WORDS);
        assert!(layout.is_opaque());
        assert!(!layout.is_one_address());
    }
}
