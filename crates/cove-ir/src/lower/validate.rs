//! The invariants a lowered program holds before the VM runs it, and the one
//! description of what each instruction does to the operand stacks.
//!
//! The VM trusts its input completely — that is most of what makes it worth
//! having — so this is where the trust is earned, and it is a pass of its
//! own over finished code for the same reason: a check the emitter made as
//! it emitted would be a check made by the thing being checked.
//!
//! [`stack_shape`] is here rather than beside the lowering, and it is not
//! two descriptions of one thing. `super::body`'s `Body::emit` reads it as
//! it emits and [`validate`] reads it as it simulates, so a boundary between
//! the two would be a boundary between two things that must agree. There is
//! one of it, and each of its readers is on the far side of it from the
//! others — including the third, `cove_runtime::frame`'s operand-kind
//! simulation, which is why it is `pub`.

use crate::{
    Const, ConstId, Function, FunctionId, Inst, Program, Region, SlotKind, StructId, StructType,
};

use super::fuel::{block_fuel, ends_a_block};

/// Checks the invariants a lowered function must hold before the VM runs it.
///
/// The VM trusts its input completely — that is most of what makes it worth
/// having — so this is where the trust is earned. Every jump lands on an
/// instruction, every slot is inside the frame, every id names something,
/// every recorded argument span belongs to an instruction that exists, every
/// function ends in the return its convention names, and both operand stacks
/// have one depth at every instruction control can reach: a join point
/// arrived at with two different depths is a bug in the lowering, and finding
/// it here is the difference between a failed test and a VM reading a value
/// that is not there.
///
/// A slot is addressed as what it is, too. A slot number is a number in the
/// one frame numbering, and `region_of` is the one question this asks of it:
/// is this a slot of this frame, and is it in the region the instruction
/// naming it reads. A scalar instruction reaching a value slot — or the
/// other way round, or either reaching a place slot — is caught here rather
/// than read as whichever eight bytes happened to stand there, which is a
/// check three separate bounds could not make: each number was in range of
/// its own stack, and there was no single number that told a value slot and
/// a scalar slot apart.
///
/// The calling convention is checked from both ends, which is what makes it
/// an invariant rather than a convention. A function's `params` has one
/// entry per argument and each stack has room for the parameters that live
/// in it; a function ends in the return its `returns` names and holds no
/// instance of the other one; and every `Call` supplies the counts its
/// callee's `params` describe and expects its answer on the stack its
/// callee's `returns` leaves it on.
pub fn validate(program: &Program) -> Result<(), String> {
    for (index, function) in program.functions.iter().enumerate() {
        let id = FunctionId(index as u32);
        validate_function(program, id)
            .map_err(|why| format!("{}.{}: {why}", function.module, function.name))?;
    }
    Ok(())
}

fn validate_function(program: &Program, id: FunctionId) -> Result<(), String> {
    let function = program.function(id);
    if function.code.is_empty() {
        return Err("has no instructions".to_string());
    }
    if function.spans.len() != function.code.len() {
        return Err(format!(
            "carries {} spans for {} instructions",
            function.spans.len(),
            function.code.len()
        ));
    }
    if function.params.len() != function.arity as usize {
        return Err(format!(
            "takes {} arguments but says where {} of them arrive",
            function.arity,
            function.params.len()
        ));
    }
    let value_params = function
        .params
        .iter()
        .filter(|k| matches!(k, SlotKind::Value))
        .count() as u32;
    let scalar_params = function.params.iter().filter(|k| k.is_scalar()).count() as u32;
    let place_params = function.params.iter().filter(|k| k.is_place()).count() as u32;
    if value_params > function.value_frame_size {
        return Err(format!(
            "takes {value_params} value arguments but has a value frame of {}",
            function.value_frame_size
        ));
    }
    if scalar_params > function.scalar_frame_size {
        return Err(format!(
            "takes {scalar_params} scalar arguments but has a scalar frame of {}",
            function.scalar_frame_size
        ));
    }
    if place_params > function.place_frame_size {
        return Err(format!(
            "takes {place_params} place arguments but has a place frame of {}",
            function.place_frame_size
        ));
    }
    // One return instruction per function, decided by where the answer
    // travels: a caller reads exactly the stack `returns` names, and nothing
    // tells it which of the two a given return happened to use. There is no
    // third: a place is what a parameter can be and not what a call can
    // answer, so a function that claimed to answer one is refused here
    // rather than left for a return instruction that does not exist.
    let (ends_in, other, stack) = match function.returns {
        SlotKind::Value => (Inst::Return, Inst::ReturnScalar, "value"),
        SlotKind::Scalar(_) => (Inst::ReturnScalar, Inst::Return, "scalar"),
        SlotKind::Place => return Err("answers a place, which no call reads".to_string()),
    };
    if function.code.last() != Some(&ends_in) {
        return Err(format!("does not end in a `{}`", render_return(ends_in)));
    }
    if function.code.contains(&other) {
        return Err(format!(
            "answers on the {stack} stack and holds a `{}`",
            render_return(other)
        ));
    }
    // The value captures stand in the value slots straight after the
    // parameters that arrived on the value stack, put there by the call out
    // of the closure it went through, so `capture_base` has to be that
    // number and the frame has to have room for them. The scalar captures
    // stand at scalar slot 0 for the same reason read on the other stack,
    // and the check below that no argument arrives there is what makes 0 a
    // static number. This is the one place the layout the call fills in and
    // the layout the body reads are reconciled.
    if function.capture_base != function.value_origin() + value_params {
        return Err(format!(
            "begins its captures at slot {} and takes {value_params} value argument(s), \
             whose value region begins at slot {}",
            function.capture_base,
            function.value_origin()
        ));
    }
    if !function.captures.is_empty() {
        // Every argument but a `lock` closure's first one arrives on the
        // value stack, which is what makes a capture's slot a static number:
        // see `Function::capture_base`.
        if scalar_params > 0 || place_params > 1 {
            return Err(format!(
                "holds {} capture(s) and takes {} of its {} arguments off another stack",
                function.captures.len(),
                function.arity - value_params,
                function.arity
            ));
        }
        // A closure captures the value a place names and never the place,
        // so no capture is a place slot. `Inst::PlaceLocal` is the argument.
        if function
            .captures
            .iter()
            .any(|(_, kind)| matches!(kind, SlotKind::Place))
        {
            return Err("holds a capture in a place slot".to_string());
        }
        let values = function
            .captures
            .iter()
            .filter(|(_, kind)| matches!(kind, SlotKind::Value))
            .count();
        let scalars = function.captures.len() - values;
        let window = function.capture_base + values as u32;
        if window > function.place_origin() {
            return Err(format!(
                "holds {values} value capture(s) from slot {} in a value region that ends at {}",
                function.capture_base,
                function.place_origin()
            ));
        }
        if scalars > function.scalar_frame_size as usize {
            return Err(format!(
                "holds {scalars} scalar capture(s) in a scalar frame of {}",
                function.scalar_frame_size
            ));
        }
    }
    for at in function.arg_spans.keys() {
        if *at as usize >= function.code.len() {
            return Err(format!(
                "carries argument spans for instruction {at} of {}",
                function.code.len()
            ));
        }
    }

    let length = function.code.len();
    for (pc, inst) in function.code.iter().enumerate() {
        let at = |why: String| format!("{pc}: {why}");
        let constant = |which: ConstId, what: &str| -> Result<(), String> {
            match program.constants.get(which.0 as usize) {
                Some(Const::Name(_)) => Ok(()),
                Some(other) => Err(at(format!("{what} names {other:?} rather than a name"))),
                None => Err(at(format!("{what} names constant {} of none", which.0))),
            }
        };
        let declared_struct = |which: StructId, what: &str| -> Result<&StructType, String> {
            program
                .structs
                .get(which.0 as usize)
                .ok_or_else(|| at(format!("{what} names struct {} of none", which.0)))
        };
        match *inst {
            Inst::Const(which) => {
                if program.constants.get(which.0 as usize).is_none() {
                    return Err(at(format!(
                        "loads constant {}, which does not exist",
                        which.0
                    )));
                }
            }
            // One numbering, so one question: is the slot this instruction
            // names a slot of this frame at all, and is it in the region the
            // instruction is the reader of? Both halves used to be one bound
            // per stack, which could only ask the first half of the question
            // — a `load-scalar 0` and a `load-local 0` named two different
            // slots and each was in range of its own stack. Now they name one
            // slot at most one of them may touch.
            Inst::LoadLocal(slot) | Inst::StoreLocal(slot) | Inst::PlaceLocal(slot) => {
                in_region(function, slot, Region::Value).map_err(&at)?;
            }
            Inst::LoadScalar(slot) | Inst::StoreScalar(slot) | Inst::PlaceScalar(slot, _) => {
                in_region(function, slot, Region::Scalar).map_err(&at)?;
            }
            Inst::LoadPlace(slot) => {
                in_region(function, slot, Region::Place).map_err(&at)?;
            }
            Inst::MakeClosure {
                function: target,
                captures,
            } => {
                let Some(target) = program.functions.get(target.0 as usize) else {
                    return Err(at(format!(
                        "makes a closure of function {}, which does not exist",
                        target.0
                    )));
                };
                if target.captures.len() != usize::from(captures) {
                    return Err(at(format!(
                        "makes a closure of `{}.{}` over {captures} captures, which takes {}",
                        target.module,
                        target.name,
                        target.captures.len()
                    )));
                }
                // The convention `Inst::CallValue` calls under, checked
                // where the closure is *made*, because that is the last
                // point at which the target is known: the call itself
                // reaches whatever value stands there. So a closure can only
                // ever be made of a function that takes its arguments on the
                // value stack and answers on it.
                //
                // With one exception, and it is the one call that does not go
                // through `Inst::CallValue`. The closure `Inst::Lock` is
                // given may take its *first* parameter as a place, because
                // that instruction hands one over itself; every parameter
                // after it is an argument like any other. What keeps such a
                // closure away from a `CallValue` is that `crate::lower`
                // builds one only as the direct argument of a `lock`, where
                // the very next instruction consumes it, so it never becomes
                // a value the program can name.
                if target
                    .params
                    .iter()
                    .skip(1)
                    .any(|kind| !matches!(kind, SlotKind::Value))
                    || target.params.first().is_some_and(|kind| kind.is_scalar())
                {
                    return Err(at(format!(
                        "makes a closure of `{}.{}`, which takes an argument off another stack",
                        target.module, target.name
                    )));
                }
                if target.returns.is_scalar() {
                    return Err(at(format!(
                        "makes a closure of `{}.{}`, which answers on the scalar stack",
                        target.module, target.name
                    )));
                }
            }
            Inst::MakeDyn { trait_name, .. } => constant(trait_name, "the trait")?,
            // The same convention `Inst::MakeClosure` is checked against,
            // and checked here for the same reason: this is the last point
            // at which the targets are known, because the call itself
            // reaches whichever of them the receiver turns out to carry. So
            // every candidate has to take every argument on the value stack
            // and answer on it, and they all have to take the same number.
            Inst::CallDyn { site, argc } => {
                let Some(dispatch) = program.dispatches.get(site.0 as usize) else {
                    return Err(at(format!(
                        "dispatches through site {}, which does not exist",
                        site.0
                    )));
                };
                for (type_name, id) in &dispatch.cases {
                    let Some(target) = program.functions.get(id.0 as usize) else {
                        return Err(at(format!(
                            "dispatches to function {}, which does not exist",
                            id.0
                        )));
                    };
                    if target.arity != u32::from(argc) {
                        return Err(at(format!(
                            "dispatches to `{type_name}.{}` with {argc} arguments, which takes {}",
                            dispatch.method, target.arity
                        )));
                    }
                    if target
                        .params
                        .iter()
                        .any(|kind| !matches!(kind, SlotKind::Value))
                    {
                        return Err(at(format!(
                            "dispatches to `{type_name}.{}`, which takes an argument off another stack",
                            dispatch.method
                        )));
                    }
                    if target.returns.is_scalar() {
                        return Err(at(format!(
                            "dispatches to `{type_name}.{}`, which answers on the scalar stack",
                            dispatch.method
                        )));
                    }
                    if !target.captures.is_empty() {
                        return Err(at(format!(
                            "dispatches to `{type_name}.{}`, which is entered through the closure \
                             that holds its {} capture(s)",
                            dispatch.method,
                            target.captures.len()
                        )));
                    }
                }
            }
            Inst::Jump(to)
            | Inst::JumpIfFalse(to)
            | Inst::JumpIfTrue(to)
            | Inst::JumpIfFalseScalar(to)
            | Inst::JumpIfTrueScalar(to) => {
                if to as usize >= length {
                    return Err(at(format!("jumps to {to}, past the {length} instructions")));
                }
            }
            Inst::Call {
                function: target,
                value_argc,
                scalar_argc,
                place_argc,
                returns_scalar,
            } => {
                let Some(target) = program.functions.get(target.0 as usize) else {
                    return Err(at(format!(
                        "calls function {}, which does not exist",
                        target.0
                    )));
                };
                let value_argc = u32::from(value_argc);
                let scalar_argc = u32::from(scalar_argc);
                let place_argc = u32::from(place_argc);
                if target.arity != value_argc + scalar_argc + place_argc {
                    return Err(at(format!(
                        "calls `{}.{}` with {} arguments, which takes {}",
                        target.module,
                        target.name,
                        value_argc + scalar_argc + place_argc,
                        target.arity
                    )));
                }
                let values = target
                    .params
                    .iter()
                    .filter(|k| matches!(k, SlotKind::Value))
                    .count() as u32;
                let scalars = target.params.iter().filter(|k| k.is_scalar()).count() as u32;
                let places = target.params.iter().filter(|k| k.is_place()).count() as u32;
                if values != value_argc || scalars != scalar_argc || places != place_argc {
                    return Err(at(format!(
                        "calls `{}.{}` with {value_argc} value, {scalar_argc} scalar and {place_argc} place arguments, which takes {values}, {scalars} and {places}",
                        target.module, target.name
                    )));
                }
                if !target.captures.is_empty() {
                    return Err(at(format!(
                        "calls `{}.{}` directly, which is entered through the closure that \
                         holds its {} capture(s)",
                        target.module,
                        target.name,
                        target.captures.len()
                    )));
                }
                if target.returns.is_scalar() != returns_scalar {
                    return Err(at(format!(
                        "calls `{}.{}` for an answer on the {} stack, which answers on the {}",
                        target.module,
                        target.name,
                        if returns_scalar { "scalar" } else { "value" },
                        if target.returns.is_scalar() {
                            "scalar"
                        } else {
                            "value"
                        }
                    )));
                }
            }
            Inst::CallHost { module, op, .. } => {
                constant(module, "the host module")?;
                constant(op, "the host operation")?;
            }
            Inst::CallResource { op, .. } => constant(op, "the resource operation")?,
            Inst::CallBuiltin { name, .. } => constant(name, "the builtin method")?,
            Inst::MakeBuiltin { name, .. } => constant(name, "the builtin")?,
            Inst::MakeEnum { ty, case, .. } => {
                constant(ty, "the enum")?;
                constant(case, "the case")?;
            }
            Inst::CallBuiltinAssoc { ty, name, .. } => {
                constant(ty, "the builtin type")?;
                constant(name, "the associated function")?;
            }
            Inst::TestCase(case) => constant(case, "the case")?,
            Inst::GetField(name) | Inst::SetField(name) => constant(name, "the field")?,
            // A `make-struct` names a `StructType` rather than two constants,
            // so what has to exist is the type.
            Inst::MakeStruct(of) => {
                declared_struct(of, "the type")?;
            }
            Inst::GetFieldAt { of, at: index } => {
                let declared = declared_struct(of, "the struct")?;
                if index as usize >= declared.fields.len() {
                    return Err(at(format!(
                        "reads field {index} of `{}`, which has {}",
                        declared.name,
                        declared.fields.len()
                    )));
                }
            }
            _ => {}
        }
    }

    // Both operand stacks, simulated over every path control can take. Code
    // no path reaches is not simulated: it cannot be run, so its depths are
    // not a fact about anything.
    let mut depths: Vec<Option<(i64, i64, i64)>> = vec![None; length];
    let mut pending = vec![(0usize, (0i64, 0i64, 0i64))];
    while let Some((pc, depth)) = pending.pop() {
        if pc >= length {
            return Err(format!(
                "{}: control runs past the last instruction",
                pc - 1
            ));
        }
        if let Some(seen) = depths[pc] {
            if seen != depth {
                return Err(format!(
                    "{pc}: reached with {} values, {} scalars and {} places on the stack and with {}, {} and {}",
                    depth.0, depth.1, depth.2, seen.0, seen.1, seen.2
                ));
            }
            continue;
        }
        depths[pc] = Some(depth);
        let inst = function.code[pc];
        let shape = stack_shape(&program.structs, inst);
        if depth.0 < i64::from(shape.values.0) {
            return Err(format!(
                "{pc}: takes {} values off a stack of {}",
                shape.values.0, depth.0
            ));
        }
        if depth.1 < i64::from(shape.scalars.0) {
            return Err(format!(
                "{pc}: takes {} scalars off a stack of {}",
                shape.scalars.0, depth.1
            ));
        }
        if depth.2 < i64::from(shape.places.0) {
            return Err(format!(
                "{pc}: takes {} places off a stack of {}",
                shape.places.0, depth.2
            ));
        }
        let after = (
            depth.0 - i64::from(shape.values.0) + i64::from(shape.values.1),
            depth.1 - i64::from(shape.scalars.0) + i64::from(shape.scalars.1),
            depth.2 - i64::from(shape.places.0) + i64::from(shape.places.1),
        );
        match inst {
            // None continues: a return leaves the frame, whichever stack it
            // reads, and a `match` that covered nothing stops the run.
            Inst::Return | Inst::ReturnScalar | Inst::NoMatch => {}
            Inst::Jump(to) => pending.push((to as usize, after)),
            Inst::JumpIfFalse(to)
            | Inst::JumpIfTrue(to)
            | Inst::JumpIfFalseScalar(to)
            | Inst::JumpIfTrueScalar(to) => {
                pending.push((to as usize, after));
                pending.push((pc + 1, after));
            }
            _ => pending.push((pc + 1, after)),
        }
    }

    // The block table, which the VM charges fuel from without looking at it
    // twice. A count is an extent: how far the straight line from that head
    // runs. So each one has to end on an instruction that ends a block and
    // run through no earlier one, and the heads have to be exactly the
    // indices the code names — a head the code does not name is one the VM
    // never arrives at, and a head the table is missing is an arrival that
    // charges nothing.
    if function.block_fuel.len() != length {
        return Err(format!(
            "carries {} block lengths for {length} instructions",
            function.block_fuel.len()
        ));
    }
    for (pc, count) in function.block_fuel.iter().enumerate() {
        let count = *count as usize;
        if count == 0 {
            continue;
        }
        if pc + count > length {
            return Err(format!(
                "{pc}: begins a block of {count}, which runs past the {length} instructions"
            ));
        }
        if let Some(inside) = (pc..pc + count - 1).find(|at| ends_a_block(function.code[*at])) {
            return Err(format!(
                "{pc}: begins a block of {count}, which runs through the one that ends at {inside}"
            ));
        }
        if !ends_a_block(function.code[pc + count - 1]) {
            return Err(format!(
                "{pc}: begins a block of {count}, which ends where control does not"
            ));
        }
    }
    let expected = block_fuel(&function.code);
    if let Some((pc, (held, want))) = function
        .block_fuel
        .iter()
        .zip(&expected)
        .enumerate()
        .find(|(_, (held, want))| held != want)
    {
        return Err(match (held, want) {
            (0, _) => format!("{pc}: begins a block of {want}, and the table begins none there"),
            (_, 0) => format!("{pc}: begins no block, and the table begins one of {held} there"),
            _ => format!("{pc}: begins a block of {want}, and the table says {held}"),
        });
    }
    Ok(())
}

/// How many operands an instruction takes off each stack and puts back.
///
/// Three pairs rather than one, because there are three stacks and an
/// instruction may read one and write another: that is what a boundary
/// instruction *is*, and reading a place is the boundary between the place
/// stack and the value stack.
#[derive(Clone, Copy)]
pub struct Shape {
    /// Taken off, and put back on, the value stack.
    pub values: (u32, u32),
    /// Taken off, and put back on, the scalar stack.
    pub scalars: (u32, u32),
    /// Taken off, and put back on, the place stack.
    pub places: (u32, u32),
}

impl Shape {
    /// An instruction that touches only the value stack.
    const fn on_values(taken: u32, left: u32) -> Shape {
        Shape {
            values: (taken, left),
            scalars: (0, 0),
            places: (0, 0),
        }
    }

    /// An instruction that touches only the scalar stack.
    const fn on_scalars(taken: u32, left: u32) -> Shape {
        Shape {
            values: (0, 0),
            scalars: (taken, left),
            places: (0, 0),
        }
    }

    /// An instruction that touches only the place stack.
    const fn on_places(taken: u32, left: u32) -> Shape {
        Shape {
            values: (0, 0),
            scalars: (0, 0),
            places: (taken, left),
        }
    }
}

/// How many operands an instruction takes off each stack and puts back.
///
/// One description, read by the lowering as it emits and by [`validate`] as
/// it simulates, so the two cannot disagree about what an instruction does.
///
/// It has a third reader outside this crate now, and that is why it is
/// exported rather than copied: `cove_runtime::frame`'s operand-kind
/// simulation walks a function's value operand stack over every path control
/// can take, and needs exactly these counts to know how deep it is standing.
/// A copy of them there would be a second description of what an instruction
/// does, and the whole argument for there being one of these is that two can
/// come apart. A third reader on the far side of the one description cannot
/// disagree with the two already here.
pub fn stack_shape(structs: &[StructType], inst: Inst) -> Shape {
    match inst {
        Inst::Const(_) | Inst::LoadLocal(_) | Inst::MakeHostEnum { .. } => Shape::on_values(0, 1),
        Inst::StoreLocal(_) | Inst::Pop => Shape::on_values(1, 0),
        Inst::SpreadArgument => Shape::on_values(2, 1),
        Inst::Dup => Shape::on_values(1, 2),
        Inst::Unary(_)
        | Inst::GetField(_)
        | Inst::GetFieldAt { .. }
        | Inst::Try
        | Inst::Snapshot => Shape::on_values(1, 1),
        // The fusion of `Inst::GetFieldAt` with `Inst::ValueToScalar`: the
        // struct it reads is the same one value in, and the field it reads
        // out lands on the other stack.
        Inst::GetFieldAtScalar(_) => Shape {
            values: (1, 0),
            scalars: (0, 1),
            places: (0, 0),
        },
        Inst::Binary(_) | Inst::SetField(_) => Shape::on_values(2, 1),
        // The typed operator is the scalar stack's: two `i64` in, one out.
        Inst::IntBinary(_) => Shape::on_scalars(2, 1),
        Inst::ScalarConst(_) | Inst::LoadScalar(_) => Shape::on_scalars(0, 1),
        Inst::StoreScalar(_)
        | Inst::ScalarPop
        | Inst::JumpIfFalseScalar(_)
        | Inst::JumpIfTrueScalar(_) => Shape::on_scalars(1, 0),
        // The two boundary instructions, and the only ones that move
        // anything between the stacks.
        Inst::ScalarToValue(_) => Shape {
            values: (0, 1),
            scalars: (1, 0),
            places: (0, 0),
        },
        Inst::ValueToScalar => Shape {
            values: (1, 0),
            scalars: (0, 1),
            places: (0, 0),
        },
        // The place stack's own three, and then the two that cross out of
        // it. A place is built and refined where it stands; reading one puts
        // a `Value` on the value stack and writing one takes a `Value` off
        // it, which is the same kind of boundary the two above are.
        Inst::PlaceLocal(_) | Inst::PlaceScalar(..) | Inst::LoadPlace(_) => Shape::on_places(0, 1),
        Inst::PlaceField(_) => Shape::on_places(1, 1),
        Inst::PlacePop => Shape::on_places(1, 0),
        Inst::PlaceRead | Inst::Freeze => Shape {
            values: (0, 1),
            scalars: (0, 0),
            places: (1, 0),
        },
        Inst::PlaceWrite => Shape {
            values: (1, 0),
            scalars: (0, 0),
            places: (1, 0),
        },
        Inst::Jump(_) => Shape::on_values(0, 0),
        Inst::JumpIfFalse(_) | Inst::JumpIfTrue(_) | Inst::Return => Shape::on_values(1, 0),
        Inst::ReturnScalar => Shape::on_scalars(1, 0),
        // A call reads each stack for the arguments that arrived on it and
        // leaves its answer on the one its callee's return type named.
        Inst::Call {
            value_argc,
            scalar_argc,
            place_argc,
            returns_scalar,
            ..
        } => Shape {
            values: (u32::from(value_argc), u32::from(!returns_scalar)),
            scalars: (u32::from(scalar_argc), u32::from(returns_scalar)),
            places: (u32::from(place_argc), 0),
        },
        Inst::CallHost { argc, .. } | Inst::MakeBuiltin { argc, .. } => Shape::on_values(argc, 1),
        // The captured values, in the order `Function::captures` names them.
        Inst::MakeClosure { captures, .. } => Shape::on_values(u32::from(captures), 1),
        // The arguments and the callee above them, and one answer back.
        // Every one of them is on the value stack: nothing at a call through
        // a value knows which function it will reach.
        Inst::CallValue { argc } => Shape::on_values(u32::from(argc) + 1, 1),
        // The receiver is the first of the arguments rather than a fourth
        // operand: it is `self`, and it becomes the callee's slot 0.
        Inst::CallDyn { argc, .. } => Shape::on_values(u32::from(argc), 1),
        // One value in and the trait object made of it out, whatever the
        // conversion turned out to reach inside.
        Inst::MakeDyn { .. } => Shape::on_values(1, 1),
        // The receiver sits below the arguments, and for a resource call the
        // receiver is the handle the call is routed by.
        Inst::CallBuiltin { argc, .. } | Inst::CallResource { argc, .. } => {
            Shape::on_values(argc + 1, 1)
        }
        Inst::MakeArray(len) => Shape::on_values(len, 1),
        // Two settled `Int` bounds off the scalar stack, and the one `Range`
        // they make onto the value stack.
        Inst::MakeRange { .. } => Shape {
            values: (0, 1),
            scalars: (2, 0),
            places: (0, 0),
        },
        Inst::Concat(parts) => Shape::on_values(parts, 1),
        // One value per field of the declared type, which the type says and
        // the construction no longer has to.
        Inst::MakeStruct(of) => Shape::on_values(
            structs
                .get(of.0 as usize)
                .map_or(0, |declared| declared.fields.len() as u32),
            1,
        ),
        // A case's payload is what it is built from, and an associated
        // function has no receiver, so both read exactly their arguments.
        Inst::MakeEnum { argc, .. } | Inst::CallBuiltinAssoc { argc, .. } => {
            Shape::on_values(argc, 1)
        }
        // Both peek: a pattern tests the subject and then binds out of it,
        // and the arm after this one needs the subject still there.
        Inst::TestCase(_) | Inst::GetPayload(_) => Shape::on_values(0, 1),
        // The iterable is read and the `Array` of what a `for` walks it as
        // stands where it stood.
        Inst::IterItems => Shape::on_values(1, 1),
        // The value no arm covered is what the message names, so it is read;
        // nothing is put back, because control does not continue.
        Inst::NoMatch => Shape::on_values(1, 0),
        // A scope is a value, so opening one leaves it standing for the
        // `store` that binds it.
        Inst::EnterScope(_) => Shape::on_values(0, 1),
        // The scope's own value in, and the `Result` the `try` below it
        // reads out.
        Inst::LeaveScope => Shape::on_values(1, 1),
        // Nothing at all: what a `break` needs is the waiting, and the scope
        // it waits for is the frame's rather than an operand.
        Inst::CancelScope => Shape::on_values(0, 0),
        // The scope and the closure to run in it, and the handle back.
        Inst::Spawn => Shape::on_values(2, 1),
        // The cell and the closure to run under its lock, and what the
        // closure answered back. The contents the closure is given do not
        // stand on the stack when the instruction begins: `Inst::Lock` puts
        // them there itself, and takes them back before it ends.
        Inst::Lock => Shape::on_values(2, 1),
        // The handle in and the value it settled to out; and the handle in
        // and the `()` a `cancel` answers out.
        Inst::Await | Inst::Cancel => Shape::on_values(1, 1),
    }
}

/// A return instruction as a diagnostic names it.
/// Whether `slot` is a slot of this frame at all, and whether it is one of
/// the region the instruction naming it is the reader of.
///
/// The one numbering is what makes this one question. A slot number is unique
/// within a frame, so `region_of` answering the wrong region *is* a scalar
/// instruction reaching a value slot — the failure the three separate bounds
/// could not see, because each number was in range of its own stack and there
/// was no number that told them apart.
fn in_region(function: &Function, slot: u32, wanted: Region) -> Result<(), String> {
    match function.region_of(slot) {
        Some(region) if region == wanted => Ok(()),
        Some(region) => Err(format!(
            "reaches slot {slot}, which this frame keeps in its {} region and not its {}",
            region_name(region),
            region_name(wanted)
        )),
        None => Err(format!(
            "reaches slot {slot} of a frame of {}",
            function.slot_count()
        )),
    }
}

fn region_name(region: Region) -> &'static str {
    match region {
        Region::Value => "value",
        Region::Scalar => "scalar",
        Region::Place => "place",
    }
}

fn render_return(inst: Inst) -> &'static str {
    match inst {
        Inst::ReturnScalar => "return-scalar",
        _ => "return",
    }
}
