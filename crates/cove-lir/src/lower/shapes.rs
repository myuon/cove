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
        Ty::Str | Ty::Error | Ty::Option(_) | Ty::Result(..) => Some(Repr::Ref),
        Ty::Struct(_, args) | Ty::Enum(_, args) if args.is_empty() => Some(Repr::Ref),
        _ => None,
    }
}

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
                    shape: Shape::Struct { fields },
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
        Ty::Struct(name, args) | Ty::Enum(name, args) if args.is_empty() => {
            let (_, short) = declaring(checked, module, name)?;
            Arc::from(short)
        }
        _ => return None,
    })
}

/// The module that declares `name`, and the name within it.
///
/// A type the checker settled carries either a bare name — one this module
/// declares — or a key another module's declaration is known by. Both are
/// answered here so that no caller has to know which it is holding.
fn declaring(checked: &Checked, module: &str, name: &str) -> Option<(String, String)> {
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
