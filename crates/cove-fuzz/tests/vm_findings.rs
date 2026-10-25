//! The programs behind ADR 0120's findings, built by hand and held to its
//! contract: each is refused by `cove_ir::verify`, or runs in every mode to
//! `Ok` or `Err(RuntimeError)`, and never panics.
//!
//! None of them is a program the lowering writes. Each is a lowered program
//! with one function added or one layout edited, which is what an image from
//! another build, a lowering bug, or the `ir_mutate` target hands the machine.
//! Before ADR 0120 each reached memory the program did not own, or a word read
//! as something it was not.
//!
//! Each also writes its image body to `fuzz/regressions/image/<name>` when
//! `COVE_FUZZ_WRITE_FINDINGS` is set, which is how those inputs were made. The
//! committed files are replayed by `tests/regressions.rs` like any other
//! `image` regression; this file is what says what each one is, and keeps
//! building the program even after the standard library changes the hashes
//! an old image carries.

use std::path::Path;

use cove_ir::serial::Image;
use cove_ir::{
    Arg, ArgsId, Function, FunctionId, Inst, Layout, LayoutId, Len, Program, Repr, Shape,
};

/// A small program that has what the findings need: a struct with a
/// reference field, a closure with no captures and an address parameter, a
/// `Shared` cell, and a value rendered through `dyn`, so that the view
/// layouts are in the table.
const BASE: &str = r#"use console.println

trait Named {
  fn name(self) -> String
}

struct Pair {
  n: Int
  s: String
}

impl Named for Pair {
  fn name(self) -> String {
    self.s
  }
}

export fn main() -> Result<Unit, Error> {
  let p = Pair(n: 1, s: "a")
  let cell = Shared(p.n)
  cell.lock(fn(var v) { v = v + 1 })
  let d: dyn Named = p
  println("{d}")?
  Ok(())
}
"#;

fn header(body: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&cove_ir::serial::MAGIC);
    bytes.extend_from_slice(&cove_ir::serial::FINGERPRINT.to_le_bytes());
    bytes.extend_from_slice(&(body.len() as u64).to_le_bytes());
    bytes.extend_from_slice(body);
    bytes
}

fn base() -> Image {
    let body = cove_fuzz::image_body(BASE).expect("the base program lowers");
    cove_ir::serial::decode(&header(&body)).expect("the base image reads back")
}

fn layout(program: &Program, what: impl Fn(&Layout) -> bool) -> LayoutId {
    let at = program
        .layouts
        .iter()
        .position(what)
        .expect("the base program has the layout");
    LayoutId(at as u32)
}

fn function(program: &Program, name: &str) -> FunctionId {
    let at = program
        .functions
        .iter()
        .position(|function| &*function.module == "playground" && &*function.name == name)
        .expect("the base program has the function");
    FunctionId(at as u32)
}

/// A function `finding` of `reprs`, answering `returns`, running `code`, made
/// the image's entry.
fn entry(image: &mut Image, reprs: Vec<Repr>, returns: LayoutId, code: Vec<Inst>) {
    let main = function(&image.program, "main");
    let mut made: Function = image.program.functions[main.index()].clone();
    made.name = "finding".into();
    made.params = Vec::new();
    made.captures = Vec::new();
    made.locals = Vec::new();
    made.inlined = Vec::new();
    made.spans = vec![made.span; code.len()];
    made.refs = cove_ir::RefMap::of(&reprs);
    made.reprs = reprs;
    made.returns = returns;
    made.code = code;
    image.program.functions.push(made);
    let id = FunctionId(image.program.functions.len() as u32 - 1);
    image
        .program
        .by_name
        .insert(("playground".into(), "finding".into()), id);
    image.name = "finding".into();
}

fn args(program: &mut Program, row: Vec<Arg>) -> ArgsId {
    program.args.push(row);
    ArgsId(program.args.len() as u32 - 1)
}

/// Encodes `image`, runs it through the `image` harness — which panics on a
/// finding — and, when asked, writes its body as a regression input.
fn hold(name: &str, image: &Image) {
    let bytes = cove_ir::serial::encode(&image.program, &image.sources, &image.module, &image.name)
        .expect("the edited program encodes");
    let body = &bytes[cove_ir::serial::HEADER_BYTES..];
    cove_fuzz::image(body);
    if std::env::var_os("COVE_FUZZ_WRITE_FINDINGS").is_some() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/regressions/image");
        std::fs::write(dir.join(name), body).expect("write the regression input");
    }
}

/// What `verify` and the machine say of `image`, for the assertions below.
fn outcome(image: &Image) -> String {
    let bytes = cove_ir::serial::encode(&image.program, &image.sources, &image.module, &image.name)
        .expect("the edited program encodes");
    let subject = cove_fuzz::ir::image_input(&bytes[cove_ir::serial::HEADER_BYTES..])
        .expect("the edited program reads back");
    cove_fuzz::ir::run(subject).describe()
}

/// A field read as a reference where the object holds bytes: `load.field`
/// trusted its own layout, so a `String`'s first payload word, eight bytes of
/// text, came back as an object address and was followed.
#[test]
fn a_field_is_read_as_what_its_object_holds() {
    let mut image = base();
    let program = &image.program;
    let text = program.functions[function(program, "main").index()]
        .code
        .iter()
        .find_map(|inst| match inst {
            Inst::Str { text, .. } => Some(*text),
            _ => None,
        })
        .expect("main writes a literal");
    let string = program.str_layout;
    entry(
        &mut image,
        vec![Repr::Ref, Repr::Ref],
        string,
        vec![
            Inst::Str { dst: 0, text },
            Inst::LoadField {
                dst: 1,
                obj: 0,
                at: 0,
                layout: string,
            },
            Inst::Return { src: 1 },
        ],
    );
    let said = outcome(&image);
    assert!(said.contains("verify: ok"), "{said}");
    assert!(!said.contains("vm: ok"), "{said}");
    hold("field-read-as-another-family", &image);
}

/// `lock` on an object that is not a cell: the compare-and-swap wrote the
/// lock state over the first payload word of whatever the slot named — here a
/// closure's callee — and the call through it went wherever that said.
#[test]
fn a_lock_is_taken_only_on_a_cell() {
    let mut image = base();
    let program = &image.program;
    let callee = function(program, "main#0");
    let closure = layout(
        program,
        |l| matches!(l.shape, Shape::Closure { function, .. } if function == callee),
    );
    let int = layout(program, |l| l.shape == Shape::Word(Repr::Int));
    let unit = layout(program, |l| l.shape == Shape::Word(Repr::Unit));
    entry(
        &mut image,
        vec![Repr::Ref, Repr::Int, Repr::Unit],
        unit,
        vec![
            Inst::Alloc {
                dst: 0,
                layout: closure,
                len: Len::Fixed,
            },
            Inst::FuncRef { dst: 1, callee },
            Inst::StoreField {
                obj: 0,
                at: 0,
                src: 1,
                layout: int,
            },
            Inst::SharedLock { cell: 0 },
            Inst::SharedUnlock { cell: 0 },
            Inst::Unit { dst: 2 },
            Inst::Return { src: 2 },
        ],
    );
    let said = outcome(&image);
    assert!(said.contains("is not a `Shared`"), "{said}");
    hold("lock-on-a-closure", &image);
}

/// A closure called with an `Int` where its function takes an address: the
/// arguments were copied at the callee's parameter widths and never compared,
/// so the callee loaded and stored through the address `5`, a word of the
/// stack nobody formed.
#[test]
fn a_closure_is_called_with_its_own_parameters() {
    let mut image = base();
    let program = &mut image.program;
    let callee = function(program, "main#0");
    let closure = layout(
        program,
        |l| matches!(l.shape, Shape::Closure { function, .. } if function == callee),
    );
    let int = layout(program, |l| l.shape == Shape::Word(Repr::Int));
    let unit = layout(program, |l| l.shape == Shape::Word(Repr::Unit));
    let row = args(
        program,
        vec![Arg {
            slot: 2,
            layout: int,
        }],
    );
    entry(
        &mut image,
        vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Unit],
        unit,
        vec![
            Inst::Alloc {
                dst: 0,
                layout: closure,
                len: Len::Fixed,
            },
            Inst::FuncRef { dst: 1, callee },
            Inst::StoreField {
                obj: 0,
                at: 0,
                src: 1,
                layout: int,
            },
            Inst::Int { dst: 2, value: 5 },
            Inst::CallClosure {
                dst: 3,
                closure: 0,
                args: row,
                result: unit,
            },
            Inst::Return { src: 3 },
        ],
    );
    let said = outcome(&image);
    assert!(said.contains("verify: ok"), "{said}");
    assert!(said.contains("not the ones it takes"), "{said}");
    hold("closure-called-with-another-signature", &image);
}

/// A view forged out of two `Int`s and a reference: the `String` layout over
/// a `Shared` cell, read as a string. The view was taken at its word, so the
/// cell came back as a `String` whose bytes were its lock and its value.
#[test]
fn a_forged_view_is_refused_before_it_is_read() {
    let mut image = base();
    let program = &image.program;
    let cell = layout(program, |l| matches!(l.shape, Shape::Shared { .. }));
    let string = program.str_layout;
    entry(
        &mut image,
        vec![Repr::Unit, Repr::Int, Repr::Ref, Repr::Int, Repr::Ref],
        string,
        vec![
            Inst::Alloc {
                dst: 2,
                layout: cell,
                len: Len::Fixed,
            },
            Inst::Int {
                dst: 1,
                value: i64::from(string.0),
            },
            Inst::Int { dst: 3, value: 0 },
            Inst::DynRead { dst: 4, view: 1 },
            Inst::Return { src: 4 },
        ],
    );
    let said = outcome(&image);
    assert!(said.contains("verify: ok"), "{said}");
    assert!(said.contains("not the object the view names"), "{said}");
    hold("view-forged-over-a-cell", &image);
}

/// A struct whose words disagree with its fields: `Pair`'s reference word made
/// an `Int`, so the collector stopped tracing the string a `Pair` held while a
/// field load still read it as one. The layouts pass refuses the table.
#[test]
fn a_layout_whose_words_are_not_its_fields_is_refused() {
    let mut image = base();
    let pair = layout(&image.program, |l| {
        l.name.ends_with("Pair") && matches!(l.shape, Shape::Struct { .. })
    });
    let words = &mut image.program.layouts[pair.index()].words;
    let at = words
        .iter()
        .position(|repr| *repr == Repr::Ref)
        .expect("Pair holds a reference");
    words[at] = Repr::Int;
    let said = outcome(&image);
    assert!(said.contains("verify: refused"), "{said}");
    hold("struct-words-disagree-with-fields", &image);
}

/// The address of an `Int` slot read through as a reference: a place had no
/// need, so the `7` in the slot came back as an object. The places pass
/// refuses it.
#[test]
fn a_place_is_used_as_what_it_holds() {
    let mut image = base();
    let string = image.program.str_layout;
    entry(
        &mut image,
        vec![Repr::Int, Repr::Addr, Repr::Ref],
        string,
        vec![
            Inst::Int { dst: 0, value: 7 },
            Inst::AddrOfSlot { dst: 1, slot: 0 },
            Inst::Load {
                dst: 2,
                addr: 1,
                layout: string,
            },
            Inst::Return { src: 2 },
        ],
    );
    let said = outcome(&image);
    assert!(said.contains("verify: refused"), "{said}");
    hold("int-slot-read-through-as-a-reference", &image);
}
