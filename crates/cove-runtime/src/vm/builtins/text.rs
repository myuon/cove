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

use cove_ir::{LayoutId, Repr};

use crate::error::RuntimeError;
use crate::vm::builtins::operand::Operand;
use crate::vm::builtins::{make, operand};
use crate::vm::exec::Machine;

/// The address of a `String` receiver, with nothing read out of it.
fn receiver_addr(
    machine: &Machine,
    method: &str,
    receiver: Operand<'_>,
) -> Result<u64, RuntimeError> {
    let Some((Repr::Ref, addr)) = operand::as_word(machine, receiver) else {
        return Err(operand::no_method(machine, receiver, method));
    };
    if addr == 0 {
        return Err(operand::null_value());
    }
    if !super::is_string(machine, addr) {
        return Err(operand::no_method(machine, receiver, method));
    }
    Ok(addr)
}

/// The text of a `String` receiver.
///
/// This copies the whole object and validates it, once per call, which is
/// what every operation above wanted and what the two byte-counted ones
/// below exist to not do: they take [`receiver_addr`] and read the words they
/// actually need.
fn receiver(
    machine: &Machine,
    method: &str,
    receiver: Operand<'_>,
) -> Result<String, RuntimeError> {
    super::string_of(machine, receiver_addr(machine, method, receiver)?)
}

/// The byte at `at` in the string object at `addr`.
///
/// The payload holds eight bytes to a word, least-significant byte first —
/// this is the inverse of `Machine::write_bytes` — so one byte is one word
/// read and a shift, and no part of the object is copied.
fn byte_at(machine: &Machine, addr: u64, at: usize) -> u8 {
    machine.byte_of(addr, at)
}

/// The Unicode scalar value beginning at byte `at`, or `None` when `at` is
/// inside a character.
///
/// Nothing writes a string object except from a Rust `&str`, so the bytes are
/// valid UTF-8 in the shortest form and the lead byte alone gives the width.
/// A continuation byte in the lead position is the whole of "this offset is
/// not a character boundary".
fn decode(machine: &Machine, addr: u64, at: usize, len: usize) -> Option<u32> {
    let lead = byte_at(machine, addr, at);
    let (width, mut scalar) = match lead {
        0x00..=0x7F => return Some(lead as u32),
        0xC0..=0xDF => (2usize, (lead & 0x1F) as u32),
        0xE0..=0xEF => (3, (lead & 0x0F) as u32),
        0xF0..=0xF7 => (4, (lead & 0x07) as u32),
        _ => return None,
    };
    if at + width > len {
        return None;
    }
    for step in 1..width {
        scalar = (scalar << 6) | (byte_at(machine, addr, at + step) & 0x3F) as u32;
    }
    Some(scalar)
}

/// `String.codePointAtByte(offset) -> Option<Int>`.
pub(super) fn code_point_at_byte(
    machine: &mut Machine,
    result: LayoutId,
    operands: &[Operand<'_>],
    out: &mut Vec<u64>,
) -> Result<(), RuntimeError> {
    let (self_, args) = operand::method("String.codePointAtByte", operands, 1)?;
    let addr = receiver_addr(machine, "codePointAtByte", self_)?;
    let offset = operand::int(machine, "String.codePointAtByte", "offset", args[0])?;
    let len = machine.object_len(addr) as usize;
    match usize::try_from(offset)
        .ok()
        .filter(|at| *at < len)
        .and_then(|at| decode(machine, addr, at, len))
    {
        Some(scalar) => make::some(machine, result, &[scalar as u64], out),
        None => make::none(machine, result, out),
    }
}

/// `String.length() -> Int`, in characters.
pub(super) fn length(machine: &mut Machine, operands: &[Operand<'_>]) -> Result<u64, RuntimeError> {
    let (self_, _) = operand::method("length", operands, 0)?;
    Ok(receiver(machine, "length", self_)?.chars().count() as u64)
}

/// `String.words() -> Array<String>`, split on ASCII whitespace.
pub(super) fn words(machine: &mut Machine, operands: &[Operand<'_>]) -> Result<u64, RuntimeError> {
    let (self_, _) = operand::method("words", operands, 0)?;
    let text = receiver(machine, "words", self_)?;
    let parts: Vec<&str> = text.split_ascii_whitespace().collect();
    make::strings(machine, &parts)
}

/// `String.chars() -> Array<String>`.
///
/// A character in Cove is a `String` of length 1; there is no `Character`
/// type for this to answer instead.
pub(super) fn chars(machine: &mut Machine, operands: &[Operand<'_>]) -> Result<u64, RuntimeError> {
    let (self_, _) = operand::method("chars", operands, 0)?;
    let text = receiver(machine, "chars", self_)?;
    let parts: Vec<String> = text.chars().map(String::from).collect();
    make::strings(machine, &parts)
}

/// `String.split(separator) -> Array<String>`.
pub(super) fn split(machine: &mut Machine, operands: &[Operand<'_>]) -> Result<u64, RuntimeError> {
    let (self_, args) = operand::method("String.split", operands, 1)?;
    let text = receiver(machine, "split", self_)?;
    let separator = operand::text(machine, "String.split", "separator", args[0])?;
    if separator.is_empty() {
        return Err(operand::empty_needle(
            "String.split",
            "separator",
            "use `chars()` to take a string apart character by character",
        ));
    }
    let parts: Vec<&str> = text.split(&separator).collect();
    make::strings(machine, &parts)
}

/// `String.slice(from, to) -> String`, in character positions.
pub(super) fn slice(machine: &mut Machine, operands: &[Operand<'_>]) -> Result<u64, RuntimeError> {
    let (self_, args) = operand::method("String.slice", operands, 2)?;
    let text = receiver(machine, "slice", self_)?;
    let from = operand::int(machine, "String.slice", "from", args[0])?;
    let to = operand::int(machine, "String.slice", "to", args[1])?;
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len() as i64;
    let from = from.clamp(0, len) as usize;
    let to = to.clamp(0, len) as usize;
    let sliced = if to <= from {
        String::new()
    } else {
        chars[from..to].iter().collect()
    };
    machine.new_string(&sliced)
}

/// `String.trim() -> String`.
pub(super) fn trim(machine: &mut Machine, operands: &[Operand<'_>]) -> Result<u64, RuntimeError> {
    let (self_, _) = operand::method("trim", operands, 0)?;
    let text = receiver(machine, "trim", self_)?;
    machine.new_string(text.trim())
}

/// `String.contains(text) -> Bool`.
pub(super) fn contains(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, args) = operand::method("String.contains", operands, 1)?;
    let text = receiver(machine, "contains", self_)?;
    let needle = operand::text(machine, "String.contains", "text", args[0])?;
    Ok(text.contains(&needle) as u64)
}

/// `String.startsWith(prefix) -> Bool`.
pub(super) fn starts_with(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, args) = operand::method("String.startsWith", operands, 1)?;
    let text = receiver(machine, "startsWith", self_)?;
    let prefix = operand::text(machine, "String.startsWith", "prefix", args[0])?;
    Ok(text.starts_with(&prefix) as u64)
}

/// `String.endsWith(suffix) -> Bool`.
pub(super) fn ends_with(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, args) = operand::method("String.endsWith", operands, 1)?;
    let text = receiver(machine, "endsWith", self_)?;
    let suffix = operand::text(machine, "String.endsWith", "suffix", args[0])?;
    Ok(text.ends_with(&suffix) as u64)
}

/// `String.indexOf(text) -> Option<Int>`, in character positions.
///
/// An `Option` is inline, so what this answers is the run of words
/// `[disc, Int]` rather than an address — and a `None` leaves the payload
/// word zero, which is what makes the region's one static reference map right
/// for both cases.
pub(super) fn index_of(
    machine: &mut Machine,
    result: LayoutId,
    operands: &[Operand<'_>],
    out: &mut Vec<u64>,
) -> Result<(), RuntimeError> {
    let (self_, args) = operand::method("String.indexOf", operands, 1)?;
    let text = receiver(machine, "indexOf", self_)?;
    let needle = operand::text(machine, "String.indexOf", "text", args[0])?;
    match text.find(&needle) {
        // `find` answers a byte offset; the characters before it are counted
        // to convert that into the character index `length()` counts in.
        Some(byte) => make::some(machine, result, &[text[..byte].chars().count() as u64], out),
        None => make::none(machine, result, out),
    }
}

/// `String.replace(old, new) -> String`.
pub(super) fn replace(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, args) = operand::method("String.replace", operands, 2)?;
    let text = receiver(machine, "replace", self_)?;
    let old = operand::text(machine, "String.replace", "old", args[0])?;
    if old.is_empty() {
        return Err(operand::empty_needle(
            "String.replace",
            "old",
            "`old` is the text to look for, and an empty `old` names none",
        ));
    }
    let new = operand::text(machine, "String.replace", "new", args[1])?;
    let replaced = text.replace(&old, &new);
    machine.new_string(&replaced)
}

/// `String.toUpper() -> String`.
pub(super) fn to_upper(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, _) = operand::method("toUpper", operands, 0)?;
    let text = receiver(machine, "toUpper", self_)?.to_uppercase();
    machine.new_string(&text)
}

/// `String.toLower() -> String`.
pub(super) fn to_lower(
    machine: &mut Machine,
    operands: &[Operand<'_>],
) -> Result<u64, RuntimeError> {
    let (self_, _) = operand::method("toLower", operands, 0)?;
    let text = receiver(machine, "toLower", self_)?.to_lowercase();
    machine.new_string(&text)
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
    result: LayoutId,
    operands: &[Operand<'_>],
    out: &mut Vec<u64>,
) -> Result<(), RuntimeError> {
    let args = operand::free("String.fromCodePoint", operands, 1)?;
    let code_point = operand::int(machine, "String.fromCodePoint", "codePoint", args[0])?;
    if (0xD800..=0xDFFF).contains(&code_point) {
        let message =
            format!("`{code_point}` is a surrogate half, which is not a character on its own");
        return make::failed(machine, result, &message, out);
    }
    match u32::try_from(code_point).ok().and_then(char::from_u32) {
        Some(character) => {
            let text = machine.new_string(&character.to_string())?;
            // Nothing allocates between the string and the `Ok` around it,
            // because a `Result` is words: the case is built out of the
            // layout table and the word it was just handed.
            make::ok(machine, result, &[text], out)
        }
        None => {
            let message = format!("`{code_point}` is not a Unicode code point");
            make::failed(machine, result, &message, out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::builtins::tests::{
        elements, message_of, named, option_of, read, result_of, run, scalar, word, words_of, world,
    };

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

    /// Every `String` operation answers the same thing to a receiver that is
    /// not one, in the oracle's words.
    #[test]
    fn a_receiver_that_is_not_a_string_says_so() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        let error = run(&mut machine, "String", "trim", &[(Repr::Int, 1)]).unwrap_err();
        assert_eq!(error.message, "`Int` has no method `trim`");

        let self_ = machine.new_string("x").unwrap();
        let error = run(
            &mut machine,
            "String",
            "contains",
            &[(Repr::Ref, self_), (Repr::Int, 1)],
        )
        .unwrap_err();
        assert_eq!(
            error.message,
            "`String.contains` expects `String` for `text`, but found `Int`"
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
