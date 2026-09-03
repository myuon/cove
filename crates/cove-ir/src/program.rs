//! A lowered program, and the tables an instruction indexes into.
//!
//! A [`Program`] is immutable once lowered. ADR 0008 runs a spawned task on
//! a thread of its own and a task's body is a lowered function like any
//! other, so every thread of one run reads this same program rather than a
//! copy of it — which is why the strings in it are `Arc<str>` and why
//! nothing here is behind a cell.

use std::collections::BTreeMap;
use std::sync::Arc;

use cove_diag::Span;

use crate::inst::{Inst, Pc, Slot};
use crate::layout::{Layout, LayoutId};
use crate::repr::{RefMap, Repr};

macro_rules! id {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub u32);

        impl $name {
            /// The index this id names.
            pub fn index(self) -> usize {
                self.0 as usize
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }
    };
}

id!(
    /// Names a [`Function`] in [`Program::functions`].
    FunctionId, "fn"
);
id!(
    /// Names a string in [`Program::strings`].
    StrId, "str"
);
id!(
    /// Names an argument list in [`Program::args`].
    ///
    /// A call's arguments are a static list of [`Arg`]s, held once in the
    /// program rather than inline in the instruction, so that [`Inst`] stays
    /// small enough to be worth copying and a repeated call shape costs one
    /// list rather than one per site.
    ArgsId, "args"
);
id!(
    /// Names a jump table in [`Program::tables`].
    TableId, "table"
);
id!(
    /// Names a host operation in [`Program::host_ops`].
    HostOpId, "host"
);
id!(
    /// Names a builtin in [`Program::builtins`].
    BuiltinId, "builtin"
);

/// One argument of a call: where the value is, and what it is.
///
/// A slot alone says where an operand *begins* and never how wide it is. A
/// scalar is described by the `Repr` of the slot it sits in and a reference
/// by the header of the object it names, but an inline struct or enum is a
/// run of words with nothing attached to it at all — a `Point` in a frame is
/// described by neither. So a callee that is polymorphic over the values it
/// is handed had no way to read one: `"{Point(x: 1)}"` rendered the first
/// word, `a == b` on two structs compared the first word, and the operations
/// that put a whole value into a collection refused rather than store half of
/// one.
///
/// Carrying the layout beside the slot answers all of them at once, and it is
/// carried for *every* argument rather than for the calls that turned out to
/// need it. A layout is what an argument is; which callee reads it is not the
/// argument's business, and one rule the verifier checks everywhere is worth
/// more than the word this costs at the sites that could have done without.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Arg {
    /// The first slot of the value location in the caller's frame.
    pub slot: Slot,
    /// The layout of that location, which is what says how wide it is.
    pub layout: LayoutId,
}

/// One host operation a program calls: `console.log`, `files.read`,
/// `files.Writer.writeLine`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostOp {
    pub module: Arc<str>,
    pub operation: Arc<str>,
    /// The resource kind the operation belongs to, for one addressed to a
    /// handle rather than to the module: `Writer` in
    /// `files.Writer.writeLine`.
    ///
    /// It is what [`Inst::CallResource`] names and what
    /// [`Inst::CallHost`] does not, so one table holds both and the two
    /// namings cannot collide: a module's `files.write` and a resource's
    /// `files.Writer.write` are two entries rather than one.
    ///
    /// Nothing dispatches on it. Which resource an operation reaches is the
    /// business of the handle the receiver names — ADR 0013 gives the host
    /// the only record of what is open — and this is what the call site
    /// settled, kept for the disassembly and for a diagnostic that has to
    /// say what was being called.
    pub resource: Option<Arc<str>>,
    /// The layout of the value location the host's answer is written into.
    ///
    /// A schema that declared its result `Any` gives a boxed layout;
    /// anything else gives the layout of the declared type.
    pub result: LayoutId,
}

impl HostOp {
    /// The operation as the source writes it: `console.println`, or
    /// `files.Writer.writeLine` for one addressed to a resource.
    pub fn qualified(&self) -> String {
        match &self.resource {
            Some(kind) => format!("{}.{kind}.{}", self.module, self.operation),
            None => format!("{}.{}", self.module, self.operation),
        }
    }
}

/// One builtin a program calls: `Array.length`, `String.split`, `Int.abs`.
///
/// A builtin is named rather than numbered because the set of them is the
/// language reference's, not the IR's: adding one is a runtime change, and
/// the IR should not have to be renumbered for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Builtin {
    /// The type the operation belongs to: `Array`, `String`, `Map`, `Int`.
    pub receiver: Arc<str>,
    pub operation: Arc<str>,
    pub result: LayoutId,
}

/// Where a [`Inst::Switch`] goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    /// One target per case index, in order.
    pub targets: Vec<Pc>,
    /// Where an index outside `targets` goes.
    ///
    /// A `match` the checker proved exhaustive still has one, because the
    /// value being switched on came out of a heap object and the machine
    /// does not take the lowering's word for what is in it.
    pub default: Pc,
}

/// A capture a closure body reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capture {
    pub name: Arc<str>,
    /// The first slot of the closure frame's value location for it.
    ///
    /// Captures follow the parameters, each taking the words its layout
    /// says. It is written down rather than derived because the machine
    /// should not have to re-add a run of widths it can read.
    pub slot: Slot,
    pub layout: LayoutId,
}

/// One named binding, and the range of the function's code over which that
/// name denotes that slot.
///
/// A side table, and read for the same reason [`Function::spans`] is: a name
/// is wanted when a *human* asks what a frame holds — a debugger stopped at a
/// breakpoint, issue #241 — and never in the dispatch loop, so it belongs
/// beside the code rather than in it.
///
/// It exists because neither half of that question is answerable from the
/// frame. [`Function::reprs`] says what a slot's *word* holds, for the whole
/// function, and that is all it says: until this table the only name anywhere
/// in a lowered program was [`Capture::name`], parameters were positional and
/// locals were anonymous. So a debugger could say `s7:int = 3`, which is true
/// of the machine and can be a lie about the program.
///
/// # Two locals may share a slot, and that is the point
///
/// [`Function::reprs`]' own note says a slot may be reused by a later value
/// of the same `Repr`, because the lowering hands a dead run to the next
/// value that asks for that shape. One slot is therefore several source
/// variables over a function's life, and nothing but this table can tell them
/// apart. Two locals of one slot have *disjoint* ranges and, usually,
/// different names.
///
/// # Two locals may share a name
///
/// Shadowing is recorded, not resolved. `let x = 1; let x = "two"` is two
/// bindings and both are kept, because the first is still what the frame
/// holds at every pc before the second — and because resolving here would
/// make the table disagree with the lowering, whose scope is searched
/// backwards so that the latest declaration wins. Their ranges may overlap
/// and their slots differ. A reader keeps the locals whose range contains the
/// pc and **takes the last match**; [`Function::local_at`] is that rule
/// written down.
///
/// A `break` or a `continue` is not an end of a range. `[from, to)` is an
/// interval of program counters, every pc inside a scope's body is one the
/// binding is live at, and the pc a `break` jumps to is outside the interval
/// already.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Local {
    pub name: Arc<str>,
    pub slot: Slot,
    pub layout: LayoutId,
    /// The first pc at which the name is bound.
    pub from: Pc,
    /// One past the last. `[from, to)` is a half-open interval, like a
    /// [`Span`].
    pub to: Pc,
}

/// One lowered function.
#[derive(Clone, Debug)]
pub struct Function {
    /// The module and name the source declared, for diagnostics and for
    /// [`Program::function_named`].
    pub module: Arc<str>,
    pub name: Arc<str>,
    /// The layout of each parameter, in declaration order.
    ///
    /// Parameters occupy the frame from slot 0 onward, each taking the words
    /// its layout says: a `(Int, Point, Int)` list occupies slots 0, 1–2 and
    /// 3. Declaration order, not a permutation into type groups — ADR 0034's
    /// *"a mixed list such as `(Int, String, Int)` is not permuted into type
    /// regions"*. There are no type regions to permute into.
    pub params: Vec<LayoutId>,
    /// What each slot of the frame holds. `reprs.len()` is the frame size.
    ///
    /// A slot's `Repr` is fixed for the whole function; that is what makes
    /// [`Function::refs`] correct at every program counter. A slot may be
    /// reused by a later value of the same `Repr`, and a reference slot is
    /// cleared to null at its last use, so the static map costs no retention
    /// beyond a value's live range.
    pub reprs: Vec<Repr>,
    /// Which slots are references, derived from [`Function::reprs`].
    pub refs: RefMap,
    /// The layout of what the function answers.
    ///
    /// [`Inst::Return`] names the base slot of the answer in the callee's
    /// frame and the caller's [`Inst::Call`] names the base slot of the
    /// destination location in its own; the machine copies this many words
    /// between them.
    pub returns: LayoutId,
    /// The values the enclosing body handed this function, if it is a
    /// lambda. Empty for a declared function.
    pub captures: Vec<Capture>,
    pub code: Vec<Inst>,
    /// The source span of each instruction, parallel to [`Function::code`].
    ///
    /// A parallel array rather than a field of [`Inst`]: a span is read when
    /// a run fails or a trace is written, and never in the dispatch loop, so
    /// it should not be in the cache line the loop is reading.
    pub spans: Vec<Span>,
    /// What the source called the values in the frame, and where each name
    /// meant which slot.
    ///
    /// In declaration order, which is the order the shadowing rule reads
    /// them in; see [`Local`]. Not parallel to anything — a function binds as
    /// many names as it binds — and empty is a legal answer for a body that
    /// binds none.
    pub locals: Vec<Local>,
    /// Where the declaration itself is, for a diagnostic that is about the
    /// function rather than about one of its instructions.
    pub span: Span,
    /// Whether the body is a task's: `async fn`, or the lambda a `spawn`
    /// was handed.
    pub is_async: bool,
}

impl Function {
    /// How many words a call to this function occupies on the stack.
    pub fn frame_size(&self) -> u32 {
        self.reprs.len() as u32
    }

    /// How many parameters the function declares.
    pub fn arity(&self) -> u32 {
        self.params.len() as u32
    }

    /// The first slot of parameter `at`, which is the widths of the ones
    /// before it.
    pub fn param_slot(&self, at: usize, layouts: &[Layout]) -> Slot {
        self.params[..at]
            .iter()
            .map(|id| layouts[id.index()].width())
            .sum()
    }

    /// How many slots the parameters occupy in total.
    pub fn param_words(&self, layouts: &[Layout]) -> u32 {
        self.params
            .iter()
            .map(|id| layouts[id.index()].width())
            .sum()
    }

    /// What slot `slot` holds.
    pub fn repr(&self, slot: Slot) -> Option<Repr> {
        self.reprs.get(slot as usize).copied()
    }

    /// The span of the instruction at `pc`, or the declaration's own.
    pub fn span_at(&self, pc: usize) -> Span {
        self.spans.get(pc).copied().unwrap_or(self.span)
    }

    /// Which slot `name` denotes at `pc`, if the source bound it there.
    ///
    /// The last match wins, because a shadowing declaration is recorded
    /// beside the one it shadows rather than in place of it: see [`Local`].
    pub fn local_at(&self, name: &str, pc: Pc) -> Option<&Local> {
        self.locals
            .iter()
            .rev()
            .find(|local| &*local.name == name && local.from <= pc && pc < local.to)
    }

    /// `module.name`, as a diagnostic writes it.
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.module, self.name)
    }
}

/// A whole lowered package.
#[derive(Clone, Debug, Default)]
pub struct Program {
    pub functions: Vec<Function>,
    pub layouts: Vec<Layout>,
    pub strings: Vec<Arc<str>>,
    pub args: Vec<Vec<Arg>>,
    pub tables: Vec<Table>,
    pub host_ops: Vec<HostOp>,
    pub builtins: Vec<Builtin>,
    /// The layout every string object shares.
    ///
    /// One field rather than a layout in each [`Inst::Str`], because every
    /// string in a program has the same shape and the machine should not
    /// have to be told it once per literal. A program that mentions no
    /// string still declares it: the machine allocates one for a host's
    /// answer, and a table it has to check for emptiness first is a branch
    /// on a path that always takes the same side.
    pub str_layout: LayoutId,
    /// The layout every [`Inst::Box`] allocates its object as.
    ///
    /// A program-wide constant for the same reason [`Program::str_layout`]
    /// is one: every box has the same *object* shape, and what differs — the
    /// layout of the value inside it — is in the box's first payload word.
    /// The machine should not have to search a table for a shape that is
    /// always the same, and a search that fails has to answer something.
    pub boxed_layout: LayoutId,
    /// `module.name` to id, for an entry point named on a command line.
    pub by_name: BTreeMap<(Arc<str>, Arc<str>), FunctionId>,
}

impl Program {
    pub fn function(&self, id: FunctionId) -> &Function {
        &self.functions[id.index()]
    }

    pub fn layout(&self, id: LayoutId) -> &Layout {
        &self.layouts[id.index()]
    }

    pub fn string(&self, id: StrId) -> &Arc<str> {
        &self.strings[id.index()]
    }

    pub fn arg_list(&self, id: ArgsId) -> &[Arg] {
        &self.args[id.index()]
    }

    pub fn table(&self, id: TableId) -> &Table {
        &self.tables[id.index()]
    }

    pub fn host_op(&self, id: HostOpId) -> &HostOp {
        &self.host_ops[id.index()]
    }

    pub fn builtin(&self, id: BuiltinId) -> &Builtin {
        &self.builtins[id.index()]
    }

    /// The id of `module.name`, if the program has it.
    pub fn function_named(&self, module: &str, name: &str) -> Option<FunctionId> {
        self.by_name
            .iter()
            .find(|((m, n), _)| &**m == module && &**n == name)
            .map(|(_, id)| *id)
    }
}
