use super::*;

// ----------------------------------------------------------- validate

// ------------------------------------------------------------- blocks

/// A listing of the blocks, read the way `render` reads instructions: the
/// head, how far it reaches, and the instruction it begins at.
fn blocks(program: &Program, function: &str) -> String {
    let id = program
        .function_named("m", function)
        .expect("the function is lowered");
    let function = program.function(id);
    let mut out = String::new();
    for (pc, count) in function.block_fuel.iter().enumerate() {
        if *count != 0 {
            out.push_str(&format!(
                "{pc}+{count} {}\n",
                crate::render(program, id)
                    .lines()
                    .nth(pc + 1)
                    .and_then(|line| line.trim().split_once("  "))
                    .expect("the listing has a line per instruction")
                    .1
            ));
        }
    }
    out
}

/// Every head reaches the jump that ends its straight line, and the run-up
/// to a loop reaches past the head the back edge lands on — which is what
/// makes the extents overlap and what makes falling into a head already
/// paid for.
#[test]
fn a_head_reaches_the_jump_that_ends_its_line() {
    let program = lower(&checked(
        "fn f() -> Int {\n  var i = 0\n  while i < 10 {\n    i = i + 1\n  }\n  i\n}\n",
    ))
    .expect("it lowers");
    assert_eq!(
        blocks(&program, "f"),
        "0+6 scalar-const 0\n\
         2+4 load-scalar 0\n\
         6+5 load-scalar 0\n\
         11+2 load-scalar 0\n"
    );
}

/// The case a partition would lose: an `if` with no `else` falls into the
/// join its own jump also targets, and nothing about that fall announces
/// itself. The head above the join has to reach past it, or the
/// instructions after the join run for free.
#[test]
fn a_head_reaches_past_a_join_it_falls_into() {
    let program = lower(&checked(
        "fn f(b: Bool) -> Int {\n  var i = 0\n  if b {\n    i = 1\n  }\n  i\n}\n",
    ))
    .expect("it lowers");
    let function = program.function(program.function_named("m", "f").expect("`f` is lowered"));
    let join = match function.code.iter().find_map(|inst| match inst {
        Inst::JumpIfFalse(to) | Inst::JumpIfFalseScalar(to) => Some(*to as usize),
        _ => None,
    }) {
        Some(join) => join,
        None => panic!("an `if` lowers to a conditional jump"),
    };
    assert_ne!(function.block_fuel[join], 0, "the join is a head");
    let above = (0..join)
        .rev()
        .find(|pc| function.block_fuel[*pc] != 0)
        .expect("some head stands above the join");
    assert!(
        above + function.block_fuel[above] as usize > join,
        "the head at {above} reaches {} and the join is at {join}",
        above + function.block_fuel[above] as usize
    );
}

/// A call ends a block, because the callee runs before the caller's next
/// instruction does and the caller's fuel has not been charged yet.
#[test]
fn a_call_ends_the_block_it_stands_in() {
    let program = lower(&checked(
        "fn g(a: Int) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g(1) + g(2)\n}\n",
    ))
    .expect("it lowers");
    assert_eq!(
        blocks(&program, "f"),
        "0+2 scalar-const 1\n\
         2+2 scalar-const 2\n\
         4+2 int Add\n"
    );
}

/// Whichever head control last arrived at, its extent reaches every
/// instruction between that head and the next one it can leave from. That
/// is the property the VM's instruction count rests on: an instruction
/// outside every extent above it would run without being charged.
#[test]
fn every_instruction_is_inside_the_extent_of_the_head_above_it() {
    let program = lower(&checked(
        "fn g(a: Int) -> Int {\n  a\n}\n\n\
         fn f(b: Bool) -> Int {\n  \
           var total = 0\n  \
           for x in [1, 2, 3] {\n    \
             if b && x > 1 {\n      total = total + g(x)\n    } else {\n      total = total - 1\n    }\n  \
           }\n  \
           total\n\
         }\n",
    ))
    .expect("it lowers");
    validate(&program).expect("it holds the invariants");
    for function in &program.functions {
        let mut reaches = 0usize;
        for pc in 0..function.code.len() {
            if function.block_fuel[pc] != 0 {
                reaches = reaches.max(pc + function.block_fuel[pc] as usize);
            }
            assert!(
                reaches > pc,
                "{}: {pc} is inside no head's extent",
                function.name
            );
        }
    }
}

/// A head the code does not name is a head the VM never arrives at, so
/// the instructions its extent covers would be charged twice — once by
/// it, and once by whichever head control really came from.
#[test]
fn validate_refuses_a_block_head_the_code_does_not_name() {
    let mut program = lower(&checked(
        "fn f() -> Int {\n  var i = 0\n  while i < 10 {\n    i = i + 1\n  }\n  i\n}\n",
    ))
    .expect("it lowers");
    program.functions[0].block_fuel[3] = 3;
    assert_eq!(
        validate(&program).expect_err("a head nothing reaches is refused"),
        "m.f: 3: begins no block, and the table begins one of 3 there"
    );
}

/// And a head the code does name, missing from the table, is an arrival
/// that charges nothing at all.
#[test]
fn validate_refuses_a_block_head_the_table_is_missing() {
    let mut program = lower(&checked(
        "fn f() -> Int {\n  var i = 0\n  while i < 10 {\n    i = i + 1\n  }\n  i\n}\n",
    ))
    .expect("it lowers");
    program.functions[0].block_fuel[2] = 0;
    assert_eq!(
        validate(&program).expect_err("a head the table is missing is refused"),
        "m.f: 2: begins a block of 4, and the table begins none there"
    );
}

/// An extent that stops short of the instruction control leaves from is
/// refused whatever it stops on, because the rest of that straight line
/// would run uncharged.
#[test]
fn validate_refuses_a_block_that_ends_where_control_does_not() {
    let mut program = lower(&checked(
        "fn f() -> Int {\n  var i = 0\n  while i < 10 {\n    i = i + 1\n  }\n  i\n}\n",
    ))
    .expect("it lowers");
    program.functions[0].block_fuel[2] = 3;
    assert_eq!(
        validate(&program).expect_err("a block that stops short is refused"),
        "m.f: 2: begins a block of 3, which ends where control does not"
    );
}

#[test]
fn validate_refuses_a_jump_past_the_end() {
    let mut program = lower(&checked("fn f() -> Int {\n  1\n}\n")).expect("it lowers");
    let span = program.functions[0].span;
    program.functions[0].code.insert(0, Inst::Jump(99));
    program.functions[0].spans.insert(0, span);
    assert_eq!(
        validate(&program).expect_err("a jump past the end is refused"),
        "m.f: 0: jumps to 99, past the 3 instructions"
    );
}

#[test]
fn validate_refuses_a_slot_outside_the_frame() {
    let mut program = lower(&checked("fn f() -> Int {\n  1\n}\n")).expect("it lowers");
    program.functions[0].code[0] = Inst::LoadLocal(4);
    assert_eq!(
        validate(&program).expect_err("a slot outside the frame is refused"),
        "m.f: 0: reaches slot 4 of a frame of 0"
    );
}

/// An instruction that names a slot of the wrong region is refused as
/// *that*, and not as a slot outside the frame.
///
/// This is what the one numbering made sayable. A slot number used to be
/// bounded by its own stack's frame size, and two numberings meant slot 0
/// existed twice — so a `store` reaching a scalar slot could only be caught
/// where the value frame happened to be too small for the number, and could
/// not be caught at all where both frames were wide enough. There is one
/// slot 0 now, and the region it is in is a fact about the frame, so the
/// diagnostic names the mistake rather than a coincidence.
///
/// Both programs below have room for the number in the *other* region, which
/// is the case the old bound could not see: the first has one scalar slot
/// and no value slot, the second one value slot and no scalar slot, and each
/// is refused for naming the region it did not mean.
#[test]
fn validate_refuses_a_slot_of_the_wrong_region() {
    let source = "fn f() -> Int {\n  let a = 1\n  a\n}\n";
    let mut program = lower(&checked(source)).expect("it lowers");
    program.functions[0].code[1] = Inst::StoreLocal(0);
    assert_eq!(
        validate(&program).expect_err("a value instruction naming a scalar slot is refused"),
        "m.f: 1: reaches slot 0, which this frame keeps in its scalar region and not its value"
    );

    let mut program = lower(&checked("fn f(s: String) -> String {\n  s\n}\n")).expect("it lowers");
    program.functions[0].code[0] = Inst::LoadScalar(0);
    assert_eq!(
        validate(&program).expect_err("a scalar instruction naming a value slot is refused"),
        "m.f: 0: reaches slot 0, which this frame keeps in its value region and not its scalar"
    );
}

/// The two numbers a mixed frame gives one region each, told apart.
///
/// `f` keeps one scalar slot and one value slot, so its numbering is
/// `0` scalar and `1` value and there is no number both instructions may
/// carry. The check that this replaced could see neither of these: slot 0
/// and slot 1 were each in range of both stacks.
#[test]
fn validate_tells_the_two_regions_of_one_frame_apart() {
    let source = "fn f(s: String) -> Int {\n  let a = 1\n  a\n}\n";
    let mut program = lower(&checked(source)).expect("it lowers");
    assert_eq!(program.functions[0].scalar_frame_size, 1);
    assert_eq!(program.functions[0].value_frame_size, 1);

    let mut wrong = lower(&checked(source)).expect("it lowers");
    wrong.functions[0].code[1] = Inst::StoreLocal(0);
    assert_eq!(
        validate(&wrong).expect_err("a value instruction naming the scalar slot is refused"),
        "m.f: 1: reaches slot 0, which this frame keeps in its scalar region and not its value"
    );

    program.functions[0].code[1] = Inst::StoreScalar(1);
    assert_eq!(
        validate(&program).expect_err("a scalar instruction naming the value slot is refused"),
        "m.f: 1: reaches slot 1, which this frame keeps in its value region and not its scalar"
    );
}

/// Two sibling blocks each take the first slot of their own region, because
/// a region's width is its own high-water mark.
///
/// The first block's `Int` takes the first slot of the scalar region, which
/// is slot 0. The second block's `String` takes the first slot of the value
/// region, which is a different number and is not skipped past — the two
/// regions grow independently even though the numbering over them is one.
/// The third block's `Int` reuses slot 0 again. The frame is as wide as each
/// region's deepest block, not as wide as their sum.
#[test]
fn sibling_blocks_share_a_slot_number_regardless_of_kind() {
    assert_eq!(
        listing(
            "fn f() -> Unit {\n  {\n    let a = 1\n  }\n  {\n    let b = \"two\"\n  }\n  {\n    let c = 3\n  }\n}\n",
            "f"
        ),
        "fn m.f arity=0 frame=1/1 -> value\n\
         \x20  0  scalar-const 1\n\
         \x20  1  store-scalar 0\n\
         \x20  2  const Str(\"two\")\n\
         \x20  3  store 1\n\
         \x20  4  scalar-const 3\n\
         \x20  5  store-scalar 0\n\
         \x20  6  const Unit\n\
         \x20  7  return\n"
    );
}

#[test]
fn validate_refuses_a_join_reached_at_two_depths() {
    let mut program = lower(&checked(
        "fn f(b: Bool) -> Int {\n  if b {\n    1\n  } else {\n    2\n  }\n}\n",
    ))
    .expect("it lowers");
    // One more value on the branch that jumps to the join than on the
    // branch that falls into it.
    let unit = program.constants.len() as u32;
    program.constants.push(Const::Unit);
    let function = &mut program.functions[0];
    let span = function.span;
    function.code.insert(2, Inst::Const(ConstId(unit)));
    function.spans.insert(2, span);
    for inst in &mut function.code {
        match inst {
            Inst::Jump(to) | Inst::JumpIfFalse(to) | Inst::JumpIfTrue(to) => *to += 1,
            _ => {}
        }
    }
    assert!(
        validate(&program)
            .expect_err("a join at two depths is refused")
            .contains("on the stack"),
        "{:?}",
        validate(&program)
    );
}

#[test]
fn validate_refuses_a_function_that_does_not_end_in_a_return() {
    let mut program = lower(&checked("fn f() -> String {\n  \"hi\"\n}\n")).expect("it lowers");
    program.functions[0].code.pop();
    program.functions[0].spans.pop();
    assert_eq!(
        validate(&program).expect_err("a missing return is refused"),
        "m.f: does not end in a `return`"
    );
}

/// The instruction a function must end in is the one its convention
/// names, so a scalar-answering function is missing a different one.
#[test]
fn validate_refuses_a_scalar_function_that_does_not_end_in_a_return_scalar() {
    let mut program = lower(&checked("fn f() -> Int {\n  1\n}\n")).expect("it lowers");
    program.functions[0].code.pop();
    program.functions[0].spans.pop();
    assert_eq!(
        validate(&program).expect_err("a missing return is refused"),
        "m.f: does not end in a `return-scalar`"
    );
}

#[test]
fn validate_refuses_argument_spans_for_an_instruction_that_does_not_exist() {
    let mut program = lower(&checked(
        "fn f() -> Result<Unit, Error> {\n  assert(1 > 2)?\n  Ok(())\n}\n",
    ))
    .expect("it lowers");
    let function = &mut program.functions[0];
    let span = function.span;
    function.arg_spans.insert(99, vec![span]);
    assert_eq!(
        validate(&program).expect_err("spans for no instruction are refused"),
        "m.f: carries argument spans for instruction 99 of 10"
    );
}

#[test]
fn validate_refuses_a_call_with_the_wrong_number_of_arguments() {
    let mut program = lower(&checked(
        "fn g(a: Int) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g(1)\n}\n",
    ))
    .expect("it lowers");
    let id = program.function_named("m", "f").expect("`f` is lowered");
    let function = &mut program.functions[id.0 as usize];
    for inst in &mut function.code {
        if let Inst::Call { scalar_argc, .. } = inst {
            *scalar_argc = 2;
        }
    }
    assert!(validate(&program)
        .expect_err("a mismatched call is refused")
        .contains("with 2 arguments, which takes 1"),);
}

/// A function that holds captures is entered through the closure that
/// holds them, so a direct `Call` to one would open a frame with the
/// capture slots left holding `Unit`.
#[test]
fn validate_refuses_a_direct_call_to_a_function_that_holds_captures() {
    let mut program = lower(&checked(
        "fn f(a: Int) -> fn() -> Int {\n  fn() {\n    a\n  }\n}\n",
    ))
    .expect("it lowers");
    let closure = program
        .function_named("m", "<closure 0>")
        .expect("the lambda is lowered");
    let id = program.function_named("m", "f").expect("`f` is lowered");
    program.functions[id.0 as usize].code[0] = Inst::Call {
        function: closure,
        value_argc: 0,
        scalar_argc: 0,
        place_argc: 0,
        returns_scalar: false,
    };
    assert!(validate(&program)
        .expect_err("a direct call to a closure body is refused")
        .contains("directly, which is entered through the closure"));
}

/// A closure is called under one convention, and the last point at which
/// the target is known is where the closure is made — so that is where
/// the convention is checked.
#[test]
fn validate_refuses_a_closure_made_of_a_function_that_answers_a_scalar() {
    let mut program = lower(&checked(
        "fn g() -> Int {\n  1\n}\n\nfn f() -> fn() -> Int {\n  fn() {\n    g()\n  }\n}\n",
    ))
    .expect("it lowers");
    let scalar = program.function_named("m", "g").expect("`g` is lowered");
    let id = program.function_named("m", "f").expect("`f` is lowered");
    program.functions[id.0 as usize].code[0] = Inst::MakeClosure {
        function: scalar,
        captures: 0,
    };
    assert!(validate(&program)
        .expect_err("a closure over a scalar answer is refused")
        .contains("which answers on the scalar stack"));
}

/// The counts are per stack and not only in total, because a call that
/// supplied the right number of arguments on the wrong stacks would read
/// words nobody wrote.
#[test]
fn validate_refuses_a_call_that_puts_its_arguments_on_the_wrong_stack() {
    let mut program = lower(&checked(
        "fn g(a: Int) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g(1)\n}\n",
    ))
    .expect("it lowers");
    let id = program.function_named("m", "f").expect("`f` is lowered");
    let function = &mut program.functions[id.0 as usize];
    for inst in &mut function.code {
        if let Inst::Call {
            value_argc,
            scalar_argc,
            ..
        } = inst
        {
            *value_argc = 1;
            *scalar_argc = 0;
        }
    }
    assert!(validate(&program)
        .expect_err("a call on the wrong stacks is refused")
        .contains("with 1 value, 0 scalar and 0 place arguments, which takes 0, 1 and 0"),);
}

/// And the answer's stack likewise: a caller that read the wrong one
/// would read whatever the callee happened to leave behind.
#[test]
fn validate_refuses_a_call_that_expects_its_answer_on_the_wrong_stack() {
    let mut program = lower(&checked(
        "fn g(a: Int) -> Int {\n  a\n}\n\nfn f() -> Int {\n  g(1)\n}\n",
    ))
    .expect("it lowers");
    let id = program.function_named("m", "f").expect("`f` is lowered");
    let function = &mut program.functions[id.0 as usize];
    for inst in &mut function.code {
        if let Inst::Call { returns_scalar, .. } = inst {
            *returns_scalar = false;
        }
    }
    assert!(validate(&program)
        .expect_err("a call reading the wrong stack is refused")
        .contains("for an answer on the value stack, which answers on the scalar"),);
}

/// A function returns on one stack, so a body holding both instructions
/// would leave its caller reading whichever one happened to run.
#[test]
fn validate_refuses_a_function_that_mixes_the_two_returns() {
    let mut program = lower(&checked("fn f(a: Int) -> Int {\n  a\n}\n")).expect("it lowers");
    let id = program.function_named("m", "f").expect("`f` is lowered");
    let function = &mut program.functions[id.0 as usize];
    function.code.insert(0, Inst::Return);
    function.spans.insert(0, function.span);
    assert!(validate(&program)
        .expect_err("a mixed return is refused")
        .contains("answers on the scalar stack and holds a `return`"),);
}
