//! The seven observations, run as instructions over boxed values of every
//! kind.
//!
//! The values are built in the heap from Rust — a box is one object whose
//! payload word 0 is a `LayoutId` — and every question about them is asked by
//! running a function of the program, so what is under test is the dispatch
//! arm and not only the helper beneath it. The walks are the machine's own:
//! [`describe`] calls one small function per observation, and the counting and
//! rooting walks are written whole in IR, in one run, because an allocation
//! count and a collection are facts about a run.
//!
//! `crate::builtins`' tests describe the same values through the oracle's
//! `call_core` arms and hold them to the same strings.

use std::sync::Arc;

use cove_ir::dynamic::view_layout;
use cove_ir::{
    ArithOp, CmpOp, DynamicKind, FunctionId, Inst, Layout, LayoutId, Len, Program, Repr, Shape,
    Storage,
};

use super::super::tests::{budget, Build};
use super::super::{Machine, GROWABLE_LEN, GROWABLE_STORE};

/// Every layout the fixtures use, and the observation functions over them.
struct Fixture {
    program: Program,
    layouts: Layouts,
    functions: Functions,
}

#[derive(Clone, Copy)]
struct Layouts {
    boxed: LayoutId,
    string: LayoutId,
    unit: LayoutId,
    boolean: LayoutId,
    int: LayoutId,
    float: LayoutId,
    duration: LayoutId,
    point: LayoutId,
    inner: LayoutId,
    outer: LayoutId,
    holder: LayoutId,
    mark: LayoutId,
    option_int: LayoutId,
    option_string: LayoutId,
    result: LayoutId,
    array_int: LayoutId,
    vector_point: LayoutId,
    set_int: LayoutId,
    map: LayoutId,
    range: LayoutId,
    closure: LayoutId,
    secret: LayoutId,
    shared: LayoutId,
}

#[derive(Clone, Copy)]
struct Functions {
    open: FunctionId,
    kind: FunctionId,
    count: FunctionId,
    case: FunctionId,
    child: FunctionId,
    same: FunctionId,
    read_bool: FunctionId,
    read_int: FunctionId,
    read_float: FunctionId,
    read_duration: FunctionId,
    read_string: FunctionId,
    sum_points: FunctionId,
    rooted_in_a_slot: FunctionId,
    rooted_in_a_vector: FunctionId,
    held_by_nothing: FunctionId,
}

/// The view's three words, as a frame holds them.
const VIEW: [Repr; 3] = [Repr::Int, Repr::Ref, Repr::Int];

fn fixture() -> Fixture {
    let mut build = Build::default();
    let boxed = build.boxed();
    let string = build.string_layout();
    let unit = build.scalar(Repr::Unit);
    let boolean = build.scalar(Repr::Bool);
    let int = build.scalar(Repr::Int);
    let float = build.scalar(Repr::Float);
    let duration = build.scalar(Repr::Duration);
    let reference = build.scalar(Repr::Ref);
    build.program.layouts.push(view_layout(int, reference));
    let view = LayoutId(build.program.layouts.len() as u32 - 1);
    build.program.view_layout = view;

    let point = build.structure("m.Point", &[("x", int), ("y", int)]);
    let inner = build.structure("m.Inner", &[("label", string), ("at", point)]);
    let outer = build.structure("m.Outer", &[("name", string), ("inner", inner)]);
    // A field holding a box: the child is followed through it, so a view
    // never denotes the box.
    let holder = build.structure("m.Holder", &[("it", boxed)]);
    let mark = build.enumeration(
        "m.Mark",
        &[
            ("Plain", vec![]),
            ("Count", vec![int]),
            ("Named", vec![string]),
        ],
    );
    let option_int = build.enumeration("Option", &[("None", vec![]), ("Some", vec![int])]);
    let option_string = build.enumeration("Option", &[("None", vec![]), ("Some", vec![string])]);
    let result = build.enumeration("Result", &[("Ok", vec![int]), ("Err", vec![string])]);
    let array_int = build.layout(
        "Array",
        Shape::Elements {
            elem: int,
            growable: false,
        },
    );
    build.layout(
        "Array",
        Shape::Elements {
            elem: point,
            growable: true,
        },
    );
    let vector_point = build.layout("Vector", Shape::Vector { elem: point });
    build.layout(
        "Array",
        Shape::Elements {
            elem: view,
            growable: true,
        },
    );
    // The work stack ADR 0068's walks will keep their views on (#480).
    build.layout("Vector", Shape::Vector { elem: view });
    let set_int = build.layout("Set", Shape::Members { elem: int });
    let map = build.layout(
        "Map",
        Shape::Entries {
            key: string,
            value: int,
        },
    );
    let range = build.structure(
        cove_schema::builtins::RANGE.name,
        &[("start", int), ("end", int), ("inclusive", boolean)],
    );
    let closure = build.layout(
        "closure t.open",
        Shape::Closure {
            function: FunctionId(0),
            captures: Vec::new(),
        },
    );
    let shared = build.layout("Shared", Shape::Shared { value: int });
    build.program.layouts.push(Layout::inline(
        "m.Secret",
        Shape::Struct {
            fields: vec![cove_ir::Field {
                name: Arc::from("code"),
                layout: int,
                at: 0,
            }],
            opaque: true,
        },
        vec![Repr::Int],
    ));
    let secret = LayoutId(build.program.layouts.len() as u32 - 1);

    let reads = |repr: Repr| [VIEW.as_slice(), &[repr]].concat();
    let open = build.function(
        "open",
        &[boxed],
        &[Repr::Ref, Repr::Int, Repr::Ref, Repr::Int],
        view,
        vec![Inst::DynOpen { dst: 1, src: 0 }, Inst::Return { src: 1 }],
    );
    let mut asks = |name: &str, answer: LayoutId, repr: Repr, inst: Inst| {
        build.function(
            name,
            &[view],
            &reads(repr),
            answer,
            vec![inst, Inst::Return { src: 3 }],
        )
    };
    let kind = asks("kind", int, Repr::Int, Inst::DynKind { dst: 3, view: 0 });
    let count = asks("count", int, Repr::Int, Inst::DynCount { dst: 3, view: 0 });
    let case = asks("case", int, Repr::Int, Inst::DynCase { dst: 3, view: 0 });
    let read_bool = asks(
        "readBool",
        boolean,
        Repr::Bool,
        Inst::DynRead { dst: 3, view: 0 },
    );
    let read_int = asks("readInt", int, Repr::Int, Inst::DynRead { dst: 3, view: 0 });
    let read_float = asks(
        "readFloat",
        float,
        Repr::Float,
        Inst::DynRead { dst: 3, view: 0 },
    );
    let read_duration = asks(
        "readDuration",
        duration,
        Repr::Duration,
        Inst::DynRead { dst: 3, view: 0 },
    );
    let read_string = asks(
        "readString",
        string,
        Repr::Ref,
        Inst::DynRead { dst: 3, view: 0 },
    );
    let child = build.function(
        "child",
        &[view, int],
        &[
            Repr::Int,
            Repr::Ref,
            Repr::Int,
            Repr::Int,
            Repr::Int,
            Repr::Ref,
            Repr::Int,
        ],
        view,
        vec![
            Inst::DynChild {
                dst: 4,
                view: 0,
                index: 3,
            },
            Inst::Return { src: 4 },
        ],
    );
    let same = build.function(
        "same",
        &[view, view],
        &[
            Repr::Int,
            Repr::Ref,
            Repr::Int,
            Repr::Int,
            Repr::Ref,
            Repr::Int,
            Repr::Bool,
        ],
        boolean,
        vec![
            Inst::DynSameType { dst: 6, a: 0, b: 3 },
            Inst::Return { src: 6 },
        ],
    );
    let sum_points = sum_points(&mut build, boxed, int);
    let rooted_in_a_slot = rooted(&mut build, boxed, string, array_int, Hold::Slot);
    let rooted_in_a_vector = rooted(
        &mut build,
        boxed,
        string,
        array_int,
        Hold::Vector(reference),
    );
    let held_by_nothing = rooted(&mut build, boxed, string, array_int, Hold::Nothing);
    Fixture {
        program: build.done(),
        layouts: Layouts {
            boxed,
            string,
            unit,
            boolean,
            int,
            float,
            duration,
            point,
            inner,
            outer,
            holder,
            mark,
            option_int,
            option_string,
            result,
            array_int,
            vector_point,
            set_int,
            map,
            range,
            closure,
            secret,
            shared,
        },
        functions: Functions {
            open,
            kind,
            count,
            case,
            child,
            same,
            read_bool,
            read_int,
            read_float,
            read_duration,
            read_string,
            sum_points,
            rooted_in_a_slot,
            rooted_in_a_vector,
            held_by_nothing,
        },
    }
}

/// `fn sumPoints(box: Any) -> Int`: opens a boxed `Vector<Point>` and adds
/// up every `x` and `y` of every element, each read as a view's scalar — a
/// walk to the leaves of a thousand structs, in one run.
///
/// ```text
///   0  dyn.open   v <- box
///   1  dyn.count  n <- v
///   2  int        i <- 0
///   3  int        total <- 0
///   4  int        zero <- 0
///   5  int        one <- 1
/// loop:
///   6  lt         more <- i < n
///   7  branch-false more -> done
///   8  dyn.child  e <- v[i]
///   9  dyn.child  f <- e[0]
///  10  dyn.read   k <- f
///  11  add        total <- total + k
///  12  dyn.child  f <- e[1]
///  13  dyn.read   k <- f
///  14  add        total <- total + k
///  15  add        i <- i + 1
///  16  jump loop
/// done:
///  17  return total
/// ```
fn sum_points(build: &mut Build, boxed: LayoutId, int: LayoutId) -> FunctionId {
    let reprs = [
        Repr::Ref, // 0 box
        Repr::Int, // 1..=3 v
        Repr::Ref,
        Repr::Int,
        Repr::Int,  // 4 n
        Repr::Int,  // 5 i
        Repr::Int,  // 6 total
        Repr::Int,  // 7 zero
        Repr::Int,  // 8 one
        Repr::Bool, // 9 more
        Repr::Int,  // 10..=12 e
        Repr::Ref,
        Repr::Int,
        Repr::Int, // 13..=15 f
        Repr::Ref,
        Repr::Int,
        Repr::Int, // 16 k
    ];
    let add = |dst, a, b| Inst::Arith {
        num: cove_ir::Num::Int,
        op: ArithOp::Add,
        dst,
        a,
        b,
    };
    build.function(
        "sumPoints",
        &[boxed],
        &reprs,
        int,
        vec![
            Inst::DynOpen { dst: 1, src: 0 },
            Inst::DynCount { dst: 4, view: 1 },
            Inst::Int { dst: 5, value: 0 },
            Inst::Int { dst: 6, value: 0 },
            Inst::Int { dst: 7, value: 0 },
            Inst::Int { dst: 8, value: 1 },
            Inst::Cmp {
                on: cove_ir::Compare::Int,
                op: CmpOp::Lt,
                dst: 9,
                a: 5,
                b: 4,
            },
            Inst::BranchFalse { cond: 9, to: 17 },
            Inst::DynChild {
                dst: 10,
                view: 1,
                index: 5,
            },
            Inst::DynChild {
                dst: 13,
                view: 10,
                index: 7,
            },
            Inst::DynRead { dst: 16, view: 13 },
            add(6, 6, 16),
            Inst::DynChild {
                dst: 13,
                view: 10,
                index: 8,
            },
            Inst::DynRead { dst: 16, view: 13 },
            add(6, 6, 16),
            add(5, 5, 8),
            Inst::Jump { to: 6 },
            Inst::Return { src: 6 },
        ],
    )
}

/// What holds the view across [`rooted`]'s collections.
#[derive(Clone, Copy)]
enum Hold {
    /// Its own three slots.
    Slot,
    /// Nothing but a `Vector<DynamicView>`; the layout is the program's
    /// `<ref>` word, which the vector's store is read as.
    Vector(LayoutId),
    /// Nothing at all: no view is opened. The control.
    Nothing,
}

/// A function that opens the boxed `m.Inner` it is handed, lets go of every
/// other reference to it, allocates until the heap has been collected, and
/// then answers the `String` in field 0 — read *through the view* after the
/// collection.
///
/// With [`Hold::Vector`] it holds the view in nothing but a
/// `Vector<DynamicView>` — pushed with ADR 0062's window, the view's own slots
/// cleared — and reads it back out of the vector's store after the collection.
/// With [`Hold::Nothing`] it opens no view, and answers nothing: what is
/// looked at is the heap afterwards.
fn rooted(
    build: &mut Build,
    boxed: LayoutId,
    string: LayoutId,
    garbage: LayoutId,
    hold: Hold,
) -> FunctionId {
    let view = build.program.view_layout;
    let reprs = [
        Repr::Ref, // 0 box
        Repr::Int, // 1..=3 v
        Repr::Ref,
        Repr::Int,
        Repr::Int, // 4..=6 field
        Repr::Ref,
        Repr::Int,
        Repr::Int,  // 7 zero
        Repr::Ref,  // 8 garbage
        Repr::Int,  // 9 turns
        Repr::Bool, // 10 more
        Repr::Ref,  // 11 answer
        Repr::Ref,  // 12 the vector
        Repr::Int,  // 13 one
        Repr::Int,  // 14 length
        Repr::Ref,  // 15 store
        Repr::Int,  // 16 the commit's one
    ];
    let mut code = Vec::new();
    if !matches!(hold, Hold::Nothing) {
        code.push(Inst::DynOpen { dst: 1, src: 0 });
    }
    code.extend([
        // The box's only other holder is gone: from here the view's owner
        // word is what keeps the object alive.
        Inst::Clear {
            slot: 0,
            layout: boxed,
        },
        Inst::Int { dst: 7, value: 0 },
    ]);
    if let Hold::Vector(reference) = hold {
        code.extend([
            Inst::GrowableAlloc {
                dst: 12,
                capacity: 7,
                storage: Storage::Words(view),
            },
            Inst::LoadField {
                dst: 14,
                obj: 12,
                at: GROWABLE_LEN,
                layout: build.scalar(Repr::Int),
            },
            Inst::Int { dst: 13, value: 1 },
            Inst::GrowableEnsure {
                owner: 12,
                additional: 13,
                storage: Storage::Words(view),
            },
            Inst::LoadField {
                dst: 15,
                obj: 12,
                at: GROWABLE_STORE,
                layout: reference,
            },
            Inst::StoreElem {
                obj: 15,
                index: 14,
                src: 1,
                layout: view,
            },
            Inst::Clear {
                slot: 15,
                layout: reference,
            },
            Inst::Int { dst: 16, value: 1 },
            Inst::GrowableCommit {
                owner: 12,
                count: 16,
                storage: Storage::Words(view),
            },
            // And now the view's own slots let go too: the vector is the
            // only thing left holding it.
            Inst::Clear {
                slot: 1,
                layout: view,
            },
        ]);
    }
    let top = code.len() as u32;
    code.extend([
        // Twelve thousand-word arrays through a heap of a few thousand words:
        // the collector has to run, several times, to make room.
        Inst::Int { dst: 9, value: 0 },
        Inst::Alloc {
            dst: 8,
            layout: garbage,
            len: Len::Count(1000),
        },
        Inst::ArithImm {
            op: ArithOp::Add,
            dst: 9,
            a: 9,
            value: 1,
        },
        Inst::CmpImm {
            op: CmpOp::Lt,
            dst: 10,
            a: 9,
            value: 12,
        },
        Inst::BranchFalse {
            cond: 10,
            to: top + 6,
        },
        Inst::Jump { to: top + 1 },
        Inst::Clear {
            slot: 8,
            layout: garbage,
        },
    ]);
    if let Hold::Vector(reference) = hold {
        code.extend([
            Inst::LoadField {
                dst: 15,
                obj: 12,
                at: GROWABLE_STORE,
                layout: reference,
            },
            Inst::LoadElem {
                dst: 1,
                obj: 15,
                index: 7,
                layout: view,
            },
        ]);
    }
    if !matches!(hold, Hold::Nothing) {
        code.extend([
            Inst::DynChild {
                dst: 4,
                view: 1,
                index: 7,
            },
            Inst::DynRead { dst: 11, view: 4 },
        ]);
    }
    code.push(Inst::Return { src: 11 });
    let name = match hold {
        Hold::Slot => "rootedInASlot",
        Hold::Vector(_) => "rootedInAVector",
        Hold::Nothing => "heldByNothing",
    };
    build.function(name, &[boxed], &reprs, string, code)
}

/// A machine over the fixture, with room enough that building the values
/// never collects.
fn machine(fixture: &Fixture) -> Machine<'_> {
    Machine::new(&fixture.program, 1 << 16)
}

/// A box holding `words` as a value of `layout`.
fn boxed(machine: &mut Machine, layout: LayoutId, words: &[u64]) -> u64 {
    let object = machine
        .allocate(machine.boxed_layout(), words.len() as i64)
        .expect("a box fits");
    machine.set_payload(object, 0, u64::from(layout.0));
    for (at, word) in words.iter().enumerate() {
        machine.set_payload(object, 1 + at as u32, *word);
    }
    object
}

/// An object of `layout` whose header length is `len` and whose payload is
/// `words`.
fn object(machine: &mut Machine, layout: LayoutId, len: u32, words: &[u64]) -> u64 {
    let object = machine.new_object(layout, len).expect("an object fits");
    for (at, word) in words.iter().enumerate() {
        machine.set_payload(object, at as u32, *word);
    }
    object
}

fn string(machine: &mut Machine, text: &str) -> u64 {
    machine.new_string(text).expect("a string fits")
}

/// A `Vector<Point>` holding `points`.
fn points(machine: &mut Machine, fixture: &Fixture, points: &[(i64, i64)]) -> u64 {
    let owner = machine
        .alloc_vector(fixture.layouts.point, points.len() as i64)
        .expect("a vector fits");
    let store = machine.payload(owner, GROWABLE_STORE);
    for (at, (x, y)) in points.iter().enumerate() {
        machine.set_payload(store, 2 * at as u32, *x as u64);
        machine.set_payload(store, 2 * at as u32 + 1, *y as u64);
    }
    machine.set_payload(owner, GROWABLE_LEN, points.len() as u64);
    owner
}

/// Runs `function` over `args` and answers its words.
fn call(machine: &mut Machine, function: FunctionId, args: &[u64]) -> Vec<u64> {
    machine
        .run(function, args, &budget())
        .unwrap_or_else(|error| panic!("{}", error.message))
}

/// The view of the value `boxed` holds, through `dyn.open`.
fn open(machine: &mut Machine, fixture: &Fixture, boxed: u64) -> Vec<u64> {
    call(machine, fixture.functions.open, &[boxed])
}

/// A view's whole value, described by running the observations over it: the
/// kind's name, the case of an enum after `#`, the children in brackets, and a
/// scalar as its value. `crate::builtins`' tests describe the oracle's values
/// in the same words.
fn describe(machine: &mut Machine, fixture: &Fixture, view: &[u64]) -> String {
    let functions = fixture.functions;
    let one = |machine: &mut Machine, function| call(machine, function, view)[0];
    let code = one(machine, functions.kind) as i64;
    let kind = DynamicKind::from_code(code).unwrap_or_else(|| panic!("kind {code}"));
    match kind {
        DynamicKind::Unit => "()".to_string(),
        DynamicKind::Bool => (one(machine, functions.read_bool) != 0).to_string(),
        DynamicKind::Int => (one(machine, functions.read_int) as i64).to_string(),
        DynamicKind::Float => format!("{:?}", f64::from_bits(one(machine, functions.read_float))),
        DynamicKind::Duration => format!("{}ns", one(machine, functions.read_duration) as i64),
        DynamicKind::String => {
            let text = one(machine, functions.read_string);
            format!(
                "{:?}",
                String::from_utf8(machine.string_bytes(text)).unwrap()
            )
        }
        DynamicKind::Function | DynamicKind::Opaque | DynamicKind::Shared => {
            assert_eq!(one(machine, functions.count), 0, "{}", kind.name());
            kind.name().to_string()
        }
        _ => {
            let mut out = kind.name().to_string();
            if kind == DynamicKind::Enum {
                out.push_str(&format!("#{}", one(machine, functions.case)));
            }
            let count = one(machine, functions.count);
            let children: Vec<String> = (0..count)
                .map(|at| {
                    let child = call(machine, functions.child, &[view, &[at]].concat());
                    describe(machine, fixture, &child)
                })
                .collect();
            out.push_str(&format!("[{}]", children.join(", ")));
            out
        }
    }
}

/// Every kind, boxed, described through the seven instructions: a struct with
/// a nested `String` and a nested struct, an enum case with a payload and
/// without, `Some`/`None` and `Ok`/`Err`, an array, a vector, a set, a map, a
/// range, a closure, a box inside a box, a box inside a field, an opaque
/// struct, and each scalar.
#[test]
fn every_kind_is_described_through_the_instructions() {
    let fixture = fixture();
    let layouts = fixture.layouts;
    let mut machine = machine(&fixture);
    let machine = &mut machine;

    let outer_name = string(machine, "outer");
    let inner_label = string(machine, "inner");
    let named = string(machine, "named");
    let err = string(machine, "no");
    let a = string(machine, "a");
    let b = string(machine, "b");
    let only = string(machine, "only");
    let array = object(machine, layouts.array_int, 3, &[1, 2, 3]);
    let vector = points(machine, &fixture, &[(1, 2), (3, 4)]);
    let set = object(machine, layouts.set_int, 2, &[1, 5]);
    let map = object(machine, layouts.map, 2, &[a, 1, b, 2]);
    let closure = object(machine, layouts.closure, 0, &[0]);
    let cell = object(machine, layouts.shared, 2, &[0, 3]);
    let point = boxed(machine, layouts.point, &[1, 2]);
    let name = string(machine, "n");
    let held = boxed(machine, layouts.inner, &[name, 5, 6]);

    let cases: Vec<(&str, u64)> = vec![
        (
            "struct[\"outer\", struct[\"inner\", struct[3, 4]]]",
            boxed(machine, layouts.outer, &[outer_name, inner_label, 3, 4]),
        ),
        ("enum#0[]", boxed(machine, layouts.mark, &[0, 0, 0])),
        ("enum#1[7]", boxed(machine, layouts.mark, &[1, 7, 0])),
        (
            "enum#2[\"named\"]",
            boxed(machine, layouts.mark, &[2, 0, named]),
        ),
        ("enum#1[5]", boxed(machine, layouts.option_int, &[1, 5])),
        ("enum#0[]", boxed(machine, layouts.option_string, &[0, 0])),
        (
            "enum#1[\"only\"]",
            boxed(machine, layouts.option_string, &[1, only]),
        ),
        ("enum#0[1]", boxed(machine, layouts.result, &[0, 1, 0])),
        (
            "enum#1[\"no\"]",
            boxed(machine, layouts.result, &[1, 0, err]),
        ),
        (
            "Array[1, 2, 3]",
            boxed(machine, layouts.array_int, &[array]),
        ),
        (
            "Vector[struct[1, 2], struct[3, 4]]",
            boxed(machine, layouts.vector_point, &[vector]),
        ),
        ("Set[1, 5]", boxed(machine, layouts.set_int, &[set])),
        (
            "Map[\"a\", 1, \"b\", 2]",
            boxed(machine, layouts.map, &[map]),
        ),
        (
            "Range[1, 4, false]",
            boxed(machine, layouts.range, &[1, 4, 0]),
        ),
        ("function", boxed(machine, layouts.closure, &[closure])),
        // A cell is a kind of its own, and as unreadable as a handle.
        ("Shared", boxed(machine, layouts.shared, &[cell])),
        // A box inside a box is opened through, to the value.
        ("struct[1, 2]", boxed(machine, layouts.boxed, &[point])),
        // And so is a box in a field: the child is the value it holds.
        (
            "struct[struct[\"n\", struct[5, 6]]]",
            boxed(machine, layouts.holder, &[held]),
        ),
        // An `opaque` struct is a struct: its fields are private to the module
        // that declared it, and the standard library's walks are what read a
        // view (ADR 0068's Phase 2 — it was an opaque value in Phase 1).
        ("struct[9]", boxed(machine, layouts.secret, &[9])),
        ("()", boxed(machine, layouts.unit, &[0])),
        ("true", boxed(machine, layouts.boolean, &[1])),
        ("-7", boxed(machine, layouts.int, &[(-7i64) as u64])),
        ("1.5", boxed(machine, layouts.float, &[1.5f64.to_bits()])),
        ("250ns", boxed(machine, layouts.duration, &[250])),
        ("\"s\"", {
            let s = string(machine, "s");
            boxed(machine, layouts.string, &[s])
        }),
    ];
    for (want, value) in cases {
        let view = open(machine, &fixture, value);
        assert_eq!(describe(machine, &fixture, &view), want);
    }
}

/// A view never denotes a box or a bare reference: opening a box of a
/// `String` answers the string object at word 0 under the header's layout,
/// and a struct inside a box is the box at word 1.
#[test]
fn a_view_is_normalised_to_the_value() {
    let fixture = fixture();
    let layouts = fixture.layouts;
    let mut machine = machine(&fixture);
    let text = string(&mut machine, "s");
    let of_string = boxed(&mut machine, layouts.string, &[text]);
    assert_eq!(
        open(&mut machine, &fixture, of_string),
        vec![u64::from(layouts.string.0), text, 0]
    );
    let of_point = boxed(&mut machine, layouts.point, &[1, 2]);
    assert_eq!(
        open(&mut machine, &fixture, of_point),
        vec![u64::from(layouts.point.0), of_point, 1]
    );
    let nested = boxed(&mut machine, layouts.boxed, &[of_point]);
    assert_eq!(
        open(&mut machine, &fixture, nested),
        vec![u64::from(layouts.point.0), of_point, 1]
    );
}

/// `dyn.same-type` is the kind and, for the nominal kinds, the declared name
/// with the instantiation left off — so two instantiations of `Option` are one
/// type, and two structs of the same words are not.
#[test]
fn same_type_is_kind_and_declared_name() {
    let fixture = fixture();
    let layouts = fixture.layouts;
    let mut machine = machine(&fixture);
    let machine = &mut machine;
    let text = string(machine, "x");
    let some_int = boxed(machine, layouts.option_int, &[1, 5]);
    let none_string = boxed(machine, layouts.option_string, &[0, 0]);
    let some_string = boxed(machine, layouts.option_string, &[1, text]);
    let ok = boxed(machine, layouts.result, &[0, 1, 0]);
    let point = boxed(machine, layouts.point, &[1, 2]);
    let range = boxed(machine, layouts.range, &[1, 2, 0]);
    let other_range = boxed(machine, layouts.range, &[5, 9, 1]);
    let int = boxed(machine, layouts.int, &[3]);
    let other_int = boxed(machine, layouts.int, &[4]);
    let duration = boxed(machine, layouts.duration, &[3]);
    let label = string(machine, "l");
    let inner = boxed(machine, layouts.inner, &[label, 1, 2]);
    let rows: Vec<(u64, u64, bool)> = vec![
        (some_int, none_string, true),
        (some_int, some_string, true),
        (some_int, ok, false),
        (point, point, true),
        (point, inner, false),
        (range, other_range, true),
        (range, point, false),
        (int, other_int, true),
        (int, duration, false),
    ];
    for (a, b, want) in rows {
        let (a, b) = (open(machine, &fixture, a), open(machine, &fixture, b));
        let same = call(
            machine,
            fixture.functions.same,
            &[a.clone(), b.clone()].concat(),
        )[0];
        assert_eq!(same != 0, want, "{a:?} and {b:?}");
    }
}

/// Walking every child of a boxed vector of a thousand structs, down to their
/// scalars, allocates nothing: a child is three words computed from its
/// parent's, never an object. ADR 0068's "no child allocation required merely
/// to traverse a value", read off the counter `--stats` reports.
#[test]
fn walking_a_thousand_structs_allocates_nothing() {
    let fixture = fixture();
    let mut machine = machine(&fixture);
    let run: Vec<(i64, i64)> = (0..1000).map(|at| (at, 2 * at)).collect();
    let vector = points(&mut machine, &fixture, &run);
    let value = boxed(&mut machine, fixture.layouts.vector_point, &[vector]);

    let (objects, words) = (machine.allocations(), machine.allocated_words());
    let total = call(&mut machine, fixture.functions.sum_points, &[value])[0] as i64;
    assert_eq!(total, 3 * (0..1000).sum::<i64>());
    assert_eq!(
        machine.allocations() - objects,
        0,
        "objects allocated by the walk"
    );
    assert_eq!(
        machine.allocated_words() - words,
        0,
        "words allocated by the walk"
    );
}

/// The value a boxed `m.Inner` holding `label` is: built before the run, with
/// nothing but the argument word holding the box.
fn rooting_fixture(machine: &mut Machine, fixture: &Fixture, label: &str) -> u64 {
    let text = string(machine, label);
    boxed(machine, fixture.layouts.inner, &[text, 1, 2])
}

/// A view keeps what it reads alive: the box is dropped, the heap collected
/// several times over, and the `String` read through the view afterwards is
/// the one that was there — once with the view in its own slots, and once
/// held by nothing but a `Vector<DynamicView>`.
#[test]
fn a_view_keeps_its_owner_alive_across_a_collection() {
    let fixture = fixture();
    for (function, label) in [
        (fixture.functions.rooted_in_a_slot, "kept by a slot"),
        (fixture.functions.rooted_in_a_vector, "kept by a vector"),
    ] {
        let mut machine = Machine::new(&fixture.program, 4096);
        let value = rooting_fixture(&mut machine, &fixture, label);
        let answer = call(&mut machine, function, &[value])[0];
        assert!(
            machine.collected().collections > 0,
            "{label}: the run collected"
        );
        assert_eq!(machine.string_bytes(answer), label.as_bytes(), "{label}");
    }
}

/// The control for the test above: the same run with no view opened leaves
/// nothing holding the box, and the collections reclaim its `String` and hand
/// the words to the garbage — so the test above is observing the view's root,
/// and not a collector that happened to leave the object where it was.
#[test]
fn without_a_view_the_same_run_reclaims_the_value() {
    let fixture = fixture();
    let label = "kept by nothing";
    let mut machine = Machine::new(&fixture.program, 4096);
    let value = rooting_fixture(&mut machine, &fixture, label);
    let text = machine.payload(value, 1);
    call(&mut machine, fixture.functions.held_by_nothing, &[value]);
    assert!(machine.collected().collections > 0, "the run collected");
    assert_ne!(
        machine.object_layout(text),
        fixture.layouts.string,
        "the string's words were reclaimed and reused"
    );
}

/// A view whose kind disagrees with the question is an internal error rather
/// than a word read as something it is not.
#[test]
fn a_question_the_kind_has_no_answer_to_is_refused() {
    let fixture = fixture();
    let layouts = fixture.layouts;
    let mut machine = machine(&fixture);
    let text = string(&mut machine, "t");
    let of_string = boxed(&mut machine, layouts.string, &[text]);
    let view = open(&mut machine, &fixture, of_string);
    let functions = fixture.functions;
    for (function, args, message) in [
        (
            functions.read_int,
            view.clone(),
            "internal error: a dynamic view of a String was read as a `Int`",
        ),
        (
            functions.case,
            view.clone(),
            "internal error: the case of a dynamic view of a String was asked",
        ),
        (
            functions.child,
            [view.as_slice(), &[0]].concat(),
            "internal error: child 0 of a dynamic view with 0 children was asked",
        ),
    ] {
        let error = machine.run(function, &args, &budget()).unwrap_err();
        assert_eq!(error.message, message);
    }
    let error = machine.run(functions.open, &[text], &budget()).unwrap_err();
    assert_eq!(
        error.message,
        "internal error: a dynamic view was opened on a `String`, which is not an erased value"
    );
}
