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
use cove_native::Unavailable;

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
    compile: Duration,
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
}

impl Tiered for NativeProgram {
    fn entry(&self, id: FunctionId) -> Option<NativeEntry> {
        self.entries.get(id.index()).copied().flatten()
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
#[cfg(feature = "template")]
pub fn compile(program: &Program) -> Result<NativeProgram, Unavailable> {
    use crate::wallclock::Instant;

    // Direct native-to-native calls are on, which is PR #368 and what issue #369
    // says to measure: "Direct native-to-native calls from PR #368 are enabled."
    let mut jit = cove_native::template::Jit::new(crate::native_helpers())?.calling_directly();
    let mut entries = vec![None; program.functions.len()];
    let mut refusals = Vec::new();
    let mut done = Vec::new();
    let mut code_bytes = 0u64;
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
        compile,
    })
}

/// One refused function's row: the reason, the pc, and the opcode.
#[cfg(feature = "template")]
fn refused_row(program: &Program, id: FunctionId) -> Refused {
    let function = program.function(id);
    let refusal = cove_native::refusal(program, function)
        .expect("a function with no compiled code was refused for a reason");
    let instruction = refusal.at.and_then(|pc| opcode_at(function, pc));
    Refused {
        id,
        name: function.qualified(),
        reason: refusal.reason.to_string(),
        at: refusal.at,
        instruction,
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
pub fn compile(_program: &Program) -> Result<NativeProgram, Unavailable> {
    Err(Unavailable::new(
        "this build has no native code generator; rebuild with `--features template`",
    ))
}
