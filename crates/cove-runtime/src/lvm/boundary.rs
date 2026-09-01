//! Where a word becomes a public [`Value`], and back.
//!
//! This is the only place in the linear-memory backend that knows what a
//! `Value` is. [ADR 0034](../../../../docs/adr/0034-one-physical-word-stack.md)
//! keeps the materialised `Value` as the host and oracle boundary
//! representation and forbids it everywhere else: not as an operand store,
//! not as a call buffer, not as a spill area, not as a fallback path. Keeping
//! the conversion in a file of its own is how that stays checkable — if
//! anything else in `lvm` ever needs `Value`, it has to say so by importing
//! it, and the import is the thing to argue with.
//!
//! Three things cross here and nothing else does: a Host call's arguments and
//! answer, an entry's arguments and answer, and a trace capture.
//!
//! # A conversion needs the `Repr`
//!
//! A word is untagged, so nothing about `7` says whether it is an `Int`, a
//! `Duration` of seven nanoseconds, or a `Bool` that got there by a lowering
//! bug. What says so is the static metadata the word came from: a slot's
//! `Repr`, a host operation's declared result, a layout's field. The
//! conversion therefore takes the `Repr` as an argument rather than
//! inspecting the word, which is the same discipline the collector follows
//! and for the same reason.

use cove_lir::{Repr, Shape};

use crate::error::RuntimeError;
use crate::lvm::exec::Machine;
use crate::value::Value;

/// The public value of `word`, read as `repr`.
pub(crate) fn to_value(machine: &Machine, repr: Repr, word: u64) -> Result<Value, RuntimeError> {
    Ok(match repr {
        Repr::Unit => Value::unit(),
        Repr::Bool => Value::bool(word != 0),
        Repr::Int => Value::int(word as i64),
        Repr::Float => Value::float(f64::from_bits(word)),
        Repr::Duration => Value::duration(word as i64),
        Repr::Ref => return object_to_value(machine, word),
        // Neither can leave the machine. An address names a word of this
        // run's memory and means nothing outside it, and a host handle is
        // the host's to hand out — a boundary that received one back would
        // be receiving its own bookkeeping.
        Repr::Addr | Repr::Host => {
            return Err(RuntimeError::new(
                "this value cannot cross the boundary as it is represented",
            ))
        }
    })
}

/// The public value of the object at `addr`.
fn object_to_value(machine: &Machine, addr: u64) -> Result<Value, RuntimeError> {
    if addr == 0 {
        return Err(RuntimeError::new(
            "this value was read before it was given one",
        ));
    }
    let layout = machine.program().layout(machine.object_layout(addr));
    match &layout.shape {
        Shape::Str => {
            let bytes = machine.string_bytes(addr);
            let text = String::from_utf8(bytes).map_err(|_| {
                // Every string in the heap was written from a `&str` or from
                // a host's `String`, so this cannot happen without a bug in
                // whatever wrote it. Reporting it is cheaper than the
                // alternative, which is a `Value` holding invalid UTF-8.
                RuntimeError::new("this string's bytes are not valid UTF-8")
            })?;
            Ok(Value::string(text))
        }
        other => Err(RuntimeError::new(format!(
            "a `{}` cannot cross the boundary yet ({other:?})",
            layout.name
        ))),
    }
}

/// The word `value` occupies in a slot of `repr`.
pub(crate) fn from_value(
    machine: &mut Machine,
    repr: Repr,
    value: &Value,
) -> Result<u64, RuntimeError> {
    let mismatch = || {
        RuntimeError::new(format!(
            "this value is not the `{}` that was expected here",
            repr.name()
        ))
    };
    match repr {
        Repr::Unit if value.is_unit() => Ok(0),
        Repr::Bool => value.as_bool().map(|b| b as u64).ok_or_else(mismatch),
        Repr::Int => value.as_int().map(|n| n as u64).ok_or_else(mismatch),
        Repr::Float => value.as_float().map(|x| x.to_bits()).ok_or_else(mismatch),
        Repr::Duration => value
            .as_duration_nanos()
            .map(|n| n as u64)
            .ok_or_else(mismatch),
        Repr::Ref => match value.as_str() {
            Some(text) => {
                let text = text.to_string();
                machine.new_string(&text)
            }
            None => Err(mismatch()),
        },
        _ => Err(mismatch()),
    }
}
