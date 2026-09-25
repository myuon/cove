//! Which kind of Host resource each handle in a key is: the part of a key's
//! type the layout table forgets, carried to the walk that words its refusal.
//!
//! A refusal names a value by its type, and every evaluator names a Host
//! resource by its qualified type — `http.Server` — as the oracle's
//! `MapKey::convert` always has (issue #506). A walk
//! [`super::synth`] composes for a known layout knows every other type it can
//! refuse from the layout alone: a `Float`, a `Vector`, a `Task`, a
//! `TaskScope`, a function, a `Shared` cell each have a layout of their own.
//! A resource does not. Every resource is the one [`super::shapes::HOST`]
//! word, because a handle is an index into the run's resource table whatever
//! it names, and the layouts that hold one are shared the same way:
//! `Array<http.Server>` and `Array<files.Reader>` are one `Array` layout.
//!
//! The type is not lost where the key is admitted, though. The call site has
//! the key's [`Ty`], and the checker's declarations say the type of every
//! part of it — a struct's fields, a case's parts, an array's elements, a
//! map's values — so the resource at each position of a key is known before
//! the run. This module reads it off and hands it to the wording walk as a
//! [`NamedId`]: a node for one type, holding the resource it is, if it is
//! one, and a node for each part the wording walk visits, in the walk's
//! order. The walk emits the name as a literal, like every other word of a
//! refusal composed for a known layout, and never asks the run's resource
//! table.
//!
//! # Only where a resource is
//!
//! A type that holds no resource anywhere has no node: [`Naming::of`] answers
//! `None`, and the walk for it is the one keyed by its layout alone, exactly
//! as it was before this module existed. So a program whose keys hold no
//! resource — every program in the repository but the ones that pin this —
//! composes the same walks, instruction for instruction.
//!
//! A node is interned by its type and the module it is read in, so a type
//! that holds itself is a finite graph and a walk that meets itself again
//! meets the same node, which is what lets its call of itself terminate.

use std::sync::Arc;

use cove_sema::resolve::Program as Checked;
use cove_sema::typeck::Ty;

use super::shapes::{self, Shapes};

/// A node of a [`Naming`]: one type a key or a part of one has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct NamedId(u32);

impl NamedId {
    /// The position of the node, which a walk's name carries.
    pub(super) fn index(self) -> usize {
        self.0 as usize
    }
}

/// What one type says about the resources in a value of it.
#[derive(Clone, Debug)]
struct Named {
    /// The qualified type, where this type is a Host resource.
    resource: Option<Arc<str>>,
    /// The node of each part a wording walk visits, in its order: a struct's
    /// fields, every case's parts one case after another, an array's element
    /// and a map's value.
    parts: Vec<NamedId>,
}

/// Every node the program's key admissions have asked for.
#[derive(Default)]
pub(super) struct Naming {
    nodes: Vec<Named>,
    /// The type and module each node was made for, by position.
    made_for: Vec<(String, Ty)>,
    /// Whether a resource is reachable from each node, by position: settled
    /// after every [`Naming::of`], over the whole graph.
    holds: Vec<bool>,
}

/// The most nodes one program's keys may make: past it a type is growing
/// rather than recurring, and the key is not named here at all. A type that
/// grows is one the layout table could not have finished either.
const MOST: usize = 4096;

impl Naming {
    /// The node for a key of `ty`, read in `module`, where a Host resource is
    /// reachable from it; `None` where none is, or where the type could not
    /// be followed.
    pub(super) fn of(
        &mut self,
        checked: &Checked,
        shapes: &Shapes,
        module: &str,
        ty: &Ty,
    ) -> Option<NamedId> {
        let root = self.node(checked, shapes, module, ty)?;
        self.settle();
        self.holds[root.0 as usize].then_some(root)
    }

    /// The node of part `at` of `named`, where a resource is reachable from
    /// it.
    pub(super) fn part(&self, named: Option<NamedId>, at: usize) -> Option<NamedId> {
        let part = *self.nodes[named?.0 as usize].parts.get(at)?;
        self.holds[part.0 as usize].then_some(part)
    }

    /// The qualified type of the resource `named` is, if it is one.
    pub(super) fn resource(&self, named: Option<NamedId>) -> Option<&str> {
        self.nodes[named?.0 as usize].resource.as_deref()
    }

    /// The node for `ty` in `module`, made with every node below it if it is
    /// new.
    fn node(
        &mut self,
        checked: &Checked,
        shapes: &Shapes,
        module: &str,
        ty: &Ty,
    ) -> Option<NamedId> {
        if let Some(at) = self
            .made_for
            .iter()
            .position(|(held, made)| held == module && made == ty)
        {
            return Some(NamedId(at as u32));
        }
        if self.nodes.len() >= MOST {
            return None;
        }
        // Reserved before the parts are made, so that a type that holds
        // itself finds this node rather than making another.
        let id = NamedId(self.nodes.len() as u32);
        let resource = match ty {
            Ty::Host(qualified)
                if qualified
                    .rsplit_once('.')
                    .is_some_and(|(host, kind)| shapes.is_resource(host, kind)) =>
            {
                Some(qualified.clone())
            }
            _ => None,
        };
        self.nodes.push(Named {
            resource,
            parts: Vec::new(),
        });
        self.made_for.push((module.to_string(), ty.clone()));
        self.holds.push(false);
        let mut parts = Vec::new();
        for (owner, part) in parts_of(checked, module, ty)? {
            parts.push(self.node(checked, shapes, &owner, &part)?);
        }
        self.nodes[id.0 as usize].parts = parts;
        Some(id)
    }

    /// Settles [`Naming::holds`] for every node: a node holds a resource
    /// where it is one or a part of it holds one. A fixed point, because the
    /// graph can have cycles.
    fn settle(&mut self) {
        loop {
            let mut changed = false;
            for at in 0..self.nodes.len() {
                if self.holds[at] {
                    continue;
                }
                let named = &self.nodes[at];
                if named.resource.is_some()
                    || named.parts.iter().any(|part| self.holds[part.0 as usize])
                {
                    self.holds[at] = true;
                    changed = true;
                }
            }
            if !changed {
                return;
            }
        }
    }
}

/// The types of the parts a wording walk of a value of `ty` visits, in its
/// order, each with the module it is read in: [`Shapes::of`]'s reading of
/// the same type, which is what the layout the walk is composed from was
/// built from, so the two agree part for part.
///
/// A family whose parts the walk never visits — a set, whose members were
/// admitted when they were put there, and a map's keys — and a family that is
/// refused whole or admitted whole has none. `None` where a declaration
/// cannot be read, which the layout table would have refused first.
fn parts_of(checked: &Checked, module: &str, ty: &Ty) -> Option<Vec<(String, Ty)>> {
    let here = |ty: &Ty| (module.to_string(), ty.clone());
    Some(match ty {
        Ty::Struct(name, args) => {
            let (owner, _) = shapes::declaring(checked, module, name)?;
            let args = shapes::qualify_all(checked, module, args);
            shapes::struct_fields(checked, module, &Ty::Struct(name.clone(), args))?
                .into_iter()
                .map(|(_, part)| (owner.clone(), part))
                .collect()
        }
        Ty::Error | Ty::MapEntry(..) => shapes::struct_fields(checked, module, ty)?
            .iter()
            .map(|(_, part)| here(part))
            .collect(),
        Ty::Enum(name, args) => {
            let (owner, _) = shapes::declaring(checked, module, name)?;
            let args = shapes::qualify_all(checked, module, args);
            shapes::enum_cases(checked, module, &Ty::Enum(name.clone(), args))?
                .into_iter()
                .flat_map(|(_, parts)| parts)
                .map(|part| (owner.clone(), part))
                .collect()
        }
        Ty::Option(_) | Ty::Result(..) => shapes::enum_cases(checked, module, ty)?
            .iter()
            .flat_map(|(_, parts)| parts.iter().map(here))
            .collect(),
        Ty::Array(elem) => vec![here(elem)],
        Ty::Map(_, value) => vec![here(value)],
        _ => Vec::new(),
    })
}
