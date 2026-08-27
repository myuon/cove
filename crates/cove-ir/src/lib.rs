//! The executable IR a Cove program is lowered to, and the lowering itself.
//!
//! [ADR 0019](../../../docs/adr/0019-executable-ir-and-vm.md) decides why this
//! exists. The short version is that a tree-walking interpreter re-derives, on
//! every evaluation, facts that were settled before the program ran — where a
//! binding lives, what a call targets, how big a frame is — and a tree has
//! nowhere to put an answer so that using it costs nothing. This is that
//! somewhere: an index is *in* the instruction, and a frame is a region of a
//! stack whose size was known when the function was lowered.
//!
//! # This is a lowering, not a second source of truth
//!
//! `cove-sema` already answers what every reference denotes. Nothing here
//! re-derives that; it records the answers in a shape the VM can act on
//! without asking again. Where the two could disagree, the checker is right by
//! construction, because the lowering reads its answers rather than
//! recomputing them.
//!
//! Lowering runs once per program and may allocate freely. Execution is the
//! thing being made fast, and nothing else is.
//!
//! # What is not here
//!
//! No serialization, no version number, no compatibility promise. Nothing
//! outside this repository consumes the IR, and ADR 0019 says it stays that
//! way until there is evidence worth stabilizing.
//!
//! A construct the lowering does not cover yet is reported as
//! [`Unsupported`] rather than approximated. ADR 0019's no-silent-fallback
//! rule is what makes that the right answer: a VM that quietly finished a run
//! on the interpreter would be a VM whose measurements are about a mixture and
//! whose conformance is about whatever it happened to cover.

pub mod lower;

use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

use cove_diag::Span;

/// One lowered program: every function it can execute, and the constants they
/// share.
#[derive(Debug, Default)]
pub struct Program {
    /// Every lowered function, addressed by [`FunctionId`].
    pub functions: Vec<Function>,
    /// Every constant any function loads, addressed by [`ConstId`].
    pub constants: Vec<Const>,
}

impl Program {
    /// The function `id` names.
    pub fn function(&self, id: FunctionId) -> &Function {
        &self.functions[id.0 as usize]
    }

    /// The constant `id` names.
    pub fn constant(&self, id: ConstId) -> &Const {
        &self.constants[id.0 as usize]
    }

    /// The function a qualified name denotes, if this program lowered one.
    ///
    /// This is how an entry point is found, once, before a run. Nothing on a
    /// hot path looks a function up by name.
    pub fn function_named(&self, module: &str, name: &str) -> Option<FunctionId> {
        self.functions
            .iter()
            .position(|function| &*function.module == module && &*function.name == name)
            .map(|index| FunctionId(index as u32))
    }
}

/// One lowered function: what it needs to run and the instructions that run
/// it.
#[derive(Debug)]
pub struct Function {
    /// The module that declares it, and the name it declares — both kept for
    /// diagnostics and traces, and read by nothing on a hot path.
    pub module: Rc<str>,
    pub name: Rc<str>,
    /// How many slots one call needs: `self` if it has a receiver, then its
    /// parameters, then every local and temporary the body declares.
    ///
    /// A frame is this many slots of a stack that already exists, which is
    /// the whole reason for lowering.
    pub frame_size: u32,
    /// How many arguments a call must supply, `self` included.
    pub arity: u32,
    /// Whether slot 0 is a receiver rather than the first parameter.
    pub has_receiver: bool,
    /// What the closure that created this body handed it, in the order the
    /// instructions expect. Empty for a declared function.
    pub captures: Vec<Rc<str>>,
    pub code: Vec<Inst>,
    /// One span per instruction, so a runtime error points at source.
    ///
    /// Parallel to `code` rather than inside `Inst`, so that an instruction
    /// stays small and a span costs nothing to skip.
    pub spans: Vec<Span>,
    /// The spans of an instruction's arguments, by instruction index, for the
    /// instructions whose diagnostic quotes source.
    ///
    /// An instruction's own span covers the whole call it came from, so a
    /// diagnostic that quotes the source of one *argument* — which is what a
    /// failing `assert` does, and the whole reason `assert` is a builtin
    /// rather than a library function — cannot be written from it. This is
    /// where the argument's own source is, and it is recorded only where such
    /// a diagnostic exists: a span nothing quotes would be a cost with no
    /// reader.
    pub arg_spans: BTreeMap<u32, Vec<Span>>,
    /// Where the function itself was declared, for a diagnostic about the
    /// function rather than about one of its instructions.
    pub span: Span,
}

impl Function {
    /// The span of the instruction at `pc`, or the function's own.
    pub fn span_at(&self, pc: usize) -> Span {
        self.spans.get(pc).copied().unwrap_or(self.span)
    }

    /// The spans of the arguments of the instruction at `pc`, and nothing
    /// where that instruction has no diagnostic that quotes them.
    pub fn arg_spans_at(&self, pc: usize) -> &[Span] {
        self.arg_spans
            .get(&(pc as u32))
            .map_or(&[][..], Vec::as_slice)
    }
}

/// Addresses a [`Function`] of a [`Program`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FunctionId(pub u32);

/// Addresses a [`Const`] of a [`Program`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConstId(pub u32);

/// A value an instruction can push without computing it.
///
/// Only what a literal can be, plus the names a structured operation needs to
/// carry. A name here is a constant because it is written once at lowering and
/// read by an instruction that already knows what to do with it — not because
/// anything looks it up.
#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Duration(i64),
    Str(Rc<str>),
    /// A name carried by an instruction: a field, a host module, a host
    /// operation, a builtin method, or a declared type.
    Name(Rc<str>),
}

/// One VM instruction.
///
/// The set is deliberately small and deliberately not general: every variant
/// exists because some Cove construct lowers to it, and none exists in case
/// something might.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inst {
    /// Pushes a constant.
    Const(ConstId),
    /// Pushes the value in a frame slot.
    LoadLocal(u32),
    /// Pops a value into a frame slot.
    StoreLocal(u32),
    /// Pushes one of this call's captures.
    LoadCapture(u32),
    /// Discards the top of the stack.
    Pop,
    /// Applies a unary operator to the top of the stack.
    Unary(UnaryOp),
    /// Applies a binary operator to the top two values, left below right.
    ///
    /// This is the operator that does not know what it is applied to. It goes
    /// through the interpreter's own `binary`, which takes two 40-byte
    /// `Value`s, answers a 120-byte `Result`, and matches three times to find
    /// out that it was adding two integers. Where the checker settled that it
    /// *is* two integers, [`Inst::IntBinary`] is emitted instead.
    Binary(BinaryOp),
    /// Applies a binary operator to two `Int` on top of the stack.
    ///
    /// Only emitted where the checker recorded both operands as `Int`, so the
    /// operands need no examining — the type is in the instruction. Overflow,
    /// division by zero, and remainder by zero raise what the untyped operator
    /// raises, because a broken invariant is the same broken invariant however
    /// it was reached.
    IntBinary(IntOp),
    /// Pushes a field of the struct on top of the stack, by position.
    ///
    /// Emitted where the checker settled the receiver's type, which is what
    /// makes the position knowable. [`Inst::GetField`] is what a receiver
    /// whose type the checker abstained about still gets.
    GetFieldAt(u32),
    /// Jumps to an instruction index.
    Jump(u32),
    /// Pops a `Bool` and jumps when it is false.
    JumpIfFalse(u32),
    /// Pops a `Bool` and jumps when it is true.
    JumpIfTrue(u32),
    /// Duplicates the top of the stack.
    Dup,
    /// Calls a lowered function with `argc` arguments, pushed left to right.
    Call { function: FunctionId, argc: u32 },
    /// Calls a Host operation. `module` and `op` are `Const::Name`.
    CallHost {
        module: ConstId,
        op: ConstId,
        argc: u32,
    },
    /// Calls a builtin method on a receiver below `argc` arguments. `name` is
    /// a `Const::Name`.
    CallBuiltin { name: ConstId, argc: u32 },
    /// Builds an `Array` from the top `len` values.
    MakeArray(u32),
    /// Renders the top `parts` values as one string, left to right.
    ///
    /// This is what `"a{b}c"` lowers to: interpolation is not concatenation
    /// in the language — `+` on two strings is refused — but it is one
    /// operation over a known number of parts here, which is what an
    /// instruction is for.
    Concat(u32),
    /// Builds a declared struct. `ty` is a `Const::Name` holding the qualified
    /// type name, and `fields` names each field in the order the values were
    /// pushed.
    MakeStruct { ty: ConstId, fields: ConstId },
    /// Pushes a field of the struct on top of the stack. `name` is a
    /// `Const::Name`.
    GetField(ConstId),
    /// Replaces a field: pops a value and the struct below it, and pushes the
    /// struct with that field changed. `name` is a `Const::Name`.
    ///
    /// Writing a field is a whole-value update here rather than a mutation
    /// through a place, which is what `crate::lower` can do while a `var`
    /// parameter is not in the subset it lowers. A struct is a value, and the
    /// only holder of a local's struct is that local, so replacing the local
    /// with an updated struct is what assigning to its field means. That stops
    /// being true the moment two names can reach one struct — which is exactly
    /// what a `var` parameter is — so whoever lowers `var` has to give the VM
    /// a real place model first, and this instruction is not it.
    SetField(ConstId),
    /// Builds one of the builtin constructors: `Ok`, `Err`, `Some`, or
    /// `Error`, over one value, or `None` over none; and runs the two
    /// assertions, `assert` and `assertEqual`.
    ///
    /// A failing assertion quotes the source text of its condition, so the
    /// spans of an assertion's arguments are recorded in
    /// [`Function::arg_spans`] beside it.
    MakeBuiltin { name: ConstId, argc: u32 },
    /// Builds a case of a declared enum. `ty` is a `Const::Name` holding the
    /// qualified type name and `case` the case's own name.
    MakeEnum {
        ty: ConstId,
        case: ConstId,
        argc: u32,
    },
    /// Calls an associated function of a builtin type, such as `Vector.of` or
    /// `Int.parse`. `ty` and `name` are `Const::Name`.
    ///
    /// Separate from [`Inst::CallBuiltin`] because there is no receiver: the
    /// type is named rather than stood on, so the arguments are the whole of
    /// the stack this reads.
    CallBuiltinAssoc {
        ty: ConstId,
        name: ConstId,
        argc: u32,
    },
    /// Pushes whether the value on top is an enum of case `case`, without
    /// consuming it. `case` is a `Const::Name`.
    ///
    /// A pattern tests the subject and then binds out of it, and both need the
    /// subject still there — which is why this peeks. `Pop` is what a lowering
    /// writes when an arm is done with it.
    TestCase(ConstId),
    /// Pushes one payload of the enum on top, without consuming it.
    GetPayload(u32),
    /// Pops a value and pushes an `Array` of what `for` walks over it: the
    /// elements of a sequence, the `MapEntry` of each pair of a `Map`, a
    /// `Set`'s elements in ascending order.
    ///
    /// A `for` could walk a sequence by index, and did, and that was wrong for
    /// the two collections whose iteration is not indexing. So the question is
    /// asked once, by the same function the interpreter asks, and the loop
    /// walks what comes back.
    IterItems,
    /// Stops the run because no `match` arm covered the value on top.
    ///
    /// Exhaustiveness is the checker's to prove and it does not prove it yet,
    /// so a `match` that covers nothing has to fail rather than answer. It
    /// carries no name: what the message needs is the value it could not
    /// match, and that is on the stack.
    NoMatch,
    /// `expr?`: pops a `Result` or `Option`, pushes its payload, or returns
    /// the failure from this call.
    Try,
    /// Returns the top of the stack.
    Return,
}

/// The unary operators the IR carries, which are the language's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
}

/// What [`Inst::IntBinary`] does to two integers.
///
/// A separate enum from [`BinaryOp`] because it is a smaller question: these
/// are the operators `Int` answers, and nothing here has a case for a type
/// that cannot arrive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// The binary operators the IR carries.
///
/// `&&` and `||` are absent on purpose: they short-circuit, so they lower to
/// a jump rather than to an operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Is,
}

/// A construct the lowering does not cover.
///
/// Named rather than approximated: ADR 0019 requires that a VM run either
/// finish on the VM or fail before any side effect, so an unsupported
/// construct has to stop the lowering and say what it was.
#[derive(Clone, Debug)]
pub struct Unsupported {
    /// What was not lowered, in the words a Cove programmer would use.
    pub what: String,
    pub span: Span,
}

impl fmt::Display for Unsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the VM cannot run {} yet", self.what)
    }
}

impl Unsupported {
    /// Reports that `what` has no lowering.
    pub fn new(what: impl Into<String>, span: Span) -> Unsupported {
        Unsupported {
            what: what.into(),
            span,
        }
    }
}

// ------------------------------------------------------------------ printing

/// Renders one function the way a golden test reads it.
///
/// Deterministic and stable under anything that does not change the
/// instructions, so a test can assert on the whole listing rather than on
/// whichever part it thought to check.
pub fn render(program: &Program, id: FunctionId) -> String {
    let function = program.function(id);
    let mut out = String::new();
    out.push_str(&format!(
        "fn {}.{} arity={} frame={}",
        function.module, function.name, function.arity, function.frame_size
    ));
    if function.has_receiver {
        out.push_str(" receiver");
    }
    if !function.captures.is_empty() {
        out.push_str(&format!(" captures=[{}]", function.captures.join(", ")));
    }
    out.push('\n');
    for (pc, inst) in function.code.iter().enumerate() {
        out.push_str(&format!("{pc:4}  {}\n", render_inst(program, *inst)));
    }
    out
}

/// Renders one instruction, resolving the constants it names so that a
/// listing reads without a second table beside it.
fn render_inst(program: &Program, inst: Inst) -> String {
    let name = |id: ConstId| match program.constant(id) {
        Const::Name(name) => name.to_string(),
        other => format!("{other:?}"),
    };
    match inst {
        Inst::Const(id) => format!("const {:?}", program.constant(id)),
        Inst::LoadLocal(slot) => format!("load {slot}"),
        Inst::StoreLocal(slot) => format!("store {slot}"),
        Inst::LoadCapture(index) => format!("capture {index}"),
        Inst::Pop => "pop".to_string(),
        Inst::Dup => "dup".to_string(),
        Inst::Unary(op) => format!("unary {op:?}"),
        Inst::Binary(op) => format!("binary {op:?}"),
        Inst::IntBinary(op) => format!("int {op:?}"),
        Inst::GetFieldAt(index) => format!("get-field-at {index}"),
        Inst::Jump(to) => format!("jump {to}"),
        Inst::JumpIfFalse(to) => format!("jump-if-false {to}"),
        Inst::JumpIfTrue(to) => format!("jump-if-true {to}"),
        Inst::Call { function, argc } => {
            let target = program.function(function);
            format!("call {}.{} argc={argc}", target.module, target.name)
        }
        Inst::CallHost { module, op, argc } => {
            format!("call-host {}.{} argc={argc}", name(module), name(op))
        }
        Inst::CallBuiltin { name: n, argc } => format!("call-builtin {} argc={argc}", name(n)),
        Inst::MakeArray(len) => format!("make-array {len}"),
        Inst::Concat(parts) => format!("concat {parts}"),
        Inst::MakeStruct { ty, fields } => {
            format!("make-struct {} fields={}", name(ty), name(fields))
        }
        Inst::GetField(n) => format!("get-field {}", name(n)),
        Inst::SetField(n) => format!("set-field {}", name(n)),
        Inst::MakeBuiltin { name: n, argc } => format!("make-builtin {} argc={argc}", name(n)),
        Inst::MakeEnum { ty, case, argc } => {
            format!("make-enum {}.{} argc={argc}", name(ty), name(case))
        }
        Inst::CallBuiltinAssoc { ty, name: n, argc } => {
            format!("call-assoc {}.{} argc={argc}", name(ty), name(n))
        }
        Inst::TestCase(case) => format!("test-case {}", name(case)),
        Inst::GetPayload(index) => format!("get-payload {index}"),
        Inst::IterItems => "iter-items".to_string(),
        Inst::NoMatch => "no-match".to_string(),
        Inst::Try => "try".to_string(),
        Inst::Return => "return".to_string(),
    }
}
