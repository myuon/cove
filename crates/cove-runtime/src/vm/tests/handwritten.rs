//! Instructions reached over an IR written by hand, because no checked
//! program can reach them.
//!
//! `cove-sema` refuses both of these mistakes, so there is no source to lower
//! that would arrive at either instruction and no differential run to make.
//! What is left to hold is that the floor under a checker that stopped
//! proving one of them is *one* floor and not two, so the VM's answer is
//! compared against the interpreter's own function, called directly.

use super::*;

/// A value no `for` can walk fails in `interp::items_of`'s words on the
/// VM, because they *are* its words: `IterItems` calls that function
/// rather than restating what it decides.
///
/// This does not run both backends from source, because no program can
/// reach it on either. `cove-sema` refuses the mistake —
/// `cove::type::iterable` — so a checked program has no `for` over a
/// value that is not a collection, and there is nothing to lower that
/// would arrive at one. What is left to hold is that the floor under a
/// checker that stopped proving it is one floor and not two, so the
/// instruction is executed over an IR written by hand and the answer is
/// compared against the oracle's own function.
#[test]
fn a_value_that_cannot_be_walked_fails_in_the_interpreters_words() {
    let (sources, checked) = checked_module("export fn main() -> Int {\n  1\n}\n");
    let span = Span::new(FileId(0), 0, 1);
    let (on_the_vm, on_the_oracle) = crate::on_cove_stack(|| {
        // The IR holds `Rc`s, so it is built on the thread that runs it.
        let code = vec![
            cove_ir::Inst::Const(cove_ir::ConstId(0)),
            cove_ir::Inst::IterItems,
            cove_ir::Inst::Return,
        ];
        let program = Program {
            dispatches: Vec::new(),
            constants: vec![Const::Int(1)],
            functions: vec![cove_ir::Function {
                module: "m".into(),
                name: "main".into(),
                value_frame_size: 0,
                scalar_frame_size: 0,
                place_frame_size: 0,
                arity: 0,
                params: Vec::new(),
                returns: cove_ir::SlotKind::Value,
                has_receiver: false,
                answers_a_task: false,
                captures: Vec::new(),
                capture_base: 0,
                param_names: Vec::new(),
                spans: vec![span; code.len()],
                block_fuel: cove_ir::lower::block_fuel(&code),
                code,
                arg_spans: BTreeMap::new(),
                span,
            }],
        };
        cove_ir::lower::validate(&program)
            .unwrap_or_else(|why| panic!("the hand-written IR holds the VM's invariants: {why}"));
        let buffer = Buffer::default();
        let hosts = hosts(&buffer, None);
        let runtime = Runtime::new(checked.clone(), sources.clone(), hosts.clone());
        let on_the_vm = Vm::new(&runtime, &hosts, &Arc::new(program))
            .run(FunctionId(0), Vec::new())
            .expect_err("an `Int` cannot be walked")
            .message;
        let on_the_oracle = crate::interp::items_of(Value::Int(1), span)
            .expect_err("an `Int` cannot be walked")
            .message;
        (on_the_vm, on_the_oracle)
    })
    .expect("a thread to run Cove on");
    assert_eq!(on_the_vm, on_the_oracle);
    assert_eq!(
        on_the_vm,
        "`for` iterates an `Array`, a `Vector`, a `Range`, a `Set`, or a `Map`, but found `Int`"
    );
}

/// A case that does not exist, and a payload of the wrong length, fail in
/// `Interpreter::enum_case`'s words on the VM, because they *are* its
/// words: the VM's `MakeEnum` calls that function rather than restating
/// what it decides.
///
/// This is the one place here that does not run both backends, because no
/// program can reach it on either. `cove-sema` refuses both mistakes —
/// `cove::type::unknown_case` and `cove::type::payload_arity` — so a
/// checked program has neither, and there is nothing to lower that would
/// arrive at one. What is left to hold is that the floor under a checker
/// that stopped proving it is one floor and not two, so the instruction is
/// executed over an IR written by hand and the answer is compared against
/// the oracle's own function.
#[test]
fn a_case_that_does_not_exist_fails_in_the_interpreters_words() {
    let (sources, checked) = checked_module(
        "enum Status {\n  Confirmed\n  Pending(Int)\n}\n\nexport fn main() -> Status {\n  Status.Confirmed\n}\n",
    );
    let decl = checked
        .modules
        .get("m")
        .and_then(|resolved| resolved.enums.get("Status"))
        .map(|entry| entry.decl.clone())
        .expect("the module declares `Status`");

    // A case the declaration does not write, over no payload.
    let (vm_said, oracle_said) = built_by_hand(&checked, &sources, "Nope", 0, &decl);
    assert_eq!(vm_said, oracle_said);
    assert_eq!(
        vm_said,
        "enum `Status` has no case or associated function `Nope`"
    );

    // A case that exists, over a payload of the wrong length.
    let (vm_said, oracle_said) = built_by_hand(&checked, &sources, "Confirmed", 2, &decl);
    assert_eq!(vm_said, oracle_said);
    assert_eq!(
        vm_said,
        "case `Status.Confirmed` carries 0 value(s), but 2 were given"
    );
}

/// Runs one `MakeEnum` over `payload` `Unit`s on the VM, and asks
/// [`crate::interp::enum_case`] the same question directly.
///
/// The IR is written here rather than lowered because no source lowers to
/// it: what is being checked is the instruction, not a program.
fn built_by_hand(
    checked: &Arc<Checked>,
    sources: &Arc<SourceMap>,
    case: &str,
    payload: u32,
    decl: &Arc<cove_syntax::ast::EnumDecl>,
) -> (String, String) {
    let span = decl.span;
    crate::on_cove_stack(|| {
        // The IR holds `Rc`s, so it is built on the thread that runs it.
        let mut code = vec![cove_ir::Inst::Const(cove_ir::ConstId(2)); payload as usize];
        code.push(cove_ir::Inst::MakeEnum {
            ty: cove_ir::ConstId(0),
            case: cove_ir::ConstId(1),
            argc: payload,
        });
        code.push(cove_ir::Inst::Return);
        let program = Program {
            dispatches: Vec::new(),
            constants: vec![
                Const::Name("m.Status".into()),
                Const::Name(case.into()),
                Const::Unit,
            ],
            functions: vec![cove_ir::Function {
                module: "m".into(),
                name: "main".into(),
                value_frame_size: 0,
                scalar_frame_size: 0,
                place_frame_size: 0,
                arity: 0,
                params: Vec::new(),
                returns: cove_ir::SlotKind::Value,
                has_receiver: false,
                answers_a_task: false,
                captures: Vec::new(),
                capture_base: 0,
                param_names: Vec::new(),
                spans: vec![span; code.len()],
                block_fuel: cove_ir::lower::block_fuel(&code),
                code,
                arg_spans: BTreeMap::new(),
                span,
            }],
        };
        cove_ir::lower::validate(&program)
            .unwrap_or_else(|why| panic!("the hand-written IR holds the VM's invariants: {why}"));
        let buffer = Buffer::default();
        let hosts = hosts(&buffer, None);
        let runtime = Runtime::new(checked.clone(), sources.clone(), hosts.clone());
        let on_the_vm = Vm::new(&runtime, &hosts, &Arc::new(program))
            .run(FunctionId(0), Vec::new())
            .expect_err("the case cannot be built")
            .message;
        let on_the_oracle = crate::interp::enum_case(
            checked,
            "m",
            decl,
            case,
            &mut vec![Value::Unit; payload as usize],
            span,
        )
        .expect_err("the case cannot be built")
        .message;
        (on_the_vm, on_the_oracle)
    })
    .expect("a thread to run Cove on")
}
