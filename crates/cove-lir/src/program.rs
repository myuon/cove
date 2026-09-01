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
    /// A call's arguments are a static list of source slots, held once in the
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

/// One host operation a program calls: `console.log`, `files.read`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostOp {
    pub module: Arc<str>,
    pub operation: Arc<str>,
    /// The layout of the value location the host's answer is written into.
    ///
    /// A schema that declared its result `Any` gives a boxed layout;
    /// anything else gives the layout of the declared type.
    pub result: LayoutId,
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
    pub args: Vec<Vec<Slot>>,
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

    pub fn arg_list(&self, id: ArgsId) -> &[Slot] {
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
