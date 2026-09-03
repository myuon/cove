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
//! # A *declaration* is one layout per instantiation
//!
//! A generic `struct` is the other side of that rule. `Cell<Int>` is one word
//! and `Cell<Point>` is two, so they are not one family and cannot be one
//! layout: a frame's per-slot `Repr` map is static and it is what the
//! collector's reference map is derived from. So a declaration's key carries
//! the arguments it was reached at — `m.Cell<Int>` and `m.Cell<m.Point>` —
//! and two instantiations are two layouts with two names. The fields are the
//! declaration's own, completed with [`Ty::instantiate`], because the checker
//! records a declaration's shape once in terms of the parameters it binds.
//!
//! # A type a host module declares is one of three things
//!
//! `Ty::Host` is written the same way whichever it is — `http.Response`,
//! which the host hands over, reads like `http.Server`, which it keeps — so
//! the name says nothing about the words and the schema says everything. A
//! **resource** is one [`Repr::Host`] word, an index into the run's host
//! resource table, and it is the one word that is neither a scalar nor an
//! address into this run's memory: ADR 0013 says the host keeps what the
//! handle names, so there is nothing on this side for a collection to trace.
//! A host **enum** is a discriminant, and a host **struct** is its fields
//! inline, both exactly as a declared one is. See [`Shapes::host_type`].
//!
//! # Recursion is finite exactly when it passes through a reference
//!
//! `struct Node { value: Int, next: Option<Node> }` has no finite inline
//! width. [ADR 0035](../../../../docs/adr/0035-a-value-type-may-not-contain-itself.md)
//! decides what happens to it: it is a **checker** error, so that both
//! execution backends agree on which programs exist. A recursive cycle must
//! pass through a type whose values are a reference — `Array`, `Vector`,
//! `Map`, `Set`, `String`, `Shared`, a closure, a `dyn` — and
//! `Node { peers: Vector<Node> }` is the shape a program writes instead.
//!
//! An earlier version of this module inserted a box wherever it found a
//! cycle. That made an ordinary assignment share mutation — copying the
//! location copied the address — so whether `b.value = 7` was visible through
//! `a` depended on whether the type happened to mention itself. A
//! representation was deciding the language's semantics, which is exactly
//! what ADR 0035 refuses. It is gone, and [`Shape::Boxed`] is left with one
//! meaning: a value whose type was *intentionally* erased.
//!
//! What is left here is the two questions the *table* has to answer, and
//! [`Shapes::reached`] is where they are told apart.
//!
//! A cycle through a reference is finite and has to be built. `Node`'s
//! layout names `Vector<Node>`'s and `Vector<Node>`'s names `Node`'s, so
//! neither can be finished first — but a collection's own layout is one
//! `Repr::Ref` word *whatever it holds*, so it needs the element's
//! [`LayoutId`] and never the element's words. So a declaration's id is
//! **reserved** before its fields are resolved and filled in on the way back
//! out, and a mention of it from behind a reference answers the reserved id.
//!
//! A cycle that is not behind one cannot be built, and by ADR 0035 it is not
//! a program. The checker rejects it; this records it in
//! [`Shapes::recursive`] and answers `None`, which every caller turns into a
//! gap naming the type — because the walk still has to terminate if one ever
//! reaches here.
//!
//! # It reads the checker's answers
//!
//! A struct's fields and an enum's cases are read from the `Signature` the
//! checker recorded for the declaration. Nothing here re-resolves an
//! annotation, because an annotation is a name and only the checker knows
//! what the name meant in the module it was written in.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use cove_schema::builtins::MAP_ENTRY;
use cove_schema::{HostSchemas, HostType};
use cove_sema::resolve::Program as Checked;
use cove_sema::typeck::Ty;

use crate::layout::{enum_layout, struct_layout, Layout, LayoutId, Shape};
use crate::repr::Repr;
use crate::FunctionId;

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
        Repr::Task => TASK,
        Repr::Scope => SCOPE,
    }
}

/// An index into the run's host resource table: a `files.Reader`, an
/// `http.Server`, a `database.Connection`.
///
/// One layout for every resource a host declares, the way [`STR`] is one for
/// every string: a handle is a handle, and which resource an index names is
/// the table's business rather than the layout's. ADR 0013 is what makes
/// that enough — the host keeps whatever the thing really is, and Cove holds
/// only the name of it.
pub(super) const HOST: LayoutId = LayoutId(9);

/// The object every `Box` allocates.
///
/// One shape for every box, because what a box holds is named by its own
/// first payload word rather than by its object layout. Reserved rather than
/// interned on demand so that a program that boxes nothing still declares
/// it: the machine allocates one for a host's answer, and a table it has to
/// search first is a search that has to answer something when it fails.
pub(super) const BOXED: LayoutId = LayoutId(10);

/// A task handle: one past an index into the task's scheduler table.
///
/// One layout for every `Task<T>`, whatever `T` is, and that is not the
/// [`STR`] argument in disguise — a handle names an *entry*, and what the
/// entry holds the address of is a fact about the task rather than about the
/// word. Which layout the answer has is carried by the instructions that
/// need it, [`crate::Inst::Spawn`] and [`crate::Inst::Await`], because those
/// are the two places a value crosses between the table and a frame.
pub(super) const TASK: LayoutId = LayoutId(11);

/// A task scope: one past an index into the same table.
pub(super) const SCOPE: LayoutId = LayoutId(12);

/// Payload word 0 of a [`Shape::Closure`] object: the callee's `FunctionId`.
pub(super) const CLOSURE_CALLEE: u32 = 0;

/// Where a [`Shape::Closure`] object's captures begin, each inline at its own
/// layout's width.
pub(super) const CLOSURE_CAPTURES: u32 = 1;

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
    /// The host modules this compilation was given, which is the same set
    /// the checker read.
    ///
    /// It is carried rather than read from [`cove_schema::hosts`] because
    /// those are the *shipped* modules alone, and an embedder's are not a
    /// lesser kind: `HostApi` is a trait, `Compiler::with_host_schema` is how
    /// a run's own module is described, and a lowering that read the shipped
    /// tables would answer for fewer programs than the checker accepted —
    /// which is a backend refusing a program the language admits. See
    /// [`Shapes::host_type`].
    schemas: HostSchemas,
    /// The nominal declarations a call to [`Shapes::of`] is part way
    /// through, innermost last: the key, the [`LayoutId`] reserved for it,
    /// and how many reference-shaped families the walk was inside when it
    /// began.
    ///
    /// Meeting one of these again is the recursion, and the third field is
    /// what says which kind. See [`Shapes::reached`].
    building: Vec<(String, LayoutId, u32)>,
    /// How many reference-shaped families this walk is currently inside.
    behind: u32,
    /// The [`LayoutId`] of each nominal declaration, once it is settled.
    ///
    /// A memo rather than a second table: [`Shapes::intern`] would answer the
    /// same id, and this is here because a declaration's id is *reserved*
    /// before its fields are resolved and so cannot be found by comparing
    /// layouts while it is being built.
    named: HashMap<String, LayoutId>,
    /// The declarations whose layout was found to contain itself.
    ///
    /// Each is recorded twice, under the key a layout is named by and under
    /// the name the source wrote, so that [`Shapes::contains_itself`] is a
    /// set lookup rather than a match on how a type happens to be spelled at
    /// the place it was met.
    recursive: BTreeSet<String>,
}

impl Shapes {
    /// The layouts every program declares whether or not it uses them.
    ///
    /// `LayoutId(0)` is what the sweeper writes into a reclaimed run of
    /// words; the string layout is what the machine allocates a host's
    /// answer as; and the scalars are there because a one-word value is the
    /// width-one case of the model rather than a family of its own, so
    /// naming one should not depend on a program having mentioned it.
    pub(super) fn new(schemas: HostSchemas) -> Shapes {
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
            Layout::word("Task", Repr::Task),
            Layout::word("TaskScope", Repr::Scope),
        ];
        Shapes {
            layouts,
            schemas,
            building: Vec::new(),
            behind: 0,
            named: HashMap::new(),
            recursive: BTreeSet::new(),
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

    /// Whether the declaration `name` was found to contain itself.
    ///
    /// What it decides is one word of a diagnostic: a type with no layout is
    /// a gap either way, and this is what lets the gap say *why* rather than
    /// leaving a reader to work out that `Node` is not merely unimplemented.
    /// ADR 0035 makes this the checker's refusal; until that lands, the
    /// sentence is written here.
    pub(super) fn contains_itself(&self, name: &str) -> bool {
        self.recursive.contains(name)
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
            // `Any` and `dyn Trait` are one erased representation and two
            // types. Mapping several types onto one layout is what this
            // function is for; keeping them apart in `Ty` is what keeps a
            // `dyn Display` from answering everything, and is why
            // `cove-sema` gives `Any` a variant of its own rather than a
            // reserved trait name.
            Ty::Any | Ty::Dyn(_) => Some(BOXED),
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
                let elem = self.element(checked, module, elem)?;
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
                let elem = self.element(checked, module, elem)?;
                self.store_of(elem);
                Some(self.intern(Layout::object("Vector", Shape::Vector { elem })))
            }
            // A `Set` and a `Map` are sorted runs rather than hash tables,
            // because the language says they iterate in ascending order and
            // render that way: the order is part of the value. One layout per
            // element layout, and one per *pair* for a map — a
            // `Map<String, Int>` traces half its words and a `Map<Int, Int>`
            // none of them, and the collector is told which by the layout
            // rather than by looking.
            Ty::Set(elem) => {
                let elem = self.element(checked, module, elem)?;
                Some(self.intern(Layout::object("Set", Shape::Members { elem })))
            }
            Ty::Map(key, value) => {
                let key = self.element(checked, module, key)?;
                let value = self.element(checked, module, value)?;
                Some(self.intern(Layout::object("Map", Shape::Entries { key, value })))
            }
            // The one handle that crosses a task boundary by sharing rather
            // than by copying, and an ordinary object in the run's one heap:
            // a lock word, then the wrapped value inline. One layout per
            // wrapped-value layout, because what the value's words are is
            // what a collection and a `lock`'s address both read.
            //
            // Reached through `element`, so a cycle that passes through a cell
            // is finite the way one through an `Array` is — a cell's own
            // layout is one `Repr::Ref` word whatever it holds, which is the
            // sentence `cove-sema` quotes when it lists `Shared<T>` among the
            // references a recursive declaration may pass through.
            Ty::Shared(inner) => {
                let value = self.element(checked, module, inner)?;
                Some(self.intern(Layout::object("Shared", Shape::Shared { value })))
            }
            // The pair a `Map` is built from and iterated as, and it is an
            // ordinary inline struct: the entry a `Map.of` literal writes is
            // the run of words its two fields occupy, and one entry of a
            // `Shape::Entries` run is the same words in the same order — the
            // key's, then the value's. That correspondence is why a `for` over
            // a `Map` is one [`crate::Inst::LoadElem`] at this layout's width
            // and needs nothing built per turn.
            //
            // The name is the checker's, and `cove_schema::builtins::MAP_ENTRY`
            // is what both ends read the two field names off, so a value built
            // here is one `cove_runtime::vm::builtins::keyed` recognises.
            Ty::MapEntry(..) => {
                let declared = struct_fields(checked, module, ty)?;
                let mut placed = Vec::with_capacity(declared.len());
                for (field, ty) in &declared {
                    placed.push((field.clone(), self.of(checked, module, ty)?));
                }
                let (fields, words) = struct_layout(&placed, &self.layouts);
                Some(self.intern(Layout::inline(
                    MAP_ENTRY.name,
                    Shape::Struct {
                        fields,
                        opaque: false,
                    },
                    words,
                )))
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
            // A function value is one word holding the address of its
            // environment, and that is true of every one of them — so this
            // is one layout for the whole program, the way `Array<String>`
            // and `Array<Point>` are one layout because a reference is a
            // reference. Which function a particular value calls and what it
            // captured are facts about the *object*, in the
            // [`Shape::Closure`] layout its own header names.
            Ty::Fn(_) => Some(self.function_value()),
            // A task handle and a task scope name **scheduler state**, which
            // ADR 0034 carves out of what a value store is: one word, one
            // past an index into the task's own table, and the same for
            // every `T` because what the entry holds the address of is a
            // fact about the task rather than about the word. Neither may
            // cross a task boundary, so the table one indexes is always the
            // indexing task's.
            Ty::Task(_) => Some(TASK),
            Ty::Scope => Some(SCOPE),
            Ty::Struct(name, args) => self.declared_struct(checked, module, name, args),
            Ty::Enum(name, args) => self.declared_enum(checked, module, name, args),
            Ty::Host(qualified) => self.host_type(checked, module, qualified),
            _ => None,
        }
    }

    /// The layout of a value of a type a host module declares.
    ///
    /// [`Ty::Host`] is written the same way whichever kind of type a schema
    /// declares — `http.Response`, which the host hands over, reads like
    /// `http.Server`, which it keeps — so the name alone does not say what
    /// the words are. The schema does, and it says one of three things.
    ///
    /// A **resource** is one [`Repr::Host`] word: an index into the run's
    /// host resource table. ADR 0013 is what makes that the whole of it — a
    /// handle is an identity and never state, the host owns what it names,
    /// and Cove holds only the name. So it is neither a scalar the program
    /// computes with nor an object in the heap, and it is *not a root*: a
    /// collection traces nothing through one because there is nothing on
    /// this side to trace. Every resource shares [`HOST`], the way every
    /// `Array` shares one word of `Repr::Ref`, because a handle is a handle
    /// and which resource it names is the table's business.
    ///
    /// A host **enum** is a discriminant and nothing else: a schema writes
    /// `cases: &["Get", "Post"]` and gives them no payload to carry.
    ///
    /// A host **struct** is its fields, inline, exactly as a declared one is
    /// — `TypeSchema`'s own documentation says a host type is ordinary data
    /// and needs no representation of its own. The layout's name is the
    /// qualified one the source writes, which is what the boundary
    /// materialises a `Value::Struct` under and what it compares an incoming
    /// one against.
    ///
    /// # It reads the schemas this compilation was given
    ///
    /// [`Shapes::schemas`] and not [`cove_schema::hosts`], so a type an
    /// embedder's module declares is described exactly as a shipped one is.
    /// The checker resolved the name against the same set; a lowering that
    /// read only the shipped tables would answer for fewer programs than the
    /// checker accepted, which is a backend refusing a program the language
    /// admits rather than a gap anybody can build.
    fn host_type(&mut self, checked: &Checked, module: &str, qualified: &str) -> Option<LayoutId> {
        let (host, short) = qualified.rsplit_once('.')?;
        let schema = self.schemas.module(host)?;
        if schema.resource(short).is_some() {
            return Some(HOST);
        }
        let declared = schema.declared_type(short)?;
        if declared.is_enum() {
            let cases: Vec<(Arc<str>, Vec<LayoutId>)> = declared
                .cases
                .iter()
                .map(|case| (Arc::from(*case), Vec::new()))
                .collect();
            return self.enum_of(qualified, &cases);
        }
        // Reserved before the fields are resolved for the reason a
        // declaration's id is: a field may name another host type, and
        // nothing says a schema's types cannot refer to one another.
        if let Some(answer) = self.reached(qualified, qualified) {
            return answer;
        }
        let id = self.reserve(qualified);
        let mut placed = Vec::with_capacity(declared.fields.len());
        for field in declared.fields {
            match self.host_layout(checked, module, &field.ty) {
                Some(held) => placed.push((Arc::from(field.name), held)),
                None => {
                    self.building.pop();
                    return None;
                }
            }
        }
        self.building.pop();
        let (fields, words) = struct_layout(&placed, &self.layouts);
        let layout = Layout::inline(
            qualified,
            Shape::Struct {
                fields,
                opaque: false,
            },
            words,
        );
        Some(self.settle(qualified, id, layout))
    }

    /// Whether the host module `module` keeps `name` rather than handing it
    /// over: a `files.Writer`, not a `http.Response`.
    ///
    /// It is the one question that decides whether a method written on a
    /// value of a host type is an operation the host answers on a handle,
    /// and it is asked of the same schemas [`Shapes::host_type`] read the
    /// layout out of.
    pub(super) fn is_resource(&self, module: &str, name: &str) -> bool {
        self.schemas
            .module(module)
            .is_some_and(|schema| schema.resource(name).is_some())
    }

    /// The fields `module.name` is initialized with, where the host declares
    /// it as plain data rather than as an enum or as something it keeps.
    ///
    /// This is [`struct_fields`] for a host type, and it answers the same
    /// pair for the same reason: a label is a field's name, and a field's
    /// declared type is where a value written into it is erased. The names
    /// are `TypeSchema::fields[..].name`, which is what
    /// `interp::init_host_type` reads to line an initializer's arguments up
    /// with the fields — a host struct is initialized with labels exactly as
    /// a declared one is, and `TypeSchema`'s own documentation says so.
    ///
    /// The types are the schema's, translated by [`host_ty`], so a field the
    /// schema declared `Any` — `http.Route`'s `handler` — reads as erasure
    /// here and is boxed on the way in, rather than as an unknown the
    /// checker declined to settle.
    ///
    /// `None` for an enum, for a resource, and for a name no schema this
    /// compilation was given declares: none of the three is initialized with
    /// fields, and a caller that meets one has something else to do with it.
    pub(super) fn host_fields(&self, qualified: &str) -> Option<Vec<(Arc<str>, Ty)>> {
        let (module, short) = qualified.rsplit_once('.')?;
        let declared = self.schemas.module(module)?.declared_type(short)?;
        if declared.is_enum() {
            return None;
        }
        Some(
            declared
                .fields
                .iter()
                .map(|field| (Arc::from(field.name), host_ty(&field.ty)))
                .collect(),
        )
    }

    /// Which case of a host module's enum `case` is, if it names one.
    ///
    /// This is [`case_at`] for a host type, and the answer is an index alone
    /// rather than an index and a payload because a host's cases carry none:
    /// a schema writes `cases: &["Get", "Post"]` and gives them nothing.
    /// `interp::host_enum_case` is the oracle, and it is a function of its
    /// own there for the same reason this is one here — a host's enum has a
    /// [`TypeSchema`](cove_schema::TypeSchema) rather than a declaration, so
    /// there is nothing to read a case's payload arity from.
    ///
    /// The order is the schema's, which is the order [`Shapes::host_type`]
    /// built the layout's cases in, so an index means the same thing on both
    /// sides.
    pub(super) fn host_case(&self, qualified: &str, case: &str) -> Option<u32> {
        let (module, short) = qualified.rsplit_once('.')?;
        let declared = self.schemas.module(module)?.declared_type(short)?;
        declared
            .cases
            .iter()
            .position(|name| *name == case)
            .map(|at| at as u32)
    }

    /// The layout of a value a host schema declared, wherever the schema
    /// declares one: a field of a host struct, or an operation's result.
    ///
    /// A schema's `Any` is erasure rather than abstention, so it is a box
    /// carrying its own description — and it is read from the *schema*
    /// rather than from the type the checker settled because the two are not
    /// the same fact. The checker spells a schema's `Any` and a type
    /// parameter nothing settled with one value, `Unknown(Unconstrained)`;
    /// the schema still holds them apart, and this is the side of the
    /// boundary where it can be asked. See [`host_ty`].
    ///
    /// The whole declared type rather than its head, because a schema nests
    /// one: `clock.timeout` declares `Result<Any, Error>`, whose `Ok`
    /// carries an erased value and whose `Err` does not.
    pub(super) fn host_layout(
        &mut self,
        checked: &Checked,
        module: &str,
        declared: &HostType,
    ) -> Option<LayoutId> {
        let ty = host_ty(declared);
        self.of(checked, module, &ty)
    }

    /// The type a host operation's schema declares its result to be, as the
    /// schema writes it.
    ///
    /// `resource` names the kind a call is addressed to — `files.Writer` —
    /// and `None` is an operation of the module itself. Both are looked up
    /// in [`Shapes::schemas`], which is the set this compilation was given
    /// and so the set the checker resolved the call against.
    pub(super) fn declared_result(
        &self,
        module: &str,
        resource: Option<&str>,
        operation: &str,
    ) -> Option<&'static HostType> {
        let schema = self.schemas.module(module)?;
        let found = match resource {
            Some(kind) => schema.resource(kind)?.operation(operation)?,
            None => schema.operation(operation)?,
        };
        Some(&found.result)
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

    /// The layout of a *location* holding a function value.
    ///
    /// One word, and one layout for every signature. See [`Shapes::of`].
    pub(super) fn function_value(&mut self) -> LayoutId {
        self.intern(Layout::word("fn", Repr::Ref))
    }

    /// The layout of the environment object one lowered lambda allocates.
    ///
    /// One per lowered lambda, because payload word 0 is *that* lambda's
    /// [`FunctionId`] and the captures after it are the ones *that* body
    /// reads. It describes the object rather than the location: a location
    /// holding a function value is [`Shapes::function_value`], one word,
    /// whichever environment the word happens to name.
    pub(super) fn closure_of(
        &mut self,
        name: &str,
        function: FunctionId,
        captures: Vec<LayoutId>,
    ) -> LayoutId {
        self.intern(Layout::object(
            format!("closure {name}"),
            Shape::Closure { function, captures },
        ))
    }

    /// The layout of what a family whose values are one reference holds.
    ///
    /// This is where "the cycle passes through a reference" is recorded. Such
    /// a family's own layout is one `Repr::Ref` word *whatever it holds*, so
    /// it needs the element's [`LayoutId`] and never the element's words —
    /// which is exactly what lets a declaration reached from inside one
    /// answer the id reserved for it before its own fields are resolved.
    fn element(&mut self, checked: &Checked, module: &str, ty: &Ty) -> Option<LayoutId> {
        self.behind += 1;
        let id = self.of(checked, module, ty);
        self.behind -= 1;
        id
    }

    /// What a mention of the declaration `key` answers before its fields are
    /// walked: its settled id, its reserved one, `None` for a cycle ADR 0035
    /// forbids, or nothing at all when it has to be built.
    ///
    /// The one question is whether the walk has passed through a
    /// reference-shaped family since this declaration began. If it has, the
    /// recursion is finite and the reserved id is the answer. If it has not,
    /// the declaration contains itself by value: the checker rejects it, and
    /// this records it and stops so that the walk terminates.
    fn reached(&mut self, key: &str, name: &str) -> Option<Option<LayoutId>> {
        if let Some((_, id, depth)) = self.building.iter().find(|(held, _, _)| held == key) {
            let (id, depth) = (*id, *depth);
            if self.behind > depth {
                return Some(Some(id));
            }
            return Some(self.cycle(key, name));
        }
        self.named.get(key).copied().map(Some)
    }

    /// Takes the [`LayoutId`] a declaration will have, before it is known
    /// what is in it.
    ///
    /// The placeholder's words are never read: the only thing that can name
    /// this id before [`Shapes::settle`] fills it in is a family whose own
    /// layout is one `Repr::Ref` however wide its elements are.
    fn reserve(&mut self, key: &str) -> LayoutId {
        let id = LayoutId(self.layouts.len() as u32);
        self.layouts.push(Layout::inline(
            format!("<building {key}>"),
            Shape::Free,
            Vec::new(),
        ));
        self.building.push((key.to_string(), id, self.behind));
        id
    }

    /// Fills in a reserved id, and remembers it so the declaration is built
    /// once.
    fn settle(&mut self, key: &str, id: LayoutId, layout: Layout) -> LayoutId {
        self.layouts[id.index()] = layout;
        self.named.insert(key.to_string(), id);
        id
    }

    /// Records that `key`, written `name` where it was met, has a layout
    /// that contains itself — and answers `None`, which is what stops the
    /// walk.
    fn cycle(&mut self, key: &str, name: &str) -> Option<LayoutId> {
        self.recursive.insert(key.to_string());
        self.recursive.insert(name.to_string());
        None
    }

    /// A discriminant word and a payload region, as a layout.
    fn enum_shape(&self, name: &str, cases: &[(Arc<str>, Vec<LayoutId>)]) -> Layout {
        let (cases, payload) = enum_layout(cases, &self.layouts);
        let mut words = Vec::with_capacity(1 + payload.len());
        words.push(Repr::Int);
        words.extend_from_slice(&payload);
        Layout::inline(name, Shape::Enum { cases, payload }, words)
    }

    /// The same, interned: `Option` and `Result` are families rather than
    /// declarations, so one shape is one id and there is no id to reserve.
    fn enum_of(&mut self, name: &str, cases: &[(Arc<str>, Vec<LayoutId>)]) -> Option<LayoutId> {
        let layout = self.enum_shape(name, cases);
        Some(self.intern(layout))
    }

    fn declared_struct(
        &mut self,
        checked: &Checked,
        module: &str,
        name: &str,
        args: &[Ty],
    ) -> Option<LayoutId> {
        let (owner, short) = declaring(checked, module, name)?;
        let args = qualify_all(checked, module, args);
        let key = instance_key(&owner, &short, &args);
        if let Some(answer) = self.reached(&key, name) {
            return answer;
        }
        let declared = struct_fields(checked, module, &Ty::Struct(Arc::from(name), args))?;
        let id = self.reserve(&key);
        let mut placed = Vec::with_capacity(declared.len());
        for (field, ty) in &declared {
            match self.of(checked, &owner, ty) {
                Some(held) => placed.push((field.clone(), held)),
                None => {
                    self.building.pop();
                    return None;
                }
            }
        }
        self.building.pop();
        let (fields, words) = struct_layout(&placed, &self.layouts);
        let opaque = struct_is_opaque(checked, module, name);
        let layout = Layout::inline(key.clone(), Shape::Struct { fields, opaque }, words);
        Some(self.settle(&key, id, layout))
    }

    fn declared_enum(
        &mut self,
        checked: &Checked,
        module: &str,
        name: &str,
        args: &[Ty],
    ) -> Option<LayoutId> {
        let (owner, short) = declaring(checked, module, name)?;
        let args = qualify_all(checked, module, args);
        let key = instance_key(&owner, &short, &args);
        if let Some(answer) = self.reached(&key, name) {
            return answer;
        }
        let declared = enum_cases(checked, module, &Ty::Enum(Arc::from(name), args))?;
        let id = self.reserve(&key);
        let mut placed = Vec::with_capacity(declared.len());
        for (case, types) in &declared {
            let mut parts = Vec::with_capacity(types.len());
            for ty in types {
                match self.of(checked, &owner, ty) {
                    Some(held) => parts.push(held),
                    None => {
                        self.building.pop();
                        return None;
                    }
                }
            }
            placed.push((case.clone(), parts));
        }
        self.building.pop();
        let layout = self.enum_shape(&key, &placed);
        Some(self.settle(&key, id, layout))
    }
}

/// A schema's vocabulary read as the checker's.
///
/// The same translation `Checker::host_ty` makes, with one difference that
/// is the whole reason it is written again rather than shared:
/// [`HostType::Any`] is [`Ty::Dyn`] here and an unconstrained unknown there.
///
/// The checker is not wrong. `Any` in a *parameter* is a promise that every
/// value is accepted, and an unknown equal to every type is exactly that
/// promise — [ADR 0016](../../../../docs/adr/0016-four-kinds-of-unknown.md)
/// says so. What the checker's answer loses is the difference between that
/// promise and a type parameter nothing settled, because it spells both
/// `Unknown(Unconstrained)`. A backend cannot recover the difference from
/// the type; it can only ask the schema, which is what this reads.
///
/// [`Ty::Dyn`] rather than a marker of its own, because there is nothing to
/// distinguish. `docs/LINEAR_VM.md` names one representation for both — "a
/// value whose type is *intentionally* erased — `dyn Trait`, a Host result a
/// schema declared `Any` — is one `Ref` word naming a `Boxed` object" — so a
/// schema's `Any` and a written `dyn` reach [`Shapes::of`] as the same thing
/// and leave it as the same [`BOXED`]. The name is [`ERASED`], because no
/// trait was written; nothing reads it, since every `Ty::Dyn` has a layout
/// and so no diagnostic is ever built from one.
fn host_ty(declared: &HostType) -> Ty {
    let one = |ty: &HostType| Box::new(host_ty(ty));
    match declared {
        HostType::Unit => Ty::Unit,
        HostType::Bool => Ty::Bool,
        HostType::Int => Ty::Int,
        HostType::String => Ty::Str,
        HostType::Duration => Ty::Duration,
        HostType::Error => Ty::Error,
        HostType::Array(item) => Ty::Array(one(item)),
        HostType::Set(item) => Ty::Set(one(item)),
        HostType::Map(key, value) => Ty::Map(one(key), one(value)),
        HostType::Option(some) => Ty::Option(one(some)),
        HostType::Result(ok, error) => Ty::Result(one(ok), one(error)),
        HostType::Named(name) => Ty::Host((*name).into()),
        HostType::Any => Ty::Dyn(Arc::from(ERASED)),
    }
}

/// The trait name an erased Host value is written under when there is none.
///
/// A schema's `Any` names no trait, and [`Ty::Dyn`] carries one. This is what
/// stands in it: one string, never rendered, and the same one every time so
/// that two erased positions read as one type.
const ERASED: &str = "Any";

/// What a declaration's layout is named by at one instantiation.
///
/// The declaration alone for a declaration that binds no type parameters,
/// and the declaration with the arguments written after it for one that
/// does. A layout's name is an identity, and `m.Cell<Int>` and
/// `m.Cell<m.Point>` are two identities because they are two widths — which
/// is the whole reason monomorphisation is the only representation that fits
/// this machine.
///
/// The arguments are [`qualified`] first, so that a `Cell<Point>` written in
/// the module that declares `Point` and a `Cell<m.Point>` written anywhere
/// else are one key and therefore one [`LayoutId`]. Two ids for one shape
/// would be two arms of a `dyn` dispatch table for one type.
fn instance_key(owner: &str, short: &str, args: &[Ty]) -> String {
    if args.is_empty() {
        return format!("{owner}.{short}");
    }
    let written: Vec<String> = args.iter().map(Ty::to_string).collect();
    format!("{owner}.{short}<{}>", written.join(", "))
}

/// The type parameters `name`'s declaration binds, in declaration order.
///
/// Empty for a declaration that binds none, which is what makes
/// [`Ty::instantiate`] the identity for one.
fn generics_of(checked: &Checked, owner: &str, short: &str) -> Vec<Arc<str>> {
    let Some(resolved) = checked.modules.get(owner) else {
        return Vec::new();
    };
    let generics = match (resolved.structs.get(short), resolved.enums.get(short)) {
        (Some(entry), _) => &entry.decl.generics,
        (_, Some(entry)) => &entry.decl.generics,
        _ => return Vec::new(),
    };
    generics
        .iter()
        .map(|param| Arc::from(param.name.node.as_str()))
        .collect()
}

/// The same type with every declared name written the way the package names
/// it: `main.Article` rather than `Article`.
///
/// A type the checker settled is spelled in the vocabulary of the module it
/// was settled in — a module's own declaration is a bare name there and a
/// qualified one everywhere else. That is exactly what a monomorphisation's
/// identity must not depend on: two modules passing one type to one generic
/// declaration ask for one instantiation, and the body they get is lowered
/// in the *declaring* module, where the call site's bare name means nothing.
///
/// So a type argument is written out once, here, before it becomes part of an
/// instantiation's name or of a layout's. Nothing is resolved: `declaring` is
/// the checker's own `owner_of` table, and a name it cannot place is left as
/// it was written.
pub(super) fn qualified(checked: &Checked, module: &str, ty: &Ty) -> Ty {
    let named = |name: &Arc<str>| match declaring(checked, module, name) {
        Some((owner, short)) => Arc::from(format!("{owner}.{short}")),
        None => name.clone(),
    };
    let each = |args: &[Ty]| -> Vec<Ty> {
        args.iter()
            .map(|arg| qualified(checked, module, arg))
            .collect()
    };
    let one = |ty: &Ty| Box::new(qualified(checked, module, ty));
    match ty {
        Ty::Struct(name, args) => Ty::Struct(named(name), each(args)),
        Ty::Enum(name, args) => Ty::Enum(named(name), each(args)),
        // A trait's name is spelled the two ways a type's is, and one written
        // both ways would be two keys naming one box.
        Ty::Dyn(name) => Ty::Dyn(named(name)),
        Ty::Array(elem) => Ty::Array(one(elem)),
        Ty::Vector(elem) => Ty::Vector(one(elem)),
        Ty::Set(elem) => Ty::Set(one(elem)),
        Ty::Option(some) => Ty::Option(one(some)),
        Ty::Task(inner) => Ty::Task(one(inner)),
        Ty::Shared(inner) => Ty::Shared(one(inner)),
        Ty::Map(key, value) => Ty::Map(one(key), one(value)),
        Ty::MapEntry(key, value) => Ty::MapEntry(one(key), one(value)),
        Ty::Result(ok, err) => Ty::Result(one(ok), one(err)),
        other => other.clone(),
    }
}

/// [`qualified`] over a list.
fn qualify_all(checked: &Checked, module: &str, args: &[Ty]) -> Vec<Ty> {
    args.iter()
        .map(|arg| qualified(checked, module, arg))
        .collect()
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
/// be a second description of the same object. `MapEntry` is the second one
/// the language declares rather than a module, and its two fields are the
/// labels `MapEntry(key:, value:)` is written with.
///
/// A generic declaration's fields are recorded once, in terms of the
/// parameters it binds — `Cell<T>`'s field is a `T` however many `Cell<Int>`s
/// a program holds — so a use of it completes them with
/// [`Ty::instantiate`]. That is the checker's own note on
/// [`Signature`](cove_sema::facts::Signature) followed rather than a second
/// reading of the declaration.
pub(super) fn struct_fields(
    checked: &Checked,
    module: &str,
    ty: &Ty,
) -> Option<Vec<(Arc<str>, Ty)>> {
    match ty {
        Ty::Error => Some(vec![(Arc::from("message"), Ty::Str)]),
        Ty::MapEntry(key, value) => Some(vec![
            (Arc::from(MAP_ENTRY.fields[0].name), (**key).clone()),
            (Arc::from(MAP_ENTRY.fields[1].name), (**value).clone()),
        ]),
        Ty::Struct(name, args) => {
            let (owner, short) = declaring(checked, module, name)?;
            let entry = checked.modules.get(&owner)?.structs.get(&short)?;
            let signature = checked
                .facts
                .signature(entry.decl.span.file, entry.decl.span)?;
            let generics = generics_of(checked, &owner, &short);
            Some(
                entry
                    .decl
                    .fields
                    .iter()
                    .zip(&signature.params)
                    .map(|(field, ty)| {
                        (
                            Arc::from(field.name.node.as_str()),
                            ty.instantiate(&generics, args),
                        )
                    })
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
        Ty::Enum(name, args) => {
            let (owner, short) = declaring(checked, module, name)?;
            let entry = checked.modules.get(&owner)?.enums.get(&short)?;
            let generics = generics_of(checked, &owner, &short);
            let mut cases = Vec::with_capacity(entry.decl.cases.len());
            for case in &entry.decl.cases {
                let signature = checked.facts.signature(case.span.file, case.span)?;
                let payload = signature
                    .params
                    .iter()
                    .map(|ty| ty.instantiate(&generics, args))
                    .collect();
                cases.push((Arc::from(case.name.node.as_str()), payload));
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
