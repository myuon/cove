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
fn integer_arithmetic_answers_what_the_vm_answers() {
    suite::integer_arithmetic_answers_what_the_vm_answers::<Template>();
}

#[test]
fn every_arithmetic_failure_is_the_vms() {
    suite::every_arithmetic_failure_is_the_vms::<Template>();
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
