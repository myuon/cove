//! A lowered program as bytes, readable only by the build that wrote them.
//!
//! [ADR 0077](../../../docs/adr/0077-cove-fmt-is-covefmt.md) ships
//! `tools/covefmt` inside the `cove` binary as lowered IR, so that `cove fmt`
//! pays no front end and no lowering. `cove-cli`'s build script lowers it and
//! calls [`encode`]; the binary calls [`decode`] on the bytes it embedded. This
//! module is both halves, and it is **not an interface**: the format is
//! whatever this file writes in this build, it changes in any commit without
//! a version scheme or a migration, and a reader refuses an image any other
//! build wrote. ADR 0055 declined to promise a serialized IR, and this does
//! not promise one either — it is a build artifact, like a CPython `.pyc` or a
//! Dart SDK snapshot.
//!
//! # The header
//!
//! ```text
//! byte:  0 .. 7     8 .. 15        16 .. 23
//!        MAGIC      FINGERPRINT    body length
//!                   u64 LE         u64 LE
//! ```
//!
//! [`MAGIC`] says the bytes are an image at all. [`FINGERPRINT`] says which
//! build wrote them: it is a hash of this crate's source, computed by its build
//! script (see `build.rs` for why it is derived rather than bumped by hand),
//! so the encoder and decoder of one build agree by construction and an image
//! from another build is refused before a byte of it is read. The length is
//! what makes a truncated image a refusal rather than a read past the end.
//!
//! # The body
//!
//! Integers are LEB128 varints, signed ones zigzagged. Every name — a
//! function's, a layout's, a field's, a literal — goes into one table of
//! strings written first, and is a varint index after it. An instruction is
//! [`crate::bytecode`]'s sixteen-byte encoding with its fields written as
//! varints: the opcode byte, the three slot fields and the payload, so a
//! `move s3, s4` is four bytes rather than sixteen, and [`crate::bytecode::decode()`]
//! — which is strict, and refuses a field an opcode does not use — reads it
//! back. A span is written as a delta from the one before it.
//!
//! # What is kept
//!
//! What a run needs and nothing else ([`kept`] is the rule, written once):
//!
//! - every function, **stubs included**. A [`FunctionId`] is a position in
//!   [`Program::functions`], every call and closure names one, and keeping the
//!   placeholders is what keeps the ids the lowering assigned — renumbering
//!   would rewrite every instruction that names a function for the sake of a
//!   few bytes a stub costs;
//! - the program's literals, layouts, argument lists, jump tables, host
//!   operations and placed names, which instructions index;
//! - every span, because a runtime error names a file and a line and a chain
//!   of call sites;
//! - the source text of each file a lowered body's span points into, so that
//!   an error renders with its excerpt as every other Cove runtime error does.
//!   A standard-library file is named by its path, with a hash of its text,
//!   because the binary reading the image already holds that text in
//!   `cove_sema::stdlib` — a reader whose copy differs refuses the image;
//! - the entry's module and name. Its signature is its
//!   [`Function::params`] and [`Function::returns`], which is what the VM's
//!   boundary converts a host's argument and its answer by.
//!
//! What is dropped is [`Function::locals`] and [`Inlined::locals`] — the names
//! a debugger shows, which no run reads — and the text of a file only a stub
//! points into; such a stub's spans are moved to the start of its file, which
//! is where an error could only ever be blamed if a stub, whose one
//! instruction is a `return`, could raise one.
//!
//! # What is trusted
//!
//! A reader refuses wrong bytes it can recognise as wrong — a magic, a
//! fingerprint, a truncation, a tag or an instruction that is not one, a
//! string that is not UTF-8 — with an [`Unreadable`] and never a panic. It
//! does not re-verify the program: an image that passed the header is one
//! this build wrote from a program the lowering verified, and the machine
//! encodes and verifies the bytecode it runs in any case.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;

use cove_diag::{FileId, SourceMap, Span};

use crate::bytecode::{self, EncodedInst};
use crate::inst::Pc;
use crate::layout::{Case, Field, Layout, LayoutId, Part, Shape};
use crate::program::{
    Arg, Capture, Function, FunctionId, HostOp, Inlined, LayoutNames, Program, ResourceName, StrId,
    Table,
};
use crate::repr::{RefMap, Repr};

include!(concat!(env!("OUT_DIR"), "/fingerprint.rs"));

/// The first eight bytes of every image.
pub const MAGIC: [u8; 8] = *b"\0coveir\n";

/// How many bytes the header occupies: the magic, the fingerprint and the
/// body's length.
pub const HEADER_BYTES: usize = 24;

/// A program read back from an image, with what a run of it needs beside it.
pub struct Image {
    pub program: Program,
    /// The files the program's spans point into, at the [`FileId`]s the spans
    /// name. A file no lowered body points into is present with empty text,
    /// so that the ids still line up.
    pub sources: SourceMap,
    /// The module of the function the image was built to run.
    pub module: Arc<str>,
    /// Its name.
    pub name: Arc<str>,
}

impl Image {
    /// The entry's id in [`Image::program`].
    pub fn entry(&self) -> FunctionId {
        self.program
            .function_named(&self.module, &self.name)
            .expect("decode refuses an image whose entry is not in its program")
    }
}

/// Why bytes are not an image this build can read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unreadable {
    /// The first eight bytes are not [`MAGIC`].
    NotAnImage,
    /// A different build wrote it.
    OtherBuild { found: u64, expected: u64 },
    /// It stops before the header or the body says it ends.
    Truncated { needed: usize, found: usize },
    /// The bytes are the right length and say something this build would not
    /// have written.
    Malformed { at: usize, what: String },
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unreadable::NotAnImage => {
                write!(
                    f,
                    "these bytes are not a lowered-program image: the magic is wrong"
                )
            }
            Unreadable::OtherBuild { found, expected } => write!(
                f,
                "this lowered-program image was written by a different build of cove-ir \
                 (fingerprint {found:#018x}, and this build is {expected:#018x}); an image is \
                 readable only by the build that wrote it"
            ),
            Unreadable::Truncated { needed, found } => write!(
                f,
                "this lowered-program image is truncated: it needs {needed} bytes and has {found}"
            ),
            Unreadable::Malformed { at, what } => {
                write!(
                    f,
                    "this lowered-program image is malformed at byte {at}: {what}"
                )
            }
        }
    }
}

impl std::error::Error for Unreadable {}

/// Writes `program` as an image whose entry is `module.name`.
///
/// `sources` is the map the program was lowered against: its files' paths and
/// texts are what a runtime error will render from. Refused when the entry is
/// not in the program, when a span names a file `sources` does not hold, or
/// when an instruction has no encoding — which a program the lowering handed
/// back cannot have.
pub fn encode(
    program: &Program,
    sources: &SourceMap,
    module: &str,
    name: &str,
) -> Result<Vec<u8>, String> {
    if program.function_named(module, name).is_none() {
        return Err(format!(
            "the program does not hold its entry `{module}.{name}`"
        ));
    }
    let files: Vec<_> = sources.files().collect();
    let read = text_bearing_files(program);
    if let Some(missing) = read.iter().find(|id| id.0 as usize >= files.len()) {
        return Err(format!(
            "a span names file {}, and the source map holds {}",
            missing.0,
            files.len()
        ));
    }
    let program = kept(program);

    let mut w = Writer::default();

    // The files, at the ids the spans name.
    w.len(files.len());
    for file in &files {
        let path = file.path.to_string_lossy();
        w.str(&path);
        let library = sources.is_library(file.id);
        w.byte(library as u8);
        let library_text = cove_sema::stdlib::text_of(&path).filter(|text| *text == file.text);
        match (read.contains(&file.id), library_text) {
            (false, _) => w.byte(FILE_OMITTED),
            (true, Some(text)) if library => {
                w.byte(FILE_LIBRARY);
                w.u64(fnv(text.as_bytes()));
            }
            (true, _) => {
                w.byte(FILE_CARRIED);
                w.bytes(file.text.as_bytes());
            }
        }
    }

    w.str(module);
    w.str(name);

    w.len(program.layouts.len());
    for layout in &program.layouts {
        w.layout(layout);
    }

    w.len(program.functions.len());
    for function in &program.functions {
        w.function(function)?;
    }

    w.len(program.strings.len());
    for text in &program.strings {
        w.str(text);
    }

    w.len(program.args.len());
    for list in &program.args {
        w.len(list.len());
        for arg in list {
            w.u32(arg.slot);
            w.u32(arg.layout.0);
        }
    }

    w.len(program.tables.len());
    for table in &program.tables {
        w.len(table.targets.len());
        for target in &table.targets {
            w.u32(*target);
        }
        w.u32(table.default);
    }

    w.len(program.host_ops.len());
    for op in &program.host_ops {
        w.str(&op.module);
        w.str(&op.operation);
        match &op.resource {
            None => w.byte(0),
            Some(resource) => {
                w.byte(1);
                w.str(resource);
            }
        }
        w.u32(op.result.0);
    }

    for id in [
        program.str_layout,
        program.bytes_layout,
        program.buffer_layout,
        program.boxed_layout,
        program.view_layout,
        program.render_path_layout,
        program.identity_set_layout,
        program.identity_table_layout,
    ] {
        w.u32(id.0);
    }

    w.len(program.names.len());
    for names in &program.names {
        match names.name {
            None => w.u64(0),
            Some(id) => w.u64(u64::from(id.0) + 1),
        }
        w.len(names.parts.len());
        for part in &names.parts {
            w.u32(part.0);
        }
    }

    w.len(program.resource_names.len());
    for named in &program.resource_names {
        w.str(&named.module);
        w.str(&named.resource);
        w.u32(named.text.0);
    }

    w.len(program.by_name.len());
    for ((module, name), id) in &program.by_name {
        w.str(module);
        w.str(name);
        w.u32(id.0);
    }

    Ok(w.finish())
}

/// Reads an image this build wrote.
pub fn decode(bytes: &[u8]) -> Result<Image, Unreadable> {
    if bytes.len() < MAGIC.len() || bytes[..MAGIC.len()] != MAGIC {
        return Err(Unreadable::NotAnImage);
    }
    if bytes.len() < 16 {
        return Err(Unreadable::Truncated {
            needed: HEADER_BYTES,
            found: bytes.len(),
        });
    }
    let found = u64::from_le_bytes(bytes[8..16].try_into().expect("eight bytes"));
    if found != FINGERPRINT {
        return Err(Unreadable::OtherBuild {
            found,
            expected: FINGERPRINT,
        });
    }
    if bytes.len() < HEADER_BYTES {
        return Err(Unreadable::Truncated {
            needed: HEADER_BYTES,
            found: bytes.len(),
        });
    }
    let length = u64::from_le_bytes(bytes[16..24].try_into().expect("eight bytes"));
    let needed = usize::try_from(length)
        .ok()
        .and_then(|length| length.checked_add(HEADER_BYTES))
        .unwrap_or(usize::MAX);
    if bytes.len() < needed {
        return Err(Unreadable::Truncated {
            needed,
            found: bytes.len(),
        });
    }
    if bytes.len() > needed {
        return Err(Unreadable::Malformed {
            at: needed,
            what: format!(
                "{} bytes follow the end the header gives",
                bytes.len() - needed
            ),
        });
    }

    let mut r = Reader {
        bytes,
        at: HEADER_BYTES,
        strings: Vec::new(),
    };
    let count = r.len()?;
    let mut strings = Vec::with_capacity(count);
    for _ in 0..count {
        let text = r.text("a name")?;
        strings.push(Arc::<str>::from(text));
    }
    r.strings = strings;

    let mut sources = SourceMap::new();
    let count = r.len()?;
    for _ in 0..count {
        let path = r.str()?;
        let library = match r.byte()? {
            0 => false,
            1 => true,
            other => return Err(r.malformed(&format!("a library flag is {other}"))),
        };
        let text = match r.byte()? {
            FILE_OMITTED => String::new(),
            FILE_CARRIED => r.text("a source file")?.to_string(),
            FILE_LIBRARY => {
                let hash = r.u64()?;
                let text = cove_sema::stdlib::text_of(&path).ok_or_else(|| {
                    r.malformed(&format!("this build has no standard-library file `{path}`"))
                })?;
                if fnv(text.as_bytes()) != hash {
                    return Err(r.malformed(&format!(
                        "this build's standard-library file `{path}` is not the one the image \
                         was written against"
                    )));
                }
                text.to_string()
            }
            other => return Err(r.malformed(&format!("a file kind is {other}"))),
        };
        let path = PathBuf::from(&*path);
        if library {
            sources.add_library(path, text);
        } else {
            sources.add(path, text);
        }
    }

    let module = r.str()?;
    let name = r.str()?;

    let count = r.len()?;
    let mut layouts = Vec::with_capacity(count);
    for _ in 0..count {
        layouts.push(r.layout()?);
    }

    let count = r.len()?;
    let mut functions = Vec::with_capacity(count);
    for _ in 0..count {
        functions.push(r.function()?);
    }

    let count = r.len()?;
    let mut literals = Vec::with_capacity(count);
    for _ in 0..count {
        literals.push(r.str()?);
    }

    let count = r.len()?;
    let mut args = Vec::with_capacity(count);
    for _ in 0..count {
        let n = r.len()?;
        let mut list = Vec::with_capacity(n);
        for _ in 0..n {
            list.push(Arg {
                slot: r.u32()?,
                layout: LayoutId(r.u32()?),
            });
        }
        args.push(list);
    }

    let count = r.len()?;
    let mut tables = Vec::with_capacity(count);
    for _ in 0..count {
        let n = r.len()?;
        let mut targets = Vec::with_capacity(n);
        for _ in 0..n {
            targets.push(r.u32()?);
        }
        tables.push(Table {
            targets,
            default: r.u32()?,
        });
    }

    let count = r.len()?;
    let mut host_ops = Vec::with_capacity(count);
    for _ in 0..count {
        let module = r.str()?;
        let operation = r.str()?;
        let resource = match r.byte()? {
            0 => None,
            1 => Some(r.str()?),
            other => return Err(r.malformed(&format!("an option tag is {other}"))),
        };
        host_ops.push(HostOp {
            module,
            operation,
            resource,
            result: LayoutId(r.u32()?),
        });
    }

    let mut special = [LayoutId(0); 8];
    for id in &mut special {
        *id = LayoutId(r.u32()?);
    }

    let count = r.len()?;
    let mut names = Vec::with_capacity(count);
    for _ in 0..count {
        let name = match r.u64()? {
            0 => None,
            n => Some(StrId(
                u32::try_from(n - 1).map_err(|_| r.malformed("a string id is too wide"))?,
            )),
        };
        let n = r.len()?;
        let mut parts = Vec::with_capacity(n);
        for _ in 0..n {
            parts.push(StrId(r.u32()?));
        }
        names.push(LayoutNames { name, parts });
    }

    let count = r.len()?;
    let mut resource_names = Vec::with_capacity(count);
    for _ in 0..count {
        resource_names.push(ResourceName {
            module: r.str()?,
            resource: r.str()?,
            text: StrId(r.u32()?),
        });
    }

    let count = r.len()?;
    let mut by_name = BTreeMap::new();
    for _ in 0..count {
        let module = r.str()?;
        let name = r.str()?;
        let id = r.u32()?;
        if id as usize >= functions.len() {
            return Err(r.malformed(&format!("`{module}.{name}` names function {id}")));
        }
        by_name.insert((module, name), FunctionId(id));
    }

    if r.at != bytes.len() {
        return Err(r.malformed("the body ends before its length"));
    }

    let [str_layout, bytes_layout, buffer_layout, boxed_layout, view_layout, render_path_layout, identity_set_layout, identity_table_layout] =
        special;
    let program = Program {
        functions,
        layouts,
        strings: literals,
        args,
        tables,
        host_ops,
        str_layout,
        bytes_layout,
        buffer_layout,
        boxed_layout,
        view_layout,
        render_path_layout,
        identity_set_layout,
        identity_table_layout,
        names,
        resource_names,
        by_name,
    };
    if program.function_named(&module, &name).is_none() {
        return Err(Unreadable::Malformed {
            at: bytes.len(),
            what: format!("its entry `{module}.{name}` is not in its program"),
        });
    }
    Ok(Image {
        program,
        sources,
        module,
        name,
    })
}

/// What an image of `program` holds: `program` itself, less what no run
/// reads.
///
/// The one place that rule is written, so that [`encode`] writes this and the
/// round-trip test compares a decoded image against it. See the module
/// documentation for what is dropped and why.
pub fn kept(program: &Program) -> Program {
    let read = text_bearing_files(program);
    let mut kept = program.clone();
    for function in &mut kept.functions {
        function.locals.clear();
        for expansion in &mut function.inlined {
            expansion.locals.clear();
        }
        if function.stub {
            let at_start = |span: Span| {
                if read.contains(&span.file) {
                    span
                } else {
                    Span::new(span.file, 0, 0)
                }
            };
            function.span = at_start(function.span);
            for span in &mut function.spans {
                *span = at_start(*span);
            }
        }
    }
    kept
}

/// The files a runtime error could render an excerpt from: every file a span
/// of a lowered body points into.
///
/// A stub's spans are not counted. Its one instruction is a `return` that
/// cannot fail, and its declaration span is read for its name in a chain and
/// never rendered, so its file's text would be carried for nobody.
fn text_bearing_files(program: &Program) -> BTreeSet<FileId> {
    let mut read = BTreeSet::new();
    for function in program.functions.iter().filter(|f| !f.stub) {
        read.insert(function.span.file);
        read.extend(function.spans.iter().map(|span| span.file));
        read.extend(function.inlined.iter().map(|held| held.site.file));
    }
    read
}

/// Where each function's run of span deltas starts from.
const ORIGIN: Span = Span {
    file: FileId(0),
    start: 0,
    end: 0,
};

const FILE_OMITTED: u8 = 0;
const FILE_CARRIED: u8 = 1;
const FILE_LIBRARY: u8 = 2;

/// 64-bit FNV-1a, for the standard-library texts an image names by path.
fn fnv(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn repr_tag(repr: Repr) -> u8 {
    match repr {
        Repr::Unit => 0,
        Repr::Bool => 1,
        Repr::Int => 2,
        Repr::Float => 3,
        Repr::Duration => 4,
        Repr::Ref => 5,
        Repr::Addr => 6,
        Repr::Host => 7,
        Repr::Task => 8,
        Repr::Scope => 9,
        Repr::Tag => 10,
    }
}

fn repr_of(tag: u8) -> Option<Repr> {
    Some(match tag {
        0 => Repr::Unit,
        1 => Repr::Bool,
        2 => Repr::Int,
        3 => Repr::Float,
        4 => Repr::Duration,
        5 => Repr::Ref,
        6 => Repr::Addr,
        7 => Repr::Host,
        8 => Repr::Task,
        9 => Repr::Scope,
        10 => Repr::Tag,
        _ => return None,
    })
}

/// The body under construction, and the string table it is interning into.
#[derive(Default)]
struct Writer {
    body: Vec<u8>,
    strings: Vec<Arc<str>>,
    interned: HashMap<Arc<str>, u32>,
    /// The span before the one being written, which the next is a delta from.
    last: Option<Span>,
}

impl Writer {
    fn byte(&mut self, byte: u8) {
        self.body.push(byte);
    }

    fn u64(&mut self, mut n: u64) {
        loop {
            let low = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                self.body.push(low);
                return;
            }
            self.body.push(low | 0x80);
        }
    }

    fn i64(&mut self, n: i64) {
        self.u64(((n << 1) ^ (n >> 63)) as u64);
    }

    fn u32(&mut self, n: u32) {
        self.u64(u64::from(n));
    }

    fn len(&mut self, n: usize) {
        self.u64(n as u64);
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.len(bytes.len());
        self.body.extend_from_slice(bytes);
    }

    fn str(&mut self, text: &str) {
        let index = match self.interned.get(text) {
            Some(index) => *index,
            None => {
                let index = self.strings.len() as u32;
                let held: Arc<str> = Arc::from(text);
                self.strings.push(held.clone());
                self.interned.insert(held, index);
                index
            }
        };
        self.u32(index);
    }

    fn span(&mut self, span: Span) {
        self.u32(span.file.0);
        let last = self.last.unwrap_or(ORIGIN);
        self.i64(i64::from(span.start) - i64::from(last.start));
        self.i64(i64::from(span.end) - i64::from(span.start));
        self.last = Some(span);
    }

    fn reprs(&mut self, reprs: &[Repr]) {
        self.len(reprs.len());
        for repr in reprs {
            self.byte(repr_tag(*repr));
        }
    }

    fn layout(&mut self, layout: &Layout) {
        self.str(&layout.name);
        match &layout.shape {
            Shape::Free => self.byte(0),
            Shape::Word(repr) => {
                self.byte(1);
                self.byte(repr_tag(*repr));
            }
            Shape::Struct { fields, opaque } => {
                self.byte(2);
                self.len(fields.len());
                for field in fields {
                    self.str(&field.name);
                    self.u32(field.layout.0);
                    self.u32(field.at);
                }
                self.byte(*opaque as u8);
            }
            Shape::Enum { cases, payload } => {
                self.byte(3);
                self.len(cases.len());
                for case in cases {
                    self.str(&case.name);
                    self.len(case.parts.len());
                    for part in &case.parts {
                        self.u32(part.layout.0);
                        self.u32(part.at);
                    }
                }
                self.reprs(payload);
            }
            Shape::Str => self.byte(4),
            Shape::Bytes => self.byte(5),
            Shape::Elements { elem, growable } => {
                self.byte(6);
                self.u32(elem.0);
                self.byte(*growable as u8);
            }
            Shape::Vector { elem } => {
                self.byte(7);
                self.u32(elem.0);
            }
            Shape::ByteBuffer => self.byte(8),
            Shape::Members { elem } => {
                self.byte(9);
                self.u32(elem.0);
            }
            Shape::Entries { key, value } => {
                self.byte(10);
                self.u32(key.0);
                self.u32(value.0);
            }
            Shape::Closure { function, captures } => {
                self.byte(11);
                self.u32(function.0);
                self.len(captures.len());
                for capture in captures {
                    self.u32(capture.0);
                }
            }
            Shape::Shared { value } => {
                self.byte(12);
                self.u32(value.0);
            }
            Shape::Boxed => self.byte(13),
            Shape::IdentityTable => self.byte(14),
        }
        self.reprs(&layout.words);
    }

    fn function(&mut self, function: &Function) -> Result<(), String> {
        self.str(&function.module);
        self.str(&function.name);
        self.len(function.params.len());
        for param in &function.params {
            self.u32(param.0);
        }
        self.reprs(&function.reprs);
        self.u32(function.returns.0);
        self.len(function.captures.len());
        for capture in &function.captures {
            self.str(&capture.name);
            self.u32(capture.slot);
            self.u32(capture.layout.0);
        }
        self.len(function.code.len());
        for (pc, inst) in function.code.iter().enumerate() {
            let encoded = bytecode::encode(inst, pc as Pc).map_err(|error| {
                format!(
                    "`{}` has an instruction at {pc} with no encoding: {error}",
                    function.qualified()
                )
            })?;
            self.byte(encoded.opcode());
            self.u32(u32::from(encoded.a()));
            self.u32(u32::from(encoded.b()));
            self.u32(u32::from(encoded.c()));
            self.u64(encoded.payload());
        }
        if function.spans.len() != function.code.len() {
            return Err(format!(
                "`{}` has {} instructions and {} spans",
                function.qualified(),
                function.code.len(),
                function.spans.len()
            ));
        }
        self.last = Some(ORIGIN);
        self.span(function.span);
        for span in &function.spans {
            self.span(*span);
        }
        self.len(function.inlined.len());
        for expansion in &function.inlined {
            self.u32(expansion.from);
            self.u32(expansion.to);
            self.u32(expansion.callee.0);
            self.span(expansion.site);
        }
        self.byte(function.is_async as u8 | (function.stub as u8) << 1);
        Ok(())
    }

    /// The header, the string table and the body, in that order.
    fn finish(self) -> Vec<u8> {
        let mut table = Writer::default();
        table.len(self.strings.len());
        for text in &self.strings {
            table.bytes(text.as_bytes());
        }
        let length = (table.body.len() + self.body.len()) as u64;
        let mut out = Vec::with_capacity(HEADER_BYTES + length as usize);
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&FINGERPRINT.to_le_bytes());
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&table.body);
        out.extend_from_slice(&self.body);
        out
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    strings: Vec<Arc<str>>,
}

impl Reader<'_> {
    fn malformed(&self, what: &str) -> Unreadable {
        Unreadable::Malformed {
            at: self.at,
            what: what.to_string(),
        }
    }

    fn byte(&mut self) -> Result<u8, Unreadable> {
        let byte = *self
            .bytes
            .get(self.at)
            .ok_or_else(|| self.malformed("the body ends inside a field"))?;
        self.at += 1;
        Ok(byte)
    }

    fn u64(&mut self) -> Result<u64, Unreadable> {
        let mut n = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            let low = u64::from(byte & 0x7f);
            if shift == 63 && low > 1 {
                return Err(self.malformed("a varint is wider than sixty-four bits"));
            }
            n |= low << shift;
            if byte & 0x80 == 0 {
                return Ok(n);
            }
        }
        Err(self.malformed("a varint is wider than sixty-four bits"))
    }

    fn i64(&mut self) -> Result<i64, Unreadable> {
        let n = self.u64()?;
        Ok(((n >> 1) as i64) ^ -((n & 1) as i64))
    }

    fn u32(&mut self) -> Result<u32, Unreadable> {
        let n = self.u64()?;
        u32::try_from(n).map_err(|_| self.malformed(&format!("{n} does not fit thirty-two bits")))
    }

    fn u16(&mut self) -> Result<u16, Unreadable> {
        let n = self.u64()?;
        u16::try_from(n).map_err(|_| self.malformed(&format!("{n} does not fit a slot field")))
    }

    /// A count of things that follow, each at least a byte long — which is
    /// what keeps a corrupt count from asking for an allocation the bytes
    /// could not fill.
    fn len(&mut self) -> Result<usize, Unreadable> {
        let n = self.u64()?;
        let left = (self.bytes.len() - self.at) as u64;
        if n > left {
            return Err(self.malformed(&format!(
                "a count of {n} is more than the {left} bytes left"
            )));
        }
        Ok(n as usize)
    }

    /// A length-prefixed run of UTF-8, borrowed from the image.
    fn text(&mut self, what: &str) -> Result<&str, Unreadable> {
        let n = self.len()?;
        let at = self.at;
        self.at += n;
        std::str::from_utf8(&self.bytes[at..at + n]).map_err(|_| Unreadable::Malformed {
            at,
            what: format!("{what} is not UTF-8"),
        })
    }

    fn str(&mut self) -> Result<Arc<str>, Unreadable> {
        let index = self.u32()? as usize;
        self.strings.get(index).cloned().ok_or_else(|| {
            self.malformed(&format!(
                "string {index} of a table of {}",
                self.strings.len()
            ))
        })
    }

    fn flag(&mut self) -> Result<bool, Unreadable> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(self.malformed(&format!("a flag is {other}"))),
        }
    }

    fn span(&mut self, last: &mut Span) -> Result<Span, Unreadable> {
        let file = FileId(self.u32()?);
        let start = i64::from(last.start) + self.i64()?;
        let end = start + self.i64()?;
        let (Ok(start), Ok(end)) = (u32::try_from(start), u32::try_from(end)) else {
            return Err(self.malformed("a span lands outside a file"));
        };
        let span = Span::new(file, start, end);
        *last = span;
        Ok(span)
    }

    fn reprs(&mut self) -> Result<Vec<Repr>, Unreadable> {
        let n = self.len()?;
        let mut reprs = Vec::with_capacity(n);
        for _ in 0..n {
            let tag = self.byte()?;
            reprs.push(repr_of(tag).ok_or_else(|| self.malformed(&format!("a repr is {tag}")))?);
        }
        Ok(reprs)
    }

    fn layout(&mut self) -> Result<Layout, Unreadable> {
        let name = self.str()?;
        let shape = match self.byte()? {
            0 => Shape::Free,
            1 => {
                let tag = self.byte()?;
                Shape::Word(
                    repr_of(tag).ok_or_else(|| self.malformed(&format!("a repr is {tag}")))?,
                )
            }
            2 => {
                let n = self.len()?;
                let mut fields = Vec::with_capacity(n);
                for _ in 0..n {
                    fields.push(Field {
                        name: self.str()?,
                        layout: LayoutId(self.u32()?),
                        at: self.u32()?,
                    });
                }
                Shape::Struct {
                    fields,
                    opaque: self.flag()?,
                }
            }
            3 => {
                let n = self.len()?;
                let mut cases = Vec::with_capacity(n);
                for _ in 0..n {
                    let name = self.str()?;
                    let m = self.len()?;
                    let mut parts = Vec::with_capacity(m);
                    for _ in 0..m {
                        parts.push(Part {
                            layout: LayoutId(self.u32()?),
                            at: self.u32()?,
                        });
                    }
                    cases.push(Case { name, parts });
                }
                Shape::Enum {
                    cases,
                    payload: self.reprs()?,
                }
            }
            4 => Shape::Str,
            5 => Shape::Bytes,
            6 => Shape::Elements {
                elem: LayoutId(self.u32()?),
                growable: self.flag()?,
            },
            7 => Shape::Vector {
                elem: LayoutId(self.u32()?),
            },
            8 => Shape::ByteBuffer,
            9 => Shape::Members {
                elem: LayoutId(self.u32()?),
            },
            10 => Shape::Entries {
                key: LayoutId(self.u32()?),
                value: LayoutId(self.u32()?),
            },
            11 => {
                let function = FunctionId(self.u32()?);
                let n = self.len()?;
                let mut captures = Vec::with_capacity(n);
                for _ in 0..n {
                    captures.push(LayoutId(self.u32()?));
                }
                Shape::Closure { function, captures }
            }
            12 => Shape::Shared {
                value: LayoutId(self.u32()?),
            },
            13 => Shape::Boxed,
            14 => Shape::IdentityTable,
            other => return Err(self.malformed(&format!("a shape is {other}"))),
        };
        Ok(Layout {
            name,
            shape,
            words: self.reprs()?,
        })
    }

    fn function(&mut self) -> Result<Function, Unreadable> {
        let module = self.str()?;
        let name = self.str()?;
        let n = self.len()?;
        let mut params = Vec::with_capacity(n);
        for _ in 0..n {
            params.push(LayoutId(self.u32()?));
        }
        let reprs = self.reprs()?;
        let returns = LayoutId(self.u32()?);
        let n = self.len()?;
        let mut captures = Vec::with_capacity(n);
        for _ in 0..n {
            captures.push(Capture {
                name: self.str()?,
                slot: self.u32()?,
                layout: LayoutId(self.u32()?),
            });
        }
        let n = self.len()?;
        let mut code = Vec::with_capacity(n);
        for pc in 0..n {
            let opcode = self.byte()?;
            let a = self.u16()?;
            let b = self.u16()?;
            let c = self.u16()?;
            let payload = self.u64()?;
            let encoded = EncodedInst::new(opcode, a, b, c, payload);
            let inst = bytecode::decode(encoded, pc as Pc)
                .map_err(|error| self.malformed(&format!("`{module}.{name}` at {pc}: {error}")))?;
            code.push(inst);
        }
        let mut last = ORIGIN;
        let span = self.span(&mut last)?;
        let mut spans = Vec::with_capacity(code.len());
        for _ in 0..code.len() {
            spans.push(self.span(&mut last)?);
        }
        let n = self.len()?;
        let mut inlined = Vec::with_capacity(n);
        for _ in 0..n {
            inlined.push(Inlined {
                from: self.u32()?,
                to: self.u32()?,
                callee: FunctionId(self.u32()?),
                site: self.span(&mut last)?,
                locals: Vec::new(),
            });
        }
        let flags = self.byte()?;
        if flags > 0b11 {
            return Err(self.malformed(&format!("a function's flags are {flags}")));
        }
        Ok(Function {
            module,
            name,
            params,
            refs: RefMap::of(&reprs),
            reprs,
            returns,
            captures,
            code,
            spans,
            locals: Vec::new(),
            inlined,
            span,
            is_async: flags & 1 != 0,
            stub: flags & 2 != 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// covefmt, lowered from `formatSource` exactly as `cove-cli`'s build
    /// script lowers it, with the map it was lowered against.
    fn covefmt() -> (Program, SourceMap) {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tools/covefmt");
        let mut sources = SourceMap::new();
        let package = cove_sema::package::load(&root, &mut sources).expect("covefmt loads");
        let checked = cove_sema::Compiler::new()
            .compile(&package)
            .expect("covefmt checks");
        let program = crate::lower_entry(
            &checked,
            &sources,
            &cove_sema::HostSchemas::new(),
            "covefmt",
            "formatSource",
        )
        .expect("covefmt lowers");
        (program, sources)
    }

    fn image() -> Vec<u8> {
        let (program, sources) = covefmt();
        encode(&program, &sources, "covefmt", "formatSource").expect("covefmt encodes")
    }

    /// The round trip is exact against what an image keeps, and a decoded
    /// image encodes to the same bytes again.
    ///
    /// `Debug` is the comparison because [`Program`] has no `PartialEq` — an
    /// [`crate::Inst`] holds a float — and `Debug` writes every field, so two
    /// programs that print alike are alike field for field.
    #[test]
    fn covefmt_reads_back_as_what_its_image_keeps() {
        let (program, sources) = covefmt();
        let bytes = encode(&program, &sources, "covefmt", "formatSource").unwrap();
        let image = decode(&bytes).expect("an image this build wrote reads back");
        assert_eq!(
            format!("{:?}", image.program),
            format!("{:?}", kept(&program)),
            "the decoded program is not what the image keeps"
        );
        assert_eq!((&*image.module, &*image.name), ("covefmt", "formatSource"));
        assert_eq!(
            encode(&image.program, &image.sources, "covefmt", "formatSource").unwrap(),
            bytes,
            "a decoded image does not encode to the bytes it was read from"
        );
        crate::verify(&image.program).expect("a decoded program verifies");

        // What is kept beside the program: every file, at its id, and the text
        // of each one a lowered body points into.
        let read = text_bearing_files(&program);
        assert_eq!(image.sources.files().count(), sources.files().count());
        for (kept, original) in image.sources.files().zip(sources.files()) {
            assert_eq!(kept.path, original.path);
            assert_eq!(kept.id, original.id);
            assert_eq!(
                image.sources.is_library(kept.id),
                sources.is_library(original.id)
            );
            match read.contains(&kept.id) {
                true => assert_eq!(kept.text, original.text, "{}", kept.path.display()),
                false => assert_eq!(kept.text, "", "{}", kept.path.display()),
            }
        }
    }

    /// What [`kept`] drops is exactly what the module documentation says it
    /// drops: the names a debugger reads, and the spans of a stub into a file
    /// whose text is not carried.
    #[test]
    fn an_image_keeps_everything_but_debugger_names_and_unread_stub_spans() {
        let (mut program, _) = covefmt();
        let read = text_bearing_files(&program);
        let kept = kept(&program);
        let mut moved = 0;
        for function in &mut program.functions {
            function.locals.clear();
            for expansion in &mut function.inlined {
                expansion.locals.clear();
            }
            if function.stub && !read.contains(&function.span.file) {
                moved += 1;
                function.span = Span::new(function.span.file, 0, 0);
                for span in &mut function.spans {
                    *span = Span::new(span.file, 0, 0);
                }
            }
        }
        assert_eq!(format!("{program:?}"), format!("{kept:?}"));
        assert!(
            moved > 0,
            "covefmt has stubs declared in files no body reads"
        );
    }

    /// The three ways a header can be wrong are refused, each in its own
    /// words, and none of them panics.
    #[test]
    fn a_wrong_magic_a_wrong_fingerprint_or_a_truncation_is_refused() {
        let bytes = image();

        let mut wrong = bytes.clone();
        wrong[0] ^= 0xff;
        assert_eq!(decode(&wrong).err(), Some(Unreadable::NotAnImage));
        assert_eq!(decode(b"").err(), Some(Unreadable::NotAnImage));
        assert_eq!(decode(b"#!/bin/sh\n").err(), Some(Unreadable::NotAnImage));

        let mut other = bytes.clone();
        other[8] ^= 1;
        let refused = decode(&other)
            .err()
            .expect("another build's image is refused");
        assert_eq!(
            refused,
            Unreadable::OtherBuild {
                found: FINGERPRINT ^ 1,
                expected: FINGERPRINT
            }
        );
        assert!(refused.to_string().contains("different build"), "{refused}");

        // Every proper prefix: the header's length is what catches it, so
        // this is cheap however long the image is.
        for cut in MAGIC.len()..bytes.len() {
            match decode(&bytes[..cut]) {
                Err(Unreadable::Truncated { needed, found }) => {
                    assert_eq!(found, cut);
                    assert!(needed > cut);
                }
                other => panic!("a prefix of {cut} bytes read as {:?}", other.err()),
            }
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(matches!(decode(&longer), Err(Unreadable::Malformed { .. })));
    }

    /// A body cut short *and* given a header that agrees with the cut — which
    /// the length check cannot catch — is still refused, by the reader's own
    /// bounds, wherever it was cut.
    #[test]
    fn a_body_cut_anywhere_is_refused_rather_than_read_past() {
        let bytes = image();
        let body = bytes.len() - HEADER_BYTES;
        for step in 0..400 {
            let cut = body * step / 400;
            let mut short = bytes[..HEADER_BYTES + cut].to_vec();
            short[16..24].copy_from_slice(&(cut as u64).to_le_bytes());
            assert!(
                matches!(decode(&short), Err(Unreadable::Malformed { .. })),
                "a body cut at {cut} of {body} was not refused"
            );
        }
    }

    /// A build script's image is made on the *host* and read on the
    /// *target*, which is sound only while nothing that makes or reads one
    /// depends on which machine it is compiled for. So nothing may: the
    /// front end, the lowering and this format are free of target
    /// conditions, and this holds them to it.
    #[test]
    fn nothing_that_makes_or_reads_an_image_depends_on_the_target() {
        const CONDITIONS: [&str; 7] = [
            "target_arch",
            "target_os",
            "target_pointer_width",
            "target_endian",
            "target_family",
            "cfg(unix",
            "cfg(windows",
        ];
        let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut offending = Vec::new();
        for name in [
            "cove-diag",
            "cove-schema",
            "cove-syntax",
            "cove-sema",
            "cove-ir",
        ] {
            let mut files = Vec::new();
            sources_under(&crates.join(name).join("src"), &mut files);
            assert!(!files.is_empty(), "{name} has no source here");
            for path in files {
                let text = std::fs::read_to_string(&path).unwrap();
                for (at, line) in text.lines().enumerate() {
                    let line = line.trim_start();
                    // A comment, or one of the names above written as a
                    // string, is not a condition.
                    if line.starts_with("//") || line.starts_with('"') {
                        continue;
                    }
                    if CONDITIONS.iter().any(|condition| line.contains(condition)) {
                        offending.push(format!("{}:{}: {line}", path.display(), at + 1));
                    }
                }
            }
        }
        assert!(
            offending.is_empty(),
            "a crate that makes or reads a program image is conditional on the target, so an \
             image a build script makes on the host may not be the one the target would make:\n{}",
            offending.join("\n")
        );
    }

    fn sources_under(dir: &std::path::Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                sources_under(&path, found);
            } else if path.extension().is_some_and(|e| e == "rs") {
                found.push(path);
            }
        }
    }
}
