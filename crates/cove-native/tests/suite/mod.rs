//! What the scalar and control-flow slice of the native tier actually does,
//! run — once, for whichever code generator is asked for.
//!
//! # One suite, two arms
//!
//! There are two code generators in this crate and the reason for the second is
//! to measure it against the first. So the expectations are written once, over
//! the [`Arm`] trait, and each arm's test file is a list of one-line wrappers:
//! two arms that agreed with two different suites would not have been compared.
//!
//! # Why the expectations are literals
//!
//! These tests cannot compare against `Machine`, because `cove-native` does
//! not depend on `cove-runtime` and must not — see that crate's documentation
//! for the inversion. So the encoded tier's behaviour is written out here as
//! literal expectations, and **every one of them cites the arm of
//! `crates/cove-runtime/src/vm/exec/encoded.rs` it mirrors, by file and
//! line**, so that a reader can check the mirror rather than trust it.
//!
//! The line numbers are as of the commit that added this file. They are a
//! pointer to an arm, not a contract; the arm's *name* is given beside every
//! one of them because a name survives an edit above it.
//!
//! A differential test against the real VM is the right test and it is the
//! next slice's, once the tier is reachable from a `Machine`. Until then this
//! is the strongest thing available, and it is stronger than nothing by
//! exactly the amount of trouble a wrong `+` would cause later.

#![allow(dead_code)]

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use cove_diag::{FileId, Span};
use cove_ir::{
    Arg, ArgsId, ArithOp, BuiltinId, CaseId, CmpOp, Compare, Function, FunctionId, Inst, Layout,
    LayoutId, Num, Program, RefMap, Repr, Slot, StrId, Table, TableId,
};
use cove_native::{Entry, NativeCtx, NativeHelpers, Opened, Outcome, Raise};
use cove_native::{HEAP_CHUNK_WORDS, HEAP_ORIGIN_WORDS};

// --- the safepoint helper -----------------------------------------------------

thread_local! {
    /// Every safepoint this thread's compiled code has taken, as
    /// `(pc, work_since_last)`.
    ///
    /// A thread-local rather than a static, because `cargo test` runs these
    /// in parallel and each test body is one thread. The helper is a plain
    /// `extern "C"` function and therefore cannot close over a recorder, so
    /// the recorder has to be reachable from a bare function — which is what
    /// a thread-local is for.
    pub static POLLS: RefCell<Vec<(u32, u64)>> = const { RefCell::new(Vec::new()) };
    /// How many safepoints to allow before answering "stop".
    pub static POLLS_ALLOWED: Cell<usize> = const { Cell::new(usize::MAX) };
    /// A word of the frame the safepoint helper reads, by index into `words`.
    ///
    /// How the claim in `cove_native::abi` — "at a safepoint every live reference
    /// is already in the slot the frame's static map names" — is *tested* rather
    /// than argued: the helper is the collector's stand-in, and what it can see
    /// is exactly what a root walk can see. If an arm kept a reference only in a
    /// register across the call, the word this reads would be stale or zero.
    pub static WATCHED: Cell<Option<usize>> = const { Cell::new(None) };
    /// What [`WATCHED`] held at each safepoint, in order.
    pub static WATCHED_SAW: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// The runtime's half of the boundary, as a test double.
///
/// A real one is where [ADR 0040]'s three-step order lives — cancellation and
/// task-local stops, then fuel and deadline accounting, then the collector
/// rendezvous. This one records and optionally stops, which is the whole of
/// what compiled code can observe about it.
///
/// [ADR 0040]: ../../../../docs/adr/0040-a-bound-outlives-its-backend.md
unsafe extern "C" fn safepoint(ctx: *mut NativeCtx, pc: u32, work: u64) -> bool {
    POLLS.with(|polls| polls.borrow_mut().push((pc, work)));
    if let Some(at) = WATCHED.with(Cell::get) {
        // Safety: the caller set `at` to an index inside the `words` it handed
        // the entry point.
        let word = (*ctx).words.add(at).read();
        WATCHED_SAW.with(|saw| saw.borrow_mut().push(word));
    }
    let taken = POLLS.with(|polls| polls.borrow().len());
    taken < POLLS_ALLOWED.with(Cell::get)
}

/// One call compiled code made through [`CallFn`](cove_native::CallFn).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Called {
    pub base: u64,
    pub pc: u32,
    pub callee: u32,
    pub args: u32,
    pub dst: u32,
    /// The unpaid work the caller published before handing over.
    ///
    /// A call is a safepoint — a callee may allocate and an allocation may
    /// collect — so the work goes over with it, and this is what says the arm
    /// published it rather than dropping it.
    pub work: u64,
}

thread_local! {
    /// Every call this thread's compiled code has made, in order.
    pub static CALLS: RefCell<Vec<Called>> = const { RefCell::new(Vec::new()) };
    /// What the next call answers, and the one after it: an [`Outcome`] as its
    /// ABI number, taken from the front.
    ///
    /// Empty means "return", which is what a leaf answer is. A script is how a
    /// raise and a stop coming *out of a callee* are tested without a runtime to
    /// raise one.
    pub static CALL_ANSWERS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
}

/// The runtime's call helper, as a test double.
///
/// A real one opens the callee's frame with the runtime's own `open_frame` and
/// runs it on whichever tier it is on. This one records the hand-over and writes
/// one word — `callee * 1000 + dst`, a number no other part of a frame holds —
/// into the destination slot, which is enough to say that the caller named the
/// slot the IR named and that the answer arrived where the next instruction
/// reads it.
///
/// # Safety
///
/// `ctx` is the pointer the entry point was called with, and `base + dst` is a
/// slot of the caller's frame, which `crate::subset`'s `supported` bounded.
unsafe extern "C" fn call(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    callee: u32,
    args: u32,
    dst: u32,
) -> u32 {
    let work = (*ctx).pending_work;
    CALLS.with(|calls| {
        calls.borrow_mut().push(Called {
            base,
            pc,
            callee,
            args,
            dst,
            work,
        })
    });
    // A real helper charges what it was handed, so a real one clears it.
    (*ctx).pending_work = 0;
    let scripted = CALL_ANSWERS.with(|answers| {
        let mut answers = answers.borrow_mut();
        if answers.is_empty() {
            None
        } else {
            Some(answers.remove(0))
        }
    });
    match scripted {
        None | Some(0) => {
            let at = (*ctx).words.add((base + u64::from(dst)) as usize);
            at.write(u64::from(callee) * 1000 + u64::from(dst));
            Outcome::Returned.abi()
        }
        Some(other) => other,
    }
}

/// The runtime's open half, as a test double: the runtime finishes the call.
///
/// A direct call asks where the callee's code is and this answers *nowhere* —
/// `entry: None`, which is the ordinary answer for a callee the tier has not
/// compiled and means the runtime made the whole call itself. So it does: the
/// same recording and the same one recognisable word [`call`] writes, and that
/// call's outcome in [`Opened::base`].
///
/// Every expectation in this file therefore holds whether the arm under test
/// emitted a direct call or a mediated one, which is what lets the one arm that
/// can emit both be held to the same suite twice. A double that handed back a
/// real entry would need a real callee and a real frame to give it, and that is
/// a test about the *direct* protocol rather than about the slice — it lives in
/// `tests/template.rs`, beside the only arm that emits one.
///
/// # Safety
///
/// As [`call`].
unsafe extern "C" fn open(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    callee: u32,
    args: u32,
    dst: u32,
) -> Opened {
    Opened {
        entry: None,
        base: u64::from(call(ctx, base, pc, callee, args, dst)),
    }
}

/// The runtime's close half, as a test double, and it is a tripwire.
///
/// This file's [`open`] never opens a frame, so nothing in this file can reach
/// this: a direct call that got here would be one whose caller entered a callee
/// that was never handed to it. Answering something would let that pass quietly.
///
/// # Safety
///
/// Reads nothing through any of its arguments.
unsafe extern "C" fn close(_ctx: *mut NativeCtx, outcome: u32, callee: u32) -> u32 {
    panic!(
        "the close helper was reached for callee {callee} with outcome {outcome}, and this \
         suite's `open` never opens a frame for one"
    )
}

pub fn helpers() -> NativeHelpers {
    NativeHelpers {
        safepoint,
        call,
        open,
        close,
    }
}

pub fn calls() -> Vec<Called> {
    CALLS.with(|calls| calls.borrow().clone())
}

pub fn forget_calls() {
    CALLS.with(|calls| calls.borrow_mut().clear());
    CALL_ANSWERS.with(|answers| answers.borrow_mut().clear());
}

/// Scripts what the next calls answer. See [`CALL_ANSWERS`].
pub fn calls_answer(outcomes: &[Outcome]) {
    CALL_ANSWERS.with(|answers| {
        *answers.borrow_mut() = outcomes.iter().map(|outcome| outcome.abi()).collect()
    });
}

pub fn polls() -> Vec<(u32, u64)> {
    POLLS.with(|polls| polls.borrow().clone())
}

pub fn forget_polls() {
    POLLS.with(|polls| polls.borrow_mut().clear());
    POLLS_ALLOWED.with(|allowed| allowed.set(usize::MAX));
    WATCHED.with(|watched| watched.set(None));
    WATCHED_SAW.with(|saw| saw.borrow_mut().clear());
}

/// Asks the safepoint helper to read word `at` of the frame's segment.
pub fn watch(at: usize) {
    WATCHED.with(|watched| watched.set(Some(at)));
}

pub fn watched() -> Vec<u64> {
    WATCHED_SAW.with(|saw| saw.borrow().clone())
}

// --- the arm under test -------------------------------------------------------

/// One code generator, as the suite below uses it.
///
/// The four operations every arm has: make one, compile a function, make the
/// code executable, and take an entry point. Nothing else is asked of an arm,
/// and nothing in the suite names a concrete one.
pub trait Arm {
    /// What this arm's `compile` answers. Both arms' are `Copy` and carry the
    /// `FunctionId` and the code size; the suite needs neither.
    type Handle: Copy;

    fn new(helpers: NativeHelpers) -> Self;
    fn compile(&mut self, program: &Program, id: FunctionId) -> Option<Self::Handle>;
    fn finalize(&mut self);
    fn entry(&self, handle: Self::Handle) -> Entry;
}

// --- building a program by hand ----------------------------------------------

// `LayoutId(0)` is `LayoutId::FREE` and names no value, so the table below
// starts with it and nothing here uses it.
pub const INT: LayoutId = LayoutId(1);
pub const BOOL: LayoutId = LayoutId(2);
pub const UNIT: LayoutId = LayoutId(3);
/// Two `Int` words inline, for the only multi-word thing this slice moves.
pub const PAIR: LayoutId = LayoutId(4);
pub const DURATION: LayoutId = LayoutId(5);
/// An `Int` and a reference inline, which is a `struct Pair { n: Int, s: String }`.
pub const REF_PAIR: LayoutId = LayoutId(6);
/// One `Repr::Tag` word, which is what an enum with no payload is.
pub const TAG: LayoutId = LayoutId(7);
/// One reference word.
pub const REF: LayoutId = LayoutId(8);
/// An `Int` and a `Repr::Host` word inline, which no arm lowers.
pub const HOST_PAIR: LayoutId = LayoutId(9);
/// A family of *no* words, which is what an empty struct lowers to.
///
/// It is the zero-width return ADR 0057 says writes nothing, and it is a real
/// shape rather than an invented one: `Unit` is one word in this IR —
/// `Layout::word("Unit", Repr::Unit)` above — and `struct Empty {}` is none. The
/// lowering gives a width-0 destination the next free slot number, which may be
/// one the frame does not have, so a return that formed the address anyway would
/// be writing into whatever is above the frame.
pub const EMPTY: LayoutId = LayoutId(10);
/// One [`Repr::Addr`] word, which is the whole of what a place is.
///
/// `Inst::AddrOfSlot`'s destination and `Inst::Load`'s address: ADR 0034's "There
/// is no place object, no place stack and no table of places", so a `var`
/// parameter is an ordinary slot holding a linear word index.
pub const ADDR: LayoutId = LayoutId(11);

pub fn span() -> Span {
    Span::new(FileId(0), 0, 0)
}

pub fn function(reprs: Vec<Repr>, returns: LayoutId, code: Vec<Inst>) -> Function {
    Function {
        module: Arc::from("m"),
        name: Arc::from("f"),
        params: Vec::new(),
        spans: vec![span(); code.len()],
        refs: RefMap::of(&reprs),
        reprs,
        returns,
        captures: Vec::new(),
        code,
        locals: Vec::new(),
        inlined: Vec::new(),
        span: span(),
        is_async: false,
        stub: false,
    }
}

pub fn program(function: Function) -> Program {
    Program {
        functions: vec![function],
        layouts: vec![
            Layout::free(),
            Layout::word("Int", Repr::Int),
            Layout::word("Bool", Repr::Bool),
            Layout::word("Unit", Repr::Unit),
            Layout::inline(
                "Pair",
                cove_ir::Shape::Struct {
                    fields: Vec::new(),
                    opaque: false,
                },
                vec![Repr::Int, Repr::Int],
            ),
            Layout::word("Duration", Repr::Duration),
            Layout::inline(
                "RefPair",
                cove_ir::Shape::Struct {
                    fields: Vec::new(),
                    opaque: false,
                },
                vec![Repr::Int, Repr::Ref],
            ),
            Layout::word("Kind", Repr::Tag),
            Layout::word("Ref", Repr::Ref),
            Layout::inline(
                "HostPair",
                cove_ir::Shape::Struct {
                    fields: Vec::new(),
                    opaque: false,
                },
                vec![Repr::Int, Repr::Host],
            ),
            Layout::inline(
                "Empty",
                cove_ir::Shape::Struct {
                    fields: Vec::new(),
                    opaque: false,
                },
                Vec::new(),
            ),
            Layout::word("Addr", Repr::Addr),
        ],
        // `ArgsId(0)` is the empty argument list, which is what a call in these
        // tests hands over: the double records the hand-over and does not read
        // the list, and a table with nothing in it would panic the subset
        // predicate that bounds every argument's slot.
        args: vec![Vec::new()],
        ..Program::default()
    }
}

/// The same program, with switch tables.
pub fn program_with_tables(function: Function, tables: Vec<Table>) -> Program {
    Program {
        tables,
        ..program(function)
    }
}

/// The same program, with one non-empty argument list at `ArgsId(1)`.
pub fn program_with_args(function: Function, args: Vec<Arg>) -> Program {
    let mut held = program(function);
    held.args.push(args);
    held
}

/// The same program, with one builtin at `BuiltinId(0)` and its operands at
/// `ArgsId(1)`.
///
/// A builtin is named rather than numbered — see [`cove_ir::Builtin`] — so the
/// two strings are what decide whether the tier lowers this call at all, and a
/// case that passes the wrong pair should be refused rather than compiled. That
/// is what `a_builtin_no_arm_lowers_refuses_the_function` checks with them.
pub fn program_with_builtin(
    function: Function,
    receiver: &str,
    operation: &str,
    result: LayoutId,
    args: Vec<Arg>,
) -> Program {
    let mut held = program_with_args(function, args);
    held.builtins.push(cove_ir::Builtin {
        receiver: Arc::from(receiver),
        operation: Arc::from(operation),
        result,
    });
    held
}

/// The linear address of word zero of the segment every case runs over.
///
/// **Not zero, and that is the whole point of it.** It is
/// [`NativeCtx::stack_origin`], and an arm that resolved a stack address as
/// though the segment began at address zero — which is what a lowering that
/// confused the linear address with the word index would do — would read a word
/// a million places away from the one the VM reads. Zero would have let that
/// through, exactly as a `base` of zero would have let an address formed as if
/// the frame began at word zero through, which is why no case here uses one.
///
/// `1 << 20` is a real segment origin: `cove_runtime::vm::mem`'s `SEGMENT_WORDS`
/// is that, so this is the second task's segment.
pub const SEGMENT_ORIGIN: u64 = 1 << 20;

/// How many words of destination an entry is given.
///
/// One more than the widest return in this file, so that every case can assert
/// the answer *and* that the word past it was left alone — which is what says a
/// return of `width` words wrote `width` words.
pub const DESTINATION_WORDS: usize = 4;

/// What an unwritten word of a destination holds.
///
/// A number no lowering in this crate produces and no fixture computes, so a
/// destination word that still holds it is a word nothing wrote. Zero would not
/// do: a `Bool` answer is zero and so is a frame nobody touched.
pub const UNWRITTEN: u64 = 0x5555_aaaa_5555_aaaa;

/// What one entry into compiled code answered.
pub struct Answer {
    pub outcome: Outcome,
    /// The destination, as the entry left it: [`DESTINATION_WORDS`] words of
    /// which the first `Function::returns`' width are the answer and the rest
    /// are [`UNWRITTEN`].
    ///
    /// ADR 0057 — the callee writes its answer into the run its caller named, so
    /// this is what a case reads instead of a reported slot. It is a stronger
    /// thing to read: a slot number says where the answer would have been copied
    /// from, and this is the copy.
    pub returned: Vec<u64>,
    pub raise: Option<Raise>,
    pub raise_detail: u32,
    /// Which instruction raised, and the two numbers an out-of-range message
    /// names. See [`NativeCtx::raise_pc`].
    pub raise_pc: u32,
    pub raise_a: i64,
    pub raise_b: i64,
    pub pending_work: u64,
}

/// Compiles the one function of `program` and enters it over `words`.
///
/// `base` is deliberately not zero in every caller: it is a *word index* into
/// the segment, so a frame that does not begin at word zero is the case that
/// catches an address formed as if it did.
pub fn run<A: Arm>(program: &Program, words: &mut [u64], base: u64) -> Answer {
    let mut jit = A::new(helpers());
    let compiled = jit
        .compile(program, FunctionId(0))
        .expect("the function is inside the slice");
    jit.finalize();
    enter(&jit, compiled, words, base)
}

/// A heap for the tests, in the shape compiled code addresses one.
///
/// One `Vec<u64>` per chunk and a table of their base pointers, which is what
/// [`NativeCtx::chunks`] is. The real heap's chunks are `AtomicU64` and are
/// committed by the allocator; these are plain words and all of them exist,
/// which is a difference no emitted instruction can see — a `Relaxed` load of an
/// `AtomicU64` is a plain load, and the table says nothing about how a chunk
/// came to be there.
///
/// It holds more than one chunk on purpose. The whole of the chunk arithmetic is
/// only exercised by an object whose words are *not* all in chunk zero, and an
/// object that straddles a boundary is the case a single-chunk heap would have
/// let through.
pub struct Heap {
    held: Vec<Vec<u64>>,
    table: Vec<*mut u64>,
}

impl Heap {
    pub fn new(chunks: usize) -> Heap {
        let mut held: Vec<Vec<u64>> = (0..chunks)
            .map(|_| vec![0u64; HEAP_CHUNK_WORDS as usize])
            .collect();
        let table = held.iter_mut().map(|chunk| chunk.as_mut_ptr()).collect();
        Heap { held, table }
    }

    /// The linear address of heap word `index`, which is what a `Repr::Ref` slot
    /// holds.
    pub fn addr(&self, index: u64) -> u64 {
        HEAP_ORIGIN_WORDS + index
    }

    pub fn set(&mut self, index: u64, word: u64) {
        let chunk = (index / HEAP_CHUNK_WORDS) as usize;
        let at = (index % HEAP_CHUNK_WORDS) as usize;
        self.held[chunk][at] = word;
    }

    pub fn get(&self, index: u64) -> u64 {
        let chunk = (index / HEAP_CHUNK_WORDS) as usize;
        let at = (index % HEAP_CHUNK_WORDS) as usize;
        self.held[chunk][at]
    }

    /// An object's header at `index`, and its payload from `index + 1`.
    ///
    /// `mem::header`: the layout in the high half and the length field in the
    /// low one. The length means whatever the layout says — a byte count for a
    /// string, an element count for an array.
    pub fn object(&mut self, index: u64, layout: LayoutId, len: u32) -> u64 {
        self.set(index, (u64::from(layout.0) << 32) | u64::from(len));
        self.addr(index)
    }

    pub fn table(&self) -> *const *mut u64 {
        self.table.as_ptr()
    }
}

pub fn run_over<A: Arm>(program: &Program, words: &mut [u64], base: u64, heap: &Heap) -> Answer {
    let mut jit = A::new(helpers());
    let compiled = jit
        .compile(program, FunctionId(0))
        .expect("the function is inside the slice");
    jit.finalize();
    enter_over(&jit, compiled, words, base, heap.table())
}

pub fn enter<A: Arm>(jit: &A, compiled: A::Handle, words: &mut [u64], base: u64) -> Answer {
    enter_over(jit, compiled, words, base, std::ptr::null())
}

/// Enters `compiled` over `words` and a destination of the suite's own.
///
/// The destination is [`DESTINATION_WORDS`] words *past* everything `words`
/// holds, reached as `return_base + return_slot` with a slot of one — so that the
/// two numbers are added rather than one of them being ignored, and so that the
/// word before the run is a guard this function checks itself. The segment the
/// entry is given is therefore a copy of `words` with that run appended, and the
/// prefix is copied back before this returns: a case reads its frame out of
/// `words` exactly as it did before the destination existed.
pub fn enter_over<A: Arm>(
    jit: &A,
    compiled: A::Handle,
    words: &mut [u64],
    base: u64,
    chunks: *const *mut u64,
) -> Answer {
    let mut held: Vec<u64> = words.to_vec();
    let guard = held.len() as u64;
    held.extend([UNWRITTEN; DESTINATION_WORDS + 1]);
    let mut ctx =
        NativeCtx::new(std::ptr::null_mut(), held.as_mut_ptr(), SEGMENT_ORIGIN).over_heap(chunks);
    let entry = jit.entry(compiled);
    // Safety: `ctx.words` is `held`, `base` indexes into it, the functions below
    // are all built with frames that fit inside the prefix, and the destination
    // is `DESTINATION_WORDS` words that no frame overlaps.
    let outcome = unsafe { entry(&mut ctx, base, guard, 1) };
    assert_eq!(
        held[guard as usize], UNWRITTEN,
        "the word before the destination is not the destination's, and a return \
         that wrote it formed the address without the slot"
    );
    let returned = held[guard as usize + 1..].to_vec();
    words.copy_from_slice(&held[..words.len()]);
    Answer {
        outcome,
        returned,
        raise: ctx.raise(),
        raise_detail: ctx.raise_detail,
        raise_pc: ctx.raise_pc,
        raise_a: ctx.raise_a,
        raise_b: ctx.raise_b,
        pending_work: ctx.pending_work,
    }
}

/// Whether the one function of `program` is inside the slice at all.
pub fn compiles<A: Arm>(program: &Program) -> bool {
    let mut jit = A::new(helpers());
    jit.compile(program, FunctionId(0)).is_some()
}

// --- a loop ------------------------------------------------------------------

/// `total = 0; i = 1; while i <= n { total += i; i += 1 }; return total`
///
/// Slots: `s0` the bound, `s1` the running total, `s2` the counter, `s3` the
/// condition.
///
/// ```text
/// 0: int    s1 = 0
/// 1: int    s2 = 1
/// 2: le.int s3 = s2, s0
/// 3: branch-false s3 -> 7
/// 4: add.int s1 = s1, s2
/// 5: add.int.imm s2 = s2, 1
/// 6: jump -> 2
/// 7: return s1
/// ```
pub fn summing_loop() -> Program {
    program(function(
        vec![Repr::Int, Repr::Int, Repr::Int, Repr::Bool],
        INT,
        vec![
            Inst::Int { dst: 1, value: 0 },
            Inst::Int { dst: 2, value: 1 },
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Le,
                dst: 3,
                a: 2,
                b: 0,
            },
            Inst::BranchFalse { cond: 3, to: 7 },
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: 1,
                a: 1,
                b: 2,
            },
            Inst::ArithImm {
                op: ArithOp::Add,
                dst: 2,
                a: 2,
                value: 1,
            },
            Inst::Jump { to: 2 },
            Inst::Return { src: 1 },
        ],
    ))
}

/// The loop runs, answers, and polls exactly once per backedge.
///
/// The answer mirrors `encoded.rs`'s `int_op!` (`ADD_INT`, line 1112) and
/// `arith_imm!` (`ADD_INT_IMM`, line 1155), its `cmp_int!` (`LE_INT`) and its
/// `BRANCH_FALSE` arm at line 1191 — which tests the *word* against zero,
/// which is why a `Bool` slot holding one is taken as true.
///
/// The poll count is the interesting half. There is one backedge, the `jump`
/// at pc 6, and the loop body executes ten times for `n = 10`, so the
/// safepoint helper is entered ten times and not eleven: the iteration that
/// fails the condition leaves through the forward branch, which is not a
/// backedge and is not a safepoint.
pub fn a_loop_answers_and_polls_once_per_backedge<A: Arm>() {
    forget_polls();
    let mut words = vec![0u64; 16];
    // A frame at word 4, not word 0, so a base the code ignored would read
    // zeroes and answer zero.
    words[4] = 10;
    let answer = run::<A>(&summing_loop(), &mut words, 4);

    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[4 + 1], 55);
    assert_eq!(
        answer.returned,
        [55, UNWRITTEN, UNWRITTEN, UNWRITTEN],
        "one word of answer into the destination, and nothing past it"
    );
    assert_eq!(words[4 + 2], 11, "the counter is left one past the bound");

    let polls = polls();
    assert_eq!(polls.len(), 10, "one safepoint per backedge, ten backedges");
    assert!(
        polls.iter().all(|(pc, _)| *pc == 2),
        "a backedge's safepoint reports the pc the frame is about to resume at, \
         which is the jump's target: {polls:?}"
    );
}

/// The work charge is the static instruction count of the blocks actually
/// entered, and it is reset at every safepoint.
///
/// Spelled out rather than merely "nonzero", because ADR 0055's block
/// accounting is the part with no other test. The blocks of
/// [`summing_loop`] are `[0,2)`, `[2,4)`, `[4,7)` and `[7,8)` — four leaders:
/// the entry, the `jump`'s target, the `branch-false`'s fall-through, and the
/// `branch-false`'s target — so they are 2, 2, 3 and 1 instructions long.
///
/// The first backedge is reached having entered blocks 0, 2 and 4: 2 + 2 + 3
/// = 7. Every later one is reached from the safepoint that reset the
/// accumulator, through blocks 2 and 4: 2 + 3 = 5. The return is reached
/// after the last reset through blocks 2 and 7: 2 + 1 = 3, and that is what
/// is left pending, which is ADR 0055's "pending work charged on every exit".
pub fn the_work_charge_is_the_static_block_count<A: Arm>() {
    forget_polls();
    let mut words = vec![0u64; 8];
    words[0] = 3;
    let answer = run::<A>(&summing_loop(), &mut words, 0);

    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(polls(), vec![(2, 7), (2, 5), (2, 5)]);
    assert_eq!(answer.pending_work, 3);
}

/// A safepoint that answers "stop" stops the run, and leaves nothing pending.
///
/// This is the only test in which the helper returns `false`, and it is also
/// the only one that proves the frame pointer is re-derived after a call:
/// compiled code that had cached a pointer across the helper would still
/// answer here, so what this really pins is the *outcome* of a stop and the
/// accounting at it. ADR 0040's bounded stop is the helper's, and this is
/// the compiled side of it: leave at once, and do not charge twice.
pub fn a_safepoint_can_stop_the_run<A: Arm>() {
    forget_polls();
    POLLS_ALLOWED.with(|allowed| allowed.set(3));
    let mut words = vec![0u64; 8];
    words[0] = 1_000_000;
    let answer = run::<A>(&summing_loop(), &mut words, 0);

    assert_eq!(answer.outcome, Outcome::Stopped);
    assert_eq!(polls().len(), 3);
    assert_eq!(
        answer.pending_work, 0,
        "the charge went to the helper before it answered, so nothing is pending"
    );
}

/// ADR 0057: a zero-width return writes nothing at all.
///
/// `struct Empty {}` is no words, and the lowering gives a width-0 destination
/// the next free slot number — `call s2:m.Empty` in a two-slot frame is what
/// `cove_ir` emits — so the destination may not be a word of anything. A return
/// that formed the address and stored a word would be writing above the caller's
/// frame, which is the callee's own frame while the callee is still running.
///
/// The `Return` here names `src: 1` for the same reason: a zero-width value's
/// slot is one past everything the frame holds, and `crate::subset`'s `run`
/// admits it because zero words at the end of a frame are inside it.
pub fn a_zero_width_return_writes_nothing<A: Arm>() {
    forget_polls();
    let held = program(function(
        vec![Repr::Int, Repr::Int],
        EMPTY,
        vec![Inst::Int { dst: 0, value: 7 }, Inst::Return { src: 2 }],
    ));
    assert!(compiles::<A>(&held));
    // A word at the slot the zero-width return names, which is one past the
    // frame. It is here so that a return which wrote one word anyway would copy
    // *this* into the destination and be caught: with nothing there, the word
    // such a return copies is whatever lies past the frame, and the suite's own
    // guard word is exactly `UNWRITTEN` — so the assertion below would hold for
    // the wrong reason. It was checked by making the arm write one word, which
    // is how the coincidence was found.
    let mut words = vec![0u64, 0, 0xdead_beef_dead_beef, 0];
    let answer = run::<A>(&held, &mut words, 0);

    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[0], 7, "the body ran");
    assert_eq!(
        answer.returned, [UNWRITTEN; DESTINATION_WORDS],
        "a zero-width return wrote no word of the destination"
    );
}

/// ADR 0057: a raise and a stop publish no result.
///
/// "Semantically" is the whole of it — a destination nothing may read is free to
/// hold anything — but neither arm writes it at all, and that is the easiest
/// version of the rule to keep true and the easiest to check. Both ways out are
/// checked in one function because they are one claim about the two.
pub fn leaving_publishes_no_destination<A: Arm>() {
    // A raise: the addition overflows before the return is reached.
    let (answer, _) = arith::<A>(ArithOp::Add, i64::MAX, 1);
    assert_eq!(answer.outcome, Outcome::Raised);
    assert_eq!(
        answer.returned, [UNWRITTEN; DESTINATION_WORDS],
        "a raise wrote no word of the destination"
    );

    // A stop: the safepoint helper answers `false` on the fourth poll.
    forget_polls();
    POLLS_ALLOWED.with(|allowed| allowed.set(3));
    let mut words = vec![0u64; 8];
    words[0] = 1_000_000;
    let answer = run::<A>(&summing_loop(), &mut words, 0);
    assert_eq!(answer.outcome, Outcome::Stopped);
    assert_eq!(
        answer.returned, [UNWRITTEN; DESTINATION_WORDS],
        "a stop wrote no word of the destination"
    );
}

// --- arithmetic --------------------------------------------------------------

/// `s2 = s0 op s1; return s2`, over `Int` slots.
pub fn binary(op: ArithOp) -> Program {
    program(function(
        vec![Repr::Int, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::Arith {
                num: Num::Int,
                op,
                dst: 2,
                a: 0,
                b: 1,
            },
            Inst::Return { src: 2 },
        ],
    ))
}

pub fn arith<A: Arm>(op: ArithOp, a: i64, b: i64) -> (Answer, Vec<u64>) {
    forget_polls();
    let mut words = vec![a as u64, b as u64, 0];
    let answer = run::<A>(&binary(op), &mut words, 0);
    (answer, words)
}

/// The ordinary answers, including the two rounding questions `sdiv` and
/// `srem` could get wrong.
///
/// `encoded.rs`'s `int_op!` (line 877) calls `int_arith`
/// (`crates/cove-runtime/src/vm/exec.rs:3705`), whose `Div` and `Rem` arms are
/// `checked_div` and `checked_rem` — Rust's truncating division, so `-7 / 2`
/// is `-3` and `-7 % 2` is `-1`. Cranelift's `sdiv` and `srem` are the same
/// two, which is why this passes; it is here because "the same two" is a
/// claim and not an axiom.
pub fn integer_arithmetic_answers_what_the_vm_answers<A: Arm>() {
    for (op, a, b, expected) in [
        (ArithOp::Add, 2i64, 3i64, 5i64),
        (ArithOp::Add, i64::MAX, -1, i64::MAX - 1),
        (ArithOp::Sub, 2, 3, -1),
        (ArithOp::Mul, -6, 7, -42),
        (ArithOp::Div, 7, 2, 3),
        (ArithOp::Div, -7, 2, -3),
        (ArithOp::Rem, 7, 2, 1),
        (ArithOp::Rem, -7, 2, -1),
        (ArithOp::Rem, 7, -2, 1),
    ] {
        let (answer, words) = arith::<A>(op, a, b);
        assert_eq!(answer.outcome, Outcome::Returned, "{op:?} {a} {b}");
        assert_eq!(words[2] as i64, expected, "{op:?} {a} {b}");
        assert_eq!(
            answer.returned[0] as i64, expected,
            "the destination holds what the slot holds: {op:?} {a} {b}"
        );
    }
}

/// Every way `int_arith` can fail, and the error it names.
///
/// This is the test the whole slice is worth having. `int_arith`
/// (`crates/cove-runtime/src/vm/exec.rs:3713`) is `checked_add`,
/// `checked_sub`, `checked_mul`, and a zero test *before* `checked_div` and
/// `checked_rem`. A native `+` that wrapped where this raises would be a
/// silent wrong answer, and `Raise` is how the runtime is told which of
/// `overflowed`/`divided_by_zero` to build — so the mapping is asserted, not
/// assumed.
///
/// The two `i64::MIN` rows are the ones easiest to get wrong in either
/// direction: `checked_div(i64::MIN, -1)` is `None`, which `int_arith` reports
/// as an *overflow* of division rather than as a division by zero, and
/// Cranelift's `sdiv` would have trapped on it — a machine signal, not a Cove
/// error — if the lowering had not branched around it.
///
/// The zero-first order matters too: `int_arith`'s `Div` arm tests `b == 0`
/// before `checked_div`, so `i64::MIN / 0` is "by zero" and not "overflowed".
pub fn every_arithmetic_failure_is_the_vms<A: Arm>() {
    for (op, a, b, expected) in [
        (ArithOp::Add, i64::MAX, 1i64, Raise::AddOverflowed),
        (ArithOp::Add, i64::MIN, -1, Raise::AddOverflowed),
        (ArithOp::Sub, i64::MIN, 1, Raise::SubOverflowed),
        (ArithOp::Sub, i64::MAX, -1, Raise::SubOverflowed),
        (ArithOp::Mul, i64::MAX, 2, Raise::MulOverflowed),
        (ArithOp::Mul, i64::MIN, -1, Raise::MulOverflowed),
        (ArithOp::Div, 1, 0, Raise::DividedByZero),
        (ArithOp::Div, 0, 0, Raise::DividedByZero),
        (ArithOp::Div, i64::MIN, 0, Raise::DividedByZero),
        (ArithOp::Div, i64::MIN, -1, Raise::DivOverflowed),
        (ArithOp::Rem, 1, 0, Raise::RemainderByZero),
        (ArithOp::Rem, i64::MIN, 0, Raise::RemainderByZero),
        (ArithOp::Rem, i64::MIN, -1, Raise::RemOverflowed),
    ] {
        let (answer, _) = arith::<A>(op, a, b);
        assert_eq!(answer.outcome, Outcome::Raised, "{op:?} {a} {b}");
        assert_eq!(answer.raise, Some(expected), "{op:?} {a} {b}");
        assert_eq!(
            answer.pending_work, 2,
            "the whole block is charged at its entry — both the arithmetic and \
             the return it never reached — so a raise leaves that much pending"
        );
    }
}

/// `Inst::ArithImm` is the same arithmetic with the operand in the
/// instruction, and it fails the same way.
///
/// `encoded.rs`'s `arith_imm!` (line 902) is `int_op!` with
/// `held.payload()` where the second slot read was, calling the same
/// `int_arith`. The IR says the same thing — `Inst::ArithImm`'s own
/// documentation: "The same arithmetic `Inst::Arith` does — the same overflow,
/// the same division and remainder by zero, the same `Duration` naming".
pub fn an_immediate_operand_fails_the_same_way<A: Arm>() {
    for (op, a, value, expected) in [
        (ArithOp::Add, i64::MAX, 1i64, Some(Raise::AddOverflowed)),
        (ArithOp::Sub, i64::MIN, 1, Some(Raise::SubOverflowed)),
        (ArithOp::Mul, i64::MAX, 2, Some(Raise::MulOverflowed)),
        (ArithOp::Div, 1, 0, Some(Raise::DividedByZero)),
        (ArithOp::Rem, 1, 0, Some(Raise::RemainderByZero)),
        (ArithOp::Div, i64::MIN, -1, Some(Raise::DivOverflowed)),
        (ArithOp::Add, 40, 2, None),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Int],
            INT,
            vec![
                Inst::ArithImm {
                    op,
                    dst: 1,
                    a: 0,
                    value,
                },
                Inst::Return { src: 1 },
            ],
        ));
        let mut words = vec![a as u64, 0];
        let answer = run::<A>(&held, &mut words, 0);
        match expected {
            Some(raise) => {
                assert_eq!(answer.outcome, Outcome::Raised, "{op:?} {a} {value}");
                assert_eq!(answer.raise, Some(raise), "{op:?} {a} {value}");
            }
            None => {
                assert_eq!(answer.outcome, Outcome::Returned);
                assert_eq!(words[1], 42);
            }
        }
    }
}

/// `s1 = -s0; return s1`, over `Int` slots of the given `Repr`.
///
/// `dst` is the second slot so that the source is still readable afterwards,
/// which is what lets the negation case assert the operand was not clobbered.
pub fn negation(dst: Repr) -> Program {
    program(function(
        vec![Repr::Int, dst],
        match dst {
            Repr::Duration => DURATION,
            _ => INT,
        },
        vec![
            Inst::Neg {
                num: Num::Int,
                dst: 1,
                a: 0,
            },
            Inst::Return { src: 1 },
        ],
    ))
}

pub fn negate<A: Arm>(a: i64) -> (Answer, Vec<u64>) {
    forget_polls();
    let mut words = vec![a as u64, 0];
    let answer = run::<A>(&negation(Repr::Int), &mut words, 0);
    (answer, words)
}

/// `Inst::Neg` over `Num::Int` is `checked_neg`, and its answers are the VM's.
///
/// `encoded.rs`'s `NEG_INT` arm (line 1144) reads the word as `i64`, calls
/// `checked_neg`, and stores the answer — so zero negates to zero rather than to
/// a negative zero, and `i64::MAX` to `i64::MIN + 1`.
pub fn negation_answers_what_the_vm_answers<A: Arm>() {
    for (a, expected) in [
        (0i64, 0i64),
        (1, -1),
        (-1, 1),
        (42, -42),
        (i64::MAX, i64::MIN + 1),
        (i64::MIN + 1, i64::MAX),
    ] {
        let (answer, words) = negate::<A>(a);
        assert_eq!(answer.outcome, Outcome::Returned, "-({a})");
        assert_eq!(words[1] as i64, expected, "-({a})");
        assert_eq!(
            words[0] as i64, a,
            "the operand is not the destination and was not written: -({a})"
        );
        assert_eq!(
            answer.returned[0] as i64, expected,
            "the destination holds what the slot holds: -({a})"
        );
    }
}

/// Negating `i64::MIN` raises, and it raises the VM's error.
///
/// The one input `checked_neg` answers `None` for. `encoded.rs`'s `NEG_INT` arm
/// reports it as `overflowed("negation")` — **not** renamed by a `Duration`
/// destination, because that arm calls `overflowed` directly instead of going
/// through `int_arith`'s `named` closure, which is the asymmetry a
/// reimplementation tidies up by accident. Both destinations are asserted here for
/// that reason.
///
/// It is the case the whole lowering is worth having: a native `-` that wrapped
/// where this raises would be a silent wrong answer, and the template arm's
/// `jno` and the Cranelift arm's comparison against `i64::MIN` are two different
/// ways to get it wrong.
pub fn negating_the_least_int_raises<A: Arm>() {
    for dst in [Repr::Int, Repr::Duration] {
        forget_polls();
        let mut words = vec![i64::MIN as u64, 0];
        let answer = run::<A>(&negation(dst), &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Raised, "-(i64::MIN) into {dst:?}");
        assert_eq!(
            answer.raise,
            Some(Raise::NegOverflowed),
            "and `overflowed(\"negation\")` whatever the destination is: {dst:?}"
        );
        assert_eq!(
            answer.returned, [UNWRITTEN; DESTINATION_WORDS],
            "a raise wrote no word of the destination: {dst:?}"
        );
        assert_eq!(
            answer.pending_work, 2,
            "the whole block is charged at its entry — the negation and the return \
             it never reached"
        );
    }
}

/// A `Duration` destination renames the overflow, and only for the three
/// operations that consult the name.
///
/// `encoded.rs`'s `int_op!` asks `machine.repr(id, a!()) ==
/// Some(Repr::Duration)` and hands the answer to `int_arith`, whose `named`
/// closure is called by the `Add`, `Sub` and `Mul` arms and **not** by `Div`
/// or `Rem` (`crates/cove-runtime/src/vm/exec.rs:3713`). So a duration
/// division that overflows is still `overflowed("division")`, and that
/// asymmetry is the whole of what this test is for — it is the sort of thing a
/// reimplementation tidies up by accident.
pub fn a_duration_destination_renames_only_three_overflows<A: Arm>() {
    for (op, a, b, expected) in [
        (ArithOp::Add, i64::MAX, 1i64, Raise::DurationOverflowed),
        (ArithOp::Sub, i64::MIN, 1, Raise::DurationOverflowed),
        (ArithOp::Mul, i64::MAX, 2, Raise::DurationOverflowed),
        (ArithOp::Div, i64::MIN, -1, Raise::DivOverflowed),
        (ArithOp::Rem, i64::MIN, -1, Raise::RemOverflowed),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Int, Repr::Duration],
            DURATION,
            vec![
                Inst::Arith {
                    num: Num::Int,
                    op,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        ));
        let mut words = vec![a as u64, b as u64, 0];
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Raised, "{op:?}");
        assert_eq!(answer.raise, Some(expected), "{op:?}");
    }
}

// --- fused comparisons, which are ADR 0054's -------------------------------

/// `if !(s0 op s1) goto 3; s3 = 10; return s3; s3 = 20; return s3`, with the
/// comparison fused into the branch.
pub fn fused(op: CmpOp) -> Program {
    program(function(
        vec![Repr::Int, Repr::Int, Repr::Bool, Repr::Int],
        INT,
        vec![
            Inst::CmpBranch {
                on: Compare::Int,
                op,
                dst: 2,
                a: 0,
                b: 1,
                target: 3,
            },
            Inst::Int { dst: 3, value: 10 },
            Inst::Return { src: 3 },
            Inst::Int { dst: 3, value: 20 },
            Inst::Return { src: 3 },
        ],
    ))
}

/// `Inst::CmpBranch` branches *and* writes the `Bool`, both ways.
///
/// [ADR 0054] is explicit that the fusion "is exactly the two instructions it
/// replaces, in that order, with no condition attached — the `Bool` is still
/// written to `dst`", and `encoded.rs`'s `cmp_int_branch!` macro (line 1197
/// onward, `LT_INT_BRANCH` and its siblings) does the store before the test.
/// So the write is asserted on both paths and not only on the one that falls
/// through: a lowering that skipped it on the taken path would pass a test
/// that only looked at the answer.
///
/// [ADR 0054]: ../../../../docs/adr/0054-a-comparison-that-only-feeds-a-branch-is-the-branch.md
pub fn a_fused_comparison_branches_and_writes_its_bool<A: Arm>() {
    for (op, a, b, answered, flag) in [
        (CmpOp::Lt, 1i64, 2i64, 10i64, 1u64),
        (CmpOp::Lt, 2, 1, 20, 0),
        (CmpOp::Lt, 2, 2, 20, 0),
        (CmpOp::Ge, 2, 2, 10, 1),
        (CmpOp::Eq, -3, -3, 10, 1),
        (CmpOp::Ne, -3, -3, 20, 0),
        (CmpOp::Gt, i64::MIN, i64::MAX, 20, 0),
        (CmpOp::Le, i64::MIN, i64::MAX, 10, 1),
    ] {
        forget_polls();
        let mut words = vec![a as u64, b as u64, 0xdead, 0];
        let answer = run::<A>(&fused(op), &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned, "{op:?} {a} {b}");
        assert_eq!(words[3] as i64, answered, "{op:?} {a} {b}");
        assert_eq!(
            words[2], flag,
            "the fused comparison still writes its `Bool`: {op:?} {a} {b}"
        );
    }
}

/// `Inst::CmpImmBranch` is the same, with the right operand in the
/// instruction.
///
/// `encoded.rs`'s `cmp_imm_branch!` (line 1040) is `cmp_int_branch!` reading
/// `held.lo() as i32` where the second slot was — which is why the IR's
/// immediate is an `i32` here and an `i64` on `Inst::CmpImm`. The negative
/// row is the one that matters: a lowering that zero-extended the immediate
/// instead of sign-extending it would answer the opposite.
pub fn a_fused_immediate_comparison_branches_and_writes_its_bool<A: Arm>() {
    for (op, a, value, answered, flag) in [
        (CmpOp::Lt, 1i64, 5i32, 10i64, 1u64),
        (CmpOp::Lt, 5, 5, 20, 0),
        (CmpOp::Ge, 5, 5, 10, 1),
        (CmpOp::Gt, 0, -1, 10, 1),
        (CmpOp::Lt, 0, -1, 20, 0),
        (CmpOp::Eq, -7, -7, 10, 1),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Int, Repr::Bool, Repr::Int],
            INT,
            vec![
                Inst::CmpImmBranch {
                    op,
                    dst: 2,
                    a: 0,
                    value,
                    target: 3,
                },
                Inst::Int { dst: 3, value: 10 },
                Inst::Return { src: 3 },
                Inst::Int { dst: 3, value: 20 },
                Inst::Return { src: 3 },
            ],
        ));
        let mut words = vec![a as u64, 0, 0xdead, 0];
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned, "{op:?} {a} {value}");
        assert_eq!(words[3] as i64, answered, "{op:?} {a} {value}");
        assert_eq!(words[2], flag, "{op:?} {a} {value}");
    }
}

/// An unfused `Inst::Cmp` and `Inst::CmpImm` write the same `Bool`.
///
/// `encoded.rs`'s `cmp_int!` (line 921) and `cmp_imm!` (line 913):
/// `compare(op, x.cmp(&y))` stored as `answer as u64`, so one or zero and
/// never a mask. A lowering that stored a comparison's raw result without
/// narrowing it would put something other than 0 or 1 in a `Bool` slot, and
/// then `branch-false` — which tests the whole word — would still work while
/// `Bool` equality would not.
pub fn a_comparison_writes_one_or_zero<A: Arm>() {
    for (op, a, b, flag) in [
        (CmpOp::Lt, 1i64, 2i64, 1u64),
        (CmpOp::Lt, 2, 1, 0),
        (CmpOp::Ge, -1, -1, 1),
        (CmpOp::Ne, -1, -1, 0),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Int, Repr::Bool],
            BOOL,
            vec![
                Inst::Cmp {
                    on: Compare::Int,
                    op,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        ));
        let mut words = vec![a as u64, b as u64, 0xdead];
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(words[2], flag, "{op:?} {a} {b}");

        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Bool],
            BOOL,
            vec![
                Inst::CmpImm {
                    op,
                    dst: 1,
                    a: 0,
                    value: b,
                },
                Inst::Return { src: 1 },
            ],
        ));
        let mut words = vec![a as u64, 0xdead];
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(words[1], flag, "imm {op:?} {a} {b}");
    }
}

// --- copies, traps, refusals -------------------------------------------------

/// `Inst::Copy` moves a layout's whole run of words, and moves rather than
/// smears.
///
/// `encoded.rs`'s `COPY` arm (line 1082) is `Memory::copy_slots`, which is a
/// `copy_within` — a `memmove` — and its documentation says why: "two slots
/// of one frame may overlap and a lowering is free to emit that rather than
/// having to prove it does not". The second row here is that overlap, and a
/// forward run of load-store pairs would answer `[7, 7, 7]` for it.
pub fn a_copy_moves_every_word_and_does_not_smear<A: Arm>() {
    for (dst, src, before, after) in [
        (2u32, 0u32, [7u64, 9, 0, 0], [7u64, 9, 7, 9]),
        (1, 0, [7, 9, 0, 0], [7, 7, 9, 0]),
        (0, 1, [7, 9, 11, 0], [9, 11, 11, 0]),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int; 4],
            PAIR,
            vec![
                Inst::Copy {
                    dst,
                    src,
                    layout: PAIR,
                },
                Inst::Return { src: dst },
            ],
        ));
        let mut words = before.to_vec();
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(words, after.to_vec(), "copy {src} -> {dst}");
        assert_eq!(
            answer.returned,
            [
                after[dst as usize],
                after[dst as usize + 1],
                UNWRITTEN,
                UNWRITTEN
            ],
            "two words of answer into the destination, and nothing past them"
        );
    }
}

/// `Inst::Trap` leaves with the message's `StrId` and nothing else.
///
/// `encoded.rs`'s `TRAP` arm (line 1870) is
/// `fail!(RuntimeError::new(program.string(StrId(held.lo())).to_string()))`.
/// The lookup is the runtime's, so what crosses the boundary is the id: this
/// crate has no strings table and must not grow one, which is the same rule
/// that keeps the overflow messages out of `Raise`.
pub fn a_trap_names_its_message_by_id<A: Arm>() {
    forget_polls();
    let held = program(function(
        vec![Repr::Int],
        UNIT,
        vec![
            Inst::Int { dst: 0, value: 1 },
            Inst::Trap { message: StrId(37) },
        ],
    ));
    let mut words = vec![0u64];
    let answer = run::<A>(&held, &mut words, 0);

    assert_eq!(answer.outcome, Outcome::Raised);
    assert_eq!(answer.raise, Some(Raise::Trapped));
    assert_eq!(answer.raise_detail, 37);
    assert_eq!(answer.pending_work, 2, "both instructions of the block");
}

/// A function holding anything outside the slice compiles to `None`, whole.
///
/// ADR 0055: "A function containing an operation the native lowering does not
/// yet support runs entirely on the encoded VM. The initial implementation
/// does not split one function into native and interpreted regions." So the
/// answer is `None` and not a partly compiled entry — including for
/// `Inst::Unit`, which is one store and is refused anyway, because the line
/// is drawn at the adoption gate's list and not at what happens to be easy.
pub fn anything_outside_the_slice_refuses_the_whole_function<A: Arm>() {
    let refused: Vec<(&str, Program)> = vec![
        (
            "a unit constant, which is not on the adoption gate's list",
            program(function(
                vec![Repr::Unit],
                UNIT,
                vec![Inst::Unit { dst: 0 }, Inst::Return { src: 0 }],
            )),
        ),
        (
            "float negation, which `Num::Int` negation being lowered does not admit",
            program(function(
                vec![Repr::Float],
                INT,
                vec![
                    Inst::Neg {
                        num: Num::Float,
                        dst: 0,
                        a: 0,
                    },
                    Inst::Return { src: 0 },
                ],
            )),
        ),
        (
            "float arithmetic",
            program(function(
                vec![Repr::Float, Repr::Float],
                INT,
                vec![
                    Inst::Arith {
                        num: Num::Float,
                        op: ArithOp::Add,
                        dst: 1,
                        a: 0,
                        b: 0,
                    },
                    Inst::Return { src: 1 },
                ],
            )),
        ),
        (
            "a float comparison",
            program(function(
                vec![Repr::Float, Repr::Bool],
                BOOL,
                vec![
                    Inst::Cmp {
                        on: Compare::Float,
                        op: CmpOp::Lt,
                        dst: 1,
                        a: 0,
                        b: 0,
                    },
                    Inst::Return { src: 1 },
                ],
            )),
        ),
        (
            "an ordered `Bool` comparison, which the VM answers with a runtime error",
            program(function(
                vec![Repr::Bool, Repr::Bool],
                BOOL,
                vec![
                    Inst::Cmp {
                        on: Compare::Bool,
                        op: CmpOp::Lt,
                        dst: 1,
                        a: 0,
                        b: 0,
                    },
                    Inst::Return { src: 1 },
                ],
            )),
        ),
        (
            "a frame holding a host handle, however scalar its instructions are",
            program(function(
                vec![Repr::Int, Repr::Host],
                INT,
                vec![Inst::Int { dst: 0, value: 1 }, Inst::Return { src: 0 }],
            )),
        ),
        // The two `Repr` checks overlap in any *real* program — a function that
        // copies a layout holding a host handle has a `Repr::Host` slot to copy
        // it into, so the frame check above would already have refused it. This
        // row exercises the layout check on its own, which is why its frame is
        // scalar and the program is one no lowering would emit: a check that is
        // only ever reached behind another check is a check nothing tests.
        (
            "a copy of a layout holding a host handle",
            program(function(
                vec![Repr::Int, Repr::Int, Repr::Int, Repr::Int],
                INT,
                vec![
                    Inst::Copy {
                        dst: 2,
                        src: 0,
                        layout: HOST_PAIR,
                    },
                    Inst::Return { src: 2 },
                ],
            )),
        ),
        ("a stub, which stands in for a body no lowering lowered", {
            let mut held = program(function(
                vec![Repr::Int],
                INT,
                vec![Inst::Int { dst: 0, value: 1 }, Inst::Return { src: 0 }],
            ));
            held.functions[0].stub = true;
            held
        }),
        (
            "a jump past the end, which the IR verifier refuses too",
            program(function(
                vec![Repr::Int],
                INT,
                vec![Inst::Int { dst: 0, value: 1 }, Inst::Jump { to: 9 }],
            )),
        ),
        (
            "a body whose last instruction is not a terminator",
            program(function(
                vec![Repr::Int],
                INT,
                vec![Inst::Int { dst: 0, value: 1 }],
            )),
        ),
    ];
    for (what, held) in refused {
        assert!(!compiles::<A>(&held), "should have refused: {what}");
    }
}

/// A `Bool` equality *is* inside the slice, which is the other side of the
/// refusal above.
///
/// `encoded.rs` groups `EQ_BOOL | EQ_REF | EQ_TAG => cmp_word!(true)` and
/// refuses the ordered forms beside them at `not_ordered!()` (line 1213). The
/// admitted half is a word comparison; only the refused half is a runtime
/// error.
pub fn a_bool_equality_is_inside_the_slice<A: Arm>() {
    forget_polls();
    let held = program(function(
        vec![Repr::Bool, Repr::Bool, Repr::Bool],
        BOOL,
        vec![
            Inst::Cmp {
                on: Compare::Bool,
                op: CmpOp::Eq,
                dst: 2,
                a: 0,
                b: 1,
            },
            Inst::Return { src: 2 },
        ],
    ));
    let mut words = vec![1, 1, 0xdead];
    let answer = run::<A>(&held, &mut words, 0);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[2], 1);
}

/// `Inst::Bool` writes a word, which is `encoded.rs`'s
/// `CONST_BOOL | CONST_INT | CONST_FLOAT` arm (line 1057) storing
/// `held.payload()`.
pub fn a_boolean_constant_is_a_word<A: Arm>() {
    for (value, word) in [(true, 1u64), (false, 0)] {
        forget_polls();
        let held = program(function(
            vec![Repr::Bool],
            BOOL,
            vec![Inst::Bool { dst: 0, value }, Inst::Return { src: 0 }],
        ));
        let mut words = vec![0xdead];
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(words[0], word);
    }
}

/// Two functions in one code generator, both entered, and the second one
/// compiled after the first was defined.
///
/// Not a correctness question so much as a shape one: ADR 0055 makes "a
/// function the unit of compilation, caching and selection", so a code
/// generator that could only hold one would be the wrong shape however well
/// it worked.
pub fn one_code_generator_holds_many_functions<A: Arm>() {
    forget_polls();
    let mut jit = A::new(helpers());
    let doubling = program(function(
        vec![Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: 1,
                a: 0,
                b: 0,
            },
            Inst::Return { src: 1 },
        ],
    ));
    let first = jit.compile(&doubling, FunctionId(0)).expect("inside");
    let second = jit.compile(&summing_loop(), FunctionId(0)).expect("inside");
    jit.finalize();

    let mut words = vec![21u64, 0];
    assert_eq!(enter(&jit, first, &mut words, 0).outcome, Outcome::Returned);
    assert_eq!(words[1], 42);

    let mut words = vec![0u64; 8];
    words[0] = 4;
    assert_eq!(
        enter(&jit, second, &mut words, 0).outcome,
        Outcome::Returned
    );
    assert_eq!(words[1], 10);
}

// --- the covefmt slice -------------------------------------------------------
//
// Everything below was added for the second raced slice: `covefmt.byteOfPunct`
// and `covefmt.wantsASpaceBetween`, which between them need an `Array` element
// read, an enum's tag and switch, a tag comparison, `String.byteAt`, a length,
// and calls. Every expectation cites the arm of
// `crates/cove-runtime/src/vm/exec/encoded.rs` it mirrors, as the ones above do.

/// A reference slot is inside the slice, which is the other half of the
/// `Repr::Host` refusal above.
///
/// It is worth asserting on its own because it was a refusal one slice ago, and
/// what changed is not a code generator but the collector argument: the frame is
/// the canonical home of every value at every instruction boundary, so a
/// `Repr::Ref` slot is a root the existing walk already finds. See
/// `cove_native::abi`'s "References are live here".
pub fn a_reference_slot_is_inside_the_slice<A: Arm>() {
    let held = program(function(
        vec![Repr::Ref, Repr::Ref],
        REF,
        vec![
            Inst::Copy {
                dst: 1,
                src: 0,
                layout: REF,
            },
            Inst::Return { src: 1 },
        ],
    ));
    assert!(compiles::<A>(&held));
    let mut words = vec![0u64, 0, 0xfeed_beef, 0];
    let answer = run::<A>(&held, &mut words, 2);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[3], 0xfeed_beef, "the reference was copied as a word");
    assert_eq!(
        answer.returned,
        [0xfeed_beef, UNWRITTEN, UNWRITTEN, UNWRITTEN],
        "a reference return is one word — the address, not the object"
    );
}

/// `encoded.rs`'s `FUNC_REF | CONST_TAG` arm (line 1071): a case index is
/// written by the same store a constant is.
pub fn a_tag_is_the_case_index_as_a_word<A: Arm>() {
    let held = program(function(
        vec![Repr::Tag],
        TAG,
        vec![
            Inst::Tag {
                dst: 0,
                layout: TAG,
                case: CaseId(3),
            },
            Inst::Return { src: 0 },
        ],
    ));
    let mut words = vec![0u64; 4];
    let answer = run::<A>(&held, &mut words, 3);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[3], 3);
}

/// `encoded.rs`'s `EQ_TAG`/`NE_TAG` arms (line 1143), which are `cmp_word!`:
/// two case indices compare as the words they are.
pub fn a_tag_comparison_is_a_word_comparison<A: Arm>() {
    for (a, b, same) in [(1u64, 1u64, 1u64), (1, 2, 0), (0, 0, 1)] {
        let held = program(function(
            vec![Repr::Tag, Repr::Tag, Repr::Bool],
            BOOL,
            vec![
                Inst::Cmp {
                    on: Compare::Tag,
                    op: CmpOp::Eq,
                    dst: 2,
                    a: 0,
                    b: 1,
                },
                Inst::Return { src: 2 },
            ],
        ));
        let mut words = vec![a, b, 0xdead];
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(words[2], same, "{a} == {b}");
    }
}

/// `encoded.rs`'s `NOT` arm (line 1170), which tests the whole *word* against
/// zero rather than its low byte.
pub fn not_tests_the_whole_word<A: Arm>() {
    for (given, answer) in [(0u64, 1u64), (1, 0), (0x100, 0), (u64::MAX, 0)] {
        let held = program(function(
            vec![Repr::Bool, Repr::Bool],
            BOOL,
            vec![Inst::Not { dst: 1, a: 0 }, Inst::Return { src: 1 }],
        ));
        let mut words = vec![given, 0xdead];
        assert_eq!(run::<A>(&held, &mut words, 0).outcome, Outcome::Returned);
        assert_eq!(words[1], answer, "!{given:#x}");
    }
}

/// `encoded.rs`'s `LEN` arm (line 1620): the header's length field, and a null
/// reference refused first.
pub fn a_len_reads_the_header_and_refuses_null<A: Arm>() {
    let held = program(function(
        vec![Repr::Ref, Repr::Int],
        INT,
        vec![Inst::Len { dst: 1, obj: 0 }, Inst::Return { src: 1 }],
    ));

    // An object in the *second* chunk, so the chunk arithmetic is exercised
    // rather than a heap whose every word is in chunk zero.
    let at = HEAP_CHUNK_WORDS + 17;
    let mut heap = Heap::new(2);
    let addr = heap.object(at, INT, 4242);
    let mut words = vec![addr, 0];
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[1], 4242);

    let mut words = vec![0u64, 0];
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Raised);
    assert_eq!(answer.raise, Some(Raise::NullObject));
    assert_eq!(answer.raise_pc, 0);
}

/// `String.byteLength()`, which is the `LEN` arm reached through a
/// `call-builtin`.
///
/// `vm::builtins::text::byte_length` is `receiver_addr` and then
/// `machine.object_len(addr)`: the same null refusal and the same header read as
/// `Inst::Len`, so the same three assertions hold — including that the raise names
/// the *builtin's* pc and not the `Len`'s, because a `String.byteLength()` that
/// reported the wrong instruction would print the wrong span.
pub fn a_byte_length_builtin_reads_the_header_and_refuses_null<A: Arm>() {
    let held = program_with_builtin(
        function(
            vec![Repr::Ref, Repr::Int],
            INT,
            vec![
                // One instruction ahead of the builtin, so that `raise_pc` is a
                // number a dropped `self.pc` could not have answered by accident.
                Inst::Int { dst: 1, value: 7 },
                Inst::CallBuiltin {
                    dst: 1,
                    builtin: BuiltinId(0),
                    args: ArgsId(1),
                },
                Inst::Return { src: 1 },
            ],
        ),
        "String",
        "byteLength",
        INT,
        vec![Arg {
            slot: 0,
            layout: REF,
        }],
    );

    // A multi-byte string: the length field is a *byte* count, so a two-byte
    // character is two. `mem::header`'s low half is what both arms read.
    let at = HEAP_CHUNK_WORDS + 9;
    let mut heap = Heap::new(2);
    let addr = heap.object(at, INT, 2);
    let mut words = vec![addr, 0];
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[1], 2, "the header's low half, as a byte count");
    assert_eq!(answer.returned[0], 2);

    // A byte count that needs more than the low half of a word would be a
    // different object; what is asserted here is that the *high* half — the
    // layout — is not read into the answer.
    let addr = heap.object(at, PAIR, 0x1234_5678);
    let mut words = vec![addr, 0];
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[1], 0x1234_5678);

    // `receiver_addr`'s `if addr == 0 { null_value() }`, which is the refusal
    // `Raise::NullObject` names.
    let mut words = vec![0u64, 0];
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Raised);
    assert_eq!(answer.raise, Some(Raise::NullObject));
    assert_eq!(answer.raise_pc, 1, "the builtin's pc, not the constant's");
}

/// A `call-builtin` of a name no arm lowers refuses the whole function.
///
/// The name is the decision — see `subset::method_of` — so this is the one case
/// that says the decision is really made on it: the same instruction, the same
/// operands, the same widths, and a different pair of strings.
pub fn a_builtin_no_arm_lowers_refuses_the_function<A: Arm>() {
    let one = |receiver: &str, operation: &str| {
        program_with_builtin(
            function(
                vec![Repr::Ref, Repr::Int],
                INT,
                vec![
                    Inst::CallBuiltin {
                        dst: 1,
                        builtin: BuiltinId(0),
                        args: ArgsId(1),
                    },
                    Inst::Return { src: 1 },
                ],
            ),
            receiver,
            operation,
            INT,
            vec![Arg {
                slot: 0,
                layout: REF,
            }],
        )
    };
    assert!(compiles::<A>(&one("String", "byteLength")));
    for (receiver, operation) in [
        ("String", "length"),
        ("Array", "byteLength"),
        ("Vector", "push"),
    ] {
        assert!(
            !compiles::<A>(&one(receiver, operation)),
            "`{receiver}.{operation}` is not lowered, so the function is refused"
        );
    }
}

/// `encoded.rs`'s `LOAD_ELEM` arm (line 1425), which is `Machine::element` and
/// then a copy at the element layout's stride.
///
/// The element is two words wide, so this is also the assertion that the stride
/// is the *element's* width and not one: `Machine::element` answers
/// `at as u32 * width`.
pub fn a_load_elem_strides_and_bounds_its_index<A: Arm>() {
    let held = program(function(
        vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Int],
        PAIR,
        vec![
            Inst::LoadElem {
                dst: 2,
                obj: 0,
                index: 1,
                layout: PAIR,
            },
            Inst::Return { src: 2 },
        ],
    ));

    // Three two-word elements, laid out so the last of them is in the next
    // chunk: an object that straddles a boundary is the case one chunk would
    // have let through.
    let at = HEAP_CHUNK_WORDS - 4;
    let build = || {
        let mut heap = Heap::new(2);
        heap.object(at, PAIR, 3);
        for word in 0..6u64 {
            heap.set(at + 1 + word, 100 + word);
        }
        heap
    };

    for (index, first, second) in [(0i64, 100u64, 101u64), (1, 102, 103), (2, 104, 105)] {
        let heap = build();
        let mut words = vec![heap.addr(at), index as u64, 0, 0];
        let answer = run_over::<A>(&held, &mut words, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Returned, "index {index}");
        assert_eq!((words[2], words[3]), (first, second), "index {index}");
        assert_eq!(
            answer.returned,
            [first, second, UNWRITTEN, UNWRITTEN],
            "index {index}"
        );
    }

    // `Machine::element`: "index {at} is outside a collection of {len}", for a
    // negative index as much as for a large one.
    for index in [3i64, -1, i64::MIN] {
        let heap = build();
        let mut words = vec![heap.addr(at), index as u64, 0, 0];
        let answer = run_over::<A>(&held, &mut words, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Raised, "index {index}");
        assert_eq!(answer.raise, Some(Raise::IndexOutOfRange));
        assert_eq!(answer.raise_a, index);
        assert_eq!(answer.raise_b, 3);
    }

    let heap = build();
    let mut words = vec![0u64, 0, 0, 0];
    let answer = run_over::<A>(&held, &mut words, 0, &heap);
    assert_eq!(answer.raise, Some(Raise::NullObject));
}

/// `encoded.rs`'s `BYTE_AT` arm (line 1456): a payload read, a shift and a mask,
/// eight bytes to a word and least-significant byte first.
pub fn a_byte_at_reads_one_byte_and_bounds_it<A: Arm>() {
    let held = program(function(
        vec![Repr::Ref, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::ByteAt {
                dst: 2,
                obj: 0,
                at: 1,
            },
            Inst::Return { src: 2 },
        ],
    ));

    // Ten bytes over two payload words, and the object placed so the second of
    // them is in the next chunk.
    let bytes: [u8; 10] = [7, 8, 9, 10, 200, 0, 255, 1, 42, 43];
    let at = HEAP_CHUNK_WORDS - 2;
    let build = || {
        let mut heap = Heap::new(2);
        heap.object(at, INT, bytes.len() as u32);
        for (word, run) in bytes.chunks(8).enumerate() {
            let mut packed = 0u64;
            for (byte, value) in run.iter().enumerate() {
                packed |= u64::from(*value) << (byte * 8);
            }
            heap.set(at + 1 + word as u64, packed);
        }
        heap
    };

    for (offset, byte) in bytes.iter().enumerate() {
        let heap = build();
        let mut words = vec![heap.addr(at), offset as u64, 0xdead];
        let answer = run_over::<A>(&held, &mut words, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Returned, "byte {offset}");
        assert_eq!(words[2], u64::from(*byte), "byte {offset}");
    }

    for offset in [10i64, -1] {
        let heap = build();
        let mut words = vec![heap.addr(at), offset as u64, 0];
        let answer = run_over::<A>(&held, &mut words, 0, &heap);
        assert_eq!(answer.outcome, Outcome::Raised, "byte {offset}");
        assert_eq!(answer.raise, Some(Raise::ByteOffset));
        assert_eq!(answer.raise_a, offset);
        assert_eq!(answer.raise_b, 10, "the length, not the last legal offset");
    }
}

/// `encoded.rs`'s `SWITCH` arm (line 1234):
/// `targets.get(index).unwrap_or(&default)`.
///
/// The word above `u32::MAX` is the case that matters. It is what a switch reads
/// out of a slot the program filled, the machine does not take the lowering's
/// word for what is in it, and one arm reaches its jump table through an `i32`.
pub fn a_switch_takes_its_case_or_the_default<A: Arm>() {
    let held = program_with_tables(
        function(
            vec![Repr::Tag, Repr::Int],
            INT,
            vec![
                Inst::Switch {
                    on: 0,
                    table: TableId(0),
                },
                Inst::Int { dst: 1, value: 11 },
                Inst::Return { src: 1 },
                Inst::Int { dst: 1, value: 22 },
                Inst::Return { src: 1 },
                Inst::Int { dst: 1, value: 33 },
                Inst::Return { src: 1 },
            ],
        ),
        vec![Table {
            targets: vec![1, 3],
            default: 5,
        }],
    );
    for (index, answer) in [
        (0u64, 11u64),
        (1, 22),
        (2, 33),
        (1 << 32, 33),
        ((1 << 32) | 1, 33),
        (u64::MAX, 33),
    ] {
        let mut words = vec![index, 0];
        let left = run::<A>(&held, &mut words, 0);
        assert_eq!(left.outcome, Outcome::Returned, "case {index}");
        assert_eq!(words[1], answer, "case {index}");
    }
}

/// A call is handed to the runtime whole, and what comes back decides.
///
/// The helper is a double — see [`call`] — so what is asserted here is the
/// *protocol* and not a callee's answer: the caller's frame and the call's own
/// pc go over, the unpaid work goes with them, the answer lands in the slot the
/// IR named, and an outcome that is not `Returned` leaves the function carrying
/// that outcome.
pub fn a_call_hands_over_and_an_outcome_travels_out<A: Arm>() {
    let held = program_with_args(
        function(
            vec![Repr::Int, Repr::Int, Repr::Int],
            INT,
            vec![
                Inst::Int { dst: 0, value: 5 },
                Inst::Call {
                    dst: 2,
                    callee: FunctionId(0),
                    args: ArgsId(1),
                },
                Inst::Return { src: 2 },
            ],
        ),
        vec![Arg {
            slot: 0,
            layout: INT,
        }],
    );

    forget_polls();
    forget_calls();
    let mut words = vec![0u64; 7];
    let answer = run::<A>(&held, &mut words, 4);
    assert_eq!(answer.outcome, Outcome::Returned);
    // The double writes `callee * 1000 + dst`, which for `FunctionId(0)` into
    // slot 2 is 2 — a number no other word of this frame holds.
    assert_eq!(words[6], 2, "the answer landed in `dst`");
    assert_eq!(
        answer.returned,
        [2, UNWRITTEN, UNWRITTEN, UNWRITTEN],
        "and then out of `dst` into this function's own destination"
    );
    assert_eq!(
        calls(),
        vec![Called {
            base: 4,
            pc: 1,
            callee: 0,
            args: 1,
            dst: 2,
            // Three instructions in the one block, charged at its entry and
            // still unpaid when the call handed over.
            work: 3,
        }]
    );

    // A raise out of a callee leaves this function with the callee's outcome,
    // and does not overwrite what the helper recorded about it.
    forget_calls();
    calls_answer(&[Outcome::Raised]);
    let mut words = vec![0u64; 7];
    let answer = run::<A>(&held, &mut words, 4);
    assert_eq!(answer.outcome, Outcome::Raised);
    assert_eq!(answer.raise, None, "the runtime holds the error, not this");

    // And so does a stop.
    forget_calls();
    calls_answer(&[Outcome::Stopped]);
    let mut words = vec![0u64; 7];
    assert_eq!(run::<A>(&held, &mut words, 4).outcome, Outcome::Stopped);
}

/// A reference is in its slot at every safepoint, and that is checked rather
/// than reasoned about.
///
/// ADR 0055's "Collection uses the VM stack as the first root map" is only true
/// if every live reference is materialised in its Cove slot before a safepoint.
/// Neither arm register-promotes, so it *should* be true at every instruction
/// boundary — but "should" is what this test is for: the safepoint helper stands
/// where the collector stands and reads the frame word the reference lives in.
///
/// The loop is what makes there be safepoints at all: they go on backedges, and a
/// function with no loop reaches one only at a call. So this is a counting loop
/// that never touches the reference, which is the case a register allocator would
/// get wrong — a value nothing in the loop reads is exactly the one an optimiser
/// would be happiest to leave somewhere else.
pub fn a_reference_is_in_its_slot_at_every_safepoint<A: Arm>() {
    forget_polls();
    let held = program(function(
        vec![Repr::Ref, Repr::Int, Repr::Int, Repr::Bool],
        INT,
        vec![
            Inst::Int { dst: 2, value: 0 },
            Inst::Cmp {
                on: Compare::Int,
                op: CmpOp::Lt,
                dst: 3,
                a: 2,
                b: 1,
            },
            Inst::BranchFalse { cond: 3, to: 5 },
            Inst::ArithImm {
                op: ArithOp::Add,
                dst: 2,
                a: 2,
                value: 1,
            },
            Inst::Jump { to: 1 },
            Inst::Return { src: 2 },
        ],
    ));

    // The frame does not begin at word zero, so a slot address formed as if it
    // did would read the wrong word and this test would notice.
    let base = 3usize;
    let sentinel = 0x1234_5678_9abc_def0u64;
    let mut words = vec![0u64; base + 4];
    words[base] = sentinel;
    words[base + 1] = 5;
    // Slot 0 of the frame, which is where the reference is and where the frame's
    // static `refs` map says a collector will look for it.
    watch(base);

    let answer = run::<A>(&held, &mut words, base as u64);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words[base + 2], 5, "the loop ran");
    assert_eq!(
        words[base], sentinel,
        "the reference is still in its slot when the function leaves"
    );
    let saw = watched();
    assert_eq!(saw.len(), 5, "one safepoint per backedge");
    assert!(
        saw.iter().all(|word| *word == sentinel),
        "the reference was in its slot at every safepoint, and what was seen was {saw:?}"
    );
}

// --- the address family and `clear` ------------------------------------------

/// `encoded.rs`'s `CLEAR` arm (line 1138): `clear_words(base + slot, width)`.
///
/// The instruction whose whole purpose is what it stops happening — a reference
/// the frame no longer needs is not a root — so what is asserted is *zero*, at
/// every word the layout names and at no word beside them. A clear that missed a
/// word would leave a stale address for the collector to follow, and a clear that
/// wrote one too many would zero a live slot.
///
/// Three widths, because the bug in each direction is a different bug: one word
/// is a reference, two are a `struct { n: Int, s: String }` whose second word is
/// the one that matters, and none is an empty struct — for which `clear_words`
/// returns before it does anything and so must this.
pub fn a_clear_zeroes_the_words_its_layout_names<A: Arm>() {
    let held = |slot: Slot, layout: LayoutId| {
        program(function(
            vec![Repr::Int, Repr::Ref, Repr::Ref, Repr::Int],
            INT,
            vec![Inst::Clear { slot, layout }, Inst::Return { src: 3 }],
        ))
    };
    let full = [0x1111u64, 0x2222, 0x3333, 7];

    for (slot, layout, after) in [
        (1u32, REF, [0x1111u64, 0, 0x3333, 7]),
        (1, REF_PAIR, [0x1111, 0, 0, 7]),
        (1, EMPTY, [0x1111, 0x2222, 0x3333, 7]),
    ] {
        forget_polls();
        let mut words = full.to_vec();
        let answer = run::<A>(&held(slot, layout), &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned, "clear at {slot}");
        assert_eq!(words, after.to_vec(), "clear at {slot}");
    }

    // And at a frame that does not begin at word zero, because a clear whose
    // address was formed as if it did would zero somebody else's words.
    forget_polls();
    let mut words = vec![0xfeedu64, 0x1111, 0x2222, 0x3333, 7];
    let answer = run::<A>(&held(1, REF_PAIR), &mut words, 1);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(words, vec![0xfeed, 0x1111, 0, 0, 7]);
}

/// `encoded.rs`'s `ADDR_OF_SLOT` arm (line 1699): `base + slot`, where `base` is
/// the frame's **linear address**.
///
/// The thing this catches is the one mistake the ABI's shape makes easy. The entry
/// point is handed the frame as a *segment-relative index*, because the stack's
/// `Vec` moves and the index does not; a `Repr::Addr` word is a linear address,
/// which is that index plus the segment's origin. An arm that wrote the index
/// would produce a word that is wrong by a million and that the VM would follow
/// into another task's segment — and it would be *right* on the first segment,
/// which is why [`SEGMENT_ORIGIN`] is not zero.
pub fn an_address_of_a_slot_is_the_linear_address_of_it<A: Arm>() {
    for (base, slot) in [(0u64, 0u32), (0, 2), (3, 1)] {
        forget_polls();
        let held = program(function(
            vec![Repr::Int, Repr::Addr, Repr::Int, Repr::Int],
            ADDR,
            vec![Inst::AddrOfSlot { dst: 1, slot }, Inst::Return { src: 1 }],
        ));
        let mut words = vec![0u64; base as usize + 4];
        let answer = run::<A>(&held, &mut words, base);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(
            words[base as usize + 1],
            SEGMENT_ORIGIN + base + u64::from(slot),
            "the address of slot {slot} of the frame at {base}"
        );
    }
}

/// `encoded.rs`'s `ADDR_OF_PART` arm (line 1729), whose own comment is the whole
/// of it: "Arithmetic and nothing else."
///
/// A place is the address of the *first* word of a value location, so a part of
/// one is at a static word offset from it — and that holds whichever region the
/// address names, which is why one arm serves a field of a stack local and a field
/// of a heap object alike.
pub fn an_address_of_a_part_is_one_addition<A: Arm>() {
    for (addr, at) in [
        (SEGMENT_ORIGIN + 7, 0u32),
        (SEGMENT_ORIGIN + 7, 3),
        (HEAP_ORIGIN_WORDS + 9, 2),
    ] {
        forget_polls();
        let held = program(function(
            vec![Repr::Addr, Repr::Addr],
            ADDR,
            vec![
                Inst::AddrOfPart {
                    dst: 1,
                    addr: 0,
                    at,
                },
                Inst::Return { src: 1 },
            ],
        ));
        let mut words = vec![addr, 0];
        let answer = run::<A>(&held, &mut words, 0);
        assert_eq!(answer.outcome, Outcome::Returned);
        assert_eq!(words[1], addr + u64::from(at), "{addr} + {at}");
    }
}

/// `encoded.rs`'s `LOAD` and `STORE` arms (lines 1735 and 1740), which are
/// `Memory::copy_words` through an address — **and so are the `is_stack(addr)`
/// branch in front of it**.
///
/// One address type names two regions, and that is the whole of what these two
/// instructions are for: `bump(var total)` writes a local through the same
/// instruction pair that `piece.count = 1` writes a field with. So each direction
/// is asked twice, once at an address in this frame and once at an address in the
/// heap, and an arm that decoded the region wrongly reads or writes a word a
/// billion places from the right one.
///
/// The heap object straddles a chunk boundary for [`a_load_elem_strides_and_bounds_its_index`]'s
/// reason: a two-word run whose words are in different chunks is the case a
/// pointer formed once and reused would get wrong.
pub fn a_load_and_a_store_reach_either_region<A: Arm>() {
    // `s1 = *s0` at two words, and then the answer.
    let loads = program(function(
        vec![Repr::Addr, Repr::Int, Repr::Int],
        PAIR,
        vec![
            Inst::Load {
                dst: 1,
                addr: 0,
                layout: PAIR,
            },
            Inst::Return { src: 1 },
        ],
    ));
    // `*s0 = s1`, and an answer that says nothing, so what is asserted is the
    // words the store left behind.
    let stores = program(function(
        vec![Repr::Addr, Repr::Int, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::Int { dst: 3, value: 0 },
            Inst::Store {
                addr: 0,
                src: 1,
                layout: PAIR,
            },
            Inst::Return { src: 3 },
        ],
    ));

    // --- the stack ----------------------------------------------------------
    //
    // The address names word 4 of the segment, which is slot 4 of the frame at
    // zero: a load through it is a load of the frame's own words, which is what a
    // `var` parameter naming a caller's local is.
    forget_polls();
    let mut words = vec![SEGMENT_ORIGIN + 4, 0, 0, 0, 101, 102];
    let answer = run::<A>(&loads, &mut words, 0);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!((words[1], words[2]), (101, 102), "loaded off the stack");
    assert_eq!(answer.returned, [101, 102, UNWRITTEN, UNWRITTEN]);

    forget_polls();
    let mut words = vec![SEGMENT_ORIGIN + 4, 201, 202, 0, 0, 0];
    let answer = run::<A>(&stores, &mut words, 0);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!((words[4], words[5]), (201, 202), "stored onto the stack");
    assert_eq!(words[3], 0, "and nothing beside them");

    // A frame that does not begin at word zero, and an address into word zero of
    // the segment, below it: the caller's frame is where a `var` parameter's
    // target actually is.
    forget_polls();
    let mut words = vec![0, 0, SEGMENT_ORIGIN, 301, 302, 0, 0];
    let answer = run::<A>(&stores, &mut words, 2);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!((words[0], words[1]), (301, 302), "stored below the frame");

    // --- the heap -----------------------------------------------------------
    let at = HEAP_CHUNK_WORDS - 1;
    let build = || {
        let mut heap = Heap::new(2);
        heap.set(at, 401);
        heap.set(at + 1, 402);
        heap
    };

    forget_polls();
    let heap = build();
    let mut words = vec![heap.addr(at), 0, 0];
    let answer = run_over::<A>(&loads, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        (words[1], words[2]),
        (401, 402),
        "loaded across a chunk boundary"
    );

    forget_polls();
    let mut heap = build();
    let mut words = vec![heap.addr(at), 501, 502, 0];
    let answer = run_over::<A>(&stores, &mut words, 0, &heap);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        (heap.get(at), heap.get(at + 1)),
        (501, 502),
        "stored across a chunk boundary"
    );
    heap.set(at, 0);

    // --- a run that overlaps itself -----------------------------------------
    //
    // `Memory::copy_words` is a `memmove`, and an address formed by
    // `addr-of-slot` from *this* frame is what makes the overlapping case
    // reachable: a forward run of load-store pairs would smear `[601, 602]` into
    // `[601, 601]`.
    forget_polls();
    let overlapping = program(function(
        vec![Repr::Addr, Repr::Int, Repr::Int, Repr::Int],
        INT,
        vec![
            Inst::Int { dst: 3, value: 0 },
            Inst::AddrOfSlot { dst: 0, slot: 2 },
            Inst::Store {
                addr: 0,
                src: 1,
                layout: PAIR,
            },
            Inst::Return { src: 3 },
        ],
    ));
    let mut words = vec![0u64, 601, 602, 0];
    let answer = run::<A>(&overlapping, &mut words, 0);
    assert_eq!(answer.outcome, Outcome::Returned);
    assert_eq!(
        (words[2], words[3]),
        (601, 602),
        "the run moved rather than smearing"
    );
}
