//! What an address is used as, and that the place it names holds that.
//!
//! A [`Repr::Addr`] word is one word with no layout: the address of the first
//! word of a value location, in a frame or in an object. [`Inst::Load`] and
//! [`Inst::Store`] move whatever words their own layout names through it, and
//! [`Inst::AddrOfPart`] adds any offset to it. So until this pass a verified
//! program could form the address of an `Int` slot and store a reference
//! through it — a reference nobody allocated, in a slot the collector traces —
//! or add a million to an address and write past every frame. Nothing about
//! one instruction says otherwise; it is a fact about where the address came
//! from and everything it is used for.
//!
//! This is that fact, worked out once per program ([ADR 0120]):
//!
//! - **An address goes down the stack and nowhere else.** It may be copied
//!   within its frame and passed to a direct call, whose frame is above the one
//!   it names. It may not be answered, captured, boxed, stored through another
//!   address, handed to a closure, a host or a task, or allocated into an
//!   object — every one of those keeps the place past the frame it is in. (A
//!   heap payload holding one is refused by [`super::layouts`].)
//! - **Every place formed has a *need***: for every word offset from it, the
//!   `Repr` read or written there and whether it is written. The formation is
//!   an instruction that writes an address slot — the address of a slot, of a
//!   field, of a part of another place — or a parameter, which the caller
//!   formed. Which formations reach which uses is the control flow's question
//!   (the lowering reuses an address slot for one place after another), so it
//!   is answered by reaching definitions over the function's instructions. A
//!   load or a store needs its layout's words of every formation that reaches
//!   it; a part's formation passes its own need, shifted by the part's offset,
//!   to the formations of the address it was taken from; a copy passes its
//!   need back to what it copied; and an argument passes the callee's
//!   parameter's need — this same question about the callee, so the needs of
//!   every function's address parameters are found together, to a fixed
//!   point. Two uses that need two `Repr`s at one offset are a fault.
//! - **Every place formed meets its need.** The address of a frame slot is
//!   checked here: every needed word is inside the frame and holds the needed
//!   `Repr`. The address of a field of an object is the machine's to check,
//!   because which object it is is a run-time fact; this pass hands it the need
//!   ([`field_needs`]), and the machine holds the object's header to it where
//!   the address is formed. A parameter's need is its callers'.
//!
//! An address slot no formation reaches holds null — a frame is zeroed on
//! entry, and a clear writes null — and the machine refuses a null address
//! wherever one is followed.
//!
//! # An address loaded from a frame is checked where it is used
//!
//! One shape the lowering writes is outside all of that: the path of vectors a
//! tracked walk keeps (`lower::synth`), a list linked through the frames of the
//! walks it is inside, each entry the address of three words of a deeper frame
//! whose third is the address of the entry before it. The walk loads the next
//! address out of the entry it holds and reads through it, as far as a depth
//! it was handed says to. What those reads need is the list's, not any one
//! formation's, and which formations they reach is a run-time fact — an entry
//! of depth nought is never read at all, and the wrapper that starts a walk
//! hands over the address of a frame whose words are not an entry's.
//!
//! So an address that *may* have been loaded out of memory — or derived from
//! one, by a part or a copy — is **loaded**, and a load or a store through one
//! is a [`dynamic_uses`] instruction: its needs are not passed to anything,
//! and the machine checks it where it runs, against the frame the address
//! lands in. Moving an address *into* a frame through another one is still
//! refused, which is what keeps every address a frame holds pointing at its
//! own frame or a deeper one: so an address loaded out of a frame names one
//! that is live for as long as the frame loading it is. A loaded address may
//! not be passed to a call, whose parameter's need could not be held to it.
//!
//! [ADR 0120]: ../../../../docs/adr/0120-the-vm-runs-only-what-verifies.md

use std::collections::{BTreeMap, BTreeSet};

use super::Invalid;
use crate::inst::{Inst, Slot};
use crate::layout::LayoutId;
use crate::program::{Function, FunctionId, Program};
use crate::repr::Repr;

/// What a place is used as: per word offset from it, the `Repr` read or
/// written there and whether it is written.
pub type Need = BTreeMap<u32, (Repr, bool)>;

/// The most rounds the needs of address parameters are propagated through the
/// call graph before the pass gives up on bounding them. A lowering's needs
/// settle in as many rounds as its deepest chain of calls passing an address
/// down; a part taken in a recursive call grows a need by one offset a round
/// and never settles.
const ROUNDS: usize = 256;

/// The furthest word from its place an address may be used at. Nothing the
/// lowering forms reaches past the widest value location, which a frame bounds.
const REACH: u32 = crate::MAX_FRAME_WORDS as u32;

/// Every fault in how `program` forms and uses addresses.
pub(super) fn check(program: &Program) -> Vec<Invalid> {
    let mut faults = Vec::new();
    let Some(exported) = settle(program) else {
        faults.push(Invalid {
            function: "<program>".to_string(),
            pc: None,
            what: "the uses of an address parameter do not settle: an address is offset further \
                   on every call, and its reach cannot be bounded"
                .to_string(),
        });
        return faults;
    };
    for (index, function) in program.functions.iter().enumerate() {
        let mut local = Vec::new();
        let needs = Needs::of(program, FunctionId(index as u32), &exported, &mut local);
        let mut fault = |pc: Option<usize>, what: String| {
            faults.push(Invalid {
                function: function.qualified(),
                pc,
                what,
            })
        };
        for (pc, what) in local {
            fault(pc, what);
        }
        let holds_a_place = |layout: LayoutId| {
            program
                .layouts
                .get(layout.index())
                .is_some_and(|held| held.words.contains(&Repr::Addr))
        };
        if holds_a_place(function.returns) {
            fault(
                None,
                "answers an address, which would outlive the frame it names".to_string(),
            );
        }
        for capture in &function.captures {
            if holds_a_place(capture.layout) {
                fault(
                    None,
                    format!(
                        "captures `{}` as an address, and a closure's environment is a heap object",
                        capture.name
                    ),
                );
            }
        }
        let row_holds_a_place = |args: crate::ArgsId| {
            program
                .args
                .get(args.index())
                .is_some_and(|row| row.iter().any(|arg| holds_a_place(arg.layout)))
        };
        let callees = closure_callees(program, function);
        for (pc, inst) in function.code.iter().enumerate() {
            let at = Some(pc);
            let escapes = |what: &str| format!("{what} an address, which would outlive its frame");
            match inst {
                Inst::Store { layout, .. } if holds_a_place(*layout) => {
                    fault(at, escapes("stores through an address"));
                }
                Inst::Box { layout, .. } | Inst::Unbox { layout, .. } if holds_a_place(*layout) => {
                    fault(at, escapes("boxes"));
                }
                Inst::Alloc { layout, .. } if holds_a_place(*layout) => {
                    fault(at, escapes("allocates an object holding"));
                }
                Inst::GrowableAlloc {
                    storage: crate::inst::Storage::Words(layout),
                    ..
                } if holds_a_place(*layout) => {
                    fault(at, escapes("allocates a run of"));
                }
                Inst::Spawn { answer, .. }
                | Inst::Await { answer, .. }
                | Inst::Settled { answer, .. }
                    if holds_a_place(*answer) =>
                {
                    fault(at, escapes("hands a task"));
                }
                Inst::ScopeLeave { layout, .. } if holds_a_place(*layout) => {
                    fault(at, escapes("hands a scope"));
                }
                Inst::CallClosure { result, .. } if holds_a_place(*result) => {
                    fault(at, escapes("answers, from a closure,"));
                }
                Inst::CallClosure { args, .. }
                    if row_holds_a_place(*args) && !matches!(callees.get(&pc), Some(Some(_))) =>
                {
                    fault(
                        at,
                        escapes("hands a closure, whose callee is not known here,"),
                    );
                }
                Inst::CallHost { args, op, .. } | Inst::CallResource { args, op, .. } => {
                    let answers = program
                        .host_ops
                        .get(op.index())
                        .is_some_and(|op| holds_a_place(op.result));
                    if row_holds_a_place(*args) || answers {
                        fault(at, escapes("hands a host"));
                    }
                }
                // The one place formed in a frame: every word its need names
                // is inside the frame and holds what is needed there.
                Inst::AddrOfSlot { dst, slot } => {
                    let Some(need) = needs.formed.get(&(pc, *dst)) else {
                        continue;
                    };
                    for (offset, (repr, _)) in need {
                        let word = u64::from(*slot) + u64::from(*offset);
                        match function.reprs.get(word as usize) {
                            Some(held) if held == repr => {}
                            Some(held) => fault(
                                at,
                                format!(
                                    "forms the address of slot {slot}, and word {offset} from it \
                                     is used as {repr} where the frame holds {held}"
                                ),
                            ),
                            None => fault(
                                at,
                                format!(
                                    "forms the address of slot {slot}, and word {offset} from it \
                                     is used, past the frame's {} words",
                                    function.reprs.len()
                                ),
                            ),
                        }
                    }
                }
                _ => {}
            }
        }
    }
    faults
}

/// For every [`Inst::AddrOfField`] of `program`, by function and pc, what the
/// place it forms is used as: the need the machine holds the object's header
/// to where the address is formed. Empty where the address is never read or
/// written through.
///
/// Answers for a program [`super::verify`] accepted; for any other it answers
/// what it can, and the machine never asks.
pub fn field_needs(program: &Program) -> Vec<Vec<Option<Need>>> {
    let exported =
        settle(program).unwrap_or_else(|| vec![BTreeMap::new(); program.functions.len()]);
    program
        .functions
        .iter()
        .enumerate()
        .map(|(index, function)| {
            let needs = Needs::of(
                program,
                FunctionId(index as u32),
                &exported,
                &mut Vec::new(),
            );
            function
                .code
                .iter()
                .enumerate()
                .map(|(pc, inst)| match inst {
                    Inst::AddrOfField { dst, .. } => {
                        Some(needs.formed.get(&(pc, *dst)).cloned().unwrap_or_default())
                    }
                    _ => None,
                })
                .collect()
        })
        .collect()
}

/// For every function of `program`, by pc, whether the instruction is a load
/// or a store through an address that may have been loaded out of memory:
/// the ones the machine checks where they run, against the frame the address
/// lands in. See the module.
///
/// Answers for a program [`super::verify`] accepted.
pub fn dynamic_uses(program: &Program) -> Vec<Vec<bool>> {
    let exported =
        settle(program).unwrap_or_else(|| vec![BTreeMap::new(); program.functions.len()]);
    program
        .functions
        .iter()
        .enumerate()
        .map(|(index, function)| {
            let needs = Needs::of(
                program,
                FunctionId(index as u32),
                &exported,
                &mut Vec::new(),
            );
            (0..function.code.len())
                .map(|pc| needs.dynamic.contains(&pc))
                .collect()
        })
        .collect()
}

/// What every function's address parameters are used as, by parameter slot:
/// the needs propagated through the call graph to a fixed point, or `None`
/// if they do not reach one within [`ROUNDS`].
fn settle(program: &Program) -> Option<Vec<BTreeMap<Slot, Need>>> {
    let mut exported: Vec<BTreeMap<Slot, Need>> = vec![BTreeMap::new(); program.functions.len()];
    for _ in 0..ROUNDS {
        let mut changed = false;
        for index in 0..program.functions.len() {
            let needs = Needs::of(
                program,
                FunctionId(index as u32),
                &exported,
                &mut Vec::new(),
            );
            if needs.params != exported[index] {
                exported[index] = needs.params;
                changed = true;
            }
        }
        if !changed {
            return Some(exported);
        }
    }
    None
}

/// Where an address slot's value came from at some instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Formed {
    /// The caller formed it: the parameter in this slot.
    Param(Slot),
    /// The instruction at this pc wrote it into this slot.
    At(usize, Slot),
    /// Nothing did, or a clear did: the slot holds null.
    Null,
    /// The instruction at this pc loaded it out of memory, or made it from
    /// an address that may have been: see the module's last section.
    Loaded(usize, Slot),
}

/// The needs of one function's places.
struct Needs {
    /// Of each place an instruction formed, by its pc and the slot it went into.
    formed: BTreeMap<(usize, Slot), Need>,
    /// Of each address parameter, by its slot.
    params: BTreeMap<Slot, Need>,
    /// The loads and stores through an address that may be loaded, by pc.
    dynamic: BTreeSet<usize>,
}

impl Needs {
    /// What every place function `id` forms or is handed is used as, given
    /// what its callees' address parameters are used as. A conflict is
    /// recorded in `faults` and the first need kept.
    fn of(
        program: &Program,
        id: FunctionId,
        exported: &[BTreeMap<Slot, Need>],
        faults: &mut Vec<(Option<usize>, String)>,
    ) -> Needs {
        let function = &program.functions[id.index()];
        let mut needs = Needs {
            formed: BTreeMap::new(),
            params: BTreeMap::new(),
            dynamic: BTreeSet::new(),
        };
        // Most functions hold no address at all, and have nothing to learn.
        let places: Vec<Slot> = (0..function.reprs.len() as Slot)
            .filter(|slot| function.reprs[*slot as usize] == Repr::Addr)
            .collect();
        if places.is_empty() || function.code.is_empty() {
            return needs;
        }
        let reaching = reaching(program, function, &places);
        let callees = closure_callees(program, function);
        let index_of = |slot: Slot| places.binary_search(&slot).ok();
        let words = |layout: LayoutId| {
            program
                .layouts
                .get(layout.index())
                .map_or(&[][..], |held| held.words.as_slice())
        };
        let mut need: BTreeMap<Formed, Need> = BTreeMap::new();
        // Faults found outside `flow`, which holds `faults` while it lives.
        let mut refused: BTreeSet<(usize, String)> = BTreeSet::new();
        // To a fixed point: a part's need, a copy's and an argument's flow to
        // what reached them, and a need may be learned after it was passed on.
        loop {
            let mut changed = false;
            for (pc, inst) in function.code.iter().enumerate() {
                let before = &reaching[pc];
                // Every formation of `slot` that reaches `pc` — or none, for
                // a slot that may hold a loaded address, whose uses are the
                // machine's to check and pass no need on.
                let loaded = |slot: Slot| {
                    index_of(slot).is_some_and(|at| {
                        before[at]
                            .iter()
                            .any(|formed| matches!(formed, Formed::Loaded(..)))
                    })
                };
                let sources = |slot: Slot| -> Vec<Formed> {
                    if loaded(slot) {
                        return Vec::new();
                    }
                    index_of(slot)
                        .map(|at| before[at].iter().copied().collect())
                        .unwrap_or_default()
                };
                let mut flow = |need: &mut BTreeMap<Formed, Need>,
                                into: &[Formed],
                                shift: u64,
                                from: &[(u32, (Repr, bool))]|
                 -> bool {
                    let mut grew = false;
                    for formed in into {
                        if *formed == Formed::Null {
                            continue;
                        }
                        for (offset, (repr, written)) in from {
                            let word = shift + u64::from(*offset);
                            if word > u64::from(REACH) {
                                faults.push((
                                    Some(pc),
                                    format!(
                                        "uses a place {word} words from where it begins, past the \
                                         widest value location"
                                    ),
                                ));
                                continue;
                            }
                            let entry = need.entry(*formed).or_default();
                            match entry.get_mut(&(word as u32)) {
                                None => {
                                    entry.insert(word as u32, (*repr, *written));
                                    grew = true;
                                }
                                Some((held, was)) if held == repr => {
                                    grew |= *written && !*was;
                                    *was |= *written;
                                }
                                Some((held, _)) => faults.push((
                                    Some(pc),
                                    format!(
                                        "uses a place as {repr} at word {word}, where another \
                                         use reads it as {held}"
                                    ),
                                )),
                            }
                        }
                    }
                    grew
                };
                let of =
                    |need: &BTreeMap<Formed, Need>, formed: Formed| -> Vec<(u32, (Repr, bool))> {
                        need.get(&formed)
                            .map(|held| held.iter().map(|(k, v)| (*k, *v)).collect())
                            .unwrap_or_default()
                    };
                match inst {
                    Inst::Load { addr, .. } | Inst::Store { addr, .. } if loaded(*addr) => {
                        needs.dynamic.insert(pc);
                    }
                    Inst::Load { addr, layout, .. } | Inst::Store { addr, layout, .. } => {
                        let written = matches!(inst, Inst::Store { .. });
                        let used: Vec<(u32, (Repr, bool))> = words(*layout)
                            .iter()
                            .enumerate()
                            .map(|(k, repr)| (k as u32, (*repr, written)))
                            .collect();
                        changed |= flow(&mut need, &sources(*addr), 0, &used);
                    }
                    Inst::AddrOfPart { dst, addr, at } => {
                        let part = of(&need, Formed::At(pc, *dst));
                        changed |= flow(&mut need, &sources(*addr), u64::from(*at), &part);
                    }
                    Inst::Copy { dst, src, layout } => {
                        for (k, repr) in words(*layout).iter().enumerate() {
                            if *repr == Repr::Addr {
                                let copy = of(&need, Formed::At(pc, *dst + k as u32));
                                changed |= flow(&mut need, &sources(*src + k as u32), 0, &copy);
                            }
                        }
                    }
                    Inst::Call { .. } | Inst::CallClosure { .. } => {
                        let (args, targets) = match inst {
                            Inst::Call { callee, args, .. } => (*args, vec![*callee]),
                            Inst::CallClosure { args, .. } => match callees.get(&pc) {
                                Some(Some(known)) => (*args, known.clone()),
                                // Refused by `check` if it is handed an address.
                                _ => continue,
                            },
                            _ => unreachable!(),
                        };
                        let Some(row) = program.args.get(args.index()) else {
                            continue;
                        };
                        for callee in targets {
                            let Some(wanted) = exported.get(callee.index()) else {
                                continue;
                            };
                            let mut param = 0u32;
                            for arg in row {
                                for (k, repr) in words(arg.layout).iter().enumerate() {
                                    if *repr != Repr::Addr {
                                        continue;
                                    }
                                    if loaded(arg.slot + k as u32) {
                                        refused.insert((
                                            pc,
                                            format!(
                                            "passes slot {} to a call, which may hold an address \
                                             loaded out of memory, and a parameter's need cannot \
                                             be held to one",
                                            arg.slot + k as u32
                                        ),
                                        ));
                                        continue;
                                    }
                                    if let Some(wants) = wanted.get(&(param + k as u32)) {
                                        let wants: Vec<_> =
                                            wants.iter().map(|(k, v)| (*k, *v)).collect();
                                        changed |= flow(
                                            &mut need,
                                            &sources(arg.slot + k as u32),
                                            0,
                                            &wants,
                                        );
                                    }
                                }
                                param += words(arg.layout).len() as u32;
                            }
                        }
                    }
                    _ => {}
                }
            }
            if !changed {
                break;
            }
        }
        faults.extend(refused.into_iter().map(|(pc, what)| (Some(pc), what)));
        // The fixed point visits an instruction more than once; report a
        // fault once.
        faults.sort();
        faults.dedup();
        for (formed, held) in need {
            match formed {
                Formed::Param(slot) => {
                    needs.params.insert(slot, held);
                }
                Formed::At(pc, slot) => {
                    needs.formed.insert((pc, slot), held);
                }
                Formed::Null | Formed::Loaded(..) => {}
            }
        }
        needs
    }
}

/// For every [`Inst::CallClosure`] of `function` that hands its callee an
/// address, by pc: the functions the closure it calls may be, if every
/// definition of the closure slot that reaches the call is the allocation of a
/// closure ([`crate::Shape::Closure`] names its function), and `None` if any
/// is not — a parameter, a load, a call's answer, a copy.
///
/// It is a closure call's one way to take a `var` argument: `lock` hands its
/// closure the address of the cell's value, and the lowering makes the closure
/// in the frame that calls it. What the callee uses the place as is then known
/// here, and is passed to the place as a direct call's would be. Which object
/// the slot holds when the call runs is still the machine's to hold to the
/// answer ([`closure_callee_table`]): a store through an address can reach the
/// slot without writing it here.
pub fn closure_callees(
    program: &Program,
    function: &Function,
) -> BTreeMap<usize, Option<Vec<FunctionId>>> {
    let holds_a_place = |layout: LayoutId| {
        program
            .layouts
            .get(layout.index())
            .is_some_and(|held| held.words.contains(&Repr::Addr))
    };
    let mut answers = BTreeMap::new();
    for (pc, inst) in function.code.iter().enumerate() {
        let Inst::CallClosure { closure, args, .. } = inst else {
            continue;
        };
        let addressed = program
            .args
            .get(args.index())
            .is_some_and(|row| row.iter().any(|arg| holds_a_place(arg.layout)));
        if addressed {
            answers.insert(pc, callees_reaching(program, function, pc, *closure));
        }
    }
    answers
}

/// What a definition of a closure slot made it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Made {
    /// A closure of this function.
    Closure(FunctionId),
    /// Anything else, which this pass cannot see into.
    Unknown,
}

/// The functions the closure in `slot` may call at `target`: reaching
/// definitions over `function`'s control flow, for the one slot.
fn callees_reaching(
    program: &Program,
    function: &Function,
    target: usize,
    slot: Slot,
) -> Option<Vec<FunctionId>> {
    let len = function.code.len();
    let params = function.param_words(&program.layouts);
    // A frame is zeroed on entry, so a slot that is no parameter holds null,
    // which the machine refuses to call: it contributes no callee.
    let entry: BTreeSet<Made> = if slot < params {
        BTreeSet::from([Made::Unknown])
    } else {
        BTreeSet::new()
    };
    let mut before: Vec<Option<BTreeSet<Made>>> = vec![None; len];
    if len == 0 {
        return None;
    }
    before[0] = Some(entry);
    let mut work = vec![0usize];
    while let Some(pc) = work.pop() {
        let Some(state) = before[pc].clone() else {
            continue;
        };
        let inst = &function.code[pc];
        let mut written = false;
        inst.writes(program, &mut |base, width| {
            written |= base <= slot && u64::from(slot) < u64::from(base) + u64::from(width);
        });
        let after = if !written {
            state
        } else {
            match inst {
                Inst::Alloc { dst, layout, .. } if *dst == slot => {
                    match program.layouts.get(layout.index()).map(|held| &held.shape) {
                        Some(crate::layout::Shape::Closure { function, .. }) => {
                            BTreeSet::from([Made::Closure(*function)])
                        }
                        _ => BTreeSet::from([Made::Unknown]),
                    }
                }
                _ => BTreeSet::from([Made::Unknown]),
            }
        };
        for next in successors(program, function, pc) {
            if next >= len {
                continue;
            }
            match &mut before[next] {
                None => {
                    before[next] = Some(after.clone());
                    work.push(next);
                }
                Some(held) => {
                    let size = held.len();
                    held.extend(after.iter().copied());
                    if held.len() != size {
                        work.push(next);
                    }
                }
            }
        }
    }
    let reaching = before[target].clone().unwrap_or_default();
    let mut callees = Vec::new();
    for made in reaching {
        match made {
            Made::Closure(callee) if callee.index() < program.functions.len() => {
                callees.push(callee);
            }
            _ => return None,
        }
    }
    (!callees.is_empty()).then_some(callees)
}

/// For every function of `program`, the closure calls that hand an address
/// and the functions each was verified as calling, by pc ascending: what the
/// machine holds the callee it reads out of the object to.
///
/// Answers for a program [`super::verify`] accepted, in which every such call
/// has its callees.
pub fn closure_callee_table(program: &Program) -> Vec<Vec<(usize, Vec<FunctionId>)>> {
    program
        .functions
        .iter()
        .map(|function| {
            closure_callees(program, function)
                .into_iter()
                .map(|(pc, callees)| (pc, callees.unwrap_or_default()))
                .collect()
        })
        .collect()
}

/// Which formations of each address slot in `places` reach each instruction,
/// before it runs: reaching definitions over the function's control flow.
fn reaching(program: &Program, function: &Function, places: &[Slot]) -> Vec<Vec<BTreeSet<Formed>>> {
    let len = function.code.len();
    let params = function.param_words(&program.layouts);
    let index_of = |slot: Slot| places.binary_search(&slot).ok();
    let entry: Vec<BTreeSet<Formed>> = places
        .iter()
        .map(|slot| {
            BTreeSet::from([if *slot < params {
                Formed::Param(*slot)
            } else {
                Formed::Null
            }])
        })
        .collect();
    let mut before: Vec<Option<Vec<BTreeSet<Formed>>>> = vec![None; len];
    before[0] = Some(entry);
    let mut work = vec![0usize];
    while let Some(pc) = work.pop() {
        let Some(state) = before[pc].clone() else {
            continue;
        };
        let mut after = state.clone();
        let loaded = |slot: Slot| {
            index_of(slot).is_some_and(|at| {
                state[at]
                    .iter()
                    .any(|formed| matches!(formed, Formed::Loaded(..)))
            })
        };
        for (slot, formed, from) in writes(program, &function.code[pc], pc) {
            if let Some(at) = index_of(slot) {
                // A part or a copy of a loaded address is loaded too.
                let formed = match (formed, from) {
                    (Formed::At(pc, slot), Some(from)) if loaded(from) => Formed::Loaded(pc, slot),
                    (formed, _) => formed,
                };
                after[at] = BTreeSet::from([formed]);
            }
        }
        for next in successors(program, function, pc) {
            if next >= len {
                continue;
            }
            match &mut before[next] {
                None => {
                    before[next] = Some(after.clone());
                    work.push(next);
                }
                Some(held) => {
                    let mut grew = false;
                    for (into, from) in held.iter_mut().zip(&after) {
                        for formed in from {
                            grew |= into.insert(*formed);
                        }
                    }
                    if grew {
                        work.push(next);
                    }
                }
            }
        }
    }
    // An instruction no path reaches runs never; it reaches nothing either.
    before
        .into_iter()
        .map(|state| state.unwrap_or_else(|| vec![BTreeSet::new(); places.len()]))
        .collect()
}

/// The address slots `inst` writes, what each then holds, and the address slot
/// it was made from, for a part or a copy. Only these write one: every other
/// instruction's destination is held by the per-instruction checks to a layout
/// or a `Repr` that is not an address.
fn writes(program: &Program, inst: &Inst, pc: usize) -> Vec<(Slot, Formed, Option<Slot>)> {
    let words = |layout: LayoutId| {
        program
            .layouts
            .get(layout.index())
            .map_or(&[][..], |held| held.words.as_slice())
    };
    match inst {
        Inst::AddrOfSlot { dst, .. } | Inst::AddrOfField { dst, .. } => {
            vec![(*dst, Formed::At(pc, *dst), None)]
        }
        Inst::AddrOfPart { dst, addr, .. } => vec![(*dst, Formed::At(pc, *dst), Some(*addr))],
        Inst::Copy { dst, src, layout } => words(*layout)
            .iter()
            .enumerate()
            .filter(|(_, repr)| **repr == Repr::Addr)
            .map(|(k, _)| {
                let k = k as u32;
                (*dst + k, Formed::At(pc, *dst + k), Some(*src + k))
            })
            .collect(),
        Inst::Load { dst, layout, .. } => words(*layout)
            .iter()
            .enumerate()
            .filter(|(_, repr)| **repr == Repr::Addr)
            .map(|(k, _)| (*dst + k as u32, Formed::Loaded(pc, *dst + k as u32), None))
            .collect(),
        Inst::Clear { slot, layout } => (0..words(*layout).len() as u32)
            .map(|k| (*slot + k, Formed::Null, None))
            .collect(),
        _ => Vec::new(),
    }
}

/// Where control goes after the instruction at `pc`.
fn successors(program: &Program, function: &Function, pc: usize) -> Vec<usize> {
    let next = pc + 1;
    match &function.code[pc] {
        Inst::Jump { to } => vec![*to as usize],
        Inst::BranchFalse { to, .. } => vec![next, *to as usize],
        Inst::CmpBranch { target, .. } | Inst::CmpImmBranch { target, .. } => {
            vec![next, *target as usize]
        }
        Inst::Switch { table, .. } => match program.tables.get(table.index()) {
            Some(table) => table
                .targets
                .iter()
                .chain(std::iter::once(&table.default))
                .map(|to| *to as usize)
                .collect(),
            None => Vec::new(),
        },
        Inst::Return { .. } | Inst::Trap { .. } => Vec::new(),
        _ => vec![next],
    }
}
