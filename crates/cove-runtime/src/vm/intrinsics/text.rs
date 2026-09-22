//! `String`.
//!
//! A Cove `String` is UTF-8 and its object is the bytes, eight to a word. The
//! operations here are the oracle's, and where the oracle's answer depends on
//! how it reads those bytes, this one reads them the same way rather than
//! choosing again:
//!
//! - **`length()` is not here.** It counted characters rather than bytes —
//!   `chars().count()`, and not the header's length field — and a count in
//!   characters is a policy over a representation rather than an operation of
//!   the machine, so [ADR 0064](../../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)
//!   moved it to `std.string.length`: a Cove loop that reads each lead byte
//!   and advances by the width that byte declares.
//! - **`chars()` is not here either, and it is the one whose departure the
//!   bullet above predicted.** `length` agreed with it because both counted
//!   the same thing; issue #454's Step 3 made them the *same loop*.
//!   `std.string.chars` sizes its run with the private walk under
//!   `std.string.length` and then walks the bytes again taking a
//!   `core.stringSlice` per character, so the two can no longer disagree
//!   about where a character begins — where this file had two readings of
//!   that question and an argument that they matched. What left with it is a
//!   decode: the arm here was `text.chars().map(String::from).collect()`,
//!   which ran Rust's UTF-8 decoder over the receiver to produce scalars and
//!   then encoded every scalar back into bytes, to answer strings whose bytes
//!   were already in the receiver in the right order.
//! - **`slice(from, to)` is not here either, and it was the hardest of the
//!   three to give up**: its positions are **characters** and the substrate
//!   is bytes, so it decoded the whole receiver into a `Vec<char>` to take
//!   two of them. ADR 0064 moved it to `std.string.slice`, where the two
//!   positions are clamped and then walked into byte offsets by the same
//!   lead-byte step `length` uses, and the copy beneath is the byte run slice
//!   `sliceBytes` already stood on. What `indexOf` answers is in characters
//!   for the same reason and left the same way — the byte offset a search
//!   finds becomes a character position in `std.string.indexOf`, by walking
//!   the lead bytes in front of it.
//! - **`split` and `replace` match bytes**, which for UTF-8 is the same set
//!   of matches as matching characters and is what Rust's own `str` does.
//!   `startsWith`, `endsWith`, `contains` and `indexOf` matched bytes here
//!   too, and match them in Cove now: ADR 0064 moved the two comparisons to
//!   `std.string.startsWith` and `std.string.endsWith`, and
//!   [ADR 0065](../../../../../docs/adr/0065-a-run-search-is-the-one-loop-that-stays-below.md)
//!   moved both searches to `std.string.contains` and `std.string.indexOf`
//!   over `Inst::RunFind`. All four doc comments carry the boundary argument
//!   this bullet is a summary of — the suffix one in full, the prefix one in
//!   the half it needs, and the search ones at every offset rather than at
//!   one.
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
//! Seven of the operations below walk the whole receiver and one builds its
//! answer out of parts, and until
//! [ADR 0064](../../../../../docs/adr/0064-an-intrinsic-names-a-machine-not-a-method.md)'s
//! Decision 7 every one of them cost the run **one** unit of work — the one
//! an `add.int` costs. So `"ab".toUpper()` and a `toUpper()` over a hundred
//! thousand characters spent the same fuel, ran for hundreds of times the
//! wall clock, and were the same to a cancellation, a deadline and a fuel
//! bound alike.
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
//! - the seven that walk the receiver — `words`, `chars`, `split`, `trim`,
//!   `replace`, `toUpper`, `toLower` — charge the receiver's own byte length,
//!   because [`operand::text`] has already decoded the whole of it before any
//!   of them looks at a single character. `slice` was an eighth and charged
//!   the same way — the receiver's bytes and not the answer's, because a
//!   two-character slice of a megabyte read the megabyte. In `std.string` it
//!   charges what it *walks*, an instruction a character as far as `to` and
//!   not one byte further, which is the same trade ADR 0065 made for the
//!   searches;
//! - a bullet here used to say that `startsWith` and `endsWith` compare at
//!   most the needle, so they charge the needle's length capped at the
//!   receiver's. Neither is an intrinsic any more: ADR 0064 made both of them
//!   Cove loops, where each byte read is paid for as an instruction — which
//!   is exactly the charge that bullet was approximating, made by the
//!   mechanism that charges everything else;
//! - a bullet here used to say that `contains` and `indexOf` are charged the
//!   **receiver's** length, an upper bound rather than a measurement, because
//!   `str::find` reports where it matched and not how much it read on the way.
//!   Neither is an intrinsic any more either: ADR 0065 moved both onto
//!   `Inst::RunFind`, which charges what it *consumed* rather than an upper
//!   bound on it, because a resumable matcher knows where it stopped and
//!   `str::find` does not. That is the difference an instruction buys over a
//!   call, in the one coordinate this bullet was about;
//! - a bullet here used to say that `join` charges the bytes of the answer
//!   it builds, parts and separators together, rather than the length of the
//!   array it was handed. It is not an arm in this file any more either: issue
//!   #454's Step 3 made it `std.string.join`, which sums the parts, sizes a
//!   `StringBuilder` by that sum and appends into it — so every byte it copies
//!   is charged by the `Inst::RunCopy` that copies it, and the sum and the
//!   loop around it are charged an instruction at a time. That is the same
//!   exchange `length`, `slice` and the four predicates above made, on the one
//!   operation here that *built* its answer rather than reading one.
//!
//! The charge is made as soon as the receiver has been read, which is before
//! `split` and `replace` refuse an empty needle: a call that raises still
//! walked what it walked, and a bound a program could slip under by failing
//! would not be one.
//!
//! **Neither of those two refusals can move into Cove, and that is issue
//! [#461](https://github.com/myuon/cove/issues/461) rather than a gap in this
//! module.** The standard library could compute both answers — `Inst::RunFind`
//! is already under `std.string.contains` and `std.string.indexOf`, and
//! `std.string.chars` and `std.string.join` are the vector build and the byte
//! build — but a Cove body has nothing to *raise* with: `ExprKind` has no
//! `raise` form, `Inst::Trap` carries a `StrId` and not a slot, and it carries
//! neither the `rule` nor the `help` that these two sentences have. So
//! `Intrinsic::StringSplit` and `Intrinsic::StringReplace` stand where
//! `Intrinsic::StringRefuseByteRange` stands, and issue #454's Step 3 stops at
//! seventeen variants rather than the thirteen it planned for.
//!
//! **Step 4 found the same wall at a different receiver and went round it.**
//! `Int.parse` and `Int.parseRadix` were to move together, the general one
//! first and the decimal one as a radix-10 wrapper over it; `int_parse_radix`
//! raises on a radix outside `2..=36`, so the order inverted and `parse` alone
//! became `std.int.parse` — which never raises, because every failure it has
//! is an `Err` value it builds. Sixteen variants, and #461 now holds six of
//! them.
//!
//! `tests/e2e/values_string_split` and `tests/e2e/values_string_replace` pin
//! what the two operations answer, and `tests/e2e/fail_string_split_empty` and
//! `tests/e2e/fail_string_replace_empty` pin what they refuse, word for word —
//! including that the two `help` lines are **not** the same sentence.

use crate::error::RuntimeError;
use crate::vm::exec::Machine;
use crate::vm::intrinsics::operand::{Dest, Frame};
use crate::vm::intrinsics::{make, operand};

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

// `chars` was here, between `words` and `split`, and it was the only arm in
// this file that *decoded*. It collected `text.chars().map(String::from)` — a
// `char` per character out of Rust's UTF-8 decoder, then a fresh heap
// `String` per `char` out of Rust's encoder — and handed the vector to
// `make::strings`. `std.string.chars` answers the same array by walking lead
// bytes and taking one `core.stringSlice` per character, which copies the
// bytes where they already are and never forms a scalar at all; issue #454's
// Step 3. A character in Cove is a `String` of length 1 either way — there is
// no `Character` type for this to answer instead — and the module's header
// says what moving it settled about where a character begins.

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

// `join` was here, and it was the only arm in this file that *built* a
// string rather than reading one. It summed the parts' lengths and the
// separator's times one fewer than the parts, allocated the answer once at
// exactly that size with `new_string_of`, and copied each part and each
// separator into it with `copy_string_bytes` — one pass over the bytes where
// the shape before it had four. `std.string.join` is that same arithmetic in
// Cove over a `StringBuilder` sized by it, which is why the answer's
// allocation is still made once and at its exact length; issue #454's Step 3.
//
// `joined_bytes` and `elements_of` went with it. `elements_of` was a
// `debug_assert!`'s helper, checking that `join`'s second operand really was
// the non-growable run of one-word references the verifier had already held it
// to (#378, P5-3), and nothing else in this file ever asked. A `for` loop in
// Cove asks the same question of the same object through `Inst` bounds
// checking, every time rather than in a debug build.
//
// So did the null-part refusal. `join`'s loop declined a payload word of zero
// with `operand::null_value()`, under a comment saying no array a program
// builds holds one — which was true, and is why the path was unreachable from
// Cove and is not reproduced in the Cove body. `tests/e2e/values_string_join`
// is where the reachable behaviour lives now, on both evaluators.

// **`slice` is not here any more.** It was `chars().collect()`, two
// `i64::clamp`s and a re-collection — the whole receiver decoded into a
// `Vec<char>` so that two of them could be taken — and ADR 0064's Decision 2
// refuses it twice over: `slice` is a method's name that would have to be
// renamed the day the method was, and a clamp is a range policy, which ADR
// 0058's table gives to Cove and keeps the bounded copy below. It is
// `std.string.slice` now: two comparisons that move each bound into
// `0..length()`, one left-to-right walk of the lead bytes that turns the two
// **character positions** into the two **byte offsets** underneath them, and
// `core.stringSlice` — the byte `Inst::RunSlice` `sliceBytes` already stands
// on — between those offsets. The clamp is why this file's own doc comment
// says `slice` parts from `sliceBytes`, and the walk is why the positions
// stayed characters when the substrate is bytes.

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

// **Nothing that searches a `String` is here any more**, and the four went
// out in three different ways. `startsWith` and `endsWith` stood side by side
// and were charged the same way — at most the needle, capped at the receiver
// — and ADR 0064 moved both into `std.string`, as loops that compare the
// receiver's first or last bytes against the needle's. The suffix one needs
// UTF-8's self-synchronization to justify starting at `n - m`; the prefix one
// starts at 0 and needs only that a prefix's own bytes end at a boundary.
//
// `contains` and `indexOf` could not go the same way for nothing, and that is
// the whole of ADR 0065: their work is proportional to a haystack the caller
// did not size, where the comparisons' is bounded by an argument the caller
// already holds, so a Cove scan would pay a VM dispatch per byte of a run
// nobody sized. Both are `std.string` bodies now over one `Inst::RunFind` — a
// bounded run search that is chunked, charged and polled the way
// `Inst::RunCopy` is, and that charges what it consumed rather than the upper
// bound this file had to charge because `str::find` does not report where it
// stopped. `contains` compares the byte offset it answers against -1;
// `indexOf` walks the lead bytes in front of that offset to turn it into the
// character position its API promises — the one half of the old arm that was
// never about searching at all.
//
// With `indexOf` went the last arm in this file that read its operands
// without allocating, and so the last caller of `operand::with_text` and of
// `Machine`'s scratch pool. Both are gone too: a mechanism whose callers have
// all migrated is not kept against a caller that might arrive.

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::intrinsics::tests::{read, run, word, words_of, world};
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

    fn text_of(machine: &mut Machine, source: &str, operation: &str) -> String {
        let word = on(machine, source, operation, &[]);
        read(machine, word)
    }

    // `length()` had a test here — `héllo` is six bytes and five
    // characters — until ADR 0064 moved the count into `std.string.length`.
    // What replaced it is `tests/e2e/values_string_length`, a program that
    // asks for the count and the byte length of the same string at every
    // character width and is run on both evaluators: an oracle no arm in this
    // file supplies, for a body no arm in this file executes.
    //
    // `isEmpty` is not a machine builtin for `String` either, for the same
    // reason and since ADR 0058: it is `std.string.isEmpty`, and it is
    // `cove-sema`'s and `cove-ir`'s tests that check it rather than a word
    // read off the machine here.
    //
    // `fromCodePoint` had a test here too — `é`, a surrogate half, and one
    // code point past the end — until ADR 0064 moved the encode into
    // `std.string.fromCodePoint`. `tests/e2e/values_string_from_code_point`
    // replaced it with 61 rows on both evaluators, and the difference is not
    // the count: the golden was written by a standalone `rustc` oracle, so
    // every width boundary, both ends of the surrogate hole and both ends of
    // `Int` are checked against an implementation this repository does not
    // ship, where the three cases here were checked against the arm that
    // answered them.

    /// `chars` had the first line of this test — `"hé"` into `["h", "é"]` —
    /// until issue #454's Step 3 moved it to `std.string.chars`.
    /// `tests/e2e/values_string_chars` replaced it with 41 calls on both
    /// evaluators against a golden a standalone `rustc` oracle wrote, and
    /// the difference is not the count: the oracle has its own UTF-8 encoder
    /// and its own lead-byte splitter, so every width boundary, a NUL, and
    /// nine rows of combining marks, joiners and modifiers are checked
    /// against an implementation this repository does not ship, where the one
    /// case here was checked against the arm that answered it.
    #[test]
    fn words_splits_on_ascii_whitespace() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
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

    // Three `join` cases stood here — one comparing against `[&str]::join`
    // at every separator width and every alignment of a part boundary, one
    // over a null part, and one asserting the receiver goes *between* the
    // parts. `join` is not an arm in this file any more. What replaced the
    // first and the third is `tests/e2e/values_string_join`, which asks the
    // same questions of *both* evaluators over twenty-nine rows against a
    // golden a standalone `rustc` oracle wrote, including the separator count
    // isolated on parts that are all empty; the second had no replacement to
    // need, because a null part was unreachable from Cove and the checker is
    // what kept it so.

    // A `slice_is_in_characters_and_clamps_both_bounds` case stood here,
    // asserting that `"héllo".slice(1, 3)` is `él` — a *character* range where
    // a byte range would have cut `é` in half — and that `(-9, 99)` answers
    // the whole string and `(3, 1)` the empty one. `slice` is not an arm in
    // this file any more. What replaced it is `tests/e2e/values_string_slice`,
    // which asks the same questions of *both* evaluators over sixty-one rows
    // at three character widths, at both ends of `Int`, and on receivers a run
    // built rather than a literal — and whose golden a standalone `rustc`
    // oracle wrote before the harness was run once. An oracle no arm here
    // supplies, for a body no arm here executes.

    #[test]
    fn trim_and_the_case_mappings_are_the_oracles() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 14);
        // Unicode whitespace, not just ASCII.
        assert_eq!(text_of(&mut machine, "\u{a0} a \n", "trim"), "a");
        assert_eq!(text_of(&mut machine, "straße", "toUpper"), "STRASSE");
        assert_eq!(text_of(&mut machine, "ÉÀ", "toLower"), "éà");
    }

    // A `the_predicates_match_bytes` case stood here, asserting that
    // `"héllo".contains("él")` is `true` and `"héllo".contains("z")` is
    // `false` — and, before that, a line each for `startsWith` and `endsWith`.
    // Beside it stood two cases about the scratch pool an `indexOf` call
    // filled, and one asserting that `"héllo".indexOf("l")` is `Some(2)` and
    // not `Some(3)` — a *character* position where `str::find` answers a byte
    // offset. None of the four operations is an arm in this file any more, and
    // with the last of them the pool those two cases watched is gone as well.
    // What replaced each is an end-to-end suite that asks the question on
    // *both* evaluators at every character width and at the byte patterns the
    // body could be wrong about: `tests/e2e/values_string_starts_with`,
    // `values_string_ends_with`, `values_string_contains` and
    // `values_string_index_of` — the last of which asserts that same
    // byte-against-character disagreement from a Cove body, over a corpus far
    // wider than one word. An oracle no arm here supplies, for a body no arm
    // here executes.

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

    /// The array a `split()` builds is a root while it is being filled: the
    /// heap is full of dead objects, so a string made partway through the
    /// walk collects, and an unrooted array would be freed under it.
    ///
    /// **It was `chars()` that asked this, and the question outlived it.**
    /// Issue #454's Step 3 moved that operation into `std.string.chars`,
    /// where the array under construction is a `Vector` the frame holds and
    /// the collector finds through the frame rather than through
    /// `make::strings`' temporary root. What is left below this file that
    /// fills an array a word at a time is `words` and `split`, so `split` asks
    /// it now — and it asks it harder, because it allocates a string per part
    /// *and* the array, where `chars` allocated one string per character of a
    /// receiver that was already on the heap. Ten one-character parts, which
    /// is the fixture `chars` had, spelled as a separator run.
    #[test]
    fn split_holds_the_array_it_is_filling() {
        let program = world();
        let mut machine = Machine::new(&program, 1 << 12);
        let source = machine.new_string("a,b,c,d,e,f,g,h,i,j").unwrap();
        machine.push_temp(source);
        let comma = machine.new_string(",").unwrap();
        machine.push_temp(comma);
        while machine.heap_words() + 2 <= 1 << 12 {
            machine.new_string("dead").unwrap();
        }
        let before = machine.collected().collections;

        let items = word(
            &mut machine,
            "String",
            "split",
            &[(Repr::Ref, source), (Repr::Ref, comma)],
        )
        .unwrap();
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
