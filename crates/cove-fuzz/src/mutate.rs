//! Edits to a lowered program, for the `ir_mutate` target ([ADR 0120]).
//!
//! [`crate::irgen`] builds programs from nothing, and they look like nothing
//! the lowering writes. This starts from what the lowering wrote — a corpus
//! program, carried in the input as an image body — and changes a few things
//! in it: one operand of one instruction, a frame's `Repr`, a layout's field,
//! a parameter, an argument row, a jump table, a name's string id, an
//! instruction moved, copied or removed. Most edits make a program `cove_ir::verify` refuses, and that
//! is half the point: the refusal has to be the machine's too. The rest are
//! programs one step away from a real one, which no generator would write.
//!
//! # The input
//!
//! A little-endian `u32` length, that many bytes of image body (what
//! [`crate::image`] takes: the header is supplied), and then the choices the
//! edits are made with. The `ir_mutate` fuzz target mutates only the choices
//! (see `fuzz/fuzz_targets/ir_mutate.rs`), so a seed's body stays a program.
//!
//! [ADR 0120]: ../../../docs/adr/0120-the-vm-runs-only-what-verifies.md

use arbitrary::Unstructured;
use cove_ir::{
    ArgsId, CaseId, FunctionId, HostOpId, Inst, Layout, LayoutId, Len, Program, RefMap, Repr,
    Shape, StrId, TableId,
};

use crate::ir::Subject;

/// How many bytes of a `ir_mutate` input are its body: the first four, read
/// as a little-endian `u32`.
pub const LENGTH_BYTES: usize = 4;

/// An `ir_mutate` input: `body` behind its length, then `choices`.
pub fn input(body: &[u8], choices: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(LENGTH_BYTES + body.len() + choices.len());
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(body);
    bytes.extend_from_slice(choices);
    bytes
}

/// Where an input's choices begin, if it holds a whole body.
pub fn choices_at(data: &[u8]) -> Option<usize> {
    let length = u32::from_le_bytes(data.get(..LENGTH_BYTES)?.try_into().ok()?) as usize;
    let end = LENGTH_BYTES.checked_add(length)?;
    (end <= data.len()).then_some(end)
}

/// The edited program `data` describes, or `None` if its body does not read.
pub fn from_bytes(data: &[u8]) -> Option<Subject> {
    let end = choices_at(data)?;
    let body = &data[LENGTH_BYTES..end];
    let image = cove_ir::serial::decode(&crate::with_header(body)).ok()?;
    let mut subject = crate::ir::image_subject(image);
    let mut edit = Edit {
        u: Unstructured::new(&data[end..]),
        program: &mut subject.program,
    };
    let edits = 1 + edit.below(4);
    for _ in 0..edits {
        edit.one();
    }
    Some(subject)
}

/// Values a number is most likely to go wrong at, for any operand.
const EDGES: [i64; 12] = [
    0,
    1,
    -1,
    2,
    63,
    64,
    255,
    65_535,
    65_536,
    i64::MAX,
    i64::MIN,
    u32::MAX as i64,
];

struct Edit<'a, 'p> {
    u: Unstructured<'a>,
    program: &'p mut Program,
}

/// One operand of an instruction, as something an edit can change.
enum Operand<'i> {
    Slot(&'i mut u32),
    /// A program counter, or a word offset: any `u32` that is not an id.
    Number(&'i mut u32),
    Layout(&'i mut LayoutId),
    Function(&'i mut FunctionId),
    Str(&'i mut StrId),
    Args(&'i mut ArgsId),
    Table(&'i mut TableId),
    Host(&'i mut HostOpId),
    Case(&'i mut CaseId),
    Int(&'i mut i64),
    Small(&'i mut i32),
    Bits(&'i mut u64),
}

/// Every operand of `inst`. No wildcard arm, so that a new instruction's
/// operands are decided here before it compiles.
fn operands(inst: &mut Inst) -> Vec<Operand<'_>> {
    use Operand::*;
    match inst {
        Inst::Unit { dst } => vec![Slot(dst)],
        Inst::Bool { dst, .. } => vec![Slot(dst)],
        Inst::Int { dst, value } => vec![Slot(dst), Int(value)],
        Inst::FuncRef { dst, callee } => vec![Slot(dst), Function(callee)],
        Inst::Tag { dst, layout, case } => vec![Slot(dst), Layout(layout), Case(case)],
        Inst::Float { dst, bits } => vec![Slot(dst), Bits(bits)],
        Inst::Str { dst, text } => vec![Slot(dst), Str(text)],
        Inst::Copy { dst, src, layout } => vec![Slot(dst), Slot(src), Layout(layout)],
        Inst::Clear { slot, layout } => vec![Slot(slot), Layout(layout)],
        Inst::Neg { dst, a, .. } => vec![Slot(dst), Slot(a)],
        Inst::Arith { dst, a, b, .. } => vec![Slot(dst), Slot(a), Slot(b)],
        Inst::Cmp { dst, a, b, .. } => vec![Slot(dst), Slot(a), Slot(b)],
        Inst::ArithImm { dst, a, value, .. } => vec![Slot(dst), Slot(a), Int(value)],
        Inst::CmpImm { dst, a, value, .. } => vec![Slot(dst), Slot(a), Int(value)],
        Inst::Not { dst, a } => vec![Slot(dst), Slot(a)],
        Inst::Convert { dst, a, .. } => vec![Slot(dst), Slot(a)],
        Inst::FloatAbs { dst, a } => vec![Slot(dst), Slot(a)],
        Inst::FloatMinMax { dst, a, b, .. } => vec![Slot(dst), Slot(a), Slot(b)],
        Inst::FloatRound { dst, a } => vec![Slot(dst), Slot(a)],
        Inst::FloatSqrt { dst, a } => vec![Slot(dst), Slot(a)],
        Inst::FloatTruncate { dst, ok, a } => vec![Slot(dst), Slot(ok), Slot(a)],
        Inst::Bits { dst, a, b, .. } => vec![Slot(dst), Slot(a), Slot(b)],
        Inst::BitNot { dst, a } => vec![Slot(dst), Slot(a)],
        Inst::Shift { dst, a, n, .. } => vec![Slot(dst), Slot(a), Slot(n)],
        Inst::Jump { to } => vec![Number(to)],
        Inst::BranchFalse { cond, to } => vec![Slot(cond), Number(to)],
        Inst::CmpBranch {
            dst, a, b, target, ..
        } => vec![Slot(dst), Slot(a), Slot(b), Number(target)],
        Inst::CmpImmBranch {
            dst,
            a,
            value,
            target,
            ..
        } => vec![Slot(dst), Slot(a), Small(value), Number(target)],
        Inst::Switch { on, table } => vec![Slot(on), Table(table)],
        Inst::Return { src } => vec![Slot(src)],
        Inst::Call { dst, callee, args } => vec![Slot(dst), Function(callee), Args(args)],
        Inst::CallClosure {
            dst,
            closure,
            args,
            result,
        } => vec![Slot(dst), Slot(closure), Args(args), Layout(result)],
        Inst::CallHost { dst, op, args } => vec![Slot(dst), Host(op), Args(args)],
        Inst::CallResource {
            dst,
            receiver,
            op,
            args,
        } => vec![Slot(dst), Slot(receiver), Host(op), Args(args)],
        Inst::Alloc { dst, layout, len } => {
            let mut held = vec![Slot(dst), Layout(layout)];
            match len {
                Len::Fixed => {}
                Len::Count(n) => held.push(Number(n)),
                Len::Slot(slot) => held.push(Slot(slot)),
            }
            held
        }
        Inst::LoadField {
            dst,
            obj,
            at,
            layout,
        } => vec![Slot(dst), Slot(obj), Number(at), Layout(layout)],
        Inst::StoreField {
            obj,
            at,
            src,
            layout,
        } => vec![Slot(obj), Number(at), Slot(src), Layout(layout)],
        Inst::LoadElem {
            dst,
            obj,
            index,
            layout,
        } => vec![Slot(dst), Slot(obj), Slot(index), Layout(layout)],
        Inst::StoreElem {
            obj,
            index,
            src,
            layout,
        } => vec![Slot(obj), Slot(index), Slot(src), Layout(layout)],
        Inst::RunLoad {
            dst, run, index, ..
        } => vec![Slot(dst), Slot(run), Slot(index)],
        Inst::RunCopy { args, storage } => storage_operands(vec![Args(args)], storage),
        Inst::RunSlice { args, storage } => storage_operands(vec![Args(args)], storage),
        Inst::RunFind { args, storage } => storage_operands(vec![Args(args)], storage),
        Inst::GrowableAlloc {
            dst,
            capacity,
            storage,
        } => storage_operands(vec![Slot(dst), Slot(capacity)], storage),
        Inst::GrowableEnsure {
            owner,
            additional,
            storage,
        } => storage_operands(vec![Slot(owner), Slot(additional)], storage),
        Inst::GrowableCommit {
            owner,
            count,
            storage,
        } => storage_operands(vec![Slot(owner), Slot(count)], storage),
        Inst::RunStore {
            run,
            index,
            src,
            storage,
        } => storage_operands(vec![Slot(run), Slot(index), Slot(src)], storage),
        Inst::GrowableTruncate {
            owner,
            len,
            storage,
        } => storage_operands(vec![Slot(owner), Slot(len)], storage),
        Inst::RunFinish {
            dst,
            owner,
            target,
            storage,
            ..
        } => storage_operands(vec![Slot(dst), Slot(owner), Layout(target)], storage),
        Inst::Len { dst, obj } => vec![Slot(dst), Slot(obj)],
        Inst::AddrOfSlot { dst, slot } => vec![Slot(dst), Slot(slot)],
        Inst::AddrOfField { dst, obj, at } => vec![Slot(dst), Slot(obj), Number(at)],
        Inst::AddrOfPart { dst, addr, at } => vec![Slot(dst), Slot(addr), Number(at)],
        Inst::Load { dst, addr, layout } => vec![Slot(dst), Slot(addr), Layout(layout)],
        Inst::Store { addr, src, layout } => vec![Slot(addr), Slot(src), Layout(layout)],
        Inst::Box { dst, src, layout } => vec![Slot(dst), Slot(src), Layout(layout)],
        Inst::Unbox { dst, src, layout } => vec![Slot(dst), Slot(src), Layout(layout)],
        Inst::DynOpen { dst, src } => vec![Slot(dst), Slot(src)],
        Inst::DynKind { dst, view } => vec![Slot(dst), Slot(view)],
        Inst::DynSameType { dst, a, b } => vec![Slot(dst), Slot(a), Slot(b)],
        Inst::DynSameObject { dst, a, b } => vec![Slot(dst), Slot(a), Slot(b)],
        Inst::DynIdentitySet { dst } => vec![Slot(dst)],
        Inst::DynIdentityEnter { set, a, b } => vec![Slot(set), Slot(a), Slot(b)],
        Inst::DynIdentityLeave { set, a, b } => vec![Slot(set), Slot(a), Slot(b)],
        Inst::DynNameOrder { dst, a, b } => vec![Slot(dst), Slot(a), Slot(b)],
        Inst::DynRead { dst, view } => vec![Slot(dst), Slot(view)],
        Inst::DynCase { dst, view } => vec![Slot(dst), Slot(view)],
        Inst::DynCount { dst, view } => vec![Slot(dst), Slot(view)],
        Inst::DynChild { dst, view, index } => vec![Slot(dst), Slot(view), Slot(index)],
        Inst::HandleText { dst, src } => vec![Slot(dst), Slot(src)],
        Inst::DynTypeName { dst, view } => vec![Slot(dst), Slot(view)],
        Inst::DynFieldName { dst, view, index } => vec![Slot(dst), Slot(view), Slot(index)],
        Inst::DynCaseName { dst, view } => vec![Slot(dst), Slot(view)],
        Inst::DynOpaque { dst, view } => vec![Slot(dst), Slot(view)],
        Inst::DynHandleText { dst, view } => vec![Slot(dst), Slot(view)],
        Inst::DynOnPath { dst, view, path } => vec![Slot(dst), Slot(view), Slot(path)],
        Inst::ScopeEnter { dst, name } => vec![Slot(dst), Str(name)],
        Inst::ScopeLeave {
            scope,
            failed,
            error,
            layout,
        } => vec![Slot(scope), Slot(failed), Slot(error), Layout(layout)],
        Inst::ScopeCancel { scope } => vec![Slot(scope)],
        Inst::Spawn {
            dst,
            scope,
            closure,
            answer,
        } => vec![Slot(dst), Slot(scope), Slot(closure), Layout(answer)],
        Inst::Await { dst, task, answer } => vec![Slot(dst), Slot(task), Layout(answer)],
        Inst::Cancel { task } => vec![Slot(task)],
        Inst::Settled { dst, src, answer } => vec![Slot(dst), Slot(src), Layout(answer)],
        Inst::SharedLock { cell } => vec![Slot(cell)],
        Inst::SharedUnlock { cell } => vec![Slot(cell)],
        Inst::Trap {
            message,
            rule,
            help,
        } => vec![Slot(message), Slot(rule), Slot(help)],
        Inst::AssertFailed { message } => vec![Slot(message)],
    }
}

fn storage_operands<'i>(
    mut held: Vec<Operand<'i>>,
    storage: &'i mut cove_ir::Storage,
) -> Vec<Operand<'i>> {
    if let cove_ir::Storage::Words(layout) = storage {
        held.push(Operand::Layout(layout));
    }
    held
}

impl Edit<'_, '_> {
    fn byte(&mut self) -> u8 {
        self.u.arbitrary().unwrap_or(0)
    }

    fn below(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        self.u.choose_index(n).unwrap_or(0)
    }

    /// A number near `was` or near `len`, or an edge, or past everything.
    fn index_near(&mut self, was: u32, len: usize) -> u32 {
        match self.byte() % 6 {
            0 => was.wrapping_add(1),
            1 => was.wrapping_sub(1),
            2 => self.below(len.max(1)) as u32,
            3 => len as u32,
            4 => EDGES[self.below(EDGES.len())] as u32,
            _ => self.u.arbitrary().unwrap_or(0),
        }
    }

    fn one(&mut self) {
        if self.program.functions.is_empty() {
            return;
        }
        let which = self.byte();
        // The four values `% 12` would map a second time onto `0..=3`, so
        // that adding this edit left every other byte's edit as it was.
        if which >= 252 {
            return self.name();
        }
        match which % 12 {
            0..=4 => self.operand(),
            5 => self.repr(),
            6 => self.layout(),
            7 => self.signature(),
            8 => self.arg_row(),
            9 => self.table(),
            10 => self.move_instruction(),
            _ => self.copy_instruction(),
        }
    }

    /// A string id a name holds — a layout's rendered name, a field's or a
    /// case's, a resource's — moved to the string table's boundary: its last
    /// row, its length, one past it, or near what it was. Those ids are held
    /// outside any instruction, so [`Edit::operand`] never reaches them, and
    /// the lowering never writes one past the table; `cove_ir::verify` has to
    /// refuse it (mutant 013, docs/FUZZ_EVALUATION.md section 12). A program
    /// with no names is given one, for the first layout.
    fn name(&mut self) {
        let len = self.program.strings.len();
        let was = |id: Option<StrId>| id.map_or(0, |id| id.0);
        let to = |edit: &mut Self, was: u32| match edit.byte() % 4 {
            0 => len.saturating_sub(1) as u32,
            1 => len as u32,
            2 => len as u32 + 1,
            _ => edit.index_near(was, len),
        };
        let rows = self.program.names.len();
        let resources = self.program.resource_names.len();
        if resources > 0 && (rows == 0 || self.byte() & 1 == 0) {
            let at = self.below(resources);
            let id = to(self, self.program.resource_names[at].text.0);
            self.program.resource_names[at].text = StrId(id);
            return;
        }
        if rows == 0 {
            let id = to(self, 0);
            self.program.names.push(cove_ir::LayoutNames {
                name: Some(StrId(id)),
                parts: Vec::new(),
            });
            return;
        }
        let at = self.below(rows);
        let parts = self.program.names[at].parts.len();
        let part = self.below(parts + 1);
        if part < parts {
            let id = to(self, self.program.names[at].parts[part].0);
            self.program.names[at].parts[part] = StrId(id);
        } else {
            let id = to(self, was(self.program.names[at].name));
            self.program.names[at].name = Some(StrId(id));
        }
    }

    /// A function with code, and a pc in it.
    fn site(&mut self) -> Option<(usize, usize)> {
        let f = self.below(self.program.functions.len());
        let len = self.program.functions[f].code.len();
        (len > 0).then(|| (f, self.below(len)))
    }

    fn operand(&mut self) {
        let Some((f, pc)) = self.site() else {
            return;
        };
        let frame = self.program.functions[f].reprs.len();
        let code_len = self.program.functions[f].code.len();
        let counts = (
            self.program.layouts.len(),
            self.program.functions.len(),
            self.program.strings.len(),
            self.program.args.len(),
            self.program.tables.len(),
            self.program.host_ops.len(),
        );
        let mut inst = self.program.functions[f].code[pc].clone();
        let mut ops = operands(&mut inst);
        if ops.is_empty() {
            return;
        }
        let at = self.below(ops.len());
        let choice = self.byte();
        let pick = |edit: &mut Self, was: u32, len: usize| -> u32 {
            if choice.is_multiple_of(7) {
                edit.index_near(was, len)
            } else {
                edit.below(len.max(1)) as u32
            }
        };
        match &mut ops[at] {
            Operand::Slot(slot) => **slot = pick(self, **slot, frame),
            Operand::Number(n) => **n = pick(self, **n, code_len.max(8)),
            Operand::Layout(id) => id.0 = pick(self, id.0, counts.0),
            Operand::Function(id) => id.0 = pick(self, id.0, counts.1),
            Operand::Str(id) => id.0 = pick(self, id.0, counts.2),
            Operand::Args(id) => id.0 = pick(self, id.0, counts.3),
            Operand::Table(id) => id.0 = pick(self, id.0, counts.4),
            Operand::Host(id) => id.0 = pick(self, id.0, counts.5),
            Operand::Case(id) => id.0 = pick(self, id.0, 4),
            Operand::Int(v) => **v = EDGES[self.below(EDGES.len())],
            Operand::Small(v) => **v = EDGES[self.below(EDGES.len())] as i32,
            Operand::Bits(v) => **v = self.u.arbitrary().unwrap_or(0),
        }
        drop(ops);
        self.program.functions[f].code[pc] = inst;
    }

    fn repr(&mut self) {
        let f = self.below(self.program.functions.len());
        let function = &mut self.program.functions[f];
        if function.reprs.is_empty() {
            return;
        }
        let reprs = [
            Repr::Unit,
            Repr::Bool,
            Repr::Int,
            Repr::Float,
            Repr::Duration,
            Repr::Ref,
            Repr::Addr,
            Repr::Host,
            Repr::Task,
            Repr::Scope,
            Repr::Tag,
        ];
        let at = self.u.choose_index(function.reprs.len()).unwrap_or(0);
        let repr = reprs[self.u.choose_index(reprs.len()).unwrap_or(0)];
        let function = &mut self.program.functions[f];
        match self.u.arbitrary::<u8>().unwrap_or(0) % 4 {
            // A slot more or fewer, as a frame size that disagrees with
            // everything that was numbered against it.
            0 => function.reprs.push(repr),
            1 => {
                function.reprs.pop();
            }
            _ => function.reprs[at] = repr,
        }
        // Mostly keep the map the reprs imply, so that the edit is to what a
        // slot means and not only to the map's agreement with it.
        if self.u.ratio(7, 8).unwrap_or(true) {
            let function = &mut self.program.functions[f];
            function.refs = RefMap::of(&function.reprs);
        }
    }

    fn layout(&mut self) {
        let count = self.program.layouts.len();
        if count == 0 {
            return;
        }
        let at = self.below(count);
        let other = LayoutId(self.below(count + 1) as u32);
        let n = self.byte();
        let layout: &mut Layout = &mut self.program.layouts[at];
        match &mut layout.shape {
            Shape::Struct { fields, .. } if !fields.is_empty() => {
                let i = n as usize % fields.len();
                if n & 0x80 == 0 {
                    fields[i].at = fields[i].at.wrapping_add(u32::from(n & 3) + 1);
                } else {
                    fields[i].layout = other;
                }
            }
            Shape::Enum { cases, .. } if !cases.is_empty() => {
                let i = n as usize % cases.len();
                if let Some(part) = cases[i].parts.first_mut() {
                    if n & 0x80 == 0 {
                        part.at = part.at.wrapping_add(1);
                    } else {
                        part.layout = other;
                    }
                } else {
                    cases.pop();
                }
            }
            Shape::Elements { elem, .. }
            | Shape::Vector { elem }
            | Shape::Members { elem }
            | Shape::Shared { value: elem } => *elem = other,
            Shape::Entries { key, value } => {
                if n & 1 == 0 {
                    *key = other
                } else {
                    *value = other
                }
            }
            Shape::Closure { function, captures } => {
                if n & 1 == 0 || captures.is_empty() {
                    function.0 = function.0.wrapping_add(1);
                } else {
                    captures[0] = other;
                }
            }
            _ => {
                // The words themselves: one more, one fewer, or one changed.
                match n % 3 {
                    0 => layout.words.push(Repr::Ref),
                    1 => {
                        layout.words.pop();
                    }
                    _ => {
                        if let Some(word) = layout.words.first_mut() {
                            *word = if *word == Repr::Ref {
                                Repr::Int
                            } else {
                                Repr::Ref
                            };
                        }
                    }
                }
            }
        }
    }

    fn signature(&mut self) {
        let f = self.below(self.program.functions.len());
        let other = LayoutId(self.below(self.program.layouts.len() + 1) as u32);
        let n = self.byte();
        let function = &mut self.program.functions[f];
        match n % 4 {
            0 => function.returns = other,
            1 => function.params.push(other),
            2 => {
                function.params.pop();
            }
            _ => {
                if let Some(param) = function.params.first_mut() {
                    *param = other;
                }
            }
        }
    }

    fn arg_row(&mut self) {
        if self.program.args.is_empty() {
            return;
        }
        let row = self.below(self.program.args.len());
        let other = LayoutId(self.below(self.program.layouts.len() + 1) as u32);
        let n = self.byte();
        let slot = u32::from(self.byte());
        let held = &mut self.program.args[row];
        match n % 4 {
            0 => {
                held.pop();
            }
            1 => held.push(cove_ir::Arg {
                slot,
                layout: other,
            }),
            2 => {
                if let Some(arg) = held.first_mut() {
                    arg.slot = slot;
                }
            }
            _ => {
                if let Some(arg) = held.last_mut() {
                    arg.layout = other;
                }
            }
        }
    }

    fn table(&mut self) {
        if self.program.tables.is_empty() {
            return;
        }
        let at = self.below(self.program.tables.len());
        let to = u32::from(self.byte());
        let n = self.byte();
        let table = &mut self.program.tables[at];
        match n % 3 {
            0 => table.default = to,
            1 => table.targets.push(to),
            _ => {
                table.targets.pop();
            }
        }
    }

    /// Two instructions of one function trade places, or one is removed.
    fn move_instruction(&mut self) {
        let Some((f, pc)) = self.site() else {
            return;
        };
        let other = self.below(self.program.functions[f].code.len());
        let remove = self.byte().is_multiple_of(4);
        let function = &mut self.program.functions[f];
        if remove && function.code.len() > 1 {
            function.code.remove(pc);
            if pc < function.spans.len() {
                function.spans.remove(pc);
            }
        } else {
            function.code.swap(pc, other);
        }
    }

    /// An instruction from anywhere in the program written over one here,
    /// so that its operands are numbered against another frame.
    fn copy_instruction(&mut self) {
        let Some((from, from_pc)) = self.site() else {
            return;
        };
        let Some((to, to_pc)) = self.site() else {
            return;
        };
        let inst = self.program.functions[from].code[from_pc].clone();
        self.program.functions[to].code[to_pc] = inst;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program small enough to edit quickly, which renders a struct, so
    /// that the lowering placed names for it.
    const SOURCE: &str = "use console.println\n\nstruct Point {\n  x: Int\n  y: Int\n}\n\n/// m\nexport fn main() -> Result<Unit, Error> {\n  println(\"{Point(x: 1, y: 2)}\")?\n  Ok(())\n}\n";

    /// The names edit moves a name to each side of the string table's
    /// boundary within the first few thousand fixed choices, and a clean
    /// `verify` refuses every edited program that names a string past the
    /// table — the target can explore mutant 013's boundary, and the clean
    /// build does not mistake it for a finding.
    #[test]
    fn a_name_is_moved_to_each_side_of_the_string_tables_end() {
        let body = crate::image_body(SOURCE).expect("the source lowers to an image");
        let mut reached = [false; 3];
        for seed in 0..4_000u64 {
            let data = input(&body, &crate::ir::choices(seed, 24));
            let subject = from_bytes(&data).expect("the body reads");
            let program = &subject.program;
            let len = program.strings.len();
            let ids: Vec<usize> = program
                .names
                .iter()
                .flat_map(|names| names.name.iter().chain(&names.parts))
                .chain(program.resource_names.iter().map(|named| &named.text))
                .map(|id| id.index())
                .collect();
            for (at, want) in [len - 1, len, len + 1].into_iter().enumerate() {
                reached[at] |= ids.contains(&want);
            }
            if ids.iter().any(|&id| id >= len) {
                assert!(
                    cove_ir::verify(program).is_err(),
                    "seed {seed}: a name past the table verified"
                );
                let (_, finding) = crate::ir::ir_mutate_classified(&data);
                assert_eq!(finding, None, "seed {seed}");
            }
            if reached == [true; 3] {
                return;
            }
        }
        panic!("the boundary was not reached on every side: {reached:?}");
    }
}
