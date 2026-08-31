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
//! # Two phases, and which one this is
//!
//! **Phase A** ran `benches/arith`, `benches/call` and `benches/pure` over one
//! contiguous `Vec<u64>`, and no word of it was ever a reference:
//! [`admits`] refused any function with a nonzero `value_frame_size`, which is
//! what made its "no `Value` in the hot path" claim structural rather than
//! careful. It priced a call and a return at 14.4 ns against the VM's 38.3 —
//! *in its own build*, which is the only kind of comparison ADR 0029 allows —
//! and said nothing at all about what a *rooted* frame costs.
//!
//! **Phase B** is this: a word-wide slot stack with a GC bitmap, which is
//! [issue #162](https://github.com/myuon/cove/issues/162)'s Design B proper. A
//! frame word may now be a reference into a VM-owned traced object heap, and
//! `benches/field` and `benches/method` run on it. What that adds, and what it
//! costs, is under "The bitmap" below and in `docs/VM_ARCHITECTURE.md` under
//! "What a rooted frame costs to walk".
//!
//! [ADR 0033](../../../docs/adr/0033-an-identity-is-not-a-vm-heap-object.md)
//! is what says a struct may be in that heap at all, and it is binding: plain
//! copyable aggregates — strings, arrays, structs, ordinary enums — are
//! candidates for the VM-owned handle heap, and the five identity-bearing
//! kinds are **not**. Nothing here puts a `Vector`, a `Shared`, a `Task`, a
//! `TaskScope` or a `Resource` in it; [`admits`] refuses every one of them by
//! name, as it did before.
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
//! A frame is `words[base .. base + width]`, where `width` is the callee's two
//! frame sizes added. **Parameters, locals and temporaries are one index space
//! from one base**: parameter `i` is `base + i`, the body's locals follow it
//! densely, and a temporary is pushed above `base + width` and addressed by
//! nothing. That is the whole of the arrangement; there is no second stack, no
//! second base, and no second count on a call.
//!
//! The lowering still numbers *two* spaces — an `Inst::LoadScalar` addresses
//! one and an `Inst::LoadLocal` the other — and `FrameMap` is what makes
//! them one region from one base, which is decision 1's "every physical offset
//! derives from the one frame layout" met by deriving the map rather than by
//! having been given it. Deriving it in `cove_ir` instead is Phase C's.
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
//! | `Unit` | a canonical zero word where the layout cannot omit it |
//! | a struct | a handle into this backend's traced object heap |
//!
//! The bits are not self-describing. What a word means comes from the
//! instruction that touches it and from `cove_ir::Function`'s per-slot
//! metadata, both of which are the checker's answers written down at
//! lowering time. `Word` is the whole of the codec and it is a
//! reinterpretation in both directions: nothing is truncated, tagged, or
//! canonicalised on the way in.
//!
//! `Float` is included because ADR 0028 decides it, and it is *not* exercised
//! by any of the four rows this backend runs: `cove_ir::Scalar` is `Int | Bool`
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
//!   it reads one, because the checker settled that before lowering. A zero
//!   word is *also* never a live handle — `crate::slot` never issues
//!   generation zero — so a frame with reference slots is opened by the same
//!   instruction as one without, and a walk that reaches an unwritten value
//!   slot finds nothing there rather than something arbitrary.
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
//! # The bitmap, and how a word is known to be a reference
//!
//! **One bit per word of the one stack, packed sixty-four to a limb.** That is
//! the whole of what a collection consults; the words themselves say nothing,
//! and ADR 0028 decision 1's invariant — "a slot the layout calls scalar must
//! never be reachable by a walk that treats it as a reference" — holds here
//! because the walk has no other thing it *could* read.
//!
//! A bit is written by one of three authorities, and never by looking at the
//! word:
//!
//! | where the word is | what says whether it is a reference |
//! | --- | --- |
//! | a frame slot | `FrameMap`, derived from `cove_ir::Function`'s two frame sizes; one masked pass per call |
//! | an operand pushed by the scalar core, a `const`, or a `make-struct` | the instruction, which knows what it pushed |
//! | an operand pushed by a field read | the **object's** reference map, `crate::slot::HandleHeap::word_is_reference` |
//!
//! The third is the one that cannot be static: `get-field-at` is one
//! instruction whose answer is a handle for a struct-typed field and scalar
//! bits for an `Int` one, and only decision 2's reference map knows which.
//!
//! The first has a condition attached, and [`admits`] is what enforces it. The
//! frame map calls **every** value slot a reference, so a value slot that held
//! anything else would be scalar bits the walk reads as a handle — decision
//! 1's invariant broken from the other side. The lowering can produce one:
//! ADR 0027 records that a declaration reached through a value is lowered
//! "with every argument on the value stack", so a slot `cove_ir` calls
//! `SlotKind::Value` may hold an `Int`. So a `store-local`, and a value
//! argument of a call, are admitted only where the instruction that pushed the
//! word says it is a reference.
//!
//! **A pop writes no bit.** The word above the top is stale and is never read,
//! because the walk stops at `words.len()` and every push writes its own bit
//! before that word is inside the walk. So the bitmap costs a masked store per
//! push, a masked pass over `width / 64` limbs per call, and nothing per pop
//! or per return.
//!
//! ## What the walk yields
//!
//! `FrameRoots` is the whole root set: every set bit below `words.len()`,
//! then the shadow stack. ADR 0028 decision 8's three multiplicities:
//!
//! 1. **Root storage locations are yielded once** — one bit, one visit. A
//!    struct standing in a frame slot and in an operand word is *two*
//!    locations and is yielded twice, which is not a fault and is what
//!    `a_reference_in_a_slot_and_in_an_operand_is_two_locations_and_one_expansion`
//!    pins.
//! 2. **Real graph edges counted once each** does not arise, because there is
//!    no `Rc::strong_count` to compare against. That absence is the reason a
//!    bitmap over words is sound where a shadow stack over `Value` would not
//!    be — [PR #210](https://github.com/myuon/cove/pull/210)'s finding, which
//!    ADR 0033 preserves.
//! 3. **Objects are expanded once during marking** —
//!    `crate::slot::HandleCollection::expansions`, asserted equal to the
//!    live set on every collection of a real run.
//!
//! ## The shadow stack is empty, and that is a finding
//!
//! `crate::slot::TempRoots` is wired and stays at depth zero for the whole of an admitted
//! run. ADR 0028 decision 8's third candidate mechanism — "the dispatch
//! discipline guarantees that a collection can occur only when every live
//! handle has been returned to a mapped VM slot" — is *false* for `Vm` at the
//! five places `crate::slot`'s module documentation names, and is **true here
//! by construction**: a one-stack backend has nowhere else to put an operand.
//! It stops being free the moment an aggregate crosses decision 5's boundary,
//! which is Phase C's problem, so the mechanism is present and empty rather
//! than absent — `nothing_is_rooted_outside_the_one_stack` reads it.
//!
//! # The boundary, and where a `Value` is allowed to be
//!
//! Issue #212's hard constraint is that **no general Rust `Value` is
//! constructed, cloned, dropped or pattern-matched in the hot execution
//! path**. This backend keeps it structurally rather than by care:
//!
//! - **No frame word is ever a `Value`.** There is no `Vec<Value>` frame here
//!   to be one: a value slot holds a handle, and what it names is words.
//! - `const` and `scalar-to-value` **no longer materialise anything**, which is
//!   the change Phase A did not make. A constant this backend admits is one of
//!   decision 1's four kinds and therefore *is* eight bytes, and a word the
//!   checker settled as an `Int` is the same eight bytes on both sides of a
//!   conversion — so `scalar-to-value` and `value-to-scalar` are nothing at
//!   all. That is ADR 0027's per-read crossing removed rather than narrowed.
//! - Four instructions materialise a `Value`: `make-builtin`, over its
//!   arguments and its answer, and `try`, `pop` and `return` over what one
//!   left. They hold their operands in a buffer that is not a frame: nothing
//!   indexes it, no frame owns a window of it, and [`admits`] refuses a
//!   function that would need one of its entries to survive a call.
//! - Every one of them increments [`FrameVm::materialized`], so the claim is a
//!   number a test reads rather than a sentence. `benches/arith`,
//!   `benches/call`, `benches/pure`, `benches/field` and `benches/method` each
//!   report **8**, all eight in the epilogue, and every loop reports zero —
//!   including the two whose loops build and mutate a struct.
//!
//! This is ADR 0028 decision 5 — "`Value` is materialized at the boundary,
//! and the boundary list is closed" — with the list written out.
//!
//! # What it refuses
//!
//! Everything else, by name, before any side effect, with no fallback. See
//! [`admits`]. In particular there is no `Dynamic`, no `dyn`, no `Any`, no
//! enum layout, no place, no `var`, no closure, no Host call, no task, no
//! string and no collection — and none of ADR 0033's five identity-bearing
//! kinds, which that ADR puts outside this heap on purpose. What Phase B adds
//! to the admitted subset is the struct, and nothing else.

use std::rc::Rc;

use cove_diag::Span;
use cove_ir::{Const, ConstId, FunctionId, Inst, Program, Scalar, SlotKind};
use cove_schema::builtins::{free_builtin, FreeBuiltinKind};

use crate::budget::Meter;
use crate::error::RuntimeError;
use crate::heap::HeapStats;
use crate::host::HostRegistry;
use crate::interp::{returned_error_message, source_text, MAX_CALL_DEPTH};
use crate::runtime::Runtime;
use crate::slot::{Handle, HandleHeap, HandleRoots, Layout, LayoutId, Part, TempRoots};
use crate::trace::{RunOutcome, Timing, TraceEvent};
use crate::value::{Repr, Value};
use crate::vm::{
    as_value_of, int_binary, name as const_name, opened, BACK_EDGE_FUEL, INSTRUCTION_FUEL,
    SAFEPOINT_INTERVAL,
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
    let structs = struct_words(program);
    let fields = field_positions(program, &structs);
    let mut seen = vec![false; program.functions.len()];
    let mut queue = vec![entry];
    seen[entry.0 as usize] = true;
    while let Some(id) = queue.pop() {
        for reached in admits_function(program, id, &structs, &fields)? {
            if !seen[reached.0 as usize] {
                seen[reached.0 as usize] = true;
                queue.push(reached);
            }
        }
    }
    Ok(entry)
}

/// Whether the instruction before `pc` leaves a materialised `Value` in the
/// boundary buffer.
///
/// The boundary buffer holds `Value`s and the one stack holds words, and
/// nothing converts between them except the instructions listed here. So a
/// `try`, a `pop` or a `return` is admitted only where the thing it consumes
/// really is one of these, which is what stops the two universes from being
/// read into each other by accident.
fn leaves_a_boundary_value(program: &Program, function: &cove_ir::Function, pc: usize) -> bool {
    match function.code.get(pc.wrapping_sub(1)) {
        Some(Inst::MakeBuiltin { .. } | Inst::Try) => true,
        Some(Inst::Call {
            function: target, ..
        }) => !matches!(program.function(*target).returns, SlotKind::Scalar(_)),
        _ => false,
    }
}

/// One function's shape and instructions, and the functions it calls.
fn admits_function(
    program: &Program,
    id: FunctionId,
    structs: &[Option<StructWords>],
    fields: &[Option<u32>],
) -> Result<Vec<FunctionId>, Refused> {
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
            // The reference core: every one of these reads or writes a word
            // the frame's reference map or an object's calls a handle, and
            // none of them builds a `Value`.
            Inst::LoadLocal(_)
            | Inst::Dup
            | Inst::ValueToScalar
            | Inst::GetFieldAt(_)
            | Inst::GetFieldAtScalar(_)
            | Inst::ScalarToValue(_) => {}
            // **A value slot holds a handle, and this is what makes that
            // true.** The frame map calls every value slot a reference, so
            // storing anything else into one would put bits the walk reads as
            // a handle where the layout says a handle is -- which is exactly
            // the invariant ADR 0028 decision 1 states for any physical
            // arrangement, from the other side.
            //
            // The lowering can produce such a store: ADR 0027 records that a
            // declaration reached through a value is lowered "with every
            // argument on the value stack", so a slot `cove_ir` calls
            // `SlotKind::Value` may hold an `Int` at run time. Nothing in the
            // rows this backend runs does, and rather than rely on that, the
            // instruction that pushed the word has to say it is a reference.
            Inst::StoreLocal(_) => {
                if pushed_kinds(program, function, pc, 1).as_deref() != Some(&[Kind::Reference]) {
                    return Err(Refused::new(
                        format!(
                            "a general value slot in {} that the 8-byte frame cannot show holds \
                             a heap object",
                            named()
                        ),
                        span,
                    ));
                }
            }
            // A constant is a word here rather than a `Value`. Narrowed to
            // the four kinds ADR 0028 decision 1 gives a word to: a `Str`, a
            // `Name` and a `Duration` have no eight-byte form and are out of
            // this backend's scope.
            Inst::Const(id) => match program.constant(*id) {
                Const::Unit | Const::Bool(_) | Const::Int(_) | Const::Float(_) => {}
                Const::Str(_) | Const::Name(_) => {
                    return Err(Refused::new(format!("a string in {}", named()), span))
                }
                Const::Duration(_) => {
                    return Err(Refused::new(format!("a `Duration` in {}", named()), span))
                }
            },
            Inst::MakeStruct { ty, .. } => {
                if structs[ty.0 as usize].is_none() {
                    return Err(Refused::new(
                        format!(
                            "`{}` in {}, whose fields the 8-byte frame cannot read off its \
                             construction",
                            const_name(program, *ty),
                            named()
                        ),
                        span,
                    ));
                }
            }
            Inst::SetField(id) => {
                if fields[id.0 as usize].is_none() {
                    return Err(Refused::new(
                        format!(
                            "a write to `.{}` in {}, which names no field of one settled struct",
                            const_name(program, *id),
                            named()
                        ),
                        span,
                    ));
                }
            }
            // The boundary. A `make-builtin` is admitted where the words its
            // arguments stand in can be read as the `Value`s it wants, and the
            // three that consume one are admitted where what they consume
            // really is one.
            Inst::MakeBuiltin { argc, .. } => {
                if boundary_kinds(program, function, pc, *argc as usize).is_none() {
                    return Err(Refused::new(
                        format!(
                            "a builtin call in {}, whose arguments the 8-byte frame cannot read \
                             as values",
                            named()
                        ),
                        span,
                    ));
                }
            }
            Inst::Pop | Inst::Try | Inst::Return => {
                if !leaves_a_boundary_value(program, function, pc) {
                    return Err(Refused::new(
                        format!(
                            "{} in {}, over something that is a word rather than a value",
                            match inst {
                                Inst::Pop => "a discarded value",
                                Inst::Try => "a `?`",
                                _ => "a `return`",
                            },
                            named()
                        ),
                        span,
                    ));
                }
            }
            Inst::Call {
                function: target,
                scalar_argc,
                value_argc,
                place_argc,
                ..
            } => {
                if *place_argc != 0 {
                    return Err(Refused::new(
                        format!("a call in {} that passes a `var` argument", named()),
                        span,
                    ));
                }
                // The arguments become the callee's first words without
                // moving, and the callee's frame puts one kind of slot first.
                // A call that passes both kinds would need them interleaved
                // the way the caller pushed them, which no single frame map
                // can describe. See [`FrameMap`].
                if *value_argc != 0 && *scalar_argc != 0 {
                    return Err(Refused::new(
                        format!(
                            "a call in {} that passes both a value and a scalar argument",
                            named()
                        ),
                        span,
                    ));
                }
                // A value argument becomes a value slot of the callee without
                // moving, so it is the same question `Inst::StoreLocal` asks:
                // the frame map will call that word a reference, so the
                // instruction that pushed it has to say it is one.
                if *value_argc != 0
                    && !pushed_kinds(program, function, pc, *value_argc as usize)
                        .is_some_and(|kinds| kinds.iter().all(|kind| *kind == Kind::Reference))
                {
                    return Err(Refused::new(
                        format!(
                            "a call in {} whose value argument the 8-byte frame cannot show is a \
                             heap object",
                            named()
                        ),
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
    // A receiver is parameter 0 and nothing else, now that a parameter may be
    // a reference: `method.Cursor.position` takes its `Cursor` in the frame's
    // first word, and the word is a handle because the frame map says so.
    for kind in &function.params {
        if matches!(kind, SlotKind::Place) {
            return Err(Refused::new(
                format!("{}, which takes a `var` parameter", named()),
                function.span,
            ));
        }
    }
    // One kind of parameter, because they arrive without moving and the frame
    // puts the scalars first. See [`FrameMap`].
    if function
        .params
        .iter()
        .any(|kind| matches!(kind, SlotKind::Value))
        && function.params.iter().any(|kind| kind.is_scalar())
    {
        return Err(Refused::new(
            format!(
                "{}, which takes both a value and a scalar parameter",
                named()
            ),
            function.span,
        ));
    }
    // And a value parameter stands at the frame base, which is scalar slot 0's
    // place, so a function that has both is one whose parameters cannot arrive
    // without moving. What would remove this is the lowering numbering one
    // space; see [`FrameMap`].
    if function
        .params
        .iter()
        .any(|kind| matches!(kind, SlotKind::Value))
        && function.scalar_frame_size != 0
    {
        return Err(Refused::new(
            format!(
                "{}, which takes a value parameter and also keeps a scalar slot",
                named()
            ),
            function.span,
        ));
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

/// How many sixty-four-word limbs the bitmap reserves.
///
/// [`INITIAL_WORDS`]'s argument applies to the bitmap and applies *harder*,
/// which is a thing Phase B found rather than assumed. The bitmap's limbs are
/// a second small heap buffer that the hot loop touches on every push, so it
/// is exactly the size class the bimodality of "The reservation is a
/// measurement fix" lived in — one bit per word means the sixty-four limbs
/// `INITIAL_WORDS` implies are 512 bytes, and a 512-byte block's placement
/// inside a cache line is decided by the process's allocator state.
///
/// So it reserves 4 KB and not 512 bytes, for a bitmap every admitted row uses
/// two limbs of. The number is a size class, not a capacity.
const INITIAL_LIMBS: usize = 512;

// ---------------------------------------------------- the GC bitmap

/// One bit per word of the one stack: whether the word is a reference.
///
/// This is issue #162's Design B proper, and it is the whole of what the
/// collector consults. ADR 0028 decision 1 requires that "a slot the layout
/// calls scalar must never be reachable by a walk that treats it as a
/// reference"; here that is not a discipline anybody keeps but the only thing
/// the walk can read, because the words themselves say nothing.
///
/// # Where a bit comes from
///
/// Two places, and between them they cover every word that can exist:
///
/// - **A frame word's bit is static.** A call writes the callee's whole range
///   in one masked pass from [`FrameMap`], which is derived from
///   `cove_ir::Function`'s two frame sizes. Nothing per-slot happens.
/// - **An operand word's bit is written by the instruction that pushed it.**
///   `load` and `dup` copy the bit of the word they read, `make-struct` and
///   `set-field` push a reference, the scalar core pushes scalars, and
///   `get-field-at` asks the *object's* reference map — which is the one place
///   a bit is decided by metadata that is neither the frame's nor the
///   instruction's, and is decision 2's reference map doing its job.
///
/// **A pop writes no bit.** The word above the top is stale and is never read,
/// because the walk stops at `words.len()` and every push writes its own bit
/// before that word is inside the walk. That is the asymmetry that makes the
/// bitmap cheap: it costs a masked store per push and nothing per pop.
///
/// # Packed, and why
///
/// Sixty-four words to a limb. A `Vec<bool>` would make a push a plain byte
/// store instead of a read-modify-write, and would make the walk read one byte
/// per word instead of skipping sixty-four at a time on a zero limb. Which of
/// those two matters is a measurement question and
/// `docs/VM_ARCHITECTURE.md`'s "What a rooted frame costs to walk" is the
/// answer; the packed form is the one #162 names.
#[derive(Debug, Default)]
struct Bitmap {
    limbs: Vec<u64>,
}

impl Bitmap {
    /// A bitmap with room for `words` words. Reached only by the tests that
    /// exercise the masks directly; a `FrameVm` reserves by limb, because what
    /// [`INITIAL_LIMBS`] is choosing is a size class rather than a capacity.
    #[cfg(test)]
    fn with_capacity(words: usize) -> Bitmap {
        Bitmap::with_limbs(words.div_ceil(64))
    }

    /// A bitmap of `limbs` limbs, reserved up front for [`INITIAL_LIMBS`]'s
    /// reason.
    fn with_limbs(limbs: usize) -> Bitmap {
        Bitmap {
            limbs: vec![0; limbs],
        }
    }

    /// Says whether the word at `at` is a reference.
    ///
    /// One bounds check, which is why it is a `get_mut` and a `match` rather
    /// than a length test and two indexings: this runs once per push and the
    /// growth arm runs on nothing this backend admits, because
    /// [`INITIAL_WORDS`] reserved past every row's high-water mark.
    #[inline(always)]
    fn write(&mut self, at: usize, is_reference: bool) {
        let bit = 1u64 << (at % 64);
        match self.limbs.get_mut(at / 64) {
            Some(limb) => {
                if is_reference {
                    *limb |= bit;
                } else {
                    *limb &= !bit;
                }
            }
            None => {
                self.limbs.resize(at / 64 + 1, 0);
                self.limbs[at / 64] = if is_reference { bit } else { 0 };
            }
        }
    }

    /// Whether the word at `at` is a reference.
    #[inline(always)]
    fn read(&self, at: usize) -> bool {
        self.limbs[at / 64] >> (at % 64) & 1 == 1
    }

    /// Writes a whole frame's worth in one pass: every word of
    /// `base .. base + width` a scalar, except `references` relative to `base`.
    ///
    /// **One read-modify-write per limb**, which for every frame this backend
    /// opens is one, because a frame narrower than sixty-four words lies inside
    /// a single limb. That is what a packed bitmap is for, and it is why
    /// opening a frame costs O(width / 64) rather than O(width) — a call does
    /// not pay per slot for slots it is about to overwrite anyway.
    ///
    /// The clearing half is load-bearing rather than tidy. A return writes no
    /// bit, so the words a returning frame occupied keep its answers about
    /// them; the next frame at that depth would inherit them if opening did
    /// not say otherwise.
    fn write_frame(&mut self, base: usize, width: usize, references: std::ops::Range<usize>) {
        if width == 0 {
            return;
        }
        let end = base + width;
        if end.div_ceil(64) > self.limbs.len() {
            self.limbs.resize(end.div_ceil(64), 0);
        }
        let refs = base + references.start..base + references.end;
        for index in base / 64..=(end - 1) / 64 {
            let frame = Bitmap::mask(index, base..end);
            let named = Bitmap::mask(index, refs.clone());
            self.limbs[index] = (self.limbs[index] & !frame) | named;
        }
    }

    /// The bits of limb `index` that `range` covers.
    #[inline(always)]
    fn mask(index: usize, range: std::ops::Range<usize>) -> u64 {
        let low = index * 64;
        let high = low + 64;
        if range.start >= high || range.end <= low {
            return 0;
        }
        let from = range.start.max(low) - low;
        let to = range.end.min(high) - low;
        if to == 64 {
            u64::MAX << from
        } else {
            (u64::MAX << from) & !(u64::MAX << to)
        }
    }

    /// Calls `visit` once per set bit in `range`, skipping sixty-four words at
    /// a time wherever a limb is empty.
    ///
    /// The skipping is the whole reason to pack the bits, and it is what makes
    /// the walk cost the *live references* rather than the stack depth: a
    /// frame of scalars with a loop running above it is one zero limb, read
    /// once.
    fn for_each(&self, range: std::ops::Range<usize>, visit: &mut dyn FnMut(usize)) {
        if range.is_empty() {
            return;
        }
        let first = range.start / 64;
        let last = (range.end - 1) / 64;
        for index in first..=last.min(self.limbs.len().saturating_sub(1)) {
            let mut limb = self.limbs[index];
            if index == first && !range.start.is_multiple_of(64) {
                limb &= u64::MAX << (range.start % 64);
            }
            if index == last && !range.end.is_multiple_of(64) {
                limb &= !(u64::MAX << (range.end % 64));
            }
            while limb != 0 {
                let bit = limb.trailing_zeros() as usize;
                visit(index * 64 + bit);
                limb &= limb - 1;
            }
        }
    }
}

// ------------------------------------------------------ what a root is

/// Which words a collection may find roots in.
///
/// [`RootScope::EveryWord`] is the mechanism. The other two are the mutations
/// that say what each half of it is holding up, and they exist for the reason
/// `crate::slot`'s negative tests do: a rooting claim nobody can make fail is
/// not a claim, and both of these fail loudly, in a real run of a real
/// benchmark, with the heap's own use-after-free message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum RootScope {
    /// Every word the bitmap calls a reference, which is what a run does.
    EveryWord,
    /// The standing frames' windows and nothing above them.
    ///
    /// The mutation that drops the **operand** words, and
    /// `a_call_argument_is_a_root_before_the_callee_has_a_frame` is what it
    /// costs: an argument is pushed by the caller and is not in any frame
    /// until the callee's base moves under it, and the call is a safepoint in
    /// between.
    WithoutOperands,
    /// The words above the running frame and nothing inside it.
    ///
    /// The mutation that drops the frame's own **reference slots**, which is
    /// ADR 0028 decision 8's "a handle slot is a root according to the frame
    /// reference map" removed. `a_value_slot_is_a_root_across_the_loop_it_lives_in`
    /// is what it costs.
    WithoutFrameSlots,
}

/// One safepoint's roots: every word of the one stack the bitmap calls a
/// reference, and then the shadow stack.
///
/// The counterpart of `vm::StackRoots`, and the list of what is *not* here is
/// the same kind of list that one carries. A word the bitmap does not name is
/// scalar bits, whatever those bits look like. The object table is not a root:
/// it is what is being collected.
///
/// **Each storage location is yielded once**, which is decision 8's first
/// multiplicity: one bit, one visit, and no attempt to de-duplicate the
/// *handles*. A struct standing in a frame slot and also in an operand word is
/// two locations and one object, and
/// `a_reference_in_a_slot_and_in_an_operand_is_two_locations_and_one_expansion`
/// pins it.
struct FrameRoots<'v> {
    words: &'v [u64],
    refs: &'v Bitmap,
    temps: &'v TempRoots,
    /// Which words the walk reads. `0..words.len()` in a run; narrower in the
    /// two mutations. See [`RootScope`].
    range: std::ops::Range<usize>,
}

impl HandleRoots for FrameRoots<'_> {
    fn walk(&self, visit: &mut dyn FnMut(Handle)) {
        let words = self.words;
        self.refs.for_each(self.range.clone(), &mut |at| {
            visit(Handle::from_slot(words[at]))
        });
        self.temps.walk(visit);
    }
}

// -------------------------------------------------- one frame's layout

/// Where one function's words stand in the frame, and which of them are
/// references.
///
/// **This is the one frame layout ADR 0028 decision 1 asks every physical
/// offset to derive from**, and it is derived in turn from the two frame sizes
/// `cove_ir::Function` carries. The lowering still numbers two spaces — an
/// `Inst::LoadScalar` addresses one and an `Inst::LoadLocal` the other — and
/// this is the map that makes them one region from one base: **the scalar
/// slots first and the value slots behind them**, so scalar slot *i* stands at
/// `base + i` and value slot *j* at `base + values + j`.
///
/// # Why the scalars come first, and what that refuses
///
/// A call does not move its arguments: `base' = top - argc`, so the words the
/// caller pushed *are* the callee's first slots. With the scalars first that
/// works for a scalar parameter always, because a scalar parameter is scalar
/// slot 0. It works for a *value* parameter only where the function has no
/// scalar slots at all, and [`admits`] refuses one that does — by name, and
/// naming what would be needed instead, which is a lowering that numbers one
/// space rather than two.
///
/// The alternative was a map that puts whichever kind the parameters are
/// first. It costs the dispatch loop a second base to keep beside `base`, on
/// every instruction that addresses a word, for a generality no admitted row
/// uses; this way the scalar core's addressing is `base + slot`, which is
/// Phase A's unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FrameMap {
    /// How many words one call needs: both frame sizes, added.
    width: u32,
    /// Where value slot 0 stands, relative to the frame base — which is the
    /// scalar frame size, because the scalars come first.
    values: u32,
    /// How many value slots there are. The frame's reference range is
    /// `values .. values + value_count`, and every word outside it is scalar.
    value_count: u32,
}

impl FrameMap {
    /// The map `function`'s two frame sizes imply.
    fn of(function: &cove_ir::Function) -> FrameMap {
        FrameMap {
            width: function.value_frame_size + function.scalar_frame_size,
            values: function.scalar_frame_size,
            value_count: function.value_frame_size,
        }
    }

    /// The frame's reference range, relative to its base.
    fn references(&self) -> std::ops::Range<usize> {
        self.values as usize..(self.values + self.value_count) as usize
    }
}

// ----------------------------------------- what a struct is, as words

/// What one declared struct looks like as a run of eight-byte words.
///
/// # Where this comes from, and what it costs to come from there
///
/// **`cove_ir` does not carry a struct's per-field slot kind.** An
/// `Inst::MakeStruct` names the type and the field names and nothing else, and
/// `cove_ir::Function` numbers slots without saying what is in one beyond
/// `params` and two counts. So a backend that needs decision 2's reference map
/// — "which of its words are handles, so a collector scans those and not the
/// scalars beside them" — cannot read one off the lowering today.
///
/// What it can do, and what this does, is read it off the *construction*: the
/// `fields.len()` instructions before a `make-struct` are what pushed its
/// words, and each of them says what kind of word it pushed. Two sites for one
/// type that disagree, or a site whose producers this cannot read, leave the
/// type unsettled and [`admits`] refuses every function that builds it, by
/// name.
///
/// That is a prototype economy and it is the largest single thing Phase C
/// owes: a reference map belongs in `cove_ir` beside the frame sizes, derived
/// from the checker's field types once, rather than re-derived from the
/// instruction stream by every backend that wants one.
#[derive(Clone, Debug)]
struct StructWords {
    /// The field names, in declaration order, as the `make-struct` writes
    /// them.
    fields: Vec<String>,
    /// What each field's word is. `Part::Nested` is a handle the collector
    /// follows and everything else is scalar bits it must not.
    parts: Vec<Part>,
}

/// Every struct the program builds, worked out once, indexed by the `ConstId`
/// of its type name exactly as `Vm::shapes` is.
///
/// `None` is either "no such type constant" or "this backend cannot settle
/// what its words are", and the two are the same answer here because both mean
/// no object of it may be built.
fn struct_words(program: &Program) -> Vec<Option<StructWords>> {
    let mut settled: Vec<Option<StructWords>> = vec![None; program.constants.len()];
    let mut refused = vec![false; program.constants.len()];
    for function in &program.functions {
        for (pc, inst) in function.code.iter().enumerate() {
            let Inst::MakeStruct { ty, fields } = *inst else {
                continue;
            };
            let written = const_name(program, fields);
            let fields: Vec<String> = if written.is_empty() {
                Vec::new()
            } else {
                written.split(',').map(str::to_string).collect()
            };
            let Some(kinds) = pushed_kinds(program, function, pc, fields.len()) else {
                refused[ty.0 as usize] = true;
                continue;
            };
            let parts: Vec<Part> = kinds.into_iter().map(Kind::part).collect();
            let found = StructWords { fields, parts };
            match &settled[ty.0 as usize] {
                // One type built two ways that disagree about its words is a
                // type this backend has no single reference map for.
                Some(before) if before.parts != found.parts => refused[ty.0 as usize] = true,
                Some(_) => {}
                None => settled[ty.0 as usize] = Some(found),
            }
        }
    }
    for (at, refused) in refused.into_iter().enumerate() {
        if refused {
            settled[at] = None;
        }
    }
    settled
}

/// What one word means, where something outside the one stack has to be told.
///
/// The bits are not self-describing, so every question of this shape is
/// answered by something that is not the word: the frame map, an object's
/// reference map, or -- here -- the instruction that pushed it. This is the
/// third of those, and it is the smallest: four answers, one per kind of word
/// ADR 0028 decision 1 gives eight bytes to, plus the reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Unit,
    Bool,
    Int,
    Float,
    Reference,
}

impl Kind {
    /// What a heap object's reference map calls a word of this kind.
    ///
    /// A `Unit` is a canonical zero word, which decision 1 permits where the
    /// layout cannot omit it, and the map's question about one is the same
    /// question it asks of an `Int`: not a reference.
    fn part(self) -> Part {
        match self {
            Kind::Unit | Kind::Int => Part::Int,
            Kind::Bool => Part::Bool,
            Kind::Float => Part::Float,
            Kind::Reference => Part::Nested,
        }
    }
}

/// What the `count` instructions before `pc` pushed, one [`Kind`] each, or
/// `None` where one of them is not a single word this backend can name.
///
/// Each of these either pushes one word or replaces the top one, so `count` of
/// them in a row leave `count` operands. Anything else -- a call, an operator,
/// a branch -- is refused rather than guessed at, which is what keeps this a
/// reading of the program rather than an inference about it.
fn pushed_kinds(
    program: &Program,
    function: &cove_ir::Function,
    pc: usize,
    count: usize,
) -> Option<Vec<Kind>> {
    if count > pc {
        return None;
    }
    function.code[pc - count..pc]
        .iter()
        .map(|inst| match inst {
            Inst::Const(id) => match program.constant(*id) {
                Const::Unit => Some(Kind::Unit),
                Const::Int(_) => Some(Kind::Int),
                Const::Bool(_) => Some(Kind::Bool),
                Const::Float(_) => Some(Kind::Float),
                Const::Str(_) | Const::Name(_) | Const::Duration(_) => None,
            },
            Inst::ScalarToValue(Scalar::Int) => Some(Kind::Int),
            Inst::ScalarToValue(Scalar::Bool) => Some(Kind::Bool),
            Inst::LoadLocal(_) | Inst::Dup | Inst::MakeStruct { .. } | Inst::SetField(_) => {
                Some(Kind::Reference)
            }
            _ => None,
        })
        .collect()
}

/// What a `Value` at decision 5's boundary is made of, per argument of a
/// `make-builtin`.
///
/// The same reading as [`pushed_kinds`] and for the same reason: a word is not
/// self-describing, so the only thing that can say what one means is what put
/// it there. A handle does not cross this boundary -- materialising an
/// aggregate is `crate::slot::Machine::materialise`'s job and it is not wired
/// here -- so a reference operand refuses the whole call.
fn boundary_kinds(
    program: &Program,
    function: &cove_ir::Function,
    pc: usize,
    argc: usize,
) -> Option<Vec<Kind>> {
    let kinds = pushed_kinds(program, function, pc, argc)?;
    kinds
        .iter()
        .all(|kind| *kind != Kind::Reference)
        .then_some(kinds)
}

/// Which word of a struct a `set-field` names, indexed by the `ConstId` of the
/// field name.
///
/// A name is interned once per string, so one entry per constant is a complete
/// table and reading it costs one indexed load rather than the walk over field
/// names `Vm::SetField` does per execution. Where two admitted structs put the
/// same field name at different positions the entry is `None` and [`admits`]
/// refuses the function that writes it — a `set-field` whose target type is
/// not known statically is exactly the thing a per-pc type map would settle,
/// and Phase C owes that map.
fn field_positions(program: &Program, structs: &[Option<StructWords>]) -> Vec<Option<u32>> {
    let mut positions: Vec<Option<u32>> = vec![None; program.constants.len()];
    let mut ambiguous = vec![false; program.constants.len()];
    for function in &program.functions {
        for inst in &function.code {
            let Inst::SetField(id) = *inst else {
                continue;
            };
            let wanted = const_name(program, id);
            for shape in structs.iter().flatten() {
                let Some(at) = shape.fields.iter().position(|field| field == wanted) else {
                    continue;
                };
                match positions[id.0 as usize] {
                    Some(before) if before != at as u32 => ambiguous[id.0 as usize] = true,
                    Some(_) => {}
                    None => positions[id.0 as usize] = Some(at as u32),
                }
            }
        }
    }
    for (at, ambiguous) in ambiguous.into_iter().enumerate() {
        if ambiguous {
            positions[at] = None;
        }
    }
    positions
}

/// The word an admitted constant is, which is the whole of what
/// `Inst::Const` does in this backend.
///
/// A constant this backend admits is one of the four kinds ADR 0028 decision 1
/// gives a word to, so it *is* a word and does not have to become a `Value` to
/// be pushed. That is the change Phase A did not make: there, `const`
/// materialised, and the only loop it fed was the epilogue. Here the same
/// instruction feeds `make-struct`.
///
/// Zero for the three kinds that have no eight-byte form, every one of which
/// [`admits`] refuses: the table is built over every constant of the program
/// and a refused one is never read.
fn const_word(constant: &Const) -> u64 {
    match constant {
        Const::Unit => 0,
        Const::Bool(value) => Word::of_bool(*value),
        Const::Int(value) => Word::of_int(*value),
        Const::Float(value) => Word::of_float(*value),
        Const::Str(_) | Const::Name(_) | Const::Duration(_) => 0,
    }
}

/// The `Value` a word stands for at decision 5's boundary, read as [`Kind`]
/// says to and never out of the bits.
fn crossed(kind: Kind, word: u64) -> Value {
    match kind {
        Kind::Unit => Value::unit(),
        Kind::Bool => Value(Repr::Bool(Word::canonical_bool(word))),
        Kind::Int => as_value_of(Scalar::Int, Word::int(word)),
        Kind::Float => Value::float(Word::float(word)),
        Kind::Reference => {
            unreachable!("`admits` refuses a boundary crossing that carries a reference")
        }
    }
}

fn describe(inst: &Inst) -> &'static str {
    match inst {
        Inst::Unary(_) | Inst::Binary(_) => "an operator over a general value",
        Inst::JumpIfFalse(_) | Inst::JumpIfTrue(_) => "a branch on a general value",
        Inst::MakeClosure { .. } | Inst::CallValue { .. } => "a closure",
        Inst::MakeDyn { .. } | Inst::CallDyn { .. } => "`dyn` dispatch",
        Inst::CallHost { .. } | Inst::CallResource { .. } => "a Host call",
        Inst::CallBuiltin { .. } | Inst::CallBuiltinAssoc { .. } => "a builtin method",
        Inst::MakeArray(_) | Inst::MakeRange { .. } | Inst::IterItems | Inst::SpreadArgument => {
            "a collection"
        }
        Inst::Concat(_) => "string interpolation",
        Inst::GetField(_) => "a struct field read by name",
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
        | Inst::ValueToScalar
        | Inst::LoadLocal(_)
        | Inst::StoreLocal(_)
        | Inst::Dup
        | Inst::MakeStruct { .. }
        | Inst::SetField(_)
        | Inst::GetFieldAt(_)
        | Inst::GetFieldAtScalar(_)
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
    /// One bit per word of the one stack: whether the word is a reference.
    ///
    /// The whole of what a collection consults, and the only thing that can
    /// say what a word is. See [`Bitmap`].
    refs: Bitmap,
    /// The VM-owned traced object heap, which is `crate::slot`'s and is wired
    /// here rather than reimplemented.
    heap: HandleHeap,
    /// One layout per struct the program builds, indexed by the `ConstId` of
    /// its type name, and `None` for a type this backend refused.
    shapes: Vec<Option<LayoutId>>,
    /// Which word a `set-field` names, indexed by the `ConstId` of the field
    /// name. See [`field_positions`].
    field_at: Vec<Option<u32>>,
    /// One frame map per function of the program: the one layout every
    /// physical offset in a run derives from. See [`FrameMap`].
    maps: Vec<FrameMap>,
    /// The shadow-root stack, empty for the whole of an admitted run.
    ///
    /// **That emptiness is a finding and not an omission.** ADR 0028 decision
    /// 8's third mechanism -- "the dispatch discipline guarantees that a
    /// collection can occur only when every live handle has been returned to a
    /// mapped VM slot" -- is false for `Vm` at the five places
    /// `crate::slot`'s module documentation names, and is true here by
    /// construction, because a one-stack backend has nowhere else to put an
    /// operand. It is kept because the moment an aggregate crosses decision
    /// 5's boundary the discipline stops being free, and because a mechanism
    /// that is present and empty is checkable where an absent one is not:
    /// `nothing_is_rooted_outside_the_one_stack` reads it.
    temps: TempRoots,
    /// Which words a collection may find roots in, which is
    /// [`RootScope::EveryWord`] outside the two mutation tests.
    scope: RootScope,
    /// Whether a collection is due, kept here rather than asked of the heap at
    /// every safepoint.
    ///
    /// **This is a measurement fix and it is the same one twice.**
    /// `benches/pure` takes a safepoint at every call and at every return and
    /// allocates nothing at all, so asking the heap would be two loads from a
    /// structure the row never otherwise touches — a cold line, twice a call,
    /// for an answer that is always no. A row that allocates asks the heap
    /// once per allocation instead, where the structure is warm because it
    /// just wrote to it.
    ///
    /// So the pacing decision lives in the heap, as it should, and *this* is
    /// one hot bool beside `fuel`.
    due: bool,
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
    /// The word every constant of the program is, worked out once.
    ///
    /// `Vm::constants` is a `Vec<Value>` for the same reason at the same
    /// point, and the difference is the whole of Phase B at the boundary: a
    /// constant this backend admits *is* eight bytes, so nothing is
    /// materialised to push one.
    constants: Vec<u64>,
    /// How many `Value`s this run built or consumed at decision 5's boundary.
    ///
    /// The measurement issue #212 asks for, kept as a counter rather than as a
    /// claim. Eight for `benches/arith`, `benches/call`, `benches/pure`,
    /// `benches/field` and `benches/method` -- every one of them in the nine
    /// instructions after the loop, and none of them inside it, whatever the
    /// loop does with references.
    materialized: u64,
    /// What the traced heap did, in the shape `crate::heap` reports.
    heap_stats: HeapStats,
    /// The three multiplicities ADR 0028 decision 8 distinguishes, summed over
    /// the run's collections: root storage locations yielded, and objects
    /// expanded by the mark phase.
    roots_yielded: u64,
    expansions: u64,
    /// Objects the mark phase found live, summed over the same collections.
    /// Equal to `expansions` whatever the shape of the graph, which is what
    /// decision 8's third multiplicity says.
    marked: u64,
    /// The most root storage locations any one collection yielded, and the
    /// most objects any one expanded.
    ///
    /// A sum cannot tell "two locations, one object" from "one location twice
    /// over two collections", and that distinction is the whole of decision
    /// 8's first multiplicity. These two can.
    most_roots_at_once: u64,
    most_expansions_at_once: u64,
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
        let mut heap = HandleHeap::new();
        let settled = struct_words(program);
        let field_at = field_positions(program, &settled);
        let shapes = settled
            .iter()
            .enumerate()
            .map(|(at, shape)| {
                let shape = shape.as_ref()?;
                let refs = shape
                    .parts
                    .iter()
                    .enumerate()
                    .filter(|(_, part)| **part == Part::Nested)
                    .map(|(at, _)| at)
                    .collect();
                Some(heap.register(Layout::new(
                    const_name(program, ConstId(at as u32)),
                    shape.parts.len(),
                    refs,
                )))
            })
            .collect();
        FrameVm {
            runtime,
            program,
            words: Vec::with_capacity(INITIAL_WORDS),
            refs: Bitmap::with_limbs(INITIAL_LIMBS),
            heap,
            shapes,
            field_at,
            maps: program.functions.iter().map(FrameMap::of).collect(),
            temps: TempRoots::new(),
            scope: RootScope::EveryWord,
            due: false,
            frames: Vec::with_capacity(MAX_CALL_DEPTH),
            boundary: Vec::new(),
            constants: program.constants.iter().map(const_word).collect(),
            materialized: 0,
            heap_stats: HeapStats::default(),
            roots_yielded: 0,
            expansions: 0,
            marked: 0,
            most_roots_at_once: 0,
            most_expansions_at_once: 0,
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

    /// Collects at every safepoint, whatever the heap's pacing says.
    ///
    /// `crate::slot::HandleHeap::stress`'s argument, at the scale of a whole
    /// program: which safepoint a collection lands on is otherwise an accident
    /// of what the program allocated before it, and a rooting test that
    /// depends on that accident is a test that passes by luck.
    #[cfg(test)]
    fn stress(&mut self) {
        self.heap.stress(true);
        self.due = true;
    }

    /// How many collections this run ran, and what they found.
    #[cfg(test)]
    fn collections(&self) -> (u64, u64, u64) {
        (
            self.heap_stats.collections,
            self.roots_yielded,
            self.expansions,
        )
    }

    /// How many handles stand outside the one stack, which is none.
    #[cfg(test)]
    fn rooted_outside_the_stack(&self) -> usize {
        self.temps.depth()
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

    /// What this run allocated, which is now this backend's own traced heap
    /// rather than the runtime's counted one.
    ///
    /// The two never overlap: nothing an admitted program builds is a `Value`
    /// the runtime's heap could hold, so adding them would be adding a zero.
    pub fn heap_stats(&self) -> HeapStats {
        self.heap_stats
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
        let _ = self.open(function, 0);
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
        // The second of the two numberings the lowering still keeps, read
        // through the one frame map into one region from one base. A local
        // rather than a field for the reason `base` and `pc` are: every
        // instruction that addresses a value slot reads it. The *first*
        // numbering needs no local at all, because the scalars stand at the
        // base — see `FrameMap`.
        let mut values = base + self.maps[standing.function.0 as usize].values as usize;
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
                Inst::ScalarConst(value) => self.push_scalar(Word::of_int(value)),
                Inst::LoadScalar(slot) => {
                    let word = self.words[base + slot as usize];
                    self.push_scalar(word);
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
                    self.push_scalar(Word::of_int(answer));
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
                    value_argc,
                    ..
                } => {
                    let span = running.span_at(pc);
                    let callee = program.function(target);
                    self.enter(callee, span)?;
                    self.charge(callee.block_fuel[0], || callee.span_at(0))?;
                    // The arguments the caller pushed *are* the callee's
                    // first words. Nothing is transferred; the base moves.
                    // One of the two counts is zero, because `admits` refuses
                    // a call that passes both kinds -- see `FrameMap`.
                    let argc = (scalar_argc + value_argc) as usize;
                    let callee_base = self.words.len() - argc;
                    values = self.open(target, callee_base);
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
                            self.push_scalar(answer);
                            base = caller.base as usize;
                            values = base + self.maps[caller.function.0 as usize].values as usize;
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

                // ----------------------------------------- the reference core
                Inst::LoadLocal(slot) => {
                    let word = self.words[values + slot as usize];
                    self.push_reference(word);
                }
                Inst::StoreLocal(slot) => {
                    let word = self.pop_word();
                    self.words[values + slot as usize] = word;
                }
                Inst::Dup => {
                    let at = self.words.len() - 1;
                    let word = self.words[at];
                    let is_reference = self.refs.read(at);
                    self.push_word(word, is_reference);
                }
                // **Both of these are nothing at all here**, and that is the
                // whole of ADR 0027's per-read crossing removed rather than
                // narrowed: a word the checker settled as an `Int` is the same
                // eight bytes on either side of the conversion, so there is
                // nothing to convert. The instructions still execute and are
                // still counted, which is what keeps the two backends'
                // instruction counts equal.
                Inst::ScalarToValue(_) | Inst::ValueToScalar => {
                    debug_assert!(
                        !self.refs.read(self.words.len() - 1),
                        "a conversion between a scalar and a value was handed a reference"
                    );
                }
                Inst::MakeStruct { ty, .. } => {
                    let layout = self.shapes[ty.0 as usize]
                        .expect("`admits` settled every struct this backend builds");
                    let width = self.heap.layout(layout).words();
                    let at = self.words.len() - width;
                    debug_assert!(
                        (at..self.words.len()).all(|word| self.refs.read(word)
                            == self.heap.layout(layout).is_reference(word - at)),
                        "a field word disagrees with the layout's reference map"
                    );
                    // The field words are operands until the object exists, so
                    // they are roots until the object exists; the truncation
                    // is after the allocation and not before it.
                    let handle = self.heap.allocate_from(layout, &self.words[at..]);
                    self.words.truncate(at);
                    self.allocated(width);
                    self.push_reference(handle.to_slot());
                    // `Vm::MakeStruct` charges the width beside the
                    // instruction, so this does too: same schedule, same fuel.
                    self.fuel += width as u64;
                }
                // One instruction in this backend, two in the lowering, and
                // the difference between them is which stack the answer went
                // on. There is one stack.
                Inst::GetFieldAt(index) | Inst::GetFieldAtScalar(index) => {
                    let source = Handle::from_slot(self.pop_word());
                    let at = index as usize;
                    let word = self.heap.word(source, at);
                    // The object's reference map, and nothing else, says what
                    // kind of word this is -- ADR 0028 decision 2, asked one
                    // word at a time because a frame has to be told what it
                    // just received.
                    let is_reference = self.heap.word_is_reference(source, at);
                    self.push_word(word, is_reference);
                }
                Inst::SetField(field) => {
                    let at = self.field_at[field.0 as usize]
                        .expect("`admits` settled every field this backend writes")
                        as usize;
                    let word = self.pop_word();
                    let target = Handle::from_slot(self.pop_word());
                    let handle = self.heap.copy_replacing(target, at, word);
                    let width = self.heap.layout_words(handle);
                    self.allocated(width);
                    self.push_reference(handle.to_slot());
                    self.fuel += width as u64;
                }

                // --------------------------------------------- the boundary
                Inst::Const(id) => self.push_scalar(self.constants[id.0 as usize]),
                Inst::Pop => {
                    self.materialized += 1;
                    self.pop_value();
                }
                Inst::MakeBuiltin { name: which, argc } => {
                    let span = running.span_at(pc);
                    let which = const_name(program, which);
                    let kinds = boundary_kinds(program, running, pc, argc as usize)
                        .expect("`admits` settled every builtin call this backend runs");
                    let at = self.words.len() - argc as usize;
                    let mut arguments: Vec<Value> = kinds
                        .iter()
                        .enumerate()
                        .map(|(offset, kind)| {
                            self.materialized += 1;
                            crossed(*kind, self.words[at + offset])
                        })
                        .collect();
                    self.words.truncate(at);
                    self.materialized += 1;
                    let answer =
                        self.make_builtin(which, &mut arguments, running.arg_spans_at(pc), span);
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
                                    let caller =
                                        self.frames.last().expect("a caller stands").function;
                                    values = base + self.maps[caller.0 as usize].values as usize;
                                    running = program.function(caller);
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
                            let caller = self.frames.last().expect("a caller stands").function;
                            values = base + self.maps[caller.0 as usize].values as usize;
                            running = program.function(caller);
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

    /// Opens `function`'s frame at `base`: the words it needs, and the bits
    /// that say which of them are references.
    ///
    /// **This is what rooting costs a call, beside one indexed load of the
    /// map.** The words are one `Vec::resize`, exactly as Phase A's were -- a zero word is a canonical
    /// `Unit`, a `false` and a `0`, and it is *also* never a live handle,
    /// because `crate::slot` never issues generation zero. So a frame with
    /// reference slots is opened by the same instruction as one without, and
    /// what is added is the bitmap's masked pass over `width / 64` limbs.
    fn open(&mut self, function: FunctionId, base: usize) -> usize {
        let map = self.maps[function.0 as usize];
        let width = map.width as usize;
        self.words.resize(base + width, 0);
        self.refs.write_frame(base, width, map.references());
        base + map.values as usize
    }

    /// Pushes a word the layout calls scalar.
    #[inline(always)]
    fn push_scalar(&mut self, word: u64) {
        let at = self.words.len();
        self.words.push(word);
        self.refs.write(at, false);
    }

    /// Pushes a word the layout calls a reference.
    #[inline(always)]
    fn push_reference(&mut self, word: u64) {
        let at = self.words.len();
        self.words.push(word);
        self.refs.write(at, true);
    }

    /// Pushes a word whose kind is decided by something the caller read --
    /// another word's bit, or an object's reference map.
    #[inline(always)]
    fn push_word(&mut self, word: u64, is_reference: bool) {
        let at = self.words.len();
        self.words.push(word);
        self.refs.write(at, is_reference);
    }

    /// Records one object of `width` words, for the heap figures a run
    /// reports.
    #[inline(always)]
    fn allocated(&mut self, width: usize) {
        self.heap_stats.allocated_objects += 1;
        self.heap_stats.allocated_bytes += (width * std::mem::size_of::<u64>()) as u64;
        // Asked here rather than at the safepoint. Allocating is the only
        // thing that can make a collection due, and this is the one moment the
        // heap is certainly warm.
        self.due |= self.heap.should_collect();
    }

    /// The running frame's window, which is what the two mutations of
    /// [`RootScope`] cut the walk down to.
    fn window(&self) -> std::ops::Range<usize> {
        let standing = *self
            .frames
            .last()
            .expect("a frame stands at every safepoint");
        let base = standing.base as usize;
        let width = self.maps[standing.function.0 as usize].width as usize;
        base..(base + width).min(self.words.len())
    }

    /// Collects if the heap says one is due, from every word the bitmap calls
    /// a reference.
    ///
    /// `Vm::collect_if_due` at the same point on the same schedule, and the
    /// difference is what it reads: `Vm` walks its whole value stack because
    /// every word of one is a `Value`, and this walks a bitmap because most
    /// words are not references and the bitmap is what says which are.
    fn collect_if_due(&mut self) {
        if !self.due {
            return;
        }
        let range = match self.scope {
            RootScope::EveryWord => 0..self.words.len(),
            RootScope::WithoutOperands => 0..self.window().end,
            RootScope::WithoutFrameSlots => self.window().end..self.words.len(),
        };
        let began = std::time::Instant::now();
        let Self {
            heap,
            words,
            refs,
            temps,
            ..
        } = self;
        let roots = FrameRoots {
            words: words.as_slice(),
            refs,
            temps,
            range,
        };
        let collected = heap.collect(&roots);
        self.heap_stats.collections += 1;
        self.heap_stats.freed_objects += collected.freed_objects;
        self.heap_stats.live_objects = collected.live_objects;
        self.heap_stats.live_bytes = collected.live_bytes;
        self.heap_stats.peak_bytes = self.heap_stats.peak_bytes.max(collected.live_bytes);
        self.heap_stats.pause += began.elapsed();
        self.roots_yielded += collected.roots_yielded;
        self.expansions += collected.expansions;
        self.marked += collected.live_objects;
        self.most_roots_at_once = self.most_roots_at_once.max(collected.roots_yielded);
        self.most_expansions_at_once = self.most_expansions_at_once.max(collected.expansions);
        self.due = self.heap.should_collect();
    }

    /// The top of the one stack.
    ///
    /// `cove_ir::lower::validate` simulated the depth of every instruction
    /// control can reach before this backend was handed the program, so an
    /// empty stack here is a broken invariant rather than a program that
    /// could be told about it. It is `Vm::pop_scalar`'s argument word for
    /// word.
    ///
    /// **No bit is written.** The word above the top is stale and is never
    /// read, because the walk stops at `words.len()` and every push writes its
    /// own bit before the word it pushed is inside the walk. That asymmetry is
    /// what makes the bitmap cost a masked store per push and nothing per pop.
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
    /// **A collection**, on the schedule and at the points `Vm::safepoint`
    /// runs one, over this backend's own traced heap. Phase A said "no
    /// collection, because this backend owns no heap"; Phase B is that
    /// sentence stopping being true.
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
        self.collect_if_due();
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
        self.words.clear();
        self.frames.clear();
        self.boundary.clear();
        self.fuel = 0;
        self.words.extend_from_slice(arguments);
        let _ = self.open(function, 0);
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
