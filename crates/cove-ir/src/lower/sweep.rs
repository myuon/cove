//! Standing down a function the finished program stopped naming.
//!
//! The slice is decided before any optimisation runs. [`lower_roots`](super::lower_roots)
//! seeds from the checker's call graph and closes the slice against *what the
//! lowering emits references to* — its own "What `reachable` is, and how this
//! is sure of it" says why the graph alone is not enough — and that fixed
//! point is reached while the program is still being written.
//!
//! `super::inline` then runs over the finished program and **removes
//! references**. A call to a small leaf becomes the leaf's instructions, and
//! when every call site of a function is expanded, nothing names the function
//! any more. Nothing recomputed reachability afterwards, so it stayed in the
//! program: emitted, encoded, and compiled by the native tier. Measured twice,
//! on `examples/covefmt` — `std.string.length` (#438) was emitted with zero
//! calls and no profile row, and `std.string.endsWith` (#439) cost 1,856 bytes
//! of machine code that could not be reached (#440).
//!
//! So this is [`lower_roots`](super::lower_roots)' question asked a second time, after the
//! pass that can answer it differently. Not a rule about which functions are
//! allowed to exist: `std.string.endsWith` is nineteen instructions, past the
//! cold limit `inline::LIMIT`, so `fn cold(a: String, b: String) -> Bool {
//! a.endsWith(b) }` keeps its call and keeps the function. The same function
//! is genuinely needed in one program and genuinely dead in another, and that
//! is a property of the lowered program rather than of the library.
//!
//! # What names a function
//!
//! Four things, and a sweep that knows only about [`Inst::Call`] deletes a
//! live body. A function reached only as a *value* — a callback handed to
//! `map`, a conformance a `dyn` dispatch picks, a `Snapshot` implementation
//! nothing writes a call to — contributes no call edge at all, which is the
//! very reason `lower_roots` does not trust the call graph.
//!
//! - [`Inst::Call`] and [`Inst::FuncRef`], the two instructions that name one.
//!   [`Inst::callee`](Inst::callee) answers both through a match with no wildcard arm, so an
//!   instruction added later does not compile until it is listed there.
//! - `Shape::Closure`'s `function`, in [`Program::layouts`]. A closure object
//!   carries its callee twice — the word `FuncRef` writes and the typed fact
//!   in the layout — and [`mod@crate::verify`] checks the two agree. It is
//!   **redundant today**, and counted anyway: `lower::closures`' `close_over`
//!   is the only place a closure layout is made and it emits the `FuncRef`
//!   three instructions later, so removing this line fails no test. It is here
//!   because a second, independently verified copy of the fact is not a copy a
//!   sweep may decide to stop reading, and because it is counted whether or
//!   not the layout is reachable from live code — the layout table is interned
//!   and never pruned, so this can keep a lambda alive that nothing runs. A
//!   function swept less often than it could be is the direction to be wrong
//!   in.
//! - The roots, which are kept whatever names them. See below.
//!
//! And one thing that deliberately does **not** count: [`Inlined::callee`](crate::Inlined::callee),
//! the record an expansion writes of whose instructions it copied. Counting it
//! would sweep nothing at all, because the function this is about is exactly
//! the one every expansion records. It is not a reference to code — nothing is
//! executed *through* the record — it is a name, read by an error's chain, a
//! backtrace and a debugger, and the stand-in below still answers it.
//!
//! # Why a stand-in rather than a smaller table
//!
//! A [`FunctionId`] is dense: it is a position in [`Program::functions`], so
//! removing an entry renumbers every id after it — in two instructions, in a
//! closure layout, in every expansion record, and in [`Program::by_name`].
//! Compacting is possible and it is not what this does, for a reason that is
//! specific rather than squeamish: **the expansion records name the functions
//! being swept**. `Inlined::callee` is a `FunctionId` and a swept function has
//! no compacted id to rewrite it to, so compaction would mean giving `Inlined`
//! a different way to name a callee and changing every reader of it — the
//! verifier, the debugger's frame names, the VM's backtrace, the differential
//! suite's "was this expanded" probe — to buy a shorter vector.
//!
//! So the id stays where it is and the *body* goes. What is left is a stub,
//! the same stand-in `lower::stub` leaves for a declaration the slice never
//! reached, and every stage downstream already understands one: it is not
//! counted in the emitted IR (`vm::report::Emitted` skips it), the native tier
//! declines to compile it and counts it apart ("N further declaration(s) are
//! stubs no path in this slice reaches"), and the debugger skips it rather
//! than resolving a breakpoint into it. Nothing new had to learn anything.
//!
//! # Roots, and why not every name is one
//!
//! A root is kept whatever its reference count: it is what the run is about,
//! and nothing inside the program calls it. The roots are the ones the caller
//! named, resolved the way the machine resolves an entry —
//! [`Program::function_named`] — so what the sweep protects is exactly what
//! `Vm::run_entry` can reach.
//!
//! [`Program::by_name`] is *not* the root set, and that is the whole reason
//! this pass can do anything: it holds every reached non-generic declaration,
//! `std.string.endsWith` among them. Treating a nameable function as a root
//! would protect the entire standard library.
//!
//! A whole-package lowering sweeps nothing. [`lower`](super::lower) means everything
//! the package declares is part of the program — that is what a listing, the
//! corpus survey and an embedding that invokes several functions through one
//! `Vm` all ask for — so there every declaration is a root and there is
//! nothing here to find.
//!
//! # A fixed point, and the one thing it cannot collect
//!
//! Standing a function down removes the references *its* body held, which may
//! be the last one to something else, so the question is asked again until a
//! round finds nothing. It terminates because a round either stands a function
//! down or stops, and a function is stood down once.
//!
//! This counts references rather than marking from the roots, and the
//! difference is a cycle: two functions that name each other and that no root
//! reaches survive. Marking would collect them and would also mean that a root
//! this pass failed to resolve deletes a live program, which is the failure
//! this would rather not have. A cycle unreachable from a root is a function
//! the slice should not have lowered in the first place, and the slice is
//! closed against the call graph, so there is nothing here that is known to
//! produce one.

use std::collections::HashSet;

use crate::inst::Inst;
use crate::layout::Shape;
use crate::program::{Function, FunctionId, Program};
use crate::repr::{RefMap, Repr};

use super::shapes;

/// Replaces every function nothing names with a stub, to a fixed point.
///
/// `roots` are kept whatever names them.
pub(super) fn stand_down_unreferenced(program: &mut Program, roots: &HashSet<FunctionId>) {
    loop {
        let named = named_anywhere(program, roots);
        let dead: Vec<usize> = (0..program.functions.len())
            .filter(|at| !named[*at] && !program.functions[*at].stub)
            .collect();
        if dead.is_empty() {
            return;
        }
        for at in dead {
            stand_down(&mut program.functions[at]);
        }
    }
}

/// Which functions something in the program names, one flag per id.
///
/// Every way a [`FunctionId`] is written down but the two this module's
/// header explains: the roots are seeded true, and an expansion record is not
/// read at all.
fn named_anywhere(program: &Program, roots: &HashSet<FunctionId>) -> Vec<bool> {
    let mut named = vec![false; program.functions.len()];
    let mut mark = |id: FunctionId| {
        if let Some(seen) = named.get_mut(id.index()) {
            *seen = true;
        }
    };
    for id in roots {
        mark(*id);
    }
    for layout in &program.layouts {
        if let Shape::Closure { function, .. } = layout.shape {
            mark(function);
        }
    }
    for function in &program.functions {
        for inst in &function.code {
            if let Some(callee) = inst.callee() {
                mark(callee);
            }
        }
    }
    named
}

/// Replaces a body with the stand-in, keeping the identity a name is read
/// from.
///
/// The shape is `lower::stub`'s, field for field, and it is written out
/// again here rather than shared because the two are built from different
/// things: `stub` has the `Decl` the lowering planned and this has the
/// `Function` the lowering produced. What they must agree on is the shape,
/// and [`crate::Function::is_stub`] documents what that shape is for.
///
/// The module, the name and the declaration's span stay, because they are
/// what an expansion record resolves to: a debugger's frame name and an
/// error's chain read `program.function(inlined.callee).qualified()`, and a
/// stood-down function is precisely the one an expansion is most likely to
/// name.
fn stand_down(function: &mut Function) {
    let reprs = vec![Repr::Unit];
    *function = Function {
        module: function.module.clone(),
        name: function.name.clone(),
        params: Vec::new(),
        refs: RefMap::of(&reprs),
        reprs,
        returns: shapes::UNIT,
        captures: Vec::new(),
        code: vec![Inst::Return { src: 0 }],
        spans: vec![function.span],
        locals: Vec::new(),
        inlined: Vec::new(),
        span: function.span,
        is_async: false,
        stub: true,
    };
}
