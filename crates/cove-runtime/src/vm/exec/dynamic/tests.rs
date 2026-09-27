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

use std::cell::Cell;

use cove_ir::dynamic::{identity_set_layout, identity_table_layout, view_layout};
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
    /// `Vector<Vector<Point>>`: a vector of vectors, each of which is
    /// identity-bearing.
    vector_vectors: LayoutId,
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
    entered_twice: FunctionId,
    identity_across_collections: FunctionId,
    enter_every: FunctionId,
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
    build.layout(
        "Array",
        Shape::Elements {
            elem: vector_point,
            growable: true,
        },
    );
    let vector_vectors = build.layout("Vector", Shape::Vector { elem: vector_point });
    // Issue #514's F4: the identity set and the table beneath it.
    let table = {
        build.program.layouts.push(identity_table_layout());
        LayoutId(build.program.layouts.len() as u32 - 1)
    };
    let identity = {
        build
            .program
            .layouts
            .push(identity_set_layout(table, boolean));
        LayoutId(build.program.layouts.len() as u32 - 1)
    };
    build.program.identity_table_layout = table;
    build.program.identity_set_layout = identity;
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
    let entered_twice = entered_twice(&mut build, boolean);
    let identity_across_collections =
        identity_across_collections(&mut build, boxed, int, array_int);
    let enter_every = enter_every(&mut build, boxed, int);
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
            vector_vectors,
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
            entered_twice,
            identity_across_collections,
            enter_every,
        },
    }
}

/// `fn enteredTwice(v: DynamicView) -> Bool`: makes an identity set, enters
/// `(v, v)` into it twice, and answers what the second entry answered —
/// `false` exactly when the first one entered the pair, which is exactly when
/// `v` is identity-bearing.
fn entered_twice(build: &mut Build, boolean: LayoutId) -> FunctionId {
    let view = build.program.view_layout;
    build.function(
        "enteredTwice",
        &[view],
        &[Repr::Int, Repr::Ref, Repr::Int, Repr::Ref, Repr::Bool],
        boolean,
        vec![
            Inst::DynIdentitySet { dst: 3 },
            Inst::DynIdentityEnter { set: 3, a: 0, b: 0 },
            Inst::DynIdentityEnter { set: 3, a: 0, b: 0 },
            Inst::Return { src: 4 },
        ],
    )
}

/// `fn identityAcrossCollections(a: Any, b: Any) -> Int`: opens two boxed
/// vectors, lets go of the boxes, and enters `(a, b)` into an identity set;
/// allocates until the heap has been collected; and then asks the set again.
/// Answers `0` when every answer was the one it should be, and otherwise the
/// number of the first that was not:
///
/// 1. `(a, b)` is entered;
/// 2. after the collections, `(a, b)` is still in the set;
/// 3. `(b, a)`, the pair swapped, is not — and is entered;
/// 4. once both are left and the heap collected again, `(a, b)` is entered
///    anew.
///
/// The vectors are held by nothing but the views, and the table by nothing but
/// the set's own word: a table the collector did not trace would be reclaimed
/// and handed to the garbage, and answer 2 wrong.
fn identity_across_collections(
    build: &mut Build,
    boxed: LayoutId,
    int: LayoutId,
    garbage: LayoutId,
) -> FunctionId {
    let reprs = [
        Repr::Ref, // 0 a
        Repr::Ref, // 1 b
        Repr::Int, // 2..=4 view of a
        Repr::Ref,
        Repr::Int,
        Repr::Int, // 5..=7 view of b
        Repr::Ref,
        Repr::Int,
        Repr::Ref,  // 8..=9 the set
        Repr::Bool, //
        Repr::Ref,  // 10 garbage
        Repr::Int,  // 11 turns
        Repr::Bool, // 12 more, and a negated answer
        Repr::Int,  // 13 the answer
    ];
    // Twelve thousand-word arrays through a heap of a few thousand words.
    let churn = |top: u32| {
        vec![
            Inst::Int { dst: 11, value: 0 },
            Inst::Alloc {
                dst: 10,
                layout: garbage,
                len: Len::Count(1000),
            },
            Inst::ArithImm {
                op: ArithOp::Add,
                dst: 11,
                a: 11,
                value: 1,
            },
            Inst::CmpImm {
                op: CmpOp::Lt,
                dst: 12,
                a: 11,
                value: 12,
            },
            Inst::BranchFalse {
                cond: 12,
                to: top + 6,
            },
            Inst::Jump { to: top + 1 },
            Inst::Clear {
                slot: 10,
                layout: garbage,
            },
        ]
    };
    let failed = |code: &mut Vec<Inst>, at: u32| {
        // Filled in below, once the refusal arms' counters are known.
        code.push(Inst::BranchFalse { cond: 9, to: at });
    };
    let mut code = vec![
        Inst::DynOpen { dst: 2, src: 0 },
        Inst::DynOpen { dst: 5, src: 1 },
        Inst::Clear {
            slot: 0,
            layout: boxed,
        },
        Inst::Clear {
            slot: 1,
            layout: boxed,
        },
        Inst::DynIdentitySet { dst: 8 },
        Inst::DynIdentityEnter { set: 8, a: 2, b: 5 },
    ];
    let first = code.len();
    failed(&mut code, 0);
    let top = code.len() as u32;
    code.extend(churn(top));
    code.push(Inst::DynIdentityEnter { set: 8, a: 2, b: 5 });
    code.push(Inst::Not { dst: 12, a: 9 });
    let second = code.len();
    code.push(Inst::BranchFalse { cond: 12, to: 0 });
    code.push(Inst::DynIdentityEnter { set: 8, a: 5, b: 2 });
    let third = code.len();
    failed(&mut code, 0);
    code.push(Inst::DynIdentityLeave { set: 8, a: 5, b: 2 });
    code.push(Inst::DynIdentityLeave { set: 8, a: 2, b: 5 });
    let top = code.len() as u32;
    code.extend(churn(top));
    code.push(Inst::DynIdentityEnter { set: 8, a: 2, b: 5 });
    let fourth = code.len();
    failed(&mut code, 0);
    code.push(Inst::Int { dst: 13, value: 0 });
    code.push(Inst::Return { src: 13 });
    for (number, at) in [(1, first), (2, second), (3, third), (4, fourth)] {
        let arm = code.len() as u32;
        code.push(Inst::Int {
            dst: 13,
            value: number,
        });
        code.push(Inst::Return { src: 13 });
        match &mut code[at] {
            Inst::BranchFalse { to, .. } => *to = arm,
            other => unreachable!("{other:?}"),
        }
    }
    build.function(
        "identityAcrossCollections",
        &[boxed, boxed],
        &reprs,
        int,
        code,
    )
}

/// `fn enterEvery(box: Any) -> Int`: opens a boxed `Vector<Vector<Point>>` and,
/// over an identity set, enters `(e, e)` for every element `e` — each a vector,
/// so each a pair of its own — then enters them all again, then leaves them
/// all, then enters them all once more. Answers `0` when the first and last
/// passes entered every pair and the second entered none, and otherwise `1`,
/// `2` or `4` for the pass that went wrong.
///
/// Forty elements grow the table from sixteen slots to a hundred and
/// twenty-eight through three rehashes, and every growth is a place a
/// collection can happen: [`collect_if_asked`] makes one happen there.
fn enter_every(build: &mut Build, boxed: LayoutId, int: LayoutId) -> FunctionId {
    let reprs = [
        Repr::Ref, // 0 box
        Repr::Int, // 1..=3 outer view
        Repr::Ref,
        Repr::Int,
        Repr::Int,  // 4 n
        Repr::Int,  // 5 i
        Repr::Int,  // 6 unused
        Repr::Bool, // 7 more
        Repr::Int,  // 8..=10 element view
        Repr::Ref,
        Repr::Int,
        Repr::Ref,  // 11..=12 the set
        Repr::Bool, //
        Repr::Bool, // 13 a negated answer
        Repr::Int,  // 14 the answer
    ];
    let mut code = vec![
        Inst::DynOpen { dst: 1, src: 0 },
        Inst::DynCount { dst: 4, view: 1 },
        Inst::DynIdentitySet { dst: 11 },
    ];
    // Each pass: `i = 0; while i < n { e = v[i]; <body>; i += 1 }`, with the
    // body's branch to its refusal arm patched once the arms exist.
    let mut patches: Vec<(usize, i64)> = Vec::new();
    for (body, refusal) in [
        (
            Inst::DynIdentityEnter {
                set: 11,
                a: 8,
                b: 8,
            },
            Some((12, 1)),
        ),
        (
            Inst::DynIdentityEnter {
                set: 11,
                a: 8,
                b: 8,
            },
            Some((13, 2)),
        ),
        (
            Inst::DynIdentityLeave {
                set: 11,
                a: 8,
                b: 8,
            },
            None,
        ),
        (
            Inst::DynIdentityEnter {
                set: 11,
                a: 8,
                b: 8,
            },
            Some((12, 4)),
        ),
    ] {
        code.push(Inst::Int { dst: 5, value: 0 });
        let head = code.len() as u32;
        code.push(Inst::Cmp {
            on: cove_ir::Compare::Int,
            op: CmpOp::Lt,
            dst: 7,
            a: 5,
            b: 4,
        });
        let exit = code.len();
        code.push(Inst::BranchFalse { cond: 7, to: 0 });
        code.push(Inst::DynChild {
            dst: 8,
            view: 1,
            index: 5,
        });
        code.push(body);
        if let Some((cond, number)) = refusal {
            if cond == 13 {
                code.push(Inst::Not { dst: 13, a: 12 });
            }
            patches.push((code.len(), number));
            code.push(Inst::BranchFalse { cond, to: 0 });
        }
        code.push(Inst::ArithImm {
            op: ArithOp::Add,
            dst: 5,
            a: 5,
            value: 1,
        });
        code.push(Inst::Jump { to: head });
        let after = code.len() as u32;
        match &mut code[exit] {
            Inst::BranchFalse { to, .. } => *to = after,
            other => unreachable!("{other:?}"),
        }
    }
    code.push(Inst::Int { dst: 14, value: 0 });
    code.push(Inst::Return { src: 14 });
    for (at, number) in patches {
        let arm = code.len() as u32;
        code.push(Inst::Int {
            dst: 14,
            value: number,
        });
        code.push(Inst::Return { src: 14 });
        match &mut code[at] {
            Inst::BranchFalse { to, .. } => *to = arm,
            other => unreachable!("{other:?}"),
        }
    }
    build.function("enterEvery", &[boxed], &reprs, int, code)
}

thread_local! {
    /// Whether [`collect_if_asked`] collects: set by the test that asks for a
    /// collection in the middle of a table's growth.
    static COLLECT_ON_GROWTH: Cell<bool> = const { Cell::new(false) };
    /// How many collections [`collect_if_asked`] has made on this thread.
    static GROWTH_COLLECTIONS: Cell<u64> = const { Cell::new(0) };
}

/// Called by `super::grow` before it allocates a table's replacement — the one
/// point in an entry where a collection can happen — and collects there when a
/// test has asked it to, so that the rehash after it runs over a heap a
/// collection has just walked.
pub(super) fn collect_if_asked(machine: &mut Machine) {
    if COLLECT_ON_GROWTH.with(Cell::get) {
        machine.collect();
        GROWTH_COLLECTIONS.with(|count| count.set(count.get() + 1));
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

/// The audit of placed names answers, both ways: a box holding a struct whose
/// names were not placed is named — the struct, and the structs and the enum
/// inside it — and once the names are placed it finds nothing.
///
/// The survey that runs every program in the repository asserts that the audit
/// found nothing over the whole corpus (`cove-cli`'s `tests/vm_coverage.rs`),
/// and a nothing is only evidence if the audit can say something. This is where
/// it is shown to.
#[test]
fn the_audit_names_a_boxed_layout_whose_names_were_not_placed() {
    let unplaced = |fixture: &Fixture| {
        let mut machine = machine(fixture);
        let label = string(&mut machine, "l");
        let name = string(&mut machine, "n");
        let outer = boxed(&mut machine, fixture.layouts.outer, &[name, label, 1, 2]);
        let mark = boxed(&mut machine, fixture.layouts.mark, &[1, 5, 0]);
        let mut found = super::unplaced_in(&machine, outer);
        found.extend(super::unplaced_in(&machine, mark));
        found.sort();
        found
    };
    let bare = fixture();
    assert_eq!(unplaced(&bare), ["m.Inner", "m.Mark", "m.Outer", "m.Point"]);

    let mut named = fixture();
    let mut names = vec![cove_ir::LayoutNames::default(); named.program.layouts.len()];
    let text = cove_ir::StrId(0);
    for layout in [
        named.layouts.outer,
        named.layouts.inner,
        named.layouts.point,
    ] {
        names[layout.index()] = cove_ir::LayoutNames {
            name: Some(text),
            parts: vec![text, text],
        };
    }
    // An enum's own name is placed too since ADR 0068's Phase 4c: a refused
    // boxed key's path begins with it, `Mark.Weight(0)`. One without it is
    // still named by the audit.
    names[named.layouts.mark.index()] = cove_ir::LayoutNames {
        name: None,
        parts: vec![text, text, text],
    };
    named.program.names = names.clone();
    assert_eq!(unplaced(&named), ["m.Mark"]);
    names[named.layouts.mark.index()].name = Some(text);
    named.program.names = names;
    assert_eq!(unplaced(&named), Vec::<String>::new());
}

/// **The descriptor table compiled code reads is this arm's own answers**, for
/// every layout of the fixture and every pair of them: the kind `dyn.kind`
/// answers, the same-type test `dyn.same-type` answers, and a struct's or a
/// range's field count, which is what `dyn.count` answers of one. A reclaimed
/// layout asks, because this arm refuses it. So a native `dyn.kind` read out of
/// `NativeCtx::dyn_layouts` cannot say what the encoded machine would not
/// (issue #494).
#[test]
fn the_descriptor_table_is_this_arms_own_answers() {
    use super::{descriptors, kind, same_type, View};
    use cove_native::{DYN_ASK, DYN_COUNT_SHIFT, DYN_KIND_MASK, DYN_TYPE_MASK};
    let fixture = fixture();
    let machine = machine(&fixture);
    let table = descriptors(&fixture.program);
    assert_eq!(table.len(), fixture.program.layouts.len());
    let view = |at: usize| View {
        layout: LayoutId(at as u32),
        owner: 1,
        at: 0,
    };
    for (at, described) in fixture.program.layouts.iter().enumerate() {
        if matches!(described.shape, Shape::Free) {
            assert_eq!(table[at], DYN_ASK, "{}", described.name);
            assert!(kind(&machine, view(at)).is_err());
            continue;
        }
        let answered = kind(&machine, view(at)).expect("a layout the program has");
        assert_eq!(
            table[at] & DYN_KIND_MASK,
            answered.code() as u64,
            "{}",
            described.name
        );
        if let Shape::Struct { fields, .. } = &described.shape {
            assert_eq!(table[at] >> DYN_COUNT_SHIFT, fields.len() as u64);
        }
        for (other, beside) in fixture.program.layouts.iter().enumerate() {
            if matches!(beside.shape, Shape::Free) {
                continue;
            }
            assert_eq!(
                table[at] & DYN_TYPE_MASK == table[other] & DYN_TYPE_MASK,
                same_type(&machine, view(at), view(other)).expect("two layouts it has"),
                "{} and {}",
                described.name,
                beside.name
            );
        }
    }
}

/// **The child table compiled code reads is this arm's own answers** (#514
/// F2), both ways.
///
/// Read against the layout table: every layout with children has a block, a
/// struct's entry `i` is field `i`'s layout at its `at`, an enum's case block
/// is that case's parts at `1 + at`, a run's entry is its element at its
/// stride, a map's is its entry stride and then its key and its value — and
/// an entry is marked `DYN_SETTLE` exactly when the child's layout is one
/// address, which is when `settle` has something to do.
///
/// Read against [`super::child`]: every child of every value below, at every
/// index from one before the count to one past it, is answered by
/// [`super::inline_child`] with the view `child` answers when the child is
/// inline — it keeps its parent's owner, or a vector's store — and not at all
/// otherwise; and nothing is answered past the count, of an enum in a case its
/// layout does not have, or of a consumed vector, each of which `child`
/// refuses.
#[test]
fn the_child_table_is_this_arms_own_answers() {
    use super::{child, children, count, inline_child, View};
    use cove_native::{DYN_OFFSET_SHIFT, DYN_SETTLE};
    let fixture = fixture();
    let layouts = fixture.layouts;
    let program = &fixture.program;
    let table = children(program);
    let entry = |layout: LayoutId, offset: u32| {
        let described = program.layout(layout);
        match matches!(described.shape, Shape::Free) || described.is_one_address() {
            true => u64::from(layout.0) | DYN_SETTLE,
            false => u64::from(layout.0) | u64::from(offset) << DYN_OFFSET_SHIFT,
        }
    };
    let width = |layout: LayoutId| program.layout(layout).width();
    for (at, described) in program.layouts.iter().enumerate() {
        let block = table[at] as usize;
        let expected: Vec<u64> = match &described.shape {
            Shape::Struct { fields, .. } if !fields.is_empty() => fields
                .iter()
                .map(|field| entry(field.layout, field.at))
                .collect(),
            Shape::Enum { cases, .. } if !cases.is_empty() => {
                assert_eq!(table[block], cases.len() as u64, "{}", described.name);
                for (nth, case) in cases.iter().enumerate() {
                    let parts = table[block + 1 + nth] as usize;
                    let want: Vec<u64> = std::iter::once(case.parts.len() as u64)
                        .chain(
                            case.parts
                                .iter()
                                .map(|part| entry(part.layout, 1 + part.at)),
                        )
                        .collect();
                    assert_eq!(
                        table[parts..parts + want.len()],
                        want,
                        "{} case {nth}",
                        described.name
                    );
                }
                continue;
            }
            Shape::Elements { elem, .. } | Shape::Members { elem } | Shape::Vector { elem } => {
                vec![entry(*elem, width(*elem))]
            }
            Shape::Entries { key, value } => vec![
                u64::from(width(*key) + width(*value)),
                entry(*key, 0),
                entry(*value, width(*key)),
            ],
            _ => {
                assert_eq!(block, 0, "{} has no block", described.name);
                continue;
            }
        };
        assert!(block >= program.layouts.len(), "{}", described.name);
        assert_eq!(
            table[block..block + expected.len()],
            expected,
            "{}",
            described.name
        );
    }

    let mut machine = machine(&fixture);
    let machine = &mut machine;
    let outer_name = string(machine, "outer");
    let inner_label = string(machine, "inner");
    let named = string(machine, "named");
    let a = string(machine, "a");
    let b = string(machine, "b");
    let array = object(machine, layouts.array_int, 3, &[1, 2, 3]);
    let vector = points(machine, &fixture, &[(1, 2), (3, 4)]);
    let consumed = points(machine, &fixture, &[(5, 6)]);
    machine.set_payload(consumed, GROWABLE_STORE, 0);
    let set = object(machine, layouts.set_int, 2, &[1, 5]);
    let map = object(machine, layouts.map, 2, &[a, 1, b, 2]);
    let name = string(machine, "n");
    let held = boxed(machine, layouts.inner, &[name, 5, 6]);
    let values = [
        boxed(machine, layouts.outer, &[outer_name, inner_label, 3, 4]),
        boxed(machine, layouts.mark, &[0, 0, 0]),
        boxed(machine, layouts.mark, &[1, 7, 0]),
        boxed(machine, layouts.mark, &[2, 0, named]),
        // A case the layout does not have.
        boxed(machine, layouts.mark, &[3, 0, 0]),
        boxed(machine, layouts.option_int, &[1, 5]),
        boxed(machine, layouts.result, &[0, 1, 0]),
        boxed(machine, layouts.array_int, &[array]),
        boxed(machine, layouts.vector_point, &[vector]),
        boxed(machine, layouts.vector_point, &[consumed]),
        boxed(machine, layouts.set_int, &[set]),
        boxed(machine, layouts.map, &[map]),
        boxed(machine, layouts.range, &[1, 4, 0]),
        boxed(machine, layouts.holder, &[held]),
        boxed(machine, layouts.secret, &[9]),
        boxed(machine, layouts.int, &[7]),
    ];
    let mut pending: Vec<View> = values
        .iter()
        .map(|value| {
            let words = open(machine, &fixture, *value);
            View {
                layout: LayoutId(words[0] as u32),
                owner: words[1],
                at: words[2] as u32,
            }
        })
        .collect();
    let (mut inline, mut settled, mut refused) = (0, 0, 0);
    while let Some(view) = pending.pop() {
        // The owner an inline child keeps: a vector's elements are in its store.
        let home = match &program.layout(view.layout).shape {
            Shape::Vector { .. } => machine.payload(view.owner, GROWABLE_STORE),
            _ => view.owner,
        };
        let counted = count(machine, view).unwrap_or(0);
        for index in [-1, counted, counted + 1].into_iter().chain(0..counted) {
            let fast = inline_child(machine, view, index);
            match child(machine, view, index) {
                Ok(answer) if answer.owner == home => {
                    assert_eq!(fast, Some(answer), "{view:?} child {index}");
                    inline += 1;
                    pending.push(answer);
                }
                Ok(answer) => {
                    assert_eq!(fast, None, "{view:?} child {index} settles");
                    settled += 1;
                    pending.push(answer);
                }
                Err(_) => {
                    assert_eq!(fast, None, "{view:?} child {index} is refused");
                    refused += 1;
                }
            }
        }
    }
    assert!(inline >= 20, "{inline} inline children");
    assert!(settled >= 5, "{settled} children that settle");
    assert!(refused >= 30, "{refused} refusals");
}

// ---- issue #514's F4: the identity set --------------------------------------

/// **Only a whole `Vector` is identity-bearing.** Entering `(v, v)` twice
/// answers `false` the second time for a vector — it was entered — and `true`
/// for every other kind: a struct, an array, a set, a map, a string, a scalar,
/// an enum and a box's struct are entered by nothing. And a set nothing was
/// entered into allocates nothing: an empty set is two words and no object.
#[test]
fn only_a_whole_vector_is_identity_bearing() {
    let fixture = fixture();
    let layouts = fixture.layouts;
    let mut machine = machine(&fixture);
    let machine = &mut machine;
    let vector = points(machine, &fixture, &[(1, 2)]);
    let array = object(machine, layouts.array_int, 2, &[1, 2]);
    let set = object(machine, layouts.set_int, 1, &[1]);
    let text = string(machine, "s");
    let cases: Vec<(&str, u64, bool)> = vec![
        (
            "a vector",
            boxed(machine, layouts.vector_point, &[vector]),
            true,
        ),
        (
            "an array",
            boxed(machine, layouts.array_int, &[array]),
            false,
        ),
        ("a set", boxed(machine, layouts.set_int, &[set]), false),
        ("a struct", boxed(machine, layouts.point, &[1, 2]), false),
        (
            "an enum",
            boxed(machine, layouts.option_int, &[1, 5]),
            false,
        ),
        ("a string", boxed(machine, layouts.string, &[text]), false),
        ("an Int", boxed(machine, layouts.int, &[3]), false),
    ];
    for (what, value, bearing) in cases {
        let view = open(machine, &fixture, value);
        let (objects, words) = (machine.allocations(), machine.allocated_words());
        let second = call(machine, fixture.functions.entered_twice, &view)[0] != 0;
        assert_eq!(second, !bearing, "{what}");
        if !bearing {
            assert_eq!(
                (
                    machine.allocations() - objects,
                    machine.allocated_words() - words
                ),
                (0, 0),
                "{what}: an entry of nothing allocated"
            );
        } else {
            assert_eq!(
                machine.allocations() - objects,
                1,
                "{what}: the first entry allocates the table, and only it"
            );
        }
    }
    // An element of a vector is not the vector: a struct in a vector's store
    // is inline in it, and is entered by nothing.
    let owner = boxed(machine, layouts.vector_point, &[vector]);
    let whole = open(machine, &fixture, owner);
    let element = call(
        machine,
        fixture.functions.child,
        &[whole.as_slice(), &[0]].concat(),
    );
    assert!(call(machine, fixture.functions.entered_twice, &element)[0] != 0);
}

/// Condition 4's first half: **a collection between an entry and its
/// leaving** changes nothing the set answers. The two vectors are rooted by
/// nothing but the views that name them and the table by nothing but the set's
/// own word, and after the heap has been collected several times over the pair
/// is still in the set, the swapped pair is not, and once both are left the
/// pair is entered anew.
#[test]
fn an_identity_set_answers_the_same_across_a_collection() {
    let fixture = fixture();
    let layouts = fixture.layouts;
    let mut machine = Machine::new(&fixture.program, 4096);
    let left = points(&mut machine, &fixture, &[(1, 2)]);
    let right = points(&mut machine, &fixture, &[(3, 4)]);
    let a = boxed(&mut machine, layouts.vector_point, &[left]);
    let b = boxed(&mut machine, layouts.vector_point, &[right]);
    let answer = call(
        &mut machine,
        fixture.functions.identity_across_collections,
        &[a, b],
    )[0];
    assert!(
        machine.collected().collections >= 2,
        "the run collected between the entries"
    );
    assert_eq!(answer, 0, "the answer that went wrong");
}

/// Condition 4's second half: **a collection during a rehash.** Forty vectors
/// are entered, which grows the table three times, and a collection is made at
/// every growth — after the old table's pairs were decided on and before the
/// new table is allocated and filled. Every pair is then found again, none is
/// entered twice, all are left, and all are entered anew.
///
/// The elements are rooted only through the outer vector, which the box and
/// the view keep; the table only through the set's word. A table the collector
/// reclaimed mid-growth would be read as garbage by the rehash.
#[test]
fn an_identity_set_answers_the_same_across_a_collection_in_a_rehash() {
    let fixture = fixture();
    let layouts = fixture.layouts;
    let mut machine = Machine::new(&fixture.program, 1 << 14);
    let outer = machine
        .alloc_vector(layouts.vector_point, 40)
        .expect("a vector fits");
    for at in 0..40u32 {
        let inner = points(&mut machine, &fixture, &[(i64::from(at), 0)]);
        let store = machine.payload(outer, GROWABLE_STORE);
        machine.set_payload(store, at, inner);
    }
    machine.set_payload(outer, GROWABLE_LEN, 40);
    let value = boxed(&mut machine, layouts.vector_vectors, &[outer]);

    COLLECT_ON_GROWTH.with(|on| on.set(true));
    GROWTH_COLLECTIONS.with(|count| count.set(0));
    let answer = call(&mut machine, fixture.functions.enter_every, &[value])[0];
    COLLECT_ON_GROWTH.with(|on| on.set(false));
    let collections = GROWTH_COLLECTIONS.with(Cell::get);
    assert_eq!(answer, 0, "the pass that went wrong");
    // Sixteen slots at the first pair, then thirty-two at the ninth,
    // sixty-four at the seventeenth and a hundred and twenty-eight at the
    // thirty-third: four allocations, and a collection at each.
    assert_eq!(collections, 4, "a collection at every growth");
    assert!(machine.collected().collections >= 4);
}

/// An entry that would allocate is refused where nothing may: [`execute`],
/// which the native tier's leaf call reaches, is handed an entry into a set
/// with no table, which only a disagreement between the tier and the runtime
/// could do — so it is the internal error, and not an allocation behind
/// compiled code's back.
#[test]
fn an_entry_that_needs_room_is_refused_where_nothing_may_allocate() {
    let fixture = fixture();
    let layouts = fixture.layouts;
    let mut machine = machine(&fixture);
    let vector = points(&mut machine, &fixture, &[(1, 2)]);
    let value = boxed(&mut machine, layouts.vector_point, &[vector]);
    let view = open(&mut machine, &fixture, value);
    // A frame of our own: the view at 0..=2 and a set at 3..=4.
    let base = machine.mem.push_frame(8).expect("a frame fits");
    let at = machine.mem.stack_index(base);
    for (offset, word) in view.iter().enumerate() {
        machine.mem.set_word_at(at + offset, *word);
    }
    let refused = super::identity_enter(&mut machine, at, 3, 0, 0, false)
        .expect_err("no table, and no room to make one");
    assert!(
        refused.message.contains("where nothing may allocate"),
        "{}",
        refused.message
    );
    assert!(super::enter_allocates(&machine, at + 3));
    super::identity_enter(&mut machine, at, 3, 0, 0, true).expect("room to make one");
    assert!(!super::enter_allocates(&machine, at + 3));
    assert_eq!(machine.mem.word_at(at + 4), 1, "entered");
    super::identity_enter(&mut machine, at, 3, 0, 0, false).expect("found, and nothing to add");
    assert_eq!(machine.mem.word_at(at + 4), 0, "already there");
}
