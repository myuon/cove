//! What an expansion leaves behind, asserted against the counters it names.
//!
//! `super::super::inline` writes no listing of its own — what it does is
//! visible in every other file here, because a lowered call to a small leaf is
//! not a call any more. What is *not* visible anywhere else is
//! [`Inlined`](crate::program::Inlined): nothing in a lowered function reads
//! it, so a range that has drifted still verifies, still runs, and still
//! answers. It answers about the wrong instructions, which only an error
//! chain or a backtrace would ever notice.

use cove_schema::HostSchemas;

use super::super::inline;
use super::checked;
use crate::{lower, ArithOp, Function, FunctionId, Inst, Program, RefMap, Repr};

/// The lowered program, and `m.main` within it.
fn program(source: &str) -> (Program, Function) {
    let (sources, held) = checked(source);
    let program = lower(&held, &sources, &HostSchemas::new()).expect("the program lowers");
    let main = program
        .functions
        .iter()
        .find(|f| &*f.module == "m" && &*f.name == "main")
        .expect("`main` was lowered")
        .clone();
    (program, main)
}

/// A call to a small leaf is gone, and what took its place says where it
/// was called from.
///
/// The two halves are one fact. An expansion that removed the call without
/// recording it would pass the first assertion and leave an error raised
/// inside the expanded body with no caller to name — which is what the
/// oracle reports and the machine used not to.
#[test]
fn an_expanded_call_records_the_site_it_removed() {
    let (_, main) = program(
        "fn divide(a: Int, b: Int) -> Int {\n  a / b\n}\nfn main() -> Int {\n  divide(10, 0)\n}",
    );
    assert!(
        !main
            .code
            .iter()
            .any(|inst| matches!(inst, Inst::Call { .. })),
        "the call was expanded"
    );
    let at = main
        .code
        .iter()
        .position(|inst| {
            matches!(
                inst,
                Inst::Arith {
                    op: ArithOp::Div,
                    ..
                }
            )
        })
        .expect("the leaf's division is in `main` now");
    let sites: Vec<_> = main.inlined_at(at as u32).map(|held| held.site).collect();
    assert_eq!(sites.len(), 1, "one expansion covers the division");
}

/// The range still names the division after the passes that renumber.
///
/// `super::super::inline` runs before `tails` and `frees`, both of which
/// drop [`Inst::Clear`]s and move every counter after them. `dropping::rewrite`
/// moves a `Local`'s pair and has to move an `Inlined`'s the same way; when
/// it did not, this program's range sat two counters past the division and
/// the error it raises reported no caller at all.
///
/// The `var` is what makes the difference: it is a reference the caller
/// clears, so there are clears between the top of `main` and the expansion
/// for `frees` to take out.
#[test]
fn an_expansion_s_range_survives_the_passes_that_renumber() {
    let (_, plain) = program(
        "fn divide(a: Int, b: Int) -> Int {\n  a / b\n}\nfn main() -> Int {\n  divide(10, 0)\n}",
    );
    let (_, after) = program(
        "fn divide(a: Int, b: Int) -> Int {\n  a / b\n}\n\
         fn main() -> Int {\n  let s = \"ab\"\n  divide(s.length(), 0)\n}",
    );
    let division = |f: &Function| {
        f.code
            .iter()
            .position(|inst| {
                matches!(
                    inst,
                    Inst::Arith {
                        op: ArithOp::Div,
                        ..
                    }
                )
            })
            .expect("the leaf's division is in `main` now")
    };
    assert!(
        division(&after) > division(&plain),
        "the second program has instructions before the expansion"
    );
    assert_eq!(
        after.inlined_at(division(&after) as u32).count(),
        1,
        "the recorded range still covers the division it was made for"
    );
}

/// A body that can fail is expanded.
///
/// This is the rule [`Inlined`](crate::program::Inlined) removed, pinned so
/// that it is not put back by accident: `divide` divides, a division by zero
/// stops the run, and the pass expands it anyway because the record is what
/// keeps the chain rather than the frame.
#[test]
fn a_leaf_that_can_fail_is_expanded_because_the_record_keeps_its_caller() {
    let (_, main) = program(
        "fn divide(a: Int, b: Int) -> Int {\n  a / b\n}\nfn main() -> Int {\n  divide(10, 0)\n}",
    );
    assert!(!main
        .code
        .iter()
        .any(|inst| matches!(inst, Inst::Call { .. })));
}

/// The names an expanded body bound are recorded with it, not merged into
/// the caller's table.
///
/// The two cannot be told apart afterwards, which is the whole reason. In
/// `raise` below, `let raised = n + 1` binds at the counter the expansion
/// begins at — the argument needed no copy, so the body's first instruction
/// *is* that counter — and `twice`'s own parameter `n` is bound there too.
/// A reader sorting one table by "which expansion contains this counter"
/// would put both in the same body, and a debugger stopped inside `twice`
/// would offer a name `twice` never bound.
#[test]
fn an_expansion_records_the_names_the_expanded_body_bound() {
    let (program, _) = program(
        "fn twice(n: Int) -> Int {\n  let doubled = n * 2\n  doubled\n}\n\
         fn raise(n: Int) -> Int {\n  let raised = n + 1\n  twice(raised)\n}\n\
         fn main() -> Int { raise(1) }",
    );
    let raise = program
        .functions
        .iter()
        .find(|f| &*f.module == "m" && &*f.name == "raise")
        .expect("`raise` was lowered");
    let held = raise
        .inlined
        .first()
        .expect("`twice` was expanded into `raise`");
    let mut names: Vec<&str> = held.locals.iter().map(|local| &*local.name).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["doubled", "n"],
        "the expansion carries the callee's names and only the callee's"
    );
    assert!(
        raise.locals.iter().all(|local| &*local.name != "doubled"),
        "and the caller's table is not where they went"
    );
}

/// A caller built by hand around one call to `callee`, with a frame of exactly
/// `words` words: the answer in slot zero, the arguments after it, then `Int`
/// padding.
///
/// The lowering chooses a caller's frame, and a test that is about where a
/// budget rule falls needs to choose it instead — so the caller is written
/// out, appended to a lowered program, and expanded by
/// `inline::expand_cold`, which is the pass's own `expand` over one function.
fn caller_of(program: &mut Program, callee: &str, words: usize) -> FunctionId {
    let at = program
        .functions
        .iter()
        .position(|f| f.qualified() == callee)
        .unwrap_or_else(|| panic!("`{callee}` was lowered"));
    let leaf = program.functions[at].clone();
    let answer = program.layout(leaf.returns).width();
    let mut args = Vec::new();
    let mut slot = answer;
    for layout in &leaf.params {
        args.push(crate::program::Arg {
            slot,
            layout: *layout,
        });
        slot += program.layout(*layout).width();
    }
    let mut reprs = vec![Repr::Int; slot as usize];
    let mut param = 0;
    for arg in &args {
        let width = program.layout(arg.layout).width() as usize;
        let first = arg.slot as usize;
        reprs[first..first + width].copy_from_slice(&leaf.reprs[param..param + width]);
        param += width;
    }
    assert!(
        words >= reprs.len(),
        "a frame of {words} cannot hold the call"
    );
    reprs.resize(words, Repr::Int);
    let listed = program.args.len() as u32;
    program.args.push(args);
    let mut caller = leaf.clone();
    caller.name = "caller".into();
    caller.params = Vec::new();
    caller.refs = RefMap::of(&reprs);
    caller.reprs = reprs;
    caller.code = vec![
        Inst::Call {
            dst: 0,
            callee: FunctionId(at as u32),
            args: crate::ArgsId(listed),
        },
        Inst::Return { src: 0 },
    ];
    caller.spans = vec![leaf.span; 2];
    caller.locals = Vec::new();
    caller.inlined = Vec::new();
    program.functions.push(caller);
    FunctionId(program.functions.len() as u32 - 1)
}

/// Whether the hand-built caller still calls something.
fn still_calls(program: &Program, id: FunctionId) -> bool {
    program
        .function(id)
        .code
        .iter()
        .any(|inst| matches!(inst, Inst::Call { .. }))
}

/// The frame budget is charged the words an expansion appends, not the
/// callee's whole frame.
///
/// `offset` never writes its four parameters, so an expansion reads them
/// where the caller has them and appends only the rest of the frame. A caller
/// with exactly that much room left expands it and ends at the budget to the
/// word; one word less and it is left a call. Charged by the whole frame — the
/// rule this replaced — the first caller was refused too.
#[test]
fn the_frame_budget_is_charged_what_an_expansion_appends() {
    let (mut program, _) = program(
        "fn offset(a: Int, b: Int, c: Int, d: Int) -> Int {\n  let x = a + b\n  let y = c * d\n  x - y\n}\n\
         fn main() -> Int { offset(1, 2, 3, 4) }",
    );
    let leaf = program
        .functions
        .iter()
        .find(|f| f.qualified() == "m.offset")
        .expect("`offset` was lowered")
        .clone();
    let appended = inline::appended_words(&program, &leaf, false);
    assert!(
        appended > 0 && appended + 4 <= leaf.reprs.len(),
        "the parameters are not charged: {appended} of {}",
        leaf.reprs.len()
    );

    let fits = caller_of(&mut program, "m.offset", inline::FRAME_BUDGET - appended);
    inline::expand_cold(&mut program, fits);
    assert!(
        !still_calls(&program, fits),
        "the call that fits is expanded"
    );
    assert_eq!(
        program.function(fits).reprs.len(),
        inline::FRAME_BUDGET,
        "and it appended exactly what it was charged"
    );

    let over = caller_of(
        &mut program,
        "m.offset",
        inline::FRAME_BUDGET - appended + 1,
    );
    inline::expand_cold(&mut program, over);
    assert!(
        still_calls(&program, over),
        "one word past the budget, it is left a call"
    );
}

/// A thin standard-library wrapper is expanded in a caller that has spent
/// its whole budget, and the same body declared by the program is not.
///
/// `std.duration.millis` is a builtin read and a division, which is what
/// `is_thin_library` admits whatever the caller holds. `millisOf` is that body
/// word for word in the program's own module, and a caller already over the
/// budget leaves it a call — so what decided was whose function it is, not
/// its shape.
#[test]
fn a_thin_library_wrapper_is_expanded_past_the_budget() {
    let (mut program, _) = program(
        "fn millisOf(duration: Duration) -> Int {\n  duration.nanos() / 1_000_000\n}\n\
         fn main() -> Int {\n  let d = Duration.seconds(1)\n  d.millis() + millisOf(d)\n}",
    );
    let library = program
        .functions
        .iter()
        .find(|f| f.qualified() == "std.duration.millis")
        .expect("`std.duration.millis` was lowered");
    assert!(
        inline::is_thin_library(&program, library),
        "{:?}",
        library.code
    );

    let wide = inline::FRAME_BUDGET + 8;
    let thin = caller_of(&mut program, "std.duration.millis", wide);
    inline::expand_cold(&mut program, thin);
    assert!(
        !still_calls(&program, thin),
        "the library's wrapper is expanded"
    );

    let own = caller_of(&mut program, "m.millisOf", wide);
    let mine = program
        .functions
        .iter()
        .find(|f| f.qualified() == "m.millisOf")
        .expect("`millisOf` was lowered");
    assert!(!inline::is_thin_library(&program, mine));
    inline::expand_cold(&mut program, own);
    assert!(
        still_calls(&program, own),
        "the program's own body of the same shape is left a call"
    );
}

/// `Vector.push` is still a thin wrapper now that its body is ADR 0062's
/// window, because `is_thin_library` counts a recognised window as one step.
///
/// The body is eight instructions and a `unit` — past `THIN` by any count of
/// instructions — so without that rule a caller that had spent its budget
/// would leave every push a call, and a push that was one dispatch would be a
/// frame. The caller here is over the budget, and the push is expanded.
#[test]
fn a_push_window_is_one_step_of_a_thin_wrapper() {
    let (mut program, _) = program(
        "fn main() -> Int {\n  var xs: Vector<Int> = Vector.of()\n  xs.push(1)\n  xs.length()\n}",
    );
    let push = program
        .functions
        .iter()
        .find(|f| f.qualified() == "std.vector.push<Int>")
        .expect("`std.vector.push<Int>` was lowered");
    assert!(
        push.code.len() > 5,
        "the body is more instructions than THIN: {:?}",
        push.code
    );
    assert_eq!(
        crate::legalize::windows(&program, push).len(),
        1,
        "the body is one window: {:?}",
        push.code
    );
    assert!(inline::is_thin_library(&program, push), "{:?}", push.code);

    let wide = inline::FRAME_BUDGET + 8;
    let caller = caller_of(&mut program, "std.vector.push<Int>", wide);
    inline::expand_cold(&mut program, caller);
    assert!(!still_calls(&program, caller), "the push is expanded");
    assert_eq!(
        crate::legalize::windows(&program, program.function(caller)).len(),
        1,
        "and is still one window where it was expanded"
    );
}

/// Every ensure and every commit the standard library writes is inside a window
/// `crate::legalize` recognises, in the body that writes it and wherever that
/// body is expanded.
///
/// ADR 0062 asks for this pin by name: a window that stops matching is still
/// correct primitive IR, so a body edit that put one more row inside it — a
/// `unit` a statement wrote, a constant into the wrong slot — would pass every
/// other test and cost each push its fused dispatch in silence. The program
/// reaches every standard-library function that appends one element:
/// `Vector.push` at a one-word and a two-word element, `Map.of`, `Map.inserted`,
/// `Map.keys`, `Map.values`, `Set.of` and `Set.inserted`. And every one that
/// appends bytes: `StringBuilder.append` and `appendByte`, the `appendText` and
/// `appendByteInto` they call, an interpolation's literal of one byte and of
/// several, its `String` piece, and `std.int.renderInto` for its `Int` pieces,
/// both where it is written and expanded into a loop. And `appendSlice`, whose
/// copy is the same window once `appendRange` has decided its range.
#[test]
fn every_append_the_standard_library_writes_is_a_window() {
    let (program, _) = program(
        "use std.stringbuilder.StringBuilder\n\
         struct Point { x: Int, y: Int }\n\
         fn main() -> Int {\n  \
           var xs: Vector<Int> = Vector.of()\n  xs.push(1)\n  \
           var ps: Vector<Point> = Vector.of()\n  ps.push(Point(x: 1, y: 2))\n  \
           let m = Map.of(MapEntry(key: \"b\", value: 2), MapEntry(key: \"a\", value: 1))\n  \
           let more = m.inserted(\"c\", 3)\n  \
           let s = Set.of(3, 1, 2)\n  let bigger = s.inserted(4)\n  \
           var out = StringBuilder.withCapacity(4)\n  out.append(\"ab\")\n  out.appendByte(99)\n  \
           out.appendSlice(\"cde\", 1, 3)\n  \
           let size = out.length()\n  var bytes = out.finish().byteLength()\n  var i = 0\n  \
           while i < 3 {\n    bytes = bytes + \"<{s.length()}>, {size} and {\"x\"}\".byteLength()\n    i = i + 1\n  }\n  \
           more.keys().length() + more.values().length() + bigger.length() + xs.length() + ps.length() + bytes\n}",
    );
    let mut writers = std::collections::BTreeSet::new();
    let mut windows = 0;
    for f in &program.functions {
        let found = crate::legalize::windows(&program, f);
        let inside = |pc: usize| {
            found
                .iter()
                .any(|window| (window.head..window.head + window.rows).contains(&pc))
        };
        for (pc, inst) in f.code.iter().enumerate() {
            if matches!(
                inst,
                Inst::GrowableEnsure { .. } | Inst::GrowableCommit { .. }
            ) {
                assert!(
                    inside(pc),
                    "{} +{pc} is not inside a window:\n{}",
                    f.qualified(),
                    crate::print::function(&program, program_id(&program, f))
                );
                let name = f.qualified();
                writers.insert(name.split('<').next().unwrap_or(&name).to_string());
            }
        }
        windows += found.len();
    }
    for writer in [
        "m.main",
        "std.vector.push",
        "std.map.inserted",
        "std.map.keys",
        "std.map.values",
        "std.map.placeAt",
        "std.set.inserted",
        "std.set.placeAt",
        "std.stringbuilder.appendText",
        "std.stringbuilder.appendRange",
        "std.stringbuilder.appendByteInto",
        "std.stringbuilder.StringBuilder.append",
        "std.stringbuilder.StringBuilder.appendByte",
        "std.int.renderInto",
    ] {
        assert!(
            writers.contains(writer),
            "`{writer}` holds an append: {writers:?}"
        );
    }
    assert!(windows >= writers.len(), "{windows} window(s)");
}

/// Nothing publishes a growable owner's length with a field store.
///
/// ADR 0062's safety contract, as a property of the lowered program: "the
/// destination is not visible before initialization. Length changes only at
/// commit." A `StoreField` is the instruction every struct write uses, so the
/// verifier cannot be given a rule about it — it would have to decide which
/// field store is a commit by the offset it writes, a nominal exception inside
/// a general instruction, which is the alternative the ADR rejects. What is
/// left is this: a lowering test, over every function a program that reaches
/// each appending body lowers.
///
/// The rule is that **a field store at a growable owner's length offset may
/// only write a constant `0`**, and that is the distinction that matters.
/// Writing `0` into an owner an `Alloc` has just made publishes nothing —
/// there is nothing above the length to publish. Writing `length + count`
/// publishes units the store may not hold, which is what `core.extendFromSet`
/// and `core.extendFromMap` did until this stage replaced them with an ensure,
/// a copy and a commit.
///
/// The constructions that remain are `core.vectorWithCapacity`'s, and they are
/// counted rather than merely tolerated, so that this cannot pass by finding
/// nothing. Removing them is admitting `GrowableAlloc{Words}`, which ADR 0062
/// leaves as separate and measured work.
///
/// Both approximations here fail rather than pass. A slot is taken for an
/// owner if it is *ever* one in the function, and a slot is taken for a
/// constant `0` only within the block that wrote it.
#[test]
fn no_field_store_publishes_a_growable_owner_s_length() {
    let (program, _) = program(
        "use std.stringbuilder.StringBuilder\n\
         fn main() -> Int {\n  \
           var xs: Vector<Int> = Vector.of()\n  xs.push(1)\n  \
           let m = Map.of(MapEntry(key: \"b\", value: 2), MapEntry(key: \"a\", value: 1))\n  \
           let more = m.inserted(\"c\", 3)\n  let fewer = more.removed(\"a\")\n  \
           let s = Set.of(3, 1, 2)\n  let bigger = s.inserted(4)\n  \
           let smaller = bigger.removed(1)\n  \
           var out = StringBuilder.withCapacity(4)\n  out.append(\"ab\")\n  \
           out.appendSlice(\"cde\", 1, 3)\n  out.appendByte(99)\n  \
           fewer.keys().length() + smaller.length() + xs.length() +\n    \
             out.finish().byteLength()\n}",
    );
    let owned = |layout| {
        matches!(
            program.layout(layout).shape,
            crate::layout::Shape::Vector { .. } | crate::layout::Shape::ByteBuffer
        )
    };
    let mut constructions = 0;
    for f in &program.functions {
        let mut owner = vec![false; f.reprs.len()];
        for inst in &f.code {
            match inst {
                Inst::Alloc { dst, layout, .. } if owned(*layout) => owner[*dst as usize] = true,
                Inst::GrowableAlloc { dst, .. } => owner[*dst as usize] = true,
                Inst::GrowableEnsure { owner: at, .. }
                | Inst::GrowableCommit { owner: at, .. }
                | Inst::GrowableTruncate { owner: at, .. }
                | Inst::RunFinish { owner: at, .. } => owner[*at as usize] = true,
                _ => {}
            }
        }
        let leaders = crate::flow::leaders(&program, f);
        let mut zero = vec![false; f.reprs.len()];
        for (pc, inst) in f.code.iter().enumerate() {
            if leaders[pc] {
                zero.iter_mut().for_each(|held| *held = false);
            }
            if let Inst::StoreField { obj, at, src, .. } = inst {
                if *at == crate::legalize::LENGTH && owner[*obj as usize] {
                    assert!(
                        zero[*src as usize],
                        "{} +{pc} publishes a growable owner's length with a field store:\n{}",
                        f.qualified(),
                        crate::print::function(&program, program_id(&program, f))
                    );
                    constructions += 1;
                }
            }
            inst.writes(&program, &mut |slot, width| {
                for at in slot..slot + width {
                    if let Some(held) = zero.get_mut(at as usize) {
                        *held = false;
                    }
                }
            });
            if let Inst::Int { dst, value } = inst {
                zero[*dst as usize] = *value == 0;
            }
        }
    }
    assert!(
        constructions > 0,
        "`core.vectorWithCapacity`'s zero is still written somewhere"
    );
}

fn program_id(program: &Program, f: &Function) -> FunctionId {
    let at = program
        .functions
        .iter()
        .position(|held| std::ptr::eq(held, f))
        .expect("the function is the program's");
    FunctionId(at as u32)
}

/// A standard-library method that takes `var self` is expanded where it is
/// called, and a program's own function with a `var` parameter is not.
///
/// The builder's four `var self` methods are a load of the owner through the
/// address and what the owner is handed to — one byte run instruction for
/// `finish`, and for `append` and `appendByte` a call of `appendText` or
/// `appendByteInto`, which is ADR 0062's window and is expanded in its turn —
/// which is what `examples/covefmt` made half a million calls a run to (#378,
/// Phase 3 Q7). What is expanded is the body *with* its address: the owner is
/// still read through it, so an append in the caller's frame reaches the
/// builder the caller named.
///
/// **`appendSlice` is the one that is not expanded here**, and that is
/// `inline::LIMIT` rather than anything about its shape. ADR 0062 moved its
/// range policy out of `core.bytesExtend` and into Cove, so its body is five
/// questions and a window instead of one instruction, and a run of thirty rows
/// is over the limit a *cold* site expands at. It is under `inline::HOT_LIMIT`,
/// so the sites that run often — every `appendSlice` `examples/covefmt` makes —
/// expand it, which `an_append_slice_inside_a_loop_is_expanded_as_one_window`
/// pins and which `--boundary`'s unexpanded count is what actually measures.
#[test]
fn a_library_method_that_takes_var_self_is_expanded() {
    let (program, main) = program(
        "use std.stringbuilder.StringBuilder\n\
         fn bump(var x: Int) {\n  x = x + 1\n}\n\
         fn main() -> String {\n  var n = 0\n  bump(var n)\n  var out = StringBuilder.withCapacity(4)\n  \
         out.append(\"a\")\n  out.appendSlice(\"bcd\", 1, 2)\n  out.appendByte(100)\n  out.finish()\n}",
    );
    let named = |id: FunctionId| program.function(id).qualified();
    let calls: Vec<String> = main
        .code
        .iter()
        .filter_map(|inst| match inst {
            Inst::Call { callee, .. } => Some(named(*callee)),
            _ => None,
        })
        .collect();
    assert_eq!(
        calls,
        ["m.bump", "std.stringbuilder.StringBuilder.appendSlice"],
        "the program's own `var` function, and the one library method a cold \
         site is too small to take on"
    );
    for method in ["append", "appendByte", "finish"] {
        assert!(
            main.inlined.iter().any(|held| {
                let name = named(held.callee);
                name.starts_with("std.stringbuilder.") && name.ends_with(&format!(".{method}"))
            }),
            "`StringBuilder.{method}` was expanded into `main`"
        );
    }
    let has = |wanted: fn(&Inst) -> bool| main.code.iter().any(wanted);
    assert!(has(|inst| matches!(inst, Inst::RunFinish { .. })));
    let patterns: Vec<crate::legalize::Pattern> = crate::legalize::windows(&program, &main)
        .iter()
        .map(|window| window.pattern)
        .collect();
    assert_eq!(
        patterns,
        [
            crate::legalize::Pattern::AppendBytes,
            crate::legalize::Pattern::PushByte
        ],
        "`append` and `appendByte` are each one window where they were called"
    );
    assert!(
        has(|inst| matches!(inst, Inst::Load { .. })),
        "the owner is read through the address the caller formed"
    );
}

/// An `appendSlice` at a site that runs often is expanded, and what lands there
/// is one recognised append window.
///
/// The other half of the test above. `appendSlice` costs more rows than a cold
/// site will take since ADR 0062 put its range policy in Cove, and the whole
/// point of putting it there is that the copy beneath the five questions is a
/// window a backend fuses — which is worth nothing if the method stays a call
/// wherever it is hot. `examples/covefmt` calls it two hundred thousand times
/// from inside its printer's walk, and that is this shape.
#[test]
fn an_append_slice_inside_a_loop_is_expanded_as_one_window() {
    let (program, main) = program(
        "use std.stringbuilder.StringBuilder\n\
         fn main() -> String {\n  var out = StringBuilder.withCapacity(4)\n  \
         var at = 0\n  while at < 3 {\n    out.appendSlice(\"bcd\", at, at + 1)\n    at = at + 1\n  }\n  \
         out.finish()\n}",
    );
    assert!(
        !main
            .code
            .iter()
            .any(|inst| matches!(inst, Inst::Call { .. })),
        "`appendSlice` is expanded at a site inside a loop"
    );
    let patterns: Vec<crate::legalize::Pattern> = crate::legalize::windows(&program, &main)
        .iter()
        .map(|window| window.pattern)
        .collect();
    assert_eq!(
        patterns,
        [crate::legalize::Pattern::AppendBytes],
        "the copy the five questions guard is one window where it was called"
    );
    assert_eq!(
        main.code
            .iter()
            .filter(|inst| matches!(
                inst,
                Inst::IntrinsicCall { site, .. }
                    if program.intrinsic_site(*site).intrinsic
                        == crate::Intrinsic::StringRefuseByteRange
            ))
            .count(),
        5,
        "the five refusals are intrinsic calls, so the body is still a leaf"
    );
}

/// A caller built by hand around one call to the library leaf at `callee`,
/// passing the address of `a` (slot 1) and the `Int` in `value`, and answering
/// into `dst`.
///
/// Slot 0 is the answer, slot 1 is the binding `a`, slot 2 the address of it and
/// slot 3 another `Int`; the binding is recorded as a [`Local`](crate::Local),
/// which is what says how far an address of it reaches. A `dst` of 1 is the
/// shape `a = bumpThenAdd(var a, ...)` would have if the lowering handed the
/// call the binding as its destination.
fn var_caller_of(program: &mut Program, callee: FunctionId, value: u32, dst: u32) -> FunctionId {
    use super::super::shapes::{ADDR, INT};
    let leaf = program.function(callee).clone();
    let listed = program.args.len() as u32;
    program.args.push(vec![
        crate::program::Arg {
            slot: 2,
            layout: ADDR,
        },
        crate::program::Arg {
            slot: value,
            layout: INT,
        },
    ]);
    let reprs = vec![Repr::Int, Repr::Int, Repr::Addr, Repr::Int];
    let mut caller = leaf.clone();
    caller.module = "m".into();
    caller.name = "caller".into();
    caller.params = Vec::new();
    caller.refs = RefMap::of(&reprs);
    caller.reprs = reprs;
    caller.code = vec![
        Inst::Int { dst: 1, value: 1 },
        Inst::Int { dst: 3, value: 1 },
        Inst::AddrOfSlot { dst: 2, slot: 1 },
        Inst::Call {
            dst,
            callee,
            args: crate::ArgsId(listed),
        },
        Inst::Return { src: 0 },
    ];
    caller.spans = vec![leaf.span; 5];
    caller.locals = vec![crate::Local {
        name: "a".into(),
        slot: 1,
        layout: INT,
        from: 0,
        to: 5,
    }];
    caller.inlined = Vec::new();
    program.functions.push(caller);
    FunctionId(program.functions.len() as u32 - 1)
}

/// An argument an address the caller formed can reach is copied before an
/// expanded body that writes through an address runs, and the answer is
/// assembled apart from the destination — the order a call observes. An
/// argument no such address reaches is read where the caller has it.
///
/// `bumpThenAdd` writes `x` and then reads `by`. Handed `var a` and `a` itself,
/// a call copied `a` into `by` first; reading `by` in place after the write
/// would answer one more than the call did. The body is the program's own,
/// relabelled into `std.int` — being in a standard-library module is the whole
/// of what makes a function the library's — so that the rule this pins is the
/// expansion's and not the checker's.
#[test]
fn an_argument_an_address_reaches_is_copied_before_a_var_body_runs() {
    let (mut program, _) = program(
        "fn bumpThenAdd(var x: Int, by: Int) -> Int {\n  x = x + 1\n  x + by\n}\n\
         fn main() -> Int {\n  var a = 1\n  bumpThenAdd(var a, 2)\n}",
    );
    let at = program
        .functions
        .iter()
        .position(|f| f.qualified() == "m.bumpThenAdd")
        .expect("`bumpThenAdd` was lowered");
    program.functions[at].module = "std.int".into();
    let leaf = FunctionId(at as u32);

    let copied_before_the_store = |program: &Program, id: FunctionId, src: u32| {
        let code = &program.function(id).code;
        let store = code
            .iter()
            .position(|inst| matches!(inst, Inst::Store { .. }))
            .expect("the body writes through the address");
        code[..store]
            .iter()
            .any(|inst| matches!(inst, Inst::Copy { src: from, .. } if *from == src))
    };
    let copied_out = |program: &Program, id: FunctionId, into: u32| {
        program
            .function(id)
            .code
            .iter()
            .any(|inst| matches!(inst, Inst::Copy { dst, .. } if *dst == into))
    };

    let reached = var_caller_of(&mut program, leaf, 1, 0);
    inline::expand_cold(&mut program, reached);
    assert!(
        !still_calls(&program, reached),
        "the library leaf is expanded"
    );
    assert!(
        copied_before_the_store(&program, reached, 1),
        "`a` is copied into the body's run before the body writes through its address: {:?}",
        program.function(reached).code
    );
    assert!(
        copied_out(&program, reached, 0),
        "and the answer is assembled apart and copied out: {:?}",
        program.function(reached).code
    );

    let apart = var_caller_of(&mut program, leaf, 3, 0);
    inline::expand_cold(&mut program, apart);
    assert!(
        !still_calls(&program, apart),
        "the library leaf is expanded"
    );
    assert!(
        !copied_before_the_store(&program, apart, 3),
        "an argument no address reaches is read where it stands: {:?}",
        program.function(apart).code
    );
    assert!(
        !copied_out(&program, apart, 0),
        "and the answer is assembled in the destination: {:?}",
        program.function(apart).code
    );

    // The destination alone within reach: the body would write `a` through its
    // address after assembling an answer in `a`, so the answer is assembled apart
    // and copied in when the body is done.
    let into = var_caller_of(&mut program, leaf, 3, 1);
    inline::expand_cold(&mut program, into);
    assert!(!still_calls(&program, into), "the library leaf is expanded");
    assert!(
        copied_out(&program, into, 1),
        "a destination an address reaches is written after the body: {:?}",
        program.function(into).code
    );
}
