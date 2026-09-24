//! The operations the language has but the instruction set does not.
//!
//! A builtin is a method of a type the language ships — `String`, `Array`,
//! `Int` — that is too large or too specific to be an [`Inst`](cove_ir::Inst)
//! and too fixed to be a Host call. [`cove_ir::IntrinsicSite`] names one by a
//! closed [`cove_ir::Intrinsic`] rather than by a pair of strings matched at
//! run time — see [ADR
//! 0058](../../../../docs/adr/0058-collection-apis-lower-through-typed-run-intrinsics.md)
//! — so the match below is exhaustive: teaching the machine one more
//! operation is adding a variant to `cove_ir::intrinsic` and an arm here,
//! and there is no longer a name this backend has not been taught to fall
//! through to.
//!
//! # A builtin is not a boundary
//!
//! It reads the words and the heap objects the machine already holds and
//! answers the words of a value location. Nothing here materialises a public `Value`, and this
//! file does not import one — which is the same check `boundary` makes of the
//! rest of the backend, made of this file. ADR 0034 puts `Value` at the Host
//! boundary and nowhere else, and a builtin is not at it: `"n is {n}"` is
//! Cove talking to itself.
//!
//! # Rendering is the language's, and it is written twice
//!
//! What `{n}` puts in a string is a rule of the language, and the oracle's
//! copy of it is `Display for Value` in [`crate::value`]. That one reads a
//! materialised tree; this one reads the heap. They cannot share an
//! implementation without one of them building what the other exists to
//! avoid, so the rule is written down twice and the differential corpus is
//! what keeps the two copies saying the same thing — the same arrangement
//! [`crate::vm::exec`]'s arithmetic messages are under, and for the same
//! reason.
//!
//! Two of the rules are facts about a *declaration* rather than about a
//! family, and the layout table carries each of them for that reason: an
//! `export opaque struct` renders as its bare name, and a builtin `Error`
//! renders as its message. Neither can be derived here, because by the time a
//! value is a word the declaration is gone.
//!
//! **Since ADR 0068's Phase 4b-ii no rendering a program asks for runs here.**
//! A value whose layout the lowering knows is a walk it composes, and a box is
//! `std.dynamic.renderInto`, a Cove walk over a view of it; `Value.renderInto`
//! is gone. [`render_value`] is left, private, for one reader: `key`'s wording
//! of a refused key, whose path through a map quotes the entry's key as it
//! renders — until that wording moves too (ADR 0068's Phase 4c).

use std::cell::Cell;
use std::fmt::Write as _;

use cove_ir::{Intrinsic, LayoutId, Repr, Shape};
use cove_schema::builtins::{ERROR, MESSAGE_FIELD};

use crate::vm::boundary::{declared, is_range, short};
use crate::vm::intrinsics::operand::{Dest, Frame};

use crate::error::RuntimeError;
use crate::vm::exec::Machine;

mod equal;
mod key;
#[cfg(test)]
pub(crate) use key::is_ascending_and_distinct;
mod make;
pub(crate) mod operand;
mod scalar;
mod seq;
mod text;

/// How deep the recursive walks still in this module may nest: [`equal`]'s,
/// which no program reaches, and [`operand`]'s naming of a value.
///
/// For the reason [`crate::vm::boundary`]'s limit exists: an object graph
/// can hold itself and a walk that met one would recurse until the native
/// stack ran out. The rendering was bounded by this too until ADR 0068's
/// Phase 4b-i made it a loop over a stack with the vectors it is inside on a
/// path, which is what issue #480's "no nesting bound" and issue #493's cycle
/// rule ask of it.
const MAX_DEPTH: usize = 128;

/// Runs `intrinsic` over the operands `frame` names, writing its answer into
/// `dest`.
///
/// Each operand is a value location in the caller's frame: the layout the
/// call's argument names and the words at its slot. A word is untagged, so
/// the pair is the whole of what a builtin has to work from — and it is read
/// where it is, rather than copied into a buffer first, and the answer is
/// written where it goes, rather than carried home in another (#378, P5-4).
/// See [`Frame`] for the one rule that makes that sound: every operand is
/// read before the answer is written.
pub(crate) fn call(
    machine: &mut Machine,
    intrinsic: Intrinsic,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    // One match over the intrinsic the IR names, so that teaching the
    // machine an operation is adding a variant to `cove_ir::intrinsic` and
    // an arm here. The match is exhaustive: there is no pair left to fall
    // through on, because `Intrinsic` is the closed set this backend has
    // been taught.
    match intrinsic {
        // ---- rendering ---------------------------------------------------
        //
        // `Value.renderInto` stood here, what `"{x}"` appended for a piece
        // whose layout did not say what it was, until ADR 0068's Phase 4b-ii
        // made it `std.dynamic.renderInto`, a Cove walk over a view of the
        // box. Every other piece was already a walk the lowering composes. The
        // walk below it, `render_value`, is left for one reader: `key`'s
        // wording of a refused key, which quotes a map key as it renders.

        // ---- Array -------------------------------------------------------
        //
        // Every arm below is one line, so that the whole set of operations
        // this backend has been taught reads as a table.
        //
        // A builtin's answer is a value location like any other, so what an
        // arm produces is a *run of words* at the destination, written there
        // through `Dest`. It used to be a `Vec` per call — an allocation, and
        // 39 ns of an 86 ns call — then a buffer the machine reused and copied
        // into the frame afterwards; now it is neither. What each one means is
        // in the module it delegates to, beside the reading of the oracle it
        // follows.
        //
        // `get` and `length` are not here, for `Array` or `Vector`: the
        // lowering has always answered both with instructions, and neither
        // had a caller.
        // `Array.isEmpty` is not here: it is `std.array.isEmpty`, the first
        // builtin method whose body is Cove rather than a machine builtin —
        // see `cove_schema::builtins::standard_binding` and
        // `cove_ir::lower::methods::Body::call_std_binding`.
        // `contains` and `indexOf` are not here: each is `std.array`, a loop
        // over `==` in Cove.
        // `slice` and `toVector` are not here: they are `std.array` over a
        // word `Inst::RunSlice` and a word `Inst::RunCopy`.

        // ---- Vector ------------------------------------------------------
        // `Vector.of` is not here: the lowering allocates it — see
        // `cove_ir::lower::collections`' `vector_of`.
        // `Vector.push` is not here: it is `std.vector.push`, an ensure, a
        // `store-elem` and a commit in Cove — see `cove_ir::legalize`.
        // `Vector.set` is not here: it is `std.vector.set`, a range check and
        // an `Option` in Cove over an element `LoadElem` and `StoreElem`.
        // `Vector.pop` and `Vector.remove` are not here: they are `std.vector`
        // over an element load, a word `Inst::RunCopy` of the tail and a word
        // `Inst::GrowableTruncate` — see `Machine::truncate_words`.
        // `Vector.contains` and `Vector.indexOf` are not here: each is
        // `std.vector`, a loop over `==` in Cove through `core.vectorLoad`.
        // `Vector.isEmpty` is not here: it is `std.vector.isEmpty` — see
        // `cove_schema::builtins::standard_binding`.
        // `Vector.freeze` is not here: it is `std.vector.freeze` over the core
        // intrinsic that is a word `Inst::RunFinish` — see
        // `Machine::finish_words`. `slice` and `toArray` are `std.vector` over
        // a word `Inst::RunSlice`.

        // ---- Set and Map -------------------------------------------------
        //
        // Nothing. Every public operation of both — the literals, the
        // searches, the updates and the projections — is `std.set` or
        // `std.map` over the three `Value` intrinsics at the end, run copies
        // and slices, and a keyed finish (ADR 0059, #378 Phase 4).

        // ---- String ------------------------------------------------------
        // Neither `String.length` nor `String.isEmpty` is here. Both are
        // `std.string`'s — `isEmpty` since ADR 0058 and `length` since ADR
        // 0064, a Cove loop over `core.byteLength` and one byte load a
        // character — and `cove_schema::builtins::standard_binding` resolves
        // each before lowering ever looks for an intrinsic.
        // `String.words` and `String.trim` are not here either, and issue
        // #454's Step 5 moved them together for a reason that turned out to be
        // the opposite of a shared substrate: `trim` removes Unicode's
        // twenty-five `White_Space` code points and `words` splits on five
        // ASCII bytes, and `U+000B` is in one set and not the other. Each is a
        // `std.string` body over `core.byteLength`, `byteAt` and one
        // `core.stringSlice` a part — and `trim`'s table is written out in
        // Cove with its Unicode version stated, which is ADR 0064's Decision 5
        // and the thing `str::trim` was quietly inheriting from the toolchain.
        // `String.split` followed them at the end of Step 3; see `replace` below.
        // None of the four searches and predicates ADR 0046 measured together
        // is here any more, and they did not all leave the same way.
        // `String.startsWith` and `String.endsWith` became Cove loops over
        // `core.byteLength` and `byteAt` (ADR 0064), one comparing the
        // receiver's first bytes against the prefix's and one its last against
        // the suffix's — a bounded comparison needs nothing underneath it.
        // Neither `String.contains` nor `String.indexOf` could be written that
        // way for nothing: their work is proportional to a haystack the caller
        // did not size, and a Cove scan pays a VM dispatch per byte of one. So
        // ADR 0065 added `Inst::RunFind`, a bounded search over a run of
        // packed bytes, and each is a `std.string` body over it —
        // `contains` a comparison of its answer against -1, `indexOf` a walk
        // of the prefix's lead bytes that turns the byte offset it found into
        // the character position the method promises.
        // `String.split` and `String.replace` finished Step 3 and are not here
        // either: each is a `std.string` body over the same `core.stringFind`,
        // and each raises on an empty needle through ADR 0067's
        // `core.refuse`, which is the one thing that had kept them below.
        // `String.toUpper` and `String.toLower` finished issue #454's Step 5
        // and are not here either. They are the pair that needed the thing
        // `trim` did not: a **generated table**, because the full case
        // mappings are 1,580 code points one way and 1,488 the other, 102 of
        // them one-to-many. `crates/cove-sema/tests/unicase.rs` generates all
        // 10,969 bytes of it into six `std.string` string literals and asserts
        // they are byte for byte what is checked in, so a toolchain whose
        // Unicode tables move is a failing test rather than a silent change to
        // what every Cove program means. Each body binary-searches a literal
        // ADR 0045 placed before the run's first instruction.
        //
        // `toLower` is the only operation this file ever dispatched whose
        // answer depends on a character's neighbours — `Σ` is `ς` at a word's
        // end and `σ` elsewhere — and the two extra tables that rule needs are
        // 40% of the asset.
        // Not a method of `String` a program can call: the refusal
        // `std.stringbuilder`'s `appendRange` reaches once its own range check
        // has failed. It never answers, so there is nothing to write into
        // `dest`.
        Intrinsic::StringRefuseByteRange => text::refuse_byte_range(machine, frame),
        // No byte-counted operation is here any more. All three are
        // `std.string`: `byteLength` over the core intrinsic that is an
        // `Inst::Len`, `sliceBytes` over the one that is a byte
        // `Inst::RunSlice`, and `codePointAtByte` a UTF-8 decode in Cove over
        // `byteAt`, which is a byte `Inst::RunLoad`.

        // ---- Int ---------------------------------------------------------
        // `Int.toFloat` is not here: it is an `Inst::Convert`, as both halves
        // of `Duration.nanos` are (#378, P5-2).
        // `Int.min`, `Int.max`, and `Int.abs` are not here: they are
        // `std.int.min`, `std.int.max`, and `std.int.abs` — see
        // `cove_schema::builtins::standard_binding`.
        // `Int.parse` is not here either, and it is the first of this
        // receiver's *associated* functions to go: `std.int.parse` reads the
        // receiver's bytes with `core.byteLength` and one `byteAt` a byte,
        // into an accumulator that runs negative because Cove traps on
        // overflow and the least `Int` has a magnitude the greatest has not.
        // `parseRadix` followed it once ADR 0067 gave a Cove body
        // `core.refuse` to raise with, which is what it does for a radix
        // outside `2..=36`: it is `std.int.parseRadix`, the same loop with the
        // radix where the ten was. `Int` has no arm here now.

        // ---- Float -------------------------------------------------------
        Intrinsic::FloatToInt => scalar::float_to_int(machine, frame, dest),
        // `Float.format` is `std.float.format`, exact decimal in Cove.
        Intrinsic::FloatParse => scalar::float_parse(machine, frame, dest),

        // `Bool` has no operations: the schema gives it none beyond
        // `snapshot`, and `!`, `&&` and `||` are instructions rather than
        // builtins.

        // ---- equality ----------------------------------------------------
        //
        // There is no equality arm. `==` on a value whose layout is known is
        // a walk the lowering synthesizes (ADR 0064's Decision 3), and `==` on
        // two erased values is `std.dynamic.equals`, a Cove loop over a view
        // of each box (ADR 0068's Phase 2) — so `Any.equals`, the Rust walk
        // that answered for a box, has no caller and no variant.

        // ---- keys --------------------------------------------------------
        //
        // One of ADR 0059's three: the admission a keyed search in the
        // standard library asks of a key before anything is compared, which
        // answers nothing — the zero word. The order that search steps by
        // stood beside it until ADR 0068's Phase 3 made the order of two
        // erased keys `std.dynamic.order`, a Cove loop over a view of each
        // box; a key whose layout is known was already a walk the lowering
        // writes. The third, the refusal of a literal with a key twice, is
        // `std.set.of`'s and `std.map.of`'s own Cove since ADR 0067.
        Intrinsic::ValueAdmitKey => key::admit_key(machine, frame, dest),
    }
}

/// What a vector the rendering is already inside renders as.
///
/// Issue #499's decision 2, and `Display for Value`'s marker for the same
/// value: the brackets a vector always has, around the ellipsis that says
/// what is inside them has been shown already, further out.
const REPEAT: &str = "[…]";

/// The text of `word`, read as `repr`, appended to `out` — or, for a
/// reference, the object it names pushed onto `steps` to be rendered next.
///
/// The width-one case of [`render_value`], and what every walk below reaches
/// when it gets down to one word of scalar bits or one address.
fn render(
    machine: &Machine,
    repr: Repr,
    word: u64,
    walk: &mut Walk,
    out: &mut String,
) -> Result<(), RuntimeError> {
    match repr {
        Repr::Unit => out.push_str("()"),
        Repr::Bool => out.push_str(if word != 0 { "true" } else { "false" }),
        Repr::Int => write!(out, "{}", word as i64).expect("a string never fails to be written to"),
        Repr::Float => float(out, f64::from_bits(word)),
        Repr::Duration => duration(out, word as i64),
        Repr::Ref => return render_object(machine, word, walk, out),
        Repr::Host | Repr::Scope | Repr::Task => handle_text(machine, repr, word, out)?,
        // An address is a place and not a value; interpolating one would be
        // putting this run's bookkeeping into a string a program prints.
        //
        // A tag is not a value either, for a reason of its own: it is word 0
        // of an enum and an enum renders whole, through its layout, as the
        // case it holds. A tag reaching here alone is a lowering bug.
        Repr::Addr | Repr::Tag => {
            return Err(RuntimeError::new("this value has no text of its own"))
        }
    }
    Ok(())
}

/// The text of the handle `word`, read as `repr`, appended to `out` — what
/// `Inst::HandleText` answers as a new `String`, and what this rendering
/// writes for a handle inside a box.
///
/// Written once for the two, so that a walk the lowering composed for a known
/// layout and this rendering of an erased one cannot come to say different
/// things of one handle.
pub(crate) fn handle_text(
    machine: &Machine,
    repr: Repr,
    word: u64,
    out: &mut String,
) -> Result<(), RuntimeError> {
    match repr {
        // A handle shows as what it names, identity included — `Display for
        // Value`'s `<{handle}>`, which is `<{module}.{Type}#{n}>`: two
        // connections are told apart by the number the host issued and by
        // nothing else. The module, the type and the number are the run's
        // resource table's, which is why a walk the lowering composed cannot
        // place this as a literal and asks `Inst::HandleText` for it (issue
        // #499).
        Repr::Host => {
            let handle = machine
                .resource(word)
                .ok_or_else(crate::vm::boundary::no_such_resource)?;
            write!(out, "<{handle}>").expect("a string never fails to be written to");
        }
        // A scope shows the name it is bound to, which is the scheduler's
        // entry for it rather than anything in the layout.
        Repr::Scope => {
            let name = machine.scope_name(word)?;
            write!(out, "<task scope {name}>").expect("a string never fails to be written to");
        }
        // A task shows as the handle it is, never as the value it will
        // produce: that value is observable only through `await` or the scope
        // settling it. A walk writes this one itself, as a literal; it is here
        // for a task inside a box.
        Repr::Task => out.push_str("<task>"),
        other => {
            return Err(RuntimeError::new(format!(
                "internal error: a `{}` word is not a handle with a text",
                other.name()
            )))
        }
    }
    Ok(())
}

/// One piece of work a rendering has still to do.
///
/// The walk is a loop over a stack of these rather than a recursion, so a
/// value nests as deep as it likes and the host's stack is not what stops it
/// — issue #480 decided that the language has no nesting bound, and this was
/// the last renderer with one (`MAX_DEPTH`, 128 steps, which a boxed value
/// nested 64 levels deep ran out of). The work a step stands for is the work
/// the recursive walk did at that point, in the same order, so the text is
/// the same byte for byte.
///
/// A run, a struct's fields and an enum's parts are one step each that comes
/// back for its next part, rather than one step per part pushed at once: what
/// the stack holds is proportional to how deep the value is, not how wide.
///
/// A step that reads a value location names its words by where they start in
/// [`Walk::words`], which is a stack too: a step's words are on top of it
/// whenever the step is taken, and are taken off when it is done. So a walk
/// allocates its two stacks and nothing per value.
enum Step {
    /// A value location of `layout`: its `width` words, from `from`.
    Value {
        layout: LayoutId,
        from: usize,
        width: usize,
    },
    /// A closing bracket, or the colon between a key and its value.
    Text(&'static str),
    /// A struct's fields from the `next`th; its words are from `from`.
    Fields {
        layout: LayoutId,
        from: usize,
        next: usize,
    },
    /// The parts of the case an enum holds, from the `next`th; its words are
    /// from `from`.
    Parts {
        layout: LayoutId,
        case: usize,
        from: usize,
        next: usize,
    },
    /// The units of a run of elements or members at `addr`, from the
    /// `next`th, each preceded by `, ` but the first.
    Run {
        addr: u64,
        elem: LayoutId,
        len: u32,
        next: u32,
    },
    /// A map's entries, `key: value` apiece, from the `next`th.
    Entries {
        addr: u64,
        key: LayoutId,
        value: LayoutId,
        len: u32,
        next: u32,
    },
    /// The end of a vector: its closing bracket, and the vector off the path.
    Leave,
}

/// A rendering in progress: what is left to do, the words it is done over,
/// and the vectors it is inside.
struct Walk {
    steps: Vec<Step>,
    /// The words of every value location a step on [`Walk::steps`] reads,
    /// in the order the steps were pushed.
    words: Vec<u64>,
    /// The addresses of the non-empty vectors whose elements are being
    /// rendered, outermost first — issue #493's current path, for the
    /// rendering.
    ///
    /// An address is pushed when a vector's elements begin and popped by its
    /// [`Step::Leave`], when they end, so a vector met twice by two routes —
    /// a shared DAG — is on it only while one of them is being rendered, and
    /// renders in full both times. A vector met again **while** it is on it
    /// is one the value holds inside itself, and renders as [`REPEAT`].
    ///
    /// **The path starts empty**, and since ADR 0068's Phase 4b-ii that is
    /// the whole of what this walk is asked: its one caller is `key`'s wording
    /// of a refused key, which renders a map key, and a key holds no vector.
    /// The rendering of a box a program asks for is `std.dynamic.renderInto`,
    /// which continues the static walk's path through the box, as issue #499's
    /// decision 3 has it.
    inside: Vec<u64>,
}

impl Walk {
    /// Pushes a step over the `width` words of the object at `addr` from
    /// payload word `at`, copied onto [`Walk::words`].
    fn object(&mut self, machine: &Machine, layout: LayoutId, addr: u64, at: u32, width: u32) {
        let from = self.words.len();
        self.words
            .extend((0..width).map(|offset| machine.payload(addr, at + offset)));
        self.steps.push(Step::Value {
            layout,
            from,
            width: width as usize,
        });
    }

    /// Pushes a step over `width` words that are already on
    /// [`Walk::words`] from `at`, copied onto its top.
    fn part(&mut self, layout: LayoutId, at: usize, width: usize) {
        let from = self.words.len();
        self.words.extend_from_within(at..at + width);
        self.steps.push(Step::Value {
            layout,
            from,
            width,
        });
    }
}

/// The text of the value location of `layout` holding `words`, appended to
/// `out`.
///
/// A struct is its fields in place and an enum is a discriminant and a
/// payload region, so rendering one is reading runs of words rather than
/// following an address per field. This is the same walk
/// [`crate::vm::boundary`] makes, and it is written twice for the reason the
/// module docs give.
///
/// It reports one unit per value it visits, which is [ADR 0064]'s Decision 7
/// asked of a walk over a value: every field, element, member and entry-half
/// arrives as a [`Step::Value`], and each is reported once, where it is taken
/// off the stack.
///
/// **Its one caller is `key`'s wording of a refused key** since ADR 0068's
/// Phase 4b-ii deleted `Value.renderInto`: a map key quoted in a refusal's path
/// is rendered here, on the path that ends the run. It is private to this
/// module and its children for that reason, and goes when that wording moves
/// into Cove (Phase 4c).
///
/// [ADR 0064]: ../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md
fn render_value(
    machine: &Machine,
    layout: LayoutId,
    words: &[u64],
    out: &mut String,
) -> Result<(), RuntimeError> {
    let mut walk = SPARE.with(Cell::take).unwrap_or_else(|| Walk {
        steps: Vec::new(),
        words: Vec::new(),
        inside: Vec::new(),
    });
    let answer = walk_value(machine, layout, words, &mut walk, out);
    walk.steps.clear();
    walk.words.clear();
    walk.inside.clear();
    walk.steps.shrink_to(SPARE_ROOM);
    walk.words.shrink_to(SPARE_ROOM);
    walk.inside.shrink_to(SPARE_ROOM);
    SPARE.with(|spare| spare.set(Some(walk)));
    answer
}

thread_local! {
    /// The stacks the last rendering on this thread was done over, emptied,
    /// for the next one to use.
    ///
    /// A rendering allocates its stacks and nothing per value, and keeping
    /// them is what makes it allocate nothing at all once a thread has
    /// rendered once: `benches/rendering`'s `boxed` row, one small struct in a
    /// box, was one allocation a rendering before the walk was a loop, and
    /// would be two more without this. A rendering runs no Cove code, so no
    /// second one can begin on this thread while this one holds them.
    static SPARE: Cell<Option<Walk>> = const { Cell::new(None) };
}

/// How many entries each of a spare [`Walk`]'s stacks keeps room for, so that
/// one rendering nested far deeper than the rest does not keep its stacks'
/// memory for the rest of the run.
const SPARE_ROOM: usize = 256;

/// [`render_value`]'s walk, over stacks it is handed empty.
fn walk_value(
    machine: &Machine,
    layout: LayoutId,
    words: &[u64],
    walk: &mut Walk,
    out: &mut String,
) -> Result<(), RuntimeError> {
    let program = machine.program();
    walk.words.extend_from_slice(words);
    walk.steps.push(Step::Value {
        layout,
        from: 0,
        width: words.len(),
    });
    while let Some(step) = walk.steps.pop() {
        match step {
            Step::Value {
                layout,
                from,
                width,
            } => {
                // One value visited, reported for [`render_into`]'s reason.
                // The bytes are not reported here: `render_into` reports the
                // whole of the text once.
                machine.examined(1);
                value(machine, layout, from, width, walk, out)?;
            }
            Step::Text(text) => out.push_str(text),
            Step::Fields { layout, from, next } => {
                let Shape::Struct { fields, .. } = &program.layout(layout).shape else {
                    unreachable!("a struct's fields are asked of a struct");
                };
                let Some(field) = fields.get(next) else {
                    out.push(')');
                    walk.words.truncate(from);
                    continue;
                };
                if next > 0 {
                    out.push_str(", ");
                }
                write!(out, "{}: ", field.name).expect("a string never fails to be written to");
                let at = from + field.at as usize;
                let width = program.layout(field.layout).width() as usize;
                if at + width > walk.words.len() {
                    return Err(short_run(&field.name));
                }
                walk.steps.push(Step::Fields {
                    layout,
                    from,
                    next: next + 1,
                });
                walk.part(field.layout, at, width);
            }
            Step::Parts {
                layout,
                case,
                from,
                next,
            } => {
                let described = program.layout(layout);
                let Shape::Enum { cases, .. } = &described.shape else {
                    unreachable!("an enum's parts are asked of an enum");
                };
                let Some(part) = cases[case].parts.get(next) else {
                    out.push(')');
                    walk.words.truncate(from);
                    continue;
                };
                if next > 0 {
                    out.push_str(", ");
                }
                let at = from + 1 + part.at as usize;
                let width = program.layout(part.layout).width() as usize;
                if at + width > walk.words.len() {
                    return Err(short_run(&described.name));
                }
                walk.steps.push(Step::Parts {
                    layout,
                    case,
                    from,
                    next: next + 1,
                });
                walk.part(part.layout, at, width);
            }
            Step::Run {
                addr,
                elem,
                len,
                next,
            } => {
                if next == len {
                    continue;
                }
                if next > 0 {
                    out.push_str(", ");
                }
                let stride = program.layout(elem).width();
                walk.steps.push(Step::Run {
                    addr,
                    elem,
                    len,
                    next: next + 1,
                });
                walk.object(machine, elem, addr, next * stride, stride);
            }
            Step::Entries {
                addr,
                key,
                value,
                len,
                next,
            } => {
                if next == len {
                    continue;
                }
                if next > 0 {
                    out.push_str(", ");
                }
                let widths = (program.layout(key).width(), program.layout(value).width());
                let stride = widths.0 + widths.1;
                walk.steps.push(Step::Entries {
                    addr,
                    key,
                    value,
                    len,
                    next: next + 1,
                });
                // The value's words beneath the key's, so that the key, which
                // is rendered first, is on top.
                walk.object(machine, value, addr, next * stride + widths.0, widths.1);
                walk.steps.push(Step::Text(": "));
                walk.object(machine, key, addr, next * stride, widths.0);
            }
            Step::Leave => {
                walk.inside.pop();
                out.push(']');
            }
        }
    }
    Ok(())
}

/// One value location's own text — `layout`, over the `width` words on top of
/// `walk`'s from `from` — with whatever is inside it pushed onto `walk` to be
/// rendered next.
fn value(
    machine: &Machine,
    layout: LayoutId,
    from: usize,
    width: usize,
    walk: &mut Walk,
    out: &mut String,
) -> Result<(), RuntimeError> {
    let program = machine.program();
    let described = program.layout(layout);
    let words = &walk.words[from..from + width];
    match &described.shape {
        Shape::Word(repr) => {
            let word = at(words, 0)?;
            walk.words.truncate(from);
            return render(machine, *repr, word, walk, out);
        }
        // A builtin `Error` renders as the message it carries, not as the
        // struct it happens to be. The oracle special-cases it in
        // `Display for Value` for the reason this one does: a program that
        // prints an error is printing what went wrong, and `Error(message: x)`
        // says the same thing twice. Recognising it by the layout's name is
        // sound because the name is the checker's, and `Error` is a builtin
        // type a module cannot redeclare.
        Shape::Struct { fields, .. }
            if &*described.name == ERROR.name
                && fields.first().map(|field| &*field.name) == Some(MESSAGE_FIELD.name) =>
        {
            let field = &fields[0];
            let at = field.at as usize;
            let part = program.layout(field.layout).width() as usize;
            if at + part > width {
                return Err(short_run(&field.name));
            }
            // The message takes the struct's place on the stack of words.
            walk.words.copy_within(from + at..from + at + part, from);
            walk.words.truncate(from + part);
            walk.steps.push(Step::Value {
                layout: field.layout,
                from,
                width: part,
            });
        }
        // An opaque type renders as its name and nothing else. Its fields are
        // the declaring module's own business, and a rendering is read by
        // whoever the string reaches, so showing them here would publish
        // through `println` what the checker refuses to let a caller name.
        // That is ADR 0014's whole point, and it is why the layout carries
        // the flag rather than this deriving it.
        //
        // `declared` first, `short` second: the layout's name carries the
        // instantiation's type arguments (`m.Cell<m.Point>`), and `short`
        // cuts at the *last* `.` — which, for a qualified argument, is
        // inside the brackets. Stripping the arguments first leaves only
        // the module-qualified declared name for `short` to cut at.
        Shape::Struct { opaque: true, .. } => {
            out.push_str(short(declared(&described.name)));
            walk.words.truncate(from);
        }
        // A `Range` renders as the operator it was written with: `1..3` and
        // `1..<4` cover the same values and are two different renderings,
        // because they are two different values — `==` on ranges compares the
        // bounds a program wrote, not the set they describe.
        Shape::Struct { .. } if is_range(program, described) => {
            let start = at(words, 0)? as i64;
            let end = at(words, 1)? as i64;
            let operator = if at(words, 2)? != 0 { ".." } else { "..<" };
            write!(out, "{start}{operator}{end}").expect("a string never fails to be written to");
            walk.words.truncate(from);
        }
        Shape::Struct { .. } => {
            // The declared name without its module, which is what the
            // public `Display` shows. The layout carries the qualified
            // *instantiation* — type arguments included — because a layout
            // is an identity, so `declared` strips those before `short`
            // strips the module: `short` alone would cut at a qualified
            // argument's own `.` instead (#407).
            write!(out, "{}(", short(declared(&described.name)))
                .expect("a string never fails to be written to");
            walk.steps.push(Step::Fields {
                layout,
                from,
                next: 0,
            });
        }
        // The collector no longer reads the discriminant — the payload
        // region's reference map is static — but a *reader* still must:
        // which of the payload words belong to this value is exactly what
        // the case says.
        Shape::Enum { cases, .. } => {
            let index = at(words, 0)?;
            let case = cases.get(index as usize).ok_or_else(|| {
                RuntimeError::new(format!(
                    "this `{}` is in case {index}, which it does not have",
                    described.name
                ))
            })?;
            out.push_str(&case.name);
            if case.parts.is_empty() {
                walk.words.truncate(from);
            } else {
                out.push('(');
                walk.steps.push(Step::Parts {
                    layout,
                    case: index as usize,
                    from,
                    next: 0,
                });
            }
        }
        Shape::Free => return Err(reclaimed()),
        // Everything left lives in the heap, so the location is one address.
        _ => {
            let addr = at(words, 0)?;
            walk.words.truncate(from);
            return render_object(machine, addr, walk, out);
        }
    }
    Ok(())
}

/// The text of the object at `addr`, appended to `out`, with whatever is
/// inside it pushed onto `walk` to be rendered next.
fn render_object(
    machine: &Machine,
    addr: u64,
    walk: &mut Walk,
    out: &mut String,
) -> Result<(), RuntimeError> {
    if addr == 0 {
        return Err(RuntimeError::new(
            "this value was read before it was given one",
        ));
    }
    let program = machine.program();
    let id = machine.object_layout(addr);
    let layout = program.layout(id);
    match &layout.shape {
        Shape::Str => out.push_str(&string_of(machine, addr)?),
        // Not a Cove value, so nothing renders it deliberately — reached only
        // from a debugger inspecting a run under construction.
        Shape::Bytes => out.push_str("<byte run>"),
        // Nor this, for the same reason. A buffer's bytes may not be valid
        // UTF-8, so the owner shows as the handle it is.
        Shape::ByteBuffer => out.push_str("<byte buffer>"),
        // A value whose *object* this is: a layout the lowering broke a
        // recursion at holds the value's own inline words as its payload, and
        // `Layout::payload_words` answers that same width.
        Shape::Word(_) | Shape::Struct { .. } | Shape::Enum { .. } => {
            walk.object(machine, id, addr, 0, layout.width());
        }
        // A cell shows as the handle it is rather than as what it currently
        // holds, which is `Display for Value`'s answer for the same value:
        // its contents are reachable only under a `lock`, and rendering one
        // would be reading it without taking it.
        Shape::Shared { .. } => out.push_str("<shared>"),
        // A vector renders like an array, because the indirection is what
        // lets it grow without moving and is not a fact about the value:
        // `[1, 2]` is what a program that wrote `Vector.of(1, 2)` sees.
        //
        // The length comes from the vector and not from the store, which is
        // the whole reason the two are separate: a store is as long as the
        // last growth made it, and the elements past the length are the
        // spare room, not the value.
        //
        // A vector is also the only object a value can meet again — every
        // other family here is immutable once built — so it is the one that
        // is looked for on the path (see [`Walk::inside`]). An empty one is
        // never looked for and never pushed: nothing is under it, so it
        // cannot lead back.
        Shape::Vector { elem } => {
            let len = machine.payload(addr, 0) as u32;
            let store = machine.payload(addr, 1);
            if len == 0 || store == 0 {
                out.push_str("[]");
            } else if walk.inside.contains(&addr) {
                out.push_str(REPEAT);
            } else {
                walk.inside.push(addr);
                out.push('[');
                walk.steps.push(Step::Leave);
                walk.steps.push(Step::Run {
                    addr: store,
                    elem: *elem,
                    len,
                    next: 0,
                });
            }
        }
        // An `Array` and a vector's store render alike, which is why one
        // shape covers both — and the stride is the element's width, so an
        // `Array<Point>` renders two words at a time.
        Shape::Elements { elem, .. } => {
            out.push('[');
            walk.steps.push(Step::Text("]"));
            walk.steps.push(Step::Run {
                addr,
                elem: *elem,
                len: machine.object_len(addr),
                next: 0,
            });
        }
        // A set and a map both render inside braces, which is how the
        // language writes them and why they are ordered families rather than
        // hashed ones: the order is part of what a program sees.
        Shape::Members { elem } => {
            out.push('{');
            walk.steps.push(Step::Text("}"));
            walk.steps.push(Step::Run {
                addr,
                elem: *elem,
                len: machine.object_len(addr),
                next: 0,
            });
        }
        Shape::Entries { key, value } => {
            out.push('{');
            walk.steps.push(Step::Text("}"));
            walk.steps.push(Step::Entries {
                addr,
                key: *key,
                value: *value,
                len: machine.object_len(addr),
                next: 0,
            });
        }
        // Erasure is looked through: a `dyn Display` shows the value it
        // holds, because the wrapper is a representation and not something
        // the program put there. Payload word 0 is the layout of what it
        // holds and the words after it are that value, inline.
        Shape::Boxed => {
            let held = LayoutId(machine.payload(addr, 0) as u32);
            let described = program
                .layouts
                .get(held.index())
                .ok_or_else(|| RuntimeError::new("this boxed value carries no known type"))?;
            walk.object(machine, held, addr, 1, described.width());
        }
        Shape::Closure { .. } => out.push_str("<fn>"),
        Shape::Free => return Err(reclaimed()),
    }
    Ok(())
}

/// The word at `at` of a value location.
fn at(words: &[u64], at: usize) -> Result<u64, RuntimeError> {
    words
        .get(at)
        .copied()
        .ok_or_else(|| short_run("value location"))
}

/// A value location held fewer words than its layout says it has.
///
/// A lowering bug rather than anything a program can do, reported because the
/// alternative is reading whatever followed the run.
fn short_run(name: &str) -> RuntimeError {
    RuntimeError::new(format!(
        "this `{}` is narrower than the layout that describes it",
        short(name)
    ))
}

fn reclaimed() -> RuntimeError {
    RuntimeError::new("this value was read after it was reclaimed")
}

/// Whether the object at `addr` is a string.
pub(super) fn is_string(machine: &Machine, addr: u64) -> bool {
    addr != 0
        && matches!(
            machine.program().layout(machine.object_layout(addr)).shape,
            Shape::Str
        )
}

/// The text of the string object at `addr`.
pub(super) fn string_of(machine: &Machine, addr: u64) -> Result<String, RuntimeError> {
    String::from_utf8(machine.string_bytes(addr))
        .map_err(|_| RuntimeError::new("this string's bytes are not valid UTF-8"))
}

/// Renders a `Float` so that it is never mistaken for an `Int`, appended to
/// `out`.
///
/// The language performs no implicit numeric conversions, so a float with no
/// fractional part still shows its point.
fn float(out: &mut String, x: f64) {
    if x.is_nan() {
        out.push_str("NaN");
        return;
    }
    if x.is_infinite() {
        out.push_str(if x.is_sign_negative() { "-inf" } else { "inf" });
        return;
    }
    if x.fract() == 0.0 {
        write!(out, "{x:.1}").expect("a string never fails to be written to");
    } else {
        write!(out, "{x}").expect("a string never fails to be written to");
    }
}

/// Nanoseconds per duration unit, largest first, in the suffixes the lexer
/// accepts.
const DURATION_UNITS: [(i64, &str); 6] = [
    (3_600_000_000_000, "h"),
    (60_000_000_000, "m"),
    (1_000_000_000, "s"),
    (1_000_000, "ms"),
    (1_000, "us"),
    (1, "ns"),
];

/// Renders a `Duration` in the largest unit that divides it exactly,
/// appended to `out`.
fn duration(out: &mut String, ns: i64) {
    if ns == 0 {
        out.push_str("0ns");
        return;
    }
    for (factor, suffix) in DURATION_UNITS {
        if ns % factor == 0 {
            write!(out, "{}{suffix}", ns / factor).expect("a string never fails to be written to");
            return;
        }
    }
    unreachable!("every duration is divisible by one nanosecond")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::exec::tests::Build;
    use crate::vm::intrinsics::operand::Operand;
    use cove_ir::{LayoutId, Program, Repr, Shape};

    /// The program every builtin test is run against.
    ///
    /// One fixture with every family a builtin reaches for, because a builtin
    /// that answers an `Option<Int>` has to find one in the layout table and a
    /// test that declared only the families it thought it needed would be
    /// testing its own fixture. `pub(super)` so that each module's tests build
    /// their objects into the same world; a hand-written program is the only
    /// kind any of them uses, for the reason
    /// [`crate::vm::exec::tests::Build`] gives.
    ///
    /// A family is named by a `LayoutId` rather than by a `Repr` now, so the
    /// scalars are declared first and everything else is built out of them —
    /// which is also what makes an `Array<Point>` expressible here at all.
    pub(super) fn world() -> Program {
        let mut build = Build::default();
        let _unit = build.word("Unit", Repr::Unit);
        let boolean = build.word("Bool", Repr::Bool);
        let int = build.word("Int", Repr::Int);
        let float = build.word("Float", Repr::Float);
        let _duration = build.word("Duration", Repr::Duration);
        let string = build.layout("String", Shape::Str);
        build.program.str_layout = string;

        // An `Error` is its one `String` field, inline: one word.
        let error = build.structure("Error", &[("message", string)]);
        let point = build.structure("Point", &[("x", int), ("y", int)]);

        for elem in [string, int, point] {
            build.layout(
                "Array",
                Shape::Elements {
                    elem,
                    growable: false,
                },
            );
            build.layout(
                "Vector",
                Shape::Elements {
                    elem,
                    growable: true,
                },
            );
            build.layout("Vector", Shape::Vector { elem });
            build.enumeration("Option", &[("None", vec![]), ("Some", vec![elem])]);
        }
        // A `Result` whose `Ok` carries a `String` and whose `Err` is two
        // words, declared *before* the `Result<String, Error>` below and
        // indistinguishable from it by name and by what `Ok` holds. It is
        // here so that a builder answering the narrow one has a wider wrong
        // answer to find — see
        // `a_builtin_answers_the_result_its_instruction_declares`.
        //
        // **The payload has to be a reference for the pair to differ in
        // width at all**, which is the payload-agreement rule and not a
        // choice. `Ok` carrying an `Int` and `Err` carrying an `Error` cannot
        // share the region's first word — one is a scalar and one is an
        // address — so `Result<Int, Error>` is already three words, exactly
        // what `Result<Int, Point>` is, and the two would be
        // indistinguishable by width as well as by name. Two references pack
        // into one word and a `Point`'s two `Int`s do not, so `String` is the
        // one `Ok` payload in this fixture whose two `Result`s differ in
        // width at all.
        // That is why re-pointing this pair at `Int.parse` when
        // `String.fromCodePoint` left does not work, and why the test below
        // drives `make::ok` rather than an intrinsic.
        build.enumeration("Result", &[("Ok", vec![string]), ("Err", vec![point])]);
        for ok in [int, float, string] {
            build.enumeration("Result", &[("Ok", vec![ok]), ("Err", vec![error])]);
        }
        build.layout("Boxed", Shape::Boxed);
        // A `Range` is a struct with the three fields the design fixes, and
        // it is in here because a key sorts after every other family when it
        // is one.
        build.structure(
            "Range",
            &[("start", int), ("end", int), ("inclusive", boolean)],
        );
        for elem in [int, string] {
            build.layout("Set", Shape::Members { elem });
        }
        build.layout(
            "Map",
            Shape::Entries {
                key: int,
                value: int,
            },
        );
        build.layout(
            "Map",
            Shape::Entries {
                key: string,
                value: int,
            },
        );
        build.structure("MapEntry", &[("key", int), ("value", int)]);

        // A two-word element whose reference is **not** its first word.
        //
        // `Point` is two words of `Int` and `String` is one word that is a
        // reference; neither can tell a walk at the element's stride from a
        // walk at a stride of one, because the two coincide. A `Note` can: a
        // clear at the wrong stride leaves word 3 — the second note's text —
        // standing, and a clear at the wrong offset takes out word 1, which is
        // the first note's. `Vector<Note>` is what
        // `a_truncate_of_a_two_word_element_clears_the_reference_in_its_second_word`
        // is built over.
        let note = build.structure("Note", &[("at", int), ("text", string)]);
        build.layout(
            "Array",
            Shape::Elements {
                elem: note,
                growable: false,
            },
        );
        build.layout(
            "Vector",
            Shape::Elements {
                elem: note,
                growable: true,
            },
        );
        build.layout("Vector", Shape::Vector { elem: note });
        build.done()
    }

    /// Calls `receiver.operation` over hand-built operands.
    ///
    /// Direct rather than through the dispatch loop: what a builtin reads is
    /// words and heap objects, so building those by hand is what makes a
    /// failure unambiguously the operation's rather than the lowering's or the
    /// loop's.
    ///
    /// `result` is the free layout here, which is what a caller passes when
    /// it does not care: it names no enum, so `make` falls back to searching
    /// the layout table as it always did. [`answering`] is the form for a
    /// test that does care.
    pub(super) fn run(
        machine: &mut Machine,
        receiver: &str,
        operation: &str,
        operands: &[(Repr, u64)],
    ) -> Result<Vec<u64>, RuntimeError> {
        let held: Vec<(LayoutId, u64)> = operands
            .iter()
            .map(|(repr, word)| (described(machine, *repr, *word), *word))
            .collect();
        let passed: Vec<(LayoutId, &[u64])> = held
            .iter()
            .map(|(layout, word)| (*layout, std::slice::from_ref(word)))
            .collect();
        values(machine, receiver, operation, &passed)
    }

    /// The layout of the value a one-word operand is, as a test hands one
    /// over.
    ///
    /// A scalar's is the fixture's one-word layout for its `Repr`. A
    /// reference's is the layout of the object it names, which is where that
    /// answer has always lived — a `Repr::Ref` says a word is an address and
    /// nothing about what is at the end of it. A null one is a `String`,
    /// which is only a stand-in for "some family that lives in the heap":
    /// what a builtin does with a null reference is refuse it, and every
    /// family refuses it in the same words.
    fn described(machine: &Machine, repr: Repr, word: u64) -> LayoutId {
        match repr {
            Repr::Ref if word == 0 => machine.program().str_layout,
            Repr::Ref => machine.object_layout(word),
            _ => scalar(machine.program(), repr),
        }
    }

    /// Calls `receiver.operation` over operands that are value locations.
    ///
    /// What [`run`] is in terms of, and what a test of a value wider than one
    /// word uses directly: an operand is a layout and the words at a
    /// location, so a `Point` argument is the layout and both of its words.
    /// The same, declaring the layout the answer is supposed to have.
    ///
    /// What `Inst::IntrinsicCall` carries, and the only thing that tells two
    /// instantiations of one family apart.
    pub(super) fn answering(
        machine: &mut Machine,
        receiver: &str,
        operation: &str,
        result: LayoutId,
        operands: &[(LayoutId, &[u64])],
    ) -> Result<Vec<u64>, RuntimeError> {
        let intrinsic = Intrinsic::from_names(receiver, operation)
            .unwrap_or_else(|| panic!("`{receiver}.{operation}` has no `Intrinsic`"));
        in_frame(machine, operands, result, |machine, frame, dest| {
            call(machine, intrinsic, frame, dest)
        })
    }

    /// What `body` writes into a destination of `result`, given a frame
    /// holding `operands` in order.
    ///
    /// The frame is a test's own, pushed onto the machine's stack for the
    /// call and popped after it: operand 0 at slot 0, each next one where the
    /// one before it ends, and the destination after the last. A free
    /// `result` is a one-word answer whose layout the test does not care
    /// about.
    ///
    /// Two guard words follow the destination and are checked afterwards,
    /// because a builtin that wrote past its declared answer is the bug
    /// `make`'s `a_builtin_answers_the_result_its_instruction_declares` pins,
    /// and a destination in a frame cannot say how much of it was written.
    pub(super) fn in_frame(
        machine: &mut Machine,
        operands: &[(LayoutId, &[u64])],
        result: LayoutId,
        body: impl FnOnce(&mut Machine, Frame<'_>, Dest) -> Result<(), RuntimeError>,
    ) -> Result<Vec<u64>, RuntimeError> {
        const GUARD: [u64; 2] = [0x5afe_5afe_5afe_5afe, 0xdead_beef_dead_beef];
        let mut args = Vec::with_capacity(operands.len());
        let mut words = Vec::new();
        for (layout, held) in operands {
            args.push(cove_ir::Arg {
                slot: words.len() as u32,
                layout: *layout,
            });
            words.extend_from_slice(held);
        }
        let dst = words.len() as u32;
        let width = if result == LayoutId::FREE {
            1
        } else {
            machine.words_of(result)
        };
        words.resize(words.len() + width as usize, 0);
        words.extend_from_slice(&GUARD);

        let base = machine.push_test_frame(&words);
        machine.begin_intrinsic();
        let answered = body(
            machine,
            Frame::new(base, &args),
            Dest::new(base, dst, result),
        );
        let after = machine.pop_test_frame(base, words.len() as u32);
        assert_eq!(
            &after[(dst + width) as usize..],
            &GUARD,
            "a builtin wrote past the answer its instruction declares"
        );
        answered.map(|()| after[dst as usize..(dst + width) as usize].to_vec())
    }

    pub(super) fn values(
        machine: &mut Machine,
        receiver: &str,
        operation: &str,
        operands: &[(LayoutId, &[u64])],
    ) -> Result<Vec<u64>, RuntimeError> {
        let result = declared(machine.program(), receiver, operation, operands);
        answering(machine, receiver, operation, result, operands)
    }

    /// The layout the lowering would have put in `Inst::IntrinsicCall`.
    ///
    /// A fixture standing in for the lowering, and it is here rather than in
    /// `make` on purpose: the machine is *given* the layout of the answer,
    /// and a test that builds its operands by hand has to decide it the same
    /// way. What this must not be is a search inside the builtin — that is
    /// the bug `a_builtin_answers_the_result_its_instruction_declares` pins,
    /// and the reason both this and production hand the layout down as data.
    ///
    /// Every operation that answers an `Option` or a `Result` is named here.
    /// Anything else answers a value whose layout the builtin already knows,
    /// so the free layout is right for it and is never read.
    fn declared(
        program: &Program,
        receiver: &str,
        operation: &str,
        operands: &[(LayoutId, &[u64])],
    ) -> LayoutId {
        let ints = || word_layout(program, Repr::Int);
        let held = |at: usize| operands.get(at).map(|(layout, _)| *layout);
        // The payload of the sequence or map the call is on.
        let inside = || {
            let layout = program.layout(held(0)?);
            match layout.shape {
                Shape::Elements { elem, .. } | Shape::Vector { elem } => Some(elem),
                Shape::Entries { value, .. } => Some(value),
                _ => None,
            }
        };
        let payload = match (receiver, operation) {
            ("Array" | "Vector", "get" | "set" | "pop" | "remove") => inside(),
            ("String", "indexOf") => ints(),
            ("Int", "parse" | "parseRadix") | ("Float", "toInt") => ints(),
            ("Float", "parse") => word_layout(program, Repr::Float),
            _ => None,
        };
        let (family, carrier) = match (receiver, operation) {
            ("Int", "parse" | "parseRadix") | ("Float", "parse" | "toInt") => ("Result", "Ok"),
            _ => ("Option", "Some"),
        };
        payload
            .and_then(|payload| carrying(program, family, carrier, payload))
            .unwrap_or(LayoutId::FREE)
    }

    /// The enum called `name` whose case `carrier` holds one `payload`.
    fn carrying(
        program: &Program,
        name: &str,
        carrier: &str,
        payload: LayoutId,
    ) -> Option<LayoutId> {
        program
            .layouts
            .iter()
            .enumerate()
            .find(|(_, layout)| {
                &*layout.name == name
                    && matches!(&layout.shape, Shape::Enum { cases, .. } if cases.iter().any(|case| {
                        &*case.name == carrier
                            && case.parts.len() == 1
                            && case.parts[0].layout == payload
                    }))
            })
            .map(|(at, _)| LayoutId(at as u32))
    }

    /// The one-word layout of `repr`, where the program declares one.
    fn word_layout(program: &Program, repr: Repr) -> Option<LayoutId> {
        program
            .layouts
            .iter()
            .position(|layout| layout.shape == Shape::Word(repr))
            .map(|at| LayoutId(at as u32))
    }

    /// An operand naming the value location `words` of `layout`.
    ///
    /// What a test writes where it means a value rather than a word: a
    /// `Point` argument is `at(point, &[1, 2])`, and the borrow lives as long
    /// as the call it is an argument of.
    pub(super) fn at(layout: LayoutId, words: &[u64]) -> Operand<'_> {
        Operand { layout, words }
    }

    /// The text of the string object at `addr`.
    pub(super) fn read(machine: &Machine, addr: u64) -> String {
        String::from_utf8(machine.string_bytes(addr)).expect("a builtin writes valid UTF-8")
    }

    /// The layout of a run of `elem` elements, as a test builds one.
    pub(super) fn elements(program: &Program, elem: LayoutId, growable: bool) -> LayoutId {
        super::make::elements(program, elem, growable).expect("the fixture declares every family")
    }

    /// The layout of a `Vector` header over `elem` elements.
    pub(super) fn vector(program: &Program, elem: LayoutId) -> LayoutId {
        super::make::vector(program, elem).expect("the fixture declares every family")
    }

    /// The one-word layout of `repr`.
    pub(super) fn scalar(program: &Program, repr: Repr) -> LayoutId {
        program
            .layouts
            .iter()
            .position(|layout| layout.shape == Shape::Word(repr))
            .map(|at| LayoutId(at as u32))
            .expect("the fixture declares every scalar")
    }

    /// The first layout the fixture declares under `name`.
    pub(super) fn named(program: &Program, name: &str) -> LayoutId {
        program
            .layouts
            .iter()
            .position(|layout| &*layout.name == name)
            .map(|at| LayoutId(at as u32))
            .expect("the fixture declares every family")
    }

    /// The enum called `name` whose case `carrier` holds one `payload`.
    ///
    /// The same search `make::two_case` makes, so a test names an
    /// `Option<Int>` the way a builtin finds one.
    pub(super) fn two_case(
        program: &Program,
        name: &str,
        carrier: &str,
        payload: LayoutId,
    ) -> LayoutId {
        program
            .layouts
            .iter()
            .position(|layout| {
                let Shape::Enum { cases, .. } = &layout.shape else {
                    return false;
                };
                &*layout.name == name
                    && cases.iter().any(|case| {
                        &*case.name == carrier
                            && case.parts.len() == 1
                            && case.parts[0].layout == payload
                    })
            })
            .map(|at| LayoutId(at as u32))
            .expect("the fixture declares every family")
    }

    /// The case name and payload words of the enum value `words`, read as the
    /// family `layout` describes.
    ///
    /// An enum is inline now, so what a test asserts on is a run of words
    /// rather than an object — and which of the payload words belong to the
    /// value is what the case says.
    pub(super) fn case_of(
        program: &Program,
        layout: LayoutId,
        words: &[u64],
    ) -> (String, Vec<u64>) {
        let described = program.layout(layout);
        let Shape::Enum { cases, .. } = &described.shape else {
            panic!("`{}` is not an enum", described.name);
        };
        let case = &cases[words[0] as usize];
        let mut payload = Vec::new();
        for part in &case.parts {
            let at = 1 + part.at as usize;
            let width = program.layout(part.layout).width() as usize;
            payload.extend_from_slice(&words[at..at + width]);
        }
        (case.name.to_string(), payload)
    }

    // `option_of` stood here, beside `result_of`. Its callers were `text`'s
    // cases over `String.indexOf`, which is the last intrinsic that answered
    // an `Option` and is `std.string.indexOf` since ADR 0064. No arm answers
    // one now, so nothing reads one back.

    /// What the `Result` whose `Ok` carries an `ok` holds.
    pub(super) fn result_of(program: &Program, ok: LayoutId, words: &[u64]) -> (String, Vec<u64>) {
        case_of(program, two_case(program, "Result", "Ok", ok), words)
    }

    /// The message of the `Error` the `Result` in `words` failed with.
    ///
    /// One dereference rather than two: an `Error` is its `String` field
    /// inline, so the payload word *is* the message's address.
    pub(super) fn message_of(machine: &Machine, ok: LayoutId, words: &[u64]) -> String {
        let (case, payload) = result_of(machine.program(), ok, words);
        assert_eq!(case, "Err", "this `Result` did not fail");
        read(machine, payload[0])
    }

    /// The element words of a run-shaped object at `addr`.
    pub(super) fn words_of(machine: &Machine, addr: u64) -> Vec<u64> {
        let layout = machine.program().layout(machine.object_layout(addr));
        let stride = match layout.shape {
            Shape::Elements { elem, .. } | Shape::Members { elem } => machine.words_of(elem),
            _ => 1,
        };
        machine.payload_run(addr, 0, machine.object_len(addr) * stride)
    }

    // `text_of`, `rendered` and the cases over them stood here: each ran
    // `Value.renderInto` through the dispatch loop over a boxed value. ADR
    // 0068's Phase 4b-ii deleted the intrinsic, and the text of a box is
    // `std.dynamic.renderInto`'s now, held byte for byte to the oracle by the
    // `values_*` corpus on every evaluator. `render_value` stays, for `key`'s
    // wording of a refused key, and the case below still holds it.

    /// A compound value is a run of words now, so a rendering reads runs
    /// rather than following an address per field — and an `Option<Point>`
    /// shows its `Point` inline, out of the same run.
    #[test]
    fn a_compound_value_renders_the_way_the_oracle_shows_it() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let point = named(&program, "Point");
        let option = two_case(&program, "Option", "Some", point);

        let mut out = String::new();
        render_value(&machine, point, &[1, (-2i64) as u64], &mut out).unwrap();
        assert_eq!(out, "Point(x: 1, y: -2)");

        // `[disc, x, y]`: the `Point` is inline in the payload region.
        let mut out = String::new();
        render_value(&machine, option, &[1, 1, (-2i64) as u64], &mut out).unwrap();
        assert_eq!(out, "Some(Point(x: 1, y: -2))");

        let mut out = String::new();
        render_value(&machine, option, &[0, 0, 0], &mut out).unwrap();
        assert_eq!(out, "None");

        // An `Array<Point>` is a run of two-word elements, walked at that
        // stride.
        let run = elements(&program, point, false);
        let items = machine.new_object(run, 2).unwrap();
        machine.set_payload_run(items, 0, &[1, 2, 3, 4]);
        let mut out = String::new();
        render_value(&machine, run, &[items], &mut out).unwrap();
        assert_eq!(out, "[Point(x: 1, y: 2), Point(x: 3, y: 4)]");
        let _ = int;
    }

    // `an_answer_may_be_written_over_its_own_operand` stood here: `x =
    // x.replace("a", "o")` lowered to a call whose destination was `x`, and the
    // case asserted that every arm reads all its operands before it writes
    // (#378, Q5.2). It was `x.trim()` and then `x.toUpper()` before that, and
    // each moved into `std.string`; `replace` was the last, at the end of issue
    // #454's Step 3.
    //
    // **There is no sample left to write it with, and that is a finding rather
    // than a gap.** An alias needs an answer whose slot can be an operand's
    // slot, which needs an operand of the answer's own kind, and no surviving
    // signature has one: `Float.format` answers a `String` over a `Float`,
    // `Float.parse` and `Float.toInt` answer a `Result`, and
    // `String.refuseByteRange` and `Value.admitKey` answer nothing.
    // (`Value.renderInto`, which appended, stood here until ADR 0068's Phase
    // 4b-ii.)
    // (`Value.order`, which answered an `Int` over values of any kind but
    // took a box, which is not an `Int`, and `Any.equals`, which answered a
    // `Bool` the same way, stood beside them until ADR 0068 moved both into
    // `std.dynamic`.) The discipline the case was about is still held for every arm,
    // mechanically rather than by example: the case below panics under
    // `debug_assertions` on an operand read after the answer was written.

    /// The contract that makes that sound is held, not hoped for: an arm that
    /// read an operand after writing its answer would read what it wrote, and
    /// under `debug_assertions` — which the `checked` profile keeps on — the
    /// read panics instead.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "read an operand after writing its answer")]
    fn reading_an_operand_after_writing_the_answer_panics() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let _ = in_frame(&mut machine, &[(int, &[1])], int, |machine, frame, dest| {
            dest.word(machine, 2);
            frame.word(machine, 0);
            Ok(())
        });
    }
}
