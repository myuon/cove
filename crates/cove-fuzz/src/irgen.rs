//! A generator of [`cove_ir::Program`]s, for the `ir` target ([ADR 0120]).
//!
//! The source generator ([`crate::generate`]) can only say what Cove says, and
//! the lowering only writes what it was written to write. The machine's
//! contract is about every `Program`, so this builds them directly: a layout
//! table, a handful of functions, and in each a frame and a body, from the
//! fuzzer's bytes.
//!
//! # Built by `Repr` and by layout, so that most programs verify
//!
//! A program `cove_ir::verify` refuses tests one thing — that the machine
//! refuses it too — and a generator whose programs were all refused would
//! test nothing else. So every operand is chosen from the frame by what the
//! instruction needs of it: an `Int` slot for an index, a location of the
//! layout for a copy, the callee's own parameter layouts for a call. A slot
//! the frame does not have yet is added, at the end, with the `Repr` asked
//! for. Jumps land on an instruction, tables name instructions, every body
//! ends in a `Return` of the function's own answer, and ADR 0062's window is
//! emitted in the one shape the reservation rule admits. What the generator
//! does **not** do is keep the program meaningful: a load reads whatever is
//! at the offset it names, a closure call calls whatever object the slot
//! holds, an index is whatever the `Int` slot holds. Those are the machine's
//! questions, at run time, and asking them is what the target is for.
//!
//! Now and then — [`Gen::wild`] — an operand is chosen with no regard to what
//! the instruction needs, so that the refusals are exercised too.
//!
//! # Every instruction
//!
//! [`Kind`] names every [`Inst`] variant, and [`kind_of`] maps an instruction
//! to its kind with no wildcard arm: a new instruction does not compile until
//! it is given a kind, and [`Body::emit`] does not compile until the kind has
//! a way to be generated. `tests/regressions.rs` holds a fixed set of inputs
//! to reaching every kind, and to a share of programs that verify.
//!
//! [ADR 0120]: ../../../docs/adr/0120-the-vm-runs-only-what-verifies.md

use std::collections::BTreeMap;
use std::sync::Arc;

use arbitrary::Unstructured;
use cove_diag::{FileId, SourceMap, Span};
use cove_ir::{
    enum_layout, struct_layout, Arg as IrArg, ArgsId, ArithOp, BitOp, Capture, CaseId, CmpOp,
    Compare, Convert, Function, FunctionId, HostOp, HostOpId, Inst, Layout, LayoutId, Len,
    MinMax, Num, Program, RefMap, Repr, Shape, ShiftOp, Slot, Storage, StrId, Table, TableId,
    Validation,
};

use crate::ir::{Arg, Subject};

/// The module every generated function is declared in.
pub const MODULE: &str = "fuzz";
/// The entry, function 0.
pub const ENTRY: &str = "main";

// The layouts every generated program begins with, at these ids: the table
// `cove_ir::lower`'s `Shapes::new` seeds, then the identity table and set, then
// the few families a host's answer and a closure need.
pub const FREE: LayoutId = LayoutId(0);
pub const STR: LayoutId = LayoutId(1);
pub const UNIT: LayoutId = LayoutId(2);
pub const BOOL: LayoutId = LayoutId(3);
pub const INT: LayoutId = LayoutId(4);
pub const FLOAT: LayoutId = LayoutId(5);
pub const DURATION: LayoutId = LayoutId(6);
pub const REF: LayoutId = LayoutId(7);
pub const ADDR: LayoutId = LayoutId(8);
pub const HOST: LayoutId = LayoutId(9);
pub const BOXED: LayoutId = LayoutId(10);
pub const TASK: LayoutId = LayoutId(11);
pub const SCOPE: LayoutId = LayoutId(12);
pub const TAG: LayoutId = LayoutId(13);
pub const BYTES: LayoutId = LayoutId(14);
pub const BYTE_BUFFER: LayoutId = LayoutId(15);
pub const VIEW: LayoutId = LayoutId(16);
pub const PATH: LayoutId = LayoutId(17);
pub const IDENTITY_TABLE: LayoutId = LayoutId(18);
pub const IDENTITY_SET: LayoutId = LayoutId(19);
pub const ERROR: LayoutId = LayoutId(20);
pub const FN_VALUE: LayoutId = LayoutId(21);
pub const RESULT_UNIT: LayoutId = LayoutId(22);
pub const OPTION_INT: LayoutId = LayoutId(23);
pub const ARRAY_STR: LayoutId = LayoutId(24);
/// How many layouts [`base_layouts`] answers.
pub const BASE_LAYOUTS: usize = 25;

/// The table every generated program starts from.
pub fn base_layouts() -> Vec<Layout> {
    let mut layouts = vec![
        Layout::free(),
        Layout::object("String", Shape::Str),
        Layout::word("Unit", Repr::Unit),
        Layout::word("Bool", Repr::Bool),
        Layout::word("Int", Repr::Int),
        Layout::word("Float", Repr::Float),
        Layout::word("Duration", Repr::Duration),
        Layout::word("<ref>", Repr::Ref),
        Layout::word("<addr>", Repr::Addr),
        Layout::word("<host>", Repr::Host),
        Layout::object("Any", Shape::Boxed),
        Layout::word("Task", Repr::Task),
        Layout::word("TaskScope", Repr::Scope),
        Layout::word("<tag>", Repr::Tag),
        Layout::object("Bytes", Shape::Bytes),
        Layout::object("ByteBuffer", Shape::ByteBuffer),
        cove_ir::dynamic::view_layout(INT, REF),
        cove_ir::dynamic::render_path_layout(ADDR, INT),
        cove_ir::dynamic::identity_table_layout(),
    ];
    layouts.push(cove_ir::dynamic::identity_set_layout(IDENTITY_TABLE, BOOL));
    let (fields, words) = struct_layout(&[(Arc::from("message"), STR)], &layouts);
    layouts.push(Layout::inline(
        "Error",
        Shape::Struct {
            fields,
            opaque: false,
        },
        words,
    ));
    layouts.push(Layout::word("fn", Repr::Ref));
    layouts.push(enum_shape(
        "Result",
        &[(Arc::from("Ok"), vec![UNIT]), (Arc::from("Err"), vec![ERROR])],
        &layouts,
    ));
    layouts.push(enum_shape(
        "Option",
        &[(Arc::from("None"), vec![]), (Arc::from("Some"), vec![INT])],
        &layouts,
    ));
    layouts.push(Layout::object(
        "Array",
        Shape::Elements {
            elem: STR,
            growable: false,
        },
    ));
    debug_assert_eq!(layouts.len(), BASE_LAYOUTS);
    layouts
}

/// An enum's layout as the lowering lays one out: a tag, then the payload
/// region its cases share.
fn enum_shape(name: &str, cases: &[(Arc<str>, Vec<LayoutId>)], layouts: &[Layout]) -> Layout {
    let (cases, payload) = enum_layout(cases, layouts);
    let mut words = Vec::with_capacity(1 + payload.len());
    words.push(Repr::Tag);
    words.extend_from_slice(&payload);
    Layout::inline(name, Shape::Enum { cases, payload }, words)
}

/// Integers a generated program is most likely to go wrong at.
const EDGES: [i64; 22] = [
    0,
    1,
    -1,
    2,
    7,
    8,
    63,
    64,
    255,
    256,
    -256,
    1 << 16,
    (1 << 31) - 1,
    1 << 31,
    1 << 32,
    (1 << 53) + 1,
    i64::MAX,
    i64::MIN,
    i64::MAX - 1,
    i64::MIN + 1,
    u32::MAX as i64,
    -(1 << 40),
];

/// Floats likewise, by their bits.
const FLOAT_EDGES: [u64; 12] = [
    0,
    0x8000_0000_0000_0000,
    0x3ff0_0000_0000_0000,
    0xbff0_0000_0000_0000,
    0x7ff0_0000_0000_0000,
    0xfff0_0000_0000_0000,
    0x7ff8_0000_0000_0000,
    0xfff0_0000_dead_beef,
    0x0000_0000_0000_0001,
    0x7fef_ffff_ffff_ffff,
    0x43e0_0000_0000_0000,
    0x4330_0000_0000_0001,
];

/// String literals: empty, one to four bytes a character, a combining mark,
/// one long enough to span words.
const TEXTS: [&str; 7] = [
    "",
    "a",
    "héllo",
    "😀x",
    "e\u{301}",
    "the quick brown fox jumps over the lazy dog",
    "\u{0}\u{7f}\u{80}\u{7ff}\u{800}\u{ffff}\u{10000}\u{10ffff}",
];

/// The most words a generated frame grows to.
const MAX_FRAME: usize = 192;

/// What an instruction is, without its operands: one per [`Inst`] variant,
/// and a few shapes of more than one instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Unit,
    Bool,
    Int,
    FuncRef,
    Tag,
    Float,
    Str,
    Copy,
    Clear,
    Neg,
    Arith,
    Cmp,
    ArithImm,
    CmpImm,
    Not,
    Convert,
    FloatAbs,
    FloatMinMax,
    FloatRound,
    FloatSqrt,
    FloatTruncate,
    Bits,
    BitNot,
    Shift,
    Jump,
    BranchFalse,
    CmpBranch,
    CmpImmBranch,
    Switch,
    Return,
    Call,
    CallClosure,
    CallHost,
    CallResource,
    Alloc,
    LoadField,
    StoreField,
    LoadElem,
    StoreElem,
    RunLoad,
    RunCopy,
    RunSlice,
    RunFind,
    GrowableAlloc,
    GrowableEnsure,
    GrowableCommit,
    RunStore,
    GrowableTruncate,
    RunFinish,
    Len,
    AddrOfSlot,
    AddrOfField,
    AddrOfPart,
    Load,
    Store,
    Box,
    Unbox,
    DynOpen,
    DynKind,
    DynSameType,
    DynSameObject,
    DynIdentitySet,
    DynIdentityEnter,
    DynIdentityLeave,
    DynNameOrder,
    DynRead,
    DynCase,
    DynCount,
    DynChild,
    HandleText,
    DynTypeName,
    DynFieldName,
    DynCaseName,
    DynOpaque,
    DynHandleText,
    DynOnPath,
    ScopeEnter,
    ScopeLeave,
    ScopeCancel,
    Spawn,
    Await,
    Cancel,
    Settled,
    SharedLock,
    SharedUnlock,
    Trap,
    AssertFailed,
}

/// Every kind, in the order a choice byte indexes them.
pub const KINDS: [Kind; 87] = [
    Kind::Unit,
    Kind::Bool,
    Kind::Int,
    Kind::FuncRef,
    Kind::Tag,
    Kind::Float,
    Kind::Str,
    Kind::Copy,
    Kind::Clear,
    Kind::Neg,
    Kind::Arith,
    Kind::Cmp,
    Kind::ArithImm,
    Kind::CmpImm,
    Kind::Not,
    Kind::Convert,
    Kind::FloatAbs,
    Kind::FloatMinMax,
    Kind::FloatRound,
    Kind::FloatSqrt,
    Kind::FloatTruncate,
    Kind::Bits,
    Kind::BitNot,
    Kind::Shift,
    Kind::Jump,
    Kind::BranchFalse,
    Kind::CmpBranch,
    Kind::CmpImmBranch,
    Kind::Switch,
    Kind::Return,
    Kind::Call,
    Kind::CallClosure,
    Kind::CallHost,
    Kind::CallResource,
    Kind::Alloc,
    Kind::LoadField,
    Kind::StoreField,
    Kind::LoadElem,
    Kind::StoreElem,
    Kind::RunLoad,
    Kind::RunCopy,
    Kind::RunSlice,
    Kind::RunFind,
    Kind::GrowableAlloc,
    Kind::GrowableEnsure,
    Kind::GrowableCommit,
    Kind::RunStore,
    Kind::GrowableTruncate,
    Kind::RunFinish,
    Kind::Len,
    Kind::AddrOfSlot,
    Kind::AddrOfField,
    Kind::AddrOfPart,
    Kind::Load,
    Kind::Store,
    Kind::Box,
    Kind::Unbox,
    Kind::DynOpen,
    Kind::DynKind,
    Kind::DynSameType,
    Kind::DynSameObject,
    Kind::DynIdentitySet,
    Kind::DynIdentityEnter,
    Kind::DynIdentityLeave,
    Kind::DynNameOrder,
    Kind::DynRead,
    Kind::DynCase,
    Kind::DynCount,
    Kind::DynChild,
    Kind::HandleText,
    Kind::DynTypeName,
    Kind::DynFieldName,
    Kind::DynCaseName,
    Kind::DynOpaque,
    Kind::DynHandleText,
    Kind::DynOnPath,
    Kind::ScopeEnter,
    Kind::ScopeLeave,
    Kind::ScopeCancel,
    Kind::Spawn,
    Kind::Await,
    Kind::Cancel,
    Kind::Settled,
    Kind::SharedLock,
    Kind::SharedUnlock,
    Kind::Trap,
    Kind::AssertFailed,
];

/// The kind of `inst`. No wildcard arm: a new [`Inst`] variant is a compile
/// error here until it is given a [`Kind`], and then one in [`Body::emit`]
/// until it is generated.
pub fn kind_of(inst: &Inst) -> Kind {
    match inst {
        Inst::Unit { .. } => Kind::Unit,
        Inst::Bool { .. } => Kind::Bool,
        Inst::Int { .. } => Kind::Int,
        Inst::FuncRef { .. } => Kind::FuncRef,
        Inst::Tag { .. } => Kind::Tag,
        Inst::Float { .. } => Kind::Float,
        Inst::Str { .. } => Kind::Str,
        Inst::Copy { .. } => Kind::Copy,
        Inst::Clear { .. } => Kind::Clear,
        Inst::Neg { .. } => Kind::Neg,
        Inst::Arith { .. } => Kind::Arith,
        Inst::Cmp { .. } => Kind::Cmp,
        Inst::ArithImm { .. } => Kind::ArithImm,
        Inst::CmpImm { .. } => Kind::CmpImm,
        Inst::Not { .. } => Kind::Not,
        Inst::Convert { .. } => Kind::Convert,
        Inst::FloatAbs { .. } => Kind::FloatAbs,
        Inst::FloatMinMax { .. } => Kind::FloatMinMax,
        Inst::FloatRound { .. } => Kind::FloatRound,
        Inst::FloatSqrt { .. } => Kind::FloatSqrt,
        Inst::FloatTruncate { .. } => Kind::FloatTruncate,
        Inst::Bits { .. } => Kind::Bits,
        Inst::BitNot { .. } => Kind::BitNot,
        Inst::Shift { .. } => Kind::Shift,
        Inst::Jump { .. } => Kind::Jump,
        Inst::BranchFalse { .. } => Kind::BranchFalse,
        Inst::CmpBranch { .. } => Kind::CmpBranch,
        Inst::CmpImmBranch { .. } => Kind::CmpImmBranch,
        Inst::Switch { .. } => Kind::Switch,
        Inst::Return { .. } => Kind::Return,
        Inst::Call { .. } => Kind::Call,
        Inst::CallClosure { .. } => Kind::CallClosure,
        Inst::CallHost { .. } => Kind::CallHost,
        Inst::CallResource { .. } => Kind::CallResource,
        Inst::Alloc { .. } => Kind::Alloc,
        Inst::LoadField { .. } => Kind::LoadField,
        Inst::StoreField { .. } => Kind::StoreField,
        Inst::LoadElem { .. } => Kind::LoadElem,
        Inst::StoreElem { .. } => Kind::StoreElem,
        Inst::RunLoad { .. } => Kind::RunLoad,
        Inst::RunCopy { .. } => Kind::RunCopy,
        Inst::RunSlice { .. } => Kind::RunSlice,
        Inst::RunFind { .. } => Kind::RunFind,
        Inst::GrowableAlloc { .. } => Kind::GrowableAlloc,
        Inst::GrowableEnsure { .. } => Kind::GrowableEnsure,
        Inst::GrowableCommit { .. } => Kind::GrowableCommit,
        Inst::RunStore { .. } => Kind::RunStore,
        Inst::GrowableTruncate { .. } => Kind::GrowableTruncate,
        Inst::RunFinish { .. } => Kind::RunFinish,
        Inst::Len { .. } => Kind::Len,
        Inst::AddrOfSlot { .. } => Kind::AddrOfSlot,
        Inst::AddrOfField { .. } => Kind::AddrOfField,
        Inst::AddrOfPart { .. } => Kind::AddrOfPart,
        Inst::Load { .. } => Kind::Load,
        Inst::Store { .. } => Kind::Store,
        Inst::Box { .. } => Kind::Box,
        Inst::Unbox { .. } => Kind::Unbox,
        Inst::DynOpen { .. } => Kind::DynOpen,
        Inst::DynKind { .. } => Kind::DynKind,
        Inst::DynSameType { .. } => Kind::DynSameType,
        Inst::DynSameObject { .. } => Kind::DynSameObject,
        Inst::DynIdentitySet { .. } => Kind::DynIdentitySet,
        Inst::DynIdentityEnter { .. } => Kind::DynIdentityEnter,
        Inst::DynIdentityLeave { .. } => Kind::DynIdentityLeave,
        Inst::DynNameOrder { .. } => Kind::DynNameOrder,
        Inst::DynRead { .. } => Kind::DynRead,
        Inst::DynCase { .. } => Kind::DynCase,
        Inst::DynCount { .. } => Kind::DynCount,
        Inst::DynChild { .. } => Kind::DynChild,
        Inst::HandleText { .. } => Kind::HandleText,
        Inst::DynTypeName { .. } => Kind::DynTypeName,
        Inst::DynFieldName { .. } => Kind::DynFieldName,
        Inst::DynCaseName { .. } => Kind::DynCaseName,
        Inst::DynOpaque { .. } => Kind::DynOpaque,
        Inst::DynHandleText { .. } => Kind::DynHandleText,
        Inst::DynOnPath { .. } => Kind::DynOnPath,
        Inst::ScopeEnter { .. } => Kind::ScopeEnter,
        Inst::ScopeLeave { .. } => Kind::ScopeLeave,
        Inst::ScopeCancel { .. } => Kind::ScopeCancel,
        Inst::Spawn { .. } => Kind::Spawn,
        Inst::Await { .. } => Kind::Await,
        Inst::Cancel { .. } => Kind::Cancel,
        Inst::Settled { .. } => Kind::Settled,
        Inst::SharedLock { .. } => Kind::SharedLock,
        Inst::SharedUnlock { .. } => Kind::SharedUnlock,
        Inst::Trap { .. } => Kind::Trap,
        Inst::AssertFailed { .. } => Kind::AssertFailed,
    }
}

/// A generated program, from the fuzzer's bytes.
pub fn from_bytes(data: &[u8]) -> Subject {
    Gen::new(data).program()
}

/// A function's signature, decided before any body is.
#[derive(Clone, Debug)]
struct Sig {
    params: Vec<LayoutId>,
    returns: LayoutId,
    /// The layouts of its captures, for a function a closure layout names.
    captures: Vec<LayoutId>,
    is_async: bool,
}

/// The program-wide state of one generation.
pub struct Gen<'a> {
    u: Unstructured<'a>,
    layouts: Vec<Layout>,
    strings: Vec<Arc<str>>,
    args: Vec<Vec<IrArg>>,
    tables: Vec<Table>,
    host_ops: Vec<HostOp>,
    sigs: Vec<Sig>,
    /// Layouts a frame location may have: scalars, one-word references, and
    /// the inline structs and enums.
    values: Vec<LayoutId>,
    /// Enum layouts, for a tag.
    enums: Vec<LayoutId>,
    /// Layouts an `Alloc` may name: every object family, and the inline
    /// layouts, which allocate a fixed payload of their own words.
    objects: Vec<LayoutId>,
    /// Element layouts some `Elements`, `Vector` or keyed run is of.
    elements: Vec<LayoutId>,
    /// `Shape::Closure` layouts, and the function each names.
    closures: Vec<(LayoutId, FunctionId)>,
    /// Which tables each function's switches own, to patch their targets
    /// once the function's length is known.
    owned_tables: Vec<Vec<TableId>>,
    /// `Shape::Vector` layouts and their elements: a word `GrowableAlloc`
    /// allocates the vector its element has.
    vectors: Vec<(LayoutId, LayoutId)>,
    /// See [`Gen::wild`].
    wild_programs: bool,
}

impl<'a> Gen<'a> {
    /// A generation over `data`.
    pub fn new(data: &'a [u8]) -> Gen<'a> {
        let mut u = Unstructured::new(data);
        let wild_programs = u.ratio(1, 6).unwrap_or(false);
        Gen {
            u,
            vectors: Vec::new(),
            wild_programs,
            layouts: base_layouts(),
            strings: TEXTS.iter().map(|t| Arc::from(*t)).collect(),
            args: Vec::new(),
            tables: Vec::new(),
            host_ops: Vec::new(),
            sigs: Vec::new(),
            values: vec![
                UNIT, BOOL, INT, FLOAT, DURATION, STR, REF, HOST, BOXED, TASK, SCOPE, TAG, BYTES,
                BYTE_BUFFER, VIEW, ERROR, FN_VALUE, RESULT_UNIT, OPTION_INT, ARRAY_STR,
            ],
            enums: vec![RESULT_UNIT, OPTION_INT],
            // Not `BOXED`: a box is allocated by `Box`, which knows what goes
            // in it, and `verify` refuses an `Alloc` of one.
            objects: vec![STR, BYTES, BYTE_BUFFER, ARRAY_STR, IDENTITY_TABLE],
            elements: vec![STR],
            closures: Vec::new(),
            owned_tables: Vec::new(),
        }
    }

    // ---- choices ----------------------------------------------------------

    fn byte(&mut self) -> u8 {
        self.u.arbitrary().unwrap_or(0)
    }

    fn below(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        self.u.choose_index(n).unwrap_or(0)
    }

    fn chance(&mut self, numerator: u8, denominator: u8) -> bool {
        self.u.ratio(numerator, denominator).unwrap_or(false)
    }

    /// Whether this operand is chosen with no regard for what it should be.
    ///
    /// Decided per program first: one program in six is wild, and in it one
    /// operand in twelve. Spread over every program instead, nearly every one
    /// would hold one wild operand and be refused for it.
    fn wild(&mut self) -> bool {
        self.wild_programs && self.chance(1, 12)
    }

    fn int(&mut self) -> i64 {
        match self.byte() % 4 {
            0 => self.u.arbitrary().unwrap_or(0),
            _ => EDGES[self.below(EDGES.len())],
        }
    }

    fn float_bits(&mut self) -> u64 {
        match self.byte() % 3 {
            0 => self.u.arbitrary().unwrap_or(0),
            _ => FLOAT_EDGES[self.below(FLOAT_EDGES.len())],
        }
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.below(from.len())]
    }

    // ---- the program ------------------------------------------------------

    fn program(mut self) -> Subject {
        self.declare_layouts();
        self.declare_host_ops();
        let count = 1 + self.below(5);
        for at in 0..count {
            let sig = self.signature(at == 0);
            self.sigs.push(sig);
        }
        self.declare_closures();
        for _ in &self.sigs {
            self.owned_tables.push(Vec::new());
        }
        let mut functions = Vec::with_capacity(self.sigs.len());
        for at in 0..self.sigs.len() {
            functions.push(self.function(FunctionId(at as u32)));
        }
        for (at, function) in functions.iter().enumerate() {
            let len = function.code.len() as u32;
            for table in self.owned_tables[at].clone() {
                let held = &mut self.tables[table.index()];
                for to in held.targets.iter_mut() {
                    *to %= len;
                }
                held.default %= len;
            }
        }
        let mut by_name = BTreeMap::new();
        for (at, function) in functions.iter().enumerate() {
            by_name.insert(
                (function.module.clone(), function.name.clone()),
                FunctionId(at as u32),
            );
        }
        let entry_params = self.sigs[0].params.clone();
        let program = Program {
            functions,
            layouts: self.layouts,
            strings: self.strings,
            args: self.args,
            tables: self.tables,
            host_ops: self.host_ops,
            str_layout: STR,
            bytes_layout: BYTES,
            buffer_layout: BYTE_BUFFER,
            boxed_layout: BOXED,
            view_layout: VIEW,
            render_path_layout: PATH,
            identity_set_layout: IDENTITY_SET,
            identity_table_layout: IDENTITY_TABLE,
            names: Vec::new(),
            resource_names: Vec::new(),
            by_name,
        };
        let mut u = self.u;
        let args = entry_params
            .iter()
            .map(|layout| {
                if u.ratio(1, 8).unwrap_or(false) {
                    Arg::Unit
                } else {
                    match crate::ir::plausible_arg(&program, *layout) {
                        Arg::Int(_) => Arg::Int(u.arbitrary().unwrap_or(7)),
                        other => other,
                    }
                }
            })
            .collect();
        let mut sources = SourceMap::new();
        sources.add("fuzz/generated.ir", "x".repeat(4096));
        Subject {
            program,
            sources: Arc::new(sources),
            module: Arc::from(MODULE),
            name: Arc::from(ENTRY),
            args,
        }
    }

    /// Inline structs and enums over what exists, then object families over
    /// them.
    fn declare_layouts(&mut self) {
        for index in 0..self.below(4) {
            let fields = 1 + self.below(4);
            let mut declared = Vec::new();
            for field in 0..fields {
                let layout = self.value_for_field();
                declared.push((Arc::from(format!("f{field}").as_str()), layout));
            }
            let (fields, words) = struct_layout(&declared, &self.layouts);
            let opaque = self.chance(1, 6);
            let id = self.push_layout(Layout::inline(
                format!("S{index}"),
                Shape::Struct { fields, opaque },
                words,
            ));
            self.values.push(id);
            self.objects.push(id);
        }
        for index in 0..self.below(3) {
            let mut cases = Vec::new();
            for case in 0..1 + self.below(3) {
                let mut parts = Vec::new();
                for _ in 0..self.below(3) {
                    parts.push(self.value_for_field());
                }
                cases.push((Arc::from(format!("C{case}").as_str()), parts));
            }
            let layout = enum_shape(&format!("E{index}"), &cases, &self.layouts);
            let id = self.push_layout(layout);
            self.values.push(id);
            self.enums.push(id);
            self.objects.push(id);
        }
        // Scalars and the fixed-payload enums allocate as objects too.
        self.objects.extend([INT, FLOAT, OPTION_INT, RESULT_UNIT, ERROR]);
        for _ in 0..1 + self.below(4) {
            let elem = self.value_for_field();
            if !self.elements.contains(&elem) {
                self.elements.push(elem);
            }
            let shape = match self.byte() % 6 {
                0 => Shape::Elements {
                    elem,
                    growable: false,
                },
                1 => Shape::Elements {
                    elem,
                    growable: true,
                },
                2 => {
                    // A vector's store is found by the machine, so it is
                    // declared beside it, as the lowering declares it.
                    let store = Layout::object(
                        "Vector",
                        Shape::Elements {
                            elem,
                            growable: true,
                        },
                    );
                    self.push_layout(store);
                    let vector = self.push_layout(Layout::object("Vector", Shape::Vector { elem }));
                    self.vectors.push((vector, elem));
                    Shape::Vector { elem }
                }
                3 => Shape::Members { elem },
                4 => {
                    let value = self.value_for_field();
                    let (fields, words) = struct_layout(
                        &[(Arc::from("key"), elem), (Arc::from("value"), value)],
                        &self.layouts,
                    );
                    let entry = self.push_layout(Layout::inline(
                        "MapEntry",
                        Shape::Struct {
                            fields,
                            opaque: false,
                        },
                        words,
                    ));
                    self.values.push(entry);
                    if !self.elements.contains(&entry) {
                        self.elements.push(entry);
                    }
                    Shape::Entries { key: elem, value }
                }
                _ => Shape::Shared { value: elem },
            };
            let name = match &shape {
                Shape::Elements { growable: true, .. } => "Vector",
                Shape::Elements { .. } => "Array",
                Shape::Vector { .. } => "Vector",
                Shape::Members { .. } => "Set",
                Shape::Entries { .. } => "Map",
                _ => "Shared",
            };
            let id = self.push_layout(Layout::object(name, shape));
            self.values.push(id);
            self.objects.push(id);
        }
    }

    /// A layout a field, a part or an element may have.
    fn value_for_field(&mut self) -> LayoutId {
        let choices = [
            INT, INT, FLOAT, BOOL, STR, STR, UNIT, DURATION, TAG, HOST, BOXED, ERROR, OPTION_INT,
            FN_VALUE, ARRAY_STR, REF,
        ];
        match self.byte() % 5 {
            0 if self.values.len() > BASE_LAYOUTS - 5 => {
                let at = self.below(self.values.len());
                let id = self.values[at];
                // A location's layout, but never one so wide that a frame of a
                // few of them would not fit.
                if self.layouts[id.index()].width() <= 8 {
                    id
                } else {
                    INT
                }
            }
            _ => self.pick(&choices),
        }
    }

    fn push_layout(&mut self, layout: Layout) -> LayoutId {
        if let Some(at) = self.layouts.iter().position(|held| *held == layout) {
            return LayoutId(at as u32);
        }
        self.layouts.push(layout);
        LayoutId(self.layouts.len() as u32 - 1)
    }

    fn declare_host_ops(&mut self) {
        let op = |module: &str, operation: &str, resource: Option<&str>, result| HostOp {
            module: Arc::from(module),
            operation: Arc::from(operation),
            resource: resource.map(Arc::from),
            result,
        };
        self.host_ops = vec![
            op("console", "println", None, RESULT_UNIT),
            op("console", "print", None, RESULT_UNIT),
            op("console", "eprintln", None, RESULT_UNIT),
            op("clock", "now", None, INT),
            op("clock", "sleep", None, UNIT),
            op("console", "println", None, INT),
            op("files", "read", None, STR),
            op("nope", "missing", None, UNIT),
            op("files", "write", Some("Writer"), RESULT_UNIT),
            op("console", "println", Some("Line"), UNIT),
        ];
    }

    fn signature(&mut self, entry: bool) -> Sig {
        let mut params = Vec::new();
        let count = if entry {
            self.below(3)
        } else {
            self.below(4)
        };
        for _ in 0..count {
            let layout = if entry {
                self.pick(&[INT, INT, BOOL, FLOAT, STR, ARRAY_STR, DURATION])
            } else {
                self.value_for_field()
            };
            params.push(layout);
        }
        // `var` parameters, now and then: an address the caller formed.
        if !entry && self.chance(1, 5) {
            params.push(ADDR);
        }
        let returns = if entry {
            self.pick(&[UNIT, INT, STR, RESULT_UNIT, OPTION_INT, FLOAT, BOOL, ERROR])
        } else {
            self.value_for_field()
        };
        Sig {
            params,
            returns,
            captures: Vec::new(),
            is_async: false,
        }
    }

    /// Gives some functions captures and a `Shape::Closure` layout naming them.
    fn declare_closures(&mut self) {
        for at in 1..self.sigs.len() {
            if !self.chance(1, 2) {
                continue;
            }
            let mut captures = Vec::new();
            for _ in 0..self.below(3) {
                captures.push(self.value_for_field());
            }
            self.sigs[at].captures = captures.clone();
            self.sigs[at].is_async = self.chance(1, 4);
            let id = FunctionId(at as u32);
            let layout = self.push_layout(Layout::object(
                format!("closure{at}"),
                Shape::Closure {
                    function: id,
                    captures,
                },
            ));
            self.closures.push((layout, id));
            self.objects.push(layout);
        }
    }

    /// A layout's width; one word for a wild id past the table, which a
    /// location is still made for so that the instruction naming it is
    /// written and refused.
    fn width(&self, layout: LayoutId) -> u32 {
        self.layouts.get(layout.index()).map_or(1, Layout::width)
    }

    fn words(&self, layout: LayoutId) -> Vec<Repr> {
        self.layouts
            .get(layout.index())
            .map_or_else(|| vec![Repr::Int], |held| held.words.clone())
    }

    fn args_row(&mut self, row: Vec<IrArg>) -> ArgsId {
        if let Some(at) = self.args.iter().position(|held| *held == row) {
            return ArgsId(at as u32);
        }
        self.args.push(row);
        ArgsId(self.args.len() as u32 - 1)
    }

    fn string(&mut self) -> StrId {
        if self.chance(1, 8) {
            let text: String = self.u.arbitrary().unwrap_or_default();
            self.strings.push(Arc::from(text.as_str()));
            return StrId(self.strings.len() as u32 - 1);
        }
        StrId(self.below(self.strings.len()) as u32)
    }

    fn function(&mut self, id: FunctionId) -> Function {
        let sig = self.sigs[id.index()].clone();
        let mut body = Body::new(self, id, &sig);
        let steps = 2 + body.g.below(40);
        for _ in 0..steps {
            if body.reprs.len() >= MAX_FRAME {
                break;
            }
            let kind = KINDS[body.g.below(KINDS.len())];
            body.emit(kind);
        }
        body.finish(sig)
    }
}

/// One function under construction: its frame and its code.
struct Body<'g, 'a> {
    g: &'g mut Gen<'a>,
    id: FunctionId,
    reprs: Vec<Repr>,
    code: Vec<Inst>,
    /// Locations whose layout is known: the parameters, the captures, and
    /// every location an instruction was given as one.
    locs: Vec<(Slot, LayoutId)>,
    /// Instructions whose targets are still raw choices, resolved by
    /// [`Body::finish`] once the length is known.
    jumps: Vec<usize>,
    /// Reference slots an instruction of this body wrote an object into, and
    /// the layout of that object: what a field or an element access is
    /// mostly aimed at, so that a run gets past its first load.
    objs: Vec<(Slot, LayoutId)>,
    /// Slots no instruction but their own may write: an identity set's,
    /// which `verify` holds to having been written by a set instruction alone.
    reserved: Vec<bool>,
}

impl<'g, 'a> Body<'g, 'a> {
    fn new(g: &'g mut Gen<'a>, id: FunctionId, sig: &Sig) -> Body<'g, 'a> {
        let mut body = Body {
            g,
            id,
            reprs: Vec::new(),
            code: Vec::new(),
            locs: Vec::new(),
            jumps: Vec::new(),
            objs: Vec::new(),
            reserved: Vec::new(),
        };
        for layout in sig.params.iter().chain(&sig.captures) {
            body.fresh(*layout);
        }
        body.prologue();
        body
    }

    /// A few objects to work on, so that the body's loads, stores and calls
    /// reach something other than null.
    fn prologue(&mut self) {
        for _ in 0..1 + self.g.below(4) {
            let dst = self.fresh(REF);
            match self.g.byte() % 4 {
                0 => {
                    let text = self.g.string();
                    self.push(Inst::Str { dst, text });
                    self.objs.push((dst, STR));
                }
                1 if !self.g.vectors.is_empty() => {
                    let vectors = self.g.vectors.clone();
                    let (vector, elem) = self.g.pick(&vectors);
                    let capacity = self.fresh(INT);
                    let value = i64::from(self.g.byte() % 9);
                    self.push(Inst::Int {
                        dst: capacity,
                        value,
                    });
                    self.push(Inst::GrowableAlloc {
                        dst,
                        capacity,
                        storage: Storage::Words(elem),
                    });
                    self.objs.push((dst, vector));
                }
                2 => {
                    let capacity = self.fresh(INT);
                    let value = i64::from(self.g.byte() % 17);
                    self.push(Inst::Int {
                        dst: capacity,
                        value,
                    });
                    self.push(Inst::GrowableAlloc {
                        dst,
                        capacity,
                        storage: Storage::PackedBytes,
                    });
                    self.objs.push((dst, BYTE_BUFFER));
                }
                _ => {
                    let objects = self.g.objects.clone();
                    let layout = self.g.pick(&objects);
                    let len = Len::Count(u32::from(self.g.byte() % 6));
                    self.push(Inst::Alloc { dst, layout, len });
                    self.objs.push((dst, layout));
                }
            }
        }
    }

    /// An object operand: mostly one this body allocated, with its layout,
    /// and otherwise any reference slot.
    fn object(&mut self) -> (Slot, Option<LayoutId>) {
        if !self.objs.is_empty() && self.g.chance(3, 4) {
            let objs = self.objs.clone();
            let (slot, layout) = self.g.pick(&objs);
            return (slot, Some(layout));
        }
        (self.slot(Repr::Ref), None)
    }

    /// A field of an object of `layout` — an offset and the layout of what is
    /// there — where the object's shape says what its payload is.
    fn field_of(&mut self, layout: LayoutId) -> Option<(u32, LayoutId)> {
        let held = self.g.layouts.get(layout.index())?.clone();
        match &held.shape {
            Shape::Struct { fields, .. } if !fields.is_empty() => {
                let field = &fields[self.g.below(fields.len())];
                Some((field.at, field.layout))
            }
            Shape::Enum { cases, .. } => {
                if self.g.chance(1, 3) {
                    return Some((0, TAG));
                }
                let case = &cases[self.g.below(cases.len())];
                let part = case.parts.first()?;
                Some((1 + part.at, part.layout))
            }
            Shape::Word(_) => Some((0, layout)),
            Shape::Vector { .. } | Shape::ByteBuffer => {
                if self.g.chance(1, 2) {
                    Some((0, INT))
                } else {
                    Some((1, REF))
                }
            }
            Shape::Shared { value } => Some((1, *value)),
            Shape::Closure { captures, .. } => {
                if captures.is_empty() || self.g.chance(1, 3) {
                    return Some((0, INT));
                }
                let at = self.g.below(captures.len());
                let offset: u32 = captures[..at].iter().map(|c| self.g.width(*c)).sum();
                Some((1 + offset, captures[at]))
            }
            _ => None,
        }
    }

    /// The element layout of a run object of `layout`, if it is one.
    fn element_of(&self, layout: LayoutId) -> Option<LayoutId> {
        match &self.g.layouts.get(layout.index())?.shape {
            Shape::Elements { elem, .. } | Shape::Members { elem } => Some(*elem),
            Shape::Entries { key, value } => self
                .g
                .values
                .iter()
                .copied()
                .find(|id| cove_ir::is_entry_of(&self.g.layouts, *id, *key, *value)),
            _ => None,
        }
    }

    /// A new location of `layout` at the end of the frame.
    fn fresh(&mut self, layout: LayoutId) -> Slot {
        let at = self.reprs.len() as Slot;
        let words = self.g.words(layout);
        self.reserved.extend(words.iter().map(|_| layout == IDENTITY_SET));
        self.reprs.extend(words);
        self.locs.push((at, layout));
        at
    }

    /// A location of `layout`: one the frame has, or a new one.
    fn loc(&mut self, layout: LayoutId) -> Slot {
        if self.g.wild() && !self.reprs.is_empty() {
            return self.g.below(self.reprs.len() + 2) as Slot;
        }
        let known: Vec<Slot> = self
            .locs
            .iter()
            .filter(|(_, held)| *held == layout)
            .map(|(slot, _)| *slot)
            .collect();
        if !known.is_empty() && (self.reprs.len() >= MAX_FRAME || self.g.chance(3, 4)) {
            return self.g.pick(&known);
        }
        // Any run of slots whose words are the layout's is a location of it,
        // but for a set, which is one only where a set instruction made it.
        let words = self.g.words(layout);
        if !words.is_empty() && layout != IDENTITY_SET && self.g.chance(1, 3) {
            let fits: Vec<Slot> = (0..self.reprs.len().saturating_sub(words.len() - 1))
                .filter(|&at| self.reprs[at..at + words.len()] == words[..])
                .filter(|&at| !self.reserved[at..at + words.len()].contains(&true))
                .map(|at| at as Slot)
                .collect();
            if !fits.is_empty() {
                return self.g.pick(&fits);
            }
        }
        self.fresh(layout)
    }

    /// A slot holding a word of `repr`.
    fn slot(&mut self, repr: Repr) -> Slot {
        self.slot_of(&[repr])
    }

    /// A slot holding a word of one of `reprs`.
    fn slot_of(&mut self, reprs: &[Repr]) -> Slot {
        if self.g.wild() && !self.reprs.is_empty() {
            return self.g.below(self.reprs.len() + 2) as Slot;
        }
        let known: Vec<Slot> = (0..self.reprs.len())
            .filter(|&at| reprs.contains(&self.reprs[at]) && !self.reserved[at])
            .map(|at| at as Slot)
            .collect();
        if !known.is_empty() && (self.reprs.len() >= MAX_FRAME || self.g.chance(3, 4)) {
            return self.g.pick(&known);
        }
        let repr = self.g.pick(reprs);
        let at = self.reprs.len() as Slot;
        self.reprs.push(repr);
        self.reserved.push(false);
        at
    }

    fn layout_of_value(&mut self) -> LayoutId {
        if self.g.wild() {
            return LayoutId(self.g.below(self.g.layouts.len() + 1) as u32);
        }
        let values = self.g.values.clone();
        self.g.pick(&values)
    }

    fn push(&mut self, inst: Inst) {
        self.code.push(inst);
    }

    /// A raw jump target, resolved by [`Body::finish`].
    fn target(&mut self) -> u32 {
        // Mostly forward and near, so that most programs end: the low bit
        // says which way, the rest how far.
        let raw: u8 = self.g.byte();
        let here = self.code.len() as u32;
        if raw & 1 == 0 || raw & 0x0e == 0x0e {
            here + 1 + u32::from(raw >> 4)
        } else {
            here.saturating_sub(u32::from(raw >> 4))
        }
    }

    fn compare(&mut self) -> (Compare, &'static [Repr]) {
        match self.g.byte() % 6 {
            0 => (Compare::Float, &[Repr::Float]),
            1 => (Compare::Bool, &[Repr::Bool]),
            2 => (Compare::Str, &[Repr::Ref]),
            3 => (Compare::Identity, &[Repr::Ref, Repr::Int, Repr::Host]),
            4 => (Compare::Tag, &[Repr::Tag]),
            _ => (Compare::Int, &[Repr::Int, Repr::Duration]),
        }
    }

    fn cmp_op(&mut self, ordered: bool) -> CmpOp {
        let ops = [CmpOp::Eq, CmpOp::Ne, CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge];
        if ordered && self.g.chance(1, 5) {
            return CmpOp::Order;
        }
        self.g.pick(&ops)
    }

    fn arith_op(&mut self) -> ArithOp {
        self.g.pick(&[
            ArithOp::Add,
            ArithOp::Sub,
            ArithOp::Mul,
            ArithOp::Div,
            ArithOp::Rem,
        ])
    }

    fn storage(&mut self) -> Storage {
        if self.g.chance(1, 2) {
            Storage::PackedBytes
        } else {
            let elements = self.g.elements.clone();
            Storage::Words(self.g.pick(&elements))
        }
    }

    /// An argument row of `layouts`, each at a location of its own.
    fn row(&mut self, layouts: &[LayoutId]) -> ArgsId {
        let row = layouts
            .iter()
            .map(|layout| IrArg {
                slot: self.loc(*layout),
                layout: *layout,
            })
            .collect();
        self.g.args_row(row)
    }

    /// An argument row of slots chosen by `Repr`, each named by `layouts`.
    fn row_of(&mut self, wants: &[(Repr, LayoutId)]) -> ArgsId {
        let row = wants
            .iter()
            .map(|(repr, layout)| IrArg {
                slot: self.slot(*repr),
                layout: *layout,
            })
            .collect();
        self.g.args_row(row)
    }

    /// A function to call, and one a `FuncRef` names.
    fn callee(&mut self) -> FunctionId {
        FunctionId(self.g.below(self.g.sigs.len()) as u32)
    }

    /// A field offset into an object of `layout`'s payload, mostly inside it.
    fn offset(&mut self) -> u32 {
        match self.g.byte() % 8 {
            0 => self.g.u.arbitrary().unwrap_or(0),
            1..=3 => 0,
            4 => 1,
            _ => u32::from(self.g.byte() % 6),
        }
    }

    fn emit(&mut self, kind: Kind) {
        match kind {
            Kind::Unit => {
                let dst = self.slot(Repr::Unit);
                self.push(Inst::Unit { dst });
            }
            Kind::Bool => {
                let dst = self.slot(Repr::Bool);
                let value = self.g.chance(1, 2);
                self.push(Inst::Bool { dst, value });
            }
            Kind::Int => {
                let dst = self.slot_of(&[Repr::Int, Repr::Duration]);
                let value = self.g.int();
                self.push(Inst::Int { dst, value });
            }
            Kind::FuncRef => {
                let dst = self.slot(Repr::Int);
                let callee = self.callee();
                self.push(Inst::FuncRef { dst, callee });
            }
            Kind::Tag => {
                let enums = self.g.enums.clone();
                let layout = self.g.pick(&enums);
                let cases = match &self.g.layouts[layout.index()].shape {
                    Shape::Enum { cases, .. } => cases.len(),
                    _ => 1,
                };
                let past = usize::from(self.g.wild());
                let case = CaseId(self.g.below(cases + past) as u32);
                let dst = self.slot(Repr::Tag);
                self.push(Inst::Tag { dst, layout, case });
            }
            Kind::Float => {
                let dst = self.slot(Repr::Float);
                let bits = self.g.float_bits();
                self.push(Inst::Float { dst, bits });
            }
            Kind::Str => {
                let dst = self.slot(Repr::Ref);
                let text = self.g.string();
                self.push(Inst::Str { dst, text });
            }
            Kind::Copy => {
                let layout = self.layout_of_value();
                let src = self.loc(layout);
                let dst = self.loc(layout);
                self.push(Inst::Copy { dst, src, layout });
            }
            Kind::Clear => {
                let layout = self.layout_of_value();
                let slot = self.loc(layout);
                self.push(Inst::Clear { slot, layout });
            }
            Kind::Neg => {
                let (num, reprs): (Num, &[Repr]) = if self.g.chance(1, 2) {
                    (Num::Int, &[Repr::Int, Repr::Duration])
                } else {
                    (Num::Float, &[Repr::Float])
                };
                let a = self.slot_of(reprs);
                let dst = self.slot_of(reprs);
                self.push(Inst::Neg { num, dst, a });
            }
            Kind::Arith => {
                let (num, reprs): (Num, &[Repr]) = if self.g.chance(1, 2) {
                    (Num::Int, &[Repr::Int, Repr::Duration])
                } else {
                    (Num::Float, &[Repr::Float])
                };
                let op = self.arith_op();
                let a = self.slot_of(reprs);
                let b = self.slot_of(reprs);
                let dst = self.slot_of(reprs);
                self.push(Inst::Arith { num, op, dst, a, b });
            }
            Kind::Cmp => {
                let (on, reprs) = self.compare();
                let op = self.cmp_op(!matches!(on, Compare::Float | Compare::Identity));
                let a = self.slot_of(reprs);
                let b = self.slot_of(reprs);
                let dst = if op == CmpOp::Order {
                    self.slot(Repr::Int)
                } else {
                    self.slot(Repr::Bool)
                };
                self.push(Inst::Cmp { on, op, dst, a, b });
            }
            Kind::ArithImm => {
                let op = self.arith_op();
                let a = self.slot_of(&[Repr::Int, Repr::Duration]);
                let dst = self.slot_of(&[Repr::Int, Repr::Duration]);
                let value = self.g.int();
                self.push(Inst::ArithImm { op, dst, a, value });
            }
            Kind::CmpImm => {
                let op = self.cmp_op(false);
                let a = self.slot_of(&[Repr::Int, Repr::Duration]);
                let dst = self.slot(Repr::Bool);
                let value = self.g.int();
                self.push(Inst::CmpImm { op, dst, a, value });
            }
            Kind::Not => {
                let a = self.slot(Repr::Bool);
                let dst = self.slot(Repr::Bool);
                self.push(Inst::Not { dst, a });
            }
            Kind::Convert => {
                let (to, from, into) = match self.g.byte() % 3 {
                    0 => (Convert::IntToFloat, Repr::Int, Repr::Float),
                    1 => (Convert::DurationToInt, Repr::Duration, Repr::Int),
                    _ => (Convert::IntToDuration, Repr::Int, Repr::Duration),
                };
                let a = self.slot(from);
                let dst = self.slot(into);
                self.push(Inst::Convert { to, dst, a });
            }
            Kind::FloatAbs | Kind::FloatRound | Kind::FloatSqrt => {
                let a = self.slot(Repr::Float);
                let dst = self.slot(Repr::Float);
                self.push(match kind {
                    Kind::FloatAbs => Inst::FloatAbs { dst, a },
                    Kind::FloatRound => Inst::FloatRound { dst, a },
                    _ => Inst::FloatSqrt { dst, a },
                });
            }
            Kind::FloatMinMax => {
                let op = if self.g.chance(1, 2) {
                    MinMax::Min
                } else {
                    MinMax::Max
                };
                let a = self.slot(Repr::Float);
                let b = self.slot(Repr::Float);
                let dst = self.slot(Repr::Float);
                self.push(Inst::FloatMinMax { op, dst, a, b });
            }
            Kind::FloatTruncate => {
                let a = self.slot(Repr::Float);
                let dst = self.slot(Repr::Int);
                let ok = self.slot(Repr::Bool);
                self.push(Inst::FloatTruncate { dst, ok, a });
            }
            Kind::Bits => {
                let op = self.g.pick(&[BitOp::And, BitOp::Or, BitOp::Xor]);
                let a = self.slot(Repr::Int);
                let b = self.slot(Repr::Int);
                let dst = self.slot(Repr::Int);
                self.push(Inst::Bits { op, dst, a, b });
            }
            Kind::BitNot => {
                let a = self.slot(Repr::Int);
                let dst = self.slot(Repr::Int);
                self.push(Inst::BitNot { dst, a });
            }
            Kind::Shift => {
                let op = self
                    .g
                    .pick(&[ShiftOp::Left, ShiftOp::Right, ShiftOp::RightLogical]);
                let a = self.slot(Repr::Int);
                let n = self.slot(Repr::Int);
                let dst = self.slot(Repr::Int);
                self.push(Inst::Shift { op, dst, a, n });
            }
            Kind::Jump => {
                let to = self.target();
                self.jumps.push(self.code.len());
                self.push(Inst::Jump { to });
            }
            Kind::BranchFalse => {
                let cond = self.slot(Repr::Bool);
                let to = self.target();
                self.jumps.push(self.code.len());
                self.push(Inst::BranchFalse { cond, to });
            }
            Kind::CmpBranch => {
                let (on, reprs) = self.compare();
                let op = self.cmp_op(false);
                let a = self.slot_of(reprs);
                let b = self.slot_of(reprs);
                let dst = self.slot(Repr::Bool);
                let target = self.target();
                self.jumps.push(self.code.len());
                self.push(Inst::CmpBranch {
                    on,
                    op,
                    dst,
                    a,
                    b,
                    target,
                });
            }
            Kind::CmpImmBranch => {
                let op = self.cmp_op(false);
                let a = self.slot_of(&[Repr::Int, Repr::Duration]);
                let dst = self.slot(Repr::Bool);
                let value = self.g.int() as i32;
                let target = self.target();
                self.jumps.push(self.code.len());
                self.push(Inst::CmpImmBranch {
                    op,
                    dst,
                    a,
                    value,
                    target,
                });
            }
            Kind::Switch => {
                let on = self.slot_of(&[Repr::Tag, Repr::Int]);
                let mut targets = Vec::new();
                for _ in 0..self.g.below(5) {
                    targets.push(self.target());
                }
                let default = self.target();
                self.g.tables.push(Table { targets, default });
                let table = TableId(self.g.tables.len() as u32 - 1);
                self.g.owned_tables[self.id.index()].push(table);
                self.push(Inst::Switch { on, table });
            }
            Kind::Return => {
                let returns = self.g.sigs[self.id.index()].returns;
                let src = self.loc(returns);
                self.push(Inst::Return { src });
            }
            Kind::Call => {
                let callee = self.callee();
                let sig = self.g.sigs[callee.index()].clone();
                let args = self.row(&sig.params);
                let dst = self.loc(sig.returns);
                self.push(Inst::Call { dst, callee, args });
            }
            Kind::CallClosure => {
                // Mostly a closure that exists, with the arguments its callee
                // takes; the machine checks the object either way.
                let (params, result) = match self.g.closures.clone().as_slice() {
                    [] => (vec![INT], INT),
                    closures => {
                        let (_, callee) = self.g.pick(closures);
                        let sig = self.g.sigs[callee.index()].clone();
                        (sig.params, sig.returns)
                    }
                };
                let closure = self.slot(Repr::Ref);
                let args = self.row(&params);
                let dst = self.loc(result);
                self.push(Inst::CallClosure {
                    dst,
                    closure,
                    args,
                    result,
                });
            }
            Kind::CallHost => {
                let op = HostOpId(self.g.below(8) as u32);
                let result = self.g.host_ops[op.index()].result;
                let params: Vec<LayoutId> = (0..self.g.below(3))
                    .map(|_| self.layout_of_value())
                    .collect();
                let args = self.row(&params);
                let dst = self.loc(result);
                self.push(Inst::CallHost { dst, op, args });
            }
            Kind::CallResource => {
                let op = HostOpId(8 + self.g.below(2) as u32);
                let result = self.g.host_ops[op.index()].result;
                let receiver = self.slot(Repr::Host);
                let params: Vec<LayoutId> = (0..self.g.below(2))
                    .map(|_| self.layout_of_value())
                    .collect();
                let args = self.row(&params);
                let dst = self.loc(result);
                self.push(Inst::CallResource {
                    dst,
                    receiver,
                    op,
                    args,
                });
            }
            Kind::Alloc => {
                let objects = self.g.objects.clone();
                let layout = self.g.pick(&objects);
                let len = match self.g.byte() % 3 {
                    0 => Len::Fixed,
                    1 => Len::Count(u32::from(self.g.byte())),
                    _ => Len::Slot(self.slot(Repr::Int)),
                };
                let dst = self.slot(Repr::Ref);
                self.push(Inst::Alloc { dst, layout, len });
                self.objs.push((dst, layout));
            }
            Kind::LoadField => {
                let (obj, held) = self.object();
                let (at, layout) = match held.and_then(|held| self.field_of(held)) {
                    Some(field) if self.g.chance(3, 4) => field,
                    _ => (self.offset(), self.layout_of_value()),
                };
                let dst = self.loc(layout);
                self.push(Inst::LoadField {
                    dst,
                    obj,
                    at,
                    layout,
                });
            }
            Kind::StoreField => {
                let (obj, held) = self.object();
                let (at, layout) = match held.and_then(|held| self.field_of(held)) {
                    Some(field) if self.g.chance(3, 4) => field,
                    _ => (self.offset(), self.layout_of_value()),
                };
                let src = self.loc(layout);
                self.push(Inst::StoreField {
                    obj,
                    at,
                    src,
                    layout,
                });
            }
            Kind::LoadElem => {
                let (obj, held) = self.object();
                let index = self.slot(Repr::Int);
                let layout = match held.and_then(|held| self.element_of(held)) {
                    Some(elem) if self.g.chance(3, 4) => elem,
                    _ => self.layout_of_value(),
                };
                let dst = self.loc(layout);
                self.push(Inst::LoadElem {
                    dst,
                    obj,
                    index,
                    layout,
                });
            }
            Kind::StoreElem => {
                let (obj, held) = self.object();
                let index = self.slot(Repr::Int);
                let layout = match held.and_then(|held| self.element_of(held)) {
                    Some(elem) if self.g.chance(3, 4) => elem,
                    _ => self.layout_of_value(),
                };
                let src = self.loc(layout);
                self.push(Inst::StoreElem {
                    obj,
                    index,
                    src,
                    layout,
                });
            }
            Kind::RunLoad => {
                let run = self.slot(Repr::Ref);
                let index = self.slot(Repr::Int);
                let dst = self.slot(Repr::Int);
                self.push(Inst::RunLoad {
                    dst,
                    run,
                    index,
                    storage: Storage::PackedBytes,
                });
            }
            Kind::RunCopy => {
                let storage = self.storage();
                let args = self.row_of(&[
                    (Repr::Ref, REF),
                    (Repr::Int, INT),
                    (Repr::Ref, REF),
                    (Repr::Int, INT),
                    (Repr::Int, INT),
                ]);
                self.push(Inst::RunCopy { args, storage });
            }
            Kind::RunSlice => {
                let storage = self.storage();
                let target = match storage {
                    Storage::PackedBytes => STR,
                    Storage::Words(elem) => self.g.push_layout(Layout::object(
                        "Array",
                        Shape::Elements {
                            elem,
                            growable: false,
                        },
                    )),
                };
                let args = self.row_of(&[
                    (Repr::Ref, target),
                    (Repr::Ref, REF),
                    (Repr::Int, INT),
                    (Repr::Int, INT),
                ]);
                self.push(Inst::RunSlice { args, storage });
            }
            Kind::RunFind => {
                let args = self.row_of(&[
                    (Repr::Int, INT),
                    (Repr::Ref, STR),
                    (Repr::Ref, STR),
                    (Repr::Int, INT),
                ]);
                self.push(Inst::RunFind {
                    args,
                    storage: Storage::PackedBytes,
                });
            }
            Kind::GrowableAlloc => {
                let storage = match self.g.vectors.clone().as_slice() {
                    [] => self.storage(),
                    vectors if self.g.chance(1, 2) => Storage::Words(self.g.pick(vectors).1),
                    _ => self.storage(),
                };
                let capacity = self.slot(Repr::Int);
                let dst = self.slot(Repr::Ref);
                self.push(Inst::GrowableAlloc {
                    dst,
                    capacity,
                    storage,
                });
            }
            // The window, in the shape ADR 0062's rule admits: the length, a
            // count of one, the ensure, the store, the write at the length,
            // and the commit. Three kinds reach it so that it is not rare.
            Kind::GrowableEnsure | Kind::GrowableCommit | Kind::RunStore => self.window(),
            Kind::GrowableTruncate => {
                let elements = self.g.elements.clone();
                let elem = self.g.pick(&elements);
                let owner = self.slot(Repr::Ref);
                let len = self.slot(Repr::Int);
                self.push(Inst::GrowableTruncate {
                    owner,
                    len,
                    storage: Storage::Words(elem),
                });
            }
            Kind::RunFinish => {
                let storage = self.storage();
                let (target, validation) = match storage {
                    Storage::PackedBytes => (STR, Validation::Utf8),
                    Storage::Words(elem) => (
                        self.g.push_layout(Layout::object(
                            "Array",
                            Shape::Elements {
                                elem,
                                growable: false,
                            },
                        )),
                        Validation::None,
                    ),
                };
                let owner = self.slot(Repr::Ref);
                let dst = self.slot(Repr::Ref);
                self.push(Inst::RunFinish {
                    dst,
                    owner,
                    target,
                    validation,
                    storage,
                });
            }
            Kind::Len => {
                let obj = self.slot(Repr::Ref);
                let dst = self.slot(Repr::Int);
                self.push(Inst::Len { dst, obj });
            }
            Kind::AddrOfSlot => {
                let slot = if self.reprs.is_empty() {
                    self.slot(Repr::Int)
                } else {
                    self.g.below(self.reprs.len()) as Slot
                };
                let dst = self.slot(Repr::Addr);
                self.push(Inst::AddrOfSlot { dst, slot });
            }
            Kind::AddrOfField => {
                let obj = self.slot(Repr::Ref);
                let at = self.offset();
                let dst = self.slot(Repr::Addr);
                self.push(Inst::AddrOfField { dst, obj, at });
            }
            Kind::AddrOfPart => {
                let addr = self.slot(Repr::Addr);
                let at = self.offset();
                let dst = self.slot(Repr::Addr);
                self.push(Inst::AddrOfPart { dst, addr, at });
            }
            Kind::Load => {
                let addr = self.slot(Repr::Addr);
                let layout = self.layout_of_value();
                let dst = self.loc(layout);
                self.push(Inst::Load { dst, addr, layout });
            }
            Kind::Store => {
                let addr = self.slot(Repr::Addr);
                let layout = self.layout_of_value();
                let src = self.loc(layout);
                self.push(Inst::Store { addr, src, layout });
            }
            Kind::Box => {
                let layout = self.layout_of_value();
                let src = self.loc(layout);
                let dst = self.slot(Repr::Ref);
                self.push(Inst::Box { dst, src, layout });
            }
            Kind::Unbox => {
                let layout = self.layout_of_value();
                let src = self.slot(Repr::Ref);
                let dst = self.loc(layout);
                self.push(Inst::Unbox { dst, src, layout });
            }
            Kind::DynOpen => {
                let src = self.slot(Repr::Ref);
                let dst = self.loc(VIEW);
                self.push(Inst::DynOpen { dst, src });
            }
            Kind::DynKind
            | Kind::DynCase
            | Kind::DynCount
            | Kind::DynOpaque
            | Kind::DynTypeName
            | Kind::DynCaseName
            | Kind::DynHandleText => {
                let view = self.loc(VIEW);
                let inst = match kind {
                    Kind::DynKind => Inst::DynKind {
                        dst: self.slot(Repr::Int),
                        view,
                    },
                    Kind::DynCase => Inst::DynCase {
                        dst: self.slot(Repr::Int),
                        view,
                    },
                    Kind::DynCount => Inst::DynCount {
                        dst: self.slot(Repr::Int),
                        view,
                    },
                    Kind::DynOpaque => Inst::DynOpaque {
                        dst: self.slot(Repr::Bool),
                        view,
                    },
                    Kind::DynTypeName => Inst::DynTypeName {
                        dst: self.slot(Repr::Ref),
                        view,
                    },
                    Kind::DynCaseName => Inst::DynCaseName {
                        dst: self.slot(Repr::Ref),
                        view,
                    },
                    _ => Inst::DynHandleText {
                        dst: self.slot(Repr::Ref),
                        view,
                    },
                };
                self.push(inst);
            }
            Kind::DynSameType | Kind::DynSameObject | Kind::DynNameOrder => {
                let a = self.loc(VIEW);
                let b = self.loc(VIEW);
                let inst = match kind {
                    Kind::DynSameType => Inst::DynSameType {
                        dst: self.slot(Repr::Bool),
                        a,
                        b,
                    },
                    Kind::DynSameObject => Inst::DynSameObject {
                        dst: self.slot(Repr::Bool),
                        a,
                        b,
                    },
                    _ => Inst::DynNameOrder {
                        dst: self.slot(Repr::Int),
                        a,
                        b,
                    },
                };
                self.push(inst);
            }
            Kind::DynIdentitySet => {
                let dst = self.loc(IDENTITY_SET);
                self.push(Inst::DynIdentitySet { dst });
            }
            Kind::DynIdentityEnter | Kind::DynIdentityLeave => {
                // A set a set instruction made, or one made here: `verify`
                // admits an entry only into a set whose table nothing else
                // wrote.
                let made: Vec<Slot> = self
                    .locs
                    .iter()
                    .filter(|(_, layout)| *layout == IDENTITY_SET)
                    .map(|(slot, _)| *slot)
                    .collect();
                let set = match made.as_slice() {
                    [] => {
                        let dst = self.fresh(IDENTITY_SET);
                        self.push(Inst::DynIdentitySet { dst });
                        dst
                    }
                    made => self.g.pick(made),
                };
                let a = self.loc(VIEW);
                let b = self.loc(VIEW);
                self.push(match kind {
                    Kind::DynIdentityEnter => Inst::DynIdentityEnter { set, a, b },
                    _ => Inst::DynIdentityLeave { set, a, b },
                });
            }
            Kind::DynRead => {
                let view = self.loc(VIEW);
                let dst = self.slot_of(&[
                    Repr::Bool,
                    Repr::Int,
                    Repr::Float,
                    Repr::Duration,
                    Repr::Ref,
                ]);
                self.push(Inst::DynRead { dst, view });
            }
            Kind::DynChild => {
                let view = self.loc(VIEW);
                let index = self.slot(Repr::Int);
                let dst = self.loc(VIEW);
                self.push(Inst::DynChild { dst, view, index });
            }
            Kind::DynFieldName => {
                let view = self.loc(VIEW);
                let index = self.slot(Repr::Int);
                let dst = self.slot(Repr::Ref);
                self.push(Inst::DynFieldName { dst, view, index });
            }
            Kind::HandleText => {
                let src = self.slot_of(&[Repr::Host, Repr::Scope, Repr::Task]);
                let dst = self.slot(Repr::Ref);
                self.push(Inst::HandleText { dst, src });
            }
            Kind::DynOnPath => {
                let view = self.loc(VIEW);
                let path = self.loc(PATH);
                let dst = self.slot(Repr::Bool);
                self.push(Inst::DynOnPath { dst, view, path });
            }
            Kind::ScopeEnter => {
                let name = self.g.string();
                let dst = self.slot(Repr::Scope);
                self.push(Inst::ScopeEnter { dst, name });
            }
            Kind::ScopeLeave => {
                let scope = self.slot(Repr::Scope);
                let failed = self.slot(Repr::Bool);
                let layout = self.layout_of_value();
                let error = self.loc(layout);
                self.push(Inst::ScopeLeave {
                    scope,
                    failed,
                    error,
                    layout,
                });
            }
            Kind::ScopeCancel => {
                let scope = self.slot(Repr::Scope);
                self.push(Inst::ScopeCancel { scope });
            }
            Kind::Spawn => {
                let scope = self.slot(Repr::Scope);
                let closure = self.slot(Repr::Ref);
                let answer = self.layout_of_value();
                let dst = self.slot(Repr::Task);
                self.push(Inst::Spawn {
                    dst,
                    scope,
                    closure,
                    answer,
                });
            }
            Kind::Await => {
                let task = self.slot(Repr::Task);
                let answer = self.layout_of_value();
                let dst = self.loc(answer);
                self.push(Inst::Await { dst, task, answer });
            }
            Kind::Cancel => {
                let task = self.slot(Repr::Task);
                self.push(Inst::Cancel { task });
            }
            Kind::Settled => {
                let answer = self.layout_of_value();
                let src = self.loc(answer);
                let dst = self.slot(Repr::Task);
                self.push(Inst::Settled { dst, src, answer });
            }
            Kind::SharedLock | Kind::SharedUnlock => {
                let cell = self.slot(Repr::Ref);
                self.push(match kind {
                    Kind::SharedLock => Inst::SharedLock { cell },
                    _ => Inst::SharedUnlock { cell },
                });
            }
            Kind::Trap => {
                let message = self.slot(Repr::Ref);
                let rule = self.slot(Repr::Ref);
                let help = self.slot(Repr::Ref);
                self.push(Inst::Trap {
                    message,
                    rule,
                    help,
                });
            }
            Kind::AssertFailed => {
                let message = self.slot(Repr::Ref);
                self.push(Inst::AssertFailed { message });
            }
        }
    }

    /// ADR 0062's window over an owner the frame has: one unit pushed.
    fn window(&mut self) {
        let owners: Vec<(Slot, Storage)> = self
            .objs
            .iter()
            .filter_map(|(slot, layout)| {
                if *layout == BYTE_BUFFER {
                    return Some((*slot, Storage::PackedBytes));
                }
                self.g
                    .vectors
                    .iter()
                    .find(|(vector, _)| vector == layout)
                    .map(|(_, elem)| (*slot, Storage::Words(*elem)))
            })
            .collect();
        let (owner, storage) = if !owners.is_empty() && self.g.chance(4, 5) {
            self.g.pick(&owners)
        } else {
            (self.slot(Repr::Ref), self.storage())
        };
        let len = self.slot(Repr::Int);
        let count = self.fresh(INT);
        let store = self.fresh(REF);
        self.push(Inst::LoadField {
            dst: len,
            obj: owner,
            at: 0,
            layout: INT,
        });
        self.push(Inst::Int {
            dst: count,
            value: 1,
        });
        self.push(Inst::GrowableEnsure {
            owner,
            additional: count,
            storage,
        });
        self.push(Inst::LoadField {
            dst: store,
            obj: owner,
            at: 1,
            layout: REF,
        });
        match storage {
            Storage::PackedBytes => {
                let src = self.slot(Repr::Int);
                self.push(Inst::RunStore {
                    run: store,
                    index: len,
                    src,
                    storage,
                });
            }
            Storage::Words(elem) => {
                let src = self.loc(elem);
                self.push(Inst::StoreElem {
                    obj: store,
                    index: len,
                    src,
                    layout: elem,
                });
            }
        }
        self.push(Inst::Clear {
            slot: store,
            layout: REF,
        });
        self.push(Inst::GrowableCommit {
            owner,
            count,
            storage,
        });
    }

    fn finish(mut self, sig: Sig) -> Function {
        // The answer: a location of the function's own return layout, which
        // a fresh frame holds as zeroes if nothing wrote it.
        let src = self.loc(sig.returns);
        self.push(Inst::Return { src });
        let len = self.code.len() as u32;
        for at in std::mem::take(&mut self.jumps) {
            match &mut self.code[at] {
                Inst::Jump { to } | Inst::BranchFalse { to, .. } => *to %= len,
                Inst::CmpBranch { target, .. } | Inst::CmpImmBranch { target, .. } => {
                    *target %= len
                }
                _ => {}
            }
        }
        let params = sig.params.clone();
        let param_words: u32 = params.iter().map(|p| self.g.width(*p)).sum();
        let mut slot = param_words;
        let captures = sig
            .captures
            .iter()
            .enumerate()
            .map(|(at, layout)| {
                let held = Capture {
                    name: Arc::from(format!("c{at}").as_str()),
                    slot,
                    layout: *layout,
                };
                slot += self.g.width(*layout);
                held
            })
            .collect();
        let spans = (0..self.code.len())
            .map(|pc| Span::new(FileId(0), pc as u32, pc as u32 + 1))
            .collect();
        let refs = RefMap::of(&self.reprs);
        Function {
            module: Arc::from(MODULE),
            name: Arc::from(if self.id.index() == 0 {
                ENTRY.to_string()
            } else {
                format!("f{}", self.id.index())
            }),
            params,
            reprs: self.reprs,
            refs,
            returns: sig.returns,
            captures,
            code: self.code,
            spans,
            locals: Vec::new(),
            inlined: Vec::new(),
            span: Span::new(FileId(0), 0, 1),
            is_async: sig.is_async,
            stub: false,
        }
    }
}
