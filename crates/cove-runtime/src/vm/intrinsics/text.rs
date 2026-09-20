//! `String`.
//!
//! A Cove `String` is UTF-8 and its object is the bytes, eight to a word. The
//! operations here are the oracle's, and where the oracle's answer depends on
//! how it reads those bytes, this one reads them the same way rather than
//! choosing again:
//!
//! - **`length()` counts characters, not bytes.** It is `chars().count()`, so
//!   it agrees with `chars()`, and it is *not* the header's length field —
//!   which is why `String.length` is a builtin rather than an
//!   [`Inst::Len`](cove_ir::Inst::Len).
//! - **`slice(from, to)` is in character positions**, and so is what
//!   `indexOf` answers, which is why `indexOf` converts the byte offset it
//!   finds by counting the characters before it.
//! - **`contains`, `startsWith`, `endsWith`, `split` and `replace` match
//!   bytes**, which for UTF-8 is the same set of matches as matching
//!   characters and is what Rust's own `str` does.
//! - **`trim()` trims Unicode whitespace** and **`words()` splits on ASCII
//!   whitespace**, which is the pair the oracle has and is not a distinction
//!   this file invented.
//! - **`toUpper()` and `toLower()` are full Unicode case mappings**, so the
//!   answer may be longer than what it was called on.
//!
//! Every one of those is `crates/cove-runtime/src/builtins.rs`'s reading. The
//! bytes are decoded into a Rust `String` once and the operation runs on
//! that, so there is exactly one place either backend could be reading them
//! differently, and it is this sentence.
//!
//! # Every operation here says what it examined, in bytes
//!
//! Nine of the operations below walk the whole receiver, four search it and
//! one builds its answer out of parts, and until
//! [ADR 0064](../../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
//! Decision 7 every one of them cost the run **one** unit of work — the one
//! an `add.int` costs. So `"ab".length()` and a `length()` over a hundred
//! thousand characters spent the same fuel, ran for 383 times the wall clock,
//! and were the same to a cancellation, a deadline and a fuel bound alike.
//!
//! Each arm therefore calls [`Machine::examined`] with what it looked at, and
//! **the unit is bytes**, not characters and not words: it is the unit
//! `Machine::bulk_work` already counts for an `Inst::RunCopy` over a
//! `Storage::PackedBytes`, and a `String`'s object *is* a packed byte run.
//! Charging characters would make the same text cost different amounts
//! depending on the script it is written in, and charging words would divide
//! every figure by eight for no reason a reader could recover.
//!
//! What is charged is **what was examined**, not what was handed back, and
//! the two part company in both directions:
//!
//! - the nine that walk the receiver — `length`, `words`, `chars`, `split`,
//!   `slice`, `trim`, `replace`, `toUpper`, `toLower` — charge the receiver's
//!   own byte length, because [`operand::text`] has already decoded the whole
//!   of it before any of them looks at a single character;
//! - `startsWith` and `endsWith` compare at most the needle, so they charge
//!   the needle's length capped at the receiver's;
//! - `contains` and `indexOf` charge the **receiver's** length, and that is
//!   an upper bound rather than a measurement: `str::find` does not report
//!   how far it got before it matched. An upper bound is the safe direction
//!   for a *bound* — overcharging makes a safepoint arrive early, where
//!   undercharging is the overshoot this whole decision exists to close;
//! - `join` charges the bytes of the answer it builds, parts and separators
//!   together, which is what it copies and is unrelated to the length of the
//!   array it was handed.
//!
//! The charge is made as soon as the receiver has been read, which is before
//! `split` and `replace` refuse an empty needle: a call that raises still
//! walked what it walked, and a bound a program could slip under by failing
//! would not be one.

use cove_ir::{LayoutId, Shape};

use crate::error::RuntimeError;
use crate::vm::exec::Machine;
use crate::vm::intrinsics::operand::{Dest, Frame};
use crate::vm::intrinsics::{make, operand};

/// `String.length() -> Int`, in characters.
pub(super) fn length(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let text = operand::text(machine, frame, 0)?;
    machine.examined(text.len() as u64);
    let count = text.chars().count();
    dest.word(machine, count as u64);
    Ok(())
}

/// `String.words() -> Array<String>`, split on ASCII whitespace.
pub(super) fn words(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let text = operand::text(machine, frame, 0)?;
    machine.examined(text.len() as u64);
    let parts: Vec<&str> = text.split_ascii_whitespace().collect();
    let array = make::strings(machine, &parts)?;
    dest.word(machine, array);
    Ok(())
}

/// `String.chars() -> Array<String>`.
///
/// A character in Cove is a `String` of length 1; there is no `Character`
/// type for this to answer instead.
pub(super) fn chars(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let text = operand::text(machine, frame, 0)?;
    machine.examined(text.len() as u64);
    let parts: Vec<String> = text.chars().map(String::from).collect();
    let array = make::strings(machine, &parts)?;
    dest.word(machine, array);
    Ok(())
}

/// `String.split(separator) -> Array<String>`.
pub(super) fn split(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let text = operand::text(machine, frame, 0)?;
    // Before the refusal below, not after it: the receiver was decoded either
    // way. See the module's "Every operation here says what it examined".
    machine.examined(text.len() as u64);
    let separator = operand::text(machine, frame, 1)?;
    if separator.is_empty() {
        return Err(operand::empty_needle(
            "String.split",
            "separator",
            "use `chars()` to take a string apart character by character",
        ));
    }
    let parts: Vec<&str> = text.split(&separator).collect();
    let array = make::strings(machine, &parts)?;
    dest.word(machine, array);
    Ok(())
}

/// `String.join(parts) -> String`, where the receiver is the separator.
pub(super) fn join(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let separator_addr = operand::string(machine, frame, 0);
    let addr = frame.word(machine, 1);
    // An `Array<String>` is what the verifier held `parts` to (#378, P5-3),
    // so its element layout is not asked again: it is a run of one-word
    // references, collected without asking each of them what it is.
    debug_assert!(
        elements_of(machine, addr).is_some(),
        "an operand verified to be an `Array<String>`"
    );
    let len = machine.object_len(addr);
    let mut parts = Vec::with_capacity(len as usize);
    for at in 0..len {
        let part = machine.payload(addr, at);
        // A null part is refused rather than joined as the empty string. No
        // array a program builds holds one.
        if part == 0 {
            return Err(operand::null_value());
        }
        parts.push(part);
    }
    let joined = joined_bytes(machine, separator_addr, &parts)?;
    dest.word(machine, joined);
    Ok(())
}

/// `parts` joined by the string at `separator`, as one allocation and a run
/// of copies.
///
/// The lengths are summed before anything is allocated, so the answer is
/// allocated once at exactly its size and no part is ever copied twice. The
/// previous shape of this read every part into a Rust `String`, validating
/// UTF-8 it had itself written, appended it to a buffer that grew as it went,
/// and then packed the whole thing back into a Cove object — four passes over
/// the bytes where this has one.
///
/// Summing in `i64` and handing the total to `new_string_of` is what refuses
/// a join too long to have a length, through the error an exhausted heap
/// already raises.
fn joined_bytes(machine: &mut Machine, separator: u64, parts: &[u64]) -> Result<u64, RuntimeError> {
    let width = |addr: u64| machine.object_len(addr) as i64;
    let separator_len = width(separator);
    let mut total = separator_len * (parts.len() as i64 - 1).max(0);
    for part in parts {
        total += width(*part);
    }
    // The bytes this builds, which is what it copies: every part once and
    // every separator between two of them. It is not the length of the array
    // it was handed, and it is not the receiver — the receiver is the
    // separator, and a join of one part copies none of it.
    machine.examined(total.max(0) as u64);
    let result = machine.new_string_of(total)?;
    let mut at = 0usize;
    for (index, part) in parts.iter().enumerate() {
        if index > 0 && separator_len > 0 {
            machine.copy_string_bytes(result, at, separator, 0, separator_len as usize);
            at += separator_len as usize;
        }
        let len = machine.object_len(*part) as usize;
        machine.copy_string_bytes(result, at, *part, 0, len);
        at += len;
    }
    Ok(result)
}

/// The element layout and length of the `Array` at `addr`.
fn elements_of(machine: &Machine, addr: u64) -> Option<(LayoutId, u32)> {
    match machine.program().layout(machine.object_layout(addr)).shape {
        Shape::Elements {
            elem,
            growable: false,
        } => Some((elem, machine.object_len(addr))),
        _ => None,
    }
}

/// `String.slice(from, to) -> String`, in character positions.
pub(super) fn slice(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let text = operand::text(machine, frame, 0)?;
    // The **receiver's** bytes rather than the answer's, because that is what
    // this arm examines: `from` and `to` are character positions, so the line
    // below collects every character of the receiver before it can take a
    // range of them. A slice of two characters out of a megabyte reads the
    // megabyte, and charging the answer's own length would say it did not.
    machine.examined(text.len() as u64);
    let from = operand::int(machine, frame, 1);
    let to = operand::int(machine, frame, 2);
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len() as i64;
    let from = from.clamp(0, len) as usize;
    let to = to.clamp(0, len) as usize;
    let sliced = if to <= from {
        String::new()
    } else {
        chars[from..to].iter().collect()
    };
    let word = machine.new_string(&sliced)?;
    dest.word(machine, word);
    Ok(())
}

/// `core.refuseByteRange(text, from, to)`: what is wrong with a byte range of
/// `text` that `std.stringbuilder`'s `appendRange` has already found to be
/// wrong, as the run stops.
///
/// [ADR 0062](../../../../../docs/adr/0062-an-append-is-ensure-store-commit.md)
/// takes the range policy out of the copy, so this decides nothing about
/// whether a range is legal: `appendRange` asks `String.sliceBytes`' five
/// questions in Cove, and reaches this only when one of them has failed. What
/// is left here is the sentence, and it is `sliceBytes`' sentence — the one
/// `std.string`'s `refuseRange` writes out in Cove and
/// [`crate::builtins`]' `wrong_byte_range` writes out for the oracle, in the
/// same order and with the same final `else`, so that the three agree about
/// which sentence a range gets by having one shape rather than by each
/// deciding.
///
/// It never answers, so it takes no [`Dest`].
pub(super) fn refuse_byte_range(
    machine: &mut Machine,
    frame: Frame<'_>,
) -> Result<(), RuntimeError> {
    let text = operand::text(machine, frame, 0)?;
    let from = operand::int(machine, frame, 1);
    let to = operand::int(machine, frame, 2);
    let len = text.len() as i64;
    let boundary = |at: i64| at < len && !text.is_char_boundary(at as usize);
    let message = if from < 0 || from > len {
        format!("`from` is `{from}`, and a byte offset into this string is 0 to {len}")
    } else if to < 0 || to > len {
        format!("`to` is `{to}`, and a byte offset into this string is 0 to {len}")
    } else if from > to {
        format!("`from` is `{from}` and `to` is `{to}`, so this range runs backwards")
    } else if boundary(from) {
        format!("`from` is `{from}`, which is inside a character rather than at the start of one")
    } else {
        format!("`to` is `{to}`, which is inside a character rather than at the start of one")
    };
    Err(RuntimeError::new(message))
}

/// `String.trim() -> String`.
pub(super) fn trim(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let text = operand::text(machine, frame, 0)?;
    machine.examined(text.len() as u64);
    let word = machine.new_string(text.trim())?;
    dest.word(machine, word);
    Ok(())
}

/// `String.contains(text) -> Bool`.
pub(super) fn contains(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let text = operand::text(machine, frame, 0)?;
    // An upper bound: `str::contains` does not report where it stopped. See
    // the module's note on why an upper bound is the safe direction here.
    machine.examined(text.len() as u64);
    let needle = operand::text(machine, frame, 1)?;
    dest.word(machine, text.contains(&needle) as u64);
    Ok(())
}

/// `String.startsWith(prefix) -> Bool`.
pub(super) fn starts_with(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let text = operand::text(machine, frame, 0)?;
    let prefix = operand::text(machine, frame, 1)?;
    // At most the needle, and never past the receiver: a prefix longer than
    // what it is tested against is refused on the length alone.
    machine.examined(prefix.len().min(text.len()) as u64);
    dest.word(machine, text.starts_with(&prefix) as u64);
    Ok(())
}

/// `String.endsWith(suffix) -> Bool`.
pub(super) fn ends_with(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let text = operand::text(machine, frame, 0)?;
    let suffix = operand::text(machine, frame, 1)?;
    // As `startsWith`: at most the needle, capped at the receiver.
    machine.examined(suffix.len().min(text.len()) as u64);
    dest.word(machine, text.ends_with(&suffix) as u64);
    Ok(())
}

/// `String.indexOf(text) -> Option<Int>`, in character positions.
///
/// An `Option` is inline, so what this answers is the run of words
/// `[disc, Int]` written into the destination rather than an address — and a
/// `None` leaves the payload word zero, which is what makes the region's one
/// static reference map right for both cases.
pub(super) fn index_of(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let text = operand::text(machine, frame, 0)?;
    // An upper bound, as `contains`: `str::find` answers where it matched and
    // not how much it read on the way, and the character count below walks
    // the front of the receiver again.
    machine.examined(text.len() as u64);
    let needle = operand::text(machine, frame, 1)?;
    match text.find(&needle) {
        // `find` answers a byte offset; the characters before it are counted
        // to convert that into the character index `length()` counts in.
        Some(byte) => make::some(machine, dest, &[text[..byte].chars().count() as u64]),
        None => make::none(machine, dest),
    }
}

/// `String.replace(old, new) -> String`.
pub(super) fn replace(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let text = operand::text(machine, frame, 0)?;
    // Before the refusal below, as `split`.
    machine.examined(text.len() as u64);
    let old = operand::text(machine, frame, 1)?;
    if old.is_empty() {
        return Err(operand::empty_needle(
            "String.replace",
            "old",
            "`old` is the text to look for, and an empty `old` names none",
        ));
    }
    let new = operand::text(machine, frame, 2)?;
    let replaced = text.replace(&old, &new);
    let word = machine.new_string(&replaced)?;
    dest.word(machine, word);
    Ok(())
}

/// `String.toUpper() -> String`.
pub(super) fn to_upper(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let receiver = operand::text(machine, frame, 0)?;
    machine.examined(receiver.len() as u64);
    let text = receiver.to_uppercase();
    let word = machine.new_string(&text)?;
    dest.word(machine, word);
    Ok(())
}

/// `String.toLower() -> String`.
pub(super) fn to_lower(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let receiver = operand::text(machine, frame, 0)?;
    machine.examined(receiver.len() as u64);
    let text = receiver.to_lowercase();
    let word = machine.new_string(&text)?;
    dest.word(machine, word);
    Ok(())
}

/// `String.fromCodePoint(codePoint) -> Result<String, Error>`.
///
/// The surrogates are told apart from the other refusals because they are the
/// one a caller can usually do something about: a format that writes a code
/// point in sixteen bits writes anything past `0xFFFF` as a pair of them, so
/// a program that reached here with a `0xD800` has half of a character rather
/// than a bad one.
pub(super) fn from_code_point(
    machine: &mut Machine,
    frame: Frame<'_>,
    dest: Dest,
) -> Result<(), RuntimeError> {
    let code_point = operand::int(machine, frame, 0);
    if (0xD800..=0xDFFF).contains(&code_point) {
        let message =
            format!("`{code_point}` is a surrogate half, which is not a character on its own");
        return make::failed(machine, dest, &message);
    }
    match u32::try_from(code_point).ok().and_then(char::from_u32) {
        Some(character) => {
            let text = machine.new_string(&character.to_string())?;
            // Nothing allocates between the string and the `Ok` around it,
            // because a `Result` is words: the case is built out of the
            // layout table and the word it was just handed.
            make::ok(machine, dest, &[text])
        }
        None => {
            let message = format!("`{code_point}` is not a Unicode code point");
            make::failed(machine, dest, &message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::intrinsics::tests::{
        elements, message_of, option_of, read, result_of, run, scalar, word, words_of, world,
    };
    use cove_ir::Repr;

    /// The parts of an `Array<String>` a builtin answered.
    fn parts(machine: &Machine, addr: u64) -> Vec<String> {
        words_of(machine, addr)
            .into_iter()
            .map(|word| read(machine, word))
            .collect()
    }

    /// `text.operation(args)`, for the operations that answer one word.
    fn on(machine: &mut Machine, text: &str, operation: &str, args: &[(Repr, u64)]) -> u64 {
        let self_ = machine.new_string(text).unwrap();
        let mut operands = vec![(Repr::Ref, self_)];
        operands.extend_from_slice(args);
        word(machine, "String", operation, &operands).unwrap()
    }

    /// The same, for the one that answers an `Option<Int>`: an enum is inline
    /// now, so the answer is a run of words rather than an address.
    fn words_on(
        machine: &mut Machine,
        text: &str,
        operation: &str,
        args: &[(Repr, u64)],
    ) -> Vec<u64> {
        let self_ = machine.new_string(text).unwrap();
        let mut operands = vec![(Repr::Ref, self_)];
        operands.extend_from_slice(args);
        run(machine, "String", operation, &operands).unwrap()
    }

    fn text_of(machine: &mut Machine, source: &str, operation: &str) -> String {
        let word = on(machine, source, operation, &[]);
        read(machine, word)
    }

    /// `length()` is `chars().count()` and not the header's byte count, which
    /// is the whole reason it is a builtin rather than an `Inst::Len`.
    #[test]
    fn length_counts_characters_and_not_bytes() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let text = machine.new_string("héllo").unwrap();
        assert_eq!(machine.object_len(text), 6, "six bytes");
        assert_eq!(
            word(&mut machine, "String", "length", &[(Repr::Ref, text)]).unwrap(),
            5,
            "five characters"
        );

        // `isEmpty` is not a machine builtin for `String` either: it is
        // `std.string.isEmpty`, and it is `cove-sema`'s and `cove-ir`'s
        // tests that check it rather than a word read off the machine here.
    }

    #[test]
    fn chars_and_words_take_a_string_apart() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let word = on(&mut machine, "hé", "chars", &[]);
        assert_eq!(parts(&machine, word), vec!["h", "é"]);
        // ASCII whitespace, and runs of it collapse.
        let word = on(&mut machine, "  one  two ", "words", &[]);
        assert_eq!(parts(&machine, word), vec!["one", "two"]);
    }

    #[test]
    fn split_separates_on_the_separator_and_refuses_an_empty_one() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let comma = machine.new_string(",").unwrap();
        let word = on(&mut machine, "a,,b", "split", &[(Repr::Ref, comma)]);
        assert_eq!(parts(&machine, word), vec!["a", "", "b"]);

        let empty = machine.new_string("").unwrap();
        let self_ = machine.new_string("ab").unwrap();
        let error = run(
            &mut machine,
            "String",
            "split",
            &[(Repr::Ref, self_), (Repr::Ref, empty)],
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "`String.split` cannot use an empty `separator`"
        );
        assert_eq!(
            error.help.as_deref(),
            Some("use `chars()` to take a string apart character by character")
        );
    }

    /// The receiver is the separator and the argument is the parts, which is
    /// the way round the schema declares it.
    /// A join sizes its answer by summing the parts, so every part and every
    /// separator lands at an offset the previous ones decided. A separator
    /// whose length is not a multiple of eight is what makes those offsets
    /// unaligned, and that is the case worth walking.
    #[test]
    fn join_agrees_with_rust_at_every_separator_width() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 18);
        let layout = elements(&program, program.str_layout, false);
        let cases: &[&[&str]] = &[
            &[],
            &[""],
            &["a"],
            &["", ""],
            &["a", ""],
            &["", "b"],
            &["one", "two", "three"],
            &["12345678", "12345678"],
            &["1234567", "123456789"],
            &["h\u{e9}llo", "w\u{f6}rld", "\u{1f600}"],
            &["a", "b", "c", "d", "e", "f", "g", "h", "i"],
        ];
        for separator in ["", " ", ", ", "--", "1234567", "12345678", "123456789"] {
            for parts in cases {
                let items = machine.new_object(layout, parts.len() as u32).unwrap();
                for (at, part) in parts.iter().enumerate() {
                    let word = machine.new_string(part).unwrap();
                    machine.set_payload(items, at as u32, word);
                }
                let joined = on(&mut machine, separator, "join", &[(Repr::Ref, items)]);
                assert_eq!(
                    read(&machine, joined),
                    parts.join(separator),
                    "{parts:?} joined by {separator:?}"
                );
            }
        }
    }

    /// An `Array<String>` holding a null is refused rather than joined as if
    /// the part were empty.
    #[test]
    fn a_join_over_a_null_part_is_refused_as_it_was() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let layout = elements(&program, program.str_layout, false);
        let items = machine.new_object(layout, 1).unwrap();
        machine.set_payload(items, 0, 0);
        let self_ = machine.new_string(", ").unwrap();
        let error = run(
            &mut machine,
            "String",
            "join",
            &[(Repr::Ref, self_), (Repr::Ref, items)],
        )
        .unwrap_err();
        assert!(
            !error.message.is_empty(),
            "a null part is refused rather than joined as an empty string"
        );
    }

    #[test]
    fn join_puts_the_receiver_between_the_parts() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let layout = elements(&program, program.str_layout, false);
        let items = machine.new_object(layout, 2).unwrap();
        let a = machine.new_string("a").unwrap();
        let b = machine.new_string("b").unwrap();
        machine.set_payload(items, 0, a);
        machine.set_payload(items, 1, b);

        let joined = on(&mut machine, ", ", "join", &[(Repr::Ref, items)]);
        assert_eq!(read(&machine, joined), "a, b");
    }

    /// Character positions, and both bounds clamped, exactly as a sequence
    /// slice is.
    #[test]
    fn slice_is_in_characters_and_clamps_both_bounds() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let sliced = |machine: &mut Machine, from: i64, to: i64| {
            let word = on(
                machine,
                "héllo",
                "slice",
                &[(Repr::Int, from as u64), (Repr::Int, to as u64)],
            );
            read(machine, word)
        };
        assert_eq!(sliced(&mut machine, 1, 3), "él");
        assert_eq!(sliced(&mut machine, -9, 99), "héllo");
        assert_eq!(sliced(&mut machine, 3, 1), "");
    }

    #[test]
    fn trim_and_the_case_mappings_are_the_oracles() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        // Unicode whitespace, not just ASCII.
        assert_eq!(text_of(&mut machine, "\u{a0} a \n", "trim"), "a");
        assert_eq!(text_of(&mut machine, "straße", "toUpper"), "STRASSE");
        assert_eq!(text_of(&mut machine, "ÉÀ", "toLower"), "éà");
    }

    #[test]
    fn the_predicates_match_bytes() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let needle = machine.new_string("él").unwrap();
        assert_eq!(
            on(&mut machine, "héllo", "contains", &[(Repr::Ref, needle)]),
            1
        );
        let prefix = machine.new_string("hé").unwrap();
        assert_eq!(
            on(&mut machine, "héllo", "startsWith", &[(Repr::Ref, prefix)]),
            1
        );
        let suffix = machine.new_string("lo").unwrap();
        assert_eq!(
            on(&mut machine, "héllo", "endsWith", &[(Repr::Ref, suffix)]),
            1
        );
        let absent = machine.new_string("z").unwrap();
        assert_eq!(
            on(&mut machine, "héllo", "contains", &[(Repr::Ref, absent)]),
            0
        );
    }

    /// `find` answers a byte offset and `indexOf` answers a character
    /// position, so the two disagree for anything past the first non-ASCII
    /// character — and the character position is the one `length()` and
    /// `slice()` count in.
    #[test]
    fn index_of_answers_a_character_position() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let int = scalar(&program, Repr::Int);
        let needle = machine.new_string("l").unwrap();
        let words = words_on(&mut machine, "héllo", "indexOf", &[(Repr::Ref, needle)]);
        assert_eq!(
            option_of(&program, int, &words),
            ("Some".to_string(), vec![2])
        );

        let absent = machine.new_string("z").unwrap();
        let words = words_on(&mut machine, "héllo", "indexOf", &[(Repr::Ref, absent)]);
        assert_eq!(option_of(&program, int, &words).0, "None");
        // `None` fills none of the payload region, and what it does not fill
        // reads null.
        assert_eq!(words, vec![0, 0]);
    }

    #[test]
    fn replace_rewrites_every_match_and_refuses_an_empty_old() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let old = machine.new_string("a").unwrap();
        let new = machine.new_string("bb").unwrap();
        let word = on(
            &mut machine,
            "banana",
            "replace",
            &[(Repr::Ref, old), (Repr::Ref, new)],
        );
        assert_eq!(read(&machine, word), "bbbnbbnbb");

        let empty = machine.new_string("").unwrap();
        let self_ = machine.new_string("x").unwrap();
        let error = run(
            &mut machine,
            "String",
            "replace",
            &[(Repr::Ref, self_), (Repr::Ref, empty), (Repr::Ref, new)],
        )
        .unwrap_err();
        assert_eq!(error.message, "`String.replace` cannot use an empty `old`");
        assert_eq!(
            error.help.as_deref(),
            Some("`old` is the text to look for, and an empty `old` names none")
        );
    }

    /// A code point that names a character, and the two ways one does not.
    #[test]
    fn from_code_point_answers_a_character_or_says_why_not() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let string = program.str_layout;
        let of = |machine: &mut Machine, point: i64| {
            run(
                machine,
                "String",
                "fromCodePoint",
                &[(Repr::Int, point as u64)],
            )
            .unwrap()
        };
        let words = of(&mut machine, 0x00E9);
        let (case, payload) = result_of(&program, string, &words);
        assert_eq!(
            (case.as_str(), read(&machine, payload[0]).as_str()),
            ("Ok", "é")
        );

        let words = of(&mut machine, 0xD800);
        assert_eq!(
            message_of(&machine, string, &words),
            "`55296` is a surrogate half, which is not a character on its own"
        );
        let words = of(&mut machine, 0x11_0000);
        assert_eq!(
            message_of(&machine, string, &words),
            "`1114112` is not a Unicode code point"
        );
    }

    /// The array a `chars()` builds is a root while it is being filled: the
    /// heap is full of dead objects, so a string made partway through the
    /// walk collects, and an unrooted array would be freed under it.
    #[test]
    fn chars_holds_the_array_it_is_filling() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 12);
        let source = machine.new_string("abcdefghij").unwrap();
        machine.push_temp(source);
        while machine.heap_words() + 2 <= 1 << 12 {
            machine.new_string("dead").unwrap();
        }
        let before = machine.collected().collections;

        let items = word(&mut machine, "String", "chars", &[(Repr::Ref, source)]).unwrap();
        assert!(
            machine.collected().collections > before,
            "the fixture did not force a collection"
        );
        assert_eq!(
            parts(&machine, items),
            vec!["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"]
        );
    }
}
