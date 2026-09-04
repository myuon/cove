//! Words as text, for a reader that must always be answered.
//!
//! [`crate::vm::boundary::to_value`] is the other direction this crate reads
//! words in, and it returns a `Result` because it is right to: at a boundary
//! crossing, a reclaimed layout or a case index the family does not have is a
//! program about to be handed a value that does not exist, and stopping is
//! the only honest answer.
//!
//! A debugger wants the opposite. **The frames it is most needed for are the
//! broken ones**, and a renderer that refused to describe a value because
//! something inside it was wrong would go silent at exactly the moment it was
//! worth reading. So everything here answers, and every way it can fail has a
//! legible marker of its own: [`RECLAIMED`], [`CYCLE`], [`DEPTH`],
//! [`NO_TYPE`], [`SHORT`], and the sentence a bad case index writes about
//! itself.
//!
//! # It shares `to_value`'s understanding of shapes rather than copying it
//!
//! Every location is offered to [`boundary::to_value`] first, and the whole
//! of a value that converts is rendered by the public [`Value`]'s own
//! `Display` — so a string, a vector, a map, a range and a closure read here
//! exactly as they read anywhere else, and none of that knowledge is written
//! down twice. Only when the conversion *fails* does this file take the value
//! apart, one level at a time, and offer each part to `to_value` in turn.
//! The effect is that a failure is localised to the smallest sub-value it
//! belongs to: a `Point(x: 1, y: <reclaimed>)` says which half is broken,
//! where a single `Result` could only say that something was.
//!
//! What is duplicated is therefore the *decomposition* — which words of a
//! struct are a field, where an enum's payload begins — and nothing else. It
//! is duplicated because it has to run on the path where the shared code has
//! already declined to answer.
//!
//! # What it does not promise
//!
//! Every address this file follows itself is checked against the memory the
//! run has ([`Machine::readable`]) before it is read, which is what makes a
//! word a caller made up answer `<unreadable>` rather than fault. Inside
//! `to_value` there is no such check, and there is deliberately no attempt to
//! add one: the addresses it follows are the ones the collector already
//! trusts, and a heap whose interior addresses are wild is a heap the next
//! collection would not survive either.

use cove_ir::{Layout, LayoutId, Repr, Shape};

use crate::value::Value;
use crate::vm::boundary::{self, declared};
use crate::vm::exec::Machine;

/// A value whose words the sweeper has taken back.
pub(crate) const RECLAIMED: &str = "<reclaimed>";
/// An object that is its own descendant.
pub(crate) const CYCLE: &str = "<cycle>";
/// A value nested deeper than this file walks.
pub(crate) const DEPTH: &str = "<depth>";
/// A layout id no layout in this program answers to.
pub(crate) const NO_TYPE: &str = "<no type>";
/// Fewer words than the location's own width.
pub(crate) const SHORT: &str = "<short>";
/// A reference to no object.
pub(crate) const NULL: &str = "<null>";

/// How deep this file takes a value apart before it stops.
///
/// Only the failing path counts against it — a value that converts is
/// rendered whole by one call — so it bounds the work a *broken* value can
/// cause. A cycle is caught by identity before the count runs out; this is
/// what catches the shapes that are deep without being cyclic.
const MAX_DEPTH: usize = 24;

/// How many characters of one rendered value a reader is given.
///
/// A local may be a vector of a million elements, and a debugger that pasted
/// all of it into a backtrace would be unreadable rather than informative.
/// The marker is the ellipsis, and the reader that wants the rest asks about
/// the object.
const MAX_TEXT: usize = 240;

/// The value at a location of `layout` holding `words`, always.
pub(crate) fn lossy(machine: &Machine, layout: LayoutId, words: &[u64]) -> String {
    let mut inside = Vec::new();
    at_location(machine, layout, words, 0, &mut inside)
}

/// One word read as `repr`, without following it.
///
/// This is the frame's own vocabulary rather than a value's: a slot declares
/// a [`Repr`] and nothing more, so a reference renders as the address it is
/// and is not chased. What it is for is the words of a frame that no local
/// names — the view VM development wants, where the question is what is
/// *in* the word rather than what the program meant by it.
pub(crate) fn raw(machine: &Machine, repr: Repr, word: u64) -> String {
    match repr {
        // The four scalars are rendered by the public value's own `Display`,
        // so a `Duration` reads `1.5s` here exactly as it does when a program
        // prints one, and a float's formatting rules are not decided twice.
        Repr::Unit => Value::unit().to_string(),
        Repr::Bool => Value::bool(word != 0).to_string(),
        Repr::Int => Value::int(word as i64).to_string(),
        Repr::Float => Value::float(f64::from_bits(word)).to_string(),
        Repr::Duration => Value::duration(word as i64).to_string(),
        Repr::Ref => {
            if word == 0 {
                NULL.to_string()
            } else {
                format!("&{word}")
            }
        }
        Repr::Host => match machine.resource(word) {
            Some(resource) => format!("{}#{}", resource.qualified_type(), resource.id),
            None if word == 0 => NULL.to_string(),
            None => "<no resource>".to_string(),
        },
        // Three words that name something of this run and mean nothing
        // outside it. The boundary refuses all three; a debugger is inside
        // the run, so it shows the number and says what it indexes.
        Repr::Addr => format!("@{word}"),
        Repr::Task => task_or_scope("task", word),
        Repr::Scope => task_or_scope("scope", word),
    }
}

/// The name and the parts of the object at `addr`, or `None` for a word that
/// names no object this run's memory holds.
///
/// The parts are named the way the shape names them: a struct's fields by
/// their source names, a run of elements by its indices, an enum by `case`
/// and then its payload's positions.
pub(crate) fn parts(machine: &Machine, addr: u64) -> Option<(String, Vec<(String, String)>)> {
    if addr == 0 || !machine.readable(addr, 1) {
        return None;
    }
    let program = machine.program();
    let id = machine.object_layout(addr);
    let described = program.layouts.get(id.index())?;
    let mut inside = vec![addr];
    let name = described.name.to_string();
    let len = machine.object_len(addr);
    let fields = match &described.shape {
        Shape::Free => vec![("words".to_string(), len.to_string())],
        Shape::Str => vec![("text".to_string(), string_of(machine, addr))],
        Shape::Word(_) | Shape::Struct { .. } | Shape::Enum { .. } => {
            match payload(machine, addr, 0, described.width()) {
                Some(words) => inline_parts(machine, id, described, &words, &mut inside),
                None => vec![],
            }
        }
        Shape::Elements { elem, .. } | Shape::Members { elem } => {
            indexed(machine, addr, *elem, 0, len, &mut inside)
        }
        Shape::Vector { elem } => {
            let (count, store) = match payload(machine, addr, 0, 2) {
                Some(words) => (words[0] as u32, words[1]),
                None => (0, 0),
            };
            let mut out = vec![("length".to_string(), count.to_string())];
            if store != 0 && machine.readable(store, 1) {
                out.extend(indexed(machine, store, *elem, 0, count, &mut inside));
            }
            out
        }
        Shape::Entries { key, value } => {
            let widths = (program.layout(*key).width(), program.layout(*value).width());
            let stride = widths.0 + widths.1;
            (0..len)
                .map(|at| {
                    let key = element(machine, addr, *key, at * stride, widths.0, &mut inside);
                    let value = element(
                        machine,
                        addr,
                        *value,
                        at * stride + widths.0,
                        widths.1,
                        &mut inside,
                    );
                    (key, value)
                })
                .collect()
        }
        // Payload word 0 is the family, and the words after it are the value.
        Shape::Boxed => {
            let held = LayoutId(machine.payload(addr, 0) as u32);
            let name = match program.layouts.get(held.index()) {
                Some(described) => described.name.to_string(),
                None => NO_TYPE.to_string(),
            };
            vec![
                ("held".to_string(), name),
                (
                    "value".to_string(),
                    element(
                        machine,
                        addr,
                        held,
                        1,
                        held_width(machine, held),
                        &mut inside,
                    ),
                ),
            ]
        }
        Shape::Closure { .. } => {
            let mut out = Vec::new();
            out.push((
                "function".to_string(),
                match machine.callee_of(addr) {
                    Ok(callee) => program.function(callee).qualified(),
                    Err(_) => NO_TYPE.to_string(),
                },
            ));
            if let Ok(callee) = machine.callee_of(addr) {
                let mut at = 1;
                for (index, capture) in program.function(callee).captures.iter().enumerate() {
                    let width = program.layout(capture.layout).width();
                    out.push((
                        format!("capture {index}"),
                        element(machine, addr, capture.layout, at, width, &mut inside),
                    ));
                    at += width;
                }
            }
            out
        }
        // The lock word, and then the value inline. A `Shared` never crosses
        // the boundary — `to_value` refuses it, and rightly — which is
        // exactly why a debugger has to be able to look inside one.
        Shape::Shared { value } => {
            let width = program.layout(*value).width();
            vec![
                (
                    "locked".to_string(),
                    (machine.payload(addr, 0) != 0).to_string(),
                ),
                (
                    "value".to_string(),
                    element(
                        machine,
                        addr,
                        *value,
                        cove_ir::layout::SHARED_VALUE,
                        width,
                        &mut inside,
                    ),
                ),
            ]
        }
    };
    Some((name, fields))
}

/// The value at a location of `layout` holding `words`, `depth` levels down.
fn at_location(
    machine: &Machine,
    layout: LayoutId,
    words: &[u64],
    depth: usize,
    inside: &mut Vec<u64>,
) -> String {
    if depth >= MAX_DEPTH {
        return DEPTH.to_string();
    }
    let program = machine.program();
    let Some(described) = program.layouts.get(layout.index()) else {
        return NO_TYPE.to_string();
    };
    if (words.len() as u32) < described.width() {
        return SHORT.to_string();
    }
    // An object this walk is already inside is a cycle, and it is answered
    // before the conversion is tried: `to_value` would spend its whole depth
    // budget discovering the same thing and then say only that the value was
    // too deep.
    let address = described.is_one_address().then(|| words[0]);
    if let Some(addr) = address {
        if inside.contains(&addr) {
            return CYCLE.to_string();
        }
        if addr != 0 && !machine.readable(addr, 1) {
            return format!("<unreadable {addr}>");
        }
    }
    // The ordinary path, and the reason this file is not a second boundary.
    if let Ok(value) = boundary::to_value(machine, layout, words) {
        return clip(&value.to_string());
    }
    // From here on the conversion has declined, so the value is taken apart
    // to find out which part of it is the reason.
    match &described.shape {
        Shape::Free => RECLAIMED.to_string(),
        Shape::Word(repr) => match repr {
            Repr::Ref => object(machine, words[0], depth, inside),
            other => raw(machine, *other, words[0]),
        },
        Shape::Struct { .. } | Shape::Enum { .. } => {
            let parts = inline_parts(machine, layout, described, words, inside);
            wrap(declared(&described.name), &parts)
        }
        _ => object(machine, words[0], depth, inside),
    }
}

/// The object at `addr`, as a reader sees it.
fn object(machine: &Machine, addr: u64, depth: usize, inside: &mut Vec<u64>) -> String {
    if addr == 0 {
        return NULL.to_string();
    }
    if inside.contains(&addr) {
        return CYCLE.to_string();
    }
    if depth >= MAX_DEPTH {
        return DEPTH.to_string();
    }
    if !machine.readable(addr, 1) {
        return format!("<unreadable {addr}>");
    }
    let program = machine.program();
    let id = machine.object_layout(addr);
    let Some(described) = program.layouts.get(id.index()) else {
        return NO_TYPE.to_string();
    };
    let deeper = depth + 1;
    let len = machine.object_len(addr);
    inside.push(addr);
    let out = match &described.shape {
        Shape::Free => RECLAIMED.to_string(),
        Shape::Str => clip(&format!("\"{}\"", string_of(machine, addr))),
        // A layout the lowering broke a recursion at holds the value's own
        // inline words as its payload, so the payload is read as a location.
        Shape::Word(_) | Shape::Struct { .. } | Shape::Enum { .. } => {
            match payload(machine, addr, 0, described.width()) {
                Some(words) => at_location(machine, id, &words, deeper, inside),
                None => SHORT.to_string(),
            }
        }
        Shape::Elements { elem, .. } => list(machine, addr, *elem, len, deeper, inside),
        Shape::Members { elem } => list(machine, addr, *elem, len, deeper, inside),
        Shape::Vector { elem } => {
            let (count, store) = match payload(machine, addr, 0, 2) {
                Some(words) => (words[0] as u32, words[1]),
                None => (0, 0),
            };
            if store == 0 {
                "[]".to_string()
            } else {
                list(machine, store, *elem, count, deeper, inside)
            }
        }
        Shape::Entries { key, value } => {
            let widths = (program.layout(*key).width(), program.layout(*value).width());
            let stride = widths.0 + widths.1;
            let shown: Vec<String> = (0..len)
                .map(|at| {
                    let k = one(machine, addr, *key, at * stride, widths.0, deeper, inside);
                    let v = one(
                        machine,
                        addr,
                        *value,
                        at * stride + widths.0,
                        widths.1,
                        deeper,
                        inside,
                    );
                    format!("{k}: {v}")
                })
                .collect();
            clip(&format!("{{{}}}", shown.join(", ")))
        }
        Shape::Boxed => {
            let held = LayoutId(machine.payload(addr, 0) as u32);
            match program.layouts.get(held.index()) {
                Some(_) => one(
                    machine,
                    addr,
                    held,
                    1,
                    held_width(machine, held),
                    deeper,
                    inside,
                ),
                None => NO_TYPE.to_string(),
            }
        }
        Shape::Closure { .. } => match machine.callee_of(addr) {
            Ok(callee) => format!("<closure {}>", program.function(callee).qualified()),
            Err(_) => "<closure>".to_string(),
        },
        Shape::Shared { value } => {
            let width = program.layout(*value).width();
            let held = one(
                machine,
                addr,
                *value,
                cove_ir::layout::SHARED_VALUE,
                width,
                deeper,
                inside,
            );
            format!("Shared({held})")
        }
    };
    inside.pop();
    out
}

/// The named parts of an inline struct or enum, each rendered on its own.
fn inline_parts(
    machine: &Machine,
    layout: LayoutId,
    described: &Layout,
    words: &[u64],
    inside: &mut Vec<u64>,
) -> Vec<(String, String)> {
    let program = machine.program();
    match &described.shape {
        Shape::Struct { fields, .. } => fields
            .iter()
            .map(|field| {
                let width = program.layout(field.layout).width();
                let run = words.get(field.at as usize..(field.at + width) as usize);
                let text = match run {
                    Some(run) => at_location(machine, field.layout, run, 1, inside),
                    None => SHORT.to_string(),
                };
                (field.name.to_string(), text)
            })
            .collect(),
        Shape::Enum { cases, .. } => {
            let index = words[0];
            let Some(case) = cases.get(index as usize) else {
                // The marker the sketch asks for, in the words the family
                // makes true: a case index a value cannot have.
                return vec![(
                    "case".to_string(),
                    format!("<case {index} of {}>", cases.len()),
                )];
            };
            let mut out = vec![("case".to_string(), case.name.to_string())];
            for (at, part) in case.parts.iter().enumerate() {
                let width = program.layout(part.layout).width();
                let from = 1 + part.at as usize;
                let run = words.get(from..from + width as usize);
                out.push((
                    at.to_string(),
                    match run {
                        Some(run) => at_location(machine, part.layout, run, 1, inside),
                        None => SHORT.to_string(),
                    },
                ));
            }
            out
        }
        _ => vec![(
            "value".to_string(),
            at_location(machine, layout, words, 1, inside),
        )],
    }
}

/// `len` elements of the object at `addr`, rendered as a list.
fn list(
    machine: &Machine,
    addr: u64,
    elem: LayoutId,
    len: u32,
    depth: usize,
    inside: &mut Vec<u64>,
) -> String {
    let stride = machine.program().layout(elem).width();
    let shown: Vec<String> = (0..len)
        .map(|at| one(machine, addr, elem, at * stride, stride, depth, inside))
        .collect();
    clip(&format!("[{}]", shown.join(", ")))
}

/// The same, as named parts.
fn indexed(
    machine: &Machine,
    addr: u64,
    elem: LayoutId,
    from: u32,
    len: u32,
    inside: &mut Vec<u64>,
) -> Vec<(String, String)> {
    let stride = machine.program().layout(elem).width();
    (0..len)
        .map(|at| {
            (
                at.to_string(),
                element(machine, addr, elem, from + at * stride, stride, inside),
            )
        })
        .collect()
}

/// One value of `layout` at payload word `at` of the object at `addr`.
fn one(
    machine: &Machine,
    addr: u64,
    layout: LayoutId,
    at: u32,
    width: u32,
    depth: usize,
    inside: &mut Vec<u64>,
) -> String {
    match payload(machine, addr, at, width) {
        Some(words) => at_location(machine, layout, &words, depth, inside),
        None => SHORT.to_string(),
    }
}

/// The same, from the top: what a part of an object renders as.
fn element(
    machine: &Machine,
    addr: u64,
    layout: LayoutId,
    at: u32,
    width: u32,
    inside: &mut Vec<u64>,
) -> String {
    one(machine, addr, layout, at, width, 1, inside)
}

/// `words` payload words of the object at `addr`, or `None` when this run's
/// memory does not hold them.
fn payload(machine: &Machine, addr: u64, at: u32, words: u32) -> Option<Vec<u64>> {
    machine
        .readable(addr + 1 + at as u64, words)
        .then(|| machine.payload_run(addr, at, words))
}

/// The bytes of the string object at `addr`, however they decode.
///
/// Lossy in the standard library's sense as well as this file's: bytes that
/// are not UTF-8 become the replacement character rather than an error, which
/// is the whole difference from the boundary — a string a bug wrote is a
/// string a debugger most wants to see.
fn string_of(machine: &Machine, addr: u64) -> String {
    String::from_utf8_lossy(&machine.string_bytes(addr)).into_owned()
}

/// The width of a boxed family, or one word if the program has no such
/// family.
fn held_width(machine: &Machine, held: LayoutId) -> u32 {
    machine
        .program()
        .layouts
        .get(held.index())
        .map(Layout::width)
        .unwrap_or(1)
}

/// `name(part: value, ...)`, which is how an inline family reads when it had
/// to be taken apart.
fn wrap(name: &str, parts: &[(String, String)]) -> String {
    let shown: Vec<String> = parts
        .iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect();
    clip(&format!("{name}({})", shown.join(", ")))
}

/// A word that is one past an index into a table of this task's.
fn task_or_scope(what: &str, word: u64) -> String {
    match word {
        0 => NULL.to_string(),
        _ => format!("<{what} {}>", word - 1),
    }
}

/// `text`, cut to something a reader can read.
fn clip(text: &str) -> String {
    if text.chars().count() <= MAX_TEXT {
        return text.to_string();
    }
    let kept: String = text.chars().take(MAX_TEXT).collect();
    format!("{kept}…")
}

#[cfg(test)]
mod tests {
    use cove_ir::{Program, Shape};

    use super::*;
    use crate::vm::exec::tests::Build;

    /// A fixture: a program of layouts, and a machine over it.
    struct World {
        program: Program,
    }

    impl World {
        fn new(build: impl FnOnce(&mut Build)) -> World {
            let mut it = Build::default();
            build(&mut it);
            World { program: it.bare() }
        }

        fn machine(&self) -> Machine<'_> {
            Machine::new(&self.program, 1 << 12)
        }

        fn named(&self, name: &str) -> LayoutId {
            self.program
                .layouts
                .iter()
                .position(|layout| &*layout.name == name)
                .map(|at| LayoutId(at as u32))
                .expect("the fixture declares every family")
        }
    }

    /// **What can be read is read by the boundary's own conversion.**
    ///
    /// The rule that keeps this file from being a second boundary: a value
    /// that crosses renders exactly as the public `Value` renders it, and
    /// none of what `to_value` knows about shapes is written down twice.
    #[test]
    fn a_value_that_converts_renders_as_the_boundary_renders_it() {
        let world = World::new(|build| {
            let int = build.scalar(cove_ir::Repr::Int);
            build.layout(
                "m.List",
                Shape::Elements {
                    elem: int,
                    growable: false,
                },
            );
        });
        let list = world.named("m.List");
        let mut machine = world.machine();
        let addr = machine.new_object(list, 2).expect("the heap has room");
        machine.set_payload(addr, 0, 1);
        machine.set_payload(addr, 1, 2);

        assert_eq!(lossy(&machine, list, &[addr]), "[1, 2]");
    }

    /// **A value whose object has been reclaimed says so.**
    ///
    /// `to_value` refuses this and is right to — a program handed a
    /// reclaimed value would be handed words that are no longer anybody's —
    /// and a debugger needs the opposite, because a frame holding a
    /// reclaimed reference is exactly the frame worth looking at.
    #[test]
    fn a_reclaimed_object_renders_as_reclaimed_rather_than_failing() {
        let world = World::new(|build| {
            let int = build.scalar(cove_ir::Repr::Int);
            build.layout(
                "m.List",
                Shape::Elements {
                    elem: int,
                    growable: false,
                },
            );
        });
        let list = world.named("m.List");
        let mut machine = world.machine();
        let addr = machine.new_object(list, 2).expect("the heap has room");
        // Nothing roots it: this machine has no frames, no temporaries and
        // no interned strings, so the collection takes it back.
        machine.collect();

        assert!(
            boundary::to_value(&machine, list, &[addr]).is_err(),
            "the boundary refuses a reclaimed value, which is what makes this worth answering"
        );
        assert_eq!(lossy(&machine, list, &[addr]), RECLAIMED);
    }

    /// **An enum in a case its family does not have says which case, and how
    /// many there are.**
    ///
    /// The one failure a marker alone would not explain: `<case 7 of 3>` is
    /// what a reader needs to tell a wrong discriminant from a wrong layout.
    #[test]
    fn an_enum_in_a_case_it_does_not_have_names_the_case_and_the_count() {
        let world = World::new(|build| {
            let int = build.scalar(cove_ir::Repr::Int);
            build.enumeration("m.Colour", &[("Red", vec![]), ("Green", vec![int])]);
        });
        let colour = world.named("m.Colour");
        let machine = world.machine();
        let width = machine.program().layout(colour).width() as usize;
        let mut words = vec![0; width];
        words[0] = 7;

        let text = lossy(&machine, colour, &words);
        assert!(
            text.contains("<case 7 of 2>"),
            "a case index the family does not have is named: {text}"
        );
    }

    /// **An object that reaches itself renders as a cycle rather than
    /// recursing.**
    ///
    /// `StoreField` can make an object hold itself, and `to_value` answers
    /// that by running out of depth — a true statement that says nothing.
    /// This says which part of the value closed the loop.
    #[test]
    fn an_object_that_holds_itself_renders_as_a_cycle() {
        let world = World::new(|build| {
            let int = build.scalar(cove_ir::Repr::Int);
            let list = build.layout(
                "m.List",
                Shape::Elements {
                    elem: int,
                    growable: false,
                },
            );
            // Tied afterwards, because a list of lists cannot name itself
            // until it exists.
            if let Shape::Elements { elem, .. } = &mut build.program.layouts[list.index()].shape {
                *elem = list;
            }
        });
        let list = world.named("m.List");
        let mut machine = world.machine();
        let addr = machine.new_object(list, 1).expect("the heap has room");
        machine.set_payload(addr, 0, addr);

        assert_eq!(lossy(&machine, list, &[addr]), format!("[{CYCLE}]"));
    }

    /// **A word this memory does not hold is answered, not followed.**
    ///
    /// The difference between a debugger and a boundary in one line: a
    /// caller may hand this file a word it made up, and the answer is a
    /// marker rather than a read of whatever is at that address.
    #[test]
    fn a_word_that_names_nothing_is_answered_rather_than_followed() {
        let world = World::new(|build| {
            let int = build.scalar(cove_ir::Repr::Int);
            build.layout(
                "m.List",
                Shape::Elements {
                    elem: int,
                    growable: false,
                },
            );
        });
        let list = world.named("m.List");
        let machine = world.machine();

        assert_eq!(lossy(&machine, list, &[0]), NULL);
        assert!(lossy(&machine, list, &[1 << 40]).starts_with("<unreadable"));
        assert!(parts(&machine, 1 << 40).is_none());
    }

    /// **An object's parts come back named the way its family names them.**
    ///
    /// What a session shows when it is asked about an address: the family,
    /// and each part under the name that family gives it.
    #[test]
    fn an_object_hands_out_its_parts_under_their_own_names() {
        let world = World::new(|build| {
            let int = build.scalar(cove_ir::Repr::Int);
            build.layout(
                "m.List",
                Shape::Elements {
                    elem: int,
                    growable: false,
                },
            );
        });
        let list = world.named("m.List");
        let mut machine = world.machine();
        let addr = machine.new_object(list, 2).expect("the heap has room");
        machine.set_payload(addr, 0, 11);
        machine.set_payload(addr, 1, 22);

        let (name, fields) = parts(&machine, addr).expect("an object of this heap");
        assert_eq!(name, "m.List");
        assert_eq!(
            fields,
            vec![
                ("0".to_string(), "11".to_string()),
                ("1".to_string(), "22".to_string())
            ]
        );
    }

    /// **A frame word renders as what the frame says is in it.**
    ///
    /// The VM development view: a word no name covers is read by its
    /// `Repr` and a reference is shown rather than chased, because what is
    /// being asked is what is in the word.
    #[test]
    fn a_raw_word_renders_under_the_frame_s_own_description() {
        let world = World::new(|_| {});
        let machine = world.machine();
        assert_eq!(raw(&machine, cove_ir::Repr::Int, -3i64 as u64), "-3");
        assert_eq!(raw(&machine, cove_ir::Repr::Bool, 1), "true");
        assert_eq!(raw(&machine, cove_ir::Repr::Float, 1.5f64.to_bits()), "1.5");
        assert_eq!(raw(&machine, cove_ir::Repr::Ref, 0), NULL);
        assert_eq!(raw(&machine, cove_ir::Repr::Ref, 96), "&96");
        assert_eq!(raw(&machine, cove_ir::Repr::Task, 3), "<task 2>");
    }
}
