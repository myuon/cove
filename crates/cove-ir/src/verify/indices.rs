//! Every id in a program names a row that exists.
//!
//! [`verify`](super::verify) is written against programs the lowering built,
//! whose tables are consistent by construction, and it indexes them freely:
//! `layouts[id.index()]`, `functions[id.index()]`. A program that did not come
//! from the lowering, such as one [`crate::serial::decode`] accepted (it checks
//! the bytes, not what the ids in them mean), can hold an id past the end of a
//! table, and the first such index would be a panic rather than a refusal.
//!
//! So this runs first, over the whole program, and [`verify`](super::verify)
//! goes no further when it finds anything. It checks *references between
//! tables* only: that a [`LayoutId`], [`FunctionId`], [`StrId`], [`ArgsId`],
//! [`TableId`] or [`HostOpId`] is in range wherever one is held. What the rows
//! say, and whether slots and counters are in range, is the rest of the
//! verifier's business and is checked there, after this has made it safe to
//! look.

use super::Invalid;
use crate::inst::{Inst, Storage};
use crate::layout::{LayoutId, Shape};
use crate::program::{ArgsId, FunctionId, HostOpId, Program, StrId, TableId};

/// Every out-of-range id in `program`, or nothing.
pub(super) fn check(program: &Program) -> Vec<Invalid> {
    let mut faults = Vec::new();
    let sizes = Sizes {
        functions: program.functions.len(),
        layouts: program.layouts.len(),
        strings: program.strings.len(),
        args: program.args.len(),
        tables: program.tables.len(),
        host_ops: program.host_ops.len(),
    };

    let mut scope = Scope {
        sizes: &sizes,
        function: "<program>".to_string(),
        pc: None,
        uses_constants: false,
        faults: &mut faults,
    };
    for (index, layout) in program.layouts.iter().enumerate() {
        scope.function = format!("<layout {index}>");
        match &layout.shape {
            Shape::Free
            | Shape::Word(_)
            | Shape::Str
            | Shape::Bytes
            | Shape::ByteBuffer
            | Shape::Boxed
            | Shape::IdentityTable => {}
            Shape::Struct { fields, .. } => {
                for field in fields {
                    scope.layout("a field's layout", field.layout);
                }
            }
            Shape::Enum { cases, .. } => {
                for case in cases {
                    for part in &case.parts {
                        scope.layout("a case part's layout", part.layout);
                    }
                }
            }
            Shape::Elements { elem, .. } | Shape::Vector { elem } | Shape::Members { elem } => {
                scope.layout("an element layout", *elem);
            }
            Shape::Entries { key, value } => {
                scope.layout("a key layout", *key);
                scope.layout("a value layout", *value);
            }
            // `function` is compared, never indexed, by the verifier.
            Shape::Closure { captures, .. } => {
                for capture in captures {
                    scope.layout("a capture's layout", *capture);
                }
            }
            Shape::Shared { value } => scope.layout("a shared value's layout", *value),
        }
    }
    scope.function = "<program>".to_string();
    for names in &program.names {
        if let Some(name) = names.name {
            scope.string("a layout's name", name);
        }
        for part in &names.parts {
            scope.string("a layout part's name", *part);
        }
    }
    for resource in &program.resource_names {
        scope.string("a resource's name", resource.text);
    }
    for ((module, name), id) in &program.by_name {
        scope.function_id(&format!("the entry `{module}.{name}`"), *id);
    }
    for list in &program.args {
        for arg in list {
            scope.layout("an argument's layout", arg.layout);
        }
    }
    for op in &program.host_ops {
        scope.layout("a host operation's result layout", op.result);
    }

    for function in &program.functions {
        scope.function = format!("{}.{}", function.module, function.name);
        for id in &function.params {
            scope.layout("a parameter's layout", *id);
        }
        scope.layout("the return layout", function.returns);
        for capture in &function.captures {
            scope.layout("a capture's layout", capture.layout);
        }
        let inlined_locals = function.inlined.iter().flat_map(|inlined| &inlined.locals);
        for local in function.locals.iter().chain(inlined_locals) {
            scope.layout("a local's layout", local.layout);
        }
        for inlined in &function.inlined {
            scope.function_id("an expansion's callee", inlined.callee);
        }
        for (pc, inst) in function.code.iter().enumerate() {
            scope.pc = Some(pc);
            inst_ids(inst, &mut scope);
        }
        scope.pc = None;
    }
    scope.function = "<program>".to_string();
    // The program-wide layouts are read only by the instructions that name no
    // layout of their own, so a program with no layouts at all and none of
    // those instructions (a hand-built fixture, say) is left alone, and
    // anything else must have them in range.
    if !program.layouts.is_empty() || scope.uses_constants {
        for (what, id) in [
            ("str_layout", program.str_layout),
            ("bytes_layout", program.bytes_layout),
            ("buffer_layout", program.buffer_layout),
            ("boxed_layout", program.boxed_layout),
            ("view_layout", program.view_layout),
            ("render_path_layout", program.render_path_layout),
            ("identity_set_layout", program.identity_set_layout),
            ("identity_table_layout", program.identity_table_layout),
        ] {
            scope.layout(what, id);
        }
    }
    faults
}

struct Sizes {
    functions: usize,
    layouts: usize,
    strings: usize,
    args: usize,
    tables: usize,
    host_ops: usize,
}

struct Scope<'a> {
    sizes: &'a Sizes,
    function: String,
    pc: Option<usize>,
    /// Whether any instruction read so far uses a program-wide layout.
    uses_constants: bool,
    faults: &'a mut Vec<Invalid>,
}

impl Scope<'_> {
    fn fault(&mut self, what: &str, id: String, len: usize, table: &str) {
        self.faults.push(Invalid {
            function: self.function.clone(),
            pc: self.pc,
            what: format!("{what} is {id}, but the program has {len} {table}"),
        });
    }

    fn layout(&mut self, what: &str, id: LayoutId) {
        if id.index() >= self.sizes.layouts {
            self.fault(what, id.to_string(), self.sizes.layouts, "layouts");
        }
    }

    fn function_id(&mut self, what: &str, id: FunctionId) {
        if id.index() >= self.sizes.functions {
            self.fault(what, id.to_string(), self.sizes.functions, "functions");
        }
    }

    fn string(&mut self, what: &str, id: StrId) {
        if id.index() >= self.sizes.strings {
            self.fault(what, id.to_string(), self.sizes.strings, "strings");
        }
    }

    fn args(&mut self, id: ArgsId) {
        if id.index() >= self.sizes.args {
            let len = self.sizes.args;
            self.fault("an argument list", id.to_string(), len, "argument lists");
        }
    }

    fn table(&mut self, id: TableId) {
        if id.index() >= self.sizes.tables {
            let len = self.sizes.tables;
            self.fault("a switch table", id.to_string(), len, "switch tables");
        }
    }

    fn host_op(&mut self, id: HostOpId) {
        if id.index() >= self.sizes.host_ops {
            let len = self.sizes.host_ops;
            self.fault("a host operation", id.to_string(), len, "host operations");
        }
    }

    fn storage(&mut self, storage: Storage) {
        if let Storage::Words(layout) = storage {
            self.layout("a run's element layout", layout);
        }
    }
}

/// Reports every id `inst` holds. Written without a wildcard arm, so a new
/// instruction does not compile until it has been considered here.
fn inst_ids(inst: &Inst, scope: &mut Scope<'_>) {
    scope.uses_constants |= matches!(
        inst,
        Inst::Str { .. }
            | Inst::Box { .. }
            | Inst::Unbox { .. }
            | Inst::RunLoad { .. }
            | Inst::RunCopy { .. }
            | Inst::RunSlice { .. }
            | Inst::RunFind { .. }
            | Inst::GrowableAlloc { .. }
            | Inst::GrowableEnsure { .. }
            | Inst::GrowableCommit { .. }
            | Inst::RunStore { .. }
            | Inst::GrowableTruncate { .. }
            | Inst::RunFinish { .. }
            | Inst::DynOpen { .. }
            | Inst::DynKind { .. }
            | Inst::DynSameType { .. }
            | Inst::DynSameObject { .. }
            | Inst::DynIdentitySet { .. }
            | Inst::DynIdentityEnter { .. }
            | Inst::DynIdentityLeave { .. }
            | Inst::DynNameOrder { .. }
            | Inst::DynRead { .. }
            | Inst::DynCase { .. }
            | Inst::DynCount { .. }
            | Inst::DynChild { .. }
            | Inst::HandleText { .. }
            | Inst::DynTypeName { .. }
            | Inst::DynFieldName { .. }
            | Inst::DynCaseName { .. }
            | Inst::DynOpaque { .. }
            | Inst::DynHandleText { .. }
            | Inst::DynOnPath { .. }
    );
    match inst {
        Inst::FuncRef { callee, .. } => scope.function_id("a function reference", *callee),
        Inst::Tag { layout, .. }
        | Inst::Copy { layout, .. }
        | Inst::Clear { layout, .. }
        | Inst::Alloc { layout, .. }
        | Inst::LoadField { layout, .. }
        | Inst::StoreField { layout, .. }
        | Inst::LoadElem { layout, .. }
        | Inst::StoreElem { layout, .. }
        | Inst::Load { layout, .. }
        | Inst::Store { layout, .. }
        | Inst::Box { layout, .. }
        | Inst::Unbox { layout, .. }
        | Inst::ScopeLeave { layout, .. } => scope.layout("an instruction's layout", *layout),
        // `Str`'s id is checked where it is read, in `Check::check_inst`, in
        // words that say which instruction it is.
        Inst::Str { .. } => {}
        Inst::ScopeEnter { name, .. } => scope.string("a scope's name", *name),
        Inst::Switch { table, .. } => scope.table(*table),
        Inst::Call { callee, args, .. } => {
            scope.function_id("a callee", *callee);
            scope.args(*args);
        }
        Inst::CallClosure { args, result, .. } => {
            scope.args(*args);
            scope.layout("a closure call's result layout", *result);
        }
        Inst::CallHost { op, args, .. } | Inst::CallResource { op, args, .. } => {
            scope.host_op(*op);
            scope.args(*args);
        }
        Inst::RunCopy { args, storage }
        | Inst::RunSlice { args, storage }
        | Inst::RunFind { args, storage } => {
            scope.args(*args);
            scope.storage(*storage);
        }
        Inst::RunLoad { storage, .. }
        | Inst::GrowableAlloc { storage, .. }
        | Inst::GrowableEnsure { storage, .. }
        | Inst::GrowableCommit { storage, .. }
        | Inst::RunStore { storage, .. }
        | Inst::GrowableTruncate { storage, .. } => scope.storage(*storage),
        Inst::RunFinish {
            target, storage, ..
        } => {
            scope.layout("a run's target layout", *target);
            scope.storage(*storage);
        }
        Inst::Spawn { answer, .. } | Inst::Await { answer, .. } | Inst::Settled { answer, .. } => {
            scope.layout("a task's answer layout", *answer)
        }
        Inst::Unit { .. }
        | Inst::Bool { .. }
        | Inst::Int { .. }
        | Inst::Float { .. }
        | Inst::Neg { .. }
        | Inst::Arith { .. }
        | Inst::Cmp { .. }
        | Inst::ArithImm { .. }
        | Inst::CmpImm { .. }
        | Inst::Not { .. }
        | Inst::Convert { .. }
        | Inst::FloatAbs { .. }
        | Inst::FloatMinMax { .. }
        | Inst::FloatRound { .. }
        | Inst::FloatSqrt { .. }
        | Inst::FloatTruncate { .. }
        | Inst::Bits { .. }
        | Inst::BitNot { .. }
        | Inst::Shift { .. }
        | Inst::Jump { .. }
        | Inst::BranchFalse { .. }
        | Inst::CmpBranch { .. }
        | Inst::CmpImmBranch { .. }
        | Inst::Return { .. }
        | Inst::Len { .. }
        | Inst::AddrOfSlot { .. }
        | Inst::AddrOfField { .. }
        | Inst::AddrOfPart { .. }
        | Inst::DynOpen { .. }
        | Inst::DynKind { .. }
        | Inst::DynSameType { .. }
        | Inst::DynSameObject { .. }
        | Inst::DynIdentitySet { .. }
        | Inst::DynIdentityEnter { .. }
        | Inst::DynIdentityLeave { .. }
        | Inst::DynNameOrder { .. }
        | Inst::DynRead { .. }
        | Inst::DynCase { .. }
        | Inst::DynCount { .. }
        | Inst::DynChild { .. }
        | Inst::HandleText { .. }
        | Inst::DynTypeName { .. }
        | Inst::DynFieldName { .. }
        | Inst::DynCaseName { .. }
        | Inst::DynOpaque { .. }
        | Inst::DynHandleText { .. }
        | Inst::DynOnPath { .. }
        | Inst::ScopeCancel { .. }
        | Inst::Cancel { .. }
        | Inst::SharedLock { .. }
        | Inst::SharedUnlock { .. }
        | Inst::Trap { .. }
        | Inst::AssertFailed { .. } => {}
    }
}
