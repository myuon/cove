//! What a type is once it is words and objects.
//!
//! Two questions are asked of a type here and nowhere else: which one word a
//! value of it occupies, and — when that word is a reference — what the
//! object it names is made of. Everything that allocates, reads a field,
//! reads a case index or names a payload goes through this module, so a
//! struct's field order and an enum's case order are decided once.
//!
//! # A layout describes a family
//!
//! `docs/LINEAR_VM.md` fixes the table: `Array<String>` and `Array<Point>`
//! are one layout because a reference is a reference, and `Array<Int>` and
//! `Array<Duration>` are two because their [`Repr`]s differ and the boundary
//! has to know which. The same rule is why `Option<Int>` and `Option<Float>`
//! are two layouts and `Option<String>` and `Option<Point>` are one.
//!
//! [`Shapes`] interns them, so the same shape is the same [`LayoutId`]
//! however many times the source writes it, and a program's layout table is
//! as long as the shapes it actually holds rather than as long as its types.
//!
//! # It reads the checker's answers
//!
//! A struct's fields and an enum's cases are read from the [`Signature`] the
//! checker recorded for the declaration — for a struct, its fields in
//! declaration order; for an enum, one record per case holding that case's
//! payload types. Nothing here re-resolves an annotation, because an
//! annotation is a name and only the checker knows what the name meant in
//! the module it was written in.

use std::sync::Arc;

use cove_sema::resolve::Program as Checked;
use cove_sema::typeck::Ty;

use crate::layout::{Case, Field, Layout, LayoutId, Shape};
use crate::repr::Repr;

/// The layout every string object shares.
///
/// It is `LayoutId(1)` in every program: `LayoutId(0)` is what the sweeper
/// writes into a reclaimed run of words, and the string layout is declared
/// next whether or not the program mentions a string, because the machine
/// allocates a host's answer as one.
pub(super) const STR: LayoutId = LayoutId(1);

/// The one word a value of this type occupies.
///
/// [`Ty::Never`] answers a word too, and it is `Unit`. A value of that type
/// is never produced — the expression left the frame or the loop before it
/// could be — so the slot exists to keep the numbering uniform and nothing
/// ever writes it.
///
/// Every compound value is one [`Repr::Ref`]: what it *is* is a question its
/// own object answers from its own header, which is what keeps the frame's
/// reference map one bit per slot. A generic instantiation answers `None`
/// rather than `Ref`, because the lowering has not been taught generics and
/// a layout it cannot build is a gap rather than a reference to nothing.
pub(super) fn word_of(ty: &Ty) -> Option<Repr> {
    match ty {
        Ty::Unit | Ty::Never => Some(Repr::Unit),
        Ty::Bool => Some(Repr::Bool),
        Ty::Int => Some(Repr::Int),
        Ty::Float => Some(Repr::Float),
        Ty::Duration => Some(Repr::Duration),
        Ty::Str | Ty::Error | Ty::Option(_) | Ty::Result(..) | Ty::Range => Some(Repr::Ref),
        // A sequence is one reference whatever it holds, but its element's
        // word is what its layout is keyed by — `Array<Int>` and
        // `Array<Duration>` are two families — so an element with no word is
        // a sequence this lowering cannot describe rather than a reference to
        // an object it could not build.
        Ty::Array(elem) | Ty::Vector(elem) => word_of(elem).map(|_| Repr::Ref),
        Ty::Struct(_, args) | Ty::Enum(_, args) if args.is_empty() => Some(Repr::Ref),
        // A `dyn Trait` is erased on purpose, and erasure is a reference to
        // an object carrying its own description. That is the one thing
        // `docs/LINEAR_VM.md` separates from a `Ty::Unknown`, which is the
        // checker declining and has no word at all.
        Ty::Dyn(_) => Some(Repr::Ref),
        _ => None,
    }
}

/// Payload word 0 of a [`Shape::Vector`] object: how many elements it holds.
///
/// The count is in the header rather than in the store, because a store is as
/// long as the last growth made it and the elements past the count are spare
/// room rather than value.
pub(super) const VECTOR_LEN: u32 = 0;

/// Payload word 1 of a [`Shape::Vector`] object: the
/// [`Shape::Elements`] object holding them.
pub(super) const VECTOR_STORE: u32 = 1;

/// The fields of the one struct-shaped layout a `Range` is.
///
/// `docs/LINEAR_VM.md` fixes it as `Struct { start: Int, end: Int,
/// inclusive: Bool }`, one layout for the program: `..` and `..<` are two
/// ways of writing one value, and which one a range was written with is a
/// word of the object rather than two families.
fn range_fields() -> Vec<Field> {
    vec![
        Field {
            name: Arc::from("start"),
            repr: Repr::Int,
        },
        Field {
            name: Arc::from("end"),
            repr: Repr::Int,
        },
        Field {
            name: Arc::from("inclusive"),
            repr: Repr::Bool,
        },
    ]
}

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
}

impl Shapes {
    /// The two layouts every program declares whether or not it uses them.
    ///
    /// `LayoutId(0)` is what the sweeper writes into a reclaimed run of
    /// words, and the string layout is what the machine allocates a host's
    /// answer as. A scalar-only program names neither.
    pub(super) fn new() -> Shapes {
        Shapes {
            layouts: vec![
                Layout::free(),
                Layout {
                    name: Arc::from("String"),
                    shape: Shape::Str,
                },
            ],
        }
    }

    pub(super) fn into_table(self) -> Vec<Layout> {
        self.layouts
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

    /// The layout of the objects a value of `ty` names, read as the module
    /// `module` reads the names in it.
    ///
    /// `None` where the lowering has not been taught the type. Every caller
    /// turns that into a gap naming the type, so the reason a program stops
    /// is written where the type is rather than here.
    pub(super) fn of(&mut self, checked: &Checked, module: &str, ty: &Ty) -> Option<LayoutId> {
        let name = nominal(checked, module, ty)?;
        match ty {
            Ty::Str => Some(STR),
            // One `Boxed` layout for the whole program, whatever trait was
            // written: what is inside is a question the box answers, from
            // the `Repr` tag in its own payload word 0. A layout per trait
            // would be a runtime type universe keyed by a static name, which
            // is exactly the table a family-shaped layout exists to avoid.
            Ty::Dyn(_) => Some(self.intern(Layout {
                name,
                shape: Shape::Boxed,
            })),
            Ty::Error | Ty::Struct(..) => {
                let declared = struct_fields(checked, module, ty)?;
                let mut fields = Vec::with_capacity(declared.len());
                for (name, ty) in declared {
                    fields.push(Field {
                        name,
                        repr: word_of(&ty)?,
                    });
                }
                Some(self.intern(Layout {
                    name,
                    shape: Shape::Struct {
                        fields,
                        opaque: struct_is_opaque(checked, module, ty),
                    },
                }))
            }
            Ty::Range => Some(self.intern(Layout {
                name,
                shape: Shape::Struct {
                    fields: range_fields(),
                    opaque: false,
                },
            })),
            Ty::Array(elem) => {
                let elem = word_of(elem)?;
                Some(self.intern(Layout {
                    name,
                    shape: Shape::Elements {
                        elem,
                        growable: false,
                    },
                }))
            }
            // A vector is two layouts, and both are declared here because the
            // machine has to find the second without being told: growing
            // replaces the store beneath a header that stays where it is, and
            // the only thing that says what a new store looks like is this
            // table. Interning the store beside the header is what makes
            // `Shape::Vector { elem }` enough to name it.
            Ty::Vector(elem) => {
                let elem = word_of(elem)?;
                self.store_of(elem);
                Some(self.intern(Layout {
                    name,
                    shape: Shape::Vector { elem },
                }))
            }
            Ty::Option(_) | Ty::Result(..) | Ty::Enum(..) => {
                let declared = enum_cases(checked, module, ty)?;
                let mut cases = Vec::with_capacity(declared.len());
                for (name, types) in declared {
                    let mut payload = Vec::with_capacity(types.len());
                    for ty in types {
                        payload.push(word_of(&ty)?);
                    }
                    cases.push(Case { name, payload });
                }
                Some(self.intern(Layout {
                    name,
                    shape: Shape::Enum { cases },
                }))
            }
            _ => None,
        }
    }

    /// The layout of the run of words a [`Shape::Vector`] over `elem` keeps
    /// its elements in.
    ///
    /// It is the same [`Shape::Elements`] an `Array` is, with `growable` set:
    /// one shape covers both, and the flag is what a reader consults to tell
    /// an array from the store beneath a vector. It is named `Vector` because
    /// a store that reached a boundary would be shown as the value it belongs
    /// to.
    pub(super) fn store_of(&mut self, elem: Repr) -> LayoutId {
        self.intern(Layout {
            name: Arc::from("Vector"),
            shape: Shape::Elements {
                elem,
                growable: true,
            },
        })
    }
}

/// What a boundary calls an object of this type.
///
/// It is the type's own name, without the module that declares it, because
/// that is what a value of it is *shown* as: `Display for Value` writes the
/// last segment of a struct's qualified type name, and
/// `cove_runtime::lvm::boundary` matches an incoming value to a family by the
/// same last segment. A qualified name here would render one way on this
/// backend and another on the oracle, and would match nothing coming in.
///
/// What it costs is that two modules each declaring a `Point` with the same
/// field words are one layout. Telling them apart needs the layout table to
/// carry the declaring module and both readers above to ask for it, which is
/// a change on the other side of the boundary from this one.
fn nominal(checked: &Checked, module: &str, ty: &Ty) -> Option<Arc<str>> {
    Some(match ty {
        Ty::Str => Arc::from("String"),
        Ty::Error => Arc::from("Error"),
        Ty::Option(_) => Arc::from("Option"),
        Ty::Result(..) => Arc::from("Result"),
        Ty::Range => Arc::from("Range"),
        Ty::Array(_) => Arc::from("Array"),
        Ty::Vector(_) => Arc::from("Vector"),
        // Not the trait's name: one layout describes every erased value, and
        // naming it after one trait would say a value of another trait is of
        // a family it is not.
        Ty::Dyn(_) => Arc::from("Dyn"),
        // Qualified, and that is what makes a layout an identity rather than
        // a shape. Two modules may each declare `struct Point { x: Int }`,
        // and interning them together would be saying they are one type: a
        // `dyn` dispatch would then reach the wrong conformance, and a
        // `Map` would order them the way it orders one type with itself.
        //
        // This is also the name the oracle carries — `StructValue::type_name`
        // is qualified — so the boundary can match a value to a layout
        // exactly rather than by a name two types can share. A rendering
        // shortens it, which is what `Display for Value` does with the same
        // string.
        Ty::Struct(name, args) | Ty::Enum(name, args) if args.is_empty() => {
            let (owner, short) = declaring(checked, module, name)?;
            Arc::from(format!("{owner}.{short}"))
        }
        _ => return None,
    })
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

/// Whether `ty` was declared `export opaque struct`.
///
/// A type this cannot find an entry for is not opaque, which is the safe
/// reading for the one thing that consults it: a rendering. `Error` is a
/// builtin and never opaque, and a rendering of it has its own rule.
fn struct_is_opaque(checked: &Checked, module: &str, ty: &Ty) -> bool {
    let Ty::Struct(name, _) = ty else {
        return false;
    };
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

/// Where a field sits in a struct-shaped object's payload, and what it holds.
pub(super) fn field_at(checked: &Checked, module: &str, ty: &Ty, name: &str) -> Option<(u32, Ty)> {
    let fields = struct_fields(checked, module, ty)?;
    fields
        .into_iter()
        .enumerate()
        .find(|(_, (field, _))| &**field == name)
        .map(|(at, (_, ty))| (at as u32, ty))
}

/// A case's index and the types of its payload words.
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
