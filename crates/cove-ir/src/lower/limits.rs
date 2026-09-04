//! The one compiler limit the fixed-width encoding costs.
//!
//! [ADR 0041](../../../../docs/adr/0041-a-slot-number-fits-in-sixteen-bits.md)
//! makes a slot operand sixteen bits, so a function's frame may hold at most
//! [`MAX_FRAME_WORDS`] words. A frame over that is **refused here, at compile
//! time, with a source diagnostic**. Nothing truncates and nothing wraps: a
//! slot number that did not fit would name a different slot, and every
//! guarantee the verifier makes downstream rests on it naming the one the
//! lowering meant.
//!
//! # The diagnostic names the layout, not only the number
//!
//! ADR 0041 measured why, and it is the whole reason this module is more than
//! one comparison. A frame over the limit will almost never be a function with
//! too many locals — reaching 65,536 that way takes about 65,000 lines,
//! because the lowering hands a dead run to the next value of the same shape.
//! It will almost always be **one binding whose layout is enormous**: an
//! inline value occupies as many frame words as its layout, a struct's fields
//! are inline, and [ADR 0035](../../../../docs/adr/0035-a-value-type-may-not-contain-itself.md)
//! forbids only a value type containing *itself* — so widths multiply through
//! nesting, and twelve bytes of source are the difference between a frame that
//! fits and one twice the size.
//!
//! A message that said *this function's frame is 131,072 words* and stopped
//! there would point at the wrong cause. So this names the widest locations
//! and the layouts they hold, which is the fact a reader can act on.
//!
//! # It is not the run's stack budget
//!
//! Three limits bound a frame and only this one is a compile-time question:
//!
//! | limit | bounds | when | reported as |
//! |---|---|---|---|
//! | this one | one *function*, 65,536 words | compile time | a diagnostic at the declaration |
//! | `SEGMENT_WORDS` | one *task's whole stack*, `1 << 20` words | run time | `"this call nests too deeply"` |
//! | `Limits::max_call_depth` | one task's *frame count* | run time | `RunOutcome::CallDepth` |
//!
//! The two run-time ones are the machine's and say nothing about which
//! function is at fault. This one names it.

use cove_diag::Diagnostic;

use crate::bytecode::MAX_FRAME_WORDS;
use crate::layout::LayoutId;
use crate::program::{Function, Program};

/// A function whose frame is wider than a sixteen-bit slot can name.
pub(crate) const FRAME_TOO_LARGE: &str = "cove::lower::frame_too_large";

/// How many of the widest locations a diagnostic lists.
///
/// One is usually the whole story — the construction that reaches the cap is a
/// single enormous binding — and three is enough to show a pattern without
/// turning the message into the frame.
const NAMED: usize = 3;

/// Every function of `program` whose frame is over the limit, as diagnostics.
///
/// Empty is the ordinary answer, and it is the answer for every program in
/// this repository: the largest frame the corpus lowers is 122 words.
pub(crate) fn oversized_frames(program: &Program) -> Vec<Diagnostic> {
    program
        .functions
        .iter()
        .filter(|function| function.reprs.len() > MAX_FRAME_WORDS)
        .map(|function| refuse(program, function))
        .collect()
}

fn refuse(program: &Program, function: &Function) -> Diagnostic {
    let size = function.reprs.len();
    let cause = match widest(program, function) {
        held if held.is_empty() => {
            format!("no one location in it is wider than a word, and it holds {size} of them")
        }
        held => held.join("\n  "),
    };
    Diagnostic::error(
        FRAME_TOO_LARGE,
        format!(
            "this function's frame is {size} words, and a function's frame may hold at most \
             {MAX_FRAME_WORDS}:\n  {cause}"
        ),
    )
    .at(function.span)
    .rule(
        "A slot number is sixteen bits, so one function's frame holds at most 65,536 words. \
         This is not the run's stack budget, which bounds a whole task's stack rather than \
         one call.",
    )
    .help(
        "an inline value occupies as many frame words as its layout, and a struct's fields \
         are inline — so a width doubles at every level of nesting\n\
         put the wide value on the heap, behind an `Array`, a `Shared` or a `dyn`, or nest \
         it less deeply",
    )
}

/// The widest locations the frame holds, worst first, as a reader would name
/// them.
///
/// Every location a lowered function *records* is here: the names the source
/// bound, the values a closure captured, and the answer. A temporary has no
/// name to give, so it is not among them — and the construction that reaches
/// the cap always has a named binding or an answer at its head, because a
/// value that wide had to be written down to be built.
fn widest(program: &Program, function: &Function) -> Vec<String> {
    let width = |layout: LayoutId| {
        program
            .layouts
            .get(layout.index())
            .map_or(1, |held| held.width())
    };
    let name = |layout: LayoutId| match program.layouts.get(layout.index()) {
        Some(held) => held.name.to_string(),
        None => layout.to_string(),
    };
    // A parameter is a `Local` like any other — the lowering binds it at slot
    // 0 onwards — but calling one "the local `x`" would send a reader looking
    // for a `let` that is not there.
    let taken = function.param_words(&program.layouts);
    let mut held: Vec<(u32, String)> = Vec::new();
    for local in &function.locals {
        let role = if local.slot < taken {
            "parameter"
        } else {
            "local"
        };
        held.push((
            width(local.layout),
            format!("the {role} `{}` is a `{}`", local.name, name(local.layout)),
        ));
    }
    for capture in &function.captures {
        held.push((
            width(capture.layout),
            format!(
                "the capture `{}` is a `{}`",
                capture.name,
                name(capture.layout)
            ),
        ));
    }
    held.push((
        width(function.returns),
        format!("what it answers is a `{}`", name(function.returns)),
    ));
    // Widest first, and a stable order among equals so that the message a
    // test pins does not depend on which of two identical bindings was
    // recorded first.
    held.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    held.dedup();
    held.into_iter()
        .filter(|(words, _)| *words > 1)
        .take(NAMED)
        .map(|(words, what)| format!("{what}, {words} words"))
        .collect()
}
