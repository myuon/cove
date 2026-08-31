//! An experimental third execution path: one contiguous stack of eight-byte
//! untagged words, one logical numbering, one frame base.
//!
//! [ADR 0028](../../../docs/adr/0028-five-representations-and-one-is-public.md)
//! decision 1 says what a slot is — "eight bytes, untagged, and its kind
//! comes from metadata", and "one logical frame, one slot numbering, one
//! base" — and then says the thing this module exists to answer:
//!
//! > **The physical arrangement is a measurement question and is not decided
//! > here.**
//!
//! [Issue #212](https://github.com/myuon/cove/issues/212) is that
//! measurement, and this is its vehicle. It is **not a third permanent
//! evaluator**: it runs a closed subset of the IR, refuses everything else
//! before any side effect, is never selected by `cove run`, and is written
//! to be deleted or absorbed once the comparison is recorded.
//!
//! # The frame
//!
//! ```text
//! words:      Vec<u64>   one contiguous stack; every frame is a window of it
//! frames:     Vec<Call>  one record per standing call
//! base                   where the running frame's window begins
//! top                    words.len(); the running frame's operand top
//! pc                     an index into the running function's code
//! ```
//!
//! A frame is `words[base .. base + width]`, where `width` is the callee's
//! `cove_ir::Function::scalar_frame_size`. **Parameters, locals and
//! temporaries are one index space from one base**: parameter `i` is
//! `base + i`, the body's locals follow it densely, and a temporary is
//! pushed above `base + width` and addressed by nothing. That is the whole
//! of the arrangement; there is no second stack, no second base, and no
//! second count on a call.
//!
//! Every logical value in this slice is exactly one word, as ADR 0028
//! permits ("most values have width one"); a layout that spans adjacent
//! words is legal there and is not built here.
//!
//! ## What a word holds
//!
//! | static kind | the eight bytes |
//! | --- | --- |
//! | `Int` | the full signed 64-bit value, as `i64 as u64` |
//! | `Float` | the full IEEE-754 bit pattern, every pattern including every NaN payload |
//! | `Bool` | canonical 0 or 1 |
//! | `Unit` | no word; the layout omits it |
//!
//! The bits are not self-describing. What a word means comes from the
//! instruction that touches it and from `cove_ir::Function`'s per-slot
//! metadata, both of which are the checker's answers written down at
//! lowering time. `Word` is the whole of the codec and it is a
//! reinterpretation in both directions: nothing is truncated, tagged, or
//! canonicalised on the way in.
//!
//! `Float` is included because ADR 0028 decides it, and it is *not* exercised
//! by the two rows this slice was built for: `cove_ir::Scalar` is `Int | Bool`
//! today, so a `Float` is still lowered as a `Value` and this backend refuses
//! any function that holds one. What the tests prove is the word: all 64 bits
//! survive both the codec and a real frame.
//!
//! # The calling convention
//!
//! Stated here because [issue #212](https://github.com/myuon/cove/issues/212)
//! asks for it before or with the implementation.
//!
//! - **Arguments are evaluated in source order** and pushed onto the one
//!   stack as they are evaluated. `cove_ir::lower` emits them that way and
//!   this backend does not reorder.
//! - **An argument becomes the callee's parameter word without moving.**
//!   There is no argument vector and nothing is copied: `base' = top - argc`,
//!   so the words the caller pushed *are* `words[base' .. base' + argc]`,
//!   which is parameters `0 .. argc` of the callee.
//! - **The frame base changes to `base'` and the stack pointer to
//!   `base' + width`.** `Vec::resize` fills the locals with zero, which is
//!   the canonical `Unit`/`false`/`0` word; a body writes every local before
//!   it reads one, because the checker settled that before lowering.
//! - **The return address and the caller's metadata** are one `Call`
//!   record pushed onto the frame stack: the callee's `FunctionId`, the
//!   caller's resume point (`pc + 1`, which `cove_ir::lower` makes a block
//!   head for exactly this reason), and the caller's frame base. A `Call`
//!   is 12 bytes and `Copy`.
//! - **A scalar return leaves its answer in one word.** `return-scalar` pops
//!   the answer, truncates the stack to the returning frame's base — which
//!   discards its locals and the arguments it was given together, because
//!   they are the same storage — and pushes the answer, so it lands exactly
//!   where the caller's own operand top was before it pushed the arguments.
//! - **Caller locals are preserved** because they are *below* the returning
//!   frame's base and the truncation stops there.
//! - **Recursion** is bounded twice, exactly as `Vm::enter` bounds it:
//!   `crate::interp::MAX_CALL_DEPTH` frames is a hard ceiling with the
//!   interpreter's own message, and a run whose budget sets
//!   `max_call_depth` is stopped by the budget's message below it.
//! - **A run that raises** leaves its frames where they stand and unwinds
//!   through Rust's `?`. Whatever fuel had been charged and not handed over
//!   is spent at the end of the run, however the run ended; see
//!   `FrameVm::spend_pending_fuel`, which is `Vm::spend_pending_fuel`.
//! - **Fuel and cancellation** are asked at the same control-flow points the
//!   VM asks them at, on the same schedule and against the same four
//!   constants of
//!   [ADR 0024](../../../docs/adr/0024-a-stop-is-a-bound-not-a-point.md):
//!   every call, every return, every backward jump once `BACK_EDGE_FUEL` has
//!   gathered, entry to the entry, and any block entered with
//!   `SAFEPOINT_INTERVAL` already standing.
//! - **Trace events** are the two source-level ones ADR 0019 keeps on every
//!   backend — `EntryEnter` and `EntryExit` — plus the run's `HeapSummary`
//!   and `RunEnded`, emitted in the order and at the points `Vm::run` emits
//!   them, so `cove trace` reads a run of this backend the way it reads a
//!   run of either other one.
//!
//! **A call and a return allocate nothing once the capacity is warm.** The
//! only growth is `Vec::resize` on a `Vec<u64>` past its capacity, and
//! `INITIAL_WORDS` reserves 32 KB of it before the first call so that even
//! that does not happen on anything this backend admits.
//! `crates/cove-runtime/tests/frame_allocation.rs` counts what is left with a
//! global allocator: ten thousand extra calls and ten thousand extra returns
//! reach it **zero** times.
//!
//! # The boundary, and where a `Value` is allowed to be
//!
//! Issue #212's hard constraint is that **no general Rust `Value` is
//! constructed, cloned, dropped or pattern-matched in the hot execution
//! path**. This backend keeps it structurally rather than by care:
//!
//! - [`admits`] refuses any function whose `value_frame_size` or
//!   `place_frame_size` is nonzero, so **no frame word is ever a `Value`**.
//!   There is no `Vec<Value>` frame here to be one.
//! - Six instructions materialise a `Value` — `const`, `scalar-to-value`,
//!   `pop`, `make-builtin`, `try` and `return` — and they exist because
//!   `benches/arith` and `benches/call` end with `assertEqual(...)?` and
//!   `Ok(())`, which is nine instructions run *once* against a loop run two
//!   million times. They hold their operands in a materialisation buffer
//!   that is not a frame: nothing indexes it, no frame owns a window of it,
//!   and a function that needed one word of it to survive a call is refused.
//! - Every one of the six increments [`FrameVm::materialized`], so the claim
//!   is a number a test reads rather than a sentence. `benches/arith` and
//!   `benches/call` each report **8**, and the loop reports zero.
//!
//! This is ADR 0028 decision 5 — "`Value` is materialized at the boundary,
//! and the boundary list is closed" — with the list written out.
//!
//! # What it refuses
//!
//! Everything else, by name, before any side effect, with no fallback. See
//! [`admits`]. In particular there are no heap objects, no `Dynamic`, no
//! `dyn`, no `Any`, no enum layout, no places, no `var`, no closures, no Host
//! calls, no tasks, no strings and no collections: those are Phase B of
//! [#197](https://github.com/myuon/cove/issues/197) and this slice is scalars
//! only.

use std::rc::Rc;

use cove_diag::Span;
use cove_ir::{Const, FunctionId, Inst, Program, Scalar, SlotKind};
use cove_schema::builtins::{free_builtin, FreeBuiltinKind};

use crate::budget::Meter;
use crate::error::RuntimeError;
use crate::heap::HeapStats;
use crate::host::HostRegistry;
use crate::interp::{returned_error_message, source_text, MAX_CALL_DEPTH};
use crate::runtime::Runtime;
use crate::trace::{RunOutcome, Timing, TraceEvent};
use crate::value::{Repr, Value};
use crate::vm::{
    as_value_of, constant, int_binary, name as const_name, opened, BACK_EDGE_FUEL,
    INSTRUCTION_FUEL, SAFEPOINT_INTERVAL,
};
use crate::Stopped;

/// The codec between a Cove scalar and the eight bytes a word holds.
///
/// Every conversion here is a reinterpretation. Nothing is truncated and
/// nothing is tagged, which is the whole reason a *typed* word can hold a
/// full `Int` and a full `Float` where a *tagged* value cannot — ADR 0028's
/// "the floor for a universal tagged value in this language is 16 bytes".
///
/// A unit struct rather than free functions so that the four rules read as
/// one table, which is how ADR 0028 writes them.
///
/// Private, and it stays private: it is representation 2's encoding, and
/// ADR 0028 decision 0's visibility column says no public signature of this
/// crate names one. What crosses the boundary is a `Value`, materialised by
/// the six instructions the module docs list.
///
/// Three of the six are reached only by the mechanism tests, and they are
/// kept rather than deleted because they are the half of ADR 0028's table
/// this backend cannot yet execute: `cove_ir::Scalar` is `Int | Bool`, so a
/// `Float` is still lowered as a `Value` and [`admits`] refuses every
/// function that holds one. The tests prove the word carries all 64 bits
/// anyway, which is what issue #212 asks for where the rows do not exercise
/// it.
struct Word;

#[allow(dead_code)]
impl Word {
    /// An `Int` is the full signed 64-bit value.
    #[inline(always)]
    fn of_int(value: i64) -> u64 {
        value as u64
    }

    /// And back, losslessly, for every one of the 2^64 patterns.
    #[inline(always)]
    fn int(word: u64) -> i64 {
        word as i64
    }

    /// A `Bool` is canonical: 0 or 1 and nothing else.
    #[inline(always)]
    fn of_bool(value: bool) -> u64 {
        value as u64
    }

    /// A `Float` is the full IEEE-754 bit pattern, including every NaN
    /// payload and both zeroes.
    ///
    /// `to_bits` rather than a transmute through `i64`, because the two
    /// differ on nothing and this one says what it means.
    #[inline(always)]
    fn of_float(value: f64) -> u64 {
        value.to_bits()
    }

    /// And back. `from_bits` is defined for every pattern, so this is total.
    #[inline(always)]
    fn float(word: u64) -> f64 {
        f64::from_bits(word)
    }

    /// The `Bool` a word stands for, refusing a non-canonical pattern.
    ///
    /// A word the layout calls `Bool` holds 0 or 1, because every road to
    /// one writes [`Word::of_bool`] or an `IntOp` comparison, and both
    /// produce exactly those. Anything else is a broken invariant of this
    /// backend rather than a program that could be told about it, so it
    /// panics rather than raising — the treatment `Vm` gives a value stack
    /// that ran dry, and for the same reason.
    ///
    /// It is asked here and not in `jump-if-false`, which reads `!= 0`
    /// exactly as `Vm` does: this is the boundary where the bits become a
    /// `Value` an embedder can see, so it is the last place the invariant is
    /// still checkable and the only place off the hot path.
    #[inline]
    fn canonical_bool(word: u64) -> bool {
        match word {
            0 => false,
            1 => true,
            other => unreachable!(
                "a word the layout calls `Bool` holds 0 or 1, and this one holds {other}"
            ),
        }
    }
}

/// Why an entry is not admitted to this backend.
///
/// It names the construct in the words a Cove programmer would use, exactly
/// as `cove_ir::Unsupported` does, because the refusal is read by the same
/// kind of reader. It is a separate type because it is a refusal by a
/// *backend* over an already-lowered program, and reporting it as a lowering
/// failure would say the wrong thing about where it happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    /// What this backend cannot run.
    pub what: String,
    /// Where it is, so a reader can find it, and `None` for a refusal about
    /// a name rather than about a construct.
    pub span: Option<Span>,
}

impl Refused {
    fn new(what: impl Into<String>, span: Span) -> Refused {
        Refused {
            what: what.into(),
            span: Some(span),
        }
    }

    /// A refusal with nowhere to point: the entry itself was not found.
    fn nowhere(what: impl Into<String>) -> Refused {
        Refused {
            what: what.into(),
            span: None,
        }
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the 8-byte frame cannot run {}", self.what)
    }
}

/// Whether this backend can run `module.name` and everything it reaches, and
/// the entry's id when it can.
///
/// **This is asked before any side effect and its answer is final.** ADR
/// 0019's rule for the VM is the rule here: a run either finishes on this
/// backend or fails before it begins, and there is no fallback to the VM or
/// to the tree walk. Nothing in [`FrameVm::run_entry`] re-checks, so a
/// refusal that this misses would be an `unreachable!` there and not a
/// quiet mixture.
///
/// The walk is over everything reachable from the entry through
/// `cove_ir::Inst::Call`, which is the only call in the admitted subset, so
/// the closure of the walk is exactly the set of functions a run can enter.
pub fn admits(program: &Program, module: &str, name: &str) -> Result<FunctionId, Refused> {
    let Some(entry) = program.function_named(module, name) else {
        return Err(Refused::nowhere(format!(
            "`{module}.{name}`, which this package does not declare"
        )));
    };
    let mut seen = vec![false; program.functions.len()];
    let mut queue = vec![entry];
    seen[entry.0 as usize] = true;
    while let Some(id) = queue.pop() {
        for reached in admits_function(program, id)? {
            if !seen[reached.0 as usize] {
                seen[reached.0 as usize] = true;
                queue.push(reached);
            }
        }
    }
    Ok(entry)
}

/// One function's shape and instructions, and the functions it calls.
fn admits_function(program: &Program, id: FunctionId) -> Result<Vec<FunctionId>, Refused> {
    let function = program.function(id);
    // Built where a refusal is built and not before it. `admits` runs once
    // per run, so this is not a hot path — but it is a `format!` per function
    // of the program on the way into every run that is *not* refused, and a
    // refusal is the only reader it has.
    let named = || format!("`{}.{}`", function.module, function.name);
    let mut calls = Vec::new();
    for (pc, inst) in function.code.iter().enumerate() {
        let span = function.span_at(pc);
        match inst {
            // The scalar core: everything here reads and writes words and
            // nothing else.
            Inst::ScalarConst(_)
            | Inst::LoadScalar(_)
            | Inst::StoreScalar(_)
            | Inst::ScalarPop
            | Inst::IntBinary(_)
            | Inst::Jump(_)
            | Inst::JumpIfFalseScalar(_)
            | Inst::JumpIfTrueScalar(_)
            | Inst::ReturnScalar => {}
            // A constant that had to be materialised because the builtin
            // below it takes a `Value`. Narrowed to the four kinds ADR 0028
            // decision 1 gives a word to: a `Str`, a `Name` and a `Duration`
            // are out of this slice's scope, and admitting one would let a
            // program run entirely at the boundary and never touch a word,
            // which is not what this backend is for.
            Inst::Const(id) => match program.constant(*id) {
                Const::Unit | Const::Bool(_) | Const::Int(_) | Const::Float(_) => {}
                Const::Str(_) | Const::Name(_) => {
                    return Err(Refused::new(format!("a string in {}", named()), span))
                }
                Const::Duration(_) => {
                    return Err(Refused::new(format!("a `Duration` in {}", named()), span))
                }
            },
            // The rest of the closed boundary list. See the module docs.
            Inst::Pop
            | Inst::ScalarToValue(_)
            | Inst::MakeBuiltin { .. }
            | Inst::Try
            | Inst::Return => {}
            Inst::Call {
                function: target,
                value_argc,
                place_argc,
                ..
            } => {
                // Belt and braces: the callee's own shape is checked when the
                // walk reaches it, and a call that brought a value or a place
                // would have to have been lowered from a callee that declared
                // one. Refusing here as well means the executing loop never
                // has to look.
                if *value_argc != 0 || *place_argc != 0 {
                    return Err(Refused::new(
                        format!("a call in {} that passes a general value", named()),
                        span,
                    ));
                }
                calls.push(*target);
            }
            other => {
                return Err(Refused::new(
                    format!("{} in {}", describe(other), named()),
                    span,
                ))
            }
        }
    }
    // The frame shape, after the instructions rather than before them, and
    // the order is a choice about what a refusal *says*. Either check would
    // do: both are static, both happen before any side effect, and a program
    // failing one usually fails the other. But `value_frame_size != 0` names
    // nothing a programmer wrote, while a `make-struct` in the body names the
    // struct — and the refusals are the roadmap, which is only useful if it
    // says what to build next. So the instructions speak first, and this is
    // what catches a function whose *shape* is out of reach although every
    // instruction in it is admitted.
    if function.value_frame_size != 0 {
        return Err(Refused::new(
            format!("{}, which holds a general value in a frame slot", named()),
            function.span,
        ));
    }
    if function.place_frame_size != 0 {
        return Err(Refused::new(
            format!("{}, which takes a `var` parameter", named()),
            function.span,
        ));
    }
    if !function.captures.is_empty() {
        return Err(Refused::new(
            format!("{}, which is a closure", named()),
            function.span,
        ));
    }
    if function.answers_a_task {
        return Err(Refused::new(
            format!("{}, which is `async`", named()),
            function.span,
        ));
    }
    if function.has_receiver {
        return Err(Refused::new(
            format!("{}, which takes a receiver", named()),
            function.span,
        ));
    }
    for kind in &function.params {
        if !matches!(kind, SlotKind::Scalar(_)) {
            return Err(Refused::new(
                format!("{}, whose parameters are not all `Int` or `Bool`", named()),
                function.span,
            ));
        }
    }
    Ok(calls)
}

/// What an instruction outside the admitted subset is called, in words a
/// Cove programmer would recognise.
///
/// Grouped by construct rather than by instruction, because that is how the
/// differential harness groups a refusal and how a reader thinks about one:
/// what stopped the run is `dyn` or a closure or a struct, not the
/// particular opcode the lowering chose for it.
/// How many words the one stack reserves when a `FrameVm` is built.
///
/// **This is a measurement fix and not a capacity guess**, which is why it is
/// this large for a stack `benches/arith` needs five words of.
///
/// Without it the buffer is whatever `Vec`'s doubling arrives at — 64 bytes
/// for `arith` — and where a 64-byte block lands inside a cache line is
/// decided by the process's allocator state. The two loop rows came back
/// **bimodal across processes of one unchanged binary**: `arith` at 91.1 ms
/// or 112.2 ms and `call` at 120.1 ms or 143.6 ms, in ten processes, always
/// the two together, each mode internally tight to under 2%, while the `Vm`
/// rows measured in the same processes held to under 1.5%. `benches/pure`,
/// whose frames are one word and whose hot data is the frame *stack* rather
/// than two locals in it, did not move at all.
///
/// Reserving 32 KB takes the allocation out of the size class where that is
/// decided, and the modes go with it. `docs/VM_ARCHITECTURE.md`, under "One
/// physical frame, measured", is the evidence and is honest about what it
/// does not establish: that the *mechanism* is cache-line straddling is the
/// hypothesis this fix was chosen from, and what is measured is that the
/// bimodality is gone.
///
/// It is also what a VM ought to do. Issue #212 asks that calls and returns
/// allocate nothing after warm capacity, and reserving is how a stack becomes
/// warm before the first call rather than during it.
const INITIAL_WORDS: usize = 4096;

fn describe(inst: &Inst) -> &'static str {
    match inst {
        Inst::LoadLocal(_) | Inst::StoreLocal(_) | Inst::Dup => "a general value slot",
        Inst::Unary(_) | Inst::Binary(_) => "an operator over a general value",
        Inst::ValueToScalar | Inst::GetFieldAt(_) | Inst::GetFieldAtScalar(_) => {
            "a general value read as a scalar"
        }
        Inst::JumpIfFalse(_) | Inst::JumpIfTrue(_) => "a branch on a general value",
        Inst::MakeClosure { .. } | Inst::CallValue { .. } => "a closure",
        Inst::MakeDyn { .. } | Inst::CallDyn { .. } => "`dyn` dispatch",
        Inst::CallHost { .. } | Inst::CallResource { .. } => "a Host call",
        Inst::CallBuiltin { .. } | Inst::CallBuiltinAssoc { .. } => "a builtin method",
        Inst::MakeArray(_) | Inst::MakeRange { .. } | Inst::IterItems | Inst::SpreadArgument => {
            "a collection"
        }
        Inst::Concat(_) => "string interpolation",
        Inst::MakeStruct { .. } | Inst::GetField(_) | Inst::SetField(_) => "a struct",
        Inst::MakeEnum { .. }
        | Inst::MakeHostEnum { .. }
        | Inst::TestCase(_)
        | Inst::GetPayload(_)
        | Inst::NoMatch => "an enum",
        Inst::PlaceLocal(_)
        | Inst::PlaceScalar(..)
        | Inst::LoadPlace(_)
        | Inst::PlaceField(_)
        | Inst::PlacePop
        | Inst::PlaceRead
        | Inst::PlaceWrite
        | Inst::Freeze => "a `var` parameter",
        Inst::EnterScope(_)
        | Inst::LeaveScope
        | Inst::CancelScope
        | Inst::Spawn
        | Inst::Await
        | Inst::Cancel
        | Inst::Lock => "a task",
        Inst::Snapshot => "a snapshot",
        // Everything the subset admits. Unreachable from `describe`'s one
        // caller, which is the `match`'s fallthrough arm.
        Inst::Const(_)
        | Inst::Pop
        | Inst::ScalarConst(_)
        | Inst::LoadScalar(_)
        | Inst::StoreScalar(_)
        | Inst::ScalarPop
        | Inst::IntBinary(_)
        | Inst::Jump(_)
        | Inst::JumpIfFalseScalar(_)
        | Inst::JumpIfTrueScalar(_)
        | Inst::ScalarToValue(_)
        | Inst::MakeBuiltin { .. }
        | Inst::Try
        | Inst::Call { .. }
        | Inst::Return
        | Inst::ReturnScalar => "an admitted instruction",
    }
}

/// One standing call: what is running, where its caller resumes, and where
/// the caller's frame begins.
///
/// Twelve bytes and `Copy`, which is what lets the running frame's base live
/// in a register of the dispatch loop and be restored from here on a return
/// without touching memory twice.
#[derive(Clone, Copy)]
struct Call {
    /// The function whose instructions are running.
    function: FunctionId,
    /// Where the caller resumes: the instruction after its `call`.
    return_pc: u32,
    /// Where this frame's words begin. A return truncates to it, which
    /// discards this frame's locals and the arguments it was given together,
    /// because they are the same storage.
    base: u32,
}

/// The eight-byte-frame backend.
///
/// One stack, one numbering, one base per frame. See the module docs for the
/// layout and the calling convention, and [`admits`] for what it runs.
pub struct FrameVm<'a> {
    runtime: &'a Runtime,
    program: &'a Program,
    /// The one stack. Every frame is a window of it and every operand stands
    /// above the running frame's window.
    words: Vec<u64>,
    /// One record per standing call.
    frames: Vec<Call>,
    /// Where a `Value` is materialised, and the only place one exists in a
    /// run of this backend.
    ///
    /// Not a frame and not indexed by one: the six boundary instructions
    /// push and pop it in the order the lowering emitted them, and
    /// [`admits`] refuses any function that would need one of its entries to
    /// survive a call.
    boundary: Vec<Value>,
    /// One `Value` per constant, made once per run rather than once per
    /// load, exactly as `Vm::constants` is and for the same reason.
    constants: Vec<Value>,
    /// How many instructions materialised a `Value` — constructed one,
    /// cloned one, dropped one, or matched on one.
    ///
    /// The measurement issue #212 asks for, kept as a counter rather than as
    /// a claim: one per *boundary instruction executed*, which is exactly the
    /// set of instructions in which a general `Value` exists at all. Eight for
    /// `benches/arith` and `benches/call`, all eight in the epilogue, and zero
    /// for every instruction inside their loops.
    materialized: u64,
    /// Fuel charged since the last safepoint, spent at the next one.
    fuel: u64,
    /// How many instructions this run executed. Exact, and unaffected by
    /// anything a rebuild can move.
    instructions: u64,
    budget: Option<Meter>,
    call_depth_limit: Option<usize>,
    timings: Vec<Timing>,
    wait: std::time::Duration,
    assertion_failure: Option<(Span, String)>,
    /// The deepest the one stack ever grew, in words. Reported rather than
    /// used: it is what "maximum stack capacity" means for this arrangement.
    high_water: usize,
}

impl<'a> FrameVm<'a> {
    /// A frame VM for `program`, running against `runtime` and its hosts.
    ///
    /// `hosts` is taken although nothing in the admitted subset calls one,
    /// because the run's budget is installed there and the caller builds it
    /// the way `cove run` builds it. Binding the budget here rather than at
    /// each safepoint is `Vm::bind_budget`'s decision and its measurement.
    pub fn new(runtime: &'a Runtime, hosts: &'a HostRegistry, program: &'a Program) -> Self {
        let (budget, call_depth_limit) = hosts
            .with_budget(|budget| (Some(budget.meter()), budget.limits().max_call_depth))
            .unwrap_or((None, None));
        FrameVm {
            runtime,
            program,
            words: Vec::with_capacity(INITIAL_WORDS),
            frames: Vec::with_capacity(MAX_CALL_DEPTH),
            boundary: Vec::new(),
            constants: program.constants.iter().map(constant).collect(),
            materialized: 0,
            fuel: 0,
            instructions: 0,
            budget,
            call_depth_limit,
            timings: Vec::new(),
            wait: std::time::Duration::ZERO,
            assertion_failure: None,
            high_water: 0,
        }
    }

    /// How many instructions this backend has executed.
    pub fn instructions(&self) -> u64 {
        self.instructions
    }

    /// How many instructions of the run materialised a `Value`, all of them
    /// at the boundary and none of them on a hot path.
    pub fn materialized(&self) -> u64 {
        self.materialized
    }

    /// The deepest the one stack grew, in eight-byte words.
    pub fn high_water_words(&self) -> usize {
        self.high_water
    }

    /// What this run allocated, which for the admitted subset is whatever
    /// the runtime already held: this backend owns no heap, because nothing
    /// it runs allocates a collection.
    pub fn heap_stats(&self) -> HeapStats {
        self.runtime.heap_stats()
    }

    /// Where the most recent assertion failed, and the message it produced.
    pub fn assertion_failure(&self) -> Option<(Span, &str)> {
        self.assertion_failure
            .as_ref()
            .map(|(span, message)| (*span, message.as_str()))
    }

    /// What the run spent waiting on hosts, which is zero: the admitted
    /// subset reaches none.
    pub fn wait(&self) -> std::time::Duration {
        self.wait
    }

    /// Runs the entry `module.name` and reports how the run ended.
    ///
    /// The same seam `Vm::run_entry` and `Interpreter::run_entry` are, and
    /// it answers the same way, so choosing this backend chooses which of
    /// the three to build and decides nothing else.
    ///
    /// `args` is refused rather than passed: an entry that takes process
    /// arguments takes an `Array<String>`, which is a value slot, and
    /// [`admits`] has already refused such an entry. It is taken so that the
    /// three seams have one shape.
    pub fn run_entry(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        let outcome = self.invoke_entry(module, name, args);
        let (classification, message) = match &outcome {
            Ok(value) if value.is_err() => (RunOutcome::Error, returned_error_message(value)),
            Ok(_) => (RunOutcome::Success, None),
            Err(error) => (error.outcome, Some(error.message.clone())),
        };
        self.runtime.trace(TraceEvent::RunEnded {
            outcome: classification,
            message,
        });
        outcome
    }

    fn invoke_entry(
        &mut self,
        module: &str,
        name: &str,
        args: Vec<Rc<str>>,
    ) -> Result<Value, RuntimeError> {
        let id = admits(self.program, module, name).map_err(|refused| {
            let error = RuntimeError::new(refused.to_string()).with_rule(
                "A run on the 8-byte frame either finishes on it or fails before any side \
                 effect; it never falls back to another backend.",
            );
            match refused.span {
                Some(span) => error.at(span),
                None => error,
            }
        })?;
        let entry = self.program.function(id);
        if entry.arity != 0 {
            // Unreachable through `admits`, which refuses a value parameter,
            // and stated rather than assumed because the alternative is a
            // silent mixture.
            return Err(RuntimeError::new(format!(
                "entry `{module}.{name}` declares {} parameter(s), which the 8-byte frame cannot supply",
                entry.arity
            ))
            .at(entry.span));
        }
        drop(args);
        self.run(id)
    }

    /// Opens the entry's frame and runs it to its answer.
    fn run(&mut self, function: FunctionId) -> Result<Value, RuntimeError> {
        let entry = self.program.function(function);
        self.words.clear();
        self.frames.clear();
        self.boundary.clear();
        self.fuel = 0;
        self.words.resize(entry.scalar_frame_size as usize, 0);
        self.frames.push(Call {
            function,
            return_pc: 0,
            base: 0,
        });

        self.runtime.trace(TraceEvent::EntryEnter {
            module: entry.module.to_string(),
            function: entry.name.to_string(),
        });
        self.timings.push(Timing::start());
        let outcome = self.execute();
        let timing = self
            .timings
            .pop()
            .expect("a run pushes exactly the one timing it pops");
        self.wait = timing.wait();
        self.runtime.trace(TraceEvent::EntryExit {
            module: entry.module.to_string(),
            function: entry.name.to_string(),
            cpu: timing.cpu(),
            wait: timing.wait(),
        });
        self.spend_pending_fuel();
        let heap = self.heap_stats();
        self.runtime.trace(TraceEvent::HeapSummary {
            allocated: heap.allocated_objects,
            allocated_bytes: heap.allocated_bytes,
            collections: heap.collections,
            live_bytes: heap.live_bytes,
            peak_bytes: heap.peak_bytes,
            pause: heap.pause,
        });
        outcome
    }

    /// The dispatch loop.
    ///
    /// `base` and `pc` are locals rather than fields for the reason
    /// `Vm::execute` keeps its `Frame` in one: they are read by every
    /// instruction that addresses a word, and a field would be a load
    /// through `self` on each.
    fn execute(&mut self) -> Result<Value, RuntimeError> {
        let program = self.program;
        let standing = *self.frames.last().expect("the caller pushed a frame");
        let mut base = standing.base as usize;
        let mut running = program.function(standing.function);
        let mut code: &[Inst] = &running.code;
        let mut blocks: &[u32] = &running.block_fuel;
        let mut pc = 0usize;

        // Entering a call is a safepoint and the entry is a call, so a run
        // cancelled before it began stops before its first instruction.
        self.safepoint(running.span)?;
        self.charge(blocks[0], || running.span_at(0))?;

        loop {
            match code[pc] {
                // ------------------------------------------ the scalar core
                Inst::ScalarConst(value) => self.words.push(Word::of_int(value)),
                Inst::LoadScalar(slot) => {
                    let word = self.words[base + slot as usize];
                    self.words.push(word);
                }
                Inst::StoreScalar(slot) => {
                    let word = self.pop_word();
                    self.words[base + slot as usize] = word;
                }
                Inst::ScalarPop => {
                    self.pop_word();
                }
                Inst::IntBinary(op) => {
                    let rhs = Word::int(self.pop_word());
                    let lhs = Word::int(self.pop_word());
                    let answer = int_binary(op, lhs, rhs, running.span_at(pc))?;
                    self.words.push(Word::of_int(answer));
                }
                Inst::Jump(to) => {
                    let to = to as usize;
                    if to <= pc {
                        self.back_edge(running.span_at(pc))?;
                    }
                    self.charge(blocks[to], || running.span_at(to))?;
                    pc = to;
                    continue;
                }
                Inst::JumpIfFalseScalar(to) => {
                    // A word the layout calls `Bool` is 0 or 1, so the word
                    // *is* the answer. `Vm` reads it the same way.
                    let to = to as usize;
                    if self.pop_word() == 0 {
                        if to <= pc {
                            self.back_edge(running.span_at(pc))?;
                        }
                        self.charge(blocks[to], || running.span_at(to))?;
                        pc = to;
                        continue;
                    }
                    self.charge(blocks[pc + 1], || running.span_at(pc + 1))?;
                }
                Inst::JumpIfTrueScalar(to) => {
                    let to = to as usize;
                    if self.pop_word() != 0 {
                        if to <= pc {
                            self.back_edge(running.span_at(pc))?;
                        }
                        self.charge(blocks[to], || running.span_at(to))?;
                        pc = to;
                        continue;
                    }
                    self.charge(blocks[pc + 1], || running.span_at(pc + 1))?;
                }

                // ------------------------------------------- call and return
                Inst::Call {
                    function: target,
                    scalar_argc,
                    ..
                } => {
                    let span = running.span_at(pc);
                    let callee = program.function(target);
                    self.enter(callee, span)?;
                    self.charge(callee.block_fuel[0], || callee.span_at(0))?;
                    // The arguments the caller pushed *are* the callee's
                    // first words. Nothing is transferred; the base moves.
                    let callee_base = self.words.len() - scalar_argc as usize;
                    self.words
                        .resize(callee_base + callee.scalar_frame_size as usize, 0);
                    if self.words.len() > self.high_water {
                        self.high_water = self.words.len();
                    }
                    self.frames.push(Call {
                        function: target,
                        return_pc: pc as u32 + 1,
                        base: callee_base as u32,
                    });
                    base = callee_base;
                    running = callee;
                    code = &callee.code;
                    blocks = &callee.block_fuel;
                    pc = 0;
                    continue;
                }
                Inst::ReturnScalar => {
                    self.safepoint(running.span_at(pc))?;
                    let answer = self.pop_word();
                    let done = self.frames.pop().expect("a return leaves a frame");
                    self.words.truncate(done.base as usize);
                    match self.frames.last().copied() {
                        Some(caller) => {
                            self.words.push(answer);
                            base = caller.base as usize;
                            running = program.function(caller.function);
                            code = &running.code;
                            blocks = &running.block_fuel;
                            pc = done.return_pc as usize;
                            self.charge(blocks[pc], || running.span_at(pc))?;
                            continue;
                        }
                        // The run's own answer. A scalar word has no tag, so
                        // the callee's `returns` is what says which `Value`
                        // it stands for — the one boundary a whole run has.
                        None => {
                            let returns = program.function(done.function).returns;
                            self.materialized += 1;
                            return Ok(match returns {
                                SlotKind::Scalar(Scalar::Int) => Value(Repr::Int(Word::int(answer))),
                                SlotKind::Scalar(Scalar::Bool) => {
                                    Value(Repr::Bool(Word::canonical_bool(answer)))
                                }
                                other => unreachable!(
                                    "`return-scalar` was reached in a function that answers {other:?}"
                                ),
                            });
                        }
                    }
                }

                // --------------------------------------------- the boundary
                Inst::Const(id) => {
                    self.materialized += 1;
                    self.boundary.push(self.constants[id.0 as usize].clone());
                }
                Inst::Pop => {
                    self.materialized += 1;
                    self.pop_value();
                }
                Inst::ScalarToValue(what) => {
                    let word = self.pop_word();
                    self.materialized += 1;
                    self.boundary.push(match what {
                        Scalar::Int => as_value_of(Scalar::Int, Word::int(word)),
                        // The one place a non-canonical `Bool` word could
                        // become a `Value` an embedder sees, so the one place
                        // the invariant is asked about.
                        Scalar::Bool => Value(Repr::Bool(Word::canonical_bool(word))),
                    });
                }
                Inst::MakeBuiltin { name: which, argc } => {
                    let span = running.span_at(pc);
                    let which = const_name(program, which);
                    let at = self.boundary.len() - argc as usize;
                    let mut values: Vec<Value> = self.boundary.drain(at..).collect();
                    self.materialized += 1;
                    let answer =
                        self.make_builtin(which, &mut values, running.arg_spans_at(pc), span);
                    self.boundary.push(answer?);
                }
                Inst::Try => {
                    let span = running.span_at(pc);
                    let value = self.pop_value();
                    self.materialized += 1;
                    match opened(value, span)? {
                        Ok(payload) => {
                            self.boundary.push(payload);
                            self.charge(blocks[pc + 1], || running.span_at(pc + 1))?;
                        }
                        Err(failure) => {
                            self.safepoint(span)?;
                            match self.leave_with_value(failure, &mut base) {
                                Ok(value) => return Ok(value),
                                Err(resumed) => {
                                    running = program.function(
                                        self.frames.last().expect("a caller stands").function,
                                    );
                                    code = &running.code;
                                    blocks = &running.block_fuel;
                                    pc = resumed;
                                    self.charge(blocks[pc], || running.span_at(pc))?;
                                    continue;
                                }
                            }
                        }
                    }
                }
                Inst::Return => {
                    self.safepoint(running.span_at(pc))?;
                    self.materialized += 1;
                    let value = self.pop_value();
                    match self.leave_with_value(value, &mut base) {
                        Ok(value) => return Ok(value),
                        Err(resumed) => {
                            running = program
                                .function(self.frames.last().expect("a caller stands").function);
                            code = &running.code;
                            blocks = &running.block_fuel;
                            pc = resumed;
                            self.charge(blocks[pc], || running.span_at(pc))?;
                            continue;
                        }
                    }
                }

                // `admits` refused every other instruction before the run
                // began, so reaching one is a broken invariant of this
                // backend and never a program that could be told about it.
                other => unreachable!(
                    "`admits` refuses {other:?}, and one reached the dispatch loop in `{}.{}`",
                    running.module, running.name
                ),
            }
            pc += 1;
        }
    }

    /// Pops a frame whose answer is a materialised `Value`.
    ///
    /// `Ok(value)` when the run is over and `Err(resumed)` when a caller
    /// stands, in which case `base` has been moved to the caller's.
    fn leave_with_value(&mut self, value: Value, base: &mut usize) -> Result<Value, usize> {
        let done = self.frames.pop().expect("a return leaves a frame");
        self.words.truncate(done.base as usize);
        match self.frames.last().copied() {
            Some(caller) => {
                self.boundary.push(value);
                *base = caller.base as usize;
                Err(done.return_pc as usize)
            }
            None => Ok(value),
        }
    }

    /// The top of the one stack.
    ///
    /// `cove_ir::lower::validate` simulated the depth of every instruction
    /// control can reach before this backend was handed the program, so an
    /// empty stack here is a broken invariant rather than a program that
    /// could be told about it. It is `Vm::pop_scalar`'s argument word for
    /// word.
    #[inline(always)]
    fn pop_word(&mut self) -> u64 {
        self.words
            .pop()
            .expect("a validated instruction takes only words that are there")
    }

    /// The top of the boundary buffer, with the same argument.
    #[inline]
    fn pop_value(&mut self) -> Value {
        self.boundary
            .pop()
            .expect("a validated boundary instruction takes only values that are there")
    }

    /// Opens a call: the two depth bounds, and the safepoint every call is.
    ///
    /// `Vm::enter` word for word, including which message each bound
    /// produces, because a recursion that stops on one backend and answers
    /// on another is the one difference between two backends that is not
    /// allowed to exist.
    fn enter(&mut self, callee: &cove_ir::Function, span: Span) -> Result<(), RuntimeError> {
        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(RuntimeError::new(format!(
                "call depth limit of {MAX_CALL_DEPTH} reached while calling `{}`",
                callee.name
            ))
            .at(span)
            .with_rule("Recursion depth is a runtime control, not a proof obligation."));
        }
        let depth = self.frames.len() + 1;
        if let Some(limit) = self.call_depth_limit {
            if depth > limit {
                if let Some(budget) = &self.budget {
                    return Err(budget.to_runtime_error(Stopped::CallDepth).at(span));
                }
            }
        }
        self.safepoint(span)
    }

    /// Charges a block's worth of instructions, and takes a safepoint once
    /// [`SAFEPOINT_INTERVAL`] has gathered.
    ///
    /// `Vm::charge`, against the same constant and with the same two lines
    /// doing both counters, which is most of why charging by the block is
    /// cheap.
    #[inline(always)]
    fn charge(&mut self, block: u32, span: impl FnOnce() -> Span) -> Result<(), RuntimeError> {
        let count = u64::from(block);
        self.instructions += count;
        self.fuel += count * INSTRUCTION_FUEL;
        if self.fuel >= SAFEPOINT_INTERVAL {
            self.safepoint(span())?;
        }
        Ok(())
    }

    /// A safepoint at a loop's back edge, taken once [`BACK_EDGE_FUEL`] has
    /// gathered. `Vm::back_edge`, against the same constant.
    #[inline(always)]
    fn back_edge(&mut self, span: Span) -> Result<(), RuntimeError> {
        if self.fuel >= BACK_EDGE_FUEL {
            self.safepoint(span)?;
        }
        Ok(())
    }

    /// Spends the fuel charged since the last safepoint and asks the budget
    /// whether the run may continue.
    ///
    /// `Vm::safepoint` without two of its three parts, each absent for a
    /// stated reason rather than forgotten.
    ///
    /// **No collection**, because this backend owns no heap: nothing in the
    /// admitted subset allocates a collection, so there is nothing to sweep.
    ///
    /// **No `crate::interp::stopped_here`**, because both lists it reads are
    /// empty here by construction: this backend runs no spawned task, so it
    /// owns no task cancellation flag, and it makes no Host call, so it is
    /// inside no bounded call that could raise one. What remains is the
    /// run's own cancellation, and that is the *budget's* flag —
    /// `crate::budget::Budget::safepoint` is where ADR 0024 puts it, and it
    /// is asked one line below on exactly the schedule `Vm` asks it on.
    fn safepoint(&mut self, span: Span) -> Result<(), RuntimeError> {
        let fuel = std::mem::take(&mut self.fuel);
        if let Some(budget) = &self.budget {
            if let Err(stopped) = budget.safepoint(fuel) {
                return Err(budget.to_runtime_error(stopped).at(span));
            }
        }
        Ok(())
    }

    /// Spends whatever this run charged and had not yet handed over, however
    /// the run ended. `Vm::spend_pending_fuel`, and its argument applies word
    /// for word: a run that raised left through Rust's `?` rather than
    /// through an instruction, and the fuel it had charged was work it really
    /// did.
    fn spend_pending_fuel(&mut self) {
        let fuel = std::mem::take(&mut self.fuel);
        if fuel != 0 {
            if let Some(budget) = &self.budget {
                budget.spend(fuel);
            }
        }
    }

    /// Calls `function` with `arguments` already in words, and answers the
    /// word it returned.
    ///
    /// The mechanism hook the `Float` test needs and nothing else uses: a
    /// `Float` cannot arrive through a Cove source, because `cove_ir::Scalar`
    /// is `Int | Bool` and a `Float` is still lowered as a general value, so
    /// the only way to put all 64 bits into a frame slot is to put them
    /// there. It is `#[cfg(test)]` because it hands a raw word across, which
    /// is exactly what ADR 0028 decision 0's visibility column forbids of a
    /// public signature.
    #[cfg(test)]
    fn call_for_test(&mut self, function: FunctionId, arguments: &[u64]) -> Option<u64> {
        let callee = self.program.function(function);
        self.words.clear();
        self.frames.clear();
        self.boundary.clear();
        self.fuel = 0;
        self.words.extend_from_slice(arguments);
        self.words.resize(callee.scalar_frame_size as usize, 0);
        self.frames.push(Call {
            function,
            return_pc: 0,
            base: 0,
        });
        match self.execute().ok()? {
            Value(Repr::Int(answer)) => Some(Word::of_int(answer)),
            _ => None,
        }
    }

    /// The one builtin constructor or assertion the boundary reaches.
    ///
    /// `Vm::make_builtin` for the two kinds `benches/arith` and
    /// `benches/call` need — a constructor such as `Ok`, and an assertion
    /// such as `assertEqual`, whose diagnostic quotes the source of its own
    /// argument. Neither re-enters the evaluator, which is why this backend
    /// can offer them without a `Callable`.
    fn make_builtin(
        &mut self,
        which: &str,
        values: &mut Vec<Value>,
        arg_spans: &[Span],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let assertion =
            free_builtin(which).is_some_and(|schema| schema.kind == FreeBuiltinKind::Assertion);
        if !assertion {
            return crate::builtins::call_constructor(which, values, span);
        }
        let sources: Vec<&str> = arg_spans
            .iter()
            .map(|span| source_text(self.runtime.sources(), *span))
            .collect();
        let outcome = crate::builtins::call_assertion(which, values, &sources, span)?;
        if let Some(payload) = outcome.err_payload() {
            self.assertion_failure = Some((span, payload[0].to_string()));
        }
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests;
