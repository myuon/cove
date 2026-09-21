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

    fn code_bytes(handle: Compiled) -> u32 {
        handle.code_bytes
    }

    fn window_code(handle: Compiled) -> cove_native::WindowCode {
        handle.windows
    }

    fn intrinsic_code(handle: Compiled) -> cove_native::IntrinsicCode {
        handle.intrinsics
    }

    const ATTRIBUTES_WINDOWS: bool = false;
    const ATTRIBUTES_INTRINSIC_CALLS: bool = false;
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
fn a_backedge_polls_only_once_the_threshold_is_reached() {
    suite::a_backedge_polls_only_once_the_threshold_is_reached::<Cranelift>();
}

#[test]
fn a_threshold_of_nothing_polls_at_every_backedge() {
    suite::a_threshold_of_nothing_polls_at_every_backedge::<Cranelift>();
}

#[test]
fn a_stop_is_taken_at_the_first_poll_past_the_threshold() {
    suite::a_stop_is_taken_at_the_first_poll_past_the_threshold::<Cranelift>();
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
fn negation_answers_what_the_vm_answers() {
    suite::negation_answers_what_the_vm_answers::<Cranelift>();
}

#[test]
fn negating_the_least_int_raises() {
    suite::negating_the_least_int_raises::<Cranelift>();
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
fn a_three_way_order_writes_minus_one_zero_or_one() {
    suite::a_three_way_order_writes_minus_one_zero_or_one::<Cranelift>();
}

#[test]
fn a_string_order_is_the_runtimes_leaf() {
    suite::a_string_order_is_the_runtimes_leaf::<Cranelift>();
}

#[test]
fn only_a_strings_order_is_in_the_slice() {
    suite::only_a_strings_order_is_in_the_slice::<Cranelift>();
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
fn an_allocation_hands_the_layout_and_the_length_over_whole() {
    suite::an_allocation_hands_the_layout_and_the_length_over_whole::<Cranelift>();
}

#[test]
fn an_allocation_the_runtime_refuses_leaves_as_called() {
    suite::an_allocation_the_runtime_refuses_leaves_as_called::<Cranelift>();
}

#[test]
fn a_reference_is_in_its_slot_across_an_allocation() {
    suite::a_reference_is_in_its_slot_across_an_allocation::<Cranelift>();
}

#[test]
fn a_freeze_relabels_the_store_in_place() {
    suite::a_freeze_relabels_the_store_in_place::<Cranelift>();
}

#[test]
fn every_cold_path_of_a_freeze_goes_to_the_runtime() {
    suite::every_cold_path_of_a_freeze_goes_to_the_runtime::<Cranelift>();
}

#[test]
fn a_keyed_finish_relabels_the_store_into_a_set_or_a_map() {
    suite::a_keyed_finish_relabels_the_store_into_a_set_or_a_map::<Cranelift>();
}

#[test]
fn a_freeze_refuses_a_null_receiver() {
    suite::a_freeze_refuses_a_null_receiver::<Cranelift>();
}

#[test]
fn an_intrinsic_call_is_handed_over_by_its_effects() {
    suite::an_intrinsic_call_is_handed_over_by_its_effects::<Cranelift>();
}

#[test]
fn an_intrinsic_call_out_of_bounds_refuses_the_function() {
    suite::an_intrinsic_call_out_of_bounds_refuses_the_function::<Cranelift>();
}

#[test]
fn a_load_elem_strides_and_bounds_its_index() {
    suite::a_load_elem_strides_and_bounds_its_index::<Cranelift>();
}

#[test]
fn a_store_elem_strides_and_bounds_its_index() {
    suite::a_store_elem_strides_and_bounds_its_index::<Cranelift>();
}

#[test]
fn a_byte_at_reads_one_byte_and_bounds_it() {
    suite::a_byte_at_reads_one_byte_and_bounds_it::<Cranelift>();
}

#[test]
fn a_field_access_reads_and_writes_a_fixed_object() {
    suite::a_field_access_reads_and_writes_a_fixed_object::<Cranelift>();
}

#[test]
fn a_field_access_refuses_a_null_receiver() {
    suite::a_field_access_refuses_a_null_receiver::<Cranelift>();
}

#[test]
fn a_field_access_on_a_variable_payload_object_goes_to_the_runtime() {
    suite::a_field_access_on_a_variable_payload_object_goes_to_the_runtime::<Cranelift>();
}

#[test]
fn a_refused_field_access_publishes_its_unpaid_work() {
    suite::a_refused_field_access_publishes_its_unpaid_work::<Cranelift>();
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

#[test]
fn a_clear_zeroes_the_words_its_layout_names() {
    suite::a_clear_zeroes_the_words_its_layout_names::<Cranelift>();
}

#[test]
fn an_address_of_a_slot_is_the_linear_address_of_it() {
    suite::an_address_of_a_slot_is_the_linear_address_of_it::<Cranelift>();
}

#[test]
fn an_address_of_a_part_is_one_addition() {
    suite::an_address_of_a_part_is_one_addition::<Cranelift>();
}

#[test]
fn a_load_and_a_store_reach_either_region() {
    suite::a_load_and_a_store_reach_either_region::<Cranelift>();
}

#[test]
fn a_literal_is_the_address_the_run_placed() {
    suite::a_literal_is_the_address_the_run_placed::<Cranelift>();
}

#[test]
fn a_literal_past_the_table_refuses_the_function() {
    suite::a_literal_past_the_table_refuses_the_function::<Cranelift>();
}

#[test]
fn a_growable_buffer_is_handed_to_the_runtime_whole() {
    suite::a_growable_buffer_is_handed_to_the_runtime_whole::<Cranelift>();
}

#[test]
fn a_buffer_op_the_runtime_refused_leaves_with_that_outcome() {
    suite::a_buffer_op_the_runtime_refused_leaves_with_that_outcome::<Cranelift>();
}

#[test]
fn a_growable_buffer_is_admitted_as_a_family() {
    suite::a_growable_buffer_is_admitted_as_a_family::<Cranelift>();
}

#[test]
fn a_run_copy_is_handed_to_the_runtime_whole() {
    suite::a_run_copy_is_handed_to_the_runtime_whole::<Cranelift>();
}

#[test]
fn a_run_copy_the_runtime_refused_leaves_with_that_outcome() {
    suite::a_run_copy_the_runtime_refused_leaves_with_that_outcome::<Cranelift>();
}

#[test]
fn a_run_copy_is_admitted_with_five_one_word_operands() {
    suite::a_run_copy_is_admitted_with_five_one_word_operands::<Cranelift>();
}

#[test]
fn a_word_truncate_is_handed_to_the_runtime_whole() {
    suite::a_word_truncate_is_handed_to_the_runtime_whole::<Cranelift>();
}

#[test]
fn a_run_slice_is_handed_to_the_runtime_whole() {
    suite::a_run_slice_is_handed_to_the_runtime_whole::<Cranelift>();
}

#[test]
fn a_run_slice_is_admitted_with_four_one_word_operands() {
    suite::a_run_slice_is_admitted_with_four_one_word_operands::<Cranelift>();
}

#[test]
fn a_run_find_is_handed_to_the_runtime_whole() {
    suite::a_run_find_is_handed_to_the_runtime_whole::<Cranelift>();
}

#[test]
fn a_run_find_is_admitted_with_four_one_word_operands() {
    suite::a_run_find_is_admitted_with_four_one_word_operands::<Cranelift>();
}

#[test]
fn a_unit_constant_is_a_zero_word() {
    suite::a_unit_constant_is_a_zero_word::<Cranelift>();
}

#[test]
fn a_reservation_with_room_is_answered_in_emitted_code() {
    suite::a_reservation_with_room_is_answered_in_emitted_code::<Cranelift>();
}

#[test]
fn every_cold_path_of_a_reservation_goes_to_the_runtime() {
    suite::every_cold_path_of_a_reservation_goes_to_the_runtime::<Cranelift>();
}

#[test]
fn a_byte_store_blends_the_byte() {
    suite::a_byte_store_blends_the_byte::<Cranelift>();
}

#[test]
fn every_cold_path_of_a_byte_store_goes_to_the_runtime() {
    suite::every_cold_path_of_a_byte_store_goes_to_the_runtime::<Cranelift>();
}

#[test]
fn a_window_instruction_refuses_null_and_leaves_when_refused() {
    suite::a_window_instruction_refuses_null_and_leaves_when_refused::<Cranelift>();
}

#[test]
fn a_push_window_with_room_writes_the_unit_and_commits_it() {
    suite::a_push_window_with_room_writes_the_unit_and_commits_it::<Cranelift>();
}

#[test]
fn every_cold_path_of_a_push_window_is_one_of_its_rows() {
    suite::every_cold_path_of_a_push_window_is_one_of_its_rows::<Cranelift>();
}

#[test]
fn an_append_window_is_an_ensure_a_copy_and_a_commit() {
    suite::an_append_window_is_an_ensure_a_copy_and_a_commit::<Cranelift>();
}

#[test]
fn a_window_is_less_code_than_its_rows() {
    suite::a_window_is_less_code_than_its_rows::<Cranelift>();
}

#[test]
fn a_windows_machine_code_is_charged_to_its_pattern() {
    suite::a_windows_machine_code_is_charged_to_its_pattern::<Cranelift>();
}

#[test]
fn an_intrinsic_calls_machine_code_is_charged_to_its_variant() {
    suite::an_intrinsic_calls_machine_code_is_charged_to_its_variant::<Cranelift>();
}

#[test]
fn every_intrinsic_call_site_is_counted() {
    suite::every_intrinsic_call_site_is_counted::<Cranelift>();
}
