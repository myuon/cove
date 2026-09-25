//! The names a rendering of an erased value shows, placed before the run.
//!
//! [ADR 0068](../../../../docs/adr/0068-a-dynamic-value-is-inspected-in-cove-not-walked-in-rust.md)'s
//! Phase 4b-ii renders a box in Cove, over a view of it, and a rendering
//! writes three things no value's words hold: a struct's name, its fields'
//! names and an enum's case name. They are the layout table's. Issue #499's
//! decision 4 (option N1) is that they reach Cove as **literals**: a `String`
//! the machine places before the first instruction, as it places every
//! [`Inst::Str`]'s, so that [`Inst::DynTypeName`], [`Inst::DynFieldName`] and
//! [`Inst::DynCaseName`] are each one load of an address and a rendering
//! allocates nothing per node.
//!
//! This pass decides which layouts get names, and writes them into
//! [`Program::strings`] and [`Program::names`].
//!
//! # Only the layouts a box can hold
//!
//! A literal is paid for once per run whether or not it is read, so a name
//! nothing can ask for is words of heap for nothing — and most programs box
//! nothing and ask for none. The set is therefore **exact** rather than
//! generous, and a name read that was never placed is an internal error
//! rather than a fallback.
//!
//! A value reaches a box in one of two ways, and they are answered apart:
//!
//! - **An [`Inst::Box`]** names the layout of what it boxes. Everything a
//!   reflection can see inside it is reachable from that layout through the
//!   parts the layout table records — a struct's fields, every case's parts,
//!   a run's elements, a map's keys and values — so the names are placed for
//!   exactly that closure. A box inside it was boxed by an instruction of its
//!   own, which is a root of its own; a `Shared` cell and a closure render as
//!   `<shared>` and `<fn>` and are not looked inside.
//! - **A Host operation that answers `Any`.** The boundary boxes the host's
//!   value at a layout it finds by searching the whole table
//!   (`cove-runtime`'s `boundary::held_layout`), so which layouts it can
//!   choose is not something the program's code says. A program with such an
//!   operation places names for **every** nominal layout it has: a sound
//!   over-approximation, paid only where `Any` crosses in — today
//!   `clock.timeout`'s `Result<Any, Error>`.
//!
//! A program that does neither places nothing, and its literal heap is what
//! it was before this pass existed. `covefmt` and `cq` are two such programs.
//!
//! # What is placed for each
//!
//! [`LayoutNames`] says. A struct that is not the program's `Range` gets its
//! shown name, and, unless it is `opaque`, its fields' names; an enum gets its
//! shown name and its cases' names. A range renders as its bounds and gets
//! nothing.
//!
//! An enum's own name is not rendered — a value of one renders as its case —
//! and it is placed for the refusal of a boxed key: `std.dynamic.refuseKey`
//! begins the path to the refused part of an enum at the root of a key with the
//! enum's name and its case, `Mark.Weight(0)`, as the oracle's
//! `MapKey::convert` does (ADR 0068's Phase 4c).
//!
//! An opaque value a box can hold gets the name of its type, which is what a
//! refused key is called by (issue #506): a task, a task scope, a byte run and
//! a byte buffer their layout's, `Task`, `TaskScope`, `Bytes`, `ByteBuffer`.
//! A Host resource cannot, because every resource shares one layout; where a
//! box can hold that layout, the qualified type of every kind of resource the
//! run can hold is placed instead — `http.Server` — in
//! [`Program::resource_names`], and the machine finds the one a handle needs
//! by the kind its resource table records. Every string
//! is interned into [`Program::strings`] as the lowering's own pool interns —
//! one entry per distinct text — so a field named `name` in two structs, or a
//! name the standard library's rendering already writes as a literal, is one
//! placed string.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use cove_schema::builtins::RANGE;
use cove_schema::HostSchemas;

use crate::inst::Inst;
use crate::layout::{LayoutId, Shape};
use crate::program::{LayoutNames, Program, ResourceName, StrId};
use crate::repr::Repr;

/// Places the names every layout a box can hold needs, and records them in
/// [`Program::names`]. See the module's documentation for which those are.
pub(super) fn place(program: &mut Program, schemas: &HostSchemas) {
    let every = program
        .host_ops
        .iter()
        .any(|op| reaches_a_box(program, op.result));
    let roots: Vec<LayoutId> = if every {
        (0..program.layouts.len())
            .map(|at| LayoutId(at as u32))
            .collect()
    } else {
        program
            .functions
            .iter()
            .flat_map(|function| &function.code)
            .filter_map(|inst| match inst {
                Inst::Box { layout, .. } => Some(*layout),
                _ => None,
            })
            .collect()
    };
    if roots.is_empty() {
        return;
    }
    let held = held_by(program, &roots);
    let mut interned: HashMap<Arc<str>, StrId> = program
        .strings
        .iter()
        .enumerate()
        .map(|(at, text)| (text.clone(), StrId(at as u32)))
        .rev()
        .collect();
    let mut names = vec![LayoutNames::default(); program.layouts.len()];
    let mut resources = Vec::new();
    let issued = kinds(program, schemas);
    let mut ordered: Vec<LayoutId> = held.into_iter().collect();
    ordered.sort();
    for layout in ordered {
        if is_range(program, layout) {
            continue;
        }
        let described = program.layouts[layout.index()].clone();
        let mut intern = |text: &str| -> StrId {
            if let Some(id) = interned.get(text) {
                return *id;
            }
            let id = StrId(program.strings.len() as u32);
            let text: Arc<str> = Arc::from(text);
            program.strings.push(text.clone());
            interned.insert(text, id);
            id
        };
        let placed = match &described.shape {
            Shape::Struct { fields, opaque } => {
                let name = Some(intern(crate::dynamic::shown_name(&described.name)));
                let parts = if *opaque {
                    Vec::new()
                } else {
                    fields.iter().map(|field| intern(&field.name)).collect()
                };
                LayoutNames { name, parts }
            }
            Shape::Enum { cases, .. } => LayoutNames {
                name: Some(intern(crate::dynamic::shown_name(&described.name))),
                parts: cases.iter().map(|case| intern(&case.name)).collect(),
            },
            // A task, a task scope, a byte run and a byte buffer are each one
            // layout of their own, and that layout's name is the type's:
            // `Task`, `TaskScope`, `Bytes`, `ByteBuffer`.
            Shape::Word(Repr::Task | Repr::Scope) | Shape::Bytes | Shape::ByteBuffer => {
                LayoutNames {
                    name: Some(intern(&described.name)),
                    parts: Vec::new(),
                }
            }
            // Every resource shares this one layout, so its names are not the
            // layout's: they are the kinds of resource the run can hold.
            Shape::Word(Repr::Host) => {
                resources = issued
                    .iter()
                    .cloned()
                    .map(|(module, resource)| {
                        let text = intern(&format!("{module}.{resource}"));
                        ResourceName {
                            module,
                            resource,
                            text,
                        }
                    })
                    .collect();
                continue;
            }
            _ => continue,
        };
        names[layout.index()] = placed;
    }
    program.names = names;
    program.resource_names = resources;
}

/// Every kind of resource a run of `program` can hold, as the module that
/// issues it and the kind's name, in the order the schemas declare them.
///
/// A handle is issued by an operation of the module that declares its kind —
/// `http.listen` answers an `http.Server` — so a run holds only kinds of the
/// modules its Host operations name. That is exact enough: a program that
/// calls one operation of a module places the names of every kind the module
/// declares, which is a few words, and only once a box can hold a resource.
fn kinds(program: &Program, schemas: &HostSchemas) -> Vec<(Arc<str>, Arc<str>)> {
    let mut modules: Vec<&str> = program.host_ops.iter().map(|op| &*op.module).collect();
    modules.sort_unstable();
    modules.dedup();
    let mut kinds = Vec::new();
    for module in modules {
        let Some(schema) = schemas.module(module) else {
            continue;
        };
        for resource in schema.resources {
            kinds.push((Arc::from(module), Arc::from(resource.name)));
        }
    }
    kinds
}

/// Every layout a value of one of `roots` can hold a part of, `roots`
/// included: the parts a view can be projected to, which are the ones a
/// rendering shows.
fn held_by(program: &Program, roots: &[LayoutId]) -> HashSet<LayoutId> {
    let mut seen = HashSet::new();
    let mut pending = roots.to_vec();
    while let Some(at) = pending.pop() {
        if at.index() >= program.layouts.len() || !seen.insert(at) {
            continue;
        }
        match &program.layouts[at.index()].shape {
            Shape::Struct { fields, .. } => pending.extend(fields.iter().map(|field| field.layout)),
            Shape::Enum { cases, .. } => pending.extend(
                cases
                    .iter()
                    .flat_map(|case| case.parts.iter().map(|part| part.layout)),
            ),
            Shape::Elements { elem, .. } | Shape::Vector { elem } | Shape::Members { elem } => {
                pending.push(*elem)
            }
            Shape::Entries { key, value } => pending.extend([*key, *value]),
            _ => {}
        }
    }
    seen
}

/// Whether a value of `layout` can hold a box: whether the boundary may box
/// something at a layout of its own choosing when a host answers one.
fn reaches_a_box(program: &Program, layout: LayoutId) -> bool {
    held_by(program, &[layout])
        .into_iter()
        .any(|at| matches!(program.layouts[at.index()].shape, Shape::Boxed))
}

/// Whether `layout` is the program's builtin `Range`, which renders as its
/// bounds and shows no name: `cove-runtime`'s `boundary::is_range`, over the
/// finished table.
fn is_range(program: &Program, layout: LayoutId) -> bool {
    let described = &program.layouts[layout.index()];
    let Shape::Struct { fields, .. } = &described.shape else {
        return false;
    };
    let word = |at: usize, name: &str, repr: Repr| {
        fields.get(at).is_some_and(|field| {
            &*field.name == name && program.layouts[field.layout.index()].words == [repr]
        })
    };
    &*described.name == RANGE.name
        && fields.len() == 3
        && described.words == [Repr::Int, Repr::Int, Repr::Bool]
        && word(0, "start", Repr::Int)
        && word(1, "end", Repr::Int)
        && word(2, "inclusive", Repr::Bool)
}
