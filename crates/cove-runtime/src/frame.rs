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
//! # Three phases, and which one this is
//!
//! **Phase A** ran `benches/arith`, `benches/call` and `benches/pure` over one
//! contiguous `Vec<u64>`, and no word of it was ever a reference:
//! [`admits`] refused any function with a nonzero `value_frame_size`, which is
//! what made its "no `Value` in the hot path" claim structural rather than
//! careful. It priced a call and a return at 14.4 ns against the VM's 38.3 —
//! *in its own build*, which is the only kind of comparison ADR 0029 allows —
//! and said nothing at all about what a *rooted* frame costs.
//!
//! **Phase B** was a word-wide slot stack with a GC bitmap, which is
//! [issue #162](https://github.com/myuon/cove/issues/162)'s Design B proper. A
//! frame word may be a reference into a VM-owned traced object heap, and
//! `benches/field` and `benches/method` run on it. What that adds, and what it
//! costs, is under "The bitmap" below and in `docs/VM_ARCHITECTURE.md` under
//! "What a rooted frame costs to walk".
//!
//! **Phase C** is this, and it is not a change to the arrangement. It is what
//! Phase B named as its largest debt: `cove_ir` carried no per-field slot
//! kind, so decision 2's reference map for a struct was read off the
//! *construction* — the instructions that pushed one instance's words — and a
//! type built two ways that disagreed was refused by name. `cove_ir::StructType`
//! now carries one `SlotKind` per field, settled from the checker's answer
//! about the declared type by the rule that settles every other slot's, and
//! this backend reads the map off the lowered type. **A type is a static fact
//! and it is now read as one.** Two consequences follow, and they are the whole
//! of what changed here: the by-name refusal is gone because it has become
//! impossible to state, and the bitmap's third authority — an operand a field
//! read pushed — is decided by the instruction rather than by the object.
//!
//! What Phase C did **not** close is the frame map, and the reason is that
//! Phase B's "the same absence is why" was a diagnosis of one cause for two
//! symptoms that turn out to be two. See `FrameMap`.
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
//! having been given it. Phase C looked at moving that into `cove_ir` and did
//! not: see `FrameMap` for what it would take and why the per-field kind did
//! not bring it.
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
//! | an operand pushed by a field read | the **lowered type** the instruction names: `cove_ir::StructType`'s per-field `SlotKind` |
//!
//! **The third was the one that could not be static, and Phase C is what made
//! it so.** Phase B asked `crate::slot::HandleHeap::word_is_reference` — the
//! object's own layout, read per execution — because `get-field-at` is one
//! instruction whose answer is a handle for a struct field and scalar bits for
//! an `Int` one, and nothing in the IR said which. `Inst::GetFieldAt` now
//! carries the `cove_ir::StructId` of the type the checker settled for the
//! receiver, and a type's field kinds are the same on every execution of one
//! instruction, so the bit is a table lookup decided before the run. The
//! object is still asked under `debug_assert`, which is the two answers being
//! compared rather than one of them being trusted.
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
//! kinds, which that ADR puts outside this heap on purpose. What Phase B added
//! to the admitted subset is the struct.
//!
//! **What Phase C adds is one shape and it is the shape the static map made
//! readable**: a struct-typed field read whose answer is then stored, passed or
//! built with. `Inst::GetFieldAt` was unreadable to `pushed_kinds` while only
//! the object knew what it pushed, so `var inner = outer.inner` was refused;
//! now the instruction names the type and the read is a reference the frame can
//! account for. `a_nested_struct_read_into_a_slot_is_rooted` is the coverage,
//! and it is the reason the widening is taken — ADR 0029's rule read as a rule
//! about admitting: a shape no test runs is a shape nobody knows runs.

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
    let structs = struct_parts(program);
    let fields = field_positions(program);
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
    structs: &[Vec<Part>],
    fields: &[Option<u32>],
) -> Result<Vec<FunctionId>, Refused> {
    let function = program.function(id);
    // Built where a refusal is built and not before it. `admits` runs once
    // per run, so this is not a hot path — but it is a `format!` per function
    // of the program on the way into every run that is *not* refused, and a
    // refusal is the only reader it has.
    let named = || format!("`{}.{}`", function.module, function.name);
    let mut calls = Vec::new();
    // Once for the function, and every question below is a lookup in it. See
    // [`Operands`]: what this replaced looked at the instructions immediately
    // before the one asking, which is a different thing and a smaller one.
    let operands = simulate(program, function);
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
            | Inst::GetFieldAtScalar(_)
            | Inst::ScalarToValue(_) => {}
            // A field read is a word whose kind is the *type's*, and the
            // instruction names the type. `validate` has already checked that
            // the position is inside it, so there is nothing left to refuse.
            Inst::GetFieldAt { .. } => {}
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
                if operands.top(pc, 1).as_deref() != Some(&[Kind::Reference]) {
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
            // **The type says what its words are, and this asks whether the
            // site pushed them.** The reference map is
            // `structs[of]`, derived from `cove_ir::StructType`'s per-field
            // slot kinds, so two sites for one type cannot disagree about it
            // and there is no type-wide refusal left to make. What is still a
            // question is the same one `Inst::StoreLocal` asks: the map will
            // call some of these words references, so the instructions that
            // pushed them have to say they are.
            //
            // ADR 0027 is why that question survives a static map. A
            // declaration reached through a value is lowered "with every
            // argument on the value stack", so an `Int` field's argument can
            // arrive as a word the frame calls a reference; and a `Float`
            // field is a value slot because `cove_ir::Scalar` is `Int | Bool`,
            // while a `Float` constant is scalar bits. Both are refusals about
            // this *site*, with its span, rather than about the type.
            Inst::MakeStruct(of) => {
                let declared = &structs[of.0 as usize];
                let pushed = operands.top(pc, declared.len());
                let agrees = pushed.as_ref().is_some_and(|kinds| {
                    kinds
                        .iter()
                        .map(|kind| kind.part())
                        .eq(declared.iter().copied())
                });
                if !agrees {
                    return Err(Refused::new(
                        format!(
                            "building `{}` in {} out of words the 8-byte frame cannot show are \
                             what the type's fields are",
                            program.struct_type(*of).name,
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
                if operands.boundary(pc, *argc as usize).is_none() {
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
                // A value argument becomes a value slot of the callee -- at
                // the slot the callee's numbering gives it, which is where it
                // stands already or where `FrameVm::permute` puts it -- so it
                // is the same question `Inst::StoreLocal` asks: the frame map
                // will call that word a reference, so the instruction that
                // pushed it has to say it is one.
                //
                // The `value_argc` value operands are the value arguments in
                // declaration order whether or not a scalar argument stands
                // between two of them, because the simulation is over the
                // value operand stack and a scalar argument is not on it. A
                // call that passes both kinds is admitted for the same reason
                // a mixed *frame* is: moving a word is something the frame
                // knows how to do now. See [`FrameMap`].
                if *value_argc != 0
                    && !operands
                        .top(pc, *value_argc as usize)
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
    // Two refusals stood here and are gone: a function taking both a value
    // and a scalar parameter, and a function taking a value parameter while
    // keeping a scalar slot. Both said the same thing -- an argument arrives
    // in declaration order and the numbering groups slots by region, so this
    // one's arguments do not arrive at the slots they name -- and neither is
    // a shape the frame cannot hold. `FrameVm::permute` moves them, and
    // `cove_ir::Function::param_slot` says where to. See [`FrameMap`].
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
///   `get-field-at` reads the lowered type it names. That last one was the
///   exception until Phase C: it asked the *object's* reference map, per
///   execution, because nothing in the IR said what a field held. Decision 2's
///   reference map is still what decides it — the map is just written down in
///   `cove_ir::StructType` before the run rather than reconstructed during it.
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

/// Where the bit a `get-field-at` writes comes from.
///
/// [`FieldMap::TheLoweredType`] is the mechanism and is what a run uses:
/// `Inst::GetFieldAt` names a `cove_ir::StructType`, and the field's
/// `SlotKind` says whether the word it just pushed is a handle. This is Phase
/// C's whole change to the bitmap — Phase B asked the *object* the same
/// question, per execution, because nothing in the IR answered it.
///
/// [`FieldMap::Dropped`] is the mutation, and it exists for the reason the
/// other two do: a claim nobody can make fail is not a claim.
/// `a_field_reads_bit_comes_from_the_lowered_type` is what it costs, and it
/// costs the heap's own use-after-free message on a real program.
///
/// It is read **only** inside a `debug_assert`, so it is nothing at all in a
/// release build and the hot path is one indexed load either way. The mutation
/// itself is applied to [`FrameVm::field_refs`], and this is what stops the
/// assertion from catching it before the collector does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum FieldMap {
    /// The `SlotKind` of the field, off the type the instruction names.
    TheLoweredType,
    /// Every field read says scalar, whatever the type says.
    Dropped,
}

/// Where the bits of a *permuted* frame's argument words come from.
///
/// [`ArgumentBits::TheFrameMap`] is the mechanism and is what a run uses:
/// [`FrameVm::open`] writes every bit of the frame from [`FrameMap`], after
/// the permutation, so each bit is about the slot its word ended up in.
///
/// [`ArgumentBits::ThePushesThatWroteThem`] is the mutation, and it is the
/// mistake this backend was making until an argument could move: it leaves
/// the argument words' bits exactly as the pushes wrote them, which is *right*
/// for every frame whose arguments arrive in their slots and wrong for every
/// frame whose arguments do not. A handle moved into the value region then
/// carries the bit of the scalar it displaced, so the walk steps over it, and
/// `an_arguments_bit_moves_with_the_word` is what that costs. It is issue
/// #192's `arg_vectors` failure in a new place: a root that moved, yielded
/// from where it was rather than from where it is.
///
/// Read only where a frame is permuted at all, so the frames that move
/// nothing are one comparison away from it and no run outside the mutation
/// test is ever the other arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
enum ArgumentBits {
    /// Every bit of an opened frame, the arguments included, says what the
    /// numbering says about the slot the word stands in.
    TheFrameMap,
    /// The argument words keep the bits their pushes wrote.
    ThePushesThatWroteThem,
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
/// offset to derive from**, and since Phase D it is not a second layout at
/// all: it is `cove_ir::Function`'s own numbering, read. The lowering numbers
/// one space — the scalar region, then the value region, then the place
/// region, from one origin — so **a slot's number is its offset from this
/// frame's base**, for an `Inst::LoadScalar` and an `Inst::LoadLocal` alike.
/// There is nothing here to translate, and no second base to keep beside
/// `base` on every instruction that addresses a word.
///
/// What is left is the part a *number* cannot carry: how wide a frame is, and
/// which run of it a collection follows. Both come off
/// `cove_ir::Function::slot_count` and `cove_ir::Function::value_origin`, so
/// this struct is those answers held where a per-call [`FrameVm::open`] can
/// use them without asking again.
///
/// # Why the scalars come first, and what an argument does about it
///
/// A call does not move its arguments *as a rule*: `base' = top - argc`, so
/// the words the caller pushed stand at the callee's first slots. With the
/// scalars first that is already the right place for a scalar parameter,
/// because a scalar parameter is slot 0; it is the right place for a *value*
/// parameter only where nothing scalar is numbered before it.
///
/// Phase D moved that order out of this file and into the lowering, where it
/// is the one numbering's order rather than one backend's convention, and
/// found that the order was not what refused a mixed function. **The
/// arguments arrive in declaration order and the numbering groups slots by
/// region, and those are two orders.** Where they differ the frame's own
/// numbering says where each argument belongs —
/// `cove_ir::Function::param_slot` — and [`FrameVm::permute`] moves it there
/// as the frame opens. `arrivals` is which of the two cases a function is,
/// decided once when the map is built.
///
/// **Which is Phase E's decision, and it is a decision about a cost.** The
/// alternative is a convention that states each argument's slot so the caller
/// pushes it there — nothing to move, ever. That is not free, it is
/// unreachable: a value parameter's slot is `value_origin` plus its rank, and
/// `value_origin` is `scalar_frame_size`, a width of the callee's whole body.
/// `cove_ir::Function::params` says so where it says why it is written out
/// rather than derived — "a caller has to place its arguments before the
/// callee exists: a recursive call is lowered before its own `Function`
/// does". A numbering in which a parameter's slot is not a width — the
/// parameters first, in arrival order, and the regions after them — would
/// reach it, and would cost a *physically split* realisation an indexed load
/// per slot access, because its regions would no longer be runs. ADR 0028
/// decision 1 permits such a realisation and requires only that every
/// physical offset derive from the one layout; this backend is one array and
/// would not pay, and the production `Vm` is three stacks and would. So the
/// cost is paid where it can be measured and where it is smallest: per call,
/// on the frames that need it, and nothing at all on the frames that do not.
///
/// # Why Phase C's per-field kind did not close this, although Phase B said it
/// would
///
/// Phase B wrote that "the same absence is why the frame map is derived at run
/// time from two frame sizes instead of being lowered as one numbering". One
/// absence, two symptoms — and having removed the absence, they were two.
///
/// A struct's reference map was genuinely missing from the IR: nothing in
/// `cove_ir` said what a field held, so a backend had to invent an answer, and
/// two inventions could disagree. `cove_ir::StructType` supplies it and the
/// invention is gone. A frame's reference map was **not** missing; it was
/// `value_frame_size` and `scalar_frame_size`, which say precisely which slots
/// are references. What was missing was a *number* that named one slot rather
/// than one slot of one stack, and that is what Phase D added.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FrameMap {
    /// How many words one call needs, which is every slot of the one
    /// numbering.
    width: u32,
    /// Where the value region begins, relative to the frame base — and,
    /// because a slot's number is its offset, the number of the first value
    /// slot as well.
    values: u32,
    /// How many value slots there are. The frame's reference range is
    /// `values .. values + value_count`, and every word outside it is a
    /// scalar or a place.
    value_count: u32,
    /// Whether the words a call pushed already stand at the slots they name.
    ///
    /// Read on every call and answered by the numbering rather than by the
    /// run: see [`Arrivals`].
    arrivals: Arrivals,
}

/// Whether a call's arguments are already in their slots, or have to be moved
/// into them.
///
/// One byte of [`FrameMap`], which is loaded once per call anyway, so the
/// frames that need nothing pay one comparison and no second lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arrivals {
    /// Every argument arrives at the slot number it becomes, which is
    /// `cove_ir::Function::arguments_arrive_in_their_slots`. Opening the
    /// frame moves nothing.
    InTheirSlots,
    /// Declaration order and the numbering's order differ here, so the
    /// arguments are permuted into their slots as the frame opens. See
    /// [`FrameVm::permute`].
    ToBeMoved,
}

impl FrameMap {
    /// The map `function`'s own numbering states.
    fn of(function: &cove_ir::Function) -> FrameMap {
        FrameMap {
            width: function.slot_count(),
            values: function.value_origin(),
            value_count: function.value_frame_size,
            arrivals: match function.arguments_arrive_in_their_slots() {
                true => Arrivals::InTheirSlots,
                false => Arrivals::ToBeMoved,
            },
        }
    }

    /// The frame's reference range, relative to its base.
    fn references(&self) -> std::ops::Range<usize> {
        self.values as usize..(self.values + self.value_count) as usize
    }
}

// ----------------------------------------- what a struct is, as words

/// What one declared struct is as a run of eight-byte words, read off the
/// **type**.
///
/// # Where this comes from
///
/// `cove_ir::StructType` carries one `SlotKind` per field, settled from the
/// checker's answer about the declared field type by the rule that decides a
/// parameter's slot and a local's. So decision 2's reference map — "which of
/// its words are handles, so a collector scans those and not the scalars
/// beside them" — is read off the lowering, one [`Part`] per field, and
/// nothing about a construction is consulted.
///
/// **That is the difference Phase C exists to make.** Phase B derived this map
/// from the `fields.len()` instructions before each `make-struct`, so a type
/// built two ways that disagreed had no single reference map and was refused
/// by name. A fact neither construction states is a fact no two constructions
/// can disagree about, so that refusal is not diagnosed here — it cannot
/// arise. What [`admits`] still asks is the other half, per site: whether the
/// words a given `make-struct` pushed *are* what the type says its fields are.
///
/// # What a slot kind is as a word
///
/// `SlotKind::Value` is a word the collector follows and `SlotKind::Scalar` is
/// one it must not, which is decision 1's invariant stated for an object's
/// interior.
///
/// `SlotKind::Place` is not a refusal but an `unreachable!`, and the
/// difference is deliberate. A refusal is for a program this backend cannot
/// run; a place-kinded *field* is a lowering that cannot exist, because
/// `lower::convention::slot_kind_of` answers only `Scalar` or `Value` and a
/// field's kind is that function's answer and nothing else. It is
/// `Word::canonical_bool`'s treatment for the same reason: a broken invariant
/// of the pipeline rather than a program that could be told about it.
fn struct_parts(program: &Program) -> Vec<Vec<Part>> {
    program
        .structs
        .iter()
        .map(|declared| {
            declared
                .fields
                .iter()
                .map(|field| match field.kind {
                    SlotKind::Value => Part::Nested,
                    SlotKind::Scalar(Scalar::Int) => Part::Int,
                    SlotKind::Scalar(Scalar::Bool) => Part::Bool,
                    SlotKind::Place => unreachable!(
                        "`{}` declares field `{}` as a place, which no lowering emits",
                        declared.name, field.name
                    ),
                })
                .collect()
        })
        .collect()
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

/// What one value operand is, wherever control can be standing at one
/// instruction: a [`Kind`] every path agrees on, or `None`.
///
/// `None` is both "no path that reaches here can say" and "two paths disagree",
/// and the two do not need telling apart: either way nothing static may be
/// asserted about the word, so a refusal is the only honest answer.
type Held = Option<Kind>;

/// The **value operand stack**, abstracted to one [`Held`] per word, as it
/// stands on entry to every instruction of one function.
///
/// # Why this replaced a peephole
///
/// What it replaced read the `count` instructions immediately before `pc` and
/// called them the `count` operands, which is right only where every operand
/// took exactly one instruction and that instruction left exactly one word on
/// the value stack. `Cursor(at: 0, step: 1)` satisfies it: two constants, two
/// operands. `Cursor(at: i, step: here.step)` does not — the two instructions
/// before it are a `load` and a `get-field-at`, and the read *consumes* the
/// object the load pushed, so between them they leave one operand where the
/// peephole counted two.
///
/// **A misaligned window does not merely fail to name the operands; it names
/// something else.** The reading there is `[Reference, Int]` where the operands
/// are `[Int, Int]`, so the `make-struct` was refused for disagreeing with a
/// type it agrees with. `the_peepholes_window_was_not_this_programs_operands`
/// is that stated as an arithmetic fact about the program, through the same
/// `stack_shape` this counts with. A check that refuses programs it could read
/// is keeping work off the frame for no reason, and a check that could as
/// easily have derived the wrong kinds and *admitted* one is worse than that.
///
/// It is also stricter in one place, and that is the more important half. The
/// peephole read `Inst::Dup` as a reference unconditionally, because the
/// instruction it could see says nothing about what it copies. That is wrong
/// for `dup` over an `Int`, and a wrong *acceptance* is worse than a refusal
/// here: a `store-local` of such a word would put a non-handle where the frame
/// map says a handle is, which is exactly the invariant ADR 0028 decision 1
/// states for any physical arrangement. Simulating the stack gives `dup` the
/// kind it actually copies, so that program is refused rather than admitted.
/// The dispatch loop was never wrong about it — `Inst::Dup` copies the *bit* —
/// so nothing that ran was unsound; what was unsound was the check.
///
/// # How it is right
///
/// The pop and push counts are `cove_ir::lower::stack_shape`, which is the one
/// description `cove_ir::lower`'s emitter and its `validate` already read: a
/// third reader of one description cannot disagree with the two, and inventing
/// a second table here is precisely how it would. `validate` has also already
/// proved that every instruction control can reach is reached at one operand
/// *depth*, so the only thing left to settle is which [`Kind`] stands at each
/// of those depths.
///
/// The fixed point is reached because a word only ever moves from a `Kind` to
/// `None` and never back, and the depth at each instruction never changes
/// after the first path arrives — so each instruction is re-entered at most
/// once per word it holds.
struct Operands {
    /// The stack on entry to each instruction, and `None` at an instruction
    /// control cannot arrive at.
    at: Vec<Option<Vec<Held>>>,
}

impl Operands {
    /// The `count` value operands standing on top at `pc`, or `None` where any
    /// of them is a word this backend cannot name.
    fn top(&self, pc: usize, count: usize) -> Option<Vec<Kind>> {
        let stack = self.at.get(pc)?.as_ref()?;
        if stack.len() < count {
            return None;
        }
        stack[stack.len() - count..].iter().copied().collect()
    }

    /// The same question a `make-builtin` asks: what its arguments are made
    /// of, and `None` where one of them is a handle.
    ///
    /// A handle does not cross decision 5's boundary — materialising an
    /// aggregate is `crate::slot::Machine::materialise`'s job and it is not
    /// wired here — so a reference operand refuses the whole call.
    fn boundary(&self, pc: usize, argc: usize) -> Option<Vec<Kind>> {
        let kinds = self.top(pc, argc)?;
        kinds
            .iter()
            .all(|kind| *kind != Kind::Reference)
            .then_some(kinds)
    }
}

/// What one instruction leaves on the value stack, where every word it leaves
/// is the same and is not a copy of one it took.
///
/// `None` is an abstention and never a claim. An instruction whose answer is
/// only knowable at run time — a call, a `?`, an operator over general values
/// — is one this backend refuses anyway, so the abstention costs nothing it
/// would otherwise have.
fn pushed_kind(program: &Program, inst: Inst) -> Held {
    match inst {
        Inst::Const(id) => match program.constant(id) {
            Const::Unit => Some(Kind::Unit),
            Const::Int(_) => Some(Kind::Int),
            Const::Bool(_) => Some(Kind::Bool),
            Const::Float(_) => Some(Kind::Float),
            Const::Str(_) | Const::Name(_) | Const::Duration(_) => None,
        },
        Inst::ScalarToValue(Scalar::Int) => Some(Kind::Int),
        Inst::ScalarToValue(Scalar::Bool) => Some(Kind::Bool),
        Inst::LoadLocal(_) | Inst::MakeStruct(_) | Inst::SetField(_) => Some(Kind::Reference),
        // **Static because the instruction names the type.** A field read was
        // unreadable here in Phase B — one instruction whose answer is a handle
        // for a struct field and scalar bits for an `Int` one, and only the
        // object could say which. `Inst::GetFieldAt` names the
        // `cove_ir::StructType` the checker settled for the receiver, so the
        // field's slot kind is a static fact.
        Inst::GetFieldAt { of, at } => match program.struct_type(of).fields.get(at as usize) {
            Some(field) => match field.kind {
                SlotKind::Value => Some(Kind::Reference),
                SlotKind::Scalar(Scalar::Int) => Some(Kind::Int),
                SlotKind::Scalar(Scalar::Bool) => Some(Kind::Bool),
                SlotKind::Place => None,
            },
            None => None,
        },
        _ => None,
    }
}

/// Where control can be next, by instruction index.
///
/// A jump's target and a conditional jump's two, a return's nothing, and
/// everything else the instruction after it. A `?` that fails leaves the frame
/// rather than jumping inside it, so it has one successor here like any other
/// instruction that can raise.
fn successors(inst: Inst, pc: usize, out: &mut Vec<usize>) {
    out.clear();
    match inst {
        Inst::Jump(to) => out.push(to as usize),
        Inst::JumpIfFalse(to)
        | Inst::JumpIfTrue(to)
        | Inst::JumpIfFalseScalar(to)
        | Inst::JumpIfTrueScalar(to) => {
            out.push(to as usize);
            out.push(pc + 1);
        }
        Inst::Return | Inst::ReturnScalar | Inst::NoMatch => {}
        _ => out.push(pc + 1),
    }
}

/// Simulates one function's value operand stack, one abstract word per operand,
/// over every path control can take.
fn simulate(program: &Program, function: &cove_ir::Function) -> Operands {
    let structs = &program.structs;
    let mut at: Vec<Option<Vec<Held>>> = vec![None; function.code.len()];
    if at.is_empty() {
        return Operands { at };
    }
    // A body begins with nothing standing: a parameter is a slot and not an
    // operand, which is what ADR 0019's "slots, not names" means at entry.
    at[0] = Some(Vec::new());
    let mut queue = vec![0usize];
    let mut next = Vec::new();
    while let Some(pc) = queue.pop() {
        let Some(mut stack) = at[pc].clone() else {
            continue;
        };
        let inst = function.code[pc];
        let shape = cove_ir::lower::stack_shape(structs, inst);
        let (taken, left) = shape.values;
        // `Inst::Dup` is the one instruction that puts back what it took, so
        // it is the one whose answer is not `pushed_kind`'s.
        let copied = (taken == 1 && left == 2).then(|| stack.last().copied().flatten());
        for _ in 0..taken {
            stack.pop();
        }
        let put = match copied {
            Some(held) => held,
            None => pushed_kind(program, inst),
        };
        for _ in 0..left {
            stack.push(put);
        }
        successors(inst, pc, &mut next);
        for &to in &next {
            if to >= at.len() {
                continue;
            }
            let changed = match &mut at[to] {
                None => {
                    at[to] = Some(stack.clone());
                    true
                }
                Some(held) => merge(held, &stack),
            };
            if changed {
                queue.push(to);
            }
        }
    }
    Operands { at }
}

/// Merges an arriving stack into the one already recorded, and answers whether
/// anything moved.
///
/// A word two paths disagree about becomes `None`, which is the only direction
/// this ever moves a word and is why the fixed point terminates. Two arrivals
/// at different *depths* cannot happen — `cove_ir::lower::validate` refuses a
/// program where they do — and if one ever did, everything recorded here is
/// dropped to `None` rather than aligned by guessing.
fn merge(held: &mut [Held], arriving: &[Held]) -> bool {
    if held.len() != arriving.len() {
        let changed = held.iter().any(Option::is_some);
        held.iter_mut().for_each(|word| *word = None);
        return changed;
    }
    let mut changed = false;
    for (word, coming) in held.iter_mut().zip(arriving) {
        if word.is_some() && word != coming {
            *word = None;
            changed = true;
        }
    }
    changed
}

/// Which word of a struct a `set-field` names, indexed by the `ConstId` of the
/// field name.
///
/// **This is the one thing a struct field still costs a name.** `Inst::SetField`
/// carries a `Const::Name` and nothing else — the lowering's own comment says
/// "the write goes by name whatever the checker settled" — so unlike
/// `Inst::MakeStruct` and `Inst::GetFieldAt`, which now name a
/// `cove_ir::StructType`, a write has no type on it to read a position off.
/// What this does instead is ask every declared struct type where that name
/// stands, and refuse the write where two of them answer differently.
///
/// A name is interned once per string, so one entry per constant is a complete
/// table and reading it costs one indexed load rather than the walk over field
/// names `Vm::SetField` does per execution. Where two structs put the same
/// field name at different positions the entry is `None` and [`admits`]
/// refuses the function that writes it. A `set-field` that named its type is
/// what would remove this, and it is Phase D's: `lower::expr::assign_field`
/// already resolves the base's type through `Body::field_of`, so the fact
/// exists and is thrown away.
fn field_positions(program: &Program) -> Vec<Option<u32>> {
    let mut positions: Vec<Option<u32>> = vec![None; program.constants.len()];
    let mut ambiguous = vec![false; program.constants.len()];
    for function in &program.functions {
        for inst in &function.code {
            let Inst::SetField(id) = *inst else {
                continue;
            };
            let wanted = const_name(program, id);
            for declared in &program.structs {
                let Some(at) = declared
                    .fields
                    .iter()
                    .position(|field| &*field.name == wanted)
                else {
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
        | Inst::MakeStruct(_)
        | Inst::SetField(_)
        | Inst::GetFieldAt { .. }
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
    /// One layout per struct type the program declares, addressed by the
    /// `cove_ir::StructId` a `make-struct` carries.
    shapes: Vec<LayoutId>,
    /// The same reference map again, as one bool per field, addressed by the
    /// `cove_ir::StructId` a `get-field-at` carries and then by the position.
    ///
    /// Not a second source of truth: both this and [`FrameVm::shapes`] are
    /// built from one [`struct_parts`] call, and the dispatch loop's
    /// `debug_assert` reads the *object's* map beside this one on every field
    /// read. It exists because the shapes are inside the heap and reaching one
    /// is an object, a layout id and a `Vec` scan, where a field read wants a
    /// bit — and the bit is a static fact about the instruction.
    field_refs: Vec<Vec<bool>>,
    /// Whether [`FrameVm::field_refs`] is the lowered types' map or has been
    /// emptied by the mutation. See [`FieldMap`].
    field_map: FieldMap,
    /// Which word a `set-field` names, indexed by the `ConstId` of the field
    /// name. See [`field_positions`].
    field_at: Vec<Option<u32>>,
    /// One frame map per function of the program: the one layout every
    /// physical offset in a run derives from. See [`FrameMap`].
    maps: Vec<FrameMap>,
    /// Where each of a call's arguments belongs, for the functions whose
    /// arguments do not arrive in their slots, and empty for every other.
    ///
    /// One entry per argument, in the order a call supplies them, holding the
    /// slot number `cove_ir::Function::param_slot` gives it — so this is the
    /// callee's own numbering held where [`FrameVm::permute`] can walk it,
    /// exactly as [`FrameMap`] is the callee's own layout held where
    /// [`FrameVm::open`] can. Nothing here decides anything; the lowering
    /// decided it.
    arrivals: Vec<Vec<u32>>,
    /// Where the words being permuted stand while they are between two slots.
    ///
    /// **Not a root, and it does not have to be one.** A permutation is
    /// straight-line: it allocates nothing, charges no fuel and takes no
    /// safepoint, so no collection can run while a word is here — which is
    /// why every handle a permuted frame holds is yielded from the one stack
    /// exactly once, and ADR 0028 decision 8's first multiplicity is
    /// unchanged by the move. Reserved to the widest argument list the
    /// program has, so a call allocates nothing.
    moving: Vec<u64>,
    /// Whether a permuted frame's own words get their bits from the frame map
    /// or keep the ones the pushes wrote. See [`ArgumentBits`].
    argument_bits: ArgumentBits,
    /// One value-operand simulation per function, so that the only question a
    /// run puts to it — what a `make-builtin`'s arguments are made of — is an
    /// index rather than a walk. See [`Operands`].
    ///
    /// It is the same answer [`admits`] refused the run on, computed the same
    /// way and not merely consistently with it, so the `expect` at the one
    /// place that reads it cannot fire for a program that got this far.
    operands: Vec<Operands>,
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
        let field_at = field_positions(program);
        // One heap layout per *declared type*, in `cove_ir::Program::structs`'
        // own order, so a `StructId` addresses both. The reference map is the
        // type's, which is the whole of Phase C: `struct_parts` reads it off
        // `cove_ir::StructType` and a construction has no say in it.
        //
        let parts = struct_parts(program);
        let shapes = parts
            .iter()
            .zip(&program.structs)
            .map(|(parts, declared)| {
                let refs = parts
                    .iter()
                    .enumerate()
                    .filter(|(_, part)| **part == Part::Nested)
                    .map(|(at, _)| at)
                    .collect();
                heap.register(Layout::new(&*declared.name, parts.len(), refs))
            })
            .collect();
        let field_refs = parts
            .iter()
            .map(|parts| parts.iter().map(|part| *part == Part::Nested).collect())
            .collect();
        FrameVm {
            runtime,
            program,
            words: Vec::with_capacity(INITIAL_WORDS),
            refs: Bitmap::with_limbs(INITIAL_LIMBS),
            heap,
            shapes,
            field_refs,
            field_map: FieldMap::TheLoweredType,
            field_at,
            maps: program.functions.iter().map(FrameMap::of).collect(),
            arrivals: program
                .functions
                .iter()
                .map(
                    |function| match function.arguments_arrive_in_their_slots() {
                        true => Vec::new(),
                        false => (0..function.arity as usize)
                            .map(|at| {
                                function
                                    .param_slot(at)
                                    .expect("an argument below `arity` has a slot")
                            })
                            .collect(),
                    },
                )
                .collect(),
            moving: Vec::with_capacity(
                program
                    .functions
                    .iter()
                    .map(|function| function.arity as usize)
                    .max()
                    .unwrap_or(0),
            ),
            argument_bits: ArgumentBits::TheFrameMap,
            operands: program
                .functions
                .iter()
                .map(|function| simulate(program, function))
                .collect(),
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

    /// **The mutation.** Every field read says the word it pushed is scalar,
    /// whatever the type it named says.
    ///
    /// This is Phase C's mechanism removed rather than narrowed: the lowered
    /// type is the only thing that decides a field read's bit, so emptying the
    /// map is emptying the decision. What it costs is a handle standing in a
    /// word the walk skips, and the heap says so in its own words.
    /// **The mutation.** A permuted frame's argument words keep the bits
    /// their pushes wrote, instead of the bits the frame map gives the slots
    /// they were moved into.
    ///
    /// What it removes is exactly what a *moving* convention owes and a
    /// non-moving one does not, which is why it changes nothing about any
    /// frame whose arguments arrive in their slots. See [`ArgumentBits`].
    #[cfg(test)]
    fn without_moving_the_argument_bits(&mut self) {
        self.argument_bits = ArgumentBits::ThePushesThatWroteThem;
    }

    #[cfg(test)]
    fn without_the_field_map(&mut self) {
        self.field_map = FieldMap::Dropped;
        for parts in &mut self.field_refs {
            parts.fill(false);
        }
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
        self.open(function, 0);
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
        // There is no second local here and there used to be one. The
        // lowering numbered two spaces, so a value slot's number had to be
        // read through the frame map into one region from one base, and the
        // number that did it was live across the whole loop and recomputed at
        // every frame change. One numbering deleted it: a slot's number *is*
        // its offset from `base`, whichever region it is in — see `FrameMap`.
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
                    self.open(target, callee_base);
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
                    let word = self.words[base + slot as usize];
                    self.push_reference(word);
                }
                Inst::StoreLocal(slot) => {
                    let word = self.pop_word();
                    self.words[base + slot as usize] = word;
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
                Inst::MakeStruct(of) => {
                    let layout = self.shapes[of.0 as usize];
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
                // **The word's kind is the instruction's, not the object's.**
                // Phase B asked `HandleHeap::word_is_reference` here, which is
                // the object's layout consulted per read, and the module docs
                // called it "the one that cannot be static". It can:
                // `Inst::GetFieldAt` names the `cove_ir::StructType` the
                // checker settled for the receiver, and a type's field kinds
                // do not vary between two executions of one instruction. So
                // the bit is one indexed load out of a table built before the
                // run, and the heap is not asked what it is holding.
                Inst::GetFieldAt { of, at: index } => {
                    let source = Handle::from_slot(self.pop_word());
                    let at = index as usize;
                    let word = self.heap.word(source, at);
                    let is_reference = self.field_refs[of.0 as usize][at];
                    // The object's own map read beside the type's, on every
                    // field read of every debug build, so the two are compared
                    // rather than one of them trusted. Nothing at all in a
                    // release build, which is what keeps the hot path one
                    // indexed load. See [`FieldMap`] for the condition.
                    debug_assert!(
                        self.field_map == FieldMap::Dropped
                            || is_reference == self.heap.word_is_reference(source, at),
                        "the lowered type and the object it built disagree about word {at}"
                    );
                    self.push_word(word, is_reference);
                }
                // The same read whose answer went on the other stack, which is
                // one stack here — and a scalar word by construction, because
                // the lowering emits this only where the checker settled the
                // *field's* own type as `Int` or `Bool`. Nothing is asked at
                // all.
                Inst::GetFieldAtScalar(index) => {
                    let source = Handle::from_slot(self.pop_word());
                    let at = index as usize;
                    let word = self.heap.word(source, at);
                    debug_assert!(
                        !self.heap.word_is_reference(source, at),
                        "`get-field-at-scalar` read word {at}, which the object calls a reference"
                    );
                    self.push_scalar(word);
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
                    let here = self.frames.last().expect("a frame stands").function;
                    let kinds = self.operands[here.0 as usize]
                        .boundary(pc, argc as usize)
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
    fn open(&mut self, function: FunctionId, base: usize) {
        let map = self.maps[function.0 as usize];
        let width = map.width as usize;
        self.words.resize(base + width, 0);
        // The words first and the bits after them, so that what the map says
        // is the last word on both. A permutation moves words the pushes
        // already wrote bits for, and those bits are about where the word
        // *came from*; `write_frame` is what makes every bit of this frame
        // say where its word ended up. See `ArgumentBits`.
        let mut kept = 0;
        if map.arrivals == Arrivals::ToBeMoved {
            self.permute(function, base);
            if self.argument_bits == ArgumentBits::ThePushesThatWroteThem {
                kept = self.arrivals[function.0 as usize].len();
            }
        }
        let references = map.references();
        self.refs.write_frame(
            base + kept,
            width - kept,
            references.start.saturating_sub(kept)..references.end.saturating_sub(kept),
        );
    }

    /// Moves a call's arguments from where they arrived to the slots they
    /// name.
    ///
    /// The arguments arrive in declaration order, which
    /// [ADR 0021](../../../docs/adr/0021-places-are-a-static-fact.md) states
    /// as the invariant that makes pushing them left to right the same thing,
    /// and the one numbering groups slots by region. Where the two orders
    /// differ this is the difference, and it is a permutation of
    /// `words[base .. base + arity]` and nothing else: no word leaves the
    /// frame, no word enters it, and the slot numbers come from the callee's
    /// own `cove_ir::Function::param_slot`.
    ///
    /// **What it costs a call is measured rather than assumed**, which is
    /// what `benches/sortedargs` and `benches/mixedargs` are: one program
    /// written twice, once with its parameters in the numbering's order and
    /// once against it, so the two rows of one run are this loop.
    ///
    /// # What a permutation does not have to put back
    ///
    /// A word a permutation vacated — a source slot no argument ended in —
    /// keeps the bits of whatever stood there, and that is safe because such
    /// a slot is never in the value region. An argument's destination is
    /// `param_slot`, so the value destinations are exactly
    /// `value_origin .. value_origin + value parameters`, and a source below
    /// `arity` that is not one of them is at or above that range only when
    /// the function takes a place parameter, which [`admits`] refuses. The
    /// `debug_assert` below is that argument checked rather than believed,
    /// on every permuted call of every debug build. It matters because a
    /// stale copy in a walked slot would be ADR 0028 decision 8's first
    /// multiplicity broken in the way #192 broke it: one root yielded from
    /// where it is *and* from where it was.
    ///
    /// # Why the bits are not moved with the words
    ///
    /// They could be, and it would be a second answer to a question
    /// [`FrameMap`] already answers. A word's bit says whether the *slot* it
    /// stands in is a reference, and after the move each word stands in the
    /// slot the numbering gave it — so the frame map is right about every one
    /// of them, and [`FrameVm::open`] writes the whole frame from it in one
    /// masked pass it was making anyway. Carrying the pushes' bits along
    /// would be arithmetic over the same permutation to reach the same
    /// answer, and `an_arguments_bit_moves_with_the_word` is the mutation
    /// that says the two are not interchangeable in the other direction.
    fn permute(&mut self, function: FunctionId, base: usize) {
        let slots = &self.arrivals[function.0 as usize];
        debug_assert!(
            (0..slots.len() as u32)
                .filter(|source| !slots.contains(source))
                .all(|vacated| self.program.function(function).region_of(vacated)
                    != Some(cove_ir::Region::Value)),
            "a word a permutation vacated is a slot the walk reads, so the handle it \
             still holds would be yielded from the slot it left as well as the one it \
             reached"
        );
        self.moving.clear();
        self.moving
            .extend_from_slice(&self.words[base..base + slots.len()]);
        for (at, word) in self.moving.iter().enumerate() {
            self.words[base + slots[at] as usize] = *word;
        }
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
        self.open(function, 0);
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
