//! The native tier, owned for one lowered `Program`.
//!
//! [ADR 0055] divides the native tier in two. `cove-native` emits machine code
//! for one function and knows nothing else; `vm::exec::native` is the runtime's
//! half of the boundary that code calls back through. What neither of them is, is
//! **the thing that decides which functions to compile and keeps the code alive**
//! — and until [issue #369](https://github.com/myuon/cove/issues/369) nothing
//! was, because the only caller was a comparison harness that built its own.
//!
//! This is that thing: one [`NativeProgram`] per lowered program, compiled
//! eagerly, finalized once, and then immutable for the life of the run.
//!
//! # Why eagerly, and why one finalize
//!
//! ADR 0056 measured the template compiler at **5.4 µs a function**, and drew the
//! consequence itself: "which makes ADR 0055's 'compile every supported reachable
//! function eagerly' cheap enough not to need a hotness counter — a thousand
//! functions is five milliseconds". So there is no hotness counter here, no
//! on-stack replacement and no deoptimization, which is ADR 0055's "Compile
//! functions, not traces" taken literally.
//!
//! One finalize matters more than it looks. A mapping is writable *or*
//! executable and never both, and the template compiler's `Jit::finalize` is the
//! flip — named without a link, because it exists only under a code generator's
//! feature and an intra-doc link to an item a default build does not have is a
//! broken link. Compiling on first call would mean flipping a page while Cove
//! frames stand on it, so everything is compiled before the first entry and
//! nothing is compiled after — which is also why a run cannot "rebuild or
//! refinalize the JIT", a property issue #369 asks for in as many words.
//!
//! # What a refusal is, and what it is not
//!
//! A function the code generator will not take runs on the **encoded** tier.
//! That is not a fallback: the encoded VM is a complete execution path and the
//! semantic reference, and ADR 0055's prohibition is on falling back to the
//! *tree-walking interpreter*, which nothing here can reach. Every refusal is
//! recorded with one stable reason and the first instruction that could not be
//! lowered, so a reader is told which family to build next rather than a number.
//!
//! # Compilation is not execution
//!
//! [`NativeProgram::compile_time`] is reported apart from anything a run
//! measures, because issue #369 requires it: a tier that is fast to run and slow
//! to build is a different trade from one that is neither, and a single wall-clock
//! figure covering both would hide which.
//!
//! [ADR 0055]: ../../../docs/adr/0055-native-execution-compiles-optimized-ir-one-function-at-a-time.md

use std::time::Duration;

use cove_ir::{FunctionId, Program};
use cove_native::{Unavailable, WindowCode};

use crate::vm::exec::native::Tiered;
use crate::NativeEntry;

/// Every compiled function of one lowered program, and the pages they live in.
///
/// ADR 0055's `Program + FunctionId -> encoded entry | native entry`, as the type
/// that owns both halves of the answer. It is [`Tiered`], which is the one
/// question the runtime asks of it, and it must **outlive every run that was
/// given it**: dropping it would drop the mappings its entries point into.
///
/// Build it with [`compile()`] and hand it to
/// [`Vm::with_native`](crate::Vm::with_native).
pub struct NativeProgram {
    /// The code generator, kept because it owns the pages.
    ///
    /// Never used again after [`compile()`] returns — every entry
    /// was taken out of it before then — and that is the point: this field is
    /// the lifetime of the executable memory, written down.
    #[cfg(feature = "template")]
    #[allow(dead_code)]
    jit: cove_native::template::Jit,
    /// One entry per `FunctionId`, `None` for a function that runs encoded.
    entries: Vec<Option<NativeEntry>>,
    /// One row per refused function, in `FunctionId` order.
    refusals: Vec<Refused>,
    reachable: usize,
    stubs: usize,
    compiled: usize,
    code_bytes: u64,
    /// How much of `code_bytes` is [ADR 0062] buffer windows, by pattern, and
    /// how many windows of each pattern were emitted.
    ///
    /// [Issue #423](https://github.com/myuon/cove/issues/423) asks for it, and
    /// the whole-program total above is why: before this, a report could say a
    /// program grew by 39.8% and nothing could say what of. See [`WindowCode`]
    /// for why the bytes are optional and the sites are not.
    ///
    /// [ADR 0062]: ../../../docs/adr/0062-an-append-is-ensure-store-commit.md
    windows: WindowCode,
    compile: Duration,
    /// Whether the code was compiled against the counting helpers. See
    /// [`compile_counting`].
    counts_helpers: bool,
}

/// One function that has no machine code, and why.
///
/// The row ADR 0055 asks a native run to record, and issue #369 to rank: "one
/// stable refusal reason per refused function", plus the first instruction that
/// could not be lowered. The dynamic call count that ranks them is *not* here,
/// because it is a fact about a run rather than about a program —
/// [`Vm::refused_calls`](crate::Vm::refused_calls) is where it comes from, and
/// [`Refused::id`] is what joins the two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    /// Which function, and the key into a run's dynamic call counts.
    pub id: FunctionId,
    /// Its qualified name, as a report prints it.
    pub name: String,
    /// The stable reason, as `cove_native::Reason` words it.
    ///
    /// A `String` rather than that enum so that this type exists in a build with
    /// no code generator — the CLI has to be able to *name* the report it cannot
    /// produce. The sentences are the enum's `Display` and are stable in the
    /// sense the ADR means: a family a reader can sort by and act on, not an
    /// instruction.
    pub reason: String,
    /// The pc of the first instruction that could not be lowered, if the refusal
    /// was about an instruction at all.
    pub at: Option<u32>,
    /// That instruction's opcode, by the name `cove_ir::bytecode::Op` gives it.
    ///
    /// The same spelling `encoded.rs`'s own refusal prints, so a reader meeting
    /// one here and one there is meeting one name.
    pub instruction: Option<String>,
    /// What that instruction was *about*, when its opcode is an aggregate.
    ///
    /// See [`Blocked`]. `None` for every opcode that names one operation.
    pub blocked: Option<Blocked>,
    /// Every distinct thing that blocks this function, with how many instructions
    /// each accounts for, most-frequent first.
    pub blockers: Vec<(Blocker, u32)>,
}

/// One distinct thing that blocks a function, as [`Refused::blockers`] groups
/// them.
///
/// The same pair [`Refused::instruction`] and [`Refused::blocked`] name for the
/// *first* blocker, carried so the whole set can be grouped and counted: two
/// instructions with the same opcode and the same subject are one blocker
/// however many pcs they sit at, and two `IntrinsicCall`s of different builtins
/// are two.
///
/// A `String` inside for the reason [`Refused::reason`] is: this type has to
/// exist in a build with no code generator, because the CLI must be able to
/// name the report it cannot produce.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Blocker {
    /// The opcode, by the name `cove_ir::bytecode::Op` gives it. Same spelling
    /// as `Refused::instruction`.
    pub instruction: Option<String>,
    /// What that opcode was about, for the two aggregate opcodes. See
    /// [`Blocked`].
    pub blocked: Option<Blocked>,
}

/// What a blocking instruction operates on, when naming the opcode is not enough.
///
/// An opcode is the unit [`Refused::instruction`] reports and it is the right one
/// for deciding *whether* to lower a family. It is the wrong one for deciding
/// *what to build*, because two of the opcodes at the top of a ranked table are
/// aggregates: `IntrinsicCall` is every builtin the language has, and the `Alloc`
/// opcodes are every layout a program declares. "Lower `IntrinsicCall`" is not a
/// task; "lower `String.byteAt`" is.
///
/// So this is the second key a census groups by, and it exists for exactly the
/// two aggregates. Everything else — `LoadField`, `Str`, `RunFinish` — names
/// one operation already, and inventing a subject for it would add a column that
/// repeats the opcode.
///
/// It is a `String` inside rather than a `cove_ir` or `cove_native` type, for the
/// reason [`Refused::reason`] is: this type has to exist in a build with no code
/// generator, because the CLI must be able to name the report it cannot produce.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Blocked {
    /// [`Inst::IntrinsicCall`](cove_ir::Inst::IntrinsicCall)'s builtin, as the
    /// receiver and the operation the IR's own `Builtin` names it by:
    /// `String.byteAt`, `Vector.push`.
    Intrinsic(String),
    /// [`Inst::Alloc`](cove_ir::Inst::Alloc)'s layout: what is being allocated.
    ///
    /// Both halves are carried because neither is the other. The name is the
    /// family — `covefmt.Token`, `Array<Int>` — and is what a reader recognises;
    /// the shape is `cove_ir::Shape`'s variant, and is what says how much work
    /// lowering it would be. A hundred distinct `Struct` names are one lowering;
    /// one `Vector` is another.
    Allocation {
        /// `Layout::name`, which is qualified for a declared type.
        name: String,
        /// `Shape`'s variant name, with nothing of its contents.
        shape: String,
    },
}

impl Tiered for NativeProgram {
    fn entry(&self, id: FunctionId) -> Option<NativeEntry> {
        self.entries.get(id.index()).copied().flatten()
    }

    fn counts_helpers(&self) -> bool {
        self.counts_helpers
    }
}

impl NativeProgram {
    /// Functions the lowering reached and gave a body.
    ///
    /// `cove_ir::lower_entry` slices a package by the checker's call graph and
    /// closes the slice against what the lowering names, so "reachable" is not a
    /// second analysis performed here — it is what a lowered program *is*, less
    /// the stubs.
    ///
    /// **Stubs are not counted, and that is the honest reading rather than the
    /// flattering one.** A stub is a declaration `cove_ir::lower` left a
    /// stand-in for because no path the slice followed needs its body, and a
    /// program of this shape has hundreds of them — the example package's other
    /// `[run.<name>]` entries, and every `test fn` beside them. Counted as
    /// reachable they would put the compiled share at a sixth of its true value;
    /// counted as refusals they would fill the ranked table with rows whose
    /// dynamic call count is nought. They are reported by
    /// [`NativeProgram::stubs`], apart, where a reader can see that the
    /// denominator excludes them.
    pub fn reachable(&self) -> usize {
        self.reachable
    }

    /// Declarations the lowering left as stubs, which are neither reachable nor
    /// refused. See [`NativeProgram::reachable`].
    pub fn stubs(&self) -> usize {
        self.stubs
    }

    /// Functions that have machine code.
    pub fn compiled(&self) -> usize {
        self.compiled
    }

    /// Functions that run on the encoded tier.
    pub fn refused(&self) -> usize {
        self.refusals.len()
    }

    /// Every refused function, in `FunctionId` order.
    ///
    /// Unranked on purpose: ranking needs a run's dynamic call counts, which this
    /// type does not have and must not pretend to.
    pub fn refusals(&self) -> &[Refused] {
        &self.refusals
    }

    /// Bytes of machine code emitted, over every compiled function.
    pub fn code_bytes(&self) -> u64 {
        self.code_bytes
    }

    /// Which part of [`code_bytes`](Self::code_bytes) is ADR 0062's buffer
    /// windows, and how many of each pattern were emitted.
    ///
    /// The bytes are `None` from a code generator that cannot attribute them,
    /// which is a fact about the generator rather than about the program —
    /// [`WindowCode`] says which and why. The sites are counted either way.
    pub fn window_code(&self) -> WindowCode {
        self.windows
    }

    /// What compiling cost, which is never part of what executing cost.
    pub fn compile_time(&self) -> Duration {
        self.compile
    }
}

/// Compiles every supported function of `program`, or says why this host cannot.
///
/// The whole table, eagerly, then one `finalize`. See the module documentation
/// for why both of those are the shape rather than a choice, and
/// [`Unavailable`] for what a refusal of the *host* is: a capability diagnostic,
/// never an attempted fallback.
///
/// Three ways it can answer `Err`, and all three are the host's rather than the
/// program's: this build has no code generator, this architecture is not x86-64,
/// or the operating system would not hand out an executable mapping. A *program*
/// never makes this fail — a function the generator refuses is recorded and runs
/// encoded.
pub fn compile(program: &Program) -> Result<NativeProgram, Unavailable> {
    compile_with(program, false)
}

/// [`compile()`], binding helpers that count each call compiled code makes into
/// the runtime.
///
/// What a run that wants a [`BoundaryReport`](crate::BoundaryReport)'s
/// native-to-runtime counts compiles with. The code is the code [`compile()`]
/// emits — the only difference is the helper addresses it loads — and each
/// helper is one counter write in front of the production body; see
/// [`native_helpers_counting`](crate::native_helpers_counting). A run that did not
/// ask for the counts should not use this: it is a table that pays for a question
/// nobody asked.
pub fn compile_counting(program: &Program) -> Result<NativeProgram, Unavailable> {
    compile_with(program, true)
}

#[cfg(feature = "template")]
fn compile_with(program: &Program, counting: bool) -> Result<NativeProgram, Unavailable> {
    use crate::wallclock::Instant;

    let helpers = match counting {
        false => crate::native_helpers(),
        true => crate::native_helpers_counting(),
    };
    // Direct native-to-native calls are on, which is PR #368 and what issue #369
    // says to measure: "Direct native-to-native calls from PR #368 are enabled."
    let mut jit = cove_native::template::Jit::new(helpers)?.calling_directly();
    let mut entries = vec![None; program.functions.len()];
    let mut refusals = Vec::new();
    let mut done = Vec::new();
    let mut code_bytes = 0u64;
    let mut windows = WindowCode::default();
    // One clock over the whole loop rather than one per function: what issue #369
    // asks to keep apart from execution is the *total*, and a per-function sample
    // is the comparison harness's question rather than this one's.
    let started = Instant::now();
    let mut stubs = 0;
    for at in 0..program.functions.len() {
        let id = FunctionId(at as u32);
        // A stub is a declaration with no body in this slice, so it is neither
        // compiled nor refused. See `NativeProgram::reachable` for why counting it
        // either way would misreport the run.
        if program.function(id).is_stub() {
            stubs += 1;
            continue;
        }
        match jit.compile(program, id) {
            Some(compiled) => {
                code_bytes += u64::from(compiled.code_bytes);
                windows.add(&compiled.windows);
                done.push((id, compiled));
            }
            None => refusals.push(refused_row(program, id)),
        }
    }
    // Before any entry, and once. Nothing is compiled after this line, so no page
    // is ever made writable again while a Cove frame stands on it.
    jit.finalize()?;
    let compile = started.elapsed();
    for (id, compiled) in done {
        entries[id.index()] = Some(jit.entry(compiled));
    }
    Ok(NativeProgram {
        jit,
        compiled: entries.iter().filter(|entry| entry.is_some()).count(),
        entries,
        refusals,
        reachable: program.functions.len() - stubs,
        stubs,
        code_bytes,
        windows,
        compile,
        counts_helpers: counting,
    })
}

/// One refused function's row: the reason, the pc, the opcode, and every
/// distinct blocker behind it.
#[cfg(feature = "template")]
fn refused_row(program: &Program, id: FunctionId) -> Refused {
    let function = program.function(id);
    let refusal = cove_native::refusal(program, function)
        .expect("a function with no compiled code was refused for a reason");
    let instruction = refusal.at.and_then(|pc| opcode_at(function, pc));
    let blocked = refusal.at.and_then(|pc| blocked_on(program, function, pc));
    Refused {
        id,
        name: function.qualified(),
        reason: refusal.reason.to_string(),
        at: refusal.at,
        instruction,
        blocked,
        blockers: grouped_blockers(program, function),
    }
}

/// Every distinct blocker of `function`, most-frequent first.
///
/// `cove_native::blockers` answers one `Refusal` per refused instruction —
/// including repeats, when a body is refused at the same opcode more than once
/// — and this is where they are grouped into the set [`Refused::blockers`]
/// reports: the same `(instruction, blocked)` pair [`refused_row`]'s own first
/// blocker is named by, counted.
///
/// Sorted descending by count and then ascending by the [`Blocker`] itself, so
/// the table is the same table twice for one run — `Coverage::taken`'s sort has
/// the same tie-break rationale, over a different key.
#[cfg(feature = "template")]
fn grouped_blockers(program: &Program, function: &cove_ir::Function) -> Vec<(Blocker, u32)> {
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<Blocker, u32> = BTreeMap::new();
    for refusal in cove_native::blockers(program, function) {
        let instruction = refusal.at.and_then(|pc| opcode_at(function, pc));
        let blocked = refusal.at.and_then(|pc| blocked_on(program, function, pc));
        *counts
            .entry(Blocker {
                instruction,
                blocked,
            })
            .or_insert(0) += 1;
    }
    let mut rows: Vec<(Blocker, u32)> = counts.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows
}

/// What the instruction at `pc` operates on, for the two opcodes that aggregate.
///
/// See [`Blocked`] for why only two. The `None` arm is the ordinary answer and
/// not a failure: most opcodes are their own subject.
#[cfg(feature = "template")]
fn blocked_on(program: &Program, function: &cove_ir::Function, pc: u32) -> Option<Blocked> {
    use cove_ir::Inst;
    match function.code.get(pc as usize)? {
        Inst::IntrinsicCall { site, .. } => {
            let held = program.intrinsic_site(*site);
            Some(Blocked::Intrinsic(held.intrinsic.to_string()))
        }
        Inst::Alloc { layout, .. } => {
            let held = program.layout(*layout);
            Some(Blocked::Allocation {
                name: allocation_name(program, *layout),
                // The variant and none of its contents: a `Shape::Struct`'s
                // fields are the layout's own description and the name already
                // says which struct this is.
                shape: shape_name(&held.shape).to_string(),
            })
        }
        _ => None,
    }
}

/// What a layout is called in the census, with its element where it has one.
///
/// `Layout::name` alone is not enough for the collection shapes, and it is the one
/// place this matters. `cove_ir::lower` names **every** `Array<T>` layout `Array`
/// and every `Vector<T>`'s element store `Vector` — the element is in the shape,
/// not in the name — so a census keyed on the name alone collapses `Array<Int>`
/// and `Array<covefmt.Token>` into one row. Those are not one row for the purpose
/// the table exists for: the element's width is most of what lowering an
/// allocation of it costs.
///
/// So the element is appended, one level deep and no further. One level, because
/// the element of an element is the *element's* business and a fully expanded
/// `Vector<Array<Token>>` is a type signature rather than a table cell.
#[cfg(feature = "template")]
fn allocation_name(program: &Program, layout: cove_ir::LayoutId) -> String {
    use cove_ir::Shape;
    let held = program.layout(layout);
    let named = |elem: cove_ir::LayoutId| format!("{}<{}>", held.name, program.layout(elem).name);
    match &held.shape {
        Shape::Elements { elem, .. } | Shape::Vector { elem } | Shape::Members { elem } => {
            named(*elem)
        }
        Shape::Shared { value } => named(*value),
        Shape::Entries { key, value } => format!(
            "{}<{}, {}>",
            held.name,
            program.layout(*key).name,
            program.layout(*value).name
        ),
        _ => held.name.to_string(),
    }
}

/// `Shape`'s variant name, which `Debug` would print with its contents.
///
/// Written out rather than derived from `Debug`, because a `Shape::Struct`'s
/// `Debug` is every field it has and a table cell is one word. A `match` also
/// means a new shape is a compile error here rather than a silently different
/// string.
#[cfg(feature = "template")]
fn shape_name(shape: &cove_ir::Shape) -> &'static str {
    use cove_ir::Shape;
    match shape {
        Shape::Free => "Free",
        Shape::Word(_) => "Word",
        Shape::Struct { .. } => "Struct",
        Shape::Enum { .. } => "Enum",
        Shape::Str => "Str",
        Shape::Bytes => "Bytes",
        Shape::Elements { .. } => "Elements",
        Shape::Vector { .. } => "Vector",
        Shape::ByteBuffer => "ByteBuffer",
        Shape::Members { .. } => "Members",
        Shape::Entries { .. } => "Entries",
        Shape::Closure { .. } => "Closure",
        Shape::Shared { .. } => "Shared",
        Shape::Boxed => "Boxed",
    }
}

/// The opcode of `function`'s instruction at `pc`, by the name `Op` gives it.
///
/// Through the encoder rather than through `Inst`'s own `Debug`, because the
/// encoder is what settles the *name*: one `Inst::Arith` is several opcodes, and
/// `Arith(Int, Add)` is the spelling `encoded.rs`'s refusal already prints for
/// the same instruction. An instruction with no encoding — which
/// `encoded::prepare` would have refused the program for — answers `None` rather
/// than a guess.
#[cfg(feature = "template")]
fn opcode_at(function: &cove_ir::Function, pc: u32) -> Option<String> {
    use cove_ir::bytecode::{encode, Op};
    let inst = function.code.get(pc as usize)?;
    let held = encode(inst, pc).ok()?;
    Op::from_number(held.opcode()).map(|op| format!("{op:?}"))
}

/// Native execution is not in this build.
///
/// ADR 0055's adoption gate asks that "a build without the native feature has no
/// executable-memory dependency", so the code generator is an optional feature
/// and off by default. Selecting the native tier without it is therefore a
/// **capability diagnostic** and not a silent fallback — the same answer the
/// x86-64-only generator gives on another architecture, and for the same reason.
#[cfg(not(feature = "template"))]
fn compile_with(_program: &Program, _counting: bool) -> Result<NativeProgram, Unavailable> {
    Err(Unavailable::new(
        "this build has no native code generator; rebuild with `--features template`",
    ))
}

#[cfg(all(test, feature = "template"))]
mod tests {
    use super::*;
    use cove_ir::{Function, Inst, Layout, LayoutId, Len, RefMap, Repr, Shape};
    use std::sync::Arc;

    /// **An allocation census row names the family and the shape.**
    ///
    /// It is here rather than in `tests/native_tier.rs` because of what happened
    /// to the case that used to be there: `Inst::Alloc` became a lowered
    /// instruction, so no function is refused at one any more and the row the
    /// fixture used to produce stopped existing. The *report* is a fact about a
    /// program and an operand bound could still refuse an allocation, so the arm
    /// stays — and a test of it that depends on which instructions today's subset
    /// happens to lower is a test that retires itself.
    ///
    /// Both halves are asserted because neither is the other, which is
    /// [`Blocked::Allocation`]'s own note: `cove_ir::lower` names every
    /// `Array<T>` layout `Array`, so a census keyed on the name alone collapses
    /// `Array<Int>` and `Array<covefmt.Token>` into one row.
    #[test]
    fn an_allocation_census_row_names_the_family_and_the_shape() {
        let span = cove_diag::Span::new(cove_diag::FileId(0), 0, 0);
        let reprs = vec![Repr::Ref];
        let code = vec![
            Inst::Alloc {
                dst: 0,
                layout: LayoutId(2),
                len: Len::Count(3),
            },
            Inst::Return { src: 0 },
        ];
        let function = Function {
            module: Arc::from("m"),
            name: Arc::from("f"),
            params: Vec::new(),
            spans: vec![span; code.len()],
            refs: RefMap::of(&reprs),
            reprs,
            returns: LayoutId(2),
            captures: Vec::new(),
            code,
            locals: Vec::new(),
            inlined: Vec::new(),
            span,
            is_async: false,
            stub: false,
        };
        let program = Program {
            functions: vec![function],
            layouts: vec![
                Layout::free(),
                Layout::word("Int", Repr::Int),
                Layout::object(
                    "Array",
                    Shape::Elements {
                        elem: LayoutId(1),
                        growable: false,
                    },
                ),
            ],
            ..Program::default()
        };
        let function = program.function(FunctionId(0));
        assert_eq!(
            blocked_on(&program, function, 0),
            Some(Blocked::Allocation {
                // The element is beside the family, because `Array` alone would
                // be every array the program has.
                name: "Array<Int>".to_string(),
                shape: "Elements".to_string(),
            })
        );
        // And an instruction that is its own subject carries no second key.
        assert_eq!(blocked_on(&program, function, 1), None);
    }
}
