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
    native_helpers, Grants, HostRegistry, NativeEntry, NothingCompiled, Runtime, Tiered, Value, Vm,
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
/// `sliceBytes` is outside anything a native tier lowers — which is what makes
/// it a native-to-VM call besides.
export fn allocates(s: String, n: Int) -> Int {
  match s.sliceBytes(held(0), n) {
    Ok(cut) => cut.byteLength()
    Err(_) => 0
  }
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
    assert!(
        names.len() <= ENTRIES.len(),
        "there are {} entry points and this case asked for {}",
        ENTRIES.len(),
        names.len()
    );
    PROGRAM.with(|held| *held.borrow_mut() = Some(Arc::clone(program)));
    HELPERS.with(|held| held.set(Some(native_helpers())));
    SEGMENTS.with(|seen| seen.borrow_mut().clear());
    WITNESS.with(|seen| seen.borrow_mut().clear());
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
                let outcome = (helpers.call)(ctx, base, pc as u32, callee.0, args.0, *dst);

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
            session.tiers().native >= 2,
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
        let answered = session.call(&tier, &[7]).expect("the hand tier answers");
        assert_eq!(answered, expected);
        assert_eq!(answered, vec![7, 8]);
        let tiers = session.tiers();
        assert_eq!(
            (tiers.native, tiers.encoded),
            (1, 2),
            "one native frame, and the vm ran the callee here and the whole of the oracle's call"
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
            session.tiers().native,
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

/// A VM caller does not enter native code, and that is why nothing above tests
/// one.
///
/// The tier table is consulted in exactly two places — the call helper, which is
/// reached from compiled code, and [`NativeSession::call`](cove_runtime::NativeSession::call),
/// which is the boundary a Rust caller comes in through. The encoded dispatch
/// loop's `CALL` arm consults nothing: it opens a frame and keeps dispatching. So
/// "VM to native" as a *Cove* call does not exist in this slice, and the closest
/// thing to it is the session entering the outermost frame, which every case above
/// does.
///
/// This asserts it rather than leaving it to be noticed, because a reader looking
/// for the missing direction should find out why it is missing.
#[test]
fn a_vm_caller_does_not_reach_the_tier_table() {
    with_vm(ORDINARY_HEAP_WORDS, |vm, lowered| {
        let mut session = vm
            .native_session(MODULE, "passesPair", vec![Value::int(3)])
            .expect("the session opens");
        // The callee is compiled and the caller is not, so the only way into the
        // compiled code would be the dispatch loop choosing it.
        let tier = hand(lowered, &["makesPair"]);
        let answered = session.call(&tier, &[3]).expect("the vm answers");
        assert_eq!(answered, vec![3, 4]);
        let tiers = session.tiers();
        assert_eq!(
            (tiers.native, tiers.encoded),
            (0, 1),
            "the whole call ran on the encoded tier, compiled callee and all"
        );
        assert!(
            witness().is_empty(),
            "no hand-written entry was entered, so no call was made through the helper"
        );
    });
}
