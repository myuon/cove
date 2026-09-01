//! What a type is once it is words.
//!
//! One question is asked of a type here and nowhere else: which [`Layout`] a
//! value of it has. Everything downstream — how wide a location is, which of
//! its words the collector traces, where a field sits, which payload word a
//! case writes — is read off that answer, so a struct's field order and an
//! enum's case order are decided once.
//!
//! # A value is a run of words, and the layout says how many
//!
//! `docs/LINEAR_VM.md` states the rule this module implements:
//!
//! > One slot is one eight-byte word. One value may occupy one or more
//! > consecutive slots.
//!
//! So a scalar is one word, a `struct` is the consecutive words of its
//! fields, an enum is a discriminant word and a payload region, and only the
//! families whose storage has no static width — a string, a collection, a
//! closure, an erased value — are a single [`Repr::Ref`].
//!
//! # A layout describes a family
//!
//! `Array<String>` and `Array<Point>` are one layout because a reference is
//! a reference; `Array<Int>` and `Array<Duration>` are two because their
//! words differ and a boundary has to know which. [`Shapes`] interns them,
//! so one shape is one [`LayoutId`] however many times the source writes it.
//!
//! # Recursion is where boxing is decided
//!
//! `struct Node { value: Int, next: Option<Node> }` has no finite inline
//! width. [`Shapes::of`] holds the nominal declarations it is part way
//! through, and an occurrence of one of those inside its own layout is
//! answered with a [`Shape::Boxed`] layout — one `Ref` word naming an object
//! whose payload is the type's own inline words. The cycle is broken at that
//! occurrence and nowhere else, so nothing about a `Point` changes because a
//! `Node` exists.
//!
//! Which types that happened to is recorded in [`Shapes::boxed`], and
//! [`Shapes::unboxed`] answers what a box holds — the pair a `Box`/`Unbox`
//! at a use site is emitted from.
//!
//! # It reads the checker's answers
//!
//! A struct's fields and an enum's cases are read from the `Signature` the
//! checker recorded for the declaration. Nothing here re-resolves an
//! annotation, because an annotation is a name and only the checker knows
//! what the name meant in the module it was written in.

use std::collections::HashMap;
use std::sync::Arc;

use cove_sema::resolve::Program as Checked;
use cove_sema::typeck::Ty;

use crate::layout::{enum_layout, struct_layout, Layout, LayoutId, Shape};
use crate::repr::Repr;

/// The layout every string object shares.
pub(super) const STR: LayoutId = LayoutId(1);
pub(super) const UNIT: LayoutId = LayoutId(2);
pub(super) const BOOL: LayoutId = LayoutId(3);
pub(super) const INT: LayoutId = LayoutId(4);
pub(super) const FLOAT: LayoutId = LayoutId(5);
pub(super) const DURATION: LayoutId = LayoutId(6);
/// One word that is a reference, with nothing said about what it names.
///
/// It is what a payload word of an enum is zeroed as, and what a location
/// whose family this lowering never had to name is described by. A boundary
/// never meets it: every value's own layout is one of the ones above or one
/// built from a declaration.
pub(super) const REF: LayoutId = LayoutId(7);
/// One word that is the address of a value location: a `var` parameter.
pub(super) const ADDR: LayoutId = LayoutId(8);

/// The layout a scalar word of this `Repr` has.
pub(super) fn scalar(repr: Repr) -> LayoutId {
    match repr {
        Repr::Unit => UNIT,
        Repr::Bool => BOOL,
        Repr::Int => INT,
        Repr::Float => FLOAT,
        Repr::Duration => DURATION,
        Repr::Ref => REF,
        Repr::Addr => ADDR,
        Repr::Host => HOST,
    }
}

/// An index into the run's host resource table.
pub(super) const HOST: LayoutId = LayoutId(9);

/// The object every `Box` allocates.
///
/// One shape for every box, because what a box holds is named by its own
/// first payload word rather than by its object layout. Reserved rather than
/// interned on demand so that a program that boxes nothing still declares
/// it: the machine allocates one for a host's answer, and a table it has to
/// search first is a search that has to answer something when it fails.
pub(super) const BOXED: LayoutId = LayoutId(10);

/// Payload word 0 of a [`Shape::Vector`] object: how many elements it holds.
pub(super) const VECTOR_LEN: u32 = 0;

/// Payload word 1 of a [`Shape::Vector`] object: the [`Shape::Elements`]
/// object holding them.
pub(super) const VECTOR_STORE: u32 = 1;

/// The word a `Range` holds its first value in.
pub(super) const RANGE_START: u32 = 0;

/// The word a `Range` holds its written end in — the last value it yields
/// when it is inclusive, and the first one past it when it is not.
pub(super) const RANGE_END: u32 = 1;

/// The word that says which of the two [`RANGE_END`] is.
pub(super) const RANGE_INCLUSIVE: u32 = 2;

/// The program's layout table, being built.
pub(super) struct Shapes {
    layouts: Vec<Layout>,
    /// The nominal declarations a call to [`Shapes::of`] is part way
    /// through, innermost last. An occurrence of one of these inside its own
    /// layout is the cycle, and the place it is broken.
    building: Vec<String>,
    /// The boxed layout standing in for each recursive declaration, and the
    /// inline layout the box holds once it has been built.
    boxed: HashMap<String, (LayoutId, Option<LayoutId>)>,
}

impl Shapes {
    /// The layouts every program declares whether or not it uses them.
    ///
    /// `LayoutId(0)` is what the sweeper writes into a reclaimed run of
    /// words; the string layout is what the machine allocates a host's
    /// answer as; and the scalars are there because a one-word value is the
    /// width-one case of the model rather than a family of its own, so
    /// naming one should not depend on a program having mentioned it.
    pub(super) fn new() -> Shapes {
        let layouts = vec![
            Layout::free(),
            Layout::object("String", Shape::Str),
            Layout::word("Unit", Repr::Unit),
            Layout::word("Bool", Repr::Bool),
            Layout::word("Int", Repr::Int),
            Layout::word("Float", Repr::Float),
            Layout::word("Duration", Repr::Duration),
            Layout::word("<ref>", Repr::Ref),
            Layout::word("<addr>", Repr::Addr),
            Layout::word("<host>", Repr::Host),
            Layout::object("Any", Shape::Boxed),
        ];
        Shapes {
            layouts,
            building: Vec::new(),
            boxed: HashMap::new(),
        }
    }

    pub(super) fn into_table(self) -> Vec<Layout> {
        self.layouts
    }

    pub(super) fn layout(&self, id: LayoutId) -> &Layout {
        &self.layouts[id.index()]
    }

    /// The words a value of `id` occupies.
    pub(super) fn words(&self, id: LayoutId) -> &[Repr] {
        &self.layouts[id.index()].words
    }

    pub(super) fn width(&self, id: LayoutId) -> u32 {
        self.layouts[id.index()].width()
    }

    /// Whether a location of this layout holds anything a collection would
    /// trace, or an address whose live range the lowering ends.
    pub(super) fn holds_ref(&self, id: LayoutId) -> bool {
        self.words(id)
            .iter()
            .any(|repr| matches!(repr, Repr::Ref | Repr::Addr))
    }

    /// What a [`Shape::Boxed`] layout built for a recursive declaration
    /// holds, inline.
    ///
    /// `None` for the `Boxed` layout an *erased* value uses: what a `dyn`
    /// box holds is a question the box answers at run time, and that is the
    /// whole difference between erasure and a broken cycle. A recursive
    /// declaration's box holds one known layout, and this is it.
    pub(super) fn unboxed(&self, id: LayoutId) -> Option<LayoutId> {
        self.boxed
            .values()
            .find(|(boxed, _)| *boxed == id)
            .and_then(|(_, inline)| *inline)
    }

    /// The id of a layout, adding it only if the table does not hold it.
    ///
    /// A linear scan rather than a hash: a program has a handful of shapes
    /// where it has thousands of expressions, and a [`Layout`] is a
    /// structure to compare rather than a key to hash.
    fn intern(&mut self, layout: Layout) -> LayoutId {
        match self.layouts.iter().position(|held| *held == layout) {
            Some(at) => LayoutId(at as u32),
            None => {
                self.layouts.push(layout);
                LayoutId((self.layouts.len() - 1) as u32)
            }
        }
    }

    /// The layout of a value of `ty`, read as the module `module` reads the
    /// names in it.
    ///
    /// `None` where the lowering has not been taught the type. Every caller
    /// turns that into a gap naming the type, so the reason a program stops
    /// is written where the type is rather than here.
    pub(super) fn of(&mut self, checked: &Checked, module: &str, ty: &Ty) -> Option<LayoutId> {
        match ty {
            // A value of `Never` is never produced — the expression left the
            // frame or the loop before it could be — so the location exists
            // to keep the numbering uniform and nothing ever writes it.
            Ty::Unit | Ty::Never => Some(UNIT),
            Ty::Bool => Some(BOOL),
            Ty::Int => Some(INT),
            Ty::Float => Some(FLOAT),
            Ty::Duration => Some(DURATION),
            Ty::Str => Some(STR),
            // One `Boxed` layout for the whole program, whatever trait was
            // written: what is inside is a question the box answers, from
            // the `LayoutId` in its own payload word 0. A layout per trait
            // would be a runtime type universe keyed by a static name.
            Ty::Dyn(_) => Some(BOXED),
            Ty::Error => {
                let (fields, words) = struct_layout(&[(Arc::from("message"), STR)], &self.layouts);
                Some(self.intern(Layout::inline(
                    "Error",
                    Shape::Struct {
                        fields,
                        opaque: false,
                    },
                    words,
                )))
            }
            Ty::Range => {
                let declared = [
                    (Arc::from("start"), INT),
                    (Arc::from("end"), INT),
                    (Arc::from("inclusive"), BOOL),
                ];
                let (fields, words) = struct_layout(&declared, &self.layouts);
                Some(self.intern(Layout::inline(
                    "Range",
                    Shape::Struct {
                        fields,
                        opaque: false,
                    },
                    words,
                )))
            }
            Ty::Array(elem) => {
                let elem = self.of(checked, module, elem)?;
                Some(self.intern(Layout::object(
                    "Array",
                    Shape::Elements {
                        elem,
                        growable: false,
                    },
                )))
            }
            // A vector is two layouts, and both are declared here because
            // the machine has to find the second without being told:
            // growing replaces the store beneath a header that stays where
            // it is, and the only thing that says what a new store looks
            // like is this table.
            Ty::Vector(elem) => {
                let elem = self.of(checked, module, elem)?;
                self.store_of(elem);
                Some(self.intern(Layout::object("Vector", Shape::Vector { elem })))
            }
            Ty::Option(some) => {
                let some = self.of(checked, module, some)?;
                self.enum_of(
                    "Option",
                    &[
                        (Arc::from("None"), Vec::new()),
                        (Arc::from("Some"), vec![some]),
                    ],
                )
            }
            Ty::Result(ok, err) => {
                let ok = self.of(checked, module, ok)?;
                let err = self.of(checked, module, err)?;
                self.enum_of(
                    "Result",
                    &[(Arc::from("Ok"), vec![ok]), (Arc::from("Err"), vec![err])],
                )
            }
            Ty::Struct(name, args) if args.is_empty() => {
                self.declared_struct(checked, module, name)
            }
            Ty::Enum(name, args) if args.is_empty() => self.declared_enum(checked, module, name),
            _ => None,
        }
    }

    /// The one box an intentionally erased value occupies.
    ///
    /// A host schema that declared its result `Any` is the other side of the
    /// same coin as `dyn Trait`: from that call onwards the program holds a
    /// value no schema described, so it carries its own layout rather than
    /// being a run of words the frame claims to know the shape of.
    pub(super) fn any(&mut self) -> LayoutId {
        BOXED
    }

    /// The layout of the run of words a [`Shape::Vector`] over `elem` keeps
    /// its elements in.
    ///
    /// It is the same [`Shape::Elements`] an `Array` is, with `growable`
    /// set: one shape covers both, and the flag is what a reader consults to
    /// tell an array from the store beneath a vector.
    pub(super) fn store_of(&mut self, elem: LayoutId) -> LayoutId {
        self.intern(Layout::object(
            "Vector",
            Shape::Elements {
                elem,
                growable: true,
            },
        ))
    }

    /// The box that breaks the cycle a declaration's layout contains.
    ///
    /// It is created the first time the declaration is met inside its own
    /// layout, and answered for *every* mention of the type afterwards — the
    /// top-level ones too. `docs/LINEAR_VM.md`'s table says a recursive
    /// layout is one word holding a heap address, and a type that were one
    /// word inside itself and several words outside would be two
    /// representations of one type, which is the thing a boundary and a
    /// copy both have to agree about.
    fn box_for(&mut self, key: &str) -> LayoutId {
        if let Some((boxed, _)) = self.boxed.get(key) {
            return *boxed;
        }
        let boxed = self.intern(Layout::object(format!("box {key}"), Shape::Boxed));
        self.boxed.insert(key.to_string(), (boxed, None));
        boxed
    }

    fn enum_of(&mut self, name: &str, cases: &[(Arc<str>, Vec<LayoutId>)]) -> Option<LayoutId> {
        let (cases, payload) = enum_layout(cases, &self.layouts);
        let mut words = Vec::with_capacity(1 + payload.len());
        words.push(Repr::Int);
        words.extend_from_slice(&payload);
        Some(self.intern(Layout::inline(name, Shape::Enum { cases, payload }, words)))
    }

    fn declared_struct(&mut self, checked: &Checked, module: &str, name: &str) -> Option<LayoutId> {
        let (owner, short) = declaring(checked, module, name)?;
        let key = format!("{owner}.{short}");
        if self.building.contains(&key) {
            return Some(self.box_for(&key));
        }
        let declared = struct_fields(checked, module, &Ty::Struct(Arc::from(name), Vec::new()))?;
        self.building.push(key.clone());
        let mut placed = Vec::with_capacity(declared.len());
        for (field, ty) in &declared {
            match self.of(checked, module, ty) {
                Some(id) => placed.push((field.clone(), id)),
                None => {
                    self.building.pop();
                    return None;
                }
            }
        }
        self.building.pop();
        let (fields, words) = struct_layout(&placed, &self.layouts);
        let opaque = struct_is_opaque(checked, module, name);
        let id = self.intern(Layout::inline(
            key.clone(),
            Shape::Struct { fields, opaque },
            words,
        ));
        Some(self.record_box(&key, id))
    }

    fn declared_enum(&mut self, checked: &Checked, module: &str, name: &str) -> Option<LayoutId> {
        let (owner, short) = declaring(checked, module, name)?;
        let key = format!("{owner}.{short}");
        if self.building.contains(&key) {
            return Some(self.box_for(&key));
        }
        let declared = enum_cases(checked, module, &Ty::Enum(Arc::from(name), Vec::new()))?;
        self.building.push(key.clone());
        let mut placed = Vec::with_capacity(declared.len());
        for (case, types) in &declared {
            let mut parts = Vec::with_capacity(types.len());
            for ty in types {
                match self.of(checked, module, ty) {
                    Some(id) => parts.push(id),
                    None => {
                        self.building.pop();
                        return None;
                    }
                }
            }
            placed.push((case.clone(), parts));
        }
        self.building.pop();
        let id = self.enum_of(&key, &placed)?;
        Some(self.record_box(&key, id))
    }

    /// Records what the box built for `key` holds, once the inline layout it
    /// stands in for exists, and answers what a mention of the type is.
    ///
    /// A declaration whose layout did not contain itself is its own inline
    /// words. One that did is the box: nothing about a `Point` changes
    /// because a `Node` exists, and everything about a `Node` is decided
    /// once.
    fn record_box(&mut self, key: &str, inline: LayoutId) -> LayoutId {
        match self.boxed.get_mut(key) {
            Some(entry) => {
                entry.1 = Some(inline);
                entry.0
            }
            None => inline,
        }
    }
}

/// The module that declares `name`, and the name within it.
///
/// A type the checker settled carries either a bare name — one this module
/// declares — or a key another module's declaration is known by. Both are
/// answered here so that no caller has to know which it is holding.
pub(super) fn declaring(checked: &Checked, module: &str, name: &str) -> Option<(String, String)> {
    if let Some((owner, short)) = name.rsplit_once('.') {
        return Some((owner.to_string(), short.to_string()));
    }
    let resolved = checked.modules.get(module)?;
    let owner = resolved.owner_of(name)?;
    Some((owner.to_string(), name.to_string()))
}

/// A struct-shaped type's fields, in declaration order.
///
/// `Error` is a struct like any other here: the language declares it with
/// one `message: String`, and the alternative — a shape of its own — would
/// be a second description of the same object.
pub(super) fn struct_fields(
    checked: &Checked,
    module: &str,
    ty: &Ty,
) -> Option<Vec<(Arc<str>, Ty)>> {
    match ty {
        Ty::Error => Some(vec![(Arc::from("message"), Ty::Str)]),
        Ty::Struct(name, args) if args.is_empty() => {
            let (owner, short) = declaring(checked, module, name)?;
            let entry = checked.modules.get(&owner)?.structs.get(&short)?;
            let signature = checked
                .facts
                .signature(entry.decl.span.file, entry.decl.span)?;
            Some(
                entry
                    .decl
                    .fields
                    .iter()
                    .zip(&signature.params)
                    .map(|(field, ty)| (Arc::from(field.name.node.as_str()), ty.clone()))
                    .collect(),
            )
        }
        _ => None,
    }
}

/// Whether `name` was declared `export opaque struct`.
fn struct_is_opaque(checked: &Checked, module: &str, name: &str) -> bool {
    let Some((owner, short)) = declaring(checked, module, name) else {
        return false;
    };
    checked
        .modules
        .get(&owner)
        .and_then(|resolved| resolved.structs.get(&short))
        .is_some_and(|entry| entry.opaque)
}

/// An enum-shaped type's cases, in the order the case index counts them.
///
/// `Option` is `None` then `Some`, and `Result` is `Ok` then `Err`, which is
/// the order `docs/LINEAR_VM.md` fixes. A declared enum's order is its
/// declaration's.
pub(super) fn enum_cases(
    checked: &Checked,
    module: &str,
    ty: &Ty,
) -> Option<Vec<(Arc<str>, Vec<Ty>)>> {
    match ty {
        Ty::Option(some) => Some(vec![
            (Arc::from("None"), Vec::new()),
            (Arc::from("Some"), vec![(**some).clone()]),
        ]),
        Ty::Result(ok, err) => Some(vec![
            (Arc::from("Ok"), vec![(**ok).clone()]),
            (Arc::from("Err"), vec![(**err).clone()]),
        ]),
        Ty::Enum(name, args) if args.is_empty() => {
            let (owner, short) = declaring(checked, module, name)?;
            let entry = checked.modules.get(&owner)?.enums.get(&short)?;
            let mut cases = Vec::with_capacity(entry.decl.cases.len());
            for case in &entry.decl.cases {
                let signature = checked.facts.signature(case.span.file, case.span)?;
                cases.push((Arc::from(case.name.node.as_str()), signature.params.clone()));
            }
            Some(cases)
        }
        _ => None,
    }
}

/// A case's index and the types of its payload, if `ty` has a case `name`.
pub(super) fn case_at(
    checked: &Checked,
    module: &str,
    ty: &Ty,
    name: &str,
) -> Option<(u32, Vec<Ty>)> {
    let cases = enum_cases(checked, module, ty)?;
    cases
        .into_iter()
        .enumerate()
        .find(|(_, (case, _))| &**case == name)
        .map(|(at, (_, payload))| (at as u32, payload))
}
