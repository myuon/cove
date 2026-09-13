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
    ///
    /// # Scope and stability
    ///
    /// A `FunctionId` is dense — it is a position in one [`Program`]'s
    /// `functions`, not a name — and it means nothing outside that one
    /// linked program. It is not stable across an edit to the package that
    /// produced it: declarations are numbered first and lambdas and generic
    /// instantiations are appended after them, so adding, removing or moving
    /// an earlier one renumbers every later one. It is not a stable external
    /// identity either, in the way a qualified name is — two builds of the
    /// same source are not guaranteed to number a function the same way, and
    /// nothing here promises they will.
    ///
    /// Nothing may persist a bare `FunctionId` across an artifact boundary,
    /// because there is no third thing to check it against once it is on
    /// the far side of one. Every boundary Cove has today avoids the
    /// question instead of answering it: `cove build` embeds the checked
    /// source in the generated crate and lowers a fresh [`Program`] when
    /// that crate is compiled, rather than serialising this one's; the
    /// trace and replay format keys an entry point by `module` and
    /// `function` **strings** ([`Program::function_named`] resolves the
    /// pair against whatever program is current, back into a `FunctionId`
    /// of *that* run) and never writes the id itself; and the wasm
    /// playground's boundary is rendered text — [`crate::print`]'s
    /// listing — not the program that produced it. No id crosses a boundary
    /// today because nothing here has ever needed one to.
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
    /// Names one case of an enum layout: its position in
    /// [`crate::layout::Shape::Enum::cases`].
    ///
    /// It is the number an enum's discriminant word holds, and it is a type
    /// of its own for the reason [`crate::Repr::Tag`] is: the word is an
    /// integer and the value is not one. Where the number is written into a
    /// slot — [`crate::Inst::Tag`] — the id says which case it names, and the
    /// verifier bounds it against the layout rather than against nothing.
    CaseId, "case"
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

/// A body that was written elsewhere and expanded into this one.
///
/// `[from, to)` is the run of this function's program counters the expansion
/// occupies — from where the call stood to where its answer landed —
/// `callee` is whose instructions they are, and `site` is where the call was
/// written.
///
/// `site` is the whole of what an error chain lost. `Machine::call_chain`
/// walks the live frames and reads each one's call site; an expansion has no
/// frame, so its call site was not there to read, and an error raised inside
/// one named where it happened and not where it was called from. One span per
/// expansion is what puts that back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inlined {
    pub from: Pc,
    pub to: Pc,
    pub callee: FunctionId,
    pub site: Span,
    /// The names the expanded body bound, in this function's slots and this
    /// function's counters.
    ///
    /// Here rather than in [`Function::locals`], and that is not tidiness. A
    /// caller's binding and an expanded body's parameter can be bound at the
    /// *same* program counter — the caller's `let raised = n + 1` and the
    /// callee's `n`, when the argument needed no copy — and a reader sorting
    /// one table by "which expansion contains this counter" cannot tell them
    /// apart. Which body declared a name is not something a counter answers,
    /// so it is recorded rather than derived.
    pub locals: Vec<Local>,
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
    /// The bodies this function holds that were written somewhere else.
    ///
    /// `lower::inline` expands a call to a small leaf where it is made, and
    /// the frame that call would have pushed then does not exist. Nothing
    /// downstream can tell: a run of instructions in the middle of this
    /// function *is* another function, and every reader that walks frames —
    /// an error's chain, a backtrace, a profile — sees one frame where there
    /// were two.
    ///
    /// So the expansion writes down what it removed. This is that record, and
    /// it is [`Local`]'s shape for [`Local`]'s reason: a slot number is not an
    /// answer to "what did the source call this", and a program counter is not
    /// an answer to "whose instruction is this".
    ///
    /// In the order the expansions were made, which is program-counter order,
    /// and ranges nest rather than overlap. A reader takes the *last* range
    /// that contains the pc, which is the innermost body — the same rule
    /// [`Function::local_at`] follows, for the same reason.
    pub inlined: Vec<Inlined>,
    /// Where the declaration itself is, for a diagnostic that is about the
    /// function rather than about one of its instructions.
    pub span: Span,
    /// Whether the body is a task's: `async fn`, or the lambda a `spawn`
    /// was handed.
    pub is_async: bool,
    /// Whether this is a stand-in the lowering left for a declaration it
    /// did not lower a body for. See `lower::stub`, and `Function::is_stub`.
    pub stub: bool,
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

    /// The expanded bodies `pc` is inside, innermost last.
    ///
    /// A reader that wants one frame's worth of context wants the last of
    /// them; a reader rebuilding a chain wants all of them, innermost first,
    /// which is this reversed. Ranges nest, so "contains the pc" and "in the
    /// order they were made" is enough to order them: an inner expansion is
    /// always written after the outer one it sits in.
    pub fn inlined_at(&self, pc: Pc) -> impl Iterator<Item = &Inlined> + '_ {
        self.inlined
            .iter()
            .filter(move |held| held.from <= pc && pc < held.to)
    }

    /// `module.name`, as a diagnostic writes it.
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.module, self.name)
    }

    /// Whether this is a stand-in rather than a lowered body.
    ///
    /// A stub has no body to stop at: its one instruction is a `Return`
    /// written at the declaration's own span, and it has no parameters and
    /// no names, because there was no boundary and no scope to bind them
    /// from. A tool that resolves a source location or a breakpoint against
    /// a program — a debugger walking [`Function::locals`], a stack trace
    /// reading [`Function::span_at`] — has to skip a stub rather than answer
    /// out of it, or it answers a question about a function that was never
    /// written.
    ///
    /// It answers `true` for all three kinds `lower::stub`'s doc comment
    /// describes, because `stub` is the one place any of them is built and
    /// this reads back exactly what it recorded. But a program that actually
    /// runs — the output of [`lower_roots`](crate::lower_roots) or
    /// [`lower_entry`](crate::lower_entry) once a lowering finishes without
    /// error — can only hold two of the three: the declaration a slice left
    /// out, and a generic declaration whose instantiations carry the real
    /// code beside it. The third kind, a declaration this lowering reported
    /// a gap about, belongs to a lowering that never got handed back — a gap
    /// is an error, so the program it would have been part of does not exist
    /// for a caller of this method to ask about.
    ///
    /// This is a stored fact rather than a test of the four fields above,
    /// because a shape a stub happens to have is not a shape only a stub
    /// has. `lower::stub`'s own construction is the only place that knows
    /// *why* the instruction, span, and empty lists are what they are;
    /// asking a shape test to recover that intent at a distance means the
    /// day a real body of one instruction is ever written at its
    /// declaration's own span, the test is wrong and nothing says so.
    /// Recording the fact the lowering already has costs one field;
    /// re-deriving it costs a convention two crates now have to keep in
    /// sync by hand.
    pub fn is_stub(&self) -> bool {
        self.stub
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
    /// The layout every byte run under construction shares.
    ///
    /// A program-wide constant for the reason [`Program::str_layout`] is one:
    /// [`Inst::AllocBytes`] should not have to be told this layout per call
    /// site, and [ADR 0051](../../docs/adr/0051-a-string-is-built-as-a-byte-run.md)
    /// gives every run the same [`crate::layout::Shape::Bytes`] shape whatever
    /// string it will become.
    pub bytes_layout: LayoutId,
    /// The layout every byte buffer's owner shares.
    ///
    /// A program-wide constant for [`Program::bytes_layout`]'s reason, and the
    /// other half of the pair: an owner and its store are allocated together
    /// by [`Inst::AllocBuffer`], so neither layout is named at a call site.
    /// [ADR 0052](../../docs/adr/0052-a-growable-value-is-a-stable-owner-over-a-replaceable-run.md)
    /// gives every buffer the same [`crate::layout::Shape::ByteBuffer`] shape
    /// whatever bytes it will hold, because an owner's two words are a length
    /// and a reference whatever the store's capacity.
    pub buffer_layout: LayoutId,
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
