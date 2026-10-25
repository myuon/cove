//! Every layout says one thing about its words.
//!
//! A [`Layout`] carries its [`Shape`] and its `words` side by side, and
//! everything downstream reads whichever of the two it needs: a copy moves
//! `words.len()` words, the collector traces the `Repr::Ref`s among `words`,
//! the machine bounds a field by the shape's payload, a reflection walks a
//! struct's fields. The lowering builds the two from one declaration
//! (`struct_layout`, `enum_layout`, `Layout::object`), so they agree by
//! construction there. A program from anywhere else can make them disagree,
//! and a field placed past its struct's words, or a `Word(Int)` whose one word
//! is a `Ref`, is a word read as something it is not — the confusion every
//! other check in this crate exists to rule out.
//!
//! So this runs after [`super::indices`], before anything reads a layout as
//! the truth, and checks
//!
//! - **that the words are the shape's**: a `Word(r)` is `[r]`; every object
//!   family (a string, a run, a vector, a map, a closure, a cell, a box) is
//!   the one `Ref` word that names it; a struct's fields and an enum's parts
//!   sit inside its words and are, word for word, the words of their own
//!   layouts, and an enum's words are a `Tag` and then its payload;
//! - **that `LayoutId(0)` is the free layout**, because the sweeper writes id
//!   0 into every block it reclaims and the machine reads id 0 as "this was
//!   reclaimed": an object of another layout at id 0 would be read as free;
//! - **that inline layouts do not contain themselves**: a struct whose field
//!   is the struct is a set of words with no end to walk, and only an object
//!   family — one `Ref` word — may lead back to a layout;
//! - **that no heap payload holds an [`Repr::Addr`]**: an address is a frame's
//!   and lives as long as the frame, so a run, a cell, a closure's captures or
//!   a map that kept one would keep a place past the frame it was in;
//! - **that the program-wide layouts are the shapes they are named for**: the
//!   string layout is a `Str`, the byte run a `Bytes`, and so on, because the
//!   machine allocates by them without being told what they are.
//!
//! [ADR 0120](../../../../docs/adr/0120-the-vm-runs-only-what-verifies.md).

use super::Invalid;
use crate::layout::{Layout, LayoutId, Shape};
use crate::program::Program;
use crate::repr::Repr;

/// Every disagreement between a layout's shape and its words, or nothing.
pub(super) fn check(program: &Program) -> Vec<Invalid> {
    let mut faults = Vec::new();
    let layouts = &program.layouts;
    let mut fault = |index: usize, what: String| {
        faults.push(Invalid {
            function: format!("<layout {index}>"),
            pc: None,
            what,
        })
    };
    for (index, layout) in layouts.iter().enumerate() {
        let words = |id: LayoutId| &layouts[id.index()].words;
        let name = &layout.name;
        if index == 0 && layout.shape != Shape::Free {
            fault(
                index,
                format!(
                    "is `{name}`, and layout 0 is the free layout: the sweeper writes id 0 into \
                     every block it reclaims"
                ),
            );
        }
        match &layout.shape {
            Shape::Free => {
                if !layout.words.is_empty() {
                    fault(index, format!("`{name}` is free and has words"));
                }
            }
            Shape::Word(repr) => {
                if layout.words != [*repr] {
                    fault(
                        index,
                        format!("`{name}` is a `{repr}` word and its words are not that word"),
                    );
                }
            }
            Shape::Struct { fields, .. } => {
                for field in fields {
                    let want = words(field.layout);
                    let held = slice(&layout.words, field.at, want.len());
                    if held != Some(want.as_slice()) {
                        fault(
                            index,
                            format!(
                                "field `{}` of `{name}` is at word {} and is not the words of \
                                 its own layout there",
                                field.name, field.at
                            ),
                        );
                    }
                }
            }
            Shape::Enum { cases, payload } => {
                if layout.words.first() != Some(&Repr::Tag) || layout.words[1..] != payload[..] {
                    fault(
                        index,
                        format!("`{name}` is an enum and its words are not a tag and its payload"),
                    );
                }
                for case in cases {
                    for part in &case.parts {
                        let want = words(part.layout);
                        if slice(payload, part.at, want.len()) != Some(want.as_slice()) {
                            fault(
                                index,
                                format!(
                                    "a part of case `{}` of `{name}` is at payload word {} and \
                                     is not the words of its own layout there",
                                    case.name, part.at
                                ),
                            );
                        }
                    }
                }
            }
            Shape::Str
            | Shape::Bytes
            | Shape::Elements { .. }
            | Shape::Vector { .. }
            | Shape::ByteBuffer
            | Shape::Members { .. }
            | Shape::Entries { .. }
            | Shape::Closure { .. }
            | Shape::Shared { .. }
            | Shape::Boxed
            | Shape::IdentityTable => {
                if layout.words != [Repr::Ref] {
                    fault(
                        index,
                        format!("`{name}` is an object and its words are not the one reference"),
                    );
                }
            }
        }
        // What a heap payload of this family holds, which may not be a place.
        let held: Vec<LayoutId> = match &layout.shape {
            Shape::Elements { elem, .. } | Shape::Vector { elem } | Shape::Members { elem } => {
                vec![*elem]
            }
            Shape::Entries { key, value } => vec![*key, *value],
            Shape::Closure { captures, .. } => captures.clone(),
            Shape::Shared { value } => vec![*value],
            _ => Vec::new(),
        };
        if held.iter().any(|id| holds_a_place(&layouts[id.index()])) {
            fault(
                index,
                format!(
                    "`{name}` is a heap object whose payload holds an address, and an address \
                     lives no longer than the frame it names"
                ),
            );
        }
    }
    if let Some(index) = inline_cycle(layouts) {
        let name = &layouts[index].name;
        fault(
            index,
            format!(
                "`{name}` contains itself inline; only an object, which is one reference, may \
                 lead back to a layout"
            ),
        );
    }
    let wants: [(&str, LayoutId, fn(&Shape) -> bool); 5] = [
        ("str_layout", program.str_layout, |s| *s == Shape::Str),
        ("bytes_layout", program.bytes_layout, |s| *s == Shape::Bytes),
        ("buffer_layout", program.buffer_layout, |s| {
            *s == Shape::ByteBuffer
        }),
        ("boxed_layout", program.boxed_layout, |s| *s == Shape::Boxed),
        (
            "identity_table_layout",
            program.identity_table_layout,
            |s| *s == Shape::IdentityTable,
        ),
    ];
    // A program with no layouts at all is a fixture with nothing to allocate,
    // and `indices` has already let it through on those terms.
    if !layouts.is_empty() {
        for (what, id, is) in wants {
            // `identity_table_layout` may be the free layout, in a program
            // that never reflects on a vector: the lowering seeds it lazily.
            if what == "identity_table_layout" && id == LayoutId::FREE {
                continue;
            }
            if !is(&layouts[id.index()].shape) {
                let name = &layouts[id.index()].name;
                fault(
                    id.index(),
                    format!("the program's `{what}` is `{name}`, which is not that shape"),
                );
            }
        }
    }
    faults
}

/// `words[at..at + len]`, if it is all there.
fn slice(words: &[Repr], at: u32, len: usize) -> Option<&[Repr]> {
    let at = at as usize;
    words.get(at..at.checked_add(len)?)
}

/// Whether a value of `layout` holds an address anywhere in its words.
fn holds_a_place(layout: &Layout) -> bool {
    layout.words.contains(&Repr::Addr)
}

/// A layout that reaches itself through inline fields and parts alone, if
/// there is one.
fn inline_cycle(layouts: &[Layout]) -> Option<usize> {
    // 0 unvisited, 1 on the walk's path, 2 done.
    let mut state = vec![0u8; layouts.len()];
    for start in 0..layouts.len() {
        if state[start] != 0 {
            continue;
        }
        // An explicit stack rather than recursion: a hostile table can nest as
        // deep as it is long.
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        state[start] = 1;
        while let Some(top) = stack.len().checked_sub(1) {
            let (at, next) = stack[top];
            let children = inline_children(&layouts[at]);
            if let Some(&child) = children.get(next) {
                stack[top].1 += 1;
                match state[child] {
                    0 => {
                        state[child] = 1;
                        stack.push((child, 0));
                    }
                    1 => return Some(child),
                    _ => {}
                }
            } else {
                state[at] = 2;
                stack.pop();
            }
        }
    }
    None
}

/// The layouts a value of `layout` holds inline: its fields', or its parts'.
fn inline_children(layout: &Layout) -> Vec<usize> {
    match &layout.shape {
        Shape::Struct { fields, .. } => fields.iter().map(|f| f.layout.index()).collect(),
        Shape::Enum { cases, .. } => cases
            .iter()
            .flat_map(|case| case.parts.iter().map(|p| p.layout.index()))
            .collect(),
        _ => Vec::new(),
    }
}
