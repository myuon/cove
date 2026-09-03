//! How a builtin reads the words it was handed, and what it refuses when it
//! cannot.
//!
//! An operand is a layout and a run of words, because a word is untagged and
//! the location it came out of is the only thing that says what it means.
//! Reading one as an `Int`, as text, or as a receiver of a particular family
//! is therefore a question with two answers — the value, or a refusal — and
//! both halves are here so that the refusal is written once.
//!
//! # The messages are the oracle's
//!
//! Every message below that names a method is
//! [`crate::builtins`]' word for word, because a refusal is the *language's*
//! and not a backend's: the differential corpus runs the same program on both
//! and compares the text. Where a message here has no counterpart there, it
//! is about something only this representation can go wrong at — a null
//! reference, an object the collector already reclaimed, a family the program
//! does not declare — and the doc comment says so.
//!
//! Almost none of them is reachable from a checked program. `cove-sema` has
//! already settled every receiver's type, every argument's type and every
//! call's arity, so an arity or type refusal here is a lowering bug rather
//! than a program's mistake. They are written out anyway, in the oracle's
//! words, because "should never" is not "cannot" and a silent wrong answer
//! costs more than the `match` arm that reports one.

use cove_ir::{LayoutId, Repr, Shape};

use crate::error::RuntimeError;
use crate::vm::exec::Machine;

/// One operand: the layout of the value location an argument names, and the
/// words at it.
///
/// The pair travels together everywhere, because neither half means anything
/// without the other — a word is untagged, and a layout describes nothing on
/// its own.
///
/// It used to be a `Repr` and one word, and that was the shape of a call
/// rather than a choice this file made: a `CallBuiltin`'s argument list was
/// base slots, so nothing said how wide an operand was. A scalar described
/// itself from its slot and a reference from its object's header, and an
/// inline struct or enum described itself from neither — so `"{p}"` rendered
/// a `Point`'s first word, `a == b` compared it, and the six operations that
/// put a whole value into a collection refused rather than store half of one.
/// [`cove_ir::Arg`] carries the layout now and all of those read the value.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Operand<'w> {
    pub layout: LayoutId,
    pub words: &'w [u64],
}

/// One word, and the `Repr` that says what it means.
///
/// What is left of the old operand, and it is still the right currency for
/// the two things that genuinely are one word: a scalar comparison, and the
/// address a value of a family that lives in the heap consists of.
pub(super) type Word = (Repr, u64);

impl Operand<'_> {
    /// The first word of the location.
    ///
    /// Every layout a program can name is at least one word wide — `Unit`
    /// takes a slot — so the fallback is unreachable, and it is zero rather
    /// than a panic because a builtin is not a place to discover a lowering
    /// bug by unwinding.
    pub(super) fn word(self) -> u64 {
        self.words.first().copied().unwrap_or(0)
    }
}

/// The one word an operand is, where it is one.
///
/// A scalar answers its `Repr`, and so does a family that lives in the heap,
/// because a value of one *is* the address of its object. An inline struct or
/// an enum answers nothing however wide it happens to be: a
/// `struct Error { message: String }` is one `Repr::Ref` word and is not a
/// reference to an `Error`, so reading its word as one reads the declaration
/// away. That is [`cove_ir::Layout::is_one_address`], asked here.
pub(super) fn as_word(machine: &Machine, operand: Operand<'_>) -> Option<Word> {
    let described = machine.program().layout(operand.layout);
    match &described.shape {
        Shape::Word(repr) => Some((*repr, operand.word())),
        _ if described.is_one_address() => Some((Repr::Ref, operand.word())),
        _ => None,
    }
}

/// The words of `operand`, refused when it is not a value of `want`.
///
/// What every operation that puts a whole value *into* a collection asks: a
/// member of a `Set<Point>` is two words and a store written one word at a
/// time at a stride of two is a silently wrong set. The layouts are compared
/// rather than only the widths, because two families of one width are still
/// two families and a run is traced by the element layout's map.
pub(super) fn run_of<'w>(
    machine: &Machine,
    method: &str,
    want: LayoutId,
    operand: Operand<'w>,
) -> Result<&'w [u64], RuntimeError> {
    let described = machine.program().layout(want);
    if operand.layout == want && operand.words.len() == described.width() as usize {
        return Ok(operand.words);
    }
    Err(RuntimeError::new(format!(
        "`{method}` expects `{}` here, but found `{}`",
        described.name,
        machine.program().layout(operand.layout).name
    ))
    .with_rule(
        "A value is a run of words its layout describes, and a collection holds values of one family.",
    ))
}

/// The receiver and the arguments of a method call.
///
/// A method's operands are its receiver followed by its arguments, so the
/// count this holds them to is the *argument* count — which is the count the
/// schema declares and the count the oracle's message names.
pub(super) fn method<'w, 'o>(
    shown: &str,
    operands: &'o [Operand<'w>],
    arguments: usize,
) -> Result<(Operand<'w>, &'o [Operand<'w>]), RuntimeError> {
    match operands.split_first() {
        Some((receiver, rest)) if rest.len() == arguments => Ok((*receiver, rest)),
        Some((_, rest)) => Err(arity(shown, arguments, rest.len())),
        // A method call with no receiver at all is a lowering that built the
        // argument list wrongly, and there is nothing to report but that no
        // argument arrived.
        None => Err(arity(shown, arguments, 0)),
    }
}

/// The arguments of an associated function, which is called on a name rather
/// than on a value and therefore has no receiver.
pub(super) fn free<'w, 'o>(
    shown: &str,
    operands: &'o [Operand<'w>],
    arguments: usize,
) -> Result<&'o [Operand<'w>], RuntimeError> {
    if operands.len() != arguments {
        return Err(arity(shown, arguments, operands.len()));
    }
    Ok(operands)
}

/// The `Int` in `operand`.
pub(super) fn int(
    machine: &Machine,
    method: &str,
    parameter: &str,
    operand: Operand<'_>,
) -> Result<i64, RuntimeError> {
    match as_word(machine, operand) {
        Some((Repr::Int, word)) => Ok(word as i64),
        _ => Err(type_error(machine, method, parameter, "Int", operand)),
    }
}

/// The `Float` in `operand`.
pub(super) fn float(
    machine: &Machine,
    method: &str,
    parameter: &str,
    operand: Operand<'_>,
) -> Result<f64, RuntimeError> {
    match as_word(machine, operand) {
        Some((Repr::Float, word)) => Ok(f64::from_bits(word)),
        _ => Err(type_error(machine, method, parameter, "Float", operand)),
    }
}

/// The text of the `String` in `operand`.
pub(super) fn text(
    machine: &Machine,
    method: &str,
    parameter: &str,
    operand: Operand<'_>,
) -> Result<String, RuntimeError> {
    match as_word(machine, operand) {
        Some((Repr::Ref, addr)) if super::is_string(machine, addr) => {
            super::string_of(machine, addr)
        }
        _ => Err(type_error(machine, method, parameter, "String", operand)),
    }
}

/// `` `{method}` takes {expected} argument(s), but {found} were given ``.
pub(super) fn arity(method: &str, expected: usize, found: usize) -> RuntimeError {
    RuntimeError::new(format!(
        "`{method}` takes {expected} argument(s), but {found} were given"
    ))
}

/// The same, for the three builtins that have no receiver and no schema —
/// `String.text`, `concat` and `interpolate` are the machine's own, and what
/// they take is operands.
pub(super) fn operands(shown: &str, wanted: usize, given: usize) -> RuntimeError {
    RuntimeError::new(format!(
        "`{shown}` takes {wanted} operand(s), but {given} were given"
    ))
}

/// `` `{method}` expects `{expected}` for `{parameter}`, but found `{found}` ``.
pub(super) fn type_error(
    machine: &Machine,
    method: &str,
    parameter: &str,
    expected: &str,
    found: Operand<'_>,
) -> RuntimeError {
    RuntimeError::new(format!(
        "`{method}` expects `{expected}` for `{parameter}`, but found `{}`",
        name(machine, found)
    ))
}

/// `` `{type}` has no method `{method}` ``.
///
/// What a receiver of the wrong family is answered with. The oracle reaches
/// this by falling off the end of its `match` on the receiver's
/// representation; this reaches it by finding a shape the operation is not
/// for, which is the same question asked of a header instead of an `enum`.
pub(super) fn no_method(machine: &Machine, receiver: Operand<'_>, method: &str) -> RuntimeError {
    RuntimeError::new(format!(
        "`{}` has no method `{method}`",
        name(machine, receiver)
    ))
}

/// What the language calls the value in `operand`.
///
/// Asked of the layout rather than of a `Repr`, which is what the operand
/// carries now and what makes the answer right for an inline value: a
/// `Point` that a refusal used to call an `Int` — its first word — is called
/// a `Point`.
pub(super) fn name(machine: &Machine, operand: Operand<'_>) -> String {
    layout_name(machine, operand.layout, operand.word(), 0)
}

/// What the language calls the value in `word`, read as `repr`.
///
/// [`crate::value::Value::type_name`] is the oracle's copy of this, and the
/// two are written twice for the reason [`super`]'s rendering is: one reads a
/// materialised tree and one reads the heap, and neither can be had from the
/// other without building what the other exists to avoid.
///
/// A family is named by its *shape* rather than by its layout's name wherever
/// the shape decides it — an `Array` is an `Array` whatever the lowering
/// called the layout — and by the layout's name for a struct or an enum,
/// where the name is the declaration's and is the whole of what the reader
/// wants.
pub(super) fn type_name(machine: &Machine, repr: Repr, word: u64) -> String {
    match repr {
        Repr::Unit => "Unit".to_string(),
        Repr::Bool => "Bool".to_string(),
        Repr::Int => "Int".to_string(),
        Repr::Float => "Float".to_string(),
        Repr::Duration => "Duration".to_string(),
        Repr::Ref => object_name(machine, word, 0),
        // Neither is a value, so neither has a type the language names. A
        // message that reached one is reporting on this run's bookkeeping,
        // which is a lowering bug, and saying so is more use than a type.
        Repr::Addr => "a place".to_string(),
        Repr::Host => "a host resource".to_string(),
        Repr::Task => "a task".to_string(),
        Repr::Scope => "a task scope".to_string(),
    }
}

/// What the object at `addr` is called.
fn object_name(machine: &Machine, addr: u64, depth: usize) -> String {
    if addr == 0 {
        return "nothing".to_string();
    }
    if depth >= super::MAX_DEPTH {
        return "a value that nests too deeply to name".to_string();
    }
    let id = machine.object_layout(addr);
    let layout = machine.program().layout(id);
    match &layout.shape {
        // Erasure is looked through, because `Value::type_name` is asked of
        // an `erased()` value everywhere a comparison or a refusal asks it.
        // Payload word 0 is the layout of what the box holds, so the name is
        // that layout's — one lookup rather than a tag and a guess.
        Shape::Boxed => {
            let held = LayoutId(machine.payload(addr, 0) as u32);
            match machine.program().layouts.get(held.index()) {
                Some(_) => layout_name(machine, held, machine.payload(addr, 1), depth + 1),
                None => "a value of no known type".to_string(),
            }
        }
        _ => layout_name(machine, id, addr, depth),
    }
}

/// What a value location of `layout` is called, given its first word.
///
/// A family is named by its *shape* wherever the shape decides it — an
/// `Array` is an `Array` whatever the lowering called the layout — and by the
/// layout's name for a struct or an enum, where the name is the
/// declaration's and is the whole of what the reader wants.
pub(super) fn layout_name(machine: &Machine, layout: LayoutId, first: u64, depth: usize) -> String {
    let described = machine.program().layout(layout);
    match &described.shape {
        Shape::Word(repr) => type_name(machine, *repr, first),
        Shape::Str => "String".to_string(),
        Shape::Struct { .. } | Shape::Enum { .. } => described.name.to_string(),
        Shape::Elements { growable, .. } => if *growable { "Vector" } else { "Array" }.to_string(),
        Shape::Vector { .. } => "Vector".to_string(),
        Shape::Members { .. } => "Set".to_string(),
        Shape::Entries { .. } => "Map".to_string(),
        Shape::Closure { .. } => "fn".to_string(),
        // `Value::type_name`'s word for one. A cell is a handle, and what it
        // holds is reachable only under a `lock`, so the name is the handle's.
        Shape::Shared { .. } => "Shared".to_string(),
        Shape::Boxed => object_name(machine, first, depth + 1),
        Shape::Free => "nothing".to_string(),
    }
}

/// `` `Int` {operation} overflowed ``.
///
/// [`crate::interp::overflow`] and [`crate::vm::exec`]'s own copy, in the
/// one place a builtin reaches it: `Int.abs` at `Int.MIN`, and a
/// `Duration.<unit>` whose nanoseconds do not fit.
pub(super) fn overflowed(operation: &str) -> RuntimeError {
    RuntimeError::new(format!("`Int` {operation} overflowed"))
        .with_rule("Integer overflow is a broken invariant, not a wrapped result.")
}

/// `split` and `replace` both refuse an empty needle.
pub(super) fn empty_needle(method: &str, parameter: &str, help: &str) -> RuntimeError {
    RuntimeError::new(format!("`{method}` cannot use an empty `{parameter}`"))
        .with_rule(
            "An empty separator or search string would match between every character, rather than answer the question the method asks.",
        )
        .with_help(help)
}

/// `Int.parseRadix` refused a `radix` outside `2..=36`.
pub(super) fn radix(radix: i64) -> RuntimeError {
    RuntimeError::new(format!(
        "`Int.parseRadix` cannot read a number in radix `{radix}`"
    ))
    .with_rule(
        "A radix is 2 through 36, which is as many digits as the ten numerals and the twenty-six letters afford.",
    )
    .with_help("pass a `radix` between 2 and 36, such as 16 for hexadecimal")
}

/// `Float.format` refused a `digits` outside `0..=17`.
pub(super) fn format_digits(digits: i64) -> RuntimeError {
    RuntimeError::new(format!("`Float.format` cannot use `{digits}` digits")).with_rule(
        "A Float carries at most 17 significant decimal digits, so `digits` must be between 0 and 17.",
    )
}

/// A vector `freeze()` already consumed.
///
/// The oracle's `check_live`. There it is a flag on the storage every alias
/// shares; here it is the vector's own word 1, which `freeze()` clears — so
/// the answer reaches every alias for the same reason, that the two words a
/// `Vector` *is* are shared by every copy of it.
pub(super) fn frozen(method: &str) -> RuntimeError {
    RuntimeError::new(format!(
        "`{method}` was called on a vector that `freeze()` already consumed"
    ))
    .with_rule("`freeze()` consumes its vector; the source vector is no longer usable.")
    .with_help("use the `Array` that `freeze()` returned, or build a new vector")
}

/// A reference slot that was read before anything was written to it.
///
/// Not the oracle's: a `Value` is never absent, and a null `Repr::Ref` is
/// this representation's own way of being so. [`crate::vm::exec`] answers a
/// null object in these words and this is the same event.
pub(super) fn null_value() -> RuntimeError {
    RuntimeError::new("this value was read before it was given one")
}

/// A reference into a run of words the sweeper reclaimed.
pub(super) fn reclaimed() -> RuntimeError {
    RuntimeError::new("this value was read after it was reclaimed")
}

/// A builtin has to build a value of a family this program does not declare.
///
/// [`crate::vm::boundary`] refuses in these words for the same reason: a
/// layout table describes the families a program *uses*, and a program that
/// never mentions an `Option<Int>` has no layout for one. Unlike the
/// boundary's, this one is unreachable from a checked program — the operation
/// whose result it is was type-checked, so the lowering interned the layout —
/// which makes it a lowering bug rather than a host's mistake.
pub(super) fn unknown_family(name: &str) -> RuntimeError {
    RuntimeError::new(format!(
        "this program describes no `{name}` for a value of that shape to be built as"
    ))
    .with_rule(
        "A layout describes a family of values, and a program declares the families it uses.",
    )
}
