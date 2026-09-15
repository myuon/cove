//! The hand-written x86-64 template arm, run against the shared suite.
//!
//! Every expectation is in `tests/suite/mod.rs`, and every test below is one
//! line: the suite is what both arms are held to, and a file that could
//! disagree with the other arm's file would be the wrong shape for a
//! comparison. What belongs here is the binding of the suite's `Arm` to this
//! code generator, and nothing else — every expectation the Cranelift arm is
//! held to, this arm is held to, because both must equal the VM.

#![cfg(feature = "template")]

use cove_ir::{FunctionId, Program};
use cove_native::template::{Compiled, Jit};
use cove_native::{Entry, NativeHelpers};

mod suite;

use suite::Arm;

struct Template(Jit);

impl Arm for Template {
    type Handle = Compiled;

    fn new(helpers: NativeHelpers) -> Self {
        Template(Jit::new(helpers).expect("this host is x86-64"))
    }

    fn compile(&mut self, program: &Program, id: FunctionId) -> Option<Compiled> {
        self.0.compile(program, id)
    }

    fn finalize(&mut self) {
        self.0.finalize().expect("the code finalizes");
    }

    fn entry(&self, handle: Compiled) -> Entry {
        self.0.entry(handle)
    }
}

#[test]
fn a_loop_answers_and_polls_once_per_backedge() {
    suite::a_loop_answers_and_polls_once_per_backedge::<Template>();
}

#[test]
fn the_work_charge_is_the_static_block_count() {
    suite::the_work_charge_is_the_static_block_count::<Template>();
}

#[test]
fn a_safepoint_can_stop_the_run() {
    suite::a_safepoint_can_stop_the_run::<Template>();
}

#[test]
fn a_zero_width_return_writes_nothing() {
    suite::a_zero_width_return_writes_nothing::<Template>();
}

#[test]
fn leaving_publishes_no_destination() {
    suite::leaving_publishes_no_destination::<Template>();
}

#[test]
fn integer_arithmetic_answers_what_the_vm_answers() {
    suite::integer_arithmetic_answers_what_the_vm_answers::<Template>();
}

#[test]
fn every_arithmetic_failure_is_the_vms() {
    suite::every_arithmetic_failure_is_the_vms::<Template>();
}

#[test]
fn negation_answers_what_the_vm_answers() {
    suite::negation_answers_what_the_vm_answers::<Template>();
}

#[test]
fn negating_the_least_int_raises() {
    suite::negating_the_least_int_raises::<Template>();
}

#[test]
fn an_immediate_operand_fails_the_same_way() {
    suite::an_immediate_operand_fails_the_same_way::<Template>();
}

#[test]
fn a_duration_destination_renames_only_three_overflows() {
    suite::a_duration_destination_renames_only_three_overflows::<Template>();
}

#[test]
fn a_fused_comparison_branches_and_writes_its_bool() {
    suite::a_fused_comparison_branches_and_writes_its_bool::<Template>();
}

#[test]
fn a_fused_immediate_comparison_branches_and_writes_its_bool() {
    suite::a_fused_immediate_comparison_branches_and_writes_its_bool::<Template>();
}

#[test]
fn a_comparison_writes_one_or_zero() {
    suite::a_comparison_writes_one_or_zero::<Template>();
}

#[test]
fn a_copy_moves_every_word_and_does_not_smear() {
    suite::a_copy_moves_every_word_and_does_not_smear::<Template>();
}

#[test]
fn a_trap_names_its_message_by_id() {
    suite::a_trap_names_its_message_by_id::<Template>();
}

#[test]
fn anything_outside_the_slice_refuses_the_whole_function() {
    suite::anything_outside_the_slice_refuses_the_whole_function::<Template>();
}

#[test]
fn a_bool_equality_is_inside_the_slice() {
    suite::a_bool_equality_is_inside_the_slice::<Template>();
}

#[test]
fn a_boolean_constant_is_a_word() {
    suite::a_boolean_constant_is_a_word::<Template>();
}

#[test]
fn one_code_generator_holds_many_functions() {
    suite::one_code_generator_holds_many_functions::<Template>();
}

#[test]
fn a_reference_slot_is_inside_the_slice() {
    suite::a_reference_slot_is_inside_the_slice::<Template>();
}

#[test]
fn a_tag_is_the_case_index_as_a_word() {
    suite::a_tag_is_the_case_index_as_a_word::<Template>();
}

#[test]
fn a_tag_comparison_is_a_word_comparison() {
    suite::a_tag_comparison_is_a_word_comparison::<Template>();
}

#[test]
fn not_tests_the_whole_word() {
    suite::not_tests_the_whole_word::<Template>();
}

#[test]
fn a_len_reads_the_header_and_refuses_null() {
    suite::a_len_reads_the_header_and_refuses_null::<Template>();
}

#[test]
fn an_allocation_hands_the_layout_and_the_length_over_whole() {
    suite::an_allocation_hands_the_layout_and_the_length_over_whole::<Template>();
}

#[test]
fn an_allocation_the_runtime_refuses_leaves_as_called() {
    suite::an_allocation_the_runtime_refuses_leaves_as_called::<Template>();
}

#[test]
fn a_reference_is_in_its_slot_across_an_allocation() {
    suite::a_reference_is_in_its_slot_across_an_allocation::<Template>();
}

#[test]
fn a_push_into_spare_capacity_writes_the_element_and_the_length() {
    suite::a_push_into_spare_capacity_writes_the_element_and_the_length::<Template>();
}

#[test]
fn every_cold_path_of_a_push_goes_to_the_runtime() {
    suite::every_cold_path_of_a_push_goes_to_the_runtime::<Template>();
}

#[test]
fn a_cold_push_that_raised_leaves_with_that_outcome() {
    suite::a_cold_push_that_raised_leaves_with_that_outcome::<Template>();
}

#[test]
fn a_push_refuses_a_null_receiver() {
    suite::a_push_refuses_a_null_receiver::<Template>();
}

#[test]
fn a_freeze_relabels_the_store_in_place() {
    suite::a_freeze_relabels_the_store_in_place::<Template>();
}

#[test]
fn every_cold_path_of_a_freeze_goes_to_the_runtime() {
    suite::every_cold_path_of_a_freeze_goes_to_the_runtime::<Template>();
}

#[test]
fn a_freeze_refuses_a_null_receiver() {
    suite::a_freeze_refuses_a_null_receiver::<Template>();
}

#[test]
fn a_builtin_no_arm_lowers_refuses_the_function() {
    suite::a_builtin_no_arm_lowers_refuses_the_function::<Template>();
}

#[test]
fn a_load_elem_strides_and_bounds_its_index() {
    suite::a_load_elem_strides_and_bounds_its_index::<Template>();
}

#[test]
fn a_store_elem_strides_and_bounds_its_index() {
    suite::a_store_elem_strides_and_bounds_its_index::<Template>();
}

#[test]
fn a_byte_at_reads_one_byte_and_bounds_it() {
    suite::a_byte_at_reads_one_byte_and_bounds_it::<Template>();
}

#[test]
fn a_fused_byte_comparison_branches_and_writes_both_words() {
    suite::a_fused_byte_comparison_branches_and_writes_both_words::<Template>();
}

#[test]
fn a_field_access_reads_and_writes_a_fixed_object() {
    suite::a_field_access_reads_and_writes_a_fixed_object::<Template>();
}

#[test]
fn a_field_access_refuses_a_null_receiver() {
    suite::a_field_access_refuses_a_null_receiver::<Template>();
}

#[test]
fn a_field_access_on_a_variable_payload_object_goes_to_the_runtime() {
    suite::a_field_access_on_a_variable_payload_object_goes_to_the_runtime::<Template>();
}

#[test]
fn a_refused_field_access_publishes_its_unpaid_work() {
    suite::a_refused_field_access_publishes_its_unpaid_work::<Template>();
}

#[test]
fn a_switch_takes_its_case_or_the_default() {
    suite::a_switch_takes_its_case_or_the_default::<Template>();
}

#[test]
fn a_call_hands_over_and_an_outcome_travels_out() {
    suite::a_call_hands_over_and_an_outcome_travels_out::<Template>();
}

#[test]
fn a_reference_is_in_its_slot_at_every_safepoint() {
    suite::a_reference_is_in_its_slot_at_every_safepoint::<Template>();
}

#[test]
fn a_clear_zeroes_the_words_its_layout_names() {
    suite::a_clear_zeroes_the_words_its_layout_names::<Template>();
}

#[test]
fn an_address_of_a_slot_is_the_linear_address_of_it() {
    suite::an_address_of_a_slot_is_the_linear_address_of_it::<Template>();
}

#[test]
fn an_address_of_a_part_is_one_addition() {
    suite::an_address_of_a_part_is_one_addition::<Template>();
}

#[test]
fn a_load_and_a_store_reach_either_region() {
    suite::a_load_and_a_store_reach_either_region::<Template>();
}

// --- the direct call ---------------------------------------------------------
//
// Issue #365's Part 2, and the one part of it no shared suite case can reach:
// `Jit::calling_directly` is this arm's alone, so the expectations are here.
//
// What is being tested is the *protocol* rather than the runtime: the doubles
// below are `open` and `close` as a test owns them, so the case can say what
// emitted code did — which entry it called, which frame it stored the arguments
// into, and which outcome it handed to `close` — without a `Machine` anywhere.
// `crates/cove-runtime/tests/native_return.rs` is the other half, where the
// helpers are real and the frames are a real stack.

use std::cell::{Cell, RefCell};
use std::sync::Arc;

use cove_ir::{Arg, ArgsId, ArithOp, Inst, Num, Repr};
use cove_native::{NativeCtx, Opened, Outcome};

thread_local! {
    /// What the double answers as the callee's compiled entry.
    ///
    /// `None` is the ordinary answer for a callee the tier has not compiled, and
    /// means the runtime finished the call itself.
    static CALLEE: Cell<Option<Entry>> = const { Cell::new(None) };
    /// The frame the double hands back, as a word index.
    static CALLEE_FRAME: Cell<u64> = const { Cell::new(0) };
    /// What the double answers when it answers no entry.
    static FINISHED_WITH: Cell<u32> = const { Cell::new(0) };
    /// Every `open` emitted code reached.
    static OPENS: RefCell<Vec<Handed>> = const { RefCell::new(Vec::new()) };
    /// Every `close`: the outcome and the callee it named.
    static CLOSES: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
}

/// One hand-over to `open`, as the double recorded it.
///
/// What a case reads to say that emitted code handed over the numbers the IR
/// named: the caller's frame, the instruction, the callee, its argument list, the
/// destination slot, and the unpaid work a real helper would charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Handed {
    base: u64,
    pc: u32,
    callee: u32,
    args: u32,
    dst: u32,
    work: u64,
}

/// A safepoint that always carries on. No fixture below has a backedge, so
/// nothing reaches it; it is here because the table needs one.
///
/// # Safety
///
/// Reads nothing through any of its arguments.
unsafe extern "C" fn safepoint(_ctx: *mut NativeCtx, _pc: u32, _work: u64) -> bool {
    true
}

/// A mediated call helper that must not be reached.
///
/// Every call site in this file is a direct one, so reaching this would mean the
/// generator emitted the wrong form — which a test that answered something would
/// pass anyway.
///
/// # Safety
///
/// Reads nothing through any of its arguments.
unsafe extern "C" fn call(
    _ctx: *mut NativeCtx,
    _base: u64,
    _pc: u32,
    callee: u32,
    _args: u32,
    _dst: u32,
) -> u32 {
    panic!("the mediated helper was reached for callee {callee}, and every call here is direct")
}

/// `open`, as a test owns it: it answers whatever the case asked it to.
///
/// # Safety
///
/// `ctx` is the pointer the entry point was called with.
unsafe extern "C" fn open(
    ctx: *mut NativeCtx,
    base: u64,
    pc: u32,
    callee: u32,
    args: u32,
    dst: u32,
) -> Opened {
    OPENS.with(|held| {
        held.borrow_mut().push(Handed {
            base,
            pc,
            callee,
            args,
            dst,
            work: (*ctx).pending_work,
        })
    });
    // A real one charges what it was handed, so a real one clears it.
    (*ctx).pending_work = 0;
    match CALLEE.with(Cell::get) {
        Some(entry) => Opened {
            entry: Some(entry),
            base: CALLEE_FRAME.with(Cell::get),
        },
        None => Opened {
            entry: None,
            base: u64::from(FINISHED_WITH.with(Cell::get)),
        },
    }
}

/// `close`, as a test owns it: it records and answers the outcome it was given.
///
/// # Safety
///
/// As [`open`].
unsafe extern "C" fn close(ctx: *mut NativeCtx, outcome: u32, callee: u32) -> u32 {
    CLOSES.with(|held| held.borrow_mut().push((outcome, callee)));
    (*ctx).pending_work = 0;
    outcome
}

fn direct_helpers() -> NativeHelpers {
    let shared = suite::helpers();
    NativeHelpers {
        safepoint,
        call,
        open,
        close,
        // The direct-call cases reach neither, so the suite's doubles are bound
        // rather than two more panicking stubs: a table with a `todo!()` in it is
        // a table somebody has to keep honest.
        alloc: shared.alloc,
        builtin: shared.builtin,
        growable: shared.growable,
        run_copy: shared.run_copy,
        field_load: shared.field_load,
        field_store: shared.field_store,
    }
}

/// A caller whose whole body is `g(7, 35)`, and a `g` that adds its two
/// parameters.
///
/// Two parameters rather than one because the whole of what emitted code took
/// over is *packing* them: the second one has to land in the callee's slot 1
/// however far apart the caller's slots are, and one argument cannot tell a
/// packed frame from a copied word.
fn adding_program() -> cove_ir::Program {
    let caller = suite::function(
        vec![Repr::Int, Repr::Int, Repr::Int],
        suite::INT,
        vec![
            Inst::Int { dst: 0, value: 7 },
            Inst::Int { dst: 1, value: 35 },
            Inst::Call {
                dst: 2,
                callee: FunctionId(1),
                args: ArgsId(1),
            },
            Inst::Return { src: 2 },
        ],
    );
    let mut program = suite::program_with_args(
        caller,
        vec![
            Arg {
                slot: 0,
                layout: suite::INT,
            },
            Arg {
                slot: 1,
                layout: suite::INT,
            },
        ],
    );
    let mut callee = suite::function(
        vec![Repr::Int, Repr::Int, Repr::Int],
        suite::INT,
        vec![
            Inst::Arith {
                num: Num::Int,
                op: ArithOp::Add,
                dst: 2,
                a: 0,
                b: 1,
            },
            Inst::Return { src: 2 },
        ],
    );
    callee.params = vec![suite::INT, suite::INT];
    callee.name = Arc::from("g");
    program.functions.push(callee);
    program
}

/// Where the frames and the destination sit in the words a case owns.
///
/// None of them is zero and none of them is adjacent to another: a frame formed
/// as if it began at word zero, or a destination formed without its slot, lands
/// somewhere this case reads and finds wrong.
const CALLER_AT: u64 = 8;
const CALLEE_AT: u64 = 32;
const DESTINATION_AT: u64 = 48;
const DESTINATION_SLOT: u32 = 3;

/// Compiles both functions, hands the double the callee's entry, and enters the
/// caller.
fn run_directly(program: &cove_ir::Program, words: &mut [u64], compiled_callee: bool) -> Outcome {
    let mut jit = Jit::new(direct_helpers())
        .expect("this host is x86-64")
        .calling_directly();
    let caller = jit
        .compile(program, FunctionId(0))
        .expect("the caller is inside the slice");
    let callee = jit
        .compile(program, FunctionId(1))
        .expect("the callee is inside the slice");
    jit.finalize().expect("the code finalizes");
    OPENS.with(|held| held.borrow_mut().clear());
    CLOSES.with(|held| held.borrow_mut().clear());
    CALLEE.with(|held| {
        held.set(if compiled_callee {
            Some(jit.entry(callee))
        } else {
            None
        })
    });
    CALLEE_FRAME.with(|held| held.set(CALLEE_AT));
    let mut ctx = NativeCtx::new(std::ptr::null_mut(), words.as_mut_ptr(), 0);
    let entry = jit.entry(caller);
    // Safety: `ctx.words` is `words`, every frame and the destination fit inside
    // it, and the code was emitted for exactly `Entry`'s shape.
    unsafe { entry(&mut ctx, CALLER_AT, DESTINATION_AT, DESTINATION_SLOT) }
}

/// Emitted code reaches the callee's own entry, the arguments arrive packed, and
/// the answer comes back through the destination the caller named.
#[test]
fn a_direct_call_enters_the_callee_itself() {
    let program = adding_program();
    let mut words = vec![0u64; 64];
    let outcome = run_directly(&program, &mut words, true);
    assert_eq!(outcome, Outcome::Returned);
    assert_eq!(
        words[(DESTINATION_AT + u64::from(DESTINATION_SLOT)) as usize],
        42,
        "`g(7, 35)` answered into the destination the entry was given"
    );
    assert_eq!(
        (words[CALLEE_AT as usize], words[CALLEE_AT as usize + 1]),
        (7, 35),
        "both arguments were stored into the callee's frame, packed from slot zero"
    );
    assert_eq!(
        words[CALLER_AT as usize + 2],
        42,
        "the callee wrote the caller's `dst`, which is what ADR 0057 hands it"
    );
    let opens = OPENS.with(|held| held.borrow().clone());
    assert_eq!(
        opens,
        vec![Handed {
            base: CALLER_AT,
            pc: 2,
            callee: 1,
            args: 1,
            dst: 2,
            work: 4,
        }],
        "one `open`, for callee 1 at pc 2 with `dst` 2, handed the block's four \
         instructions of unpaid work"
    );
    assert_eq!(
        CLOSES.with(|held| held.borrow().clone()),
        vec![(Outcome::Returned.abi(), 1)],
        "one `close`, naming the callee and the outcome the entry answered"
    );
}

/// A callee with no compiled code: the runtime finished the call, and emitted
/// code neither enters anything nor closes anything.
#[test]
fn a_direct_call_falls_back_to_the_runtime() {
    let program = adding_program();
    let mut words = vec![0u64; 64];
    FINISHED_WITH.with(|held| held.set(Outcome::Returned.abi()));
    let outcome = run_directly(&program, &mut words, false);
    assert_eq!(outcome, Outcome::Returned);
    assert_eq!(
        words[CALLEE_AT as usize], 0,
        "no argument was stored, because no frame was opened for one"
    );
    assert!(
        CLOSES.with(|held| held.borrow().is_empty()),
        "nothing was closed, because nothing was entered"
    );
    assert_eq!(OPENS.with(|held| held.borrow().len()), 1);
}

/// An outcome the runtime answers instead of a frame travels straight out of the
/// compiled function.
#[test]
fn a_refusal_from_the_open_helper_leaves() {
    for leaving in [Outcome::Raised, Outcome::Stopped] {
        let program = adding_program();
        let mut words = vec![0u64; 64];
        FINISHED_WITH.with(|held| held.set(leaving.abi()));
        let outcome = run_directly(&program, &mut words, false);
        assert_eq!(
            outcome, leaving,
            "`{leaving:?}` from `open` left through the caller unchanged"
        );
        assert_eq!(
            words[(DESTINATION_AT + u64::from(DESTINATION_SLOT)) as usize],
            0,
            "a call that did not happen published nothing"
        );
    }
}

/// The same arm, emitting direct calls, is still held to the suite's call case.
///
/// The suite's `open` answers "the runtime finished it", so what this exercises
/// is the fallback half of the direct sequence over every expectation that case
/// makes — the outcome that travels out, the destination the helper wrote, and
/// the work the hand-over carried.
#[test]
fn a_direct_arm_still_hands_over_and_an_outcome_travels_out() {
    suite::a_call_hands_over_and_an_outcome_travels_out::<TemplateDirect>();
}

/// The template arm with [`Jit::calling_directly`] on.
struct TemplateDirect(Jit);

impl Arm for TemplateDirect {
    type Handle = Compiled;

    fn new(helpers: NativeHelpers) -> Self {
        TemplateDirect(
            Jit::new(helpers)
                .expect("this host is x86-64")
                .calling_directly(),
        )
    }

    fn compile(&mut self, program: &Program, id: FunctionId) -> Option<Compiled> {
        self.0.compile(program, id)
    }

    fn finalize(&mut self) {
        self.0.finalize().expect("the code finalizes");
    }

    fn entry(&self, handle: Compiled) -> Entry {
        self.0.entry(handle)
    }
}

#[test]
fn a_literal_is_the_address_the_run_placed() {
    suite::a_literal_is_the_address_the_run_placed::<Template>();
}

#[test]
fn a_literal_past_the_table_refuses_the_function() {
    suite::a_literal_past_the_table_refuses_the_function::<Template>();
}

#[test]
fn a_growable_buffer_is_handed_to_the_runtime_whole() {
    suite::a_growable_buffer_is_handed_to_the_runtime_whole::<Template>();
}

#[test]
fn a_buffer_op_the_runtime_refused_leaves_with_that_outcome() {
    suite::a_buffer_op_the_runtime_refused_leaves_with_that_outcome::<Template>();
}

#[test]
fn a_growable_buffer_is_admitted_as_a_family() {
    suite::a_growable_buffer_is_admitted_as_a_family::<Template>();
}

#[test]
fn a_run_copy_is_handed_to_the_runtime_whole() {
    suite::a_run_copy_is_handed_to_the_runtime_whole::<Template>();
}

#[test]
fn a_run_copy_the_runtime_refused_leaves_with_that_outcome() {
    suite::a_run_copy_the_runtime_refused_leaves_with_that_outcome::<Template>();
}

#[test]
fn a_run_copy_is_admitted_with_five_one_word_operands() {
    suite::a_run_copy_is_admitted_with_five_one_word_operands::<Template>();
}

#[test]
fn a_word_truncate_is_handed_to_the_runtime_whole() {
    suite::a_word_truncate_is_handed_to_the_runtime_whole::<Template>();
}

#[test]
fn a_run_slice_is_handed_to_the_runtime_whole() {
    suite::a_run_slice_is_handed_to_the_runtime_whole::<Template>();
}

#[test]
fn a_run_slice_is_admitted_with_four_one_word_operands() {
    suite::a_run_slice_is_admitted_with_four_one_word_operands::<Template>();
}

#[test]
fn a_unit_constant_is_a_zero_word() {
    suite::a_unit_constant_is_a_zero_word::<Template>();
}
