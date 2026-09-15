//! [ADR 0057]'s return path, over the real runtime.
//!
//! The two code generators' own suites live in `cove-native`, where the runtime
//! is a set of test doubles: a call helper that writes one recognisable word and
//! a frame that is a `Vec<u64>` a test owns. They can say what *emitted code*
//! does and they cannot say what happens to the answer afterwards, because
//! afterwards is `cove_runtime::vm::exec::native` — `open_frame`, the tier table,
//! the frame stack, the collector, and the `Vec` behind the words that
//! reallocates when a frame is pushed.
//!
//! This file is that half, and it needs a compiled function to have one. So it
//! brings its own: `interpret` below is a **third tier** that walks the same
//! optimized IR the two code generators compile, through the same
//! [`Entry`](cove_runtime::NativeEntry) signature and the same two helpers, and
//! is installed through the same [`Tiered`] table. It emits nothing and
//! therefore needs no feature and no executable page, which is why these cases
//! run in the ordinary `cargo t` rather than only in the pass that turns a code
//! generator on.
//!
//! What that buys is an oracle. Every case runs the *same* function twice over
//! the same arguments — once with [`NothingCompiled`], which is the encoded VM,
//! and once with the hand tier — and asserts the two answers are equal. A
//! destination written one word short, one word wide or one slot over is a
//! different answer, and `assert_eq!` is where it stops.
//!
//! # What it is not
//!
//! It is not a code generator and nothing here should grow into one. It
//! implements the instructions this file's own fixtures lower to and panics,
//! naming the instruction, on anything else — a fixture that changes shape
//! fails loudly rather than quietly measuring a different program.
//!
//! [ADR 0057]: ../../../docs/adr/0057-a-native-call-returns-into-the-destination-its-caller-named.md

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use cove_diag::SourceMap;
use cove_ir::{ArithOp, CmpOp, FunctionId, Inst, Num, Program as Lowered, Slot};
use cove_native::{NativeCtx, NativeHelpers, Outcome, Raise};
use cove_runtime::{
    native_helpers, Budget, Cancellation, Grants, HostRegistry, Limits, NativeEntry,
    NothingCompiled, Runtime, Tiered, Value, Vm,
};
use cove_sema::package::{Module, Package, Unit};
use cove_sema::{Compiler, Config};

/// The module the fixtures below are declared in.
const MODULE: &str = "m";

/// The package every case calls into.
///
/// Each pair is a *caller* whose whole body is a call — so its answer is the
/// callee's answer, and a destination that arrived wrong is an answer that is
/// wrong — and a *callee* of the return shape being tested: two words, no words,
/// one word that is an address, a raise, and itself.
const SOURCE: &str = "\
/// Two words, inline, which is what makes `passesPair` a multi-word return.
export struct Pair {
  a: Int
  b: Int
}

/// The identity, written so that no call to it and no call to its callers can be
/// expanded away.
///
/// `cove_ir::lower::inline` replaces a call to a **leaf** function — one that
/// calls nothing — of under sixteen instructions with the body, and every callee
/// below is otherwise exactly that. A fixture whose call had been expanded would
/// be testing a return that no longer happens, and it would still pass: the
/// answer would be right and nothing would have returned. So each callee makes
/// one call to this, which makes it a non-leaf, and this one calls `counts` —
/// which is recursive and so can never be expanded either.
///
/// `counts(0)` is zero, so `held(x)` is `x`.
export fn held(x: Int) -> Int {
  counts(0) + x
}

/// A value of no words at all, which is what makes `passesEmpty` the zero-width
/// case: `Unit` is *one* word in this IR — `Layout::word(\"Unit\", Repr::Unit)` —
/// and an empty struct is none.
export struct Empty {
}

export fn makesEmpty(x: Int) -> Empty {
  nothing(held(x))
  Empty()
}

/// The caller goes on to use a slot of its own after the call, so that the
/// zero-width destination has a frame word to be watched at: the lowering gives a
/// width-0 destination the slot past the end of everything it needs, and a frame
/// that ended there would leave nothing to look at.
export fn passesEmpty(x: Int) -> Int {
  makesEmpty(x)
  held(x) + 100
}

export fn makesPair(x: Int) -> Pair {
  Pair(a: held(x), b: x + 1)
}

/// A caller whose answer is the callee's, so the destination is the whole of
/// what is being tested.
export fn passesPair(x: Int) -> Pair {
  makesPair(x)
}

/// A `Unit` from somewhere, expanded into its caller — a leaf is the cheapest
/// somewhere.
export fn nothing(x: Int) -> Unit {
}

/// One word, and the word is a heap address rather than the object at it.
export fn echoes(s: String, n: Int) -> String {
  if held(n) < 0 {
    return s
  }
  s
}

export fn passesEcho(s: String, n: Int) -> String {
  echoes(s, n)
}

/// A read of the object a returned reference names, which is how a case says the
/// object outlived the collection rather than only that the word did.
export fn firstByte(s: String, at: Int) -> Int {
  s.byteAt(held(at))
}

/// The allocation a collection is forced with. It runs on the VM in every case —
/// `String.slice` is outside anything a native tier lowers — which is what makes
/// it a native-to-VM call besides.
///
/// It was `sliceBytes` until ADR 0058 moved that into the standard library over
/// a byte run slice, which both code generators lower. `slice` counts characters
/// rather than bytes, and every character of the text it is handed is ASCII, so
/// the answer is the same number.
export fn allocates(s: String, n: Int) -> Int {
  s.slice(held(0), n).byteLength()
}

/// A reference held in a native frame's destination slot across an allocation,
/// and then read. The order is the order the arguments are evaluated in: the
/// echo, the allocation, and the read of what the echo answered.
export fn refThroughCollection(s: String, n: Int) -> Int {
  let kept = echoes(s, n)
  firstByte(kept, allocates(s, n))
}

/// A callee that raises, so that a caller can be asked what its destination
/// holds afterwards.
export fn refuses(x: Int) -> Int {
  100 / held(x)
}

export fn passesRefusal(x: Int) -> Int {
  refuses(x)
}

/// Several frames deep, each returning into the one below it — and deep enough
/// that the stack's `Vec` reallocates while the outer destinations are pending.
export fn counts(n: Int) -> Int {
  if n <= 0 {
    0
  } else {
    counts(n - 1) + 1
  }
}

/// Writes through a `var` parameter, which is the whole address family in two
/// frames: `addr-of-slot` in whoever calls it, and `load` and `store` here.
///
/// It answers the word it wrote as well as writing it, so a case can tell the
/// right number in the wrong place from the right number in the right place.
export fn bumps(var total: Int, by: Int) -> Int {
  total = total + held(by)
  total
}

/// A caller that lends a slot of *its own* frame and then reads it back.
export fn lends(a: Int) -> Int {
  var total = a
  let seen = bumps(var total, 5)
  total * 1000 + seen
}

/// One address carried down a recursion deep enough that the stack's `Vec`
/// reallocates under every frame holding it.
///
/// This is the reason an address is a *linear* address and not a pointer: the
/// slot it names is in the bottom frame, and three hundred `push_frame`s happen
/// between the address being formed and the last write through it.
export fn lendsDeeply(n: Int, var total: Int) -> Int {
  if n <= 0 {
    total
  } else {
    total = total + 1
    lendsDeeply(n - 1, var total)
  }
}

/// The frame that owns the word every step of the chain wrote through.
export fn threadsDeeply(n: Int) -> Int {
  var total = 0
  let seen = lendsDeeply(n, var total)
  total * 1000 + seen
}

/// A caller no tier below ever compiles, so that the call it makes is a
/// **VM-to-native** hop and not something else.
///
/// Every other wrapper in this file exists to be compiled; this one exists not to
/// be. `counts` is recursive, so `cove_ir::lower::inline` cannot expand the call
/// away — see `held` — and what is left is one encoded frame whose `call`
/// instruction is the transition under test.
export fn callsCounts(n: Int) -> Int {
  counts(n)
}
";

// --- the hand-written tier ---------------------------------------------------

thread_local! {
    /// The program the entries below interpret.
    static PROGRAM: RefCell<Option<Arc<Lowered>>> = const { RefCell::new(None) };
    /// Which function each entry point stands for, by its `const` index.
    static IDS: RefCell<Vec<FunctionId>> = const { RefCell::new(Vec::new()) };
    /// The runtime's two helpers, as compiled code is given them.
    static HELPERS: Cell<Option<NativeHelpers>> = const { Cell::new(None) };
    /// Every distinct `NativeCtx::words` an entry has been handed, in order of
    /// first sight.
    ///
    /// More than one means the stack's `Vec` reallocated *while native frames
    /// were live*, which is the case indices exist for and the one a test cannot
    /// otherwise know it constructed.
    static SEGMENTS: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
    /// What a call's destination held before it was made and after it came back.
    ///
    /// The one thing a caller cannot show from its answer: a raise leaves
    /// through the same `return` a wrong answer would, and the frame it wrote
    /// into is popped behind it.
    static WITNESS: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    /// Which functions the hand tier was entered for, in order.
    ///
    /// The witness a *callee* leaves. [`WITNESS`] is what a caller saw of its
    /// destination, so a compiled function nobody compiled a caller for appears
    /// in neither — which is exactly the shape of a VM-to-native call, and a
    /// counter moving with no code behind it is the one failure a transition test
    /// must not pass through.
    static ENTERED: RefCell<Vec<FunctionId>> = const { RefCell::new(Vec::new()) };
    /// Whether a call is made the *direct* way: `open`, the callee's entry, and
    /// `close`, rather than the one mediated `call` helper.
    ///
    /// Issue #365's Part 2. The two protocols are both the runtime's, and this
    /// tier exercises either — which is what makes the direct one testable in the
    /// ordinary `cargo t`, with no code generator and no executable page. What a
    /// code generator adds on top is the argument copy, and this does that copy
    /// too, at the same widths out of the same slots.
    static DIRECT: Cell<bool> = const { Cell::new(false) };
    /// Whether to fill a destination with [`SENTINEL`] before each call.
    ///
    /// Off by default, because it is not something emitted code does. On for the
    /// cases that ask what a return *did not* write: a destination is a zeroed
    /// frame slot otherwise, and "still zero" is a weaker sentence than "still
    /// the number nothing else in this run could have put there".
    static POISON: Cell<bool> = const { Cell::new(false) };
}

/// A word no fixture computes and no frame holds, so finding one is finding an
/// unwritten word.
const SENTINEL: u64 = 0x5555_0055_5500_5555;

/// One call's destination, before it was made and after it answered.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    callee: FunctionId,
    outcome: u32,
    before: Vec<u64>,
    after: Vec<u64>,
}

/// A compiled function's entry point, one per `const` index.
///
/// An [`Entry`](cove_runtime::NativeEntry) is handed a context and a frame and
/// no identity, exactly as a code generator's is: which function it is, is a
/// fact about the *code*, and here that fact is the `const` parameter.
///
/// # Safety
///
/// As [`cove_runtime::NativeEntry`]: `ctx` is valid and uniquely borrowed and
/// `base` is this call's frame, with its parameters in place.
unsafe extern "C" fn entry<const WHICH: usize>(
    ctx: *mut NativeCtx,
    base: u64,
    return_base: u64,
    return_slot: u32,
) -> Outcome {
    interpret(WHICH, ctx, base, return_base, return_slot)
}

/// The entry points, which is how many functions one case may install.
const ENTRIES: [NativeEntry; 6] = [
    entry::<0>, entry::<1>, entry::<2>, entry::<3>, entry::<4>, entry::<5>,
];

/// The tier table: ADR 0055's `FunctionId -> native entry`, for the functions a
/// case asked for and no others.
struct Hand(BTreeMap<FunctionId, NativeEntry>);

impl Tiered for Hand {
    fn entry(&self, id: FunctionId) -> Option<NativeEntry> {
        self.0.get(&id).copied()
    }
}

/// Installs `names` as the hand-written tier over `program`.
fn hand(program: &Arc<Lowered>, names: &[&str]) -> Hand {
    hand_with(program, names, native_helpers())
}

/// The same, over a helper table a case chose.
///
/// The one case that chooses is the ablation case below: issue #365's
/// decomposition measures variants of the call helper, and a variant that
/// answered something else would make the measurement worthless in the one way a
/// measurement cannot survive.
fn hand_with(program: &Arc<Lowered>, names: &[&str], helpers: NativeHelpers) -> Hand {
    assert!(
        names.len() <= ENTRIES.len(),
        "there are {} entry points and this case asked for {}",
        ENTRIES.len(),
        names.len()
    );
    PROGRAM.with(|held| *held.borrow_mut() = Some(Arc::clone(program)));
    HELPERS.with(|held| held.set(Some(helpers)));
    SEGMENTS.with(|seen| seen.borrow_mut().clear());
    WITNESS.with(|seen| seen.borrow_mut().clear());
    ENTERED.with(|seen| seen.borrow_mut().clear());
    let mut ids = Vec::new();
    let mut table = BTreeMap::new();
    for (at, name) in names.iter().enumerate() {
        let id = program
            .function_named(MODULE, name)
            .unwrap_or_else(|| panic!("`{MODULE}.{name}` is lowered"));
        ids.push(id);
        table.insert(id, ENTRIES[at]);
    }
    IDS.with(|held| *held.borrow_mut() = ids);
    Hand(table)
}

/// The word at `slot` of the frame at `base`.
///
/// The pointer is re-read from the context at every access rather than held,
/// which is stricter than the discipline `cove_native::abi` asks of emitted code
/// and is the right side to err on in a test: a helper that forgot to republish
/// would be caught here rather than produce a reader of freed memory.
///
/// # Safety
///
/// `ctx` is the entry's context and `base + slot` is a word of its segment.
unsafe fn word(ctx: *mut NativeCtx, base: u64, slot: Slot) -> u64 {
    (*ctx).words.add((base + u64::from(slot)) as usize).read()
}

/// # Safety
///
/// As [`word`].
unsafe fn set(ctx: *mut NativeCtx, base: u64, slot: Slot, value: u64) {
    (*ctx)
        .words
        .add((base + u64::from(slot)) as usize)
        .write(value)
}

/// The **linear address** of `slot` of the frame at `base`.
///
/// `base` is a word index relative to the segment's origin, and a `Repr::Addr`
/// word is not: the difference is [`NativeCtx::stack_origin`], and forgetting it
/// is the one mistake the ABI's shape makes easy. On the first segment the two
/// numbers are equal, which is why this test file cannot catch that mistake and
/// `cove-native`'s own suite — whose segment deliberately does not begin at zero —
/// can.
///
/// # Safety
///
/// As [`word`].
unsafe fn address(ctx: *mut NativeCtx, base: u64, slot: Slot) -> u64 {
    (*ctx).stack_origin + base + u64::from(slot)
}

/// The word at the linear address `addr`, in whichever region it names.
///
/// `Memory::read`'s `is_stack(addr)` and the two arms behind it, written a third
/// time. The heap arm is the chunk spine: one table entry per committed chunk,
/// and the word inside it.
///
/// # Safety
///
/// `ctx` is the entry's context and `addr` is an address of a value location it
/// may reach.
unsafe fn at_addr(ctx: *mut NativeCtx, addr: u64) -> u64 {
    match region(ctx, addr) {
        Region::Stack(at) => (*ctx).words.add(at).read(),
        Region::Heap(chunk, at) => (*ctx).chunks.add(chunk).read().add(at).read(),
    }
}

/// [`at_addr`] in the other direction.
///
/// # Safety
///
/// As [`at_addr`].
unsafe fn set_at_addr(ctx: *mut NativeCtx, addr: u64, held: u64) {
    match region(ctx, addr) {
        Region::Stack(at) => (*ctx).words.add(at).write(held),
        Region::Heap(chunk, at) => (*ctx).chunks.add(chunk).read().add(at).write(held),
    }
}

/// Which region an address names, and where in it.
enum Region {
    /// An index into [`NativeCtx::words`].
    Stack(usize),
    /// A chunk of [`NativeCtx::chunks`], and a word inside it.
    Heap(usize, usize),
}

/// # Safety
///
/// As [`at_addr`].
unsafe fn region(ctx: *mut NativeCtx, addr: u64) -> Region {
    if addr < cove_native::HEAP_ORIGIN_WORDS {
        return Region::Stack((addr - (*ctx).stack_origin) as usize);
    }
    let index = addr - cove_native::HEAP_ORIGIN_WORDS;
    Region::Heap(
        (index >> cove_native::HEAP_CHUNK_SHIFT) as usize,
        (index & (cove_native::HEAP_CHUNK_WORDS - 1)) as usize,
    )
}

/// One function's IR, walked as a tier.
///
/// Every slot read is a load and every slot write is a store, which is the
/// property both code generators have and the reason the collector needs no
/// spill map: at the two points a collection can happen — the safepoint helper
/// and the call helper — every live reference is in the slot
/// `Function::refs` names.
///
/// # Safety
///
/// As [`entry`].
unsafe fn interpret(
    which: usize,
    ctx: *mut NativeCtx,
    base: u64,
    return_base: u64,
    return_slot: u32,
) -> Outcome {
    let program = PROGRAM
        .with(|held| held.borrow().clone())
        .expect("a case installed a program");
    let id = IDS.with(|held| held.borrow()[which]);
    let helpers = HELPERS
        .with(Cell::get)
        .expect("a case installed the helpers");
    let function = program.function(id);
    ENTERED.with(|seen| seen.borrow_mut().push(id));
    SEGMENTS.with(|seen| {
        let mut seen = seen.borrow_mut();
        let at = (*ctx).words as usize;
        if seen.last() != Some(&at) {
            seen.push(at);
        }
    });

    // The unpaid work, as ADR 0055 states it: a count of IR instructions, held
    // in a register between safepoints and published at every exit.
    let mut work = 0u64;
    let mut pc = 0usize;
    loop {
        work += 1;
        match &function.code[pc] {
            Inst::Unit { .. } => pc += 1,
            Inst::Int { dst, value } => {
                set(ctx, base, *dst, *value as u64);
                pc += 1;
            }
            Inst::Bool { dst, value } => {
                set(ctx, base, *dst, u64::from(*value));
                pc += 1;
            }
            Inst::Copy { dst, src, layout } => {
                let width = program.layout(*layout).width();
                // Loaded before stored, because two slots of one frame may
                // overlap: `Memory::copy_slots` is a `memmove` and so is this.
                let held: Vec<u64> = (0..width).map(|at| word(ctx, base, *src + at)).collect();
                for (at, held) in held.into_iter().enumerate() {
                    set(ctx, base, *dst + at as Slot, held);
                }
                pc += 1;
            }
            Inst::Clear { slot, layout } => {
                for at in 0..program.layout(*layout).width() {
                    set(ctx, base, *slot + at, 0);
                }
                pc += 1;
            }
            Inst::Arith {
                num: Num::Int,
                op,
                dst,
                a,
                b,
            } => {
                let x = word(ctx, base, *a) as i64;
                let y = word(ctx, base, *b) as i64;
                match arith(*op, x, y) {
                    Ok(answer) => {
                        set(ctx, base, *dst, answer as u64);
                        pc += 1;
                    }
                    Err(raise) => return leave(ctx, work, raise, pc),
                }
            }
            Inst::ArithImm { op, dst, a, value } => {
                let x = word(ctx, base, *a) as i64;
                match arith(*op, x, *value) {
                    Ok(answer) => {
                        set(ctx, base, *dst, answer as u64);
                        pc += 1;
                    }
                    Err(raise) => return leave(ctx, work, raise, pc),
                }
            }
            Inst::CmpImm { op, dst, a, value } => {
                let x = word(ctx, base, *a) as i64;
                set(ctx, base, *dst, u64::from(compare(*op, x, *value)));
                pc += 1;
            }
            Inst::CmpImmBranch {
                op,
                dst,
                a,
                value,
                target,
            } => {
                let x = word(ctx, base, *a) as i64;
                let answer = compare(*op, x, i64::from(*value));
                set(ctx, base, *dst, u64::from(answer));
                match branch(ctx, &helpers, pc, *target, answer, &mut work) {
                    Ok(next) => pc = next,
                    Err(outcome) => return outcome,
                }
            }
            Inst::BranchFalse { cond, to } => {
                let answer = word(ctx, base, *cond) != 0;
                match branch(ctx, &helpers, pc, *to, answer, &mut work) {
                    Ok(next) => pc = next,
                    Err(outcome) => return outcome,
                }
            }
            Inst::Jump { to } => match branch(ctx, &helpers, pc, *to, false, &mut work) {
                Ok(next) => pc = next,
                Err(outcome) => return outcome,
            },
            Inst::Call { dst, callee, args } => {
                let width = program.layout(program.function(*callee).returns).width();
                let watched = witnessed(function.reprs.len() as Slot, *dst, width);
                if POISON.with(Cell::get) {
                    for at in 0..watched {
                        set(ctx, base, *dst + at, SENTINEL);
                    }
                }
                let before: Vec<u64> = (0..watched).map(|at| word(ctx, base, *dst + at)).collect();

                // A call is a safepoint, so the unpaid work goes over with it and
                // the helper charges it.
                (*ctx).pending_work = work;
                work = 0;
                let outcome = if DIRECT.with(Cell::get) {
                    direct(&helpers, &program, ctx, base, pc, *callee, *args, *dst)
                } else {
                    (helpers.call)(ctx, base, pc as u32, callee.0, args.0, *dst)
                };

                // Read whatever the outcome was: this frame is still this
                // function's until it returns, so the destination is readable on
                // the raising path too — which is the whole of what "a raised
                // call publishes no result" is asserted against.
                let after: Vec<u64> = (0..watched).map(|at| word(ctx, base, *dst + at)).collect();
                WITNESS.with(|seen| {
                    seen.borrow_mut().push(Seen {
                        callee: *callee,
                        outcome,
                        before,
                        after,
                    })
                });
                if outcome != Outcome::Returned.abi() {
                    // A raise or a stop is returned unchanged, so it leaves
                    // through one `return` per frame and nothing unwinds.
                    return outcome_of(outcome);
                }
                pc += 1;
            }
            // ---- places -------------------------------------------------
            //
            // A `Repr::Addr` slot holds a **linear word index**, so the three
            // instructions below are the third independent implementation of
            // `cove_native::abi`'s "An address names either region": this one, the
            // Cranelift arm's and the template arm's. That is the point of them
            // being here — a disagreement about what the word means is a wrong
            // word written into whatever the number happened to name, and the
            // encoded tier is the oracle for all three.
            Inst::AddrOfSlot { dst, slot } => {
                set(ctx, base, *dst, address(ctx, base, *slot));
                pc += 1;
            }
            Inst::AddrOfPart { dst, addr, at } => {
                let held = word(ctx, base, *addr);
                set(ctx, base, *dst, held + u64::from(*at));
                pc += 1;
            }
            Inst::Load { dst, addr, layout } => {
                let at = word(ctx, base, *addr);
                let width = program.layout(*layout).width();
                // Read before written, because `Memory::copy_words` is a
                // `memmove` and an address of this frame makes the runs overlap.
                let held: Vec<u64> = (0..width)
                    .map(|w| at_addr(ctx, at + u64::from(w)))
                    .collect();
                for (w, held) in held.into_iter().enumerate() {
                    set(ctx, base, *dst + w as Slot, held);
                }
                pc += 1;
            }
            Inst::Store { addr, src, layout } => {
                let at = word(ctx, base, *addr);
                let width = program.layout(*layout).width();
                let held: Vec<u64> = (0..width).map(|w| word(ctx, base, *src + w)).collect();
                for (w, held) in held.into_iter().enumerate() {
                    set_at_addr(ctx, at + w as u64, held);
                }
                pc += 1;
            }
            // ADR 0057: the answer goes into the run the caller named, before
            // this frame is taken away, and a width of zero writes nothing at
            // all — the destination of a zero-width return may be a slot the
            // caller's frame does not have.
            Inst::Return { src } => {
                (*ctx).pending_work = work;
                let width = program.layout(function.returns).width();
                for at in 0..width {
                    let held = word(ctx, base, *src + at);
                    set(ctx, return_base, return_slot + at, held);
                }
                return Outcome::Returned;
            }
            other => panic!(
                "`{}.{}` lowered {other:?}, which this tier does not run",
                function.module, function.name
            ),
        }
    }
}

/// One call, made the way generated code makes a direct one.
///
/// The protocol of `cove_native::OpenFn` and `cove_native::CloseFn`, in the order
/// emitted code performs it and with the one step that belongs to the *code*
/// rather than to the runtime done here too: the arguments, copied out of the
/// caller's slots into the frame `open` answered, at the widths the callee's
/// parameters declare. A code generator knows those statically; this reads them
/// out of the program, which is the same numbers by a slower route.
///
/// `entry: None` is the runtime saying it finished the call itself — a callee
/// with no compiled code — and then there is nothing to enter and nothing to
/// close.
///
/// # Safety
///
/// As [`interpret`].
#[allow(clippy::too_many_arguments)]
unsafe fn direct(
    helpers: &NativeHelpers,
    program: &Arc<Lowered>,
    ctx: *mut NativeCtx,
    base: u64,
    pc: usize,
    callee: FunctionId,
    args: cove_ir::ArgsId,
    dst: Slot,
) -> u32 {
    let opened = (helpers.open)(ctx, base, pc as u32, callee.0, args.0, dst);
    let Some(entry) = opened.entry else {
        return opened.base as u32;
    };
    // The words pointer is re-read for every one of these, because `open` pushed
    // a frame and a `Vec::resize` moves the words. Emitted code re-derives it
    // once, after the call, for the same reason.
    let target = program.function(callee);
    let mut at = 0;
    for (arg, layout) in program.arg_list(args).iter().zip(&target.params) {
        let width = program.layout(*layout).width();
        for word_at in 0..width {
            let held = word(ctx, base, arg.slot + word_at);
            set(ctx, opened.base, at + word_at, held);
        }
        at += width;
    }
    // The call itself, and the destination is the caller's frame and the slot the
    // lowering settled: ADR 0057's two indices, handed down rather than reported
    // back.
    let answered = entry(ctx, opened.base, base, dst);
    (helpers.close)(ctx, answered.abi(), callee.0)
}

/// How many words of a destination a case watches.
///
/// The answer's width and one word past it where the frame has one, so that a
/// return which wrote a word too many is a failure and not a coincidence.
fn witnessed(frame: Slot, dst: Slot, width: u32) -> u32 {
    (width + 1).min(frame.saturating_sub(dst))
}

/// Takes `target` when `taken` is false, and the next instruction otherwise.
///
/// A backward target is a loop backedge and carries the safepoint, which is
/// where both code generators put theirs. `Err` is a stop, which leaves.
///
/// # Safety
///
/// As [`interpret`].
unsafe fn branch(
    ctx: *mut NativeCtx,
    helpers: &NativeHelpers,
    pc: usize,
    target: u32,
    taken: bool,
    work: &mut u64,
) -> Result<usize, Outcome> {
    let next = if taken { pc + 1 } else { target as usize };
    if next <= pc {
        let charged = std::mem::take(work);
        if !(helpers.safepoint)(ctx, next as u32, charged) {
            // Nothing is pending at a stop: the charge went over before the
            // helper answered.
            (*ctx).pending_work = 0;
            return Err(Outcome::Stopped);
        }
    }
    Ok(next)
}

/// `encoded.rs`'s `int_arith`, in the one shape this file's fixtures need.
fn arith(op: ArithOp, x: i64, y: i64) -> Result<i64, Raise> {
    match op {
        ArithOp::Add => x.checked_add(y).ok_or(Raise::AddOverflowed),
        ArithOp::Sub => x.checked_sub(y).ok_or(Raise::SubOverflowed),
        ArithOp::Mul => x.checked_mul(y).ok_or(Raise::MulOverflowed),
        // The zero divisor is tested first, because `i64::MIN / 0` has to say
        // "by zero" and not "overflowed".
        ArithOp::Div if y == 0 => Err(Raise::DividedByZero),
        ArithOp::Rem if y == 0 => Err(Raise::RemainderByZero),
        ArithOp::Div => x.checked_div(y).ok_or(Raise::DivOverflowed),
        ArithOp::Rem => x.checked_rem(y).ok_or(Raise::RemOverflowed),
    }
}

fn compare(op: CmpOp, x: i64, y: i64) -> bool {
    match op {
        CmpOp::Eq => x == y,
        CmpOp::Ne => x != y,
        CmpOp::Lt => x < y,
        CmpOp::Le => x <= y,
        CmpOp::Gt => x > y,
        CmpOp::Ge => x >= y,
    }
}

/// Leaves with an error named rather than built, which is the boundary's whole
/// division of labour: this side names the operation, `cove-runtime` writes the
/// sentence.
///
/// # Safety
///
/// As [`interpret`].
unsafe fn leave(ctx: *mut NativeCtx, work: u64, raise: Raise, pc: usize) -> Outcome {
    (*ctx).pending_work = work;
    (*ctx).raise_code = raise.abi();
    (*ctx).raise_detail = 0;
    (*ctx).raise_pc = pc as u32;
    Outcome::Raised
}

fn outcome_of(abi: u32) -> Outcome {
    match abi {
        0 => Outcome::Returned,
        1 => Outcome::Raised,
        2 => Outcome::Stopped,
        other => panic!("the call helper answered {other}, which is not an outcome"),
    }
}

// --- the fixture -------------------------------------------------------------

/// Parses, checks and lowers [`SOURCE`], and runs `body` over a `Vm` on it.
///
/// A closure rather than a returned `Vm`, because a `Vm` borrows the runtime,
/// the host registry and the lowered program, and all three live here.
fn with_vm(heap_words: usize, body: impl FnOnce(&mut Vm<'_>, &Arc<Lowered>)) {
    let (sources, checked) = checked();
    let lowered = Arc::new(
        cove_ir::lower(&checked, &sources, &cove_sema::HostSchemas::new()).unwrap_or_else(
            |items| {
                panic!(
                    "the fixture lowers:\n{}",
                    items
                        .iter()
                        .map(|item| cove_diag::render(&sources, item))
                        .collect::<Vec<_>>()
                        .join("")
                )
            },
        ),
    );
    let hosts = Arc::new(HostRegistry::new(Grants::new(Vec::<&str>::new())));
    let runtime = Runtime::new(
        Arc::clone(&checked),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    let mut vm = Vm::with_heap_words(&runtime, &hosts, &lowered, heap_words);
    body(&mut vm, &lowered);
}

/// The same, over a run the embedder bounded.
///
/// `with_vm` grants `Limits::default()`, which bounds nothing — "A `None` field
/// imposes nothing" — and the one case that needs a bound is the runaway
/// recursion: `max_call_depth` is what refuses it, and a default budget has none.
/// The budget is installed *before* the `Vm` is built, because the meter is taken
/// where a run begins.
fn with_limited_vm(
    heap_words: usize,
    limits: Limits,
    body: impl FnOnce(&mut Vm<'_>, &Arc<Lowered>),
) {
    with_bounded_vm(heap_words, limits, |vm, lowered, _| body(vm, lowered));
}

/// The same, with the run's [`Cancellation`] handed to the body.
///
/// The one thing `with_limited_vm` cannot do: a case that asks what a *cancelled*
/// VM-to-native call does has to be able to cancel it, and the handle is the only
/// way. It is the same handle the budget was built over, so flipping it is what a
/// host or a signal would do.
fn with_bounded_vm(
    heap_words: usize,
    limits: Limits,
    body: impl FnOnce(&mut Vm<'_>, &Arc<Lowered>, &Cancellation),
) {
    let (sources, checked) = checked();
    let lowered = Arc::new(
        cove_ir::lower(&checked, &sources, &cove_sema::HostSchemas::new())
            .expect("the fixture lowers"),
    );
    let cancellation = Cancellation::new();
    let mut hosts = HostRegistry::new(Grants::new(Vec::<&str>::new()));
    hosts.set_budget(Budget::with_cancellation(limits, cancellation.clone()));
    let hosts = Arc::new(hosts);
    let runtime = Runtime::new(
        Arc::clone(&checked),
        Arc::clone(&sources),
        Arc::clone(&hosts),
    );
    let mut vm = Vm::with_heap_words(&runtime, &hosts, &lowered, heap_words);
    body(&mut vm, &lowered, &cancellation);
}

/// Runs `body` with every call in it made the direct way, and puts the switch
/// back.
///
/// See [`DIRECT`]. A guard rather than two lines at each end of a case, because a
/// case that failed between them would leave the switch on for whatever ran next
/// in the same thread.
fn directly<T>(body: impl FnOnce() -> T) -> T {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            DIRECT.with(|on| on.set(false));
        }
    }
    let _restore = Restore;
    DIRECT.with(|on| on.set(true));
    body()
}

/// What `Vm::new` uses, so that a case which does not care about collection gets
/// the ordinary heap.
const ORDINARY_HEAP_WORDS: usize = 1 << 22;

fn checked() -> (Arc<SourceMap>, Arc<cove_sema::resolve::Program>) {
    let mut sources = SourceMap::new();
    let path = PathBuf::from("m/main.cove");
    let file = sources.add(path.clone(), SOURCE);
    let ast = cove_syntax::parse_file(&sources, file).expect("the fixture parses");
    let mut modules = BTreeMap::from([(
        MODULE.to_string(),
        Module {
            name: MODULE.to_string(),
            dir: PathBuf::from(MODULE),
            units: vec![Unit { file, path, ast }],
        },
    )]);
    for (name, module) in cove_sema::stdlib::attach(&mut sources).expect("stdlib parses") {
        modules.insert(name, module);
    }
    let package = Package {
        root: PathBuf::new(),
        config: Config::default(),
        modules,
    };
    match Compiler::new().compile(&package) {
        Ok(program) => (Arc::new(sources), Arc::new(program)),
        Err(items) => panic!(
            "the fixture checks:\n{}",
            items
                .iter()
                .map(|item| cove_diag::render(&sources, item))
                .collect::<Vec<_>>()
                .join("")
        ),
    }
}

/// Every destination a case's calls were made into.
fn witness() -> Vec<Seen> {
    WITNESS.with(|seen| seen.borrow().clone())
}

/// The calls made into `MODULE.name`, in the order they were made.
///
/// Filtered by callee rather than taken as a whole, because a native callee makes
/// calls of its own: every fixture below calls `held` to keep the inliner off it,
/// and that call is in the witness too.
fn seen_into(lowered: &Arc<Lowered>, name: &str) -> Vec<Seen> {
    let id = lowered
        .function_named(MODULE, name)
        .unwrap_or_else(|| panic!("`{MODULE}.{name}` is lowered"));
    witness()
        .into_iter()
        .filter(|seen| seen.callee == id)
        .collect()
}

/// How many times the hand tier was entered for `MODULE.name`. See [`ENTERED`].
fn entries_into(lowered: &Arc<Lowered>, name: &str) -> usize {
    let id = lowered
        .function_named(MODULE, name)
        .unwrap_or_else(|| panic!("`{MODULE}.{name}` is lowered"));
    ENTERED.with(|seen| seen.borrow().iter().filter(|held| **held == id).count())
}

/// How many distinct segments the native frames ran over. See [`SEGMENTS`].
fn segments() -> usize {
    SEGMENTS.with(|seen| seen.borrow().len())
}

// --- the cases ---------------------------------------------------------------

/// A two-word answer, into a destination the caller named, checked against the
/// VM's answer for the same call.
#[test]
fn a_multi_word_answer_reaches_the_destination() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        let mut session = vm
            .native_session(MODULE, "passesPair", vec![Value::int(41)])
            .expect("the session opens");
        let expected = session
            .call(&NothingCompiled, &[41])
            .expect("the vm answers");
        assert_eq!(
            expected,
            vec![41, 42],
            "the fixture answers `Pair(x, x + 1)`"
        );

        // Both functions native, so the answer crosses one native-to-native
        // return and lands in the caller's destination.
        let tier = hand(lowered, &["passesPair", "makesPair"]);
        POISON.with(|on| on.set(true));
        let answered = session.call(&tier, &[41]).expect("the hand tier answers");
        POISON.with(|on| on.set(false));
        assert_eq!(answered, expected);

        let seen = seen_into(lowered, "makesPair");
        assert_eq!(seen.len(), 1, "one call was made into `makesPair`");
        assert_eq!(
            seen[0].before,
            vec![SENTINEL; seen[0].before.len()],
            "the destination was poisoned before the call"
        );
        assert_eq!(
            &seen[0].after[..2],
            &[41, 42],
            "the two words the callee answered are the two words the destination holds"
        );
        // Whether a *third* word was written is a question about the generator
        // rather than about the runtime, and it is asked where the destination
        // run is the test's own: `cove-native`'s suite hands its entry a
        // destination with spare words after it. Here the destination is a real
        // frame and `passesPair`'s has none to spare.
        assert_eq!(
            seen[0].after.len(),
            2,
            "the frame holds the destination and no more"
        );
        assert!(
            session.tiers().native() >= 2,
            "the caller and the callee both ran natively"
        );
    });
}

/// The same answer with the callee on the VM, which is the other side of the
/// boundary and the same destination.
#[test]
fn a_vm_callee_answers_into_a_native_caller() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        let mut session = vm
            .native_session(MODULE, "passesPair", vec![Value::int(7)])
            .expect("the session opens");
        let expected = session
            .call(&NothingCompiled, &[7])
            .expect("the vm answers");

        // Only the caller is compiled, so `makesPair` runs on the encoded tier
        // and its answer reaches the destination through the other half of the
        // helper.
        let tier = hand(lowered, &["passesPair"]);
        let before = session.tiers();
        let answered = session.call(&tier, &[7]).expect("the hand tier answers");
        assert_eq!(answered, expected);
        assert_eq!(answered, vec![7, 8]);
        let tiers = session.tiers();
        assert_eq!(
            tiers.host_to_native - before.host_to_native,
            1,
            "one compiled outermost frame"
        );
        assert!(
            tiers.native_to_vm - before.native_to_vm >= 1,
            "`makesPair` has no compiled entry, so compiled code called the VM"
        );
        assert_eq!(
            tiers.vm_to_native - before.vm_to_native,
            0,
            "nothing the VM called was compiled, so this hop was not taken"
        );
    });
}

/// A zero-width return writes nothing at all.
#[test]
fn a_zero_width_return_writes_nothing() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        let mut session = vm
            .native_session(MODULE, "passesEmpty", vec![Value::int(5)])
            .expect("the session opens");
        let expected = session
            .call(&NothingCompiled, &[5])
            .expect("the vm answers");
        assert_eq!(expected, vec![105]);

        let tier = hand(lowered, &["passesEmpty", "makesEmpty"]);
        POISON.with(|on| on.set(true));
        let answered = session.call(&tier, &[5]).expect("the hand tier answers");
        POISON.with(|on| on.set(false));
        assert_eq!(answered, expected);

        let seen = seen_into(lowered, "makesEmpty");
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].before,
            vec![SENTINEL],
            "the caller reuses the destination slot for its next call, so there is a word to watch"
        );
        assert_eq!(
            seen[0].after, seen[0].before,
            "a zero-width return left the word at its destination as it found it"
        );
    });
}

/// A reference return copies one word, and the word is the address the caller
/// handed in rather than a copy of the object.
#[test]
fn a_reference_return_copies_one_word() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        let mut session = vm
            .native_session(
                MODULE,
                "passesEcho",
                vec![Value::string("a string"), Value::int(0)],
            )
            .expect("the session opens");
        let words = session.arguments().to_vec();
        let expected = session
            .call(&NothingCompiled, &words)
            .expect("the vm answers");
        assert_eq!(expected.len(), 1, "a reference is one word");
        assert_eq!(
            expected[0], words[0],
            "the echo answers the address it was given and does not copy the object"
        );

        let tier = hand(lowered, &["passesEcho", "echoes"]);
        let answered = session.call(&tier, &words).expect("the hand tier answers");
        assert_eq!(answered, expected);
    });
}

/// A returned reference, a collection, and then a read of the object it names.
///
/// The destination of a native call is a slot of a native frame, and a native
/// frame is walked by the collector through `Function::refs` — so the word that
/// arrived there has to keep its object alive. A run that published the answer
/// somewhere the walk does not look would sweep the string and the read would
/// answer a different byte or fail.
#[test]
fn a_returned_reference_survives_a_collection() {
    // One heap chunk, which is the smallest a heap is: the slices `allocates`
    // builds do not all fit in it, so it has to be collected while the native
    // frames are live.
    const SMALL_HEAP_WORDS: usize = 1 << 13;
    with_vm(SMALL_HEAP_WORDS, |vm, lowered| {
        let text = "a string long enough that slicing it fills a heap chunk, and long enough \
                    that a byte can be read out of the middle of it without asking whether it \
                    is there: sixty-four bytes in is well inside this sentence.";
        // The byte the read answers and the length the slice allocates, in one
        // number because the fixture takes one.
        const AT: i64 = 64;
        let mut session = vm
            .native_session(
                MODULE,
                "refThroughCollection",
                vec![Value::string(text), Value::int(AT)],
            )
            .expect("the session opens");
        let words = session.arguments().to_vec();
        let expected = session
            .call(&NothingCompiled, &words)
            .expect("the vm answers");
        assert_eq!(
            expected,
            vec![u64::from(text.as_bytes()[AT as usize])],
            "the fixture answers byte `n` of the string the echo handed back"
        );

        let tier = hand(lowered, &["refThroughCollection", "echoes"]);
        let before = session.collections();
        let mut calls = 0;
        // Until a collection has run, so that the case proves what it claims
        // rather than depending on a heap size staying small enough.
        while session.collections() == before && calls < 20_000 {
            let answered = session.call(&tier, &words).expect("the hand tier answers");
            assert_eq!(answered, expected, "call {calls} answered wrongly");
            calls += 1;
        }
        assert!(
            session.collections() > before,
            "no collection ran in {calls} call(s), so this case proved nothing"
        );
    });
}

/// A raised call publishes no result: the destination holds what it held.
#[test]
fn a_raised_call_publishes_nothing() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        let mut session = vm
            .native_session(MODULE, "passesRefusal", vec![Value::int(0)])
            .expect("the session opens");
        let refused = session
            .call(&NothingCompiled, &[0])
            .expect_err("dividing by zero is refused");
        assert!(
            refused.message.contains("zero"),
            "the vm's message names the division: {}",
            refused.message
        );

        let tier = hand(lowered, &["passesRefusal", "refuses"]);
        POISON.with(|on| on.set(true));
        let mine = session
            .call(&tier, &[0])
            .expect_err("the hand tier refuses it too");
        POISON.with(|on| on.set(false));
        assert_eq!(
            mine.message, refused.message,
            "the two tiers refuse it with one sentence"
        );

        let seen = seen_into(lowered, "refuses");
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].outcome, Outcome::Raised.abi());
        assert_eq!(
            seen[0].after, seen[0].before,
            "a raised call left every word of the destination as it found it"
        );
        assert!(
            seen[0].after.iter().all(|word| *word == SENTINEL),
            "and what it found was the sentinel: {:?}",
            seen[0].after
        );

        // The session is still usable, which is what says the failure put the
        // stack back: a destination write past the frame would not have.
        let answered = session.call(&tier, &[4]).expect("a divisor that works");
        assert_eq!(answered, vec![25]);
    });
}

/// Recursion, returning through several frames, and through a reallocation of
/// the stack.
///
/// The reallocation is the reason `return_base` is an index. Every frame's
/// destination is pending while the frames below it run, and `push_frame` is a
/// `Vec::resize`: a destination held as a *pointer* would be dangling by the time
/// the innermost call returned into it. [`SEGMENTS`] is how the case knows it
/// constructed one rather than hoped for it.
#[test]
fn a_return_finds_a_destination_a_reallocation_moved() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        const DEEP: i64 = 300;
        let mut session = vm
            .native_session(MODULE, "counts", vec![Value::int(DEEP)])
            .expect("the session opens");
        // The hand tier goes *first* here, which is the opposite order to every
        // other case and is the whole of what makes this one work: a `Vec` keeps
        // its capacity across `Vec::clear`, so a 300-frame run on the VM would
        // leave the stack large enough that the next run never reallocates — and
        // the case would pass while testing nothing.
        let tier = hand(lowered, &["counts"]);
        let answered = session
            .call(&tier, &[DEEP as u64])
            .expect("the hand tier answers");
        let expected = session
            .call(&NothingCompiled, &[DEEP as u64])
            .expect("the vm answers");
        assert_eq!(expected, vec![DEEP as u64]);
        assert_eq!(
            answered, expected,
            "every one of {DEEP} frames returned into the frame below it"
        );
        assert!(
            segments() > 1,
            "the stack did not reallocate under {DEEP} frames, so this case did not test what it \
             is for"
        );
        assert_eq!(
            session.tiers().native(),
            DEEP as u64 + 1,
            "every frame of the recursion was native"
        );
        let seen = witness();
        assert_eq!(seen.len(), DEEP as usize, "one call per frame but the last");
        for (at, seen) in seen.iter().enumerate() {
            assert_eq!(
                seen.outcome,
                Outcome::Returned.abi(),
                "frame {at} returned rather than left"
            );
        }
    });
}

/// Every component the decomposition does twice answers what doing it once
/// answers.
///
/// Issue #365's Part 1 attributes the per-call cost by ablation, and the ablation
/// adds work rather than removing it: a variant of the call helper does one
/// component of the path a second time and the difference is that component. The
/// whole method rests on one claim — that doing a component twice computes the
/// same program — and this is where that claim is checked rather than asserted.
///
/// Every bit at once, which is the strongest form of it: a second span, a second
/// admission, a second safepoint, a second frame pushed and popped, a second zero
/// fill, a second argument copy, a whole second `open_frame`, a second context, a
/// second truncation, a second republication, the whole mediation again, a second
/// owned run of words at the encoded floor, an extra C-ABI hop, and the census
/// counting all of it.
///
/// The fixtures are the ones that would notice: a multi-word return, a native
/// callee and an encoded one, and three hundred frames of recursion through a
/// reallocation of the stack — because a second `push_frame` that did not undo
/// itself, or a second copy into the wrong slot, would land in exactly those.
#[test]
fn every_component_done_twice_answers_the_same() {
    use cove_runtime::native_ablate as it;
    const ALL: u64 = it::AGAIN_SPAN
        | it::AGAIN_ADMIT
        | it::AGAIN_SAFEPOINT
        | it::AGAIN_PUSH_POP
        | it::AGAIN_ZERO
        | it::AGAIN_ARG_COPY
        | it::AGAIN_ARG_LOOKUP
        | it::AGAIN_OPEN_FRAME
        | it::AGAIN_FRAMES
        | it::AGAIN_CTX
        | it::AGAIN_POP
        | it::AGAIN_REPUBLISH
        | it::AGAIN_MEDIATION
        | it::AGAIN_FLOOR_VEC
        | it::AGAIN_HOP
        | it::CENSUS;

    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        cove_runtime::census_reset();
        let mut made = 0u64;
        // `passesPair` is two words over a native callee; `passesEcho` is a
        // reference; `refThroughCollection` reaches a callee the subset refuses,
        // so it is the encoded floor and its owned `Vec`.
        for (entry, names, arguments) in [
            (
                "passesPair",
                &["passesPair", "makesPair"][..],
                vec![Value::int(41)],
            ),
            ("passesPair", &["passesPair"][..], vec![Value::int(41)]),
            (
                "passesEcho",
                &["passesEcho", "echoes"][..],
                vec![Value::string("abc"), Value::int(3)],
            ),
            (
                "refThroughCollection",
                &["refThroughCollection", "echoes"][..],
                vec![
                    Value::string("a string with more than a few bytes in it"),
                    Value::int(4),
                ],
            ),
        ] {
            let mut session = vm
                .native_session(MODULE, entry, arguments)
                .expect("the session opens");
            let arguments: Vec<u64> = session.arguments().to_vec();
            let expected = session
                .call(&NothingCompiled, &arguments)
                .expect("the vm answers");
            let production = hand(lowered, names);
            let once = session
                .call(&production, &arguments)
                .expect("the production helper answers");
            let twice = hand_with(
                lowered,
                names,
                cove_runtime::native_helpers_ablated::<ALL>(),
            );
            let again = session
                .call(&twice, &arguments)
                .expect("the ablated helper answers");
            assert_eq!(once, expected, "`{entry}` on the production helper");
            assert_eq!(again, expected, "`{entry}` with every component done twice");
            made += 2;
        }

        // Three hundred frames, and the hand tier first for the reason
        // `a_return_finds_a_destination_a_reallocation_moved` gives: a `Vec` keeps
        // its capacity, so the VM going first would leave nothing to reallocate.
        const DEEP: i64 = 300;
        let mut session = vm
            .native_session(MODULE, "counts", vec![Value::int(DEEP)])
            .expect("the session opens");
        let twice = hand_with(
            lowered,
            &["counts"],
            cove_runtime::native_helpers_ablated::<ALL>(),
        );
        let again = session
            .call(&twice, &[DEEP as u64])
            .expect("the ablated helper answers");
        let expected = session
            .call(&NothingCompiled, &[DEEP as u64])
            .expect("the vm answers");
        assert_eq!(expected, vec![DEEP as u64]);
        assert_eq!(
            again, expected,
            "{DEEP} frames of recursion, every component of every call done twice"
        );
        assert!(
            segments() > 1,
            "the stack did not reallocate, so this case did not test the case indices exist for"
        );
        made += DEEP as u64;

        // The census counted what it saw. The counters are one per process rather
        // than one per case, so what is assertable here is that they moved and
        // that nothing stopped — a stop would mean a duplicated safepoint had
        // swallowed one, which is the one thing that would make the figures in the
        // report unreadable.
        let census = cove_runtime::census_taken();
        assert!(
            census.calls >= made,
            "the census counted {} call(s) and at least {made} were made",
            census.calls
        );
        assert_eq!(census.stops, 0, "nothing stopped, so nothing was swallowed");
        assert!(
            census.frame_words >= census.param_words,
            "a frame is at least as wide as the parameters written into it"
        );
        cove_runtime::census_reset();
    });
}

/// A direct call answers what a mediated one answers, on every shape of return
/// this file has.
///
/// Issue #365's Part 2. A direct call is the same call: the same frame, the same
/// arguments, the same destination, the same answer. The way to say that is to
/// make each call three times — once on the VM, once through the mediated helper,
/// once through `open`, the callee's entry and `close` — and compare all three.
///
/// The shapes are the ones that would notice something: two words, a reference,
/// no words at all, a callee the subset refused (so the direct path's
/// `entry: None` branch is taken and the mediated helper finishes the call), and
/// a raise travelling out through a frame the direct path did not pop.
#[test]
fn a_direct_call_answers_what_a_mediated_one_answers() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        for (entry, names, arguments) in [
            (
                "passesPair",
                &["passesPair", "makesPair"][..],
                vec![Value::int(41)],
            ),
            // Only the caller compiled, so the callee is reached the way a mixed
            // call always was: `open` answers `entry: None` and the mediated
            // helper has already finished the call.
            ("passesPair", &["passesPair"][..], vec![Value::int(41)]),
            (
                "passesEmpty",
                &["passesEmpty", "makesEmpty"][..],
                vec![Value::int(5)],
            ),
            (
                "passesEcho",
                &["passesEcho", "echoes"][..],
                vec![Value::string("a string"), Value::int(0)],
            ),
            ("counts", &["counts"][..], vec![Value::int(7)]),
        ] {
            let mut session = vm
                .native_session(MODULE, entry, arguments)
                .expect("the session opens");
            let words = session.arguments().to_vec();
            let expected = session
                .call(&NothingCompiled, &words)
                .expect("the vm answers");
            let tier = hand(lowered, names);
            let mediated = session.call(&tier, &words).expect("the mediated helper");
            let tier = hand(lowered, names);
            let direct = directly(|| session.call(&tier, &words)).expect("the direct call");
            assert_eq!(mediated, expected, "`{entry}`, mediated");
            assert_eq!(direct, expected, "`{entry}`, direct");
        }

        // A raise, which is the one outcome that leaves the callee's frame
        // standing: the error's call chain is read out of it.
        let mut session = vm
            .native_session(MODULE, "passesRefusal", vec![Value::int(0)])
            .expect("the session opens");
        let refused = session
            .call(&NothingCompiled, &[0])
            .expect_err("dividing by zero is refused");
        let tier = hand(lowered, &["passesRefusal", "refuses"]);
        let direct = directly(|| session.call(&tier, &[0])).expect_err("the direct call refuses");
        assert_eq!(
            direct.message, refused.message,
            "the sentence a direct call raises is the sentence the VM raises"
        );
        // And the session still works, which is what says the frames the raise
        // left standing were put back.
        let after = directly(|| session.call(&tier, &[4])).expect("a divisor that works");
        assert_eq!(after, vec![25]);
    });
}

/// Recursion through direct calls, returning through a reallocation of the
/// stack.
///
/// `a_return_finds_a_destination_a_reallocation_moved`'s case, made the direct
/// way. It is the one that would catch a frame index turned into a pointer
/// anywhere in the new path — `open` answers an index, emitted code stores the
/// arguments through it, and the entry is given it — because 300 frames is
/// several `Vec::resize`s and every destination below is pending across them.
#[test]
fn a_direct_chain_returns_through_a_reallocation() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        const DEEP: i64 = 300;
        let mut session = vm
            .native_session(MODULE, "counts", vec![Value::int(DEEP)])
            .expect("the session opens");
        // The hand tier first, for the reason the mediated case gives: a `Vec`
        // keeps its capacity, so a VM run of the same depth would leave nothing
        // to reallocate.
        let tier = hand(lowered, &["counts"]);
        let answered = directly(|| session.call(&tier, &[DEEP as u64])).expect("the direct chain");
        let expected = session
            .call(&NothingCompiled, &[DEEP as u64])
            .expect("the vm answers");
        assert_eq!(expected, vec![DEEP as u64]);
        assert_eq!(answered, expected, "{DEEP} direct frames answered");
        assert!(
            segments() > 1,
            "the stack did not reallocate under {DEEP} direct frames, so this case did not test \
             what it is for"
        );
        assert_eq!(
            session.tiers().native(),
            DEEP as u64 + 1,
            "every frame of the recursion was entered directly bar the outermost"
        );
    });
}

/// A runaway recursion is refused rather than left to run, and the direct path is
/// where it is refused.
///
/// What catches it is `admit_frame`, against the embedder's
/// [`Limits::max_call_depth`], and then `push_frame`'s `Overflow` against the
/// task's stack segment — both of them, in that order, inside `open`, which is
/// the same two checks in the same order the mediated helper makes inside
/// `open_frame`. Nothing about a direct call skips either: the frame is still a
/// `Vec::resize` the runtime performs, and the depth is still counted in the
/// frames the runtime holds.
///
/// The bound this asserts is the configured one, and that is a choice about what
/// can be tested rather than about what exists. A recursion deep enough to
/// exhaust a *segment* — a million words, and `counts` needs six of them a frame
/// — has by then nested a hundred and seventy thousand machine frames of helper
/// and entry, which exhausts the thread's own stack first. That is true of the
/// mediated path too and is not this change's: it is the reason an embedder is
/// given `max_call_depth` at all.
#[test]
fn a_runaway_recursion_is_still_refused() {
    const LIMIT: usize = 64;
    let limits = Limits {
        max_call_depth: Some(LIMIT),
        ..Limits::default()
    };
    with_limited_vm(ORDINARY_HEAP_WORDS, limits, |vm, lowered| {
        const RUNAWAY: i64 = 100_000;
        let mut session = vm
            .native_session(MODULE, "counts", vec![Value::int(RUNAWAY)])
            .expect("the session opens");
        let refused = session
            .call(&NothingCompiled, &[RUNAWAY as u64])
            .expect_err("the vm refuses a recursion past the limit");
        let tier = hand(lowered, &["counts"]);
        let direct = directly(|| session.call(&tier, &[RUNAWAY as u64]))
            .expect_err("a direct chain past the limit is refused too");
        assert!(
            refused.message.contains(&LIMIT.to_string()),
            "the VM's refusal names the limit: {}",
            refused.message
        );
        assert_eq!(
            direct.message, refused.message,
            "a direct call is refused by the same check with the same sentence"
        );
        // The process is still here and so is the session, which is the other
        // half of "refused rather than crashing".
        let answered = directly(|| session.call(&tier, &[10])).expect("a depth inside the limit");
        assert_eq!(answered, vec![10]);
    });
}

/// A collection during a direct chain, with references live in several frames.
///
/// `a_returned_reference_survives_a_collection`'s case, made the direct way, and
/// the thing it puts at risk is new: a direct call hands the callee the
/// *caller's* context and stores the arguments through the index `open` answered,
/// so a reference that was live in three frames at once has to still be in the
/// slot `Function::refs` names when the allocation inside `allocates` collects.
/// If it were anywhere else — a register, a stale pointer, a word above the frame
/// — the string would be swept and the byte read would answer something else.
#[test]
fn a_collection_during_a_direct_chain_keeps_every_reference() {
    const SMALL_HEAP_WORDS: usize = 1 << 13;
    with_vm(SMALL_HEAP_WORDS, |vm, lowered| {
        let text = "a string long enough that slicing it fills a heap chunk, and long enough \
                    that a byte can be read out of the middle of it without asking whether it \
                    is there: sixty-four bytes in is well inside this sentence.";
        const AT: i64 = 64;
        let mut session = vm
            .native_session(
                MODULE,
                "refThroughCollection",
                vec![Value::string(text), Value::int(AT)],
            )
            .expect("the session opens");
        let words = session.arguments().to_vec();
        let expected = session
            .call(&NothingCompiled, &words)
            .expect("the vm answers");
        assert_eq!(expected, vec![u64::from(text.as_bytes()[AT as usize])]);

        let tier = hand(lowered, &["refThroughCollection", "echoes"]);
        let before = session.collections();
        let mut calls = 0;
        while session.collections() == before && calls < 20_000 {
            let answered = directly(|| session.call(&tier, &words)).expect("the direct chain");
            assert_eq!(answered, expected, "direct call {calls} answered wrongly");
            calls += 1;
        }
        assert!(
            session.collections() > before,
            "no collection ran in {calls} direct call(s), so this case proved nothing"
        );
    });
}

/// A chain that crosses the tiers: native to native to encoded, and the answer
/// comes back through all of it.
///
/// `refThroughCollection` is entered directly, calls `echoes` directly, and calls
/// `allocates` — which the subset refuses, because it reaches `String.slice`
/// — so that call is the mediated helper and the encoded dispatch loop. Both
/// callees answer into destinations in frames the direct path opened.
///
/// The fourth hop — **encoded back into native** — is not in *this* chain, and
/// that is a property of the tier the case installs rather than of the boundary:
/// `allocates` calls nothing the table compiles. `all_four_transitions_are_taken_and_counted`
/// is the case that takes all four, and `a_vm_caller_enters_a_compiled_callee`
/// the one that isolates this one. So the counts below say which hop was which
/// and no more than that.
#[test]
fn a_mixed_chain_crosses_the_tiers_and_answers() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        let text = "a string with rather more than sixty-four bytes in it, so that a byte can \
                    be read out of the middle without asking whether it is there at all.";
        const AT: i64 = 16;
        let mut session = vm
            .native_session(
                MODULE,
                "refThroughCollection",
                vec![Value::string(text), Value::int(AT)],
            )
            .expect("the session opens");
        let words = session.arguments().to_vec();
        let expected = session
            .call(&NothingCompiled, &words)
            .expect("the vm answers");
        let before = session.tiers();
        let tier = hand(lowered, &["refThroughCollection", "echoes"]);
        let answered = directly(|| session.call(&tier, &words)).expect("the mixed chain");
        assert_eq!(answered, expected, "the chain answers what the VM answers");
        let tiers = session.tiers();
        assert!(
            tiers.native() - before.native() >= 2,
            "the caller and `echoes` were both entered natively"
        );
        assert!(
            tiers.encoded() - before.encoded() >= 1,
            "at least one callee was run by the encoded tier"
        );
    });
}

/// **A VM caller enters native code**, which is the transition issue #369 exists
/// for.
///
/// The caller is refused and the callee is compiled — the shape that made
/// coverage non-compositional before this. The encoded dispatch loop's `CALL` arm
/// now asks the same tier table the call helper asks, so `makesPair` is entered as
/// a compiled function from inside a function the VM is running.
///
/// It is asserted three ways, because two of them would each pass on their own for
/// the wrong reason: the answer matches the VM's, the `vm_to_native` counter
/// moved, and the hand-written entry was actually *entered* — a counter that
/// incremented without the code running would be the worst of the three failures.
#[test]
fn a_vm_caller_enters_a_compiled_callee() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        let mut session = vm
            .native_session(MODULE, "passesPair", vec![Value::int(3)])
            .expect("the session opens");
        let expected = session
            .call(&NothingCompiled, &[3])
            .expect("the vm answers");
        let before = session.tiers();
        // The callee is compiled and the caller is not, so the only way into the
        // compiled code is the dispatch loop choosing it.
        let tier = hand(lowered, &["makesPair"]);
        let answered = session.call(&tier, &[3]).expect("the mixed call answers");
        assert_eq!(answered, expected);
        assert_eq!(answered, vec![3, 4]);
        let tiers = session.tiers();
        assert_eq!(
            tiers.host_to_vm - before.host_to_vm,
            1,
            "the outermost frame was refused, so the VM ran it"
        );
        assert_eq!(
            tiers.vm_to_native - before.vm_to_native,
            1,
            "the encoded `CALL` arm entered the one compiled callee"
        );
        assert_eq!(
            entries_into(lowered, "makesPair"),
            1,
            "the compiled entry ran, rather than a counter moving on its own"
        );
        assert_eq!(
            entries_into(lowered, "passesPair"),
            0,
            "and the caller it was called from was the VM's"
        );
    });
}

/// The four transitions, in one chain, each counted once.
///
/// Issue #369's table has four rows and this is the case that moves all four at
/// once, which is a stronger statement than four cases moving one each: a chain
/// that crosses back and forth is where a boundary that only works in one
/// direction shows.
///
/// `refThroughCollection` calls `echoes`, `allocates` and `firstByte`. The tier
/// below compiles `echoes` and `held` and leaves the other two to the VM —
/// `allocates` reaches `String.sliceBytes`, which nothing native lowers, and
/// `firstByte` is an `Inst::RunLoad` this file's tier does not walk. So each of the
/// four hops is taken by a named call and the counters can be read one by one.
#[test]
fn all_four_transitions_are_taken_and_counted() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        let text = "a string with rather more than sixty-four bytes in it, so that a byte can \
                    be read out of the middle without asking whether it is there at all.";
        const AT: i64 = 16;
        let mut session = vm
            .native_session(
                MODULE,
                "refThroughCollection",
                vec![Value::string(text), Value::int(AT)],
            )
            .expect("the session opens");
        let words = session.arguments().to_vec();
        let expected = session
            .call(&NothingCompiled, &words)
            .expect("the vm answers");
        let before = session.tiers();
        // `refThroughCollection` is *not* compiled, so the outermost frame is the
        // VM's and every crossing below it is a Cove call rather than an entry
        // from Rust. That is what makes `vm_to_native` the hop under test.
        let tier = hand(lowered, &["echoes", "held"]);
        let answered = directly(|| session.call(&tier, &words)).expect("the mixed chain");
        assert_eq!(answered, expected, "the chain answers what the VM answers");
        let took = |now: u64, then: u64| now - then;
        let tiers = session.tiers();
        assert_eq!(
            took(tiers.host_to_vm, before.host_to_vm),
            1,
            "the outermost frame was refused"
        );
        assert!(
            took(tiers.vm_to_vm, before.vm_to_vm) >= 1,
            "VM to VM: `allocates` has no compiled entry and the VM called it"
        );
        assert!(
            took(tiers.vm_to_native, before.vm_to_native) >= 1,
            "VM to native: the encoded caller entered the compiled `echoes`"
        );
        assert!(
            took(tiers.native_to_vm, before.native_to_vm) >= 1,
            "native to VM: `held` calls `counts`, which is not compiled"
        );
        assert!(
            took(
                tiers.native_to_native_direct,
                before.native_to_native_direct
            ) >= 1,
            "native to native, direct: `echoes` calls the compiled `held`"
        );
    });
}

/// The same chain with the mediated call protocol, which is the fifth counter.
///
/// `direct` is off, so a compiled callee reached from compiled code goes through
/// the one call helper. The counters have to say which protocol ran, because a
/// reader who could not tell them apart would read a mediated run as if PR #368's
/// direct calls were in it.
#[test]
fn a_mediated_native_to_native_call_is_counted_apart() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        let mut session = vm
            .native_session(MODULE, "passesPair", vec![Value::int(11)])
            .expect("the session opens");
        let expected = session
            .call(&NothingCompiled, &[11])
            .expect("the vm answers");
        let before = session.tiers();
        let tier = hand(lowered, &["passesPair", "makesPair"]);
        let answered = session.call(&tier, &[11]).expect("the hand tier answers");
        assert_eq!(answered, expected);
        let tiers = session.tiers();
        assert_eq!(
            took_all(before, tiers).native_to_native_direct,
            0,
            "nothing took the direct protocol"
        );
        assert_eq!(
            took_all(before, tiers).native_to_native_mediated,
            1,
            "the compiled caller reached the compiled callee through the helper"
        );
    });
}

/// The difference between two readings, field by field.
fn took_all(before: cove_runtime::Tiers, after: cove_runtime::Tiers) -> cove_runtime::Tiers {
    cove_runtime::Tiers {
        vm_to_vm: after.vm_to_vm - before.vm_to_vm,
        vm_to_native: after.vm_to_native - before.vm_to_native,
        native_to_vm: after.native_to_vm - before.native_to_vm,
        native_to_native_direct: after.native_to_native_direct - before.native_to_native_direct,
        native_to_native_mediated: after.native_to_native_mediated
            - before.native_to_native_mediated,
        host_to_native: after.host_to_native - before.host_to_native,
        host_to_vm: after.host_to_vm - before.host_to_vm,
    }
}

/// Every return shape, crossed by a **VM-to-native** call.
///
/// The cases above test each shape with a compiled caller, which is where ADR
/// 0057's destination was written and measured. This is the same four shapes with
/// the caller on the *encoded* tier, because the destination is then a frame the
/// dispatch loop is standing in and the hand-over is the one issue #369 added:
/// `open_frame` settled the arguments, `Inst::Call`'s `dst` settled the
/// destination, and compiled code writes it before its frame comes off.
///
/// Each row names a wrapper that is deliberately **not** compiled and the callee
/// that is, and every answer is compared against the encoded VM's for the same
/// call. A destination one word short, one word wide or one slot over is a
/// different answer.
#[test]
fn every_return_shape_crosses_a_vm_to_native_call() {
    // (the encoded caller, the compiled callee, the argument, what it answers)
    let rows: [(&str, &str, i64, Vec<u64>); 4] = [
        // Two words, inline: the multi-word return.
        ("passesPair", "makesPair", 41, vec![41, 42]),
        // No words at all, and a caller that goes on to use a slot afterwards.
        ("passesEmpty", "makesEmpty", 5, vec![105]),
        // One word, and the word is a heap address rather than the object at it.
        ("passesEcho", "echoes", 0, Vec::new()),
        // One scalar word, through a recursion the callee makes itself.
        ("callsCounts", "counts", 40, vec![40]),
    ];
    for (caller, callee, argument, answers) in rows {
        with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
            let reference = caller == "passesEcho";
            let arguments = if reference {
                vec![Value::string("a string"), Value::int(argument)]
            } else {
                vec![Value::int(argument)]
            };
            let mut session = vm
                .native_session(MODULE, caller, arguments)
                .expect("the session opens");
            let words = session.arguments().to_vec();
            let expected = session
                .call(&NothingCompiled, &words)
                .expect("the vm answers");
            if !answers.is_empty() {
                assert_eq!(expected, answers, "`{caller}` answers what this case says");
            }
            let before = session.tiers();
            let tier = hand(lowered, &[callee]);
            let answered = session
                .call(&tier, &words)
                .unwrap_or_else(|error| panic!("`{caller}` -> `{callee}`: {}", error.message));
            assert_eq!(
                answered, expected,
                "`{caller}` -> `{callee}` answered something other than the VM's answer"
            );
            let tiers = session.tiers();
            assert!(
                tiers.vm_to_native - before.vm_to_native >= 1,
                "`{caller}` -> `{callee}` did not take the VM-to-native hop"
            );
            assert_eq!(
                tiers.host_to_vm - before.host_to_vm,
                1,
                "`{caller}` was run by the VM, which is what makes the hop that one"
            );
            assert!(
                entries_into(lowered, callee) >= 1,
                "`{callee}`'s compiled entry ran"
            );
            assert_eq!(
                entries_into(lowered, caller),
                0,
                "`{caller}`'s did not, because it has none"
            );
        });
    }
}

/// A raise crosses a VM-to-native call with the sentence the VM would have
/// produced, and publishes nothing.
///
/// Two halves, and the second is the one a boundary gets wrong. The message has to
/// be the *encoded tier's* — `cove-native` names the operation and `cove-runtime`
/// writes the sentence, so a native `/` by zero says what a dispatched one says —
/// and the caller's destination has to be untouched, because the answer is
/// published only on the return path.
#[test]
fn a_raise_crosses_a_vm_to_native_call() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        let mut session = vm
            .native_session(MODULE, "passesRefusal", vec![Value::int(0)])
            .expect("the session opens");
        let refused = session
            .call(&NothingCompiled, &[0])
            .expect_err("the vm refuses a zero divisor");
        // Only the callee is compiled, so the raise leaves compiled code and is
        // built by the runtime for an encoded caller to carry.
        let tier = hand(lowered, &["refuses"]);
        let before = session.tiers();
        let crossed = session
            .call(&tier, &[0])
            .expect_err("the compiled callee refuses it too");
        assert_eq!(
            crossed.message, refused.message,
            "a native raise crossing into the VM is the same sentence"
        );
        assert_eq!(
            crossed.span, refused.span,
            "and it points at the same instruction"
        );
        // The chain is read out of the frames the failure left standing, and it is
        // asserted because getting it wrong is invisible in the message: syncing
        // the caller's pc on the way out would overwrite the *callee's* frame and
        // report the call site as the place the division failed. It did, once.
        assert_eq!(
            crossed.chain(),
            refused.chain(),
            "and the call chain it left behind is the VM's"
        );
        assert_eq!(
            session.tiers().vm_to_native - before.vm_to_native,
            1,
            "the encoded caller entered the compiled callee before it raised"
        );
        // The session is still usable, which is what says the failure put the
        // stack back rather than leaving a frame standing on it.
        let answered = session.call(&tier, &[4]).expect("a divisor that works");
        assert_eq!(answered, vec![25]);
    });
}

/// Exhausted fuel stops a run with a VM-to-native call in flight, and stops it
/// where the VM stops it.
///
/// [ADR 0040] makes fuel backend-specific — a native run does not promise the same
/// `fuel_spent` — and promises the same *stop outcome*. So this asserts the
/// outcome and the sentence and deliberately not the number: the safepoint a
/// native call takes is `Machine::safepoint`, the same three steps in the same
/// order, and what differs is how much work had accumulated before it.
///
/// [ADR 0040]: ../../../docs/adr/0040-a-bound-outlives-its-backend.md
#[test]
fn fuel_runs_out_under_a_vm_to_native_call() {
    const DEEP: i64 = 20_000;
    let limits = Limits {
        fuel: Some(2_000),
        ..Limits::default()
    };
    with_limited_vm(ORDINARY_HEAP_WORDS, limits, |vm, lowered| {
        let mut session = vm
            .native_session(MODULE, "callsCounts", vec![Value::int(DEEP)])
            .expect("the session opens");
        let stopped = session
            .call(&NothingCompiled, &[DEEP as u64])
            .expect_err("the vm runs out of fuel");
        let tier = hand(lowered, &["counts"]);
        let crossed = session
            .call(&tier, &[DEEP as u64])
            .expect_err("and so does a run that crossed into compiled code");
        assert_eq!(
            crossed.outcome, stopped.outcome,
            "the same terminal outcome, which is what ADR 0040 promises across tiers"
        );
        assert!(
            crossed.message.contains("fuel"),
            "and it says what stopped it: {}",
            crossed.message
        );
        assert!(
            session.tiers().vm_to_native >= 1,
            "the encoded caller had entered compiled code before the stop"
        );
    });
}

/// Cancellation stops a run with a VM-to-native call in flight.
///
/// The first of ADR 0040's three safepoint steps, taken by the same
/// `Machine::safepoint` an encoded run takes it with — cancellation is checked
/// before fuel and before the collector rendezvous, in compiled code as in
/// dispatched code, because neither of the three is emitted.
#[test]
fn cancellation_stops_a_vm_to_native_call() {
    const DEEP: i64 = 200_000;
    with_bounded_vm(
        ORDINARY_HEAP_WORDS,
        Limits::default(),
        |vm, lowered, cancellation| {
            let mut session = vm
                .native_session(MODULE, "callsCounts", vec![Value::int(DEEP)])
                .expect("the session opens");
            cancellation.cancel();
            let tier = hand(lowered, &["counts"]);
            let stopped = session
                .call(&tier, &[DEEP as u64])
                .expect_err("a cancelled run stops");
            assert!(
                stopped.message.contains("cancel"),
                "and says it was cancelled: {}",
                stopped.message
            );
        },
    );
}

/// A recursion past the configured call depth is refused on the VM-to-native path
/// too, by the same two checks in the same order.
///
/// `admit_frame` against the embedder's `max_call_depth`, and then `push_frame`'s
/// `Overflow` against the task's stack segment. Both are inside the *same*
/// `open_frame` the dispatch loop calls, because a VM-to-native call opens its
/// frame the way every other encoded call does and only then asks which tier runs
/// it — which is the whole reason this hop needed no new admission code.
#[test]
fn a_vm_to_native_recursion_is_refused_at_the_configured_depth() {
    const LIMIT: usize = 64;
    let limits = Limits {
        max_call_depth: Some(LIMIT),
        ..Limits::default()
    };
    with_limited_vm(ORDINARY_HEAP_WORDS, limits, |vm, lowered| {
        const RUNAWAY: i64 = 100_000;
        let mut session = vm
            .native_session(MODULE, "callsCounts", vec![Value::int(RUNAWAY)])
            .expect("the session opens");
        let refused = session
            .call(&NothingCompiled, &[RUNAWAY as u64])
            .expect_err("the vm refuses a recursion past the limit");
        let tier = hand(lowered, &["counts"]);
        let crossed = session
            .call(&tier, &[RUNAWAY as u64])
            .expect_err("a chain that crossed into compiled code is refused too");
        assert!(
            refused.message.contains(&LIMIT.to_string()),
            "the VM's refusal names the limit: {}",
            refused.message
        );
        assert_eq!(
            crossed.message, refused.message,
            "and the crossing is refused by the same check with the same sentence"
        );
        // Still usable, which is the other half of "refused rather than crashed".
        let answered = session
            .call(&tier, &[10])
            .expect("a depth inside the limit");
        assert_eq!(answered, vec![10]);
    });
}

/// A destination pending across a reallocation of the stack, with the caller on
/// the VM.
///
/// `a_return_finds_a_destination_a_reallocation_moved`'s case with the outermost
/// frame encoded, which puts a *dispatch loop's* frame under three hundred native
/// ones. The dispatch loop caches `base_at` — a word *index* — across the call for
/// exactly this reason, and a native chain deep enough to `Vec::resize` the stack
/// is what says the index survived what a pointer would not have.
#[test]
fn a_vm_destination_survives_a_reallocation_under_a_native_chain() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        const DEEP: i64 = 300;
        let mut session = vm
            .native_session(MODULE, "callsCounts", vec![Value::int(DEEP)])
            .expect("the session opens");
        // The compiled arm goes first, for the reason the direct case gives: a
        // `Vec` keeps its capacity, so a VM run of three hundred frames would
        // leave the stack large enough that nothing reallocates afterwards and
        // this case would pass while testing nothing.
        let tier = hand(lowered, &["counts"]);
        let answered = session
            .call(&tier, &[DEEP as u64])
            .expect("the crossing answers");
        let expected = session
            .call(&NothingCompiled, &[DEEP as u64])
            .expect("the vm answers");
        assert_eq!(expected, vec![DEEP as u64]);
        assert_eq!(answered, expected);
        assert!(
            segments() > 1,
            "the stack did not reallocate under {DEEP} frames, so this case did not test what it \
             is for"
        );
    });
}

/// A forced collection with references live in **both** the encoded caller and the
/// compiled callee.
///
/// ADR 0055's "Collection uses the VM stack as the first root map", across the hop
/// this issue added. The caller is a dispatch-loop frame holding a `String` in a
/// `Repr::Ref` slot; the callee is compiled and holds the same reference in a slot
/// of its own; and the allocation inside `allocates` — which runs on the VM,
/// because `String.sliceBytes` is outside anything native lowers — is what
/// collects while both are live. If either frame's reference were anywhere but the
/// slot `Function::refs` names, the string would be swept and the byte read would
/// answer something else.
#[test]
fn a_collection_keeps_references_live_in_both_an_encoded_caller_and_a_compiled_callee() {
    const SMALL_HEAP_WORDS: usize = 1 << 13;
    with_vm(SMALL_HEAP_WORDS, |vm, lowered| {
        let text = "a string long enough that slicing it fills a heap chunk, and long enough \
                    that a byte can be read out of the middle of it without asking whether it \
                    is there: sixty-four bytes in is well inside this sentence.";
        const AT: i64 = 64;
        let mut session = vm
            .native_session(
                MODULE,
                "refThroughCollection",
                vec![Value::string(text), Value::int(AT)],
            )
            .expect("the session opens");
        let words = session.arguments().to_vec();
        let expected = session
            .call(&NothingCompiled, &words)
            .expect("the vm answers");
        assert_eq!(expected, vec![u64::from(text.as_bytes()[AT as usize])]);

        // `refThroughCollection` is *not* compiled, so it is the encoded caller
        // and `echoes` is the compiled callee that holds the same reference.
        let tier = hand(lowered, &["echoes"]);
        let before = session.collections();
        let mut calls = 0;
        while session.collections() == before && calls < 20_000 {
            let answered = session.call(&tier, &words).expect("the mixed chain");
            assert_eq!(answered, expected, "crossing {calls} answered wrongly");
            calls += 1;
        }
        assert!(
            session.collections() > before,
            "no collection happened in {calls} calls, so this case tested nothing"
        );
        assert!(
            session.tiers().vm_to_native >= calls as u64,
            "at least one VM-to-native crossing per call"
        );
    });
}

/// One finalized table, many calls, and the table is never rebuilt.
///
/// Issue #369: a VM-to-native call must "not recursively rebuild or refinalize the
/// JIT". This file's tier emits nothing, so what it can say about *finalizing* is
/// nothing — what it can say, and what the property reduces to for a caller, is
/// that the table is consulted rather than constructed: the same `Tiered` is handed
/// to a thousand calls, the answers are all the VM's, and the counters grow by one
/// crossing a call and not by more.
#[test]
fn one_table_serves_repeated_calls() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        const CALLS: u64 = 1_000;
        let mut session = vm
            .native_session(MODULE, "passesPair", vec![Value::int(1)])
            .expect("the session opens");
        let tier = hand(lowered, &["makesPair"]);
        let before = session.tiers();
        for at in 0..CALLS {
            let answered = session.call(&tier, &[at]).expect("every call answers");
            assert_eq!(answered, vec![at, at + 1], "call {at}");
        }
        let took = took_all(before, session.tiers());
        assert_eq!(
            took.vm_to_native, CALLS,
            "one crossing a call, through one table"
        );
        assert_eq!(
            took.host_to_vm, CALLS,
            "and one encoded outermost frame a call"
        );
        assert_eq!(
            entries_into(lowered, "makesPair") as u64,
            CALLS,
            "the compiled entry ran once a call"
        );
    });
}

/// **A `var` parameter written through by the native tier, read back by its VM
/// caller.**
///
/// The word the callee is handed is a linear address of a slot of the *caller's*
/// frame, and the two tiers have to mean the same thing by it: the caller formed
/// it with `encoded.rs`'s `ADDR_OF_SLOT` arm and the callee follows it with the
/// address decode the ABI describes. Both halves of the fixture's answer are
/// asserted — the caller's own slot and what the callee said — so a tier that
/// wrote the right number somewhere else cannot pass on the second alone.
#[test]
fn a_var_parameter_is_written_through_by_the_native_tier() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        let mut session = vm
            .native_session(MODULE, "lends", vec![Value::int(20)])
            .expect("the session opens");
        let words = session.arguments().to_vec();
        let expected = session
            .call(&NothingCompiled, &words)
            .expect("the vm answers");
        assert_eq!(
            expected,
            vec![25 * 1000 + 25],
            "the caller's slot holds `a + 5` and so does what the callee answered"
        );

        // Only the callee, so the `addr-of-slot` is the encoded tier's and the
        // `load` and `store` through it are this one's.
        let tier = hand(lowered, &["bumps"]);
        assert_eq!(
            session.call(&tier, &words).expect("the hand tier answers"),
            expected
        );
        assert_eq!(
            entries_into(lowered, "bumps"),
            1,
            "and the callee really was the tier's"
        );

        // And with both frames on the tier, so the address is formed and followed
        // by the same one.
        let tier = hand(lowered, &["lends", "bumps"]);
        assert_eq!(
            session.call(&tier, &words).expect("the hand tier answers"),
            expected
        );
    });
}

/// One address, carried down three hundred frames while the stack's `Vec`
/// reallocates under every one of them.
///
/// [`segments`] is what says the reallocation happened: it counts the distinct
/// `NativeCtx::words` the tier was handed, and more than one means the buffer
/// moved *while native frames were live*. The slot the address names is in the
/// bottom frame, so every write after the first is a write below a stack that has
/// grown — which is the whole reason an address is an index into a segment whose
/// origin is fixed, and not a pointer.
#[test]
fn an_address_survives_a_reallocation_under_a_deep_chain() {
    const DEEP: u64 = 300;
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        let mut session = vm
            .native_session(MODULE, "threadsDeeply", vec![Value::int(DEEP as i64)])
            .expect("the session opens");
        let words = session.arguments().to_vec();
        // The hand tier goes *first*, which is `a_return_finds_a_destination_a_
        // reallocation_moved`'s reason and the whole of what makes this case work:
        // a `Vec` keeps its capacity across `Vec::clear`, so a 300-frame run on
        // the VM would leave the stack large enough that the next run never
        // reallocates — and the case would pass while testing nothing.
        let tier = hand(lowered, &["threadsDeeply", "lendsDeeply"]);
        let answered = session.call(&tier, &words).expect("the hand tier answers");
        let expected = session
            .call(&NothingCompiled, &words)
            .expect("the vm answers");
        assert_eq!(
            expected,
            vec![DEEP * 1000 + DEEP],
            "every step added one to the same word"
        );
        assert_eq!(answered, expected);
        assert!(
            segments() > 1,
            "the stack's `Vec` did not move, so this case proved nothing"
        );
    });
}
