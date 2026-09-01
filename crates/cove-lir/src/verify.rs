//! A static check that a lowered program is well formed.
//!
//! The machine takes the lowering's word for a great deal: that a value
//! location fits the frame it is in, that a jump lands on an instruction,
//! that a call passes the layouts the callee declares, and — the one that
//! matters most — that a slot's [`Repr`] is what [`Function::refs`] says it
//! is. A collection walks frames using that map, so a lowering that wrote a
//! reference into a slot the map calls an `Int` would produce a dangling
//! reference at the next collection and a wrong answer some time after that.
//!
//! # A location agrees with its layout, word for word
//!
//! Every instruction that moves a value names the layout it is moving, and a
//! layout is a run of [`Repr`]s. So the check is not "the destination is a
//! reference" but "the destination's words *are* the layout's words, in
//! order". That is what makes the one-value-many-slots rule checkable: a
//! `Copy` of a three-word `Wrapper` into a location whose second word is a
//! `Float` is a fault here rather than a `Float` traced as a pointer later.
//!
//! This is where those assumptions are checked, once, before anything runs.
//! It is not a type checker: `cove-sema` already did that, and a failure here
//! is a bug in the lowering rather than a fault in the program. It exists so
//! that such a bug is a loud failure at lowering time instead of a quiet one
//! at collection time.

use crate::inst::{Compare, Inst, Len, Num, Slot};
use crate::layout::LayoutId;
use crate::program::{Function, FunctionId, Program};
use crate::repr::{RefMap, Repr};

/// A way in which a lowered program is not well formed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invalid {
    /// The function the fault is in, as `module.name`.
    pub function: String,
    /// The instruction it is at, or `None` when the fault is the function's
    /// own — a frame whose reference map disagrees with its reprs, say.
    pub pc: Option<usize>,
    pub what: String,
}

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.pc {
            Some(pc) => write!(f, "{}+{pc}: {}", self.function, self.what),
            None => write!(f, "{}: {}", self.function, self.what),
        }
    }
}

/// Checks every function of `program`, reporting every fault rather than the
/// first: one lowering bug usually shows up in several places, and seeing
/// all of them is what says which one is the cause.
pub fn verify(program: &Program) -> Result<(), Vec<Invalid>> {
    let mut faults = Vec::new();
    for (index, function) in program.functions.iter().enumerate() {
        Check {
            program,
            function,
            id: FunctionId(index as u32),
            faults: &mut faults,
        }
        .run();
    }
    if faults.is_empty() {
        Ok(())
    } else {
        Err(faults)
    }
}

struct Check<'a> {
    program: &'a Program,
    function: &'a Function,
    id: FunctionId,
    faults: &'a mut Vec<Invalid>,
}

impl Check<'_> {
    fn run(&mut self) {
        self.check_frame();
        for pc in 0..self.function.code.len() {
            self.check_inst(pc);
        }
        self.check_falls_off_the_end();
    }

    fn fault(&mut self, pc: Option<usize>, what: impl Into<String>) {
        self.faults.push(Invalid {
            function: self.function.qualified(),
            pc,
            what: what.into(),
        });
    }

    /// The frame's own invariants: the parameters fit, the answer's layout
    /// exists, the spans line up, and the reference map is the one the reprs
    /// imply.
    fn check_frame(&mut self) {
        let size = self.function.frame_size();
        let mut at = 0;
        for (index, param) in self.function.params.clone().into_iter().enumerate() {
            if !self.layout_exists(None, param) {
                continue;
            }
            let width = self.program.layout(param).width();
            if !self.fits(None, at, param, &format!("parameter {index}")) {
                return;
            }
            at += width;
        }
        if !self.layout_exists(None, self.function.returns) {
            return;
        }
        if self.function.spans.len() != self.function.code.len() {
            self.fault(
                None,
                format!(
                    "has {} instructions but {} spans",
                    self.function.code.len(),
                    self.function.spans.len()
                ),
            );
        }
        let expected = RefMap::of(&self.function.reprs);
        if expected != self.function.refs {
            self.fault(
                None,
                "reference map disagrees with the frame's reprs, so a collection would \
                 scan the wrong slots"
                    .to_string(),
            );
        }
        for capture in self.function.captures.clone() {
            if !self.layout_exists(None, capture.layout) {
                continue;
            }
            let name = capture.name.clone();
            self.fits(
                None,
                capture.slot,
                capture.layout,
                &format!("capture `{name}`"),
            );
        }
        let _ = size;
    }

    /// A function whose last instruction can fall through has nowhere to go.
    fn check_falls_off_the_end(&mut self) {
        let last = self.function.code.len().checked_sub(1);
        let ends = matches!(
            last.map(|pc| &self.function.code[pc]),
            Some(Inst::Return { .. } | Inst::Jump { .. } | Inst::Switch { .. } | Inst::Trap { .. })
        );
        if !ends {
            self.fault(
                last,
                "the last instruction can fall through, and there is nothing after it",
            );
        }
    }

    fn check_inst(&mut self, pc: usize) {
        let inst = self.function.code[pc].clone();
        let at = Some(pc);
        match inst {
            Inst::Unit { dst } => self.expect(at, dst, &[Repr::Unit]),
            Inst::Bool { dst, .. } => self.expect(at, dst, &[Repr::Bool]),
            Inst::Int { dst, .. } => self.expect(at, dst, &[Repr::Int, Repr::Duration]),
            Inst::Float { dst, .. } => self.expect(at, dst, &[Repr::Float]),
            Inst::Str { dst, text } => {
                self.expect(at, dst, &[Repr::Ref]);
                self.in_range(at, text.index(), self.program.strings.len(), "string");
            }
            Inst::Copy { dst, src, layout } => {
                if self.layout_exists(at, layout) {
                    self.fits(at, dst, layout, "the destination of a copy");
                    self.fits(at, src, layout, "the source of a copy");
                }
            }
            Inst::Clear { slot, layout } => {
                if self.layout_exists(at, layout) {
                    self.fits(at, slot, layout, "what a clear zeroes");
                }
            }
            Inst::Neg { num, dst, a } => {
                let want = Self::numeric(num);
                self.expect(at, dst, want);
                self.expect(at, a, want);
            }
            Inst::Arith { num, dst, a, b, .. } => {
                let want = Self::numeric(num);
                self.expect(at, dst, want);
                self.expect(at, a, want);
                self.expect(at, b, want);
            }
            Inst::Cmp { on, dst, a, b, .. } => {
                self.expect(at, dst, &[Repr::Bool]);
                let want: &[Repr] = match on {
                    Compare::Int => &[Repr::Int, Repr::Duration],
                    Compare::Float => &[Repr::Float],
                    Compare::Bool => &[Repr::Bool],
                    Compare::Str => &[Repr::Ref],
                    // `is` compares words, and the only words whose identity
                    // is a language-level question are references.
                    Compare::Identity => &[Repr::Ref],
                };
                self.expect(at, a, want);
                self.expect(at, b, want);
            }
            Inst::Not { dst, a } => {
                self.expect(at, dst, &[Repr::Bool]);
                self.expect(at, a, &[Repr::Bool]);
            }
            Inst::Convert { to, dst, a } => {
                let (from, into) = match to {
                    crate::inst::Convert::IntToFloat => (Repr::Int, Repr::Float),
                    crate::inst::Convert::FloatToInt => (Repr::Float, Repr::Int),
                };
                self.expect(at, a, &[from]);
                self.expect(at, dst, &[into]);
            }
            Inst::Jump { to } => self.target(at, to),
            Inst::BranchFalse { cond, to } => {
                self.expect(at, cond, &[Repr::Bool]);
                self.target(at, to);
            }
            Inst::Switch { on, table } => {
                // The discriminant of an enum location is its first word and
                // is an `Int`; so is the layout id a `dyn` dispatch switches
                // on. Nothing else is dispatched on, and a slot's `Repr` is
                // the strongest thing a static check has to say about which
                // word this is — a location's extent is a fact about the
                // instruction that produced the word, not about the frame.
                self.expect(at, on, &[Repr::Int]);
                if self.in_range(at, table.index(), self.program.tables.len(), "table") {
                    let table = self.program.table(table).clone();
                    for to in table.targets.iter().chain(std::iter::once(&table.default)) {
                        self.target(at, *to);
                    }
                }
            }
            Inst::Return { src } => {
                let returns = self.function.returns;
                if self.layout_exists(at, returns) {
                    self.fits(at, src, returns, "what is returned");
                }
            }
            Inst::Call { dst, callee, args } => {
                if !self.in_range(at, callee.index(), self.program.functions.len(), "function") {
                    return;
                }
                let target = self.program.function(callee);
                let returns = target.returns;
                let params = target.params.clone();
                let name = target.qualified();
                if self.layout_exists(at, returns) {
                    self.fits(at, dst, returns, "the destination of a call");
                }
                self.check_args(at, args, &params, &name);
            }
            Inst::CallClosure { dst, closure, args } => {
                self.expect(at, closure, &[Repr::Ref]);
                self.repr(at, dst);
                self.each_arg(at, args);
            }
            Inst::CallHost { dst, op, args } => {
                if self.in_range(at, op.index(), self.program.host_ops.len(), "host op") {
                    let result = self.program.host_op(op).result;
                    if self.layout_exists(at, result) {
                        self.fits(at, dst, result, "the answer of a host call");
                    }
                }
                self.each_arg(at, args);
            }
            Inst::CallBuiltin { dst, builtin, args } => {
                if self.in_range(at, builtin.index(), self.program.builtins.len(), "builtin") {
                    let result = self.program.builtin(builtin).result;
                    if self.layout_exists(at, result) {
                        self.fits(at, dst, result, "the answer of a builtin");
                    }
                }
                self.each_arg(at, args);
            }
            Inst::Alloc { dst, layout, len } => {
                self.expect(at, dst, &[Repr::Ref]);
                self.layout_exists(at, layout);
                if let Len::Slot(slot) = len {
                    self.expect(at, slot, &[Repr::Int]);
                }
            }
            Inst::LoadField {
                dst, obj, layout, ..
            } => {
                self.expect(at, obj, &[Repr::Ref]);
                if self.layout_exists(at, layout) {
                    self.fits(at, dst, layout, "what a field is read into");
                }
            }
            Inst::StoreField {
                obj, src, layout, ..
            } => {
                self.expect(at, obj, &[Repr::Ref]);
                if self.layout_exists(at, layout) {
                    self.fits(at, src, layout, "what a field is written from");
                }
            }
            Inst::LoadElem {
                dst,
                obj,
                index,
                layout,
            } => {
                self.expect(at, obj, &[Repr::Ref]);
                self.expect(at, index, &[Repr::Int]);
                if self.layout_exists(at, layout) {
                    self.fits(at, dst, layout, "what an element is read into");
                }
            }
            Inst::StoreElem {
                obj,
                index,
                src,
                layout,
            } => {
                self.expect(at, obj, &[Repr::Ref]);
                self.expect(at, index, &[Repr::Int]);
                if self.layout_exists(at, layout) {
                    self.fits(at, src, layout, "what an element is written from");
                }
            }
            Inst::Len { dst, obj } => {
                self.expect(at, obj, &[Repr::Ref]);
                self.expect(at, dst, &[Repr::Int]);
            }
            Inst::LayoutOf { dst, obj } => {
                self.expect(at, obj, &[Repr::Ref]);
                self.expect(at, dst, &[Repr::Int]);
            }
            Inst::AddrOfSlot { dst, slot } => {
                self.expect(at, dst, &[Repr::Addr]);
                self.repr(at, slot);
            }
            Inst::AddrOfField { dst, obj, .. } => {
                self.expect(at, dst, &[Repr::Addr]);
                self.expect(at, obj, &[Repr::Ref]);
            }
            Inst::AddrOfElem {
                dst,
                obj,
                index,
                layout,
            } => {
                self.expect(at, dst, &[Repr::Addr]);
                self.expect(at, obj, &[Repr::Ref]);
                self.expect(at, index, &[Repr::Int]);
                self.layout_exists(at, layout);
            }
            Inst::Load { dst, addr, layout } => {
                self.expect(at, addr, &[Repr::Addr]);
                if self.layout_exists(at, layout) {
                    self.fits(at, dst, layout, "what a load answers");
                }
            }
            Inst::Store { addr, src, layout } => {
                self.expect(at, addr, &[Repr::Addr]);
                if self.layout_exists(at, layout) {
                    self.fits(at, src, layout, "what a store writes");
                }
            }
            Inst::Box { dst, src, layout } => {
                self.expect(at, dst, &[Repr::Ref]);
                if self.layout_exists(at, layout) {
                    self.fits(at, src, layout, "what is boxed");
                }
            }
            Inst::Unbox { dst, src, layout } => {
                self.expect(at, src, &[Repr::Ref]);
                if self.layout_exists(at, layout) {
                    self.fits(at, dst, layout, "what a box is opened into");
                }
            }
            Inst::Trap { message } => {
                self.in_range(at, message.index(), self.program.strings.len(), "string");
            }
        }
    }

    fn numeric(num: Num) -> &'static [Repr] {
        match num {
            // A `Duration` is nanoseconds, and nanoseconds add like
            // integers. Only the boundary cares what the answer is called.
            Num::Int => &[Repr::Int, Repr::Duration],
            Num::Float => &[Repr::Float],
        }
    }

    /// Whether `layout` names an entry of the program's layout table.
    fn layout_exists(&mut self, at: Option<usize>, layout: LayoutId) -> bool {
        self.in_range(at, layout.index(), self.program.layouts.len(), "layout")
    }

    /// Whether the location at `slot` is a value of `layout`: it is inside
    /// the frame, and its words are the layout's words in order.
    ///
    /// This is the check the whole representation turns on. A location is a
    /// base slot and a layout, and the frame's per-slot reprs are what a
    /// collection reads — so a location whose words disagree with what is
    /// being moved into it is a reference the collector will miss or a
    /// scalar it will follow.
    fn fits(&mut self, at: Option<usize>, slot: Slot, layout: LayoutId, what: &str) -> bool {
        let words = self.program.layout(layout).words.clone();
        let name = self.program.layout(layout).name.clone();
        let size = self.function.frame_size();
        if slot as u64 + words.len() as u64 > size as u64 {
            self.fault(
                at,
                format!(
                    "{what} is `{name}`, {} words at slot {slot}, and the frame has {size}",
                    words.len()
                ),
            );
            return false;
        }
        for (offset, want) in words.iter().enumerate() {
            let found = self.function.reprs[slot as usize + offset];
            if found != *want {
                self.fault(
                    at,
                    format!(
                        "{what} is `{name}`, whose word {offset} is {want}, but slot {} holds \
                         {found}",
                        slot as usize + offset
                    ),
                );
                return false;
            }
        }
        true
    }

    /// What slot `slot` holds, reporting a slot outside the frame.
    fn repr(&mut self, at: Option<usize>, slot: Slot) -> Option<Repr> {
        match self.function.repr(slot) {
            Some(repr) => Some(repr),
            None => {
                let size = self.function.frame_size();
                self.fault(at, format!("names slot {slot}, outside a frame of {size}"));
                None
            }
        }
    }

    fn expect(&mut self, at: Option<usize>, slot: Slot, want: &[Repr]) {
        let Some(found) = self.repr(at, slot) else {
            return;
        };
        if !want.contains(&found) {
            let names: Vec<&str> = want.iter().map(|repr| repr.name()).collect();
            self.fault(
                at,
                format!(
                    "slot {slot} holds {found}, but this wants {}",
                    names.join(" or ")
                ),
            );
        }
    }

    fn target(&mut self, at: Option<usize>, to: u32) {
        if to as usize >= self.function.code.len() {
            let len = self.function.code.len();
            self.fault(at, format!("jumps to {to}, past the {len} instructions"));
        }
    }

    fn in_range(&mut self, at: Option<usize>, index: usize, len: usize, what: &str) -> bool {
        if index >= len {
            self.fault(at, format!("names {what} {index}, and there are {len}"));
            false
        } else {
            true
        }
    }

    /// Every argument location is in the frame, whatever it holds.
    fn each_arg(&mut self, at: Option<usize>, args: crate::ArgsId) {
        if !self.in_range(at, args.index(), self.program.args.len(), "argument list") {
            return;
        }
        for slot in self.program.arg_list(args).to_vec() {
            self.repr(at, slot);
        }
    }

    /// Every argument is a location of the layout the callee's parameter
    /// declares, word for word.
    fn check_args(
        &mut self,
        at: Option<usize>,
        args: crate::ArgsId,
        want: &[LayoutId],
        name: &str,
    ) {
        if !self.in_range(at, args.index(), self.program.args.len(), "argument list") {
            return;
        }
        let passed = self.program.arg_list(args).to_vec();
        if passed.len() != want.len() {
            self.fault(
                at,
                format!(
                    "passes {} arguments to `{name}`, which declares {}",
                    passed.len(),
                    want.len()
                ),
            );
            return;
        }
        for (index, (slot, layout)) in passed.into_iter().zip(want).enumerate() {
            if self.layout_exists(at, *layout) {
                self.fits(at, slot, *layout, &format!("argument {index} of `{name}`"));
            }
        }
    }
}

/// The id of the function being checked is carried so that a future fault
/// can name it by id as well as by name; nothing reads it yet.
impl Check<'_> {
    #[allow(dead_code)]
    fn id(&self) -> FunctionId {
        self.id
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use cove_diag::{FileId, Span};

    use super::*;
    use crate::inst::Inst;
    use crate::layout::{Layout, Shape};
    use crate::program::{Function, Table, TableId};

    const INT: LayoutId = LayoutId(0);
    const STR: LayoutId = LayoutId(1);
    const POINT: LayoutId = LayoutId(2);
    /// `[disc: Int, Ref]`, the shape an `Option<String>` has.
    const ANSWER: LayoutId = LayoutId(3);

    fn layouts() -> Vec<Layout> {
        vec![
            Layout::word("Int", Repr::Int),
            Layout::object("String", Shape::Str),
            Layout::inline(
                "Point",
                Shape::Struct {
                    fields: Vec::new(),
                    opaque: false,
                },
                vec![Repr::Int, Repr::Int],
            ),
            Layout::inline(
                "Option",
                Shape::Enum {
                    cases: Vec::new(),
                    payload: vec![Repr::Ref],
                },
                vec![Repr::Int, Repr::Ref],
            ),
        ]
    }

    fn span() -> Span {
        Span::new(FileId(0), 0, 0)
    }

    fn function(reprs: Vec<Repr>, returns: LayoutId, code: Vec<Inst>) -> Function {
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
            span: span(),
            is_async: false,
        }
    }

    fn program(functions: Vec<Function>) -> Program {
        Program {
            functions,
            layouts: layouts(),
            str_layout: STR,
            ..Program::default()
        }
    }

    fn faults(program: &Program) -> Vec<String> {
        match verify(program) {
            Ok(()) => Vec::new(),
            Err(items) => items.into_iter().map(|item| item.what).collect(),
        }
    }

    #[test]
    fn a_well_formed_function_has_nothing_to_say_about_it() {
        let f = function(
            vec![Repr::Int, Repr::Int],
            POINT,
            vec![Inst::Return { src: 0 }],
        );
        assert_eq!(faults(&program(vec![f])), Vec::<String>::new());
    }

    #[test]
    fn a_copy_whose_destination_is_not_the_layout_s_words_is_a_fault() {
        // The whole representation turns on this: a location is a base slot
        // and a layout, and a copy of the wrong width is a reference the
        // collector will miss or a scalar it will follow.
        let f = function(
            vec![Repr::Int, Repr::Ref, Repr::Int, Repr::Int, Repr::Unit],
            INT,
            vec![
                Inst::Copy {
                    dst: 0,
                    src: 2,
                    layout: POINT,
                },
                Inst::Return { src: 4 },
            ],
        );
        assert_eq!(
            faults(&program(vec![f])),
            vec![
                "the destination of a copy is `Point`, whose word 1 is int, but slot 1 holds ref"
                    .to_string(),
                "what is returned is `Int`, whose word 0 is int, but slot 4 holds unit".to_string(),
            ]
        );
    }

    #[test]
    fn a_location_that_runs_off_the_end_of_the_frame_is_a_fault() {
        let f = function(
            vec![Repr::Int, Repr::Int],
            INT,
            vec![
                Inst::Copy {
                    dst: 1,
                    src: 0,
                    layout: POINT,
                },
                Inst::Return { src: 0 },
            ],
        );
        assert_eq!(
            faults(&program(vec![f])),
            vec!["the destination of a copy is `Point`, 2 words at slot 1, and the frame has 2"]
        );
    }

    #[test]
    fn a_reference_map_that_disagrees_with_the_reprs_is_a_fault() {
        // A collection walks frames using the map, so a lowering that wrote
        // a reference into a slot the map calls an `Int` would produce a
        // dangling reference at the next collection.
        let mut f = function(vec![Repr::Ref], STR, vec![Inst::Return { src: 0 }]);
        f.refs = RefMap::of(&[Repr::Int]);
        assert_eq!(
            faults(&program(vec![f])),
            vec![
                "reference map disagrees with the frame's reprs, so a collection would scan the \
                 wrong slots"
            ]
        );
    }

    #[test]
    fn a_call_whose_arguments_are_not_the_callee_s_parameters_is_a_fault() {
        let mut callee = function(
            vec![Repr::Int, Repr::Int, Repr::Int],
            INT,
            vec![Inst::Return { src: 2 }],
        );
        callee.params = vec![POINT];
        callee.name = Arc::from("g");
        let caller = function(
            vec![Repr::Int, Repr::Ref, Repr::Int],
            INT,
            vec![
                Inst::Call {
                    dst: 0,
                    callee: FunctionId(0),
                    args: crate::ArgsId(0),
                },
                Inst::Return { src: 0 },
            ],
        );
        let mut held = program(vec![callee, caller]);
        held.args = vec![vec![1]];
        assert_eq!(
            faults(&held),
            vec![
                "argument 0 of `m.g` is `Point`, whose word 0 is int, but slot 1 holds ref"
                    .to_string()
            ]
        );
    }

    #[test]
    fn a_call_that_passes_the_wrong_number_of_arguments_is_a_fault() {
        let mut callee = function(
            vec![Repr::Int, Repr::Int],
            INT,
            vec![Inst::Return { src: 1 }],
        );
        callee.params = vec![INT];
        callee.name = Arc::from("g");
        let caller = function(
            vec![Repr::Int],
            INT,
            vec![
                Inst::Call {
                    dst: 0,
                    callee: FunctionId(0),
                    args: crate::ArgsId(0),
                },
                Inst::Return { src: 0 },
            ],
        );
        let mut held = program(vec![callee, caller]);
        held.args = vec![Vec::new()];
        assert_eq!(
            faults(&held),
            vec!["passes 0 arguments to `m.g`, which declares 1"]
        );
    }

    #[test]
    fn a_switch_on_something_that_is_not_a_discriminant_word_is_a_fault() {
        // The discriminant of an enum location is its first word and is an
        // `Int`; so is the layout id a `dyn` dispatch switches on. A slot's
        // `Repr` is the strongest thing a static check has to say about
        // which word this is.
        let f = function(
            vec![Repr::Int, Repr::Ref],
            ANSWER,
            vec![
                Inst::Switch {
                    on: 1,
                    table: TableId(0),
                },
                Inst::Return { src: 0 },
            ],
        );
        let mut held = program(vec![f]);
        held.tables = vec![Table {
            targets: vec![1],
            default: 1,
        }];
        assert_eq!(faults(&held), vec!["slot 1 holds ref, but this wants int"]);
    }

    #[test]
    fn a_jump_that_lands_past_the_last_instruction_is_a_fault() {
        let f = function(
            vec![Repr::Int],
            INT,
            vec![Inst::Jump { to: 9 }, Inst::Return { src: 0 }],
        );
        assert_eq!(
            faults(&program(vec![f])),
            vec!["jumps to 9, past the 2 instructions"]
        );
    }

    #[test]
    fn an_id_outside_its_table_is_a_fault() {
        let f = function(
            vec![Repr::Ref],
            STR,
            vec![
                Inst::Str {
                    dst: 0,
                    text: crate::StrId(3),
                },
                Inst::Return { src: 0 },
            ],
        );
        assert_eq!(
            faults(&program(vec![f])),
            vec!["names string 3, and there are 0"]
        );
    }

    #[test]
    fn a_body_whose_last_instruction_falls_through_is_a_fault() {
        let f = function(vec![Repr::Int], INT, vec![Inst::Int { dst: 0, value: 1 }]);
        assert_eq!(
            faults(&program(vec![f])),
            vec!["the last instruction can fall through, and there is nothing after it"]
        );
    }

    #[test]
    fn a_clear_agrees_with_the_layout_it_zeroes() {
        let f = function(
            vec![Repr::Int, Repr::Ref, Repr::Unit],
            INT,
            vec![
                Inst::Clear {
                    slot: 0,
                    layout: ANSWER,
                },
                Inst::Clear {
                    slot: 1,
                    layout: ANSWER,
                },
                Inst::Return { src: 0 },
            ],
        );
        // The first is right — `[Int, Ref]` is what an `Option` is — and the
        // second names the same layout one word along, where it is not.
        assert_eq!(
            faults(&program(vec![f])),
            vec!["what a clear zeroes is `Option`, whose word 0 is int, but slot 1 holds ref"]
        );
    }
}
