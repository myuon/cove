//! An object kept alive by nothing but an address into it, across
//! collections ([issue 725]; docs/FUZZING.md, "Roots of compiled frames").
//!
//! [ADR 0120] made an address into a heap object a root of the object it is
//! inside (`Space::containers`): a frame that holds `&obj.field` and no longer
//! holds `obj` keeps `obj`. The source targets cannot test that — the
//! lowering keeps the object in a reference slot for as long as an address
//! into it lives (`Inst::AddrOfField`'s documentation), so dropping the
//! interior root leaves the object rooted anyway, and mutant 004, which drops
//! it, was found by nothing (docs/FUZZ_EVALUATION.md, section 4). This program
//! is written as IR, where nothing keeps the object but the address:
//!
//! 1. allocate a one-word struct, write 42 into it, take the address of the
//!    word, and clear the slot that held the object;
//! 2. allocate thousands more objects of the same size, each holding 7, so
//!    that the small heap collects and a freed block is reused;
//! 3. load through the address and answer what it reads.
//!
//! A machine that kept the object answers 42 in every mode. One that freed it
//! answers what reused the block — or refuses the read — on the small heap
//! only, and `vm` against `vm-gc` disagrees.
//!
//! The holder is the entry, which always runs on the encoded tier, and it has
//! to be: the native tier does not compile `AddrOfField`, so no compiled frame
//! can hold an interior address at all.
//!
//! The program's image is also `fuzz/seeds/image/interior-root`, so the
//! `image` target starts from it and `tests/regressions.rs` replays it in every
//! mode.
//!
//! [issue 725]: https://github.com/myuon/cove/issues/725
//! [ADR 0120]: ../../../docs/adr/0120-the-vm-runs-only-what-verifies.md

use std::collections::BTreeMap;
use std::sync::Arc;

use cove_diag::{FileId, SourceMap, Span};
use cove_fuzz::differential::Mode;
use cove_fuzz::ir::Subject;
use cove_fuzz::irgen::{
    base_layouts, BOXED, BYTES, BYTE_BUFFER, ENTRY, IDENTITY_SET, IDENTITY_TABLE, INT, MODULE,
    PATH, REF, STR, VIEW,
};
use cove_ir::{
    struct_layout, ArithOp, CmpOp, Function, FunctionId, Inst, Layout, LayoutId, Len, Program,
    RefMap, Repr, Shape,
};
use cove_runtime::trace::RunOutcome;

/// How many objects the loop allocates: two words each, several times the
/// small heap's allowance, and fewer than the 8,192 words after which the
/// large heap's pacing collects, so that `vm` does not collect.
const TURNS: i32 = 3_000;

/// The program, and the source map its spans point into.
fn interior_root_program() -> (Program, SourceMap) {
    let mut layouts = base_layouts();
    let (fields, words) = struct_layout(&[(Arc::from("v"), INT)], &layouts);
    layouts.push(Layout::inline(
        "Cell",
        Shape::Struct {
            fields,
            opaque: false,
        },
        words,
    ));
    let cell = LayoutId(layouts.len() as u32 - 1);
    // s0 the object, s1 the address into it, s2 the value, s3 the counter,
    // s4 the loop's test, s5 each new object, s6 the 7 they hold.
    let reprs = vec![
        Repr::Ref,
        Repr::Addr,
        Repr::Int,
        Repr::Int,
        Repr::Bool,
        Repr::Ref,
        Repr::Int,
    ];
    let code = vec![
        Inst::Alloc {
            dst: 0,
            layout: cell,
            len: Len::Fixed,
        },
        Inst::Int { dst: 2, value: 42 },
        Inst::StoreField {
            obj: 0,
            at: 0,
            src: 2,
            layout: INT,
        },
        Inst::AddrOfField {
            dst: 1,
            obj: 0,
            at: 0,
        },
        // From here only the address keeps the object.
        Inst::Clear {
            slot: 0,
            layout: REF,
        },
        Inst::Int { dst: 3, value: 0 },
        Inst::Int { dst: 6, value: 7 },
        // +7: the loop, closed by a branch taken while the counter is below
        // `TURNS` (a fused branch is taken when its comparison is false).
        Inst::Alloc {
            dst: 5,
            layout: cell,
            len: Len::Fixed,
        },
        Inst::StoreField {
            obj: 5,
            at: 0,
            src: 6,
            layout: INT,
        },
        Inst::ArithImm {
            op: ArithOp::Add,
            dst: 3,
            a: 3,
            value: 1,
        },
        Inst::CmpImmBranch {
            op: CmpOp::Ge,
            dst: 4,
            a: 3,
            value: TURNS,
            target: 7,
        },
        Inst::Clear {
            slot: 5,
            layout: REF,
        },
        Inst::Load {
            dst: 2,
            addr: 1,
            layout: INT,
        },
        Inst::Return { src: 2 },
    ];
    let spans = (0..code.len())
        .map(|pc| Span::new(FileId(0), pc as u32, pc as u32 + 1))
        .collect();
    let main = Function {
        module: Arc::from(MODULE),
        name: Arc::from(ENTRY),
        params: Vec::new(),
        refs: RefMap::of(&reprs),
        reprs,
        returns: INT,
        captures: Vec::new(),
        code,
        spans,
        locals: Vec::new(),
        inlined: Vec::new(),
        span: Span::new(FileId(0), 0, 1),
        is_async: false,
        stub: false,
    };
    let mut by_name = BTreeMap::new();
    by_name.insert((main.module.clone(), main.name.clone()), FunctionId(0));
    let program = Program {
        functions: vec![main],
        layouts,
        strings: Vec::new(),
        args: Vec::new(),
        tables: Vec::new(),
        host_ops: Vec::new(),
        str_layout: STR,
        bytes_layout: BYTES,
        buffer_layout: BYTE_BUFFER,
        boxed_layout: BOXED,
        view_layout: VIEW,
        render_path_layout: PATH,
        identity_set_layout: IDENTITY_SET,
        identity_table_layout: IDENTITY_TABLE,
        names: Vec::new(),
        resource_names: Vec::new(),
        by_name,
    };
    let mut sources = SourceMap::new();
    sources.add("fuzz/interior-root.ir", "x".repeat(64));
    (program, sources)
}

fn subject() -> Subject {
    let (program, sources) = interior_root_program();
    Subject {
        program,
        sources: Arc::new(sources),
        module: Arc::from(MODULE),
        name: Arc::from(ENTRY),
        args: Vec::new(),
    }
}

/// Every mode answers 42, the small-heap ones after collecting while the
/// address was the object's only root, and `vm` without collecting at all —
/// so `vm` against `vm-gc` is the comparison a lost interior root fails.
#[test]
fn an_address_alone_keeps_its_object_across_collections() {
    let report = cove_fuzz::ir::run(subject());
    assert_eq!(report.verified, Ok(()), "{}", report.describe());
    assert!(report.runs.mismatch.is_none(), "{}", report.describe());
    for o in &report.runs.observed {
        assert_eq!(o.outcome, RunOutcome::Success, "{}", report.describe());
        assert!(o.answer.contains("42"), "{}: {}", o.mode.name(), o.answer);
    }
    let collections = |mode| {
        report
            .runs
            .observed
            .iter()
            .find(|o| o.mode == mode)
            .and_then(|o| o.collections)
    };
    let small = collections(Mode::VmGc).expect("vm-gc runs");
    assert!(small > 0, "vm-gc did not collect: {}", report.describe());
    assert_eq!(collections(Mode::Vm), Some(0), "{}", report.describe());
    eprintln!("interior-root: vm-gc collected {small} times, vm none");
}

/// The committed `image` seed is this program's image body, so that the
/// `image` target and the regression replay run it too. Rewrite it with
/// `COVE_FUZZ_WRITE_SEEDS=1` after a change to the program or to the image
/// format.
#[test]
fn the_image_seed_is_this_program() {
    let (program, sources) = interior_root_program();
    let bytes = cove_ir::serial::encode(&program, &sources, MODULE, ENTRY).expect("it encodes");
    let body = &bytes[cove_ir::serial::HEADER_BYTES..];
    let path = format!(
        "{}/../../fuzz/seeds/image/interior-root",
        env!("CARGO_MANIFEST_DIR")
    );
    if std::env::var_os("COVE_FUZZ_WRITE_SEEDS").is_some() {
        std::fs::write(&path, body).expect("write the seed");
    }
    let committed = std::fs::read(&path).expect("the committed seed");
    assert!(
        committed == body,
        "{path} is stale: rerun with COVE_FUZZ_WRITE_SEEDS=1"
    );
}
