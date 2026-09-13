//! The Cranelift arm, run against the shared suite.
//!
//! Every expectation is in `tests/suite/mod.rs`, and every test below is one
//! line: the suite is what both arms are held to, and a file that could
//! disagree with the other arm's file would be the wrong shape for a
//! comparison. What belongs here is the binding of the suite's `Arm` to this
//! code generator, and nothing else.

#![cfg(feature = "cranelift")]

use cove_ir::{FunctionId, Program};
use cove_native::{Compiled, Entry, Jit, NativeHelpers};

mod suite;

use suite::Arm;

struct Cranelift(Jit);

impl Arm for Cranelift {
    type Handle = Compiled;

    fn new(helpers: NativeHelpers) -> Self {
        Cranelift(Jit::new(helpers).expect("this host supports native execution"))
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
    suite::a_loop_answers_and_polls_once_per_backedge::<Cranelift>();
}

#[test]
fn the_work_charge_is_the_static_block_count() {
    suite::the_work_charge_is_the_static_block_count::<Cranelift>();
}

#[test]
fn a_safepoint_can_stop_the_run() {
    suite::a_safepoint_can_stop_the_run::<Cranelift>();
}

#[test]
fn a_zero_width_return_writes_nothing() {
    suite::a_zero_width_return_writes_nothing::<Cranelift>();
}

#[test]
fn leaving_publishes_no_destination() {
    suite::leaving_publishes_no_destination::<Cranelift>();
}

#[test]
fn integer_arithmetic_answers_what_the_vm_answers() {
    suite::integer_arithmetic_answers_what_the_vm_answers::<Cranelift>();
}

#[test]
fn every_arithmetic_failure_is_the_vms() {
    suite::every_arithmetic_failure_is_the_vms::<Cranelift>();
}

#[test]
fn an_immediate_operand_fails_the_same_way() {
    suite::an_immediate_operand_fails_the_same_way::<Cranelift>();
}

#[test]
fn a_duration_destination_renames_only_three_overflows() {
    suite::a_duration_destination_renames_only_three_overflows::<Cranelift>();
}

#[test]
fn a_fused_comparison_branches_and_writes_its_bool() {
    suite::a_fused_comparison_branches_and_writes_its_bool::<Cranelift>();
}

#[test]
fn a_fused_immediate_comparison_branches_and_writes_its_bool() {
    suite::a_fused_immediate_comparison_branches_and_writes_its_bool::<Cranelift>();
}

#[test]
fn a_comparison_writes_one_or_zero() {
    suite::a_comparison_writes_one_or_zero::<Cranelift>();
}

#[test]
fn a_copy_moves_every_word_and_does_not_smear() {
    suite::a_copy_moves_every_word_and_does_not_smear::<Cranelift>();
}

#[test]
fn a_trap_names_its_message_by_id() {
    suite::a_trap_names_its_message_by_id::<Cranelift>();
}

#[test]
fn anything_outside_the_slice_refuses_the_whole_function() {
    suite::anything_outside_the_slice_refuses_the_whole_function::<Cranelift>();
}

#[test]
fn a_bool_equality_is_inside_the_slice() {
    suite::a_bool_equality_is_inside_the_slice::<Cranelift>();
}

#[test]
fn a_boolean_constant_is_a_word() {
    suite::a_boolean_constant_is_a_word::<Cranelift>();
}

#[test]
fn one_code_generator_holds_many_functions() {
    suite::one_code_generator_holds_many_functions::<Cranelift>();
}

#[test]
fn a_reference_slot_is_inside_the_slice() {
    suite::a_reference_slot_is_inside_the_slice::<Cranelift>();
}

#[test]
fn a_tag_is_the_case_index_as_a_word() {
    suite::a_tag_is_the_case_index_as_a_word::<Cranelift>();
}

#[test]
fn a_tag_comparison_is_a_word_comparison() {
    suite::a_tag_comparison_is_a_word_comparison::<Cranelift>();
}

#[test]
fn not_tests_the_whole_word() {
    suite::not_tests_the_whole_word::<Cranelift>();
}

#[test]
fn a_len_reads_the_header_and_refuses_null() {
    suite::a_len_reads_the_header_and_refuses_null::<Cranelift>();
}

#[test]
fn a_load_elem_strides_and_bounds_its_index() {
    suite::a_load_elem_strides_and_bounds_its_index::<Cranelift>();
}

#[test]
fn a_byte_at_reads_one_byte_and_bounds_it() {
    suite::a_byte_at_reads_one_byte_and_bounds_it::<Cranelift>();
}

#[test]
fn a_switch_takes_its_case_or_the_default() {
    suite::a_switch_takes_its_case_or_the_default::<Cranelift>();
}

#[test]
fn a_call_hands_over_and_an_outcome_travels_out() {
    suite::a_call_hands_over_and_an_outcome_travels_out::<Cranelift>();
}

#[test]
fn a_reference_is_in_its_slot_at_every_safepoint() {
    suite::a_reference_is_in_its_slot_at_every_safepoint::<Cranelift>();
}
